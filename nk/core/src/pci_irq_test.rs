// SPDX-License-Identifier: GPL-2.0-or-later
//! Opt-in QEMU EDU hardware interrupt probe, never a production device driver.
//! Reference: https://www.qemu.org/docs/master/specs/edu.html
//! Enabled only by NK_PCI_IRQ_TEST at build time. No DMA or disk operations.
use crate::{
    device_irq_x86 as irq,
    pci::{self, Config, ConfigWrite},
    pci_irq,
};
use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy)]
struct Prepared {
    bdf: pci::Bdf,
    bar: u64,
    msi: pci_irq::Msi,
}
static mut PREPARED: Option<Prepared> = None;
static BAR: AtomicU64 = AtomicU64::new(0);
static ACKS: AtomicU64 = AtomicU64::new(0);

pub fn prepare(
    boot: &crate::bootinfo_x86::BootInfo,
    config: &impl Config,
    devices: &[pci::Device],
) {
    let Some(device) = devices
        .iter()
        .find(|d| d.vendor == 0x1234 && d.device == 0x11e8)
    else {
        crate::println!("PCI_IRQ_FAIL missing QEMU EDU");
        return;
    };
    let bar = match device.bars[0] {
        pci::Bar::Memory32(at) => at as u64,
        pci::Bar::Memory64(at) => at,
        _ => {
            crate::println!("PCI_IRQ_FAIL EDU memory BAR");
            return;
        }
    };
    let Some(end) = bar.checked_add(1 << 20) else {
        crate::println!("PCI_IRQ_FAIL EDU BAR overflow");
        return;
    };
    if !crate::platform_x86::mmio(boot, bar, end) {
        crate::println!("PCI_IRQ_FAIL EDU BAR outside boot map");
        return;
    }
    match pci_irq::msi(config, device.bdf) {
        Ok(Some(msi)) => {
            unsafe {
                PREPARED = Some(Prepared {
                    bdf: device.bdf,
                    bar,
                    msi,
                })
            };
            crate::println!(
                "  pci-irq: EDU prepared at {bar:#x}, MSI capability {:#x}",
                msi.offset
            );
        }
        _ => crate::println!("PCI_IRQ_FAIL EDU MSI capability"),
    }
}
fn acknowledge() {
    let bar = BAR.load(Ordering::Acquire);
    if bar == 0 {
        return;
    }
    unsafe {
        let status = core::ptr::read_volatile((bar + 0x24) as *const u32);
        core::ptr::write_volatile((bar + 0x64) as *mut u32, status);
        if status != 0 {
            ACKS.fetch_add(1, Ordering::Release);
        }
    }
}
pub fn run() {
    if option_env!("NK_PCI_IRQ_TEST").is_none() {
        return;
    }
    let Some(p) = (unsafe { *(&raw const PREPARED) }) else {
        return;
    };
    let config = crate::platform_x86::Hardware;
    let Some(vector) = irq::allocate(None, Some(acknowledge)) else {
        panic!("PCI_IRQ_FAIL no vector");
    };
    let destination = crate::lapic::apic_id(0).unwrap();
    let message = crate::irq_x86::message(destination, vector).unwrap();
    BAR.store(p.bar, Ordering::Release);
    let original_command = config.read16(p.bdf, 4).unwrap();
    // PCI MSI is itself a bus-master memory write. Permit those writes for
    // this QEMU test device; its DMA engine is never programmed or started.
    assert!(
        config.write16(p.bdf, 4, original_command | 4),
        "PCI_IRQ_FAIL bus master"
    );
    let saved = match pci_irq::enable_msi(&config, p.bdf, p.msi, message) {
        Ok(saved) => saved,
        Err(error) => {
            let _ = config.write16(p.bdf, 4, original_command);
            irq::retire(vector);
            panic!("PCI_IRQ_FAIL MSI setup {error:?}");
        }
    };
    let start = irq::count(vector);
    let acks = ACKS.load(Ordering::Acquire);
    let mut passed = true;
    for pulse in 1..=3 {
        unsafe { core::ptr::write_volatile((p.bar + 0x60) as *mut u32, 1) };
        let deadline = crate::arch::cycles().wrapping_add(crate::timer::cycle_hz() / 2);
        let mut spins = 0;
        while irq::count(vector) < start + pulse
            && crate::arch::cycles() < deadline
            && spins < 100_000_000
        {
            core::hint::spin_loop();
            spins += 1;
        }
        if irq::count(vector) != start + pulse {
            passed = false;
            break;
        }
    }
    // Quiesce source and disable MSI before retiring its permanent vector.
    unsafe { core::ptr::write_volatile((p.bar + 0x64) as *mut u32, u32::MAX) };
    let disabled = config.write16(p.bdf, p.msi.offset + 2, p.msi.control & !1);
    let restored = saved.restore(&config, p.bdf) && config.write16(p.bdf, 4, original_command);
    irq::retire(vector);
    passed &= disabled && restored && ACKS.load(Ordering::Acquire) == acks + 3;
    // Read back restoration; no test state should leak into a later driver.
    passed &= config.read16(p.bdf, 4) == Some(original_command)
        && config.read16(p.bdf, p.msi.offset + 2) == Some(p.msi.control);
    assert!(
        passed,
        "PCI_IRQ_FAIL MSI delivery/ack/restore: hits {} acks {} disabled {} restored {}",
        irq::count(vector) - start,
        ACKS.load(Ordering::Acquire) - acks,
        disabled,
        restored
    );
    crate::println!("PCI_MSI_PASS vector {vector:#x} APIC {destination}, 3 hardware interrupts acknowledged; config restored");
}

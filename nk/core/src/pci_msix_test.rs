// SPDX-License-Identifier: GPL-2.0-or-later
//! Opt-in QEMU 82574 interrupt-only probe. No RX/TX rings or packet I/O.
//! Register definitions/sequence follow stela-driver-net/src/e1000e.rs.
use crate::{
    device_irq_x86 as irq,
    pci::{self, Config, ConfigWrite},
    pci_irq::{self, MsixTable},
};
use core::sync::atomic::{AtomicU64, Ordering};
#[derive(Clone, Copy)]
struct Prepared {
    bdf: pci::Bdf,
    bar: u64,
    table: u64,
    bytes: u64,
    msix: pci_irq::Msix,
}
static mut PREPARED: Option<Prepared> = None;
static BAR: AtomicU64 = AtomicU64::new(0);
static ACKS: AtomicU64 = AtomicU64::new(0);
const RXQ0: u32 = 1 << 20;

struct Table {
    base: u64,
    bytes: u64,
}
impl MsixTable for Table {
    fn len(&self) -> u64 {
        self.bytes
    }
    fn read32(&self, offset: u32) -> Option<u32> {
        if offset & 3 != 0 || offset as u64 + 4 > self.bytes {
            return None;
        }
        Some(unsafe { core::ptr::read_volatile((self.base + offset as u64) as *const u32) })
    }
    fn write32(&self, offset: u32, value: u32) -> bool {
        if offset & 3 != 0 || offset as u64 + 4 > self.bytes {
            return false;
        }
        unsafe { core::ptr::write_volatile((self.base + offset as u64) as *mut u32, value) };
        true
    }
}
unsafe fn read(bar: u64, offset: u64) -> u32 {
    core::ptr::read_volatile((bar + offset) as *const u32)
}
unsafe fn write(bar: u64, offset: u64, value: u32) {
    core::ptr::write_volatile((bar + offset) as *mut u32, value);
}
fn base(bar: pci::Bar) -> Option<u64> {
    match bar {
        pci::Bar::Memory32(v) => Some(v as u64),
        pci::Bar::Memory64(v) => Some(v),
        _ => None,
    }
}

pub fn prepare(
    boot: &crate::bootinfo_x86::BootInfo,
    config: &impl Config,
    devices: &[pci::Device],
) {
    // Require QEMU's EDU fixture as well; never probe a physical NIC just
    // because it happens to use the same Intel device ID.
    if !devices
        .iter()
        .any(|d| d.vendor == 0x1234 && d.device == 0x11e8)
    {
        return;
    }
    let result = (|| {
        let d = devices
            .iter()
            .find(|d| d.vendor == 0x8086 && d.device == 0x10d3)?;
        let m = pci_irq::msix(config, d.bdf).ok()??;
        if m.table_bar != 3 {
            return None;
        }
        let bar = base(d.bars[0])?;
        let (table, end) = m.table_range(base(d.bars[3])?, 0x4000)?;
        if !crate::platform_x86::mmio(boot, bar, bar.checked_add(0x10000)?)
            || !crate::platform_x86::mmio(boot, table, end)
        {
            return None;
        }
        Some(Prepared {
            bdf: d.bdf,
            bar,
            table,
            bytes: end - table,
            msix: m,
        })
    })();
    if let Some(p) = result {
        unsafe { PREPARED = Some(p) };
        crate::println!(
            "  pci-irq: QEMU 82574 prepared, MSI-X {} entries at {:#x}",
            p.msix.count,
            p.table
        );
    } else {
        crate::println!("PCI_IRQ_FAIL QEMU 82574 MSI-X preparation");
    }
}
fn acknowledge() {
    let bar = BAR.load(Ordering::Acquire);
    if bar == 0 {
        return;
    }
    unsafe {
        let causes = read(bar, 0xc0); // ICR read clears causes.
        if causes & RXQ0 != 0 {
            ACKS.fetch_add(1, Ordering::Release);
        }
        write(bar, 0xd0, RXQ0); // Re-arm the source after EIAC auto-clear.
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
    let command = config.read16(p.bdf, 4).unwrap();
    assert!(
        config.write16(p.bdf, 4, command | 2 | 4),
        "PCI_IRQ_FAIL MSI-X decode"
    );
    // Refuse a live DMA engine: this is only a software-cause interrupt test.
    if unsafe { read(p.bar, 0x100) & 2 != 0 || read(p.bar, 0x400) & 2 != 0 } {
        let _ = config.write16(p.bdf, 4, command);
        panic!("PCI_IRQ_FAIL QEMU 82574 RX/TX engine already active");
    }
    let regs = [0x18u64, 0xd0, 0xdc, 0xe4, 0xe8];
    let old = regs.map(|r| unsafe { read(p.bar, r) });
    unsafe {
        write(p.bar, 0xd8, u32::MAX);
        let _ = read(p.bar, 0xc0);
    }
    let vector = irq::allocate(None, Some(acknowledge)).unwrap();
    BAR.store(p.bar, Ordering::Release);
    let table = Table {
        base: p.table,
        bytes: p.bytes,
    };
    let message = crate::irq_x86::message(crate::lapic::apic_id(0).unwrap(), vector).unwrap();
    let saved = match pci_irq::enable_msix(&config, p.bdf, p.msix, &table, 0, message) {
        Ok(s) => s,
        Err(error) => {
            for (r, v) in regs.into_iter().zip(old) {
                unsafe { write(p.bar, r, v) };
            }
            let _ = config.write16(p.bdf, 4, command);
            irq::retire(vector);
            panic!("PCI_IRQ_FAIL MSI-X setup {error:?}");
        }
    };
    unsafe {
        write(p.bar, 0xe4, 8 | (8 << 16)); // RX and misc causes -> table entry zero.
        write(p.bar, 0xe8, 0); // No interrupt throttling in the bounded probe.
                               // Keep causes latched for the ISR's ICR read: EIAC auto-clear would
                               // remove RXQ0 before the handler can verify which source fired.
        write(p.bar, 0x18, (old[0] & !((1 << 27) | (1 << 24))) | (1 << 31));
        write(p.bar, 0xdc, 0);
        write(p.bar, 0xd0, RXQ0);
    }
    let start = irq::count(vector);
    let acks = ACKS.load(Ordering::Acquire);
    let mut passed = true;
    for pulse in 1..=3 {
        unsafe {
            write(p.bar, 0xc8, RXQ0);
            write(p.bar, 0xd0, RXQ0);
        }
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
    unsafe {
        write(p.bar, 0xd8, u32::MAX);
        let _ = read(p.bar, 0xc0);
    }
    let restored = saved.restore(&config, p.bdf, &table);
    let table_restored = (0..4).all(|i| table.read32(i * 4) == Some(saved.words[i as usize]));
    // Restore source routing before its old interrupt mask, and command last.
    for i in [0, 2, 3, 4] {
        unsafe {
            write(p.bar, regs[i], old[i]);
        }
    }
    unsafe {
        write(p.bar, 0xd0, old[1]);
    }
    let restored = restored && config.write16(p.bdf, 4, command);
    irq::retire(vector);
    passed &= restored
        && table_restored
        && ACKS.load(Ordering::Acquire) == acks + 3
        && config.read16(p.bdf, 4) == Some(command)
        && config.read16(p.bdf, p.msix.offset + 2) == Some(saved.control);
    assert!(
        passed,
        "PCI_IRQ_FAIL MSI-X delivery: hits {} acks {} restored {}",
        irq::count(vector) - start,
        ACKS.load(Ordering::Acquire) - acks,
        restored
    );
    crate::println!(
        "PCI_MSIX_PASS vector {vector:#x}, 3 hardware interrupts acknowledged; config restored"
    );
}

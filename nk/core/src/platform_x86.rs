// SPDX-License-Identifier: GPL-2.0-or-later
//! Early x86 firmware discovery, before APs and userspace exist.
use crate::{acpi, bootinfo_x86::BootInfo, pci, println};

struct Identity;
impl acpi::Memory for Identity {
    fn read(&self, address: u64, length: usize) -> Option<&[u8]> {
        if address == 0 || address.checked_add(length as u64)? > 1 << 32 {
            return None;
        }
        // Firmware addresses are physical. The boot map covers 0..4 GiB;
        // table lengths/checksums are validated by the pure parser.
        Some(unsafe { core::slice::from_raw_parts(address as *const u8, length) })
    }
}

fn legacy_rsdp() -> Option<u64> {
    let ebda = (unsafe { core::ptr::read_volatile(0x40e as *const u16) } as u64) << 4;
    for (start, end) in [(ebda, ebda + 1024), (0xe0000, 0x100000)] {
        if start < 0x80000 || end > 0x100000 {
            continue;
        }
        for address in (start..end).step_by(16) {
            if acpi::rsdp_valid(&Identity, address) {
                return Some(address);
            }
        }
    }
    None
}

pub(crate) fn mmio(boot: &BootInfo, start: u64, end: u64) -> bool {
    // The page attributes change whole 2 MiB leaves. Never turn usable RAM
    // (or nk's low user arena) into uncached device memory by rounding.
    let lo = start & !((1 << 21) - 1);
    let hi = end.saturating_add((1 << 21) - 1) & !((1 << 21) - 1);
    start < end
        && lo >= 1 << 30
        && hi <= 1 << 32
        && !boot.ram_regions[..boot.ram_region_count]
            .iter()
            .any(|&(s, e)| lo < e && s < hi)
        && unsafe { crate::paging::identity_mmio(start, end) }
}

/// Discover and print a read-only PCI inventory, then install MADT CPU/APIC
/// topology. Missing ACPI leaves only the BSP active; it never probes CPUs
/// by guessing a sequence of APIC IDs.
pub fn discover(boot: &BootInfo) {
    let rsdp = boot.rsdp.or_else(legacy_rsdp);
    let platform = rsdp.ok_or(acpi::Error::Rsdp).and_then(|address| {
        println!("  acpi:   RSDP {address:#x}");
        acpi::discover(&Identity, address)
    });
    let mut windows = alloc::vec::Vec::new();
    match platform {
        Ok(p) => {
            println!(
                "  acpi:   {} MADT LAPIC {:#x}, {} enabled CPUs, {} IOAPICs, {} overrides",
                if p.xsdt { "XSDT" } else { "RSDT" },
                p.lapic,
                p.cpus.len(),
                p.ioapics.len(),
                p.overrides.len()
            );
            if !mmio(boot, p.lapic, p.lapic + 4096) {
                panic!("LAPIC outside safe boot MMIO map");
            }
            unsafe { crate::lapic::set_base(p.lapic) };
            let ids: alloc::vec::Vec<u32> = p.cpus.iter().map(|c| c.apic_id).collect();
            crate::lapic::configure_topology(&ids);
            for c in &p.cpus {
                println!(
                    "  acpi:   CPU uid {} APIC {}{}",
                    c.uid,
                    c.apic_id,
                    if c.apic_id > 255 {
                        " (x2APIC-only, not started)"
                    } else {
                        ""
                    }
                );
            }
            for io in &p.ioapics {
                println!(
                    "  acpi:   IOAPIC {} at {:#x}, GSI base {}",
                    io.id, io.address, io.gsi_base
                );
            }
            if p.ioapics
                .iter()
                .all(|io| mmio(boot, io.address as u64, io.address as u64 + 4096))
            {
                match unsafe { crate::ioapic::discover(&p.ioapics, &p.overrides) } {
                    Ok(()) => {}
                    Err(error) => {
                        println!("  ioapic: discovery rejected ({error:?}); retaining PIC")
                    }
                }
            } else {
                println!("  ioapic: outside safe boot MMIO map; retaining PIC");
            }
            for iso in &p.overrides {
                println!(
                    "  acpi:   IRQ {}:{} -> GSI {}, flags {:#x}",
                    iso.bus, iso.irq, iso.gsi, iso.flags
                );
            }
            if let Some(f) = p.fadt {
                println!(
                    "  acpi:   FADT flags {:#x}, reset {:?}, PM1 controls {:?} {:?}",
                    f.flags, f.reset, f.pm1a_control, f.pm1b_control
                );
            }
            for w in p.ecam {
                let (start, end) = w.range().unwrap();
                if mmio(boot, start, end) {
                    println!(
                        "  acpi:   MCFG segment {} buses {}..{} base {:#x}",
                        w.segment, w.start_bus, w.end_bus, w.base
                    );
                    windows.push(w);
                } else {
                    println!(
                        "  acpi:   MCFG segment {} outside safe boot MMIO map; skipped",
                        w.segment
                    );
                }
            }
        }
        Err(error) => {
            println!("  acpi:   unavailable ({error:?}); BSP only, legacy PCI config");
            let base = crate::lapic::base() as u64;
            assert!(
                mmio(boot, base, base + 4096),
                "LAPIC outside safe boot MMIO map"
            );
            crate::lapic::configure_topology(&[]);
        }
    }
    // Published once before AP startup and kept for later PCI host calls.
    let windows = alloc::boxed::Box::leak(windows.into_boxed_slice());
    unsafe {
        WINDOWS = windows;
    }
    let config = Hardware;
    let devices = pci::enumerate(&config, windows);
    #[cfg(nk_lkl)]
    crate::lkl_pci_x86::choose(&devices);
    for d in &devices {
        println!(
            "  pci:    {:04x}:{:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}:{:02x}:{:02x}",
            d.bdf.segment,
            d.bdf.bus,
            d.bdf.device,
            d.bdf.function,
            d.vendor,
            d.device,
            d.class,
            d.subclass,
            d.interface
        );
        for (i, bar) in d.bars.iter().enumerate() {
            if !matches!(bar, pci::Bar::None | pci::Bar::Continuation) {
                println!("  pci:      BAR{} {:?}", i, bar);
            }
        }
    }
    if option_env!("NK_PCI_IRQ_TEST").is_some() {
        crate::pci_irq_test::prepare(boot, &config, &devices);
        crate::pci_msix_test::prepare(boot, &config, &devices);
    }
    println!(
        "  pci:    {} functions via {} (inventory only)",
        devices.len(),
        if windows.is_empty() {
            "CF8/CFC"
        } else {
            "ECAM"
        }
    );
}

// Immutable after early discovery. Configuration cycles are serialized even
// for ECAM, so later read-modify-write operations can share the same lock.
static mut WINDOWS: &'static [acpi::Ecam] = &[];
static CONFIG_LOCK: crate::sync::Spinlock = crate::sync::Spinlock::named("PCI config");
pub(crate) struct Hardware;

enum Address {
    Ecam(u64),
    Legacy(u32, u16),
}
impl Hardware {
    fn address(bdf: pci::Bdf, offset: u16) -> Option<Address> {
        if bdf.device >= 32 || bdf.function >= 8 {
            return None;
        }
        let windows = unsafe { *(&raw const WINDOWS) };
        if !windows.is_empty() {
            let w = windows.iter().find(|w| {
                w.segment == bdf.segment && bdf.bus >= w.start_bus && bdf.bus <= w.end_bus
            })?;
            return Some(Address::Ecam(w.address(
                bdf.bus,
                bdf.device,
                bdf.function,
                offset,
            )?));
        }
        if bdf.segment != 0 || offset >= 256 {
            return None;
        }
        Some(Address::Legacy(
            0x80000000
                | ((bdf.bus as u32) << 16)
                | ((bdf.device as u32) << 11)
                | ((bdf.function as u32) << 8)
                | (offset as u32 & !3),
            0xcfc + (offset & 3),
        ))
    }
}
impl pci::Config for Hardware {
    fn read32(&self, bdf: pci::Bdf, offset: u16) -> Option<u32> {
        if offset & 3 != 0 {
            return None;
        }
        let address = Self::address(bdf, offset)?;
        let flags = CONFIG_LOCK.lock();
        let value = unsafe {
            match address {
                Address::Ecam(address) => core::ptr::read_volatile(address as *const u32),
                Address::Legacy(address, port) => {
                    let value: u32;
                    core::arch::asm!("out dx,eax",in("dx")0xcf8u16,in("eax")address,options(nostack));
                    core::arch::asm!("in eax,dx",in("dx")port,out("eax")value,options(nostack));
                    value
                }
            }
        };
        unsafe { CONFIG_LOCK.unlock(flags) };
        Some(value)
    }
}
impl Hardware {
    /// One byte at its own address. LKL writes single-byte registers
    /// (cache line size, latency timer, interrupt line); widening them to a
    /// read-modify-write of the dword would write back Status's RW1C bits.
    pub(crate) fn write8(&self, bdf: pci::Bdf, offset: u16, value: u8) -> bool {
        let Some(address) = Self::address(bdf, offset) else {
            return false;
        };
        let flags = CONFIG_LOCK.lock();
        unsafe {
            match address {
                Address::Ecam(address) => core::ptr::write_volatile(address as *mut u8, value),
                Address::Legacy(address, port) => {
                    core::arch::asm!("out dx,eax",in("dx")0xcf8u16,in("eax")address,options(nostack));
                    core::arch::asm!("out dx,al",in("dx")port,in("al")value,options(nostack));
                }
            }
            CONFIG_LOCK.unlock(flags);
        }
        true
    }
}
impl pci::ConfigWrite for Hardware {
    fn write16(&self, bdf: pci::Bdf, offset: u16, value: u16) -> bool {
        if offset & 1 != 0 {
            return false;
        }
        let Some(address) = Self::address(bdf, offset) else {
            return false;
        };
        let flags = CONFIG_LOCK.lock();
        unsafe {
            match address {
                Address::Ecam(address) => core::ptr::write_volatile(address as *mut u16, value),
                Address::Legacy(address, port) => {
                    core::arch::asm!("out dx,eax",in("dx")0xcf8u16,in("eax")address,options(nostack));
                    core::arch::asm!("out dx,ax",in("dx")port,in("ax")value,options(nostack));
                }
            }
            CONFIG_LOCK.unlock(flags);
        }
        true
    }
    fn write32(&self, bdf: pci::Bdf, offset: u16, value: u32) -> bool {
        if offset & 3 != 0 {
            return false;
        }
        let Some(address) = Self::address(bdf, offset) else {
            return false;
        };
        let flags = CONFIG_LOCK.lock();
        unsafe {
            match address {
                Address::Ecam(address) => core::ptr::write_volatile(address as *mut u32, value),
                Address::Legacy(address, port) => {
                    core::arch::asm!("out dx,eax",in("dx")0xcf8u16,in("eax")address,options(nostack));
                    core::arch::asm!("out dx,eax",in("dx")port,in("eax")value,options(nostack));
                }
            }
            CONFIG_LOCK.unlock(flags);
        }
        true
    }
}

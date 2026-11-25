// SPDX-License-Identifier: GPL-2.0-or-later
//! IOAPIC register access and MADT ISA routing. Not an AML PCI _PRT router.
use crate::{
    acpi,
    irq_x86::{self, Error, Route},
};
use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
struct Chip {
    address: u64,
    gsi: u32,
    pins: u32,
}
const EMPTY: Chip = Chip {
    address: 0,
    gsi: 0,
    pins: 0,
};
static mut CHIPS: [Chip; 8] = [EMPTY; 8];
static COUNT: AtomicUsize = AtomicUsize::new(0);
static mut PIT: Option<Route> = None;
static LOCK: crate::sync::Spinlock = crate::sync::Spinlock::named("IOAPIC");

unsafe fn read(address: u64, reg: u32) -> u32 {
    core::ptr::write_volatile(address as *mut u32, reg);
    core::ptr::read_volatile((address + 0x10) as *const u32)
}
unsafe fn write(address: u64, reg: u32, value: u32) {
    core::ptr::write_volatile(address as *mut u32, reg);
    core::ptr::write_volatile((address + 0x10) as *mut u32, value);
}

/// Boot-only publication, after the MMIO pages are mapped uncached and
/// before any AP can access IOREGSEL/IOWIN. Does not change firmware routes.
pub unsafe fn discover(
    records: &[acpi::IoApic],
    overrides: &[acpi::Override],
) -> Result<(), Error> {
    if records.len() > 8 {
        return Err(Error::Range);
    }
    let mut chips = [EMPTY; 8];
    for (i, record) in records.iter().enumerate() {
        let pins = ((read(record.address as u64, 1) >> 16) & 255) + 1;
        if pins > 120 || record.gsi_base.checked_add(pins).is_none() {
            return Err(Error::Range);
        }
        let chip = Chip {
            address: record.address as u64,
            gsi: record.gsi_base,
            pins,
        };
        if chips[..i]
            .iter()
            .any(|c| c.gsi < chip.gsi + pins && chip.gsi < c.gsi + c.pins)
        {
            return Err(Error::Range);
        }
        chips[i] = chip;
        crate::println!(
            "  ioapic: at {:#x}, GSI {}..{}",
            chip.address,
            chip.gsi,
            chip.gsi + pins - 1
        );
    }
    // Validate before publishing either table: bad MADT IRQ0 metadata must
    // not silently select an ISA default route.
    let pit = irq_x86::isa_route(0, overrides)?;
    CHIPS = chips;
    PIT = Some(pit);
    COUNT.store(records.len(), Ordering::Release);
    Ok(())
}

pub fn route(route: Route, vector: u8, destination: u32, masked: bool) -> Result<(), Error> {
    let value = route.redirection(vector, destination, masked)?;
    let guard = LOCK.lock();
    let result = unsafe {
        let chips = &*(&raw const CHIPS);
        match chips[..COUNT.load(Ordering::Acquire)]
            .iter()
            .find_map(|c| irq_x86::pin(route.gsi, c.gsi, c.pins).map(|p| (*c, p)))
        {
            Some((chip, pin)) => {
                let reg = 0x10 + 2 * pin;
                let low = read(chip.address, reg);
                write(chip.address, reg, low | (1 << 16)); // Mask before changing destination.
                write(chip.address, reg + 1, (value >> 32) as u32);
                write(chip.address, reg, value as u32);
                // Posted writes must reach the controller before unmasking
                // the source/returning success to a caller.
                let actual = read(chip.address, reg) & !(1 << 12 | 1 << 14);
                if actual == value as u32 {
                    Ok(())
                } else {
                    Err(Error::Range)
                }
            }
            None => Err(Error::Range),
        }
    };
    unsafe { LOCK.unlock(guard) };
    result
}

/// Transfer the BSP's PIT to its ACPI-discovered GSI. All AP local timers
/// remain unchanged. Without a usable IOAPIC keep the PIC fallback.
pub fn route_pit() -> bool {
    if COUNT.load(Ordering::Acquire) == 0 {
        return false;
    }
    let Some(pit) = (unsafe { *(&raw const PIT) }) else {
        return false;
    };
    let Some(destination) = crate::lapic::apic_id(0) else {
        return false;
    };
    // IRQ0 is masked on the PIC before opening the IOAPIC route, avoiding
    // duplicate ticks from two interrupt controllers for the same PIT edge.
    let flags = crate::sync::irq_save();
    unsafe { crate::interrupts_x86::pic_mask(0, true) };
    let routed = route(pit, irq_x86::PIT_VECTOR, destination, false).is_ok();
    if routed {
        crate::println!(
            "  ioapic: PIT IRQ0 -> GSI {} vector {:#x} APIC {}",
            pit.gsi,
            irq_x86::PIT_VECTOR,
            destination
        );
    } else {
        // Leave no competing route if programming/readback failed.
        let _ = route(pit, irq_x86::PIT_VECTOR, destination, true);
        unsafe { crate::interrupts_x86::pic_mask(0, false) };
        crate::println!("  ioapic: PIT route failed; retaining PIC timer");
    }
    unsafe { crate::sync::irq_restore(flags) };
    routed
}

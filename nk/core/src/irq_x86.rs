// SPDX-License-Identifier: GPL-2.0-or-later
//! Pure x86 interrupt routing/encoding, shared by the host tests and nk.
use crate::acpi::Override;

pub const DEVICE_FIRST: u8 = 0x50;
pub const DEVICE_LAST: u8 = 0xdf;
pub const PIT_VECTOR: u8 = 0x40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Flags,
    Duplicate,
    Destination,
    Vector,
    Range,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub gsi: u32,
    pub low: bool,
    pub level: bool,
}

/// ISA defaults are active-high/edge. ACPI's "conforms" value inherits
/// those defaults; polarity/trigger encoding 2 is reserved, never a guess.
pub fn isa_route(irq: u8, overrides: &[Override]) -> Result<Route, Error> {
    let mut records = overrides.iter().filter(|r| r.bus == 0 && r.irq == irq);
    let Some(r) = records.next() else {
        return Ok(Route {
            gsi: irq as u32,
            low: false,
            level: false,
        });
    };
    if records.next().is_some() {
        return Err(Error::Duplicate);
    }
    if r.flags & !15 != 0 || r.flags & 3 == 2 || (r.flags >> 2) & 3 == 2 {
        return Err(Error::Flags);
    }
    Ok(Route {
        gsi: r.gsi,
        low: r.flags & 3 == 3,
        level: (r.flags >> 2) & 3 == 3,
    })
}

impl Route {
    /// Fixed delivery to one physical xAPIC destination. Callers mask the
    /// low word before changing destination, then write the final low word.
    pub fn redirection(self, vector: u8, destination: u32, masked: bool) -> Result<u64, Error> {
        if destination > 255 {
            return Err(Error::Destination);
        }
        if vector != PIT_VECTOR && !(DEVICE_FIRST..=DEVICE_LAST).contains(&vector) {
            return Err(Error::Vector);
        }
        Ok(((destination as u64) << 56)
            | vector as u64
            | ((self.low as u64) << 13)
            | ((self.level as u64) << 15)
            | ((masked as u64) << 16))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    pub address: u64,
    pub data: u32,
}
/// Physical destination, fixed delivery, edge-triggered MSI/MSI-X message.
/// The MSI aperture is architectural, independent of the LAPIC register base.
pub fn message(destination: u32, vector: u8) -> Result<Message, Error> {
    if destination > 255 {
        return Err(Error::Destination);
    }
    if !(DEVICE_FIRST..=DEVICE_LAST).contains(&vector) {
        return Err(Error::Vector);
    }
    Ok(Message {
        address: 0xfee00000 | ((destination as u64) << 12),
        data: vector as u32,
    })
}

pub fn pin(gsi: u32, base: u32, pins: u32) -> Option<u32> {
    let pin = gsi.checked_sub(base)?;
    (pin < pins && pins <= 120).then_some(pin)
}

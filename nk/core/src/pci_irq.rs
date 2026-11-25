// SPDX-License-Identifier: GPL-2.0-or-later
//! Bounded PCI capability parsing and single-message MSI programming.
//! Callers own the device and supply bounds-checked MSI-X table mappings.
use crate::{
    irq_x86::Message,
    pci::{Bdf, Config, ConfigWrite},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Access,
    Header,
    Pointer,
    Cycle,
    Truncated,
    Bar,
    Enabled,
}

pub fn capability(config: &impl Config, bdf: Bdf, wanted: u8) -> Result<Option<u16>, Error> {
    if config.read16(bdf, 6).ok_or(Error::Access)? & 0x10 == 0 {
        return Ok(None);
    }
    let header = (config.read32(bdf, 12).ok_or(Error::Access)? >> 16) as u8 & 0x7f;
    let pointer = match header {
        0 | 1 => 0x34,
        2 => 0x14,
        _ => return Err(Error::Header),
    };
    let mut at = config.read32(bdf, pointer).ok_or(Error::Access)? as u8;
    let mut seen = 0u64;
    let mut found = None;
    while at != 0 {
        if at < 0x40 || at & 3 != 0 {
            return Err(Error::Pointer);
        }
        let bit = 1u64 << (at / 4);
        if seen & bit != 0 {
            return Err(Error::Cycle);
        }
        seen |= bit;
        let head = config.read32(bdf, at as u16).ok_or(Error::Access)?;
        if head as u8 == wanted {
            found = Some(at as u16);
        }
        at = (head >> 8) as u8;
    }
    Ok(found)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Msi {
    pub offset: u16,
    pub control: u16,
    pub address64: bool,
    pub maskable: bool,
}
impl Msi {
    pub fn data(self) -> u16 {
        self.offset + if self.address64 { 12 } else { 8 }
    }
    pub fn mask(self) -> Option<u16> {
        (self.maskable).then_some(self.offset + if self.address64 { 16 } else { 12 })
    }
}
pub fn msi(config: &impl Config, bdf: Bdf) -> Result<Option<Msi>, Error> {
    let Some(offset) = capability(config, bdf, 5)? else {
        return Ok(None);
    };
    let control = config.read16(bdf, offset + 2).ok_or(Error::Access)?;
    let m = Msi {
        offset,
        control,
        address64: control & (1 << 7) != 0,
        maskable: control & (1 << 8) != 0,
    };
    let end = if let Some(mask) = m.mask() {
        mask + 8
    } else {
        m.data() + 2
    };
    if end > 256 {
        return Err(Error::Truncated);
    }
    Ok(Some(m))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Msix {
    pub offset: u16,
    pub count: u16,
    pub table_bar: u8,
    pub table_offset: u32,
    pub pending_bar: u8,
    pub pending_offset: u32,
}
pub fn msix(config: &impl Config, bdf: Bdf) -> Result<Option<Msix>, Error> {
    let Some(offset) = capability(config, bdf, 0x11)? else {
        return Ok(None);
    };
    if offset + 12 > 256 {
        return Err(Error::Truncated);
    }
    let control = config.read16(bdf, offset + 2).ok_or(Error::Access)?;
    let table = config.read32(bdf, offset + 4).ok_or(Error::Access)?;
    let pending = config.read32(bdf, offset + 8).ok_or(Error::Access)?;
    if table & 7 >= 6 || pending & 7 >= 6 {
        return Err(Error::Bar);
    }
    Ok(Some(Msix {
        offset,
        count: (control & 0x7ff) + 1,
        table_bar: (table & 7) as u8,
        table_offset: table & !7,
        pending_bar: (pending & 7) as u8,
        pending_offset: pending & !7,
    }))
}

#[derive(Clone, Copy, Debug)]
pub struct SavedMsi {
    pub capability: Msi,
    pub command: u16,
    pub address: u32,
    pub high: u32,
    pub data: u16,
    pub mask: u32,
}
impl SavedMsi {
    /// Restore with MSI disabled, then command/control last. Caller owns
    /// this device and must quiesce its interrupt source before teardown.
    pub fn restore(self, config: &impl ConfigWrite, bdf: Bdf) -> bool {
        let m = self.capability;
        let mut ok = config.write16(bdf, m.offset + 2, m.control & !1);
        ok &= config.write32(bdf, m.offset + 4, self.address);
        if m.address64 {
            ok &= config.write32(bdf, m.offset + 8, self.high);
        }
        ok &= config.write16(bdf, m.data(), self.data);
        if let Some(offset) = m.mask() {
            ok &= config.write32(bdf, offset, self.mask);
        }
        ok &= config.write16(bdf, 4, self.command);
        ok &= config.write16(bdf, m.offset + 2, m.control);
        ok
    }
}
/// Program one vector without enabling bus mastering. Writes are confined
/// to a caller-owned MSI capability and Command's native 16-bit register.
pub fn enable_msi(
    config: &impl ConfigWrite,
    bdf: Bdf,
    m: Msi,
    message: Message,
) -> Result<SavedMsi, Error> {
    if m.control & 1 != 0 {
        return Err(Error::Enabled);
    }
    if let Some(other) = msix(config, bdf)? {
        if config.read16(bdf, other.offset + 2).ok_or(Error::Access)? & (1 << 15) != 0 {
            return Err(Error::Enabled);
        }
    }
    if message.data > u16::MAX as u32 || (!m.address64 && message.address >> 32 != 0) {
        return Err(Error::Truncated);
    }
    let saved = SavedMsi {
        capability: m,
        command: config.read16(bdf, 4).ok_or(Error::Access)?,
        address: config.read32(bdf, m.offset + 4).ok_or(Error::Access)?,
        high: if m.address64 {
            config.read32(bdf, m.offset + 8).ok_or(Error::Access)?
        } else {
            0
        },
        data: config.read16(bdf, m.data()).ok_or(Error::Access)?,
        mask: if let Some(offset) = m.mask() {
            config.read32(bdf, offset).ok_or(Error::Access)?
        } else {
            0
        },
    };
    let mut ok = config.write16(bdf, m.offset + 2, m.control & !0x71); // Disabled, MME=0.
    ok &= config.write32(bdf, m.offset + 4, message.address as u32);
    if m.address64 {
        ok &= config.write32(bdf, m.offset + 8, (message.address >> 32) as u32);
    }
    ok &= config.write16(bdf, m.data(), message.data as u16);
    if let Some(offset) = m.mask() {
        ok &= config.write32(bdf, offset, saved.mask & !1);
    }
    ok &= config.write16(bdf, 4, saved.command | 2 | (1 << 10)); // Memory decode, INTx disable; no DMA enable.
    if ok {
        ok = config.write16(bdf, m.offset + 2, (m.control & !0x70) | 1);
    }
    if !ok {
        let _ = saved.restore(config, bdf);
        return Err(Error::Access);
    }
    Ok(saved)
}

impl Msix {
    pub fn table_range(self, bar: u64, bar_bytes: u64) -> Option<(u64, u64)> {
        let bytes = self.count as u64 * 16;
        let offset = self.table_offset as u64;
        if offset.checked_add(bytes)? > bar_bytes {
            return None;
        }
        let start = bar.checked_add(offset)?;
        Some((start, start.checked_add(bytes)?))
    }
}
/// Mapping begins at the MSI-X table offset, not the BAR base. Implementors
/// enforce MMIO alignment/bounds; callers verify the span against the BAR.
pub trait MsixTable {
    fn len(&self) -> u64;
    fn read32(&self, offset: u32) -> Option<u32>;
    fn write32(&self, offset: u32, value: u32) -> bool;
}
#[derive(Clone, Copy, Debug)]
pub struct SavedMsix {
    pub capability: Msix,
    pub control: u16,
    pub command: u16,
    pub index: u16,
    pub words: [u32; 4],
}
impl SavedMsix {
    pub fn restore(self, config: &impl ConfigWrite, bdf: Bdf, table: &impl MsixTable) -> bool {
        let offset = self.index as u32 * 16;
        let mut ok = config.write16(
            bdf,
            self.capability.offset + 2,
            (self.control | (1 << 14)) & !(1 << 15),
        );
        ok &= table.write32(offset + 12, self.words[3] | 1);
        for i in 0..3 {
            ok &= table.write32(offset + i * 4, self.words[i as usize]);
        }
        ok &= table.write32(offset + 12, self.words[3]);
        ok &= config.write16(bdf, 4, self.command);
        ok &= config.write16(bdf, self.capability.offset + 2, self.control);
        ok
    }
}
/// Enable one MSI-X entry on an exclusively owned, quiescent device.
/// Memory decoding must already be enabled so table snapshots are valid.
/// Bus mastering remains the caller's choice, as for ordinary MSI.
pub fn enable_msix(
    config: &impl ConfigWrite,
    bdf: Bdf,
    m: Msix,
    table: &impl MsixTable,
    index: u16,
    message: Message,
) -> Result<SavedMsix, Error> {
    if index >= m.count || table.len() < m.count as u64 * 16 {
        return Err(Error::Truncated);
    }
    let control = config.read16(bdf, m.offset + 2).ok_or(Error::Access)?;
    if control & (1 << 15) != 0 {
        return Err(Error::Enabled);
    }
    if msi(config, bdf)?.is_some_and(|m| m.control & 1 != 0) {
        return Err(Error::Enabled);
    }
    let command = config.read16(bdf, 4).ok_or(Error::Access)?;
    if command & 2 == 0 {
        return Err(Error::Access);
    }
    let offset = index as u32 * 16;
    let mut words = [0; 4];
    for i in 0..4 {
        words[i] = table.read32(offset + i as u32 * 4).ok_or(Error::Access)?;
    }
    let saved = SavedMsix {
        capability: m,
        control,
        command,
        index,
        words,
    };
    let mut ok = config.write16(bdf, m.offset + 2, control | (1 << 14)); // Function mask during programming.
    ok &= table.write32(offset + 12, words[3] | 1);
    ok &= table.write32(offset, message.address as u32);
    ok &= table.write32(offset + 4, (message.address >> 32) as u32);
    ok &= table.write32(offset + 8, message.data);
    ok &= table.write32(offset + 12, words[3] & !1);
    // Flush posted table writes before MSI-X becomes enabled.
    ok &= table.read32(offset) == Some(message.address as u32)
        && table.read32(offset + 4) == Some((message.address >> 32) as u32)
        && table.read32(offset + 8) == Some(message.data)
        && table.read32(offset + 12) == Some(words[3] & !1);
    ok &= config.write16(bdf, 4, command | (1 << 10));
    if ok {
        ok = config.write16(bdf, m.offset + 2, (control | (1 << 15)) & !(1 << 14));
    }
    if !ok {
        let _ = saved.restore(config, bdf, table);
        return Err(Error::Access);
    }
    Ok(saved)
}

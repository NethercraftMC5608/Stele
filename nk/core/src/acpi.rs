// SPDX-License-Identifier: GPL-2.0-or-later
//! Bounded, checksum-checked ACPI discovery, independent of physical mapping.
//! Firmware tables are read only. AML (_S5, _PRT, power sequencing) is not
//! interpreted here; FADT register discovery alone does not implement shutdown.
use alloc::vec::Vec;

const HEADER: usize = 36;
const MAX_TABLE: usize = 1 << 20;

pub trait Memory {
    fn read(&self, address: u64, length: usize) -> Option<&[u8]>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Unmapped,
    Rsdp,
    Table,
    Madt,
    Mcfg,
    MissingMadt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cpu {
    pub uid: u32,
    pub apic_id: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApic {
    pub id: u8,
    pub address: u32,
    pub gsi_base: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Override {
    pub bus: u8,
    pub irq: u8,
    pub gsi: u32,
    pub flags: u16,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ecam {
    pub base: u64,
    pub segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
}
impl Ecam {
    /// ACPI MCFG bases are relative to bus zero, including nonzero-start ranges.
    pub fn address(&self, bus: u8, device: u8, function: u8, offset: u16) -> Option<u64> {
        if bus < self.start_bus
            || bus > self.end_bus
            || device >= 32
            || function >= 8
            || offset >= 4096
        {
            return None;
        }
        self.base.checked_add(
            ((bus as u64) << 20)
                | ((device as u64) << 15)
                | ((function as u64) << 12)
                | offset as u64,
        )
    }
    pub fn range(&self) -> Option<(u64, u64)> {
        Some((
            self.base.checked_add((self.start_bus as u64) << 20)?,
            self.base.checked_add(((self.end_bus as u64) + 1) << 20)?,
        ))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Register {
    pub space: u8,
    pub width: u8,
    pub bit_offset: u8,
    pub access: u8,
    pub address: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fadt {
    pub flags: u32,
    pub reset: Option<(Register, u8)>,
    pub pm1a_control: Option<Register>,
    pub pm1b_control: Option<Register>,
}
#[derive(Debug)]
pub struct Platform {
    pub xsdt: bool,
    pub lapic: u64,
    pub cpus: Vec<Cpu>,
    pub ioapics: Vec<IoApic>,
    pub overrides: Vec<Override>,
    pub ecam: Vec<Ecam>,
    pub fadt: Option<Fadt>,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn checksum(b: &[u8]) -> bool {
    b.iter().fold(0u8, |s, v| s.wrapping_add(*v)) == 0
}

pub fn rsdp_valid(memory: &impl Memory, address: u64) -> bool {
    rsdp(memory, address).is_ok()
}

fn rsdp(memory: &impl Memory, address: u64) -> Result<(u64, u64), Error> {
    let short = memory.read(address, 20).ok_or(Error::Unmapped)?;
    if &short[..8] != b"RSD PTR " || !checksum(short) {
        return Err(Error::Rsdp);
    }
    let rsdt = u32_at(short, 16) as u64;
    let xsdt = if short[15] >= 2 {
        let header = memory.read(address, 36).ok_or(Error::Unmapped)?;
        let length = u32_at(header, 20) as usize;
        if !(36..=4096).contains(&length) {
            return Err(Error::Rsdp);
        }
        if !checksum(memory.read(address, length).ok_or(Error::Unmapped)?) {
            return Err(Error::Rsdp);
        }
        u64_at(header, 24)
    } else {
        0
    };
    Ok((rsdt, xsdt))
}

fn table(memory: &impl Memory, address: u64) -> Result<&[u8], Error> {
    if address == 0 {
        return Err(Error::Table);
    }
    let header = memory.read(address, HEADER).ok_or(Error::Unmapped)?;
    let length = u32_at(header, 4) as usize;
    if !(HEADER..=MAX_TABLE).contains(&length) {
        return Err(Error::Table);
    }
    let bytes = memory.read(address, length).ok_or(Error::Unmapped)?;
    if !checksum(bytes) {
        return Err(Error::Table);
    }
    Ok(bytes)
}

pub fn discover(memory: &impl Memory, address: u64) -> Result<Platform, Error> {
    let (rsdt, xsdt) = rsdp(memory, address)?;
    let root = table(memory, xsdt)
        .ok()
        .filter(|b| &b[..4] == b"XSDT" && (b.len() - HEADER) % 8 == 0);
    let (root, width) = match root {
        Some(root) => (root, 8),
        None => {
            let root = table(memory, rsdt)?;
            if &root[..4] != b"RSDT" || (root.len() - HEADER) % 4 != 0 {
                return Err(Error::Table);
            }
            (root, 4)
        }
    };
    let mut p = Platform {
        xsdt: width == 8,
        lapic: 0,
        cpus: Vec::new(),
        ioapics: Vec::new(),
        overrides: Vec::new(),
        ecam: Vec::new(),
        fadt: None,
    };
    let mut found_madt = false;
    for entry in root[HEADER..].chunks_exact(width) {
        let addr = if width == 8 {
            u64_at(entry, 0)
        } else {
            u32_at(entry, 0) as u64
        };
        if addr == 0 {
            continue;
        }
        // Only dereference full tables this discovery layer understands.
        let sig = memory.read(addr, 4).ok_or(Error::Unmapped)?;
        if sig != b"APIC" && sig != b"MCFG" && sig != b"FACP" {
            continue;
        }
        let bytes = table(memory, addr)?;
        match sig {
            b"APIC" => {
                if found_madt {
                    return Err(Error::Madt);
                }
                madt(bytes, &mut p)?;
                found_madt = true;
            }
            b"MCFG" => mcfg(bytes, &mut p.ecam)?,
            b"FACP" => p.fadt = Some(fadt(bytes)?),
            _ => {}
        }
    }
    if !found_madt {
        return Err(Error::MissingMadt);
    }
    Ok(p)
}

fn madt(bytes: &[u8], p: &mut Platform) -> Result<(), Error> {
    if bytes.len() < 44 {
        return Err(Error::Madt);
    }
    p.lapic = u32_at(bytes, 36) as u64;
    let mut at = 44;
    while at < bytes.len() {
        let head = bytes.get(at..at + 2).ok_or(Error::Madt)?;
        let len = head[1] as usize;
        if len < 2 {
            return Err(Error::Madt);
        }
        let r = bytes.get(at..at + len).ok_or(Error::Madt)?;
        match r[0] {
            0 => {
                if len < 8 {
                    return Err(Error::Madt);
                }
                if u32_at(r, 4) & 1 != 0 {
                    add_cpu(
                        p,
                        Cpu {
                            uid: r[2] as u32,
                            apic_id: r[3] as u32,
                        },
                    )?;
                }
            }
            1 => {
                if len < 12 {
                    return Err(Error::Madt);
                }
                let address = u32_at(r, 4);
                if address == 0 || address & 4095 != 0 {
                    return Err(Error::Madt);
                }
                p.ioapics.push(IoApic {
                    id: r[2],
                    address,
                    gsi_base: u32_at(r, 8),
                });
            }
            2 => {
                if len < 10 {
                    return Err(Error::Madt);
                }
                p.overrides.push(Override {
                    bus: r[2],
                    irq: r[3],
                    gsi: u32_at(r, 4),
                    flags: u16_at(r, 8),
                });
            }
            5 => {
                if len < 12 {
                    return Err(Error::Madt);
                }
                p.lapic = u64_at(r, 4);
            }
            9 => {
                if len < 16 {
                    return Err(Error::Madt);
                }
                if u32_at(r, 8) & 1 != 0 {
                    add_cpu(
                        p,
                        Cpu {
                            apic_id: u32_at(r, 4),
                            uid: u32_at(r, 12),
                        },
                    )?;
                }
            }
            _ => {} // Includes local NMI records; not IRQ routes.
        }
        at += len;
    }
    if p.lapic == 0 || p.lapic & 4095 != 0 || p.cpus.is_empty() {
        return Err(Error::Madt);
    }
    Ok(())
}
fn add_cpu(p: &mut Platform, cpu: Cpu) -> Result<(), Error> {
    if p.cpus
        .iter()
        .any(|c| c.apic_id == cpu.apic_id || c.uid == cpu.uid)
    {
        return Err(Error::Madt);
    }
    p.cpus.push(cpu);
    Ok(())
}

fn mcfg(bytes: &[u8], windows: &mut Vec<Ecam>) -> Result<(), Error> {
    if bytes.len() < 44 || (bytes.len() - 44) % 16 != 0 {
        return Err(Error::Mcfg);
    }
    for r in bytes[44..].chunks_exact(16) {
        let w = Ecam {
            base: u64_at(r, 0),
            segment: u16_at(r, 8),
            start_bus: r[10],
            end_bus: r[11],
        };
        if w.base == 0
            || w.base & ((1 << 20) - 1) != 0
            || w.start_bus > w.end_bus
            || w.range().is_none()
            || windows.iter().any(|x| {
                x.segment == w.segment && x.start_bus <= w.end_bus && w.start_bus <= x.end_bus
            })
        {
            return Err(Error::Mcfg);
        }
        windows.push(w);
    }
    Ok(())
}

fn gas(bytes: &[u8], at: usize) -> Option<Register> {
    let b = bytes.get(at..at + 12)?;
    let address = u64_at(b, 4);
    (address != 0).then_some(Register {
        space: b[0],
        width: b[1],
        bit_offset: b[2],
        access: b[3],
        address,
    })
}
fn control(bytes: &[u8], legacy: usize, extended: usize) -> Option<Register> {
    gas(bytes, extended).or_else(|| {
        let address = u32_at(bytes, legacy) as u64;
        (address != 0).then_some(Register {
            space: 1,
            width: bytes[89].saturating_mul(8),
            bit_offset: 0,
            access: 0,
            address,
        })
    })
}
fn fadt(bytes: &[u8]) -> Result<Fadt, Error> {
    if bytes.len() < 116 {
        return Err(Error::Table);
    }
    let flags = u32_at(bytes, 112);
    let reset = if flags & (1 << 10) != 0 {
        gas(bytes, 116).zip(bytes.get(128).copied())
    } else {
        None
    };
    Ok(Fadt {
        flags,
        reset,
        pm1a_control: control(bytes, 64, 172),
        pm1b_control: control(bytes, 68, 184),
    })
}

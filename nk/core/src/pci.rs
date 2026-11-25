// SPDX-License-Identifier: GPL-2.0-or-later
//! Read-only PCI inventory. No BAR sizing writes, resource assignment, DMA,
//! IRQ programming, bus-master enabling or native device drivers here.
use crate::acpi::Ecam;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bdf {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}
pub trait Config {
    fn read32(&self, bdf: Bdf, offset: u16) -> Option<u32>;
    fn read16(&self,bdf:Bdf,offset:u16)->Option<u16> {
        if offset&1 != 0 { return None; }
        Some((self.read32(bdf,offset&!3)? >> ((offset&3)*8)) as u16)
    }
}
/// Separate from read-only inventory: native-width writes avoid echoing
/// write-one-to-clear PCI Status bits when changing the Command register.
pub trait ConfigWrite: Config {
    fn write16(&self,bdf:Bdf,offset:u16,value:u16)->bool;
    fn write32(&self,bdf:Bdf,offset:u16,value:u32)->bool;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar {
    None,
    Io(u32),
    Memory32(u32),
    Memory64(u64),
    Continuation,
    Reserved,
}
#[derive(Debug)]
pub struct Device {
    pub bdf: Bdf,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub interface: u8,
    pub header: u8,
    pub bars: [Bar; 6],
}

fn function(config: &impl Config, bdf: Bdf) -> Option<Device> {
    let id = config.read32(bdf, 0)?;
    if id as u16 == 0xffff {
        return None;
    }
    let class = config.read32(bdf, 8)?;
    let header = (config.read32(bdf, 12)? >> 16) as u8;
    let count = match header & 0x7f {
        0 => 6,
        1 => 2,
        _ => 0,
    };
    let mut bars = [Bar::None; 6];
    let mut i = 0;
    while i < count {
        let raw = config.read32(bdf, 16 + i as u16 * 4)?;
        bars[i] = if raw == 0 {
            Bar::None
        } else if raw & 1 != 0 {
            Bar::Io(raw & !3)
        } else {
            match (raw >> 1) & 3 {
                0 => Bar::Memory32(raw & !15),
                2 if i + 1 < count => {
                    let high = config.read32(bdf, 16 + (i as u16 + 1) * 4)?;
                    bars[i + 1] = Bar::Continuation;
                    Bar::Memory64(((high as u64) << 32) | (raw as u64 & !15))
                }
                _ => Bar::Reserved,
            }
        };
        i += if matches!(bars[i], Bar::Memory64(_)) {
            2
        } else {
            1
        };
    }
    Some(Device {
        bdf,
        vendor: id as u16,
        device: (id >> 16) as u16,
        class: (class >> 24) as u8,
        subclass: (class >> 16) as u8,
        interface: (class >> 8) as u8,
        header,
        bars,
    })
}

/// Scan all advertised buses (including bridges), and functions 1..7 only
/// when function zero has the multifunction flag. An empty window list means
/// legacy segment-zero configuration space, not an invented ECAM address.
pub fn enumerate(config: &impl Config, windows: &[Ecam]) -> Vec<Device> {
    let mut devices = Vec::new();
    if windows.is_empty() {
        scan(config, 0, 0, 255, &mut devices);
    } else {
        for w in windows {
            scan(config, w.segment, w.start_bus, w.end_bus, &mut devices);
        }
    }
    devices
}
fn scan(config: &impl Config, segment: u16, first: u8, last: u8, devices: &mut Vec<Device>) {
    for bus in first..=last {
        for device in 0..32 {
            let bdf = Bdf {
                segment,
                bus,
                device,
                function: 0,
            };
            let Some(zero) = function(config, bdf) else {
                continue;
            };
            let multi = zero.header & 0x80 != 0;
            devices.push(zero);
            if multi {
                for f in 1..8 {
                    if let Some(d) = function(config, Bdf { function: f, ..bdf }) {
                        devices.push(d);
                    }
                }
            }
        }
    }
}

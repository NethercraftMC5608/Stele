// SPDX-License-Identifier: GPL-2.0-or-later
//! LKL's PCI host operations, served by nk on x86-64.
//!
//! arch/lkl/drivers/pci.c hosts exactly one PCI device, named on the kernel
//! command line as `lkl_pci=<segment:bus:device.function>`. Linux sees it as
//! bus 0, devfn 0 of a private root bus, and everything it does to the device
//! comes through `nk_pci_ops`:
//!
//! - **config space**: `read`/`write`, passed to nk's ECAM (or CF8/CFC)
//!   accessor at native width. Linux's BAR sizing writes go to the device.
//! - **BARs**: `resource_alloc` returns the address Linux uses as the BAR's
//!   start. Below 4 GiB that is the BAR's physical address in nk's identity
//!   map, remapped uncached; above it (firmware puts 64-bit BARs high: QEMU's
//!   NVMe sits at 768 GiB) it is a mapping in nk's kernel MMIO window.
//!   `host_ioremap` is the identity and `host_iomem_access` makes exact-width
//!   volatile accesses, so either address works as-is.
//! - **DMA**: `map_page` returns the bus address. LKL's linear map is
//!   identity with physical memory and there is no IOMMU in play, so it is
//!   the address itself.
//! - **interrupts**: pci-msi.c allocates one Linux IRQ per vector and calls
//!   `msi_init`. nk gives each a device vector whose dispatch raises that IRQ
//!   through the LKL IRQ pump, and writes the matching message into the
//!   device's MSI capability or MSI-X table. Linux then sets the enable bits
//!   and masks/unmasks entries itself, through the same identity BAR mapping.
//!   INTx would need ACPI `_PRT` (AML) and is not routed: `irq_init` accepts
//!   and does nothing, so drivers must use MSI or MSI-X.
//!
//! Every entry point runs in Linux's context on the CPU LKL is pinned to, so
//! the device state needs no lock beyond Linux's own serialisation.

use crate::pci::{Bdf, Config, ConfigWrite};
use crate::platform_x86::Hardware;
use core::ffi::{c_char, c_int, c_ulong, c_ulonglong, c_void};

const MAX_VECTORS: usize = 32;
const LKL_PCI_IRQ_MSI: c_int = 1;
const LKL_PCI_IRQ_MSIX: c_int = 2;

struct Dev {
    bdf: Bdf,
    vectors: [u8; MAX_VECTORS],
    nvec: usize,
    /// Where each BAR was mapped by `resource_alloc`, and its size.
    bar_va: [(u64, u64); 6],
}

#[repr(C)]
pub struct LklDevPciOps {
    add: unsafe extern "C" fn(*const c_char, *mut c_void, c_ulong) -> *mut Dev,
    remove: unsafe extern "C" fn(*mut Dev),
    irq_init: unsafe extern "C" fn(*mut Dev, c_int) -> c_int,
    msi_init: unsafe extern "C" fn(*mut Dev, c_int, c_int, *mut c_int) -> c_int,
    msi_teardown: unsafe extern "C" fn(*mut Dev, c_int),
    read: unsafe extern "C" fn(*mut Dev, c_int, c_int, *mut c_void) -> c_int,
    write: unsafe extern "C" fn(*mut Dev, c_int, c_int, *mut c_void) -> c_int,
    resource_alloc: unsafe extern "C" fn(*mut Dev, c_ulong, c_int) -> *mut c_void,
    map_page: unsafe extern "C" fn(*mut Dev, *mut c_void, c_ulong) -> c_ulonglong,
    unmap_page: unsafe extern "C" fn(*mut Dev, c_ulonglong, c_ulong),
}

/// Referenced from kernel/lkl/nk-host.c as `lkl_host_ops.pci_ops`.
#[no_mangle]
pub static mut nk_pci_ops: LklDevPciOps = LklDevPciOps {
    add,
    remove,
    irq_init,
    msi_init,
    msi_teardown,
    read,
    write,
    resource_alloc,
    map_page,
    unmap_page,
};

/// Parse `SSSS:BB:DD.F` or `BB:DD.F` (hex), as `lkl_pci=` carries it.
fn parse_bdf(s: &[u8]) -> Option<Bdf> {
    let s = core::str::from_utf8(s).ok()?;
    let (seg, rest) = match s.matches(':').count() {
        2 => {
            let (a, b) = s.split_once(':')?;
            (u16::from_str_radix(a, 16).ok()?, b)
        }
        1 => (0, s),
        _ => return None,
    };
    let (bus, rest) = rest.split_once(':')?;
    let (dev, func) = rest.split_once('.')?;
    let bdf = Bdf {
        segment: seg,
        bus: u8::from_str_radix(bus, 16).ok()?,
        device: u8::from_str_radix(dev, 16).ok()?,
        function: u8::from_str_radix(func, 16).ok()?,
    };
    (bdf.device < 32 && bdf.function < 8).then_some(bdf)
}

unsafe extern "C" fn add(name: *const c_char, _ram: *mut c_void, _size: c_ulong) -> *mut Dev {
    if name.is_null() {
        return core::ptr::null_mut();
    }
    let bytes = core::ffi::CStr::from_ptr(name).to_bytes();
    let Some(bdf) = parse_bdf(bytes) else {
        crate::println!("  lkl-pci: cannot parse lkl_pci={:?}", core::str::from_utf8(bytes));
        return core::ptr::null_mut();
    };
    match Hardware.read32(bdf, 0) {
        Some(id) if id as u16 != 0xffff => {
            crate::println!(
                "  lkl-pci: {:04x}:{:02x}:{:02x}.{} ({:04x}:{:04x}) handed to Linux",
                bdf.segment, bdf.bus, bdf.device, bdf.function, id as u16, id >> 16
            );
            alloc::boxed::Box::into_raw(alloc::boxed::Box::new(Dev {
                bdf,
                vectors: [0; MAX_VECTORS],
                nvec: 0,
                bar_va: [(0, 0); 6],
            }))
        }
        _ => {
            crate::println!("  lkl-pci: no device at {:?}", bdf);
            core::ptr::null_mut()
        }
    }
}

unsafe extern "C" fn remove(dev: *mut Dev) {
    if !dev.is_null() {
        msi_teardown(dev, 0);
    }
}

/// INTx needs ACPI `_PRT`, which needs AML. Accept and route nothing; the
/// driver's MSI/MSI-X request is what delivers interrupts.
unsafe extern "C" fn irq_init(dev: *mut Dev, irq: c_int) -> c_int {
    if let Some(d) = dev.as_ref() {
        crate::println!(
            "  lkl-pci: {:02x}:{:02x}.{} INTx (Linux irq {}) not routed; MSI/MSI-X only",
            d.bdf.bus, d.bdf.device, d.bdf.function, irq
        );
    }
    0
}

unsafe extern "C" fn read(dev: *mut Dev, where_: c_int, size: c_int, val: *mut c_void) -> c_int {
    let Some(d) = dev.as_ref() else { return -1 };
    if where_ < 0 || !matches!(size, 1 | 2 | 4) || where_ + size > 4096 || where_ % size != 0 {
        return -1;
    }
    let Some(word) = Hardware.read32(d.bdf, (where_ & !3) as u16) else {
        return -1;
    };
    let shift = (where_ & 3) * 8;
    let v = match size {
        1 => (word >> shift) & 0xff,
        2 => (word >> shift) & 0xffff,
        _ => word,
    };
    // LKL's caller passes a u32 for every width.
    *(val as *mut u32) = v;
    size
}

unsafe extern "C" fn write(dev: *mut Dev, where_: c_int, size: c_int, val: *mut c_void) -> c_int {
    let Some(d) = dev.as_ref() else { return -1 };
    if where_ < 0 || !matches!(size, 1 | 2 | 4) || where_ + size > 4096 || where_ % size != 0 {
        return -1;
    }
    let v = *(val as *const u32);
    let off = where_ as u16;
    // Native width every time: a read-modify-write of the surrounding dword
    // would write back RW1C status bits Linux never asked to clear.
    let ok = match size {
        1 => Hardware.write8(d.bdf, off, v as u8),
        2 => Hardware.write16(d.bdf, off, v as u16),
        _ => Hardware.write32(d.bdf, off, v),
    };
    if ok { size } else { -1 }
}

/// A memory BAR's physical address, from config space.
fn bar_address(bdf: Bdf, index: usize) -> Option<u64> {
    if index >= 6 {
        return None;
    }
    let lo = Hardware.read32(bdf, 0x10 + index as u16 * 4)?;
    if lo & 1 != 0 {
        return None; // I/O BAR: LKL panics on port I/O anyway
    }
    let mut addr = (lo & !0xf) as u64;
    if (lo >> 1) & 3 == 2 {
        let hi = Hardware.read32(bdf, 0x10 + index as u16 * 4 + 4)?;
        addr |= (hi as u64) << 32;
    }
    (addr != 0).then_some(addr)
}

/// An uncached kernel address for device memory `[start, start+len)`: the
/// identity map below 4 GiB, the MMIO window above it.
fn map_uncached(start: u64, len: u64) -> Option<u64> {
    let end = start.checked_add(len)?;
    if end <= 1 << 32 {
        unsafe { crate::paging::identity_mmio(start, end) }.then_some(start)
    } else {
        unsafe { crate::paging::map_mmio_high(start, len) }
    }
}

unsafe extern "C" fn resource_alloc(dev: *mut Dev, size: c_ulong, index: c_int) -> *mut c_void {
    let Some(d) = dev.as_mut() else { return core::ptr::null_mut() };
    let Some(phys) = bar_address(d.bdf, index as usize) else {
        return core::ptr::null_mut();
    };
    let Some(va) = map_uncached(phys, size as u64) else {
        crate::println!("  lkl-pci: BAR{} {:#x}+{:#x} could not be mapped; disabled", index, phys, size);
        return core::ptr::null_mut();
    };
    d.bar_va[index as usize] = (va, size as u64);
    crate::println!("  lkl-pci: BAR{} {:#x}+{:#x} mapped uncached at {:#x}", index, phys, size, va);
    va as *mut c_void
}

/// Identity: Linux's linear map is physical memory, and no IOMMU is set up.
unsafe extern "C" fn map_page(_dev: *mut Dev, vaddr: *mut c_void, _size: c_ulong) -> c_ulonglong {
    vaddr as c_ulonglong
}

unsafe extern "C" fn unmap_page(_dev: *mut Dev, _dma: c_ulonglong, _size: c_ulong) {}

unsafe extern "C" fn msi_init(dev: *mut Dev, ty: c_int, nvec: c_int, irqs: *mut c_int) -> c_int {
    let Some(d) = dev.as_mut() else { return -22 };
    if nvec <= 0 || nvec as usize > MAX_VECTORS || irqs.is_null() {
        return -22;
    }
    let irqs = core::slice::from_raw_parts(irqs, nvec as usize);
    // LKL runs pinned to the boot CPU; deliver there.
    let apic = crate::lapic::id();
    match ty {
        LKL_PCI_IRQ_MSI => {
            // One message. Multiple-message MSI needs nvec contiguous,
            // nvec-aligned vectors; drivers that ask fall back to MSI-X or one.
            if nvec != 1 || irqs[0] <= 0 {
                return -28; // -ENOSPC
            }
            let Ok(Some(m)) = crate::pci_irq::msi(&Hardware, d.bdf) else { return -19 };
            let Some(vector) = crate::device_irq_x86::allocate(Some(irqs[0]), None) else {
                return -28;
            };
            let Ok(msg) = crate::irq_x86::message(apic, vector) else { return -22 };
            let mut ok = Hardware.write32(d.bdf, m.offset + 4, msg.address as u32);
            if m.address64 {
                ok &= Hardware.write32(d.bdf, m.offset + 8, (msg.address >> 32) as u32);
            }
            ok &= Hardware.write16(d.bdf, m.data(), msg.data as u16);
            if !ok {
                crate::device_irq_x86::retire(vector);
                return -5;
            }
            d.vectors[0] = vector;
            d.nvec = 1;
            crate::println!("  lkl-pci: MSI -> vector {:#x} -> Linux irq {}", vector, irqs[0]);
            0
        }
        LKL_PCI_IRQ_MSIX => {
            let Ok(Some(m)) = crate::pci_irq::msix(&Hardware, d.bdf) else { return -19 };
            if nvec as u16 > m.count {
                return -22;
            }
            // Through the BAR's mapping from resource_alloc, which Linux has
            // already made by the time a driver asks for vectors.
            let (bar_va, bar_len) = d.bar_va[m.table_bar as usize];
            if bar_va == 0 || m.table_offset as u64 + m.count as u64 * 16 > bar_len {
                return -19;
            }
            let table = bar_va + m.table_offset as u64;
            for (i, &irq) in irqs.iter().enumerate() {
                if irq <= 0 {
                    continue; // index not requested
                }
                let Some(vector) = crate::device_irq_x86::allocate(Some(irq), None) else {
                    msi_teardown(dev, ty);
                    return -28;
                };
                let Ok(msg) = crate::irq_x86::message(apic, vector) else {
                    msi_teardown(dev, ty);
                    return -22;
                };
                // Address and data only. Vector control (the mask) is
                // Linux's: pci_msi_unmask_irq clears it on request_irq.
                let entry = (table + i as u64 * 16) as *mut u32;
                core::ptr::write_volatile(entry, msg.address as u32);
                core::ptr::write_volatile(entry.add(1), (msg.address >> 32) as u32);
                core::ptr::write_volatile(entry.add(2), msg.data);
                d.vectors[i] = vector;
                d.nvec = d.nvec.max(i + 1);
            }
            crate::println!(
                "  lkl-pci: MSI-X {} entries -> vectors from {:#x}",
                irqs.iter().filter(|&&q| q > 0).count(),
                d.vectors.iter().copied().find(|&v| v != 0).unwrap_or(0)
            );
            0
        }
        _ => -22,
    }
}

unsafe extern "C" fn msi_teardown(dev: *mut Dev, _ty: c_int) {
    let Some(d) = dev.as_mut() else { return };
    for v in d.vectors.iter_mut().take(d.nvec) {
        if *v != 0 {
            crate::device_irq_x86::retire(*v);
            *v = 0;
        }
    }
    d.nvec = 0;
}

/// The device to hand Linux, chosen during discovery: the first NVMe
/// controller, since that is what the roadmap attaches first.
static mut CHOSEN: Option<Bdf> = None;

pub fn choose(devices: &[crate::pci::Device]) {
    let pick = devices.iter().find(|d| d.class == 0x01 && d.subclass == 0x08);
    unsafe { CHOSEN = pick.map(|d| d.bdf) };
}

/// `lkl_pci=SSSS:BB:DD.F` for the chosen device, or nothing.
pub fn cmdline_arg(buf: &mut [u8]) -> usize {
    let Some(b) = (unsafe { CHOSEN }) else { return 0 };
    use core::fmt::Write;
    struct W<'a>(&'a mut [u8], usize);
    impl Write for W<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let end = self.1 + s.len();
            if end > self.0.len() {
                return Err(core::fmt::Error);
            }
            self.0[self.1..end].copy_from_slice(s.as_bytes());
            self.1 = end;
            Ok(())
        }
    }
    let mut w = W(buf, 0);
    if write!(
        w,
        " lkl_pci={:04x}:{:02x}:{:02x}.{}",
        b.segment, b.bus, b.device, b.function
    )
    .is_err()
    {
        return 0;
    }
    w.1
}

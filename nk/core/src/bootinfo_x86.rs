//! x86 boot-info parsers: Multiboot2 (GRUB / real hardware) and PVH (QEMU -kernel).
//!
//! On aarch64, QEMU hands over a flattened device tree and `dt.rs` parses it.
//! On x86, the boot protocol depends on how the kernel was loaded:
//!
//!   * **PVH** (`magic == 0x50564800`): QEMU's `-kernel` hands over a pointer
//!     to `struct hvm_start_info`. The memory map is an array of
//!     `struct hvm_memmap_entry` (base, size, type).
//!   * **Multiboot2** (`magic == 0x36d76289`): GRUB hands over a tag list.
//!     The memory map is inside the tag with type 6.
//!
//! Both produce the same answer: `(ram_base, ram_size, initrd)`.

/// How many RAM regions nk will record. A PC splits memory around the PCI
/// hole below 4GB, so two is the common case and four is generous.
pub const MAX_RAM_REGIONS: usize = 8;

pub struct BootInfo {
    /// The lowest RAM region: where the kernel image and its first
    /// allocations live.
    pub ram_base: u64,
    pub ram_size: u64,
    /// Every usable RAM region the loader reported, `(base, end)`.
    ///
    /// Kept separately because a PC's memory is *not* one span: QEMU with
    /// `-m 4G` reports 1MB..2GB and 4GB..6GB, with the PCI hole between
    /// them. Collapsing that to `base..max_end` hands the frame allocator
    /// two gigabytes of device aperture as though it were memory, and the
    /// failure lands wherever that frame is later used rather than here.
    pub ram_regions: [(u64, u64); MAX_RAM_REGIONS],
    pub ram_region_count: usize,
    pub initrd: Option<(u64, u64)>,
    pub cmdline: &'static str,
    /// Physical RSDP address (PVH), or the loader-owned RSDP copy (MB2).
    pub rsdp: Option<u64>,
    pub info_range: Option<(u64, u64)>,
    /// GRUB's framebuffer (Multiboot2 tag 8): address, pitch, width, height,
    /// bits per pixel. Direct RGB only; PVH boots have none.
    pub framebuffer: Option<(u64, u32, u32, u32, u8)>,
}

// ------------------------------------------------------------- PVH parser --
//
// struct hvm_start_info, defined by Xen and adopted by Linux and QEMU.
// The repr(C) layout below follows Xen's hvm_start_info v1. In particular,
// RSDP is at 0x20, and the memory map pointer is at 0x28.
#[repr(C)]
struct HvmStartInfo {
    magic: [u8; 4],
    version: u32,
    flags: u32,
    nr_modules: u32,
    modlist_paddr: u64,
    cmdline_paddr: u64,
    rsdp_paddr: u64,
    memmap_paddr: u64,
    memmap_entries: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct HvmMemmapEntry {
    addr: u64,
    size: u64,
    type_: u32,
    reserved: u32,
}

const HVM_MEMMAP_TYPE_RAM: u32 = 1;

pub unsafe fn parse_pvh(info_ptr: u64) -> Option<BootInfo> {
    if info_ptr == 0 {
        return None;
    }
    let info = &*(info_ptr as *const HvmStartInfo);
    // Xen's magic is 0x336ec578 (literally [0x78, 0xc5, 0x6e, 0x33]):
    // byte 1 is 0xc5, not ASCII 'E'.
    if info.magic != [0x78, 0xc5, 0x6e, 0x33] {
        return None;
    }

    let mut ram_base = 0u64;
    let mut ram_size = 0u64;
    let mut ram_regions = [(0u64, 0u64); MAX_RAM_REGIONS];
    let mut ram_region_count = 0usize;

    if info.memmap_paddr != 0 && info.memmap_entries > 0 {
        let entries = core::slice::from_raw_parts(
            info.memmap_paddr as *const HvmMemmapEntry,
            info.memmap_entries as usize,
        );
        // Record each usable region as its own range, in address order.
        //
        // Deliberately *not* collapsed into one `base..max_end` span. A PC
        // splits RAM around the PCI hole -- QEMU with `-m 4G` reports
        // 1MB..2GB and 4GB..6GB -- and a single span across that gives the
        // frame allocator two gigabytes of device aperture to hand out. The
        // frames look fine until one is used as a page table, at which point
        // the walk reads whatever a device decodes there.
        for e in entries {
            if e.type_ != HVM_MEMMAP_TYPE_RAM || e.addr < 0x100000 || e.size == 0 {
                continue;
            }
            if ram_region_count == MAX_RAM_REGIONS {
                break;
            }
            // Insertion sort: the loader is not required to report them in
            // order, and the lowest region is the one the kernel image and
            // the first allocations sit in.
            let entry = (e.addr, e.addr + e.size);
            let mut i = ram_region_count;
            while i > 0 && ram_regions[i - 1].0 > entry.0 {
                ram_regions[i] = ram_regions[i - 1];
                i -= 1;
            }
            ram_regions[i] = entry;
            ram_region_count += 1;
        }
        if ram_region_count > 0 {
            ram_base = ram_regions[0].0;
            ram_size = ram_regions[0].1 - ram_regions[0].0;
        }
    }

    // A module passed with -initrd
    let mut initrd = None;
    if info.nr_modules > 0 && info.modlist_paddr != 0 {
        // struct hvm_modlist_entry: paddr: u64, size: u64, cmdline_paddr: u64, reserved: u64
        #[repr(C)]
        struct HvmModlistEntry {
            paddr: u64,
            size: u64,
            cmdline_paddr: u64,
            reserved: u64,
        }
        let mods = core::slice::from_raw_parts(
            info.modlist_paddr as *const HvmModlistEntry,
            info.nr_modules as usize,
        );
        if let Some(m) = mods.first() {
            if m.size > 0 {
                initrd = Some((m.paddr, m.paddr + m.size));
            }
        }
    }

    Some(BootInfo {
        ram_base,
        ram_size,
        ram_regions,
        ram_region_count,
        initrd,
        cmdline: "",
        rsdp: (info.rsdp_paddr != 0).then_some(info.rsdp_paddr),
        info_range: None,
        framebuffer: None,
    })
}

/// Parse GRUB's bounded, eight-byte-aligned Multiboot2 tag stream.
///
/// # Safety
/// `info_ptr` must name the loader's readable info block in the boot identity map.
pub unsafe fn parse_multiboot2(info_ptr: u64) -> Option<BootInfo> {
    if info_ptr == 0 || info_ptr & 7 != 0 || info_ptr.checked_add(8)? > 1<<32 { return None; }
    let total = core::ptr::read_unaligned(info_ptr as *const u32) as usize;
    if !(16..=1<<20).contains(&total) || info_ptr.checked_add(total as u64)? > 1<<32 { return None; }
    parse_multiboot_bytes(core::slice::from_raw_parts(info_ptr as *const u8, total), info_ptr)
}

pub fn parse_multiboot_bytes(bytes: &[u8], physical_base: u64) -> Option<BootInfo> {
    let get32 = |b: &[u8], o: usize| -> Option<u32> { Some(u32::from_le_bytes(b.get(o..o+4)?.try_into().ok()?)) };
    let total = get32(bytes, 0)? as usize;
    if total != bytes.len() || total < 16 || total & 7 != 0 || get32(bytes, 4)? != 0 { return None; }
    let mut boot = BootInfo { ram_base: 0, ram_size: 0, ram_regions: [(0,0);MAX_RAM_REGIONS],
        ram_region_count: 0, initrd: None, cmdline: "", rsdp: None,
        info_range: Some((physical_base,physical_base.checked_add(total as u64)?)),
        framebuffer: None };
    let mut new_rsdp = false;
    let mut at = 8;
    while at < total {
        let kind = get32(bytes, at)?;
        let size = get32(bytes, at+4)? as usize;
        if size < 8 { return None; }
        let tag = bytes.get(at..at.checked_add(size)?)?;
        match kind {
            0 => {
                if size != 8 || at+8 != total || boot.ram_region_count == 0 { return None; }
                if boot.ram_regions[..boot.ram_region_count].windows(2).any(|r| r[0].1 > r[1].0) { return None; }
                boot.ram_base = boot.ram_regions[0].0;
                boot.ram_size = boot.ram_regions[0].1-boot.ram_base;
                return Some(boot);
            }
            3 => {
                if size < 16 { return None; }
                let lo = get32(tag, 8)? as u64;
                let hi = get32(tag, 12)? as u64;
                if hi <= lo { return None; }
                if boot.initrd.is_none() { boot.initrd = Some((lo,hi)); }
            }
            6 => {
                if size < 16 { return None; }
                let stride = get32(tag, 8)? as usize;
                if stride < 24 || get32(tag, 12)? != 0 || (size-16) % stride != 0 { return None; }
                for entry in tag[16..].chunks_exact(stride) {
                    if get32(entry,16)? != 1 { continue; }
                    let lo = u64::from_le_bytes(entry[..8].try_into().ok()?);
                    let len = u64::from_le_bytes(entry[8..16].try_into().ok()?);
                    let hi = lo.checked_add(len)?;
                    let lo = lo.max(0x100000);
                    if lo >= hi { continue; }
                    if boot.ram_region_count == MAX_RAM_REGIONS { return None; }
                    let mut i = boot.ram_region_count;
                    while i > 0 && boot.ram_regions[i-1].0 > lo {
                        boot.ram_regions[i] = boot.ram_regions[i-1]; i-=1;
                    }
                    boot.ram_regions[i] = (lo,hi);
                    boot.ram_region_count+=1;
                }
            }
            8 => {
                // framebuffer_addr u64 @8, pitch @16, width @20, height @24,
                // bpp u8 @28, type u8 @29 (1 = direct RGB).
                if size < 31 { return None; }
                let addr = u64::from_le_bytes(tag.get(8..16)?.try_into().ok()?);
                let (pitch, width, height) = (get32(tag, 16)?, get32(tag, 20)?, get32(tag, 24)?);
                let (bpp, kind) = (*tag.get(28)?, *tag.get(29)?);
                if kind == 1 && addr != 0 {
                    boot.framebuffer = Some((addr, pitch, width, height, bpp));
                }
            }
            14 | 15 => {
                if size < 8 + if kind == 15 {36} else {20} { return None; }
                if kind == 15 || !new_rsdp { boot.rsdp = Some(physical_base.checked_add(at as u64+8)?); }
                if kind == 15 { new_rsdp = true; }
            }
            _ => {}
        }
        at = at.checked_add(size)?.checked_add(7)? & !7;
    }
    None // A complete info block must end with its end tag.
}

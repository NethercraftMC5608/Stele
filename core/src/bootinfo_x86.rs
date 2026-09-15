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
}

// ------------------------------------------------------------- PVH parser --
//
// struct hvm_start_info, defined by Xen and adopted by Linux and QEMU.
// Offsets on 64-bit:
//   0x00: magic (4 bytes: "xEn3")
//   0x04: version
//   0x08: flags
//   0x10: memmap_paddr (u64)
//   0x18: memmap_entries (u32)
//   0x20: cmdline_paddr (u64)
//   0x28: rsdp_paddr (u64)
//   0x30: modlist_paddr (u64)
//   0x38: nr_modules (u32)
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
    })
}

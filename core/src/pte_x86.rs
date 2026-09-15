//! x86-64 page table entry bit definitions.
//!
//! This provides the same ten names the shared `paging.rs` uses, but with the
//! x86-64 four-level paging bit layout instead of the AArch64 one. The two
//! architectures disagree in three ways that matter here:
//!
//!  * AArch64 names "this is a next-level table" with a dedicated TABLE bit
//!    that is *also* set on a level-3 4KB page; x86-64 has no such bit. The
//!    table-versus-leaf question on x86-64 is answered by the PS bit *and* by
//!    which level the entry sits at, not by a single fixed bit. So `TABLE`
//!    below is a software marker that contributes nothing to the entry --
//!    creating a table pointer is just "present, no PS", and a level-3 leaf is
//!    the same. That keeps the shared code compiling and, on the levels that
//!    matter, produces a correct x86-64 table pointer / 4KB leaf.
//!
//!  * AArch64 picks the memory type through an index into MAIR_EL1
//!    (`attr`); x86-64 has no equivalent register -- the type is set by the
//!    page-attribute table (PAT), and ordinary RAM is write-back at PAT index
//!    zero. So `attr` contributes nothing on x86-64.
//!
//!  * The 2MB block is created without a block bit on AArch64 (the level
//!    implies it) but needs PS set on x86-64. `paging.rs` does not set PS for
//!    its level-2 block entries, so on x86-64 a mapping made through
//!    `map_normal` would map only its first 4KB. `map_normal` and the block
//!    half of `init` are not on x86-64's boot path yet -- the kernel runs under
//!    the boot table's 2MB identity map -- so this gap is documented, not yet
//!    hit. It must be closed before x86-64 maps new memory in 2MB blocks.
//!
//! The bits that *are* fixed on x86-64 (4KB granule):
//! - bit 0:  Present
//! - bit 1:  Write
//! - bit 2:  User/Supervisor
//! - bit 3:  Page Write-Through
//! - bit 4:  Page Cache Disable
//! - bit 5:  Accessed
//! - bit 6:  Dirty
//! - bit 7:  Page Size (0 = 4KB, 1 = huge)
//! - bit 8:  Global
//! - bits 9-11: available to software
//! - bits 12-51: physical address (12-47 for 4KB, 21-51 for 2MB)
//! - bit 63:  No-Execute

/// Present. An entry is a table or a page when this is set; an absent slot is
/// zero.
pub const VALID: u64 = 1 << 0;

/// See the module note: x86-64 has no "table" bit. A table pointer and a
/// level-3 4KB leaf are both "present, no PS"; a huge page additionally sets
/// PS. This marker contributes nothing to the entry so that the shared
/// `VALID | TABLE` construction yields a present entry on x86-64.
pub const TABLE: u64 = 0;

/// Present, plus the fixed bits the kernel sets on a mapping.
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const PWT: u64 = 1 << 3;
pub const PCD: u64 = 1 << 4;
pub const ACCESSED: u64 = 1 << 5;
pub const DIRTY: u64 = 1 << 6;
/// PS: 0 = 4KB, 1 = huge (2MB at level 2, 1GB at level 1).
pub const HUGE: u64 = 1 << 7;
pub const GLOBAL: u64 = 1 << 8;
/// No-execute. Applies to both privilege levels on x86-64, so UXN and PXN are
/// the same bit here where they are distinct AArch64 bits.
pub const NX: u64 = 1 << 63;

/// Accessed (bit 5). The AArch64 AF lives at bit 10; on x86-64 the hardware
/// sets it on a miss and we start it set so the entry is used immediately,
/// exactly as AArch64 starts AF set.
pub const AF: u64 = ACCESSED;

/// x86-64 has no shareability field in the entry; coherency is implicit in the
/// caching type (PAT). Contributes nothing.
pub const SH_INNER: u64 = 0;

/// No-execute at level 0. Bit 63 on x86-64.
pub const UXN: u64 = NX;
/// No-execute at level 1. Also bit 63 on x86-64 (NX is not privilege-scoped).
pub const PXN: u64 = NX;

/// "Not global" on AArch64 sets a bit; on x86-64 it is the *absence* of the
/// Global bit, so it contributes nothing here.
pub const NG: u64 = 0;

/// Write and readable by both privilege levels: Write + User.
pub const AP_RW_ANY: u64 = WRITABLE | USER;
/// Readable by both privilege levels, not writable: User only.
pub const AP_RO_ANY: u64 = USER;

/// x86-64 has no AttrIndx; attributes are in PAT, and ordinary write-back RAM
/// is PAT index 0. So this contributes nothing regardless of the index.
#[allow(unused)]
pub const fn attr(_idx: u64) -> u64 {
    0
}

/// A level-1/2 table pointer, or a level-3 4KB leaf: present, no PS.
#[inline]
pub fn table_entry(pa: u64, flags: u64) -> u64 {
     (pa & 0x000f_ffff_ffff_f000) | flags
}

/// The output (physical) address in a table pointer or a 4KB leaf.
///
/// On x86-64 a 4K entry's physical address occupies bits [47:12], so the mask
/// clears the lower 12 bits -- unlike AArch64's 52-bit address, whose mask
/// keeps 52 bits.
pub const ADDR: u64 = 0x000f_ffff_ffff_f000;

/// A descriptor is a next-level table when it is present *and* not a huge leaf.
///
/// On x86-64 there is no dedicated "table" bit: a next-level pointer is a
/// present entry with PS clear, and a level-2 huge (2MB) leaf is a present
/// entry with PS set. The shared walk only ever calls this on L0/L1/L2, where
/// PS is reserved (always 0) at L0/L1 -- so the present check alone is enough
/// there -- and at L2, where PS is what separates a 1GB/2MB block from a real
/// level-3 table. Treating a huge leaf as a table would make the walker read
/// two megabytes of RAM as page-table entries.
#[inline]
pub const fn is_table(entry: u64) -> bool {
    entry & VALID != 0 && entry & HUGE == 0
}

/// A page is writable when the W bit is set. The polarity is the opposite of
/// AArch64's, where writability is the *absence* of AP[2]; this is why the
/// walk routes through `pte::writable` rather than testing a fixed bit.
#[inline]
pub const fn writable(entry: u64) -> bool {
    entry & WRITABLE != 0
}

/// Ring 3 can reach a page when the User/Supervisor bit is set.
///
/// AArch64 asks this of AP[1] at bit 6; on x86-64 bit 6 is Dirty, which the
/// hardware sets on the first write and which says nothing about privilege.
/// Testing a fixed bit 6 here would have read "has been written" as "user can
/// reach it" -- a mapping the kernel had merely touched would have looked
/// user-accessible.
#[inline]
pub const fn user_accessible(entry: u64) -> bool {
    entry & USER != 0
}

/// Remove ring-3 reach while keeping the page mapped and its frame alive.
#[inline]
pub const fn clear_user_access(entry: u64) -> u64 {
    entry & !USER
}

/// A 2MB leaf at level 2: present with PS set.
#[inline]
pub fn huge_entry(pa: u64, flags: u64) -> u64 {
    (pa & 0x000f_ffff_fff8_0000) | HUGE | flags
}

/// A pointer to a next-level table: present, writable, user-reachable, PS
/// clear.
///
/// x86-64 permissions are the *conjunction* of every level's bits, unlike
/// AArch64 where the leaf descriptor alone decides. A table pointer without
/// USER makes every page beneath it kernel-only however the leaf is marked,
/// and one without WRITABLE makes them all read-only. So intermediate levels
/// are opened here and the leaf is left to say what is actually permitted --
/// the arrangement Linux and every other x86-64 kernel uses.
#[inline]
pub const fn table_pointer(pa: u64) -> u64 {
    pa | VALID | WRITABLE | USER
}

/// A block (large-page) leaf at level 1 (1GB) or 2 (2MB).
///
/// The PS bit is what makes it a leaf; without it the CPU reads the address
/// as a pointer to the next level. AArch64 marks a block by clearing a bit,
/// x86-64 by setting one, so this cannot be shared.
#[inline]
pub const fn block_leaf(pa: u64, flags: u64) -> u64 {
    pa | VALID | HUGE | flags
}

/// Whether an entry at a non-leaf level is a block rather than a table.
#[inline]
pub const fn is_block(entry: u64) -> bool {
    entry & VALID != 0 && entry & HUGE != 0
}

/// A level-3 4KB leaf: present, PS clear. Identical in shape to a table
/// pointer, because at level 3 there is no next level to point at.
#[inline]
pub const fn page_leaf(pa: u64, flags: u64) -> u64 {
    pa | VALID | flags
}

/// The permission bits for memory only the kernel may touch, read and write.
///
/// The mirror image of AArch64: there, kernel-only-and-writable is the
/// absence of both AP bits, so the constant is zero. Here, absent bits mean
/// supervisor-only (good) but *read-only* (wrong), so WRITABLE must be
/// present and USER must not. Spelling this as a named constant is what stops
/// "no permission bits" -- correct on one architecture -- being copied to the
/// other, where it silently produces read-only kernel memory and a fault on
/// the first write.
pub const KERNEL_RW: u64 = WRITABLE;

/// Extract the physical address from a 4KB entry.
#[inline]
pub fn phys_addr(e: u64) -> u64 {
    e & 0x000f_ffff_ffff_f000
}

/// Extract the physical address from a 2MB entry at level 2.
#[inline]
pub fn huge_phys_addr(e: u64) -> u64 {
    e & 0x000f_ffff_fff8_0000
}

/// x86-64 has no MAIR register. Provided for the shared code paths that name
/// them; they contribute nothing.
pub const MAIR_DEVICE: u64 = 0;
pub const MAIR_NORMAL: u64 = 0;
pub const MAIR: u64 = 0;

/// The bit whose presence or absence makes a descriptor a block: PS.
///
/// Must be cleared when a 2MB block is split, or each 4KB child would still
/// claim to be a huge page and the walker would stop a level early.
#[inline]
pub const fn block_marker() -> u64 {
    HUGE
}

/// The permission bits a table pointer carries, so a split can preserve them.
///
/// x86-64 permissions are the *conjunction* of every level, so an
/// intermediate entry has to be at least as permissive as the leaves beneath
/// it. When a block is split the new parent must therefore keep the block's
/// own W/U/NX rather than adopt a fixed set -- handing it `table_pointer`'s
/// USER|WRITABLE would publish a slice of kernel memory to ring 3.
#[inline]
pub const fn table_perm_mask() -> u64 {
    WRITABLE | USER | NX
}

/// What an intermediate entry must carry for a user leaf below it to be
/// reachable.
///
/// x86-64 permissions are the conjunction of every level: USER at the leaf
/// means nothing unless each table above it says USER too. WRITABLE is here
/// for the same reason -- a read-only table pointer makes every page beneath
/// it read-only however the leaf is marked. The leaf remains the place where
/// the real permission is decided.
#[inline]
pub const fn user_table_bits() -> u64 {
    WRITABLE | USER
}

//! ARM64 page table entry bit definitions.
//!
//! This mirrors the original inline `mod pte` in paging.rs. The bits are an
//! AArch64 Stage-1 descriptor layout; only the bits nk actually sets are named.

/// The bit that names a descriptor valid at all.
pub const VALID: u64 = 1 << 0;
/// At L0-L2: 1 = next-level table, 0 = block.
pub const TABLE: u64 = 1 << 1;
/// Access flag; a miss on this faults.
pub const AF: u64 = 1 << 10;
/// Inner shareable.
pub const SH_INNER: u64 = 3 << 8;
/// Never executable at EL0.
pub const UXN: u64 = 1 << 54;
/// Never executable at EL1.
pub const PXN: u64 = 1 << 53;

/// Not global: this translation belongs to one ASID only.
///
/// Every mapping nk made was global, which is the default and is wrong
/// for user space: a global entry is valid in every address space, so two
/// processes with pages at the same address share whichever the TLB saw
/// first. It was invisible because nk flushed the whole TLB on every
/// switch -- which is both slower and less correct than saying what is
/// actually private.
pub const NG: u64 = 1 << 11;

/// AP[2:1] at bits 7:6. EL0 can only reach a page that says so; there is
/// no separate user page table, only this bit.
pub const AP_RW_ANY: u64 = 1 << 6; // read/write at EL1 and EL0
pub const AP_RO_ANY: u64 = 3 << 6; // read-only at EL1 and EL0

/// AttrIndx, an index into MAIR_EL1 rather than a description of the
/// memory -- the attributes themselves live in that register.
pub const fn attr(idx: u64) -> u64 {
    idx << 2
}

/// A descriptor is a table when both low bits are set. A *block* has only
/// bit 0, and telling them apart matters wherever tables are walked or freed:
/// a block's output address is memory, and following it as a table reads a
/// gigabyte of RAM as page-table entries.
///
/// On AArch64 the TABLE bit (1) is the marker at every non-leaf level, so
/// the test is level-independent. A level-3 4KB page also sets both bits,
/// but the shared walk only ever calls this on L0/L1/L2, where a "table"
/// really is the next level and a "block" is a leaf that must not be
/// descended.
#[inline]
pub const fn is_table(entry: u64) -> bool {
    entry & 3 == 3
}

/// A page is writable when AP[2] (bit 7) is clear. EL0 reach is AP[1]
/// (bit 6); writability is the top bit of the AP field.
#[inline]
pub const fn writable(entry: u64) -> bool {
    entry & (1 << 7) == 0
}

/// EL0 can reach a page when AP[1] (bit 6) is set. This is a separate
/// question from writability, which is AP[2] (bit 7): a page can be
/// unreachable from EL0 and still writable at EL1.
#[inline]
pub const fn user_accessible(entry: u64) -> bool {
    entry & (1 << 6) != 0
}

/// Remove EL0 reach while keeping the page mapped and its frame alive.
///
/// Clearing AP[1] out of AP_RO_ANY (0b11) leaves 0b10: read-only at EL1, no
/// access at EL0, which is what PROT_NONE has to mean for a page the kernel
/// still owns.
#[inline]
pub const fn clear_user_access(entry: u64) -> u64 {
    entry & !(1 << 6)
}

/// The output (physical) address in any descriptor or TTBR value.
pub const ADDR: u64 = 0x0000_ffff_ffff_f000;

/// Extract the physical address from a leaf.
#[inline]
pub const fn phys_addr(e: u64) -> u64 {
    e & ADDR
}

/// A pointer to a next-level table, as stored in a level-0/1/2 entry.
///
/// On AArch64 a table descriptor is the address plus VALID and TABLE, at
/// every level.
#[inline]
pub const fn table_pointer(pa: u64) -> u64 {
    pa | VALID | TABLE
}

/// A block (large-page) leaf at level 1 or 2.
///
/// AArch64 says "block" by *clearing* the TABLE bit, so a block descriptor is
/// the address plus VALID alone -- the level decides whether that is 1GB or
/// 2MB. x86-64 instead needs a bit set, which is why this is a function per
/// architecture rather than a shared `| VALID`.
#[inline]
pub const fn block_leaf(pa: u64, flags: u64) -> u64 {
    pa | VALID | flags
}

/// Whether an entry at a non-leaf level is a block rather than a table.
#[inline]
pub const fn is_block(entry: u64) -> bool {
    entry & VALID != 0 && entry & TABLE == 0
}

/// A level-3 4KB leaf. Both low bits, same as a table descriptor -- at level 3
/// the TABLE bit means "page" instead, which is the one place AArch64's
/// encoding is not self-describing.
#[inline]
pub const fn page_leaf(pa: u64, flags: u64) -> u64 {
    pa | VALID | TABLE | flags
}

/// The permission bits for memory only the kernel may touch, read and write.
///
/// On AArch64 that is the *absence* of AP bits: AP[1] clear is EL1-only and
/// AP[2] clear is writable.
pub const KERNEL_RW: u64 = 0;

/// The bit whose presence or absence makes a descriptor a block.
///
/// AArch64 says "block" by *clearing* TABLE, so removing the marker from a
/// block's flags is a no-op -- there is nothing set to take away. x86-64 says
/// it by *setting* PS, which must be cleared before the same flags can
/// describe a smaller leaf.
#[inline]
pub const fn block_marker() -> u64 {
    0
}

/// The permission bits a table pointer carries, so a split can preserve them.
///
/// AArch64 resolves permissions at the leaf, not by conjunction: a table
/// descriptor has no AP/XN field to speak of (the hierarchical attributes
/// exist but nk never sets them). So there is nothing to carry across, and
/// preserving "no bits" is exactly right.
#[inline]
pub const fn table_perm_mask() -> u64 {
    0
}

/// AArch64 decides permissions at the leaf, so an intermediate descriptor
/// needs nothing added for a user page beneath it to be reachable.
#[inline]
pub const fn user_table_bits() -> u64 {
    0
}

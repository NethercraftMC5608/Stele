//! Address space a process has asked for but not yet touched.
//!
//! nk populates a mapping when it is made: a frame per page, read or zeroed,
//! whether or not the program ever looks at it. That is the simplest thing
//! that works and it is what `MAP_NORESERVE` exists to say is wrong.
//!
//! WebKit is the first program here to care. Its allocator reserves address
//! space in very large blocks -- 128MB at a time -- and commits a fraction of
//! it. Populating those eagerly costs a frame per page of a region nothing
//! reads, and the arena runs out long before the machine does: the failure
//! is `mmapfail range len 0x8000000` with a gigabyte already spoken for.
//!
//! So a `MAP_NORESERVE` anonymous mapping is recorded here and mapped
//! nowhere. The first touch of each page faults, and `fault_in` puts a zeroed
//! frame under it. That is demand paging, in the one case that needs it,
//! rather than a page-cache rewrite.
//!
//! Deliberately not general. File-backed mappings are still eager (they have
//! contents to fetch and no flag asking otherwise), and a reservation that is
//! never touched costs one table entry here and nothing else.

use crate::frames;
use crate::paging;

const MAX: usize = 256;

#[derive(Clone, Copy)]
struct Reservation {
    /// The address space this belongs to, so threads sharing one see the
    /// same reservations and two processes at the same address do not.
    space: u64,
    start: u64,
    end: u64,
    writable: bool,
    exec: bool,
    /// PROT_NONE: backed, but unreachable from user space.
    none: bool,
    live: bool,
}

static mut RESERVED: [Reservation; MAX] = [Reservation {
    space: 0,
    start: 0,
    end: 0,
    writable: false,
    exec: false,
    none: false,
    live: false,
}; MAX];

fn space() -> u64 {
    paging::table_of(crate::arch::current_page_table())
}

/// Record a range as reserved but unmapped. False when the table is full,
/// which the caller must treat as a refusal rather than ignore: a
/// reservation nobody recorded is a segfault on first touch.
pub fn add(start: u64, len: u64, writable: bool, exec: bool, none: bool) -> bool {
    let here = space();
    unsafe {
        let r = &mut *(&raw mut RESERVED);
        if let Some(slot) = r.iter_mut().find(|s| !s.live) {
            *slot = Reservation {
                space: here,
                start,
                end: start + len,
                writable,
                exec,
                none,
                live: true,
            };
            return true;
        }
    }
    false
}

/// Forget any reservation overlapping this range. Pages already faulted in
/// are ordinary mappings and are freed by the caller's own unmap.
pub fn forget(start: u64, len: u64) {
    let here = space();
    let end = start + len;
    unsafe {
        for s in (*(&raw mut RESERVED)).iter_mut() {
            if s.live && s.space == here && s.start < end && start < s.end {
                s.live = false;
            }
        }
    }
}

/// Give a new address space the reservations of the one it was copied from.
///
/// `fork` copies pages, and a reservation has none: it is a promise that a
/// range will get one on first touch. Without carrying the promise across,
/// the child faults on the first byte of an arena its parent could use.
pub fn inherit(parent: u64, child: u64) {
    unsafe {
        let r = &mut *(&raw mut RESERVED);
        for i in 0..MAX {
            if !(r[i].live && r[i].space == parent) {
                continue;
            }
            let mut copy = r[i];
            copy.space = child;
            match r.iter_mut().find(|s| !s.live) {
                Some(slot) => *slot = copy,
                // Out of slots: the child simply has one fewer reservation
                // than its parent, and faults there as it would have before
                // MAP_NORESERVE was honoured at all.
                None => return,
            }
        }
    }
}

/// Change the permissions a reservation will fault pages in with.
///
/// `mprotect` on a range that has never been touched has no pages to change,
/// and a reservation that does not hear about it goes on handing out pages
/// with the permissions the mapping was made with. A loader that reserves
/// read-write and then makes a slice of it executable got exactly that: the
/// first instruction fetched from the new range faulted, on a page nk had
/// just mapped non-executable on its behalf.
///
/// Returns whether any reservation covered part of the range -- which is how
/// `mprotect` tells "nothing there" from "nothing there *yet*".
pub fn protect(start: u64, len: u64, writable: bool, exec: bool, none: bool) -> bool {
    let here = space();
    let end = start + len;
    let mut touched = false;
    unsafe {
        let r = &mut *(&raw mut RESERVED);
        for i in 0..MAX {
            if !(r[i].live && r[i].space == here && r[i].start < end && start < r[i].end) {
                continue;
            }
            touched = true;
            let (lo, hi) = (r[i].start, r[i].end);
            // The overlap takes the new permissions; whatever of the
            // reservation lies outside it keeps the old ones, as its own
            // record. Out of records, the whole reservation changes -- which
            // is too permissive on the edges and still better than a range
            // that faults.
            let head = if lo < start { Some((lo, start)) } else { None };
            let tail = if end < hi { Some((end, hi)) } else { None };
            let old = r[i];
            r[i].start = lo.max(start);
            r[i].end = hi.min(end);
            r[i].writable = writable;
            r[i].exec = exec;
            r[i].none = none;
            for piece in [head, tail].into_iter().flatten() {
                match r.iter_mut().find(|s| !s.live) {
                    Some(slot) => {
                        *slot = old;
                        slot.start = piece.0;
                        slot.end = piece.1;
                    }
                    None => {
                        r[i].start = lo;
                        r[i].end = hi;
                    }
                }
            }
        }
    }
    touched
}

/// Where an address sits relative to this space's reservations.
///
/// Returns `(inside, last_end, next_start, live)`: whether a reservation
/// covers it, the end of the nearest one below, the start of the nearest one
/// above, and how many are live. The fault reporter prints this because
/// "unmapped" and "reserved but not yet faulted in" produce the identical
/// translation fault, and the difference is the whole diagnosis.
pub fn describe(addr: u64) -> (bool, u64, u64, usize) {
    let here = space();
    let page = addr & !4095;
    let (mut inside, mut below, mut above, mut live) = (false, 0u64, u64::MAX, 0usize);
    unsafe {
        for s in (*(&raw const RESERVED)).iter() {
            if !(s.live && s.space == here) {
                continue;
            }
            live += 1;
            if page >= s.start && page < s.end {
                inside = true;
            }
            if s.end <= page && s.end > below {
                below = s.end;
            }
            if s.start > page && s.start < above {
                above = s.start;
            }
        }
    }
    (inside, below, if above == u64::MAX { 0 } else { above }, live)
}

/// The reservation covering exactly this range, if one does.
///
/// `mremap` needs it: growing a private anonymous mapping means growing the
/// promise behind it, and the new range has to carry the same permissions
/// the old one had rather than a guess.
pub fn covering(start: u64, len: u64) -> Option<(bool, bool, bool)> {
    let here = space();
    let end = start + len;
    unsafe {
        (*(&raw const RESERVED))
            .iter()
            .find(|s| s.live && s.space == here && s.start <= start && end <= s.end)
            .map(|s| (s.writable, s.exec, s.none))
    }
}

/// How many reservations are live, and how much address space they cover.
pub fn report() {
    let here = space();
    let (mut n, mut bytes) = (0usize, 0u64);
    unsafe {
        for s in (*(&raw const RESERVED)).iter() {
            if s.live && s.space == here {
                n += 1;
                bytes += s.end - s.start;
            }
        }
    }
    crate::println!("  reservations: {} live, {} MiB of address space", n, bytes >> 20);
}

/// Drop every reservation belonging to an address space that is going away.
pub fn forget_space(table: u64) {
    unsafe {
        for s in (*(&raw mut RESERVED)).iter_mut() {
            if s.live && s.space == table {
                s.live = false;
            }
        }
    }
}

/// Put a page under `addr` if it falls in a reservation.
///
/// Called from the EL0 fault handler before it decides the process is at
/// fault. Returns whether the fault was handled, in which case the
/// instruction is retried and the program never learns anything happened.
pub fn fault_in(addr: u64) -> bool {
    let here = space();
    let page = addr & !4095;
    let (writable, exec, none) = unsafe {
        match (*(&raw const RESERVED))
            .iter()
            .find(|s| s.live && s.space == here && page >= s.start && page < s.end)
        {
            Some(s) => (s.writable, s.exec, s.none),
            None => return false,
        }
    };
    // alloc() zeroes, which is what an anonymous page must read as. Out of
    // memory here is a real refusal: say no rather than map something else.
    let Some(frame) = frames::alloc() else {
        return false;
    };
    unsafe {
        let root = paging::table_of(crate::arch::current_page_table());
        paging::map_user_permissions(root, page, frame as u64, 4096, exec, writable);
        // A PROT_NONE reservation keeps its backing but denies all user
        // access; mapping it readable would make mprotect(PROT_NONE) a
        // no-op for any page that had not yet been touched.
        if none {
            paging::protect_user_none(root, page, 4096);
        }
    }
    true
}

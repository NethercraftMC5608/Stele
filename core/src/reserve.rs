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
//! So a mapping is recorded here, with what backs it, and mapped nowhere.
//! The first touch of each page faults, and `fault_in` puts a frame under it:
//! a zeroed one for anonymous memory, one read from the file for a private
//! file mapping. That is demand paging, and the table is the process's map
//! of what its address space means -- resident pages are the kernel's, this
//! is the promise behind them.
//!
//! File pages are populated per address space: two processes mapping the same
//! library each read their own copy on first touch. Sharing one copy between
//! them is a page cache, and a page cache needs lifetime, partial-unmap and
//! invalidation semantics that this table does not have yet. Until it does,
//! not duplicating *untouched* pages is the property that matters.

use crate::frames;
use crate::paging;
use core::sync::atomic::{AtomicU64, Ordering};

const MAX: usize = 256;

/// What turns a fault on a page into its contents.
#[derive(Clone, Copy, PartialEq)]
pub enum Backing {
    /// Zero-filled on first touch.
    Anonymous,
    /// Read from `fd` at `offset + (page - area.start)` on first touch.
    /// Each address space reads its own pages; see the module comment.
    /// `dev`/`ino` identify the file the fd named at mmap time, so a page
    /// can be named without a process-local descriptor.
    FilePrivate { fd: i32, offset: u64, dev: u64, ino: u64 },
}

#[derive(Clone, Copy)]
struct VmArea {
    /// The address space this belongs to, so threads sharing one see the
    /// same areas and two processes at the same address do not.
    space: u64,
    start: u64,
    end: u64,
    writable: bool,
    exec: bool,
    /// PROT_NONE: backed, but unreachable from user space.
    none: bool,
    backing: Backing,
    live: bool,
}

static mut VMAS: [VmArea; MAX] = [VmArea {
    space: 0,
    start: 0,
    end: 0,
    writable: false,
    exec: false,
    none: false,
    backing: Backing::Anonymous,
    live: false,
}; MAX];

/// Pages faulted in, by backing, and the bytes handed over doing it. Global
/// and monotonic: the watchdog prints them next to the frame count, which is
/// what shows whether demand paging is doing what it claims.
static ANON_FAULTS: AtomicU64 = AtomicU64::new(0);
static FILE_FAULTS: AtomicU64 = AtomicU64::new(0);
static FAULT_BYTES: AtomicU64 = AtomicU64::new(0);

fn space() -> u64 {
    paging::table_of(crate::arch::current_page_table())
}

fn insert(area: VmArea) -> bool {
    unsafe {
        let r = &mut *(&raw mut VMAS);
        match r.iter_mut().find(|s| !s.live) {
            Some(slot) => {
                *slot = area;
                true
            }
            None => false,
        }
    }
}

/// Record a range as reserved but unmapped. False when the table is full,
/// which the caller must treat as a refusal rather than ignore: a
/// reservation nobody recorded is a segfault on first touch.
pub fn add(start: u64, len: u64, writable: bool, exec: bool, none: bool) -> bool {
    add_backed(start, len, writable, exec, none, Backing::Anonymous)
}

/// Record a private file mapping: pages are read from `fd` on first touch.
///
/// The mapping outlives the descriptor it was made from -- a loader maps a
/// library and closes the fd, and the fault that reads its pages comes much
/// later -- so one reference is pinned with `dup` and the area reads through
/// that. Like Linux's `struct file` held by an mmap, it is not released when
/// the area is trimmed or split: the process exit closes it with every other
/// descriptor, and an fd number stays valid for the life of the process.
pub fn add_file(start: u64, len: u64, writable: bool, exec: bool, fd: i32, offset: u64) -> bool {
    #[cfg(nk_lkl)]
    {
        let owned = crate::lkl::syscall(23, [fd as i64, 0, 0, 0, 0, 0]);
        if owned < 0 {
            return false;
        }
        let (dev, ino) = fstat_identity(owned as i32).unwrap_or((0, 0));
        let area = VmArea {
            space: space(),
            start,
            end: start + len,
            writable,
            exec,
            none: false,
            backing: Backing::FilePrivate { fd: owned as i32, offset, dev, ino },
            live: true,
        };
        if !insert(area) {
            return false;
        }
        true
    }
    #[cfg(not(nk_lkl))]
    {
        let _ = (start, len, writable, exec, fd, offset);
        false
    }
}


pub fn add_backed(start: u64, len: u64, writable: bool, exec: bool, none: bool, backing: Backing) -> bool {
    insert(VmArea {
        space: space(),
        start,
        end: start + len,
        writable,
        exec,
        none,
        backing,
        live: true,
    })
}

/// Move an area's front edge forward, carrying a file backing's offset with
/// it so the page at the new `start` still names the file byte it did.
fn shift_front(a: &mut VmArea, new_start: u64) {
    if let Backing::FilePrivate { offset, .. } = &mut a.backing {
        *offset += new_start - a.start;
    }
    a.start = new_start;
}

/// Forget any reservation overlapping this range, keeping the parts outside
/// it. Pages already faulted in are ordinary mappings and are freed by the
/// caller's own unmap.
pub fn forget(start: u64, len: u64) {
    let here = space();
    let end = start + len;
    unsafe {
        let r = &mut *(&raw mut VMAS);
        for i in 0..MAX {
            if !(r[i].live && r[i].space == here && r[i].start < end && start < r[i].end) {
                continue;
            }
            let (lo, hi) = (r[i].start, r[i].end);
            if start <= lo && hi <= end {
                r[i].live = false;
                continue;
            }
            if start <= lo {
                shift_front(&mut r[i], end);
                continue;
            }
            if hi <= end {
                r[i].end = start;
                continue;
            }
            // The range removed sits inside the area: the tail keeps the
            // promise, with its file offset moved by the same amount.
            let old = r[i];
            r[i].end = start;
            let mut tail = old;
            shift_front(&mut tail, end);
            if !insert(tail) {
                // No record for the tail. Keeping the whole area is the safe
                // failure: it over-reserves the removed range, which costs
                // address space, while dropping the tail would turn a later
                // touch into a fault with no promise behind it.
                r[i] = old;
                crate::println!("  reserve: table full; kept a split area whole");
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
        let r = &mut *(&raw mut VMAS);
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
        let r = &mut *(&raw mut VMAS);
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
                        if piece.0 != old.start {
                            // A file-backed piece past the new edge names a
                            // later byte of the same file.
                            if let Backing::FilePrivate { offset, .. } = &mut slot.backing {
                                *offset += piece.0 - old.start;
                            }
                        }
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
        for s in (*(&raw const VMAS)).iter() {
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

/// The area covering exactly this range, if one does, with its backing.
///
/// `mremap` needs it: growing a private mapping means growing the promise
/// behind it, and the new range has to carry the same permissions and the
/// same backing as the old one rather than a guess.
pub fn covering(start: u64, len: u64) -> Option<(bool, bool, bool, Backing)> {
    let here = space();
    let end = start + len;
    unsafe {
        (*(&raw const VMAS))
            .iter()
            .find(|s| s.live && s.space == here && s.start <= start && end <= s.end)
            .map(|s| (s.writable, s.exec, s.none, s.backing))
    }
}

/// How many reservations are live and how much address space they cover,
/// then what demand paging has served so far.
pub fn report() {
    let here = space();
    let (mut n, mut bytes) = (0usize, 0u64);
    unsafe {
        for s in (*(&raw const VMAS)).iter() {
            if s.live && s.space == here {
                n += 1;
                bytes += s.end - s.start;
            }
        }
    }
    crate::println!("  reservations: {} live, {} MiB of address space", n, bytes >> 20);
    let (anon, file, fault_bytes) = (
        ANON_FAULTS.load(Ordering::Relaxed),
        FILE_FAULTS.load(Ordering::Relaxed),
        FAULT_BYTES.load(Ordering::Relaxed),
    );
    crate::println!(
        "  faulted in:   {} anon pages, {} file pages, {} MiB",
        anon,
        file,
        fault_bytes >> 20
    );
}

/// Drop every reservation belonging to an address space that is going away.
pub fn forget_space(table: u64) {
    unsafe {
        for s in (*(&raw mut VMAS)).iter_mut() {
            if s.live && s.space == table {
                s.live = false;
            }
        }
    }
}

/// Read one page of `fd` at `offset` into `frame`. EOF leaves the rest of
/// the frame as it was (the caller zeroed it), which is what the tail of a
/// mapping that ends mid-page must read as. False is a read error the caller
/// turns into a process-visible fault -- never a partially filled page.
fn read_page(fd: i32, offset: u64, frame: *mut u8) -> bool {
    #[cfg(nk_lkl)]
    {
        #[repr(C)]
        struct IoVec {
            base: u64,
            len: u64,
        }
        let mut done = 0u64;
        while done < 4096 {
            let mut iov = IoVec {
                base: frame as u64 + done,
                len: 4096 - done,
            };
            // 69 is preadv: one page, one iovec, at the file offset the page
            // is backed by. Short reads are retried at the new offset; a
            // zero-length read is EOF and leaves the tail zeroed.
            let rc = crate::lkl::syscall(
                69,
                [fd as i64, &mut iov as *mut IoVec as i64, 1, (offset + done) as i64, 0, 0],
            );
            if rc < 0 {
                return false;
            }
            if rc == 0 {
                break;
            }
            done += rc as u64;
        }
        true
    }
    #[cfg(not(nk_lkl))]
    {
        let _ = (fd, offset, frame);
        false
    }
}

/// The syscall the page-in trampoline raises. Not a Linux number: it is
/// intercepted before any dispatch table is consulted.
pub const PAGEIN_SYSCALL: u64 = 0x4E4B_5049_4E00; // "NKPIN"

/// One outstanding file-backed page fault, saved by the abort handler and
/// completed from the SVC path. The frame is the complete EL0 context to
/// restore; the area is kernel metadata, re-validated on completion.
#[derive(Clone, Copy)]
struct PendingFault {
    page: u64,
    area: VmArea,
    frame: crate::user::Frame,
}

static mut PENDING: [Option<PendingFault>; crate::sched::MAX_TASKS] =
    [None; crate::sched::MAX_TASKS];

/// Save the frame and redirect ELR at the page-in trampoline.
///
/// Called from the abort handler only, and only for file-backed pages. The
/// handler itself never reads the file: entering LKL from the abort path is
/// not the syscall path, and the difference is observable (a demand read
/// there breaks Weston's DRM discovery while the identical read from SVC
/// context works). Returns false when this is not a file fault, so the
/// caller can fall back to synchronous handling.
pub fn defer_file_fault(frame: &mut crate::user::Frame, addr: u64) -> bool {
    let here = space();
    let page = addr & !4095;
    let area = unsafe {
        match (*(&raw const VMAS))
            .iter()
            .find(|s| s.live && s.space == here && page >= s.start && page < s.end)
        {
            Some(s) => *s,
            None => return false,
        }
    };
    if !matches!(area.backing, Backing::FilePrivate { .. }) {
        return false;
    }
    if paging::user_resident(page) {
        return false;
    }
    let me = crate::sched::current_id();
    if me >= crate::sched::MAX_TASKS {
        return false;
    }
    unsafe {
        if (*(&raw const PENDING))[me].is_some() {
            // One outstanding synchronous fault per task. A second one
            // before the first completes is a bug; let the normal fault
            // path deal with the process rather than overwrite the frame.
            return false;
        }
        (*(&raw mut PENDING))[me] = Some(PendingFault { page, area, frame: *frame });
    }
    frame.x[8] = PAGEIN_SYSCALL;
    frame.elr = crate::user::TRAMPOLINE_VA;
    true
}

/// Complete the pending file-backed page fault for the calling task, from
/// the ordinary SVC path. Re-validates everything from kernel state: the
/// trampoline contributes no arguments.
pub fn complete_file_fault(frame: &mut crate::user::Frame) -> bool {
    let me = crate::sched::current_id();
    if me >= crate::sched::MAX_TASKS {
        return false;
    }
    let pending = match unsafe { (*(&raw mut PENDING))[me].take() } {
        Some(p) => p,
        None => return false,
    };
    // A task that exec'd between fault and SVC would otherwise page in a
    // stale address space's VMA.
    if pending.area.space != space() {
        return false;
    }
    if !paging::user_resident(pending.page)
        && !populate(&pending.area, pending.page)
    {
        return false;
    }
    // Back to the faulting instruction, with the registers it had.
    *frame = pending.frame;
    true
}

/// Put a page under `addr` if it falls in a reservation.
///
/// Called from the EL0 fault handler before it decides the process is at
/// fault. Returns whether the fault was handled, in which case the
/// instruction is retried and the program never learns anything happened.
pub fn fault_in(addr: u64) -> bool {
    let here = space();
    let page = addr & !4095;
    let area = unsafe {
        match (*(&raw const VMAS))
            .iter()
            .find(|s| s.live && s.space == here && page >= s.start && page < s.end)
        {
            Some(s) => *s,
            None => return false,
        }
    };
    // A page that is already mapped is not a missing page: this fault is
    // about permission, and replacing the frame would throw away what the
    // program wrote there. Let the fault reporter decide. The test reads the
    // tables, not `AT S1E0R`: that applies EL0 read permission, under which
    // a resident execute-only page looks unmapped.
    if paging::user_resident(page) {
        return false;
    }
    // A file-backed page touched at EL0 goes through the trampoline: the
    // abort path must not read the file (the read belongs to the syscall
    // path, see `defer_file_fault`). The same backing arrived at from
    // uaccess -- a syscall's kernel-side copy into a page of a file mapping
    // -- is already inside that syscall, so populating here is the same
    // context the eager path uses. `in_user_syscall` is exactly that
    // distinction.
    if matches!(area.backing, Backing::FilePrivate { .. }) && !crate::sched::in_user_syscall() {
        return false;
    }
    populate(&area, page)
}

/// Fill one page of `area` and map it. Shared by the fault path and the
/// prefault mode so the A/B differs only in which context calls it.
fn fstat_identity(fd: i32) -> Option<(u64, u64)> {
    #[cfg(nk_lkl)]
    {
        let mut st = [0u8; 128];
        let rc = crate::lkl::syscall(80, [fd as i64, st.as_mut_ptr() as i64, 0, 0, 0, 0]);
        if rc < 0 {
            return None;
        }
        let dev = u64::from_ne_bytes(st[0..8].try_into().unwrap());
        let ino = u64::from_ne_bytes(st[8..16].try_into().unwrap());
        Some((dev, ino))
    }
    #[cfg(not(nk_lkl))]
    {
        let _ = fd;
        None
    }
}

fn populate(area: &VmArea, page: u64) -> bool {
    // alloc() zeroes, which is what an anonymous page must read as, and what
    // the tail of a short file read keeps reading as. Out of memory here is
    // a real refusal: say no rather than map something else.
    let Some(frame) = frames::alloc() else {
        return false;
    };
    match area.backing {
        Backing::Anonymous => {
            ANON_FAULTS.fetch_add(1, Ordering::Relaxed);
        }
        Backing::FilePrivate { fd, offset, dev, ino } => {
            let Some(file_off) = offset.checked_add(page - area.start) else {
                unsafe { frames::free(frame) };
                return false;
            };
            if !read_page(fd, file_off, frame) {
                unsafe { frames::free(frame) };
                return false;
            }
            FILE_FAULTS.fetch_add(1, Ordering::Relaxed);
        }
    }
    FAULT_BYTES.fetch_add(4096, Ordering::Relaxed);
    unsafe {
        // A frame this allocator hands out may have been executable code for
        // a process that has since died, and the I-cache does not snoop the
        // allocator's zeroing writes. Publish before the PTE exists: the
        // retried instruction fetch must not see what the last owner of this
        // frame left in cache.
        if area.exec {
            paging::clean_icache_range(frame as u64, 4096);
        }
        let root = paging::table_of(crate::arch::current_page_table());
        paging::map_user_permissions(root, page, frame as u64, 4096, area.exec, area.writable);
        // A PROT_NONE reservation keeps its backing but denies all user
        // access; mapping it readable would make mprotect(PROT_NONE) a
        // no-op for any page that had not yet been touched.
        if area.none {
            paging::protect_user_none(root, page, 4096);
        }
    }
    true
}

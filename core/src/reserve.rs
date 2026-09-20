//! Address space a process has asked for but not yet touched.
//!
//! nk populates a mapping when it is made: a frame per page, read or zeroed,
//! whether or not the program ever looks at it. That is the simplest thing
//! that works and it is what `MAP_NORESERVE` exists to say is wrong.
//!
//! So a mapping is recorded here, with what backs it, and mapped nowhere.
//! The first touch of each page faults, and `fault_in` puts a frame under it:
//! a zeroed one for anonymous memory, one read from the file for a private
//! file mapping. Resident pages are the kernel's; this table is the promise
//! behind them.
//!
//! The records belong to the address space that made them, not to a global
//! pool. A process with hundreds of mappings must not be able to exhaust the
//! metadata every other process needs: the failure mode of a shared table is
//! that the next `mmap` silently becomes a different kind of mapping.
//!
//! File pages are populated per address space: two processes mapping the same
//! library each read their own copy on first touch. Sharing one copy between
//! them is a page cache, and a page cache needs lifetime, partial-unmap and
//! invalidation semantics that this structure does not have yet. Until it
//! does, not duplicating *untouched* pages is the property that matters.

use crate::frames;
use crate::paging;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

/// Address spaces with VMAs. Far above the processes a machine runs at once;
/// a space's records are released the moment the space is destroyed.
const MAX_SPACES: usize = 64;

/// Ceiling on records in one address space. Reaching it is a refusal, never
/// a change of mapping policy.
const MAX_AREAS: usize = 65536;

/// What turns a fault on a page into its contents.
#[derive(Clone, Copy, PartialEq)]
pub enum Backing {
    /// Zero-filled on first touch.
    Anonymous,
    /// Read from the retained file at `offset + (page - area.start)` on
    /// first touch. The reference belongs to the mapping, not to a
    /// descriptor number: the process may close, dup over or recycle its
    /// fd without the mapping changing what it reads.
    FilePrivate { file: *mut FileBacking, offset: u64 },
}

/// A retained reference to an open file, shared by every VMA record that
/// maps it. `refs` counts records, not processes, and every change to it
/// happens under VMA_LOCK.
pub struct FileBacking {
    handle: *mut u8,
    dev: u64,
    ino: u64,
    refs: u32,
}

/// One more record will point at this backing.
unsafe fn backing_inc(b: &Backing) {
    if let Backing::FilePrivate { file, .. } = b {
        (*(*file)).refs += 1;
    }
}

/// One record has stopped pointing at this backing; at zero the retained
/// file reference is dropped and the record freed.
unsafe fn backing_dec(b: &Backing) {
    if let Backing::FilePrivate { file, .. } = b {
        let fb = &mut **file;
        fb.refs -= 1;
        if fb.refs == 0 {
            file_put(fb.handle);
            drop(Box::from_raw(*file));
        }
    }
}

/// `__NR_arch_specific_syscall` (244) + 5, 6 and 7: the retained-file
/// calls added to arch/lkl by patch-lkl.py. Private syscalls, so the LKL
/// CPU is acquired around the kernel code exactly as for any other syscall.
const NK_FILE_GET: i64 = 249;
const NK_FILE_PREAD: i64 = 250;
const NK_FILE_PUT: i64 = 251;

fn file_get(fd: i32) -> *mut u8 {
    let rc = crate::lkl::syscall(NK_FILE_GET, [fd as i64, 0, 0, 0, 0, 0]);
    if rc <= 0 {
        core::ptr::null_mut()
    } else {
        rc as *mut u8
    }
}

extern "C" {
    /// Direct helpers for callers already inside an LKL syscall: a nested
    /// syscall would re-acquire the LKL CPU and trip its owner accounting.
    fn nk_file_pread_here(handle: *mut u8, buf: *mut u8, len: usize, off: u64) -> i64;
    fn nk_file_put_here(handle: *mut u8);
}

fn file_pread(handle: *mut u8, buf: *mut u8, len: usize, off: u64) -> i64 {
    if crate::sched::in_user_syscall() {
        unsafe { nk_file_pread_here(handle, buf, len, off) }
    } else {
        crate::lkl::syscall(
            NK_FILE_PREAD,
            [handle as i64, buf as i64, len as i64, off as i64, 0, 0],
        )
    }
}

fn file_put(handle: *mut u8) {
    if crate::sched::in_user_syscall() {
        unsafe { nk_file_put_here(handle) };
    } else {
        let _ = crate::lkl::syscall(NK_FILE_PUT, [handle as i64, 0, 0, 0, 0, 0]);
    }
}

#[derive(Clone, Copy)]
struct VmArea {
    start: u64,
    end: u64,
    writable: bool,
    exec: bool,
    /// PROT_NONE: backed, but unreachable from user space.
    none: bool,
    backing: Backing,
}

/// One address space's map, sorted by `start` so lookup is a binary search.
struct VmMap {
    areas: Vec<VmArea>,
}

#[derive(Clone, Copy)]
struct MapEntry {
    space: u64,
    map: *mut VmMap,
}

/// Serializes every VMA table change. Never held across page population:
/// callers take a copy of the area they need and release the lock before any
/// file I/O (see `fault_in`).
static VMA_LOCK: crate::sync::Spinlock = crate::sync::Spinlock::named("VMA");

static mut MAPS: [Option<MapEntry>; MAX_SPACES] = [None; MAX_SPACES];

/// Pages faulted in, by backing, and the bytes handed over doing it.
static ANON_FAULTS: AtomicU64 = AtomicU64::new(0);
static FILE_FAULTS: AtomicU64 = AtomicU64::new(0);
static FAULT_BYTES: AtomicU64 = AtomicU64::new(0);

fn space() -> u64 {
    paging::table_of(crate::arch::current_page_table())
}

/// Run `f` on the calling space's map. With `create`, a space with no map yet
/// gets one; `None` from this function is always "no resources".
fn with_map<R>(space: u64, create: bool, f: impl FnOnce(&mut VmMap) -> R) -> Option<R> {
    let g = VMA_LOCK.lock();
    let result = unsafe {
        let maps = &mut *(&raw mut MAPS);
        let idx = match maps.iter().position(|m| matches!(m, Some(e) if e.space == space)) {
            Some(i) => i,
            None => {
                if !create {
                    VMA_LOCK.unlock(g);
                    return None;
                }
                let Some(free) = maps.iter().position(|m| m.is_none()) else {
                    VMA_LOCK.unlock(g);
                    return None;
                };
                let map = Box::into_raw(Box::new(VmMap { areas: Vec::new() }));
                maps[free] = Some(MapEntry { space, map });
                free
            }
        };
        let map = &mut *maps[idx].as_ref().unwrap().map;
        Some(f(map))
    };
    unsafe { VMA_LOCK.unlock(g) };
    result
}

/// The last area starting at or before `page`, if it covers it.
fn find(areas: &[VmArea], page: u64) -> Option<&VmArea> {
    let idx = match areas.binary_search_by(|a| a.start.cmp(&page)) {
        Ok(i) => i,
        Err(0) => return None,
        Err(i) => i - 1,
    };
    let a = &areas[idx];
    (page >= a.start && page < a.end).then_some(a)
}

fn overlaps(a: &VmArea, start: u64, end: u64) -> bool {
    a.start < end && start < a.end
}

fn insert_sorted(areas: &mut Vec<VmArea>, area: VmArea) -> bool {
    if areas.len() >= MAX_AREAS {
        return false;
    }
    let idx = match areas.binary_search_by(|a| a.start.cmp(&area.start)) {
        Ok(i) | Err(i) => i,
    };
    areas.insert(idx, area);
    true
}

/// Record a range as reserved but unmapped. False is a refusal the caller
/// must report: a mapping with no record faults with nothing behind it.
pub fn add(start: u64, len: u64, writable: bool, exec: bool, none: bool) -> bool {
    add_backed(start, len, writable, exec, none, Backing::Anonymous)
}

/// Record a private file mapping: pages are read from the retained file on
/// first touch. The reference is taken here and lives as long as any VMA
/// record maps the file -- the caller may close its descriptor immediately,
/// which is what mmap callers do.
pub fn add_file(start: u64, len: u64, writable: bool, exec: bool, fd: i32, offset: u64) -> bool {
    #[cfg(nk_lkl)]
    {
        let handle = file_get(fd);
        if handle.is_null() {
            return false;
        }
        let (dev, ino) = fstat_identity(fd).unwrap_or((0, 0));
        let file = Box::into_raw(Box::new(FileBacking { handle, dev, ino, refs: 1 }));
        let area = VmArea {
            start,
            end: start + len,
            writable,
            exec,
            none: false,
            backing: Backing::FilePrivate { file, offset },
        };
        if with_map(space(), true, |m| insert_sorted(&mut m.areas, area)).unwrap_or(false) {
            return true;
        }
        unsafe { backing_dec(&area.backing) };
        false
    }
    #[cfg(not(nk_lkl))]
    {
        let _ = (start, len, writable, exec, fd, offset);
        false
    }
}

pub fn add_backed(
    start: u64,
    len: u64,
    writable: bool,
    exec: bool,
    none: bool,
    backing: Backing,
) -> bool {
    let area = VmArea { start, end: start + len, writable, exec, none, backing };
    if with_map(space(), true, |m| insert_sorted(&mut m.areas, area)).unwrap_or(false) {
        unsafe { backing_inc(&area.backing) };
        true
    } else {
        false
    }
}

/// Forget any record overlapping this range, keeping the parts outside it.
/// Built as a new list and committed only when it is complete: a refused
/// split leaves the original mapping exactly as it was.
pub fn forget(start: u64, len: u64) {
    let end = start + len;
    let old = with_map(space(), false, |m| {
        let mut out: Vec<VmArea> = Vec::new();
        if out.try_reserve(m.areas.len() + 2).is_err() {
            return None;
        }
        for a in &m.areas {
            if !overlaps(a, start, end) {
                out.push(*a);
                continue;
            }
            if start <= a.start && a.end <= end {
                continue; // wholly removed
            }
            if start <= a.start {
                let mut tail = *a;
                shift_front(&mut tail, end);
                out.push(tail);
                continue;
            }
            if a.end <= end {
                let mut head = *a;
                head.end = start;
                out.push(head);
                continue;
            }
            let mut head = *a;
            head.end = start;
            out.push(head);
            let mut tail = *a;
            shift_front(&mut tail, end);
            out.push(tail);
        }
        for a in &out {
            unsafe { backing_inc(&a.backing) };
        }
        Some(core::mem::replace(&mut m.areas, out))
    }).flatten();
    // The file references the replaced records held are dropped outside the
    // lock: `nk_file_put` is an LKL call and is allowed to block.
    if let Some(old) = old {
        for a in &old {
            unsafe { backing_dec(&a.backing) };
        }
    }
}

/// Move an area's front edge forward, carrying a file backing's offset with
/// it so the page at the new `start` still names the file byte it did.
fn shift_front(a: &mut VmArea, new_start: u64) {
    if let Backing::FilePrivate { offset, .. } = &mut a.backing {
        *offset += new_start - a.start;
    }
    a.start = new_start;
}

/// Give a new address space the records of the one it was copied from.
/// `fork` copies pages, and a promise is not a page: without the records the
/// child faults on the first byte of an arena its parent could use.
pub fn inherit(parent: u64, child: u64) {
    let g = VMA_LOCK.lock();
    unsafe {
        let maps = &mut *(&raw mut MAPS);
        let idx = maps.iter().position(|m| matches!(m, Some(e) if e.space == parent));
        let Some(idx) = idx else {
            VMA_LOCK.unlock(g);
            return;
        };
        let src_len = (*maps[idx].as_ref().unwrap().map).areas.len();
        let kid = match maps.iter().position(|m| matches!(m, Some(e) if e.space == child)) {
            Some(i) => i,
            None => {
                let Some(free) = maps.iter().position(|m| m.is_none()) else {
                    VMA_LOCK.unlock(g);
                    return;
                };
                let map = Box::into_raw(Box::new(VmMap { areas: Vec::new() }));
                maps[free] = Some(MapEntry { space: child, map });
                free
            }
        };
        if kid == idx {
            VMA_LOCK.unlock(g);
            return;
        }
        let parent_map = &*maps[idx].as_ref().unwrap().map;
        let kid_map = &mut *maps[kid].as_ref().unwrap().map;
        if kid_map.areas.try_reserve(src_len).is_err() {
            VMA_LOCK.unlock(g);
            return;
        }
        for i in 0..src_len {
            if kid_map.areas.len() >= MAX_AREAS {
                break;
            }
            let area = parent_map.areas[i];
            let at = match kid_map.areas.binary_search_by(|a| a.start.cmp(&area.start)) {
                Ok(i) | Err(i) => i,
            };
            kid_map.areas.insert(at, area);
            unsafe { backing_inc(&area.backing) };
        }
    }
    unsafe { VMA_LOCK.unlock(g) };
}

/// Change the permissions future faults will use. Returns `None` when the
/// new records cannot be allocated -- the original map is untouched then.
pub fn protect(start: u64, len: u64, writable: bool, exec: bool, none: bool) -> Option<bool> {
    let end = start + len;
    // A space with no records at all has nothing to change permissions for:
    // that is "touched nothing", not a failure. ld.so's first mprotect --
    // RELRO on an image nk mapped itself -- arrives before any mmap.
    if with_map(space(), false, |_| ()).is_none() {
        return Some(false);
    }
    let r = with_map(space(), false, |m| {
        let mut out: Vec<VmArea> = Vec::new();
        if out.try_reserve(m.areas.len() + 2).is_err() {
            return None;
        }
        let mut touched = false;
        for a in &m.areas {
            if !overlaps(a, start, end) {
                out.push(*a);
                continue;
            }
            touched = true;
            if a.start < start {
                let mut head = *a;
                head.end = start;
                out.push(head);
            }
            let mut mid = *a;
            mid.start = a.start.max(start);
            mid.end = a.end.min(end);
            mid.writable = writable;
            mid.exec = exec;
            mid.none = none;
            if mid.start != a.start {
                if let Backing::FilePrivate { offset, .. } = &mut mid.backing {
                    *offset += mid.start - a.start;
                }
            }
            out.push(mid);
            if end < a.end {
                let mut tail = *a;
                shift_front(&mut tail, end);
                out.push(tail);
            }
        }
        for a in &out {
            unsafe { backing_inc(&a.backing) };
        }
        Some((touched, core::mem::replace(&mut m.areas, out)))
    }).flatten();
    let r = r.map(|(touched, old)| {
        for a in &old {
            unsafe { backing_dec(&a.backing) };
        }
        touched
    });
    r
}

/// Where an address sits relative to this space's records. Printed by the
/// fault reporter: "unmapped" and "reserved but not yet faulted in" produce
/// the identical translation fault, and the difference is the diagnosis.
pub fn describe(addr: u64) -> (bool, u64, u64, usize) {
    let page = addr & !4095;
    with_map(space(), false, |m| {
        let (mut below, mut above) = (0u64, u64::MAX);
        for a in &m.areas {
            if a.end <= page && a.end > below {
                below = a.end;
            }
            if a.start > page && a.start < above {
                above = a.start;
            }
        }
        let inside = find(&m.areas, page).is_some();
        (inside, below, if above == u64::MAX { 0 } else { above }, m.areas.len())
    })
    .unwrap_or((false, 0, 0, 0))
}

/// The area covering exactly this range, with its backing.
pub fn covering(start: u64, len: u64) -> Option<(bool, bool, bool, Backing)> {
    let end = start + len;
    with_map(space(), false, |m| {
        m.areas
            .iter()
            .find(|a| a.start <= start && end <= a.end)
            .map(|a| (a.writable, a.exec, a.none, a.backing))
    })
    .flatten()
}

/// Live records and address space for this space, then the fault counters.
pub fn report() {
    let (n, bytes) = with_map(space(), false, |m| {
        let mut bytes = 0u64;
        for a in &m.areas {
            bytes += a.end - a.start;
        }
        (m.areas.len(), bytes)
    })
    .unwrap_or((0, 0));
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

/// Drop every record belonging to an address space that is going away.
pub fn forget_space(table: u64) {
    let g = VMA_LOCK.lock();
    let taken = unsafe {
        let maps = &mut *(&raw mut MAPS);
        if let Some(i) = maps.iter().position(|m| matches!(m, Some(e) if e.space == table)) {
            maps[i].take()
        } else {
            None
        }
    };
    unsafe { VMA_LOCK.unlock(g) };
    if let Some(e) = taken {
        let map = unsafe { Box::from_raw(e.map) };
        for a in &map.areas {
            unsafe { backing_dec(&a.backing) };
        }
        drop(map);
    }
}

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

/// Read one page of `fd` at `offset` into `frame`. EOF leaves the rest of
/// the frame as it was (the caller zeroed it), which is what the tail of a
/// mapping that ends mid-page must read as. False is a read error the caller
/// turns into a process-visible fault -- never a partially filled page.
fn read_page(file: *mut FileBacking, offset: u64, frame: *mut u8) -> bool {
    #[cfg(nk_lkl)]
    {
        let handle = unsafe { (*file).handle };
        let mut done = 0u64;
        while done < 4096 {
            let rc = file_pread(
                handle,
                (frame as u64 + done) as *mut u8,
                4096 - done as usize,
                offset + done,
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
        let _ = (file, offset, frame);
        false
    }
}

/// The syscall the page-in trampoline raises. Not a Linux number: it is
/// intercepted before any dispatch table is consulted.
pub const PAGEIN_SYSCALL: u64 = 0x4E4B_5049_4E00; // "NKPIN"

/// One outstanding file-backed page fault, saved by the abort handler and
/// completed from the SVC path.
#[derive(Clone, Copy)]
struct PendingFault {
    space: u64,
    page: u64,
    area: VmArea,
    frame: crate::user::Frame,
}

static mut PENDING: [Option<PendingFault>; crate::sched::MAX_TASKS] =
    [None; crate::sched::MAX_TASKS];

/// Save the frame and redirect ELR at the page-in trampoline. Called from the
/// abort handler only, and only for file-backed pages. Returns false when
/// this is not a file fault, so the caller can fall back to nearby handling.
pub fn defer_file_fault(frame: &mut crate::user::Frame, addr: u64) -> bool {
    let page = addr & !4095;
    let area = with_map(space(), false, |m| find(&m.areas, page).copied()).flatten();
    let Some(area) = area else {
        return false;
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
            return false;
        }
        (*(&raw mut PENDING))[me] = Some(PendingFault { space: space(), page, area, frame: *frame });
    }
    frame.x[8] = PAGEIN_SYSCALL;
    frame.elr = crate::user::TRAMPOLINE_VA;
    true
}

/// Complete the pending file-backed page fault for the calling task, from
/// the ordinary SVC path. Re-validates everything from kernel state.
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
    if pending.space != space() {
        static A: AtomicU64 = AtomicU64::new(0);
        if A.fetch_add(1, Ordering::Relaxed) < 8 { crate::println!("  pagein: space changed"); }
        return false;
    }
    if !paging::user_resident(pending.page) && !populate(&pending.area, pending.page) {
        static B: AtomicU64 = AtomicU64::new(0);
        if B.fetch_add(1, Ordering::Relaxed) < 8 { crate::println!("  pagein: populate failed"); }
        return false;
    }
    *frame = pending.frame;
    true
}

/// Put a page under `addr` if it falls in a recorded area.
///
/// Called from the EL0 fault handler and from uaccess, before either decides
/// the process is at fault. Returns whether the fault was handled, in which
/// case the instruction is retried and the program never learns anything
/// happened.
pub fn fault_in(addr: u64) -> bool {
    let page = addr & !4095;
    let area = with_map(space(), false, |m| find(&m.areas, page).copied()).flatten();
    let Some(area) = area else {
        return false;
    };
    // A page that is already mapped is not a missing page: this fault is
    // about permission. The test reads the tables, not `AT S1E0R`: that
    // applies EL0 read permission, under which a resident execute-only page
    // looks unmapped.
    if paging::user_resident(page) {
        return false;
    }
    // A file-backed page touched at EL0 goes through the trampoline: the
    // abort path must not read the file. The same backing arrived at from
    // uaccess -- a syscall's kernel-side copy into a page of a file mapping
    // -- is already inside that syscall, so populating here is the same
    // context the eager path uses.
    if matches!(area.backing, Backing::FilePrivate { .. }) && !crate::sched::in_user_syscall() {
        return false;
    }
    populate(&area, page)
}

/// Fill one page of `area` and map it.
fn populate(area: &VmArea, page: u64) -> bool {
    let Some(frame) = frames::alloc() else {
        return false;
    };
    match area.backing {
        Backing::Anonymous => {
            ANON_FAULTS.fetch_add(1, Ordering::Relaxed);
        }
        Backing::FilePrivate { file, offset } => {
            let Some(file_off) = offset.checked_add(page - area.start) else {
                unsafe { frames::free(frame) };
                return false;
            };
            if !read_page(file, file_off, frame) {
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
        // allocator's zeroing writes. Publish before the PTE exists.
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

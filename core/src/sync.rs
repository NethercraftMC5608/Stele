//! Semaphores and mutexes that actually sleep.
//!
//! nk's first locks masked interrupts and its first waits spun on `yield_now`.
//! Both are correct on one CPU and neither can carry a kernel: a spinning
//! waiter is indistinguishable from a busy one, so the machine can never go
//! idle, and a lock held across a device operation burns every remaining
//! slice of every other task. Linux's core assumes real blocking everywhere --
//! it is what `struct semaphore`, `struct mutex` and every wait queue are.
//!
//! The waiter set is a bitmask of task slots rather than a list. Sixteen task
//! slots fit in a `u32`, wakeups need no allocation, and a waiter cannot be
//! recorded twice -- which a list has to be careful about and a bitmask
//! cannot get wrong.
//!
//! **The ordering matters and is the classic bug.** Between deciding to sleep
//! and sleeping there is a window in which the wakeup can arrive and be lost,
//! and the task then waits for an event that has already happened. So the
//! waiter bit is set with interrupts masked, and `sched::block` is handed the
//! saved interrupt state to restore *after* the task is marked blocked --
//! never before.

use crate::sched;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering::SeqCst};

/// Mask interrupts and report the previous state. Public because every
/// primitive that has to be safe against an interrupt handler needs it, and
/// duplicating two instructions per user is how they drift.
#[cfg(target_arch = "aarch64")]
pub fn irq_save() -> u64 {
    let daif: u64;
    unsafe {
        core::arch::asm!("mrs {}, daif", "msr daifset, #0x2", out(reg) daif, options(nomem, nostack))
    };
    daif
}

#[cfg(target_arch = "x86_64")]
pub fn irq_save() -> u64 {
    let rflags: u64;
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {}",
            "cli",
            out(reg) rflags,
            options(nomem, nostack)
        );
    }
    rflags
}

/// # Safety
/// `flags` came from `irq_save`.
#[cfg(target_arch = "aarch64")]
pub unsafe fn irq_restore(flags: u64) {
    core::arch::asm!("msr daif, {}", in(reg) flags, options(nomem, nostack));
}

#[cfg(target_arch = "x86_64")]
pub unsafe fn irq_restore(flags: u64) {
    core::arch::asm!(
        "push {}",
        "popfq",
        in(reg) flags,
        options(nomem, nostack)
    );
}

/// A ticket spinlock for cross-CPU exclusion of short critical sections.
///
/// `irq_save` masks the local CPU only. Two CPUs can both hold "the lock"
/// that is just a masked interrupt, then mutate TASKS, the frame allocator,
/// or the semaphore registry at once. This lock is the thing that actually
/// excludes another CPU: tickets give FIFO order so no CPU starves, and the
/// holder keeps interrupts masked so an interrupt handler on the same CPU
/// cannot deadlock against the lock its own task holds.
///
/// Short sections only -- frame allocation, task table edits, registry
/// updates. Never held across a block, a switch, or a syscall: spinning
/// with interrupts masked is burning the whole CPU, and on one CPU it is
/// also uncontended, so the cost model only matters once APs arrive.
pub struct Spinlock {
    what: &'static str,
    next: AtomicU32,
    now: AtomicU32,
    owner: AtomicU32,
    /// Return address of the last successful acquisition, for diagnosis.
    owner_lr: AtomicU64,
}

#[cfg(target_arch = "aarch64")]
fn caller_lr() -> u64 {
    let lr: u64;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr, options(nomem, nostack)) };
    lr
}

#[cfg(target_arch = "x86_64")]
fn caller_lr() -> u64 {
    // No link register; the diagnostic is aarch64-only.
    0
}

const LOCK_FREE: u32 = u32::MAX;

impl Spinlock {
    pub const fn new() -> Self {
        Spinlock {
            what: "unnamed",
            next: AtomicU32::new(0),
            now: AtomicU32::new(0),
            owner: AtomicU32::new(LOCK_FREE),
            owner_lr: AtomicU64::new(0),
        }
    }

    pub const fn named(what: &'static str) -> Self {
        Spinlock {
            what,
            next: AtomicU32::new(0),
            now: AtomicU32::new(0),
            owner: AtomicU32::new(LOCK_FREE),
            owner_lr: AtomicU64::new(0),
        }
    }

    /// Take the lock, masking interrupts across the hold.
    /// Returns the saved IRQ state for `unlock`.
    pub fn lock(&self) -> u64 {
        let entry_lr = caller_lr();
        let flags = irq_save();
        let ticket = self.next.fetch_add(1, SeqCst);
        let mut spins = 0u32;
        while self.now.load(SeqCst) != ticket {
            spins += 1;
            if spins & 0xfff == 0 {
                core::hint::spin_loop();
            }
            // A lock that never turns is a deadlock, not a slow machine.
            // Panicking names it; spinning forever hides it behind a hang
            // whose cause has to be reconstructed from a dead stop.
            assert!(
                spins < u32::MAX / 2,
                "spinlock {} deadlock: ticket {} never served, serving {} (task {} taker_lr {:#x}; held by task {} lr {:#x})",
                self.what,
                ticket,
                self.now.load(SeqCst),
                crate::sched::current_id(),
                entry_lr,
                self.owner.load(SeqCst),
                self.owner_lr.load(SeqCst)
            );
        }
        self.owner.store(crate::sched::current_id() as u32, SeqCst);
        self.owner_lr.store(caller_lr(), SeqCst);
        crate::arch::memory_barrier();
        flags
    }

    /// Ticket state, for the freeze dump: (next to issue, now serving).
    pub fn tickets(&self) -> (u32, u32) {
        (self.next.load(SeqCst), self.now.load(SeqCst))
    }

    /// # Safety
    /// `flags` came from `lock` on this lock, and the caller still holds it.
    pub unsafe fn unlock(&self, flags: u64) {
        crate::arch::memory_barrier();
        self.owner.store(LOCK_FREE, SeqCst);
        self.owner_lr.store(0, SeqCst);
        self.now.fetch_add(1, SeqCst);
        irq_restore(flags);
    }
}

unsafe impl Sync for Spinlock {}

/// Every semaphore and mutex gets a number, and every blocked task records
/// which one it is waiting on.
///
/// Without this, a deadlock reports itself as "every task is blocked", which
/// is the fact rather than the cause. With it, the watchdog prints who waits
/// on what and what each of those holds -- a wait graph, which either names
/// the cycle or names the thing nobody raised.
static mut NEXT_ID: u32 = 1;

const MAX_TRACKED: usize = 256;
static mut REGISTRY: [*const Semaphore; MAX_TRACKED] =
    [core::ptr::null(); MAX_TRACKED];
static mut MUTEXES: [*const Mutex; MAX_TRACKED] = [core::ptr::null(); MAX_TRACKED];

/// Marks an object's storage as belonging to a live object, or to one that
/// `nk_sem_free`/`nk_mutex_free` has given back. The registry keeps raw
/// pointers for ever -- it exists for the watchdog, not for allocation --
/// so without this the report would happily dereference memory the kernel
/// has handed to something else and print its bytes as semaphore state.
/// That is exactly how a freed object reads as "corruption".
const OBJ_LIVE: u64 = 0x5345_4d41_5048_4f52; // "SEMAPHOR"
const OBJ_FREED: u64 = 0x4652_4545_4453_454d; // "FREEDSEM"

fn next_id() -> u32 {
    unsafe {
        let flags = irq_save();
        let id = NEXT_ID;
        NEXT_ID += 1;
        irq_restore(flags);
        id
    }
}

/// Remember a semaphore so the watchdog can report its state. Only tracks the
/// first `MAX_TRACKED`; beyond that a semaphore still works, it is simply not
/// in the report.
pub fn track(s: &Semaphore) {
    unsafe {
        let flags = irq_save();
        let n = &mut *(&raw mut REGISTRY);
        for slot in n.iter_mut() {
            if slot.is_null() {
                *slot = s as *const Semaphore;
                break;
            }
        }
        irq_restore(flags);
    }
}

/// Forget a semaphore that is about to be freed. Without this the report
/// keeps dereferencing it, and the bytes the allocator leaves or reuses
/// read as counters -- a "corrupt" semaphore that never existed.
pub fn untrack(s: *const Semaphore) {
    unsafe {
        let flags = irq_save();
        let n = &mut *(&raw mut REGISTRY);
        for slot in n.iter_mut() {
            if *slot == s {
                *slot = core::ptr::null();
                break;
            }
        }
        irq_restore(flags);
    }
}

pub fn untrack_mutex(m: *const Mutex) {
    unsafe {
        let flags = irq_save();
        let n = &mut *(&raw mut MUTEXES);
        for slot in n.iter_mut() {
            if *slot == m {
                *slot = core::ptr::null();
                break;
            }
        }
        irq_restore(flags);
    }
}

/// The id of a tracked semaphore, by pointer. For the boot line that names
/// LKL's `cpu.sem` and `cpu.shutdown_sem` -- "task waits on sem 31" is only
/// useful once 31 has a name.
pub fn sem_id(p: *const Semaphore) -> Option<u32> {
    unsafe {
        for slot in (*(&raw const REGISTRY)).iter() {
            if !slot.is_null() && *slot == p {
                return Some((**slot).id);
            }
        }
        None
    }
}

/// Remember a mutex. Ids come from the same counter as semaphores, so a
/// wait graph that reported only one of the two showed tasks waiting on
/// numbers that appeared nowhere -- which reads as corruption rather than as
/// the other kind of object.
pub fn track_mutex(m: &Mutex) {
    unsafe {
        let flags = irq_save();
        let n = &mut *(&raw mut MUTEXES);
        for slot in n.iter_mut() {
            if slot.is_null() {
                *slot = m as *const Mutex;
                break;
            }
        }
        irq_restore(flags);
    }
}

/// Print every tracked semaphore and mutex that anybody is waiting on, and
/// for a mutex, who holds it -- which is the edge that closes a deadlock.
pub fn report() {
    unsafe {
        for slot in (*(&raw const REGISTRY)).iter() {
            if slot.is_null() {
                continue;
            }
            let s = &**slot;
            if !s.alive() {
                crate::println!(
                    "          sem   {:<3} STALE registry entry at {:#x} (freed)",
                    s.id,
                    *slot as u64
                );
                continue;
            }
            let (count, w0, w1) = (
                s.count.load(SeqCst),
                s.waiters[0].load(SeqCst),
                s.waiters[1].load(SeqCst),
            );
            if w0 != 0 || w1 != 0 || count != 0 {
                let taker = s.taker.load(SeqCst);
                let giver = s.giver.load(SeqCst);
                crate::println!(
                    "          sem   {:<3}@{:#x} count {:<4} waiters [{:#018b}, {:#018b}] ups {} downs {} last taken by uid {} slot {} given by uid {} slot {} site {:#x}",
                    s.id,
                    *slot as u64,
                    count,
                    w0,
                    w1,
                    s.ups.load(SeqCst),
                    s.downs.load(SeqCst),
                    taker as i64,
                    if taker == u64::MAX { -1 } else { (taker & 0xffff) as i64 },
                    giver as i64,
                    if giver == u64::MAX { -1 } else { (giver & 0xffff) as i64 },
                    s.site()
                );
            }
        }
        for slot in (*(&raw const MUTEXES)).iter() {
            if slot.is_null() {
                continue;
            }
            let m = &**slot;
            if !m.alive() {
                crate::println!(
                    "          mutex {:<3} STALE registry entry at {:#x} (freed)",
                    m.id,
                    *slot as u64
                );
                continue;
            }
            let (w0, w1) = (m.waiters[0].load(SeqCst), m.waiters[1].load(SeqCst));
            if w0 != 0 || w1 != 0 || m.owner().is_some() {
                match m.owner() {
                    Some(o) => crate::println!(
                        "          mutex {:<3}@{:#x} held by task {:<2} depth {}  waiters [{:#018b}, {:#018b}]",
                        m.id, *slot as u64, o, m.depth.load(SeqCst), w0, w1
                    ),
                    None => crate::println!(
                        "          mutex {:<3}@{:#x} free            waiters [{:#018b}, {:#018b}]",
                        m.id, *slot as u64, w0, w1
                    ),
                }
            }
        }
    }
}

/// Atomics, and `&self` rather than `&mut self`, and both are the fix for a
/// real bug rather than style.
///
/// `down` blocks in the middle of its own critical section: it holds a
/// reference to the semaphore across a context switch, while another task
/// mutates the same object through a reference of its own. With `&mut self`
/// that is aliasing undefined behaviour, and the compiler acts on it -- it may
/// keep `count` in a register across the switch and re-test the stale value
/// when the task resumes. The task then blocks again on a semaphore that was
/// raised while it slept, re-arming its own waiter bit each time.
///
/// It presents as a semaphore holding a token with somebody still waiting for
/// it, which is a state the code plainly cannot produce, and as a machine
/// where every thread is blocked. Nothing reports it.
pub struct Semaphore {
    pub id: u32,
    /// Set by `nk_sem_alloc` once the box has an address, so the registry
    /// can tell "this pointer is still the object it named".
    self_ptr: AtomicU64,
    /// `OBJ_LIVE` until `nk_sem_free`, then `OBJ_FREED`. See the constants.
    magic: AtomicU64,
    count: AtomicI32,
    waiters: [AtomicU64; 2],
    /// Lifetime totals. The watchdog prints them for any semaphore with a
    /// waiter: `ups` counted but the waiter still there means the wake went
    /// somewhere else (or nowhere); no `ups` at all means nobody left is in
    /// a position to wake it -- the scheduler is dead, not lossy.
    ups: AtomicU64,
    /// Times `down` found no token and parked. Fast-path takes leave no
    /// trace; every one of these is a sleep that needed a matching wake.
    downs: AtomicU64,
    /// The last task to take a token, and the last to hand one back. A
    /// semaphore with a waiter and no tokens is being held by somebody, and
    /// the whole question is who: the wait graph says which tasks are
    /// stopped, not which one is standing on the thing they want.
    taker: AtomicU64,
    giver: AtomicU64,
    /// Rotation cursor: the waiter woken last, so the next `up` resumes the
    /// scan after it. See `up` for why lowest-bit wakes are not enough.
    last_woken: AtomicU32,
    /// Where nk_sem_alloc was called from (the return address at entry).
    /// LKL allocates its semaphores from a dozen different call sites and
    /// the wait graph only ever prints the small integer id, so "task 44
    /// waiting on sem 88" names nothing. The raw address is resolved
    /// against libnklkl.a on the host after the run.
    site: AtomicU64,
}

/// The return address of the caller, for `site` above. `nomem, nostack`
/// because reading a register is the whole of it.
#[inline(never)]
#[cfg(target_arch = "aarch64")]
pub(crate) fn caller_pc() -> u64 {
    let r: u64;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) r, options(nomem, nostack)) };
    r
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn caller_pc() -> u64 {
    // The call instruction pushed the return address; it is the word at
    // the top of the stack on entry to this function.
    let r: u64;
    unsafe { core::arch::asm!("mov {}, [rsp]", out(reg) r, options(nomem, nostack)) };
    r
}

impl Semaphore {
    pub fn new(count: i32) -> Self {
        Semaphore {
            id: next_id(),
            self_ptr: AtomicU64::new(0),
            magic: AtomicU64::new(OBJ_LIVE),
            count: AtomicI32::new(count),
            waiters: [AtomicU64::new(0), AtomicU64::new(0)],
            ups: AtomicU64::new(0),
            downs: AtomicU64::new(0),
            taker: AtomicU64::new(u64::MAX),
            giver: AtomicU64::new(u64::MAX),
            last_woken: AtomicU32::new(0),
            site: AtomicU64::new(0),
        }
    }

    pub fn down(&self) {
        let me = sched::current_id();
        let me_mask = 1u64 << (me % 64);
        let mut flags = irq_save();
        loop {
            // Take a token with one atomic read-modify-write.
            //
            // The obvious `load; if > 0 { store(count - 1) }` is not one:
            // the interrupt mask stops re-entry on *this* CPU and nothing on
            // any other, so two CPUs could both read the last token, both
            // store 0 over it, and both return as if they had taken it. The
            // semaphore then grants two for one `up`, one partner proceeds
            // without a token, and the pair eventually blocks on each other
            // with the count at zero -- measured on four ARM CPUs as a
            // ping-pong pair both asleep, `ph=2`, at the exact moment the
            // storm's clock ran out. x86's stronger ordering made it rare
            // enough to survive hundreds of runs.
            let mut count = self.count.load(SeqCst);
            while count > 0 {
                match self.count.compare_exchange_weak(count, count - 1, SeqCst, SeqCst) {
                    Ok(_) => {
                        self.taker.store(sched::current_uid() as u64, SeqCst);
                        // Taking the token ends this task's interest in the
                        // semaphore, so drop the waiter bit even though the
                        // wake that raced the block means it was never
                        // re-armed. Leaving it set is how a later `up` finds
                        // a "waiter" that is not waiting and loses the wake
                        // on it.
                        self.waiters[me / 64].fetch_and(!me_mask, SeqCst);
                        unsafe { irq_restore(flags) };
                        return;
                    }
                    Err(current) => count = current,
                }
            }
            self.waiters[me / 64].fetch_or(me_mask, SeqCst);
            self.downs.fetch_add(1, SeqCst);
            // Marks this task blocked, restores `flags`, and switches away
            // -- but only after re-checking the count under the scheduler
            // lock. An `up` that landed after the waiter bit was set (and so
            // while this task still read as Running, refusing its wake) is
            // seen here as a token, and this task does not sleep through it.
            // Without the re-check both partners of a ping-pong can block on
            // each other, each having lost the other's wake.
            sched::block_on_cond(flags, self.id, || {}, || self.count.load(SeqCst) > 0);
            // `block_on_cond` restored interrupts; mask again for the retry.
            flags = irq_save();
        }
    }

    /// Set the allocation call site. See the field.
    pub fn set_site(&self, site: u64) {
        self.site.store(site, SeqCst);
    }

    /// Set the address of the object this is, for the registry's identity
    /// check. Called once, right after the box is placed.
    pub fn set_self(&self, p: *const Semaphore) {
        self.self_ptr.store(p as u64, SeqCst);
    }

    pub fn alive(&self) -> bool {
        self.magic.load(SeqCst) == OBJ_LIVE
    }

    /// Called by `nk_sem_free` before the storage goes back. Any straggler
    /// read after this must see a freed object, not whatever the allocator
    /// puts in the same memory next.
    pub fn poison(&self) {
        self.magic.store(OBJ_FREED, SeqCst);
    }

    /// Where this semaphore was allocated, for the watchdog's wait graph.
    pub fn site(&self) -> u64 {
        self.site.load(SeqCst)
    }

    pub fn up(&self) {
        let flags = irq_save();
        self.count.fetch_add(1, SeqCst);
        self.ups.fetch_add(1, SeqCst);
        self.giver.store(sched::current_uid() as u64, SeqCst);
        // Exactly one waiter, not all of them.
        //
        // Waking all of them and letting the losers re-check looks harmless
        // and is not. A caller that counts its own sleepers -- one `up` per
        // sleeper, which is how LKL's CPU lock is written -- sees each
        // spurious wakeup re-enter its wait loop and increment that count
        // again. The bookkeeping inflates, the ups and downs stop matching,
        // and the result is a set of threads that are each certain somebody
        // else holds the thing they are waiting for.
        //
        // And exactly one waiter in rotation, not the lowest-numbered one:
        // lowest-bit wakes starve higher-numbered waiters when a lower one
        // re-queues constantly -- measured on the render boot, a WebKit
        // thread polling gettimeofday sat on the CPU semaphore's lowest bit
        // while the network process queued behind it for seven million
        // slices. The scan resumes after the last waiter woken, so every
        // waiter is reached before any waiter is woken twice.
        //
        // A set bit is not proof of a waiter: the bit can be stale, left
        // by a task that took a token after a racing wake or that was
        // woken out of this `down` by a signal and now blocks elsewhere.
        // Clearing the bit and stopping there would strand the token in
        // `count` while a real waiter slept -- a lost wake with no trace.
        // So clear, try to wake, and keep scanning only while the wake
        // was refused.
        // Clear the hint only as part of a *successful* transition to
        // Ready. Clearing it before the wake attempt discarded the only
        // indication that a waiter existed when the wake was refused (the
        // waiter had registered but not yet committed `Blocked`); if the
        // token was then consumed by anyone else, the waiter slept for ever
        // with no bit and no token. A refusal leaves the hint in place: the
        // token stays in `count`, and the waiter's protected recheck either
        // takes it or re-blocks with the hint still set.
        let mut woken = None;
        let start = self.last_woken.load(SeqCst) as usize;
        for step in 1..=128usize {
            let bit = (start + step) % 128;
            let idx = bit / 64;
            let mask = 1u64 << (bit % 64);
            if self.waiters[idx].load(SeqCst) & mask == 0 {
                continue;
            }
            // Consume the registration inside the wake transition: the
            // woken task cannot run until RUNQUEUE is released, so it cannot
            // re-register before its old registration is gone. Clearing after
            // `wake_on` returned (outside the lock) is the ABA window where a
            // stale wake removed a newer registration.
            if sched::wake_on_locked(bit, self.id, || {
                self.waiters[idx].fetch_and(!mask, SeqCst);
            }) {
                woken = Some(bit);
                break;
            }
        }
        unsafe { irq_restore(flags) };
        if let Some(id) = woken {
            self.last_woken.store(id as u32, SeqCst);
            sched::yield_now();
        }
    }
}

/// A sleeping mutex, optionally recursive.
///
/// Recursion is not an indulgence: Linux's host interface asks for it by
/// name, because some of its locks are taken again from inside a callback the
/// lock holder made.
/// Atomic for the same reason as `Semaphore`; see the note there.
const NO_OWNER: usize = usize::MAX;

pub struct Mutex {
    pub id: u32,
    /// See `Semaphore::magic`: the diagnostics registry holds the pointer
    /// past the object's life, so the object says when it has ended.
    magic: AtomicU64,
    owner: AtomicUsize,
    depth: AtomicU32,
    recursive: bool,
    waiters: [AtomicU64; 2],
    /// Rotation cursor, as in `Semaphore::last_woken`.
    last_woken: AtomicU32,
}

impl Mutex {
    pub fn new(recursive: bool) -> Self {
        Mutex {
            id: next_id(),
            magic: AtomicU64::new(OBJ_LIVE),
            owner: AtomicUsize::new(NO_OWNER),
            depth: AtomicU32::new(0),
            recursive,
            waiters: [AtomicU64::new(0), AtomicU64::new(0)],
            last_woken: AtomicU32::new(0),
        }
    }

    pub fn alive(&self) -> bool {
        self.magic.load(SeqCst) == OBJ_LIVE
    }

    /// See `Semaphore::poison`.
    pub fn poison(&self) {
        self.magic.store(OBJ_FREED, SeqCst);
    }

    pub fn owner(&self) -> Option<usize> {
        match self.owner.load(SeqCst) {
            NO_OWNER => None,
            o => Some(o),
        }
    }

    pub fn lock(&self) {
        let me = sched::current_id();
        loop {
            let flags = irq_save();
            let owner = self.owner.load(SeqCst);
            if owner == NO_OWNER {
                // Claim with one atomic RMW, for the same reason
                // `Semaphore::down` takes its token that way: `load` then
                // `store` lets two CPUs both see it free and both become
                // owner, which is not a lock at all. Found on ARM; x86's
                // ordering hid it.
                if self
                    .owner
                    .compare_exchange(NO_OWNER, me, SeqCst, SeqCst)
                    .is_err()
                {
                    unsafe { irq_restore(flags) };
                    continue;
                }
                self.depth.store(1, SeqCst);
                // See `Semaphore::down`: a taken lock ends the interest,
                // and a stale bit here would eat a later unlock's wake.
                self.waiters[me / 64].fetch_and(!(1u64 << (me % 64)), SeqCst);
                unsafe { irq_restore(flags) };
                return;
            }
            if owner == me {
                if !self.recursive {
                    unsafe { irq_restore(flags) };
                    panic!("deadlock: task {} took a non-recursive mutex twice", me);
                }
                self.depth.fetch_add(1, SeqCst);
                unsafe { irq_restore(flags) };
                return;
            }
            self.waiters[me / 64].fetch_or(1u64 << (me % 64), SeqCst);
            // Predicate: `owner == NO_OWNER` (the unlock has landed).
            // Protected by: `owner`, an atomic; the runqueue lock is the
            //   thing that makes "check and sleep" atomic against the
            //   waker, which reaches this mutex only through `wake_on`.
            // Published: `unlock` stores NO_OWNER *before* it scans the
            //   waiters bitmask and calls `wake_on`; the store is a SeqCst
            //   RMW-free store and the waker's `wake_on` then acquires
            //   RUNQUEUE, so the release/acquire pair makes the store
            //   visible to anyone who gets the lock afterwards.
            // Ordering: unlock's store happens-before its RUNQUEUE acquire,
            //   which happens-before our RUNQUEUE acquire here.
            // Why the re-check closes it: an unlock that lands between our
            //   arming this bit and our choosing to sleep is seen right here
            //   -- owner is NO_OWNER, so we never set Blocked and retry the
            //   fast path. An unlock after we are Blocked finds us Blocked
            //   in `wake_on` and wakes us. There is no interleaving in
            //   which the unlock is neither seen nor woken on, which was
            //   exactly the failure of the unconditional `block_on`: it set
            //   Blocked *after* the wake had already been refused.
            sched::block_on_cond(flags, self.id, || {}, || self.owner.load(SeqCst) == NO_OWNER);
        }
    }

    pub fn unlock(&self) {
        let flags = irq_save();
        if self.depth.fetch_sub(1, SeqCst) > 1 {
            unsafe { irq_restore(flags) };
            return;
        }
        self.owner.store(NO_OWNER, SeqCst);
        // One waiter here too: only one of them can take the mutex, and
        // waking the rest to discover that is churn a single-CPU scheduler
        // pays for in full. In rotation, for the same starvation reason as
        // `Semaphore::up` -- and like there, a refused wake (stale bit)
        // must not end the scan, or the unlock lands on nobody.
        // Clear the hint only as part of a *successful* transition to
        // Ready. Clearing it before the wake attempt discarded the only
        // indication that a waiter existed when the wake was refused (the
        // waiter had registered but not yet committed `Blocked`); if the
        // token was then consumed by anyone else, the waiter slept for ever
        // with no bit and no token. A refusal leaves the hint in place: the
        // token stays in `count`, and the waiter's protected recheck either
        // takes it or re-blocks with the hint still set.
        let mut woken = None;
        let start = self.last_woken.load(SeqCst) as usize;
        for step in 1..=128usize {
            let bit = (start + step) % 128;
            let idx = bit / 64;
            let mask = 1u64 << (bit % 64);
            if self.waiters[idx].load(SeqCst) & mask == 0 {
                continue;
            }
            // Consume the registration inside the wake transition: the
            // woken task cannot run until RUNQUEUE is released, so it cannot
            // re-register before its old registration is gone. Clearing after
            // `wake_on` returned (outside the lock) is the ABA window where a
            // stale wake removed a newer registration.
            if sched::wake_on_locked(bit, self.id, || {
                self.waiters[idx].fetch_and(!mask, SeqCst);
            }) {
                woken = Some(bit);
                break;
            }
        }
        unsafe { irq_restore(flags) };
        if let Some(id) = woken {
            self.last_woken.store(id as u32, SeqCst);
            sched::yield_now();
        }
    }
}

// ---------------------------------------------------------------- console --

/// Serialises whole `print!` calls once more than one CPU can print.
///
/// The console used to be lock-free on the grounds that "every other CPU is
/// parked". Secondaries running nk made that false, and two CPUs printing at
/// once interleave inside a line: the first four-CPU ARM boot produced one
/// unreadable smear and swallowed a boot line entirely.
///
/// Why it is not simply always locked: the first prints of the boot happen
/// *before* `paging::init`, with the MMU off. With translation off, memory is
/// Device-nGnRnE, and an atomic read-modify-write -- a ticket lock's
/// `fetch_add` -- on Device memory is an abort, not a spin. Measured: taking
/// this lock from the first `println!` turned the boot into an endless
/// "!!EXC 0x96000035" (same-EL data abort) before a single banner byte.
///
/// So the lock is enabled by `enable_console_lock`, which `smp::bring_up`
/// calls just before it starts any secondary -- at which point translation is
/// on and there is something to serialise. Before that the machine is
/// single-threaded by construction and the plain store of the flag is safe.
static CONSOLE_LOCK: Spinlock = Spinlock::named("CONSOLE");
static CONSOLE_MP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// From now on, `print!` serialises. Call before starting secondaries.
pub fn enable_console_lock() {
    CONSOLE_MP.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// Take the console lock if it is in use, or return `None` when this is still
/// a single-CPU machine (or the MMU is not on yet).
pub fn console_lock() -> Option<u64> {
    if !CONSOLE_MP.load(core::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    Some(CONSOLE_LOCK.lock())
}

/// # Safety
/// `flags` came from `console_lock` on this lock.
pub unsafe fn console_unlock(flags: Option<u64>) {
    if let Some(f) = flags {
        CONSOLE_LOCK.unlock(f);
    }
}

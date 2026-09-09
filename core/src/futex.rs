//! The futex, which is nk's for the same reason `mmap` and `fork` are.
//!
//! A futex is an address two threads agree on: one sleeps until the value
//! there changes, another wakes it. Everything about that is a fact about the
//! *address space*, and the address space is nk's -- Linux has no mapping for
//! it and could not hash it into the same bucket for two threads even if it
//! did. Forwarding `futex` to Linux would give each thread a private queue
//! and a `pthread_join` that never returns.
//!
//! So the key is `(TTBR0, address)`. Two threads share an address space, so
//! they share a key; two processes do not, so the same numeric address in
//! each is a different futex, which is what `FUTEX_PRIVATE_FLAG` means.
//!
//! Small and linear on purpose. A hash table is the right shape at scale and
//! this is not at scale: a handful of threads, each waiting on one address.
//! Making it a table is a change to this file and nothing else.

use crate::sched;
use crate::sync::{irq_restore, irq_save};
use crate::uaccess;

const WAIT: u64 = 0;
const WAKE: u64 = 1;
const WAIT_BITSET: u64 = 9;
const WAKE_BITSET: u64 = 10;
/// Everything glibc asks for is private, and the private flag is not part of
/// the operation; it says only that the key need not be global.
const PRIVATE: u64 = 128;
const CLOCK_REALTIME: u64 = 256;

const MAX_WAITERS: usize = 64;

#[derive(Clone, Copy)]
struct Waiter {
    space: u64,
    addr: u64,
    task: usize,
    /// Which bits of a `*_BITSET` wake this waiter answers to. Plain WAIT is
    /// the same thing with every bit set, which is why there is one path.
    bits: u32,
    live: bool,
    /// Set by a waker before it makes the task ready, so a wake that arrives
    /// between the value check and the block is not lost.
    woken: bool,
    deadline: Option<(bool, i64)>,
    expired: bool,
    uid: usize,
    token: u64,
}

static mut WAITERS: [Waiter; MAX_WAITERS] = [Waiter {
    space: 0,
    addr: 0,
    task: 0,
    bits: 0,
    live: false,
    woken: false,
    deadline: None,
    expired: false,
    uid: 0,
    token: 0,
}; MAX_WAITERS];

static mut NEXT_TOKEN: u64 = 0;

fn clock_ns(realtime: bool) -> Result<i64, i64> {
    let mut ts = [0i64; 2];
    let was = sched::set_user_syscall(false);
    let rc = crate::lkl::syscall(113, [if realtime { 0 } else { 1 }, ts.as_mut_ptr() as i64, 0, 0, 0, 0]);
    sched::set_user_syscall(was);
    if rc < 0 { return Err(rc); }
    Ok(ts[0].saturating_mul(1_000_000_000).saturating_add(ts[1]))
}

static mut TIMER_TASK: Option<usize> = None;
static mut TIMER_DUE: bool = false;

pub fn tick() {
    unsafe {
        if let Some(task) = TIMER_TASK {
            TIMER_DUE = true;
            sched::wake_on(task, 0);
        }
    }
}

extern "C" fn timer_thread(_: usize) {
    loop {
        let flags = irq_save();
        if !unsafe { core::mem::replace(&mut *(&raw mut TIMER_DUE), false) } {
            sched::block_on(flags, 0);
            continue;
        }
        let snapshot = unsafe { *(&raw const WAITERS) };
        let needed = snapshot.iter().any(|s| s.live && !s.woken && !s.expired && s.deadline.is_some());
        unsafe { irq_restore(flags) };
        if !needed { continue; }
        let mono = clock_ns(false);
        let real = clock_ns(true);
        let flags = irq_save();
        unsafe {
            for (s, old) in (*(&raw mut WAITERS)).iter_mut().zip(snapshot.iter()) {
                if !s.live || s.woken || s.expired || s.token != old.token || sched::uid(s.task) != s.uid { continue; }
                if let Some((realtime, end)) = s.deadline {
                    let now = if realtime { real } else { mono };
                    if matches!(now, Ok(now) if now >= end) {
                        s.expired = true;
                        s.deadline = None;
                        sched::wake_on(s.task, 0);
                    }
                }
            }
            irq_restore(flags);
        }
    }
}

/// The address space a futex belongs to.
fn space() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) v, options(nomem, nostack)) };
    crate::paging::table_of(v)
}

/// # futex(uaddr, op, val, timeout, uaddr2, val3)
pub fn futex(uaddr: u64, op: u64, val: u32, timeout: u64, _uaddr2: u64, val3: u32) -> i64 {
    let cmd = op & !(PRIVATE | CLOCK_REALTIME);
    let mut deadline = None;
    if timeout != 0 && (cmd == WAIT || cmd == WAIT_BITSET) {
        let mut bytes = [0u8; 16];
        if uaccess::copy_from_user(&mut bytes, timeout).is_err() { return uaccess::EFAULT; }
        let sec = i64::from_le_bytes(bytes[..8].try_into().unwrap());
        let ns = i64::from_le_bytes(bytes[8..].try_into().unwrap());
        if sec < 0 || !(0..1_000_000_000).contains(&ns) { return -22; }
        let mut end = sec.saturating_mul(1_000_000_000).saturating_add(ns);
        if cmd == WAIT {
            end = match clock_ns(false) { Ok(now) => now.saturating_add(end), Err(e) => return e };
        }
        deadline = Some((op & CLOCK_REALTIME != 0, end));
    }
    if op & CLOCK_REALTIME != 0 && cmd != WAIT_BITSET { return -38; }
    if uaddr & 3 != 0 { return -22; }
    match cmd {
        WAIT => wait(uaddr, val, !0, deadline),
        WAIT_BITSET => wait(uaddr, val, val3, deadline),
        WAKE => wake(uaddr, val, !0),
        WAKE_BITSET => wake(uaddr, val, val3),
        other => {
            crate::println!("  futex: operation {} is not implemented", other);
            -38 // -ENOSYS
        }
    }
}

/// Sleep while the value at `uaddr` is still `val`.
///
/// The comparison and the decision to sleep have to be one step as far as any
/// waker is concerned, or a wake can land in between and be lost -- the
/// classic way to build a `pthread_join` that hangs once a week. Interrupts
/// are masked across both, and `block_on` puts them back only after the task
/// is marked blocked.
fn wait(uaddr: u64, val: u32, bits: u32, deadline: Option<(bool, i64)>) -> i64 {
    if bits == 0 {
        return -22; // -EINVAL: a wait no wake could ever match
    }
    let elapsed = match deadline {
        Some((realtime, end)) => match clock_ns(realtime) {
            Ok(now) => now >= end,
            Err(e) => return e,
        },
        None => false,
    };
    let flags = irq_save();

    let mut buf = [0u8; 4];
    if uaccess::copy_from_user(&mut buf, uaddr).is_err() {
        unsafe { irq_restore(flags) };
        return uaccess::EFAULT;
    }
    if u32::from_le_bytes(buf) != val {
        // Somebody changed it before we got here. That is not a failure: it
        // is the whole reason the value is passed in.
        unsafe { irq_restore(flags) };
        return -11; // -EAGAIN
    }

    if elapsed {
        unsafe { irq_restore(flags) };
        return -110;
    }
    if deadline.is_some() && unsafe { (*(&raw const TIMER_TASK)).is_none() } {
        let task = sched::spawn("futex-time", timer_thread, 0);
        unsafe { TIMER_TASK = Some(task) };
    }
    let me = sched::current_id();
    let slot = unsafe {
        let w = &mut *(&raw mut WAITERS);
        let Some(slot) = w.iter().position(|s| !s.live) else {
            irq_restore(flags);
            return -12; // -ENOMEM: nk is out of waiters, and says so
        };
        NEXT_TOKEN = NEXT_TOKEN.checked_add(1).expect("futex wait token exhausted");
        w[slot] = Waiter {
            space: space(),
            addr: uaddr,
            task: me,
            bits,
            live: true,
            woken: false,
            deadline,
            expired: false,
            uid: sched::current_uid(),
            token: NEXT_TOKEN,
        };
        slot
    };

    // Not blocked if a wake already happened: `woken` is set with interrupts
    // masked, so this test cannot miss one.
    if !unsafe { (*(&raw const WAITERS))[slot].woken } {
        sched::block_on(flags, 0);
    } else {
        unsafe { irq_restore(flags) };
    }

    let flags = irq_save();
    let result = unsafe {
        let s = &mut (*(&raw mut WAITERS))[slot];
        s.live = false;
        s.deadline = None;
        if s.woken { 0 } else if s.expired { -110 } else { -4 }
    };
    unsafe { irq_restore(flags) };
    result
}

/// Wake at most `n` waiters on `uaddr` whose bits overlap `bits`.
fn wake(uaddr: u64, n: u32, bits: u32) -> i64 {
    if bits == 0 { return -22; }
    let flags = irq_save();
    let here = space();
    let mut woken = 0;
    unsafe {
        let w = &mut *(&raw mut WAITERS);
        for s in w.iter_mut() {
            if woken >= n {
                break;
            }
            if s.live && !s.woken && !s.expired && sched::uid(s.task) == s.uid && s.space == here && s.addr == uaddr && s.bits & bits != 0 {
                s.woken = true;
                s.deadline = None;
                sched::wake(s.task);
                woken += 1;
            }
        }
        irq_restore(flags);
    }
    woken as i64
}

/// Every waiter still asleep. Printed by the watchdog, which runs only once
/// something has stopped making progress, so it costs nothing on the path it
/// is diagnosing -- which is the point: a futex bug is a hang, and a hang is
/// exactly the state in which no other diagnostic is running.
pub fn report() {
    let flags = irq_save();
    unsafe {
        for (i, s) in (*(&raw const WAITERS)).iter().enumerate() {
            if s.live {
                crate::println!(
                    // Not the value at the address: this runs on the
                    // watchdog, whose address space is the kernel's, so
                    // reading a user address here fails and says nothing.
                    "          futex[{}] task {} on {:#x} space {:#x}{}",
                    i,
                    s.task,
                    s.addr,
                    s.space,
                    if s.woken { " (woken)" } else { "" }
                );
            }
        }
        irq_restore(flags);
    }
}

/// Forget a task's waiters. A thread killed while waiting would otherwise
/// leave a slot claiming it, and a later wake would make a dead task ready.
pub fn forget(task: usize) {
    let flags = irq_save();
    unsafe {
        for s in (*(&raw mut WAITERS)).iter_mut() {
            if s.live && s.task == task {
                s.live = false;
            }
        }
        irq_restore(flags);
    }
}

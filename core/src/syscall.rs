//! Forwarding a system call to Linux.
//!
//! There is nothing here to keep up to date, and that is the point.
//!
//! This file used to hold a table describing every system call nk forwarded:
//! which arguments were pointers, which direction the data travelled, and how
//! long it was. It had to, because `arch/lkl` selects `UACCESS_MEMCPY` --
//! Linux assumed kernel and user shared one flat address space, so its
//! `copy_from_user` was a `memcpy` and handing it an EL0 pointer would have
//! bypassed every protection nk has. So nk copied each argument across
//! itself, which meant knowing what each argument *was*, which meant a list of
//! every system call a program might ever make.
//!
//! Linux already knows. It marks user pointers `__user` and reaches them
//! through `copy_from_user`, on every architecture, for every one of the four
//! hundred and fifty calls. `ldk lkl` patches `arch/lkl` to ask nk for that
//! rather than assume it away (see `useraccess.rs`), and the table stopped
//! having anything to say.
//!
//! What is left is the flag that tells `useraccess` these pointers came from
//! EL0 and must be treated as such.
//!
//! # The numbers are LKL's, not the host's
//!
//! `ldk lkl` builds Linux with `ARCH=lkl` on *both* hosts -- only the
//! compiler differs -- so LKL's system call numbers come from
//! `arch/lkl/include/uapi/asm/unistd.h`: the asm-generic table, plus a few
//! `__NR_arch_specific_syscall + n` additions of nk's own.
//!
//! On aarch64 that table *is* the host's table, so a number taken from a user
//! program's frame and handed to LKL unchanged is correct, and everything
//! below assumes it.
//!
//! On x86-64 it is not. An x86-64 binary asking for `write` passes 1, and 1
//! in the asm-generic table is `io_destroy`. Nothing would report an error --
//! the call would succeed, against the wrong service. So x86-64 user space
//! cannot use this path until the number is translated from the host's table
//! into LKL's; `syscall_nr.rs` now carries both, verified against the kernel
//! sources, for exactly that.
//!
//! Calls nk makes *itself* through `lkl::syscall` are unaffected: those are
//! written with LKL's numbers directly and are correct on either host.

use crate::lkl;
use crate::sched;
use crate::syscall_nr::x86_to_lkl::translate as x86_translate;

/// Hand a system call to Linux with its arguments untouched.
///
/// `Option` for the caller's sake rather than nk's: it used to mean "no
/// descriptor", and now it means only that a build without Linux has nothing
/// to forward to.
pub fn forward(nr: u64, args: &[u64; 6]) -> Option<i64> {
    count_entry(nr);
    sched::set_last_syscall(nr);
    let Some(lkl_nr) = x86_translate(nr) else {
        count_exit(nr);
        return Some(-38);
    };
    let mut args_copy = *args;
    // On a machine without an entropy device (virtio-rng), Linux's random pool
    // is never initialized (crng_ready() remains false). Plain getrandom() with
    // flags == 0 blocks indefinitely on crng_init_wait. Supply GRND_INSECURE (0x4)
    // when unseeded flags == 0 so Linux returns unseeded pseudo-random bytes immediately.
    if lkl_nr == 278 && args_copy[2] == 0 {
        args_copy[2] = 4; // GRND_INSECURE
    }
    let was = sched::set_user_syscall(true);
    let ret = lkl::syscall(lkl_nr as i64, args_copy.map(|a| a as i64));
    sched::set_user_syscall(was);
    count_exit(nr);
    Some(ret)
}

/// Entries and exits per syscall number, saturated. The wedge leaves the
/// process task READY-spinning; if it spins through here some number runs
/// away, and entry-minus-exit names a call that entered Linux and never came
/// back. Printed by the watchdog.
static mut ENTRIES: [u64; 512] = [0; 512];
static mut EXITS: [u64; 512] = [0; 512];

/// Total forwarded syscall entries, monotonic. The watchdog's liveness
/// signal: the clock says nothing about whether the machine is alive, but a
/// system making no syscalls at all -- not even a timer tick's worth of
/// gettimeofday -- is stopped, wedged or finished, whatever the time is.
pub fn activity() -> u64 {
    unsafe { (&*(&raw const ENTRIES)).iter().sum() }
}

#[inline]
fn count_entry(nr: u64) {
    let i = (nr as usize).min(511);
    unsafe {
        let c = &raw mut ENTRIES;
        (*c)[i] = (*c)[i].wrapping_add(1);
    }
}

#[inline]
fn count_exit(nr: u64) {
    let i = (nr as usize).min(511);
    unsafe {
        let c = &raw mut EXITS;
        (*c)[i] = (*c)[i].wrapping_add(1);
    }
}

/// The busiest forwarded calls and any entered-but-not-returned one.
pub fn report() {
    unsafe {
        let entries = &*(&raw const ENTRIES);
        let exits = &*(&raw const EXITS);
        // Ten busiest, by entries. A spinning forwarder shows up here as a
        // count in the millions advancing between watchdog rounds.
        for _ in 0..10 {
            let mut best = 0usize;
            let mut best_n = 0u64;
            for (nr, n) in entries.iter().enumerate() {
                if *n > best_n && !REPORTED[nr] {
                    best_n = *n;
                    best = nr;
                }
            }
            if best_n == 0 {
                break;
            }
            REPORTED[best] = true;
            crate::println!(
                "          syscall {:<4} entries {:<10} exits {}",
                best,
                best_n,
                exits[best]
            );
        }
        for nr in 0..512 {
            REPORTED[nr] = false;
        }
        for nr in 0..512 {
            if entries[nr] != exits[nr] {
                crate::println!(
                    "          syscall {:<4} IN FLIGHT (entries {} exits {})",
                    nr, entries[nr], exits[nr]
                );
            }
        }
    }
}

static mut REPORTED: [bool; 512] = [false; 512];

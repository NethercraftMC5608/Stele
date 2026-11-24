//! Architecture-specific system call numbers.
//!
//! Linux uses different syscall numbers on aarch64 and x86-64. On aarch64
//! they come from asm-generic (the standard since 2012); on x86-64 they are
//! historical and match 32-bit x86 wherever possible.
//!
//! nk intercepts memory management (mmap, munmap, brk, mremap, mprotect),
//! signals, and thread lifecycle calls; everything else is forwarded to
//! LKL. LKL itself expects the host architecture's numbers, so the forwarded
//! calls need no translation at all -- only the intercepted list differs.
//!
//! Both tables below were checked entry by entry against the kernel's own
//! sources -- `arch/x86/entry/syscalls/syscall_64.tbl` for x86-64 and
//! `include/uapi/asm-generic/unistd.h` for aarch64 -- rather than transcribed
//! from memory. All 72 agree. The two are close enough to look
//! interchangeable and are not: `writev` is 66 on aarch64 and 20 on x86-64,
//! `clone` 220 against 56, `mmap` 222 against 9. A single wrong number here
//! is a syscall silently doing something else.

#[cfg(target_arch = "aarch64")]
pub mod nr {
    pub const READ: u64 = 63;
    pub const WRITE: u64 = 64;
    pub const WRITEV: u64 = 66;
    pub const EXIT: u64 = 93;
    pub const EXIT_GROUP: u64 = 94;
    pub const WAITID: u64 = 95;
    pub const SET_TID_ADDRESS: u64 = 96;
    pub const FUTEX: u64 = 98;
    pub const SET_ROBUST_LIST: u64 = 99;
    pub const NANOSLEEP: u64 = 101;
    pub const CLOCK_GETTIME: u64 = 113;
    pub const SCHED_YIELD: u64 = 124;
    pub const KILL: u64 = 129;
    pub const TKILL: u64 = 130;
    pub const TGKILL: u64 = 131;
    pub const SIGALTSTACK: u64 = 132;
    pub const SIGSUSPEND: u64 = 133;
    pub const RT_SIGACTION: u64 = 134;
    pub const RT_SIGPROCMASK: u64 = 135;
    pub const SIGPENDING: u64 = 136;
    pub const SIGTIMEDWAIT: u64 = 137;
    pub const RT_SIGRETURN: u64 = 139;
    pub const PRCTL: u64 = 167;
    pub const BRK: u64 = 214;
    pub const MUNMAP: u64 = 215;
    pub const MREMAP: u64 = 216;
    pub const CLONE: u64 = 220;
    pub const MMAP: u64 = 222;
    pub const MPROTECT: u64 = 226;
    pub const MSYNC: u64 = 227;
    pub const MADVISE: u64 = 233;
    pub const WAIT4: u64 = 260;
    pub const PRLIMIT64: u64 = 261;
    pub const RSEQ: u64 = 293;
    pub const PIDFD_OPEN: u64 = 434;
    pub const CLONE3: u64 = 435;
}

#[cfg(target_arch = "x86_64")]
pub mod nr {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const OPEN: u64 = 2;
    pub const CLOSE: u64 = 3;
    pub const STAT: u64 = 4;
    pub const FSTAT: u64 = 5;
    pub const LSTAT: u64 = 6;
    pub const POLL: u64 = 7;
    pub const LSEEK: u64 = 8;
    pub const WRITEV: u64 = 20;
    pub const ACCESS: u64 = 21;
    pub const MMAP: u64 = 9;
    pub const MPROTECT: u64 = 10;
    pub const MUNMAP: u64 = 11;
    pub const BRK: u64 = 12;
    pub const RT_SIGACTION: u64 = 13;
    pub const RT_SIGPROCMASK: u64 = 14;
    pub const RT_SIGRETURN: u64 = 15;
    pub const IOCTL: u64 = 16;
    pub const PREAD64: u64 = 17;
    pub const PWRITE64: u64 = 18;
    pub const READV: u64 = 19;
    pub const PIPE: u64 = 22;
    pub const SCHED_YIELD: u64 = 24;
    pub const MREMAP: u64 = 25;
    pub const MSYNC: u64 = 26;
    pub const MADVISE: u64 = 28;
    pub const DUP: u64 = 32;
    pub const DUP2: u64 = 33;
    pub const NANOSLEEP: u64 = 35;
    pub const GETPID: u64 = 39;
    pub const CLONE: u64 = 56;
    pub const EXIT: u64 = 60;
    pub const WAIT4: u64 = 61;
    pub const KILL: u64 = 62;
    pub const UNAME: u64 = 63;
    pub const FCNTL: u64 = 72;
    pub const FSYNC: u64 = 74;
    pub const FDATASYNC: u64 = 75;
    pub const TRUNCATE: u64 = 76;
    pub const FTRUNCATE: u64 = 77;
    pub const GETCWD: u64 = 79;
    pub const CHDIR: u64 = 80;
    pub const FCHDIR: u64 = 81;
    pub const RENAME: u64 = 82;
    pub const MKDIR: u64 = 83;
    pub const RMDIR: u64 = 84;
    pub const UNLINK: u64 = 87;
    pub const READLINK: u64 = 89;
    pub const GETPPID: u64 = 110;
    pub const GETUID: u64 = 102;
    pub const GETGID: u64 = 104;
    pub const GETEUID: u64 = 107;
    pub const GETEGID: u64 = 108;
    pub const SYSINFO: u64 = 99;
    pub const GETTID: u64 = 186;
    pub const GETDENTS64: u64 = 217;
    pub const OPENAT: u64 = 257;
    pub const NEWFSTATAT: u64 = 262;
    pub const STATX: u64 = 332;
    pub const GETRANDOM: u64 = 318;
    pub const SIGALTSTACK: u64 = 131;
    pub const SIGSUSPEND: u64 = 130;
    pub const SIGPENDING: u64 = 127;
    pub const SIGTIMEDWAIT: u64 = 128;
    pub const PRCTL: u64 = 157;
    pub const TKILL: u64 = 200;
    pub const FUTEX: u64 = 202;
    pub const SET_TID_ADDRESS: u64 = 218;
    pub const CLOCK_GETTIME: u64 = 228;
    pub const EXIT_GROUP: u64 = 231;
    pub const TGKILL: u64 = 234;
    pub const WAITID: u64 = 247;
    pub const SET_ROBUST_LIST: u64 = 273;
    pub const PRLIMIT64: u64 = 302;
    pub const RSEQ: u64 = 334;
    pub const PIDFD_OPEN: u64 = 434;
    pub const CLONE3: u64 = 435;
}

/// x86-64 -> LKL (asm-generic) syscall number translation.
///
/// LKL builds with ARCH=lkl, which uses the asm-generic table (same as
/// aarch64). x86-64 userspace passes x86-64 host numbers. Translating here
/// rather than at the callsite keeps the forwarding path clean and the
/// translation auditable.
#[cfg(target_arch = "x86_64")]
pub mod x86_to_lkl {
    use crate::syscall_nr::nr as x86;

    /// Translate an x86-64 host syscall number to LKL's asm-generic number.
    ///
    pub fn translate(nr: u64) -> Option<u64> {
        Some(match nr {
            // The 72 numbers we track, translated to their asm-generic values.
            x86::READ => 63,
            x86::WRITE => 64,
            // x86::POLL has no asm-generic equivalent: 73 is `ppoll`, whose
            // third argument is a `timespec *`, not a millisecond count.
            // `sys_poll` converts and forwards; translating the number alone
            // would pass poll's `0` as a null timeout and block for ever.
            x86::READV => 65,
            x86::WRITEV => 66,
            x86::PREAD64 => 67,
            x86::PWRITE64 => 68,
            x86::CLOSE => 57,
            x86::LSEEK => 62,
            x86::IOCTL => 29,
            x86::FCNTL => 25,
            x86::DUP => 23,
            x86::FSYNC => 82,
            x86::FDATASYNC => 83,
            x86::FTRUNCATE => 46,
            x86::TRUNCATE => 45,
            x86::GETCWD => 17,
            x86::CHDIR => 49,
            x86::FCHDIR => 50,
            x86::GETDENTS64 => 61,
            x86::OPENAT => 56,
            x86::NEWFSTATAT => 79,
            x86::FSTAT => 80,
            x86::STATX => 291,
            x86::GETRANDOM => 278,
            x86::UNAME => 160,
            x86::SYSINFO => 179,
            x86::GETPID => 172,
            x86::GETPPID => 173,
            x86::GETUID => 174,
            x86::GETEUID => 175,
            x86::GETGID => 176,
            x86::GETEGID => 177,
            x86::GETTID => 178,
            // The legacy x86-64 file calls have no asm-generic equivalent at
            // all: asm-generic dropped every path-taking syscall in favour of
            // its `*at` form. `open`/`stat`/`unlink`/`mkdir` and the rest are
            // rewritten into `openat`/`newfstatat`/`unlinkat`/`mkdirat` with
            // AT_FDCWD by `x86_legacy` in user.rs, which is where the extra
            // argument can be inserted. Listing them here as well would send
            // them to whichever asm-generic call happens to share the number.
            x86::EXIT => 93,
            x86::EXIT_GROUP => 94,
            x86::WAITID => 95,
            x86::SET_TID_ADDRESS => 96,
            x86::FUTEX => 98,
            x86::SET_ROBUST_LIST => 99,
            x86::NANOSLEEP => 101,
            x86::CLOCK_GETTIME => 113,
            x86::SCHED_YIELD => 124,
            x86::KILL => 129,
            x86::TKILL => 130,
            x86::TGKILL => 131,
            x86::SIGALTSTACK => 132,
            x86::SIGSUSPEND => 133,
            x86::RT_SIGACTION => 134,
            x86::RT_SIGPROCMASK => 135,
            x86::SIGPENDING => 136,
            x86::SIGTIMEDWAIT => 137,
            x86::RT_SIGRETURN => 139,
            x86::PRCTL => 167,
            x86::BRK => 214,
            x86::MUNMAP => 215,
            x86::MREMAP => 216,
            x86::CLONE => 220,
            x86::MMAP => 222,
            x86::MPROTECT => 226,
            x86::MSYNC => 227,
            x86::MADVISE => 233,
            x86::WAIT4 => 260,
            x86::PRLIMIT64 => 261,
            x86::RSEQ => 293,
            x86::PIDFD_OPEN => 434,
            x86::CLONE3 => 435,
            // Everything else that exists under the same name in both
            // tables, generated from Linux's own; see syscall_x86_generic.rs.
            _ => return crate::syscall_x86_generic::lookup(nr),
        })
    }
}

/// On aarch64 the translation is identity (host = asm-generic).
#[cfg(target_arch = "aarch64")]
pub mod x86_to_lkl {
    pub fn translate(nr: u64) -> Option<u64> {
        Some(nr)
    }
}

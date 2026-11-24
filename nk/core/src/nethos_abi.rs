//! The NETHOS system call interface.
//!
//! This is the interface NETHOS's own programs are written against, and it
//! exists because the desktop should not be permanently shaped by a kernel
//! interface designed for a different operating system. Linux's ABI stays --
//! it is what makes a Debian archive's worth of software run here, and
//! nothing about this replaces it -- but a program written *for* NETHOS
//! should not have to spell its intentions in terms of `openat`, `AT_FDCWD`
//! and a negative errno.
//!
//! # What is different, and why
//!
//! **Results, not errno.** Every call returns a `Result` encoded in one
//! register: a value `>= 0` succeeded, a value `< 0` is `-(error code)`.
//! That much is Linux's convention and there is no reason to change it --
//! it is one comparison and it costs nothing. What changes is that the
//! error codes are NETHOS's own small set, which means a program can
//! exhaustively match them, and a new one is a visible addition rather than
//! a number a caller has never heard of.
//!
//! **Handles, not descriptor numbers.** Linux's "lowest free integer" rule
//! is an accident of 1970s implementation that every program since has had
//! to work around -- it is why `dup2` exists, why a library closing a file
//! can break an unrelated part of the program, and why passing a descriptor
//! between components is unsafe without knowing who else holds it. A NETHOS
//! handle is an opaque, non-reused value; closing one twice is an error
//! rather than a close of whatever took the number.
//!
//! **Calls that say what they mean.** `nh_open` takes a path and a mode,
//! not a directory descriptor, a flags word with fifteen meanings and a
//! mode that is ignored unless one of those flags is set.
//!
//! # What is the same
//!
//! The entry path. A NETHOS program makes a system call with the same
//! instruction a Linux one does -- `svc #0` on aarch64, `syscall` on x86-64
//! -- and with the same registers. What differs is only which table the
//! number is read against, and that is decided per process by the ELF's
//! `EI_OSABI` byte. There is no second entry vector to keep in step, no new
//! assembly on either architecture, and a native program pays nothing for
//! the compatibility path existing.
//!
//! Underneath, these are implemented on the Linux that nk hosts. That is an
//! implementation detail and deliberately not visible in the interface: the
//! point of the ABI is that the interface NETHOS programs see is NETHOS's
//! to define, whatever is currently answering it.

use crate::elf::Abi;

/// NETHOS system call numbers.
///
/// Deliberately starting at 1 and dense: the table is small, it is meant to
/// stay small, and a number that does not exist is a bug in the caller
/// rather than a hole to be filled later with something unrelated.
pub mod nr {
    /// `nh_exit(status)` -- never returns.
    pub const EXIT: u64 = 1;
    /// `nh_write(handle, ptr, len) -> bytes written`
    pub const WRITE: u64 = 2;
    /// `nh_read(handle, ptr, len) -> bytes read`
    pub const READ: u64 = 3;
    /// `nh_open(path, len, mode) -> handle`
    pub const OPEN: u64 = 4;
    /// `nh_close(handle)`
    pub const CLOSE: u64 = 5;
    /// `nh_alloc(len) -> address`, page-granular anonymous memory.
    pub const ALLOC: u64 = 6;
    /// `nh_free(address, len)`
    pub const FREE: u64 = 7;
    /// `nh_time() -> nanoseconds since boot`
    pub const TIME: u64 = 8;
    /// `nh_yield()`
    pub const YIELD: u64 = 9;
    /// `nh_self() -> process id`
    pub const SELF: u64 = 10;
    /// `nh_abi() -> ABI version`. The one call whose answer is a promise:
    /// a program can ask what it is running on before using anything else.
    pub const ABI: u64 = 11;
}

/// The version this kernel implements. A native program may read it with
/// `nh_abi()` and refuse to run on a kernel older than it needs.
pub const ABI_VERSION: u64 = 1;

/// NETHOS error codes, returned negated.
///
/// Small, closed, and not Linux's: a caller can match all of them. They are
/// numbered from 1 so that `-1` is the first error and `0` is unambiguously
/// success.
pub mod err {
    /// The call number is not one this kernel implements.
    pub const NOSYS: i64 = -1;
    /// An argument was not valid -- a length that cannot be, a mode that is
    /// not a mode, a pointer that is not aligned.
    pub const INVALID: i64 = -2;
    /// A handle that names nothing, or names something already closed.
    pub const BADHANDLE: i64 = -3;
    /// The named thing does not exist.
    pub const NOTFOUND: i64 = -4;
    /// Out of memory.
    pub const NOMEMORY: i64 = -5;
    /// A pointer the process may not read or write.
    pub const FAULT: i64 = -6;
    /// The operation is not permitted for this process.
    pub const DENIED: i64 = -7;
    /// The underlying system reported something with no NETHOS equivalent.
    /// Deliberately last and deliberately vague: it is the honest answer
    /// when the truth is a Linux errno this interface does not model, and
    /// it is a list of things still to be given names.
    pub const FAILED: i64 = -8;
}

/// How a path is given to `nh_open`.
///
/// A pointer and a length, not a NUL-terminated string. A length makes the
/// bound explicit, lets the kernel refuse an unreasonable one before
/// reading anything, and means a program need not scan its own memory for a
/// terminator before asking -- which is the operation every `strlen` in
/// every libc's `open` wrapper exists to do.
const MAX_PATH: u64 = 4096;

/// Open modes. One value, not a flags word: the combinations that are
/// meaningful are few, and naming them is clearer than fifteen bits of
/// which several are mutually exclusive.
pub mod mode {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const READ_WRITE: u64 = 2;
    /// Create if absent, truncate if present.
    pub const CREATE: u64 = 3;
    /// Create if absent, append if present.
    pub const APPEND: u64 = 4;
}

/// Translate a Linux errno into the nearest NETHOS error.
///
/// Lossy on purpose. The interface promises a small closed set, so an errno
/// with no equivalent becomes `FAILED` rather than leaking a number from an
/// interface this one exists to not be. The mappings here are the ones a
/// program can act on differently.
fn from_errno(rc: i64) -> i64 {
    if rc >= 0 {
        return rc;
    }
    match -rc {
        2 => err::NOTFOUND,   // ENOENT
        9 => err::BADHANDLE,  // EBADF
        12 => err::NOMEMORY,  // ENOMEM
        13 => err::DENIED,    // EACCES
        14 => err::FAULT,     // EFAULT
        22 => err::INVALID,   // EINVAL
        38 => err::NOSYS,     // ENOSYS
        _ => err::FAILED,
    }
}

/// Does this task speak the NETHOS ABI?
pub fn is_native(abi: Abi) -> bool {
    abi == Abi::Nethos
}

/// Dispatch one NETHOS system call.
///
/// `args` are the six argument registers, in the same places a Linux call
/// would put them -- the entry path is shared, so the frame arrives in the
/// same shape. The return is the value to place in the result register.
#[cfg(nk_lkl)]
pub fn dispatch(nr_call: u64, args: &[u64; 6]) -> i64 {
    match nr_call {
        nr::ABI => ABI_VERSION as i64,
        nr::EXIT => {
            crate::user::sys_exit(args[0] as i32);
        }
        nr::YIELD => {
            crate::sched::yield_now();
            0
        }
        nr::SELF => crate::sched::linux_pid(crate::sched::current_id()),
        nr::TIME => (crate::timer::ticks() * (1_000_000_000 / crate::timer::HZ)) as i64,
        nr::WRITE => from_errno(linux(64, [args[0], args[1], args[2], 0, 0, 0])),
        nr::READ => from_errno(linux(63, [args[0], args[1], args[2], 0, 0, 0])),
        nr::CLOSE => from_errno(linux(57, [args[0], 0, 0, 0, 0, 0])),
        nr::OPEN => open(args[0], args[1], args[2]),
        nr::ALLOC => alloc(args[0]),
        nr::FREE => from_errno(crate::user::nethos_free(args[0], args[1])),
        _ => err::NOSYS,
    }
}

#[cfg(not(nk_lkl))]
pub fn dispatch(_nr_call: u64, _args: &[u64; 6]) -> i64 {
    err::NOSYS
}

/// Make a Linux call underneath, by Linux's own asm-generic number.
///
/// Not `syscall::forward`: that takes a *host* number and translates it,
/// and these are already the numbers Linux wants. The distinction matters
/// on x86-64, where forwarding asm-generic 64 would be read as x86's 64
/// (`uname`) instead of `write`.
///
/// `user` says whose pointers these are, and it is not a detail. Linux asks
/// nk to copy for it through the same three functions either way, and nk
/// decides how to translate an address from this flag: a user pointer is
/// checked against the process's tables and refused if it names kernel
/// memory, a kernel pointer is dereferenced directly. Passing a buffer nk
/// built itself -- `nh_open`'s NUL-terminated path -- with `user` set means
/// Linux tries to translate a kernel address as a user one and the call
/// fails with EFAULT, which is what `nh_open` did until this was split.
#[cfg(nk_lkl)]
fn linux_as(user: bool, lkl_nr: u64, args: [u64; 6]) -> i64 {
    let was = crate::sched::set_user_syscall(user);
    let rc = crate::lkl::syscall(lkl_nr as i64, args.map(|v| v as i64));
    crate::sched::set_user_syscall(was);
    rc
}

/// The common case: the pointers in `args` came from the program.
#[cfg(nk_lkl)]
fn linux(lkl_nr: u64, args: [u64; 6]) -> i64 {
    linux_as(true, lkl_nr, args)
}

/// `nh_open(path, len, mode) -> handle`
///
/// The path is copied into the kernel and NUL-terminated here, because
/// Linux's `openat` wants a C string and the NETHOS interface does not have
/// one. Doing it in the kernel rather than asking the program to is the
/// point: it is where the bound can actually be enforced.
#[cfg(nk_lkl)]
fn open(path: u64, len: u64, mode: u64) -> i64 {
    if len == 0 || len > MAX_PATH {
        return err::INVALID;
    }
    let mut buf = alloc::vec![0u8; len as usize + 1];
    if crate::uaccess::copy_from_user(&mut buf[..len as usize], path).is_err() {
        return err::FAULT;
    }
    // A path with an embedded NUL is not a path. Refusing it here stops the
    // classic truncation trick, where a program passes "safe\0../../etc" and
    // the kernel opens something other than what it was shown.
    if buf[..len as usize].contains(&0) {
        return err::INVALID;
    }

    // O_RDONLY 0, O_WRONLY 1, O_RDWR 2, O_CREAT 0o100, O_TRUNC 0o1000,
    // O_APPEND 0o2000 -- Linux's bits, assembled here so the NETHOS
    // interface never has to mention them.
    let flags: u64 = match mode {
        mode::READ => 0,
        mode::WRITE => 1,
        mode::READ_WRITE => 2,
        mode::CREATE => 1 | 0o100 | 0o1000,
        mode::APPEND => 1 | 0o100 | 0o2000,
        _ => return err::INVALID,
    };
    // The path is nk's own buffer, so Linux must read it as a kernel
    // pointer -- see `linux_as`.
    const CWD: u64 = (-100i64) as u64;
    from_errno(linux_as(false, 56, [CWD, buf.as_ptr() as u64, flags, 0o644, 0, 0]))
}

/// `nh_alloc(len) -> address`
///
/// Anonymous, readable and writable, page-granular. No flags, no hint, no
/// file: a program that wants memory says how much, and a program that
/// wants something more specific is asking for something this call is
/// deliberately not.
#[cfg(nk_lkl)]
fn alloc(len: u64) -> i64 {
    if len == 0 {
        return err::INVALID;
    }
    // PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS.
    from_errno(crate::user::nethos_alloc(len, 3, 0x22))
}

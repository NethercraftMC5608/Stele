//! A native NETHOS program.
//!
//! No libc, because there is not one for this ABI yet -- and that is the
//! point of the test. Everything here goes through the NETHOS system call
//! interface directly, which is what a NETHOS runtime will eventually wrap.
//!
//! Built freestanding and marked `EI_OSABI = 0x4E` after linking, which is
//! what tells nk to answer its calls from the NETHOS table rather than the
//! Linux one. The same binary with that byte left at 0 would be dispatched
//! as Linux and would make no sense at all -- call 2 would be `open` instead
//! of `nh_write` -- which is exactly the confusion the marker prevents.
#![no_std]
#![no_main]

// The NETHOS ABI, as a program sees it.
const NH_EXIT: u64 = 1;
const NH_WRITE: u64 = 2;
const NH_OPEN: u64 = 4;
const NH_CLOSE: u64 = 5;
const NH_ALLOC: u64 = 6;
const NH_FREE: u64 = 7;
const NH_TIME: u64 = 8;
const NH_SELF: u64 = 10;
const NH_ABI: u64 = 11;

const MODE_CREATE: u64 = 3;

/// One system call. Same instruction and registers as a Linux call: only
/// the table the number is read against differs.
#[inline(always)]
unsafe fn syscall3(n: u64, a: u64, b: u64, c: u64) -> i64 {
    let ret: i64;
    #[cfg(target_arch = "x86_64")]
    core::arch::asm!(
        "syscall",
        inlateout("rax") n => ret,
        in("rdi") a, in("rsi") b, in("rdx") c,
        lateout("rcx") _, lateout("r11") _,
        options(nostack)
    );
    #[cfg(target_arch = "aarch64")]
    core::arch::asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a => ret,
        in("x1") b, in("x2") c,
        options(nostack)
    );
    ret
}

unsafe fn nh_write(handle: u64, s: &[u8]) -> i64 {
    syscall3(NH_WRITE, handle, s.as_ptr() as u64, s.len() as u64)
}

fn puts(s: &[u8]) {
    unsafe { nh_write(1, s) };
}

/// Print a number without a formatter, because there is no allocator and no
/// `core::fmt` machinery worth pulling in for this.
fn putn(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if v == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    puts(&buf[i..]);
}

/// The entry point, in assembly, for one reason: alignment.
///
/// The ABI says `rsp` is 16-byte aligned when a process starts. A compiled
/// function assumes the opposite -- that a `call` has just pushed a return
/// address, leaving `rsp % 16 == 8` -- and lays out its locals accordingly.
/// `_start` was never called, so a Rust `extern "C" fn` used as the entry
/// point has every SSE spill misaligned by 8, and the first `movaps` to a
/// local raises #GP. That is a real fault in a correct kernel, and it is
/// what this binary did at `0x4012ff` before the stack was fixed here.
///
/// Subtracting 8 before the call restores what the callee expects.
#[unsafe(no_mangle)]
#[unsafe(naked)]
pub unsafe extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        // Force 16-byte alignment, then `call` -- which pushes 8 and leaves
        // `rsp % 16 == 8`, which is precisely what a compiled function
        // entered by a call expects to find. Aligning here rather than
        // trusting the entry value means this is correct whatever the
        // kernel handed over.
        "and rsp, -16",
        "call {main}",
        main = sym nethos_main,
    )
}

extern "C" fn nethos_main() -> ! {
    puts(b"nethos-hello: native NETHOS binary running\n");

    // The one call whose answer is a promise: what interface is this?
    let version = unsafe { syscall3(NH_ABI, 0, 0, 0) };
    puts(b"nethos-hello: abi version ");
    putn(version as u64);
    puts(b"\n");

    let pid = unsafe { syscall3(NH_SELF, 0, 0, 0) };
    puts(b"nethos-hello: pid ");
    putn(pid as u64);
    puts(b"\n");

    let t = unsafe { syscall3(NH_TIME, 0, 0, 0) };
    puts(b"nethos-hello: uptime ns ");
    putn(t as u64);
    puts(b"\n");

    // Memory: no flags, no hint, no file. Just how much.
    let mem = unsafe { syscall3(NH_ALLOC, 8192, 0, 0) };
    if mem < 0 {
        puts(b"nethos-hello: nh_alloc failed\n");
    } else {
        // Touch it, to prove it is really there and really writable.
        unsafe {
            let p = mem as *mut u64;
            for i in 0..1024 {
                p.add(i).write_volatile(i as u64 * 3);
            }
            let mut sum = 0u64;
            for i in 0..1024 {
                sum += p.add(i).read_volatile();
            }
            puts(b"nethos-hello: alloc ok, checksum ");
            putn(sum);
            puts(b"\n");
        }
        unsafe { syscall3(NH_FREE, mem as u64, 8192, 0) };
    }

    // A file, by path and length -- no directory descriptor, no flags word.
    let path = b"/tmp/nethos-native-wrote-this";
    let h = unsafe {
        syscall3(NH_OPEN, path.as_ptr() as u64, path.len() as u64, MODE_CREATE)
    };
    if h < 0 {
        puts(b"nethos-hello: nh_open failed\n");
    } else {
        let wrote = unsafe { nh_write(h as u64, b"written through the NETHOS ABI\n") };
        puts(b"nethos-hello: wrote ");
        putn(wrote as u64);
        puts(b" bytes to a file\n");
        unsafe { syscall3(NH_CLOSE, h as u64, 0, 0) };
    }

    puts(b"nethos-hello: done\n");
    unsafe { syscall3(NH_EXIT, 0, 0, 0) };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    puts(b"nethos-hello: panic\n");
    unsafe { syscall3(NH_EXIT, 1, 0, 0) };
    loop {}
}

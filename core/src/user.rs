//! User space: an address space, a program in it, and the syscalls it makes.
//!
//! This is the gate. Everything nk has done so far -- the drivers, the shim,
//! the block and network stacks -- lives entirely inside the kernel and is
//! reachable only by nk's own code calling it. A desktop is not that. It is
//! several hundred existing binaries, compiled years ago against Linux's
//! syscall ABI, that will never be recompiled. Nothing above the driver layer
//! is possible until a program can run at EL0 and be answered.
//!
//! So the numbers here are Linux's, not nk's: `write` is 64 and `exit` is 93
//! because that is what aarch64 Linux uses and what those binaries contain.
//! Inventing a cleaner numbering would be inventing a system that nothing can
//! be run on, which is the whole thing being avoided.
//!
//! Each LKL process has a dedicated nk thread and Linux task.
//! Fork, exec replacement and userspace signals are not implemented.
//! LKL builds load an ELF fixture through Linux's rootfs; standalone builds
//! retain the raw smoke-test program. Neither supports a desktop runtime yet.

use crate::frames::{self, PAGE};
use crate::paging;
use alloc::vec::Vec;
use crate::println;

/// Where a process's image goes: 0x400000, which is where aarch64 links a
/// non-PIE executable, so an ordinary binary needs no relocation to run here.
///
/// Getting to this address took narrowing the device map to the 34MB the
/// machine actually has, and then finding that user pages were mapped global
/// in an address space with ASID 0 -- so the kernel's own walking of the low
/// half poisoned the process's translations, under HVF only. See
/// docs/KERNEL.md.
pub const USER_BASE: u64 = 0x0040_0000;
/// The top of the user address space, and so how much room a process has.
///
/// It was 256MB, which was room for anything nk had run. Mesa is not: a
/// single `PT_LOAD` in `libLLVM.so.19.1` is 117MB, and with QEMU's devices
/// still sitting in the low half at 0x08000000..0x0a200000 the largest
/// contiguous run below 256MB was 124MB -- just too small, in the way that
/// produces "failed to map segment from shared object" and nothing else.
///
/// The ceiling is RAM: QEMU's `virt` puts it at 0x4000_0000, which the kernel
/// maps through these same tables. Anything below that and above the devices
/// is the process's to use, so 768MB left 256MB of address space unused for
/// no reason -- and WebKit is the first thing to want it. nethosd alone was
/// measured peaking at ~730MB live against a ~732MB window, which is not a
/// margin, it is a coincidence.
///
/// 0x3F00_0000 is 1008MB, keeping 16MB clear of RAM. That is a ceiling, not
/// a solution: mappings are still populated eagerly, a frame per page whether
/// the page is ever touched or not, so a process cannot map more than the
/// machine has. Demand paging is what actually lifts this, and this constant
/// is what buys the room to find out whether WebKit needs it.
pub const USER_STACK_TOP: u64 = 0x3F00_0000;

/// The page-in trampoline, one page above the stack in every process.
///
/// A file-backed page fault cannot be served from the abort handler: the
/// read goes through LKL, and entering LKL from the synchronous-abort path
/// is not equivalent to entering it from a syscall. The abort handler
/// instead saves the frame, points ELR here and names the page-in syscall in
/// x8; the ordinary SVC path reads the page and restores the frame.
pub const TRAMPOLINE_VA: u64 = USER_STACK_TOP;

static mut TRAMPOLINE_PAGE: usize = 0;

fn trampoline_page() -> usize {
    unsafe {
        if TRAMPOLINE_PAGE == 0 {
            let frame = frames::alloc().expect("no frame for the page-in trampoline");
            core::ptr::write_bytes(frame, 0, PAGE);
            // svc #0 ; brk #0 -- the brk is unreachable when page-in works.
            let insns = [0xD400_0001u32, 0xD420_0000u32];
            core::ptr::copy_nonoverlapping(insns.as_ptr() as *const u8, frame, 8);
            paging::clean_icache_range(frame as u64, PAGE as u64);
            TRAMPOLINE_PAGE = frame as usize;
        }
        TRAMPOLINE_PAGE
    }
}

/// # Safety
/// `root` is a user address space no process is running in yet.
pub unsafe fn map_trampoline(root: u64) {
    paging::map_user(root, TRAMPOLINE_VA, trampoline_page() as u64, PAGE as u64, true);
}

/// Where anonymous mappings start, growing downward.
///
/// Between the heap growing up from the end of the image and this growing
/// down, the two run out of room by meeting -- which nk can detect and refuse
/// -- rather than by one silently landing on the other.
///
/// The 16MB gap below the stack is the main thread's stack guard plus room
/// for the signal trampoline page (0x3FF0_0000); mappings must stay clear of
/// both. The gap is not the budget: the budget is TOP itself. nethosd with
/// six threads peaks at ~730MB of live anonymous reservations against a
/// ~732MB usable window (measured 2026-09-08: green boot's lowest base
/// 0x1782000, red boot's extra 128MB arena refused) -- so this number is
/// the M1 ceiling, not a guess. Raising it toward RAM top (0x4000_0000)
/// is possible but moves the stack too; see USER_STACK_TOP.
pub const USER_MMAP_TOP: u64 = USER_STACK_TOP - 16 * 1024 * 1024;

/// The high arena: where anonymous and file mappings actually come from.
///
/// The low half is a dead end for address space. A process shares it with
/// nk's own identity map -- the devices at 0x0800_0000..0x0a20_0000 and RAM
/// from 0x4000_0000 up -- which leaves under a gigabyte, with a hole through
/// the middle of it. WebKit asks for a 128MB anonymous arena on a window
/// that is already down to 18MB of contiguous room, and there is no
/// arrangement of that gigabyte in which it fits.
///
/// TTBR0 covers 256TB. `new_address_space` copies L0 and then replaces slot
/// 0 with the process's own, so every other top-level slot is a private
/// zero: nothing of nk's is there and nothing another process can see. Slot
/// 1 is 512GiB..1TiB, and a 64GiB arena inside it is more address space than
/// anything nk runs will ask for.
///
/// The image, heap and main stack stay where they are: `USER_BASE` is where
/// a non-PIE aarch64 binary is linked, and moving it would mean relocating
/// every executable nk loads. Only `mmap` moves.
pub const USER_HIGH_BASE: u64 = 0x0000_0080_0000_0000;
/// The top of the high arena. Slot 1 runs to 1TiB, and the whole of it is
/// ours: demand paging moved the pressure from physical frames to address
/// space (every anonymous mapping reserves its range now, whether or not it
/// ever holds a page), and 64GiB was not enough for one WebKit session --
/// nine WebProcess spawn attempts, each reserving a fresh set of JSC cages,
// plus glibc's per-thread arenas, walked `mmap_next` right off the bottom
/// of the arena and into `mmapfail range len 0x8000000` and a glib abort.
pub const USER_HIGH_TOP: u64 = 0x0000_0100_0000_0000;

/// Whether a page-aligned range is somewhere a mapping may live.
///
/// Two disjoint answers, because there are two regions. In the high arena
/// the only question is whether the range is inside it. In the low half the
/// range must clear the heap below and the stack guard above, and must not
/// cross the device window -- those are nk's own mappings, shared by every
/// address space, and replacing one takes the machine with it.
fn range_ok(at: u64, end: u64, brk: u64) -> bool {
    if at & 4095 != 0 || end <= at {
        return false;
    }
    if at >= USER_HIGH_BASE {
        return end <= USER_HIGH_TOP;
    }
    at >= brk && end <= USER_MMAP_TOP && !(at < 0x0a20_0000 && end > 0x0800_0000)
}

/// How much stack a process starts with. One page was enough for a program
/// written in assembly and is nowhere near enough for a libc, which sets up
/// TLS, locale and stdio buffers before it reaches `main`. Mapped up front
/// rather than grown on a fault, because nk has no fault handler that could
/// tell a stack from a wild pointer yet.
pub const USER_STACK_SIZE: usize = 256 * 1024;

/// The bootstrap process entry state and its own translation table.
pub struct Process {
    pub ttbr0: u64,
    pub entry: u64,
    pub stack: u64,
    /// The first address above the loaded image: where the heap starts.
    pub brk: u64,
    /// Static TLS template from PT_TLS, mapped into the address space.
    /// A libc's `_start` reads thread-locals before any syscall, so this
    /// must exist at entry: without it the first `%fs:` access faults.
    pub tls_addr: u64,
    pub tls_filesz: usize,
    pub tls_memsz: usize,
    /// Which system-call interface this program speaks, from `EI_OSABI`.
    /// Copied onto the task by `run`, because the syscall path reads it from
    /// the task and not from here.
    pub abi: crate::elf::Abi,
}

extern "C" {
    static __user_blob_start: u8;
    static __user_blob_end: u8;
    fn enter_user(entry: u64, stack: u64, ttbr0: u64) -> !;
    #[cfg(nk_lkl)]
    fn enter_user_fresh(entry: u64, stack: u64, ttbr0: u64, kernel_sp: u64) -> !;
    #[cfg(nk_lkl)]
    fn resume_user(frame: *const Frame, ttbr0: u64, kernel_sp: u64) -> !;
}

/// Build the standalone smoke-test address space, sharing EL1-only kernel
/// mappings. Exceptions retain this TTBR0; AT S1E0R checks EL0 permissions.
/// User stacks come from their own arena, like task stacks (`sched.rs`):
/// `frames::alloc_contiguous` walks a bump pointer shared with every
/// single-frame allocation, and by the time WebKit is up it is at the end of
/// RAM, so a 256 KiB stack request fails outright even with frames free.
/// The arena is taken once, while the bump still has room; an exec consumes
/// a block from it and nothing ever returns one (the address-space teardown
/// frees stack pages as ordinary frames), so it is sized for a boot's worth
/// of process starts.
const USER_STACK_ARENA_PAGES: usize = 8192;      // 32 MiB, 128 stacks
static mut USER_STACK_ARENA: usize = 0;
static mut USER_STACK_ARENA_NEXT: usize = 0;

fn alloc_user_stack() -> Option<*mut u8> {
    unsafe {
        if USER_STACK_ARENA == 0 {
            USER_STACK_ARENA = frames::alloc_contiguous(USER_STACK_ARENA_PAGES)? as usize;
        }
        if USER_STACK_ARENA_NEXT + USER_STACK_SIZE / PAGE <= USER_STACK_ARENA_PAGES {
            let stack = USER_STACK_ARENA + USER_STACK_ARENA_NEXT * PAGE;
            USER_STACK_ARENA_NEXT += USER_STACK_SIZE / PAGE;
            core::ptr::write_bytes(stack as *mut u8, 0, USER_STACK_SIZE);
            return Some(stack as *mut u8);
        }
        frames::alloc_contiguous(USER_STACK_SIZE / PAGE)
    }
}

pub fn spawn() -> Process {
    let blob_start = &raw const __user_blob_start as usize;
    let blob_end = &raw const __user_blob_end as usize;
    let len = blob_end - blob_start;
    assert!(
        len <= PAGE,
        "the user program outgrew one page: {len} bytes"
    );

    let code = frames::alloc().expect("no memory for the user program");
    let stack = alloc_user_stack()
        .expect("no memory for the user stack");
    unsafe { core::ptr::copy_nonoverlapping(blob_start as *const u8, code, len) };

    unsafe {
        publish_code(code, PAGE);
    }
    let ttbr0 = paging::new_address_space();
    unsafe {
        paging::map_user(ttbr0, USER_BASE, code as u64, PAGE as u64, true);
        paging::map_user(
            ttbr0,
            USER_STACK_TOP - USER_STACK_SIZE as u64,
            stack as u64,
            USER_STACK_SIZE as u64,
            false,
        );
    }

    // The same initial stack a loaded binary gets. The smoke test is the same
    // program, and it checks the layout before it does anything else, so
    // building it only on the Linux path would leave this one entering EL0
    // with a stack pointer one byte past its own page.
    let random = stack_seed().expect("no stack guard");
    let sp = unsafe { crate::stack::Builder::new(stack, USER_STACK_TOP, USER_STACK_SIZE) }
        .build(
            &[b"/nk-init"],
            &[b"PATH=/bin", b"HOME=/"],
            &[(crate::stack::AT_PAGESZ, PAGE as u64), (crate::stack::AT_ENTRY, USER_BASE)],
            &random,
        )
        .expect("the initial stack does not fit in one page");

    println!(
        "  user:   {} bytes of program at {:#x}, stack at {:#x}, ttbr0 {:#x}",
        len, USER_BASE, sp, ttbr0
    );
    Process {
        ttbr0,
        entry: USER_BASE,
        stack: sp,
        brk: USER_BASE + PAGE as u64,
        tls_addr: 0,
        tls_filesz: 0,
        tls_memsz: 0,
        // A raw blob has no ELF header to ask, so it is Linux by
        // definition: it is the hand-written smoke test, and what it makes
        // is a Linux `write` and a Linux `exit_group`.
        abi: crate::elf::Abi::Linux,
    }
}

/// Run it. Does not return: every way out of EL0 is through a vector.
pub fn run(p: &Process) -> ! {
    println!();
    println!("  entering EL0...");
    println!();
    // The memory layout belongs to the running task, because `brk` and `mmap`
    // are answered from whichever thread makes the call, and this is it.
    crate::sched::set_user_memory_for(p.ttbr0, p.brk, USER_HIGH_TOP);
    // The ABI, onto the task, before the first instruction can make a call.
    // The syscall path reads it from the task rather than from the Process,
    // because by then the Process is gone and the task is what remains.
    #[cfg(nk_lkl)]
    {
        let native = crate::nethos_abi::is_native(p.abi);
        crate::sched::set_nethos_abi(native);
        if native {
            println!(
                "  abi:    native NETHOS (version {})",
                crate::nethos_abi::ABI_VERSION
            );
        }
    }
    // Static TLS must exist before entry: musl reads `%fs:0x28` in `_start`,
    // before any syscall could set the base. x86-64 TP points just past the
    // template (variant 2), so the thread pointer is template + memsz aligned
    // up -- the same layout `ARCH_SET_FS` would install later.
    #[cfg(target_arch = "x86_64")]
    if p.tls_memsz > 0 {
        // `load` built the block and recorded TP itself.
        unsafe { crate::arch::set_tls(p.tls_addr) };
    }
    unsafe { enter_user(p.entry, p.stack, p.ttbr0) }
}

/// The register frame `el0_sync_entry` builds, in the order it writes it.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Frame {
    pub x: [u64; 31],
    pub elr: u64,
    pub spsr: u64,
    pub sp: u64,
}

/// Put SP_EL0 back to meaning "the current task" now that the user's value is
/// safely in the frame. Every Linux file the shim compiles reads its stack
/// canary through it, so kernel code cannot run correctly without it.
#[no_mangle]
pub extern "C" fn nk_enter_kernel() {
    crate::sched::restore_task_ptr();
}

/// The system call entry point called from `syscall_entry` in user-x86_64.s.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub extern "C" fn rust_syscall(frame: &mut Frame) {
    handle_syscall(frame);
}

/// Everything that arrives from EL0 synchronously: system calls, and faults.
#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub extern "C" fn rust_el0_sync(frame: &mut Frame) {
    let esr: u64;
    unsafe { core::arch::asm!("mrs {}, esr_el1", out(reg) esr, options(nomem, nostack)) };
    let ec = (esr >> 26) & 0x3f;

    // 0b010101 is SVC from AArch64. Anything else from EL0 is a fault, and a
    // fault in user space must not stop the kernel -- reporting it and
    // stopping the *process* is the whole difference between the two
    // privilege levels being worth having.
    if ec != 0b010101 {
        let far: u64;
        unsafe { core::arch::asm!("mrs {}, far_el1", out(reg) far, options(nomem, nostack)) };
        // A translation fault (DFSC 0b0001xx) inside a reservation is not a
        // fault at all: it is the first touch of a page the program asked
        // for with MAP_NORESERVE. Put a frame under it and retry the
        // instruction -- the program never learns anything happened.
        // Instruction aborts (0b1000xx) as well as data aborts: a program
        // may map executable pages with MAP_NORESERVE and jump into them,
        // and an unmapped instruction fetch is the same first touch.
        let abort = matches!(ec, 0b100000 | 0b100001 | 0b100100 | 0b100101);
        let translation = matches!(esr & 0x3f, 0x04..=0x07);
        if abort && translation {
            if crate::reserve::fault_in(far) {
                return;
            }
            if crate::reserve::defer_file_fault(frame, far) {
                return;
            }
        }
        report_user_fault(frame, esr, ec, far);
        return;
    }

    handle_syscall(frame);
}

#[cfg(target_arch = "aarch64")]
fn report_user_fault(frame: &Frame, esr: u64, ec: u64, far: u64) {
    let abort = matches!(ec, 0b100000 | 0b100001 | 0b100100 | 0b100101);
    let translation = matches!(esr & 0x3f, 0x04..=0x07);
    if abort && translation {
        let far_valid = esr & (1 << 24) != 0;
        if crate::reserve::fault_in(far) {
            return;
        }
        if matches!(ec, 0b100000 | 0b100001) && (far != frame.elr || !far_valid) && crate::reserve::fault_in(frame.elr) {
            return;
        }
    }
    println!();
    let (r_inside, r_below, r_above, r_live) = crate::reserve::describe(far);
    println!(
        "   reserve: inside {} last_end {:#x} next_start {:#x} live {}",
        r_inside, r_below, r_above, r_live
    );
    println!("!! fault in user space: esr {:#x} ec {:#b} il {} iss {:#x} far {:#x}", esr, ec, (esr >> 25) & 1, esr & 0x1ffffff, far);
    println!("   pc {:#x}  sp {:#x}  lr {:#x}", frame.elr, frame.sp, frame.x[30]);
    println!("   x0-x7:   {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x}", frame.x[0], frame.x[1], frame.x[2], frame.x[3], frame.x[4], frame.x[5], frame.x[6], frame.x[7]);
    println!("   x8-x15:  {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x}", frame.x[8], frame.x[9], frame.x[10], frame.x[11], frame.x[12], frame.x[13], frame.x[14], frame.x[15]);
    println!("   x16-x24: {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x}", frame.x[16], frame.x[17], frame.x[18], frame.x[19], frame.x[20], frame.x[21], frame.x[22], frame.x[23], frame.x[24]);
    println!("   x25-x30: {:#x} {:#x} {:#x} {:#x} {:#x} {:#x}", frame.x[25], frame.x[26], frame.x[27], frame.x[28], frame.x[29], frame.x[30]);
    for off in (0..=0x50).step_by(8) {
        let va = frame.sp.wrapping_add(off);
        if let Some(p) = crate::paging::user_to_phys(va & !7) {
            let w = unsafe { core::ptr::read_volatile(p as *const u64) };
            println!("   stack [sp+{:#x} ({:#x})]: {:#x}", off, va, w);
        }
    }
    for off in (-0x40..=0x10).step_by(8) {
        let va = (frame.x[29] as i64 + off) as u64;
        if let Some(p) = crate::paging::user_to_phys(va & !7) {
            let w = unsafe { core::ptr::read_volatile(p as *const u64) };
            println!("   frame [x29{:+03} ({:#x})]: {:#x}", off, va, w);
        }
    }
    match crate::paging::user_to_phys(frame.elr & !3) {
        Some(pa) => unsafe {
            let w = core::ptr::read_volatile(pa as *const u32);
            println!("   insn at pc: {:#010x}", w);
            // Oracle: what the page around the fault actually contains.
            match crate::paging::user_to_phys(frame.elr & !4095) {
                Some(base) => {
                    let mut bytes = [0u32; 8];
                    for i in 0..8 {
                        bytes[i] = core::ptr::read_volatile((base as *const u32).add(i));
                    }
                    println!(
                        "   page at {:#x}: {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
                        frame.elr & !4095, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]
                    );
                }
                None => println!("   page at {:#x}: not resident", frame.elr & !4095),
            }
        },
        None => println!("   insn at pc: unreadable (pc unmapped for EL0)"),
    }
    match crate::paging::user_to_phys(frame.x[30].wrapping_sub(4) & !3) {
        Some(pa) => unsafe {
            let w = core::ptr::read_volatile(pa as *const u32);
            println!("   insn before lr: {:#010x}", w);
            for off in (-40..=0).step_by(4) {
                let va = (frame.x[30] as i64 + off) as u64;
                if let Some(p) = crate::paging::user_to_phys(va & !3) {
                    let insn = core::ptr::read_volatile(p as *const u32);
                    println!("   insn at lr{:+03}: {:#010x}", off, insn);
                }
            }
        },
        None => println!("   insn before lr: unreadable (lr unmapped for EL0)"),
    }
    {
        let me = crate::sched::current_id();
        let ttbr0 = crate::arch::current_page_table();
        println!(
            "   task {} ttbr0 {:#x} linux_pid {}",
            me,
            ttbr0,
            crate::sched::linux_pid(me)
        );
        crate::paging::dump_walk(ttbr0, far);
        crate::paging::dump_walk_pc(ttbr0, far, frame.elr);
        for (label, base, len) in [("x8", frame.x[8], 0x60u64), ("x0", frame.x[0], 0x20u64)] {
            let mut off = 0u64;
            while off < len {
                if let Some(p) = crate::paging::user_to_phys((base + off) & !7) {
                    let w = unsafe { core::ptr::read_volatile(p as *const u64) };
                    println!("   {}[+{:#x}]: {:#x}", label, off, w);
                }
                off += 8;
            }
        }
    }
    sys_exit(-11);
}

fn handle_syscall(frame: &mut Frame) {
    use crate::syscall_nr::nr;

    // Page-in raised from the fault trampoline. Kernel state names the
    // pending fault; nothing here trusts what the register frame carried.
    if frame.x[8] == crate::reserve::PAGEIN_SYSCALL {
        if !crate::reserve::complete_file_fault(frame) {
            frame.x[0] = (-22i64) as u64;
        }
        return;
    }

    // The ABI split. A native NETHOS program arrives here through exactly
    // the same instruction and the same frame as a Linux one -- what differs
    // is only which table its number is read against, and that was decided
    // when its ELF was loaded. Everything below this point is the Linux
    // compatibility path.
    //
    // Checked first and once: a native process never falls through into
    // Linux's numbering, so a NETHOS call can never be mistaken for the
    // Linux syscall that happens to share its number.
    #[cfg(nk_lkl)]
    if crate::sched::nethos_abi() {
        let args: [u64; 6] = frame.x[..6].try_into().unwrap();
        let ret = crate::nethos_abi::dispatch(frame.x[8], &args);
        if crate::signal::deliver(frame) {
            return;
        }
        frame.x[0] = ret as u64;
        return;
    }

    #[cfg(target_arch = "x86_64")]
    if frame.x[8] == 158 {
        frame.x[0] = match frame.x[0] {
            0x1002 => {
                if frame.x[1] >= (1u64 << 47) {
                    (-1i64) as u64
                } else {
                    unsafe { crate::arch::set_tls(frame.x[1]) };
                    0
                }
            }
            0x1003 => {
                let base = crate::arch::current_tls().to_le_bytes();
                match crate::uaccess::copy_to_user(frame.x[1], &base) {
                    Ok(()) => 0,
                    Err(e) => e as u64,
                }
            }
            _ => (-22i64) as u64,
        };
        return;
    }

    // nk owns the exit status; the host TLS destructor performs Linux's
    // do_exit for this process's backing task before the nk thread finishes.
    if frame.x[8] == nr::EXIT || frame.x[8] == nr::EXIT_GROUP {
        sys_exit(frame.x[0] as i32);
    }

    // The console is nk's, and only the console.
    crate::sched::set_last_syscall(frame.x[8]);
    let console = (frame.x[0] == 1 || frame.x[0] == 2) && !crate::sched::has_console();
    let is_sig = matches!(
        frame.x[8],
        nr::KILL | nr::TKILL | nr::TGKILL | nr::SIGALTSTACK | nr::SIGSUSPEND
            | nr::RT_SIGACTION | nr::RT_SIGPROCMASK | nr::SIGPENDING
            | nr::SIGTIMEDWAIT | nr::RT_SIGRETURN
    );
    let ret = if is_sig && cfg!(nk_lkl) {
        if (frame.x[8] == nr::RT_SIGACTION || frame.x[8] == nr::RT_SIGPROCMASK) && frame.x[3] != 8 {
            crate::println!("  signal: syscall {} with sigsetsize {}, expected 8", frame.x[8], frame.x[3]);
            -22
        } else {
            signal_dispatch(frame)
        }
    } else if frame.x[8] == nr::WRITE && console {
        sys_write(frame.x[0], frame.x[1], frame.x[2])
    } else if frame.x[8] == nr::WRITEV && console {
        sys_writev(frame.x[0], frame.x[1], frame.x[2])
    } else if matches!(frame.x[8], nr::PIDFD_OPEN | nr::WAITID) && cfg!(nk_lkl) {
        -38
    } else if frame.x[8] == nr::CLONE3 {
        -38
    } else if frame.x[8] == nr::SCHED_YIELD {
        crate::sched::yield_now();
        0
    } else if cfg!(target_arch = "x86_64") && frame.x[8] == 7 && cfg!(nk_lkl) {
        poll_shim(frame.x[0], frame.x[1], frame.x[2] as i64)
    } else if frame.x[8] == nr::FUTEX && cfg!(nk_lkl) {
        futex(frame)
    } else if matches!(frame.x[8], nr::CLONE | nr::WAIT4) {
        process(frame)
    } else if (frame.x[8] == 221 && cfg!(target_arch = "aarch64") || frame.x[8] == 59 && cfg!(target_arch = "x86_64")) && cfg!(nk_lkl) {
        exec(frame.x[0], frame.x[1], frame.x[2])
    } else if matches!(
        frame.x[8],
        nr::BRK | nr::MMAP | nr::MUNMAP | nr::MPROTECT | nr::SET_TID_ADDRESS
            | nr::SET_ROBUST_LIST | nr::RSEQ | nr::PRLIMIT64 | nr::MSYNC
            | nr::MREMAP | nr::MADVISE
    ) {
        if frame.x[8] == nr::BRK {
            sys_brk(frame.x[0])
        } else if frame.x[8] == nr::MMAP {
            sys_mmap(frame.x[0], frame.x[1], frame.x[2], frame.x[3], frame.x[4] as i64, frame.x[5], frame.elr)
        } else if frame.x[8] == nr::MUNMAP {
            sys_munmap(frame.x[0], frame.x[1])
        } else if frame.x[8] == nr::MPROTECT {
            sys_mprotect(frame.x[0], frame.x[1], frame.x[2])
        } else if frame.x[8] == nr::SET_TID_ADDRESS {
            sys_set_tid_address()
        } else if frame.x[8] == nr::PRLIMIT64 {
            sys_prlimit64(frame.x[0], frame.x[1], frame.x[2], frame.x[3])
        } else if frame.x[8] == nr::MSYNC {
            sys_msync(frame.x[0], frame.x[1], frame.x[2])
        } else if frame.x[8] == nr::MREMAP {
            sys_mremap(frame.x[0], frame.x[1], frame.x[2], frame.x[3], frame.x[4], frame.elr)
        } else if frame.x[8] == nr::MADVISE {
            0
        } else {
            -38
        }
    } else if frame.x[8] == nr::CLOCK_GETTIME && cfg!(nk_lkl) && clock_gettime_fast(frame.x[0], frame.x[1]) {
        0
    } else if frame.x[8] == 999 {
        crate::manual_dump();
        0
    } else {
        if frame.x[8] == nr::PRCTL {
            record_comm(frame.x[0], frame.x[1]);
        }
        let args: [u64; 6] = frame.x[..6].try_into().unwrap();
        // A close that names a file mapping's pinned descriptor is not the
        // program's to make: the number is nk's read reference. A bulk close
        // that would sweep one is preceded by moving the pin out of range.
        // The legacy x86-64 calls first: they have no number to forward
        // to, only an `*at` form to be rewritten into. Everything else goes
        // by number, which is what `forward` translates.
        match x86_legacy(frame.x[8], &args) {
            Some(rc) => rc,
            None => forward(frame.x[8], &args),
        }
    };

    // Delivering a signal redirects the frame to the handler: `x0` is then
    // the signal number, not the syscall return, and must not be
    // overwritten. `rt_sigreturn` restores `x0` itself and returns a
    // sentinel for the same reason.
    if ret == -512 {
        return;
    }
    // An unimplemented syscall is said out loud on x86-64, because the port
    // is young enough that "-ENOSYS" is nearly always the next thing to fix
    // and a program reports it as a bewildering error much further on --
    // musl's startup turned one into "abort()" and nothing else.
    #[cfg(all(nk_lkl, target_arch = "x86_64"))]
    if ret == -38 {
        crate::println!(
            "  ENOSYS: syscall {} ({:#x}, {:#x}, {:#x}) from {:#x}",
            frame.x[8], frame.x[0], frame.x[1], frame.x[2], frame.elr
        );
    }
    #[cfg(nk_lkl)]
    if crate::signal::deliver(frame) {
        return;
    }
    frame.x[0] = ret as u64;
}

/// Write `clock_gettime`'s answer without entering Linux, if nk has it.
///
/// False means "not answered" and the caller must forward: an unknown
/// clock, the offsets not yet learned, or a user pointer that cannot be
/// written -- the last of which is left to Linux so the error it reports is
/// the one it would have reported anyway.
#[cfg(nk_lkl)]
fn clock_gettime_fast(clock: u64, out: u64) -> bool {
    if out == 0 {
        return false;
    }
    let Some(ns) = crate::futex::clock_for_syscall(clock) else {
        return false;
    };
    // struct timespec: two 64-bit words on this ABI.
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
    buf[8..].copy_from_slice(&(ns % 1_000_000_000).to_le_bytes());
    crate::uaccess::copy_to_user(out, &buf).is_ok()
}

#[cfg(not(nk_lkl))]
fn clock_gettime_fast(_clock: u64, _out: u64) -> bool {
    false
}

/// Remember a thread's chosen name (`prctl(PR_SET_NAME)`).
///
/// The watchdog's wait graph named tasks "thread pid 42" and nothing said
/// which thread of what: a standoff between WebKit's IPC threads and GLib's
/// main loop reads as four anonymous waiters. Zero-cost until something sets
/// a name (WebKit and GLib both do), and a bad pointer is simply no name.
fn record_comm(op: u64, arg: u64) {
    const PR_SET_NAME: u64 = 15;
    if op != PR_SET_NAME {
        return;
    }
    let mut raw = [0u8; 16];
    match crate::uaccess::copy_cstr_from_user(arg, &mut raw) {
        Ok(n) if n > 0 => {
            let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
            crate::sched::set_comm(&raw[..end.min(15)]);
        }
        _ => {}
    }
}

/// Signal syscalls, dispatched from `rust_el0_sync`. All numbers and
/// behaviours are Linux's; the state is nk's.
#[cfg(nk_lkl)]
fn signal_dispatch(frame: &mut Frame) -> i64 {
    use crate::syscall_nr::nr;
    match frame.x[8] {
        // kill(pid, sig): tkill is kill-to-self, tgkill names the thread
        // group explicitly. nk has one thread per Linux task and no groups
        // beyond parentage, so all three post to a Linux pid.
        nr::KILL => {
            let rc = crate::signal::kill(frame.x[0] as i64, frame.x[1]);
            if rc == 0 {
                record_signal_sender(frame.x[0] as i64, frame.x[1], 0);
            }
            rc
        }
        nr::TKILL => {
            let me = crate::sched::linux_pid(crate::sched::current_id());
            let rc = crate::signal::post(me, frame.x[0]);
            if rc == 0 {
                record_signal_sender(me, frame.x[0], 0);
            }
            rc
        }
        nr::TGKILL => {
            // tgkill(tgid, tid, sig): the group is ignored, the tid names
            // the task. A `tid` of -1 or 0 is -EINVAL, like Linux.
            let (tgid, tid, sig) = (frame.x[0] as i64, frame.x[1] as i64, frame.x[2]);
            if tid <= 0 {
                return -22;
            }
            let _ = tgid;
            let rc = crate::signal::post(tid, sig);
            if rc == 0 {
                record_signal_sender(tid, sig, 0);
            }
            rc
        }
        nr::SIGALTSTACK => sys_sigaltstack(frame.x[0], frame.x[1]),
        // sigsuspend: replace the mask, wait for a signal, return EINTR.
        // nk has no resume-other-than-EINTR: any delivered signal wakes,
        // and waking without one cannot happen, because nothing else wakes
        // this wait.
        nr::SIGSUSPEND => {
            let mut buf = [0u8; 8];
            if frame.x[0] != 0
                && crate::uaccess::copy_from_user(&mut buf, frame.x[0]).is_err()
            {
                return crate::uaccess::EFAULT;
            }
    let me = crate::sched::current_id();
    let saved = crate::sched::signal_mask(me);
    if frame.x[0] != 0 {
        let mut bits = u64::from_le_bytes(buf);
        bits &= !((1 << (crate::signal::SIGKILL - 1)) | (1 << (crate::signal::SIGSTOP - 1)));
                crate::sched::set_signal_mask(me, bits);
            }
            // A signal already pending is returned without sleeping: the
            // wait is over before it starts, which is what makes a blocked
            // mask plus a pending signal a poll rather than a hang.
            loop {
                if crate::signal::interrupt_pending() {
                    crate::sched::set_signal_mask(me, saved);
                    return -4; // -EINTR
                }
                crate::sched::yield_now();
            }
        }
        nr::RT_SIGACTION => crate::signal::sigaction(frame.x[0], frame.x[1], frame.x[2]),
        nr::RT_SIGPROCMASK => crate::signal::sigprocmask(frame.x[0], frame.x[1], frame.x[2]),
        nr::SIGPENDING => crate::signal::sigpending(frame.x[0]),
        // sigtimedwait: like sigsuspend but over a set, returning the
        // signal number instead of EINTR. The timeout is accepted and
        // ignored: nk waits until a signal in the set arrives, which is
        // correct for an infinite timeout and early for any other.
        nr::SIGTIMEDWAIT => {
            let mut buf = [0u8; 8];
            if frame.x[0] == 0 {
                return -22;
            }
            if crate::uaccess::copy_from_user(&mut buf, frame.x[0]).is_err() {
                return crate::uaccess::EFAULT;
            }
            let want = u64::from_le_bytes(buf);
            let me = crate::sched::current_id();
            loop {
                let got = crate::sched::signal_pending(me) & want & !crate::sched::signal_mask(me);
                if got != 0 {
                    let sig = got.trailing_zeros() as u64 + 1;
                    crate::sched::clear_signal_pending(me, 1 << (sig - 1));
                    if frame.x[2] != 0 {
                        let info = (sig as i32).to_le_bytes();
                        if crate::uaccess::copy_to_user(frame.x[2], &info).is_err() {
                            return crate::uaccess::EFAULT;
                        }
                    }
                    return sig as i64;
                }
                crate::sched::yield_now();
            }
        }
        nr::RT_SIGRETURN => crate::signal::sigreturn(frame),
        _ => -38,
    }
}

/// Remember who sent a signal, for the `siginfo` in the delivered frame.
/// `SI_USER` (0) for a `kill` from a task, whose Linux pid is recorded.
#[cfg(nk_lkl)]
fn record_signal_sender(pid: i64, sig: u64, code: i32) {
    let Some(id) = crate::sched::task_with_linux_pid(pid) else {
        return;
    };
    let sender = crate::sched::linux_pid(crate::sched::current_id()) as i32;
    crate::sched::set_signal_source(id, sig, code, sender);
}

/// # sigaltstack(ss, oss)
///
/// Records the alternate stack. `SS_DISABLE` (2) in `ss_flags` disables.
/// The old stack goes to `oss` when asked. Delivery honours it for
/// `SA_ONSTACK` handlers; the kernel never switches to it for anything
/// else.
#[cfg(nk_lkl)]
fn sys_sigaltstack(ss: u64, oss: u64) -> i64 {
    // stack_t is 24 bytes: base, flags, size.
    let me = crate::sched::current_id();
    if oss != 0 {
        let (base, size, in_use) = crate::sched::signal_altstack(me);
        let mut buf = [0u8; 24];
        buf[0..8].copy_from_slice(&base.to_le_bytes());
        buf[8..16].copy_from_slice(&if in_use { 1u64 } else { 2u64 }.to_le_bytes());
        buf[16..24].copy_from_slice(&size.to_le_bytes());
        if crate::uaccess::copy_to_user(oss, &buf).is_err() {
            return crate::uaccess::EFAULT;
        }
        // `ss_flags` reads `SS_DISABLE` when no stack is registered, which
        // is what a libc checks before installing its first one.
        if base == 0 {
            let mut zero = [0u8; 24];
            zero[8..16].copy_from_slice(&2u64.to_le_bytes());
            if crate::uaccess::copy_to_user(oss, &zero).is_err() {
                return crate::uaccess::EFAULT;
            }
        }
    }
    if ss != 0 {
        let mut buf = [0u8; 24];
        if crate::uaccess::copy_from_user(&mut buf, ss).is_err() {
            return crate::uaccess::EFAULT;
        }
        let base = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let flags = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let size = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        if flags & 2 != 0 {
            crate::sched::set_signal_altstack(me, 0, 0);
            return 0;
        }
        if size < 5120 {
            // MINSIGSTKSZ: a smaller stack cannot hold the frame.
            return -12; // -ENOMEM, like Linux
        }
        crate::sched::set_signal_altstack(me, base, size);
    }
    0
}

/// Without Linux there are no signal tasks to post to.
#[cfg(not(nk_lkl))]
fn signal_dispatch(_frame: &Frame) -> i64 {
    -38
}

/// Hand a call to Linux, with its pointers copied across.
#[cfg(nk_lkl)]
fn forward(nr: u64, args: &[u64; 6]) -> i64 {
    match crate::syscall::forward(nr, args) {
        Some(ret) => ret,
        None => -38, // -ENOSYS: no Linux to forward to
    }
}

/// # poll(fds, nfds, timeout_ms)
///
/// asm-generic -- which is what LKL is built with -- has no `poll`. It has
/// `ppoll`, and the difference is the timeout: poll's is a count of
/// milliseconds with -1 for "no timeout", ppoll's is a pointer to a
/// `timespec` with NULL for it.
///
/// Translating the syscall number alone gets this exactly backwards. poll's
/// most common timeout is `0` -- ask what is ready and return now -- and `0`
/// read as a pointer is NULL, which is ppoll's *block for ever*. That is a
/// program asking not to wait and being put to sleep permanently, which is
/// how musl's startup wedged here: one `poll(fds, 3, 0)` that never returned.
///
/// The `timespec` is nk's own, on nk's stack, and Linux reads it with
/// `copy_from_user` -- so the translation has to be off for the duration, or
/// the address would be checked against the process's tables and refused.
#[cfg(all(nk_lkl, target_arch = "x86_64"))]
fn sys_poll(fds: u64, nfds: u64, timeout_ms: i64) -> i64 {
    const PPOLL: u64 = 73;
    // A null timeout is ppoll's "wait for ever", which is what poll means by
    // a negative count. Everything else is a real deadline and needs a
    // `timespec` -- including zero, which must stay zero and not become null.
    if timeout_ms < 0 {
        return forward(PPOLL, &[fds, nfds, 0, 0, 0, 0]);
    }

    // `struct pollfd` is 8 bytes: fd, events, revents. Bounded because this
    // is nk's heap and `nfds` arrives from the process.
    const POLLFD: usize = 8;
    let Ok(n) = usize::try_from(nfds) else {
        return -22;
    };
    if n > 1024 {
        return -22; // -EINVAL, and well under Linux's own RLIMIT_NOFILE cap
    }
    let mut buf = alloc::vec![0u8; n * POLLFD];
    if n > 0 && crate::uaccess::copy_from_user(&mut buf, fds).is_err() {
        return crate::uaccess::EFAULT;
    }

    // Both buffers are nk's own now, so Linux's `copy_from_user` must treat
    // them as kernel addresses -- `in_user_syscall` false -- and the process
    // never gets an unchecked pointer through this path: its array was
    // copied in above and is copied back out below.
    let ts: [i64; 2] = [timeout_ms / 1000, (timeout_ms % 1000) * 1_000_000];
    let was = crate::sched::set_user_syscall(false);
    let rc = crate::lkl::syscall(
        PPOLL as i64,
        [buf.as_mut_ptr() as i64, nfds as i64, ts.as_ptr() as i64, 0, 0, 0],
    );
    crate::sched::set_user_syscall(was);

    if rc >= 0 && n > 0 && crate::uaccess::copy_to_user(fds, &buf).is_err() {
        return crate::uaccess::EFAULT;
    }
    rc
}

/// `nh_alloc` and `nh_free`, in terms of nk's own mmap.
///
/// Thin on purpose. The NETHOS ABI's memory calls are a narrowing of mmap,
/// not a different mechanism: the same reservation table, the same
/// demand-paged frames, the same accounting. What the ABI removes is the
/// choice of everything a program asking for memory does not care about.
///
/// `elr` is 0: it is only used to name the caller in a failure line, and a
/// native call has no libc frame to blame.
#[cfg(nk_lkl)]
pub fn nethos_alloc(len: u64, prot: u64, flags: u64) -> i64 {
    sys_mmap(0, len, prot, flags, -1, 0, 0)
}

#[cfg(nk_lkl)]
pub fn nethos_free(addr: u64, len: u64) -> i64 {
    sys_munmap(addr, len)
}

/// The x86-64 syscalls asm-generic does not have, in the form it does.
///
/// asm-generic -- LKL's table -- has no path-taking syscall at all. Where
/// x86-64 has `open(path, flags, mode)` it has only `openat(dirfd, path,
/// flags, mode)`, and the same for every `stat`, `unlink`, `mkdir`,
/// `rename` and `readlink` a program uses. The two differ by one leading
/// argument, `AT_FDCWD`, which means "relative to the current directory" --
/// exactly what the legacy call meant.
///
/// So this is not emulation, it is the same call with the argument the newer
/// form makes explicit. Returning `None` means "not one of these", and the
/// caller forwards by number as usual.
///
/// `dup2(old, new)` is the one that is not just a shifted argument:
/// asm-generic has `dup3(old, new, flags)`, which differs in refusing
/// `old == new` rather than returning it. That case is answered here.
#[cfg(all(nk_lkl, target_arch = "x86_64"))]
fn x86_legacy(nr: u64, a: &[u64; 6]) -> Option<i64> {
    use crate::syscall_nr::nr as x86;
    /// AT_FDCWD is -100 in every Linux ABI.
    const CWD: u64 = (-100i64) as u64;

    /// Straight to Linux by its *own* number.
    ///
    /// Deliberately not `forward`: that takes an x86-64 number and
    /// translates it, and everything below has already been translated.
    /// Passing `openat`'s asm-generic 56 to it would have been read as
    /// x86-64's 56, which is `clone` -- the rewritten `open` would have
    /// forked.
    fn at(lkl_nr: u64, args: [u64; 6]) -> i64 {
        let was = crate::sched::set_user_syscall(true);
        let rc = crate::lkl::syscall(lkl_nr as i64, args.map(|v| v as i64));
        crate::sched::set_user_syscall(was);
        rc
    }
        Some(match nr {
        // openat(AT_FDCWD, path, flags, mode)
        x86::OPEN => at(56, [CWD, a[0], a[1], a[2], 0, 0]),
        // newfstatat(AT_FDCWD, path, statbuf, flags)
        x86::STAT => at(79, [CWD, a[0], a[1], 0, 0, 0]),
        // ... with AT_SYMLINK_NOFOLLOW (0x100), which is what lstat is.
        x86::LSTAT => at(79, [CWD, a[0], a[1], 0x100, 0, 0]),
        // faccessat(AT_FDCWD, path, mode)
        x86::ACCESS => at(48, [CWD, a[0], a[1], 0, 0, 0]),
        // unlinkat(AT_FDCWD, path, 0)
        x86::UNLINK => at(35, [CWD, a[0], 0, 0, 0, 0]),
        // unlinkat with AT_REMOVEDIR (0x200), which is what rmdir is.
        x86::RMDIR => at(35, [CWD, a[0], 0x200, 0, 0, 0]),
        // mkdirat(AT_FDCWD, path, mode)
        x86::MKDIR => at(34, [CWD, a[0], a[1], 0, 0, 0]),
        // renameat(AT_FDCWD, old, AT_FDCWD, new)
        x86::RENAME => at(38, [CWD, a[0], CWD, a[1], 0, 0]),
        // readlinkat(AT_FDCWD, path, buf, size)
        x86::READLINK => at(78, [CWD, a[0], a[1], a[2], 0, 0]),
        // pipe2(fds, 0)
        x86::PIPE => at(59, [a[0], 0, 0, 0, 0, 0]),
        // dup3, except that dup2 of a descriptor onto itself is that
        // descriptor and dup3 of one onto itself is -EINVAL.
        x86::DUP2 => {
            if a[0] == a[1] {
                // Still has to be a real descriptor, which fcntl(F_GETFD)
                // asks without changing anything.
                let rc = at(25, [a[0], 1, 0, 0, 0, 0]);
                if rc < 0 { rc } else { a[0] as i64 }
            } else {
                at(24, [a[0], a[1], 0, 0, 0, 0])
            }
        }
        _ => return None,
    })
}

#[cfg(all(nk_lkl, not(target_arch = "x86_64")))]
fn x86_legacy(_nr: u64, _a: &[u64; 6]) -> Option<i64> {
    None
}

/// `sys_poll` on x86-64, and unreachable anywhere else.
///
/// aarch64 userspace uses the asm-generic table, where there is no `poll` to
/// translate: number 7 there is something else entirely and must not be
/// caught. The shim exists so the dispatch arm can be written once.
#[cfg(all(nk_lkl, target_arch = "x86_64"))]
fn poll_shim(fds: u64, nfds: u64, timeout_ms: i64) -> i64 {
    sys_poll(fds, nfds, timeout_ms)
}

#[cfg(all(nk_lkl, not(target_arch = "x86_64")))]
fn poll_shim(_fds: u64, _nfds: u64, _timeout_ms: i64) -> i64 {
    -38
}

/// Without Linux there is nothing to forward to, and nk's own table is two
/// entries: the console, and exit.
#[cfg(not(nk_lkl))]
fn forward(nr: u64, _args: &[u64; 6]) -> i64 {
    println!("  syscall {} is not implemented", nr);
    -38
}

/// # write(fd, buf, count)
///
/// The exception retains the process TTBR0. Translate with EL0 permissions
/// before reading through the kernel identity map; an EL1 dereference alone
/// would also allow the caller to read kernel pages.
fn sys_write(fd: u64, buf: u64, count: u64) -> i64 {
    if fd != 1 && fd != 2 {
        return -9; // -EBADF
    }
    let count = (count as usize).min(crate::uaccess::MAX_TRANSFER);
    let mut bytes = alloc::vec![0u8; count];
    if crate::uaccess::copy_from_user(&mut bytes, buf).is_err() {
        // Said out loud, because a refusal that only shows up as an errno in
        // an exit status is a thing nobody reads.
        println!("  refused a user pointer into kernel memory (EFAULT)");
        return crate::uaccess::EFAULT;
    }
    let uart = crate::uart::console();
    for b in &bytes {
        if *b == b'\n' {
            uart.put(b'\r');
        }
        uart.put(*b);
    }
    count as i64
}

/// # writev(fd, iov, iovcnt)
///
/// The console's scatter/gather form. Each entry is checked and copied on its
/// own, so a bad pointer half way down refuses without having invented a
/// short write: nothing is emitted until every entry has been read.
fn sys_writev(fd: u64, iov: u64, count: u64) -> i64 {
    if fd != 1 && fd != 2 {
        return -9; // -EBADF
    }
    if count > 1024 {
        return -22; // -EINVAL, and Linux's own UIO_MAXIOV
    }
    let mut raw = alloc::vec![0u8; count as usize * 16];
    if crate::uaccess::copy_from_user(&mut raw, iov).is_err() {
        println!("  refused a user pointer into kernel memory (EFAULT)");
        return crate::uaccess::EFAULT;
    }

    let mut bytes: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    for i in 0..count as usize {
        let e = &raw[i * 16..];
        let base = u64::from_le_bytes(e[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(e[8..16].try_into().unwrap()) as usize;
        if len == 0 {
            continue;
        }
        if bytes.len() + len > crate::uaccess::MAX_TRANSFER {
            return -22;
        }
        let at = bytes.len();
        bytes.resize(at + len, 0);
        if crate::uaccess::copy_from_user(&mut bytes[at..], base).is_err() {
            println!("  refused a user pointer into kernel memory (EFAULT)");
            return crate::uaccess::EFAULT;
        }
    }

    let uart = crate::uart::console();
    for b in &bytes {
        if *b == b'\n' {
            uart.put(b'\r');
        }
        uart.put(*b);
    }
    bytes.len() as i64
}

/// How far a process may grow its heap. A runaway `brk` loop should be told
/// no, not allowed to exhaust the machine's memory on behalf of a program
/// that has already gone wrong.
const USER_MEM_LIMIT: u64 = 64 * 1024 * 1024;

/// How large a single mapping may be, which is a different question from how
/// large a heap may be and was wrongly the same constant. A heap grows one
/// small step at a time and a limit on it catches a loop; a mapping is asked
/// for once, at a size the file decides, and refusing 117MB of libLLVM is
/// refusing to run Mesa rather than catching a mistake.
///
/// nk maps eagerly -- a frame per page, read at map time -- so this is real
/// memory, not address space. It is bounded by what the frame allocator has
/// rather than by anything a program can talk nk into.
const USER_MAP_LIMIT: u64 = 256 * 1024 * 1024;

/// # brk(addr)
///
/// Returns the break. `brk(0)` asks for it; anything else sets it, and the
/// return value is what it actually became -- Linux's brk reports failure by
/// returning the *old* break, never an errno, and a libc that gets an errno
/// here will not recognise it.
fn sys_brk(addr: u64) -> i64 {
    let (brk, brk_min, mmap_next) = crate::sched::user_memory();
    if brk_min == 0 {
        return brk as i64; // not a process with a heap
    }
    let want = addr.div_ceil(PAGE as u64) * PAGE as u64;
    if addr == 0 || want < brk_min || want >= mmap_next || want - brk_min > USER_MEM_LIMIT {
        return brk as i64;
    }
    if want > brk {
        // Mapped before committing: a sibling thread growing the heap at
        // the same time re-checks in `commit_brk`, so an overlap fails
        // rather than silently sharing pages.
        if !map_anonymous(brk, want - brk) {
            return brk as i64;
        }
        let committed = crate::sched::commit_brk(addr, want);
        if committed != addr {
            unsafe { crate::paging::unmap_user(current_ttbr0(), brk, want - brk) };
            return committed as i64;
        }
    } else if want < brk {
        unsafe { crate::paging::unmap_user(current_ttbr0(), want, brk - want) };
        crate::sched::set_user_brk(want);
    }
    // The *requested* address, not the page it rounded up to. Linux tracks
    // the break at byte granularity even though it maps whole pages, and a
    // libc told it got more than it asked for will hand the difference out
    // twice.
    addr as i64
}

/// # mmap(addr, len, prot, flags, fd, off)
///
/// Anonymous and private file mappings, eagerly populated through Linux
/// pread -- plus shared file mappings, which live in the pool in `shm.rs.
/// A shared mapping is the same physical pages for every holder, filled once
/// from the file and written back on `munmap` or `msync`. Anonymous shared
/// mappings have no file and are never written back.
fn sys_mmap(addr: u64, length: u64, prot: u64, flags: u64, fd: i64, offset: u64, elr: u64) -> i64 {
    if length == 0 || length > USER_MAP_LIMIT || offset & 4095 != 0 || prot & !7 != 0 {
        return -22;
    }
    // MAP_SHARED (0x1) or MAP_PRIVATE (0x2): exactly one must be set.
    let shared = flags & 3 == 1;
    if flags & 3 == 0 || flags & 3 == 3 {
        return -22; // -EINVAL: neither, or both
    }
    // MAP_NORESERVE (0x4000) is accepted and ignored, which is what it means
    // on Linux too: it says "do not account this against commit limits",
    // advice to an overcommit policy nk does not have. Refusing it is not
    // conservative, it is wrong -- a caller reserving address space gets
    // -EOPNOTSUPP for a flag that asks for nothing. WebKit reserves that way,
    // and an unrecognised flag here is invisible in a log that only reports
    // -ENOMEM.
    const MAP_NORESERVE: u64 = 0x4000;
    let _ = MAP_NORESERVE;
    if flags & !(0x1 | 0x2 | 0x10 | 0x20 | 0x8000 | 0x800 | 0x1000 | MAP_NORESERVE | 0x20000) != 0 {
        return -95; // -EOPNOTSUPP: an unknown flag, not a guess
    }
    if prot & 6 == 6 { return -13; }
    // MAP_SHARED without a file is anonymous-shared: same pages for every
    // holder that maps them, no file behind them. With MAP_ANONYMOUS the fd
    // is ignored, by the same rule Linux applies.
    let anonymous = flags & 0x20 != 0;

    let len = length.div_ceil(4096)*4096;
    let fixed = flags & 0x10 != 0;
    // Non-fixed mappings draw from the address space's shared pool, reserved
    // up front under one critical section: threads share these tables, so
    // two of them reading `mmap_next` apart and both subtracting is two
    // overlapping mappings, one thread's committed page inside another's
    // `PROT_NONE` reserve. Fixed mappings name their address and need none.
    // The reservation only guarantees non-overlap with other reservations;
    // the range checks below still apply, and a rejected reservation leaks
    // address space (not memory) -- see `reserve_mmap`. Failure lines name
    // the requester (elr) so the consumer can be found, not guessed.
    let at = if fixed {
        addr
    } else {
        match crate::sched::reserve_mmap(len) {
            Some(v) => v,
            None => {
                let (used, total) = crate::frames::stats();
                let (brk0, _, next0) = crate::sched::user_memory();
                crate::println!(
                    "  mmapfail reserve len {:#x} flags {:#x} fd {} brk {:#x} mmap_next {:#x} frames {}/{} elr {:#x}",
                    len, flags, fd, brk0, next0, used, total, elr
                );
                return -12;
            }
        }
    };
    let (brk, _, _) = crate::sched::user_memory();
    let Some(end) = at.checked_add(len) else {
        return -22;
    };
    // The current low-half layout still contains QEMU's devices. Never
    // replace those inherited mappings, even for a caller using MAP_FIXED.
    if !range_ok(at, end, brk) {
        let (_, _, next0) = crate::sched::user_memory();
        crate::println!(
            "  mmapfail range len {:#x} at {:#x} end {:#x} brk {:#x} mmap_next {:#x} flags {:#x} fd {} elr {:#x}",
            len, at, end, brk, next0, flags, fd, elr
        );
        return -12;
    }
    let executable = prot & 4 != 0;
    let writable = prot & 2 != 0;
    if fixed && crate::shm::overlaps_device(current_ttbr0(), at, len) {
        return -95;
    }

    if shared {
        if fixed {
            // A fixed shared mapping replaces whatever was there, including
            // pool references -- which must be released, not freed.
            for m in crate::sched::remove_shared_maps(at, len) {
                let lo = at.max(m.start);
                let hi = (at + len).min(m.start + m.len);
                if hi > lo {
                    unsafe { crate::paging::unmap_user_nofree(current_ttbr0(), lo, hi - lo) };
                }
                if m.anonymous {
                    crate::shm::release_anon(m.region);
                } else {
                    crate::shm::release(m.region, m.fd);
                }
            }
        }
        // Anonymous-shared ignores the fd entirely, like Linux: no file to
        // key on, no `fstat`, no writeback. A shared mapping of a file with
        // fd < 0 is -EBADF, not anonymous -- the flag decides, not the fd.
        if anonymous {
            let region = crate::shm::map_anonymous_shared(len, at, writable, executable, prot == 0);
            if region == usize::MAX {
                return -12; // -ENOMEM
            }
            if !crate::sched::add_shared_map(SharedMap { start: at, len, region, fd: -1, anonymous: true }) {
                unsafe { crate::paging::unmap_user_nofree(current_ttbr0(), at, len) };
                crate::shm::release_anon(region);
                return -12;
            }
            return at as i64;
        }
        if fd < 0 {
            return -9; // -EBADF: shared file mapping with no file
        }
        let region = match crate::shm::map_shared(fd, offset, len, at, writable, executable, prot == 0) {
            Ok(r) => r,
            Err(e) => return e,
        };
        if !crate::sched::add_shared_map(SharedMap { start: at, len, region, fd, anonymous: false }) {
            // The table is full. Undo the mapping and say so: the pool
            // reference is released, the pages stay for other holders.
            unsafe { crate::paging::unmap_user_nofree(current_ttbr0(), at, len) };
            crate::shm::release(region, fd);
            return -12;
        }
        return at as i64;
    }

    // A batch of pages per read, not a read per page.
    //
    // The frames a mapping is built from are scattered -- the allocator's
    // free list is single pages in no order -- so a chunk cannot be one
    // buffer. `preadv` is exactly the operation for that shape: one syscall,
    // one iovec per page, filling memory that is not contiguous. It matters
    // because a `pread64` per 4KB goes into LKL, through ext4 and out to
    // virtio-blk, and libLLVM's text segment is 117MB: thirty thousand round
    // trips, and minutes of wall clock before Mesa has even loaded.
    //
    // `alloc_contiguous` would have been the other answer and is the wrong
    // one: it only ever bumps, because the free list cannot satisfy a run, so
    // every unmapped file mapping would return pages it could never reuse.
    const BATCH: usize = 256;
    #[repr(C)]
    struct IoVec { base: u64, len: u64 }

    // Anonymous mappings are demand-paged, not populated: the range goes in
    // the reservation table and `reserve::fault_in` puts a zeroed frame under
    // each page on first touch. This used to be for MAP_NORESERVE only, and
    // only when not MAP_FIXED and not PROT_NONE -- three doors, and JSC's
    // cage reservation walked through all of them: a gigabyte reserved
    // MAP_FIXED and PROT_NONE, then committed slice by slice with mprotect.
    // Eager population turned the reservation into 250,000 real frames in
    // one second, and a machine with 100MB free hit `out of memory for a
    // page table` (765849/765849, twice, at t+18s) -- whether it did was a
    // coin flip on what had been allocated before the WebProcess started.
    // Linux faults anonymous memory in on touch; MAP_POPULATE is the one
    // flag that asks otherwise, and it is honoured. PROT_NONE ranges are
    // recorded but never faulted in -- a touch there is a real fault.
    const MAP_POPULATE: u64 = 0x8000;
    if anonymous && flags & MAP_POPULATE == 0 {
        if fixed {
            // A fixed mapping replaces whatever was there, mappings and
            // reservations alike, exactly as the eager path below does.
            unsafe { crate::paging::unmap_user(current_ttbr0(), at, len) };
        }
        crate::reserve::forget(at, len);
        if crate::reserve::add(at, len, prot & 2 != 0, prot & 4 != 0, prot == 0) {
            if !fixed {
                crate::sched::set_user_mmap_next(at);
            }
            return at as i64;
        }
        // No metadata, no mapping. Falling back to eager population here
        // would silently change the memory policy of the mapping because a
        // table was full -- and for a large library that is a frame
        // exhaustion failure with no obvious cause.
        return -12;
    }

    // Private file mappings are demand-paged the same way: the range and the
    // file it reads from are recorded, and the first touch of each page
    // preads that page. Eagerly reading every page made four WebKit processes
    // mapping one library four resident copies of it, and the frame pool ran
    // out before the NetworkProcess could start. MAP_POPULATE asks for the
    // old behaviour and gets it.
    // Read-only for now: a library's text and read-only data are the bulk
    // of what four WebKit processes duplicate, and serving them has no
    // write-back or COW question. Writable private file pages stay eager
    // until that question has an answer.
    if !anonymous && !writable && fd >= 0 && flags & MAP_POPULATE == 0 {
        if fixed {
            unsafe { crate::paging::unmap_user(current_ttbr0(), at, len) };
        }
        crate::reserve::forget(at, len);
        if crate::reserve::add_file(at, len, writable, executable, fd as i32, offset) {
            if !fixed {
                crate::sched::set_user_mmap_next(at);
            }
            return at as i64;
        }
        return -12;
    }

    let mut pages: Vec<*mut u8> = Vec::new();
    let mut iov: Vec<IoVec> = Vec::new();
    // File size distinguishes EOF tail (must stay zero) from a short read
    // within the file (must be retried). `preadv` past EOF returns 0/short,
    // not an errno, so the old `rc < 0` check alone cannot tell them apart.
    #[cfg(nk_lkl)]
    let file_size: u64 = if !anonymous {
        let mut st = [0u8; 128];
        let rc = crate::lkl::syscall(80, [fd, st.as_mut_ptr() as i64, 0, 0, 0, 0]);
        if rc < 0 {
            return rc;
        }
        u64::from_le_bytes(st[48..56].try_into().unwrap())
    } else {
        0
    };
    #[cfg(not(nk_lkl))]
    let file_size: u64 = 0;
    let mut off = 0u64;
    while off < len {
        let want = core::cmp::min(BATCH as u64, (len - off) / PAGE as u64) as usize;
        iov.clear();
        let first = pages.len();
        for _ in 0..want {
            let Some(page) = frames::alloc() else {
                for page in pages { unsafe { frames::free(page); } }
                return -12;
            };
            // alloc() zeroes, which is what a mapping past the end of a file
            // and the tail of a partial page both require.
            iov.push(IoVec { base: page as u64, len: PAGE as u64 });
            pages.push(page);
        }
        if !anonymous {
            // A short read past EOF is not an error: those pages must read
            // as zero, which they already do. A short read *within* the
            // file is retried: per-page loop until full, error, or EOF hit.
            #[cfg(nk_lkl)]
            {
                let base = match offset.checked_add(off) {
                    Some(v) if v <= i64::MAX as u64 => v,
                    _ => {
                        for page in pages { unsafe { frames::free(page); } }
                        return -22;
                    }
                };
                // Retry within-file short reads to the exact byte: preadv
                // fills the iovec array sequentially, so resume at byte
                // `done` with the first iovec shortened by the intra-page
                // remainder. Bytes never re-read, never skipped.
                let fend = offset.saturating_add(len).min(file_size);
                let mut done = 0usize;
                let total = want * PAGE as usize;
                loop {
                    let start_page = done / PAGE as usize;
                    let intra = done % PAGE as usize;
                    let mut win: Vec<IoVec> = Vec::new();
                    for (j, e) in iov.iter().enumerate().skip(start_page) {
                        if j == start_page {
                            win.push(IoVec { base: e.base + intra as u64, len: e.len - intra as u64 });
                        } else {
                            win.push(IoVec { base: e.base, len: e.len });
                        }
                    }
                    if win.is_empty() {
                        break;
                    }
                    let rc = crate::lkl::syscall(69, [fd, win.as_ptr() as i64, win.len() as i64,
                        (base + done as u64) as i64, 0, 0]);
                    if rc < 0 {
                        for page in pages { unsafe { frames::free(page); } }
                        return rc;
                    }
                    if rc == 0 {
                        break;
                    }
                    done += rc as usize;
                    if done >= total {
                        break;
                    }
                    if base + done as u64 >= fend {
                        break;
                    }
                    if rc as usize % PAGE as usize != 0 {
                        crate::println!(
                            "  mmapshort fd {} off {:#x} done {}/{} elr {:#x}",
                            fd, base, done, total, elr
                        );
                    }
                }
            }
            #[cfg(not(nk_lkl))]
            {
                let rc = -38;
                if rc < 0 {
                    for page in pages { unsafe { frames::free(page); } }
                    return rc;
                }
            }
        }
        if prot & 4 != 0 {
            for page in &pages[first..] { unsafe { publish_code(*page, PAGE); } }
        }
        off += (want * PAGE) as u64;
    }
    let root = current_ttbr0();
    if fixed { unsafe { paging::unmap_user(root,at,len); } }
    for (i,page) in pages.into_iter().enumerate() {
        unsafe { paging::map_user_permissions(root,at+i as u64*4096,page as u64,4096,prot&4!=0,prot&2!=0); }
    }
    if prot == 0 { unsafe { paging::protect_user_none(root, at, len); } }
    at as i64
}

/// # mremap(old_addr, old_size, new_size, flags, new_addr)
///
/// Every supported move carries MREMAP_MAYMOVE: the mapping is re-created
/// at a fresh range and the old one dropped. Growing in place would need
/// the neighbour's address space, which reservations never give back, so
/// without MAYMOVE the answer is -ENOMEM -- which libc's realloc is written
/// to take (it falls back to malloc, copy, free) and which no caller here
/// passes anyway.
///
/// Two kinds of mapping move:
///
///   * a shared file mapping -- the `wl_shm` pool case, which is why this
///     exists: weston's answer to `wl_shm_pool.resize` is an mremap, and
///     the refusal used to end the connection with "failed mremap" the
///     first time a cursor changed over an app page. The pool is re-mapped
///     from the same fd and offset; the region's pages are shared, so the
///     bytes move with the mapping and nothing is copied. A pool the client
///     has already grown is found by the same (file, offset) region lookup,
///     so the remap lands on the client's pages, cursor image included.
///     Anonymous-shared is refused: its region cannot be aliased through
///     any map call, and moving the record onto fresh pages would silently
///     fork the contents.
///   * a private anonymous reservation -- glibc's realloc moves its large
///     chunks this way. The new range is reserved, the pages the old range
///     actually faulted in are copied, and pages never touched stay
///     demand-paged zeros.
fn sys_mremap(old_addr: u64, old_size: u64, new_size: u64, flags: u64, new_addr: u64, elr: u64) -> i64 {
    const MREMAP_MAYMOVE: u64 = 1;
    const MREMAP_FIXED: u64 = 2;
    if old_addr & 4095 != 0 || old_size == 0 || new_size == 0
        || new_size > USER_MAP_LIMIT || flags & !(MREMAP_MAYMOVE | MREMAP_FIXED) != 0 {
        return -22;
    }
    if flags & MREMAP_MAYMOVE == 0 {
        return -12;
    }
    let old_len = old_size.div_ceil(4096) * 4096;
    let new_len = new_size.div_ceil(4096) * 4096;
    let (brk, _, _) = crate::sched::user_memory();
    let fixed = flags & MREMAP_FIXED != 0;
    let at = if fixed {
        if new_addr & 4095 != 0 {
            return -22;
        }
        let Some(end) = new_addr.checked_add(new_len) else {
            return -22;
        };
        if !range_ok(new_addr, end, brk) {
            return -12;
        }
        // A fixed destination replaces whatever is there, exactly as the
        // MAP_FIXED path of mmap does: pool references released, not freed,
        // private pages freed, reservations forgotten.
        for m in crate::sched::remove_shared_maps(new_addr, new_len) {
            let lo = new_addr.max(m.start);
            let hi = (new_addr + new_len).min(m.start + m.len);
            if hi > lo {
                unsafe { crate::paging::unmap_user_nofree(current_ttbr0(), lo, hi - lo) };
            }
            if m.anonymous {
                crate::shm::release_anon(m.region);
            } else {
                crate::shm::release(m.region, m.fd);
            }
        }
        unsafe { crate::paging::unmap_user(current_ttbr0(), new_addr, new_len) };
        crate::reserve::forget(new_addr, new_len);
        new_addr
    } else {
        match crate::sched::reserve_mmap(new_len) {
            Some(v) => v,
            None => {
                crate::println!(
                    "  mremapfail reserve old {:#x} old_len {:#x} new_len {:#x} elr {:#x}",
                    old_addr, old_len, new_len, elr
                );
                return -12;
            }
        }
    };

    // Shared file mapping: the pool case. The replacement is mapped first,
    // so the region holds a reference for this task across the swap; the
    // old record then goes whole, exactly as an unmap of it would take it.
    // grow_shared keeps the region's pages -- the pool's live bytes -- and
    // extends the region when the new length reaches past it, so a second
    // holder that resizes after the first still finds the same pages. Pools
    // are read-write and non-executable, the only shape of shared file
    // mapping anything here mremaps (a wl_shm pool is mapped
    // PROT_READ|PROT_WRITE by every holder).
    if let Some(m) = crate::sched::shared_map_covering(old_addr, old_len) {
        if m.anonymous {
            return -12;
        }
        let new_region = match crate::shm::grow_shared(m.region, at, new_len, true, false, false) {
            Ok(r) => r,
            Err(e) => return e,
        };
        if !crate::sched::add_shared_map(SharedMap { start: at, len: new_len, region: new_region, fd: m.fd, anonymous: false }) {
            unsafe { crate::paging::unmap_user_nofree(current_ttbr0(), at, new_len) };
            crate::shm::release(new_region, m.fd);
            return -12;
        }
        for old in crate::sched::remove_shared_maps(old_addr, old_len) {
            unsafe { crate::paging::unmap_user_nofree(current_ttbr0(), old.start, old.len) };
            crate::shm::release(old.region, old.fd);
        }
        if !fixed {
            crate::sched::set_user_mmap_next(at);
        }
        return at as i64;
    }

    // Private anonymous: one reservation covering the whole old range, its
    // flags describing what fault_in would map any missing page with.
    if let Some((writable, exec, none, backing)) = crate::reserve::covering(old_addr, old_len) {
        if !crate::reserve::add_backed(at, new_len, writable, exec, none, backing) {
            return -12;
        }
        if none {
            // A PROT_NONE reservation has no pages to copy; the new range
            // must refuse a touch in exactly the same way.
            unsafe { crate::paging::protect_user_none(current_ttbr0(), at, new_len) };
        } else {
            let root = current_ttbr0();
            for i in 0..old_len.min(new_len) / 4096 {
                let old_va = old_addr + i * 4096;
                let Some(src) = paging::user_to_phys(old_va) else { continue };
                let Some(frame) = frames::alloc() else {
                    // Give the moved pages back; the old range is untouched
                    // and the caller keeps a working mapping to retry from.
                    unsafe { crate::paging::unmap_user(root, at, new_len) };
                    crate::reserve::forget(at, new_len);
                    return -12;
                };
                unsafe {
                    core::ptr::copy_nonoverlapping(src as *const u8, frame, 4096);
                    paging::map_user_permissions(root, at + i * 4096, frame as u64, 4096, exec, writable);
                }
            }
        }
        crate::reserve::forget(old_addr, old_len);
        unsafe { crate::paging::unmap_user(current_ttbr0(), old_addr, old_len) };
        if !fixed {
            crate::sched::set_user_mmap_next(at);
        }
        return at as i64;
    }

    crate::println!(
        "  mremap: {:#x}+{:#x} is not a shared mapping or a reservation elr {:#x}",
        old_addr, old_len, elr
    );
    -14 // -EFAULT: nothing mapped there to move
}

/// # munmap(addr, len)
///
/// Unmapping a hole is legal and returns success, which is what makes it safe
/// for a libc to call over a range it is not sure about. The addresses are
/// not reused: `mmap_next` only ever falls, so a freed region stays free
/// rather than being handed out again while something still holds a pointer
/// into it. That wastes address space and not memory, and the pages
/// themselves do go back.
///
/// Shared mappings are the exception to "the pages go back": the pool owns
/// them, so unmapping drops a pool reference (writing back first) and only
/// removes the caller's page-table entries, without freeing.
fn sys_munmap(addr: u64, len: u64) -> i64 {
    crate::reserve::forget(addr & !4095, len.div_ceil(4096) * 4096);
    let (_, brk_min, _) = crate::sched::user_memory();
    if brk_min == 0 || len == 0 {
        return -22; // -EINVAL
    }
    let start = addr & !(PAGE as u64 - 1);
    let end = (addr + len).div_ceil(PAGE as u64) * PAGE as u64;
    let high = start >= USER_HIGH_BASE;
    if end <= start
        || (high && end > USER_HIGH_TOP)
        || (!high && (start < brk_min || end > USER_STACK_TOP))
    {
        return -22;
    }
    // Release pool references first, over the page-rounded range. A shared
    // record overlapping the range is taken whole: the pool counts
    // references per mapping, not per page, so a partial unmap cannot drop
    // half a reference. Rounding up over-unmaps a partial request -- the
    // whole record goes -- which is exact in the accounting and surprising
    // only to a caller unmapping half a shared mapping, which no libc does
    // to a `wl_shm` buffer. The pool's pages leave the tables via
    // `unmap_user_nofree` (never freed: other holders still map them) before
    // the private `unmap_user` below, which then finds holes there and frees
    // nothing it should not.
    for m in crate::sched::remove_shared_maps(start, end - start) {
        if m.start < start || end < m.start + m.len {
            crate::println!(
                "  shm: partial munmap {:#x}..{:#x} takes whole record {:#x}..{:#x} (anon {})",
                start, end, m.start, m.start + m.len, m.anonymous
            );
        }
        unsafe { crate::paging::unmap_user_nofree(current_ttbr0(), m.start, m.len) };
        if m.anonymous {
            crate::shm::release_anon(m.region);
        } else {
            crate::shm::release(m.region, m.fd);
        }
    }
    unsafe { crate::paging::unmap_user(current_ttbr0(), start, end - start) };
    0
}

#[cfg(nk_lkl)]
fn futex(frame: &Frame) -> i64 {
    crate::futex::futex(
        frame.x[0],
        frame.x[1],
        frame.x[2] as u32,
        frame.x[3],
        frame.x[4],
        frame.x[5] as u32,
    )
}

#[cfg(not(nk_lkl))]
fn futex(_frame: &Frame) -> i64 {
    -38
}

/// clone and wait4, or -ENOSYS on a build with no Linux behind them.
#[cfg(nk_lkl)]
fn process(frame: &Frame) -> i64 {
    if frame.x[8] == 220 {
        fork(frame)
    } else {
        wait4(frame.x[0] as i64, frame.x[1], frame.x[2])
    }
}

#[cfg(not(nk_lkl))]
fn process(frame: &Frame) -> i64 {
    println!("  syscall {} needs a Linux task to attach to", frame.x[8]);
    -38
}

/// What a forked child needs to become itself, handed to its new nk thread.
#[cfg(nk_lkl)]
struct Forked {
    frame: Frame,
    ttbr0: u64,
    tpidr: u64,
    /// Whose descriptors the child is to inherit.
    parent_pid: i64,
    /// A thread shares its creator's address space rather than owning a copy.
    /// The failure path has to know: freeing it would take the address space
    /// out from under the threads still running in it.
    shares_mm: bool,
    /// A vfork child shares the address space like a thread and is a process
    /// like a fork: its own pid, its own descriptor table, its own signal
    /// dispositions. The two flags are independent for that reason.
    vfork: bool,
    /// Whether the forking process had a real `/dev/console` on 0, 1 and 2.
    /// A child that inherits those descriptors has one too, and must be told
    /// so: without it nk answers the child's writes to 1 and 2 itself, which
    /// sends them to the UART no matter what they were redirected to.
    has_console: bool,
    /// Which ABI the parent spoke. A child is running the parent's image --
    /// fork copies it, clone shares it -- so it speaks the same interface,
    /// and inheriting it is the only answer that can be right.
    nethos_abi: bool,
    /// Addresses the clone asked to have the new thread id written to, or 0.
    /// Written by the thread itself rather than by its creator: the creator
    /// only regains control after the thread is already running, by which
    /// time a short thread may have exited and cleared the very word we are
    /// about to write. Linux writes these in copy_process for that reason.
    set_tid: (u64, u64),
    /// Signalled once the child has a Linux pid, because `fork` has to return
    /// that pid to the parent and only the child can obtain one: attaching
    /// binds the Linux task to the host thread it runs on.
    ready: &'static crate::sync::Semaphore,
    pid: &'static core::sync::atomic::AtomicI64,
    /// Shared regions the child inherits. `fork` copies the pages into the
    /// child's address space the way it copies everything else -- a real
    /// copy, not a second reference -- and records what it mapped so
    /// `munmap` can release the pool's reference rather than freeing pages
    /// the pool still owns.
    shared: alloc::vec::Vec<SharedMap>,
    /// Signal state the child inherits: dispositions, mask, altstack. The
    /// child starts with nothing pending -- signals sent to the parent
    /// before the fork are the parent's, not the child's. A thread takes
    /// nothing: it shares the creator's table by sharing its address space.
    sig_actions: [crate::signal::Action; 64],
    sig_mask: u64,
    /// The vector unit, copied as Linux copies it: a fork or a clone child
    /// runs with the parent's q0-q31 until it changes them.
    fpsimd: crate::sched::FpSave,
    sig_alt_base: u64,
    sig_alt_size: u64,
    cloexec: [u64; 8],
}

/// One shared mapping in a process: where it is, and which pool region it
/// points at. Kept per task in the scheduler, next to `brk` and `mmap_next`.
/// `anonymous` distinguishes pool regions with no file behind them: they are
/// released without writeback.
#[derive(Clone, Copy)]
pub struct SharedMap {
    pub start: u64,
    pub len: u64,
    pub region: usize,
    pub fd: i64,
    pub anonymous: bool,
}

/// # clone(flags, stack, ...) -- but only the shape `fork` uses
///
/// A real `clone` is a menu: sharing memory makes a thread, sharing nothing
/// makes a process, and the flags say which. nk implements the one column of
/// that menu it can honour completely, and refuses the rest rather than
/// silently giving a thread its own memory -- which would look like it worked
/// until two of them disagreed about a variable.
#[cfg(nk_lkl)]
fn fork(frame: &Frame) -> i64 {
    use core::sync::atomic::{AtomicI64, Ordering};

    // glibc's fork() is clone(CLONE_CHILD_CLEARTID|CLONE_CHILD_SETTID|SIGCHLD,
    // 0, ...). The tid flags concern a pointer nk does not write to, and
    // SIGCHLD is the exit signal, which nk does not deliver -- wait4 is how a
    // parent finds out here. Anything asking to share memory or files is a
    // thread and is refused.
    const CLONE_VM: u64 = 0x0100;
    const CLONE_THREAD: u64 = 0x00010000;
    if frame.x[0] & CLONE_THREAD != 0 {
        return thread(frame);
    }
    // CLONE_VM without CLONE_THREAD is vfork's shape, and it is what glibc's
    // `posix_spawn` uses: clone(CLONE_VM|CLONE_VFORK|SIGCHLD, stack). It is
    // also the only way WebKit starts its network and web processes, so
    // refusing it is refusing a browser.
    //
    // nk gives the child a *copy* of the address space rather than sharing
    // it, and does not stop the parent. Both are departures from vfork, and
    // both are safe in the direction that matters here: a child with its own
    // pages cannot corrupt the parent's before `execve`, and a parent that
    // keeps running cannot be deadlocked by a child that never execs.
    //
    // What is lost is the one word the sharing was for. `posix_spawn` has
    // the child write `execve`'s errno into memory the parent reads, so a
    // failed exec becomes a `posix_spawn` failure; with a copy the parent
    // reads its own zero and gets a pid whose process exits immediately.
    // The caller sees a child that died rather than a spawn that failed --
    // less informative, and not wrong.
    let vfork = frame.x[0] & CLONE_VM != 0;
    if vfork && frame.x[1] == 0 {
        // No stack. Parent and child would run on the same one in the same
        // address space, which is the single thing vfork cannot survive.
        return -22; // -EINVAL
    }

    let parent = current_ttbr0();
    // vfork shares; fork copies. Sharing is not an optimisation here, it is
    // the only thing that works: `posix_spawn` is called from processes with
    // hundreds of megabytes mapped, and copying all of it to throw it away
    // one syscall later at `execve` took longer than the run's whole budget.
    let shared = crate::sched::shared_maps();
    let ttbr0 = if vfork {
        parent
    } else {
        let Some(child) = (unsafe { paging::copy_user_address_space(parent) }) else {
            return -12; // -ENOMEM
        };
        // The child's tables are a copy; so is its layout entry. From here
        // the two address spaces allocate independently.
        crate::sched::set_user_memory_full(child, false);
        // Reservations are promises, not pages, so copying pages does not
        // carry them. Without this the child faults on the first byte of an
        // arena its parent could use.
        crate::reserve::inherit(paging::table_of(parent), paging::table_of(child));
        for m in &shared {
            crate::shm::retain(m.region);
        }
        child
    };
    // What the parent blocks on. Leaked for the same reason as `ready`: the
    // child signals it from its own thread, after this frame is gone.
    let done: &'static crate::sync::Semaphore =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(crate::sync::Semaphore::new(0)));
    crate::sync::track(done);

    let tpidr = crate::arch::current_tls();

    // Leaked on purpose: the child reads them on its own thread after this
    // one has returned to EL0, so they cannot live in this stack frame. One
    // pair per fork is a real cost and the honest fix is a slab of them,
    // which is worth doing when fork is on a hot path and not before.
    let ready: &'static crate::sync::Semaphore =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(crate::sync::Semaphore::new(0)));
    crate::sync::track(ready);
    let pid: &'static AtomicI64 =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(AtomicI64::new(0)));

    let mut child_frame = Frame { x: frame.x, elr: frame.elr, spsr: frame.spsr, sp: frame.sp };
    child_frame.x[0] = 0; // what fork returns in the child
    // vfork's caller supplies the stack the child runs on, because the two
    // were meant to share one address space and could not share one stack.
    // The copy makes it the child's own; the pointer is still where the
    // caller expects its frame to be.
    if vfork && frame.x[1] != 0 {
        child_frame.sp = frame.x[1];
    }

    let me0 = crate::sched::current_id();
    let mut cloexec = [0u64; 8];
    for fd in 0..512 {
        let fl = crate::lkl::syscall(25, [fd as i64, 1, 0, 0, 0, 0]);
        if fl > 0 && fl & 1 != 0 {
            cloexec[fd / 64] |= 1u64 << (fd % 64);
        }
    }
    let arg = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(Forked {
        frame: child_frame,
        ttbr0,
        tpidr,
        parent_pid: crate::sched::linux_pid(me0),
        shares_mm: vfork,
        vfork,
        has_console: crate::sched::has_console(),
        nethos_abi: crate::sched::nethos_abi(),
        set_tid: (0, 0),
        ready,
        pid,
        shared,
        sig_actions: crate::sched::signal_actions(me0),
        sig_mask: crate::sched::signal_mask(me0),
        fpsimd: crate::sched::fp_snapshot(me0),
        sig_alt_base: crate::sched::signal_altstack(me0).0,
        sig_alt_size: crate::sched::signal_altstack(me0).1,
        cloexec,
    })) as usize;

    let me = crate::sched::current_id();
    let flags = crate::sync::irq_save();
    let id = crate::sched::spawn("forked", forked_entry, arg);
    crate::sched::set_parent(id, me);
    if vfork {
        crate::sched::set_vfork_child(id, done as *const _ as usize);
    }
    unsafe { crate::sync::irq_restore(flags) };

    // Wait for the child to have a pid. fork returns it, and only the child
    // can get one -- attaching binds the Linux task to the host thread that
    // does the attaching.
    ready.down();
    let p = pid.load(Ordering::Acquire);
    // vfork's promise. Only after this may the parent touch the address
    // space again -- the child has been running in it, on the stack the
    // caller passed, and has now either replaced it or left it.
    if vfork && p > 0 {
        done.down();
    }
    p
}

#[cfg(nk_lkl)]
extern "C" fn forked_entry(arg: usize) {
    use core::sync::atomic::Ordering;
    let f = *unsafe { alloc::boxed::Box::from_raw(arg as *mut Forked) };

    // A Linux task of its own, so the child has its own pid and its own view
    // of the filesystem. This is *not* a copy of the parent's: descriptors
    // the parent had open are not inherited, which real fork does inherit and
    // a shell will need. It is a limitation, not a design.
    // A process that cannot fork is a failed fork, not a dead kernel: the
    // parent gets an errno and decides what to do about it.
    // A thread shares its creator's descriptors; a process gets its own.
    // Both need a Linux task of their own -- two nk tasks cannot answer
    // syscalls as one Linux task -- but only one of them wants a private
    // table. See `attach_thread`.
    // A vfork child shares memory and nothing else: it needs a Linux task
    // with its own pid and its own descriptor table, exactly as a fork does.
    let as_process = !f.shares_mm || f.vfork;
    let attach = if as_process {
        crate::lkl::attach_process()
    } else {
        crate::lkl::attach_thread(f.parent_pid)
    };
    let pid = match attach {
        Ok(pid) => pid,
        Err(e) => {
            f.pid.store(e, Ordering::Release);
            f.ready.up();
            if as_process && !f.vfork {
                unsafe { paging::destroy_user_address_space(f.ttbr0) };
                crate::sched::drop_user_memory(f.ttbr0);
            }
            if f.vfork {
                let d = crate::sched::take_vfork_done();
                if d != 0 {
                    unsafe { (*(d as *const crate::sync::Semaphore)).up() };
                }
            }
            return;
        }
    };
    crate::sched::bind_linux_pid(pid);
    // The address space this task is *for*, installed before anything writes
    // through a user pointer.
    //
    // A freshly spawned task runs with the kernel's tables (`spawn` sets
    // that, and the switch installs it), and `resume_user` at the bottom of
    // this function is where the child's would otherwise arrive. Everything
    // between the two -- the tid writes below, most of all -- was therefore
    // translating user addresses through tables that have no user half, so
    // every one of them failed and was thrown away by `let _ =`.
    //
    // What that cost: glibc passes `&pd->tid` with CLONE_PARENT_SETTID and
    // reads it back as the thread's own id. Never written, it stayed zero,
    // and glibc's `pthread_rwlock_rdlock` compares an unlocked lock's writer
    // (zero) against it -- so every read lock taken on any thread returned
    // EDEADLK. GLib reports that as "Failed to get RW lock: Resource
    // deadlock avoided", and WebKit's network and web processes died on it.
    //
    // Safe here for the same reason it is safe in `resume_user`: every user
    // address space carries a copy of the kernel's mappings, so the code
    // doing the switching stays mapped across it.
    unsafe {
        crate::arch::set_page_table(f.ttbr0);
    }
    // Both before ready.up(): the creator must not observe the thread until
    // its id is where the caller asked for it, and the thread must not reach
    // user code — where it could exit and clear these words — before then.
    let bytes = (pid as u32).to_le_bytes();
    for addr in [f.set_tid.0, f.set_tid.1] {
        if addr != 0 {
            let _ = crate::uaccess::copy_to_user(addr, &bytes);
        }
    }
    // Before the parent is told the child exists, so the parent cannot close
    // a descriptor between forking and the child copying it.
    // Copying descriptors is the process path only: a thread already has
    // its creator's table, and copying into it would duplicate every entry.
    let inherited = if as_process { crate::lkl::inherit_fds(f.parent_pid) } else { 3 };
    // The child's descriptor table is a copy of the parent's, so the pins
    // for file-backed mappings are in it too; its closes must answer for
    // them the way the parent's do.
    // A child with the parent's 0, 1 and 2 has a console in exactly the way
    // the parent did, and saying otherwise is not a missing feature but a
    // wrong answer: nk's fallback writes descriptor 1 to the UART, so a
    // child's `> file` would be honoured by Linux and then bypassed by nk.
    // That is why `dd of=... ` and `2>/dev/null` in a forked shell had been
    // printing to the console regardless.
    if f.has_console && inherited >= 3 {
        crate::sched::set_has_console(true);
    }
    // The ABI is the parent's image, so it is the child's too.
    crate::sched::set_nethos_abi(f.nethos_abi);
    f.pid.store(pid, Ordering::Release);
    f.ready.up();
    if inherited > 0 && as_process {
        println!("  fork: child {} inherited {} descriptors", pid, inherited);
        for fd in 0..512 {
            let flags = ((f.cloexec[fd / 64] >> (fd % 64)) & 1) as i64;
            crate::lkl::syscall(25, [fd as i64, 2, flags, 0, 0, 0]);
        }
    }

    crate::sched::set_shared_maps(&f.shared);
    if as_process {
        // A forked child inherits dispositions, mask and altstack -- and
        // nothing pending. A thread inherits nothing: it shares the
        // creator's table by sharing its address space.
        crate::sched::install_signal_state(
            f.sig_actions,
            f.sig_mask,
            f.sig_alt_base,
            f.sig_alt_size,
        );
    } else {
        // A thread carries the process's dispositions, taken from the
        // creator now rather than from the snapshot in `Forked`: the
        // creator could have installed a handler between taking the
        // snapshot and this task becoming visible, and a propagation at
        // that moment could not have seen a task that did not exist yet.
        // Reading here closes both orders. The altstack is the thread's
        // own and starts empty, which is what Linux gives a new thread.
        let creator = crate::sched::task_with_linux_pid(f.parent_pid);
        let (actions, mask) = match creator {
            Some(c) => (crate::sched::signal_actions(c), crate::sched::signal_mask(c)),
            None => (f.sig_actions, f.sig_mask),
        };
        crate::sched::install_signal_state(actions, mask, 0, 0);
    }
    // The child runs with the vector unit as its creator left it, whether it
    // is a process copy or a thread; `resume_user`/`enter_user` restores this
    // before the first EL0 instruction.
    crate::sched::fp_install(crate::sched::current_id(), f.fpsimd);
    unsafe {
        crate::arch::set_tls(f.tpidr);
        resume_user(
            &f.frame,
            f.ttbr0,
            crate::sched::kernel_stack_top() as u64,
        )
    }
}

/// # clone(flags, stack, parent_tid, tls, child_tid) -- the thread column
///
/// A thread is the same program in the same address space with a stack of its
/// own. So there is no copy here: the new task takes its creator's `TTBR0`
/// unchanged, and the scheduler is told not to tear that address space down
/// when the thread exits, because the threads it shares with are still in it.
///
/// What arrives in the new thread is the caller's register frame with three
/// things changed -- `x0` is zero, the stack pointer is the one the caller
/// passed, and `TPIDR_EL0` is the thread pointer it passed. glibc puts its
/// whole thread-local area behind that pointer, so a thread with its parent's
/// is a thread sharing its parent's `errno`.
///
/// On aarch64 the argument order is flags, stack, parent_tid, **tls**,
/// child_tid -- the last two are the other way round on x86, and getting it
/// wrong gives a thread whose thread pointer is a pointer to a thread id.
#[cfg(nk_lkl)]
fn thread(frame: &Frame) -> i64 {
    use core::sync::atomic::{AtomicI64, Ordering};
    const CLONE_PARENT_SETTID: u64 = 0x00100000;
    const CLONE_CHILD_CLEARTID: u64 = 0x00200000;
    const CLONE_CHILD_SETTID: u64 = 0x01000000;
    const CLONE_SETTLS: u64 = 0x00080000;

    let flags_arg = frame.x[0];
    let stack = frame.x[1];
    let parent_tid = frame.x[2];
    let tls = frame.x[3];
    let child_tid = frame.x[4];

    if stack == 0 {
        // A thread with no stack of its own would run on its creator's, which
        // is the one thing sharing an address space makes possible and fatal.
        return -22; // -EINVAL
    }

    let ready: &'static crate::sync::Semaphore =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(crate::sync::Semaphore::new(0)));
    crate::sync::track(ready);
    let tid: &'static AtomicI64 =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(AtomicI64::new(0)));

    let mut new = Frame { x: frame.x, elr: frame.elr, spsr: frame.spsr, sp: stack };
    new.x[0] = 0;

    let mine = crate::arch::current_tls();

    let creator = crate::sched::current_id();

    let arg = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(Forked {
        frame: new,
        ttbr0: current_ttbr0(),
        tpidr: if flags_arg & CLONE_SETTLS != 0 { tls } else { mine },
        parent_pid: crate::sched::linux_pid(crate::sched::current_id()),
        shares_mm: true,
        vfork: false,
        has_console: crate::sched::has_console(),
        nethos_abi: crate::sched::nethos_abi(),
        set_tid: (
            if flags_arg & CLONE_PARENT_SETTID != 0 { parent_tid } else { 0 },
            if flags_arg & CLONE_CHILD_SETTID != 0 { child_tid } else { 0 },
        ),
        ready,
        pid: tid,
        // Same address space, same mappings: the thread sees the pool's
        // pages directly, which is the one case where sharing is real
        // rather than copied. No extra pool reference -- the creator's
        // covers the address space, and the thread must not release it.
        // `sys_exit` therefore releases shared maps only for processes.
        shared: alloc::vec::Vec::new(),
        // A thread shares its creator's dispositions the way Linux shares
        // them: they are the process's, and a thread made after an install
        // must see it. The table is per task and `set_signal_action`
        // propagates every write by address space, so the creator's table
        // is the process's current one -- copy it, and do not copy the
        // altstack (Linux gives a new thread none).
        sig_actions: crate::sched::signal_actions(creator),
        sig_mask: crate::sched::signal_mask(creator),
        fpsimd: crate::sched::fp_snapshot(creator),
        sig_alt_base: 0,
        sig_alt_size: 0,
        cloexec: [0u64; 8],
    })) as usize;

    let me = crate::sched::current_id();
    let flags = crate::sync::irq_save();
    let id = crate::sched::spawn("thread", forked_entry, arg);
    crate::sched::set_parent(id, me);
    crate::sched::set_thread(
        id,
        if flags_arg & CLONE_CHILD_CLEARTID != 0 { child_tid } else { 0 },
    );
    unsafe { crate::sync::irq_restore(flags) };

    ready.down();
    let got = tid.load(Ordering::Acquire);
    if got < 0 {
        return got;
    }

    got
}

/// # wait4(pid, status, options, rusage)
///
/// nk's, because nk owns the exit status: a process's status is recorded when
/// it calls `exit`, and Linux's own task is torn down with `do_exit(0)`
/// underneath. Only children of the caller, which is why the scheduler
/// records who forked whom.
#[cfg(nk_lkl)]
fn wait4(pid: i64, status: u64, options: u64) -> i64 {
    const WNOHANG: u64 = 1;
    let me = crate::sched::current_id();
    loop {
        if let Some(id) = crate::sched::finished_child(me, pid) {
            let child = crate::sched::linux_pid(id);
            let code = crate::sched::exit_status(id);
            crate::sched::reap_process(id);
            if status != 0 {
                // A wait status is not an exit code: the low byte says how it
                // died and the second says with what, so a normal exit is the
                // code shifted up by eight. A libc's WEXITSTATUS undoes
                // exactly this and gets nonsense from a plain code.
                let w = ((code as u32 & 0xff) << 8).to_le_bytes();
                if crate::uaccess::copy_to_user(status, &w).is_err() {
                    return crate::uaccess::EFAULT;
                }
            }
            return child;
        }
        if !crate::sched::has_live_child(me, pid) {
            return -10; // -ECHILD
        }
        if options & WNOHANG != 0 {
            return 0;
        }
        // Blocked, not spinning. This waits for as long as the child runs --
        // the init shell's wait on the session supervisor is the whole
        // session -- and a task that is Ready for hours keeps the CPU from
        // ever idling: the emulated machine burns a host core doing nothing
        // but asking whether the child has finished yet. `sys_exit` wakes
        // the parent out of this generic wait (token 0) as it records the
        // status, and a pending signal must interrupt a wait too, so both
        // are tested on the way round.
        if crate::signal::interrupt_pending() {
            return -4; // -EINTR: a signal, not the child, ended the wait
        }
        let flags = crate::sync::irq_save();
        // Predicate: a child of ours is Finished, or a signal is pending and
        //   must interrupt the wait.
        // Protected by: the child's `state` (read here under the runqueue
        //   lock) and the per-task signal mask.
        // Published: `sys_exit` records the status, marks the child Finished
        //   and calls `wake_on(parent, 0)` -- the wake takes the runqueue
        //   lock, so its acquire orders the state store ahead of it.
        // Why the re-check closes it: the old form re-tested with interrupts
        //   masked, which is only protection against *this* CPU's handlers.
        //   A child exiting on another CPU between that test and the block
        //   was refused by `wake_on` (we still read Running) and we then
        //   slept past the exit. Under the runqueue lock the exit is either
        //   visible to this re-check (we never sleep) or it arrives after we
        //   are Blocked and wakes us.
        crate::sched::block_on_cond(flags, 0, || {}, || {
            // The runqueue lock is held here; `finished_child` would take it
            // again and deadlock on a single CPU. The locked variant reads
            // the same table and asks the caller to hold what it already
            // does.
            unsafe { crate::sched::finished_child_locked(me, pid) }.is_some()
                || crate::signal::interrupt_pending()
        });
    }
}

/// `execve`, or -ENOSYS on a build with no Linux to read the file through.
#[cfg(nk_lkl)]
fn exec(path: u64, argv: u64, envp: u64) -> i64 {
    sys_execve(path, argv, envp)
}

#[cfg(not(nk_lkl))]
fn exec(_path: u64, _argv: u64, _envp: u64) -> i64 {
    println!("  execve needs a filesystem to read from");
    -38
}

/// How many arguments and environment entries `execve` will carry, and how
/// many bytes of them. Linux's own limits are a quarter of the stack rlimit
/// and 32 pages per string; these are smaller because nk's initial stack is
/// one fixed allocation and everything has to fit in it beside the vector.
#[cfg(nk_lkl)]
const MAX_ARGS: usize = 64;
#[cfg(nk_lkl)]
const MAX_ARG_BYTES: usize = 16 * 1024;

/// Copy a NULL-terminated array of user string pointers.
///
/// This is the shape the marshalling table cannot describe: the argument is a
/// pointer to an array of pointers, each into user memory, with no length
/// anywhere -- the array ends at a NULL and each string at a NUL. So it is
/// walked, one `copy_from_user` per pointer and one per string, with a bound
/// on both counts because a process that asks for a million arguments should
/// be told no rather than answered.
#[cfg(nk_lkl)]
fn copy_string_array(mut at: u64, budget: &mut usize) -> Result<alloc::vec::Vec<alloc::vec::Vec<u8>>, i64> {
    use alloc::vec::Vec;
    let mut out: Vec<Vec<u8>> = Vec::new();
    if at == 0 {
        return Ok(out); // a null argv is an empty one, and legal
    }
    loop {
        if out.len() >= MAX_ARGS {
            return Err(-7); // -E2BIG
        }
        let mut word = [0u8; 8];
        crate::uaccess::copy_from_user(&mut word, at)?;
        let ptr = u64::from_le_bytes(word);
        if ptr == 0 {
            return Ok(out);
        }
        let mut buf = alloc::vec![0u8; (*budget).min(crate::uaccess::PATH_MAX)];
        let len = crate::uaccess::copy_cstr_from_user(ptr, &mut buf)?;
        if len > *budget {
            return Err(-7);
        }
        *budget -= len;
        buf.truncate(len);
        out.push(buf);
        at += 8;
    }
}

/// # execve(path, argv, envp)
///
/// nk's, not Linux's. LKL has no user space to exec into -- forwarding this
/// would ask Linux to replace an address space it does not have -- so nk reads
/// the file through Linux's VFS and does the replacing itself.
///
/// The order is the whole difficulty. argv and envp live in the address space
/// being replaced, so they are copied first. The new address space is built
/// second, and only when it is complete and `TTBR0` points at it is the old
/// one torn down: the kernel is mapped through the same tables, so a process
/// that frees its own address space before leaving it does not survive to
/// report the mistake.
///
/// On success this does not return. On failure it returns an errno and the
/// caller carries on with everything it had, which is what execve promises.
#[cfg(nk_lkl)]
fn sys_execve(path: u64, argv: u64, envp: u64) -> i64 {
    use crate::uaccess::{self, PATH_MAX};

    let mut name = alloc::vec![0u8; PATH_MAX];
    let len = match uaccess::copy_cstr_from_user(path, &mut name) {
        Ok(n) => n,
        Err(e) => return e,
    };
    name.truncate(len + 1); // keep the NUL: Linux's openat expects one

    let mut budget = MAX_ARG_BYTES;
    let args = match copy_string_array(argv, &mut budget) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let envs = match copy_string_array(envp, &mut budget) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let Ok(cpath) = core::ffi::CStr::from_bytes_with_nul(&name) else {
        return -22; // -EINVAL: an embedded NUL is not a path
    };
    let bytes = match crate::lkl::read_file(cpath) {
        Ok(b) => b,
        Err(e) => {
            // Named, because "execve returned an errno" is the least useful
            // thing a kernel can say when a program will not start.
            println!("  execve: cannot read {:?}: {}", cpath, e);
            return e;
        }
    };

    // A script names the program that can read it, and that program is what
    // is actually loaded. execve of a script with no argv at all is not a
    // thing a libc does, but argv[0] is what gets replaced by the script's
    // path, so there has to be one to replace.
    let mut args = args;
    if args.is_empty() {
        args.push(name[..name.len() - 1].to_vec());
    }
    let (bytes, args) =
        match follow_interpreters(name[..name.len() - 1].to_vec(), bytes, args) {
            Ok(both) => both,
            Err(e) => return e,
        };
    // Borrowed views, because the loader wants slices and the owners are the
    // vectors above -- which must outlive the stack that is built from them.
    let argv: alloc::vec::Vec<&[u8]> = args.iter().map(|v| v.as_slice()).collect();
    let envv: alloc::vec::Vec<&[u8]> = envs.iter().map(|v| v.as_slice()).collect();

    let p = match load(&bytes, &argv, &envv) {
        Ok(p) => p,
        // The image was rejected before anything was replaced, so the caller
        // still has everything it had. That is what makes a failed execve
        // survivable and why the loader validates before it allocates.
        Err(why) => {
            println!("  execve: {}", why);
            return -8; // -ENOEXEC
        }
    };

    let rc = crate::lkl::syscall(248, [0; 6]);
    if rc < 0 {
        unsafe { paging::destroy_user_address_space(p.ttbr0) };
        crate::sched::drop_user_memory(p.ttbr0);
        return rc;
    }

    let old = current_ttbr0();
    // A vfork child is running in its parent's address space. Replacing its
    // image means leaving that address space, not destroying it -- and not
    // releasing its shared mappings either, which are the parent's.
    let borrowed = crate::sched::shares_mm_current();
    // The old address space's shared mappings end with it: write back and
    // drop the pool references before the tables are destroyed. The pages
    // themselves stay in the pool for other holders; destroying the tables
    // must not free them, so this goes through `unmap_user_nofree`.
    if borrowed {
        crate::sched::set_shared_maps(&[]);
    } else {
        for m in crate::sched::take_shared_maps() {
            unsafe { crate::paging::unmap_user_nofree(old, m.start, m.len) };
            if m.anonymous {
                crate::shm::release_anon(m.region);
            } else {
                crate::shm::release(m.region, m.fd);
            }
        }
    }
    unsafe {
        // The new tables first, then the old ones freed. The other order
        // unmaps the kernel from under the code doing the freeing.
        crate::arch::set_page_table(p.ttbr0);
        if !borrowed {
            paging::destroy_user_address_space(old);
            crate::sched::drop_user_memory(old);
        }
        // The thread pointer belonged to the program that is gone. A libc
        // sets its own before it needs one; leaving the old value would give
        // the new program a pointer into memory that has just been freed.
        crate::arch::set_tls(0);
    }
    crate::sched::set_user_memory_for(p.ttbr0, p.brk, USER_HIGH_TOP);
    // This address space is the task's own from here, whoever's it was
    // before, so its exit must tear this one down.
    crate::sched::own_mm_current();
    // And the vfork parent may run again: the borrowed address space is
    // no longer being used by anybody but its owner.
    let done = crate::sched::take_vfork_done();
    if done != 0 {
        unsafe { (*(done as *const crate::sync::Semaphore)).up() };
    }
    // Linux's `flush_signal_handlers`: an image that replaces the program
    // must not keep handlers that point into a program that is gone. The
    // wrapper script two execs back had a SIGCHLD handler at an address the
    // new image may not even map.
    crate::sched::reset_signal_handlers(crate::sched::current_id());
    println!("  execve: replaced this process with {} bytes at {:#x} path {:?}", bytes.len(), p.entry, cpath);
    unsafe { enter_user_fresh(p.entry, p.stack, p.ttbr0, crate::sched::kernel_stack_top() as u64) }
}

/// # mprotect(addr, len, prot)
///
/// Nothing gets both write and execute. A libc asks for this to make its
/// relocated GOT read-only after startup -- GNU_RELRO -- which is the one
/// thing standing between a static binary and running here.
fn sys_mprotect(addr: u64, len: u64, prot: u64) -> i64 {
    const PROT_WRITE: u64 = 2;
    const PROT_EXEC: u64 = 4;
    let (_, brk_min, _) = crate::sched::user_memory();
    if brk_min == 0 || len == 0 || addr & (PAGE as u64 - 1) != 0 {
        return -22; // -EINVAL
    }
    let size = len.div_ceil(PAGE as u64) * PAGE as u64;
    let top = if addr >= USER_HIGH_BASE { USER_HIGH_TOP } else { USER_STACK_TOP };
    if addr >= top || size > top - addr {
        return -22;
    }
    let exec = prot & PROT_EXEC != 0;
    let writable = prot & PROT_WRITE != 0;

    if exec && writable {
        return -13; // -EACCES, and nk will not make an exception
    }
    // Reservations first: pages that have not been touched have nothing to
    // change, and the permissions they will be faulted in with are the only
    // record of this call that will survive to matter.
    let Some(reserved) = crate::reserve::protect(addr, size, writable, exec, prot == 0) else {
        return -12;
    };
    if prot == 0 {
        if unsafe { paging::protect_user_none(current_ttbr0(), addr, size) } {
            return 0;
        }
        return if reserved { 0 } else { -12 };
    }
    if unsafe { !paging::protect_user(current_ttbr0(), addr, size, exec, writable) } {
        // A hole is -ENOMEM, which is Linux's answer -- unless the hole is a
        // reservation, where there is nothing to change yet and the change
        // has already been recorded for when there is.
        return if reserved { 0 } else { -12 };
    }
    0
}

/// # msync(addr, len, flags)
///
/// Write back every dirty shared region. The address range is accepted and
/// not filtered on: every region is small, writes complete before the call
/// returns, and `MS_ASYNC` vs `MS_SYNC` differ in when the write completes,
/// which here is already. An `msync` over no shared mappings is success.
fn sys_msync(_addr: u64, len: u64, _flags: u64) -> i64 {
    if len == 0 {
        return -22; // -EINVAL
    }
    crate::shm::msync_all(-1)
}

/// # set_tid_address(ptr)
///
/// Answered here rather than forwarded because the pointer is a user address
/// and Linux, flat, would store it and later write through it into whatever
/// happens to be at that offset in the kernel. Nothing writes to it: the
/// address is where Linux would clear the tid when the thread dies, and nk
/// has no threads to clear it for yet. The *return* is what the caller
/// actually uses, and it is the real tid.
fn sys_set_tid_address() -> i64 {
    crate::sched::linux_pid(crate::sched::current_id())
}

/// # prlimit64(pid, resource, new, old)
///
/// Only the stack limit, and only reading it. A libc asks so it knows how far
/// it may let the stack grow before it should guard; telling it the truth --
/// that the stack is exactly what nk mapped -- is better than -ENOSYS, which
/// makes it assume a default it will not get.
#[no_mangle]
pub extern "C" fn nk_stack_top(pid: i64) -> u64 {
    if crate::sched::task_with_linux_pid(pid).is_some() {
        USER_STACK_TOP
    } else {
        0
    }
}

fn sys_prlimit64(pid: u64, resource: u64, new: u64, old: u64) -> i64 {
    const RLIMIT_STACK: u64 = 3;
    const RLIM_INFINITY: u64 = !0;
    let me = crate::sched::linux_pid(crate::sched::current_id()) as u64;
    if resource != RLIMIT_STACK || (pid != 0 && pid != me) {
        return forward(261, &[pid, resource, new, old, 0, 0]);
    }
    if new != 0 {
        return -1; // -EPERM: nothing may raise a limit here
    }
    if old == 0 {
        return 0;
    }
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&(USER_STACK_SIZE as u64).to_le_bytes());
    buf[8..].copy_from_slice(&RLIM_INFINITY.to_le_bytes());
    if crate::uaccess::copy_to_user(old, &buf).is_err() {
        return crate::uaccess::EFAULT;
    }
    0
}

fn current_ttbr0() -> u64 {
    crate::arch::current_page_table()
}

/// ELR of the EL0 caller is threaded through from the dispatch frame.
pub fn current_ttbr0_pub() -> u64 {
    current_ttbr0()
}

/// Back a user range with fresh zeroed pages. All or nothing: a partial
/// mapping would leave the process holding an address range that faults half
/// way through, which is worse than being told no.
fn map_anonymous(at: u64, len: u64) -> bool {
    let ttbr0 = current_ttbr0();
    let mut done = 0;
    while done < len {
        let Some(page) = frames::alloc() else {
            unsafe { crate::paging::unmap_user(ttbr0, at, done) };
            return false;
        };
        unsafe {
            core::ptr::write_bytes(page, 0, PAGE);
            paging::map_user_permissions(ttbr0, at + done, page as u64, PAGE as u64, false, true);
        }
        done += PAGE as u64;
    }
    true
}

pub fn sys_exit(status: i32) -> ! {
    let me = crate::sched::current_id();
    crate::println!("sys_exit called by task {} lpid {} status {}", me, crate::sched::linux_pid(me), status);
    #[cfg(nk_lkl)]
    {
        // What pthread_join is waiting for.
        //
        // CLONE_CHILD_CLEARTID asks the kernel to zero a word in the thread's
        // descriptor when it dies and wake anything sleeping on it. That is
        // the entire mechanism behind a join: glibc does not poll, it futexes
        // on that word, and a kernel that forgets this half gives a join that
        // never returns and no other symptom.
        let tidptr = crate::sched::clear_child_tid();
        if tidptr != 0 {
            let _ = crate::uaccess::copy_to_user(tidptr, &0u32.to_le_bytes());
            crate::futex::futex(tidptr, 1, u32::MAX, 0, 0, u32::MAX);
        }
        crate::futex::forget(crate::sched::current_id());
    }
    // A thread exiting is not the process exiting, and saying so would be a
    // lie in the middle of a program that is still running.
    // A vfork child that never exec'd is leaving an address space it does
    // not own, which is a thread's exit rather than a process's -- and its
    // parent has been waiting all along to have that address space back.
    #[cfg(nk_lkl)]
    {
        let done = crate::sched::take_vfork_done();
        if done != 0 {
            unsafe { (*(done as *const crate::sync::Semaphore)).up() };
        }
    }
    #[cfg(nk_lkl)]
    let thread = crate::sched::clear_child_tid() != 0 || crate::sched::shares_mm_current();
    #[cfg(not(nk_lkl))]
    let thread = false;
    // A process's shared mappings end with it: write back and drop the pool
    // references. A thread's do not -- the address space is still in use by
    // the threads it shares with, and the creator's exit covers them.
    //
    // And the parent learns of it by signal as well as by `wait4`: a child
    // that is not ignored posts `SIGCHLD`, which is what makes a shell
    // print "done" without polling and what wakes a blocked `waitpid`.
    #[cfg(nk_lkl)]
    if !thread {
        for m in crate::sched::take_shared_maps() {
            unsafe { crate::paging::unmap_user_nofree(current_ttbr0(), m.start, m.len) };
            if m.anonymous {
                crate::shm::release_anon(m.region);
            } else {
                crate::shm::release(m.region, m.fd);
            }
        }
        let me = crate::sched::current_id();
        let parent = crate::sched::task_parent(me);
        if parent != 0 && parent != me {
            let act = crate::sched::signal_action(parent, crate::signal::SIGCHLD);
            if act.handler != crate::signal::SIG_IGN {
                crate::sched::add_signal_pending(parent, 1 << (crate::signal::SIGCHLD - 1));
                crate::sched::set_signal_source(
                    parent,
                    crate::signal::SIGCHLD,
                    128, // SI_KERNEL: from the kernel, not a kill
                    crate::sched::linux_pid(me) as i32,
                );
                // Only if the parent is in a wait of nk's own -- a parent
                // blocked inside Linux must be left where it is, and the
                // signal stays pending for when it next returns to EL0.
                // `wait4` blocks on this same generic token, so this is also
                // what ends the wait for a parent that is in one.
                crate::sched::wake_on(parent, 0);
                println!(
                    "  sigchld: delivered to task {} lpid {} handler {:#x} for lpid {}",
                    parent,
                    crate::sched::linux_pid(parent),
                    act.handler,
                    crate::sched::linux_pid(me)
                );
            } else {
                // Ignoring SIGCHLD says nothing about `wait4`: a parent may
                // ignore the signal and still be blocked in the wait, and
                // with no wake here it would sleep past its child's exit
                // for ever. The status is recorded either way; only the
                // signal is suppressed.
                crate::sched::wake_on(parent, 0);
                println!(
                    "  sigchld: parent task {} lpid {} has SIG_IGN for lpid {}",
                    parent,
                    crate::sched::linux_pid(parent),
                    crate::sched::linux_pid(me)
                );
            }
        } else {
            println!("  sigchld: no parent for lpid {} (parent task {})",
                     crate::sched::linux_pid(me), parent);
        }
    }
    if !thread {
        println!();
        println!("  the process (lpid {}) exited with status {}", crate::sched::linux_pid(crate::sched::current_id()), status);
        // The handler ran and returned, or it never ran at all: either way
        // the next line out of EL0 names the cause. A SIGABRT (-6) with no
        // preceding fault line means the process killed itself -- glibc's
        // default for a signal whose handler returned without a restorer,
        // or an abort() after a failed assertion in the handler path.
        if status == -6 {
            println!("  (SIGABRT: check the handler's return path -- x30/restorer -- and what it ran)");
        }
    }
    #[cfg(nk_lkl)]
    {
        crate::sched::set_exit_status(status);
        crate::sched::exit_current()
    }
    #[cfg(not(nk_lkl))]
    crate::stop()
}

/// Whether an initrd already put a program at `/nk-init`.
///
/// If it did, nk must not write over it; if it did not, nk has to seed
/// something before there is anything to load. These are two different
/// questions from the one below and conflating them cost a debugging round:
/// a `--init` binary is not the fixture *and* still has to be seeded.
#[cfg(nk_lkl)]
static mut INIT_FROM_INITRD: bool = false;

/// Whether the program at `/nk-init` is nk's own fixture.
///
/// The self-checks that follow a run are claims about the fixture -- that it
/// exits with its own Linux pid, that it spun long enough to be preempted --
/// and they are not true of an arbitrary binary. `--init` settles this at
/// build time and an initrd settles it at run time.
#[cfg(nk_lkl)]
pub fn ran_fixture() -> bool {
    !cfg!(nk_init) && !unsafe { INIT_FROM_INITRD }
}

/// Unpack a cpio archive into Linux's rootfs, and return how many entries it
/// held.
///
/// This is what makes a program on nk something other than part of the
/// kernel. Until now `/nk-init` was seeded from a fixture inside `nk.bin`,
/// which is fine for a program written to be a test and useless for a
/// userland: a real one is many files, and they have to come from outside.
///
/// Directories first is not assumed. A cpio archive usually lists a directory
/// before its contents and is not required to, so each file's parents are
/// created as it goes -- which also means the same directory is met more than
/// once as a matter of course, and `mkdir` treats that as success.
#[cfg(nk_lkl)]
pub fn unpack_initrd(archive: &[u8]) -> Result<usize, &'static str> {
    use alloc::vec::Vec;
    let mut count = 0;
    crate::cpio::each(archive, |e| {
        // The names in an archive are relative ("bin/sh"); the rootfs wants
        // them absolute, and a leading "./" is how most archives spell the
        // same thing.
        let name = e.name.strip_prefix("./").unwrap_or(e.name);
        if name.is_empty() {
            return;
        }
        let mut path = Vec::with_capacity(name.len() + 2);
        path.push(b'/');
        path.extend_from_slice(name.as_bytes());
        path.push(0);

        // Every parent, in order, before the entry itself.
        for i in 1..path.len() - 1 {
            if path[i] != b'/' {
                continue;
            }
            let saved = path[i];
            path[i] = 0;
            let _ = crate::lkl::mkdir(cstr(&path[..=i]), 0o755);
            path[i] = saved;
        }

        let c = cstr(&path);
        // Whether the archive supplies the init has to be settled here,
        // while the rootfs is still empty. Asking later -- "does /nk-init
        // exist?" -- gets the wrong answer the moment nk has seeded its own
        // fixture, and the second process to start then reports that the
        // first one's file came from an archive that was never there.
        if name == "nk-init" && e.is_file() {
            unsafe { INIT_FROM_INITRD = true };
        }
        let ok = if e.is_dir() {
            crate::lkl::mkdir(c, e.perms() as i64).is_ok()
        } else if e.is_file() {
            crate::lkl::write_file_mode(c, e.data, e.perms() as i64).is_ok()
        } else if e.is_symlink() {
            // newc stores the target without a NUL. Keep it relative so a
            // bin/cat -> busybox applet resolves beside bin/busybox.
            if e.data.contains(&0) || e.data.is_empty() {
                false
            } else {
                let mut target = e.data.to_vec();
                target.push(0);
                crate::lkl::syscall(36, [target.as_ptr() as i64, -100,
                    c.as_ptr() as i64, 0, 0, 0]) == 0 // symlinkat
            }
        } else {
            // Devices and fifos. Nothing needs them yet and
            // pretending to create one by making an empty file would be
            // worse than leaving it out visibly.
            false
        };
        if ok {
            count += 1;
        }
    })?;
    Ok(count)
}

/// A NUL-terminated slice as a `CStr`, for paths built a byte at a time.
#[cfg(nk_lkl)]
fn cstr(bytes: &[u8]) -> &core::ffi::CStr {
    core::ffi::CStr::from_bytes_until_nul(bytes).unwrap_or(c"/")
}

/// Load the bootstrap ELF through Linux's existing rootfs and VFS. The file
/// is seeded from the kernel image until a persistent root disk is attached.
#[cfg(nk_lkl)]
pub fn spawn_from_rootfs() -> Result<Process, &'static str> {
    extern "C" {
        static __user_elf_start: u8;
        static __user_elf_end: u8;
    }
    let start = &raw const __user_elf_start;
    let size = (&raw const __user_elf_end as usize) - start as usize;
    #[cfg(not(nk_init))]
    let fixture = unsafe { core::slice::from_raw_parts(start, size) };
    // An externally built binary, when `run-kernel.sh --init` supplied one.
    // It travels inside the kernel image because the rootfs is memory-backed
    // and there is no disk to read it from yet.
    #[cfg(nk_init)]
    let fixture: &[u8] = {
        let _ = (start, size);
        include_bytes!(concat!(env!("OUT_DIR"), "/nk-init.bin"))
    };
    // An initrd's /nk-init wins. Seeding the fixture over it would throw away
    // the whole point of having unpacked an archive, and doing that silently
    // is worse than not supporting it.
    let bytes = if !unsafe { INIT_FROM_INITRD } {
        crate::lkl::write_file(c"/nk-init", fixture).map_err(|_| "rootfs write failed")?;
        let b = crate::lkl::read_file(c"/nk-init").map_err(|_| "rootfs read failed")?;
        if b != fixture {
            return Err("rootfs round trip differs");
        }
        println!(
            "  rootfs: /nk-init read back through Linux VFS ({} bytes)",
            b.len()
        );
        b
    } else {
        let b = crate::lkl::read_file(c"/nk-init").map_err(|_| "rootfs read failed")?;
        println!("  rootfs: /nk-init came from the initrd ({} bytes)", b.len());
        b
    };
    // The init may be a script, and usually is. Following the `#!` here as
    // well as in execve means an initrd can ship `#!/bin/busybox sh` and no
    // part of nk has to know what busybox is.
    let (bytes, args) = follow_interpreters(
        b"/nk-init".to_vec(),
        bytes,
        alloc::vec![b"/nk-init".to_vec()],
    )
    .map_err(|_| "the init names an interpreter that cannot be read")?;
    let argv: alloc::vec::Vec<&[u8]> = args.iter().map(|v| v.as_slice()).collect();
    load(&bytes, &argv, &[b"PATH=/bin", b"HOME=/"])
}

/// Follow `#!` lines until something is an ELF.
///
/// An init is usually a shell script, and a script is not a thing a loader can
/// enter: the file names the program that can read it. Linux resolves this in
/// `binfmt_script`, and the rules are its rules -- the first line only, up to
/// a fixed length, the first word is the interpreter and *everything after it
/// is one argument* however many spaces it contains, and the script's own path
/// is inserted as the interpreter's first argument while `argv[0]` is
/// discarded.
///
/// That last part is why busybox as `/nk-init` exits 127 without this: it
/// chooses its applet from `basename(argv[0])`, nk passes the path it loaded,
/// and there is no applet called `nk-init`. `#!/bin/busybox sh` says what was
/// meant.
///
/// Bounded at four, as Linux is. A script whose interpreter is itself is not
/// an error anyone can act on, and following it is a loop.
#[cfg(nk_lkl)]
fn follow_interpreters(
    mut path: alloc::vec::Vec<u8>,
    mut bytes: alloc::vec::Vec<u8>,
    mut args: alloc::vec::Vec<alloc::vec::Vec<u8>>,
) -> Result<(alloc::vec::Vec<u8>, alloc::vec::Vec<alloc::vec::Vec<u8>>), i64> {
    use alloc::vec::Vec;
    /// Linux's own BINPRM_BUF_SIZE. A longer first line is not truncated into
    /// something plausible; it is refused, because a truncated interpreter
    /// path is a different program.
    const LINE: usize = 256;

    for _ in 0..4 {
        if bytes.len() < 2 || &bytes[..2] != b"#!" {
            return Ok((bytes, args));
        }
        let end = bytes.iter().take(LINE).position(|&c| c == b'\n').ok_or(-8i64)?;
        let line = &bytes[2..end];
        let line = &line[line.iter().take_while(|c| **c == b' ' || **c == b'\t').count()..];
        if line.is_empty() {
            return Err(-8); // -ENOEXEC: "#!" and nothing to run
        }
        let split = line.iter().position(|&c| c == b' ' || c == b'\t').unwrap_or(line.len());
        let interp = line[..split].to_vec();
        let rest = line[split..].iter().copied().skip_while(|c| *c == b' ' || *c == b'\t')
            .collect::<Vec<u8>>();
        let rest = match rest.iter().rposition(|c| *c != b' ' && *c != b'\t') {
            Some(last) => rest[..=last].to_vec(),
            None => Vec::new(),
        };

        // argv[0] is the interpreter, then its one optional argument, then the
        // script's path, then whatever the caller passed after argv[0].
        let mut next: Vec<Vec<u8>> = alloc::vec![interp.clone()];
        if !rest.is_empty() {
            next.push(rest);
        }
        next.push(path.clone());
        next.extend(args.into_iter().skip(1));
        args = next;

        let mut c = interp.clone();
        c.push(0);
        let name = core::ffi::CStr::from_bytes_with_nul(&c).map_err(|_| -22i64)?;
        bytes = crate::lkl::read_file(name)?;
        path = interp;
    }
    Err(-36) // -ENAMETOOLONG, which is what Linux returns for too many levels
}

/// Build a fresh address space around an ELF image and the arguments it is to
/// start with.
///
/// Everything a process is: its mapped segments, a stack with argc/argv/envp
/// and the auxiliary vector on it, and the address at which to begin. It does
/// not touch the *current* address space, which is what lets `execve` build
/// the replacement before tearing down what it replaces -- the order matters,
/// because the kernel is mapped through the same tables and the process
/// cannot survive unmapping itself half way through.
#[cfg(nk_lkl)]
pub fn load(
    bytes: &[u8],
    args: &[&[u8]],
    envs: &[&[u8]],
) -> Result<Process, &'static str> {
    let image = crate::elf::parse(bytes, USER_BASE, USER_MMAP_TOP)?;
    // Read before anything is mapped, because it decides which interface the
    // program will be answered on for the rest of its life. An interpreted
    // binary takes its ABI from the executable, not from the loader: `ld.so`
    // is machinery, and what it goes on to run is the program.
    let abi = image.abi;
    if image.segments.iter().any(|s| s.address < 0x0a20_0000 && s.address+s.memsz as u64 > 0x0800_0000) {
        return Err("ELF overlaps kernel device mappings");
    }
    let ttbr0 = paging::new_address_space();
    if let Err(e) = map_elf_segments(ttbr0, &image, bytes) {
        unsafe { paging::destroy_user_address_space(ttbr0); }
        return Err(e);
    }
    let mut entry = image.entry;
    let mut interpreter_base = 0;
    if let Some(path) = &image.interpreter {
        let result = (|| {
            let bytes = crate::lkl::read_file(core::ffi::CStr::from_bytes_with_nul(path).map_err(|_| "interpreter path")?)
                .map_err(|_| "cannot read ELF interpreter")?;
            let loader = crate::elf::parse_at(&bytes, USER_MMAP_TOP, USER_STACK_TOP-USER_STACK_SIZE as u64, USER_MMAP_TOP)?;
            if loader.interpreter.is_some() { return Err("recursive ELF interpreter"); }
            map_elf_segments(ttbr0, &loader, &bytes)?;
            Ok((loader.entry, loader.load_bias))
        })();
        match result { Ok((pc, bias)) => { entry = pc; interpreter_base = bias; },
            Err(e) => { unsafe { paging::destroy_user_address_space(ttbr0); } return Err(e); } }
    }
    let Some(stack) = alloc_user_stack() else {
        unsafe { paging::destroy_user_address_space(ttbr0); }
        return Err("no memory for the ELF stack");
    };
    unsafe { core::ptr::write_bytes(stack, 0, USER_STACK_SIZE) };
    let sp = match build_initial_stack(stack, &image, interpreter_base, args, envs) {
        Ok(sp) => sp,
        Err(e) => {
            unsafe {
                for off in (0..USER_STACK_SIZE).step_by(PAGE) { frames::free(stack.add(off)); }
                paging::destroy_user_address_space(ttbr0);
            }
            return Err(e);
        }
    };
    unsafe {
        paging::map_user(
            ttbr0,
            USER_STACK_TOP - USER_STACK_SIZE as u64,
            stack as u64,
            USER_STACK_SIZE as u64,
            false,
        );
        map_trampoline(ttbr0);
        crate::arch::tlb_flush_all();
    }
    apply_relocations(ttbr0, &image)?;
    println!(
        "  ELF: {} PT_LOAD segment(s), {} relocations, entry {:#x}, zero-filled BSS",
        image.segments.len(),
        image.relas.len(),
        image.entry
    );
    // Static TLS: copy the template into fresh pages and map them where the
    // program headers say. musl's `_start` reads `%fs:0x28` (the stack guard)
    // before any syscall, so a missing template is a fault at an address
    // that names no mapping -- not a TLS error, because no TLS exists yet.
    //
    // x86 only. aarch64 libcs install their own thread pointer and never read
    // it before their own setup runs, and the template lives *inside* a
    // PT_LOAD of an ordinary non-PIE binary -- remapping fresh pages over it
    // would replace the image's own data with an uninitialized copy.
    // The heap starts on the first page boundary past everything the image
    // asked for, so growing it can never land on the program's own bss.
    let mut brk = image
        .segments
        .iter()
        .map(|s| s.address + s.memsz as u64)
        .max()
        .unwrap_or(USER_BASE)
        .div_ceil(PAGE as u64)
        * PAGE as u64;
    // x86 only. aarch64 libcs install their own thread pointer and never read
    // it before their own setup runs.
    //
    // On x86-64 musl's `_start` reads `%fs:0x28` (the stack guard) before any
    // syscall could set the base, so a thread pointer has to exist at entry.
    // It is built in pages of its own, at the initial break, and the break
    // moves past them. It must *not* be mapped over the template's own
    // address: the template lives inside a PT_LOAD (busybox: .tdata is the
    // first thing in its RW segment), so fresh pages there replaced the
    // image's data with zeroes -- the same fault 3ab5a7c fixed on aarch64,
    // seen here as a NULL read through a global glibc had already set.
    //
    // Variant 2: the block ends at TP, TP is the TCB, and the TCB's first
    // word points at itself (`%fs:0` is how libc finds it). glibc replaces
    // all of this with its own block through ARCH_SET_FS; musl uses it until
    // it does the same. `tls_addr` carries TP itself.
    let (tls_addr, tls_filesz, tls_memsz) = if !cfg!(target_arch = "x86_64") {
        (0, 0, 0)
    } else {
        match &image.tls {
            Some(t) => {
                let align = t.align.max(64);
                let block = (t.memsz as u64).div_ceil(align) * align;
                // The TCB: glibc's is ~0x700 bytes; two pages of room past TP
                // covers it and the stack guard at 0x28.
                let total = block + 2 * PAGE as u64;
                let pages = total.div_ceil(PAGE as u64);
                let base = brk;
                let tp = base + block;
                for i in 0..pages {
                    let page = frames::alloc().ok_or("no memory for TLS")?;
                    unsafe { core::ptr::write_bytes(page, 0, PAGE) };
                    // The template sits at the block's start; copy whatever
                    // of its initialized bytes fall in this page.
                    let page_va = base + i * PAGE as u64;
                    let init_end = base + t.filesz.min(t.memsz) as u64;
                    let lo = page_va.max(base);
                    let hi = (page_va + PAGE as u64).min(init_end);
                    if lo < hi {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                bytes.as_ptr().add(t.offset + (lo - base) as usize),
                                page.add((lo - page_va) as usize),
                                (hi - lo) as usize,
                            );
                        }
                    }
                    if (page_va..page_va + PAGE as u64).contains(&tp) {
                        unsafe { *(page.add((tp - page_va) as usize) as *mut u64) = tp };
                    }
                    unsafe {
                        paging::map_user_permissions(ttbr0, page_va, page as u64, PAGE as u64, false, true);
                    }
                }
                brk = base + pages * PAGE as u64;
                (tp, t.filesz, t.memsz)
            }
            None => (0, 0, 0),
        }
    };
    Ok(Process { ttbr0, entry, stack: sp, brk, tls_addr, tls_filesz, tls_memsz, abi })
}

#[cfg(nk_lkl)]
fn map_elf_segments(ttbr0: u64, image: &crate::elf::Image, bytes: &[u8]) -> Result<(), &'static str> {
    for s in &image.segments {
        let base = s.address & !(PAGE as u64 - 1);
        let end = (s.address + s.memsz as u64).div_ceil(PAGE as u64) * PAGE as u64;
        for va in (base..end).step_by(PAGE) {
            let page = frames::alloc().ok_or("no memory for ELF")?;
            // Zero first, always. A PT_LOAD's memsz runs past its filesz --
            // that tail is the .bss -- and a frame handed back by the
            // allocator holds whatever the last user of it left there. The
            // fault this caused was a long way from here: glibc's exit path
            // walks a btree rooted in .bss, so a program that touched none of
            // its own uninitialised data still died on the way out.
            unsafe { core::ptr::write_bytes(page, 0, PAGE) };
            let lo = va.max(s.address);
            let hi = (va + PAGE as u64).min(s.address + s.filesz as u64);
            if hi > lo {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr().add(s.offset + (lo - s.address) as usize),
                        page.add((lo - va) as usize),
                        (hi - lo) as usize,
                    );
                }
            }
            if s.executable {
                unsafe {
                    publish_code(page, PAGE);
                }
            }
            unsafe {
                paging::map_user_permissions(
                    ttbr0,
                    va,
                    page as u64,
                    PAGE as u64,
                    s.executable,
                    s.writable,
                );
            }
        }
    }
    Ok(())
}

/// Apply RELATIVE relocations after the segments are mapped.
#[cfg(nk_lkl)]
fn apply_relocations(ttbr0: u64, image: &crate::elf::Image) -> Result<(), &'static str> {
    if image.relas.is_empty() {
        return Ok(());
    }
    // The targets are user virtual addresses, so the writes happen with that
    // address space current. Each RELATIVE entry stores bias + addend; the
    // value already in memory is the link-time address and is discarded.
    let cur = crate::arch::current_page_table();
    unsafe { crate::arch::set_page_table(ttbr0) };
    for r in &image.relas {
        unsafe { core::ptr::write_volatile(r.address as *mut u64, r.addend) };
    }
    unsafe {
        crate::arch::set_page_table(cur);
        crate::arch::tlb_flush_all();
    }
    Ok(())
}

/// Sixteen bytes for AT_RANDOM, from which a libc takes its stack guard.
///
/// GRND_INSECURE, and it is not optional. Plain `getrandom` *blocks* until
/// the CRNG is seeded, and on a machine whose only entropy is a virtual timer
/// it may never be: the first attempt deadlocked the boot thread inside Linux
/// with every other task idle, which is what the watchdog exists to report.
/// GRND_INSECURE is Linux's own answer to exactly this -- bytes now, from a
/// pool that says it is not yet trustworthy.
#[cfg(nk_lkl)]
fn stack_seed() -> Result<[u8; 16], &'static str> {
    const GRND_INSECURE: i64 = 0x0004;
    let mut random = [0u8; 16];
    let args = [random.as_mut_ptr() as i64, random.len() as i64, GRND_INSECURE, 0, 0, 0];
    if crate::lkl::syscall(278, args) != random.len() as i64 {
        return Err("getrandom did not fill the stack guard");
    }
    Ok(random)
}

/// Without Linux there is no generator, and the virtual counter is the only
/// thing on this machine that differs between one boot and the next. It is
/// not entropy and is not claimed to be; it is here so the standalone smoke
/// test gets a stack of the same *shape*, which is what it is checking.
#[cfg(not(nk_lkl))]
fn stack_seed() -> Result<[u8; 16], &'static str> {
    let mut random = [0u8; 16];
    for (i, chunk) in random.chunks_mut(8).enumerate() {
        let t = crate::timer::ticks();
        chunk.copy_from_slice(&(t ^ (0x9e37_79b9_7f4a_7c15u64.wrapping_mul(i as u64 + 1))).to_le_bytes());
    }
    Ok(random)
}

/// argc, argv, envp and the auxiliary vector, as a libc's `_start` expects
/// to find them.
///
/// Nothing declares this interface: `_start` takes no arguments and reads it
/// off the stack at a shape the kernel is simply expected to have built. So
/// the failure mode for getting it wrong is not a syscall nk could name, it
/// is the program dereferencing whatever happened to be there.
#[cfg(nk_lkl)]
fn build_initial_stack(
    page: *mut u8,
    image: &crate::elf::Image,
    interpreter_base: u64,
    args: &[&[u8]],
    envs: &[&[u8]],
) -> Result<u64, &'static str> {
    use crate::stack::*;

    // AT_PHDR is the *address* the program headers ended up at, which is only
    // knowable from the segment that happens to contain them -- usually the
    // first, because it starts at file offset 0 and so covers the headers.
    let phdr = image
        .segments
        .iter()
        .find(|s| image.phoff >= s.offset && image.phoff < s.offset + s.filesz)
        .map(|s| s.address + (image.phoff - s.offset) as u64)
        .unwrap_or(0);

    let random = stack_seed()?;

    let aux = [
        (AT_PHDR, phdr),
        (AT_PHENT, 56),
        (AT_PHNUM, image.phnum as u64),
        (AT_PAGESZ, PAGE as u64),
        // The executable owns AT_ENTRY/PHDR; ld.so owns the initial PC
        // and gets its relocation base separately.
        (AT_BASE, interpreter_base),
        (AT_FLAGS, 0),
        (AT_ENTRY, image.entry),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        // Claiming no optional CPU features is always safe; claiming one nk
        // has not enabled at EL0 -- FP, SVE -- is a trap the libc springs
        // itself, on its first instruction that uses it.
        (AT_HWCAP, 0),
        (AT_CLKTCK, 100),
        (AT_SECURE, 0),
    ];

    let sp = unsafe { crate::stack::Builder::new(page, USER_STACK_TOP, USER_STACK_SIZE) }
        .build(args, envs, &aux, &random)
        .ok_or("the initial stack does not fit")?;
    println!(
        "  stack: argc {}, {} environment entries, {} auxv pairs, sp {:#x}",
        args.len(),
        envs.len(),
        aux.len() + 2,
        sp
    );
    Ok(sp)
}

/// Publish freshly copied instructions through the physical identity alias.
/// D-cache writes must reach PoU before invalidating the I-cache; a barrier
/// alone does not clean dirty cache lines on machines without IDC coherence.
unsafe fn publish_code(start: *mut u8, len: usize) {
    crate::paging::clean_icache_range(start as u64, len as u64);
}

#[cfg(nk_lkl)]
pub fn launch(p: Process, prepare: Option<fn(i64)>) -> usize {
    let arg = alloc::boxed::Box::into_raw(alloc::boxed::Box::new((p, prepare))) as usize;
    let flags = crate::sync::irq_save();
    let id = crate::sched::spawn("process", process_entry, arg);
    unsafe {
        crate::sync::irq_restore(flags);
    }
    id
}

#[cfg(nk_lkl)]
extern "C" fn process_entry(arg: usize) {
    let (p, prepare) =
        *unsafe { alloc::boxed::Box::from_raw(arg as *mut (Process, Option<fn(i64)>)) };
    let pid = crate::lkl::attach_process().expect("Linux process attach failed");
    crate::sched::bind_linux_pid(pid);
    println!(
        "  process: nk {} Linux pid {} tid {}",
        crate::sched::current_id(),
        pid,
        crate::lkl::syscall(178, [0; 6])
    );
    if crate::lkl::open_console() {
        crate::sched::set_has_console(true);
    }
    if let Some(prepare) = prepare {
        prepare(pid);
    }
    run(&p);
}

#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub extern "C" fn rust_el0_irq() {
    crate::sched::record_user_irq();
    crate::exceptions::rust_irq();
}

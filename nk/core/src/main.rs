//! nk -- the NETHOS kernel.
//!
//! Stage 0: reach Rust from the reset vector, own the exception table, and
//! say so on the serial port. Nothing more. See docs/KERNEL.md.

#![no_std]
#![no_main]

extern crate alloc;

use core::arch::global_asm;

/// Where the boot loader left an initial ramdisk. Read once from the device
/// tree at boot, because the device tree is parsed long before there is a
/// filesystem to unpack it into.
static mut INITRD: Option<(u64, u64)> = None;

pub mod arch;
pub mod pte;
#[cfg(target_arch = "aarch64")]
pub mod pte_arm;
#[cfg(target_arch = "x86_64")]
pub mod pte_x86;
#[cfg(nk_lkl)]
pub mod console;
pub mod cpio;
pub mod dt;
#[cfg(nk_lkl)]
pub mod elf;
pub mod nethos_abi;
#[cfg(target_arch = "aarch64")]
pub mod exceptions;
pub mod frames;
#[cfg(nk_lkl)]
pub mod futex;
#[cfg(target_arch = "aarch64")]
pub mod gic;
#[cfg(target_arch = "x86_64")]
pub mod interrupts_x86;
#[cfg(target_arch = "x86_64")]
pub mod lapic;
#[cfg(target_arch = "x86_64")]
pub mod tss_x86;
#[cfg(target_arch = "x86_64")]
pub use interrupts_x86 as gic;
pub mod heap;
pub mod hostops;
#[cfg(nk_lkl)]
pub mod lkl;
#[cfg(nk_lkl)]
pub mod lklirq;
#[cfg(nk_linux)]
pub mod linux;
pub mod mmio;
pub mod paging;
pub mod psci;
pub mod reserve;
pub mod sched;
pub mod smp;
pub mod schedpolicy;
pub mod schedtest;
pub mod schedtrace;
pub mod selftest;
pub mod shm;
pub mod signal;
pub mod stack;
pub mod stub;
pub mod sync;
pub mod syscall_nr;
#[cfg(target_arch = "x86_64")]
pub mod syscall_x86_generic;
#[cfg(nk_lkl)]
pub mod syscall;
pub mod uaccess;
#[cfg(nk_lkl)]
pub mod useraccess;
pub mod user;
pub mod timer;
#[cfg(target_arch = "aarch64")]
#[cfg(target_arch = "aarch64")]
pub mod uart;
#[cfg(target_arch = "x86_64")]
pub mod uart_x86;
#[cfg(target_arch = "x86_64")]
pub use uart_x86 as uart;

// The assembly lives in a real .s file rather than inline in a string, so it
// can be read and diffed as assembly. global_asm! means rustc's own assembler
// handles it and no separate binutils cross-toolchain is needed to build the
// kernel at all -- rustup and nothing else.
//
// One boot file per architecture. They are not variations on a theme: the
// aarch64 one is handed a device tree and drops from EL2 to EL1, the x86-64
// one is a Multiboot2 image that has to build page tables and enter long
// mode before any Rust can run at all.
#[cfg(target_arch = "aarch64")]
global_asm!(include_str!("boot.s"));
#[cfg(target_arch = "aarch64")]
global_asm!(include_str!("user.s"));
// att_syntax because the file is written in AT&T assembly, and `global_asm!`
// defaults to Intel on x86. Without it the first `mov $x, %reg` is a parse
// error about an unknown token, which names the column and not the cause.
#[cfg(target_arch = "x86_64")]
global_asm!(include_str!("boot-x86_64.s"), options(att_syntax));
#[cfg(target_arch = "x86_64")]
global_asm!(include_str!("user-x86_64.s"), options(att_syntax));
// The ring-3 smoke test, copied verbatim to USER_BASE by `user::spawn`.
//
// Not an ELF: spawn copies one page and jumps to its first byte, so the
// program has to be position-independent within that page. build.rs writes
// the file into OUT_DIR -- which has no fixed location, so the path is
// spliced in with `concat!` rather than named in the assembly, because
// `.incbin` wants a quoted literal.
#[cfg(target_arch = "x86_64")]
global_asm!(
    ".section \".rodata\"",
    ".balign 4096",
    ".global __user_blob_start",
    "__user_blob_start:",
    concat!(".incbin \"", env!("NK_USER_BLOB_PATH"), "\""),
    ".global __user_blob_end",
    "__user_blob_end:",
    // The built-in fixture aarch64 keeps in user.s: an ELF assembled by hand
    // so a standalone build has something to run. There is no x86-64 one, so
    // these are empty -- `spawn_from_rootfs` names them, and an initrd's
    // /nk-init takes priority over the fixture anyway.
    ".global __user_elf_start",
    "__user_elf_start:",
    ".global __user_elf_end",
    "__user_elf_end:",
    options(att_syntax)
);
#[cfg(target_arch = "x86_64")]
global_asm!(include_str!("interrupts-x86_64.s"), options(att_syntax));

#[cfg(target_arch = "x86_64")]
pub mod bootinfo_x86;

/// Where boot-x86_64.s hands over.
///
/// `magic` names the protocol the loader spoke: 0x36d76289 for Multiboot2,
/// 0x50564800 ("PVH\0") for PVH, which is what QEMU's -kernel uses. `info`
/// is the physical address of the data structure describing the machine:
/// a Multiboot2 tag list for the first, an hvm_start_info for the second.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub extern "C" fn rust_main(magic: u64, info: u64) -> ! {
    unsafe { uart_x86::console().init() };
    println!();
    println!("NETHOS kernel (nk) {} -- x86_64", env!("CARGO_PKG_VERSION"));
    println!("  magic: {:#x}  info: {:#x}", magic, info);
    let proto = match magic as u32 {
        0x36d76289 => "Multiboot2 (GRUB / real hardware)",
        0x50564800 => "PVH (QEMU -kernel)",
        _ => "unknown loader",
    };
    println!("  boot:  {}", proto);

    let boot = match magic as u32 {
        0x50564800 => unsafe { bootinfo_x86::parse_pvh(info) },
        _ => None,
    };

    let Some(boot) = boot else {
        panic!("could not parse boot information from loader");
    };

    let mut usable = 0u64;
    for &(s, e) in &boot.ram_regions[..boot.ram_region_count] {
        println!("  memory: {:#x}..{:#x} ({} MiB)", s, e, (e - s) >> 20);
        usable += e - s;
    }
    println!("  usable: {} MiB across {} region(s)", usable >> 20, boot.ram_region_count);

    if let Some((s, e)) = boot.initrd {
        println!("  initrd: {:#x}..{:#x} ({} KiB)", s, e, (e - s) >> 10);
        unsafe { INITRD = Some((s, e)) };
    }

    // Hand free RAM above the kernel to the page allocator.
    extern "C" {
        static __image_end: u8;
    }
    let image_end = &raw const __image_end as usize;
    let ram_start = (image_end + 4095) & !4095;

    // Everything that is in RAM but must not be handed out, lowest first.
    // Adding a range and hoping nothing lands on the initrd or on Linux's
    // window does not fail where the mistake is: it fails later, in whatever
    // was given the page, with someone else's bytes in it.
    #[cfg(nk_lkl)]
    let linux = (
        paging::LINUX_PHYS_BASE as usize,
        (paging::LINUX_PHYS_BASE + paging::LINUX_PHYS_SIZE) as usize,
    );
    #[cfg(not(nk_lkl))]
    let linux = (0usize, 0usize);
    let initrd = unsafe { INITRD }
        .map(|(s, e)| (s as usize, e as usize))
        .unwrap_or((0, 0));

    // Only memory the boot page tables actually map may be handed out.
    //
    // boot-x86_64.s identity-maps the first 4GB, so a frame above that has no
    // virtual address at all: using one as a page table faults inside the
    // walker, and using one as a user page maps something that is not there.
    // A 6GB machine reports a region at 4GB..6GB, and every page of it is
    // unusable until nk maps high memory.
    const MAPPED_LIMIT: usize = 4 * 1024 * 1024 * 1024;

    let mut reserved = [initrd, linux];
    reserved.sort_unstable();

    let mut highest_end = 0usize;
    for &(rs, re) in &boot.ram_regions[..boot.ram_region_count] {
        // The kernel image sits in the lowest region; skip past it there.
        let lo = (rs as usize).max(ram_start);
        let hi = (re as usize).min(MAPPED_LIMIT);
        if lo >= hi {
            continue;
        }
        highest_end = highest_end.max(hi);
        let mut at = lo;
        for (xs, xe) in reserved {
            if xe <= at || xs >= hi {
                continue;
            }
            if xs > at {
                unsafe { frames::add(at, xs.min(hi)) };
            }
            at = at.max(xe);
        }
        if at < hi {
            unsafe { frames::add(at, hi) };
        }
    }

    // LKL's linear map is a *physical* address compiled into the archive
    // (CONFIG_LKL_MEMORY_START), because its __pa() is the identity. A
    // machine too small to contain it cannot run Linux at all, and saying so
    // here beats a driver reading memory that was never there.
    #[cfg(nk_lkl)]
    assert!(
        linux.1 <= highest_end,
        "Linux's window {:#x}..{:#x} does not fit in mapped RAM ending at {highest_end:#x}",
        linux.0,
        linux.1,
    );

    let (used, total) = frames::stats();
    println!("  frames: {}/{} available", total - used, total);
    #[cfg(nk_lkl)]
    println!(
        "  linux:  {:#x}..{:#x} ({} MiB) reserved for LKL",
        linux.0,
        linux.1,
        (linux.1 - linux.0) >> 20,
    );

    // Reserve 192MB for the kernel heap, matching the aarch64 heap bump.
    unsafe { heap::init(192) };

    // Before any process exists, because every address space nk creates
    // copies the top of the kernel's tables and Linux must be in all of them.
    #[cfg(nk_lkl)]
    unsafe {
        paging::reserve_linux_window()
    };

    sched::init();

    // Interrupts: IDT loaded, PIC remapped, then the PIT started. The order
    // matters -- a tick that arrives before the IDT is loaded lands on the
    // BIOS's vector table.
    unsafe { interrupts_x86::init() };
    // The TSS must exist before ring 3 does: an interrupt taken in ring 3
    // with no RSP0 to switch to triple-faults and resets the machine silently.
    unsafe {
        // __stack_top is where boot-x86_64.s set RSP, so it is the real top of
        // the boot task's stack -- rounding the *current* RSP up to a page
        // would name a point part way down it, and every trap would then land
        // inside the boot task's own live frames.
        unsafe extern "C" {
            static __stack_top: u8;
        }
        let top = &raw const __stack_top as u64;
        tss_x86::init(top);
        sched::set_boot_stack_top(top);
    }
    unsafe { timer::init(0) };
    // `syscall` is an invalid opcode until EFER.SCE says otherwise, and the
    // MSRs that say where it lands (LSTAR), what it runs as (STAR) and which
    // flags it clears (FMASK) are equally unset at reset. Without this the
    // first system call from ring 3 raises #UD at the `syscall` itself --
    // which reads as a corrupt program rather than a kernel that never
    // enabled the instruction.
    unsafe {
        extern "C" {
            fn x86_syscall_init();
        }
        x86_syscall_init();
        println!("  syscall: SCE enabled, LSTAR/STAR/FMASK programmed");
    }

    // Before the scheduler runs: the policy measures in microseconds and the
    // tick is 10 ms, so without a calibrated cycle counter every latency it
    // reports is either 0 or 10 ms and every slice comparison is meaningless.
    timer::calibrate();
    schedtrace::start();

    // The local APIC, before any AP: it is what sends the INIT/STARTUP pair
    // and the reschedule IPI, and its timer is what gives each AP a tick.
    // Enabling it on the boot CPU does not disturb the 8259 -- device
    // interrupts keep arriving through the PIC exactly as before.
    unsafe {
        lapic::init();
        lapic::calibrate_timer();
    }

    println!();
    println!("Stage 1 up on x86-64. Unmasking interrupts.");
    sched::enable();
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };

    // Secondary CPUs, once interrupts are live on this one: an AP signals
    // readiness through shared memory, but the BSP's wait is bounded by the
    // TSC rather than a tick, so this order is a convenience rather than a
    // requirement. No-op unless NK_SMP was set at build time.
    smp::bring_up();

    println!("Interrupts unmasked. nk x86-64 running.");

    // Prove the IDT entries are reached and returned from, rather than merely
    // loaded. `int3` is the one vector that can be raised deliberately without
    // breaking anything: it runs the same stub, the same frame build and the
    // same `iretq` as a hardware interrupt, so arriving at the line after it
    // is evidence the whole entry path works. Loading a table and never taking
    // an interrupt through it would have looked identical up to this point.
    unsafe { interrupts_x86::selftest() };

    // And that hardware interrupts arrive, not only software ones. int3 proves
    // the gate and the return path; only a tick proves the PIT is running, the
    // PIC is unmasked and routed, and the EOI is accepted -- an unacknowledged
    // PIC delivers exactly one interrupt and then goes quiet, which looks the
    // same as a timer that was never programmed.
    {
        let start = timer::ticks();
        let mut spins = 0u64;
        while timer::ticks() == start && spins < 200_000_000 {
            core::hint::spin_loop();
            spins += 1;
        }
        let now = timer::ticks();
        if now > start {
            // One tick could be a single latched interrupt; keep waiting to
            // show the PIC is re-arming after the EOI rather than delivering
            // once and going quiet.
            let mut spins2 = 0u64;
            while timer::ticks() < now + 5 && spins2 < 1_000_000_000 {
                core::hint::spin_loop();
                spins2 += 1;
            }
            println!("  timer:  ticking ({} -> {} -> {}, {} ms elapsed)",
                start, now, timer::ticks(), timer::time_ns() / 1_000_000);
        } else {
            println!("  timer:  !! no tick after {} spins -- PIT or PIC not delivering", spins);
        }
    }

    // Exercise the context switch and timer: two threads alternating
    extern "C" fn test_worker(id: usize) {
        let names = ["ping", "pong"];
        // RSP0 is repointed at this task's own kernel stack on every switch;
        // verified once by printing it from both tasks and seeing two
        // different stack tops, each matching its own.
        debug_assert_eq!(
            unsafe { tss_x86::kernel_stack() },
            sched::kernel_stack_top() as u64,
            "RSP0 does not name the running task's kernel stack",
        );
        for i in 1..=5 {
            println!("  [x86] {} #{}", names[id], i);
            sched::yield_now();
        }
        println!("  [x86] {} done", names[id]);
    }

    sched::spawn("ping", test_worker, 0);
    sched::spawn("pong", test_worker, 1);

    println!("Spawned ping/pong. Running scheduler.");
    // Let the two demo tasks finish before Linux starts, so their output is
    // not interleaved with the kernel's boot log.
    for _ in 0..64 {
        sched::yield_now();
    }

    // The scheduler's own torture tests, before Linux exists to complicate
    // the picture. Behind an environment flag because they deliberately
    // saturate the machine for several seconds, which is not something an
    // ordinary boot should do.
    if option_env!("NK_SCHEDTEST").is_some() {
        schedtest::run();
    }


    // The whole Linux kernel, when it is linked in. Same order as aarch64:
    // the timer thread and the IRQ thread must exist before the kernel that
    // will use them, and the watchdog before a boot that might not finish.
    #[cfg(nk_lkl)]
    {
        hostops::start_timer_thread();
        lklirq::start();
        sched::spawn("watchdog", watchdog, 0);
        if lkl::boot() {
            println!();
            println!("Linux is up on nk (x86-64). Asking it something:");
            println!();
            // LKL's numbers, not x86-64's: `ldk lkl` builds with ARCH=lkl on
            // both hosts, so the table is asm-generic wherever nk runs. 172 is
            // getpid there; the x86-64 host number for getpid is 39, and
            // using it here would have called something else entirely.
            println!("  getpid()  -> {}", lkl::syscall(172, [0; 6]));
            println!("  gettid()  -> {}", lkl::syscall(178, [0; 6]));
            println!("  getuid()  -> {}", lkl::syscall(174, [0; 6]));
            check_lkl_cpu_addr();
            lkl::mount_kernel_filesystems();

            // The initrd, into Linux's rootfs, before anything looks for a
            // program there.
            if let Some((s, e)) = unsafe { INITRD } {
                let archive =
                    unsafe { core::slice::from_raw_parts(s as *const u8, (e - s) as usize) };
                match user::unpack_initrd(archive) {
                    Ok(n) => println!("  initrd: {} entries unpacked into Linux's rootfs", n),
                    Err(e) => println!("  initrd: {} -- ignored", e),
                }
            }

            // Ring 3.
            //
            // After Linux, not before: the initial stack guard comes from
            // `getrandom`, and the program itself is read back through the
            // VFS, so both need a kernel that exists. In its own task because
            // `user::run` never returns.
            //
            // A real ELF from the rootfs, not the hand-written blob that
            // first proved the transition: `spawn_from_rootfs` is the path
            // aarch64 uses, so an ordinary statically linked binary is what
            // gets run rather than something shaped to avoid the loader.
            println!();
            match user::spawn_from_rootfs() {
                Ok(p) => {
                    println!("  ring3:  entering ring 3 with /nk-init");
                    // `user::launch`, not a bare `sched::spawn` of `run`:
                    // launch's entry attaches the task to Linux first, which
                    // is what gives it a pid, a descriptor table and a
                    // console. Running `run` directly skipped all three, and
                    // the process reported `pid 0` -- a task Linux had never
                    // been told about, whose first file operation would have
                    // been made by nobody.
                    user::launch(p, None);
                }
                Err(e) => println!("  ring3:  no init to run: {e}"),
            }
        } else {
            println!("  linux:  did not start");
        }
    }

    loop {
        sched::yield_now();
    }
}

/// Where boot.s hands over on aarch64. `dtb` is whatever x0 held at reset:
/// on QEMU's `virt` that is the physical address of the flattened device
/// tree, which is the whole of what this kernel is told about the machine.
#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub extern "C" fn rust_main(dtb: *const u8) -> ! {
    println!();
    println!("NETHOS kernel (nk) {} -- aarch64", env!("CARGO_PKG_VERSION"));

    println!("  running at EL{}", current_el());

    // Everything nk knows about the machine comes through here. A null or
    // unparseable pointer is fatal on purpose: guessing at hardware addresses
    // is how a kernel ends up working on exactly one emulator.
    let Some(fdt) = (unsafe { dt::Fdt::from_ptr(dtb) }) else {
        panic!("no usable device tree at {:#018x}", dtb as usize);
    };
    println!("  device tree at {:#018x}, {} bytes", fdt.base(), fdt.total_size());

    let (ac, sc) = fdt.root_cells();
    println!("  #address-cells {ac}  #size-cells {sc}");

    if let Some(root) = fdt.find_by_prefix("") {
        if let Some(m) = root.prop("model") {
            println!("  model: {}", core::str::from_utf8(&m[..m.len().saturating_sub(1)]).unwrap_or("?"));
        }
    }
    if let Some((base, size)) = fdt.find_by_prefix("memory@").and_then(|n| n.reg(0)) {
        println!("  memory: {:#x}..{:#x} ({} MiB)", base, base + size, size >> 20);
    }
    if let Some((s, e)) = fdt.initrd() {
        println!("  initrd: {:#x}..{:#x} ({} KiB)", s, e, (e - s) >> 10);
        unsafe { INITRD = Some((s, e)) };
    }
    if let Some((base, _)) = fdt.find_compatible("arm,pl011").and_then(|n| n.reg(0)) {
        println!("  pl011:  {:#x}", base);
    }
    if let Some(gic) = fdt.find_compatible("arm,gic-v3") {
        let d = gic.reg(0).map(|r| r.0).unwrap_or(0);
        let r = gic.reg(1).map(|r| r.0).unwrap_or(0);
        println!("  gicv3:  dist {:#x}  redist {:#x}", d, r);
    }
    if let Some((_, num, _)) = fdt.find_compatible("arm,armv8-timer").and_then(|n| n.interrupt(2)) {
        // The four entries are secure physical, non-secure physical, virtual,
        // hypervisor -- in that order. Index 2, the virtual timer, is the one
        // nk uses; see the note at the top of timer.rs for why the physical
        // one at index 1 is the trap it looks like the right answer.
        println!("  timer:  virtual, PPI INTID {}", num + 16);
    }
    let mut virtio = 0;
    fdt.each_compatible("virtio,mmio", |_| virtio += 1);
    println!("  virtio-mmio transports: {}", virtio);

    let (ram_base, ram_size) = fdt
        .find_by_prefix("memory@")
        .and_then(|n| n.reg(0))
        .expect("device tree has no memory node");
    unsafe { paging::init(ram_base, ram_size) };

    unsafe { claim_memory(&fdt, ram_base as usize, (ram_base + ram_size) as usize) };
    frames::report();
    // 64MB, not 16. nk's heap holds whatever a program is while it is being
    // loaded -- a real init is a couple of megabytes and an ELF is read whole
    // before any of it is mapped -- as well as every bounce buffer a syscall
    // needs. Sixteen was chosen when the largest thing on it was a page table.
    heap::init(192);
    selftest::run();

    let gic = fdt.find_compatible("arm,gic-v3").expect("no GICv3 in the device tree");
    let gicd = gic.reg(0).expect("GIC has no distributor reg").0 as usize;
    let gicr = gic.reg(1).expect("GIC has no redistributor reg").0 as usize;
    unsafe { gic::init(gicd, gicr) };

    let (_, ppi, _) = fdt
        .find_compatible("arm,armv8-timer")
        .and_then(|n| n.interrupt(2))
        .expect("no virtual timer in the device tree");
    unsafe { timer::init(ppi + 16) };
    // What a secondary needs to bring up its own redistributor. The stride is
    // architectural for GICv3 -- a redistributor is two 64 KiB frames, RD_base
    // then SGI_base -- and `virt` lays them out densely in CPU order.
    unsafe { gic::remember_secondary_params(gicr, 0x20000, ppi + 16) };

    // The console can now be typed into. An SPI's device-tree number is
    // relative to the first SPI, and the GIC's first SPI is INTID 32.
    if let Some((_, spi, _)) = fdt.find_compatible("arm,pl011").and_then(|n| n.interrupt(0)) {
        unsafe { uart::init_receive(spi + 32) };
        println!("  pl011:  receive on SPI {} (INTID {})", spi, spi + 32);
    }

    // Before any process exists, because every address space nk creates
    // copies the top of the kernel's tables and Linux must be in all of them.
    #[cfg(nk_lkl)]
    unsafe {
        paging::reserve_linux_window()
    };

    sched::init();

    psci::init(&fdt);

    // See the x86 path: the scheduler's units are microseconds, and nothing
    // it measures means anything until the cycle counter's rate is known.
    timer::calibrate();
    schedtrace::start();

    println!();
    println!("Stage 1 up. Unmasking interrupts.");
    sched::enable();
    unsafe { core::arch::asm!("msr daifclr, #0xf", options(nomem, nostack)) };
    // Secondary CPUs, once interrupts are live on this one. Same position
    // and same reasoning as the x86 path: the BSP's wait is bounded by the
    // counter rather than a tick, so the order is a convenience rather than
    // a requirement. No-op unless NK_SMP was set at build time.
    smp::bring_up();

    // See the x86 path. Behind a flag: these saturate the machine for
    // several seconds on purpose.
    if option_env!("NK_SCHEDTEST").is_some() {
        schedtest::run();
    }

    // The whole Linux kernel, when it is linked in: nk stops being the
    // kernel and becomes the machine underneath one.
    #[cfg(nk_lkl)]
    {
        // Before Linux starts: its timer callbacks run here, in thread
        // context, not in the interrupt that noticed they were due.
        hostops::start_timer_thread();
        lklirq::start();
        // A watchdog rather than a timeout. Linux either finishes booting or
        // deadlocks, and the difference between the two from outside is
        // nothing at all -- both are a machine that has stopped printing. The
        // watchdog prints what every task is doing, which turns "it hung"
        // into a wait graph.
        sched::spawn("watchdog", watchdog, 0);
        if lkl::boot() {
            println!();
            println!("Linux is up on nk. Asking it something:");
            println!();
            // getpid is 172 on aarch64. Chosen because it is the smallest
            // possible question -- it touches the task model and nothing
            // else -- and because an answer at all means the scheduler,
            // memory, timers and locks nk handed over all work.
            println!("  getpid()  -> {}", lkl::syscall(172, [0; 6]));
            println!("  gettid()  -> {}", lkl::syscall(178, [0; 6]));
            println!("  getuid()  -> {}", lkl::syscall(174, [0; 6]));
            // After the syscalls, not before: lkl_cpu_init assigns lock,
            // sem and shutdown_sem one after another from LKL's init
            // thread, which runs concurrently with this one. Printed
            // earlier, "cpu.sem 0x0" is a race with line 282, not a fact.
            // "task 44 waits on sem 31" is only a name once 31 has one:
            // LKL allocates the two back to back, so consecutive ids here
            // confirm which is which in the graphs.
            check_lkl_cpu_addr();
            report_lkl_cpu_identity();
            lkl::mount_kernel_filesystems();

            // The futex layer must not ask Linux for the time: every
            // clock_gettime is an LKL entry, which takes the cpu lock, and
            // the futex paths run from contexts (the timer thread, and the
            // syscall path of a task about to sleep) where queueing behind
            // that lock wedges the whole machine -- the renderer reliably
            // froze with futex-time as the cpu semaphore's last taker and
            // every later syscall queued behind it. Linux reads that same
            // time from the host op (host_time -> nk_time_ns), so the value
            // is available without entering Linux at all; only the realtime
            // offset -- a constant, nothing in the guest adjusts it -- has
            // to be learned, once, here, while this boot task is the only
            // natural entrant.
            futex::capture_realtime_offset();

            // Every virtio transport that has anything behind it, handed to
            // Linux. arch/lkl has no bus to enumerate and no device tree, so
            // it is told: virtio_mmio_device_add registers a platform device
            // at the address, and Linux's ordinary virtio_mmio driver probes
            // whatever is there. Reading the magic first because QEMU's virt
            // provides thirty-two transports and a machine usually has two or
            // three devices; registering the empty ones would work and would
            // print thirty failed probes.
            attach_virtio(&fdt);

            // And from user space, which is the whole point: a process at
            // EL0 makes an `svc`, nk catches it, and Linux answers.
            println!();
            // Anything the boot loader handed over goes into Linux's rootfs
            // before the first process looks for a program there.
            if let Some((s, e)) = unsafe { INITRD } {
                let archive =
                    unsafe { core::slice::from_raw_parts(s as *const u8, (e - s) as usize) };
                match user::unpack_initrd(archive) {
                    Ok(n) => println!("  initrd: {} entries unpacked into Linux's rootfs", n),
                    Err(e) => println!("  initrd: {} -- ignored", e),
                }
            }

            println!("Now the same question from EL0:");
            // One init, unless nk is running its own fixture.
            //
            // A kernel starts one init. The second process here exists to
            // check that two of them are independent -- different Linux pids,
            // separate descriptor tables, address spaces that survive being
            // preempted into each other -- and every one of those is a claim
            // about nk, provable only against a program written to prove it.
            // Running a supplied init twice made every demo read oddly: two
            // shells racing for the same disk, two greetings, a spurious
            // "another process has it".
            let first = user::spawn_from_rootfs().expect("cannot load rootfs executable");
            let selftest = user::ran_fixture();

            let parent_mask = lkl::syscall(166, [0o22, 0, 0, 0, 0, 0]);
            let fd = lkl::syscall(56, [-100, c"/nk-init".as_ptr() as i64, 0, 0, 0, 0]);
            assert!(fd >= 0);
            selftest::process_descriptor(fd);

            let a = user::launch(first, Some(selftest::process_context));
            sched::set_init_task(a);
            let b = if selftest {
                let second =
                    user::spawn_from_rootfs().expect("cannot load rootfs executable");
                Some(user::launch(second, Some(selftest::process_context)))
            } else {
                None
            };
            sched::join(a);
            if let Some(b) = b {
                sched::join(b);
            }

            let pa = sched::linux_pid(a);
            assert!(pa > 1);
            if !selftest {
                println!("  init: exited with {}", sched::exit_status(a));
            }
            assert_eq!(lkl::syscall(57, [fd, 0, 0, 0, 0, 0]), 0);
            assert_eq!(lkl::syscall(166, [parent_mask, 0, 0, 0, 0, 0]), 0o22);

            if let Some(b) = b {
                let pb = sched::linux_pid(b);
                assert!(pb > 1 && pa != pb);
                // The fixture exits with its own Linux pid, which is how the
                // two processes prove they were told different ones.
                assert_eq!(sched::exit_status(a), pa as i32);
                assert_eq!(sched::exit_status(b), pb as i32);
                // That both Linux tasks are gone is only answerable while
                // nothing else has been created since -- a pid Linux has
                // finished with is one it is free to hand out again.
                assert_eq!(lkl::syscall(129, [pa, 0, 0, 0, 0, 0]), -3);
                assert_eq!(lkl::syscall(129, [pb, 0, 0, 0, 0, 0]), -3);
                println!(
                    "  processes: distinct PIDs, private files/fs, both Linux tasks reaped"
                );
                assert_eq!(lkl::syscall(172, [0; 6]), 1);
                println!("  parent: Linux init survived both exits");
                // The fixture spins long enough to be preempted several times
                // on purpose, which is how it shows that address spaces
                // survive a switch. A real binary is simply too quick.
                assert!(sched::user_irqs(a) > 0 && sched::user_irqs(b) > 0);
                println!(
                    "  processes: EL0 IRQs {} and {}, private stacks survived",
                    sched::user_irqs(a),
                    sched::user_irqs(b)
                );
                sched::reap_process(b);
            }
            sched::reap_process(a);
            println!("  processes: nk page tables, pages and task stacks reclaimed");
        }
        stop();
    }

    // Linux code runs from here on. Interrupts are already on and the
    // scheduler is running, because a driver's probe may sleep, wait on a
    // completion, or take a timeout -- all of which need a tick and something
    // else to run.
    #[cfg(nk_linux)]
    {
        map_vmemmap(ram_base, ram_size);
        linux::init();
        linux::probe(&fdt);

        // Probing is not synchronous from here: virtio_blk's own probe path
        // defers part of itself to a workqueue, which runs on the kworker
        // thread and needs the CPU. Yielding until the capacity arrives is
        // what waiting for a device to appear looks like with no completion
        // to wait on.
        // Yield a bounded number of times rather than waiting a number of
        // seconds.
        //
        // Probing is not synchronous: virtio_blk defers part of its own to a
        // workqueue, which runs on the kworker thread and needs the CPU. But
        // a *time* limit is the wrong shape here -- under TCG the virtual
        // timer counts guest cycles rather than following the host clock, so
        // a three-second deadline is around seven hundred real ones, and the
        // wait is indistinguishable from a hang. Yields are the thing
        // actually being waited for, so count those.
        for _ in 0..2000 {
            if linux::capacity() != 0 {
                break;
            }
            sched::yield_now();
        }

        println!();
        if linux::capacity() != 0 {
            println!(
                "Stage 3: virtio-blk is up -- {} sectors, {} MiB",
                linux::capacity(),
                linux::capacity() * 512 / (1024 * 1024)
            );
            read_a_sector();
            stop();
        }
        if let Some(mac) = linux::net_up() {
            arp_exchange(mac);
            stop();
        }
        println!("No device appeared.");
        stop();
    }

    // Without a Linux port linked in there are no drivers to exercise, so the
    // two demo threads run instead -- still the only thing that proves the
    // context switch, and what tests/test_kernel_boot.py checks.
    #[cfg(not(nk_linux))]
    {
        sched::spawn("ping", worker, 0);
        sched::spawn("pong", worker, 1);
        println!("Two threads should now alternate.");
        println!();
        let deadline = timer::ticks() + timer::HZ * 4;
        while timer::ticks() < deadline {
            sched::yield_now();
        }

        println!();
        println!("Now user space.");
        println!();
        let p = user::spawn();
        user::run(&p);
    }
}

/// Report what the machine is doing, once a second, until init finishes.
///
/// Used to be four rounds and then `stop()`: a four-second guillotine that
/// killed every test longer than four seconds mid-syscall and presented as a
/// kernel wedge. Then a 600-second backstop with the same flaw at a longer
/// scale: a desktop session's init *runs forever* (`exec sleep 86400` once
/// the shell is up), so a flat backstop powered off every healthy session at
/// exactly ten minutes -- measured as live windows dying while the shell
/// inside them still worked. The clock cannot tell a working machine from a
/// wedged one; syscalls can. So the machine is stopped only when init has
/// exited (the boot-test shape, unchanged) or when no syscall at all was
/// forwarded for `STALL_LIMIT` consecutive seconds -- a machine that quiet
/// is stopped whatever the round counter says, and one that keeps making
/// syscalls is allowed to run indefinitely.
///
/// The graph is also the bulk of the console -- and console output on this
/// machine is a UART, so printing it every second is itself a large part of
/// what the boot spends its time doing. So after the first half minute a
/// graph only every thirtieth round -- except while stalled, when the wait
/// graph is exactly what is wanted, immediately and every round after.
#[cfg(nk_lkl)]
extern "C" fn watchdog(_: usize) {
    const STALL_LIMIT: u32 = 120;
    let mut round = 0u32;
    let mut last_activity = crate::syscall::activity();
    let mut stalled: u32 = 0;
    loop {
        // A yield loop, not `sleep_ticks`, and deliberately so: this task
        // also runs `lkl_cpu_release_if_dying` once a round, which is what
        // clears a CPU label left by a dead LKL thread. Parking the
        // watchdog on the tick made it miss that entirely and the boot
        // stopped at the first weston start, every time. `yield_now` halts
        // when nothing else is runnable, so the idle cost of this is a
        // wake per tick rather than a spin -- and when something *is*
        // runnable, this loop is no longer what is keeping the CPU up.
        let until = timer::ticks() + timer::HZ;
        while timer::ticks() < until {
            sched::yield_now();
        }
        round += 1;
        let activity = crate::syscall::activity();
        if activity == last_activity {
            stalled += 1;
        } else {
            stalled = 0;
        }
        last_activity = activity;
        // A scheduler switch can hand the CPU label to a Linux task whose
        // host thread is already dead (change_owner(dead tid) inside LKL's
        // __switch_to), and from then on nobody can release it: the dying
        // thread's own exit saw a different owner, and every live caller
        // fails the equality check for ever. The watchdog runs regardless,
        // so once a second it clears a dead owner and wakes a sleeper; the
        // machine heals instead of stopping with the graph above.
        #[cfg(nk_lkl)]
        crate::lkl_cpu_release_if_dying(stalled);
        let (frames_used, frames_total) = crate::frames::stats();
        if round <= 30 || round % 30 == 0 || stalled != 0 {
            println!();
            #[cfg(nk_lkl)]
            report_virtio();
            #[cfg(nk_lkl)]
            report_lkl_cpu();
            // A stall's first second is the only interesting one: dump the
            // label's recent transitions then, and again as it deepens.
            #[cfg(nk_lkl)]
            if stalled == 1 || stalled == 10 || (stalled != 0 && stalled % 30 == 0) {
                crate::lkl_cpu_trace_dump(220);
                crate::lkl_switch_trace_dump(90);
            }
            #[cfg(nk_lkl)]
            futex::report();
            unsafe {
                println!(
                    "  watchdog {} at {} ticks: frames {}/{} armed={} due={} fired={} last_deadline={}",
                    round,
                    timer::ticks(),
                    frames_used,
                    frames_total,
                    hostops::N_ARMED,
                    hostops::N_DUE,
                    hostops::N_FIRED,
                    hostops::LAST_DEADLINE
                );
            }
            sched::report();
            sync::report();
            crate::syscall::report();
            crate::reserve::report();
            crate::heap::report();
            if round % 10 == 0 {
                crate::heap::raw_report();
            }
            unsafe {
                if sched::LOST_WAKEUPS != 0 {
                    println!("          {} lost wakeups", sched::LOST_WAKEUPS);
                }
            }
        }
        if sched::init_done() {
            println!("  watchdog: init finished after {}s", round);
            break;
        }
        if stalled >= STALL_LIMIT {
            println!(
                "  watchdog: no syscalls for {}s -- stopping with the graph above",
                stalled
            );
            break;
        }
    }
    stop();
}

/// Print the same reports the watchdog prints, on demand from the guest.
///
/// Syscall 999 from any process reaches here; the supervisor calls it the
/// moment the shell goes silent, because a hung shell never stills the
/// whole machine (weston and nethosd keep polling), so the watchdog's own
/// trigger never fires and the wait graph -- the one thing that names the
/// syscall the dead tasks are parked in -- is printed too late or never.
pub fn manual_dump() {
    println!();
    println!("MANUAL_DUMP_BEGIN");
    #[cfg(nk_lkl)]
    {
        crate::lkl_cpu_trace_dump(120);
        crate::lkl_switch_trace_dump(60);
        futex::report();
    }
    sched::report();
    sync::report();
    #[cfg(nk_lkl)]
    crate::syscall::report();
    crate::reserve::report();
    crate::heap::report();
    unsafe {
        if sched::LOST_WAKEUPS != 0 {
            println!("          {} lost wakeups", sched::LOST_WAKEUPS);
        }
    }
    println!("MANUAL_DUMP_END");
}

/// Finish: say so, and ask the machine to switch itself off.
///
/// Powering down rather than spinning matters for the tests. A kernel killed
/// by a watchdog looks exactly like one that hung -- both end with a signal
/// after N seconds -- and that ambiguity cost real time on one silent hang.
pub fn stop() -> ! {
    println!();
    println!("nk: done.");
    psci::poweroff();
    halt()
}

/// Stage 4: send an Ethernet frame through the unmodified driver and read the
/// answer that comes back.
///
/// An ARP request, sent *by* nk rather than waiting for one. QEMU's user-mode
/// network only ARPs a guest when it has traffic for it, so waiting is a test
/// that depends on the host deciding to speak first. Asking for the gateway's
/// address exercises the same two paths -- transmit and receive -- and does
/// so on nk's own initiative.
#[cfg(nk_linux)]
fn arp_exchange(mac: [u8; 6]) {
    const OURS: [u8; 4] = [10, 0, 2, 15]; // what QEMU's DHCP would hand out
    const GATEWAY: [u8; 4] = [10, 0, 2, 2];

    println!(
        "Stage 4: virtio-net is up -- {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    let mut f = [0u8; 42];
    f[0..6].fill(0xff); // broadcast
    f[6..12].copy_from_slice(&mac);
    f[12..14].copy_from_slice(&0x0806u16.to_be_bytes()); // ARP
    f[14..16].copy_from_slice(&1u16.to_be_bytes()); // over Ethernet
    f[16..18].copy_from_slice(&0x0800u16.to_be_bytes()); // resolving IPv4
    f[18] = 6;
    f[19] = 4;
    f[20..22].copy_from_slice(&1u16.to_be_bytes()); // request
    f[22..28].copy_from_slice(&mac);
    f[28..32].copy_from_slice(&OURS);
    // target hardware address left zero: that is the question
    f[38..42].copy_from_slice(&GATEWAY);

    println!();
    println!("  who has 10.0.2.2? asking as 10.0.2.15");
    if let Err(e) = linux::net_xmit(&f) {
        println!("  transmit failed: {}", e);
        return;
    }

    let mut buf = [0u8; 1600];
    let deadline = timer::ticks() + timer::HZ * 3;
    while timer::ticks() < deadline {
        let n = linux::net_recv(&mut buf);
        if n == 0 {
            sched::yield_now();
            continue;
        }
        // The driver hands frames up with the Ethernet header still on.
        if n >= 42 && buf[12] == 0x08 && buf[13] == 0x06 && buf[21] == 2 {
            println!(
                "  reply: 10.0.2.2 is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                buf[22], buf[23], buf[24], buf[25], buf[26], buf[27]
            );
            println!();
            println!("  the frame, as the driver delivered it:");
            hexdump(&buf[..n.min(64)], 0);
            return;
        }
        println!("  {} bytes, not the ARP reply -- still waiting", n);
    }
    println!("  no reply in 3 seconds");
}

/// The whole point of Stage 3: ask the unmodified Linux driver for a sector
/// and look at what comes back.
#[cfg(nk_linux)]
fn read_a_sector() {
    // Prefilled rather than zeroed. A read that never reaches the buffer
    // leaves it exactly as it was, and zeroes would be indistinguishable from
    // a disk full of zeroes -- which is precisely the case that looked like a
    // working read for a while.
    let mut buf = alloc::vec![0xAAu8; 512];
    println!();
    println!("  reading sector 0 through the Linux driver...");
    match linux::read(0, &mut buf) {
        Ok(()) => {
            hexdump(&buf[..96], 0);
            // Printed as text as well: the disk built by the test carries a
            // readable marker, so a correct read is legible rather than
            // something to verify byte by byte.
            crate::print!("  as text: \"");
            for &b in &buf[..32] {
                crate::print!("{}", if (0x20..0x7f).contains(&b) { b as char } else { '.' });
            }
            println!("\"");
        }
        Err(e) => println!("  read failed: {}", e),
    }
}

/// A hex dump written without `core::fmt`.
///
/// Deliberately raw, and this is not premature caution. The formatted version
/// of this function -- `print!("{:08x}", ...)` in a loop -- printed its first
/// lines correctly and then stopped the kernel dead: no fault, no panic, the
/// CPU idle in wfi, every later print lost, in the Linux-linked build only.
/// The same `print!` calls work everywhere else in the kernel, including
/// immediately after this function. It is not the UART -- removing flow
/// control entirely changed nothing -- and it is not a fault, because the
/// exception path now writes a raw marker before it formats anything.
///
/// **That is an open bug, not a solved one**, and it is recorded in
/// docs/KERNEL.md rather than papered over. This version avoids it, and is
/// the better thing for a kernel to have regardless: the one routine used to
/// inspect memory when something is wrong should not itself depend on the
/// largest piece of machinery in the binary.
#[cfg(nk_linux)]
fn hexdump(bytes: &[u8], base: usize) {
    let u = uart::console();
    let _ = base;
    let hex = |n: u8| if n < 10 { b'0' + n } else { b'a' + n - 10 };
    for (i, b) in bytes.iter().enumerate() {
        if i % 16 == 0 {
            u.put(b'\r');
            u.put(b'\n');
            u.put(b' ');
            u.put(b' ');
        }
        u.put(hex(b >> 4));
        u.put(hex(b & 15));
        u.put(b' ');
    }
    u.put(b'\r');
    u.put(b'\n');
}

/// Two of these run when no Linux port is linked, to show that preemption
/// works and that each thread keeps its own stack and registers across a
/// switch it never asked for.
#[cfg(not(nk_linux))]
extern "C" fn worker(id: usize) {
    let names = ["ping", "pong"];
    let mut n = 0u64;
    loop {
        // A local that must survive being preempted: if the context switch
        // loses a callee-saved register or lands on the wrong stack, this is
        // where it shows, as a counter that jumps or resets.
        n += 1;
        println!("[{:>5}ms] {} #{}", timer::ms(), names[id], n);
        if n == 5 {
            println!();
            sched::report();
            println!();
        }
        if n >= 8 {
            println!("{} done", names[id]);
            return;
        }
        // Spin out the slice rather than sleeping: there is no sleep yet, and
        // the point is to be interrupted involuntarily rather than to yield.
        let until = timer::ticks() + 20;
        while timer::ticks() < until {
            core::hint::spin_loop();
        }
    }
}

/// Give Linux the `struct page` array it believes already exists.
///
/// Everything in Linux that handles memory eventually holds a `struct page`.
/// nk got a long way without one: `virt_to_page` and `page_to_phys` are pure
/// arithmetic, and virtio-blk only ever converted an address into a page and
/// straight back again, so the pages it named never had to exist. The first
/// driver that *reads* one -- virtio-net's receive path, through
/// `virt_to_head_page` -- faults on an address nothing ever mapped.
///
/// So it gets mapped. Eight megabytes for a 512MB guest, at an address Linux's
/// own arithmetic chooses; nk allocates the memory, zeroes it and maps it
/// where the formula says it is. See emul/mm.c for why the formula lands where
/// it does.
#[cfg(nk_linux)]
fn map_vmemmap(ram_base: u64, ram_size: u64) {
    const BLOCK: u64 = 2 * 1024 * 1024;
    let (va, size) = linux::vmemmap_range(ram_base, ram_size);
    // Rounded outwards to whole 2MB blocks: the mapping granule is larger
    // than the array, and a partial block at either end would leave a page
    // Linux thinks exists unmapped.
    let start = va & !(BLOCK - 1);
    let bytes = ((va + size + BLOCK - 1) & !(BLOCK - 1)) - start;

    let pa = frames::alloc_contiguous_aligned(
        (bytes / frames::PAGE as u64) as usize,
        BLOCK as usize,
    )
    .expect("not enough memory for the struct page array");
    unsafe { paging::map_normal(start, pa as u64, bytes) };
    println!(
        "  vmemmap: {} MiB of struct page at {:#x} -> {:#x}",
        bytes / (1024 * 1024),
        start,
        pa as usize
    );
    // Proof it is really there, before anything relies on it. A write that
    // faults here is a mapping bug; one that faults later is a driver bug,
    // and telling those apart afterwards is expensive.
    unsafe {
        let p = start as *mut u64;
        p.write_volatile(0);
        p.add((bytes / 8 - 1) as usize).write_volatile(0);
    }
}

/// Hand every page of RAM to the frame allocator except the ones already
/// spoken for: the kernel image itself, and the device tree, which stays
/// mapped for as long as anything might want to re-read it.
///
/// The reserved ranges are sorted rather than assumed to be in any order.
/// QEMU happens to put the DTB above the kernel; U-Boot does not always, and
/// a boot that hands out the pages the kernel is executing from fails in a
/// way that has no useful symptom at all.
/// The transport to report on when a probe stops answering. Read from the
/// watchdog, which is the only thread still running when a driver has blocked
/// waiting for a device that never replied.
#[cfg(nk_lkl)]
static mut PROBE: usize = 0;

/// LKL's own CPU lock state, straight from its global.
///
/// When the machine wedges, the question that decides everything is "who
/// holds the CPU": every later syscall queues behind that one fact. LKL
/// keeps the answer in `cpu.count`/`cpu.owner`/`cpu.sleepers` (cpu.c), and
/// the owner is `nk thread id + 1` because that is what nk's host ops hand
/// out -- so the wait graph names the exact task that never came back.
#[cfg(nk_lkl)]
#[repr(C)]
struct LklCpu {
    lock: *mut u8,
    shutdown_gate: u32,
    irqs_pending: bool,
    sleepers: u32,
    count: u32,
    owner: u64,
    sem: *mut u8,
    shutdown_sem: *mut u8,
}

/// The linked address of LKL's `cpu` global (a local symbol: `b cpu`, so it
/// cannot be named from Rust). Derived with `llvm-nm` on the linked ELF.
///
/// This is a hazard: it is a hardcoded address for a symbol the linker is
/// free to move, and it has gone stale at least once -- an unrelated change
/// shifted it by 0x800 and nothing said so, because a stale address does not
/// fault. It reads whatever now lives there as an `LklCpu`, and the wait
/// graph reports an owner, count and sleeper tally that are entirely
/// plausible and belong to a different structure.
///
/// `check_lkl_cpu_addr` compares it against the offset build.rs read out of
/// the archive, so a stale constant is reported at boot instead of being
/// believed.
///
/// Per-architecture: the same symbol in the same archive lands at a
/// different address because nk's own sections are sized differently.
#[cfg(all(nk_lkl, target_arch = "aarch64"))]
const LKL_CPU_SEED: usize = 0x0000000040e29eb8;
#[cfg(all(nk_lkl, target_arch = "x86_64"))]
const LKL_CPU_SEED: usize = 0x0000000040e7ca60;

/// Where `cpu` actually is, once `check_lkl_cpu_addr` has confirmed it.
///
/// The seed above is only a starting guess. Any edit to nk relinks the image
/// and moves every file-local bss symbol with it, so a constant is stale the
/// moment the next build runs -- and a stale one does not fault, it reports a
/// wait graph belonging to whatever else now lives there. The search below
/// finds the real one at boot and everything after reads it from here.
#[cfg(nk_lkl)]
static mut LKL_CPU_FOUND: usize = LKL_CPU_SEED;

#[cfg(nk_lkl)]
#[inline]
fn lkl_cpu_addr() -> usize {
    unsafe { *(&raw const LKL_CPU_FOUND) }
}

/// Does `addr` hold something shaped like LKL's `cpu`?
///
/// Called after `lkl_start_kernel`, so both semaphores are allocated and
/// nk's own allocator is the one that made them: a pointer it does not
/// recognise is not a semaphore, which is a far stronger test than "not
/// null" and is what makes scanning for the structure safe.
#[cfg(nk_lkl)]
fn looks_like_lkl_cpu(addr: usize) -> bool {
    if addr & 7 != 0 {
        return false;
    }
    let c = unsafe { &*(addr as *const LklCpu) };
    crate::sync::sem_id(c.sem as *const crate::sync::Semaphore).is_some()
        && crate::sync::sem_id(c.shutdown_sem as *const crate::sync::Semaphore).is_some()
        && c.sem != c.shutdown_sem
        && c.count < 1_000_000
        && c.sleepers < 1_000_000
}

/// Verify `LKL_CPU_ADDR` still names LKL's `cpu`.
///
/// build.rs records the symbol's offset within lkl.o's bss. The linked
/// address must sit that far above the start of the bss region LKL's objects
/// contributed -- not an exact test, since nk's own bss is in there too, but
/// it catches the case that actually happens: the constant left behind by a
/// relink while everything else moved.
#[cfg(nk_lkl)]
fn check_lkl_cpu_addr() {
    let off = option_env!("NK_LKL_CPU_BSS_OFFSET")
        .and_then(|o| usize::from_str_radix(o.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0);

    // The seed first: when the constant is still right this costs one read.
    if looks_like_lkl_cpu(LKL_CPU_SEED) {
        unsafe { LKL_CPU_FOUND = LKL_CPU_SEED };
        println!("  lkl cpu: address {LKL_CPU_SEED:#x} verified (bss offset {off:#x})");
        return;
    }

    // It moved. Sweep outwards from the seed rather than over all of memory:
    // the symbol travels with nk's bss, which grows and shrinks by kilobytes
    // between builds, not megabytes -- and a bounded search cannot wander
    // into a device window and read a register.
    const REACH: usize = 8 << 20;
    for delta in (8..REACH).step_by(8) {
        for addr in [LKL_CPU_SEED.wrapping_add(delta), LKL_CPU_SEED.wrapping_sub(delta)] {
            if looks_like_lkl_cpu(addr) {
                unsafe { LKL_CPU_FOUND = addr };
                println!(
                    "  lkl cpu: seed {LKL_CPU_SEED:#x} was stale, found at {addr:#x} \
                     (bss offset {off:#x}) -- update LKL_CPU_SEED"
                );
                return;
            }
        }
    }
    println!("  lkl cpu: !! no `cpu` found near {LKL_CPU_SEED:#x} -- re-derive with llvm-nm");
}

#[cfg(nk_lkl)]
fn report_lkl_cpu() {
    unsafe {
        let c = &*(lkl_cpu_addr() as *const LklCpu);
        let owner = c.owner;
        let task = owner.wrapping_sub(1) as usize;
        // The state of the owner decides everything after a wedge: a
        // "blocked" owner is a task that may yet resume (leave it alone),
        // "gone"/"finished" is a label nobody can release and the watchdog
        // may repair. Printing it makes the graph self-explaining.
        let state = match crate::sched::from_uid(task) {
            None => "gone",
            Some(t) if crate::sched::finished(t) => "finished",
            Some(t) if crate::sched::blocked(t) => "blocked",
            Some(_) => "live",
        };
        println!(
            "  lkl cpu: count {} sleepers {} irqs_pending {} owner {} (uid {:#x}, {})",
            c.count, c.sleepers, c.irqs_pending as u32, owner, task, state
        );
    }
}

/// Name LKL's two long-lived semaphores by the ids the wait graph prints.
///
/// The shapes that matter differ by which one a task is parked on: the CPU
/// semaphore means "queued for Linux", a thread's `sched_sem` means "inside
/// LKL's scheduler, waiting to be switched to". Without this line the two
/// are the same small integer.
#[cfg(nk_lkl)]
fn report_lkl_cpu_identity() {
    unsafe {
        let c = &*(lkl_cpu_addr() as *const LklCpu);
        println!(
            "  lkl sems: cpu.sem {:#x} (id {:?}), shutdown_sem {:#x} (id {:?})",
            c.sem as usize,
            crate::sync::sem_id(c.sem as *const crate::sync::Semaphore),
            c.shutdown_sem as usize,
            crate::sync::sem_id(c.shutdown_sem as *const crate::sync::Semaphore),
        );
    }
}

/// One sample of LKL's CPU label, taken at every tick.
///
/// The dead-owner strand is installed by a single `__switch_to`, and the
/// one-second watchdog sees only the corpse: owner already dead, every
/// syscall parked. This ring keeps the transitions -- who held the label,
/// who was executing when it changed, how many sleepers there were -- so
/// the watchdog's stall dump contains the switch that did it.
#[cfg(nk_lkl)]
#[derive(Clone, Copy)]
struct CpuTrace {
    tick: u64,
    count: u32,
    owner: u64,
    sleepers: u32,
    irqs: u32,
    cur: u64,
}

#[cfg(nk_lkl)]
const CPU_TRACE_LEN: usize = 512;

#[cfg(nk_lkl)]
static mut CPU_TRACE: [CpuTrace; CPU_TRACE_LEN] = [CpuTrace {
    tick: 0,
    count: 0,
    owner: 0,
    sleepers: 0,
    irqs: 0,
    cur: 0,
}; CPU_TRACE_LEN];
#[cfg(nk_lkl)]
static mut CPU_TRACE_HEAD: usize = 0;

#[cfg(nk_lkl)]
pub fn lkl_cpu_trace_sample() {
    unsafe {
        let c = &*(lkl_cpu_addr() as *const LklCpu);
        let cur = crate::sched::current_uid() as u64;
        let head = CPU_TRACE_HEAD;
        let changed = if head == 0 {
            true
        } else {
            let p = &(*(&raw const CPU_TRACE))[(head - 1) % CPU_TRACE_LEN];
            p.count != c.count
                || p.owner != c.owner
                || p.sleepers != c.sleepers
                || p.irqs != c.irqs_pending as u32
                || p.cur != cur
        };
        if changed {
            (*(&raw mut CPU_TRACE))[head % CPU_TRACE_LEN] = CpuTrace {
                tick: crate::timer::ticks(),
                count: c.count,
                owner: c.owner,
                sleepers: c.sleepers,
                irqs: c.irqs_pending as u32,
                cur,
            };
            CPU_TRACE_HEAD = head + 1;
        }
    }
}

#[cfg(nk_lkl)]
pub fn lkl_cpu_trace_dump(n: usize) {
    unsafe {
        let head = CPU_TRACE_HEAD;
        let count = n.min(head).min(CPU_TRACE_LEN);
        println!("  lkl cpu trace (last {} of {} transitions):", count, head);
        for i in (head - count)..head {
            let e = &(*(&raw const CPU_TRACE))[i % CPU_TRACE_LEN];
            println!(
                "    t={} count {} sleepers {} irqs {} owner {} (uid {:#x}) cur {:#x}",
                e.tick,
                e.count,
                e.sleepers,
                e.irqs,
                e.owner,
                e.owner.wrapping_sub(1),
                e.cur
            );
        }
    }
}

/// One `__switch_to` or `switch_to_host_task` tid write, reported by LKL.
///
/// The CPU trace says *when* the dead label appeared; this says who put it
/// there: prev/next pid and tid, and which nk task was executing at the
/// time. A `prev_pid` of -1 is a tid assignment (the host task named by
/// `next_pid` was repointed at `next_tid`), not a switch.
#[cfg(nk_lkl)]
#[derive(Clone, Copy)]
struct SwitchTrace {
    tick: u64,
    prev_pid: i32,
    prev_tid: u64,
    next_pid: i32,
    next_tid: u64,
    cur: u64,
}

#[cfg(nk_lkl)]
const SW_TRACE_LEN: usize = 768;

#[cfg(nk_lkl)]
static mut SW_TRACE: [SwitchTrace; SW_TRACE_LEN] = [SwitchTrace {
    tick: 0,
    prev_pid: 0,
    prev_tid: 0,
    next_pid: 0,
    next_tid: 0,
    cur: 0,
}; SW_TRACE_LEN];
#[cfg(nk_lkl)]
static mut SW_TRACE_HEAD: usize = 0;

#[cfg(nk_lkl)]
#[no_mangle]
pub extern "C" fn nk_lkl_switch_trace(
    prev_pid: i32,
    prev_tid: usize,
    next_pid: i32,
    next_tid: usize,
) {
    unsafe {
        let head = SW_TRACE_HEAD;
        (*(&raw mut SW_TRACE))[head % SW_TRACE_LEN] = SwitchTrace {
            tick: crate::timer::ticks(),
            prev_pid,
            prev_tid: prev_tid as u64,
            next_pid,
            next_tid: next_tid as u64,
            cur: crate::sched::current_uid() as u64,
        };
        SW_TRACE_HEAD = head + 1;
    }
}

#[cfg(nk_lkl)]
#[no_mangle]
pub extern "C" fn nk_thread_alive(tid: usize) -> i32 {
    if tid == 0 {
        return 0;
    }
    match crate::sched::from_uid(tid - 1) {
        Some(t) => (!crate::sched::finished(t)) as i32,
        None => 0,
    }
}

#[cfg(nk_lkl)]
pub fn lkl_switch_trace_dump(n: usize) {
    unsafe {
        let head = SW_TRACE_HEAD;
        let count = n.min(head).min(SW_TRACE_LEN);
        println!("  lkl switch trace (last {} of {}):", count, head);
        for i in (head - count)..head {
            let e = &(*(&raw const SW_TRACE))[i % SW_TRACE_LEN];
            let kind = if e.prev_pid < 0 { "tid=" } else { "sw " };
            println!(
                "    t={} {} prev pid {} tid {:#x} -> next pid {} tid {:#x} cur {:#x}",
                e.tick, kind, e.prev_pid, e.prev_tid, e.next_pid, e.next_tid, e.cur
            );
        }
    }
}

/// Release LKL's CPU when the host thread running on it dies, or repair a
/// label that names a thread which can never run again.
///
/// `lkl_cpu_release` is a local symbol in the archive, so nk cannot call it,
/// and `lkl_cpu_put_if_owned` refuses whenever the owner label does not match
/// the caller -- which is exactly what a dying thread sees when the label
/// names a dead incarnation's id (measured: count 1, owner a uid whose
/// generation is long gone, irqs_pending 1, every later syscall queued
/// behind cpu.sem for ever).
///
/// Two callers, with different rights:
///
///   * the Linux thread trampoline at thread exit (`stalled_seconds` 0):
///     the caller *is* the owner and is about to stop existing, so this
///     mirrors `lkl_cpu_drop` -- one count per call, never the whole
///     nesting, because count > 1 means acquisitions with no other put.
///   * the watchdog, once a second: the caller is nobody in particular,
///     so it may only clear a label proven dead -- the task is gone or
///     finished -- and only when count == 1. A blocked-but-live owner is
///     deliberately left alone: if it ever wakes it resumes inside Linux,
///     and letting a second host thread in would run Linux on two stacks
///     at once, which is silent corruption rather than a wedge.
///
/// Interrupts are masked across the update: this is the only writer that
/// runs without LKL's own mutex (which may itself be held by the dead
/// context), and on one CPU a masked section is atomic against everything
/// except the handler, which is also masked.
#[cfg(nk_lkl)]
pub fn lkl_cpu_release_if_dying(stalled_seconds: u32) {
    extern "C" {
        fn nk_sem_up(s: *mut u8);
    }
    unsafe {
        let c = &mut *(lkl_cpu_addr() as *mut LklCpu);
        let flags = crate::sync::irq_save();
        if c.count != 0 && c.owner != 0 {
            let owner_uid = c.owner as usize - 1;
            if owner_uid == crate::sched::current_uid() {
                // The dying thread itself: lkl_cpu_drop's body.
                c.count -= 1;
                if c.count == 0 {
                    if c.sleepers != 0 {
                        c.sleepers -= 1;
                        if !c.sem.is_null() {
                            nk_sem_up(c.sem);
                        }
                    }
                    c.owner = 0;
                }
            } else {
                let owner_state = crate::sched::from_uid(owner_uid);
                let dead = match owner_state {
                    None => true,
                    Some(t) => crate::sched::finished(t),
                };
                if dead && c.count == 1 {
                    let sleepers = c.sleepers;
                    c.count = 0;
                    if c.sleepers != 0 {
                        c.sleepers -= 1;
                        if !c.sem.is_null() {
                            nk_sem_up(c.sem);
                        }
                    }
                    c.owner = 0;
                    crate::sync::irq_restore(flags);
                    println!(
                        "  lkl cpu: released dead owner uid {:#x} (sleepers {})",
                        owner_uid, sleepers
                    );
                    return;
                }
                if dead {
                    crate::sync::irq_restore(flags);
                    println!(
                        "  lkl cpu: DEAD owner uid {:#x} with count {} -- never flatten nesting, stopping",
                        owner_uid, c.count
                    );
                    return;
                }
                let blocked = owner_state.map_or(false, |t| crate::sched::blocked(t));
                if blocked && stalled_seconds == 10 {
                    println!(
                        "  lkl cpu: owner uid {:#x} live but blocked 10s -- not releasing; see the wait graph",
                        owner_uid
                    );
                }
            }
        }
        crate::sync::irq_restore(flags);
    }
}

/// What the driver told the device, and what the device says back.
///
/// A blocked probe is invisible from inside Linux -- the task that would have
/// reported it is the one that is waiting -- but the registers it programmed
/// are still there to be read, and reading them says whether the queue was
/// ever set up and whether the device has an interrupt pending that nobody
/// collected.
#[cfg(nk_lkl)]
fn report_virtio() {
    let base = unsafe { PROBE };
    if base == 0 {
        return;
    }
    unsafe {
        println!(
            "  virtio {:#x}: status {:#x} intr {:#x}",
            base,
            crate::mmio::readl(base + 0x70), // Status
            crate::mmio::readl(base + 0x60), // InterruptStatus
        );
        // Queue 0's ring indices: where a stalled request actually stopped.
        // avail.idx > used.idx means Linux handed the device work it has not
        // completed -- the stall is in the device or the notify. Equal means
        // the device finished and the stall is on the Linux side: the
        // completion interrupt or the wake-up behind it went missing. The
        // distinction was guesswork without this line.
        crate::mmio::writel(base + 0x30, 0); // QueueSel = 0
        let ready = crate::mmio::readl(base + 0x44); // QueueReady (modern)
        let desc = crate::mmio::readl(base + 0x80) as usize; // QueueDescLow
        let avail = crate::mmio::readl(base + 0x90) as usize; // QueueDriverLow
        let used = crate::mmio::readl(base + 0xa0) as usize; // QueueDeviceLow
        let avail_idx = if avail != 0 {
            core::ptr::read_volatile((avail + 2) as *const u16)
        } else {
            0
        };
        let used_idx = if used != 0 {
            core::ptr::read_volatile((used + 2) as *const u16)
        } else {
            0
        };
        println!(
            "          queue0 ready {} desc {:#x} avail {:#x} used {:#x} avail.idx {} used.idx {}",
            ready, desc, avail, used, avail_idx, used_idx
        );
    }
}

/// Give Linux every virtio-mmio device the machine actually has.
#[cfg(nk_lkl)]
fn attach_virtio(fdt: &dt::Fdt) {
    const MAGIC: u32 = 0x7472_6976; // "virt", little-endian
    let mut found = 0;
    fdt.each_compatible("virtio,mmio", |node| {
        let Some((base, size)) = node.reg(0) else { return };
        let Some((_, spi, _)) = node.interrupt(0) else { return };
        // The transport is there whether or not anything is plugged into it;
        // the device id is what says which.
        let magic = unsafe { crate::mmio::readl(base as usize) };
        let id = unsafe { crate::mmio::readl(base as usize + 8) };
        if magic != MAGIC || id == 0 {
            return;
        }
        // Routed before it is registered, because registering it probes it
        // and the probe waits for the device to answer.
        let Some(irq) = (unsafe { lklirq::reserve(c"virtio-mmio") }) else {
            return;
        };
        lklirq::map_gic(spi + 32, irq);
        unsafe { gic::enable_spi(spi + 32) };
        unsafe { PROBE = base as usize };
        if !lkl::add_virtio_mmio(base, size, irq) {
            println!("  virtio: Linux refused the device at {:#x}", base);
            return;
        }

        println!(
            "  virtio: device {} at {:#x} -> Linux, GIC {} as LKL irq {}",
            id,
            base,
            spi + 32,
            irq
        );
        found += 1;
    });
    if found == 0 {
        println!("  virtio: no devices behind the transports");
    }
}

unsafe fn claim_memory(fdt: &dt::Fdt, ram_start: usize, ram_end: usize) {
    extern "C" {
        static __image_end: u8;
    }
    #[cfg(target_arch = "aarch64")]
    let image_start = 0x4008_0000usize; // where linker.ld links; see boot.s
    #[cfg(target_arch = "x86_64")]
    let image_start = 0x4000_0000usize; // 1GB; see linker-x86_64.ld
    let image_end = &raw const __image_end as usize;

    // The initial ramdisk sits in RAM like everything else and nothing else
    // knows it is there. Handing its pages to the allocator does not fail
    // anywhere near the initrd: it fails later, in whatever happened to be
    // given the page, with the archive's bytes in it.
    let (istart, iend) = fdt.initrd().unwrap_or((0, 0));
    // Linux's own physical memory, when Linux is managing memory. Fixed
    // rather than allocated, because LKL's __pa() is the identity and a
    // device has to be programmed with an address that is really there; see
    // paging::LINUX_PHYS_BASE.
    #[cfg(nk_lkl)]
    let linux = (
        paging::LINUX_PHYS_BASE as usize,
        (paging::LINUX_PHYS_BASE + paging::LINUX_PHYS_SIZE) as usize,
    );
    #[cfg(not(nk_lkl))]
    let linux = (0, 0);

    let mut reserved = [
        (image_start, image_end),
        (fdt.base(), fdt.base() + fdt.total_size()),
        (istart as usize, iend as usize),
        linux,
    ];
    reserved.sort_unstable();

    let mut at = ram_start;
    for (rs, re) in reserved {
        if rs > at {
            frames::add(at, rs.min(ram_end));
        }
        at = at.max(re);
    }
    if at < ram_end {
        frames::add(at, ram_end);
    }
}

#[cfg(target_arch = "aarch64")]
fn current_el() -> u64 {
    let el: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)) };
    el >> 2
}

/// Mask interrupts and stop.
pub fn halt() -> ! {
    #[cfg(target_arch = "aarch64")]
    unsafe { core::arch::asm!("msr daifset, #0xf", options(nomem, nostack)) };
    #[cfg(target_arch = "x86_64")]
    unsafe { core::arch::asm!("cli", options(nomem, nostack)) };
    loop {
        crate::arch::halt();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // Deliberately uses the raw console rather than anything that could be
    // locked or allocated: the panic path has to work when the reason for the
    // panic is that one of those is broken.
    raw_println!();
    raw_println!("!! kernel panic");
    if let Some(loc) = info.location() {
        raw_println!("   at {}:{}:{}", loc.file(), loc.line(), loc.column());
    }
    raw_println!("   {}", info.message());
    halt();
}

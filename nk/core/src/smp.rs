//! Symmetric multiprocessing bring-up.
//!
//! One CPU boots from firmware; the rest start in real mode at a page the
//! BSP chooses, climb through the trampoline in boot-x86_64.s (or the park
//! loop in boot.s), and arrive here. This module is the Rust half: copying
//! the trampoline, sequencing INIT/STARTUP, and initializing each AP's
//! interrupt controller, timer, TSS, and scheduler state.
//!
//! Gated by NK_SMP at compile time. Without it, `bring_up` is a no-op and
//! the kernel is exactly the single-CPU kernel: same codegen on the boot
//! path, same behavior, same metrics. SMP is opt-in until the torture tests
//! below pass on real multi-CPU boots.

use crate::println;

/// Maximum APs to start. Matches MAX_CPUS in sched and the GDT/TSS tables.
pub const MAX_APS: usize = crate::sched::MAX_CPUS - 1;

/// How many CPUs bring-up actually found online, recorded at the end of
/// `bring_up`. The torture compares the scheduler's count against this: an
/// SMP-capable build booted on one CPU (`-smp 1`) is a configuration this
/// project tests on purpose, so "the build has SMP" is not the same claim as
/// "this machine has more than one CPU".
static CPUS_ONLINE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(1);

pub fn cpus_online() -> usize {
    CPUS_ONLINE.load(core::sync::atomic::Ordering::Relaxed).max(1)
}

fn record_cpus_online() {
    let mut n = 0usize;
    for cpu in 0..crate::sched::MAX_CPUS {
        if crate::sched::current_cpu_state(cpu).2 {
            n += 1;
        }
    }
    CPUS_ONLINE.store(n.max(1), core::sync::atomic::Ordering::Relaxed);
}

/// Where the AP trampoline is copied: a page below 1MB that firmware and
/// QEMU both leave alone. 0x8000 is the conventional spot -- above the IVT
/// and BDA, below the option ROMs.
#[cfg(target_arch = "x86_64")]
const TRAMPOLINE_PAGE: usize = 0x8000;

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    static ap_trampoline_src: u64;
    static ap_trampoline_len: u64;
    static ap_prot: u8;
    static mut ap_stack: [u64; crate::sched::MAX_CPUS];
    static mut ap_ready: [u64; crate::sched::MAX_CPUS];
    static mut ap_go: [u64; crate::sched::MAX_CPUS];
    static mut ap_slot: u64;
    static ap_gdt_ptr: u8;
    static mut ap_gdt_base: u32;
    static ap_gdt32: u8;
    static mut ap_far_imm: u32;
    static mut ap_cr3_slot: u32;
    static mut ap_gdt64_ptr: [u32; 3];
    static pml4: u8;
    static gdt: u8;
    static gdt_end: u8;
}

/// AP stacks: one 16KB stack per CPU, page-aligned, in .bss.
#[cfg(target_arch = "x86_64")]
#[repr(align(16))]
#[derive(Clone, Copy)]
struct ApStack([u8; 16384]);

#[cfg(target_arch = "x86_64")]
static mut AP_STACKS: [ApStack; MAX_APS] = [ApStack([0; 16384]); MAX_APS];

/// Bring up APs. No-op without NK_SMP.
pub fn bring_up() {
    if option_env!("NK_SMP").is_none() {
        return;
    }
    println!("  smp:    bring-up starting");
    // From here on, more than one CPU can print. Enabled before any
    // secondary exists, so its own announcements are serialised too.
    crate::sync::enable_console_lock();

    // An idle task for the boot CPU, exactly as each AP gets one.
    //
    // Without it the boot CPU has no fallback task. When the boot task has
    // migrated elsewhere and some other task on cpu 0 finishes -- a storm
    // worker, an exiting process -- `task_exit` yields with nothing
    // pickable, and that Finished task then sits as cpu 0's "current" for
    // ever: one wake per tick, never switched away, and so never reclaimed
    // (`reclaim_finished` refuses a task that still has `on_cpu` set). The
    // APs never had this problem because they each spawn an idle task below.
    if crate::sched::cpu_id() == 0 {
        let idle = crate::sched::spawn("idle", idle_entry, 0);
        crate::sched::set_class(idle, crate::schedpolicy::Class::Background);
        crate::sched::set_affinity(idle, 0x01);
        crate::sched::register_idle_task(0, idle);
    }
    if option_env!("NK_SMP_PROBE").is_some() {
        println!("  smp:    probe only, not starting APs");
        return;
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        bring_up_x86();
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        bring_up_arm();
    }
    record_cpus_online();
}

/// Which AP bring-up layer is live. Raised one step at a time, in order:
/// IDT, then TSS, then LAPIC, then the scheduler slot, then the timer.
/// Each step is verified on a real multi-CPU boot before the next is
/// switched on -- the layer that wedges the AP is then the layer just
/// added, not a guess across five of them.
#[cfg(target_arch = "x86_64")]
static AP_LAYER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Entry point for x86 APs from the trampoline. Never returns.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn rust_ap_main(cpu: u64) -> ! {
    let cpu = (cpu as usize).min(MAX_APS);
    // 'R': entered Rust. Everything before this was the trampoline; if
    // this byte is missing the AP never left assembly.
    outb_0xe9(b'R');

    // `syscall` MSRs, unconditionally and before any layer.
    //
    // These are per-CPU MSRs -- EFER.SCE, STAR, LSTAR, FMASK -- so reset
    // leaves them clear on every CPU independently, and the BSP's Stage 1
    // setup does not reach an AP. The trampoline sets EFER.LME and NXE (it
    // has to, for long mode) but not SCE, so before this an AP ran ring 3
    // with `syscall` still an invalid opcode: the first system call from a
    // user task scheduled on that AP died with #UD *at the syscall
    // instruction*, which reads as a corrupt program rather than a CPU that
    // was never told the instruction exists. Found by booting the x86
    // userland under AP_LAYER=4, where the init fixture's first syscall
    // faulted on cpu 3 at 0x4012a6.
    unsafe {
        extern "C" {
            fn x86_syscall_init();
        }
        x86_syscall_init();
    }

    let layer = AP_LAYER.load(core::sync::atomic::Ordering::Relaxed);
    if layer >= 1 {
        crate::interrupts_x86::idt_init();
        outb_0xe9(b'S');
    }
    if layer >= 2 {
        crate::tss_x86::init_cpu(cpu, ap_stack_top(cpu));
        outb_0xe9(b'T');
    }
    if layer >= 3 {
        crate::lapic::init();
        outb_0xe9(b'U');
    }
    if layer >= 4 {
        crate::sched::init_secondary(cpu);
        // Every CPU needs a real task to be "current" before its first
        // tick: schedule_on accounts the outgoing task, and usize::MAX is
        // not a task. The idle task is that placeholder -- it runs only
        // when nothing else on this CPU is runnable, which is exactly the
        // state an AP is in from birth.
        let idle = crate::sched::spawn("idle", ap_idle, cpu);
        crate::sched::set_class(idle, crate::schedpolicy::Class::Background);
        crate::sched::set_affinity(idle, 1u16 << cpu);
        // The AP's idle loop runs on the stack the trampoline set up, not on
        // the pool allocation `spawn` gave the placeholder task, so record
        // the stack that is really in use. See `sched::set_task_stack`.
        crate::sched::set_task_stack(idle, (ap_stack_top(cpu) - 16384) as usize, 16384);
        crate::sched::online_cpu(cpu, idle);
        crate::sched::register_idle_task(cpu, idle);
        println!("  smp:    cpu{} online (apic {})", cpu, crate::lapic::id());
        // This CPU's own tick, started only once a valid task is current:
        // the timer handler schedules, and scheduling on usize::MAX is the
        // panic this ordering exists to avoid. Without a tick the AP never
        // preempts -- it runs whatever it picked first until that task
        // blocks, and hogs sit Ready on an idle CPU while the BSP does
        // all the sharing. The PIT belongs to the BSP; each AP times
        // itself with its LAPIC, calibrated once from the shared bus rate.
        crate::lapic::start_timer(crate::timer::TIMER_VECTOR);
        // Unmask interrupts on this CPU. The AP arrives from STARTUP with
        // IF clear and nothing before this point needs it: runqueue
        // membership and APIC delivery precede any interrupt. Without this
        // the CPU can steal work it never runs, because the reschedule IPI
        // that should wake it stays pending behind a masked flag -- which
        // is why the scheduler torture stalled with every AP "online".
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
        outb_0xe9(b'V');
    }
    loop {
        if crate::sched::take_need_resched_cpu(cpu) {
            crate::sched::schedule();
        } else if crate::sched::schedule() {
            // Stole or picked local work above; loop back and run it.
        } else {
            // Nothing runnable anywhere this CPU may take from. Try one
            // steal from the busiest CPU before halting, so an idle AP
            // pulls work instead of waiting to be handed it.
            if crate::sched::balance_on_idle(cpu) {
                crate::sched::schedule();
            } else {
                crate::arch::halt();
            }
        }
    }
}

/// The per-CPU idle task. Runs only when its CPU has nothing else: it
/// halts in a loop, and any wakeup preempts it through the normal path.
/// A real task rather than a null current, so accounting, pick, and the
/// tick handler never touch an invalid slot.
extern "C" fn idle_entry(_arg: usize) {
    loop {
        crate::arch::halt();
    }
}

#[cfg(target_arch = "x86_64")]
extern "C" fn ap_idle(_arg: usize) {
    idle_entry(_arg)
}

#[cfg(target_arch = "x86_64")]
fn outb_0xe9(b: u8) {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") 0xe9u16, in("al") b,
            options(nomem, nostack, preserves_flags));
    }
}

/// Set the AP bring-up layer. Called by the BSP before starting APs;
/// NK_AP_LAYER names it at build time.
pub fn set_ap_layer(layer: u32) {
    #[cfg(target_arch = "x86_64")]
    AP_LAYER.store(layer, core::sync::atomic::Ordering::Relaxed);
    let _ = layer;
}

pub fn ap_layer() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        AP_LAYER.load(core::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

/// A slot's offset within the trampoline, from its link address.
#[cfg(target_arch = "x86_64")]
fn copy_of(sym: *const impl Sized, src: usize) -> usize {
    (sym as usize).wrapping_sub(src)
}

/// That offset, as an address in the copy.
#[cfg(target_arch = "x86_64")]
fn at_copy(off: usize) -> usize {
    TRAMPOLINE_PAGE + off
}

#[cfg(target_arch = "x86_64")]
fn ap_stack_top(cpu: usize) -> u64 {
    unsafe { (&raw const AP_STACKS[cpu.saturating_sub(1).min(MAX_APS - 1)].0 as u64) + 16384 }
}

#[cfg(target_arch = "x86_64")]
unsafe fn bring_up_x86() {
    println!("  smp:    reading trampoline descriptor");
    let src = core::ptr::read_volatile(&raw const ap_trampoline_src) as usize;
    let len = core::ptr::read_volatile(&raw const ap_trampoline_len) as usize;
    println!("  smp:    trampoline src {:#x} len {}", src, len);
    if len == 0 || len >= 4096 {
        println!("  smp:    trampoline length implausible -- not starting APs");
        return;
    }
    core::ptr::copy_nonoverlapping(src as *const u8, TRAMPOLINE_PAGE as *mut u8, len);

    // Patch the copy's slots: each slot's offset within the copy equals its
    // offset within the source (same bytes, same layout), so subtract the
    // source base and add the copy base.
    //
    // `src`, the value read out of the descriptor -- not
    // `ap_trampoline_src as usize`, which is the address *of* that word and
    // sits in .data, megabytes from the trampoline in .text.boot. Every
    // offset computed from it was wrong by that distance, so the patches
    // landed outside the copy entirely: the far jump was written with a
    // plausible-looking target that pointed at whatever the arithmetic
    // produced, and the AP jumped into zeros. The instruction trace showed
    // it exactly -- `ljmpl $0x8:$0x8064` followed by `00 00` at 0x8064.
    let src_base = src;
    // CR3: the BSP's live tables.
    let cr3: u64;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    // GDT base and length for the 64-bit reload.
    let gdt_base = &raw const gdt as usize;
    let gdt_len = (&raw const gdt_end as usize).wrapping_sub(gdt_base);
    // ap_prot's offset within the trampoline.
    let prot_off = (&raw const ap_prot as usize).wrapping_sub(src_base);
    patch_trampoline(TRAMPOLINE_PAGE, src_base, cr3 as u32, gdt_base, gdt_len, prot_off);

    println!("  smp:    trampoline {} bytes at {:#x}, cr3 {:#x}", len, TRAMPOLINE_PAGE, cr3);
    // What the AP will actually read, from the copy rather than the source:
    // every bug so far has been a patch landing at the wrong offset, and
    // the copy is the only place that shows it.
    {
        let gdtr = at_copy(copy_of(&raw const ap_gdt_ptr, src));
        let limit = core::ptr::read_volatile(gdtr as *const u16);
        let base = core::ptr::read_volatile((gdtr + 2) as *const u32);
        println!("  smp:    gdtr limit {:#x} base {:#x}", limit, base);
        for i in 0..3usize {
            let d = core::ptr::read_volatile((base as usize + i * 8) as *const u64);
            println!("  smp:      gdt[{}] {:#018x}", i, d);
        }
        let far_off = core::ptr::read_volatile(
            at_copy(copy_of(&raw const ap_far_imm, src)) as *const u32);
        let far_sel = core::ptr::read_volatile(
            (at_copy(copy_of(&raw const ap_far_imm, src)) + 4) as *const u16);
        println!("  smp:    far {:#x}:{:#x}", far_sel, far_off);

        // The bytes the AP will actually execute at the far-jump target,
        // read back from the copy. The source and the copy have disagreed
        // once already -- the instruction trace showed zeros where the
        // symbols promised code -- and this is the shortest way to tell
        // which of the two is lying.
        let prot = at_copy(prot_off);
        let s: &[u8] = core::slice::from_raw_parts(prot as *const u8, 8);
        println!(
            "  smp:    bytes at target {:#x}: {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}",
            prot, s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]
        );
        let o: &[u8] = core::slice::from_raw_parts((src + prot_off) as *const u8, 8);
        println!(
            "  smp:    bytes at source {:#x}: {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}",
            src + prot_off, o[0], o[1], o[2], o[3], o[4], o[5], o[6], o[7]
        );
    }

    if let Some(layer) = option_env!("NK_AP_LAYER") {
        if let Ok(n) = layer.parse::<u32>() {
            set_ap_layer(n);
            println!("  smp:    AP layer {}", n);
        }
    }

    let mut started = 0;
    // MADT supplies the targets: physical APIC IDs need not be dense, and
    // a missing/disabled slot is not the end of the processor list.
    for cpu in 1..crate::lapic::cpu_count() {
        let Some(apic) = crate::lapic::apic_id(cpu) else { continue };
        // Pass the dense topology slot to the trampoline, never the APIC
        // ID. Even QEMU's q35 has gaps with non-power-of-two core counts.
        ap_stack[cpu] = ap_stack_top(cpu);
        ap_ready[cpu] = 0;
        ap_go[cpu] = 0;
        core::ptr::write_volatile(&raw mut ap_slot, cpu as u64);
        crate::arch::memory_barrier();
        println!("  smp:    starting cpu{} (apic {})", cpu, apic);
        crate::lapic::start_ap(apic, (TRAMPOLINE_PAGE >> 12) as u8);
        if wait_ready(cpu, 20_000_000) {
            started += 1;
            println!("  smp:    cpu{} reached 64-bit mode and halted", cpu);
        } else {
            println!(
                "  smp:    cpu{} did not answer (ready={}) -- skipping this MADT target",
                cpu,
                core::ptr::read_volatile(&raw const ap_ready[cpu]),
            );
            continue;
        }
        if started >= MAX_APS {
            break;
        }
    }
    println!("  smp:    {} APs started, {} CPUs total", started, crate::sched::cpu_count());
}

/// Patch the trampoline copy's address slots.
///
/// Code and data travel together: every slot's offset within the copy equals
/// its link-time offset from `src`, so each copy address is
/// `copy_base + (link_addr - src_base)`. The 16-bit code in the copy then
/// reaches every slot with plain operands -- no segment arithmetic needed
/// beyond DS=CS, because the distances are identical to link time.
#[cfg(target_arch = "x86_64")]
unsafe fn patch_trampoline(
    copy: usize,
    src: usize,
    cr3: u32,
    gdt_base: usize,
    gdt_len: usize,
    prot_off: usize,
) {
    let at = |link: usize| copy + link.wrapping_sub(src);
    // 32-bit GDT base: the table travels inside the copy, so its runtime
    // address is copy + its offset within the trampoline. Taken from the
    // symbol rather than counted forward from another slot -- an earlier
    // version guessed `ap_gdt_base + 4` and landed six bytes short, on
    // ap_far, which pointed the GDTR at a far-jump target and made the AP
    // run off into memory the moment protected mode came on.
    let gdt32_off = (&raw const ap_gdt32 as usize).wrapping_sub(src);
    *(at(&raw const ap_gdt_base as usize) as *mut u32) = (copy + gdt32_off) as u32;
    // Far jump target: the 32-bit offset embedded in the JMP FAR ptr16:32
    // instruction itself. The selector half follows it in the encoding and
    // is already 0x08 in the source, so only these four bytes move.
    let target = copy + prot_off;
    *(at(&raw const ap_far_imm as usize) as *mut u32) = target as u32;
    // CR3: live tables.
    *(at(&raw const ap_cr3_slot as usize) as *mut u32) = cr3;
    // 64-bit GDT pointer: a 16-bit limit immediately followed by a 64-bit
    // base -- ten bytes, unaligned, exactly as LGDT reads it.
    //
    // Written as u16 then u64 at +2. Writing the limit as a u32 and the
    // base at `+1 u32` instead spans twelve bytes, and the last four land
    // on whatever follows: here that was `ap_prot`'s first instruction,
    // replaced with zeros. The AP then jumped to a correct address holding
    // rubble, which is why it neither arrived nor faulted.
    let p = at(&raw const ap_gdt64_ptr as usize);
    core::ptr::write_unaligned(p as *mut u16, ((gdt_len - 1) & 0xffff) as u16);
    core::ptr::write_unaligned((p + 2) as *mut u64, gdt_base as u64);
}

#[cfg(target_arch = "x86_64")]
fn wait_ready(cpu: usize, spins: u64) -> bool {
    for _ in 0..spins {
        if unsafe { ap_ready[cpu] } != 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

#[cfg(target_arch = "aarch64")]
/// The boot CPU's mailbox to the parked secondaries.
///
/// aarch64 does not need a trampoline copy the way x86 does: QEMU's `virt`
/// starts *every* CPU in `-smp` at `_start`, and `boot.s` parks the ones that
/// are not affinity 0 before it has done anything at all. So the release is a
/// value in memory plus an event, not an INIT/STARTUP dance -- and it cannot
/// use `static mut` fields in Rust order, because `park` reads it from
/// assembly with the MMU still off.
///
/// One 64-byte aligned block so a single `dc cvac` publishes all of it: the
/// boot CPU runs with caches on and the parked CPUs with the MMU (and so the
/// caches) off, which makes this a coherence problem, not a store.
#[cfg(target_arch = "aarch64")]
#[repr(C, align(64))]
pub struct ApMailbox {
    /// Set to `AP_RELEASE_MAGIC` by the boot CPU to release the others.
    /// Unused by the PSCI path -- nothing is parked -- and kept for the
    /// configuration where secondaries do arrive at `_start`.
    pub go: u32,
    pub _pad: u32,
    /// `reached[cpu] == cpu + 1` once that CPU has entered its first
    /// instruction. Zero means it never arrived.
    pub reached: [u32; MAX_APS],
    pub _pad2: u32,
    /// The kernel stack top for each CPU, written by the boot CPU before it
    /// asks PSCI to start that CPU. The secondary reads it with the MMU off,
    /// so it is plain memory and the boot CPU cleans the line.
    pub stack_top: [u64; MAX_APS],
}

#[cfg(target_arch = "aarch64")]
const AP_RELEASE_MAGIC: u32 = 0x4e4b_4150;

#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub static mut AP_MAILBOX: ApMailbox = ApMailbox {
    go: 0,
    _pad: 0,
    reached: [0; MAX_APS],
    _pad2: 0,
    stack_top: [0; MAX_APS],
};

/// One 16 KiB stack per secondary, in .bss and therefore identity-mapped
/// before the MMU is on -- which it has to be, since the CPU loads SP from
/// one of these before it can turn the MMU on at all.
#[cfg(target_arch = "aarch64")]
#[repr(align(16))]
#[derive(Clone, Copy)]
struct ApStackArm([u8; 16384]);

#[cfg(target_arch = "aarch64")]
static mut AP_STACKS_ARM: [ApStackArm; MAX_APS] = [ApStackArm([0; 16384]); MAX_APS];

#[cfg(target_arch = "aarch64")]
fn ap_stack_top_arm(cpu: usize) -> u64 {
    unsafe { (&raw const AP_STACKS_ARM[cpu.min(MAX_APS - 1)].0 as u64) + 16384 }
}

#[cfg(target_arch = "aarch64")]
unsafe fn dc_cvac(addr: u64) {
    core::arch::asm!("dc cvac, {}", in(reg) addr, options(nostack, preserves_flags));
}

#[cfg(target_arch = "aarch64")]
unsafe fn dc_ivac(addr: u64) {
    core::arch::asm!("dc ivac, {}", in(reg) addr, options(nostack, preserves_flags));
}

#[cfg(target_arch = "aarch64")]
unsafe fn dsb_sy() {
    core::arch::asm!("dsb sy", options(nostack, preserves_flags));
}

#[cfg(target_arch = "aarch64")]
unsafe fn sev() {
    core::arch::asm!("sev", options(nostack, preserves_flags));
}

/// Milestone 1: release the parked CPUs and see which ones answer.
///
/// The question this stage answers is empirical -- how many CPUs the machine
/// actually started, and whether `wfe`/`sev` reaches them at all -- so it
/// deliberately stops before stacks, MMU or Rust. A CPU that arrives sets a
/// word in the mailbox and waits; the boot CPU cleans, releases, and reports.
#[cfg(target_arch = "aarch64")]
unsafe fn bring_up_arm() {
    unsafe extern "C" {
        fn secondary_entry();
    }
    let entry = secondary_entry as *const () as u64;

    println!("  smp:    ARM bring-up: PSCI CPU_ON, entry {:#x}", entry);

    let mb = &raw mut AP_MAILBOX;
    (*mb).reached = [0; MAX_APS];
    // Not the parked-CPU magic: under HVF nothing is sitting in `park`
    // waiting for it, and leaving it unset keeps that path inert so a CPU
    // that *does* arrive at `_start` cannot confuse this stage's count.
    (*mb).go = 0;
    // Each secondary's stack, before that CPU exists: it loads SP from here
    // before it can do anything else, so this cannot be handed over later.
    for cpu in 0..MAX_APS {
        (*mb).stack_top[cpu] = ap_stack_top_arm(cpu);
    }
    // Publish both cache lines: the secondaries read them with the MMU off.
    dc_cvac(mb as u64);
    dc_cvac((mb as u64) + 64);
    dsb_sy();

    // Targets are MPIDR values. On `virt` the secondary affinities are
    // 1..n-1 in order, which is what this stage walks; the device tree is the
    // authority and replaces this once the path works.
    let mut started = 0usize;
    for target in 1..MAX_APS as u64 {
        // Every fact the BSP supplies: the target MPIDR affinity, the logical
        // slot it intends that target to become, the entry address, and the
        // context_id PSCI carries to the secondary. The secondary records
        // what it actually saw; the two are compared in the identity report.
        println!("  smp:    CPU_ON target aff0 {} entry {:#x} context_id {} stack_top {:#x}",
            target, entry, target, (*mb).stack_top[target as usize]);
        let rc = crate::psci::cpu_on(target, entry, target);
        if rc == 0 {
            started += 1;
        } else if rc != -2 {
            println!("  smp:    cpu{} refused: PSCI {}", target, rc);
        }
    }
    println!("  smp:    {} CPUs accepted CPU_ON", started);

    // Wait for the arrivals, bounded by the counter rather than a tick.
    let start = crate::arch::cycles();
    let mut answered = 0usize;
    loop {
        dc_ivac(mb as u64);
        dsb_sy();
        answered = (0..MAX_APS).filter(|&i| (*mb).reached[i] != 0).count();
        if answered >= MAX_APS {
            break;
        }
        let waited = crate::arch::cycles().wrapping_sub(start);
        if crate::timer::cycles_to_ns(waited) >= 300_000_000 {
            break;
        }
    }
    println!("  smp:    {} secondary entries answered", answered);

    // Now wait for them to reach the scheduler, which is the thing that
    // actually has to happen: a CPU that entered the trampoline and then
    // faulted would have answered the mailbox and never arrived here. The
    // state is in the scheduler, so there is no second mailbox flag to keep
    // in step with it.
    let start = crate::arch::cycles();
    let mut online = 0usize;
    loop {
        online = (0..MAX_APS)
            .filter(|&c| crate::sched::current_cpu_state(c).2)
            .count();
        if online >= answered + 1 {
            break;
        }
        let waited = crate::arch::cycles().wrapping_sub(start);
        if crate::timer::cycles_to_ns(waited) >= 500_000_000 {
            break;
        }
    }
    println!("  smp:    {} CPUs online", online);
    for cpu in 0..MAX_APS {
        if crate::sched::current_cpu_state(cpu).2 {
            println!("  smp:    cpu{} online", cpu);
        }
    }
    counter_sample_report(online);
}

/// Where a secondary lands after its stack is set: Rust, with translation
/// about to come on.
///
/// The half of `paging::init` that matters here is the per-CPU half -- the
/// identity map is already built and global -- so this calls only
/// `enable_translation`, then proves the CPU is really running nk's kernel
/// by printing through the same console as the boot CPU.
#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub unsafe extern "C" fn rust_ap_main_arm(cpu: u64) -> ! {
    let cpu = (cpu as usize).min(MAX_APS);
    crate::paging::enable_translation();
    println!("  smp:    secondary {} entering scheduler bring-up", cpu);

    crate::arch::set_vectors();

    // This CPU's redistributor, its CPU interface, and the two private
    // interrupts it needs: the timer that gives it a tick, and the SGI that
    // lets another CPU wake it.
    let (redist, stride, timer_intid) = crate::gic::secondary_params();
    crate::gic::init_secondary(redist, stride, timer_intid);

    // A scheduler slot, and a task to run when nothing else wants this CPU.
    // Same shape as the x86 AP path: `online_cpu` marks the task Running
    // because this CPU starts *inside* it, with no switch to record it.
    crate::sched::init_secondary(cpu);
    let idle = crate::sched::spawn("idle", idle_entry, cpu);
    crate::sched::set_class(idle, crate::schedpolicy::Class::Background);
    crate::sched::set_affinity(idle, 1u16 << cpu);
    // The AP's idle loop runs on the stack `secondary_entry` set up, not on
    // the pool allocation `spawn` gave the placeholder task, so record the
    // stack that is really in use. See `sched::set_task_stack`.
    crate::sched::set_task_stack(idle, (ap_stack_top_arm(cpu) - 16384) as usize, 16384);
    crate::sched::online_cpu(cpu, idle);
    crate::sched::register_idle_task(cpu, idle);

    // Its own tick, started only now that a valid task is current: the
    // handler schedules, and scheduling on usize::MAX is the panic this
    // ordering exists to avoid.
    crate::timer::start_this_cpu();

    // Interrupts on. Everything before this point is per-CPU state that
    // must exist before the first interrupt can be taken.
    core::arch::asm!("msr daifclr, #0xf", options(nomem, nostack));
    println!("  smp:    cpu{} online", cpu);

    // The same idle loop the x86 APs run: take a local reschedule if one is
    // pending, pick work if there is any, try one steal, otherwise stop.
    loop {
        {
            // The loop carries the cpu it was built for. If this body is ever
            // resumed on a different CPU, every per-CPU call below names the
            // wrong CPU while the switch still happens here -- which is
            // exactly what "crossed current values" looks like from outside.
            //
        }
        if crate::sched::take_need_resched_cpu(cpu) {
            crate::sched::schedule();
            // This frame is resumed *inside* `schedule_on`, past the check at
            // the top of the loop. If another CPU resumed it, this is the
            // first instruction where that shows.
        } else {
            if crate::sched::schedule() {
                // Picked work above; loop back and run it.
            } else if crate::sched::balance_on_idle(cpu) {
                crate::sched::schedule();
            } else {
                crate::arch::halt();
            }
        }
    }
}

/// One-shot, near-simultaneous counter sample from every CPU.
///
/// QEMU/HVF and the ARM architecture both *intend* CNTVCT to be a
/// system-wide timebase, so "the counter is per-CPU" is a claim that needs
/// measuring rather than asserting: the `wait_until` timeout was rewritten
/// because `cycles() - start` went backwards across a migration, and that
/// has to be explained by the actual numbers.
static CNT_GO: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static CNT_READY: [core::sync::atomic::AtomicBool; MAX_APS] =
    [const { core::sync::atomic::AtomicBool::new(false) }; MAX_APS];
static mut CNT_VIRT: [u64; MAX_APS] = [0; MAX_APS];
static mut CNT_PHYS: [u64; MAX_APS] = [0; MAX_APS];

#[cfg(target_arch = "aarch64")]
unsafe fn read_cntvct() -> u64 {
    let v: u64;
    core::arch::asm!("mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack));
    v
}

#[cfg(target_arch = "aarch64")]
unsafe fn read_cntpct() -> u64 {
    let v: u64;
    core::arch::asm!("mrs {}, cntpct_el0", out(reg) v, options(nomem, nostack));
    v
}

/// Called by each secondary after it is online: wait for the boot CPU's go,
/// then sample both counters as close together as possible.
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn counter_sample_secondary(cpu: usize) {
    let deadline = read_cntvct().wrapping_add(24_000_000); // ~1 s at 24 MHz
    while !CNT_GO.load(core::sync::atomic::Ordering::Acquire) {
        if read_cntvct() > deadline {
            return;
        }
    }
    let v = read_cntvct();
    let p = read_cntpct();
    CNT_VIRT[cpu.min(MAX_APS - 1)] = v;
    CNT_PHYS[cpu.min(MAX_APS - 1)] = p;
    CNT_READY[cpu.min(MAX_APS - 1)].store(true, core::sync::atomic::Ordering::Release);
}

/// Boot CPU: trigger the sample, take its own, wait for the rest, report.
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn counter_sample_report(online: usize) {
    CNT_GO.store(true, core::sync::atomic::Ordering::Release);
    CNT_VIRT[0] = read_cntvct();
    CNT_PHYS[0] = read_cntpct();
    let deadline = CNT_VIRT[0].wrapping_add(48_000_000);
    while read_cntvct() < deadline {
        let mut all = true;
        for c in 1..online {
            if !CNT_READY[c].load(core::sync::atomic::Ordering::Acquire) {
                all = false;
            }
        }
        if all {
            break;
        }
    }
    crate::println!("  smp:    counters (virt / phys, delta vs cpu0):");
    for c in 0..online {
        crate::println!("    cpu{}: cntvct {:#x} cntpct {:#x} d_virt {} d_phys {}",
            c, CNT_VIRT[c], CNT_PHYS[c],
            (CNT_VIRT[c] as i64 - CNT_VIRT[0] as i64),
            (CNT_PHYS[c] as i64 - CNT_PHYS[0] as i64));
    }
}

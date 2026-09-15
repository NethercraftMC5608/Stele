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
pub const MAX_APS: usize = 7;

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
    static mut ap_stack: [u64; 8];
    static mut ap_ready: [u64; 8];
    static mut ap_go: [u64; 8];
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
        crate::sched::set_affinity(idle, 1u8 << cpu.min(7));
        crate::sched::online_cpu(cpu, idle);
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
            crate::sched::schedule_on(cpu);
        } else if crate::sched::schedule_on(cpu) {
            // Stole or picked local work above; loop back and run it.
        } else {
            // Nothing runnable anywhere this CPU may take from. Try one
            // steal from the busiest CPU before halting, so an idle AP
            // pulls work instead of waiting to be handed it.
            if crate::sched::balance_on_idle(cpu) {
                crate::sched::schedule_on(cpu);
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
#[cfg(target_arch = "x86_64")]
extern "C" fn ap_idle(_arg: usize) {
    loop {
        crate::arch::halt();
    }
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
    unsafe { (&raw const AP_STACKS[cpu.min(MAX_APS - 1)].0 as u64) + 16384 }
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

    let bsp_id = crate::arch::cpu_id();
    let mut started = 0;
    // Stop at the first CPU that does not answer. QEMU's q35 numbers APIC
    // IDs densely from 0, so a gap means there are no more -- continuing
    // would spend the full timeout on every absent ID in turn, which is
    // what made an early version look like a hung boot.
    for apic in 0..8u32 {
        if apic as usize == bsp_id {
            continue;
        }
        // The logical slot is the loop's own counter, not the APIC ID: see
        // the note at `ap_slot` in boot-x86_64.s. They happen to coincide
        // on QEMU's q35 and must not be assumed to in general.
        let cpu = started + 1;
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
                "  smp:    cpu{} did not answer (ready={}) -- assuming no more CPUs",
                cpu,
                core::ptr::read_volatile(&raw const ap_ready[cpu]),
            );
            break;
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
unsafe fn bring_up_arm() {
    // PSCI CPU_ON path: implemented in the ARM stage.
    println!("  smp:    ARM bring-up not yet implemented");
}

// SPDX-License-Identifier: GPL-2.0-or-later
//! High-resolution one-shot timers on dedicated cores (ROADMAP Phase 3).
//!
//! nk's clock ticks at 100 Hz and its "one-shot" timers are deadlines checked
//! on that tick, so nothing fires sooner than 10 ms. A guest vCPU's ARM
//! generic timer needs microseconds. A dedicated core (`sched::dedicate_cpu`)
//! has no periodic tick, so its LAPIC timer is free: this module owns it for
//! the core's one task and fires a callback at a TSC deadline.
//!
//! The LAPIC's TSC-deadline mode is used when CPUID advertises it (the
//! deadline is written to IA32_TSC_DEADLINE directly, in TSC cycles); without
//! it, one-shot count mode is programmed from the calibrated bus rate.
//!
//! One timer per core and one callback, run in interrupt context: it must be
//! bounded and must not block or take the console lock -- record and return,
//! as a guest timer does by marking its interrupt pending.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

pub const VECTOR: u8 = 0xee;
const MAX_CPUS: usize = 8;
const IA32_TSC_DEADLINE: u32 = 0x6e0;

static CALLBACK: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static OWNED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static FIRED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

fn cpu() -> usize {
    (crate::arch::cpu_id() as usize).min(MAX_CPUS - 1)
}

/// Whether this CPU's LAPIC has TSC-deadline mode (CPUID.01H:ECX bit 24).
pub fn tsc_deadline_supported() -> bool {
    let ecx: u32;
    unsafe {
        core::arch::asm!(
            "mov {tmp}, rbx",
            "cpuid",
            "xchg {tmp}, rbx",
            tmp = out(reg) _,
            inout("eax") 1u32 => _,
            out("ecx") ecx,
            out("edx") _,
            options(nomem, nostack, preserves_flags),
        );
    }
    ecx & (1 << 24) != 0
}

/// Take this CPU's LAPIC timer for one-shot use: periodic ticking stops and
/// nothing fires until `arm_at`. Only for a dedicated core, from its task.
pub fn claim_this_cpu() {
    let c = cpu();
    CALLBACK[c].store(0, Ordering::Release);
    unsafe { crate::lapic::oneshot_mode(VECTOR, tsc_deadline_supported()) };
    OWNED[c].store(true, Ordering::Release);
}

/// Give the LAPIC timer back to the periodic scheduler tick.
pub fn release_this_cpu() {
    let c = cpu();
    cancel();
    OWNED[c].store(false, Ordering::Release);
    unsafe { crate::lapic::start_timer(crate::timer::TIMER_VECTOR) };
}

/// Fire `callback` (in interrupt context, on this CPU) at TSC `deadline`.
/// Replaces any timer already armed. False if this CPU's timer is not
/// claimed.
pub fn arm_at(deadline: u64, callback: fn()) -> bool {
    let c = cpu();
    if !OWNED[c].load(Ordering::Acquire) {
        return false;
    }
    CALLBACK[c].store(callback as usize, Ordering::Release);
    if tsc_deadline_supported() {
        unsafe {
            core::arch::asm!("wrmsr", in("ecx") IA32_TSC_DEADLINE,
                in("eax") deadline as u32, in("edx") (deadline >> 32) as u32,
                options(nomem, nostack));
        }
    } else {
        let now = crate::arch::cycles();
        let ns = crate::timer::cycles_to_ns(deadline.saturating_sub(now));
        let ticks = (ns as u128 * crate::lapic::bus_hz() as u128 / 16 / 1_000_000_000) as u64;
        unsafe { crate::lapic::set_initial_count(ticks.clamp(1, u32::MAX as u64) as u32) };
    }
    true
}

/// Fire `callback` `ns` nanoseconds from now.
pub fn arm_in_ns(ns: u64, callback: fn()) -> bool {
    let hz = crate::timer::cycle_hz();
    if hz == 0 {
        return false;
    }
    let cycles = (ns as u128 * hz as u128 / 1_000_000_000) as u64;
    arm_at(crate::arch::cycles() + cycles.max(1), callback)
}

/// Disarm this CPU's timer.
pub fn cancel() {
    let c = cpu();
    CALLBACK[c].store(0, Ordering::Release);
    if tsc_deadline_supported() {
        unsafe {
            core::arch::asm!("wrmsr", in("ecx") IA32_TSC_DEADLINE, in("eax") 0u32,
                in("edx") 0u32, options(nomem, nostack));
        }
    } else {
        unsafe { crate::lapic::set_initial_count(0) };
    }
}

/// How many timers have fired on this CPU.
pub fn fired() -> u64 {
    FIRED[cpu()].load(Ordering::Acquire)
}

/// From the interrupt handler for `VECTOR`, after EOI.
pub fn fire() {
    let c = cpu();
    FIRED[c].fetch_add(1, Ordering::AcqRel);
    let cb = CALLBACK[c].swap(0, Ordering::AcqRel);
    if cb != 0 {
        // Only `arm_at` stores here, and it stores an `fn()`.
        let f: fn() = unsafe { core::mem::transmute(cb) };
        f();
    }
}

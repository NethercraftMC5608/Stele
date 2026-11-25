// SPDX-License-Identifier: GPL-2.0-or-later
//! NK_DEDICATED_TEST: a dedicated core is not preempted (ROADMAP Phase 3).
//!
//! Run after Linux is up, so it also shows the core works while LKL owns CPU 0.
//! CPU 2 is contested: three spinners pinned to it, a fourth allowed on both target slots,
//! and a control probe pinned to 2. CPU 3 is dedicated to a probe.
//!
//! Timestamp gaps are no evidence under TCG -- QEMU itself time-slices vCPUs
//! -- so the test counts what nk's scheduler does: how often each probe was
//! switched in during its run window (`slices_of`), and whether the
//! wide-affinity spinner ever ran on the dedicated CPU. PASS needs the dedicated probe switched in
//! exactly once, the control probe preempted (so the counter is shown to
//! detect it), and the dedicated CPU untouched by anything else.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const RUN_NS: u64 = 1_500_000_000;

static SLICES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static LOOPS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static SPINNER_ON_DEDICATED: AtomicBool = AtomicBool::new(false);
static CPU_SEEN: [AtomicU64; 2] = [AtomicU64::new(u64::MAX), AtomicU64::new(u64::MAX)];

fn now_ns() -> u64 {
    crate::timer::cycles_to_ns(crate::arch::cycles())
}

/// Busy for RUN_NS; record how often the scheduler switched us in meanwhile.
extern "C" fn probe(which: usize) {
    let me = crate::sched::current_id();
    let cpu = crate::arch::cpu_id() as u64;
    CPU_SEEN[which].store(cpu, Ordering::Relaxed);
    let before = crate::sched::slices_of(me);
    let start = now_ns();
    let mut loops = 0u64;
    while now_ns().wrapping_sub(start) < RUN_NS {
        loops += 1;
        core::hint::spin_loop();
        if crate::arch::cpu_id() as u64 != cpu {
            // Migrated: a dedicated probe must never move.
            CPU_SEEN[which].store(u64::MAX - 1, Ordering::Relaxed);
        }
    }
    SLICES[which].store(crate::sched::slices_of(me) - before, Ordering::Relaxed);
    LOOPS[which].store(loops, Ordering::Relaxed);
}

/// Contend for CPU 2, and optionally watch the dedicated CPU.
extern "C" fn spinner(watch_cpu: usize) {
    let start = now_ns();
    while now_ns().wrapping_sub(start) < RUN_NS + 300_000_000 {
        if watch_cpu != 0 && crate::arch::cpu_id() == watch_cpu {
            SPINNER_ON_DEDICATED.store(true, Ordering::Relaxed);
        }
        core::hint::spin_loop();
    }
}

// --- high-resolution timers on the dedicated core ---------------------------

const PER_INTERVAL: usize = 100;
const INTERVALS_NS: [u64; 4] = [50_000, 200_000, 2_000_000, 5_000_000];
static FIRED_AT: AtomicU64 = AtomicU64::new(0);
static LATE_NS: [[AtomicU64; PER_INTERVAL]; 4] =
    [const { [const { AtomicU64::new(u64::MAX) }; PER_INTERVAL] }; 4];

fn on_timer() {
    FIRED_AT.store(crate::arch::cycles(), Ordering::Release);
}

/// On the dedicated core: for each interval, arm PER_INTERVAL one-shots that
/// far ahead, one at a time, recording how late each fired (u64::MAX: never).
static RATE_CHECK: AtomicU64 = AtomicU64::new(0);

extern "C" fn timer_probe(_: usize) {
    let hz = crate::timer::cycle_hz();
    // The LAPIC count rate on this core, over 10 ms of TSC: the nk-side check
    // that a programmed count means the time it should.
    if !crate::hrtimer::tsc_deadline_supported() {
        unsafe { crate::lapic::set_initial_count(u32::MAX) };
        let t0 = crate::arch::cycles();
        let c0 = unsafe { crate::lapic::current_count() };
        while crate::arch::cycles() - t0 < hz / 100 {
            core::hint::spin_loop();
        }
        let c1 = unsafe { crate::lapic::current_count() };
        unsafe { crate::lapic::set_initial_count(0) };
        RATE_CHECK.store(((c0 - c1) as u64) * 100 * 16, Ordering::Relaxed);
    }
    for (k, &interval) in INTERVALS_NS.iter().enumerate() {
        let ahead = interval * hz / 1_000_000_000;
        for i in 0..PER_INTERVAL {
            FIRED_AT.store(0, Ordering::Release);
            let deadline = crate::arch::cycles() + ahead;
            if !crate::hrtimer::arm_at(deadline, on_timer) {
                return;
            }
            let give_up = deadline + hz / 20; // 50 ms
            loop {
                let at = FIRED_AT.load(Ordering::Acquire);
                if at != 0 {
                    LATE_NS[k][i].store(crate::timer::cycles_to_ns(at.saturating_sub(deadline)), Ordering::Relaxed);
                    break;
                }
                if crate::arch::cycles() > give_up {
                    crate::hrtimer::cancel();
                    break;
                }
                core::hint::spin_loop();
            }
        }
    }
}

/// nk's side is checked directly: the LAPIC count rate on the dedicated core
/// must match the boot calibration, so a programmed count is the right time.
/// Several intervals then show how the host delivers the interrupts. Under TCG on macOS, QEMU's main loop waits in whole
/// milliseconds, so interrupts land on a grid of about a millisecond: a timer
/// fires at the first grid point after its deadline, late by less than one
/// grid step whatever its length. An error in nk's conversion would instead
/// stretch the fire time in proportion to the interval (a 5 ms timer firing
/// at 6+ ms). The grid is measured as the fire time of the shortest interval.
/// PASS: every timer fired, the grid is far below the 10 ms tick this
/// replaces, and every interval's median lateness is under two grid steps.
/// One step is the expectation, but QEMU on macOS sometimes delivers a whole
/// run of an interval one step later (the 5 ms timers at ~2.5 ms in about a
/// third of runs, with or without nk changes): that is the host, which the
/// LAPIC rate check already separates from nk.
fn timer_phase(cpu: usize) -> bool {
    let Some(id) = crate::sched::dedicate_cpu(cpu, "vcpu-timer", timer_probe, 0) else {
        crate::println!("  hrtimer: dedicate_cpu({cpu}) refused");
        return false;
    };
    crate::sched::join(id);
    let mode = if crate::hrtimer::tsc_deadline_supported() { "TSC-deadline" } else { "LAPIC count mode" };
    let mut ok = true;
    let mut medians = [0u64; 4];
    for (k, &interval) in INTERVALS_NS.iter().enumerate() {
        let mut late: alloc::vec::Vec<u64> = LATE_NS[k].iter().map(|a| a.load(Ordering::Relaxed)).collect();
        let fired = late.iter().filter(|&&n| n != u64::MAX).count();
        late.retain(|&n| n != u64::MAX);
        late.sort_unstable();
        let pick = |q: usize| late.get((late.len() * q / 100).min(late.len().saturating_sub(1))).copied().unwrap_or(u64::MAX);
        medians[k] = pick(50);
        crate::println!(
            "  hrtimer: {:>4} us x{}: {}/{} fired on cpu {cpu} ({}); late median {} us, p99 {} us, max {} us",
            interval / 1000, PER_INTERVAL, fired, PER_INTERVAL, mode,
            pick(50) / 1000, pick(99) / 1000, late.last().copied().unwrap_or(u64::MAX) / 1000
        );
        ok &= fired == PER_INTERVAL;
    }
    let rate = RATE_CHECK.load(Ordering::Relaxed);
    let bus = crate::lapic::bus_hz();
    let rate_ok = rate == 0 || rate.abs_diff(bus) * 50 < bus; // within 2%, or TSC-deadline
    crate::println!("  hrtimer: LAPIC count rate on cpu {cpu} {} Hz vs calibrated {} Hz ({})",
        rate, bus, if rate_ok { "matches" } else { "MISMATCH" });
    let grid = INTERVALS_NS[0] + medians[0];
    crate::println!("  hrtimer: host delivery grid ~{} us (fire time of the {} us timers)", grid / 1000, INTERVALS_NS[0] / 1000);
    ok && rate_ok && grid < 2_000_000 && medians.iter().all(|&m| m < 2 * grid)
}

pub fn run() {
    if crate::sched::cpu_count() < 4 {
        crate::println!("DEDICATED_SKIP: needs 4 CPUs (build with NK_SMP=1)");
        return;
    }
    let cpu = crate::sched::cpu_count() - 1;
    let mut ids = alloc::vec::Vec::new();
    for _ in 0..3 {
        let id = crate::sched::spawn("spin2", spinner, 0);
        crate::sched::set_affinity(id, 1 << 2);
        ids.push(id);
    }
    // Dedication must remove the reserved slot from this task's affinity.
    let wide = crate::sched::spawn("spin-wide", spinner, cpu);
    crate::sched::set_affinity(wide, (1 << 2) | (1 << cpu));
    ids.push(wide);
    let control = crate::sched::spawn("ctl-probe", probe, 0);
    crate::sched::set_affinity(control, 1 << 2);
    ids.push(control);
    let Some(dedicated) = crate::sched::dedicate_cpu(cpu, "vcpu-probe", probe, 1) else {
        crate::println!("DEDICATED_FAIL: dedicate_cpu({cpu}) refused");
        return;
    };
    ids.push(dedicated);
    for id in ids {
        crate::sched::join(id);
    }

    let (ctl, ded) = (SLICES[0].load(Ordering::Relaxed), SLICES[1].load(Ordering::Relaxed));
    let ded_cpu = CPU_SEEN[1].load(Ordering::Relaxed);
    let on_dedicated = SPINNER_ON_DEDICATED.load(Ordering::Relaxed);
    let reserved_after = crate::sched::reserved_cpus();
    crate::println!(
        "  dedicated: vcpu-probe on cpu {} switched in {}x during {} ms ({} loops); control on cpu 2 switched in {}x ({} loops); wide spinner ran on cpu {cpu}: {}; reserved after: {:#x}",
        ded_cpu, ded, RUN_NS / 1_000_000, LOOPS[1].load(Ordering::Relaxed),
        ctl, LOOPS[0].load(Ordering::Relaxed), on_dedicated, reserved_after
    );
    let cores = ded == 0 && ded_cpu == cpu as u64 && ctl > 2 && !on_dedicated && reserved_after & (1 << cpu) == 0;
    let timers = timer_phase(cpu);
    crate::println!("{}", if cores { "DEDICATED_PASS" } else { "DEDICATED_FAIL" });
    crate::println!("{}", if timers { "HRTIMER_PASS" } else { "HRTIMER_FAIL" });
}

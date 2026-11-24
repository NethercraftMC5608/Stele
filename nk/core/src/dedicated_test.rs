// SPDX-License-Identifier: GPL-2.0-or-later
//! NK_DEDICATED_TEST: a dedicated core is not preempted (ROADMAP Phase 3).
//!
//! Run after Linux is up, so it also shows the core works while LKL owns CPU 0.
//! CPU 2 is contested: three spinners pinned to it, a fourth allowed on 2 and 3,
//! and a control probe pinned to 2. CPU 3 is dedicated to a probe.
//!
//! Timestamp gaps are no evidence under TCG -- QEMU itself time-slices vCPUs
//! -- so the test counts what nk's scheduler does: how often each probe was
//! switched in during its run window (`slices_of`), and whether the {2,3}
//! spinner ever ran on CPU 3. PASS needs the dedicated probe switched in
//! exactly once, the control probe preempted (so the counter is shown to
//! detect it), and CPU 3 untouched by anything else.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const RUN_NS: u64 = 1_500_000_000;

static SLICES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static LOOPS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static SPINNER_ON_3: AtomicBool = AtomicBool::new(false);
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

/// Contend for CPU 2 (and, if allowed, 3) for a little longer than the probes.
extern "C" fn spinner(watch_cpu3: usize) {
    let start = now_ns();
    while now_ns().wrapping_sub(start) < RUN_NS + 300_000_000 {
        if watch_cpu3 != 0 && crate::arch::cpu_id() == 3 {
            SPINNER_ON_3.store(true, Ordering::Relaxed);
        }
        core::hint::spin_loop();
    }
}

pub fn run() {
    if crate::sched::cpu_count() < 4 {
        crate::println!("DEDICATED_SKIP: needs 4 CPUs (build with NK_SMP=1)");
        return;
    }
    let mut ids = alloc::vec::Vec::new();
    for _ in 0..3 {
        let id = crate::sched::spawn("spin2", spinner, 0);
        crate::sched::set_affinity(id, 1 << 2);
        ids.push(id);
    }
    // Allowed on 2 and 3 before CPU 3 is dedicated: the reservation must
    // strip 3 from it.
    let wide = crate::sched::spawn("spin23", spinner, 1);
    crate::sched::set_affinity(wide, (1 << 2) | (1 << 3));
    ids.push(wide);
    let control = crate::sched::spawn("ctl-probe", probe, 0);
    crate::sched::set_affinity(control, 1 << 2);
    ids.push(control);
    let Some(dedicated) = crate::sched::dedicate_cpu(3, "vcpu-probe", probe, 1) else {
        crate::println!("DEDICATED_FAIL: dedicate_cpu(3) refused");
        return;
    };
    ids.push(dedicated);
    for id in ids {
        crate::sched::join(id);
    }

    let (ctl, ded) = (SLICES[0].load(Ordering::Relaxed), SLICES[1].load(Ordering::Relaxed));
    let ded_cpu = CPU_SEEN[1].load(Ordering::Relaxed);
    let on3 = SPINNER_ON_3.load(Ordering::Relaxed);
    let reserved_after = crate::sched::reserved_cpus();
    crate::println!(
        "  dedicated: vcpu-probe on cpu {} switched in {}x during {} ms ({} loops); control on cpu 2 switched in {}x ({} loops); spin23 ran on cpu 3: {}; reserved after: {:#x}",
        ded_cpu, ded, RUN_NS / 1_000_000, LOOPS[1].load(Ordering::Relaxed),
        ctl, LOOPS[0].load(Ordering::Relaxed), on3, reserved_after
    );
    let pass = ded == 0 && ded_cpu == 3 && ctl > 2 && !on3 && reserved_after & (1 << 3) == 0;
    crate::println!("{}", if pass { "DEDICATED_PASS" } else { "DEDICATED_FAIL" });
}

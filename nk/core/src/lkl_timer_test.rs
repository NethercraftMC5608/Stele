// SPDX-License-Identifier: GPL-2.0-or-later
//! NK_LKL_TIMER_TEST: how promptly Linux's own timers fire on nk (ROADMAP
//! Phase 3).
//!
//! Linux arms its clockevent through `nk_timer_set_oneshot`. Checked only on
//! nk's 100 Hz tick, every sleep shorter than 10 ms takes a tick or two. With
//! CPU 0's LAPIC timer claimed for them (`hostops`, x86), they fire at their
//! deadline, bounded below by the host's own interrupt delivery.
//!
//! The probe calls Linux's nanosleep (asm-generic 101) from CPU 0 for several
//! lengths and records how long each sleep took past its request. PASS: every
//! interval's median lateness is under 3 ms -- a third of the tick the
//! deadline timer replaces, above QEMU-on-macOS's ~1.3 ms delivery grid.

const NANOSLEEP: i64 = 101;
const PER_INTERVAL: usize = 30;
const INTERVALS_NS: [u64; 4] = [100_000, 500_000, 1_000_000, 3_000_000];
const PASS_MEDIAN_NS: u64 = 3_000_000;

pub fn run() {
    let mut ok = true;
    for &interval in INTERVALS_NS.iter() {
        let mut late = alloc::vec::Vec::with_capacity(PER_INTERVAL);
        let mut errors = 0;
        for _ in 0..PER_INTERVAL {
            let req: [i64; 2] = [0, interval as i64];
            let t0 = crate::timer::time_ns();
            let r = crate::lkl::syscall(NANOSLEEP, [req.as_ptr() as i64, 0, 0, 0, 0, 0]);
            let took = crate::timer::time_ns() - t0;
            if r != 0 {
                errors += 1;
                continue;
            }
            late.push(took.saturating_sub(interval));
        }
        late.sort_unstable();
        let at = |q: usize| late.get((late.len() * q / 100).min(late.len().saturating_sub(1))).copied().unwrap_or(u64::MAX);
        crate::println!(
            "  lkl-timer: nanosleep {:>4} us x{}: {} errors; late median {} us, p90 {} us, max {} us",
            interval / 1000, PER_INTERVAL, errors, at(50) / 1000, at(90) / 1000,
            late.last().copied().unwrap_or(u64::MAX) / 1000
        );
        ok &= errors == 0 && at(50) < PASS_MEDIAN_NS;
    }
    crate::println!("{}", if ok { "LKL_TIMER_PASS" } else { "LKL_TIMER_FAIL" });
}

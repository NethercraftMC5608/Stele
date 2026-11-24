//! The scheduler's flight recorder.
//!
//! Every scheduling decision, recorded as it happens, so that "the audio
//! crackled" can be answered with what actually ran instead of a guess. This
//! exists *before* the policy it is meant to judge, on purpose: a scheduler
//! heuristic without evidence is an opinion, and nk has already paid for
//! three confident wrong diagnoses elsewhere in this tree.
//!
//! Three rules, all of which come from the hot path being the hot path:
//!
//! **Fixed-size records, no strings.** A `SchedEvent` is 32 bytes of numbers.
//! Formatting a name inside `schedule()` would cost more than the switch it
//! is describing, and would make the recorder change what it measures.
//!
//! **A ring, overwritten.** The recorder never allocates, never blocks and
//! never runs out. What it holds is the recent past, which is what a latency
//! investigation wants; an event from four minutes ago explains nothing.
//!
//! **Never off, and cheap enough to mean it.** A recorder that has to be
//! enabled is never on when the interesting thing happens. One bounded index
//! increment and a handful of stores is the price.
//!
//! Sized for one CPU today. The structure is per-CPU-shaped -- a caller
//! passes its CPU id and the ring is selected by it -- so that SMP adds a
//! dimension rather than a rewrite.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// What happened.
///
/// Numbered explicitly because these are written to a ring that outlives the
/// code that wrote it, and a decoder should not have to guess.
#[derive(Clone, Copy, PartialEq, Debug)]
#[repr(u8)]
pub enum Event {
    /// A task stopped running and another started.
    ContextSwitch = 1,
    /// A blocked task was made runnable.
    Wakeup = 2,
    /// A running task blocked.
    Block = 3,
    /// A task was taken off the CPU involuntarily, by the tick or by a
    /// wakeup that outranked it.
    Preempt = 4,
    /// A task's scheduling class or weight changed.
    PriorityChange = 5,
    /// The CPU found nothing to run and halted.
    IdleEnter = 6,
    /// An interrupt ended the halt.
    IdleExit = 7,
    /// A task exited.
    Exit = 8,
    /// A task waited long enough that the scheduler raised it to stop it
    /// starving. Rare by design: a stream of these means the load is beyond
    /// what the policy can share fairly, which is worth seeing.
    Aged = 9,
    Migrate = 10,
}

/// One scheduling event. 32 bytes, all of it numbers.
///
/// `a` and `b` mean different things per event, which is a compromise made
/// knowingly: a variant-shaped record would be larger and need a tag to
/// decode anyway. What they hold is documented at each `record` call site.
#[derive(Clone, Copy)]
pub struct SchedEvent {
    /// Cycle counter at the moment of the event, not a tick count. The
    /// latencies being measured are shorter than a tick.
    pub at: u64,
    pub a: u64,
    pub b: u64,
    pub task: u16,
    pub prev: u16,
    pub cpu: u8,
    pub event: u8,
    pub class: u8,
    pub _pad: u8,
}

impl SchedEvent {
    const EMPTY: Self = Self {
        at: 0,
        a: 0,
        b: 0,
        task: 0,
        prev: 0,
        cpu: 0,
        event: 0,
        class: 0,
        _pad: 0,
    };
}

/// Power of two, so the wrap is a mask rather than a division.
///
/// 1024 events is roughly a second of a busy desktop and a few seconds of an
/// idle one -- long enough to contain the run-up to a glitch, which is the
/// only window that matters.
const RING: usize = 1024;
const RING_MASK: u64 = (RING - 1) as u64;

/// One ring per CPU. One CPU today; indexed by CPU id so that more is a
/// bigger array rather than a different design.
pub const MAX_CPUS: usize = 8;

static mut RINGS: [[SchedEvent; RING]; MAX_CPUS] = [[SchedEvent::EMPTY; RING]; MAX_CPUS];
/// Monotonic write counter per CPU. The index is this modulo the ring size;
/// keeping the full count is what tells a reader how much it missed.
static NEXT: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Off until the scheduler is real, so that boot-time spawns do not fill the
/// ring with events from before there was anything to schedule.
static RECORDING: AtomicBool = AtomicBool::new(false);

pub fn start() {
    RECORDING.store(true, Ordering::Relaxed);
}

/// Record an event.
///
/// Callable from interrupt context and from inside the scheduler with the
/// run queue held. It takes no locks and cannot block: the only shared state
/// is one atomic counter, and a torn record read by a dump is a cosmetic
/// problem in a diagnostic, not a correctness one in the kernel.
#[inline]
pub fn record(cpu: usize, event: Event, task: usize, prev: usize, class: u8, a: u64, b: u64) {
    if !RECORDING.load(Ordering::Relaxed) || cpu >= MAX_CPUS {
        return;
    }
    let flags = crate::sync::irq_save();
    let seq = NEXT[cpu].load(Ordering::Relaxed);
    let slot = (seq & RING_MASK) as usize;
    unsafe {
        let ring = &mut (*(&raw mut RINGS))[cpu];
        ring[slot] = SchedEvent {
            at: crate::arch::cycles(),
            a,
            b,
            task: task as u16,
            prev: prev as u16,
            cpu: cpu as u8,
            event: event as u8,
            class,
            _pad: 0,
        };
        NEXT[cpu].store(seq.wrapping_add(1), Ordering::Release);
        crate::sync::irq_restore(flags);
    }
}

/// How many events this CPU has recorded, ever. The difference between two
/// reads is how many happened in between, including those overwritten.
pub fn count(cpu: usize) -> u64 {
    if cpu >= MAX_CPUS {
        return 0;
    }
    NEXT[cpu].load(Ordering::Relaxed)
}

fn name(event: u8) -> &'static str {
    match event {
        1 => "switch",
        2 => "wakeup",
        3 => "block",
        4 => "preempt",
        5 => "prio",
        6 => "idle-in",
        7 => "idle-out",
        8 => "exit",
        9 => "aged",
        10 => "migrate",
        _ => "?",
    }
}

/// Print the last `n` events on a CPU, oldest first.
///
/// Timestamps are shown relative to the newest event, because the absolute
/// cycle count is meaningless and the question is always "how long before
/// the thing I am looking at".
pub fn dump(cpu: usize, n: usize) {
    if cpu >= MAX_CPUS {
        return;
    }
    let mut snapshot = [SchedEvent::EMPTY; 128];
    let flags = crate::sync::irq_save();
    let total = NEXT[cpu].load(Ordering::Acquire);
    let n = n.min(snapshot.len()).min(total.min(RING as u64) as usize);
    for (k, entry) in snapshot[..n].iter_mut().enumerate() {
        let seq = total.wrapping_sub(n as u64).wrapping_add(k as u64);
        *entry = unsafe { RINGS[cpu][(seq & RING_MASK) as usize] };
    }
    unsafe { crate::sync::irq_restore(flags) };
    if total == 0 {
        crate::println!("  sched trace: nothing recorded");
        return;
    }
    let newest = snapshot[..n].last().map_or(0, |e| e.at);

    crate::println!(
        "  sched trace (last {} of {} events on cpu {}):",
        n, total, cpu
    );
    for e in &snapshot[..n] {
        let ago = crate::timer::cycles_to_ns(newest.wrapping_sub(e.at));
        match e.event {
            1 => crate::println!(
                "    -{:>9}ns switch  [{}] -> [{}] class {} ran {}ns",
                ago, e.prev, e.task, e.class, crate::timer::cycles_to_ns(e.a)
            ),
            2 => crate::println!(
                "    -{:>9}ns wakeup  [{}] by [{}] on {}",
                ago, e.task, e.prev, e.a
            ),
            3 => crate::println!(
                "    -{:>9}ns block   [{}] on {}",
                ago, e.task, e.a
            ),
            4 => crate::println!(
                "    -{:>9}ns preempt [{}] by [{}] ({})",
                ago, e.prev, e.task,
                if e.a == 1 { "wakeup" } else { "tick" }
            ),
            _ => crate::println!(
                "    -{:>9}ns {:<7} [{}] a={} b={}",
                ago, name(e.event), e.task, e.a, e.b
            ),
        }
    }
}

/// Wakeup-to-run latency, in cycles: how long a task waited between being
/// made runnable and actually getting the CPU.
///
/// This is the number the whole scheduler is judged by. Kept as a histogram
/// rather than a mean, because a mean hides exactly the thing that is felt:
/// a scheduler that wakes in 100 µs but occasionally takes 50 ms is worse to
/// use than one that always takes 500 µs, and both have the same average.
///
/// Buckets are powers of two of microseconds, which is coarse and is enough
/// to tell those two schedulers apart at a glance.
const BUCKETS: usize = 24;
static LATENCY: [AtomicU32; BUCKETS] = [const { AtomicU32::new(0) }; BUCKETS];
static LATENCY_MAX: AtomicU64 = AtomicU64::new(0);
static LATENCY_SUM: AtomicU64 = AtomicU64::new(0);
static LATENCY_N: AtomicU64 = AtomicU64::new(0);

pub fn record_latency(cycles: u64) {
    let ns = crate::timer::cycles_to_ns(cycles);
    if ns == 0 {
        return;
    }
    let us = ns / 1000;
    // ilog2 of the microsecond count, clamped into the table.
    let bucket = if us == 0 { 0 } else { (64 - (us.leading_zeros() as usize)).min(BUCKETS - 1) };
    let flags = crate::sync::irq_save();
    LATENCY[bucket].fetch_add(1, Ordering::Relaxed);
    LATENCY_SUM.fetch_add(ns, Ordering::Relaxed);
    LATENCY_N.fetch_add(1, Ordering::Relaxed);
    LATENCY_MAX.fetch_max(ns, Ordering::Relaxed);
    unsafe { crate::sync::irq_restore(flags) };
}

/// Print the wakeup latency distribution, with the percentiles that matter.
fn percentile_rank(n: u64, want: u64) -> u64 {
    ((n as u128 * want as u128).div_ceil(100)) as u64
}

pub fn percentile_regression() -> bool {
    percentile_rank(1, 50) == 1
        && percentile_rank(3, 50) == 2
        && percentile_rank(101, 99) == 100
        && percentile_rank(u64::MAX, 100) == u64::MAX
}

pub fn report_latency() {
    let flags = crate::sync::irq_save();
    let n = LATENCY_N.load(Ordering::Relaxed);
    let sum = LATENCY_SUM.load(Ordering::Relaxed);
    let max = LATENCY_MAX.load(Ordering::Relaxed);
    let buckets: [u32; BUCKETS] = core::array::from_fn(|i| LATENCY[i].load(Ordering::Relaxed));
    unsafe { crate::sync::irq_restore(flags) };
    if n == 0 {
        crate::println!("  wake latency: nothing measured");
        return;
    }

    // Percentiles from the histogram: the bucket the nth sample falls in.
    // An upper bound, not an exact figure -- which is the honest thing a
    // power-of-two histogram can say.
    let pct = |want: u64| -> u64 {
        let target = percentile_rank(n, want);
        let mut seen = 0u64;
        for (i, b) in buckets.iter().enumerate() {
            seen += *b as u64;
            if seen >= target {
                return if i == BUCKETS - 1 { max / 1000 + 1 } else { 1u64 << i };
            }
        }
        1u64 << (BUCKETS - 1)
    };

    crate::println!(
        "  wake latency: n {} mean {}ns  p50 <{}us  p95 <{}us  p99 <{}us  max {}ns",
        n, sum / n, pct(50), pct(95), pct(99), max
    );
}

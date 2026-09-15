//! What the scheduler should run next, and why.
//!
//! Separate from `sched.rs`, which owns tasks, stacks and the context
//! switch. This file owns only the decision: given a set of runnable tasks,
//! which one. Keeping it apart is what stops the policy from being tangled
//! with the machinery -- the switch is delicate and rarely changes, the
//! policy is going to change constantly and needs to be safe to change.
//!
//! The rule the whole thing serves:
//!
//! > The machine is always ready, but never restless.
//!
//! Which in scheduling terms is: latency first, predictability second,
//! throughput third -- and throughput only ever yields what it must, because
//! a compiler that is slow for no reason is its own kind of broken.
//!
//! ## Classes, not numbers
//!
//! A single priority number cannot express the difference between "run this
//! soon" and "run this a lot", and every scheduler that tries ends up with
//! nice values that mean neither. So a task has a *class*, which says what
//! kind of work it is, and a weight *within* its class.
//!
//! Classes are strictly ordered for selection. Higher class first, always,
//! with one exception -- starvation, handled by ageing below. That strictness
//! is what makes latency predictable: an interactive wake does not have to
//! out-accumulate a compiler's fairness credit, it simply goes first.
//!
//! ## Fairness inside a class
//!
//! Within a class, weighted virtual runtime. A task that has consumed less
//! weighted CPU runs first:
//!
//! ```text
//! vruntime += ran_for * (NICE_0_WEIGHT / weight)
//! ```
//!
//! So a task with twice the weight accumulates vruntime half as fast and
//! therefore gets twice the CPU. This is the one idea worth taking from CFS,
//! and it is taken without the red-black tree: with 128 task slots a linear
//! scan is a few hundred nanoseconds and is not what will be slow. §6 of the
//! brief is explicit about not optimising data structures before measuring
//! contention, and there is no contention on one CPU.
//!
//! ## Interactivity is observed, not declared
//!
//! A program does not get to say it is interactive -- it demonstrates it, by
//! blocking. A task that sleeps waiting for input and then runs briefly is
//! interactive; a task that uses its whole slice every time is not, whatever
//! it claims. The score moves on that evidence alone, which means a spinning
//! program cannot buy priority by spinning harder.

/// What kind of work a task is.
///
/// Ordered: `Realtime` outranks `Interactive` outranks `Normal`, and so on.
/// Derived `PartialOrd` follows declaration order, which is why the order
/// here is load-bearing rather than cosmetic.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Class {
    /// Lowest. Indexing, cleanup, update checks. Should be invisible the
    /// moment the user does anything.
    Background = 0,
    /// Compilation, encoding, rendering. Allowed to eat the whole machine
    /// when nothing else wants it -- and to be thrown off it instantly when
    /// something does. Both halves matter.
    Throughput = 1,
    /// Ordinary work with no strong opinion. The default.
    Normal = 2,
    /// The compositor, input dispatch, the foreground application, a
    /// terminal being typed into. Short bursts, woken by events.
    Interactive = 3,
    /// Audio and anything else with a deadline. Not yet granted to anything
    /// -- the class exists so the ordering is built in from the start rather
    /// than retrofitted around it, and the budget enforcement it requires is
    /// deliberately not pretended at yet.
    Realtime = 4,
}

impl Class {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn name(self) -> &'static str {
        match self {
            Class::Background => "background",
            Class::Throughput => "throughput",
            Class::Normal => "normal",
            Class::Interactive => "interactive",
            Class::Realtime => "realtime",
        }
    }
}

/// The weight of an ordinary task. Everything is expressed relative to this.
///
/// 1024 rather than 1 so that the division in the vruntime update keeps its
/// precision in integer arithmetic; a task weighted 1.5x is 1536.
pub const NICE_0_WEIGHT: u64 = 1024;

/// The default weight of each class.
///
/// These are the *fairness* weights used within a class, not the ordering
/// between classes -- ordering is by class, strictly. They matter when two
/// tasks of different classes end up compared during ageing, and when a
/// task's class changes and its vruntime has to stay comparable.
pub fn class_weight(class: Class) -> u64 {
    match class {
        Class::Realtime => NICE_0_WEIGHT * 8,
        Class::Interactive => NICE_0_WEIGHT * 2,
        Class::Normal => NICE_0_WEIGHT,
        Class::Throughput => NICE_0_WEIGHT / 2,
        Class::Background => NICE_0_WEIGHT / 8,
    }
}

/// How long a task of this class may run before the tick considers
/// preempting it, in nanoseconds.
///
/// Interactive work runs in short bursts and benefits from being rescheduled
/// promptly; throughput work benefits from *not* being rescheduled, because
/// every switch costs cache and TLB state that has to be rebuilt. So the
/// slices differ by an order of magnitude across the range.
///
/// These are starting values, to be moved once the flight recorder says
/// something about them. They are deliberately not tuned yet: §9 of the
/// brief says instrument before choosing, and nothing has been measured.
pub fn slice_ns(class: Class) -> u64 {
    match class {
        Class::Realtime => 1_000_000,       // 1ms
        Class::Interactive => 2_000_000,    // 2ms
        Class::Normal => 6_000_000,         // 6ms
        Class::Throughput => 20_000_000,    // 20ms
        Class::Background => 20_000_000,    // 20ms, but trivially preempted
    }
}

/// The interactivity score's range. Not a wide scale on purpose: it selects
/// between a few behaviours, and a finer one would imply a precision the
/// evidence behind it does not have.
pub const INTERACTIVE_MAX: i8 = 8;
/// At or above this, a Normal task is treated as Interactive.
pub const INTERACTIVE_THRESHOLD: i8 = 5;

/// A task blocked voluntarily: it asked to wait for something rather than
/// being thrown off the CPU. That is the signature of interactive work, and
/// the only evidence that counts toward the score.
///
/// Weighted by how long it ran first. A task that blocks after a very short
/// burst is behaving like a UI thread; one that blocks after using most of
/// its slice is doing real work and merely happened to wait at the end.
pub fn on_voluntary_block(score: i8, ran_ns: u64, slice: u64) -> i8 {
    let gain = if ran_ns * 4 < slice { 2 } else { 1 };
    (score + gain).min(INTERACTIVE_MAX)
}

/// A task used its entire slice and was preempted. It is CPU-bound right
/// now, whatever it was before.
///
/// Decays by more than a single block adds, so that sustained CPU use pulls
/// a task out of the interactive set faster than occasional blocking pushes
/// it in. The asymmetry is deliberate: over-boosting interactive tasks
/// starves everything else, and the brief is explicit about being
/// conservative here.
pub fn on_slice_exhausted(score: i8) -> i8 {
    (score - 2).max(0)
}

/// The class a task should be treated as, given what it was assigned and how
/// it has actually behaved.
///
/// Only `Normal` is promoted. A task explicitly placed in `Throughput` or
/// `Background` stays there -- that assignment is a statement of intent
/// about the work, and a compiler that happens to block on I/O often is
/// still a compiler. Promotion exists for the common case of a program
/// nobody has classified, where the behaviour is the only information
/// available.
pub fn effective_class(assigned: Class, score: i8) -> Class {
    match assigned {
        Class::Normal if score >= INTERACTIVE_THRESHOLD => Class::Interactive,
        other => other,
    }
}

/// How much vruntime to add for having run `ran_ns` at this weight.
///
/// The core of the fairness rule. A heavier task's clock runs slower, so it
/// is picked more often.
#[inline]
pub fn vruntime_delta(ran_ns: u64, weight: u64) -> u64 {
    if weight == 0 {
        return ran_ns;
    }
    ((ran_ns as u128 * NICE_0_WEIGHT as u128) / weight as u128) as u64
}

/// Whether a task waking up should throw the running task off the CPU now,
/// rather than at the end of its slice.
///
/// This is the wakeup path, and it is most of what "feels instant" means: a
/// compositor woken by a keystroke should not wait out a compiler's 20 ms
/// slice. The check is deliberately narrow.
///
/// Preemption is *not* free -- it costs a switch, cache locality and branch
/// predictor state -- so a small advantage does not justify it. Only a task
/// of a strictly higher class preempts, and only if the running task has had
/// a minimum amount of time to make progress. That floor is what stops a
/// stream of wakeups from reducing a throughput task to nothing but context
/// switches (§8's "preemption granularity").
pub fn should_preempt(waker: Class, running: Class, running_for_ns: u64) -> bool {
    if waker <= running {
        return false;
    }
    // Realtime does not wait. Its whole purpose is a bounded wakeup, and a
    // granularity floor is exactly the unbounded delay it must not have.
    if waker == Class::Realtime {
        return true;
    }
    running_for_ns >= PREEMPT_GRANULARITY_NS
}

/// The minimum a task runs before a higher-class wakeup may take the CPU.
///
/// 500 µs: long enough that a burst of wakeups cannot turn the machine into
/// a switching benchmark, short enough to be well inside a 16 ms frame and
/// therefore invisible to the person using it.
pub const PREEMPT_GRANULARITY_NS: u64 = 500_000;

/// How long a runnable task may wait before the scheduler raises it to stop
/// it starving, in nanoseconds.
///
/// The brief's §30: no user should ever find that their export never
/// finishes because they kept moving the mouse. Strict class ordering makes
/// that outcome possible, so something must break it, and this is that
/// something -- a background task that has been ready for a full second is
/// considered against the class above it.
///
/// One second is long in scheduler terms and is meant to be. This is a
/// safety net against pathological load, not a tuning parameter; if it
/// triggers routinely the policy above it is wrong and the `Aged` events in
/// the flight recorder will say so.
pub const STARVATION_NS: u64 = 1_000_000_000;

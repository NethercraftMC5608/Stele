//! Threads, and a round robin over them.
//!
//! Kernel threads and single-threaded EL0 processes share the scheduler.
//! Each task retains its translation root; Linux task ownership lives in
//! the host TLS associated with that same nk thread.
//!
//! Preemptive, from the timer interrupt. Cooperative scheduling would be less
//! code and would be a trap: a driver that spins waiting for a device would
//! hang the machine rather than merely be slow, and the shim cannot fix that
//! from outside.

use crate::frames::{self, PAGE};
use crate::println;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Four pages. Generous for what runs here, and cheap; a Linux driver's probe
/// path is not shy with stack, and a kernel stack overflow with no guard page
/// silently corrupts whatever is below it.
const STACK_PAGES: usize = 32;
/// Sixty-four, because Linux wants them.
///
/// Sixteen was plenty for nk's own threads and is nowhere near enough for a
/// booting Linux: init, kthreadd, the RCU threads, the per-subsystem
/// workqueues, kdevtmpfs, kblockd, writeback and the rest are two dozen
/// before anything useful runs. Sixty-four is also the width of the waiter
/// bitmask in `sync.rs`, and the two must stay equal -- a task whose slot is
/// past the end of that mask can be blocked and never woken.
pub const MAX_TASKS: usize = 128;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Unused,
    Ready,
    Running,
    /// Waiting for something. The scheduler will not pick it until somebody
    /// calls `wake`.
    ///
    /// Everything in nk until now waited by yielding in a loop, which works
    /// and is wrong in a way that matters as soon as there is more than a
    /// demo running: a spinning task is indistinguishable from a busy one, so
    /// the machine can never be idle and a lock held across a long operation
    /// burns every remaining slice. A real block is what a semaphore, a mutex
    /// and a wait queue are all built from, and Linux's core assumes all
    /// three.
    Blocked,
    Finished,
}

/// No CPU owns (or has reserved) a task's execution context.
pub const NO_OWNER: usize = usize::MAX;
/// No task was released by the switch recorded for a CPU.
pub const NO_TASK: usize = usize::MAX;

pub struct Task {
    /// Saved stack pointer, published by the outgoing CPU and consumed only
    /// after observing `owner == NO_OWNER` with Acquire.
    pub sp: core::sync::atomic::AtomicUsize,
    pub stack: usize,
    /// Length of the range `stack` names. Not always `STACK_PAGES * PAGE`:
    /// the per-CPU idle tasks run on the 16 KiB secondary stacks.
    pub stack_size: usize,
    /// What SP_EL0 points at while this task runs.
    ///
    /// On arm64 Linux, SP_EL0 in kernel mode holds the current task_struct --
    /// `current` is literally a read of it -- and kbuild compiles every driver
    /// with `-mstack-protector-guard=sysreg -mstack-protector-guard-reg=sp_el0
    /// -mstack-protector-guard-offset=1344`, so the stack canary is read from
    /// SP_EL0 + 1344 on entry to almost every function. With SP_EL0 left at
    /// zero, the first Linux function called dereferences address 1344.
    ///
    /// So each task gets a page. nk does not have task_structs and the page is
    /// zeroed, which makes the canary a consistent zero -- weaker than Linux's
    /// per-task random value, and still catches the linear overflow the canary
    /// exists for. A driver that follows `current` into it reads zeroes, which
    /// is a real limitation and will need a proper shadow task_struct the
    /// first time one does.
    pub shadow: usize,
    pub state: State,
    /// Whether this task is on a CPU right now: running, or picked and not
    /// yet fully switched out.
    ///
    /// `schedule` marks the outgoing task `Ready` while its registers are
    /// still live on the CPU doing the switch, so between that mark and the
    /// completion of `cpu_switch` a `Ready` task is not yet pickable. This
    /// flag closes that window: set when a task is picked, cleared by the
    /// incoming task in the assembly switch, once the switch has landed. Without
    /// it two CPUs pick one `Ready` task and share its stack.
    /// Execution-context ownership: `NO_OWNER` when the context is
    /// physically unowned and claimable, otherwise the CPU that owns or has
    /// reserved it. This is not `home_cpu` and not a wake target.
    pub owner_cpu: core::sync::atomic::AtomicUsize,
    pub name: &'static str,
    pub home_cpu: u8,
    pub affinity: u16,
    pub migrations: u32,
    /// What kind of work this is. See `schedpolicy::Class`.
    ///
    /// What the task was *assigned*; what it is treated as also depends on
    /// how it has behaved, which is `effective_class`.
    pub class: crate::schedpolicy::Class,
    /// Weighted CPU time consumed, in nanoseconds scaled by weight. The
    /// fairness key: within a class, least-vruntime runs first.
    ///
    /// Monotonic per task, and never reset -- a task that blocks for an hour
    /// must not come back with a vruntime so far behind everything else that
    /// it monopolises the CPU to catch up. New and waking tasks are placed
    /// relative to the run queue's minimum instead; see `place_vruntime`.
    pub vruntime: u64,
    /// Evidence of interactivity, 0..=INTERACTIVE_MAX. Raised by blocking
    /// voluntarily, lowered by using a whole slice. Never set by the task.
    pub interactive: i8,
    /// Cycle counter reading when this task was last given the CPU, and the
    /// total it has run for. `ran_for` is what the fairness and interactivity
    /// updates are computed from.
    pub ran_at: u64,
    pub accounted_at: u64,
    pub slice_scored: bool,
    pub wake_pending: bool,
    pub ran_ns: u64,
    /// When this task became runnable, for the wakeup latency histogram and
    /// for ageing. Zero when it is not waiting.
    pub ready_at: u64,
    /// The thread's own name from `prctl(PR_SET_NAME)`, for the wait graph.
    /// `name` above is nk's kind ("thread", "forked"); this is what the guest
    /// called itself ("gmain", "WebKit2IPC", "pool-spawner").
    pub comm: [u8; 16],
    pub slices: u64,
    /// What this task is blocked on, when it is blocked: the id of a
    /// semaphore or mutex. Zero when it blocked for some other reason.
    pub waiting_on: u32,
    /// The task blocked in `join` on this one, plus one; 0 for nobody.
    /// Stored as an offset so that a zeroed table means "no joiner" rather
    /// than "task 0 is waiting", which would make `task_exit` wake the boot
    /// task out of whatever it was really doing.
    pub joiner: u32,
    /// The tick this task's `sleep_ticks` ends at; 0 when it is not
    /// sleeping. Read by the timer interrupt, which is what wakes it.
    pub wake_at: u64,
    /// The most recent system call this task forwarded to Linux. Set before
    /// every entry; a task that entered Linux and never came back leaves its
    /// last syscall number here, which turns "task 44 holds the cpu lock" into
    /// "task 44 is stuck in syscall N". Zero until the first forward.
    pub last_syscall: u64,
    pub ttbr0: u64,
    pub linux_pid: i64,
    pub exit_status: i32,
    pub user_irqs: u64,
    /// The process's heap break, and the lowest value `brk` may return to.
    /// Zero for a kernel thread, which has neither.
    pub brk: u64,
    pub brk_min: u64,
    /// Where the next anonymous mapping goes. Grows downward, away from the
    /// heap, so the two run out of room by meeting rather than by silently
    /// overwriting one another.
    pub mmap_next: u64,
    /// The task that forked this one, or 0. `wait4` needs it: a process may
    /// only wait for its own children, and "its own" is a fact nothing else
    /// records -- Linux knows about its tasks but nk owns the address spaces
    /// and the exit statuses, so the relation has to live where those do.
    pub parent: usize,
    /// Whether this task shares its address space with the one that made it.
    ///
    /// A thread does. It must not have the address space torn down when it
    /// exits, because the threads it shares with are still using it -- and
    /// the tables are the same tables, not a copy.
    /// How many times this slot has been used.
    ///
    /// A task id is a slot index and slots are reused, so an id alone does
    /// not identify a thread over time: a task exits, its slot is taken by
    /// the next one, and the two are indistinguishable. That is fine inside
    /// nk, which only ever asks about live tasks -- and wrong for Linux,
    /// which holds `cpu.owner` across a task's whole life and compares it
    /// with `thread_self()` to decide whether it already owns the CPU lock.
    /// An alias there is not a cosmetic mix-up: matching a dead owner takes
    /// the lock without acquiring it, and failing to match one waits on a
    /// thread that no longer exists. See `uid`.
    pub generation: u64,
    pub shares_mm: bool,
    /// A leaked `Semaphore` the vfork parent is blocked on, or 0.
    ///
    /// vfork's promise is that the parent does not run again until the child
    /// has stopped using the address space they share -- by `execve` or by
    /// exiting. This is how the child says so.
    pub vfork_done: usize,
    /// Where to write a zero and wake a futex when this task exits, if
    /// `CLONE_CHILD_CLEARTID` asked for it. That write is what `pthread_join`
    /// is waiting for.
    pub clear_child_tid: u64,
    /// Whether this process has descriptors 0, 1 and 2 open on a real
    /// console. When it has not, nk answers writes to 1 and 2 itself, which
    /// prints and cannot redirect.
    pub has_console: bool,
    /// Which system-call interface this task's program speaks.
    ///
    /// Per task, not per machine: a native NETHOS program and a Linux one
    /// run side by side, and the same `svc`/`syscall` instruction is read
    /// against a different table depending on which this is. Set from the
    /// ELF's `EI_OSABI` when the program is loaded, and inherited across
    /// fork -- a child of a native process is running the same image.
    pub nethos_abi: bool,
    /// Whether this task is inside a system call made *from EL0*.
    ///
    /// Linux asks nk to copy user memory for it, and the same code path
    /// serves two callers that must be treated differently: a program at EL0,
    /// whose pointers have to be translated with its own permissions and
    /// refused if they name kernel memory, and nk itself, which calls into
    /// Linux with kernel buffers to seed the rootfs and load an ELF. The flag
    /// says which, and it is per task because both can be happening at once.
    pub user_syscall: bool,
    /// The process's thread pointer, `TPIDR_EL0`.
    ///
    /// nk never reads it, which is exactly why it has to be saved here: it
    /// belongs entirely to EL0, so nothing in the kernel would notice it
    /// being wrong. A libc puts its whole thread-local area behind it --
    /// `errno`, the malloc tcache, the locale -- so leaving one process's
    /// value in place while another runs hands the second process the first
    /// one's heap bookkeeping, and the crash lands some distance away in
    /// malloc rather than anywhere near the switch.
    pub tpidr: u64,
    /// Shared mappings this task holds: where each is, and which pool region
    /// it points at. A thread shares its creator's list -- it is the same
    /// address space -- while a forked child gets a copy of the entries
    /// (its pages are private copies; see `user.rs`). Bounded: a process
    /// with more shared mappings than this is told no at `mmap` time.
    pub shared: [crate::user::SharedMap; 256],
    pub nshared: usize,
    /// Signal state. `actions` is shared across threads the way the address
    /// space is -- dispositions belong to the process -- while `mask` and
    /// `pending` are per task. A forked child inherits a copy of all three.
    pub sig_actions: [crate::signal::Action; 64],
    pub sig_mask: u64,
    pub sig_pending: u64,
    /// `si_code` and sender pid for the pending signals, so `siginfo` in the
    /// delivered frame says something true. `SI_USER` for a `kill`,
    /// `SI_KERNEL` for a `SIGCHLD` from `sys_exit`.
    pub sig_code: [i32; 64],
    pub sig_sender: [i32; 64],
    /// Alternate signal stack from `sigaltstack`, and whether a handler is
    /// running on it now (nested `SA_ONSTACK` handlers reuse it -- Linux
    /// refuses the second, nk notes it and carries on).
    pub sig_alt_base: u64,
    pub sig_alt_size: u64,
    pub sig_alt_in_use: bool,
    /// What `rt_sigreturn` restores: the mask saved at delivery, and whether
    /// the handler ran on the alternate stack.
    pub sig_return_mask: u64,
    pub sig_return_alt: bool,
}

impl Task {
    /// The saved SP, read after the Acquire that observed `owner == NO_OWNER`.
    #[inline]
    pub fn saved_sp(&self) -> usize {
        self.sp.load(Ordering::Acquire)
    }

    /// The CPU that owns this execution context, or `NO_OWNER`. The load
    /// that observes `NO_OWNER` is the one that makes the saved context
    /// visible (it pairs with the assembly's release store).
    #[inline]
    pub fn owner(&self) -> usize {
        self.owner_cpu.load(Ordering::Acquire)
    }

    pub fn is_owned(&self) -> bool {
        self.owner() != NO_OWNER
    }

    pub fn is_owned_by(&self, cpu: usize) -> bool {
        self.owner() == cpu
    }

    pub fn set_owner(&self, cpu: usize) {
        self.owner_cpu.store(cpu, Ordering::Relaxed);
    }

    pub fn release_owner(&self) {
        self.owner_cpu.store(NO_OWNER, Ordering::Relaxed);
    }

}

static mut TASKS: [Task; MAX_TASKS] = [const {
    Task {
        sp: core::sync::atomic::AtomicUsize::new(0),
    stack: 0,
    stack_size: 0,
    shadow: 0,
        state: State::Unused,
        owner_cpu: core::sync::atomic::AtomicUsize::new(NO_OWNER),
        name: "",
    home_cpu: 0,
    affinity: ALL_CPU_SLOTS,
    migrations: 0,
    class: crate::schedpolicy::Class::Normal,
    vruntime: 0,
    interactive: 0,
    ran_at: 0,
    accounted_at: 0,
    slice_scored: false,
    wake_pending: false,
    ran_ns: 0,
    ready_at: 0,
    comm: [0; 16],
    slices: 0,
    waiting_on: 0,
    joiner: 0,
    wake_at: 0,
    last_syscall: 0,
    ttbr0: 0,
    linux_pid: 0,
    exit_status: 0,
    user_irqs: 0,
    brk: 0,
    brk_min: 0,
    mmap_next: 0,
    parent: 0,
    generation: 0,
    shares_mm: false,
    vfork_done: 0,
    clear_child_tid: 0,
    has_console: false,
    nethos_abi: false,
    user_syscall: false,
    tpidr: 0,
    shared: [crate::user::SharedMap {
        start: 0,
        len: 0,
        region: 0,
        fd: -1,
        anonymous: false,
    }; 256],
    nshared: 0,
    sig_actions: [crate::signal::Action::default(); 64],
    sig_mask: 0,
    sig_pending: 0,
    sig_code: [0; 64],
    sig_sender: [0; 64],
    sig_alt_base: 0,
    sig_alt_size: 0,
    sig_alt_in_use: false,
    sig_return_mask: 0,
    sig_return_alt: false,
} }; MAX_TASKS];

static mut CURRENT: usize = 0;
static ENABLED: AtomicBool = AtomicBool::new(false);

#[cfg(target_arch = "x86_64")]
pub const MAX_CPUS: usize = 12;
#[cfg(not(target_arch = "x86_64"))]
pub const MAX_CPUS: usize = 8;
const ALL_CPU_SLOTS: u16 = (1 << MAX_CPUS) - 1;

struct CpuState {
    current: core::sync::atomic::AtomicUsize,
    need_resched: core::sync::atomic::AtomicBool,
    online: core::sync::atomic::AtomicBool,
    /// The outgoing task of the physical context switch that last occurred
    /// on this CPU. Written by the outgoing context, consumed by whatever
    /// context the switch resurfaced. Single writer (this CPU).
    released_task: core::sync::atomic::AtomicUsize,
    idle_task: usize,
    idle: core::sync::atomic::AtomicBool,
    ipi_pending: core::sync::atomic::AtomicBool,
    switches: core::sync::atomic::AtomicU64,
    stolen: core::sync::atomic::AtomicU64,
}

const CPU_INIT: CpuState = CpuState {
    current: core::sync::atomic::AtomicUsize::new(usize::MAX),
    need_resched: core::sync::atomic::AtomicBool::new(false),
    online: core::sync::atomic::AtomicBool::new(false),
    released_task: core::sync::atomic::AtomicUsize::new(NO_TASK),
    idle_task: usize::MAX,
    idle: core::sync::atomic::AtomicBool::new(false),
    ipi_pending: core::sync::atomic::AtomicBool::new(false),
    switches: core::sync::atomic::AtomicU64::new(0),
    stolen: core::sync::atomic::AtomicU64::new(0),
};

static mut CPUS: [CpuState; MAX_CPUS] = [CPU_INIT; MAX_CPUS];
static NCPUS: AtomicUsize = AtomicUsize::new(1);

#[inline]
fn cpu_index(cpu: usize) -> usize {
    if cpu < MAX_CPUS {
        cpu
    } else {
        0
    }
}

pub fn cpu_id() -> usize {
    crate::arch::cpu_id()
}

pub fn cpu_count() -> usize {
    NCPUS.load(Ordering::Relaxed).max(1).min(MAX_CPUS)
}

pub fn current_cpu_state(cpu: usize) -> (usize, bool, bool) {
    let cpu = cpu_index(cpu);
    unsafe {
        let st = &(*(&raw const CPUS))[cpu];
        (st.current.load(Ordering::Acquire), st.need_resched.load(Ordering::Acquire), st.online.load(Ordering::Acquire))
    }
}

fn cpu_current_unchecked() -> usize {
    let cpu = cpu_index(crate::arch::cpu_id());
    unsafe { (*(&raw const CPUS))[cpu].current.load(Ordering::Acquire) }
}

/// The calling CPU's current task.
///
/// Every direct `TASKS[cur()]` read outside the switch path goes through
/// here. `CURRENT` itself is kept as a cpu-0 mirror only -- anything still
/// reading it directly is correct on the boot CPU and wrong everywhere
/// else, which is exactly the class of bug SMP exists to expose.
#[inline]
fn cur() -> usize {
    cpu_current_unchecked()
}

fn set_cpu_current(cpu: usize, task: usize) {
    let cpu = cpu_index(cpu);
    unsafe {
        (*(&raw mut CPUS))[cpu].current.store(task, Ordering::Release);
    }
    if cpu == 0 {
        unsafe {
            CURRENT = task;
        }
    }
}

fn cpu_online(cpu: usize) -> bool {
    let cpu = cpu_index(cpu);
    unsafe { (*(&raw const CPUS))[cpu].online.load(Ordering::Acquire) }
}

fn mark_cpu_online(cpu: usize, current: usize) {
    let cpu = cpu_index(cpu);
    unsafe {
        let st = &mut (*(&raw mut CPUS))[cpu];
        st.online.store(true, Ordering::Release);
        st.current.store(current, Ordering::Release);
    }
    let live = NCPUS.load(Ordering::Relaxed);
    let want = (cpu + 1).min(MAX_CPUS);
    if want > live {
        NCPUS.store(want, Ordering::Relaxed);
    }
    if cpu == 0 {
        unsafe {
            CURRENT = current;
        }
    }
}

#[inline]
fn need_resched_on(cpu: usize) -> bool {
    let cpu = cpu_index(cpu);
    unsafe { (*(&raw const CPUS))[cpu].need_resched.load(Ordering::Acquire) }
}

#[inline]
fn set_need_resched_on(cpu: usize) {
    let cpu = cpu_index(cpu);
    unsafe {
        (*(&raw mut CPUS))[cpu].need_resched.store(true, Ordering::Release);
    }
    if cpu == 0 {
        NEED_RESCHED.store(true, Ordering::Relaxed);
    }
}

#[inline]
#[inline]
fn take_need_resched_on(cpu: usize) -> bool {
    let cpu = cpu_index(cpu);
    let was = unsafe {
        let st = &mut (*(&raw mut CPUS))[cpu];
        st.need_resched.swap(false, Ordering::AcqRel)
    };
    if cpu == 0 && was {
        NEED_RESCHED.store(false, Ordering::Relaxed);
    }
    was
}

pub fn cpu_stats(cpu: usize) -> (u64, u64) {
    let cpu = cpu_index(cpu);
    unsafe {
        let st = &(*(&raw const CPUS))[cpu];
        (st.switches.load(Ordering::Relaxed), st.stolen.load(Ordering::Relaxed))
    }
}

pub fn cpu_idle(cpu: usize) -> bool {
    let cpu = cpu_index(cpu);
    unsafe { (*(&raw const CPUS))[cpu].idle.load(Ordering::Acquire) }
}

fn set_cpu_idle(cpu: usize, idle: bool) {
    let cpu = cpu_index(cpu);
    unsafe {
        (*(&raw mut CPUS))[cpu].idle.store(idle, Ordering::Release);
    }
}

pub fn take_cpu_ipi(cpu: usize) -> bool {
    let cpu = cpu_index(cpu);
    unsafe {
        let st = &mut (*(&raw mut CPUS))[cpu];
        st.ipi_pending.swap(false, Ordering::AcqRel)
    }
}

/// Ask another CPU to reschedule: publish the request, then kick it. Remote
/// work never performs another CPU's context switch locally.
pub fn request_reschedule(cpu: usize) {
    if cpu_index(cpu) == cpu_index(crate::arch::cpu_id()) {
        set_need_resched_on(cpu);
        return;
    }
    send_reschedule_ipi(cpu);
}

pub fn send_reschedule_ipi(cpu: usize) {
    let cpu = cpu_index(cpu);
    let me = cpu_index(crate::arch::cpu_id());
    if cpu == me {
        set_need_resched_on(cpu);
        return;
    }
    unsafe {
        let st = &mut (*(&raw mut CPUS))[cpu];
        if !st.online.load(Ordering::Acquire) {
            return;
        }
        st.ipi_pending.store(true, Ordering::Release);
        st.need_resched.store(true, Ordering::Release);
        st.idle.store(false, Ordering::Relaxed);
    }
    crate::arch::send_ipi(cpu);
}

fn pick_target_cpu(id: usize) -> usize {
    let forced = TEST_WAKE_TARGET.load(Ordering::Relaxed);
    if forced != usize::MAX {
        return cpu_index(forced);
    }
    let mask = unsafe { (*(&raw const TASKS))[id].affinity };
    let me = cpu_index(crate::arch::cpu_id());
    // Wakes are placement decisions too, and the same LKL line applies: a
    // task woken onto an AP while Linux is running would execute its next
    // syscall there. Everything comes back to the boot CPU. See `LKL_ACTIVE`.
    // ...unless the task cannot run there at all: nk-native work pinned
    // away from CPU 0 (a dedicated vCPU) is placed by its own mask.
    if lkl_active() && mask & 1 != 0 {
        return 0;
    }
    if mask & (1 << me) != 0 && cpu_online(me) {
        return me;
    }
    let n = cpu_count();
    for cpu in 0..n {
        if mask & (1 << cpu) != 0 && cpu_online(cpu) {
            return cpu;
        }
    }
    me
}

pub fn set_affinity(id: usize, mask: u16) {
    let mask = mask & ALL_CPU_SLOTS;
    if id >= MAX_TASKS || mask == 0 {
        return;
    }
    // Under the runqueue lock, like every other mutation of task state.
    // `balance_on_idle_locked` reads `affinity` and writes `home_cpu` under
    // this lock; an unguarded `set_affinity` can be overwritten by a steal
    // that had already decided to take the task.
    let flags = RUNQUEUE.lock();
    unsafe {
        TASKS[id].affinity = mask;
        if mask & (1u16 << usize::from(TASKS[id].home_cpu).min(MAX_CPUS - 1)) == 0 {
            let mut target = usize::MAX;
            for cpu in 0..cpu_count() {
                if mask & (1u16 << cpu.min(MAX_CPUS - 1)) != 0 && cpu_online(cpu) {
                    target = cpu;
                    break;
                }
            }
            if target != usize::MAX {
                TASKS[id].home_cpu = target as u8;
                TASKS[id].migrations += 1;
            }
        }
        RUNQUEUE.unlock(flags);
    }
}

pub fn task_home_cpu(id: usize) -> usize {
    if id >= MAX_TASKS {
        return 0;
    }
    let g = RUNQUEUE.lock();
    let h = unsafe { TASKS[id].home_cpu as usize };
    unsafe { RUNQUEUE.unlock(g) };
    h
}

pub fn task_migrations(id: usize) -> u32 {
    if id >= MAX_TASKS {
        return 0;
    }
    let g = RUNQUEUE.lock();
    let m = unsafe { TASKS[id].migrations };
    unsafe { RUNQUEUE.unlock(g) };
    m
}

pub fn wake_on_cpu(id: usize, what: u32, cpu: usize) -> bool {
    let cpu = cpu_index(cpu);
    if !cpu_online(cpu) {
        return false;
    }
    let me = cpu_index(crate::arch::cpu_id());
    let woke = wake_on(id, what);
    if woke && cpu != me {
        request_reschedule(cpu);
    }
    woke
}

extern "C" {
    /// The five arguments, in order:
    ///   x0 = &outgoing TASKS[cur].owner_cpu (release-store NO_OWNER)
    ///   x1 = &outgoing TASKS[cur].sp      (the saved context)
    ///   x2 = incoming saved SP            (TASKS[next].sp)
    ///   x3 = &CPUS[cpu].current           (published as `next`)
    ///   x4 = incoming task id
    fn cpu_switch(
        prev_owner: *mut usize,
        prev_sp: *mut usize,
        next_sp: usize,
        next_current: *mut usize,
        next_id: usize,
    );
    fn task_start();
}

/// Slot 0 is whatever was already running when the scheduler started -- the
/// boot path itself. It needs no stack allocated because it is already on one,
/// and it needs an entry in the table because it is what the first switch
/// switches *away from*.
pub fn init() {
    unsafe {
        let t = &mut (*(&raw mut TASKS))[0];
        t.state = State::Running;
        t.set_owner(0);
        t.name = "boot";
        t.shadow = frames::alloc().expect("no memory for the boot task shadow") as usize;
        let shadow = t.shadow;
        mark_cpu_online(0, 0);
        set_shadow(shadow);
    }
}

pub fn init_secondary(cpu: usize) {
    let cpu = cpu_index(cpu);
    unsafe {
        let st = &mut (*(&raw mut CPUS))[cpu];
        st.online.store(false, Ordering::Relaxed);
        st.current.store(usize::MAX, Ordering::Relaxed);
        st.need_resched.store(false, Ordering::Relaxed);
        st.released_task.store(NO_TASK, Ordering::Relaxed);
        st.idle_task = usize::MAX;
    }
    let live = NCPUS.load(Ordering::Relaxed);
    let want = (cpu + 1).min(MAX_CPUS);
    if want > live {
        NCPUS.store(want, Ordering::Relaxed);
    }
}

/// The idle task registered for each CPU: the only thing that makes "this
/// CPU's idle task" well defined for picker exclusion and fallback.
static IDLE_SLOT: [core::sync::atomic::AtomicU64; MAX_CPUS] =
    [const { core::sync::atomic::AtomicU64::new(u64::MAX) }; MAX_CPUS];

fn idle_task_of(cpu: usize) -> u64 {
    IDLE_SLOT[cpu_index(cpu)].load(Ordering::SeqCst)
}

/// If `slot` is a CPU's registered idle task, which CPU owns it.
///
/// The picker excludes every registered idle task, not just the picking
/// CPU's own: an idle task is a per-CPU fallback and must never be selected
/// as ordinary runnable work. Affinity is a defensive invariant here, not
/// the mechanism that keeps them apart.
pub fn idle_owner_of_slot(slot: usize) -> Option<usize> {
    if slot >= MAX_TASKS {
        return None;
    }
    for c in 0..MAX_CPUS {
        if IDLE_SLOT[c].load(Ordering::SeqCst) == slot as u64 {
            return Some(c);
        }
    }
    None
}

pub fn register_idle_task(cpu: usize, slot: usize) {
    IDLE_SLOT[cpu_index(cpu)].store(slot as u64, Ordering::SeqCst);
}

/// The AP idle tasks run on the secondary stacks, not on the pool stack
/// `spawn` allocated for them; the table's stack metadata must name the
/// range really in use.
pub fn set_task_stack(id: usize, base: usize, len: usize) {
    if id < MAX_TASKS {
        unsafe {
            (*(&raw mut TASKS))[id].stack = base;
            (*(&raw mut TASKS))[id].stack_size = len;
        }
    }
}

pub fn online_cpu(cpu: usize, current: usize) {
    if current < MAX_TASKS {
        // Initialization before publication: this CPU is not yet online and
        // the idle task is affinity-pinned to it, so no other CPU can pick
        // it. The state becomes Running+owned before this CPU schedules.
        unsafe {
            // The task was spawned Ready; it becomes Running only by being
            // picked, but this CPU starts *inside* it -- there is no switch
            // to perform. Mark it so pick and account treat it as the
            // incumbent rather than a rival candidate.
            if TASKS[current].state == State::Ready {
                TASKS[current].state = State::Running;
                // This CPU runs the task from birth, with no switch to set
                // the flag, so it is set here or the task would look
                // pickable while this CPU already owns it.
                TASKS[current].set_owner(cpu_index(cpu));
                TASKS[current].ran_at = crate::arch::cycles();
                TASKS[current].accounted_at = TASKS[current].ran_at;
                TASKS[current].ready_at = 0;
            }
        }
    }
    mark_cpu_online(cpu, current);
}

/// Put SP_EL0 back to the current task's shadow after a return from EL0,
/// where it held the user stack pointer instead.
pub fn restore_task_ptr() {
    let cur = cpu_current_unchecked();
    unsafe { set_shadow((*(&raw const TASKS))[cur].shadow) };
}

/// Point SP_EL0 at this task's shadow page. Must happen before any Linux code
/// runs on the task, and before every switch to it.
#[inline]
fn set_shadow(addr: usize) {
    #[cfg(target_arch = "aarch64")]
    unsafe { core::arch::asm!("msr sp_el0, {}", in(reg) addr, options(nomem, nostack)) };
    #[cfg(target_arch = "x86_64")]
    let _ = addr;
}

/// One task's FPSIMD state: q0-q31, FPCR and FPSR, which is everything an
/// EL0 thread can carry in the vector unit.
///
/// nk enables FP/SIMD for EL0 and EL1 (`CPACR_EL1.FPEN = 3`), and the guest
/// is full of it -- Skia's raster pipeline, JSC, glibc's memcpy are all NEON.
/// Nothing saved the vector registers across a context switch, so a task
/// preempted mid-SIMD resumed with whatever the last task to touch the unit
/// had left in it. The first symptom looked like Skia's gradient stage
/// searching a stop index of 775 million and reading a kernel address: the
/// index was not computed from the gradient at all, it was another task's
/// vector lane.
///
/// 16-byte aligned because `stp qN, qN+1` requires it. Indexed by task slot,
/// so it is host-static and reset when a slot is reused.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct FpSave {
    pub q: [u64; 64],
    pub fpcr: u64,
    pub fpsr: u64,
}

static mut FP_SAVE: [FpSave; MAX_TASKS] = [FpSave { q: [0; 64], fpcr: 0, fpsr: 0 }; MAX_TASKS];

/// A fresh task starts with a zeroed vector unit, like a process on Linux.
pub fn fp_reset(id: usize) {
    unsafe {
        let p = (&raw mut FP_SAVE[id]) as *mut u8;
        core::ptr::write_bytes(p, 0, core::mem::size_of::<FpSave>());
    }
}

/// `fork` and `clone` copy the vector unit; the child runs on the parent's
/// state until it changes it.
pub fn fp_snapshot(id: usize) -> FpSave {
    unsafe { FP_SAVE[id] }
}

pub fn fp_install(id: usize, state: FpSave) {
    unsafe { FP_SAVE[id] = state };
}

/// The destination for the assembly save/restore, for the current task.
#[no_mangle]
pub extern "C" fn nk_fp_addr() -> u64 {
    unsafe { (&raw mut FP_SAVE[current_id()]) as u64 }
}

/// Reclaim tasks that have finished and that nobody is going to wait for.
///
/// `task_exit` cannot free the stack it is standing on, so it leaves the slot
/// Finished for somebody else to reclaim -- and until now the only somebody
/// was `wait4`. A thread is never waited for (`pthread_join` waits on a
/// futex, not on the task), and an orphan's parent is gone, so both piled up
/// with a stack and a shadow page each. WebKit makes enough of both to
/// exhaust the machine:
///
///   !! kernel panic  out of memory for a task stack
///
/// Only threads and orphans, and never the running task. A finished child
/// with a live parent is that parent's to reap: its exit status is still the
/// answer to a `wait4` that has not happened yet.
fn reclaim_finished() {
    // Under the runqueue lock: this frees stacks and shadows and blanks
    // slots, and every other CPU picks tasks under this lock.
    let g = RUNQUEUE.lock();
    unsafe {
        for id in 0..MAX_TASKS {
            if id == cur() {
                continue;
            }
            let t = &TASKS[id];
            if t.state != State::Finished {
                continue;
            }
            // A finished task is not necessarily *off its stack*. `task_exit`
            // marks Finished and then yields in a loop, so until it is
            // switched away it is still executing -- or halted -- on the very
            // pages this would hand back. Freeing them lets the next `spawn`
            // build a new task on top of the old one's live frame, and the
            // old task resumes into it: a wild jump (measured: `rip 0x3`, the
            // return address the new frame happened to contain) attributed to
            // whichever task was unlucky enough to be running there.
            //
            // `on_cpu` is exactly that fact -- set when a CPU starts running a
            // task, cleared by the assembly switch once its registers are stored.
            // A finished task that has genuinely switched away has it clear
            // and is safe to reclaim; one still on a stack does not.
            if t.is_owned() {
                continue;
            }
            let orphan = TASKS[t.parent].state == State::Unused || TASKS[t.parent].state == State::Finished;
            let kthread = t.ttbr0 == 0;
            if !(t.shares_mm || orphan || kthread) {
                continue;
            }
            // A thread's address space belongs to the threads it shared it
            // with; an orphan process owns its own and takes it with it.
            if !t.shares_mm && t.ttbr0 != 0 {
                crate::paging::destroy_user_address_space(t.ttbr0);
                mm_drop(t.ttbr0);
            }
            return_stack(t.stack);
            frames::free(t.shadow as *mut u8);
            // A task that still holds TLS at reclaim never went through
            // `task_exit`: whatever host task pointer it left behind would
            // be inherited by the slot's next occupant -- which would then
            // destroy a Linux task belonging to a dead generation, or hand
            // its dead tid to `__switch_to`. Clear it, and say so: with
            // the current exit paths this line should never print.
            if let Some((key, gen)) = crate::hostops::tls_pending(id) {
                println!(
                    "  !! reclaim: task {} (gen {}) had live TLS key {} -- clearing",
                    id, gen, key
                );
                crate::hostops::tls_forget(id);
            }
            TASKS[id].state = State::Unused;
        }
    }
    unsafe { RUNQUEUE.unlock(g) };
}

/// Task stacks come from one arena carved out of RAM at first use.
///
/// `frames::alloc_contiguous` walks a bump pointer that `alloc()` shares: by
/// the time WebKit is up, single-frame allocations have walked it to the end
/// of RAM, and a contiguous request then fails even with tens of thousands
/// of frames free -- "out of memory for a task stack: free=22185" on a
/// machine with 86 MiB idle. The arena is taken once, while the bump still
/// has room, and handed out as 64 KiB stacks through a pool, so spawn/reap
/// churn reuses it and never touches the dying bump again.
const STACK_POOL_MAX: usize = 256;
const STACK_ARENA_PAGES: usize = 4096;      // 16 MiB, 256 stacks
static mut STACK_POOL: [usize; STACK_POOL_MAX] = [0; STACK_POOL_MAX];
static mut STACK_POOL_LEN: usize = 0;
static mut STACK_ARENA: usize = 0;
static mut STACK_ARENA_NEXT: usize = 0;

/// Serialises the stack pool. It was a plain `static mut` guarded only by an
/// interrupt mask, and an interrupt mask stops nothing on another CPU: an AP
/// spawning its idle task while the BSP spawned anything else ran two
/// `take_stack`s against one `STACK_POOL_LEN`, and the pool handed the *same*
/// 64 KiB stack to two tasks. Both then build their initial frame at the same
/// top and run on the same memory; the visible result is a wild jump or a
/// corrupted trap frame somewhere else entirely, which is what made this look
/// like a scheduler bug for so long.
///
/// Its own lock rather than the runqueue's: the pool is also touched from
/// `reclaim_finished`, which already holds the runqueue lock, so reusing that
/// lock here would self-deadlock. Held only for the pool/arena arithmetic --
/// zeroing a 64 KiB stack happens outside it.
static STACK_LOCK: crate::sync::Spinlock = crate::sync::Spinlock::named("STACK_LOCK");

fn take_stack() -> Option<usize> {
    let flags = STACK_LOCK.lock();
    let taken = unsafe {
        if STACK_POOL_LEN > 0 {
            STACK_POOL_LEN -= 1;
            let stack = STACK_POOL[STACK_POOL_LEN];
            STACK_POOL[STACK_POOL_LEN] = 0;
            Some(stack)
        } else if STACK_ARENA == 0 {
            // No `?` here: an early return would drop the lock still held.
            match frames::alloc_contiguous(STACK_ARENA_PAGES) {
                Some(p) => {
                    STACK_ARENA = p as usize;
                    STACK_ARENA_NEXT = STACK_PAGES;
                    Some(STACK_ARENA)
                }
                None => None,
            }
        } else if STACK_ARENA_NEXT + STACK_PAGES <= STACK_ARENA_PAGES {
            let stack = STACK_ARENA + STACK_ARENA_NEXT * PAGE;
            STACK_ARENA_NEXT += STACK_PAGES;
            Some(stack)
        } else {
            // Arena exhausted: the old path, and its panic, as the last word.
            frames::alloc_contiguous(STACK_PAGES).map(|p| p as usize)
        }
    };
    unsafe { STACK_LOCK.unlock(flags) };
    // Zeroed outside the lock: 64 KiB under a spinlock would stall every
    // other CPU for no reason. The address is already exclusively ours.
    if let Some(stack) = taken {
        unsafe { core::ptr::write_bytes(stack as *mut u8, 0, STACK_PAGES * PAGE) };
    }
    taken
}

fn return_stack(stack: usize) {
    let flags = STACK_LOCK.lock();
    unsafe {
        if STACK_POOL_LEN < STACK_POOL_MAX {
            STACK_POOL[STACK_POOL_LEN] = stack;
            STACK_POOL_LEN += 1;
        }
        // Full pool: the block stays in the arena. Returning it to the
        // single-frame list would put a 64 KiB stack beyond the reach of
        // every future take, which is how the bump ran out to begin with.
        STACK_LOCK.unlock(flags);
    }
}

/// Create a task. `entry` is called with `arg`, and falling off the end of it
/// is fine -- task_start catches the return.
pub fn spawn(name: &'static str, entry: extern "C" fn(usize), arg: usize) -> usize {
    let flags = crate::sync::irq_save();
    reclaim_finished();
    // Serialize spawns, but do not hold RUNQUEUE across the allocations:
    // the commit below takes RUNQUEUE itself. Lock order SPAWN -> RUNQUEUE.
    let sg = SPAWN.lock();
    unsafe {
        let placed = place_vruntime();
        let slot = match (*(&raw const TASKS)).iter().position(|t| t.state == State::Unused) {
            Some(s) => s,
            None => {
                crate::println!("out of task slots! dumping 64 tasks:");
                for (i, t) in (*(&raw const TASKS)).iter().enumerate() {
                    crate::println!(
                        "  [{}] name {:<10} state {:?} parent {} ttbr0 {:#x} mm {}",
                        i, t.name, t.state, t.parent, t.ttbr0, t.shares_mm
                    );
                }
                panic!("no free task slots");
            }
        };

        // Bumped before the task exists, so no id for this incarnation can
        // ever equal one handed out for the last occupant of the slot.
        let generation = TASKS[slot].generation.wrapping_add(1);

        let stack = take_stack().unwrap_or_else(|| {
            let (used, total) = frames::stats();
            panic!("out of memory for a task stack: frames total={} free={} requested={}",
                total, total - used, STACK_PAGES);
        }) as *mut u8;
        let top = stack as usize + STACK_PAGES * PAGE;

        // The frame cpu_switch will pop: twelve callee-saved registers, laid
        // out exactly as its `stp` sequence writes them. x30 is task_start, so
        // the `ret` at the end of cpu_switch lands there; x19 and x20 carry
        // what task_start needs, because there is no other way to pass an
        // argument through a `ret`.
        #[cfg(target_arch = "aarch64")]
        let sp = {
            let sp = top - 96;
            let f = sp as *mut usize;
            f.add(0).write(entry as *const () as usize); // x19
            f.add(1).write(arg); // x20
            for i in 2..10 {
                f.add(i).write(0); // x21..x28
            }
            f.add(10).write(0); // x29, frame pointer
            f.add(11).write(task_start as *const () as usize); // x30
            sp
        };

        #[cfg(target_arch = "x86_64")]
        let sp = {
            // Frame cpu_switch pops: r15, r14, r13, r12, rbx, rbp, ret
            // 7 words = 56 bytes.
            let sp = top - 56;
            let f = sp as *mut usize;
            f.add(0).write(0);                                  // r15
            f.add(1).write(0);                                  // r14
            f.add(2).write(arg);                                // r13 -> argument
            f.add(3).write(entry as *const () as usize);        // r12 -> entry function
            f.add(4).write(0);                                  // rbx
            f.add(5).write(0);                                  // rbp (frame pointer top)
            f.add(6).write(task_start as *const () as usize);   // return address -> task_start
            sp
        };

        let shadow = frames::alloc().expect("no memory for a task shadow") as usize;
        TASKS[slot] = Task {
            sp: core::sync::atomic::AtomicUsize::new(sp),
            stack: stack as usize,
            stack_size: STACK_PAGES * PAGE,
            shadow,
            state: State::Unused,
            owner_cpu: core::sync::atomic::AtomicUsize::new(NO_OWNER),
            name,
            home_cpu: cpu_index(crate::arch::cpu_id()) as u8,
            affinity: default_affinity(),
            migrations: 0,
            class: crate::schedpolicy::Class::Normal,
            // Placed against the run queue rather than at zero: a vruntime of
            // 0 is far behind every running task, so a new task would run
            // exclusively until it caught up -- which for a long-lived
            // machine is a very long time.
            vruntime: placed,
            interactive: 0,
            ran_at: 0,
            accounted_at: 0,
            slice_scored: false,
            wake_pending: false,
            ran_ns: 0,
            ready_at: crate::arch::cycles(),
            comm: [0; 16],
            slices: 0,
            waiting_on: 0,
            joiner: 0,
            wake_at: 0,
            last_syscall: 0,
            ttbr0: 0,
            linux_pid: 0,
            exit_status: 0,
            user_irqs: 0,
            brk: 0,
            brk_min: 0,
            mmap_next: 0,
            parent: 0,
            generation,
            shares_mm: false,
            vfork_done: 0,
            clear_child_tid: 0,
            has_console: false,
            nethos_abi: false,
            user_syscall: false,
            tpidr: 0,
            shared: [crate::user::SharedMap {
                start: 0,
                len: 0,
                region: 0,
                fd: -1,
                anonymous: false,
            }; 256],
            nshared: 0,
            sig_actions: [crate::signal::Action::default(); 64],
            sig_mask: 0,
            sig_pending: 0,
            sig_code: [0; 64],
            sig_sender: [0; 64],
            sig_alt_base: 0,
            sig_alt_size: 0,
            sig_alt_in_use: false,
            sig_return_mask: 0,
            sig_return_alt: false,
        };
        TASKS[slot].ttbr0 = crate::paging::kernel_address_space();
        // A slot that had an occupant before must not hand that task's
        // vectors to the new one.
        fp_reset(slot);
        // Publish under the runqueue lock. The slot was written field by
        // field above and `Ready` is the last store, but "last store" only
        // orders anything for a reader that also synchronises -- an
        // interrupt mask here stops other *handlers* on this CPU and
        // nothing on any other. An AP reaching `pick_on` takes this lock,
        // so acquiring it here both orders the slot's fields ahead of the
        // state and keeps the task invisible until they are all in place.
        // A waker that runs while this holds simply waits; it cannot see a
        // half-built task and switch into it.
        let published = RUNQUEUE.lock();
        TASKS[slot].state = State::Ready;
        unsafe { RUNQUEUE.unlock(published) };
        SPAWN.unlock(sg);
        crate::sync::irq_restore(flags);
        slot
    }
}

/// Run somebody else if anybody else wants to run. Whether it switched.
///
/// The answer is what the idle halt is built on: a task told "nobody else"
/// is the only thing between this CPU and an idle one, and on an emulated
/// machine a CPU that spins rather than stopping is a host core at 100% for
/// a guest doing nothing. `yield_now` and `sleep_ticks` halt on a false.
///
/// The halt is deliberately *not* here. This is called with interrupts
/// masked from every kind of critical section, and `irq_restore` puts back
/// what the caller had -- so halting here can halt with interrupts masked,
/// where `wfi` wakes on the pending tick but the handler that would make
/// something runnable never runs. That is a machine that stops during boot,
/// which is exactly what it did.
pub fn schedule() -> bool {
    // Mask before deriving the CPU: the value must be read in a window where
    // this frame cannot be interrupted and resumed on another CPU, or the
    // local scheduler state of one CPU is manipulated on another.
    let outer = crate::sync::irq_save();
    let cpu = cpu_index(crate::arch::cpu_id());
    let switched = schedule_on(cpu);
    unsafe { crate::sync::irq_restore(outer) };
    switched
}

/// Run the scheduler pinned to one CPU. `schedule()` is this on the calling
/// CPU; AP idle loops call it with their own index because `arch::cpu_id`
/// is trustworthy there too, but being explicit keeps the idle path honest.
/// The runqueue lock. Held across pick, account, and state changes --
/// every path that reads another task's state and acts on it. Without
/// this, two CPUs can pick the same Ready task and both switch into it:
/// one CPU's stack, two occupants, and the corruption that follows reads
/// as wild jumps and garbled consoles rather than anything schedulery.
///
/// A ticket lock, not irq_save alone: masking interrupts excludes handlers
/// on this CPU and nothing on any other. Short sections only -- never held
/// across cpu_switch, which would serialize all CPUs behind one stack
/// switch and deadlock the moment two schedule at once.
static RUNQUEUE: crate::sync::Spinlock = crate::sync::Spinlock::named("RUNQUEUE");

/// CPUs given to one task each (`dedicate_cpu`): bit n set means CPU n runs
/// only the task whose affinity is exactly that bit. Other tasks have the bit
/// stripped from their affinity when the CPU is dedicated, new tasks never
/// get it, a dedicated CPU never steals, and its tick is masked while its
/// task runs. For guest vCPUs: translated code that is never preempted.
static RESERVED: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);

pub fn reserved_cpus() -> u16 {
    RESERVED.load(Ordering::Acquire)
}

/// The affinity a new task starts with: every CPU not dedicated to another.
fn default_affinity() -> u16 {
    ALL_CPU_SLOTS & !RESERVED.load(Ordering::Acquire)
}

struct Dedicated {
    entry: extern "C" fn(usize),
    arg: usize,
    cpu: usize,
}

extern "C" fn dedicated_trampoline(arg: usize) {
    let d = *unsafe { alloc::boxed::Box::from_raw(arg as *mut Dedicated) };
    // `dedicate_cpu` narrows the affinity just after `spawn` publishes the
    // task, so another CPU may have picked it up first. Yield until the
    // scheduler has moved it; only then is this CPU's timer ours to mask.
    while cpu_index(crate::arch::cpu_id()) != d.cpu {
        yield_now();
    }
    // No periodic tick here: the LAPIC timer becomes this task's one-shot
    // high-resolution timer (hrtimer.rs), unarmed until it asks.
    #[cfg(target_arch = "x86_64")]
    crate::hrtimer::claim_this_cpu();
    (d.entry)(d.arg);
    #[cfg(target_arch = "x86_64")]
    crate::hrtimer::release_this_cpu();
    RESERVED.fetch_and(!(1u16 << d.cpu.min(MAX_CPUS - 1)), Ordering::AcqRel);
}

/// Give `cpu` to a new task running `entry(arg)`, and to nothing else, until
/// `entry` returns. Not the boot CPU (Linux and the global clock live there).
/// Returns the task id, or None if the CPU is the BSP, offline, or already
/// dedicated.
pub fn dedicate_cpu(cpu: usize, name: &'static str, entry: extern "C" fn(usize), arg: usize) -> Option<usize> {
    if cpu == 0 || cpu >= MAX_CPUS || !cpu_online(cpu) {
        return None;
    }
    let bit = 1u16 << cpu;
    if RESERVED.fetch_or(bit, Ordering::AcqRel) & bit != 0 {
        return None;
    }
    // Strip the CPU from every task that can run elsewhere, and move any
    // that call it home.
    let flags = RUNQUEUE.lock();
    unsafe {
        let tasks = &mut *(&raw mut TASKS);
        for (id, t) in tasks.iter_mut().enumerate() {
            if idle_owner_of_slot(id).is_some() || t.affinity & !bit == 0 {
                continue;
            }
            t.affinity &= !bit;
            if t.home_cpu as usize == cpu {
                for other in 0..cpu_count() {
                    if t.affinity & (1u16 << other.min(MAX_CPUS - 1)) != 0 && cpu_online(other) {
                        t.home_cpu = other as u8;
                        break;
                    }
                }
            }
        }
        RUNQUEUE.unlock(flags);
    }
    let boxed = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(Dedicated { entry, arg, cpu })) as usize;
    let id = spawn(name, dedicated_trampoline, boxed);
    set_affinity(id, bit);
    set_need_resched_on(cpu);
    crate::arch::send_ipi(cpu);
    Some(id)
}
// Serializes task creation: the slot scan reads every task's state and the
// commit writes a slot, and two concurrent spawns must not take one slot.
// Lock order is SPAWN -> RUNQUEUE; no path takes RUNQUEUE then SPAWN.
static SPAWN: crate::sync::Spinlock = crate::sync::Spinlock::named("SPAWN");

/// Whether LKL (Linux) is running. It is a single-CPU unikernel: its internal
/// state assumes one host thread executes Linux code at a time, and nk's
/// model -- preempt any task anywhere, run the next one on any CPU -- breaks
/// that the moment an AP picks up a task whose syscalls are forwarded into
/// Linux. The failure is not a clean fault in LKL: the boot thread, which is
/// the one *inside* Linux, is the task that dies, jumping into LKL's own
/// memory region with a corrupted frame (measured: RIP inside
/// 0x50000000..0x54000000 on the `boot` task, and stub frames shifted into
/// `rip 0x3`). Reproduced only with APs online: the same image is clean at
/// `-smp 1`, faults at `-smp 2` and `-smp 4`, and everything up to the Linux
/// handover -- including the whole SMP scheduler torture -- is unaffected.
///
/// So LKL is pinned to the boot CPU: the APs run every part of nk's own work
/// (which is what the SMP correctness work is about) and stand down once
/// Linux owns the machine. This is a deliberate scope line, not a fix for
/// LKL's own threading; multi-CPU Linux would need LKL's host threads to be
/// pinned and its CPU accounting kept per host CPU, which is a separate
/// piece of work.
static LKL_ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Called once, just before Linux takes over.
pub fn set_lkl_active() {
    LKL_ACTIVE.store(true, core::sync::atomic::Ordering::SeqCst);
    // Kick every other CPU: whatever it is running now must come off before
    // it reaches a syscall, because from this point only the boot CPU may
    // execute Linux code. `schedule_on` on those CPUs will find nothing to
    // pick (see `pick_on`) and idle; the task it was running is left Ready
    // and the boot CPU takes it.
    for cpu in 1..cpu_count() {
        if cpu_online(cpu) {
            request_reschedule(cpu);
        }
    }
}

pub fn lkl_active() -> bool {
    LKL_ACTIVE.load(core::sync::atomic::Ordering::SeqCst)
}

/// Publish the task the running CPU just replaced: clear its `on_cpu`, so
/// that the `Ready` it was marked as is now real and it may be picked.
///
/// Called by every task the moment it starts executing -- `task_start` for a
/// fresh task, the tail of `schedule_on` for one that was switched out and
/// resumed -- so the delay between switching and publishing is a few
/// instructions.
///
/// The CPU is resolved here rather than passed in because the frame that
/// resumes a task is the frame from where it was switched *out*, which may
/// be a different CPU. A task switched out on cpu0 and resumed on cpu1 runs
/// cpu0's stale frame; a `cpu` argument would publish on cpu0 and leave
/// cpu1's `prev` unpublished, so the task it replaced would stay
/// unpickable forever.
///
/// # Safety
/// Runs on the CPU that performed the switch, before anything else.

/// the assembly task entry points, which cannot name a
/// Rust function with a generic signature. Resolves the calling CPU the
/// same way `schedule` does.
#[no_mangle]

pub fn schedule_on(cpu: usize) -> bool {
    let cpu = cpu_index(cpu);
    // Local scheduling only: the argument must be the executing CPU.
    #[cfg(target_arch = "aarch64")]
    {
        let actual = cpu_index(crate::arch::cpu_id());
        assert!(actual == cpu, "schedule_on({}) on cpu{}", cpu, actual);
    }
    // Mask first, then take the lock. Taking the lock alone would restore
    // interrupts when it is released, and there is a switch still to come:
    // in that window a timer could re-enter `schedule` on a half-built
    // frame. With the outer save, the lock's release restores *masked*, so
    // interrupts stay off until the switch has landed and this task's own
    // state is restored below. `outer` is frame-local, so a resumed task
    // restores its own interrupt state, not the CPU's last one.
    let outer = crate::sync::irq_save();
    let flags = RUNQUEUE.lock();
    unsafe {
        // No exclusive reference survives cpu_switch: another task mutates
        // this table while the outgoing one sleeps.
        let cur = (*(&raw const CPUS))[cpu].current.load(Ordering::Acquire);
        let now = crate::arch::cycles();

        // Charge the outgoing task for what it just ran, before anything
        // else looks at vruntimes -- the pick below compares against it.
        let ran_ns = account(cur, now);

        let mut next = pick_on(cur, cpu);
        if next.is_none() {
            // This CPU's own idle task, and only when it is available.
            let idle = idle_task_of(cpu) as usize;
            if idle < MAX_TASKS
                && idle != cur
                && TASKS[idle].state == State::Ready
                && !TASKS[idle].is_owned()
            {
                next = Some(idle);
            }
        }
        if let Some(next) = next {
            if TASKS[cur].state == State::Running {
                TASKS[cur].state = State::Ready;
                TASKS[cur].ready_at = now;
            }

            // Wakeup-to-run latency: the number the whole policy is judged
            // by. Measured here because this is the moment the wait ends.
            if TASKS[next].wake_pending {
                crate::schedtrace::record_latency(now.wrapping_sub(TASKS[next].ready_at));
            }

            crate::schedtrace::record(
                cpu,
                crate::schedtrace::Event::ContextSwitch,
                next,
                cur,
                effective_class_of(next).as_u8(),
                now.wrapping_sub(TASKS[cur].ran_at),
                ran_ns,
            );

            TASKS[next].state = State::Running;
            TASKS[next].set_owner(cpu);
            TASKS[next].slices += 1;
            TASKS[next].ran_at = now;
            TASKS[next].accounted_at = now;
            TASKS[next].slice_scored = false;
            TASKS[next].wake_pending = false;
            TASKS[next].ready_at = 0;
            TASKS[cur].ttbr0 = crate::arch::current_page_table();
            TASKS[cur].tpidr = crate::arch::current_tls();
            // `current` is published by `cpu_switch` itself, in the same
            // sequence as the stack handoff.
            (*(&raw mut CPUS))[cpu].switches.fetch_add(1, Ordering::Relaxed);
            if (*(&raw const CPUS))[cpu].idle.load(Ordering::Acquire) {
                set_cpu_idle(cpu, false);
                crate::schedtrace::record(
                    cpu,
                    crate::schedtrace::Event::IdleExit,
                    next,
                    cur,
                    effective_class_of(next).as_u8(),
                    0,
                    0,
                );
            }
            set_shadow(TASKS[next].shadow);
            // A space without the kernel's vectors cannot take an exception:
            // the vector fetch loops forever and the machine is gone.
            // A task can still reference a space that was torn down (its
            // L0 has been cleared). Switching to it freezes the machine:
            // the first exception's vector fetch faults at level 0, and the
            // entry faults again every time it is taken. Run such a task on
            // the kernel's own space instead -- its user addresses fault
            // visibly, which is a process failure rather than a dead
            // machine -- and say so.
            if !crate::paging::mapped_with_root(TASKS[next].ttbr0, 0x4008_0800) {
                crate::println!(
                    "  !! task {} ({}) has torn-down space {:#x}; running on the kernel's",
                    next, TASKS[next].name, TASKS[next].ttbr0
                );
                crate::arch::set_page_table(crate::paging::kernel_address_space());
            } else {
                crate::arch::set_page_table(TASKS[next].ttbr0);
            }
            crate::arch::set_tls(TASKS[next].tpidr);
            // On x86-64 the kernel stack a trap or syscall lands on is not a
            // register the CPU keeps per task: it is read from the TSS (for an
            // interrupt) or from a variable the syscall stub loads. Both name
            // one stack, so both have to be repointed at the incoming task or
            // its first syscall would run on the outgoing task's stack.
            // aarch64 needs nothing here -- SP_EL1 is banked.
            #[cfg(target_arch = "x86_64")]
            set_kernel_stack_for(next);
            // Park the outgoing task where the task that starts running next
            // can find it. `cur` was just marked Ready, but its registers
            // are still live on this CPU -- `cpu_switch` has not run yet --
            // so it must not be picked until this switch completes. The
            // incoming task clears `cur`'s `on_cpu` in the switch itself, and
            // `pick_on` refuses any task that still has it set. Without that
            // two CPUs end up sharing one stack.
            // Validate the incoming saved context before committing to it.
            // A value outside the task's own stack means an earlier switch
            // saved a foreign SP; aborting here keeps the machine coherent.
            let incoming = TASKS[next].sp.load(Ordering::Acquire);
            let stack = TASKS[next].stack;
            let size = TASKS[next].stack_size;
            if incoming == 0 || (stack != 0 && size != 0 && !(incoming > stack && incoming <= stack + size)) {
                crate::println!("  !! incoming SP {:#x} outside task {} '{}' stack {:#x}+{:#x}; aborting switch",
                    incoming, next, TASKS[next].name, stack, size);
                TASKS[next].state = State::Ready;
                TASKS[next].release_owner();
                TASKS[cur].state = State::Running;
                RUNQUEUE.unlock(flags);
                crate::sync::irq_restore(outer);
                return false;
            }
            // Releases the runqueue lock only; interrupts stay masked
            // because `outer` masked them before the lock was taken.
            unsafe { RUNQUEUE.unlock(flags) };
            // The switch publishes `current`, switches the stack, and clears
            // the outgoing task's `on_cpu` with a release store -- after the
            // saved SP above is visible.
            // Record the outgoing task for the switch that is about to
            // happen. This is a physical-switch fact, not ownership: it
            // identifies the task this CPU is releasing right now, with no
            // dependence on Rust frame locals after the switch.
            (*(&raw mut CPUS))[cpu].released_task.store(cur, Ordering::Relaxed);
            cpu_switch(
                &raw mut TASKS[cur].owner_cpu as *mut usize,
                &raw mut TASKS[cur].sp as *mut usize,
                incoming,
                &raw mut (*(&raw mut CPUS))[cpu].current as *mut usize,
                next,
            );

            // Common switch-completion. IRQs are still masked here; the CPU
            // identity is derived from hardware because this frame may have
            // been built on a different CPU when this task last blocked.
            while finish_switch_work() {
                schedule_on(cpu_index(crate::arch::cpu_id()));
            }

            crate::sync::irq_restore(outer);
            return true;
        }
        if TASKS[cur].state != State::Running {
            set_cpu_idle(cpu, true);
            crate::schedtrace::record(
                cpu,
                crate::schedtrace::Event::IdleEnter,
                cur,
                cur,
                0,
                0,
                0,
            );
        }
        unsafe { RUNQUEUE.unlock(flags) };
        crate::sync::irq_restore(outer);
        false
    }
}

/// Whether the task still carries its wake obligation. For the regression
/// test: it must be true from the wake until a successful claim.
pub fn wake_pending_of(slot: usize) -> bool {
    if slot >= MAX_TASKS {
        return false;
    }
    let g = RUNQUEUE.lock();
    let w = unsafe { (*(&raw const TASKS))[slot].wake_pending };
    unsafe { RUNQUEUE.unlock(g) };
    w
}

/// The completed-switch record currently held for a CPU (`NO_TASK` when the
/// switch that last ran on it has already been consumed). For diagnostics.
pub fn released_task_of(cpu: usize) -> usize {
    let cpu = cpu_index(cpu);
    unsafe { (*(&raw const CPUS))[cpu].released_task.load(Ordering::Acquire) }
}

/// Consume this CPU's completed-switch record and, if the switch released a
/// task that is now Ready, still carries its wake obligation, and is
/// unowned, create a fresh scheduling opportunity for it.
///
/// Returns true when the caller must re-enter the scheduler on this CPU
/// before returning to ordinary execution (local forwarding). Remote
/// forwarding sets the target's doorbell and sends the IPI outside the
/// runqueue lock. `wake_pending` is never cleared here: only a successful
/// claim discharges it.
pub fn finish_switch_work() -> bool {
    let cpu = cpu_index(crate::arch::cpu_id());
    let released = unsafe {
        (*(&raw mut CPUS))[cpu].released_task.swap(NO_TASK, Ordering::AcqRel)
    };
    if released >= MAX_TASKS {
        return false;
    }
    let mut local = false;
    let mut remote: usize = usize::MAX;
    {
        let g = RUNQUEUE.lock();
        unsafe {
            let t = &(*(&raw const TASKS))[released];
            if t.state == State::Ready && t.wake_pending && t.owner() == NO_OWNER {
                let mask = t.affinity;
                if mask & (1u16 << cpu.min(MAX_CPUS - 1)) != 0 {
                    local = true;
                } else {
                    let n = cpu_count();
                    let mut c = 0usize;
                    while c < n {
                        if mask & (1u16 << c.min(MAX_CPUS - 1)) != 0 && cpu_online(c) {
                            remote = c;
                            break;
                        }
                        c += 1;
                    }
                    if remote == usize::MAX {
                        local = true;
                    }
                }
            }
        }
        unsafe { RUNQUEUE.unlock(g) };
    }
    if local {
        return true;
    }
    if remote != usize::MAX {
        set_need_resched_on(remote);
        crate::arch::send_ipi(remote);
    }
    false
}

/// Entry every fresh task takes after its first physical switch. Consumes
/// the completed-switch record (a switch into a fresh task releases an
/// outgoing one just like any other) and honours a local retry before the
/// task body runs. Called with IRQs masked from `task_start`.
#[no_mangle]
pub extern "C" fn switch_complete_entry() {
    if finish_switch_work() {
        schedule();
    }
}

/// The class a task is actually treated as: what it was assigned, adjusted
/// by how it has behaved. See `schedpolicy::effective_class`.
fn effective_class_of(id: usize) -> crate::schedpolicy::Class {
    unsafe {
        let t = &(*(&raw const TASKS))[id];
        crate::schedpolicy::effective_class(t.class, t.interactive)
    }
}

/// Charge a task for the CPU time it has just used, and let that time move
/// its interactivity score. Returns how long it ran, in nanoseconds.
///
/// Called on the way out of `schedule` for the outgoing task. Both the
/// fairness key and the interactivity evidence come from this one number, so
/// it is computed once, here, rather than at each of the places that want it.
///
/// # Safety
/// Caller holds the scheduler's interrupt mask.
unsafe fn account(id: usize, now: u64) -> u64 {
    account_task(&mut TASKS[id], now)
}

struct AccountState {
    state: State,
    class: crate::schedpolicy::Class,
    interactive: i8,
    ran_at: u64,
    accounted_at: u64,
    ran_ns: u64,
    vruntime: u64,
    slice_scored: bool,
}

fn account_state(s: &mut AccountState, now: u64) -> u64 {
    if s.accounted_at == 0 {
        if s.state == State::Running {
            s.ran_at = now;
            s.accounted_at = now;
        }
        return 0;
    }
    let ran_ns = crate::timer::cycles_to_ns(now.wrapping_sub(s.accounted_at));
    s.accounted_at = if matches!(s.state, State::Blocked | State::Finished) { 0 } else { now };
    s.ran_ns = s.ran_ns.wrapping_add(ran_ns);
    let class = crate::schedpolicy::effective_class(s.class, s.interactive);
    s.vruntime = s.vruntime.saturating_add(crate::schedpolicy::vruntime_delta(
        ran_ns, crate::schedpolicy::class_weight(class),
    ));
    let slice = crate::schedpolicy::slice_ns(class);
    let slice_ran = crate::timer::cycles_to_ns(now.wrapping_sub(s.ran_at));
    if s.state == State::Blocked {
        s.interactive = crate::schedpolicy::on_voluntary_block(s.interactive, slice_ran, slice);
    } else if s.state == State::Running && slice_ran >= slice && !s.slice_scored {
        s.interactive = crate::schedpolicy::on_slice_exhausted(s.interactive);
        s.slice_scored = true;
    }
    ran_ns
}

pub fn accounting_regression() -> bool {
    let step = 1000;
    let mut running = AccountState {
        state: State::Running,
        class: crate::schedpolicy::Class::Normal,
        interactive: 0,
        ran_at: 1,
        accounted_at: 1,
        ran_ns: 0,
        vruntime: 0,
        slice_scored: false,
    };
    let first = account_state(&mut running, 1 + step);
    let second = account_state(&mut running, 1 + step * 2);
    let exact = running.ran_ns == first + second && first == second && running.ran_at == 1;
    let mut blocked = AccountState {
        state: State::Blocked,
        class: crate::schedpolicy::Class::Normal,
        interactive: 0,
        ran_at: 1,
        accounted_at: 1,
        ran_ns: 100,
        vruntime: 200,
        slice_scored: false,
    };
    account_state(&mut blocked, 1 + step * 3);
    let before = (blocked.ran_ns, blocked.vruntime, blocked.interactive, blocked.accounted_at);
    account_state(&mut blocked, 1 + step * 100);
    let blocked_once = (blocked.ran_ns, blocked.vruntime, blocked.interactive, blocked.accounted_at) == before;
    let mut idle = AccountState {
        state: State::Running,
        class: crate::schedpolicy::Class::Normal,
        interactive: 0,
        ran_at: 1,
        accounted_at: 0,
        ran_ns: before.0,
        vruntime: before.1,
        slice_scored: false,
    };
    account_state(&mut idle, 1 + step * 200);
    let idle_free = (idle.ran_ns, idle.vruntime) == (before.0, before.1);
    let mut scored = AccountState {
        state: State::Running,
        class: crate::schedpolicy::Class::Normal,
        interactive: 4,
        ran_at: 1,
        accounted_at: 1,
        ran_ns: 0,
        vruntime: 0,
        slice_scored: false,
    };
    account_state(&mut scored, 1_000_000_000);
    let score = (scored.interactive, scored.slice_scored);
    account_state(&mut scored, 1_000_000_000 + step);
    exact && blocked_once && idle_free && score == (2, true) && (scored.interactive, scored.slice_scored) == score
}

pub fn placement_regression() -> bool {
    // Under the runqueue lock, not just an interrupt mask. This test drives
    // the real `place_vruntime`/`make_ready` path by turning a live, unused
    // slot into a `Blocked` task and making it `Ready` -- and for an instant
    // that slot is a runnable task with no stack and no saved registers.
    // With only the interrupt mask held, another CPU is free to run
    // `pick_on`, choose it, and `cpu_switch` into it: two CPUs then treat a
    // dead slot as a task, which shows up as a hang or a wild jump nowhere
    // near this function. The mask stops nothing on other CPUs; the
    // runqueue lock is what `pick_on` is reached under, so holding it makes
    // the fabricated state unobservable. Restoration happens before the
    // release for the same reason.
    let flags = RUNQUEUE.lock();
    let passed = unsafe {
        if let Some(id) = (*(&raw const TASKS)).iter().position(|t| t.state == State::Unused) {
            let saved_state = TASKS[id].state;
            let saved_vruntime = TASKS[id].vruntime;
            let saved_ready = TASKS[id].ready_at;
            let saved_pending = TASKS[id].wake_pending;
            let current = TASKS[cur()].vruntime;
            TASKS[cur()].vruntime = current.max(1000);
            let expected = place_vruntime();
            TASKS[id].state = State::Blocked;
            TASKS[id].vruntime = 0;
            make_ready(id, 0);
            let passed = TASKS[id].vruntime == expected && TASKS[id].wake_pending;
            TASKS[id].state = saved_state;
            TASKS[id].vruntime = saved_vruntime;
            TASKS[id].ready_at = saved_ready;
            TASKS[id].wake_pending = saved_pending;
            TASKS[cur()].vruntime = current;
            passed
        } else {
            false
        }
    };
    unsafe { RUNQUEUE.unlock(flags) };
    passed
}

fn account_task(t: &mut Task, now: u64) -> u64 {
    if t.accounted_at == 0 {
        if t.state == State::Running {
            t.ran_at = now;
            t.accounted_at = now;
        }
        return 0;
    }
    let ran_ns = crate::timer::cycles_to_ns(now.wrapping_sub(t.accounted_at));
    t.accounted_at = if matches!(t.state, State::Blocked | State::Finished) { 0 } else { now };
    t.ran_ns = t.ran_ns.saturating_add(ran_ns);
    let class = crate::schedpolicy::effective_class(t.class, t.interactive);
    t.vruntime = t.vruntime.saturating_add(crate::schedpolicy::vruntime_delta(
        ran_ns, crate::schedpolicy::class_weight(class),
    ));
    let slice = crate::schedpolicy::slice_ns(class);
    let slice_ran = crate::timer::cycles_to_ns(now.wrapping_sub(t.ran_at));
    if t.state == State::Blocked {
        t.interactive = crate::schedpolicy::on_voluntary_block(t.interactive, slice_ran, slice);
    } else if t.state == State::Running && slice_ran >= slice && !t.slice_scored {
        t.interactive = crate::schedpolicy::on_slice_exhausted(t.interactive);
        t.slice_scored = true;
    }
    ran_ns
}

/// Where a new or newly-woken task's vruntime should start.
///
/// The minimum across the run queue, not zero. A task placed at zero is
/// arbitrarily far behind everything else and runs to the exclusion of all
/// of it until it catches up -- on a machine that has been up for an hour,
/// that is minutes of one task. Placing it at the minimum makes it merely
/// *next*, which is what "it should run soon" actually means.
fn place_vruntime() -> u64 {
    unsafe {
        let tasks = &*(&raw const TASKS);
        let mut min = u64::MAX;
        for t in tasks.iter() {
            if matches!(t.state, State::Ready | State::Running) && t.vruntime < min {
                min = t.vruntime;
            }
        }
        if min == u64::MAX {
            0
        } else {
            min
        }
    }
}

/// Choose the next task to run, or `None` if nothing else is runnable.
///
/// Highest effective class first, and within that class the least weighted
/// runtime. Strict class ordering is what makes interactive latency
/// predictable; `STARVATION_NS` is what stops it being cruel.
///
/// Linear over the task table. 128 slots is a few hundred nanoseconds and is
/// not what will be slow here -- and a tree would have to be kept correct
/// across every state change in this file, which is a much better way to
/// introduce a bug than to save time that is not being lost.
///
/// # Safety
/// Caller holds the scheduler's interrupt mask.
unsafe fn pick(cur: usize) -> Option<usize> {
    pick_on(cur, cpu_index(crate::arch::cpu_id()))
}

unsafe fn pick_on(cur: usize, cpu: usize) -> Option<usize> {
    let tasks = &*(&raw const TASKS);
    let now = crate::arch::cycles();
    let cpu = cpu_index(cpu);
    // Linux runs on the boot CPU only; see `LKL_ACTIVE`. An AP that picked a
    // Linux-ABI task here would run its syscalls into LKL concurrently with
    // the boot CPU's, and LKL's single-CPU state cannot survive that.
    // An AP may still run nk-native tasks that exclude CPU 0 -- which no
    // Linux-ABI task does -- so a dedicated vCPU keeps its core while Linux
    // runs. Everything that may run on CPU 0 is left to CPU 0.
    let ap_under_linux = cpu != 0 && lkl_active();
    let cpu_bit = 1u16 << (cpu.min(MAX_CPUS - 1));
    let dedicated = RESERVED.load(Ordering::Acquire) & cpu_bit != 0;

    let mut best: Option<usize> = None;
    let mut best_class = crate::schedpolicy::Class::Background;
    let mut best_vruntime = u64::MAX;

    // Round-robin the scan origin so that tasks which tie on both class and
    // vruntime -- everything, on an idle machine where nobody has run for a
    // measurable time -- still take turns rather than the lowest slot always
    // winning. Without this a machine whose tasks all sit at vruntime 0
    // starves every slot above the first.
    for i in 1..=MAX_TASKS {
        let id = (cur + i) % MAX_TASKS;
        if tasks[id].state != State::Ready {
            continue;
        }
        // Idle tasks are a per-CPU fallback and are never selected here,
        // for any CPU: the fallback in `schedule_on` is the single selection
        // site, and it only ever chooses this CPU's own idle task.
        if idle_owner_of_slot(id).is_some() {
            continue;
        }
        // Still on another CPU: marked Ready so it is picked again later,
        // but `cpu_switch` has not finished storing it. See `Task::on_cpu`.
        if tasks[id].is_owned() {
            continue;
        }
        if tasks[id].affinity & cpu_bit == 0 {
            continue;
        }
        if ap_under_linux && tasks[id].affinity & 1 != 0 {
            continue;
        }
        if dedicated && tasks[id].affinity != cpu_bit {
            continue;
        }

        let mut class = crate::schedpolicy::effective_class(tasks[id].class, tasks[id].interactive);

        // Ageing. A task that has been ready far too long is considered one
        // class higher than it is, which is enough to get it past whatever
        // has been monopolising the CPU without inverting the policy
        // wholesale. This is the promise that a background job still
        // finishes while the user keeps working.
        if tasks[id].ready_at != 0 {
            let waited = crate::timer::cycles_to_ns(now.wrapping_sub(tasks[id].ready_at));
            if waited >= crate::schedpolicy::STARVATION_NS {
                class = match class {
                    crate::schedpolicy::Class::Background
                    | crate::schedpolicy::Class::Throughput
                    | crate::schedpolicy::Class::Normal => crate::schedpolicy::Class::Interactive,
                    other => other,
                };
                crate::schedtrace::record(
                    0,
                    crate::schedtrace::Event::Aged,
                    id,
                    cur,
                    class.as_u8(),
                    waited,
                    0,
                );
            }
        }

        let better = match best {
            None => true,
            Some(_) => class > best_class || (class == best_class && tasks[id].vruntime < best_vruntime),
        };
        if better {
            best = Some(id);
            best_class = class;
            best_vruntime = tasks[id].vruntime;
        }
    }
    best
}

pub fn balance_on_idle(cpu: usize) -> bool {
    let cpu = cpu_index(cpu);
    let n = cpu_count();
    if n <= 1 {
        return false;
    }
    // Same line as `pick_on`: while Linux is running, its tasks may only
    // execute on the boot CPU, so an AP must not pull one onto itself.
    if cpu != 0 && lkl_active() {
        return false;
    }
    // A dedicated CPU runs its one task and never pulls work.
    if RESERVED.load(Ordering::Acquire) & (1u16 << cpu.min(MAX_CPUS - 1)) != 0 {
        return false;
    }
    // Under the runqueue lock: the scan reads other tasks' state/affinity
    // and writes `home_cpu`, the same fields `pick_on` and `set_affinity`
    // mutate under this lock.
    let flags = RUNQUEUE.lock();
    let stolen = unsafe { balance_on_idle_locked(cpu, n) };
    unsafe { RUNQUEUE.unlock(flags) };
    stolen
}

unsafe fn balance_on_idle_locked(cpu: usize, n: usize) -> bool {
    let tasks = &mut *(&raw mut TASKS);
    let cpu_bit = 1u16 << cpu.min(MAX_CPUS - 1);
    let mut best_cpu = usize::MAX;
    let mut best_count = 1usize;
    for other in 0..n {
        if other == cpu || !cpu_online(other) {
            continue;
        }
        let mut count = 0usize;
        for (id, t) in tasks.iter().enumerate() {
            if idle_owner_of_slot(id).is_some() {
                continue;
            }
            if t.state == State::Ready && t.affinity & (1u16 << other.min(MAX_CPUS - 1)) != 0 {
                count += 1;
            }
        }
        if count > best_count {
            best_count = count;
            best_cpu = other;
        }
    }
    if best_cpu == usize::MAX {
        return false;
    }
    let mut best: Option<usize> = None;
    let mut best_vruntime = u64::MAX;
    for (id, t) in tasks.iter().enumerate() {
        if idle_owner_of_slot(id).is_some() {
            continue;
        }
        if t.state != State::Ready {
            continue;
        }
        if t.affinity & cpu_bit == 0 {
            continue;
        }
        if t.affinity & (1u16 << best_cpu.min(MAX_CPUS - 1)) == 0 {
            continue;
        }
        if t.vruntime < best_vruntime {
            best_vruntime = t.vruntime;
            best = Some(id);
        }
    }
    if let Some(id) = best {
        tasks[id].home_cpu = cpu as u8;
        tasks[id].migrations += 1;
        (*(&raw mut CPUS))[cpu].stolen.fetch_add(1, Ordering::Relaxed);
        crate::schedtrace::record(
            cpu,
            crate::schedtrace::Event::Migrate,
            id,
            (*(&raw const CPUS))[cpu].current.load(Ordering::Acquire),
            tasks[id].class.as_u8(),
            best_cpu as u64,
            tasks[id].vruntime,
        );
        return true;
    }
    false
}

/// Stop the CPU until an interrupt, with interrupts open across the halt.
///
/// Only for a task that has nothing to do and holds nothing: the mask is
/// lifted so the handler that makes something runnable can actually run,
/// and put back afterwards. `wfi` on an already-pending interrupt returns
/// at once, so this cannot sleep through a wake that has already happened.
fn halt_until_interrupt() {
    let saved = crate::sync::irq_save();
    unsafe {
        account(cur(), crate::arch::cycles());
        TASKS[cur()].accounted_at = 0;
        #[cfg(target_arch = "aarch64")]
        {
            let flags: u64;
            core::arch::asm!("mrs {}, daif", out(reg) flags, options(nomem, nostack));
            core::arch::asm!("msr daifclr, #0xf", options(nomem, nostack));
            core::arch::asm!("wfi", options(nomem, nostack));
            core::arch::asm!("msr daif, {}", in(reg) flags, options(nomem, nostack));
        }
        #[cfg(target_arch = "x86_64")]
        {
            // `sti; hlt` is the standard x86 idle sequence: interrupts are
            // enabled for the halt instruction itself without a window for
            // an interrupt to land before the CPU actually stops.
            core::arch::asm!("sti", "hlt", "cli", options(nomem, nostack));
        }
        if TASKS[cur()].state == State::Running {
            TASKS[cur()].ran_at = crate::arch::cycles();
            TASKS[cur()].accounted_at = TASKS[cur()].ran_at;
            TASKS[cur()].slice_scored = false;
        }
        crate::sync::irq_restore(saved);
    }
}

/// The memory layout of one address space: heap break, its floor, and where
/// the next anonymous mapping goes.
///
/// Per address space, keyed by `TTBR0` -- not per task. Threads share their
/// creator's tables, so two threads allocating at once must draw from the
/// same pool: per-task copies diverge, hand out overlapping regions, and one
/// thread's committed page lands in another's `PROT_NONE` reserve (an EL0
/// permission fault in a thread that did nothing wrong, while the committing
/// `mprotect` prints success a line later). `fork` copies the entry for the
/// child's new root; a new thread shares its creator's.
///
/// Fixed-size and linear: one entry per live process at most, scanned with
/// interrupts masked. Entries are dropped when the address space is
/// destroyed (`reap_process` for processes, `execve` for the old root).
#[derive(Clone, Copy)]
struct MmLayout {
    ttbr0: u64,
    brk: u64,
    brk_min: u64,
    mmap_next: u64,
    live: bool,
}

static mut MM: [MmLayout; MAX_TASKS] = [MmLayout {
    ttbr0: 0,
    brk: 0,
    brk_min: 0,
    mmap_next: 0,
    live: false,
}; MAX_TASKS];

/// The layout for `ttbr0`, if it has one.
fn mm_find(ttbr0: u64) -> Option<(u64, u64, u64)> {
    unsafe {
        (*(&raw const MM))
            .iter()
            .find(|e| e.live && e.ttbr0 == ttbr0)
            .map(|e| (e.brk, e.brk_min, e.mmap_next))
    }
}

fn mm_current_ttbr0() -> u64 {
    crate::arch::current_page_table()
}

/// Give an address space its memory layout, just before a task enters EL0
/// in it. The root is passed explicitly: at every call site the new tables
/// exist but are not installed yet (spawn, `execve`), so reading TTBR0 would
/// name the wrong address space. It is set here rather than carried in the
/// `Process` because `brk` and `mmap` are answered from whatever thread is
/// running, and that is this one.
pub fn set_user_memory_for(ttbr0: u64, brk: u64, mmap_top: u64) {
    // One entry per root: a stale one for a recycled table address is
    // dropped first (table pages are reused after destroy).
    mm_drop(ttbr0);
    mm_create(ttbr0, brk, mmap_top);
}

/// A fresh layout for new tables. The tables are new, so no other task can
/// hold this root yet and no lock is needed beyond the write itself.
fn mm_create(ttbr0: u64, brk: u64, mmap_top: u64) {
    let flags = crate::sync::irq_save();
    unsafe {
        if let Some(e) = (*(&raw mut MM)).iter_mut().find(|e| !e.live) {
            *e = MmLayout {
                ttbr0,
                brk,
                brk_min: brk,
                mmap_next: mmap_top,
                live: true,
            };
        } else {
            panic!("out of address-space layouts");
        }
        crate::sync::irq_restore(flags);
    }
}

/// Copy the calling task's layout to a new root, for `fork`. The values are
/// read under the same mask that every reservation takes, so a sibling
/// allocating concurrently cannot slip an update between the read and the
/// copy.
fn mm_copy_to(new_ttbr0: u64) {
    let flags = crate::sync::irq_save();
    unsafe {
        let cur = mm_current_ttbr0();
        let found = (*(&raw const MM))
            .iter()
            .find(|e| e.live && e.ttbr0 == cur)
            .copied();
        if let Some(src) = found {
            if let Some(e) = (*(&raw mut MM)).iter_mut().find(|e| !e.live) {
                *e = MmLayout {
                    ttbr0: new_ttbr0,
                    brk: src.brk,
                    brk_min: src.brk_min,
                    mmap_next: src.mmap_next,
                    live: true,
                };
            } else {
                panic!("out of address-space layouts");
            }
        }
        crate::sync::irq_restore(flags);
    }
}

/// Drop the layout for `ttbr0`, if any. Called when the tables are destroyed.
/// Threads must not call this: the address space outlives any one of them.
fn mm_drop(ttbr0: u64) {
    let flags = crate::sync::irq_save();
    unsafe {
        if let Some(e) = (*(&raw mut MM))
            .iter_mut()
            .find(|e| e.live && e.ttbr0 == ttbr0)
        {
            e.live = false;
        }
        crate::sync::irq_restore(flags);
    }
}

/// Give the current task a process's memory layout, just before it enters
/// EL0. It is set here rather than carried in the `Process` because `brk` and
/// `mmap` are answered from whatever thread is running, and that is this one.
pub fn set_user_memory(brk: u64, mmap_top: u64) {
    let ttbr0 = mm_current_ttbr0();
    // A replaced-in-place root (execve reuses the caller's tables until it
    // switches) keeps one entry: drop any stale one first.
    mm_drop(ttbr0);
    mm_create(ttbr0, brk, mmap_top);
}

/// (brk, brk_min, mmap_next) for the running task's address space.
pub fn user_memory() -> (u64, u64, u64) {
    let ttbr0 = mm_current_ttbr0();
    mm_find(ttbr0).unwrap_or((0, 0, 0))
}

/// Reserve `len` bytes of anonymous mapping space, growing down from the top.
/// One critical section from read to store: two threads reserving at once
/// must not read the same `mmap_next`. Returns the base, or `None` when the
/// address space has no room. The floor is the arena's base: without it the
/// pointer walks off the bottom on a failure and every later mapping is
/// `mmapfail range` -- the reservation leaks into a range `range_ok` was
/// always going to refuse.
pub fn reserve_mmap(len: u64) -> Option<u64> {
    const HIGH_BASE: u64 = crate::user::USER_HIGH_BASE;
    let ttbr0 = mm_current_ttbr0();
    let flags = crate::sync::irq_save();
    let at = unsafe {
        (*(&raw mut MM))
            .iter_mut()
            .find(|e| e.live && e.ttbr0 == ttbr0)
            .and_then(|e| {
                let at = e.mmap_next.checked_sub(len)?;
                if at < HIGH_BASE {
                    return None;
                }
                e.mmap_next = at;
                Some(at)
            })
    };
    unsafe { crate::sync::irq_restore(flags) };
    at
}

/// Commit a grown break previously mapped by the caller. Re-checks the range
/// under the mask: a sibling that moved `brk` or `mmap_next` meanwhile makes
/// this fail rather than silently overlap it. Returns the break to report.
pub fn commit_brk(addr: u64, page_want: u64) -> u64 {
    let ttbr0 = mm_current_ttbr0();
    let flags = crate::sync::irq_save();
    let ret = unsafe {
        match (*(&raw mut MM))
            .iter_mut()
            .find(|e| e.live && e.ttbr0 == ttbr0)
        {
            Some(e) if page_want >= e.brk_min && page_want < e.mmap_next => {
                e.brk = page_want;
                addr
            }
            Some(e) => e.brk,
            None => 0,
        }
    };
    unsafe { crate::sync::irq_restore(flags) };
    ret as i64 as u64
}

pub fn set_user_brk(v: u64) {
    let ttbr0 = mm_current_ttbr0();
    let flags = crate::sync::irq_save();
    unsafe {
        if let Some(e) = (*(&raw mut MM))
            .iter_mut()
            .find(|e| e.live && e.ttbr0 == ttbr0)
        {
            e.brk = v;
        }
        crate::sync::irq_restore(flags);
    }
}

pub fn set_user_mmap_next(v: u64) {
    let ttbr0 = mm_current_ttbr0();
    let flags = crate::sync::irq_save();
    unsafe {
        if let Some(e) = (*(&raw mut MM))
            .iter_mut()
            .find(|e| e.live && e.ttbr0 == ttbr0)
        {
            e.mmap_next = v;
        }
        crate::sync::irq_restore(flags);
    }
}

/// Mark the running task as being inside a system call from EL0, and return
/// what the flag was, so it can be put back. Nested is possible: `execve`
/// reads a file through Linux while itself serving a user syscall.
pub fn set_user_syscall(on: bool) -> bool {
    unsafe {
        let was = TASKS[cur()].user_syscall;
        TASKS[cur()].user_syscall = on;
        was
    }
}

/// Record the syscall about to be forwarded, for the watchdog's wait graph.
pub fn set_last_syscall(nr: u64) {
    unsafe { TASKS[cur()].last_syscall = nr }
}

/// Record the running task's own name (`prctl(PR_SET_NAME)`), 15 bytes max.
pub fn set_comm(name: &[u8]) {
    unsafe {
        let t = &mut TASKS[cur()];
        t.comm = [0; 16];
        let n = name.len().min(15);
        t.comm[..n].copy_from_slice(&name[..n]);
    }
}

pub fn set_has_console(on: bool) {
    unsafe { TASKS[cur()].has_console = on }
}

pub fn has_console() -> bool {
    unsafe { TASKS[cur()].has_console }
}

/// Record that the running task speaks the NETHOS ABI. Called once, when
/// its program is loaded.
pub fn set_nethos_abi(on: bool) {
    unsafe { TASKS[cur()].nethos_abi = on }
}

/// Which ABI the running task speaks. Read on every system call, so it is
/// a field rather than anything that has to be looked up.
pub fn nethos_abi() -> bool {
    unsafe { TASKS[cur()].nethos_abi }
}

/// As `nethos_abi`, for a task that is not the running one -- fork, which
/// copies it to the child.
pub fn task_nethos_abi(id: usize) -> bool {
    unsafe { TASKS[id].nethos_abi }
}

pub fn set_task_nethos_abi(id: usize, on: bool) {
    unsafe { TASKS[id].nethos_abi = on }
}

pub fn in_user_syscall() -> bool {
    unsafe { TASKS[cur()].user_syscall }
}

/// Record who forked whom, and answer questions about it.
/// Mark a task as sharing its creator's address space, and say where to
/// clear a thread id when it goes.
pub fn set_thread(id: usize, clear_child_tid: u64) {
    unsafe {
        TASKS[id].shares_mm = true;
        TASKS[id].clear_child_tid = clear_child_tid;
    }
}

/// Mark a task as sharing an address space it does not own, with a parent
/// waiting for it to stop doing so. The vfork half of `set_thread`.
pub fn set_vfork_child(id: usize, done: usize) {
    unsafe {
        TASKS[id].shares_mm = true;
        TASKS[id].vfork_done = done;
    }
}

/// Whether the running task is in an address space belonging to somebody
/// else -- a thread of it, or a vfork child that has not exec'd yet.
pub fn shares_mm_current() -> bool {
    unsafe { TASKS[cur()].shares_mm }
}

/// The running task now owns its address space: `execve` gave it one of its
/// own, so its exit must tear that one down.
pub fn own_mm_current() {
    unsafe { TASKS[cur()].shares_mm = false }
}

/// Take the semaphore a vfork parent is waiting on, leaving none behind: the
/// promise is kept once, whether by `execve` or by exit.
pub fn take_vfork_done() -> usize {
    unsafe {
        let v = TASKS[cur()].vfork_done;
        TASKS[cur()].vfork_done = 0;
        v
    }
}

pub fn clear_child_tid() -> u64 {
    unsafe { TASKS[cur()].clear_child_tid }
}

pub fn set_parent(child: usize, parent: usize) {
    unsafe { TASKS[child].parent = parent }
}

/// Whether a child belongs to the waiter -- to it, or to a thread of it.
///
/// `wait` is a property of a process, and a process here is several tasks
/// sharing one address space. glibc's `posix_spawn` is called from whichever
/// thread wants a child and GLib's child watch reaps from the main loop's,
/// so matching only the task that made the call answers ECHILD to a parent
/// whose child is sitting right there:
///
///   waitpid(pid:61) failed: No child processes (10)
///
/// and WebKit tore down a web process it believed had vanished.
fn child_of(child: usize, waiter: usize) -> bool {
    unsafe {
        let p = TASKS[child].parent;
        if p == waiter {
            return true;
        }
        // Same address space, and not the "no address space" of a task that
        // never had one -- that would make every kernel task everyone's
        // parent.
        let space = TASKS[waiter].ttbr0;
        space != 0
            && space != crate::paging::kernel_address_space()
            && TASKS[p].ttbr0 == space
            && TASKS[p].state != State::Unused
    }
}

/// A child of `parent` that has finished, if there is one.
pub fn finished_child(parent: usize, want_pid: i64) -> Option<usize> {
    let flags = RUNQUEUE.lock();
    let found = unsafe { finished_child_locked(parent, want_pid) };
    unsafe { RUNQUEUE.unlock(flags) };
    found
}

/// `finished_child` for a caller that already holds the runqueue lock.
///
/// The only correct form inside `block_on_cond`'s `ready`, which runs with
/// the lock held -- re-locking there is a self-deadlock, not a race.
///
/// # Safety
/// Caller holds RUNQUEUE.
pub unsafe fn finished_child_locked(parent: usize, want_pid: i64) -> Option<usize> {
    (0..MAX_TASKS).find(|&i| {
        child_of(i, parent)
            && TASKS[i].state == State::Finished
            && (want_pid <= 0 || TASKS[i].linux_pid == want_pid)
    })
}

/// Whether `parent` still has a child that might yet finish. `wait4` blocks
/// only while this is true; with no children at all it has to return ECHILD
/// rather than wait for one that is never coming.
pub fn has_live_child(parent: usize, want_pid: i64) -> bool {
    unsafe {
        (0..MAX_TASKS).any(|i| {
            child_of(i, parent)
                && TASKS[i].state != State::Unused
                && (want_pid <= 0 || TASKS[i].linux_pid == want_pid)
        })
    }
}

/// The task's own memory layout, for a child that inherits its parent's.
///
/// For `fork` this copies the layout to the child's new root; for a thread
/// (same root) there is nothing to do -- the layout is already shared, and
/// writing the creation-time copy back would rewind allocations siblings
/// made since. Takes the new root and whether the child shares memory.
pub fn set_user_memory_full(new_ttbr0: u64, shares_mm: bool) {
    if !shares_mm {
        mm_copy_to(new_ttbr0);
    }
}

/// Drop the calling task's address-space layout, for `execve`'s old root.
/// The new root gets its entry from `set_user_memory`.
pub fn drop_user_memory(ttbr0: u64) {
    mm_drop(ttbr0);
}

/// Shared mappings of the running task, copied out. `fork` hands them to the
/// child, which records them as pool references over its private copies.
pub fn shared_maps() -> alloc::vec::Vec<crate::user::SharedMap> {
    unsafe {
        TASKS[cur()].shared[..TASKS[cur()].nshared]
            .iter()
            .copied()
            .collect::<alloc::vec::Vec<_>>()
    }
}

/// Install the shared-mapping list for the running task. The child's entries
/// arrive in `Forked`; threads share the creator's list by copying it too --
/// same address space, same mappings.
pub fn set_shared_maps(maps: &[crate::user::SharedMap]) {
    unsafe {
        let n = maps.len().min(TASKS[cur()].shared.len());
        TASKS[cur()].shared[..n].copy_from_slice(&maps[..n]);
        TASKS[cur()].nshared = n;
    }
}

/// Record one shared mapping for the running task. False when the table is
/// full: the mapping is already in place, and the caller must undo it.
pub fn add_shared_map(m: crate::user::SharedMap) -> bool {
    unsafe {
        if TASKS[cur()].nshared >= TASKS[cur()].shared.len() {
            return false;
        }
        let n = TASKS[cur()].nshared;
        TASKS[cur()].shared[n] = m;
        TASKS[cur()].nshared = n + 1;
        true
    }
}

/// The shared-mapping record covering `at..at+len` whole, if one record
/// does. `mremap` moves a mapping, not a stack of fragments, and needs to
/// look before it removes: the replacement is mapped first so the region's
/// reference count never dips while both ranges exist.
pub fn shared_map_covering(at: u64, len: u64) -> Option<crate::user::SharedMap> {
    unsafe {
        (0..TASKS[cur()].nshared)
            .map(|i| TASKS[cur()].shared[i])
            .find(|m| m.start <= at && at + len <= m.start + m.len)
    }
}

/// Drop every shared-mapping record overlapping `start..start+len`, handing
/// back what was dropped so the caller can release the pool references.
pub fn remove_shared_maps(start: u64, len: u64) -> alloc::vec::Vec<crate::user::SharedMap> {
    use alloc::vec::Vec;
    unsafe {
        let mut out = Vec::new();
        let mut i = 0;
        while i < TASKS[cur()].nshared {
            let m = TASKS[cur()].shared[i];
            if m.start < start + len && start < m.start + m.len {
                out.push(m);
                TASKS[cur()].shared.copy_within(i + 1..TASKS[cur()].nshared, i);
                TASKS[cur()].nshared -= 1;
            } else {
                i += 1;
            }
        }
        out
    }
}

/// All shared mappings of the running task. `exit` releases every one.
pub fn take_shared_maps() -> alloc::vec::Vec<crate::user::SharedMap> {
    use alloc::vec::Vec;
    unsafe {
        let mut out = Vec::with_capacity(TASKS[cur()].nshared);
        for i in 0..TASKS[cur()].nshared {
            out.push(TASKS[cur()].shared[i]);
        }
        TASKS[cur()].nshared = 0;
        out
    }
}

/// Signal helpers: actions, masks and pending bits live in the task table
/// because they are per task (or per process, for actions), and `signal.rs`
/// owns the semantics while this owns the storage.
pub fn signal_action(id: usize, sig: u64) -> crate::signal::Action {
    unsafe { TASKS[id].sig_actions.get((sig - 1) as usize).copied().unwrap_or(crate::signal::Action::default()) }
}

pub fn set_signal_action(id: usize, sig: u64, act: crate::signal::Action) {
    unsafe {
        if sig >= 1 && sig <= 64 {
            // A vfork child shares its creator's page tables but not its
            // signal state: Linux copies `signal_struct` there, and every
            // `posix_spawn` resets the child's handled signals to default
            // before exec. Propagating those writes by address space would
            // wipe the parent's handlers through the shared root -- measured
            // as the UI process losing SIGCHLD and SIGUSR1 when a web
            // process was spawned. After exec the child has a root of its
            // own, so only this pre-exec window needs the exception.
            if TASKS[id].vfork_done != 0 {
                TASKS[id].sig_actions[(sig - 1) as usize] = act;
                return;
            }
            // Threads share dispositions: writing one task's table writes
            // every task sharing its address space. The table is small and
            // sharing is by `ttbr0`, which threads share and processes do
            // not -- fork copies, threads alias.
            let root = TASKS[id].ttbr0;
            for t in TASKS.iter_mut() {
                if t.state != State::Unused && t.ttbr0 == root {
                    t.sig_actions[(sig - 1) as usize] = act;
                }
            }
        }
    }
}

/// `execve`'s half of signal state: Linux resets every handled signal to
/// default and leaves ignored ones ignored. The mask and the altstack are
/// the task's own and survive.
pub fn reset_signal_handlers(id: usize) {
    unsafe {
        if id >= MAX_TASKS {
            return;
        }
        for act in TASKS[id].sig_actions.iter_mut() {
            if act.handler != crate::signal::SIG_DFL && act.handler != crate::signal::SIG_IGN {
                *act = crate::signal::Action::default();
            }
        }
    }
}

pub fn signal_mask(id: usize) -> u64 {
    unsafe { TASKS[id].sig_mask }
}

pub fn set_signal_mask(id: usize, mask: u64) {
    unsafe { TASKS[id].sig_mask = mask }
}

pub fn signal_pending(id: usize) -> u64 {
    unsafe { TASKS[id].sig_pending }
}

pub fn add_signal_pending(id: usize, bits: u64) {
    unsafe { TASKS[id].sig_pending |= bits }
}

pub fn clear_signal_pending(id: usize, bits: u64) {
    unsafe { TASKS[id].sig_pending &= !bits }
}

/// Record where a signal came from, for the `siginfo` in the frame.
pub fn set_signal_source(id: usize, sig: u64, code: i32, sender: i32) {
    unsafe {
        if sig >= 1 && sig <= 64 {
            TASKS[id].sig_code[(sig - 1) as usize] = code;
            TASKS[id].sig_sender[(sig - 1) as usize] = sender;
        }
    }
}

pub fn signal_code(id: usize, sig: u64) -> i32 {
    unsafe {
        TASKS[id]
            .sig_code
            .get((sig.wrapping_sub(1)) as usize)
            .copied()
            .unwrap_or(0)
    }
}

pub fn signal_sender(id: usize, sig: u64) -> i32 {
    unsafe {
        TASKS[id]
            .sig_sender
            .get((sig.wrapping_sub(1)) as usize)
            .copied()
            .unwrap_or(0)
    }
}

/// The task with Linux pid `pid`, if it is still alive.
pub fn task_with_linux_pid(pid: i64) -> Option<usize> {
    unsafe {
        (0..MAX_TASKS).find(|&i| {
            TASKS[i].linux_pid == pid
                && (TASKS[i].state == State::Running
                    || TASKS[i].state == State::Ready
                    || TASKS[i].state == State::Blocked)
        })
    }
}

/// `(linux_pid, parent, live)` for `kill(0/-1)` group delivery.
pub fn task_identity(id: usize) -> (i64, usize, bool) {
    unsafe {
        let t = &TASKS[id];
        let live = t.state == State::Running || t.state == State::Ready || t.state == State::Blocked;
        (t.linux_pid, t.parent, live)
    }
}

pub fn task_parent(id: usize) -> usize {
    unsafe { TASKS[id].parent }
}

pub fn signal_altstack(id: usize) -> (u64, u64, bool) {
    unsafe { (TASKS[id].sig_alt_base, TASKS[id].sig_alt_size, TASKS[id].sig_alt_in_use) }
}

pub fn set_signal_altstack(id: usize, base: u64, size: u64) {
    unsafe {
        TASKS[id].sig_alt_base = base;
        TASKS[id].sig_alt_size = size;
        TASKS[id].sig_alt_in_use = false;
    }
}

pub fn set_signal_altstack_in_use(id: usize, used: bool) {
    unsafe { TASKS[id].sig_alt_in_use = used }
}

pub fn set_signal_return(id: usize, mask: u64, used_alt: bool) {
    unsafe {
        TASKS[id].sig_return_mask = mask;
        TASKS[id].sig_return_alt = used_alt;
    }
}

pub fn signal_return(id: usize) -> (u64, bool) {
    unsafe { (TASKS[id].sig_return_mask, TASKS[id].sig_return_alt) }
}

/// Copy signal state to a forked child: dispositions, mask, altstack. The
/// child starts with nothing pending -- signals sent to the parent before
/// the fork are the parent's, not the child's.
pub fn inherit_signal_state(child: usize, parent: usize) {
    unsafe {
        TASKS[child].sig_actions = TASKS[parent].sig_actions;
        TASKS[child].sig_mask = TASKS[parent].sig_mask;
        TASKS[child].sig_alt_base = TASKS[parent].sig_alt_base;
        TASKS[child].sig_alt_size = TASKS[parent].sig_alt_size;
        TASKS[child].sig_alt_in_use = false;
        TASKS[child].sig_pending = 0;
        TASKS[child].sig_code = [0; 64];
        TASKS[child].sig_sender = [0; 64];
        TASKS[child].sig_return_mask = 0;
        TASKS[child].sig_return_alt = false;
    }
}

/// Install carried signal state on the running task. `fork` copies the
/// parent's dispositions into `Forked` before spawning; the child installs
/// them here, on itself, once it exists.
pub fn install_signal_state(
    actions: [crate::signal::Action; 64],
    mask: u64,
    alt_base: u64,
    alt_size: u64,
) {
    unsafe {
        TASKS[cur()].sig_actions = actions;
        TASKS[cur()].sig_mask = mask;
        TASKS[cur()].sig_alt_base = alt_base;
        TASKS[cur()].sig_alt_size = alt_size;
        TASKS[cur()].sig_alt_in_use = false;
        TASKS[cur()].sig_pending = 0;
        TASKS[cur()].sig_code = [0; 64];
        TASKS[cur()].sig_sender = [0; 64];
        TASKS[cur()].sig_return_mask = 0;
        TASKS[cur()].sig_return_alt = false;
    }
}

/// Dispositions of the running task, for carrying across `fork`.
pub fn signal_actions(id: usize) -> [crate::signal::Action; 64] {
    unsafe { TASKS[id].sig_actions }
}

/// The top of a task's kernel stack.
///
/// `execve` needs it: it never returns through the exception frame it was
/// called on, and every frame below that one is dead the moment the new
/// program starts. Without resetting the stack pointer those frames are
/// leaked for the life of the task, which a shell would notice.
pub fn kernel_stack_top() -> usize {
    kernel_stack_top_of(cur())
}

pub fn kernel_stack_top_of(id: usize) -> usize {
    unsafe { TASKS[id].stack + STACK_PAGES * PAGE }
}

/// Publish task `id`'s kernel stack as the one traps and syscalls land on.
///
/// Two places need it and they are not the same mechanism: the TSS's RSP0 is
/// what the CPU loads on a ring-3 interrupt, and `kernel_sp_scratch` is what
/// the `syscall` stub loads by hand, because `syscall` does not switch stacks
/// at all -- it leaves RSP pointing at the user stack.
///
/// `kernel_sp_scratch` was previously never written, so it held zero: the
/// first `syscall` from ring 3 would have loaded RSP = 0 and faulted on the
/// first push, with the fault itself unable to push. Nothing had reached ring
/// 3 yet, so it had never been observed.
#[cfg(target_arch = "x86_64")]
pub fn set_kernel_stack_for(id: usize) {
    // Task 0 is the boot path, which was already running on the stack the
    // linker gave it; its `stack` field is zero because nothing allocated one.
    // Computing a top from that zero would name a page near address 0 and send
    // the next trap there, so the boot stack top is recorded separately.
    let top = unsafe {
        if TASKS[id].stack == 0 {
            BOOT_STACK_TOP
        } else {
            kernel_stack_top_of(id) as u64
        }
    };
    if top == 0 {
        return;
    }
    unsafe {
        crate::tss_x86::set_kernel_stack(top);
        let cpu = crate::arch::cpu_id().min(MAX_CPUS - 1);
        core::ptr::write_volatile(&raw mut kernel_sp_scratch[cpu], top);
    }
}

/// The top of the stack the boot task is already running on; see
/// `set_kernel_stack_for`.
#[cfg(target_arch = "x86_64")]
static mut BOOT_STACK_TOP: u64 = 0;

/// Record the boot stack top, and publish it as the current trap stack.
///
/// # Safety
/// Call once, early, from the boot task, with the top of its stack.
#[cfg(target_arch = "x86_64")]
pub unsafe fn set_boot_stack_top(top: u64) {
    BOOT_STACK_TOP = top;
    set_kernel_stack_for(cur());
}

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    /// The per-CPU scratch slots the syscall stub loads RSP from; see
    /// user-x86_64.s. Indexed by (APIC ID & 7).
    static mut kernel_sp_scratch: [u64; MAX_CPUS];
}

pub fn bind_linux_pid(pid: i64) {
    unsafe {
        TASKS[cur()].linux_pid = pid;
    }
}
pub fn linux_pid(id: usize) -> i64 {
    unsafe { TASKS[id].linux_pid }
}
pub fn set_exit_status(status: i32) {
    unsafe {
        TASKS[cur()].exit_status = status;
    }
}
pub fn exit_status(id: usize) -> i32 {
    unsafe { TASKS[id].exit_status }
}

/// Sleep until `ticks` timer ticks have passed.
///
/// A wait, not a spin: the caller is off the run queue for the duration and
/// the CPU stops when nothing else wants it. `while ticks() < until
/// { yield_now() }` is the same wait written as a busy loop, and on an
/// emulated machine that is a host core at 100% for as long as it lasts --
/// the watchdog's one-second pause alone kept this one from ever idling.
///
/// The tick handler wakes sleepers whose deadline has passed, so the
/// resolution is one tick (HZ) and never shorter than asked for.
// Regression-test infrastructure: the forced mid-switch window and the
// wake-target override. Both are inert unless `mid_switch_wake_test` arms
// them; nothing in ordinary scheduler operation writes either.
pub static TEST_WAKE_TARGET: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);

pub static TEST_MW_ARMED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
pub static TEST_MW_ENTERED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
pub static TEST_MW_RELEASE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
pub static TEST_MW_TASK: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);
pub static TEST_MW_RUN_CYC: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

pub fn sleep_ticks(ticks: u64) {
    let until = crate::timer::ticks() + ticks;
    loop {
        let flags = crate::sync::irq_save();
        if crate::timer::ticks() >= until {
            unsafe { crate::sync::irq_restore(flags) };
            return;
        }

        // Predicate: the millisecond clock has reached `until`.
        // Protected by: the monotonic TICKS counter, read here under the
        //   runqueue lock so the read and the decision to sleep cannot
        //   straddle a tick.
        // Published: `wake_sleepers` runs from the BSP tick, and under
        //   RUNQUEUE clears `wake_at` and makes this task Ready.
        // Ordering: a tick either already bumped TICKS before we take the
        //   lock (we see the deadline passed) or it takes the lock after we
        //   are marked Blocked and wakes us. Bumping TICKS is atomic and
        //   happens before `wake_sleepers` acquires the lock.
        // Why the re-check closes it: previously a tick landing between the
        //   `wake_at` store above and `block_on` marking us Blocked found a
        //   task that was still Running, refused the wake, and this task
        //   then slept through its own deadline until the *next* tick. The
        //   re-check turns that lost tick into a visible deadline.
        let was_blocked = block_on_cond(
            flags,
            SLEEP_WAIT,
            // Deadline recorded before blocking, under the runqueue lock, so
            // a tick can never find a half-armed sleeper.
            || unsafe {
                (*(&raw mut TASKS))[cur()].wake_at = until;
            },
            || crate::timer::ticks() >= until,
        );
        // `block_on` returns without having switched when nothing else is
        // Ready, and this task is then still marked Blocked with its own
        // deadline pending. Halting is exactly right here: the caller
        // asked to be away for a while, the tick is what ends that, and
        // this is the state an idle machine should be sitting in.
        // `blocked == true` has two outcomes: the task was switched away and
        // has now been woken and physically resumed (state == Running), or
        // `schedule()` found nothing else runnable and the task is still
        // Blocked on this CPU. Only the second case needs the halt; halting
        // after a completed wake inserts an extra tick and exposes the
        // resumed task to preemption. `blocked()` reads the state under
        // RUNQUEUE.
        if was_blocked && unsafe { blocked(cur()) } {
            halt_until_interrupt();
        }
    }
}

/// What a sleeper is blocked on, so only the tick may wake it.
const SLEEP_WAIT: u32 = u32::MAX - 1;

/// Wake every sleeper whose deadline has passed. Called from the timer
/// interrupt, which is the only thing that can make one runnable again.
pub fn wake_sleepers() {
    let now = crate::timer::ticks();
    // Under the runqueue lock: `make_ready` publishes state that a pick on
    // another CPU is concurrently reading. This runs from the BSP's tick,
    // which is why it may take the lock at all -- an interrupt handler that
    // only ever reads its own CPU's state could skip it.
    let flags = RUNQUEUE.lock();
    unsafe {
        for i in 0..MAX_TASKS {
            // Snapshot the whole predicate from one iteration: the wake
            // decision is a pure function of these three fields, and reading
            // them once keeps the check and the wake on the same values.
            let (state, what, wake_at) = (
                TASKS[i].state,
                TASKS[i].waiting_on,
                TASKS[i].wake_at,
            );
            if state == State::Blocked
                && what == SLEEP_WAIT
                && wake_at != 0
                && now >= wake_at
            {
                TASKS[i].wake_at = 0;
                make_ready(i, SLEEP_WAIT);
            }
        }
        RUNQUEUE.unlock(flags);
    }
}

/// Give up the rest of this slice, and stop the CPU if that is all there is.
///
/// The spin-wait loops (the watchdog's second, the boot task's waits) are
/// written as `while not_yet() { yield_now() }`, which on a machine with
/// nothing else to run is a tight loop around a clock read. That is a host
/// core at 100% for a guest that is doing nothing at all -- the guest looks
/// busy because it never stops asking whether it is time yet.
///
/// So when the yield found nobody, halt until the next interrupt. The timer
/// runs at HZ, so this wakes at least every tick and no wait gets longer;
/// it just stops burning the CPU between them. `wfi` returns on a pending
/// interrupt regardless of the mask, and the caller's loop re-tests its own
/// condition afterwards, so this cannot swallow a wakeup.
pub fn yield_now() {
    if !schedule() {
        halt_until_interrupt();
    }
}

/// Which task is running. The identity a semaphore or a join needs.
/// An identity for a task that is unique for the life of the machine.
///
/// The slot in the low bits, the generation above it. Linux is given this
/// rather than the slot: it keeps `cpu.owner` for as long as a thread lives
/// and compares it against `thread_self()`, so an identity that a later
/// thread can repeat makes it take a lock it never acquired or wait for a
/// thread that has gone.
pub fn uid(id: usize) -> usize {
    debug_assert!(id < MAX_TASKS);
    unsafe { ((TASKS[id].generation as usize) << 16) | id }
}

/// The slot a `uid` names, if that incarnation is still the one in it.
/// `None` once the task has exited, which is the whole point: a stale id
/// must resolve to nothing rather than to whoever took the slot next.
pub fn from_uid(uid: usize) -> Option<usize> {
    let id = uid & 0xffff;
    if id >= MAX_TASKS {
        return None;
    }
    let generation = (uid >> 16) as u64;
    unsafe { (TASKS[id].generation == generation).then_some(id) }
}

pub fn current_uid() -> usize {
    uid(current_id())
}

pub fn current_id() -> usize {
    cpu_current_unchecked()
}

pub fn current_cpu() -> usize {
    cpu_index(crate::arch::cpu_id())
}

pub fn set_current_cpu(_cpu: usize) {}

/// Stop running until somebody calls `wake`.
///
/// The caller must have arranged to be woken *before* calling this, and with
/// interrupts masked across both, or the wake can land in the gap between
/// deciding to sleep and sleeping -- the classic lost-wakeup, and the reason
/// this takes the saved interrupt state rather than masking it itself.
pub fn block(flags: u64) {
    block_on(flags, 0)
}

/// Block unless `ready` says otherwise, with the check and the blocking
/// decision made under the runqueue lock.
///
/// This is the fix for the classic lost wakeup. A primitive arms its own
/// waiter state, then calls here. Between that arming and the sleep, a waker
/// on another CPU can post and try to wake this task -- and `up` makes a
/// task Ready only while its state is `Blocked`, so the wake is refused
/// (the task is still `Running`) and then this function would set `Blocked`
/// anyway, sleeping through a wake that already happened. Both sides of a
/// ping-pong pair can do that to each other and end up blocked at once.
///
/// Re-checking under the same lock the waker takes closes it: if the post
/// landed, `ready` is true here and the task does not sleep; if the post
/// lands after, the waker finds the task `Blocked` and wakes it. The waker
/// increments its counter *before* taking the lock, so a post that is
/// already in flight but not yet visible at the first check is still either
/// seen here or woken after.
pub fn block_on_cond(
    flags: u64,
    what: u32,
    prep: impl FnOnce(),
    ready: impl Fn() -> bool,
) -> bool {
    let g = RUNQUEUE.lock();
    // The caller's wait state -- the deadline for a sleeper -- is published
    // under the same lock the waker takes, so no reader can observe a
    // half-armed waiter.
    prep();
    if ready() {
        unsafe { RUNQUEUE.unlock(g) };
        unsafe { crate::sync::irq_restore(flags) };
        return false;
    }
    unsafe {
        (*(&raw mut TASKS))[cur()].waiting_on = what;
        (*(&raw mut TASKS))[cur()].state = State::Blocked;
        (*(&raw mut TASKS))[cur()].ready_at = 0;
        account(cur(), crate::arch::cycles());
        crate::schedtrace::record(
            0,
            crate::schedtrace::Event::Block,
            cur(),
            cur(),
            effective_class_of(cur()).as_u8(),
            what as u64,
            0,
        );
        RUNQUEUE.unlock(g);
    }
    // Mid-switch wake regression hook: the task is Blocked, still owns its
    // execution context, and the physical switch has not happened. IRQs are
    // still masked here (the caller's flags are restored below), so the
    // window cannot be preempted out from under the test. Inert unless the
    // test arms it for its own helper.
    if TEST_MW_ARMED.load(Ordering::Acquire)
        && cur() == TEST_MW_TASK.load(Ordering::Relaxed)
    {
        TEST_MW_ENTERED.store(true, Ordering::Release);
        let mut spins = 0u64;
        while !TEST_MW_RELEASE.load(Ordering::Acquire) && spins < 500_000_000 {
            core::hint::spin_loop();
            spins += 1;
        }
    }
    unsafe { crate::sync::irq_restore(flags) };
    schedule();
    true
}

/// Block, recording what is being waited for so a deadlock names itself.
pub fn block_on(flags: u64, what: u32) {
    // Marking Blocked under the runqueue lock, so a wake on another CPU
    // either sees Blocked (and makes it Ready) or arrives before and lets
    // this loop re-test its condition. The old unlocked store raced
    // `wake_on` and lost wakes outright.
    let g = RUNQUEUE.lock();
    unsafe {
        (*(&raw mut TASKS))[cur()].waiting_on = what;
        (*(&raw mut TASKS))[cur()].state = State::Blocked;
        (*(&raw mut TASKS))[cur()].ready_at = 0;
        account(cur(), crate::arch::cycles());
        crate::schedtrace::record(
            0,
            crate::schedtrace::Event::Block,
            cur(),
            cur(),
            effective_class_of(cur()).as_u8(),
            what as u64,
            0,
        );
        RUNQUEUE.unlock(g);
        // Interrupts come back on before the switch: the task is already
        // marked blocked, so a wake arriving now sets it Ready again rather
        // than being lost.
        crate::sync::irq_restore(flags);
        // One switch, and the caller re-tests its own condition in a loop.
        //
        // Not a halt-until-really-elsewhere loop here, tempting as that is:
        // this runs from every blocking primitive in the kernel, including
        // LKL's semaphores during early boot and inside sections that block
        // with interrupts deliberately masked. Halting in those contexts
        // waits for an interrupt that cannot arrive -- measured as a boot
        // that stops at the first mount. The idle halt lives in `yield_now`
        // and `sleep_ticks`, where the context is known to be safe.
        schedule();
    }
}

/// A wake aimed at a task that was not blocked.
///
/// This is the shape of every lost-wakeup bug: somebody released a resource,
/// the release was recorded, and the waiter never learned of it. Counted
/// rather than assumed absent, because a lost wakeup does not fail where it
/// happens -- it fails later, as a machine where everything is waiting.
pub static mut LOST_WAKEUPS: u64 = 0;

/// Make a task runnable only if it is waiting on `what`.
///
/// `wake` does not look at what a task is blocked on, which is safe only
/// while every wait is nk's own. It is not: a task inside a system call is
/// often blocked in one of Linux's semaphores, and waking it there returns
/// it from `sem_down` without the semaphore having been given to it. LKL
/// then retries -- its `lkl_cpu_get` loops -- and increments `cpu.sleepers`
/// again on the way round, so the count of sleepers and the semaphore's own
/// count drift apart until a real `sem_up` is absorbed by a sleeper that was
/// never asleep. What comes out is a machine where everything is waiting,
/// with no single lost wakeup to point at.
///
/// The token is the rule: 0 is nk's own generic wait and may be interrupted
/// by anyone, and anything else names a primitive that only its owner may
/// wake a task out of.
/// Wake a task and consume its wait registration in the same critical
/// section as the Ready transition.
///
/// `clear` runs with RUNQUEUE held, and only when the wake actually
/// happened. The woken task cannot run -- and therefore cannot register a
/// new wait -- until the lock is released, which removes the ABA window in
/// which an older wake's cleanup removed a *new* registration for the same
/// slot.
pub fn wake_on_locked(id: usize, what: u32, clear: impl FnOnce()) -> bool {
    let flags = RUNQUEUE.lock();
    let woke = unsafe {
        if id >= MAX_TASKS {
            false
        } else if TASKS[id].state == State::Blocked {
            if TASKS[id].waiting_on == what {
                make_ready(id, what);
                clear();
                true
            } else {
                false
            }
        } else if matches!(TASKS[id].state, State::Ready | State::Running) {
            false
        } else {
            LOST_WAKEUPS += 1;
            false
        }
    };
    unsafe { RUNQUEUE.unlock(flags) };
    woke
}

pub fn wake_on(id: usize, what: u32) -> bool {
    let flags = RUNQUEUE.lock();
    let woke = unsafe {
        if id >= MAX_TASKS {
            false
        } else if TASKS[id].state == State::Blocked {
            if TASKS[id].waiting_on == what {
                make_ready(id, what);
                true
            } else {
                false
            }
        } else if matches!(TASKS[id].state, State::Ready | State::Running) {
            false
        } else {
            LOST_WAKEUPS += 1;
            false
        }
    };
    unsafe { RUNQUEUE.unlock(flags) };
    woke
}

/// Make a blocked task runnable, whatever it is waiting on.
///
/// Use `wake_on` unless you are the primitive the task is blocked in. See
/// the note there for what waking a task out of somebody else's semaphore
/// does.
pub fn wake(id: usize) {
    let flags = RUNQUEUE.lock();
    unsafe {
        if id < MAX_TASKS {
            match TASKS[id].state {
                State::Blocked => make_ready(id, TASKS[id].waiting_on),
                State::Ready | State::Running => {}
                _ => LOST_WAKEUPS += 1,
            }
        }
        unsafe { RUNQUEUE.unlock(flags) };
    }
}

/// Set a blocked task runnable, and decide whether it should run *now*.
///
/// Every wakeup goes through here, which is what makes the responsiveness
/// policy actually apply: a task made Ready by a hand-written
/// `state = Ready` would be correct but would wait for the tick, and a
/// compositor woken by a keystroke waiting out a compiler's 20 ms slice is
/// precisely the feeling this scheduler exists to avoid.
///
/// The decision itself is `schedpolicy::should_preempt`, and the switch is
/// not done here -- this runs from interrupt handlers and from inside
/// locks. It sets `NEED_RESCHED`, and the switch happens at the next safe
/// point: the end of the interrupt, or the next `schedule()`.
///
/// # Safety
/// Caller holds the scheduler's interrupt mask.
unsafe fn make_ready(id: usize, what: u32) {
    // Finished is terminal for scheduling: a task whose entry has returned
    // must never re-enter the run queue.
    if (*(&raw const TASKS))[id].state == State::Finished {
        return;
    }
    let placed = place_vruntime();
    let now = crate::arch::cycles();
    let cpu = cpu_index(crate::arch::cpu_id());
    let current = (*(&raw const CPUS))[cpu].current.load(Ordering::Acquire);
    TASKS[id].ready_at = now;
    TASKS[id].wake_pending = true;
    TASKS[id].state = State::Ready;
    // A task that has been asleep for a long time would otherwise come back
    // with a vruntime far behind the running set and monopolise the CPU
    // catching up. Only ever moved forward: a task that slept briefly keeps
    // its place, which is what makes short blocking cheap.
    if TASKS[id].vruntime < placed {
        TASKS[id].vruntime = placed;
    }

    crate::schedtrace::record(
        0,
        crate::schedtrace::Event::Wakeup,
        id,
        current,
        crate::schedpolicy::effective_class(TASKS[id].class, TASKS[id].interactive).as_u8(),
        what as u64,
        0,
    );

    if id == current {
        return;
    }

    let waker = crate::schedpolicy::effective_class(TASKS[id].class, TASKS[id].interactive);
    let running = crate::schedpolicy::effective_class(
        TASKS[current].class,
        TASKS[current].interactive,
    );
    let running_for = if TASKS[current].ran_at == 0 {
        0
    } else {
        crate::timer::cycles_to_ns(now.wrapping_sub(TASKS[current].ran_at))
    };

    let should = crate::schedpolicy::should_preempt(waker, running, running_for);
    if should {
        crate::schedtrace::record(
            0,
            crate::schedtrace::Event::Preempt,
            id,
            current,
            waker.as_u8(),
            1,
            running_for,
        );
        let target = pick_target_cpu(id);
        if target == cpu {
            set_need_resched_on(cpu);
        } else {
            send_reschedule_ipi(target);
        }
    }
}

/// Set when something has happened that should change which task is running
/// -- a wakeup that outranks the current task, or a slice that has run out.
///
/// A flag rather than a switch, because the places that discover this are
/// the worst possible places to switch from: inside an interrupt handler,
/// inside a lock, inside LKL. The switch happens at the next point that is
/// known to be safe, which is what §28 of the brief means by doing minimal
/// work in interrupt context.
pub static NEED_RESCHED: AtomicBool = AtomicBool::new(false);

/// Whether a reschedule is wanted, clearing the request.
pub fn take_need_resched() -> bool {
    let cpu = cpu_index(crate::arch::cpu_id());
    take_need_resched_on(cpu)
}

/// Ask for a reschedule at the next safe point.
pub fn set_need_resched() {
    let cpu = cpu_index(crate::arch::cpu_id());
    set_need_resched_on(cpu)
}

pub fn take_need_resched_cpu(cpu: usize) -> bool {
    take_need_resched_on(cpu)
}

pub fn set_need_resched_cpu(cpu: usize) {
    set_need_resched_on(cpu)
}

/// Whether the running task has used its slice.
///
/// Called from the tick. A task that has had its share should be rotated
/// even with nothing higher-class waiting, which is what keeps equals
/// sharing fairly rather than the first one running until it blocks.
pub fn slice_expired() -> bool {
    unsafe {
        let tasks = &*(&raw const TASKS);
        let cur = cpu_current_unchecked();
        if tasks[cur].ran_at == 0 {
            return false;
        }
        let class = crate::schedpolicy::effective_class(tasks[cur].class, tasks[cur].interactive);
        let ran = crate::timer::cycles_to_ns(crate::arch::cycles().wrapping_sub(tasks[cur].ran_at));
        ran >= crate::schedpolicy::slice_ns(class)
    }
}

/// Whether the task is parked rather than ready or running.
///
/// For the watchdog's LKL strand check: a blocked host thread cannot be
/// inside Linux executing anything, so if it still owns LKL's CPU while the
/// machine makes no syscalls, nobody else ever can either.
pub fn blocked(id: usize) -> bool {
    if id >= MAX_TASKS {
        return false;
    }
    let flags = RUNQUEUE.lock();
    let b = unsafe { (*(&raw const TASKS))[id].state == State::Blocked };
    unsafe { RUNQUEUE.unlock(flags) };
    b
}

/// Whether the slot holds a task that can never run again -- finished but
/// not yet reaped, or already reclaimed. A `uid` naming one of these is dead
/// for ownership purposes even while `from_uid` still resolves it.
pub fn finished(id: usize) -> bool {
    if id >= MAX_TASKS {
        return true;
    }
    let flags = RUNQUEUE.lock();
    let done = unsafe {
        matches!(
                (*(&raw const TASKS))[id].state,
                State::Finished | State::Unused
            )
    };
    unsafe { RUNQUEUE.unlock(flags) };
    done
}

/// A task's state as a number, for diagnostics: 0 unused, 1 ready,
/// 2 running, 3 blocked, 4 finished, 5 stale uid, 6 out of range.
pub fn task_state_of(uid: usize) -> u8 {
    let id = uid & 0xffff;
    if id >= MAX_TASKS {
        return 6;
    }
    let g = RUNQUEUE.lock();
    let r = unsafe {
        if TASKS[id].generation != (uid >> 16) as u64 {
            5
        } else {
            match TASKS[id].state {
                State::Unused => 0,
                State::Ready => 1,
                State::Running => 2,
                State::Blocked => 3,
                State::Finished => 4,
            }
        }
    };
    unsafe { RUNQUEUE.unlock(g) };
    r
}

/// The base address of a task's kernel stack, for diagnostics. Two tasks
/// sharing a base is the allocator handing one stack to both.
pub fn task_stack_of(id: usize) -> usize {
    if id >= MAX_TASKS { return 0; }
    let g = RUNQUEUE.lock();
    let v = unsafe { (*(&raw const TASKS))[id].stack };
    unsafe { RUNQUEUE.unlock(g) };
    v
}

/// Raw per-slot scheduler facts for diagnostics: state, what it is blocked
/// on, whether a CPU owns it, and its home CPU. Takes a slot, not a uid --
/// `task_state_of` masks a uid and reports 5 for a slot, which reads as
/// "exited" and hides a live task.
pub fn task_probe(id: usize) -> (u8, u32, bool, u8) {
    if id >= MAX_TASKS {
        return (6, 0, false, 0);
    }
    let g = RUNQUEUE.lock();
    let r = unsafe {
        let t = &(*(&raw const TASKS))[id];
        let st = match t.state {
            State::Unused => 0,
            State::Ready => 1,
            State::Running => 2,
            State::Blocked => 3,
            State::Finished => 4,
        };
        (st, t.waiting_on, t.is_owned(), t.home_cpu)
    };
    unsafe { RUNQUEUE.unlock(g) };
    r
}

/// Wait for a task to finish.
///
/// Blocked, not spinning. The boot task joins init and init is the whole
/// session, so a spin here is a task that is Ready for hours: it costs a
/// context switch per pass, and -- because `schedule` only stops the CPU
/// when nothing at all is runnable -- it is on its own enough to keep a
/// host core at 100% for an idle machine. `task_exit` wakes the joiner.
pub fn join(id: usize) {
    loop {
        let flags = crate::sync::irq_save();
        // The test and the registration happen together under the runqueue
        // lock, which is the same lock `task_exit` takes to publish Finished
        // and to take the joiner field. An exit landing between this test and
        // the block below therefore either turns the test true right here, or
        // finds a registered joiner when it takes the field and wakes it --
        // there is no gap left for it to fall through.
        //
        // One slot, so one sleeping joiner: a second would overwrite the
        // first and that first would never be woken. Nothing in nk joins the
        // same task twice, and if anything ever does it yields instead of
        // sleeping -- slower, and still correct.
        let (done, alone) = {
            let g = RUNQUEUE.lock();
            let state = unsafe { (*(&raw const TASKS))[id].state };
            let done = state == State::Finished || state == State::Unused;
            let alone = if done {
                false
            } else {
                unsafe {
                    let tasks = &mut *(&raw mut TASKS);
                    if tasks[id].joiner == 0 || tasks[id].joiner == cur() as u32 + 1 {
                        tasks[id].joiner = cur() as u32 + 1;
                        true
                    } else {
                        false
                    }
                }
            };
            unsafe { RUNQUEUE.unlock(g) };
            (done, alone)
        };
        if done {
            unsafe { crate::sync::irq_restore(flags) };
            return;
        }
        if alone {
            // Predicate: the target task is Finished (or its slot already
            //   reclaimed to Unused).
            // Protected by: the target's `state` under the runqueue lock.
            //   `task_exit` now writes Finished *inside* that lock and takes
            //   the joiner field in the same critical section, so there is
            //   no unlocked window on either side.
            // Published: `task_exit` holds the lock across "state = Finished
            //   and read joiner", releases, then calls
            //   `wake_on(joiner-1, JOIN_WAIT)`.
            // Why the re-check closes it: an exit that lands between the
            //   test at the top of this loop and this block is caught here
            //   -- Finished makes the predicate true, so the joiner never
            //   sleeps. An exit after the joiner is Blocked is delivered by
            //   `wake_on`, which finds it Blocked and wakes it. The
            //   unconditional `block_on` would have set Blocked after the
            //   wake was refused, and the joiner would have slept forever on
            //   a task that had already exited.
            block_on_cond(flags, JOIN_WAIT, || {}, || {
                let t = unsafe { &(*(&raw const TASKS))[id] };
                t.state == State::Finished || t.state == State::Unused
            });
        } else {
            unsafe { crate::sync::irq_restore(flags) };
            yield_now();
        }
    }
}

/// What a joiner is blocked on, so only `task_exit` may wake it. See
/// `wake_on`: a bare `wake` would return a task from somebody else's
/// semaphore without the semaphore having been given to it.
const JOIN_WAIT: u32 = u32::MAX;

/// End the current task from inside it. What `task_exit` does when an entry
/// function returns, exposed for a caller that decides to stop early.
pub fn exit_current() -> ! {
    task_exit()
}

/// Called by task_start when a task's entry function returns.
#[no_mangle]
pub extern "C" fn task_exit() -> ! {
    crate::hostops::tls_cleanup();
    let flags = crate::sync::irq_save();
    // Publish Finished under the runqueue lock, and read the joiner and the
    // parent in the same critical section. `join` registers `joiner` under
    // this lock too, and `wait4` re-checks the child's state under it, so
    // both facts are ordered against their readers. A plain store ordered
    // only by the wake's lock acquire would still be a data race on a
    // non-atomic field, which is undefined behaviour even when it happens to
    // work.
    let (joiner, parent) = {
        let g = RUNQUEUE.lock();
        let published = unsafe {
            TASKS[cur()].state = State::Finished;
            account(cur(), crate::arch::cycles());
            // Whoever is in `join` on this task is blocked, not spinning,
            // and this is the only thing that will ever wake it.
            (core::mem::take(&mut TASKS[cur()].joiner), TASKS[cur()].parent)
        };
        unsafe { RUNQUEUE.unlock(g) };
        published
    };
    // Woken after the publication and outside the lock: `wake_on` takes the
    // lock itself. A joiner or parent that acquires it now sees Finished and
    // does not sleep; one already Blocked is woken here.
    if joiner != 0 {
        wake_on(joiner as usize - 1, JOIN_WAIT);
    }
    // And a parent blocked in `wait4`. The wake has to be here rather than
    // where the status is recorded: `sys_exit` posts SIGCHLD and wakes the
    // parent while this task is still Running, so a parent that reaches its
    // "has the child finished?" test in between sees "no", blocks, and is
    // never woken again -- the whole machine then waits on a child that
    // exited seconds ago. Marked Finished first, woken second, so the test
    // the parent re-runs after waking can only ever see the state that made
    // the wake true.
    if parent != 0 && parent != cur() {
        wake_on(parent, 0);
    }
    // The stack is deliberately not freed here: it is the stack this code is
    // standing on. It is reclaimed by whoever reaps the slot, which nothing
    // does yet -- a real leak, bounded by MAX_TASKS, and noted rather than
    // hidden.
    // Interrupts back on before parking. This runs with the mask held from
    // `irq_save` above, and the loop below never returns -- a finished task
    // that sat here masked would stop the timer, and on a single CPU that is
    // a sleep nobody can ever wake: the finished task spins in `schedule`
    // while the task that could reap it is parked and the clock that would
    // wake it is off. It is invisible on SMP, where some other CPU runs the
    // timer, which is exactly how it survived this long.
    unsafe { crate::sync::irq_restore(flags) };
    loop {
        // `yield_now` halts when there is genuinely nothing else to run,
        // rather than spinning a dead task at full tilt.
        yield_now();
    }
}

/// Let the timer interrupt preempt. Off until the tasks exist, because a tick
/// arriving mid-`spawn` would switch to a task whose frame is half written.
pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

pub fn preempt_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// Put a task in a scheduling class.
///
/// The only way a class is ever set: nothing infers one from a name, and no
/// program may ask for one directly. What a task claims about itself is not
/// evidence -- a program that could declare itself interactive would, and
/// every program would, and the class would mean nothing. Interactivity is
/// earned through behaviour (see `schedpolicy`); this is for the cases where
/// nk genuinely knows, such as its own background threads.
pub fn set_class(id: usize, class: crate::schedpolicy::Class) {
    if id >= MAX_TASKS {
        return;
    }
    let flags = crate::sync::irq_save();
    unsafe {
        let placed = place_vruntime();
        let tasks = &mut *(&raw mut TASKS);
        if tasks[id].class != class {
            crate::schedtrace::record(
                0,
                crate::schedtrace::Event::PriorityChange,
                id,
                cur(),
                class.as_u8(),
                tasks[id].class.as_u8() as u64,
                0,
            );
            tasks[id].class = class;
            // The vruntime was accumulated at the old class's weight and is
            // not comparable at the new one. Re-placing against the run
            // queue keeps it meaningful without giving the task either a
            // windfall or a penalty for the reclassification.
            tasks[id].vruntime = placed;
        }
        crate::sync::irq_restore(flags);
    }
}

pub fn set_current_class(class: crate::schedpolicy::Class) {
    set_class(current_id(), class);
}

/// How many times a task has been given the CPU. The crude measure of how
/// much of the machine it got, and enough to tell whether class ordering is
/// doing anything.
pub fn slices_of(id: usize) -> u64 {
    if id >= MAX_TASKS {
        return 0;
    }
    unsafe { (*(&raw const TASKS))[id].slices }
}

pub fn task_name(id: usize) -> &'static str {
    if id >= MAX_TASKS {
        return "?";
    }
    unsafe { (*(&raw const TASKS))[id].name }
}

/// Total CPU time a task has consumed, in nanoseconds.
pub fn runtime_of(id: usize) -> u64 {
    if id >= MAX_TASKS {
        return 0;
    }
    unsafe { (*(&raw const TASKS))[id].ran_ns }
}

pub fn class(id: usize) -> crate::schedpolicy::Class {
    if id >= MAX_TASKS {
        return crate::schedpolicy::Class::Normal;
    }
    unsafe { (*(&raw const TASKS))[id].class }
}

/// A summary of how the scheduler is behaving, for the diagnostics path.
pub fn report_classes() {
    unsafe {
        let tasks = &*(&raw const TASKS);
        let mut runnable = 0;
        let mut blocked = 0;
        let mut per_class = [0usize; 5];
        for t in tasks.iter() {
            match t.state {
                State::Ready | State::Running => {
                    runnable += 1;
                    let c = crate::schedpolicy::effective_class(t.class, t.interactive);
                    per_class[c.as_u8() as usize] += 1;
                }
                State::Blocked => blocked += 1,
                _ => {}
            }
        }
        println!(
            "  runqueue: {} runnable, {} blocked  (rt {}, interactive {}, normal {}, throughput {}, background {})",
            runnable, blocked, per_class[4], per_class[3], per_class[2], per_class[1], per_class[0]
        );
        crate::schedtrace::report_latency();
    }
}

pub fn current() -> usize {
    cur()
}

pub fn report() {
    fn comm_of(t: &Task) -> &str {
        let end = t.comm.iter().position(|b| *b == 0).unwrap_or(t.comm.len());
        core::str::from_utf8(&t.comm[..end]).unwrap_or("")
    }
    unsafe {
        let tasks = &*(&raw const TASKS);
        for (i, t) in tasks.iter().enumerate() {
            if t.state != State::Unused {
                let s = match t.state {
                    State::Running => "running",
                    State::Ready => "ready",
                    State::Blocked => "blocked",
                    State::Finished => "finished",
                    State::Unused => "",
                };
                if t.state == State::Blocked && t.waiting_on != 0 {
                    println!(
                        "          [{}] {:<10} {:<9} {} slices  pid {}  uid {:#x}  waiting on {} syscall {}  comm {}",
                        i, t.name, s, t.slices, t.linux_pid, uid(i), t.waiting_on, t.last_syscall, comm_of(t)
                    );
                } else if t.last_syscall != 0 {
                    println!(
                        "          [{}] {:<10} {:<9} {} slices  pid {}  uid {:#x}  syscall {}  comm {}",
                        i, t.name, s, t.slices, t.linux_pid, uid(i), t.last_syscall, comm_of(t)
                    );
                } else {
                    println!(
                        "          [{}] {:<10} {:<9} {} slices  pid {}  comm {}",
                        i, t.name, s, t.slices, t.linux_pid, comm_of(t)
                    );
                }
            }
        }
    }
}

/// Reap only a joined process task. Its stack is now inactive and its TLS
/// destructors have already released the Linux task.
pub fn reap_process(id: usize) {
    let flags = crate::sync::irq_save();
    unsafe {
        assert_ne!(id, cur());
        let task = &TASKS[id];
        assert!(task.state == State::Finished && task.linux_pid > 1);
        // A thread's address space belongs to the threads it shared it with,
        // and they are still running in it.
        if !task.shares_mm {
            crate::paging::destroy_user_address_space(task.ttbr0);
            mm_drop(task.ttbr0);
        }
        return_stack(task.stack);
        frames::free(task.shadow as *mut u8);
        TASKS[id].state = State::Unused;
        crate::sync::irq_restore(flags);
    }
}

pub fn record_user_irq() {
    unsafe {
        TASKS[cur()].user_irqs += 1;
    }
}
pub fn user_irqs(id: usize) -> u64 {
    unsafe { TASKS[id].user_irqs }
}

/// Whether the init process the watchdog waits for is still running.
///
/// Set once, after `user::launch`, from the boot task; read by the watchdog.
/// The watchdog used to stop the machine after four one-second rounds no
/// matter what init was doing, which guillotined every test longer than four
/// seconds mid-syscall and read exactly like a kernel wedge: the process
/// READY-spinning, timers advancing, one ppoll in flight with its hrtimer
/// still queued in the future. Now it waits for init and only backstops a
/// boot that never finishes.
static mut INIT_TASK: usize = usize::MAX;

pub fn set_init_task(id: usize) {
    unsafe {
        INIT_TASK = id;
    }
}

/// True once init has exited (or was never launched): the watchdog may stop
/// the machine. A task that finished but has not been reaped yet still counts
/// as done -- reaping is the boot task's job, not the watchdog's.
pub fn init_done() -> bool {
    unsafe {
        let id = INIT_TASK;
        if id == usize::MAX {
            return false;
        }
        matches!(
            (*(&raw const TASKS))[id].state,
            State::Finished | State::Unused
        )
    }
}

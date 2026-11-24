//! The local APIC: per-CPU interrupts, timers, and IPIs on x86-64.
//!
//! The 8259 PIC that `interrupts_x86.rs` drives is a machine-wide relic: one
//! routing, one delivery, no notion of CPUs. An IPI -- one CPU interrupting
//! another -- is not expressible through it at all. The LAPIC is per-CPU
//! hardware at a fixed MMIO base, and it is the only way to wake a specific
//! CPU, which is what `send_reschedule_ipi` needs and what AP startup uses
//! for INIT and STARTUP.
//!
//! Only the subset SMP needs: ID, spurious vector, EOI, ICR for IPIs, LVT
//! timer for per-CPU ticks, and INIT/STARTUP sequencing. The I/O APIC (device
//! routing) is not here -- devices stay on the PIC until a later stage.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

static BASE: AtomicUsize = AtomicUsize::new(0xfee0_0000);
static APIC_IDS: [AtomicU32; 8] = [const { AtomicU32::new(u32::MAX) }; 8];
static CPU_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn base() -> usize { BASE.load(Ordering::Relaxed) }

/// Install the firmware topology before the scheduler or secondary CPUs run.
/// Slot zero is always the BSP, regardless of its physical APIC ID.
pub fn configure_topology(ids: &[u32]) {
    let bsp = id();
    APIC_IDS[0].store(bsp, Ordering::Relaxed);
    let mut count = 1;
    for &apic in ids {
        if apic == bsp || apic > 255 || count == APIC_IDS.len() { continue; }
        if APIC_IDS[..count].iter().any(|v| v.load(Ordering::Relaxed) == apic) { continue; }
        APIC_IDS[count].store(apic, Ordering::Relaxed);
        count += 1;
    }
    CPU_COUNT.store(count, Ordering::Release);
}
pub fn cpu_count() -> usize { CPU_COUNT.load(Ordering::Acquire).max(1) }
pub fn apic_id(cpu: usize) -> Option<u32> {
    (cpu < CPU_COUNT.load(Ordering::Acquire)).then(|| APIC_IDS[cpu].load(Ordering::Relaxed))
}
pub fn logical_id() -> usize {
    let count = CPU_COUNT.load(Ordering::Acquire);
    // Before discovery only the BSP executes; no physical ID is a task index.
    if count == 0 { return 0; }
    let hardware = id();
    APIC_IDS[..count].iter().position(|v| v.load(Ordering::Relaxed) == hardware)
        .expect("CPU absent from the firmware topology")
}

unsafe fn apic_msr() -> u64 {
    let lo: u32; let hi: u32;
    core::arch::asm!("rdmsr", in("ecx") 0x1bu32, out("eax") lo, out("edx") hi, options(nostack));
    ((hi as u64)<<32) | lo as u64
}
/// Learn the current base before any APIC reads. x2APIC needs a separate
/// MSR interface; fail explicitly rather than accessing disabled xAPIC MMIO.
pub unsafe fn discover_base() {
    let msr = apic_msr();
    assert!(msr & (1<<10) == 0, "x2APIC mode is not supported yet");
    let address = msr & 0x000f_ffff_ffff_f000;
    assert!(address != 0 && address < 1<<32, "LAPIC outside boot identity map");
    BASE.store(address as usize, Ordering::Relaxed);
}
pub unsafe fn set_base(address: u64) {
    assert!(address != 0 && address & 4095 == 0 && address < 1<<32);
    BASE.store(address as usize, Ordering::Relaxed);
    let old = apic_msr();
    let msr = address | (old & 0xfff) | (1<<11);
    core::arch::asm!("wrmsr", in("ecx") 0x1bu32, in("eax") msr as u32,
        in("edx") (msr>>32) as u32, options(nostack));
}

const ID: usize = 0x020;
const EOI: usize = 0x0b0;
const SPURIOUS: usize = 0x0f0;
const ICR_LO: usize = 0x300;
const ICR_HI: usize = 0x310;
const LVT_TIMER: usize = 0x320;
const TIMER_INIT: usize = 0x380;
const TIMER_COUNT: usize = 0x390;
const TIMER_DIV: usize = 0x3e0;

/// Vector for reschedule IPIs. Above the PIC remap (32..47) and below the
/// syscall/sysret range, in the permanently-reserved local-vector space.
pub const IPI_VECTOR: u8 = 0xf0;
/// Spurious vector: bit 8 enables the LAPIC, low 8 are the vector.
const SPURIOUS_VECTOR: u32 = 0x1ff;

#[inline]
unsafe fn read(off: usize) -> u32 {
    core::ptr::read_volatile((base() + off) as *const u32)
}

#[inline]
unsafe fn write(off: usize, v: u32) {
    core::ptr::write_volatile((base() + off) as *mut u32, v);
}

/// This CPU's APIC ID. The same value `arch::cpu_id` derives from CPUID,
/// read back from the hardware that delivers to it.
pub fn id() -> u32 {
    unsafe { read(ID) >> 24 }
}

/// Enable the LAPIC with the spurious vector. The spurious vector must be
/// non-zero and have bit 8 set or the LAPIC stays disabled and every ICR
/// write is silently dropped -- measured as IPIs that never arrive.
pub unsafe fn init() {
    set_base(base() as u64);
    write(SPURIOUS, SPURIOUS_VECTOR);
}

/// Signal end-of-interrupt to the LAPIC. Called for the IPI vector;
/// the PIC path keeps its own EOI.
pub unsafe fn eoi() {
    write(EOI, 0);
}

/// Send a fixed IPI to one CPU.
///
/// Delivery waits for the ICR idle bit rather than assuming it: back-to-back
/// IPIs without the wait collapse into one on real hardware, which presents
/// as a lost reschedule and a CPU that sleeps through its wakeup. Bounded --
/// a LAPIC that never goes idle is broken hardware, and hanging the sender
/// forever turns one dead CPU into a dead machine.
pub unsafe fn send_ipi(apic_id: u32, vector: u8) {
    let mut spins = 0u32;
    while read(ICR_LO) & (1 << 12) != 0 {
        spins += 1;
        if spins > 1_000_000 {
            crate::println!("  lapic:  ICR never idle, dropping IPI to {}", apic_id);
            return;
        }
        core::hint::spin_loop();
    }
    write(ICR_HI, apic_id << 24);
    // Fixed delivery (bits 8..10 = 000), physical destination (bit 11 = 0).
    write(ICR_LO, vector as u32);
}

/// INIT followed by two STARTUP IPIs: the standard AP bring-up sequence.
///
/// INIT takes the AP out of halt; STARTUP points it at a page whose first
/// bytes it executes in 16-bit real mode. Two STARTUPs because the first can
/// be lost on real silicon (and costs nothing on QEMU). Each followed by a
/// delay the BSP spends doing nothing: 10ms after INIT, 200us between
/// STARTUPs, per the Intel manual's timing. Implemented as TSC-deadline
/// spins so the delays hold without a calibrated timer.
pub unsafe fn start_ap(apic_id: u32, startup_page: u8) {
    crate::println!("  lapic:  init ->{}", apic_id);
    send_init(apic_id);
    crate::println!("  lapic:  init done");
    deadline_spin(10_000_000);
    crate::println!("  lapic:  startup 1");
    send_startup(apic_id, startup_page);
    deadline_spin(200_000);
    crate::println!("  lapic:  startup 2");
    send_startup(apic_id, startup_page);
    deadline_spin(200_000);
    crate::println!("  lapic:  startup done");
}

unsafe fn send_init(apic_id: u32) {
    // INIT assert, edge-triggered, and nothing else.
    //
    // The old "INIT assert / INIT deassert" pair is a P6-era requirement.
    // On anything modern -- and on QEMU -- the deassert is a *level*
    // INIT with Level clear, which is the encoding for "hold this CPU in
    // reset". Sending it leaves the target parked at the reset vector with
    // HLT set, and the STARTUP that follows is ignored: no fault, no
    // triple fault, just a CPU that never appears. Measured exactly that
    // way, with `-d int,cpu_reset` showing CPU 1 at EIP=0000fff0 HLT=1 and
    // not one exception in the whole log.
    //
    // Bit 14 is Level (1 = assert); bit 15 (Trigger Mode) stays clear for
    // the edge-triggered form the manual specifies for this sequence.
    wait_idle();
    write(ICR_HI, apic_id << 24);
    write(ICR_LO, (0b101 << 8) | (1 << 14));
}

unsafe fn send_startup(apic_id: u32, page: u8) {
    wait_idle();
    write(ICR_HI, apic_id << 24);
    // STARTUP (bits 8..10 = 110), vector = the page number.
    write(ICR_LO, (0b110 << 8) | page as u32);
}

unsafe fn wait_idle() {
    let mut spins = 0u32;
    while read(ICR_LO) & (1 << 12) != 0 {
        spins += 1;
        if spins > 1_000_000 {
            return;
        }
        core::hint::spin_loop();
    }
}

fn deadline_spin(ns: u64) {
    let hz = crate::timer::cycle_hz();
    if hz == 0 {
        // Uncalibrated: fall back to a bounded spin that is roughly right on
        // a 1GHz TSC and harmless elsewhere -- AP startup timing is generous.
        for _ in 0..ns / 100 {
            core::hint::spin_loop();
        }
        return;
    }
    let start = crate::arch::cycles();
    let delta = ns.saturating_mul(hz) / 1_000_000_000;
    // Bounded by iteration count as well as by the deadline: if the TSC
    // stops advancing -- which it does under some hypervisors when the CPU
    // is descheduled -- the deadline is never reached and this becomes the
    // hang it exists to avoid.
    let mut spins = 0u64;
    while crate::arch::cycles().wrapping_sub(start) < delta {
        spins += 1;
        if spins > 1_000_000_000 {
            return;
        }
        core::hint::spin_loop();
    }
}

/// Program the LAPIC timer for periodic ticks at HZ.
///
/// Divide-by-16 then an initial count derived from the calibrated TSC rate:
/// the LAPIC timer on QEMU ticks at the bus rate, which is the TSC rate
/// divided by a model-specific factor. Measured rather than assumed -- see
/// `calibrate_timer`, called once on the BSP before APs start.
static BUS_HZ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn bus_hz() -> u64 {
    BUS_HZ.load(core::sync::atomic::Ordering::Relaxed)
}

/// Measure the LAPIC timer against the calibrated TSC over ~10ms.
pub unsafe fn calibrate_timer() {
    // Divide by 16 (bits 0..1 = 11, bit 2 = 0).
    write(TIMER_DIV, 0x3);
    // One-shot, and *masked* (bit 16). Calibration only reads the count
    // register; it must not deliver. An earlier version left the vector at
    // 0x20 unmasked, which is the PIC's IRQ0 vector -- the LAPIC then
    // raised the timer vector with no LAPIC EOI behind it, and the boot
    // stopped dead the moment interrupts were unmasked.
    write(LVT_TIMER, 1 << 16);
    write(TIMER_INIT, u32::MAX);
    let hz = crate::timer::cycle_hz();
    let start_tsc = crate::arch::cycles();
    let start_lapic = read(TIMER_COUNT);
    let wait = hz / 100;
    while crate::arch::cycles().wrapping_sub(start_tsc) < wait {
        core::hint::spin_loop();
    }
    let end_lapic = read(TIMER_COUNT);
    let ticks = (start_lapic - end_lapic) as u64;
    // ticks in 10ms -> per second, times the divide-by-16.
    let bus = ticks * 100 * 16;
    BUS_HZ.store(bus, core::sync::atomic::Ordering::Relaxed);
    crate::println!("  lapic:  timer bus {} MHz", bus / 1_000_000);
}

/// Stop this CPU's periodic tick (mask the LVT timer). Used on a dedicated
/// core while its one task runs: nothing else may run there, so a tick can
/// only interrupt it. Wakes still arrive by IPI.
pub unsafe fn mask_timer() {
    write(LVT_TIMER, read(LVT_TIMER) | 1 << 16);
}

/// Undo `mask_timer`.
pub unsafe fn unmask_timer() {
    write(LVT_TIMER, read(LVT_TIMER) & !(1 << 16));
}

/// One-shot timer mode at `vector`, unarmed: TSC-deadline (LVT bits 18:17 =
/// 10b, armed through IA32_TSC_DEADLINE) or count mode (armed by writing the
/// initial count). Stops any periodic tick. See hrtimer.rs.
pub unsafe fn oneshot_mode(vector: u8, tsc_deadline: bool) {
    write(TIMER_INIT, 0);
    write(TIMER_DIV, 0x3);
    let mode = if tsc_deadline { 0b10 << 17 } else { 0 };
    write(LVT_TIMER, mode | vector as u32);
}

/// The count-mode timer's current count.
pub unsafe fn current_count() -> u32 {
    read(TIMER_COUNT)
}

/// Arm count-mode one-shot (0 disarms).
pub unsafe fn set_initial_count(count: u32) {
    write(TIMER_INIT, count);
}

/// Start periodic LAPIC timer ticks on this CPU at `vector`.
pub unsafe fn start_timer(vector: u8) {
    let bus = bus_hz();
    if bus == 0 {
        return;
    }
    write(TIMER_DIV, 0x3);
    let count = (bus / 16 / crate::timer::HZ) as u32;
    // Periodic (bit 17), masked-not (bit 16 clear), vector low 8.
    write(LVT_TIMER, 0x2_0000 | vector as u32);
    write(TIMER_INIT, count);
}

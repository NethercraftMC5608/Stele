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

/// Default physical base. Matches QEMU's q35 and every real PC; relocated
/// only by firmware doing something exotic, which nk does not support yet.
pub const BASE: usize = 0xfee0_0000;

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
    core::ptr::read_volatile((BASE + off) as *const u32)
}

#[inline]
unsafe fn write(off: usize, v: u32) {
    core::ptr::write_volatile((BASE + off) as *mut u32, v);
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

//! The virtual timer, and the tick everything else is paced by.
//!
//! **CNTV_*_EL0, not CNTP_*.** The device tree lists four timer interrupts --
//! secure physical, non-secure physical, virtual, hypervisor -- and the
//! physical one at index 1 looks like the obvious choice for a kernel running
//! at EL1. It is not, and this cost a real debugging session:
//!
//! Under a hypervisor, EL2 belongs to the hypervisor and the physical timer
//! belongs with it. Apple's Hypervisor.framework does not set
//! CNTHCTL_EL2.EL1PCEN for its guests, so `msr CNTP_TVAL_EL0` from EL1 traps
//! -- and it does not trap as a recognisable system-register access. It
//! arrives as a synchronous exception with EC 0 ("unknown reason"), which says
//! nothing at all about what happened. It was identified by disassembling
//! around ELR, not by reading the syndrome.
//!
//! The virtual timer works in both worlds: bare metal at EL1 with CNTVOFF
//! zero, and under any hypervisor, which is precisely what it exists for.
//! Linux picks it for the same reason whenever it does not own EL2. boot.s
//! still zeroes CNTVOFF_EL2 on the way down from EL2, which is what keeps the
//! virtual counter equal to the physical one when nk *is* the hypervisor's
//! level.
//!
//! A count-down, not a deadline. CNTV_TVAL_EL0 is written with an interval and
//! the timer fires when it reaches zero, which is one register per tick and no
//! arithmetic that can drift into the past -- the failure mode of writing an
//! absolute CNTP_CVAL that has already been passed is a timer that never
//! fires again.

use crate::println;
use core::sync::atomic::{AtomicU64, Ordering};

/// Ticks per second. 100 is a compromise nobody argues with: fine enough that
/// a round-robin slice is imperceptible, coarse enough that the handler is
/// not a measurable fraction of the machine.
pub const HZ: u64 = 100;

/// LAPIC timer vector on x86 APs. Distinct from the PIC remap (32..47) and
/// the reschedule IPI (0xf0); handled by the same tick path.
#[cfg(target_arch = "x86_64")]
pub const TIMER_VECTOR: u8 = 0xef;

static TICKS: AtomicU64 = AtomicU64::new(0);
static mut INTERVAL: u64 = 0;
static mut INTID: u32 = 27;

fn interval() -> u64 {
    // Through a raw pointer rather than a reference: a shared reference to a
    // mutable static is undefined behaviour the moment anything writes it, and
    // rearm() does, from interrupt context.
    unsafe { core::ptr::read(&raw const INTERVAL) }
}

#[cfg(target_arch = "aarch64")]
fn frequency() -> u64 {
    let f: u64;
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack)) };
    f
}

/// The 8253/8254 PIT counts down from a divisor at a fixed 1.193182 MHz --
/// one twelfth of the original PC's 14.31818 MHz colour-burst crystal, which
/// is why it is not a round number. It is architectural on every PC and every
/// VM, unlike the TSC and the local APIC timer, whose rates have to be
/// measured against something else before they can be used.
#[cfg(target_arch = "x86_64")]
const PIT_HZ: u64 = 1_193_182;

#[cfg(target_arch = "x86_64")]
fn frequency() -> u64 {
    PIT_HZ
}

/// # Safety
/// The GIC must be up; `intid` is the PPI the device tree named.
#[cfg(target_arch = "aarch64")]
pub unsafe fn init(intid: u32) {
    let freq = frequency();
    assert!(freq != 0, "CNTFRQ_EL0 is zero -- firmware never set the timer frequency");
    INTID = intid;
    INTERVAL = freq / HZ;
    crate::gic::enable_ppi(intid);
    rearm();
    // ENABLE, with IMASK clear. Both matter: an enabled timer with IMASK set
    // counts down and fires nothing.
    core::arch::asm!("msr cntv_ctl_el0, {}", in(reg) 1u64, options(nomem, nostack));
    println!(
        "  timer:  {} Hz on PPI {} ({} MHz counter, {} ticks/interval)",
        HZ,
        intid,
        freq / 1_000_000,
        interval()
    );
}

/// Arm and enable this CPU's virtual timer, for a secondary.
///
/// `init` cannot be reused: it calls `gic::enable_ppi`, which targets the
/// *boot* CPU's cached redistributor base, so on an AP it would enable the
/// timer interrupt on the wrong CPU. The private interrupt is enabled by
/// `gic::init_secondary` instead, and what is left here is the timer itself,
/// which is per-CPU state (CNTV_TVAL/CNTV_CTL) and has to be programmed on
/// the CPU that will take the interrupt.
///
/// # Safety
/// The GIC's redistributor for this CPU must already be awake and have the
/// timer PPI enabled.
#[cfg(target_arch = "aarch64")]
pub unsafe fn start_this_cpu() {
    rearm();
    // ENABLE with IMASK clear, exactly as `init` does.
    core::arch::asm!("msr cntv_ctl_el0, {}", in(reg) 1u64, options(nomem, nostack));
}

/// Program the PIT to interrupt at `HZ` on IRQ 0.
///
/// # Safety
/// The PIC must be remapped and the IDT loaded, or the first tick lands on a
/// vector that is still the BIOS's.
#[cfg(target_arch = "x86_64")]
pub unsafe fn init(_intid: u32) {
    // Channel 0, access mode lobyte/hibyte, mode 2 (rate generator), binary.
    // Mode 2 reloads itself, so the tick is periodic and `rearm` has nothing
    // to do -- unlike the aarch64 countdown timer, which must be rewritten
    // after every fire.
    const PIT_CH0: u16 = 0x40;
    const PIT_CMD: u16 = 0x43;

    let divisor = frequency() / HZ;
    // The divisor is a 16-bit register: 0 means 65536, and anything above that
    // cannot be expressed, which would silently give a much faster tick.
    assert!(divisor > 0 && divisor <= 0x10000,
        "PIT divisor out of range -- HZ is too low for a 16-bit divisor");
    let divisor = if divisor == 0x10000 { 0u16 } else { divisor as u16 };

    core::arch::asm!(
        "out dx, al",
        in("dx") PIT_CMD, in("al") 0x34u8,
        options(nomem, nostack, preserves_flags),
    );
    core::arch::asm!(
        "out dx, al",
        in("dx") PIT_CH0, in("al") (divisor & 0xff) as u8,
        options(nomem, nostack, preserves_flags),
    );
    core::arch::asm!(
        "out dx, al",
        in("dx") PIT_CH0, in("al") (divisor >> 8) as u8,
        options(nomem, nostack, preserves_flags),
    );

    INTID = 0;
    INTERVAL = divisor as u64;
    println!(
        "  timer:  {} Hz on IRQ 0 (PIT, {} Hz base, divisor {})",
        HZ, PIT_HZ, if divisor == 0 { 65536 } else { divisor as u32 },
    );
}

/// # Safety
/// Called from the interrupt handler, or from init.
#[cfg(target_arch = "aarch64")]
pub unsafe fn rearm() {
    core::arch::asm!("msr cntv_tval_el0, {}", in(reg) INTERVAL, options(nomem, nostack));
}

/// Nothing to do on x86-64: the PIT runs in mode 2 (rate generator), which
/// reloads its divisor automatically. This is empty because the hardware is
/// periodic, not because rearming was forgotten.
#[cfg(target_arch = "x86_64")]
pub unsafe fn rearm() {}

pub fn intid() -> u32 {
    unsafe { INTID }
}

/// Cycles per second for `arch::cycles`, or 0 before it is known.
///
/// On aarch64 this is `CNTFRQ_EL0` and exact. On x86-64 the TSC's rate is not
/// architecturally discoverable -- it is whatever the part runs at, and under
/// a VM whatever the host's is -- so it has to be measured against a clock
/// whose rate *is* known. See `calibrate`.
static CYCLE_HZ: AtomicU64 = AtomicU64::new(0);

/// Measure the cycle counter against a clock of known frequency.
///
/// aarch64 needs no measuring: `CNTFRQ_EL0` is the answer.
#[cfg(target_arch = "aarch64")]
pub fn calibrate() {
    CYCLE_HZ.store(frequency(), Ordering::Relaxed);
    println!("  clock:  {} MHz counter (CNTFRQ_EL0)", frequency() / 1_000_000);
}

/// Measure the TSC against PIT channel 2.
///
/// Channel 2 rather than channel 0, which is the system tick and must not be
/// reprogrammed: channel 2 is the speaker gate, it is free, and reading its
/// counter back is the standard way to do this on a PC. It runs at the same
/// known 1.193182 MHz, so a TSC delta across a known number of its ticks
/// gives the TSC rate directly.
///
/// One-shot (mode 0), counting down from a divisor chosen to take about
/// 10 ms -- long enough that the read overhead is noise, short enough not to
/// be felt at boot. Interrupts are masked across it because a tick landing in
/// the middle would be counted as elapsed TSC time that the PIT did not see,
/// which reports a TSC faster than it is.
#[cfg(target_arch = "x86_64")]
pub fn calibrate() {
    const PIT_CH2: u16 = 0x42;
    const PIT_CMD: u16 = 0x43;
    /// Port 0x61: bit 0 gates channel 2, bit 1 would connect it to the
    /// speaker. Setting only bit 0 counts without making a noise.
    const PORT_61: u16 = 0x61;

    // 10 ms worth of the 1.193182 MHz PIT, comfortably inside 16 bits.
    const SAMPLE: u16 = (PIT_HZ / 100) as u16;

    let flags = crate::sync::irq_save();
    let measured = unsafe {
        let mut gate: u8;
        core::arch::asm!("in al, dx", out("al") gate, in("dx") PORT_61,
            options(nomem, nostack, preserves_flags));
        // Gate on, speaker off.
        let on = (gate & 0xfc) | 1;
        core::arch::asm!("out dx, al", in("dx") PORT_61, in("al") on,
            options(nomem, nostack, preserves_flags));

        // Channel 2, lobyte/hibyte, mode 0 (interrupt on terminal count).
        core::arch::asm!("out dx, al", in("dx") PIT_CMD, in("al") 0xb0u8,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("out dx, al", in("dx") PIT_CH2, in("al") (SAMPLE & 0xff) as u8,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("out dx, al", in("dx") PIT_CH2, in("al") (SAMPLE >> 8) as u8,
            options(nomem, nostack, preserves_flags));

        let start = crate::arch::cycles();
        // Bit 5 of port 0x61 goes high when channel 2 reaches terminal count.
        // Bounded: a machine where this never rises (no PIT at all) must not
        // hang the boot, it must report that calibration failed.
        let mut spins = 0u64;
        loop {
            let mut status: u8;
            core::arch::asm!("in al, dx", out("al") status, in("dx") PORT_61,
                options(nomem, nostack, preserves_flags));
            if status & 0x20 != 0 {
                break;
            }
            spins += 1;
            if spins > 100_000_000 {
                break;
            }
        }
        let end = crate::arch::cycles();

        // Gate back off.
        core::arch::asm!("out dx, al", in("dx") PORT_61, in("al") gate & 0xfc,
            options(nomem, nostack, preserves_flags));

        if spins > 100_000_000 { 0 } else { end.wrapping_sub(start) }
    };
    unsafe { crate::sync::irq_restore(flags) };

    if measured == 0 {
        println!("  clock:  TSC calibration failed -- latency will be reported in ticks");
        return;
    }

    // measured cycles took SAMPLE / PIT_HZ seconds.
    let hz = (measured as u128 * PIT_HZ as u128 / SAMPLE as u128) as u64;
    CYCLE_HZ.store(hz, Ordering::Relaxed);
    println!("  clock:  TSC {} MHz (measured against PIT channel 2)", hz / 1_000_000);
}

pub fn cycle_hz() -> u64 {
    CYCLE_HZ.load(Ordering::Relaxed)
}

/// Convert a cycle count difference to nanoseconds.
///
/// Returns 0 before calibration, which is deliberate: a latency figure that
/// is silently in the wrong unit is worse than one that is obviously absent.
#[inline]
pub fn cycles_to_ns(cycles: u64) -> u64 {
    let hz = CYCLE_HZ.load(Ordering::Relaxed);
    if hz == 0 {
        return 0;
    }
    ((cycles as u128 * 1_000_000_000) / hz as u128) as u64
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

#[cfg(target_arch = "aarch64")]
pub fn time_ns() -> u64 {
    let ticks: u64;
    unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) ticks, options(nomem, nostack)) };
    let freq = frequency();
    if freq == 0 {
        return 0;
    }
    ((ticks as u128 * 1_000_000_000) / freq as u128) as u64
}

#[cfg(target_arch = "x86_64")]
pub fn time_ns() -> u64 {
    // Derived from the periodic tick count on x86, where the TSC frequency
    // varies by model and is not handed to us in an architectural register.
    // Resolution is one tick, so this cannot time anything shorter than the
    // tick period; a TSC calibrated against the PIT is the eventual fix.
    ticks().saturating_mul(1_000_000_000 / HZ)
}

pub fn on_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    // Whoever asked to be woken at this tick. Nothing else can make a
    // sleeper runnable, so this is what stops `sleep_ticks` being a spin.
    crate::sched::wake_sleepers();
    #[cfg(nk_lkl)]
    crate::futex::tick();
    // Sample LKL's CPU label at every tick: the strand is installed by a
    // single switch, and a one-second watchdog sees only the corpse. The
    // ring is dumped when a stall begins.
    #[cfg(nk_lkl)]
    crate::lkl_cpu_trace_sample();
}

/// Milliseconds since the timer started. Derived from the tick count rather
/// than read from the counter, so it moves only when interrupts are actually
/// being delivered -- which makes it useful evidence that they are.
pub fn ms() -> u64 {
    ticks() * 1000 / HZ
}

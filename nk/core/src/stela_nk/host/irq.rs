// SPDX-License-Identifier: GPL-2.0-or-later
//! A host device interrupt into a translated guest through the AIC model
//! (ROADMAP Phase 3, NK_STELA_IRQ_TEST).
//!
//! The device's state lives in nk, outside the board: on hardware, a Linux
//! driver's completion or an NVMe queue. nk raises its line from another CPU
//! (`HOST_LINE`) and kicks the dedicated core with an IPI (vector 0xed). The
//! board (`IrqDevices`) carries the line into `stela_soc::aic::Aic2`, the
//! M1/M2 AIC model, whose IRQ output becomes the guest PE's IRQ pin through
//! the engine's `Devices::lines`. Nothing here simulates the AIC: the guest
//! programs and claims it through its registers.
//!
//! The guest enables the AIC, unmasks input 32, writes the device's request
//! register and waits in WFI with IRQs unmasked. Its handler claims the event
//! (AIC2 masks the input as it does), acknowledges the device (whose line
//! drops), unmasks and counts. A PE waiting with no deadline makes `run`
//! return `Stop::Waiting` (`Devices::waits_on_host`); the core then halts
//! until the kick and runs the guest again.

use super::*;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use stela_contracts::bus::{Access, MmioDevice, Width};
use stela_contracts::interrupt::{AicInput, InterruptLatch};
use stela_engine::machine::{DeviceError, Devices, Lines};
use stela_soc::aic::Aic2;
use stela_soc_data::aic::{Aic2Config, AIC2_IRQ_CFG, AIC2_TIMESTAMPS};

/// Vector of the IPI that wakes a dedicated core for host device work
/// (handled in `interrupts_x86`).
const KICK_VECTOR: u8 = 0xed;

// Guest physical fixtures, outside every nk mapping (as the UART probe's).
const AIC: u64 = 0x10_0001_0000;
const DOOR: u64 = 0x10_0002_0000;
const INPUT: AicInput = AicInput::new(0, 32);
const EVENT_IRQ: u64 = 1 << 16;
const GUEST_CPU: usize = 3;
const HELPER_CPU: usize = 2;
/// How long after the guest's request the host raises the line.
/// Past the engine's own poll before it returns `Stop::Waiting`, so the core
/// really halts and the kick is what ends it.
const RAISE_AFTER_NS: u64 = 5_000_000;

/// The host device's line level, set by nk and cleared by the guest's ack.
static HOST_LINE: AtomicBool = AtomicBool::new(false);
/// `time_ns` of the guest's request (0: none yet), the raise and the ack.
static REQUESTED: AtomicU64 = AtomicU64::new(0);
static RAISED: AtomicU64 = AtomicU64::new(0);
static ACKED: AtomicU64 = AtomicU64::new(0);
static KICKS: AtomicU64 = AtomicU64::new(0);
static HALTS: AtomicU64 = AtomicU64::new(0);
static TIMED_OUT: AtomicBool = AtomicBool::new(false);

fn timed_out() {
    TIMED_OUT.store(true, Ordering::Release);
}

/// A kick not yet taken by the core it woke.
static KICK: AtomicBool = AtomicBool::new(false);

/// From the IDT, after EOI: the halt it ended is all a kick is for.
pub(crate) fn kicked() {
    KICKS.fetch_add(1, Ordering::AcqRel);
    KICK.store(true, Ordering::Release);
}

/// Wake dedicated core `cpu` for host device work.
pub(super) fn kick(cpu: usize) {
    if let Some(apic) = crate::lapic::apic_id(cpu) {
        unsafe { crate::lapic::send_ipi(apic, KICK_VECTOR) };
    }
}

/// Whether a kick arrived since the last look, consuming it.
pub(super) fn take_kick() -> bool {
    KICK.swap(false, Ordering::AcqRel)
}

/// Whether host device work woke this core (for `BootHost::wait_for_interrupt`).
pub(super) fn host_line() -> bool {
    HOST_LINE.load(Ordering::Acquire)
}

struct IrqDevices {
    aic: Aic2,
    wires: InterruptLatch,
    span: u64,
    /// The AIC input level as last forwarded.
    level: bool,
}

impl IrqDevices {
    fn new() -> Self {
        let aic = Aic2::stacked(
            Aic2Config {
                version: 2,
                nr_irq: 0x3a0,
                max_irq: 0x400,
                irq_config: AIC2_IRQ_CFG,
                event_offset: 0x8000,
                timestamps: 16,
                timestamp_base: AIC2_TIMESTAMPS,
                cpus: 1,
                default_cpus: 1,
            },
            1,
        )
        .expect("AIC2 fixture configuration");
        let span = aic.span();
        Self { aic, wires: InterruptLatch::new(), span, level: false }
    }
    /// Forward the host line to the AIC input when it changed.
    fn sync(&mut self, cpu: usize, now: u64) {
        let level = HOST_LINE.load(Ordering::Acquire);
        if level != self.level {
            self.level = level;
            self.aic
                .set_input(INPUT, level, &mut Access::new(cpu, now, &mut self.wires))
                .expect("AIC fixture input");
        }
    }
}

fn refused(_what: &'static str) -> DeviceError {
    DeviceError::Refused(stela_contracts::refuse!("nk IRQ fixture refused the access"))
}

impl Devices for IrqDevices {
    fn read(&mut self, cpu: usize, at: u64, bytes: u8, now: u64) -> Result<u64, DeviceError> {
        let width = Width::from_bytes(bytes as u64).ok_or_else(|| refused("width"))?;
        if (AIC..AIC + self.span).contains(&at) {
            self.sync(cpu, now);
            return self
                .aic
                .read(at - AIC, width, &mut Access::new(cpu, now, &mut self.wires))
                .map_err(|_| refused("AIC read"));
        }
        match at.wrapping_sub(DOOR) {
            0 => Ok(HOST_LINE.load(Ordering::Acquire) as u64),
            _ => Err(DeviceError::NoDevice),
        }
    }
    fn write(&mut self, cpu: usize, at: u64, bytes: u8, value: u64, now: u64) -> Result<(), DeviceError> {
        let width = Width::from_bytes(bytes as u64).ok_or_else(|| refused("width"))?;
        if (AIC..AIC + self.span).contains(&at) {
            return self
                .aic
                .write(at - AIC, width, value, &mut Access::new(cpu, now, &mut self.wires))
                .map_err(|_| refused("AIC write"));
        }
        match at.wrapping_sub(DOOR) {
            // Acknowledge: the device's line drops.
            0 => {
                ACKED.store(crate::timer::time_ns(), Ordering::Release);
                HOST_LINE.store(false, Ordering::Release);
                self.sync(cpu, now);
                Ok(())
            }
            // Request one interrupt from the host side.
            8 => {
                REQUESTED.store(crate::timer::time_ns(), Ordering::Release);
                Ok(())
            }
            _ => Err(DeviceError::NoDevice),
        }
    }
    fn lines(&mut self, cpu: usize, now: u64) -> Lines {
        self.sync(cpu, now);
        let pins = self.wires.pins(cpu);
        Lines { irq: pins.irq, fiq: pins.fiq }
    }
    fn waits_on_host(&self) -> bool {
        true
    }
}

/// The host side, on another CPU: once the guest asks, wait, raise the line
/// and kick the guest's core.
extern "C" fn helper(_: usize) {
    let give_up = crate::timer::time_ns() + 2_000_000_000;
    let asked = loop {
        let at = REQUESTED.load(Ordering::Acquire);
        if at != 0 {
            break at;
        }
        if crate::timer::time_ns() > give_up {
            return;
        }
        core::hint::spin_loop();
    };
    while crate::timer::time_ns() < asked + RAISE_AFTER_NS {
        core::hint::spin_loop();
    }
    RAISED.store(crate::timer::time_ns(), Ordering::Release);
    HOST_LINE.store(true, Ordering::Release);
    kick(GUEST_CPU);
}

pub(crate) fn irq_probe() -> bool {
    static PASSED: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_core(_: usize) {
        const PAGES: usize = 256;
        let stack = crate::frames::alloc_contiguous(PAGES).expect("stela engine stack");
        let flags = crate::sync::irq_save();
        unsafe {
            nk_stela_on_stack(stack.add(PAGES * 4096), run_irq_guest);
            crate::sync::irq_restore(flags);
            for page in 0..PAGES {
                crate::frames::free(stack.add(page * 4096));
            }
        }
    }
    extern "C" fn run_irq_guest() {
        PASSED.store(irq_guest(), Ordering::Release);
    }
    let helper = crate::sched::spawn("host-irq", helper, 0);
    crate::sched::set_affinity(helper, 1 << HELPER_CPU);
    let Some(id) = crate::sched::dedicate_cpu(GUEST_CPU, "vcpu-guest-irq", on_core, 0) else {
        crate::println!("STELA_IRQ: dedicate_cpu({}) refused (needs NK_SMP)", GUEST_CPU);
        return false;
    };
    crate::sched::join(id);
    crate::sched::join(helper);
    // Read once the core is done: a kick that lands while the guest runs
    // (IF masked) is delivered only when it returns.
    let (kicks, halts) = (KICKS.load(Ordering::Acquire), HALTS.load(Ordering::Acquire));
    crate::println!("STELA_IRQ: core halted {} times for host work; kicks delivered {}", halts, kicks);
    let passed = PASSED.load(Ordering::Acquire) && kicks >= 1 && halts >= 1;
    crate::println!("{}", if passed { "STELA_IRQ_PASS" } else { "STELA_IRQ_FAIL" });
    passed
}

fn irq_guest() -> bool {
    let ram = Ram::new();
    let base = ram.0 as u64;
    let host = BootHost::new(ram);
    // Assembled with clang (aarch64-none-elf):
    //   adr x0, vectors; msr vbar_el1, x0; isb; mov x5, #0
    //   ldr x20, =AIC; ldr x21, =DOOR
    //   mov w1, #1; str w1, [x20, #0x14]            AIC CONFIG: enable
    //   mov x2, #0x3184; str w1, [x20, x2]          MASK_CLR: input 32
    //   str w1, [x21, #8]                           ask the host
    //   msr daifclr, #2
    // wait: wfi; cbz x5, wait; msr daifset, #2
    // done: b done
    //   AIC, DOOR (literals)
    let code: [u32; 20] = [
        0x1000_4000, 0xd518_c000, 0xd503_3fdf, 0xd280_0005, 0x5800_0194, 0x5800_01b5,
        0x5280_0021, 0xb900_1681, 0xd286_3082, 0xb822_6a81, 0xb900_0aa1, 0xd503_42ff,
        0xd503_207f, 0xb4ff_ffe5, 0xd503_42df, 0x1400_0000,
        AIC as u32, (AIC >> 32) as u32, DOOR as u32, (DOOR >> 32) as u32,
    ];
    // vectors (base + 0x800) + 0x280, current EL with SPx, IRQ:
    //   mov x2, #0x8000; ldr w9, [x20, x2]          claim the event
    //   str wzr, [x21]                              ack the device
    //   mov w3, #1; mov x2, #0x3184; str w3, [x20, x2]   unmask input 32
    //   add x5, x5, #1; eret
    let irq: [u32; 8] = [
        0xd290_0002, 0xb862_6a89, 0xb900_02bf, 0x5280_0023, 0xd286_3082, 0xb822_6a83,
        0x9100_04a5, 0xd69f_03e0,
    ];
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    let handler: Vec<u8> = irq.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut host = host;
    assert!(host.write_ram(base, &bytes));
    assert!(host.write_ram(base + 0xa80, &handler));
    let chip = stela_contracts::Chip::AppleM2;
    let target = TargetCpu::Baseline;
    let cpu = Cpu::of(chip, base, Pstate::reset(1));
    let profile = CpuProfile {
        name: "nk guest IRQ probe",
        chip,
        counter_frequency: 24_000_000,
        fixed: identification::derive(chip, target, true, identification::Speculation::NONE),
        processors: Vec::new(),
    };
    let mut machine = Machine::new(cpu, host, IrqDevices::new(), profile, target);
    let stop_pc = base + 0x3c;
    machine.set_breakpoints(&[stop_pc]);
    let (mut stop, mut waits_for_host) = (machine.run(10_000), 0u32);
    while matches!(stop, Stop::Waiting) && waits_for_host < 8 {
        waits_for_host += 1;
        // Halt until the host's kick; a 100 ms hrtimer bounds a lost one.
        TIMED_OUT.store(false, Ordering::Release);
        crate::hrtimer::arm_in_ns(100_000_000, timed_out);
        while !HOST_LINE.load(Ordering::Acquire) && !TIMED_OUT.load(Ordering::Acquire) {
            HALTS.fetch_add(1, Ordering::Relaxed);
            unsafe { core::arch::asm!("sti", "hlt", "cli", options(nomem, nostack)) };
        }
        crate::hrtimer::cancel();
        stop = machine.run(10_000);
    }
    let st = &machine.cpu.state;
    let (raised, acked) = (RAISED.load(Ordering::Acquire), ACKED.load(Ordering::Acquire));
    let latency_us = acked.saturating_sub(raised) / 1000;
    crate::println!(
        "STELA_IRQ: stop {:?}; IRQs taken {}; AIC event {:#x}; run returned Waiting {}x; raise -> guest ack {} us",
        stop, st.x[5], st.x[9] as u32, waits_for_host, latency_us
    );
    matches!(stop, Stop::Breakpoint(pc) if pc == stop_pc)
        && st.x[5] == 1
        && st.x[9] as u32 as u64 == EVENT_IRQ | INPUT.irq as u64
        && waits_for_host >= 1
        && raised != 0
        && acked >= raised
        && !HOST_LINE.load(Ordering::Acquire)
}

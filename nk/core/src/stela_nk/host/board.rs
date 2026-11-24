// SPDX-License-Identifier: GPL-2.0-or-later
//! The M2 board's first devices mounted on nk from a DeviceTree (ROADMAP
//! Phase 5, NK_STELA_BOARD_TEST).
//!
//! `stela_soc_data::board::board` parses an M2-shaped (chip 0x8112)
//! DeviceTree -- the fixture `stela-soc-data` itself tests with, until a real
//! ADT is read from disk -- and its UART, watchdog and AIC are placed on
//! `stela_soc::machine::AppleDevices`, the same board adapter and calls
//! `stela-runtime`'s full mount starts with. The guest is given only their
//! addresses and the UART's AIC input; every register decision is the
//! models'.
//!
//! nk "types" `nk\n` into the board's console from CPU 2 and kicks CPU 3.
//! The UART model takes the bytes on its next advance and raises its RX
//! timeout interrupt into the AIC, which raises the guest's IRQ pin. The
//! guest's handler claims the AIC event, drains URXH, echoes each byte
//! uppercased through UTXH, clears the status and unmasks. PASS: the console
//! receives `NK\n` and the AIC event is the UART's input.

use super::*;
use alloc::{rc::Rc, vec};
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use stela_soc::machine::{AicInput, AppleDevices, ConsoleSink, Range};

const GUEST_CPU: usize = 3;
const HELPER_CPU: usize = 2;
const INPUT: &[u8] = b"nk\n";
const TYPE_AFTER_NS: u64 = 5_000_000;

/// Host console input: bytes `0..TYPED` of `INPUT` are typed; `TAKEN` of
/// them the UART has taken.
static TYPED: AtomicUsize = AtomicUsize::new(0);
static TAKEN: AtomicUsize = AtomicUsize::new(0);
static READY: AtomicBool = AtomicBool::new(false);
static HALTS: AtomicU64 = AtomicU64::new(0);
static TIMED_OUT: AtomicBool = AtomicBool::new(false);

struct Console(Rc<RefCell<Vec<u8>>>);
impl ConsoleSink for Console {
    fn transmit(&mut self, byte: u8) -> Result<(), stela_soc::uart::ConsoleBusy> {
        self.0.borrow_mut().push(byte);
        Ok(())
    }
    fn poll(&mut self) -> Option<u8> {
        let taken = TAKEN.load(Ordering::Acquire);
        (taken < TYPED.load(Ordering::Acquire)).then(|| {
            TAKEN.store(taken + 1, Ordering::Release);
            INPUT[taken]
        })
    }
}

fn timed_out() {
    TIMED_OUT.store(true, Ordering::Release);
}

/// CPU 2: once the guest is waiting, type the input and kick its core.
extern "C" fn typist(_: usize) {
    let give_up = crate::timer::time_ns() + 2_000_000_000;
    while !READY.load(Ordering::Acquire) {
        if crate::timer::time_ns() > give_up {
            return;
        }
        core::hint::spin_loop();
    }
    let at = crate::timer::time_ns() + TYPE_AFTER_NS;
    while crate::timer::time_ns() < at {
        core::hint::spin_loop();
    }
    TYPED.store(INPUT.len(), Ordering::Release);
    super::irq::kick(GUEST_CPU);
}

pub(crate) fn board_probe() -> bool {
    static PASSED: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_core(_: usize) {
        const PAGES: usize = 256;
        let stack = crate::frames::alloc_contiguous(PAGES).expect("stela engine stack");
        let flags = crate::sync::irq_save();
        unsafe {
            nk_stela_on_stack(stack.add(PAGES * 4096), run_board_guest);
            crate::sync::irq_restore(flags);
            for page in 0..PAGES {
                crate::frames::free(stack.add(page * 4096));
            }
        }
    }
    extern "C" fn run_board_guest() {
        PASSED.store(board_guest(), Ordering::Release);
    }
    let helper = crate::sched::spawn("host-console", typist, 0);
    crate::sched::set_affinity(helper, 1 << HELPER_CPU);
    let Some(id) = crate::sched::dedicate_cpu(GUEST_CPU, "vcpu-board", on_core, 0) else {
        crate::println!("STELA_BOARD: dedicate_cpu({}) refused (needs NK_SMP)", GUEST_CPU);
        return false;
    };
    crate::sched::join(id);
    crate::sched::join(helper);
    let passed = PASSED.load(Ordering::Acquire) && HALTS.load(Ordering::Acquire) >= 1;
    crate::println!("{}", if passed { "STELA_BOARD_PASS" } else { "STELA_BOARD_FAIL" });
    passed
}

fn board_guest() -> bool {
    use stela_soc_data::board::fixture_support::tree;
    let dt = tree(0x8112, "Apple M2", ["uart-1,samsung", "wdt,s5l8960x", "aic,2"]);
    let board = match stela_soc_data::board::board(&dt) {
        Ok(board) => board,
        Err(error) => {
            crate::println!("STELA_BOARD: DeviceTree refused: {:?}", error);
            return false;
        }
    };
    let (uart, wdt, aic) = (board.uart, board.watchdog, board.aic);
    let uart_irq = AicInput::new(0, uart.irq as u16);
    let output = Rc::new(RefCell::new(Vec::new()));
    let mut devices = AppleDevices::for_boot_processor(0);
    let placed = devices
        .add_uart(Console(output.clone()), &[Range { physical: uart.physical, bytes: uart.bytes }], Some(uart_irq))
        .and_then(|()| {
            devices.add_watchdog_with_side(
                Range { physical: wdt.physical, bytes: wdt.bytes },
                board.watchdog_side.as_ref(),
                Some(AicInput::new(0, wdt.irq as u16)),
                0,
            )
        })
        .and_then(|()| devices.add_aic_for_processors(aic.physical, aic.inputs, aic.layout, 1));
    if let Err(error) = placed {
        crate::println!("STELA_BOARD: placement refused: {:?}", error);
        return false;
    }
    let stela_soc_data::aic::AicLayout::V2 { irq_config, max_irq, event, .. } = aic.layout else {
        crate::println!("STELA_BOARD: the M2 DeviceTree gave an AICv1");
        return false;
    };
    let mask_clear = stela_soc_data::aic::layout::Aic2DieLayout::contiguous(irq_config, max_irq)
        .expect("AIC2 die layout")
        .mask_clear;
    crate::println!(
        "STELA_BOARD: AIC2 MAX_IRQ {:#x}, IRQ_CFG {:#x}, MASK_CLR {:#x}, event {:#x}",
        max_irq, irq_config, mask_clear, event
    );
    crate::println!(
        "STELA_BOARD: chip {:#x} from DeviceTree: uart {:#x}+{:#x} irq {}, wdt {:#x} irq {}, aic {:#x} ({} inputs)",
        0x8112, uart.physical, uart.bytes, uart.irq, wdt.physical, wdt.irq, aic.physical, aic.inputs
    );

    let ram = Ram::new();
    let base = ram.0 as u64;
    let mut host = BootHost::new(ram);
    // Assembled with clang (aarch64-none-elf). x20 = AIC, x21 = UART; from
    // the parsed AIC layout, x23 = the UART input's MASK_CLR word offset,
    // w24 = its bit and x25 = the event register's offset:
    //   adr x0, vectors; msr vbar_el1, x0; isb; mov x5, #0
    //   mov w1, #1; str w1, [x20, #0x14]                 AIC CONFIG: enable
    //   str w24, [x20, x23]                              unmask the UART
    //   mov w1, #0x285; str w1, [x21, #4]                UCON: RX/TX modes,
    //                                                    RX timeout + RXTO IRQ
    //   msr daifclr, #2
    // wait: wfi; cmp x5, #3; b.lo wait; msr daifset, #2
    // done: b done
    let code: [u32; 15] = [
        0x1000_4000, 0xd518_c000, 0xd503_3fdf, 0xd280_0005, 0x5280_0021, 0xb900_1681,
        0xb837_6a98, 0x5280_50a1, 0xb900_06a1, 0xd503_42ff, 0xd503_207f, 0xf100_0cbf,
        0x54ff_ffc3, 0xd503_42df, 0x1400_0000,
    ];
    // vectors (base + 0x800) + 0x280, current EL with SPx, IRQ:
    //   ldr w9, [x20, x25]                               AIC event: claim
    // rx: ldr w6, [x21, #0x10]; tbz w6, #0, drained      UTRSTAT.RX ready
    //   ldr w7, [x21, #0x24]                             URXH
    //   if 'a' <= w7 <= 'z' { w7 -= 0x20 }
    //   str w7, [x21, #0x20]; add x5, x5, #1; b rx       UTXH: echo
    // drained: mov w6, #0x218; str w6, [x21, #0x10]      clear RX statuses
    //   str w24, [x20, x23]; eret                        unmask the UART
    let irq: [u32; 15] = [
        0xb879_6a89, 0xb940_12a6, 0x3600_0126, 0xb940_26a7, 0x5101_84e8, 0x7100_651f,
        0x5400_0048, 0x5100_80e7, 0xb900_22a7, 0x9100_04a5, 0x17ff_fff7, 0x5280_4306,
        0xb900_12a6, 0xb837_6a98, 0xd69f_03e0,
    ];
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    let handler: Vec<u8> = irq.iter().flat_map(|w| w.to_le_bytes()).collect();
    assert!(host.write_ram(base, &bytes));
    assert!(host.write_ram(base + 0xa80, &handler));
    let chip = stela_contracts::Chip::AppleM2;
    let target = TargetCpu::Baseline;
    let cpu = Cpu::of(chip, base, Pstate::reset(1));
    let profile = CpuProfile {
        name: "nk M2 board probe",
        chip,
        counter_frequency: 24_000_000,
        fixed: identification::derive(chip, target, true, identification::Speculation::NONE),
        processors: vec![],
    };
    let mut machine = Machine::new(cpu, host, devices, profile, target);
    machine.cpu.state.x[20] = aic.physical;
    machine.cpu.state.x[21] = uart.physical;
    machine.cpu.state.x[23] = mask_clear + 4 * u64::from(uart.irq / 32);
    machine.cpu.state.x[24] = 1 << (uart.irq % 32);
    machine.cpu.state.x[25] = event;
    let stop_pc = base + 0x38;
    machine.set_breakpoints(&[stop_pc]);
    let mut stop = machine.run(10_000);
    READY.store(true, Ordering::Release);
    let mut runs = 1u32;
    // A PE waiting with no device deadline returns (Idle, or Waiting while
    // the SoC sleeps); halt until the host's kick and run it again.
    while matches!(stop, Stop::Idle | Stop::Waiting) && runs < 8 {
        TIMED_OUT.store(false, Ordering::Release);
        crate::hrtimer::arm_in_ns(100_000_000, timed_out);
        while !super::irq::take_kick() && !TIMED_OUT.load(Ordering::Acquire) {
            HALTS.fetch_add(1, Ordering::Relaxed);
            unsafe { core::arch::asm!("sti", "hlt", "cli", options(nomem, nostack)) };
        }
        crate::hrtimer::cancel();
        stop = machine.run(10_000);
        runs += 1;
    }
    let st = &machine.cpu.state;
    let echoed = output.borrow().clone();
    let shown: alloc::string::String = echoed.iter().map(|&b| if b == b'\n' { '|' } else { b as char }).collect();
    crate::println!(
        "STELA_BOARD: stop {:?}; runs {}; bytes received {}; AIC event {:#x}; console got {:?}; halts {}; typed {}, taken by the UART {}; pc {:#x}",
        stop, runs, st.x[5], st.x[9] as u32, shown, HALTS.load(Ordering::Acquire),
        TYPED.load(Ordering::Acquire), TAKEN.load(Ordering::Acquire), machine.cpu.pc.wrapping_sub(base)
    );
    matches!(stop, Stop::Breakpoint(pc) if pc == stop_pc)
        && st.x[5] == 3
        && st.x[9] as u32 == (1 << 16) | uart.irq as u32
        && echoed == b"NK\n"
        && TAKEN.load(Ordering::Acquire) == INPUT.len()
}

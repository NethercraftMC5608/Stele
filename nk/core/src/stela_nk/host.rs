//! Bounded system-mode engine host for the trusted pre-SMP probe only.
//! MMU-off identity RAM, bounded native fault recovery and a polled UART model.
//! Its shadow child adds an MMU-on fixture; there is no arbitrary guest
//! admission or complete device board here.
use super::Code;
use alloc::vec::Vec;
use stela_compiler::{GuestState, TargetCpu};
use stela_engine::machine::{
    exception::Pstate, identification, mmu, Cpu, CpuProfile, Host, InstallError, Machine,
    NativeCode, NativeExit, NoDevices, Regime, Stop,
};
mod board;
mod disk;
mod irq;
mod mount;
mod shadow;
mod sptm;
pub(crate) use board::{adt_probe, board_probe};
pub(crate) use mount::mount_probe;
pub(crate) use sptm::sptm_probe;
pub(crate) use irq::{irq_probe, kicked};

// The pinned system-mode ABI borrows callee-saved host registers. Keep them
// outside the generated entry, and keep the call stack SysV-aligned.
core::arch::global_asm!(
    ".global nk_stela_on_stack",
    "nk_stela_on_stack:",
    "push rbx",
    "mov rbx, rsp",
    "mov rsp, rdi",
    "and rsp, -16",
    "call rsi",
    "mov rsp, rbx",
    "pop rbx",
    "ret",
    ".global nk_stela_system_enter",
    "nk_stela_system_enter:",
    "push rbx",
    "push rbp",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "sub rsp, 8",
    "mov [rcx], rsp", // checked invocation recovery stack
    "mov rax, rdi",   // pinned entry
    "mov rcx, rsi",   // installed native block
    "mov rdi, rdx",   // GuestState
    "call rax",
    ".Lnk_stela_return:",
    "add rsp, 8",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbp",
    "pop rbx",
    "ret",
    ".global nk_stela_system_recover",
    "nk_stela_system_recover:",
    "mov rsp, rdi",
    "xor eax, eax",
    "mov edx, 0xffff",
    "jmp .Lnk_stela_return",
);
unsafe extern "sysv64" {
    fn nk_stela_on_stack(top: *mut u8, run: extern "C" fn());
    pub(super) fn nk_stela_system_enter(
        entry: *const u8,
        code: usize,
        state: *mut GuestState,
        stack: *mut u64,
    ) -> u128;
    pub(super) fn nk_stela_system_recover();
}

struct Ram(*mut u8);
impl Ram {
    fn new() -> Self {
        let ptr = crate::frames::alloc().expect("stela guest RAM");
        unsafe { crate::paging::boot_code_rights(ptr as u64, false) };
        Self(ptr)
    }
    fn offset(&self, at: u64, len: usize) -> Option<usize> {
        let off = usize::try_from(at.checked_sub(self.0 as u64)?).ok()?;
        (off.checked_add(len)? <= 4096).then_some(off)
    }
}
impl Drop for Ram {
    fn drop(&mut self) {
        unsafe { crate::frames::free(self.0) };
    }
}

struct BootHost {
    ram: Ram,
    code: Vec<(Code, usize)>,
    entry: Code,
    executions: usize,
    patches: usize,
    faults: usize,
    /// The TSC at which the guest counter reads 0.
    tsc_base: u64,
    /// The guest's armed timer, in counter ticks.
    deadline: Option<u64>,
    /// Whether that deadline is on this core's hrtimer.
    hrtimer: bool,
    waits: u64,
}
impl BootHost {
    fn new(ram: Ram) -> Self {
        Self {
            ram,
            code: Vec::new(),
            entry: Code::new(&stela_compiler::x86_64::pinned::entry()),
            executions: 0,
            patches: 0,
            faults: 0,
            tsc_base: crate::arch::cycles(),
            deadline: None,
            hrtimer: false,
            waits: 0,
        }
    }
    fn extent(&self, at: usize, len: usize) -> Option<&Code> {
        self.code.iter().find_map(|(code, size)| {
            let off = at.checked_sub(code.0 as usize)?;
            (off.checked_add(len)? <= *size).then_some(code)
        })
    }
}
impl Host for BootHost {
    fn read_ram(&mut self, at: u64, out: &mut [u8]) -> bool {
        let Some(off) = self.ram.offset(at, out.len()) else {
            return false;
        };
        unsafe { core::ptr::copy_nonoverlapping(self.ram.0.add(off), out.as_mut_ptr(), out.len()) };
        true
    }
    fn write_ram(&mut self, at: u64, data: &[u8]) -> bool {
        let Some(off) = self.ram.offset(at, data.len()) else {
            return false;
        };
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.ram.0.add(off), data.len()) };
        true
    }
    fn install(&mut self, bytes: &[u8]) -> Result<NativeCode, InstallError> {
        if bytes.is_empty() || bytes.len() > 4096 || self.code.len() >= 64 {
            return Err(InstallError::Full);
        }
        let code = Code::new(bytes);
        let address = NativeCode(code.0 as usize);
        self.code.push((code, bytes.len()));
        Ok(address)
    }
    fn patch(&mut self, at: NativeCode, bytes: &[u8]) {
        let code = self
            .extent(at.0, bytes.len())
            .expect("owned idle patch range");
        unsafe {
            crate::paging::boot_code_rights(code.0 as u64, false);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), at.0 as *mut u8, bytes.len());
            crate::paging::boot_code_rights(code.0 as u64, true);
        }
        self.patches += 1;
    }
    fn read_native(&mut self, at: NativeCode, out: &mut [u8]) -> bool {
        if self.extent(at.0, out.len()).is_none() {
            return false;
        }
        unsafe { core::ptr::copy_nonoverlapping(at.0 as *const u8, out.as_mut_ptr(), out.len()) };
        true
    }
    fn discard_code(&mut self) {
        self.code.clear();
    }
    unsafe fn execute(&mut self, at: NativeCode, state: &mut GuestState) -> NativeExit {
        assert!(self.code.iter().any(|(c, _)| c.0 as usize == at.0));
        self.executions += 1;
        // The fixture owns every reachable RAM/device address. This host is
        // private and admits no arbitrary guest input or concurrent execution.
        let raw = super::fault::invoke(
            self.entry.0,
            at.0,
            state,
            self.code
                .iter()
                .map(|(c, size)| (c.0 as u64, c.0 as u64 + *size as u64)),
            self.ram.0 as u64,
            None,
        );
        if let Err(fault) = raw {
            self.faults += 1;
            return NativeExit::Fault(fault.identity());
        }
        let raw = raw.unwrap();
        NativeExit::Exit {
            next_pc: raw as u64,
            kind: (raw >> 64) as u64,
        }
    }
    fn placement(&self) -> mmu::placement::Placement {
        mmu::placement::Placement::FourLevel { slices: 2 }
    }
    fn set_regime(&mut self, regime: &Regime) {
        assert_eq!(regime.controls.sctlr & 1, 0, "boot host requires MMU off");
        assert!(!regime.controls.stage2, "boot host has no stage 2");
    }
    fn invalidate_translations(&mut self) {} // identity RAM; no shadow cache
    /// The guest system counter at the M2's 24 MHz, from the TSC.
    fn counter(&mut self) -> u64 {
        guest_count(self.tsc_base)
    }
    /// On a dedicated core the deadline arms that core's hrtimer; elsewhere
    /// (the pre-SMP boot probe) `wait_for_interrupt` waits on the counter.
    fn arm_timer(&mut self, deadline: Option<u64>) {
        guest_timer().store(false, core::sync::atomic::Ordering::Release);
        match deadline {
            None => {
                self.deadline = None;
                crate::hrtimer::cancel();
            }
            Some(count) => {
                self.deadline = Some(count);
                let tsc = self.tsc_base + count_to_tsc(count);
                self.hrtimer = crate::hrtimer::arm_at(tsc, guest_timer_fired);
            }
        }
    }
    /// Halt until the guest's timer interrupt. Native execution runs with IF
    /// masked (`fault::invoke`), so interrupts open only across the halt:
    /// `sti; hlt` admits no gap between the check and the sleep.
    fn wait_for_interrupt(&mut self) {
        use core::sync::atomic::Ordering;
        self.waits += 1;
        if self.hrtimer {
            // Host device work ends the halt as well -- a raised line or
            // a kick (`irq`): the engine then advances the devices.
            while !guest_timer().swap(false, Ordering::AcqRel) && !irq::host_line() && !irq::take_kick() {
                unsafe { core::arch::asm!("sti", "hlt", "cli", options(nomem, nostack)) };
            }
            return;
        }
        let Some(deadline) = self.deadline else {
            panic!("boot guest waits with no timer armed");
        };
        while guest_count(self.tsc_base) < deadline {
            core::hint::spin_loop();
        }
    }
    fn poll_interrupt(&mut self) -> bool {
        guest_timer().swap(false, core::sync::atomic::Ordering::AcqRel)
    }
}

const GUEST_HZ: u128 = 24_000_000;
static GUEST_TIMERS: [core::sync::atomic::AtomicBool; crate::sched::MAX_CPUS] =
    [const { core::sync::atomic::AtomicBool::new(false) }; crate::sched::MAX_CPUS];

fn guest_timer() -> &'static core::sync::atomic::AtomicBool {
    &GUEST_TIMERS[crate::arch::cpu_id()]
}

fn guest_timer_fired() {
    guest_timer().store(true, core::sync::atomic::Ordering::Release);
}

fn guest_count(tsc_base: u64) -> u64 {
    let hz = crate::timer::cycle_hz().max(1) as u128;
    ((crate::arch::cycles().wrapping_sub(tsc_base) as u128 * GUEST_HZ) / hz) as u64
}

fn count_to_tsc(count: u64) -> u64 {
    let hz = crate::timer::cycle_hz().max(1) as u128;
    ((count as u128 * hz).div_ceil(GUEST_HZ)) as u64
}

pub(super) fn probe() {
    // Machine construction/dispatch needs more than nk's 64 KiB boot stack.
    // A private pre-SMP stack keeps those frames out of static kernel/LKL data.
    const PAGES: usize = 256;
    let stack = crate::frames::alloc_contiguous(PAGES).expect("stela engine stack");
    unsafe {
        core::ptr::write_bytes(stack, 0xa5, PAGES * 4096);
        nk_stela_on_stack(stack.add(PAGES * 4096), run_probe);
        let bytes = core::slice::from_raw_parts(stack, PAGES * 4096);
        assert!(
            bytes[..4096].iter().all(|b| *b == 0xa5),
            "engine stack exhausted"
        );
        let low = bytes.iter().position(|b| *b != 0xa5).unwrap_or(bytes.len());
        crate::println!("STELA_ENGINE_STACK: {} bytes touched", bytes.len() - low);
        for page in 0..PAGES {
            crate::frames::free(stack.add(page * 4096));
        }
    }
}

#[inline(never)]
extern "C" fn run_probe() {
    crate::println!("STELA_ENGINE_BEGIN");
    let ram = Ram::new();
    let base = ram.0 as u64;
    let mut host = BootHost::new(ram);
    // mov x0,#3; add x2,x2,#7; subs x0,x0,#1; b.ne add;
    // mrs x3,CurrentEL; msr TPIDR_EL1,x2; mrs x4,TPIDR_EL1;
    // str x2,[x1]; b stop. The engine handles all exits and dispatch.
    let words = [
        0xd280_0060u32,
        0x9100_1c42,
        0xf100_0400,
        0x54ff_ffc1,
        0xd538_4243,
        0xd518_d082,
        0xd538_d084,
        0xf900_0022,
        0x1400_0001,
    ];
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    assert!(host.write_ram(base, &bytes));
    // Host-side range checks are independent of the trusted native fixture.
    assert!(!host.read_ram(base - 1, &mut [0]));
    assert!(!host.write_ram(base + 4095, &[1, 2]));
    let chip = stela_contracts::Chip::AppleM2;
    let target = TargetCpu::Baseline;
    let mut cpu = Cpu::of(chip, base, Pstate::reset(1));
    cpu.state.x[1] = base + 2048;
    let profile = CpuProfile {
        name: "nk bounded M2 engine probe",
        chip,
        counter_frequency: 24_000_000,
        fixed: identification::derive(chip, target, true, identification::Speculation::NONE),
        processors: Vec::new(),
    };
    let mut machine = Machine::new(cpu, host, NoDevices, profile, target);
    let stop_pc = base + bytes.len() as u64;
    machine.set_breakpoints(&[stop_pc]);
    let stop = machine.run(64);
    assert!(
        matches!(stop, Stop::Breakpoint(pc) if pc == stop_pc),
        "engine stopped: {:?}",
        stop
    );
    assert_eq!(machine.cpu.state.x[0], 0);
    assert_eq!(machine.cpu.state.x[2], 21);
    assert_eq!(machine.cpu.state.x[3], 4); // EL1
    assert_eq!(machine.cpu.state.x[4], 21);
    let mut data = [0; 8];
    assert!(machine.host.read_ram(base + 2048, &mut data));
    assert_eq!(u64::from_le_bytes(data), 21);
    assert!(machine.host.executions > 0, "must reach native host entry");
    crate::println!(
        "STELA_ENGINE_PASS: {} native entries, {} patches, {} blocks",
        machine.host.executions,
        machine.host.patches,
        machine.statistics.blocks
    );
    drop(machine);
    uart_probe();
}

// A fixture address intentionally outside every nk mapping, not an ADT mount.
const UART: u64 = 0x10_0000_0000;
struct Console(alloc::rc::Rc<core::cell::RefCell<Vec<u8>>>);
impl stela_soc::uart::ConsoleSink for Console {
    fn transmit(&mut self, byte: u8) -> Result<(), stela_soc::uart::ConsoleBusy> {
        self.0.borrow_mut().push(byte);
        Ok(())
    }
}
struct UartDevices {
    bus: stela_contracts::bus::Bus,
    wires: stela_contracts::interrupt::InterruptLatch,
    reads: usize,
    writes: usize,
}
impl stela_engine::machine::Devices for UartDevices {
    fn read(
        &mut self,
        cpu: usize,
        at: u64,
        bytes: u8,
        now: u64,
    ) -> Result<u64, stela_engine::machine::DeviceError> {
        use stela_contracts::bus::{Access, Width};
        self.reads += 1;
        let width = Width::from_bytes(bytes as u64).expect("scalar device width");
        self.bus
            .read(at, width, &mut Access::new(cpu, now, &mut self.wires))
            .map_err(|e| {
                stela_engine::machine::DeviceError::Refused(stela_contracts::refuse!(
                    "UART read refused",
                    ?e
                ))
            })
    }
    fn write(
        &mut self,
        cpu: usize,
        at: u64,
        bytes: u8,
        value: u64,
        now: u64,
    ) -> Result<(), stela_engine::machine::DeviceError> {
        use stela_contracts::bus::{Access, Width};
        self.writes += 1;
        let width = Width::from_bytes(bytes as u64).expect("scalar device width");
        self.bus
            .write(
                at,
                width,
                value,
                &mut Access::new(cpu, now, &mut self.wires),
            )
            .map_err(|e| {
                stela_engine::machine::DeviceError::Refused(stela_contracts::refuse!(
                    "UART write refused",
                    ?e
                ))
            })
    }
    fn lines(&mut self, _: usize, _: u64) -> stela_engine::machine::Lines {
        stela_engine::machine::Lines::default() // polled UART, no IRQ connected
    }
}

fn uart_probe() {
    use alloc::{boxed::Box, rc::Rc};
    use core::cell::RefCell;
    use stela_contracts::{
        bus::{Access, Bus, Width},
        interrupt::InterruptLatch,
    };
    use stela_engine::machine::system::Register;
    crate::println!("STELA_MMIO_BEGIN");
    let output = Rc::new(RefCell::new(Vec::new()));
    let mut bus = Bus::new();
    let uart = bus.add(Box::new(stela_soc::uart::S5lUart::new(
        Console(output.clone()),
        None,
    )));
    bus.map(uart, UART, 0x100, 0).expect("UART fixture bus map");
    let mut wires = InterruptLatch::new();
    // The real model must reject an unsupported width rather than emit a byte.
    assert!(bus
        .write(
            UART + 0x20,
            Width::Byte,
            0xff,
            &mut Access::new(0, 0, &mut wires)
        )
        .is_err());
    assert!(output.borrow().is_empty());
    let devices = UartDevices {
        bus,
        wires,
        reads: 0,
        writes: 0,
    };
    let ram = Ram::new();
    let base = ram.0 as u64;
    let mut host = BootHost::new(ram);
    // ORR sets a cached vector before the fault; read UTRSTAT; write NK\n to UTXH.
    let words = [
        0x4ea2_1c20u32,
        0xb940_10a9,
        0xb900_20a6,
        0xb900_20a7,
        0xb900_20a8,
        0x1400_0001,
    ];
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    assert!(host.write_ram(base, &bytes));
    let chip = stela_contracts::Chip::AppleM2;
    let target = TargetCpu::Baseline;
    let mut cpu = Cpu::of(chip, base, Pstate::reset(1));
    cpu.registers.set_raw(Register::CPACR_EL1, 0, 3 << 20);
    cpu.state.x[5] = UART;
    cpu.state.x[6] = b'N' as u64;
    cpu.state.x[7] = b'K' as u64;
    cpu.state.x[8] = b'\n' as u64;
    cpu.state.v[1] = [0x00ff_00ff_00ff_00ff; 2];
    cpu.state.v[2] = [0xff00_ff00_ff00_ff00; 2];
    let profile = CpuProfile {
        name: "nk M2 UART fixture",
        chip,
        counter_frequency: 24_000_000,
        fixed: identification::derive(chip, target, true, identification::Speculation::NONE),
        processors: Vec::new(),
    };
    let mut machine = Machine::new(cpu, host, devices, profile, target);
    let end = base + bytes.len() as u64;
    machine.set_breakpoints(&[end]);
    let stop = machine.run(64);
    assert!(
        matches!(stop, Stop::Breakpoint(pc) if pc == end),
        "UART engine stopped: {stop:?}"
    );
    assert_eq!(&*output.borrow(), b"NK\n");
    assert_eq!(machine.cpu.state.x[9] & 6, 6);
    assert_eq!(machine.cpu.state.v[0], [u64::MAX; 2]);
    assert!(
        machine.host.faults > 0,
        "must exercise real native fault recovery"
    );
    assert_eq!(machine.devices.reads, 1);
    assert_eq!(machine.devices.writes, 3);
    crate::println!(
        "STELA_MMIO_UART_OK: {} native faults, exact NK newline, vector preserved",
        machine.host.faults
    );
    drop(machine);
    abort_probe();
    shadow::probe();
    super::ram::canonical_probe();
    crate::println!("STELA_MMIO_PASS");
}

fn abort_probe() {
    use stela_engine::machine::system::Register;
    let ram = Ram::new();
    let base = ram.0 as u64;
    let mut host = BootHost::new(ram);
    let bytes: Vec<u8> = [0xf940_0020u32, 0x1400_0001]
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();
    assert!(host.write_ram(base, &bytes));
    let chip = stela_contracts::Chip::AppleM2;
    let target = TargetCpu::Baseline;
    let mut cpu = Cpu::of(chip, base, Pstate::reset(1));
    cpu.state.x[0] = 0xfeed;
    cpu.state.x[1] = UART + 0x1000; // no device owns this address
    cpu.registers.set_raw(Register::VBAR_EL1, 0, base + 0x800);
    let profile = CpuProfile {
        name: "nk guest abort fixture",
        chip,
        counter_frequency: 24_000_000,
        fixed: identification::derive(chip, target, true, identification::Speculation::NONE),
        processors: Vec::new(),
    };
    let mut machine = Machine::new(cpu, host, NoDevices, profile, target);
    let vector = base + 0xa00; // synchronous exception from current EL using SPx
    machine.set_breakpoints(&[vector, base + bytes.len() as u64]);
    let stop = machine.run(64);
    assert!(
        matches!(stop, Stop::Breakpoint(pc) if pc == vector),
        "unmapped device stopped: {stop:?}"
    );
    assert_eq!(
        machine.cpu.state.x[0], 0xfeed,
        "failed load must not produce a value"
    );
    assert_eq!(machine.cpu.registers.get(Register::ELR_EL1, 0), base);
    assert_eq!(
        machine.cpu.registers.get(Register::FAR_EL1, 0),
        UART + 0x1000
    );
    assert_eq!(machine.cpu.registers.get(Register::ESR_EL1, 0) >> 26, 0x25);
    assert!(machine.host.faults > 0);
    crate::println!(
        "STELA_GUEST_ABORT_OK: missing device, original ELR/FAR and destination retained"
    );
}

/// A guest generic timer interrupt, end to end, on a dedicated core
/// (ROADMAP Phase 3): the guest arms CNTV 10 ms ahead and waits in WFI; the
/// engine computes the deadline and asks the Host to sleep; nk arms the
/// core's hrtimer, halts, and wakes on it; the engine raises the M2's timer
/// FIQ; the guest's handler disables the timer and counts. Run on a dedicated
/// core: `dedicate_cpu` gives the probe the LAPIC timer.
pub(crate) fn timer_probe() -> bool {
    use core::sync::atomic::{AtomicBool, Ordering};
    static PASSED: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_core(_: usize) {
        const PAGES: usize = 256;
        let stack = crate::frames::alloc_contiguous(PAGES).expect("stela engine stack");
        let flags = crate::sync::irq_save();
        unsafe {
            nk_stela_on_stack(stack.add(PAGES * 4096), run_timer_guest);
            crate::sync::irq_restore(flags);
            for page in 0..PAGES {
                crate::frames::free(stack.add(page * 4096));
            }
        }
    }
    extern "C" fn run_timer_guest() {
        PASSED.store(timer_guest(), Ordering::Release);
    }
    let Some(id) = crate::sched::dedicate_cpu(3, "vcpu-guest-timer", on_core, 0) else {
        crate::println!("STELA_TIMER: dedicate_cpu(3) refused (needs NK_SMP)");
        return false;
    };
    crate::sched::join(id);
    let passed = PASSED.load(Ordering::Acquire);
    crate::println!("{}", if passed { "STELA_TIMER_PASS" } else { "STELA_TIMER_FAIL" });
    passed
}

fn timer_guest() -> bool {
    let ram = Ram::new();
    let base = ram.0 as u64;
    let mut host = BootHost::new(ram);
    // Assembled with clang (aarch64-none-elf); see the commit for the source.
    //   adr x0, vectors; msr vbar_el1, x0; isb; mov x5, #0
    //   mrs x1, cntvct_el0; x1 += 240000 (10 ms); msr cntv_cval_el0, x1
    //   msr cntv_ctl_el0, #1; msr daifclr, #1 (FIQ)
    // wait: wfi; cbz x5, wait; mrs x7, cntvct_el0; msr daifset, #1
    // done: b done
    let code: [u32; 17] = [
        0x1000_4000, 0xd518_c000, 0xd503_3fdf, 0xd280_0005, 0xd53b_e041, 0xd295_3002,
        0xf2a0_0062, 0x8b02_0021, 0xd51b_e341, 0xd280_0023, 0xd51b_e323, 0xd503_41ff,
        0xd503_207f, 0xb4ff_ffe5, 0xd53b_e047, 0xd503_41df, 0x1400_0000,
    ];
    // vectors + 0x300 (current EL, SPx, FIQ):
    //   mov x6, #0; msr cntv_ctl_el0, x6; mrs x8, cntvct_el0; add x5, x5, #1; eret
    let fiq: [u32; 5] = [0xd280_0006, 0xd51b_e326, 0xd53b_e048, 0x9100_04a5, 0xd69f_03e0];
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    let handler: Vec<u8> = fiq.iter().flat_map(|w| w.to_le_bytes()).collect();
    assert!(host.write_ram(base, &bytes));
    assert!(host.write_ram(base + 0xb00, &handler));
    let chip = stela_contracts::Chip::AppleM2;
    let target = TargetCpu::Baseline;
    let cpu = Cpu::of(chip, base, Pstate::reset(1));
    let profile = CpuProfile {
        name: "nk guest timer probe",
        chip,
        counter_frequency: 24_000_000,
        fixed: identification::derive(chip, target, true, identification::Speculation::NONE),
        processors: Vec::new(),
    };
    let fired_before = crate::hrtimer::fired();
    let mut machine = Machine::new(cpu, host, NoDevices, profile, target);
    let stop_pc = base + 0x40;
    machine.set_breakpoints(&[stop_pc]);
    let stop = machine.run(10_000);
    let fired = crate::hrtimer::fired() - fired_before;
    let st = &machine.cpu.state;
    let (deadline, at_fiq, after) = (st.x[1], st.x[8], st.x[7]);
    let late_us = at_fiq.saturating_sub(deadline) * 1_000_000 / 24_000_000;
    crate::println!(
        "STELA_TIMER: stop {:?}; FIQs taken {}; deadline count {}; FIQ at {} ({} us late); host waits {}, hrtimer fires {}",
        stop, st.x[5], deadline, at_fiq, late_us, machine.host.waits, fired
    );
    matches!(stop, Stop::Breakpoint(pc) if pc == stop_pc)
        && st.x[5] == 1
        && at_fiq >= deadline
        && after >= at_fiq
        && machine.host.waits >= 1
        && fired >= 1
        && late_us < 5_000
}

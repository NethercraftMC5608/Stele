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
mod shadow;

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
}
impl BootHost {
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
    fn counter(&mut self) -> u64 {
        0
    } // bounded instruction fixture, no timer
    fn arm_timer(&mut self, deadline: Option<u64>) {
        assert!(deadline.is_none(), "boot host has no guest timer");
    }
    fn wait_for_interrupt(&mut self) {
        panic!("boot guest must not wait");
    }
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
    let mut host = BootHost {
        ram,
        code: Vec::new(),
        entry: Code::new(&stela_compiler::x86_64::pinned::entry()),
        executions: 0,
        patches: 0,
        faults: 0,
    };
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
    let mut host = BootHost {
        ram,
        code: Vec::new(),
        entry: Code::new(&stela_compiler::x86_64::pinned::entry()),
        executions: 0,
        patches: 0,
        faults: 0,
    };
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
    let mut host = BootHost {
        ram,
        code: Vec::new(),
        entry: Code::new(&stela_compiler::x86_64::pinned::entry()),
        executions: 0,
        patches: 0,
        faults: 0,
    };
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

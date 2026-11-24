//! Eager, bounded shadow mappings for an MMU-on EL1 fixture. Not a sandbox.
//! All table walks/allocation happen outside traps, before native entry.
use super::super::ram::GuestRam;
use super::*;
use stela_engine::machine::system::Register;
use stela_engine::machine::FaultCause;

const PAGE: usize = 4096;
const RAM_BYTES: u64 = 1 << 32;
const GPA: u64 = 0x8000_0000;
const VA: u64 = 0x0040_0000;
const WINDOW: u64 = 2 * 1024 * 1024;
const DATA: u64 = VA + 4096;
const CODE_PA: u64 = GPA + 4 * 4096;
const DATA_PA: u64 = GPA + RAM_BYTES - 4096;
const AF_SH: u64 = (1 << 10) | (3 << 8);

impl GuestRam {
    fn new() -> Self {
        Self::reserve(GPA, RAM_BYTES).expect("guest RAM reservation")
    }
    fn word(&mut self, pa: u64, value: u64) {
        assert!(self.write(pa, &value.to_le_bytes()));
    }
    fn words(&mut self, pa: u64, words: &[u32]) {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert!(self.write(pa, &bytes));
    }
}
struct Tables<'a>(&'a GuestRam);
impl mmu::TableMemory for Tables<'_> {
    fn read_descriptor(&mut self, pa: u64) -> Option<u64> {
        if pa & 7 != 0 {
            return None;
        }
        let mut bytes = [0; 8];
        self.0
            .read(pa, &mut bytes)
            .then(|| u64::from_le_bytes(bytes))
    }
    fn update_descriptor(&mut self, _: u64, _: u64, _: u64) -> Option<bool> {
        None
    }
}

struct Root(u64);
impl Root {
    fn new() -> Self {
        Self(crate::paging::new_address_space())
    }
    fn clear(&mut self) {
        unsafe { crate::paging::unmap_user_nofree(self.0, VA, WINDOW) };
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        assert_ne!(self.0, crate::arch::current_page_table());
        self.clear(); // leaves borrow GuestRam: never free its frames twice
        unsafe { crate::paging::destroy_user_address_space(self.0) };
    }
}

struct ShadowHost {
    root: Root, // remove borrowed leaves before GuestRam drops
    ram: GuestRam,
    inner: BootHost,
    regime: Option<Regime>,
    mapped: usize,
    switches: usize,
    invalidations: usize,
    regimes: Vec<u64>,
}
impl ShadowHost {
    fn translate(&self, address: u64, access: mmu::Access) -> mmu::Outcome {
        let regime = self.regime.expect("shadow regime initialized");
        mmu::translate(
            &regime.controls,
            address,
            access,
            regime.requester,
            &mut Tables(&self.ram),
        )
    }
    fn refresh(&mut self) {
        self.root.clear();
        self.mapped = 0;
        // A fixed 2 MiB aperture, not general demand-filled guest mappings.
        for va in (VA..VA + WINDOW).step_by(PAGE) {
            if let mmu::Outcome::Mapped(t) = self.translate(va, mmu::Access::Read) {
                if self.ram.contains(t.physical & !4095, PAGE) {
                    let host = self
                        .ram
                        .ensure_page(t.physical & !4095)
                        .expect("shadow backing frame");
                    unsafe {
                        crate::paging::boot_shadow_page(self.root.0, va, host as u64, t.writable)
                    };
                    self.mapped += 1;
                }
            }
        }
    }
}
impl Host for ShadowHost {
    fn read_ram(&mut self, pa: u64, out: &mut [u8]) -> bool {
        self.ram.read(pa, out)
    }
    fn write_ram(&mut self, pa: u64, bytes: &[u8]) -> bool {
        self.ram.write(pa, bytes)
    }
    fn install(&mut self, bytes: &[u8]) -> Result<NativeCode, InstallError> {
        self.inner.install(bytes)
    }
    fn patch(&mut self, code: NativeCode, bytes: &[u8]) {
        self.inner.patch(code, bytes)
    }
    fn read_native(&mut self, code: NativeCode, out: &mut [u8]) -> bool {
        self.inner.read_native(code, out)
    }
    fn discard_code(&mut self) {
        self.inner.discard_code()
    }
    unsafe fn execute(&mut self, at: NativeCode, state: &mut GuestState) -> NativeExit {
        assert!(self.inner.code.iter().any(|(c, _)| c.0 as usize == at.0));
        let original = crate::arch::current_page_table();
        assert_eq!(original, crate::paging::kernel_address_space());
        self.inner.executions += 1;
        self.switches += 1;
        crate::arch::set_page_table(self.root.0);
        let returned = super::super::fault::invoke(
            self.inner.entry.0,
            at.0,
            state,
            self.inner
                .code
                .iter()
                .map(|(c, size)| (c.0 as u64, c.0 as u64 + *size as u64)),
            self.inner.ram.0 as u64,
            Some((VA, VA + WINDOW)),
        );
        crate::arch::set_page_table(original);
        match returned {
            Ok(raw) => NativeExit::Exit {
                next_pc: raw as u64,
                kind: (raw >> 64) as u64,
            },
            Err(capture) => {
                self.inner.faults += 1;
                if capture.trap != 14 {
                    return NativeExit::Fault(capture.identity());
                }
                // Fresh software walk after leaving the trap and restoring CR3:
                // never mistake the guest VA for a physical device address.
                let access = if capture.write {
                    mmu::Access::Write
                } else {
                    mmu::Access::Read
                };
                let cause = match self.translate(capture.address, access) {
                    mmu::Outcome::Fault(fault) => FaultCause::Guest(fault),
                    mmu::Outcome::Unsupported(reason) => FaultCause::Translation(reason),
                    mmu::Outcome::Mapped(t) => {
                        assert!(
                            !self.ram.contains(t.physical, 1),
                            "resident shadow RAM unexpectedly faulted"
                        );
                        FaultCause::Device {
                            physical: t.physical,
                        }
                    }
                };
                NativeExit::Fault(capture.native(cause))
            }
        }
    }
    fn placement(&self) -> mmu::placement::Placement {
        self.inner.placement()
    }
    fn set_regime(&mut self, regime: &Regime) {
        assert_ne!(regime.controls.sctlr & 1, 0, "fixture requires MMU on");
        assert!(!regime.controls.stage2 && !regime.tag_checked());
        assert_eq!(regime.requester.el, 1);
        if self.regimes.last() != Some(&regime.controls.ttbr0) {
            self.regimes.push(regime.controls.ttbr0);
        }
        self.regime = Some(*regime);
        self.refresh();
    }
    fn invalidate_translations(&mut self) {
        self.invalidations += 1;
        self.refresh();
    }
    fn counter(&mut self) -> u64 {
        0
    }
    fn arm_timer(&mut self, deadline: Option<u64>) {
        assert!(deadline.is_none());
    }
    fn wait_for_interrupt(&mut self) {
        panic!("shadow fixture must not wait")
    }
}

fn machine<D: stela_engine::machine::Devices>(
    ram: GuestRam,
    ttbr: u64,
    devices: D,
) -> Machine<ShadowHost, D> {
    let inner = BootHost {
        ram: Ram::new(),
        code: Vec::new(),
        entry: Code::new(&stela_compiler::x86_64::pinned::entry()),
        executions: 0,
        patches: 0,
        faults: 0,
    };
    let host = ShadowHost {
        root: Root::new(),
        ram,
        inner,
        regime: None,
        mapped: 0,
        switches: 0,
        invalidations: 0,
        regimes: Vec::new(),
    };
    let chip = stela_contracts::Chip::AppleM2;
    let target = TargetCpu::Baseline;
    let mut cpu = Cpu::of(chip, VA, Pstate::reset(1));
    for (register, value) in [
        (Register::HCR_EL2, (1 << 31) | (1 << 40) | (1 << 41)),
        (Register::SCTLR_EL1, 1 | (1 << 2) | (1 << 12)),
        (
            Register::TCR_EL1,
            17 | (1 << 8) | (1 << 10) | (3 << 12) | (1 << 23) | (5 << 32),
        ),
        (Register::MAIR_EL1, 0xff),
        (Register::CPACR_EL1, 3 << 20),
        (Register::TTBR0_EL1, ttbr),
        (Register::VBAR_EL1, VA + 0x800),
    ] {
        cpu.registers.set_raw(register, 0, value);
    }
    cpu.state.x[1] = DATA;
    let profile = CpuProfile {
        name: "nk MMU-on shadow fixture",
        chip,
        counter_frequency: 24_000_000,
        fixed: identification::derive(chip, target, true, identification::Speculation::NONE),
        processors: Vec::new(),
    };
    Machine::new(cpu, host, devices, profile, target)
}

pub(super) fn probe() {
    crate::println!("STELA_SHADOW_BEGIN");
    sparse_probe();
    let mut ram = GuestRam::new();
    // Four-level 4 KiB ARM page tables. Guest PA, guest VA and native backing
    // are all different, so accidental identity execution cannot pass.
    ram.word(GPA, (GPA + 4096) | 3);
    ram.word(GPA + 4096, (GPA + 8192) | 3);
    ram.word(GPA + 8192 + 2 * 8, (GPA + 12288) | 3);
    ram.word(GPA + 12288, CODE_PA | 3 | AF_SH | (1 << 7) | (1 << 54));
    let data_pte = GPA + 12288 + 8;
    let data_desc = DATA_PA | 3 | AF_SH | (1 << 53) | (1 << 54);
    ram.word(data_pte, data_desc);
    ram.word(DATA_PA, 41);
    ram.words(
        CODE_PA,
        &[0xf940_0020, 0x9100_0400, 0xf900_0020, 0x1400_0001],
    );
    // TLBI VMALLE1IS; DSB ISH; ISB; STR x0,[x1]; B stop.
    ram.words(
        CODE_PA + 0x100,
        &[
            0xd508_831f,
            0xd503_3b9f,
            0xd503_3fdf,
            0xf900_0020,
            0x1400_0001,
        ],
    );
    ram.words(
        CODE_PA + 0x200,
        &[
            0xd508_831f,
            0xd503_3b9f,
            0xd503_3fdf,
            0xf940_0020,
            0x1400_0001,
        ],
    );
    assert_ne!(ram.host(GPA, 1).unwrap() as u64, GPA);
    assert_ne!(ram.host(DATA_PA, 1).unwrap() as u64, DATA);
    assert!(DATA_PA > 1 << 32 && (ram.host(DATA_PA, 1).unwrap() as u64) < 1 << 32);
    let mut machine = machine(ram, GPA, NoDevices);
    machine.set_breakpoints(&[VA + 16]);
    let stop = machine.run(64);
    assert!(
        matches!(stop, Stop::Breakpoint(pc) if pc==VA+16),
        "shadow stopped: {stop:?}"
    );
    assert_eq!(machine.cpu.state.x[0], 42);
    let mut data = [0; 8];
    assert!(machine.host.read_ram(DATA_PA, &mut data));
    assert_eq!(u64::from_le_bytes(data), 42);
    assert_eq!(machine.host.mapped, 2);
    assert!(machine.host.switches > 0);
    assert_eq!(
        crate::arch::current_page_table(),
        crate::paging::kernel_address_space()
    );
    crate::println!(
        "STELA_SHADOW_RAM_OK: GPA above 4GiB, sparse host backing, {} CR3 entries",
        machine.host.switches
    );

    // The guest invalidates the previously writable translation, then its
    // write must fault without changing RAM or the source instruction PC.
    machine.host.ram.word(data_pte, data_desc | (1 << 7));
    let invalidations = machine.host.invalidations;
    machine.cpu.pc = VA + 0x100;
    machine.cpu.state.x[0] = 99;
    let vector = VA + 0xa00;
    machine.set_breakpoints(&[vector, VA + 0x114]);
    let stop = machine.run(128);
    assert!(
        matches!(stop, Stop::Breakpoint(pc) if pc==vector),
        "shadow permission stopped: {stop:?}"
    );
    assert!(
        machine.host.invalidations > invalidations,
        "guest TLBI must reach host"
    );
    assert!(
        machine.host.inner.faults > 0,
        "native write protection must fault"
    );
    assert_eq!(machine.cpu.registers.get(Register::ELR_EL1, 0), VA + 0x10c);
    assert_eq!(machine.cpu.registers.get(Register::FAR_EL1, 0), DATA);
    let esr = machine.cpu.registers.get(Register::ESR_EL1, 0);
    assert_eq!(esr >> 26, 0x25);
    assert_eq!(esr & 63, 15); // level-3 permission fault
    assert!(machine.host.read_ram(DATA_PA, &mut data));
    assert_eq!(u64::from_le_bytes(data), 42);
    assert_eq!(
        crate::arch::current_page_table(),
        crate::paging::kernel_address_space()
    );
    crate::println!("STELA_SHADOW_PERMISSION_OK: TLBI, native protection fault, ARM ELR/FAR/ESR");
    // Removing the ARM leaf and executing TLBI must remove the native
    // mapping too; a stale shadow must not satisfy this load from old RAM.
    machine.host.ram.word(data_pte, 0);
    let invalidations = machine.host.invalidations;
    machine.cpu.pc = VA + 0x200;
    machine.cpu.state.x[0] = 0xbeef;
    machine.set_breakpoints(&[vector, VA + 0x214]);
    let stop = machine.run(192);
    assert!(
        matches!(stop, Stop::Breakpoint(pc) if pc == vector),
        "shadow missing leaf stopped: {stop:?}"
    );
    assert!(machine.host.invalidations > invalidations);
    assert_eq!(machine.host.mapped, 1);
    assert_eq!(machine.cpu.state.x[0], 0xbeef);
    assert_eq!(machine.cpu.registers.get(Register::ELR_EL1, 0), VA + 0x20c);
    assert_eq!(machine.cpu.registers.get(Register::FAR_EL1, 0), DATA);
    assert_eq!(machine.cpu.registers.get(Register::ESR_EL1, 0) & 63, 7);
    assert_eq!(
        crate::arch::current_page_table(),
        crate::paging::kernel_address_space()
    );
    crate::println!(
        "STELA_SHADOW_UNMAP_OK: TLBI removes mapping, original load destination retained"
    );
    drop(machine);
    context_probe();
    mmio_probe();
    crate::println!("STELA_SHADOW_PASS");
}

fn sparse_probe() {
    let mut ram = GuestRam::new();
    assert_eq!(ram.resident_pages(), 0);
    let payload: [u8; 40] = core::array::from_fn(|i| (i as u8).wrapping_mul(7));
    let crossing = GPA + 4096 - 9;
    assert!(ram.write(crossing, &payload));
    let mut copy = [0; 40];
    assert!(ram.read(crossing, &mut copy));
    assert_eq!(copy, payload);
    assert_eq!(ram.resident_pages(), 2);
    assert!(
        ram.host(crossing, payload.len()).is_none(),
        "raw spans may not cross backing frames"
    );
    let mut zero = [0xcc; 17];
    assert!(ram.read(GPA + (1 << 30), &mut zero));
    assert_eq!(zero, [0; 17]);
    assert_eq!(ram.resident_pages(), 2, "reads of holes must not allocate");
    assert!(!ram.write(GPA + RAM_BYTES - 7, &[1; 8]));
    assert!(!ram.write(u64::MAX - 3, &[1; 8]));
    assert_eq!(
        ram.resident_pages(),
        2,
        "rejected writes must not commit a prefix"
    );
    let mut guard = [0x5a; 8];
    assert!(!ram.read(GPA - 1, &mut guard));
    assert_eq!(guard, [0x5a; 8]);
    let mut tail = [0xcc; 7];
    assert!(ram.read(GPA + RAM_BYTES - 7, &mut tail));
    assert_eq!(tail, [0; 7]);
    ram.word(DATA_PA, 0x1234_5678_9abc_def0);
    let mut high = [0; 8];
    assert!(ram.read(DATA_PA, &mut high));
    assert_eq!(u64::from_le_bytes(high), 0x1234_5678_9abc_def0);
    assert_eq!(ram.resident_pages(), 3);
    assert!(GuestRam::reserve(GPA + 1, 4096).is_none());
    assert!(GuestRam::reserve(u64::MAX & !4095, 4096).is_none());
    assert!(GuestRam::reserve(GPA, 0).is_none());
    {
        use stela_contracts::{
            bus::Access,
            dma::{DmaError, Physical, Port},
            interrupt::InterruptLatch,
        };
        let mut iommu = Physical;
        let mut wires = InterruptLatch::new();
        let mut access = Access::new(0, 0, &mut wires);
        let mut port = Port {
            iommu: &mut iommu,
            stream: 0,
            memory: &mut ram,
            access: &mut access,
        };
        assert!(port.check(crossing, 40, true).is_ok());
        port.write(crossing, &[0x6d; 40])
            .expect("model DMA scatter write");
        let mut bytes = [0; 40];
        port.read(crossing, &mut bytes)
            .expect("model DMA scatter read");
        assert_eq!(bytes, [0x6d; 40]);
        let fault = port
            .write_progress(GPA + RAM_BYTES - 4, &[0xa7; 8])
            .expect_err("DMA must stop outside RAM");
        assert_eq!(fault.error, DmaError::Memory);
        assert_eq!(
            fault.completed, 4,
            "DMA must retain the committed page prefix"
        );
    }
    let mut dma_tail = [0; 4];
    assert!(ram.read(GPA + RAM_BYTES - 4, &mut dma_tail));
    assert_eq!(dma_tail, [0xa7; 4]);
    assert_eq!(ram.resident_pages(), 3);
    crate::println!(
        "STELA_RAM_SPARSE_OK: 4GiB reserved, 3 backing pages, cross-page I/O and bounds"
    );
    crate::println!(
        "STELA_RAM_DMA_OK: model port crosses backing pages, failed transfer retains exact prefix"
    );
}

fn mmio_probe() {
    use alloc::{boxed::Box, rc::Rc};
    use core::cell::RefCell;
    use stela_contracts::{bus::Bus, interrupt::InterruptLatch};
    const DEVICE_VA: u64 = VA + 2 * 4096;
    let output = Rc::new(RefCell::new(Vec::new()));
    let mut bus = Bus::new();
    let uart = bus.add(Box::new(stela_soc::uart::S5lUart::new(
        Console(output.clone()),
        None,
    )));
    bus.map(uart, UART, 0x100, 0).expect("translated UART map");
    let devices = UartDevices {
        bus,
        wires: InterruptLatch::new(),
        reads: 0,
        writes: 0,
    };
    let mut ram = GuestRam::new();
    ram.word(GPA, (GPA + 4096) | 3);
    ram.word(GPA + 4096, (GPA + 8192) | 3);
    ram.word(GPA + 8192 + 2 * 8, (GPA + 12288) | 3);
    ram.word(GPA + 12288, CODE_PA | 3 | AF_SH | (1 << 7) | (1 << 54));
    // AttrIdx 1 selects MAIR's zero byte: Device-nGnRnE. UART is not RAM
    // and receives no native leaf. Its guest VA must fault and translate.
    ram.word(
        GPA + 12288 + 2 * 8,
        UART | 3 | (1 << 2) | AF_SH | (1 << 53) | (1 << 54),
    );
    ram.words(
        CODE_PA,
        &[
            0x4ea2_1c20,
            0xb940_10a9,
            0xb900_20a6,
            0xb900_20a7,
            0xb900_20a8,
            0x1400_0001,
        ],
    );
    let mut machine = machine(ram, GPA, devices);
    machine.cpu.state.x[5] = DEVICE_VA;
    machine.cpu.state.x[6] = b'N' as u64;
    machine.cpu.state.x[7] = b'K' as u64;
    machine.cpu.state.x[8] = b'\n' as u64;
    machine.cpu.state.v[1] = [0x00ff_00ff_00ff_00ff; 2];
    machine.cpu.state.v[2] = [0xff00_ff00_ff00_ff00; 2];
    let end = VA + 24;
    machine.set_breakpoints(&[end]);
    let stop = machine.run(64);
    assert!(
        matches!(stop, Stop::Breakpoint(pc) if pc == end),
        "MMU-on UART stopped: {stop:?}"
    );
    assert_ne!(DEVICE_VA, UART);
    assert_eq!(&*output.borrow(), b"NK\n");
    assert_eq!((machine.devices.reads, machine.devices.writes), (1, 3));
    assert_eq!(machine.cpu.state.x[9] & 6, 6);
    assert_eq!(machine.cpu.state.v[0], [u64::MAX; 2]);
    assert_eq!(
        machine.host.mapped, 1,
        "device memory must not gain a RAM shadow leaf"
    );
    assert!(machine.host.inner.faults > 0);
    assert_eq!(
        crate::arch::current_page_table(),
        crate::paging::kernel_address_space()
    );
    crate::println!(
        "STELA_SHADOW_MMIO_OK: translated UART VA to PA, exact output and vector recovery"
    );
}

fn context_probe() {
    let mut ram = GuestRam::new();
    let root_b = GPA + 7 * 4096;
    let code_b = GPA + 11 * 4096;
    let data_b = GPA + 6 * 4096;
    let ttbr_a = GPA | (1 << 48);
    let ttbr_b = root_b | (2 << 48);
    // Separate non-global code/data in each address space at identical VAs.
    for (root, code, data) in [(GPA, CODE_PA, DATA_PA), (root_b, code_b, data_b)] {
        ram.word(root, (root + 4096) | 3);
        ram.word(root + 4096, (root + 8192) | 3);
        ram.word(root + 8192 + 2 * 8, (root + 12288) | 3);
        ram.word(
            root + 12288,
            code | 3 | AF_SH | (1 << 7) | (1 << 11) | (1 << 54),
        );
        ram.word(
            root + 12288 + 8,
            data | 3 | AF_SH | (1 << 11) | (1 << 53) | (1 << 54),
        );
    }
    ram.word(DATA_PA, 41);
    ram.word(data_b, 100);
    for (code, add) in [(CODE_PA, 0x9100_0400), (code_b, 0x9100_2800)] {
        // Different native bodies at the same PC must never share a stale
        // compiled source entry after a guest TTBR/ASID switch.
        ram.words(code, &[0xf940_0020, add, 0xf900_0020, 0x1400_0001]);
        // MSR TTBR0_EL1,x10; ISB; B VA. Distinct ASIDs need no TLBI.
        ram.words(
            code + 0x300,
            &[
                0xd518_200a,
                0xd503_3fdf,
                0x1400_0000 | ((-(0x308i32 / 4)) as u32 & 0x03ff_ffff),
            ],
        );
        // Reusing an ASID for another root requires an explicit invalidate.
        ram.words(
            code + 0x340,
            &[
                0xd518_200a,
                0xd503_3fdf,
                0xd508_831f,
                0xd503_3b9f,
                0xd503_3fdf,
                0x1400_0000 | ((-(0x354i32 / 4)) as u32 & 0x03ff_ffff),
            ],
        );
    }
    let mut machine = machine(ram, ttbr_a, NoDevices);
    machine.set_breakpoints(&[VA + 16]);
    let mut a = 41u64;
    let mut b = 100u64;
    // First A entry, then alternate B/A. Switching itself is performed by
    // translated ARM MSR instructions, not by editing host CPU registers.
    let sequence = [
        (ttbr_a, false),
        (ttbr_b, false),
        (ttbr_a, false),
        (ttbr_b, false),
        (ttbr_a, false),
        (ttbr_b, false),
        (ttbr_a, false),
        (root_b | (1 << 48), true),
        (ttbr_a, true),
    ];
    for (index, (ttbr, reuse)) in sequence.into_iter().enumerate() {
        let invalidations = machine.host.invalidations;
        if index != 0 {
            machine.cpu.pc = VA + if reuse { 0x340 } else { 0x300 };
            machine.cpu.state.x[10] = ttbr;
        }
        let stop = machine.run((index as u64 + 1) * 64);
        assert!(
            matches!(stop, Stop::Breakpoint(pc) if pc == VA + 16),
            "context switch stopped: {stop:?}"
        );
        let is_a = ttbr & 0x0000_ffff_ffff_f000 == GPA;
        if is_a {
            a += 1
        } else {
            b += 10
        }
        assert_eq!(machine.cpu.state.x[0], if is_a { a } else { b });
        assert_eq!(machine.cpu.registers.get(Register::TTBR0_EL1, 0), ttbr);
        assert_eq!(machine.host.regimes.last(), Some(&ttbr));
        if reuse {
            assert!(machine.host.invalidations > invalidations);
        }
        for (pa, value) in [(DATA_PA, a), (data_b, b)] {
            let mut bytes = [0; 8];
            assert!(machine.host.read_ram(pa, &mut bytes));
            assert_eq!(
                u64::from_le_bytes(bytes),
                value,
                "address space writes crossed"
            );
        }
        assert_eq!(
            crate::arch::current_page_table(),
            crate::paging::kernel_address_space()
        );
    }
    assert_eq!((a, b), (46, 140));
    assert_eq!(machine.host.regimes.len(), sequence.len());
    crate::println!(
        "STELA_SHADOW_CONTEXT_OK: guest TTBR/ASID switches, distinct code/data at same VAs"
    );
    crate::println!(
        "STELA_SHADOW_ASID_REUSE_OK: explicit TLBI, no stale code/data or cross writes"
    );
}

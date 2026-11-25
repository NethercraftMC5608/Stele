//! Per-CPU, interrupt-masked recovery from an owned native invocation.
//! The trap hook never allocates from the heap or calls the engine or Linux.
use crate::interrupts_x86::TrapFrame;
use core::sync::atomic::{AtomicPtr, Ordering};
use stela_compiler::{
    block::{HostGprSnapshot, HostSpillSnapshot},
    GuestState,
};
use stela_engine::machine::{FaultCause, NativeFault, NativeFaultOrigin};

#[derive(Debug, Clone, Copy)]
pub(super) struct Captured {
    pub address: u64,
    pub write: bool,
    pub trap: u16,
    offset: usize,
    error_code: u64,
    registers: HostGprSnapshot,
}
impl Captured {
    pub(super) fn native(self, cause: FaultCause) -> NativeFault {
        NativeFault {
            origin: NativeFaultOrigin::Exception,
            offset: self.offset,
            address: self.address,
            write: self.write,
            cause,
            trap: self.trap,
            error_code: self.error_code,
            spills: HostSpillSnapshot::new(),
            registers: self.registers,
        }
    }
    pub(super) fn identity(self) -> NativeFault {
        let cause = match self.trap {
            14 => FaultCause::Device {
                physical: self.address,
            },
            6 => FaultCause::InvalidOpcode,
            _ => FaultCause::NonCanonical,
        };
        self.native(cause)
    }
}

struct Recovery {
    stack: u64,
    entry: u64,
    ram: u64,
    ranges: [(u64, u64); 64],
    count: usize,
    fault: Option<Captured>,
    shadow_window: Option<(u64, u64)>,
    fill: Option<Filler>,
}

/// Shadow operations invoked in the #PF path with interrupts masked.
pub(super) struct Filler {
    pub context: *mut (),
    /// A not-present access can retry once its page is mapped.
    pub fill: unsafe fn(*mut (), u64, bool) -> bool,
    /// A protection fault can reach the ARM walker only on a guest leaf.
    pub owns: unsafe fn(*mut (), u64) -> bool,
}
static ACTIVE: [AtomicPtr<Recovery>; crate::sched::MAX_CPUS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; crate::sched::MAX_CPUS];

/// # Safety
/// The caller stays on the same CPU with interrupts masked. All extents
/// are immutable RX storage kept alive through the invocation and trap return.
pub(super) unsafe fn invoke(
    thunk: *const u8,
    entry: usize,
    state: &mut GuestState,
    ranges: impl Iterator<Item = (u64, u64)>,
    ram: u64,
    shadow_window: Option<(u64, u64)>,
) -> Result<u128, Captured> {
    invoke_filling(thunk, entry, state, ranges, ram, shadow_window, None)
}

/// [`invoke`] with a demand shadow fill for not-present faults.
pub(super) unsafe fn invoke_filling(
    thunk: *const u8,
    entry: usize,
    state: &mut GuestState,
    ranges: impl Iterator<Item = (u64, u64)>,
    ram: u64,
    shadow_window: Option<(u64, u64)>,
    fill: Option<Filler>,
) -> Result<u128, Captured> {
    let flags: u64;
    core::arch::asm!("pushfq", "pop {}", out(reg) flags);
    assert_eq!(
        flags & (1 << 9),
        0,
        "native boot recovery requires IF masked"
    );
    let mut recovery = Recovery {
        stack: 0,
        entry: entry as u64,
        ram,
        ranges: [(0, 0); 64],
        count: 0,
        fault: None,
        shadow_window,
        fill,
    };
    for range in ranges {
        assert!(recovery.count < recovery.ranges.len());
        recovery.ranges[recovery.count] = range;
        recovery.count += 1;
    }
    let active = &ACTIVE[crate::arch::cpu_id()];
    assert!(
        active
            .compare_exchange(
                core::ptr::null_mut(),
                &raw mut recovery,
                Ordering::AcqRel,
                Ordering::Acquire
            )
            .is_ok(),
        "nested native boot entry"
    );
    let raw = super::host::nk_stela_system_enter(thunk, entry, state, &raw mut recovery.stack);
    active.store(core::ptr::null_mut(), Ordering::Release);
    match recovery.fault {
        Some(fault) => Err(fault),
        None => Ok(raw),
    }
}

/// An unrelated kernel/user fault always follows nk's ordinary trap path.
/// # Safety
/// `fp` is this trap's complete 512-byte FXSAVE image from isr_common.
pub(crate) unsafe fn recover_native_fault(f: &mut TrapFrame, fp: *const u8) -> bool {
    if f.cs & 3 != 0 || !matches!(f.vector, 6 | 13 | 14) {
        return false;
    }
    let Some(active) = ACTIVE[crate::arch::cpu_id()].load(Ordering::Acquire).as_mut() else {
        return false;
    };
    if active.stack == 0
        || active.fault.is_some()
        || !active.ranges[..active.count]
            .iter()
            .any(|&(s, e)| s <= f.rip && f.rip < e)
    {
        return false;
    }
    // Only supervisor not-present data reads/writes belong to this MMIO
    // fixture. Present/protection, reserved-bit, fetch and key faults are
    // host failures and must never be projected onto a device.
    let address = if f.vector == 14 {
        let cr2: u64;
        core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
        cr2
    } else {
        0
    };
    // A not-present data access the host's shadow can fill: map it and
    // retry the instruction natively, as the host's own #PF gate would.
    if f.vector == 14 && f.error & 0b1_1001 == 0 {
        if let Some(filler) = &active.fill {
            if (filler.fill)(filler.context, address, f.error & 2 != 0) {
                return true;
            }
        }
    }
    let aperture = active
        .shadow_window
        .is_some_and(|(s, e)| s <= address && address < e);
    if f.vector == 14 {
        // Only the separately owned shadow aperture can project a present
        // protection fault into an ARM permission fault, after a fresh walk.
        if active.shadow_window.is_some() && !aperture {
            return false;
        }
        let shadow = aperture
            || (f.error == 3
                && active.fill.as_ref().is_some_and(|filler| (filler.owns)(filler.context, address)));
        if f.error & !(if shadow { 3 } else { 2 }) != 0 {
            return false;
        }
    }
    // The probe has one fully resident identity RAM page. Its faults are
    // host failures. Only non-RAM data accesses can reach guest devices.
    if f.vector == 14 && (active.ram..active.ram + 4096).contains(&address) {
        return false;
    }
    let registers = [
        f.rax, f.rcx, f.rdx, f.rbx, f.rsp, f.rbp, f.rsi, f.rdi, f.r8, f.r9, f.r10, f.r11, f.r12,
        f.r13, f.r14, f.r15,
    ];
    let vectors = core::array::from_fn(|n| {
        let xmm = fp.add(160 + (n + 8) * 16) as *const [u64; 2];
        core::ptr::read_unaligned(xmm)
    });
    active.fault = Some(Captured {
        offset: f.rip.wrapping_sub(active.entry) as usize,
        address,
        write: f.error & 2 != 0,
        trap: f.vector as u16,
        error_code: f.error,
        // No spill lease or extended bank is invented: unsupported recovery
        // rows must refuse rather than restore fabricated register values.
        registers: HostGprSnapshot::legacy(registers).with_vectors(vectors),
    });
    f.rdi = active.stack;
    f.rsp = active.stack;
    f.rip = super::host::nk_stela_system_recover as *const () as u64;
    true
}

//! The x86-64 Task State Segment.
//!
//! Long mode keeps almost nothing of the 32-bit TSS -- there is no hardware
//! task switching any more -- but it keeps the one field that matters here:
//! RSP0, the stack the CPU switches to when an interrupt raises the privilege
//! level from ring 3 to ring 0.
//!
//! Without a TSS, an interrupt taken in ring 3 has nowhere to go. The CPU does
//! not "keep using the current stack": for a privilege change it *must* load a
//! new RSP from the TSS, and with no TSS loaded that is a fault while handling
//! a fault, which is a double fault, and then a triple fault -- the machine
//! resets with no output at all. That reset is indistinguishable from a hang,
//! which is why this exists before ring 3 is attempted rather than after the
//! first mysterious reboot.
//!
//! aarch64 needs no equivalent: SP_EL1 is a separate register the CPU always
//! has, so an exception from EL0 lands on the kernel stack automatically.

/// The long-mode TSS. 104 bytes+ of mostly-reserved fields; only the privilege
/// stacks, the IST entries and the I/O map base are live.
#[repr(C, packed)]
pub struct Tss {
    reserved0: u32,
    /// The stacks for ring 0, 1 and 2. Only rsp0 is used.
    pub rsp0: u64,
    pub rsp1: u64,
    pub rsp2: u64,
    reserved1: u64,
    /// The Interrupt Stack Table: seven stacks a gate can demand
    /// unconditionally, even without a privilege change. The right home for
    /// double-fault and NMI handlers, which cannot trust the current stack.
    pub ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    /// Offset of the I/O permission bitmap. Set past the limit to mean "no
    /// bitmap", so every ring-3 port access faults rather than reading
    /// whatever follows the structure.
    pub iomap_base: u16,
}

impl Tss {
    pub const fn new() -> Self {
        Tss {
            reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            // sizeof(Tss) == 104: no bitmap present.
            iomap_base: 104,
        }
    }
}

pub const MAX_CPUS: usize = 8;

static mut TSS: [Tss; MAX_CPUS] = [Tss::new(), Tss::new(), Tss::new(), Tss::new(),
    Tss::new(), Tss::new(), Tss::new(), Tss::new()];

/// A stack for exceptions that cannot trust the one they interrupted.
///
/// A double fault raised *because* the stack pointer is bad cannot push its
/// frame onto that same stack; it would fault again and triple-fault. IST1
/// points here so the handler always has somewhere to land. Per CPU: two
/// CPUs double-faulting at once on one stack would corrupt each other.
#[repr(align(16))]
struct IstStack([u8; 8192]);
static mut DF_STACK: [IstStack; MAX_CPUS] = [IstStack([0; 8192]), IstStack([0; 8192]),
    IstStack([0; 8192]), IstStack([0; 8192]),
    IstStack([0; 8192]), IstStack([0; 8192]),
    IstStack([0; 8192]), IstStack([0; 8192])];

unsafe extern "C" {
    /// The 16-byte TSS slots in the GDT, from boot-x86_64.s. One per CPU:
    /// `ltr` loads one TSS per CPU, and two CPUs sharing a descriptor would
    /// share RSP0.
    static mut gdt_tss: [[u64; 2]; MAX_CPUS];
}

#[inline]
fn cpu() -> usize {
    crate::arch::cpu_id().min(MAX_CPUS - 1)
}

/// Build the TSS descriptor, load it, and point RSP0 at `kernel_stack_top`.
///
/// # Safety
/// The GDT must be loaded and `kernel_stack_top` must be the top of a stack
/// that stays valid for as long as ring 3 can be entered.
pub unsafe fn init(kernel_stack_top: u64) {
    init_cpu(crate::arch::cpu_id(), kernel_stack_top);
}

/// Build one CPU's TSS descriptor, load it, and point its RSP0 at the stack.
///
/// # Safety
/// The GDT must be loaded and `kernel_stack_top` must stay valid.
pub unsafe fn init_cpu(cpu: usize, kernel_stack_top: u64) {
    let cpu = cpu.min(MAX_CPUS - 1);
    let base = &raw const TSS[cpu] as u64;
    let limit = (core::mem::size_of::<Tss>() - 1) as u64;

    TSS[cpu].rsp0 = kernel_stack_top;
    TSS[cpu].ist[0] = (&raw const DF_STACK[cpu] as u64) + 8192;

    // A 64-bit system descriptor, split across two quads in the peculiar
    // order the architecture inherited from the 286: the base is in three
    // pieces and the limit in two.
    //
    // type 0x9 = available 64-bit TSS, P = present.
    let low = (limit & 0xffff)
        | ((base & 0xff_ffff) << 16)
        | (0x9 << 40)              // type: available 64-bit TSS
        | (1 << 47)                // present
        | (((limit >> 16) & 0xf) << 48)
        | (((base >> 24) & 0xff) << 56);
    let high = (base >> 32) & 0xffff_ffff;

    gdt_tss[cpu][0] = low;
    gdt_tss[cpu][1] = high;

    // `ltr` takes the selector of the descriptor, which is its byte offset in
    // the GDT: the first TSS sits after five eight-byte entries at 0x28, and
    // each further CPU's descriptor is 16 bytes later.
    let sel = (0x28 + cpu * 16) as u16;
    core::arch::asm!("ltr {0:x}", in(reg) sel, options(nomem, nostack));

    // Read back through a raw pointer: TSS is `repr(packed)`, so borrowing a
    // field to pass it by reference is an unaligned reference, which is UB
    // regardless of whether this particular field happens to be aligned.
    let ist1 = core::ptr::read_unaligned(&raw const TSS[cpu].ist[0]);
    crate::println!("  tss:    cpu{} loaded, rsp0 {:#018x}, ist1 {:#018x}",
        cpu, kernel_stack_top, ist1);
}

/// Point RSP0 at a different stack.
///
/// Called on every switch to a task that can enter ring 3: RSP0 has to name
/// *that* task's kernel stack, or its first syscall would land on the previous
/// task's stack and corrupt it.
///
/// # Safety
/// `stack_top` must be the top of the incoming task's kernel stack.
#[inline]
pub unsafe fn set_kernel_stack(stack_top: u64) {
    TSS[cpu()].rsp0 = stack_top;
}

/// Read RSP0 back. Only the self-test uses it: it is the one way to tell that
/// a switch really did repoint the trap stack.
pub unsafe fn kernel_stack() -> u64 {
    core::ptr::read_unaligned(&raw const TSS[cpu()].rsp0)
}

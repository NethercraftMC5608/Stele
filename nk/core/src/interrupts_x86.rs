//! x86-64 interrupts: the IDT, the 8259 PIC, and the timer tick.
//!
//! The PC equivalent of `gic.rs` and `exceptions.rs` together. On aarch64 the
//! vector table is in boot.s and the GIC routes IRQs; on x86-64 the CPU
//! expects an Interrupt Descriptor Table (IDT) loaded with `lidt`, and the
//! legacy PC routes device interrupts through the dual 8259 PIC.
//!
//! APIC is what modern PCs use; the 8259 PIC is what every x86 VM and PC
//! starts with, and it is 80 lines of port I/O against several hundred for an
//! xAPIC/x2APIC driver. It is the minimum that makes timer and keyboard
//! interrupts arrive.

// ------------------------------------------------------------- 8259 PIC --
//
// Two cascaded 8259 controllers: master at port 0x20, slave at 0xa0. Out of
// reset the PC BIOS maps them to interrupt vectors 0x08-0x0f, which on a
// 32-bit or 64-bit CPU collides with the CPU's own exceptions (double fault is
// 8, GPF is 13). So they are remapped to vectors 0x20-0x2f (32-47), well above
// the 32 architectural exception vectors.

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xa0;
const PIC2_DATA: u16 = 0xa1;

const PIC_EOI: u8 = 0x20;

#[inline]
unsafe fn outb(port: u16, val: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
}

#[inline]
unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    core::arch::asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags));
    v
}

/// Remap the PIC to vectors 32..47 and mask all lines except the timer.
pub unsafe fn pic_init() {
    // ICW1: begin initialisation, edge-triggered, cascade mode, ICW4 needed
    outb(PIC1_CMD, 0x11);
    outb(PIC2_CMD, 0x11);
    // ICW2: vector offsets (master 32, slave 40)
    outb(PIC1_DATA, 32);
    outb(PIC2_DATA, 40);
    // ICW3: master has slave on IRQ2; slave cascade identity is 2
    outb(PIC1_DATA, 0x04);
    outb(PIC2_DATA, 0x02);
    // ICW4: 8086 mode
    outb(PIC1_DATA, 0x01);
    outb(PIC2_DATA, 0x01);

    // Mask everything except IRQ0 (timer) and IRQ4 (COM1).
    // IRQ2 (cascade to slave) must stay unmasked if any slave line is used.
    outb(PIC1_DATA, !(1 << 0 | 1 << 4));
    outb(PIC2_DATA, 0xff); // all slave IRQs masked for now
}

pub unsafe fn pic_eoi(irq: u8) {
    if irq >= 8 {
        outb(PIC2_CMD, PIC_EOI);
    }
    outb(PIC1_CMD, PIC_EOI);
}

/// Caller serializes PIC changes with local interrupts masked.
pub unsafe fn pic_mask(irq: u8, masked: bool) {
    assert!(irq < 16);
    let port = if irq < 8 {PIC1_DATA} else {PIC2_DATA};
    let bit = 1 << (irq%8);
    let current = inb(port);
    outb(port,if masked {current|bit} else {current&!bit});
}

// ------------------------------------------------------------------ IDT --
//
// 256 entries, each 16 bytes. Descriptors are 64-bit interrupt gates:
// target offset split across four fields, selector 0x08 (kernel code),
// flags 0x8e (present, ring 0, 64-bit interrupt gate: clears IF on entry).

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    zero: u32,
}

impl IdtEntry {
    const fn missing() -> Self {
        IdtEntry {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            zero: 0,
        }
    }

    fn new(handler: usize) -> Self {
        Self::with_ist(handler, 0)
    }

    /// A gate that switches to an Interrupt Stack Table entry.
    ///
    /// `ist` is 1..=7 to select a stack unconditionally, or 0 to keep the
    /// current one. Used for the double fault, whose usual cause is a stack
    /// that cannot be pushed to.
    fn with_ist(handler: usize, ist: u8) -> Self {
        IdtEntry {
            offset_low: handler as u16,
            selector: 0x08, // kernel code from GDT in boot-x86_64.s
            ist,
            type_attr: 0x8e, // present, ring 0, interrupt gate
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            zero: 0,
        }
    }
}

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

static mut IDT: [IdtEntry; 256] = [IdtEntry::missing(); 256];

unsafe extern "C" {
    /// 256 entry stubs from interrupts-x86_64.s, one per vector.
    static isr_table: [usize; 256];
}

/// The register state an interrupt stub saves, in the order it pushes them.
///
/// This must match the layout documented in interrupts-x86_64.s exactly: the
/// stub pushes r15 last, so r15 is at the lowest address and therefore the
/// first field.
#[repr(C)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    /// Which vector arrived; pushed by the stub, not the CPU.
    pub vector: u64,
    /// The CPU's error code, or a zero the stub supplied.
    pub error: u64,
    // Below here is what the CPU itself pushed.
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// The names of the architectural exceptions, for the report.
const EXCEPTION_NAMES: [&str; 32] = [
    "divide error", "debug", "NMI", "breakpoint",
    "overflow", "bound range exceeded", "invalid opcode", "device not available",
    "double fault", "coprocessor segment overrun", "invalid TSS", "segment not present",
    "stack-segment fault", "general protection fault", "page fault", "reserved",
    "x87 floating-point", "alignment check", "machine check", "SIMD floating-point",
    "virtualisation", "control protection", "reserved", "reserved",
    "reserved", "reserved", "reserved", "hypervisor injection",
    "VMM communication", "security", "reserved", "reserved",
];

/// Every interrupt and exception arrives here from the assembly stubs.
///
/// # Safety
/// Called only from `isr_common` with a valid frame on the current stack.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_trap(frame: *mut TrapFrame, fp: *const u8) {
    let f = &mut *frame;
    #[cfg(feature = "stela")]
    if crate::stela_nk::recover_native_fault(f, fp) {
        return;
    }
    #[cfg(not(feature = "stela"))]
    let _ = fp;
    match f.vector {
        // IRQ 0: the timer.
        32 | 0x40 => {
            // EOI before any switch, not after. `cpu_switch` does not return
            // here -- it returns into some other task -- so an EOI placed
            // after it is never reached, the PIC still considers IRQ 0 in
            // service, and the timer ticks exactly once. This is the same
            // trap the aarch64 path documents at its `gic::eoi`.
            if f.vector == 32 { pic_eoi(0); } else { crate::lapic::eoi(); }
            crate::timer::on_tick();
            crate::hostops::tick_timers();

            // Preemption, which this path did not do until the scheduler
            // could describe *when* to preempt rather than just switching on
            // every tick.
            //
            // Switching from inside the interrupt is safe here because of
            // where the frame lives. `isr_common` builds the TrapFrame on
            // the kernel stack of the task that was interrupted -- from ring
            // 3 via TSS.RSP0, from ring 0 on the stack already in use -- and
            // that stack is per task. `cpu_switch` saves RSP into the
            // outgoing task's slot with the frame still sitting on it, so
            // when the task is picked again it returns through `schedule`
            // into `x86_trap`, pops that same frame and `iretq`s to exactly
            // where it was. The outgoing state being a TrapFrame rather than
            // a `cpu_switch` frame does not matter, because nothing unwinds
            // it: it is resumed, not reconstructed.
            //
            // The one stack this must not happen on is IST1, which is shared
            // rather than per task -- but IST1 is only vector 8, and a double
            // fault is not something to reschedule out of.
            if crate::sched::preempt_enabled()
                && (crate::sched::take_need_resched() || crate::sched::slice_expired())
            {
                crate::sched::schedule();
            }
        }
        // IRQ 4: COM1. Drained but not dispatched; the console is polled.
        36 => {
            pic_eoi(4);
        }
        // LAPIC timer on secondary CPUs: local preemption only.
        //
        // The global clock stays on the BSP's PIT. `timer::on_tick` bumps
        // the shared millisecond counter and wakes sleepers, and
        // `hostops::tick_timers` walks one TIMERS table; running either
        // from every CPU would advance `TICKS` at N× the real rate and
        // have four CPUs mutating one table with no lock. So this handler
        // does the one thing a per-CPU tick is for: decide whether the
        // task *this* CPU is running has had enough. `slice_expired`
        // compares against `TASKS[cur].ran_at`, which is this CPU's own
        // current task, and `take_need_resched` reads this CPU's flag --
        // both local, neither touches another CPU's state.
        //
        // EOI before the switch, as in the PIT path: cpu_switch does not
        // return here, so a late EOI would leave the LAPIC in service and
        // this CPU would tick exactly once.
        0xef => {
            crate::lapic::eoi();
            if crate::sched::preempt_enabled()
                && (crate::sched::take_need_resched() || crate::sched::slice_expired())
            {
                crate::sched::schedule();
            }
        }
        // High-resolution one-shot timer on a dedicated core (hrtimer.rs).
        // EOI first; the callback is bounded and does not switch.
        // On CPU 0 it carries Linux's one-shots (hostops): the callback may
        // have woken the timer thread, so act on need_resched as the tick
        // does. Never on a dedicated core, whose guest may be mid-WFI.
        0xee => {
            crate::lapic::eoi();
            crate::hrtimer::fire();
            if crate::arch::cpu_id() == 0
                && crate::sched::preempt_enabled()
                && crate::sched::take_need_resched()
            {
                crate::sched::schedule();
            }
        }
        // Reschedule IPI from another CPU (LAPIC vector 0xf0). LAPIC EOI,
        // not PIC: this interrupt never went through the 8259. The sender
        // already set this CPU's need_resched; act on it at the same safe
        // point a local wakeup uses.
        0xf0 => {
            crate::lapic::eoi();
            if crate::sched::preempt_enabled() && crate::sched::take_need_resched() {
                crate::sched::schedule();
            }
        }
        // Device MSI/MSI-X and explicitly routed IOAPIC interrupts. Device
        // callbacks acknowledge their source before EOI; LKL work runs in
        // its IRQ pump rather than blocking this interrupt frame.
        0x50..=0xdf => {
            crate::device_irq_x86::dispatch(f.vector as u8);
            crate::lapic::eoi();
        }
        // The remaining PIC lines: acknowledge so the controller does not
        // wedge on an unacknowledged interrupt.
        33..=47 => {
            pic_eoi((f.vector - 32) as u8);
        }
        // Breakpoint. The CPU has already advanced RIP past the one-byte
        // `int3`, so returning resumes at the next instruction; this is what
        // the self-test counts.
        3 => {
            BREAKPOINT_HITS += 1;
        }
        // Page fault. A *not-present* fault from ring 3 inside a reservation
        // is the first touch of a page the program asked for and nk has not
        // put a frame under yet -- anonymous mmap is demand-paged. Mapping it
        // and returning retries the instruction, and the program never learns
        // anything happened. This is the x86 half of what `rust_el0_sync`
        // does on aarch64; without it every anonymous mapping faults on its
        // first write, which is musl's first malloc.
        //
        // Only not-present (error bit 0 clear) and only from user mode (bit
        // 2 set): a protection fault is a real permission violation, and a
        // kernel fault here is nk's own bug and must still be reported.
        14 if f.error & 1 == 0 && f.error & 4 != 0 => {
            let cr2: u64;
            core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
            if crate::reserve::fault_in(cr2) {
                return;
            }
            let mut saved = crate::user::Frame {
                x: [0; 31], elr: f.rip, spsr: f.rflags, sp: f.rsp,
            };
            saved.x[..16].copy_from_slice(&[
                f.rax, f.rsi, f.rdx, f.r10, f.r8, f.r9, f.rbx, f.rbp,
                0, f.r12, f.r13, f.r14, f.r15, f.rdi, f.rcx, f.r11,
            ]);
            if crate::reserve::defer_file_fault(&mut saved, cr2) {
                f.rax = saved.x[8];
                f.rip = saved.elr;
                return;
            }
            report_trap(f, 14);
        }
        v => report_trap(f, v),
    }
}

/// Say what went wrong and stop. Never returns: every caller has already
/// decided the fault is not one nk can repair.
unsafe fn report_trap(f: &TrapFrame, v: u64) -> ! {
    let name = if v < 32 { EXCEPTION_NAMES[v as usize] } else { "unexpected vector" };
    crate::println!("\n!! x86-64 exception {}: {}", v, name);
    let (cur, _nr, _on) = crate::sched::current_cpu_state(crate::arch::cpu_id());
    if cur < crate::sched::MAX_TASKS {
        let (st, what, on_cpu, home) = crate::sched::task_probe(cur);
        crate::println!("   cpu {} task {} '{}' st {} what {} on_cpu {} home {} stack {:#x}",
            crate::arch::cpu_id(), cur, crate::sched::task_name(cur),
            st, what, on_cpu, home, crate::sched::task_stack_of(cur));
    } else {
        crate::println!("   cpu {} task {} (none)", crate::arch::cpu_id(), cur);
    }
    crate::println!("   error {:#x}  rip {:#018x}  cs {:#x}  rflags {:#x}",
        f.error, f.rip, f.cs, f.rflags);
    crate::println!("   rsp {:#018x}  ss {:#x}", f.rsp, f.ss);
    // A page fault says which address it was, in cr2 -- the single most
    // useful fact about it, and it is nowhere in the frame.
    if v == 14 {
        let cr2: u64;
        core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
        crate::println!("   cr2 {:#018x}  ({}{}{})", cr2,
            if f.error & 1 == 0 { "not present" } else { "protection" },
            if f.error & 2 != 0 { ", write" } else { ", read" },
            if f.error & 4 != 0 { ", user" } else { ", kernel" });
        let (inside, below, above, live) = crate::reserve::describe(cr2);
        crate::println!("   reserve: inside {} last_end {:#x} next_start {:#x} live {}",
            inside, below, above, live);
    }
    // Raw words at the failing stack pointer. A frame shifted by one word
    // -- the classic entry/exit asymmetry -- shows up here immediately: the
    // words that should be RIP/CS/RFLAGS are visibly one slot off, and the
    // value being executed is recognisable as a stub's own vector number.
    if f.rsp != 0 {
        let p = f.rsp as *const u64;
        let w = |i: isize| core::ptr::read_unaligned(p.offset(i));
        crate::println!("   stack from rsp: {:#x} {:#x} {:#x} {:#x} {:#x} {:#x}",
            w(0), w(1), w(2), w(3), w(4), w(5));
    }
    crate::println!("   rax {:#018x} rbx {:#018x} rcx {:#018x}", f.rax, f.rbx, f.rcx);
    crate::println!("   rdx {:#018x} rsi {:#018x} rdi {:#018x}", f.rdx, f.rsi, f.rdi);
    crate::halt()
}

pub unsafe fn idt_init() {
    for i in 0..256 {
        IDT[i] = IdtEntry::new(isr_table[i]);
    }
    // Vector 8 is the double fault, and it is reached precisely when the CPU
    // could not deliver another exception -- very often because the stack was
    // unusable. Handling it on that same stack faults again, which is the
    // triple fault that resets the machine with no output. IST1 gives it a
    // stack that is known good.
    IDT[8] = IdtEntry::with_ist(isr_table[8], 1);

    let ptr = IdtPointer {
        limit: (core::mem::size_of::<[IdtEntry; 256]>() - 1) as u16,
        base: &raw const IDT as u64,
    };
    core::arch::asm!("lidt [{}]", in(reg) &ptr, options(nostack));
}

/// How many times vector 3 has been taken; the self-test reads this to tell
/// "the handler ran" from "nothing happened".
static mut BREAKPOINT_HITS: u64 = 0;

/// Raise a breakpoint and confirm the handler ran and returned.
///
/// # Safety
/// Requires the IDT to be loaded.
pub unsafe fn selftest() {
    let before = core::ptr::read_volatile(&raw const BREAKPOINT_HITS);
    // Deliberately *not* `options(nomem)`. The handler this instruction
    // reaches writes BREAKPOINT_HITS, so telling the compiler the asm touches
    // no memory is a lie -- and it acted on it, caching the counter across the
    // trap and reporting "handler did not run" for a handler that had run.
    core::arch::asm!("int3");
    let after = core::ptr::read_volatile(&raw const BREAKPOINT_HITS);
    if after == before + 1 {
        crate::println!("  idt:    int3 entered and returned (handler ran, iretq works)");
    } else {
        // Reaching here means execution resumed without the handler counting,
        // which would mean the gate pointed somewhere unexpected.
        crate::println!("  idt:    !! int3 returned but handler did not run ({} -> {})",
            before, after);
    }
}

/// Initialise the x86-64 interrupt subsystem: PIC and IDT.
pub unsafe fn init() {
    pic_init();
    idt_init();
    crate::println!("  idt:    256-entry table loaded, PIC remapped to vectors 32..47");
}

/// Shim for lklirq: unmask a device interrupt.
pub unsafe fn unmask_spi(intid: u32) {
    if intid < 8 {
        let mask = inb(PIC1_DATA) & !(1 << intid);
        outb(PIC1_DATA, mask);
    } else if intid < 16 {
        let mask = inb(PIC2_DATA) & !(1 << (intid - 8));
        outb(PIC2_DATA, mask);
    }
}

pub unsafe fn enable_spi(intid: u32) {
    unmask_spi(intid);
}

pub unsafe fn mask_spi(intid: u32) {
    if intid < 8 {
        let mask = inb(PIC1_DATA) | (1 << intid);
        outb(PIC1_DATA, mask);
    } else if intid < 16 {
        let mask = inb(PIC2_DATA) | (1 << (intid - 8));
        outb(PIC2_DATA, mask);
    }
}

pub unsafe fn eoi(intid: u32) {
    pic_eoi(intid as u8);
}

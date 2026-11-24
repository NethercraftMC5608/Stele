const MSR_FS_BASE: u32 = 0xc000_0100u32;

#[inline]
pub fn current_page_table() -> u64 {
    let cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack)) };
    cr3
}

#[inline]
pub unsafe fn set_page_table(root: u64) {
    core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack));
}

#[inline]
pub fn tlb_flush_all() {
    unsafe {
        let cr3: u64;
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
        core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nomem, nostack));
    }
}

#[inline]
pub fn tlb_flush_page(va: u64) {
    unsafe { core::arch::asm!("invlpg [{}]", in(reg) va, options(nostack)) };
}

#[inline]
pub fn memory_barrier() {
    unsafe { core::arch::asm!("mfence", options(nostack)) };
}

#[inline]
pub fn halt() {
    unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
}

/// Which CPU this is: the initial APIC ID, bits 24..31 of EBX from CPUID.1.
///
/// QEMU's q35 assigns APIC IDs 0..N in boot order, so this is the CPU index
/// for the machines nk boots on. Masked to three bits to match MAX_CPUS.
/// Reads CPUID, not the LAPIC ID register: CPUID works before the LAPIC is
/// mapped or enabled, and `cpu_id` is called from paths that run before it.
#[inline]
pub fn cpu_id() -> usize {
    // rbx is LLVM's, so CPUID's ebx output is swapped out through a scratch
    // register -- and the swap has to be the full 64 bits. With 32-bit
    // `mov`/`xchg` the restore wrote ebx, which zeroes rbx's upper half, and
    // whatever the compiler was keeping there lost it: x86_legacy held a
    // syscall's result in rbx across set_user_syscall (which calls this), so
    // mkdirat's -17 reached busybox as 0xffffffef and glibc never set errno.
    //
    // eax is an output too: CPUID overwrites it, and declaring it input-only
    // let the compiler assume it survived.
    let rbx: u64;
    unsafe {
        core::arch::asm!(
            "mov {tmp}, rbx",
            "cpuid",
            "xchg {tmp}, rbx",
            tmp = out(reg) rbx,
            inout("eax") 1u32 => _,
            out("ecx") _,
            out("edx") _,
            options(nomem, nostack, preserves_flags),
        );
    }
    let from_cpuid = ((rbx >> 24) & 0x7) as usize;
    // CPUID is not trustworthy this early on a secondary: the bring-up loop
    // showed it reading 0 while the same CPU's LAPIC id was already 1. The
    // architectural stand-in is the local APIC id; the firmware/APIC-derived
    // dense mapping is the eventual design. Temporary, per the clean-tree
    // parity pass.
    let from_lapic = (crate::lapic::id() & 0xff) as usize;
    if from_cpuid != from_lapic {
        return from_lapic;
    }
    from_cpuid
}

/// Interrupt another CPU so it takes its pending reschedule promptly.
///
/// Maps the CPU index to an APIC ID 1:1 -- true on QEMU's q35 and on every
/// machine nk boots, where firmware assigns IDs densely from 0. Sends the
/// reschedule vector; the target's handler sets its own need_resched and the
/// switch happens at the next safe point, exactly like a local wakeup.
#[inline]
pub fn send_ipi(cpu: usize) {
    unsafe { crate::lapic::send_ipi(cpu as u32, crate::lapic::IPI_VECTOR) };
}

/// Load `%fs.base` with the thread-local storage base.
#[inline]
pub unsafe fn set_tls(addr: u64) {
    core::arch::asm!("wrmsr", in("ecx") 0xc0000100u32, in("eax") addr as u32, in("edx") (addr >> 32) as u32, options(nomem, nostack));
}

/// A free-running cycle counter, for measuring things shorter than a tick.
///
/// The scheduler's targets are in microseconds and the tick is 10 ms, so the
/// tick count cannot see any of them: every wakeup latency worth reporting
/// rounds to either 0 or 10 ms. `rdtsc` is the only clock on x86-64 with the
/// resolution to tell a 200 µs scheduling delay from a 5 ms one.
///
/// Monotonic but *not* of known frequency -- see `timer::calibrate`. Use it
/// for differences, and convert with `timer::cycles_to_ns`.
///
/// `rdtsc` rather than `rdtscp`: the serialising variant is not available on
/// every machine that runs this, and a few cycles of out-of-order skew do not
/// matter at the scale being measured.
#[inline]
pub fn cycles() -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack));
    }
    ((high as u64) << 32) | (low as u64)
}

/// Read `%fs.base`.
#[inline]
pub fn current_tls() -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") 0xc0000100u32,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack)
        );
    }
    ((high as u64) << 32) | (low as u64)
}

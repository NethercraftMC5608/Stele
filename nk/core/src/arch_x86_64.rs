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

/// Dense logical index established from the enabled MADT processors.
#[inline]
pub fn cpu_id() -> usize {
    crate::lapic::logical_id()
}

/// Address the physical APIC that owns a logical scheduler slot.
#[inline]
pub fn send_ipi(cpu: usize) {
    if let Some(apic) = crate::lapic::apic_id(cpu) {
        unsafe { crate::lapic::send_ipi(apic, crate::lapic::IPI_VECTOR) };
    }
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

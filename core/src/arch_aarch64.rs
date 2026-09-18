#[inline]
pub fn current_page_table() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) v, options(nomem, nostack)) };
    v
}

#[inline]
pub unsafe fn set_page_table(root: u64) {
    core::arch::asm!(
        "msr ttbr0_el1, {}",
        "dsb ishst",
        "tlbi vmalle1",
        "dsb ish",
        "isb",
        in(reg) root,
        options(nostack)
    );
}

#[inline]
pub fn current_tls() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, tpidr_el0", out(reg) v, options(nomem, nostack)) };
    v
}

/// A free-running cycle counter, for measuring things shorter than a tick.
///
/// The virtual counter, to match `timer.rs` -- the physical one traps under
/// Apple's hypervisor, which is the whole reason the virtual timer is used
/// there. Its frequency is `CNTFRQ_EL0`, known exactly and without
/// calibration, which is the pleasant half of this on aarch64.
#[inline]
pub fn cycles() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack)) };
    v
}

#[inline]
pub unsafe fn set_tls(tls: u64) {
    core::arch::asm!("msr tpidr_el0, {}", in(reg) tls, options(nostack));
}

#[inline]
pub fn tlb_flush_all() {
    unsafe {
        core::arch::asm!("dsb ishst", "tlbi vmalle1is", "dsb ish", "isb", options(nostack));
    }
}

#[inline]
pub fn tlb_flush_page(va: u64) {
    unsafe {
        let page = va >> 12;
        core::arch::asm!("dsb ishst", "tlbi vae1is, {}", "dsb ish", "isb", in(reg) page, options(nostack));
    }
}

#[inline]
pub fn memory_barrier() {
    unsafe { core::arch::asm!("dsb ishst", "isb", options(nomem, nostack)) };
}

#[inline]
pub fn halt() {
    unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
}

/// Point VBAR_EL1 at nk's vector table.
///
/// The boot CPU does this in `boot.s` before it calls Rust; a secondary
/// starts with VBAR_EL1 at its reset value, so until this runs any exception
/// on that CPU vectors to an address that does not exist -- and the result is
/// not a fault report, it is silence. Called before interrupts are unmasked.
///
/// # Safety
/// `__vectors` must be the vector table, and the caller must not be relying
/// on the old one.
pub unsafe fn set_vectors() {
    unsafe extern "C" {
        static __vectors: u8;
    }
    let v = &raw const __vectors as u64;
    core::arch::asm!("msr vbar_el1, {}", "isb", in(reg) v, options(nostack));
}

/// Which CPU this is, from the affinity the firmware assigned.
///
/// MPIDR_EL1 Aff0 is the core number on `virt` and on every Apple Silicon
/// performance/efficiency cluster layout nk boots on. Masked to three bits:
/// MAX_CPUS is 8, and anything above that clamps in `cpu_index` anyway.
#[inline]
pub fn cpu_id() -> usize {
    let mpidr: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack)) };
    (mpidr & 0xff) as usize
}

/// Signal another CPU via SGI 1, the reschedule vector.
///
/// GICv3 SGIs carry no MMIO: ICC_SGI1R_EL1 names the interrupt (bits 27..24),
/// the affinity of the target (bits 47..32 and 23..16), and whether it goes
/// to one CPU or all-but-self (bit 40). Target-list model with Aff0 as the
/// CPU number -- matches `cpu_id` on the machines nk boots. Group 1, so the
/// target sees it as a normal non-secure interrupt.
#[inline]
pub fn send_ipi(cpu: usize) {
    let cpu = (cpu & 0xff) as u64;
    const SGI_RESCHED: u64 = 1;
    // ICC_SGI1R_EL1: TargetList[15:0], Aff1[23:16], IntID[27:24],
    // Aff2[39:32], Aff3[55:48]. Aff0 is selected by which TargetList bit
    // is set, not by a field. All of nk's CPUs sit in cluster Aff3=Aff2=
    // Aff1=0, so the target is exactly bit `cpu` of TargetList.
    let sgi = (SGI_RESCHED << 24) | (1u64 << cpu);
    unsafe {
        core::arch::asm!("msr ICC_SGI1R_EL1, {}", "isb", in(reg) sgi, options(nostack));
    }
}

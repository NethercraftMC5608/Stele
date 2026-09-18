//! PSCI: asking the firmware to turn the machine off.
//!
//! nk cannot power down a machine itself -- that is below EL1, in whatever
//! runs the platform. PSCI is the standard way to ask, and QEMU's `virt`
//! implements it, so `nk_poweroff` ends a run cleanly instead of leaving a
//! kernel spinning until something outside kills it.
//!
//! That matters more than it sounds for the tests. Killing QEMU on a watchdog
//! makes "the kernel finished" and "the kernel hung" look identical from the
//! outside -- both end with a signal after N seconds -- and that ambiguity is
//! exactly what made one silent hang take far longer to pin down than it
//! should have. A clean exit is a signal in itself.
//!
//! The call method comes from the device tree's `psci` node, which says `hvc`
//! or `smc` depending on whether firmware sits at EL2 or EL3. QEMU's virt
//! under HVF says `hvc`. Assuming one would work on this machine and fail on
//! the next, in a way that presents as the machine simply not switching off.

use crate::dt::Fdt;

const SYSTEM_OFF: u32 = 0x8400_0008;

#[derive(Clone, Copy, PartialEq)]
enum Method {
    None,
    Hvc,
    Smc,
}

static mut METHOD: Method = Method::None;

pub fn init(fdt: &Fdt) {
    let m = fdt
        .find_compatible("arm,psci-0.2")
        .or_else(|| fdt.find_compatible("arm,psci"))
        .and_then(|n| n.prop("method"))
        .map(|v| if v.starts_with(b"smc") { Method::Smc } else { Method::Hvc })
        .unwrap_or(Method::None);
    unsafe { METHOD = m };
    crate::println!(
        "  psci:   {}",
        match m {
            Method::Hvc => "hvc",
            Method::Smc => "smc",
            Method::None => "absent -- the machine will not switch itself off",
        }
    );
}

/// Start a secondary CPU at a physical address, with a context value.
///
/// PSCI 0.2+ `CPU_ON`, the 64-bit function id. The target is an MPIDR value,
/// the entry a *physical* address -- the CPU starts with the MMU off, so a
/// link address only works because nk's image is loaded at the address it was
/// linked for -- and the context is handed over in x0.
///
/// The return is the PSCI status: 0 is success, -2 (PSCI_ALREADY_ON) means
/// the CPU was already running, and anything else is a refusal that says the
/// target id was not one the firmware knows. Returning it rather than
/// panicking is deliberate: "this CPU is already on" is a normal answer
/// during bring-up, and the caller decides what it means.
///
/// # Safety
/// `entry` must be executable with the MMU off. PSCI must have been found.
#[cfg(target_arch = "aarch64")]
pub unsafe fn cpu_on(target: u64, entry: u64, context: u64) -> i64 {
    let ret: u64;
    match METHOD {
        Method::Hvc => core::arch::asm!(
            "hvc #0",
            inout("x0") 0xC400_0003u64 => ret,
            in("x1") target,
            in("x2") entry,
            in("x3") context,
            options(nostack),
        ),
        Method::Smc => core::arch::asm!(
            "smc #0",
            inout("x0") 0xC400_0003u64 => ret,
            in("x1") target,
            in("x2") entry,
            in("x3") context,
            options(nostack),
        ),
        Method::None => return -1,
    }
    ret as i64
}

/// Turn the machine off. Returns only if the firmware refuses.
pub fn poweroff() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        match core::ptr::read(&raw const METHOD) {
            Method::Hvc => core::arch::asm!("hvc #0", in("x0") SYSTEM_OFF as u64, options(nostack)),
            Method::Smc => core::arch::asm!("smc #0", in("x0") SYSTEM_OFF as u64, options(nostack)),
            Method::None => {}
        }
    }
    // PSCI is an Arm firmware interface; there is no `hvc`/`smc` on x86-64.
    // Powering off a PC means an ACPI write or a hypervisor-specific port,
    // neither of which nk has yet -- so this is honestly a no-op and the
    // caller falls through to halting.
    #[cfg(target_arch = "x86_64")]
    {
        let _ = SYSTEM_OFF;
    }
}

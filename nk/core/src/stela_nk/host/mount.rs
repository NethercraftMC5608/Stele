// SPDX-License-Identifier: GPL-2.0-or-later
//! The whole M2 board mounted on nk, the way `stela.efi` mounts it (ROADMAP
//! Phase 5, NK_STELA_MOUNT_TEST).
//!
//! `stela_runtime::machine::mount::devices_with_memory` places every device
//! the bundle's DeviceTree describes -- UART, watchdog, AIC, GPIO and NOR,
//! storage, power controllers, processor blocks, I2C, Type-C PHYs, DARTs,
//! PMGR power states, the memory controllers, display, SEP, USB, AES and SoC
//! banks -- around native memory owners. On the M2 those are the cold T8112
//! memory plan (`t8112_cold::Plan`) over canonical guest RAM, which nk
//! registers from its own frames (`GuestRam::into_registered`) at the M2's
//! DRAM base.
//!
//! Host inputs nk does not have yet stay absent or stand-ins: an empty sparse
//! disk for the internal SSD, a headless 1080p scanout for the display, no
//! SEP state partition, no USB cables, no wall clock.

use super::super::ram::GuestRam;
use super::*;
use alloc::{boxed::Box, string::String, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};
use stela_runtime::machine::mount;
use stela_soc::machine::memory_resources::{t8112::cache::ZeroWayConfiguration, MemoryResources};
use stela_soc::machine::{t8112_cold, ConsoleSink, UsbCables};

/// The M2's DRAM base, where the bundle's regions start.
const DRAM: u64 = 0x8_0000_0000;
/// Guest RAM registered for the memory owners: the first 2 MiB of DRAM.
const RAM_BYTES: u64 = 2 << 20;
/// The sparse disk behind the ANS2: 256 GiB of 4 KiB blocks, like the
/// smallest MacBook Air M2 SSD.
const SSD_BLOCKS: u64 = 256 << 18;

/// An internal SSD that stores only the blocks written; the rest read as
/// zeros. The stand-in until a disk Linux drives backs the ANS2.
struct SparseDisk {
    identity: stela_driver_base::block::Identity,
    blocks: alloc::collections::BTreeMap<u64, Box<[u8; 4096]>>,
}
impl SparseDisk {
    fn new(blocks: u64) -> Self {
        use stela_driver_base::block::{Features, Identity, Transport};
        Self {
            identity: Identity {
                transport: Transport::VirtioBlk,
                location: String::from("nk"),
                model: String::from("nk sparse SSD"),
                serial: String::from("NKSPARSESSD"),
                blocks,
                block_size: 4096,
                read_only: false,
                features: Features::default(),
            },
            blocks: alloc::collections::BTreeMap::new(),
        }
    }
}
impl stela_driver_base::block::BlockDevice for SparseDisk {
    fn identity(&self) -> &stela_driver_base::block::Identity {
        &self.identity
    }
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), stela_driver_base::block::BlockError> {
        self.identity.check(lba, buffer.len())?;
        for (i, out) in buffer.chunks_mut(4096).enumerate() {
            match self.blocks.get(&(lba + i as u64)) {
                Some(block) => out.copy_from_slice(&block[..out.len()]),
                None => out.fill(0),
            }
        }
        Ok(())
    }
    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), stela_driver_base::block::BlockError> {
        self.identity.check_write(lba, data.len())?;
        for (i, chunk) in data.chunks(4096).enumerate() {
            let mut block = Box::new([0u8; 4096]);
            block[..chunk.len()].copy_from_slice(chunk);
            self.blocks.insert(lba + i as u64, block);
        }
        Ok(())
    }
}

struct Silent;
impl ConsoleSink for Silent {
    fn transmit(&mut self, _: u8) -> Result<(), stela_soc::uart::ConsoleBusy> {
        Ok(())
    }
}

/// The external display's monitor: a 1920x1080 BGRA scanout that counts
/// the frames the display coprocessor presents. Drawing them on nk's
/// framebuffer (`fbcon`) comes later.
struct Headless {
    frames: u64,
}
impl stela_soc::scanout::Scanout for Headless {
    fn mode(&self) -> stela_soc::scanout::Mode {
        stela_soc::scanout::Mode {
            width: 1920,
            height: 1080,
            format: stela_soc::scanout::PixelFormat::Bgra8888,
        }
    }
    fn present(&mut self, _rows: &[u8]) -> stela_soc::Result<()> {
        self.frames += 1;
        Ok(())
    }
}

/// Files the probe reads through LKL before it dedicates a core.
struct Bundle {
    tree: Vec<u8>,
    hpm: Vec<u8>,
}
static mut BUNDLE: Option<Bundle> = None;

pub(crate) fn mount_probe(dir: &str) -> bool {
    static PASSED: AtomicBool = AtomicBool::new(false);
    let read = |name: &str| super::board::read_linux_file(&alloc::format!("{dir}/{name}"));
    let (tree, hpm) = match (read("devicetree.adt"), read("USB-C_HPM,46-P01-AP.bin")) {
        (Ok(tree), Ok(hpm)) => (tree, hpm),
        (tree, hpm) => {
            crate::println!(
                "STELA_MOUNT: cannot read the bundle in {}: devicetree {:?}, HPM firmware {:?}",
                dir, tree.err(), hpm.err()
            );
            crate::println!("STELA_MOUNT_FAIL");
            return false;
        }
    };
    unsafe { *(&raw mut BUNDLE) = Some(Bundle { tree, hpm }) };
    extern "C" fn on_core(_: usize) {
        const PAGES: usize = 512;
        let stack = crate::frames::alloc_contiguous(PAGES).expect("mount stack");
        let flags = crate::sync::irq_save();
        unsafe {
            nk_stela_on_stack(stack.add(PAGES * 4096), run_mount);
            crate::sync::irq_restore(flags);
            for page in 0..PAGES {
                crate::frames::free(stack.add(page * 4096));
            }
        }
    }
    extern "C" fn run_mount() {
        PASSED.store(mount_board(), Ordering::Release);
    }
    let Some(id) = crate::sched::dedicate_cpu(3, "board-mount", on_core, 0) else {
        crate::println!("STELA_MOUNT: dedicate_cpu(3) refused (needs NK_SMP)");
        return false;
    };
    crate::sched::join(id);
    let passed = PASSED.load(Ordering::Acquire);
    crate::println!("{}", if passed { "STELA_MOUNT_PASS" } else { "STELA_MOUNT_FAIL" });
    passed
}

/// Hardware entropy for the SEP model: RDRAND when the CPU has it.
fn rdrand_fill(bytes: &mut [u8]) -> stela_soc::Result<()> {
    let ecx: u32;
    unsafe {
        core::arch::asm!("mov {t}, rbx", "cpuid", "xchg {t}, rbx", t = out(reg) _,
            inout("eax") 1u32 => _, out("ecx") ecx, out("edx") _, options(nomem, nostack));
    }
    if ecx & (1 << 30) == 0 {
        return Err(stela_contracts::refuse!("this CPU has no RDRAND for the SEP's entropy"));
    }
    for chunk in bytes.chunks_mut(8) {
        let mut value: u64;
        let mut tries = 0;
        loop {
            let ok: u8;
            unsafe { core::arch::asm!("rdrand {v}", "setc {ok}", v = out(reg) value, ok = out(reg_byte) ok) };
            if ok != 0 {
                break;
            }
            tries += 1;
            if tries > 100 {
                return Err(stela_contracts::refuse!("RDRAND gave no value"));
            }
        }
        chunk.copy_from_slice(&value.to_le_bytes()[..chunk.len()]);
    }
    Ok(())
}

fn mount_board() -> bool {
    let Some(bundle) = (unsafe { (*(&raw mut BUNDLE)).take() }) else { return false };
    let hpm: &'static [u8] = Box::leak(bundle.hpm.into_boxed_slice());
    let board = match stela_soc_data::board::board(&bundle.tree) {
        Ok(board) => board,
        Err(error) => {
            crate::println!("STELA_MOUNT: DeviceTree refused: {:?}", error);
            return false;
        }
    };
    crate::println!(
        "STELA_MOUNT: {:?}, {} processors; firmware files {:?}",
        board.cpus.chip, board.cpus.processors.len(), board.firmware_files()
    );

    // Canonical guest RAM at the DRAM base, from nk's frames.
    let Some(mut ram) = GuestRam::reserve(DRAM, RAM_BYTES) else { return false };
    let mut page = DRAM;
    while page < DRAM + RAM_BYTES {
        if ram.ensure_page(page).is_none() {
            crate::println!("STELA_MOUNT: out of frames for guest RAM");
            return false;
        }
        page += 4096;
    }
    {
        let (cr0, cr4, flags): (u64, u64, u64);
        let (lo, hi): (u32, u32);
        unsafe {
            core::arch::asm!("mov {}, cr0", out(reg) cr0);
            core::arch::asm!("mov {}, cr4", out(reg) cr4);
            core::arch::asm!("pushfq", "pop {}", out(reg) flags);
            core::arch::asm!("rdmsr", in("ecx") 0xc000_0080u32, out("eax") lo, out("edx") hi);
        }
        crate::println!("STELA_MOUNT: cpu {} cr0 {:#x} cr4 {:#x} efer {:#x} rflags {:#x}",
            crate::arch::cpu_id(), cr0, cr4, (hi as u64) << 32 | lo as u64, flags);
    }
    let canonical = match unsafe { ram.into_registered() } {
        Ok(ram) => Arc::new(ram),
        Err(error) => {
            crate::println!("STELA_MOUNT: RAM registration refused: {:?}", error);
            return false;
        }
    };
    // No CTRR regions yet: they cover the bundle's read-only run, which is
    // loaded in a later step.
    let programmed = stela_soc_data::amcc::ctrr_a(None);
    let plan = t8112_cold::Configuration::clear_software_controls(ZeroWayConfiguration::DisableArgument)
        .and_then(|configuration| {
            t8112_cold::Plan::prepare_ap_frontend(&board, &canonical, programmed, &configuration)
        });
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => {
            crate::println!("STELA_MOUNT: T8112 memory plan refused: {:?}", error);
            return false;
        }
    };
    let build = stela_formats::builds::find("26B131").ok();
    let firmware = move |name: &str| (name == "USB-C_HPM,46-P01-AP.bin").then_some(hpm);
    let inputs = mount::Inputs {
        programmed,
        reset: None,
        now: 0,
        wall: None,
        rtc_anchor: None,
        // The internal SSD: an empty sparse disk until a Linux block
        // device (through LKL, on CPU 0) backs it.
        backing: Some(Box::new(SparseDisk::new(SSD_BLOCKS))),
        sep_state: None,
        sep_observe: false,
        sep_recovery_policy_nonce: None,
        entropy: Box::new(rdrand_fill),
        restore: false,
        input: false,
        usb: UsbCables::default(),
        scanout: Some(Box::new(Headless { frames: 0 })),
        firmware: &firmware,
        build,
    };
    let mut lines = 0usize;
    let mut log = |arguments: core::fmt::Arguments<'_>| {
        lines += 1;
        let text: String = alloc::format!("{arguments}");
        for line in text.lines() {
            crate::println!("  mount: {}", line);
        }
    };
    let result = mount::devices_with_memory(
        &board,
        Silent,
        inputs,
        Some(MemoryResources::T8112Cold(plan)),
        &mut log,
    );
    match result {
        Ok((_devices, usb)) => {
            crate::println!(
                "STELA_MOUNT: the whole board is placed ({} log lines; restore USB {:?})",
                lines, usb.is_some()
            );
            true
        }
        Err(error) => {
            crate::println!("STELA_MOUNT: mount refused: {:?}", error);
            false
        }
    }
}

// SPDX-License-Identifier: GPL-2.0-or-later
//! macOS 27's first instructions on nk: the bundle loaded into guest DRAM
//! and SPTM entered on the mounted M2 board (ROADMAP Phase 5,
//! NK_STELA_SPTM_TEST).
//!
//! CPU 0 reads the bundle through LKL: `bundle.json` gives each region's
//! file, offset, length and guest physical address, the SPTM entry, its x0
//! (the boot arguments) and the read-only run CTRR-A locks. Guest DRAM is
//! nk frames, one per page, mapped at the guest's own
//! addresses in nk's guest DRAM window (`paging::GUEST_DRAM_VA`), so native
//! guest code reaches RAM with the MMU off exactly where the guest puts it,
//! and the same frames are registered as the canonical RAM the board's
//! memory owners check.
//!
//! On dedicated CPU 3 the whole board is mounted (`mount.rs`'s inputs) and
//! PE 0 starts at EL2, MMU off, at SPTM's entry -- as `stela.efi` starts it.
//! Once the guest enables its MMU, `DramHost` fills x86 shadow translations
//! for guest RAM. The current probe time-slices all guest PEs on this AP.

use super::*;
use alloc::{boxed::Box, string::String, sync::Arc, vec};
use core::sync::atomic::{AtomicBool, Ordering};
use stela_engine::memory::{RegisteredSystemRam, ReservedSystemRamRange};
use stela_runtime::machine::mount;
use stela_soc::machine::memory_resources::{t8112::cache::ZeroWayConfiguration, MemoryResources};
use stela_soc::machine::{t8112_cold, ConsoleSink, UsbCables};
use stela_engine::machine::FaultCause;
use stela_formats::boot_bundle::{Framebuffer, Machine as BootMachine, Manifest, PatchSource, schema::Region};

const DRAM: u64 = crate::paging::GUEST_DRAM_VA;
/// Guest DRAM backed and registered: `NK_STELA_SPTM_DRAM_MIB` at build
/// time, 2 GiB by default -- the size the bundle must be prepared with
/// (`stela-dev boot prepare --dram-size`), since SPTM uses all it is told
/// of. It comes from RAM above nk's 4 GiB identity map first
/// (`frames::alloc_high`), then from nk's own frames.
const RAM_BYTES: u64 = match option_env!("NK_STELA_SPTM_DRAM_MIB") {
    Some(s) => parse_seconds(s) << 20,
    None => 2 << 30,
};
/// Console silence that ends the run as a hang: `NK_STELA_SPTM_STALL` s at
/// build time. Restore userspace can go quiet for more than 90 seconds
/// while launchd boots its jobs under translation.
const STALL_NS: u64 = match option_env!("NK_STELA_SPTM_STALL") {
    Some(s) => parse_seconds(s) * 1_000_000_000,
    None => 240_000_000_000,
};
/// Frames below this are nk's own (identity mapped) and go back to it.
const IDENTITY_LIMIT: u64 = 1 << 32;
/// Native code arena: 4 MiB of frames, one recovery range.
const CODE_PAGES: usize = 1024;
/// Blocks per `run` call, between checks of the regime.
const SLICE_BLOCKS: u64 = 2_000;
const GUEST_CPU: usize = 3;
/// Wall-clock bound on the whole run: `NK_STELA_SPTM_SECONDS` at build
/// time, 60 s by default. Under QEMU TCG on an ARM Mac the guest runs two
/// translations deep, so SPTM moves slowly.
const RUN_NS: u64 = match option_env!("NK_STELA_SPTM_SECONDS") {
    Some(s) => parse_seconds(s) * 1_000_000_000,
    None => 60_000_000_000,
};
const fn parse_seconds(s: &str) -> u64 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut n = 0u64;
    while i < b.len() {
        n = n * 10 + (b[i] - b'0') as u64;
        i += 1;
    }
    n
}

/// What CPU 0 prepared for the core.
struct Prepared {
    tree: Vec<u8>,
    framebuffer: Framebuffer,
    disk: Option<super::disk::LinuxDisk>,
    restore: bool,
    hpm: Vec<u8>,
    /// Each guest DRAM page's backing frame, in guest order.
    frames: Vec<u64>,
    entry: u64,
    x0: u64,
    read_only: (u64, u64),
}
static mut PREPARED: Option<Prepared> = None;

/// The guest's console, printed as whole lines.
struct Console {
    line: Vec<u8>,
    bytes: Arc<core::sync::atomic::AtomicU64>,
}
impl ConsoleSink for Console {
    fn transmit(&mut self, byte: u8) -> Result<(), stela_soc::uart::ConsoleBusy> {
        self.bytes.fetch_add(1, Ordering::Relaxed);
        if byte == b'\n' || self.line.len() >= 160 {
            crate::println!("GUEST| {}", String::from_utf8_lossy(&self.line));
            self.line.clear();
        } else if byte != b'\r' {
            self.line.push(byte);
        }
        Ok(())
    }
}

const SCANOUT_MODE: stela_soc::scanout::Mode = stela_soc::scanout::Mode {
    width: 1920,
    height: 1080,
    format: stela_soc::scanout::PixelFormat::Bgra8888,
};

struct Headless;
impl stela_soc::scanout::Scanout for Headless {
    fn mode(&self) -> stela_soc::scanout::Mode {
        SCANOUT_MODE
    }
    fn present(&mut self, _rows: &[u8]) -> stela_soc::Result<()> {
        Ok(())
    }
}

/// The engine's host over the guest DRAM window, MMU off.
struct DramHost {
    arena: *mut u8,
    used: usize,
    entry: Code,
    executions: u64,
    faults: u64,
    tsc_base: u64,
    deadline: Option<u64>,
    hrtimer: bool,
    /// The first regime asking for the MMU (SCTLR.M), with the count of
    /// executions before it.
    mmu: Option<(u64, u64)>,
    /// Guest DRAM's frames, for the shadow.
    frames: Vec<u64>,
    /// The MMU-on shadow for the current regime, once the guest turned its
    /// MMU on.
    shadow: Option<Box<Shadow>>,
    /// Shadows of recent other regimes (kernel/user mode, other processes),
    /// kept filled so a switch back reuses them; oldest first.
    cached: Vec<Box<Shadow>>,
    /// Regime switches, and those a cached shadow served.
    switches: u64,
    reused: u64,
    /// TLB invalidations served page by page, and those that cleared all.
    page_invalidations: u64,
    full_invalidations: u64,
    /// Native accesses below `UPPER` that the shadow could not fill.
    lower_faults: u64,
    entered_el0: bool,
    called_from_el0: bool,
    trace_exec: bool,
    exec_reports: u8,
}

impl DramHost {
    fn new(frames: Vec<u64>, trace_exec: bool) -> Option<Self> {
        let arena = crate::frames::alloc_contiguous(CODE_PAGES)?;
        Some(Self {
            arena,
            used: 0,
            entry: Code::new(&stela_compiler::x86_64::pinned::entry()),
            executions: 0,
            faults: 0,
            tsc_base: crate::arch::cycles(),
            deadline: None,
            hrtimer: false,
            mmu: None,
            frames,
            shadow: None,
            cached: Vec::new(),
            switches: 0,
            reused: 0,
            page_invalidations: 0,
            full_invalidations: 0,
            lower_faults: 0,
            entered_el0: false,
            called_from_el0: false,
            trace_exec,
            exec_reports: 0,
        })
    }
    fn in_ram(at: u64, len: usize) -> bool {
        at >= DRAM && at.checked_add(len as u64).is_some_and(|end| end <= DRAM + RAM_BYTES)
    }
    fn arena_range(&self) -> (u64, u64) {
        (self.arena as u64, self.arena as u64 + (CODE_PAGES * 4096) as u64)
    }
    /// Write `bytes` at arena offset `at`, flipping the pages it touches to
    /// RW/NX and back to RX (no block runs while the engine installs).
    unsafe fn write_code(&mut self, at: usize, bytes: &[u8]) {
        let first = at / 4096;
        let last = (at + bytes.len() - 1) / 4096;
        for page in first..=last {
            crate::paging::boot_code_rights(self.arena as u64 + (page * 4096) as u64, false);
        }
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.arena.add(at), bytes.len());
        for page in first..=last {
            crate::paging::boot_code_rights(self.arena as u64 + (page * 4096) as u64, true);
        }
    }
}

impl Drop for DramHost {
    fn drop(&mut self) {
        for page in 0..CODE_PAGES {
            let frame = unsafe { self.arena.add(page * 4096) };
            unsafe {
                crate::paging::boot_code_rights(frame as u64, false);
                crate::frames::free(frame);
            }
        }
    }
}

/// Give guest DRAM back: unmap the window and free its frames, once nothing
/// (machine, board, canonical RAM) holds it.
fn release_dram(frames: &[u64]) {
    for page in 0..frames.len() as u64 {
        unsafe { crate::paging::unmap_guest_dram(DRAM + page * 4096) };
    }
    crate::arch::tlb_flush_all();
    // nk's own frames go back; high frames stay with the pool's bump
    // (`frames::alloc_high` cannot take them back).
    for &frame in frames.iter().filter(|&&f| f < IDENTITY_LIMIT) {
        unsafe { crate::frames::free(frame as *mut u8) };
    }
}

impl DramHost {
    /// Remove what `reach` names from one shadow: page by page for a page
    /// or small range, the whole shadow otherwise. True when it cleared all.
    fn apply(shadow: &mut Shadow, reach: mmu::tlbi::Reach) -> bool {
        use mmu::tlbi::Reach;
        match reach {
            Reach::Nothing => false,
            Reach::All => {
                shadow.clear();
                true
            }
            Reach::Containing(va) => shadow.invalidate(va, va),
            Reach::Overlapping { first, last } => shadow.invalidate(first, last),
        }
    }
    fn counter_now(&self) -> u64 {
        guest_count(self.tsc_base)
    }
}

impl Host for DramHost {
    fn read_ram(&mut self, at: u64, out: &mut [u8]) -> bool {
        if !Self::in_ram(at, out.len()) {
            return false;
        }
        unsafe { core::ptr::copy_nonoverlapping(at as *const u8, out.as_mut_ptr(), out.len()) };
        true
    }
    fn write_ram(&mut self, at: u64, data: &[u8]) -> bool {
        if !Self::in_ram(at, data.len()) {
            return false;
        }
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), at as *mut u8, data.len()) };
        true
    }
    fn install(&mut self, bytes: &[u8]) -> Result<NativeCode, InstallError> {
        let at = (self.used + 15) & !15;
        if bytes.is_empty() || at + bytes.len() > CODE_PAGES * 4096 {
            return Err(InstallError::Full);
        }
        unsafe { self.write_code(at, bytes) };
        self.used = at + bytes.len();
        Ok(NativeCode(self.arena as usize + at))
    }
    fn patch(&mut self, at: NativeCode, bytes: &[u8]) {
        let offset = at.0 - self.arena as usize;
        assert!(offset + bytes.len() <= self.used, "patch outside installed code");
        unsafe { self.write_code(offset, bytes) };
    }
    fn read_native(&mut self, at: NativeCode, out: &mut [u8]) -> bool {
        let (start, _) = self.arena_range();
        let Some(offset) = (at.0 as u64).checked_sub(start) else { return false };
        if offset as usize + out.len() > self.used {
            return false;
        }
        unsafe { core::ptr::copy_nonoverlapping(at.0 as *const u8, out.as_mut_ptr(), out.len()) };
        true
    }
    fn discard_code(&mut self) {
        self.used = 0;
    }
    unsafe fn execute(&mut self, at: NativeCode, state: &mut GuestState) -> NativeExit {
        self.executions += 1;
        let range = self.arena_range();
        let Some(shadow) = self.shadow.as_mut() else {
            let raw = super::super::fault::invoke(self.entry.0, at.0, state, core::iter::once(range), DRAM, None);
            return match raw {
                Err(fault) => {
                    self.faults += 1;
                    NativeExit::Fault(fault.identity())
                }
                Ok(raw) => NativeExit::Exit { next_pc: raw as u64, kind: (raw >> 64) as u64 },
            };
        };
        // MMU on: run on the shadow root; not-present RAM accesses fill
        // in the trap and retry.
        let original = crate::arch::current_page_table();
        crate::arch::set_page_table(shadow.cr3());
        let context = &mut **shadow as *mut Shadow as *mut ();
        let raw = super::super::fault::invoke_filling(
            self.entry.0, at.0, state, core::iter::once(range), DRAM, None,
            Some(super::super::fault::Filler { context, fill: shadow_fill, owns: shadow_owns }),
        );
        crate::arch::set_page_table(original);
        match raw {
            Ok(raw) => NativeExit::Exit { next_pc: raw as u64, kind: (raw >> 64) as u64 },
            Err(capture) => {
                self.faults += 1;
                if capture.trap != 14 {
                    return NativeExit::Fault(capture.identity());
                }
                if capture.address < UPPER {
                    self.lower_faults += 1;
                }
                // A fresh walk after the trap: guest fault, device, or a
                // configuration the engine does not translate.
                let cause = match shadow.translate(capture.address, capture.write) {
                    mmu::Outcome::Fault(fault) => FaultCause::Guest(fault),
                    mmu::Outcome::Unsupported(reason) => FaultCause::Translation(reason),
                    mmu::Outcome::Mapped(t) => {
                        let page = capture.address & !4095;
                        if shadow.regime.watch.blocks(page) {
                            FaultCause::Watched
                        } else if shadow.regime.tag_blocks(page, t.attributes) {
                            FaultCause::TagChecked
                        } else if DramHost::in_ram(t.physical, 1) {
                            FaultCause::UnmappedRam
                        } else {
                            FaultCause::Device { physical: t.physical }
                        }
                    }
                };
                NativeExit::Fault(capture.native(cause))
            }
        }
    }
    fn report(&mut self, event: &stela_engine::machine::Event) {
        use stela_engine::machine::{Event, exception::Vector};
        // TXM runs at EL0 in the upper half before XNU enters user space.
        if let Event::Return { to_el: 0, pc, .. } = event {
            if *pc < LOWER_END && !self.entered_el0 {
                self.entered_el0 = true;
                crate::println!("STELA_SPTM: entered lower guest EL0 at {pc:#x}");
            }
        }
        if let Event::Exception { vector, from_el, syndrome, return_address, .. } = event {
            let ec = (syndrome >> 26) & 0x3f;
            let fsc = syndrome & 0x3f;
            if *from_el == 0 && ec == 0x15 && *return_address <= LOWER_END && !self.called_from_el0 {
                self.called_from_el0 = true;
                crate::println!("STELA_SPTM: guest EL0 made an SVC at {:#x}", return_address - 4);
            }
            if *vector == Vector::SError
                || (matches!(ec, 0x20 | 0x21 | 0x24 | 0x25)
                    && matches!(fsc, 0x10 | 0x14..=0x17))
            {
                crate::println!("STELA_SPTM: ARM trap {event:x?}");
            }
        }
    }
    fn report_user_svc(&mut self, pc: u64, immediate: u16, state: &GuestState) {
        if pc >= LOWER_END || immediate != 0x80 || self.exec_reports == 32 {
            return;
        }
        // XNU bsd/kern/syscalls.master: execve(path, ...) and posix_spawn(pid, path, ...).
        let (operation, address) = match state.x[16] {
            59 => ("execve", state.x[0]),
            244 => ("posix_spawn", state.x[1]),
            _ => return,
        };
        let Some(shadow) = &self.shadow else { return };
        let mut path = [0u8; 128];
        for (offset, byte) in path.iter_mut().enumerate() {
            let Some(va) = address.checked_add(offset as u64) else { return };
            let mmu::Outcome::Mapped(t) = shadow.translate(va, false) else { return };
            if !DramHost::in_ram(t.physical, 1) {
                return;
            }
            *byte = unsafe { (t.physical as *const u8).read_volatile() };
            if *byte == 0 {
                if let Ok(path) = core::str::from_utf8(&path[..offset]) {
                    self.exec_reports += 1;
                    crate::println!("STELA_SPTM: guest {operation} {path:?}");
                }
                return;
            }
        }
    }
    fn observes_user_svc(&self) -> bool {
        self.trace_exec
    }
    fn placement(&self) -> mmu::placement::Placement {
        mmu::placement::Placement::FourLevel { slices: 2 }
    }
    fn set_regime(&mut self, regime: &Regime) {
        if regime.controls.sctlr & 1 == 0 {
            self.shadow = None;
            return;
        }
        if self.mmu.is_none() {
            self.mmu = Some((regime.controls.sctlr, self.executions));
        }
        if self.shadow.as_ref().is_some_and(|shadow| shadow.regime == *regime) {
            return;
        }
        // Keep the outgoing shadow filled; take one of this regime if kept.
        self.switches += 1;
        if SHADOW_CACHE == 0 {
            if let Some(shadow) = self.shadow.as_mut() {
                shadow.clear();
                shadow.regime = *regime;
            } else {
                self.shadow = Shadow::new(self.frames.clone(), *regime).map(Box::new);
            }
            return;
        }
        if let Some(outgoing) = self.shadow.take() {
            self.cached.push(outgoing);
        }
        if let Some(at) = self.cached.iter().position(|shadow| shadow.regime == *regime) {
            self.reused += 1;
            self.shadow = Some(self.cached.remove(at));
            return;
        }
        self.shadow = if self.cached.len() >= SHADOW_CACHE {
            // Reuse the oldest kept shadow's frames for this regime.
            let mut shadow = self.cached.remove(0);
            shadow.clear();
            shadow.regime = *regime;
            Some(shadow)
        } else {
            Shadow::new(self.frames.clone(), *regime).map(Box::new)
        };
    }
    fn invalidate_translations(&mut self) {
        self.full_invalidations += 1;
        for shadow in self.shadow.iter_mut().chain(self.cached.iter_mut()) {
            shadow.clear();
        }
    }
    /// A TLBI of a page or a small range unmaps just those pages, from every
    /// kept shadow (a superset of the contexts it reaches); only a full or
    /// large invalidation clears them. New guest mappings need no TLBI
    /// (they fault in), so kept shadows stay correct between invalidations.
    fn invalidate_reach(&mut self, reach: mmu::tlbi::Reach, everywhere: mmu::tlbi::Everywhere) {
        use mmu::tlbi::{Everywhere, Reach};
        // The current shadow takes what the TLBI reaches in this context;
        // every kept shadow takes what it removes from any context
        // (`everywhere`), as a superset: ASIDs are not told apart.
        self.page_invalidations += 1;
        if let Some(shadow) = self.shadow.as_mut() {
            if Self::apply(shadow, reach) {
                self.full_invalidations += 1;
            }
        }
        let kept = match everywhere {
            Everywhere::Addresses(reach) | Everywhere::AsidAddresses(reach, _) => reach,
            Everywhere::NonGlobal(_) | Everywhere::All => Reach::All,
        };
        for shadow in self.cached.iter_mut() {
            Self::apply(shadow, kept);
        }
    }
    fn counter(&mut self) -> u64 {
        guest_count(self.tsc_base)
    }
    fn arm_timer(&mut self, deadline: Option<u64>) {
        guest_timer().store(false, Ordering::Release);
        match deadline {
            None => {
                self.deadline = None;
                crate::hrtimer::cancel();
            }
            Some(count) => {
                self.deadline = Some(count);
                self.hrtimer = crate::hrtimer::arm_at(self.tsc_base + count_to_tsc(count), guest_timer_fired);
            }
        }
    }
    fn wait_for_interrupt(&mut self) {
        if self.hrtimer {
            while !guest_timer().swap(false, Ordering::AcqRel) && !super::irq::take_kick() {
                unsafe { core::arch::asm!("sti", "hlt", "cli", options(nomem, nostack)) };
            }
            return;
        }
        if let Some(deadline) = self.deadline {
            while guest_count(self.tsc_base) < deadline {
                core::hint::spin_loop();
            }
        }
    }
    fn poll_interrupt(&mut self) -> bool {
        guest_timer().swap(false, Ordering::AcqRel)
    }
}

// --- Demand-filled shadow page tables -------------------------------------

/// The engine's four-level placement with two slices: guest addresses at or
/// above this run natively at the same host address. Below it is nk's own
/// half (PML4 entries 0..264), shared from the kernel's root.
const UPPER: u64 = 0xffff_8000_0000_0000 + 2 * (1 << 41);
const LOWER_END: u64 = 1 << 47;
const UPPER_ENTRY: usize = ((UPPER >> 39) & 511) as usize;
/// Shadow table frames between clears (each maps up to 2 MiB of leaves).
const SHADOW_TABLES: usize = 1024;
/// Shadows kept filled for other regimes besides the current one:
/// `NK_STELA_SHADOW_CACHE` at build time, 8 by default; 0 clears and refills
/// one shadow on every regime switch.
const SHADOW_CACHE: usize = match option_env!("NK_STELA_SHADOW_CACHE") {
    Some(s) => parse_seconds(s) as usize,
    None => 8,
};
/// A range TLBI of more x86 pages than this clears the shadows instead.
const PAGE_INVALIDATION_LIMIT: u64 = 256;
/// The largest guest translation counted as a page (ARM's 64 KiB granule);
/// larger ones are block mappings.
const GUEST_PAGE_MAX: u64 = 64 << 10;
/// Block mappings a shadow records between clears.
const SHADOW_BLOCKS: usize = 64;

/// Guest tables read straight from guest DRAM (mapped at its own address).
struct DramTables;
impl mmu::TableMemory for DramTables {
    fn read_descriptor(&mut self, pa: u64) -> Option<u64> {
        (pa & 7 == 0 && DramHost::in_ram(pa, 8)).then(|| unsafe { core::ptr::read_volatile(pa as *const u64) })
    }
    fn update_descriptor(&mut self, _: u64, _: u64, _: u64) -> Option<bool> {
        None
    }
}

/// A private PML4 for the guest's core. Upper guest mappings use private
/// tables. Lower guest mappings can fill holes in nk's address space, after
/// copying each shared table along their path.
struct Shadow {
    root: *mut u64,
    kernel: *const u64,
    lower_private: bool,
    tables: Vec<*mut u8>,
    frames: Vec<u64>,
    regime: Regime,
    fills: u64,
    lower_fills: u64,
    refused: u64,
    clears: u64,
    /// Clears because the table budget ran out, not the guest's doing.
    budget_clears: u64,
    /// The largest guest page (not block) size filled since the last clear:
    /// macOS uses 16 KiB pages, so one guest page is four x86 pages.
    granule: u64,
    /// Guest block mappings (larger than a page) filled since the last
    /// clear, as (base, bytes); fixed capacity, as fills run in the trap.
    blocks: Vec<(u64, u64)>,
    /// More blocks were filled than `blocks` holds.
    blocks_overflowed: bool,
}

impl Shadow {
    fn new(frames: Vec<u64>, regime: Regime) -> Option<Self> {
        let kernel = crate::paging::table_of(crate::paging::kernel_address_space()) as *const u64;
        // nk must own nothing in the guest's upper range.
        if (UPPER_ENTRY..512).any(|i| unsafe { *kernel.add(i) } & crate::pte::VALID != 0) {
            return None;
        }
        let root = crate::frames::alloc()? as *mut u64;
        unsafe { core::ptr::copy_nonoverlapping(kernel, root, UPPER_ENTRY) };
        // Capacity fixed here: fills run in the #PF trap and never allocate
        // from the heap there.
        Some(Self { root, kernel, lower_private: false, tables: Vec::with_capacity(SHADOW_TABLES), frames, regime, fills: 0, lower_fills: 0, refused: 0, clears: 0, budget_clears: 0, granule: 4096, blocks: Vec::with_capacity(SHADOW_BLOCKS), blocks_overflowed: false })
    }
    fn cr3(&self) -> u64 {
        self.root as u64
    }
    /// Drop every guest mapping: the guest changed its translation.
    fn clear(&mut self) {
        if self.lower_private {
            unsafe { core::ptr::copy_nonoverlapping(self.kernel, self.root, UPPER_ENTRY) };
            self.lower_private = false;
        }
        for i in UPPER_ENTRY..512 {
            unsafe { *self.root.add(i) = 0 };
        }
        for table in self.tables.drain(..) {
            unsafe { crate::frames::free(table) };
        }
        self.granule = 4096;
        self.blocks.clear();
        self.blocks_overflowed = false;
        self.clears += 1;
    }
    /// Follow the next table or take a private copy of the corresponding
    /// kernel table. A host block mapping owns its entire span.
    unsafe fn next(&mut self, entry: *mut u64, host: u64) -> Option<*mut u64> {
        use crate::pte::{ADDR, HUGE, VALID, WRITABLE};
        if host & (VALID | HUGE) == VALID | HUGE {
            return None;
        }
        if *entry & VALID == 0 || *entry & ADDR == host & ADDR {
            if self.tables.len() == self.tables.capacity() {
                return None;
            }
            let table = crate::frames::alloc()?;
            self.tables.push(table);
            if host & VALID != 0 {
                core::ptr::copy_nonoverlapping((host & ADDR) as *const u64, table.cast(), 512);
            }
            *entry = table as u64 | (host & !ADDR) | VALID | WRITABLE;
        }
        Some((*entry & ADDR) as *mut u64)
    }
    unsafe fn map(&mut self, va: u64, frame: u64, writable: bool) -> bool {
        use crate::pte::{ADDR, VALID};
        let mut shadow = self.root;
        let mut host = self.kernel;
        if va < UPPER {
            // A partial fill can already have copied a table.
            self.lower_private = true;
        }
        for shift in [39, 30, 21] {
            let index = ((va >> shift) & 511) as usize;
            let host_entry = if host.is_null() { 0 } else { *host.add(index) };
            let Some(next) = self.next(shadow.add(index), host_entry) else { return false };
            shadow = next;
            host = if host_entry & VALID == 0 {
                core::ptr::null()
            } else {
                (host_entry & ADDR) as *const u64
            };
        }
        let index = ((va >> 12) & 511) as usize;
        if !host.is_null() && *host.add(index) & VALID != 0 {
            return false;
        }
        *shadow.add(index) = frame
            | crate::pte::VALID
            | crate::pte::NX
            | if writable { crate::pte::WRITABLE } else { 0 };
        true
    }
    /// What a TLBI of guest VAs `first..=last` removes: every x86 page of
    /// the guest pages it touches, or the whole shadow when it touches a
    /// filled block mapping (or more blocks were filled than recorded).
    /// True when it cleared everything.
    fn invalidate(&mut self, first: u64, last: u64) -> bool {
        let touches_block = self.blocks_overflowed
            || self.blocks.iter().any(|&(base, bytes)| first < base + bytes && base <= last);
        let granule = self.granule.max(4096);
        let start = first & !(granule - 1);
        let end = (last | (granule - 1)).wrapping_add(1);
        let pages = end.wrapping_sub(start) >> 12;
        if touches_block || end <= start || pages > PAGE_INVALIDATION_LIMIT {
            self.clear();
            return true;
        }
        for page in 0..pages {
            self.unmap(start + page * 4096);
        }
        false
    }
    /// Remove this shadow's own mapping of the page at `va`, if it has one;
    /// nk's own mappings (shared or copied from the kernel) stay.
    fn unmap(&mut self, va: u64) {
        use crate::pte::{ADDR, HUGE, VALID};
        if (LOWER_END..UPPER).contains(&va) || Self::mapped(self.kernel, va) {
            return;
        }
        let mut table = self.root;
        for shift in [39, 30, 21] {
            let entry = unsafe { *table.add(((va >> shift) & 511) as usize) };
            if entry & VALID == 0 || entry & HUGE != 0 {
                return;
            }
            table = (entry & ADDR) as *mut u64;
        }
        unsafe { *table.add(((va >> 12) & 511) as usize) = 0 };
    }
    fn mapped(mut root: *const u64, address: u64) -> bool {
        use crate::pte::{ADDR, HUGE, VALID};
        for shift in [39, 30, 21, 12] {
            let index = ((address >> shift) & 511) as usize;
            let entry = unsafe { *root.add(index) };
            if entry & VALID == 0 {
                return false;
            }
            if shift == 12 || entry & HUGE != 0 {
                return true;
            }
            root = (entry & ADDR) as *const u64;
        }
        unreachable!()
    }
    fn owns(&self, address: u64) -> bool {
        !Self::mapped(self.kernel, address) && Self::mapped(self.root, address)
    }
    fn translate(&self, address: u64, write: bool) -> mmu::Outcome {
        let access = if write { mmu::Access::Write } else { mmu::Access::Read };
        mmu::translate(&self.regime.controls, address, access, self.regime.requester, &mut DramTables)
    }
    /// Map the page at `address` if the guest maps it to DRAM with the
    /// access allowed; false leaves the fault to the engine.
    fn fill(&mut self, address: u64, write: bool) -> bool {
        if (LOWER_END..UPPER).contains(&address) {
            return false;
        }
        let page = address & !4095;
        let mmu::Outcome::Mapped(t) = self.translate(address, write) else {
            self.refused += 1;
            return false;
        };
        let physical = t.physical & !4095;
        let allowed = if write { t.writable } else { t.readable };
        if !allowed || !DramHost::in_ram(physical, 4096) || self.regime.watch.blocks(page)
            || self.regime.tag_blocks(page, t.attributes)
            || Self::mapped(self.kernel, page)
        {
            self.refused += 1;
            return false;
        }
        let frame = self.frames[((physical - DRAM) / 4096) as usize];
        if unsafe { !self.map(page, frame, t.writable) } {
            // A kept shadow can use up its table budget: start it afresh
            // (this frees frames and allocates nothing from the heap, so it
            // is safe in the trap) and map again.
            if self.tables.len() < self.tables.capacity() {
                return false;
            }
            self.clear();
            self.budget_clears += 1;
            if unsafe { !self.map(page, frame, t.writable) } {
                return false;
            }
        }
        // The page may have been present (read-only, now upgraded): its
        // old entry can be in this CPU's TLB, with the shadow root live.
        unsafe { core::arch::asm!("invlpg [{}]", in(reg) page, options(nostack)) };
        if t.size > GUEST_PAGE_MAX {
            let base = address & !(t.size - 1);
            if !self.blocks.iter().any(|&(b, n)| b == base && n == t.size) {
                if self.blocks.len() < self.blocks.capacity() {
                    self.blocks.push((base, t.size));
                } else {
                    self.blocks_overflowed = true;
                }
            }
        } else if t.size > self.granule {
            self.granule = t.size;
        }
        self.fills += 1;
        self.lower_fills += u64::from(address < LOWER_END);
        true
    }
}

impl Drop for Shadow {
    fn drop(&mut self) {
        self.clear();
        unsafe { crate::frames::free(self.root as *mut u8) };
    }
}

/// The #PF path's fill (`fault::Filler`).
unsafe fn shadow_fill(context: *mut (), address: u64, write: bool) -> bool {
    (*(context as *mut Shadow)).fill(address, write)
}

unsafe fn shadow_owns(context: *mut (), address: u64) -> bool {
    (*(context as *const Shadow)).owns(address)
}

/// Read the bundle manifest and stream its regions into guest DRAM on CPU 0.
fn prepare(dir: &str) -> Result<Prepared, String> {
    let source = Source::open(dir);
    crate::println!("STELA_SPTM: bundle from {}", source.describe());
    let bundle = Manifest::parse(&source.read("bundle.json")?)
        .map_err(|error| alloc::format!("{error}"))?;
    let hpm = source.read("USB-C_HPM,46-P01-AP.bin")?;
    let entry = bundle.entry.physical_address;
    let x0 = bundle.entry_x0;
    let read_only = bundle.read_only_region.as_ref().ok_or("no read-only region")?;
    let read_only = (read_only.base, read_only.bytes);
    let regions = &bundle.regions;
    let framebuffer = Framebuffer::at_dram_end(
        u64::from(SCANOUT_MODE.width),
        u64::from(SCANOUT_MODE.height),
        DRAM + RAM_BYTES,
    )
    .ok_or("guest framebuffer dimensions exceed DRAM")?;
    if bundle.options.dram_base != DRAM
        || bundle.options.phys_base < DRAM
        || bundle.top_of_kernel_data > framebuffer.guest_pa
        || bundle.carveouts.iter().any(|range| {
            range.base < DRAM
                || range.base.checked_add(range.bytes).is_none_or(|end| end > framebuffer.guest_pa)
        })
    {
        return Err("bundle layout does not leave room for the guest framebuffer".into());
    }
    for region in regions {
        if region.guest_pa < DRAM
            || region.bytes > region.memory_bytes
            || region.guest_pa.checked_add(region.memory_bytes)
                .is_none_or(|end| end > framebuffer.guest_pa)
        {
            return Err(alloc::format!("region {} is outside managed guest DRAM", region.name));
        }
    }

    // Frame by frame: guest DRAM is mapped page by page, so it need not be
    // physically contiguous, and contiguous runs are scarce.
    let pages = (RAM_BYTES / 4096) as usize;
    let mut frames = Vec::with_capacity(pages);
    let high_before = crate::frames::high_free();
    for page in 0..pages as u64 {
        let frame = match crate::frames::alloc_high() {
            Some(frame) => frame,
            None => match crate::frames::alloc() {
                Some(frame) => frame as u64,
                None => {
                    release_dram(&frames);
                    return Err(alloc::format!("out of frames at {} MiB of guest DRAM", page >> 8));
                }
            },
        };
        frames.push(frame);
        let va = DRAM + page * 4096;
        if !unsafe { crate::paging::map_guest_dram(va, frame) } {
            release_dram(&frames);
            return Err(String::from("guest DRAM window not reserved"));
        }
        if frame >= IDENTITY_LIMIT {
            // High frames are not zeroed by the allocator, which cannot see
            // them; the window can. The new mapping is not in any TLB yet.
            unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096) };
        }
    }
    crate::arch::tlb_flush_all();
    crate::println!(
        "STELA_SPTM: guest DRAM {} MiB: {} MiB above 4 GiB, {} MiB from nk's frames",
        RAM_BYTES >> 20,
        (high_before - crate::frames::high_free()) >> 20,
        (RAM_BYTES - (high_before - crate::frames::high_free())) >> 20
    );

    let load = || -> Result<(Vec<u8>, u64), String> {
        let mut loaded = 0;
        for region in regions.iter().filter(|region| region.bytes != 0) {
            source.pread_into(&region.file, region.file_offset, region.guest_pa, region.bytes)?;
            loaded += region.bytes;
        }

        // The cacheless T8112 memory plan needs the same flag as stela.efi.
        let mut command_line = bundle.boot_args.command_line.clone();
        if !command_line.split_ascii_whitespace().any(|word| word == "-disable_mcc") {
            command_line.push_str(" -disable_mcc");
        }
        crate::println!("STELA_SPTM: boot-args: {}", command_line);
        let machine = BootMachine {
            dram_base: DRAM,
            ram_bytes: RAM_BYTES,
            dram_bytes: RAM_BYTES,
            timebase_hz: 24_000_000,
            framebuffer: Some(framebuffer),
            boot_args: Some(command_line),
            nvram: None,
        };
        for patch in &bundle.patches {
            let value = bundle
                .patch_value(patch, &machine, &mut super::mount::rdrand_fill)
                .map_err(|error| alloc::format!("{error}"))?;
            let Some(value) = value else {
                if patch.required {
                    return Err(alloc::format!("patch {} has no value", patch.field));
                }
                continue;
            };
            let pa = bundle.patch_address(patch).map_err(|error| alloc::format!("{error}"))?;
            // The region and patch extent have been checked before writing.
            unsafe { core::ptr::copy_nonoverlapping(value.as_ptr(), pa as *mut u8, value.len()) };
            if patch.source == PatchSource::Entropy {
                crate::println!("STELA_SPTM: patched {} ({} random bytes at {:#x})", patch.field, value.len(), pa);
            }
        }
        Ok((prepared_tree(regions, x0)?, loaded))
    };
    let (tree, loaded) = match load() {
        Ok(result) => result,
        Err(error) => {
            release_dram(&frames);
            return Err(error);
        }
    };
    crate::println!(
        "STELA_SPTM: boot framebuffer {}x{} at {:#x}, stride {}, {:#x} bytes",
        framebuffer.width, framebuffer.height, framebuffer.guest_pa, framebuffer.row_bytes, framebuffer.bytes
    );
    crate::println!(
        "STELA_SPTM: {} regions, {} MiB loaded into guest DRAM at {:#x} ({} MiB backed)",
        regions.len(), loaded >> 20, DRAM, RAM_BYTES >> 20
    );
    Ok(Prepared {
        tree,
        framebuffer,
        disk: None,
        restore: bundle.boot_args.command_line.split_ascii_whitespace().any(|word| word == "-restore"),
        hpm,
        frames,
        entry,
        x0,
        read_only,
    })
}

/// Keep older bundles' PMGR metadata consistent with their hidden ANE.
/// Apply this after all file-offset patches, which refer to the original ADT.
fn prepared_tree(regions: &[Region], x0: u64) -> Result<Vec<u8>, String> {
    use stela_loader::{apple_device_tree::DeviceTree, boot_args::offset::DEVICE_TREE_LENGTH};

    let region = |name| {
        regions
            .iter()
            .find(|region| region.name == name)
            .map(|region| (region.guest_pa, region.bytes))
    };
    let (pa, bytes) = region("DeviceTree").ok_or("missing DeviceTree region")?;
    if bytes == 0 || !DramHost::in_ram(pa, bytes as usize) {
        return Err("DeviceTree region outside guest DRAM".into());
    }
    // The region has been loaded and patched on CPU 0, before the guest starts.
    let loaded = unsafe { core::slice::from_raw_parts_mut(pa as *mut u8, bytes as usize) };
    let mut tree = DeviceTree::parse(loaded).map_err(|error| alloc::format!("DeviceTree: {error}"))?;
    if !stela_loader::devices::withdraw_hidden_ane_energy(&mut tree) {
        return Ok(loaded.to_vec());
    }
    let (args_pa, args_bytes) = region("BootArgs").ok_or("missing BootArgs region")?;
    if args_pa != x0
        || args_bytes < (DEVICE_TREE_LENGTH + 4) as u64
        || !DramHost::in_ram(args_pa, args_bytes as usize)
        || (args_pa < pa + bytes && pa < args_pa + args_bytes)
    {
        return Err("BootArgs region cannot supply the DeviceTree length".into());
    }
    let length = unsafe { (args_pa as *mut u8).add(DEVICE_TREE_LENGTH).cast::<u32>() };
    if u64::from(u32::from_le(unsafe { length.read_unaligned() })) != bytes {
        return Err("BootArgs DeviceTree length differs from the loaded region".into());
    }
    let tree = tree.serialize();
    loaded[..tree.len()].copy_from_slice(&tree);
    loaded[tree.len()..].fill(0);
    unsafe { length.write_unaligned((tree.len() as u32).to_le()) };
    crate::println!("STELA_SPTM: withdrawn PMGR energy counter for hidden ANE");
    Ok(tree)
}

/// Where the bundle comes from: files in a directory of Linux's rootfs (the
/// initrd), or a cpio newc archive written raw to the first NVMe disk -- for
/// a bundle the initrd cannot carry (a restore ramdisk is ~360 MiB). Files on
/// the disk are read straight into guest DRAM with pread, never copied into
/// Linux's memory.
enum Source {
    Dir(String),
    Disk { fd: i64, files: Vec<(String, u64, u64)> },
}

impl Source {
    fn open(dir: &str) -> Self {
        Self::disk().unwrap_or_else(|| Self::Dir(String::from(dir)))
    }
    fn describe(&self) -> String {
        match self {
            Self::Dir(dir) => alloc::format!("{dir} (initrd)"),
            Self::Disk { files, .. } => alloc::format!("/dev/nvme0n1 ({} files, cpio)", files.len()),
        }
    }
    /// The first NVMe namespace through devtmpfs, if it holds a cpio archive.
    fn disk() -> Option<Self> {
        const MKDIRAT: i64 = 34;
        const MOUNT: i64 = 40;
        let _ = crate::lkl::syscall(MKDIRAT, [-100, c"/stele-dev".as_ptr() as i64, 0o755, 0, 0, 0]);
        let _ = crate::lkl::syscall(MOUNT, [c"devtmpfs".as_ptr() as i64, c"/stele-dev".as_ptr() as i64,
            c"devtmpfs".as_ptr() as i64, 0, 0, 0]);
        // Linux probes NVMe asynchronously: the namespace can appear a few
        // seconds after boot. Wait up to 10 s (asm-generic nanosleep 101).
        let mut fd = -2;
        for _ in 0..40 {
            fd = crate::lkl::syscall(56, [-100, c"/stele-dev/nvme0n1".as_ptr() as i64, 0, 0, 0, 0]);
            if fd >= 0 {
                break;
            }
            let quarter: [i64; 2] = [0, 250_000_000];
            crate::lkl::syscall(101, [quarter.as_ptr() as i64, 0, 0, 0, 0, 0]);
        }
        if fd < 0 {
            crate::println!("STELA_SPTM: no bundle disk: /dev/nvme0n1 errno {}", -fd);
            return None;
        }
        let mut files = Vec::new();
        let mut at = 0u64;
        let mut header = [0u8; 110 + 256];
        loop {
            if pread(fd, &mut header, at) != header.len() as i64 || &header[..6] != b"070701" {
                break;
            }
            let field = |i: usize| {
                core::str::from_utf8(&header[6 + i * 8..14 + i * 8]).ok().and_then(|h| u64::from_str_radix(h, 16).ok())
            };
            let (Some(size), Some(namesize)) = (field(6), field(11)) else { break };
            let namesize = namesize as usize;
            if namesize == 0 || namesize > 256 {
                break;
            }
            let name = String::from_utf8_lossy(&header[110..110 + namesize - 1]).into_owned();
            if name == "TRAILER!!!" {
                break;
            }
            let data = (at + 110 + namesize as u64 + 3) & !3;
            files.push((name, data, size));
            at = (data + size + 3) & !3;
        }
        if files.is_empty() {
            crate::lkl::syscall(57, [fd, 0, 0, 0, 0, 0]);
            return None;
        }
        Some(Self::Disk { fd, files })
    }
    fn find(&self, name: &str) -> Result<(i64, u64, u64), String> {
        let Self::Disk { fd, files } = self else { unreachable!() };
        files
            .iter()
            .find(|(n, _, _)| n == name || n.rsplit('/').next() == Some(name))
            .map(|&(_, data, size)| (*fd, data, size))
            .ok_or_else(|| alloc::format!("{name}: not on the bundle disk"))
    }
    fn read(&self, name: &str) -> Result<Vec<u8>, String> {
        match self {
            Self::Dir(dir) => super::board::read_linux_file(&alloc::format!("{dir}/{name}"))
                .map_err(|errno| alloc::format!("{dir}/{name}: errno {}", -errno)),
            Self::Disk { .. } => {
                let (fd, data, size) = self.find(name)?;
                let mut bytes = vec![0u8; size as usize];
                let mut done = 0usize;
                while done < bytes.len() {
                    let n = pread(fd, &mut bytes[done..], data + done as u64);
                    if n <= 0 {
                        return Err(alloc::format!("{name}: short read on the bundle disk"));
                    }
                    done += n as usize;
                }
                Ok(bytes)
            }
        }
    }
    /// `bytes` of `file` from `offset`, into guest DRAM at `pa`.
    fn pread_into(&self, file: &str, offset: u64, pa: u64, bytes: u64) -> Result<(), String> {
        match self {
            Self::Dir(dir) => pread_into(&alloc::format!("{dir}/{file}"), offset, pa, bytes),
            Self::Disk { .. } => {
                let (fd, data, size) = self.find(file)?;
                if offset + bytes > size {
                    return Err(alloc::format!("{file}: region past the file's end"));
                }
                copy_into(fd, data + offset, pa, bytes, file)
            }
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        if let Self::Disk { fd, .. } = self {
            crate::lkl::syscall(57, [*fd, 0, 0, 0, 0, 0]);
        }
    }
}

/// asm-generic pread64 (67).
fn pread(fd: i64, out: &mut [u8], at: u64) -> i64 {
    crate::lkl::syscall(67, [fd, out.as_mut_ptr() as i64, out.len() as i64, at as i64, 0, 0])
}

/// `bytes` from `fd` at `at` into guest DRAM at `pa`, through a bounce buffer.
fn copy_into(fd: i64, at: u64, pa: u64, bytes: u64, what: &str) -> Result<(), String> {
    let mut chunk = vec![0u8; 1 << 20];
    let mut done = 0u64;
    while done < bytes {
        let want = (bytes - done).min(chunk.len() as u64) as usize;
        let n = pread(fd, &mut chunk[..want], at + done);
        if n <= 0 {
            return Err(alloc::format!("{what}: short read at {} (errno {})", at + done, -n));
        }
        unsafe { core::ptr::copy_nonoverlapping(chunk.as_ptr(), (pa + done) as *mut u8, n as usize) };
        done += n as u64;
    }
    Ok(())
}

/// `bytes` of `path` from `offset`, into guest DRAM at `pa` (asm-generic
/// openat 56, pread64 67, close 57).
fn pread_into(path: &str, offset: u64, pa: u64, bytes: u64) -> Result<(), String> {
    let mut name = Vec::from(path.as_bytes());
    name.push(0);
    let fd = crate::lkl::syscall(56, [-100, name.as_ptr() as i64, 0, 0, 0, 0]);
    if fd < 0 {
        return Err(alloc::format!("{path}: errno {}", -fd));
    }
    let mut chunk = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    let result = loop {
        if done == bytes {
            break Ok(());
        }
        let want = (bytes - done).min(chunk.len() as u64) as usize;
        let n = crate::lkl::syscall(67, [fd, chunk.as_mut_ptr() as i64, want as i64, (offset + done) as i64, 0, 0]);
        if n <= 0 {
            break Err(alloc::format!("{path}: short read at {} (errno {})", offset + done, -n));
        }
        unsafe { core::ptr::copy_nonoverlapping(chunk.as_ptr(), (pa + done) as *mut u8, n as usize) };
        done += n as u64;
    };
    crate::lkl::syscall(57, [fd, 0, 0, 0, 0, 0]);
    result
}

pub(crate) fn sptm_probe(dir: &str) -> bool {
    static PASSED: AtomicBool = AtomicBool::new(false);
    let mut prepared = match prepare(dir) {
        Ok(prepared) => prepared,
        Err(error) => {
            crate::println!("STELA_SPTM: cannot prepare the bundle: {}", error);
            crate::println!("STELA_SPTM_FAIL");
            return false;
        }
    };
    let mut disk_worker = None;
    if option_env!("NK_STELA_SPTM_NVME_SSD") == Some("1") {
        match super::disk::Worker::open() {
            Ok((worker, disk)) => {
                disk_worker = Some(worker);
                prepared.disk = Some(disk);
            }
            Err(error) => {
                release_dram(&prepared.frames);
                crate::println!("STELA_SPTM: {error}");
                crate::println!("STELA_SPTM_FAIL");
                return false;
            }
        }
    }
    unsafe { *(&raw mut PREPARED) = Some(prepared) };
    extern "C" fn on_core(_: usize) {
        const PAGES: usize = 512;
        let stack = crate::frames::alloc_contiguous(PAGES).expect("SPTM stack");
        let flags = crate::sync::irq_save();
        unsafe {
            nk_stela_on_stack(stack.add(PAGES * 4096), run_sptm);
            crate::sync::irq_restore(flags);
            for page in 0..PAGES {
                crate::frames::free(stack.add(page * 4096));
            }
        }
    }
    extern "C" fn run_sptm() {
        PASSED.store(enter_sptm(), Ordering::Release);
    }
    let Some(id) = crate::sched::dedicate_cpu(GUEST_CPU, "vcpu-sptm", on_core, 0) else {
        if let Some(prepared) = unsafe { (*(&raw mut PREPARED)).take() } {
            release_dram(&prepared.frames);
        }
        crate::println!("STELA_SPTM: dedicate_cpu(3) refused (needs NK_SMP)");
        return false;
    };
    if let Some(worker) = &mut disk_worker {
        while !crate::sched::finished(id) {
            if !worker.service(GUEST_CPU) {
                crate::sched::sleep_ticks(1);
            }
        }
    }
    crate::sched::join(id);
    let passed = PASSED.load(Ordering::Acquire);
    crate::println!("{}", if passed { "STELA_SPTM_PASS" } else { "STELA_SPTM_FAIL" });
    passed
}

fn enter_sptm() -> bool {
    let Some(p) = (unsafe { (*(&raw mut PREPARED)).take() }) else { return false };
    let frames = p.frames.clone();
    // Everything holding guest DRAM -- machine, board, canonical RAM -- is
    // dropped inside `run_board`, on every path, before it is given back.
    let passed = run_board(p);
    release_dram(&frames);
    passed
}

fn report_restore_usb(log: stela_soc::machine::UsbLog) {
    use stela_runtime::restore::{plist, session::PROTOCOL};
    use stela_soc::machine::UsbLog;

    match log {
        UsbLog::Descriptor(descriptor) if descriptor.kind == 2 => {
            crate::println!("STELA_SPTM: USB host read a guest configuration descriptor");
        }
        UsbLog::QueryTypeReply(bytes) => {
            let supported = plist::decode(&bytes, plist::Limits::default()).is_ok_and(|reply| {
                reply.required("Type").and_then(plist::Value::string) == Ok("com.apple.mobile.restored")
                    && reply.required("Result").and_then(plist::Value::string) == Ok("Success")
                    && reply.required("RestoreProtocolVersion").and_then(plist::Value::uint) == Ok(PROTOCOL)
            });
            if supported {
                crate::println!("STELA_SPTM: restored answered QueryType over USB (protocol {PROTOCOL})");
            } else {
                crate::println!("STELA_SPTM: unsupported restored QueryType reply ({} bytes)", bytes.len());
            }
        }
        UsbLog::MuxRejected(bytes) => {
            crate::println!("STELA_SPTM: USB mux rejected guest data ({} logged bytes)", bytes.len());
        }
        _ => {}
    }
}

fn run_board(p: Prepared) -> bool {
    let board = match stela_soc_data::board::board(&p.tree) {
        Ok(board) => board,
        Err(error) => {
            crate::println!("STELA_SPTM: DeviceTree refused: {:?}", error);
            return false;
        }
    };
    // The same frames, as the canonical RAM the board's memory owners check.
    // One range per physically adjacent run of frames; the window maps them
    // at consecutive guest addresses.
    struct Kept;
    let mut ranges: Vec<ReservedSystemRamRange> = Vec::new();
    for (page, &frame) in p.frames.iter().enumerate() {
        let source = DRAM + page as u64 * 4096;
        if let Some(last) = ranges.last_mut() {
            if last.host_pa + last.bytes as u64 == frame {
                last.bytes += 4096;
                continue;
            }
        }
        ranges.push(ReservedSystemRamRange {
            source_pa: source,
            host_pa: frame,
            host_base: core::ptr::NonNull::new(source as *mut u8).expect("guest DRAM window"),
            bytes: 4096,
        });
    }
    crate::println!("STELA_SPTM: guest DRAM is {} physical runs", ranges.len());
    let canonical = match unsafe { RegisteredSystemRam::register(&ranges, Arc::new(Kept), 64, 8) } {
        Ok(ram) => Arc::new(ram),
        Err(error) => {
            crate::println!("STELA_SPTM: RAM registration refused: {:?}", error);
            return false;
        }
    };
    // CTRR-A over the bundle's read-only run; the bundle has no core locks.
    let programmed = stela_soc_data::amcc::ctrr_a(Some(p.read_only));
    let plan = t8112_cold::Configuration::clear_software_controls(ZeroWayConfiguration::DisableArgument)
        .and_then(|c| t8112_cold::Plan::prepare_ap_frontend(&board, &canonical, programmed, &c));
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => {
            crate::println!("STELA_SPTM: T8112 memory plan refused: {:?}", error);
            return false;
        }
    };
    let hpm: &'static [u8] = Box::leak(p.hpm.into_boxed_slice());
    let firmware = move |name: &str| (name == "USB-C_HPM,46-P01-AP.bin").then_some(hpm);
    let console_bytes = Arc::new(core::sync::atomic::AtomicU64::new(0));
    let backing: Box<dyn stela_driver_base::block::BlockDevice> = match p.disk {
        Some(mut disk) => {
            use stela_driver_base::block::BlockDevice;
            if let Err(error) = disk.read(0, &mut [0; 4096]).and_then(|()| disk.flush()) {
                crate::println!("STELA_SPTM: cannot check the LKL SSD: {error}");
                return false;
            }
            Box::new(disk)
        }
        None => Box::new(super::mount::SparseDisk::new(super::mount::SSD_BLOCKS)),
    };
    let restore_usb = p.restore && option_env!("NK_STELA_SPTM_RESTORE_USB") == Some("1");
    let inputs = mount::Inputs {
        programmed,
        reset: Some(p.entry),
        now: 0,
        wall: None,
        rtc_anchor: None,
        backing: Some(backing),
        sep_state: None,
        sep_observe: false,
        sep_recovery_policy_nonce: None,
        entropy: Box::new(super::mount::rdrand_fill),
        restore: restore_usb,
        input: false,
        usb: UsbCables::default(),
        scanout: Some(Box::new(Headless)),
        firmware: &firmware,
        build: stela_formats::builds::find("26B131").ok(),
    };
    let console = Console { line: Vec::new(), bytes: console_bytes.clone() };
    let mut log = |arguments: core::fmt::Arguments<'_>| {
        let text = alloc::format!("{arguments}");
        for line in text.lines().filter(|l| l.starts_with("DEVICES uart")) {
            crate::println!("  mount: {}", line);
        }
    };
    let (mut devices, usb) = match mount::devices_with_memory(&board, console, inputs, Some(MemoryResources::T8112Cold(plan)), &mut log) {
        Ok(mounted) => mounted,
        Err(error) => {
            crate::println!("STELA_SPTM: mount refused: {:?}", error);
            return false;
        }
    };
    if restore_usb {
        let result = usb
            .ok_or(stela_contracts::refuse!("the board has no restore USB port"))
            .and_then(|usb| devices.usb_client(usb)?.query_type());
        if let Err(error) = result {
            crate::println!("STELA_SPTM: restore USB probe refused: {error}");
            return false;
        }
        crate::println!("STELA_SPTM: restore USB host cabled, QueryType queued");
    }

    let chip = board.cpus.chip;
    let Some(host) = DramHost::new(p.frames.clone(), restore_usb) else {
        crate::println!("STELA_SPTM: no frames for the code arena");
        return false;
    };
    let target = canonical.target();
    let mut fixed = identification::derive(chip, target, true, identification::Speculation::NONE);
    fixed.push((stela_engine::machine::registers::encodings::DCZID_EL0, 4));
    let profile = CpuProfile {
        name: chip.name(),
        chip,
        counter_frequency: 24_000_000,
        fixed,
        processors: board.cpus.processors.clone(),
    };
    let mut cpu = Cpu::of(chip, p.entry, Pstate::reset(2));
    cpu.state.x[0] = p.x0;
    crate::println!("STELA_SPTM: entry pc={:#x} x0={:#x} el=2 mmu=off, {} PEs in the profile", p.entry, p.x0, profile.processors.len());
    let mut machine = Machine::new(cpu, host, devices, profile, target);

    let start = crate::timer::time_ns();
    let mut slices = 0u64;
    let mut reported = start;
    let mut framebuffer_drawn = false;
    let mut usb_status = Vec::new();
    let (mut last_bytes, mut last_output) = (0u64, start);
    let stop = loop {
        // The budget is a total: the engine stops once its block count
        // reaches it, so each slice asks for SLICE_BLOCKS more.
        let stop = machine.run(machine.statistics.blocks + SLICE_BLOCKS);
        for requirement in machine.take_unmodeled() {
            crate::println!("STELA_SPTM: degraded {requirement}");
        }
        while let Some(log) = machine.devices.take_usb_log() {
            report_restore_usb(log);
        }
        slices += 1;
        if crate::timer::time_ns() - reported > 10_000_000_000 {
            reported = crate::timer::time_ns();
            if restore_usb {
                let status = machine.devices.usb_diagnostics();
                if status != usb_status {
                    for port in &status {
                        crate::println!("STELA_SPTM: restore USB {port}");
                    }
                    usb_status = status;
                }
            }
            if !framebuffer_drawn {
                let framebuffer = p.framebuffer;
                let pixels = framebuffer.row_bytes * framebuffer.height / 4;
                let base = framebuffer.guest_pa as *const u32;
                framebuffer_drawn = (0..pixels as usize)
                    .any(|pixel| unsafe { base.add(pixel).read_volatile() } & 0x00ff_ffff != 0);
                if framebuffer_drawn {
                    crate::println!("STELA_SPTM: guest drew into the boot framebuffer");
                }
            }
            crate::println!(
                "STELA_SPTM: ... {} s: {} blocks, {} native runs, pc {:#x}, console bytes {}",
                (reported - start) / 1_000_000_000, machine.statistics.blocks, machine.host.executions,
                machine.cpu.pc, console_bytes.load(Ordering::Relaxed)
            );
        }
        // A console silent this long is a hang: stop, so the state dump
        // below shows where the guest waits.
        let bytes = console_bytes.load(Ordering::Relaxed);
        if bytes != last_bytes {
            last_bytes = bytes;
            last_output = crate::timer::time_ns();
        }
        let stalled = crate::timer::time_ns() - last_output > STALL_NS;
        if stalled {
            crate::println!("STELA_SPTM: console silent for {} s: stopping to dump state", STALL_NS / 1_000_000_000);
        }
        let more = matches!(stop, Stop::Budget | Stop::Waiting) && crate::timer::time_ns() - start < RUN_NS && !stalled;
        if !more {
            break stop;
        }
    };
    let elapsed_ms = (crate::timer::time_ns() - start) / 1_000_000;
    let blocks = machine.statistics.blocks;
    let pc = machine.cpu.pc;
    crate::println!(
        "STELA_SPTM: stop {:?} after {} blocks ({} native runs, {} native faults) in {} ms; pc {:#x}, console bytes {}",
        stop, blocks, machine.host.executions, machine.host.faults, elapsed_ms, pc,
        console_bytes.load(Ordering::Relaxed)
    );
    {
        use stela_engine::machine::system::Register as R;
        let r = &machine.cpu.registers;
        crate::println!(
            "STELA_SPTM: x0 {:#x}; ESR_EL2 {:#x} ELR_EL2 {:#x} FAR_EL2 {:#x}; ESR_EL1 {:#x} ELR_EL1 {:#x} FAR_EL1 {:#x}; VBAR_EL2 {:#x} VBAR_EL1 {:#x}",
            machine.cpu.state.x[0],
            r.get(R::ESR_EL2, 0), r.get(R::ELR_EL2, 0), r.get(R::FAR_EL2, 0),
            r.get(R::ESR_EL1, 0), r.get(R::ELR_EL1, 0), r.get(R::FAR_EL1, 0),
            r.get(R::VBAR_EL2, 0), r.get(R::VBAR_EL1, 0)
        );
    }
    if let Some((sctlr, at)) = machine.host.mmu {
        crate::println!("STELA_SPTM: SPTM turned its MMU on (SCTLR {:#x}) after {} native runs", sctlr, at);
    }
    // Where the guest is: its registers and the code around its PC, read
    // through its own translation, to disassemble on the host.
    if let Some(shadow) = machine.host.shadow.as_ref() {
        use stela_engine::machine::system::Register as R;
        let st = &machine.cpu.state;
        for row in 0..8 {
            let i = row * 4;
            crate::println!(
                "STELA_SPTM: x{:<2} {:#018x} {:#018x} {:#018x} {:#018x}",
                i, st.x[i], st.x[i + 1], st.x[i + 2], if i + 3 < 31 { st.x[i + 3] } else { 0 }
            );
        }
        let r = &machine.cpu.registers;
        crate::println!(
            "STELA_SPTM: CNTV_CTL {:#x} CNTV_CVAL {:#x} CNTP_CTL {:#x} CNTP_CVAL {:#x} counter {:#x}; DAIF {:#x}",
            r.get(R::CNTV_CTL_EL0, 0), r.get(R::CNTV_CVAL_EL0, 0),
            r.get(R::CNTP_CTL_EL0, 0), r.get(R::CNTP_CVAL_EL0, 0),
            machine.host.counter_now(), machine.cpu.pstate.daif
        );
        for (what, at) in [("lr", st.x[30] & 0x0000_ffff_ffff_ffff | 0xffff_0000_0000_0000), ("pc", machine.cpu.pc)] {
        let pc = at;
        let from = (pc & !3).saturating_sub(0x60);
        if let mmu::Outcome::Mapped(t) = shadow.translate(from, false) {
            let mut words = [0u32; 48];
            for (n, w) in words.iter_mut().enumerate() {
                let pa = (t.physical & !4095) + ((from & 4095) + n as u64 * 4);
                if DramHost::in_ram(pa, 4) {
                    *w = unsafe { core::ptr::read_volatile(pa as *const u32) };
                }
            }
            crate::println!("STELA_SPTM: code at {} from {:#x} (PA {:#x}):", what, from, t.physical);
            for chunk in words.chunks(8) {
                let line: alloc::vec::Vec<alloc::string::String> =
                    chunk.iter().map(|w| alloc::format!("{:08x}", w)).collect();
                crate::println!("STELA_SPTM:   {}", line.join(" "));
            }
        }
        }
    }
    if let Some(shadow) = machine.host.shadow.as_ref() {
        crate::println!(
            "STELA_SPTM: shadow: {} pages filled ({} lower), {} refused, {} clears ({} for the table budget); {} native faults below the upper range",
            shadow.fills, shadow.lower_fills, shadow.refused, shadow.clears, shadow.budget_clears, machine.host.lower_faults);
        crate::println!(
            "STELA_SPTM: shadow cache: {} regime switches, {} served by a kept shadow; TLBIs: {} page-wise, {} full",
            machine.host.switches, machine.host.reused, machine.host.page_invalidations, machine.host.full_invalidations
        );
    }
    // Slice 1 passes once SPTM's own code ran natively on the board and the
    // run ended at the MMU or a guest-visible stop, not a host refusal.
    blocks > 100 && (machine.host.mmu.is_some() || matches!(stop, Stop::Budget))
}

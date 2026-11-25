// SPDX-License-Identifier: GPL-2.0-or-later
//! macOS 27's first instructions on nk: the bundle loaded into guest DRAM
//! and SPTM entered on the mounted M2 board (ROADMAP Phase 5,
//! NK_STELA_SPTM_TEST). Slice 1 of the host port: MMU off only.
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
//! `DramHost` has no shadow page tables yet, so the run ends where SPTM
//! turns its MMU on (`set_regime` records the regime that asks), or at the
//! first stop before that.

use super::*;
use alloc::{boxed::Box, string::String, sync::Arc, vec};
use core::sync::atomic::{AtomicBool, Ordering};
use stela_engine::memory::{RegisteredSystemRam, ReservedSystemRamRange};
use stela_runtime::machine::mount;
use stela_soc::machine::memory_resources::{t8112::cache::ZeroWayConfiguration, MemoryResources};
use stela_soc::machine::{t8112_cold, ConsoleSink, UsbCables};
use stela_engine::machine::FaultCause;

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
/// Frames below this are nk's own (identity mapped) and go back to it.
const IDENTITY_LIMIT: u64 = 1 << 32;
/// Native code arena: 4 MiB of frames, one recovery range.
const CODE_PAGES: usize = 1024;
/// Blocks per `run` call, between checks of the regime.
const SLICE_BLOCKS: u64 = 2_000;
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

struct Headless;
impl stela_soc::scanout::Scanout for Headless {
    fn mode(&self) -> stela_soc::scanout::Mode {
        stela_soc::scanout::Mode {
            width: 1920,
            height: 1080,
            format: stela_soc::scanout::PixelFormat::Bgra8888,
        }
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
    /// The MMU-on shadow, once the guest turned its MMU on.
    shadow: Option<Box<Shadow>>,
    /// Native accesses below `UPPER` with the MMU on that faulted: the
    /// guest's lower range is not shadowed yet.
    lower_faults: u64,
}

impl DramHost {
    fn new(frames: Vec<u64>) -> Option<Self> {
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
            lower_faults: 0,
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
        // MMU on: run on the shadow root; not-present upper accesses fill
        // in the trap and retry.
        let original = crate::arch::current_page_table();
        crate::arch::set_page_table(shadow.cr3());
        let context = &mut **shadow as *mut Shadow as *mut ();
        let raw = super::super::fault::invoke_filling(
            self.entry.0, at.0, state, core::iter::once(range), DRAM, None,
            Some((shadow_fill, context)),
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
                    mmu::Outcome::Mapped(t) if !DramHost::in_ram(t.physical, 1) => {
                        FaultCause::Device { physical: t.physical }
                    }
                    // RAM the shadow does not map: the lower range, or a
                    // watched or tag-checked page.
                    mmu::Outcome::Mapped(t) => FaultCause::Device { physical: t.physical },
                };
                NativeExit::Fault(capture.native(cause))
            }
        }
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
        match self.shadow.as_mut() {
            Some(shadow) => {
                shadow.clear();
                shadow.regime = *regime;
            }
            None => self.shadow = Shadow::new(self.frames.clone(), *regime).map(Box::new),
        }
    }
    fn invalidate_translations(&mut self) {
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.clear();
        }
    }
    fn counter(&mut self) -> u64 {
        guest_count(self.tsc_base)
    }
    fn arm_timer(&mut self, deadline: Option<u64>) {
        GUEST_TIMER.store(false, Ordering::Release);
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
            while !GUEST_TIMER.swap(false, Ordering::AcqRel) && !super::irq::take_kick() {
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
        GUEST_TIMER.swap(false, Ordering::AcqRel)
    }
}

// --- slice 2: shadow page tables for the guest's upper range ---------------

/// The engine's four-level placement with two slices: guest addresses at or
/// above this run natively at the same host address. Below it is nk's own
/// half (PML4 entries 0..264), shared from the kernel's root.
const UPPER: u64 = 0xffff_8000_0000_0000 + 2 * (1 << 41);
const UPPER_ENTRY: usize = ((UPPER >> 39) & 511) as usize;
/// Shadow table frames between clears (each maps up to 2 MiB of leaves).
const SHADOW_TABLES: usize = 1024;

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

/// A private PML4 for the guest's core: nk's entries below `UPPER_ENTRY`
/// copied from the kernel root, and the guest's upper range in tables this
/// owns, filled on demand from the guest's own translation.
struct Shadow {
    root: *mut u64,
    tables: Vec<*mut u8>,
    frames: Vec<u64>,
    regime: Regime,
    fills: u64,
    refused: u64,
    clears: u64,
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
        Some(Self { root, tables: Vec::with_capacity(SHADOW_TABLES), frames, regime, fills: 0, refused: 0, clears: 0 })
    }
    fn cr3(&self) -> u64 {
        self.root as u64
    }
    /// Drop every guest mapping: the guest changed its translation.
    fn clear(&mut self) {
        for i in UPPER_ENTRY..512 {
            unsafe { *self.root.add(i) = 0 };
        }
        for table in self.tables.drain(..) {
            unsafe { crate::frames::free(table) };
        }
        self.clears += 1;
    }
    /// The next-level table under `entry`, made when absent. Private: only
    /// entries at or above `UPPER_ENTRY` reach here.
    unsafe fn next(&mut self, entry: *mut u64) -> Option<*mut u64> {
        if *entry & crate::pte::VALID == 0 {
            if self.tables.len() == self.tables.capacity() {
                return None;
            }
            let table = crate::frames::alloc()?;
            self.tables.push(table);
            *entry = table as u64 | crate::pte::VALID | crate::pte::WRITABLE;
        }
        Some((*entry & crate::pte::ADDR) as *mut u64)
    }
    unsafe fn map(&mut self, va: u64, frame: u64, writable: bool) -> bool {
        let index = |shift: u32| ((va >> shift) & 511) as usize;
        let Some(l3) = self.next(self.root.add(index(39))) else { return false };
        let Some(l2) = self.next(l3.add(index(30))) else { return false };
        let Some(l1) = self.next(l2.add(index(21))) else { return false };
        *l1.add(index(12)) = frame
            | crate::pte::VALID
            | crate::pte::NX
            | if writable { crate::pte::WRITABLE } else { 0 };
        true
    }
    fn watched(&self, page: u64) -> bool {
        self.regime
            .watch
            .ranges
            .iter()
            .any(|&(first, last)| first <= last && first <= page + 4095 && page <= last)
    }
    fn translate(&self, address: u64, write: bool) -> mmu::Outcome {
        let access = if write { mmu::Access::Write } else { mmu::Access::Read };
        mmu::translate(&self.regime.controls, address, access, self.regime.requester, &mut DramTables)
    }
    /// Map the page at `address` if the guest maps it to DRAM with the
    /// access allowed; false leaves the fault to the engine.
    fn fill(&mut self, address: u64, write: bool) -> bool {
        if address < UPPER {
            return false;
        }
        let page = address & !4095;
        let mmu::Outcome::Mapped(t) = self.translate(address, write) else {
            self.refused += 1;
            return false;
        };
        let physical = t.physical & !4095;
        let allowed = if write { t.writable } else { t.readable };
        if !allowed || !DramHost::in_ram(physical, 4096) || self.watched(page)
            || self.regime.tag_blocks(page, t.attributes)
        {
            self.refused += 1;
            return false;
        }
        let frame = self.frames[((physical - DRAM) / 4096) as usize];
        if unsafe { !self.map(page, frame, t.writable) } {
            return false;
        }
        self.fills += 1;
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

/// A JSON number at `path` in `bundle.json`.
fn number(value: &serde_json::Value, path: &[&str]) -> Option<u64> {
    path.iter().try_fold(value, |v, key| v.get(*key))?.as_u64()
}

/// Read `dir/bundle.json` and every region's bytes through LKL into fresh
/// guest DRAM. CPU 0 (LKL's).
fn prepare(dir: &str) -> Result<Prepared, String> {
    let read = |name: &str| {
        super::board::read_linux_file(&alloc::format!("{dir}/{name}"))
            .map_err(|errno| alloc::format!("{dir}/{name}: errno {}", -errno))
    };
    let json = read("bundle.json")?;
    let bundle: serde_json::Value =
        serde_json::from_slice(&json).map_err(|e| alloc::format!("bundle.json: {e}"))?;
    let entry = number(&bundle, &["entry", "physical_address"]).ok_or("bundle has no entry")?;
    let x0 = number(&bundle, &["entry_x0"]).ok_or("bundle has no entry x0")?;
    let read_only = (
        number(&bundle, &["read_only_region", "base"]).ok_or("no read-only region")?,
        number(&bundle, &["read_only_region", "bytes"]).ok_or("no read-only region")?,
    );
    let regions = bundle.get("regions").and_then(|r| r.as_array()).ok_or("bundle has no regions")?;

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

    let mut loaded = 0u64;
    for region in regions {
        let file = region.get("file").and_then(|f| f.as_str()).ok_or("region without file")?;
        let pa = number(region, &["guest_pa"]).ok_or("region without guest_pa")?;
        let bytes = number(region, &["bytes"]).ok_or("region without bytes")?;
        let offset = number(region, &["file_offset"]).ok_or("region without file_offset")?;
        let memory = number(region, &["memory_bytes"]).unwrap_or(bytes);
        if bytes == 0 {
            continue;
        }
        if pa < DRAM || pa + memory.max(bytes) > DRAM + RAM_BYTES {
            return Err(alloc::format!("region {file} at {pa:#x} is outside guest DRAM"));
        }
        pread_into(&alloc::format!("{dir}/{file}"), offset, pa, bytes)?;
        loaded += bytes;
    }
    crate::println!(
        "STELA_SPTM: {} regions, {} MiB loaded into guest DRAM at {:#x} ({} MiB backed)",
        regions.len(), loaded >> 20, DRAM, RAM_BYTES >> 20
    );
    Ok(Prepared {
        tree: read("devicetree.adt")?,
        hpm: read("USB-C_HPM,46-P01-AP.bin")?,
        frames,
        entry,
        x0,
        read_only,
    })
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
    match prepare(dir) {
        Ok(prepared) => unsafe { *(&raw mut PREPARED) = Some(prepared) },
        Err(error) => {
            crate::println!("STELA_SPTM: cannot prepare the bundle: {}", error);
            crate::println!("STELA_SPTM_FAIL");
            return false;
        }
    }
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
    let Some(id) = crate::sched::dedicate_cpu(3, "vcpu-sptm", on_core, 0) else {
        crate::println!("STELA_SPTM: dedicate_cpu(3) refused (needs NK_SMP)");
        return false;
    };
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
    let inputs = mount::Inputs {
        programmed,
        reset: Some(p.entry),
        now: 0,
        wall: None,
        rtc_anchor: None,
        backing: Some(Box::new(super::mount::SparseDisk::new(super::mount::SSD_BLOCKS))),
        sep_state: None,
        sep_observe: false,
        sep_recovery_policy_nonce: None,
        entropy: Box::new(super::mount::rdrand_fill),
        restore: false,
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
    let devices = match mount::devices_with_memory(&board, console, inputs, Some(MemoryResources::T8112Cold(plan)), &mut log) {
        Ok((devices, _)) => devices,
        Err(error) => {
            crate::println!("STELA_SPTM: mount refused: {:?}", error);
            return false;
        }
    };

    let chip = board.cpus.chip;
    let Some(host) = DramHost::new(p.frames.clone()) else {
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
    let stop = loop {
        // The budget is a total: the engine stops once its block count
        // reaches it, so each slice asks for SLICE_BLOCKS more.
        let stop = machine.run(machine.statistics.blocks + SLICE_BLOCKS);
        slices += 1;
        if crate::timer::time_ns() - reported > 10_000_000_000 {
            reported = crate::timer::time_ns();
            crate::println!(
                "STELA_SPTM: ... {} s: {} blocks, {} native runs, pc {:#x}, console bytes {}",
                (reported - start) / 1_000_000_000, machine.statistics.blocks, machine.host.executions,
                machine.cpu.pc, console_bytes.load(Ordering::Relaxed)
            );
        }
        let more = matches!(stop, Stop::Budget) && crate::timer::time_ns() - start < RUN_NS;
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
    if let Some(shadow) = machine.host.shadow.as_ref() {
        crate::println!(
            "STELA_SPTM: shadow: {} pages filled in the #PF path, {} refused, {} clears; {} native faults below the upper range",
            shadow.fills, shadow.refused, shadow.clears, machine.host.lower_faults
        );
    }
    // Slice 1 passes once SPTM's own code ran natively on the board and the
    // run ended at the MMU or a guest-visible stop, not a host refusal.
    blocks > 100 && (machine.host.mmu.is_some() || matches!(stop, Stop::Budget))
}

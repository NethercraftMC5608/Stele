//! Sparse guest physical RAM, owned independently of shadow mappings.
//! Pre-SMP only: frames are made RW/NX in nk's boot identity mapping.
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use stela_engine::memory::{
    RegisteredSystemRam, ReservedSystemRamRange, SystemRamRegistrationError,
};

const PAGE: u64 = 4096;
struct Frame(*mut u8);

// No byte-access API: registration alone owns all subsequent accesses. Integer
// addresses let this keep-alive satisfy the canonical owner's Send + Sync
// boundary without declaring the mutable sparse builder thread-safe. This
// private prototype must still be destroyed on the BSP before SMP: nk's frame
// allocator is not synchronized.
struct KeptFrames(Vec<usize>);
impl Drop for KeptFrames {
    fn drop(&mut self) {
        for &page in &self.0 {
            unsafe { crate::frames::free(page as *mut u8) };
        }
    }
}
impl Frame {
    fn new() -> Option<Self> {
        let page = crate::frames::alloc()?; // already zero-filled
        unsafe { crate::paging::boot_code_rights(page as u64, false) };
        Some(Self(page))
    }
}
impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { crate::frames::free(self.0) };
    }
}

pub(super) struct GuestRam {
    base: u64,
    bytes: u64,
    pages: BTreeMap<u64, Frame>,
}
impl stela_contracts::dma::Memory for GuestRam {
    fn read(&mut self, physical: u64, out: &mut [u8]) -> bool {
        GuestRam::read(self, physical, out)
    }
    fn write(&mut self, physical: u64, bytes: &[u8]) -> bool {
        GuestRam::write(self, physical, bytes)
    }
    fn check_range(
        &mut self,
        physical: u64,
        bytes: usize,
        _: bool,
    ) -> Result<(), stela_contracts::dma::DmaError> {
        self.contains(physical, bytes)
            .then_some(())
            .ok_or(stela_contracts::dma::DmaError::Memory)
    }
}
impl GuestRam {
    /// Consume the builder and transfer committed pages to the existing
    /// canonical memory authority. Uncommitted holes are NOT registered RAM;
    /// callers must commit every required page before this one-way handoff.
    ///
    /// # Safety
    /// Before SMP, at CPL0 with IF masked. No shadow root, raw pointer, direct
    /// native writer, Rust reference or DMA consumer may retain access to the
    /// payload. All later accesses must use the canonical system. The returned
    /// registration and every borrowed capability must be destroyed before APs
    /// start, because frame reclamation still uses nk's boot-only allocator.
    pub(super) unsafe fn into_registered(
        self,
    ) -> Result<RegisteredSystemRam, SystemRamRegistrationError> {
        let mut ranges = Vec::with_capacity(self.pages.len());
        let mut kept = KeptFrames(Vec::with_capacity(self.pages.len()));
        for (source_pa, frame) in self.pages {
            ranges.push(ReservedSystemRamRange {
                source_pa,
                host_pa: frame.0 as u64,
                host_base: core::ptr::NonNull::new(frame.0).expect("owned RAM frame"),
                bytes: PAGE as usize,
            });
            kept.0.push(frame.0 as usize);
            core::mem::forget(frame); // KeptFrames now reclaims this frame
        }
        // SAFETY: consumed builder has no remaining byte API or shadow aliases;
        // the caller closes all external accesses. These are pinned, initialized
        // identity-mapped WB frames. Arc keeps them through the last backing,
        // including registration failure; no second RAM store or copy is made.
        unsafe { RegisteredSystemRam::register(&ranges, Arc::new(kept), 64, 8) }
    }

    pub(super) fn reserve(base: u64, bytes: u64) -> Option<Self> {
        let end = base.checked_add(bytes)?;
        if bytes == 0 || (base | bytes) & (PAGE - 1) != 0 || end > 1 << 48 {
            return None;
        }
        Some(Self {
            base,
            bytes,
            pages: BTreeMap::new(),
        })
    }
    pub(super) fn contains(&self, pa: u64, len: usize) -> bool {
        pa.checked_sub(self.base)
            .and_then(|off| off.checked_add(len as u64))
            .is_some_and(|end| end <= self.bytes)
    }
    /// One resident page's borrowed host span. Cross-page raw pointers are
    /// never returned: adjacent guest pages need not be adjacent host frames.
    pub(super) fn host(&self, pa: u64, len: usize) -> Option<*mut u8> {
        if !self.contains(pa, len) {
            return None;
        }
        let offset = pa & (PAGE - 1);
        if offset + len as u64 > PAGE {
            return None;
        }
        let frame = self.pages.get(&(pa & !(PAGE - 1)))?;
        Some(unsafe { frame.0.add(offset as usize) })
    }
    pub(super) fn ensure_page(&mut self, page: u64) -> Option<*mut u8> {
        if page & (PAGE - 1) != 0 || !self.contains(page, PAGE as usize) {
            return None;
        }
        if !self.pages.contains_key(&page) {
            self.pages.insert(page, Frame::new()?);
        }
        self.host(page, PAGE as usize)
    }
    pub(super) fn resident_pages(&self) -> usize {
        self.pages.len()
    }
    pub(super) fn read(&self, pa: u64, out: &mut [u8]) -> bool {
        if !self.contains(pa, out.len()) {
            return false;
        }
        let mut done = 0;
        while done < out.len() {
            let address = pa + done as u64;
            let take = (PAGE as usize - (address as usize & 4095)).min(out.len() - done);
            if let Some(ptr) = self.host(address, take) {
                unsafe { core::ptr::copy_nonoverlapping(ptr, out[done..].as_mut_ptr(), take) };
            } else {
                out[done..done + take].fill(0);
            }
            done += take;
        }
        true
    }
    pub(super) fn write(&mut self, pa: u64, bytes: &[u8]) -> bool {
        if !self.contains(pa, bytes.len()) {
            return false;
        }
        if bytes.is_empty() {
            return true;
        }
        let first = pa & !(PAGE - 1);
        let last = (pa + bytes.len() as u64 - 1) & !(PAGE - 1);
        // Commit every required frame before copying any bytes. Allocation
        // failure may leave zero pages resident but never partial data writes.
        for page in (first..=last).step_by(PAGE as usize) {
            if self.ensure_page(page).is_none() {
                return false;
            }
        }
        let mut done = 0;
        while done < bytes.len() {
            let address = pa + done as u64;
            let take = (PAGE as usize - (address as usize & 4095)).min(bytes.len() - done);
            let ptr = self.host(address, take).expect("committed guest RAM span");
            unsafe { core::ptr::copy_nonoverlapping(bytes[done..].as_ptr(), ptr, take) };
            done += take;
        }
        true
    }
}

/// Actual CPL0 registration, not the engine's hosted fixture constructor.
/// Runs while the dedicated probe stack and BSP-only boot contract are live.
pub(super) fn canonical_probe() {
    const BASE: u64 = 0x8000_0000;
    const BYTES: u64 = 1 << 32;
    const HIGH: u64 = BASE + BYTES - PAGE;
    let mut ram = GuestRam::reserve(BASE, BYTES).unwrap();
    let crossing = BASE + PAGE - 9;
    let initial: [u8; 40] = core::array::from_fn(|i| i as u8 ^ 0xa5);
    assert!(ram.write(crossing, &initial));
    assert!(ram.write(HIGH, &0x1234_5678u64.to_le_bytes()));
    assert_eq!(ram.resident_pages(), 3);
    // SAFETY: no raw host pointer/shadow mapping escaped this builder. IF is
    // masked, APs do not exist, and the registration is dropped below. Only
    // canonical APIs touch these payloads from this point onward.
    let canonical = unsafe { ram.into_registered() }.expect("native canonical RAM registration");
    assert_eq!(canonical.placements().count(), 3);
    assert!(canonical
        .placements()
        .all(|(guest, host, bytes)| { guest != host && host < 1 << 32 && bytes == PAGE as usize }));
    assert_eq!(canonical.reservation_granule(), 64);
    let mut copy = [0; 40];
    // SAFETY: all source/destination arrays here are private stack buffers,
    // disjoint from registered RAM, and its sole writer is the canonical API.
    unsafe { canonical.read_physical(crossing, &mut copy) }.unwrap();
    assert_eq!(copy, initial, "registration must preserve historical bytes");
    unsafe { canonical.publish_physical(crossing, &[0x79; 40]) }.unwrap();
    unsafe { canonical.read_physical(crossing, &mut copy) }.unwrap();
    assert_eq!(copy, [0x79; 40]);
    assert_eq!(canonical.read_descriptor(HIGH).unwrap(), 0x1234_5678);
    assert!(!canonical.update_descriptor(HIGH, 0, 7).unwrap());
    assert_eq!(canonical.read_descriptor(HIGH).unwrap(), 0x1234_5678);
    assert!(canonical.update_descriptor(HIGH, 0x1234_5678, 7).unwrap());
    assert_eq!(canonical.read_descriptor(HIGH).unwrap(), 7);
    assert!(canonical.update_descriptor(HIGH + 1, 7, 8).is_err());
    assert!(canonical.check_dma_range(crossing, 40, true).is_ok());
    // Uncommitted builder holes are absent from canonical identity, rather
    // than magically registered zero storage. Whole-range preflight refuses
    // without changing the output or the last resident bytes.
    let edge = BASE + 2 * PAGE - 4;
    let mut tail_before = [0; 4];
    unsafe { canonical.read_physical(edge, &mut tail_before) }.unwrap();
    let refused = unsafe { canonical.publish_physical(edge, &[0xee; 8]) }
        .expect_err("canonical writes may not enter unregistered holes");
    assert_eq!(refused.completed, 0);
    let mut tail_after = [0; 4];
    unsafe { canonical.read_physical(edge, &mut tail_after) }.unwrap();
    assert_eq!(tail_after, tail_before);
    let mut guard = [0x5a; 8];
    assert!(unsafe { canonical.read_physical(edge, &mut guard) }.is_err());
    assert_eq!(guard, [0x5a; 8]);
    assert!(canonical.check_dma_range(edge, 8, true).is_err());
    assert!(canonical.check_dma_range(u64::MAX - 3, 8, false).is_err());
    crate::println!("STELA_RAM_CANONICAL_OK: real CPL0 registration, preserved bytes, scatter publication and descriptor CAS");

    {
        let service = canonical
            .try_service()
            .unwrap()
            .expect("idle canonical owner");
        assert!(service.belongs_to(&canonical));
        assert_eq!(service.generation(), canonical.generation().unwrap());
        service.validate_current().unwrap();
        assert!(canonical.try_service().unwrap().is_none());
    }
    let service = canonical.try_service().unwrap().expect("service released");
    service.fail();
    drop(service);
    assert!(canonical.try_service().is_err());
    assert!(unsafe { canonical.publish_physical(HIGH, &[0; 8]) }.is_err());
    assert!(unsafe { canonical.read_physical(HIGH, &mut guard) }.is_err());
    assert_eq!(guard, [0x5a; 8]);
    let before_drop = crate::frames::stats().0;
    drop(canonical);
    assert_eq!(crate::frames::stats().0 + 3, before_drop);
    crate::println!("STELA_RAM_AUTHORITY_OK: exclusive service, permanent failure refusal and exact frame reclamation");
}

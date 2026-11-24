// SPDX-License-Identifier: GPL-2.0-or-later
// Host tests for the exact parsers compiled into nk, with bounded fake memory.
extern crate alloc;
#[path = "../core/src/acpi.rs"]
mod acpi;
#[path = "../core/src/bootinfo_x86.rs"]
mod bootinfo;
#[path = "../core/src/device_irq_x86.rs"]
mod device_irq_x86;
#[path = "../core/src/irq_x86.rs"]
mod irq_x86;
#[path = "../core/src/pci.rs"]
mod pci;
#[path = "../core/src/pci_irq.rs"]
mod pci_irq;
use std::collections::BTreeMap;

struct Memory(BTreeMap<u64, Vec<u8>>);
impl acpi::Memory for Memory {
    fn read(&self, address: u64, length: usize) -> Option<&[u8]> {
        self.0.get(&address)?.get(..length)
    }
}
fn seal(mut b: Vec<u8>) -> Vec<u8> {
    let len = b.len() as u32;
    b[4..8].copy_from_slice(&len.to_le_bytes());
    b[9] = 0;
    b[9] = 0u8.wrapping_sub(b.iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    b
}
fn header(sig: &[u8; 4], len: usize) -> Vec<u8> {
    let mut b = vec![0; len];
    b[..4].copy_from_slice(sig);
    b
}
fn rsdp(rsdt: u32, xsdt: u64) -> Vec<u8> {
    let mut b = vec![0; 36];
    b[..8].copy_from_slice(b"RSD PTR ");
    b[15] = 2;
    b[16..20].copy_from_slice(&rsdt.to_le_bytes());
    b[20..24].copy_from_slice(&36u32.to_le_bytes());
    b[24..32].copy_from_slice(&xsdt.to_le_bytes());
    b[8] = 0u8.wrapping_sub(b[..20].iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    b[32] = 0u8.wrapping_sub(b.iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    b
}
fn madt() -> Vec<u8> {
    let mut b = header(b"APIC", 44);
    b[36..40].copy_from_slice(&0xfee00000u32.to_le_bytes());
    for (uid, id, flags) in [(0, 0, 1u32), (1, 2, 1), (2, 7, 0), (3, 9, 2)] {
        b.extend_from_slice(&[0, 8, uid, id]);
        b.extend_from_slice(&flags.to_le_bytes());
    }
    b.extend_from_slice(&[1, 12, 2, 0]);
    b.extend_from_slice(&0xfec00000u32.to_le_bytes());
    b.extend_from_slice(&24u32.to_le_bytes());
    b.extend_from_slice(&[2, 10, 0, 9]);
    b.extend_from_slice(&33u32.to_le_bytes());
    b.extend_from_slice(&13u16.to_le_bytes());
    b.extend_from_slice(&[5, 12, 0, 0]);
    b.extend_from_slice(&0xfee01000u64.to_le_bytes());
    b.extend_from_slice(&[9, 16, 0, 0]);
    b.extend_from_slice(&0x102u32.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&8u32.to_le_bytes());
    seal(b)
}
fn mcfg(entries: &[(u64, u16, u8, u8)]) -> Vec<u8> {
    let mut b = header(b"MCFG", 44);
    for &(base, seg, lo, hi) in entries {
        b.extend_from_slice(&base.to_le_bytes());
        b.extend_from_slice(&seg.to_le_bytes());
        b.extend_from_slice(&[lo, hi, 0, 0, 0, 0]);
    }
    seal(b)
}
fn fixture() -> Memory {
    let mut xsdt = header(b"XSDT", 36);
    for addr in [0u64, 0x3000, 0x4000, 0x5000] {
        xsdt.extend_from_slice(&addr.to_le_bytes());
    }
    let mut rsdt = header(b"RSDT", 36);
    rsdt.extend_from_slice(&0x3000u32.to_le_bytes());
    let mut fadt = header(b"FACP", 196);
    fadt[112..116].copy_from_slice(&(1u32 << 10).to_le_bytes());
    fadt[116..120].copy_from_slice(&[1, 8, 0, 1]);
    fadt[120..128].copy_from_slice(&0xcf9u64.to_le_bytes());
    fadt[128] = 6;
    fadt[64..68].copy_from_slice(&0x604u32.to_le_bytes());
    fadt[89] = 2;
    fadt[172..176].copy_from_slice(&[1, 16, 0, 2]);
    fadt[176..184].copy_from_slice(&0x704u64.to_le_bytes());
    Memory(BTreeMap::from([
        (0x1000, rsdp(0x6000, 0x2000)),
        (0x2000, seal(xsdt)),
        (0x3000, madt()),
        (0x4000, mcfg(&[(0x90000000, 2, 32, 47)])),
        (0x5000, seal(fadt)),
        (0x6000, seal(rsdt)),
    ]))
}
#[test]
fn xsdt_madt_mcfg_fadt() {
    let p = acpi::discover(&fixture(), 0x1000).unwrap();
    assert!(p.xsdt);
    assert_eq!(p.lapic, 0xfee01000);
    assert_eq!(
        p.cpus.iter().map(|c| c.apic_id).collect::<Vec<_>>(),
        [0, 2, 0x102]
    );
    assert_eq!(p.ioapics[0].gsi_base, 24);
    assert_eq!(p.overrides[0].gsi, 33);
    let f = p.fadt.unwrap();
    assert_eq!(f.reset.unwrap().0.address, 0xcf9);
    assert_eq!(f.pm1a_control.unwrap().address, 0x704);
    assert_eq!(p.ecam[0].address(32, 3, 2, 0xabc), Some(0x9201aabc));
    assert_eq!(p.ecam[0].range(), Some((0x92000000, 0x93000000)));
    assert_eq!(p.ecam[0].address(31, 0, 0, 0), None);
    assert_eq!(p.ecam[0].address(32, 32, 0, 0), None);
}
#[test]
fn damaged_xsdt_falls_back_to_rsdt() {
    let mut m = fixture();
    m.0.get_mut(&0x2000).unwrap()[9] ^= 1;
    let p = acpi::discover(&m, 0x1000).unwrap();
    assert!(!p.xsdt);
    assert_eq!(p.cpus.len(), 3);
}
#[test]
fn acpi1_rsdt() {
    let mut m = fixture();
    let mut b = rsdp(0x6000, 0);
    b[15] = 0;
    b.truncate(20);
    b[8] = 0;
    b[8] = 0u8.wrapping_sub(b.iter().fold(0u8, |s, b| s.wrapping_add(*b)));
    m.0.insert(0x1000, b);
    assert!(!acpi::discover(&m, 0x1000).unwrap().xsdt);
}
#[test]
fn reject_bad_rsdp_extended_checksum_and_length() {
    let mut m = fixture();
    m.0.get_mut(&0x1000).unwrap()[32] ^= 1;
    assert_eq!(acpi::discover(&m, 0x1000).unwrap_err(), acpi::Error::Rsdp);
    let mut m = fixture();
    m.0.get_mut(&0x1000).unwrap()[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(acpi::discover(&m, 0x1000).unwrap_err(), acpi::Error::Rsdp);
}
#[test]
fn reject_truncated_table_and_malformed_madt_records() {
    let mut m = fixture();
    m.0.get_mut(&0x3000).unwrap().truncate(40);
    assert_eq!(
        acpi::discover(&m, 0x1000).unwrap_err(),
        acpi::Error::Unmapped
    );
    for len in [0, 1, 255] {
        let mut m = fixture();
        let mut b = madt();
        b[45] = len;
        m.0.insert(0x3000, seal(b));
        assert_eq!(acpi::discover(&m, 0x1000).unwrap_err(), acpi::Error::Madt);
    }
}
#[test]
fn reject_bad_table_checksum() {
    let mut m = fixture();
    m.0.get_mut(&0x3000).unwrap()[9] ^= 1;
    assert_eq!(acpi::discover(&m, 0x1000).unwrap_err(), acpi::Error::Table);
}
#[test]
fn reject_duplicate_cpu_ids() {
    let mut m = fixture();
    let mut b = madt();
    b[55] = 0;
    m.0.insert(0x3000, seal(b));
    assert_eq!(acpi::discover(&m, 0x1000).unwrap_err(), acpi::Error::Madt);
}
#[test]
fn reject_bad_mcfg_windows() {
    for entries in [
        vec![(0x90000100, 0, 0, 255)],
        vec![(0x90000000, 0, 10, 9)],
        vec![(0x90000000, 0, 0, 4), (0xa0000000, 0, 4, 8)],
        vec![(0xfffffffffff00000, 0, 0, 255)],
    ] {
        let mut m = fixture();
        m.0.insert(0x4000, mcfg(&entries));
        assert_eq!(acpi::discover(&m, 0x1000).unwrap_err(), acpi::Error::Mcfg);
    }
}

type Key = (u16, u8, u8, u8, u16);
struct Config(BTreeMap<Key, u32>);
impl pci::Config for Config {
    fn read32(&self, b: pci::Bdf, o: u16) -> Option<u32> {
        Some(
            *self
                .0
                .get(&(b.segment, b.bus, b.device, b.function, o))
                .unwrap_or(&u32::MAX),
        )
    }
}
impl Config {
    fn add(&mut self, b: pci::Bdf, header: u8) {
        for o in (0..256).step_by(4) {
            self.set(b, o, 0);
        }
        self.set(b, 0, 0x12348086);
        self.set(b, 12, (header as u32) << 16);
    }
    fn set(&mut self, b: pci::Bdf, o: u16, v: u32) {
        self.0
            .insert((b.segment, b.bus, b.device, b.function, o), v);
    }
}
#[test]
fn pci_multifunction_segments_bridges_and_bars() {
    let mut c = Config(BTreeMap::new());
    let b = pci::Bdf {
        segment: 2,
        bus: 32,
        device: 3,
        function: 0,
    };
    c.add(b, 0x80);
    c.set(b, 8, 0x01080201);
    c.set(b, 16, 0x34500004);
    c.set(b, 20, 0x12);
    c.set(b, 24, 0xc001);
    let b1 = pci::Bdf { function: 1, ..b };
    c.add(b1, 0);
    let bridge = pci::Bdf {
        segment: 4,
        bus: 2,
        device: 0,
        function: 0,
    };
    c.add(bridge, 1);
    c.set(bridge, 24, 0x12345678);
    let v = pci::enumerate(
        &c,
        &[
            acpi::Ecam {
                base: 0x90000000,
                segment: 2,
                start_bus: 32,
                end_bus: 32,
            },
            acpi::Ecam {
                base: 0xa0000000,
                segment: 4,
                start_bus: 2,
                end_bus: 2,
            },
        ],
    );
    assert_eq!(v.len(), 3);
    assert_eq!(v[0].bars[0], pci::Bar::Memory64(0x1234500000));
    assert_eq!(v[0].bars[1], pci::Bar::Continuation);
    assert_eq!(v[0].bars[2], pci::Bar::Io(0xc000));
    assert_eq!(v[0].class, 1);
    assert_eq!(v[0].subclass, 8);
    assert_eq!(v[0].interface, 2);
    assert_eq!(v[2].bars[2], pci::Bar::None);
}
#[test]
fn pci_skips_unadvertised_functions_and_scans_legacy_buses() {
    let mut c = Config(BTreeMap::new());
    let b = pci::Bdf {
        segment: 0,
        bus: 245,
        device: 4,
        function: 0,
    };
    c.add(b, 0);
    c.add(pci::Bdf { function: 1, ..b }, 0);
    let v = pci::enumerate(&c, &[]);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].bdf, b);
}

fn mb_tag(b: &mut Vec<u8>, kind: u32, payload: &[u8]) {
    b.extend_from_slice(&kind.to_le_bytes());
    b.extend_from_slice(&((8 + payload.len()) as u32).to_le_bytes());
    b.extend_from_slice(payload);
    while b.len() % 8 != 0 {
        b.push(0);
    }
}
fn multiboot() -> Vec<u8> {
    let mut b = vec![0; 8];
    let mut mmap = Vec::new();
    mmap.extend_from_slice(&24u32.to_le_bytes());
    mmap.extend_from_slice(&0u32.to_le_bytes());
    for (lo, len, kind) in [
        (0x100000000u64, 0x40000000u64, 1u32),
        (0x100000, 0x7ff00000, 1),
        (0x80000000, 0x100000, 2),
    ] {
        mmap.extend_from_slice(&lo.to_le_bytes());
        mmap.extend_from_slice(&len.to_le_bytes());
        mmap.extend_from_slice(&kind.to_le_bytes());
        mmap.extend_from_slice(&0u32.to_le_bytes());
    }
    mb_tag(&mut b, 6, &mmap);
    mb_tag(&mut b, 15, &rsdp(0x6000, 0x2000));
    mb_tag(&mut b, 14, &rsdp(0x6000, 0)[..20]);
    let mut module = Vec::new();
    module.extend_from_slice(&0x70000000u32.to_le_bytes());
    module.extend_from_slice(&0x70001000u32.to_le_bytes());
    mb_tag(&mut b, 3, &module);
    mb_tag(&mut b, 0, &[]);
    let len = b.len() as u32;
    b[..4].copy_from_slice(&len.to_le_bytes());
    b
}
#[test]
fn multiboot_memory_regions_module_and_new_acpi_preference() {
    let b = multiboot();
    let info = bootinfo::parse_multiboot_bytes(&b, 0x10000).unwrap();
    assert_eq!(info.ram_region_count, 2);
    assert_eq!(info.ram_base, 0x100000);
    assert_eq!(info.ram_regions[1].0, 0x100000000);
    assert_eq!(info.rsdp, Some(0x10068));
    assert_eq!(info.initrd, Some((0x70000000, 0x70001000)));
    assert_eq!(info.info_range, Some((0x10000, 0x10000 + b.len() as u64)));
}
#[test]
fn multiboot_rejects_truncation_bad_stride_and_missing_end() {
    let b = multiboot();
    for len in 0..b.len() {
        assert!(bootinfo::parse_multiboot_bytes(&b[..len], 0).is_none());
    }
    let mut b = multiboot();
    b[16..20].copy_from_slice(&8u32.to_le_bytes());
    assert!(bootinfo::parse_multiboot_bytes(&b, 0).is_none());
    let mut b = multiboot();
    let len = b.len();
    b[len - 8..len - 4].copy_from_slice(&99u32.to_le_bytes());
    assert!(bootinfo::parse_multiboot_bytes(&b, 0).is_none());
}

#[test]
fn isa_routing_and_redirection_encoding() {
    use acpi::Override;
    use irq_x86::{Error, Route};
    assert_eq!(
        irq_x86::isa_route(0, &[]),
        Ok(Route {
            gsi: 0,
            low: false,
            level: false
        })
    );
    let r = Override {
        bus: 0,
        irq: 0,
        gsi: 2,
        flags: 0,
    };
    let pit = irq_x86::isa_route(0, &[r]).unwrap();
    assert_eq!(pit.redirection(0x40, 6, false), Ok((6u64 << 56) | 0x40));
    let low = irq_x86::isa_route(0, &[Override { flags: 15, ..r }]).unwrap();
    assert_eq!(low.redirection(0x50, 2, true), Ok((2u64 << 56) | 0x1a050));
    for flags in [2, 8, 16] {
        assert_eq!(
            irq_x86::isa_route(0, &[Override { flags, ..r }]),
            Err(Error::Flags)
        );
    }
    assert_eq!(irq_x86::isa_route(0, &[r, r]), Err(Error::Duplicate));
    assert_eq!(pit.redirection(0x40, 256, false), Err(Error::Destination));
    for vector in [0x20, 0x41, 0xef, 0xf0, 0xff] {
        assert_eq!(pit.redirection(vector, 0, false), Err(Error::Vector));
    }
    assert_eq!(irq_x86::pin(25, 24, 24), Some(1));
    assert_eq!(irq_x86::pin(23, 24, 24), None);
    assert_eq!(irq_x86::pin(48, 24, 24), None);
}

#[test]
fn msi_message_destinations_and_reserved_vectors() {
    assert_eq!(
        irq_x86::message(6, 0x51).unwrap(),
        irq_x86::Message {
            address: 0xfee06000,
            data: 0x51
        }
    );
    assert!(irq_x86::message(256, 0x50).is_err());
    for vector in [0, 0x40, 0xef, 0xf0, 0xff] {
        assert!(irq_x86::message(0, vector).is_err());
    }
}

use std::cell::{Cell, RefCell};
struct Writable {
    bytes: RefCell<[u8; 256]>,
    writes: RefCell<Vec<(u16, u32, u8)>>,
    fail: Cell<Option<u16>>,
}
const TEST_BDF: pci::Bdf = pci::Bdf {
    segment: 0,
    bus: 1,
    device: 2,
    function: 0,
};
impl Writable {
    fn new(control: u16) -> Self {
        let mut bytes = [0u8; 256];
        bytes[6] = 0x10;
        bytes[0x34] = 0x40;
        bytes[0x40] = 5;
        bytes[0x42..0x44].copy_from_slice(&control.to_le_bytes());
        bytes[4] = 1;
        Self {
            bytes: RefCell::new(bytes),
            writes: RefCell::new(Vec::new()),
            fail: Cell::new(None),
        }
    }
    fn set32(&self, at: usize, v: u32) {
        self.bytes.borrow_mut()[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn write(&self, at: u16, v: u32, n: u8) -> bool {
        self.writes.borrow_mut().push((at, v, n));
        if self.fail.get() == Some(at) {
            self.fail.set(None);
            return false;
        }
        let mut bytes = self.bytes.borrow_mut();
        let at = at as usize;
        let Some(dest) = bytes.get_mut(at..at + n as usize) else {
            return false;
        };
        dest.copy_from_slice(&v.to_le_bytes()[..n as usize]);
        true
    }
}
impl pci::Config for Writable {
    fn read32(&self, _: pci::Bdf, at: u16) -> Option<u32> {
        Some(u32::from_le_bytes(
            self.bytes
                .borrow()
                .get(at as usize..at as usize + 4)?
                .try_into()
                .ok()?,
        ))
    }
}
impl pci::ConfigWrite for Writable {
    fn write16(&self, _: pci::Bdf, at: u16, v: u16) -> bool {
        self.write(at, v as u32, 2)
    }
    fn write32(&self, _: pci::Bdf, at: u16, v: u32) -> bool {
        self.write(at, v, 4)
    }
}
#[test]
fn pci_capability_bounds_cycles_and_msix_metadata() {
    let c = Writable::new(0x180);
    let m = pci_irq::msi(&c, TEST_BDF).unwrap().unwrap();
    assert!(m.address64 && m.maskable);
    assert_eq!(m.data(), 0x4c);
    assert_eq!(m.mask(), Some(0x50));
    c.set32(0x40, 0x00006005);
    c.set32(0x60, 0x00070011);
    c.set32(0x64, 0x2002);
    c.set32(0x68, 0x803);
    let m = pci_irq::msix(&c, TEST_BDF).unwrap().unwrap();
    assert_eq!(m.count, 8);
    assert_eq!(
        (m.table_bar, m.table_offset, m.pending_bar, m.pending_offset),
        (2, 0x2000, 3, 0x800)
    );
    c.set32(0x64, 0x2006);
    assert_eq!(pci_irq::msix(&c, TEST_BDF), Err(pci_irq::Error::Bar));
    c.set32(0x60, 0x00004011);
    assert_eq!(pci_irq::msi(&c, TEST_BDF), Err(pci_irq::Error::Cycle));
    for pointer in [0x20, 0x41, 0xff] {
        c.bytes.borrow_mut()[0x34] = pointer;
        assert_eq!(pci_irq::msi(&c, TEST_BDF), Err(pci_irq::Error::Pointer));
    }
    c.bytes.borrow_mut()[0x34] = 0xfc;
    c.set32(0xfc, 0x00800005);
    assert_eq!(pci_irq::msi(&c, TEST_BDF), Err(pci_irq::Error::Truncated));
}
#[test]
fn msi_programming_native_width_no_dma_and_exact_restore() {
    use pci::Config;
    for control in [0u16, 0x80, 0x100, 0x180] {
        let c = Writable::new(control);
        let initial = *c.bytes.borrow();
        let m = pci_irq::msi(&c, TEST_BDF).unwrap().unwrap();
        let saved =
            pci_irq::enable_msi(&c, TEST_BDF, m, irq_x86::message(4, 0x55).unwrap()).unwrap();
        assert_eq!(c.read16(TEST_BDF, 4), Some(0x403)); // Memory+INTx-disable, no bus-master enable.
        assert_eq!(c.read16(TEST_BDF, 6), Some(0x10)); // Status register untouched.
        assert_eq!(c.read32(TEST_BDF, 0x44), Some(0xfee04000));
        assert_eq!(c.read16(TEST_BDF, m.data()), Some(0x55));
        assert_eq!(c.read16(TEST_BDF, 0x42), Some(control | 1));
        assert!(c
            .writes
            .borrow()
            .iter()
            .all(|&(at, _, width)| at != 4 || width == 2));
        assert!(saved.restore(&c, TEST_BDF));
        assert_eq!(*c.bytes.borrow(), initial);
    }
}
#[test]
fn msi_failure_rolls_back_without_enabling_and_rejects_active_device() {
    let c = Writable::new(0x80);
    let initial = *c.bytes.borrow();
    let m = pci_irq::msi(&c, TEST_BDF).unwrap().unwrap();
    c.fail.set(Some(0x44));
    assert_eq!(
        pci_irq::enable_msi(&c, TEST_BDF, m, irq_x86::message(0, 0x50).unwrap()).unwrap_err(),
        pci_irq::Error::Access
    );
    assert_eq!(*c.bytes.borrow(), initial);
    let c = Writable::new(1);
    let m = pci_irq::msi(&c, TEST_BDF).unwrap().unwrap();
    assert_eq!(
        pci_irq::enable_msi(&c, TEST_BDF, m, irq_x86::message(0, 0x50).unwrap()).unwrap_err(),
        pci_irq::Error::Enabled
    );
    assert!(c.writes.borrow().is_empty());
    let c = Writable::new(0);
    let m = pci_irq::msi(&c, TEST_BDF).unwrap().unwrap();
    assert_eq!(
        pci_irq::enable_msi(
            &c,
            TEST_BDF,
            m,
            irq_x86::Message {
                address: 1 << 32,
                data: 0x50
            }
        )
        .unwrap_err(),
        pci_irq::Error::Truncated
    );
    assert!(c.writes.borrow().is_empty());
}

#[test]
fn device_vectors_dispatch_retirement_and_exhaustion() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    fn callback() {
        CALLS.fetch_add(1, Ordering::SeqCst);
    }
    assert!(device_irq_x86::allocate(Some(128), None).is_none());
    assert!(device_irq_x86::allocate(Some(-1), None).is_none());
    let vector = device_irq_x86::allocate(None, Some(callback)).unwrap();
    assert_eq!(vector, irq_x86::DEVICE_FIRST);
    device_irq_x86::dispatch(vector);
    device_irq_x86::dispatch(vector);
    assert_eq!(CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(device_irq_x86::count(vector), 2);
    device_irq_x86::retire(vector);
    device_irq_x86::dispatch(vector);
    assert_eq!(CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(device_irq_x86::count(vector), 3);
    for expected in irq_x86::DEVICE_FIRST + 1..=irq_x86::DEVICE_LAST {
        assert_eq!(device_irq_x86::allocate(None, None), Some(expected));
    }
    assert!(device_irq_x86::allocate(None, None).is_none());
}

struct FakeTable {
    words: RefCell<Vec<u32>>,
    fail: Cell<Option<u32>>,
}
impl pci_irq::MsixTable for FakeTable {
    fn len(&self) -> u64 {
        self.words.borrow().len() as u64 * 4
    }
    fn read32(&self, offset: u32) -> Option<u32> {
        self.words.borrow().get(offset as usize / 4).copied()
    }
    fn write32(&self, offset: u32, value: u32) -> bool {
        if self.fail.get() == Some(offset) {
            self.fail.set(None);
            return false;
        }
        let mut words = self.words.borrow_mut();
        let Some(word) = words.get_mut(offset as usize / 4) else {
            return false;
        };
        *word = value;
        true
    }
}
fn msix_fixture() -> (Writable, pci_irq::Msix, FakeTable) {
    let c = Writable::new(0);
    c.bytes.borrow_mut()[4] = 3;
    c.set32(0x40, 0x00020011);
    c.set32(0x44, 3);
    c.set32(0x48, 0x803);
    let m = pci_irq::msix(&c, TEST_BDF).unwrap().unwrap();
    let table = FakeTable {
        words: RefCell::new(vec![
            0x11, 0x22, 0x33, 1, 0x44, 0x55, 0x66, 1, 0x77, 0x88, 0x99, 1,
        ]),
        fail: Cell::new(None),
    };
    (c, m, table)
}
#[test]
fn msix_bounds_programming_masks_and_restore() {
    use pci::Config;
    let (c, m, table) = msix_fixture();
    let initial = *c.bytes.borrow();
    let words = table.words.borrow().clone();
    assert_eq!(m.table_range(0x100000, 48), Some((0x100000, 0x100030)));
    assert_eq!(m.table_range(0x100000, 47), None);
    assert_eq!(m.table_range(u64::MAX - 8, 48), None);
    let saved = pci_irq::enable_msix(
        &c,
        TEST_BDF,
        m,
        &table,
        1,
        irq_x86::message(5, 0x54).unwrap(),
    )
    .unwrap();
    assert_eq!(&table.words.borrow()[4..8], &[0xfee05000, 0, 0x54, 0]);
    assert_eq!(&table.words.borrow()[..4], &words[..4]);
    assert_eq!(&table.words.borrow()[8..], &words[8..]);
    assert_eq!(c.read16(TEST_BDF, 0x42), Some(0x8002));
    assert_eq!(c.read16(TEST_BDF, 4), Some(0x403));
    assert!(saved.restore(&c, TEST_BDF, &table));
    assert_eq!(*c.bytes.borrow(), initial);
    assert_eq!(*table.words.borrow(), words);
}
#[test]
fn msix_failure_rolls_back_and_invalid_indices_do_not_write() {
    let (c, m, table) = msix_fixture();
    let initial = *c.bytes.borrow();
    let words = table.words.borrow().clone();
    table.fail.set(Some(20));
    assert_eq!(
        pci_irq::enable_msix(
            &c,
            TEST_BDF,
            m,
            &table,
            1,
            irq_x86::message(0, 0x50).unwrap()
        )
        .unwrap_err(),
        pci_irq::Error::Access
    );
    assert_eq!(*c.bytes.borrow(), initial);
    assert_eq!(*table.words.borrow(), words);
    c.writes.borrow_mut().clear();
    assert_eq!(
        pci_irq::enable_msix(
            &c,
            TEST_BDF,
            m,
            &table,
            3,
            irq_x86::message(0, 0x50).unwrap()
        )
        .unwrap_err(),
        pci_irq::Error::Truncated
    );
    assert!(c.writes.borrow().is_empty());
}
#[test]
fn conflicting_msi_msix_modes_are_rejected_without_writes() {
    let c = Writable::new(0);
    c.set32(0x40, 0x00006005);
    c.set32(0x60, 0x80000011);
    c.set32(0x64, 3);
    c.set32(0x68, 0x803);
    let m = pci_irq::msi(&c, TEST_BDF).unwrap().unwrap();
    assert_eq!(
        pci_irq::enable_msi(&c, TEST_BDF, m, irq_x86::message(0, 0x50).unwrap()).unwrap_err(),
        pci_irq::Error::Enabled
    );
    assert!(c.writes.borrow().is_empty());
    c.set32(0x40, 0x00016005);
    c.set32(0x60, 0x11);
    let m = pci_irq::msix(&c, TEST_BDF).unwrap().unwrap();
    let table = FakeTable {
        words: RefCell::new(vec![0; 4]),
        fail: Cell::new(None),
    };
    assert_eq!(
        pci_irq::enable_msix(
            &c,
            TEST_BDF,
            m,
            &table,
            0,
            irq_x86::message(0, 0x50).unwrap()
        )
        .unwrap_err(),
        pci_irq::Error::Enabled
    );
    assert!(c.writes.borrow().is_empty());
}

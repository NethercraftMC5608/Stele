//! A bounded ELF64 loader description, independent of the MMU.
//! Reject unsupported layouts before allocating or modifying any page tables.
extern crate alloc;
use alloc::vec::Vec;

#[derive(Debug)]
pub struct Segment {
    pub address: u64,
    pub offset: usize,
    pub filesz: usize,
    pub memsz: usize,
    pub executable: bool,
    pub writable: bool,
}
#[derive(Debug, Clone, Copy)]
pub struct Rela {
    pub address: u64,
    pub addend: u64,
}
#[derive(Debug, Clone, Copy)]
pub struct TlsTemplate {
    pub address: u64,
    pub filesz: usize,
    pub memsz: usize,
    pub offset: usize,
    pub align: u64,
}
/// Which system-call interface a program was written against.
///
/// NETHOS runs two, deliberately. The Linux one is the whole reason the
/// machine is useful today: a Debian archive's worth of binaries expect it,
/// and nk answers them by forwarding to the Linux it hosts. The native one
/// is what NETHOS's own programs will use, and it exists so that the
/// desktop is not permanently shaped by a kernel interface designed for a
/// different operating system -- handles instead of descriptor numbers,
/// results instead of errno, and calls that say what they mean.
///
/// They coexist per process, not per machine. A native app and a Linux app
/// run side by side, and a native app still has Linux underneath it: the
/// ABI decides which table its syscalls are read against, not what the
/// kernel is able to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abi {
    /// The Linux ABI: asm-generic or x86-64 numbers, errno returns.
    Linux,
    /// The NETHOS ABI. `EI_OSABI` is 0x4E ('N').
    Nethos,
}

/// `EI_OSABI` for a native NETHOS binary: 'N'.
///
/// The byte is at offset 7 of every ELF file and exists for exactly this --
/// naming the interface the binary expects. It sits in the range ELF
/// reserves for "architecture-specific" use, which is where a system that is
/// not one of the registered vendors belongs. A Linux binary has 0
/// (SYSV) or 3 (GNU) there and is unaffected.
pub const ELFOSABI_NETHOS: u64 = 0x4E;

#[derive(Debug)]
pub struct Image {
    /// Which syscall interface this program speaks. Read from `EI_OSABI`.
    pub abi: Abi,
    pub entry: u64,
    pub segments: Vec<Segment>,
    pub relas: Vec<Rela>,
    pub tls: Option<TlsTemplate>,
    /// Where the program headers are in the file, and how many. A libc reads
    /// its own headers at startup -- for TLS, for the stack-guard flag, for
    /// `dl_iterate_phdr` -- so it is handed their *address* in AT_PHDR, and
    /// that can only be worked out from the segment that happens to contain
    /// them.
    pub phoff: usize,
    pub phnum: usize,
    pub interpreter: Option<alloc::vec::Vec<u8>>,
    pub load_bias: u64,
}
fn n(bytes: &[u8], off: usize, len: usize) -> Result<u64, &'static str> {
    let s = bytes
        .get(off..off.checked_add(len).ok_or("overflow")?)
        .ok_or("truncated ELF")?;
    Ok(s.iter()
        .enumerate()
        .fold(0, |v, (i, b)| v | ((*b as u64) << (8 * i))))
}
pub fn parse(b: &[u8], low: u64, high: u64) -> Result<Image, &'static str> {
    parse_at(b, low, high, low)
}
pub fn parse_at(b: &[u8], low: u64, high: u64, bias: u64) -> Result<Image, &'static str> {
    // e_machine: the binary has to be for the architecture nk is running on.
    // 183 is EM_AARCH64, 62 is EM_X86_64. Accepting the wrong one does not
    // fail in the loader -- the segments map, the entry is jumped to, and the
    // first instruction is decoded as whatever this CPU makes of it.
    #[cfg(target_arch = "aarch64")]
    const MACHINE: u64 = 183;
    #[cfg(target_arch = "x86_64")]
    const MACHINE: u64 = 62;

    let kind = n(b, 16, 2)?;
    let bias = if kind == 3 { bias } else { 0 };
    // Byte 7 is EI_OSABI and decides which syscall table the program's
    // `svc`/`syscall` will be read against. Anything that is not NETHOS's
    // own marker is treated as Linux, which is the right default: every
    // binary that exists today says 0 or 3 there, and a file claiming an ABI
    // nk does not implement is better served by the compatibility path than
    // by a refusal.
    let abi = if n(b, 7, 1)? == ELFOSABI_NETHOS {
        Abi::Nethos
    } else {
        Abi::Linux
    };
    if b.get(..7) != Some(b"\x7fELF\x02\x01\x01")
        || !matches!(kind, 2 | 3)
        || n(b, 18, 2)? != MACHINE
        || n(b, 20, 4)? != 1
        || n(b, 52, 2)? != 64
        || n(b, 54, 2)? != 56
    {
        return Err("ELF64 for the wrong architecture, or not little-endian ELF64");
    }
    let entry = n(b, 24, 8)?.checked_add(bias).ok_or("entry overflow")?;
    let phoff = usize::try_from(n(b, 32, 8)?).map_err(|_| "overflow")?;
    let count = n(b, 56, 2)? as usize;
    if count == 0 || count > 32 {
        return Err("invalid program header count");
    }
    let mut interpreter = None;
    let mut segments: Vec<Segment> = Vec::new();
    let mut relas: Vec<Rela> = Vec::new();
    let mut tls: Option<TlsTemplate> = None;
    let mut dyn_vaddr: Option<u64> = None;
    for i in 0..count {
        let p = phoff.checked_add(i * 56).ok_or("overflow")?;
        b.get(p..p.checked_add(56).ok_or("overflow")?)
            .ok_or("truncated headers")?;
        let kind = n(b, p, 4)?;
        if kind == 3 {
            if interpreter.is_some() {
                return Err("multiple interpreters");
            }
            let offset = n(b, p + 8, 8)?;
            let size = n(b, p + 32, 8)?;
            if size < 2 || size > 4096 {
                return Err("invalid interpreter");
            }
            let end = offset.checked_add(size).ok_or("interpreter overflow")?;
            let path = b
                .get(offset as usize..end as usize)
                .ok_or("truncated interpreter")?;
            if path[0] != b'/' || path.last() != Some(&0) || path[..path.len() - 1].contains(&0) {
                return Err("invalid interpreter path");
            }
            interpreter = Some(path.to_vec());
        }
        if kind == 7 {
            let addr = n(b, p + 16, 8)?.checked_add(bias).ok_or("tls address overflow")?;
            tls = Some(TlsTemplate {
                address: addr,
                filesz: usize::try_from(n(b, p + 32, 8)?).map_err(|_| "overflow")?,
                memsz: usize::try_from(n(b, p + 40, 8)?).map_err(|_| "overflow")?,
                offset: usize::try_from(n(b, p + 8, 8)?).map_err(|_| "overflow")?,
                align: n(b, p + 48, 8)?,
            });
        }
        if kind == 2 {
            dyn_vaddr = Some(n(b, p + 16, 8)?);
        }
        if kind != 1 {
            continue;
        }
        let flags = n(b, p + 4, 4)?;
        let offset = n(b, p + 8, 8)?;
        let address = n(b, p + 16, 8)?
            .checked_add(bias)
            .ok_or("address overflow")?;
        let filesz = n(b, p + 32, 8)?;
        let memsz = n(b, p + 40, 8)?;
        let align = n(b, p + 48, 8)?;
        let end = address.checked_add(memsz).ok_or("segment overflow")?;
        if filesz > memsz
            || offset.checked_add(filesz).ok_or("file overflow")? > b.len() as u64
            || address < low
            || end > high
            || flags & 3 == 3
            || flags & 4 == 0
            || (align > 1 && (!align.is_power_of_two() || address % align != offset % align))
        {
            return Err("invalid load segment");
        }
        if memsz == 0 {
            continue;
        }
        let page_start = address & !4095;
        let page_end = end.checked_add(4095).ok_or("overflow")? & !4095;
        if segments.iter().any(|s| {
            page_start < ((s.address + s.memsz as u64 + 4095) & !4095)
                && page_end > (s.address & !4095)
        }) {
            return Err("overlapping load pages");
        }
        segments.push(Segment {
            address,
            offset: offset as usize,
            filesz: filesz as usize,
            memsz: memsz as usize,
            executable: flags & 1 != 0,
            writable: flags & 2 != 0,
        });
    }
    if entry % 4 != 0
        || !segments
            .iter()
            .any(|s| s.executable && entry >= s.address && entry < s.address + s.filesz as u64)
    {
        return Err("entry is not executable file data");
    }
    // Resolve RELATIVE relocations from PT_DYNAMIC, translating the dynamic
    // section's virtual address back to file bytes through the load segments
    // that contain it. Static PIE needs these before entry: every absolute
    // address in .data/.got was linked at vaddr 0 and must gain the bias, or
    // the first indirect call jumps into the null page.
    let mut rela_off = 0usize;
    let mut rela_bytes = 0usize;
    // Only for an executable that has no interpreter. A dynamically linked
    // binary has a `.rela.dyn` too, but it is `ld.so`'s to apply, not the
    // kernel's: it is full of symbol relocations that need the libraries
    // resolved first, and the kernel has neither loaded them nor any business
    // doing so. Applying it here meant every ordinary aarch64 program failed
    // to load with "unsupported relocation type" -- the kernel refusing a
    // relocation that was never addressed to it.
    let self_relocating = interpreter.is_none();
    if let Some(dyn_vaddr) = dyn_vaddr.filter(|_| self_relocating) {
        // .dynamic values are link-time addresses; add the load bias before
        // comparing against segments, whose addresses are already biased.
        let dyn_vaddr = dyn_vaddr.checked_add(bias).ok_or("dyn overflow")?;
        let dyn_off = segments
            .iter()
            .find(|s| dyn_vaddr >= s.address && dyn_vaddr < s.address + s.filesz as u64)
            .map(|s| (dyn_vaddr - s.address) as usize + s.offset)
            .ok_or("dynamic section outside load segments")?;
        let mut i = 0usize;
        let (mut rela_vaddr, mut rela_sz) = (0u64, 0u64);
        loop {
            let e = dyn_off.checked_add(i * 16).ok_or("overflow")?;
            let tag = n(b, e, 8)?;
            let val = n(b, e + 8, 8)?;
            if tag == 0 { break; }
            match tag {
                7 => rela_vaddr = val,
                8 => rela_sz = val,
                9 => { if val != 24 { return Err("unexpected rela entry size"); } }
                _ => {}
            }
            i += 1;
            if i > 64 { return Err("dynamic section too long"); }
        }
        if rela_sz > 0 {
            let rela_vaddr = rela_vaddr.checked_add(bias).ok_or("rela overflow")?;
            let seg = segments
                .iter()
                .find(|s| rela_vaddr >= s.address && rela_vaddr < s.address + s.filesz as u64)
                .ok_or("rela outside load segments")?;
            rela_off = (rela_vaddr - seg.address) as usize + seg.offset;
            rela_bytes = rela_sz as usize;
            if rela_bytes % 24 != 0
                || rela_off.checked_add(rela_bytes).ok_or("overflow")? > b.len()
            {
                return Err("invalid rela table");
            }
        }
    }
    let mut j = 0usize;
    while j < rela_bytes {
        let e = rela_off + j;
        let r_offset = u64::from_le_bytes(b[e..e + 8].try_into().map_err(|_| "truncated rela")?);
        let r_info = u64::from_le_bytes(b[e + 8..e + 16].try_into().map_err(|_| "truncated rela")?);
        let r_addend = u64::from_le_bytes(b[e + 16..e + 24].try_into().map_err(|_| "truncated rela")?);
        // R_*_RELATIVE: "add the load bias to the word at this address". It
        // is the only relocation a static-PIE executable needs, and the only
        // one applied here -- but its *number* is per-architecture, and the
        // two do not agree: 8 is R_X86_64_RELATIVE, 1027 is
        // R_AARCH64_RELATIVE. Hardcoding x86's rejected every aarch64 binary
        // with "unsupported relocation type", which is a true statement about
        // the wrong architecture.
        const RELATIVE: u64 = if cfg!(target_arch = "x86_64") { 8 } else { 1027 };
        // Anything else is skipped rather than refused. A symbol relocation
        // needs a symbol table resolved against libraries, which is `ld.so`'s
        // work and not something the kernel can or should do -- and `ld.so`
        // itself is an executable with no interpreter that carries one
        // (R_AARCH64_ABS64 against its own `_DYNAMIC`) and applies it during
        // its own bootstrap. Refusing it meant nk could not load the dynamic
        // loader at all.
        if r_info & 0xffffffff == RELATIVE {
            relas.push(Rela {
                address: r_offset.checked_add(bias).ok_or("rela overflow")?,
                addend: r_addend.checked_add(bias).ok_or("rela overflow")?,
            });
        }
        j += 24;
        if relas.len() > 1 << 20 {
            return Err("too many relocations");
        }
    }
    Ok(Image {
        abi,
        entry,
        segments,
        relas,
        tls,
        phoff,
        phnum: count,
        interpreter,
        load_bias: bias,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn put(b: &mut [u8], o: usize, v: u64, len: usize) {
        b[o..o + len].copy_from_slice(&v.to_le_bytes()[..len]);
    }
    fn fixture() -> Vec<u8> {
        let mut b = alloc::vec![0; 4100];
        b[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        for (o, v, l) in [
            (16, 2, 2),
            (18, 183, 2),
            (20, 1, 4),
            (24, 0x1000, 8),
            (32, 64, 8),
            (52, 64, 2),
            (54, 56, 2),
            (56, 1, 2),
            (64, 1, 4),
            (68, 5, 4),
            (72, 4096, 8),
            (80, 0x1000, 8),
            (96, 4, 8),
            (104, 4096, 8),
            (112, 4096, 8),
        ] {
            put(&mut b, o, v, l);
        }
        b
    }
    #[test]
    fn valid_bss() {
        let i = parse(&fixture(), 0x1000, 0x10000).unwrap();
        assert_eq!(i.entry, 0x1000);
        assert_eq!(i.segments[0].memsz, 4096);
        assert_eq!(i.segments[0].filesz, 4);
        assert!(!i.segments[0].writable);
    }
    #[test]
    fn all_truncations_rejected() {
        let b = fixture();
        for len in 0..b.len() {
            assert!(parse(&b[..len], 0x1000, 0x10000).is_err());
        }
    }
    #[test]
    fn invalid_headers_and_segments() {
        for (off, value, len) in [
            (18, 62, 2),
            (64, 3, 4),
            (64, 2, 4),
            (64, 7, 4),
            (68, 7, 4),
            (32, u64::MAX, 8),
            (72, u64::MAX, 8),
            (104, 2, 8),
            (104, u64::MAX, 8),
            (80, 0, 8),
            (80, 0x10000, 8),
            (112, 3, 8),
            (24, 0x1004, 8),
            (24, 0x1001, 8),
            (56, 33, 2),
            (56, 0, 2),
        ] {
            let mut b = fixture();
            put(&mut b, off, value, len);
            assert!(
                parse(&b, 0x1000, 0x10000).is_err(),
                "field {off}, value {value}"
            );
        }
    }
    #[test]
    fn pie_is_rebased() {
        let mut b = fixture();
        put(&mut b, 16, 3, 2);
        let image = parse_at(&b, 0x1000, 0x10000, 0x4000).unwrap();
        assert_eq!(image.entry, 0x5000);
        assert_eq!(image.segments[0].address, 0x5000);
        assert!(parse_at(&b, 0x1000, 0x10000, u64::MAX).is_err());
    }
    #[test]
    fn interpreter_path_is_bounded_and_nul_terminated() {
        let mut b = fixture();
        put(&mut b, 56, 2, 2);
        put(&mut b, 120, 3, 4);
        put(&mut b, 128, 200, 8);
        put(&mut b, 152, 8, 8);
        b[200..208].copy_from_slice(b"/ld.so\0\0");
        assert!(parse(&b, 0x1000, 0x10000).is_err());
        put(&mut b, 152, 7, 8);
        assert_eq!(
            parse(&b, 0x1000, 0x10000).unwrap().interpreter.unwrap(),
            b"/ld.so\0"
        );
        b[206] = b'x';
        assert!(parse(&b, 0x1000, 0x10000).is_err());
    }
    #[test]
    fn overlapping_pages_rejected() {
        let mut b = fixture();
        let ph = b[64..120].to_vec();
        b[120..176].copy_from_slice(&ph);
        put(&mut b, 56, 2, 2);
        assert!(parse(&b, 0x1000, 0x10000).is_err());
    }
}

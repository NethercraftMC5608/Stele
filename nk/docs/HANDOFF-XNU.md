# Handoff: macOS 27 booting on nk (2026-11-25)

State of the `nk` branch (PR #2, NethercraftMC5608/Stele) and how to carry on.

## Where it is

macOS 27.2.1 (26B131) for the MacBook Air M2 (Mac14,2) boots on nk as far as
IOKit with its **restore ramdisk mounted as root**. Chain that works:

1. nk (Rust kernel) + LKL (Linux 6.12) on x86-64; CPU 3 dedicated to the guest.
2. Bundle (`stela-dev boot prepare`) read from an NVMe disk image through
   Linux (`/dev/nvme0n1`, cpio layout) into guest DRAM at 0x800000000
   (4 GiB, from RAM above nk's 4 GiB identity map).
3. Whole M2 board mounted with `stela_runtime::machine::mount::devices_with_memory`.
4. Stela's engine runs SPTM -> TXM -> XNU on nk's `DramHost`
   (`nk/core/src/stela_nk/host/sptm.rs`) with demand-filled shadow page
   tables (filled in nk's #PF path, `stela_nk/fault.rs`).
5. XNU: VM, BSD, AMFI/Sandbox/ASP, ramdisk root (`BSD root: md0`, APFS
   mount), ANS2 storage, PMP/SMC/SEP/DCPEXT/GFX coprocessor mailboxes,
   PMGR, CLPC, SEP manager registered.

**Current stop:** `panic(cpu 3): "Unhandled E-core error: LLC_ERR_STS=0x10000f7c00000000 ..."`
from `AppleFireStormErrorHandler`. The engine returns iBoot's handoff value
for L2C_ERR_STS on purpose (`stela-engine/src/machine/impdef.rs`,
`L2C_ERR_STS_HANDOFF`); XNU's error handler only runs after an SError or an
implementation-defined error exception, which the engine is not supposed to
raise. So something delivered one to PE 3. **Next step: find what.** Ideas:
- a native fault nk classifies as `FaultCause::Device` for an address with
  no device model (`DramHost::execute`, MMU-on branch: "RAM the shadow does
  not map" also returns `Device`) -> engine raises an external abort that
  XNU takes as an SError;
- dump PE 3's ESR/ELR/FAR at the moment of the SError: add a check in the
  run loop in `sptm.rs` (or an engine `Event` report via `Host::report`)
  when `cpu.pc` lands in XNU's SError vector (`VBAR_EL2 + 0x180/0x380`).

Earlier stops, all fixed (each is a commit on the branch):
seed bytes (entropy patches), progress bar (`-noprogress`, needs a real
framebuffer later), root disk (restore ramdisk), memory-cache wait
(`-disable_mcc`, which `stela.efi` adds for the cacheless memory plan).

## Machines

- **Mac** (this repo): `/Users/mac/Stele`, nk in `nk/`. Apple files live in
  `~/Stele-assets` (never commit them): the IPSW, bundles, `nk-machine-ecid.txt`
  (generated ECID for the nk test machine), `nk-overlay.json`,
  `restore-bundle.img` / `restore-noisp.img` (bundle disk images).
- **Bazzite PC** (Intel i5-12400F, 32 GB): `ssh nether@192.168.1.9`
  (hostname `default-rdns`, key auth). KVM works (`/dev/kvm` rw). QEMU runs
  in podman image `localhost/nk-qemu` (Fedora 43, qemu 10.1.5). Work dir
  `~/nk-xnu`: `nk-restore` (kernel), `desktop-x86.cpio`, `restore-bundle.img`,
  `tests/run-dedicated-x86.py`, `xnu-restore-kvm.sh`. ~20x faster than the
  Mac's TCG (~3 M guest blocks/s).

## How to run

Build on the Mac (from `nk/`), copy, run on the PC with a live console:

```sh
NK_STELA_SPTM_DRAM_MIB=4096 NK_STELA_SPTM_SECONDS=3600 NK_STELA_SPTM_TEST=1 \
  NK_SMP=1 NK_AP_LAYER=4 NK_LKL_LIB=$PWD/ldk/build/lkl-x86_64/libnklkl.a \
  cargo build --release --features stela --target x86_64-unknown-none --target-dir target-xnu-restore
scp target-xnu-restore/x86_64-unknown-none/release/nk nether@192.168.1.9:nk-xnu/nk-restore
ssh -t nether@192.168.1.9 nk-xnu/xnu-restore-kvm.sh     # Ctrl-C stops it
```

The probe stops by itself after 90 s of console silence
(`NK_STELA_SPTM_STALL`) and dumps x0-x30, ESR/ELR/FAR, and code at PC and LR.
The full log is `~/nk-xnu/run-restore/serial.log` on the PC (`GUEST|` lines
are XNU's console).

A new bundle: `stela-dev boot prepare --ipsw ~/Stele-assets/UniversalMac_27.2.1_26B131_Restore.ipsw
--model Mac14,2 --ecid "$(cat ~/Stele-assets/nk-machine-ecid.txt)" --dram-size 0x100000000
--restore --output DIR` (note: `--restore` contacts Apple's signing service,
gs.apple.com, with that ECID), then `python3 nk/tests/bundle-disk.py DIR IMG`
and copy IMG to the PC (edit the image name in the script).

Without the PC: `nk/tests/xnu-live.sh` boots the non-restore bundle under
TCG on the Mac (slow; stops at "Waiting on IOMedia Apple_HFS", no root disk).

## Pitfalls learned (also in AGENTS.md)

- Kernel addresses change every boot (KASLR): find a dumped PC's code in
  the kernelcache by its bytes (unique, page-aligned slide), then read the
  strings its adrp/add pairs reference to name the kext.
- QEMU `-cpu max` (TCG) or `-cpu host` (KVM): qemu64 makes SPTM die on `ldapr`.
- `Machine::run(n)` budgets are cumulative block counts.
- Bundle `--dram-size` must equal what nk backs (`NK_STELA_SPTM_DRAM_MIB`).
- `prepare` patches (entropy, boot-args) are filled by the bootloader at
  boot; nk applies entropy and appends `-disable_mcc`; framebuffer and NVRAM
  patches are still unapplied.
- Podman: scripts use `--replace`; a stale container blocks a new run.
- In zsh write `${f}:stele/${f}` (`$f:s` is a history modifier).

## Bigger remaining work (ROADMAP.md)

User space (`launchd` in the restore ramdisk), a guest framebuffer
(`boot_args.Video`), SSD -> Linux block device bridge (Phase 6), the other
PEs on their own dedicated cores (now all 8 PEs share CPU 3 serially), and
booting nk directly on the PC (Phase 2 real hardware).

# Handoff: macOS 27 booting on nk (2026-11-25)

State of the `nk` branch (PR #2, NethercraftMC5608/Stele) and how to carry on.
Read `AGENTS.md` and `ROADMAP.md` first. The changes described below are in
the shared working tree, with no new commit.

## Shadow page tables: kept per regime (2026-11-25)

`DramHost` keeps up to 8 filled shadows keyed by the whole regime and
implements `invalidate_reach`: the current shadow takes the TLBI's `reach`,
kept shadows take its `everywhere` (address forms page-wise, ASID-wide or
full forms clear). Invalidation covers whole guest pages (macOS uses 16 KiB
pages: four x86 pages) and clears a shadow when a TLBI touches a recorded
block mapping. A write to a present read-only shadow page upgrades it in
the #PF fill (ARM lets a guest make a page writable without a TLBI).
Before the 16 KiB fix, kept shadows served stale x86 pages: launchd died
with CS_KILLED (page hash mismatch) or "Library not loaded".
Measured on Bazzite (restore boot, 790 s): page fills 12.3 M -> 218 k,
full clears 606 k -> 8.9 k, 659 k of 729 k regime switches reuse a kept
shadow. Wall-clock speed improved only ~10-15% (launchd at ~70 s vs
~80 s): the main cost is the 8 PEs time-slicing on one host core.
Both builds go quiet at the same point after syslogd starts, with
restored_external running and DWC3 never powered: a guest-side wait,
not slowness. Build options: `NK_STELA_SHADOW_CACHE=0` restores the
clear-on-every-switch behaviour.

## Resume here

1. Continue the **restore USB handshake**. The nine-CPU Bazzite run reached
   `restored_external` running as PID 5 at about 425 host seconds. It
   ended at the external QEMU timeout of 660 s. The USB host never read a
   configuration descriptor or received QueryType. Port 1 briefly showed
   `DCTL.RUN is clear` and later returned to DWC3 power off with USB2
   USBCTL isolated. Reboot with the existing traced kernel and a longer
   timeout. Correlate port 1 power, ATC PHY and DCTL writes with the
   daemon's progress. The port state is an observation, and the exact
   handshake blocker remains open.
2. After `restored` answers, wire up `stela_runtime::restore::driver::Driver`
   through the existing `UsbClient` ports and drive it from bounded turns
   between guest slices. Reuse the staged inventory and the UEFI restore
   service's protocol flow. Neither `StartRestore` nor an installation
   has run. Verify persistent writes and APFS on the dedicated NVMe
   namespace before preparing the boot of the installed system.
3. Wire concurrent PEs using the engine's shared-board dispatcher, with
   canonical shared RAM and per-PE host state. The scheduler prerequisite
   is verified. The eight guest PEs still time-slice on CPU 3. Keep host
   CPU 0 available for LKL. Bazzite has six physical cores with twelve
   hardware threads, so eight separate guest slots here use logical CPUs.
4. The boot framebuffer already draws. Visible scanout needs a host
   display bridge.

Preserved evidence on Bazzite is in
`~/nk-xnu/run-launchd-final/restored-pid5.log`. The latest built kernel
uploaded there is `~/nk-xnu/nk-restore-traced`, with the opt-in USB
and SSD probes and complete syscall observation. It built successfully.
The preserved PID 5 log is from its predecessor `nk-restore-nine`.

On Bazzite, from `~/nk-xnu`, the next agent can run:

```sh
podman run --rm --name nk-xnu-restore-traced --device /dev/kvm \
  -v "$PWD":/w:Z -w /w localhost/nk-qemu \
  timeout 1200 qemu-system-x86_64 -M q35 -m 12G \
  -smp 9,sockets=3,cores=3,threads=1 -cpu host -accel kvm \
  -drive file=restore-bundle.img,if=none,id=bundle,format=raw,readonly=on \
  -drive file=stele-install-ssd.img,if=none,id=install,format=raw \
  -device nvme,id=stele-nvme,serial=nkbundle \
  -device nvme-ns,drive=bundle,bus=stele-nvme,nsid=1 \
  -device nvme-ns,drive=install,bus=stele-nvme,nsid=2 \
  -kernel nk-restore-traced -initrd desktop-x86.cpio \
  -display none -serial file:run-launchd-final/serial.log -monitor none -no-reboot
```

The traced build has a 240 s internal console-silence cutoff. Rebuild with a
larger `NK_STELA_SPTM_STALL` if the next investigation needs to distinguish a
long quiet startup from a hang. Enable both `NK_STELA_SPTM_NVME_SSD=1` and
`NK_STELA_SPTM_RESTORE_USB=1` when rebuilding.

The Mac has the 26B131 IPSW and a matching inventory in
`~/Stele-assets/restore-inventory.moqQxA` (`payload.json`, `blocks.bin`,
`report.json`). The IPSW remains on the Mac. The 256 GiB SSD image on
Bazzite was created as a sparse file and had zero allocated blocks after
the PID 5 run. Keep Apple assets out of Git.

## Where it is

macOS 27.2.1 (26B131) for the MacBook Air M2 (Mac14,2) boots on nk into
**restore userspace**, with `launchd` as PID 1. Chain that works:

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
6. XNU launches `/sbin/launchd`. The Bazzite KVM run identifies PID 1 as
   `launchd` and prints the Darwin Ignition Sequence banner and libignition
   arguments from userspace.

**Diagnosed panic:** AppleT8112CLPC calls ApplePMGR to sample the ANE energy
counter. ApplePMGR's 64-bit `ldr x0, [x8]` at `0xfffffe00381f9ad0` in
the traced boot reads VA `0xfffffe4b48604108`, PA `0x26b910108`. The
restore ADT's `/arm-io/pmgr/hw-dpe-reg` still advertises that address as
the direct 64-bit ANE sample, although the board has no MMIO model there.

The engine reports `no device model` for that PA and takes a synchronous
external data abort at EL2 on PE 3 (`ESR=0x96000010`, EC `0x25`,
DFSC `0x10`). XNU dispatches the abort to
`AppleFireStormErrorHandler`, which panics with
`LLC_ERR_STS=0x10000f7c00000000`. That register holds the engine's iBoot
handoff enable bits (`L2C_ERR_STS_HANDOFF`). The instrumented boot
recorded no SError before the panic. The evidence is on the Bazzite PC
in `~/nk-xnu/run-restore-trace/serial.log` (`STELA_SPTM: exception` and
`STELA_SPTM: degraded`).

Bundle preparation now withdraws the direct ANE sample from
`/arm-io/pmgr/hw-dpe-reg` when the ANE is hidden. For older bundles, nk
applies the same change after the offset-based boot patches and updates
`boot_args.deviceTreeLength` before entering SPTM. The original bundle
can still be used with the restore runner.

**Next fault after the ANE fix:** On Bazzite under QEMU/KVM, the original restore
bundle mounted `md0` and XNU printed
`load_init_program: attempting to load /sbin/launchd`. The next stop was a
synchronous external data abort on PE 5 (`ESR=0x96000050`). The engine
reported a write to VA `0x16d653e10` and PA `0x8247c3e10` as unmodeled
MMIO, even though that PA is inside guest DRAM. The log for that run is
`~/nk-xnu/run-restore-ane-fix/ane-only.log` (`STELA_SPTM: ARM trap` and
`STELA_SPTM: degraded`).

The shadow now demand-fills lower VA holes with translated RAM. It makes
private copies of shared x86 page-table branches before installing leaves and
discards them on regime changes and translation invalidations. A refused
RAM fill stops at the host boundary. The fixture still shares nk's mappings
and is for trusted boot code. The first userspace run is in
`~/nk-xnu/run-launchd-final/userspace-first.log` on Bazzite.
The repeated run logged the first lower-address EL0 entry and its first
SVC in `~/nk-xnu/run-launchd-final/userspace-confirmed.log`.

The common boot bundle patch writer now gives nk a 1920×1080 boot
framebuffer at the top of guest DRAM, reserved from XNU's managed RAM.
The original `-progress` bundle drew pixels and completed libignition on
Bazzite. That serial log is `~/nk-xnu/run-launchd-final/framebuffer.log`.
The host scanout still discards frames.

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

The probe stops by itself after 240 s of console silence
(`NK_STELA_SPTM_STALL`) and dumps x0-x30, ESR/ELR/FAR, and code at PC and LR.
The full log is `~/nk-xnu/run-restore/serial.log` on the PC (`GUEST|` lines
are XNU's console).

Bazzite's Steam Gaming Mode session can suspend the host during a run.
To hold a sleep inhibitor from SSH, run this in a separate terminal and
leave it open during the test. Ctrl-C releases it.

```sh
ssh -t nether@192.168.1.9 'sudo systemd-inhibit --what=sleep:idle --mode=block --who="Stele KVM" --why="Restore boot" sleep infinity'
```

A new bundle: `stela-dev boot prepare --ipsw ~/Stele-assets/UniversalMac_27.2.1_26B131_Restore.ipsw
--model Mac14,2 --ecid "$(cat ~/Stele-assets/nk-machine-ecid.txt)" --dram-size 0x100000000
--restore --output DIR` (note: `--restore` contacts Apple's signing service,
gs.apple.com, with that ECID), then `python3 nk/tests/bundle-disk.py DIR IMG`
and copy IMG to the PC (edit the image name in the script).

Without the PC: `nk/tests/xnu-live.sh` boots the non-restore bundle under
TCG on the Mac (slow; stops at "Waiting on IOMedia Apple_HFS", no root disk).

### Restore USB and install disk probes

Build with `NK_STELA_SPTM_RESTORE_USB=1` to cable the modeled restore host
on the first USB-C port of a restore boot. It sends the existing read-only
`QueryType` probe on the mux stream. The ramdisk declares
`com.apple.restored_external` as a RunAtLoad job for
`/usr/local/bin/restored_external`. A successful response verifies the
daemon is reachable before any restore request goes out.

Build with `NK_STELA_SPTM_NVME_SSD=1` to give the ANS2 a persistent disk.
Use a new sparse raw image as the writable second namespace. The first
namespace, holding the restore bundle, stays read-only. The two namespaces
share one controller because nk's current LKL PCI host selects a single
device. The runner accepts `--disk restore-bundle.img --ssd stele-install-ssd.img`.
CPU 0 opens the second namespace exclusively and services the guest AP's
read, write and flush requests through LKL.
In the KVM probe, the AP's startup read and flush succeeded on a fresh
256 GiB sparse image. The restore has yet to issue an install write.

The old 90 s console-silence limit ended the first probes during userland
bootstrap. With the longer limit, launchd spawned the job's xpcproxy as
PID 5 at about 326 host seconds. The process then invoked
`posix_spawn("/usr/local/bin/restored_external")` and launchd reported
`restored_external[5]` running at about 425 host seconds.
The full log is `~/nk-xnu/run-launchd-final/serial.log`.

### Dedicated PE prerequisites

nk x86 now has twelve logical CPU slots. QEMU booted nine and twelve CPUs
with sparse APIC IDs and passed the ring-3 epoll harness. The dedicated
task on logical CPU 8 passed the preemption and LAPIC timer checks. Native
fault recovery, guest timer state and kick delivery now have one owner
per host CPU. The restore probe still time-slices the eight M2 PEs on CPU 3.
Concurrent execution needs the engine's shared-board PE dispatcher and
canonical RAM ownership across those host CPUs. CPU 0 remains reserved for
LKL.

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

Continue past restore userspace startup, add a guest framebuffer
(`boot_args.Video`), bridge SSD -> Linux block device (Phase 6), run the other
PEs on their own dedicated cores (now all 8 PEs share CPU 3 serially), and
booting nk directly on the PC (Phase 2 real hardware).

# Roadmap: macOS 27 on nk + LKL

The ordered checklist for getting from where Stele and nk are today to a
macOS 27 desktop on an x86-64 PC, with Stela's translator and M2 model running
on nk, and the PC hardware driven by unmodified Linux drivers through LKL.
Read [AGENTS.md](AGENTS.md) first for the architecture and the names.

Each phase depends on the one before it. Each item says when it counts as
done. `[x]` means done and verified, and the item names where.

Stela itself already installs and boots macOS 27 (`stela-cli` handles `27.x`
restore images). So the macOS side is solved once. The work below is mostly
moving that proven path onto a new host.

---

## Phase 0: baseline on Stela as it is

Know what "working" looks like before changing the host.

- [ ] Build Stele unchanged (`cargo xtask dist`) and boot macOS 27 on one
      supported PC (GTX 16 / RTX GPU, supported NIC). Done when: login screen.
- [ ] Save that boot's log (`stela log --output`) as the reference: device
      order, timing, the first guest serial lines. Done when: committed under
      `docs/reference/`.
- [ ] Write down which Stela drivers that machine used (storage, NIC, audio,
      USB, GPU). These are the ones LKL must replace first.

## Phase 1: nk x86-64 userland (QEMU)

Branch `nk-x86-userland` in the NETHOS repo.

- [x] Boot, 4 CPUs, scheduler torture, LKL starts (`smp-sleep-halt-fix`).
- [x] Ring 3 with a static binary: 1 GiB link, private low page tables,
      `syscall_entry` stack index, TLS pages (`9826149`, `04f4650`, `7b87594`).
- [x] fork / vfork / execve (`4827970`, `5d8edac`).
- [x] 309 syscalls translated by name, `stat` converted, legacy calls (`5d8edac`).
- [x] Signals with handlers and `rt_sigreturn` (`ae78a27`).
- [x] `cpu_id` rbx truncation (`72b0b82`); open flags to arm64 values, epoll
      repacking (`08d835e`).
- [x] Harness `/nk-init` runs to the end; `ls`, `find`, pipes, file I/O.
- [x] Exercise epoll (a small static test program): `epoll_wait`,
      `epoll_pwait`, `epoll_pwait2` return the right fd and 64-bit data on
      nk + LKL in QEMU with four active CPUs. Removed the incorrect 16-byte
      conversion: x86 LKL already uses packed 12-byte events. Reproducer:
      `nk/tests/run-epoll-x86.py` (verified 2026-11-24; uncommitted).
- [x] Dynamically linked programs: `ld-linux-x86-64.so`, glibc, Python 3.13
      prints `1`, imports ctypes, and calls `libm.cos(0.0)` through `dlopen`.
      Rebuilt x86 LKL with corrected retained-file syscalls; archive ABI guard
      passes. Added x86 file-page trampoline, full register return and syscall
      FP/SIMD preservation. `nk/tests/run-dynamic-x86.py` passes on four-CPU
      q35 QEMU (2026-11-24; uncommitted).
      The harness now provisions fresh host entropy, credits it through
      Linux RNDADDENTROPY, and requires nonblocking getrandom success before
      Python. This removes an unseeded-CRNG stall measured in Linux's entropy
      generation path; a production nk entropy provider remains open
      (`nk/init/seed-entropy-x86.c`, 2026-11-25; uncommitted).
- [x] Threads on x86: eight pthread-backed Python threads simultaneously
      reach a barrier, retain distinct glibc errno/TLS and Python thread-local
      values across timed waits, increment a locked counter 160 times and join.
      `nk/tests/run-dynamic-x86.py --threads` passes on four QEMU CPUs
      (2026-11-24; uncommitted). Robust-list/rseq remain unsupported; this does
      not verify cancellation, owner-death recovery or all pthread APIs.
- [x] Return-value-before-signal ordering on aarch64: measured `kill()`
      restoring its PID argument instead of zero, then fixed the shared
      return path. Also fixed saving the temporary handler mask, which
      blocked all later deliveries on both architectures. The freestanding
      `nk/tests/run-signal-return.py` probes pass on QEMU virt/aarch64
      (one CPU) and q35/x86-64 (four CPUs), including repeated delivery,
      handler masks and blocked/pending signals (2026-11-24; uncommitted).
- [x] Removed `LKL_CPU_SEED` and runtime BSS searches. The build makes a
      private archive exporting `nk_lkl_cpu`; the final linker supplies its
      address. Live layout checks and QEMU boots pass on both architectures;
      `nk/tests/lkl-archive.rs` tests x86/arm64 fixtures and input immutability
      (2026-11-24; uncommitted).
- [ ] Merge `nk-x86-userland` with `nk-initrd-builder` and push.

## Phase 2: nk on a real PC

LKL's drivers attach to real hardware. Pick one reference PC (from Phase 0).

- [ ] Boot path: GRUB Multiboot2 on UEFI (already parsed by
      `bootinfo_x86.rs`), or nk's own EFI stub. Done when: nk's banner on serial
      or framebuffer on real hardware.
  - [x] QEMU + OVMF: a GPT disk with GRUB on its ESP boots nk from a USB
        mass-storage device; Multiboot2 memory map, ACPI and framebuffer tags
        are used; SMP comes up; the init script runs
        (`nk/tests/run-uefi-x86.py`, 2026-11-25). The same `disk.img` can be
        written to a USB stick.
- [ ] A console that works without serial: UEFI GOP framebuffer text, and
      later netconsole. Done when: the boot log can be read on screen.
  - [x] GOP framebuffer text console (`core/src/fbcon.rs`), fed from the
        serial print path; verified by screenshot under OVMF (2026-11-25).
        Real hardware, higher resolutions and netconsole still open.
- [ ] ACPI: RSDP → XSDT → MADT (CPUs, IOAPIC, LAPIC address), MCFG (PCIe
      ECAM), FADT (reset, power off). Replace the q35 assumptions in `smp.rs`
      and `lapic.rs`. Done when: all CPUs come up on the real machine.
  - [x] QEMU discovery: PVH RSDP, legacy BIOS scan, bounded/checksummed
        RSDT/XSDT, MADT (including overrides/x2APIC records), MCFG, FADT
        register metadata. Host tests also cover GRUB Multiboot2 ACPI tags.
        MADT drives AP startup and logical-slot → physical-APIC IPIs;
        sparse IDs `0,1,2,4,5,6` boot successfully. MMIO is supervisor-only
        and uncached. See `nk/tests/run-platform-x86.py` and
        `nk/tests/platform-x86.rs` (2026-11-24; uncommitted).
  - [ ] Verify the GRUB boot path, ACPI topology and all CPUs on a real PC.
        FADT discovery does not yet implement reset/shutdown or AML `_S5`.
- [ ] IOAPIC and MSI/MSI-X delivery into nk's interrupt path.
  - [x] QEMU: PIT routed through its MADT IRQ0 override and IOAPIC, PIC IRQ0
        masked, correct LAPIC EOI. Device vectors `0x50..0xdf` dispatch
        bounded callbacks and can defer an LKL IRQ to the existing pump.
        MSI (EDU) and MSI-X (82574) each deliver and acknowledge three actual
        device-generated interrupts, then restore configuration/table state.
        ECAM, sparse APIC IDs and CF8/CFC/PIC fallback all pass alongside
        ring-3 epoll. `nk/tests/run-pci-irq-x86.py`, 21 host tests
        (2026-11-24; uncommitted).
  - [ ] Real hardware, PCI INTx `_PRT` routing, and delivery to an attached
        Linux driver's ISR. The QEMU probes do not attach LKL drivers.
- [ ] PCIe enumeration and LKL's PCI host ops (`lkl_dev_pci_ops` in
      `host_ops.h`): config space, BARs, DMA mapping, MSI-X. Done when: LKL logs
      PCI devices.
  - [x] Read-only nk inventory via MCFG ECAM, plus segment-zero CF8/CFC
        fallback. QEMU finds multifunction devices, 32/64-bit BAR addresses,
        and NVMe behind a PCIe root port; host tests cover multiple segments
        and nonzero start buses (2026-11-24; uncommitted).
  - [x] LKL attachment in QEMU: `lkl_pci_x86.rs` serves LKL's `pci_ops`
        (native-width config, BARs mapped uncached -- high 64-bit BARs through
        a kernel MMIO window at 12 GiB -- identity DMA, MSI and MSI-X wired to
        Linux IRQs through device vectors and the LKL IRQ pump). LKL's PCI host
        is x86-only (`lkl/nk-x86_64.config`). `nk/tests/run-nvme-x86.py`
        (2026-11-24).
  - [x] Several PCI devices: `lkl_pci=` takes a list, one root bus each
        (`ldk/patch-lkl.py: multi_pci_devices`); nk passes NVMe and the first
        Ethernet controller (2026-11-25).
  - [ ] INTx (needs ACPI `_PRT`/AML) and multiple-message MSI; BAR
        assignment when firmware left a BAR unassigned.
- [ ] Linux drivers on real devices, one class at a time, each from LKL:
  - [ ] NVMe (`nvme`): read the disk's GPT. QEMU done: Linux's driver
        binds, reads the GPT (`nvme0n1: p1`), and busybox reads and writes
        the partition. Real hardware still open.
  - [ ] Ethernet (`igc`/`e1000e`/`r8169`): DHCP and a ping. QEMU done
        for `e1000e`: link up, DHCP lease, 3/3 pings
        (`nk/tests/run-net-x86.py`, 2026-11-25). Real hardware, and
        `igc`/`r8169`, still open.
  - [ ] xHCI + HID (`xhci_hcd`, `usbhid`): keyboard input. QEMU done: a
        key typed on QEMU's USB keyboard is read as `EV_KEY` from
        `/dev/input/event0` (`nk/tests/run-usb-hda-x86.py`, 2026-11-25).
  - [ ] HD Audio (`snd_hda_intel`): play a tone. QEMU: the controller and
        codec probe with MSI and card 0 registers; playback not tested yet
        (busybox has no player). `igc` and `r8169` are built in, untested
        until real hardware.
  - [ ] Wi-Fi (`iwlwifi` + firmware): associate. (Stela has no Wi-Fi at all.)
- [ ] LKL's single-CPU bottleneck measured on real I/O (disk throughput,
      network). Decide whether it's acceptable for a guest. If not, plan
      multi-CPU LKL or per-device threading.
- [ ] Optional: move LKL from 6.12 to a newer tree if a needed driver is missing.

## Phase 3: nk as a host for a translated guest

What Stela's runtime needs from the kernel underneath it.

- [ ] Guest memory: large physically contiguous regions (several GiB), 2 MiB
      pages, mapped where the engine expects. Done when: the engine's memory
      service allocates guest RAM on nk.
  - [x] Small MMU-on fixture: guest VA, guest PA and native RAM backing differ.
        Four-level ARM tables feed an eager private x86 shadow aperture; code
        and data mappings, CR3 restoration and borrowed-frame teardown pass.
        After guest TLBI, a read-only write takes a native protection fault
        projected to an ARM level-3 permission abort; removing a leaf takes a
        level-3 translation abort and preserves the load destination.
        `nk/tests/run-stela-x86.py` (2026-11-25; uncommitted). Fixed 2 MiB EL1
        aperture, not multi-GiB memory, general demand filling or isolation.
  - [x] Sparse guest physical owner reserves 4 GiB without allocating the
        whole range. Zero reads, cross-page copies, checked bounds and model
        DMA partial-transfer progress pass; the standalone owner probe uses
        three backing pages. Translated MMU-on data near guest PA 6 GiB maps
        to an nk frame below 4 GiB. Shadow leaves borrow this backing
        (`nk/tests/run-stela-x86.py`, 2026-11-25; uncommitted). The virtual
        aperture remains 2 MiB; contiguous bulk memory, 2 MiB leaves and the
        production engine memory service remain open.
  - [x] One-way handoff of committed nk frames to the engine's real CPL0
        `RegisteredSystemRam`, preserving bytes and transferring ownership
        without copying. Canonical scatter publication, descriptor CAS,
        hole/bounds refusal, exclusive service, permanent domain failure and
        exact frame reclamation pass (`nk/tests/run-stela-x86.py`,
        2026-11-25; uncommitted). Measured PVH CR0 lacked NE; BSP/AP boot now
        enables native x87 exceptions to satisfy existing target admission.
        Separate pre-SMP probe only: canonical native execution and full-board
        memory-controller admission remain open; sparse holes are not registered.
- [x] Dedicated cores: pin guest vCPU threads, keep LKL and nk's own work on
      other CPUs. Done when: a busy loop on a vCPU core is not preempted.
  - `sched::dedicate_cpu(cpu, ...)` reserves an AP for one task: other tasks
    lose it from their affinity, new tasks never get it, it never steals, and
    its LAPIC tick is masked while the task runs; an AP may run tasks that
    exclude CPU 0 while Linux is active. QEMU, with Linux up on CPU 0: the
    probe on CPU 3 was switched in 0 times in 1.5 s (2.2M loops) while a
    control on contested CPU 2 was switched in 30 times (0.5M loops), and a
    task allowed on {2,3} never ran on 3 (`nk/tests/run-dedicated-x86.py`,
    2026-11-25). Counted in scheduler events, since TCG's own vCPU slicing
    makes timestamp gaps meaningless. Real hardware and interrupt routing away
    from the core are not verified.
- [ ] Executable memory for translated code (W^X handling, icache is coherent
      on x86), and fault handling the engine can hook (guest page faults, MMIO
      traps). Done when: a page fault in translated code reaches an engine
      callback.
  - [x] Trusted boot-time probe owns 4 KiB code frames, copies while RW/NX,
        seals RX/supervisor-only, and restores RW/NX before freeing. Runs
        before APs/processes exist; this is not a concurrent code cache
        (`nk/core/src/stela_nk.rs`, 2026-11-25).
  - [x] Native fault -> engine callback in QEMU: the IDT captures owned native
        data faults, GPRs and XMM8-15, returns through the saved invocation
        stack, and lets the engine recover/complete device accesses outside
        the handler. UART read/three writes take four real page faults; a
        missing device becomes an ARM data abort with original ELR/FAR and an
        unchanged load destination. IRQ/ordinary trap handling still passes
        (`nk/tests/run-stela-x86.py`, 2026-11-25; uncommitted).
        This is a private IF-masked pre-SMP fixture, not isolated guest RAM,
        full shadow paging or a general native fault/spill-recovery platform.
- [x] High-resolution timers and a clean way to inject guest interrupts (x86).
  - [x] One-shot high-resolution timer per dedicated core (`core/src/hrtimer.rs`):
        the core's LAPIC timer becomes its task's, TSC-deadline when CPUID has
        it, count mode otherwise; callback in interrupt context (vector 0xee).
        QEMU: 400 one-shots of 50 us-5 ms all fire; the LAPIC rate measured on
        the core matches calibration (1.0038 vs 1.0002 GHz); lateness stays
        under QEMU-on-macOS's own ~1.28 ms delivery step, against the 10 ms
        tick this replaces (`nk/tests/run-dedicated-x86.py`, 2026-11-25).
        True microsecond precision needs real hardware or a host without a
        millisecond-granular event loop; QEMU's TCG exposes no TSC-deadline.
  - [x] Guest timer interrupt injection: the engine's `Host` timer methods
        on nk (`core/src/stela_nk/host.rs`) arm `hrtimer::arm_at` at the guest
        generic timer's deadline (24 MHz guest counter on the TSC), the
        callback marks the FIQ pending, and guest WFI sleeps in `sti; hlt`
        until it fires. QEMU: a translated M2 guest on dedicated CPU 3 arms
        CNTV, executes WFI, takes the FIQ once, and returns with 1 host wait,
        1 hrtimer fire, 2.4-3.1 ms after its 2 ms deadline under TCG
        (`NK_STELA_TIMER_TEST`, `run-dedicated-x86.py --require
        STELA_TIMER_PASS`, 2026-11-25). Before SMP (no claimed hrtimer) the
        host still spins on the counter.
  - [x] Host device interrupts into a guest through the AIC model
        (`core/src/stela_nk/host/irq.rs`): a line nk raises from another CPU
        feeds `stela_soc::aic::Aic2` input 32, whose IRQ output is the guest
        PE's IRQ pin (`Devices::lines`); an IPI (vector 0xed) wakes the
        dedicated core. The guest enables and unmasks the AIC, asks for an
        interrupt, waits in WFI, claims event 0x10020, acks the device and
        unmasks: one IRQ, the core halted once and was kicked once, ~2.5 ms
        raise-to-ack under TCG, 3/3 runs (`NK_STELA_IRQ_TEST`,
        `run-dedicated-x86.py --require STELA_IRQ_PASS`, 2026-11-25). A
        fixture device and fixed addresses, not an ADT-mounted board.
  - [x] LKL's own timers at their deadlines (x86): one-shot deadlines are
        kept in TSC nanoseconds and CPU 0's LAPIC timer (free: the boot CPU
        ticks from the PIT) is claimed as an hrtimer armed for the earliest;
        the PIT tick still checks them as a backstop. Linux nanosleep of
        100 us-3 ms went from ~7-10 ms late to a 0.9-1.8 ms median under QEMU
        TCG, which is QEMU's own delivery grid (`NK_LKL_TIMER_TEST`,
        `run-dedicated-x86.py --require LKL_TIMER_PASS`, 2026-11-25). aarch64
        still uses the tick (CNTV_CVAL deadline not done).
- [ ] Decide the isolation model: translated guest code at ring 0 inside nk
      (like stela.efi), at ring 3, or in VMX non-root. Record it in AGENTS.md.

## Phase 4: Stela's translator inside nk

- [x] Build `stela-compiler`, `stela-engine`, `stela-contracts`,
      `stela-formats` for `x86_64-unknown-none` with `default-features = false`.
      `stela-boot` already does this for UEFI, so it should mostly work. Done
      when: they link into nk.
  - [x] All eight build for `x86_64-unknown-none` (contracts, formats,
        compiler, engine, soc-data, soc, loader, runtime). Needed: the engine's
        ring-0 cfg generalised from UEFI to `stela_ring0` (UEFI or none), and
        the soft crypto backends the UEFI build already uses
        (`--cfg aes_force_soft` etc. in RUSTFLAGS) (2026-11-25).
  - [x] `stela-compiler` links into nk (`--features stela`) and translates an
        arm64 block at boot: 8 bytes -> 47 bytes of x86-64 (2026-11-25).
  - [x] Execute trusted compiler blocks using the canonical block-entry thunk
        and nk-owned RX frames. Register/return state, taken/fallthrough
        branches, RAM load/store, NZCV and a NEON vector OR are checked before
        LKL and four-CPU userland epoll boot (`nk/tests/run-stela-x86.py`,
        2026-11-25; uncommitted).
  - [x] Engine, contracts and formats link with no default features and soft
        crypto cfgs in `nk/.cargo/config.toml`. A private nk Host supplies
        bounded MMU-off RAM, owned RX code, idle W^X patching and pinned native
        entry. A real `Machine` runs a loop, reads CurrentEL, writes/reads
        TPIDR_EL1, stores 21 to RAM and reaches its breakpoint: 4 native entries,
        2 patches. LKL/epoll and dynamic Python/eight-thread regression probes
        run afterward (`nk/tests/run-stela-x86.py`, 2026-11-25; uncommitted).
        The probe needs its own 1 MiB stack: measured 209,320 bytes touched,
        overflowing the ordinary 64 KiB boot stack before this fix.
  - [x] Link `stela-soc` and its UART model; engine-driven MMIO and guest abort
        recovery pass in the bounded pre-SMP fixture (2026-11-25; uncommitted).
  - [x] Bounded MMU-on shadow mappings and TLBI/permission/unmap checks pass
        (`nk/core/src/stela_nk/host/shadow.rs`, 2026-11-25; uncommitted).
  - [x] Bounded TTBR/ASID switching: nine guest turns, eight actual ARM MSR
        TTBR0 writes, different non-global code/data at identical VAs, and
        ASID reuse with explicit TLBI. Results stay separate (46 and 140);
        guest register state and kernel CR3 match after every turn. One root
        is eagerly rebuilt; retained per-ASID roots/scoped invalidation are
        not verified (`nk/tests/run-stela-x86.py`, 2026-11-25; uncommitted).
  - [x] MMU-on Device-nGnRnE UART: guest VA differs from UART PA; native faults
        are walked and completed at the device PA, with exact output, one
        read/three writes, vector recovery, and no native RAM mapping of MMIO
        (`nk/tests/run-stela-x86.py`, 2026-11-25; uncommitted).
  - [ ] Link the runtime/full board, then implement general shadow paging,
        retained ASID roots/scoped invalidation, general fault/spill recovery,
        guest timers and a persistent engine Host.
- [ ] A host trait between `stela-runtime` and its host (memory, timers, I/O,
      logging), with nk as one implementation and stela.efi as the other.
- [ ] Run translated arm64 blocks inside nk (the equivalent of `stela-dev
      run-blocks`). Done when: a known block sequence gives the same final
      state as on Stela.
  - [x] Bounded boot execution checks against architectural expected values
        pass; no hosted Stela differential comparison yet. Only trusted,
        fixtures at ring 0 before SMP, including recovered device faults;
        not a macOS guest.
- [ ] Pointer authentication, AMX, NEON/SVE paths verified on nk the same way.

## Phase 5: the M2 machine and macOS 27's early boot

- [ ] `stela-soc` + `stela-soc-data` (the T8112 device models) running on nk.
  - [x] The existing S5L UART model runs through an engine `Devices` adapter:
        translated ARM64 reads UTRSTAT and transmits exactly `NK\n` via three
        UTXH writes, with vector state preserved across native faults.
        Unsupported access widths refuse. Synthetic fixture address, polled
        console only: no ADT/full-board mount yet; the AIC2 model itself now
        delivers a host device IRQ on nk (Phase 3)
        (`nk/tests/run-stela-x86.py`, 2026-11-25; uncommitted).
        The UART also passes through a distinct virtual mapping with the ARM
        MMU enabled; the full ADT-described board remains open.
  - [x] The board's first devices mounted from a DeviceTree on nk
        (`core/src/stela_nk/host/board.rs`): `stela_soc_data::board::board`
        parses an M2-shaped tree (chip 0x8112) and its UART (0x235200000,
        AIC input 739), watchdog and AICv2 (0x23b0c0000, MAX_IRQ 0x1000) are
        placed on `stela_soc::machine::AppleDevices` as `stela-runtime`'s
        mount places them. nk types `nk\n` into the board's console from
        CPU 2 and kicks CPU 3; the UART raises its RX-timeout interrupt, the
        AIC delivers event 0x102e3, and the guest echoes `NK\n` uppercased
        through UTXH. 3/3 runs (`NK_STELA_BOARD_TEST`, `run-dedicated-x86.py
        --require STELA_BOARD_PASS`, 2026-11-25). The tree is stela-soc-data's
        fixture, not a real ADT read from disk, and only these three devices
        are placed: no PMGR, DARTs, storage or memory controllers yet.
  - [x] A real M2 ADT from disk: `stela-dev boot prepare --ipsw
        UniversalMac_27.2.1_26B131_Restore.ipsw --model Mac14,2` (Apple's
        IPSW for the MacBook Air M2, from mesu.apple.com) writes the bundle's
        `devicetree.adt`; Linux on nk reads it from its filesystem through
        LKL (openat/read) and the same three devices are mounted from it:
        8 processors, AIC with 1152 inputs, UART at 0x235200000 input 739.
        The guest's UART-interrupt echo passes against it, 3/3 runs
        (`NK_STELA_ADT_TEST`, `run-dedicated-x86.py --add
        <bundle>/devicetree.adt:stele/devicetree.adt --require
        STELA_ADT_PASS`, 2026-11-25). The file comes in the initrd, not yet
        from a disk partition; `stela-dev boot check` of the same bundle on
        the host stops at the MCC/DCS/PTD mount (needs native memory).
- [ ] `stela-loader`: load the boot bundle and kernelcache that `stela-cli`
      prepares for macOS 27 from a disk LKL reads.
- [ ] Hand off to the macOS 27 kernel. Done when: XNU's first serial output
      matches the Phase 0 reference.
- [ ] Get as far as the root filesystem mount on the reference timeline.

## Phase 6: device bridges (fake Apple device → Linux)

Each bridge replaces one Stela driver class with LKL. Order follows what macOS
needs to reach a login screen.

- [ ] Storage: the guest's Apple NVMe/ANS → an LKL block device (the macOS
      partition). Done when: macOS mounts its APFS volumes.
- [ ] Display, basic: guest framebuffer → LKL DRM/KMS on the real GPU. Done
      when: the macOS boot screen shows.
- [ ] Input: guest USB keyboard and mouse → LKL evdev.
- [ ] Network: guest Ethernet → an LKL netdev (tap or raw).
- [ ] Audio: the guest USB speaker → ALSA via LKL (Stele's audio-bridge mod is
      the same idea on Stela's own driver).
- [ ] USB passthrough for other devices (optional).

## Phase 7: GPU and Metal

The largest single piece, and the one that makes the desktop usable.

- [ ] Choose the kernel side: Stela's own nouveau port (`stela-driver-gpu`),
      or Linux nouveau with GSP firmware inside LKL, reached through DRM.
- [ ] Bring `stela-mesa` (NVK, NIR, NAK) and the Metal device (AIR → NIR)
      onto nk, on top of that kernel side.
- [ ] `stela-gpu-maintenance` installs the guest GPU components into macOS 27.
- [ ] WindowServer composites with Metal. Done when: the desktop is GPU-drawn.

## Phase 8: macOS 27 installed and usable

- [ ] Install flow: `stela-cli` prepares the USB stick with nk as the host
      instead of stela.efi, and installs macOS 27 to the internal disk.
- [ ] Setup Assistant completes; log in; Finder, Safari, Terminal work.
- [ ] Sleep/wake, shutdown and restart through ACPI (FADT).
- [ ] A macOS 27.x point update installs and boots.
- [ ] Performance compared with Stela on the same PC (boot time, a CPU
      benchmark, disk and network throughput).

## Phase 9: hardening and release

- [ ] Long-running stability: a 24-hour idle and a 4-hour load run without a
      host stall.
- [ ] Crash capture: nk panics and LKL oopses saved to disk and readable with
      `stela log`.
- [ ] Hardware matrix: two Intel and two AMD PCs, NVIDIA GTX 16 / RTX 20–50.
- [ ] Release build and a README section for installing Stele-on-nk.
- [ ] Keep upstream Stela merges working throughout (see AGENTS.md).

---

Notes:

- Running macOS on non-Apple hardware is outside Apple's software licence;
  Stela's README already makes users responsible for that. Keep that wording.
- Phase 2 is useful on its own (NETHOS on nk on real hardware), so it's a safe
  investment even before the macOS work starts paying off.

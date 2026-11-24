# Stele

Guidance for agents working on Stele and the kernel it is meant to run on.
Read all of it before changing anything: the project spans two repositories,
and the names in them are easy to confuse.

## What this project is trying to do

Run macOS (arm64) on ordinary x86-64 PCs, **with Linux's hardware support
instead of hand-written drivers.**

Stele is a fork of [Stela](https://github.com/tylertms/Stela) (via
[reece-builds/Stela-Mods](https://github.com/reece-builds/Stela-Mods)). Stela
is a UEFI boot program: an arm64 → x86-64 translator, a model of an Apple M2
Mac mini, a Metal driver for NVIDIA GPUs, and its own PC drivers. Its weak
point is the same as every Hackintosh-style project: every network card,
disk controller and sound chip needs a driver someone wrote by hand.

The plan is to keep Stela's translator and Apple hardware model and put them
on top of **nk**, a Rust kernel that runs **unmodified Linux drivers** through
LKL (the Linux Kernel Library). Each device class then needs only a small
bridge (fake Apple device → Linux interface), not a driver per chip:

```
macOS (arm64 guest)
  ↕ fake M2 hardware        stela-soc, stela-runtime
Translator                  stela-compiler, stela-engine
  ↕ device bridges          disk→block dev, net→netdev, audio→ALSA, GPU→DRM
nk (Rust): scheduler, memory, SMP, interrupts
  ↕ LKL: a whole Linux kernel as a library, unmodified drivers
Real PC
```

**Decided:** the host is **nk + LKL**, not a normal Linux kernel and not KVM.
Do not redirect the work to "run Stele as a Linux process" or "use KVM"
without the owner asking.

## The two repositories

| | Stele | NETHOS (contains nk) |
|---|---|---|
| GitHub | `NethercraftMC5608/Stele` | `NethercraftMC5608/nethos` (`Caleb22589/nethos` redirects) |
| Local | `/Users/mac/Stele` | `/Users/mac/Downloads/untitled folder/nethos` |
| Language | Rust workspace + vendored C (Linux, Mesa, LLVM, xnu submodules) | Rust `no_std` kernel in `kernel/`, Python/shell elsewhere |
| Licence | GPL-2.0-or-later | `kernel/core` separately licensed; `kernel/linux` GPL-2.0 |

nk was also imported into this repository as `nk/` (`c4cad58`, `207fbb4`);
continue host work there unless cross-repository changes are needed. It has
its own Rust workspace and stable toolchain; run its cargo commands in `nk/`.

Code may move from Stele into nk, but anything copied into `kernel/core`
makes that part GPL-2.0-or-later. Say so in the commit when you do it.

## Names that are easy to confuse

- **nk**: the Rust kernel in `nethos/kernel/`. Boots in QEMU (aarch64 `virt`
  under HVF and x86-64 `q35`). Not yet on real hardware.
- **LKL**: Linux built as a library inside nk (`kernel/lkl`, built by
  `kernel/ldk/ldk.py lkl`). It comes from `github.com/lkl/linux` and is
  **Linux 6.12**, not 7.2. Archives: `kernel/ldk/build/lkl{,-x86_64}/libnklkl.a`.
- **ldk single-driver ports**: `ldk.py build virtio-blk|virtio-net` compile
  individual drivers from **Linux 7.2** source (`DEFAULT_VERSION = "7.2"`).
- **The NETHOS kernel / Linux 7.2.0 + BORE**: a *normal* Linux kernel
  (`/Users/mac/builds/linux-7.2.0-x86_64.tar.gz`, release `kernel-7.2.0`)
  that the NETHOS installer and desktop use. It is **not nk** and does not
  contain it. The WebKit desktop on real x86 laptops ran on this kernel.

## Stele crate map (what to reuse)

Keep: the translator and the Mac model. Replace: the PC drivers, with Linux's.

- `stela-compiler`: AOT arm64 → x86-64 translation. Has a `std` build.
- `stela-engine`: code cache, dispatch, guest memory, system state. `std` build with `libc`.
- `stela-runtime`: board mounting, restore, host I/O for the translated Mac.
- `stela-soc`, `stela-soc-data`: register-level M2 (T8112) device models.
- `stela-loader`, `stela-formats`: Mach-O, restore images, boot bundles.
- `stela-driver-gpu`, `stela-mesa`: Metal device, AIR → NIR, NVK/NAK. The GPU path to keep.
- `stela-linux`: Stela's own shim for running unchanged Linux driver code. nk+LKL
  does this job; read it for ideas, don't build on both.
- `stela-drivers`, `stela-driver-{base,audio,net,storage}`: Stela's hand-written
  PC drivers. These are what LKL replaces.
- `stela-boot`, `stela-boot-core`, `stela-cli`: the UEFI program and installer.
  Keep them building: the fork still ships Stela's UEFI path.
- `stela-dev run-blocks`: runs translated code natively on a host. The only
  hosted runner today; a full guest boots only through `stela.efi`.

The full ordered checklist to macOS 27 is in [ROADMAP.md](ROADMAP.md). Tick
items there as they land, with the commit that did it.

## nk x86-64: where it stands

Branch **`nk-x86-userland`** (based on `smp-sleep-halt-fix`) in the NETHOS repo.
Working: PVH/Multiboot2 boot, 4 CPUs, LAPIC, scheduler torture, LKL boots,
ring 3, static busybox, `fork`/`vfork`/`execve`, signals with handlers and
`rt_sigreturn`, 309 syscalls translated by name (`syscall_x86_generic.rs`),
`stat` converted between layouts. The harness `/nk-init` script runs to the
end. Next gaps, in order:

`ls`, `find`, pipelines and file I/O work too (open flags translated). The
epoll probe now passes on nk + LKL with four active QEMU CPUs; see
`nk/tests/README.md`. x86 LKL already uses packed events, so no repacking.

1. Dynamic glibc/Python and `dlopen` now pass with the rebuilt x86 LKL archive
   (`nk/tests/run-dynamic-x86.py`). x86 file faults use the syscall trampoline
   and preserve all general registers plus FP/SIMD state.
2. Eight pthread-backed Python threads pass with `--threads`: distinct glibc
   errno/TLS pointers, timed waits, lock contention and joins on four QEMU CPUs.
   Signal return ordering and pre-handler mask
   restoration now pass on aarch64 and x86 (`nk/tests/run-signal-return.py`).
3. ACPI/MADT/MCFG discovery and read-only PCI inventory now work in QEMU
   (`nk/tests/run-platform-x86.py`), including sparse APIC IDs and a bridged
   NVMe. IOAPIC timer routing plus MSI/MSI-X device delivery now pass in
   QEMU (`nk/tests/run-pci-irq-x86.py`). Next: LKL's PCI host ops, DMA and
   Linux IRQ bindings, so real Linux drivers attach. FADT registers are
   discovered; power operations remain open.
4. Real hardware via GRUB (Multiboot2). Then VT-x, then the Stele translator on nk.

Build and boot (from `kernel/`):

```bash
NK_LKL_LIB="$PWD/ldk/build/lkl-x86_64/libnklkl.a" \
  NK_SMP=1 NK_AP_LAYER=4 \
  cargo build --release --target x86_64-unknown-none --target-dir target-hx
qemu-system-x86_64 -M q35 -m 4G -smp 4 \
  -kernel target-hx/x86_64-unknown-none/release/nk \
  -initrd /Users/mac/nk-x86-harness/desktop-x86.cpio \
  -display none -serial stdio -no-reboot
```

The x86 build needs `NK_LKL_LIB` (modules are cfg'd out without it). Rebuilding
LKL needs Docker (`colima start`), then `python3 ldk/ldk.py lkl --arch x86_64`.
On Apple silicon that cross-compiles natively (`x86_64-linux-gnu-`); a full
rebuild takes about 90 seconds with Colima at 12 CPUs / 16 GiB. Don't go back
to the emulated amd64 builder: ~20 minutes and random gcc segfaults.
`NK_SMP=1` also needs `NK_AP_LAYER=4`: layer zero does not initialize the
AP scheduler slots and panics on an invalid current-task index.
The old NETHOS cached x86 archive predates the retained-file ABI fix. The local
`nk/ldk/build/lkl-x86_64/libnklkl.a` was rebuilt and verified on 2026-11-24;
use it for dynamic glibc/Python. The reproducer and exact failures are
documented in `nk/tests/README.md`.
`python3 nk/ldk/lkl_abi.py <archive>` checks actual private syscall-table
relocations without Docker. New archive publication and the dynamic runner
require it to pass; both the rebuilt x86 archive and cached arm64 archive pass.

## x86 pitfalls already paid for

- **Ordinary translated blocks need the canonical entry thunk.**
  `stela_compiler::x86_64::block_entry::enter` saves the host stack in
  `GuestState::exit_rsp`; a plain function-pointer call leaves exits restoring
  RSP from zero. `nk/core/src/stela_nk.rs` exercises trusted finite blocks
  before SMP with owned RW/NX -> RX code pages. Its private `host.rs` now runs
  a real system-mode `Machine`, with bounded MMU-off RAM and idle code patching.
  System blocks need the pinned entry plus callee-saved host register
  preservation; ordinary block entry alone is insufficient. Native fault
  recovery and the existing S5L UART model now pass in the private fixture;
  general guest isolation and concurrent code remain open. A separate bounded
  MMU-on fixture now uses ARM tables and a private x86 shadow root.
- **Native fault recovery is invocation-owned.** The pre-SMP probe runs after
  IDT/TSS setup with IF masked. Only synchronous faults whose RIP belongs to
  the active immutable native images are captured; other faults use nk's
  normal path. `isr_common` passes its FXSAVE image so recovery captures XMM8-15
  before Rust can change them. No engine/LKL work runs inside the handler.
  Full spill/extended-register recovery, general shadow paging, guest concurrency
  and arbitrary guest code remain unsupported. A missing device must raise an ARM
  abort, never return a fabricated value.
- **Shadow leaves borrow their backing.** `stela_nk/host/shadow.rs` uses a
  private low 2 MiB aperture, separate guest physical RAM and Stela's ARM
  walker. It eagerly rebuilds mappings outside native entry; no table walks
  or allocations happen in the trap. Supervisor/NX x86 leaves enforce guest
  data write permissions. CR3 is restored before fault classification; the
  fresh ARM walk distinguishes permission/translation faults from device PAs.
  Before destroying the root, remove borrowed leaves without freeing payload
  frames, then let the RAM owner free them. Guest TLBI permission and unmap
  tests pass. Nine address-space turns now verify guest MSR TTBR0 switches,
  distinct code/data at the same VAs, and ASID reuse with explicit TLBI.
  The host still eagerly rebuilds one root: retained per-ASID roots, scoped
  invalidation, general demand fills and isolation remain open. MMU-on UART
  accesses also translate VA to device PA without mapping a RAM leaf.
- **Guest physical RAM need not be contiguous on the host.** The private
  `stela_nk/ram.rs` owner reserves a checked guest range and commits zeroed
  4 KiB frames as needed before native entry. Uncommitted reads return zero;
  cross-page copies split at backing boundaries. Raw host pointers cover only
  one resident page. The shadow fixture reserves 4 GiB, with data near its
  6 GiB upper end backed by an nk frame below 4 GiB. Model DMA ports use the
  same checked memory interface, including exact partial-transfer progress.
  This remains pre-SMP: no production memory service, real-device DMA/DART,
  retained ASID roots or general native demand filling is implied.
- **Canonical RAM handoff consumes the sparse builder.** Its unsafe
  `into_registered` path transfers committed frames to the engine's real
  `RegisteredSystemRam::register` at CPL0 with IF masked. No payload is copied;
  an immutable keep-alive reclaims frames after the last backing drops.
  Uncommitted holes stay unregistered. Retire all shadow/direct-writer aliases
  before handoff, and use only canonical physical/service APIs afterward.
  The separate probe verifies publication, descriptor CAS, refusal across
  holes, service exclusion, permanent owner failure and reclamation. Existing
  translated fixtures still use their older private host; they do not acquire
  canonical native-execution permission. Keep everything before SMP until the
  frame allocator and persistent guest ownership are made safe for it.
- **Set CR0.NE during x86 boot.** PVH left CR0 at `0x80010013`, so the engine's
  canonical native-target admission correctly refused. BSP and AP setup now
  enable native x87 exceptions (#MF) rather than the external FERR# route.
  Do not weaken the engine's control checks to accommodate a missing boot bit.
- **The engine probe needs a dedicated stack.** A measured 209,320-byte stack
  footprint corrupted LKL statics below nk's 64 KiB boot stack, despite the
  engine fixture itself passing. It now runs on an owned 1 MiB pre-SMP stack,
  checks its bottom canary and reports touched bytes before freeing it.
- **Dynamic tests must provision entropy.** Python/urandom stalled in Linux's
  `try_to_generate_entropy` without a seeded CRNG. The dynamic runner now
  supplies 32 fresh host-CSPRNG bytes to its test image; `seed-entropy-x86.c`
  credits them with RNDADDENTROPY and requires nonblocking getrandom to fill
  a buffer before Python starts. This is test provisioning, not a production
  RNG provider; do not substitute fixed/time-derived seeds or insecure flags.
- **Shared code assumes aarch64 numbers and layouts.** Clone was dispatched on
  `220` (aarch64) instead of `nr::CLONE` (56 on x86). Clone's last two args are
  swapped between ABIs. Asm-generic structs (`stat`, `epoll_event`) differ from
  x86's. When something works on aarch64 and fails on x86, look here first.
- **LKL uses arm64's open flags** (`patch-lkl.py: arm64_open_flags`), so x86
  `O_DIRECTORY`/`O_DIRECT`/`O_LARGEFILE`/`O_NOFOLLOW` are translated in
  `x86_legacy` (`open_flags_in`/`_out`). Any new call taking open flags needs it.
- **epoll layout follows the build compiler too.** Linux's UAPI selects a
  packed 12-byte event under `__x86_64__`, including x86 LKL. Repacking it as
  an asm-generic 16-byte event corrupts data and multi-event strides. Use
  `nk/tests/run-epoll-x86.py` to exercise all three wait variants.
- **Linux PCI drivers go through `lkl_pci_x86.rs`.** LKL hosts exactly one
  PCI device, named by `lkl_pci=` (nk picks the first NVMe). Config writes go
  at native width (`Hardware::write8/16/32`) because RMW would clear RW1C
  status bits. BARs above 4 GiB are mapped in the kernel MMIO window at
  12 GiB, whose directory must exist before the first process. LKL's PCI
  host is x86-only (`lkl/nk-x86_64.config`): on aarch64 it would route
  virtio-mmio DMA through `lkl_dma_ops`.
- **LKL on x86 is built with `-mno-red-zone`** (`ldk.py`, KCFLAGS and the
  nk-host.c compile). Linux runs at ring 0 under nk, where interrupts push
  onto the current stack; with the userspace red zone, device interrupts
  overwrote driver locals. Rebuilding the archive is required after pulling.
- **The x86 clock is the TSC** (`timer::time_ns`, `nk_time_ns`). Syscalls run
  with IF masked, so a tick-based clock froze and LKL's `udelay` spun forever.
- **Signals Linux raises itself** (itimer SIGALRM, SIGPIPE) are moved to nk on
  an interrupted return (`user::lkl_interrupted`, draining with
  `rt_sigtimedwait`), and caught dispositions are mirrored into LKL
  (`signal::mirror_to_lkl`) so Linux doesn't take the default action. x86
  only so far; aarch64 has the same gap.
- **Real PCs boot nk through GRUB (Multiboot2) on UEFI.** `tests/run-uefi-x86.py`
  builds the same disk image a USB stick needs and boots it under OVMF; use it
  for anything touching boot info, ACPI discovery or the console. The screen
  console (`fbcon.rs`) mirrors serial output; GRUB calls GOP's 32-bit pixels
  24 bpp.
- **CPU 0 belongs to Linux; dedicated APs belong to one task.** While LKL is
  active, anything that may run on CPU 0 runs only there; APs run only tasks
  that exclude CPU 0. `sched::dedicate_cpu` gives an AP to one task (guest
  vCPUs) and masks its tick. Never give a Linux-ABI task an affinity without
  CPU 0.
- **Guest WFI sleeps on the dedicated core's hrtimer.** `BootHost::arm_timer`
  arms `hrtimer::arm_at`; `wait_for_interrupt` opens interrupts only across
  `sti; hlt; cli`, because `fault::invoke` needs IF masked while guest code
  runs. Without a claimed hrtimer (pre-SMP probes) it spins on the counter.
- **The kernel links at 1 GiB on x86** (`linker-x86_64.ld`), because static
  binaries load at 0x400000 and every process owns the low gigabyte. Each
  address space has private copies of the top three page-table levels, exactly
  like aarch64. Don't map anything of the kernel's below 1 GiB.
- **LKL CPU state is linker-resolved.** `core/build.rs` and `lkl_archive.rs`
  copy the input archive into OUT_DIR, export/namespace its BSS `cpu` as
  `nk_lkl_cpu`, and leave the source archive untouched. Rust references that
  symbol directly and validates its live layout after LKL initializes it.
  Keep this path; do not reintroduce hardcoded addresses or memory scans.
- **Entry asm restores registers before using them as indices.** Read the
  per-CPU slot first, then restore user registers (`syscall_entry`). In
  `resume_user`, `r10` carries the user rsp until the iret frame is pushed.
- **TLS goes in its own pages at the program break**, never over the PT_TLS
  template, which lives inside a PT_LOAD.
- **Inline asm must preserve full 64-bit registers.** `cpu_id()` swapped rbx
  through 32-bit registers around CPUID and zeroed rbx's upper half, so a
  syscall result held in rbx came back as `0xffffffef`. A syscall that "returns
  a positive number but should have failed" smells like this.
- **APIC IDs are hardware identifiers, not scheduler indices.** MADT can have
  gaps (QEMU with six CPUs/two sockets uses 0,1,2,4,5,6). `lapic` maps them
  to dense logical slots for `cpu_id`, AP stacks and outgoing IPIs. Never
  reintroduce an APIC-ID mask or stop probing at the first numeric gap.
- **Interrupts must not enter LKL directly.** Device vectors use bounded
  callbacks to acknowledge hardware and `lklirq::raise` to defer Linux work.
  `lkl_trigger_irq` can block and belongs only in the IRQ pump. The BSP's PIT
  uses IOAPIC vector 0x40 when MADT routing is usable; AP timers stay at 0xef.
- **PCI Command and Status share a dword.** Use native 16-bit Command writes:
  reflecting the Status half of a read/modify/write clears W1C bits. MSI is
  itself a bus-master write; QEMU interrupt probes temporarily enable BME
  and restore it, while leaving packet/disk DMA engines stopped.
- **Signals on x86:** the restorer goes on the stack at `[rsp]`, arguments in
  rdi (`x[13]`), rsi, rdx, and the frame skips the 128-byte red zone.
- **Save completed returns and interrupted masks.** Set the syscall result
  before building a signal frame (aarch64 needs this too), and save the mask
  from before adding the handler/self-block bits. Restoring the temporary
  mask made only the first signal work. Run `run-signal-return.py` on both
  architectures after signal changes.
- **Debug by measurement.** lldb can attach to QEMU's gdbstub (`-s -S`,
  `gdb-remote 1234`); write breakpoint callbacks to a file, since batch output
  is lost when the process is killed. QEMU's `-d int -D file` gives the real faulting
  RIP and CR2. A temporary syscall trace in `rust_syscall` found the fork bug
  in one run. Remove debug code before committing.

## Working rules

- The NETHOS checkout is shared with other agents. Read its `AGENTS.md` and use
  `tools/crew/crew` claims before editing there. Prefer a git worktree for nk work.
- Commit only when asked. Small commits, one cause each, with the evidence
  (what failed, what was measured) in the message.
- Stele keeps up with upstream Stela and Stela-Mods; keep changes in separate
  crates or clearly marked so merges stay possible.
- Don't overstate status. "Boots in QEMU" is not "runs on hardware"; say which
  kernel (nk vs Linux 7.2) a result came from.

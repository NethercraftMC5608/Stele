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

## nk x86-64: where it stands

Branch **`nk-x86-userland`** (based on `smp-sleep-halt-fix`) in the NETHOS repo.
Working: PVH/Multiboot2 boot, 4 CPUs, LAPIC, scheduler torture, LKL boots,
ring 3, static busybox, `fork`/`vfork`/`execve`, signals with handlers and
`rt_sigreturn`, 309 syscalls translated by name (`syscall_x86_generic.rs`),
`stat` converted between layouts. The harness `/nk-init` script runs to the
end. Next gaps, in order:

`ls`, `find`, pipelines and file I/O work too (open flags translated); epoll
records are repacked but no test program has exercised epoll yet.

1. Test epoll and dynamically linked programs (the harness is static busybox).
2. The same syscall-return-before-signal ordering fix on aarch64 (done on x86).
3. ACPI (MADT, MCFG), PCI enumeration, MSI-X, then LKL's PCI host ops, so
   real drivers (NVMe, igc/e1000e, r8169, snd_hda_intel) attach.
4. Real hardware via GRUB (Multiboot2). Then VT-x, then the Stele translator on nk.

Build and boot (from `kernel/`):

```bash
NK_LKL_LIB="$PWD/ldk/build/lkl-x86_64/libnklkl.a" \
  cargo build --release --target x86_64-unknown-none --target-dir target-hx
qemu-system-x86_64 -M q35 -m 4G -smp 4 \
  -kernel target-hx/x86_64-unknown-none/release/nk \
  -initrd /Users/mac/nk-x86-harness/desktop-x86.cpio \
  -display none -serial stdio -no-reboot
```

The x86 build needs `NK_LKL_LIB` (modules are cfg'd out without it). Rebuilding
LKL needs Docker (`colima start`), then `python3 ldk/ldk.py lkl --arch x86_64`.

## x86 pitfalls already paid for

- **Shared code assumes aarch64 numbers and layouts.** Clone was dispatched on
  `220` (aarch64) instead of `nr::CLONE` (56 on x86). Clone's last two args are
  swapped between ABIs. Asm-generic structs (`stat`, `epoll_event`) differ from
  x86's. When something works on aarch64 and fails on x86, look here first.
- **LKL uses arm64's open flags** (`patch-lkl.py: arm64_open_flags`), so x86
  `O_DIRECTORY`/`O_DIRECT`/`O_LARGEFILE`/`O_NOFOLLOW` are translated in
  `x86_legacy` (`open_flags_in`/`_out`). Any new call taking open flags needs it.
- **The kernel links at 1 GiB on x86** (`linker-x86_64.ld`), because static
  binaries load at 0x400000 and every process owns the low gigabyte. Each
  address space has private copies of the top three page-table levels, exactly
  like aarch64. Don't map anything of the kernel's below 1 GiB.
- **`LKL_CPU_SEED`** in `main.rs` is a hardcoded address of LKL's `cpu` symbol.
  Any relink can move it; the boot log says `was stale ... update LKL_CPU_SEED`.
  Get the new value with `nm <nk> | grep ' b cpu$'`.
- **Entry asm restores registers before using them as indices.** Read the
  per-CPU slot first, then restore user registers (`syscall_entry`). In
  `resume_user`, `r10` carries the user rsp until the iret frame is pushed.
- **TLS goes in its own pages at the program break**, never over the PT_TLS
  template, which lives inside a PT_LOAD.
- **Inline asm must preserve full 64-bit registers.** `cpu_id()` swapped rbx
  through 32-bit registers around CPUID and zeroed rbx's upper half, so a
  syscall result held in rbx came back as `0xffffffef`. A syscall that "returns
  a positive number but should have failed" smells like this.
- **Signals on x86:** the restorer goes on the stack at `[rsp]`, arguments in
  rdi (`x[13]`), rsi, rdx, and the frame skips the 128-byte red zone.
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

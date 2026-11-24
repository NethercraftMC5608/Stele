# nk x86-64 userland probes

Run these from `nk/`. They boot nk + LKL in QEMU q35, not the NETHOS
Linux desktop kernel. No physical hardware result is implied.

## epoll

Requirements: stable Rust with `x86_64-unknown-none`, an x86-64 LKL archive,
Clang that emits ELF x86-64 objects, Rust's bundled `rust-lld`, Python 3,
and `qemu-system-x86_64`. No Linux libc, sysroot, Docker daemon or rootfs is
needed to build the probe. Building the LKL archive itself still needs Docker.

```sh
NK_SMP=1 NK_AP_LAYER=4 NK_LKL_LIB=/absolute/path/to/libnklkl.a \
  cargo build --release --target x86_64-unknown-none --target-dir target-hx
python3 tests/run-epoll-x86.py \
  --kernel target-hx/x86_64-unknown-none/release/nk
```

`NK_AP_LAYER=4` initializes each AP's scheduler slot before it schedules.
`NK_SMP=1` alone leaves the bring-up layer at zero and panics on an invalid
current-task index. Omitting both variables builds a single active CPU.

The runner builds `init/epoll-x86.c` as a static Linux ELF at 4 MiB and packs
it into a minimal newc initrd as `/nk-init`. It captures serial output in
`target-epoll/serial.log`, stops QEMU after success/failure or a 60-second
timeout, and returns nonzero on failure. `--output`, `--clang`, `--linker`,
`--qemu` and `--timeout` override those defaults. Generated files stay in an
ignored target directory.

The probe checks two eventfds through raw `epoll_wait`, `epoll_pwait` with
an explicit empty mask, and `epoll_pwait2` with a zero timespec. It verifies
EPOLLIN, distinct fd identifiers in full-width 64-bit data, consecutive
12-byte records, buffer guards, ADD/MOD/DEL, draining and empty waits, bad
user pointers, invalid signal-mask size, zero maxevents, and fd cleanup.
It does not verify signal interruption or blocking wakeups from another thread.

Success requires all three `EPOLL_*_OK` markers and `EPOLL_PASS`.

### Regression measured on 2026-11-24

Before the fix, the initial two-event wait returned data
`0x0000000100000004` instead of the registered 64-bit tag and the probe
reported `EPOLL_FAIL 64-bit data/fd`. The x86 LKL build uses the packed
12-byte layout selected by Linux's `__x86_64__` UAPI, so nk's conversion
to/from 16-byte records corrupted both the data offset and event stride.
Passing the user records directly through LKL fixes it. All checks pass
with both a single active CPU and four active CPUs (`NK_AP_LAYER=4`).

## ACPI and PCI discovery

Build with the same x86 LKL/SMP command above, then run:

```sh
mkdir -p target-platform
rustc --edition=2021 --test tests/platform-x86.rs -o target-platform/parser-tests
target-platform/parser-tests
python3 tests/run-platform-x86.py \
  --kernel target-hx/x86_64-unknown-none/release/nk
```

The host suite compiles nk's actual ACPI, PCI and Multiboot2 parsers. It
checks table bounds/checksums, RSDT fallback, disabled CPUs, local APIC
address overrides, x2APIC records, malformed MADT entries, overlapping MCFG
windows, nonzero-start ECAM addressing, multiple PCI segments, multifunction
devices, bridge headers, 64-bit BARs, and malformed GRUB tags.

The QEMU suite runs the ring-3 epoll probe in four configurations: default
q35 with NVMe behind a PCIe root port, six CPUs with sparse APIC IDs
`0,1,2,4,5,6`, eight CPUs, and `q35,acpi=off` (BSP-only/CF8/CFC fallback).
It requires the expected firmware/device/CPU markers as well as EPOLL_PASS.
The NVMe image is a generated, read-only, empty test disk. No real disk is
attached. Serial logs live in each case's output directory.

ACPI table reads and PCI inventory run before APs start. The LAPIC and ECAM
ranges must fit the initial 4 GiB identity map, be above the low user arena,
and not overlap usable RAM at 2 MiB granularity. Higher ECAM windows are
reported and skipped; legacy configuration can still scan segment zero.
The host currently supports at most eight xAPIC CPUs; x2APIC-mode hardware
is explicitly refused. IOAPIC/ISO records now route the BSP's PIT as described
below. FADT records remain metadata: power operations/AML and real-PC testing
remain open.
PCI inventory does not size or assign BARs, enable bus mastering, configure
interrupts on ordinary devices, provide LKL PCI host ops, or attach any Linux
device driver. The explicit IRQ test build takes temporary ownership of its
QEMU fixtures only.

## IOAPIC, PCI MSI and MSI-X

Normal x86 builds map the MADT IOAPIC register pages uncached, validate GSI
ranges/ISA override flags, and route PIT IRQ0 to the advertised GSI (q35:
GSI 2, vector 0x40). PIC IRQ0 is masked during handover so the global clock
does not receive duplicate ticks. AP local timers and IPIs keep their own
vectors. Without ACPI/IOAPIC, the PIC timer fallback still works.

The PCI interrupt test build additionally requires QEMU EDU and its default
82574/e1000e NIC. Build and run it separately:

```sh
NK_SMP=1 NK_AP_LAYER=4 NK_PCI_IRQ_TEST=1 \
  NK_LKL_LIB=/absolute/path/to/libnklkl.a \
  cargo build --offline --release --target x86_64-unknown-none \
  --target-dir target-pci-irq-build
python3 tests/run-pci-irq-x86.py \
  --kernel target-pci-irq-build/x86_64-unknown-none/release/nk
```

This suite runs q35/ECAM, six sparse-ID CPUs, and ACPI-disabled/CF8/PIC cases.
Each boot must produce `PCI_MSI_PASS` from EDU, `PCI_MSIX_PASS` from the
82574, continuing timer ticks, and the ring-3 `EPOLL_PASS`. Each device must
generate three hardware interrupts, acknowledged by its source handler.
The MSI-X NIC probe injects software interrupt causes, with RX/TX engines
stopped and no packet rings allocated. It requires EDU's presence as well
as the NIC's ID and is gated by `NK_PCI_IRQ_TEST`, not enabled in normal
builds. No real disks or NICs are attached to the QEMU tests.

The first MSI probe failed with BME clear: MSI itself is a bus-master memory
write. The test now temporarily permits bus mastering, never starts a DMA
engine, and restores the original Command/capability/table/source state.
The MSI-X probe initially received interrupts with empty ICR cause bits:
EIAC was auto-clearing RXQ0 before the handler read it. The probe now disables
auto-clear so its handler explicitly verifies and acknowledges every cause.
Both probes pass with the final configuration.

Native-width PCI writes preserve the W1C Status register. Capability traversal
rejects cycles, malformed pointers and truncation. MSI and MSI-X setup reject
an already active/conflicting mode, snapshot state, and attempt rollback on
failed writes. MSI-X programming masks the function and entry while updating,
flushes table writes before enabling, and checks table spans against caller
supplied BAR bounds. Host tests cover those cases, exact restoration,
non-target entries, route encoding, vector exhaustion and retirement.

Device vectors occupy only `0x50..0xdf`; callbacks acknowledge their device
before LAPIC EOI. Linux IRQ notifications use the existing deferred IRQ pump,
never a blocking LKL call in a trap. That hook is implemented, but delivery
to an actual attached Linux driver's ISR remains unverified. Vector retirement
does not reuse numbers while messages could remain in flight. LKL PCI host
ops, DMA translation, PCI INTx `_PRT` routing, level-triggered IRQ mask/unmask
coordination, full hot-unplug and real-hardware validation remain open.

## Dynamic glibc / Python and eight-thread probe

Given cached NETHOS amd64 `.npk` packages and a static Linux x86 busybox:

```sh
python3 tests/run-dynamic-x86.py \
  --kernel target-hx/x86_64-unknown-none/release/nk \
  --lkl-lib /absolute/path/to/libnklkl.a \
  --packages /path/to/cached/npk/repository \
  --busybox /path/to/static/x86_64/busybox
```

The runner builds an initrd containing glibc, Python 3.13 and its minimal
dependencies. It requires Python to print `1`, import ctypes, and dlopen
libm with a working `cos(0.0)` call. Selected package names and the complete
serial log are retained under `target-dynamic/`. Failure is a nonzero exit.
Before creating any initrd or starting QEMU, it checks the supplied archive
(which must be the one linked into that kernel) with `ldk/lkl_abi.py`.

Both the base probe and `--threads` pass on nk + rebuilt x86 LKL in q35
QEMU with four CPUs (2026-11-24). The thread variant keeps eight pthread-backed
Python threads alive at a barrier, checks eight distinct glibc errno pointers
and Python thread-local values over twenty timed waits per thread, increments
a locked counter to 160, and joins every thread. Use a separate output folder
with `--threads --output target-dynamic-threads` to retain both logs.
This covers clone, futex waits/wakes, TLS and ordinary thread exit/join;
robust-list/rseq return ENOSYS and cancellation/owner-death recovery are untested.

The minimal init script creates Linux's standard `/dev/urandom` character node
(1:9) for Python's startup fallback after nonblocking getrandom. This is not
an entropy-source test: nk's existing unseeded random policy is unchanged.

After the ABI rebuild, x86 first faulted inside the loader reading a retained
file mapping: its trap handler only populated anonymous pages. It now redirects
file faults through the mapped `syscall; ud2` trampoline, pages in from normal
syscall context, and returns with iret restoring every general register,
including rcx/r11. The syscall entry also preserves FP/SIMD state. Without
that preservation, ctypes imported a method named `get_errno\\x00ctypes.set_errno`
instead of `get_errno` after a page fault inside vectorized string handling;
the complete ctypes import and libm call now pass. The aarch64 trampoline
continues to use `svc; brk`.

The cached x86 LKL archive tested on 2026-11-24 has `exec_files` at 248,
file_pread at 249 and file_put at 250; file_get has no live syscall slot.
nk expects file_get/file_pread/file_put at 249/250/251. This is caused by
the patcher's first-build ordering: it used to insert 248/249/250 and only
correct them on a subsequent patch run. It now normalizes all three numbers
on the first run, emits valid C instead of literal backslash-t tokens, and
takes one file reference instead of leaking a second fget_raw reference.

The patcher's fresh/old/current-tree idempotency test is independent of Docker:

```sh
python3 -B -m unittest discover -s tests -p test_lkl_file_patch.py -v
```

The old NETHOS x86 archive must be rebuilt before the dynamic probe can pass:

```sh
python3 ldk/ldk.py lkl --arch x86_64
```

The host-side Docker rebuild completed, and the local
`ldk/build/lkl-x86_64/libnklkl.a` passes the ABI check and both dynamic probes.
This session still cannot access the Docker socket, so future LKL rebuilds
must run in the host Terminal. Kernel builds and QEMU probes run here.

## Exact LKL linkage and private ABI checks

LKL's file-local BSS `cpu` used to be located with architecture-specific
address guesses and an 8 MiB runtime sweep after every relink. The kernel
now resolves a real `nk_lkl_cpu` symbol. `core/lkl_archive.rs` uses Rust's
`llvm-nm` and `llvm-objcopy` tools to make a private archive in Cargo's
OUT_DIR: it verifies one BSS `cpu`, globalizes it, then namespaces it.
The source archive is never rewritten. Already-exported input is copied;
missing/ambiguous symbols and input/output aliasing are refused.

The final linker supplies the relocated address on either architecture.
`main.rs` validates the initialized semaphore/layout fields at that exact
address and reports `lkl cpu: linked address ... verified`; it never scans
unrelated kernel memory. Both x86 and aarch64 release builds boot with it.
If LLVM tools are missing, install `rustup component add llvm-tools`.

```sh
rustc --edition=2021 --test tests/lkl-archive.rs -o target-platform/lkl-archive-tests
target-platform/lkl-archive-tests
python3 -B -m unittest discover -s tests -p 'test_lkl_*.py' -v
python3 -B ldk/lkl_abi.py /path/to/libnklkl.a
```

The archive tests compile tiny x86 and arm64 ELF fixtures with Clang and
check that preparation preserves inputs, accepts exported symbols and
rejects ambiguous/missing symbols. The ABI checker parses regular ar and
ELF64 relocation records without dependencies or executing guest code. It
resolves syscall-table slots 248..251 to function symbols, including GCC's
section-symbol plus addend form. Merely finding the functions somewhere in
the object does not pass. Malformed/truncated objects and unsupported/thin
archives fail explicitly. Tests cover x86/aarch64, shifted slots, Unix/GNU/
BSD archive names, duplicate members and invalid pointer relocations.

The cached arm64 and rebuilt local x86 archives pass. The old NETHOS x86 archive reports 249=file_pread,
250=file_put, and 251=sys_ni_syscall. `ldk.py lkl` now validates a temporary
new archive before atomically publishing it, so an ABI failure preserves the
last archive. The dynamic runner also refuses that bad archive immediately,
before extraction/boot. The Linux rebuild must currently run in the host
Terminal because this session cannot connect to the Docker socket.

The imported LKL entry point now defers NETHOS's optional `npkg_elf` import
until a legacy individual-driver report needs it. LKL builds and CLI help
work without that separate package-manager tree. A missing per-architecture
Docker builder image is built locally from `ldk/Dockerfile` instead of being
pulled as an unpublished image name. `test_ldk_entrypoint.py` checks the CLI
and builder/publication flow without requiring Docker.

## Signal return ordering and mask restoration

`init/signal-return.c` is a static, libc-free Linux ABI probe compiled for
either architecture. It verifies successful self-kill returns zero after a
handler, repeated delivery, the handler's temporary mask, preservation of
an unrelated blocked signal, explicit blocking/queuing/unblocking, and
continued execution after `rt_sigreturn`.

```sh
NK_LKL_LIB=/path/to/arm64/libnklkl.a \
  cargo build --offline --release --target aarch64-unknown-none-softfloat \
  --target-dir target-lkl-link-arm
python3 tests/run-signal-return.py --arch aarch64 \
  --kernel target-lkl-link-arm/aarch64-unknown-none-softfloat/release/nk \
  --output target-signal-arm64
python3 tests/run-signal-return.py --arch x86_64 \
  --kernel target-hx/x86_64-unknown-none/release/nk \
  --output target-signal-x86
```

The ARM runner converts nk's ELF to its arm64 Linux-header image so QEMU
supplies a DTB, then boots virt/GICv3 with one Cortex-A72 and 2 GiB RAM.
With this kernel map, a 4 GiB ARM guest overlaps LKL's reserved 4 GiB virtual
window and is not the tested configuration. The x86 runner uses q35/four CPUs.
Each requires its `ARM_SIGNAL_PASS` or `X86_SIGNAL_PASS` serial marker.

Measured regression: arm64 originally failed the first self-kill's return
check because the signal context saved its PID argument. Saving the syscall
result first fixed that, exposing a second failure on both architectures:
the handler mask was saved after adding the self-block bit, so only the
first signal ran. Saving the interrupted mask fixes repeated delivery.
Both complete probes now pass. Nested handlers, full native-ABI signal
behavior and complete Linux signal ABI compatibility are not claimed.

## NVMe through LKL's PCI bridge (x86-64)

Linux's own `nvme` driver, attached through nk's `pci_ops`
(`core/src/lkl_pci_x86.rs`): nk chooses the first NVMe controller from its
inventory and passes it as `lkl_pci=`, maps the BAR (QEMU's 64-bit BAR at
768 GiB goes through the kernel MMIO window at 12 GiB), programs MSI-X for
Linux's IRQs, and serves DMA by identity. Needs an x86 archive built with
`lkl/nk-x86_64.config` (PCI, PCI_MSI, BLK_DEV_NVME, GPT).

```sh
python3 ldk/ldk.py lkl --arch x86_64
NK_LKL_LIB=$PWD/ldk/build/lkl-x86_64/libnklkl.a \
  cargo build --release --target x86_64-unknown-none --target-dir target-hx
python3 tests/run-nvme-x86.py --kernel target-hx/x86_64-unknown-none/release/nk
```

It writes a 16 MiB GPT disk, boots q35 with `-device nvme`, and requires the
bridge markers plus Linux's `nvme0n1: p1`, which needs config space, BAR
access, DMA and interrupt completion all working. `NVME_PASS` on success.
Verified by hand on 2026-11-24 as well: from busybox, `/dev/nvme0n1p1` exists,
sector 1 reads `EFI PART`, and 1 MiB writes and a full-partition read complete.

## Ethernet: e1000e, DHCP and ping (x86-64)

Linux's `e1000e` driver through the same bridge, on its own root bus beside
an NVMe disk (`lkl_pci=` takes a list since `patch-lkl.py`'s
`multi_pci_devices`). busybox `ifconfig eth0 up`, `udhcpc` against QEMU's
user-mode network, and `ping -c 3 10.0.2.2`.

```sh
python3 ldk/ldk.py lkl --arch x86_64        # e1000e, AF_PACKET, -mno-red-zone
NK_LKL_LIB=$PWD/ldk/build/lkl-x86_64/libnklkl.a \
  cargo build --release --target x86_64-unknown-none --target-dir target-hx
python3 tests/run-net-x86.py --kernel target-hx/x86_64-unknown-none/release/nk \
  --busybox /path/to/static/x86_64/busybox
```

`NET_PASS` needs the link up, a lease (`LEASE eth0 10.0.2.15`), three replies
and 0% loss. It found three x86 problems, all fixed in the same change:
the clock froze inside syscalls (udelay spun for ever), LKL was built with
the userspace red zone (interrupt frames overwrote driver locals), and a
signal Linux raised itself (ping's SIGALRM) took Linux's default action.
Verified 2026-11-25 with `NK_SMP=1 NK_AP_LAYER=4` and without.

## USB keyboard (xHCI + HID) and HD Audio (x86-64)

xHCI and HD Audio controllers are handed to Linux beside NVMe and Ethernet.
The runner boots `qemu-xhci` + `usb-kbd` and `intel-hda` + `hda-duplex`, finds
the keyboard's event node in `/proc/bus/input/devices`, types `a` through the
QEMU monitor, and requires an `EV_KEY`/`KEY_A`/press record read from that
node; it also requires `snd_hda_intel` to have registered a card (its codec
interrupts arrive by MSI).

```sh
python3 tests/run-usb-hda-x86.py --kernel target-hx/x86_64-unknown-none/release/nk \
  --busybox /path/to/static/x86_64/busybox
```

## Rebuilding the x86 LKL archive

On an arm64 Docker host (Colima on Apple silicon), `ldk.py lkl --arch x86_64`
cross-compiles natively with `x86_64-linux-gnu-` from the `nethos-ldk-cross-
amd64` image instead of running gcc under amd64 emulation. A full clean build
went from ~20 minutes plus random gcc segfaults to 86 seconds on a 12-CPU
Colima VM (2026-11-25). The first build after switching builders runs
`mrproper` once (`.nk-build-host`), because the tree's host tools are
binaries for the previous builder.

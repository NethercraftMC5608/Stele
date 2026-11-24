# nk x86-64 userland probes

Run these from `nk/`. They boot nk + LKL in QEMU q35, not the NETHOS
Linux desktop kernel. No physical hardware result is implied.

## Translated ARM64 execution on nk

```sh
NK_SMP=1 NK_AP_LAYER=4 NK_LKL_LIB="$PWD/ldk/build/lkl-x86_64/libnklkl.a" \
  cargo build --offline --release --features stela \
  --target x86_64-unknown-none --target-dir target-stela-exec
python3 tests/run-stela-x86.py \
  --kernel target-stela-exec/x86_64-unknown-none/release/nk
```

The optional boot probe runs after IDT/TSS setup, before SMP, with IF masked.
It compiles and executes trusted finite ARM64 blocks:
42 from an increment, return PC/kind, a three-iteration branch sequence with
both taken and fallthrough edges, a 64-bit RAM load/increment/store, SUBS NZCV,
and NEON OR with preserved flags. The runner requires all `STELA_EXEC_*`
markers, the system-mode engine checks below, and successful Linux epoll
after LKL and four-CPU scheduler startup.
Logs and the static epoll initrd remain in `target-stela-probe/`.

Each native image owns a frame, initializes it RW/NX, seals it RX and
supervisor-only, then restores RW/NX before freeing. Rights changes happen
only before APs and user address spaces exist, avoiding remote stale TLBs.
Execution uses the compiler's canonical block-entry thunk, which initializes
the exit stack pointer; a direct function-pointer call instead triple-faulted
in the first probe, with RSP=8 after an exit restored the uninitialized field.

The engine probe links `stela-engine`, contracts and formats without std.
`stela_nk/host.rs` implements a private bounded Host: one MMU-off guest RAM
page, at most 64 single-page native images, owned native-range readback,
idle W^X patching, and a system-mode pinned entry wrapped with host register
preservation. The engine itself fetches, compiles, dispatches and patches a
fixture that loops three times, reads CurrentEL, writes/reads TPIDR_EL1 and
stores 21 in RAM. It must reach the configured breakpoint within 64 blocks,
with matching architectural state and a positive native entry count.
`STELA_ENGINE_PASS` records 4 native entries, 2 patches and 4 blocks in QEMU.

The next fixture links the existing `stela-soc::uart::S5lUart` behind the
engine's Devices interface at a synthetic, unmapped 64 GiB address. Translated
code computes a vector, reads UTRSTAT, and writes `NK\n` through three UTXH
stores. Four actual x86 page faults reach the owned invocation's recovery
frame, then the engine completes the accesses against the real model.
The byte stream must match exactly (no duplicate/replayed writes); status and
vector state must match. An unsupported byte-width write refuses without
output. A separate missing-device load must reach the ARM data-abort vector,
retain its original ELR/FAR and destination register, and report EC 0x25.
The runner requires `STELA_MMIO_UART_OK`, `STELA_GUEST_ABORT_OK` and
`STELA_MMIO_PASS`, plus nk's ordinary int3 self-test and subsequent epoll.

The trap hook captures only an armed invocation's owned native RIP ranges.
It records GPRs and XMM8-15 from the ISR's FXSAVE image without allocating,
calling the engine or entering LKL. Returning through the saved host stack
restores callee-saved host registers; engine fault recovery runs afterward.
Protection/instruction-fetch/reserved-bit faults and faults within the fully resident
RAM fixture remain host failures. No spill lease/extended GPR bank is invented;
recovery rows needing unavailable state must refuse. APs do not exist while
the global boot-only invocation record is armed, and it is cleared on return.

The first engine run passed its own checks but corrupted LKL statics, later
faulting in `ip6_fl_purge`. Its measured 209,320-byte stack use exceeded the
64 KiB boot stack. The engine now runs on an owned 1 MiB stack with a bottom
canary and a reported usage watermark, then frees it before LKL starts.
The expanded UART/abort/shadow/sparse-RAM suite touches 463,456 bytes in the
current build.
`run-dynamic-x86.py --threads` also passes after the engine probe when given
the same stela-enabled kernel, using `--output target-stela-dynamic`.

The separate `stela_nk/host/shadow.rs` fixture enables the ARM MMU with four
levels of 4 KiB tables. Guest physical RAM reserves [2 GiB, 6 GiB), guest
virtual RAM starts at 4 MiB, and native backing is allocated elsewhere.
The data page lies just below guest PA 6 GiB while its nk backing is below
4 GiB. Stela's existing
ARM walker translates descriptors through the RAM owner. The host eagerly
builds a fixed low 2 MiB shadow aperture outside native entry, using a private
x86 root with supervisor-only, NX leaves, and switches CR3 only during native
execution. Reads/writes must produce 42 in the owned backing, not nk's low
identity RAM; kernel CR3 must be restored after every native call/fault.

`stela_nk/ram.rs` owns sparse backing separately from the shadow root.
Reservation allocates no payload frames. Uncommitted reads return zero;
writes commit zeroed 4 KiB frames and split copies at backing boundaries.
Raw host spans cannot cross a page. The standalone probe reserves 4 GiB,
uses only three backing pages, checks cross-page round trips and untouched
holes, and rejects overflowing/out-of-range transfers without partial writes.
The existing model DMA `Port` also reads/writes across backing pages; its
progress API reports the exact four-byte prefix before an out-of-range page.
This is model memory access, not real-device DMA or DART integration.
Required markers: `STELA_RAM_SPARSE_OK` and `STELA_RAM_DMA_OK`.
Shadow refresh commits valid RAM leaves outside traps; native demand filling
and a production concurrent memory service remain open.

The separate canonical-owner probe consumes a sparse builder and calls the
engine's real CPL0 `RegisteredSystemRam::register`, with IF masked. Each
committed page retains its guest PA and actual host PA/CPU mapping. A kept
owner reclaims all three frames only after the canonical backings are gone;
registration preserves historical bytes and introduces no second RAM store.
No raw/shadow alias escapes this fixture. All later reads, publications and
descriptor updates go through the canonical authority. Cross-page publication
and successful/mismatched descriptor CAS pass. Transfers entering an
unregistered hole refuse before effects; refused reads preserve the output.
An exclusive service prevents a second service acquisition, and failing that
service permanently refuses future acquisition and physical transfers.
Required: `STELA_RAM_CANONICAL_OK` and `STELA_RAM_AUTHORITY_OK`.

The first real registration refused native-target admission. Measurement found
PVH CR0=`0x80010013`, missing NE (native x87 exceptions). BSP/AP boot setup now
sets NE; the existing engine control checks remain intact. This owner probe
does not enter translated code through canonical native loans, join the old
shadow host's direct writers, or mount the M2 memory controller. Only committed
pages are registered; a 4 GiB sparse reservation is not 4 GiB of admitted RAM.

The same kernel passed dynamic Python/eight-thread, signal-return and all four
platform configurations after registration/reclamation. The x87 boot change
also passed OVMF/GRUB Multiboot2 boot with four CPUs, canonical markers and
epoll (`target-canonical-uefi/serial.log`, 2026-11-25). Docker's socket was
inaccessible for this run: a copy of the cached `target-uefi/disk.img` had its
kernel and initrd FAT chains replaced with the new ELF and epoll image, with
payload readback verified before boot. This reused GRUB; it did not rebuild
the EFI loader or exercise a physical PC.

After changing the data descriptor to read-only, the guest executes
`TLBI VMALLE1IS; DSB ISH; ISB` and writes. It must take a real x86 protection
fault, then an ARM same-EL data abort with the store's ELR, data VA in FAR,
and level-3 permission FSC 15; RAM must remain 42. Removing the descriptor
and executing TLBI before a load must remove the shadow mapping, preserve
the destination, and report level-3 translation FSC 7. Both cases use a
fresh walk after restoring nk's CR3, outside the trap handler. Only faults
within the owned shadow aperture may project x86 present/protection faults;
the identity fixture's protections remain host failures.

Required markers: `STELA_SHADOW_RAM_OK`, `STELA_SHADOW_PERMISSION_OK`,
`STELA_SHADOW_UNMAP_OK`, `STELA_SHADOW_PASS`. Root teardown clears borrowed
leaves without freeing RAM payloads, reclaims its own table frames, then
lets the RAM owner free the backing. Subsequent Linux userland probes check
that these roots and borrowed frames do not corrupt nk/LKL state.

The context fixture gives two roots different non-global physical code and
data pages at identical virtual addresses. One code body adds 1, the other
10. Nine turns use actual guest `MSR TTBR0_EL1,x10`/ISB instructions to switch
roots and ASIDs. Every turn checks both physical data pages, the result and
TTBR register, the host-observed regime, and restored kernel CR3. The last
two switches reuse an ASID with explicit `TLBI VMALLE1IS; DSB ISH; ISB`.
Final values must be 46 and 140: stale native code, stale data mappings or
cross-space writes fail. Required: `STELA_SHADOW_CONTEXT_OK` and
`STELA_SHADOW_ASID_REUSE_OK`. This uses eager full rebuilds of one shadow
root, not preserved per-ASID hardware mappings or scoped invalidation.

The MMU-on UART fixture maps a virtual page at 4 MiB + 8 KiB to the model's
64 GiB physical address with MAIR Device-nGnRnE attributes. Native accesses
fault because the device gets no RAM leaf. The host walks the fault VA and
passes the translated PA to the same S5L model. The guest must read ready
status, transmit exactly `NK\n` in three writes, preserve its vector result,
and restore kernel CR3. Only the code page is mapped as RAM. Required:
`STELA_SHADOW_MMIO_OK`.

These are architectural expected-value checks, not a hosted-Stela differential
run. All Hosts are private trusted fixtures before SMP and share kernel
identity mappings: this is not guest isolation. The identity host refuses MMU
enabling; the shadow host covers only a fixed EL1 aperture. Stage 2 and guest
timer arming remain refused. General demand filling, retained ASID roots and
scoped invalidation, a persistent
concurrent Host, general fault/spill recovery, guest isolation, the full M2
board/runtime and macOS boot remain open. Verified in QEMU q35 on 2026-11-25.

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

The minimal init script creates Linux's `/dev/random` (1:8) and `/dev/urandom`
(1:9) nodes. Every runner invocation supplies 32 fresh bytes from the host's
CSPRNG (`os.urandom`), with mode 0600 in the generated test rootfs.
The libc-free `init/seed-entropy-x86.c` helper reads those bytes, credits 256
bits with Linux RNDADDENTROPY, and requires a complete 32-byte nonblocking
getrandom result before printing `HOST_ENTROPY_SEEDED`. The init script then
removes the seed file from the running guest. Failure prevents Python launch
and makes the probe fail; passing requires the entropy-ready marker too.

Without this provisioning, both concurrent and standalone Python runs stalled
before their first print. QEMU register/stack inspection located the loop in
`try_to_generate_entropy -> random_get_entropy_fallback -> clock_read ->
nk_time_ns`, not in the shadow translation path. After seeding, Linux reports
`crng init done`, Python/ctypes and all eight threads pass. This provisions the
test image only; it does not implement a production nk entropy source or
change the kernel's existing unseeded getrandom forwarding policy.

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

## Booting like a PC: UEFI + GRUB + Multiboot2, and the screen console

Every other test starts nk with QEMU's `-kernel` (PVH). This one builds a GPT
disk whose EFI System Partition holds GRUB (`EFI/BOOT/BOOTX64.EFI`), nk and
an initrd, and boots it from a USB mass-storage device under OVMF, as a PC
boots a USB stick. nk must take its memory map, ACPI RSDP and framebuffer from
GRUB's Multiboot2 tags.

```sh
python3 tests/run-uefi-x86.py --kernel target-hx/x86_64-unknown-none/release/nk \
  --initrd /path/to/initrd.cpio --require X86_ROOT_NOT_FOUND \
  --require "framebuffer console" --screenshot target-uefi/screen.png
```

GRUB's EFI image is assembled in Docker (Debian's amd64 `grub-efi-amd64-bin`
modules, native `grub-mkimage`, mtools) and cached in the output directory.
`--disk-only` stops after writing `target-uefi/disk.img`, which is also what
goes on a USB stick for a real PC (`dd` it to the whole device).

The framebuffer console (`core/src/fbcon.rs`) draws everything printed to
serial -- nk, Linux's printk, panics -- in Linux's 8x16 font on the GOP
framebuffer. GRUB reports GOP's 32-bit pixels as 24 bpp, so the pixel size
comes from the pitch. It wraps instead of scrolling (scrolling copies the
whole uncached framebuffer per line). Verified 2026-11-25: `UEFI_PASS` with
and without SMP, and a screenshot showing the log at 800x600 (100x37 text).

## Stele's translator inside nk

`--features stela` links `stela-compiler` into nk and, at boot, translates
`add x0, x0, #1; ret` and prints the result. Translation only; nothing runs.

```sh
NK_LKL_LIB=$PWD/ldk/build/lkl-x86_64/libnklkl.a cargo build --release \
  --features stela --target x86_64-unknown-none --target-dir target-stela
# serial: "stela:  translated 8 arm64 bytes at 0x1000 into 47 x86-64 bytes"
```

The rest of the translator crates (engine, runtime, SoC model, loader) build
for nk's target as well, with the soft crypto backends:
`RUSTFLAGS='--cfg aes_force_soft --cfg chacha20_backend="soft" --cfg
aes_backend="soft" --cfg polyval_backend="soft" --cfg poly1305_backend="soft"'
cargo build -p stela-runtime --no-default-features --target x86_64-unknown-none`.

## Dedicated vCPU cores

```sh
NK_DEDICATED_TEST=1 NK_SMP=1 NK_AP_LAYER=4 NK_LKL_LIB=$PWD/ldk/build/lkl-x86_64/libnklkl.a \
  cargo build --release --target x86_64-unknown-none --target-dir target-dedicated
python3 tests/run-dedicated-x86.py --kernel target-dedicated/x86_64-unknown-none/release/nk \
  --initrd /path/to/initrd.cpio
```

After Linux is up, `sched::dedicate_cpu(3, ...)` runs a busy probe while CPU 2
is contested by pinned spinners and a control probe. PASS: the dedicated
probe is switched in 0 times during its window, the control is preempted,
a task allowed on {2,3} never runs on 3, and the reservation is released.
Scheduler events, not timestamps, are the evidence: under TCG QEMU slices
vCPUs itself.

The same build then tests `hrtimer`, the dedicated core's one-shot timer:
100 timers each of 50 us, 200 us, 2 ms and 5 ms must all fire, the LAPIC
count rate measured on that core must match the boot calibration (the
nk-side proof that a count means the right time), and each interval's median
lateness must stay under QEMU's own delivery step (~1.28 ms on macOS, whose
QEMU main loop waits in whole milliseconds). `HRTIMER_PASS`. Run the scheduler torture test (`NK_SCHEDTEST=1`) after any
scheduler change; it passed 3/3 with this one.

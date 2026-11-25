#!/usr/bin/env python3
"""Boot nk with an NVMe disk and require Linux's nvme driver to read its GPT.

Exercises the LKL PCI bridge (core/src/lkl_pci_x86.rs) end to end: nk picks
the NVMe controller from its PCI inventory and passes it as `lkl_pci=`, maps
its BAR (QEMU puts the 64-bit BAR at 768 GiB, so through the MMIO window),
programs MSI-X for Linux's queues, and serves DMA by identity. Linux finding
the partition proves config space, BAR access, DMA and interrupt completion.

Needs a kernel built against an x86 LKL archive with CONFIG_PCI and
CONFIG_BLK_DEV_NVME (lkl/nk-x86_64.config). No initrd is required: the
markers all come from Linux's probe, before user space starts.
"""
import argparse
from pathlib import Path
import struct
import subprocess
import time
import uuid
import zlib

SECTOR = 512
LINUX_FS = uuid.UUID("0fc63daf-8483-4772-8e79-3d69d8477de4")


def gpt_disk(path, mib=16):
    """A protective MBR and a GPT with one partition at sector 2048."""
    total = mib * 1024 * 1024 // SECTOR
    disk = bytearray(total * SECTOR)
    disk[446:462] = bytes([0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF]) + struct.pack("<II", 1, total - 1)
    disk[510:512] = b"\x55\xaa"
    entries = bytearray(128 * 128)
    entry = (LINUX_FS.bytes_le + uuid.uuid4().bytes_le
             + struct.pack("<QQQ", 2048, total - 2048, 0) + "NKTEST".encode("utf-16-le"))
    entries[: len(entry)] = entry
    crc = zlib.crc32(entries)

    def header(current, backup, first_entry):
        h = bytearray(struct.pack("<8sIIIIQQQQ16sQIII", b"EFI PART", 0x10000, 92, 0, 0,
                                  current, backup, 34, total - 34, uuid.uuid4().bytes_le,
                                  first_entry, 128, 128, crc))
        h[16:20] = struct.pack("<I", zlib.crc32(h[:92]))
        return bytes(h)

    disk[SECTOR:SECTOR + 92] = header(1, total - 1, 2)
    disk[2 * SECTOR:2 * SECTOR + len(entries)] = entries
    disk[(total - 33) * SECTOR:(total - 33) * SECTOR + len(entries)] = entries
    disk[(total - 1) * SECTOR:(total - 1) * SECTOR + 92] = header(total - 1, 1, total - 33)
    path.write_bytes(disk)


REQUIRED = [
    "lkl_pci=",                     # nk chose the controller
    "lkl-pci: BAR0",                # resource_alloc mapped it
    "lkl-pci: MSI-X",               # msi_init programmed the table
    "nvme nvme0: pci function",     # Linux's driver bound
    "nvme0n1: p1",                  # DMA + completions: the GPT was read
]
FATAL = ["Kernel panic", "!! x86-64 exception", "BUG:", "Oops"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--initrd", type=Path)
    parser.add_argument("--output", type=Path, default=Path("target-nvme"))
    parser.add_argument("--qemu", default="qemu-system-x86_64")
    parser.add_argument("--smp", default="4")
    parser.add_argument("--timeout", type=float, default=90)
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    disk = out / "nvme.img"
    gpt_disk(disk)
    log = out / "serial.log"
    cmd = [args.qemu, "-M", "q35", "-m", "4G", "-smp", args.smp,
           "-kernel", str(args.kernel.resolve(strict=True)),
           "-drive", f"file={disk},if=none,id=nv0,format=raw",
           "-device", "nvme,drive=nv0,serial=nk0",
           "-display", "none", "-serial", f"file:{log}", "-monitor", "none", "-no-reboot"]
    if args.initrd:
        cmd += ["-initrd", str(args.initrd.resolve(strict=True))]
    proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL)
    deadline = time.monotonic() + args.timeout
    text = ""
    try:
        while time.monotonic() < deadline:
            time.sleep(1)
            text = log.read_text(errors="replace") if log.exists() else ""
            if any(f in text for f in FATAL) or all(r in text for r in REQUIRED):
                break
            if proc.poll() is not None:
                break
    finally:
        proc.kill()
        proc.wait()
    bad = [f for f in FATAL if f in text]
    missing = [r for r in REQUIRED if r not in text]
    if bad or missing:
        for f in bad:
            print(f"FAIL: {f!r} in serial log")
        for r in missing:
            print(f"FAIL: missing {r!r}")
        print(f"serial log: {log}")
        return 1
    print(f"NVME_PASS (serial log: {log})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

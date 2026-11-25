#!/usr/bin/env python3
"""Write a `stela-dev boot prepare` bundle as a raw disk image for nk.

The image is a cpio newc archive of the bundle's files, padded to 4 KiB.
Attached as an NVMe drive (`run-dedicated-x86.py --disk`), it lets nk read a
bundle the initrd cannot carry -- a restore ramdisk is ~360 MiB -- straight
from Linux's /dev/nvme0n1 into guest DRAM (core/src/stela_nk/host/sptm.rs).
"""
import argparse
from pathlib import Path


def newc_header(name, size, mode):
    name_b = name.encode() + b"\0"
    fields = [0, mode, 0, 0, 1, 0, size, 0, 0, 0, 0, len(name_b), 0]
    head = b"070701" + b"".join(b"%08X" % f for f in fields) + name_b
    return head + b"\0" * (-len(head) % 4)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=Path)
    parser.add_argument("image", type=Path)
    args = parser.parse_args()
    files = sorted(p for p in args.bundle.iterdir() if p.is_file())
    with args.image.open("wb") as out:
        for path in files:
            size = path.stat().st_size
            out.write(newc_header(path.name, size, 0o100644))
            with path.open("rb") as src:
                while chunk := src.read(1 << 24):
                    out.write(chunk)
            out.write(b"\0" * (-size % 4))
        out.write(newc_header("TRAILER!!!", 0, 0))
        out.write(b"\0" * (-out.tell() % 4096))
    print(f"{args.image}: {len(files)} files, {args.image.stat().st_size >> 20} MiB")


if __name__ == "__main__":
    main()

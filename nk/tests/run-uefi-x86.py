#!/usr/bin/env python3
"""Boot nk the way a PC boots a USB stick: UEFI firmware, GRUB, Multiboot2.

Every other x86 test starts nk with QEMU's `-kernel` (PVH), which no real
machine offers. This one builds a GPT disk whose EFI System Partition holds
GRUB as EFI/BOOT/BOOTX64.EFI, nk and an initrd, and boots it under OVMF.
GRUB loads nk with `multiboot2` and the initrd with `module2`; nk must
recognise Multiboot2, take its memory map and ACPI RSDP from GRUB's tags
(there is no BIOS area to scan under UEFI), and bring Linux up.

GRUB's EFI image is assembled in Docker (Debian's x86_64-efi modules from the
amd64 grub-efi-amd64-bin package, the native grub-mkimage, mtools) and cached
in the output directory. The same disk image can be written to a USB stick
for a real PC: `--disk-only` stops after building it.
"""
import argparse
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import time
import uuid
import zlib

SECTOR = 512
ESP_TYPE = uuid.UUID("c12a7328-f81f-11d2-ba4b-00a0c93ec93b")
OVMF_DIRS = [Path("/opt/homebrew/share/qemu"), Path("/usr/local/share/qemu"),
             Path("/usr/share/qemu"), Path("/usr/share/OVMF")]

GRUB_CFG = """set timeout=0
set default=0
serial --unit=0 --speed=115200
terminal_input serial console
terminal_output serial console
insmod all_video
insmod efi_gop
set gfxpayload=keep
menuentry "nk" {
    echo "GRUB: loading nk (multiboot2)"
    multiboot2 /boot/nk
    module2 /boot/initrd.cpio
    boot
}
"""

GRUB_MODULES = ("part_gpt fat normal configfile echo test multiboot2 serial terminal "
                "all_video efi_gop efi_uga video video_fb gfxterm search").split()

DOCKER_SCRIPT = r"""
set -e
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null
apt-get install -y -qq --no-install-recommends grub-common mtools dosfstools >/dev/null
if [ ! -f /out/BOOTX64.EFI ]; then
  dpkg --add-architecture amd64
  apt-get update -qq >/dev/null
  cd /tmp && apt-get download -qq grub-efi-amd64-bin:amd64 >/dev/null
  dpkg -x /tmp/grub-efi-amd64-bin_*.deb /tmp/g
  grub-mkimage -d /tmp/g/usr/lib/grub/x86_64-efi -O x86_64-efi \
      -o /out/BOOTX64.EFI -p /boot/grub __MODULES__
fi
# FAT32 inside the ESP partition (offset 1 MiB), then the files.
mformat -i /out/disk.img@@1M -F -v NKBOOT ::
mmd -i /out/disk.img@@1M ::/EFI ::/EFI/BOOT ::/boot ::/boot/grub
mcopy -i /out/disk.img@@1M /out/BOOTX64.EFI ::/EFI/BOOT/BOOTX64.EFI
mcopy -i /out/disk.img@@1M /out/grub.cfg ::/boot/grub/grub.cfg
mcopy -i /out/disk.img@@1M /out/nk ::/boot/nk
mcopy -i /out/disk.img@@1M /out/initrd.cpio ::/boot/initrd.cpio
echo DISK_READY
"""


def gpt_esp_disk(path, mib):
    """A GPT disk with one EFI System Partition from 1 MiB to the end."""
    total = mib * 1024 * 1024 // SECTOR
    first, last = 2048, total - 34
    disk = bytearray(total * SECTOR)
    disk[446:462] = bytes([0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF]) + struct.pack("<II", 1, total - 1)
    disk[510:512] = b"\x55\xaa"
    entries = bytearray(128 * 128)
    entry = (ESP_TYPE.bytes_le + uuid.uuid4().bytes_le + struct.pack("<QQQ", first, last, 0)
             + "EFI system".encode("utf-16-le"))
    entries[: len(entry)] = entry
    crc = zlib.crc32(entries)

    def header(current, backup, first_entry):
        h = bytearray(struct.pack("<8sIIIIQQQQ16sQIII", b"EFI PART", 0x10000, 92, 0, 0,
                                  current, backup, 34, last, uuid.uuid4().bytes_le,
                                  first_entry, 128, 128, crc))
        h[16:20] = struct.pack("<I", zlib.crc32(h[:92]))
        return bytes(h)

    disk[SECTOR:SECTOR + 92] = header(1, total - 1, 2)
    disk[2 * SECTOR:2 * SECTOR + len(entries)] = entries
    disk[(total - 33) * SECTOR:(total - 33) * SECTOR + len(entries)] = entries
    disk[(total - 1) * SECTOR:(total - 1) * SECTOR + 92] = header(total - 1, 1, total - 33)
    path.write_bytes(disk)


def ppm_to_png(ppm, png):
    """QEMU's screendump writes binary PPM (P6); write it as PNG, no PIL."""
    data = ppm.read_bytes()
    parts, at = [], 0
    while len(parts) < 4:
        while data[at:at + 1].isspace():
            at += 1
        end = at
        while not data[end:end + 1].isspace():
            end += 1
        parts.append(data[at:end])
        at = end
    at += 1
    w, h = int(parts[1]), int(parts[2])
    pixels = data[at:at + w * h * 3]
    raw = b"".join(b"\0" + pixels[y * w * 3:(y + 1) * w * 3] for y in range(h))

    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    png.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
                    + chunk(b"IDAT", zlib.compress(raw, 6)) + chunk(b"IEND", b""))


def find_ovmf():
    for d in OVMF_DIRS:
        code = d / "edk2-x86_64-code.fd"
        if code.exists():
            vars_ = d / "edk2-i386-vars.fd"
            return code, vars_ if vars_.exists() else None
        if (d / "OVMF_CODE.fd").exists():
            return d / "OVMF_CODE.fd", d / "OVMF_VARS.fd"
    return None, None


REQUIRED = ["GRUB: loading nk", "boot:  Multiboot2", "Linux is up on nk"]
FATAL = ["Kernel panic", "!! x86-64 exception", "BUG:", "Oops", "error: "]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--initrd", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("target-uefi"))
    parser.add_argument("--image", default="nethos-ldk-cross-amd64",
                        help="Docker image with apt (any Debian trixie image)")
    parser.add_argument("--disk-mib", type=int, default=64)
    parser.add_argument("--disk-only", action="store_true")
    parser.add_argument("--require", action="append", default=[],
                        help="additional serial-log marker to require")
    parser.add_argument("--screenshot", type=Path,
                        help="after the markers, save the screen here as PNG")
    parser.add_argument("--port", type=int, default=4481)
    parser.add_argument("--qemu", default="qemu-system-x86_64")
    parser.add_argument("--smp", default="4")
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)

    shutil.copyfile(args.kernel, out / "nk")
    shutil.copyfile(args.initrd, out / "initrd.cpio")
    (out / "grub.cfg").write_text(GRUB_CFG)
    gpt_esp_disk(out / "disk.img", args.disk_mib)
    script = DOCKER_SCRIPT.replace("__MODULES__", " ".join(GRUB_MODULES))
    build = subprocess.run(["docker", "run", "--rm", "-v", f"{out}:/out", args.image,
                            "sh", "-c", script], capture_output=True, text=True)
    if "DISK_READY" not in build.stdout:
        print(build.stdout[-2000:], build.stderr[-2000:])
        print("FAIL: could not build the GRUB EFI disk image")
        return 1
    print(f"disk image: {out / 'disk.img'}")
    if args.disk_only:
        return 0

    code, vars_template = find_ovmf()
    if not code:
        print("FAIL: no OVMF (edk2-x86_64-code.fd) found")
        return 1
    vars_ = out / "ovmf-vars.fd"
    if vars_template:
        shutil.copyfile(vars_template, vars_)
    else:
        vars_.write_bytes(b"\0" * (528 * 1024))
    log = out / "serial.log"
    cmd = [args.qemu, "-M", "q35", "-m", "4G", "-smp", args.smp,
           "-drive", f"if=pflash,format=raw,readonly=on,file={code}",
           "-drive", f"if=pflash,format=raw,file={vars_}",
           "-drive", f"file={out / 'disk.img'},format=raw,if=none,id=usb0",
           "-device", "qemu-xhci,id=xhci", "-device", "usb-storage,bus=xhci.0,drive=usb0,bootindex=0",
           "-display", "none", "-serial", f"file:{log}", "-no-reboot",
           "-monitor", f"tcp:127.0.0.1:{args.port},server,nowait" if args.screenshot else "none"]
    proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL)
    deadline = time.monotonic() + args.timeout
    want = REQUIRED + args.require
    text = ""
    try:
        while time.monotonic() < deadline:
            time.sleep(1)
            text = log.read_text(errors="replace") if log.exists() else ""
            if any(x in text for x in FATAL) or all(r in text for r in want) or proc.poll() is not None:
                break
        if args.screenshot and proc.poll() is None and all(r in text for r in want):
            time.sleep(2)
            ppm = out / "screen.ppm"
            with socket.create_connection(("127.0.0.1", args.port), timeout=5) as mon:
                mon.sendall(f"screendump {ppm}\n".encode())
                time.sleep(2)
            if ppm.exists():
                ppm_to_png(ppm, args.screenshot.resolve())
                print(f"screenshot: {args.screenshot}")
    finally:
        proc.kill()
        proc.wait()
    bad = [x for x in FATAL if x in text]
    missing = [r for r in want if r not in text]
    for x in bad:
        print(f"FAIL: {x!r} in serial log")
    for r in missing:
        print(f"FAIL: missing {r!r}")
    if bad or missing:
        print(f"serial log: {log}")
        return 1
    print(f"UEFI_PASS (serial log: {log})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

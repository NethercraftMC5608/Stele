#!/usr/bin/env python3
"""Boot nk with xHCI + a USB keyboard and HD Audio; require Linux's drivers.

Through the LKL PCI bridge, nk hands Linux the first xHCI controller and the
first HD Audio controller (beside NVMe and Ethernet). The test:

- finds the QEMU USB keyboard's event node in /proc/bus/input/devices,
- types a key through the QEMU monitor (`sendkey a`) and requires the
  key-press event (EV_KEY, KEY_A, value 1) to be read from that node,
- requires Linux's HDA driver to have registered a sound card
  (/proc/asound/cards).

Needs an x86 LKL archive built with lkl/nk-x86_64.config (USB, xHCI, HID,
HD Audio) and a static x86-64 busybox (`--busybox`).
"""
import argparse
from pathlib import Path
import socket
import subprocess
import time

INIT = r"""#!/bin/busybox sh
export PATH=/bin
B=/bin/busybox
$B mount -t devtmpfs devtmpfs /dev
$B mount -t proc proc /proc 2>/dev/null
echo USB_PROBE_START
$B cat /proc/asound/cards 2>&1 | $B head -4
E=$($B grep -A8 "USB Keyboard" /proc/bus/input/devices | $B grep -o "event[0-9]*" | $B head -1)
echo "KBD_NODE $E"
echo KBD_READY
$B head -c 96 /dev/input/$E | $B hexdump -v -e '24/1 "%02x " "\n"'
echo USB_PROBE_END
exec $B sleep 3600
"""


def newc(entries):
    out = bytearray()
    for ino, (name, mode, data) in enumerate(entries + [("TRAILER!!!", 0, b"")], 1):
        nm = name.encode() + b"\0"
        fields = [ino, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(nm), 0]
        out += b"070701" + b"".join(f"{v:08x}".encode() for v in fields) + nm
        out += b"\0" * (-len(out) % 4)
        out += data
        out += b"\0" * (-len(out) % 4)
    return bytes(out)


# struct input_event on x86-64: 16-byte timeval, u16 type, u16 code, s32 value.
# EV_KEY (1), KEY_A (30 = 0x1e), press (1).
KEY_A_PRESS = "01 00 1e 00 01 00 00 00"
FATAL = ["Kernel panic", "!! x86-64 exception", "BUG:", "Oops"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--busybox", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("target-usb"))
    parser.add_argument("--qemu", default="qemu-system-x86_64")
    parser.add_argument("--smp", default="4")
    parser.add_argument("--port", type=int, default=4471)
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    d, f = 0o040755, 0o100755
    initrd = out / "usb.cpio"
    initrd.write_bytes(newc([
        ("bin", d, b""), ("dev", d, b""), ("proc", d, b""), ("sys", d, b""),
        ("bin/busybox", f, args.busybox.read_bytes()),
        ("nk-init", f, INIT.encode()),
    ]))
    log = out / "serial.log"
    cmd = [args.qemu, "-M", "q35", "-m", "4G", "-smp", args.smp,
           "-kernel", str(args.kernel.resolve(strict=True)), "-initrd", str(initrd),
           "-device", "qemu-xhci,id=xhci", "-device", "usb-kbd,bus=xhci.0",
           "-audiodev", "none,id=snd0", "-device", "intel-hda",
           "-device", "hda-duplex,audiodev=snd0",
           "-display", "none", "-serial", f"file:{log}", "-no-reboot",
           "-monitor", f"tcp:127.0.0.1:{args.port},server,nowait"]
    proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL)
    deadline = time.monotonic() + args.timeout
    text, sent = "", False
    try:
        while time.monotonic() < deadline:
            time.sleep(1)
            text = log.read_text(errors="replace") if log.exists() else ""
            if any(x in text for x in FATAL) or "USB_PROBE_END" in text or proc.poll() is not None:
                break
            if "KBD_READY" in text and not sent:
                time.sleep(2)  # let `head` open the node and block
                with socket.create_connection(("127.0.0.1", args.port), timeout=5) as mon:
                    for _ in range(3):
                        mon.sendall(b"sendkey a\n")
                        time.sleep(0.5)
                sent = True
    finally:
        proc.kill()
        proc.wait()
    checks = {
        "xHCI handed to Linux": "lkl-pci: 0000" in text and "1b36:000d" in text,
        "keyboard event node found": "KBD_NODE event" in text,
        "KEY_A press read from it": KEY_A_PRESS in text,
        "HDA sound card registered": "HDA Intel" in text or "HDA-Intel" in text,
    }
    bad = [x for x in FATAL if x in text]
    for x in bad:
        print(f"FAIL: {x!r} in serial log")
    for name, ok in checks.items():
        print(("ok:   " if ok else "FAIL: ") + name)
    if bad or not all(checks.values()):
        print(f"serial log: {log}")
        return 1
    print(f"USB_HDA_PASS (serial log: {log})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

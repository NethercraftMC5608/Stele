#!/usr/bin/env python3
"""Boot nk with q35's e1000e and require DHCP and a ping through Linux's driver.

Exercises the LKL PCI bridge with a second device class: nk passes the NIC
(and an NVMe disk, so two root buses are in use) as `lkl_pci=`, Linux's
e1000e driver binds with MSI-X, `ifconfig eth0 up` brings the link up,
busybox `udhcpc` gets a lease from QEMU's user-mode network, and `ping`
reaches the gateway 10.0.2.2.

Needs a kernel built against an x86 LKL archive with lkl/nk-x86_64.config
(PCI, NVMe, e1000e, AF_PACKET), built with -mno-red-zone, and a static x86-64
busybox with udhcpc, ifconfig, route and ping (`--busybox`).
"""
import argparse
from pathlib import Path
import subprocess
import time

INIT = r"""#!/bin/busybox sh
export PATH=/bin
B=/bin/busybox
$B mount -t devtmpfs devtmpfs /dev
$B mount -t proc proc /proc 2>/dev/null
echo NET_PROBE_START
$B ifconfig lo 127.0.0.1 up
$B ifconfig eth0 up && echo ETH0_UP
$B udhcpc -i eth0 -n -q -t 8 -s /usr/share/udhcpc/default.script && echo DHCP_OK
$B ping -c 3 -W 2 10.0.2.2 && echo PING_OK
echo NET_PROBE_END
exec $B sleep 3600
"""

LEASE_SCRIPT = r"""#!/bin/busybox sh
# Apply a udhcpc lease with ifconfig/route (no `ip` applet needed).
B=/bin/busybox
case "$1" in
  deconfig) $B ifconfig "$interface" 0.0.0.0 up ;;
  bound|renew)
    $B ifconfig "$interface" "$ip" netmask "${subnet:-255.255.255.0}" up
    for r in $router; do $B route add default gw "$r" dev "$interface" && break; done
    echo "LEASE $interface $ip router $router"
    ;;
esac
exit 0
"""


def newc(entries):
    """A newc cpio archive from (name, mode, data) entries."""
    out = bytearray()
    ino = 1
    for name, mode, data in entries + [("TRAILER!!!", 0, b"")]:
        nm = name.encode() + b"\0"
        fields = [ino, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(nm), 0]
        out += b"070701" + b"".join(f"{v:08x}".encode() for v in fields) + nm
        out += b"\0" * (-len(out) % 4)
        out += data
        out += b"\0" * (-len(out) % 4)
        ino += 1
    return bytes(out)


REQUIRED = ["lkl_pci=", "e1000e", "eth0: NIC Link is Up", "ETH0_UP", "LEASE eth0",
            "DHCP_OK", "bytes from 10.0.2.2", "PING_OK", "NET_PROBE_END"]
FATAL = ["Kernel panic", "!! x86-64 exception", "BUG:", "Oops"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--busybox", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("target-net"))
    parser.add_argument("--qemu", default="qemu-system-x86_64")
    parser.add_argument("--smp", default="4")
    parser.add_argument("--timeout", type=float, default=150)
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)

    d, f = 0o040755, 0o100755
    initrd = out / "net.cpio"
    initrd.write_bytes(newc([
        ("bin", d, b""), ("dev", d, b""), ("proc", d, b""), ("sys", d, b""),
        ("etc", d, b""), ("usr", d, b""), ("usr/share", d, b""),
        ("usr/share/udhcpc", d, b""),
        ("bin/busybox", f, args.busybox.read_bytes()),
        ("usr/share/udhcpc/default.script", f, LEASE_SCRIPT.encode()),
        ("nk-init", f, INIT.encode()),
    ]))
    disk = out / "nvme.img"
    disk.write_bytes(b"\0" * (4 * 1024 * 1024))
    log = out / "serial.log"
    cmd = [args.qemu, "-M", "q35", "-m", "4G", "-smp", args.smp,
           "-kernel", str(args.kernel.resolve(strict=True)), "-initrd", str(initrd),
           "-drive", f"file={disk},if=none,id=nv0,format=raw",
           "-device", "nvme,drive=nv0,serial=nk0",
           "-display", "none", "-serial", f"file:{log}", "-monitor", "none", "-no-reboot"]
    proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL)
    deadline = time.monotonic() + args.timeout
    text = ""
    try:
        while time.monotonic() < deadline:
            time.sleep(1)
            text = log.read_text(errors="replace") if log.exists() else ""
            if any(x in text for x in FATAL) or "NET_PROBE_END" in text or proc.poll() is not None:
                break
    finally:
        proc.kill()
        proc.wait()
    bad = [x for x in FATAL if x in text]
    missing = [r for r in REQUIRED if r not in text]
    if bad or missing:
        for x in bad:
            print(f"FAIL: {x!r} in serial log")
        for r in missing:
            print(f"FAIL: missing {r!r}")
        print(f"serial log: {log}")
        return 1
    print(f"NET_PASS (serial log: {log})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

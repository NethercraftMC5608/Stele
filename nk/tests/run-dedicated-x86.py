#!/usr/bin/env python3
"""A dedicated vCPU core is not preempted while Linux owns CPU 0.

Needs a kernel built with NK_DEDICATED_TEST=1 NK_SMP=1 NK_AP_LAYER=4 (see
core/src/dedicated_test.rs). The in-kernel probe runs after Linux is up and
prints DEDICATED_PASS when the dedicated probe on CPU 3 was switched in 0
times during its 1.5 s window, the control probe on contested CPU 2 was
preempted, no other task ran on CPU 3, and the reservation was released.
It then runs one-shot high-resolution timers on the dedicated core
(HRTIMER_PASS: all fire, the core's LAPIC rate matches calibration, and
lateness stays under the host's delivery step). The boot must then still
reach the init script.

`--require STELA_TIMER_PASS` instead checks a kernel built with
NK_STELA_TIMER_TEST=1 and `--features stela`: a translated ARM64 guest on the
dedicated core arms its generic timer and sleeps in WFI until the hrtimer
delivers the FIQ (core/src/stela_nk/host.rs, `timer_probe`).
"""
import argparse
from pathlib import Path
import subprocess
import time


def newc(name, data, mode):
    """One cpio newc entry."""
    name_b = name.encode() + b"\0"
    fields = [0, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(name_b), 0]
    head = b"070701" + b"".join(b"%08X" % f for f in fields)
    pad = lambda n: b"\0" * (-n % 4)
    return head + name_b + pad(len(head) + len(name_b)) + data + pad(len(data))


def with_files(archive, files):
    """`archive` (newc) with `files` [(guest path, bytes)] and their parent
    directories inserted before its first trailer: nk's unpacker stops
    there, so anything after it (a second concatenated archive) is unread."""
    end = 0
    while True:  # walk the entries: the name may appear inside file data
        assert archive[end:end + 6] == b"070701", "not a newc archive"
        size = int(archive[end + 54:end + 62], 16)
        namesize = int(archive[end + 94:end + 102], 16)
        name = archive[end + 110:end + 110 + namesize - 1]
        if name == b"TRAILER!!!":
            break
        data = (end + 110 + namesize + 3) & ~3
        end = (data + size + 3) & ~3
    out, dirs = archive[:end], set()
    for path, data in files:
        parts = path.strip("/").split("/")
        for i in range(1, len(parts)):
            d = "/".join(parts[:i])
            if d not in dirs:
                dirs.add(d)
                out += newc(d, b"", 0o040755)
        out += newc("/".join(parts), data, 0o100644)
    return out + archive[end:]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--initrd", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("target-dedicated-run"))
    parser.add_argument("--qemu", default="qemu-system-x86_64")
    parser.add_argument("--timeout", type=float, default=150)
    parser.add_argument("--cpu", help="QEMU -cpu model (default: QEMU's own)")
    parser.add_argument("--memory", default="4G", help="QEMU -m (default 4G)")
    parser.add_argument("--accel", help="QEMU -accel (kvm on a Linux x86 host)")
    parser.add_argument("--disk", type=Path,
                        help="attach this raw image read-only as an NVMe drive (tests/bundle-disk.py)")
    parser.add_argument("--add", action="append", default=[], metavar="HOST:GUEST",
                        help="add a host file to the initrd at a guest path")
    parser.add_argument("--require", action="append", default=[],
                        help="markers to require (default DEDICATED_PASS and HRTIMER_PASS)")
    args = parser.parse_args()
    want = args.require or ["DEDICATED_PASS", "HRTIMER_PASS"]
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    log = out / "serial.log"
    initrd = args.initrd.resolve(strict=True)
    if args.add:
        files = [(guest, Path(host).read_bytes())
                 for host, guest in (spec.split(":", 1) for spec in args.add)]
        initrd = out / "initrd.cpio"
        initrd.write_bytes(with_files(args.initrd.read_bytes(), files))
    cpu = (["-cpu", args.cpu] if args.cpu else []) + (["-accel", args.accel] if args.accel else [])
    if args.disk:
        cpu += ["-drive", f"file={args.disk.resolve(strict=True)},if=none,id=bundle,format=raw,readonly=on",
                "-device", "nvme,drive=bundle,serial=nkbundle"]
    proc = subprocess.Popen([args.qemu, "-M", "q35", "-m", args.memory, "-smp", "4", *cpu,
                             "-kernel", str(args.kernel.resolve(strict=True)),
                             "-initrd", str(initrd),
                             "-display", "none", "-serial", f"file:{log}",
                             "-monitor", "none", "-no-reboot"], stdin=subprocess.DEVNULL)
    deadline = time.monotonic() + args.timeout
    text = ""
    try:
        while time.monotonic() < deadline:
            time.sleep(1)
            text = log.read_text(errors="replace") if log.exists() else ""
            if "_FAIL" in text or "DEDICATED_SKIP" in text or "X86_ROOT" in text \
                    or "exception" in text or proc.poll() is not None:
                break
    finally:
        proc.kill()
        proc.wait()
    for line in text.splitlines():
        if "dedicated:" in line or "hrtimer:" in line or "STELA_TIMER:" in line:
            print(line.strip())
    ok = (all(m in text for m in want) and "_FAIL" not in text
          and "X86_ROOT" in text and "exception" not in text)
    print(" ".join(want) if ok else f"FAIL (serial log: {log})")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())

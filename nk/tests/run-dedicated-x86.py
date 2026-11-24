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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--initrd", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("target-dedicated-run"))
    parser.add_argument("--qemu", default="qemu-system-x86_64")
    parser.add_argument("--timeout", type=float, default=150)
    parser.add_argument("--require", action="append", default=[],
                        help="markers to require (default DEDICATED_PASS and HRTIMER_PASS)")
    args = parser.parse_args()
    want = args.require or ["DEDICATED_PASS", "HRTIMER_PASS"]
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    log = out / "serial.log"
    proc = subprocess.Popen([args.qemu, "-M", "q35", "-m", "4G", "-smp", "4",
                             "-kernel", str(args.kernel.resolve(strict=True)),
                             "-initrd", str(args.initrd.resolve(strict=True)),
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

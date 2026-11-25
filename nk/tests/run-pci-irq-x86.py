#!/usr/bin/env python3
"""Verify actual EDU MSI delivery and timer routing, then ring-3 userland."""
import argparse
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True,
                        help="nk built with NK_PCI_IRQ_TEST=1, NK_SMP=1, NK_AP_LAYER=4")
    parser.add_argument("--output", type=Path, default=Path("target-pci-irq"))
    args = parser.parse_args()
    runner = Path(__file__).with_name("run-epoll-x86.py")
    cases = [
        ("ecam", [], ["PIT IRQ0 -> GSI 2 vector 0x40", "functions via ECAM"]),
        ("sparse", ["--smp", "6,sockets=2,cores=3,threads=1"],
         ["cpu5 online (apic 6)", "6 CPUs total", "PIT IRQ0 -> GSI 2 vector 0x40"]),
        ("cf8-pic", ["--machine", "q35,acpi=off"],
         ["BSP only, legacy PCI config", "functions via CF8/CFC"]),
    ]
    for name, options, markers in cases:
        out = args.output.resolve() / name
        subprocess.run([sys.executable, str(runner), "--kernel", str(args.kernel.resolve()),
                        "--output", str(out), "--qemu-arg=-device", "--qemu-arg=edu"] + options, check=True)
        log = (out / "serial.log").read_text(errors="replace")
        for marker in markers + ["PCI_MSI_PASS", "PCI_MSIX_PASS", "3 hardware interrupts acknowledged; config restored", "timer:  ticking"]:
            if marker not in log:
                raise SystemExit(f"FAIL {name}: missing {marker!r}")
        if name == "cf8-pic" and "PIT IRQ0 -> GSI" in log:
            raise SystemExit("FAIL cf8-pic: unexpected IOAPIC route without ACPI")
        print(f"PCI_IRQ_PASS {name}", flush=True)


if __name__ == "__main__":
    main()

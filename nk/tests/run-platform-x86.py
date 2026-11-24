#!/usr/bin/env python3
"""Verify ACPI/PCI inventory and sparse CPU topology alongside ring-3 epoll."""
import argparse
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("target-platform"))
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    disk = out / "nvme.raw"
    # A throwaway test device. Inventory must not write to it.
    if not disk.exists():
        with disk.open("wb") as stream:
            stream.truncate(32 * 1024 * 1024)
    runner = Path(__file__).with_name("run-epoll-x86.py")
    cases = [
        ("q35-nvme", [], ["MCFG segment 0", "functions via ECAM", "class 01:08:02", "0000:01:00.0", "4 CPUs total"]),
        ("sparse", ["--smp", "6,sockets=2,cores=3,threads=1"],
         ["cpu3 online (apic 4)", "cpu4 online (apic 5)", "cpu5 online (apic 6)", "6 CPUs total"]),
        ("eight", ["--smp", "8"], ["cpu7 online (apic 7)", "8 CPUs total"]),
        ("legacy", ["--machine", "q35,acpi=off"], ["BSP only, legacy PCI config", "functions via CF8/CFC", "1 CPUs total"]),
    ]
    for name, options, expected in cases:
        if name == "q35-nvme":
            for value in ["-device", "pcie-root-port,id=rp1,chassis=1,slot=1", "-drive",
                          f"file={disk},if=none,id=nvm,format=raw,readonly=on", "-device",
                          "nvme,drive=nvm,serial=nk-test,bus=rp1"]:
                options.append("--qemu-arg=" + value)
        subprocess.run([sys.executable, str(runner), "--kernel", str(args.kernel.resolve()),
                        "--output", str(out / name)] + options, check=True)
        log = (out / name / "serial.log").read_text(errors="replace")
        for marker in expected:
            if marker not in log:
                raise SystemExit(f"FAIL {name}: missing {marker!r}")
        print(f"PLATFORM_PASS {name}", flush=True)


if __name__ == "__main__":
    main()

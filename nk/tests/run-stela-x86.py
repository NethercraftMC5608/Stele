#!/usr/bin/env python3
"""Verify trusted ARM64 blocks execute inside nk, then userland epoll still runs."""
import argparse
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True, help="nk built with --features stela")
    parser.add_argument("--output", type=Path, default=Path("target-stela-probe"))
    args = parser.parse_args()
    subprocess.run([sys.executable, str(Path(__file__).with_name("run-epoll-x86.py")),
                    "--kernel", str(args.kernel.resolve()), "--output", str(args.output.resolve())], check=True)
    log = (args.output / "serial.log").read_text(errors="replace")
    markers = ("STELA_EXEC_BEGIN", "STELA_EXEC_REGISTERS_OK", "STELA_EXEC_BRANCH_OK",
               "STELA_EXEC_MEMORY_OK", "STELA_EXEC_FLAGS_SIMD_OK", "STELA_EXEC_PASS",
               "STELA_ENGINE_BEGIN", "STELA_ENGINE_PASS", "STELA_MMIO_BEGIN",
               "STELA_MMIO_UART_OK", "STELA_GUEST_ABORT_OK", "STELA_SHADOW_BEGIN",
               "STELA_RAM_SPARSE_OK", "STELA_RAM_DMA_OK", "STELA_SHADOW_RAM_OK",
               "STELA_SHADOW_PERMISSION_OK", "STELA_SHADOW_UNMAP_OK",
               "STELA_SHADOW_CONTEXT_OK", "STELA_SHADOW_ASID_REUSE_OK", "STELA_SHADOW_MMIO_OK", "STELA_SHADOW_PASS",
               "STELA_RAM_CANONICAL_OK", "STELA_RAM_AUTHORITY_OK",
               "STELA_MMIO_PASS", "EPOLL_PASS")
    positions = [log.find(marker) for marker in markers]
    if any(pos < 0 for pos in positions) or positions != sorted(positions):
        parser.exit(1, "FAIL: missing or out-of-order translated execution markers\n")
    if "int3 entered and returned" not in log:
        parser.exit(1, "FAIL: ordinary nk breakpoint trap did not complete\n")
    print("STELA_QEMU_PASS: sparse/canonical RAM, model DMA, engine, TTBR/ASID, UART MMIO, shadow permissions and Linux epoll")


if __name__ == "__main__":
    main()

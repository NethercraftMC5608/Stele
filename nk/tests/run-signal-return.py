#!/usr/bin/env python3
"""Boot a libc-free arm64/x86-64 signal-return probe on nk + LKL in QEMU."""
import argparse
from pathlib import Path
import runpy
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=("aarch64", "x86_64"), required=True)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("target-signal-return"))
    parser.add_argument("--timeout", type=float, default=60)
    args = parser.parse_args()
    kernel = args.kernel.resolve(strict=True)
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
    bins = list(sysroot.glob("lib/rustlib/*/bin/rust-lld"))
    if len(bins) != 1:
        parser.error("install rustup's llvm-tools component")
    source = Path(__file__).resolve().parents[1] / "init/signal-return.c"
    flags = ["-mgeneral-regs-only"] if args.arch == "aarch64" else ["-mno-red-zone"]
    subprocess.run(["clang", f"--target={args.arch}-unknown-linux-gnu", "-O2", "-Wall", "-Wextra", "-Werror",
                    "-ffreestanding", "-fno-stack-protector", "-fno-pic"] + flags + ["-c",
                    str(source), "-o", str(out / "probe.o")], check=True)
    emulation = "aarch64elf" if args.arch == "aarch64" else "elf_x86_64"
    subprocess.run([str(bins[0]), "-flavor", "gnu", "-m", emulation, "-static", "--build-id=none",
                    "--image-base=0x400000", "-e", "_start", "-o", str(out / "nk-init"),
                    str(out / "probe.o")], check=True)
    if args.arch == "aarch64":
        subprocess.run([str(bins[0].with_name("llvm-objcopy")), "-O", "binary", str(kernel), str(out / "nk.bin")], check=True)
        kernel = out / "nk.bin"
    newc = runpy.run_path(str(Path(__file__).with_name("run-epoll-x86.py")))["newc"]
    initrd = out / "signals.cpio"
    initrd.write_bytes(newc("nk-init", (out / "nk-init").read_bytes(), 0o100755) + newc("TRAILER!!!", b"", 0))
    log = out / "serial.log"
    prefix = "ARM_SIGNAL" if args.arch == "aarch64" else "X86_SIGNAL"
    passed = False
    with log.open("wb") as stream:
        if args.arch == "aarch64":
            machine = ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-cpu", "cortex-a72", "-m", "2G", "-smp", "1"]
        else:
            machine = ["qemu-system-x86_64", "-M", "q35", "-m", "4G", "-smp", "4"]
        process = subprocess.Popen(machine + ["-kernel", str(kernel), "-initrd", str(initrd),
            "-display", "none", "-serial", "stdio", "-monitor", "none", "-no-reboot"],
            stdout=stream, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + args.timeout
            while time.monotonic() < deadline:
                text = log.read_text(errors="replace")
                if any(x in text for x in (prefix + "_FAIL", "kernel panic", "no init to run:")):
                    break
                if prefix + "_PASS" in text:
                    passed = True
                    break
                if process.poll() is not None:
                    break
                time.sleep(0.1)
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
    text = log.read_text(errors="replace")
    print("\n".join(line for line in text.splitlines() if line.startswith(prefix) or "linked address" in line)
          if passed else "\n".join(text.splitlines()[-35:]))
    print(f"{'PASS' if passed else 'FAIL'}: serial log: {log}")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())

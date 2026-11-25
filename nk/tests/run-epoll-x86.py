#!/usr/bin/env python3
"""Build a libc-free epoll /nk-init and verify its serial markers in QEMU."""
import argparse
from pathlib import Path
import shutil
import subprocess
import time


def newc(name, data, mode):
    name = name.encode() + b"\0"
    fields = [1, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(name), 0]
    entry = b"070701" + b"".join(f"{v:08x}".encode() for v in fields) + name
    entry += b"\0" * (-len(entry) % 4)
    entry += data
    return entry + b"\0" * (-len(entry) % 4)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("target-epoll"))
    parser.add_argument("--clang", default=shutil.which("clang"))
    parser.add_argument("--linker", help="ELF linker; defaults to Rust's bundled rust-lld")
    parser.add_argument("--qemu", default="qemu-system-x86_64")
    parser.add_argument("--machine", default="q35")
    parser.add_argument("--smp", default="4")
    parser.add_argument("--qemu-arg", action="append", default=[])
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--source", type=Path,
                        help="another freestanding /nk-init (default init/epoll-x86.c)")
    parser.add_argument("--marker", default="EPOLL",
                        help="its marker prefix: <MARKER>_PASS / <MARKER>_FAIL")
    parser.add_argument("--expect", action="append", default=None,
                        help="markers required with the PASS line")
    args = parser.parse_args()
    kernel = args.kernel.resolve(strict=True)
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    linker = args.linker
    if not linker:
        sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
        candidates = list(sysroot.glob("lib/rustlib/*/bin/rust-lld"))
        if len(candidates) != 1:
            parser.error("pass --linker with the path to an ELF lld")
        linker = str(candidates[0])
    source = (args.source.resolve(strict=True) if args.source
              else Path(__file__).resolve().parents[1] / "init/epoll-x86.c")
    marker = args.marker
    expect = args.expect if args.expect is not None else (
        ["EPOLL_WAIT_OK", "EPOLL_PWAIT_OK", "EPOLL_PWAIT2_OK"] if marker == "EPOLL" else [])
    subprocess.run([args.clang, "--target=x86_64-unknown-linux-gnu", "-O2", "-Wall", "-Wextra",
                    "-Werror", "-ffreestanding", "-fno-stack-protector", "-fno-pie", "-mno-red-zone",
                    "-c", str(source), "-o", str(out / "probe.o")], check=True)
    subprocess.run([linker, "-flavor", "gnu", "-static", "--build-id=none", "--image-base=0x400000", "-e", "_start",
                    "-o", str(out / "nk-init"), str(out / "probe.o")], check=True)
    initrd = out / "epoll.cpio"
    initrd.write_bytes(newc("nk-init", (out / "nk-init").read_bytes(), 0o100755)
                       + newc("TRAILER!!!", b"", 0))
    log = out / "serial.log"
    cmd = [args.qemu, "-M", args.machine, "-m", "4G", "-smp", args.smp, "-kernel", str(kernel),
           "-initrd", str(initrd), "-display", "none", "-serial", "stdio", "-monitor", "none", "-no-reboot"]
    cmd.extend(args.qemu_arg)
    passed = False
    with log.open("wb") as stream:
        process = subprocess.Popen(cmd, stdout=stream, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + args.timeout
            while time.monotonic() < deadline:
                text = log.read_text(errors="replace")
                if any(m in text for m in (f"{marker}_FAIL", "PCI_IRQ_FAIL", "kernel panic", "PANIC", "no init to run:")):
                    break
                if f"{marker}_PASS\n" in text:
                    passed = all(m in text for m in expect)
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
    print("\n".join(text.splitlines()[-35:]) if not passed else
          "\n".join(line for line in text.splitlines() if line.startswith(f"{marker}_") or line.startswith("phase")))
    print(f"{'PASS' if passed else 'FAIL'}: serial log: {log}")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())

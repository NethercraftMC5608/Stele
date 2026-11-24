#!/usr/bin/env python3
"""Build a glibc/Python initrd from cached NETHOS amd64 .npk packages and boot nk."""
import argparse
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ldk"))
import lkl_abi

PACKAGES = ("libc6-", "libpython3.13-minimal-", "libpython3.13-stdlib-",
            "python3.13-minimal-", "libexpat1-", "zlib1g-", "libffi8-", "libbz2-", "liblzma5-")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--lkl-lib", type=Path, required=True, help="the LKL archive linked into this kernel")
    parser.add_argument("--packages", type=Path, required=True)
    parser.add_argument("--busybox", type=Path, required=True, help="static Linux x86-64 busybox")
    parser.add_argument("--output", type=Path, default=Path("target-dynamic"))
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--threads", action="store_true", help="also exercise eight pthread-backed Python threads and glibc TLS")
    args = parser.parse_args()
    kernel = args.kernel.resolve(strict=True)
    try:
        lkl_abi.verify(args.lkl_lib.read_bytes())
    except (OSError, lkl_abi.Invalid, UnicodeError) as error:
        parser.exit(1, f"LKL_ABI_FAIL {error}\nRebuild LKL before running dynamic userland.\n")
    print("LKL_ABI_PASS", flush=True)
    out = args.output.resolve()
    root = out / "rootfs"
    root.mkdir(parents=True, exist_ok=True)
    selected = []
    for prefix in PACKAGES:
        paths = list(args.packages.glob(prefix + "*.amd64.npk"))
        if len(paths) != 1:
            parser.error(f"expected one {prefix}*.amd64.npk, got {len(paths)}")
        selected.append(paths[0].name)
        subprocess.run(["bsdtar", "-xf", str(paths[0].resolve()), "-C", str(root)], check=True)
    (out / "packages.txt").write_text("\n".join(selected) + "\n")
    for name, target in (("lib", "usr/lib"), ("lib64", "usr/lib64")):
        path = root / name
        if not path.exists() and not path.is_symlink():
            path.symlink_to(target)
    (root / "bin").mkdir(exist_ok=True)
    shutil.copyfile(args.busybox, root / "bin/busybox")
    (root / "bin/busybox").chmod(0o755)
    (root / "probe.py").write_text('''print(1)
print("PYTHON_PRINT_OK", flush=True)
import ctypes
m = ctypes.CDLL("libm.so.6")
m.cos.argtypes = [ctypes.c_double]
m.cos.restype = ctypes.c_double
assert m.cos(0.0) == 1.0
print("PYTHON_DLOPEN_OK", flush=True)
''')
    if args.threads:
        with (root / "probe.py").open("a") as probe:
            probe.write('''
import threading
c = ctypes.CDLL("libc.so.6")
c.__errno_location.restype = ctypes.POINTER(ctypes.c_int)
c.usleep.argtypes = [ctypes.c_uint]
c.usleep.restype = ctypes.c_int
gate = threading.Barrier(9, timeout=15)
lock = threading.Lock()
local = threading.local()
addresses, errors = [], []
counter = 0
def worker(index):
    global counter
    try:
        local.index = index
        ptr = c.__errno_location()
        ptr[0] = 1000 + index
        with lock:
            addresses.append(ctypes.addressof(ptr.contents))
        gate.wait()
        for _ in range(20):
            assert c.usleep(1000) == 0
            assert ptr[0] == 1000 + index
            assert local.index == index
            with lock:
                counter += 1
    except BaseException as error:
        with lock:
            errors.append(repr(error))
threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
for thread in threads:
    thread.start()
gate.wait()
for thread in threads:
    thread.join(timeout=15)
assert not any(thread.is_alive() for thread in threads), "join stalled"
assert not errors, errors
assert len(set(addresses)) == 8, addresses
assert counter == 160, counter
print("PYTHON_THREADS_OK", flush=True)
''')
    (root / "nk-init").write_text('''#!/bin/busybox sh
export PATH=/bin:/usr/bin
/bin/busybox mknod -m 666 /dev/urandom c 1 9 || exit 1
/usr/bin/python3.13 -I -S -u /probe.py
if [ "$?" = 0 ]; then echo DYNAMIC_PASS; else echo DYNAMIC_FAIL; fi
exec /bin/busybox sleep 3600
''')
    (root / "nk-init").chmod(0o755)
    newc = runpy.run_path(str(Path(__file__).with_name("run-epoll-x86.py")))["newc"]
    archive = bytearray()
    for path in sorted(root.rglob("*")):
        data = (str(path.readlink()).encode() if path.is_symlink() else
                path.read_bytes() if path.is_file() else b"")
        archive += newc(path.relative_to(root).as_posix(), data, path.lstat().st_mode)
    archive += newc("TRAILER!!!", b"", 0)
    initrd = out / "initrd.cpio"
    initrd.write_bytes(archive)
    log = out / "serial.log"
    passed = False
    with log.open("wb") as stream:
        process = subprocess.Popen(["qemu-system-x86_64", "-M", "q35", "-m", "4G", "-smp", "4",
            "-kernel", str(kernel), "-initrd", str(initrd), "-display", "none", "-serial", "stdio",
            "-monitor", "none", "-no-reboot"], stdout=stream, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + args.timeout
            while time.monotonic() < deadline:
                text = log.read_text(errors="replace")
                if any(x in text for x in ("DYNAMIC_FAIL", "kernel panic", "no init to run:")):
                    break
                if "DYNAMIC_PASS" in text:
                    passed = all(x in text for x in ("PYTHON_PRINT_OK", "PYTHON_DLOPEN_OK"))
                    if args.threads:
                        passed = passed and "PYTHON_THREADS_OK" in text
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
    print("\n".join(log.read_text(errors="replace").splitlines()[-35:]))
    print(f"{'PASS' if passed else 'FAIL'}: serial log: {log}")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())

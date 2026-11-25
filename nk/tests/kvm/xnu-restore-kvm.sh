#!/bin/sh
# Boot macOS 27's restore ramdisk on nk under QEMU/KVM, the bundle read from
# an NVMe disk image (restore-bundle.img), and stream the console.
set -eu
cd "$(dirname "$0")"
OUT=run-restore; LOG=$OUT/serial.log
mkdir -p "$OUT"; rm -f "$LOG"
podman run --rm --replace --name nk-xnu-restore --device /dev/kvm -v "$PWD":/w:Z -w /w localhost/nk-qemu \
  python3 tests/run-dedicated-x86.py --kernel nk-restore --initrd desktop-x86.cpio \
  --disk restore-bundle.img --accel kvm --cpu host --memory 12G \
  --require STELA_SPTM_PASS --output "$OUT" --timeout 4200 > "$OUT/runner.txt" 2>&1 &
trap 'podman stop -t 2 nk-xnu-restore >/dev/null 2>&1; exit 0' INT TERM
echo "booting nk + LKL under KVM (restore ramdisk)..."
n=0; while [ ! -f "$LOG" ]; do sleep 1; n=$((n+1)); if [ $n -ge 60 ]; then echo "QEMU never started:"; cat "$OUT/runner.txt"; exit 1; fi; done
tail -n +1 -f "$LOG" | while IFS= read -r line; do
  case "$line" in
    GUEST\|*) printf '%s\n' "${line#GUEST| }" ;;
    STELA_SPTM:\ ...*) printf '\033[90m[nk] %s\033[0m\n' "${line#STELA_SPTM: ... }" ;;
    STELA_SPTM:\ stop*|STELA_SPTM_*|*panicked*) printf '\033[33m[nk] %s\033[0m\n' "$line" ;;
    STELA_SPTM:*|*nvme*) printf '\033[90m[nk] %s\033[0m\n' "${line#STELA_SPTM: }" ;;
  esac
done

#!/bin/sh
# Boot macOS 27's kernel on nk under QEMU/KVM in a Fedora container, and
# stream its console. Run on the Bazzite PC from ~/nk-xnu.
set -eu
cd "$(dirname "$0")"
OUT=run; LOG=$OUT/serial.log
mkdir -p "$OUT"; rm -f "$LOG"
if ! podman image exists localhost/nk-qemu; then
  echo "building the QEMU container (once)..."
  printf 'FROM registry.fedoraproject.org/fedora:43\nRUN dnf -y install qemu-system-x86-core python3 && dnf clean all\n' \
    | podman build -q -t nk-qemu -f - . >/dev/null
fi
set --
for f in bundle.json devicetree.adt USB-C_HPM,46-P01-AP.bin boot-manifest.im4m txm.img \
         trustcache.bin kernelcache.img sptm.img bootargs.bin; do
  set -- "$@" --add "bundle/${f}:stele/${f}"
done
podman run --rm --replace --name nk-xnu --device /dev/kvm -v "$PWD":/w:Z -w /w localhost/nk-qemu \
  python3 tests/run-dedicated-x86.py --kernel nk --initrd desktop-x86.cpio "$@" \
  --accel kvm --cpu host --memory 6G --require STELA_SPTM_PASS --output "$OUT" --timeout 4200 \
  > "$OUT/runner.txt" 2>&1 &
trap 'podman stop -t 2 nk-xnu >/dev/null 2>&1; exit 0' INT TERM
echo "booting nk + LKL under KVM..."
n=0; while [ ! -f "$LOG" ]; do sleep 1; n=$((n+1)); if [ $n -ge 60 ]; then echo "QEMU never started:"; cat "$OUT/runner.txt"; exit 1; fi; done
tail -n +1 -f "$LOG" | while IFS= read -r line; do
  case "$line" in
    GUEST\|*) printf '%s\n' "${line#GUEST| }" ;;
    STELA_SPTM:\ ...*) printf '\033[90m[nk] %s\033[0m\n' "${line#STELA_SPTM: ... }" ;;
    STELA_SPTM:\ stop*|STELA_SPTM_*|*panicked*) printf '\033[33m[nk] %s\033[0m\n' "$line" ;;
    STELA_SPTM:*) printf '\033[90m[nk] %s\033[0m\n' "${line#STELA_SPTM: }" ;;
  esac
done

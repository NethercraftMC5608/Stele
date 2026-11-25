#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-or-later
# Boot macOS 27's kernel on nk in QEMU and stream its console live.
#
#   tests/xnu-live.sh [BUNDLE] [SECONDS]
#
# BUNDLE: a `stela-dev boot prepare` directory (default: the Mac14,2 bundle in
# ~/Stele-assets prepared with --dram-size 0x80000000 and -noprogress).
# SECONDS: how long nk runs the guest (default 3600).
# XNU's lines are shown as they arrive; nk's progress lines (blocks, PC) every
# 10 s in grey. Ctrl-C stops QEMU.
set -eu
cd "$(dirname "$0")/.."
BUNDLE=${1:-$HOME/Stele-assets/bundle-Mac14,2-26B131-nk2g-np}
SECONDS_TO_RUN=${2:-3600}
OUT=target-xnu-live
LOG=$OUT/serial.log
mkdir -p "$OUT"

echo "building nk (guest run up to ${SECONDS_TO_RUN}s)..."
NK_STELA_SPTM_SECONDS=$SECONDS_TO_RUN NK_STELA_SPTM_TEST=1 NK_SMP=1 NK_AP_LAYER=4 \
  NK_LKL_LIB="$PWD/ldk/build/lkl-x86_64/libnklkl.a" \
  cargo build --release --features stela --target x86_64-unknown-none \
  --target-dir target-xnu-live-build 2>&1 | grep -E "^error|Finished" || true

set --
for f in bundle.json devicetree.adt "USB-C_HPM,46-P01-AP.bin" boot-manifest.im4m txm.img \
         trustcache.bin kernelcache.img sptm.img bootargs.bin; do
  set -- "$@" --add "${BUNDLE}/${f}:stele/${f}"
done
rm -f "$LOG"
python3 tests/run-dedicated-x86.py \
  --kernel target-xnu-live-build/x86_64-unknown-none/release/nk \
  --initrd "$HOME/nk-x86-harness/desktop-x86.cpio" "$@" \
  --cpu max --memory 6G --require STELA_SPTM_PASS --output "$OUT" \
  --timeout $((SECONDS_TO_RUN + 600)) > "$OUT/runner.txt" 2>&1 &
RUNNER=$!
trap 'kill $RUNNER 2>/dev/null; pkill -f "file:$PWD/$LOG" 2>/dev/null; exit 0' INT TERM

echo "booting nk + LKL, loading the bundle (about a minute before XNU starts)..."
while [ ! -f "$LOG" ]; do sleep 1; done
tail -n +1 -f "$LOG" | while IFS= read -r line; do
  case "$line" in
    GUEST\|*) printf '%s\n' "${line#GUEST| }" ;;
    STELA_SPTM:\ ...*) printf '\033[90m[nk] %s\033[0m\n' "${line#STELA_SPTM: ... }" ;;
    STELA_SPTM:\ stop*|STELA_SPTM_*|*panicked*) printf '\033[33m[nk] %s\033[0m\n' "$line" ;;
    STELA_SPTM:*) printf '\033[90m[nk] %s\033[0m\n' "${line#STELA_SPTM: }" ;;
  esac
done

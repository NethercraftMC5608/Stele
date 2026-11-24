# The machine ldk compiles Linux drivers on.
#
# In a container because the Linux source tree cannot live on macOS at all:
# it contains filenames that differ only in case (xt_CONNMARK.h beside
# xt_connmark.h, among others), and a case-insensitive APFS volume silently
# loses one of every such pair on extraction. The same reason the x86 kernel
# build already keeps its tree in the `nethos-kernel-src` docker volume rather
# than on a bind mount -- see payload/bin/nethos-kernel.
#
# arm64 host, so the native compiler already targets aarch64 and no
# CROSS_COMPILE is needed for it. x86-64 is cross-compiled from the same
# arm64 image built with CROSS=amd64 (see below and ldk.py's cmd_lkl).
FROM debian:trixie-slim

# CROSS=amd64 adds the x86-64 cross toolchain, so ldk builds the x86 LKL
# archive natively on arm64 instead of compiling it under amd64 emulation
# (which was several times slower and segfaulted gcc at random).
ARG CROSS=
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential flex bison bc libssl-dev libelf-dev \
        python3 make perl xz-utils cpio kmod rsync \
        ${CROSS:+crossbuild-essential-$CROSS} \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

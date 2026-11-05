#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.
#
# Usage: build-prebuilt.sh <libcamera-tag> <out-dir> [cross-triplet]
#
# Builds the portable prebuilt shim for one libcamera minor, as the release
# ships it, inside Debian bookworm (GCC 12, glibc 2.36):
#  1. libcamera core at <libcamera-tag> (an upstream vX.Y.0 tag), with no
#     pipelines or IPAs, for its headers and the libraries to link against;
#  2. the shim against it;
#  3. the import gate (check-symbols.sh) against that same libcamera.
# Writes <out-dir>/libcamera-0.<minor>/libedgefirst_camera_libcamera.so.1.
#
# Run it as root in a debian:bookworm container with this directory
# mounted. With [cross-triplet] (aarch64-linux-gnu) it cross-builds;
# otherwise it builds for the container's own architecture.
set -eu
TAG=$1
OUT=$(realpath -m "$2")
CROSS=${3:-}
SHIM=$(cd "$(dirname "$0")" && pwd)
WORK=${WORK:-/tmp/efcam-lc-build}
LIBCAMERA_GIT=${LIBCAMERA_GIT:-https://git.libcamera.org/libcamera/libcamera.git}

export DEBIAN_FRONTEND=noninteractive
packages="git ca-certificates meson ninja-build pkg-config python3 python3-yaml python3-jinja2 python3-ply binutils"
if [ -n "$CROSS" ]; then
    arch=$(echo "$CROSS" | sed 's/-linux-gnu//; s/aarch64/arm64/; s/x86_64/amd64/')
    dpkg --add-architecture "$arch"
    packages="$packages g++-$CROSS binutils-$CROSS libyaml-dev:$arch"
else
    packages="$packages g++ libyaml-dev"
fi
apt-get update -qq
# shellcheck disable=SC2086
apt-get install -y -qq --no-install-recommends $packages >/dev/null

rm -rf "$WORK"
mkdir -p "$WORK"
git -c advice.detachedHead=false clone -q --depth 1 --branch "$TAG" "$LIBCAMERA_GIT" "$WORK/libcamera"

cross_args=""
strip=strip
if [ -n "$CROSS" ]; then
    cat >"$WORK/cross.ini" <<EOF
[binaries]
c = '$CROSS-gcc'
cpp = '$CROSS-g++'
ar = '$CROSS-ar'
strip = '$CROSS-strip'
pkgconfig = 'pkg-config'

[properties]
pkg_config_libdir = ['/usr/lib/$CROSS/pkgconfig', '/usr/share/pkgconfig']

[host_machine]
system = 'linux'
cpu_family = '$(echo "$CROSS" | cut -d- -f1)'
cpu = '$(echo "$CROSS" | cut -d- -f1)'
endian = 'little'
EOF
    cross_args="--cross-file $WORK/cross.ini"
    strip=$CROSS-strip
fi

# shellcheck disable=SC2086
meson setup $cross_args --prefix "$WORK/prefix" --libdir lib \
    -Dpipelines=[] -Dipas=[] -Dcam=disabled -Dqcam=disabled \
    -Dgstreamer=disabled -Dpycamera=disabled -Dv4l2=false \
    -Dlc-compliance=disabled -Ddocumentation=disabled -Dtest=false \
    -Dtracing=disabled -Dudev=disabled \
    "$WORK/libcamera-build" "$WORK/libcamera" >"$WORK/libcamera-setup.log"
ninja -C "$WORK/libcamera-build" install >/dev/null

# shellcheck disable=SC2086
PKG_CONFIG_PATH="$WORK/prefix/lib/pkgconfig" \
    meson setup $cross_args --buildtype release "$WORK/shim-build" "$SHIM" >"$WORK/shim-setup.log"
PKG_CONFIG_PATH="$WORK/prefix/lib/pkgconfig" ninja -C "$WORK/shim-build" >/dev/null

so="$WORK/shim-build/libedgefirst_camera_libcamera.so.1"
"$strip" --strip-unneeded "$so"
version=$(pkg-config --modversion --define-variable=prefix="$WORK/prefix" \
    "$WORK/prefix/lib/pkgconfig/libcamera.pc" 2>/dev/null || echo "${TAG#v}")
minor=$(echo "$version" | cut -d. -f1-2)
"$SHIM/check-symbols.sh" "$so" "$WORK/prefix/lib"

# Natively, load it as the SDK does and start a CameraManager. No pipelines
# are built, so it finds no cameras; in a container without media devices
# it fails to start with -ENODEV, which still proves libcamera was called.
if [ -z "$CROSS" ]; then
    LD_LIBRARY_PATH="$WORK/prefix/lib" python3 - "$so" <<'PY'
import ctypes, sys
lib = ctypes.CDLL(sys.argv[1])
assert lib.efcam_lc_abi_version() == 1, "ABI version"
lib.efcam_lc_build_version.restype = ctypes.c_char_p
mgr = ctypes.c_void_p()
ret = lib.efcam_lc_mgr_create(ctypes.byref(mgr))
assert ret in (0, -19), f"CameraManager start: {ret}"
if ret == 0:
    count = lib.efcam_lc_mgr_camera_count(mgr)
    lib.efcam_lc_mgr_destroy(mgr)
    print(f"smoke: shim {lib.efcam_lc_build_version().decode()} loaded, {count} cameras")
else:
    print(f"smoke: shim {lib.efcam_lc_build_version().decode()} loaded; no media devices here")
PY
fi

mkdir -p "$OUT/libcamera-$minor"
cp "$so" "$OUT/libcamera-$minor/"
echo "built $OUT/libcamera-$minor/libedgefirst_camera_libcamera.so.1 against libcamera $version (${CROSS:-native})"

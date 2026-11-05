#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.
#
# Usage: check-symbols.sh <shim.so> <libcamera-libdir> [max-glibc] [max-glibcxx]
#
# The import gate for a built shim:
#  - it exports only efcam_lc_* symbols, under the EFCAM_LC_1 version node;
#  - every unversioned symbol it imports is exported by the libcamera and
#    libcamera-base found in <libcamera-libdir>, so it loads on that minor;
#  - the glibc and libstdc++ symbol versions it requires are no newer than
#    the oldest supported target (defaults: GLIBC_2.32, GLIBCXX_3.4.30).
set -eu
SHIM=$1
LIBDIR=$2
MAX_GLIBC=${3:-2.32}
MAX_GLIBCXX=${4:-3.4.30}
export LC_ALL=C
fail=0
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

nm -D --defined-only "$SHIM" | awk '$2 ~ /[TDBRVW]/ {print $3}' | sort -u >"$tmp/exports"
if grep -v '^efcam_lc_' "$tmp/exports"; then
    echo "error: exports other than efcam_lc_* (above)" >&2
    fail=1
fi
if ! objdump -T "$SHIM" | grep -q 'EFCAM_LC_1'; then
    echo "error: no EFCAM_LC_1 version node" >&2
    fail=1
fi

nm -D --undefined-only "$SHIM" | awk '$1 == "U" {print $2}' | grep -v '@' | sort -u >"$tmp/imports"
for lib in "$LIBDIR"/libcamera.so.0.* "$LIBDIR"/libcamera-base.so.0.*; do
    [ -f "$lib" ] && nm -D --defined-only "$lib" | awk '{print $3}' | sed 's/@.*//'
done | sort -u >"$tmp/available"
comm -23 "$tmp/imports" "$tmp/available" >"$tmp/missing" || true
if [ -s "$tmp/missing" ]; then
    echo "error: imports $LIBDIR does not export:" >&2
    c++filt <"$tmp/missing" >&2
    fail=1
fi

newest() {
    local lib=$1
    objdump -T "$SHIM" | grep -oE "\\b${lib}_[0-9.]+" | sed "s/^${lib}_//" | sort -Vu | tail -1
}
check_max() {
    local lib=$1 max=$2
    got=$(newest "$lib")
    if [ -n "$got" ] && [ "$(printf '%s\n%s\n' "$got" "$max" | sort -V | tail -1)" != "$max" ]; then
        echo "error: requires ${lib}_$got, newer than ${lib}_$max" >&2
        fail=1
    fi
    echo "$lib: ${got:-none} (max $max)"
}
check_max GLIBC "$MAX_GLIBC"
check_max GLIBCXX "$MAX_GLIBCXX"

echo "exports: $(wc -l <"$tmp/exports"), libcamera imports: $(wc -l <"$tmp/imports")"
NEEDED=$(readelf -d "$SHIM" | sed -n 's/.*NEEDED.*\[\(.*\)\]/\1/p' | tr '\n' ' ')
echo "needed: $NEEDED"
exit $fail

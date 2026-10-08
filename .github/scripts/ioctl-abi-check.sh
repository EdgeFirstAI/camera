#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.
#
# Runs the SDK's vivid tests under strace and fails on an ioctl ABI mismatch.
#
# A V4L2 ioctl number encodes the size of its argument struct, so a struct
# laid out differently from the kernel's yields a number the kernel does not
# know, and the call fails with ENOTTY. The tests alone can miss this when the
# failing call has a fallback. This script fails when any non-terminal ioctl
# returns ENOTTY, unless its name is in ABI_ALLOW_ENOTTY (space-separated), and
# when no VIDIOC_STREAMON succeeded, so a run that captured nothing cannot
# pass. Requests strace cannot decode on the V4L2 type ('V', 0x56) are
# listed as warnings: a newer ioctl than this strace knows, or a wrong size.
#
# Needs vivid loaded (vivid-setup.sh), strace and jq. Usage:
#   .github/scripts/ioctl-abi-check.sh [log-dir]

set -euo pipefail

out="${1:-target/ioctl-abi}"
mkdir -p "$out"
rm -f "$out"/trace.* "$out/strace.log"

bin=$(cargo test --locked -p edgefirst-camera --test v4l2_vivid --no-run \
        --message-format=json |
      jq -r 'select(.reason == "compiler-artifact" and .executable != null
                    and .target.name == "v4l2_vivid") | .executable' | tail -n 1)
if [[ -z "$bin" ]]; then
  echo "::error::could not find the v4l2_vivid test binary"
  exit 1
fi

echo "Tracing $bin"
# One file per thread: with a shared file, a call that blocks while another
# thread runs is split into "<unfinished ...>" and "<... resumed>" lines, and
# the resumed half, which carries the errno, lacks the request name.
EDGEFIRST_CAMERA_REQUIRE_VIVID=1 strace -ff -qq -e trace=ioctl -o "$out/trace" "$bin"
log="$out/strace.log"
cat "$out"/trace.* > "$log"
rm -f "$out"/trace.*
if [[ -n "${ABI_CHECK_INJECT:-}" ]]; then
  echo "$ABI_CHECK_INJECT" >> "$log"
fi

# "ioctl(3, VIDIOC_S_FMT, {...}) = -1 ENOTTY (...)" -> "ENOTTY VIDIOC_S_FMT"
failures=$(sed -nE 's/^ioctl\([0-9]+, (_IOC\([^)]*\)|[A-Z0-9_]+),.* = -1 ([A-Z]+) .*/\2 \1/p' "$log" |
           sort | uniq -c | sort -rn)
streamon=$(grep -cE 'ioctl\([0-9]+, VIDIOC_STREAMON, .*\) = 0$' "$log" || true)
undecoded=$(grep -oE 'ioctl\([0-9]+, _IOC\([^,]+, 0x56, [^)]*\)' "$log" | sed -E 's/^ioctl\([0-9]+, //' |
            sort -u || true)

allow=" ${ABI_ALLOW_ENOTTY:-} "
unexpected=""
while read -r _ errno name; do
  [[ "$errno" == ENOTTY ]] || continue
  [[ "$name" == TC* || "$name" == TIOC* ]] && continue
  [[ "$allow" == *" $name "* ]] && continue
  unexpected+="$name; "
done <<< "$failures"

{
  echo "### ioctl ABI check"
  echo
  echo "Successful VIDIOC_STREAMON calls: $streamon"
  echo
  echo "Failed ioctls (count, errno, request):"
  echo
  echo '```text'
  echo "${failures:-none}"
  echo '```'
} | tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"

status=0
if [[ -n "$undecoded" ]]; then
  while read -r req; do
    echo "::warning::strace could not decode V4L2 request $req"
  done <<< "$undecoded"
fi
if [[ "$streamon" -eq 0 ]]; then
  echo "::error::no VIDIOC_STREAMON succeeded; the trace captured nothing"
  status=1
fi
if [[ -n "$unexpected" ]]; then
  echo "::error::unexpected ENOTTY from: ${unexpected%; } (an ioctl argument layout differs from the kernel's)"
  status=1
fi
exit "$status"

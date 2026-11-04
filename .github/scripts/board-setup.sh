#!/usr/bin/env bash
# Board lane setup: points the board tests at the board's camera and a
# recording, and makes every test skip a failure unless the lane expects it.
#
# Exports through GITHUB_ENV:
#   EDGEFIRST_CAMERA_REQUIRE_BOARD=1  a test that would skip fails instead
#   EDGEFIRST_CAMERA_ALLOW_SKIPS      the skips this board expects (names
#                                     from tests/common/mod.rs, Skip)
#   EDGEFIRST_CAMERA_RECORDING        the checked-out test clip and sidecar,
#                                     for the decode and file-source tests
#
# V4L2 boards keep the tests' default camera. On an i.MX 95 the camera is
# reached through libcamera's nxp/neo pipeline, so this fetches the run's
# prebuilt aarch64 shims (one per libcamera minor; the SDK loads the one
# matching the board's libcamera) and names the first libcamera camera.
#
# Runs as the shared Full workflow's board-pre-command, which provides
# GH_TOKEN and GITHUB_ENV.
set -euo pipefail

recording="$GITHUB_WORKSPACE/tests/data/testsrc-160x120.h264"
if [[ ! -f "$recording" || ! -f "${recording%.h264}.json" ]]; then
  echo "::error::board-setup: $recording or its .json sidecar is missing"
  exit 1
fi

allow=()
compatible=$( { tr '\0' ' ' </proc/device-tree/compatible; } 2>/dev/null || true)
case "$compatible" in
  *fsl,imx95*)
    shims="$RUNNER_TEMP/libcamera-shim"
    rm -rf "$shims"
    mkdir -p "$shims"
    auth=(-H "Authorization: Bearer $GH_TOKEN" -H "Accept: application/vnd.github+json")
    api="$GITHUB_API_URL/repos/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/artifacts?per_page=100"
    curl -fsSL --retry 3 "${auth[@]}" "$api" >"$shims/artifacts.json"
    urls=$(python3 - "$shims/artifacts.json" <<'PY'
import json, sys
for a in json.load(open(sys.argv[1]))["artifacts"]:
    if a["name"].startswith("libcamera-shim-aarch64-"):
        print(a["archive_download_url"])
PY
)
    if [[ -z "$urls" ]]; then
      echo "::error::no libcamera-shim-aarch64-* artifacts in run $GITHUB_RUN_ID"
      exit 1
    fi
    while read -r url; do
      curl -fsSL --retry 3 "${auth[@]}" -o "$shims/shim.zip" "$url"
      python3 -m zipfile -e "$shims/shim.zip" "$shims"
    done <<<"$urls"
    rm -f "$shims/shim.zip" "$shims/artifacts.json"
    find "$shims" -name '*.so.1' | sort

    {
      echo "EDGEFIRST_CAMERA_LIBCAMERA_SHIM_DIR=$shims"
      echo "LIBCAMERA_PIPELINES_MATCH_LIST=nxp/neo"
      echo "EDGEFIRST_CAMERA_SOURCE=libcamera"
    } >>"$GITHUB_ENV"
    # The neo ISP has no vvcam daemon to restart, and validate() turns any
    # orientation back to Rotate0, so it cannot mirror.
    allow+=(not-vvcam no-mirror)
    echo "board-setup: libcamera camera through nxp/neo"
    ;;
  *)
    # The ISP-restart test needs the camera to be the vvcam ISP's node,
    # which the tests recognise by its V4L2 driver name, as here. A board
    # can carry imx8-isp.service and still capture through the ISI.
    camera=${EDGEFIRST_CAMERA_SOURCE:-/dev/video3}
    driver=$(python3 - "$camera" <<'PY' || true
import fcntl, sys
VIDIOC_QUERYCAP = 0x80685600  # _IOR('V', 0, struct v4l2_capability)
buf = bytearray(104)
with open(sys.argv[1], "rb", buffering=0) as dev:
    fcntl.ioctl(dev, VIDIOC_QUERYCAP, buf)
print(buf[:16].split(b"\0")[0].decode())
PY
)
    if [[ "$driver" != *viv* ]]; then
      allow+=(not-vvcam)
      echo "::notice::board-setup: $camera is driver '${driver:-unknown}', not the vvcam ISP; the ISP-restart test is expected to skip"
    fi
    echo "board-setup: V4L2 camera"
    ;;
esac

skips=$(IFS=,; echo "${allow[*]:-}")
{
  echo "EDGEFIRST_CAMERA_REQUIRE_BOARD=1"
  echo "EDGEFIRST_CAMERA_ALLOW_SKIPS=$skips"
  echo "EDGEFIRST_CAMERA_RECORDING=$recording"
} >>"$GITHUB_ENV"
echo "board-setup: skips allowed: ${skips:-none}; recording $recording"

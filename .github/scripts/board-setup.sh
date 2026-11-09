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
#   EDGEFIRST_CAMERA_SOURCE, _SIZE, _FPS  the camera and what to request
#                                     from it, where the defaults
#                                     (/dev/video3, 1080p30) do not fit
#
# i.MX V4L2 boards keep the tests' default camera, and the IQ-9075 EVK uses
# its USB webcam. On an i.MX 95 the camera is reached through libcamera's
# nxp/neo pipeline, and on a Raspberry Pi 5 through Raspberry Pi's
# libcamera, so this fetches the run's prebuilt aarch64 shims (one per
# libcamera minor, plus Raspberry Pi's build; the SDK loads the one matching
# the board's libcamera) and names the first libcamera camera.
#
# Runs as the shared Full workflow's board-pre-command, which provides
# GH_TOKEN and GITHUB_ENV.
set -euo pipefail

recording="$GITHUB_WORKSPACE/tests/data/testsrc-160x120.h264"
if [[ ! -f "$recording" || ! -f "${recording%.h264}.json" ]]; then
  echo "::error::board-setup: $recording or its .json sidecar is missing"
  exit 1
fi

# Fetches this run's prebuilt aarch64 libcamera shims, one per libcamera
# minor plus Raspberry Pi's build, into $shims for the SDK to choose from.
fetch_shims() {
  shims="$RUNNER_TEMP/libcamera-shim"
  rm -rf "$shims"
  mkdir -p "$shims"
  auth=(-H "Authorization: Bearer $GH_TOKEN" -H "Accept: application/vnd.github+json")
  api="$GITHUB_API_URL/repos/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/artifacts?per_page=100"
  curl -fsSL --proto '=https' --tlsv1.2 --retry 3 "${auth[@]}" "$api" >"$shims/artifacts.json"
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
    curl -fsSL --proto '=https' --tlsv1.2 --retry 3 "${auth[@]}" -o "$shims/shim.zip" "$url"
    python3 -m zipfile -e "$shims/shim.zip" "$shims"
  done <<<"$urls"
  rm -f "$shims/shim.zip" "$shims/artifacts.json"
  find "$shims" -name '*.so.1' | sort
}

allow=()
compatible=$( { tr '\0' ' ' </proc/device-tree/compatible; } 2>/dev/null || true)
case "$compatible" in
  *fsl,imx95*)
    fetch_shims

    {
      echo "EDGEFIRST_CAMERA_LIBCAMERA_SHIM_DIR=$shims"
      echo "LIBCAMERA_PIPELINES_MATCH_LIST=nxp/neo"
      echo "EDGEFIRST_CAMERA_SOURCE=libcamera"
    } >>"$GITHUB_ENV"
    # The neo ISP has no vvcam daemon to restart, and validate() turns any
    # orientation back to Rotate0, so it cannot mirror. libcamera allocates
    # its own buffers, so the SDK's memory order does not apply.
    allow+=(not-vvcam no-mirror not-v4l2)
    echo "board-setup: libcamera camera through nxp/neo"
    ;;
  *raspberrypi,5-model-b*)
    fetch_shims
    {
      echo "EDGEFIRST_CAMERA_LIBCAMERA_SHIM_DIR=$shims"
      echo "EDGEFIRST_CAMERA_SOURCE=libcamera"
    } >>"$GITHUB_ENV"
    # PiSP has no vvcam daemon, and libcamera allocates its own buffers.
    # The Raspberry Pi 5 has no H.264 encoder or decoder.
    allow+=(not-vvcam not-v4l2 no-encoder no-decoder)
    echo "board-setup: libcamera camera through the Raspberry Pi pipeline"
    ;;
  *qcom,qcs9075*)
    # The IQ-9075 EVK's camera is a USB webcam. It resets about 30 s after
    # boot and comes back under a new node number, so wait that out and use
    # its stable by-id node. Its YUYV modes above 640x480 run at 5 fps.
    while (( $(cut -d. -f1 /proc/uptime) < 60 )); do sleep 2; done
    camera=""
    for _ in $(seq 1 30); do
      camera=$(find /dev/v4l/by-id -name '*-video-index0' 2>/dev/null | sort | head -1)
      [[ -n "$camera" ]] && break
      sleep 2
    done
    if [[ -z "$camera" ]]; then
      echo "::error::board-setup: no USB camera under /dev/v4l/by-id"
      exit 1
    fi
    {
      echo "EDGEFIRST_CAMERA_SOURCE=$camera"
      echo "EDGEFIRST_CAMERA_SIZE=640x480"
      echo "EDGEFIRST_CAMERA_FPS=30"
    } >>"$GITHUB_ENV"
    # No vvcam ISP and no CMA heap, and the webcam cannot mirror; the H.264
    # encoder takes NV12 only and the webcam delivers YUYV.
    allow+=(not-vvcam no-mirror no-cma-heap encoder-format)
    echo "board-setup: USB camera $camera"
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
    # The OV5640 stops streaming after a runtime-PM suspend and resume,
    # and only a reboot brings it back: keep it powered. A sensor that has
    # already stopped is reported by the mirror test, which shows its
    # colour bars.
    for name in /sys/bus/i2c/drivers/ov5640/*/name; do
      [[ -e "$name" ]] || continue
      sensor=$(dirname "$name")
      echo on >"$sensor/power/control"
      echo "board-setup: $(basename "$sensor") (OV5640) kept powered; runtime PM was $(cat "$sensor/power/runtime_status")"
    done
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

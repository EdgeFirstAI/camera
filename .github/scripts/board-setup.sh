#!/usr/bin/env bash
# Board lane setup: points the board tests at the board's camera.
#
# V4L2 boards keep the tests' default camera. On an i.MX 95 the camera is
# reached through libcamera's nxp/neo pipeline, so this fetches the run's
# prebuilt aarch64 shims (one per libcamera minor; the SDK loads the one
# matching the board's libcamera) and names the first libcamera camera.
#
# Runs as the shared Full workflow's board-pre-command, which provides
# GH_TOKEN and GITHUB_ENV.
set -euo pipefail

compatible=$(tr '\0' ' ' </proc/device-tree/compatible 2>/dev/null || true)
case "$compatible" in
  *fsl,imx95*) ;;
  *) echo "board-setup: V4L2 camera"; exit 0 ;;
esac

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
echo "board-setup: libcamera camera through nxp/neo"

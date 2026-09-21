#!/usr/bin/env bash
# On-target integration: unpack the instrumented archive, smoke-test the binary,
# and run ignored test_image integration tests (legacy test.yml hardware phase).
set -euo pipefail

mkdir -p coverage/profraw coverage/test-output board-archive board-extract board-tmp
export TMPDIR="${GITHUB_WORKSPACE}/board-tmp"

# board-run skips download-artifact when board-command is set; fetch the archive here.
if [[ -z "$(find board-archive -name 'nextest-archive.tar.zst' 2>/dev/null | head -1)" ]]; then
  if [[ -z "${GITHUB_TOKEN:-}" ]]; then
    echo "::error::GITHUB_TOKEN is required to download nextest-archive-aarch64 on the board runner"
    exit 1
  fi
  python3 <<'PY'
import io
import json
import os
import urllib.error
import urllib.request
import zipfile

token = os.environ["GITHUB_TOKEN"]
repo = os.environ["GITHUB_REPOSITORY"]
run_id = os.environ["GITHUB_RUN_ID"]
headers = {
    "Authorization": f"Bearer {token}",
    "Accept": "application/vnd.github+json",
    "User-Agent": "edgefirst-camera-ci-board-run",
}


def api_get(url: str) -> bytes:
    req = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(req) as resp:
        return resp.read()


artifacts = json.loads(
    api_get(
        f"https://api.github.com/repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100"
    )
)["artifacts"]
match = next((a for a in artifacts if a.get("name") == "nextest-archive-aarch64"), None)
if match is None:
    raise SystemExit("::error::nextest-archive-aarch64 artifact not found for this workflow run")
zip_bytes = api_get(
    f"https://api.github.com/repos/{repo}/actions/artifacts/{match['id']}/zip"
)
os.makedirs("board-archive", exist_ok=True)
with zipfile.ZipFile(io.BytesIO(zip_bytes)) as zf:
    zf.extractall("board-archive")
PY
fi

archive="$(find board-archive -name 'nextest-archive.tar.zst' | head -1)"
if [[ -z "$archive" ]]; then
  archive="$(find board-archive -type f ! -name cargo-nextest | head -1)"
fi
if [[ -z "$archive" ]]; then
  echo "::error::nextest archive not found in board-archive/"
  exit 1
fi

nextest_bin="$(find board-archive -name cargo-nextest -type f | head -1)"
if [[ -z "$nextest_bin" ]]; then
  echo "::error::cargo-nextest not shipped in nextest-archive-aarch64 artifact"
  exit 1
fi
chmod +x "$nextest_bin"

echo "::group::Unpack archive"
"$nextest_bin" nextest list --archive-file "$archive" \
  --extract-to "${GITHUB_WORKSPACE}/board-extract" \
  --workspace-remap "${GITHUB_WORKSPACE}" \
  --list-type binaries-only
echo "::endgroup::"

camera_bin="$(find "${GITHUB_WORKSPACE}/board-extract" -type f -name edgefirst-camera -perm -u+x 2>/dev/null | head -1)"
if [[ -z "$camera_bin" ]]; then
  camera_bin="$(find "${GITHUB_WORKSPACE}/board-extract" -path '*/profiling/edgefirst-camera' -type f | head -1)"
fi
if [[ -z "$camera_bin" || ! -x "$camera_bin" ]]; then
  echo "::error::instrumented edgefirst-camera binary not found in archive"
  find "${GITHUB_WORKSPACE}/board-extract" -maxdepth 5 -type f | head -40
  exit 1
fi
chmod +x "$camera_bin"

TEST_FAILED=0

echo "=== Testing binary help output ==="
"$camera_bin" --help

run_timeout() {
  local label="$1" duration="$2"
  shift 2
  echo ""
  echo "=== ${label} (${duration}s) ==="
  timeout --signal=TERM --kill-after=5 "$duration" "$@" 2>&1 || {
    local ec=$?
    if [[ $ec -eq 124 ]]; then
      echo "✓ ${label} ran for ${duration} seconds"
    else
      echo "${label} failed with exit code $ec"
      TEST_FAILED=1
    fi
  }
}

run_timeout "JPEG capture" 15 "$camera_bin" --jpeg
run_timeout "H.264 encoding" 15 "$camera_bin" --h264

RECORD_FILE="/tmp/edgefirst-record-test.h264"
rm -f "$RECORD_FILE" "${RECORD_FILE%.h264}.json"
run_timeout "Recording H.264 to file" 10 "$camera_bin" --h264 --record "$RECORD_FILE"

if [[ -s "$RECORD_FILE" ]]; then
  echo "✓ Recorded .h264 non-empty: $(stat -c %s "$RECORD_FILE") bytes"
else
  echo "✗ Recorded .h264 is missing or empty"
  TEST_FAILED=1
fi
if [[ -s "${RECORD_FILE%.h264}.json" ]]; then
  echo "✓ Sidecar .json present"
  python3 -c "import json; json.load(open('${RECORD_FILE%.h264}.json'))" && \
    echo "✓ Sidecar parses as JSON" || { echo "✗ Sidecar not valid JSON"; TEST_FAILED=1; }
else
  echo "✗ Sidecar .json missing"
  TEST_FAILED=1
fi

if [[ -s "$RECORD_FILE" && -s "${RECORD_FILE%.h264}.json" ]]; then
  run_timeout "Replaying recorded file" 10 "$camera_bin" --replay "$RECORD_FILE" --replay-loop
else
  echo "Skipping replay: record artifacts missing"
fi
rm -f "$RECORD_FILE" "${RECORD_FILE%.h264}.json"

echo ""
echo "=== Running instrumented integration test binaries ==="
mapfile -t test_bins < <(find "${GITHUB_WORKSPACE}/board-extract" -type f -name 'test_image*' -perm -u+x 2>/dev/null || true)
if [[ ${#test_bins[@]} -eq 0 ]]; then
  deps="$(find "${GITHUB_WORKSPACE}/board-extract/target" -maxdepth 2 -type d -name deps | head -1)"
  if [[ -n "$deps" ]]; then
    mapfile -t test_bins < <(find "$deps" -type f -name 'test_image*' -perm -u+x 2>/dev/null || true)
  fi
fi

for test_bin in "${test_bins[@]}"; do
  [[ -f "$test_bin" ]] || continue
  test_name="$(basename "$test_bin")"
  echo "--- Running $test_name ---"
  if ! "$test_bin" --test-threads=1 --include-ignored 2>&1 | tee "coverage/test-output/${test_name}.txt"; then
    echo "FAILED: $test_name"
    TEST_FAILED=1
  fi
done

echo ""
echo "=== Profraw files generated ==="
find coverage/profraw/ -name '*.profraw' -exec ls -lh {} \; 2>/dev/null || echo "No profraw files"

rm -rf board-archive board-extract board-tmp videostream videostream.zip 2>/dev/null || true
rm -rf /tmp/test_* /tmp/edgefirst-camera* /tmp/profraw 2>/dev/null || true

if [[ $TEST_FAILED -ne 0 ]]; then
  echo ""
  echo "ERROR: One or more integration tests failed!"
  exit 1
fi

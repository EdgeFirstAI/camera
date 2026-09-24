#!/usr/bin/env bash
# Board-only setup: videostream runtime for instrumented edgefirst-camera binaries.
set -euo pipefail

# Keep in sync with the videostream crate version in Cargo.toml / Cargo.lock.
VIDEOSTREAM_VERSION="2.5.3"
# SHA-256 of videostream-${VIDEOSTREAM_VERSION}-linux-aarch64.zip from GitHub release assets.
VIDEOSTREAM_SHA256="161cc469acdb28d124fc3c789b1a08d0dd63b9b519a9445f45e099a1cd3f0307"

ARCHIVE="videostream-${VIDEOSTREAM_VERSION}-linux-aarch64.zip"
curl --fail --show-error --location --proto '=https' --tlsv1.2 \
  "https://github.com/EdgeFirstAI/videostream/releases/download/v${VIDEOSTREAM_VERSION}/${ARCHIVE}" \
  -o videostream.zip
echo "${VIDEOSTREAM_SHA256}  videostream.zip" | sha256sum --check --strict
unzip -q videostream.zip -d videostream
echo "VideoStream library extracted:"
find videostream -name "*.so*" -type f

echo "LD_LIBRARY_PATH=${GITHUB_WORKSPACE}/videostream/lib" >> "${GITHUB_ENV}"

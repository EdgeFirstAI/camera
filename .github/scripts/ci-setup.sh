#!/usr/bin/env bash
# Host and board CI setup (apt packages required to compile edgefirst-camera).
set -euo pipefail

if [[ "${SKIP_PACKAGES:-0}" == "1" ]]; then
  echo "ci-setup: skipping apt (SKIP_PACKAGES=1)"
  exit 0
fi

sudo apt-get update
sudo apt-get install -y \
  build-essential \
  cmake \
  pkg-config \
  nasm \
  gcc-aarch64-linux-gnu \
  g++-aarch64-linux-gnu

# vivid for the V4L2 backend tests; a no-op off GitHub-hosted Linux runners
# and never fatal.
bash "$(dirname "${BASH_SOURCE[0]}")/vivid-setup.sh" \
  || echo "::warning::vivid-setup.sh failed; V4L2 tests may skip"

#!/usr/bin/env bash
# Host CI setup: apt packages required to compile edgefirst-camera, then
# vivid for the V4L2 backend tests.
#
#   SKIP_PACKAGES=1  skip apt (vivid is still checked)
set -euo pipefail

if [[ "${SKIP_PACKAGES:-0}" == "1" ]]; then
  echo "ci-setup: skipping apt (SKIP_PACKAGES=1)"
else
  sudo apt-get update
  sudo apt-get install -y \
    build-essential \
    cmake \
    pkg-config \
    nasm \
    gcc-aarch64-linux-gnu \
    g++-aarch64-linux-gnu
fi

# Fails the lane when vivid cannot be loaded, unless
# EDGEFIRST_CAMERA_ALLOW_NO_VIVID=1 (see vivid-setup.sh).
bash "$(dirname "${BASH_SOURCE[0]}")/vivid-setup.sh"

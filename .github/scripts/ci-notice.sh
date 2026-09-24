#!/usr/bin/env bash
# Quick-tier NOTICE check: dependency SBOM via cargo-cyclonedx (no scancode).
set -euo pipefail

PROJECT_NAME="edgefirst-camera"

if ! command -v cargo >/dev/null 2>&1; then
  echo "::error::Rust toolchain required for cargo cyclonedx"
  exit 1
fi

cargo cyclonedx --format json --all
if [[ ! -f "${PROJECT_NAME}.cdx.json" ]]; then
  echo "::error::expected ${PROJECT_NAME}.cdx.json after cargo cyclonedx"
  exit 1
fi
mv "${PROJECT_NAME}.cdx.json" sbom.json

python3 .github/scripts/check_license_policy.py sbom.json
python3 .github/scripts/validate_notice.py NOTICE sbom.json

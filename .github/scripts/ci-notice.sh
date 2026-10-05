#!/usr/bin/env bash
# Quick-tier NOTICE check: dependency SBOM via cargo-cyclonedx (no scancode).
set -euo pipefail

# One dependency SBOM per workspace package: the application at the root
# and each crate under crates/. NOTICE covers their union.
SBOMS=("edgefirst-camera-app.cdx.json" "crates/camera/edgefirst-camera.cdx.json")

if ! command -v cargo >/dev/null 2>&1; then
  echo "::error::Rust toolchain required for cargo cyclonedx"
  exit 1
fi

cargo cyclonedx --format json --all
for sbom in "${SBOMS[@]}"; do
  if [[ ! -f "$sbom" ]]; then
    echo "::error::expected $sbom after cargo cyclonedx"
    exit 1
  fi
  python3 .github/scripts/check_license_policy.py "$sbom"
done
cp "${SBOMS[0]}" sbom.json

python3 .github/scripts/validate_notice.py NOTICE "${SBOMS[@]}"

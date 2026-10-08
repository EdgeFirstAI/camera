#!/usr/bin/env bash
# Load the kernel's virtual V4L2 capture driver, vivid, for the V4L2 backend
# tests (crates/camera/tests/v4l2_vivid.rs) on a GitHub-hosted Ubuntu runner:
# a single-planar, a multi-planar and a single-planar instance that the
# unplug test disconnects. Also opens /dev/dma_heap/system, which the hosted
# images ship as root:video 0660, for the Import and caller-pool tests.
#
# Exports EDGEFIRST_CAMERA_REQUIRE_VIVID=1 through $GITHUB_ENV only when the
# vivid nodes appear, so a lane that should have vivid fails instead of
# skipping. When the archive has no linux-modules-extra for the runner's
# kernel, the tests skip and the step summary says why. Never fails the job,
# and does nothing on self-hosted machines, where loading a module and
# opening device nodes would outlive the job.
set -uo pipefail

report() {
    echo "vivid-setup: $1"
    if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
        echo "$1" >> "${GITHUB_STEP_SUMMARY}"
    fi
}

if [[ "$(uname -s)" != Linux* || "${RUNNER_ENVIRONMENT:-}" != "github-hosted" ]]; then
    report "vivid: n/a (not a GitHub-hosted Linux runner)"
    exit 0
fi

if [[ -e /dev/dma_heap/system ]]; then
    sudo chmod a+rw /dev/dma_heap/system || true
fi

pkg="linux-modules-extra-$(uname -r)"
if ! apt-cache show "${pkg}" > /dev/null 2>&1; then
    report "vivid: unavailable — V4L2 tests skipped (${pkg} is not in the archive)"
    exit 0
fi
if ! sudo apt-get install -y -q "${pkg}" \
    || ! sudo modprobe vivid n_devs=3 node_types=0x1,0x1,0x1 multiplanar=1,2,1; then
    report "vivid: unavailable — V4L2 tests skipped (installing or loading vivid failed)"
    exit 0
fi
# udev creates and permissions the nodes after modprobe returns; wait for it,
# or it resets the modes changed below.
sudo udevadm settle
nodes=()
for d in /sys/class/video4linux/video*; do
    [[ "$(cat "${d}/name" 2>/dev/null)" == vivid-* ]] && nodes+=("/dev/$(basename "${d}")")
done
if [[ ${#nodes[@]} -lt 3 ]]; then
    report "vivid: unavailable — V4L2 tests skipped (expected 3 vivid nodes, found ${#nodes[@]})"
    exit 0
fi
sudo chmod a+rw "${nodes[@]}"
if [[ -n "${GITHUB_ENV:-}" ]]; then
    echo "EDGEFIRST_CAMERA_REQUIRE_VIVID=1" >> "${GITHUB_ENV}"
fi
report "vivid: required (${nodes[*]})"

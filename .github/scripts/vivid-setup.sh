#!/usr/bin/env bash
# Load the kernel's virtual V4L2 capture driver, vivid, for the V4L2 backend
# tests (crates/camera/tests/v4l2_vivid.rs) on a GitHub-hosted Ubuntu runner:
# a single-planar, a multi-planar and a single-planar instance that the
# unplug test disconnects. Also opens /dev/dma_heap/system, which the hosted
# images ship as root:video 0660, for the Import and caller-pool tests.
#
# vivid.ko ships in linux-modules for some runner kernels (the 7.0 azure
# kernel on ubuntu-24.04) and in linux-modules-extra for others (the 6.8
# azure kernel), so this installs the extra package only when the running
# kernel's modules lack vivid.
#
# On success exports EDGEFIRST_CAMERA_REQUIRE_VIVID=1 through $GITHUB_ENV, so
# the tests fail rather than skip. When vivid cannot be loaded the script
# fails, since the V4L2 tests would otherwise pass as skips, unless
# EDGEFIRST_CAMERA_ALLOW_NO_VIVID=1 (the CAMERA_ALLOW_NO_VIVID repository
# variable or the ci:no-vivid label, see ci.yml), which turns that into a
# warning.
#
# A self-hosted Linux runner is never modified, since loading a module and
# opening device nodes would outlive the job: vivid must already be loaded
# there with its nodes open to the runner's user.
set -uo pipefail

report() {
    echo "vivid-setup: $1"
    if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
        echo "$1" >> "${GITHUB_STEP_SUMMARY}"
    fi
}

unavailable() {
    local why=$1
    if [[ "${EDGEFIRST_CAMERA_ALLOW_NO_VIVID:-0}" == 1 ]]; then
        report "vivid: unavailable, V4L2 tests skipped as allowed by EDGEFIRST_CAMERA_ALLOW_NO_VIVID ($why)"
        echo "::warning::vivid is unavailable ($why); the V4L2 tests skip"
        exit 0
    fi
    report "vivid: unavailable ($why)"
    echo "::error::vivid is unavailable ($why), so the V4L2 tests would pass as skips. Set the repository variable CAMERA_ALLOW_NO_VIVID=true, or label the pull request ci:no-vivid, to allow that."
    exit 1
}

vivid_nodes() {
    nodes=()
    for d in /sys/class/video4linux/video*; do
        [[ "$(cat "${d}/name" 2>/dev/null)" == vivid-* ]] && nodes+=("/dev/$(basename "${d}")")
    done
}

require() {
    if [[ -n "${GITHUB_ENV:-}" ]]; then
        echo "EDGEFIRST_CAMERA_REQUIRE_VIVID=1" >> "${GITHUB_ENV}"
    fi
    report "vivid: required (${nodes[*]})"
}

if [[ "$(uname -s)" != Linux* ]]; then
    report "vivid: n/a (not Linux; the V4L2 tests are not built)"
    exit 0
fi

if [[ "${RUNNER_ENVIRONMENT:-}" != "github-hosted" ]]; then
    vivid_nodes
    if [[ ${#nodes[@]} -lt 3 ]]; then
        unavailable "self-hosted runner with ${#nodes[@]} of 3 vivid nodes loaded"
    fi
    for node in "${nodes[@]}"; do
        [[ -r "${node}" && -w "${node}" ]] || unavailable "${node} is not read-write for $(id -un)"
    done
    require
    exit 0
fi

if [[ -e /dev/dma_heap/system ]]; then
    sudo chmod a+rw /dev/dma_heap/system || true
fi

kernel="$(uname -r)"
if ! modinfo vivid > /dev/null 2>&1; then
    pkg="linux-modules-extra-${kernel}"
    if ! apt-cache show "${pkg}" > /dev/null 2>&1; then
        unavailable "linux-modules-${kernel} has no vivid and ${pkg} is not in the archive"
    fi
    sudo apt-get install -y -q "${pkg}" || unavailable "installing ${pkg} failed"
    modinfo vivid > /dev/null 2>&1 || unavailable "${pkg} has no vivid"
fi
sudo modprobe vivid n_devs=3 node_types=0x1,0x1,0x1 multiplanar=1,2,1 \
    || unavailable "modprobe vivid failed on ${kernel}"
# udev creates and permissions the nodes after modprobe returns; wait for it,
# or it resets the modes changed below.
sudo udevadm settle
vivid_nodes
if [[ ${#nodes[@]} -lt 3 ]]; then
    unavailable "expected 3 vivid nodes, found ${#nodes[@]}"
fi
sudo chmod a+rw "${nodes[@]}"
require

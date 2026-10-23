#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.
#
# Compares the installed camera service with a new build on a Maivin, side
# by side under its own camera.service, and writes the results to a
# directory on this host.
#
#   scripts/parity.sh [-s seconds] [--no-clock-step] <ssh-host> <output-dir>
#
# Build first:
#   cargo zigbuild --target aarch64-unknown-linux-gnu.2.35 --release --locked \
#       --bin edgefirst-camera --example parity
#   cargo zigbuild --target aarch64-unknown-linux-gnu.2.35 --release --locked \
#       -p edgefirst-camera --example probe
#
# For each version it runs the deployed configuration (/etc/default/camera)
# and the same with --jpeg, measuring with `parity measure` on the board.
# It then measures the driver's own delivery delay with the SDK's `probe`
# with the service stopped, and, unless --no-clock-step, the clock-step
# acceptance on the new build: the service starts with the clock at the
# epoch and chronyd stopped, chronyd then steps it forward to GPS/NTP time,
# and after a manual step one hour ahead chronyd steps it back.
#
# The service override lives in /run, so a reboot also restores the board.
# Needs passwordless sudo on the board, and ffprobe here.

set -euo pipefail

SECONDS_PER_RUN=60
CLOCK_STEP=1
while [[ $# -gt 0 ]]; do
    case "$1" in
    -s) SECONDS_PER_RUN="$2"; shift 2 ;;
    --no-clock-step) CLOCK_STEP=0; shift ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) break ;;
    esac
done
if [[ $# -ne 2 ]]; then
    echo "usage: $0 [-s seconds] [--no-clock-step] <ssh-host> <output-dir>" >&2
    exit 2
fi
HOST="$1"
OUT="$2"
TARGET="${TARGET:-aarch64-unknown-linux-gnu}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REL="$ROOT/target/$TARGET/release"
BEFORE_BIN="${BEFORE_BIN:-/usr/bin/edgefirst-camera}"
DEVICE="${DEVICE:-/dev/video3}"
WARMUP=15
DIR=parity
DROPIN=/run/systemd/system/camera.service.d/parity.conf

mkdir -p "$OUT"
on() { ssh -o ConnectTimeout=10 "$HOST" "$@"; }
log() { printf '[parity] %s\n' "$*" >&2; }

restore() {
    log "restoring the installed service and clock sync"
    on "sudo rm -f $DROPIN; sudo systemctl daemon-reload; sudo systemctl start chronyd; sudo systemctl restart camera" || true
}
trap restore EXIT

for f in "$REL/edgefirst-camera" "$REL/examples/parity" "$REL/examples/probe"; do
    [[ -x "$f" ]] || { echo "missing $f; build it first" >&2; exit 2; }
done
on "mkdir -p $DIR && rm -f $DIR/*.json $DIR/*.h264"
scp -q "$REL/edgefirst-camera" "$REL/examples/parity" "$REL/examples/probe" "$HOST:$DIR/"
AFTER_BIN="$(on 'echo $HOME')/$DIR/edgefirst-camera"
BEFORE_VERSION="$(on "$BEFORE_BIN --version" | awk '{print $2}')"
AFTER_VERSION="$(on "$AFTER_BIN --version" | awk '{print $2}')"
log "comparing $BEFORE_VERSION ($BEFORE_BIN) with $AFTER_VERSION ($AFTER_BIN)"

# Runs camera.service with ExecStart replaced, and prints its PID once the
# warm-up has passed.
start_service() {
    local exec="$1"
    on "sudo mkdir -p $(dirname $DROPIN) && printf '[Service]\nExecStart=\nExecStart=%s\n' '$exec' | sudo tee $DROPIN >/dev/null && sudo systemctl daemon-reload && sudo systemctl restart camera"
    sleep "$WARMUP"
    on "systemctl is-active --quiet camera" || { on "journalctl -u camera -n 30 --no-pager" >&2; exit 1; }
    on "systemctl show -p MainPID --value camera"
}

measure() {
    local name="$1" pid="$2" seconds="$3"
    on "$DIR/parity measure --seconds $seconds --pid $pid --label '$name' --h264-out $DIR/$name.h264 > $DIR/$name.json"
    scp -q "$HOST:$DIR/$name.json" "$HOST:$DIR/$name.h264" "$OUT/"
}

# The same, while also measuring what the services that read camera/frame
# publish, to show they keep working.
measure_with_consumers() {
    local name="$1" pid="$2" seconds="$3" host
    host="$(on hostname)"
    on "$DIR/parity measure --seconds $seconds --keys '$host/model/**' '$host/fusion/**' --label '$name' > $DIR/$name-consumers.json & \
        $DIR/parity measure --seconds $seconds --pid $pid --label '$name' --h264-out $DIR/$name.h264 > $DIR/$name.json; wait"
    scp -q "$HOST:$DIR/$name.json" "$HOST:$DIR/$name-consumers.json" "$HOST:$DIR/$name.h264" "$OUT/"
}

for version in before after; do
    if [[ $version == before ]]; then bin=$BEFORE_BIN; label=$BEFORE_VERSION; else bin=$AFTER_BIN; label=$AFTER_VERSION; fi
    for config in default jpeg; do
        args=""
        [[ $config == jpeg ]] && args=" --jpeg"
        log "$label, $config configuration, ${SECONDS_PER_RUN}s"
        pid="$(start_service "$bin$args")"
        if [[ $config == default ]]; then
            measure_with_consumers "$label-$config" "$pid" "$SECONDS_PER_RUN"
        else
            measure "$label-$config" "$pid" "$SECONDS_PER_RUN"
        fi
    done
done

log "driver delivery delay with the service stopped"
on "sudo systemctl stop camera && sudo /usr/bin/maivin-camera-select-mode >/dev/null 2>&1; sudo $DIR/probe --source $DEVICE --size 1920x1080 --fps 30 --frames 900 --json" >"$OUT/floor.json"

if [[ $CLOCK_STEP == 1 ]]; then
    log "clock step: service starts at the epoch with chronyd stopped"
    on "sudo systemctl stop chronyd && sudo date -s @86400 >/dev/null"
    since="$(on 'date "+%Y-%m-%d %H:%M:%S"')"
    pid="$(start_service "$AFTER_BIN")"
    measure clock-1-epoch "$pid" 10
    log "clock step: chronyd steps the clock forward"
    on "sudo systemctl start chronyd && chronyc waitsync 60 0.1 >/dev/null"
    sleep 2
    measure clock-2-forward "$pid" 10
    log "clock step: one hour ahead, then chronyd steps it back"
    on "sudo systemctl stop chronyd && sudo date -s @\$((\$(date +%s) + 3600)) >/dev/null"
    sleep 2
    measure clock-3-ahead "$pid" 10
    on "sudo systemctl start chronyd && chronyc waitsync 60 0.1 >/dev/null"
    sleep 2
    measure clock-4-back "$pid" 10
    on "echo pid_before=$pid pid_after=\$(systemctl show -p MainPID --value camera) restarts=\$(systemctl show -p NRestarts --value camera)" >"$OUT/clock-pid.txt"
    on "journalctl -u camera --since '$since' -o cat --no-pager" >"$OUT/clock-journal.txt" || true
fi

trap - EXIT
restore
sleep "$WARMUP"
on "systemctl is-active camera; $BEFORE_BIN --version" >"$OUT/restored.txt"

for f in "$OUT"/*.h264; do
    ffprobe -v error -count_frames -select_streams v:0 \
        -show_entries stream=codec_name,profile,width,height,has_b_frames,nb_read_frames \
        -of json "$f" >"${f%.h264}.ffprobe.json" 2>"${f%.h264}.ffprobe.err" || true
done

for config in default jpeg default-consumers; do
    cargo run -q --release --locked --example parity -- compare --floor "$OUT/floor.json" \
        "$OUT/$BEFORE_VERSION-$config.json" "$OUT/$AFTER_VERSION-$config.json" >"$OUT/$config.md"
done
log "results in $OUT"

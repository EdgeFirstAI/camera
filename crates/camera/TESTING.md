# Testing

## Host tests

Unit tests live beside the code. Public-API tests against the mock backend are in `tests/mock.rs` and need the `mock` feature; `tests/schema.rs` also needs `schemas`, and `tests/file.rs` needs `file`.

```bash
cargo test -p edgefirst-camera --features mock,schemas,file
cargo clippy -p edgefirst-camera --all-targets --features mock,schemas,file -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p edgefirst-camera --no-deps --features mock,schemas,file
```

These run in every CI lane:

- **Linux:** the Quick tier.
- **macOS and Windows:** the Full tier's `sdk-portable` job, triggered by the `ci:full` label.

## What is covered

| Area | Tests |
|---|---|
| Wall-clock conversion | `timestamp.rs`: bracket midpoint, retry of a wide bracket, forward and backward steps, pre-epoch times, conversion against the host clocks |
| Source strings | `builder.rs`: every form and the malformed ones |
| Mode naming | `enumerate.rs`: discrete sizes (OV5640 on ISI), stepwise ranges (vvcam), unknown rates, fractional rates |
| Errors | `error.rs`: rejection reason and slot, errno mapping, error sources |
| Slot ownership | `pool.rs`: a held slot is never released before its frame drops; release order; detached frames keep their memory and release nothing; a detached table hands out no frames; one slot cannot be framed twice; releases from other threads |
| Frame lifetime | `frame.rs` and `tests/mock.rs`: release on drop; starvation; pool depths 2 to 8; a frame held across `stop()`/`start()` stays valid and requeues on drop; frames outliving a pool change and the camera |
| Buffers | `pool.rs`: each rejection reason (including a PBO tensor as `NotNativeHandle`) with its slot, count limits, contiguity and pitch rules. `tests/mock.rs`: caller pools, rejection and recovery through `take_buffers` and `set_buffers` |
| Controls | `tests/mock.rs`: applied, clamped and unsupported outcomes |
| File source (`file`) | `backend/file/`, with the decoder mocked: the sidecar (a 2.x sidecar, extras, colorimetry labels, version, codec and field errors); the Annex B reader (units across read boundaries, a start code split by a read, leading garbage, a long stretch without start codes, rewind); pacing (spacing, re-anchoring after a stall, requested rate); every picture then `EndOfStream`; `seq` across loops; `encoded()` reproducing the file byte for byte with a reordering decoder; a rejected unit skipped; a damaged picture counted; held pictures blocking until one drops; caller pools refused and handed back. `tests/file.rs`: missing recording and sidecar, an unsupported codec, no decoder off Linux |
| `CameraFrame` mapping (`schemas`) | `schema.rs`: byte-for-byte against the schemas `CameraFrame_split_fd` golden (NV12M), field by field against `CameraFrame` (single-buffer NV12), unspecified colorimetry, unformatted tensors, epoch clamp and Y2038 saturation, the Zenoh NTP64 timestamp within 2 ns of the header stamp. `tests/schema.rs`: every mock format at nine sizes passes `TensorFields::validate` and decodes to the frame's bytes. `tests/v4l2_vivid.rs`: real DMA-BUF frames map to planes by reference |

## V4L2 backend on vivid

`tests/v4l2_vivid.rs` runs the V4L2 backend against the kernel's virtual capture driver. Load three instances: a single-planar one, a multi-planar one and a single-planar one that the unplug test disconnects (it stays disconnected until the module reloads):

```bash
sudo modprobe vivid n_devs=3 node_types=0x1,0x1,0x1 multiplanar=1,2,1
cargo test -p edgefirst-camera --test v4l2_vivid
```

The user needs read-write access to `/dev/video*` and `/dev/dma_heap/system` (usually the `video` group). Tests that share a vivid instance serialise on a lock file, so they are safe under `cargo test` threads and `cargo nextest` processes. Without vivid every test prints `SKIPPED` and passes. Hosted CI must provide vivid, so when `EDGEFIRST_CAMERA_REQUIRE_VIVID` or `EDGEFIRST_CAMERA_REQUIRE_BOARD` is set the tests fail rather than skip. The fault-injection tests need a debug build.

In CI, `.github/scripts/vivid-setup.sh` (run from `ci-setup.sh` on GitHub-hosted Linux runners) loads vivid from the runner kernel's modules, installing `linux-modules-extra` for that kernel only when vivid is not already there, opens the vivid nodes and the system DMA heap, and sets `EDGEFIRST_CAMERA_REQUIRE_VIVID=1`. When vivid cannot be loaded the step fails, unless the repository variable `CAMERA_ALLOW_NO_VIVID=true` or the PR label `ci:no-vivid` allows the skip. Instances shared between tests serialise on `/run/lock/edgefirst-camera-vivid-N.lock`, falling back to the temp directory, so separate target directories and concurrent jobs on one runner do not collide.

| Acceptance criterion | Test |
|---|---|
| Negotiation | `negotiation_reports_what_the_driver_applied`, `an_unsupported_format_is_reported`, `enumerate_and_probe_list_vivid` |
| Both memory strategies | `import_captures_*`, `export_captures_*`, `auto_imports_when_the_driver_accepts` (single- and multi-planar) |
| Controls | `controls_are_applied_and_read_back` (mirror, a clamped custom control) |
| Timeout | `a_timeout_is_reported_when_every_buffer_is_held` |
| EINTR | `a_signal_during_the_wait_does_not_fail_it` |
| Unplug | `unplug_is_disconnected` (vivid's Disconnect control) |
| Timestamp flags | `timestamps_map_to_clock_and_source` |
| Auto falls back on a refused `QBUF` | `auto_falls_back_to_export_when_import_is_refused`, `explicit_import_does_not_fall_back` |
| `BuffersRejected` | `a_caller_pool_the_driver_refuses_is_rejected`, `a_mismatched_caller_pool_is_rejected_and_recoverable` |
| `ContiguousUnavailable` | `contiguous_memory_unavailable_is_reported` (hosts without a CMA heap) |
| Close with frames held | `exported_frames_held_across_close_stay_readable_and_the_device_reopens`, `close_is_deferred_when_the_driver_cannot_orphan_buffers` |
| Measured rate, wait handle, exclusivity | `the_rate_is_measured_from_the_frames`, `the_wait_handle_polls_ready`, `an_exclusive_device_is_busy_for_a_second_user` |

vivid has neither a CMA-backed import path nor the i.MX quirks, so contiguous Import, the vvcam flip and `NotReady` after an ISP restart are validated on the boards by the board test suite (`tests/board.rs`). vivid stamps a frame with its simulated end-of-frame time, about one period ahead of delivery, so the `probe` example reports a negative latency on it.

## File source on a board

`tests/file.rs` plays a real recording when `EDGEFIRST_CAMERA_RECORDING` names a camera 2.x `.h264` file with its `.json` sidecar beside it, on a target with a V4L2 H.264 decoder (i.MX 8M Plus, i.MX 95). It checks one frame per access unit, a rate within 2% of the sidecar's, `extras()`, `EndOfStream` at the end, and `seq` across two loops. Without the variable those tests print a note and pass, except on the board lanes (below), where CI points it at `tests/data/testsrc-160x120.h264`. Build it optimised: a debug build splits a high-bitrate recording (the 2.x app records 1080p at about 64 Mbit/s) too slowly to keep 30 fps.

```bash
EDGEFIRST_CAMERA_RECORDING=/home/root/run01.h264 cargo test --release -p edgefirst-camera --features file --test file
```

## ioctl ABI check

A V4L2 ioctl number encodes the size of its argument, so a struct whose layout differs from the kernel's produces a request the kernel does not know, and the call fails with `ENOTTY`. A fallback in the backend can hide that from the tests, so `.github/scripts/ioctl-abi-check.sh` runs `tests/v4l2_vivid.rs` under `strace -ff -e trace=ioctl` and fails when:

- any ioctl other than a terminal one returns `ENOTTY`, unless its name is listed in `ABI_ALLOW_ENOTTY` (space-separated), or
- no `VIDIOC_STREAMON` succeeded, so a run that captured nothing cannot pass.

V4L2 requests strace cannot decode are reported as warnings. The step summary lists every failed ioctl with its errno; on vivid the expected ones are the `EINVAL` that end each enumeration and the `ENODEV` from the unplug test. Locally, with vivid loaded:

```sh
make abi-check
```

CI runs it on every pull request in the `V4L2 ioctl ABI` job on `ubuntu-24.04`; when the runner's kernel has no `linux-modules-extra` package the job warns and skips.

## Benchmarks

`benches/capture.rs` measures a live V4L2 camera with criterion:

| Benchmark | What it measures |
|---|---|
| `capture/<config>/frame_interval` | One `next_frame` call: the wait for the next frame, so the frame period and its jitter |
| `capture/<config>/delivery_latency` | Capture timestamp to `next_frame` returning, on fresh frames. Skipped unless the timestamps are `CLOCK_MONOTONIC` and precede delivery (vivid's lead it) |
| `frame/<config>/import` | `dup` of a frame's DMA-BUF and `TensorDyn::from_fd`, the import a consumer repeats per frame |
| `frame/<config>/cpu_read` | Mapping a frame and reading every byte, which shows whether capture memory is CPU-cached |

`<config>` names what was applied, for example `Nv12-1920x1080-import-cma`. The source is `EDGEFIRST_CAMERA_BENCH_SOURCE` or the first V4L2 capture node; `EDGEFIRST_CAMERA_BENCH_SIZE` (`WxH`) and `EDGEFIRST_CAMERA_BENCH_FORMAT` are requests. Buffers are contiguous where the platform has a CMA heap. Without a camera the benchmark prints `SKIPPED`.

```sh
EDGEFIRST_CAMERA_BENCH_SOURCE=/dev/video3 cargo bench -p edgefirst-camera --bench capture
```

On the board fleet, run the `Camera benchmarks` workflow (`camera-bench.yml`) by hand with the board runner labels and, optionally, the source, size and format. It builds the benchmark once for aarch64, runs it on each board, and summarises the bencher-format results; each board's artifact also records the kernel, governors, clocks and temperatures.

## Board test variables

`.github/scripts/board-setup.sh` sets these on the board lanes, and they work the same when running `tests/board.rs` or `tests/on_target.rs` by hand:

| Variable | Effect |
|---|---|
| `EDGEFIRST_CAMERA_REQUIRE_BOARD=1` | Every test that would print `SKIPPED` fails instead, naming the reason |
| `EDGEFIRST_CAMERA_ALLOW_SKIPS` | Comma-separated skip reasons this board may still take: `no-camera`, `no-mirror`, `uniform-scene`, `not-vvcam`, `not-monotonic`, `no-recording`, `not-v4l2`, `no-cma-heap`, `no-encoder`, `no-decoder`, `encoder-format`. The i.MX 95 lane allows `not-vvcam,no-mirror,not-v4l2`; the Raspberry Pi 5 lane, which has no H.264 encoder or decoder, `not-vvcam,not-v4l2,no-encoder,no-decoder`. Boards tested by hand set the same variables: the IQ-9075 EVK with its USB webcam, for example, uses `EDGEFIRST_CAMERA_SIZE=640x480` and allows `not-vvcam,no-mirror,no-cma-heap,encoder-format` |
| `EDGEFIRST_CAMERA_SOURCE` | The camera: a V4L2 node or link (default `/dev/video3`), `libcamera:<id>`, or `libcamera` for the first camera libcamera lists |
| `EDGEFIRST_CAMERA_SIZE` | The capture size the tests request, and `on_target`'s stream size, as `WIDTHxHEIGHT` (default `1920x1080`) |
| `EDGEFIRST_CAMERA_RECORDING` | A recording with its `.json` sidecar, for the decode and playback tests |
| `EDGEFIRST_CAMERA_FPS` | The rate the tests request, and expects at least 90% of on `camera/frame` and `camera/h264` (default 30). `camera/jpeg` is compressed in software, so it is only required to keep delivering, at no particular rate |
| `EDGEFIRST_SOAK_SECS` | How long the soak runs (default 20; nightly runs 600) |

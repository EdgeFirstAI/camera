# Testing

## Host tests

Unit tests live beside the code. Public-API tests against the mock backend are in `tests/mock.rs` and need the `mock` feature.

```bash
cargo test -p edgefirst-camera --features mock
cargo clippy -p edgefirst-camera --all-targets --features mock -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p edgefirst-camera --no-deps --features mock
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
| Frame lifetime | `frame.rs` and `tests/mock.rs`: release on drop; starvation counted as drops; pool depths 2 to 8; a frame held across `stop()`/`start()` stays valid and requeues on drop; frames outliving a pool change and the camera |
| Buffers | `pool.rs`: each rejection reason (including a PBO tensor as `NotNativeHandle`) with its slot, count limits, contiguity and pitch rules. `tests/mock.rs`: caller pools, rejection and recovery through `take_buffers` and `set_buffers` |
| Controls | `tests/mock.rs`: applied, clamped and unsupported outcomes |

## V4L2 backend on vivid

`tests/v4l2_vivid.rs` runs the V4L2 backend against the kernel's virtual capture driver. Load three instances: a single-planar one, a multi-planar one and a single-planar one that the unplug test disconnects (it stays disconnected until the module reloads):

```bash
sudo modprobe vivid n_devs=3 node_types=0x1,0x1,0x1 multiplanar=1,2,1
cargo test -p edgefirst-camera --test v4l2_vivid
```

The user needs read-write access to `/dev/video*` and `/dev/dma_heap/system` (usually the `video` group). Tests that share a vivid instance serialise on a lock file, so they are safe under `cargo test` threads and `cargo nextest` processes. Without vivid every test prints `SKIPPED` and passes; `EDGEFIRST_CAMERA_REQUIRE_VIVID=1` makes that a failure. The fault-injection tests need a debug build.

In CI, `.github/scripts/vivid-setup.sh` (run from `ci-setup.sh` on GitHub-hosted Linux runners) installs `linux-modules-extra` for the runner's kernel, loads vivid, opens the vivid nodes and the system DMA heap, and sets `EDGEFIRST_CAMERA_REQUIRE_VIVID=1` only when the nodes appear.

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
| Drops, wait handle, exclusivity | `drops_are_counted_from_timestamp_gaps`, `the_wait_handle_polls_ready`, `an_exclusive_device_is_busy_for_a_second_user` |

vivid has neither a CMA-backed import path nor the i.MX quirks, so contiguous Import, the vvcam flip and `NotReady` after an ISP restart are validated on the boards (T1.21–T1.24). vivid stamps a frame with its simulated end-of-frame time, about one period ahead of delivery, so the `probe` example reports a negative latency on it.

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

On the board fleet, run the `Camera benchmarks` workflow (`camera-bench.yml`) by hand with the board runner labels and, optionally, the source, size and format. It builds the benchmark once for aarch64, runs it on each board, and summarises the bencher-format results; each board's artifact also records the kernel, governors, clocks and temperatures. Conversion and encoding benchmarks over SDK frames follow once the application converts and encodes through the HAL; until then the application's `benches/convert.rs` and `benches/encode.rs` cover them.

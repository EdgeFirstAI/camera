# EdgeFirst Camera SDK

`edgefirst-camera` is a portable camera capture API for the EdgeFirst stack. Cameras deliver frames as [`edgefirst-tensor`](https://crates.io/crates/edgefirst-tensor) tensors, so they go straight to `edgefirst-image` for conversion, to `edgefirst-codec` for encoding, and onto the wire as `edgefirst-schemas` `CameraFrame` messages without a copy.

> **Status:** in development on the 3.0 integration branch. The public API, the `mock` backend and the V4L2 backend are in place; the file and libcamera backends follow.

## Quick start

```rust
use edgefirst_camera::CameraBuilder;
use std::time::Duration;

let mut camera = CameraBuilder::source("mock:1280x720@30")?
    .buffers(4)
    .open()?;
camera.start()?;
let frame = camera.next_frame(Some(Duration::from_secs(1)))?;
println!(
    "frame {} acquired at {:?}, {} plane(s)",
    frame.seq(),
    frame.realtime(),
    frame.planes().len()
);
# Ok::<(), edgefirst_camera::Error>(())
```

## Concepts

- **Requests and negotiated configuration.** Size, format, frame rate and controls are requests. `Camera::config()` and `Applied` report what was configured. The SDK never changes the stream to enforce a request, for example by dropping frames.
- **Frames.** A `Frame` dereferences to its `TensorDyn` and carries capture metadata: the SDK sequence counter, the raw driver sequence, the capture timestamp, the wall-clock acquisition time and the plane layout. A buffer is never given back to the driver while a frame references it.
- **Acquisition time.** Each frame's capture instant is converted to `CLOCK_REALTIME` once, at dequeue, by `RealtimeClock`. The offset is measured at every conversion, so frames follow wall-clock steps without a restart.
- **Buffers.** `MemoryStrategy::Auto` prefers Import into SDK-allocated buffers and falls back to Export. Caller-provided buffers (`with_buffers`) are never replaced silently; a refusal is `ErrorKind::BuffersRejected`, with the reason in `Error::rejection()`. The camera stays open for recovery through `take_buffers`, `set_buffers` and `set_contiguity`.
- **Modes.** `probe()` returns each camera's sizes and rates, and `modes()` names them (`1080p30`) so users can see what a camera offers. Modes are never set directly.

## Sources

| Source string | Backend | Status |
|---|---|---|
| `mock[:WxH@fps]` | Synthetic frames (feature `mock`) | available |
| `/dev/videoN` | V4L2 capture (feature `v4l2`, Linux) | available |
| `file:<path>` | Recorded Annex-B H.264 with JSON sidecar | planned |
| `libcamera:<id>` | libcamera through a runtime-loaded shim | planned |

## Features

| Feature | Default | Enables |
|---|---|---|
| `static` | yes | forwards to `edgefirst-tensor/static` |
| `v4l2` | yes | the V4L2 capture backend (Linux) |
| `mock` | no | the synthetic frame source |

## Platforms

Linux (x86_64, aarch64), macOS and Windows build and run against `mock`. Native backends are Linux-first.

`cargo run -p edgefirst-camera --example probe -- --source /dev/video3 --frames 300 --close-test --json` reports what a camera negotiates and delivers on a platform: driver and BSP identity, format and pitch, the memory strategy `Auto` resolved to, contiguity, frame rate, drops, latency, the timestamp clock, and whether a frame held across close stays intact.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the repository root.

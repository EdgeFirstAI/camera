# Architecture

The design lives in Confluence, under [EdgeFirst Camera SDK](https://au-zone.atlassian.net/wiki/spaces/EAM/pages/2750545921). This file maps that design onto the crate.

## Modules

| Module | Contents |
|---|---|
| `lib.rs` | Re-exports, `enumerate()`, `probe()` |
| `camera.rs` | The object-safe `Camera` trait, `CaptureStats`, `WaitHandle` |
| `builder.rs` | `CameraBuilder` and source-string parsing |
| `config.rs` | `StreamRequest` (what was asked), `StreamConfig` (what was negotiated), `MemoryStrategy`, `Contiguity`, `BufferPool`, `Mirror` |
| `control.rs` | Backend-neutral controls and their outcomes (`Applied`) |
| `frame.rs` | `Frame`, `FrameMeta`, `PlaneLayout`, `SlotRelease` |
| `pool.rs` | `SlotTable` (slot ownership shared by every backend) and caller-pool validation |
| `timestamp.rs` | `Timestamp`, `CaptureClock`, `TimestampSource`, `RealtimeClock` |
| `enumerate.rs` | `CameraDescriptor`, `FormatInfo`, `Sizes`, `Rates`, `Mode`, `modes()` |
| `error.rs` | `Error`, `ErrorKind`, `Rejection` |
| `backend/` | Backend dispatch; `mock.rs`, `v4l2/` on Linux, and `file/` |

## Rules every backend follows

1. **Requests are reported, never enforced.** A backend applies what the device accepts, records the result in `StreamConfig`, and reports controls as `Applied`. It never drops frames, rescales or otherwise alters the stream to meet a request.
2. **A buffer is never handed back while a frame references it.** Backends keep their buffers in a `SlotTable`. A frame owns what keeps its tensor alive: the slot's `Arc<TensorDyn>`, or for the file source the decoded picture itself. The slot stays held until the frame drops, then the backend hands it back to the driver with `SlotTable::pop_released`. The tensor is reachable only through the frame (`Frame::tensor()` borrows it), so nothing keeps reading a buffer after it has gone back. Frames keep their memory through what they own, never through a backend object, so they outlive `stop()` and the camera.
3. **One acquisition time per frame.** Backends convert the capture timestamp with `RealtimeClock` at dequeue and store it in `FrameMeta::realtime`; consumers never convert again.
4. **No silent fallback for caller buffers or contiguity.** Only SDK-allocated pools may fall back from Import to Export. Everything else is a dedicated error, and the camera stays open and stopped for recovery.

## Slot ownership

Each slot in a `SlotTable` is in one of three states:

- **Queued:** the driver owns the buffer and may write into it.
- **Held:** a frame references the buffer.
- **Released:** the frame dropped, and the buffer is waiting to go back to the driver.

`SlotTable::frame` takes a slot from queued to held and panics on anything else, because anything else would give one buffer two owners.

`SlotTable::detach` retires a table: its frames keep their memory, dropping them releases nothing, and the table hands out no more frames. When to use it:

- **On `close()`:** always.
- **On `stop()`:** only when the driver frees its buffers (Export, libcamera). The next buffers get a new table, so nothing ever writes into a detached frame.
- **Import `stop()`:** keeps its table. The same buffers re-attach, and frames held across the restart requeue when they drop.

Caller-provided pools are checked by one shared function, `pool::validate`, at `start()`. It checks count, native memory kind, format, size, pitch (when the backend knows the driver's pitch) and contiguity (when the memory is known to be non-contiguous), and returns the first `Rejection` with its slot.

## The mock backend

`backend/mock.rs` is both the SDK's test source and the worked example for third-party backends:

- it allocates host-memory tensors, or validates a caller pool with `pool::validate`;
- it keeps them in a `SlotTable`, with a FIFO standing in for the driver queue;
- it paces frames at the configured rate and counts a lost period as a drop when every buffer is held.

It behaves like an Import backend:

- `stop()` keeps the table;
- `set_buffers()` and dropping the camera detach it.

## The V4L2 backend

`backend/v4l2/` implements design §4.2 over [`edgefirst-v4l2`](https://crates.io/crates/edgefirst-v4l2).

| File | Contents |
|---|---|
| `device.rs` | Open (`O_RDWR \| O_NONBLOCK \| O_CLOEXEC`, `flock(LOCK_EX \| LOCK_NB)` when exclusive), the capture and streaming capability check, error mapping, enumeration of formats, sizes and rates |
| `format.rs` | V4L2 fourcc ↔ `PixelFormat`, preferring single-buffer NV12 over `NV12M` |
| `negotiate.rs` | `TRY_FMT` → `S_FMT` (field `NONE`, a 64-byte-aligned pitch when the SDK allocates) → `S_PARM` → `G_PARM` |
| `controls.rs` | Mirror, exposure, gain and white balance over V4L2 control IDs, plus every integer, boolean and menu control as `Control::Custom` |
| `quirks.rs` | The i.MX 8M Plus ISP (vvcam): flip through `V4L2_CID_VIV_EXTCTRL` JSON |
| `mod.rs` | The camera: buffers, `start()`, the dequeue loop, `stop()` and close |

**Negotiation.** The driver's answer is authoritative. A format the driver substitutes is `UnsupportedFormat`; an adjusted size or rate is reported in `StreamConfig`, never an error. SDK pools are allocated after `S_FMT` at the `bytesperline` and `sizeimage` the driver returned: when that pitch is not the tensor crate's 64-byte padding (vvcam returns a packed pitch), the pool is a raw DMA-BUF of the driver's size described at its pitch.

**Buffers.** Import pools come from `TensorDyn::image_desc` with `ImageDesc::with_contiguous` while `Contiguity::Required`, so they are CMA or `ContiguousUnavailable`. `config().contiguous` reports what the pool is known to be. Export buffers are `REQBUFS(MMAP)` and `EXPBUF`, wrapped with `TensorDyn::from_fd` at the driver's pitch; `NV12M` becomes a two-plane tensor through `Tensor::from_planes`. Import of two-buffer formats is not supported: `Auto` exports them and `Import` is `InvalidConfig`.

**`start()`.** Every slot is queued, then the stream runs to its first frame, within 5 s. For an SDK pool under `Auto`, any refusal on the way (an errno at `REQBUFS`, `QBUF` or `STREAMON`, an errored first buffer, or no frame in time) frees the queue and switches to Export, logged once. A caller pool is never replaced: a static mismatch or a driver refusal is `BuffersRejected`, with the errno and slot when the driver named one, and the pool goes back to the caller through `take_buffers`. With no first frame in time, the error is `NotReady`: vvcam delivers nothing for about 4 s after `isp_media_server` restarts. The first frame is kept and returned by the next `next_frame`.

**Dequeue.** `next_frame` first returns released slots to the driver. While nothing is queued it polls in 20 ms slices, so a slot released on another thread goes back promptly. Errored buffers are skipped and counted. The `V4L2_BUF_FLAG_TIMESTAMP_*` and `TSTAMP_SRC_*` bits map to `CaptureClock` and `TimestampSource`, logged once per camera. Drops are counted from timestamp gaps against the negotiated frame interval, because vvcam's sequence does not count them.

**Stop and close.** `stop()` is `STREAMOFF` then `REQBUFS(0)`. Import keeps its tensors and re-queues them on `start()`, so held frames requeue when they drop. Export's buffers are orphaned by `REQBUFS(0)`, so the table detaches and its frames keep their `EXPBUF` fds. Dropping the camera does the same and releases the lock, so the device reopens at once. A driver without `V4L2_BUF_CAP_SUPPORTS_ORPHANED_BUFS` cannot do that: when it holds Export frames at close, the camera keeps its open file (and the lock) in the slot table with `SlotTable::keep_alive`, and the device closes when the last frame drops.

**Testing hooks.** `CameraBuilder::fault` injects `import-qbuf` (the first Import `QBUF` is refused) and `no-orphan` (the driver is treated as unable to orphan buffers). It is honoured only in debug builds and is not part of the stable API.

## The file backend

`backend/file/` (feature `file`) plays a recording made by camera 2.x: `<name>.h264`, a raw Annex B stream, and its `<name>.json` sidecar.

| File | Contents |
|---|---|
| `sidecar.rs` | Sidecar v1: `fps`, `width`, `height` and colorimetry (tensor-crate labels) configure the stream; every other field (`camera_info`, `tf_static`) is kept verbatim for `Camera::extras()` |
| `annexb.rs` | Reads the file in chunks and splits it with `edgefirst_codec::video::access_units`; an access unit is complete once the next one starts or the file ends. Bytes outside any access unit are skipped with a warning |
| `decode.rs` | The `Decode` seam the source drives, implemented over `edgefirst_codec::video::VideoDecoder`; unit tests drive a mock instead |
| `mod.rs` | The `Camera`: pacing, looping, stamps, `encoded()` |

**Decoding.** One access unit per `decode` call, as the decoder requires. Pictures land in the decoder's own NV12 pool (`--camera-buffers` frames may be held); a frame owns its `DecodedFrame`, and dropping the frame gives the buffer back to the decoder, which is the slot rule with the decoder as the driver. Caller pools are refused with `BuffersRejected`: decoders write at the coded size (1088 rows for 1080p) and their own pitch. When every picture is held, `next_frame` waits for one to drop, up to its timeout, as a capture driver whose buffers are all dequeued does.

**Pacing and stamps.** Frames are due one interval apart at the sidecar's rate or the requested one (`frame_rate`, `Control::FrameRate`), counted from the previous due time so decode time and late wake-ups do not add drift. A frame more than an interval behind re-anchors the schedule rather than catching up, and no frame follows the previous delivery by less than half an interval, so lateness never turns into a burst. Each frame is stamped at its paced delivery in the clock the mock uses, and converted with `RealtimeClock` like a live frame.

**Looping and the end.** At the end of the file the decoder is drained. With `CameraBuilder::looping(true)` the file restarts and `seq` keeps counting; otherwise `next_frame` returns `ErrorKind::EndOfStream`. A file with no decodable pictures ends rather than looping forever.

**`encoded()`.** Each frame carries every access unit read since the previous picture came out of the decoder, in file order. Forwarding them reproduces the recording byte for byte even when the decoder holds pictures back.

**Damage.** A unit the decoder rejects is skipped with a warning and counted in `stats().errors`; a picture the decoder reports as damaged is counted and not delivered, its units going out with the next frame. After 30 rejected units in a row the decoder is taken to have failed (`ErrorKind::Backend`).

**Platforms.** Decoding is V4L2 memory-to-memory only in phase 1, so playback runs on i.MX targets; elsewhere `open()` fails with `NotFound` naming the decoder.

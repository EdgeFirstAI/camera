# Architecture

This file describes the crate's modules, the rules every backend follows, how slots are owned, and each backend. The service built on it is described in the [application architecture](../../ARCHITECTURE.md).

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
| `backend/` | Backend dispatch; `mock.rs`, `v4l2/` and `libcamera/` on Linux, `media_foundation/` on Windows, `file/`, and `rate.rs`, the measured-rate meter every capture backend shares |

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

## Desktop platforms

The core carries what the macOS and Windows backends share, so each backend adds only its own capture code.

- **Sources.** `mediafoundation:<id>` (Windows, the Media Foundation backend below) and `avfoundation:<id>` (macOS and iOS) parse on every platform, like `libcamera:<id>`. In a build without the backend, `open()` fails with `NotFound` ("the Media Foundation backend is not available in this build"), as a Linux backend that is not built does.
- **Capture clock.** `CaptureClock::Qpc` is the Windows performance counter in nanoseconds. Media Foundation stamps capture samples with it in 100 ns units: on a USB webcam, sample times and `MFSampleExtension_DeviceTimestamp` both equal `QueryPerformanceCounter` read 0.1 to 1.3 ms before `ReadSample` returns. `RealtimeClock` converts it on Windows and returns `Backend` elsewhere. The mock and file sources stamp in `Qpc` on Windows too, so their frames go through the same conversion. The Apple host clock stays `Unknown` until the AVFoundation backend measures it.
- **Rate meter.** `backend/rate.rs` builds on every target, so every backend that measures reports `measured_frame_rate()` with the same meter.
- **Wait handle.** `WaitHandle` is a Unix descriptor. Windows has no form of it yet, and Windows backends return `None` from `wait_handle()`.
- **Camera permission.** macOS and Windows refuse capture until the user allows camera access. A backend reports that as `ErrorKind::PermissionDenied`, and the message says where to allow it: Settings > Privacy & security > Camera, with "Let desktop apps access your camera" on, on Windows; System Settings > Privacy & Security > Camera, for the application or terminal running the program, on macOS. The application exits with 77 (`NOPERM`). On Linux, `EACCES` keeps its `Io` kind.

## The mock backend

`backend/mock.rs` is both the SDK's test source and the worked example for third-party backends:

- it allocates host-memory tensors, or validates a caller pool with `pool::validate`;
- it keeps them in a `SlotTable`, with a FIFO standing in for the driver queue;
- it paces frames at the configured rate and skips a period when every buffer is held.

It behaves like an Import backend:

- `stop()` keeps the table;
- `set_buffers()` and dropping the camera detach it.

## The V4L2 backend

`backend/v4l2/` is built over [`edgefirst-v4l2`](https://crates.io/crates/edgefirst-v4l2).

| File | Contents |
|---|---|
| `device.rs` | Open (`O_RDWR \| O_NONBLOCK \| O_CLOEXEC`, `flock(LOCK_EX \| LOCK_NB)` when exclusive), the capture and streaming capability check, error mapping, enumeration of formats, sizes and rates |
| `format.rs` | V4L2 fourcc ↔ `PixelFormat`, preferring single-buffer NV12 over `NV12M` |
| `negotiate.rs` | `TRY_FMT` → `S_FMT` (field `NONE`, a 64-byte-aligned pitch when the SDK allocates) → `S_PARM` → `G_PARM` |
| `controls.rs` | Mirror, exposure, gain and white balance over V4L2 control IDs, plus every integer, boolean and menu control as `Control::Custom` |
| `quirks.rs` | The i.MX 8M Plus ISP (vvcam): flip and frame rate through `V4L2_CID_VIV_EXTCTRL` JSON |
| `mod.rs` | The camera: buffers, `start()`, the dequeue loop, `stop()` and close |

**Negotiation.** The driver's answer is authoritative. A format the driver substitutes is `UnsupportedFormat`; an adjusted size or rate is reported in `StreamConfig`, never an error. SDK pools are allocated after `S_FMT` at the `bytesperline` and `sizeimage` the driver returned: when that pitch is not the tensor crate's 64-byte padding (vvcam returns a packed pitch), the pool is a raw DMA-BUF of the driver's size described at its pitch.

**Buffers.** Import pools come from `TensorDyn::image_desc` with `ImageDesc::with_contiguous` while `Contiguity::Required`, so they are CMA or `ContiguousUnavailable`; with `MemoryStrategy::Auto`, a machine with no `linux,cma` heap gets the system DMA-BUF heap instead and one where no heap can be opened gets Export, while a CMA heap that is full or not accessible is still an error. `config().contiguous` reports what the pool is known to be. Export buffers are `REQBUFS(MMAP)` and `EXPBUF`, wrapped with `TensorDyn::from_fd` at the driver's pitch; `NV12M` becomes a two-plane tensor through `Tensor::from_planes`. Import of two-buffer formats is not supported: `Auto` exports them and `Import` is `InvalidConfig`.

**`start()`.** Every slot is queued, then the stream runs to its first frame, within 5 s. For an SDK pool under `Auto`, any refusal on the way (an errno at `REQBUFS`, `QBUF` or `STREAMON`, an errored first buffer, or no frame in time) frees the queue and switches to Export, logged once. A caller pool is never replaced: a static mismatch or a driver refusal is `BuffersRejected`, with the errno and slot when the driver named one, and the pool goes back to the caller through `take_buffers`. With no first frame in time, the error is `NotReady`: vvcam delivers nothing for about 4 s after `isp_media_server` restarts, and some vvcam releases answer `S_FMT` with `EAGAIN` meanwhile, which is `NotReady` too. The first frame is kept and returned by the next `next_frame`.

**Frame rate on vvcam.** vvcam releases that reject `S_PARM` (Maivin's) run the sensor at its mode's rate (1080p60). A rate `S_PARM` refuses is sent as the ISP's `s.fps` JSON command once the stream runs, at every `start()` because the ISP may return to the mode's rate after a stop, and on `set_control(FrameRate)`. When the ISP accepts it, `config().frame_rate` follows the requested rate.

**Dequeue.** `next_frame` first returns released slots to the driver. While nothing is queued it polls in 20 ms slices, so a slot released on another thread goes back promptly. Errored buffers are skipped and counted. The `V4L2_BUF_FLAG_TIMESTAMP_*` and `TSTAMP_SRC_*` bits map to `CaptureClock` and `TimestampSource`, logged once per camera.

**Stop and close.** `stop()` is `STREAMOFF` then `REQBUFS(0)`. Import keeps its tensors and re-queues them on `start()`, so held frames requeue when they drop. Export's buffers are orphaned by `REQBUFS(0)`, so the table detaches and its frames keep their `EXPBUF` fds. Dropping the camera does the same and releases the lock, so the device reopens at once. A driver without `V4L2_BUF_CAP_SUPPORTS_ORPHANED_BUFS` cannot do that: when it holds Export frames at close, the camera keeps its open file (and the lock) in the slot table with `SlotTable::keep_alive`, and the device closes when the last frame drops.

**Testing hooks.** `CameraBuilder::fault` injects `import-qbuf` (the first Import `QBUF` is refused) and `no-orphan` (the driver is treated as unable to orphan buffers). It is honoured only in debug builds and is not part of the stable API.

## The libcamera backend

`backend/libcamera/` (feature `libcamera`, Linux) is built over [`edgefirst-libcamera-sys`](../libcamera-sys), which loads a C shim built for the installed libcamera at run time. Nothing links libcamera, so one binary runs with or without it; without it, `enumerate()` lists no libcamera cameras and opening one is `NotFound`.

| File | Contents |
|---|---|
| `manager.rs` | The process-wide `CameraManager`: started by the first camera or enumeration that needs it, stopped when the last drops it; the shim loaded once; the pipeline match list |
| `describe.rs` | DRM fourcc ↔ `PixelFormat`, descriptors, capture drivers from `properties::SystemDevices` and sysfs, and `probe()` |
| `mod.rs` | `Handle` (an acquired shim camera) and the `Camera`: buffers, `start()`, the dequeue loop, controls, `stop()` and close |
| `tests.rs` | The backend on the shim's mock (memfd buffers, completions on dequeue, cancelled completions at stop) |

**Configuration.** One stream in the `VideoRecording` role. A requested format libcamera replaces is `UnsupportedFormat`; an adjusted size is reported in `StreamConfig` and logged. Mirroring is `CameraConfiguration::orientation`, so it is applied at configuration: `set_control(Mirror)` reconfigures while stopped, and a pipeline that cannot mirror (the i.MX 95 `nxp/neo`) leaves the stream as it was and reports `Unsupported(Camera)`.

**Buffers.** Export only: libcamera's allocator creates the buffers with V4L2 `MMAP` + `EXPBUF` and orphans them at once, so each plane's dma-buf lives as long as a duplicate of its fd. The backend duplicates every plane fd and wraps them: one tensor when the planes share a dma-buf at consecutive offsets (RPi `pisp`), or a two-plane tensor through `Tensor::from_planes` when they are separate dma-bufs (i.MX 95 `nxp/neo`). `MemoryStrategy::Import` is `InvalidConfig` at open, and a caller pool is `BuffersRejected` at `start()` and goes back through `take_buffers`.

**Requests and completions.** Each buffer has one request whose cookie is its slot. libcamera's thread only queues completions in the shim and signals an eventfd, which is the `WaitHandle`; every call into libcamera is made from the thread that owns the camera. A dropped frame's slot is requeued with `Request::reuse(ReuseBuffers)`, carrying any controls set since the previous request.

**Controls.** The `ControlSet` comes from the camera's `ControlInfoMap` after configuration: `FrameDurationLimits` as `FrameRate`, `ExposureTime` as `Exposure` (µs), `AnalogueGain` as `Gain` (a `Float` multiplier) and `ColourTemperature` as `WhiteBalance`. Values outside the range are clamped and reported as `Clamped`; a control the camera lacks is `Unsupported(Camera)`. Manual exposure and gain use `ExposureTimeMode` and `AnalogueGainMode` where the pipeline has them and `AeEnable` elsewhere. Manual white balance needs a settable colour temperature; neo takes only `ColourGains`, which would need a calibration, so it is reported unsupported.

**Frame rate.** A requested rate is sent as `FrameDurationLimits`, but some pipelines ignore it (neo runs the OS08A20 at 60 fps), and neo's `FrameDuration` metadata does not match its frame interval either (15.97 ms at 60 fps). `config().frame_rate` is the rate the latest frame's `FrameDuration` metadata reports, so it follows auto-exposure where the metadata does, as PiSP's does. `Camera::measured_frame_rate()` reports separately the rate measured from sensor timestamps (`backend/rate.rs`, shared with V4L2): frames over time across a window of about a second, skipping the first two intervals of a start. It is the plain mean, because any filter would need a claim about what the camera should be doing, and a mean depends only on the first and last timestamps of its window, so jitter cancels out: a Brio 101 stamps 32 and 36 ms at a true 30 fps. A window with a stall in it reads low, including one in which the application wasn't reading, so a caller that stopped reading on purpose calls `restart_frame_rate_measurement()`. On neo, `set_control` still answers `Clamped` with the rate `FrameDurationLimits` was clipped to, which the pipeline then ignores.

**Lost frames are not reported.** No per-frame signal for them holds across cameras. `FrameBuffer` sequence numbers show no gap across a one-second stall on PiSP and neo, nor do V4L2's on vvcam, though the ISI's and UVC's do; neo's `FrameDuration` never changes; and not every V4L2 driver reports a rate. A timestamp gap cannot separate a lost frame from a longer exposure either, so the SDK exposes no count until this is researched and tested on more platforms and under changing light.

**Rates just after boot.** Capture timestamps are on `CLOCK_MONOTONIC`, which time synchronisation slews: on an i.MX 95 it ran 2.3% slow against `CLOCK_MONOTONIC_RAW` right after boot and settled within about 30 s. A rate measured in that time is off by as much, and it corrects itself as the measurement carries on.

**Timestamps.** `SensorTimestamp`, which the pipelines copy from the capture node's V4L2 buffer timestamp: `CLOCK_MONOTONIC`, at frame start on the RPi receivers (`rp1-cfe`, `unicam`) and at frame end elsewhere. The capture drivers come from `properties::SystemDevices` through `/sys/dev/char`.

**`probe()`.** Opens the camera, configures each size once and reads the `FrameDurationLimits` the pipeline then reports, as `rpicam-hello --list-cameras` does. Sizes offered as a range become the standard sizes inside it. Each size is configured in NV12 (or YUYV), and its rates apply to every format: the rate follows the sensor mode, and the RPi 5 `pisp` pipeline lists VYUY but aborts the process when asked to validate it.

**Stop and close.** `stop()` stops the camera, drains the cancelled completions and frees the buffers; the table detaches and held frames keep their duplicated fds, so nothing writes into them and the camera restarts at once. Dropping the camera does the same and releases it.

**Pipelines.** On the i.MX 95, the raw-only `imx8-isi` pipeline claims the camera unless `LIBCAMERA_PIPELINES_MATCH_LIST=nxp/neo` is set; enumeration warns when a camera offers only raw formats. `set_libcamera_pipelines` sets the list for library users before the `CameraManager` starts; the SDK never switches pipelines on its own.

## The Media Foundation backend

`backend/media_foundation/` (feature `media-foundation`, Windows) captures with an `IMFSourceReader` over the `windows` crate. The feature is on by default and builds only on Windows, as `v4l2` and `libcamera` build only on Linux. The `windows` crate links `mfplat.dll`, `mf.dll` and `mfreadwrite.dll` when the program loads, so on a Windows N edition a build with the feature needs the Media Feature Pack to start.

| File | Contents |
|---|---|
| `com.rs` | COM's multithreaded apartment joined once per calling thread, the Media Foundation start-up hold, HRESULT → `ErrorKind`, attribute strings |
| `describe.rs` | Devices from `MFEnumDeviceSources`, native media types, subtype ↔ `PixelFormat`, colorimetry from the media type |
| `select.rs` | How each native subtype is packed and the format it delivers, the native type for a request, and the formats `enumerate()` and `probe()` report |
| `copy.rs` | Copying a locked sample, top-down or bottom-up, into a tensor's rows, repacked as the delivered format |
| `controls.rs` | Exposure, gain and white balance through `IAMCameraControl` and `IAMVideoProcAmp` |
| `mod.rs` | The camera: the reader's callback and its sample queue, the pool, `start()`, `next_frame`, `stop()` and close |

**Sources.** `mediafoundation:<id>` names a camera by its symbolic link (the descriptor's `id`), compared without case, or by its index in `enumerate()`. Each `enumerate()` activates every camera once to read its media types.

**Modes.** A camera lists one native media type per subtype, size and rate, and the backend only ever sets one of them: Media Foundation never converts, scales or resamples the stream. Each uncompressed subtype delivers one format, repacked without colour conversion: NV12, I420 and YV12 as NV12 (the chroma planes interleaved), YUY2 and UYVY as YUYV, ARGB32, RGB32 and RGB24 as BGRA (RGB24 and RGB32 with an opaque alpha). Compressed types (MJPEG, H.264) are never chosen. A request picks a format, else the current type's; then an exact size, else the nearest by area; then the nearest rate at that size (the highest without a request, or the current one at the current size); and among types left, the one delivered as it is. A format the camera does not deliver is `UnsupportedFormat`. Each format is reported at the sizes and rates of the types that deliver it. The QuickCam Pro for Notebooks offers only RGB24 and I420, so it delivers BGRA and NV12.

The video processor (`MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING`) is not used because it re-stamps samples: through it, I420 to NV12 sample times scattered from 15 ms ahead of `QueryPerformanceCounter` to 38 ms behind it, where the camera's own samples trail it by 0.15 to 1 ms.

**Buffers.** The SDK's or the caller's (Import), copied into from each sample. Media Foundation's own textures cannot be wrapped as tensors. Given an `IMFDXGIDeviceManager` on the tensor crate's device, the reader delivers single textures (not arrays), but in `DXGI_FORMAT_NV12` and `DXGI_FORMAT_YUY2`, which D3D11 tensors do not describe (they hold NV12 as an `R8` texture 1.5 times the height). Even its ARGB32 textures, whose format matches, lack the `D3D11_RESOURCE_MISC_SHARED_NTHANDLE` sharing that `from_d3d11_texture` requires. Its sample pool also holds only two or three samples: a third held sample stops capture with `MF_E_SAMPLEALLOCATOR_EMPTY`. So the reader runs without a device manager and delivers system memory, and each sample is copied into a slot. For a USB camera that is the upload Media Foundation would make itself. The SDK pool is D3D11 tensors (`TensorMemory::DmaBuf` on Windows) on the tensor crate's device, or host memory when no D3D11 device is available. A caller pool may be D3D11 or host tensors of the configured size and format. `MemoryStrategy::Export` is `InvalidConfig` at open.

**Capture.** The reader runs in asynchronous mode. Its callback, on a Media Foundation work-queue thread, only queues the sample and requests the next; it holds up to the pool depth of samples and drops a sample that arrives when the queue is full, as a driver drops a frame with no free buffer. `next_frame` waits on the queue with the caller's timeout, takes a free slot (or one whose frame has dropped, polling every 20 ms while every slot is held), and copies the oldest sample in through `IMF2DBuffer2::Lock2DSize`, on the caller's thread. Uploading from the callback instead, while another thread read D3D11 frames back as the application does to publish them inline, held Media Foundation's callbacks back for 5 s every few minutes, with the camera still capturing; with host-memory buffers, or with the upload on the caller's thread, it never did. Stream ticks and discontinuities are logged at debug level, not counted. A stream error, end of stream, or a media type the device changes while streaming fails the next `next_frame`. A sample that fails to copy is skipped and counted in `stats().errors`, within the caller's timeout; 30 in a row are reported. A sample buffer that does not report its pitch, in a media type that states no default stride, is laid out as Media Foundation does: RGB bottom-up with rows padded to 4 bytes, YUV top-down.

**Timestamps.** The sample time, `CaptureClock::Qpc`, `TimestampSource::EndOfFrame`: the counter as the frame arrived (see Desktop platforms).

**Controls.** `FrameRate` is the native types' rates at the current size. `set_control(FrameRate)` picks the nearest native type while stopped, and is `InvalidConfig` while streaming, because Media Foundation sets the rate with the format. Exposure is in log2 seconds in `IAMCameraControl` and reported in microseconds; gain is in the driver's units and white balance in kelvin. A property the driver reports no range for is not offered. Mirroring is `Unsupported(Backend)`.

**`start()`, stop and close.** `start()` waits up to 5 s for the first frame, so a refused camera permission or a dead device fails there, and keeps that frame for the next `next_frame`. `stop()` flushes the reader and waits for its `OnFlush`; the pool is kept, so frames held across a restart requeue when they drop. Closing flushes, detaches the pool and shuts the media source down: held frames keep their memory and nothing writes into them again, and the camera reopens at once.

## The file backend

`backend/file/` (feature `file`) plays a recording: `<name>.h264`, a raw Annex B stream, and its `<name>.json` sidecar.

The sidecar is the public `Sidecar` (`sidecar.rs`, feature `file`), which recorders also write: version 1, whose `fps`, `width`, `height` and colorimetry (tensor-crate labels) configure the stream, and every other field (`camera_info`, `tf_static`) is kept verbatim as `extras` for `Camera::extras()`. It writes a whole rate as an integer and unspecified colorimetry as empty labels, as 2.x reads them.

| File | Contents |
|---|---|
| `annexb.rs` | Reads the file in chunks and splits it with `edgefirst_codec::video::access_units`; an access unit is complete once the next one starts or the file ends. Bytes outside any access unit are skipped with a warning |
| `decode.rs` | The `Decode` seam the source drives, implemented over `edgefirst_codec::video::VideoDecoder`; unit tests drive a mock instead |
| `mod.rs` | The `Camera`: pacing, looping, stamps, `encoded()` |

**Decoding.** One access unit per `decode` call, as the decoder requires. Pictures land in the decoder's own NV12 pool (`--camera-buffers` frames may be held); a frame owns its `DecodedFrame`, and dropping the frame gives the buffer back to the decoder, which is the slot rule with the decoder as the driver. Caller pools are refused with `BuffersRejected`: decoders write at the coded size (1088 rows for 1080p) and their own pitch. When every picture is held, `next_frame` waits for one to drop, up to its timeout, as a capture driver whose buffers are all dequeued does.

**Pacing and stamps.** Frames are due one interval apart at the sidecar's rate or the requested one (`frame_rate`, `Control::FrameRate`), counted from the previous due time so decode time and late wake-ups do not add drift. A frame more than an interval behind re-anchors the schedule rather than catching up, and no frame follows the previous delivery by less than half an interval, so lateness never turns into a burst. Each frame is stamped at its paced delivery in the clock the mock uses, and converted with `RealtimeClock` like a live frame.

**Looping and the end.** At the end of the file the decoder is drained. With `CameraBuilder::looping(true)` the file restarts and `seq` keeps counting; otherwise `next_frame` returns `ErrorKind::EndOfStream`. A file with no decodable pictures ends rather than looping forever.

**`encoded()`.** Each frame carries every access unit read since the previous picture came out of the decoder, in file order. Forwarding them reproduces the recording byte for byte even when the decoder holds pictures back.

**Damage.** A unit the decoder rejects is skipped with a warning and counted in `stats().errors`; a picture the decoder reports as damaged is counted and not delivered, its units going out with the next frame. After 30 rejected units in a row the decoder is taken to have failed (`ErrorKind::Backend`).

**Platforms.** Decoding is currently V4L2 memory-to-memory only, so playback runs on i.MX targets; elsewhere `open()` fails with `NotFound` naming the decoder.

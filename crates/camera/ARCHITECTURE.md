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
| `backend/` | Backend dispatch; `mock.rs` today |

## Rules every backend follows

1. **Requests are reported, never enforced.** A backend applies what the device accepts, records the result in `StreamConfig`, and reports controls as `Applied`. It never drops frames, rescales or otherwise alters the stream to meet a request.
2. **A buffer is never handed back while a frame references it.** Backends keep their buffers in a `SlotTable`. A frame holds an `Arc<TensorDyn>` for its slot; the slot stays held until the frame drops, then the backend hands it back to the driver with `SlotTable::pop_released`. Frames keep their memory through that `Arc`, never through a backend object, so they outlive `stop()` and the camera.
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

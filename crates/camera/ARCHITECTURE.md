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
| `timestamp.rs` | `Timestamp`, `CaptureClock`, `TimestampSource`, `RealtimeClock` |
| `enumerate.rs` | `CameraDescriptor`, `FormatInfo`, `Sizes`, `Rates`, `Mode`, `modes()` |
| `error.rs` | `Error`, `ErrorKind`, `Rejection` |
| `backend/` | Backend dispatch; `mock.rs` today |

## Rules every backend follows

1. **Requests are reported, never enforced.** A backend applies what the device accepts, records the result in `StreamConfig`, and reports controls as `Applied`. It never drops frames, rescales or otherwise alters the stream to meet a request.
2. **A buffer is never handed back while a frame references it.** A frame holds an `Arc<TensorDyn>` for its slot and returns the slot through `SlotRelease` when it drops. Frames held across `stop()` or after the camera is dropped keep their memory through a reference the backend does not own.
3. **One acquisition time per frame.** Backends convert the capture timestamp with `RealtimeClock` at dequeue and store it in `FrameMeta::realtime`; consumers never convert again.
4. **No silent fallback for caller buffers or contiguity.** Only SDK-allocated pools may fall back from Import to Export. Everything else is a dedicated error, and the camera stays open and stopped for recovery.

## The mock backend

`backend/mock.rs` is both the SDK's test source and the worked example for third-party backends:

- it allocates host-memory tensors, or validates a caller pool against the negotiated format and size;
- it builds frames with `Frame::new`;
- it returns slots through a shared free list implementing `SlotRelease`;
- it paces frames at the configured rate and counts a lost period as a drop when every buffer is held.

`start()` builds a fresh free list, so a frame held across a restart releases into the old list and its slot is never reused while it lives.

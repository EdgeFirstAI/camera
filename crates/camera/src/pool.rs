// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Slot ownership shared by every backend, and caller-pool validation.
//!
//! A [`SlotTable`] owns one [`TensorDyn`] per capture buffer and tracks who
//! holds each slot. A slot moves between three states:
//!
//! - **Queued**: the driver owns the buffer and may write into it.
//! - **Held**: a [`Frame`] references the buffer; the driver must not touch
//!   it.
//! - **Released**: the last frame dropped; the backend hands the slot back
//!   to the driver on its next pass ([`SlotTable::pop_released`]).
//!
//! A buffer is therefore never handed to the driver while a frame
//! references it. [`SlotTable::detach`] retires a table: its frames keep
//! their memory, but dropping them no longer releases anything, and the
//! table hands out no more frames. Backends detach on `close()`, and on
//! `stop()` when the driver frees its buffers (Export, libcamera); the next
//! buffers get a new table, so nothing ever writes into a detached frame.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use edgefirst_tensor::TensorDyn;

use crate::{Frame, FrameMeta, SlotRelease};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Queued,
    Held,
    Released,
}

#[derive(Debug)]
struct State {
    detached: bool,
    slots: Vec<SlotState>,
    released: VecDeque<usize>,
}

struct Shared {
    state: Mutex<State>,
    /// Kept until the table and every frame from it have dropped.
    keep_alive: Mutex<Vec<Box<dyn std::any::Any + Send + Sync>>>,
}

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock leaves the slot bookkeeping
        // consistent (every update is a single assignment), so recover.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Returns slots to a [`SlotTable`].
#[derive(Debug)]
struct Guard {
    shared: Arc<Shared>,
}

impl SlotRelease for Guard {
    fn release(&self, slot: usize) {
        let mut state = self.shared.lock();
        if state.detached {
            // The frame keeps its memory; the driver no longer uses this
            // buffer set.
            return;
        }
        if state.slots.get(slot) == Some(&SlotState::Held) {
            state.slots[slot] = SlotState::Released;
            state.released.push_back(slot);
        }
    }
}

/// Slot ownership for one pool of capture buffers.
///
/// Backends build a table from their buffers, mark slots queued as they hand
/// them to the driver, wrap each dequeued buffer with [`SlotTable::frame`],
/// and return released slots to the driver with
/// [`SlotTable::pop_released`]. Third-party backends use it the same way.
///
/// # Examples
///
/// ```
/// use edgefirst_camera::{CaptureClock, FrameMeta, SlotTable, Timestamp, TimestampSource};
/// use edgefirst_tensor::{CpuAccess, DType, PixelFormat, TensorDyn, TensorMemory};
///
/// let buffers = (0..2)
///     .map(|_| {
///         TensorDyn::image(16, 16, PixelFormat::Grey, DType::U8, Some(TensorMemory::Mem), CpuAccess::ReadWrite)
///     })
///     .collect::<Result<Vec<_>, _>>()?;
/// let table = SlotTable::new(buffers);
/// let ts = Timestamp { clock: CaptureClock::Monotonic, source: TimestampSource::EndOfFrame, nanos: 0 };
///
/// // The driver filled slot 0.
/// let frame = table.frame(FrameMeta::new(0, 0, ts));
/// assert_eq!(table.pop_released(), None, "held slots are never returned");
/// drop(frame);
/// assert_eq!(table.pop_released(), Some(0), "back to the driver");
/// # Ok::<(), edgefirst_tensor::Error>(())
/// ```
pub struct SlotTable {
    tensors: Vec<Arc<TensorDyn>>,
    shared: Arc<Shared>,
    guard: Arc<Guard>,
}

impl SlotTable {
    /// Builds a table over `tensors`; every slot starts queued (owned by
    /// the driver).
    pub fn new(tensors: Vec<TensorDyn>) -> Self {
        let len = tensors.len();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                detached: false,
                slots: vec![SlotState::Queued; len],
                released: VecDeque::new(),
            }),
            keep_alive: Mutex::new(Vec::new()),
        });
        let guard = Arc::new(Guard {
            shared: shared.clone(),
        });
        Self {
            tensors: tensors.into_iter().map(Arc::new).collect(),
            shared,
            guard,
        }
    }

    /// Number of slots.
    pub fn len(&self) -> usize {
        self.tensors.len()
    }

    /// Whether the table has no slots.
    pub fn is_empty(&self) -> bool {
        self.tensors.is_empty()
    }

    /// The tensor for `slot`.
    ///
    /// # Panics
    ///
    /// When `slot` is out of range.
    pub fn tensor(&self, slot: usize) -> &Arc<TensorDyn> {
        &self.tensors[slot]
    }

    /// The tensors, in slot order.
    pub fn tensors(&self) -> &[Arc<TensorDyn>] {
        &self.tensors
    }

    /// Wraps the buffer the driver just filled in `meta.slot` as a frame.
    /// The slot is held until the frame drops.
    ///
    /// # Panics
    ///
    /// When `meta.slot` is out of range, the slot is not queued (which would
    /// hand one buffer to two owners), or the table is detached.
    pub fn frame(&self, meta: FrameMeta) -> Frame {
        let slot = meta.slot;
        {
            let mut state = self.shared.lock();
            assert!(!state.detached, "slot table is detached");
            assert_eq!(
                state.slots[slot],
                SlotState::Queued,
                "slot {slot} is not owned by the driver"
            );
            state.slots[slot] = SlotState::Held;
        }
        let guard: Arc<dyn SlotRelease> = self.guard.clone();
        Frame::new(self.tensors[slot].clone(), meta, Some(guard))
    }

    /// The next slot whose frames have all dropped, now queued again for
    /// the driver; `None` when there is none.
    pub fn pop_released(&self) -> Option<usize> {
        let mut state = self.shared.lock();
        let slot = state.released.pop_front()?;
        state.slots[slot] = SlotState::Queued;
        Some(slot)
    }

    /// Marks a queued slot the driver failed to fill (for example an
    /// errored buffer) as released, so it is handed back on the next pass.
    pub fn recycle(&self, slot: usize) {
        let mut state = self.shared.lock();
        if state.slots[slot] == SlotState::Queued {
            state.slots[slot] = SlotState::Released;
            state.released.push_back(slot);
        }
    }

    /// Retires the table. Its frames keep their memory, but dropping them no
    /// longer releases a slot, and the table hands out no more frames. Use a
    /// new table for the driver's next buffers.
    pub fn detach(&self) {
        let mut state = self.shared.lock();
        state.detached = true;
        state.released.clear();
    }

    /// Keeps `owner` alive until this table and every frame it handed out
    /// have dropped. Backends use it to keep a device open while frames
    /// still reference its buffers, when the driver cannot orphan them.
    pub fn keep_alive(&self, owner: Box<dyn std::any::Any + Send + Sync>) {
        self.shared
            .keep_alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(owner);
    }

    /// Whether [`detach`](Self::detach) was called.
    pub fn is_detached(&self) -> bool {
        self.shared.lock().detached
    }

    /// Slots owned by the driver.
    pub fn queued(&self) -> usize {
        self.count(SlotState::Queued)
    }

    /// Slots held by frames.
    pub fn held(&self) -> usize {
        self.count(SlotState::Held)
    }

    fn count(&self, which: SlotState) -> usize {
        self.shared
            .lock()
            .slots
            .iter()
            .filter(|s| **s == which)
            .count()
    }
}

impl fmt::Debug for SlotTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.shared.lock();
        f.debug_struct("SlotTable")
            .field("len", &self.tensors.len())
            .field("detached", &state.detached)
            .field("slots", &state.slots)
            .finish_non_exhaustive()
    }
}

#[cfg(any(feature = "mock", feature = "v4l2", test))]
pub(crate) use validation::{validate, BufferInfo, PoolRequirements};

/// Caller-pool validation shared by backends.
#[cfg(any(feature = "mock", feature = "v4l2", test))]
mod validation {
    use edgefirst_tensor::{Contiguity as Heap, PixelFormat, TensorDyn, TensorMemory};

    use crate::{Contiguity, Rejection};

    /// What a backend requires of caller-provided buffers.
    #[derive(Debug, Clone)]
    pub(crate) struct PoolRequirements {
        pub(crate) format: PixelFormat,
        pub(crate) width: usize,
        pub(crate) height: usize,
        /// The pitch the driver returned, when the backend has negotiated one.
        pub(crate) row_stride: Option<usize>,
        pub(crate) min_count: usize,
        pub(crate) max_count: usize,
        pub(crate) contiguity: Contiguity,
        /// Memory kinds the backend can capture into.
        pub(crate) native: &'static [TensorMemory],
    }

    /// The properties of one caller-provided buffer that validation checks.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct BufferInfo {
        pub(crate) memory: TensorMemory,
        pub(crate) format: Option<PixelFormat>,
        pub(crate) width: Option<usize>,
        pub(crate) height: Option<usize>,
        pub(crate) row_stride: Option<usize>,
        /// `Some(false)` when the memory is known to be non-contiguous.
        pub(crate) contiguous: Option<bool>,
    }

    impl BufferInfo {
        pub(crate) fn of(tensor: &TensorDyn) -> Self {
            Self {
                memory: tensor.memory(),
                format: tensor.format(),
                width: tensor.width(),
                height: tensor.height(),
                row_stride: tensor.effective_row_stride(),
                contiguous: match tensor.contiguity() {
                    Heap::Contiguous => Some(true),
                    Heap::NonContiguous => Some(false),
                    _ => None,
                },
            }
        }
    }

    /// Checks caller-provided buffers against a backend's requirements,
    /// returning the first reason and slot for a refusal.
    pub(crate) fn validate(
        pool: &[BufferInfo],
        req: &PoolRequirements,
    ) -> Result<(), (Rejection, Option<usize>)> {
        if pool.len() < req.min_count || pool.len() > req.max_count {
            return Err((Rejection::Count, None));
        }
        for (slot, info) in pool.iter().enumerate() {
            let reject = |r| Err((r, Some(slot)));
            if !req.native.contains(&info.memory) {
                return reject(Rejection::NotNativeHandle);
            }
            if info.format != Some(req.format) {
                return reject(Rejection::Format);
            }
            if info.width != Some(req.width) || info.height != Some(req.height) {
                return reject(Rejection::Size);
            }
            if let Some(stride) = req.row_stride {
                if info.row_stride != Some(stride) {
                    return reject(Rejection::Pitch);
                }
            }
            if req.contiguity == Contiguity::Required && info.contiguous == Some(false) {
                return reject(Rejection::NotContiguous);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CaptureClock, Contiguity, Rejection, Timestamp, TimestampSource};
    use edgefirst_tensor::{CpuAccess, DType, PixelFormat, TensorMemory};

    fn table(n: usize) -> SlotTable {
        SlotTable::new(
            (0..n)
                .map(|_| {
                    TensorDyn::image(
                        8,
                        8,
                        PixelFormat::Grey,
                        DType::U8,
                        Some(TensorMemory::Mem),
                        CpuAccess::ReadWrite,
                    )
                    .unwrap()
                })
                .collect(),
        )
    }

    fn meta(slot: usize) -> FrameMeta {
        FrameMeta::new(
            slot,
            0,
            Timestamp {
                clock: CaptureClock::Monotonic,
                source: TimestampSource::EndOfFrame,
                nanos: 0,
            },
        )
    }

    #[test]
    fn held_slot_is_never_released_until_the_frame_drops() {
        let t = table(3);
        let f = t.frame(meta(1));
        assert_eq!((t.queued(), t.held()), (2, 1));
        assert_eq!(t.pop_released(), None);
        drop(f);
        assert_eq!(t.held(), 0);
        assert_eq!(t.pop_released(), Some(1));
        assert_eq!(t.queued(), 3);
        assert_eq!(t.pop_released(), None, "released once only");
    }

    #[test]
    fn release_order_is_preserved() {
        let t = table(4);
        let frames: Vec<_> = (0..4).map(|s| t.frame(meta(s))).collect();
        let mut frames = frames.into_iter();
        let (a, b, c, d) = (
            frames.next().unwrap(),
            frames.next().unwrap(),
            frames.next().unwrap(),
            frames.next().unwrap(),
        );
        drop(c);
        drop(a);
        drop(d);
        drop(b);
        let order: Vec<_> = std::iter::from_fn(|| t.pop_released()).collect();
        assert_eq!(order, [2, 0, 3, 1]);
    }

    #[test]
    fn detached_frames_keep_memory_and_never_release() {
        let t = table(2);
        let held = t.frame(meta(0));
        let done = t.frame(meta(1));
        drop(done);
        t.detach();
        assert!(t.is_detached());
        assert_eq!(t.pop_released(), None, "pending releases are discarded");
        let bytes = held.map_bytes(edgefirst_tensor::CpuAccess::Read).unwrap();
        assert_eq!(bytes.len(), 64, "a detached frame keeps its memory");
        drop(bytes);
        drop(held);
        assert_eq!(t.pop_released(), None, "a detached drop releases nothing");
    }

    #[test]
    #[should_panic(expected = "detached")]
    fn a_detached_table_hands_out_no_frames() {
        let t = table(2);
        t.detach();
        let _f = t.frame(meta(0));
    }

    #[test]
    fn a_detached_frame_outlives_its_table() {
        let t = table(2);
        let f = t.frame(meta(0));
        t.detach();
        drop(t);
        assert_eq!(f.height(), Some(8));
    }

    #[test]
    fn recycle_returns_an_unfilled_slot() {
        let t = table(2);
        t.recycle(1);
        assert_eq!(t.pop_released(), Some(1));
    }

    #[test]
    #[should_panic(expected = "not owned by the driver")]
    fn a_held_slot_cannot_be_framed_twice() {
        let t = table(2);
        let _a = t.frame(meta(0));
        let _b = t.frame(meta(0));
    }

    #[test]
    fn frames_release_from_other_threads() {
        let t = table(2);
        let f = t.frame(meta(0));
        std::thread::spawn(move || drop(f)).join().unwrap();
        assert_eq!(t.pop_released(), Some(0));
    }

    fn req() -> PoolRequirements {
        PoolRequirements {
            format: PixelFormat::Nv12,
            width: 1920,
            height: 1080,
            row_stride: Some(1920),
            min_count: 2,
            max_count: 32,
            contiguity: Contiguity::Required,
            native: &[TensorMemory::DmaBuf],
        }
    }

    fn good() -> BufferInfo {
        BufferInfo {
            memory: TensorMemory::DmaBuf,
            format: Some(PixelFormat::Nv12),
            width: Some(1920),
            height: Some(1080),
            row_stride: Some(1920),
            contiguous: None,
        }
    }

    #[test]
    fn buffer_info_reads_tensor_properties() {
        let t = TensorDyn::image(
            64,
            48,
            PixelFormat::Nv12,
            DType::U8,
            Some(TensorMemory::Mem),
            CpuAccess::ReadWrite,
        )
        .unwrap();
        let info = BufferInfo::of(&t);
        assert_eq!(info.memory, TensorMemory::Mem);
        assert_eq!(info.format, Some(PixelFormat::Nv12));
        assert_eq!((info.width, info.height), (Some(64), Some(48)));
        assert_eq!(info.row_stride, t.effective_row_stride());
        assert_eq!(info.contiguous, None);
    }

    #[test]
    fn valid_pool_passes() {
        assert_eq!(validate(&[good(), good()], &req()), Ok(()));
    }

    #[test]
    fn each_mismatch_names_its_reason_and_slot() {
        let cases = [
            (
                BufferInfo {
                    memory: TensorMemory::Pbo,
                    ..good()
                },
                Rejection::NotNativeHandle,
            ),
            (
                BufferInfo {
                    format: Some(PixelFormat::Yuyv),
                    ..good()
                },
                Rejection::Format,
            ),
            (
                BufferInfo {
                    height: Some(720),
                    ..good()
                },
                Rejection::Size,
            ),
            (
                BufferInfo {
                    row_stride: Some(1984),
                    ..good()
                },
                Rejection::Pitch,
            ),
            (
                BufferInfo {
                    contiguous: Some(false),
                    ..good()
                },
                Rejection::NotContiguous,
            ),
        ];
        for (bad, reason) in cases {
            assert_eq!(
                validate(&[good(), bad], &req()),
                Err((reason, Some(1))),
                "{reason:?}"
            );
        }
    }

    #[test]
    fn count_limits_apply() {
        assert_eq!(validate(&[good()], &req()), Err((Rejection::Count, None)));
        let many = vec![good(); 33];
        assert_eq!(validate(&many, &req()), Err((Rejection::Count, None)));
    }

    #[test]
    fn non_contiguous_memory_passes_when_any_is_allowed() {
        let req = PoolRequirements {
            contiguity: Contiguity::Any,
            ..req()
        };
        let pool = [
            good(),
            BufferInfo {
                contiguous: Some(false),
                ..good()
            },
        ];
        assert_eq!(validate(&pool, &req), Ok(()));
    }

    #[test]
    fn unknown_pitch_requirement_is_not_checked() {
        let req = PoolRequirements {
            row_stride: None,
            ..req()
        };
        let pool = [
            good(),
            BufferInfo {
                row_stride: Some(2048),
                ..good()
            },
        ];
        assert_eq!(validate(&pool, &req), Ok(()));
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Captured frames and their lifetime.
//!
//! A [`Frame`] holds a shared reference to its slot's tensor. Dropping the
//! frame hands the slot back to its backend through [`SlotRelease`]; a
//! buffer is never returned to the driver while a frame references it.

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use std::time::SystemTime;

use edgefirst_tensor::TensorDyn;

use crate::Timestamp;

/// Returns a slot to its backend when the last frame for it drops.
///
/// Implemented by backends, including third-party ones; see
/// [`Frame::new`].
pub trait SlotRelease: Send + Sync {
    /// Called once when the frame occupying `slot` is dropped.
    fn release(&self, slot: usize);
}

/// Layout of one image plane within its buffer.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlaneLayout {
    /// Native handle of the plane's buffer: a DMA-BUF fd on Linux, or -1
    /// when the buffer has none (host memory).
    pub handle: i64,
    /// Byte offset of the plane within the buffer.
    pub offset: usize,
    /// Bytes per row.
    pub stride: usize,
    /// Plane extent in bytes.
    pub size: usize,
    /// Bytes holding valid data for this frame.
    pub used: usize,
    /// DRM format modifier; 0 for linear.
    pub modifier: u64,
}

impl PlaneLayout {
    /// Creates a linear plane with every byte in use.
    pub fn new(handle: i64, offset: usize, stride: usize, size: usize) -> Self {
        Self {
            handle,
            offset,
            stride,
            size,
            used: size,
            modifier: 0,
        }
    }
}

/// Per-frame capture metadata.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct FrameMeta {
    /// Slot index within the backend's pool.
    pub slot: usize,
    /// SDK frame counter, continuous across restarts and file loops.
    pub seq: u64,
    /// Raw driver sequence number. It may not count drops (vvcam does not).
    pub driver_sequence: Option<u32>,
    /// Capture instant in the backend's clock.
    pub timestamp: Timestamp,
    /// Acquisition time, converted once at dequeue, or `None` when the
    /// capture clock cannot be converted.
    pub realtime: Option<SystemTime>,
    /// Plane layout.
    pub planes: Vec<PlaneLayout>,
    /// Total bytes holding valid data.
    pub bytes_used: usize,
    /// Encoded access unit the frame was decoded from (file source).
    pub encoded: Option<Arc<[u8]>>,
}

impl FrameMeta {
    /// Creates metadata with no planes, no realtime stamp and no encoded
    /// payload; set the remaining fields directly.
    pub fn new(slot: usize, seq: u64, timestamp: Timestamp) -> Self {
        Self {
            slot,
            seq,
            driver_sequence: None,
            timestamp,
            realtime: None,
            planes: Vec::new(),
            bytes_used: 0,
            encoded: None,
        }
    }
}

/// A captured frame: a tensor plus capture metadata.
///
/// `Frame` dereferences to the underlying [`TensorDyn`]. It is `Send` and
/// `Sync`, so it can be handed to encoder threads; the slot returns to the
/// backend when the frame drops.
pub struct Frame {
    tensor: Arc<TensorDyn>,
    meta: FrameMeta,
    release: Option<Arc<dyn SlotRelease>>,
}

impl Frame {
    /// Builds a frame. Third-party backends call this with a `release` that
    /// returns the slot to their driver only when the frame drops; pass
    /// `None` for a frame that owns its buffer outright.
    pub fn new(
        tensor: Arc<TensorDyn>,
        meta: FrameMeta,
        release: Option<Arc<dyn SlotRelease>>,
    ) -> Self {
        Self {
            tensor,
            meta,
            release,
        }
    }

    /// The frame's tensor.
    pub fn tensor(&self) -> &Arc<TensorDyn> {
        &self.tensor
    }

    /// The full metadata.
    pub fn meta(&self) -> &FrameMeta {
        &self.meta
    }

    /// SDK frame counter.
    pub fn seq(&self) -> u64 {
        self.meta.seq
    }

    /// Raw driver sequence number, which may not count drops.
    pub fn driver_sequence(&self) -> Option<u32> {
        self.meta.driver_sequence
    }

    /// Capture instant in the backend's clock.
    pub fn timestamp(&self) -> Timestamp {
        self.meta.timestamp
    }

    /// Acquisition time (wall clock), converted once at dequeue.
    pub fn realtime(&self) -> Option<SystemTime> {
        self.meta.realtime
    }

    /// Plane layout.
    pub fn planes(&self) -> &[PlaneLayout] {
        &self.meta.planes
    }

    /// Total bytes holding valid data.
    pub fn bytes_used(&self) -> usize {
        self.meta.bytes_used
    }

    /// Encoded access unit this frame was decoded from (file source).
    pub fn encoded(&self) -> Option<&Arc<[u8]>> {
        self.meta.encoded.as_ref()
    }

    /// Slot index within the backend's pool.
    pub fn slot(&self) -> usize {
        self.meta.slot
    }
}

impl Deref for Frame {
    type Target = TensorDyn;

    fn deref(&self) -> &TensorDyn {
        &self.tensor
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release.release(self.meta.slot);
        }
    }
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("meta", &self.meta)
            .field("tensor", &self.tensor)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CaptureClock, TimestampSource};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<usize>>);

    impl SlotRelease for Recorder {
        fn release(&self, slot: usize) {
            self.0.lock().unwrap().push(slot);
        }
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn frame_is_send_and_sync() {
        assert_send_sync::<Frame>();
    }

    #[test]
    fn drop_releases_slot_once() {
        let tensor = TensorDyn::image(
            8,
            4,
            edgefirst_tensor::PixelFormat::Grey,
            edgefirst_tensor::DType::U8,
            Some(edgefirst_tensor::TensorMemory::Mem),
            edgefirst_tensor::CpuAccess::ReadWrite,
        )
        .unwrap();
        let ts = Timestamp {
            clock: CaptureClock::Monotonic,
            source: TimestampSource::EndOfFrame,
            nanos: 1,
        };
        let recorder = Arc::new(Recorder::default());
        let frame = Frame::new(
            Arc::new(tensor),
            FrameMeta::new(3, 7, ts),
            Some(recorder.clone() as Arc<dyn SlotRelease>),
        );
        assert_eq!(frame.seq(), 7);
        assert_eq!(frame.width(), Some(8));
        drop(frame);
        assert_eq!(*recorder.0.lock().unwrap(), vec![3]);
    }
}

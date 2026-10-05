// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The object-safe camera trait every backend implements.

use std::time::Duration;

use edgefirst_tensor::TensorDyn;

use crate::{
    Applied, BufferPool, CameraDescriptor, Contiguity, Control, ControlId, ControlSet,
    ControlValue, Frame, Result, StreamConfig,
};

/// Capture counters.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CaptureStats {
    /// Frames delivered by `next_frame`.
    pub frames: u64,
    /// Buffers currently owned by the driver.
    pub queued: usize,
    /// Buffers currently held by frames.
    pub held: usize,
    /// Frames lost, counted from timestamp gaps against the negotiated
    /// frame interval.
    pub dropped: u64,
    /// `next_frame` calls that timed out.
    pub timeouts: u64,
    /// Buffers the driver flagged as errored and the backend skipped.
    pub errors: u64,
}

/// A pollable handle for event-loop integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WaitHandle {
    #[cfg(unix)]
    fd: std::os::fd::RawFd,
}

#[cfg(unix)]
impl WaitHandle {
    /// Wraps a file descriptor that becomes readable when a frame is ready.
    /// The backend keeps ownership of the descriptor.
    pub fn new(fd: std::os::fd::RawFd) -> Self {
        Self { fd }
    }

    /// The descriptor to poll for readability.
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        self.fd
    }
}

/// A camera opened by a backend.
///
/// The trait is object-safe and has no generics or lifetimes, so
/// `Box<dyn Camera>` is the handle every language binding wraps.
///
/// Requests are applied as well as the backend can; read
/// [`config`](Camera::config) for what was negotiated.
pub trait Camera: Send + std::fmt::Debug {
    /// Identity and capabilities of the camera.
    fn descriptor(&self) -> &CameraDescriptor;

    /// The negotiated configuration.
    fn config(&self) -> &StreamConfig;

    /// Controls this camera supports through this backend, with ranges.
    fn controls(&self) -> &ControlSet;

    /// Reads a control's current value.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfig`](crate::ErrorKind::InvalidConfig) when
    /// the control is not supported.
    fn get_control(&self, id: ControlId) -> Result<ControlValue>;

    /// Applies a control and reports what the backend did with it.
    ///
    /// # Errors
    ///
    /// Only for device failures; an unsupported control is
    /// [`Applied::Unsupported`], not an error.
    fn set_control(&mut self, ctl: Control) -> Result<Applied>;

    /// Starts streaming. Under `MemoryStrategy::Auto` this probes Import
    /// and may fall back to Export for SDK-allocated buffers.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::BuffersRejected`](crate::ErrorKind::BuffersRejected)
    /// for refused caller-provided buffers,
    /// [`ErrorKind::ContiguousUnavailable`](crate::ErrorKind::ContiguousUnavailable)
    /// when contiguous memory cannot be allocated,
    /// [`ErrorKind::NotReady`](crate::ErrorKind::NotReady) when the device
    /// cannot deliver frames yet. The camera stays open and stopped after
    /// any of these.
    fn start(&mut self) -> Result<()>;

    /// Stops streaming. The device stays open; held frames keep their
    /// memory.
    ///
    /// # Errors
    ///
    /// Device failures while stopping.
    fn stop(&mut self) -> Result<()>;

    /// Replaces the buffer pool while stopped.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfig`](crate::ErrorKind::InvalidConfig) while
    /// streaming.
    fn set_buffers(&mut self, pool: BufferPool) -> Result<()>;

    /// Hands a caller-provided pool back while stopped, for example after
    /// it was rejected. Returns an empty vector when the pool is the SDK's.
    fn take_buffers(&mut self) -> Vec<TensorDyn>;

    /// Changes the contiguity requirement while stopped.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfig`](crate::ErrorKind::InvalidConfig) while
    /// streaming.
    fn set_contiguity(&mut self, contiguity: Contiguity) -> Result<()>;

    /// Waits for the next frame; `None` waits indefinitely.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Timeout`](crate::ErrorKind::Timeout) when no frame
    /// arrives in time,
    /// [`ErrorKind::InvalidConfig`](crate::ErrorKind::InvalidConfig) when
    /// not streaming,
    /// [`ErrorKind::Disconnected`](crate::ErrorKind::Disconnected) when the
    /// device went away.
    fn next_frame(&mut self, timeout: Option<Duration>) -> Result<Frame>;

    /// Capture counters.
    fn stats(&self) -> CaptureStats;

    /// A pollable handle, where the backend has one.
    fn wait_handle(&self) -> Option<WaitHandle>;
}

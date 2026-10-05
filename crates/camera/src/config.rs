// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Stream requests and the configuration a backend actually negotiated.

use edgefirst_tensor::{Colorimetry, PixelFormat, TensorDyn};

/// How capture buffers are allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MemoryStrategy {
    /// Probe Import at `start()` and fall back to Export when the driver
    /// refuses the SDK's own buffers. Caller-provided buffers never fall
    /// back.
    #[default]
    Auto,
    /// The SDK or the caller allocates tensors and the driver imports them.
    Import,
    /// The driver allocates buffers and the SDK wraps them.
    Export,
}

/// The memory strategy in effect after negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResolvedMemory {
    /// SDK- or caller-allocated buffers imported by the driver.
    Import,
    /// Driver-allocated buffers wrapped by the SDK.
    Export,
}

/// Image mirroring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mirror {
    /// No mirroring.
    #[default]
    None,
    /// Mirror left to right.
    Horizontal,
    /// Mirror top to bottom.
    Vertical,
    /// Both, equivalent to a 180° rotation.
    Both,
}

/// Physical contiguity required of SDK-allocated buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Contiguity {
    /// Allocate from CMA only; fail with
    /// [`ErrorKind::ContiguousUnavailable`](crate::ErrorKind::ContiguousUnavailable)
    /// rather than fall back to non-contiguous memory.
    #[default]
    Required,
    /// Accept any memory the tensor crate allocates.
    Any,
}

/// Where capture buffers come from.
#[derive(Debug, Default)]
pub enum BufferPool {
    /// The SDK allocates (Import) or wraps the driver's buffers (Export).
    #[default]
    Sdk,
    /// Caller-provided tensors, imported by the driver. Never replaced by
    /// the SDK; a refusal is
    /// [`ErrorKind::BuffersRejected`](crate::ErrorKind::BuffersRejected).
    Provided(Vec<TensorDyn>),
}

/// What the caller asked for. Every field is a request; read
/// [`StreamConfig`] for what was configured.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct StreamRequest {
    /// Requested size, or `None` to keep the device's current size.
    pub size: Option<(u32, u32)>,
    /// Requested pixel format, or `None` for the backend's default.
    pub format: Option<PixelFormat>,
    /// Requested frame rate, or `None` to keep the device's current rate.
    pub frame_rate: Option<f64>,
    /// Requested pool depth.
    pub buffers: usize,
    /// Buffer allocation strategy.
    pub memory: MemoryStrategy,
    /// Contiguity required of SDK-allocated buffers.
    pub contiguity: Contiguity,
    /// Requested mirroring, if any.
    pub mirror: Option<Mirror>,
}

impl Default for StreamRequest {
    fn default() -> Self {
        Self {
            size: None,
            format: None,
            frame_rate: None,
            buffers: 4,
            memory: MemoryStrategy::Auto,
            contiguity: Contiguity::Required,
            mirror: None,
        }
    }
}

/// What the backend negotiated. Backends may adjust a request; consumers
/// read this rather than assume their request was applied.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct StreamConfig {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Pixel format of every frame.
    pub format: PixelFormat,
    /// Byte pitch of plane 0 as the driver returned it.
    pub row_stride: usize,
    /// Number of image planes.
    pub planes: usize,
    /// Negotiated frame rate, or `None` when the backend cannot report it.
    pub frame_rate: Option<f64>,
    /// Colorimetry, or `None` when the driver reports it as unspecified.
    pub colorimetry: Option<Colorimetry>,
    /// Number of capture buffers.
    pub buffer_count: usize,
    /// Memory strategy in effect.
    pub memory: ResolvedMemory,
    /// `Some(true)` for CMA memory, `Some(false)` for system-heap memory,
    /// `None` when unknown (Export buffers, imported fds).
    pub contiguous: Option<bool>,
}

impl StreamConfig {
    /// Creates a configuration with one plane at the natural pitch; set the
    /// remaining fields directly.
    pub fn new(width: u32, height: u32, format: PixelFormat, row_stride: usize) -> Self {
        Self {
            width,
            height,
            format,
            row_stride,
            planes: 1,
            frame_rate: None,
            colorimetry: None,
            buffer_count: 0,
            memory: ResolvedMemory::Import,
            contiguous: None,
        }
    }
}

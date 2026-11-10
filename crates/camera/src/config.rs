// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Stream requests and the configuration a backend actually negotiated.

use std::ops::RangeInclusive;

use edgefirst_tensor::{Colorimetry, PixelFormat, TensorDyn};

#[cfg(any(
    feature = "v4l2",
    feature = "libcamera",
    feature = "mock",
    feature = "file",
    all(windows, feature = "media-foundation")
))]
use crate::{Error, ErrorKind, Result};

/// The capture pool depths every backend accepts.
pub const POOL_DEPTH: RangeInclusive<usize> = 2..=32;

/// Refuses a pool depth outside [`POOL_DEPTH`].
#[cfg(any(
    feature = "v4l2",
    feature = "libcamera",
    feature = "mock",
    feature = "file",
    all(windows, feature = "media-foundation")
))]
pub(crate) fn check_pool_depth(buffers: usize) -> Result<()> {
    if POOL_DEPTH.contains(&buffers) {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::InvalidConfig,
        format!(
            "pool depth {buffers} outside {}..={}",
            POOL_DEPTH.start(),
            POOL_DEPTH.end()
        ),
    ))
}

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

impl Mirror {
    /// Every mirror, in order of [`Mirror::bits`].
    pub const ALL: [Mirror; 4] = [Self::None, Self::Horizontal, Self::Vertical, Self::Both];

    /// The mirror that flips left to right when `horizontal` and top to
    /// bottom when `vertical`.
    pub fn from_flips(horizontal: bool, vertical: bool) -> Self {
        Self::from_bits(u8::from(horizontal) | (u8::from(vertical) << 1))
    }

    /// The value of [`crate::ControlId::Mirror`]: bit 0 flips left to
    /// right, bit 1 top to bottom.
    pub fn bits(self) -> u8 {
        self as u8
    }

    /// The mirror of a [`Mirror::bits`] value; higher bits are ignored.
    pub fn from_bits(bits: u8) -> Self {
        Self::ALL[usize::from(bits & 3)]
    }

    /// Whether it flips left to right.
    pub fn horizontal(self) -> bool {
        self.bits() & 1 != 0
    }

    /// Whether it flips top to bottom.
    pub fn vertical(self) -> bool {
        self.bits() & 2 != 0
    }
}

/// Physical contiguity required of SDK-allocated buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Contiguity {
    /// Allocate from CMA only; fail with
    /// [`ErrorKind::ContiguousUnavailable`](crate::ErrorKind::ContiguousUnavailable)
    /// rather than fall back to non-contiguous memory. The exception is
    /// [`MemoryStrategy::Auto`] on a machine with no CMA heap at all, such as
    /// a desktop, where the SDK's own pool comes from the system DMA-BUF heap,
    /// or from the driver when no heap can be opened. A CMA heap that is full
    /// or not accessible is still an error.
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
    /// The rate the camera reports, or `None` when it reports none. With
    /// libcamera it follows the frame duration of the latest frame. It can
    /// differ from what the camera delivers, which is
    /// [`Camera::measured_frame_rate`](crate::Camera::measured_frame_rate).
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

/// Relative difference between two frame rates that is worth reporting.
const RATE_TOLERANCE: f64 = 0.01;

/// Whether frame rate `a` differs from `b` by more than measurement noise.
pub fn rates_differ(a: f64, b: f64) -> bool {
    (a - b).abs() > b * RATE_TOLERANCE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_within_the_tolerance_do_not_differ() {
        assert!(!rates_differ(30.0, 30.2));
        assert!(rates_differ(30.0, 31.0));
    }
}

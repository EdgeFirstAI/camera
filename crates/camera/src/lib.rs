// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! # EdgeFirst Camera SDK
//!
//! Portable camera capture into [`edgefirst_tensor`] zero-copy buffers.
//!
//! A [`Camera`] delivers [`Frame`]s: tensors plus capture metadata. Frames
//! from Linux cameras are DMA-BUF tensors, ready for `edgefirst-image`
//! conversion and for publishing as `edgefirst-schemas` `CameraFrame`
//! messages.
//!
//! - **Requests, not commands.** Size, format, frame rate and controls are
//!   requests. [`Camera::config`] and [`Applied`] report what was actually
//!   configured, and the SDK never alters the stream to enforce a request.
//! - **Safe buffer lifetime.** A buffer is never handed back to the driver
//!   while a [`Frame`] references it. Frames keep their memory through
//!   `stop()` and through closing the camera.
//! - **Acquisition time.** Each frame carries its capture instant in the
//!   driver's clock and its wall-clock acquisition time, converted once at
//!   dequeue with [`RealtimeClock`].
//! - **Bindable.** The API has no generics or lifetimes, so
//!   `Box<dyn Camera>` can be wrapped by other languages, and [`ErrorKind`]
//!   is a stable error category.
//!
//! ## Example
//!
//! ```
//! # #[cfg(feature = "mock")]
//! # fn main() -> edgefirst_camera::Result<()> {
//! use edgefirst_camera::CameraBuilder;
//! use std::time::Duration;
//!
//! let mut camera = CameraBuilder::source("mock:1280x720@30")?.open()?;
//! camera.start()?;
//! let frame = camera.next_frame(Some(Duration::from_secs(1)))?;
//! println!(
//!     "frame {} {}x{} {:?} acquired at {:?}",
//!     frame.seq(),
//!     camera.config().width,
//!     camera.config().height,
//!     camera.config().format,
//!     frame.realtime(),
//! );
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "mock"))]
//! # fn main() {}
//! ```
//!
//! ## Features
//!
//! - `v4l2` (default): the V4L2 capture backend on Linux (`/dev/videoN`).
//! - `libcamera` (default): the libcamera backend on Linux
//!   (`libcamera:<id>`), loaded at run time through the
//!   `edgefirst-libcamera-sys` shim, so binaries run without libcamera.
//! - `mock`: the synthetic frame source (`mock[:WxH@fps]`).
//! - `file`: recorded H.264 playback (`file:<name>.h264` with its
//!   `<name>.json` sidecar), decoded by the platform's V4L2 decoder.
//! - `schemas`: the `schema` module, the `Frame` →
//!   `edgefirst_msgs/CameraFrame` mapping.

mod backend;
mod builder;
mod camera;
mod config;
mod control;
mod enumerate;
mod error;
mod frame;
mod pool;
#[cfg(feature = "schemas")]
pub mod schema;
#[cfg(feature = "file")]
mod sidecar;
mod timestamp;

pub use builder::CameraBuilder;
pub use camera::{Camera, CaptureStats, WaitHandle};
pub use config::{
    BufferPool, Contiguity, MemoryStrategy, Mirror, ResolvedMemory, StreamConfig, StreamRequest,
    POOL_DEPTH,
};
pub use control::{
    Applied, Control, ControlFlags, ControlId, ControlInfo, ControlSet, ControlValue, Exposure,
    Gain, Unsupported, WhiteBalance,
};
pub use enumerate::{
    modes, named_size, size_names, Backend, CameraDescriptor, FormatInfo, Mode, Rates, SizeRates,
    Sizes,
};
pub use error::{Error, ErrorKind, Rejection, Result};
pub use frame::{Frame, FrameMeta, PlaneLayout, SlotRelease};
pub use pool::SlotTable;
#[cfg(feature = "file")]
pub use sidecar::Sidecar;
pub use timestamp::{CaptureClock, RealtimeClock, Timestamp, TimestampSource};

/// Lists the cameras every compiled backend can see.
///
/// Formats and sizes are filled in; rates only where the backend reports
/// them cheaply. Use [`probe`] for the full size and rate table.
///
/// # Errors
///
/// A backend failure while scanning devices.
pub fn enumerate() -> Result<Vec<CameraDescriptor>> {
    backend::enumerate()
}

/// Restricts the libcamera pipeline handlers, as the
/// `LIBCAMERA_PIPELINES_MATCH_LIST` environment variable does; `None` leaves
/// libcamera's default. On the i.MX 95, `Some("nxp/neo")` makes the ISP
/// pipeline claim the camera instead of the raw-only `imx8-isi` one. The
/// SDK never switches pipelines on its own.
///
/// Applies when libcamera's `CameraManager` starts: call it before the
/// first libcamera [`enumerate`] or open. It sets the environment variable,
/// or with `None` removes it.
///
/// # Errors
///
/// [`ErrorKind::InvalidConfig`] when a libcamera camera is open with a
/// different list.
///
/// # Safety
///
/// It changes the process environment, which is undefined behaviour on
/// Unix while another thread reads or writes it (libraries read it
/// unannounced). Call it before the process starts other threads, as at
/// the top of `main`, or set the variable outside the process instead.
#[cfg(all(target_os = "linux", feature = "libcamera"))]
pub unsafe fn set_libcamera_pipelines(list: Option<&str>) -> Result<()> {
    // SAFETY: the caller upholds the contract above.
    unsafe { backend::libcamera::set_match_list(list) }
}

/// Probes a camera's full size and rate table.
///
/// Some backends must configure each mode to read its rate (libcamera), so
/// this can take a noticeable time; call it on request, not per frame.
///
/// # Errors
///
/// [`ErrorKind::NotFound`] when the camera's backend is not compiled in, or
/// a backend failure while probing.
pub fn probe(descriptor: &CameraDescriptor) -> Result<Vec<FormatInfo>> {
    backend::probe(descriptor)
}

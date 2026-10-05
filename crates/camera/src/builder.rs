// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Opening cameras.

use std::path::PathBuf;

use edgefirst_tensor::{PixelFormat, TensorDyn};

use crate::{
    backend, Backend, Camera, CameraDescriptor, Contiguity, Control, Error, ErrorKind,
    MemoryStrategy, Mirror, Result, StreamRequest,
};

/// Where frames come from, parsed from a source string.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Source {
    /// A V4L2 device path.
    Device(PathBuf),
    /// A libcamera camera id.
    Libcamera(String),
    /// A recorded file.
    File(PathBuf),
    /// Synthetic frames with a default size and rate.
    Mock { width: u32, height: u32, fps: f64 },
}

impl Source {
    pub(crate) fn parse(spec: &str) -> Result<Self> {
        let invalid =
            |why: &str| Error::new(ErrorKind::InvalidConfig, format!("source '{spec}': {why}"));
        if let Some(id) = spec.strip_prefix("libcamera:") {
            if id.is_empty() {
                return Err(invalid("missing camera id"));
            }
            return Ok(Self::Libcamera(id.to_owned()));
        }
        if let Some(path) = spec.strip_prefix("file:") {
            if path.is_empty() {
                return Err(invalid("missing file path"));
            }
            return Ok(Self::File(PathBuf::from(path)));
        }
        if spec == "mock" {
            return Ok(Self::Mock {
                width: 1280,
                height: 720,
                fps: 30.0,
            });
        }
        if let Some(params) = spec.strip_prefix("mock:") {
            let (size, fps) = params.split_once('@').unwrap_or((params, "30"));
            let (w, h) = size
                .split_once('x')
                .ok_or_else(|| invalid("expected mock:WxH@fps"))?;
            let parse_u32 = |v: &str| {
                v.parse::<u32>()
                    .map_err(|_| invalid("expected mock:WxH@fps"))
            };
            let fps: f64 = fps.parse().map_err(|_| invalid("expected mock:WxH@fps"))?;
            if !(fps.is_finite() && fps > 0.0) {
                return Err(invalid("frame rate must be positive"));
            }
            return Ok(Self::Mock {
                width: parse_u32(w)?,
                height: parse_u32(h)?,
                fps,
            });
        }
        if spec.starts_with('/') {
            return Ok(Self::Device(PathBuf::from(spec)));
        }
        Err(invalid(
            "expected a device path, libcamera:<id>, file:<path> or mock[:WxH@fps]",
        ))
    }
}

/// Configures and opens a camera.
///
/// Every setting is a request; read [`Camera::config`] after opening for
/// what was configured.
///
/// # Examples
///
/// ```
/// # #[cfg(feature = "mock")]
/// # fn main() -> edgefirst_camera::Result<()> {
/// use edgefirst_camera::CameraBuilder;
/// use std::time::Duration;
///
/// let mut camera = CameraBuilder::source("mock:640x480@30")?.buffers(3).open()?;
/// camera.start()?;
/// let frame = camera.next_frame(Some(Duration::from_secs(1)))?;
/// assert_eq!(frame.width(), Some(640));
/// # Ok(())
/// # }
/// # #[cfg(not(feature = "mock"))]
/// # fn main() {}
/// ```
#[derive(Debug)]
pub struct CameraBuilder {
    pub(crate) source: Source,
    pub(crate) request: StreamRequest,
    pub(crate) provided: Option<Vec<TensorDyn>>,
    pub(crate) exclusive: bool,
    pub(crate) controls: Vec<Control>,
}

impl CameraBuilder {
    /// Starts from a source string: a device path (`/dev/video3`),
    /// `libcamera:<id>`, `file:<path>` or `mock[:WxH@fps]`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfig`] when the string matches none of these.
    pub fn source(spec: &str) -> Result<Self> {
        Ok(Self::with_source(Source::parse(spec)?))
    }

    /// Starts from a descriptor returned by [`enumerate`](crate::enumerate).
    pub fn from_descriptor(d: &CameraDescriptor) -> Self {
        let source = match d.backend {
            Backend::Libcamera => Source::Libcamera(d.id.clone()),
            Backend::File => Source::File(PathBuf::from(&d.id)),
            Backend::Mock => Source::parse(&d.id).unwrap_or(Source::Mock {
                width: 1280,
                height: 720,
                fps: 30.0,
            }),
            _ => Source::Device(PathBuf::from(&d.id)),
        };
        Self::with_source(source)
    }

    fn with_source(source: Source) -> Self {
        Self {
            source,
            request: StreamRequest::default(),
            provided: None,
            exclusive: true,
            controls: Vec::new(),
        }
    }

    /// Requests a capture size; `config()` reports what was applied.
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.request.size = Some((width, height));
        self
    }

    /// Requests a pixel format.
    pub fn format(mut self, format: PixelFormat) -> Self {
        self.request.format = Some(format);
        self
    }

    /// Requests a frame rate. A request only: the SDK never drops frames to
    /// enforce it.
    pub fn frame_rate(mut self, fps: f64) -> Self {
        self.request.frame_rate = Some(fps);
        self
    }

    /// Requests a pool depth (default 4).
    pub fn buffers(mut self, count: usize) -> Self {
        self.request.buffers = count;
        self
    }

    /// Selects the buffer strategy (default [`MemoryStrategy::Auto`]).
    pub fn memory(mut self, memory: MemoryStrategy) -> Self {
        self.request.memory = memory;
        self
    }

    /// Captures into caller-provided tensors. A refusal is
    /// [`ErrorKind::BuffersRejected`] at `start()`; they are never replaced
    /// silently.
    pub fn with_buffers(mut self, pool: Vec<TensorDyn>) -> Self {
        self.provided = Some(pool);
        self
    }

    /// Sets the contiguity requirement for SDK-allocated buffers (default
    /// [`Contiguity::Required`]).
    pub fn contiguity(mut self, contiguity: Contiguity) -> Self {
        self.request.contiguity = contiguity;
        self
    }

    /// Requests mirroring.
    pub fn mirror(mut self, mirror: Mirror) -> Self {
        self.request.mirror = Some(mirror);
        self
    }

    /// Takes an advisory exclusive lock on the device (default `true`).
    pub fn exclusive(mut self, exclusive: bool) -> Self {
        self.exclusive = exclusive;
        self
    }

    /// Adds a control applied at start; an unsupported control is logged,
    /// not fatal.
    pub fn control(mut self, control: Control) -> Self {
        self.controls.push(control);
        self
    }

    /// Opens the camera.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotFound`] when the source does not exist or its
    /// backend is not compiled in, [`ErrorKind::InvalidConfig`] for an
    /// invalid request, [`ErrorKind::Busy`] when another user holds the
    /// device.
    pub fn open(self) -> Result<Box<dyn Camera>> {
        backend::open(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_source_form() {
        assert_eq!(
            Source::parse("/dev/video3").unwrap(),
            Source::Device(PathBuf::from("/dev/video3"))
        );
        assert_eq!(
            Source::parse("libcamera:/base/soc/os08a20@36").unwrap(),
            Source::Libcamera("/base/soc/os08a20@36".into())
        );
        assert_eq!(
            Source::parse("file:capture.h264").unwrap(),
            Source::File(PathBuf::from("capture.h264"))
        );
        assert_eq!(
            Source::parse("mock:640x480@60").unwrap(),
            Source::Mock {
                width: 640,
                height: 480,
                fps: 60.0
            }
        );
        assert_eq!(
            Source::parse("mock:320x240").unwrap(),
            Source::Mock {
                width: 320,
                height: 240,
                fps: 30.0
            }
        );
    }

    #[test]
    fn rejects_malformed_sources() {
        for bad in [
            "video3",
            "libcamera:",
            "file:",
            "mock:640",
            "mock:axb@30",
            "mock:640x480@0",
        ] {
            assert_eq!(
                Source::parse(bad).unwrap_err().kind(),
                ErrorKind::InvalidConfig,
                "{bad}"
            );
        }
    }
}

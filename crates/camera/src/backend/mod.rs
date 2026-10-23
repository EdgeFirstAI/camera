// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Backend dispatch.

#[cfg(feature = "file")]
mod file;
#[cfg(all(target_os = "linux", feature = "libcamera"))]
pub(crate) mod libcamera;
#[cfg(feature = "mock")]
pub(crate) mod mock;
#[cfg(all(target_os = "linux", feature = "v4l2"))]
pub(crate) mod v4l2;

use crate::builder::Source;
use crate::{Camera, CameraBuilder, CameraDescriptor, Error, ErrorKind, FormatInfo, Result};

fn not_compiled(what: &str) -> Error {
    Error::new(
        ErrorKind::NotFound,
        format!("the {what} backend is not available in this build"),
    )
}

pub(crate) fn open(builder: CameraBuilder) -> Result<Box<dyn Camera>> {
    match &builder.source {
        #[cfg(feature = "mock")]
        Source::Mock { .. } => mock::open(builder),
        #[cfg(not(feature = "mock"))]
        Source::Mock { .. } => Err(not_compiled("mock")),
        #[cfg(all(target_os = "linux", feature = "v4l2"))]
        Source::Device(_) => v4l2::open(builder),
        #[cfg(not(all(target_os = "linux", feature = "v4l2")))]
        Source::Device(_) => Err(not_compiled("V4L2")),
        #[cfg(all(target_os = "linux", feature = "libcamera"))]
        Source::Libcamera(_) => libcamera::open(builder),
        #[cfg(not(all(target_os = "linux", feature = "libcamera")))]
        Source::Libcamera(_) => Err(not_compiled("libcamera")),
        #[cfg(feature = "file")]
        Source::File(_) => file::open(builder),
        #[cfg(not(feature = "file"))]
        Source::File(_) => Err(not_compiled("file")),
    }
}

pub(crate) fn enumerate() -> Result<Vec<CameraDescriptor>> {
    #[allow(unused_mut)]
    let mut cameras: Vec<CameraDescriptor> = Vec::new();
    #[cfg(all(target_os = "linux", feature = "v4l2"))]
    cameras.extend(v4l2::enumerate()?);
    #[cfg(all(target_os = "linux", feature = "libcamera"))]
    cameras.extend(libcamera::enumerate()?);
    #[cfg(feature = "mock")]
    cameras.extend([mock::descriptor("mock")]);
    Ok(cameras)
}

pub(crate) fn probe(d: &CameraDescriptor) -> Result<Vec<FormatInfo>> {
    match d.backend {
        #[cfg(feature = "mock")]
        crate::Backend::Mock => Ok(d.formats.clone()),
        #[cfg(feature = "file")]
        crate::Backend::File => Ok(d.formats.clone()),
        #[cfg(all(target_os = "linux", feature = "v4l2"))]
        crate::Backend::V4l2 => v4l2::probe(d),
        #[cfg(all(target_os = "linux", feature = "libcamera"))]
        crate::Backend::Libcamera => libcamera::probe(d),
        #[allow(
            unreachable_patterns,
            reason = "reachable unless every backend feature is enabled"
        )]
        _ => Err(not_compiled(&format!("{:?}", d.backend))),
    }
}

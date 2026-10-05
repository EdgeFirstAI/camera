// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Backend dispatch.

#[cfg(feature = "mock")]
pub(crate) mod mock;

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
        Source::Device(_) => Err(not_compiled("V4L2")),
        Source::Libcamera(_) => Err(not_compiled("libcamera")),
        Source::File(_) => Err(not_compiled("file")),
    }
}

pub(crate) fn enumerate() -> Result<Vec<CameraDescriptor>> {
    Ok(vec![
        #[cfg(feature = "mock")]
        mock::descriptor("mock"),
    ])
}

pub(crate) fn probe(d: &CameraDescriptor) -> Result<Vec<FormatInfo>> {
    match d.backend {
        #[cfg(feature = "mock")]
        crate::Backend::Mock => Ok(d.formats.clone()),
        _ => Err(not_compiled(&format!("{:?}", d.backend))),
    }
}

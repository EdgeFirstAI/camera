// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The camera the board tests run against.

use std::path::Path;

use edgefirst_camera::Backend;

/// The source spec from `EDGEFIRST_CAMERA_SOURCE`: a V4L2 node (default
/// `/dev/video3`), `libcamera:<id>`, or `libcamera` for the first camera
/// libcamera lists. `None`, after printing `SKIPPED`, when it is absent.
pub fn source() -> Option<String> {
    let spec = std::env::var("EDGEFIRST_CAMERA_SOURCE").unwrap_or_else(|_| "/dev/video3".into());
    let found = if spec == "libcamera" || spec.starts_with("libcamera:") {
        edgefirst_camera::enumerate()
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.backend == Backend::Libcamera)
            .map(|c| format!("libcamera:{}", c.id))
            .find(|s| spec == "libcamera" || *s == spec)
    } else {
        Path::new(&spec).exists().then(|| spec.clone())
    };
    if found.is_none() {
        eprintln!("SKIPPED: no camera at {spec}; set EDGEFIRST_CAMERA_SOURCE");
    }
    found
}

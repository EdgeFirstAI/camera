// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Sidecar metadata written alongside a recorded H.264 file.
//!
//! A recording is a `.h264` file (raw Annex-B bitstream) plus a `.json`
//! file of the same basename. The `.json` carries every piece of
//! producer-global state that cannot be recovered from the bitstream
//! itself: the camera-level colorimetry tags, the `sensor_msgs/CameraInfo`
//! fields, and the `geometry_msgs/TransformStamped` for `/tf_static`.
//!
//! The sidecar is written once at record startup (everything here is
//! stable for the session) and read once at replay startup. There is no
//! per-frame metadata — that stays entirely in the Annex-B bitstream.
//!
//! See `ARCHITECTURE.md` for the full file format and the design
//! rationale behind keeping the sidecar minimal.
//!
//! # File format (version 1)
//!
//! ```jsonc
//! {
//!   "version":     1,
//!   "codec":       "h264",
//!   "fps":         30,
//!   "width":       1920,
//!   "height":      1080,
//!   "colorimetry": { "color_space": "bt709", ... },
//!   "camera_info": { ... CameraInfoFields ... },
//!   "tf_static":   { ... TfStaticFields ... }
//! }
//! ```
//!
//! `fps` / `width` / `height` describe the encoded bitstream and are what
//! the replay path uses to initialize its decoder and pacing. The schema
//! deliberately omits a `format` field because the decoder always
//! produces NV12 regardless of the source fourcc, so the source format is
//! not observable on replay.

use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};
use videostream::camera::CameraReader;

use crate::{CameraInfoFields, Colorimetry, TfStaticFields};

/// Current sidecar format version. Bump on any incompatible change.
pub(crate) const SIDECAR_VERSION: u32 = 1;

/// Recorded codec identifier. Only `h264` is supported in v1.
pub(crate) const SIDECAR_CODEC_H264: &str = "h264";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Sidecar {
    pub version: u32,
    pub codec: String,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    pub colorimetry: Colorimetry,
    pub camera_info: CameraInfoFields,
    pub tf_static: TfStaticFields,
}

impl Sidecar {
    /// Build a sidecar from live capture state at record-start time.
    ///
    /// All fields are resolved once here and immutable for the session —
    /// there is nothing per-frame to track.
    pub(crate) fn from_live(
        fps: u32,
        cam: &CameraReader,
        camera_info: CameraInfoFields,
        tf_static: TfStaticFields,
    ) -> Self {
        Sidecar {
            version: SIDECAR_VERSION,
            codec: SIDECAR_CODEC_H264.to_string(),
            fps,
            width: cam.width() as u32,
            height: cam.height() as u32,
            colorimetry: Colorimetry::from_camera(cam),
            camera_info,
            tf_static,
        }
    }

    /// Serialize to `<h264_path>.json`. Plain `fs::write`; no temp-file
    /// rename dance. The sidecar is written once at startup so a torn
    /// write is a launch-time failure the operator sees immediately.
    pub(crate) fn write_paired(&self, h264_path: &Path) -> Result<PathBuf, Box<dyn Error>> {
        let path = Self::paired_path(h264_path);
        let bytes = serde_json::to_vec_pretty(self)?;
        fs::write(&path, &bytes)?;
        Ok(path)
    }

    /// Load and validate a sidecar paired with the given `.h264` path.
    pub(crate) fn load_paired(h264_path: &Path) -> Result<Self, Box<dyn Error>> {
        let path = Self::paired_path(h264_path);
        let bytes = fs::read(&path).map_err(|e| {
            format!(
                "Cannot open sidecar {:?} (expected alongside the .h264 file): {e}",
                path
            )
        })?;
        let sidecar: Sidecar = serde_json::from_slice(&bytes)
            .map_err(|e| format!("Failed to parse sidecar {:?}: {e}", path))?;
        if sidecar.version != SIDECAR_VERSION {
            return Err(format!(
                "Sidecar {:?} is version {}, but this build only reads version {}",
                path, sidecar.version, SIDECAR_VERSION
            )
            .into());
        }
        if sidecar.codec != SIDECAR_CODEC_H264 {
            return Err(format!(
                "Sidecar {:?} codec {:?} is not supported (only {:?} in this release)",
                path, sidecar.codec, SIDECAR_CODEC_H264
            )
            .into());
        }
        Ok(sidecar)
    }

    /// Derive the sidecar path from an `.h264` path by swapping the
    /// extension to `.json`. A path like `capture.h264` pairs with
    /// `capture.json`; `recordings/run01.h264` pairs with
    /// `recordings/run01.json`. Always derived — no user override.
    pub(crate) fn paired_path(h264_path: &Path) -> PathBuf {
        h264_path.with_extension("json")
    }
}

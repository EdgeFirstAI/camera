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
    ///
    /// `encoded_width` / `encoded_height` must be the H.264 bitstream
    /// dimensions (what the encoder is configured to emit), not the
    /// camera capture dimensions. The encoder may resize the camera
    /// frame via G2D before encoding (when `--stream-size` differs
    /// from `--camera-size`), and the replay decoder's output will
    /// match the *encoded* size, not the capture size.
    pub(crate) fn from_live(
        fps: u32,
        encoded_width: u32,
        encoded_height: u32,
        cam: &CameraReader,
        camera_info: CameraInfoFields,
        tf_static: TfStaticFields,
    ) -> Self {
        Sidecar {
            version: SIDECAR_VERSION,
            codec: SIDECAR_CODEC_H264.to_string(),
            fps,
            width: encoded_width,
            height: encoded_height,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CameraInfoFields, RoiFields, TfStaticFields};

    fn sample_sidecar() -> Sidecar {
        Sidecar {
            version: SIDECAR_VERSION,
            codec: SIDECAR_CODEC_H264.to_string(),
            fps: 30,
            width: 1920,
            height: 1080,
            colorimetry: Colorimetry {
                space: "bt709".into(),
                transfer: "bt709".into(),
                encoding: "bt601".into(),
                range: "limited".into(),
            },
            camera_info: CameraInfoFields {
                frame_id: "camera".into(),
                width: 1920,
                height: 1080,
                distortion_model: "plumb_bob".into(),
                d: vec![0.0; 5],
                k: [1270.0, 0.0, 960.0, 0.0, 1270.0, 540.0, 0.0, 0.0, 1.0],
                r: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
                p: [
                    1270.0, 0.0, 960.0, 0.0, 0.0, 1270.0, 540.0, 0.0, 0.0, 0.0, 1.0, 0.0,
                ],
                binning_x: 1,
                binning_y: 1,
                roi: RoiFields {
                    x_offset: 0,
                    y_offset: 0,
                    height: 1080,
                    width: 1920,
                    do_rectify: false,
                },
            },
            tf_static: TfStaticFields {
                base_frame_id: "base_link".into(),
                child_frame_id: "camera".into(),
                translation: [0.0, 0.1, 0.2],
                rotation: [0.0, 0.0, 0.0, 1.0],
            },
        }
    }

    #[test]
    fn paired_path_swaps_extension() {
        assert_eq!(
            Sidecar::paired_path(Path::new("capture.h264")),
            PathBuf::from("capture.json")
        );
        assert_eq!(
            Sidecar::paired_path(Path::new("recordings/run01.h264")),
            PathBuf::from("recordings/run01.json")
        );
        // File without extension gains one.
        assert_eq!(
            Sidecar::paired_path(Path::new("capture")),
            PathBuf::from("capture.json")
        );
    }

    #[test]
    fn roundtrip_preserves_every_field() {
        let src = sample_sidecar();
        let bytes = serde_json::to_vec_pretty(&src).unwrap();
        let back: Sidecar = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(back.version, src.version);
        assert_eq!(back.codec, src.codec);
        assert_eq!(back.fps, src.fps);
        assert_eq!(back.width, src.width);
        assert_eq!(back.height, src.height);
        assert_eq!(back.colorimetry.space, src.colorimetry.space);
        assert_eq!(back.colorimetry.transfer, src.colorimetry.transfer);
        assert_eq!(back.colorimetry.encoding, src.colorimetry.encoding);
        assert_eq!(back.colorimetry.range, src.colorimetry.range);
        assert_eq!(back.camera_info.k, src.camera_info.k);
        assert_eq!(back.camera_info.p, src.camera_info.p);
        assert_eq!(back.camera_info.roi.width, src.camera_info.roi.width);
        assert_eq!(back.tf_static.base_frame_id, src.tf_static.base_frame_id);
        assert_eq!(back.tf_static.translation, src.tf_static.translation);
        assert_eq!(back.tf_static.rotation, src.tf_static.rotation);
    }

    #[test]
    fn colorimetry_serializes_with_schema_field_names() {
        let bytes = serde_json::to_vec(&sample_sidecar()).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        // Field names on the wire must match CameraFrame.msg vocabulary.
        assert!(s.contains("\"color_space\""));
        assert!(s.contains("\"color_transfer\""));
        assert!(s.contains("\"color_encoding\""));
        assert!(s.contains("\"color_range\""));
    }

    #[test]
    fn write_and_load_paired_round_trip() {
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let h264 = tmp.join(format!("edgefirst_camera_sidecar_test_{pid}.h264"));
        // Clean up from any earlier run.
        let _ = std::fs::remove_file(Sidecar::paired_path(&h264));

        let src = sample_sidecar();
        let json_path = src.write_paired(&h264).unwrap();
        assert_eq!(json_path, h264.with_extension("json"));
        assert!(json_path.exists());

        let back = Sidecar::load_paired(&h264).unwrap();
        assert_eq!(back.version, src.version);
        assert_eq!(back.fps, src.fps);
        assert_eq!(back.width, src.width);

        std::fs::remove_file(&json_path).ok();
    }

    #[test]
    fn load_paired_rejects_unknown_version() {
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let h264 = tmp.join(format!("edgefirst_camera_sidecar_version_test_{pid}.h264"));
        let json = Sidecar::paired_path(&h264);

        // Same shape as v1 but with version bumped into the future.
        let mut src = sample_sidecar();
        src.version = 999;
        std::fs::write(&json, serde_json::to_vec_pretty(&src).unwrap()).unwrap();

        let err = Sidecar::load_paired(&h264).unwrap_err();
        assert!(
            err.to_string().contains("version"),
            "expected version rejection, got: {err}"
        );

        std::fs::remove_file(&json).ok();
    }

    #[test]
    fn load_paired_rejects_unknown_codec() {
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let h264 = tmp.join(format!("edgefirst_camera_sidecar_codec_test_{pid}.h264"));
        let json = Sidecar::paired_path(&h264);

        let mut src = sample_sidecar();
        src.codec = "h265".into();
        std::fs::write(&json, serde_json::to_vec_pretty(&src).unwrap()).unwrap();

        let err = Sidecar::load_paired(&h264).unwrap_err();
        assert!(
            err.to_string().contains("codec"),
            "expected codec rejection, got: {err}"
        );

        std::fs::remove_file(&json).ok();
    }

    #[test]
    fn load_paired_missing_file_is_clear_error() {
        let err = Sidecar::load_paired(Path::new("/definitely/does/not/exist.h264"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("sidecar"),
            "expected sidecar-context error: {err}"
        );
    }
}

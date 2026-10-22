// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The JSON sidecar of a recording, version 1.
//!
//! A recording is `<name>.h264`, a raw Annex B stream, and `<name>.json`:
//!
//! ```jsonc
//! {
//!   "version": 1,
//!   "codec": "h264",
//!   "fps": 30,
//!   "width": 1920,
//!   "height": 1080,
//!   "colorimetry": { "color_space": "bt709", "color_transfer": "bt709",
//!                    "color_encoding": "bt601", "color_range": "limited" },
//!   "camera_info": { ... },
//!   "tf_static": { ... }
//! }
//! ```
//!
//! `fps`, `width` and `height` describe the encoded stream. Colorimetry
//! labels are the tensor crate's (an empty string is unspecified). Every
//! other field is the application's, and is kept verbatim as `extras`.

use std::path::{Path, PathBuf};

use edgefirst_tensor::{ColorEncoding, ColorRange, ColorSpace, ColorTransfer, Colorimetry};
use serde_json::{Map, Value};

use crate::{Error, ErrorKind, Result};

const VERSION: u64 = 1;
const CODEC: &str = "h264";
/// Fields that describe the stream; the rest are extras.
const STREAM_FIELDS: [&str; 6] = ["version", "codec", "fps", "width", "height", "colorimetry"];

/// The parts of a sidecar the source uses.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sidecar {
    pub(crate) fps: f64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) colorimetry: Option<Colorimetry>,
    /// The other fields, as a JSON object.
    pub(crate) extras: String,
}

/// The sidecar of `stream`: the same path with a `.json` extension.
pub(crate) fn paired_path(stream: &Path) -> PathBuf {
    stream.with_extension("json")
}

impl Sidecar {
    /// Reads the sidecar paired with `stream`.
    pub(crate) fn load(stream: &Path) -> Result<Self> {
        let path = paired_path(stream);
        let bytes = std::fs::read(&path).map_err(|e| {
            let kind = if e.kind() == std::io::ErrorKind::NotFound {
                ErrorKind::NotFound
            } else {
                ErrorKind::Io
            };
            Error::new(kind, format!("sidecar {}: {e}", path.display())).with_source(e)
        })?;
        Self::parse(&bytes)
            .map_err(|e| Error::new(e.kind(), format!("sidecar {}: {e}", path.display())))
    }

    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        let invalid = |why: String| Error::new(ErrorKind::InvalidConfig, why);
        let value: Value = serde_json::from_slice(bytes).map_err(|e| invalid(e.to_string()))?;
        let Value::Object(object) = value else {
            return Err(invalid("not a JSON object".into()));
        };
        let version = object.get("version").and_then(Value::as_u64);
        if version != Some(VERSION) {
            return Err(invalid(format!(
                "version {} is not supported (this build reads version {VERSION})",
                object.get("version").unwrap_or(&Value::Null)
            )));
        }
        match object.get("codec").and_then(Value::as_str) {
            Some(CODEC) => {}
            other => {
                return Err(Error::new(
                    ErrorKind::UnsupportedFormat,
                    format!("codec {other:?} is not supported (only {CODEC:?})"),
                ))
            }
        }
        let fps = object
            .get("fps")
            .and_then(Value::as_f64)
            .filter(|f| f.is_finite() && *f > 0.0)
            .ok_or_else(|| invalid("fps must be a positive number".into()))?;
        let dimension = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v > 0)
                .ok_or_else(|| invalid(format!("{name} must be a positive integer")))
        };
        let (width, height) = (dimension("width")?, dimension("height")?);
        let colorimetry = object.get("colorimetry").and_then(colorimetry);
        let extras: Map<String, Value> = object
            .into_iter()
            .filter(|(k, _)| !STREAM_FIELDS.contains(&k.as_str()))
            .collect();
        Ok(Self {
            fps,
            width,
            height,
            colorimetry,
            extras: Value::Object(extras).to_string(),
        })
    }
}

/// The colorimetry of a sidecar's `colorimetry` object; `None` when every
/// axis is unspecified. An unknown label is unspecified, with a warning.
fn colorimetry(value: &Value) -> Option<Colorimetry> {
    fn axis<T>(value: &Value, name: &str, parse: fn(&str) -> Option<T>) -> Option<T> {
        let label = value.get(name)?.as_str()?;
        if label.is_empty() {
            return None;
        }
        let parsed = parse(label);
        if parsed.is_none() {
            tracing::warn!(
                "sidecar {name} {label:?} is not a known label; treating it as unspecified"
            );
        }
        parsed
    }
    let c = Colorimetry {
        space: axis(value, "color_space", ColorSpace::from_str_code),
        transfer: axis(value, "color_transfer", ColorTransfer::from_str_code),
        encoding: axis(value, "color_encoding", ColorEncoding::from_str_code),
        range: axis(value, "color_range", ColorRange::from_str_code),
    };
    (c != Colorimetry::default()).then_some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sidecar as camera 2.x writes it.
    const RECORDED: &str = r#"{
      "version": 1,
      "codec": "h264",
      "fps": 30,
      "width": 1920,
      "height": 1080,
      "colorimetry": { "color_space": "bt709", "color_transfer": "bt709",
                       "color_encoding": "bt601", "color_range": "limited" },
      "camera_info": { "frame_id": "camera", "width": 1920, "height": 1080 },
      "tf_static": { "base_frame_id": "base_link", "child_frame_id": "camera" }
    }"#;

    fn with(field: &str, value: Value) -> Vec<u8> {
        let mut v: Value = serde_json::from_str(RECORDED).unwrap();
        v[field] = value;
        v.to_string().into_bytes()
    }

    #[test]
    fn reads_a_recorded_sidecar() {
        let s = Sidecar::parse(RECORDED.as_bytes()).unwrap();
        assert_eq!((s.fps, s.width, s.height), (30.0, 1920, 1080));
        assert_eq!(
            s.colorimetry,
            Some(Colorimetry {
                space: Some(ColorSpace::Bt709),
                transfer: Some(ColorTransfer::Bt709),
                encoding: Some(ColorEncoding::Bt601),
                range: Some(ColorRange::Limited),
            })
        );
    }

    #[test]
    fn extras_keep_every_application_field_and_nothing_else() {
        let s = Sidecar::parse(RECORDED.as_bytes()).unwrap();
        let extras: Value = serde_json::from_str(&s.extras).unwrap();
        let recorded: Value = serde_json::from_str(RECORDED).unwrap();
        let keys: Vec<_> = extras.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["camera_info", "tf_static"]);
        assert_eq!(extras["camera_info"], recorded["camera_info"]);
        assert_eq!(extras["tf_static"], recorded["tf_static"]);
    }

    #[test]
    fn empty_and_unknown_colorimetry_labels_are_unspecified() {
        let partial = with(
            "colorimetry",
            serde_json::json!({ "color_space": "", "color_range": "full", "color_encoding": "xyz" }),
        );
        let c = Sidecar::parse(&partial).unwrap().colorimetry.unwrap();
        assert_eq!((c.space, c.encoding), (None, None));
        assert_eq!(c.range, Some(ColorRange::Full));
        let none = with("colorimetry", serde_json::json!({ "color_space": "" }));
        assert_eq!(Sidecar::parse(&none).unwrap().colorimetry, None);
    }

    #[test]
    fn rejects_other_versions_and_codecs() {
        let v2 = Sidecar::parse(&with("version", 2.into())).unwrap_err();
        assert_eq!(v2.kind(), ErrorKind::InvalidConfig);
        assert!(v2.to_string().contains("version"), "{v2}");
        let h265 = Sidecar::parse(&with("codec", "h265".into())).unwrap_err();
        assert_eq!(h265.kind(), ErrorKind::UnsupportedFormat);
    }

    #[test]
    fn rejects_missing_or_bad_stream_fields() {
        for (field, value) in [
            ("fps", Value::from(0)),
            ("fps", Value::from("30")),
            ("width", Value::from(0)),
            ("height", Value::from(-1)),
            ("height", Value::from(u64::from(u32::MAX) + 1)),
            ("width", Value::Null),
        ] {
            let err = Sidecar::parse(&with(field, value.clone())).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidConfig, "{field} = {value}");
        }
        assert!(Sidecar::parse(b"[1, 2]").is_err());
        assert!(Sidecar::parse(b"{ not json").is_err());
    }

    #[test]
    fn accepts_a_fractional_rate() {
        let s = Sidecar::parse(&with("fps", 29.97.into())).unwrap();
        assert_eq!(s.fps, 29.97);
    }

    #[test]
    fn pairs_by_swapping_the_extension() {
        assert_eq!(
            paired_path(Path::new("run01.h264")),
            Path::new("run01.json")
        );
        assert_eq!(
            paired_path(Path::new("rec/run01.h264")),
            Path::new("rec/run01.json")
        );
        assert_eq!(paired_path(Path::new("capture")), Path::new("capture.json"));
    }

    #[test]
    fn a_missing_sidecar_is_not_found() {
        let err = Sidecar::load(Path::new("/nonexistent/recording.h264")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(err.to_string().contains("recording.json"), "{err}");
    }
}

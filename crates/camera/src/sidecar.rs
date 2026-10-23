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

/// The sidecar of a recorded H.264 stream (`file:` sources).
///
/// Read it with [`load`](Self::load); a recorder fills one in and writes it
/// next to its stream with [`write`](Self::write).
///
/// ```
/// use edgefirst_camera::Sidecar;
///
/// let mut sidecar = Sidecar::new(30.0, 1920, 1080);
/// sidecar.extras = r#"{"camera_info":{"frame_id":"camera"}}"#.into();
/// let json = sidecar.to_json()?;
/// assert_eq!(Sidecar::parse(json.as_bytes())?, sidecar);
/// # Ok::<(), edgefirst_camera::Error>(())
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Sidecar {
    /// Frame rate of the stream.
    pub fps: f64,
    /// Width of the encoded pictures.
    pub width: u32,
    /// Height of the encoded pictures.
    pub height: u32,
    /// Colorimetry of the pictures, or `None` when unspecified.
    pub colorimetry: Option<Colorimetry>,
    /// The application's fields, as a JSON object (`{}` for none).
    pub extras: String,
}

/// The sidecar path of `stream`: the same path with a `.json` extension.
pub(crate) fn paired_path(stream: &Path) -> PathBuf {
    stream.with_extension("json")
}

impl Sidecar {
    /// A sidecar for a `width`x`height` stream at `fps`, with unspecified
    /// colorimetry and no extras.
    pub fn new(fps: f64, width: u32, height: u32) -> Self {
        Self {
            fps,
            width,
            height,
            colorimetry: None,
            extras: "{}".into(),
        }
    }

    /// The sidecar path paired with `stream`: `run01.h264` pairs with
    /// `run01.json`.
    pub fn paired_path(stream: &Path) -> PathBuf {
        paired_path(stream)
    }

    /// Reads the sidecar paired with `stream`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotFound`] when there is no sidecar, [`ErrorKind::Io`]
    /// when it cannot be read, and the errors of [`parse`](Self::parse).
    pub fn load(stream: &Path) -> Result<Self> {
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

    /// Parses a sidecar.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::UnsupportedFormat`] for a codec other than H.264, and
    /// [`ErrorKind::InvalidConfig`] for any other version, a missing or
    /// invalid stream field, or text that is not a JSON object.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
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

    /// The sidecar as pretty-printed JSON. A whole frame rate is written as
    /// an integer, and unspecified colorimetry as empty labels, as camera
    /// 2.x reads them.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfig`] when the stream fields are not positive,
    /// or `extras` is not a JSON object or names a stream field.
    pub fn to_json(&self) -> Result<String> {
        let invalid = |why: String| Error::new(ErrorKind::InvalidConfig, why);
        if !(self.fps.is_finite() && self.fps > 0.0) {
            return Err(invalid(format!("fps {} must be positive", self.fps)));
        }
        if self.width == 0 || self.height == 0 {
            return Err(invalid(format!(
                "size {}x{} must be positive",
                self.width, self.height
            )));
        }
        let extras = match serde_json::from_str(&self.extras) {
            Ok(Value::Object(extras)) => extras,
            _ => return Err(invalid("extras must be a JSON object".into())),
        };
        if let Some(key) = extras.keys().find(|k| STREAM_FIELDS.contains(&k.as_str())) {
            return Err(invalid(format!("extras must not set the stream field {key:?}")));
        }
        let fps = if self.fps.fract() == 0.0 && self.fps <= u32::MAX as f64 {
            Value::from(self.fps as u32)
        } else {
            Value::from(self.fps)
        };
        let c = self.colorimetry.unwrap_or_default();
        let label = |code: Option<&'static str>| Value::from(code.unwrap_or(""));
        let mut object = Map::new();
        object.insert("version".into(), VERSION.into());
        object.insert("codec".into(), CODEC.into());
        object.insert("fps".into(), fps);
        object.insert("width".into(), self.width.into());
        object.insert("height".into(), self.height.into());
        object.insert(
            "colorimetry".into(),
            serde_json::json!({
                "color_space": label(c.space.map(|v| v.as_str())),
                "color_transfer": label(c.transfer.map(|v| v.as_str())),
                "color_encoding": label(c.encoding.map(|v| v.as_str())),
                "color_range": label(c.range.map(|v| v.as_str())),
            }),
        );
        object.extend(extras);
        serde_json::to_string_pretty(&Value::Object(object)).map_err(|e| invalid(e.to_string()))
    }

    /// Writes the sidecar next to `stream` and returns its path.
    ///
    /// # Errors
    ///
    /// The errors of [`to_json`](Self::to_json), and [`ErrorKind::Io`] when
    /// the file cannot be written.
    pub fn write(&self, stream: &Path) -> Result<PathBuf> {
        let path = paired_path(stream);
        std::fs::write(&path, self.to_json()?).map_err(|e| {
            Error::new(ErrorKind::Io, format!("sidecar {}: {e}", path.display())).with_source(e)
        })?;
        Ok(path)
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

    #[test]
    fn writes_what_it_reads() {
        let recorded = Sidecar::parse(RECORDED.as_bytes()).unwrap();
        let json = recorded.to_json().unwrap();
        assert_eq!(Sidecar::parse(json.as_bytes()).unwrap(), recorded);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert!(v["fps"].is_u64(), "a whole rate is an integer: {json}");
        assert_eq!(v["camera_info"]["frame_id"], "camera");
    }

    #[test]
    fn writes_unspecified_colorimetry_as_empty_labels() {
        let json = Sidecar::new(29.97, 640, 480).to_json().unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["fps"], 29.97);
        assert_eq!(v["colorimetry"]["color_space"], "");
        assert_eq!(v["colorimetry"]["color_range"], "");
    }

    #[test]
    fn refuses_to_write_invalid_extras_or_fields() {
        let mut s = Sidecar::new(30.0, 640, 480);
        for bad in ["[1]", "not json", r#"{"fps": 60}"#] {
            s.extras = bad.into();
            assert_eq!(s.to_json().unwrap_err().kind(), ErrorKind::InvalidConfig, "{bad}");
        }
        let zero = Sidecar::new(0.0, 640, 480);
        assert!(zero.to_json().is_err());
        let empty = Sidecar::new(30.0, 0, 480);
        assert!(empty.to_json().is_err());
    }

    #[test]
    fn writes_beside_the_stream() {
        let dir = std::env::temp_dir().join(format!("edgefirst-sidecar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stream = dir.join("run.h264");
        let written = Sidecar::new(30.0, 640, 480).write(&stream).unwrap();
        assert_eq!(written, dir.join("run.json"));
        assert_eq!(Sidecar::load(&stream).unwrap().width, 640);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

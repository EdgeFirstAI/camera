// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

//! The messages this service publishes, other than `camera/frame`'s tensor
//! (the SDK's `schema` mapping): `camera/info`, `tf_static`, the compressed
//! image and video payloads, and the Zenoh sample timestamp they share.

use std::{error::Error, fs::File};

use edgefirst_camera::schema;
use edgefirst_schemas::{
    builtin_interfaces::Time,
    foxglove_msgs::FoxgloveCompressedVideo,
    geometry_msgs::{Quaternion, Transform, TransformStamped, Vector3},
    sensor_msgs::{CameraInfo, CompressedImage, RegionOfInterest},
};
use tracing::warn;
use zenoh::{
    bytes::{Encoding, ZBytes},
    time::{Timestamp as ZenohTimestamp, NTP64},
    Session,
};

use crate::args::Args;

/// The Zenoh sample timestamp for a header stamp: the same instant, in the
/// session's name.
pub(crate) fn sample_timestamp(session: &Session, stamp: Time) -> ZenohTimestamp {
    ZenohTimestamp::new(NTP64(schema::ntp64(stamp)), session.zid().into())
}

/// A `sensor_msgs/CompressedImage` JPEG payload.
pub(crate) fn jpeg_msg(
    jpeg: &[u8],
    stamp: Time,
    frame_id: &str,
) -> Result<(ZBytes, Encoding), Box<dyn Error>> {
    let msg = CompressedImage::builder()
        .stamp(stamp)
        .frame_id(frame_id)
        .format("jpeg")
        .data(jpeg)
        .build()?;
    let enc = Encoding::APPLICATION_CDR.with_schema("sensor_msgs/msg/CompressedImage");
    Ok((ZBytes::from(msg.into_cdr()), enc))
}

/// A `foxglove_msgs/CompressedVideo` H.264 payload.
pub(crate) fn video_msg(
    data: &[u8],
    stamp: Time,
    frame_id: &str,
) -> Result<(ZBytes, Encoding), Box<dyn Error>> {
    let msg = FoxgloveCompressedVideo::builder()
        .stamp(stamp)
        .frame_id(frame_id)
        .data(data)
        .format("h264")
        .build()?;
    let enc = Encoding::APPLICATION_CDR.with_schema("foxglove_msgs/msg/CompressedVideo");
    Ok((ZBytes::from(msg.into_cdr()), enc))
}

/// Geometry fields shared by the calibration loader and the built-in
/// fallback: `(width, height, distortion_model, d, k, r, p)`.
type Calibration = (
    u32,
    u32,
    &'static str,
    Vec<f64>,
    [f64; 9],
    [f64; 9],
    [f64; 12],
);

/// Plain-Rust projection of a `sensor_msgs/CameraInfo` payload, decoupled
/// from the CDR-backed wire type. Lets live capture and record/replay
/// share the same shape: the live path builds it from `Args` at startup,
/// the record path serializes it into the sidecar, and the replay path
/// deserializes it back without re-running the JSON / CLI parsers.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct CameraInfoFields {
    pub frame_id: String,
    pub width: u32,
    pub height: u32,
    pub distortion_model: String,
    pub d: Vec<f64>,
    pub k: [f64; 9],
    pub r: [f64; 9],
    pub p: [f64; 12],
    pub binning_x: u32,
    pub binning_y: u32,
    pub roi: RoiFields,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct RoiFields {
    pub x_offset: u32,
    pub y_offset: u32,
    pub height: u32,
    pub width: u32,
    pub do_rectify: bool,
}

impl From<RoiFields> for RegionOfInterest {
    fn from(r: RoiFields) -> Self {
        RegionOfInterest {
            x_offset: r.x_offset,
            y_offset: r.y_offset,
            height: r.height,
            width: r.width,
            do_rectify: r.do_rectify,
        }
    }
}

impl CameraInfoFields {
    /// Compute the fields that would populate a live `/camera/info`
    /// message from `Args`.
    ///
    /// Reads the calibration JSON at `args.capture.cam_info_path` when one is
    /// configured. A calibration that cannot be loaded -- absent,
    /// unreadable, malformed, or structurally wrong -- is reported and
    /// then treated exactly like no calibration at all: the node keeps
    /// running on `default_calibration` and `camera/info` keeps
    /// publishing. Calibration is only needed to describe the geometry on
    /// that one topic, so losing it must not take the capture pipeline
    /// down with it, and a subscriber that requires `camera/info` to
    /// exist (edgefirst-fusion) must not be starved because one file went
    /// missing under a service that was otherwise healthy.
    pub(crate) fn from_args(args: &Args) -> Self {
        let loaded = if args.capture.cam_info_path.is_empty() {
            None
        } else {
            match Self::load_calibration(&args.capture.cam_info_path) {
                Ok(fields) => Some(fields),
                Err(e) => {
                    warn!(
                        path = %args.capture.cam_info_path,
                        error = %e,
                        "cannot load camera calibration; falling back to built-in defaults -- \
                         camera/info geometry is APPROXIMATE and not valid for projection or \
                         sensor fusion"
                    );
                    None
                }
            }
        };

        let (width, height, distortion_model, d, k, r, p) =
            loaded.unwrap_or_else(Self::default_calibration);

        CameraInfoFields {
            frame_id: args.capture.camera_frame_id.clone(),
            width,
            height,
            distortion_model: distortion_model.to_string(),
            d,
            k,
            r,
            p,
            binning_x: 1,
            binning_y: 1,
            roi: RoiFields {
                x_offset: 0,
                y_offset: 0,
                height,
                width,
                do_rectify: false,
            },
        }
    }

    /// The built-in fallback geometry, used when no calibration is
    /// configured and when a configured one cannot be loaded. Nominal for
    /// a 1080p sensor; it describes no particular lens.
    fn default_calibration() -> Calibration {
        let k = [1270.0, 0.0, 960.0, 0.0, 1270.0, 540.0, 0.0, 0.0, 1.0];
        let p = [
            k[0], k[1], k[2], 0.0, k[3], k[4], k[5], 0.0, k[6], k[7], k[8], 0.0,
        ];
        let r = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        (1920, 1080, "plumb_bob", vec![0.0; 5], k, r, p)
    }

    /// Parse an isp-imx dewarp configuration into `CameraInfo` geometry.
    fn load_calibration(path: &str) -> Result<Calibration, Box<dyn Error>> {
        let file = File::open(path).map_err(|e| format!("Cannot open file {:?}: {e:?}", path))?;
        let json: serde_json::Value = serde_json::from_reader(file)
            .map_err(|e| format!("Cannot parse camera info JSON from {:?}: {e}", path))?;
        let bypass = json["bypass"].as_bool().unwrap_or(false);
        let dewarp_configs = &json["dewarpConfigArray"];
        if !dewarp_configs.is_array() {
            return Err(Box::from("Did not find dewarpConfigArray as an array"));
        }
        let dewarp_config = &dewarp_configs[0];
        let d: Vec<f64> = if bypass {
            let distortion_coeff = dewarp_config["distortion_coeff"].as_array();
            match distortion_coeff {
                Some(v) => Self::parse_f64_array(v, "distortion_coeff")?,
                None => {
                    return Err(Box::from("Did not find distortion_coeff as an array"));
                }
            }
        } else {
            // the camera driver already applies this distortion correction, so we
            // set it to zero, as ROS expects the camera info to contain the distortion
            // information of the image coming from the camera
            vec![0.0; 5]
        };

        let camera_matrix = dewarp_config["camera_matrix"].as_array();
        let kv: Vec<f64> = match camera_matrix {
            Some(v) => Self::parse_f64_array(v, "camera_matrix")?,
            None => return Err(Box::from("Did not find camera_matrix as an array")),
        };
        if kv.len() != 9 {
            return Err(Box::from(format!(
                "Expected exactly 9 elements in camera_matrix array but found {}",
                kv.len()
            )));
        }
        let p = [
            kv[0], kv[1], kv[2], 0.0, kv[3], kv[4], kv[5], 0.0, kv[6], kv[7], kv[8], 0.0,
        ];
        let k = [
            kv[0], kv[1], kv[2], kv[3], kv[4], kv[5], kv[6], kv[7], kv[8],
        ];

        let width = dewarp_config["source_image"]["width"]
            .as_f64()
            .ok_or("Did not find camera width in camera info json")? as u32;
        let height = dewarp_config["source_image"]["height"]
            .as_f64()
            .ok_or("Did not find camera height in camera info json")? as u32;
        let r = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

        Ok((width, height, "plumb_bob", d, k, r, p))
    }

    /// Parse a JSON array into `f64`s, failing on the first non-numeric
    /// element rather than silently coercing it to `0.0` -- a
    /// structurally invalid calibration must not look successfully
    /// loaded.
    fn parse_f64_array(
        values: &[serde_json::Value],
        field: &str,
    ) -> Result<Vec<f64>, Box<dyn Error>> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                v.as_f64()
                    .ok_or_else(|| Box::<dyn Error>::from(format!("{field}[{i}] is not a number")))
            })
            .collect()
    }

    /// Serialize these fields into a fresh `sensor_msgs/CameraInfo` CDR
    /// buffer stamped with the acquisition time of the frame it describes.
    pub(crate) fn build_msg(&self, stamp: Time) -> Result<CameraInfo<Vec<u8>>, Box<dyn Error>> {
        Ok(CameraInfo::builder()
            .stamp(stamp)
            .frame_id(self.frame_id.as_str())
            .height(self.height)
            .width(self.width)
            .distortion_model(self.distortion_model.as_str())
            .d(&self.d)
            .k(self.k)
            .r(self.r)
            .p(self.p)
            .binning_x(self.binning_x)
            .binning_y(self.binning_y)
            .roi(self.roi.into())
            .build()?)
    }
}

/// Plain-Rust projection of a `geometry_msgs/TransformStamped` for
/// `/tf_static`. Same motivation as [`CameraInfoFields`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct TfStaticFields {
    pub base_frame_id: String,
    pub child_frame_id: String,
    /// Translation vector (x, y, z).
    pub translation: [f64; 3],
    /// Rotation quaternion (x, y, z, w).
    pub rotation: [f64; 4],
}

impl TfStaticFields {
    pub(crate) fn from_args(args: &Args) -> Self {
        TfStaticFields {
            base_frame_id: args.capture.base_frame_id.clone(),
            child_frame_id: args.capture.camera_frame_id.clone(),
            translation: [
                args.capture.cam_tf_vec[0],
                args.capture.cam_tf_vec[1],
                args.capture.cam_tf_vec[2],
            ],
            rotation: [
                args.capture.cam_tf_quat[0],
                args.capture.cam_tf_quat[1],
                args.capture.cam_tf_quat[2],
                args.capture.cam_tf_quat[3],
            ],
        }
    }

    pub(crate) fn build_msg(
        &self,
        stamp: Time,
    ) -> Result<TransformStamped<Vec<u8>>, Box<dyn Error>> {
        let transform = Transform {
            translation: Vector3 {
                x: self.translation[0],
                y: self.translation[1],
                z: self.translation[2],
            },
            rotation: Quaternion {
                x: self.rotation[0],
                y: self.rotation[1],
                z: self.rotation[2],
                w: self.rotation[3],
            },
        };

        Ok(TransformStamped::builder()
            .stamp(stamp)
            .frame_id(self.base_frame_id.as_str())
            .child_frame_id(self.child_frame_id.as_str())
            .transform(transform)
            .build()?)
    }
}

/// The `camera_info` and `tf_static` a recording's sidecar carries, or
/// `None` for a field that is missing or malformed, with a warning.
pub(crate) fn from_extras(extras: &str) -> (Option<CameraInfoFields>, Option<TfStaticFields>) {
    let value: serde_json::Value = match serde_json::from_str(extras) {
        Ok(v) => v,
        Err(e) => {
            warn!("recording metadata is not JSON: {e}");
            return (None, None);
        }
    };
    fn field<T: serde::de::DeserializeOwned>(value: &serde_json::Value, name: &str) -> Option<T> {
        let v = value.get(name)?;
        serde_json::from_value(v.clone())
            .map_err(|e| warn!("recording {name} is malformed, using the command line's: {e}"))
            .ok()
    }
    (field(&value, "camera_info"), field(&value, "tf_static"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn default_args() -> Args {
        Args::parse_from(["edgefirst-camera"])
    }

    #[test]
    fn camera_info_fields_from_args_with_no_json_path_uses_defaults() {
        let mut args = default_args();
        args.capture.cam_info_path = String::new();
        let f = CameraInfoFields::from_args(&args);
        assert_eq!(f.width, 1920);
        assert_eq!(f.height, 1080);
        assert_eq!(f.distortion_model, "plumb_bob");
        assert_eq!(f.d.len(), 5);
        assert!(f.d.iter().all(|&v| v == 0.0));
        // Default k has 1270 on the principal-axis focals and 960/540 on
        // the principal point for 1920x1080.
        assert_eq!(f.k[0], 1270.0);
        assert_eq!(f.k[4], 1270.0);
        assert_eq!(f.k[2], 960.0);
        assert_eq!(f.k[5], 540.0);
        assert_eq!(f.binning_x, 1);
        assert_eq!(f.binning_y, 1);
        assert_eq!(f.roi.width, 1920);
        assert_eq!(f.roi.height, 1080);
        assert!(!f.roi.do_rectify);
    }

    /// A calibration that could not be loaded must leave the node running
    /// on the same built-in defaults it uses when no path is configured
    /// at all -- `camera/info` keeps publishing, with approximate
    /// geometry, rather than the process dying over one unreadable file.
    fn assert_built_in_defaults(f: &CameraInfoFields) {
        assert_eq!(f.width, 1920);
        assert_eq!(f.height, 1080);
        assert_eq!(f.distortion_model, "plumb_bob");
        assert_eq!(f.d.len(), 5);
        assert!(f.d.iter().all(|&v| v == 0.0));
        assert_eq!(f.k[0], 1270.0);
        assert_eq!(f.k[4], 1270.0);
        assert_eq!(f.k[2], 960.0);
        assert_eq!(f.k[5], 540.0);
    }

    #[test]
    fn camera_info_fields_degrades_when_file_missing() {
        // A CAM_INFO_PATH left pointing at a path that no longer exists,
        // such as a stale /etc/default/camera carried across an upgrade.
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let path = tmp.join(format!("edgefirst_cam_info_absent_{pid}.json"));
        std::fs::remove_file(&path).ok();

        let mut args = default_args();
        args.capture.cam_info_path = path.to_string_lossy().into_owned();
        let f = CameraInfoFields::from_args(&args);
        assert_built_in_defaults(&f);
    }

    #[test]
    fn camera_info_fields_degrades_on_bad_camera_matrix_length() {
        // camera_matrix of length 8 hits the length-validation branch.
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let path = tmp.join(format!("edgefirst_cam_info_bad_matrix_{pid}.json"));
        std::fs::write(
            &path,
            r#"{
                "bypass": false,
                "dewarpConfigArray": [{
                    "camera_matrix": [1,2,3,4,5,6,7,8],
                    "source_image": {"width": 1920, "height": 1080}
                }]
            }"#,
        )
        .unwrap();

        let mut args = default_args();
        args.capture.cam_info_path = path.to_string_lossy().into_owned();
        let f = CameraInfoFields::from_args(&args);
        assert_built_in_defaults(&f);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn camera_info_fields_degrades_on_malformed_json() {
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let path = tmp.join(format!("edgefirst_cam_info_bad_json_{pid}.json"));
        std::fs::write(&path, b"this is not valid json {").unwrap();

        let mut args = default_args();
        args.capture.cam_info_path = path.to_string_lossy().into_owned();
        let f = CameraInfoFields::from_args(&args);
        assert_built_in_defaults(&f);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn camera_info_fields_degrades_on_missing_dewarp_array() {
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let path = tmp.join(format!("edgefirst_cam_info_no_array_{pid}.json"));
        std::fs::write(&path, r#"{"dewarpConfigArray": "not_an_array"}"#).unwrap();

        let mut args = default_args();
        args.capture.cam_info_path = path.to_string_lossy().into_owned();
        let f = CameraInfoFields::from_args(&args);
        assert_built_in_defaults(&f);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn camera_info_fields_degrades_on_non_numeric_camera_matrix_element() {
        // A `camera_matrix` with the right length but a non-numeric
        // element used to be silently coerced to 0.0, producing a bogus
        // intrinsics matrix that looked "successfully loaded" and never
        // triggered the fallback warning.
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let path = tmp.join(format!("edgefirst_cam_info_nan_matrix_{pid}.json"));
        std::fs::write(
            &path,
            r#"{
                "bypass": false,
                "dewarpConfigArray": [{
                    "camera_matrix": [1,2,3,4,"bad",6,7,8,9],
                    "source_image": {"width": 1920, "height": 1080}
                }]
            }"#,
        )
        .unwrap();

        let mut args = default_args();
        args.capture.cam_info_path = path.to_string_lossy().into_owned();
        let f = CameraInfoFields::from_args(&args);
        assert_built_in_defaults(&f);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn camera_info_fields_degrades_on_non_numeric_distortion_coeff_element() {
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let path = tmp.join(format!("edgefirst_cam_info_nan_distortion_{pid}.json"));
        std::fs::write(
            &path,
            r#"{
                "bypass": true,
                "dewarpConfigArray": [{
                    "distortion_coeff": [1,2,3,4,null],
                    "camera_matrix": [1,2,3,4,5,6,7,8,9],
                    "source_image": {"width": 1920, "height": 1080}
                }]
            }"#,
        )
        .unwrap();

        let mut args = default_args();
        args.capture.cam_info_path = path.to_string_lossy().into_owned();
        let f = CameraInfoFields::from_args(&args);
        assert_built_in_defaults(&f);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn camera_info_fields_degrades_on_missing_source_image_dimensions() {
        // A missing width/height used to be logged as an `error!` and
        // silently substituted (1920x1080) while still returning `Ok`,
        // so `from_args` never emitted the fallback warning even though
        // the calibration was structurally incomplete.
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let path = tmp.join(format!("edgefirst_cam_info_no_dims_{pid}.json"));
        std::fs::write(
            &path,
            r#"{
                "bypass": false,
                "dewarpConfigArray": [{
                    "camera_matrix": [1,2,3,4,5,6,7,8,9],
                    "source_image": {}
                }]
            }"#,
        )
        .unwrap();

        let mut args = default_args();
        args.capture.cam_info_path = path.to_string_lossy().into_owned();
        let f = CameraInfoFields::from_args(&args);
        assert_built_in_defaults(&f);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tf_static_fields_from_args_mirrors_cli_shape() {
        let args = default_args();
        let tf = TfStaticFields::from_args(&args);
        // The defaults may evolve, but base/child frame IDs must always
        // come from the CLI strings and the arrays must have the
        // expected arity.
        assert_eq!(tf.base_frame_id, args.capture.base_frame_id);
        assert_eq!(tf.child_frame_id, args.capture.camera_frame_id);
        assert_eq!(tf.translation.len(), 3);
        assert_eq!(tf.rotation.len(), 4);
    }

    #[test]
    fn tf_static_fields_build_msg_produces_nonempty_cdr() {
        let args = default_args();
        let tf = TfStaticFields::from_args(&args);
        let stamp = Time {
            sec: 1_790_000_000,
            nanosec: 123_456_789,
        };
        let msg = tf.build_msg(stamp).expect("tf CDR build must succeed");
        assert!(!msg.as_cdr().is_empty());
        assert_eq!(msg.stamp(), stamp);
    }

    #[test]
    fn camera_info_fields_build_msg_produces_nonempty_cdr() {
        let mut args = default_args();
        args.capture.cam_info_path = String::new();
        let info = CameraInfoFields::from_args(&args);
        let stamp = Time {
            sec: 1_790_000_000,
            nanosec: 123_456_789,
        };
        let msg = info.build_msg(stamp).expect("info CDR build must succeed");
        assert!(!msg.as_cdr().is_empty());
        assert_eq!(msg.stamp(), stamp);
    }

    #[test]
    fn extras_round_trip_the_recorded_fields() {
        let args = default_args();
        let info = CameraInfoFields::from_args(&args);
        let tf = TfStaticFields::from_args(&args);
        let extras = serde_json::json!({ "camera_info": info, "tf_static": tf }).to_string();
        let (i, t) = from_extras(&extras);
        assert_eq!(i.unwrap().k, info.k);
        assert_eq!(t.unwrap().rotation, tf.rotation);
    }

    #[test]
    fn missing_or_malformed_extras_are_none() {
        assert!(matches!(from_extras("{}"), (None, None)));
        assert!(matches!(from_extras("not json"), (None, None)));
        let bad = r#"{"camera_info": {"width": "wide"}, "tf_static": 3}"#;
        assert!(matches!(from_extras(bad), (None, None)));
    }

    #[test]
    fn sample_timestamp_is_the_header_stamp() {
        let stamp = Time {
            sec: 1_790_000_000,
            nanosec: 123_456_789,
        };
        let t = sample_timestamp_of(stamp);
        let back = t.get_time().to_duration();
        let want = std::time::Duration::new(stamp.sec as u64, stamp.nanosec);
        let diff = back.abs_diff(want);
        assert!(diff <= std::time::Duration::from_nanos(2), "{diff:?}");
    }

    fn sample_timestamp_of(stamp: Time) -> ZenohTimestamp {
        ZenohTimestamp::new(
            NTP64(schema::ntp64(stamp)),
            zenoh::time::TimestampId::rand(),
        )
    }
}

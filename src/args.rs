// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

use clap::Parser;
use serde_json::json;
use std::path::PathBuf;
use zenoh::config::{Config, WhatAmI};

/// Camera image mirroring options.
///
/// Determines how the camera image should be flipped before processing.
/// Useful for correcting camera orientation.
#[derive(clap::ValueEnum, Clone, Debug, PartialEq, Copy)]
pub enum MirrorSetting {
    /// No mirroring
    None,
    /// Flip horizontally (left-right)
    Horizontal,
    /// Flip vertically (top-bottom)
    Vertical,
    /// Flip both horizontally and vertically (180-degree rotation)
    Both,
}

/// H.264 encoding bitrate presets.
///
/// Controls the trade-off between video quality and file size.
/// Higher bitrates produce better quality but larger files.
#[derive(clap::ValueEnum, Clone, Debug, PartialEq, Copy)]
pub enum H264Bitrate {
    /// Automatic bitrate selection based on resolution
    Auto,
    /// 5 Mbps (suitable for 720p)
    Mbps5,
    /// 25 Mbps (suitable for 1080p)
    Mbps25,
    /// 50 Mbps (suitable for high-quality 1080p)
    Mbps50,
    /// 100 Mbps (suitable for 4K or very high quality)
    Mbps100,
}

/// Command-line arguments for EdgeFirst Camera Node.
///
/// This structure defines all configuration options for the camera node,
/// including camera selection, output formats, Zenoh configuration, and
/// debugging options. Arguments can be specified via command line or
/// environment variables.
///
/// # Example
///
/// ```bash
/// # Via command line
/// edgefirst-camera --camera /dev/video0 --jpeg --h264
///
/// # Via environment variables
/// export CAMERA=/dev/video0
/// export JPEG=true
/// edgefirst-camera
/// ```
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Camera capture device path (e.g., /dev/video0)
    #[arg(short, long, env = "CAMERA", default_value = "/dev/video3")]
    pub camera: String,

    /// Camera capture resolution in pixels (width height)
    #[arg(
        long,
        env = "CAMERA_SIZE",
        default_value = "1920 1080",
        value_delimiter = ' ',
        num_args = 2
    )]
    pub camera_size: Vec<u32>,

    /// Camera image mirroring setting
    #[arg(long, env = "MIRROR", default_value = "both", value_enum)]
    pub mirror: MirrorSetting,

    /// Zenoh topic for multi-plane camera frame (edgefirst_msgs/CameraFrame).
    /// Supersedes `--dma-topic` from 2.6.x. The new topic drops the `rt/`
    /// prefix per the schemas 3.1 convention for newly introduced topics.
    #[arg(long, default_value = "camera/frame")]
    pub frame_topic: String,

    /// Zenoh topic for camera calibration info (sensor_msgs/CameraInfo)
    #[arg(long, default_value = "rt/camera/info")]
    pub info_topic: String,

    /// Enable JPEG streaming output
    #[arg(long, env = "JPEG")]
    pub jpeg: bool,

    /// Zenoh topic for JPEG compressed images (sensor_msgs/CompressedImage)
    #[arg(long, default_value = "rt/camera/jpeg")]
    pub jpeg_topic: String,

    /// Enable H.264 video streaming output
    #[arg(long, env = "H264")]
    pub h264: bool,

    /// Zenoh topic for H.264 video stream (foxglove_msgs/CompressedVideo)
    #[arg(long, default_value = "rt/camera/h264")]
    pub h264_topic: String,

    /// H.264 encoding bitrate preset
    #[arg(long, env = "H264_BITRATE", default_value = "auto")]
    pub h264_bitrate: H264Bitrate,

    /// Enable 4K tiling (splits 4K into 4x 1080p tiles for hardware encoding)
    #[arg(long, env = "H264_TILES")]
    pub h264_tiles: bool,

    /// Zenoh topics for H.264 tiles: top-left, top-right, bottom-left,
    /// bottom-right
    #[arg(
        long,
        default_value = "rt/camera/h264/tl rt/camera/h264/tr rt/camera/h264/bl rt/camera/h264/br",
        value_delimiter = ' ',
        num_args = 4
    )]
    pub h264_tiles_topics: Vec<String>,

    /// FPS limit for H.264 tiles (lower than camera FPS to reduce compression
    /// artifacts)
    #[arg(long, env = "H264_TILES_FPS", default_value = "15")]
    pub h264_tiles_fps: u32,

    /// Record the live H.264 stream to this file (raw Annex-B `.h264`).
    ///
    /// A matching `<path>.json` sidecar is written alongside at startup
    /// carrying colorimetry, `/camera/info`, and `/tf_static` — every
    /// piece of producer-global state that is not recoverable from the
    /// bitstream. Requires `--h264`; mutually exclusive with `--replay`.
    #[arg(long, env = "RECORD", conflicts_with = "replay")]
    pub record: Option<PathBuf>,

    /// Replay a previously recorded H.264 file instead of opening a V4L2
    /// camera device.
    ///
    /// Requires the matching `<path>.json` sidecar alongside the `.h264`
    /// file. Mutually exclusive with `--record`. When enabled, `--jpeg`
    /// and `--h264-tiles` are rejected because the recorded file carries
    /// only the main H.264 bitstream.
    #[arg(long, env = "REPLAY")]
    pub replay: Option<PathBuf>,

    /// Loop the replay back to the start on EOF instead of exiting.
    ///
    /// The `CameraFrame.seq` counter continues to increment across loop
    /// boundaries so consumers see one continuous monotonic stream,
    /// matching the contract of a live camera session.
    #[arg(long, env = "REPLAY_LOOP", default_value_t = false)]
    pub replay_loop: bool,

    /// Override the playback frame rate. Defaults to the sidecar's `fps`.
    #[arg(long, env = "REPLAY_FPS")]
    pub replay_fps: Option<u32>,

    /// Output streaming resolution in pixels (width height)
    #[arg(
        short,
        long,
        env = "STREAM_SIZE",
        default_value = "1920 1080",
        value_delimiter = ' ',
        num_args = 2
    )]
    pub stream_size: Vec<u32>,

    /// Enable verbose debug logging
    #[arg(short, long)]
    pub verbose: bool,

    /// Path to camera calibration JSON file (isp-imx format)
    #[arg(long, env = "CAM_INFO_PATH", default_value = "")]
    pub cam_info_path: String,

    /// Camera optical frame translation from base_link (x y z in meters)
    #[arg(
        long,
        env = "CAM_TF_VEC",
        default_value = "0 0 0",
        value_delimiter = ' ',
        num_args = 3
    )]
    pub cam_tf_vec: Vec<f64>,

    /// Camera optical frame rotation quaternion from base_link (x y z w)
    #[arg(
        long,
        env = "CAM_TF_QUAT",
        default_value = "-1 1 -1 1",
        value_delimiter = ' ',
        num_args = 4
    )]
    pub cam_tf_quat: Vec<f64>,

    /// TF frame ID for robot base
    #[arg(long, default_value = "base_link")]
    pub base_frame_id: String,

    /// TF frame ID for camera optical frame
    #[arg(long, default_value = "camera_optical")]
    pub camera_frame_id: String,

    /// Enable Tokio async runtime console for debugging
    #[arg(long, env = "TOKIO_CONSOLE")]
    pub tokio_console: bool,

    /// Enable Tracy profiler for performance analysis
    #[arg(long, env = "TRACY")]
    pub tracy: bool,

    /// Zenoh participant mode (peer, client, or router)
    #[arg(long, env = "MODE", default_value = "peer")]
    mode: WhatAmI,

    /// Zenoh endpoints to connect to (can specify multiple)
    #[arg(long, env = "CONNECT")]
    connect: Vec<String>,

    /// Zenoh endpoints to listen on (can specify multiple)
    #[arg(long, env = "LISTEN")]
    listen: Vec<String>,

    /// Disable Zenoh multicast peer discovery
    #[arg(long, env = "NO_MULTICAST_SCOUTING")]
    no_multicast_scouting: bool,
}

impl From<Args> for Config {
    fn from(args: Args) -> Self {
        let mut config = Config::default();

        config
            .insert_json5("mode", &json!(args.mode).to_string())
            .unwrap();

        let connect: Vec<_> = args.connect.into_iter().filter(|s| !s.is_empty()).collect();
        if !connect.is_empty() {
            config
                .insert_json5("connect/endpoints", &json!(connect).to_string())
                .unwrap();
        }

        let listen: Vec<_> = args.listen.into_iter().filter(|s| !s.is_empty()).collect();
        if !listen.is_empty() {
            config
                .insert_json5("listen/endpoints", &json!(listen).to_string())
                .unwrap();
        }

        if args.no_multicast_scouting {
            config
                .insert_json5("scouting/multicast/enabled", &json!(false).to_string())
                .unwrap();
        }

        config
            .insert_json5("scouting/multicast/interface", &json!("lo").to_string())
            .unwrap();

        config
    }
}

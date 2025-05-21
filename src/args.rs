use clap::Parser;
use serde_json::json;
use std::path::PathBuf;
use zenoh::config::{Config, WhatAmI};

#[derive(clap::ValueEnum, Clone, Debug, PartialEq, Copy)]
pub enum MirrorSetting {
    None,
    Horizontal,
    Vertical,
    Both,
}

#[derive(clap::ValueEnum, Clone, Debug, PartialEq, Copy)]
pub enum H264Bitrate {
    Auto,
    Mbps5,
    Mbps25,
    Mbps50,
    Mbps100,
}

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// camera capture device
    #[arg(short, long, env, default_value = "/dev/video3")]
    pub camera: String,

    /// camera capture resolution
    #[arg(
        long,
        env,
        default_value = "1920 1080",
        value_delimiter = ' ',
        num_args = 2
    )]
    pub camera_size: Vec<u32>,

    /// camera mirror
    #[arg(long, env, default_value = "both", value_enum)]
    pub mirror: MirrorSetting,

    /// raw dma topic
    #[arg(long, default_value = "rt/camera/dma")]
    pub dma_topic: String,

    /// camera_info topic
    #[arg(long, default_value = "rt/camera/info")]
    pub info_topic: String,

    /// stream JPEGs
    #[arg(long, env)]
    pub jpeg: bool,

    /// jpeg ros topic
    #[arg(long, default_value = "rt/camera/jpeg")]
    pub jpeg_topic: String,

    /// stream H264
    #[arg(long, env)]
    pub h264: bool,

    /// h264 foxglove topic
    #[arg(long, default_value = "rt/camera/h264")]
    pub h264_topic: String,

    /// h264 bitrate setting
    #[arg(long, env, default_value = "auto")]
    pub h264_bitrate: H264Bitrate,

    /// streaming resolution
    #[arg(
        short,
        long,
        env,
        default_value = "1920 1080",
        value_delimiter = ' ',
        num_args = 2
    )]
    pub stream_size: Vec<u32>,

    /// verbose logging
    #[arg(short, long)]
    pub verbose: bool,

    /// isp-imx data location (json format)
    #[arg(long, env)]
    pub cam_info_path: Option<PathBuf>,

    /// camera optical frame transform vector from base_link
    #[arg(
        long,
        env,
        default_value = "0 0 0",
        value_delimiter = ' ',
        num_args = 3
    )]
    pub cam_tf_vec: Vec<f64>,

    /// camera optical frame transform quaternion from base_link
    #[arg(
        long,
        env,
        default_value = "-1 1 -1 1",
        value_delimiter = ' ',
        num_args = 4
    )]
    pub cam_tf_quat: Vec<f64>,

    /// The name of the base frame
    #[arg(long, default_value = "base_link")]
    pub base_frame_id: String,

    /// The name of the camera optical frame
    #[arg(long, default_value = "camera_optical")]
    pub camera_frame_id: String,

    /// Enable tokio console logging
    #[arg(long, env)]
    pub tokio_console: bool,

    /// Enable Tracy profiler broadcast
    #[arg(long, env)]
    pub tracy: bool,

    /// zenoh connection mode
    #[arg(long, env, default_value = "peer")]
    mode: WhatAmI,

    /// connect to zenoh endpoints
    #[arg(long, env)]
    connect: Vec<String>,

    /// listen to zenoh endpoints
    #[arg(long, env)]
    listen: Vec<String>,

    /// disable zenoh multicast scouting
    #[arg(long, env)]
    no_multicast_scouting: bool,
}

impl From<Args> for Config {
    fn from(args: Args) -> Self {
        let mut config = Config::default();

        config
            .insert_json5("mode", &json!(args.mode).to_string())
            .unwrap();

        if !args.connect.is_empty() {
            config
                .insert_json5("connect/endpoints", &json!(args.connect).to_string())
                .unwrap();
        }

        if !args.listen.is_empty() {
            config
                .insert_json5("listen/endpoints", &json!(args.listen).to_string())
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

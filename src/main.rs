use camera::image::{encode_jpeg, Image, ImageManager, RGBA};
use cdr::{CdrLe, Infinite};
use clap::Parser;
use std::{error::Error, str::FromStr, time::Instant};
use video::VideoManager;
use videostream::{
    camera::{create_camera, Mirror},
    fourcc::FourCC,
};
use zenoh::{config::Config, prelude::r#async::*};
use zenoh_ros_type::{
    foxglove_msgs::FoxgloveCompressedVideo, rcl_interfaces::builtin_interfaces::Time as ROSTime,
    sensor_msgs::CompressedImage, std_msgs,
};

mod video;

#[derive(clap::ValueEnum, Clone, Debug)]
enum StreamType {
    Jpeg,
    H264,
}
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// camera capture device
    #[arg(short, long, default_value = "/dev/video3")]
    camera: String,

    /// camera capture resolution
    #[arg(long, default_value = "960 544", value_delimiter = ' ', num_args = 2)]
    stream_size: Vec<i32>,

    /// zenoh connection mode
    #[arg(short, long, default_value = "peer")]
    mode: String,

    /// connect to endpoint
    #[arg(short, long)]
    endpoint: Vec<String>,

    /// ros topic
    #[arg(short, long, default_value = "rt/camera/compressed")]
    topic: String,

    /// verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// stream type
    #[arg(long, default_value = "jpeg", value_enum)]
    codec: StreamType,
}

fn update_fps(prev: &mut Instant, history: &mut Vec<i64>, index: &mut usize) -> i64 {
    let now = Instant::now();

    let elapsed = now.duration_since(*prev);
    *prev = Instant::now();

    history[*index] = 1e9 as i64 / elapsed.as_nanos() as i64;
    *index = (*index + 1) % history.len();

    (history.iter().sum::<i64>() as f64 / history.len() as f64).round() as i64
}

#[async_std::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("VideoStream ROS Publisher");

    let args = Args::parse();
    let mut config = Config::default();

    let mode = WhatAmI::from_str(&args.mode).unwrap();
    config.set_mode(Some(mode)).unwrap();
    config.connect.endpoints = args.endpoint.iter().map(|v| v.parse().unwrap()).collect();

    let session = zenoh::open(config).res().await.unwrap();

    let cam = create_camera()
        .with_device(&args.camera)
        .with_resolution(args.stream_size[0], args.stream_size[1])
        .with_format(FourCC(*b"YUYV"))
        .with_mirror(Mirror::Both)
        .open()?;
    cam.start()?;

    if cam.width() != args.stream_size[0] || cam.height() != args.stream_size[1] {
        eprintln!(
            "WARNING: User requested {} {} resolution but camera set {} {} resolution",
            args.stream_size[0],
            args.stream_size[1],
            cam.width(),
            cam.height()
        );
    }

    let img = Image::new(cam.width(), cam.height(), RGBA)?;
    let imgmgr = ImageManager::new()?;

    let mut prev = Instant::now();
    let mut history = vec![0; 30];
    let mut index = 0;
    match args.codec {
        StreamType::Jpeg => loop {
            let fps = update_fps(&mut prev, &mut history, &mut index);
            let mut now = Instant::now();
            let buf = cam.read()?;
            let ts = buf.timestamp();
            let capture_time = now.elapsed();

            now = Instant::now();
            imgmgr.convert(&Image::from_camera(buf)?, &img, None)?;
            let convert_time = now.elapsed();

            now = Instant::now();
            let dma = img.dmabuf();
            let mem = dma.memory_map()?;
            let jpeg = mem.read(encode_jpeg, Some(&img))?;
            let encode_time = now.elapsed();

            if args.verbose {
                println!(
                    "camera {}x{} image {}x{} size: {}KB jpeg: {}KB capture: {:?} convert: {:?} encode: {:?} fps: {}",
                    cam.width(),
                    cam.height(),
                    img.width(),
                    img.height(),
                    img.width() * img.height() * 4 / 1024,
                    jpeg.len() / 1024,
                    capture_time,
                    convert_time,
                    encode_time,
                    fps
                );
            }

            let msg = CompressedImage {
                header: std_msgs::Header {
                    stamp: ROSTime {
                        sec: ts.seconds() as i32,
                        nanosec: ts.subsec(9),
                    },
                    frame_id: "".to_string(),
                },
                format: "jpeg".to_string(),
                data: jpeg.to_vec(),
            };

            let encoded = cdr::serialize::<_, _, CdrLe>(&msg, Infinite)?;
            session.put(&args.topic, encoded).res().await.unwrap();
        },
        StreamType::H264 => {
            let mut vid = VideoManager::new(FourCC(*b"H264"), cam.width(), cam.height());
            loop {
                let fps = update_fps(&mut prev, &mut history, &mut index);
                let mut now = Instant::now();
                let buf = cam.read()?;
                let ts = buf.timestamp();
                let capture_time = now.elapsed();
                now = Instant::now();
                let data = match vid.encode_and_save(&buf) {
                    Ok(d) => d.0,
                    Err(e) => {
                        eprintln!("{e:?}");
                        continue;
                    }
                };
                let encode_time = now.elapsed();
                if args.verbose {
                    println!(
                        "camera {}x{} size: {}KB video_frame: {}KB capture: {:?} encode: {:?} fps: {}",
                        cam.width(),
                        cam.height(),
                        cam.width() * cam.height() * 4 / 1024,
                        data.len() / 1024,
                        capture_time,
                        encode_time,
                        fps
                    );
                }
                let msg = FoxgloveCompressedVideo {
                    header: std_msgs::Header {
                        stamp: ROSTime {
                            sec: ts.seconds() as i32,
                            nanosec: ts.subsec(9),
                        },
                        frame_id: "".to_string(),
                    },
                    format: "h264".to_string(),
                    data,
                };
                let encoded = cdr::serialize::<_, _, CdrLe>(&msg, Infinite)?;
                session.put(&args.topic, encoded).res().await.unwrap();
            }
        }
    }
}

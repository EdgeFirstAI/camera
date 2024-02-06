use camera::image::{encode_jpeg, Image, ImageManager, RGBA};
use cdr::{CdrLe, Infinite};
use clap::Parser;
use std::{
    error::Error,
    fs::File,
    os::fd::AsRawFd,
    path::PathBuf,
    process,
    str::FromStr,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use video::VideoManager;
use videostream::{
    camera::{create_camera, CameraBuffer, CameraReader, Mirror},
    fourcc::FourCC,
};
use zenoh::{config::Config, prelude::r#async::*};
use zenoh_ros_type::{
    deepview_msgs::DeepviewDMABuf,
    foxglove_msgs::FoxgloveCompressedVideo,
    rcl_interfaces::builtin_interfaces::Time as ROSTime,
    sensor_msgs::{CameraInfo, CompressedImage, RegionOfInterest},
    std_msgs,
};
mod video;

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
enum StreamType {
    Jpeg,
    H264,
    RawOnly,
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

    /// image ros topic
    #[arg(short, long, default_value = "rt/camera/compressed")]
    image_topic: String,

    /// raw dma topic
    #[arg(short, long, default_value = "rt/camera/raw")]
    dma_topic: String,

    /// camera_info topic
    #[arg(short, long, default_value = "rt/camera/camera_info")]
    info_topic: String,

    /// verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// stream type
    #[arg(long, default_value = "jpeg", value_enum)]
    codec: StreamType,

    /// isp-imx data location
    #[arg(
        long,
        default_value = "/usr/share/imx8-isp/dewarp_config/sensor_dwe_os08a20_1080P_config.json"
    )]
    cam_info_path: PathBuf,
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

    // match args.codec {
    //     StreamType::Jpeg => stream_jpeg(cam, session, args).await,
    //     StreamType::H264 => stream_h264(cam, session, args).await,
    //     StreamType::RawOnly => stream_dma(cam, session, args).await,
    // }
    stream(cam, session, args).await
}

async fn stream(cam: CameraReader, session: Session, args: Args) -> Result<(), Box<dyn Error>> {
    let mut img = None;
    let mut imgmgr = None;
    let mut vid = None;
    let stream_type = args.codec.clone();
    match stream_type {
        StreamType::Jpeg => {
            img = Some(Image::new(cam.width(), cam.height(), RGBA)?);
            imgmgr = Some(ImageManager::new()?);
        }
        StreamType::H264 => {
            vid = Some(VideoManager::new(
                FourCC(*b"H264"),
                cam.width(),
                cam.height(),
            ));
        }
        StreamType::RawOnly => {}
    }
    let src_pid = process::id();

    let mut prev = Instant::now();
    let mut history = vec![0; 30];
    let mut index = 0;
    loop {
        let fps = update_fps(&mut prev, &mut history, &mut index);
        let now = Instant::now();
        let buf = cam.read()?;
        let capture_time = now.elapsed();

        if args.verbose {
            println!("camera capture: {:?} fps: {}", capture_time, fps);
        }

        let dma_msg = build_dma_msg(&buf, src_pid, &args);
        match dma_msg {
            Ok(m) => {
                let encoded = cdr::serialize::<_, _, CdrLe>(&m, Infinite)?;
                session.put(&args.dma_topic, encoded).res().await.unwrap();
            }
            Err(e) => eprintln!("{e:?}"),
        }

        let info_msg = build_info_msg(&cam, &args);
        match info_msg {
            Ok(m) => {
                let encoded = cdr::serialize::<_, _, CdrLe>(&m, Infinite)?;
                session.put(&args.info_topic, encoded).res().await.unwrap();
            }
            Err(e) => eprintln!("{e:?}"),
        }
        match stream_type {
            StreamType::Jpeg => {
                let imgmgr = imgmgr.as_ref().unwrap();
                let img = img.as_ref().unwrap();
                let msg = build_jpeg_msg(&buf, &imgmgr, &img, &args);
                match msg {
                    Ok(m) => {
                        let encoded = cdr::serialize::<_, _, CdrLe>(&m, Infinite)?;
                        session.put(&args.image_topic, encoded).res().await.unwrap();
                    }
                    Err(e) => eprintln!("{e:?}"),
                }
            }
            StreamType::H264 => {
                let vid = vid.as_ref().unwrap();
                let msg = build_video_msg(&buf, &vid, &args);
                match msg {
                    Ok(m) => {
                        let encoded = cdr::serialize::<_, _, CdrLe>(&m, Infinite)?;
                        session.put(&args.image_topic, encoded).res().await.unwrap();
                    }
                    Err(e) => eprintln!("{e:?}"),
                }
            }
            StreamType::RawOnly => {}
        }
    }
}

async fn stream_jpeg(
    cam: CameraReader,
    session: Session,
    args: Args,
) -> Result<(), Box<dyn Error>> {
    let img = Image::new(cam.width(), cam.height(), RGBA)?;
    let imgmgr = ImageManager::new()?;
    let mut prev = Instant::now();
    let mut history = vec![0; 30];
    let mut index = 0;
    loop {
        let fps = update_fps(&mut prev, &mut history, &mut index);
        let now = Instant::now();
        let buf = cam.read()?;
        let capture_time = now.elapsed();

        if args.verbose {
            println!("camera capture: {:?} fps: {}", capture_time, fps);
        }

        let msg = build_jpeg_msg(&buf, &imgmgr, &img, &args);
        match msg {
            Ok(m) => {
                let encoded = cdr::serialize::<_, _, CdrLe>(&m, Infinite)?;
                session.put(&args.image_topic, encoded).res().await.unwrap();
            }
            Err(e) => eprintln!("{e:?}"),
        }
    }
}

async fn stream_h264(
    cam: CameraReader,
    session: Session,
    args: Args,
) -> Result<(), Box<dyn Error>> {
    let vid = VideoManager::new(FourCC(*b"H264"), cam.width(), cam.height());
    let mut prev = Instant::now();
    let mut history = vec![0; 30];
    let mut index = 0;
    loop {
        let fps = update_fps(&mut prev, &mut history, &mut index);
        let now = Instant::now();
        let buf = cam.read()?;

        let capture_time = now.elapsed();

        if args.verbose {
            println!("camera capture: {:?} fps: {}", capture_time, fps);
        }
        let msg = build_video_msg(&buf, &vid, &args);
        match msg {
            Ok(m) => {
                let encoded = cdr::serialize::<_, _, CdrLe>(&m, Infinite)?;
                session.put(&args.image_topic, encoded).res().await.unwrap();
            }
            Err(e) => eprintln!("{e:?}"),
        }
    }
}

async fn stream_dma(cam: CameraReader, session: Session, args: Args) -> Result<(), Box<dyn Error>> {
    let mut prev = Instant::now();
    let mut history = vec![0; 30];
    let mut index = 0;
    let src_pid = process::id();
    loop {
        let fps = update_fps(&mut prev, &mut history, &mut index);
        let now = Instant::now();
        let buf = cam.read()?;
        let capture_time = now.elapsed();

        if args.verbose {
            println!("camera capture: {:?} fps: {}", capture_time, fps);
        }
        let dma_msg = build_dma_msg(&buf, src_pid, &args);
        match dma_msg {
            Ok(m) => {
                let encoded = cdr::serialize::<_, _, CdrLe>(&m, Infinite)?;
                session.put(&args.dma_topic, encoded).res().await.unwrap();
            }
            Err(e) => eprintln!("{e:?}"),
        }
    }
}

fn build_jpeg_msg(
    buf: &CameraBuffer<'_>,
    imgmgr: &ImageManager,
    img: &Image,
    args: &Args,
) -> Result<CompressedImage, Box<dyn Error>> {
    let now = Instant::now();
    let ts = buf.timestamp();
    imgmgr.convert(&Image::from_camera(buf)?, img, None)?;
    let convert_time = now.elapsed();

    let now = Instant::now();
    let dma = img.dmabuf();
    let mem = dma.memory_map()?;
    let jpeg = mem.read(encode_jpeg, Some(img))?;
    let encode_time = now.elapsed();

    if args.verbose {
        println!(
            "camera {}x{} image {}x{} size: {}KB jpeg: {}KB convert: {:?} encode: {:?}",
            buf.width(),
            buf.height(),
            img.width(),
            img.height(),
            img.width() * img.height() * 4 / 1024,
            jpeg.len() / 1024,
            convert_time,
            encode_time,
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
    return Ok(msg);
}

fn build_video_msg(
    buf: &CameraBuffer<'_>,
    vid: &VideoManager,
    args: &Args,
) -> Result<FoxgloveCompressedVideo, Box<dyn Error>> {
    let now = Instant::now();
    let ts = buf.timestamp();
    let data = match vid.encode(&buf) {
        Ok(d) => d.0,
        Err(e) => {
            return Err(e);
        }
    };
    let encode_time = now.elapsed();
    if args.verbose {
        println!(
            "video h.264 {}x{} size: {}KB video_frame: {}KB encode: {:?} ",
            buf.width(),
            buf.height(),
            buf.width() * buf.height() * 4 / 1024,
            data.len() / 1024,
            encode_time,
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
    return Ok(msg);
}

fn build_dma_msg(
    buf: &CameraBuffer<'_>,
    src_pid: u32,
    args: &Args,
) -> Result<DeepviewDMABuf, Box<dyn Error>> {
    let _ = args;

    let ts = buf.timestamp();
    let width = buf.width() as u32;
    let height = buf.height() as u32;
    let fourcc = buf.format().into();
    let dma_buf = buf.fd().as_raw_fd();
    let msg = DeepviewDMABuf {
        header: std_msgs::Header {
            stamp: ROSTime {
                sec: ts.seconds() as i32,
                nanosec: ts.subsec(9),
            },
            frame_id: "".to_string(),
        },
        src_pid,
        dma_fd: dma_buf,
        width,
        height,
        stride: width,
        fourcc,
    };
    return Ok(msg);
}

fn build_info_msg(cam: &CameraReader, args: &Args) -> Result<CameraInfo, Box<dyn Error>> {
    let file = File::open(args.cam_info_path.clone()).expect("file should open read only");
    let json: serde_json::Value =
        serde_json::from_reader(file).expect("file should be proper JSON");
    let dewarp_configs = &json["dewarpConfigArray"];
    if !dewarp_configs.is_array() {
        return Err(Box::from("Did not find dewarpConfigArray as an array"));
    }
    let dewarp_config = &dewarp_configs[0];
    let distortion_coeff = dewarp_config["distortion_coeff"].as_array();
    let d: Vec<f64>;
    match distortion_coeff {
        Some(v) => {
            d = v.into_iter().map(|x| x.as_f64().unwrap_or(0.0)).collect();
        }
        None => {
            return Err(Box::from("Did not find distortion_coeff as an array"));
        }
    };

    let camera_matrix = dewarp_config["camera_matrix"].as_array();
    let k: Vec<f64>;
    match camera_matrix {
        Some(v) => {
            k = v.into_iter().map(|x| x.as_f64().unwrap_or(0.0)).collect();
        }
        None => {
            return Err(Box::from("Did not find camera_matrix as an array"));
        }
    }
    if k.len() != 9 {
        return Err(Box::from(format!(
            "Expected exactly 9 elements in distortion_coeff array but found {}",
            d.len()
        )));
    }
    let p = [
        k[0], k[1], k[2], 0.0, k[3], k[4], k[5], 0.0, k[6], k[7], k[8], 0.0,
    ];
    // TODO: Is there an easier way to do this conversion?
    let k = [k[0], k[1], k[2], k[3], k[4], k[5], k[6], k[7], k[8]];

    let width = cam.width() as u32;
    let height = cam.height() as u32;
    let r = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    let now = SystemTime::now();
    let since_the_epoch = now.duration_since(UNIX_EPOCH).expect("Time went backwards");
    let msg = CameraInfo {
        header: std_msgs::Header {
            stamp: ROSTime {
                sec: since_the_epoch.as_secs() as i32,
                nanosec: since_the_epoch.subsec_nanos(),
            },
            frame_id: "".to_string(),
        },
        width,
        height,
        distortion_model: String::from("plumb_bob"),
        d,
        k,
        r,
        p,
        binning_x: 1,
        binning_y: 1,
        roi: RegionOfInterest {
            x_offset: 0,
            y_offset: 0,
            height,
            width,
            do_rectify: false,
        },
    };
    return Ok(msg);
}

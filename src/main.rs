use camera::image::{encode_jpeg, Image, ImageManager, RGBA};
use cdr::{CdrLe, Infinite};
use clap::Parser;
use std::{
    error::Error,
    fs::File,
    path::PathBuf,
    process,
    str::FromStr,
    sync::mpsc::{self, RecvError},
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use unix_ts::Timestamp;
use video::VideoManager;
use videostream::{
    camera::{create_camera, CameraBuffer, CameraReader, Mirror},
    fourcc::FourCC,
};
use zenoh::{
    config::Config,
    prelude::{r#async::*, sync::SyncResolve},
};
use zenoh_ros_type::{
    deepview_msgs::DeepviewDMABuf,
    foxglove_msgs::FoxgloveCompressedVideo,
    rcl_interfaces::builtin_interfaces::Time as ROSTime,
    sensor_msgs::{CameraInfo, CompressedImage, RegionOfInterest},
    std_msgs,
};
mod video;

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
enum MirrorSetting {
    None,
    Horizontal,
    Vertical,
    Both,
}
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// camera capture device
    #[arg(short, long, default_value = "/dev/video3")]
    camera: String,

    /// camera capture resolution
    #[arg(long, default_value = "3840 2160", value_delimiter = ' ', num_args = 2)]
    camera_size: Vec<i32>,

    /// camera mirror
    #[arg(long, default_value = "both", value_enum)]
    mirror: MirrorSetting,

    /// raw dma topic
    #[arg(long, default_value = "rt/camera/raw")]
    dma_topic: String,

    /// camera_info topic
    #[arg(long, default_value = "rt/camera/camera_info")]
    info_topic: String,

    /// zenoh connection mode
    #[arg(short, long, default_value = "peer")]
    mode: String,

    /// connect to endpoint
    #[arg(short, long)]
    endpoint: Vec<String>,

    /// stream JPEGs
    #[arg(long)]
    jpeg: bool,

    /// jpeg ros topic
    #[arg(long, default_value = "rt/camera/image")]
    jpeg_topic: String,

    /// stream H264
    #[arg(long)]
    h264: bool,

    /// h264 foxglove topic
    #[arg(long, default_value = "rt/camera/compressed")]
    h264_topic: String,

    /// streaming resolution
    #[arg(
        short,
        long,
        default_value = "1920 1080",
        value_delimiter = ' ',
        num_args = 2
    )]
    stream_size: Vec<i32>,

    /// verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// isp-imx data location
    #[arg(
        long,
        default_value = "/usr/share/imx8-isp/dewarp_config/sensor_dwe_os08a20_4K_config.json"
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
    println!("Maivin Camera Publisher");

    let args = Args::parse();
    let mut config = Config::default();

    let mode = WhatAmI::from_str(&args.mode).unwrap();
    config.set_mode(Some(mode)).unwrap();
    config.connect.endpoints = args.endpoint.iter().map(|v| v.parse().unwrap()).collect();

    let session = zenoh::open(config).res_async().await.unwrap();

    let mirror = match args.mirror {
        MirrorSetting::None => Mirror::None,
        MirrorSetting::Horizontal => Mirror::Horizontal,
        MirrorSetting::Vertical => Mirror::Vertical,
        MirrorSetting::Both => Mirror::Both,
    };

    let cam = create_camera()
        .with_device(&args.camera)
        .with_resolution(args.camera_size[0], args.camera_size[1])
        .with_format(FourCC(*b"YUYV"))
        .with_mirror(mirror)
        .open()?;
    cam.start()?;

    if cam.width() != args.camera_size[0] || cam.height() != args.camera_size[1] {
        eprintln!(
            "WARNING: User requested {} {} resolution but camera set {} {} resolution",
            args.camera_size[0],
            args.camera_size[1],
            cam.width(),
            cam.height()
        );
    }

    stream(cam, session, args).await
}

async fn stream(cam: CameraReader, session: Session, args: Args) -> Result<(), Box<dyn Error>> {
    let imgmgr = ImageManager::new()?;

    let mut img_h264 = None;
    let mut vidmgr = None;
    let (tx, rx) = mpsc::channel();
    if args.h264 {
        img_h264 = Some(Image::new(args.stream_size[0], args.stream_size[1], RGBA)?);
        vidmgr = Some(VideoManager::new(
            FourCC(*b"H264"),
            args.stream_size[0],
            args.stream_size[1],
        ));
    }
    if args.jpeg {
        // JPEG encoding will live in a thread since it's possible to for it to be
        // significantly slower than the camera's frame rate
        let args = args.clone();

        let jpeg_func = move || {
            let imgmgr = ImageManager::new().unwrap();
            let img_jpeg = Image::new(args.stream_size[0], args.stream_size[1], RGBA).unwrap();
            let mut config = Config::default();

            let mode = WhatAmI::from_str(&args.mode).unwrap();
            config.set_mode(Some(mode)).unwrap();
            config.connect.endpoints = args.endpoint.iter().map(|v| v.parse().unwrap()).collect();

            let session = zenoh::open(config.clone()).res_sync().unwrap();
            loop {
                while rx.try_recv().is_ok() {}
                let (msg, ts) = match rx.recv() {
                    Ok(v) => v,
                    Err(RecvError) => {
                        // main thread exited
                        return;
                    }
                };
                let msg = build_jpeg_msg(&msg, &ts, &imgmgr, &img_jpeg, &args);
                match msg {
                    Ok(m) => {
                        let encoded = cdr::serialize::<_, _, CdrLe>(&m, Infinite).unwrap();
                        session.put(&args.jpeg_topic, encoded).res_sync().unwrap();
                    }
                    Err(e) => eprintln!("{e:?}"),
                }
            }
        };
        thread::spawn(jpeg_func);
    }
    // TODO: Decide if the H264 encode/decode should also live in a thread
    let info_msg = build_info_msg(&cam, &args);
    let info_msg = match info_msg {
        Ok(m) => Some(cdr::serialize::<_, _, CdrLe>(&m, Infinite)?),
        Err(e) => {
            eprintln!("{e:?}");
            None
        }
    };

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
                session
                    .put(&args.dma_topic, encoded)
                    .res_async()
                    .await
                    .unwrap();
            }
            Err(e) => eprintln!("{e:?}"),
        }

        match info_msg {
            Some(ref msg) => {
                session
                    .put(&args.info_topic, msg.clone())
                    .res_async()
                    .await
                    .unwrap();
            }
            None => {}
        }

        if args.jpeg {
            let ts = buf.timestamp();
            let src_img = Image::from_camera(&buf)?;
            match tx.send((src_img, ts)) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("Jpeg send error: {:?}", e);
                }
            }
        }

        if args.h264 {
            let vid = vidmgr.as_ref().unwrap();
            let img = img_h264.as_ref().unwrap();
            let ts = buf.timestamp();
            let src_img = Image::from_camera(&buf)?;
            let msg = build_video_msg(&src_img, &ts, &vid, &imgmgr, &img, &args);
            match msg {
                Ok(m) => {
                    let encoded = cdr::serialize::<_, _, CdrLe>(&m, Infinite)?;
                    session
                        .put(&args.h264_topic, encoded)
                        .res_async()
                        .await
                        .unwrap();
                }
                Err(e) => eprintln!("{e:?}"),
            }
        }
    }
}

fn build_jpeg_msg(
    buf: &Image,
    ts: &Timestamp,
    imgmgr: &ImageManager,
    img: &Image,
    args: &Args,
) -> Result<CompressedImage, Box<dyn Error>> {
    let now = Instant::now();
    imgmgr.convert(&buf, &img, None)?;
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
    buf: &Image,
    ts: &Timestamp,
    vid: &VideoManager,
    imgmgr: &ImageManager,
    img: &Image,
    args: &Args,
) -> Result<FoxgloveCompressedVideo, Box<dyn Error>> {
    let now = Instant::now();
    let data = match vid.resize_and_encode(&buf, &imgmgr, &img) {
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
    let dma_buf = buf.rawfd();
    // let dma_buf = buf.original_fd;
    let length = buf.length() as u32;
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
        length,
    };
    if args.verbose {
        println!(
            "dmabuf dma_buf: {} src_pid: {} length: {}",
            dma_buf, src_pid, length,
        );
    }
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

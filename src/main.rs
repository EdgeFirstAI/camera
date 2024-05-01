use camera::image::{encode_jpeg, Image, ImageManager, RGBA};
use cdr::{CdrLe, Infinite};
use clap::Parser;
use edgefirst_schemas::{
    builtin_interfaces::Time as ROSTime,
    edgefirst_msgs::DmaBuf,
    foxglove_msgs::FoxgloveCompressedVideo,
    sensor_msgs::{CameraInfo, CompressedImage, RegionOfInterest},
    std_msgs,
};
use log::{error, info, trace, warn};
use std::{
    error::Error,
    fs::File,
    path::PathBuf,
    process,
    str::FromStr,
    sync::mpsc::{self, RecvError},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
    publication::Publisher,
};
mod video;

const TIME_LIMIT: Duration = Duration::from_millis(33); // at most 33ms between current step and previous step

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
    #[arg(short, long, env, default_value = "/dev/video3")]
    camera: String,

    /// camera capture resolution
    #[arg(
        long,
        env,
        default_value = "1920 1080",
        value_delimiter = ' ',
        num_args = 2
    )]
    camera_size: Vec<i32>,

    /// camera mirror
    #[arg(long, env, default_value = "both", value_enum)]
    mirror: MirrorSetting,

    /// raw dma topic
    #[arg(long, default_value = "rt/camera/dma")]
    dma_topic: String,

    /// camera_info topic
    #[arg(long, default_value = "rt/camera/info")]
    info_topic: String,

    /// zenoh connection mode
    #[arg(short, long, default_value = "client")]
    mode: String,

    /// connect to endpoint
    #[arg(short, long, default_value = "tcp/127.0.0.1:7447")]
    endpoints: Vec<String>,

    /// listen to zenoh endpoints
    #[arg(long)]
    listen: Vec<String>,

    /// stream JPEGs
    #[arg(long, env)]
    jpeg: bool,

    /// jpeg ros topic
    #[arg(long, default_value = "rt/camera/jpeg")]
    jpeg_topic: String,

    /// stream H264
    #[arg(long, env)]
    h264: bool,

    /// h264 foxglove topic
    #[arg(long, default_value = "rt/camera/h264")]
    h264_topic: String,

    /// streaming resolution
    #[arg(
        short,
        long,
        env,
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
        default_value = "/usr/share/imx8-isp/dewarp_config/sensor_dwe_os08a20_1080P_config.json"
    )]
    cam_info_path: PathBuf,
}

fn update_fps(prev: &mut Instant, history: &mut [i64], index: &mut usize) -> i64 {
    let now = Instant::now();

    let elapsed = now.duration_since(*prev);
    *prev = Instant::now();

    history[*index] = 1e9 as i64 / elapsed.as_nanos() as i64;
    *index = (*index + 1) % history.len();

    (history.iter().sum::<i64>() as f64 / history.len() as f64).round() as i64
}

#[async_std::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();
    let args = Args::parse();

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
        warn!(
            "User requested {}x{} resolution but camera set {}x{} resolution",
            args.camera_size[0],
            args.camera_size[1],
            cam.width(),
            cam.height()
        );
    }
    info!(
        "Opened camera: {} resolution: {}x{} stream: {}x{} mirror: {}",
        args.camera,
        cam.width(),
        cam.height(),
        args.stream_size[0],
        args.stream_size[1],
        mirror
    );

    let mut config = Config::default();
    let mode = WhatAmI::from_str(&args.mode).unwrap();
    config.set_mode(Some(mode)).unwrap();
    config.connect.endpoints = args.endpoints.iter().map(|v| v.parse().unwrap()).collect();
    config.listen.endpoints = args.listen.iter().map(|v| v.parse().unwrap()).collect();
    let _ = config.scouting.multicast.set_enabled(Some(false));
    let _ = config.scouting.gossip.set_enabled(Some(true));
    let session = zenoh::open(config.clone()).res_async().await.unwrap();
    info!("Opened Zenoh session");
    stream(cam, session, args).await
}

async fn stream(cam: CameraReader, session: Session, args: Args) -> Result<(), Box<dyn Error>> {
    let session = session.into_arc();
    let publ_dma = match session
        .declare_publisher(args.dma_topic.clone())
        .priority(Priority::RealTime)
        .congestion_control(CongestionControl::Drop)
        .res_async()
        .await
    {
        Ok(v) => v,
        Err(e) => {
            error!(
                "Error while declaring DMA publisher {}: {:?}",
                args.dma_topic, e
            );
            return Err(e);
        }
    };

    let publ_info = match session
        .declare_publisher(args.info_topic.clone())
        .priority(Priority::Background)
        .congestion_control(CongestionControl::Drop)
        .res_async()
        .await
    {
        Ok(v) => v,
        Err(e) => {
            error!(
                "Error while declaring camera info publisher {}: {:?}",
                args.info_topic, e
            );
            return Err(e);
        }
    };

    let publ_h264: Option<Publisher<'_>>;

    let imgmgr = ImageManager::new()?;
    let mut img_h264 = None;
    let mut vidmgr = None;

    if args.h264 {
        publ_h264 = match session
            .declare_publisher(args.h264_topic.clone())
            .priority(Priority::Data)
            .congestion_control(CongestionControl::Block)
            .res_async()
            .await
        {
            Ok(v) => Some(v),
            Err(e) => {
                error!(
                    "Error while declaring H264 publisher {}: {:?}",
                    args.h264_topic, e
                );
                return Err(e);
            }
        };
        img_h264 = Some(Image::new(args.stream_size[0], args.stream_size[1], RGBA)?);

        vidmgr = match VideoManager::new(FourCC(*b"H264"), args.stream_size[0], args.stream_size[1])
        {
            Ok(v) => Some(v),
            Err(e) => {
                error!("Could not create Video Manager for H264 encoding: {:?}", e);
                return Err(e);
            }
        }
    } else {
        publ_h264 = None;
    }
    let (tx, rx) = mpsc::channel();
    if args.jpeg {
        // JPEG encoding will live in a thread since it's possible to for it to be
        // significantly slower than the camera's frame rate
        let args = args.clone();
        let publ_jpeg = match session
            .declare_publisher(args.jpeg_topic.clone())
            .priority(Priority::Data)
            .congestion_control(CongestionControl::Block)
            .res_async()
            .await
        {
            Ok(v) => v,
            Err(e) => {
                error!(
                    "Error while declaring JPEG publisher {}: {:?}",
                    args.jpeg_topic, e
                );
                return Err(e);
            }
        };
        let jpeg_func = move || {
            let imgmgr = ImageManager::new().unwrap();
            let img_jpeg = Image::new(args.stream_size[0], args.stream_size[1], RGBA).unwrap();

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
                        let encoded =
                            Value::from(cdr::serialize::<_, _, CdrLe>(&m, Infinite).unwrap())
                                .encoding(Encoding::WithSuffix(
                                    KnownEncoding::AppOctetStream,
                                    "sensor_msgs/msg/CompressedImage".into(),
                                ));
                        trace!("Encoded JPEG message to CDR");
                        let now = Instant::now();
                        publ_jpeg.put(encoded).res_sync().unwrap();
                        let elapsed = now.elapsed().as_secs_f64() * 1000.0;
                        if elapsed > 33.3 {
                            warn!(
                                "Send to JPEG topic {:?} ({elapsed:.2} ms)",
                                publ_jpeg.key_expr()
                            );
                        } else {
                            trace!(
                                "Send to JPEG topic {:?} ({elapsed:.2} ms)",
                                publ_jpeg.key_expr()
                            );
                        }
                    }
                    Err(e) => error!("Error when building JPEG message {e:?}"),
                }
            }
        };
        thread::spawn(jpeg_func);
    }
    // TODO: Decide if the H264 encode/decode should also live in a thread
    let info_msg = build_info_msg(&cam, &args);
    let info_msg = match info_msg {
        Ok(m) => Some(
            Value::from(cdr::serialize::<_, _, CdrLe>(&m, Infinite)?).encoding(
                Encoding::WithSuffix(
                    KnownEncoding::AppOctetStream,
                    "sensor_msgs/msg/CameraInfo".into(),
                ),
            ),
        ),
        Err(e) => {
            error!("Error when building camera info message: {e:?}");
            None
        }
    };

    let src_pid = process::id();

    let mut prev = Instant::now();
    let mut history = vec![0; 30];
    let mut index = 0;

    loop {
        let fps = update_fps(&mut prev, &mut history, &mut index);
        if fps < 29 {
            warn!("camera FPS is {}", fps);
        }
        let now = Instant::now();
        let buf = cam.read()?;
        let capture_time = now.elapsed();
        let now = Instant::now();
        trace!("camera capture: {:?} fps: {}", capture_time, fps);
        if capture_time > TIME_LIMIT {
            warn!(
                "camera capture: {:?} exceeds {:?}",
                capture_time, TIME_LIMIT
            );
        } else {
            trace!("camera capture: {:?}", capture_time);
        }
        let dma_msg = build_dma_msg(&buf, src_pid, &args);
        match dma_msg {
            Ok(m) => {
                let encoded = Value::from(cdr::serialize::<_, _, CdrLe>(&m, Infinite)?).encoding(
                    Encoding::WithSuffix(
                        KnownEncoding::AppOctetStream,
                        "edgefirst_msgs/msg/DmaBuffer".into(),
                    ),
                );
                trace!("Encoded DMA message to CDR");
                publ_dma.put(encoded).res_async().await.unwrap();
                trace!("Send to DMA topic {:?}", args.dma_topic);
            }
            Err(e) => error!("Error when building DMA message: {e:?}"),
        }
        let dma_msg_time = now.elapsed();
        let now = Instant::now();
        if dma_msg_time > TIME_LIMIT {
            warn!("dma msg time: {:?} exceeds {:?}", dma_msg_time, TIME_LIMIT);
        } else {
            trace!("dma msg time: {:?}", dma_msg_time);
        }

        if let Some(ref msg) = info_msg {
            publ_info.put(msg.clone()).res_async().await.unwrap();
            trace!("Send to info topic {:?}", args.info_topic);
        }
        let dma_zenoh_time = now.elapsed();
        // let now = Instant::now();
        if dma_zenoh_time > TIME_LIMIT {
            warn!(
                "dma zenoh time: {:?} exceeds {:?}",
                dma_zenoh_time, TIME_LIMIT
            );
        } else {
            trace!("dma zenoh time: {:?}", dma_zenoh_time)
        }
        if args.jpeg {
            let ts = buf.timestamp();
            let src_img = Image::from_camera(&buf)?;
            match tx.send((src_img, ts)) {
                Ok(_) => {}
                Err(e) => {
                    error!("JPEG thread messaging error: {:?}", e);
                }
            }
        }

        if args.h264 {
            trace!("Start h264");
            let vid = vidmgr.as_ref().unwrap();
            let img = img_h264.as_ref().unwrap();
            let ts = buf.timestamp();
            let src_img = Image::from_camera(&buf)?;
            let msg = build_video_msg(&src_img, &ts, vid, &imgmgr, img, &args);
            let now = Instant::now();
            match msg {
                Ok(m) => {
                    let encoded = Value::from(cdr::serialize::<_, _, CdrLe>(&m, Infinite)?)
                        .encoding(Encoding::WithSuffix(
                            KnownEncoding::AppOctetStream,
                            "foxglove_msgs/msg/CompressedVideo".into(),
                        ));
                    if let Some(publ) = publ_h264.as_ref() {
                        publ.put(encoded).res_async().await.unwrap();
                    }
                    trace!("Send to H264 topic {:?}", args.h264_topic);
                }
                Err(e) => error!("Error when building video message: {e:?}"),
            }
            let h264_zenoh_time = now.elapsed();
            if h264_zenoh_time > TIME_LIMIT {
                warn!(
                    "h264 zenoh time: {:?} exceeds {:?}",
                    h264_zenoh_time, TIME_LIMIT
                );
            } else {
                trace!("h264 zenoh time: {:?}", h264_zenoh_time)
            }
        }
    }
}

fn build_jpeg_msg(
    buf: &Image,
    ts: &Timestamp,
    imgmgr: &ImageManager,
    img: &Image,
    _: &Args,
) -> Result<CompressedImage, Box<dyn Error>> {
    let now = Instant::now();
    imgmgr.convert(buf, img, None)?;
    let convert_time = now.elapsed();

    let now = Instant::now();
    let dma = img.dmabuf();
    let mem = dma.memory_map()?;
    let jpeg = mem.read(encode_jpeg, Some(img))?;
    let encode_time = now.elapsed();

    trace!(
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
    Ok(msg)
}

fn build_video_msg(
    buf: &Image,
    ts: &Timestamp,
    vid: &VideoManager,
    imgmgr: &ImageManager,
    img: &Image,
    _: &Args,
) -> Result<FoxgloveCompressedVideo, Box<dyn Error>> {
    let now = Instant::now();
    let data = match vid.resize_and_encode(buf, imgmgr, img) {
        Ok(d) => d.0,
        Err(e) => {
            return Err(e);
        }
    };
    let encode_time = now.elapsed();
    trace!(
        "video h.264 {}x{} size: {}KB video_frame: {}KB encode: {:?} ",
        buf.width(),
        buf.height(),
        buf.width() * buf.height() * 4 / 1024,
        data.len() / 1024,
        encode_time,
    );

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
    Ok(msg)
}

fn build_dma_msg(buf: &CameraBuffer<'_>, pid: u32, args: &Args) -> Result<DmaBuf, Box<dyn Error>> {
    let _ = args;

    let ts = buf.timestamp();
    let width = buf.width() as u32;
    let height = buf.height() as u32;
    let fourcc = buf.format().into();
    let dma_buf = buf.rawfd();
    // let dma_buf = buf.original_fd;
    let length = buf.length() as u32;
    let msg = DmaBuf {
        header: std_msgs::Header {
            stamp: ROSTime {
                sec: ts.seconds() as i32,
                nanosec: ts.subsec(9),
            },
            frame_id: "".to_string(),
        },
        pid,
        fd: dma_buf,
        width,
        height,
        stride: width,
        fourcc,
        length,
    };
    trace!(
        "dmabuf dma_buf: {} pid: {} length: {}",
        dma_buf,
        pid,
        length,
    );
    Ok(msg)
}

fn build_info_msg(cam: &CameraReader, args: &Args) -> Result<CameraInfo, Box<dyn Error>> {
    let file = match File::open(args.cam_info_path.clone()) {
        Ok(v) => v,
        Err(e) => {
            return Err(Box::from(format!(
                "Cannot open file {:?}: {e:?}",
                &args.cam_info_path
            )));
        }
    };
    let json: serde_json::Value =
        serde_json::from_reader(file).expect("file should be proper JSON");
    let dewarp_configs = &json["dewarpConfigArray"];
    if !dewarp_configs.is_array() {
        return Err(Box::from("Did not find dewarpConfigArray as an array"));
    }
    let dewarp_config = &dewarp_configs[0];
    let distortion_coeff = dewarp_config["distortion_coeff"].as_array();
    let d: Vec<f64> = match distortion_coeff {
        Some(v) => v.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect(),
        None => return Err(Box::from("Did not find distortion_coeff as an array")),
    };

    let camera_matrix = dewarp_config["camera_matrix"].as_array();
    let k: Vec<f64> = match camera_matrix {
        Some(v) => v.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect(),
        None => return Err(Box::from("Did not find camera_matrix as an array")),
    };
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
    Ok(msg)
}

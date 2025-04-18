mod args;
mod video;

use args::{Args, MirrorSetting};
use camera::image::{encode_jpeg, Image, ImageManager, RGBA};
use cdr::{CdrLe, Infinite};
use clap::Parser;
use edgefirst_schemas::{
    builtin_interfaces::{self, Time},
    edgefirst_msgs::DmaBuf,
    foxglove_msgs::FoxgloveCompressedVideo,
    geometry_msgs::{Quaternion, Transform, TransformStamped, Vector3},
    sensor_msgs::{CameraInfo, CompressedImage, RegionOfInterest},
    std_msgs::{self, Header},
};
use kanal::Receiver;
use std::{
    env,
    error::Error,
    fs::File,
    process,
    thread::{self},
    time::{Duration, Instant},
};
use tracing::{error, info, info_span, instrument, warn, Instrument};
use tracing_subscriber::{layer::SubscriberExt as _, Layer as _, Registry};
use tracy_client::{frame_mark, plot, secondary_frame_mark};
use unix_ts::Timestamp;
use video::VideoManager;
use videostream::{
    camera::{create_camera, CameraBuffer, CameraReader, Mirror},
    fourcc::FourCC,
};
use zenoh::{
    bytes::{Encoding, ZBytes},
    qos::{CongestionControl, Priority},
    Session,
};

#[cfg(feature = "profiling")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(std::alloc::System, 100);

// TODO: Add setting target FPS
const TARGET_FPS: i32 = 30;

fn update_fps(prev: &mut Instant, history: &mut [f64], index: &mut usize) -> f64 {
    let now = Instant::now();

    let elapsed = now.duration_since(*prev);
    *prev = now;

    history[*index] = elapsed.as_nanos() as f64;
    *index = (*index + 1) % history.len();

    let avg = history.iter().sum::<f64>() / history.len() as f64;

    1e9 / avg
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    args.tracy.then(tracy_client::Client::start);

    let stdout_log = tracing_subscriber::fmt::layer()
        .pretty()
        .with_filter(args.rust_log);

    let journald = match tracing_journald::layer() {
        Ok(journald) => Some(journald.with_filter(args.rust_log)),
        Err(_) => None,
    };

    let (console, console_server) = match args.tokio_console {
        true => {
            match env::var("TOKIO_CONSOLE_BIND") {
                Ok(_) => {}
                Err(_) => env::set_var("TOKIO_CONSOLE_BIND", "localhost:7000"),
            };
            let (console, console_server) = console_subscriber::ConsoleLayer::builder()
                .with_default_env()
                .build();
            (Some(console), Some(console_server))
        }
        false => (None, None),
    };

    let tracy = match args.tracy {
        true => Some(tracing_tracy::TracyLayer::default().with_filter(args.rust_log)),
        false => None,
    };

    let subscriber = Registry::default()
        .with(stdout_log)
        .with(journald)
        .with(console)
        .with(tracy);
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
    tracing_log::LogTracer::init()?;

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

    let session = zenoh::open(args.clone()).await.unwrap();
    let stream_task = stream(cam, session, args);

    if let Some(console_server) = console_server {
        let console_task = console_server.serve();
        let (console_task, stream_task) = tokio::join!(console_task, stream_task);
        console_task.unwrap();
        stream_task.unwrap();
    } else {
        stream_task.await.unwrap();
    }

    Ok(())
}

async fn stream(cam: CameraReader, session: Session, args: Args) -> Result<(), Box<dyn Error>> {
    let publ_info = match session
        .declare_publisher(args.info_topic.clone())
        .priority(Priority::Background)
        .congestion_control(CongestionControl::Drop)
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

    let (h264_tx, rx) = kanal::bounded(1);
    if args.h264 {
        let session = session.clone();
        let args = args.clone();
        thread::Builder::new()
            .name("h264".to_string())
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(h264_task(session, args, rx));
            })?;
    }

    let (jpeg_tx, rx) = kanal::bounded(1);
    if args.jpeg {
        let session = session.clone();
        let args = args.clone();
        thread::Builder::new()
            .name("jpeg".to_string())
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(jpeg_task(session, args, rx));
            })?;
    }

    let tf_session = session.clone();
    let tf_msg = build_tf_msg(&args);
    let tf_msg = ZBytes::from(cdr::serialize::<_, _, CdrLe>(&tf_msg, Infinite).unwrap());
    let tf_enc = Encoding::APPLICATION_CDR.with_schema("geometry_msgs/msg/TransformStamped");
    let tf_task = tokio::task::Builder::new()
        .name("tf_static")
        .spawn(async move { tf_static(tf_session, tf_msg, tf_enc).await })?;
    std::mem::drop(tf_task);

    let info_msg = build_info_msg(&args)?;
    let info_msg = ZBytes::from(cdr::serialize::<_, _, CdrLe>(&info_msg, Infinite)?);
    let info_enc = Encoding::APPLICATION_CDR.with_schema("sensor_msgs/msg/CameraInfo");

    let src_pid = process::id();

    let mut prev = Instant::now();
    let mut history = vec![0.0; 60];
    let mut index = 0;

    loop {
        let camera_buffer = info_span!("camera_read").in_scope(|| cam.read())?;

        let fps = update_fps(&mut prev, &mut history, &mut index);
        if fps < TARGET_FPS as f64 * 0.9 {
            warn!("low camera fps {} (target {})", fps, TARGET_FPS);
        }
        args.tracy.then(|| plot!("fps", fps));

        let (msg, enc) =
            camera_dma_serialize(&camera_buffer, src_pid, args.camera_frame_id.clone())?;
        let span = info_span!("camera_publish");
        let local_session = session.clone();
        let dma_topic = args.dma_topic.clone();
        let dma_task = async move {
            local_session
                .put(dma_topic, msg)
                .encoding(enc)
                .priority(Priority::Data)
                .congestion_control(CongestionControl::Drop)
                .await
                .unwrap();
        }
        .instrument(span);
        let info_task = publ_info.put(info_msg.clone()).encoding(info_enc.clone());

        if args.h264 {
            let ts = camera_buffer.timestamp();
            let src_img = Image::from_camera(&camera_buffer)?;

            match h264_tx.try_send((src_img, ts)) {
                Ok(_) => {}
                Err(e) => {
                    error!("H264 thread messaging error: {:?}", e);
                }
            }
        }

        if args.jpeg {
            let ts = camera_buffer.timestamp();
            let src_img = Image::from_camera(&camera_buffer)?;

            match jpeg_tx.try_send((src_img, ts)) {
                Ok(_) => {}
                Err(e) => {
                    error!("JPEG thread messaging error: {:?}", e);
                }
            }
        }

        let (_dma_task, info_task) = tokio::join!(dma_task, info_task);
        info_task.unwrap();

        args.tracy.then(frame_mark);
    }
}

async fn tf_static(
    session: Session,
    msg: ZBytes,
    enc: Encoding,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let topic = "rt/tf_static".to_string();
    let mut interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        interval.tick().await;
        session
            .put(&topic, msg.clone())
            .encoding(enc.clone())
            .await?;
    }
}

async fn h264_task(session: Session, args: Args, rx: Receiver<(Image, Timestamp)>) {
    let publisher = match session
        .declare_publisher(args.h264_topic.clone())
        .priority(Priority::Data)
        .congestion_control(CongestionControl::Drop)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            error!(
                "Error while declaring H264 publisher {}: {:?}",
                args.h264_topic, e
            );
            return;
        }
    };

    let imgmgr = ImageManager::new().unwrap();
    info!("Opened G2D with version {}", imgmgr.version());

    let img_h264 = Image::new(args.stream_size[0], args.stream_size[1], RGBA).unwrap();
    let mut vidmgr = VideoManager::new(
        FourCC(*b"H264"),
        args.stream_size[0],
        args.stream_size[1],
        args.h264_bitrate,
    )
    .unwrap();

    loop {
        let (msg, ts) = match rx.recv() {
            Ok(v) => v,
            Err(_) => {
                // main thread exited
                return;
            }
        };

        let span = info_span!("h264");
        async {
            let (msg, enc) =
                build_video_msg(&msg, &ts, &mut vidmgr, &imgmgr, &img_h264, &args).unwrap();
            publisher.put(msg).encoding(enc).await.unwrap();
        }
        .instrument(span)
        .await;
        args.tracy.then(|| secondary_frame_mark!("h264"));
    }
}

async fn jpeg_task(session: Session, args: Args, rx: Receiver<(Image, Timestamp)>) {
    let publisher = match session
        .declare_publisher(args.jpeg_topic.clone())
        .priority(Priority::Data)
        .congestion_control(CongestionControl::Drop)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            error!(
                "Error while declaring JPEG publisher {}: {:?}",
                args.jpeg_topic, e
            );
            return;
        }
    };

    let imgmgr = ImageManager::new().unwrap();
    let img_jpeg = Image::new(args.stream_size[0], args.stream_size[1], RGBA).unwrap();

    loop {
        let (msg, ts) = match rx.recv() {
            Ok(v) => v,
            Err(_) => {
                // main thread exited
                return;
            }
        };

        let span = info_span!("jpeg");
        async {
            let (msg, enc) = build_jpeg_msg(&msg, &ts, &imgmgr, &img_jpeg, &args).unwrap();
            publisher.put(msg).encoding(enc).await.unwrap();
        }
        .instrument(span)
        .await;
        args.tracy.then(|| secondary_frame_mark!("jpeg"));
    }
}

fn build_jpeg_msg(
    buf: &Image,
    ts: &Timestamp,
    imgmgr: &ImageManager,
    img: &Image,
    args: &Args,
) -> Result<(ZBytes, Encoding), Box<dyn Error>> {
    info_span!("jpeg_convert").in_scope(|| imgmgr.convert(buf, img, None))?;

    let jpeg = info_span!("jpeg_encode").in_scope(|| {
        let dma = img.dmabuf();
        let buf = dma.memory_map()?.read(encode_jpeg, Some(img))?;
        Ok::<_, Box<dyn Error>>(buf)
    })?;

    args.tracy
        .then(|| plot!("jpeg_kb", (jpeg.len() / 1024) as f64));

    info_span!("jpeg_publish").in_scope(|| {
        let msg = CompressedImage {
            header: std_msgs::Header {
                stamp: builtin_interfaces::Time {
                    sec: ts.seconds() as i32,
                    nanosec: ts.subsec(9),
                },
                frame_id: args.camera_frame_id.clone(),
            },
            format: "jpeg".to_string(),
            data: jpeg.to_vec(),
        };

        let msg = ZBytes::from(cdr::serialize::<_, _, CdrLe>(&msg, Infinite).unwrap());
        let enc = Encoding::APPLICATION_CDR.with_schema("sensor_msgs/msg/CompressedImage");

        Ok((msg, enc))
    })
}

fn build_video_msg(
    buf: &Image,
    ts: &Timestamp,
    vid: &mut VideoManager,
    imgmgr: &ImageManager,
    img: &Image,
    args: &Args,
) -> Result<(ZBytes, Encoding), Box<dyn Error>> {
    let data = vid.resize_and_encode(buf, imgmgr, img)?.0;
    info_span!("h264_publish").in_scope(|| {
        let msg = FoxgloveCompressedVideo {
            header: std_msgs::Header {
                stamp: builtin_interfaces::Time {
                    sec: ts.seconds() as i32,
                    nanosec: ts.subsec(9),
                },
                frame_id: args.camera_frame_id.clone(),
            },
            format: "h264".to_string(),
            data,
        };

        let msg = ZBytes::from(cdr::serialize::<_, _, CdrLe>(&msg, Infinite).unwrap());
        let enc = Encoding::APPLICATION_CDR.with_schema("foxglove_msgs/msg/CompressedVideo");

        Ok((msg, enc))
    })
}

#[instrument(skip_all, fields(width = buf.width(), height = buf.height(), format = buf.format().to_string()))]
fn camera_dma_serialize(
    buf: &CameraBuffer<'_>,
    pid: u32,
    frame_id: String,
) -> Result<(ZBytes, Encoding), cdr::Error> {
    let ts = buf.timestamp();
    let width = buf.width() as u32;
    let height = buf.height() as u32;
    let fourcc = buf.format().into();
    let dma_buf = buf.rawfd();
    let length = buf.length() as u32;

    let msg = DmaBuf {
        header: std_msgs::Header {
            stamp: builtin_interfaces::Time {
                sec: ts.seconds() as i32,
                nanosec: ts.subsec(9),
            },
            frame_id,
        },
        pid,
        fd: dma_buf,
        width,
        height,
        stride: width, // FIXME: stride is not always equal to width!
        fourcc,
        length,
    };

    let msg = ZBytes::from(cdr::serialize::<_, _, CdrLe>(&msg, Infinite)?);
    let enc = Encoding::APPLICATION_CDR.with_schema("edgefirst_msgs/msg/DmaBuffer");

    Ok((msg, enc))
}

fn build_info_msg(args: &Args) -> Result<CameraInfo, Box<dyn Error>> {
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
    let bypass = json["bypass"].as_bool().unwrap_or(false);
    let dewarp_configs = &json["dewarpConfigArray"];
    if !dewarp_configs.is_array() {
        return Err(Box::from("Did not find dewarpConfigArray as an array"));
    }
    let dewarp_config = &dewarp_configs[0];
    let d = if bypass {
        let distortion_coeff = dewarp_config["distortion_coeff"].as_array();
        match distortion_coeff {
            Some(v) => v.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect(),
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

    let width = dewarp_config["source_image"]["width"]
        .as_f64()
        .unwrap_or_else(|| {
            error!("Could not find camera width in camera info json");
            1920.0
        }) as u32;
    let height = dewarp_config["source_image"]["height"]
        .as_f64()
        .unwrap_or_else(|| {
            error!("Could not find camera height in camera info json");
            1080.0
        }) as u32;
    let r = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

    let msg = CameraInfo {
        header: std_msgs::Header {
            stamp: timestamp()?,
            frame_id: args.camera_frame_id.clone(),
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

fn build_tf_msg(args: &Args) -> TransformStamped {
    TransformStamped {
        header: Header {
            frame_id: args.base_frame_id.clone(),
            stamp: timestamp().unwrap_or(Time { sec: 0, nanosec: 0 }),
        },
        child_frame_id: args.camera_frame_id.clone(),
        transform: Transform {
            translation: Vector3 {
                x: args.cam_tf_vec[0],
                y: args.cam_tf_vec[1],
                z: args.cam_tf_vec[2],
            },
            rotation: Quaternion {
                x: args.cam_tf_quat[0],
                y: args.cam_tf_quat[1],
                z: args.cam_tf_quat[2],
                w: args.cam_tf_quat[3],
            },
        },
    }
}

fn timestamp() -> Result<builtin_interfaces::Time, std::io::Error> {
    let mut tp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let err = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut tp) };
    if err != 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(builtin_interfaces::Time {
        sec: tp.tv_sec as i32,
        nanosec: tp.tv_nsec as u32,
    })
}

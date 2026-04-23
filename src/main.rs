// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

mod args;
mod replay;
mod sidecar;
mod video;

use args::{Args, MirrorSetting};
use clap::Parser;
use edgefirst_camera::image::{encode_jpeg, Image, ImageManager, Rotation, RGBA};
use edgefirst_schemas::{
    builtin_interfaces::{self, Time},
    edgefirst_msgs::{CameraFrame, CameraPlaneView},
    foxglove_msgs::FoxgloveCompressedVideo,
    geometry_msgs::{Quaternion, Transform, TransformStamped, Vector3},
    sensor_msgs::{CameraInfo, CompressedImage, RegionOfInterest},
};
use kanal::{Receiver, Sender};
use sidecar::Sidecar;
use std::{
    env,
    error::Error,
    fs::File,
    process,
    sync::atomic::{AtomicBool, Ordering},
    thread::{self},
    time::{Duration, Instant},
};
use tracing::{error, info, info_span, instrument, level_filters::LevelFilter, warn, Instrument};
use tracing_subscriber::{layer::SubscriberExt as _, EnvFilter, Layer as _, Registry};
use tracy_client::{frame_mark, plot, secondary_frame_mark};
use unix_ts::Timestamp;
use video::VideoManager;
use videostream::{
    camera::{create_camera, CameraBuffer, CameraReader, Mirror},
    colorimetry::{ColorEncoding, ColorRange, ColorSpace, ColorTransfer},
    fourcc::FourCC,
};
use zenoh::{
    bytes::{Encoding, ZBytes},
    qos::{CongestionControl, Priority},
    time::{Timestamp as ZenohTimestamp, NTP64},
    Session,
};

/// Global shutdown flag for graceful termination
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "profiling")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(std::alloc::System, 100);

const TARGET_FPS: i32 = 30;

#[derive(Clone, Copy, Debug)]
enum TilePosition {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl TilePosition {
    fn get_crop_params(&self, source_width: u32, source_height: u32) -> (u32, u32, u32, u32) {
        let source_tile_width = source_width / 2;
        let source_tile_height = source_height / 2;

        match self {
            TilePosition::TopLeft => (0, 0, source_tile_width, source_tile_height),
            TilePosition::TopRight => (source_tile_width, 0, source_tile_width, source_tile_height),
            TilePosition::BottomLeft => {
                (0, source_tile_height, source_tile_width, source_tile_height)
            }
            TilePosition::BottomRight => (
                source_tile_width,
                source_tile_height,
                source_tile_width,
                source_tile_height,
            ),
        }
    }

    fn get_output_dimensions() -> (u32, u32) {
        (1920, 1080)
    }
}

fn update_fps(prev: &mut Instant, history: &mut [f64], index: &mut usize) -> f64 {
    let now = Instant::now();

    let elapsed = now.duration_since(*prev);
    *prev = now;

    history[*index] = elapsed.as_nanos() as f64;
    *index = (*index + 1) % history.len();

    let avg = history.iter().sum::<f64>() / history.len() as f64;

    1e9 / avg
}

fn get_env_filter() -> EnvFilter {
    tracing_subscriber::EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Set up signal handler for graceful shutdown (SIGTERM/SIGINT)
    // This enables profraw coverage file generation when terminated
    tokio::spawn(async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to register SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("Failed to register SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM, initiating graceful shutdown");
            }
            _ = sigint.recv() => {
                info!("Received SIGINT, initiating graceful shutdown");
            }
        }
        SHUTDOWN.store(true, Ordering::SeqCst);
    });

    let mut args = Args::parse();

    // Validate record/replay arg combinations before touching anything.
    validate_record_replay_args(&args)?;

    args.tracy.then(tracy_client::Client::start);

    let stdout_log = tracing_subscriber::fmt::layer()
        .pretty()
        .with_filter(get_env_filter());

    let journald = match tracing_journald::layer() {
        Ok(journald) => Some(journald.with_filter(get_env_filter())),
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
        true => Some(tracing_tracy::TracyLayer::default().with_filter(get_env_filter())),
        false => None,
    };

    let subscriber = Registry::default()
        .with(stdout_log)
        .with(journald)
        .with(console)
        .with(tracy);
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
    tracing_log::LogTracer::init()?;

    let session = zenoh::open(args.clone()).await.unwrap();

    if args.replay.is_some() {
        // Replay mode: source frames from a recorded .h264 file. We do
        // not open the V4L2 camera device in this mode; the decoder's
        // output Frame stands in for CameraBuffer on the publish path.
        let replay_task = replay::run_replay(session, args);
        if let Some(console_server) = console_server {
            let console_task = console_server.serve();
            let (console_task, replay_task) = tokio::join!(console_task, replay_task);
            console_task.unwrap();
            replay_task?;
        } else {
            replay_task.await?;
        }
        return Ok(());
    }

    let mirror = match args.mirror {
        MirrorSetting::None => Mirror::None,
        MirrorSetting::Horizontal => Mirror::Horizontal,
        MirrorSetting::Vertical => Mirror::Vertical,
        MirrorSetting::Both => Mirror::Both,
    };

    let cam = create_camera()
        .with_device(&args.camera)
        .with_resolution(args.camera_size[0] as i32, args.camera_size[1] as i32)
        .with_format(FourCC(*b"YUYV"))
        .with_mirror(mirror)
        .open()?;
    cam.start()?;
    if cam.width() as u32 != args.camera_size[0] || cam.height() as u32 != args.camera_size[1] {
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
    args.camera_size[0] = cam.width() as u32;
    args.camera_size[1] = cam.height() as u32;

    // Automatically enable tiling for resolutions greater than 1080p
    if args.camera_size[1] > 1080 {
        if !args.h264_tiles {
            info!(
                "Camera resolution {}x{} exceeds 1080p, automatically enabling H264 tiling",
                args.camera_size[0], args.camera_size[1]
            );
            args.h264_tiles = true;
        } else {
            info!(
                "H264 tiling already enabled for {}x{} resolution",
                args.camera_size[0], args.camera_size[1]
            );
        }
    } else if args.h264_tiles {
        info!(
            "H264 tiling manually enabled for {}x{} resolution",
            args.camera_size[0], args.camera_size[1]
        );
    }

    let stream_task = stream(cam, session, args);
    if let Some(console_server) = console_server {
        let console_task = console_server.serve();
        let (console_task, stream_task) = tokio::join!(console_task, stream_task);
        console_task.unwrap();
        stream_task?;
    } else {
        stream_task.await?;
    }

    Ok(())
}

/// Validate the `--record` / `--replay` / `--replay-*` arg combinations up
/// front so we can fail the process with a single clear message before
/// opening the camera or any file handles.
fn validate_record_replay_args(args: &Args) -> Result<(), Box<dyn Error>> {
    if let Some(ref path) = args.record {
        if !args.h264 {
            return Err(Box::from(format!(
                "--record {:?} requires --h264 (record writes the main H.264 stream)",
                path
            )));
        }
    }
    if args.replay.is_some() {
        if args.jpeg {
            return Err(Box::from(
                "--replay does not support --jpeg (recorded files carry H.264 only)",
            ));
        }
        if args.h264_tiles {
            return Err(Box::from(
                "--replay does not support --h264-tiles (recorded files carry only the main stream)",
            ));
        }
    } else {
        // --replay-loop / --replay-fps are only meaningful with --replay.
        if args.replay_loop {
            warn!("--replay-loop has no effect without --replay");
        }
        if args.replay_fps.is_some() {
            warn!("--replay-fps has no effect without --replay");
        }
    }
    Ok(())
}

async fn stream(cam: CameraReader, session: Session, args: Args) -> Result<(), Box<dyn Error>> {
    // Compute monotonic→realtime offset once at startup for V4L2 timestamp conversion
    let clock_offset = ClockOffset::new()?;
    info!(
        "Clock offset: REALTIME - MONOTONIC = {}s {}ns",
        clock_offset.offset_sec, clock_offset.offset_nsec
    );

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
                    .block_on(h264_task(session, args, rx, clock_offset));
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
                    .block_on(jpeg_task(session, args, rx, clock_offset));
            })?;
    }

    let mut h264_tiles_txs = Vec::new();
    if args.h264_tiles {
        // Create 4 separate encoding threads, one for each tile
        let tile_positions = [
            TilePosition::TopLeft,
            TilePosition::TopRight,
            TilePosition::BottomLeft,
            TilePosition::BottomRight,
        ];

        for (i, &tile_pos) in tile_positions.iter().enumerate() {
            let (tx, rx) = kanal::bounded(3);
            let session = session.clone();
            let args = args.clone();
            let tile_topic = args.h264_tiles_topics[i].clone();

            thread::Builder::new()
                .name(format!("h264_tile_{:?}", tile_pos).to_lowercase())
                .spawn(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(h264_single_tile_task(
                            session,
                            args,
                            rx,
                            tile_pos,
                            tile_topic,
                            clock_offset,
                        ));
                })?;

            h264_tiles_txs.push(tx);
        }
    }

    // Colorimetry is resolved once at camera init time and constant for the
    // session. Populate CameraFrame's four colorimetry fields from it on
    // every publish without a per-frame FFI call.
    let colorimetry = Colorimetry::from_camera(&cam);

    let tf_fields = TfStaticFields::from_args(&args);
    let info_fields = CameraInfoFields::from_args(&args)?;

    // When --record is set, write the sidecar JSON before any frames flow.
    // Fields are stable for the session so one write at startup is enough.
    if let Some(ref record_path) = args.record {
        let sidecar = Sidecar::from_live(
            TARGET_FPS as u32,
            &cam,
            info_fields.clone(),
            tf_fields.clone(),
        );
        let written = sidecar.write_paired(record_path)?;
        info!(
            "Recording: H.264 bitstream → {:?}, sidecar → {:?}",
            record_path, written
        );
    }

    let tf_session = session.clone();
    let tf_msg = ZBytes::from(tf_fields.build_msg()?.into_cdr());
    let tf_enc = Encoding::APPLICATION_CDR.with_schema("geometry_msgs/msg/TransformStamped");
    let tf_task = tokio::spawn(async move { tf_static(tf_session, tf_msg, tf_enc).await });
    std::mem::drop(tf_task);

    let info_msg = ZBytes::from(info_fields.build_msg()?.into_cdr());
    let info_enc = Encoding::APPLICATION_CDR.with_schema("sensor_msgs/msg/CameraInfo");

    let src_pid = process::id();

    let mut prev = Instant::now();
    let mut history = vec![0.0; 60];
    let mut index = 0;

    while !SHUTDOWN.load(Ordering::SeqCst) {
        let camera_buffer = match info_span!("camera_read").in_scope(|| cam.read()) {
            Ok(buf) => buf,
            Err(videostream::Error::Io(e)) if e.kind() == std::io::ErrorKind::Interrupted => {
                // System call was interrupted by signal - check if shutdown requested
                if SHUTDOWN.load(Ordering::SeqCst) {
                    info!("Camera read interrupted by shutdown signal");
                    break;
                }
                continue;
            }
            Err(e) => return Err(e.into()),
        };

        let fps = update_fps(&mut prev, &mut history, &mut index);
        if fps < TARGET_FPS as f64 * 0.9 {
            warn!("low camera fps {} (target {})", fps, TARGET_FPS);
        }
        args.tracy.then(|| plot!("fps", fps));

        let cam_ts = camera_buffer.timestamp()?;
        let frame_sample_ts = zenoh_ts_for_frame(&session, &clock_offset, &cam_ts);
        let (msg, enc) = camera_frame_serialize(
            &camera_buffer,
            &cam_ts,
            src_pid,
            &args.camera_frame_id,
            &clock_offset,
            &colorimetry,
        )?;
        let span = info_span!("camera_publish");
        let local_session = session.clone();
        let frame_topic = args.frame_topic.clone();
        let frame_task = async move {
            local_session
                .put(frame_topic, msg)
                .encoding(enc)
                .timestamp(frame_sample_ts)
                .priority(Priority::Data)
                .congestion_control(CongestionControl::Drop)
                .await
                .unwrap();
        }
        .instrument(span);
        let info_task = publ_info
            .put(info_msg.clone())
            .encoding(info_enc.clone())
            .timestamp(session.new_timestamp());

        if args.h264 {
            let ts = camera_buffer.timestamp()?;
            let src_img = Image::from_camera(&camera_buffer)?;
            try_send(&h264_tx, src_img, ts, "H264");
        }

        if args.jpeg {
            let ts = camera_buffer.timestamp()?;
            let src_img = Image::from_camera(&camera_buffer)?;
            try_send(&jpeg_tx, src_img, ts, "JPEG");
        }

        if args.h264_tiles {
            let ts = camera_buffer.timestamp()?;
            for (i, tx) in h264_tiles_txs.iter().enumerate() {
                let src_img = Image::from_camera(&camera_buffer)?;
                try_send(tx, src_img, ts, &format!("H264_TILE_{}", i));
            }
        }

        let (_frame_task, info_task) = tokio::join!(frame_task, info_task);
        info_task.unwrap();

        args.tracy.then(frame_mark);
    }

    info!("Shutdown complete");
    Ok(())
}

fn try_send(tx: &Sender<(Image, Timestamp)>, img: Image, ts: Timestamp, _name: &str) {
    match tx.try_send((img, ts)) {
        Ok(_) => {}
        Err(_) => {
            // Channel issue - likely full due to slow encoding, which is
            // expected with 4 tile threads Silently drop frames
            // when channels are full to avoid log spam
        }
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
            .timestamp(session.new_timestamp())
            .await?;
    }
}

async fn h264_task(
    session: Session,
    args: Args,
    rx: Receiver<(Image, Timestamp)>,
    clock_offset: ClockOffset,
) {
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
        args.stream_size[0] as i32,
        args.stream_size[1] as i32,
        args.h264_bitrate,
    )
    .unwrap();

    // When --record is set, open the output file and keep a 256 KiB
    // BufWriter on it. The bitstream is raw Annex-B (no container) so
    // any prefix is a valid partial decode target and every NAL unit is
    // written whole via write_all. We flush on every keyframe, bounding
    // the power-loss window to one GOP (~1 s at default settings).
    let mut recorder: Option<std::io::BufWriter<std::fs::File>> = match args.record.as_ref() {
        Some(path) => match std::fs::File::create(path) {
            Ok(f) => Some(std::io::BufWriter::with_capacity(256 * 1024, f)),
            Err(e) => {
                error!("Failed to create recording file {:?}: {e}", path);
                return;
            }
        },
        None => None,
    };

    loop {
        let (msg, ts) = match rx.recv() {
            Ok(v) => v,
            Err(_) => {
                // main thread exited
                break;
            }
        };

        let span = info_span!("h264");
        let sample_ts = zenoh_ts_for_frame(&session, &clock_offset, &ts);
        let stamp = clock_offset.to_realtime(&ts);
        async {
            // Encode once. The bytes feed both the recorder tap and the
            // Zenoh publish path so a late publish-side drop doesn't
            // cost us a recorded frame.
            let (data, is_key) = match info_span!("h264_resize_encode")
                .in_scope(|| vidmgr.resize_and_encode(&msg, &imgmgr, &img_h264))
            {
                Ok(v) => v,
                Err(e) => {
                    error!("h264 encode failed: {e}");
                    return;
                }
            };

            if let Some(w) = recorder.as_mut() {
                use std::io::Write;
                if let Err(e) = w.write_all(&data) {
                    error!("h264 recorder write failed: {e}");
                } else if is_key {
                    if let Err(e) = w.flush() {
                        error!("h264 recorder flush failed: {e}");
                    }
                }
            }

            let (msg, enc) = build_h264_msg(&data, stamp, &args.camera_frame_id).unwrap();
            publisher
                .put(msg)
                .encoding(enc)
                .timestamp(sample_ts)
                .await
                .unwrap();
        }
        .instrument(span)
        .await;
        args.tracy.then(|| secondary_frame_mark!("h264"));
    }

    // BufWriter flushes on drop, but make the ordering explicit so the
    // last GOP hits disk before we return and the tokio runtime tears
    // this thread down.
    if let Some(mut w) = recorder.take() {
        use std::io::Write;
        if let Err(e) = w.flush() {
            error!("h264 recorder final flush failed: {e}");
        }
    }
}

async fn jpeg_task(
    session: Session,
    args: Args,
    rx: Receiver<(Image, Timestamp)>,
    clock_offset: ClockOffset,
) {
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
        let sample_ts = zenoh_ts_for_frame(&session, &clock_offset, &ts);
        async {
            let (msg, enc) =
                build_jpeg_msg(&msg, &ts, &imgmgr, &img_jpeg, &args, &clock_offset).unwrap();
            publisher
                .put(msg)
                .encoding(enc)
                .timestamp(sample_ts)
                .await
                .unwrap();
        }
        .instrument(span)
        .await;
        args.tracy.then(|| secondary_frame_mark!("jpeg"));
    }
}

async fn h264_single_tile_task(
    session: Session,
    args: Args,
    rx: Receiver<(Image, Timestamp)>,
    tile_pos: TilePosition,
    topic: String,
    clock_offset: ClockOffset,
) {
    let publisher = match session
        .declare_publisher(topic.clone())
        .priority(Priority::Data)
        .congestion_control(CongestionControl::Drop)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            error!(
                "Error while declaring H264 tile publisher {}: {:?}",
                topic, e
            );
            return;
        }
    };

    let (output_width, output_height) = TilePosition::get_output_dimensions();

    let initial_width = 3840u32; // Assume 4K source
    let initial_height = 2160u32;
    let (crop_x, crop_y, crop_width, crop_height) =
        tile_pos.get_crop_params(initial_width, initial_height);

    let mut vid_mgr = match VideoManager::new_with_crop(
        FourCC(*b"H264"),
        output_width as i32,
        output_height as i32,
        (
            crop_x as i32,
            crop_y as i32,
            crop_width as i32,
            crop_height as i32,
        ),
        args.h264_bitrate,
        Some(args.h264_tiles_fps as i32),
    ) {
        Ok(mgr) => mgr,
        Err(e) => {
            error!(
                "Failed to create VideoManager for tile {:?} with dimensions {}x{}, crop ({}, {}, {}, {}): {:?}",
                tile_pos, output_width, output_height, crop_x, crop_y, crop_width, crop_height, e
            );
            return;
        }
    };

    let mut last_source_size = (initial_width, initial_height);
    let tile_fps_limit = args.h264_tiles_fps;
    let frame_interval = Duration::from_millis(1000 / tile_fps_limit as u64);
    let mut last_encode_time = Instant::now();

    loop {
        let (source_img, ts) = match rx.recv() {
            Ok(v) => v,
            Err(_) => {
                // main thread exited
                return;
            }
        };

        let span = info_span!("h264_tile", tile = ?tile_pos);
        async {
            let now = Instant::now();
            if now.duration_since(last_encode_time) < frame_interval {
                return;
            }
            last_encode_time = now;
            let current_source_size = (source_img.width(), source_img.height());
            if current_source_size != last_source_size {
                let (new_crop_x, new_crop_y, new_crop_width, new_crop_height) =
                    tile_pos.get_crop_params(source_img.width(), source_img.height());
                vid_mgr.update_crop_region(
                    new_crop_x as i32,
                    new_crop_y as i32,
                    new_crop_width as i32,
                    new_crop_height as i32,
                );
                last_source_size = current_source_size;
            }

            match vid_mgr.encode_direct(&source_img) {
                Ok((data, _is_key)) => {
                    match build_tile_video_msg(&data, &ts, &args, tile_pos, &clock_offset) {
                        Ok((msg, enc)) => {
                            let sample_ts = zenoh_ts_for_frame(&session, &clock_offset, &ts);
                            if let Err(e) =
                                publisher.put(msg).encoding(enc).timestamp(sample_ts).await
                            {
                                error!("Failed to publish tile {:?}: {:?}", tile_pos, e);
                            }
                        }
                        Err(e) => {
                            error!("Failed to build tile video message: {:?}", e);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to encode tile {:?}: {:?}", tile_pos, e);
                }
            }
        }
        .instrument(span)
        .await;
        args.tracy.then(|| secondary_frame_mark!("h264_tile"));
    }
}

fn build_jpeg_msg(
    buf: &Image,
    ts: &Timestamp,
    imgmgr: &ImageManager,
    img: &Image,
    args: &Args,
    clock_offset: &ClockOffset,
) -> Result<(ZBytes, Encoding), Box<dyn Error>> {
    info_span!("jpeg_convert").in_scope(|| imgmgr.convert(buf, img, None, Rotation::Rotation0))?;

    let jpeg = info_span!("jpeg_encode").in_scope(|| {
        let dma = img.dmabuf();
        let buf = dma.memory_map()?.read(encode_jpeg, Some(img))?;
        Ok::<_, Box<dyn Error>>(buf)
    })?;

    args.tracy
        .then(|| plot!("jpeg_kb", (jpeg.len() / 1024) as f64));

    info_span!("jpeg_publish").in_scope(|| {
        let msg = CompressedImage::new(
            clock_offset.to_realtime(ts),
            &args.camera_frame_id,
            "jpeg",
            &jpeg,
        )?;
        let bytes = ZBytes::from(msg.into_cdr());
        let enc = Encoding::APPLICATION_CDR.with_schema("sensor_msgs/msg/CompressedImage");
        Ok((bytes, enc))
    })
}

/// Package already-encoded (or already-read) H.264 Annex-B bytes into a
/// `foxglove_msgs/CompressedVideo` CDR payload. Shared by the live
/// encode path and by replay (which reads the bytes from disk and
/// forwards them verbatim).
fn build_h264_msg(
    data: &[u8],
    stamp: builtin_interfaces::Time,
    frame_id: &str,
) -> Result<(ZBytes, Encoding), Box<dyn Error>> {
    info_span!("h264_publish").in_scope(|| {
        let msg = FoxgloveCompressedVideo::new(stamp, frame_id, data, "h264")?;
        let bytes = ZBytes::from(msg.into_cdr());
        let enc = Encoding::APPLICATION_CDR.with_schema("foxglove_msgs/msg/CompressedVideo");
        Ok((bytes, enc))
    })
}

fn build_tile_video_msg(
    data: &[u8],
    ts: &Timestamp,
    args: &Args,
    tile_pos: TilePosition,
    clock_offset: &ClockOffset,
) -> Result<(ZBytes, Encoding), Box<dyn Error>> {
    info_span!("h264_tile_publish").in_scope(|| {
        let frame_id = format!("{}_{:?}", args.camera_frame_id, tile_pos).to_lowercase();
        let msg =
            FoxgloveCompressedVideo::new(clock_offset.to_realtime(ts), &frame_id, data, "h264")?;
        let bytes = ZBytes::from(msg.into_cdr());
        let enc = Encoding::APPLICATION_CDR.with_schema("foxglove_msgs/msg/CompressedVideo");
        Ok((bytes, enc))
    })
}

/// Camera-level colorimetry captured once at startup and reused for every
/// published [`CameraFrame`]. V4L2 resolves these at `vsl_camera_init_device`
/// time and they are constant for the session, so we pay the FFI cost once.
/// Fields are empty strings when the driver returned V4L2 `_DEFAULT` or a
/// value outside the CameraFrame.msg vocabulary — matching the schema's
/// `""` = unknown convention.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Colorimetry {
    #[serde(rename = "color_space")]
    pub space: String,
    #[serde(rename = "color_transfer")]
    pub transfer: String,
    #[serde(rename = "color_encoding")]
    pub encoding: String,
    #[serde(rename = "color_range")]
    pub range: String,
}

impl Colorimetry {
    fn from_camera(cam: &CameraReader) -> Self {
        fn opt_str<T: std::fmt::Display>(r: Result<Option<T>, videostream::Error>) -> String {
            match r {
                Ok(Some(v)) => v.to_string(),
                _ => String::new(),
            }
        }
        Self {
            space: opt_str::<ColorSpace>(cam.color_space()),
            transfer: opt_str::<ColorTransfer>(cam.color_transfer()),
            encoding: opt_str::<ColorEncoding>(cam.color_encoding()),
            range: opt_str::<ColorRange>(cam.color_range()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_camera_frame_msg(
    stamp: builtin_interfaces::Time,
    frame_id: &str,
    seq: u64,
    pid: u32,
    width: u32,
    height: u32,
    format: &str,
    plane_fd: i32,
    plane_stride: u32,
    plane_len: u32,
    colorimetry: &Colorimetry,
) -> Result<(ZBytes, Encoding), Box<dyn Error>> {
    // Single-plane, contiguous DMA-BUF. Plane 0 covers the whole buffer;
    // for packed formats (YUYV) that is the entire image, for NV12 the
    // chroma plane lives inside the same fd via its natural offset but
    // is not described by a second CameraPlane entry until videostream
    // exposes multi-plane offsets (known limitation, tracked in the
    // 2.7.0 release notes).
    let plane = CameraPlaneView {
        fd: plane_fd,
        offset: 0,
        stride: plane_stride,
        size: plane_len,
        used: plane_len,
        data: &[],
    };

    let msg = CameraFrame::new(
        stamp,
        frame_id,
        seq,
        pid,
        width,
        height,
        format,
        &colorimetry.space,
        &colorimetry.transfer,
        &colorimetry.encoding,
        &colorimetry.range,
        /* fence_fd: */ -1,
        &[plane],
    )?;

    let bytes = ZBytes::from(msg.into_cdr());
    let enc = Encoding::APPLICATION_CDR.with_schema("edgefirst_msgs/msg/CameraFrame");
    Ok((bytes, enc))
}

#[instrument(skip_all, fields(width = buf.width(), height = buf.height(), format = buf.format().to_string()))]
fn camera_frame_serialize(
    buf: &CameraBuffer<'_>,
    ts: &Timestamp,
    pid: u32,
    frame_id: &str,
    clock_offset: &ClockOffset,
    colorimetry: &Colorimetry,
) -> Result<(ZBytes, Encoding), Box<dyn Error>> {
    build_camera_frame_msg(
        clock_offset.to_realtime(ts),
        frame_id,
        buf.sequence()? as u64,
        pid,
        buf.width() as u32,
        buf.height() as u32,
        &buf.format().to_string(),
        buf.rawfd(),
        buf.bytes_per_line()?,
        buf.length()? as u32,
        colorimetry,
    )
}

/// Build a Zenoh sample Timestamp from a ROS2 wall-clock `Time` (sec, nanosec
/// since Unix epoch). Uses the session's ZenohId as the timestamp ID so the
/// sample is attributable to this producer. Pre-epoch times (negative sec)
/// saturate both fields to the Unix epoch so the sample timestamp cannot
/// drift from the payload `Header.stamp` via partial clamping.
fn zenoh_ts_from_ros_time(session: &Session, t: builtin_interfaces::Time) -> ZenohTimestamp {
    let dur = if t.sec < 0 {
        Duration::new(0, 0)
    } else {
        Duration::new(t.sec as u64, t.nanosec)
    };
    ZenohTimestamp::new(NTP64::from(dur), session.zid().into())
}

/// Convenience: derive a Zenoh sample Timestamp from a V4L2 camera frame
/// timestamp, converting monotonic → wall-clock via the cached ClockOffset.
/// Matches the `Header.stamp` used in the CDR payload.
fn zenoh_ts_for_frame(
    session: &Session,
    clock_offset: &ClockOffset,
    cam_ts: &Timestamp,
) -> ZenohTimestamp {
    zenoh_ts_from_ros_time(session, clock_offset.to_realtime(cam_ts))
}

/// Saturated timestamp used when the system clock exceeds the ROS 2 Y2038 limit.
const SATURATED_TIME: builtin_interfaces::Time = builtin_interfaces::Time {
    sec: i32::MAX,
    nanosec: 999_999_999,
};

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
    /// Compute the fields that would populate a live `/camera/info` message
    /// from `Args`. Reads the optional calibration JSON at
    /// `args.cam_info_path`; falls back to reasonable defaults when not
    /// provided.
    pub(crate) fn from_args(args: &Args) -> Result<Self, Box<dyn Error>> {
        let (width, height, distortion_model, d, k, r, p) = if !args.cam_info_path.is_empty() {
            let file = File::open(&args.cam_info_path)
                .map_err(|e| format!("Cannot open file {:?}: {e:?}", &args.cam_info_path))?;
            let json: serde_json::Value =
                serde_json::from_reader(file).expect("file should be proper JSON");
            let bypass = json["bypass"].as_bool().unwrap_or(false);
            let dewarp_configs = &json["dewarpConfigArray"];
            if !dewarp_configs.is_array() {
                return Err(Box::from("Did not find dewarpConfigArray as an array"));
            }
            let dewarp_config = &dewarp_configs[0];
            let d: Vec<f64> = if bypass {
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
            let kv: Vec<f64> = match camera_matrix {
                Some(v) => v.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect(),
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

            (width, height, "plumb_bob", d, k, r, p)
        } else {
            let k = [1270.0, 0.0, 960.0, 0.0, 1270.0, 540.0, 0.0, 0.0, 1.0];
            let p = [
                k[0], k[1], k[2], 0.0, k[3], k[4], k[5], 0.0, k[6], k[7], k[8], 0.0,
            ];
            let r = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
            (1920, 1080, "plumb_bob", vec![0.0; 5], k, r, p)
        };

        Ok(CameraInfoFields {
            frame_id: args.camera_frame_id.clone(),
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
        })
    }

    /// Serialize these fields into a fresh `sensor_msgs/CameraInfo` CDR
    /// buffer stamped with the current wall-clock time.
    pub(crate) fn build_msg(&self) -> Result<CameraInfo<Vec<u8>>, Box<dyn Error>> {
        let stamp = match timestamp() {
            Ok(t) => t,
            Err(TimestampError::Overflow) => {
                warn!("Timestamp overflow: system clock exceeds i32 range (Y2038), saturating");
                SATURATED_TIME
            }
            Err(e) => return Err(e.into()),
        };
        Ok(CameraInfo::new(
            stamp,
            &self.frame_id,
            self.height,
            self.width,
            &self.distortion_model,
            &self.d,
            self.k,
            self.r,
            self.p,
            self.binning_x,
            self.binning_y,
            self.roi.into(),
        )?)
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
            base_frame_id: args.base_frame_id.clone(),
            child_frame_id: args.camera_frame_id.clone(),
            translation: [args.cam_tf_vec[0], args.cam_tf_vec[1], args.cam_tf_vec[2]],
            rotation: [
                args.cam_tf_quat[0],
                args.cam_tf_quat[1],
                args.cam_tf_quat[2],
                args.cam_tf_quat[3],
            ],
        }
    }

    pub(crate) fn build_msg(&self) -> Result<TransformStamped<Vec<u8>>, Box<dyn Error>> {
        let stamp = match timestamp() {
            Ok(t) => t,
            Err(TimestampError::Overflow) => {
                warn!("Timestamp overflow: system clock exceeds i32 range (Y2038), saturating");
                SATURATED_TIME
            }
            Err(e) => {
                warn!("Failed to get timestamp: {e}");
                Time { sec: 0, nanosec: 0 }
            }
        };

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

        Ok(TransformStamped::new(
            stamp,
            &self.base_frame_id,
            &self.child_frame_id,
            transform,
        )?)
    }
}

/// Errors that can occur when generating timestamps.
#[derive(Debug)]
enum TimestampError {
    /// System clock is before Unix epoch.
    BeforeEpoch(std::time::SystemTimeError),
    /// System clock seconds exceed i32 range (Y2038).
    Overflow,
}

impl std::fmt::Display for TimestampError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeEpoch(e) => write!(f, "system clock is before Unix epoch: {e}"),
            Self::Overflow => write!(f, "system clock seconds exceed i32::MAX (Y2038)"),
        }
    }
}

impl std::error::Error for TimestampError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BeforeEpoch(e) => Some(e),
            Self::Overflow => None,
        }
    }
}

/// Returns the current wall-clock time as a ROS2-compatible timestamp.
///
/// `SystemTime::now()` uses CLOCK_REALTIME on Linux (via vDSO, no actual syscall).
/// On embedded systems without battery-backed RTC (e.g., i.MX8MP), the wall clock
/// may jump once at boot when NTP syncs, but is stable afterward (NTP only slews).
///
/// Returns `TimestampError::Overflow` if the system clock exceeds `i32::MAX` seconds
/// (2038-01-19T03:14:07Z), which is the ROS 2 `builtin_interfaces/msg/Time` limit.
fn timestamp() -> Result<builtin_interfaces::Time, TimestampError> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(TimestampError::BeforeEpoch)?;

    let secs = duration.as_secs();
    if secs > i32::MAX as u64 {
        return Err(TimestampError::Overflow);
    }

    Ok(builtin_interfaces::Time {
        sec: secs as i32,
        nanosec: duration.subsec_nanos(),
    })
}

/// Cached offset between CLOCK_REALTIME and CLOCK_MONOTONIC for converting V4L2
/// hardware timestamps to wall-clock time.
///
/// V4L2 captures frame timestamps using CLOCK_MONOTONIC, but ROS2 Header stamps
/// require CLOCK_REALTIME. This offset converts between the two clock domains:
///
///   wall_time = v4l2_monotonic_timestamp + offset
///
/// This is the same pattern used by ROS2 image_transport and usb_cam drivers.
/// The offset is stable after NTP settles (typically within 30s of boot).
#[derive(Clone, Copy)]
struct ClockOffset {
    offset_sec: i64,
    offset_nsec: i64,
}

impl ClockOffset {
    /// Compute the offset by reading both clocks back-to-back.
    fn new() -> Result<Self, std::io::Error> {
        let mut realtime = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let mut monotonic = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };

        unsafe {
            if libc::clock_gettime(libc::CLOCK_REALTIME, &mut realtime) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut monotonic) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        // offset = realtime - monotonic (using i128 to avoid overflow during subtraction)
        let real_ns = realtime.tv_sec as i128 * 1_000_000_000 + realtime.tv_nsec as i128;
        let mono_ns = monotonic.tv_sec as i128 * 1_000_000_000 + monotonic.tv_nsec as i128;
        let offset_ns = real_ns - mono_ns;

        Ok(Self {
            offset_sec: (offset_ns / 1_000_000_000) as i64,
            offset_nsec: (offset_ns % 1_000_000_000) as i64,
        })
    }

    /// Convert a V4L2 CLOCK_MONOTONIC timestamp to CLOCK_REALTIME for ROS2 Header stamps.
    fn to_realtime(self, ts: &Timestamp) -> builtin_interfaces::Time {
        let mono_sec = ts.seconds();
        let mono_nsec = ts.subsec(9) as i64;

        let mut real_sec = mono_sec + self.offset_sec;
        let mut real_nsec = mono_nsec + self.offset_nsec;

        // Normalize nanoseconds into [0, 999_999_999]
        if real_nsec >= 1_000_000_000 {
            real_sec += 1;
            real_nsec -= 1_000_000_000;
        } else if real_nsec < 0 {
            real_sec -= 1;
            real_nsec += 1_000_000_000;
        }

        // Clamp to i32 range for ROS2 builtin_interfaces::Time (Y2038 limit)
        let sec = if real_sec > i32::MAX as i64 {
            warn!("Timestamp overflow: V4L2 converted time exceeds i32 range (Y2038), saturating");
            return SATURATED_TIME;
        } else {
            real_sec as i32
        };

        builtin_interfaces::Time {
            sec,
            nanosec: real_nsec as u32,
        }
    }
}

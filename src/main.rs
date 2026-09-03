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
    edgefirst_msgs::{CameraFrame, TensorFields, TensorPlaneView},
    foxglove_msgs::FoxgloveCompressedVideo,
    geometry_msgs::{Quaternion, Transform, TransformStamped, Vector3},
    sensor_msgs::{CameraInfo, CompressedImage, RegionOfInterest},
};
use kanal::{Receiver, Sender};
use sidecar::Sidecar;
use std::{
    collections::BTreeMap,
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

/// How many consecutive failed `cam.read()` calls to tolerate before
/// giving up on the camera. Each failed read costs the driver's own frame
/// timeout, so this is a duration budget as much as a count.
const MAX_CONSECUTIVE_READ_FAILURES: u32 = 5;

/// How often to summarise dropped frames. One line per window, not per
/// drop: under sustained load every frame can be a drop.
const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(10);

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

    // The h264 thread is spawned later (after the recorder file is
    // opened and the sidecar is written) so a doomed `--record` run
    // fails the whole process before any thread is running.
    let (h264_tx, h264_rx) = kanal::bounded(1);

    let (jpeg_tx, rx) = kanal::bounded(1);
    if args.jpeg {
        let session = session.clone();
        let args = args.clone();
        thread::Builder::new()
            .name("jpeg".to_string())
            .spawn(move || {
                // Multi-thread with one worker is what Zenoh 1.6+
                // requires for `Session::drop`'s internal close path —
                // it calls `block_in_place` from `ZRuntime::Net` and
                // panics if the surrounding runtime is current-thread
                // ("Zenoh runtime doesn't support Tokio's current
                // thread scheduler"). One worker preserves the
                // single-encoder-per-thread shape we want here.
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
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
                    // Multi-thread with one worker — see the matching
                    // comment on the h264 spawn above for why current-
                    // thread is not viable with Zenoh 1.6+.
                    tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(1)
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
    // session. Populate the embedded Tensor's four colorimetry fields from
    // it on every publish without a per-frame FFI call.
    let colorimetry = Colorimetry::from_camera(&cam);

    let tf_fields = TfStaticFields::from_args(&args);
    let info_fields = CameraInfoFields::from_args(&args)?;

    // When --record is set, open the H.264 output file and the
    // matching sidecar before any frames flow. Order matters:
    //
    //   1. Open the BufWriter on the .h264 file. If creation fails
    //      (path missing, no perms, FS full) we surface the error
    //      here and abort the run cleanly — never produce an
    //      orphaned sidecar for a recording that never started.
    //   2. Write the .json sidecar. Fields are stable for the
    //      session so one write at startup is enough.
    //
    // Use the encoder's stream dimensions in the sidecar (what the
    // recorded .h264 file will actually contain), not the camera
    // capture dimensions — those can differ when --stream-size
    // rescales from --camera-size.
    let recorder: Option<std::io::BufWriter<std::fs::File>> = match args.record.as_ref() {
        Some(path) => {
            let file = std::fs::File::create(path)
                .map_err(|e| format!("Cannot create recording file {:?}: {e}", path))?;
            let bw = std::io::BufWriter::with_capacity(256 * 1024, file);

            let sidecar = Sidecar::from_live(
                TARGET_FPS as u32,
                args.stream_size[0],
                args.stream_size[1],
                &cam,
                info_fields.clone(),
                tf_fields.clone(),
            );
            let written = sidecar.write_paired(path)?;
            info!(
                "Recording: H.264 bitstream → {:?}, sidecar → {:?}",
                path, written
            );
            Some(bw)
        }
        None => None,
    };

    // Spawn the h264 thread now that the recorder file (if any) is
    // open. The thread takes ownership of the BufWriter; flushes on
    // every keyframe; final flush on drop.
    if args.h264 {
        let session = session.clone();
        let args = args.clone();
        let rx = h264_rx;
        thread::Builder::new()
            .name("h264".to_string())
            .spawn(move || {
                // Multi-thread with one worker is what Zenoh 1.6+
                // requires for `Session::drop`'s internal close path —
                // it calls `block_in_place` from `ZRuntime::Net` and
                // panics if the surrounding runtime is current-thread
                // ("Zenoh runtime doesn't support Tokio's current
                // thread scheduler"). One worker preserves the
                // single-encoder-per-thread shape we want here.
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(h264_task(session, args, rx, clock_offset, recorder));
            })?;
    } else {
        // --record requires --h264 (enforced by validate_record_replay_args),
        // so an open recorder always pairs with the spawn above. Drop the
        // unused receiver explicitly to keep the channel from staying open.
        drop(h264_rx);
        drop(recorder);
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
    let mut read_retry = ReadRetry::new(MAX_CONSECUTIVE_READ_FAILURES);
    let mut drops = DropStats::new(DROP_REPORT_INTERVAL, Instant::now());

    // The camera fourcc is set at open() time and constant for the
    // session, so the Tensor.format string can be computed once and
    // reused. Lazily initialized from the first buffer to avoid an
    // extra `cam.read()` outside the loop. Avoids a per-frame
    // allocation in the hot publish path.
    let mut fourcc_str: Option<String> = None;

    while !SHUTDOWN.load(Ordering::SeqCst) {
        let camera_buffer = match info_span!("camera_read").in_scope(|| cam.read()) {
            Ok(buf) => {
                read_retry.on_success();
                buf
            }
            Err(videostream::Error::Io(e)) if e.kind() == std::io::ErrorKind::Interrupted => {
                // System call was interrupted by signal - check if shutdown requested
                if SHUTDOWN.load(Ordering::SeqCst) {
                    info!("Camera read interrupted by shutdown signal");
                    break;
                }
                continue;
            }
            Err(e) => {
                // One failed read is not evidence the camera is gone, and
                // the error cannot be trusted to say which it is: the
                // underlying vsl_camera_get_data reports a timeout by
                // returning NULL, leaving videostream to surface whatever
                // errno happened to hold. Spend a retry budget instead of
                // treating the first failure as fatal -- a transient gap
                // used to kill the process, and the unit then sat out its
                // restart delay, turning one hiccup into a multi-second
                // hole in the operator's recording (EDGEAI-1403).
                if !read_retry.should_retry() {
                    error!(
                        failures = read_retry.consecutive(),
                        "camera read failed {MAX_CONSECUTIVE_READ_FAILURES} times in a row: {e}"
                    );
                    return Err(e.into());
                }
                warn!(
                    attempt = read_retry.consecutive(),
                    max = MAX_CONSECUTIVE_READ_FAILURES,
                    "camera read failed, retrying: {e}"
                );
                continue;
            }
        };

        let fps = update_fps(&mut prev, &mut history, &mut index);
        if fps < TARGET_FPS as f64 * 0.9 {
            warn!("low camera fps {} (target {})", fps, TARGET_FPS);
        }
        args.tracy.then(|| plot!("fps", fps));

        let fourcc = fourcc_str.get_or_insert_with(|| camera_buffer.format().to_string());

        let cam_ts = camera_buffer.timestamp()?;
        let frame_sample_ts = zenoh_ts_for_frame(&session, &clock_offset, &cam_ts);
        let (msg, enc) = camera_frame_serialize(
            &camera_buffer,
            &cam_ts,
            src_pid,
            &args.camera_frame_id,
            &clock_offset,
            &colorimetry,
            fourcc,
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
            try_send(&h264_tx, src_img, ts, "h264", &mut drops);
        }

        if args.jpeg {
            let ts = camera_buffer.timestamp()?;
            let src_img = Image::from_camera(&camera_buffer)?;
            try_send(&jpeg_tx, src_img, ts, "jpeg", &mut drops);
        }

        if args.h264_tiles {
            let ts = camera_buffer.timestamp()?;
            for (i, tx) in h264_tiles_txs.iter().enumerate() {
                let src_img = Image::from_camera(&camera_buffer)?;
                try_send(
                    tx,
                    src_img,
                    ts,
                    TILE_SINKS.get(i).copied().unwrap_or("h264/tile"),
                    &mut drops,
                );
            }
        }

        if let Some(report) = drops.take_report(Instant::now()) {
            warn!("{report}");
        }

        let (_frame_task, info_task) = tokio::join!(frame_task, info_task);
        info_task.unwrap();

        args.tracy.then(frame_mark);
    }

    info!("Shutdown complete");
    Ok(())
}

/// Names of the sinks `try_send` can drop a frame on. Static so the hot
/// path does not allocate a label per frame per tile.
const TILE_SINKS: [&str; 4] = ["h264/tl", "h264/tr", "h264/bl", "h264/br"];

/// Counters for frames discarded because a downstream encoder channel was
/// full.
///
/// Drops are expected under load -- four tile encoders cannot always keep
/// up with capture, and dropping is the correct response. The problem this
/// solves is that they were invisible: the discard arm was empty, so an
/// operator had no way to tell a captured frame from a discarded one, or
/// to know a recording had holes in it. Reporting is rate-limited to one
/// line per interval rather than per drop, which is why the empty arm
/// existed in the first place.
pub(crate) struct DropStats {
    dropped: BTreeMap<&'static str, u64>,
    sent: u64,
    interval: Duration,
    last_report: Instant,
}

impl DropStats {
    pub(crate) fn new(interval: Duration, now: Instant) -> Self {
        Self {
            dropped: BTreeMap::new(),
            sent: 0,
            interval,
            last_report: now,
        }
    }

    pub(crate) fn record_sent(&mut self) {
        self.sent = self.sent.saturating_add(1);
    }

    pub(crate) fn record_drop(&mut self, sink: &'static str) {
        *self.dropped.entry(sink).or_insert(0) += 1;
    }

    /// A one-line summary of the window just ended, or `None` while the
    /// reporting interval has not elapsed. Taking a report resets the
    /// window.
    pub(crate) fn take_report(&mut self, now: Instant) -> Option<String> {
        if now.duration_since(self.last_report) < self.interval {
            return None;
        }
        self.last_report = now;
        if self.dropped.is_empty() {
            self.sent = 0;
            return None;
        }

        let total: u64 = self.dropped.values().sum();
        let per_sink = self
            .dropped
            .iter()
            .map(|(sink, n)| format!("{sink}={n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let report = format!(
            "dropped {total} of {} frames (encoder channels full): {per_sink}",
            total + self.sent
        );

        self.dropped.clear();
        self.sent = 0;
        Some(report)
    }
}

/// Retry budget for `cam.read()`.
///
/// A single failed read is not evidence the camera is gone. The ISP can
/// stall for a frame or two across a mode change or a transient bus
/// hiccup, and `vsl_camera_get_data` reports that by returning NULL --
/// after which `videostream` surfaces whatever `errno` happened to hold,
/// so the error kind cannot be trusted to classify it (EDGEAI-1403). Count
/// consecutive failures instead and only give up once the camera has
/// stopped producing frames for a sustained stretch.
pub(crate) struct ReadRetry {
    consecutive: u32,
    max: u32,
}

impl ReadRetry {
    pub(crate) fn new(max: u32) -> Self {
        Self {
            consecutive: 0,
            max,
        }
    }

    pub(crate) fn on_success(&mut self) {
        self.consecutive = 0;
    }

    /// Records a failed read. Returns `true` while the budget allows
    /// another attempt, `false` once it is exhausted.
    pub(crate) fn should_retry(&mut self) -> bool {
        self.consecutive = self.consecutive.saturating_add(1);
        self.consecutive <= self.max
    }

    pub(crate) fn consecutive(&self) -> u32 {
        self.consecutive
    }
}

/// Minimum spacing between encoded tile frames for a requested FPS limit.
///
/// `0` means no limit: the operator can set `H264_TILES_FPS=0` and it used
/// to divide by zero and take the tile threads down silently.
pub(crate) fn tile_frame_interval(fps: u32) -> Duration {
    match fps {
        0 => Duration::ZERO,
        fps => Duration::from_millis(1000 / u64::from(fps)),
    }
}

fn try_send(
    tx: &Sender<(Image, Timestamp)>,
    img: Image,
    ts: Timestamp,
    sink: &'static str,
    stats: &mut DropStats,
) {
    record_send_outcome(tx.try_send((img, ts)), sink, stats);
}

/// Record whether a `try_send` actually delivered the frame.
///
/// kanal reports a **full** channel as `Ok(false)`, not as an error --
/// `Err` means the channel is closed. So `Ok(_) => delivered` silently
/// counts every dropped frame as a sent one, which is what the original
/// empty-discard-arm code did and what made the first version of these
/// counters read zero under load: four tile encoders publishing at 3.7Hz
/// against a 53Hz source could not fill a bounded(3) channel according to
/// the counters, which was obviously false.
fn record_send_outcome(
    outcome: Result<bool, kanal::SendError>,
    sink: &'static str,
    stats: &mut DropStats,
) {
    match outcome {
        Ok(true) => stats.record_sent(),
        // Full: encoding is slower than capture, expected under load.
        // Dropping is the right response; counting it is what lets an
        // operator see that it happened.
        Ok(false) => stats.record_drop(sink),
        // Closed: the encoder thread is gone. Still a frame that did not
        // reach anyone, so it counts the same way.
        Err(_) => stats.record_drop(sink),
    }
}

async fn tf_static(
    session: Session,
    msg: ZBytes,
    enc: Encoding,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let topic = "tf_static".to_string();
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
    // Pre-opened in `stream()` before the sidecar write so a doomed
    // record run aborts the whole process before producing orphaned
    // metadata. `None` when `--record` is not set.
    mut recorder: Option<std::io::BufWriter<std::fs::File>>,
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
    let frame_interval = tile_frame_interval(tile_fps_limit);
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
        let msg = CompressedImage::builder()
            .stamp(clock_offset.to_realtime(ts))
            .frame_id(args.camera_frame_id.as_str())
            .format("jpeg")
            .data(jpeg.as_ref())
            .build()?;
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
        let msg = FoxgloveCompressedVideo::builder()
            .stamp(stamp)
            .frame_id(frame_id)
            .data(data)
            .format("h264")
            .build()?;
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
        let msg = FoxgloveCompressedVideo::builder()
            .stamp(clock_offset.to_realtime(ts))
            .frame_id(frame_id.as_str())
            .data(data)
            .format("h264")
            .build()?;
        let bytes = ZBytes::from(msg.into_cdr());
        let enc = Encoding::APPLICATION_CDR.with_schema("foxglove_msgs/msg/CompressedVideo");
        Ok((bytes, enc))
    })
}

/// Camera-level colorimetry captured once at startup and reused for every
/// published [`CameraFrame`]. V4L2 resolves these at `vsl_camera_init_device`
/// time and they are constant for the session, so we pay the FFI cost once.
/// Fields are empty strings when the driver returned V4L2 `_DEFAULT` or a
/// value outside the Tensor.msg vocabulary — matching the schema's
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

/// HAL Modular Tensor ABI codes carried (not interpreted) by schemas 4.0.
/// `storage_kind = 2` is `EfStorageKind::DmaBuf`; `dtype = 0` is
/// `EfDtype::U8` (`I8` is 1). See `edgefirst-tensor-abi` — schemas must
/// not grow a parallel enum.
const TENSOR_STORAGE_KIND_DMA_BUF: u32 = 2;
const TENSOR_DTYPE_U8: u32 = 0;

/// Bytes per addressing-grid sample along the width axis.
///
/// Tensor `shape` is `[height, width]`, not the byte layout. Packed YUYV
/// stores two bytes per pixel; NV12 luma is one; RGB variants follow bpp.
/// Empty strides are only valid for densely packed C-order, so any
/// format whose last-dim stride is not 1 must be explicit.
fn pixel_stride_bytes(format: &str) -> i64 {
    match format {
        "YUYV" | "UYVY" | "YVYU" | "VYUY" => 2,
        "RGB3" | "BGR3" => 3,
        "RGBA" | "RGBX" | "BGRA" | "BGRX" | "ARGB" | "ABGR" => 4,
        _ => 1,
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
    // is not described by a second TensorPlane entry until videostream
    // exposes multi-plane offsets (known limitation, tracked in the
    // 2.7.0 release notes).
    let shape = [height as u64, width as u64];
    let strides = [plane_stride as i64, pixel_stride_bytes(format)];
    let plane = TensorPlaneView {
        handle: plane_fd as i64,
        offset: 0,
        stride: plane_stride as u64,
        size: plane_len as u64,
        used: plane_len as u64,
        modifier: 0,
        handle_bytes: &[],
        data: &[],
    };
    let tensor = TensorFields {
        storage_kind: TENSOR_STORAGE_KIND_DMA_BUF,
        pid,
        fence_fd: -1,
        dtype: TENSOR_DTYPE_U8,
        quant_axis: -2,
        shape: &shape,
        strides: &strides,
        quant_scales: &[],
        quant_zero_points: &[],
        format: format.into(),
        color_space: colorimetry.space.as_str().into(),
        color_transfer: colorimetry.transfer.as_str().into(),
        color_encoding: colorimetry.encoding.as_str().into(),
        color_range: colorimetry.range.as_str().into(),
        planes: std::slice::from_ref(&plane),
    };

    let mut buf = Vec::new();
    CameraFrame::builder()
        .stamp(stamp)
        .frame_id(frame_id)
        .seq(seq)
        .tensor(&tensor)
        .encode_into_vec(&mut buf)?;

    let bytes = ZBytes::from(buf);
    let enc = Encoding::APPLICATION_CDR.with_schema("edgefirst_msgs/msg/CameraFrame");
    Ok((bytes, enc))
}

#[instrument(skip_all, fields(width = buf.width(), height = buf.height(), format = fourcc))]
fn camera_frame_serialize(
    buf: &CameraBuffer<'_>,
    ts: &Timestamp,
    pid: u32,
    frame_id: &str,
    clock_offset: &ClockOffset,
    colorimetry: &Colorimetry,
    fourcc: &str,
) -> Result<(ZBytes, Encoding), Box<dyn Error>> {
    build_camera_frame_msg(
        clock_offset.to_realtime(ts),
        frame_id,
        buf.sequence()? as u64,
        pid,
        buf.width() as u32,
        buf.height() as u32,
        fourcc,
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
                .map_err(|e| format!("Cannot open file {:?}: {e:?}", args.cam_info_path))?;
            let json: serde_json::Value = serde_json::from_reader(file).map_err(|e| {
                format!(
                    "Cannot parse camera info JSON from {:?}: {e}",
                    args.cam_info_path
                )
            })?;
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

        Ok(TransformStamped::builder()
            .stamp(stamp)
            .frame_id(self.base_frame_id.as_str())
            .child_frame_id(self.child_frame_id.as_str())
            .transform(transform)
            .build()?)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Build an `Args` pre-populated with the clap defaults so tests can
    /// flip individual fields without rebuilding the whole struct.
    fn default_args() -> Args {
        Args::parse_from(["edgefirst-camera"])
    }

    #[test]
    fn validate_accepts_live_capture_with_no_record_or_replay() {
        let args = default_args();
        validate_record_replay_args(&args).expect("plain live path must validate");
    }

    #[test]
    fn validate_record_requires_h264() {
        let mut args = default_args();
        args.record = Some(PathBuf::from("/tmp/not-written.h264"));
        args.h264 = false;
        let err = validate_record_replay_args(&args).unwrap_err().to_string();
        assert!(
            err.contains("--record") && err.contains("--h264"),
            "expected record-requires-h264 error, got: {err}"
        );
    }

    #[test]
    fn validate_record_with_h264_is_ok() {
        let mut args = default_args();
        args.record = Some(PathBuf::from("/tmp/not-written.h264"));
        args.h264 = true;
        validate_record_replay_args(&args).unwrap();
    }

    #[test]
    fn validate_replay_rejects_jpeg() {
        let mut args = default_args();
        args.replay = Some(PathBuf::from("/tmp/not-read.h264"));
        args.jpeg = true;
        let err = validate_record_replay_args(&args).unwrap_err().to_string();
        assert!(
            err.contains("--replay") && err.contains("--jpeg"),
            "expected replay-rejects-jpeg error, got: {err}"
        );
    }

    #[test]
    fn validate_replay_rejects_h264_tiles() {
        let mut args = default_args();
        args.replay = Some(PathBuf::from("/tmp/not-read.h264"));
        args.h264_tiles = true;
        let err = validate_record_replay_args(&args).unwrap_err().to_string();
        assert!(
            err.contains("--replay") && err.contains("--h264-tiles"),
            "expected replay-rejects-tiles error, got: {err}"
        );
    }

    #[test]
    fn validate_replay_with_h264_forward_is_ok() {
        let mut args = default_args();
        args.replay = Some(PathBuf::from("/tmp/not-read.h264"));
        args.h264 = true;
        validate_record_replay_args(&args).unwrap();
    }

    #[test]
    fn kanal_reports_a_full_channel_as_ok_false() {
        // Pins the dependency contract this counting depends on. kanal's
        // try_send returns Result<bool, SendError>: Err means the channel
        // is closed, and a *full* channel is Ok(false). Matching on Ok(_)
        // therefore counts every dropped frame as a delivered one.
        let (tx, _rx) = kanal::bounded::<u8>(1);
        assert_eq!(tx.try_send(1), Ok(true));
        assert_eq!(tx.try_send(2), Ok(false), "a full channel is Ok(false)");
    }

    #[test]
    fn a_full_channel_is_counted_as_a_drop_not_a_send() {
        let t0 = Instant::now();
        let mut stats = DropStats::new(Duration::from_secs(10), t0);
        record_send_outcome(Ok(true), "h264", &mut stats);
        record_send_outcome(Ok(false), "h264", &mut stats);
        record_send_outcome(Err(kanal::SendError::ReceiveClosed), "h264", &mut stats);

        let report = stats
            .take_report(t0 + Duration::from_secs(11))
            .expect("two of the three sends did not deliver");
        assert!(report.contains("h264=2"), "got: {report}");
        assert!(report.contains("of 3 frames"), "got: {report}");
    }

    #[test]
    fn drop_stats_counts_per_sink_and_reports_after_the_interval() {
        let t0 = Instant::now();
        let mut stats = DropStats::new(Duration::from_secs(10), t0);

        stats.record_sent();
        stats.record_drop("h264/tl");
        stats.record_drop("h264/tl");
        stats.record_drop("jpeg");

        assert!(
            stats.take_report(t0 + Duration::from_secs(1)).is_none(),
            "must stay quiet inside the reporting interval"
        );

        let report = stats
            .take_report(t0 + Duration::from_secs(11))
            .expect("a report is due once the interval has elapsed");
        assert!(report.contains("h264/tl=2"), "got: {report}");
        assert!(report.contains("jpeg=1"), "got: {report}");
    }

    #[test]
    fn drop_stats_reports_nothing_when_no_frames_were_dropped() {
        let t0 = Instant::now();
        let mut stats = DropStats::new(Duration::from_secs(10), t0);
        stats.record_sent();
        assert!(stats.take_report(t0 + Duration::from_secs(11)).is_none());
    }

    #[test]
    fn drop_stats_resets_the_window_after_reporting() {
        let t0 = Instant::now();
        let mut stats = DropStats::new(Duration::from_secs(10), t0);
        stats.record_drop("jpeg");
        stats.take_report(t0 + Duration::from_secs(11)).unwrap();

        stats.record_drop("jpeg");
        let second = stats
            .take_report(t0 + Duration::from_secs(22))
            .expect("second window should report on its own");
        assert!(
            second.contains("jpeg=1"),
            "counts must not carry over between windows, got: {second}"
        );
    }

    #[test]
    fn read_retry_spends_its_budget_then_gives_up() {
        let mut retry = ReadRetry::new(3);
        assert!(retry.should_retry(), "1st failure is retryable");
        assert!(retry.should_retry(), "2nd failure is retryable");
        assert!(retry.should_retry(), "3rd failure is retryable");
        assert!(!retry.should_retry(), "budget of 3 is spent");
    }

    #[test]
    fn read_retry_resets_on_a_successful_read() {
        let mut retry = ReadRetry::new(2);
        assert!(retry.should_retry());
        retry.on_success();
        assert_eq!(retry.consecutive(), 0);
        assert!(retry.should_retry(), "budget is restored after a good read");
        assert!(retry.should_retry());
        assert!(!retry.should_retry());
    }

    #[test]
    fn tile_frame_interval_treats_zero_as_no_limit() {
        // H264_TILES_FPS=0 is operator-settable and used to divide by
        // zero, killing the tile threads silently.
        assert_eq!(tile_frame_interval(0), Duration::ZERO);
    }

    #[test]
    fn tile_frame_interval_spaces_frames_for_a_positive_limit() {
        assert_eq!(tile_frame_interval(15), Duration::from_millis(66));
        assert_eq!(tile_frame_interval(10), Duration::from_millis(100));
    }

    #[test]
    fn camera_info_fields_from_args_with_no_json_path_uses_defaults() {
        let mut args = default_args();
        args.cam_info_path = String::new();
        let f = CameraInfoFields::from_args(&args).unwrap();
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

    #[test]
    fn camera_info_fields_rejects_bad_camera_matrix_length() {
        // Write a calibration JSON with camera_matrix of length 8 to hit
        // the length-validation branch and confirm the error message
        // points at camera_matrix (Copilot PR #6 feedback).
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
        args.cam_info_path = path.to_string_lossy().into_owned();
        let err = CameraInfoFields::from_args(&args).unwrap_err().to_string();
        assert!(
            err.contains("camera_matrix"),
            "error must reference camera_matrix, got: {err}"
        );
        assert!(
            err.contains("8"),
            "error must include actual length (8), got: {err}"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn camera_info_fields_rejects_malformed_json() {
        // A non-JSON file at cam_info_path used to panic via
        // `.expect("file should be proper JSON")`; now it must surface a
        // structured error that names the offending file.
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let path = tmp.join(format!("edgefirst_cam_info_bad_json_{pid}.json"));
        std::fs::write(&path, b"this is not valid json {").unwrap();

        let mut args = default_args();
        args.cam_info_path = path.to_string_lossy().into_owned();
        let err = CameraInfoFields::from_args(&args).unwrap_err().to_string();
        assert!(
            err.to_lowercase().contains("parse"),
            "error must say it failed to parse, got: {err}"
        );
        assert!(
            err.contains(args.cam_info_path.as_str()),
            "error must include the offending file path, got: {err}"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn camera_info_fields_rejects_missing_dewarp_array() {
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let path = tmp.join(format!("edgefirst_cam_info_no_array_{pid}.json"));
        std::fs::write(&path, r#"{"dewarpConfigArray": "not_an_array"}"#).unwrap();

        let mut args = default_args();
        args.cam_info_path = path.to_string_lossy().into_owned();
        let err = CameraInfoFields::from_args(&args).unwrap_err().to_string();
        assert!(err.to_lowercase().contains("dewarp"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tf_static_fields_from_args_mirrors_cli_shape() {
        let args = default_args();
        let tf = TfStaticFields::from_args(&args);
        // The defaults may evolve, but base/child frame IDs must always
        // come from the CLI strings and the arrays must have the
        // expected arity.
        assert_eq!(tf.base_frame_id, args.base_frame_id);
        assert_eq!(tf.child_frame_id, args.camera_frame_id);
        assert_eq!(tf.translation.len(), 3);
        assert_eq!(tf.rotation.len(), 4);
    }

    #[test]
    fn tf_static_fields_build_msg_produces_nonempty_cdr() {
        let args = default_args();
        let tf = TfStaticFields::from_args(&args);
        let msg = tf.build_msg().expect("tf CDR build must succeed");
        assert!(!msg.as_cdr().is_empty());
    }

    #[test]
    fn camera_info_fields_build_msg_produces_nonempty_cdr() {
        let mut args = default_args();
        args.cam_info_path = String::new();
        let info = CameraInfoFields::from_args(&args).unwrap();
        let msg = info.build_msg().expect("info CDR build must succeed");
        assert!(!msg.as_cdr().is_empty());
    }

    #[test]
    fn colorimetry_default_is_all_unknown_empty_strings() {
        let c = Colorimetry::default();
        assert!(c.space.is_empty());
        assert!(c.transfer.is_empty());
        assert!(c.encoding.is_empty());
        assert!(c.range.is_empty());
    }

    #[test]
    fn camera_frame_embeds_tensor_with_dma_plane() {
        let colorimetry = Colorimetry {
            space: "bt709".into(),
            transfer: "bt709".into(),
            encoding: "bt601".into(),
            range: "limited".into(),
        };
        let (payload, _enc) = build_camera_frame_msg(
            Time { sec: 1, nanosec: 2 },
            "camera",
            42,
            1000,
            1920,
            1080,
            "YUYV",
            7,
            3840,
            1920 * 1080 * 2,
            &colorimetry,
        )
        .expect("CameraFrame CDR build must succeed");

        let raw = payload.to_bytes();
        let cf = CameraFrame::<&[u8]>::from_cdr(raw.as_ref()).unwrap();
        assert_eq!(cf.seq(), 42);
        assert_eq!(cf.frame_id(), "camera");
        assert_eq!(cf.stamp(), Time { sec: 1, nanosec: 2 });

        let t = cf.tensor();
        assert_eq!(t.storage_kind(), TENSOR_STORAGE_KIND_DMA_BUF);
        // Literal HAL ABI value: EfDtype::U8 = 0 (I8 = 1). Do not
        // assert against TENSOR_DTYPE_U8 or a mistyped constant would
        // hide the same class of error this test exists to catch.
        assert_eq!(t.dtype(), 0);
        assert_eq!(t.pid(), 1000);
        assert_eq!(t.fence_fd(), -1);
        assert_eq!(t.format(), "YUYV");
        assert_eq!(t.color_space(), "bt709");
        assert_eq!(t.color_encoding(), "bt601");
        assert_eq!(t.shape().collect::<Vec<_>>(), vec![1080, 1920]);
        assert_eq!(t.strides().collect::<Vec<_>>(), vec![3840, 2]);
        assert_eq!(t.num_planes(), 1);
        let plane = t.plane_at(0).unwrap();
        assert_eq!(plane.handle, 7);
        assert_eq!(plane.stride, 3840);
        assert_eq!(plane.size, 1920 * 1080 * 2);
        assert!(plane.data.is_empty());
    }
}

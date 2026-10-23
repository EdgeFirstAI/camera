// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

//! The capture pipeline.
//!
//! One thread owns the camera and publishes `camera/frame` and
//! `camera/info`; the JPEG, H.264 and tile encoders each run on a thread of
//! their own, fed over bounded channels. Every message about one frame
//! carries one stamp, taken once from the frame's acquisition time, and a
//! Zenoh sample timestamp equal to it.

use std::{
    collections::BTreeMap,
    error::Error,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use edgefirst_camera::{
    schema::{self, FrameTensor},
    Camera, ErrorKind, Frame, Mirror, Sidecar, StreamConfig,
};
use edgefirst_codec::video::{Bitrate, EncodedFrame};
use edgefirst_schemas::{builtin_interfaces::Time, edgefirst_msgs::CameraFrame};
use kanal::{Receiver, Sender};
use tracing::{debug, error, info, info_span, warn};
use tracy_client::{frame_mark, plot, secondary_frame_mark};
use zenoh::{
    bytes::{Encoding, ZBytes},
    pubsub::Publisher,
    qos::{CongestionControl, Priority},
    Session, Wait,
};

use crate::{
    encode::{self, OpenEncoder, Plan, Stage, TILE_NAMES},
    exit::{Failure, Status},
    jpeg::JpegStage,
    messages::{jpeg_msg, sample_timestamp, video_msg, CameraInfoFields, TfStaticFields},
    source::{Opened, SourceKind},
};

/// How many consecutive `next_frame` timeouts to tolerate before giving up
/// on the camera. A transient gap (an ISP stall across a mode change) must
/// not kill the service, which would then sit out its restart delay.
pub(crate) const MAX_CONSECUTIVE_READ_FAILURES: u32 = 5;

/// How long one `next_frame` may wait, at least.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// How often to summarise dropped frames. One line per window, not per
/// drop: under sustained load every frame can be a drop.
const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// The rate assumed when neither the camera nor the command line gives one.
const FALLBACK_FPS: f64 = 30.0;

/// Interval between `tf_static` republishes.
const TF_INTERVAL: Duration = Duration::from_secs(1);

/// What to publish, resolved from the command line.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) frame_topic: String,
    pub(crate) info_topic: String,
    pub(crate) jpeg_topic: String,
    pub(crate) h264_topic: String,
    pub(crate) tile_topics: Vec<String>,
    pub(crate) frame_id: String,
    /// JPEG quality, when JPEG is on.
    pub(crate) jpeg: Option<u8>,
    pub(crate) h264: bool,
    /// Tiles requested.
    pub(crate) tiles: bool,
    /// Turn tiles on above 1080p even when not requested.
    pub(crate) auto_tiles: bool,
    pub(crate) tile_fps: u32,
    pub(crate) bitrate: Bitrate,
    pub(crate) stream_size: (u32, u32),
    pub(crate) record: Option<PathBuf>,
    /// The rate requested, for a camera that cannot report its own.
    pub(crate) requested_fps: Option<f64>,
    pub(crate) info: CameraInfoFields,
    pub(crate) tf: TfStaticFields,
    pub(crate) tracy: bool,
    /// Stop after this many captured frames.
    pub(crate) max_frames: Option<u64>,
    /// Stop after capturing for this long.
    pub(crate) max_duration: Option<Duration>,
}

/// A frame on its way to an encoder thread, with its stamp.
type Job = (Arc<Frame>, Time);

/// Opens the source again after it stopped delivering; `None` when a
/// shutdown came first.
pub(crate) type Reopen<'a> = &'a mut dyn FnMut() -> Result<Option<Opened>, Box<dyn Error>>;

/// What the capture loop carries across a reopened camera.
#[derive(Debug)]
struct Progress {
    /// Added to the camera's `seq`, so `seq` continues across a reopen.
    seq_base: u64,
    /// The `seq` after the last one published.
    next_seq: u64,
    /// Frames captured in all.
    captured: u64,
    /// Frames the current camera delivered.
    since_open: u64,
    /// When `max_duration` runs out.
    deadline: Option<Instant>,
}

/// Runs the pipeline until `shutdown` is set, the camera fails, or a file
/// source ends.
///
/// A camera that delivered frames and then stops (read timeouts, a
/// disconnect, an ISP restart) is closed and opened again with `reopen`
/// while the encoders keep running, when the new camera delivers the same
/// stream.
///
/// # Errors
///
/// Startup failures (no encoder, an unwritable recording, a stream larger
/// than the capture) and camera failures that a reopen does not recover.
pub(crate) fn run(
    opened: Opened,
    session: &Session,
    settings: &Settings,
    open_encoder: OpenEncoder,
    mut reopen: Option<Reopen<'_>>,
    shutdown: &AtomicBool,
) -> Result<(), Box<dyn Error>> {
    let Opened {
        mut camera,
        kind,
        software_mirror,
    } = opened;
    let config = camera.config().clone();
    let fps = match config.frame_rate.or(settings.requested_fps) {
        Some(fps) => fps,
        None => {
            warn!("the camera reports no frame rate; assuming {FALLBACK_FPS} fps");
            FALLBACK_FPS
        }
    };
    info!("Capturing {}x{} at {fps} fps", config.width, config.height);

    let file = kind == SourceKind::File;
    // A recording's H.264 is forwarded as recorded, and its other outputs
    // keep its size.
    let stream = if file {
        (config.width, config.height)
    } else {
        settings.stream_size
    };
    let encodes = settings.h264 || settings.jpeg.is_some();
    if !file && encodes && (stream.0 > config.width || stream.1 > config.height) {
        return Err(Failure::new(
            Status::InvalidArgs,
            format!(
                "STREAM_SIZE {}x{} is larger than capture size {}x{}; lower STREAM_SIZE, or \
                 raise CAMERA_SIZE / choose a larger CAMERA_MODE that the sensor supports.",
                stream.0, stream.1, config.width, config.height
            ),
        )
        .into());
    }
    let tiles = tiles_enabled(settings.tiles, settings.auto_tiles, &config);

    let mut threads: Vec<JoinHandle<()>> = Vec::new();
    let mut sinks: Vec<(Sender<Job>, &'static str)> = Vec::new();

    let recorder = match &settings.record {
        Some(path) if settings.h264 && !file => Some(open_recording(
            path,
            fps,
            stream,
            &config,
            &settings.info,
            &settings.tf,
        )?),
        _ => None,
    };

    if settings.h264 && !file {
        let plan = Plan {
            label: "h264".into(),
            size: stream,
            region: None,
            mirror: software_mirror,
            frame_rate: fps,
            bitrate: settings.bitrate,
            limit_fps: 0,
        };
        let stage = Stage::new(plan, &config, open_encoder)?;
        let (tx, rx) = kanal::bounded(H264_QUEUE);
        let out = Output::new(session, &settings.h264_topic, &settings.frame_id)?;
        let tracy = settings.tracy;
        threads.push(spawn("h264", move || {
            run_stage(stage, &rx, out, recorder, tracy)
        })?);
        sinks.push((tx, "h264"));
    }

    if tiles {
        let size = encode::tile_output(config.width, config.height);
        let tile_fps = settings.tile_fps;
        for (i, name) in TILE_NAMES.iter().enumerate() {
            let limited = tile_fps > 0 && f64::from(tile_fps) < fps;
            let plan = Plan {
                label: format!("h264/{name}"),
                size,
                region: Some(encode::tile_region(
                    i,
                    config.width,
                    config.height,
                    software_mirror,
                )),
                mirror: software_mirror,
                frame_rate: if limited { f64::from(tile_fps) } else { fps },
                bitrate: settings.bitrate,
                limit_fps: tile_fps,
            };
            let stage = Stage::new(plan, &config, open_encoder)?;
            let (tx, rx) = kanal::bounded(H264_QUEUE);
            let frame_id = format!("{}_{}", settings.frame_id, tile_frame_suffix(i));
            let out = Output::new(session, &settings.tile_topics[i], &frame_id)?;
            let tracy = settings.tracy;
            threads.push(spawn(&format!("h264_{name}"), move || {
                run_stage(stage, &rx, out, None, tracy)
            })?);
            sinks.push((tx, TILE_SINKS[i]));
        }
    }

    if let Some(quality) = settings.jpeg {
        let stage = JpegStage::new(&config, stream, software_mirror, quality)?;
        let (tx, rx) = kanal::bounded(JPEG_QUEUE);
        let out = Output::new(session, &settings.jpeg_topic, &settings.frame_id)?;
        let tracy = settings.tracy;
        threads.push(spawn("jpeg", move || run_jpeg(stage, &rx, out, tracy))?);
        sinks.push((tx, "jpeg"));
    }

    let forward = if settings.h264 && file {
        Some(Output::new(
            session,
            &settings.h264_topic,
            &settings.frame_id,
        )?)
    } else {
        None
    };

    let done = Arc::new(AtomicBool::new(false));
    let tf = {
        let session = session.clone();
        let fields = settings.tf.clone();
        let done = Arc::clone(&done);
        spawn("tf_static", move || republish_tf(&session, &fields, &done))?
    };

    if !file {
        discard_waiting(camera.as_mut());
    }
    let mut progress = Progress {
        seq_base: 0,
        next_seq: 0,
        captured: 0,
        since_open: 0,
        deadline: settings.max_duration.map(|d| Instant::now() + d),
    };
    let mut camera = Some(camera);
    let result = loop {
        let Some(current) = camera.as_deref_mut() else {
            break Ok(());
        };
        let result = capture(
            current,
            session,
            settings,
            &config,
            fps,
            &sinks,
            forward.as_ref(),
            &mut progress,
            shutdown,
        );
        let Err(e) = result else {
            break Ok(());
        };
        let Some(reopen) = reopen
            .as_mut()
            .filter(|_| progress.since_open > 0 && recoverable(&*e))
        else {
            break Err(e);
        };
        warn!("the camera stopped delivering ({e}); reopening it");
        if let Some(mut old) = camera.take() {
            if let Err(e) = old.stop() {
                debug!("stopping the camera: {e}");
            }
        }
        match reopen() {
            Ok(Some(next)) => {
                if let Err(e) = same_stream(&config, software_mirror, &next) {
                    break Err(e);
                }
                let mut next = next.camera;
                discard_waiting(next.as_mut());
                info!("Camera reopened; capturing again");
                progress.seq_base = progress.next_seq;
                progress.since_open = 0;
                camera = Some(next);
            }
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };

    drop(sinks);
    for t in threads {
        if t.join().is_err() {
            error!("an encoder thread panicked");
        }
    }
    done.store(true, Ordering::SeqCst);
    let _ = tf.join();
    if let Some(mut camera) = camera {
        if let Err(e) = camera.stop() {
            warn!("stopping the camera: {e}");
        }
    }
    result
}

/// Whether a reopen may recover the camera failure `e`.
fn recoverable(e: &(dyn Error + 'static)) -> bool {
    e.downcast_ref::<edgefirst_camera::Error>()
        .is_some_and(|e| {
            matches!(
                e.kind(),
                ErrorKind::Timeout | ErrorKind::Disconnected | ErrorKind::NotReady | ErrorKind::Io
            )
        })
}

/// Refuses a reopened camera whose stream differs from the one the
/// encoders were set up for.
fn same_stream(
    config: &StreamConfig,
    software_mirror: Option<Mirror>,
    next: &Opened,
) -> Result<(), Box<dyn Error>> {
    let got = next.camera.config();
    if (got.width, got.height, got.format) != (config.width, config.height, config.format)
        || next.software_mirror != software_mirror
    {
        return Err(format!(
            "the reopened camera delivers {}x{} {}, mirrored in software: {:?}, where the \
             stream is {}x{} {}, mirrored in software: {:?}",
            got.width,
            got.height,
            got.format,
            next.software_mirror,
            config.width,
            config.height,
            config.format,
            software_mirror
        )
        .into());
    }
    Ok(())
}

/// Discards the frames captured while the encoders started: they are
/// stale, and the gap behind them is no capture loss.
fn discard_waiting(camera: &mut dyn Camera) {
    let mut stale = 0;
    while camera.next_frame(Some(Duration::ZERO)).is_ok() {
        stale += 1;
    }
    if stale > 0 {
        debug!("discarded {stale} frames captured while the encoders started");
    }
}

/// Names of the tile sinks, for drop reports. Static so the hot path does
/// not allocate a label per frame per tile.
const TILE_SINKS: [&str; 4] = ["h264/tl", "h264/tr", "h264/bl", "h264/br"];

/// The `frame_id` suffix of tile `i`, as camera 2.x named them.
fn tile_frame_suffix(i: usize) -> &'static str {
    ["topleft", "topright", "bottomleft", "bottomright"][i]
}

/// Tiles are on when requested, and with `auto` above 1080p, where the
/// main stream is scaled down.
fn tiles_enabled(requested: bool, auto: bool, config: &StreamConfig) -> bool {
    let (w, h) = (config.width, config.height);
    if auto && h > 1080 && !requested {
        info!("Camera resolution {w}x{h} exceeds 1080p, automatically enabling H264 tiling");
        true
    } else {
        if requested {
            info!("H264 tiling enabled for {w}x{h}");
        }
        requested
    }
}

/// Frames an H.264 channel queues. A queued frame holds its capture
/// buffer, so the queue stays short; one absorbs encoder jitter.
const H264_QUEUE: usize = 1;

/// Frames the JPEG channel queues. One keeps the compressor busy: it takes
/// longer than a frame period, and waiting for the next frame after each
/// one would round its rate down to every other frame.
const JPEG_QUEUE: usize = 1;

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>, Box<dyn Error>> {
    Ok(thread::Builder::new().name(name.to_string()).spawn(f)?)
}

/// Opens the recording and writes its sidecar, before any frame flows, so
/// a recording that cannot start fails the service without leaving an
/// orphaned sidecar.
fn open_recording(
    path: &PathBuf,
    fps: f64,
    stream: (u32, u32),
    config: &StreamConfig,
    info: &CameraInfoFields,
    tf: &TfStaticFields,
) -> Result<BufWriter<File>, Box<dyn Error>> {
    let file = File::create(path)
        .map_err(|e| format!("cannot create recording file {}: {e}", path.display()))?;
    let mut sidecar = Sidecar::new(fps, stream.0, stream.1);
    sidecar.colorimetry = config.colorimetry;
    sidecar.extras = serde_json::json!({ "camera_info": info, "tf_static": tf }).to_string();
    let written = sidecar.write(path)?;
    info!(
        "Recording: H.264 → {}, sidecar → {}",
        path.display(),
        written.display()
    );
    Ok(BufWriter::with_capacity(256 * 1024, file))
}

/// A topic and the `frame_id` its messages carry.
struct Output {
    session: Session,
    publisher: Publisher<'static>,
    frame_id: String,
}

impl Output {
    fn new(session: &Session, topic: &str, frame_id: &str) -> Result<Self, Box<dyn Error>> {
        let publisher = session
            .declare_publisher(topic.to_string())
            .priority(Priority::Data)
            .congestion_control(CongestionControl::Drop)
            .wait()
            .map_err(|e| format!("cannot declare publisher {topic}: {e}"))?;
        Ok(Self {
            session: session.clone(),
            publisher,
            frame_id: frame_id.to_string(),
        })
    }

    fn put(&self, (bytes, encoding): (ZBytes, Encoding), stamp: Time) {
        let ts = sample_timestamp(&self.session, stamp);
        if let Err(e) = self
            .publisher
            .put(bytes)
            .encoding(encoding)
            .timestamp(ts)
            .wait()
        {
            warn!("publishing {}: {e}", self.publisher.key_expr());
        }
    }

    fn video(&self, data: &[u8], stamp: Time) {
        match video_msg(data, stamp, &self.frame_id) {
            Ok(msg) => self.put(msg, stamp),
            Err(e) => error!("building {}: {e}", self.publisher.key_expr()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn capture(
    camera: &mut dyn Camera,
    session: &Session,
    settings: &Settings,
    config: &StreamConfig,
    fps: f64,
    sinks: &[(Sender<Job>, &'static str)],
    forward: Option<&Output>,
    progress: &mut Progress,
    shutdown: &AtomicBool,
) -> Result<(), Box<dyn Error>> {
    let frames = session
        .declare_publisher(settings.frame_topic.clone())
        .priority(Priority::Data)
        .congestion_control(CongestionControl::Drop)
        .wait()
        .map_err(|e| format!("cannot declare publisher {}: {e}", settings.frame_topic))?;
    let infos = session
        .declare_publisher(settings.info_topic.clone())
        .priority(Priority::Background)
        .congestion_control(CongestionControl::Drop)
        .wait()
        .map_err(|e| format!("cannot declare publisher {}: {e}", settings.info_topic))?;
    let frame_enc = Encoding::APPLICATION_CDR.with_schema("edgefirst_msgs/msg/CameraFrame");
    let info_enc = Encoding::APPLICATION_CDR.with_schema("sensor_msgs/msg/CameraInfo");

    let timeout = READ_TIMEOUT.max(Duration::from_secs_f64(3.0 / fps));
    let mut retry = ReadRetry::new(MAX_CONSECUTIVE_READ_FAILURES);
    let mut drops = DropStats::new(DROP_REPORT_INTERVAL, Instant::now());
    let mut rate = Rate::new();
    // Frames the camera lost before the first one read (while the encoders
    // started) are not capture losses; the baseline is the first frame's.
    let mut lost: Option<u64> = None;
    let mut cdr = Vec::new();

    while !shutdown.load(Ordering::SeqCst) {
        if settings
            .max_frames
            .is_some_and(|max| progress.captured >= max)
            || progress.deadline.is_some_and(|d| Instant::now() >= d)
        {
            info!("Captured {} frames; stopping", progress.captured);
            break;
        }
        let frame = match info_span!("camera_read").in_scope(|| camera.next_frame(Some(timeout))) {
            Ok(frame) => {
                retry.on_success();
                frame
            }
            Err(e) => match e.kind() {
                ErrorKind::EndOfStream => {
                    info!("End of the recording");
                    break;
                }
                ErrorKind::Interrupted => continue,
                ErrorKind::Timeout if retry.should_retry() => {
                    warn!(
                        attempt = retry.consecutive(),
                        max = MAX_CONSECUTIVE_READ_FAILURES,
                        "no frame within {timeout:?}, retrying"
                    );
                    continue;
                }
                _ => {
                    error!("camera read failed: {e}");
                    return Err(e.into());
                }
            },
        };
        progress.captured += 1;
        progress.since_open += 1;
        let seq = progress.seq_base + frame.seq();
        progress.next_seq = seq + 1;
        if settings.tracy {
            plot!("fps", rate.update(Instant::now()));
        }

        // Every representation of this frame carries this one stamp; the
        // encoder threads must not take another, or a clock step between
        // them would split the frame across two time domains.
        let stamp = schema::stamp(&frame).unwrap_or_else(schema::now);
        let ts = sample_timestamp(session, stamp);

        info_span!("camera_publish").in_scope(|| -> Result<(), Box<dyn Error>> {
            let tensor = FrameTensor::new(&frame, config.colorimetry.as_ref())?;
            cdr.clear();
            tensor.with_fields(|fields| {
                CameraFrame::builder()
                    .stamp(stamp)
                    .frame_id(settings.frame_id.as_str())
                    .seq(seq)
                    .tensor(fields)
                    .encode_into_vec(&mut cdr)
            })?;
            if let Err(e) = frames
                .put(ZBytes::from(cdr.as_slice()))
                .encoding(frame_enc.clone())
                .timestamp(ts)
                .wait()
            {
                warn!("publishing {}: {e}", settings.frame_topic);
            }
            let info = settings.info.build_msg(stamp)?;
            if let Err(e) = infos
                .put(ZBytes::from(info.into_cdr()))
                .encoding(info_enc.clone())
                .timestamp(ts)
                .wait()
            {
                warn!("publishing {}: {e}", settings.info_topic);
            }
            Ok(())
        })?;

        if let (Some(out), Some(au)) = (forward, frame.encoded()) {
            out.video(au, stamp);
        }

        let frame = Arc::new(frame);
        for (tx, sink) in sinks {
            record_send_outcome(tx.try_send((Arc::clone(&frame), stamp)), sink, &mut drops);
        }
        drop(frame);

        let dropped = camera.stats().dropped;
        drops.record_camera(dropped.saturating_sub(lost.unwrap_or(dropped)));
        lost = Some(dropped);
        if let Some(report) = drops.take_report(Instant::now()) {
            warn!("{report}");
        }
        if settings.tracy {
            frame_mark();
        }
    }
    Ok(())
}

fn run_stage(
    mut stage: Stage,
    rx: &Receiver<Job>,
    out: Output,
    mut recorder: Option<BufWriter<File>>,
    tracy: bool,
) {
    let publish = |au: EncodedFrame, stamp: Time, recorder: &mut Option<BufWriter<File>>| {
        if let Some(w) = recorder.as_mut() {
            if let Err(e) = write_recording(w, &au.data, au.keyframe) {
                error!("h264 recorder write failed: {e}");
            }
        }
        if tracy {
            plot!("h264_kb", au.data.len() as f64 / 1024.0);
        }
        out.video(&au.data, stamp);
    };
    while let Ok((frame, stamp)) = rx.recv() {
        if !stage.accept(Instant::now()) {
            continue;
        }
        let span = info_span!("h264", stage = stage.label());
        match span.in_scope(|| stage.encode(frame, stamp)) {
            Ok(aus) => aus
                .into_iter()
                .for_each(|(au, stamp)| publish(au, stamp, &mut recorder)),
            Err(e) => error!("{} encode failed: {e}", stage.label()),
        }
        if tracy {
            secondary_frame_mark!("h264");
        }
    }
    match stage.flush() {
        Ok(aus) => aus
            .into_iter()
            .for_each(|(au, stamp)| publish(au, stamp, &mut recorder)),
        Err(e) => warn!("{}: flushing the encoder: {e}", stage.label()),
    }
    if let Some(mut w) = recorder.take() {
        if let Err(e) = w.flush() {
            error!("h264 recorder final flush failed: {e}");
        }
    }
}

fn run_jpeg(mut stage: JpegStage, rx: &Receiver<Job>, out: Output, tracy: bool) {
    while let Ok((frame, stamp)) = rx.recv() {
        let jpeg = match info_span!("jpeg").in_scope(|| stage.encode(frame)) {
            Ok(jpeg) => jpeg,
            Err(e) => {
                error!("jpeg encode failed: {e}");
                continue;
            }
        };
        if tracy {
            plot!("jpeg_kb", (jpeg.len() / 1024) as f64);
        }
        match jpeg_msg(&jpeg, stamp, &out.frame_id) {
            Ok(msg) => out.put(msg, stamp),
            Err(e) => error!("building the JPEG message: {e}"),
        }
        if tracy {
            secondary_frame_mark!("jpeg");
        }
    }
}

/// Republishes the camera transform every second, stamped at each
/// republish so it follows the wall clock.
fn republish_tf(session: &Session, fields: &TfStaticFields, done: &AtomicBool) {
    const TICK: Duration = Duration::from_millis(100);
    let enc = Encoding::APPLICATION_CDR.with_schema("geometry_msgs/msg/TransformStamped");
    let mut next = Instant::now();
    while !done.load(Ordering::SeqCst) {
        if Instant::now() >= next {
            next += TF_INTERVAL;
            let stamp = schema::now();
            match fields.build_msg(stamp) {
                Ok(msg) => {
                    if let Err(e) = session
                        .put("tf_static", ZBytes::from(msg.into_cdr()))
                        .encoding(enc.clone())
                        .timestamp(sample_timestamp(session, stamp))
                        .wait()
                    {
                        warn!("publishing tf_static: {e}");
                    }
                }
                Err(e) => {
                    error!("building tf_static: {e}");
                    return;
                }
            }
        }
        thread::sleep(TICK.min(next.saturating_duration_since(Instant::now())));
    }
}

fn write_recording(writer: &mut impl Write, data: &[u8], flush: bool) -> std::io::Result<()> {
    writer.write_all(data)?;
    if flush {
        writer.flush()?;
    }
    Ok(())
}

/// Frame rate over the last 60 frames, for the profiler.
struct Rate {
    prev: Instant,
    history: [f64; 60],
    index: usize,
}

impl Rate {
    fn new() -> Self {
        Self {
            prev: Instant::now(),
            history: [0.0; 60],
            index: 0,
        }
    }

    fn update(&mut self, now: Instant) -> f64 {
        self.history[self.index] = now.duration_since(self.prev).as_nanos() as f64;
        self.prev = now;
        self.index = (self.index + 1) % self.history.len();
        1e9 / (self.history.iter().sum::<f64>() / self.history.len() as f64)
    }
}

/// Counters for frames lost: by the camera (from its capture stats), and
/// discarded because an encoder channel was full.
///
/// Encoder drops are expected under load, and dropping is the right
/// response; counting them is what lets an operator see that a recording
/// has holes. Reports are rate-limited to one line per interval.
pub(crate) struct DropStats {
    dropped: BTreeMap<&'static str, u64>,
    camera: u64,
    sent: u64,
    interval: Duration,
    last_report: Instant,
}

impl DropStats {
    pub(crate) fn new(interval: Duration, now: Instant) -> Self {
        Self {
            dropped: BTreeMap::new(),
            camera: 0,
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

    /// Frames the camera lost since the last call.
    pub(crate) fn record_camera(&mut self, lost: u64) {
        self.camera = self.camera.saturating_add(lost);
    }

    /// A one-line summary of the window just ended, or `None` while the
    /// interval has not elapsed or nothing was lost. Taking a report resets
    /// the window.
    pub(crate) fn take_report(&mut self, now: Instant) -> Option<String> {
        if now.duration_since(self.last_report) < self.interval {
            return None;
        }
        self.last_report = now;
        let mut parts = Vec::new();
        if self.camera > 0 {
            parts.push(format!("camera dropped {} frames", self.camera));
        }
        if !self.dropped.is_empty() {
            let total: u64 = self.dropped.values().sum();
            let per_sink = self
                .dropped
                .iter()
                .map(|(sink, n)| format!("{sink}={n}"))
                .collect::<Vec<_>>()
                .join(" ");
            parts.push(format!(
                "dropped {total} of {} frames (encoder channels full): {per_sink}",
                total + self.sent
            ));
        }
        self.dropped.clear();
        self.sent = 0;
        self.camera = 0;
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// Records whether a `try_send` delivered the frame. kanal reports a full
/// channel as `Ok(false)`, not as an error; `Err` means it is closed.
fn record_send_outcome(
    outcome: Result<bool, kanal::SendError>,
    sink: &'static str,
    stats: &mut DropStats,
) {
    match outcome {
        Ok(true) => stats.record_sent(),
        Ok(false) | Err(_) => stats.record_drop(sink),
    }
}

/// Retry budget for consecutive `next_frame` timeouts.
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

    /// Records a failure. Returns `true` while the budget allows another
    /// attempt, `false` once `max` consecutive failures are recorded.
    pub(crate) fn should_retry(&mut self) -> bool {
        self.consecutive = self.consecutive.saturating_add(1);
        self.consecutive < self.max
    }

    pub(crate) fn consecutive(&self) -> u32 {
        self.consecutive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::tests::open_fake;
    use crate::source::{self, Request};
    use clap::Parser;
    use edgefirst_schemas::{
        foxglove_msgs::FoxgloveCompressedVideo,
        geometry_msgs::TransformStamped,
        sensor_msgs::{CameraInfo, CompressedImage},
    };
    use std::collections::{BTreeSet, HashMap};
    use std::sync::Mutex;

    #[test]
    fn kanal_reports_a_full_channel_as_ok_false() {
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
        let report = stats.take_report(t0 + Duration::from_secs(11)).unwrap();
        assert!(report.contains("h264=2"), "{report}");
        assert!(report.contains("of 3 frames"), "{report}");
    }

    #[test]
    fn drop_stats_counts_per_sink_and_reports_after_the_interval() {
        let t0 = Instant::now();
        let mut stats = DropStats::new(Duration::from_secs(10), t0);
        stats.record_sent();
        stats.record_drop("h264/tl");
        stats.record_drop("h264/tl");
        stats.record_drop("jpeg");
        assert!(stats.take_report(t0 + Duration::from_secs(1)).is_none());
        let report = stats.take_report(t0 + Duration::from_secs(11)).unwrap();
        assert!(report.contains("h264/tl=2"), "{report}");
        assert!(report.contains("jpeg=1"), "{report}");
    }

    #[test]
    fn drop_stats_reports_camera_drops() {
        let t0 = Instant::now();
        let mut stats = DropStats::new(Duration::from_secs(10), t0);
        stats.record_camera(3);
        stats.record_camera(0);
        let report = stats.take_report(t0 + Duration::from_secs(11)).unwrap();
        assert_eq!(report, "camera dropped 3 frames");
        stats.record_sent();
        assert!(stats.take_report(t0 + Duration::from_secs(22)).is_none());
    }

    #[test]
    fn drop_stats_resets_the_window_after_reporting() {
        let t0 = Instant::now();
        let mut stats = DropStats::new(Duration::from_secs(10), t0);
        stats.record_drop("jpeg");
        stats.take_report(t0 + Duration::from_secs(11)).unwrap();
        stats.record_drop("jpeg");
        let second = stats.take_report(t0 + Duration::from_secs(22)).unwrap();
        assert!(second.contains("jpeg=1"), "{second}");
    }

    #[test]
    fn read_retry_spends_its_budget_then_resets() {
        let mut retry = ReadRetry::new(2);
        assert!(retry.should_retry());
        retry.on_success();
        assert_eq!(retry.consecutive(), 0);
        assert!(retry.should_retry());
        assert!(!retry.should_retry());
    }

    #[test]
    fn recording_write_appends_encoded_frame() {
        let mut output = Vec::new();
        write_recording(&mut output, b"encoded frame", false).unwrap();
        assert_eq!(output, b"encoded frame");
    }

    /// A Zenoh session that only talks to itself.
    fn local_session() -> Session {
        let mut config = zenoh::Config::default();
        for (key, value) in [
            ("mode", "\"peer\""),
            ("listen/endpoints", "[]"),
            ("connect/endpoints", "[]"),
            ("scouting/multicast/enabled", "false"),
            ("scouting/gossip/enabled", "false"),
        ] {
            config.insert_json5(key, value).unwrap();
        }
        zenoh::open(config).wait().unwrap()
    }

    /// Every sample received, by topic: (header stamp, sample timestamp in
    /// nanoseconds, payload).
    type Received = Arc<Mutex<HashMap<String, Vec<(Time, u128, Vec<u8>)>>>>;

    fn header_stamp(topic: &str, payload: &[u8]) -> Time {
        match topic {
            "camera/frame" => CameraFrame::from_cdr(payload).unwrap().stamp(),
            "camera/info" => CameraInfo::from_cdr(payload).unwrap().stamp(),
            "camera/jpeg" => CompressedImage::from_cdr(payload).unwrap().stamp(),
            "tf_static" => TransformStamped::from_cdr(payload).unwrap().stamp(),
            _ => FoxgloveCompressedVideo::from_cdr(payload).unwrap().stamp(),
        }
    }

    fn nanos(t: Time) -> u128 {
        t.sec as u128 * 1_000_000_000 + u128::from(t.nanosec)
    }

    fn settings(args: &[&str]) -> (Request, Settings) {
        let args = crate::args::Args::parse_from([&["edgefirst-camera"][..], args].concat());
        let request = Request::from_args(&args, |_| unreachable!()).unwrap();
        let settings = crate::settings(&args, &request, None);
        (request, settings)
    }

    /// Runs the pipeline on `args` until `frames` frames are published, and
    /// returns what every topic received.
    fn run_until(args: &[&str], frames: usize) -> HashMap<String, Vec<(Time, u128, Vec<u8>)>> {
        let (request, settings) = settings(args);
        let session = local_session();
        let received: Received = Arc::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        let _subscriber = {
            let received = Arc::clone(&received);
            let shutdown = Arc::clone(&shutdown);
            session
                .declare_subscriber("**")
                .callback(move |sample| {
                    let topic = sample.key_expr().to_string();
                    let ts = sample
                        .timestamp()
                        .map(|t| t.get_time().to_duration().as_nanos())
                        .unwrap_or_default();
                    let payload = sample.payload().to_bytes().to_vec();
                    let stamp = header_stamp(&topic, &payload);
                    let mut all = received.lock().unwrap();
                    let list = all.entry(topic.clone()).or_default();
                    list.push((stamp, ts, payload));
                    if topic == "camera/frame" && list.len() >= frames {
                        shutdown.store(true, Ordering::SeqCst);
                    }
                })
                .wait()
                .unwrap()
        };
        let processor = edgefirst_image::ImageProcessor::new().ok();
        let opened = source::open(&request, processor.as_ref(), &shutdown)
            .unwrap()
            .unwrap();
        run(opened, &session, &settings, open_fake, None, &shutdown).unwrap();
        // tf_static is republished every second.
        let deadline = Instant::now() + Duration::from_secs(3);
        while !received.lock().unwrap().contains_key("tf_static") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        let all = received.lock().unwrap().clone();
        all
    }

    /// Every topic on a mock camera: each frame's messages share its
    /// stamp, and every sample timestamp is its header stamp.
    #[test]
    fn every_topic_carries_its_frames_one_stamp() {
        let all = run_until(
            &[
                "--source",
                "mock:320x240@30",
                "--stream-size",
                "320",
                "240",
                "--jpeg",
                "--h264-tiles",
                "--h264-tiles-fps",
                "0",
            ],
            30,
        );
        let topics: BTreeSet<&str> = all.keys().map(String::as_str).collect();
        for want in [
            "camera/frame",
            "camera/info",
            "camera/jpeg",
            "camera/h264",
            "camera/h264/tl",
            "camera/h264/tr",
            "camera/h264/bl",
            "camera/h264/br",
            "tf_static",
        ] {
            assert!(topics.contains(want), "no {want} in {topics:?}");
        }
        for (topic, samples) in &all {
            for (stamp, ts, _) in samples {
                let diff = nanos(*stamp).abs_diff(*ts);
                assert!(
                    diff <= 2,
                    "{topic}: sample timestamp {ts} vs header {stamp:?}"
                );
            }
        }
        let frames: BTreeSet<u128> = all["camera/frame"].iter().map(|s| nanos(s.0)).collect();
        assert_eq!(
            frames.len(),
            all["camera/frame"].len(),
            "one stamp per frame"
        );
        let infos: BTreeSet<u128> = all["camera/info"].iter().map(|s| nanos(s.0)).collect();
        assert_eq!(infos, frames, "camera/info is stamped per frame");
        for (topic, samples) in &all {
            if topic == "tf_static" {
                continue;
            }
            for (stamp, _, _) in samples {
                assert!(
                    frames.contains(&nanos(*stamp)),
                    "{topic} stamp {stamp:?} is no frame's"
                );
            }
        }
        // The fake encoder's access unit is its frame's pts (seq): the
        // stamp it carries is that frame's.
        let by_seq: HashMap<u64, u128> = all["camera/frame"]
            .iter()
            .map(|(stamp, _, p)| {
                (
                    CameraFrame::from_cdr(p.as_slice()).unwrap().seq(),
                    nanos(*stamp),
                )
            })
            .collect();
        for (stamp, _, payload) in &all["camera/h264"] {
            let msg = FoxgloveCompressedVideo::from_cdr(payload.as_slice()).unwrap();
            let seq = u64::from_le_bytes(msg.data().try_into().unwrap());
            if let Some(&frame_stamp) = by_seq.get(&seq) {
                assert_eq!(frame_stamp, nanos(*stamp), "access unit {seq}");
            }
        }
        let jpeg = CompressedImage::from_cdr(all["camera/jpeg"][0].2.as_slice()).unwrap();
        assert_eq!(jpeg.format(), "jpeg");
        let header = turbojpeg::read_header(jpeg.data()).unwrap();
        assert_eq!((header.width, header.height), (320, 240));
    }

    fn mock_opened(fault: &str) -> Opened {
        let mut camera = edgefirst_camera::CameraBuilder::source("mock:64x48@120")
            .unwrap()
            .fault(fault)
            .open()
            .unwrap();
        camera.start().unwrap();
        Opened {
            camera,
            kind: SourceKind::Synthetic,
            software_mirror: None,
        }
    }

    /// The `seq` of every `camera/frame` published until `count` arrive.
    fn frame_seqs(
        session: &Session,
        count: usize,
        shutdown: &Arc<AtomicBool>,
    ) -> (Arc<Mutex<Vec<u64>>>, zenoh::pubsub::Subscriber<()>) {
        let seqs: Arc<Mutex<Vec<u64>>> = Arc::default();
        let subscriber = {
            let seqs = Arc::clone(&seqs);
            let shutdown = Arc::clone(shutdown);
            session
                .declare_subscriber("camera/frame")
                .callback(move |sample| {
                    let payload = sample.payload().to_bytes();
                    let mut seqs = seqs.lock().unwrap();
                    seqs.push(CameraFrame::from_cdr(&payload[..]).unwrap().seq());
                    if seqs.len() >= count {
                        shutdown.store(true, Ordering::SeqCst);
                    }
                })
                .wait()
                .unwrap()
        };
        (seqs, subscriber)
    }

    /// A camera that goes away after delivering is opened again, and the
    /// published `seq` runs on instead of restarting.
    #[test]
    #[cfg(debug_assertions)]
    fn a_camera_that_goes_away_is_reopened_and_seq_runs_on() {
        let (_, settings) = settings(&["--source", "mock:64x48@120", "--h264", "false"]);
        let session = local_session();
        let shutdown = Arc::new(AtomicBool::new(false));
        let (seqs, _subscriber) = frame_seqs(&session, 35, &shutdown);
        let mut reopens = 0;
        let mut reopen = || -> Result<Option<Opened>, Box<dyn Error>> {
            reopens += 1;
            Ok(Some(mock_opened("disconnect-after=10")))
        };
        run(
            mock_opened("disconnect-after=10"),
            &session,
            &settings,
            open_fake,
            Some(&mut reopen),
            &shutdown,
        )
        .unwrap();
        let seqs = seqs.lock().unwrap().clone();
        assert!(reopens >= 3, "{reopens} reopens");
        // Each camera's first frame is discarded as stale, as at startup,
        // so a reopen skips one number and never starts again from 0.
        assert!(
            seqs.windows(2).all(|w| w[1] > w[0] && w[1] - w[0] <= 2),
            "{seqs:?}"
        );
        assert!(seqs.last().unwrap() >= &30, "{seqs:?}");
    }

    /// A camera that fails before delivering a frame is not reopened: the
    /// failure ends the run, for the service manager to restart.
    #[test]
    #[cfg(debug_assertions)]
    fn a_camera_that_never_delivers_is_not_reopened() {
        let (_, settings) = settings(&["--source", "mock:64x48@120", "--h264", "false"]);
        let session = local_session();
        let shutdown = AtomicBool::new(false);
        let mut reopens = 0;
        let mut reopen = || -> Result<Option<Opened>, Box<dyn Error>> {
            reopens += 1;
            Ok(Some(mock_opened("disconnect-after=0")))
        };
        let err = run(
            mock_opened("disconnect-after=10"),
            &session,
            &settings,
            open_fake,
            Some(&mut reopen),
            &shutdown,
        )
        .unwrap_err();
        assert_eq!(reopens, 1, "reopened once, then gave up");
        assert!(err.to_string().contains("went away"), "{err}");
    }

    #[test]
    fn camera_frame_describes_the_tensor() {
        let all = run_until(
            &[
                "--source",
                "mock:64x48@30",
                "--h264",
                "false",
                "--camera-frame-id",
                "cam",
            ],
            3,
        );
        let msg = CameraFrame::from_cdr(all["camera/frame"][0].2.as_slice()).unwrap();
        assert_eq!(msg.frame_id(), "cam");
        let t = msg.tensor();
        assert_eq!(t.format(), "YUYV");
        assert_eq!(t.shape().collect::<Vec<_>>(), [48, 64, 2]);
        assert_eq!(t.num_planes(), 1);
        assert_eq!(t.pid(), std::process::id());
        assert!(!all.contains_key("camera/h264"));
        assert!(!all.contains_key("camera/jpeg"));
    }

    #[test]
    fn records_the_stream_with_its_sidecar() {
        let dir = std::env::temp_dir().join(format!("edgefirst-camera-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rec.h264");
        let p = path.to_str().unwrap();
        let all = run_until(
            &[
                "--source",
                "mock:64x48@30",
                "--stream-size",
                "64",
                "48",
                "--record",
                p,
                "--camera-frame-id",
                "rec",
            ],
            10,
        );
        let published: usize = all["camera/h264"]
            .iter()
            .map(|(_, _, payload)| {
                FoxgloveCompressedVideo::from_cdr(payload.as_slice())
                    .unwrap()
                    .data()
                    .len()
            })
            .sum();
        assert_eq!(std::fs::metadata(&path).unwrap().len() as usize, published);
        let sidecar = Sidecar::load(&path).unwrap();
        assert_eq!((sidecar.width, sidecar.height, sidecar.fps), (64, 48, 30.0));
        let extras: serde_json::Value = serde_json::from_str(&sidecar.extras).unwrap();
        assert_eq!(extras["camera_info"]["frame_id"], "rec");
        assert_eq!(extras["tf_static"]["child_frame_id"], "rec");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stream_larger_than_the_capture_is_refused() {
        let (request, settings) = settings(&["--source", "mock:64x48@30"]);
        let session = local_session();
        let flag = AtomicBool::new(false);
        let opened = source::open(&request, None, &flag).unwrap().unwrap();
        let err = run(opened, &session, &settings, open_fake, None, &flag).unwrap_err();
        assert!(err.to_string().contains("STREAM_SIZE 1920x1080"), "{err}");
    }

    #[test]
    fn tf_static_follows_the_command_line() {
        let all = run_until(
            &[
                "--source",
                "mock:64x48@30",
                "--h264",
                "false",
                "--base-frame-id",
                "base",
                "--camera-frame-id",
                "cam",
            ],
            3,
        );
        let tf = TransformStamped::from_cdr(all["tf_static"][0].2.as_slice()).unwrap();
        assert_eq!(tf.frame_id(), "base");
        assert_eq!(tf.child_frame_id(), "cam");
    }
}

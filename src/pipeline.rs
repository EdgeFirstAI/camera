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
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex, PoisonError,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use edgefirst_camera::{
    rates_differ,
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
    args::H264Fps,
    encode::{self, OpenEncoder, Plan, Stage, TILES},
    exit::{Failure, Status},
    jpeg::JpegStage,
    messages::{jpeg_msg, sample_timestamp, video_msg, CameraInfoFields, TfStaticFields},
    notify,
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

/// The rates `--h264-fps auto` sets the encoders up for.
const PRESET_FPS: [f64; 3] = [15.0, 30.0, 60.0];

/// How long `--h264-fps measure` watches the camera.
const MEASURE_TIMEOUT: Duration = Duration::from_secs(3);

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
    /// The rate the encoders are set up for.
    pub(crate) h264_fps: H264Fps,
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
    /// The current camera's measured rate as last reported.
    measured: Option<f64>,
    /// The recording's sidecar, corrected to the rate delivered.
    sidecar: Option<SidecarFile>,
    /// Whether systemd was told the service is ready.
    ready: bool,
    /// The encoder reported as stalled, so it is reported once.
    stalled: Option<&'static str>,
}

impl Progress {
    /// Whether `max_frames` or `max_duration` is reached.
    fn limit_reached(&self, settings: &Settings) -> bool {
        settings.max_frames.is_some_and(|max| self.captured >= max)
            || self.deadline.is_some_and(|d| Instant::now() >= d)
    }
}

/// A recording's sidecar and the stream it describes.
#[derive(Debug)]
struct SidecarFile {
    stream: PathBuf,
    sidecar: Sidecar,
    captured: CapturedSpan,
    /// Frames in the recording so far.
    recorded: Arc<AtomicU64>,
}

impl SidecarFile {
    /// Sets the sidecar's rate, unless it is already within the tolerance.
    fn correct(&mut self, fps: f64) {
        if !rates_differ(self.sidecar.fps, fps) {
            return;
        }
        self.sidecar.fps = fps;
        match self.sidecar.write(&self.stream) {
            Ok(path) => info!("Recording sidecar {} set to {fps} fps", path.display()),
            Err(e) => error!("cannot correct the recording sidecar: {e}"),
        }
    }

    /// Sets the sidecar's rate to the one at which the frames in the file last
    /// as long as the capture did. Only for when the encoder has drained: until
    /// then the frames it holds are not in the file yet.
    fn finish(&mut self) {
        let recorded = self.recorded.load(Ordering::Relaxed);
        if let Some(fps) = self.captured.recorded_fps(recorded) {
            self.correct(fps);
        }
    }
}

/// Where the main stream is recorded.
#[derive(Debug)]
struct Recorder {
    writer: BufWriter<File>,
    /// Frames written, one per access unit.
    recorded: Arc<AtomicU64>,
}

impl Recorder {
    fn write(&mut self, au: &EncodedFrame) -> std::io::Result<()> {
        write_recording(&mut self.writer, &au.data, au.keyframe)?;
        self.recorded.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// How long the frames of a recording took, without the time a camera was
/// away, for the mean rate that keeps a rate-changing recording's duration.
#[derive(Debug, Default)]
struct CapturedSpan {
    /// Frames captured, all of which were offered to the encoder.
    frames: u64,
    /// Intervals of the current camera not counted yet, which skip its start.
    settling: u32,
    last_nanos: Option<i64>,
    intervals: u64,
    nanos: i64,
}

impl CapturedSpan {
    /// Intervals at the start of a camera that may include the sensor
    /// starting up.
    const SETTLING: u32 = 2;
    /// Intervals below which a mean during a recording says more about the
    /// start than the rate. A finished recording has all it will ever have.
    const MIN_INTERVALS: u64 = 30;

    fn frame(&mut self, nanos: i64) {
        self.frames += 1;
        let Some(last) = self.last_nanos.replace(nanos) else {
            return;
        };
        if self.settling < Self::SETTLING {
            self.settling += 1;
        } else if nanos > last {
            self.intervals += 1;
            self.nanos += nanos - last;
        }
    }

    /// The next frame comes from a new camera, or after a gap in this one.
    fn camera_changed(&mut self) {
        self.settling = 0;
        self.last_nanos = None;
    }

    fn mean(&self) -> Option<f64> {
        (self.nanos > 0).then(|| round_fps(self.intervals as f64 * 1e9 / self.nanos as f64))
    }

    /// The mean rate so far, once there are enough intervals to trust.
    fn mean_fps(&self) -> Option<f64> {
        self.mean()
            .filter(|_| self.intervals >= Self::MIN_INTERVALS)
    }

    /// The mean rate of a finished recording, for the `recorded` of its frames
    /// that reached the file.
    fn recorded_fps(&self, recorded: u64) -> Option<f64> {
        let mean = self.mean()?;
        (recorded > 0).then(|| round_fps(mean * recorded as f64 / self.frames as f64))
    }
}

/// A rate rounded to hundredths, as the SDK reports them.
fn round_fps(fps: f64) -> f64 {
    (fps * 100.0).round() / 100.0
}

/// The rate the encoders are set up for, from `--h264-fps`.
fn encoder_rate(choice: H264Fps, reported: Option<f64>, camera: &mut dyn Camera) -> f64 {
    match choice {
        H264Fps::Fixed(fps) => fps,
        H264Fps::Auto => preset_for(reported),
        H264Fps::Measure => match measure(camera) {
            Some(fps) => {
                debug!("measured {fps} fps before setting up the encoders");
                snap(fps)
            }
            None => {
                warn!(
                    "the frame rate was not measured within {MEASURE_TIMEOUT:?}; \
                     using the preset for the reported rate"
                );
                preset_for(reported)
            }
        },
    }
}

/// The preset nearest the reported rate.
fn preset_for(reported: Option<f64>) -> f64 {
    let Some(fps) = reported else {
        warn!("the camera reports no frame rate; assuming {FALLBACK_FPS} fps");
        return FALLBACK_FPS;
    };
    PRESET_FPS
        .into_iter()
        .min_by(|a, b| (a - fps).abs().total_cmp(&(b - fps).abs()))
        .unwrap_or(FALLBACK_FPS)
}

/// A measured rate, as the preset it is within the tolerance of.
fn snap(fps: f64) -> f64 {
    PRESET_FPS
        .into_iter()
        .find(|p| !rates_differ(fps, *p))
        .unwrap_or(fps)
}

/// Reads frames, discarding them, until the camera has measured its rate.
fn measure(camera: &mut dyn Camera) -> Option<f64> {
    let deadline = Instant::now() + MEASURE_TIMEOUT;
    loop {
        if let Some(fps) = camera.measured_frame_rate() {
            return Some(fps);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || camera.next_frame(Some(left)).is_err() {
            return None;
        }
    }
}

/// Reports the rate the camera delivers against the one the encoders were
/// set up for, and corrects the recording's sidecar, so a recording plays back
/// at the speed it was captured: to the mean rate so far once there is one,
/// since a recording has one rate however often the camera's changed.
fn report_rate(measured: f64, used: f64, sidecar: Option<&mut SidecarFile>) {
    if rates_differ(used, measured) {
        info!("Measured {measured} fps; the encoders were set up for {used} fps");
    } else {
        info!("Measured {measured} fps");
    }
    if let Some(file) = sidecar {
        file.correct(file.captured.mean_fps().unwrap_or(measured));
    }
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
    let file = kind == SourceKind::File;
    let reported = config.frame_rate.or(settings.requested_fps);
    let fps = if file {
        // A recording plays at the rate it was recorded at.
        reported.unwrap_or(FALLBACK_FPS)
    } else {
        encoder_rate(settings.h264_fps, reported, camera.as_mut())
    };
    info!(
        "Capturing {}x{}; encoders at {fps} fps",
        config.width, config.height
    );

    // A recording's H.264 is forwarded as recorded, and its other outputs
    // keep its size.
    let stream = if file {
        (config.width, config.height)
    } else {
        settings.stream_size
    };
    if !file {
        check_stream_size(settings, stream, &config)?;
    }
    let tiles = tiles_enabled(
        settings.tiles,
        settings.auto_tiles && settings.h264,
        (config.width, config.height),
    );

    let mut sidecar = None;
    let recorder = match &settings.record {
        Some(path) if settings.h264 && !file => {
            let (writer, file) =
                open_recording(path, fps, stream, &config, &settings.info, &settings.tf)?;
            sidecar = Some(file);
            Some(writer)
        }
        _ => None,
    };

    // Dropped on every return, so an error while the later encoders start
    // still closes and joins the earlier ones.
    let mut workers = Workers::default();
    let encoders = Encoders {
        session,
        settings,
        config: &config,
        open_encoder,
        fps,
        stream,
        software_mirror,
    };
    if settings.h264 && !file {
        encoders.add_main_h264(&mut workers, recorder)?;
    }
    if tiles {
        encoders.add_tiles(&mut workers)?;
    }
    if let Some(quality) = settings.jpeg {
        encoders.add_jpeg(&mut workers, quality)?;
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
        measured: None,
        sidecar,
        ready: false,
        stalled: None,
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
            &workers.sinks,
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
        match replace_camera(reopen, &mut camera, &config, software_mirror, &mut progress) {
            Ok(true) => {}
            Ok(false) => break Ok(()),
            Err(e) => break Err(e),
        }
    };

    drop(workers);
    if let Some(file) = progress.sidecar.as_mut() {
        file.finish();
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

/// Refuses a stream larger than the capture: the encoders scale down only.
fn check_stream_size(
    settings: &Settings,
    stream: (u32, u32),
    config: &StreamConfig,
) -> Result<(), Box<dyn Error>> {
    let encodes = settings.h264 || settings.jpeg.is_some();
    if encodes && (stream.0 > config.width || stream.1 > config.height) {
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
    Ok(())
}

/// Stops `camera` and opens the next one in its place. `Ok(false)` when a
/// shutdown came first.
fn replace_camera(
    reopen: Reopen<'_>,
    camera: &mut Option<Box<dyn Camera>>,
    config: &StreamConfig,
    software_mirror: Option<Mirror>,
    progress: &mut Progress,
) -> Result<bool, Box<dyn Error>> {
    if let Some(mut old) = camera.take() {
        if let Err(e) = old.stop() {
            debug!("stopping the camera: {e}");
        }
    }
    let Some(next) = reopen()? else {
        return Ok(false);
    };
    same_stream(config, software_mirror, &next)?;
    let mut next = next.camera;
    discard_waiting(next.as_mut());
    info!("Camera reopened; capturing again");
    progress.seq_base = progress.next_seq;
    progress.since_open = 0;
    progress.measured = None;
    if let Some(file) = progress.sidecar.as_mut() {
        file.captured.camera_changed();
    }
    *camera = Some(next);
    Ok(true)
}

/// What every encoder of one run starts from.
struct Encoders<'a> {
    session: &'a Session,
    settings: &'a Settings,
    config: &'a StreamConfig,
    open_encoder: OpenEncoder,
    fps: f64,
    /// The size of the main stream and JPEG.
    stream: (u32, u32),
    software_mirror: Option<Mirror>,
}

impl Encoders<'_> {
    /// The main H.264 stream, recorded when `recorder` is set.
    fn add_main_h264(
        &self,
        workers: &mut Workers,
        recorder: Option<Recorder>,
    ) -> Result<(), Box<dyn Error>> {
        let plan = Plan {
            label: "h264".into(),
            size: self.stream,
            region: None,
            mirror: self.software_mirror,
            frame_rate: self.fps,
            bitrate: self.settings.bitrate,
            limit_fps: 0,
        };
        let s = self.settings;
        self.add_h264(
            workers,
            plan,
            &s.h264_topic,
            &s.frame_id,
            "h264",
            "h264",
            recorder,
        )
    }

    /// The four tiles, each at most 1080p and at the tile rate limit.
    fn add_tiles(&self, workers: &mut Workers) -> Result<(), Box<dyn Error>> {
        let (width, height) = (self.config.width, self.config.height);
        let size = encode::tile_output(width, height);
        let tile_fps = self.settings.tile_fps;
        let limited = tile_fps > 0 && f64::from(tile_fps) < self.fps;
        for (i, tile) in TILES.iter().enumerate() {
            let plan = Plan {
                label: tile.label.into(),
                size,
                region: Some(encode::tile_region(i, width, height, self.software_mirror)),
                mirror: self.software_mirror,
                frame_rate: if limited {
                    f64::from(tile_fps)
                } else {
                    self.fps
                },
                bitrate: self.settings.bitrate,
                limit_fps: tile_fps,
            };
            let frame_id = format!("{}_{}", self.settings.frame_id, tile.frame_suffix);
            let thread = format!("h264_{}", tile.name);
            let topic = &self.settings.tile_topics[i];
            self.add_h264(workers, plan, topic, &frame_id, tile.label, &thread, None)?;
        }
        Ok(())
    }

    /// One H.264 encoder thread publishing on `topic`.
    #[allow(clippy::too_many_arguments)]
    fn add_h264(
        &self,
        workers: &mut Workers,
        plan: Plan,
        topic: &str,
        frame_id: &str,
        sink: &'static str,
        thread: &str,
        recorder: Option<Recorder>,
    ) -> Result<(), Box<dyn Error>> {
        let stage = Stage::new(plan, self.config, self.open_encoder)?;
        let (tx, rx) = kanal::bounded(H264_QUEUE);
        let out = Output::new(self.session, topic, frame_id)?;
        let tracy = self.settings.tracy;
        let sink = Sink::new(sink, Queue::Fifo(tx));
        let heartbeat = Arc::clone(&sink.heartbeat);
        workers.threads.push(spawn(thread, move || {
            run_stage(stage, &rx, out, recorder, tracy, &heartbeat)
        })?);
        workers.sinks.push(sink);
        Ok(())
    }

    /// The JPEG encoder thread.
    fn add_jpeg(&self, workers: &mut Workers, quality: u8) -> Result<(), Box<dyn Error>> {
        let stage = JpegStage::new(self.config, self.stream, self.software_mirror, quality)?;
        let (tx, rx) = latest();
        let s = self.settings;
        let out = Output::new(self.session, &s.jpeg_topic, &s.frame_id)?;
        let tracy = s.tracy;
        let sink = Sink::new("jpeg", Queue::Latest(tx));
        let heartbeat = Arc::clone(&sink.heartbeat);
        workers.threads.push(spawn("jpeg", move || {
            run_jpeg(stage, &rx, out, tracy, &heartbeat)
        })?);
        workers.sinks.push(sink);
        Ok(())
    }
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

/// Discards the frames captured while the encoders started, which are stale,
/// and measures the rate from the frames after them.
fn discard_waiting(camera: &mut dyn Camera) {
    let mut stale = 0;
    while camera.next_frame(Some(Duration::ZERO)).is_ok() {
        stale += 1;
    }
    camera.restart_frame_rate_measurement();
    if stale > 0 {
        debug!("discarded {stale} frames captured while the encoders started");
    }
}

/// Tiles are on when requested, and with `auto` above 1080p, where the
/// main stream is scaled down. The caller allows `auto` only while H.264 is
/// on, so a camera above 1080p starts no encoder the user did not ask for.
fn tiles_enabled(requested: bool, auto: bool, (w, h): (u32, u32)) -> bool {
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

/// How long an encoder may go without finishing a frame before the service
/// stops telling the systemd watchdog it is alive.
const STAGE_STALL: Duration = Duration::from_secs(10);

/// Consecutive encode failures after which an encoder thread gives up,
/// which ends the service so systemd restarts it.
const ENCODE_FAILURE_LIMIT: u32 = 30;

/// Where the capture loop hands frames to one encoder thread.
struct Sink {
    name: &'static str,
    queue: Queue,
    heartbeat: Arc<Heartbeat>,
}

enum Queue {
    /// Frames wait in order; a full queue drops the new frame.
    Fifo(Sender<Job>),
    /// The newest frame waits; it replaces one that has not been taken.
    /// For an encoder slower than the camera, which should always work on
    /// the newest frame.
    Latest(LatestSender<Job>),
}

/// The encoder threads and the sinks that feed them.
#[derive(Default)]
struct Workers {
    sinks: Vec<Sink>,
    threads: Vec<JoinHandle<()>>,
}

impl Drop for Workers {
    /// Closes every sink, so each encoder drains and flushes, then waits
    /// for them.
    fn drop(&mut self) {
        self.sinks.clear();
        for t in self.threads.drain(..) {
            if t.join().is_err() {
                error!("an encoder thread panicked");
            }
        }
    }
}

/// What became of a frame offered to a sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Offer {
    /// Queued.
    Sent,
    /// Queued in place of a frame that waited, which is dropped.
    Replaced,
    /// Not queued: the queue is full.
    Dropped,
    /// The encoder thread is gone.
    Closed,
}

impl Sink {
    fn new(name: &'static str, queue: Queue) -> Self {
        Self {
            name,
            queue,
            heartbeat: Heartbeat::new(),
        }
    }

    fn offer(&self, job: Job) -> Offer {
        match &self.queue {
            // kanal reports a full channel as `Ok(false)`; `Err` means the
            // receiver is gone.
            Queue::Fifo(tx) => match tx.try_send(job) {
                Ok(true) => Offer::Sent,
                Ok(false) => Offer::Dropped,
                Err(_) => Offer::Closed,
            },
            Queue::Latest(tx) => tx.offer(job),
        }
    }
}

/// When an encoder thread last finished with a frame.
struct Heartbeat(AtomicU64);

impl Heartbeat {
    fn new() -> Arc<Self> {
        let h = Arc::new(Self(AtomicU64::new(0)));
        h.beat();
        h
    }

    fn beat(&self) {
        self.0.store(since_epoch(Instant::now()), Ordering::Relaxed);
    }

    fn age(&self, now: Instant) -> Duration {
        Duration::from_millis(since_epoch(now).saturating_sub(self.0.load(Ordering::Relaxed)))
    }
}

/// Milliseconds since the first call, for heartbeats.
fn since_epoch(now: Instant) -> u64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let epoch = *EPOCH.get_or_init(Instant::now);
    now.saturating_duration_since(epoch).as_millis() as u64
}

/// A one-slot queue whose newest item wins.
struct Mailbox<T> {
    slot: Mutex<MailboxSlot<T>>,
    filled: Condvar,
}

struct MailboxSlot<T> {
    item: Option<T>,
    sender_gone: bool,
    receiver_gone: bool,
}

/// The sending end of [`latest`]; dropping it ends the receiver's loop once
/// the waiting item is taken.
struct LatestSender<T>(Arc<Mailbox<T>>);

/// The receiving end of [`latest`]; dropping it makes every offer
/// [`Offer::Closed`].
struct LatestReceiver<T>(Arc<Mailbox<T>>);

fn latest<T>() -> (LatestSender<T>, LatestReceiver<T>) {
    let mailbox = Arc::new(Mailbox {
        slot: Mutex::new(MailboxSlot {
            item: None,
            sender_gone: false,
            receiver_gone: false,
        }),
        filled: Condvar::new(),
    });
    (LatestSender(Arc::clone(&mailbox)), LatestReceiver(mailbox))
}

impl<T> Mailbox<T> {
    fn lock(&self) -> std::sync::MutexGuard<'_, MailboxSlot<T>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T> LatestSender<T> {
    fn offer(&self, item: T) -> Offer {
        let mut slot = self.0.lock();
        if slot.receiver_gone {
            return Offer::Closed;
        }
        let replaced = slot.item.replace(item).is_some();
        drop(slot);
        self.0.filled.notify_one();
        if replaced {
            Offer::Replaced
        } else {
            Offer::Sent
        }
    }
}

impl<T> Drop for LatestSender<T> {
    fn drop(&mut self) {
        self.0.lock().sender_gone = true;
        self.0.filled.notify_one();
    }
}

impl<T> LatestReceiver<T> {
    /// The waiting item, waiting for one; `None` once the sender is gone
    /// and nothing waits.
    fn recv(&self) -> Option<T> {
        let mut slot = self.0.lock();
        loop {
            if let Some(item) = slot.item.take() {
                return Some(item);
            }
            if slot.sender_gone {
                return None;
            }
            slot = self
                .0
                .filled
                .wait(slot)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl<T> Drop for LatestReceiver<T> {
    fn drop(&mut self) {
        let mut slot = self.0.lock();
        slot.receiver_gone = true;
        slot.item = None;
    }
}

/// The encoder end of a sink.
trait Inbox {
    fn next_job(&self) -> Option<Job>;
}

impl Inbox for Receiver<Job> {
    fn next_job(&self) -> Option<Job> {
        self.recv().ok()
    }
}

impl Inbox for LatestReceiver<Job> {
    fn next_job(&self) -> Option<Job> {
        self.recv()
    }
}

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
) -> Result<(Recorder, SidecarFile), Box<dyn Error>> {
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
    let recorded = Arc::new(AtomicU64::new(0));
    Ok((
        Recorder {
            writer: BufWriter::with_capacity(256 * 1024, file),
            recorded: Arc::clone(&recorded),
        },
        SidecarFile {
            stream: path.clone(),
            sidecar,
            captured: CapturedSpan::default(),
            recorded,
        },
    ))
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
    sinks: &[Sink],
    forward: Option<&Output>,
    progress: &mut Progress,
    shutdown: &AtomicBool,
) -> Result<(), Box<dyn Error>> {
    let mut publisher = FramePublisher::new(session, settings, config)?;
    let timeout = READ_TIMEOUT.max(Duration::from_secs_f64(3.0 / fps));
    let mut retry = ReadRetry::new(MAX_CONSECUTIVE_READ_FAILURES);
    let mut drops = DropStats::new(DROP_REPORT_INTERVAL, Instant::now());
    let mut rate = Rate::new();

    while !shutdown.load(Ordering::SeqCst) {
        if progress.limit_reached(settings) {
            info!("Captured {} frames; stopping", progress.captured);
            break;
        }
        let frame = match read_frame(camera, timeout, &mut retry)? {
            Read::Frame(frame) => frame,
            Read::Retry => continue,
            Read::End => break,
        };
        progress.captured += 1;
        progress.since_open += 1;
        if let Some(file) = progress.sidecar.as_mut() {
            file.captured.frame(frame.timestamp().nanos);
        }
        note_measured_rate(camera, progress, fps);
        let seq = progress.seq_base + frame.seq();
        progress.next_seq = seq + 1;
        if settings.tracy {
            plot!("fps", rate.update(Instant::now()));
        }

        // Every representation of this frame carries this one stamp; the
        // encoder threads must not take another, or a clock step between
        // them would split the frame across two time domains.
        let stamp = schema::stamp(&frame).unwrap_or_else(schema::now);
        info_span!("camera_publish").in_scope(|| publisher.publish(&frame, seq, stamp))?;
        if let (Some(out), Some(au)) = (forward, frame.encoded()) {
            out.video(au, stamp);
        }
        hand_to_encoders(sinks, frame, stamp, &mut drops)?;
        report_health(sinks, progress, config, fps);

        if let Some(report) = drops.take_report(Instant::now()) {
            warn!("{report}");
        }
        if settings.tracy {
            frame_mark();
        }
    }
    Ok(())
}

/// What one read of the camera gave.
enum Read {
    Frame(Frame),
    /// Nothing yet, within the retry budget.
    Retry,
    /// The recording ended.
    End,
}

/// Reads the next frame, retrying timeouts within `retry`'s budget. Any
/// other failure, or a budget spent, is an error.
fn read_frame(
    camera: &mut dyn Camera,
    timeout: Duration,
    retry: &mut ReadRetry,
) -> Result<Read, Box<dyn Error>> {
    match info_span!("camera_read").in_scope(|| camera.next_frame(Some(timeout))) {
        Ok(frame) => {
            retry.on_success();
            Ok(Read::Frame(frame))
        }
        Err(e) => match e.kind() {
            ErrorKind::EndOfStream => {
                info!("End of the recording");
                Ok(Read::End)
            }
            ErrorKind::Interrupted => Ok(Read::Retry),
            ErrorKind::Timeout if retry.should_retry() => {
                warn!(
                    attempt = retry.consecutive(),
                    max = MAX_CONSECUTIVE_READ_FAILURES,
                    "no frame within {timeout:?}, retrying"
                );
                Ok(Read::Retry)
            }
            _ => {
                error!("camera read failed: {e}");
                Err(e.into())
            }
        },
    }
}

/// Reports the camera's measured rate when it first has one and whenever it
/// changes.
fn note_measured_rate(camera: &dyn Camera, progress: &mut Progress, fps: f64) {
    let Some(measured) = camera.measured_frame_rate() else {
        return;
    };
    if progress
        .measured
        .is_some_and(|last| !rates_differ(measured, last))
    {
        return;
    }
    progress.measured = Some(measured);
    report_rate(measured, fps, progress.sidecar.as_mut());
}

/// Offers `frame` to every encoder. An encoder thread that is gone ends
/// the run.
fn hand_to_encoders(
    sinks: &[Sink],
    frame: Frame,
    stamp: Time,
    drops: &mut DropStats,
) -> Result<(), Box<dyn Error>> {
    let frame = Arc::new(frame);
    for sink in sinks {
        let offer = sink.offer((Arc::clone(&frame), stamp));
        if offer == Offer::Closed {
            return Err(Failure::new(
                Status::Software,
                format!("the {} encoder stopped", sink.name),
            )
            .into());
        }
        drops.record(sink.name, offer);
    }
    Ok(())
}

/// Tells systemd the service is ready after its first frame, and alive
/// while every encoder keeps finishing frames.
fn report_health(sinks: &[Sink], progress: &mut Progress, config: &StreamConfig, fps: f64) {
    if !progress.ready {
        progress.ready = true;
        notify::ready();
        notify::status(&format!(
            "Capturing {}x{} at {fps} fps",
            config.width, config.height
        ));
    }
    let now = Instant::now();
    match sinks.iter().find(|s| s.heartbeat.age(now) > STAGE_STALL) {
        None => {
            progress.stalled = None;
            notify::watchdog();
        }
        Some(sink) if progress.stalled != Some(sink.name) => {
            error!(
                "the {} encoder has not finished a frame in {:?}",
                sink.name,
                sink.heartbeat.age(now)
            );
            progress.stalled = Some(sink.name);
        }
        Some(_) => {}
    }
}

/// Publishes `camera/frame` and `camera/info`, reusing one CDR buffer.
struct FramePublisher<'a> {
    session: &'a Session,
    settings: &'a Settings,
    config: &'a StreamConfig,
    frames: Publisher<'static>,
    infos: Publisher<'static>,
    frame_enc: Encoding,
    info_enc: Encoding,
    cdr: Vec<u8>,
}

impl<'a> FramePublisher<'a> {
    fn new(
        session: &'a Session,
        settings: &'a Settings,
        config: &'a StreamConfig,
    ) -> Result<Self, Box<dyn Error>> {
        let declare = |topic: &str, priority: Priority| {
            session
                .declare_publisher(topic.to_string())
                .priority(priority)
                .congestion_control(CongestionControl::Drop)
                .wait()
                .map_err(|e| format!("cannot declare publisher {topic}: {e}"))
        };
        Ok(Self {
            session,
            settings,
            config,
            frames: declare(&settings.frame_topic, Priority::Data)?,
            infos: declare(&settings.info_topic, Priority::Background)?,
            frame_enc: Encoding::APPLICATION_CDR.with_schema("edgefirst_msgs/msg/CameraFrame"),
            info_enc: Encoding::APPLICATION_CDR.with_schema("sensor_msgs/msg/CameraInfo"),
            cdr: Vec::new(),
        })
    }

    fn publish(&mut self, frame: &Frame, seq: u64, stamp: Time) -> Result<(), Box<dyn Error>> {
        let ts = sample_timestamp(self.session, stamp);
        let tensor = FrameTensor::new(frame, self.config.colorimetry.as_ref())?;
        self.cdr.clear();
        tensor.with_fields(|fields| {
            CameraFrame::builder()
                .stamp(stamp)
                .frame_id(self.settings.frame_id.as_str())
                .seq(seq)
                .tensor(fields)
                .encode_into_vec(&mut self.cdr)
        })?;
        if let Err(e) = self
            .frames
            .put(ZBytes::from(self.cdr.as_slice()))
            .encoding(self.frame_enc.clone())
            .timestamp(ts)
            .wait()
        {
            warn!("publishing {}: {e}", self.settings.frame_topic);
        }
        let info = self.settings.info.build_msg(stamp)?;
        if let Err(e) = self
            .infos
            .put(ZBytes::from(info.into_cdr()))
            .encoding(self.info_enc.clone())
            .timestamp(ts)
            .wait()
        {
            warn!("publishing {}: {e}", self.settings.info_topic);
        }
        Ok(())
    }
}

fn run_stage(
    mut stage: Stage,
    rx: &impl Inbox,
    out: Output,
    mut recorder: Option<Recorder>,
    tracy: bool,
    heartbeat: &Heartbeat,
) {
    let publish = |au: EncodedFrame, stamp: Time, recorder: &mut Option<Recorder>| {
        if let Some(r) = recorder.as_mut() {
            if let Err(e) = r.write(&au) {
                error!("h264 recorder write failed: {e}");
            }
        }
        if tracy {
            plot!("h264_kb", au.data.len() as f64 / 1024.0);
        }
        out.video(&au.data, stamp);
    };
    let mut failures = 0;
    while let Some((frame, stamp)) = rx.next_job() {
        if !stage.accept(Instant::now()) {
            heartbeat.beat();
            continue;
        }
        let span = info_span!("h264", stage = stage.label());
        match span.in_scope(|| stage.encode(frame, stamp)) {
            Ok(aus) => {
                failures = 0;
                aus.into_iter()
                    .for_each(|(au, stamp)| publish(au, stamp, &mut recorder));
            }
            Err(e) => {
                failures += 1;
                error!("{} encode failed: {e}", stage.label());
                if failures >= ENCODE_FAILURE_LIMIT {
                    error!(
                        "{}: {failures} encodes failed in a row; stopping the encoder",
                        stage.label()
                    );
                    break;
                }
            }
        }
        heartbeat.beat();
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
    if let Some(mut r) = recorder.take() {
        if let Err(e) = r.writer.flush() {
            error!("h264 recorder final flush failed: {e}");
        }
    }
}

fn run_jpeg(
    mut stage: JpegStage,
    rx: &impl Inbox,
    out: Output,
    tracy: bool,
    heartbeat: &Heartbeat,
) {
    let mut failures = 0;
    while let Some((frame, stamp)) = rx.next_job() {
        let encoded = info_span!("jpeg").in_scope(|| stage.encode(frame));
        heartbeat.beat();
        let jpeg = match encoded {
            Ok(jpeg) => {
                failures = 0;
                jpeg
            }
            Err(e) => {
                failures += 1;
                error!("jpeg encode failed: {e}");
                if failures >= ENCODE_FAILURE_LIMIT {
                    error!("jpeg: {failures} encodes failed in a row; stopping the encoder");
                    return;
                }
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

/// Counters for frames lost by each encoder that was still busy with an
/// earlier frame.
///
/// Encoder drops are expected under load, and dropping is the right
/// response; counting them is what lets an operator see that a recording
/// has holes. Reports are rate-limited to one line per interval, and give
/// each encoder's drops against the frames offered to it.
pub(crate) struct DropStats {
    /// Per sink: frames offered and frames dropped.
    sinks: BTreeMap<&'static str, (u64, u64)>,
    interval: Duration,
    last_report: Instant,
}

impl DropStats {
    pub(crate) fn new(interval: Duration, now: Instant) -> Self {
        Self {
            sinks: BTreeMap::new(),
            interval,
            last_report: now,
        }
    }

    /// Counts a frame offered to `sink`.
    fn record(&mut self, sink: &'static str, offer: Offer) {
        let (offered, dropped) = self.sinks.entry(sink).or_insert((0, 0));
        *offered += 1;
        if matches!(offer, Offer::Dropped | Offer::Replaced | Offer::Closed) {
            *dropped += 1;
        }
    }

    /// A one-line summary of the window just ended, or `None` while the
    /// interval has not elapsed or nothing was lost. Taking a report resets
    /// the window.
    pub(crate) fn take_report(&mut self, now: Instant) -> Option<String> {
        if now.duration_since(self.last_report) < self.interval {
            return None;
        }
        self.last_report = now;
        let busy: Vec<String> = self
            .sinks
            .iter()
            .filter(|(_, (_, dropped))| *dropped > 0)
            .map(|(sink, (offered, dropped))| format!("{sink}={dropped}/{offered}"))
            .collect();
        self.sinks.clear();
        (!busy.is_empty()).then(|| format!("encoders busy, frames dropped: {}", busy.join(" ")))
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
    use crate::encode::{tests::open_fake, Encode};
    use crate::source::{self, Request};
    use clap::Parser;
    use edgefirst_codec::{
        video::{EncoderConfig, FrameOptions},
        CodecError,
    };
    use edgefirst_schemas::{
        foxglove_msgs::FoxgloveCompressedVideo,
        geometry_msgs::TransformStamped,
        sensor_msgs::{CameraInfo, CompressedImage},
    };
    use edgefirst_tensor::TensorDyn;
    use std::collections::{BTreeSet, HashMap};
    use std::sync::Mutex;

    #[test]
    fn kanal_reports_a_full_channel_as_ok_false() {
        let (tx, _rx) = kanal::bounded::<u8>(1);
        assert_eq!(tx.try_send(1), Ok(true));
        assert_eq!(tx.try_send(2), Ok(false), "a full channel is Ok(false)");
    }

    #[test]
    fn a_full_channel_is_a_drop_and_a_gone_receiver_is_closed() {
        let mut opened = mock_opened("");
        let mut job = || {
            let frame = opened
                .camera
                .next_frame(Some(Duration::from_secs(1)))
                .unwrap();
            (Arc::new(frame), schema::now())
        };
        let (tx, rx) = kanal::bounded::<Job>(1);
        let sink = Sink::new("h264", Queue::Fifo(tx));
        assert_eq!(sink.offer(job()), Offer::Sent);
        assert_eq!(sink.offer(job()), Offer::Dropped);
        drop(rx);
        assert_eq!(sink.offer(job()), Offer::Closed);
    }

    #[test]
    fn the_newest_frame_replaces_one_that_waits() {
        let (tx, rx) = latest::<u32>();
        assert_eq!(tx.offer(1), Offer::Sent);
        assert_eq!(tx.offer(2), Offer::Replaced);
        assert_eq!(rx.recv(), Some(2));
        assert_eq!(tx.offer(3), Offer::Sent);
        drop(tx);
        assert_eq!(rx.recv(), Some(3), "a waiting item outlives the sender");
        assert_eq!(rx.recv(), None);
    }

    #[test]
    fn a_mailbox_whose_receiver_is_gone_is_closed() {
        let (tx, rx) = latest::<u32>();
        drop(rx);
        assert_eq!(tx.offer(1), Offer::Closed);
    }

    #[test]
    fn a_mailbox_wakes_its_receiver() {
        let (tx, rx) = latest::<u32>();
        let taker = thread::spawn(move || rx.recv());
        thread::sleep(Duration::from_millis(20));
        tx.offer(7);
        assert_eq!(taker.join().unwrap(), Some(7));
    }

    #[test]
    fn heartbeats_age() {
        let h = Heartbeat::new();
        let now = Instant::now();
        assert!(h.age(now) < Duration::from_secs(1));
        assert!(h.age(now + Duration::from_secs(12)) > STAGE_STALL);
    }

    #[test]
    fn drops_are_reported_per_sink_against_frames_offered() {
        let t0 = Instant::now();
        let mut stats = DropStats::new(Duration::from_secs(10), t0);
        for _ in 0..300 {
            stats.record("h264", Offer::Sent);
        }
        for i in 0..300 {
            stats.record(
                "jpeg",
                if i % 3 == 0 {
                    Offer::Replaced
                } else {
                    Offer::Sent
                },
            );
        }
        assert!(stats.take_report(t0 + Duration::from_secs(1)).is_none());
        let report = stats.take_report(t0 + Duration::from_secs(11)).unwrap();
        assert_eq!(report, "encoders busy, frames dropped: jpeg=100/300");
    }

    #[test]
    fn drop_stats_resets_the_window_after_reporting() {
        let t0 = Instant::now();
        let mut stats = DropStats::new(Duration::from_secs(10), t0);
        stats.record("jpeg", Offer::Dropped);
        stats.take_report(t0 + Duration::from_secs(11)).unwrap();
        stats.record("jpeg", Offer::Dropped);
        stats.record("jpeg", Offer::Sent);
        let second = stats.take_report(t0 + Duration::from_secs(22)).unwrap();
        assert!(second.contains("jpeg=1/2"), "{second}");
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
        let request = Request::from_args(&args, |_| unreachable!())
            .unwrap()
            .unwrap();
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
        let opened = source::open(&request, &shutdown).unwrap().unwrap();
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
                "--h264",
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

    /// An encoder that keeps failing ends the run, so the service manager
    /// restarts the service, rather than streaming on without its output.
    #[test]
    fn an_encoder_that_keeps_failing_ends_the_run() {
        let (_, settings) = settings(&[
            "--source",
            "mock:64x48@120",
            "--stream-size",
            "64",
            "48",
            "--h264",
        ]);
        let session = local_session();
        let shutdown = AtomicBool::new(false);
        let err = run(
            mock_opened(""),
            &session,
            &settings,
            crate::encode::tests::open_failing,
            None,
            &shutdown,
        )
        .unwrap_err();
        assert_eq!(crate::exit::status(&*err), Status::Software, "{err}");
        assert!(err.to_string().contains("h264"), "{err}");
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
                "--h264",
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
        assert_eq!((sidecar.width, sidecar.height), (64, 48));
        let extras: serde_json::Value = serde_json::from_str(&sidecar.extras).unwrap();
        assert_eq!(extras["camera_info"]["frame_id"], "rec");
        assert_eq!(extras["tf_static"]["child_frame_id"], "rec");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_takes_the_nearest_preset_and_measure_snaps_only_when_close() {
        assert_eq!(preset_for(Some(62.61)), 60.0, "the i.MX 95 neo pipeline");
        assert_eq!(preset_for(Some(29.97)), 30.0);
        assert_eq!(preset_for(Some(12.0)), 15.0);
        assert_eq!(preset_for(None), FALLBACK_FPS);
        assert_eq!(snap(60.02), 60.0);
        assert_eq!(snap(29.98), 30.0);
        assert_eq!(snap(25.0), 25.0);
    }

    #[test]
    fn measure_sets_the_encoders_up_for_the_delivered_rate() {
        let (_, settings) = settings(&["--source", "mock:64x48@30", "--h264-fps", "measure"]);
        assert_eq!(settings.h264_fps, H264Fps::Measure);
    }

    #[test]
    fn a_measured_rate_corrects_the_recording_sidecar() {
        let dir =
            std::env::temp_dir().join(format!("edgefirst-camera-rate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stream = dir.join("rec.h264");
        let mut file = SidecarFile {
            stream: stream.clone(),
            sidecar: Sidecar::new(30.0, 64, 48),
            captured: CapturedSpan::default(),
            recorded: Arc::default(),
        };
        file.sidecar.write(&stream).unwrap();
        report_rate(30.1, 30.0, Some(&mut file));
        assert_eq!(
            Sidecar::load(&stream).unwrap().fps,
            30.0,
            "within tolerance"
        );
        report_rate(60.02, 30.0, Some(&mut file));
        assert_eq!(Sidecar::load(&stream).unwrap().fps, 60.02);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rate_changing_recording_gets_the_mean_rate() {
        const MS: i64 = 1_000_000;
        let mut span = CapturedSpan::default();
        let mut t = 0;
        // The start, then 100 frames at 10 fps and 100 at 20 fps.
        span.frame(t);
        for interval in [500, 500].into_iter().chain([100; 100]).chain([50; 100]) {
            t += interval * MS;
            span.frame(t);
        }
        assert_eq!(span.mean_fps(), Some(13.33));

        // The time a camera was away is not part of the recording.
        span.camera_changed();
        t += 60_000 * MS;
        span.frame(t);
        assert_eq!(span.mean_fps(), Some(13.33));
    }

    #[test]
    fn a_short_recording_has_no_mean_rate() {
        let mut span = CapturedSpan::default();
        for i in 0..10 {
            span.frame(i * 33_000_000);
        }
        assert_eq!(span.mean_fps(), None);
    }

    #[test]
    fn frames_that_missed_the_recording_lower_its_rate() {
        let mut span = CapturedSpan::default();
        for i in 0..101 {
            span.frame(i * 33_333_333);
        }
        assert_eq!(span.mean_fps(), Some(30.0));
        assert_eq!(span.recorded_fps(101), Some(30.0));
        assert_eq!(span.recorded_fps(50), Some(14.85));
        assert_eq!(span.recorded_fps(0), None);
    }

    #[test]
    fn frames_that_missed_a_short_recording_lower_its_rate() {
        let mut span = CapturedSpan::default();
        for i in 0..20 {
            span.frame(i * 200_000_000);
        }
        assert_eq!(span.mean_fps(), None);
        assert_eq!(span.recorded_fps(20), Some(5.0));
        assert_eq!(span.recorded_fps(10), Some(2.5));
    }

    #[test]
    fn a_recording_too_short_to_time_has_no_rate() {
        let mut span = CapturedSpan::default();
        for i in 0..3 {
            span.frame(i * 200_000_000);
        }
        assert_eq!(span.recorded_fps(2), None);
    }

    /// An encoder that skips every other frame and holds the rest until it is
    /// flushed, so the frames only reach the recording once it has drained.
    fn open_skipping(_: EncoderConfig) -> Result<Box<dyn Encode>, CodecError> {
        struct Skipping(Vec<EncodedFrame>);
        impl Encode for Skipping {
            fn encode(
                &mut self,
                _: &TensorDyn,
                opts: &FrameOptions,
            ) -> Result<Option<EncodedFrame>, CodecError> {
                if opts.pts.is_multiple_of(2) {
                    self.0.push(EncodedFrame {
                        data: vec![0; AU_BYTES],
                        keyframe: false,
                        pts: opts.pts,
                    });
                }
                Ok(None)
            }
            fn flush(&mut self) -> Result<Vec<EncodedFrame>, CodecError> {
                Ok(std::mem::take(&mut self.0))
            }
        }
        Ok(Box::new(Skipping(Vec::new())))
    }

    const AU_BYTES: usize = 16;

    /// Records `frames` from a mock camera at `fps`, half of them reaching the
    /// file. A stall lengthens the capture and the playback alike, so playback
    /// is held below the time the run took rather than the nominal time.
    fn assert_recording_lasts_as_long_as_its_capture(frames: u64, fps: u32) {
        let dir = std::env::temp_dir().join(format!(
            "edgefirst-camera-span-{}-{frames}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stream = dir.join("rec.h264");
        let (request, mut settings) = settings(&[
            "--source",
            &format!("mock:64x48@{fps}"),
            "--stream-size",
            "64",
            "48",
            "--h264",
            "--record",
            stream.to_str().unwrap(),
        ]);
        settings.max_frames = Some(frames);
        let session = local_session();
        let flag = AtomicBool::new(false);
        let opened = source::open(&request, &flag).unwrap().unwrap();
        let started = Instant::now();
        run(opened, &session, &settings, open_skipping, None, &flag).unwrap();
        let took = started.elapsed().as_secs_f64();

        let recorded = std::fs::metadata(&stream).unwrap().len() as f64 / AU_BYTES as f64;
        let sidecar_fps = Sidecar::load(&stream).unwrap().fps;
        let played = recorded / sidecar_fps;
        let nominal = (frames - 1) as f64 / f64::from(fps);
        assert!(
            played > 0.7 * nominal && played < 1.25 * took,
            "{recorded} frames at {sidecar_fps} fps play for {played:.2} s \
             of {nominal:.2} s nominal, in a run of {took:.2} s"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_recording_lasts_as_long_as_its_capture_whatever_reached_it() {
        assert_recording_lasts_as_long_as_its_capture(61, 60);
    }

    #[test]
    fn a_short_recording_lasts_as_long_as_its_capture_whatever_reached_it() {
        assert_recording_lasts_as_long_as_its_capture(31, 30);
    }

    #[test]
    fn tiles_turn_on_by_themselves_only_above_1080p_with_h264() {
        assert!(tiles_enabled(false, true, (3840, 2160)));
        assert!(!tiles_enabled(false, true, (1920, 1080)));
        assert!(!tiles_enabled(false, false, (3840, 2160)), "H.264 off");
        assert!(tiles_enabled(true, false, (1920, 1080)), "requested");
    }

    #[test]
    fn a_stream_larger_than_the_capture_is_refused() {
        let (request, settings) = settings(&["--source", "mock:64x48@30", "--h264"]);
        let session = local_session();
        let flag = AtomicBool::new(false);
        let opened = source::open(&request, &flag).unwrap().unwrap();
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

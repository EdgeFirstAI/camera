// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

//! Opening the frame source: the request built from the command line, the
//! capture pool, mirroring, and the retry while the ISP starts.

use std::{
    error::Error,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use edgefirst_camera::{
    Applied, Backend, BufferPool, Camera, CameraBuilder, CameraDescriptor, Control, ErrorKind,
    Exposure, FormatInfo, Gain, MemoryStrategy, Mirror, Rates, Sizes, StreamConfig, WhiteBalance,
};
use edgefirst_image::ImageProcessor;
use edgefirst_tensor::{CpuAccess, DType, ImageDesc, PixelFormat, TensorDyn, TensorMemory};
use tracing::{info, warn};

use crate::args::{Args, CameraMode, MemorySetting, MirrorSetting, Setting};

/// How long to keep retrying while the camera reports it is not ready:
/// the ISP daemon has not started serving the device, or delivers no first
/// frame yet.
///
/// Deliberately small. camera.service bounds its own restart loop with
/// StartLimitIntervalSec=60 / StartLimitBurst=5 so that a camera which
/// cannot start lands in "failed", where it is visible, rather than
/// hammering the ISP. Every second spent retrying in-process is a second
/// that window cannot account for, and a long enough budget would stop five
/// starts from ever fitting inside it.
pub(crate) const ISP_READY_BUDGET: Duration = Duration::from_secs(15);

/// How long to wait between attempts.
pub(crate) const ISP_READY_DELAY: Duration = Duration::from_secs(2);

/// How many attempts to make before giving up on the ISP. A count as well
/// as a budget, because an attempt that fails fast says nothing about how
/// many fit in the budget, and each one costs the ISP a client connection.
pub(crate) const ISP_READY_ATTEMPT_LIMIT: u32 = 6;

// A budget shorter than two delays collapses the loop to a single attempt
// and the retry buys nothing. Compared in nanoseconds: `as_secs()` truncates
// and would let a sub-second mistake through.
const _: () = assert!(ISP_READY_BUDGET.as_nanos() >= ISP_READY_DELAY.as_nanos() * 2);
const _: () = assert!(ISP_READY_ATTEMPT_LIMIT >= 2);

/// Capture buffers added for the encoders, which hold a frame's buffer
/// while it waits for them and while they read it.
pub(crate) const ENCODER_BUFFERS: usize = 2;

/// The SDK's largest pool.
const MAX_BUFFERS: usize = 32;

/// The format requested from camera devices.
pub(crate) const CAPTURE_FORMAT: PixelFormat = PixelFormat::Yuyv;

/// What kind of source a spec names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// A camera device.
    Device,
    /// A recording.
    File,
    /// Synthetic frames.
    Synthetic,
}

impl SourceKind {
    pub(crate) fn of(spec: &str) -> Self {
        if spec.starts_with("file:") {
            Self::File
        } else if spec == "mock" || spec.starts_with("mock:") {
            Self::Synthetic
        } else {
            Self::Device
        }
    }
}

/// A started camera.
#[derive(Debug)]
pub(crate) struct Opened {
    pub(crate) camera: Box<dyn Camera>,
    pub(crate) kind: SourceKind,
    /// Mirroring the camera could not apply, for the encoders to apply.
    pub(crate) software_mirror: Option<Mirror>,
}

/// Everything needed to open the source, resolved from the command line.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub(crate) spec: String,
    pub(crate) kind: SourceKind,
    pub(crate) size: Option<(u32, u32)>,
    pub(crate) fps: Option<f64>,
    /// Format to capture, when the camera offers it.
    pub(crate) format: Option<PixelFormat>,
    pub(crate) buffers: usize,
    pub(crate) memory: MemoryStrategy,
    pub(crate) mirror: Mirror,
    pub(crate) looping: bool,
    pub(crate) controls: Vec<Control>,
}

impl Request {
    /// The request `args` describe. A `CAMERA_MODE` is checked against the
    /// modes `probe` reports and expanded; explicit size and rate win over
    /// it.
    pub(crate) fn from_args(
        args: &Args,
        probe: impl FnOnce(&str) -> edgefirst_camera::Result<Vec<FormatInfo>>,
    ) -> Result<Self, Box<dyn Error>> {
        let spec = args.source_spec();
        let kind = SourceKind::of(&spec);
        let mut size = args.explicit_size();
        let mut fps = args.explicit_fps();
        if let Some(mode) = &args.camera_mode {
            if kind != SourceKind::Device {
                return Err(format!(
                    "CAMERA_MODE {mode} needs a camera that reports its modes, and {spec} does \
                     not; set CAMERA_SIZE and CAMERA_FPS instead"
                )
                .into());
            }
            let formats = probe(&spec)
                .map_err(|e| format!("cannot read the modes of {spec} for CAMERA_MODE: {e}"))?;
            check_mode(mode, &formats)?;
            let mode_fps = f64::from(mode.fps);
            match size {
                Some(s) if s != mode.size => warn!(
                    "CAMERA_SIZE {}x{} overrides the size of CAMERA_MODE {mode}",
                    s.0, s.1
                ),
                _ => size = Some(mode.size),
            }
            match fps {
                Some(f) if f != mode_fps => {
                    warn!("CAMERA_FPS {f} overrides the rate of CAMERA_MODE {mode}")
                }
                _ => fps = Some(mode_fps),
            }
        }
        if kind != SourceKind::File {
            if args.replay_loop {
                warn!("REPLAY_LOOP applies to file: sources only; ignored for {spec}");
            }
            if args.replay_fps.is_some() {
                warn!("REPLAY_FPS applies to file: sources only; ignored for {spec}");
            }
        } else if let (Some(c), Some(r)) = (args.camera_fps, args.replay_fps) {
            if c != r {
                warn!("CAMERA_FPS {c} overrides REPLAY_FPS {r}");
            }
        }
        Ok(Self {
            // Packed YUV is what the encoders and converters take directly
            // on the i.MX 8M Plus; a device keeps its last format otherwise.
            format: (kind == SourceKind::Device).then_some(CAPTURE_FORMAT),
            spec,
            kind,
            size,
            fps,
            buffers: pool_depth(args),
            memory: match args.memory {
                MemorySetting::Auto => MemoryStrategy::Auto,
                MemorySetting::Import => MemoryStrategy::Import,
                MemorySetting::Export => MemoryStrategy::Export,
            },
            // A recording was mirrored when it was made.
            mirror: match (kind, args.mirror) {
                (SourceKind::File, _) | (_, MirrorSetting::None) => Mirror::None,
                (_, MirrorSetting::Horizontal) => Mirror::Horizontal,
                (_, MirrorSetting::Vertical) => Mirror::Vertical,
                (_, MirrorSetting::Both) => Mirror::Both,
            },
            looping: args.replay_loop,
            controls: controls(args),
        })
    }

    /// The device path, for messages.
    pub(crate) fn name(&self) -> &str {
        &self.spec
    }
}

/// The capture pool depth: `CAMERA_BUFFERS`, which is what subscribers to
/// `camera/frame` can rely on, plus what the encoders hold.
fn pool_depth(args: &Args) -> usize {
    let buffers = args.camera_buffers as usize;
    if args.h264 || args.h264_tiles || args.jpeg {
        (buffers + ENCODER_BUFFERS).min(MAX_BUFFERS)
    } else {
        buffers
    }
}

/// The camera controls set on the command line.
fn controls(args: &Args) -> Vec<Control> {
    let mut out = Vec::new();
    if let Some(e) = args.exposure {
        out.push(Control::Exposure(match e {
            Setting::Auto => Exposure::Auto,
            Setting::Manual(us) => Exposure::Manual(Duration::from_micros(u64::from(us))),
        }));
    }
    if let Some(g) = args.gain {
        out.push(Control::Gain(match g {
            Setting::Auto => Gain::Auto,
            Setting::Manual(g) => Gain::Manual(g),
        }));
    }
    if let Some(wb) = args.white_balance {
        out.push(Control::WhiteBalance(match wb {
            Setting::Auto => WhiteBalance::Auto,
            Setting::Manual(kelvin) => WhiteBalance::Manual { kelvin },
        }));
    }
    out
}

/// Probes a V4L2 device's modes.
pub(crate) fn probe_device(path: &str) -> edgefirst_camera::Result<Vec<FormatInfo>> {
    edgefirst_camera::probe(&CameraDescriptor::new(Backend::V4l2, path, "", ""))
}

/// Accepts `mode` when some format offers its size at its rate; otherwise
/// the error lists the modes the camera reports.
pub(crate) fn check_mode(mode: &CameraMode, formats: &[FormatInfo]) -> Result<(), String> {
    let (w, h) = mode.size;
    let fps = f64::from(mode.fps);
    let rate_ok = |rates: &Rates| match rates {
        Rates::Discrete(list) => list.iter().any(|r| (r - fps).abs() < 0.5),
        Rates::Range { min, max } => *min - 0.5 <= fps && fps <= *max + 0.5,
        Rates::Unknown => true,
    };
    let offered = formats.iter().any(|f| match &f.sizes {
        Sizes::Discrete(sizes) => sizes
            .iter()
            .any(|s| (s.width, s.height) == (w, h) && rate_ok(&s.rates)),
        Sizes::Stepwise {
            min,
            max,
            step,
            rates,
        } => {
            let fits = |v: u32, lo: u32, hi: u32, step: u32| {
                (lo..=hi).contains(&v) && (step <= 1 || (v - lo).is_multiple_of(step))
            };
            fits(w, min.0, max.0, step.0) && fits(h, min.1, max.1, step.1) && rate_ok(rates)
        }
    });
    if offered {
        return Ok(());
    }
    let mut names: Vec<String> = formats
        .iter()
        .flat_map(|f| edgefirst_camera::modes(formats, f.format))
        .map(|m| m.name())
        .collect();
    names.sort();
    names.dedup();
    Err(if names.is_empty() {
        format!("CAMERA_MODE {mode} is not offered, and the camera reports no named modes")
    } else {
        format!(
            "CAMERA_MODE {mode} is not offered; the camera reports {}",
            names.join(", ")
        )
    })
}

/// Opens and starts the source, retrying while the camera is not ready.
///
/// Returns `Ok(None)` if a shutdown arrived before the camera started. That
/// is a successful outcome, not a failure: an intentional `systemctl stop`
/// during startup must not leave the unit in "failed", which is the signal
/// reserved for a camera that genuinely cannot start.
pub(crate) fn open(
    request: &Request,
    processor: Option<&ImageProcessor>,
    shutdown: &AtomicBool,
) -> Result<Option<Opened>, Box<dyn Error>> {
    let deadline = Instant::now() + ISP_READY_BUDGET;
    retry_while_not_ready(request.name(), deadline, ISP_READY_DELAY, shutdown, || {
        open_once(request, processor)
    })
}

/// One attempt: open, mirror, provide the pool, start.
fn open_once(
    request: &Request,
    processor: Option<&ImageProcessor>,
) -> edgefirst_camera::Result<Opened> {
    let builder = |format: Option<PixelFormat>| -> edgefirst_camera::Result<CameraBuilder> {
        let mut b = CameraBuilder::source(&request.spec)?
            .buffers(request.buffers)
            .memory(request.memory);
        if let Some((w, h)) = request.size {
            b = b.size(w, h);
        }
        if let Some(fps) = request.fps {
            b = b.frame_rate(fps);
        }
        if let Some(format) = format {
            b = b.format(format);
        }
        if request.kind == SourceKind::File {
            b = b.looping(request.looping);
        }
        for &c in &request.controls {
            b = b.control(c);
        }
        Ok(b)
    };
    let mut camera = match builder(request.format)?.open() {
        Err(e) if e.kind() == ErrorKind::UnsupportedFormat && request.format.is_some() => {
            warn!("{e}; capturing in the camera's current format instead");
            builder(None)?.open()?
        }
        other => other?,
    };

    // A camera that cannot mirror, or refuses to in this mode, is mirrored
    // in the encoders instead; mirroring is not worth failing the service.
    let mut software_mirror = None;
    if request.mirror != Mirror::None {
        let why = match camera.set_control(Control::Mirror(request.mirror)) {
            Ok(Applied::Unsupported(why)) => Some(format!("{why:?}")),
            Ok(_) => None,
            Err(e) => Some(e.to_string()),
        };
        if let Some(why) = why {
            warn!(
                "{} cannot mirror ({why}); mirroring {:?} in the encoders instead. \
                 camera/frame is published unmirrored.",
                request.name(),
                request.mirror
            );
            software_mirror = Some(request.mirror);
        }
    }

    let contiguous = request.kind == SourceKind::Device;
    if request.kind != SourceKind::File && request.memory != MemoryStrategy::Export {
        if let Some(processor) = processor {
            match allocate_pool(processor, camera.config(), contiguous) {
                Ok(pool) => camera.set_buffers(BufferPool::Provided(pool))?,
                Err(e) if contiguous => {
                    return Err(contiguous_unavailable(camera.config(), &e.to_string()))
                }
                Err(e) => warn!("cannot allocate the capture pool, using the camera's: {e}"),
            }
        }
    }

    match camera.start() {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::BuffersRejected => {
            let (reason, slot) = e.rejection().unzip();
            warn!(
                "{} refused this service's capture buffers ({reason:?}, slot {:?}): {e}; \
                 using the camera's own buffers",
                request.name(),
                slot.flatten()
            );
            camera.take_buffers();
            camera.set_buffers(BufferPool::Sdk)?;
            camera.start().map_err(|e| fatal(camera.config(), e))?;
        }
        Err(e) => return Err(fatal(camera.config(), e)),
    }

    let c = camera.config();
    info!(
        "Opened {}: {}x{} {} at {} with {} {:?} buffers",
        request.name(),
        c.width,
        c.height,
        c.format,
        c.frame_rate
            .map_or_else(|| "an unreported rate".to_string(), |f| format!("{f} fps")),
        c.buffer_count,
        c.memory
    );
    Ok(Opened {
        camera,
        kind: request.kind,
        software_mirror,
    })
}

/// Allocates the capture pool for `config` through the image crate, so its
/// buffers suit the converters. `contiguous` asks for CMA memory only.
fn allocate_pool(
    processor: &ImageProcessor,
    config: &StreamConfig,
    contiguous: bool,
) -> Result<Vec<TensorDyn>, edgefirst_image::Error> {
    let mut desc = ImageDesc::new(
        config.width as usize,
        config.height as usize,
        config.format,
        DType::U8,
    )
    .with_access(CpuAccess::ReadWrite);
    if contiguous {
        desc = desc
            .with_memory(Some(TensorMemory::DmaBuf))
            .with_contiguous(true);
    }
    (0..config.buffer_count.max(1))
        .map(|_| processor.create_image_desc(&desc))
        .collect()
}

/// `e`, reworded when the camera could not get contiguous memory.
fn fatal(config: &StreamConfig, e: edgefirst_camera::Error) -> edgefirst_camera::Error {
    if e.kind() == ErrorKind::ContiguousUnavailable {
        contiguous_unavailable(config, &e.to_string())
    } else {
        e
    }
}

/// The error for a pool that does not fit the contiguous memory (CMA).
fn contiguous_unavailable(config: &StreamConfig, cause: &str) -> edgefirst_camera::Error {
    edgefirst_camera::Error::new(
        ErrorKind::ContiguousUnavailable,
        format!(
            "not enough contiguous (CMA) memory for {} capture buffers of {}x{} {}: {cause}. \
             Lower CAMERA_BUFFERS or reserve more CMA; the image converters on this platform \
             need contiguous buffers.",
            config.buffer_count, config.width, config.height, config.format
        ),
    )
}

/// Runs `open_once` until it succeeds, the budget runs out, or a shutdown
/// is requested, retrying only while the camera reports `NotReady`.
///
/// The policy lives here, apart from the one step that needs hardware, so
/// it can be tested without a camera.
pub(crate) fn retry_while_not_ready<T, F>(
    camera: &str,
    deadline: Instant,
    delay: Duration,
    shutdown: &AtomicBool,
    mut open_once: F,
) -> Result<Option<T>, Box<dyn Error>>
where
    F: FnMut() -> edgefirst_camera::Result<T>,
{
    let mut attempt = 0u32;

    loop {
        // Re-checked before every attempt: an attempt can block in the
        // driver for seconds, so a decision taken before it says nothing
        // about whether the next one is still wanted.
        if shutdown.load(Ordering::SeqCst) {
            info!("Shutdown requested while waiting for the camera to become ready");
            return Ok(None);
        }

        attempt += 1;
        match open_once() {
            Ok(opened) => {
                if attempt > 1 {
                    info!("Camera started after {attempt} attempts");
                }
                return Ok(Some(opened));
            }
            Err(err) if err.kind() == ErrorKind::NotReady => {
                if !retry_permitted(attempt, deadline) {
                    return Err(Box::from(not_ready_error(camera, attempt, &err)));
                }
                warn!("{camera} is not ready (attempt {attempt}/{ISP_READY_ATTEMPT_LIMIT}): {err}");
                if !wait_before_retry(deadline, delay, shutdown) {
                    if shutdown.load(Ordering::SeqCst) {
                        info!("Shutdown requested while waiting for the camera to become ready");
                        return Ok(None);
                    }
                    return Err(Box::from(not_ready_error(camera, attempt, &err)));
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
}

/// Waits before the next attempt. Returns false when no further attempt
/// should be made, because the budget ran out or a shutdown was requested.
///
/// The shutdown flag has no wakeup, so this polls it on a short tick: a
/// `systemctl stop` arriving mid-wait should not sit out the rest of the
/// delay. The wait never runs past `deadline`.
pub(crate) fn wait_before_retry(deadline: Instant, delay: Duration, shutdown: &AtomicBool) -> bool {
    const TICK: Duration = Duration::from_millis(100);

    let until = deadline.min(Instant::now() + delay);
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let now = Instant::now();
        if now >= until {
            break;
        }
        std::thread::sleep(TICK.min(until - now));
    }
    Instant::now() < deadline
}

/// The give-up message, naming the recovery that actually works.
pub(crate) fn not_ready_error(
    camera: &str,
    attempts: u32,
    err: &edgefirst_camera::Error,
) -> String {
    format!(
        "{camera} still not ready after {attempts} attempts: {err}. The ISP pipeline is likely \
         wedged rather than slow; restarting camera.service restarts imx8-isp.service via \
         maivin-camera-select-mode."
    )
}

/// Whether another attempt is allowed: the attempt count and the
/// wall-clock budget must both still permit one.
pub(crate) fn retry_permitted(attempt: u32, deadline: Instant) -> bool {
    attempt < ISP_READY_ATTEMPT_LIMIT && Instant::now() < deadline
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use edgefirst_camera::SizeRates;
    use std::str::FromStr;

    fn not_ready() -> edgefirst_camera::Error {
        edgefirst_camera::Error::new(ErrorKind::NotReady, "no first frame")
    }

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    const FAST: Duration = Duration::from_millis(1);

    fn vvcam_formats() -> Vec<FormatInfo> {
        vec![FormatInfo {
            format: PixelFormat::Yuyv,
            sizes: Sizes::Stepwise {
                min: (176, 144),
                max: (3840, 2160),
                step: (16, 8),
                rates: Rates::Discrete(vec![30.0]),
            },
        }]
    }

    fn isi_formats() -> Vec<FormatInfo> {
        vec![FormatInfo {
            format: PixelFormat::Yuyv,
            sizes: Sizes::Discrete(vec![
                SizeRates {
                    width: 640,
                    height: 480,
                    rates: Rates::Discrete(vec![15.0, 30.0]),
                },
                SizeRates {
                    width: 1920,
                    height: 1080,
                    rates: Rates::Discrete(vec![15.0, 30.0]),
                },
            ]),
        }]
    }

    fn mode(s: &str) -> CameraMode {
        CameraMode::from_str(s).unwrap()
    }

    #[test]
    fn a_mode_the_camera_offers_is_accepted() {
        check_mode(&mode("1080p30"), &vvcam_formats()).unwrap();
        check_mode(&mode("720p30"), &vvcam_formats()).unwrap();
        check_mode(&mode("1080p15"), &isi_formats()).unwrap();
        check_mode(&mode("VGA30"), &isi_formats()).unwrap();
    }

    #[test]
    fn a_mode_the_camera_lacks_lists_the_offered_modes() {
        let err = check_mode(&mode("720p30"), &isi_formats()).unwrap_err();
        assert!(err.contains("VGA30") && err.contains("1080p30"), "{err}");
        let err = check_mode(&mode("1080p60"), &vvcam_formats()).unwrap_err();
        assert!(err.contains("1080p30"), "{err}");
        assert!(check_mode(&mode("4K30"), &isi_formats()).is_err());
    }

    fn request(argv: &[&str]) -> Result<Request, Box<dyn Error>> {
        let args = Args::parse_from([&["edgefirst-camera"][..], argv].concat());
        Request::from_args(&args, |_| Ok(vvcam_formats()))
    }

    #[test]
    fn a_mode_expands_to_size_and_rate() {
        let r = request(&["--camera-mode", "720p30"]).unwrap();
        assert_eq!((r.size, r.fps), (Some((1280, 720)), Some(30.0)));
    }

    #[test]
    fn explicit_size_and_rate_win_over_the_mode() {
        let r = request(&[
            "--camera-mode",
            "720p30",
            "--camera-size",
            "1920",
            "1080",
            "--camera-fps",
            "15",
        ])
        .unwrap();
        assert_eq!((r.size, r.fps), (Some((1920, 1080)), Some(15.0)));
    }

    #[test]
    fn a_mode_needs_a_source_that_reports_modes() {
        for source in ["mock:640x480@30", "file:rec.h264"] {
            let err = request(&["--source", source, "--camera-mode", "VGA30"]).unwrap_err();
            assert!(err.to_string().contains("CAMERA_SIZE"), "{source}: {err}");
        }
    }

    #[test]
    fn a_mode_is_probed_only_when_set() {
        let args = Args::parse_from(["edgefirst-camera"]);
        let r = Request::from_args(&args, |_| panic!("probed without CAMERA_MODE")).unwrap();
        assert_eq!((r.size, r.fps), (None, None));
    }

    #[test]
    fn file_sources_take_replay_options_and_are_not_mirrored() {
        let r = request(&[
            "--source",
            "file:rec.h264",
            "--replay-loop",
            "--replay-fps",
            "10",
        ])
        .unwrap();
        assert_eq!(r.kind, SourceKind::File);
        assert!(r.looping);
        assert_eq!(r.fps, Some(10.0));
        assert_eq!(r.mirror, Mirror::None, "MIRROR defaults to both");
        assert_eq!(r.format, None);
        let live = request(&[]).unwrap();
        assert_eq!(live.mirror, Mirror::Both);
        assert_eq!(live.format, Some(PixelFormat::Yuyv));
    }

    #[test]
    fn the_pool_adds_the_encoders_buffers() {
        assert_eq!(request(&[]).unwrap().buffers, 6, "H.264 is on by default");
        assert_eq!(request(&["--h264", "false"]).unwrap().buffers, 4);
        let jpeg = request(&["--h264", "false", "--jpeg", "--camera-buffers", "3"]).unwrap();
        assert_eq!(jpeg.buffers, 5);
        assert_eq!(request(&["--camera-buffers", "32"]).unwrap().buffers, 32);
    }

    #[test]
    fn controls_are_requested() {
        let r = request(&["--exposure", "auto", "--white-balance", "5000"]).unwrap();
        assert_eq!(
            r.controls,
            [
                Control::Exposure(Exposure::Auto),
                Control::WhiteBalance(WhiteBalance::Manual { kelvin: 5000 })
            ]
        );
    }

    #[test]
    fn source_kinds() {
        assert_eq!(SourceKind::of("/dev/video3"), SourceKind::Device);
        assert_eq!(SourceKind::of("file:a.h264"), SourceKind::File);
        assert_eq!(SourceKind::of("mock"), SourceKind::Synthetic);
        assert_eq!(SourceKind::of("mock:64x64@5"), SourceKind::Synthetic);
    }

    /// Opening a mock source runs the whole path: the image-crate pool is
    /// provided and the camera starts on it.
    #[test]
    fn opens_a_mock_source_on_an_image_crate_pool() {
        let r = request(&[
            "--source",
            "mock:64x48@30",
            "--camera-buffers",
            "3",
            "--h264",
            "false",
        ])
        .unwrap();
        let processor = ImageProcessor::new().ok();
        let flag = AtomicBool::new(false);
        let mut opened = open(&r, processor.as_ref(), &flag).unwrap().unwrap();
        assert_eq!(opened.software_mirror, Some(Mirror::Both));
        let c = opened.camera.config().clone();
        assert_eq!((c.width, c.height, c.buffer_count), (64, 48, 3));
        let frame = opened
            .camera
            .next_frame(Some(Duration::from_secs(1)))
            .unwrap();
        assert_eq!(frame.width(), Some(64));
    }

    #[test]
    fn retry_permitted_stops_at_attempt_limit() {
        assert!(retry_permitted(ISP_READY_ATTEMPT_LIMIT - 1, far_deadline()));
        assert!(!retry_permitted(ISP_READY_ATTEMPT_LIMIT, far_deadline()));
    }

    #[test]
    fn retry_permitted_stops_at_deadline() {
        let spent = Instant::now();
        assert!(!retry_permitted(1, spent));
    }

    #[test]
    fn wait_returns_immediately_on_shutdown() {
        let flag = AtomicBool::new(true);
        let started = Instant::now();
        assert!(!wait_before_retry(far_deadline(), ISP_READY_DELAY, &flag));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn wait_refuses_once_budget_is_spent() {
        let flag = AtomicBool::new(false);
        assert!(!wait_before_retry(
            Instant::now(),
            Duration::from_millis(10),
            &flag
        ));
    }

    #[test]
    fn wait_never_waits_past_the_deadline() {
        let flag = AtomicBool::new(false);
        let started = Instant::now();
        let deadline = started + Duration::from_millis(50);
        assert!(!wait_before_retry(deadline, Duration::from_secs(30), &flag));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn wait_allows_another_attempt_within_budget() {
        let flag = AtomicBool::new(false);
        assert!(wait_before_retry(
            far_deadline(),
            Duration::from_millis(10),
            &flag
        ));
    }

    #[test]
    fn not_ready_error_names_device_attempts_and_recovery() {
        let msg = not_ready_error("/dev/video3", 6, &not_ready());
        for want in [
            "/dev/video3",
            "6 attempts",
            "camera.service",
            "imx8-isp.service",
        ] {
            assert!(msg.contains(want), "{msg}");
        }
    }

    #[test]
    fn opens_on_the_first_attempt() {
        let flag = AtomicBool::new(false);
        let mut calls = 0u32;
        let opened = retry_while_not_ready("/dev/video3", far_deadline(), FAST, &flag, || {
            calls += 1;
            Ok(7u32)
        })
        .unwrap();
        assert_eq!((opened, calls), (Some(7), 1));
    }

    #[test]
    fn retries_until_ready() {
        let flag = AtomicBool::new(false);
        let mut calls = 0u32;
        let opened = retry_while_not_ready("/dev/video3", far_deadline(), FAST, &flag, || {
            calls += 1;
            if calls < 3 {
                Err(not_ready())
            } else {
                Ok(7u32)
            }
        })
        .unwrap();
        assert_eq!((opened, calls), (Some(7), 3));
    }

    #[test]
    fn stops_at_the_attempt_cap() {
        let flag = AtomicBool::new(false);
        let mut calls = 0u32;
        let err = retry_while_not_ready("/dev/video3", far_deadline(), FAST, &flag, || {
            calls += 1;
            Err::<u32, _>(not_ready())
        })
        .unwrap_err();
        assert_eq!(calls, ISP_READY_ATTEMPT_LIMIT);
        assert!(err.to_string().contains("still not ready"), "{err}");
    }

    #[test]
    fn stops_when_the_budget_is_spent() {
        let flag = AtomicBool::new(false);
        let mut calls = 0u32;
        let err = retry_while_not_ready(
            "/dev/video3",
            Instant::now() + Duration::from_millis(120),
            Duration::from_millis(40),
            &flag,
            || {
                calls += 1;
                Err::<u32, _>(not_ready())
            },
        )
        .unwrap_err();
        assert!(calls < ISP_READY_ATTEMPT_LIMIT, "{calls} calls");
        assert!(err.to_string().contains("still not ready"), "{err}");
    }

    /// Busy and every other failure need the ISP restarted, which only the
    /// service restart does; retrying would only delay it.
    #[test]
    fn does_not_retry_other_errors() {
        let flag = AtomicBool::new(false);
        for kind in [ErrorKind::Busy, ErrorKind::NotFound, ErrorKind::Backend] {
            let mut calls = 0u32;
            let err = retry_while_not_ready("/dev/video3", far_deadline(), FAST, &flag, || {
                calls += 1;
                Err::<u32, _>(edgefirst_camera::Error::new(kind, "no"))
            })
            .unwrap_err();
            assert_eq!(calls, 1, "{kind:?}");
            assert!(!err.to_string().contains("still not ready"), "{err}");
        }
    }

    #[test]
    fn reports_shutdown_as_success_not_failure() {
        let flag = AtomicBool::new(true);
        let mut calls = 0u32;
        let opened = retry_while_not_ready("/dev/video3", far_deadline(), FAST, &flag, || {
            calls += 1;
            Err::<u32, _>(not_ready())
        })
        .unwrap();
        assert!(opened.is_none());
        assert_eq!(calls, 0);
    }
}

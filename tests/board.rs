// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Platform validation of the SDK with the image and codec crates on a
//! board's camera: a V4L2 camera on the i.MX 8M Plus (an ISI sensor or the
//! vvcam ISP that Maivin and Raivin use), or a libcamera camera on the
//! i.MX 95 neo ISP or the Raspberry Pi 5. Codec tests skip on a board
//! without an H.264 encoder or decoder (`no-encoder`, `no-decoder`).
//!
//! Every test is ignored; run them on a board with `--include-ignored`.
//!
//! - `EDGEFIRST_CAMERA_SOURCE`: the camera, default `/dev/video3`; see
//!   `common::source`. Without it the tests print `SKIPPED` and pass.
//! - `EDGEFIRST_SOAK_SECS`: how long the 1080p30 soak runs, default 20;
//!   600 for the ten-minute platform run.
//! - `EDGEFIRST_CAMERA_RECORDING`: a camera 2.x recording (`.h264` with its
//!   `.json`) for the decode test, which skips without it.
//! - `EDGEFIRST_CAMERA_REQUIRE_BOARD=1`: every skip fails, except those
//!   named in `EDGEFIRST_CAMERA_ALLOW_SKIPS` (see `common::Skip`).
//!
//! vvcam-only tests (the ISP daemon restart, the VIV mirror quirk) check the
//! driver and skip elsewhere. The restart test restarts `imx8-isp.service`.

#![cfg(target_os = "linux")]

use std::{
    error::Error,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use edgefirst_camera::{
    Backend, Camera, CameraBuilder, CaptureClock, ControlId, ControlValue, ErrorKind, Frame,
    MemoryStrategy, Mirror, ResolvedMemory,
};
use edgefirst_codec::video::{EncoderConfig, FrameOptions, VideoEncoder};
use edgefirst_image::{
    ComputeBackend, ImageProcessor, ImageProcessorConfig, ImageProcessorTrait as _,
};
use edgefirst_tensor::{CpuAccess, DType, ImageDesc, PixelFormat, TensorDyn, TensorMemory};
use serial_test::serial;

mod common;

use common::Skip;

type TestResult = Result<(), Box<dyn Error>>;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
/// The rate requested; pipelines that ignore it report what they deliver.
const FPS: f64 = 30.0;
const BUFFERS: usize = 4;
const TIMEOUT: Option<Duration> = Some(Duration::from_secs(3));
/// The ISP service on i.MX 8M Plus BSPs.
const ISP_SERVICE: &str = "imx8-isp.service";
/// The H.264 latency target, capture to encoded access unit.
const ENCODE_TARGET: Duration = Duration::from_millis(100);

/// The camera under test and what its backend captures.
struct Target {
    source: String,
    /// YUYV from V4L2, as the service captures; NV12 from libcamera, which
    /// the i.MX 95 ISP produces.
    format: PixelFormat,
    /// Whether the driver imports the caller's pool. libcamera captures only
    /// into its own buffers.
    import: bool,
}

fn target() -> Option<Target> {
    let source = common::source()?;
    let libcamera = source.starts_with("libcamera:");
    Some(Target {
        source,
        format: if libcamera {
            PixelFormat::Nv12
        } else {
            PixelFormat::Yuyv
        },
        import: !libcamera,
    })
}

/// Whether `device` is the vvcam ISP's node.
fn is_vvcam(device: &str) -> bool {
    edgefirst_camera::enumerate()
        .unwrap_or_default()
        .iter()
        .any(|c| c.backend == Backend::V4l2 && c.id == device && c.driver.contains("viv"))
}

/// The OV5640's V4L2 sub-device, when the board's camera is that sensor
/// (the i.MX 8M Plus EVK's MIPI module, through the ISI).
fn ov5640_subdev() -> Option<PathBuf> {
    std::fs::read_dir("/sys/class/video4linux")
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("v4l-subdev"))
        .find(|e| {
            std::fs::read_to_string(e.path().join("name")).is_ok_and(|n| n.contains("ov5640"))
        })
        .map(|e| Path::new("/dev").join(e.file_name()))
}

/// `V4L2_CID_TEST_PATTERN`; on the OV5640, 1 is colour bars.
const V4L2_CID_TEST_PATTERN: u32 = 0x009f_0903;
/// `VIDIOC_S_CTRL`: `_IOWR('V', 28, struct v4l2_control)`.
const VIDIOC_S_CTRL: u64 = 0xc008_561c;

/// Shows the sensor's colour bars until dropped, so a flip can be seen
/// whatever the camera points at.
struct TestPattern(std::fs::File);

impl TestPattern {
    fn on(subdev: &Path) -> std::io::Result<Self> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(subdev)?;
        set_test_pattern(&f, 1)?;
        Ok(Self(f))
    }
}

impl Drop for TestPattern {
    fn drop(&mut self) {
        let _ = set_test_pattern(&self.0, 0);
    }
}

fn set_test_pattern(f: &std::fs::File, pattern: i32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    // struct v4l2_control { __u32 id; __s32 value; }
    let mut control = [V4L2_CID_TEST_PATTERN as i32, pattern];
    // SAFETY: an open descriptor and a v4l2_control-sized buffer.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), VIDIOC_S_CTRL as _, control.as_mut_ptr()) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// How many times systemd has restarted the ISP daemon this boot.
fn isp_restarts() -> Option<u64> {
    let out = Command::new("systemctl")
        .args(["show", ISP_SERVICE, "-p", "NRestarts", "--value"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Whether the ISP daemon restarted since `before`, waiting up to
/// `window` for it: `isp_media_server` can crash some seconds after a
/// session closes.
fn isp_crashed_since(before: Option<u64>, window: Duration) -> bool {
    let Some(before) = before else {
        return false;
    };
    let deadline = Instant::now() + window;
    loop {
        if isp_restarts().is_some_and(|n| n > before) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn env_secs(name: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default),
    )
}

fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec for the call to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

/// A contiguous pool from the image crate, as the service allocates.
fn pool(
    processor: &ImageProcessor,
    format: PixelFormat,
    n: usize,
) -> Result<Vec<TensorDyn>, Box<dyn Error>> {
    let desc = ImageDesc::new(WIDTH as usize, HEIGHT as usize, format, DType::U8)
        .with_access(CpuAccess::ReadWrite)
        .with_memory(Some(TensorMemory::DmaBuf))
        .with_contiguous(true);
    Ok((0..n)
        .map(|_| processor.create_image_desc(&desc))
        .collect::<Result<_, _>>()?)
}

fn builder(t: &Target) -> CameraBuilder {
    CameraBuilder::source(&t.source)
        .expect("a camera source")
        .size(WIDTH, HEIGHT)
        .format(t.format)
        .frame_rate(FPS)
}

/// [`builder`] with `n` tensors of the caller's pool when the backend
/// imports one, else with the camera's own buffers.
fn pooled(t: &Target, processor: &ImageProcessor, n: usize) -> CameraBuilder {
    if t.import {
        builder(t).with_buffers(pool(processor, t.format, n).unwrap())
    } else {
        builder(t)
    }
}

/// The rate the camera reports, which a pipeline that ignores the request
/// measures from what it delivers.
fn rate(camera: &dyn Camera) -> f64 {
    camera.config().frame_rate.unwrap_or(FPS)
}

/// Opens and starts `builder`, retrying while the ISP is not ready.
fn start(builder: impl Fn() -> CameraBuilder) -> Result<Box<dyn Camera>, Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let attempt = (|| {
            let mut camera = builder().open()?;
            camera.start()?;
            camera.next_frame(TIMEOUT)?;
            Ok::<_, edgefirst_camera::Error>(camera)
        })();
        match attempt {
            Ok(camera) => return Ok(camera),
            Err(e)
                if matches!(e.kind(), ErrorKind::NotReady | ErrorKind::NotFound)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn gl_processor() -> Result<ImageProcessor, Box<dyn Error>> {
    Ok(ImageProcessor::with_config(ImageProcessorConfig {
        backend: ComputeBackend::OpenGl,
        ..ImageProcessorConfig::default()
    })?)
}

fn cache_stats(
    processor: &ImageProcessor,
) -> Result<edgefirst_image::GlCacheStats, Box<dyn Error>> {
    let gl = processor.opengl.as_ref().ok_or("no OpenGL backend")?;
    Ok(gl.egl_cache_stats()?)
}

fn isp(action: &str) -> Result<(), Box<dyn Error>> {
    let status = Command::new("systemctl")
        .args([action, ISP_SERVICE])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("systemctl {action} {ISP_SERVICE}: {status}").into())
    }
}

fn isp_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", ISP_SERVICE])
        .status()
        .is_ok_and(|s| s.success())
}

/// 1080p at 30 fps (or the rate a pipeline holds to) into the caller's
/// image-crate pool where the backend imports one, each frame converted to
/// RGBA by the GL backend: no stall longer than three frame periods, the
/// delivered rate agreeing with the measured one, every frame in the pool, and
/// after warm-up every import a cache hit.
#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn soak_1080p30_into_the_callers_pool_without_stalls() -> TestResult {
    let Some(t) = target() else {
        return Ok(());
    };
    let soak = env_secs("EDGEFIRST_SOAK_SECS", 20);
    let mut processor = gl_processor()?;
    let mut camera = start(|| pooled(&t, &processor, BUFFERS))?;
    // Import with a caller pool means the driver fills the caller's tensors.
    let memory = if t.import {
        ResolvedMemory::Import
    } else {
        ResolvedMemory::Export
    };
    assert_eq!(camera.config().memory, memory);
    assert_eq!(camera.config().format, t.format);
    let buffers = if t.import {
        BUFFERS
    } else {
        camera.config().buffer_count
    };
    let mut rgba = processor.create_image(
        WIDTH as usize,
        HEIGHT as usize,
        PixelFormat::Rgba,
        DType::U8,
        None,
        CpuAccess::ReadWrite,
    )?;

    let mut pool_ids = std::collections::HashSet::new();
    let warm_up = Instant::now() + Duration::from_secs(2);
    while Instant::now() < warm_up {
        let frame = camera.next_frame(TIMEOUT)?;
        pool_ids.insert(frame.tensor().buffer_identity().id());
        processor.convert(frame.tensor(), &mut rgba, Default::default())?;
    }
    let warm = cache_stats(&processor)?;

    let begin = Instant::now();
    let mut frames = 0u64;
    let mut last = None;
    let mut longest = 0;
    while begin.elapsed() < soak {
        let frame = camera.next_frame(TIMEOUT)?;
        let nanos = frame.timestamp().nanos;
        longest = longest.max(nanos - last.unwrap_or(nanos));
        last = Some(nanos);
        let id = frame.tensor().buffer_identity().id();
        assert!(
            pool_ids.contains(&id) || pool_ids.len() < buffers,
            "frame outside the capture pool"
        );
        pool_ids.insert(id);
        processor.convert(frame.tensor(), &mut rgba, Default::default())?;
        frames += 1;
    }
    let elapsed = begin.elapsed().as_secs_f64();
    let steady = cache_stats(&processor)?;
    let fps = frames as f64 / elapsed;
    let reported = rate(camera.as_ref());
    let measured = camera
        .measured_frame_rate()
        .ok_or("no measured frame rate after the soak")?;
    let longest_ms = longest as f64 / 1e6;
    eprintln!(
        "soak: {frames} frames in {elapsed:.1} s ({fps:.2} fps, {reported:.2} reported, {measured:.2} measured), \
         longest interval {longest_ms:.1} ms, pool of {} {:?}, cache warm {warm:?} steady {steady:?}",
        pool_ids.len(),
        camera.config().memory,
    );
    assert_eq!(
        pool_ids.len(),
        buffers,
        "frames land in the {buffers} capture buffers"
    );
    assert!(
        longest_ms < 3000.0 / fps,
        "a stall of {longest_ms:.1} ms at {fps:.2} fps"
    );
    // Measured over the latest frames, it should agree with the whole soak.
    assert!(
        (fps - measured).abs() <= (measured * 0.02).max(0.5),
        "{fps:.2} fps delivered, {measured:.2} measured"
    );
    assert_eq!(
        steady.total_misses(),
        warm.total_misses(),
        "a steady-state frame re-imported its buffer"
    );
    // The HAL imports a frame whole (src) or plane by plane (nv_r8, nv_uv),
    // depending on what the GPU accepts.
    let source_hits = |s: &edgefirst_image::GlCacheStats| s.src.hits + s.nv_r8.hits + s.nv_uv.hits;
    assert!(
        source_hits(&steady) >= source_hits(&warm) + frames,
        "imports missed the cache"
    );
    Ok(())
}

/// Mean luma of a `cols` x `rows` grid over a YUYV or NV12 frame.
fn luma_grid(frame: &Frame, cols: usize, rows: usize) -> Result<Vec<f64>, Box<dyn Error>> {
    let stride = frame.planes()[0].stride;
    // Luma is every other byte of YUYV, and NV12's whole first plane.
    let step = if frame.tensor().format() == Some(PixelFormat::Yuyv) {
        2
    } else {
        1
    };
    let bytes = frame.tensor().map_bytes(CpuAccess::Read)?;
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let mut grid = vec![0.0; cols * rows];
    for (cell, value) in grid.iter_mut().enumerate() {
        let (cx, cy) = (cell % cols, cell / cols);
        let (x0, y0) = (cx * w / cols, cy * h / rows);
        let (x1, y1) = ((cx + 1) * w / cols, (cy + 1) * h / rows);
        let mut sum = 0u64;
        for y in (y0..y1).step_by(4) {
            for x in (x0..x1).step_by(4) {
                sum += u64::from(bytes[y * stride + x * step]);
            }
        }
        *value = sum as f64 / (((y1 - y0) / 4) * ((x1 - x0) / 4)) as f64;
    }
    Ok(grid)
}

fn mean_abs_diff(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f64>() / a.len() as f64
}

/// `Mirror::Both` turns the image by 180 degrees: on vvcam through the
/// VIV flip quirk. Compares a still scene captured both ways. A pipeline
/// that cannot mirror (the i.MX 95 neo) must still stream and say so.
#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn mirror_both_turns_the_image() -> TestResult {
    let Some(t) = target() else {
        return Ok(());
    };
    let (cols, rows) = (16, 9);
    // The OV5640 mirrors through the ISI, but its EVK points at whatever is
    // in front of it, often nothing: its colour bars give the flip a scene.
    let ov5640 = ov5640_subdev().filter(|_| !is_vvcam(&t.source));
    let _pattern = ov5640.as_deref().map(TestPattern::on).transpose()?;
    let capture = |mirror: Mirror| -> Result<(Vec<f64>, ControlValue), Box<dyn Error>> {
        let mut camera = start(|| builder(&t).mirror(mirror))?;
        // Let exposure settle and the flip reach the frames.
        for _ in 0..30 {
            camera.next_frame(TIMEOUT)?;
        }
        let grid = luma_grid(&camera.next_frame(TIMEOUT)?, cols, rows)?;
        Ok((grid, camera.get_control(ControlId::Mirror)?))
    };
    let vvcam = is_vvcam(&t.source);
    let restarts = vvcam.then(isp_restarts).flatten();
    let captured = (|| Ok::<_, Box<dyn Error>>((capture(Mirror::None)?, capture(Mirror::Both)?)))();
    // On the i.MX 8M Plus EVK's vvcam ISP, isp_media_server often crashes
    // after a session that flipped the dewarp unit closes. Waiting it out
    // here keeps the restart away from the tests that follow.
    if vvcam && isp_crashed_since(restarts, Duration::from_secs(25)) {
        let _ = start(|| builder(&t));
        common::known_failure(
            "the vvcam ISP",
            format!(
                "isp_media_server crashed and restarted after a {} session that flipped the \
                 dewarp unit closed{}",
                t.source,
                match &captured {
                    Ok(_) => String::new(),
                    Err(e) => format!(", and the capture failed: {e}"),
                }
            ),
        );
        if captured.is_err() {
            return Ok(());
        }
    }
    let ((plain, _), (turned, applied)) = captured?;
    // `ControlId::Mirror` reads back 3 for both axes.
    if applied != ControlValue::Int(3) {
        common::skip(
            Skip::NoMirror,
            format!(
                "{} cannot mirror (reads back {applied:?}) and kept streaming",
                t.source
            ),
        );
        return Ok(());
    }
    let mut rotated = plain.clone();
    rotated.reverse();
    let mean = plain.iter().sum::<f64>() / plain.len() as f64;
    let spread = mean_abs_diff(&plain, &vec![mean; plain.len()]);
    if spread < 4.0 {
        if ov5640.is_some() {
            // Its colour bars are never uniform: the sensor is delivering
            // nothing. The OV5640 stops after a runtime-PM suspend and
            // resume, until the board reboots; keeping it powered
            // (power/control=on) avoids it.
            common::known_failure(
                "the OV5640",
                format!(
                    "{} delivers a flat image (spread {spread:.1}) with the sensor's test \
                     pattern on: the sensor is not streaming. It stops after a runtime-PM \
                     resume until the board reboots; reboot it and keep it powered.",
                    t.source
                ),
            );
            return Ok(());
        }
        common::skip(
            Skip::UniformScene,
            format!("the scene is too uniform (spread {spread:.1}) to see a flip"),
        );
        return Ok(());
    }
    let same = mean_abs_diff(&plain, &turned);
    let flipped = mean_abs_diff(&rotated, &turned);
    eprintln!(
        "mirror: vvcam={} ov5640 test pattern={} spread {spread:.1}, |plain-turned| \
         {same:.1}, |rot180(plain)-turned| {flipped:.1}",
        is_vvcam(&t.source),
        ov5640.is_some()
    );
    assert!(
        flipped < same / 2.0,
        "the turned frame is not the plain one rotated"
    );
    Ok(())
}

/// Restarting the ISP daemon makes the node report `NotReady` until it
/// serves again, and the camera then opens and delivers.
#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn not_ready_across_an_isp_restart() -> TestResult {
    let Some(t) = target() else {
        return Ok(());
    };
    if !is_vvcam(&t.source) {
        common::skip(Skip::NotVvcam, format!("{} is not the vvcam ISP", t.source));
        return Ok(());
    }
    isp("restart")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut kinds = Vec::new();
    let mut messages = Vec::new();
    loop {
        let attempt = (|| {
            let mut camera = builder(&t).open()?;
            camera.start()?;
            camera.next_frame(TIMEOUT).map(|_| ())
        })();
        match attempt {
            Ok(()) => break,
            Err(e) if Instant::now() < deadline => {
                kinds.push(e.kind());
                messages.push(e.to_string());
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(format!("no frame 60 s after the restart: {e}").into()),
        }
    }
    eprintln!("after the ISP restart: {messages:?}, then frames");
    assert!(
        kinds.contains(&ErrorKind::NotReady),
        "no NotReady while the ISP restarted: {kinds:?}"
    );
    Ok(())
}

/// Repeated stop and start keep delivering, and on vvcam leave the ISP
/// daemon running.
#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn stop_start_cycles_keep_delivering() -> TestResult {
    let Some(t) = target() else {
        return Ok(());
    };
    let vvcam = is_vvcam(&t.source);
    let mut camera = start(|| builder(&t))?;
    for cycle in 0..20 {
        camera.stop()?;
        camera.start()?;
        for _ in 0..3 {
            camera
                .next_frame(TIMEOUT)
                .map_err(|e| format!("cycle {cycle}: {e}"))?;
        }
    }
    drop(camera);
    if vvcam {
        assert!(isp_active(), "{ISP_SERVICE} stopped during the cycles");
    }
    Ok(())
}

/// H.264 of 1080p frames from capture to access unit within the target,
/// the encoder fed the capture buffers directly.
#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn h264_encodes_within_the_latency_target() -> TestResult {
    let Some(t) = target() else {
        return Ok(());
    };
    if !common::h264_encoder() {
        return Ok(());
    }
    let processor = ImageProcessor::new()?;
    let mut camera = start(|| pooled(&t, &processor, BUFFERS + 2))?;
    let mut encoder = VideoEncoder::new(EncoderConfig::h264(
        WIDTH,
        HEIGHT,
        t.format,
        rate(camera.as_ref()),
    ))?;
    let mut latencies = Vec::new();
    for i in 0..300 {
        let frame = camera.next_frame(TIMEOUT)?;
        let ts = frame.timestamp();
        if ts.clock != CaptureClock::Monotonic {
            common::skip(
                Skip::NotMonotonic,
                format!("frames are stamped on {:?}", ts.clock),
            );
            return Ok(());
        }
        let pts = u64::try_from(ts.nanos)?;
        let options = FrameOptions {
            pts,
            ..FrameOptions::default()
        };
        if let Some(au) = encoder.encode(frame.tensor(), &options)? {
            // The first frames include the encoder's start-up.
            if i >= 30 {
                latencies.push(Duration::from_nanos(
                    u64::try_from(monotonic_ns())? - au.pts,
                ));
            }
        }
    }
    latencies.sort();
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[latencies.len() * 95 / 100];
    let max = latencies[latencies.len() - 1];
    eprintln!(
        "h264 {}: capture to access unit p50 {p50:?} p95 {p95:?} max {max:?} over {}",
        encoder.device(),
        latencies.len()
    );
    assert!(p95 < ENCODE_TARGET, "p95 {p95:?} over {ENCODE_TARGET:?}");
    Ok(())
}

/// A camera 2.x recording plays through the SDK's file source on the
/// platform decoder.
#[test]
#[serial]
#[ignore = "requires a recording (set EDGEFIRST_CAMERA_RECORDING on a board)"]
fn decodes_a_2x_recording() -> TestResult {
    let Ok(path) = std::env::var("EDGEFIRST_CAMERA_RECORDING") else {
        common::skip(
            Skip::NoRecording,
            "set EDGEFIRST_CAMERA_RECORDING to a camera 2.x recording",
        );
        return Ok(());
    };
    if !common::h264_decoder() {
        return Ok(());
    }
    let sidecar = edgefirst_camera::Sidecar::load(Path::new(&path))?;
    let mut camera = CameraBuilder::source(&format!("file:{path}"))?
        .frame_rate(1000.0)
        .open()?;
    camera.start()?;
    let config = camera.config().clone();
    assert_eq!(
        (config.width, config.height),
        (sidecar.width, sidecar.height)
    );
    let mut frames = 0;
    while frames < 300 {
        match camera.next_frame(TIMEOUT) {
            Ok(frame) => {
                assert_eq!(frame.tensor().format(), Some(config.format));
                frames += 1;
            }
            Err(e) if e.kind() == ErrorKind::EndOfStream => break,
            Err(e) => return Err(e.into()),
        }
    }
    eprintln!(
        "decoded {frames} frames of {path}: {}x{} {:?}",
        config.width, config.height, config.format
    );
    assert!(frames >= 30, "only {frames} frames");
    Ok(())
}

/// `Auto` must not leave contiguous memory unused where the machine has it,
/// which an NPU or GPU without an IOMMU depends on for zero-copy.
#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn auto_imports_contiguous_memory_where_there_is_a_cma_heap() -> TestResult {
    let Some(t) = target() else {
        return Ok(());
    };
    if !t.import {
        common::skip(
            Skip::NotV4l2,
            format!("{} allocates its own buffers", t.source),
        );
        return Ok(());
    }
    if !Path::new("/dev/dma_heap/linux,cma").exists() {
        common::skip(Skip::NoCmaHeap, "no /dev/dma_heap/linux,cma");
        return Ok(());
    }
    let camera = start(|| builder(&t).memory(MemoryStrategy::Auto))?;
    assert_eq!(camera.config().memory, ResolvedMemory::Import);
    assert_eq!(camera.config().contiguous, Some(true));
    Ok(())
}

/// Frames held across `close()` stay readable and unchanged, and the device
/// reopens at once; for the caller's pool where the backend imports one, and
/// for driver buffers.
#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn frames_held_across_close_stay_readable_and_the_device_reopens() -> TestResult {
    let Some(t) = target() else {
        return Ok(());
    };
    let processor = ImageProcessor::new()?;
    let memories: &[MemoryStrategy] = if t.import {
        &[MemoryStrategy::Import, MemoryStrategy::Export]
    } else {
        &[MemoryStrategy::Export]
    };
    for &memory in memories {
        let make = || {
            let b = builder(&t).memory(memory);
            if memory == MemoryStrategy::Import {
                b.with_buffers(pool(&processor, t.format, BUFFERS).unwrap())
            } else {
                b
            }
        };
        let mut camera = start(make)?;
        let held: Vec<Arc<Frame>> = (0..2)
            .map(|_| camera.next_frame(TIMEOUT).map(Arc::new))
            .collect::<Result<_, _>>()?;
        let before: Vec<Vec<u8>> = held
            .iter()
            .map(|f| f.tensor().map_bytes(CpuAccess::Read).map(|b| b.to_vec()))
            .collect::<Result<_, _>>()?;
        drop(camera);

        let builder = make();
        let reopen = Instant::now();
        let mut again = builder.open()?;
        let reopened = reopen.elapsed();
        again.start()?;
        again.next_frame(TIMEOUT)?;
        let first_frame = reopen.elapsed();
        for _ in 0..30 {
            again.next_frame(TIMEOUT)?;
        }
        drop(again);

        for (frame, bytes) in held.iter().zip(&before) {
            let after = frame.tensor().map_bytes(CpuAccess::Read)?;
            assert!(
                after[..] == bytes[..],
                "{memory:?}: a held frame changed after close"
            );
        }
        eprintln!(
            "{memory:?}: held frames intact; reopened in {reopened:?}, first frame after \
             {first_frame:?}"
        );
        // Opening while the old frames are still held is what proves the
        // device was released; the bound only catches a reopen that waited.
        assert!(
            reopened < Duration::from_secs(2),
            "{memory:?}: reopen took {reopened:?}"
        );
    }
    Ok(())
}

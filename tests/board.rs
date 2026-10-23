// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Platform validation of the SDK with the image and codec crates on a
//! board with a V4L2 camera: the i.MX 8M Plus with an ISI sensor or the
//! vvcam ISP that Maivin and Raivin use.
//!
//! Every test is ignored; run them on a board with `--include-ignored`.
//!
//! - `EDGEFIRST_CAMERA_DEVICE`: the capture node, default `/dev/video3`.
//!   Without it the tests print `SKIPPED` and pass.
//! - `EDGEFIRST_SOAK_SECS`: how long the 1080p30 soak runs, default 20;
//!   600 for the ten-minute platform run.
//! - `EDGEFIRST_RECORDING`: a camera 2.x recording (`.h264` with its
//!   `.json`) for the decode test, which skips without it.
//!
//! vvcam-only tests (the ISP daemon restart, the VIV mirror quirk) check the
//! driver and skip elsewhere. The restart test restarts `imx8-isp.service`.

#![cfg(target_os = "linux")]

use std::{
    error::Error,
    path::Path,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use edgefirst_camera::{
    Backend, Camera, CameraBuilder, CaptureClock, ErrorKind, Frame, MemoryStrategy, Mirror,
    ResolvedMemory,
};
use edgefirst_codec::video::{EncoderConfig, FrameOptions, VideoEncoder};
use edgefirst_image::{
    ComputeBackend, ImageProcessor, ImageProcessorConfig, ImageProcessorTrait as _,
};
use edgefirst_tensor::{CpuAccess, DType, ImageDesc, PixelFormat, TensorDyn, TensorMemory};
use serial_test::serial;

type TestResult = Result<(), Box<dyn Error>>;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FPS: f64 = 30.0;
const BUFFERS: usize = 4;
const TIMEOUT: Option<Duration> = Some(Duration::from_secs(3));
/// The ISP service on i.MX 8M Plus BSPs.
const ISP_SERVICE: &str = "imx8-isp.service";
/// The H.264 latency target, capture to encoded access unit.
const ENCODE_TARGET: Duration = Duration::from_millis(100);

fn device() -> Option<String> {
    let device = std::env::var("EDGEFIRST_CAMERA_DEVICE").unwrap_or_else(|_| "/dev/video3".into());
    if Path::new(&device).exists() {
        Some(device)
    } else {
        eprintln!("SKIPPED: no camera at {device}; set EDGEFIRST_CAMERA_DEVICE");
        None
    }
}

/// Whether `device` is the vvcam ISP's node.
fn is_vvcam(device: &str) -> bool {
    edgefirst_camera::enumerate()
        .unwrap_or_default()
        .iter()
        .any(|c| c.backend == Backend::V4l2 && c.id == device && c.driver.contains("viv"))
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

/// A contiguous YUYV pool from the image crate, as the service allocates.
fn pool(processor: &ImageProcessor, n: usize) -> Result<Vec<TensorDyn>, Box<dyn Error>> {
    let desc = ImageDesc::new(
        WIDTH as usize,
        HEIGHT as usize,
        PixelFormat::Yuyv,
        DType::U8,
    )
    .with_access(CpuAccess::ReadWrite)
    .with_memory(Some(TensorMemory::DmaBuf))
    .with_contiguous(true);
    Ok((0..n)
        .map(|_| processor.create_image_desc(&desc))
        .collect::<Result<_, _>>()?)
}

fn builder(device: &str) -> CameraBuilder {
    CameraBuilder::source(device)
        .expect("a device source")
        .size(WIDTH, HEIGHT)
        .format(PixelFormat::Yuyv)
        .frame_rate(FPS)
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

/// 1080p YUYV at 30 fps into the caller's image-crate pool, each frame
/// converted to RGBA by the GL backend: no unexpected drops, every frame in
/// a pool tensor, and after warm-up every import a cache hit.
#[test]
#[serial]
#[ignore = "requires a V4L2 camera (run with --include-ignored on a board)"]
fn soak_1080p30_into_the_callers_pool_without_drops() -> TestResult {
    let Some(device) = device() else {
        return Ok(());
    };
    let soak = env_secs("EDGEFIRST_SOAK_SECS", 20);
    let mut processor = gl_processor()?;
    let mut camera = start(|| builder(&device).with_buffers(pool(&processor, BUFFERS).unwrap()))?;
    // Import with a caller pool means the driver fills the caller's tensors.
    assert_eq!(camera.config().memory, ResolvedMemory::Import);
    assert_eq!(camera.config().format, PixelFormat::Yuyv);
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
    let dropped_before = camera.stats().dropped;

    let begin = Instant::now();
    let mut frames = 0u64;
    while begin.elapsed() < soak {
        let frame = camera.next_frame(TIMEOUT)?;
        let id = frame.tensor().buffer_identity().id();
        assert!(
            pool_ids.contains(&id) || pool_ids.len() < BUFFERS,
            "frame outside the caller's pool"
        );
        pool_ids.insert(id);
        processor.convert(frame.tensor(), &mut rgba, Default::default())?;
        frames += 1;
    }
    let elapsed = begin.elapsed().as_secs_f64();
    let dropped = camera.stats().dropped - dropped_before;
    let steady = cache_stats(&processor)?;
    let fps = frames as f64 / elapsed;
    eprintln!(
        "soak: {frames} frames in {elapsed:.1} s ({fps:.2} fps), {dropped} dropped, \
         pool of {}, cache warm {warm:?} steady {steady:?}",
        pool_ids.len()
    );
    assert_eq!(
        pool_ids.len(),
        BUFFERS,
        "frames land in the caller's {BUFFERS} tensors"
    );
    assert_eq!(dropped, 0, "unexpected drops");
    assert!(fps > FPS - 0.5, "{fps:.2} fps");
    assert_eq!(
        steady.total_misses(),
        warm.total_misses(),
        "a steady-state frame re-imported its buffer"
    );
    assert!(
        steady.src.hits >= warm.src.hits + frames,
        "imports missed the cache"
    );
    Ok(())
}

/// Mean luma of a `cols` x `rows` grid over a YUYV frame.
fn luma_grid(frame: &Frame, cols: usize, rows: usize) -> Result<Vec<f64>, Box<dyn Error>> {
    let stride = frame.planes()[0].stride;
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
                sum += u64::from(bytes[y * stride + x * 2]);
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
/// VIV flip quirk. Compares a still scene captured both ways.
#[test]
#[serial]
#[ignore = "requires a V4L2 camera (run with --include-ignored on a board)"]
fn mirror_both_turns_the_image() -> TestResult {
    let Some(device) = device() else {
        return Ok(());
    };
    let (cols, rows) = (16, 9);
    let capture = |mirror: Mirror| -> Result<Vec<f64>, Box<dyn Error>> {
        let mut camera = start(|| builder(&device).mirror(mirror))?;
        // Let exposure settle and the flip reach the frames.
        for _ in 0..30 {
            camera.next_frame(TIMEOUT)?;
        }
        luma_grid(&camera.next_frame(TIMEOUT)?, cols, rows)
    };
    let plain = capture(Mirror::None)?;
    let turned = capture(Mirror::Both)?;
    let mut rotated = plain.clone();
    rotated.reverse();
    let mean = plain.iter().sum::<f64>() / plain.len() as f64;
    let spread = mean_abs_diff(&plain, &vec![mean; plain.len()]);
    if spread < 4.0 {
        eprintln!("SKIPPED: the scene is too uniform (spread {spread:.1}) to see a flip");
        return Ok(());
    }
    let same = mean_abs_diff(&plain, &turned);
    let flipped = mean_abs_diff(&rotated, &turned);
    eprintln!(
        "mirror: vvcam={} spread {spread:.1}, |plain-turned| {same:.1}, \
         |rot180(plain)-turned| {flipped:.1}",
        is_vvcam(&device)
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
#[ignore = "requires a V4L2 camera (run with --include-ignored on a board)"]
fn not_ready_across_an_isp_restart() -> TestResult {
    let Some(device) = device() else {
        return Ok(());
    };
    if !is_vvcam(&device) {
        eprintln!("SKIPPED: {device} is not the vvcam ISP");
        return Ok(());
    }
    isp("restart")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut kinds = Vec::new();
    let mut messages = Vec::new();
    loop {
        let attempt = (|| {
            let mut camera = builder(&device).open()?;
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
#[ignore = "requires a V4L2 camera (run with --include-ignored on a board)"]
fn stop_start_cycles_keep_delivering() -> TestResult {
    let Some(device) = device() else {
        return Ok(());
    };
    let vvcam = is_vvcam(&device);
    let mut camera = start(|| builder(&device))?;
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

/// H.264 of 1080p YUYV frames from capture to access unit within the
/// target, the encoder fed the capture buffers directly.
#[test]
#[serial]
#[ignore = "requires a V4L2 camera (run with --include-ignored on a board)"]
fn h264_encodes_within_the_latency_target() -> TestResult {
    let Some(device) = device() else {
        return Ok(());
    };
    let processor = ImageProcessor::new()?;
    let mut camera =
        start(|| builder(&device).with_buffers(pool(&processor, BUFFERS + 2).unwrap()))?;
    let mut encoder =
        VideoEncoder::new(EncoderConfig::h264(WIDTH, HEIGHT, PixelFormat::Yuyv, FPS))?;
    let mut latencies = Vec::new();
    for i in 0..300 {
        let frame = camera.next_frame(TIMEOUT)?;
        let ts = frame.timestamp();
        if ts.clock != CaptureClock::Monotonic {
            eprintln!("SKIPPED: frames are stamped on {:?}", ts.clock);
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
#[ignore = "requires a recording (set EDGEFIRST_RECORDING on a board)"]
fn decodes_a_2x_recording() -> TestResult {
    let Ok(path) = std::env::var("EDGEFIRST_RECORDING") else {
        eprintln!("SKIPPED: set EDGEFIRST_RECORDING to a camera 2.x recording");
        return Ok(());
    };
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

/// Frames held across `close()` stay readable and unchanged, and the device
/// reopens at once; for the caller's pool and for driver buffers.
#[test]
#[serial]
#[ignore = "requires a V4L2 camera (run with --include-ignored on a board)"]
fn frames_held_across_close_stay_readable_and_the_device_reopens() -> TestResult {
    let Some(device) = device() else {
        return Ok(());
    };
    let processor = ImageProcessor::new()?;
    for memory in [MemoryStrategy::Import, MemoryStrategy::Export] {
        let make = || {
            let b = builder(&device).memory(memory);
            if memory == MemoryStrategy::Import {
                b.with_buffers(pool(&processor, BUFFERS).unwrap())
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

// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Helpers shared by the application's integration tests: the binary and
//! its child processes, scratch directories, a Zenoh subscriber session, and
//! the camera the board tests run against.
//!
//! Board lanes set `EDGEFIRST_CAMERA_REQUIRE_BOARD=1`, which turns every
//! [`skip`] into a failure unless the lane names the reason in
//! `EDGEFIRST_CAMERA_ALLOW_SKIPS` (comma- or space-separated [`Skip`] names).

// Each test crate includes this module and uses a different part of it.
#![allow(dead_code)]

use std::{
    fmt::Display,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, ExitStatus},
    thread,
    time::{Duration, Instant},
};

use edgefirst_camera::Backend;
use zenoh::Wait as _;

/// Set to `1` on board lanes: a test that would skip fails instead.
pub const REQUIRE_BOARD: &str = "EDGEFIRST_CAMERA_REQUIRE_BOARD";
/// The [`Skip`] reasons a board lane expects, by name.
pub const ALLOW_SKIPS: &str = "EDGEFIRST_CAMERA_ALLOW_SKIPS";

/// How long a stopped binary may take to exit before it is killed.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Why a board test cannot check what it is for on this target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// `EDGEFIRST_CAMERA_SOURCE` names no camera.
    NoCamera,
    /// The camera does not apply `Mirror::Both`.
    NoMirror,
    /// The scene is too uniform to see a flip.
    UniformScene,
    /// The camera is not the vvcam ISP, so there is no ISP daemon to restart.
    NotVvcam,
    /// Frames are not stamped on `CLOCK_MONOTONIC`.
    NotMonotonic,
    /// `EDGEFIRST_CAMERA_RECORDING` names no recording.
    NoRecording,
    /// The camera is not a V4L2 camera, so it takes no pool from the SDK.
    NotV4l2,
    /// The machine has no `/dev/dma_heap/linux,cma`.
    NoCmaHeap,
    /// The board has no H.264 encoder.
    NoEncoder,
    /// The board has no H.264 decoder.
    NoDecoder,
    /// The board's H.264 encoder does not take the camera's pixel format.
    EncoderFormat,
}

impl Skip {
    pub const ALL: [Skip; 11] = [
        Skip::NoCamera,
        Skip::NoMirror,
        Skip::UniformScene,
        Skip::NotVvcam,
        Skip::NotMonotonic,
        Skip::NoRecording,
        Skip::NotV4l2,
        Skip::NoCmaHeap,
        Skip::NoEncoder,
        Skip::NoDecoder,
        Skip::EncoderFormat,
    ];

    /// The name a lane lists in `EDGEFIRST_CAMERA_ALLOW_SKIPS`.
    pub fn name(self) -> &'static str {
        match self {
            Skip::NoCamera => "no-camera",
            Skip::NoMirror => "no-mirror",
            Skip::UniformScene => "uniform-scene",
            Skip::NotVvcam => "not-vvcam",
            Skip::NotMonotonic => "not-monotonic",
            Skip::NoRecording => "no-recording",
            Skip::NotV4l2 => "not-v4l2",
            Skip::NoCmaHeap => "no-cma-heap",
            Skip::NoEncoder => "no-encoder",
            Skip::NoDecoder => "no-decoder",
            Skip::EncoderFormat => "encoder-format",
        }
    }
}

fn board_required() -> bool {
    std::env::var(REQUIRE_BOARD).as_deref() == Ok("1")
}

/// The reasons the lane allows. Panics on a name that is not a [`Skip`],
/// so a typo in a lane's list cannot silently allow nothing.
fn allowed_skips() -> Vec<Skip> {
    let list = std::env::var(ALLOW_SKIPS).unwrap_or_default();
    list.split([',', ' '])
        .filter(|s| !s.is_empty())
        .map(|name| {
            Skip::ALL
                .into_iter()
                .find(|s| s.name() == name)
                .unwrap_or_else(|| {
                    let known: Vec<_> = Skip::ALL.iter().map(|s| s.name()).collect();
                    panic!("{ALLOW_SKIPS} names {name:?}, which is none of {known:?}")
                })
        })
        .collect()
}

/// Reports that the calling test skips for `reason`. Fails the test instead
/// when the lane requires the board and does not allow `reason`.
/// Reports a failure that is known on this hardware, instead of failing
/// the test. Only for a fault confirmed on the device itself, never as a
/// way around a test.
///
/// nextest hides the output of passing tests, so besides stderr and a
/// GitHub `::warning::` line (an annotation when output is shown), this
/// writes a warning block into the job summary, which always shows on the
/// run's page.
pub fn known_failure(hardware: &str, why: impl Display) {
    let banner = "!".repeat(72);
    eprintln!("{banner}\nKNOWN HARDWARE FAILURE on {hardware}: {why}\n{banner}");
    println!("::warning title=Known hardware failure on {hardware}::{why}");
    if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(summary)
        {
            let _ = writeln!(
                f,
                "\n> [!WARNING]\n> **Known hardware failure on {hardware}:** {why}\n"
            );
        }
    }
}

pub fn skip(reason: Skip, why: impl Display) {
    let allowed = allowed_skips();
    if board_required() && !allowed.contains(&reason) {
        panic!(
            "{REQUIRE_BOARD}=1 and the test would skip ({}): {why}; a lane where that is \
             expected lists `{}` in {ALLOW_SKIPS}",
            reason.name(),
            reason.name()
        );
    }
    eprintln!("SKIPPED ({}): {why}", reason.name());
}

/// The capture size the board tests request: `EDGEFIRST_CAMERA_SIZE` as
/// `WIDTHxHEIGHT`, default 1920x1080.
pub fn camera_size() -> (u32, u32) {
    let Ok(spec) = std::env::var("EDGEFIRST_CAMERA_SIZE") else {
        return (1920, 1080);
    };
    spec.split_once('x')
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .filter(|&(w, h)| w > 0 && h > 0)
        .unwrap_or_else(|| panic!("EDGEFIRST_CAMERA_SIZE={spec:?} is not WIDTHxHEIGHT"))
}

/// The rate the board tests request: `EDGEFIRST_CAMERA_FPS`, default 30.
pub fn camera_fps() -> f64 {
    std::env::var("EDGEFIRST_CAMERA_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|f: &f64| *f > 0.0)
        .unwrap_or(30.0)
}

/// Whether the machine has a CMA heap. Without one, pools the tests
/// allocate are not physically contiguous.
pub fn cma_heap() -> bool {
    std::path::Path::new("/dev/dma_heap/linux,cma").exists()
}

/// The pixel format a source captures in: NV12 from libcamera, YUYV from
/// V4L2, as the service captures.
pub fn capture_format(source: &str) -> edgefirst_tensor::PixelFormat {
    if source.starts_with("libcamera") {
        edgefirst_tensor::PixelFormat::Nv12
    } else {
        edgefirst_tensor::PixelFormat::Yuyv
    }
}

/// Whether the board has an H.264 encoder that takes `format` at
/// [`camera_size`]; reports [`Skip::NoEncoder`] when it has none and
/// [`Skip::EncoderFormat`] when it refuses the format. Any other refusal
/// is left for the test to report.
pub fn h264_encoder(format: edgefirst_tensor::PixelFormat) -> bool {
    use edgefirst_codec::{
        video::{EncoderConfig, VideoEncoder},
        CodecError,
    };
    let (width, height) = camera_size();
    match VideoEncoder::new(EncoderConfig::h264(width, height, format, camera_fps())) {
        Err(CodecError::NoDevice(why)) => {
            skip(Skip::NoEncoder, why);
            false
        }
        Err(CodecError::UnsupportedFormat(format)) => {
            skip(
                Skip::EncoderFormat,
                format!("the H.264 encoder does not take {format:?}"),
            );
            false
        }
        _ => true,
    }
}

/// Whether the board has an H.264 decoder; reports [`Skip::NoDecoder`]
/// when it has none.
pub fn h264_decoder() -> bool {
    use edgefirst_codec::{
        video::{DecoderConfig, VideoDecoder},
        CodecError,
    };
    match VideoDecoder::new(DecoderConfig::h264()) {
        Err(CodecError::NoDevice(why)) => {
            skip(Skip::NoDecoder, why);
            false
        }
        _ => true,
    }
}

/// The source spec from `EDGEFIRST_CAMERA_SOURCE`: a V4L2 node (default
/// `/dev/video3`), `libcamera:<id>`, or `libcamera` for the first camera
/// libcamera lists. `None`, after [`skip`], when it is absent.
pub fn source() -> Option<String> {
    let spec = std::env::var("EDGEFIRST_CAMERA_SOURCE").unwrap_or_else(|_| "/dev/video3".into());
    let found = if spec == "libcamera" || spec.starts_with("libcamera:") {
        edgefirst_camera::enumerate()
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.backend == Backend::Libcamera)
            .map(|c| format!("libcamera:{}", c.id))
            .find(|s| spec == "libcamera" || *s == spec)
    } else {
        Path::new(&spec).exists().then(|| spec.clone())
    };
    if found.is_none() {
        skip(
            Skip::NoCamera,
            format!("no camera at {spec}; set EDGEFIRST_CAMERA_SOURCE"),
        );
    }
    found
}

/// The `edgefirst-camera` binary. nextest remaps this path when running
/// from an archive; the compile-time `CARGO_BIN_EXE_` path names the build
/// host.
pub fn camera_bin() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_edgefirst_camera")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_edgefirst-camera")))
}

/// Waits up to `limit` for `child` to exit.
pub fn wait_with_deadline(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Stops `child` the way systemd does and requires a clean exit within
/// [`SHUTDOWN_GRACE`], returning how long it took. Windows has no signal a
/// test can send to one process, so there it is killed.
pub fn stop(mut child: Child) -> Result<Duration, Box<dyn std::error::Error + Send + Sync>> {
    #[cfg(unix)]
    {
        let pid = libc::pid_t::try_from(child.id())?;
        let sent = Instant::now();
        // SAFETY: `pid` is our own child, which has not been reaped yet.
        if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        match wait_with_deadline(&mut child, SHUTDOWN_GRACE) {
            Some(status) if status.success() => Ok(sent.elapsed()),
            Some(status) => Err(format!("exited with {status} after SIGTERM").into()),
            None => {
                child.kill()?;
                child.wait()?;
                Err(format!("ignored SIGTERM for {SHUTDOWN_GRACE:?}").into())
            }
        }
    }
    #[cfg(not(unix))]
    {
        child.kill()?;
        child.wait()?;
        Ok(Duration::ZERO)
    }
}

/// A fresh directory under the system temporary directory, unique to this
/// process.
pub fn scratch_dir(name: &str) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("edgefirst-camera-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A loopback endpoint no other test is using.
pub fn free_endpoint() -> std::io::Result<String> {
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    Ok(format!("tcp/127.0.0.1:{port}"))
}

/// A Zenoh session that talks only to `endpoint`.
pub fn subscriber_session(endpoint: &str) -> zenoh::Result<zenoh::Session> {
    let mut config = zenoh::Config::default();
    for (key, value) in [
        ("mode", "\"peer\"".to_owned()),
        ("connect/endpoints", format!("[\"{endpoint}\"]")),
        ("listen/endpoints", "[]".to_owned()),
        ("scouting/multicast/enabled", "false".to_owned()),
    ] {
        config.insert_json5(key, &value)?;
    }
    zenoh::open(config).wait()
}

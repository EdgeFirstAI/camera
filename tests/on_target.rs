// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! End-to-end runs of the `edgefirst-camera` binary on a board's camera,
//! named by `EDGEFIRST_CAMERA_SOURCE` (see `common::source`). The capture
//! runs subscribe to the binary over Zenoh, as a downstream service would,
//! and require each topic to start and keep its rate. Each run is stopped
//! with SIGTERM so the binary shuts down through its normal path, which is
//! also what writes its coverage profile.
//!
//! - `EDGEFIRST_CAMERA_FPS`: the rate requested from the camera and expected
//!   on every topic, default 30.

#![cfg(target_os = "linux")]

use edgefirst_schemas::edgefirst_msgs::CameraFrame;
use serial_test::serial;
use std::{
    error::Error,
    ffi::OsStr,
    fmt::Debug,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use zenoh::Wait as _;

mod common;

use common::{camera_bin, free_endpoint, scratch_dir, subscriber_session, wait_with_deadline};

type TestResult = Result<(), Box<dyn Error + Send + Sync>>;

/// How long a stopped binary may take to exit.
const SHUTDOWN_BOUND: Duration = Duration::from_secs(3);
/// The same with coverage on: the binary writes its profile as it exits,
/// which on a board's SD card, behind other tests' profiles, can take
/// seconds. Below [`common::SHUTDOWN_GRACE`].
const SHUTDOWN_BOUND_WITH_COVERAGE: Duration = Duration::from_secs(8);
/// From start-up to the first sample on every topic.
const FIRST_SAMPLE: Duration = Duration::from_secs(20);
/// How long each topic's rate is measured, from its first sample.
const RATE_WINDOW: Duration = Duration::from_secs(10);
/// The share of the expected rate a topic must keep.
const MIN_RATE: f64 = 0.9;

const FRAME_TOPIC: &str = "camera/frame";
const JPEG_TOPIC: &str = "camera/jpeg";
const H264_TOPIC: &str = "camera/h264";

/// What a topic must deliver over the [`RATE_WINDOW`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// At least [`MIN_RATE`] of the requested rate: capture, and outputs
    /// a hardware encoder produces.
    Rate,
    /// Samples in both halves of the window, at whatever rate: outputs
    /// compressed in software, which no platform can promise a rate for.
    /// JPEG is compressed with libjpeg-turbo on every platform; a platform
    /// that gains a hardware JPEG encoder can expect a rate there.
    Delivers,
}

/// The rate requested from the camera and expected of [`Expect::Rate`]
/// topics.
fn expected_fps() -> f64 {
    common::camera_fps()
}

fn spawn<S: AsRef<OsStr>>(args: &[S]) -> std::io::Result<Child> {
    Command::new(camera_bin())
        .args(args)
        .stdin(Stdio::null())
        .spawn()
}

/// Sends SIGTERM and requires a clean exit within [`SHUTDOWN_BOUND`], or
/// [`SHUTDOWN_BOUND_WITH_COVERAGE`] when coverage is collected.
fn stop<S: AsRef<OsStr> + Debug>(child: Child, args: &[S]) -> TestResult {
    let bound = if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
        SHUTDOWN_BOUND_WITH_COVERAGE
    } else {
        SHUTDOWN_BOUND
    };
    let took = common::stop(child).map_err(|e| format!("{args:?}: {e}"))?;
    eprintln!("stopped {took:?} after SIGTERM");
    if took > bound {
        return Err(format!("{args:?} took {took:?} to stop, over {bound:?}").into());
    }
    Ok(())
}

/// Runs the binary for `duration`, requires it to still be running at the
/// end, then requires a prompt, clean exit after SIGTERM.
fn run_for<S: AsRef<OsStr> + Debug>(args: &[S], duration: Duration) -> TestResult {
    let mut child = spawn(args)?;
    if let Some(status) = wait_with_deadline(&mut child, duration) {
        return Err(format!("{args:?} exited early with {status}").into());
    }
    stop(child, args)
}

/// Arrivals on one topic, with the frame `seq` where the topic carries one.
#[derive(Default)]
struct Arrivals {
    times: Vec<Instant>,
    seqs: Vec<u64>,
    errors: Vec<String>,
}

/// Runs the binary with `args`, subscribed to `topics`. Requires the first
/// sample on each within [`FIRST_SAMPLE`], then what each topic's
/// [`Expect`] asks over the [`RATE_WINDOW`] after it, frame `seq` that
/// never goes back, and a prompt, clean exit after SIGTERM.
fn stream(args: &[String], expected: &[(&'static str, Expect)]) -> TestResult {
    let topics: Vec<&'static str> = expected.iter().map(|&(t, _)| t).collect();
    let endpoint = free_endpoint()?;
    let mut args = args.to_vec();
    args.extend([
        "--listen".into(),
        endpoint.clone(),
        "--no-multicast-scouting".into(),
    ]);
    let started = Instant::now();
    let mut child = spawn(&args)?;

    let arrivals: Vec<Arc<Mutex<Arrivals>>> = topics.iter().map(|_| Arc::default()).collect();
    let watched = (|| -> TestResult {
        let session = subscriber_session(&endpoint)?;
        let _subscribers = subscribe(&session, &topics, &arrivals)?;
        watch_windows(&mut child, &args, &topics, &arrivals, started)
    })();
    let stopped = stop(child, &args);
    watched?;
    stopped?;

    let fps = expected_fps();
    let failures: Vec<String> = expected
        .iter()
        .zip(&arrivals)
        .flat_map(|(&(topic, expect), log)| log.lock().unwrap().failures(topic, expect, fps))
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; ").into())
    }
}

/// One subscriber per topic, logging each sample's arrival in `arrivals`.
fn subscribe(
    session: &zenoh::Session,
    topics: &[&'static str],
    arrivals: &[Arc<Mutex<Arrivals>>],
) -> zenoh::Result<Vec<zenoh::pubsub::Subscriber<()>>> {
    topics
        .iter()
        .zip(arrivals)
        .map(|(&topic, log)| {
            let log = Arc::clone(log);
            // The binary namespaces its keys with the host name.
            session
                .declare_subscriber(format!("*/{topic}"))
                .callback(move |sample| log.lock().unwrap().record(topic, &sample))
                .wait()
        })
        .collect()
}

/// Fails if the binary has exited.
fn still_running(child: &mut Child, args: &[String]) -> TestResult {
    match child.try_wait()? {
        Some(status) => Err(format!("{args:?} exited early with {status}").into()),
        None => Ok(()),
    }
}

/// Waits for the first sample on every topic, within [`FIRST_SAMPLE`] of
/// `started`, then for the last topic's [`RATE_WINDOW`] to close, with the
/// binary running throughout.
fn watch_windows(
    child: &mut Child,
    args: &[String],
    topics: &[&'static str],
    arrivals: &[Arc<Mutex<Arrivals>>],
    started: Instant,
) -> TestResult {
    let last_first = loop {
        still_running(child, args)?;
        let firsts: Vec<_> = arrivals
            .iter()
            .map(|log| log.lock().unwrap().times.first().copied())
            .collect();
        if let Some(last) = firsts.iter().copied().collect::<Option<Vec<_>>>() {
            break last.into_iter().max().ok_or("no topics")?;
        }
        if started.elapsed() > FIRST_SAMPLE {
            let missing: Vec<_> = topics
                .iter()
                .zip(&firsts)
                .filter(|(_, f)| f.is_none())
                .map(|(t, _)| *t)
                .collect();
            return Err(format!("no sample on {missing:?} within {FIRST_SAMPLE:?}").into());
        }
        thread::sleep(Duration::from_millis(100));
    };
    eprintln!(
        "every topic started {:?} after launch",
        last_first - started
    );
    // Every window has closed once the last topic's has.
    let end = last_first + RATE_WINDOW + Duration::from_millis(500);
    while Instant::now() < end {
        still_running(child, args)?;
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

impl Arrivals {
    /// Logs one sample of `topic`, with its `seq` on `camera/frame`.
    fn record(&mut self, topic: &str, sample: &zenoh::sample::Sample) {
        self.times.push(Instant::now());
        if topic == FRAME_TOPIC {
            match CameraFrame::from_cdr(sample.payload().to_bytes().into_owned()) {
                Ok(frame) => self.seqs.push(frame.seq()),
                Err(e) => self.errors.push(e.to_string()),
            }
        }
    }

    /// What `topic` failed of `expect` over its [`RATE_WINDOW`], at the
    /// requested `fps`, and of its samples' contents.
    fn failures(&self, topic: &str, expect: Expect, fps: f64) -> Vec<String> {
        let mut failures = Vec::new();
        let first = self.times[0];
        let in_window: Vec<Duration> = self
            .times
            .iter()
            .map(|&t| t.duration_since(first))
            .filter(|&d| d < RATE_WINDOW)
            .collect();
        let count = in_window.len();
        let rate = count as f64 / RATE_WINDOW.as_secs_f64();
        match expect {
            Expect::Rate => {
                let wanted = MIN_RATE * fps * RATE_WINDOW.as_secs_f64();
                eprintln!(
                    "{topic}: {count} samples in {RATE_WINDOW:?} ({rate:.2}/s, expected {fps})"
                );
                if (count as f64) < wanted {
                    failures.push(format!(
                        "{topic}: {count} samples in {RATE_WINDOW:?}, under {wanted:.0} \
                         ({MIN_RATE} of {fps}/s)"
                    ));
                }
            }
            Expect::Delivers => {
                eprintln!(
                    "{topic}: {count} samples in {RATE_WINDOW:?} ({rate:.2}/s, no rate expected)"
                );
                if !in_window.iter().any(|&d| d >= RATE_WINDOW / 2) {
                    failures.push(format!(
                        "{topic}: no sample in the second half of {RATE_WINDOW:?}; it stopped"
                    ));
                }
            }
        }
        if !self.errors.is_empty() {
            failures.push(format!("{topic}: undecodable samples: {:?}", self.errors));
        }
        if let Some(w) = self.seqs.windows(2).find(|w| w[1] < w[0]) {
            failures.push(format!("{topic}: seq went back from {} to {}", w[0], w[1]));
        }
        if topic == FRAME_TOPIC && self.seqs.len() != self.times.len() {
            failures.push(format!(
                "{topic}: {} of {} samples carried a seq",
                self.seqs.len(),
                self.times.len()
            ));
        }
        failures
    }
}

/// `args` capturing from the board's camera at the lane's size and rate,
/// and streaming at that size, or `None` without a camera. Unset, the app
/// keeps the sensor's current size, which after a reboot can be smaller than
/// the default 1080p stream (the OV5640 comes up at 640x480).
fn capture(args: &[&str]) -> Option<Vec<String>> {
    let source = common::source()?;
    let fps = expected_fps().to_string();
    let (width, height) = common::camera_size();
    let (width, height) = (width.to_string(), height.to_string());
    let camera = [
        "--source",
        &source,
        "--camera-size",
        &width,
        &height,
        "--camera-fps",
        &fps,
        "--stream-size",
        &width,
        &height,
    ];
    Some(camera.iter().chain(args).map(ToString::to_string).collect())
}

/// Whether the board's H.264 encoder takes what the camera captures.
fn encoder_takes_the_camera() -> bool {
    let source = std::env::var("EDGEFIRST_CAMERA_SOURCE").unwrap_or_else(|_| "/dev/video3".into());
    common::h264_encoder(common::capture_format(&source))
}

fn sidecar(recording: &Path) -> PathBuf {
    recording.with_extension("json")
}

#[test]
fn help_exits_cleanly() -> TestResult {
    let status = Command::new(camera_bin())
        .arg("--help")
        .stdout(Stdio::null())
        .status()?;
    assert!(status.success(), "--help exited with {status}");
    Ok(())
}

/// JPEG on its own, as on a board without an H.264 encoder: the two
/// compressed outputs are alternatives.
#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn jpeg_capture() -> TestResult {
    let Some(args) = capture(&["--jpeg"]) else {
        return Ok(());
    };
    stream(
        &args,
        &[(FRAME_TOPIC, Expect::Rate), (JPEG_TOPIC, Expect::Delivers)],
    )
}

/// H.264 on its own, where the board has an encoder for the camera's format.
#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn h264_capture() -> TestResult {
    if !encoder_takes_the_camera() {
        return Ok(());
    }
    let Some(args) = capture(&["--h264"]) else {
        return Ok(());
    };
    stream(
        &args,
        &[(FRAME_TOPIC, Expect::Rate), (H264_TOPIC, Expect::Rate)],
    )
}

#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn record_then_replay() -> TestResult {
    if !encoder_takes_the_camera() || !common::h264_decoder() {
        return Ok(());
    }
    let dir = scratch_dir("record")?;
    let recording = dir.join("record.h264");
    let result = (|| -> TestResult {
        let path = recording.to_str().ok_or("non-UTF-8 temp path")?;
        let Some(record) = capture(&["--h264", "--record", path]) else {
            return Ok(());
        };
        run_for(&record, Duration::from_secs(10))?;

        let size = std::fs::metadata(&recording)?.len();
        assert!(size > 0, "recording {} is empty", recording.display());
        let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(sidecar(&recording))?)?;
        assert!(meta.is_object(), "sidecar is not a JSON object: {meta}");

        let source = format!("file:{path}");
        run_for(
            &["--source", &source, "--replay-loop", "--jpeg"],
            Duration::from_secs(10),
        )
    })();
    std::fs::remove_dir_all(&dir)?;
    result
}

#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn record_subcommand_writes_a_playable_recording() -> TestResult {
    if !encoder_takes_the_camera() || !common::h264_decoder() {
        return Ok(());
    }
    let dir = scratch_dir("record-sub")?;
    let recording = dir.join("record.h264");
    let result = (|| -> TestResult {
        let path = recording.to_str().ok_or("non-UTF-8 temp path")?;
        let Some(camera) = capture(&[]) else {
            return Ok(());
        };
        let mut args: Vec<String> =
            vec!["record".into(), path.into(), "--frames".into(), "90".into()];
        args.extend(camera);
        let mut child = spawn(&args)?;
        match wait_with_deadline(&mut child, Duration::from_secs(30)) {
            Some(status) if status.success() => {}
            Some(status) => return Err(format!("{args:?} exited with {status}").into()),
            None => {
                child.kill()?;
                child.wait()?;
                return Err(format!("{args:?} did not stop after 90 frames").into());
            }
        }
        assert!(std::fs::metadata(&recording)?.len() > 0);
        let source = format!("file:{path}");
        run_for(
            &["--source", &source, "--replay-loop", "--jpeg"],
            Duration::from_secs(5),
        )
    })();
    std::fs::remove_dir_all(&dir)?;
    result
}

#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn devices_reports_the_cameras_modes() -> TestResult {
    let Some(source) = common::source() else {
        return Ok(());
    };
    // `devices` names V4L2 cameras by their /dev/videoN node, which a
    // by-id link resolves to.
    let (backend, id) = match source.strip_prefix("libcamera:") {
        Some(id) => ("libcamera", id.to_owned()),
        None => (
            "v4l2",
            std::fs::canonicalize(&source)?
                .to_string_lossy()
                .into_owned(),
        ),
    };
    let (width, height) = common::camera_size();
    let out = Command::new(camera_bin())
        .args(["devices", "--json"])
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let devices: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let cameras = devices["cameras"].as_array().ok_or("no cameras array")?;
    let formats: Vec<&serde_json::Value> = cameras
        .iter()
        .filter(|c| c["backend"] == backend && c["id"] == id)
        .flat_map(|c| c["formats"].as_array().into_iter().flatten())
        .collect();
    // Rates differ by camera: neo offers 1080p only at the rate it holds to,
    // and imx8-isi and NXP's mxc-isi_v1 report none, which names no mode
    // but still lists the size. Stepwise ranges list modes only.
    let mode = formats
        .iter()
        .flat_map(|f| f["modes"].as_array().into_iter().flatten())
        .filter_map(|m| m.as_str())
        .any(|m| mode_is_size(m, width, height));
    let size = formats
        .iter()
        .flat_map(|f| f["sizes"].as_array().into_iter().flatten())
        .any(|s| s["width"] == width && s["height"] == height);
    assert!(
        mode || size,
        "{source} offers no {width}x{height}: {devices}"
    );
    Ok(())
}

/// Whether a mode name (`1080p30`, `VGA30`, `752x416@5`) is of the given size.
fn mode_is_size(mode: &str, width: u32, height: u32) -> bool {
    match mode.split_once('@') {
        Some((size, _)) => size == format!("{width}x{height}"),
        None => {
            let name = mode.trim_end_matches(|c: char| c.is_ascii_digit());
            edgefirst_camera::named_size(name) == Some((width, height))
        }
    }
}

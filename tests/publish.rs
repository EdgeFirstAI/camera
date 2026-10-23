// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Runs the `edgefirst-camera` binary on the mock source and subscribes to
//! it over Zenoh, as a downstream service would. Needs no camera, so it runs
//! on every host, macOS and Windows included.

use edgefirst_schemas::edgefirst_msgs::CameraFrame;
use std::{
    error::Error,
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};
use zenoh::Wait;

type BoxError = Box<dyn Error + Send + Sync>;

const FRAMES: usize = 100;
/// Covers a cold debug build's startup on a hosted runner.
const DEADLINE: Duration = Duration::from_secs(60);
#[cfg(unix)]
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

fn camera_bin() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_edgefirst_camera")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_edgefirst-camera")))
}

/// A loopback endpoint no other test is using.
fn free_endpoint() -> Result<String, BoxError> {
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    Ok(format!("tcp/127.0.0.1:{port}"))
}

/// A session that talks only to `endpoint`.
fn subscriber_session(endpoint: &str) -> zenoh::Result<zenoh::Session> {
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

fn wait_with_deadline(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Stops `child` the way systemd does and requires a clean exit. Windows
/// has no signal a test can send to one process, so there it is killed.
fn stop(mut child: Child) -> Result<(), BoxError> {
    #[cfg(unix)]
    {
        let pid = libc::pid_t::try_from(child.id())?;
        // SAFETY: `pid` is our own child, which has not been reaped yet.
        if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        match wait_with_deadline(&mut child, SHUTDOWN_GRACE) {
            Some(status) if status.success() => Ok(()),
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
        Ok(())
    }
}

#[test]
fn a_subscriber_receives_camera_frames() -> Result<(), BoxError> {
    let endpoint = free_endpoint()?;
    let topic = format!("edgefirst-camera-test-{}/frame", std::process::id());
    let mut child = Command::new(camera_bin())
        .args([
            "--source",
            "mock:320x240@30",
            "--h264",
            "false",
            "--frame-topic",
            &topic,
            "--listen",
            &endpoint,
            "--no-multicast-scouting",
        ])
        .stdin(Stdio::null())
        .spawn()?;

    let received = (|| -> Result<Vec<CameraFrame<Vec<u8>>>, BoxError> {
        let session = subscriber_session(&endpoint)?;
        // The binary namespaces its keys with the host name.
        let subscriber = session.declare_subscriber(format!("**/{topic}")).wait()?;
        let deadline = Instant::now() + DEADLINE;
        let mut frames = Vec::with_capacity(FRAMES);
        while frames.len() < FRAMES {
            if let Some(status) = child.try_wait()? {
                return Err(format!("exited early with {status}").into());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("{} of {FRAMES} frames in {DEADLINE:?}", frames.len()).into());
            }
            if let Ok(Some(sample)) = subscriber.recv_timeout(left.min(Duration::from_secs(1))) {
                frames.push(CameraFrame::from_cdr(
                    sample.payload().to_bytes().into_owned(),
                )?);
            }
        }
        Ok(frames)
    })();
    let stopped = stop(child);
    let frames = received?;
    stopped?;

    for frame in &frames {
        let tensor = frame.tensor();
        assert_eq!(tensor.shape().collect::<Vec<_>>(), [240, 320, 2]);
        assert!(tensor.num_planes() >= 1);
        for plane in tensor.planes() {
            // Off Linux the capture pool is process memory, which only
            // travels inline. On Linux it may be DMA-BUF, sent by reference.
            if cfg!(not(target_os = "linux")) {
                assert!(plane.is_inline(), "plane sent by reference: {plane:?}");
            }
            if plane.is_inline() {
                assert!(!plane.data.is_empty(), "inline plane without data");
            }
        }
    }
    let seqs: Vec<_> = frames.iter().map(CameraFrame::seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "out of order: {seqs:?}"
    );
    Ok(())
}

/// No H.264 encoder off Linux: the default `--h264` stops the service at
/// startup and says how to run without it.
#[cfg(not(target_os = "linux"))]
#[test]
fn h264_without_an_encoder_fails_at_startup() -> Result<(), BoxError> {
    use std::io::Read as _;

    let mut child = Command::new(camera_bin())
        .args([
            "--source",
            "mock:64x48@30",
            "--stream-size",
            "64",
            "48",
            "--listen",
            &free_endpoint()?,
            "--no-multicast-scouting",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let Some(status) = wait_with_deadline(&mut child, DEADLINE) else {
        child.kill()?;
        child.wait()?;
        return Err(format!("still running after {DEADLINE:?} with no encoder").into());
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .ok_or("stderr not captured")?
        .read_to_string(&mut stderr)?;
    assert!(!status.success(), "started without an encoder: {stderr}");
    assert!(stderr.contains("--h264 false"), "{stderr}");
    Ok(())
}

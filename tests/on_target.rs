// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! End-to-end runs of the `edgefirst-camera` binary on a board's camera,
//! named by `EDGEFIRST_CAMERA_SOURCE` (see `common::source`). Each run is
//! stopped with SIGTERM so the binary shuts down through its normal path,
//! which is also what writes its coverage profile.

#![cfg(target_os = "linux")]

use serial_test::serial;
use std::{
    error::Error,
    ffi::OsStr,
    fmt::Debug,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

mod common;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// nextest remaps this path when running from an archive; the compile-time
/// `CARGO_BIN_EXE_` path names the build host.
fn camera_bin() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_edgefirst_camera")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_edgefirst-camera")))
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

/// Runs the binary for `duration`, requires it to still be running at the
/// end, then requires a clean exit after SIGTERM.
fn run_for<S: AsRef<OsStr> + Debug>(args: &[S], duration: Duration) -> Result<(), Box<dyn Error>> {
    let mut child = Command::new(camera_bin())
        .args(args)
        .stdin(Stdio::null())
        .spawn()?;

    if let Some(status) = wait_with_deadline(&mut child, duration) {
        return Err(format!("{args:?} exited early with {status}").into());
    }

    let pid = libc::pid_t::try_from(child.id())?;
    // SAFETY: `pid` is our own child, which has not been reaped yet.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    match wait_with_deadline(&mut child, SHUTDOWN_GRACE) {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(format!("{args:?} exited with {status} after SIGTERM").into()),
        None => {
            child.kill()?;
            child.wait()?;
            Err(format!("{args:?} ignored SIGTERM for {SHUTDOWN_GRACE:?}").into())
        }
    }
}

fn scratch_dir(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let dir = std::env::temp_dir().join(format!("edgefirst-camera-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// `args` capturing from the board's camera at 1080p, or `None` without a
/// camera. Unset, the app keeps the sensor's current size, which after a
/// reboot can be smaller than the default 1080p stream (the OV5640 comes up
/// at 640x480).
fn capture(args: &[&str]) -> Option<Vec<String>> {
    let source = common::source()?;
    let size = ["--source", &source, "--camera-size", "1920", "1080"];
    Some(size.iter().chain(args).map(|s| s.to_string()).collect())
}

fn sidecar(recording: &Path) -> PathBuf {
    recording.with_extension("json")
}

#[test]
fn help_exits_cleanly() -> Result<(), Box<dyn Error>> {
    let status = Command::new(camera_bin())
        .arg("--help")
        .stdout(Stdio::null())
        .status()?;
    assert!(status.success(), "--help exited with {status}");
    Ok(())
}

#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn jpeg_capture() -> Result<(), Box<dyn Error>> {
    let Some(args) = capture(&["--jpeg"]) else {
        return Ok(());
    };
    run_for(&args, Duration::from_secs(15))
}

#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn h264_capture() -> Result<(), Box<dyn Error>> {
    let Some(args) = capture(&["--h264"]) else {
        return Ok(());
    };
    run_for(&args, Duration::from_secs(15))
}

#[test]
#[serial]
#[ignore = "requires a camera (run with --include-ignored on a board)"]
fn record_then_replay() -> Result<(), Box<dyn Error>> {
    let dir = scratch_dir("record")?;
    let recording = dir.join("record.h264");
    let result = (|| -> Result<(), Box<dyn Error>> {
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
fn record_subcommand_writes_a_playable_recording() -> Result<(), Box<dyn Error>> {
    let dir = scratch_dir("record-sub")?;
    let recording = dir.join("record.h264");
    let result = (|| -> Result<(), Box<dyn Error>> {
        let path = recording.to_str().ok_or("non-UTF-8 temp path")?;
        let Some(camera) = capture(&[]) else {
            return Ok(());
        };
        let mut args: Vec<String> =
            vec!["record".into(), path.into(), "--frames".into(), "90".into()];
        args.extend(camera);
        let mut child = Command::new(camera_bin())
            .args(&args)
            .stdin(Stdio::null())
            .spawn()?;
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
fn devices_reports_the_cameras_modes() -> Result<(), Box<dyn Error>> {
    let Some(source) = common::source() else {
        return Ok(());
    };
    let (backend, id) = match source.strip_prefix("libcamera:") {
        Some(id) => ("libcamera", id),
        None => ("v4l2", source.as_str()),
    };
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
    let modes: Vec<&str> = cameras
        .iter()
        .filter(|c| c["backend"] == backend && c["id"] == id)
        .flat_map(|c| c["formats"].as_array().into_iter().flatten())
        .flat_map(|f| f["modes"].as_array().into_iter().flatten())
        .filter_map(|m| m.as_str())
        .collect();
    // The i.MX 95 neo pipeline offers 1080p only at the rate it holds to.
    let wanted = if backend == "libcamera" {
        "1080p"
    } else {
        "1080p30"
    };
    assert!(
        modes.iter().any(|m| m.starts_with(wanted)),
        "{source}: {devices}"
    );
    Ok(())
}

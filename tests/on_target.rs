// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! End-to-end runs of the `edgefirst-camera` binary on a board with a V4L2
//! camera. Each run is stopped with SIGTERM so the binary shuts down through
//! its normal path, which is also what writes its coverage profile.

use serial_test::serial;
use std::{
    error::Error,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

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
fn run_for(args: &[&str], duration: Duration) -> Result<(), Box<dyn Error>> {
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
#[ignore = "requires a V4L2 camera (run with --include-ignored on a board)"]
fn jpeg_capture() -> Result<(), Box<dyn Error>> {
    run_for(&["--jpeg"], Duration::from_secs(15))
}

#[test]
#[serial]
#[ignore = "requires a V4L2 camera (run with --include-ignored on a board)"]
fn h264_capture() -> Result<(), Box<dyn Error>> {
    run_for(&["--h264"], Duration::from_secs(15))
}

#[test]
#[serial]
#[ignore = "requires a V4L2 camera (run with --include-ignored on a board)"]
fn record_then_replay() -> Result<(), Box<dyn Error>> {
    let dir = scratch_dir("record")?;
    let recording = dir.join("record.h264");
    let result = (|| -> Result<(), Box<dyn Error>> {
        let path = recording.to_str().ok_or("non-UTF-8 temp path")?;
        run_for(&["--h264", "--record", path], Duration::from_secs(10))?;

        let size = std::fs::metadata(&recording)?.len();
        assert!(size > 0, "recording {} is empty", recording.display());
        let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(sidecar(&recording))?)?;
        assert!(meta.is_object(), "sidecar is not a JSON object: {meta}");

        run_for(
            &["--replay", path, "--replay-loop"],
            Duration::from_secs(10),
        )
    })();
    std::fs::remove_dir_all(&dir)?;
    result
}

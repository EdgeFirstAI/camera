// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The file source through the public API.
//!
//! Opening errors are tested everywhere. Playback needs a hardware H.264
//! decoder and a recording: set `EDGEFIRST_CAMERA_RECORDING` to a camera
//! 2.x `.h264` file (its `.json` sidecar beside it) on an M2M-capable
//! target. Without it the playback tests pass with a note.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use edgefirst_camera::{CameraBuilder, ErrorKind};
use edgefirst_codec::video::access_units;

/// A scratch directory under the target dir, unique to the test.
fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const SIDECAR: &str = r#"{"version":1,"codec":"h264","fps":30,"width":64,"height":48}"#;

#[test]
fn a_missing_recording_is_not_found() {
    let err = CameraBuilder::source("file:/nonexistent/clip.h264")
        .unwrap()
        .open()
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound, "{err}");
}

#[test]
fn a_missing_sidecar_is_not_found() {
    let dir = scratch("a_missing_sidecar_is_not_found");
    let clip = dir.join("clip.h264");
    std::fs::write(&clip, [0, 0, 0, 1, 0x67]).unwrap();
    let _ = std::fs::remove_file(dir.join("clip.json"));
    let err = CameraBuilder::source(&format!("file:{}", clip.display()))
        .unwrap()
        .open()
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound, "{err}");
    assert!(err.to_string().contains("clip.json"), "{err}");
}

#[test]
fn an_unsupported_sidecar_is_refused() {
    let dir = scratch("an_unsupported_sidecar_is_refused");
    let clip = dir.join("clip.h264");
    std::fs::write(&clip, [0, 0, 0, 1, 0x67]).unwrap();
    std::fs::write(dir.join("clip.json"), SIDECAR.replace("h264\"", "h265\"")).unwrap();
    let err = CameraBuilder::source(&format!("file:{}", clip.display()))
        .unwrap()
        .open()
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnsupportedFormat, "{err}");
}

/// Off Linux there is no decoder in phase 1: opening names the decoder.
#[cfg(not(target_os = "linux"))]
#[test]
fn without_a_decoder_opening_is_not_found() {
    let dir = scratch("without_a_decoder_opening_is_not_found");
    let clip = dir.join("clip.h264");
    std::fs::write(&clip, [0, 0, 0, 1, 0x67]).unwrap();
    std::fs::write(dir.join("clip.json"), SIDECAR).unwrap();
    let err = CameraBuilder::source(&format!("file:{}", clip.display()))
        .unwrap()
        .open()
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound, "{err}");
    assert!(err.to_string().contains("decoder"), "{err}");
}

/// The recording named by `EDGEFIRST_CAMERA_RECORDING`, its picture count
/// and sidecar rate.
fn recording() -> Option<(String, usize, f64)> {
    let Ok(path) = std::env::var("EDGEFIRST_CAMERA_RECORDING") else {
        eprintln!("EDGEFIRST_CAMERA_RECORDING is not set; skipping playback");
        return None;
    };
    let bytes = std::fs::read(&path).expect("read the recording");
    let sidecar: serde_json::Value = serde_json::from_slice(
        &std::fs::read(Path::new(&path).with_extension("json")).expect("read the sidecar"),
    )
    .expect("parse the sidecar");
    let fps = sidecar["fps"].as_f64().expect("sidecar fps");
    Some((format!("file:{path}"), access_units(&bytes).count(), fps))
}

/// A 2.x recording plays every picture at the sidecar's rate, then ends.
#[test]
fn a_recording_plays_at_its_rate_and_ends() {
    let Some((source, units, fps)) = recording() else {
        return;
    };
    let mut camera = CameraBuilder::source(&source).unwrap().open().unwrap();
    assert_eq!(camera.config().frame_rate, Some(fps));
    assert!(camera.extras().is_some_and(|e| e.contains("camera_info")));
    camera.start().unwrap();
    let mut frames = 0usize;
    let mut first = None;
    let mut last = 0i64;
    let err = loop {
        match camera.next_frame(Some(Duration::from_secs(5))) {
            Ok(frame) => {
                assert_eq!(frame.seq(), frames as u64);
                first.get_or_insert(frame.timestamp().nanos);
                last = frame.timestamp().nanos;
                frames += 1;
            }
            Err(e) => break e,
        }
    };
    assert_eq!(err.kind(), ErrorKind::EndOfStream, "{err}");
    assert_eq!(frames, units, "one frame per access unit");
    assert_eq!(camera.stats().errors, 0);
    let measured = (frames - 1) as f64 / ((last - first.unwrap()) as f64 / 1e9);
    eprintln!("{frames} frames of {units} access units at {measured:.3} fps (sidecar {fps})");
    assert!(
        (measured - fps).abs() / fps < 0.02,
        "played at {measured:.2} fps, the sidecar says {fps}"
    );
}

/// Looping restarts the file and `seq` keeps counting.
#[test]
fn a_looping_recording_keeps_seq_monotonic() {
    let Some((source, units, _)) = recording() else {
        return;
    };
    let mut camera = CameraBuilder::source(&source)
        .unwrap()
        .looping(true)
        .frame_rate(240.0)
        .open()
        .unwrap();
    camera.start().unwrap();
    let total = units * 2 + 3;
    let started = Instant::now();
    for seq in 0..total as u64 {
        let frame = camera.next_frame(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(frame.seq(), seq);
    }
    eprintln!("{total} frames over two loops in {:?}", started.elapsed());
}

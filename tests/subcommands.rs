// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The `devices`, `info`, `record` and `convert` subcommands, run as the
//! binary. They need no camera: the mock source is always listed, and
//! `record` is checked against what `info` reports about the encoder.

use std::{
    error::Error,
    path::{Path, PathBuf},
    process::{Command, Output},
};

type BoxError = Box<dyn Error + Send + Sync>;

fn camera_bin() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_edgefirst_camera")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_edgefirst-camera")))
}

fn run(args: &[&str]) -> Result<Output, BoxError> {
    Ok(Command::new(camera_bin()).args(args).output()?)
}

fn json(args: &[&str]) -> Result<serde_json::Value, BoxError> {
    let out = run(args)?;
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(serde_json::from_slice(&out.stdout)?)
}

fn scratch_dir(name: &str) -> Result<PathBuf, BoxError> {
    let dir = std::env::temp_dir().join(format!("edgefirst-camera-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn path(p: &Path) -> Result<&str, BoxError> {
    Ok(p.to_str().ok_or("non-UTF-8 temp path")?)
}

#[test]
fn devices_lists_formats_sizes_rates_and_modes() -> Result<(), BoxError> {
    let devices = json(&["devices", "--json"])?;
    let cameras = devices["cameras"].as_array().ok_or("no cameras array")?;
    let mock = cameras
        .iter()
        .find(|c| c["backend"] == "mock")
        .ok_or("the mock camera is not listed")?;
    let yuyv = mock["formats"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["format"] == "YUYV"))
        .ok_or("mock has no YUYV")?;
    assert!(yuyv["sizes"]["max"].is_array(), "stepwise sizes: {yuyv}");
    assert!(
        yuyv["sizes"]["fps"]["max"].is_number(),
        "rate range: {yuyv}"
    );
    let modes = yuyv["modes"].as_array().ok_or("no modes")?;
    assert!(modes.contains(&"1080p240".into()), "{modes:?}");

    // Every V4L2 camera here (vivid on CI) has formats with named modes.
    for camera in cameras.iter().filter(|c| c["backend"] == "v4l2") {
        let formats = camera["formats"].as_array().ok_or("no formats")?;
        assert!(!formats.is_empty(), "{camera}");
        assert!(
            formats
                .iter()
                .any(|f| f["modes"].as_array().is_some_and(|m| !m.is_empty())),
            "{camera}"
        );
    }
    Ok(())
}

#[test]
fn info_reports_cameras_and_codecs() -> Result<(), BoxError> {
    let info = json(&["info", "--json"])?;
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    assert!(info["cameras"].as_array().is_some_and(|c| !c.is_empty()));
    for codec in ["encoder", "decoder"] {
        let available = info[codec]["available"]
            .as_bool()
            .ok_or("no available flag")?;
        if available {
            assert!(info[codec]["device"].is_string(), "{info}");
        } else {
            assert!(info[codec]["error"].is_string(), "{info}");
        }
    }
    Ok(())
}

#[test]
fn convert_writes_an_mp4() -> Result<(), BoxError> {
    let dir = scratch_dir("convert")?;
    let out = dir.join("clip.mp4");
    let result = (|| -> Result<(), BoxError> {
        let input = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/testsrc-160x120.h264"
        );
        let o = run(&["convert", input, path(&out)?, "--fps", "30"])?;
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

        let size = std::fs::metadata(&out)?.len();
        let reader =
            mp4::Mp4Reader::read_header(std::io::BufReader::new(std::fs::File::open(&out)?), size)?;
        let track = reader.tracks().get(&1).ok_or("no video track")?;
        assert_eq!(track.sample_count(), 30);
        assert_eq!((track.width(), track.height()), (160, 120));
        Ok(())
    })();
    std::fs::remove_dir_all(&dir)?;
    result
}

/// `record` writes the stream and a sidecar a `file:` source reads where
/// there is an encoder, and exits with 4 where there is none.
#[test]
fn record_needs_an_encoder() -> Result<(), BoxError> {
    let encoder = json(&["info", "--json"])?["encoder"]["available"]
        .as_bool()
        .ok_or("no encoder flag")?;
    let dir = scratch_dir("record-sub")?;
    let rec = dir.join("mock.h264");
    let result = (|| -> Result<(), BoxError> {
        let o = run(&[
            "record",
            path(&rec)?,
            "--source",
            "mock:320x240@30",
            "--stream-size",
            "320",
            "240",
            "--frames",
            "30",
        ])?;
        let stderr = String::from_utf8_lossy(&o.stderr);
        if !encoder {
            assert_eq!(o.status.code(), Some(4), "{stderr}");
            assert!(stderr.contains("record needs an H.264 encoder"), "{stderr}");
            return Ok(());
        }
        assert!(o.status.success(), "{stderr}");
        assert!(std::fs::metadata(&rec)?.len() > 0);
        let sidecar: serde_json::Value =
            serde_json::from_slice(&std::fs::read(rec.with_extension("json"))?)?;
        assert_eq!(sidecar["width"], 320);
        Ok(())
    })();
    std::fs::remove_dir_all(&dir)?;
    result
}

#[test]
fn record_refuses_a_recording_as_its_source() -> Result<(), BoxError> {
    let o = run(&["record", "out.h264", "--source", "file:in.h264"])?;
    assert_eq!(
        o.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    Ok(())
}

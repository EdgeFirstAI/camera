// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The Media Foundation backend on a real camera. Every test is ignored;
//! run them on a Windows machine with a camera:
//!
//! ```text
//! set EDGEFIRST_CAMERA_SOURCE=mediafoundation
//! cargo test -p edgefirst-camera --test media_foundation -- --include-ignored --test-threads 1
//! ```
//!
//! - `EDGEFIRST_CAMERA_SOURCE`: `mediafoundation` for the first camera, or
//!   `mediafoundation:<id or index>`. Without it every test prints
//!   `SKIPPED`, and fails instead when `EDGEFIRST_CAMERA_REQUIRE_BOARD=1`.
//! - `EDGEFIRST_CAMERA_SIZE` and `EDGEFIRST_CAMERA_FPS`: the mode requested,
//!   default 1920x1080 at 30. A camera without it delivers its nearest.

#![cfg(all(windows, feature = "media-foundation"))]

use std::time::{Duration, Instant, SystemTime};

use edgefirst_camera::{
    enumerate, Applied, Backend, CameraBuilder, CaptureClock, Control, ControlId, Exposure, Frame,
    ResolvedMemory, TimestampSource,
};
use edgefirst_tensor::{CpuAccess, DType, PixelFormat, TensorDyn, TensorMemory};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const WAIT: Option<Duration> = Some(Duration::from_secs(2));

/// The camera's source spec, or `None` after reporting the skip.
fn source() -> Option<String> {
    let spec = std::env::var("EDGEFIRST_CAMERA_SOURCE").unwrap_or_default();
    let found = if spec == "mediafoundation" {
        enumerate()
            .unwrap_or_else(|e| {
                eprintln!("enumerate() failed: {e}");
                Vec::new()
            })
            .into_iter()
            .find(|c| c.backend == Backend::MediaFoundation)
            .map(|c| format!("mediafoundation:{}", c.id))
    } else {
        spec.starts_with("mediafoundation:").then(|| spec.clone())
    };
    if found.is_none() {
        let why = format!("no Media Foundation camera at {spec:?}; set EDGEFIRST_CAMERA_SOURCE");
        assert!(
            std::env::var("EDGEFIRST_CAMERA_REQUIRE_BOARD").as_deref() != Ok("1"),
            "{why}"
        );
        eprintln!("SKIPPED (no-camera): {why}");
    }
    found
}

fn size() -> (u32, u32) {
    std::env::var("EDGEFIRST_CAMERA_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1920, 1080))
}

fn fps() -> f64 {
    std::env::var("EDGEFIRST_CAMERA_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30.0)
}

fn builder(source: &str) -> CameraBuilder {
    let (w, h) = size();
    CameraBuilder::source(source)
        .unwrap()
        .size(w, h)
        .frame_rate(fps())
}

fn checksum(frame: &Frame) -> u64 {
    let map = frame.map_bytes(CpuAccess::Read).unwrap();
    map.iter().enumerate().fold(0u64, |h, (i, &b)| {
        h.wrapping_mul(31).wrapping_add(u64::from(b) ^ i as u64)
    })
}

#[test]
#[ignore = "requires a camera (set EDGEFIRST_CAMERA_SOURCE)"]
fn captures_the_requested_mode_into_d3d11_tensors() -> TestResult {
    let Some(source) = source() else {
        return Ok(());
    };
    let mut camera = builder(&source).open()?;
    let config = camera.config().clone();
    eprintln!(
        "{}x{} {:?} at {:?} fps",
        config.width, config.height, config.format, config.frame_rate
    );
    assert_eq!(config.memory, ResolvedMemory::Import);
    camera.start()?;
    let started = Instant::now();
    let mut last = None;
    while started.elapsed() < Duration::from_secs(3) {
        let frame = camera.next_frame(WAIT)?;
        assert_eq!(frame.memory(), TensorMemory::DmaBuf);
        assert!(frame.d3d11_texture().is_some(), "not a D3D11 tensor");
        assert_eq!(frame.width(), Some(config.width as usize));
        let ts = frame.timestamp();
        assert_eq!(ts.clock, CaptureClock::Qpc);
        assert_eq!(ts.source, TimestampSource::EndOfFrame);
        assert!(last.is_none_or(|l| ts.nanos > l), "timestamps go back");
        last = Some(ts.nanos);
        let age = SystemTime::now().duration_since(frame.realtime().expect("acquired"))?;
        assert!(age < Duration::from_secs(1), "acquired {age:?} ago");
    }
    let (measured, reported) = (
        camera.measured_frame_rate().expect("measured"),
        config.frame_rate.expect("reported"),
    );
    eprintln!("measured {measured} fps, reported {reported}");
    assert!(
        (measured - reported).abs() <= reported * 0.1,
        "measured {measured} fps, reported {reported}"
    );
    camera.stop()?;
    Ok(())
}

#[test]
#[ignore = "requires a camera (set EDGEFIRST_CAMERA_SOURCE)"]
fn every_format_the_camera_offers_is_delivered() -> TestResult {
    let Some(source) = source() else {
        return Ok(());
    };
    let offered: Vec<PixelFormat> = builder(&source)
        .open()?
        .descriptor()
        .formats
        .iter()
        .map(|f| f.format)
        .collect();
    eprintln!("formats: {offered:?}");
    assert!(!offered.is_empty());
    for format in offered {
        let planes = if format == PixelFormat::Nv12 { 2 } else { 1 };
        let mut camera = builder(&source).format(format).open()?;
        assert_eq!(camera.config().format, format);
        camera.start()?;
        let frame = camera.next_frame(WAIT)?;
        assert_eq!(frame.format(), Some(format));
        assert_eq!(frame.planes().len(), planes, "{format:?}");
        assert_eq!(camera.config().planes, planes, "{format:?}");
    }
    Ok(())
}

#[test]
#[ignore = "requires a camera (set EDGEFIRST_CAMERA_SOURCE)"]
fn frames_held_across_close_stay_readable_and_the_camera_reopens() -> TestResult {
    let Some(source) = source() else {
        return Ok(());
    };
    let mut camera = builder(&source).open()?;
    camera.start()?;
    let held: Vec<Frame> = (0..2)
        .map(|_| camera.next_frame(WAIT))
        .collect::<Result<_, _>>()?;
    let before: Vec<u64> = held.iter().map(checksum).collect();
    drop(camera);
    std::thread::sleep(Duration::from_millis(300));
    let after: Vec<u64> = held.iter().map(checksum).collect();
    assert_eq!(before, after, "a held frame changed after close");

    let reopened = Instant::now();
    let mut camera = builder(&source).open()?;
    camera.start()?;
    camera.next_frame(WAIT)?;
    eprintln!("reopened in {:?}", reopened.elapsed());
    assert!(reopened.elapsed() < Duration::from_secs(3));
    Ok(())
}

#[test]
#[ignore = "requires a camera (set EDGEFIRST_CAMERA_SOURCE)"]
fn stop_start_cycles_keep_delivering() -> TestResult {
    let Some(source) = source() else {
        return Ok(());
    };
    let mut camera = builder(&source).buffers(3).open()?;
    let mut last_seq = None;
    for _ in 0..4 {
        camera.start()?;
        let mut kept = None;
        for _ in 0..5 {
            let frame = camera.next_frame(WAIT)?;
            assert!(last_seq.is_none_or(|s| frame.seq() > s));
            last_seq = Some(frame.seq());
            kept = Some(frame);
        }
        camera.stop()?;
        // A frame held across a stop keeps its slot until it drops.
        let kept = kept.expect("a frame");
        let sum = checksum(&kept);
        camera.start()?;
        camera.next_frame(WAIT)?;
        assert_eq!(sum, checksum(&kept), "a held frame was overwritten");
        camera.stop()?;
    }
    Ok(())
}

#[test]
#[ignore = "requires a camera (set EDGEFIRST_CAMERA_SOURCE)"]
fn a_callers_pool_is_filled() -> TestResult {
    let Some(source) = source() else {
        return Ok(());
    };
    let probe = builder(&source).open()?;
    let (w, h, format) = (
        probe.config().width as usize,
        probe.config().height as usize,
        probe.config().format,
    );
    drop(probe);
    let pool: Vec<TensorDyn> = (0..4)
        .map(|_| {
            TensorDyn::image(
                w,
                h,
                format,
                DType::U8,
                Some(TensorMemory::DmaBuf),
                CpuAccess::ReadWrite,
            )
        })
        .collect::<Result<_, _>>()?;
    let textures: Vec<_> = pool.iter().map(|t| t.d3d11_texture()).collect();
    let mut camera = builder(&source).with_buffers(pool).open()?;
    camera.start()?;
    for _ in 0..8 {
        let frame = camera.next_frame(WAIT)?;
        assert!(
            textures.contains(&frame.d3d11_texture()),
            "not a caller texture"
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires a camera (set EDGEFIRST_CAMERA_SOURCE)"]
fn controls_report_only_what_the_camera_supports() -> TestResult {
    let Some(source) = source() else {
        return Ok(());
    };
    let mut camera = builder(&source).open()?;
    let offered: Vec<ControlId> = camera.controls().iter().map(|(id, _)| id).collect();
    eprintln!("controls: {offered:?}");
    assert!(offered.contains(&ControlId::FrameRate));
    for id in &offered {
        camera.get_control(*id)?;
    }
    let applied = camera.set_control(Control::Exposure(Exposure::Auto))?;
    if !offered.contains(&ControlId::Exposure) {
        assert!(matches!(applied, Applied::Unsupported(_)));
    }
    let fastest = camera.controls().get(ControlId::FrameRate).map(|i| i.max);
    camera.set_control(Control::FrameRate(1000.0))?;
    assert_eq!(
        camera.get_control(ControlId::FrameRate).ok(),
        fastest,
        "the rate clamps to the fastest offered"
    );
    camera.start()?;
    assert!(camera.set_control(Control::FrameRate(5.0)).is_err());
    Ok(())
}

#[test]
#[ignore = "requires a camera (set EDGEFIRST_CAMERA_SOURCE)"]
fn a_timeout_is_honoured_while_every_buffer_is_held() -> TestResult {
    let Some(source) = source() else {
        return Ok(());
    };
    let mut camera = builder(&source).buffers(2).open()?;
    camera.start()?;
    let _held: Vec<Frame> = (0..2)
        .map(|_| camera.next_frame(WAIT))
        .collect::<Result<_, _>>()?;
    let asked = Instant::now();
    let e = camera
        .next_frame(Some(Duration::from_millis(300)))
        .unwrap_err();
    assert_eq!(e.kind(), edgefirst_camera::ErrorKind::Timeout);
    assert!(asked.elapsed() < Duration::from_secs(1));
    assert_eq!(camera.stats().held, 2);
    Ok(())
}

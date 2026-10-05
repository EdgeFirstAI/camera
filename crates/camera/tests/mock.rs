// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Public-API tests against the mock backend.

use std::time::{Duration, UNIX_EPOCH};

use edgefirst_camera::{
    enumerate, modes, probe, Applied, Backend, BufferPool, CameraBuilder, Control, ControlId,
    ControlValue, ErrorKind, Exposure, MemoryStrategy, Rejection, Unsupported,
};
use edgefirst_tensor::{CpuAccess, DType, PixelFormat, TensorDyn, TensorMemory};

const WAIT: Option<Duration> = Some(Duration::from_secs(2));

fn host_image(w: usize, h: usize, format: PixelFormat) -> TensorDyn {
    TensorDyn::image(
        w,
        h,
        format,
        DType::U8,
        Some(TensorMemory::Mem),
        CpuAccess::ReadWrite,
    )
    .unwrap()
}

#[test]
fn test_mock_captures_frames_with_metadata() {
    let mut camera = CameraBuilder::source("mock:320x240@120")
        .unwrap()
        .format(PixelFormat::Nv12)
        .open()
        .unwrap();
    camera.start().unwrap();
    let cfg = camera.config().clone();
    assert_eq!(
        (cfg.width, cfg.height, cfg.format),
        (320, 240, PixelFormat::Nv12)
    );
    assert_eq!(cfg.planes, 2);
    assert_eq!(cfg.frame_rate, Some(120.0));

    let mut last_ts = None;
    for expected in 0..5u64 {
        let frame = camera.next_frame(WAIT).unwrap();
        assert_eq!(frame.seq(), expected);
        assert_eq!(frame.width(), Some(320));
        assert_eq!(frame.planes().len(), 2);
        assert!(frame.planes()[1].offset >= 320 * 240);
        let t = frame.timestamp().nanos;
        if let Some(prev) = last_ts {
            assert!(t > prev, "timestamps must increase");
        }
        last_ts = Some(t);
        let wall = frame.realtime().expect("acquisition time");
        assert!(wall > UNIX_EPOCH);
        // The pattern changes from frame to frame.
        let bytes = frame.map_bytes(CpuAccess::Read).unwrap();
        assert_eq!(bytes[0], expected as u8);
    }
    assert_eq!(camera.stats().frames, 5);
}

#[test]
fn test_mock_paces_at_requested_rate() {
    let mut camera = CameraBuilder::source("mock:64x64@50")
        .unwrap()
        .open()
        .unwrap();
    camera.start().unwrap();
    let start = std::time::Instant::now();
    for _ in 0..6 {
        drop(camera.next_frame(WAIT).unwrap());
    }
    // Six frames at 50 fps span at least five intervals of 20 ms.
    assert!(
        start.elapsed() >= Duration::from_millis(95),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn test_mock_held_frames_starve_and_count_drops() {
    let mut camera = CameraBuilder::source("mock:64x64@200")
        .unwrap()
        .buffers(2)
        .open()
        .unwrap();
    camera.start().unwrap();
    let a = camera.next_frame(WAIT).unwrap();
    let b = camera.next_frame(WAIT).unwrap();
    assert_ne!(a.slot(), b.slot());
    assert_eq!(camera.stats().held, 2);

    let err = camera
        .next_frame(Some(Duration::from_millis(30)))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Timeout);
    assert!(camera.stats().dropped > 0);
    assert_eq!(camera.stats().timeouts, 1);

    drop(a);
    let c = camera.next_frame(WAIT).unwrap();
    assert_eq!(c.seq(), 2, "seq counts delivered frames, not drops");
}

#[test]
fn test_mock_frames_survive_stop_and_drop_of_camera() {
    let mut camera = CameraBuilder::source("mock:64x64@100")
        .unwrap()
        .open()
        .unwrap();
    camera.start().unwrap();
    let frame = camera.next_frame(WAIT).unwrap();
    camera.stop().unwrap();
    assert_eq!(
        camera.next_frame(WAIT).unwrap_err().kind(),
        ErrorKind::InvalidConfig
    );
    drop(camera);
    // The frame still owns its memory.
    let bytes = frame.map_bytes(CpuAccess::Read).unwrap();
    assert_eq!(bytes[0], 0);
}

#[test]
fn test_mock_controls_report_outcomes() {
    let mut camera = CameraBuilder::source("mock").unwrap().open().unwrap();
    assert!(camera.controls().contains(ControlId::FrameRate));
    assert_eq!(
        camera.set_control(Control::FrameRate(60.0)).unwrap(),
        Applied::Driver
    );
    assert_eq!(camera.config().frame_rate, Some(60.0));
    assert_eq!(
        camera.set_control(Control::FrameRate(1000.0)).unwrap(),
        Applied::Clamped(ControlValue::Float(240.0))
    );
    assert_eq!(
        camera
            .set_control(Control::Exposure(Exposure::Auto))
            .unwrap(),
        Applied::Unsupported(Unsupported::Backend)
    );
    assert_eq!(
        camera.get_control(ControlId::FrameRate).unwrap(),
        ControlValue::Float(240.0)
    );
    assert_eq!(
        camera.get_control(ControlId::Gain).unwrap_err().kind(),
        ErrorKind::InvalidConfig
    );
}

#[test]
fn test_mock_rejected_caller_pool_recovers_with_sdk_pool() {
    let pool = vec![
        host_image(64, 64, PixelFormat::Yuyv),
        host_image(32, 32, PixelFormat::Yuyv),
    ];
    let mut camera = CameraBuilder::source("mock:64x64@100")
        .unwrap()
        .memory(MemoryStrategy::Import)
        .with_buffers(pool)
        .open()
        .unwrap();
    let err = camera.start().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BuffersRejected);
    assert_eq!(err.rejection(), Some((Rejection::Size, Some(1))));

    // The camera stays open and stopped; the caller takes its buffers back
    // and falls back to SDK-allocated buffers.
    assert_eq!(camera.take_buffers().len(), 2);
    camera.set_buffers(BufferPool::Sdk).unwrap();
    camera.start().unwrap();
    assert!(camera.next_frame(WAIT).is_ok());
    assert_eq!(
        camera.set_buffers(BufferPool::Sdk).unwrap_err().kind(),
        ErrorKind::InvalidConfig,
        "pool changes are refused while streaming"
    );
}

#[test]
fn test_mock_captures_into_caller_pool() {
    let pool: Vec<_> = (0..3)
        .map(|_| host_image(64, 48, PixelFormat::Grey))
        .collect();
    let mut camera = CameraBuilder::source("mock:64x48@100")
        .unwrap()
        .format(PixelFormat::Grey)
        .with_buffers(pool)
        .open()
        .unwrap();
    camera.start().unwrap();
    assert_eq!(camera.config().buffer_count, 3);
    let frame = camera.next_frame(WAIT).unwrap();
    assert_eq!(frame.height(), Some(48));
}

#[test]
fn test_mock_rejects_too_few_caller_buffers() {
    let mut camera = CameraBuilder::source("mock:64x64@100")
        .unwrap()
        .with_buffers(vec![host_image(64, 64, PixelFormat::Yuyv)])
        .open()
        .unwrap();
    assert_eq!(
        camera.start().unwrap_err().rejection(),
        Some((Rejection::Count, None))
    );
}

#[test]
fn test_mock_unsupported_format_and_sources() {
    let err = CameraBuilder::source("mock")
        .unwrap()
        .format(PixelFormat::Nv16)
        .open()
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnsupportedFormat);
    let err = CameraBuilder::source("/dev/video99")
        .unwrap()
        .open()
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

#[test]
fn test_mock_enumerates_and_probes_modes() {
    let cameras = enumerate().unwrap();
    let mock = cameras
        .iter()
        .find(|d| d.backend == Backend::Mock)
        .expect("mock camera");
    let formats = probe(mock).unwrap();
    let names: Vec<_> = modes(&formats, PixelFormat::Yuyv)
        .iter()
        .map(|m| m.name())
        .collect();
    assert!(names.contains(&"1080p240".to_owned()), "{names:?}");
    let mut camera = CameraBuilder::from_descriptor(mock)
        .size(640, 480)
        .open()
        .unwrap();
    camera.start().unwrap();
    assert_eq!(camera.next_frame(WAIT).unwrap().width(), Some(640));
}

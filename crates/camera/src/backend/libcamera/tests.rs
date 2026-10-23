// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The libcamera backend on the shim's mock.

use std::time::Duration;

use edgefirst_libcamera_sys::mock::{self, Event, Layout};
use edgefirst_libcamera_sys::Shim;

use super::*;

const T: Option<Duration> = Some(Duration::from_secs(1));
const ID: &str = "/base/soc/i2c@42530000/os08a20@36";

fn manager(cameras: Vec<mock::Camera>) -> Arc<Manager> {
    mock::configure(cameras);
    let shim = Shim::from_functions(mock::functions(), "mock").unwrap();
    Manager::start(Arc::new(shim)).unwrap()
}

fn open_with(
    cameras: Vec<mock::Camera>,
    build: impl FnOnce(CameraBuilder) -> CameraBuilder,
) -> Result<Box<dyn Camera>> {
    let m = manager(cameras);
    open_on(m, build(CameraBuilder::source(&format!("libcamera:{ID}"))?))
}

fn open(layout: Layout) -> Box<dyn Camera> {
    open_with(vec![mock::Camera::new(ID, layout)], |b| {
        b.format(PixelFormat::Nv12).size(1920, 1080)
    })
    .unwrap()
}

/// The first byte of a frame's plane, read through its handle.
fn first_byte(frame: &Frame, plane: usize) -> u8 {
    let p = frame.planes()[plane];
    let mut b = 0u8;
    // SAFETY: the frame's tensor keeps the handle open; one byte read.
    let n = unsafe {
        libc::pread(
            p.handle as i32,
            (&mut b as *mut u8).cast(),
            1,
            p.offset as i64,
        )
    };
    assert_eq!(n, 1);
    b
}

#[test]
fn separate_planes_become_a_two_buffer_nv12_frame() {
    let mut c = open(Layout::Separate);
    let cfg = c.config().clone();
    assert_eq!(
        (cfg.width, cfg.height, cfg.format),
        (1920, 1080, PixelFormat::Nv12)
    );
    assert_eq!((cfg.row_stride, cfg.planes), (1920, 2));
    assert_eq!(cfg.memory, ResolvedMemory::Export);
    c.start().unwrap();
    let f = c.next_frame(T).unwrap();
    let p = f.planes();
    assert_eq!(p.len(), 2);
    assert_ne!(p[0].handle, p[1].handle);
    assert_eq!(
        (p[0].offset, p[0].size, p[0].used),
        (0, 1920 * 1080, 1920 * 1080)
    );
    assert_eq!((p[1].offset, p[1].size), (0, 1920 * 540));
    assert_eq!(f.width(), Some(1920));
    assert_eq!(f.tensor().format(), Some(PixelFormat::Nv12));
}

#[test]
fn shared_planes_become_one_contiguous_nv12_tensor() {
    let mut c = open(Layout::Shared);
    c.start().unwrap();
    let f = c.next_frame(T).unwrap();
    let p = f.planes();
    assert_eq!(p[0].handle, p[1].handle);
    assert_eq!((p[0].offset, p[1].offset), (0, 1920 * 1080));
    assert_eq!(f.tensor().shape(), &[1620, 1920]);
}

#[test]
fn yuyv_is_one_plane_at_its_stride() {
    let mut c = open_with(vec![mock::Camera::new(ID, Layout::Separate)], |b| {
        b.format(PixelFormat::Yuyv).size(1280, 720)
    })
    .unwrap();
    assert_eq!(c.config().row_stride, 2560);
    c.start().unwrap();
    let f = c.next_frame(T).unwrap();
    assert_eq!(f.planes().len(), 1);
    assert_eq!(f.tensor().effective_row_stride(), Some(2560));
}

#[test]
fn frames_carry_sensor_timestamps_and_sequences() {
    let mut c = open(Layout::Separate);
    c.start().unwrap();
    let frames: Vec<Frame> = (0..3).map(|_| c.next_frame(T).unwrap()).collect();
    for (i, f) in frames.iter().enumerate() {
        assert_eq!(f.seq(), i as u64);
        assert_eq!(f.driver_sequence(), Some(i as u32));
        let ts = f.timestamp();
        assert_eq!(ts.clock, CaptureClock::Monotonic);
        assert_eq!(ts.source, TimestampSource::EndOfFrame);
        assert_eq!(ts.nanos, 1_000_000_000 + i as i64 * 33_333_000);
        assert!(f.realtime().is_some());
    }
    assert_eq!(c.stats().dropped, 0);
}

#[test]
fn dropped_frames_requeue_and_capture_continues() {
    let mut c = open(Layout::Separate);
    c.start().unwrap();
    for i in 0..20u64 {
        let f = c.next_frame(T).unwrap();
        assert_eq!(f.seq(), i);
    }
    let s = c.stats();
    assert_eq!((s.frames, s.held), (20, 0));
}

#[test]
fn every_buffer_held_times_out_then_resumes_on_release() {
    let mut c = open(Layout::Separate);
    c.start().unwrap();
    let held: Vec<Frame> = (0..4).map(|_| c.next_frame(T).unwrap()).collect();
    let e = c.next_frame(Some(Duration::from_millis(30))).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Timeout);
    drop(held);
    assert_eq!(c.next_frame(T).unwrap().seq(), 4);
}

#[test]
fn the_requested_rate_goes_out_at_start_and_is_reported() {
    let mut c = open_with(vec![mock::Camera::new(ID, Layout::Separate)], |b| {
        b.frame_rate(50.0)
    })
    .unwrap();
    c.start().unwrap();
    assert_eq!(c.config().frame_rate, Some(50.0));
    let starts: Vec<_> = mock::events()
        .into_iter()
        .filter_map(|e| match e {
            Event::Start(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].frame_duration_min_us, 20_000);
    assert_eq!(starts[0].frame_duration_max_us, 20_000);
}

#[test]
fn a_pipeline_ignoring_the_rate_reports_what_it_delivers() {
    let mut cam = mock::Camera::new(ID, Layout::Separate);
    cam.honours_frame_duration = false;
    cam.frame_duration_us = 16_666;
    cam.reported_duration_us = Some(15_972);
    let mut c = open_with(vec![cam], |b| b.frame_rate(30.0)).unwrap();
    assert_eq!(c.config().frame_rate, Some(30.0));
    c.start().unwrap();
    assert_eq!(c.config().frame_rate, Some(60.0), "from the first interval");
    for _ in 0..=RATE_FRAMES {
        c.next_frame(T).unwrap();
    }
    assert_eq!(c.config().frame_rate, Some(60.0), "from the timestamps");
    assert_eq!(c.stats().dropped, 0);
}

#[test]
fn probe_configures_each_size_once_in_a_safe_format() {
    let m = manager(vec![mock::Camera::new(ID, Layout::Separate)]);
    let info = m.cameras().remove(0);
    let d = describe::descriptor(&m, &info);
    let probed = describe::probe(&m, &d).unwrap();
    assert_eq!(probed.len(), 2, "NV12 and YUYV both carry rates");
    let configured: Vec<_> = mock::events()
        .into_iter()
        .filter_map(|e| match e {
            Event::Configure(r) => Some((r.fourcc, r.width, r.height)),
            _ => None,
        })
        .collect();
    assert_eq!(
        configured,
        [(mock::NV12, 1920, 1080), (mock::NV12, 1280, 720)]
    );
}

#[test]
fn controls_set_while_streaming_ride_the_next_request() {
    let mut c = open(Layout::Separate);
    c.start().unwrap();
    let f = c.next_frame(T).unwrap();
    let got = c
        .set_control(Control::Exposure(Exposure::Manual(Duration::from_millis(
            10,
        ))))
        .unwrap();
    assert_eq!(got, Applied::Driver);
    drop(f);
    c.next_frame(T).unwrap();
    let requeued: Vec<_> = mock::events()
        .into_iter()
        .filter_map(|e| match e {
            Event::Requeue(slot, ctl) => Some((slot, ctl)),
            _ => None,
        })
        .collect();
    assert_eq!(requeued.len(), 1, "sent once");
    let (slot, ctl) = requeued[0];
    assert_eq!(slot, 0);
    assert_eq!(ctl.exposure_mode, sys::EFCAM_LC_MODE_MANUAL);
    assert_eq!(ctl.exposure_time_us, 10_000);
}

#[test]
fn out_of_range_controls_are_clamped_and_said_so() {
    let mut c = open(Layout::Separate);
    assert_eq!(
        c.set_control(Control::Gain(Gain::Manual(64.0))).unwrap(),
        Applied::Clamped(ControlValue::Float(16.0))
    );
    assert_eq!(
        c.set_control(Control::FrameRate(120.0)).unwrap(),
        Applied::Clamped(ControlValue::Float(1e6 / 16_666.0))
    );
}

#[test]
fn controls_the_camera_lacks_are_reported_unsupported() {
    let mut cam = mock::Camera::new(ID, Layout::Separate);
    cam.controls
        .retain(|c| c.0 != sys::EFCAM_LC_CONTROL_COLOUR_TEMPERATURE);
    let mut c = open_with(vec![cam], |b| b).unwrap();
    assert!(!c.controls().contains(ControlId::WhiteBalance));
    assert_eq!(
        c.set_control(Control::WhiteBalance(WhiteBalance::Manual { kelvin: 4000 }))
            .unwrap(),
        Applied::Unsupported(Unsupported::Camera)
    );
    assert_eq!(
        c.set_control(Control::WhiteBalance(WhiteBalance::Auto))
            .unwrap(),
        Applied::Driver
    );
    assert_eq!(
        c.set_control(Control::Custom {
            backend: crate::Backend::Libcamera,
            id: 1,
            value: ControlValue::Int(1)
        })
        .unwrap(),
        Applied::Unsupported(Unsupported::Backend)
    );
}

#[test]
fn controls_read_back_from_the_last_frame() {
    let mut c = open(Layout::Separate);
    c.start().unwrap();
    let _f = c.next_frame(T).unwrap();
    assert_eq!(
        c.get_control(ControlId::Exposure).unwrap(),
        ControlValue::Int(20_000)
    );
    assert_eq!(
        c.get_control(ControlId::Gain).unwrap(),
        ControlValue::Float(2.0)
    );
    assert_eq!(
        c.get_control(ControlId::Mirror).unwrap(),
        ControlValue::Int(0)
    );
}

#[test]
fn mirroring_reconfigures_and_a_refusal_keeps_the_stream() {
    let mut c = open(Layout::Separate);
    assert_eq!(
        c.set_control(Control::Mirror(Mirror::Both)).unwrap(),
        Applied::Driver
    );
    assert_eq!(
        c.get_control(ControlId::Mirror).unwrap(),
        ControlValue::Int(3)
    );

    let mut cam = mock::Camera::new(ID, Layout::Separate);
    cam.mirrors = false;
    let mut c = open_with(vec![cam], |b| b.size(1280, 720)).unwrap();
    assert_eq!(
        c.set_control(Control::Mirror(Mirror::Horizontal)).unwrap(),
        Applied::Unsupported(Unsupported::Camera)
    );
    assert_eq!((c.config().width, c.config().height), (1280, 720));
    c.start().unwrap();
    assert_eq!(
        c.set_control(Control::Mirror(Mirror::None))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidConfig
    );
}

#[test]
fn frames_held_across_stop_keep_their_memory_and_are_never_written() {
    let mut c = open(Layout::Shared);
    c.start().unwrap();
    let _ = c.next_frame(T).unwrap();
    let held = c.next_frame(T).unwrap();
    assert_eq!(first_byte(&held, 0), 1);
    c.stop().unwrap();
    assert_eq!(c.stats().held, 0, "detached");
    c.start().unwrap();
    for _ in 0..8 {
        c.next_frame(T).unwrap();
    }
    assert_eq!(first_byte(&held, 0), 1);
    assert_eq!(first_byte(&held, 1), 1);
    drop(c);
    assert_eq!(first_byte(&held, 0), 1, "outlives the camera");
    let events = mock::events();
    assert_eq!(events.iter().filter(|e| **e == Event::Free).count(), 3);
    assert_eq!(events.last(), Some(&Event::Close));
}

#[test]
fn caller_pools_are_rejected_and_handed_back() {
    let mut c = open(Layout::Separate);
    let pool = vec![TensorDyn::image(
        64,
        64,
        PixelFormat::Nv12,
        DType::U8,
        Some(edgefirst_tensor::TensorMemory::Mem),
        edgefirst_tensor::CpuAccess::ReadWrite,
    )
    .unwrap()];
    c.set_buffers(BufferPool::Provided(pool)).unwrap();
    let e = c.start().unwrap_err();
    assert_eq!(e.kind(), ErrorKind::BuffersRejected);
    assert_eq!(c.take_buffers().len(), 1);
    c.set_buffers(BufferPool::Sdk).unwrap();
    c.start().unwrap();
}

#[test]
fn import_absent_and_busy_cameras_fail_at_open() {
    let e = open_with(vec![mock::Camera::new(ID, Layout::Separate)], |b| {
        b.memory(MemoryStrategy::Import)
    })
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidConfig);

    let e = open_with(vec![mock::Camera::new("/other", Layout::Separate)], |b| b).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound);

    let mut cam = mock::Camera::new(ID, Layout::Separate);
    cam.busy = true;
    assert_eq!(
        open_with(vec![cam], |b| b).unwrap_err().kind(),
        ErrorKind::Busy
    );

    let m = manager(vec![mock::Camera::new(ID, Layout::Separate)]);
    let spec = format!("libcamera:{ID}");
    let first = open_on(m.clone(), CameraBuilder::source(&spec).unwrap()).unwrap();
    let e = open_on(m, CameraBuilder::source(&spec).unwrap()).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Busy);
    drop(first);
}

#[test]
fn a_format_the_camera_lacks_is_unsupported() {
    let mut cam = mock::Camera::new(ID, Layout::Separate);
    cam.formats.retain(|f| f.0 == mock::NV12);
    let e = open_with(vec![cam], |b| b.format(PixelFormat::Yuyv)).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::UnsupportedFormat);
}

#[test]
fn probe_reads_each_modes_frame_duration_limits() {
    let m = manager(vec![mock::Camera::new(ID, Layout::Separate)]);
    let info = m.cameras().remove(0);
    let d = describe::descriptor(&m, &info);
    assert_eq!(d.backend, crate::Backend::Libcamera);
    assert_eq!(d.formats.len(), 2);
    let probed = describe::probe(&m, &d).unwrap();
    let modes: Vec<String> = crate::modes(&probed, PixelFormat::Nv12)
        .iter()
        .map(|m| m.name())
        .collect();
    assert_eq!(modes, ["720p60", "1080p60"]);
}

#[test]
fn a_raw_only_camera_lists_no_formats() {
    let mut cam = mock::Camera::new(ID, Layout::Separate);
    cam.formats = vec![(mock::RAW10, 3840, 2160)];
    let m = manager(vec![cam]);
    let info = m.cameras().remove(0);
    assert!(describe::descriptor(&m, &info).formats.is_empty());
}

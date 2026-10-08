// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The V4L2 backend against the kernel's virtual capture driver, vivid.
//!
//! Load three instances, a single-planar, a multi-planar and a single-planar
//! one that the unplug test disconnects:
//!
//! ```text
//! sudo modprobe vivid n_devs=3 node_types=0x1,0x1,0x1 multiplanar=1,2,1
//! ```
//!
//! and give the user read-write access to `/dev/video*` and
//! `/dev/dma_heap/system`. Without vivid every test prints `SKIPPED` and
//! passes; `EDGEFIRST_CAMERA_REQUIRE_VIVID=1` makes that a failure.
//! The fault-injection tests need a debug build.
#![cfg(all(target_os = "linux", feature = "v4l2"))]

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use edgefirst_camera::{
    Applied, BufferPool, Camera, CameraBuilder, CaptureClock, Contiguity, Control, ControlId,
    ControlValue, ErrorKind, Frame, MemoryStrategy, Mirror, Rejection, ResolvedMemory, Sizes,
    TimestampSource,
};
use edgefirst_tensor::{CpuAccess, DType, PixelFormat, TensorDyn, TensorMemory};

const TIMEOUT: Option<Duration> = Some(Duration::from_secs(3));
const SINGLE: u32 = 0;
const MULTI: u32 = 1;
/// Disconnected by `unplug_is_disconnected`; used by nothing else.
const UNPLUG: u32 = 2;

/// A vivid instance, held exclusively by this test until it drops.
struct Vivid {
    path: String,
    _lock: File,
}

/// The node of vivid instance `n`, locked against other tests in this run.
fn vivid(n: u32) -> Option<Vivid> {
    let name = format!("vivid-{n:03}-vid-cap");
    let node = std::fs::read_dir("/sys/class/video4linux")
        .ok()
        .and_then(|dir| {
            dir.flatten().find_map(|e| {
                let found = std::fs::read_to_string(e.path().join("name")).ok()?;
                (found.trim() == name).then(|| format!("/dev/{}", e.file_name().to_string_lossy()))
            })
        });
    let Some(path) = node else {
        let why = format!("vivid instance {n} ({name}) is not loaded");
        assert!(
            std::env::var("EDGEFIRST_CAMERA_REQUIRE_VIVID").as_deref() != Ok("1"),
            "EDGEFIRST_CAMERA_REQUIRE_VIVID=1 but {why}"
        );
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "SKIPPED: {why}");
        return None;
    };
    let lock = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("vivid-{n}.lock"));
    let file = File::create(lock).expect("lock file");
    // SAFETY: the descriptor is open for the call.
    assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
    Some(Vivid { path, _lock: file })
}

fn builder(v: &Vivid) -> CameraBuilder {
    CameraBuilder::source(&v.path)
        .unwrap()
        .size(640, 480)
        .format(PixelFormat::Yuyv)
        // The rate persists on the device between opens; set it so no test
        // depends on what an earlier one left.
        .frame_rate(30.0)
        .buffers(4)
        // The hosted runners have no CMA heap; the contiguity test sets
        // Required itself.
        .contiguity(Contiguity::Any)
}

fn next(camera: &mut Box<dyn Camera>) -> Frame {
    camera.next_frame(TIMEOUT).expect("a frame")
}

fn cma() -> bool {
    std::path::Path::new("/dev/dma_heap/linux,cma").exists()
}

fn assert_frame_matches_config(camera: &dyn Camera, frame: &Frame) {
    let c = camera.config();
    assert_eq!(frame.width(), Some(c.width as usize));
    assert_eq!(frame.height(), Some(c.height as usize));
    assert_eq!(frame.format(), Some(c.format));
    assert_eq!(frame.effective_row_stride(), Some(c.row_stride));
    assert!(frame.bytes_used() >= c.row_stride * c.height as usize);
    let bytes = frame.map_bytes(CpuAccess::Read).unwrap();
    assert!(bytes.iter().any(|&b| b != 0), "vivid wrote an empty frame");
}

#[test]
fn enumerate_and_probe_list_vivid() {
    let Some(v) = vivid(SINGLE) else { return };
    let cameras = edgefirst_camera::enumerate().unwrap();
    let d = cameras
        .iter()
        .find(|d| d.id == v.path)
        .expect("vivid in enumerate()");
    assert_eq!(d.driver, "vivid");
    assert!(d.formats.iter().any(|f| f.format == PixelFormat::Yuyv));
    let formats = edgefirst_camera::probe(d).unwrap();
    let yuyv = formats
        .iter()
        .find(|f| f.format == PixelFormat::Yuyv)
        .unwrap();
    let Sizes::Discrete(sizes) = &yuyv.sizes else {
        panic!("vivid's webcam input enumerates discrete sizes");
    };
    let vga = sizes
        .iter()
        .find(|s| (s.width, s.height) == (640, 480))
        .unwrap();
    assert!(vga.rates.max().is_some_and(|r| r >= 25.0));
}

#[test]
fn negotiation_reports_what_the_driver_applied() {
    let Some(v) = vivid(SINGLE) else { return };
    let mut camera = CameraBuilder::source(&v.path)
        .unwrap()
        .size(700, 500)
        .format(PixelFormat::Nv12)
        .frame_rate(15.0)
        .contiguity(Contiguity::Any)
        .open()
        .unwrap();
    let c = camera.config().clone();
    assert_eq!(c.format, PixelFormat::Nv12);
    assert_ne!(
        (c.width, c.height),
        (700, 500),
        "vivid adjusts to a webcam size"
    );
    assert_eq!(c.planes, 2);
    assert!(c.row_stride >= c.width as usize);
    let fps = c.frame_rate.expect("vivid reports a frame rate");
    assert!(fps > 0.0);
    camera.start().unwrap();
    let f = next(&mut camera);
    assert_frame_matches_config(camera.as_ref(), &f);
    assert_eq!(f.planes().len(), 2, "NV12 has a luma and a chroma plane");
    assert!(f.planes()[1].offset >= c.row_stride * c.height as usize);
}

#[test]
fn an_unsupported_format_is_reported() {
    let Some(v) = vivid(SINGLE) else { return };
    let err = builder(&v)
        .format(PixelFormat::PlanarRgb)
        .open()
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnsupportedFormat);
}

fn capture(n: u32, memory: MemoryStrategy, expected: ResolvedMemory) {
    let Some(v) = vivid(n) else { return };
    let mut camera = builder(&v).memory(memory).open().unwrap();
    camera.start().unwrap();
    assert_eq!(camera.config().memory, expected);
    let mut held = Vec::new();
    for _ in 0..8 {
        let f = next(&mut camera);
        assert_frame_matches_config(camera.as_ref(), &f);
        held.push(f);
        if held.len() > 2 {
            held.remove(0);
        }
    }
    assert!(held[1].seq() > held[0].seq());
    camera.stop().unwrap();
    camera.start().unwrap();
    let f = next(&mut camera);
    assert!(f.seq() > held[1].seq(), "seq continues across a restart");
}

#[test]
fn import_captures_single_planar() {
    capture(SINGLE, MemoryStrategy::Import, ResolvedMemory::Import);
}

#[test]
fn import_captures_multi_planar() {
    capture(MULTI, MemoryStrategy::Import, ResolvedMemory::Import);
}

#[test]
fn export_captures_single_planar() {
    capture(SINGLE, MemoryStrategy::Export, ResolvedMemory::Export);
}

#[test]
fn export_captures_multi_planar() {
    capture(MULTI, MemoryStrategy::Export, ResolvedMemory::Export);
}

#[test]
fn auto_imports_when_the_driver_accepts() {
    capture(SINGLE, MemoryStrategy::Auto, ResolvedMemory::Import);
}

#[test]
#[cfg(debug_assertions)]
fn auto_falls_back_to_export_when_import_is_refused() {
    let Some(v) = vivid(SINGLE) else { return };
    let mut camera = builder(&v).fault("import-qbuf").open().unwrap();
    camera.start().unwrap();
    assert_eq!(camera.config().memory, ResolvedMemory::Export);
    assert_eq!(camera.config().contiguous, None);
    let f = next(&mut camera);
    assert_frame_matches_config(camera.as_ref(), &f);
}

#[test]
#[cfg(debug_assertions)]
fn explicit_import_does_not_fall_back() {
    let Some(v) = vivid(SINGLE) else { return };
    let mut camera = builder(&v)
        .memory(MemoryStrategy::Import)
        .fault("import-qbuf")
        .open()
        .unwrap();
    assert!(camera.start().is_err());
}

fn caller_pool(n: usize, format: PixelFormat) -> Vec<TensorDyn> {
    (0..n)
        .map(|_| {
            TensorDyn::image(
                640,
                480,
                format,
                DType::U8,
                Some(TensorMemory::DmaBuf),
                CpuAccess::ReadWrite,
            )
            .expect("DMA-BUF tensor")
        })
        .collect()
}

#[test]
fn a_caller_pool_captures_into_the_callers_tensors() {
    let Some(v) = vivid(SINGLE) else { return };
    let pool = caller_pool(3, PixelFormat::Yuyv);
    let ids: Vec<_> = pool.iter().map(|t| t.buffer_identity().id()).collect();
    let mut camera = builder(&v).with_buffers(pool).open().unwrap();
    camera.start().unwrap();
    assert_eq!(camera.config().memory, ResolvedMemory::Import);
    for _ in 0..4 {
        let f = next(&mut camera);
        let id = f.buffer_identity().id();
        assert!(ids.contains(&id));
    }
}

#[test]
fn a_mismatched_caller_pool_is_rejected_and_recoverable() {
    let Some(v) = vivid(SINGLE) else { return };
    let mut camera = builder(&v)
        .with_buffers(caller_pool(3, PixelFormat::Rgba))
        .open()
        .unwrap();
    let err = camera.start().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BuffersRejected);
    assert_eq!(err.rejection(), Some((Rejection::Format, Some(0))));
    assert_eq!(
        camera.take_buffers().len(),
        3,
        "the caller gets its pool back"
    );
    camera.set_buffers(BufferPool::Sdk).unwrap();
    camera.start().unwrap();
    next(&mut camera);
}

#[test]
#[cfg(debug_assertions)]
fn a_caller_pool_the_driver_refuses_is_rejected() {
    let Some(v) = vivid(SINGLE) else { return };
    let mut camera = builder(&v)
        .with_buffers(caller_pool(3, PixelFormat::Yuyv))
        .fault("import-qbuf")
        .open()
        .unwrap();
    let err = camera.start().unwrap_err();
    assert_eq!(
        err.kind(),
        ErrorKind::BuffersRejected,
        "never a silent fallback"
    );
    assert_eq!(
        err.rejection(),
        Some((
            Rejection::Driver {
                errno: libc::EINVAL
            },
            Some(0)
        ))
    );
    assert_eq!(camera.take_buffers().len(), 3);
}

#[test]
fn contiguous_memory_unavailable_is_reported() {
    let Some(v) = vivid(SINGLE) else { return };
    if cma() {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"SKIPPED: a CMA heap is present; this test needs a host without one\n",
        );
        return;
    }
    let mut camera = builder(&v)
        .memory(MemoryStrategy::Import)
        .contiguity(Contiguity::Required)
        .open()
        .unwrap();
    let err = camera.start().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ContiguousUnavailable);
    // The camera stays open and stopped; relaxing the requirement recovers.
    camera.set_contiguity(Contiguity::Any).unwrap();
    camera.start().unwrap();
    assert_eq!(
        camera.config().contiguous,
        Some(false),
        "system-heap memory"
    );
    next(&mut camera);
}

#[test]
fn controls_are_applied_and_read_back() {
    let Some(v) = vivid(SINGLE) else { return };
    let mut camera = builder(&v).open().unwrap();
    assert!(camera.controls().contains(ControlId::Mirror));
    assert_eq!(
        camera.set_control(Control::Mirror(Mirror::Both)).unwrap(),
        Applied::Driver
    );
    assert_eq!(
        camera.get_control(ControlId::Mirror).unwrap(),
        ControlValue::Int(3)
    );
    camera.set_control(Control::Mirror(Mirror::None)).unwrap();
    assert_eq!(
        camera.get_control(ControlId::Mirror).unwrap(),
        ControlValue::Int(0)
    );
    assert!(camera.controls().contains(ControlId::FrameRate));
    let brightness = ControlId::Custom {
        backend: edgefirst_camera::Backend::V4l2,
        id: u64::from(edgefirst_v4l2::uapi::V4L2_CID_BRIGHTNESS),
    };
    let max = match camera
        .controls()
        .get(brightness)
        .expect("vivid brightness")
        .max
    {
        ControlValue::Int(m) => m,
        other => panic!("{other:?}"),
    };
    let applied = camera
        .set_control(Control::Custom {
            backend: edgefirst_camera::Backend::V4l2,
            id: u64::from(edgefirst_v4l2::uapi::V4L2_CID_BRIGHTNESS),
            value: ControlValue::Int(max + 1000),
        })
        .unwrap();
    assert_eq!(applied, Applied::Clamped(ControlValue::Int(max)));
    assert_eq!(
        camera.get_control(brightness).unwrap(),
        ControlValue::Int(max)
    );
}

#[test]
fn timestamps_map_to_clock_and_source() {
    let Some(v) = vivid(SINGLE) else { return };
    let mut camera = builder(&v).open().unwrap();
    camera.start().unwrap();
    let a = next(&mut camera);
    let b = next(&mut camera);
    assert_eq!(a.timestamp().clock, CaptureClock::Monotonic);
    assert_eq!(a.timestamp().source, TimestampSource::EndOfFrame);
    assert!(b.timestamp().nanos > a.timestamp().nanos);
    let wall = b.realtime().expect("acquisition time");
    let age = SystemTime::now().duration_since(wall).unwrap_or_default();
    assert!(
        age < Duration::from_secs(1),
        "acquisition time is {age:?} old"
    );
    assert_eq!(a.driver_sequence().map(|s| s + 1), b.driver_sequence());
}

#[test]
fn a_timeout_is_reported_when_every_buffer_is_held() {
    let Some(v) = vivid(SINGLE) else { return };
    let mut camera = builder(&v).buffers(2).open().unwrap();
    camera.start().unwrap();
    let held = [next(&mut camera), next(&mut camera)];
    let err = camera
        .next_frame(Some(Duration::from_millis(200)))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Timeout);
    assert_eq!(camera.stats().timeouts, 1);
    assert_eq!(camera.stats().held, 2);
    drop(held);
    next(&mut camera);
}

extern "C" fn ignore(_: libc::c_int) {}

#[test]
fn a_signal_during_the_wait_does_not_fail_it() {
    let Some(v) = vivid(SINGLE) else { return };
    // SAFETY: installs a handler that does nothing, without SA_RESTART, so
    // the signal interrupts poll() with EINTR.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = ignore as *const () as usize;
        libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
    }
    let mut camera = builder(&v).buffers(2).open().unwrap();
    camera.start().unwrap();
    let held = [next(&mut camera), next(&mut camera)];
    // SAFETY: pthread_self is always valid.
    let me = unsafe { libc::pthread_self() } as usize;
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        for _ in 0..3 {
            // SAFETY: the waiting thread is alive until join below.
            unsafe { libc::pthread_kill(me as libc::pthread_t, libc::SIGUSR1) };
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(held);
    });
    let f = camera.next_frame(Some(Duration::from_secs(2)));
    releaser.join().unwrap();
    assert!(f.is_ok(), "EINTR must be retried: {:?}", f.err());
}

#[test]
fn drops_are_counted_from_timestamp_gaps() {
    let Some(v) = vivid(SINGLE) else { return };
    let mut camera = builder(&v).buffers(2).open().unwrap();
    camera.start().unwrap();
    let fps = camera
        .config()
        .frame_rate
        .expect("vivid reports a frame rate");
    let held = [next(&mut camera), next(&mut camera)];
    // With nothing queued vivid skips frames.
    let starved = Duration::from_millis(300);
    std::thread::sleep(starved);
    drop(held);
    for _ in 0..3 {
        next(&mut camera);
    }
    let missed = (starved.as_secs_f64() * fps) as u64;
    let dropped = camera.stats().dropped;
    assert!(
        dropped >= missed / 2 && dropped <= missed + 3,
        "{dropped} dropped for ~{missed} missed periods at {fps} fps"
    );
}

#[test]
fn the_wait_handle_polls_ready() {
    let Some(v) = vivid(SINGLE) else { return };
    let mut camera = builder(&v).open().unwrap();
    camera.start().unwrap();
    drop(next(&mut camera));
    let handle = camera.wait_handle().expect("a pollable fd");
    let mut pfd = libc::pollfd {
        fd: handle.raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    let n = unsafe { libc::poll(&mut pfd, 1, 3000) };
    assert_eq!(n, 1);
    assert!(pfd.revents & libc::POLLIN != 0);
}

#[test]
fn exported_frames_held_across_close_stay_readable_and_the_device_reopens() {
    let Some(v) = vivid(SINGLE) else { return };
    let mut camera = builder(&v).memory(MemoryStrategy::Export).open().unwrap();
    camera.start().unwrap();
    let held = Arc::new(next(&mut camera));
    let before = held.map_bytes(CpuAccess::Read).unwrap().to_vec();
    drop(camera);
    // The device was released at once: an exclusive reopen succeeds.
    let mut again = builder(&v).memory(MemoryStrategy::Export).open().unwrap();
    again.start().unwrap();
    next(&mut again);
    drop(again);
    let after = held.map_bytes(CpuAccess::Read).unwrap().to_vec();
    assert_eq!(before, after, "nothing writes into a detached frame");
}

#[test]
#[cfg(debug_assertions)]
fn close_is_deferred_when_the_driver_cannot_orphan_buffers() {
    let Some(v) = vivid(SINGLE) else { return };
    let mut camera = builder(&v)
        .memory(MemoryStrategy::Export)
        .fault("no-orphan")
        .open()
        .unwrap();
    camera.start().unwrap();
    let held = next(&mut camera);
    drop(camera);
    let busy = builder(&v).open().unwrap_err();
    assert_eq!(
        busy.kind(),
        ErrorKind::Busy,
        "the device stays open for the held frame"
    );
    assert!(held.map_bytes(CpuAccess::Read).is_ok());
    drop(held);
    builder(&v)
        .open()
        .expect("closed once the last frame dropped");
}

#[test]
fn an_exclusive_device_is_busy_for_a_second_user() {
    let Some(v) = vivid(SINGLE) else { return };
    let _first = builder(&v).open().unwrap();
    assert_eq!(builder(&v).open().unwrap_err().kind(), ErrorKind::Busy);
    builder(&v)
        .exclusive(false)
        .open()
        .expect("a shared open is allowed");
}

#[test]
fn unplug_is_disconnected() {
    let Some(v) = vivid(UNPLUG) else { return };
    let mut camera = builder(&v).open().unwrap();
    camera.start().unwrap();
    let held = next(&mut camera);
    // vivid's "Disconnect" button simulates a USB unplug. The instance
    // stays disconnected until the module is reloaded.
    let dev = edgefirst_v4l2::device::Device::open(&v.path).unwrap();
    let disconnect = edgefirst_v4l2::controls::query_all(&dev)
        .unwrap()
        .into_iter()
        .find(|c| c.name == "Disconnect")
        .expect("vivid Disconnect control");
    edgefirst_v4l2::controls::set(
        &dev,
        &disconnect,
        &edgefirst_v4l2::controls::ControlValue::Integer(1),
    )
    .unwrap();
    let err = (0..10)
        .find_map(|_| camera.next_frame(Some(Duration::from_millis(500))).err())
        .expect("next_frame fails after the unplug");
    assert_eq!(err.kind(), ErrorKind::Disconnected, "{err}");
    assert!(
        held.map_bytes(CpuAccess::Read).is_ok(),
        "held frames keep their memory"
    );
}

/// Real DMA-BUF frames map to `CameraFrame` planes by reference: luma and
/// chroma in one buffer, or one buffer each when the driver delivers NV12M.
#[cfg(feature = "schemas")]
#[test]
fn frames_map_to_camera_frame_planes_by_reference() {
    use edgefirst_camera::schema::FrameTensor;
    use edgefirst_schemas::edgefirst_msgs::CameraFrame;

    for (n, memory) in [
        (SINGLE, MemoryStrategy::Export),
        (SINGLE, MemoryStrategy::Import),
        (MULTI, MemoryStrategy::Export),
        (MULTI, MemoryStrategy::Import),
    ] {
        let Some(v) = vivid(n) else { return };
        let what = format!("vivid {n} {memory:?}");
        let mut camera = builder(&v)
            .format(PixelFormat::Nv12)
            .memory(memory)
            .open()
            .unwrap();
        camera.start().unwrap();
        let frame = next(&mut camera);
        assert_eq!(frame.memory(), TensorMemory::DmaBuf, "{what}");
        let tensor = FrameTensor::new(&frame, camera.config().colorimetry.as_ref()).unwrap();
        let mut cdr = Vec::new();
        tensor
            .with_fields(|f| {
                f.validate()?;
                CameraFrame::builder()
                    .frame_id("camera")
                    .seq(frame.seq())
                    .tensor(f)
                    .encode_into_vec(&mut cdr)
            })
            .unwrap_or_else(|e| panic!("{what}: {e:?}"));
        let msg = CameraFrame::from_cdr(cdr.as_slice()).unwrap();
        let t = msg.tensor();
        assert_eq!(t.storage_kind(), 2, "{what}: DMA-BUF storage kind");
        assert_eq!(t.dtype(), 0, "{what}: U8");
        assert_eq!(t.format(), "NV12", "{what}");
        let c = camera.config();
        assert_eq!(
            t.shape().collect::<Vec<_>>(),
            [u64::from(c.height), u64::from(c.width)],
            "{what}"
        );
        assert_eq!(
            t.strides().collect::<Vec<_>>(),
            [c.row_stride as i64, 1],
            "{what}"
        );
        let planes = t.planes_vec();
        assert_eq!(planes.len(), 2, "{what}: luma and chroma");
        assert!(
            planes.iter().all(|p| p.handle >= 0 && p.data.is_empty()),
            "{what}"
        );
        assert_eq!(
            planes[0].size,
            (c.row_stride * c.height as usize) as u64,
            "{what}"
        );
        if planes[1].handle == planes[0].handle {
            assert_eq!(
                planes[1].offset, planes[0].size,
                "{what}: chroma follows luma"
            );
        } else {
            assert_eq!(
                planes[1].offset, 0,
                "{what}: NV12M chroma has its own buffer"
            );
        }
    }
}

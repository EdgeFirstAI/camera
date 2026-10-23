// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! An in-process implementation of the shim ABI, for tests on machines
//! without libcamera.
//!
//! [`configure`] sets the cameras the next manager created on this thread
//! sees; [`functions`] gives the entry points to wrap with
//! [`Shim::from_functions`](crate::Shim::from_functions). Buffers are
//! memfds. A queued request completes when the consumer dequeues it: the
//! mock writes the frame's sequence number into the first byte of every
//! plane, so a test can tell whether a buffer was written after it was
//! handed out. `stop` turns in-flight requests into cancelled completions,
//! as libcamera does.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{c_char, c_int, CStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use crate::abi::*;

/// DRM fourcc of NV12.
pub const NV12: u32 = u32::from_le_bytes(*b"NV12");
/// DRM fourcc of YUYV.
pub const YUYV: u32 = u32::from_le_bytes(*b"YUYV");
/// DRM fourcc of a raw Bayer format (`pRAA`, 10-bit packed RGGB).
pub const RAW10: u32 = u32::from_le_bytes(*b"pRAA");

/// How the planes of a multi-plane buffer are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One dma-buf per plane, each at offset 0 (i.MX 95 `nxp/neo`).
    Separate,
    /// One dma-buf holding every plane at increasing offsets (RPi `pisp`).
    Shared,
}

/// A mock camera.
#[derive(Debug, Clone)]
pub struct Camera {
    /// libcamera camera id.
    pub id: String,
    /// `properties::Model`.
    pub model: String,
    /// `properties::SystemDevices`.
    pub devices: Vec<u64>,
    /// Offered formats: fourcc, width, height.
    pub formats: Vec<(u32, u32, u32)>,
    /// Plane layout of multi-plane formats.
    pub layout: Layout,
    /// `ControlInfoMap` entries: control, min, max, default.
    pub controls: Vec<(u32, f64, f64, f64)>,
    /// Whether `FrameDurationLimits` sets the delivered frame duration.
    pub honours_frame_duration: bool,
    /// Frame duration delivered otherwise, in microseconds.
    pub frame_duration_us: i64,
    /// `FrameDuration` metadata when it differs from the delivered
    /// duration, as on the i.MX 95 neo.
    pub reported_duration_us: Option<i64>,
    /// Whether orientations other than `Rotate0` are accepted.
    pub mirrors: bool,
    /// Another process holds the camera.
    pub busy: bool,
}

impl Camera {
    /// An NV12 and YUYV camera at 1920x1080 and 1280x720 with the usual
    /// controls, delivering 30 fps.
    pub fn new(id: &str, layout: Layout) -> Self {
        Self {
            id: id.to_owned(),
            model: "mock".to_owned(),
            devices: Vec::new(),
            formats: vec![
                (NV12, 1920, 1080),
                (NV12, 1280, 720),
                (YUYV, 1920, 1080),
                (YUYV, 1280, 720),
            ],
            layout,
            controls: vec![
                (
                    EFCAM_LC_CONTROL_FRAME_DURATION_LIMITS,
                    16_666.0,
                    200_000.0,
                    33_333.0,
                ),
                (EFCAM_LC_CONTROL_EXPOSURE_TIME, 100.0, 66_000.0, 20_000.0),
                (EFCAM_LC_CONTROL_ANALOGUE_GAIN, 1.0, 16.0, 1.0),
                (EFCAM_LC_CONTROL_AWB_ENABLE, 0.0, 1.0, 1.0),
                (
                    EFCAM_LC_CONTROL_COLOUR_TEMPERATURE,
                    2_500.0,
                    10_000.0,
                    5_000.0,
                ),
            ],
            honours_frame_duration: true,
            frame_duration_us: 33_333,
            reported_duration_us: None,
            mirrors: true,
            busy: false,
        }
    }
}

/// Controls applied through the mock, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A stream was configured with this request.
    Configure(efcam_lc_stream_request),
    /// The camera started with these controls.
    Start(efcam_lc_controls),
    /// A request was requeued with these (set) controls.
    Requeue(u32, efcam_lc_controls),
    /// The camera stopped.
    Stop,
    /// Buffers were freed.
    Free,
    /// The camera was released.
    Close,
}

thread_local! {
    static NEXT: RefCell<Vec<Camera>> = const { RefCell::new(Vec::new()) };
    static LOG: RefCell<Option<Arc<Mutex<Vec<Event>>>>> = const { RefCell::new(None) };
}

/// Sets the cameras of the next manager this thread creates.
pub fn configure(cameras: Vec<Camera>) {
    NEXT.with(|n| *n.borrow_mut() = cameras);
}

/// The events of the last manager this thread created.
pub fn events() -> Vec<Event> {
    LOG.with(|l| {
        l.borrow()
            .as_ref()
            .map(|log| log.lock().unwrap().clone())
            .unwrap_or_default()
    })
}

/// The mock's entry points.
pub fn functions() -> Functions {
    Functions {
        abi_version,
        build_version,
        runtime_version,
        mgr_create,
        mgr_destroy,
        mgr_camera_count,
        mgr_camera_info,
        mgr_formats,
        cam_open,
        cam_configure,
        cam_control_info,
        cam_alloc,
        cam_buffer,
        cam_start,
        cam_event_fd,
        cam_dequeue,
        cam_requeue,
        cam_stop,
        cam_free,
        cam_close,
    }
}

struct Mgr {
    cameras: Vec<Camera>,
    acquired: Mutex<Vec<String>>,
    log: Arc<Mutex<Vec<Event>>>,
}

struct Stream {
    fourcc: u32,
    height: u32,
    stride: u32,
    buffers: u32,
}

struct Cam {
    mgr: *const Mgr,
    camera: Camera,
    stream: Option<Stream>,
    buffers: Vec<Vec<(OwnedFd, u32, u32)>>,
    inflight: VecDeque<u32>,
    done: VecDeque<efcam_lc_completion>,
    efd: OwnedFd,
    running: bool,
    sequence: u32,
    duration_us: i64,
}

impl Cam {
    fn log(&self, e: Event) {
        // SAFETY: the manager outlives its cameras, as with libcamera.
        unsafe { &*self.mgr }.log.lock().unwrap().push(e);
    }

    /// Makes the eventfd readable exactly while a completion is pending.
    fn signal(&self) {
        let mut v = 0u64;
        // SAFETY: reading 8 bytes into a u64 from our own eventfd.
        unsafe { libc::read(self.efd.as_raw_fd(), (&mut v as *mut u64).cast(), 8) };
        if !self.inflight.is_empty() || !self.done.is_empty() {
            let one = 1u64;
            // SAFETY: writing 8 bytes from a u64 to our own eventfd.
            unsafe { libc::write(self.efd.as_raw_fd(), (&one as *const u64).cast(), 8) };
        }
    }

    fn capture(&mut self, slot: u32) -> efcam_lc_completion {
        let seq = self.sequence;
        self.sequence += 1;
        let mut c = efcam_lc_completion {
            slot,
            status: EFCAM_LC_STATUS_OK,
            sequence: seq,
            sensor_timestamp_ns: 1_000_000_000 + i64::from(seq) * self.duration_us * 1000,
            exposure_time_us: 20_000,
            analogue_gain: 2.0,
            frame_duration_us: self.camera.reported_duration_us.unwrap_or(self.duration_us),
            colour_temperature: 5_000,
            ..efcam_lc_completion::default()
        };
        c.buffer_timestamp_ns = c.sensor_timestamp_ns as u64 + 5_000_000;
        for (i, (fd, offset, length)) in self.buffers[slot as usize].iter().enumerate() {
            let byte = seq as u8;
            // SAFETY: writes one byte inside the memfd we sized.
            unsafe {
                libc::pwrite(
                    fd.as_raw_fd(),
                    (&byte as *const u8).cast(),
                    1,
                    i64::from(*offset),
                )
            };
            c.bytes_used[i] = *length;
        }
        c
    }
}

fn mgr<'a>(m: *mut efcam_lc_mgr) -> &'a Mgr {
    // SAFETY: handles come from mgr_create and stay valid until destroy.
    unsafe { &*(m as *const Mgr) }
}

fn cam<'a>(c: *mut efcam_lc_cam) -> &'a mut Cam {
    // SAFETY: handles come from cam_open and stay valid until close; the
    // ABI is not thread-safe per camera, as with the real shim.
    unsafe { &mut *(c as *mut Cam) }
}

unsafe fn id_of(id: *const c_char) -> String {
    // SAFETY: the caller passes a NUL-terminated id.
    unsafe { CStr::from_ptr(id) }.to_string_lossy().into_owned()
}

fn copy_out<T: Copy>(out: *mut T, mut v: T, size_of_v: impl FnOnce(&mut T) -> &mut u32) {
    // SAFETY: the caller's struct starts with its size, and is at least
    // that large.
    let size = unsafe { *(out as *const u32) } as usize;
    *size_of_v(&mut v) = size as u32;
    let n = size.min(std::mem::size_of::<T>());
    // SAFETY: copies n bytes, no more than either struct holds.
    unsafe { std::ptr::copy_nonoverlapping((&v as *const T).cast::<u8>(), out.cast::<u8>(), n) };
}

unsafe extern "C" fn abi_version() -> u32 {
    EFCAM_LC_ABI_VERSION
}

unsafe extern "C" fn build_version() -> *const c_char {
    c"0.7.0".as_ptr()
}

unsafe extern "C" fn runtime_version() -> *const c_char {
    c"v0.7.0+mock".as_ptr()
}

unsafe extern "C" fn mgr_create(out: *mut *mut efcam_lc_mgr) -> c_int {
    let log = Arc::new(Mutex::new(Vec::new()));
    LOG.with(|l| *l.borrow_mut() = Some(log.clone()));
    let m = Box::new(Mgr {
        cameras: NEXT.with(|n| n.borrow().clone()),
        acquired: Mutex::new(Vec::new()),
        log,
    });
    // SAFETY: the caller passes a valid out pointer.
    unsafe { *out = Box::into_raw(m).cast() };
    0
}

unsafe extern "C" fn mgr_destroy(m: *mut efcam_lc_mgr) {
    if !m.is_null() {
        // SAFETY: created by mgr_create.
        drop(unsafe { Box::from_raw(m as *mut Mgr) });
    }
}

unsafe extern "C" fn mgr_camera_count(m: *mut efcam_lc_mgr) -> c_int {
    mgr(m).cameras.len() as c_int
}

unsafe extern "C" fn mgr_camera_info(
    m: *mut efcam_lc_mgr,
    index: u32,
    info: *mut efcam_lc_camera_info,
) -> c_int {
    let Some(c) = mgr(m).cameras.get(index as usize) else {
        return -libc::ENOENT;
    };
    let mut v = efcam_lc_camera_info::default();
    for (d, s) in v.id.iter_mut().zip(c.id.bytes().take(255)) {
        *d = s as c_char;
    }
    for (d, s) in v.model.iter_mut().zip(c.model.bytes().take(63)) {
        *d = s as c_char;
    }
    v.location = -1;
    v.rotation = 0;
    for (d, s) in v.devices.iter_mut().zip(&c.devices) {
        *d = *s;
        v.num_devices += 1;
    }
    copy_out(info, v, |v| &mut v.size);
    0
}

unsafe extern "C" fn mgr_formats(
    m: *mut efcam_lc_mgr,
    id: *const c_char,
    _role: u32,
    out: *mut efcam_lc_format,
    capacity: u32,
) -> c_int {
    // SAFETY: the caller passes a NUL-terminated id.
    let id = unsafe { id_of(id) };
    let Some(c) = mgr(m).cameras.iter().find(|c| c.id == id) else {
        return -libc::ENODEV;
    };
    if capacity > 0 && !out.is_null() {
        // SAFETY: the first element's size is the array's element size.
        let stride = unsafe { (*out).size } as usize;
        for (i, &(fourcc, width, height)) in c.formats.iter().enumerate().take(capacity as usize) {
            let v = efcam_lc_format {
                fourcc,
                width,
                height,
                ..efcam_lc_format::default()
            };
            // SAFETY: the caller's array holds `capacity` elements of
            // `stride` bytes.
            let e = unsafe { out.cast::<u8>().add(i * stride) }.cast::<efcam_lc_format>();
            // SAFETY: as above; the element's size field is written first.
            unsafe { *(e as *mut u32) = stride as u32 };
            copy_out(e, v, |v| &mut v.size);
        }
    }
    c.formats.len() as c_int
}

unsafe extern "C" fn cam_open(
    m: *mut efcam_lc_mgr,
    id: *const c_char,
    out: *mut *mut efcam_lc_cam,
) -> c_int {
    // SAFETY: the caller passes a NUL-terminated id.
    let id = unsafe { id_of(id) };
    let manager = mgr(m);
    let Some(camera) = manager.cameras.iter().find(|c| c.id == id) else {
        return -libc::ENODEV;
    };
    let mut acquired = manager.acquired.lock().unwrap();
    if camera.busy || acquired.contains(&id) {
        return -libc::EBUSY;
    }
    // SAFETY: plain eventfd creation.
    let efd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if efd < 0 {
        return -libc::EMFILE;
    }
    acquired.push(id);
    let c = Box::new(Cam {
        mgr: manager,
        camera: camera.clone(),
        stream: None,
        buffers: Vec::new(),
        inflight: VecDeque::new(),
        done: VecDeque::new(),
        // SAFETY: a fresh fd we own.
        efd: unsafe { OwnedFd::from_raw_fd(efd) },
        running: false,
        sequence: 0,
        duration_us: camera.frame_duration_us,
    });
    // SAFETY: the caller passes a valid out pointer.
    unsafe { *out = Box::into_raw(c).cast() };
    0
}

unsafe extern "C" fn cam_configure(
    c: *mut efcam_lc_cam,
    req: *const efcam_lc_stream_request,
    info: *mut efcam_lc_stream_info,
) -> c_int {
    let c = cam(c);
    if c.running || !c.buffers.is_empty() {
        return -libc::EBUSY;
    }
    // SAFETY: the caller passes a valid request.
    let req = unsafe { *req };
    c.log(Event::Configure(req));
    let formats = &c.camera.formats;
    let Some(&(first, _, _)) = formats.first() else {
        return -libc::EINVAL;
    };
    let fourcc = if formats.iter().any(|f| f.0 == req.fourcc) {
        req.fourcc
    } else {
        first
    };
    let sizes: Vec<(u32, u32)> = formats
        .iter()
        .filter(|f| f.0 == fourcc)
        .map(|f| (f.1, f.2))
        .collect();
    let (width, height) = if sizes.contains(&(req.width, req.height)) {
        (req.width, req.height)
    } else {
        sizes[0]
    };
    let orientation = match req.orientation {
        EFCAM_LC_ORIENTATION_DEFAULT => EFCAM_LC_ORIENTATION_ROTATE0,
        o if c.camera.mirrors => o,
        _ => EFCAM_LC_ORIENTATION_ROTATE0,
    };
    let buffers = if req.buffer_count == 0 {
        4
    } else {
        req.buffer_count
    };
    let stride = if fourcc == YUYV { width * 2 } else { width };
    let frame_size = if fourcc == NV12 {
        stride * height * 3 / 2
    } else {
        stride * height
    };
    let adjusted = fourcc != req.fourcc && req.fourcc != 0
        || (width, height) != (req.width, req.height) && req.width != 0
        || orientation != req.orientation && req.orientation != EFCAM_LC_ORIENTATION_DEFAULT;
    c.stream = Some(Stream {
        fourcc,
        height,
        stride,
        buffers,
    });
    let v = efcam_lc_stream_info {
        fourcc,
        width,
        height,
        stride,
        frame_size,
        buffer_count: buffers,
        orientation,
        adjusted: u32::from(adjusted),
        ..efcam_lc_stream_info::default()
    };
    copy_out(info, v, |v| &mut v.size);
    0
}

unsafe extern "C" fn cam_control_info(
    c: *mut efcam_lc_cam,
    control: u32,
    info: *mut efcam_lc_control_info,
) -> c_int {
    let Some(&(_, min, max, def)) = cam(c).camera.controls.iter().find(|e| e.0 == control) else {
        return -libc::ENOENT;
    };
    let v = efcam_lc_control_info {
        min,
        max,
        def,
        ..efcam_lc_control_info::default()
    };
    copy_out(info, v, |v| &mut v.size);
    0
}

fn memfd(len: u32) -> Option<OwnedFd> {
    // SAFETY: plain memfd creation with a static name.
    let fd = unsafe { libc::memfd_create(c"efcam-mock".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: a fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: sizes our own memfd.
    (unsafe { libc::ftruncate(fd.as_raw_fd(), i64::from(len)) } == 0).then_some(fd)
}

unsafe extern "C" fn cam_alloc(c: *mut efcam_lc_cam) -> c_int {
    let c = cam(c);
    let Some(s) = &c.stream else {
        return -libc::EINVAL;
    };
    if !c.buffers.is_empty() {
        return -libc::EBUSY;
    }
    let luma = s.stride * s.height;
    let planes: Vec<u32> = if s.fourcc == NV12 {
        vec![luma, luma / 2]
    } else {
        vec![luma]
    };
    let mut buffers = Vec::new();
    for _ in 0..s.buffers {
        let mut buffer = Vec::new();
        match c.camera.layout {
            Layout::Separate => {
                for &len in &planes {
                    let Some(fd) = memfd(len) else {
                        return -libc::ENOMEM;
                    };
                    buffer.push((fd, 0, len));
                }
            }
            Layout::Shared => {
                let Some(fd) = memfd(planes.iter().sum()) else {
                    return -libc::ENOMEM;
                };
                let mut offset = 0;
                for &len in &planes {
                    let Ok(dup) = fd.try_clone() else {
                        return -libc::EMFILE;
                    };
                    buffer.push((dup, offset, len));
                    offset += len;
                }
            }
        }
        buffers.push(buffer);
    }
    c.buffers = buffers;
    c.buffers.len() as c_int
}

unsafe extern "C" fn cam_buffer(
    c: *mut efcam_lc_cam,
    slot: u32,
    buf: *mut efcam_lc_buffer,
) -> c_int {
    let c = cam(c);
    let Some(planes) = c.buffers.get(slot as usize) else {
        return -libc::ENOENT;
    };
    let mut v = efcam_lc_buffer {
        num_planes: planes.len() as u32,
        ..efcam_lc_buffer::default()
    };
    // Shared planes report one fd number, as libcamera does.
    let first = planes[0].0.as_raw_fd();
    for (p, (fd, offset, length)) in v.planes.iter_mut().zip(planes) {
        p.fd = if c.camera.layout == Layout::Shared {
            first
        } else {
            fd.as_raw_fd()
        };
        p.offset = *offset;
        p.length = *length;
    }
    copy_out(buf, v, |v| &mut v.size);
    0
}

fn requested_duration(c: &Cam, ctrls: &efcam_lc_controls) -> Option<i64> {
    (c.camera.honours_frame_duration && ctrls.frame_duration_min_us > 0)
        .then_some(ctrls.frame_duration_min_us)
}

unsafe extern "C" fn cam_start(c: *mut efcam_lc_cam, ctrls: *const efcam_lc_controls) -> c_int {
    let c = cam(c);
    if c.running {
        return -libc::EBUSY;
    }
    if c.buffers.is_empty() {
        return -libc::EINVAL;
    }
    let ctrls = if ctrls.is_null() {
        efcam_lc_controls::unset()
    } else {
        // SAFETY: the caller passes a valid struct.
        unsafe { *ctrls }
    };
    if let Some(d) = requested_duration(c, &ctrls) {
        c.duration_us = d;
    }
    c.log(Event::Start(ctrls));
    c.running = true;
    c.inflight = (0..c.buffers.len() as u32).collect();
    c.signal();
    0
}

unsafe extern "C" fn cam_event_fd(c: *mut efcam_lc_cam) -> c_int {
    cam(c).efd.as_raw_fd()
}

unsafe extern "C" fn cam_dequeue(c: *mut efcam_lc_cam, out: *mut efcam_lc_completion) -> c_int {
    let c = cam(c);
    let next = match c.done.pop_front() {
        Some(done) => Some(done),
        None => c.inflight.pop_front().map(|slot| c.capture(slot)),
    };
    c.signal();
    match next {
        Some(v) => {
            copy_out(out, v, |v| &mut v.size);
            1
        }
        None => 0,
    }
}

unsafe extern "C" fn cam_requeue(
    c: *mut efcam_lc_cam,
    slot: u32,
    ctrls: *const efcam_lc_controls,
) -> c_int {
    let c = cam(c);
    if slot as usize >= c.buffers.len() {
        return -libc::ENOENT;
    }
    if !c.running {
        return -libc::ESHUTDOWN;
    }
    if !ctrls.is_null() {
        // SAFETY: the caller passes a valid struct.
        let ctrls = unsafe { *ctrls };
        if !ctrls.is_unset() {
            if let Some(d) = requested_duration(c, &ctrls) {
                c.duration_us = d;
            }
            c.log(Event::Requeue(slot, ctrls));
        }
    }
    c.inflight.push_back(slot);
    c.signal();
    0
}

unsafe extern "C" fn cam_stop(c: *mut efcam_lc_cam) -> c_int {
    let c = cam(c);
    if !c.running {
        return 0;
    }
    c.running = false;
    while let Some(slot) = c.inflight.pop_front() {
        c.done.push_back(efcam_lc_completion {
            slot,
            status: EFCAM_LC_STATUS_CANCELLED,
            sensor_timestamp_ns: -1,
            ..efcam_lc_completion::default()
        });
    }
    c.log(Event::Stop);
    c.signal();
    0
}

unsafe extern "C" fn cam_free(handle: *mut efcam_lc_cam) -> c_int {
    // SAFETY: forwarded handle.
    unsafe { cam_stop(handle) };
    let c = cam(handle);
    c.buffers.clear();
    c.done.clear();
    c.log(Event::Free);
    c.signal();
    0
}

unsafe extern "C" fn cam_close(handle: *mut efcam_lc_cam) {
    if handle.is_null() {
        return;
    }
    // SAFETY: forwarded handle.
    unsafe { cam_free(handle) };
    // SAFETY: created by cam_open; not used after this.
    let c = unsafe { Box::from_raw(handle as *mut Cam) };
    c.log(Event::Close);
    // SAFETY: the manager outlives its cameras.
    let m = unsafe { &*c.mgr };
    m.acquired.lock().unwrap().retain(|id| *id != c.camera.id);
}

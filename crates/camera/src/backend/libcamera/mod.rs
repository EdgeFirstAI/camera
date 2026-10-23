// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! libcamera capture backend (feature `libcamera`, Linux).
//!
//! libcamera is reached through `edgefirst-libcamera-sys`, which loads the
//! shim built for the installed libcamera at run time; nothing links it.
//!
//! - **Buffers** are libcamera's own (Export only): its allocator creates
//!   them with V4L2 `MMAP` + `EXPBUF` and orphans them at once, so each
//!   plane's dma-buf lives as long as a duplicate of its fd. Planes may be
//!   separate dma-bufs (i.MX 95 `nxp/neo`) or one dma-buf at offsets (RPi
//!   `pisp`). Caller pools and `MemoryStrategy::Import` are refused.
//! - **Requests** carry the slot index. Completions arrive on libcamera's
//!   thread, which only queues them and signals an eventfd; every call into
//!   libcamera is made from the thread that owns the camera.
//! - **Controls** are checked against the camera's `ControlInfoMap` and
//!   reported, never emulated. A pipeline that ignores a control (the
//!   i.MX 95 `nxp/neo` frame rate) shows in `config()`, which reports the
//!   frame duration the camera delivers once streaming.
//! - **Timestamps** are `SensorTimestamp`, which libcamera copies from the
//!   capture node's `CLOCK_MONOTONIC` buffer timestamp.
//! - **`stop()`** frees the buffers; frames held across it keep their
//!   memory and are detached.

mod describe;
mod manager;

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::{Duration, Instant};

use edgefirst_libcamera_sys as sys;
use edgefirst_tensor::{DType, PixelFormat, Tensor, TensorDyn};

use self::manager::{c_id, errno_error, Manager};
use crate::builder::Source;
use crate::{
    Applied, BufferPool, Camera, CameraBuilder, CameraDescriptor, CaptureClock, CaptureStats,
    Contiguity, Control, ControlId, ControlInfo, ControlSet, ControlValue, Error, ErrorKind,
    Exposure, FormatInfo, Frame, FrameMeta, Gain, MemoryStrategy, Mirror, PlaneLayout,
    RealtimeClock, Rejection, ResolvedMemory, Result, SlotTable, StreamConfig, Timestamp,
    TimestampSource, Unsupported, WaitHandle, WhiteBalance,
};

pub(crate) use self::manager::set_match_list;

const MIN_BUFFERS: usize = 2;
const MAX_BUFFERS: usize = 32;
/// How long `start()` waits for the first frame before `NotReady`.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// Frames whose sensor-timestamp intervals give the delivered rate.
const RATE_FRAMES: usize = 10;
/// Poll slice while every buffer is held, so slots released on other
/// threads go back to libcamera promptly.
const STARVED_POLL: Duration = Duration::from_millis(20);

pub(crate) fn open(builder: CameraBuilder) -> Result<Box<dyn Camera>> {
    open_on(Manager::shared()?, builder)
}

pub(crate) fn enumerate() -> Result<Vec<CameraDescriptor>> {
    let m = match Manager::shared() {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            tracing::debug!("{e}");
            return Ok(Vec::new());
        }
        Err(e) => {
            tracing::warn!("{e}");
            return Ok(Vec::new());
        }
    };
    Ok(m.cameras()
        .iter()
        .map(|info| describe::descriptor(&m, info))
        .collect())
}

pub(crate) fn probe(d: &CameraDescriptor) -> Result<Vec<FormatInfo>> {
    describe::probe(&Manager::shared()?, d)
}

/// An acquired libcamera camera.
#[derive(Debug)]
pub(crate) struct Handle {
    // Declared before `manager`: the camera is released before the
    // manager can stop.
    raw: NonNull<sys::efcam_lc_cam>,
    manager: Arc<Manager>,
}

// SAFETY: the shim camera is used only through `&mut` or from the owning
// thread; libcamera's own thread touches only the shim's locked queue.
unsafe impl Send for Handle {}

impl Handle {
    pub(crate) fn open(manager: Arc<Manager>, id: &str) -> Result<Self> {
        let cid = c_id(id)?;
        let mut raw = std::ptr::null_mut();
        // SAFETY: a live manager, a NUL-terminated id and an out pointer.
        let ret = unsafe { (manager.functions().cam_open)(manager.raw(), cid.as_ptr(), &mut raw) };
        if ret < 0 {
            return Err(errno_error(ret, &format!("open libcamera camera {id}")));
        }
        let raw = NonNull::new(raw).ok_or_else(|| {
            Error::new(ErrorKind::Backend, "the libcamera shim returned no camera")
        })?;
        Ok(Self { raw, manager })
    }

    fn f(&self) -> &sys::Functions {
        self.manager.functions()
    }

    fn raw(&self) -> *mut sys::efcam_lc_cam {
        self.raw.as_ptr()
    }

    pub(crate) fn configure(
        &self,
        req: &sys::efcam_lc_stream_request,
    ) -> Result<sys::efcam_lc_stream_info> {
        let mut info = sys::efcam_lc_stream_info::default();
        // SAFETY: a live camera and sized structs.
        let ret = unsafe { (self.f().cam_configure)(self.raw(), req, &mut info) };
        if ret < 0 {
            return Err(errno_error(ret, "configure the libcamera stream"));
        }
        Ok(info)
    }

    pub(crate) fn control_info(&self, control: u32) -> Option<sys::efcam_lc_control_info> {
        let mut info = sys::efcam_lc_control_info::default();
        // SAFETY: a live camera and a sized struct.
        (unsafe { (self.f().cam_control_info)(self.raw(), control, &mut info) } == 0)
            .then_some(info)
    }

    fn alloc(&self) -> Result<u32> {
        // SAFETY: a live camera.
        let ret = unsafe { (self.f().cam_alloc)(self.raw()) };
        if ret < 0 {
            return Err(errno_error(ret, "allocate libcamera buffers"));
        }
        Ok(ret as u32)
    }

    fn buffer(&self, slot: u32) -> Result<sys::efcam_lc_buffer> {
        let mut buf = sys::efcam_lc_buffer::default();
        // SAFETY: a live camera and a sized struct.
        let ret = unsafe { (self.f().cam_buffer)(self.raw(), slot, &mut buf) };
        if ret < 0 {
            return Err(errno_error(ret, &format!("read libcamera buffer {slot}")));
        }
        Ok(buf)
    }

    fn start(&self, controls: &sys::efcam_lc_controls) -> Result<()> {
        // SAFETY: a live camera and a sized struct.
        let ret = unsafe { (self.f().cam_start)(self.raw(), controls) };
        if ret < 0 {
            return Err(errno_error(ret, "start the libcamera camera"));
        }
        Ok(())
    }

    fn event_fd(&self) -> i32 {
        // SAFETY: a live camera.
        unsafe { (self.f().cam_event_fd)(self.raw()) }
    }

    fn dequeue(&self) -> Option<sys::efcam_lc_completion> {
        let mut c = sys::efcam_lc_completion::default();
        // SAFETY: a live camera and a sized struct.
        (unsafe { (self.f().cam_dequeue)(self.raw(), &mut c) } == 1).then_some(c)
    }

    fn requeue(&self, slot: u32, controls: Option<&sys::efcam_lc_controls>) -> Result<()> {
        let p = controls.map_or(std::ptr::null(), |c| c as *const _);
        // SAFETY: a live camera and a sized struct or null.
        let ret = unsafe { (self.f().cam_requeue)(self.raw(), slot, p) };
        if ret < 0 {
            return Err(errno_error(ret, &format!("queue libcamera request {slot}")));
        }
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        // SAFETY: a live camera.
        let ret = unsafe { (self.f().cam_stop)(self.raw()) };
        if ret < 0 {
            return Err(errno_error(ret, "stop the libcamera camera"));
        }
        Ok(())
    }

    fn free(&self) -> Result<()> {
        // SAFETY: a live camera.
        let ret = unsafe { (self.f().cam_free)(self.raw()) };
        if ret < 0 {
            return Err(errno_error(ret, "free libcamera buffers"));
        }
        Ok(())
    }

    /// Waits until a completion is pending; `false` on timeout.
    fn wait(&self, timeout: Option<Duration>) -> Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.event_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
        loop {
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut pfd, 1, ms) };
            if n >= 0 {
                return Ok(n > 0);
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(Error::new(ErrorKind::Io, "poll the libcamera eventfd").with_source(e));
            }
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is live and not used after this.
        unsafe { (self.f().cam_close)(self.raw()) };
    }
}

/// One plane of a buffer: the duplicated fd number, offset and length.
type PlaneRef = (i64, usize, usize);

/// The buffers in use and the native handle of each plane, per slot.
#[derive(Debug)]
struct Pool {
    table: SlotTable,
    planes: Vec<Vec<PlaneRef>>,
}

#[derive(Debug)]
struct LibcameraCamera {
    descriptor: CameraDescriptor,
    request: crate::StreamRequest,
    config: StreamConfig,
    set: ControlSet,
    stream: sys::efcam_lc_stream_info,
    /// Controls for the next start or requeue.
    pending: sys::efcam_lc_controls,
    /// The frame rate asked for, to report a pipeline that ignores it.
    asked_fps: Option<f64>,
    /// The last completion, for reading controls back.
    last: Option<sys::efcam_lc_completion>,
    provided: Option<Vec<TensorDyn>>,
    pool: Option<Pool>,
    /// Frames `start()` dequeued, not yet returned.
    early: VecDeque<sys::efcam_lc_completion>,
    streaming: bool,
    source: TimestampSource,
    clock: RealtimeClock,
    seq: u64,
    stats: CaptureStats,
    last_nanos: Option<i64>,
    /// Sensor-timestamp intervals until the delivered rate is measured.
    intervals: Vec<i64>,
    /// The measured frame interval, once known.
    period_ns: Option<f64>,
    // Declared last: released after the pool's frames are detached.
    handle: Handle,
}

/// The libcamera orientation for a mirror request.
fn orientation(m: Mirror) -> u32 {
    match m {
        Mirror::None => sys::EFCAM_LC_ORIENTATION_ROTATE0,
        Mirror::Horizontal => sys::EFCAM_LC_ORIENTATION_ROTATE0_MIRROR,
        Mirror::Vertical => sys::EFCAM_LC_ORIENTATION_ROTATE180_MIRROR,
        Mirror::Both => sys::EFCAM_LC_ORIENTATION_ROTATE180,
    }
}

fn mirror_of(o: u32) -> Option<Mirror> {
    [
        Mirror::None,
        Mirror::Horizontal,
        Mirror::Vertical,
        Mirror::Both,
    ]
    .into_iter()
    .find(|&m| orientation(m) == o)
}

/// Opens camera `builder.source` on `manager`.
pub(crate) fn open_on(manager: Arc<Manager>, builder: CameraBuilder) -> Result<Box<dyn Camera>> {
    let Source::Libcamera(id) = &builder.source else {
        return Err(Error::new(
            ErrorKind::InvalidConfig,
            "not a libcamera source",
        ));
    };
    let request = builder.request;
    if !(MIN_BUFFERS..=MAX_BUFFERS).contains(&request.buffers) {
        return Err(Error::new(
            ErrorKind::InvalidConfig,
            format!(
                "pool depth {} outside {MIN_BUFFERS}..={MAX_BUFFERS}",
                request.buffers
            ),
        ));
    }
    if request.memory == MemoryStrategy::Import {
        return Err(Error::new(
            ErrorKind::InvalidConfig,
            "the libcamera backend captures into libcamera's own buffers; use Export or Auto",
        ));
    }
    let _span = tracing::debug_span!("camera.libcamera.open", id = %id).entered();
    let info = manager
        .cameras()
        .into_iter()
        .find(|c| sys::field_str(&c.id) == *id)
        .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no libcamera camera {id}")))?;
    let descriptor = describe::descriptor(&manager, &info);
    let source = describe::timestamp_source(&describe::drivers(&info));
    let handle = Handle::open(manager, id)?;

    let fourcc = match request.format {
        Some(f) => describe::drm_fourcc(f).ok_or_else(|| {
            Error::new(
                ErrorKind::UnsupportedFormat,
                format!("{id}: libcamera cannot capture {f:?}"),
            )
        })?,
        None => 0,
    };
    let (width, height) = request.size.unwrap_or((0, 0));
    let req = sys::efcam_lc_stream_request {
        role: sys::EFCAM_LC_ROLE_VIDEO,
        fourcc,
        width,
        height,
        buffer_count: request.buffers as u32,
        orientation: request
            .mirror
            .map_or(sys::EFCAM_LC_ORIENTATION_DEFAULT, orientation),
        ..Default::default()
    };
    let stream = handle.configure(&req)?;
    let Some(format) = describe::pixel_format(stream.fourcc) else {
        return Err(Error::new(
            ErrorKind::UnsupportedFormat,
            format!(
                "{id}: libcamera configured {}, which frames cannot carry",
                describe::fourcc_str(stream.fourcc)
            ),
        ));
    };
    if fourcc != 0 && stream.fourcc != fourcc {
        return Err(Error::new(
            ErrorKind::UnsupportedFormat,
            format!(
                "{id}: {:?} is not offered; libcamera chose {format:?}",
                request.format.unwrap_or(format)
            ),
        ));
    }
    if stream.adjusted != 0 {
        tracing::info!(
            "{id}: libcamera adjusted the request to {}x{} {format:?}",
            stream.width,
            stream.height
        );
    }

    let mut camera = LibcameraCamera {
        config: config_of(&stream, format),
        set: control_set(&handle),
        descriptor,
        request,
        stream,
        pending: sys::efcam_lc_controls::unset(),
        asked_fps: None,
        last: None,
        provided: builder.provided,
        pool: None,
        early: VecDeque::new(),
        streaming: false,
        source,
        clock: RealtimeClock::new(),
        seq: 0,
        stats: CaptureStats::default(),
        last_nanos: None,
        intervals: Vec::new(),
        period_ns: None,
        handle,
    };
    if let Some(m) = camera.request.mirror {
        if mirror_of(camera.stream.orientation) != Some(m) {
            tracing::warn!("{id}: the pipeline cannot mirror {m:?}");
        }
    }
    let mut requested = builder.controls;
    if let Some(fps) = camera.request.frame_rate {
        requested.insert(0, Control::FrameRate(fps));
    }
    for control in requested {
        if let Applied::Unsupported(why) = camera.set_control(control)? {
            tracing::warn!("{id}: control {:?} unsupported ({why:?})", control.id());
        }
    }
    Ok(Box::new(camera))
}

fn round_fps(fps: f64) -> f64 {
    (fps * 100.0).round() / 100.0
}

fn config_of(s: &sys::efcam_lc_stream_info, format: PixelFormat) -> StreamConfig {
    let mut c = StreamConfig::new(s.width, s.height, format, s.stride as usize);
    c.planes = format
        .plane_table(s.width as usize, s.height as usize, s.stride as usize)
        .map_or(1, |t| t.len());
    c.buffer_count = s.buffer_count as usize;
    c.memory = ResolvedMemory::Export;
    c
}

/// The controls the configured camera offers, from its `ControlInfoMap`.
fn control_set(h: &Handle) -> ControlSet {
    let mut set = ControlSet::new();
    let (f, i) = (ControlValue::Float, ControlValue::Int);
    if let Some(c) = h.control_info(sys::EFCAM_LC_CONTROL_FRAME_DURATION_LIMITS) {
        if c.min > 0.0 && c.max >= c.min {
            let def = if c.def > 0.0 {
                1e6 / c.def
            } else {
                1e6 / c.min
            };
            set.insert(
                ControlId::FrameRate,
                ControlInfo::new(f(1e6 / c.max), f(1e6 / c.min), f(0.0), f(def)),
            );
        }
    }
    if let Some(c) = h.control_info(sys::EFCAM_LC_CONTROL_EXPOSURE_TIME) {
        set.insert(
            ControlId::Exposure,
            ControlInfo::new(i(c.min as i64), i(c.max as i64), i(1), i(c.def as i64)),
        );
    }
    if let Some(c) = h.control_info(sys::EFCAM_LC_CONTROL_ANALOGUE_GAIN) {
        set.insert(
            ControlId::Gain,
            ControlInfo::new(f(c.min), f(c.max), f(0.0), f(c.def)),
        );
    }
    if let Some(c) = h.control_info(sys::EFCAM_LC_CONTROL_COLOUR_TEMPERATURE) {
        set.insert(
            ControlId::WhiteBalance,
            ControlInfo::new(i(c.min as i64), i(c.max as i64), i(1), i(c.def as i64)),
        );
    }
    set.insert(ControlId::Mirror, ControlInfo::new(i(0), i(3), i(1), i(0)));
    set
}

fn clamp(v: f64, info: Option<&ControlInfo>) -> f64 {
    match info {
        Some(ControlInfo {
            min: ControlValue::Float(lo),
            max: ControlValue::Float(hi),
            ..
        }) => v.clamp(*lo, *hi),
        Some(ControlInfo {
            min: ControlValue::Int(lo),
            max: ControlValue::Int(hi),
            ..
        }) => v.clamp(*lo as f64, *hi as f64),
        _ => v,
    }
}

/// `Driver` when `got` is what was asked, else `Clamped`.
fn applied(asked: f64, got: f64, value: ControlValue) -> Applied {
    if (asked - got).abs() <= f64::EPSILON * asked.abs().max(1.0) {
        Applied::Driver
    } else {
        Applied::Clamped(value)
    }
}

/// Duplicates a borrowed plane fd.
fn dup(fd: i32) -> Result<OwnedFd> {
    // SAFETY: the shim keeps the fd open while its buffer is allocated.
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    fd.try_clone_to_owned()
        .map_err(|e| Error::new(ErrorKind::Io, "duplicate a libcamera buffer fd").with_source(e))
}

/// Whether two fds refer to the same buffer.
fn same_buffer(a: &OwnedFd, b: &OwnedFd) -> bool {
    let stat = |fd: &OwnedFd| {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: a valid fd and a zeroed stat buffer.
        (unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } == 0).then_some((st.st_dev, st.st_ino))
    };
    matches!((stat(a), stat(b)), (Some(x), Some(y)) if x == y)
}

impl LibcameraCamera {
    fn id(&self) -> &str {
        &self.descriptor.id
    }

    fn format(&self) -> PixelFormat {
        self.config.format
    }

    /// Wraps the planes of buffer `slot` as one tensor, and records each
    /// plane's duplicated fd, offset and length.
    fn wrap(&self, slot: u32) -> Result<(TensorDyn, Vec<PlaneRef>)> {
        let buf = self.handle.buffer(slot)?;
        let planes = &buf.planes[..(buf.num_planes as usize).min(buf.planes.len())];
        let format = self.format();
        let (w, h) = (self.config.width as usize, self.config.height as usize);
        let stride = self.config.row_stride;
        let fds: Vec<OwnedFd> = planes.iter().map(|p| dup(p.fd)).collect::<Result<_>>()?;
        let layout = |handles: &[i64]| -> Vec<PlaneRef> {
            planes
                .iter()
                .zip(handles)
                .map(|(p, &h)| (h, p.offset as usize, p.length as usize))
                .collect()
        };
        let shape = format.allocation_shape(w, h).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidConfig,
                format!("no tensor shape for {format:?} {w}x{h}"),
            )
        })?;

        let contiguous = match planes {
            [_] => true,
            [luma, chroma] => {
                same_buffer(&fds[0], &fds[1])
                    && chroma.offset as usize == luma.offset as usize + stride * h
            }
            _ => false,
        };
        // The tensors keep the fds they are given, so these numbers stay
        // valid as long as the tensor.
        let handles: Vec<i64> = fds.iter().map(|fd| i64::from(fd.as_raw_fd())).collect();
        if contiguous {
            let mut fds = fds.into_iter();
            let fd = fds.next().expect("at least one plane");
            let mut t = TensorDyn::from_fd(fd, &shape, DType::U8, None)?;
            t.set_format(format)?;
            t.set_row_stride(stride)?;
            if planes[0].offset != 0 {
                t.set_plane_offset(planes[0].offset as usize);
            }
            // Further planes share the first plane's buffer.
            return Ok((t, layout(&vec![handles[0]; planes.len()])));
        }
        if planes.len() != 2 || !matches!(format, PixelFormat::Nv12 | PixelFormat::Nv16) {
            return Err(Error::new(
                ErrorKind::Backend,
                format!("{}: {} planes for {format:?}", self.id(), planes.len()),
            ));
        }
        let chroma_rows = if format == PixelFormat::Nv12 {
            h.div_ceil(2)
        } else {
            h
        };
        let mut fds = fds.into_iter();
        // Raw planes carry no format, so their pitch is set unchecked; it is
        // libcamera's stride for both planes of NV12 and NV16.
        let mut luma = Tensor::<u8>::from_fd(fds.next().expect("two planes"), &[h, w], None)?;
        luma.set_row_stride_unchecked(stride);
        if planes[0].offset != 0 {
            luma.set_plane_offset(planes[0].offset as usize);
        }
        let mut chroma =
            Tensor::<u8>::from_fd(fds.next().expect("two planes"), &[chroma_rows, w], None)?;
        chroma.set_row_stride_unchecked(stride);
        if planes[1].offset != 0 {
            chroma.set_plane_offset(planes[1].offset as usize);
        }
        let t = Tensor::<u8>::from_planes(luma, chroma, format)?;
        Ok((TensorDyn::from(t), layout(&handles)))
    }

    /// Allocates libcamera's buffers and wraps them.
    fn setup(&mut self) -> Result<()> {
        let count = self.handle.alloc()?;
        let built: Result<Vec<_>> = (0..count).map(|slot| self.wrap(slot)).collect();
        let built = match built {
            Ok(b) => b,
            Err(e) => {
                let _ = self.handle.free();
                return Err(e);
            }
        };
        let (tensors, planes): (Vec<_>, Vec<_>) = built.into_iter().unzip();
        self.config.buffer_count = tensors.len();
        self.config.contiguous = None;
        self.pool = Some(Pool {
            table: SlotTable::new(tensors),
            planes,
        });
        Ok(())
    }

    /// Stops and frees libcamera's buffers; held frames are detached and
    /// keep their memory.
    fn halt(&mut self) -> Result<()> {
        self.streaming = false;
        self.early.clear();
        self.last_nanos = None;
        self.intervals.clear();
        self.period_ns = None;
        let stop = self.handle.stop();
        while self.handle.dequeue().is_some() {}
        let free = self.handle.free();
        if let Some(pool) = self.pool.take() {
            pool.table.detach();
        }
        stop.and(free)
    }

    /// Waits for the next completed frame while starting.
    fn first_frame(&mut self) -> Result<sys::efcam_lc_completion> {
        let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
        loop {
            while let Some(c) = self.handle.dequeue() {
                match c.status {
                    sys::EFCAM_LC_STATUS_OK => return Ok(c),
                    sys::EFCAM_LC_STATUS_ERROR => {
                        self.stats.errors += 1;
                        self.handle.requeue(c.slot, None)?;
                    }
                    _ => {}
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.handle.wait(Some(left))? {
                return Err(Error::new(
                    ErrorKind::NotReady,
                    format!(
                        "{}: no frame within {FIRST_FRAME_TIMEOUT:?} of starting",
                        self.id()
                    ),
                ));
            }
        }
    }

    /// Reports the rate libcamera's frame duration claims, until frames
    /// measure it.
    fn note_rate(&mut self, c: &sys::efcam_lc_completion) {
        if c.frame_duration_us > 0 {
            self.config.frame_rate = Some(round_fps(1e6 / c.frame_duration_us as f64));
        }
    }

    /// Measures the delivered rate from the median sensor-timestamp
    /// interval of the first frames, which pipelines report more reliably
    /// than their frame-duration metadata, and warns when it is not the
    /// rate asked for.
    fn measure_rate(&mut self, interval: i64) {
        if self.period_ns.is_some() || interval <= 0 {
            return;
        }
        self.intervals.push(interval);
        if self.intervals.len() < RATE_FRAMES {
            return;
        }
        self.intervals.sort_unstable();
        let median = self.intervals[self.intervals.len() / 2] as f64;
        self.period_ns = Some(median);
        let fps = round_fps(1e9 / median);
        self.config.frame_rate = Some(fps);
        if let Some(asked) = self.asked_fps {
            if (fps - asked).abs() > asked * 0.01 {
                tracing::warn!(
                    "{}: {asked} fps requested; the camera delivers {fps} fps",
                    self.id()
                );
            }
        }
    }

    fn requeue_released(&mut self) -> Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };
        while let Some(slot) = pool.table.pop_released() {
            let controls = (!self.pending.is_unset()).then_some(self.pending);
            self.handle.requeue(slot as u32, controls.as_ref())?;
            self.pending = sys::efcam_lc_controls::unset();
        }
        Ok(())
    }

    fn frame(&mut self, c: sys::efcam_lc_completion) -> Frame {
        let timestamp = if c.sensor_timestamp_ns >= 0 {
            Timestamp {
                clock: CaptureClock::Monotonic,
                source: self.source,
                nanos: c.sensor_timestamp_ns,
            }
        } else {
            Timestamp {
                clock: CaptureClock::Monotonic,
                source: TimestampSource::Unknown,
                nanos: i64::try_from(c.buffer_timestamp_ns).unwrap_or(i64::MAX),
            }
        };
        let period = self
            .period_ns
            .or_else(|| (c.frame_duration_us > 0).then_some(c.frame_duration_us as f64 * 1e3));
        if let Some(last) = self.last_nanos {
            let interval = timestamp.nanos - last;
            if let Some(period) = period {
                let periods = (interval as f64 / period).round() as i64;
                if periods > 1 {
                    self.stats.dropped += (periods - 1) as u64;
                }
            }
            self.measure_rate(interval);
        }
        self.last_nanos = Some(timestamp.nanos);
        self.last = Some(c);

        let pool = self.pool.as_ref().expect("frames come from a pool");
        let slot = c.slot as usize;
        let mut meta = FrameMeta::new(slot, self.seq, timestamp);
        self.seq += 1;
        meta.driver_sequence = Some(c.sequence);
        meta.realtime = self.clock.to_realtime(timestamp).ok();
        let stride = self.config.row_stride;
        meta.planes = pool.planes[slot]
            .iter()
            .zip(c.bytes_used)
            .map(|(&(handle, offset, length), used)| {
                let mut l = PlaneLayout::new(handle, offset, stride, length);
                l.used = used as usize;
                l
            })
            .collect();
        meta.bytes_used = meta.planes.iter().map(|p| p.used).sum();
        self.stats.frames += 1;
        let _span =
            tracing::trace_span!("camera.libcamera.dequeue", seq = meta.seq, slot).entered();
        pool.table.frame(meta)
    }

    /// Reconfigures for a new orientation while stopped.
    fn set_mirror(&mut self, m: Mirror) -> Result<Applied> {
        if self.streaming {
            return Err(Error::new(
                ErrorKind::InvalidConfig,
                "libcamera applies mirroring at configuration; stop the camera first",
            ));
        }
        if mirror_of(self.stream.orientation) == Some(m) {
            return Ok(Applied::Driver);
        }
        let mut req = sys::efcam_lc_stream_request {
            role: sys::EFCAM_LC_ROLE_VIDEO,
            fourcc: self.stream.fourcc,
            width: self.stream.width,
            height: self.stream.height,
            buffer_count: self.request.buffers as u32,
            orientation: orientation(m),
            ..Default::default()
        };
        let got = self.handle.configure(&req)?;
        let same_stream = (got.fourcc, got.width, got.height)
            == (self.stream.fourcc, self.stream.width, self.stream.height);
        if got.orientation == req.orientation && same_stream {
            self.stream = got;
            return Ok(Applied::Driver);
        }
        req.orientation = self.stream.orientation;
        self.stream = self.handle.configure(&req)?;
        Ok(Applied::Unsupported(Unsupported::Camera))
    }
}

impl Drop for LibcameraCamera {
    fn drop(&mut self) {
        if self.streaming || self.pool.is_some() {
            let _ = self.halt();
        }
    }
}

impl Camera for LibcameraCamera {
    fn descriptor(&self) -> &CameraDescriptor {
        &self.descriptor
    }

    fn config(&self) -> &StreamConfig {
        &self.config
    }

    fn controls(&self) -> &ControlSet {
        &self.set
    }

    fn get_control(&self, id: ControlId) -> Result<ControlValue> {
        let unsupported = || {
            Error::new(
                ErrorKind::InvalidConfig,
                format!("{}: {id:?} is not available", self.descriptor.id),
            )
        };
        if !self.set.contains(id) {
            return Err(unsupported());
        }
        let last = self.last.as_ref();
        let value = match id {
            ControlId::FrameRate => self.config.frame_rate.map(ControlValue::Float),
            ControlId::Mirror => mirror_of(self.stream.orientation).map(|m| {
                ControlValue::Int(match m {
                    Mirror::None => 0,
                    Mirror::Horizontal => 1,
                    Mirror::Vertical => 2,
                    Mirror::Both => 3,
                })
            }),
            ControlId::Exposure => last
                .filter(|c| c.exposure_time_us >= 0)
                .map(|c| ControlValue::Int(c.exposure_time_us)),
            ControlId::Gain => last
                .filter(|c| c.analogue_gain >= 0.0)
                .map(|c| ControlValue::Float(c.analogue_gain)),
            ControlId::WhiteBalance => last
                .filter(|c| c.colour_temperature >= 0)
                .map(|c| ControlValue::Int(i64::from(c.colour_temperature))),
            _ => None,
        };
        value
            .or_else(|| self.set.get(id).map(|i| i.default))
            .ok_or_else(unsupported)
    }

    fn set_control(&mut self, ctl: Control) -> Result<Applied> {
        let unsupported = Ok(Applied::Unsupported(Unsupported::Camera));
        match ctl {
            Control::Mirror(m) => self.set_mirror(m),
            Control::FrameRate(fps) => {
                if !(fps.is_finite() && fps > 0.0) {
                    return Err(Error::new(
                        ErrorKind::InvalidConfig,
                        format!("frame rate {fps} must be positive"),
                    ));
                }
                let Some(info) = self.set.get(ControlId::FrameRate) else {
                    return unsupported;
                };
                let got = clamp(fps, Some(info));
                let us = (1e6 / got).round() as i64;
                self.pending.frame_duration_min_us = us;
                self.pending.frame_duration_max_us = us;
                self.asked_fps = Some(fps);
                if !self.streaming {
                    self.config.frame_rate = Some(got);
                }
                Ok(applied(fps, got, ControlValue::Float(got)))
            }
            Control::Exposure(e) => {
                let Some(info) = self.set.get(ControlId::Exposure).copied() else {
                    return unsupported;
                };
                match e {
                    Exposure::Auto => {
                        self.pending.exposure_mode = sys::EFCAM_LC_MODE_AUTO;
                        Ok(Applied::Driver)
                    }
                    Exposure::Manual(d) => {
                        let asked = d.as_micros() as f64;
                        let got = clamp(asked, Some(&info)).round();
                        self.pending.exposure_mode = sys::EFCAM_LC_MODE_MANUAL;
                        self.pending.exposure_time_us = got as i32;
                        Ok(applied(asked, got, ControlValue::Int(got as i64)))
                    }
                }
            }
            Control::Gain(g) => {
                let Some(info) = self.set.get(ControlId::Gain).copied() else {
                    return unsupported;
                };
                match g {
                    Gain::Auto => {
                        self.pending.gain_mode = sys::EFCAM_LC_MODE_AUTO;
                        Ok(Applied::Driver)
                    }
                    Gain::Manual(v) => {
                        let got = clamp(f64::from(v), Some(&info));
                        self.pending.gain_mode = sys::EFCAM_LC_MODE_MANUAL;
                        self.pending.analogue_gain = got as f32;
                        Ok(applied(f64::from(v), got, ControlValue::Float(got)))
                    }
                }
            }
            Control::WhiteBalance(wb) => match wb {
                WhiteBalance::Auto => {
                    if self
                        .handle
                        .control_info(sys::EFCAM_LC_CONTROL_AWB_ENABLE)
                        .is_none()
                    {
                        return unsupported;
                    }
                    self.pending.awb_enable = 1;
                    Ok(Applied::Driver)
                }
                WhiteBalance::Manual { kelvin } => {
                    // A pipeline without a settable colour temperature
                    // takes gains only, which need a calibration this
                    // backend does not have.
                    let Some(info) = self.set.get(ControlId::WhiteBalance).copied() else {
                        return unsupported;
                    };
                    let got = clamp(f64::from(kelvin), Some(&info)).round();
                    self.pending.awb_enable = 0;
                    self.pending.colour_temperature = got as i32;
                    Ok(applied(
                        f64::from(kelvin),
                        got,
                        ControlValue::Int(got as i64),
                    ))
                }
            },
            _ => Ok(Applied::Unsupported(Unsupported::Backend)),
        }
    }

    fn start(&mut self) -> Result<()> {
        if self.streaming {
            return Ok(());
        }
        if self.provided.is_some() {
            return Err(Error::buffers_rejected(
                Rejection::Driver {
                    errno: libc::EOPNOTSUPP,
                },
                None,
            )
            .with_source(Error::new(
                ErrorKind::BuffersRejected,
                "the libcamera backend captures into libcamera's own buffers",
            )));
        }
        let _span = tracing::debug_span!("camera.libcamera.start", id = %self.id()).entered();
        self.setup()?;
        if let Err(e) = self.handle.start(&self.pending) {
            let _ = self.halt();
            return Err(e);
        }
        self.pending = sys::efcam_lc_controls::unset();
        self.streaming = true;
        // The rate is measured from the first two frames, which libcamera's
        // frame-duration metadata does not always match (the i.MX 95 neo
        // reports 15.97 ms at 60 fps).
        let first = match self.first_frame() {
            Ok(c) => c,
            Err(e) => {
                let _ = self.halt();
                return Err(e);
            }
        };
        self.note_rate(&first);
        self.early.push_back(first);
        if self.config.buffer_count > 1 {
            if let Ok(second) = self.first_frame() {
                let interval = second.sensor_timestamp_ns - first.sensor_timestamp_ns;
                if first.sensor_timestamp_ns >= 0 && interval > 0 {
                    self.config.frame_rate = Some(round_fps(1e9 / interval as f64));
                }
                self.early.push_back(second);
            }
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if !self.streaming {
            return Ok(());
        }
        self.halt()
    }

    fn set_buffers(&mut self, pool: BufferPool) -> Result<()> {
        if self.streaming {
            return Err(Error::new(
                ErrorKind::InvalidConfig,
                "set_buffers while streaming",
            ));
        }
        self.provided = match pool {
            BufferPool::Sdk => None,
            BufferPool::Provided(p) => Some(p),
        };
        Ok(())
    }

    fn take_buffers(&mut self) -> Vec<TensorDyn> {
        if self.streaming {
            return Vec::new();
        }
        self.provided.take().unwrap_or_default()
    }

    fn set_contiguity(&mut self, contiguity: Contiguity) -> Result<()> {
        if self.streaming {
            return Err(Error::new(
                ErrorKind::InvalidConfig,
                "set_contiguity while streaming",
            ));
        }
        // libcamera allocates its own buffers; the requirement applies to
        // SDK allocations only.
        self.request.contiguity = contiguity;
        Ok(())
    }

    fn next_frame(&mut self, timeout: Option<Duration>) -> Result<Frame> {
        if !self.streaming {
            return Err(Error::new(
                ErrorKind::InvalidConfig,
                "next_frame while stopped",
            ));
        }
        self.requeue_released()?;
        if let Some(c) = self.early.pop_front() {
            return Ok(self.frame(c));
        }
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            self.requeue_released()?;
            while let Some(c) = self.handle.dequeue() {
                match c.status {
                    sys::EFCAM_LC_STATUS_OK => return Ok(self.frame(c)),
                    sys::EFCAM_LC_STATUS_ERROR => self.stats.errors += 1,
                    _ => {}
                }
                if let Some(pool) = &self.pool {
                    pool.table.recycle(c.slot as usize);
                }
                self.requeue_released()?;
            }
            let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            let starved = self
                .pool
                .as_ref()
                .is_some_and(|p| p.table.held() == p.table.len());
            let slice = if starved {
                Some(left.map_or(STARVED_POLL, |l| l.min(STARVED_POLL)))
            } else {
                left
            };
            if !self.handle.wait(slice)? && deadline.is_some_and(|d| Instant::now() >= d) {
                self.stats.timeouts += 1;
                return Err(Error::new(
                    ErrorKind::Timeout,
                    format!("no frame within {timeout:?}"),
                ));
            }
        }
    }

    fn stats(&self) -> CaptureStats {
        let mut s = self.stats;
        if let Some(pool) = &self.pool {
            s.held = pool.table.held();
            s.queued = pool.table.len() - s.held;
        } else {
            s.queued = 0;
            s.held = 0;
        }
        s
    }

    fn wait_handle(&self) -> Option<WaitHandle> {
        Some(WaitHandle::new(self.handle.event_fd()))
    }
}

#[cfg(test)]
mod tests;

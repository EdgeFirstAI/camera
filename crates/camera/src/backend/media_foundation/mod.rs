// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Media Foundation capture backend (feature `media-foundation`, Windows).
//!
//! - **Capture** is an `IMFSourceReader` in asynchronous mode. Its callback
//!   only queues each sample and requests the next, so `next_frame` waits
//!   on the queue with the caller's timeout.
//! - **Buffers** are the SDK's own or the caller's (Import): D3D11 tensors
//!   on the tensor crate's device by default. Media Foundation's own
//!   textures cannot be wrapped: they are in DXGI's NV12 and YUY2 formats,
//!   which D3D11 tensors do not describe, they lack the NT-handle sharing
//!   the tensors need, and its pool holds two or three samples. Each frame
//!   is copied from the sample in system memory and uploaded by
//!   `next_frame`, on the caller's thread. Uploading from the callback
//!   instead, while another thread read frames back, held Media Foundation's
//!   callbacks back for 5 s at a time.
//! - **Modes** are the camera's native media types; format, size and rate
//!   pick one ([`select`]). Each delivers one format, repacked without colour
//!   conversion (I420 as NV12, UYVY as YUYV, RGB24 as BGRA). Media
//!   Foundation's video processor is not used: it re-stamps samples.
//!   Compressed types (MJPEG, H.264) are not captured.
//! - **Timestamps** are the sample times, which are `QueryPerformanceCounter`
//!   in 100 ns units, taken as the frame arrives: `CaptureClock::Qpc`,
//!   `TimestampSource::EndOfFrame`.
//! - **Lost frames:** stream ticks and discontinuities are logged, not
//!   counted. A sample that arrives while as many are queued as the pool
//!   holds is dropped, as a driver drops a frame with no free buffer.
//! - **`stop()`** flushes the reader and keeps the pool, so frames held
//!   across a restart stay valid and requeue when they drop. Closing
//!   detaches the pool: held frames keep their memory, and nothing writes
//!   into it again.

mod com;
mod controls;
mod copy;
mod describe;
mod select;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use edgefirst_tensor::{CpuAccess, DType, PixelFormat, TensorDyn, TensorMemory};
use windows::core::{Interface, Ref, HRESULT};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFMediaEvent, IMFMediaSource, IMFMediaType, IMFSample, IMFSourceReader,
    IMFSourceReaderCallback, IMFSourceReaderCallback_Impl, MF2DBuffer_LockFlags_Read,
    MFCreateAttributes, MFCreateSourceReaderFromMediaSource, MFSampleExtension_Discontinuity,
    MF_MT_DEFAULT_STRIDE, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_SOURCE_READERF_STREAMTICK, MF_SOURCE_READER_ASYNC_CALLBACK,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};
use windows_core::implement;

use self::com::{com, error, Agile, Platform};
use self::controls::Controls;
use self::select::{NativeType, Packing};
use super::rate::RateMeter;
use crate::builder::Source;
use crate::pool::{self, BufferInfo, PoolRequirements};
use crate::{
    Applied, BufferPool, Camera, CameraBuilder, CameraDescriptor, CaptureClock, CaptureStats,
    Contiguity, Control, ControlId, ControlInfo, ControlSet, ControlValue, Error, ErrorKind,
    FormatInfo, Frame, FrameMeta, MemoryStrategy, Mirror, PlaneLayout, RealtimeClock,
    ResolvedMemory, Result, SlotTable, StreamConfig, StreamRequest, Timestamp, TimestampSource,
    Unsupported, WaitHandle,
};

/// The stream every call addresses.
pub(super) const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
/// How long `start()` waits for the first frame before `NotReady`.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `stop()` waits for the reader to finish flushing.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// Poll slice while every buffer is held, so a slot released on another
/// thread is taken promptly.
const STARVED_POLL: Duration = Duration::from_millis(20);
/// Memory frames are copied into: D3D11 textures, or host memory.
const NATIVE: &[TensorMemory] = &[TensorMemory::DmaBuf, TensorMemory::Mem];

pub(crate) fn enumerate() -> Result<Vec<CameraDescriptor>> {
    let _platform = match Platform::start() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("{e}");
            return Ok(Vec::new());
        }
    };
    Ok(describe::devices()?
        .into_iter()
        .map(|device| {
            let mut d = device.descriptor.clone();
            match device.formats() {
                Ok(formats) => d.formats = formats,
                Err(e) => tracing::warn!("{}: {e}", d.name),
            }
            d
        })
        .collect())
}

pub(crate) fn probe(d: &CameraDescriptor) -> Result<Vec<FormatInfo>> {
    let _platform = Platform::start()?;
    describe::find(&d.id)?.formats()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every update under these locks is a single assignment or queue
    // operation, so a panic leaves them consistent.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the reader's callback has delivered and `next_frame` has not taken.
#[derive(Debug, Default)]
struct Queue {
    /// Samples with their times, in 100 ns, oldest first.
    samples: VecDeque<(Agile<IMFSample>, i64)>,
    /// The stream failed; no sample follows until a restart.
    failed: Option<Error>,
    /// A flush completed.
    flushed: bool,
}

/// State the camera shares with the reader's callback.
#[derive(Debug)]
struct Shared {
    streaming: AtomicBool,
    /// Cleared on close, so the reader and its callback can be released.
    reader: Mutex<Option<Agile<IMFSourceReader>>>,
    queue: Mutex<Queue>,
    ready: Condvar,
    /// The most samples queued: the pool depth.
    depth: AtomicUsize,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut Queue)) {
        f(&mut lock(&self.queue));
        self.ready.notify_all();
    }

    /// Asks for the next sample; it arrives in the callback.
    fn request(&self) {
        let reader = lock(&self.reader).clone();
        if let Some(r) = reader {
            // SAFETY: a live reader in asynchronous mode, where every
            // out-parameter is null.
            if let Err(e) = unsafe { r.ReadSample(STREAM, 0, None, None, None, None) } {
                self.update(|q| q.failed = Some(error(e, "request a frame")));
            }
        }
    }

    fn sample(&self, status: HRESULT, flags: u32, time: i64, sample: Option<&IMFSample>) {
        if let Err(e) = status.ok() {
            self.update(|q| q.failed = Some(error(e, "capture a frame")));
            return;
        }
        let flags = flags as i32;
        if flags & MF_SOURCE_READERF_ERROR.0 != 0 {
            self.update(|q| {
                q.failed = Some(Error::new(ErrorKind::Backend, "the camera stream failed"))
            });
            return;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 != 0 {
            self.update(|q| {
                q.failed = Some(Error::new(
                    ErrorKind::Disconnected,
                    "the camera stream ended",
                ))
            });
            return;
        }
        if flags & MF_SOURCE_READERF_STREAMTICK.0 != 0 {
            tracing::debug!("mediafoundation: a gap in the stream at {time}");
        }
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 != 0 {
            tracing::warn!("mediafoundation: the camera changed its media type");
        }
        if let Some(sample) = sample {
            // SAFETY: a live sample.
            if unsafe { sample.GetUINT32(&MFSampleExtension_Discontinuity) }.is_ok_and(|d| d != 0) {
                tracing::debug!("mediafoundation: a discontinuity before the frame at {time}");
            }
            let depth = self.depth.load(Ordering::Acquire);
            self.update(|q| {
                if q.samples.len() < depth {
                    q.samples.push_back((Agile(sample.clone()), time));
                } else {
                    tracing::trace!("mediafoundation: {depth} frames are waiting; a frame is lost");
                }
            });
        }
        if self.streaming.load(Ordering::Acquire) {
            self.request();
        }
    }
}

#[implement(IMFSourceReaderCallback)]
struct Callback(Arc<Shared>);

impl IMFSourceReaderCallback_Impl for Callback_Impl {
    fn OnReadSample(
        &self,
        status: HRESULT,
        _stream: u32,
        flags: u32,
        time: i64,
        sample: Ref<'_, IMFSample>,
    ) -> windows::core::Result<()> {
        self.0.sample(status, flags, time, sample.as_ref());
        Ok(())
    }

    fn OnFlush(&self, _stream: u32) -> windows::core::Result<()> {
        self.0.update(|q| q.flushed = true);
        Ok(())
    }

    fn OnEvent(&self, _stream: u32, _event: Ref<'_, IMFMediaEvent>) -> windows::core::Result<()> {
        Ok(())
    }
}

/// How a sample's image is laid out.
#[derive(Debug, Clone, Copy)]
struct Layout {
    packing: Packing,
    width: usize,
    height: usize,
    /// Row pitch of a sample whose buffer does not report one.
    pitch: isize,
}

/// Copies `sample` into `tensor`.
fn copy_sample(sample: &IMFSample, tensor: &TensorDyn, layout: Layout) -> Result<()> {
    // SAFETY: a live sample.
    let buffer =
        unsafe { sample.ConvertToContiguousBuffer() }.map_err(|e| error(e, "read a frame"))?;
    let stride = tensor
        .effective_row_stride()
        .ok_or_else(|| Error::new(ErrorKind::Backend, "a capture buffer has no row stride"))?;
    let mut map = tensor.map_bytes(CpuAccess::Write)?;
    let into = |rows: copy::Rows<'_>, map: &mut [u8]| {
        copy::copy(
            layout.packing,
            layout.width,
            layout.height,
            rows,
            map,
            stride,
        )
        .map_err(|m| Error::new(ErrorKind::Backend, m))
    };
    if let Ok(b) = buffer.cast::<IMF2DBuffer2>() {
        let (mut scan0, mut pitch) = (std::ptr::null_mut(), 0);
        let (mut start, mut len) = (std::ptr::null_mut(), 0);
        // SAFETY: valid out-pointers; unlocked below.
        unsafe {
            b.Lock2DSize(
                MF2DBuffer_LockFlags_Read,
                &mut scan0,
                &mut pitch,
                &mut start,
                &mut len,
            )
        }
        .map_err(|e| error(e, "lock a frame"))?;
        // SAFETY: the buffer is locked and `len` bytes long from `start`.
        let bytes = unsafe { std::slice::from_raw_parts(start, len as usize) };
        let rows = copy::Rows {
            bytes,
            top: (scan0 as usize).wrapping_sub(start as usize),
            pitch: pitch as isize,
        };
        let copied = into(rows, &mut map);
        // SAFETY: locked above.
        let _ = unsafe { b.Unlock2D() };
        return copied;
    }
    let (mut p, mut len) = (std::ptr::null_mut(), 0);
    // SAFETY: valid out-pointers; unlocked below.
    unsafe { buffer.Lock(&mut p, None, Some(&mut len)) }.map_err(|e| error(e, "lock a frame"))?;
    // SAFETY: the buffer is locked and holds `len` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(p, len as usize) };
    let top = if layout.pitch < 0 {
        layout.height.saturating_sub(1) * layout.pitch.unsigned_abs()
    } else {
        0
    };
    let rows = copy::Rows {
        bytes,
        top,
        pitch: layout.pitch,
    };
    let copied = into(rows, &mut map);
    // SAFETY: locked above.
    let _ = unsafe { buffer.Unlock() };
    copied
}

#[derive(Debug)]
struct MediaFoundationCamera {
    descriptor: CameraDescriptor,
    request: StreamRequest,
    config: StreamConfig,
    set: ControlSet,
    controls: Controls,
    /// The camera's native types, by index.
    types: Vec<NativeType>,
    media_types: Vec<Agile<IMFMediaType>>,
    /// The native type the camera is set to.
    native: Option<NativeType>,
    packing: Packing,
    /// Row pitch of samples whose buffer does not report one.
    pitch: isize,
    provided: Option<Vec<TensorDyn>>,
    table: Option<SlotTable>,
    /// Queued slots not yet filled.
    free: VecDeque<usize>,
    shared: Arc<Shared>,
    streaming: bool,
    clock: RealtimeClock,
    seq: u64,
    stats: CaptureStats,
    rate: RateMeter,
    reader: Agile<IMFSourceReader>,
    source: Agile<IMFMediaSource>,
    // Declared last: the platform stays started until the rest is released.
    _platform: Platform,
}

pub(crate) fn open(builder: CameraBuilder) -> Result<Box<dyn Camera>> {
    let Source::MediaFoundation(id) = &builder.source else {
        return Err(Error::new(
            ErrorKind::InvalidConfig,
            "not a Media Foundation source",
        ));
    };
    let request = builder.request;
    if builder.provided.is_none() {
        crate::config::check_pool_depth(request.buffers)?;
    }
    if request.memory == MemoryStrategy::Export {
        return Err(Error::new(
            ErrorKind::InvalidConfig,
            "the Media Foundation backend copies each frame into SDK or caller buffers; \
             use Import or Auto",
        ));
    }
    let _span = tracing::debug_span!("camera.mediafoundation.open", id = %id).entered();
    let platform = Platform::start()?;
    let device = describe::find(id)?;
    let source = device.activate()?;
    let name = device.descriptor.name.clone();

    let shared = Arc::new(Shared {
        streaming: AtomicBool::new(false),
        reader: Mutex::new(None),
        queue: Mutex::new(Queue::default()),
        ready: Condvar::new(),
        depth: AtomicUsize::new(request.buffers),
    });
    let callback: IMFSourceReaderCallback = Callback(shared.clone()).into();
    let mut attributes = None;
    // SAFETY: a valid out-pointer.
    unsafe { MFCreateAttributes(&mut attributes, 1) }
        .map_err(|e| error(e, "create reader attributes"))?;
    let attributes = attributes.ok_or_else(|| {
        Error::new(
            ErrorKind::Backend,
            "Media Foundation returned no attributes",
        )
    })?;
    // SAFETY: a live attribute store and callback.
    unsafe { attributes.SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback)? };
    // SAFETY: a live media source and attribute store.
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &attributes) }
        .map_err(|e| error(e, &format!("open {name}")))?;
    *lock(&shared.reader) = Some(Agile(reader.clone()));

    // From here the camera's drop releases the reader and the source.
    let mut camera = MediaFoundationCamera {
        descriptor: device.descriptor,
        controls: Controls::of(&source),
        config: StreamConfig::new(0, 0, PixelFormat::Nv12, 0),
        set: ControlSet::new(),
        types: Vec::new(),
        media_types: Vec::new(),
        native: None,
        packing: Packing::Nv12,
        pitch: 0,
        provided: builder.provided,
        table: None,
        free: VecDeque::new(),
        shared,
        streaming: false,
        clock: RealtimeClock::new(),
        seq: 0,
        stats: CaptureStats::default(),
        rate: RateMeter::default(),
        reader: Agile(reader),
        source: Agile(source),
        _platform: platform,
        request,
    };
    let natives = describe::native_types(&camera.reader)?;
    // SAFETY: a live reader.
    let current = unsafe { camera.reader.GetCurrentMediaType(STREAM) }
        .ok()
        .map(|t| describe::native_type(u32::MAX, &t));
    (camera.types, camera.media_types) = natives.into_iter().map(|(t, m)| (t, Agile(m))).unzip();
    camera.descriptor.formats = select::formats(&camera.types);
    let current = current.and_then(|c| {
        camera
            .types
            .iter()
            .find(|t| (t.subtype, t.size(), t.rate) == (c.subtype, c.size(), c.rate))
            .copied()
    });
    let choice = select::choose(
        &camera.types,
        current.as_ref(),
        camera.request.format,
        camera.request.size,
        camera.request.frame_rate,
    )?;
    camera.configure(choice)?;
    if let Some(m) = camera.request.mirror.filter(|m| *m != Mirror::None) {
        tracing::warn!("{name}: Media Foundation cannot mirror {m:?}");
    }
    for control in builder.controls {
        if let Applied::Unsupported(why) = camera.set_control(control)? {
            tracing::warn!("{name}: control {:?} unsupported ({why:?})", control.id());
        }
    }
    Ok(Box::new(camera))
}

impl MediaFoundationCamera {
    fn name(&self) -> &str {
        &self.descriptor.name
    }

    fn layout(&self) -> Layout {
        Layout {
            packing: self.packing,
            width: self.config.width as usize,
            height: self.config.height as usize,
            pitch: self.pitch,
        }
    }

    /// Sets the camera to native type `choice` and reads back what was configured. A
    /// new size or format retires the pool.
    fn configure(&mut self, choice: NativeType) -> Result<()> {
        let native = &self.media_types[choice.index as usize];
        // SAFETY: a live reader and media type.
        unsafe { self.reader.SetCurrentMediaType(STREAM, None, &native.0) }
            .map_err(|e| error(e, "set the camera's format"))?;
        // SAFETY: a live reader.
        let current = unsafe { self.reader.GetCurrentMediaType(STREAM) }
            .map_err(|e| error(e, "read the camera's format"))?;
        let got = describe::native_type(choice.index, &current);
        let Some(packing) = got.packing() else {
            return Err(Error::new(
                ErrorKind::UnsupportedFormat,
                format!("{}: the camera did not take its own format", self.name()),
            ));
        };
        let format = packing.delivers();
        let natural = match packing {
            Packing::Yuy2 | Packing::Uyvy => 2,
            Packing::Rgb24 => 3,
            Packing::Rgb32 | Packing::Argb32 => 4,
            Packing::Nv12 | Packing::I420 | Packing::Yv12 => 1,
        } * got.width as isize;
        // SAFETY: a live media type; the stride is a signed value.
        self.pitch = unsafe { current.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
            .map_or(natural, |s| s as i32 as isize);
        if let Some((w, h)) = self.request.size.filter(|&s| s != got.size()) {
            tracing::info!(
                "{}: {w}x{h} is not offered; capturing {}x{}",
                self.name(),
                got.width,
                got.height
            );
        }
        if !packing.is_identity() {
            tracing::info!("{}: repacking {packing:?} as {format:?}", self.name());
        }
        let changed = (self.config.width, self.config.height, self.config.format)
            != (got.width, got.height, format);
        let mut config = StreamConfig::new(got.width, got.height, format, 0);
        config.frame_rate = got.fps();
        config.colorimetry = describe::colorimetry(&current);
        config.buffer_count = self.request.buffers;
        config.memory = ResolvedMemory::Import;
        if changed {
            self.retire();
        } else {
            config.row_stride = self.config.row_stride;
            config.planes = self.config.planes;
            config.buffer_count = self.config.buffer_count;
        }
        self.config = config;
        self.native = Some(got);
        self.packing = packing;
        self.describe_controls();
        Ok(())
    }

    fn describe_controls(&mut self) {
        let mut set = ControlSet::new();
        if let Some(native) = self.native {
            let rates = select::rates_at(&self.types, self.config.format, native.size());
            if let (Some(&max), Some(&min)) = (rates.first(), rates.last()) {
                let f = ControlValue::Float;
                let current = self.config.frame_rate.unwrap_or(max);
                set.insert(
                    ControlId::FrameRate,
                    ControlInfo::new(f(min), f(max), f(0.0), f(current)),
                );
            }
        }
        self.controls.describe(&mut set);
        self.set = set;
    }

    /// Detaches the pool: held frames keep their memory, and nothing fills
    /// it again.
    fn retire(&mut self) {
        if let Some(table) = self.table.take() {
            table.detach();
        }
        self.free.clear();
    }

    /// The SDK's D3D11 pool, host memory when no D3D11 device is available,
    /// or the caller's pool once it is validated.
    fn pool(&mut self) -> Result<SlotTable> {
        let (w, h, format) = (
            self.config.width as usize,
            self.config.height as usize,
            self.config.format,
        );
        if let Some(pool) = self.provided.take() {
            let infos: Vec<_> = pool.iter().map(BufferInfo::of).collect();
            let req = PoolRequirements {
                format,
                width: w,
                height: h,
                row_stride: None,
                min_count: *crate::POOL_DEPTH.start(),
                max_count: *crate::POOL_DEPTH.end(),
                // Frames are copied in, so any memory the CPU can write
                // will do.
                contiguity: Contiguity::Any,
                native: NATIVE,
            };
            if let Err((reason, slot)) = pool::validate(&infos, &req) {
                self.provided = Some(pool);
                return Err(Error::buffers_rejected(reason, slot));
            }
            return Ok(SlotTable::new(pool));
        }
        let allocate = |memory| {
            (0..self.request.buffers)
                .map(|_| {
                    TensorDyn::image(w, h, format, DType::U8, Some(memory), CpuAccess::ReadWrite)
                })
                .collect::<std::result::Result<Vec<_>, _>>()
        };
        let tensors = match allocate(TensorMemory::DmaBuf) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    "{}: no D3D11 textures ({e}); capturing into host memory",
                    self.name()
                );
                allocate(TensorMemory::Mem)?
            }
        };
        Ok(SlotTable::new(tensors))
    }

    /// Stops the reader and forgets the samples it delivered; keeps the
    /// pool.
    fn halt(&mut self) -> Result<()> {
        self.streaming = false;
        self.shared.streaming.store(false, Ordering::Release);
        self.rate.restart();
        // SAFETY: a live reader; the flush completes in the callback.
        let flushed = unsafe { self.reader.Flush(STREAM) }
            .map_err(|e| error(e, "stop the camera"))
            .and_then(|()| {
                let q = lock(&self.shared.queue);
                let (mut q, wait) = self
                    .shared
                    .ready
                    .wait_timeout_while(q, FLUSH_TIMEOUT, |q| !q.flushed)
                    .unwrap_or_else(PoisonError::into_inner);
                *q = Queue::default();
                if wait.timed_out() {
                    return Err(Error::new(
                        ErrorKind::Backend,
                        format!(
                            "{}: the camera did not stop within {FLUSH_TIMEOUT:?}",
                            self.name()
                        ),
                    ));
                }
                Ok(())
            });
        flushed
    }

    /// A slot no frame holds, if there is one.
    fn take_slot(&mut self) -> Option<usize> {
        self.free
            .pop_front()
            .or_else(|| self.table.as_ref()?.pop_released())
    }

    fn frame(&mut self, slot: usize, time: i64) -> Option<Frame> {
        let timestamp = Timestamp {
            clock: CaptureClock::Qpc,
            source: TimestampSource::EndOfFrame,
            nanos: time.saturating_mul(100),
        };
        self.rate.frame(timestamp.nanos);
        let mut meta = FrameMeta::new(slot, self.seq, timestamp);
        self.seq += 1;
        meta.realtime = self.clock.to_realtime(timestamp).ok();
        let table = self.table.as_ref()?;
        meta.planes = planes(&self.config, table.tensor(slot));
        meta.bytes_used = meta.planes.iter().map(|p| p.used).sum();
        self.stats.frames += 1;
        let _span =
            tracing::trace_span!("camera.mediafoundation.dequeue", seq = meta.seq, slot).entered();
        Some(table.frame(meta))
    }

    fn set_frame_rate(&mut self, fps: f64) -> Result<Applied> {
        if !(fps.is_finite() && fps > 0.0) {
            return Err(Error::new(
                ErrorKind::InvalidConfig,
                format!("frame rate {fps} must be positive"),
            ));
        }
        let Some(native) = self.native else {
            return Ok(Applied::Unsupported(Unsupported::Camera));
        };
        if self.streaming {
            return Err(Error::new(
                ErrorKind::InvalidConfig,
                "Media Foundation sets the frame rate with the format; stop the camera first",
            ));
        }
        let next = select::choose(
            &self.types,
            Some(&native),
            Some(self.config.format),
            Some(native.size()),
            Some(fps),
        )?;
        self.configure(next)?;
        self.rate.restart();
        let got = self.config.frame_rate.unwrap_or(fps);
        Ok(if (got - fps).abs() < 0.005 {
            Applied::Driver
        } else {
            Applied::Clamped(ControlValue::Float(got))
        })
    }
}

fn planes(config: &StreamConfig, tensor: &TensorDyn) -> Vec<PlaneLayout> {
    let stride = tensor.effective_row_stride().unwrap_or(0);
    config
        .format
        .plane_table(config.width as usize, config.height as usize, stride)
        .unwrap_or_default()
        .into_iter()
        .map(|p| PlaneLayout::new(-1, p.offset as usize, p.stride as usize, p.size as usize))
        .collect()
}

impl Drop for MediaFoundationCamera {
    fn drop(&mut self) {
        com();
        if self.streaming {
            let _ = self.halt();
        }
        self.retire();
        lock(&self.shared.reader).take();
        *lock(&self.shared.queue) = Queue::default();
        // SAFETY: a live source, unused after this.
        let _ = unsafe { self.source.Shutdown() };
    }
}

impl Camera for MediaFoundationCamera {
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
        com();
        let unsupported = || {
            Error::new(
                ErrorKind::InvalidConfig,
                format!("{}: {id:?} is not available", self.name()),
            )
        };
        if !self.set.contains(id) {
            return Err(unsupported());
        }
        match id {
            ControlId::FrameRate => self
                .config
                .frame_rate
                .map(ControlValue::Float)
                .ok_or_else(unsupported),
            _ => self
                .controls
                .value(id)
                .unwrap_or_else(|| Err(unsupported())),
        }
    }

    fn set_control(&mut self, ctl: Control) -> Result<Applied> {
        com();
        match ctl {
            Control::FrameRate(fps) => self.set_frame_rate(fps),
            Control::Exposure(e) => self.controls.exposure(e),
            Control::Gain(g) => self.controls.gain(g),
            Control::WhiteBalance(wb) => self.controls.white_balance(wb),
            Control::Mirror(Mirror::None) => Ok(Applied::Driver),
            _ => Ok(Applied::Unsupported(Unsupported::Backend)),
        }
    }

    fn start(&mut self) -> Result<()> {
        if self.streaming {
            return Ok(());
        }
        com();
        let _span = tracing::debug_span!("camera.mediafoundation.start", id = %self.descriptor.id)
            .entered();
        if self.table.is_none() {
            let table = self.pool()?;
            self.free = (0..table.len()).collect();
            self.table = Some(table);
        }
        let Some(table) = &self.table else {
            return Err(Error::new(ErrorKind::InvalidConfig, "no buffers"));
        };
        self.config.row_stride = table.tensor(0).effective_row_stride().unwrap_or(0);
        self.config.planes = planes(&self.config, table.tensor(0)).len();
        self.config.buffer_count = table.len();
        self.shared.depth.store(table.len(), Ordering::Release);
        *lock(&self.shared.queue) = Queue::default();
        self.rate.restart();
        self.streaming = true;
        self.shared.streaming.store(true, Ordering::Release);
        self.shared.request();
        let q = lock(&self.shared.queue);
        let (mut q, wait) = self
            .shared
            .ready
            .wait_timeout_while(q, FIRST_FRAME_TIMEOUT, |q| {
                q.samples.is_empty() && q.failed.is_none()
            })
            .unwrap_or_else(PoisonError::into_inner);
        let failed = q.failed.take();
        drop(q);
        let error = match (failed, wait.timed_out()) {
            (Some(e), _) => e,
            (None, true) => Error::new(
                ErrorKind::NotReady,
                format!(
                    "{}: no frame within {FIRST_FRAME_TIMEOUT:?} of starting",
                    self.name()
                ),
            ),
            (None, false) => return Ok(()),
        };
        let _ = self.halt();
        Err(error)
    }

    fn stop(&mut self) -> Result<()> {
        if !self.streaming {
            return Ok(());
        }
        com();
        self.halt()
    }

    fn set_buffers(&mut self, pool: BufferPool) -> Result<()> {
        if self.streaming {
            return Err(Error::new(
                ErrorKind::InvalidConfig,
                "set_buffers while streaming",
            ));
        }
        self.retire();
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
        // Frames are copied in; contiguity has no meaning here.
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
        let shared = self.shared.clone();
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut slot = None;
        let result = loop {
            if slot.is_none() {
                slot = self.take_slot();
            }
            let mut q = lock(&shared.queue);
            if let Some(e) = q.failed.take() {
                break Err(e);
            }
            if let Some(s) = slot {
                if let Some((sample, time)) = q.samples.pop_front() {
                    drop(q);
                    let copied = match &self.table {
                        Some(table) => copy_sample(&sample, table.tensor(s), self.layout()),
                        None => Err(Error::new(ErrorKind::InvalidConfig, "no buffers")),
                    };
                    match copied {
                        Ok(()) => {
                            slot = None;
                            match self.frame(s, time) {
                                Some(frame) => break Ok(frame),
                                None => {
                                    break Err(Error::new(ErrorKind::InvalidConfig, "no buffers"))
                                }
                            }
                        }
                        Err(e) => {
                            self.stats.errors += 1;
                            tracing::warn!("{}: {e}", self.name());
                            continue;
                        }
                    }
                }
            }
            let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if left.is_some_and(|l| l.is_zero()) {
                self.stats.timeouts += 1;
                break Err(Error::new(
                    ErrorKind::Timeout,
                    format!("no frame within {timeout:?}"),
                ));
            }
            // Every buffer held: wake to look for a released one.
            let slice = slot.is_none().then_some(STARVED_POLL);
            let wait = match (left, slice) {
                (Some(l), Some(s)) => Some(l.min(s)),
                (l, s) => l.or(s),
            };
            drop(match wait {
                Some(w) => {
                    shared
                        .ready
                        .wait_timeout(q, w)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => shared.ready.wait(q).unwrap_or_else(PoisonError::into_inner),
            });
        };
        if let Some(s) = slot {
            self.free.push_front(s);
        }
        result
    }

    fn measured_frame_rate(&self) -> Option<f64> {
        self.rate.measured()
    }

    fn restart_frame_rate_measurement(&mut self) {
        self.rate.restart();
    }

    fn stats(&self) -> CaptureStats {
        let mut s = self.stats;
        if let Some(table) = &self.table {
            s.held = table.held();
            s.queued = table.len() - s.held;
        } else {
            s.queued = 0;
            s.held = 0;
        }
        s
    }

    fn wait_handle(&self) -> Option<WaitHandle> {
        None
    }
}

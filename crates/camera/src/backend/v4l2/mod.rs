// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! V4L2 capture backend.
//!
//! Buffers are imported or exported per [`MemoryStrategy`]:
//!
//! - **Import:** the SDK (or the caller) allocates DMA-BUF tensors and the
//!   driver writes into them (`V4L2_MEMORY_DMABUF`). SDK pools are
//!   contiguous unless [`Contiguity::Any`], and allocated at the pitch and
//!   size the driver returned from `S_FMT`.
//! - **Export:** the driver allocates (`V4L2_MEMORY_MMAP`) and each buffer
//!   is exported with `EXPBUF` and wrapped as a tensor.
//! - **Auto:** Import with the SDK's pool, probed at `start()` by queueing
//!   every slot and running to the first frame; Export when the driver
//!   refuses. Caller pools never fall back.
//!
//! `stop()` is `STREAMOFF` then `REQBUFS(0)`: on the i.MX 8M Plus ISP a bare
//! `STREAMOFF`/`STREAMON` delivers no frames and the next close crashes
//! `isp_media_server`. Import keeps its tensors across a stop, so held
//! frames requeue when they drop; Export's buffers are orphaned by
//! `REQBUFS(0)`, so its frames are detached and keep their memory.

mod controls;
mod device;
mod format;
mod negotiate;
mod quirks;

use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};

use edgefirst_tensor::{
    Contiguity as Heap, CpuAccess, DType, ImageDesc, PixelFormat, Tensor, TensorDyn, TensorMemory,
    TensorTrait,
};
use edgefirst_v4l2::device::Device;
use edgefirst_v4l2::queue::{self as vq, BufType, Dequeued, Memory, Plane, Queue};
use edgefirst_v4l2::uapi;

use self::controls::Controls;
use self::device::{errno, v4l2_error};
use self::negotiate::{interval_for, negotiate, Negotiated};
use crate::builder::Source;
use crate::pool::{self, BufferInfo, PoolRequirements};
use crate::{
    Applied, BufferPool, Camera, CameraBuilder, CameraDescriptor, CaptureClock, CaptureStats,
    Contiguity, Control, ControlId, ControlInfo, ControlSet, ControlValue, Error, ErrorKind, Frame,
    FrameMeta, MemoryStrategy, PlaneLayout, RealtimeClock, Rejection, ResolvedMemory, Result,
    Sizes, SlotTable, StreamConfig, StreamRequest, Timestamp, TimestampSource, WaitHandle,
};

pub(crate) use self::device::{enumerate, probe};

const MIN_BUFFERS: usize = 2;
const MAX_BUFFERS: usize = 32;
/// How long `start()` waits for the first frame: the Import probe, and the
/// `NotReady` check for an ISP that is still restarting (about 4 s on the
/// i.MX 8M Plus after `isp_media_server` restarts).
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll slice while no buffer is queued, so slots released on other
/// threads go back to the driver promptly.
const STARVED_POLL: Duration = Duration::from_millis(20);

pub(crate) fn open(builder: CameraBuilder) -> Result<Box<dyn Camera>> {
    let Source::Device(path) = &builder.source else {
        return Err(Error::new(
            ErrorKind::InvalidConfig,
            "not a V4L2 device source",
        ));
    };
    let request = builder.request;
    if builder.provided.is_none() && !(MIN_BUFFERS..=MAX_BUFFERS).contains(&request.buffers) {
        return Err(Error::new(
            ErrorKind::InvalidConfig,
            format!(
                "pool depth {} outside {MIN_BUFFERS}..={MAX_BUFFERS}",
                request.buffers
            ),
        ));
    }
    let _span = tracing::debug_span!("camera.v4l2.open", path = %path.display()).entered();
    let (dev, buf_type) = device::open(path, builder.exclusive)?;
    let descriptor = device::describe(&dev, buf_type);
    let neg = negotiate(
        &dev,
        buf_type,
        &request,
        request.memory != MemoryStrategy::Export,
    )?;
    let queue = Queue::new(&dev, buf_type).map_err(|e| v4l2_error("open capture queue", e))?;
    let ctl = Controls::query(&dev);
    let mut set = ctl.control_set();
    if let Some(info) = frame_rate_info(&descriptor, &neg) {
        set.insert(ControlId::FrameRate, info);
    }

    let mut camera = V4l2Camera {
        config: config_of(&neg, &request),
        descriptor,
        request,
        set,
        ctl,
        dev,
        buf_type,
        neg,
        queue,
        provided: builder.provided,
        pool: None,
        pending: None,
        streaming: false,
        clock: RealtimeClock::new(),
        seq: 0,
        stats: CaptureStats::default(),
        last_nanos: None,
        logged_clock: false,
        faults: builder.faults,
    };
    let mut requested = builder.controls;
    if let Some(m) = camera.request.mirror {
        requested.insert(0, Control::Mirror(m));
    }
    for control in requested {
        let applied = camera.set_control(control)?;
        if let Applied::Unsupported(why) = applied {
            tracing::warn!(
                "{}: control {:?} unsupported ({why:?})",
                camera.descriptor.id,
                control.id()
            );
        }
    }
    Ok(Box::new(camera))
}

/// The configuration negotiation produced, before buffers exist.
fn config_of(neg: &Negotiated, request: &StreamRequest) -> StreamConfig {
    let format = neg.mapping.format;
    let mut config = StreamConfig::new(neg.width, neg.height, format, neg.strides[0]);
    config.planes = format
        .plane_table(neg.width as usize, neg.height as usize, neg.strides[0])
        .map_or(1, |t| t.len());
    config.frame_rate = neg.frame_rate();
    config.colorimetry = neg.colorimetry;
    config.buffer_count = request.buffers;
    config.memory = if request.memory == MemoryStrategy::Export {
        ResolvedMemory::Export
    } else {
        ResolvedMemory::Import
    };
    config
}

/// The frame-rate range the descriptor reports for the negotiated format
/// and size.
fn frame_rate_info(d: &CameraDescriptor, neg: &Negotiated) -> Option<ControlInfo> {
    let info = d.formats.iter().find(|f| f.format == neg.mapping.format)?;
    let rates = match &info.sizes {
        Sizes::Discrete(list) => {
            &list
                .iter()
                .find(|s| (s.width, s.height) == (neg.width, neg.height))?
                .rates
        }
        Sizes::Stepwise { rates, .. } => rates,
    };
    let (lo, hi) = match rates {
        crate::Rates::Discrete(r) if !r.is_empty() => (
            r.iter().copied().fold(f64::INFINITY, f64::min),
            r.iter().copied().fold(0.0, f64::max),
        ),
        crate::Rates::Range { min, max } => (*min, *max),
        _ => return None,
    };
    let f = ControlValue::Float;
    Some(ControlInfo::new(
        f(lo),
        f(hi),
        f(0.0),
        f(neg.frame_rate().unwrap_or(hi)),
    ))
}

/// The buffers in use: their slot table, how they were obtained, and the
/// native handle of each memory plane.
#[derive(Debug)]
struct Pool {
    table: SlotTable,
    memory: ResolvedMemory,
    /// Built from the caller's tensors.
    caller: bool,
    /// DMA-BUF fd of each memory plane, per slot (-1 when none).
    handles: Vec<Vec<i64>>,
}

#[derive(Debug)]
struct V4l2Camera {
    descriptor: CameraDescriptor,
    request: StreamRequest,
    config: StreamConfig,
    set: ControlSet,
    ctl: Controls,
    // `queue` holds its own dup of the device fd; it is declared after
    // `dev` only for readability, nothing depends on the order.
    dev: Device,
    buf_type: BufType,
    neg: Negotiated,
    queue: Queue,
    /// Caller-provided tensors not attached to a pool: before the first
    /// start, after a rejection, or after `set_buffers`.
    provided: Option<Vec<TensorDyn>>,
    pool: Option<Pool>,
    /// The first frame, dequeued by `start()`, not yet returned.
    pending: Option<Dequeued>,
    streaming: bool,
    clock: RealtimeClock,
    seq: u64,
    stats: CaptureStats,
    last_nanos: Option<i64>,
    logged_clock: bool,
    /// Injected faults (debug builds only; see `CameraBuilder::fault`).
    faults: Vec<String>,
}

/// The driver refused an Import pool: which slot, if one, and why.
struct Refusal {
    slot: Option<usize>,
    errno: i32,
    error: Error,
}

impl Refusal {
    fn from_v4l2(slot: Option<usize>, what: &str, e: edgefirst_v4l2::Error) -> Self {
        Self {
            slot,
            errno: errno(&e).unwrap_or(0),
            error: v4l2_error(what, e),
        }
    }
}

/// What `start()` should do when the driver refuses an Import pool.
enum OnRefusal {
    /// Caller pool: report which buffer and why.
    Reject,
    /// SDK pool under `Auto`: fall back to Export.
    Export,
    /// SDK pool under `Import`: an error.
    Fail,
}

impl V4l2Camera {
    fn path(&self) -> String {
        self.descriptor.id.clone()
    }

    /// Whether `name` was injected with `CameraBuilder::fault`:
    /// `import-qbuf` refuses the first Import `QBUF`, `no-orphan` treats the
    /// driver as unable to orphan buffers.
    fn fault(&self, name: &str) -> bool {
        self.faults.iter().any(|f| f == name)
    }

    /// Whether the driver keeps exported buffers alive after `REQBUFS(0)`
    /// and close (`V4L2_BUF_CAP_SUPPORTS_ORPHANED_BUFS`).
    fn orphans(&self) -> bool {
        !self.fault("no-orphan") && self.queue.capabilities().supports_orphaned_bufs()
    }

    fn shape(&self) -> Result<Vec<usize>> {
        self.neg
            .mapping
            .format
            .allocation_shape(self.neg.width as usize, self.neg.height as usize)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidConfig,
                    format!(
                        "no tensor shape for {:?} {}x{}",
                        self.neg.mapping.format, self.neg.width, self.neg.height
                    ),
                )
            })
    }

    /// Allocates the SDK's Import pool at the driver's pitch and size.
    fn allocate(&self) -> Result<(Vec<TensorDyn>, Option<bool>)> {
        let (w, h) = (self.neg.width as usize, self.neg.height as usize);
        let format = self.neg.mapping.format;
        let (pitch, size) = (self.neg.strides[0], self.neg.sizes[0]);
        let required = self.request.contiguity == Contiguity::Required;
        let mut heaps = Vec::with_capacity(self.request.buffers);
        let mut pool = Vec::with_capacity(self.request.buffers);
        for _ in 0..self.request.buffers {
            let desc = ImageDesc::new(w, h, format, DType::U8)
                .with_memory(Some(TensorMemory::DmaBuf))
                .with_access(CpuAccess::ReadWrite)
                .with_contiguous(required);
            let t = TensorDyn::image_desc(&desc).map_err(|e| self.alloc_error(e, size))?;
            let fits = t.effective_row_stride() == Some(pitch) && t.capacity_bytes() >= size;
            let t = if fits {
                t
            } else {
                // The driver's pitch differs from the tensor crate's 64-byte
                // padding (vvcam returns a packed pitch): allocate a raw
                // buffer of the driver's size and describe it at its pitch.
                let rows = size.div_ceil(pitch.max(1));
                let raw = TensorDyn::image_desc(
                    &ImageDesc::new(pitch, rows, PixelFormat::Grey, DType::U8)
                        .with_memory(Some(TensorMemory::DmaBuf))
                        .with_access(CpuAccess::ReadWrite)
                        .with_contiguous(required),
                )
                .map_err(|e| self.alloc_error(e, size))?;
                heaps.push(raw.contiguity());
                let fd = raw.clone_fd()?;
                let mut t = TensorDyn::from_fd(fd, &self.shape()?, DType::U8, None)?;
                t.set_format(format)?;
                t.set_row_stride(pitch)?;
                pool.push(t);
                continue;
            };
            heaps.push(t.contiguity());
            pool.push(t);
        }
        Ok((pool, contiguity_of(heaps)))
    }

    fn alloc_error(&self, e: edgefirst_tensor::Error, bytes: usize) -> Error {
        if self.request.contiguity == Contiguity::Required {
            Error::new(
                ErrorKind::ContiguousUnavailable,
                format!("{bytes} bytes of physically contiguous memory per buffer: {e}"),
            )
            .with_source(e)
        } else {
            Error::from(e)
        }
    }

    fn validate(&self, pool: &[TensorDyn]) -> Result<()> {
        let infos: Vec<_> = pool.iter().map(BufferInfo::of).collect();
        let req = PoolRequirements {
            format: self.neg.mapping.format,
            width: self.neg.width as usize,
            height: self.neg.height as usize,
            row_stride: Some(self.neg.strides[0]),
            min_count: MIN_BUFFERS,
            max_count: MAX_BUFFERS,
            contiguity: self.request.contiguity,
            native: &[TensorMemory::DmaBuf],
        };
        pool::validate(&infos, &req).map_err(|(reason, slot)| Error::buffers_rejected(reason, slot))
    }

    /// Requests DMABUF buffers for `table` and queues every slot no frame
    /// holds. Held slots are queued when their frames drop.
    fn queue_import(&mut self, table: &SlotTable) -> std::result::Result<(), Refusal> {
        let count = self
            .queue
            .request(Memory::DmaBuf, table.len() as u32)
            .map_err(|e| Refusal::from_v4l2(None, "REQBUFS DMABUF", e))?;
        if (count as usize) < table.len() {
            let _ = self.queue.free();
            return Err(Refusal {
                slot: None,
                errno: libc::ENOMEM,
                error: Error::new(
                    ErrorKind::Backend,
                    format!(
                        "{}: the driver granted {count} of {} buffers",
                        self.path(),
                        table.len()
                    ),
                ),
            });
        }
        while table.pop_released().is_some() {}
        if self.fault("import-qbuf") {
            return Err(Refusal {
                slot: Some(0),
                errno: libc::EINVAL,
                error: Error::new(ErrorKind::InvalidConfig, "QBUF: injected refusal (EINVAL)"),
            });
        }
        for slot in 0..table.len() {
            if self.queue.is_queued(slot as u32) || !self.slot_is_free(table, slot) {
                continue;
            }
            self.enqueue_import(table, slot)?;
        }
        Ok(())
    }

    /// Whether the slot is not held by a frame. A held slot is queued when
    /// its frame drops.
    fn slot_is_free(&self, table: &SlotTable, slot: usize) -> bool {
        std::sync::Arc::strong_count(table.tensor(slot)) == 1
    }

    fn enqueue_import(&self, table: &SlotTable, slot: usize) -> std::result::Result<(), Refusal> {
        let t = table.tensor(slot);
        let fd = t.dmabuf().map_err(|e| Refusal {
            slot: Some(slot),
            errno: libc::EBADF,
            error: Error::from(e),
        })?;
        let plane = Plane::DmaBuf {
            fd,
            length: t.capacity_bytes() as u32,
            bytesused: 0,
            data_offset: 0,
        };
        self.queue
            .enqueue(slot as u32, &[plane], None)
            .map_err(|e| Refusal::from_v4l2(Some(slot), "QBUF", e))
    }

    /// Requests MMAP buffers, exports them and builds a new table.
    fn setup_export(&mut self) -> Result<()> {
        let count = self
            .queue
            .request(Memory::Mmap, self.request.buffers as u32)
            .map_err(|e| v4l2_error("REQBUFS MMAP", e))?;
        let mut tensors = Vec::with_capacity(count as usize);
        let mut handles = Vec::with_capacity(count as usize);
        for i in 0..count {
            let (t, h) = self.export(i)?;
            tensors.push(t);
            handles.push(h);
        }
        let table = SlotTable::new(tensors);
        for i in 0..count {
            self.queue
                .enqueue(i, &vec![Plane::mmap(); self.neg.mapping.mem_planes], None)
                .map_err(|e| v4l2_error("QBUF", e))?;
        }
        self.pool = Some(Pool {
            table,
            memory: ResolvedMemory::Export,
            caller: false,
            handles,
        });
        self.config.memory = ResolvedMemory::Export;
        self.config.contiguous = None;
        self.config.buffer_count = count as usize;
        Ok(())
    }

    /// Exports buffer `index` as a tensor at the driver's pitch.
    fn export(&self, index: u32) -> Result<(TensorDyn, Vec<i64>)> {
        let format = self.neg.mapping.format;
        let (w, h) = (self.neg.width as usize, self.neg.height as usize);
        let fd = |plane: u32| {
            self.queue
                .export(index, plane)
                .map_err(|e| v4l2_error("EXPBUF", e))
        };
        if self.neg.mapping.mem_planes == 1 {
            let fd = fd(0)?;
            let handle = i64::from(fd.as_raw_fd());
            let mut t = TensorDyn::from_fd(fd, &self.shape()?, DType::U8, None)?;
            t.set_format(format)?;
            t.set_row_stride(self.neg.strides[0])?;
            // The tensor owns a dup; record the fd it holds.
            let handle = t.dmabuf().map_or(handle, |b| i64::from(b.as_raw_fd()));
            return Ok((t, vec![handle]));
        }
        // NV12M / NV16M: the chroma plane is its own buffer.
        let chroma_rows = if format == PixelFormat::Nv12 {
            h.div_ceil(2)
        } else {
            h
        };
        let mut luma = Tensor::<u8>::from_fd(fd(0)?, &[h, w], None)?;
        luma.set_row_stride(self.neg.strides[0])?;
        let mut chroma = Tensor::<u8>::from_fd(fd(1)?, &[chroma_rows, w], None)?;
        chroma.set_row_stride(self.neg.strides[1])?;
        let handle = |t: &Tensor<u8>| t.dmabuf().map_or(-1, |b| i64::from(b.as_raw_fd()));
        let handles = vec![handle(&luma), handle(&chroma)];
        let t = Tensor::<u8>::from_planes(luma, chroma, format)?;
        Ok((TensorDyn::from(t), handles))
    }

    /// Builds the SDK or caller Import pool and queues it. Returns `Ok(false)`
    /// when an SDK pool under `Auto` was refused and Export should be used.
    fn setup_import(&mut self, caller: Option<Vec<TensorDyn>>) -> Result<bool> {
        let on_refusal = match (&caller, self.request.memory) {
            (Some(_), _) => OnRefusal::Reject,
            (None, MemoryStrategy::Auto) => OnRefusal::Export,
            (None, _) => OnRefusal::Fail,
        };
        let (tensors, contiguous, is_caller) = match caller {
            Some(pool) => {
                let c = contiguity_of(pool.iter().map(|t| t.contiguity()));
                (pool, c, true)
            }
            None => {
                let (pool, c) = self.allocate()?;
                (pool, c, false)
            }
        };
        let handles = tensors
            .iter()
            .map(|t| vec![t.dmabuf().map_or(-1, |b| i64::from(b.as_raw_fd()))])
            .collect();
        let table = SlotTable::new(tensors);
        if let Err(refusal) = self.queue_import(&table) {
            let _ = self.queue.free();
            return self.refused(table, is_caller, on_refusal, refusal);
        }
        self.config.memory = ResolvedMemory::Import;
        self.config.contiguous = contiguous;
        self.config.buffer_count = table.len();
        self.pool = Some(Pool {
            table,
            memory: ResolvedMemory::Import,
            caller: is_caller,
            handles,
        });
        Ok(true)
    }

    fn refused(
        &mut self,
        table: SlotTable,
        is_caller: bool,
        on_refusal: OnRefusal,
        refusal: Refusal,
    ) -> Result<bool> {
        match on_refusal {
            OnRefusal::Reject => {
                if is_caller {
                    self.provided = Some(untable(table));
                }
                Err(Error::buffers_rejected(
                    Rejection::Driver {
                        errno: refusal.errno,
                    },
                    refusal.slot,
                )
                .with_source(refusal.error))
            }
            OnRefusal::Export => {
                tracing::info!(
                    "{}: driver refused imported buffers ({}); using exported buffers",
                    self.path(),
                    refusal.error
                );
                table.detach();
                Ok(false)
            }
            OnRefusal::Fail => Err(refusal.error),
        }
    }

    /// Starts streaming and waits for the first frame.
    fn stream_and_wait(&mut self) -> std::result::Result<Dequeued, StartFailure> {
        self.queue.stream_on().map_err(StartFailure::Driver)?;
        let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(StartFailure::NoFrame);
            }
            match self.queue.wait(Some(left)) {
                Ok(true) => {}
                Ok(false) => return Err(StartFailure::NoFrame),
                Err(e) => return Err(StartFailure::Driver(e)),
            }
            match self.queue.dequeue() {
                Ok(Some(d)) if d.flags.is_error() => return Err(StartFailure::ErrorFrame),
                Ok(Some(d)) => return Ok(d),
                Ok(None) => continue,
                Err(e) => return Err(StartFailure::Driver(e)),
            }
        }
    }

    /// Stops the stream and frees the queue. Import keeps its table; Export
    /// detaches it.
    fn halt(&mut self) -> Result<()> {
        self.streaming = false;
        self.pending = None;
        self.last_nanos = None;
        let off = self.queue.stream_off();
        let free = self.queue.free();
        if let Some(pool) = &self.pool {
            if pool.memory == ResolvedMemory::Export {
                pool.table.detach();
                self.pool = None;
            }
        }
        off.map_err(|e| v4l2_error("STREAMOFF", e))?;
        free.map_err(|e| v4l2_error("REQBUFS 0", e))
    }

    /// Drops the pool. Frames keep their memory; a caller pool is returned
    /// to `provided` when no frame holds it.
    fn release_pool(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.table.detach();
            if pool.caller {
                let tensors = untable(pool.table);
                if !tensors.is_empty() {
                    self.provided = Some(tensors);
                }
            }
        }
    }

    fn requeue_released(&mut self) -> Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };
        while let Some(slot) = pool.table.pop_released() {
            match pool.memory {
                ResolvedMemory::Import => self
                    .enqueue_import(&pool.table, slot)
                    .map_err(|r| r.error)?,
                ResolvedMemory::Export => self
                    .queue
                    .enqueue(
                        slot as u32,
                        &vec![Plane::mmap(); self.neg.mapping.mem_planes],
                        None,
                    )
                    .map_err(|e| v4l2_error("QBUF", e))?,
            }
        }
        Ok(())
    }

    fn frame(&mut self, d: Dequeued) -> Frame {
        let clock = match d.flags.timestamp_clock() {
            vq::TimestampClock::Monotonic => CaptureClock::Monotonic,
            _ => CaptureClock::Unknown,
        };
        let source = match d.flags.timestamp_source() {
            vq::TimestampSource::StartOfExposure => TimestampSource::StartOfExposure,
            _ => TimestampSource::EndOfFrame,
        };
        if !self.logged_clock {
            self.logged_clock = true;
            if clock == CaptureClock::Monotonic {
                tracing::info!("{}: timestamps {clock:?}, {source:?}", self.path());
            } else {
                tracing::warn!(
                    "{}: timestamps are not CLOCK_MONOTONIC ({clock:?}, {source:?}); acquisition time is unavailable",
                    self.path()
                );
            }
        }
        let nanos = i64::try_from(d.timestamp.as_nanos()).unwrap_or(i64::MAX);
        if let (Some(last), Some(interval)) = (self.last_nanos, self.neg.period()) {
            let periods = ((nanos - last) as f64 / interval.as_nanos() as f64).round() as i64;
            if periods > 1 {
                self.stats.dropped += (periods - 1) as u64;
            }
        }
        self.last_nanos = Some(nanos);
        let timestamp = Timestamp {
            clock,
            source,
            nanos,
        };

        let pool = self.pool.as_ref().expect("frames come from a pool");
        let slot = d.index as usize;
        let tensor = pool.table.tensor(slot);
        let mut meta = FrameMeta::new(slot, self.seq, timestamp);
        self.seq += 1;
        meta.driver_sequence = Some(d.sequence);
        meta.realtime = self.clock.to_realtime(timestamp).ok();
        meta.bytes_used = d.planes().iter().map(|p| p.bytesused as usize).sum();
        meta.planes = planes(&self.config, tensor, &pool.handles[slot], &d);
        self.stats.frames += 1;
        let _span = tracing::trace_span!("camera.v4l2.dequeue", seq = meta.seq, slot).entered();
        pool.table.frame(meta)
    }
}

/// Why `start()` could not reach the first frame.
enum StartFailure {
    Driver(edgefirst_v4l2::Error),
    ErrorFrame,
    NoFrame,
}

impl StartFailure {
    fn is_disconnected(&self) -> bool {
        matches!(self, Self::Driver(e) if e.is_disconnected())
    }
}

/// The errno behind a start failure, for a caller-pool rejection.
fn failure_errno(e: &Error) -> i32 {
    std::error::Error::source(e)
        .and_then(|s| s.downcast_ref::<edgefirst_v4l2::Error>())
        .and_then(errno)
        .unwrap_or(0)
}

/// Plane layout of a frame: one entry per image plane, with the native
/// handle of the memory plane it lives in.
fn planes(
    config: &StreamConfig,
    tensor: &TensorDyn,
    handles: &[i64],
    d: &Dequeued,
) -> Vec<PlaneLayout> {
    let stride = tensor.effective_row_stride().unwrap_or(config.row_stride);
    let table = config
        .format
        .plane_table(config.width as usize, config.height as usize, stride)
        .unwrap_or_default();
    let used: Vec<usize> = d.planes().iter().map(|p| p.bytesused as usize).collect();
    if handles.len() > 1 {
        // One buffer per image plane, each starting at offset 0.
        return table
            .iter()
            .zip(handles)
            .zip(&used)
            .map(|((p, &h), &u)| {
                let mut l = PlaneLayout::new(h, 0, p.stride as usize, p.size as usize);
                l.used = u;
                l
            })
            .collect();
    }
    let handle = handles.first().copied().unwrap_or(-1);
    let total = used.first().copied().unwrap_or(0);
    table
        .iter()
        .map(|p| {
            let mut l = PlaneLayout::new(
                handle,
                p.offset as usize,
                p.stride as usize,
                p.size as usize,
            );
            l.used = total.saturating_sub(p.offset as usize).min(p.size as usize);
            l
        })
        .collect()
}

/// What a pool's memory is known to be: `Some(true)` when every buffer is
/// known contiguous, `Some(false)` when any is known not to be, otherwise
/// `None`.
fn contiguity_of(heaps: impl IntoIterator<Item = Heap>) -> Option<bool> {
    let mut all_contiguous = true;
    for heap in heaps {
        match heap {
            Heap::NonContiguous => return Some(false),
            Heap::Contiguous => {}
            _ => all_contiguous = false,
        }
    }
    all_contiguous.then_some(true)
}

/// Recovers the tensors of a table no frame holds.
fn untable(table: SlotTable) -> Vec<TensorDyn> {
    let tensors: Vec<_> = table.tensors().to_vec();
    drop(table);
    tensors
        .into_iter()
        .filter_map(|t| std::sync::Arc::try_unwrap(t).ok())
        .collect()
}

impl Drop for V4l2Camera {
    fn drop(&mut self) {
        let deferred = self.pool.as_ref().is_some_and(|p| {
            p.memory == ResolvedMemory::Export && p.table.held() > 0 && !self.orphans()
        });
        if deferred {
            // The driver cannot orphan its buffers: keep the open file, and
            // with it the buffers and the device lock, until the last frame
            // drops. The queue holds its own dup of the device fd.
            let _ = self.queue.stream_off();
            if let Some(pool) = self.pool.take() {
                if let Ok(fresh) = Queue::new(&self.dev, self.buf_type) {
                    let open = std::mem::replace(&mut self.queue, fresh);
                    pool.table.keep_alive(Box::new(open));
                }
                pool.table.detach();
            }
            tracing::debug!("{}: close deferred until held frames drop", self.path());
            return;
        }
        if self.streaming || self.pool.is_some() {
            let _ = self.halt();
        }
        if let Some(pool) = self.pool.take() {
            pool.table.detach();
        }
    }
}

impl Camera for V4l2Camera {
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
        match id {
            ControlId::FrameRate => {
                self.config
                    .frame_rate
                    .map(ControlValue::Float)
                    .ok_or_else(|| {
                        Error::new(ErrorKind::InvalidConfig, "the device reports no frame rate")
                    })
            }
            other => self.ctl.get(&self.dev, other),
        }
    }

    fn set_control(&mut self, ctl: Control) -> Result<Applied> {
        let Control::FrameRate(fps) = ctl else {
            return self.ctl.set(&self.dev, ctl);
        };
        if !(fps.is_finite() && fps > 0.0) {
            return Err(Error::new(
                ErrorKind::InvalidConfig,
                format!("frame rate {fps} must be positive"),
            ));
        }
        let got = self
            .dev
            .set_frame_interval(self.buf_type, interval_for(fps))
            .map_err(|e| v4l2_error("S_PARM", e))?;
        self.neg.interval = (got.numerator != 0 && got.denominator != 0).then_some(got);
        self.config.frame_rate = self.neg.frame_rate();
        Ok(match self.config.frame_rate {
            Some(applied) if (applied - fps).abs() < 0.01 => Applied::Driver,
            Some(applied) => Applied::Clamped(ControlValue::Float(applied)),
            None => Applied::Driver,
        })
    }

    fn start(&mut self) -> Result<()> {
        if self.streaming {
            return Ok(());
        }
        let _span = tracing::debug_span!("camera.v4l2.start", path = %self.path()).entered();
        self.prepare()?;
        match self.stream_and_wait() {
            Ok(d) => return self.streaming_from(d),
            Err(failure) => {
                let _ = self.queue.stream_off();
                let _ = self.queue.free();
                let pool = self.pool.as_ref().map(|p| (p.memory, p.caller));
                let auto_probe = pool == Some((ResolvedMemory::Import, false))
                    && self.request.memory == MemoryStrategy::Auto
                    && !failure.is_disconnected();
                if !auto_probe {
                    let e = self.start_error(failure);
                    // Import keeps its tensors for the next attempt; Export
                    // buffers are gone with REQBUFS(0).
                    if pool.is_some_and(|(m, _)| m == ResolvedMemory::Export) {
                        self.release_pool();
                    }
                    return Err(match (pool, e.kind()) {
                        (Some((ResolvedMemory::Import, true)), k)
                            if k != ErrorKind::NotReady && k != ErrorKind::Disconnected =>
                        {
                            let errno = failure_errno(&e);
                            Error::buffers_rejected(Rejection::Driver { errno }, None)
                                .with_source(e)
                        }
                        _ => e,
                    });
                }
                tracing::info!(
                    "{}: imported buffers did not capture ({}); using exported buffers",
                    self.path(),
                    self.start_error(failure)
                );
                self.release_pool();
            }
        }
        self.setup_export()?;
        match self.stream_and_wait() {
            Ok(d) => self.streaming_from(d),
            Err(failure) => {
                let e = self.start_error(failure);
                let _ = self.queue.stream_off();
                let _ = self.queue.free();
                self.release_pool();
                Err(e)
            }
        }
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
        self.release_pool();
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
        if self.pool.as_ref().is_some_and(|p| p.caller) {
            self.release_pool();
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
        if contiguity != self.request.contiguity {
            self.request.contiguity = contiguity;
            if self.pool.as_ref().is_some_and(|p| !p.caller) {
                self.release_pool();
            }
        }
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
        if let Some(d) = self.pending.take() {
            return Ok(self.frame(d));
        }
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            self.requeue_released()?;
            let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if left.is_some_and(|l| l.is_zero()) {
                self.stats.timeouts += 1;
                return Err(Error::new(
                    ErrorKind::Timeout,
                    format!("no frame within {timeout:?}"),
                ));
            }
            // With nothing queued the driver cannot complete a buffer, so
            // wait in short slices and requeue what frames release.
            let slice = if self.queue.queued_count() == 0 {
                Some(left.map_or(STARVED_POLL, |l| l.min(STARVED_POLL)))
            } else {
                left
            };
            if !self.queue.wait(slice).map_err(|e| v4l2_error("poll", e))? {
                continue;
            }
            let Some(d) = self.queue.dequeue().map_err(|e| v4l2_error("DQBUF", e))? else {
                continue;
            };
            if d.flags.is_error() {
                self.stats.errors += 1;
                if let Some(pool) = &self.pool {
                    pool.table.recycle(d.index as usize);
                }
                continue;
            }
            return Ok(self.frame(d));
        }
    }

    fn stats(&self) -> CaptureStats {
        let mut s = self.stats;
        if let Some(pool) = &self.pool {
            s.queued = self.queue.queued_count();
            s.held = pool.table.held();
        } else {
            s.queued = 0;
            s.held = 0;
        }
        s
    }

    fn wait_handle(&self) -> Option<WaitHandle> {
        Some(WaitHandle::new(self.dev.as_fd().as_raw_fd()))
    }
}

impl V4l2Camera {
    /// Builds and queues the pool for `start()`: the existing Import pool,
    /// a validated caller pool, the SDK's Import pool, or Export.
    fn prepare(&mut self) -> Result<()> {
        if let Some(pool) = self.pool.take() {
            // Import across a stop: the tensors are kept, only the queue is
            // set up again.
            let r = self.queue_import(&pool.table);
            let caller = pool.caller;
            self.pool = Some(pool);
            return r.map_err(|refusal| {
                let _ = self.queue.free();
                if caller {
                    Error::buffers_rejected(
                        Rejection::Driver {
                            errno: refusal.errno,
                        },
                        refusal.slot,
                    )
                    .with_source(refusal.error)
                } else {
                    refusal.error
                }
            });
        }
        let caller = self.provided.take();
        if let Some(pool) = &caller {
            if let Err(e) = self.validate(pool) {
                self.provided = caller;
                return Err(e);
            }
            if self.neg.mapping.mem_planes > 1 {
                self.provided = caller;
                return Err(Error::new(
                    ErrorKind::InvalidConfig,
                    format!(
                        "{}: caller buffers cannot capture the multi-buffer format {}",
                        self.path(),
                        uapi::fourcc_str(self.neg.mapping.fourcc)
                    ),
                ));
            }
            return self.setup_import(caller).map(|_| ());
        }
        match self.request.memory {
            MemoryStrategy::Export => self.setup_export(),
            _ if self.neg.mapping.mem_planes > 1 => {
                if self.request.memory == MemoryStrategy::Import {
                    return Err(Error::new(
                        ErrorKind::InvalidConfig,
                        format!(
                            "{}: Import of the multi-buffer format {} is not supported",
                            self.path(),
                            uapi::fourcc_str(self.neg.mapping.fourcc)
                        ),
                    ));
                }
                self.setup_export()
            }
            _ => {
                if self.setup_import(None)? {
                    Ok(())
                } else {
                    self.setup_export()
                }
            }
        }
    }

    fn streaming_from(&mut self, first: Dequeued) -> Result<()> {
        self.pending = Some(first);
        self.streaming = true;
        Ok(())
    }

    fn start_error(&self, f: StartFailure) -> Error {
        match f {
            StartFailure::Driver(e) => v4l2_error("STREAMON", e),
            StartFailure::ErrorFrame => Error::new(
                ErrorKind::Backend,
                format!(
                    "{}: the first buffer came back flagged as errored",
                    self.path()
                ),
            ),
            StartFailure::NoFrame => Error::new(
                ErrorKind::NotReady,
                format!(
                    "{}: no frame within {FIRST_FRAME_TIMEOUT:?} of starting",
                    self.path()
                ),
            ),
        }
    }
}

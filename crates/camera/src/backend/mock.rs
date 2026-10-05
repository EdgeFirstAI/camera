// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Synthetic frame source.
//!
//! Produces a moving gradient at the requested size and rate, in host
//! memory, with the full control surface reporting `Unsupported(Backend)`
//! except the frame rate. It is the SDK's own test source and the worked
//! example for third-party backends: frames are built with [`Frame::new`]
//! and returned through [`SlotRelease`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use edgefirst_tensor::{CpuAccess, DType, PixelFormat, TensorDyn, TensorMemory};

use crate::builder::Source;
use crate::{
    timestamp, Applied, Backend, BufferPool, Camera, CameraBuilder, CameraDescriptor, CaptureClock,
    CaptureStats, Contiguity, Control, ControlId, ControlInfo, ControlSet, ControlValue, Error,
    ErrorKind, FormatInfo, Frame, FrameMeta, PlaneLayout, Rates, RealtimeClock, Rejection,
    ResolvedMemory, Result, Sizes, SlotRelease, StreamConfig, StreamRequest, Timestamp,
    TimestampSource, Unsupported, WaitHandle,
};

const FORMATS: [PixelFormat; 4] = [
    PixelFormat::Yuyv,
    PixelFormat::Nv12,
    PixelFormat::Rgba,
    PixelFormat::Grey,
];
const MIN_FPS: f64 = 1.0;
const MAX_FPS: f64 = 240.0;

pub(crate) fn descriptor(id: &str) -> CameraDescriptor {
    let mut d = CameraDescriptor::new(Backend::Mock, id, "Synthetic camera", "mock");
    d.formats = FORMATS
        .iter()
        .map(|&format| FormatInfo {
            format,
            sizes: Sizes::Stepwise {
                min: (16, 16),
                max: (8192, 8192),
                step: (2, 2),
                rates: Rates::Range {
                    min: MIN_FPS,
                    max: MAX_FPS,
                },
            },
        })
        .collect();
    d
}

/// Free-slot list shared with outstanding frames.
#[derive(Debug, Default)]
struct Slots {
    free: Mutex<VecDeque<usize>>,
}

impl SlotRelease for Slots {
    fn release(&self, slot: usize) {
        if let Ok(mut free) = self.free.lock() {
            free.push_back(slot);
        }
    }
}

#[derive(Debug)]
struct MockCamera {
    descriptor: CameraDescriptor,
    request: StreamRequest,
    config: StreamConfig,
    controls: ControlSet,
    provided: Option<Vec<TensorDyn>>,
    pool: Vec<Arc<TensorDyn>>,
    slots: Arc<Slots>,
    clock: RealtimeClock,
    streaming: bool,
    epoch: Instant,
    seq: u64,
    stats: CaptureStats,
}

pub(crate) fn open(builder: CameraBuilder) -> Result<Box<dyn Camera>> {
    let Source::Mock { width, height, fps } = builder.source else {
        return Err(Error::new(ErrorKind::InvalidConfig, "not a mock source"));
    };
    let request = builder.request;
    let (width, height) = request.size.unwrap_or((width, height));
    let format = request.format.unwrap_or(PixelFormat::Yuyv);
    if !FORMATS.contains(&format) {
        return Err(Error::new(
            ErrorKind::UnsupportedFormat,
            format!("mock source does not produce {format:?}"),
        ));
    }
    if width < 16 || height < 16 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(Error::new(
            ErrorKind::InvalidConfig,
            format!("mock size {width}x{height} must be even and at least 16x16"),
        ));
    }
    let fps = request.frame_rate.unwrap_or(fps).clamp(MIN_FPS, MAX_FPS);
    let mut config = StreamConfig::new(width, height, format, 0);
    config.frame_rate = Some(fps);
    config.buffer_count = request.buffers;

    let mut controls = ControlSet::new();
    controls.insert(
        ControlId::FrameRate,
        ControlInfo::new(
            ControlValue::Float(MIN_FPS),
            ControlValue::Float(MAX_FPS),
            ControlValue::Float(0.0),
            ControlValue::Float(30.0),
        ),
    );

    let mut camera = MockCamera {
        descriptor: descriptor(&format!("mock:{width}x{height}@{fps}")),
        request,
        config,
        controls,
        provided: builder.provided,
        pool: Vec::new(),
        slots: Arc::new(Slots::default()),
        clock: RealtimeClock::new(),
        streaming: false,
        epoch: Instant::now(),
        seq: 0,
        stats: CaptureStats::default(),
    };
    for control in builder.controls {
        let applied = camera.set_control(control)?;
        if let Applied::Unsupported(why) = applied {
            tracing::warn!("mock: control {:?} unsupported ({why:?})", control.id());
        }
    }
    Ok(Box::new(camera))
}

impl MockCamera {
    fn allocate(&self) -> Result<Vec<TensorDyn>> {
        let (w, h) = (self.config.width as usize, self.config.height as usize);
        (0..self.request.buffers)
            .map(|_| {
                TensorDyn::image(
                    w,
                    h,
                    self.config.format,
                    DType::U8,
                    Some(TensorMemory::Mem),
                    CpuAccess::ReadWrite,
                )
                .map_err(Error::from)
            })
            .collect()
    }

    fn validate(&self, pool: &[TensorDyn]) -> Result<()> {
        if pool.len() < 2 {
            return Err(Error::buffers_rejected(Rejection::Count, None));
        }
        for (slot, t) in pool.iter().enumerate() {
            if t.format() != Some(self.config.format) {
                return Err(Error::buffers_rejected(Rejection::Format, Some(slot)));
            }
            if t.width() != Some(self.config.width as usize)
                || t.height() != Some(self.config.height as usize)
            {
                return Err(Error::buffers_rejected(Rejection::Size, Some(slot)));
            }
        }
        Ok(())
    }

    fn planes(&self, tensor: &TensorDyn) -> Vec<PlaneLayout> {
        let stride = tensor.effective_row_stride().unwrap_or(0);
        self.config
            .format
            .plane_table(
                self.config.width as usize,
                self.config.height as usize,
                stride,
            )
            .unwrap_or_default()
            .into_iter()
            .map(|p| PlaneLayout::new(-1, p.offset as usize, p.stride as usize, p.size as usize))
            .collect()
    }

    fn fill(&self, tensor: &TensorDyn, stride: usize) -> Result<()> {
        let mut map = tensor.map_bytes(CpuAccess::Write)?;
        let shift = self.seq as usize;
        if stride == 0 {
            map.fill(shift as u8);
            return Ok(());
        }
        for (row, line) in map.chunks_mut(stride).enumerate() {
            line.fill((row + shift) as u8);
        }
        Ok(())
    }

    fn take_free_slot(&self) -> Option<usize> {
        self.slots.free.lock().ok()?.pop_front()
    }
}

impl Camera for MockCamera {
    fn descriptor(&self) -> &CameraDescriptor {
        &self.descriptor
    }

    fn config(&self) -> &StreamConfig {
        &self.config
    }

    fn controls(&self) -> &ControlSet {
        &self.controls
    }

    fn get_control(&self, id: ControlId) -> Result<ControlValue> {
        match id {
            ControlId::FrameRate => Ok(ControlValue::Float(self.config.frame_rate.unwrap_or(0.0))),
            other => Err(Error::new(
                ErrorKind::InvalidConfig,
                format!("mock source does not support {other:?}"),
            )),
        }
    }

    fn set_control(&mut self, ctl: Control) -> Result<Applied> {
        match ctl {
            Control::FrameRate(fps) if fps.is_finite() && fps > 0.0 => {
                let applied = fps.clamp(MIN_FPS, MAX_FPS);
                self.config.frame_rate = Some(applied);
                Ok(if applied == fps {
                    Applied::Driver
                } else {
                    Applied::Clamped(ControlValue::Float(applied))
                })
            }
            Control::FrameRate(fps) => Err(Error::new(
                ErrorKind::InvalidConfig,
                format!("frame rate {fps} must be positive"),
            )),
            _ => Ok(Applied::Unsupported(Unsupported::Backend)),
        }
    }

    fn start(&mut self) -> Result<()> {
        if self.streaming {
            return Ok(());
        }
        let pool = match self.provided.take() {
            Some(pool) => {
                if let Err(e) = self.validate(&pool) {
                    self.provided = Some(pool);
                    return Err(e);
                }
                pool
            }
            None => self.allocate()?,
        };
        self.config.row_stride = pool[0].effective_row_stride().unwrap_or(0);
        self.config.planes = self.planes(&pool[0]).len();
        self.config.buffer_count = pool.len();
        self.config.memory = ResolvedMemory::Import;
        // Host memory: contiguity is not a property the mock can promise.
        self.config.contiguous = None;
        self.pool = pool.into_iter().map(Arc::new).collect();
        // A fresh slot list: frames still held from an earlier run release
        // into the old list and are never reused.
        self.slots = Arc::new(Slots::default());
        if let Ok(mut free) = self.slots.free.lock() {
            free.extend(0..self.pool.len());
        }
        self.epoch = Instant::now();
        self.streaming = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.streaming = false;
        Ok(())
    }

    fn set_buffers(&mut self, pool: BufferPool) -> Result<()> {
        if self.streaming {
            return Err(Error::new(
                ErrorKind::InvalidConfig,
                "set_buffers while streaming",
            ));
        }
        self.pool.clear();
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
        let fps = self.config.frame_rate.unwrap_or(30.0);
        let interval = Duration::from_secs_f64(1.0 / fps);
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            let due = self.epoch + interval.mul_f64((self.seq + self.stats.dropped) as f64);
            let now = Instant::now();
            if due > now {
                if deadline.is_some_and(|d| d < due) {
                    std::thread::sleep(deadline.unwrap().saturating_duration_since(now));
                    self.stats.timeouts += 1;
                    return Err(Error::new(
                        ErrorKind::Timeout,
                        "no frame within the timeout",
                    ));
                }
                std::thread::sleep(due - now);
            }
            match self.take_free_slot() {
                Some(slot) => {
                    let tensor = self.pool[slot].clone();
                    self.fill(&tensor, self.config.row_stride)?;
                    let clock = if cfg!(unix) {
                        CaptureClock::Monotonic
                    } else {
                        CaptureClock::Realtime
                    };
                    let ts = Timestamp {
                        clock,
                        source: TimestampSource::EndOfFrame,
                        nanos: timestamp::now(clock)?,
                    };
                    let mut meta = FrameMeta::new(slot, self.seq, ts);
                    meta.driver_sequence = Some(self.seq as u32);
                    meta.realtime = self.clock.to_realtime(ts).ok();
                    meta.planes = self.planes(&tensor);
                    meta.bytes_used = meta.planes.iter().map(|p| p.used).sum();
                    self.seq += 1;
                    self.stats.frames += 1;
                    let release: Arc<dyn SlotRelease> = self.slots.clone();
                    return Ok(Frame::new(tensor, meta, Some(release)));
                }
                None => {
                    // Every buffer is held: this frame period is lost.
                    self.stats.dropped += 1;
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        self.stats.timeouts += 1;
                        return Err(Error::new(
                            ErrorKind::Timeout,
                            "no free buffer within the timeout; every buffer is held",
                        ));
                    }
                }
            }
        }
    }

    fn stats(&self) -> CaptureStats {
        let free = self.slots.free.lock().map(|f| f.len()).unwrap_or(0);
        CaptureStats {
            queued: free,
            held: self.pool.len().saturating_sub(free),
            ..self.stats
        }
    }

    fn wait_handle(&self) -> Option<WaitHandle> {
        None
    }
}

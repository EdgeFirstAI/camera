// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Synthetic frame source.
//!
//! Produces a moving gradient at the requested size and rate, in host
//! memory, with the full control surface reporting `Unsupported(Backend)`
//! except the frame rate. It is the SDK's own test source and the worked
//! example for third-party backends: buffers live in a [`SlotTable`], the
//! "driver" is a FIFO of queued slots, and released slots go back to it.
//!
//! The mock behaves like an Import backend: `stop()` keeps the table, so
//! frames held across a restart stay valid and requeue when they drop.
//! Changing the pool or dropping the camera detaches the table.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use edgefirst_tensor::{CpuAccess, DType, PixelFormat, TensorDyn, TensorMemory};

use crate::builder::Source;
use crate::pool::{self, BufferInfo, PoolRequirements};
use crate::{
    timestamp, Applied, Backend, BufferPool, Camera, CameraBuilder, CameraDescriptor, CaptureClock,
    CaptureStats, Contiguity, Control, ControlId, ControlInfo, ControlSet, ControlValue, Error,
    ErrorKind, FormatInfo, Frame, FrameMeta, PlaneLayout, Rates, RealtimeClock, ResolvedMemory,
    Result, Sizes, SlotTable, StreamConfig, StreamRequest, Timestamp, TimestampSource, Unsupported,
    WaitHandle,
};

const FORMATS: [PixelFormat; 4] = [
    PixelFormat::Yuyv,
    PixelFormat::Nv12,
    PixelFormat::Rgba,
    PixelFormat::Grey,
];
const MIN_FPS: f64 = 1.0;
const MAX_FPS: f64 = 240.0;
/// Pool depth limits, matching `--camera-buffers`.
const MIN_BUFFERS: usize = 2;
const MAX_BUFFERS: usize = 32;
/// The mock writes with the CPU, so any CPU-mappable memory will do.
const NATIVE: &[TensorMemory] = &[TensorMemory::Mem, TensorMemory::Shm, TensorMemory::DmaBuf];

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

#[derive(Debug)]
struct MockCamera {
    descriptor: CameraDescriptor,
    request: StreamRequest,
    config: StreamConfig,
    controls: ControlSet,
    provided: Option<Vec<TensorDyn>>,
    table: Option<SlotTable>,
    /// Slots the "driver" owns, in the order it fills them.
    driver: VecDeque<usize>,
    clock: RealtimeClock,
    streaming: bool,
    /// Start of the current pacing run.
    epoch: Instant,
    /// Frame periods elapsed in the current pacing run, delivered or lost.
    periods: u64,
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
    if builder.provided.is_none() && !(MIN_BUFFERS..=MAX_BUFFERS).contains(&request.buffers) {
        return Err(Error::new(
            ErrorKind::InvalidConfig,
            format!(
                "pool depth {} outside {MIN_BUFFERS}..={MAX_BUFFERS}",
                request.buffers
            ),
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
        table: None,
        driver: VecDeque::new(),
        clock: RealtimeClock::new(),
        streaming: false,
        epoch: Instant::now(),
        periods: 0,
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
        let infos: Vec<_> = pool.iter().map(BufferInfo::of).collect();
        let req = PoolRequirements {
            format: self.config.format,
            width: self.config.width as usize,
            height: self.config.height as usize,
            // The mock adapts to any pitch, as a driver honouring padding does.
            row_stride: None,
            min_count: MIN_BUFFERS,
            max_count: MAX_BUFFERS,
            contiguity: self.request.contiguity,
            native: NATIVE,
        };
        pool::validate(&infos, &req).map_err(|(reason, slot)| Error::buffers_rejected(reason, slot))
    }

    /// Retires the current table: held frames keep their memory and never
    /// requeue.
    fn detach(&mut self) {
        if let Some(table) = self.table.take() {
            table.detach();
        }
        self.driver.clear();
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

    fn fill(&self, tensor: &TensorDyn) -> Result<()> {
        let stride = self.config.row_stride;
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

    /// The next slot the driver fills, after taking back released slots.
    fn next_driver_slot(&mut self) -> Option<usize> {
        let table = self.table.as_ref()?;
        while let Some(slot) = table.pop_released() {
            self.driver.push_back(slot);
        }
        self.driver.pop_front()
    }

    fn restart_pacing(&mut self) {
        self.epoch = Instant::now();
        self.periods = 0;
    }
}

impl Drop for MockCamera {
    fn drop(&mut self) {
        self.detach();
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
                self.restart_pacing();
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
        if self.table.is_none() {
            let pool = match self.provided.take() {
                Some(pool) => {
                    if let Err(e) = self.validate(&pool) {
                        // The caller can take the pool back with take_buffers().
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
            self.driver = (0..pool.len()).collect();
            self.table = Some(SlotTable::new(pool));
        }
        self.restart_pacing();
        self.streaming = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        // Import semantics: the table and its buffers are kept, so frames
        // held across a restart stay valid and requeue when they drop.
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
        self.detach();
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
        let interval = Duration::from_secs_f64(1.0 / self.config.frame_rate.unwrap_or(30.0));
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            let due = self.epoch + interval.mul_f64(self.periods as f64);
            let now = Instant::now();
            if due > now {
                if let Some(deadline) = deadline.filter(|d| *d < due) {
                    std::thread::sleep(deadline.saturating_duration_since(now));
                    self.stats.timeouts += 1;
                    return Err(Error::new(
                        ErrorKind::Timeout,
                        "no frame within the timeout",
                    ));
                }
                std::thread::sleep(due - now);
            }
            self.periods += 1;
            let Some(slot) = self.next_driver_slot() else {
                // Every buffer is held, so this frame period is lost.
                self.stats.dropped += 1;
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    self.stats.timeouts += 1;
                    return Err(Error::new(
                        ErrorKind::Timeout,
                        "no free buffer within the timeout; every buffer is held",
                    ));
                }
                continue;
            };
            let Some(table) = self.table.as_ref() else {
                return Err(Error::new(ErrorKind::InvalidConfig, "no buffers"));
            };
            let tensor = table.tensor(slot).clone();
            self.fill(&tensor)?;
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
            let Some(table) = self.table.as_ref() else {
                return Err(Error::new(ErrorKind::InvalidConfig, "no buffers"));
            };
            return Ok(table.frame(meta));
        }
    }

    fn stats(&self) -> CaptureStats {
        let (queued, held) = self
            .table
            .as_ref()
            .map(|t| (t.queued(), t.held()))
            .unwrap_or((0, 0));
        CaptureStats {
            queued,
            held,
            ..self.stats
        }
    }

    fn wait_handle(&self) -> Option<WaitHandle> {
        None
    }
}

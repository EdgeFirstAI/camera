// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Recorded H.264 playback (`file:<name>.h264` with `<name>.json`).
//!
//! Access units are read from the file one at a time and decoded with the
//! platform decoder into its own NV12 pool; a frame owns its decoded picture
//! and gives the buffer back when it drops. Frames are paced at the
//! sidecar's rate, or the requested one, and stamped at the paced instant
//! they are delivered, in the same clock as live frames. With looping on,
//! the file restarts at its end and `seq` keeps counting.
//!
//! Each frame carries, as `encoded()`, the access units read since the
//! previous frame was decoded, in file order, so forwarding them reproduces
//! the recording byte for byte whatever the decoder holds back.
//!
//! A unit the decoder rejects is skipped with a warning, as are bytes that
//! are not part of an access unit; playback resyncs at the next unit.

mod annexb;
mod decode;
mod sidecar;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use edgefirst_tensor::{PixelFormat, TensorDyn};

use self::annexb::AccessUnitReader;
use self::decode::{Decode, DecodeError, Event, Format, HalDecoder, Picture};
use self::sidecar::Sidecar;
use crate::builder::Source;
use crate::frame::TensorHolder;
use crate::{
    timestamp, Applied, Backend, BufferPool, Camera, CameraBuilder, CameraDescriptor, CaptureClock,
    CaptureStats, Contiguity, Control, ControlId, ControlInfo, ControlSet, ControlValue, Error,
    ErrorKind, Frame, FrameMeta, PlaneLayout, RealtimeClock, Rejection, Result, StreamConfig,
    StreamRequest, Timestamp, TimestampSource, Unsupported, WaitHandle,
};

const MIN_FPS: f64 = 0.1;
const MAX_FPS: f64 = 1000.0;
/// Pool depth limits, matching `--camera-buffers`.
const MIN_BUFFERS: usize = 2;
const MAX_BUFFERS: usize = 32;
/// Consecutive undecodable units after which the decoder is taken to have
/// failed rather than the stream to be damaged.
const MAX_CONSECUTIVE_SKIPS: u32 = 30;
/// How often to retry while every decoded frame is held.
const HELD_RETRY: Duration = Duration::from_millis(2);

/// What the source reads the stream from.
trait Stream: Read + Seek + Send + std::fmt::Debug {}
impl<T: Read + Seek + Send + std::fmt::Debug> Stream for T {}

fn descriptor(path: &Path) -> CameraDescriptor {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    CameraDescriptor::new(Backend::File, path.display().to_string(), name, "file")
}

pub(crate) fn open(builder: CameraBuilder) -> Result<Box<dyn Camera>> {
    let Source::File(path) = &builder.source else {
        return Err(Error::new(ErrorKind::InvalidConfig, "not a file source"));
    };
    let path = path.clone();
    let file = File::open(&path).map_err(|e| {
        let kind = if e.kind() == std::io::ErrorKind::NotFound {
            ErrorKind::NotFound
        } else {
            ErrorKind::Io
        };
        Error::new(kind, format!("{}: {e}", path.display())).with_source(e)
    })?;
    let sidecar = Sidecar::load(&path)?;
    let held = builder.request.buffers;
    FileCamera::new(builder, path, sidecar, Box::new(file), |_| {
        Ok(Box::new(HalDecoder::open(held)?))
    })
    .map(|c| Box::new(c) as Box<dyn Camera>)
}

/// When each frame is due: one interval after the previous one, so pacing
/// does not drift with decode time. A delivery more than an interval late
/// re-anchors at the present, so a stall is not followed by a burst.
#[derive(Debug)]
struct Pacer {
    interval: Duration,
    next: Option<Instant>,
}

impl Pacer {
    fn new(fps: f64) -> Self {
        Self {
            interval: Duration::from_secs_f64(1.0 / fps),
            next: None,
        }
    }

    fn set_rate(&mut self, fps: f64) {
        self.interval = Duration::from_secs_f64(1.0 / fps);
        self.next = None;
    }

    fn restart(&mut self) {
        self.next = None;
    }

    fn due(&self, now: Instant) -> Instant {
        match self.next {
            Some(next) if now <= next + self.interval => next,
            _ => now,
        }
    }

    fn delivered(&mut self, due: Instant) {
        self.next = Some(due + self.interval);
    }
}

/// A decoded picture lent to a frame, counted while held.
struct Lent {
    inner: Box<dyn TensorHolder>,
    held: Arc<AtomicUsize>,
}

impl TensorHolder for Lent {
    fn tensor(&self) -> &TensorDyn {
        self.inner.tensor()
    }
}

impl Drop for Lent {
    fn drop(&mut self) {
        self.held.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A decoded picture waiting to be delivered.
struct Ready {
    picture: Picture,
    encoded: Option<Arc<[u8]>>,
}

impl std::fmt::Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ready")
            .field("corrupted", &self.picture.corrupted)
            .field("encoded", &self.encoded.as_ref().map(|e| e.len()))
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct FileCamera {
    descriptor: CameraDescriptor,
    request: StreamRequest,
    config: StreamConfig,
    controls: ControlSet,
    extras: String,
    provided: Option<Vec<TensorDyn>>,
    reader: AccessUnitReader<Box<dyn Stream>>,
    decoder: Box<dyn Decode>,
    looping: bool,
    streaming: bool,
    /// Pictures decoded and not yet delivered, in order.
    ready: VecDeque<Ready>,
    /// Units read since the last picture came out of the decoder.
    unclaimed: Vec<Arc<[u8]>>,
    /// A unit the decoder could not take because every frame was held.
    retry: Option<Arc<[u8]>>,
    /// Units fed to the decoder, its `pts`.
    units: u64,
    /// The decoder has been drained at the end of this pass.
    flushed: bool,
    /// Pictures decoded in this pass of the file.
    pictures: u64,
    skips: u32,
    held: Arc<AtomicUsize>,
    pacer: Pacer,
    clock: RealtimeClock,
    seq: u64,
    stats: CaptureStats,
}

impl FileCamera {
    fn new(
        builder: CameraBuilder,
        path: PathBuf,
        sidecar: Sidecar,
        stream: Box<dyn Stream>,
        decoder: impl FnOnce(usize) -> Result<Box<dyn Decode>>,
    ) -> Result<Self> {
        let request = builder.request;
        if let Some(format) = request.format.filter(|f| *f != PixelFormat::Nv12) {
            return Err(Error::new(
                ErrorKind::UnsupportedFormat,
                format!("a file source decodes to NV12, not {format:?}"),
            ));
        }
        if let Some((w, h)) = request
            .size
            .filter(|s| *s != (sidecar.width, sidecar.height))
        {
            tracing::warn!(
                "file: {w}x{h} requested; playing at the recorded {}x{}",
                sidecar.width,
                sidecar.height
            );
        }
        if request.mirror.is_some() {
            tracing::warn!("file: mirroring is not supported by a file source");
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
        let fps = request.frame_rate.unwrap_or(sidecar.fps);
        if !(fps.is_finite() && fps > 0.0) {
            return Err(Error::new(
                ErrorKind::InvalidConfig,
                format!("frame rate {fps} must be positive"),
            ));
        }
        let fps = fps.clamp(MIN_FPS, MAX_FPS);
        let decoder = decoder(request.buffers)?;

        let mut config = StreamConfig::new(sidecar.width, sidecar.height, PixelFormat::Nv12, 0);
        config.planes = 2;
        config.frame_rate = Some(fps);
        config.colorimetry = sidecar.colorimetry;
        config.buffer_count = request.buffers;

        let mut controls = ControlSet::new();
        controls.insert(
            ControlId::FrameRate,
            ControlInfo::new(
                ControlValue::Float(MIN_FPS),
                ControlValue::Float(MAX_FPS),
                ControlValue::Float(0.0),
                ControlValue::Float(sidecar.fps),
            ),
        );

        let mut camera = Self {
            descriptor: descriptor(&path),
            request,
            config,
            controls,
            extras: sidecar.extras,
            provided: builder.provided,
            reader: AccessUnitReader::new(stream),
            decoder,
            looping: builder.looping,
            streaming: false,
            ready: VecDeque::new(),
            unclaimed: Vec::new(),
            retry: None,
            units: 0,
            flushed: false,
            pictures: 0,
            skips: 0,
            held: Arc::new(AtomicUsize::new(0)),
            pacer: Pacer::new(fps),
            clock: RealtimeClock::new(),
            seq: 0,
            stats: CaptureStats::default(),
        };
        for control in builder.controls {
            if let Applied::Unsupported(why) = camera.set_control(control)? {
                tracing::warn!("file: control {:?} unsupported ({why:?})", control.id());
            }
        }
        Ok(camera)
    }

    /// Takes the decoder's events: pictures join the ready queue, claiming
    /// the units read since the last one; a format updates the config.
    fn receive(&mut self, events: Vec<Event>) {
        for event in events {
            match event {
                Event::Format(format) => self.reconfigure(format),
                Event::Picture(picture) => {
                    self.pictures += 1;
                    if picture.corrupted {
                        // As a driver's errored buffer: counted, not delivered.
                        // Its units stay unclaimed and go out with the next frame.
                        self.stats.errors += 1;
                        tracing::warn!("file: the decoder reported a damaged picture; skipping it");
                        continue;
                    }
                    let encoded = self.claim();
                    self.ready.push_back(Ready { picture, encoded });
                }
            }
        }
    }

    fn claim(&mut self) -> Option<Arc<[u8]>> {
        match self.unclaimed.len() {
            0 => None,
            1 => self.unclaimed.pop(),
            _ => {
                let joined = self.unclaimed.concat();
                self.unclaimed.clear();
                Some(joined.into())
            }
        }
    }

    fn reconfigure(&mut self, f: Format) {
        if (f.width, f.height) != (self.config.width, self.config.height) {
            tracing::warn!(
                "file: the stream is {}x{}, the sidecar says {}x{}",
                f.width,
                f.height,
                self.config.width,
                self.config.height
            );
        }
        self.config.width = f.width;
        self.config.height = f.height;
        self.config.format = f.format;
        if f.colorimetry.is_some() && self.config.colorimetry.is_none() {
            self.config.colorimetry = f.colorimetry;
        }
        self.config.buffer_count = f.buffers;
        self.config.memory = f.memory;
    }

    /// Feeds the decoder until a picture is ready. `Ok(false)` at the end
    /// of the stream.
    fn decode_more(&mut self, deadline: Option<Instant>) -> Result<bool> {
        while self.ready.is_empty() {
            let unit = match self.retry.take() {
                Some(unit) => Some(unit),
                None => self.reader.next_unit()?,
            };
            let Some(unit) = unit else {
                if !self.flushed {
                    let events = self.wait_for_buffers(deadline, |d| d.flush())?;
                    self.flushed = true;
                    self.receive(events);
                    continue;
                }
                if !self.looping {
                    return Ok(false);
                }
                if self.pictures == 0 {
                    return Ok(false);
                }
                tracing::debug!("file: looping to the start");
                self.reader.rewind()?;
                self.flushed = false;
                self.pictures = 0;
                continue;
            };
            let pts = self.units;
            let decoded = self.decoder.decode(&unit, pts);
            match decoded {
                Ok(events) => {
                    self.skips = 0;
                    self.units += 1;
                    self.unclaimed.push(unit);
                    self.receive(events);
                }
                Err(DecodeError::PoolExhausted) => {
                    self.retry = Some(unit);
                    self.wait_for_a_release(deadline)?;
                }
                Err(DecodeError::Skip(why)) => {
                    self.units += 1;
                    self.unclaimed.push(unit);
                    self.stats.errors += 1;
                    self.skips += 1;
                    tracing::warn!("file: skipping an access unit the decoder rejected: {why}");
                    if self.skips >= MAX_CONSECUTIVE_SKIPS {
                        return Err(Error::new(
                            ErrorKind::Backend,
                            format!("the decoder rejected {} access units in a row", self.skips),
                        ));
                    }
                }
                Err(DecodeError::Fatal(e)) => return Err(e),
            }
        }
        Ok(true)
    }

    fn wait_for_buffers(
        &mut self,
        deadline: Option<Instant>,
        op: impl Fn(&mut dyn Decode) -> std::result::Result<Vec<Event>, DecodeError>,
    ) -> Result<Vec<Event>> {
        loop {
            match op(self.decoder.as_mut()) {
                Ok(events) => return Ok(events),
                Err(DecodeError::PoolExhausted) => self.wait_for_a_release(deadline)?,
                Err(DecodeError::Skip(why)) => {
                    tracing::warn!("file: draining the decoder failed: {why}");
                    return Ok(Vec::new());
                }
                Err(DecodeError::Fatal(e)) => return Err(e),
            }
        }
    }

    /// Every decoded frame is held: wait for one to drop, as a driver whose
    /// buffers are all dequeued waits.
    fn wait_for_a_release(&mut self, deadline: Option<Instant>) -> Result<()> {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            self.stats.timeouts += 1;
            return Err(Error::new(
                ErrorKind::Timeout,
                "no frame within the timeout; every decoded frame is held",
            ));
        }
        std::thread::sleep(HELD_RETRY);
        Ok(())
    }

    fn deliver(&mut self, ready: Ready, deadline: Option<Instant>) -> Result<Frame> {
        let now = Instant::now();
        let due = self.pacer.due(now);
        if let Some(deadline) = deadline.filter(|d| *d < due) {
            std::thread::sleep(deadline.saturating_duration_since(now));
            self.ready.push_front(ready);
            self.stats.timeouts += 1;
            return Err(Error::new(
                ErrorKind::Timeout,
                "no frame within the timeout",
            ));
        }
        std::thread::sleep(due.saturating_duration_since(now));
        self.pacer.delivered(due);

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
        let Ready { picture, encoded } = ready;
        let mut meta = FrameMeta::new(0, self.seq, ts);
        meta.realtime = self.clock.to_realtime(ts).ok();
        meta.planes = planes(picture.holder.tensor(), &self.config);
        meta.bytes_used = meta.planes.iter().map(|p| p.used).sum();
        meta.encoded = encoded;
        self.seq += 1;
        self.stats.frames += 1;
        self.held.fetch_add(1, Ordering::Relaxed);
        let lent = Lent {
            inner: picture.holder,
            held: self.held.clone(),
        };
        Ok(Frame::held(Box::new(lent), meta, None))
    }
}

/// The plane layout of a decoded picture: one entry per plane, with the
/// DMA-BUF it lives in where it has one.
fn planes(tensor: &TensorDyn, config: &StreamConfig) -> Vec<PlaneLayout> {
    let Some(t) = tensor.as_typed::<u8>() else {
        return Vec::new();
    };
    let (width, height) = (config.width as usize, config.height as usize);
    let stride = t.effective_row_stride().unwrap_or(width);
    let handle = |t: &edgefirst_tensor::Tensor<u8>| -> i64 {
        #[cfg(target_os = "linux")]
        if let Ok(fd) = t.dmabuf() {
            use std::os::fd::AsRawFd;
            return i64::from(fd.as_raw_fd());
        }
        let _ = t;
        -1
    };
    let offset = t.plane_offset().unwrap_or(0);
    if let Some(chroma) = t.chroma() {
        let chroma_stride = chroma.row_stride().unwrap_or(stride);
        return vec![
            PlaneLayout::new(handle(t), offset, stride, stride * height),
            PlaneLayout::new(
                handle(chroma),
                chroma.plane_offset().unwrap_or(0),
                chroma_stride,
                chroma_stride * height.div_ceil(2),
            ),
        ];
    }
    config
        .format
        .plane_table(width, height, stride)
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            PlaneLayout::new(
                handle(t),
                offset + p.offset as usize,
                p.stride as usize,
                p.size as usize,
            )
        })
        .collect()
}

impl Camera for FileCamera {
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
                format!("a file source does not support {other:?}"),
            )),
        }
    }

    fn set_control(&mut self, ctl: Control) -> Result<Applied> {
        match ctl {
            Control::FrameRate(fps) if fps.is_finite() && fps > 0.0 => {
                let applied = fps.clamp(MIN_FPS, MAX_FPS);
                self.config.frame_rate = Some(applied);
                self.pacer.set_rate(applied);
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
        if self.provided.is_some() {
            // Decoders write at the coded size (1088 rows for 1080p) and their
            // own pitch, so pictures go into the decoder's pool. The caller
            // takes the buffers back with take_buffers().
            return Err(Error::buffers_rejected(Rejection::Size, None));
        }
        self.pacer.restart();
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
        let deadline = timeout.map(|t| Instant::now() + t);
        if !self.decode_more(deadline)? {
            return Err(Error::new(
                ErrorKind::EndOfStream,
                format!("{} played to its end", self.descriptor.id),
            ));
        }
        let Some(ready) = self.ready.pop_front() else {
            unreachable!("decode_more returned with nothing ready");
        };
        self.deliver(ready, deadline)
    }

    fn stats(&self) -> CaptureStats {
        let held = self.held.load(Ordering::Relaxed);
        CaptureStats {
            held,
            queued: self.config.buffer_count.saturating_sub(held),
            ..self.stats
        }
    }

    fn wait_handle(&self) -> Option<WaitHandle> {
        None
    }

    fn extras(&self) -> Option<&str> {
        Some(&self.extras)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResolvedMemory;
    use edgefirst_tensor::{CpuAccess, DType, TensorMemory};
    use std::io::Cursor;

    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80];
    const IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84, 0x00];
    /// A unit the mock decoder rejects: a P slice whose payload is 0xBA.
    const BAD: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0xba];

    fn p_slice(i: u8) -> Vec<u8> {
        vec![0, 0, 0, 1, 0x41, 0x9a, 0x10 + i]
    }

    /// `n` pictures: the IDR with its parameter sets, then P slices.
    fn recording(n: u8) -> Vec<u8> {
        let mut out = [SPS, PPS, IDR].concat();
        for i in 1..n {
            out.extend(p_slice(i));
        }
        out
    }

    /// A decoder that outputs one 64x48 picture per unit after `delay`
    /// units, rejects [`BAD`], and has `pool` buffers for held frames.
    #[derive(Debug)]
    struct MockDecoder {
        delay: usize,
        pool: usize,
        live: Arc<AtomicUsize>,
        queued: VecDeque<u64>,
        configured: bool,
        corrupt: Vec<u64>,
    }

    impl MockDecoder {
        fn new(delay: usize, pool: usize) -> Self {
            Self {
                delay,
                pool,
                live: Arc::new(AtomicUsize::new(0)),
                queued: VecDeque::new(),
                configured: false,
                corrupt: Vec::new(),
            }
        }

        fn picture(&self, pts: u64) -> Event {
            struct Pooled(Arc<TensorDyn>, Arc<AtomicUsize>);
            impl TensorHolder for Pooled {
                fn tensor(&self) -> &TensorDyn {
                    &self.0
                }
            }
            impl Drop for Pooled {
                fn drop(&mut self) {
                    self.1.fetch_sub(1, Ordering::Relaxed);
                }
            }
            self.live.fetch_add(1, Ordering::Relaxed);
            let tensor = TensorDyn::image(
                64,
                48,
                PixelFormat::Nv12,
                DType::U8,
                Some(TensorMemory::Mem),
                CpuAccess::ReadWrite,
            )
            .unwrap();
            Event::Picture(Picture {
                holder: Box::new(Pooled(Arc::new(tensor), self.live.clone())),
                corrupted: self.corrupt.contains(&pts),
            })
        }
    }

    impl Decode for MockDecoder {
        fn decode(
            &mut self,
            unit: &[u8],
            pts: u64,
        ) -> std::result::Result<Vec<Event>, DecodeError> {
            if unit == BAD {
                return Err(DecodeError::Skip("damaged slice".into()));
            }
            if self.live.load(Ordering::Relaxed) >= self.pool {
                return Err(DecodeError::PoolExhausted);
            }
            let mut events = Vec::new();
            if !self.configured {
                self.configured = true;
                events.push(Event::Format(Format {
                    width: 64,
                    height: 48,
                    format: PixelFormat::Nv12,
                    colorimetry: None,
                    buffers: self.pool,
                    memory: ResolvedMemory::Export,
                }));
            }
            self.queued.push_back(pts);
            if self.queued.len() > self.delay {
                let pts = self.queued.pop_front().unwrap();
                events.push(self.picture(pts));
            }
            Ok(events)
        }

        fn flush(&mut self) -> std::result::Result<Vec<Event>, DecodeError> {
            let queued: Vec<_> = self.queued.drain(..).collect();
            Ok(queued.into_iter().map(|pts| self.picture(pts)).collect())
        }
    }

    const SIDECAR: &str = r#"{"version":1,"codec":"h264","fps":200,"width":64,"height":48,
        "colorimetry":{"color_space":"bt709","color_transfer":"bt709","color_encoding":"bt601","color_range":"limited"},
        "camera_info":{"frame_id":"camera"},"tf_static":{"child_frame_id":"camera"}}"#;

    fn camera_with(stream: Vec<u8>, builder: CameraBuilder, decoder: MockDecoder) -> FileCamera {
        let sidecar = Sidecar::parse(SIDECAR.as_bytes()).unwrap();
        let mut c = FileCamera::new(
            builder,
            PathBuf::from("clip.h264"),
            sidecar,
            Box::new(Cursor::new(stream)),
            |_| Ok(Box::new(decoder)),
        )
        .unwrap();
        c.start().unwrap();
        c
    }

    fn camera(stream: Vec<u8>, looping: bool) -> FileCamera {
        let builder = CameraBuilder::source("file:clip.h264")
            .unwrap()
            .looping(looping);
        camera_with(stream, builder, MockDecoder::new(0, 4))
    }

    const SECOND: Option<Duration> = Some(Duration::from_secs(1));

    #[test]
    fn plays_every_picture_then_ends() {
        let mut c = camera(recording(5), false);
        let seqs: Vec<u64> = (0..5)
            .map(|_| c.next_frame(SECOND).unwrap().seq())
            .collect();
        assert_eq!(seqs, [0, 1, 2, 3, 4]);
        let end = c.next_frame(SECOND).unwrap_err();
        assert_eq!(end.kind(), ErrorKind::EndOfStream);
        assert_eq!(
            c.next_frame(SECOND).unwrap_err().kind(),
            ErrorKind::EndOfStream
        );
        assert_eq!(c.stats().frames, 5);
    }

    #[test]
    fn looping_keeps_seq_monotonic_across_passes() {
        let mut c = camera(recording(3), true);
        let seqs: Vec<u64> = (0..10)
            .map(|_| c.next_frame(SECOND).unwrap().seq())
            .collect();
        assert_eq!(seqs, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn looping_a_file_with_no_pictures_ends_instead_of_spinning() {
        let mut c = camera(vec![0xff; 64], true);
        let err = c.next_frame(SECOND).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::EndOfStream, "{err}");
    }

    #[test]
    fn encoded_reproduces_the_file_byte_for_byte() {
        let stream = recording(6);
        let builder = CameraBuilder::source("file:clip.h264").unwrap();
        // A decoder that holds two pictures back for reordering.
        let mut c = camera_with(stream.clone(), builder, MockDecoder::new(2, 8));
        let mut forwarded = Vec::new();
        let mut frames = 0;
        while let Ok(f) = c.next_frame(SECOND) {
            frames += 1;
            if let Some(e) = f.encoded() {
                forwarded.extend_from_slice(e);
            }
        }
        assert_eq!(frames, 6);
        assert_eq!(forwarded, stream);
    }

    #[test]
    fn the_first_frame_carries_the_parameter_sets() {
        let mut c = camera(recording(2), false);
        let f = c.next_frame(SECOND).unwrap();
        assert_eq!(f.encoded().unwrap().as_ref(), [SPS, PPS, IDR].concat());
    }

    #[test]
    fn a_rejected_unit_is_skipped_and_playback_continues() {
        let mut stream = recording(2);
        stream.extend_from_slice(BAD);
        stream.extend(p_slice(7));
        let mut c = camera(stream, false);
        let frames = std::iter::from_fn(|| c.next_frame(SECOND).ok()).count();
        assert_eq!(frames, 3, "the units around the bad one still play");
        assert_eq!(c.stats().errors, 1);
    }

    #[test]
    fn garbage_between_recordings_resyncs() {
        let stream = [recording(2), vec![0xff; 100], recording(2)].concat();
        let mut c = camera(stream, false);
        assert_eq!(std::iter::from_fn(|| c.next_frame(SECOND).ok()).count(), 4);
    }

    #[test]
    fn a_damaged_picture_is_counted_not_delivered() {
        let builder = CameraBuilder::source("file:clip.h264").unwrap();
        let mut decoder = MockDecoder::new(0, 4);
        decoder.corrupt = vec![1];
        let mut c = camera_with(recording(3), builder, decoder);
        let frames: Vec<_> = std::iter::from_fn(|| c.next_frame(SECOND).ok()).collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(c.stats().errors, 1);
        let total: usize = frames
            .iter()
            .filter_map(|f| f.encoded())
            .map(|e| e.len())
            .sum();
        assert_eq!(
            total,
            recording(3).len(),
            "the damaged picture's unit is still forwarded"
        );
    }

    #[test]
    fn holding_every_frame_times_out_and_dropping_one_resumes() {
        let builder = CameraBuilder::source("file:clip.h264").unwrap();
        let mut c = camera_with(recording(4), builder, MockDecoder::new(0, 2));
        let a = c.next_frame(SECOND).unwrap();
        let b = c.next_frame(SECOND).unwrap();
        assert_eq!(c.stats().held, 2);
        let err = c.next_frame(Some(Duration::from_millis(20))).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Timeout);
        drop(a);
        let next = c.next_frame(SECOND).unwrap();
        assert_eq!(next.seq(), 2, "no unit was lost while blocked");
        drop((b, next));
        assert_eq!(c.stats().held, 0);
    }

    #[test]
    fn config_comes_from_the_sidecar_then_the_decoder() {
        let mut c = camera(recording(2), false);
        assert_eq!(c.config().frame_rate, Some(200.0));
        assert_eq!(
            c.config().colorimetry.and_then(|c| c.range),
            Some(edgefirst_tensor::ColorRange::Limited)
        );
        let f = c.next_frame(SECOND).unwrap();
        assert_eq!(c.config().memory, ResolvedMemory::Export);
        assert_eq!((c.config().width, c.config().height), (64, 48));
        assert_eq!(f.planes().len(), 2);
        assert_eq!(f.planes()[1].offset, 64 * 48, "chroma follows luma");
        let extras: serde_json::Value = serde_json::from_str(c.extras().unwrap()).unwrap();
        assert_eq!(extras["camera_info"]["frame_id"], "camera");
    }

    #[test]
    fn frames_are_paced_at_the_requested_rate() {
        let builder = CameraBuilder::source("file:clip.h264")
            .unwrap()
            .frame_rate(50.0);
        let mut c = camera_with(recording(6), builder, MockDecoder::new(0, 8));
        let start = Instant::now();
        let stamps: Vec<i64> = (0..6)
            .map(|_| c.next_frame(SECOND).unwrap().timestamp().nanos)
            .collect();
        let elapsed = start.elapsed();
        // Five 20 ms intervals after the first frame, which is due at once.
        assert!(elapsed >= Duration::from_millis(100), "{elapsed:?}");
        let mean = (stamps[5] - stamps[0]) as f64 / 5.0 / 1e6;
        assert!((19.0..30.0).contains(&mean), "mean interval {mean} ms");
        assert_eq!(c.config().frame_rate, Some(50.0));
    }

    #[test]
    fn caller_buffers_are_refused_and_handed_back() {
        let pool: Vec<TensorDyn> = (0..2)
            .map(|_| {
                TensorDyn::image(
                    64,
                    48,
                    PixelFormat::Nv12,
                    DType::U8,
                    Some(TensorMemory::Mem),
                    CpuAccess::ReadWrite,
                )
                .unwrap()
            })
            .collect();
        let builder = CameraBuilder::source("file:clip.h264")
            .unwrap()
            .with_buffers(pool);
        let sidecar = Sidecar::parse(SIDECAR.as_bytes()).unwrap();
        let mut c = FileCamera::new(
            builder,
            PathBuf::from("clip.h264"),
            sidecar,
            Box::new(Cursor::new(recording(2))),
            |_| Ok(Box::new(MockDecoder::new(0, 4))),
        )
        .unwrap();
        let err = c.start().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::BuffersRejected);
        assert_eq!(c.take_buffers().len(), 2);
        c.start().unwrap();
        assert!(c.next_frame(SECOND).is_ok());
    }

    #[test]
    fn requests_the_source_cannot_honour() {
        let sidecar = || Sidecar::parse(SIDECAR.as_bytes()).unwrap();
        let open = |builder: CameraBuilder| {
            FileCamera::new(
                builder,
                PathBuf::from("clip.h264"),
                sidecar(),
                Box::new(Cursor::new(Vec::new())),
                |_| Ok(Box::new(MockDecoder::new(0, 4))),
            )
        };
        let b = || CameraBuilder::source("file:clip.h264").unwrap();
        assert_eq!(
            open(b().format(PixelFormat::Yuyv)).unwrap_err().kind(),
            ErrorKind::UnsupportedFormat
        );
        assert_eq!(
            open(b().buffers(1)).unwrap_err().kind(),
            ErrorKind::InvalidConfig
        );
        let c = open(b().size(1920, 1080)).unwrap();
        assert_eq!(
            (c.config().width, c.config().height),
            (64, 48),
            "plays at its size"
        );
    }

    #[test]
    fn pacer_spaces_frames_and_reanchors_after_a_stall() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut p = Pacer::new(100.0);
        assert_eq!(p.due(t0), t0, "the first frame is due at once");
        p.delivered(t0);
        assert_eq!(p.due(t0), t0 + ms(10));
        assert_eq!(
            p.due(t0 + ms(15)),
            t0 + ms(10),
            "slightly late keeps the cadence"
        );
        p.delivered(t0 + ms(10));
        assert_eq!(
            p.due(t0 + ms(31)),
            t0 + ms(31),
            "a stall re-anchors, no burst"
        );
        p.set_rate(50.0);
        assert_eq!(p.due(t0 + ms(40)), t0 + ms(40));
    }
}

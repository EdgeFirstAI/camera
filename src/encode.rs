// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

//! H.264 encoding of captured frames: the main stream and the 4K tiles.
//!
//! A stage feeds its encoder the captured tensor itself when the encoder
//! takes the capture format at the stream size (tiles through the encoder's
//! crop), and otherwise converts each frame with the image crate first:
//! scaling, mirroring the camera could not do, or a format the encoder does
//! not take.

use std::{
    collections::VecDeque,
    error::Error,
    sync::Arc,
    time::{Duration, Instant},
};

use edgefirst_camera::{Frame, Mirror, StreamConfig};
use edgefirst_codec::{
    video::{Bitrate, EncodedFrame, EncoderConfig, FrameOptions, VideoEncoder},
    CodecError,
};
use edgefirst_image::{ConvertOptions, Flip, ImageProcessor, ImageProcessorTrait, Rotation};
use edgefirst_schemas::builtin_interfaces::Time;
use edgefirst_tensor::{CpuAccess, DType, PixelFormat, Region, TensorDyn};
use tracing::{info, warn};

use crate::{args::H264Bitrate, exit::Failure};

/// A video encoder: the platform's, or a stand-in in tests.
pub(crate) trait Encode: Send {
    fn encode(
        &mut self,
        src: &TensorDyn,
        opts: &FrameOptions,
    ) -> Result<Option<EncodedFrame>, CodecError>;
    fn flush(&mut self) -> Result<Vec<EncodedFrame>, CodecError>;
}

impl Encode for VideoEncoder {
    fn encode(
        &mut self,
        src: &TensorDyn,
        opts: &FrameOptions,
    ) -> Result<Option<EncodedFrame>, CodecError> {
        VideoEncoder::encode(self, src, opts)
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>, CodecError> {
        VideoEncoder::flush(self)
    }
}

/// Opens an encoder for a configuration.
pub(crate) type OpenEncoder = fn(EncoderConfig) -> Result<Box<dyn Encode>, CodecError>;

/// The platform's hardware encoder.
pub(crate) fn open_hardware(config: EncoderConfig) -> Result<Box<dyn Encode>, CodecError> {
    let encoder = VideoEncoder::new(config)?;
    info!("H.264 encoder {}", encoder.device());
    Ok(Box::new(encoder))
}

/// The startup error for `plan` when its encoder does not open.
fn open_error(plan: &Plan, e: &CodecError) -> Box<dyn Error> {
    Box::new(Failure::of(e, format!("{}: {e}", plan.label)))
}

/// The encoder bitrate for a preset.
pub(crate) fn bitrate(preset: H264Bitrate) -> Bitrate {
    match preset {
        H264Bitrate::Auto => Bitrate::Auto,
        H264Bitrate::Mbps5 => Bitrate::Bps(5_000_000),
        H264Bitrate::Mbps25 => Bitrate::Bps(25_000_000),
        H264Bitrate::Mbps50 => Bitrate::Bps(50_000_000),
        H264Bitrate::Mbps100 => Bitrate::Bps(100_000_000),
    }
}

/// Convert options that apply `mirror`.
pub(crate) fn mirror_options(mirror: Option<Mirror>) -> ConvertOptions {
    match mirror {
        Some(Mirror::Horizontal) => ConvertOptions::new().flip(Flip::Horizontal),
        Some(Mirror::Vertical) => ConvertOptions::new().flip(Flip::Vertical),
        Some(Mirror::Both) => ConvertOptions::new().rotation(Rotation::Rotate180),
        _ => ConvertOptions::new(),
    }
}

/// Converts a scratch image of the capture's format and size once, so the
/// converter's first-use setup (GL shader compilation, about a second on
/// the i.MX 8M Plus) happens before capture begins rather than stalling it.
pub(crate) fn warm_up(
    processor: &mut ImageProcessor,
    capture: &StreamConfig,
    dst: &mut TensorDyn,
    options: ConvertOptions,
) -> Result<(), Box<dyn Error>> {
    let src = processor.create_image(
        capture.width as usize,
        capture.height as usize,
        capture.format,
        DType::U8,
        None,
        CpuAccess::ReadWrite,
    )?;
    processor.convert(&src, dst, Some(options))?;
    Ok(())
}

/// Formats a converted feed tries, in order: YUYV is half the bytes of
/// RGBA, and the i.MX 8M Plus G2D writes it.
const CONVERT_FORMATS: [PixelFormat; 2] = [PixelFormat::Yuyv, PixelFormat::Rgba];

/// The largest picture the encoders take for a tile.
pub(crate) const MAX_TILE: (u32, u32) = (1920, 1080);

/// Tile names, in topic order: top-left, top-right, bottom-left,
/// bottom-right.
pub(crate) const TILE_NAMES: [&str; 4] = ["tl", "tr", "bl", "br"];

/// The region of a `width`x`height` capture that tile `index` shows, in
/// the published (mirrored) picture. Mirroring moves a tile to the opposite
/// side, so a software-mirrored tile is encoded from the region its mirror
/// image covers.
pub(crate) fn tile_region(index: usize, width: u32, height: u32, mirror: Option<Mirror>) -> Region {
    let flipped = match mirror {
        Some(Mirror::Horizontal) => index ^ 1,
        Some(Mirror::Vertical) => index ^ 2,
        Some(Mirror::Both) => index ^ 3,
        _ => index,
    };
    let (tw, th) = tile_size(width, height);
    Region {
        x: (flipped % 2) * tw as usize,
        y: (flipped / 2) * th as usize,
        width: tw as usize,
        height: th as usize,
    }
}

/// The size of one quadrant of a capture, rounded down to even.
fn tile_size(width: u32, height: u32) -> (u32, u32) {
    ((width / 2) & !1, (height / 2) & !1)
}

/// The picture size a tile is encoded at: the quadrant itself, or the
/// largest tile size when the quadrant is larger.
pub(crate) fn tile_output(width: u32, height: u32) -> (u32, u32) {
    let (tw, th) = tile_size(width, height);
    if tw <= MAX_TILE.0 && th <= MAX_TILE.1 {
        (tw, th)
    } else {
        MAX_TILE
    }
}

/// Minimum spacing between encoded tile frames for a requested FPS limit;
/// `0` means no limit. Nanoseconds, because milliseconds truncate 1000/15
/// to 66.
pub(crate) fn tile_frame_interval(fps: u32) -> Duration {
    match fps {
        0 => Duration::ZERO,
        fps => Duration::from_nanos(1_000_000_000 / u64::from(fps)),
    }
}

/// Decides which captured frames a tile encoder encodes to hit a rate.
///
/// "Encode if `interval` has passed since the last encoded frame" loses
/// frames systematically: any jitter pushes the next deadline later, and the
/// error accumulates. Advancing the deadline by exactly one interval keeps
/// it phase locked, so a frame arriving a hair early is picked up by the
/// following deadline.
#[derive(Debug)]
pub(crate) struct TilePacer {
    interval: Duration,
    next: Option<Instant>,
}

impl TilePacer {
    pub(crate) fn new(fps: u32) -> Self {
        Self {
            interval: tile_frame_interval(fps),
            next: None,
        }
    }

    /// Whether the frame presented at `now` should be encoded.
    pub(crate) fn accept(&mut self, now: Instant) -> bool {
        if self.interval.is_zero() {
            return true;
        }
        match self.next {
            None => {
                self.next = Some(now + self.interval);
                true
            }
            Some(next) if now >= next => {
                let advanced = next + self.interval;
                // After a stall, resync rather than emit the backlog as a
                // burst.
                self.next = Some(if now >= advanced {
                    now + self.interval
                } else {
                    advanced
                });
                true
            }
            _ => false,
        }
    }
}

/// What a stage encodes.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    /// Name for logs.
    pub(crate) label: String,
    /// Encoded picture size.
    pub(crate) size: (u32, u32),
    /// Part of the capture to encode; `None` for all of it.
    pub(crate) region: Option<Region>,
    /// Mirroring to apply.
    pub(crate) mirror: Option<Mirror>,
    pub(crate) frame_rate: f64,
    pub(crate) bitrate: Bitrate,
    /// Frame rate limit (tiles); 0 for none.
    pub(crate) limit_fps: u32,
}

/// How frames reach the encoder.
enum Feed {
    /// The captured tensor, cropped by the encoder.
    Direct { crop: Option<Region> },
    /// Converted first.
    Convert(Box<Converter>),
}

/// A converted feed's converter and the image it writes.
struct Converter {
    processor: ImageProcessor,
    dst: TensorDyn,
    options: ConvertOptions,
}

/// One H.264 stream: an encoder and how to feed it.
pub(crate) struct Stage {
    plan: Plan,
    capture: StreamConfig,
    open: OpenEncoder,
    encoder: Box<dyn Encode>,
    feed: Feed,
    pacer: TilePacer,
    /// Stamps of the frames whose access unit has not come back, by pts.
    stamps: VecDeque<(u64, Time)>,
    /// An access unit has come back.
    produced: bool,
}

/// Stamps kept for access units the encoder still owes.
const MAX_PENDING: usize = 64;

impl Stage {
    /// Opens the stage's encoder, directly on the capture when it can.
    ///
    /// # Errors
    ///
    /// No encoder, or none that takes the capture format or a converted
    /// one.
    pub(crate) fn new(
        plan: Plan,
        capture: &StreamConfig,
        open: OpenEncoder,
    ) -> Result<Self, Box<dyn Error>> {
        let source = plan.region.map_or((capture.width, capture.height), |r| {
            (r.width as u32, r.height as u32)
        });
        if plan.mirror.is_none() && source == plan.size {
            match open(Self::config(&plan, capture.format)) {
                Ok(encoder) => {
                    let feed = Feed::Direct { crop: plan.region };
                    return Ok(Self::with(plan, capture, open, encoder, feed));
                }
                Err(CodecError::UnsupportedFormat(_)) => {}
                Err(e) => return Err(open_error(&plan, &e)),
            }
        }
        let (encoder, feed) = Self::converted(&plan, capture, open)?;
        Ok(Self::with(plan, capture, open, encoder, feed))
    }

    fn with(
        plan: Plan,
        capture: &StreamConfig,
        open: OpenEncoder,
        encoder: Box<dyn Encode>,
        feed: Feed,
    ) -> Self {
        let capture_format = capture.format;
        let how = match &feed {
            Feed::Direct { .. } => format!("{capture_format} directly"),
            Feed::Convert(c) => format!(
                "{capture_format} converted to {}",
                c.dst.format().map_or("?".to_string(), |f| f.to_string())
            ),
        };
        info!(
            "{}: {}x{} at {} fps from {how}",
            plan.label, plan.size.0, plan.size.1, plan.frame_rate
        );
        Self {
            pacer: TilePacer::new(plan.limit_fps),
            capture: capture.clone(),
            plan,
            open,
            encoder,
            feed,
            stamps: VecDeque::new(),
            produced: false,
        }
    }

    fn config(plan: &Plan, format: PixelFormat) -> EncoderConfig {
        let mut c = EncoderConfig::h264(plan.size.0, plan.size.1, format, plan.frame_rate);
        c.bitrate = plan.bitrate;
        c
    }

    /// An encoder on a converted feed.
    fn converted(
        plan: &Plan,
        capture: &StreamConfig,
        open: OpenEncoder,
    ) -> Result<(Box<dyn Encode>, Feed), Box<dyn Error>> {
        let mut last = None;
        for format in CONVERT_FORMATS {
            match open(Self::config(plan, format)) {
                Ok(encoder) => {
                    let mut processor = ImageProcessor::new()?;
                    let mut dst = processor.create_image(
                        plan.size.0 as usize,
                        plan.size.1 as usize,
                        format,
                        DType::U8,
                        None,
                        CpuAccess::ReadWrite,
                    )?;
                    let options = mirror_options(plan.mirror).source(plan.region);
                    warm_up(&mut processor, capture, &mut dst, options)?;
                    let converter = Converter {
                        processor,
                        dst,
                        options,
                    };
                    return Ok((encoder, Feed::Convert(Box::new(converter))));
                }
                Err(e @ CodecError::UnsupportedFormat(_)) => last = Some(e),
                Err(e) => return Err(open_error(plan, &e)),
            }
        }
        Err(format!(
            "{}: no encoder takes {CONVERT_FORMATS:?}: {}",
            plan.label,
            last.map_or_else(String::new, |e| e.to_string())
        )
        .into())
    }

    pub(crate) fn label(&self) -> &str {
        &self.plan.label
    }

    /// Whether the frame presented now is within the stage's rate limit.
    pub(crate) fn accept(&mut self, now: Instant) -> bool {
        self.pacer.accept(now)
    }

    /// Encodes `frame`, releasing it as soon as the encoder no longer needs
    /// it, and returns the access units ready, each with its frame's stamp.
    pub(crate) fn encode(
        &mut self,
        frame: Arc<Frame>,
        stamp: Time,
    ) -> Result<Vec<(EncodedFrame, Time)>, Box<dyn Error>> {
        let pts = frame.seq();
        self.stamps.push_back((pts, stamp));
        while self.stamps.len() > MAX_PENDING {
            self.stamps.pop_front();
        }
        let au = match self.encode_frame(&frame, pts) {
            Ok(au) => au,
            // A device that cannot express the crop or the capture layout
            // takes converted frames instead, if it never produced any.
            Err(e) if matches!(self.feed, Feed::Direct { .. }) && !self.produced => {
                warn!(
                    "{}: encoding {} directly failed ({e}); converting frames first",
                    self.plan.label, self.capture.format
                );
                let (encoder, feed) = Self::converted(&self.plan, &self.capture, self.open)?;
                self.encoder = encoder;
                self.feed = feed;
                self.encode_frame(&frame, pts)?
            }
            Err(e) => return Err(e),
        };
        drop(frame);
        Ok(au.into_iter().map(|au| self.stamped(au)).collect())
    }

    fn encode_frame(
        &mut self,
        frame: &Frame,
        pts: u64,
    ) -> Result<Option<EncodedFrame>, Box<dyn Error>> {
        let mut opts = FrameOptions {
            pts,
            ..FrameOptions::default()
        };
        Ok(match &mut self.feed {
            Feed::Direct { crop } => {
                opts.crop = *crop;
                self.encoder.encode(frame.tensor(), &opts)?
            }
            Feed::Convert(c) => {
                c.processor
                    .convert(frame.tensor(), &mut c.dst, Some(c.options))?;
                self.encoder.encode(&c.dst, &opts)?
            }
        })
    }

    /// Drains the encoder at the end of the stream.
    pub(crate) fn flush(&mut self) -> Result<Vec<(EncodedFrame, Time)>, Box<dyn Error>> {
        let out = self.encoder.flush()?;
        Ok(out.into_iter().map(|au| self.stamped(au)).collect())
    }

    /// `au` with the stamp of the frame it encodes. Stamps of frames the
    /// encoder skipped are dropped on the way.
    fn stamped(&mut self, au: EncodedFrame) -> (EncodedFrame, Time) {
        self.produced = true;
        while let Some(&(pts, stamp)) = self.stamps.front() {
            if pts > au.pts {
                break;
            }
            self.stamps.pop_front();
            if pts == au.pts {
                return (au, stamp);
            }
        }
        warn!(
            "{}: no stamp for access unit {}; stamping it now",
            self.plan.label, au.pts
        );
        (au, edgefirst_camera::schema::now())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use edgefirst_camera::CameraBuilder;
    use std::sync::Mutex;

    /// A stand-in encoder: each access unit is the frame's pts, returned one
    /// frame late as a reordering device would.
    pub(crate) struct FakeEncoder {
        held: Option<u64>,
    }

    /// Configurations the fake was opened with, across a test binary.
    pub(crate) static OPENED: Mutex<Vec<(u32, u32, PixelFormat)>> = Mutex::new(Vec::new());

    impl Encode for FakeEncoder {
        fn encode(
            &mut self,
            _src: &TensorDyn,
            opts: &FrameOptions,
        ) -> Result<Option<EncodedFrame>, CodecError> {
            Ok(self.held.replace(opts.pts).map(au))
        }

        fn flush(&mut self) -> Result<Vec<EncodedFrame>, CodecError> {
            Ok(self.held.take().map(au).into_iter().collect())
        }
    }

    fn au(pts: u64) -> EncodedFrame {
        EncodedFrame {
            data: pts.to_le_bytes().to_vec(),
            keyframe: pts.is_multiple_of(30),
            pts,
        }
    }

    pub(crate) fn open_fake(config: EncoderConfig) -> Result<Box<dyn Encode>, CodecError> {
        OPENED
            .lock()
            .unwrap()
            .push((config.width, config.height, config.input_format));
        Ok(Box::new(FakeEncoder { held: None }))
    }

    fn open_rgba_only(config: EncoderConfig) -> Result<Box<dyn Encode>, CodecError> {
        if config.input_format != PixelFormat::Rgba {
            return Err(CodecError::UnsupportedFormat(config.input_format));
        }
        Ok(Box::new(FakeEncoder { held: None }))
    }

    fn open_none(_: EncoderConfig) -> Result<Box<dyn Encode>, CodecError> {
        Err(CodecError::NoDevice("none here".into()))
    }

    fn plan(size: (u32, u32), region: Option<Region>, mirror: Option<Mirror>) -> Plan {
        Plan {
            label: "h264".into(),
            size,
            region,
            mirror,
            frame_rate: 30.0,
            bitrate: Bitrate::Auto,
            limit_fps: 0,
        }
    }

    fn mock(size: &str) -> Box<dyn edgefirst_camera::Camera> {
        let mut c = CameraBuilder::source(&format!("mock:{size}@120"))
            .unwrap()
            .open()
            .unwrap();
        c.start().unwrap();
        c
    }

    fn is_direct(s: &Stage) -> bool {
        matches!(s.feed, Feed::Direct { .. })
    }

    #[test]
    fn encodes_the_capture_directly_at_the_stream_size() {
        let cam = mock("64x48");
        let s = Stage::new(plan((64, 48), None, None), cam.config(), open_fake).unwrap();
        assert!(is_direct(&s));
    }

    #[test]
    fn converts_to_scale_or_mirror() {
        let cam = mock("64x48");
        let scaled = Stage::new(plan((32, 24), None, None), cam.config(), open_fake).unwrap();
        assert!(!is_direct(&scaled));
        let mirrored = Stage::new(
            plan((64, 48), None, Some(Mirror::Both)),
            cam.config(),
            open_fake,
        )
        .unwrap();
        assert!(!is_direct(&mirrored));
    }

    #[test]
    fn converts_to_a_format_the_encoder_takes() {
        let cam = mock("64x48");
        let s = Stage::new(plan((64, 48), None, None), cam.config(), open_rgba_only).unwrap();
        let Feed::Convert(c) = &s.feed else {
            panic!("expected a converted feed")
        };
        assert_eq!(c.dst.format(), Some(PixelFormat::Rgba));
    }

    #[test]
    fn no_encoder_is_an_error() {
        let cam = mock("64x48");
        // Direct, then scaled through a converter.
        for size in [(64, 48), (32, 24)] {
            let err = Stage::new(plan(size, None, None), cam.config(), open_none)
                .err()
                .unwrap();
            assert_eq!(
                crate::exit::status(&*err),
                crate::exit::Status::CodecUnavailable
            );
            assert!(err.to_string().contains("none here"), "{err}");
        }
    }

    /// Access units come back late; each still carries its own frame's
    /// stamp, and flushing returns the last.
    #[test]
    fn access_units_carry_their_frames_stamps() {
        let mut cam = mock("64x48");
        let mut s = Stage::new(plan((64, 48), None, None), cam.config(), open_fake).unwrap();
        let mut out = Vec::new();
        for i in 0..5 {
            let frame = cam.next_frame(Some(Duration::from_secs(1))).unwrap();
            let stamp = Time {
                sec: 100 + i,
                nanosec: frame.seq() as u32,
            };
            out.extend(s.encode(Arc::new(frame), stamp).unwrap());
        }
        out.extend(s.flush().unwrap());
        assert_eq!(out.len(), 5);
        for (au, stamp) in out {
            assert_eq!(u64::from(stamp.nanosec), au.pts);
        }
    }

    #[test]
    fn tiles_cover_the_quadrants() {
        let r = |i| tile_region(i, 3840, 2160, None);
        assert_eq!(
            (r(0).x, r(0).y, r(0).width, r(0).height),
            (0, 0, 1920, 1080)
        );
        assert_eq!((r(1).x, r(1).y), (1920, 0));
        assert_eq!((r(2).x, r(2).y), (0, 1080));
        assert_eq!((r(3).x, r(3).y), (1920, 1080));
        assert_eq!(tile_output(3840, 2160), (1920, 1080));
        assert_eq!(tile_output(2592, 1944), (1296, 972));
        assert_eq!(tile_output(8192, 4320), MAX_TILE);
        assert_eq!(tile_output(641, 481), (320, 240));
    }

    #[test]
    fn mirrored_tiles_come_from_the_opposite_quadrant() {
        let top_left = |m| {
            let r = tile_region(0, 3840, 2160, m);
            (r.x, r.y)
        };
        assert_eq!(top_left(Some(Mirror::Horizontal)), (1920, 0));
        assert_eq!(top_left(Some(Mirror::Vertical)), (0, 1080));
        assert_eq!(top_left(Some(Mirror::Both)), (1920, 1080));
    }

    /// A source at exactly `source_fps`; how many frames a pacer at
    /// `target_fps` accepts over `seconds`.
    fn paced_frame_count(source_fps: u32, target_fps: u32, seconds: u32) -> u32 {
        let mut pacer = TilePacer::new(target_fps);
        let t0 = Instant::now();
        let step = Duration::from_nanos(1_000_000_000 / u64::from(source_fps));
        (0..source_fps * seconds)
            .filter(|k| pacer.accept(t0 + step * *k))
            .count() as u32
    }

    #[test]
    fn tile_pacer_hits_15fps_from_a_30fps_source() {
        let n = paced_frame_count(30, 15, 10);
        assert!((148..=152).contains(&n), "expected ~150 frames, got {n}");
    }

    #[test]
    fn tile_pacer_hits_10fps_from_a_30fps_source() {
        let n = paced_frame_count(30, 10, 10);
        assert!((98..=102).contains(&n), "expected ~100 frames, got {n}");
    }

    #[test]
    fn tile_pacer_accepts_everything_when_unlimited() {
        assert_eq!(paced_frame_count(30, 0, 2), 60);
    }

    #[test]
    fn tile_pacer_does_not_burst_after_a_stall() {
        let mut pacer = TilePacer::new(15);
        let t0 = Instant::now();
        assert!(pacer.accept(t0));
        assert!(pacer.accept(t0 + Duration::from_secs(1)));
        assert!(!pacer.accept(t0 + Duration::from_secs(1) + Duration::from_millis(1)));
    }

    #[test]
    fn tile_frame_interval_treats_zero_as_no_limit() {
        assert_eq!(tile_frame_interval(0), Duration::ZERO);
        assert_eq!(tile_frame_interval(10), Duration::from_millis(100));
        assert_eq!(tile_frame_interval(15), Duration::from_nanos(66_666_666));
    }

    #[test]
    fn bitrate_presets_map_to_bits_per_second() {
        assert_eq!(bitrate(H264Bitrate::Auto), Bitrate::Auto);
        assert_eq!(bitrate(H264Bitrate::Mbps25), Bitrate::Bps(25_000_000));
    }
}

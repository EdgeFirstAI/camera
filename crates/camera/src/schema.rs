// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! `Frame` → `edgefirst_msgs/CameraFrame` mapping (feature `schemas`).
//!
//! The one definition of how a captured frame becomes a `CameraFrame`
//! message, for the EdgeFirst camera service and for third-party publishers
//! alike:
//!
//! ```no_run
//! # fn publish(camera: &mut dyn edgefirst_camera::Camera) -> edgefirst_camera::Result<()> {
//! use edgefirst_camera::schema::{self, FrameTensor};
//! use edgefirst_schemas::edgefirst_msgs::CameraFrame;
//!
//! let frame = camera.next_frame(None)?;
//! let tensor = FrameTensor::new(&frame, camera.config().colorimetry.as_ref())?;
//! let stamp = schema::stamp(&frame).unwrap_or_else(schema::now);
//! let mut cdr = Vec::new();
//! tensor
//!     .with_fields(|fields| {
//!         CameraFrame::builder()
//!             .stamp(stamp)
//!             .frame_id("camera")
//!             .seq(frame.seq())
//!             .tensor(fields)
//!             .encode_into_vec(&mut cdr)
//!     })
//!     .expect("a valid CameraFrame");
//! // Publish `cdr` with the sample timestamp `schema::ntp64(stamp)`.
//! # Ok(()) }
//! ```
//!
//! | Message field | Source |
//! |---|---|
//! | `header.stamp` | [`stamp`]: [`Frame::realtime`], before-epoch times clamped to the epoch and times past the ROS 2 `Time` range saturated |
//! | Zenoh sample timestamp | [`ntp64`] of the same `Time` |
//! | `seq` | [`Frame::seq`] |
//! | `storage_kind`, `dtype` | `TensorMemory::code()` and `DType::code()`, the HAL tensor-ABI codes (`EfStorageKind`, `EfDtype`) |
//! | `pid` | this process |
//! | `shape` | the format's addressing grid (`PixelFormat::addressing_shape`), or the tensor shape for an unformatted tensor |
//! | `strides` | `edgefirst_tensor::protocol::c_byte_strides` at the frame's row stride |
//! | `format` | the format's FourCC, empty for an unformatted tensor |
//! | `color_*` | the stream colorimetry; empty when unspecified |
//! | `planes` | one plane per [`Frame::planes`] entry |
//! | `fence_fd`, `modifier` | `-1` and `0` |
//!
//! Planes with a handle (a DMA-BUF fd) are sent by reference. A frame in
//! process memory has no handle another process can open, so its planes are
//! copied inline into the message.

use std::borrow::Cow;
use std::time::{SystemTime, UNIX_EPOCH};

use edgefirst_schemas::builtin_interfaces::Time;
use edgefirst_schemas::tensor::{TensorFields, TensorPlaneView};
use edgefirst_tensor::{Colorimetry, CpuAccess, DType, PixelFormat, TensorMemory};

use crate::{Error, ErrorKind, Frame, PlaneLayout, Result};

/// The latest `Time` a ROS 2 header can carry; later instants saturate to
/// it.
pub const SATURATED_TIME: Time = Time {
    sec: i32::MAX,
    nanosec: 999_999_999,
};

/// The header stamp for `frame`: its acquisition time, or `None` when the
/// capture clock could not be converted to wall-clock time.
pub fn stamp(frame: &Frame) -> Option<Time> {
    frame.realtime().map(time)
}

/// The current wall-clock time as a header stamp, for frames without one.
pub fn now() -> Time {
    time(SystemTime::now())
}

/// A wall-clock instant as a ROS 2 `Time`. Instants before the Unix epoch
/// clamp to the epoch and instants past [`SATURATED_TIME`] saturate to it,
/// so the header stamp and the Zenoh timestamp from [`ntp64`] always denote
/// the same instant.
pub fn time(t: SystemTime) -> Time {
    let Ok(d) = t.duration_since(UNIX_EPOCH) else {
        return Time { sec: 0, nanosec: 0 };
    };
    match i32::try_from(d.as_secs()) {
        Ok(sec) => Time {
            sec,
            nanosec: d.subsec_nanos(),
        },
        Err(_) => SATURATED_TIME,
    }
}

/// The Zenoh sample timestamp for a header stamp: NTP64, 32-bit seconds and
/// a 32-bit binary fraction since the Unix epoch, computed as Zenoh's
/// `NTP64::from(Duration)` does. The fraction quantises to 2⁻³² s, so the
/// decoded value is within 1 ns of `stamp`. A negative `sec` (never produced
/// by [`time`]) is treated as the epoch.
pub fn ntp64(stamp: Time) -> u64 {
    if stamp.sec < 0 {
        return 0;
    }
    let secs = stamp.sec as u64;
    let nanos = u64::from(stamp.nanosec.min(999_999_999));
    (secs << 32) + ((nanos << 32) / 1_000_000_000)
}

/// The tensor of a `CameraFrame` built from a [`Frame`].
///
/// Owns everything [`TensorFields`] borrows; [`with_fields`](Self::with_fields)
/// lends the fields to an encoder.
#[derive(Debug, Clone)]
pub struct FrameTensor {
    pub(crate) storage_kind: u32,
    pub(crate) pid: u32,
    pub(crate) fence_fd: i32,
    pub(crate) dtype: u32,
    pub(crate) shape: Vec<u64>,
    pub(crate) strides: Vec<i64>,
    pub(crate) format: String,
    pub(crate) color: [&'static str; 4],
    pub(crate) planes: Vec<Plane>,
}

#[derive(Debug, Clone)]
pub(crate) struct Plane {
    pub(crate) layout: PlaneLayout,
    /// The plane's bytes when it travels inline.
    pub(crate) data: Option<Vec<u8>>,
}

impl FrameTensor {
    /// Maps `frame`, with the stream's colorimetry (`Camera::config()`).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Tensor`] when the format cannot address the frame's size,
    /// or a frame in process memory cannot be mapped for reading.
    pub fn new(frame: &Frame, colorimetry: Option<&Colorimetry>) -> Result<Self> {
        let tensor = frame.tensor();
        let geometry = match (tensor.format(), tensor.width(), tensor.height()) {
            (Some(format), Some(width), Some(height)) => Geometry::Image {
                format,
                width,
                height,
                row_stride: tensor.effective_row_stride(),
            },
            _ => Geometry::Raw(tensor.shape().to_vec()),
        };
        let mut planes: Vec<Plane> = frame
            .planes()
            .iter()
            .map(|&layout| Plane { layout, data: None })
            .collect();
        if planes.iter().any(|p| p.layout.handle < 0) {
            let bytes = tensor.map_bytes(CpuAccess::Read)?;
            for p in &mut planes {
                let l = &p.layout;
                let range = l.offset..l.offset + l.size;
                let data = bytes.get(range).ok_or_else(|| {
                    Error::new(
                        ErrorKind::Tensor,
                        format!(
                            "plane at {}+{} lies outside the {}-byte frame",
                            l.offset,
                            l.size,
                            bytes.len()
                        ),
                    )
                })?;
                p.data = Some(data.to_vec());
            }
        }
        Self::from_parts(
            tensor.memory(),
            tensor.dtype(),
            &geometry,
            planes,
            colorimetry,
        )
    }

    pub(crate) fn from_parts(
        memory: TensorMemory,
        dtype: DType,
        geometry: &Geometry,
        planes: Vec<Plane>,
        colorimetry: Option<&Colorimetry>,
    ) -> Result<Self> {
        let (shape, strides, format) = match geometry {
            Geometry::Image {
                format,
                width,
                height,
                row_stride,
            } => {
                let shape: Vec<u64> = format
                    .addressing_shape(*width, *height)
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::Tensor,
                            format!("{format:?} cannot address a {width}x{height} image"),
                        )
                    })?
                    .into_iter()
                    .map(|d| d as u64)
                    .collect();
                let strides = edgefirst_tensor::protocol::c_byte_strides(
                    &shape,
                    dtype.size() as i64,
                    Some(format.layout()),
                    *row_stride,
                );
                (shape, strides, fourcc(*format))
            }
            Geometry::Raw(shape) => {
                let shape: Vec<u64> = shape.iter().map(|&d| d as u64).collect();
                let strides = edgefirst_tensor::protocol::c_byte_strides(
                    &shape,
                    dtype.size() as i64,
                    None,
                    None,
                );
                (shape, strides, String::new())
            }
        };
        // A message without a format carries no colorimetry.
        let color = match colorimetry {
            Some(c) if !format.is_empty() => [
                c.space.map_or("", |v| v.as_str()),
                c.transfer.map_or("", |v| v.as_str()),
                c.encoding.map_or("", |v| v.as_str()),
                c.range.map_or("", |v| v.as_str()),
            ],
            _ => [""; 4],
        };
        Ok(Self {
            storage_kind: memory.code(),
            pid: std::process::id(),
            fence_fd: -1,
            dtype: dtype.code(),
            shape,
            strides,
            format,
            color,
            planes,
        })
    }

    /// Lends the message's tensor fields to `f`, typically an encoder.
    pub fn with_fields<R>(&self, f: impl FnOnce(&TensorFields<'_>) -> R) -> R {
        let planes: Vec<TensorPlaneView<'_>> = self
            .planes
            .iter()
            .map(|p| {
                let l = &p.layout;
                TensorPlaneView {
                    handle: if p.data.is_some() { -1 } else { l.handle },
                    offset: if p.data.is_some() { 0 } else { l.offset as u64 },
                    stride: l.stride as u64,
                    size: l.size as u64,
                    used: if p.data.is_some() {
                        l.size as u64
                    } else {
                        l.used.min(l.size) as u64
                    },
                    modifier: if p.data.is_some() { 0 } else { l.modifier },
                    handle_bytes: &[],
                    data: p.data.as_deref().unwrap_or(&[]),
                }
            })
            .collect();
        let fields = TensorFields {
            storage_kind: self.storage_kind,
            pid: self.pid,
            fence_fd: self.fence_fd,
            dtype: self.dtype,
            quant_axis: -2,
            shape: &self.shape,
            strides: &self.strides,
            quant_scales: &[],
            quant_zero_points: &[],
            format: Cow::Borrowed(&self.format),
            color_space: Cow::Borrowed(self.color[0]),
            color_transfer: Cow::Borrowed(self.color[1]),
            color_encoding: Cow::Borrowed(self.color[2]),
            color_range: Cow::Borrowed(self.color[3]),
            planes: &planes,
        };
        f(&fields)
    }
}

/// What a frame's tensor addresses.
#[derive(Debug, Clone)]
pub(crate) enum Geometry {
    Image {
        format: PixelFormat,
        width: usize,
        height: usize,
        row_stride: Option<usize>,
    },
    Raw(Vec<usize>),
}

/// The FourCC of `format` as text, or the format's name for one without a
/// FourCC (planar RGB).
fn fourcc(format: PixelFormat) -> String {
    format.to_string()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use edgefirst_schemas::edgefirst_msgs::CameraFrame;
    use edgefirst_tensor::{ColorEncoding, ColorRange, ColorSpace, ColorTransfer};

    use super::*;

    // Copied from EdgeFirstAI/schemas testdata/cdr/edgefirst_msgs, generated by
    // scripts/generate_cdr_testdata.py with an independent CDR encoder.
    const GOLDEN_CAMERA_FRAME: &[u8] = include_bytes!("../tests/data/schemas/CameraFrame.cdr");
    const GOLDEN_SPLIT_FD: &[u8] = include_bytes!("../tests/data/schemas/CameraFrame_split_fd.cdr");
    const GOLDEN_STAMP: Time = Time {
        sec: 1_234_567_890,
        nanosec: 123_456_789,
    };
    const GOLDEN_FRAME_ID: &str = "test_frame";
    /// The goldens carry `dtype = 1` for 8-bit image samples. Under the
    /// tensor ABI, which `Tensor.msg` defers to, 1 is `I8`; a camera frame
    /// is `U8` (0).
    const GOLDEN_DTYPE: u32 = 1;

    fn bt709_limited() -> Colorimetry {
        Colorimetry {
            space: Some(ColorSpace::Bt709),
            transfer: Some(ColorTransfer::Bt709),
            encoding: Some(ColorEncoding::Bt709),
            range: Some(ColorRange::Limited),
        }
    }

    fn plane(handle: i64, offset: usize, stride: usize, size: usize) -> Plane {
        Plane {
            layout: PlaneLayout::new(handle, offset, stride, size),
            data: None,
        }
    }

    fn encode(t: &FrameTensor, stamp: Time, seq: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        t.with_fields(|f| {
            f.validate().expect("valid tensor fields");
            CameraFrame::builder()
                .stamp(stamp)
                .frame_id(GOLDEN_FRAME_ID)
                .seq(seq)
                .tensor(f)
                .encode_into_vec(&mut buf)
        })
        .expect("encode");
        buf
    }

    /// NV12M export: one DMA-BUF per plane, as `V4L2_PIX_FMT_NV12M` delivers.
    #[test]
    fn nv12m_matches_the_split_fd_golden() {
        let mut t = FrameTensor::from_parts(
            TensorMemory::DmaBuf,
            DType::U8,
            &Geometry::Image {
                format: PixelFormat::Nv12,
                width: 1920,
                height: 1080,
                row_stride: Some(1920),
            },
            vec![
                plane(70, 0, 1920, 1920 * 1080),
                plane(71, 0, 1920, 1920 * 1080 / 2),
            ],
            Some(&bt709_limited()),
        )
        .unwrap();
        assert_eq!(t.dtype, DType::U8.code());
        assert_eq!(t.fence_fd, -1);
        // The golden's producer-specific values: its pid, its dtype code and
        // a GPU fence the SDK does not produce.
        t.pid = 1234;
        t.dtype = GOLDEN_DTYPE;
        t.fence_fd = 77;
        assert_eq!(encode(&t, GOLDEN_STAMP, 5), GOLDEN_SPLIT_FD);
    }

    /// Single-buffer NV12, as `V4L2_PIX_FMT_NV12` delivers: chroma follows
    /// luma in the same DMA-BUF. The golden also carries opaque
    /// `handle_bytes`, which a V4L2 frame never has, so this compares every
    /// other field.
    #[test]
    fn nv12_matches_the_single_buffer_golden() {
        let golden = CameraFrame::from_cdr(GOLDEN_CAMERA_FRAME).unwrap();
        let t = FrameTensor::from_parts(
            TensorMemory::DmaBuf,
            DType::U8,
            &Geometry::Image {
                format: PixelFormat::Nv12,
                width: 640,
                height: 480,
                row_stride: Some(640),
            },
            vec![
                plane(7, 0, 640, 640 * 480),
                plane(7, 640 * 480, 640, 640 * 480 / 2),
            ],
            Some(&bt709_limited()),
        )
        .unwrap();
        let ours = encode(&t, GOLDEN_STAMP, golden.seq());
        let ours = CameraFrame::from_cdr(ours.as_slice()).unwrap();
        let (a, b) = (ours.tensor(), golden.tensor());
        assert_eq!(a.storage_kind(), b.storage_kind());
        assert_eq!(a.fence_fd(), b.fence_fd());
        assert_eq!(a.quant_axis(), b.quant_axis());
        assert_eq!(a.shape().collect::<Vec<_>>(), b.shape().collect::<Vec<_>>());
        assert_eq!(
            a.strides().collect::<Vec<_>>(),
            b.strides().collect::<Vec<_>>()
        );
        assert_eq!(a.format(), b.format());
        assert_eq!(
            [
                a.color_space(),
                a.color_transfer(),
                a.color_encoding(),
                a.color_range()
            ],
            [
                b.color_space(),
                b.color_transfer(),
                b.color_encoding(),
                b.color_range()
            ]
        );
        let strip =
            |p: TensorPlaneView<'_>| (p.handle, p.offset, p.stride, p.size, p.used, p.modifier);
        assert_eq!(
            a.planes().map(strip).collect::<Vec<_>>(),
            b.planes().map(strip).collect::<Vec<_>>()
        );
        assert_eq!(ours.stamp(), golden.stamp());
        assert_eq!(ours.frame_id(), golden.frame_id());
    }

    #[test]
    fn unspecified_colorimetry_is_empty() {
        let t = FrameTensor::from_parts(
            TensorMemory::DmaBuf,
            DType::U8,
            &Geometry::Image {
                format: PixelFormat::Yuyv,
                width: 640,
                height: 480,
                row_stride: Some(1536),
            },
            vec![plane(3, 0, 1536, 1536 * 480)],
            Some(&Colorimetry {
                space: Some(ColorSpace::Bt709),
                ..Colorimetry::default()
            }),
        )
        .unwrap();
        assert_eq!(t.color, ["bt709", "", "", ""]);
        assert_eq!(t.format, "YUYV");
        // Packed YUYV addresses [h, w, 2] at the padded pitch.
        assert_eq!(t.shape, [480, 640, 2]);
        assert_eq!(t.strides, [1536, 2, 1]);
        let none = FrameTensor::from_parts(
            TensorMemory::DmaBuf,
            DType::U8,
            &Geometry::Image {
                format: PixelFormat::Yuyv,
                width: 640,
                height: 480,
                row_stride: None,
            },
            vec![plane(3, 0, 1280, 1280 * 480)],
            None,
        )
        .unwrap();
        assert_eq!(none.color, [""; 4]);
    }

    #[test]
    fn an_unformatted_tensor_carries_no_format_or_colorimetry() {
        let t = FrameTensor::from_parts(
            TensorMemory::Mem,
            DType::F32,
            &Geometry::Raw(vec![2, 3]),
            vec![],
            Some(&bt709_limited()),
        )
        .unwrap();
        assert_eq!(t.format, "");
        assert_eq!(t.color, [""; 4]);
        assert_eq!(t.dtype, DType::F32.code());
        assert_eq!(t.storage_kind, TensorMemory::Mem.code());
        assert_eq!(t.strides, [12, 4]);
        t.with_fields(|f| f.validate()).unwrap();
    }

    #[test]
    fn stamps_clamp_before_the_epoch_and_saturate_past_2038() {
        assert_eq!(
            time(UNIX_EPOCH - Duration::from_secs(5)),
            Time { sec: 0, nanosec: 0 }
        );
        assert_eq!(
            time(UNIX_EPOCH + Duration::new(1_234_567_890, 123_456_789)),
            GOLDEN_STAMP
        );
        assert_eq!(
            time(UNIX_EPOCH + Duration::new(i32::MAX as u64, 999_999_999)),
            SATURATED_TIME
        );
        assert_eq!(
            time(UNIX_EPOCH + Duration::from_secs(i32::MAX as u64 + 1)),
            SATURATED_TIME
        );
        assert_eq!(
            ntp64(Time {
                sec: -1,
                nanosec: 5
            }),
            0
        );
    }

    /// The header stamp and the Zenoh timestamp decoded from [`ntp64`] agree
    /// within 2 ns, and match Zenoh's own conversion exactly.
    #[test]
    fn the_zenoh_timestamp_agrees_with_the_header_stamp() {
        let mut nanos = 0u32;
        for sec in [0, 1, 1_234_567_890, 1_791_000_000, i32::MAX] {
            for _ in 0..2000 {
                nanos = nanos.wrapping_mul(1_103_515_245).wrapping_add(12_345) % 1_000_000_000;
                for nanosec in [0, 1, nanos, 999_999_999] {
                    let stamp = Time { sec, nanosec };
                    let ntp = ntp64(stamp);
                    let d = Duration::new(sec as u64, nanosec);
                    assert_eq!(ntp, uhlc::NTP64::from(d).as_u64(), "{stamp:?}");
                    let decoded = uhlc::NTP64(ntp).as_nanos() as i128;
                    let expected = d.as_nanos() as i128;
                    assert!(
                        (decoded - expected).abs() <= 2,
                        "{stamp:?}: decoded {decoded} ns, expected {expected} ns"
                    );
                }
            }
        }
    }
}

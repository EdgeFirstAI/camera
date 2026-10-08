// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Format and frame-rate negotiation.
//!
//! `TRY_FMT` then `S_FMT` with field `NONE`, a 64-byte-aligned pitch when
//! the SDK will allocate the buffers, then `S_PARM` for the frame rate and
//! `G_PARM` to read it back. The driver's answer is authoritative: vvcam
//! ignores a requested pitch and returns a packed one, ISI honours padding,
//! and SDK pools are allocated at whatever it returns.

use std::time::Duration;

use edgefirst_tensor::{Colorimetry, PixelFormat};
use edgefirst_v4l2::device::{Device, Fraction};
use edgefirst_v4l2::queue::BufType;
use edgefirst_v4l2::uapi;

use super::device::v4l2_error;
use super::format::{self, Mapping};
use crate::{Error, ErrorKind, Result, StreamRequest};

/// GPU import requires a 64-byte-aligned row pitch (Mali, Vivante).
const PITCH_ALIGN: usize = 64;

/// The format the driver accepted.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Negotiated {
    pub(crate) mapping: Mapping,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// `bytesperline` of each memory plane.
    pub(crate) strides: Vec<usize>,
    /// `sizeimage` of each memory plane.
    pub(crate) sizes: Vec<usize>,
    pub(crate) colorimetry: Option<Colorimetry>,
    /// Frame interval read back with `G_PARM`, when the driver reports one.
    pub(crate) interval: Option<Fraction>,
}

impl Negotiated {
    /// Frames per second, from the frame interval.
    pub(crate) fn frame_rate(&self) -> Option<f64> {
        self.interval.and_then(|f| f.fps())
    }

    /// The frame period, for counting drops from timestamp gaps.
    pub(crate) fn period(&self) -> Option<Duration> {
        self.interval.and_then(duration_of)
    }
}

/// Bytes per pixel of the first plane.
fn luma_bpp(format: PixelFormat) -> usize {
    match format {
        PixelFormat::Yuyv | PixelFormat::Vyuy => 2,
        PixelFormat::Rgb => 3,
        PixelFormat::Rgba | PixelFormat::Bgra => 4,
        _ => 1,
    }
}

/// The pixel format to capture: the request, else the device's current
/// format, else the first capturable one it offers.
fn target(
    dev: &Device,
    buf_type: BufType,
    request: &StreamRequest,
    offers: &[u32],
) -> Result<Mapping> {
    let multiplanar = buf_type.is_multiplanar();
    let pick = |format: PixelFormat| format::choose(format, offers, multiplanar);
    if let Some(format) = request.format {
        return pick(format).ok_or_else(|| {
            Error::new(
                ErrorKind::UnsupportedFormat,
                format!("{} does not capture {format:?}", dev.path().display()),
            )
        });
    }
    let current = dev
        .format(buf_type)
        .ok()
        .map(|mut f| current_fourcc(&mut f, multiplanar));
    current
        .and_then(format::by_fourcc)
        .and_then(|m| pick(m.format))
        .or_else(|| {
            offers
                .iter()
                .find_map(|&f| format::by_fourcc(f).and_then(|m| pick(m.format)))
        })
        .ok_or_else(|| {
            Error::new(
                ErrorKind::UnsupportedFormat,
                format!(
                    "{} offers no format the SDK can capture",
                    dev.path().display()
                ),
            )
        })
}

fn current_fourcc(fmt: &mut uapi::v4l2_format, multiplanar: bool) -> u32 {
    // SAFETY: the buffer type selects the payload that is read.
    unsafe {
        if multiplanar {
            fmt.pix_mp().pixelformat
        } else {
            fmt.pix().pixelformat
        }
    }
}

/// Negotiates the request on `dev`. `pad_pitch` requests a 64-byte-aligned
/// pitch, for buffers the SDK allocates.
pub(crate) fn negotiate(
    dev: &Device,
    buf_type: BufType,
    request: &StreamRequest,
    pad_pitch: bool,
) -> Result<Negotiated> {
    let multiplanar = buf_type.is_multiplanar();
    let offers: Vec<u32> = dev
        .formats(buf_type)
        .map_err(|e| v4l2_error("ENUM_FMT", e))?
        .into_iter()
        .map(|f| f.fourcc)
        .collect();
    let mapping = target(dev, buf_type, request, &offers)?;

    let mut fmt = dev.format(buf_type).map_err(|e| v4l2_error("G_FMT", e))?;
    let (mut width, mut height) = size_of(&mut fmt, multiplanar);
    if let Some((w, h)) = request.size {
        (width, height) = (w, h);
    }
    let natural = width as usize * luma_bpp(mapping.format);
    let pitch = if pad_pitch {
        natural.next_multiple_of(PITCH_ALIGN)
    } else {
        0
    };
    let mut fmt = uapi::v4l2_format {
        type_: buf_type.raw(),
        ..Default::default()
    };
    // SAFETY: `type_` selects the payload each branch writes.
    unsafe {
        if multiplanar {
            let p = fmt.pix_mp();
            p.width = width;
            p.height = height;
            p.pixelformat = mapping.fourcc;
            p.field = uapi::V4L2_FIELD_NONE;
            p.num_planes = mapping.mem_planes as u8;
            for plane in &mut p.plane_fmt[..mapping.mem_planes] {
                plane.bytesperline = pitch as u32;
            }
        } else {
            let p = fmt.pix();
            p.width = width;
            p.height = height;
            p.pixelformat = mapping.fourcc;
            p.field = uapi::V4L2_FIELD_NONE;
            p.bytesperline = pitch as u32;
        }
    }
    let _span = tracing::debug_span!(
        "camera.v4l2.negotiate",
        fourcc = %uapi::fourcc_str(mapping.fourcc),
        width,
        height
    )
    .entered();
    dev.try_format(&mut fmt)
        .map_err(|e| v4l2_error("TRY_FMT", e))?;
    dev.set_format(&mut fmt)
        .map_err(|e| v4l2_error("S_FMT", e))?;

    let got = current_fourcc(&mut fmt, multiplanar);
    if got != mapping.fourcc {
        return Err(Error::new(
            ErrorKind::UnsupportedFormat,
            format!(
                "{} replaced {} with {}",
                dev.path().display(),
                uapi::fourcc_str(mapping.fourcc),
                uapi::fourcc_str(got)
            ),
        ));
    }
    let (width, height) = size_of(&mut fmt, multiplanar);
    let (strides, sizes, colorimetry) = layout(&mut fmt, multiplanar);
    if strides.len() != mapping.mem_planes {
        return Err(Error::new(
            ErrorKind::Backend,
            format!(
                "{} returned {} planes for {}",
                dev.path().display(),
                strides.len(),
                uapi::fourcc_str(mapping.fourcc)
            ),
        ));
    }

    if let Some(fps) = request.frame_rate {
        // Best effort: an adjusted or unsupported rate is reported, not an
        // error (D12).
        if let Err(e) = dev.set_frame_interval(buf_type, interval_for(fps)) {
            tracing::debug!("{}: S_PARM {fps} fps: {e}", dev.path().display());
        }
    }
    let interval = dev
        .frame_interval(buf_type)
        .ok()
        .flatten()
        .filter(|f| f.numerator != 0 && f.denominator != 0);

    Ok(Negotiated {
        mapping,
        width,
        height,
        strides,
        sizes,
        colorimetry,
        interval,
    })
}

fn size_of(fmt: &mut uapi::v4l2_format, multiplanar: bool) -> (u32, u32) {
    // SAFETY: the buffer type selects the payload that is read.
    unsafe {
        if multiplanar {
            let p = fmt.pix_mp();
            (p.width, p.height)
        } else {
            let p = fmt.pix();
            (p.width, p.height)
        }
    }
}

fn layout(
    fmt: &mut uapi::v4l2_format,
    multiplanar: bool,
) -> (Vec<usize>, Vec<usize>, Option<Colorimetry>) {
    // SAFETY: the buffer type selects the payload that is read.
    let (strides, sizes, cs, xfer, enc, quant) = unsafe {
        if multiplanar {
            let p = fmt.pix_mp();
            let n = usize::from(p.num_planes).min(p.plane_fmt.len());
            (
                p.plane_fmt[..n]
                    .iter()
                    .map(|pl| pl.bytesperline as usize)
                    .collect(),
                p.plane_fmt[..n]
                    .iter()
                    .map(|pl| pl.sizeimage as usize)
                    .collect(),
                p.colorspace,
                u32::from(p.xfer_func),
                u32::from(p.ycbcr_enc),
                u32::from(p.quantization),
            )
        } else {
            let p = fmt.pix();
            (
                vec![p.bytesperline as usize],
                vec![p.sizeimage as usize],
                p.colorspace,
                p.xfer_func,
                p.ycbcr_enc,
                p.quantization,
            )
        }
    };
    let c = Colorimetry::from_v4l2(cs, xfer, enc, quant);
    let specified =
        c.space.is_some() || c.transfer.is_some() || c.encoding.is_some() || c.range.is_some();
    (strides, sizes, specified.then_some(c))
}

/// The frame interval for `fps`, as a fraction with millisecond-of-a-frame
/// precision (30 fps is 1000/30000).
pub(crate) fn interval_for(fps: f64) -> Fraction {
    Fraction::new(
        1000,
        (fps * 1000.0).round().clamp(1.0, f64::from(u32::MAX)) as u32,
    )
}

fn duration_of(f: Fraction) -> Option<Duration> {
    (f.denominator != 0 && f.numerator != 0)
        .then(|| Duration::from_secs_f64(f64::from(f.numerator) / f64::from(f.denominator)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_round_trips_common_rates() {
        for fps in [15.0, 29.97, 30.0, 60.0, 120.0] {
            let d = duration_of(interval_for(fps)).unwrap();
            assert!((1.0 / d.as_secs_f64() - fps).abs() < 0.01, "{fps}");
        }
    }

    #[test]
    fn luma_pitch_bytes() {
        assert_eq!(luma_bpp(PixelFormat::Yuyv), 2);
        assert_eq!(luma_bpp(PixelFormat::Nv12), 1);
        assert_eq!(luma_bpp(PixelFormat::Rgba), 4);
    }
}

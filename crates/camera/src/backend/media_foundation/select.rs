// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Choosing the camera's native media type for a request, and the formats
//! the backend offers, over plain descriptions of the native types.
//!
//! Every native type delivers one format, repacked without colour
//! conversion when its layout differs (I420 → NV12). Format, then size,
//! then rate pick a native type, as a V4L2 driver picks the nearest size it
//! supports.

use edgefirst_tensor::PixelFormat;

use crate::backend::rate::round_fps;
use crate::{Error, ErrorKind, FormatInfo, Rates, Result, SizeRates, Sizes};

/// The formats the backend delivers, in order of preference.
pub(super) const FORMATS: [PixelFormat; 3] =
    [PixelFormat::Nv12, PixelFormat::Yuyv, PixelFormat::Bgra];

/// How an uncompressed native subtype lays out its pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Packing {
    Nv12,
    /// Planar 4:2:0, U before V.
    I420,
    /// Planar 4:2:0, V before U.
    Yv12,
    Yuy2,
    Uyvy,
    /// B, G, R.
    Rgb24,
    /// B, G, R and an unused byte.
    Rgb32,
    /// B, G, R, A.
    Argb32,
}

impl Packing {
    /// The format frames in this packing are delivered in.
    pub(super) fn delivers(self) -> PixelFormat {
        match self {
            Self::Nv12 | Self::I420 | Self::Yv12 => PixelFormat::Nv12,
            Self::Yuy2 | Self::Uyvy => PixelFormat::Yuyv,
            Self::Rgb24 | Self::Rgb32 | Self::Argb32 => PixelFormat::Bgra,
        }
    }

    /// Whether frames are delivered as the camera lays them out.
    pub(super) fn is_identity(self) -> bool {
        matches!(self, Self::Nv12 | Self::Yuy2 | Self::Argb32)
    }
}

/// How the backend can use a native subtype.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Subtype {
    Native(Packing),
    /// Compressed (MJPEG, H.264) or a layout the SDK does not capture.
    Unusable,
}

/// One of the camera's native media types.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct NativeType {
    /// Index in Media Foundation's list.
    pub(super) index: u32,
    pub(super) subtype: Subtype,
    pub(super) width: u32,
    pub(super) height: u32,
    /// Frame rate as numerator and denominator.
    pub(super) rate: (u32, u32),
}

impl NativeType {
    /// Frames per second, when the type has a rate.
    pub(super) fn fps(&self) -> Option<f64> {
        (self.rate.0 != 0 && self.rate.1 != 0)
            .then(|| round_fps(f64::from(self.rate.0) / f64::from(self.rate.1)))
    }

    pub(super) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub(super) fn packing(&self) -> Option<Packing> {
        match self.subtype {
            Subtype::Native(p) => Some(p),
            Subtype::Unusable => None,
        }
    }

    /// The format this type delivers, when the SDK captures it.
    pub(super) fn delivers(&self) -> Option<PixelFormat> {
        self.fps()?;
        self.packing().map(Packing::delivers)
    }
}

fn same_rate(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.005
}

/// The native type for a request. `current` is the type the camera is set
/// to, which a request without a format, size or rate keeps.
pub(super) fn choose(
    types: &[NativeType],
    current: Option<&NativeType>,
    format: Option<PixelFormat>,
    size: Option<(u32, u32)>,
    fps: Option<f64>,
) -> Result<NativeType> {
    let offered: Vec<PixelFormat> = FORMATS
        .into_iter()
        .filter(|&f| types.iter().any(|t| t.delivers() == Some(f)))
        .collect();
    let current = current.filter(|c| c.delivers().is_some());
    let format = match format {
        Some(f) if offered.contains(&f) => f,
        Some(f) => {
            return Err(Error::new(
                ErrorKind::UnsupportedFormat,
                format!("{f:?} is not offered; the camera delivers {offered:?}"),
            ))
        }
        None => match current
            .and_then(NativeType::delivers)
            .or(offered.first().copied())
        {
            Some(f) => f,
            None => {
                return Err(Error::new(
                    ErrorKind::UnsupportedFormat,
                    "the camera offers no uncompressed format the SDK captures",
                ))
            }
        },
    };
    let usable: Vec<&NativeType> = types
        .iter()
        .filter(|t| t.delivers() == Some(format))
        .collect();
    let current = current.filter(|c| c.delivers() == Some(format));
    let size = match size {
        Some(asked) => nearest_size(&usable, asked),
        None => current.map_or(usable[0].size(), |c| c.size()),
    };
    let at_size: Vec<&NativeType> = usable.into_iter().filter(|t| t.size() == size).collect();
    let rates = at_size.iter().filter_map(|t| t.fps());
    let rate = match (fps, current) {
        (Some(asked), _) => rates.fold(f64::NAN, |best, r| {
            let (d, db) = ((r - asked).abs(), (best - asked).abs());
            if best.is_nan() || d < db || (d == db && r > best) {
                r
            } else {
                best
            }
        }),
        (None, Some(c)) if c.size() == size => c.fps().unwrap_or_default(),
        (None, _) => rates.fold(0.0, f64::max),
    };
    let at_rate: Vec<&NativeType> = at_size
        .into_iter()
        .filter(|t| t.fps().is_some_and(|r| same_rate(r, rate)))
        .collect();
    let chosen = at_rate
        .iter()
        .find(|t| t.packing().is_some_and(Packing::is_identity))
        .or_else(|| at_rate.iter().find(|t| Some(**t) == current))
        .or_else(|| at_rate.first())
        .expect("the chosen size and rate come from a usable type");
    Ok(**chosen)
}

/// The offered size nearest `asked`: the same size, else the closest area,
/// then the closest width.
fn nearest_size(usable: &[&NativeType], asked: (u32, u32)) -> (u32, u32) {
    let area = |(w, h): (u32, u32)| u64::from(w) * u64::from(h);
    usable
        .iter()
        .map(|t| t.size())
        .min_by_key(|&s| {
            (
                area(s).abs_diff(area(asked)),
                s.0.abs_diff(asked.0),
                s.1.abs_diff(asked.1),
            )
        })
        .unwrap_or(asked)
}

/// The rates `format` is offered at at `size`, highest first.
pub(super) fn rates_at(types: &[NativeType], format: PixelFormat, size: (u32, u32)) -> Vec<f64> {
    let mut rates: Vec<f64> = types
        .iter()
        .filter(|t| t.delivers() == Some(format) && t.size() == size)
        .filter_map(NativeType::fps)
        .collect();
    rates.sort_by(|a, b| b.total_cmp(a));
    rates.dedup_by(|a, b| same_rate(*a, *b));
    rates
}

/// Each format the camera delivers, with its sizes and rates.
pub(super) fn formats(types: &[NativeType]) -> Vec<FormatInfo> {
    FORMATS
        .into_iter()
        .filter_map(|format| {
            let mut sizes: Vec<(u32, u32)> = types
                .iter()
                .filter(|t| t.delivers() == Some(format))
                .map(NativeType::size)
                .collect();
            sizes.sort_by_key(|&(w, h)| std::cmp::Reverse((u64::from(w) * u64::from(h), w)));
            sizes.dedup();
            let table: Vec<SizeRates> = sizes
                .into_iter()
                .map(|(width, height)| SizeRates {
                    width,
                    height,
                    rates: Rates::Discrete(rates_at(types, format, (width, height))),
                })
                .collect();
            (!table.is_empty()).then_some(FormatInfo {
                format,
                sizes: Sizes::Discrete(table),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const I420: Subtype = Subtype::Native(Packing::I420);
    const RGB24: Subtype = Subtype::Native(Packing::Rgb24);
    const NV12: Subtype = Subtype::Native(Packing::Nv12);
    const YUY2: Subtype = Subtype::Native(Packing::Yuy2);
    const MJPG: Subtype = Subtype::Unusable;

    fn t(index: u32, subtype: Subtype, width: u32, height: u32, fps: u32) -> NativeType {
        NativeType {
            index,
            subtype,
            width,
            height,
            rate: (fps, 1),
        }
    }

    /// The QuickCam Pro for Notebooks, abridged: RGB24 and I420 only.
    fn quickcam() -> Vec<NativeType> {
        vec![
            t(0, RGB24, 640, 480, 30),
            t(1, RGB24, 640, 480, 5),
            t(2, I420, 640, 480, 30),
            t(3, I420, 640, 480, 5),
            t(4, I420, 1600, 1200, 5),
            t(5, I420, 320, 240, 30),
            t(6, I420, 320, 240, 15),
        ]
    }

    /// A typical UVC webcam: YUY2 at low rates for large sizes, MJPEG fast.
    fn webcam() -> Vec<NativeType> {
        vec![
            t(0, MJPG, 1920, 1080, 30),
            t(1, YUY2, 1920, 1080, 5),
            t(2, YUY2, 640, 480, 30),
            t(3, NV12, 640, 480, 30),
            t(4, MJPG, 1280, 720, 60),
        ]
    }

    fn pick(
        types: &[NativeType],
        current: Option<usize>,
        format: Option<PixelFormat>,
        size: Option<(u32, u32)>,
        fps: Option<f64>,
    ) -> NativeType {
        choose(types, current.map(|i| &types[i]), format, size, fps).unwrap()
    }

    #[test]
    fn no_request_keeps_the_current_type() {
        let c = pick(&quickcam(), Some(0), None, None, None);
        assert_eq!(c.index, 0);
        assert_eq!(c.delivers(), Some(PixelFormat::Bgra));
        let c = pick(&webcam(), Some(1), None, None, None);
        assert_eq!(c.index, 1);
    }

    #[test]
    fn a_format_picks_the_types_that_deliver_it() {
        let c = pick(&quickcam(), Some(0), Some(PixelFormat::Nv12), None, None);
        assert_eq!((c.index, c.packing()), (2, Some(Packing::I420)));
        let c = pick(
            &webcam(),
            None,
            Some(PixelFormat::Nv12),
            Some((640, 480)),
            None,
        );
        assert_eq!(c.index, 3);
        let e = choose(&quickcam(), None, Some(PixelFormat::Yuyv), None, None).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UnsupportedFormat);
        let e = choose(&quickcam(), None, Some(PixelFormat::Rgb), None, None).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UnsupportedFormat);
    }

    #[test]
    fn compressed_types_are_never_chosen() {
        let c = pick(&webcam(), Some(0), None, None, None);
        assert_ne!(c.subtype, MJPG);
        let c = pick(&webcam(), None, None, Some((1280, 720)), Some(60.0));
        assert_ne!(c.subtype, MJPG);
        let only = [t(0, MJPG, 1920, 1080, 30)];
        let e = choose(&only, None, None, None, None).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UnsupportedFormat);
    }

    #[test]
    fn a_size_not_offered_takes_the_nearest() {
        let c = pick(
            &quickcam(),
            None,
            Some(PixelFormat::Nv12),
            Some((1920, 1080)),
            Some(30.0),
        );
        assert_eq!(c.size(), (1600, 1200));
        assert_eq!(c.fps(), Some(5.0), "the only rate at that size");
    }

    #[test]
    fn the_rate_is_the_nearest_offered_at_the_size() {
        let nv12 = Some(PixelFormat::Nv12);
        let c = pick(&quickcam(), None, nv12, Some((320, 240)), Some(20.0));
        assert_eq!(c.fps(), Some(15.0));
        let c = pick(&quickcam(), None, nv12, Some((320, 240)), None);
        assert_eq!(c.fps(), Some(30.0), "the highest without a request");
    }

    #[test]
    fn each_format_is_offered_at_the_sizes_of_the_types_that_deliver_it() {
        let f = formats(&webcam());
        let listed: Vec<_> = f.iter().map(|i| i.format).collect();
        assert_eq!(listed, [PixelFormat::Nv12, PixelFormat::Yuyv]);
        let Sizes::Discrete(sizes) = &f[1].sizes else {
            panic!("discrete sizes");
        };
        let yuyv: Vec<_> = sizes.iter().map(|s| (s.width, s.height)).collect();
        assert_eq!(yuyv, [(1920, 1080), (640, 480)]);
        assert_eq!(sizes[0].rates, Rates::Discrete(vec![5.0]));
        assert!(formats(&[t(0, MJPG, 640, 480, 30)]).is_empty());
    }

    #[test]
    fn fractional_rates_are_rounded() {
        let ntsc = NativeType {
            rate: (30000, 1001),
            ..t(0, NV12, 640, 480, 0)
        };
        assert_eq!(ntsc.fps(), Some(29.97));
        let both = [ntsc, t(1, NV12, 640, 480, 30)];
        assert_eq!(
            rates_at(&both, PixelFormat::Nv12, (640, 480)),
            [30.0, 29.97]
        );
    }
}

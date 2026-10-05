// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Camera discovery, capability probing and named modes.

use std::fmt;

use edgefirst_tensor::PixelFormat;

/// Capture backend.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Backend {
    /// Video4Linux2 capture nodes.
    V4l2,
    /// libcamera through the runtime-loaded shim.
    Libcamera,
    /// A recorded Annex-B H.264 file with its JSON sidecar.
    File,
    /// Synthetic frames.
    Mock,
}

/// Identity and capabilities of a camera.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct CameraDescriptor {
    /// Backend that serves this camera.
    pub backend: Backend,
    /// Backend-specific identifier: a device path, a libcamera id, a file
    /// path, or `mock`.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Driver or pipeline name.
    pub driver: String,
    /// Formats with their sizes and rates, as far as enumeration reports
    /// them cheaply; [`probe`](crate::probe) fills in the rest.
    pub formats: Vec<FormatInfo>,
}

impl CameraDescriptor {
    /// Creates a descriptor with no formats.
    pub fn new(
        backend: Backend,
        id: impl Into<String>,
        name: impl Into<String>,
        driver: impl Into<String>,
    ) -> Self {
        Self {
            backend,
            id: id.into(),
            name: name.into(),
            driver: driver.into(),
            formats: Vec::new(),
        }
    }
}

/// The sizes and rates offered for one pixel format.
#[derive(Debug, Clone, PartialEq)]
pub struct FormatInfo {
    /// Pixel format.
    pub format: PixelFormat,
    /// Sizes and their rates.
    pub sizes: Sizes,
}

/// The sizes a format is offered at.
#[derive(Debug, Clone, PartialEq)]
pub enum Sizes {
    /// Exact size and rate pairs (UVC, ISI).
    Discrete(Vec<SizeRates>),
    /// Any size in a range, as when an ISP scales (vvcam).
    Stepwise {
        /// Smallest width and height.
        min: (u32, u32),
        /// Largest width and height.
        max: (u32, u32),
        /// Width and height step.
        step: (u32, u32),
        /// Rates offered at every size in the range.
        rates: Rates,
    },
}

/// One size and the rates offered at it.
#[derive(Debug, Clone, PartialEq)]
pub struct SizeRates {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Rates offered at this size.
    pub rates: Rates,
}

/// The frame rates offered at a size.
#[derive(Debug, Clone, PartialEq)]
pub enum Rates {
    /// A list of rates in frames per second.
    Discrete(Vec<f64>),
    /// Any rate in a range, in frames per second.
    Range {
        /// Lowest rate.
        min: f64,
        /// Highest rate.
        max: f64,
    },
    /// The backend cannot report rates without a probe.
    Unknown,
}

impl Rates {
    /// Highest rate offered, if known.
    pub fn max(&self) -> Option<f64> {
        match self {
            Self::Discrete(r) => r
                .iter()
                .copied()
                .fold(None, |m, v| Some(m.map_or(v, |m: f64| m.max(v)))),
            Self::Range { max, .. } => Some(*max),
            Self::Unknown => None,
        }
    }
}

/// A size and its highest frame rate, named like `1080p30`.
///
/// Modes are derived from a probe to show which combinations a camera
/// offers. They are never set directly: open a camera with a size and a
/// frame rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mode {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Highest frame rate offered at this size.
    pub max_fps: f64,
}

/// Standard sizes with their names, used to name modes and to pick sizes
/// out of a stepwise range.
const NAMED_SIZES: [(&str, u32, u32); 6] = [
    ("VGA", 640, 480),
    ("WVGA", 800, 480),
    ("540p", 960, 540),
    ("720p", 1280, 720),
    ("1080p", 1920, 1080),
    ("4K", 3840, 2160),
];

impl Mode {
    /// The mode's name: a standard size name followed by the whole-number
    /// rate (`1080p30`), or `WxH@fps` for other sizes.
    pub fn name(&self) -> String {
        let fps = self.max_fps.round() as u64;
        match NAMED_SIZES
            .iter()
            .find(|(_, w, h)| *w == self.width && *h == self.height)
        {
            Some((name, _, _)) => format!("{name}{fps}"),
            None => format!("{}x{}@{fps}", self.width, self.height),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

/// Derives the modes offered for `format`.
///
/// Discrete sizes give one mode each at their highest rate. A stepwise
/// range gives a mode for every standard size (VGA, WVGA, 540p, 720p,
/// 1080p, 4K) that fits the range and its step. Sizes whose rates are
/// unknown give no mode; probe the camera first.
pub fn modes(formats: &[FormatInfo], format: PixelFormat) -> Vec<Mode> {
    let mut out: Vec<Mode> = Vec::new();
    for info in formats.iter().filter(|f| f.format == format) {
        match &info.sizes {
            Sizes::Discrete(sizes) => {
                for s in sizes {
                    if let Some(max_fps) = s.rates.max() {
                        out.push(Mode {
                            width: s.width,
                            height: s.height,
                            max_fps,
                        });
                    }
                }
            }
            Sizes::Stepwise {
                min,
                max,
                step,
                rates,
            } => {
                let Some(max_fps) = rates.max() else { continue };
                let fits = |v: u32, lo: u32, hi: u32, st: u32| {
                    v >= lo && v <= hi && (st <= 1 || (v - lo).is_multiple_of(st))
                };
                for (_, w, h) in NAMED_SIZES {
                    if fits(w, min.0, max.0, step.0) && fits(h, min.1, max.1, step.1) {
                        out.push(Mode {
                            width: w,
                            height: h,
                            max_fps,
                        });
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| {
        (a.width * a.height)
            .cmp(&(b.width * b.height))
            .then(a.max_fps.total_cmp(&b.max_fps))
    });
    out.dedup_by(|a, b| a.width == b.width && a.height == b.height && a.max_fps == b.max_fps);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The OV5640 on the i.MX 8M Plus ISI, as enumerated on imx8mpevk-08.
    fn isi_ov5640() -> Vec<FormatInfo> {
        let s = |w, h, r: &[f64]| SizeRates {
            width: w,
            height: h,
            rates: Rates::Discrete(r.to_vec()),
        };
        vec![FormatInfo {
            format: PixelFormat::Yuyv,
            sizes: Sizes::Discrete(vec![
                s(640, 480, &[15.0, 30.0, 60.0]),
                s(1280, 720, &[15.0, 30.0]),
                s(1920, 1080, &[15.0, 30.0]),
                s(2592, 1944, &[15.0]),
            ]),
        }]
    }

    #[test]
    fn discrete_modes_use_highest_rate() {
        let names: Vec<_> = modes(&isi_ov5640(), PixelFormat::Yuyv)
            .iter()
            .map(Mode::name)
            .collect();
        assert_eq!(names, ["VGA60", "720p30", "1080p30", "2592x1944@15"]);
    }

    #[test]
    fn stepwise_modes_pick_standard_sizes() {
        // vvcam with the os08a20_4k ISP configuration on imx8mpevk-06.
        let formats = vec![FormatInfo {
            format: PixelFormat::Nv12,
            sizes: Sizes::Stepwise {
                min: (176, 144),
                max: (4096, 3072),
                step: (16, 8),
                rates: Rates::Discrete((15..=30).map(f64::from).collect()),
            },
        }];
        let names: Vec<_> = modes(&formats, PixelFormat::Nv12)
            .iter()
            .map(Mode::name)
            .collect();
        // 540 is not reachable from 144 in steps of 8, so 540p is left out.
        assert_eq!(names, ["VGA30", "WVGA30", "720p30", "1080p30", "4K30"]);
    }

    #[test]
    fn unknown_rates_and_other_formats_give_no_modes() {
        let formats = vec![FormatInfo {
            format: PixelFormat::Nv12,
            sizes: Sizes::Discrete(vec![SizeRates {
                width: 1920,
                height: 1080,
                rates: Rates::Unknown,
            }]),
        }];
        assert!(modes(&formats, PixelFormat::Nv12).is_empty());
        assert!(modes(&isi_ov5640(), PixelFormat::Nv12).is_empty());
    }

    #[test]
    fn fractional_rates_round_in_names() {
        let m = Mode {
            width: 1536,
            height: 864,
            max_fps: 120.13,
        };
        assert_eq!(m.name(), "1536x864@120");
    }
}

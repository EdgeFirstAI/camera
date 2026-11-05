// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Camera descriptions: formats, capture drivers and probed rates.

use edgefirst_libcamera_sys::{self as sys};
use edgefirst_tensor::PixelFormat;

use super::manager::Manager;
use super::Handle;
use crate::enumerate::NAMED_SIZES;
use crate::{
    Backend, CameraDescriptor, Error, ErrorKind, FormatInfo, Rates, Result, SizeRates, Sizes,
    TimestampSource,
};

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

/// DRM fourccs of the formats frames can carry. DRM names packed RGB by
/// the little-endian word, so `RG24` (BGR888) is R, G, B in memory.
const FORMATS: [(u32, PixelFormat); 8] = [
    (fourcc(b"NV12"), PixelFormat::Nv12),
    (fourcc(b"NV16"), PixelFormat::Nv16),
    (fourcc(b"YUYV"), PixelFormat::Yuyv),
    (fourcc(b"VYUY"), PixelFormat::Vyuy),
    (fourcc(b"R8  "), PixelFormat::Grey),
    (fourcc(b"RG24"), PixelFormat::Rgb),
    (fourcc(b"AB24"), PixelFormat::Rgba),
    (fourcc(b"AR24"), PixelFormat::Bgra),
];

pub(crate) fn pixel_format(code: u32) -> Option<PixelFormat> {
    FORMATS.iter().find(|(c, _)| *c == code).map(|(_, f)| *f)
}

pub(crate) fn drm_fourcc(format: PixelFormat) -> Option<u32> {
    FORMATS.iter().find(|(_, f)| *f == format).map(|(c, _)| *c)
}

/// The fourcc as text, for messages.
pub(crate) fn fourcc_str(code: u32) -> String {
    code.to_le_bytes().iter().map(|&b| b as char).collect()
}

/// Kernel drivers of the camera's V4L2 nodes, from sysfs.
pub(crate) fn drivers(info: &sys::efcam_lc_camera_info) -> Vec<String> {
    let mut out: Vec<String> = info.devices[..(info.num_devices as usize).min(info.devices.len())]
        .iter()
        .filter_map(|&dev| {
            let link = format!(
                "/sys/dev/char/{}:{}/device/driver",
                libc::major(dev),
                libc::minor(dev)
            );
            std::fs::read_link(link)
                .ok()?
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Where `SensorTimestamp` is taken. libcamera copies it from the capture
/// node's V4L2 buffer timestamp: the RPi receivers stamp at frame start,
/// other drivers when the buffer completes.
pub(crate) fn timestamp_source(drivers: &[String]) -> TimestampSource {
    if drivers.iter().any(|d| d == "rp1-cfe" || d == "unicam") {
        TimestampSource::StartOfFrame
    } else {
        TimestampSource::EndOfFrame
    }
}

/// The descriptor of camera `info`, with enumerated formats and unknown
/// rates.
pub(crate) fn descriptor(m: &Manager, info: &sys::efcam_lc_camera_info) -> CameraDescriptor {
    let id = sys::field_str(&info.id);
    let model = sys::field_str(&info.model);
    let drivers = drivers(info);
    let mut d = CameraDescriptor::new(
        Backend::Libcamera,
        &id,
        if model.is_empty() { id.clone() } else { model },
        if drivers.is_empty() {
            "libcamera".to_owned()
        } else {
            drivers.join(",")
        },
    );
    let formats = m.formats(&id).unwrap_or_else(|e| {
        tracing::warn!("{id}: {e}");
        Vec::new()
    });
    d.formats = format_infos(&formats, |_, _, _| Rates::Unknown);
    if d.formats.is_empty() && !formats.is_empty() {
        let offered: Vec<String> = formats.iter().map(|f| fourcc_str(f.fourcc)).collect();
        tracing::warn!(
            "{id}: libcamera offers only {} here, no processed format; on i.MX 95 set \
             LIBCAMERA_PIPELINES_MATCH_LIST=nxp/neo so the ISP pipeline claims the camera",
            offered.join(", ")
        );
    }
    d
}

/// Groups shim formats by pixel format. Discrete sizes keep their rates
/// from `rates`; a range becomes the standard sizes inside it when `rates`
/// is consulted per size, otherwise a stepwise entry.
fn format_infos(
    formats: &[sys::efcam_lc_format],
    mut rates: impl FnMut(PixelFormat, u32, u32) -> Rates,
) -> Vec<FormatInfo> {
    let mut out: Vec<FormatInfo> = Vec::new();
    for f in formats {
        let Some(format) = pixel_format(f.fourcc) else {
            continue;
        };
        let sizes: Vec<(u32, u32)> = if f.width != 0 && f.height != 0 {
            vec![(f.width, f.height)]
        } else {
            NAMED_SIZES
                .iter()
                .filter(|(_, w, h)| {
                    fits(*w, f.min_width, f.max_width, f.h_step)
                        && fits(*h, f.min_height, f.max_height, f.v_step)
                })
                .map(|(_, w, h)| (*w, *h))
                .collect()
        };
        let entries = sizes.into_iter().map(|(width, height)| SizeRates {
            width,
            height,
            rates: rates(format, width, height),
        });
        match out.iter_mut().find(|i| i.format == format) {
            Some(FormatInfo {
                sizes: Sizes::Discrete(list),
                ..
            }) => list.extend(entries),
            Some(_) => {}
            None => out.push(FormatInfo {
                format,
                sizes: Sizes::Discrete(entries.collect()),
            }),
        }
    }
    for info in &mut out {
        if let Sizes::Discrete(list) = &mut info.sizes {
            list.sort_by_key(|s| (s.width * s.height, s.width));
            list.dedup_by_key(|s| (s.width, s.height));
        }
    }
    out
}

fn fits(v: u32, lo: u32, hi: u32, step: u32) -> bool {
    v >= lo && v <= hi && (step <= 1 || (v - lo).is_multiple_of(step))
}

/// Formats the probe configures, in order of preference.
const PROBE_FORMATS: [PixelFormat; 2] = [PixelFormat::Nv12, PixelFormat::Yuyv];

/// Probes each size by configuring it and reading the
/// `FrameDurationLimits` the pipeline then reports, as
/// `rpicam-hello --list-cameras` does. Needs the camera to be free.
///
/// The rate follows the sensor mode a size selects, not the output format,
/// so each size is configured once, in NV12 or else YUYV, and its rates
/// apply to every format. Configuring other formats is avoided: the RPi 5
/// `pisp` pipeline lists VYUY yet aborts the process when asked to
/// validate it.
pub(crate) fn probe(m: &std::sync::Arc<Manager>, d: &CameraDescriptor) -> Result<Vec<FormatInfo>> {
    let formats = m.formats(&d.id)?;
    let offered: Vec<PixelFormat> = formats
        .iter()
        .filter_map(|f| pixel_format(f.fourcc))
        .collect();
    let probe_format = PROBE_FORMATS
        .into_iter()
        .chain(offered.iter().copied())
        .find(|f| offered.contains(f));
    let handle = Handle::open(m.clone(), &d.id)?;
    let mut probed: Vec<((u32, u32), Rates)> = Vec::new();
    let infos = format_infos(&formats, |_, width, height| {
        if let Some((_, rates)) = probed.iter().find(|(size, _)| *size == (width, height)) {
            return rates.clone();
        }
        let rates = probe_format
            .and_then(drm_fourcc)
            .map_or(Rates::Unknown, |code| {
                rates_at(&handle, code, width, height)
            });
        probed.push(((width, height), rates.clone()));
        rates
    });
    if infos.is_empty() {
        return Err(Error::new(
            ErrorKind::UnsupportedFormat,
            format!("{}: libcamera offers no processed format", d.id),
        ));
    }
    Ok(infos)
}

/// The frame rates the pipeline reports for one configuration, rounded to
/// hundredths.
fn rates_at(handle: &Handle, code: u32, width: u32, height: u32) -> Rates {
    let req = sys::efcam_lc_stream_request {
        role: sys::EFCAM_LC_ROLE_VIDEO,
        fourcc: code,
        width,
        height,
        ..Default::default()
    };
    let round = |fps: f64| (fps * 100.0).round() / 100.0;
    match handle.configure(&req) {
        Ok(info) if info.fourcc == code && (info.width, info.height) == (width, height) => {
            match handle.control_info(sys::EFCAM_LC_CONTROL_FRAME_DURATION_LIMITS) {
                Some(fdl) if fdl.min > 0.0 && fdl.max >= fdl.min => Rates::Range {
                    min: round(1e6 / fdl.max),
                    max: round(1e6 / fdl.min),
                },
                _ => Rates::Unknown,
            }
        }
        _ => Rates::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourccs_round_trip() {
        for (code, format) in FORMATS {
            assert_eq!(pixel_format(code), Some(format));
            assert_eq!(drm_fourcc(format), Some(code));
        }
        assert_eq!(pixel_format(fourcc(b"pRAA")), None);
        assert_eq!(fourcc_str(fourcc(b"NV12")), "NV12");
    }

    #[test]
    fn rpi_receivers_stamp_at_frame_start() {
        let d = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            timestamp_source(&d(&["pispbe", "rp1-cfe"])),
            TimestampSource::StartOfFrame
        );
        assert_eq!(
            timestamp_source(&d(&["mxc-isi", "neoisp"])),
            TimestampSource::EndOfFrame
        );
        assert_eq!(timestamp_source(&[]), TimestampSource::EndOfFrame);
    }

    #[test]
    fn ranges_become_standard_sizes_and_raw_formats_are_skipped() {
        let range = sys::efcam_lc_format {
            fourcc: fourcc(b"NV12"),
            min_width: 64,
            min_height: 64,
            max_width: 1920,
            max_height: 1080,
            h_step: 2,
            v_step: 2,
            ..Default::default()
        };
        let raw = sys::efcam_lc_format {
            fourcc: fourcc(b"pRAA"),
            width: 4608,
            height: 2592,
            ..Default::default()
        };
        let infos = format_infos(&[range, raw], |_, _, _| Rates::Unknown);
        assert_eq!(infos.len(), 1);
        let Sizes::Discrete(sizes) = &infos[0].sizes else {
            panic!("discrete sizes")
        };
        let got: Vec<_> = sizes.iter().map(|s| (s.width, s.height)).collect();
        assert_eq!(
            got,
            [
                (640, 480),
                (800, 480),
                (960, 540),
                (1280, 720),
                (1920, 1080)
            ]
        );
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Opening, locking and describing V4L2 capture nodes.

use std::os::fd::{AsFd, AsRawFd};
use std::path::Path;

use edgefirst_v4l2::device::{self as v4l2dev, Device, FrameIntervals, FrameSizes, Size};
use edgefirst_v4l2::queue::BufType;

use super::format;
use crate::{Backend, CameraDescriptor, Error, ErrorKind, FormatInfo, Rates, SizeRates, Sizes};

/// Converts an `edgefirst-v4l2` error, naming what the backend was doing.
pub(crate) fn v4l2_error(what: &str, e: edgefirst_v4l2::Error) -> Error {
    use edgefirst_v4l2::ErrorKind as K;
    let kind = match e.kind() {
        K::Disconnected => ErrorKind::Disconnected,
        K::Busy => ErrorKind::Busy,
        K::InvalidArgument | K::InvalidState => ErrorKind::InvalidConfig,
        K::Unsupported => ErrorKind::Backend,
        _ => ErrorKind::Io,
    };
    Error::new(kind, format!("{what}: {e}")).with_source(e)
}

/// The errno behind an `edgefirst-v4l2` error, if it carries one.
pub(crate) fn errno(e: &edgefirst_v4l2::Error) -> Option<i32> {
    e.errno().map(|n| n as i32)
}

/// Opens `path` as a capture node: non-blocking, close-on-exec, and with an
/// exclusive advisory lock when `exclusive`, so two publishers cannot share
/// one ISP.
///
/// `ENOENT` on a node that exists is [`ErrorKind::NotReady`]: the i.MX 8M
/// Plus ISP returns it while `isp_media_server` restarts. A path that does
/// not exist is [`ErrorKind::NotFound`].
pub(crate) fn open(path: &Path, exclusive: bool) -> crate::Result<(Device, BufType)> {
    let dev = Device::open(path).map_err(|e| {
        if errno(&e) == Some(libc::ENOENT) {
            if path.symlink_metadata().is_ok() {
                Error::new(
                    ErrorKind::NotReady,
                    format!("{}: the device is not ready (ENOENT)", path.display()),
                )
                .with_source(e)
            } else {
                Error::new(
                    ErrorKind::NotFound,
                    format!("{} does not exist", path.display()),
                )
                .with_source(e)
            }
        } else {
            v4l2_error(&format!("open {}", path.display()), e)
        }
    })?;
    let caps = dev.capabilities();
    let buf_type = match caps.capture_buf_type() {
        Some(t) if caps.has_streaming() && !caps.is_m2m() => t,
        _ => {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!(
                    "{} ({}) is not a streaming video capture device",
                    path.display(),
                    caps.driver
                ),
            ))
        }
    };
    if exclusive {
        lock(&dev, path)?;
    }
    Ok((dev, buf_type))
}

/// Takes `flock(LOCK_EX | LOCK_NB)` on the open device. The lock belongs to
/// the open file, so it is released when the device closes.
fn lock(dev: &Device, path: &Path) -> crate::Result<()> {
    // SAFETY: the descriptor is open for the duration of the call.
    let rc = unsafe { libc::flock(dev.as_fd().as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Err(Error::new(
            ErrorKind::Busy,
            format!("{} is locked by another user", path.display()),
        ));
    }
    Err(Error::from(err))
}

/// Describes a capture node: identity and every capturable format with its
/// sizes and rates.
pub(crate) fn describe(dev: &Device, buf_type: BufType) -> CameraDescriptor {
    let caps = dev.capabilities();
    let mut d = CameraDescriptor::new(
        Backend::V4l2,
        dev.path().display().to_string(),
        caps.card.clone(),
        caps.driver.clone(),
    );
    d.formats = formats(dev, buf_type);
    d
}

/// The capturable formats of a node, each with its sizes and rates. Formats
/// with no tensor equivalent (such as UVC MJPEG) are left out.
pub(crate) fn formats(dev: &Device, buf_type: BufType) -> Vec<FormatInfo> {
    let mut out: Vec<FormatInfo> = Vec::new();
    for desc in dev.formats(buf_type).unwrap_or_default() {
        let Some(m) = format::by_fourcc(desc.fourcc) else {
            continue;
        };
        if m.mem_planes > 1 && !buf_type.is_multiplanar() {
            continue;
        }
        // NV12 and NV12M describe the same frames; list the format once.
        if out.iter().any(|f| f.format == m.format) {
            continue;
        }
        if let Some(sizes) = sizes(dev, desc.fourcc) {
            out.push(FormatInfo {
                format: m.format,
                sizes,
            });
        }
    }
    out
}

fn sizes(dev: &Device, fourcc: u32) -> Option<Sizes> {
    match dev.frame_sizes(fourcc).ok()? {
        FrameSizes::Discrete(list) => Some(Sizes::Discrete(
            list.into_iter()
                .map(|s| SizeRates {
                    width: s.width,
                    height: s.height,
                    rates: rates(dev, fourcc, s),
                })
                .collect(),
        )),
        FrameSizes::Stepwise(r) | FrameSizes::Continuous(r) => Some(Sizes::Stepwise {
            min: (r.min.width, r.min.height),
            max: (r.max.width, r.max.height),
            step: (r.step.width.max(1), r.step.height.max(1)),
            // An ISP that scales reports the same rates at every size; the
            // largest size is the conservative one.
            rates: rates(dev, fourcc, r.max),
        }),
    }
}

fn rates(dev: &Device, fourcc: u32, size: Size) -> Rates {
    match dev.frame_intervals(fourcc, size) {
        Ok(FrameIntervals::Discrete(list)) => {
            let mut fps: Vec<f64> = list.into_iter().filter_map(|f| f.fps()).collect();
            fps.sort_by(|a, b| b.total_cmp(a));
            fps.dedup();
            Rates::Discrete(fps)
        }
        Ok(FrameIntervals::Stepwise { min, max, .. } | FrameIntervals::Continuous { min, max }) => {
            match (max.fps(), min.fps()) {
                (Some(lo), Some(hi)) => Rates::Range { min: lo, max: hi },
                _ => Rates::Unknown,
            }
        }
        Err(_) => Rates::Unknown,
    }
}

/// Every streaming V4L2 capture node, described without locking it.
/// Memory-to-memory devices (codecs, scalers) are not cameras and are left
/// out.
pub(crate) fn enumerate() -> crate::Result<Vec<CameraDescriptor>> {
    let nodes = v4l2dev::enumerate().map_err(|e| v4l2_error("enumerate /dev/video*", e))?;
    Ok(nodes
        .into_iter()
        .filter(|n| {
            n.capabilities
                .as_ref()
                .is_ok_and(|c| c.is_capture() && c.has_streaming() && !c.is_m2m())
        })
        .filter_map(|n| open(&n.path, false).ok())
        .map(|(dev, buf_type)| describe(&dev, buf_type))
        .collect())
}

/// The full format, size and rate table of the node a descriptor names.
pub(crate) fn probe(d: &CameraDescriptor) -> crate::Result<Vec<FormatInfo>> {
    let (dev, buf_type) = open(Path::new(&d.id), false)?;
    Ok(formats(&dev, buf_type))
}

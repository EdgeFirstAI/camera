// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

//! Capture size and frame rate as the service understands them.
//!
//! When `CAMERA_MODE` names an FPS, that value is the expected rate even
//! if the driver keeps reporting the underlying sensor mode (EDGEAI-1445:
//! a 30 FPS cap on a 1080p60 pipeline used to log "configured for 60").
//! `VIDIOC_S_PARM` is attempted so the device can honour the request; a
//! clamp or unsupported ioctl is logged and the parsed FPS is still used.
//!
//! When no FPS is requested, the rate is `VIDIOC_G_PARM` as before.
//! When no size is requested, `VIDIOC_G_FMT` is the live capture size.
//!
//! `videostream` exposes neither these ioctls nor the camera device's
//! file descriptor (`CameraBuffer::fd` is the DMA buffer, not the device),
//! so this opens the device node itself. That is a second open of a
//! device the capture path already holds, which is safe here: each call
//! is an open, one ioctl and a close, with no streaming of its own.

use std::ffi::CString;
use tracing::{info, warn};

/// Used when the driver cannot tell us. Matches the historical hardcoded
/// value, so behaviour is unchanged on a device that does not answer.
pub(crate) const DEFAULT_CAPTURE_FPS: i32 = 30;

const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;

/// `_IOWR('V', 21, struct v4l2_streamparm)`.
///
/// The payload size is encoded into the request number, so this constant
/// and `V4l2StreamParm` have to agree with the kernel or the ioctl is
/// rejected outright -- which is what the size assertion in the tests
/// guards.
const VIDIOC_G_PARM: libc::c_ulong = 0xC0CC_5615;
const VIDIOC_S_PARM: libc::c_ulong = 0xC0CC_5616;

/// `_IOWR('V', 4, struct v4l2_format)`.
///
/// On 64-bit the `fmt` union is 8-byte aligned because `v4l2_window`
/// contains a pointer, so the struct is 208 bytes (4 type + 4 pad +
/// 200 union), not 204 like `v4l2_streamparm`. The size is part of the
/// request number; using 204 yields ENOTTY on aarch64.
const VIDIOC_G_FMT: libc::c_ulong = 0xC0D0_5604;

#[repr(C)]
#[derive(Default)]
struct V4l2Fract {
    numerator: u32,
    denominator: u32,
}

#[repr(C)]
#[derive(Default)]
struct V4l2CaptureParm {
    capability: u32,
    capturemode: u32,
    timeperframe: V4l2Fract,
    extendedmode: u32,
    readbuffers: u32,
    reserved: [u32; 4],
}

/// `struct v4l2_streamparm`. The kernel declares a 200-byte union after
/// `type`; we only read the capture arm, and pad out the rest so the
/// struct is the size the ioctl number claims.
#[repr(C)]
struct V4l2StreamParm {
    type_: u32,
    capture: V4l2CaptureParm,
    _reserved: [u8; 200 - std::mem::size_of::<V4l2CaptureParm>()],
}

/// `struct v4l2_format` on 64-bit: `type` is followed by 4 bytes of
/// padding so the `fmt` union (which includes a pointer-bearing
/// `v4l2_window`) is 8-byte aligned. `pix.width` / `pix.height` sit at
/// the start of that union for both single-plane and mplane layouts.
#[repr(C)]
struct V4l2Format {
    type_: u32,
    _pad: u32,
    width: u32,
    height: u32,
    _rest: [u8; 200 - 8],
}

fn open_device(device: &str) -> Option<libc::c_int> {
    let path = CString::new(device).ok()?;
    // SAFETY: `path` is a valid NUL-terminated C string that outlives the
    // call.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
    if fd < 0 {
        warn!(
            device,
            error = %std::io::Error::last_os_error(),
            "cannot open camera to query or set capture parameters"
        );
        return None;
    }
    Some(fd)
}

fn close_device(fd: libc::c_int) {
    // SAFETY: `fd` was returned by open and is not used afterwards.
    unsafe { libc::close(fd) };
}

/// Frames per second the driver reports for the current configuration, or
/// `None` if it cannot say.
pub(crate) fn query(device: &str) -> Option<u32> {
    let fd = open_device(device)?;
    let fps = query_fd(fd);
    close_device(fd);
    fps
}

fn query_fd(fd: libc::c_int) -> Option<u32> {
    for type_ in [
        V4L2_BUF_TYPE_VIDEO_CAPTURE,
        V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
    ] {
        let mut parm = parm_for_query(type_);
        // SAFETY: `parm` is a correctly sized, zero-initialised
        // v4l2_streamparm (asserted in the tests), and `fd` is a live
        // descriptor owned by the caller for the duration of the call.
        let rc = unsafe { libc::ioctl(fd, VIDIOC_G_PARM, &mut parm) };
        if rc == 0 {
            return parse_timeperframe(&parm.capture.timeperframe);
        }
    }
    warn!(
        error = %std::io::Error::last_os_error(),
        "VIDIOC_G_PARM failed; cannot read the configured capture rate"
    );
    None
}

fn parm_for_query(type_: u32) -> V4l2StreamParm {
    V4l2StreamParm {
        type_,
        capture: V4l2CaptureParm::default(),
        _reserved: [0; 200 - std::mem::size_of::<V4l2CaptureParm>()],
    }
}

fn parm_for_fps(type_: u32, fps: u32) -> V4l2StreamParm {
    V4l2StreamParm {
        type_,
        capture: V4l2CaptureParm {
            timeperframe: V4l2Fract {
                numerator: 1,
                denominator: fps,
            },
            ..V4l2CaptureParm::default()
        },
        _reserved: [0; 200 - std::mem::size_of::<V4l2CaptureParm>()],
    }
}

/// Best-effort `VIDIOC_S_PARM` for `fps`. Logs clamp or failure; never
/// returns an error -- the caller still uses the requested rate as the
/// expected capture rate.
pub(crate) fn apply(device: &str, fps: u32) {
    let Some(fd) = open_device(device) else {
        warn!(
            fps,
            "cannot apply capture frame rate; using the requested rate as the expectation"
        );
        return;
    };
    apply_fd(fd, fps);
    close_device(fd);
}

fn apply_fd(fd: libc::c_int, fps: u32) {
    for type_ in [
        V4L2_BUF_TYPE_VIDEO_CAPTURE,
        V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
    ] {
        let mut parm = parm_for_fps(type_, fps);
        // SAFETY: `parm` matches the S_PARM payload size; `fd` is live.
        let rc = unsafe { libc::ioctl(fd, VIDIOC_S_PARM, &mut parm) };
        if rc != 0 {
            continue;
        }
        match parse_timeperframe(&parm.capture.timeperframe) {
            Some(got) if got == fps => {
                info!(fps, "camera capture rate set to {fps} fps");
            }
            Some(got) => {
                warn!(
                    requested = fps,
                    driver = got,
                    "camera clamped capture rate from {fps} to {got} fps; \
                     using the requested rate as the expected rate"
                );
            }
            None => {
                warn!(
                    fps,
                    "camera accepted VIDIOC_S_PARM but reported no usable rate; \
                     using the requested rate as the expected rate"
                );
            }
        }
        return;
    }
    warn!(
        fps,
        error = %std::io::Error::last_os_error(),
        "VIDIOC_S_PARM failed; using the requested rate as the expected rate"
    );
}

/// Live capture size from `VIDIOC_G_FMT`, or `None` if the driver cannot
/// say. Fails closed: an unset `CAMERA_SIZE` / combined mode must not
/// fall back to videostream's 1920×1080 default.
pub(crate) fn query_size(device: &str) -> Option<(u32, u32)> {
    let fd = open_device(device)?;
    let size = query_size_fd(fd);
    close_device(fd);
    size
}

fn query_size_fd(fd: libc::c_int) -> Option<(u32, u32)> {
    for type_ in [
        V4L2_BUF_TYPE_VIDEO_CAPTURE,
        V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
    ] {
        let mut fmt = V4l2Format {
            type_,
            _pad: 0,
            width: 0,
            height: 0,
            _rest: [0; 192],
        };
        // SAFETY: `fmt` is the 208-byte 64-bit v4l2_format payload; `fd`
        // is live.
        let rc = unsafe { libc::ioctl(fd, VIDIOC_G_FMT, &mut fmt) };
        if rc == 0 && fmt.width > 0 && fmt.height > 0 {
            return Some((fmt.width, fmt.height));
        }
    }
    warn!(
        error = %std::io::Error::last_os_error(),
        "VIDIOC_G_FMT failed; cannot read the live capture size"
    );
    None
}

/// `timeperframe` is seconds per frame, so the rate is its reciprocal.
/// Both halves of the fraction must be non-zero for the driver response
/// to describe a usable interval and frame rate.
fn parse_timeperframe(timeperframe: &V4l2Fract) -> Option<u32> {
    if timeperframe.numerator == 0 || timeperframe.denominator == 0 {
        return None;
    }
    Some((f64::from(timeperframe.denominator) / f64::from(timeperframe.numerator)).round() as u32)
}

/// Turn what the driver reported into the rate to use, falling back when
/// the answer is missing or nonsensical.
pub(crate) fn resolve(reported: Option<u32>) -> i32 {
    match reported {
        Some(fps) if fps > 0 => fps as i32,
        _ => {
            warn!(
                assumed = DEFAULT_CAPTURE_FPS,
                "camera did not report a usable frame rate; assuming the default"
            );
            DEFAULT_CAPTURE_FPS
        }
    }
}

/// Effective capture rate: the requested mode FPS if any, otherwise the
/// driver-reported rate (or the historic 30 FPS fallback).
pub(crate) fn configured(device: &str, requested: Option<u32>) -> i32 {
    match requested {
        // The requested rate was applied before STREAMON in open_camera.
        // It remains the service expectation if the best-effort ioctl was
        // unsupported or clamped.
        Some(fps) if fps > 0 => fps as i32,
        _ => resolve(query(device)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamparm_matches_the_size_encoded_in_the_ioctl_number() {
        // The kernel derives the expected payload size from the request
        // number, so a struct of the wrong size fails the ioctl rather
        // than returning bad data. 204 = 4 (type) + 200 (union).
        assert_eq!(std::mem::size_of::<V4l2StreamParm>(), 204);
        assert_eq!((VIDIOC_G_PARM >> 16) & 0x3FFF, 204);
        assert_eq!((VIDIOC_S_PARM >> 16) & 0x3FFF, 204);
        assert_eq!(std::mem::size_of::<V4l2Format>(), 208);
        assert_eq!((VIDIOC_G_FMT >> 16) & 0x3FFF, 208);
    }

    #[test]
    fn parm_for_fps_requests_one_over_the_rate() {
        let parm = parm_for_fps(V4L2_BUF_TYPE_VIDEO_CAPTURE, 30);
        assert_eq!(parm.capture.timeperframe.numerator, 1);
        assert_eq!(parm.capture.timeperframe.denominator, 30);
    }

    #[test]
    fn resolve_uses_the_rate_the_driver_reported() {
        assert_eq!(resolve(Some(60)), 60);
        assert_eq!(resolve(Some(30)), 30);
    }

    #[test]
    fn resolve_falls_back_when_the_driver_says_nothing_useful() {
        assert_eq!(resolve(None), DEFAULT_CAPTURE_FPS);
        assert_eq!(resolve(Some(0)), DEFAULT_CAPTURE_FPS);
    }

    #[test]
    fn configured_prefers_the_requested_mode_fps() {
        assert_eq!(configured("/path/that/does/not/exist", Some(30)), 30);
        assert_eq!(configured("/path/that/does/not/exist", Some(15)), 15);
    }

    #[test]
    fn configured_queries_when_no_mode_fps_is_set() {
        assert_eq!(
            configured("/path/that/does/not/exist", None),
            DEFAULT_CAPTURE_FPS
        );
        assert_eq!(
            configured("/path/that/does/not/exist", Some(0)),
            DEFAULT_CAPTURE_FPS
        );
    }

    #[test]
    fn timeperframe_is_converted_to_rounded_frames_per_second() {
        assert_eq!(
            parse_timeperframe(&V4l2Fract {
                numerator: 1001,
                denominator: 60_000,
            }),
            Some(60)
        );
    }

    #[test]
    fn timeperframe_rejects_zero_numerator_or_denominator() {
        assert_eq!(
            parse_timeperframe(&V4l2Fract {
                numerator: 0,
                denominator: 60,
            }),
            None
        );
        assert_eq!(
            parse_timeperframe(&V4l2Fract {
                numerator: 1,
                denominator: 0,
            }),
            None
        );
    }

    #[test]
    fn query_rejects_a_path_with_an_embedded_nul() {
        assert_eq!(query("/dev/video\0invalid"), None);
        assert_eq!(query_size("/dev/video\0invalid"), None);
    }

    #[test]
    fn query_returns_none_when_the_device_cannot_be_opened() {
        assert_eq!(query("/path/that/does/not/exist"), None);
        assert_eq!(query_size("/path/that/does/not/exist"), None);
    }

    #[test]
    fn query_returns_none_when_the_device_rejects_the_ioctl() {
        assert_eq!(query("/dev/null"), None);
        assert_eq!(query_size("/dev/null"), None);
    }
}

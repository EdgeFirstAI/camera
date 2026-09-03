// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

//! The frame rate the camera is actually configured for.
//!
//! Capture rate is a property of the sensor mode the ISP was started in --
//! 60fps for the binned 1080p mode, 30fps for 4K on this platform -- so
//! asking the driver is asking the camera configuration itself, and no
//! separate setting can drift away from it.
//!
//! It used to be a hardcoded 30, which was wrong in both modes we ship.
//! The H.264 encoder was told half the real rate at 1080p60, and the
//! low-frame-rate warning compared against 30 rather than the configured
//! rate, so at 1080p60 it could not fire until capture had already
//! collapsed by more than half.
//!
//! `videostream` exposes neither `VIDIOC_G_PARM` nor the camera device's
//! file descriptor (`CameraBuffer::fd` is the DMA buffer, not the device),
//! so this opens the device node itself for the query rather than growing
//! a shared library for one field. That is a second open of a device the
//! capture path already holds, which is safe here: the query is an open,
//! one ioctl and a close, with no format negotiation or streaming, and
//! `v4l2-ctl -P` does exactly this against a live camera routinely.

use std::ffi::CString;
use tracing::warn;

/// Used when the driver cannot tell us. Matches the historical hardcoded
/// value, so behaviour is unchanged on a device that does not answer.
pub(crate) const DEFAULT_CAPTURE_FPS: i32 = 30;

const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;

/// `_IOWR('V', 21, struct v4l2_streamparm)`.
///
/// The payload size is encoded into the request number, so this constant
/// and `V4l2StreamParm` have to agree with the kernel or the ioctl is
/// rejected outright -- which is what the size assertion in the tests
/// guards.
const VIDIOC_G_PARM: libc::c_ulong = 0xC0CC_5615;

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

/// Frames per second the driver reports for the current configuration, or
/// `None` if it cannot say.
pub(crate) fn query(device: &str) -> Option<u32> {
    let path = CString::new(device).ok()?;
    // SAFETY: `path` is a valid NUL-terminated C string that outlives the
    // call.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
    if fd < 0 {
        warn!(
            device,
            error = %std::io::Error::last_os_error(),
            "cannot open camera to read its configured frame rate"
        );
        return None;
    }

    let fps = query_fd(fd);
    // SAFETY: `fd` was returned by open above and is not used afterwards.
    unsafe { libc::close(fd) };
    fps
}

fn query_fd(fd: libc::c_int) -> Option<u32> {
    let mut parm = V4l2StreamParm {
        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
        capture: V4l2CaptureParm::default(),
        _reserved: [0; 200 - std::mem::size_of::<V4l2CaptureParm>()],
    };

    // SAFETY: `parm` is a correctly sized, zero-initialised
    // v4l2_streamparm (asserted in the tests), and `fd` is a live
    // descriptor owned by the caller for the duration of the call.
    let rc = unsafe { libc::ioctl(fd, VIDIOC_G_PARM, &mut parm) };
    if rc != 0 {
        warn!(
            error = %std::io::Error::last_os_error(),
            "VIDIOC_G_PARM failed; cannot read the configured capture rate"
        );
        return None;
    }

    // timeperframe is seconds per frame, so the rate is its reciprocal.
    let tpf = &parm.capture.timeperframe;
    if tpf.numerator == 0 {
        return None;
    }
    Some((f64::from(tpf.denominator) / f64::from(tpf.numerator)).round() as u32)
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
}

// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Driver quirks.
//!
//! The i.MX 8M Plus VeriSilicon ISP (vvcam, driver `viv_v4l2_device`):
//!
//! - Flip goes through the `V4L2_CID_VIV_EXTCTRL` JSON control
//!   (`dwe.s.hflip` / `dwe.s.vflip`), tried only after `HFLIP`/`VFLIP`.
//! - While `isp_media_server` restarts, `open()` fails with `ENOENT`, then
//!   no frame arrives for about 4 s. The backend reports both as
//!   `NotReady` (see `device::open` and the first-frame timeout); neither
//!   ever appears as `EAGAIN`.
//! - A bare `STREAMOFF`/`STREAMON` restart delivers no frames and the next
//!   close crashes `isp_media_server`, so `stop()` always frees the queue
//!   with `REQBUFS(0)`.
//! - `CREATE_BUFS` returns `ENOTTY`; buffers are always allocated with
//!   `REQBUFS`.

use edgefirst_v4l2::controls::{self, ControlInfo, ControlValue};
use edgefirst_v4l2::device::Device;
use edgefirst_v4l2::uapi;

/// `V4L2_CID_VIV_EXTCTRL`: `VIV_CUSTOM_CID_BASE + 1`, where the base is
/// `V4L2_CID_USER_BASE | 0xf000`.
pub(crate) const VIV_EXTCTRL: u32 = (uapi::V4L2_CID_BASE | 0xf000) + 1;

/// The JSON request that sets one axis of the ISP's dewarp flip.
pub(crate) fn viv_flip_json(axis: Axis, on: bool) -> String {
    let (name, key) = match axis {
        Axis::Horizontal => ("dwe.s.hflip", "hflip"),
        Axis::Vertical => ("dwe.s.vflip", "vflip"),
    };
    format!(r#"{{"id": "{name}", "dwe" : {{"{key}": {on}}}}}"#)
}

/// A flip axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Axis {
    Horizontal,
    Vertical,
}

/// Sets one flip axis through the ISP's JSON control.
pub(crate) fn viv_flip(
    dev: &Device,
    info: &ControlInfo,
    axis: Axis,
    on: bool,
) -> edgefirst_v4l2::Result<()> {
    controls::set(dev, info, &ControlValue::String(viv_flip_json(axis, on))).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extctrl_id_matches_the_vendor_header() {
        assert_eq!(VIV_EXTCTRL, 0x0098_f901);
    }

    #[test]
    fn flip_json_matches_the_isp_protocol() {
        assert_eq!(
            viv_flip_json(Axis::Horizontal, true),
            r#"{"id": "dwe.s.hflip", "dwe" : {"hflip": true}}"#
        );
        assert_eq!(
            viv_flip_json(Axis::Vertical, false),
            r#"{"id": "dwe.s.vflip", "dwe" : {"vflip": false}}"#
        );
    }
}

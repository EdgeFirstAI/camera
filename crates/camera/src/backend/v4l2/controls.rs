// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Backend-neutral controls over V4L2 control IDs.
//!
//! Mirroring is `HFLIP`/`VFLIP`, exposure `EXPOSURE_AUTO` with
//! `EXPOSURE_ABSOLUTE` (100 µs units, reported in µs), gain `AUTOGAIN` with
//! `GAIN`, white balance `AUTO_WHITE_BALANCE` with
//! `WHITE_BALANCE_TEMPERATURE`. Frame rate is `S_PARM`, handled by the
//! camera. Every set reads the value back, so a clamp is reported.

use std::collections::BTreeMap;
use std::time::Duration;

use edgefirst_v4l2::controls::{self, ControlInfo as V4l2Info, ControlType, ControlValue as Raw};
use edgefirst_v4l2::device::Device;
use edgefirst_v4l2::uapi;

use super::device::v4l2_error;
use super::quirks::{self, Axis};
use crate::{
    Applied, Backend, Control, ControlFlags, ControlId, ControlInfo, ControlSet, ControlValue,
    Error, ErrorKind, Exposure, Gain, Mirror, Result, Unsupported, WhiteBalance,
};

/// `EXPOSURE_ABSOLUTE` counts in units of 100 µs.
const EXPOSURE_UNIT_US: i64 = 100;

/// The device's controls, as queried at open.
#[derive(Debug, Default)]
pub(crate) struct Controls {
    infos: BTreeMap<u32, V4l2Info>,
    /// The mirror last set through the vvcam JSON control, which cannot be
    /// read back: bit 0 horizontal, bit 1 vertical.
    viv_mirror: std::cell::Cell<u8>,
}

impl Controls {
    /// Queries every control. A device without controls has an empty set.
    pub(crate) fn query(dev: &Device) -> Self {
        let infos = controls::query_all(dev)
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.control_type != ControlType::CtrlClass && !c.flags.is_disabled())
            .map(|c| (c.id, c))
            .collect();
        Self {
            infos,
            viv_mirror: std::cell::Cell::new(0),
        }
    }

    /// Whether flips must go through the vvcam JSON control.
    fn viv_flip_only(&self) -> bool {
        self.info(uapi::V4L2_CID_HFLIP).is_none()
            && self.info(uapi::V4L2_CID_VFLIP).is_none()
            && self.info(quirks::VIV_EXTCTRL).is_some()
    }

    fn info(&self, cid: u32) -> Option<&V4l2Info> {
        self.infos.get(&cid)
    }

    /// The SDK control set this device supports.
    pub(crate) fn control_set(&self) -> ControlSet {
        let mut set = ControlSet::new();
        let int = ControlValue::Int;
        if self.info(uapi::V4L2_CID_HFLIP).is_some()
            || self.info(uapi::V4L2_CID_VFLIP).is_some()
            || self.info(quirks::VIV_EXTCTRL).is_some()
        {
            set.insert(
                ControlId::Mirror,
                ControlInfo::new(int(0), int(3), int(1), int(0)),
            );
        }
        if let Some(c) = self.info(uapi::V4L2_CID_EXPOSURE_ABSOLUTE) {
            let us = |v: i64| int(v * EXPOSURE_UNIT_US);
            set.insert(
                ControlId::Exposure,
                with_flags(
                    ControlInfo::new(
                        us(c.minimum),
                        us(c.maximum),
                        us(c.step as i64),
                        us(c.default_value),
                    ),
                    c,
                ),
            );
        }
        for (cid, id) in [
            (uapi::V4L2_CID_GAIN, ControlId::Gain),
            (
                uapi::V4L2_CID_WHITE_BALANCE_TEMPERATURE,
                ControlId::WhiteBalance,
            ),
        ] {
            if let Some(c) = self.info(cid) {
                set.insert(id, with_flags(range(c), c));
            }
        }
        for c in self.infos.values() {
            if matches!(
                c.control_type,
                ControlType::Integer
                    | ControlType::Boolean
                    | ControlType::Menu
                    | ControlType::Integer64
            ) {
                set.insert(
                    ControlId::Custom {
                        backend: Backend::V4l2,
                        id: u64::from(c.id),
                    },
                    with_flags(range(c), c),
                );
            }
        }
        set
    }

    /// Reads a control's current value.
    pub(crate) fn get(&self, dev: &Device, id: ControlId) -> Result<ControlValue> {
        let read = |cid: u32| -> Result<i64> {
            let info = self.info(cid).ok_or_else(|| unsupported(id))?;
            controls::get(dev, info)
                .map(as_i64)
                .map_err(|e| v4l2_error("G_EXT_CTRLS", e))
        };
        Ok(match id {
            ControlId::Mirror if self.viv_flip_only() => {
                ControlValue::Int(i64::from(self.viv_mirror.get()))
            }
            ControlId::Mirror => {
                let h = self
                    .info(uapi::V4L2_CID_HFLIP)
                    .map_or(Ok(0), |_| read(uapi::V4L2_CID_HFLIP))?;
                let v = self
                    .info(uapi::V4L2_CID_VFLIP)
                    .map_or(Ok(0), |_| read(uapi::V4L2_CID_VFLIP))?;
                ControlValue::Int((h != 0) as i64 | (((v != 0) as i64) << 1))
            }
            ControlId::Exposure => {
                ControlValue::Int(read(uapi::V4L2_CID_EXPOSURE_ABSOLUTE)? * EXPOSURE_UNIT_US)
            }
            ControlId::Gain => ControlValue::Int(read(uapi::V4L2_CID_GAIN)?),
            ControlId::WhiteBalance => {
                ControlValue::Int(read(uapi::V4L2_CID_WHITE_BALANCE_TEMPERATURE)?)
            }
            ControlId::Custom {
                backend: Backend::V4l2,
                id: cid,
            } => ControlValue::Int(read(u32::try_from(cid).map_err(|_| unsupported(id))?)?),
            _ => return Err(unsupported(id)),
        })
    }

    /// Applies a control other than the frame rate.
    pub(crate) fn set(&self, dev: &Device, ctl: Control) -> Result<Applied> {
        match ctl {
            Control::Mirror(m) => self.mirror(dev, m),
            Control::Exposure(Exposure::Auto) => self.exposure_auto(dev),
            Control::Exposure(Exposure::Manual(d)) => self.exposure_manual(dev, d),
            Control::Gain(Gain::Auto) => self.write_flag(dev, uapi::V4L2_CID_AUTOGAIN, true),
            Control::Gain(Gain::Manual(g)) => {
                self.write_flag_if_present(dev, uapi::V4L2_CID_AUTOGAIN, false)?;
                self.write(dev, uapi::V4L2_CID_GAIN, g.round() as i64)
            }
            Control::WhiteBalance(WhiteBalance::Auto) => {
                self.write_flag(dev, uapi::V4L2_CID_AUTO_WHITE_BALANCE, true)
            }
            Control::WhiteBalance(WhiteBalance::Manual { kelvin }) => {
                self.write_flag_if_present(dev, uapi::V4L2_CID_AUTO_WHITE_BALANCE, false)?;
                self.write(
                    dev,
                    uapi::V4L2_CID_WHITE_BALANCE_TEMPERATURE,
                    i64::from(kelvin),
                )
            }
            Control::Custom {
                backend: Backend::V4l2,
                id,
                value,
            } => {
                let Ok(cid) = u32::try_from(id) else {
                    return Ok(Applied::Unsupported(Unsupported::Camera));
                };
                let v = match value {
                    ControlValue::Bool(b) => i64::from(b),
                    ControlValue::Int(i) => i,
                    ControlValue::Float(f) => f.round() as i64,
                };
                self.write(dev, cid, v)
            }
            _ => Ok(Applied::Unsupported(Unsupported::Backend)),
        }
    }

    fn mirror(&self, dev: &Device, m: Mirror) -> Result<Applied> {
        let (h, v) = match m {
            Mirror::None => (false, false),
            Mirror::Horizontal => (true, false),
            Mirror::Vertical => (false, true),
            Mirror::Both => (true, true),
        };
        let has_h = self.info(uapi::V4L2_CID_HFLIP).is_some();
        let has_v = self.info(uapi::V4L2_CID_VFLIP).is_some();
        let standard = (!h || has_h) && (!v || has_v) && (has_h || has_v);
        if standard {
            let flipped = (|| -> Result<()> {
                if has_h {
                    self.write(dev, uapi::V4L2_CID_HFLIP, i64::from(h))?;
                }
                if has_v {
                    self.write(dev, uapi::V4L2_CID_VFLIP, i64::from(v))?;
                }
                Ok(())
            })();
            if flipped.is_ok() || self.info(quirks::VIV_EXTCTRL).is_none() {
                return flipped.map(|()| Applied::Driver);
            }
        }
        // vvcam: flips are the ISP dewarp unit's, through its JSON control.
        let Some(viv) = self.info(quirks::VIV_EXTCTRL) else {
            return Ok(Applied::Unsupported(Unsupported::Camera));
        };
        for (axis, on) in [(Axis::Horizontal, h), (Axis::Vertical, v)] {
            quirks::viv_flip(dev, viv, axis, on)
                .map_err(|e| v4l2_error("S_EXT_CTRLS VIV_EXTCTRL", e))?;
        }
        self.viv_mirror.set(u8::from(h) | (u8::from(v) << 1));
        Ok(Applied::Driver)
    }

    fn exposure_auto(&self, dev: &Device) -> Result<Applied> {
        let Some(info) = self.info(uapi::V4L2_CID_EXPOSURE_AUTO) else {
            return Ok(Applied::Unsupported(Unsupported::Camera));
        };
        // UVC cameras usually offer only manual and aperture priority.
        let mode = [
            uapi::V4L2_EXPOSURE_AUTO,
            uapi::V4L2_EXPOSURE_APERTURE_PRIORITY,
        ]
        .into_iter()
        .find(|&m| info.menu.is_empty() || info.menu.iter().any(|i| i.index == m as u32))
        .unwrap_or(uapi::V4L2_EXPOSURE_AUTO);
        self.write(dev, uapi::V4L2_CID_EXPOSURE_AUTO, i64::from(mode))?;
        Ok(Applied::Driver)
    }

    fn exposure_manual(&self, dev: &Device, d: Duration) -> Result<Applied> {
        if self.info(uapi::V4L2_CID_EXPOSURE_ABSOLUTE).is_none() {
            return Ok(Applied::Unsupported(Unsupported::Camera));
        }
        if self.info(uapi::V4L2_CID_EXPOSURE_AUTO).is_some() {
            self.write(
                dev,
                uapi::V4L2_CID_EXPOSURE_AUTO,
                i64::from(uapi::V4L2_EXPOSURE_MANUAL),
            )?;
        }
        let units = (d.as_micros() as i64 + EXPOSURE_UNIT_US / 2) / EXPOSURE_UNIT_US;
        Ok(
            match self.write(dev, uapi::V4L2_CID_EXPOSURE_ABSOLUTE, units)? {
                Applied::Clamped(ControlValue::Int(v)) => {
                    Applied::Clamped(ControlValue::Int(v * EXPOSURE_UNIT_US))
                }
                other => other,
            },
        )
    }

    fn write_flag(&self, dev: &Device, cid: u32, on: bool) -> Result<Applied> {
        if self.info(cid).is_none() {
            return Ok(Applied::Unsupported(Unsupported::Camera));
        }
        self.write(dev, cid, i64::from(on))
    }

    fn write_flag_if_present(&self, dev: &Device, cid: u32, on: bool) -> Result<()> {
        if self.info(cid).is_some() {
            self.write(dev, cid, i64::from(on))?;
        }
        Ok(())
    }

    /// Writes `value`, returning `Clamped` with the driver's value when it
    /// differs.
    fn write(&self, dev: &Device, cid: u32, value: i64) -> Result<Applied> {
        let Some(info) = self.info(cid) else {
            return Ok(Applied::Unsupported(Unsupported::Camera));
        };
        if info.flags.is_read_only() {
            return Ok(Applied::Unsupported(Unsupported::Camera));
        }
        let raw = if info.control_type == ControlType::Integer64 {
            Raw::Integer64(value)
        } else {
            Raw::Integer(value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
        };
        let got = controls::set(dev, info, &raw).map_err(|e| v4l2_error("S_EXT_CTRLS", e))?;
        let got = as_i64(got);
        Ok(if got == value {
            Applied::Driver
        } else {
            Applied::Clamped(ControlValue::Int(got))
        })
    }
}

fn as_i64(v: Raw) -> i64 {
    match v {
        Raw::Integer(i) => i64::from(i),
        Raw::Integer64(i) => i,
        _ => 0,
    }
}

fn range(c: &V4l2Info) -> ControlInfo {
    let int = ControlValue::Int;
    ControlInfo::new(
        int(c.minimum),
        int(c.maximum),
        int(c.step as i64),
        int(c.default_value),
    )
}

fn with_flags(mut info: ControlInfo, c: &V4l2Info) -> ControlInfo {
    info.flags = ControlFlags {
        read_only: c.flags.is_read_only(),
        ..ControlFlags::default()
    };
    info
}

fn unsupported(id: ControlId) -> Error {
    Error::new(
        ErrorKind::InvalidConfig,
        format!("{id:?} is not supported by this camera"),
    )
}

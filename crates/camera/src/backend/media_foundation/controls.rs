// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Exposure, gain and white balance through `IAMCameraControl` and
//! `IAMVideoProcAmp`. Only what the driver reports a range for is offered.

use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::{
    ERROR_NOT_FOUND, ERROR_NOT_SUPPORTED, ERROR_SET_NOT_FOUND, E_NOTIMPL,
};
use windows::Win32::Media::DirectShow::{
    CameraControl_Exposure, CameraControl_Flags_Auto, CameraControl_Flags_Manual, IAMCameraControl,
    IAMVideoProcAmp, VideoProcAmp_Flags_Auto, VideoProcAmp_Flags_Manual, VideoProcAmp_Gain,
    VideoProcAmp_WhiteBalance,
};
use windows::Win32::Media::MediaFoundation::IMFMediaSource;

use super::com::{error, Agile};
use crate::{
    Applied, ControlId, ControlInfo, ControlSet, ControlValue, Exposure, Gain, Result, Unsupported,
    WhiteBalance,
};

/// A property's range, as the driver reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Range {
    pub(super) min: i32,
    pub(super) max: i32,
    pub(super) step: i32,
    pub(super) default: i32,
    /// The driver can set it automatically.
    pub(super) auto: bool,
    /// The driver takes a value.
    pub(super) manual: bool,
}

impl Range {
    /// The nearest value the driver takes.
    pub(super) fn nearest(&self, v: i64) -> i32 {
        let v = v.clamp(i64::from(self.min), i64::from(self.max));
        let step = i64::from(self.step.max(1));
        let min = i64::from(self.min);
        let v = min + (v - min + step / 2) / step * step;
        v.min(i64::from(self.max)) as i32
    }
}

/// Whether a driver refused a control request as unsupported.
fn refused(code: HRESULT) -> bool {
    code == E_NOTIMPL
        || [ERROR_NOT_SUPPORTED, ERROR_NOT_FOUND, ERROR_SET_NOT_FOUND]
            .iter()
            .any(|e| code == e.to_hresult())
}

/// Exposure is set in log2 seconds; the SDK reports microseconds.
pub(super) fn exposure_us(log2_seconds: i32) -> i64 {
    (2f64.powi(log2_seconds) * 1e6).round() as i64
}

/// The log2-seconds exposure nearest `us` microseconds.
pub(super) fn exposure_log2(us: f64) -> i64 {
    (us.max(1.0) / 1e6).log2().round() as i64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Camera,
    Amp,
}

/// A property of one of the two interfaces.
#[derive(Debug, Clone, Copy)]
struct Property {
    on: Owner,
    id: i32,
    range: Range,
}

/// The camera's controls.
#[derive(Debug)]
pub(super) struct Controls {
    camera: Option<Agile<IAMCameraControl>>,
    amp: Option<Agile<IAMVideoProcAmp>>,
    exposure: Option<Property>,
    gain: Option<Property>,
    white_balance: Option<Property>,
}

impl Controls {
    pub(super) fn of(source: &IMFMediaSource) -> Self {
        let camera = source.cast::<IAMCameraControl>().ok().map(Agile);
        let amp = source.cast::<IAMVideoProcAmp>().ok().map(Agile);
        let mut c = Self {
            camera,
            amp,
            exposure: None,
            gain: None,
            white_balance: None,
        };
        c.exposure = c.property(Owner::Camera, CameraControl_Exposure.0);
        c.gain = c.property(Owner::Amp, VideoProcAmp_Gain.0);
        c.white_balance = c.property(Owner::Amp, VideoProcAmp_WhiteBalance.0);
        c
    }

    fn property(&self, on: Owner, id: i32) -> Option<Property> {
        let (mut min, mut max, mut step, mut default, mut caps) = (0, 0, 0, 0, 0);
        // SAFETY: live interfaces and valid out-pointers.
        let got = unsafe {
            match on {
                Owner::Camera => self.camera.as_ref()?.GetRange(
                    id,
                    &mut min,
                    &mut max,
                    &mut step,
                    &mut default,
                    &mut caps,
                ),
                Owner::Amp => self.amp.as_ref()?.GetRange(
                    id,
                    &mut min,
                    &mut max,
                    &mut step,
                    &mut default,
                    &mut caps,
                ),
            }
        };
        let (auto, manual) = match on {
            Owner::Camera => (CameraControl_Flags_Auto.0, CameraControl_Flags_Manual.0),
            Owner::Amp => (VideoProcAmp_Flags_Auto.0, VideoProcAmp_Flags_Manual.0),
        };
        (got.is_ok() && max >= min).then_some(Property {
            on,
            id,
            range: Range {
                min,
                max,
                step,
                default,
                auto: caps & auto != 0,
                manual: caps & manual != 0,
            },
        })
    }

    fn get(&self, p: &Property) -> Result<(i32, bool)> {
        let (mut value, mut flags) = (0, 0);
        // SAFETY: live interfaces and valid out-pointers.
        let got = unsafe {
            match p.on {
                Owner::Camera => self
                    .camera
                    .as_ref()
                    .map(|c| c.Get(p.id, &mut value, &mut flags)),
                Owner::Amp => self
                    .amp
                    .as_ref()
                    .map(|a| a.Get(p.id, &mut value, &mut flags)),
            }
        };
        if let Some(Err(e)) = got {
            return Err(error(e, "read a camera control"));
        }
        Ok((value, flags & 1 != 0))
    }

    /// Sets a property; `false` when the driver refuses the request as
    /// unsupported, which some do for a mode their range advertises.
    fn set(&self, p: &Property, value: i32, auto: bool) -> Result<bool> {
        let flags = match (p.on, auto) {
            (Owner::Camera, true) => CameraControl_Flags_Auto.0,
            (Owner::Camera, false) => CameraControl_Flags_Manual.0,
            (Owner::Amp, true) => VideoProcAmp_Flags_Auto.0,
            (Owner::Amp, false) => VideoProcAmp_Flags_Manual.0,
        };
        // SAFETY: live interfaces.
        let set = unsafe {
            match p.on {
                Owner::Camera => self.camera.as_ref().map(|c| c.Set(p.id, value, flags)),
                Owner::Amp => self.amp.as_ref().map(|a| a.Set(p.id, value, flags)),
            }
        };
        match set {
            Some(Err(e)) if refused(e.code()) => Ok(false),
            Some(Err(e)) => Err(error(e, "set a camera control")),
            _ => Ok(true),
        }
    }

    /// Adds the supported controls to `set`.
    pub(super) fn describe(&self, set: &mut ControlSet) {
        let i = ControlValue::Int;
        if let Some(r) = self.exposure.map(|p| p.range) {
            set.insert(
                ControlId::Exposure,
                ControlInfo::new(
                    i(exposure_us(r.min)),
                    i(exposure_us(r.max)),
                    i(0),
                    i(exposure_us(r.default)),
                ),
            );
        }
        for (id, p) in [
            (ControlId::Gain, self.gain),
            (ControlId::WhiteBalance, self.white_balance),
        ] {
            if let Some(r) = p.map(|p| p.range) {
                set.insert(
                    id,
                    ControlInfo::new(
                        i(r.min.into()),
                        i(r.max.into()),
                        i(r.step.into()),
                        i(r.default.into()),
                    ),
                );
            }
        }
    }

    /// The current value of `id`, or `None` when this is not one of its
    /// controls.
    pub(super) fn value(&self, id: ControlId) -> Option<Result<ControlValue>> {
        let (p, us) = match id {
            ControlId::Exposure => (self.exposure?, true),
            ControlId::Gain => (self.gain?, false),
            ControlId::WhiteBalance => (self.white_balance?, false),
            _ => return None,
        };
        Some(
            self.get(&p)
                .map(|(v, _)| ControlValue::Int(if us { exposure_us(v) } else { i64::from(v) })),
        )
    }

    /// Sets a property automatic, or to the nearest value the driver takes.
    fn apply(&self, p: Option<Property>, manual: Option<i64>) -> Result<Applied> {
        let Some(p) = p else {
            return Ok(Applied::Unsupported(Unsupported::Camera));
        };
        let Some(asked) = manual else {
            if !p.range.auto {
                return Ok(Applied::Unsupported(Unsupported::Camera));
            }
            let (value, _) = self.get(&p)?;
            if !self.set(&p, value, true)? {
                return Ok(Applied::Unsupported(Unsupported::Camera));
            }
            return Ok(Applied::Driver);
        };
        if !p.range.manual {
            return Ok(Applied::Unsupported(Unsupported::Camera));
        }
        let got = p.range.nearest(asked);
        if !self.set(&p, got, false)? {
            return Ok(Applied::Unsupported(Unsupported::Camera));
        }
        Ok(if i64::from(got) == asked {
            Applied::Driver
        } else {
            Applied::Clamped(ControlValue::Int(got.into()))
        })
    }

    pub(super) fn exposure(&self, e: Exposure) -> Result<Applied> {
        match e {
            Exposure::Auto => self.apply(self.exposure, None),
            Exposure::Manual(d) => {
                let asked = i64::try_from(d.as_micros()).unwrap_or(i64::MAX);
                let log2 = exposure_log2(asked as f64);
                let applied = self.apply(self.exposure, Some(log2))?;
                if let Applied::Unsupported(_) = applied {
                    return Ok(applied);
                }
                // The driver's value is in log2 seconds; report microseconds.
                let got = self
                    .exposure
                    .map_or(asked, |p| exposure_us(p.range.nearest(log2)));
                Ok(if got == asked {
                    Applied::Driver
                } else {
                    Applied::Clamped(ControlValue::Int(got))
                })
            }
        }
    }

    pub(super) fn gain(&self, g: Gain) -> Result<Applied> {
        match g {
            Gain::Auto => self.apply(self.gain, None),
            Gain::Manual(v) => self.apply(self.gain, Some(f64::from(v).round() as i64)),
        }
    }

    pub(super) fn white_balance(&self, wb: WhiteBalance) -> Result<Applied> {
        match wb {
            WhiteBalance::Auto => self.apply(self.white_balance, None),
            WhiteBalance::Manual { kelvin } => {
                self.apply(self.white_balance, Some(i64::from(kelvin)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_snap_to_the_driver_step_within_the_range() {
        let r = Range {
            min: 2800,
            max: 6500,
            step: 100,
            default: 4600,
            auto: true,
            manual: true,
        };
        assert_eq!(r.nearest(5555), 5600);
        assert_eq!(r.nearest(9000), 6500);
        assert_eq!(r.nearest(0), 2800);
    }

    #[test]
    fn exposure_converts_between_log2_seconds_and_microseconds() {
        assert_eq!(exposure_us(-5), 31_250);
        assert_eq!(exposure_us(0), 1_000_000);
        assert_eq!(exposure_log2(33_333.0), -5);
        assert_eq!(exposure_log2(0.0), -20);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Backend-neutral camera controls.
//!
//! Controls are requests. A backend applies what it can and reports the
//! outcome as [`Applied`]; an unsupported control is reported, never fatal.

use std::time::Duration;

use crate::{Backend, Mirror};

/// Exposure setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Exposure {
    /// Automatic exposure.
    Auto,
    /// Fixed exposure time.
    Manual(Duration),
}

/// Analogue gain setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gain {
    /// Automatic gain.
    Auto,
    /// Fixed gain multiplier.
    Manual(f32),
}

/// White-balance setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WhiteBalance {
    /// Automatic white balance.
    Auto,
    /// Fixed colour temperature.
    Manual {
        /// Colour temperature in kelvin.
        kelvin: u32,
    },
}

/// A control value as a backend reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ControlValue {
    /// A boolean control.
    Bool(bool),
    /// An integer control.
    Int(i64),
    /// A floating-point control.
    Float(f64),
}

/// A control request.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Control {
    /// Mirroring.
    Mirror(Mirror),
    /// Frame rate in frames per second.
    FrameRate(f64),
    /// Exposure.
    Exposure(Exposure),
    /// Analogue gain.
    Gain(Gain),
    /// White balance.
    WhiteBalance(WhiteBalance),
    /// A backend-specific control: a V4L2 CID or a libcamera control id.
    Custom {
        /// Backend the id belongs to.
        backend: Backend,
        /// Backend-specific control id.
        id: u64,
        /// Value to apply.
        value: ControlValue,
    },
}

impl Control {
    /// The identity of this control.
    pub fn id(&self) -> ControlId {
        match self {
            Self::Mirror(_) => ControlId::Mirror,
            Self::FrameRate(_) => ControlId::FrameRate,
            Self::Exposure(_) => ControlId::Exposure,
            Self::Gain(_) => ControlId::Gain,
            Self::WhiteBalance(_) => ControlId::WhiteBalance,
            Self::Custom { backend, id, .. } => ControlId::Custom {
                backend: *backend,
                id: *id,
            },
        }
    }
}

/// Identity of a control.
///
/// The variant documentation gives the [`ControlValue`] that
/// [`Camera::get_control`](crate::Camera::get_control) returns and that
/// [`ControlInfo`] ranges use for it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ControlId {
    /// Mirroring: `Int` 0 none, 1 horizontal, 2 vertical, 3 both.
    Mirror,
    /// Frame rate: `Float` frames per second.
    FrameRate,
    /// Exposure time: `Int` microseconds.
    Exposure,
    /// Analogue gain: `Int` in the driver's units.
    Gain,
    /// White balance: `Int` colour temperature in kelvin.
    WhiteBalance,
    /// A backend-specific control: the backend's raw value (a V4L2 CID's
    /// integer value).
    Custom {
        /// Backend the id belongs to.
        backend: Backend,
        /// Backend-specific control id.
        id: u64,
    },
}

/// Flags describing a supported control.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ControlFlags {
    /// The control can be read but not set.
    pub read_only: bool,
    /// The backend accepts the control but may ignore it (for example
    /// `FrameDurationLimits` on the i.MX 95 neo pipeline).
    pub advisory: bool,
}

/// Range and default of a supported control.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControlInfo {
    /// Minimum value.
    pub min: ControlValue,
    /// Maximum value.
    pub max: ControlValue,
    /// Step between valid values.
    pub step: ControlValue,
    /// Default value.
    pub default: ControlValue,
    /// Flags.
    pub flags: ControlFlags,
}

impl ControlInfo {
    /// Creates control information with default flags.
    pub fn new(
        min: ControlValue,
        max: ControlValue,
        step: ControlValue,
        default: ControlValue,
    ) -> Self {
        Self {
            min,
            max,
            step,
            default,
            flags: ControlFlags::default(),
        }
    }
}

/// The controls a camera supports, with their ranges.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ControlSet {
    entries: Vec<(ControlId, ControlInfo)>,
}

impl ControlSet {
    /// Creates an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces a control.
    pub fn insert(&mut self, id: ControlId, info: ControlInfo) {
        match self.entries.iter_mut().find(|(k, _)| *k == id) {
            Some(entry) => entry.1 = info,
            None => self.entries.push((id, info)),
        }
    }

    /// Information for `id`, or `None` when unsupported.
    pub fn get(&self, id: ControlId) -> Option<&ControlInfo> {
        self.entries.iter().find(|(k, _)| *k == id).map(|(_, v)| v)
    }

    /// Whether `id` is supported.
    pub fn contains(&self, id: ControlId) -> bool {
        self.get(id).is_some()
    }

    /// Iterates over the supported controls.
    pub fn iter(&self) -> impl Iterator<Item = (ControlId, &ControlInfo)> {
        self.entries.iter().map(|(k, v)| (*k, v))
    }

    /// Number of supported controls.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no control is supported.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Outcome of applying a control.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Applied {
    /// The driver accepted the value as given.
    Driver,
    /// The driver applied this adjusted value.
    Clamped(ControlValue),
    /// The control was not applied.
    Unsupported(Unsupported),
}

/// Why a control was not applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unsupported {
    /// This backend cannot express the control.
    Backend,
    /// This camera does not offer the control.
    Camera,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_set_replaces_by_id() {
        let mut set = ControlSet::new();
        let a = ControlInfo::new(
            ControlValue::Float(1.0),
            ControlValue::Float(30.0),
            ControlValue::Float(0.0),
            ControlValue::Float(30.0),
        );
        let b = ControlInfo {
            max: ControlValue::Float(60.0),
            ..a
        };
        set.insert(ControlId::FrameRate, a);
        set.insert(ControlId::FrameRate, b);
        assert_eq!(set.len(), 1);
        assert_eq!(
            set.get(ControlId::FrameRate).unwrap().max,
            ControlValue::Float(60.0)
        );
        assert!(!set.contains(ControlId::Gain));
    }

    #[test]
    fn custom_control_id_keeps_backend() {
        let c = Control::Custom {
            backend: Backend::V4l2,
            id: 0x0098_0914,
            value: ControlValue::Bool(true),
        };
        assert_eq!(
            c.id(),
            ControlId::Custom {
                backend: Backend::V4l2,
                id: 0x0098_0914
            }
        );
    }
}

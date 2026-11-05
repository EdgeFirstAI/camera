// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The shim's C ABI, mirroring `shim/efcam_lc.h`.
//!
//! The header is the definition; `tests/layout.rs` compiles it and checks
//! every size and offset here against it.

#![allow(non_camel_case_types, missing_docs)]

use std::ffi::{c_char, c_int};

/// The ABI version this crate speaks; a shim reporting another is refused.
pub const EFCAM_LC_ABI_VERSION: u32 = 1;
pub const EFCAM_LC_MAX_PLANES: usize = 4;
pub const EFCAM_LC_MAX_DEVICES: usize = 8;

pub const EFCAM_LC_ROLE_RAW: u32 = 0;
pub const EFCAM_LC_ROLE_STILL: u32 = 1;
pub const EFCAM_LC_ROLE_VIDEO: u32 = 2;
pub const EFCAM_LC_ROLE_VIEWFINDER: u32 = 3;

pub const EFCAM_LC_ORIENTATION_DEFAULT: u32 = 0;
pub const EFCAM_LC_ORIENTATION_ROTATE0: u32 = 1;
pub const EFCAM_LC_ORIENTATION_ROTATE0_MIRROR: u32 = 2;
pub const EFCAM_LC_ORIENTATION_ROTATE180: u32 = 3;
pub const EFCAM_LC_ORIENTATION_ROTATE180_MIRROR: u32 = 4;
pub const EFCAM_LC_ORIENTATION_ROTATE90_MIRROR: u32 = 5;
pub const EFCAM_LC_ORIENTATION_ROTATE270: u32 = 6;
pub const EFCAM_LC_ORIENTATION_ROTATE270_MIRROR: u32 = 7;
pub const EFCAM_LC_ORIENTATION_ROTATE90: u32 = 8;

pub const EFCAM_LC_CONTROL_FRAME_DURATION_LIMITS: u32 = 0;
pub const EFCAM_LC_CONTROL_EXPOSURE_TIME: u32 = 1;
pub const EFCAM_LC_CONTROL_ANALOGUE_GAIN: u32 = 2;
pub const EFCAM_LC_CONTROL_AWB_ENABLE: u32 = 3;
pub const EFCAM_LC_CONTROL_COLOUR_TEMPERATURE: u32 = 4;
pub const EFCAM_LC_CONTROL_COLOUR_GAINS: u32 = 5;

pub const EFCAM_LC_STATUS_OK: u32 = 0;
pub const EFCAM_LC_STATUS_ERROR: u32 = 1;
pub const EFCAM_LC_STATUS_CANCELLED: u32 = 2;

pub const EFCAM_LC_MODE_UNSET: i32 = -1;
pub const EFCAM_LC_MODE_AUTO: i32 = 0;
pub const EFCAM_LC_MODE_MANUAL: i32 = 1;

/// Opaque `efcam_lc_mgr`.
#[repr(C)]
#[derive(Debug)]
pub struct efcam_lc_mgr {
    _private: [u8; 0],
}

/// Opaque `efcam_lc_cam`.
#[repr(C)]
#[derive(Debug)]
pub struct efcam_lc_cam {
    _private: [u8; 0],
}

/// Implements `Default` with `size` set and every other field zero.
macro_rules! sized_default {
    ($($t:ty),* $(,)?) => {$(
        impl Default for $t {
            fn default() -> Self {
                // SAFETY: plain-old-data with no invariants; all-zero is valid.
                let mut v: Self = unsafe { std::mem::zeroed() };
                v.size = std::mem::size_of::<Self>() as u32;
                v
            }
        }
    )*};
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct efcam_lc_camera_info {
    pub size: u32,
    pub id: [c_char; 256],
    pub model: [c_char; 64],
    pub location: i32,
    pub rotation: i32,
    pub num_devices: u32,
    pub devices: [u64; EFCAM_LC_MAX_DEVICES],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct efcam_lc_format {
    pub size: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub min_width: u32,
    pub min_height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub h_step: u32,
    pub v_step: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct efcam_lc_stream_request {
    pub size: u32,
    pub role: u32,
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub buffer_count: u32,
    pub orientation: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct efcam_lc_stream_info {
    pub size: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub frame_size: u32,
    pub buffer_count: u32,
    pub orientation: u32,
    pub adjusted: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct efcam_lc_plane {
    pub fd: i32,
    pub offset: u32,
    pub length: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct efcam_lc_buffer {
    pub size: u32,
    pub num_planes: u32,
    pub planes: [efcam_lc_plane; EFCAM_LC_MAX_PLANES],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct efcam_lc_control_info {
    pub size: u32,
    pub min: f64,
    pub max: f64,
    pub def: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct efcam_lc_controls {
    pub size: u32,
    pub frame_duration_min_us: i64,
    pub frame_duration_max_us: i64,
    pub exposure_mode: i32,
    pub exposure_time_us: i32,
    pub gain_mode: i32,
    pub analogue_gain: f32,
    pub awb_enable: i32,
    pub colour_temperature: i32,
    pub colour_gains: [f32; 2],
}

impl efcam_lc_controls {
    /// A set with every control unset.
    pub fn unset() -> Self {
        Self {
            exposure_mode: EFCAM_LC_MODE_UNSET,
            gain_mode: EFCAM_LC_MODE_UNSET,
            awb_enable: -1,
            ..Self::default()
        }
    }

    /// Whether no control is set.
    pub fn is_unset(&self) -> bool {
        *self == Self::unset()
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct efcam_lc_completion {
    pub size: u32,
    pub slot: u32,
    pub status: u32,
    pub sequence: u32,
    pub sensor_timestamp_ns: i64,
    pub buffer_timestamp_ns: u64,
    pub exposure_time_us: i64,
    pub analogue_gain: f64,
    pub frame_duration_us: i64,
    pub colour_temperature: i32,
    pub bytes_used: [u32; EFCAM_LC_MAX_PLANES],
}

sized_default!(
    efcam_lc_camera_info,
    efcam_lc_format,
    efcam_lc_stream_request,
    efcam_lc_stream_info,
    efcam_lc_buffer,
    efcam_lc_control_info,
    efcam_lc_controls,
    efcam_lc_completion,
);

/// The shim's entry points, resolved from a loaded library or supplied by
/// a mock.
#[derive(Debug, Clone, Copy)]
pub struct Functions {
    pub abi_version: unsafe extern "C" fn() -> u32,
    pub build_version: unsafe extern "C" fn() -> *const c_char,
    pub runtime_version: unsafe extern "C" fn() -> *const c_char,
    pub mgr_create: unsafe extern "C" fn(*mut *mut efcam_lc_mgr) -> c_int,
    pub mgr_destroy: unsafe extern "C" fn(*mut efcam_lc_mgr),
    pub mgr_camera_count: unsafe extern "C" fn(*mut efcam_lc_mgr) -> c_int,
    pub mgr_camera_info:
        unsafe extern "C" fn(*mut efcam_lc_mgr, u32, *mut efcam_lc_camera_info) -> c_int,
    pub mgr_formats: unsafe extern "C" fn(
        *mut efcam_lc_mgr,
        *const c_char,
        u32,
        *mut efcam_lc_format,
        u32,
    ) -> c_int,
    pub cam_open:
        unsafe extern "C" fn(*mut efcam_lc_mgr, *const c_char, *mut *mut efcam_lc_cam) -> c_int,
    pub cam_configure: unsafe extern "C" fn(
        *mut efcam_lc_cam,
        *const efcam_lc_stream_request,
        *mut efcam_lc_stream_info,
    ) -> c_int,
    pub cam_control_info:
        unsafe extern "C" fn(*mut efcam_lc_cam, u32, *mut efcam_lc_control_info) -> c_int,
    pub cam_alloc: unsafe extern "C" fn(*mut efcam_lc_cam) -> c_int,
    pub cam_buffer: unsafe extern "C" fn(*mut efcam_lc_cam, u32, *mut efcam_lc_buffer) -> c_int,
    pub cam_start: unsafe extern "C" fn(*mut efcam_lc_cam, *const efcam_lc_controls) -> c_int,
    pub cam_event_fd: unsafe extern "C" fn(*mut efcam_lc_cam) -> c_int,
    pub cam_dequeue: unsafe extern "C" fn(*mut efcam_lc_cam, *mut efcam_lc_completion) -> c_int,
    pub cam_requeue:
        unsafe extern "C" fn(*mut efcam_lc_cam, u32, *const efcam_lc_controls) -> c_int,
    pub cam_stop: unsafe extern "C" fn(*mut efcam_lc_cam) -> c_int,
    pub cam_free: unsafe extern "C" fn(*mut efcam_lc_cam) -> c_int,
    pub cam_close: unsafe extern "C" fn(*mut efcam_lc_cam),
}

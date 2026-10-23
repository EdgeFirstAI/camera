// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The process-wide libcamera `CameraManager`.
//!
//! libcamera allows one `CameraManager` per process. It starts with the
//! first camera or enumeration that needs it and stops when the last user
//! drops it, so a process that stops using libcamera holds no devices.

use std::ffi::CString;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use edgefirst_libcamera_sys::{self as sys, LoadError, LoadErrorKind, Shim};

use crate::{Error, ErrorKind, Result};

/// libcamera reads this when its `CameraManager` starts.
const MATCH_LIST_ENV: &str = "LIBCAMERA_PIPELINES_MATCH_LIST";

static SHIM: OnceLock<std::result::Result<Arc<Shim>, LoadError>> = OnceLock::new();
static RUNNING: Mutex<Weak<Manager>> = Mutex::new(Weak::new());
static MATCH_LIST: Mutex<Option<String>> = Mutex::new(None);

/// A started `CameraManager`.
#[derive(Debug)]
pub(crate) struct Manager {
    shim: Arc<Shim>,
    raw: NonNull<sys::efcam_lc_mgr>,
}

// SAFETY: libcamera's CameraManager is thread-safe for the calls the shim
// makes (camera lookup, enumeration, acquire); per-camera state lives in
// `efcam_lc_cam`, which one `LibcameraCamera` owns.
unsafe impl Send for Manager {}
// SAFETY: as above.
unsafe impl Sync for Manager {}

impl Manager {
    /// Starts a manager on `shim`.
    pub(crate) fn start(shim: Arc<Shim>) -> Result<Arc<Self>> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: a valid out pointer; the shim checks nothing else.
        let ret = unsafe { (shim.functions().mgr_create)(&mut raw) };
        if ret < 0 {
            return Err(errno_error(ret, "start the libcamera CameraManager"));
        }
        let raw = NonNull::new(raw).ok_or_else(|| {
            Error::new(ErrorKind::Backend, "the libcamera shim returned no manager")
        })?;
        Ok(Arc::new(Self { shim, raw }))
    }

    /// The running manager, starting one on the loaded shim if none runs.
    pub(crate) fn shared() -> Result<Arc<Self>> {
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = running.upgrade() {
            return Ok(m);
        }
        let shim = shim()?;
        if let Some(list) = MATCH_LIST
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            std::env::set_var(MATCH_LIST_ENV, list);
        }
        tracing::debug!(
            "libcamera {} through {} (built against {})",
            shim.runtime_version(),
            shim.origin(),
            shim.build_version()
        );
        let m = Self::start(shim)?;
        *running = Arc::downgrade(&m);
        Ok(m)
    }

    pub(crate) fn functions(&self) -> &sys::Functions {
        self.shim.functions()
    }

    pub(crate) fn raw(&self) -> *mut sys::efcam_lc_mgr {
        self.raw.as_ptr()
    }

    /// The ids of every camera, in libcamera's order.
    pub(crate) fn cameras(&self) -> Vec<sys::efcam_lc_camera_info> {
        let f = self.functions();
        // SAFETY: a live manager handle.
        let count = unsafe { (f.mgr_camera_count)(self.raw()) }.max(0) as u32;
        (0..count)
            .filter_map(|i| {
                let mut info = sys::efcam_lc_camera_info::default();
                // SAFETY: a live manager handle and a sized struct.
                (unsafe { (f.mgr_camera_info)(self.raw(), i, &mut info) } == 0).then_some(info)
            })
            .collect()
    }

    /// The video-role formats of camera `id`.
    pub(crate) fn formats(&self, id: &str) -> Result<Vec<sys::efcam_lc_format>> {
        let f = self.functions();
        let cid = c_id(id)?;
        let role = sys::EFCAM_LC_ROLE_VIDEO;
        // SAFETY: a live manager, a NUL-terminated id and no output array.
        let count =
            unsafe { (f.mgr_formats)(self.raw(), cid.as_ptr(), role, std::ptr::null_mut(), 0) };
        if count < 0 {
            return Err(errno_error(count, &format!("list the formats of {id}")));
        }
        let mut out = vec![sys::efcam_lc_format::default(); count as usize];
        if out.is_empty() {
            return Ok(out);
        }
        // SAFETY: `out` holds `count` sized elements.
        let got = unsafe {
            (f.mgr_formats)(
                self.raw(),
                cid.as_ptr(),
                role,
                out.as_mut_ptr(),
                count as u32,
            )
        };
        if got < 0 {
            return Err(errno_error(got, &format!("list the formats of {id}")));
        }
        out.truncate(got.min(count) as usize);
        Ok(out)
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        // SAFETY: the handle is live and every camera that used it has
        // dropped its `Arc`.
        unsafe { (self.functions().mgr_destroy)(self.raw()) };
    }
}

/// The shim, loaded once per process.
pub(crate) fn shim() -> Result<Arc<Shim>> {
    SHIM.get_or_init(|| Shim::load().map(Arc::new))
        .clone()
        .map_err(|e| {
            let kind = match e.kind() {
                LoadErrorKind::NoLibcamera | LoadErrorKind::NoShim => ErrorKind::NotFound,
                _ => ErrorKind::Backend,
            };
            Error::new(kind, format!("libcamera backend: {e}")).with_source(e)
        })
}

/// Sets the pipeline handlers libcamera may use, as
/// `LIBCAMERA_PIPELINES_MATCH_LIST` does.
pub(crate) fn set_match_list(list: Option<&str>) -> Result<()> {
    let running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    let mut current = MATCH_LIST.lock().unwrap_or_else(|e| e.into_inner());
    if running.strong_count() > 0 && current.as_deref() != list {
        return Err(Error::new(
            ErrorKind::InvalidConfig,
            "the libcamera pipeline match list applies when the CameraManager starts; \
             close every libcamera camera first",
        ));
    }
    *current = list.map(str::to_owned);
    Ok(())
}

pub(crate) fn c_id(id: &str) -> Result<CString> {
    CString::new(id).map_err(|_| {
        Error::new(
            ErrorKind::InvalidConfig,
            format!("libcamera id {id:?} contains a NUL byte"),
        )
    })
}

/// An error from a shim call returning `-errno`.
pub(crate) fn errno_error(ret: i32, what: &str) -> Error {
    let errno = -ret;
    let kind = match errno {
        libc::ENODEV | libc::ENOENT => ErrorKind::NotFound,
        libc::EBUSY => ErrorKind::Busy,
        libc::EINVAL => ErrorKind::InvalidConfig,
        _ => ErrorKind::Backend,
    };
    Error::new(kind, format!("cannot {what}")).with_source(std::io::Error::from_raw_os_error(errno))
}

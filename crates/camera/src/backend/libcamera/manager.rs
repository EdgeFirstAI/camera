// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The process-wide libcamera `CameraManager`.
//!
//! libcamera allows one `CameraManager` per process. It starts with the
//! first camera or enumeration that needs it and stops when the last user
//! drops it, so a process that stops using libcamera holds no devices.

use std::ffi::CString;
use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use edgefirst_libcamera_sys::{self as sys, LoadError, LoadErrorKind, Shim};

use crate::{Error, ErrorKind, Result};

/// libcamera reads this when its `CameraManager` starts.
const MATCH_LIST_ENV: &str = "LIBCAMERA_PIPELINES_MATCH_LIST";

static SHIM: OnceLock<std::result::Result<Arc<Shim>, LoadError>> = OnceLock::new();
static LIFECYCLE: Mutex<Lifecycle> = Mutex::new(Lifecycle {
    running: Weak::new(),
    live: false,
});
/// Signalled when a manager has been destroyed.
static DESTROYED: Condvar = Condvar::new();

/// The process's manager. `live` stays set from its start until its
/// `CameraManager` is destroyed, which is after `running` stops upgrading:
/// a new manager may start only once the old one is gone.
struct Lifecycle {
    running: Weak<Manager>,
    live: bool,
}

fn lifecycle() -> MutexGuard<'static, Lifecycle> {
    LIFECYCLE.lock().unwrap_or_else(PoisonError::into_inner)
}

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
    /// Waits for a manager that is being dropped to be destroyed first.
    pub(crate) fn shared() -> Result<Arc<Self>> {
        let mut state = lifecycle();
        loop {
            if let Some(m) = state.running.upgrade() {
                return Ok(m);
            }
            if !state.live {
                break;
            }
            state = DESTROYED
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        let shim = shim()?;
        tracing::debug!(
            "libcamera {} through {} (built against {})",
            shim.runtime_version(),
            shim.origin(),
            shim.build_version()
        );
        let m = Self::start(shim)?;
        state.running = Arc::downgrade(&m);
        state.live = true;
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
            return Err(lookup_error(count, &format!("list the formats of {id}")));
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
            return Err(lookup_error(got, &format!("list the formats of {id}")));
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
        let mut state = lifecycle();
        // A manager started through `start` alone, outside `shared`, is
        // not the process's manager.
        if state.running.strong_count() == 0 {
            state.live = false;
            DESTROYED.notify_all();
        }
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

/// Sets or removes `LIBCAMERA_PIPELINES_MATCH_LIST`, which libcamera reads
/// when its `CameraManager` starts.
///
/// # Safety
///
/// It changes the process environment, so no other thread may read or
/// write the environment during the call.
pub(crate) unsafe fn set_match_list(list: Option<&str>) -> Result<()> {
    let state = lifecycle();
    if state.live && std::env::var(MATCH_LIST_ENV).ok().as_deref() != list {
        return Err(Error::new(
            ErrorKind::InvalidConfig,
            "the libcamera pipeline match list applies when the CameraManager starts; \
             close every libcamera camera first",
        ));
    }
    match list {
        Some(list) => std::env::set_var(MATCH_LIST_ENV, list),
        None => std::env::remove_var(MATCH_LIST_ENV),
    }
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

/// An error from a shim call returning `-errno`, kinded like every other
/// backend's errno.
pub(crate) fn errno_error(ret: i32, what: &str) -> Error {
    let errno = -ret;
    let kind = crate::error::errno_kind(errno).unwrap_or(ErrorKind::Backend);
    Error::new(kind, format!("cannot {what}")).with_source(std::io::Error::from_raw_os_error(errno))
}

/// An error from a shim call that looks a camera up by id, where `ENODEV`
/// means no camera has that id rather than a camera that went away.
pub(crate) fn lookup_error(ret: i32, what: &str) -> Error {
    let err = errno_error(ret, what);
    if -ret == libc::ENODEV {
        Error::new(ErrorKind::NotFound, err.to_string()).with_source(err)
    } else {
        err
    }
}

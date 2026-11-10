// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! COM and Media Foundation start-up, attribute strings, and the mapping of
//! their errors to [`ErrorKind`].

use std::fmt;

use windows::core::{GUID, HRESULT, PWSTR};
use windows::Win32::Foundation::{
    ERROR_DEVICE_NOT_CONNECTED, ERROR_SHARING_VIOLATION, E_ACCESSDENIED,
};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, MFShutdown, MFStartup, MFSTARTUP_NOSOCKET, MF_E_INVALIDMEDIATYPE, MF_E_SHUTDOWN,
    MF_E_TOPO_CODEC_NOT_FOUND, MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED,
    MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

use crate::{Error, ErrorKind, Result};

/// Where Windows lets the user allow camera access.
pub(super) const GRANT_ACCESS: &str = "allow it in Settings > Privacy & security > Camera, \
     with \"Camera access\" and \"Let desktop apps access your camera\" on";

/// The calling thread's membership of a COM apartment.
struct Apartment(bool);

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful `CoInitializeEx` on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

thread_local! {
    static APARTMENT: Apartment = Apartment(
        // SAFETY: no reserved pointer. Success, including `S_FALSE` for a
        // thread already in the apartment, is balanced when the thread exits.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok(),
    );
}

/// Joins the calling thread to COM's multithreaded apartment, once per
/// thread. A thread already in a single-threaded apartment keeps it; the
/// Media Foundation objects the backend uses are free-threaded.
pub(super) fn com() {
    APARTMENT.with(|_| {});
}

/// A hold on the Media Foundation platform, which stays started while any
/// hold lives.
pub(super) struct Platform(());

impl Platform {
    pub(super) fn start() -> Result<Self> {
        com();
        // SAFETY: plain start-up call, balanced by `MFShutdown` on drop.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }
            .map_err(|e| error(e, "start Media Foundation"))?;
        Ok(Self(()))
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        com();
        // SAFETY: balances the `MFStartup` of this hold.
        let _ = unsafe { MFShutdown() };
    }
}

impl fmt::Debug for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Platform")
    }
}

/// A COM interface the backend shares between threads.
///
/// The Media Foundation source reader, media sources and their control
/// interfaces are free-threaded: they may be called from any thread in the
/// multithreaded apartment, which [`com`] joins.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Agile<T>(pub(super) T);

// SAFETY: see the type's documentation; only free-threaded interfaces are
// wrapped.
unsafe impl<T> Send for Agile<T> {}
// SAFETY: as above.
unsafe impl<T> Sync for Agile<T> {}

impl<T> std::ops::Deref for Agile<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Agile<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(std::any::type_name::<T>())
    }
}

/// The [`ErrorKind`] of a COM or Media Foundation failure.
pub(super) fn kind_of(code: HRESULT) -> ErrorKind {
    match code {
        E_ACCESSDENIED => ErrorKind::PermissionDenied,
        MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED => ErrorKind::Busy,
        c if c == ERROR_SHARING_VIOLATION.to_hresult() => ErrorKind::Busy,
        MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED | MF_E_SHUTDOWN => ErrorKind::Disconnected,
        c if c == ERROR_DEVICE_NOT_CONNECTED.to_hresult() => ErrorKind::Disconnected,
        MF_E_TOPO_CODEC_NOT_FOUND | MF_E_INVALIDMEDIATYPE => ErrorKind::UnsupportedFormat,
        _ => ErrorKind::Backend,
    }
}

/// An SDK error for a failed `what`.
pub(super) fn error(e: windows::core::Error, what: &str) -> Error {
    let kind = kind_of(e.code());
    let message = match kind {
        ErrorKind::PermissionDenied => {
            format!("{what}: Windows refused access to the camera; {GRANT_ACCESS}")
        }
        _ => format!("{what}: {} ({:#010x})", e.message(), e.code().0),
    };
    Error::new(kind, message).with_source(e)
}

/// A string attribute, or `None` when it is not set.
pub(super) fn string(attributes: &IMFAttributes, key: &GUID) -> Option<String> {
    let mut p = PWSTR::null();
    let mut len = 0;
    // SAFETY: valid out-pointers; the string is freed below.
    unsafe { attributes.GetAllocatedString(key, &mut p, &mut len) }.ok()?;
    // SAFETY: Media Foundation returned a NUL-terminated string.
    let s = unsafe { p.to_string() }.ok();
    // SAFETY: allocated by `GetAllocatedString` with the COM allocator.
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        error(e, "Media Foundation")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_access_is_a_permission_error_that_says_how_to_allow_it() {
        let e = error(E_ACCESSDENIED.into(), "open camera");
        assert_eq!(e.kind(), ErrorKind::PermissionDenied);
        assert!(e.to_string().contains("Privacy & security > Camera"), "{e}");
    }

    #[test]
    fn device_failures_have_their_own_kinds() {
        assert_eq!(
            kind_of(MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED),
            ErrorKind::Disconnected
        );
        assert_eq!(
            kind_of(ERROR_DEVICE_NOT_CONNECTED.to_hresult()),
            ErrorKind::Disconnected
        );
        assert_eq!(
            kind_of(ERROR_SHARING_VIOLATION.to_hresult()),
            ErrorKind::Busy
        );
        assert_eq!(
            kind_of(MF_E_TOPO_CODEC_NOT_FOUND),
            ErrorKind::UnsupportedFormat
        );
        assert_eq!(kind_of(HRESULT(0x8000_4005_u32 as i32)), ErrorKind::Backend);
    }
}

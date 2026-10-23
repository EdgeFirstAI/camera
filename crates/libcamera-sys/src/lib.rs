// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! # edgefirst-libcamera-sys
//!
//! Run-time bindings to the EdgeFirst libcamera shim, a small C library
//! (`shim/`) that wraps libcamera's C++ API in a stable C ABI. Nothing here
//! links libcamera: [`Shim::load`] finds the shim built for the installed
//! libcamera and loads it with `dlopen`, so a binary built once runs with or
//! without libcamera present.
//!
//! libcamera's SONAME pins a minor version (`libcamera.so.0.7`), so one shim
//! is built per supported minor, 0.5 and newer. [`Shim::load`] looks for one
//! in this order:
//!
//! 1. the path in `EDGEFIRST_CAMERA_LIBCAMERA_SHIM`, and nothing else;
//! 2. `libedgefirst_camera_libcamera.so.1` on the library search path, as a
//!    Yocto image or distribution package installs it;
//! 3. `libcamera-0.<minor>/libedgefirst_camera_libcamera.so.1`, for each
//!    installed libcamera minor (newest first), under
//!    `EDGEFIRST_CAMERA_LIBCAMERA_SHIM_DIR`, the executable's directory, its
//!    `../lib/edgefirst-camera`, `/usr/local/lib/edgefirst-camera` and
//!    `/usr/lib/edgefirst-camera`.
//!
//! A shim built for another minor fails to load (its `DT_NEEDED` names that
//! minor), so the first one that loads matches the installed libcamera.
//!
//! The `mock` feature provides `mock`, an in-process implementation of the
//! ABI for tests on machines without libcamera.

pub mod abi;
#[cfg(all(target_os = "linux", feature = "mock"))]
pub mod mock;

use std::ffi::{c_char, CStr};
use std::fmt;
use std::path::{Path, PathBuf};

pub use abi::*;

/// Environment variable naming the shim to load, bypassing the search.
pub const SHIM_ENV: &str = "EDGEFIRST_CAMERA_LIBCAMERA_SHIM";
/// Environment variable naming a directory of per-minor shims.
pub const SHIM_DIR_ENV: &str = "EDGEFIRST_CAMERA_LIBCAMERA_SHIM_DIR";
/// The shim's SONAME.
pub const SHIM_SONAME: &str = "libedgefirst_camera_libcamera.so.1";
/// The oldest libcamera minor a shim is built for.
pub const MIN_MINOR: u32 = 5;
/// The newest libcamera minor [`Shim::load`] looks for.
const MAX_MINOR: u32 = 30;

/// Why the shim could not be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LoadErrorKind {
    /// No libcamera library is installed.
    NoLibcamera,
    /// libcamera is installed, but no shim for its version was found.
    NoShim,
    /// A shim loaded but speaks another ABI version.
    Abi,
    /// The shim named by [`SHIM_ENV`] failed to load.
    Load,
}

/// Error from [`Shim::load`].
#[derive(Debug, Clone)]
pub struct LoadError {
    kind: LoadErrorKind,
    message: String,
}

impl LoadError {
    fn new(kind: LoadErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// What went wrong.
    pub fn kind(&self) -> LoadErrorKind {
        self.kind
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LoadError {}

/// A loaded shim and its entry points.
pub struct Shim {
    functions: Functions,
    origin: String,
    // Keeps the entry points valid; dropped last.
    _library: Option<libloading::Library>,
}

impl fmt::Debug for Shim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shim")
            .field("origin", &self.origin)
            .field("build_version", &self.build_version())
            .finish_non_exhaustive()
    }
}

impl Shim {
    /// Finds and loads the shim for the installed libcamera.
    ///
    /// # Errors
    ///
    /// [`LoadErrorKind::NoLibcamera`] when libcamera is not installed,
    /// [`LoadErrorKind::NoShim`] when no shim matches it,
    /// [`LoadErrorKind::Abi`] when the shim found speaks another ABI, and
    /// [`LoadErrorKind::Load`] when the shim named by [`SHIM_ENV`] fails to
    /// load.
    pub fn load() -> Result<Self, LoadError> {
        if let Some(path) = std::env::var_os(SHIM_ENV) {
            return Self::open(Path::new(&path))
                .map_err(|e| LoadError::new(e.kind, format!("{SHIM_ENV}: {e}")));
        }
        let minors = installed_minors();
        let Some(&newest) = minors.first() else {
            return Err(LoadError::new(
                LoadErrorKind::NoLibcamera,
                "libcamera is not installed (no libcamera.so.0.<minor> could be loaded)",
            ));
        };
        let supported: Vec<u32> = minors.iter().copied().filter(|&m| m >= MIN_MINOR).collect();
        if supported.is_empty() {
            return Err(LoadError::new(
                LoadErrorKind::NoShim,
                format!("libcamera 0.{newest} is installed; 0.{MIN_MINOR} or newer is required"),
            ));
        }
        let mut tried = Vec::new();
        for candidate in candidates(&supported, &search_dirs()) {
            match Self::open(&candidate) {
                Ok(shim) => return Ok(shim),
                Err(e) if e.kind == LoadErrorKind::Abi => return Err(e),
                Err(_) => tried.push(candidate.display().to_string()),
            }
        }
        let found: Vec<String> = supported.iter().map(|m| format!("0.{m}")).collect();
        Err(LoadError::new(
            LoadErrorKind::NoShim,
            format!(
                "no libcamera shim matches the installed libcamera {}; tried {}. Install \
                 the shim built for that version, or name it with {SHIM_ENV}",
                found.join(", "),
                tried.join(", ")
            ),
        ))
    }

    /// Loads the shim at `path` (a bare SONAME searches the library path).
    ///
    /// # Errors
    ///
    /// [`LoadErrorKind::Load`] when it does not load or lacks a symbol,
    /// [`LoadErrorKind::Abi`] when it speaks another ABI version.
    pub fn open(path: &Path) -> Result<Self, LoadError> {
        let origin = path.display().to_string();
        let load =
            |e: libloading::Error| LoadError::new(LoadErrorKind::Load, format!("{origin}: {e}"));
        // SAFETY: loading runs the shim's and libcamera's initialisers, which
        // have no preconditions on this process.
        let library = unsafe { libloading::Library::new(path) }.map_err(load)?;
        macro_rules! resolve {
            ($($field:ident),* $(,)?) => {
                Functions {$(
                    // SAFETY: the symbol's type is the one the header
                    // declares, mirrored in `Functions`.
                    $field: *unsafe {
                        library.get(concat!("efcam_lc_", stringify!($field), "\0").as_bytes())
                    }
                    .map_err(load)?,
                )*}
            };
        }
        let functions = resolve!(
            abi_version,
            build_version,
            runtime_version,
            mgr_create,
            mgr_destroy,
            mgr_camera_count,
            mgr_camera_info,
            mgr_formats,
            cam_open,
            cam_configure,
            cam_control_info,
            cam_alloc,
            cam_buffer,
            cam_start,
            cam_event_fd,
            cam_dequeue,
            cam_requeue,
            cam_stop,
            cam_free,
            cam_close,
        );
        let mut shim = Self::from_functions(functions, origin)?;
        shim._library = Some(library);
        Ok(shim)
    }

    /// Wraps entry points that are not in a loaded library, such as
    /// `mock::functions`.
    ///
    /// # Errors
    ///
    /// [`LoadErrorKind::Abi`] when they report another ABI version.
    pub fn from_functions(
        functions: Functions,
        origin: impl Into<String>,
    ) -> Result<Self, LoadError> {
        let origin = origin.into();
        // SAFETY: takes no arguments and has no preconditions.
        let abi = unsafe { (functions.abi_version)() };
        if abi != EFCAM_LC_ABI_VERSION {
            return Err(LoadError::new(
                LoadErrorKind::Abi,
                format!("{origin} speaks shim ABI {abi}; this build needs {EFCAM_LC_ABI_VERSION}"),
            ));
        }
        Ok(Self {
            functions,
            origin,
            _library: None,
        })
    }

    /// The entry points.
    pub fn functions(&self) -> &Functions {
        &self.functions
    }

    /// Where the shim was loaded from.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The libcamera version the shim was built against, e.g. `0.7.0`.
    pub fn build_version(&self) -> String {
        // SAFETY: returns a static NUL-terminated string.
        unsafe { string((self.functions.build_version)()) }
    }

    /// The loaded libcamera's own version string.
    pub fn runtime_version(&self) -> String {
        // SAFETY: returns a NUL-terminated string owned by libcamera.
        unsafe { string((self.functions.runtime_version)()) }
    }
}

/// Copies a C string, empty for null.
///
/// # Safety
///
/// `p` is null or a valid NUL-terminated string.
pub unsafe fn string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the caller guarantees a valid NUL-terminated string.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Reads a fixed-size, NUL-padded C string field.
pub fn field_str(field: &[c_char]) -> String {
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| u8::from_ne_bytes(c.to_ne_bytes()))
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The libcamera minors that load in this process, newest first.
fn installed_minors() -> Vec<u32> {
    (0..=MAX_MINOR)
        .rev()
        .filter(|m| {
            // SAFETY: loads and immediately unloads libcamera, whose
            // initialisers have no preconditions on this process.
            unsafe { libloading::Library::new(format!("libcamera.so.0.{m}")) }.is_ok()
        })
        .collect()
}

/// Directories holding per-minor shims, in search order.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os(SHIM_DIR_ENV) {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        dirs.push(exe_dir.join("../lib/edgefirst-camera"));
        dirs.push(exe_dir);
    }
    dirs.push(PathBuf::from("/usr/local/lib/edgefirst-camera"));
    dirs.push(PathBuf::from("/usr/lib/edgefirst-camera"));
    dirs
}

/// Shim paths to try for `minors` (newest first) in `dirs`.
fn candidates(minors: &[u32], dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from(SHIM_SONAME)];
    for m in minors {
        for dir in dirs {
            let path = dir.join(format!("libcamera-0.{m}")).join(SHIM_SONAME);
            if path.exists() {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_try_the_soname_then_each_minor_newest_first() {
        let root = std::env::temp_dir().join(format!("efcam-lc-candidates-{}", std::process::id()));
        for m in [5, 7] {
            let dir = root.join(format!("libcamera-0.{m}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(SHIM_SONAME), b"").unwrap();
        }
        let got = candidates(&[7, 6, 5], std::slice::from_ref(&root));
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            got,
            [
                PathBuf::from(SHIM_SONAME),
                root.join("libcamera-0.7").join(SHIM_SONAME),
                root.join("libcamera-0.5").join(SHIM_SONAME),
            ]
        );
    }

    #[test]
    fn a_missing_shim_is_a_load_error() {
        let e =
            Shim::open(Path::new("/nonexistent/libedgefirst_camera_libcamera.so.1")).unwrap_err();
        assert_eq!(e.kind(), LoadErrorKind::Load);
    }

    #[test]
    fn field_str_stops_at_the_first_nul() {
        let mut f: [c_char; 8] = [0; 8];
        for (d, s) in f.iter_mut().zip(b"imx708") {
            *d = c_char::from_ne_bytes([*s]);
        }
        assert_eq!(field_str(&f), "imx708");
    }

    #[test]
    fn unset_controls_are_unset() {
        let c = efcam_lc_controls::unset();
        assert!(c.is_unset());
        assert_eq!(c.size as usize, std::mem::size_of::<efcam_lc_controls>());
        let set = efcam_lc_controls { awb_enable: 1, ..c };
        assert!(!set.is_unset());
    }
}

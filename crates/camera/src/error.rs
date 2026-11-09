// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Error type with a stable kind code for every language binding.

use std::fmt;

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Stable error category, for callers and language bindings to act on
/// without parsing messages.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The source, device or backend does not exist or is not compiled in.
    NotFound,
    /// Another user holds the device.
    Busy,
    /// The operating system refused access to the camera, as macOS and
    /// Windows do until the user allows camera access in their privacy
    /// settings. The message says how to grant it. Not retryable until the
    /// user acts.
    PermissionDenied,
    /// The device exists but cannot deliver frames yet, for example while
    /// an ISP daemon restarts. The retry policy belongs to the caller.
    NotReady,
    /// A blocking call was interrupted by a signal.
    Interrupted,
    /// No frame arrived within the requested timeout.
    Timeout,
    /// The device went away, for example on USB unplug.
    Disconnected,
    /// A file source played to its end and is not looping.
    EndOfStream,
    /// The requested pixel format is not available from this camera.
    UnsupportedFormat,
    /// The request or the call order is invalid.
    InvalidConfig,
    /// Caller-provided buffers were refused; see [`Error::rejection`].
    BuffersRejected,
    /// Physically contiguous memory could not be allocated.
    ContiguousUnavailable,
    /// An operating-system I/O error not covered by a more specific kind.
    Io,
    /// The tensor crate reported an error.
    Tensor,
    /// A backend-specific failure.
    Backend,
}

impl ErrorKind {
    /// Short lowercase name, stable across releases.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Busy => "busy",
            Self::PermissionDenied => "permission_denied",
            Self::NotReady => "not_ready",
            Self::Interrupted => "interrupted",
            Self::Timeout => "timeout",
            Self::Disconnected => "disconnected",
            Self::EndOfStream => "end_of_stream",
            Self::UnsupportedFormat => "unsupported_format",
            Self::InvalidConfig => "invalid_config",
            Self::BuffersRejected => "buffers_rejected",
            Self::ContiguousUnavailable => "contiguous_unavailable",
            Self::Io => "io",
            Self::Tensor => "tensor",
            Self::Backend => "backend",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why caller-provided buffers were refused.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rejection {
    /// A tensor does not expose the backend's native handle, for example a
    /// GL PBO or host-memory tensor where a DMA-BUF is required.
    NotNativeHandle,
    /// Too few or too many buffers.
    Count,
    /// A tensor's pixel format differs from the negotiated format.
    Format,
    /// A tensor's dimensions differ from the negotiated size.
    Size,
    /// A tensor's row pitch differs from the pitch the driver returned.
    Pitch,
    /// A tensor's memory is known to be non-contiguous while contiguity is
    /// required.
    NotContiguous,
    /// The driver refused the buffer with this errno.
    Driver {
        /// The errno the driver returned.
        errno: i32,
    },
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotNativeHandle => f.write_str("not a native buffer handle"),
            Self::Count => f.write_str("buffer count out of range"),
            Self::Format => f.write_str("pixel format mismatch"),
            Self::Size => f.write_str("size mismatch"),
            Self::Pitch => f.write_str("row pitch mismatch"),
            Self::NotContiguous => f.write_str("memory not physically contiguous"),
            Self::Driver { errno } => write!(f, "driver refused the buffer (errno {errno})"),
        }
    }
}

/// Error returned by every fallible call in the crate.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    rejection: Option<(Rejection, Option<usize>)>,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl Error {
    /// Creates an error of `kind` with a human-readable message.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            rejection: None,
            source: None,
        }
    }

    /// Creates a [`ErrorKind::BuffersRejected`] error for caller-provided
    /// buffers, naming the reason and, where known, the slot.
    pub fn buffers_rejected(reason: Rejection, slot: Option<usize>) -> Self {
        let message = match slot {
            Some(slot) => format!("caller-provided buffer {slot} rejected: {reason}"),
            None => format!("caller-provided buffers rejected: {reason}"),
        };
        Self {
            kind: ErrorKind::BuffersRejected,
            message,
            rejection: Some((reason, slot)),
            source: None,
        }
    }

    /// Attaches the underlying cause.
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// The stable error category.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The reason and slot when the kind is [`ErrorKind::BuffersRejected`].
    pub fn rejection(&self) -> Option<(Rejection, Option<usize>)> {
        self.rejection
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

/// The kind of an errno, for every backend, or `None` for an errno with no
/// more specific kind than the caller's own. Backends override it only
/// where a call gives an errno a documented meaning of its own, such as
/// vvcam's `ENOENT` while its daemon restarts.
#[cfg(unix)]
pub(crate) fn errno_kind(errno: i32) -> Option<ErrorKind> {
    Some(match errno {
        libc::ENOENT => ErrorKind::NotFound,
        libc::ENODEV | libc::ENXIO => ErrorKind::Disconnected,
        libc::EAGAIN => ErrorKind::NotReady,
        libc::EBUSY => ErrorKind::Busy,
        libc::EINTR => ErrorKind::Interrupted,
        libc::ETIMEDOUT => ErrorKind::Timeout,
        libc::EINVAL => ErrorKind::InvalidConfig,
        _ => return None,
    })
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        let by_kind = match err.kind() {
            std::io::ErrorKind::NotFound => ErrorKind::NotFound,
            std::io::ErrorKind::WouldBlock => ErrorKind::NotReady,
            std::io::ErrorKind::Interrupted => ErrorKind::Interrupted,
            std::io::ErrorKind::TimedOut => ErrorKind::Timeout,
            _ => ErrorKind::Io,
        };
        #[cfg(unix)]
        let kind = err.raw_os_error().and_then(errno_kind).unwrap_or(by_kind);
        #[cfg(not(unix))]
        let kind = by_kind;
        Self::new(kind, err.to_string()).with_source(err)
    }
}

impl From<edgefirst_tensor::Error> for Error {
    fn from(err: edgefirst_tensor::Error) -> Self {
        Self::new(ErrorKind::Tensor, err.to_string()).with_source(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejection_carries_reason_and_slot() {
        let err = Error::buffers_rejected(Rejection::Pitch, Some(2));
        assert_eq!(err.kind(), ErrorKind::BuffersRejected);
        assert_eq!(err.rejection(), Some((Rejection::Pitch, Some(2))));
        assert!(err.to_string().contains("buffer 2"));
    }

    #[test]
    fn io_errors_map_to_kinds() {
        let not_found: Error = std::io::Error::from(std::io::ErrorKind::NotFound).into();
        assert_eq!(not_found.kind(), ErrorKind::NotFound);
        let interrupted: Error = std::io::Error::from(std::io::ErrorKind::Interrupted).into();
        assert_eq!(interrupted.kind(), ErrorKind::Interrupted);
    }

    #[cfg(unix)]
    #[test]
    fn errnos_map_to_one_kind_everywhere() {
        for (errno, kind) in [
            (libc::ENODEV, ErrorKind::Disconnected),
            (libc::EAGAIN, ErrorKind::NotReady),
            (libc::EBUSY, ErrorKind::Busy),
            (libc::ENOENT, ErrorKind::NotFound),
        ] {
            let err: Error = std::io::Error::from_raw_os_error(errno).into();
            assert_eq!(err.kind(), kind, "errno {errno}");
            assert_eq!(errno_kind(errno), Some(kind), "errno {errno}");
        }
        let eio: Error = std::io::Error::from_raw_os_error(libc::EIO).into();
        assert_eq!(eio.kind(), ErrorKind::Io);
    }

    #[test]
    fn permission_denied_has_a_stable_name() {
        assert_eq!(ErrorKind::PermissionDenied.as_str(), "permission_denied");
        let err = Error::new(ErrorKind::PermissionDenied, "camera access is off");
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
    }

    #[test]
    fn source_is_preserved() {
        let err: Error = std::io::Error::other("boom").into();
        assert!(std::error::Error::source(&err).is_some());
    }
}

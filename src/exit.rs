// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Exit statuses, the same table as the videostream CLI's.

use std::{error::Error, fmt, process::ExitCode};

use edgefirst_camera::ErrorKind;
use edgefirst_codec::CodecError;

/// Why the program stopped, as its exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Status {
    /// A failure with no more specific status.
    General = 1,
    /// Invalid arguments or configuration. clap exits with 2 too.
    InvalidArgs = 2,
    /// The camera is missing, held by another process, or went away.
    CameraNotFound = 3,
    /// No H.264 encoder or decoder.
    CodecUnavailable = 4,
    /// The Zenoh session could not be opened.
    Socket = 5,
    /// The camera delivered no frames in time.
    Timeout = 6,
}

impl From<Status> for ExitCode {
    fn from(status: Status) -> Self {
        ExitCode::from(status as u8)
    }
}

/// An error that carries its exit status.
#[derive(Debug)]
pub(crate) struct Failure {
    status: Status,
    message: String,
}

impl Failure {
    pub(crate) fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// `message`, with the status of the error it describes.
    pub(crate) fn of(e: &(dyn Error + 'static), message: impl Into<String>) -> Self {
        Self::new(status(e), message)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for Failure {}

/// The exit status for `e`. A missing codec anywhere in its source chain
/// wins over the camera error that reports it (the file source's decoder).
pub(crate) fn status(e: &(dyn Error + 'static)) -> Status {
    if let Some(f) = e.downcast_ref::<Failure>() {
        return f.status;
    }
    let mut next = Some(e);
    while let Some(err) = next {
        if let Some(CodecError::NoDevice(_)) = err.downcast_ref::<CodecError>() {
            return Status::CodecUnavailable;
        }
        next = err.source();
    }
    if let Some(e) = e.downcast_ref::<edgefirst_camera::Error>() {
        return camera_status(e.kind());
    }
    Status::General
}

/// `e` with `hint` appended when it is a missing encoder or decoder.
pub(crate) fn hint_codec(e: Box<dyn Error>, hint: &str) -> Box<dyn Error> {
    if status(&*e) == Status::CodecUnavailable {
        Box::new(Failure::new(
            Status::CodecUnavailable,
            format!("{e}; {hint}"),
        ))
    } else {
        e
    }
}

fn camera_status(kind: ErrorKind) -> Status {
    match kind {
        ErrorKind::NotFound | ErrorKind::Busy | ErrorKind::Disconnected => Status::CameraNotFound,
        ErrorKind::NotReady | ErrorKind::Timeout => Status::Timeout,
        ErrorKind::InvalidConfig => Status::InvalidArgs,
        _ => Status::General,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(e: impl Error + 'static) -> Box<dyn Error> {
        Box::new(e)
    }

    #[test]
    fn statuses_match_the_videostream_table() {
        let codes = [
            (Status::General, 1),
            (Status::InvalidArgs, 2),
            (Status::CameraNotFound, 3),
            (Status::CodecUnavailable, 4),
            (Status::Socket, 5),
            (Status::Timeout, 6),
        ];
        for (status, code) in codes {
            assert_eq!(status as u8, code);
        }
    }

    #[test]
    fn errors_map_to_their_status() {
        let camera = |kind| boxed(edgefirst_camera::Error::new(kind, "x"));
        assert_eq!(
            status(&*camera(ErrorKind::NotFound)),
            Status::CameraNotFound
        );
        assert_eq!(status(&*camera(ErrorKind::Busy)), Status::CameraNotFound);
        assert_eq!(status(&*camera(ErrorKind::NotReady)), Status::Timeout);
        assert_eq!(status(&*camera(ErrorKind::Timeout)), Status::Timeout);
        assert_eq!(
            status(&*camera(ErrorKind::InvalidConfig)),
            Status::InvalidArgs
        );
        assert_eq!(status(&*camera(ErrorKind::Io)), Status::General);
        let codec = boxed(CodecError::NoDevice("none".into()));
        assert_eq!(status(&*codec), Status::CodecUnavailable);
        let wrapped = boxed(Failure::of(&*codec, "h264: none"));
        assert_eq!(status(&*wrapped), Status::CodecUnavailable);
        let decoder = boxed(
            edgefirst_camera::Error::new(ErrorKind::NotFound, "H.264 decoder")
                .with_source(CodecError::NoDevice("none".into())),
        );
        assert_eq!(status(&*decoder), Status::CodecUnavailable);
        assert_eq!(status(&*Box::<dyn Error>::from("other")), Status::General);
    }

    #[test]
    fn only_a_missing_codec_gets_the_hint() {
        let codec = hint_codec(boxed(CodecError::NoDevice("none".into())), "do this");
        assert!(codec.to_string().ends_with("; do this"), "{codec}");
        assert_eq!(status(&*codec), Status::CodecUnavailable);
        let other = hint_codec("other".into(), "do this");
        assert_eq!(other.to_string(), "other");
    }
}

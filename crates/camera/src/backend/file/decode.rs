// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The decoder seam: what the file source needs from a video decoder, and
//! its implementation over `edgefirst_codec::video::VideoDecoder`.

use edgefirst_codec::video::{
    DecodeEvent, DecodedFrame, DecoderConfig, OutputMemory, OutputPool, VideoDecoder,
};
use edgefirst_codec::CodecError;
use edgefirst_tensor::{Colorimetry, PixelFormat, TensorDyn};

use crate::frame::TensorHolder;
use crate::{Error, ErrorKind, ResolvedMemory, Result};

/// A decoded picture. Dropping it gives its buffer back to the decoder.
pub(crate) struct Picture {
    pub(crate) holder: Box<dyn TensorHolder>,
    /// The decoder reported an error decoding it.
    pub(crate) corrupted: bool,
}

/// The decoded stream's geometry, once the decoder knows it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Format {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) format: PixelFormat,
    pub(crate) colorimetry: Option<Colorimetry>,
    pub(crate) buffers: usize,
    pub(crate) memory: ResolvedMemory,
}

pub(crate) enum Event {
    Picture(Picture),
    Format(Format),
}

#[derive(Debug)]
pub(crate) enum DecodeError {
    /// Every output buffer is held by a frame; retry once one drops.
    PoolExhausted,
    /// This access unit could not be decoded; the stream may continue.
    Skip(String),
    /// The decoder cannot continue.
    Fatal(Error),
}

pub(crate) trait Decode: Send + std::fmt::Debug {
    /// Decodes one access unit.
    fn decode(&mut self, unit: &[u8], pts: u64) -> std::result::Result<Vec<Event>, DecodeError>;
    /// Drains the pictures held back; the next unit starts a new sequence.
    fn flush(&mut self) -> std::result::Result<Vec<Event>, DecodeError>;
}

impl TensorHolder for DecodedFrame {
    fn tensor(&self) -> &TensorDyn {
        DecodedFrame::tensor(self)
    }
}

/// The platform's hardware decoder.
#[derive(Debug)]
pub(crate) struct HalDecoder(VideoDecoder);

impl HalDecoder {
    /// Opens a decoder whose pool lets the caller hold `held` frames.
    pub(crate) fn open(held: usize) -> Result<Self> {
        let config = DecoderConfig::h264().with_output(OutputPool::Allocate { count: held });
        VideoDecoder::new(config).map(Self).map_err(|e| {
            let kind = match e {
                CodecError::NoDevice(_) => ErrorKind::NotFound,
                _ => ErrorKind::Backend,
            };
            Error::new(kind, format!("H.264 decoder: {e}")).with_source(e)
        })
    }

    fn events(events: Vec<DecodeEvent>) -> Vec<Event> {
        events
            .into_iter()
            .map(|event| match event {
                DecodeEvent::Frame(frame) => Event::Picture(Picture {
                    corrupted: frame.corrupted(),
                    holder: Box::new(frame),
                }),
                DecodeEvent::Reconfigured(info) => Event::Format(Format {
                    width: info.width as u32,
                    height: info.height as u32,
                    format: info.format,
                    colorimetry: info.colorimetry,
                    buffers: info.buffers,
                    memory: match info.memory {
                        OutputMemory::Imported => ResolvedMemory::Import,
                        OutputMemory::Exported => ResolvedMemory::Export,
                    },
                }),
            })
            .collect()
    }
}

fn classify(e: CodecError) -> DecodeError {
    match e {
        CodecError::PoolExhausted { .. } => DecodeError::PoolExhausted,
        CodecError::InvalidInput(_)
        | CodecError::InvalidData(_)
        | CodecError::Timeout(_)
        | CodecError::Device { .. } => DecodeError::Skip(e.to_string()),
        other => DecodeError::Fatal(
            Error::new(ErrorKind::Backend, format!("H.264 decoder: {other}")).with_source(other),
        ),
    }
}

impl Decode for HalDecoder {
    fn decode(&mut self, unit: &[u8], pts: u64) -> std::result::Result<Vec<Event>, DecodeError> {
        self.0.decode(unit, pts).map(Self::events).map_err(classify)
    }

    fn flush(&mut self) -> std::result::Result<Vec<Event>, DecodeError> {
        self.0.flush().map(Self::events).map_err(classify)
    }
}

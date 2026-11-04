// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! V4L2 fourcc codes and the tensor pixel formats they capture into.

use edgefirst_tensor::PixelFormat;
use edgefirst_v4l2::uapi;

/// One capturable V4L2 format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mapping {
    pub(crate) fourcc: u32,
    pub(crate) format: PixelFormat,
    /// Memory planes: 2 for the `M` variants, whose chroma is a separate
    /// buffer.
    pub(crate) mem_planes: usize,
}

/// The V4L2 formats whose chroma is a buffer of its own. Every other
/// format is the tensor crate's [`PixelFormat::to_fourcc`] code.
const MULTI_BUFFER: [Mapping; 2] = [
    Mapping {
        fourcc: uapi::V4L2_PIX_FMT_NV12M,
        format: PixelFormat::Nv12,
        mem_planes: 2,
    },
    Mapping {
        fourcc: uapi::V4L2_PIX_FMT_NV16M,
        format: PixelFormat::Nv16,
        mem_planes: 2,
    },
];

/// The single-buffer mapping of `format`, when it has a V4L2 code.
fn single(format: PixelFormat) -> Option<Mapping> {
    let fourcc = format.to_fourcc();
    (fourcc != 0).then_some(Mapping {
        fourcc,
        format,
        mem_planes: 1,
    })
}

/// The mapping for a V4L2 fourcc, when the backend can capture it.
pub(crate) fn by_fourcc(fourcc: u32) -> Option<Mapping> {
    PixelFormat::from_fourcc(fourcc)
        .and_then(single)
        .or_else(|| MULTI_BUFFER.into_iter().find(|m| m.fourcc == fourcc))
}

/// The preferred fourcc for `format` among those the device `offers`:
/// single-buffer ahead of the multi-buffer variant, which converters that
/// take one DMA-BUF per image cannot use and which is only usable on a
/// multi-planar queue.
pub(crate) fn choose(format: PixelFormat, offers: &[u32], multiplanar: bool) -> Option<Mapping> {
    single(format)
        .filter(|m| offers.contains(&m.fourcc))
        .or_else(|| {
            MULTI_BUFFER
                .into_iter()
                .find(|m| m.format == format && multiplanar && offers.contains(&m.fourcc))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_single_buffer_nv12() {
        let offers = [uapi::V4L2_PIX_FMT_NV12M, uapi::V4L2_PIX_FMT_NV12];
        let m = choose(PixelFormat::Nv12, &offers, true).unwrap();
        assert_eq!((m.fourcc, m.mem_planes), (uapi::V4L2_PIX_FMT_NV12, 1));
    }

    #[test]
    fn nv12m_only_on_multiplanar_queues() {
        let offers = [uapi::V4L2_PIX_FMT_NV12M];
        assert_eq!(choose(PixelFormat::Nv12, &offers, false), None);
        assert_eq!(
            choose(PixelFormat::Nv12, &offers, true).unwrap().mem_planes,
            2
        );
    }

    #[test]
    fn unknown_fourccs_are_not_capturable() {
        assert_eq!(by_fourcc(uapi::V4L2_PIX_FMT_MJPEG), None);
        assert_eq!(
            by_fourcc(uapi::V4L2_PIX_FMT_YUYV).unwrap().format,
            PixelFormat::Yuyv
        );
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! V4L2 fourcc codes and the tensor pixel formats they capture into.

use edgefirst_tensor::PixelFormat;
use edgefirst_v4l2::uapi::{self, fourcc};

/// `V4L2_PIX_FMT_RGBA32`: bytes R, G, B, A.
const RGBA32: u32 = fourcc(b'A', b'B', b'2', b'4');
/// `V4L2_PIX_FMT_NV16M`: NV16 with the chroma plane in its own buffer.
const NV16M: u32 = fourcc(b'N', b'M', b'1', b'6');

/// One capturable V4L2 format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mapping {
    pub(crate) fourcc: u32,
    pub(crate) format: PixelFormat,
    /// Memory planes: 2 for the `M` variants, whose chroma is a separate
    /// buffer.
    pub(crate) mem_planes: usize,
}

/// Every V4L2 format the backend captures, in order of preference for one
/// pixel format: single-buffer NV12 ahead of `NV12M` (§3.3; HAL's Adreno
/// path declines a chroma plane in a separate DMA-BUF).
const TABLE: &[Mapping] = &[
    Mapping {
        fourcc: uapi::V4L2_PIX_FMT_YUYV,
        format: PixelFormat::Yuyv,
        mem_planes: 1,
    },
    Mapping {
        fourcc: uapi::V4L2_PIX_FMT_VYUY,
        format: PixelFormat::Vyuy,
        mem_planes: 1,
    },
    Mapping {
        fourcc: uapi::V4L2_PIX_FMT_NV12,
        format: PixelFormat::Nv12,
        mem_planes: 1,
    },
    Mapping {
        fourcc: uapi::V4L2_PIX_FMT_NV12M,
        format: PixelFormat::Nv12,
        mem_planes: 2,
    },
    Mapping {
        fourcc: uapi::V4L2_PIX_FMT_NV16,
        format: PixelFormat::Nv16,
        mem_planes: 1,
    },
    Mapping {
        fourcc: NV16M,
        format: PixelFormat::Nv16,
        mem_planes: 2,
    },
    Mapping {
        fourcc: uapi::V4L2_PIX_FMT_NV24,
        format: PixelFormat::Nv24,
        mem_planes: 1,
    },
    Mapping {
        fourcc: uapi::V4L2_PIX_FMT_GREY,
        format: PixelFormat::Grey,
        mem_planes: 1,
    },
    Mapping {
        fourcc: uapi::V4L2_PIX_FMT_RGB24,
        format: PixelFormat::Rgb,
        mem_planes: 1,
    },
    Mapping {
        fourcc: RGBA32,
        format: PixelFormat::Rgba,
        mem_planes: 1,
    },
    Mapping {
        fourcc: uapi::V4L2_PIX_FMT_ABGR32,
        format: PixelFormat::Bgra,
        mem_planes: 1,
    },
];

/// The mapping for a V4L2 fourcc, when the backend can capture it.
pub(crate) fn by_fourcc(fourcc: u32) -> Option<Mapping> {
    TABLE.iter().copied().find(|m| m.fourcc == fourcc)
}

/// The preferred fourcc for `format` among those the device `offers`.
/// Multi-buffer variants are only usable on a multi-planar queue.
pub(crate) fn choose(format: PixelFormat, offers: &[u32], multiplanar: bool) -> Option<Mapping> {
    TABLE.iter().copied().find(|m| {
        m.format == format && (m.mem_planes == 1 || multiplanar) && offers.contains(&m.fourcc)
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

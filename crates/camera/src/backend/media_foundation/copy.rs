// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Copying a frame out of a locked Media Foundation buffer, repacked into
//! the format it is delivered in without colour conversion.

use super::select::Packing;

/// A locked image: its bytes, where its top row starts, and the signed
/// distance from one row to the next. The pitch is negative for a bottom-up
/// image, as Media Foundation lays out RGB in system memory. Planes follow
/// the first at the same pitch (NV12), or at half of it (I420, YV12).
#[derive(Debug, Clone, Copy)]
pub(super) struct Rows<'a> {
    pub(super) bytes: &'a [u8],
    pub(super) top: usize,
    pub(super) pitch: isize,
}

impl<'a> Rows<'a> {
    /// `len` bytes of row `r` of the plane starting at `start`, `pitch`
    /// apart.
    fn row(&self, start: isize, pitch: isize, r: usize, len: usize) -> Result<&'a [u8], String> {
        let at = start + r as isize * pitch;
        usize::try_from(at)
            .ok()
            .and_then(|at| self.bytes.get(at..at + len))
            .ok_or_else(|| "a source row lies outside the buffer".to_owned())
    }
}

fn row_mut(dst: &mut [u8], stride: usize, r: usize, len: usize) -> Result<&mut [u8], String> {
    if len > stride {
        return Err(format!(
            "a row of {len} bytes is longer than the destination stride {stride}"
        ));
    }
    dst.get_mut(r * stride..r * stride + len)
        .ok_or_else(|| "a destination row lies outside the tensor".to_owned())
}

/// Copies a `width` x `height` image in `packing` from `src` into `dst`,
/// whose rows are `stride` bytes apart, as the format the packing delivers:
/// NV12 with its chroma rows after the luma rows, YUYV, or BGRA.
pub(super) fn copy(
    packing: Packing,
    width: usize,
    height: usize,
    src: Rows<'_>,
    dst: &mut [u8],
    stride: usize,
) -> Result<(), String> {
    let (top, pitch) = (src.top as isize, src.pitch);
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    match packing {
        Packing::Nv12 => {
            for r in 0..height {
                row_mut(dst, stride, r, width)?.copy_from_slice(src.row(top, pitch, r, width)?);
            }
            let chroma = top + pitch * height as isize;
            for r in 0..ch {
                row_mut(dst, stride, height + r, 2 * cw)?.copy_from_slice(src.row(
                    chroma,
                    pitch,
                    r,
                    2 * cw,
                )?);
            }
        }
        Packing::I420 | Packing::Yv12 => {
            for r in 0..height {
                row_mut(dst, stride, r, width)?.copy_from_slice(src.row(top, pitch, r, width)?);
            }
            let half = pitch / 2;
            let first = top + pitch * height as isize;
            let second = first + half * ch as isize;
            let (u, v) = if packing == Packing::I420 {
                (first, second)
            } else {
                (second, first)
            };
            for r in 0..ch {
                let (us, vs) = (src.row(u, half, r, cw)?, src.row(v, half, r, cw)?);
                let out = row_mut(dst, stride, height + r, 2 * cw)?;
                for (x, pair) in out.chunks_exact_mut(2).enumerate() {
                    pair[0] = us[x];
                    pair[1] = vs[x];
                }
            }
        }
        Packing::Yuy2 | Packing::Uyvy => {
            let len = 4 * cw;
            for r in 0..height {
                let from = src.row(top, pitch, r, len)?;
                let out = row_mut(dst, stride, r, len)?;
                if packing == Packing::Yuy2 {
                    out.copy_from_slice(from);
                } else {
                    for (o, i) in out.chunks_exact_mut(2).zip(from.chunks_exact(2)) {
                        o[0] = i[1];
                        o[1] = i[0];
                    }
                }
            }
        }
        Packing::Argb32 | Packing::Rgb32 => {
            let len = 4 * width;
            for r in 0..height {
                let out = row_mut(dst, stride, r, len)?;
                out.copy_from_slice(src.row(top, pitch, r, len)?);
                if packing == Packing::Rgb32 {
                    out.iter_mut().skip(3).step_by(4).for_each(|a| *a = 0xff);
                }
            }
        }
        Packing::Rgb24 => {
            for r in 0..height {
                let from = src.row(top, pitch, r, 3 * width)?;
                let out = row_mut(dst, stride, r, 4 * width)?;
                for (o, i) in out.chunks_exact_mut(4).zip(from.chunks_exact(3)) {
                    o[..3].copy_from_slice(i);
                    o[3] = 0xff;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(bytes: &[u8], top: usize, pitch: isize) -> Rows<'_> {
        Rows { bytes, top, pitch }
    }

    #[test]
    fn nv12_planes_move_to_the_destination_stride() {
        // 4x2 NV12 at a pitch of 6: two luma rows, one chroma row.
        let src: Vec<u8> = (0..18).collect();
        let mut dst = vec![0xff; 24];
        copy(Packing::Nv12, 4, 2, rows(&src, 0, 6), &mut dst, 8).unwrap();
        assert_eq!(&dst[0..4], &[0, 1, 2, 3]);
        assert_eq!(&dst[8..12], &[6, 7, 8, 9]);
        assert_eq!(&dst[16..20], &[12, 13, 14, 15]);
        assert_eq!(dst[4], 0xff, "padding is left alone");
    }

    #[test]
    fn i420_and_yv12_interleave_their_chroma_planes() {
        // 4x2: luma at a pitch of 4, then U and V planes at a pitch of 2.
        let mut src: Vec<u8> = (0..8).collect();
        src.extend([20, 21, 30, 31]);
        let mut dst = [0; 12];
        copy(Packing::I420, 4, 2, rows(&src, 0, 4), &mut dst, 4).unwrap();
        assert_eq!(dst, [0, 1, 2, 3, 4, 5, 6, 7, 20, 30, 21, 31]);
        copy(Packing::Yv12, 4, 2, rows(&src, 0, 4), &mut dst, 4).unwrap();
        assert_eq!(&dst[8..], &[30, 20, 31, 21]);
    }

    #[test]
    fn uyvy_is_reordered_to_yuyv() {
        let src = [1, 2, 3, 4];
        let mut dst = [0; 4];
        copy(Packing::Uyvy, 2, 1, rows(&src, 0, 4), &mut dst, 4).unwrap();
        assert_eq!(dst, [2, 1, 4, 3]);
    }

    #[test]
    fn a_bottom_up_rgb24_image_becomes_top_down_bgra() {
        // Two rows of one pixel, padded to 4 bytes, stored bottom row first.
        let src = [10, 11, 12, 0, 1, 2, 3, 0];
        let mut dst = [0; 8];
        copy(Packing::Rgb24, 1, 2, rows(&src, 4, -4), &mut dst, 4).unwrap();
        assert_eq!(dst, [1, 2, 3, 0xff, 10, 11, 12, 0xff]);
    }

    #[test]
    fn rgb32_gets_an_opaque_alpha() {
        let src = [1, 2, 3, 0];
        let mut dst = [0; 4];
        copy(Packing::Rgb32, 1, 1, rows(&src, 0, 4), &mut dst, 4).unwrap();
        assert_eq!(dst, [1, 2, 3, 0xff]);
    }

    #[test]
    fn rows_outside_either_buffer_are_refused() {
        let src = [0u8; 8];
        let mut dst = [0u8; 64];
        assert!(copy(Packing::Yuy2, 2, 4, rows(&src, 0, 4), &mut dst, 4).is_err());
        let mut small = [0u8; 4];
        assert!(copy(Packing::Yuy2, 2, 2, rows(&src, 0, 4), &mut small, 4).is_err());
        assert!(copy(Packing::Argb32, 2, 1, rows(&src, 0, 8), &mut dst, 4).is_err());
    }
}

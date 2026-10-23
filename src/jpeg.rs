// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

//! JPEG compression of captured frames.

use std::{error::Error, sync::Arc};

use edgefirst_camera::{Frame, Mirror, StreamConfig};
use edgefirst_image::{ConvertOptions, ImageProcessor, ImageProcessorTrait};
use edgefirst_tensor::{CpuAccess, DType, PixelFormat, TensorDyn};

use crate::encode::{mirror_options, warm_up};

/// Converts frames to RGBA at the stream size and compresses them.
pub(crate) struct JpegStage {
    processor: ImageProcessor,
    dst: TensorDyn,
    options: ConvertOptions,
    quality: u8,
}

impl JpegStage {
    /// # Errors
    ///
    /// The image crate cannot start or allocate the RGBA image.
    pub(crate) fn new(
        capture: &StreamConfig,
        size: (u32, u32),
        mirror: Option<Mirror>,
        quality: u8,
    ) -> Result<Self, Box<dyn Error>> {
        let mut processor = ImageProcessor::new()?;
        let mut dst = processor.create_image(
            size.0 as usize,
            size.1 as usize,
            PixelFormat::Rgba,
            DType::U8,
            None,
            CpuAccess::ReadWrite,
        )?;
        let options = mirror_options(mirror);
        warm_up(&mut processor, capture, &mut dst, options)?;
        Ok(Self {
            processor,
            dst,
            options,
            quality,
        })
    }

    /// Compresses `frame`, releasing it once converted.
    pub(crate) fn encode(&mut self, frame: Arc<Frame>) -> Result<Vec<u8>, Box<dyn Error>> {
        self.processor
            .convert(frame.tensor(), &mut self.dst, Some(self.options))?;
        drop(frame);
        let width = self.dst.width().unwrap_or_default();
        let height = self.dst.height().unwrap_or_default();
        let pitch = self.dst.effective_row_stride().unwrap_or(width * 4);
        let pixels = self.dst.map_bytes(CpuAccess::Read)?;
        let image = turbojpeg::Image {
            pixels: &pixels[..pitch * height],
            width,
            pitch,
            height,
            format: turbojpeg::PixelFormat::RGBA,
        };
        let jpeg = turbojpeg::compress(image, i32::from(self.quality), turbojpeg::Subsamp::Sub2x2)?;
        Ok(jpeg.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgefirst_camera::CameraBuilder;
    use std::time::Duration;

    #[test]
    fn compresses_a_frame_at_the_stream_size() {
        let mut cam = CameraBuilder::source("mock:64x48@60")
            .unwrap()
            .open()
            .unwrap();
        cam.start().unwrap();
        let frame = cam.next_frame(Some(Duration::from_secs(1))).unwrap();
        let mut stage = JpegStage::new(cam.config(), (32, 24), Some(Mirror::Both), 85).unwrap();
        let jpeg = stage.encode(Arc::new(frame)).unwrap();
        assert_eq!(&jpeg[..2], &[0xff, 0xd8], "a JPEG starts with SOI");
        let header = turbojpeg::read_header(&jpeg).unwrap();
        assert_eq!((header.width, header.height), (32, 24));
        assert_eq!(cam.stats().held, 0, "the frame was released");
    }
}

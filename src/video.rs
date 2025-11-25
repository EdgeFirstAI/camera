// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

use edgefirst_camera::image::{Image, ImageManager, Rotation};
use std::{error::Error, os::raw::c_int};
use tracing::{debug, info_span};
use tracy_client::plot;
use videostream::{
    encoder::{Encoder, VSLEncoderProfileEnum, VSLRect},
    fourcc::FourCC,
    frame::Frame,
};

use crate::{args::H264Bitrate, TARGET_FPS};

/// Manager for hardware H.264 video encoding operations.
///
/// `VideoManager` provides an interface to the NXP hardware H.264 encoder
/// (Hantro H1) for real-time video compression. It supports configurable
/// bitrates, cropping, and tiling for high-resolution cameras.
///
/// # Example
///
/// ```no_run
/// use edgefirst_camera::{
///     image::{Image, NV12},
///     video::VideoManager,
/// };
/// use videostream::fourcc::FourCC;
/// # use edgefirst_camera::args::H264Bitrate;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mut video_mgr = VideoManager::new(FourCC(*b"H264"), 1920, 1080, H264Bitrate::Mbps25)?;
///
/// // Encode a frame (must be in NV12 format)
/// let nv12_image = Image::new(1920, 1080, NV12)?;
/// let (h264_data, is_keyframe) = video_mgr.encode_direct(&nv12_image)?;
/// # Ok(())
/// # }
/// ```
pub struct VideoManager {
    encoder: Encoder,
    crop: VSLRect,
    output_frame: Frame,
    /// Accumulated bits since last keyframe (for bitrate estimation)
    pub bits: usize,
}

impl VideoManager {
    /// Creates a new `VideoManager` for H.264 encoding.
    ///
    /// Initializes the hardware H.264 encoder with the specified parameters.
    ///
    /// # Arguments
    ///
    /// * `video_fmt` - Output encoder format (use `FourCC(*b"H264")` for H.264)
    /// * `width` - Video width in pixels (max 1920)
    /// * `height` - Video height in pixels (max 1080)
    /// * `bitrate` - Target encoding bitrate
    ///
    /// # Returns
    ///
    /// A new `VideoManager` ready for encoding frames.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Hardware encoder cannot be initialized
    /// - Dimensions exceed hardware limits (1920×1080)
    /// - Invalid format specified
    ///
    /// # Platform Requirements
    ///
    /// Requires NXP i.MX8M Plus with Hantro encoder support.
    pub fn new(
        video_fmt: FourCC,
        width: i32,
        height: i32,
        bitrate: H264Bitrate,
    ) -> Result<VideoManager, Box<dyn Error>> {
        let profile = match bitrate {
            H264Bitrate::Auto => VSLEncoderProfileEnum::Auto,
            H264Bitrate::Mbps5 => VSLEncoderProfileEnum::Kbps5000,
            H264Bitrate::Mbps25 => VSLEncoderProfileEnum::Kbps25000,
            H264Bitrate::Mbps50 => VSLEncoderProfileEnum::Kbps50000,
            H264Bitrate::Mbps100 => VSLEncoderProfileEnum::Kbps100000,
        };
        let encoder = Encoder::create(profile as u32, u32::from(video_fmt), TARGET_FPS)?;
        let crop = VSLRect::new(0, 0, width, height);
        let output_frame = encoder.new_output_frame(width, height, 30i64, 0, 0)?;
        Ok(Self {
            encoder,
            crop,
            output_frame,
            bits: 0,
        })
    }

    /// Creates a new `VideoManager` with custom cropping and FPS settings.
    ///
    /// This constructor is used for 4K tiling where each tile is a cropped
    /// region of the source image, encoded as a separate 1080p H.264 stream.
    ///
    /// # Arguments
    ///
    /// * `video_fmt` - Output encoder format (use `FourCC(*b"H264")` for H.264)
    /// * `output_width` - Output width in pixels (max 1920)
    /// * `output_height` - Output height in pixels (max 1080)
    /// * `crop_rect` - Source crop region as `(x, y, width, height)`
    /// * `bitrate` - Target encoding bitrate
    /// * `target_fps` - Optional FPS limit (useful for tiles)
    ///
    /// # Returns
    ///
    /// A new `VideoManager` configured for tiled encoding.
    ///
    /// # Errors
    ///
    /// Returns an error if hardware encoder initialization fails.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use edgefirst_camera::video::VideoManager;
    /// # use edgefirst_camera::args::H264Bitrate;
    /// # use videostream::fourcc::FourCC;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// // Encode top-left tile of a 4K image
    /// let video_mgr = VideoManager::new_with_crop(
    ///     FourCC(*b"H264"),
    ///     1920, // output size
    ///     1080,
    ///     (0, 0, 1920, 1080), // crop from top-left
    ///     H264Bitrate::Mbps25,
    ///     Some(15), // 15 FPS to reduce artifacts
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new_with_crop(
        video_fmt: FourCC,
        output_width: i32,
        output_height: i32,
        crop_rect: (i32, i32, i32, i32), // (x, y, width, height)
        bitrate: H264Bitrate,
        target_fps: Option<i32>,
    ) -> Result<VideoManager, Box<dyn Error>> {
        let profile = match bitrate {
            H264Bitrate::Auto => VSLEncoderProfileEnum::Auto,
            H264Bitrate::Mbps5 => VSLEncoderProfileEnum::Kbps5000,
            H264Bitrate::Mbps25 => VSLEncoderProfileEnum::Kbps25000,
            H264Bitrate::Mbps50 => VSLEncoderProfileEnum::Kbps50000,
            H264Bitrate::Mbps100 => VSLEncoderProfileEnum::Kbps100000,
        };

        let fps = target_fps.unwrap_or(TARGET_FPS);
        let encoder = Encoder::create(profile as u32, u32::from(video_fmt), fps)?;

        let (crop_x, crop_y, crop_width, crop_height) = crop_rect;
        let crop = VSLRect::new(crop_x, crop_y, crop_width, crop_height);

        let output_frame =
            encoder.new_output_frame(output_width, output_height, fps as i64, 0, 0)?;
        Ok(Self {
            encoder,
            crop,
            output_frame,
            bits: 0,
        })
    }

    /// Resizes an image and encodes it to H.264.
    ///
    /// Performs G2D hardware-accelerated resize followed by H.264 encoding.
    /// This is used when the camera resolution differs from the output
    /// resolution. The source image is converted to NV12 format before
    /// encoding.
    ///
    /// # Arguments
    ///
    /// * `source` - Source image (typically RGBA from camera)
    /// * `imgmgr` - ImageManager for G2D operations
    /// * `img` - Pre-allocated destination image (will be converted to NV12)
    ///
    /// # Returns
    ///
    /// A tuple of `(h264_data, is_keyframe)` where:
    /// - `h264_data` - Encoded H.264 NAL units
    /// - `is_keyframe` - `true` if this is an I-frame
    ///
    /// # Errors
    ///
    /// Returns an error if G2D conversion or H.264 encoding fails.
    pub fn resize_and_encode(
        &mut self,
        source: &Image,
        imgmgr: &ImageManager,
        img: &Image,
    ) -> Result<(Vec<u8>, bool), Box<dyn Error>> {
        info_span!("h264_resize")
            .in_scope(|| imgmgr.convert(source, img, None, Rotation::Rotation0))?;
        let frame: Frame = match img.try_into() {
            Ok(f) => f,
            Err(e) => {
                return Err(e);
            }
        };

        info_span!("h264_encode").in_scope(|| self.encode_from_vsl(&frame))
    }

    /// Encodes an image directly to H.264 without resizing.
    ///
    /// Use this when the source image is already in the correct resolution
    /// and format for encoding. The image must be in NV12 format.
    ///
    /// # Arguments
    ///
    /// * `source_img` - Source image (must be in NV12 format)
    ///
    /// # Returns
    ///
    /// A tuple of `(h264_data, is_keyframe)` where:
    /// - `h264_data` - Encoded H.264 NAL units
    /// - `is_keyframe` - `true` if this is an I-frame
    ///
    /// # Errors
    ///
    /// Returns an error if H.264 encoding fails.
    pub fn encode_direct(&mut self, source_img: &Image) -> Result<(Vec<u8>, bool), Box<dyn Error>> {
        let frame: Frame = match source_img.try_into() {
            Ok(f) => f,
            Err(e) => {
                return Err(e);
            }
        };

        info_span!("h264_encode_direct").in_scope(|| self.encode_from_vsl(&frame))
    }

    /// Updates the crop region for subsequent encoding operations.
    ///
    /// Allows dynamic adjustment of the source crop region without
    /// recreating the encoder.
    ///
    /// # Arguments
    ///
    /// * `crop_x` - X coordinate of crop region
    /// * `crop_y` - Y coordinate of crop region
    /// * `crop_width` - Width of crop region
    /// * `crop_height` - Height of crop region
    pub fn update_crop_region(
        &mut self,
        crop_x: i32,
        crop_y: i32,
        crop_width: i32,
        crop_height: i32,
    ) {
        self.crop = VSLRect::new(crop_x, crop_y, crop_width, crop_height);
    }

    fn encode_from_vsl(&mut self, source: &Frame) -> Result<(Vec<u8>, bool), Box<dyn Error>> {
        let mut key_frame: c_int = 0;
        let _ret = unsafe {
            self.encoder
                .frame(source, &self.output_frame, &self.crop, &mut key_frame)
        };
        let is_key = key_frame != 0;
        let ret = self.output_frame.mmap().unwrap().to_vec();

        if is_key && self.bits > 1000 {
            let bps = self.bits as f64 * 8.0 / 1000000.0;
            tracy_client::Client::is_running().then(|| plot!("h264_bitrate", bps));
            debug!("estimated h264 bitrate: {:.2} mbps", bps);
            self.bits = 0;
        }
        self.bits += ret.len();

        Ok((ret, is_key))
    }
}

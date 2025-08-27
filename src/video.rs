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

pub struct VideoManager {
    encoder: Encoder,
    crop: VSLRect,
    output_frame: Frame,
    pub bits: usize,
}

impl VideoManager {
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
        let encoder = Encoder::create(profile as u32, u32::from(video_fmt), TARGET_FPS);
        let crop = VSLRect::new(0, 0, width, height);
        let output_frame = match encoder.new_output_frame(width, height, -1, -1, -1) {
            Ok(f) => f,
            Err(e) => {
                return Err(e);
            }
        };
        Ok(Self {
            encoder,
            crop,
            output_frame,
            bits: 0,
        })
    }

    pub fn new_with_crop(
        video_fmt: FourCC,
        output_width: i32,
        output_height: i32,
        crop_rect: (i32, i32, i32, i32), // (x, y, width, height)
        bitrate: H264Bitrate,
    ) -> Result<VideoManager, Box<dyn Error>> {
        let profile = match bitrate {
            H264Bitrate::Auto => VSLEncoderProfileEnum::Auto,
            H264Bitrate::Mbps5 => VSLEncoderProfileEnum::Kbps5000,
            H264Bitrate::Mbps25 => VSLEncoderProfileEnum::Kbps25000,
            H264Bitrate::Mbps50 => VSLEncoderProfileEnum::Kbps50000,
            H264Bitrate::Mbps100 => VSLEncoderProfileEnum::Kbps100000,
        };
        let encoder = Encoder::create(profile as u32, u32::from(video_fmt), TARGET_FPS);
        let (crop_x, crop_y, crop_width, crop_height) = crop_rect;
        let crop = VSLRect::new(crop_x, crop_y, crop_width, crop_height);
        let output_frame = match encoder.new_output_frame(output_width, output_height, -1, -1, -1) {
            Ok(f) => f,
            Err(e) => {
                return Err(e);
            }
        };
        Ok(Self {
            encoder,
            crop,
            output_frame,
            bits: 0,
        })
    }

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

    pub fn encode_only(&mut self, img: &Image) -> Result<(Vec<u8>, bool), Box<dyn Error>> {
        let frame: Frame = match img.try_into() {
            Ok(f) => f,
            Err(e) => {
                return Err(e);
            }
        };

        info_span!("h264_encode").in_scope(|| self.encode_from_vsl(&frame))
    }

    pub fn encode_direct(&mut self, source_img: &Image) -> Result<(Vec<u8>, bool), Box<dyn Error>> {
        // Convert source image directly to frame and encode with VPU crop
        let frame: Frame = match source_img.try_into() {
            Ok(f) => f,
            Err(e) => {
                return Err(e);
            }
        };

        info_span!("h264_encode_direct").in_scope(|| self.encode_from_vsl(&frame))
    }

    fn encode_from_vsl(&mut self, source: &Frame) -> Result<(Vec<u8>, bool), Box<dyn Error>> {
        let mut key_frame: c_int = 0;
        let _ret = self
            .encoder
            .frame(source, &self.output_frame, &self.crop, &mut key_frame);
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

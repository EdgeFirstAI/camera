use camera::image::{Image, ImageManager};
use log::warn;
use std::{error::Error, os::raw::c_int, time::Instant};
use videostream::{
    encoder::{Encoder, VSLEncoderProfileEnum, VSLRect},
    fourcc::FourCC,
    frame::Frame,
};

use crate::H264Bitrate;

pub struct VideoManager {
    encoder: Encoder,
    crop: VSLRect,
}

impl VideoManager {
    pub fn new(
        video_fmt: FourCC,
        width: i32,
        height: i32,
        bitrate: H264Bitrate,
    ) -> Result<Self, Box<dyn Error>> {
        let profile = match bitrate {
            H264Bitrate::Auto => VSLEncoderProfileEnum::Auto,
            H264Bitrate::Kbps1000 => VSLEncoderProfileEnum::Kbps1000,
            H264Bitrate::Kbps2000 => VSLEncoderProfileEnum::Kbps2000,
            H264Bitrate::Kbps4000 => VSLEncoderProfileEnum::Kbps4000,
            H264Bitrate::Kbps8000 => VSLEncoderProfileEnum::Kbps8000,
            H264Bitrate::Kbps10000 => VSLEncoderProfileEnum::Kbps10000,
            H264Bitrate::Kbps20000 => VSLEncoderProfileEnum::Kbps20000,
            H264Bitrate::Kbps40000 => VSLEncoderProfileEnum::Kbps40000,
            H264Bitrate::Kbps80000 => VSLEncoderProfileEnum::Kbps80000,
            H264Bitrate::Kbps100000 => VSLEncoderProfileEnum::Kbps100000,
            H264Bitrate::Kbps200000 => VSLEncoderProfileEnum::Kbps200000,
            H264Bitrate::Kbps400000 => VSLEncoderProfileEnum::Kbps400000,
        };
        let encoder = Encoder::create(profile as u32, u32::from(video_fmt), 30);
        let crop = VSLRect::new(0, 0, width, height);
        Self { encoder, crop }
    }

    pub fn resize_and_encode(
        &self,
        source: &Image,
        imgmgr: &ImageManager,
        img: &Image,
    ) -> Result<(Vec<u8>, bool), Box<dyn Error>> {
        imgmgr.convert(source, img, None)?;
        let frame: Frame = match img.try_into() {
            Ok(f) => f,
            Err(e) => {
                return Err(e);
            }
        };
        self.encode_from_vsl(&frame)
    }

    fn encode_from_vsl(&self, source: &Frame) -> Result<(Vec<u8>, bool), Box<dyn Error>> {
        let encoded_frame = match self.encoder.new_output_frame(
            self.crop.get_width(),
            self.crop.get_height(),
            -1,
            -1,
            -1,
        ) {
            Ok(f) => f,
            Err(e) => {
                return Err(e);
            }
        };

        let mut key_frame: c_int = 0;
        let _ret = self
            .encoder
            .frame(source, &encoded_frame, &self.crop, &mut key_frame);
        let is_key = key_frame != 0;
        return Ok((encoded_frame.mmap().unwrap().to_vec(), is_key));
    }
}

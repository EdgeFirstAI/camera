use crate::TIME_LIMIT;
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
    output_frame: Frame,
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
        })
    }

    pub fn resize_and_encode(
        &self,
        source: &Image,
        imgmgr: &ImageManager,
        img: &Image,
    ) -> Result<(Vec<u8>, bool), Box<dyn Error>> {
        let now = Instant::now();
        imgmgr.convert(source, img, None)?;
        let frame: Frame = match img.try_into() {
            Ok(f) => f,
            Err(e) => {
                return Err(e);
            }
        };
        let convert_and_resize_time = now.elapsed();
        if convert_and_resize_time.as_nanos() > TIME_LIMIT {
            warn!(
                "h264 convert and resize time: {:?} exceeds 33ms",
                convert_and_resize_time
            )
        }

        self.encode_from_vsl(&frame)
    }

    fn encode_from_vsl(&self, source: &Frame) -> Result<(Vec<u8>, bool), Box<dyn Error>> {
        let mut key_frame: c_int = 0;
        let now = Instant::now();
        let _ret = self
            .encoder
            .frame(source, &self.output_frame, &self.crop, &mut key_frame);
        let is_key = key_frame != 0;
        let encode_time = now.elapsed();
        if encode_time.as_nanos() > TIME_LIMIT {
            warn!(
                "h264 encode encode frame time: {:?} exceeds 33ms",
                encode_time
            )
        }
        let now = Instant::now();
        let ret = self.output_frame.mmap().unwrap().to_vec();
        let mmap_time = now.elapsed();
        if mmap_time.as_nanos() > TIME_LIMIT {
            warn!("h264 encode mmap time: {:?} exceeds 33ms", mmap_time)
        }
        Ok((ret, is_key))
    }
}

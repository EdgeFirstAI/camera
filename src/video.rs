use crate::TIME_LIMIT;
use camera::image::{Image, ImageManager};
use log::{trace, warn};
use std::{error::Error, os::raw::c_int, time::Instant};
use videostream::{
    encoder::{Encoder, VSLRect},
    fourcc::FourCC,
    frame::Frame,
};

pub struct VideoManager {
    encoder: Encoder,
    crop: VSLRect,
    output_frame: Frame,
}
impl VideoManager {
    pub fn new(video_fmt: FourCC, width: i32, height: i32) -> Result<VideoManager, Box<dyn Error>> {
        let encoder = Encoder::create(0, u32::from(video_fmt), 30);
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
        if convert_and_resize_time > TIME_LIMIT {
            warn!(
                "h264 convert and resize time: {:?} exceeds {:?}",
                convert_and_resize_time, TIME_LIMIT
            )
        } else {
            trace!(
                "h264 convert and resize time: {:?}",
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
        if encode_time > TIME_LIMIT {
            warn!(
                "h264 encode encode frame time: {:?} exceeds {:?}",
                encode_time, TIME_LIMIT
            )
        } else {
            trace!("h264 encode encode frame time: {:?}", encode_time)
        }
        let now = Instant::now();
        let ret = self.output_frame.mmap().unwrap().to_vec();
        let mmap_time = now.elapsed();
        if mmap_time > TIME_LIMIT {
            warn!(
                "h264 encode mmap time: {:?} exceeds {:?}",
                mmap_time, TIME_LIMIT
            )
        } else {
            trace!("h264 encode mmap time: {:?}", mmap_time)
        }
        Ok((ret, is_key))
    }
}

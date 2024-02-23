use camera::image::{Image, ImageManager};
use std::{error::Error, os::raw::c_int};
use videostream::{
    encoder::{Encoder, VSLRect},
    fourcc::FourCC,
    frame::Frame,
};

pub struct VideoManager {
    encoder: Encoder,
    crop: VSLRect,
}

impl VideoManager {
    pub fn new(video_fmt: FourCC, width: i32, height: i32) -> Self {
        let encoder = Encoder::create(0, u32::from(video_fmt), 30);
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
        return self.encode_from_vsl(&frame);
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
            .frame(&source, &encoded_frame, &self.crop, &mut key_frame);
        let is_key = if key_frame != 0 { true } else { false };
        return Ok(((&encoded_frame.mmap()).unwrap().to_vec(), is_key));
    }
}

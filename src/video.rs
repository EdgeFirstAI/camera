use std::{
    error::Error,
    os::{fd::AsRawFd, raw::c_int},
};
use videostream::{
    camera::CameraBuffer,
    encoder::{Encoder, VSLRect},
    fourcc::FourCC,
    frame::Frame,
};
pub fn cam_to_vslframe(buf: &CameraBuffer) -> Result<Frame, Box<dyn Error>> {
    let frame = match Frame::new(
        buf.width().try_into().unwrap(),
        buf.height().try_into().unwrap(),
        0,
        buf.format().to_string().as_str(),
    ) {
        Ok(f) => f,
        Err(e) => return Err(e),
    };
    match frame.attach(buf.fd().as_raw_fd(), 0, 0) {
        Ok(_) => (),
        Err(e) => return Err(e),
    }
    return Ok(frame);
}
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

    pub fn encode_and_save(
        &mut self,
        source: &CameraBuffer,
    ) -> Result<(Vec<u8>, bool), Box<dyn Error>> {
        let frame = match cam_to_vslframe(source) {
            Ok(f) => f,
            Err(e) => {
                return Err(e);
            }
        };
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
            .frame(&frame, &encoded_frame, &mut self.crop, &mut key_frame);
        let is_key = if key_frame != 0 { true } else { false };
        return Ok(((&encoded_frame.mmap()).unwrap().to_vec(), is_key));
    }
}

use camera::image::{Image, ImageManager};
use log::{debug, trace, warn};
use std::{
    error::Error,
    os::raw::c_int,
    time::{Duration, Instant},
};
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
// Time limit is 8ms for 1080p frame.
// 8ms / [(1920x1080)/1_000_000] = 4.823 ms per megapixel
const H264_CONVERT_TIME_LIMIT_PER_MPIX: Duration = Duration::from_micros(4823);
// Seems like bitrate does not affect h264 encode time
const H264_ENCODE_TIME_LIMIT: Duration = Duration::from_millis(10);
// Varies by bitrate, but appears to max out at 2.7 ms at maximum bitrate
const H264_MMAP_TIME_LIMIT: Duration = Duration::from_millis(3);
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

    pub fn resize_and_encode(
        &mut self,
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
        let mpix = (source.width() * source.height()) as f64 / 1_000_000.0;
        if convert_and_resize_time > H264_CONVERT_TIME_LIMIT_PER_MPIX.mul_f64(mpix) {
            warn!(
                "h264 convert and resize time: {:?} exceeds {:?}",
                convert_and_resize_time,
                H264_CONVERT_TIME_LIMIT_PER_MPIX.mul_f64(mpix)
            )
        } else {
            trace!(
                "h264 convert and resize time: {:?}",
                convert_and_resize_time
            )
        }

        self.encode_from_vsl(&frame)
    }

    fn encode_from_vsl(&mut self, source: &Frame) -> Result<(Vec<u8>, bool), Box<dyn Error>> {
        let mut key_frame: c_int = 0;
        let now = Instant::now();
        let _ret = self
            .encoder
            .frame(source, &self.output_frame, &self.crop, &mut key_frame);
        let is_key = key_frame != 0;
        let encode_time = now.elapsed();
        if encode_time > H264_ENCODE_TIME_LIMIT {
            warn!(
                "h264 encode encode frame time: {:?} exceeds {:?}",
                encode_time, H264_ENCODE_TIME_LIMIT
            )
        } else {
            trace!("h264 encode encode frame time: {:?}", encode_time)
        }
        let now = Instant::now();
        let ret = self.output_frame.mmap().unwrap().to_vec();
        let mmap_time = now.elapsed();
        if mmap_time > H264_MMAP_TIME_LIMIT {
            warn!(
                "h264 encode mmap time: {:?} exceeds {:?}",
                mmap_time, H264_MMAP_TIME_LIMIT
            )
        } else {
            trace!("h264 encode mmap time: {:?}", mmap_time)
        }

        if is_key && self.bits > 1000 {
            debug!(
                "estimated bitrate: {:.2} mbps",
                self.bits as f32 * 8.0 / 1000000.0
            );
            self.bits = 0;
        }
        self.bits += ret.len();
        Ok((ret, is_key))
    }
}

use core::fmt;
use dma_buf::DmaBuf;
use dma_heap::{Heap, HeapKind};
use g2d_sys::{g2d as g2d_library, g2d_surface, G2DFormat, G2DPhysical};
use std::{
    error::Error,
    ffi::c_void,
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd},
        unix::io::OwnedFd,
    },
    ptr::null_mut,
};
use turbojpeg::{libc::dup, OwnedBuf};
use videostream::{camera::CameraBuffer, fourcc::FourCC, frame::Frame};

pub const RGB3: FourCC = FourCC(*b"RGB3");
pub const RGBX: FourCC = FourCC(*b"RGBX");
pub const RGBA: FourCC = FourCC(*b"RGBA");
pub const YUYV: FourCC = FourCC(*b"YUYV");
pub const NV12: FourCC = FourCC(*b"NV12");

pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub struct ImageManager {
    lib: g2d_library,
    handle: *mut c_void,
}

impl ImageManager {
    pub fn new() -> Result<Self, Box<dyn Error>> {
        let lib = unsafe { g2d_library::new("libg2d.so.2") }?;
        let mut handle: *mut c_void = null_mut();

        if unsafe { lib.g2d_open(&mut handle) } != 0 {
            let err = io::Error::last_os_error();
            return Err(Box::new(err));
        }

        Ok(Self { lib, handle })
    }

    pub fn convert(
        &self,
        from: &Image,
        to: &Image,
        crop: Option<Rect>,
    ) -> Result<(), Box<dyn Error>> {
        let from_fd = from.fd.try_clone()?;
        let from_phys: G2DPhysical = DmaBuf::from(from_fd).into();

        let to_fd = to.fd.try_clone()?;
        let to_phys: G2DPhysical = DmaBuf::from(to_fd).into();

        let mut src = g2d_surface {
            planes: [from_phys.into(), 0, 0],
            format: G2DFormat::from(from.format).format(),
            left: 0,
            top: 0,
            right: from.width,
            bottom: from.height,
            stride: from.width,
            width: from.width,
            height: from.height,
            blendfunc: 0,
            clrcolor: 0,
            rot: 0,
            global_alpha: 0,
        };

        if let Some(r) = crop {
            src.left = r.x;
            src.top = r.y;
            src.right = r.x + r.width;
            src.bottom = r.y + r.height;
        }

        let mut dst = g2d_surface {
            planes: [to_phys.into(), 0, 0],
            format: G2DFormat::from(to.format).format(),
            left: 0,
            top: 0,
            right: to.width,
            bottom: to.height,
            stride: to.width,
            width: to.width,
            height: to.height,
            blendfunc: 0,
            clrcolor: 0,
            rot: 0,
            global_alpha: 0,
        };

        if unsafe { self.lib.g2d_blit(self.handle, &mut src, &mut dst) } != 0 {
            return Err(Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                "g2d_blit failed",
            )));
        }
        if unsafe { self.lib.g2d_finish(self.handle) } != 0 {
            return Err(Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                "g2d_finish failed",
            )));
        }
        // FIXME: A cache invalidation is required here, currently missing!

        Ok(())
    }
}

impl Drop for ImageManager {
    fn drop(&mut self) {
        _ = unsafe { self.lib.g2d_close(self.handle) }
    }
}

#[derive(Debug)]
pub struct Image {
    fd: OwnedFd,
    width: i32,
    height: i32,
    format: FourCC,
}

const fn format_row_stride(format: FourCC, width: i32) -> usize {
    match format {
        RGB3 => 3 * width as usize,
        RGBA => 4 * width as usize,
        YUYV => 2 * width as usize,
        NV12 => width as usize / 2 + width as usize,
        _ => todo!(),
    }
}

const fn image_size(width: i32, height: i32, format: FourCC) -> usize {
    format_row_stride(format, width) * height as usize
}

impl Image {
    pub fn new(width: i32, height: i32, format: FourCC) -> Result<Self, Box<dyn Error>> {
        let heap = Heap::new(HeapKind::Cma)?;
        let fd = heap.allocate(image_size(width, height, format))?;

        Ok(Self {
            fd,
            width,
            height,
            format,
        })
    }

    pub fn from_camera(buffer: &CameraBuffer) -> Result<Self, Box<dyn Error>> {
        let fd = buffer.fd();

        Ok(Self {
            fd: fd.try_clone_to_owned()?,
            width: buffer.width(),
            height: buffer.height(),
            format: buffer.format(),
        })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    pub fn dmabuf(&self) -> DmaBuf {
        unsafe { DmaBuf::from_raw_fd(dup(self.fd.as_raw_fd())) }
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn format(&self) -> FourCC {
        self.format
    }

    pub fn size(&self) -> usize {
        format_row_stride(self.format, self.width) * self.height as usize
    }
}

impl TryFrom<&Image> for Frame {
    type Error = Box<dyn Error>;

    fn try_from(img: &Image) -> Result<Self, Self::Error> {
        let frame = match Frame::new(
            img.width().try_into().unwrap(),
            img.height().try_into().unwrap(),
            0,
            img.format().to_string().as_str(),
        ) {
            Ok(f) => f,
            Err(e) => return Err(e),
        };
        match frame.attach(img.fd().as_raw_fd(), 0, 0) {
            Ok(_) => (),
            Err(e) => return Err(e),
        }
        Ok(frame)
    }
}

impl fmt::Display for Image {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}x{} {} fd:{:?}",
            self.width, self.height, self.format, self.fd
        )
    }
}

pub fn encode_jpeg(pix: &[u8], img: Option<&Image>) -> Result<OwnedBuf, Box<dyn Error>> {
    let img2 = match img {
        Some(img) => turbojpeg::Image {
            width: img.width as usize,
            height: img.height as usize,
            format: turbojpeg::PixelFormat::RGBA,
            pixels: pix,
            pitch: img.width as usize * 4,
        },
        None => {
            return Err(Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no image provided",
            )));
        }
    };

    let res = turbojpeg::compress(img2, 100, turbojpeg::Subsamp::Sub2x2);
    match res {
        Ok(buf) => Ok(buf),
        Err(e) => Err(Box::new(e)),
    }
}

// #[cfg(test)]
// mod tests {
//     use super::*;
// }

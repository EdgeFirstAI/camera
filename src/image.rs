// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

use core::fmt;
use dma_buf::DmaBuf;
use dma_heap::{Heap, HeapKind};
use g2d_sys::{
    g2d as g2d_library, g2d_buf, g2d_rotation_G2D_ROTATION_0, g2d_rotation_G2D_ROTATION_180,
    g2d_rotation_G2D_ROTATION_270, g2d_rotation_G2D_ROTATION_90, g2d_surface, g2d_surface_new,
    guess_version, G2DFormat, G2DPhysical,
};
use std::{
    error::Error,
    ffi::c_void,
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd},
        unix::io::OwnedFd,
    },
    ptr::null_mut,
    slice::{from_raw_parts, from_raw_parts_mut},
};
use tracing::{debug, warn};
use turbojpeg::{
    libc::{dup, mmap, munmap, MAP_SHARED, PROT_READ, PROT_WRITE},
    OwnedBuf,
};
use videostream::{camera::CameraBuffer, encoder::VSLRect, fourcc::FourCC, frame::Frame};

/// RGB 24-bit pixel format (8 bits per channel, no alpha)
pub const RGB3: FourCC = FourCC(*b"RGB3");

/// RGBX 32-bit pixel format (8 bits per channel, unused alpha)
pub const RGBX: FourCC = FourCC(*b"RGBX");

/// RGBA 32-bit pixel format (8 bits per channel, with alpha)
pub const RGBA: FourCC = FourCC(*b"RGBA");

/// YUYV 4:2:2 YUV packed format (common camera output format)
pub const YUYV: FourCC = FourCC(*b"YUYV");

/// NV12 4:2:0 YUV semi-planar format (efficient for video encoding)
pub const NV12: FourCC = FourCC(*b"NV12");

/// Rectangle specification for crop operations.
///
/// Defines a rectangular region within an image for cropping,
/// tiling, or region-of-interest operations.
pub struct Rect {
    /// X coordinate of top-left corner
    pub x: i32,
    /// Y coordinate of top-left corner
    pub y: i32,
    /// Width of the rectangle in pixels
    pub width: i32,
    /// Height of the rectangle in pixels
    pub height: i32,
}

impl From<VSLRect> for Rect {
    fn from(value: VSLRect) -> Self {
        Rect {
            x: value.x(),
            y: value.y(),
            width: value.width(),
            height: value.height(),
        }
    }
}

/// Image rotation angles supported by G2D hardware.
///
/// The G2D hardware accelerator supports 90-degree rotations
/// for efficient image transformation without CPU intervention.
#[allow(dead_code)]
#[derive(Copy, Clone, Debug)]
pub enum Rotation {
    /// No rotation (0 degrees)
    Rotation0 = g2d_rotation_G2D_ROTATION_0 as isize,
    /// Rotate 90 degrees clockwise
    Rotation90 = g2d_rotation_G2D_ROTATION_90 as isize,
    /// Rotate 180 degrees
    Rotation180 = g2d_rotation_G2D_ROTATION_180 as isize,
    /// Rotate 270 degrees clockwise (90 degrees counter-clockwise)
    Rotation270 = g2d_rotation_G2D_ROTATION_270 as isize,
}
pub struct G2DBuffer<'a> {
    buf: *mut g2d_buf,
    imgmgr: &'a ImageManager,
}

#[allow(dead_code)]
impl G2DBuffer<'_> {
    /// Get the DMA buffer handle.
    ///
    /// # Safety
    ///
    /// This function dereferences a raw pointer to a `g2d_buf` structure.
    /// The caller must ensure that:
    /// - The `G2DBuffer` was properly initialized with a valid `g2d_buf`
    ///   pointer
    /// - The underlying buffer has not been freed
    /// - No data races occur when accessing the buffer handle
    pub unsafe fn buf_handle(&self) -> *mut c_void {
        (*self.buf).buf_handle
    }

    /// Get the virtual address of the DMA buffer.
    ///
    /// # Safety
    ///
    /// This function dereferences a raw pointer to a `g2d_buf` structure.
    /// The caller must ensure that:
    /// - The `G2DBuffer` was properly initialized with a valid `g2d_buf`
    ///   pointer
    /// - The underlying buffer has not been freed
    /// - No data races occur when accessing the buffer's virtual address
    /// - The returned pointer is only dereferenced while the buffer remains
    ///   valid
    pub unsafe fn buf_vaddr(&self) -> *mut c_void {
        (*self.buf).buf_vaddr
    }

    pub fn buf_paddr(&self) -> i32 {
        unsafe { (*self.buf).buf_paddr }
    }

    pub fn buf_size(&self) -> i32 {
        unsafe { (*self.buf).buf_size }
    }
}

impl Drop for G2DBuffer<'_> {
    fn drop(&mut self) {
        self.imgmgr.free(self);
        debug!("G2D Buffer freed")
    }
}

/// Manager for NXP G2D hardware accelerator operations.
///
/// `ImageManager` provides a safe interface to the NXP i.MX8 G2D hardware
/// accelerator for efficient image processing operations including format
/// conversion, scaling, cropping, and rotation.
///
/// # Thread Safety
///
/// `ImageManager` is **not** thread-safe. Create separate instances for each
/// thread, or use synchronization primitives to protect shared access.
///
/// # Example
///
/// ```no_run
/// use edgefirst_camera::image::{Image, ImageManager, Rotation, NV12, YUYV};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let imgmgr = ImageManager::new()?;
/// let src = Image::new(1920, 1080, YUYV)?;
/// let dst = Image::new(1920, 1080, NV12)?;
///
/// // Convert YUYV to NV12 using hardware acceleration
/// imgmgr.convert(&src, &dst, None, Rotation::Rotation0)?;
/// # Ok(())
/// # }
/// ```
pub struct ImageManager {
    lib: g2d_library,
    version: g2d_sys::Version,
    handle: *mut c_void,
}

const G2D_2_3_0: g2d_sys::Version = g2d_sys::Version {
    major: 6,
    minor: 4,
    patch: 11,
    num: 1049711,
};

impl ImageManager {
    /// Creates a new ImageManager instance and opens the G2D hardware device.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The G2D library cannot be loaded (`libg2d.so.2`)
    /// - The G2D device cannot be opened (usually `/dev/galcore`)
    /// - Insufficient permissions to access the hardware
    ///
    /// # Platform Requirements
    ///
    /// Requires NXP i.MX8M Plus with G2D hardware support.
    pub fn new() -> Result<Self, Box<dyn Error>> {
        let lib = unsafe { g2d_library::new("libg2d.so.2") }?;
        let mut handle: *mut c_void = null_mut();

        if unsafe { lib.g2d_open(&mut handle) } != 0 {
            let err = io::Error::last_os_error();
            return Err(Box::new(err));
        }
        let version = guess_version(&lib).unwrap_or(G2D_2_3_0);
        Ok(Self {
            lib,
            handle,
            version,
        })
    }

    pub fn version(&self) -> g2d_sys::Version {
        self.version
    }

    /// Allocates a G2D buffer for hardware-accelerated operations.
    ///
    /// # Arguments
    ///
    /// * `width` - Buffer width in pixels
    /// * `height` - Buffer height in pixels
    /// * `channels` - Number of bytes per pixel
    ///
    /// # Errors
    ///
    /// Returns an error if the G2D driver fails to allocate the buffer.
    pub fn alloc(
        &self,
        width: i32,
        height: i32,
        channels: i32,
    ) -> Result<G2DBuffer<'_>, Box<dyn Error>> {
        let g2d_buf = unsafe { self.lib.g2d_alloc(width * height * channels, 0) };
        if g2d_buf.is_null() {
            return Err(Box::new(io::Error::other("g2d_alloc failed")));
        }
        debug!("G2D Buffer alloc'd");
        Ok(G2DBuffer {
            buf: g2d_buf,
            imgmgr: self,
        })
    }

    pub fn free(&self, buf: &mut G2DBuffer) {
        unsafe {
            self.lib.g2d_free(buf.buf);
        }
    }

    pub fn g2d_buf_fd(&self, buf: &G2DBuffer) -> OwnedFd {
        let fd = unsafe { self.lib.g2d_buf_export_fd(buf.buf) };
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    #[allow(dead_code)]
    pub fn convert(
        &self,
        from: &Image,
        to: &Image,
        crop: Option<Rect>,
        rot: Rotation,
    ) -> Result<(), Box<dyn Error>> {
        if self.version >= G2D_2_3_0 {
            self.convert_new(from, to, crop, rot)
        } else {
            self.convert_old(from, to, crop, rot)
        }
    }

    pub fn convert_old(
        &self,
        from: &Image,
        to: &Image,
        crop: Option<Rect>,
        rot: Rotation,
    ) -> Result<(), Box<dyn Error>> {
        let mut src: g2d_surface = from.try_into()?;

        if let Some(r) = crop {
            src.left = r.x;
            src.top = r.y;
            src.right = r.x + r.width;
            src.bottom = r.y + r.height;
        }

        let mut dst: g2d_surface = to.try_into()?;

        dst.rot = rot as u32;

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

    /// Converts an image using the new G2D API (version 2.3.0+).
    ///
    /// Performs hardware-accelerated format conversion, with optional cropping
    /// and rotation. This method is used on newer NXP platforms.
    ///
    /// # Arguments
    ///
    /// * `from` - Source image (must be DMA-backed)
    /// * `to` - Destination image (must be DMA-backed)
    /// * `crop` - Optional cropping rectangle
    /// * `rot` - Rotation angle (0, 90, 180, or 270 degrees)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - G2D blit operation fails
    /// - Images are not compatible (invalid formats or dimensions)
    /// - Hardware operation cannot complete
    pub fn convert_new(
        &self,
        from: &Image,
        to: &Image,
        crop: Option<Rect>,
        rot: Rotation,
    ) -> Result<(), Box<dyn Error>> {
        let mut src: g2d_surface_new = from.try_into()?;

        if let Some(r) = crop {
            src.left = r.x;
            src.top = r.y;
            src.right = r.x + r.width;
            src.bottom = r.y + r.height;
        }

        let mut dst: g2d_surface_new = to.try_into()?;

        dst.rot = rot as u32;

        if unsafe {
            // force cast the g2d_surface_new to g2d_surface so it can be sent to the
            // g2d_blit function
            self.lib.g2d_blit(
                self.handle,
                &raw mut src as *mut g2d_surface,
                &raw mut dst as *mut g2d_surface,
            )
        } != 0
        {
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

    pub fn convert_phys(
        &self,
        from: &Frame,
        to: &Image,
        crop: &Option<Rect>,
    ) -> Result<(), Box<dyn Error>> {
        if self.version >= G2D_2_3_0 {
            self.convert_phys_new(from, to, crop)
        } else {
            self.convert_phys_old(from, to, crop)
        }
    }

    fn convert_phys_old(
        &self,
        from: &Frame,
        to: &Image,
        crop: &Option<Rect>,
    ) -> Result<(), Box<dyn Error>> {
        let mut src: g2d_surface = from.into();

        if let Some(r) = crop {
            src.left = r.x;
            src.top = r.y;
            src.right = r.x + r.width;
            src.bottom = r.y + r.height;
        }

        let mut dst: g2d_surface = to.try_into()?;

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

    fn convert_phys_new(
        &self,
        from: &Frame,
        to: &Image,
        crop: &Option<Rect>,
    ) -> Result<(), Box<dyn Error>> {
        let mut src: g2d_surface_new = from.into();

        if let Some(r) = crop {
            src.left = r.x;
            src.top = r.y;
            src.right = r.x + r.width;
            src.bottom = r.y + r.height;
        }

        let mut dst: g2d_surface_new = to.try_into()?;

        if unsafe {
            // force cast the g2d_surface_new to g2d_surface so it can be sent to the
            // g2d_blit function
            self.lib.g2d_blit(
                self.handle,
                &raw mut src as *mut g2d_surface,
                &raw mut dst as *mut g2d_surface,
            )
        } != 0
        {
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
        _ = unsafe { self.lib.g2d_close(self.handle) };
        debug!("G2D closed");
    }
}

/// DMA-backed image buffer for zero-copy image operations.
///
/// `Image` represents an image buffer allocated in DMA (Direct Memory Access)
/// memory, enabling zero-copy sharing between processes and hardware
/// accelerators. The buffer is automatically freed when the `Image` is dropped.
///
/// # Example
///
/// ```no_run
/// use edgefirst_camera::image::{Image, YUYV};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// // Allocate a 1080p YUYV image in DMA memory
/// let img = Image::new(1920, 1080, YUYV)?;
///
/// // Image dimensions and format can be queried
/// assert_eq!(img.width(), 1920);
/// assert_eq!(img.height(), 1080);
/// assert_eq!(img.format(), YUYV);
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Image {
    fd: OwnedFd,
    width: u32,
    height: u32,
    format: FourCC,
}

const fn format_row_stride(format: FourCC, width: u32) -> usize {
    match format {
        RGB3 => 3 * width as usize,
        RGBX => 4 * width as usize,
        RGBA => 4 * width as usize,
        YUYV => 2 * width as usize,
        NV12 => width as usize / 2 + width as usize,
        _ => todo!(),
    }
}

const fn image_size(width: u32, height: u32, format: FourCC) -> usize {
    format_row_stride(format, width) * height as usize
}

impl Image {
    /// Allocates a new DMA-backed image buffer.
    ///
    /// Creates an image buffer in CMA (Contiguous Memory Allocator) DMA memory,
    /// suitable for hardware-accelerated operations and zero-copy sharing.
    ///
    /// # Arguments
    ///
    /// * `width` - Image width in pixels
    /// * `height` - Image height in pixels
    /// * `format` - Pixel format (YUYV, NV12, RGBA, etc.)
    ///
    /// # Returns
    ///
    /// A new `Image` with the specified dimensions and format.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - DMA heap allocation fails (out of memory)
    /// - Invalid dimensions or format specified
    /// - DMA heap device is not accessible
    ///
    /// # Example
    ///
    /// ```no_run
    /// use edgefirst_camera::image::{Image, YUYV};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let img = Image::new(1920, 1080, YUYV)?;
    /// println!("Allocated {} bytes in DMA memory", img.size());
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(width: u32, height: u32, format: FourCC) -> Result<Self, Box<dyn Error>> {
        let heap = Heap::new(HeapKind::Cma)?;
        let fd = heap.allocate(image_size(width, height, format))?;
        Ok(Self {
            fd,
            width,
            height,
            format,
        })
    }

    pub fn new_preallocated(fd: OwnedFd, width: u32, height: u32, format: FourCC) -> Self {
        Self {
            fd,
            width,
            height,
            format,
        }
    }

    /// Creates an `Image` from a V4L2 camera buffer.
    ///
    /// Wraps an existing V4L2 camera buffer (from the videostream library)
    /// in an `Image` structure, enabling G2D operations on camera frames.
    ///
    /// # Arguments
    ///
    /// * `buffer` - Reference to a V4L2 camera buffer
    ///
    /// # Errors
    ///
    /// Returns an error if the file descriptor cannot be duplicated.
    pub fn from_camera(buffer: &CameraBuffer) -> Result<Self, Box<dyn Error>> {
        let fd = buffer.fd();

        Ok(Self {
            fd: fd.try_clone_to_owned()?,
            width: buffer.width() as u32,
            height: buffer.height() as u32,
            format: buffer.format(),
        })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    pub fn raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    pub fn dmabuf(&self) -> DmaBuf {
        unsafe { DmaBuf::from_raw_fd(dup(self.fd.as_raw_fd())) }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn format(&self) -> FourCC {
        self.format
    }

    pub fn size(&self) -> usize {
        format_row_stride(self.format, self.width) * self.height as usize
    }

    pub fn mmap(&mut self) -> MappedImage {
        let image_size = image_size(self.width, self.height, self.format);
        unsafe {
            let mmap = mmap(
                null_mut(),
                image_size,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                self.raw_fd(),
                0,
            ) as *mut u8;
            MappedImage {
                mmap,
                len: image_size,
            }
        }
    }
}

impl TryFrom<&Image> for Frame {
    type Error = Box<dyn Error>;

    fn try_from(img: &Image) -> Result<Self, Self::Error> {
        let frame = Frame::new(
            img.width(),
            img.height(),
            0,
            img.format().to_string().as_str(),
        )?;
        frame.attach(img.fd().as_raw_fd(), 0, 0)?;
        Ok(frame)
    }
}

impl TryFrom<&Image> for g2d_surface {
    type Error = std::io::Error;

    fn try_from(img: &Image) -> Result<Self, Self::Error> {
        let to_fd = img.fd.try_clone()?;
        let to_phys: G2DPhysical = DmaBuf::from(to_fd).into();
        Ok(Self {
            planes: [to_phys.into(), 0, 0],
            format: G2DFormat::from(img.format).format(),
            left: 0,
            top: 0,
            right: img.width as i32,
            bottom: img.height as i32,
            stride: img.width as i32,
            width: img.width as i32,
            height: img.height as i32,
            blendfunc: 0,
            clrcolor: 0,
            rot: 0,
            global_alpha: 0,
        })
    }
}

impl TryFrom<&Image> for g2d_surface_new {
    type Error = std::io::Error;

    fn try_from(img: &Image) -> Result<Self, Self::Error> {
        let to_fd = img.fd.try_clone()?;
        let to_phys: G2DPhysical = DmaBuf::from(to_fd).into();
        Ok(Self {
            planes: [to_phys.into(), 0, 0],
            format: G2DFormat::from(img.format).format(),
            left: 0,
            top: 0,
            right: img.width as i32,
            bottom: img.height as i32,
            stride: img.width as i32,
            width: img.width as i32,
            height: img.height as i32,
            blendfunc: 0,
            clrcolor: 0,
            rot: 0,
            global_alpha: 0,
        })
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

/// Memory-mapped view of an `Image` buffer.
///
/// Provides CPU-accessible view of a DMA image buffer through memory mapping.
/// The mapping is automatically unmapped when dropped.
///
/// # Safety
///
/// While the API is safe, concurrent access from hardware and CPU can lead to
/// race conditions. Ensure hardware operations complete before CPU access.
pub struct MappedImage {
    mmap: *mut u8,
    len: usize,
}

impl MappedImage {
    pub fn as_slice(&self) -> &[u8] {
        unsafe { from_raw_parts(self.mmap, self.len) }
    }

    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        unsafe { from_raw_parts_mut(self.mmap, self.len) }
    }
}
impl Drop for MappedImage {
    fn drop(&mut self) {
        if unsafe { munmap(self.mmap.cast::<c_void>(), self.len) } > 0 {
            warn!("unmap failed!");
        }
    }
}

/// Encodes an RGBA image to JPEG format using turbojpeg.
///
/// Uses the turbojpeg library with SIMD optimizations for fast JPEG
/// compression.
///
/// # Arguments
///
/// * `pix` - Raw RGBA pixel data
/// * `img` - Image metadata (dimensions and format)
///
/// # Returns
///
/// JPEG-compressed image as an owned buffer.
///
/// # Errors
///
/// Returns an error if:
/// - Image metadata is not provided
/// - JPEG compression fails
/// - Invalid pixel data or dimensions
///
/// # Example
///
/// ```no_run
/// use edgefirst_camera::image::{encode_jpeg, Image, RGBA};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mut img = Image::new(640, 480, RGBA)?;
/// let mut mapped = img.mmap();
/// let jpeg = encode_jpeg(mapped.as_slice(), Some(&img))?;
/// println!("Compressed to {} bytes", jpeg.len());
/// # Ok(())
/// # }
/// ```
pub fn encode_jpeg(pix: &[u8], img: Option<&Image>) -> Result<OwnedBuf, Box<dyn Error>> {
    let img2 = match img {
        Some(img) => turbojpeg::Image {
            width: img.width() as usize,
            height: img.height() as usize,
            format: turbojpeg::PixelFormat::RGBA,
            pixels: pix,
            pitch: img.width() as usize * 4,
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

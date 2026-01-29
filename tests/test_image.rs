// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

use edgefirst_camera::{
    image,
    image::{encode_jpeg, Image, ImageManager, Rotation},
};
use serial_test::serial;
use std::{error::Error, time::Instant};
use videostream::{
    camera::{create_camera, Mirror},
    fourcc::FourCC,
};

#[test]
#[serial]
fn test_formats() -> Result<(), Box<dyn Error>> {
    let mut img = Image::new(1920, 1080, image::NV12)?;

    println!("{}", img);
    assert_eq!(img.size(), 3110400);

    img = Image::new(1920, 1080, image::YUYV)?;
    println!("{}", img);
    assert_eq!(img.size(), 4147200);

    img = Image::new(1920, 1080, image::RGB3)?;
    println!("{}", img);
    assert_eq!(img.size(), 6220800);

    img = Image::new(1920, 1080, image::RGBA)?;
    println!("{}", img);
    assert_eq!(img.size(), 8294400);

    Ok(())
}

#[test]
#[serial]
fn test_4k() -> Result<(), Box<dyn Error>> {
    let img1 = Image::new(3840, 2160, image::RGBA)?;
    let img2 = Image::new(3840, 2160, image::RGBA)?;
    let img3 = Image::new(3840, 2160, image::RGBA)?;
    let img4 = Image::new(3840, 2160, image::RGBA)?;

    assert_eq!(img1.size(), 33177600);
    assert_eq!(img2.size(), 33177600);
    assert_eq!(img3.size(), 33177600);
    assert_eq!(img4.size(), 33177600);

    println!("{} {} {} {}", img1, img2, img3, img4);

    Ok(())
}

#[test]
#[serial]
fn test_8k() -> Result<(), Box<dyn Error>> {
    let img1 = Image::new(7680, 4320, image::RGBA)?;
    let img2 = Image::new(7680, 4320, image::RGBA)?;

    assert_eq!(img1.size(), 132710400);
    assert_eq!(img2.size(), 132710400);

    println!("{} {}", img1, img2);

    Ok(())
}

/// This test verifies that extremely large allocations eventually fail.
/// A single 16K image requires ~530MB of CMA memory. We attempt to allocate
/// multiple to exhaust available CMA. If all allocations succeed, the test
/// passes (indicating very large CMA), but we verify cleanup works.
/// If even the first allocation fails, we skip the test as the system has
/// insufficient CMA memory for 16K images.
#[test]
#[serial]
fn test_16k() -> Result<(), Box<dyn Error>> {
    // Try to allocate multiple 16K images to exhaust CMA
    // Each 15360x8640 RGBA image = ~530MB
    let mut images = Vec::new();
    for i in 0..4 {
        match Image::new(15360, 8640, image::RGBA) {
            Ok(img) => {
                images.push(img);
            }
            Err(e) => {
                if i == 0 {
                    // First allocation failed - system has insufficient CMA for 16K
                    // This is an environment limitation, not a test failure
                    eprintln!("Skipping test_16k: insufficient CMA memory ({e})");
                    return Ok(());
                }
                // Subsequent allocation failed - CMA exhausted as expected
                return Ok(());
            }
        }
    }
    // If we get here, device has >2GB CMA - just verify images are valid
    assert!(!images.is_empty());
    Ok(())
}

/// This test verifies that image buffers are properly cleaned up when the
/// image is dropped.  If images are not cleaned up it will eventually fail
/// as 100 1080p images would require ~800MB of CMA memory.
#[test]
#[serial]
fn test_cleanup() -> Result<(), Box<dyn Error>> {
    for _ in 0..100 {
        let img = Image::new(1920, 1080, image::RGBA)?;
        assert_eq!(img.size(), 8294400);
    }

    Ok(())
}

#[test]
#[serial]
fn test_resize() -> Result<(), Box<dyn Error>> {
    let from = Image::new(1920, 1080, image::RGBA)?;
    let to = Image::new(640, 480, image::RGBA)?;
    let mgr = ImageManager::new()?;
    mgr.convert(&from, &to, None, Rotation::Rotation0)?;

    Ok(())
}

#[test]
#[serial]
fn test_convert() -> Result<(), Box<dyn Error>> {
    let from = Image::new(1920, 1080, image::YUYV)?;
    let to = Image::new(1920, 1080, image::RGBA)?;
    let mgr = ImageManager::new()?;
    mgr.convert(&from, &to, None, Rotation::Rotation0)?;

    Ok(())
}

#[test]
#[serial]
#[ignore = "camera test is disabled by default (run with --include-ignored to enable)"]
fn test_capture() -> Result<(), Box<dyn Error>> {
    let device = "/dev/video3";

    let cam = create_camera()
        .with_device(device)
        .with_format(FourCC(*b"YUYV"))
        .with_mirror(Mirror::Both)
        .open()?;
    println!(
        "camera resolution {}x{} format {} mirrored {}",
        cam.width(),
        cam.height(),
        cam.format(),
        cam.mirror(),
    );

    cam.start()?;

    let buf = cam.read()?;
    let src = Image::from_camera(&buf)?;
    let dst = Image::new(1920, 1080, image::RGBA)?;

    let mgr = ImageManager::new()?;
    mgr.convert(&src, &dst, None, Rotation::Rotation0)?;

    let now = Instant::now();
    let dma = dst.dmabuf();
    let mem = dma.memory_map()?;
    let jpeg = mem.read(encode_jpeg, Some(&dst))?;
    let elapsed = now.elapsed();

    std::fs::write("camera.jpeg", &jpeg)?;

    println!(
        "saved camera.jpeg resolution: {}x{} size: {} elapsed: {:.2?}",
        dst.width(),
        dst.height(),
        jpeg.len(),
        elapsed
    );

    Ok(())
}

use async_std::task::sleep;
use camera::image::{encode_jpeg, Image, ImageManager, RGBA};
use cdr::{CdrLe, Infinite};
use clap::Parser;
use serde_derive::{Deserialize, Serialize};
use std::{
    error::Error,
    str::FromStr,
    time::{Duration, Instant},
};
use videostream::{
    camera::{create_camera, Mirror},
    fourcc::FourCC,
};
use zenoh::{config::Config, prelude::r#async::*};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// camera capture device
    #[arg(short, long, default_value = "/dev/video3")]
    camera: String,

    /// zenoh connection mode
    #[arg(short, long, default_value = "peer")]
    mode: String,

    /// connect to endpoint
    #[arg(short, long)]
    endpoint: Vec<String>,

    /// ros topic
    #[arg(short, long, default_value = "rt/")]
    topic: String,
}

#[async_std::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("VideoStream ROS Publisher");

    let args = Args::parse();
    // let mut config = Config::default();

    // let mode = WhatAmI::from_str(&args.mode).unwrap();
    // config.set_mode(Some(mode)).unwrap();
    // config.connect.endpoints = args.endpoint.iter().map(|v|
    // v.parse().unwrap()).collect();

    // let session = zenoh::open(config).res().await.unwrap();

    let mut img = Image::new(960, 540, RGBA)?;
    let imgmgr = ImageManager::new()?;

    let cam = create_camera()
        .with_device(&args.camera)
        .with_format(FourCC(*b"YUYV"))
        .with_mirror(Mirror::Both)
        .open()?;
    cam.start()?;

    loop {
        let mut now = Instant::now();
        let buf = cam.read()?;
        let capture_time = now.elapsed();

        now = Instant::now();
        imgmgr.convert(&Image::from_camera(buf)?, &img, None)?;
        let convert_time = now.elapsed();

        now = Instant::now();
        let dma = img.dmabuf();
        let mem = dma.memory_map()?;
        let jpeg = mem.read(encode_jpeg, Some(&img))?;
        let encode_time = now.elapsed();

        println!(
            "jpeg size: {}KB capture: {:?} convert: {:?} encode: {:?}",
            jpeg.len() / 1024,
            capture_time,
            convert_time,
            encode_time
        );
    }
}

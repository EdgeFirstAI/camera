// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! End-to-end runs of the `edgefirst-camera` binary on a desktop camera,
//! named by `EDGEFIRST_CAMERA_SOURCE` (see `common::source`), for example
//! `mediafoundation` for the first Media Foundation camera. The run
//! subscribes over Zenoh as a downstream service would. `on_target.rs` is
//! the Linux counterpart.
//!
//! - `EDGEFIRST_CAMERA_SIZE` and `EDGEFIRST_CAMERA_FPS`: the mode requested,
//!   default 1920x1080 at 30. The camera may deliver another, which the test
//!   reads from the published frames.

#![cfg(not(target_os = "linux"))]

use edgefirst_schemas::{edgefirst_msgs::CameraFrame, sensor_msgs::CompressedImage};
use std::{
    error::Error,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use zenoh::Wait as _;

mod common;

use common::{camera_bin, free_endpoint, stop, subscriber_session};

type TestResult = Result<(), Box<dyn Error + Send + Sync>>;

/// From start-up to the first frame.
const FIRST_SAMPLE: Duration = Duration::from_secs(20);
/// How long the rate is measured, from the first frame.
const WINDOW: Duration = Duration::from_secs(5);

/// Kills the binary if the test fails before stopping it.
struct Running(Option<Child>);

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
#[ignore = "requires a camera (set EDGEFIRST_CAMERA_SOURCE and run with --include-ignored)"]
fn publishes_frames_and_jpeg_from_the_camera() -> TestResult {
    let Some(source) = common::source() else {
        return Ok(());
    };
    let (width, height) = common::camera_size();
    let fps = common::camera_fps();
    let endpoint = free_endpoint()?;
    let prefix = format!("edgefirst-camera-desktop-{}", std::process::id());
    let (frame_topic, jpeg_topic) = (format!("{prefix}/frame"), format!("{prefix}/jpeg"));
    let mut child = Running(Some(
        Command::new(camera_bin())
            .args([
                "--source",
                &source,
                "--camera-size",
                &width.to_string(),
                &height.to_string(),
                "--camera-fps",
                &fps.to_string(),
                "--stream-size",
                &width.to_string(),
                &height.to_string(),
                "--h264",
                "false",
                "--jpeg",
                "--frame-topic",
                &frame_topic,
                "--jpeg-topic",
                &jpeg_topic,
                "--listen",
                &endpoint,
                "--no-multicast-scouting",
            ])
            .stdin(Stdio::null())
            .spawn()?,
    ));

    type Received = (Vec<CameraFrame<Vec<u8>>>, Vec<(u32, u32)>);
    let received = (|| -> Result<Received, Box<dyn Error + Send + Sync>> {
        let session = subscriber_session(&endpoint)?;
        let frames = session
            .declare_subscriber(format!("**/{frame_topic}"))
            .wait()?;
        let jpegs = session
            .declare_subscriber(format!("**/{jpeg_topic}"))
            .wait()?;
        let started = Instant::now();
        let mut first = None;
        let mut got = Vec::new();
        let mut jpeg = Vec::new();
        loop {
            let now = Instant::now();
            match first {
                None if now - started > FIRST_SAMPLE => {
                    return Err(format!("no frame within {FIRST_SAMPLE:?}").into())
                }
                Some(t) if now - t > WINDOW => break,
                _ => {}
            }
            while let Ok(Some(sample)) = jpegs.try_recv() {
                let image = CompressedImage::from_cdr(sample.payload().to_bytes().into_owned())?;
                let header = turbojpeg::read_header(image.data())?;
                jpeg.push((header.width as u32, header.height as u32));
            }
            if let Ok(Some(sample)) = frames.recv_timeout(Duration::from_millis(100)) {
                first.get_or_insert(Instant::now());
                got.push(CameraFrame::from_cdr(
                    sample.payload().to_bytes().into_owned(),
                )?);
            }
        }
        Ok((got, jpeg))
    })();
    let stopped = stop(child.0.take().expect("the binary is running"));
    let (frames, jpeg) = received?;
    stopped?;

    let rate = (frames.len().saturating_sub(1)) as f64 / WINDOW.as_secs_f64();
    eprintln!(
        "{} frames ({rate:.1} fps) and {} JPEG images in {WINDOW:?}",
        frames.len(),
        jpeg.len()
    );
    let seqs: Vec<_> = frames.iter().map(CameraFrame::seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "out of order: {seqs:?}"
    );
    let shape: Vec<_> = frames[0].tensor().shape().collect();
    for frame in &frames {
        let tensor = frame.tensor();
        assert_eq!(tensor.shape().collect::<Vec<_>>(), shape);
        for plane in tensor.planes() {
            assert!(plane.is_inline(), "plane sent by reference: {plane:?}");
            assert!(!plane.data.is_empty(), "inline plane without data");
        }
    }
    assert!(!jpeg.is_empty(), "no JPEG image in {WINDOW:?}");
    assert!(
        jpeg.iter().all(|&size| size == (width, height)),
        "JPEG sizes {jpeg:?}, not {width}x{height}"
    );
    // Delivered rates vary with exposure on a desktop camera; a stream far
    // below a frame a second has stalled.
    assert!(rate >= 1.0, "{rate:.2} fps");
    Ok(())
}

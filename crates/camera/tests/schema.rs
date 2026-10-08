// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Every frame the mock backend produces maps to a `CameraFrame` that
//! passes `TensorFields::validate` and decodes to the frame it came from.
#![cfg(all(feature = "mock", feature = "schemas"))]

use std::time::Duration;

use edgefirst_camera::schema::{self, FrameTensor};
use edgefirst_camera::{CameraBuilder, Frame};
use edgefirst_schemas::edgefirst_msgs::CameraFrame;
use edgefirst_tensor::{CpuAccess, PixelFormat};

const FORMATS: [PixelFormat; 4] = [
    PixelFormat::Yuyv,
    PixelFormat::Nv12,
    PixelFormat::Rgba,
    PixelFormat::Grey,
];

/// Even sizes from the mock's minimum up, including widths that are not a
/// multiple of 4, 16 or 64.
const SIZES: [(u32, u32); 9] = [
    (16, 16),
    (18, 16),
    (16, 18),
    (34, 22),
    (322, 242),
    (640, 480),
    (1282, 722),
    (1920, 1080),
    (2050, 1538),
];

fn frame(
    format: PixelFormat,
    (w, h): (u32, u32),
) -> (Frame, Option<edgefirst_tensor::Colorimetry>) {
    let mut camera = CameraBuilder::source("mock")
        .unwrap()
        .size(w, h)
        .format(format)
        .open()
        .unwrap();
    camera.start().unwrap();
    let f = camera.next_frame(Some(Duration::from_secs(1))).unwrap();
    (f, camera.config().colorimetry)
}

#[test]
fn every_mock_frame_maps_to_a_valid_camera_frame() {
    for format in FORMATS {
        for size in SIZES {
            let what = format!("{format:?} {}x{}", size.0, size.1);
            let (frame, colorimetry) = frame(format, size);
            let tensor = FrameTensor::new(&frame, colorimetry.as_ref()).unwrap();
            let stamp = schema::stamp(&frame).unwrap_or_else(schema::now);
            let mut cdr = Vec::new();
            tensor
                .with_fields(|f| {
                    f.validate()?;
                    CameraFrame::builder()
                        .stamp(stamp)
                        .frame_id("camera")
                        .seq(frame.seq())
                        .tensor(f)
                        .encode_into_vec(&mut cdr)
                })
                .unwrap_or_else(|e| panic!("{what}: {e:?}"));

            let msg = CameraFrame::from_cdr(cdr.as_slice()).unwrap();
            assert_eq!(msg.seq(), frame.seq(), "{what}");
            assert_eq!(msg.stamp(), stamp, "{what}");
            let t = msg.tensor();
            assert_eq!(t.format(), format.to_string(), "{what}");
            assert_eq!(t.pid(), std::process::id(), "{what}");
            let shape: Vec<u64> = t.shape().collect();
            let expected: Vec<u64> = format
                .addressing_shape(size.0 as usize, size.1 as usize)
                .unwrap()
                .into_iter()
                .map(|d| d as u64)
                .collect();
            assert_eq!(shape, expected, "{what}");
            assert_eq!(t.strides().count(), shape.len(), "{what}");

            // Mock frames live in process memory, so every plane travels
            // inline and holds exactly the frame's bytes.
            let bytes = frame.map_bytes(CpuAccess::Read).unwrap();
            assert_eq!(t.num_planes() as usize, frame.planes().len(), "{what}");
            for (p, l) in t.planes().zip(frame.planes()) {
                assert!(p.is_inline(), "{what}");
                assert_eq!(p.stride, l.stride as u64, "{what}");
                assert_eq!(p.data, &bytes[l.offset..l.offset + l.size], "{what}");
            }
        }
    }
}

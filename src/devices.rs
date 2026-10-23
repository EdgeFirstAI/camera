// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The `devices` and `info` subcommands.
//!
//! JSON names pixel formats by their HAL wire names (`YUYV`, `rgba8`), as
//! `CameraFrame.format` does; text uses the display names.

use std::error::Error;

use edgefirst_camera::{modes, Backend, CameraDescriptor, FormatInfo, Mode, Rates, Sizes};
use edgefirst_codec::{
    video::{DecoderConfig, EncoderConfig, VideoDecoder, VideoEncoder},
    CodecError,
};
use edgefirst_tensor::PixelFormat;
use serde::Serialize;
use tracing::warn;

use crate::{
    args::{DevicesArgs, InfoArgs},
    exit::Failure,
};

/// Encoder inputs `info` asks about: the capture format and the formats
/// the service converts to.
const ENCODER_INPUTS: [PixelFormat; 4] = [
    PixelFormat::Yuyv,
    PixelFormat::Nv12,
    PixelFormat::Rgb,
    PixelFormat::Rgba,
];

#[derive(Debug, Serialize)]
struct Devices {
    cameras: Vec<Device>,
}

#[derive(Debug, Serialize)]
struct Device {
    backend: &'static str,
    id: String,
    name: String,
    driver: String,
    formats: Vec<Format>,
    /// Why the probe failed; the formats are then enumeration's.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct Format {
    format: &'static str,
    #[serde(skip)]
    display: String,
    sizes: SizesOut,
    /// Named modes (`1080p30`) at each size's highest rate.
    modes: Vec<String>,
}

/// Discrete sizes as a list, a stepwise range as an object.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum SizesOut {
    Discrete(Vec<SizeOut>),
    Stepwise {
        min: [u32; 2],
        max: [u32; 2],
        step: [u32; 2],
        fps: Option<RatesOut>,
    },
}

#[derive(Debug, Serialize)]
struct SizeOut {
    width: u32,
    height: u32,
    /// `null` when the camera does not report its rates.
    fps: Option<RatesOut>,
}

/// Discrete rates as a list, a continuous range as an object.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum RatesOut {
    Discrete(Vec<f64>),
    Range { min: f64, max: f64 },
}

/// The `devices` subcommand.
pub(crate) fn devices(args: &DevicesArgs) -> Result<(), Box<dyn Error>> {
    let cameras = edgefirst_camera::enumerate()
        .map_err(|e| Failure::of(&e, format!("cannot list cameras: {e}")))?;
    let devices = Devices {
        cameras: cameras.iter().map(describe).collect(),
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&devices)?);
    } else if devices.cameras.is_empty() {
        println!("No cameras found");
    } else {
        for device in &devices.cameras {
            print_device(device);
        }
    }
    Ok(())
}

/// `camera` with its probed formats, or its enumerated ones when the probe
/// fails (a camera another process streams from, for example).
fn describe(camera: &CameraDescriptor) -> Device {
    let (formats, error) = match edgefirst_camera::probe(camera) {
        Ok(formats) => (formats, None),
        Err(e) => {
            warn!("cannot probe {}: {e}", camera.id);
            (camera.formats.clone(), Some(e.to_string()))
        }
    };
    Device {
        backend: backend_name(camera.backend),
        id: camera.id.clone(),
        name: camera.name.clone(),
        driver: camera.driver.clone(),
        formats: formats.iter().map(|f| format_out(f, &formats)).collect(),
        error,
    }
}

fn format_out(info: &FormatInfo, all: &[FormatInfo]) -> Format {
    let sizes = match &info.sizes {
        Sizes::Discrete(sizes) => SizesOut::Discrete(
            sizes
                .iter()
                .map(|s| SizeOut {
                    width: s.width,
                    height: s.height,
                    fps: rates_out(&s.rates),
                })
                .collect(),
        ),
        Sizes::Stepwise {
            min,
            max,
            step,
            rates,
        } => SizesOut::Stepwise {
            min: [min.0, min.1],
            max: [max.0, max.1],
            step: [step.0, step.1],
            fps: rates_out(rates),
        },
    };
    Format {
        format: info.format.as_str(),
        display: info.format.to_string(),
        sizes,
        modes: modes(all, info.format).iter().map(Mode::name).collect(),
    }
}

fn rates_out(rates: &Rates) -> Option<RatesOut> {
    match rates {
        Rates::Discrete(fps) => Some(RatesOut::Discrete(fps.clone())),
        Rates::Range { min, max } => Some(RatesOut::Range {
            min: *min,
            max: *max,
        }),
        Rates::Unknown => None,
    }
}

fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::V4l2 => "v4l2",
        Backend::Libcamera => "libcamera",
        Backend::File => "file",
        Backend::Mock => "mock",
        _ => "other",
    }
}

fn print_device(device: &Device) {
    println!(
        "{}  {} ({}, {})",
        device.id, device.name, device.driver, device.backend
    );
    if let Some(e) = &device.error {
        println!("  probe failed, showing enumerated formats: {e}");
    }
    for format in &device.formats {
        println!("  {}", format.display);
        match &format.sizes {
            SizesOut::Discrete(sizes) => {
                for s in sizes {
                    println!("    {}x{}  {}", s.width, s.height, rates_text(&s.fps));
                }
            }
            SizesOut::Stepwise {
                min,
                max,
                step,
                fps,
            } => println!(
                "    {}x{} to {}x{}, step {}x{}  {}",
                min[0],
                min[1],
                max[0],
                max[1],
                step[0],
                step[1],
                rates_text(fps)
            ),
        }
        if !format.modes.is_empty() {
            println!("    modes: {}", format.modes.join(" "));
        }
    }
}

fn rates_text(rates: &Option<RatesOut>) -> String {
    match rates {
        Some(RatesOut::Discrete(fps)) => {
            let list: Vec<String> = fps.iter().map(f64::to_string).collect();
            format!("{} fps", list.join(", "))
        }
        Some(RatesOut::Range { min, max }) => format!("{min} to {max} fps"),
        None => "rate unknown".into(),
    }
}

#[derive(Debug, Serialize)]
struct Info {
    version: &'static str,
    cameras: Vec<CameraSummary>,
    encoder: Codec,
    decoder: Codec,
}

#[derive(Debug, Serialize)]
struct CameraSummary {
    backend: &'static str,
    id: String,
    name: String,
    driver: String,
}

#[derive(Debug, Default, Serialize)]
struct Codec {
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    device: Option<String>,
    /// Input formats the encoder takes, of those the service feeds it.
    #[serde(skip_serializing_if = "Option::is_none")]
    inputs: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The `info` subcommand. Reports what is missing rather than failing.
pub(crate) fn info(args: &InfoArgs) -> Result<(), Box<dyn Error>> {
    let (cameras, camera_error) = match edgefirst_camera::enumerate() {
        Ok(cameras) => (cameras, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    let info = Info {
        version: env!("CARGO_PKG_VERSION"),
        cameras: cameras
            .iter()
            .map(|c| CameraSummary {
                backend: backend_name(c.backend),
                id: c.id.clone(),
                name: c.name.clone(),
                driver: c.driver.clone(),
            })
            .collect(),
        encoder: encoder(),
        decoder: decoder(),
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }
    println!("edgefirst-camera {}", info.version);
    println!("Cameras:");
    if let Some(e) = camera_error {
        println!("  cannot list cameras: {e}");
    } else if info.cameras.is_empty() {
        println!("  none");
    }
    for c in &info.cameras {
        println!("  {}  {} ({}, {})", c.id, c.name, c.driver, c.backend);
    }
    println!("H.264 encoder: {}", codec_text(&info.encoder));
    println!("H.264 decoder: {}", codec_text(&info.decoder));
    Ok(())
}

fn encoder() -> Codec {
    let mut codec = Codec::default();
    let mut inputs = Vec::new();
    for format in ENCODER_INPUTS {
        match VideoEncoder::new(EncoderConfig::h264(640, 480, format, 30.0)) {
            Ok(encoder) => {
                codec.device.get_or_insert_with(|| encoder.device());
                inputs.push(format.as_str());
            }
            Err(CodecError::UnsupportedFormat(_)) => {}
            Err(e) => {
                codec.error = Some(e.to_string());
                if matches!(e, CodecError::NoDevice(_)) {
                    break;
                }
            }
        }
    }
    codec.available = !inputs.is_empty();
    if codec.available {
        codec.error = None;
        codec.inputs = Some(inputs);
    }
    codec
}

fn decoder() -> Codec {
    match VideoDecoder::new(DecoderConfig::h264()) {
        Ok(decoder) => Codec {
            available: true,
            device: Some(decoder.device()),
            ..Codec::default()
        },
        Err(e) => Codec {
            error: Some(e.to_string()),
            ..Codec::default()
        },
    }
}

fn codec_text(codec: &Codec) -> String {
    if !codec.available {
        return format!("none ({})", codec.error.as_deref().unwrap_or("not found"));
    }
    let device = codec.device.as_deref().unwrap_or("unknown device");
    match &codec.inputs {
        Some(inputs) => format!("{device}, inputs {}", inputs.join(", ")),
        None => device.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgefirst_camera::SizeRates;

    fn ov5640_like() -> Vec<FormatInfo> {
        vec![FormatInfo {
            format: PixelFormat::Yuyv,
            sizes: Sizes::Discrete(vec![
                SizeRates {
                    width: 640,
                    height: 480,
                    rates: Rates::Discrete(vec![60.0, 30.0]),
                },
                SizeRates {
                    width: 1920,
                    height: 1080,
                    rates: Rates::Discrete(vec![30.0, 15.0]),
                },
                SizeRates {
                    width: 2592,
                    height: 1944,
                    rates: Rates::Unknown,
                },
            ]),
        }]
    }

    #[test]
    fn discrete_formats_list_sizes_rates_and_modes() {
        let formats = ov5640_like();
        let json = serde_json::to_value(format_out(&formats[0], &formats)).unwrap();
        assert_eq!(json["format"], "YUYV");
        assert_eq!(json["modes"], serde_json::json!(["VGA60", "1080p30"]));
        assert_eq!(
            json["sizes"][0],
            serde_json::json!({"width": 640, "height": 480, "fps": [60.0, 30.0]})
        );
        assert!(json["sizes"][2]["fps"].is_null());
    }

    #[test]
    fn stepwise_formats_give_a_range_and_the_named_sizes_in_it() {
        let formats = vec![FormatInfo {
            format: PixelFormat::Nv12,
            sizes: Sizes::Stepwise {
                min: (176, 144),
                max: (1920, 1080),
                step: (16, 8),
                rates: Rates::Range {
                    min: 1.0,
                    max: 30.0,
                },
            },
        }];
        let json = serde_json::to_value(format_out(&formats[0], &formats)).unwrap();
        assert_eq!(json["sizes"]["min"], serde_json::json!([176, 144]));
        assert_eq!(
            json["sizes"]["fps"],
            serde_json::json!({"min": 1.0, "max": 30.0})
        );
        let modes = json["modes"].as_array().unwrap();
        assert!(modes.contains(&"1080p30".into()), "{modes:?}");
    }

    #[test]
    fn rates_read_naturally() {
        assert_eq!(
            rates_text(&Some(RatesOut::Discrete(vec![30.0, 29.97]))),
            "30, 29.97 fps"
        );
        assert_eq!(rates_text(&None), "rate unknown");
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The `convert` subcommand: a recording's Annex-B H.264 into an MP4
//! container, in software.
//!
//! Each access unit becomes one sample with its NAL units length-prefixed
//! (ISO/IEC 14496-15 `avc1`). The first SPS and PPS go into the sample
//! entry and are left out of the samples, as are access unit delimiters.
//! Samples are in presentation order: the encoders this service uses
//! produce no B-frames.

use std::{
    error::Error,
    fs::File,
    io::{BufWriter, Write as _},
    path::Path,
};

use edgefirst_camera::Sidecar;
use edgefirst_codec::video::access_units;
use h264_reader::{
    nal::{sps::SeqParameterSet, NalHeader, UnitType},
    rbsp::{decode_nal, BitReader},
};
use mp4::{AvcConfig, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};
use tracing::{info, warn};

use crate::{
    args::ConvertArgs,
    exit::{Failure, Status},
};

/// The rate used when neither `--fps` nor a sidecar gives one.
const DEFAULT_FPS: f64 = 30.0;

/// Track timescale: the 90 kHz clock of MPEG video, which keeps common
/// rates, 29.97 included, to whole ticks.
const TIMESCALE: u32 = 90_000;

/// A recording's pictures, ready for the container.
#[derive(Debug)]
struct Track {
    sps: Vec<u8>,
    pps: Vec<u8>,
    width: u32,
    height: u32,
    samples: Vec<Sample>,
}

#[derive(Debug)]
struct Sample {
    /// NAL units, each after its 4-byte big-endian length.
    data: Vec<u8>,
    /// Holds an IDR picture.
    sync: bool,
}

/// The `convert` subcommand.
pub(crate) fn convert(args: &ConvertArgs) -> Result<(), Box<dyn Error>> {
    let input = &args.input;
    let stream = std::fs::read(input).map_err(|e| {
        Failure::new(
            Status::InvalidArgs,
            format!("cannot read {}: {e}", input.display()),
        )
    })?;
    let fps = args.fps.or_else(|| sidecar_fps(input)).unwrap_or_else(|| {
        warn!("no --fps and no sidecar rate; assuming {DEFAULT_FPS} fps");
        DEFAULT_FPS
    });
    let track = track(&stream).map_err(|e| format!("{}: {e}", input.display()))?;
    write_mp4(&args.output, &track, fps)?;
    println!(
        "Wrote {}: {} frames, {}x{} at {fps} fps",
        args.output.display(),
        track.samples.len(),
        track.width,
        track.height
    );
    Ok(())
}

/// The rate in the recording's sidecar, when it has one.
fn sidecar_fps(stream: &Path) -> Option<f64> {
    let path = Sidecar::paired_path(stream);
    if !path.exists() {
        return None;
    }
    match Sidecar::load(stream) {
        Ok(sidecar) => {
            info!("{} fps from {}", sidecar.fps, path.display());
            Some(sidecar.fps)
        }
        Err(e) => {
            warn!("ignoring {}: {e}", path.display());
            None
        }
    }
}

/// Splits `stream` into samples and finds its parameter sets and size.
fn track(stream: &[u8]) -> Result<Track, Box<dyn Error>> {
    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;
    let mut samples = Vec::new();
    let mut leading = 0;
    let mut changed = false;
    for au in access_units(stream) {
        let mut sample = Sample {
            data: Vec::with_capacity(au.len()),
            sync: false,
        };
        let mut picture = false;
        for nal in nal_units(au) {
            let kind = NalHeader::new(nal[0])
                .map_err(|e| format!("invalid NAL unit header: {e:?}"))?
                .nal_unit_type();
            match kind {
                UnitType::SeqParameterSet | UnitType::PicParameterSet => {
                    let set = if kind == UnitType::SeqParameterSet {
                        &mut sps
                    } else {
                        &mut pps
                    };
                    match set {
                        Some(first) => changed |= first.as_slice() != nal,
                        None => *set = Some(nal.to_vec()),
                    }
                }
                UnitType::AccessUnitDelimiter => {}
                _ => {
                    picture |= matches!(
                        kind,
                        UnitType::SliceLayerWithoutPartitioningNonIdr
                            | UnitType::SliceLayerWithoutPartitioningIdr
                    );
                    sample.sync |= kind == UnitType::SliceLayerWithoutPartitioningIdr;
                    let len = u32::try_from(nal.len())?;
                    sample.data.extend_from_slice(&len.to_be_bytes());
                    sample.data.extend_from_slice(nal);
                }
            }
        }
        if !picture {
            continue;
        }
        // A player starts at the first sync sample; pictures before it
        // reference frames that are not in the file.
        if samples.is_empty() && !sample.sync {
            leading += 1;
            continue;
        }
        samples.push(sample);
    }
    if leading > 0 {
        warn!("dropped {leading} frames before the first IDR picture");
    }
    if changed {
        warn!("the parameter sets change within the stream; the MP4 keeps the first");
    }
    let (Some(sps), Some(pps)) = (sps, pps) else {
        return Err("no SPS and PPS: not an H.264 Annex-B stream".into());
    };
    if samples.is_empty() {
        return Err("no IDR picture".into());
    }
    let rbsp = decode_nal(&sps)?;
    let (width, height) = SeqParameterSet::from_bits(BitReader::new(&rbsp[..]))
        .and_then(|s| s.pixel_dimensions())
        .map_err(|e| format!("cannot read the SPS: {e:?}"))?;
    Ok(Track {
        sps,
        pps,
        width,
        height,
        samples,
    })
}

/// The NAL units of an Annex-B buffer, without their start codes.
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &start)| {
            // Zero bytes before the next start code belong to it (a 4-byte
            // code) or are trailing_zero_8bits; a NAL unit never ends in 0.
            let mut end = starts.get(k + 1).map_or(data.len(), |&next| next - 3);
            while end > start && data[end - 1] == 0 {
                end -= 1;
            }
            &data[start..end]
        })
        .filter(|nal| !nal.is_empty())
        .collect()
}

/// When sample `i` starts, in [`TIMESCALE`] ticks. Computed per sample, so
/// a rate that is not a whole number of ticks does not drift.
fn start_time(i: usize, fps: f64) -> u64 {
    (i as f64 * f64::from(TIMESCALE) / fps).round() as u64
}

fn write_mp4(path: &Path, track: &Track, fps: f64) -> Result<(), Box<dyn Error>> {
    let file = File::create(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    let brands = ["isom", "iso2", "avc1", "mp41"]
        .iter()
        .map(|b| b.parse())
        .collect::<Result<Vec<_>, _>>()?;
    let config = Mp4Config {
        major_brand: "isom".parse()?,
        minor_version: 512,
        compatible_brands: brands,
        timescale: 1000,
    };
    let mut writer = Mp4Writer::write_start(BufWriter::new(file), &config)?;
    writer.add_track(&TrackConfig {
        track_type: TrackType::Video,
        timescale: TIMESCALE,
        language: "und".into(),
        media_conf: MediaConfig::AvcConfig(AvcConfig {
            width: u16::try_from(track.width)?,
            height: u16::try_from(track.height)?,
            seq_param_set: track.sps.clone(),
            pic_param_set: track.pps.clone(),
        }),
    })?;
    for (i, sample) in track.samples.iter().enumerate() {
        let start = start_time(i, fps);
        writer.write_sample(
            1,
            &Mp4Sample {
                start_time: start,
                duration: u32::try_from(start_time(i + 1, fps) - start)?,
                rendering_offset: 0,
                is_sync: sample.sync,
                bytes: mp4::Bytes::from(sample.data.clone()),
            },
        )?;
    }
    writer.write_end()?;
    writer.into_writer().flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 30 frames of 160x120 at 30 fps from x264: baseline profile, two
    /// slices per picture, an IDR every 10 frames, Annex-B with AUDs.
    const CLIP: &[u8] = include_bytes!("../tests/data/testsrc-160x120.h264");

    #[test]
    fn splits_nal_units_at_3_and_4_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4,
        ];
        let nals = nal_units(&data);
        assert_eq!(nals, [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4]]);
    }

    #[test]
    fn one_sample_per_picture_with_its_slices() {
        let track = track(CLIP).unwrap();
        assert_eq!((track.width, track.height), (160, 120));
        assert_eq!(track.samples.len(), 30);
        let syncs: Vec<_> = track.samples.iter().map(|s| s.sync).collect();
        assert_eq!(syncs.iter().filter(|&&s| s).count(), 3, "{syncs:?}");
        assert!(syncs[0] && syncs[10] && syncs[20]);
        // x264's SEI and two slices, length-prefixed; no parameter sets or
        // AUD.
        assert_eq!(nal_types(&track.samples[0].data), [6, 5, 5]);
        assert_eq!(nal_types(&track.samples[1].data), [1, 1]);
    }

    /// The `nal_unit_type` of each NAL unit in a length-prefixed sample.
    fn nal_types(mut data: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        while data.len() > 4 {
            let len = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
            types.push(data[4] & 0x1f);
            data = &data[4 + len..];
        }
        assert!(data.is_empty(), "trailing bytes");
        types
    }

    #[test]
    fn rates_keep_whole_ticks_without_drift() {
        assert_eq!(start_time(30, 30.0), 90_000);
        assert_eq!(start_time(1, 29.97), 3003);
        assert_eq!(start_time(30_000, 29.97), 90_090_090);
    }

    #[test]
    fn writes_an_mp4_that_reads_back() {
        let dir = std::env::temp_dir().join(format!("edgefirst-convert-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out: PathBuf = dir.join("clip.mp4");
        let track = track(CLIP).unwrap();
        write_mp4(&out, &track, 30.0).unwrap();

        let size = std::fs::metadata(&out).unwrap().len();
        let reader =
            mp4::Mp4Reader::read_header(std::io::BufReader::new(File::open(&out).unwrap()), size)
                .unwrap();
        let t = reader.tracks().get(&1).unwrap();
        assert_eq!((t.width(), t.height()), (160, 120));
        assert_eq!(t.sample_count(), 30);
        assert_eq!(t.timescale(), TIMESCALE);
        assert_eq!(t.duration().as_secs(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stream_without_parameter_sets_is_refused() {
        let err = track(&[0, 0, 1, 0x41, 0x9a]).unwrap_err();
        assert!(err.to_string().contains("SPS"), "{err}");
    }
}

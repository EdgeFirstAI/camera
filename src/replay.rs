// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Replay an H.264 Annex-B file as if it were a live V4L2 camera.
//!
//! The replay path mirrors the live capture path on the wire: consumers
//! see the same Zenoh topics, same CDR schemas, and a monotonic
//! `CameraFrame.seq` that does not reset on loop. Everything not
//! recoverable from the bitstream (`/camera/info`, `/tf_static`,
//! colorimetry) comes from the `.json` sidecar written at record time.
//!
//! The top-level [`run_replay`] function takes the place of the live
//! `stream()` path in `main.rs` when `--replay` is set.

use std::{
    error::Error,
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    sync::atomic::Ordering,
    time::Duration,
};

/// Coerce the `Box<dyn StdError + Send + Sync>` that Zenoh returns into
/// the `Box<dyn Error>` the rest of this binary uses.
fn zerr<E: std::fmt::Display>(e: E) -> Box<dyn Error> {
    e.to_string().into()
}

use tracing::{info, info_span, warn};
use videostream::{
    decoder::{DecodeReturnCode, Decoder, DecoderCodec},
    frame::Frame,
};
use zenoh::{
    bytes::{Encoding, ZBytes},
    qos::{CongestionControl, Priority},
    Session,
};

use crate::{
    args::Args, build_camera_frame_msg, build_h264_msg, sidecar::Sidecar, zenoh_ts_from_ros_time,
    CameraInfoFields, ClockOffset, TfStaticFields, SHUTDOWN,
};

/// Read-chunk size for pulling Annex-B bytes off disk. Matches the
/// BufWriter size used on the record side.
const READ_CHUNK: usize = 256 * 1024;

/// Replay the recorded file at `args.replay` until EOF (or indefinitely
/// when `--replay-loop` is set). Publishes on the same topics and
/// schemas as the live capture path.
pub(crate) async fn run_replay(session: Session, args: Args) -> Result<(), Box<dyn Error>> {
    let replay_path = args
        .replay
        .clone()
        .expect("run_replay called without --replay");

    let sidecar = Sidecar::load_paired(&replay_path)?;
    info!(
        "Replay: {:?} ({} x {} @ {} fps, codec {})",
        replay_path, sidecar.width, sidecar.height, sidecar.fps, sidecar.codec
    );

    // The sidecar owns /camera/info and /tf_static — flags that would
    // have affected these in live mode are ignored. Compare the args
    // against the sidecar values and warn on any divergence so that an
    // operator who thinks they're overriding the recording gets a clear
    // signal that the sidecar won.
    warn_on_sidecar_overrides(&args, &sidecar);

    let clock_offset = ClockOffset::new()?;
    info!(
        "Clock offset: REALTIME - MONOTONIC = {}s {}ns",
        clock_offset.offset_sec, clock_offset.offset_nsec
    );

    // Camera info and tf static are published from the sidecar values
    // rather than CLI defaults; we build the CDR payloads once here and
    // reuse the bytes per publish, same pattern as the live path.
    let info_fields: CameraInfoFields = sidecar.camera_info.clone();
    let tf_fields: TfStaticFields = sidecar.tf_static.clone();

    let publ_info = session
        .declare_publisher(args.info_topic.clone())
        .priority(Priority::Background)
        .congestion_control(CongestionControl::Drop)
        .await
        .map_err(zerr)?;

    // tf_static runs on its own loop exactly like the live path.
    let tf_session = session.clone();
    let tf_bytes = ZBytes::from(tf_fields.build_msg()?.into_cdr());
    let tf_enc = Encoding::APPLICATION_CDR.with_schema("geometry_msgs/msg/TransformStamped");
    let tf_task = tokio::spawn(async move { tf_static_loop(tf_session, tf_bytes, tf_enc).await });
    std::mem::drop(tf_task);

    let info_bytes = ZBytes::from(info_fields.build_msg()?.into_cdr());
    let info_enc = Encoding::APPLICATION_CDR.with_schema("sensor_msgs/msg/CameraInfo");

    let publ_h264 = if args.h264 {
        Some(
            session
                .declare_publisher(args.h264_topic.clone())
                .priority(Priority::Data)
                .congestion_control(CongestionControl::Drop)
                .await
                .map_err(zerr)?,
        )
    } else {
        None
    };

    // Decoder + file reader
    let fps = args.replay_fps.unwrap_or(sidecar.fps).max(1);
    let decoder = Decoder::create(DecoderCodec::H264, fps as i32)?;
    let mut reader = BufReader::with_capacity(READ_CHUNK, File::open(&replay_path)?);

    // Running state
    let mut ticker = tokio::time::interval(Duration::from_micros(1_000_000 / u64::from(fps)));
    let mut carry = Carry::new();
    let mut last_data: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut seq: u64 = 0;
    let src_pid = std::process::id();

    // Base monotonic timestamp for synthesizing Header.stamp at replay
    // rate. The on-wire Time value comes from clock_offset.to_realtime
    // applied to a synthesized unix-ts::Timestamp built from this base +
    // seq/fps, so the CDR and Zenoh stamps match the live-path shape.
    let replay_start = std::time::Instant::now();

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        // Accumulate bytes fed into the decoder this frame. Forwarded
        // verbatim to rt/camera/h264 so consumers get the same
        // Annex-B that was originally published on record.
        last_data.clear();

        // Track consecutive `decode_frame` errors on the same input —
        // the V4L2 decoder returns `VSL_DEC_ERR` when its internal
        // OUTPUT buffer pool is momentarily full ("no OUTPUT buffer
        // available"), which is backpressure rather than a malformed
        // bitstream. We give the hardware a few short sleep windows to
        // drain before treating an error as a real stream fault.
        let mut backpressure_retries: u32 = 0;

        let frame = loop {
            if SHUTDOWN.load(Ordering::SeqCst) {
                // Honour SIGTERM/SIGINT even when the decoder is in a
                // backpressure retry loop — otherwise a stuck hardware
                // queue would block shutdown until SIGKILL.
                return Ok(());
            }
            match decoder.decode_frame(carry.pending()) {
                Ok((code, used, Some(frame))) => {
                    last_data.extend_from_slice(&carry.pending()[..used]);
                    carry.consume(used);
                    backpressure_retries = 0;
                    if matches!(code, DecodeReturnCode::Initialized) {
                        // decoder just consumed SPS/PPS and told us the
                        // format is known; loop again to pull an IDR.
                        continue;
                    }
                    break frame;
                }
                Ok((_code, used, None)) => {
                    last_data.extend_from_slice(&carry.pending()[..used]);
                    carry.consume(used);
                    backpressure_retries = 0;
                    if !carry.fill_from(&mut reader)? {
                        // EOF.
                        if args.replay_loop {
                            info!("Replay: looping back to start of {:?}", replay_path);
                            reader.seek(SeekFrom::Start(0))?;
                            carry.clear();
                            continue;
                        }
                        info!("Replay: reached end of file, exiting");
                        return Ok(());
                    }
                }
                Err(e) => {
                    // V4L2 decoder backpressure ("no OUTPUT buffer
                    // available") and real stream errors share this
                    // code path — the API does not distinguish them.
                    // Wait briefly and retry the same bytes; only
                    // escalate to a resync-by-byte-drop after the
                    // hardware has had a chance to drain.
                    backpressure_retries += 1;
                    if backpressure_retries <= 20 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        continue;
                    }
                    warn!(
                        "Replay: persistent decode error near byte offset {} in {:?} \
                         after {} retries: {e}. Dropping 1 byte and resyncing on the \
                         next NAL start code.",
                        reader.stream_position().unwrap_or_default(),
                        replay_path,
                        backpressure_retries,
                    );
                    backpressure_retries = 0;
                    if !carry.pending().is_empty() {
                        carry.consume(1);
                    } else if !carry.fill_from(&mut reader)? {
                        if args.replay_loop {
                            reader.seek(SeekFrom::Start(0))?;
                            carry.clear();
                            continue;
                        }
                        return Ok(());
                    }
                }
            }
        };

        ticker.tick().await;

        // Synthesize ROS time from replay_start + seq/fps. The sidecar
        // owns colorimetry so it matches what the live producer sent.
        let elapsed = replay_start.elapsed();
        let stamp = elapsed_to_ros_time(elapsed);

        publish_replayed_frame(
            &session,
            &publ_info,
            publ_h264.as_ref(),
            &info_bytes,
            &info_enc,
            &frame,
            &last_data,
            stamp,
            src_pid,
            seq,
            &args,
            &sidecar,
        )
        .await?;

        seq += 1;
    }

    Ok(())
}

/// A growing byte buffer with an amortized O(1) consume, for feeding
/// the `Decoder::decode_frame` loop without repeatedly shifting the
/// head of a `Vec<u8>` on every consumed chunk.
///
/// The decoder returns `bytes_used` and we need to advance past that
/// many bytes — but `decode_frame` also needs a contiguous `&[u8]` of
/// the unconsumed tail. We keep both the underlying `Vec<u8>` and a
/// read offset; `pending()` returns the slice from the offset onward,
/// and `consume(n)` just bumps the offset. When enough has been
/// consumed to be worth reclaiming, `consume` compacts in one shot,
/// amortizing the cost across many `consume` calls.
struct Carry {
    buf: Vec<u8>,
    read_offset: usize,
}

impl Carry {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(READ_CHUNK * 2),
            read_offset: 0,
        }
    }

    #[inline]
    fn pending(&self) -> &[u8] {
        &self.buf[self.read_offset..]
    }

    fn consume(&mut self, n: usize) {
        self.read_offset += n;
        // Compact once the head waste exceeds one read chunk; the drain
        // cost (one memmove of the tail) is then amortized over at
        // least READ_CHUNK consume calls.
        if self.read_offset > READ_CHUNK {
            self.buf.drain(..self.read_offset);
            self.read_offset = 0;
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.read_offset = 0;
    }

    /// Read one chunk directly into the tail of `buf` — no intermediate
    /// stack allocation. Returns `false` at EOF.
    fn fill_from<R: Read>(&mut self, reader: &mut R) -> std::io::Result<bool> {
        let start = self.buf.len();
        self.buf.resize(start + READ_CHUNK, 0);
        let n = reader.read(&mut self.buf[start..])?;
        self.buf.truncate(start + n);
        Ok(n > 0)
    }
}

/// Convert a `Duration` since replay start into a ROS2 `Time`. Saturates
/// negative / overflow cases the same way the live path does.
fn elapsed_to_ros_time(elapsed: Duration) -> edgefirst_schemas::builtin_interfaces::Time {
    let secs = elapsed.as_secs();
    let nanos = elapsed.subsec_nanos();
    // i32::MAX seconds is well beyond any sensible replay duration;
    // clamp defensively.
    let sec = i32::try_from(secs).unwrap_or(i32::MAX);
    edgefirst_schemas::builtin_interfaces::Time {
        sec,
        nanosec: nanos,
    }
}

#[allow(clippy::too_many_arguments)]
async fn publish_replayed_frame(
    session: &Session,
    publ_info: &zenoh::pubsub::Publisher<'_>,
    publ_h264: Option<&zenoh::pubsub::Publisher<'_>>,
    info_bytes: &ZBytes,
    info_enc: &Encoding,
    frame: &Frame,
    h264_bytes: &[u8],
    stamp: edgefirst_schemas::builtin_interfaces::Time,
    src_pid: u32,
    seq: u64,
    args: &Args,
    sidecar: &Sidecar,
) -> Result<(), Box<dyn Error>> {
    let _span = info_span!("replay_publish").entered();

    let width = frame.width()? as u32;
    let height = frame.height()? as u32;
    let stride = frame.stride()? as u32;
    let length = frame.size()? as u32;
    let fd = frame.handle()?;
    let fourcc_raw = frame.fourcc()?;
    let fourcc = fourcc_u32_to_string(fourcc_raw);
    let sample_ts = zenoh_ts_from_ros_time(session, stamp);

    // camera/frame
    let (frame_msg, frame_enc) = build_camera_frame_msg(
        stamp,
        &args.camera_frame_id,
        seq,
        src_pid,
        width,
        height,
        &fourcc,
        fd,
        stride,
        length,
        &sidecar.colorimetry,
    )?;
    session
        .put(args.frame_topic.clone(), frame_msg)
        .encoding(frame_enc)
        .timestamp(sample_ts)
        .priority(Priority::Data)
        .congestion_control(CongestionControl::Drop)
        .await
        .map_err(zerr)?;

    // rt/camera/info — same content every frame, same cadence as the live path.
    publ_info
        .put(info_bytes.clone())
        .encoding(info_enc.clone())
        .timestamp(session.new_timestamp())
        .await
        .map_err(zerr)?;

    // rt/camera/h264 — forward the Annex-B bytes verbatim. We have them
    // in h264_bytes because the replay loop collected every byte the
    // decoder consumed for this frame.
    if let Some(publ) = publ_h264 {
        if !h264_bytes.is_empty() {
            let (msg, enc) = build_h264_msg(h264_bytes, stamp, &args.camera_frame_id)?;
            publ.put(msg)
                .encoding(enc)
                .timestamp(sample_ts)
                .await
                .map_err(zerr)?;
        }
    }

    Ok(())
}

/// Convert a `u32` fourcc (little-endian packed) into a 4-character
/// string. `videostream::fourcc::FourCC::to_string` does the same thing
/// but lives behind a constructor we do not use here; inlining keeps
/// the replay module self-contained.
fn fourcc_u32_to_string(v: u32) -> String {
    let bytes = v.to_le_bytes();
    bytes.iter().map(|b| *b as char).collect()
}

/// Emit a `warn!` for every CLI arg whose value would differ from what
/// the sidecar carries, so a replay run surfaces the ignored flags
/// rather than silently acting on the sidecar's values.
fn warn_on_sidecar_overrides(args: &Args, sidecar: &Sidecar) {
    if !args.cam_info_path.is_empty() {
        warn!(
            "--cam-info-path {:?} is ignored in replay mode; the sidecar's camera_info is used",
            args.cam_info_path
        );
    }
    if args.base_frame_id != sidecar.tf_static.base_frame_id {
        warn!(
            "--base-frame-id {:?} differs from sidecar tf_static.base_frame_id {:?}; using the sidecar value",
            args.base_frame_id, sidecar.tf_static.base_frame_id
        );
    }
    if args.camera_frame_id != sidecar.tf_static.child_frame_id {
        warn!(
            "--camera-frame-id {:?} differs from sidecar tf_static.child_frame_id {:?}; using the sidecar value",
            args.camera_frame_id, sidecar.tf_static.child_frame_id
        );
    }
    let arg_t = [args.cam_tf_vec[0], args.cam_tf_vec[1], args.cam_tf_vec[2]];
    if arg_t != sidecar.tf_static.translation {
        warn!(
            "--cam-tf-vec {:?} differs from sidecar tf_static.translation {:?}; using the sidecar value",
            arg_t, sidecar.tf_static.translation
        );
    }
    let arg_r = [
        args.cam_tf_quat[0],
        args.cam_tf_quat[1],
        args.cam_tf_quat[2],
        args.cam_tf_quat[3],
    ];
    if arg_r != sidecar.tf_static.rotation {
        warn!(
            "--cam-tf-quat {:?} differs from sidecar tf_static.rotation {:?}; using the sidecar value",
            arg_r, sidecar.tf_static.rotation
        );
    }
}

async fn tf_static_loop(
    session: Session,
    msg: ZBytes,
    enc: Encoding,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let topic = "rt/tf_static".to_string();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        session
            .put(&topic, msg.clone())
            .encoding(enc.clone())
            .timestamp(session.new_timestamp())
            .await?;
    }
}

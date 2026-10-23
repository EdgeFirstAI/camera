// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

use clap::{ArgAction, CommandFactory, Parser};
use serde_json::json;
use std::{fmt, path::PathBuf, str::FromStr};
use zenoh::config::{Config, WhatAmI};

/// A named capture mode: a size name and a frame rate (`1080p30`).
///
/// Size names: VGA (640×480), WVGA (800×480), 540p (960×540), 720p
/// (1280×720), 1080p (1920×1080), 4K (3840×2160). A mode is shorthand for
/// `CAMERA_SIZE` plus `CAMERA_FPS`, accepted only when the camera reports
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraMode {
    /// Capture size.
    pub size: (u32, u32),
    /// Frames per second.
    pub fps: u32,
}

impl fmt::Display for CameraMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (w, h) = self.size;
        write!(f, "{w}x{h}@{fps}", fps = self.fps)
    }
}

impl FromStr for CameraMode {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        parse_camera_mode(raw)
    }
}

fn parse_fps(digits: &str) -> Result<u32, String> {
    let fps: u32 = digits
        .parse()
        .map_err(|_| format!("invalid camera mode FPS '{digits}'"))?;
    if fps == 0 {
        return Err("camera mode FPS must be a positive whole number".into());
    }
    if fps > i32::MAX as u32 {
        return Err(format!("camera mode FPS must not exceed {}", i32::MAX));
    }
    Ok(fps)
}

fn named_size(token: &str) -> Result<(u32, u32), String> {
    match token {
        "vga" => Ok((640, 480)),
        "wvga" => Ok((800, 480)),
        "540p" => Ok((960, 540)),
        "720p" => Ok((1280, 720)),
        "1080p" => Ok((1920, 1080)),
        "4k" => Ok((3840, 2160)),
        other => Err(format!(
            "unknown camera size '{other}'; expected VGA, WVGA, 540p, 720p, 1080p, or 4K"
        )),
    }
}

fn parse_camera_mode(raw: &str) -> Result<CameraMode, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("camera mode must not be empty".into());
    }
    let lower = s.to_ascii_lowercase();

    if let Some(fps) = lower.strip_suffix("fps") {
        if !fps.is_empty() && fps.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "camera mode '{raw}' sets only a frame rate, which is no longer accepted; \
                 use CAMERA_FPS={fps} (--camera-fps {fps})"
            ));
        }
    }

    let fps_start = lower
        .bytes()
        .rposition(|b| !b.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    if fps_start == lower.len() {
        return Err(format!(
            "camera mode '{raw}' is missing a frame rate; use e.g. 1080p30"
        ));
    }
    if fps_start == 0 {
        return Err(format!(
            "camera mode '{raw}' needs a size name; use e.g. 1080p30"
        ));
    }

    Ok(CameraMode {
        size: named_size(&lower[..fps_start])?,
        fps: parse_fps(&lower[fps_start..])?,
    })
}

/// A camera control setting: `auto`, or a fixed value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Setting<T> {
    /// Let the camera choose.
    Auto,
    /// A fixed value.
    Manual(T),
}

fn parse_setting<T: FromStr>(raw: &str, what: &str) -> Result<Setting<T>, String> {
    let s = raw.trim();
    if s.eq_ignore_ascii_case("auto") {
        return Ok(Setting::Auto);
    }
    s.parse()
        .map(Setting::Manual)
        .map_err(|_| format!("{what} must be 'auto' or a number, not '{raw}'"))
}

fn parse_exposure(raw: &str) -> Result<Setting<u32>, String> {
    match parse_setting(raw, "exposure")? {
        Setting::Manual(0) => Err("exposure must be at least 1 microsecond".into()),
        other => Ok(other),
    }
}

fn parse_gain(raw: &str) -> Result<Setting<f32>, String> {
    match parse_setting::<f32>(raw, "gain")? {
        Setting::Manual(g) if !(g.is_finite() && g > 0.0) => {
            Err(format!("gain must be positive, not {g}"))
        }
        other => Ok(other),
    }
}

fn parse_white_balance(raw: &str) -> Result<Setting<u32>, String> {
    match parse_setting(raw, "white balance")? {
        Setting::Manual(0) => Err("white balance must be a positive colour temperature".into()),
        other => Ok(other),
    }
}

fn parse_positive_fps(raw: &str) -> Result<f64, String> {
    let fps: f64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("frame rate must be a number, not '{raw}'"))?;
    if !(fps.is_finite() && fps > 0.0) {
        return Err(format!("frame rate must be positive, not {fps}"));
    }
    Ok(fps)
}

/// How capture buffers are allocated.
#[derive(clap::ValueEnum, Clone, Debug, PartialEq, Copy)]
pub enum MemorySetting {
    /// Import this service's buffers, falling back to the driver's own when
    /// it refuses them.
    Auto,
    /// Import buffers allocated by this service.
    Import,
    /// Use the driver's own buffers.
    Export,
}

/// Camera image mirroring options.
///
/// Flips the image so it is published in its natural orientation, undoing
/// how the sensor is mounted. Downstream consumers such as fusion assume the
/// published image is natural; the camera TF and calibration describe that
/// natural image and must not encode the mount rotation.
#[derive(clap::ValueEnum, Clone, Debug, PartialEq, Copy)]
pub enum MirrorSetting {
    /// No mirroring
    None,
    /// Flip horizontally (left-right)
    Horizontal,
    /// Flip vertically (top-bottom)
    Vertical,
    /// Flip both horizontally and vertically (180-degree rotation)
    Both,
}

/// H.264 encoding bitrate presets.
///
/// Controls the trade-off between video quality and file size.
/// Higher bitrates produce better quality but larger files.
#[derive(clap::ValueEnum, Clone, Debug, PartialEq, Copy)]
pub enum H264Bitrate {
    /// Automatic bitrate selection based on resolution
    Auto,
    /// 5 Mbps (suitable for 720p)
    Mbps5,
    /// 25 Mbps (suitable for 1080p)
    Mbps25,
    /// 50 Mbps (suitable for high-quality 1080p)
    Mbps50,
    /// 100 Mbps (suitable for 4K or very high quality)
    Mbps100,
}

/// Command-line arguments for EdgeFirst Camera Node.
///
/// This structure defines all configuration options for the camera node,
/// including camera selection, output formats, Zenoh configuration, and
/// debugging options. Arguments can be specified via command line or
/// environment variables.
///
/// # Example
///
/// ```bash
/// # Via command line
/// edgefirst-camera --source /dev/video0 --jpeg --h264
///
/// # Via environment variables
/// export SOURCE=/dev/video0
/// export JPEG=true
/// edgefirst-camera
/// ```
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Frame source: a V4L2 device path (`/dev/video3`), `file:<name>.h264`
    /// for a recording with its `<name>.json` sidecar, or `mock[:WxH@fps]`
    /// for synthetic frames. Unset uses `--camera`.
    #[arg(long, env = "SOURCE")]
    pub source: Option<String>,

    /// Camera device path, used when `--source` is unset.
    #[arg(short, long, env = "CAMERA", default_value = "/dev/video3")]
    pub camera: String,

    /// Recording to replay; the 2.x form of `--source file:<path>`.
    #[arg(long, env = "REPLAY", hide = true, conflicts_with = "source")]
    pub replay: Option<PathBuf>,

    /// How capture buffers are allocated: `import` this service's
    /// contiguous buffers, `export` the driver's own, or `auto` to import
    /// and fall back to the driver's buffers when it refuses them.
    #[arg(long, env = "MEMORY", default_value = "auto", value_enum)]
    pub memory: MemorySetting,

    /// Requested capture size in pixels (width height). Unset keeps the
    /// camera's current size.
    #[arg(long, env = "CAMERA_SIZE", value_delimiter = ' ', num_args = 2)]
    pub camera_size: Option<Vec<u32>>,

    /// Requested capture rate in frames per second. Unset keeps the
    /// camera's current rate. For a `file:` source, the playback rate
    /// (default: the recording's).
    #[arg(long, env = "CAMERA_FPS", value_parser = parse_positive_fps)]
    pub camera_fps: Option<f64>,

    /// Capture mode shorthand (`1080p30`): `--camera-size` plus
    /// `--camera-fps`, accepted only when the camera reports the mode.
    /// Explicit `--camera-size` and `--camera-fps` win over it.
    #[arg(long, env = "CAMERA_MODE")]
    pub camera_mode: Option<CameraMode>,

    /// Number of capture buffers (2-32).
    ///
    /// Each published frame references one of these buffers by DMA-BUF
    /// file descriptor, and the camera refills a buffer once the frames
    /// queued ahead of it are captured. A subscriber therefore has about
    /// `(buffers - 1)` frame periods to read a frame before its pixels
    /// are overwritten: 100 ms with 4 buffers at 30 FPS, 50 ms at 60 FPS.
    /// Each buffer costs one capture frame of contiguous (CMA) memory.
    #[arg(
        long,
        env = "CAMERA_BUFFERS",
        default_value_t = 4,
        value_parser = clap::value_parser!(u32).range(2..=32)
    )]
    pub camera_buffers: u32,

    /// Exposure: `auto`, or a fixed exposure time in microseconds.
    #[arg(long, env = "EXPOSURE", value_parser = parse_exposure)]
    pub exposure: Option<Setting<u32>>,

    /// Analogue gain: `auto`, or a fixed gain multiplier.
    #[arg(long, env = "GAIN", value_parser = parse_gain)]
    pub gain: Option<Setting<f32>>,

    /// White balance: `auto`, or a fixed colour temperature in kelvin.
    #[arg(long, env = "WHITE_BALANCE", value_parser = parse_white_balance)]
    pub white_balance: Option<Setting<u32>>,

    /// Flip the camera image into its natural orientation to undo the sensor
    /// mount (Maivin and Raivin mount it upside down, hence `both`).
    #[arg(long, env = "MIRROR", default_value = "both", value_enum)]
    pub mirror: MirrorSetting,

    /// Zenoh topic for multi-plane camera frame (edgefirst_msgs/CameraFrame).
    /// Supersedes `--dma-topic` from 2.6.x. The new topic drops the `rt/`
    /// prefix per the schemas 3.1 convention for newly introduced topics.
    #[arg(long, env = "FRAME_TOPIC", default_value = "camera/frame")]
    pub frame_topic: String,

    /// Zenoh topic for camera calibration info (sensor_msgs/CameraInfo)
    #[arg(long, env = "INFO_TOPIC", default_value = "camera/info")]
    pub info_topic: String,

    /// Enable JPEG streaming output
    #[arg(long, env = "JPEG")]
    pub jpeg: bool,

    /// Zenoh topic for JPEG compressed images (sensor_msgs/CompressedImage)
    #[arg(long, env = "JPEG_TOPIC", default_value = "camera/jpeg")]
    pub jpeg_topic: String,

    /// JPEG encoding quality (1-100)
    ///
    /// Was hardcoded at 100. 85 is visually equivalent for slightly less
    /// work, but quality is a weak lever on cost: measured at 1080p30 on
    /// a Verdin iMX8MP, enabling JPEG takes this process from 27% to 104%
    /// CPU and 100 -> 85 recovers only ~3 of those 77 points. The cost is
    /// the per-frame conversion and DCT, not the entropy coding.
    #[arg(
        long,
        env = "JPEG_QUALITY",
        default_value_t = 85,
        value_parser = clap::value_parser!(u8).range(1..=100)
    )]
    pub jpeg_quality: u8,

    /// Enable H.264 video streaming output
    ///
    /// On by default. H.264 is the stream every Maivin consumer expects,
    /// and defaulting it off meant a bare `edgefirst-camera` published no
    /// video at all -- invisible to the service, which sets H264 in
    /// /etc/default/camera, but a repeated surprise when running the
    /// binary by hand. Takes an optional value (`--h264 false`) so it can
    /// still be turned off from the command line or the environment.
    #[arg(
        long,
        env = "H264",
        default_value_t = true,
        action = ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    pub h264: bool,

    /// Zenoh topic for H.264 video stream (foxglove_msgs/CompressedVideo)
    #[arg(long, env = "H264_TOPIC", default_value = "camera/h264")]
    pub h264_topic: String,

    /// H.264 encoding bitrate preset
    #[arg(long, env = "H264_BITRATE", default_value = "auto")]
    pub h264_bitrate: H264Bitrate,

    /// Enable 4K tiling (splits 4K into 4x 1080p tiles for hardware encoding)
    #[arg(long, env = "H264_TILES")]
    pub h264_tiles: bool,

    /// Zenoh topics for H.264 tiles: top-left, top-right, bottom-left,
    /// bottom-right
    #[arg(
        long,
        env = "H264_TILES_TOPICS",
        default_value = "camera/h264/tl camera/h264/tr camera/h264/bl camera/h264/br",
        value_delimiter = ' ',
        num_args = 4
    )]
    pub h264_tiles_topics: Vec<String>,

    /// FPS limit for H.264 tiles (lower than camera FPS to reduce compression
    /// artifacts)
    #[arg(long, env = "H264_TILES_FPS", default_value = "15")]
    pub h264_tiles_fps: u32,

    /// Record the H.264 stream to this file (raw Annex-B `.h264`).
    ///
    /// A matching `<name>.json` sidecar is written alongside at startup
    /// carrying colorimetry, `/camera/info`, and `/tf_static`: the producer
    /// state that is not recoverable from the bitstream. Requires `--h264`
    /// and a camera source.
    #[arg(long, env = "RECORD")]
    pub record: Option<PathBuf>,

    /// Loop a `file:` source back to its start at the end instead of
    /// exiting. `CameraFrame.seq` keeps counting across loops, as from a
    /// live camera.
    #[arg(long, env = "REPLAY_LOOP", default_value_t = false)]
    pub replay_loop: bool,

    /// Playback rate of a `file:` source; the 2.x name of `--camera-fps`,
    /// which wins when both are set.
    #[arg(long, env = "REPLAY_FPS", value_parser = parse_positive_fps)]
    pub replay_fps: Option<f64>,

    /// Output streaming resolution in pixels (width height)
    #[arg(
        short,
        long,
        env = "STREAM_SIZE",
        default_value = "1920 1080",
        value_delimiter = ' ',
        num_args = 2
    )]
    pub stream_size: Vec<u32>,

    /// Enable verbose debug logging
    #[arg(short, long)]
    pub verbose: bool,

    /// Path to camera calibration JSON file (isp-imx format)
    #[arg(long, env = "CAM_INFO_PATH", default_value = "")]
    pub cam_info_path: String,

    /// Camera optical frame translation from base_link (x y z in meters)
    #[arg(
        long,
        env = "CAM_TF_VEC",
        default_value = "0 0 0",
        value_delimiter = ' ',
        num_args = 3
    )]
    pub cam_tf_vec: Vec<f64>,

    /// Camera optical frame rotation quaternion from base_link (x y z w)
    #[arg(
        long,
        env = "CAM_TF_QUAT",
        default_value = "-1 1 -1 1",
        value_delimiter = ' ',
        num_args = 4
    )]
    pub cam_tf_quat: Vec<f64>,

    /// TF frame ID for robot base
    #[arg(long, env = "BASE_FRAME_ID", default_value = "base_link")]
    pub base_frame_id: String,

    /// TF frame ID for camera optical frame
    #[arg(long, env = "CAMERA_FRAME_ID", default_value = "camera_optical")]
    pub camera_frame_id: String,

    /// Enable Tokio async runtime console for debugging
    #[arg(long, env = "TOKIO_CONSOLE")]
    pub tokio_console: bool,

    /// Enable Tracy profiler for performance analysis
    #[arg(long, env = "TRACY")]
    pub tracy: bool,

    /// Zenoh participant mode (peer, client, or router)
    #[arg(long, env = "MODE", default_value = "peer")]
    mode: WhatAmI,

    /// Zenoh endpoints to connect to (can specify multiple)
    #[arg(long, env = "CONNECT")]
    connect: Vec<String>,

    /// Zenoh endpoints to listen on (can specify multiple)
    #[arg(long, env = "LISTEN")]
    listen: Vec<String>,

    /// Disable Zenoh multicast peer discovery
    #[arg(long, env = "NO_MULTICAST_SCOUTING")]
    no_multicast_scouting: bool,
}

impl Args {
    /// The source string for the SDK: `--source`, else `--replay` as a
    /// `file:` source, else the `--camera` device.
    pub fn source_spec(&self) -> String {
        if let Some(source) = &self.source {
            return source.clone();
        }
        if let Some(path) = &self.replay {
            return format!("file:{}", path.display());
        }
        self.camera.clone()
    }

    /// Whether the source is a recording.
    pub fn is_file_source(&self) -> bool {
        self.source_spec().starts_with("file:")
    }

    /// Capture size set by `--camera-size`.
    pub fn explicit_size(&self) -> Option<(u32, u32)> {
        self.camera_size
            .as_ref()
            .and_then(|v| (v.len() >= 2).then_some((v[0], v[1])))
    }

    /// Frame rate set by `--camera-fps`, or by `--replay-fps` for a `file:`
    /// source.
    pub fn explicit_fps(&self) -> Option<f64> {
        self.camera_fps
            .or_else(|| self.replay_fps.filter(|_| self.is_file_source()))
    }
}

/// Environment variables where an empty value is meaningful and must be
/// preserved (i.e. the argument has a non-empty default but "" is a
/// documented "disable" sentinel).
///
/// Empty for this service: every option that documents "leave empty to
/// disable" (`CAM_INFO_PATH`, `CONNECT`, `LISTEN`, `RECORD`, `SOURCE`,
/// `REPLAY_FPS`) either has no default or an empty one, so scrubbing it
/// yields the same result as passing "".
pub const KEEP: &[&str] = &[];

/// Names of this program's env-bound arguments whose value, as reported by
/// `var`, is present but empty and not listed in `keep`.
///
/// Pure: the environment is only read through `var`, so this can be unit
/// tested with a fake lookup and no process-wide mutation.
pub fn empty_env_vars<C: CommandFactory>(
    keep: &[&str],
    var: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    C::command()
        .get_arguments()
        .filter_map(|arg| arg.get_env().map(|e| e.to_string_lossy().into_owned()))
        .filter(|name| !keep.contains(&name.as_str()))
        .filter(|name| var(name).is_some_and(|v| v.is_empty()))
        .collect()
}

/// Treat an empty environment variable as unset, so clap's declared
/// `default_value` applies instead of failing to parse.
///
/// systemd's `EnvironmentFile` exports `KEY=""` as an empty string rather
/// than leaving the variable unset, and clap treats a present-but-empty
/// variable as a supplied value: `REPLAY_FPS=""` fails integer parsing,
/// `JPEG=""` is "a value is required", and `MIRROR=""` is an invalid
/// enum. A `value_parser` cannot fix this -- it can return a value or an
/// error but never "absent" -- so the variable must be removed before
/// clap sees it.
///
/// Only variables bound to this program's own arguments are considered;
/// unrelated process environment is left alone. The list is derived from
/// `C::command()` rather than written by hand so it cannot drift from
/// the argument definitions. `keep` names variables where an empty value
/// is meaningful and must be preserved.
///
/// # Safety
/// Must be called before any thread is spawned -- that is, before the
/// tokio runtime is built. Mutating the process environment is not
/// thread-safe.
pub unsafe fn scrub_empty_env<C: CommandFactory>(keep: &[&str]) {
    for name in empty_env_vars::<C>(keep, |name| std::env::var(name).ok()) {
        std::env::remove_var(&name);
    }
}

/// System hostname used as the Zenoh session namespace.
///
/// Empty or `/`-containing hostnames would create unintended sub-keys, so we
/// fall back to `"localhost"` and warn. Two devices both falling back would
/// silently share a namespace; that is a deployment defect.
fn zenoh_namespace() -> String {
    let raw = gethostname::gethostname().to_string_lossy().into_owned();
    if raw.is_empty() || raw.contains('/') {
        tracing::warn!(
            hostname = %raw,
            "system hostname is empty or contains '/' — falling back to \"localhost\""
        );
        "localhost".into()
    } else {
        raw
    }
}

impl From<Args> for Config {
    fn from(args: Args) -> Self {
        let mut config = Config::default();

        // Session namespace = hostname: application keys are bare
        // (`camera/frame`) and the wire form is `{hostname}/camera/frame`.
        config
            .insert_json5("namespace", &json!(zenoh_namespace()).to_string())
            .unwrap();

        config
            .insert_json5("mode", &json!(args.mode).to_string())
            .unwrap();

        let connect: Vec<_> = args.connect.into_iter().filter(|s| !s.is_empty()).collect();
        if !connect.is_empty() {
            config
                .insert_json5("connect/endpoints", &json!(connect).to_string())
                .unwrap();
        }

        let listen: Vec<_> = args.listen.into_iter().filter(|s| !s.is_empty()).collect();
        if !listen.is_empty() {
            config
                .insert_json5("listen/endpoints", &json!(listen).to_string())
                .unwrap();
        }

        if args.no_multicast_scouting {
            config
                .insert_json5("scouting/multicast/enabled", &json!(false).to_string())
                .unwrap();
        }

        // Linux names its loopback `lo`; macOS (`lo0`) and Windows keep
        // zenoh's default interface.
        #[cfg(target_os = "linux")]
        config
            .insert_json5("scouting/multicast/interface", &json!("lo").to_string())
            .unwrap();

        config
    }
}

/// Every flag and the environment variable that sets it, with a value
/// each accepts. `/etc/default/camera` reaches the service only through
/// the environment, so a flag without its variable is unconfigurable
/// there; `tests/env_scrub.rs` sets each variable and checks it parses
/// as the flag does.
#[cfg(test)]
pub(crate) const FLAG_ENV: &[(&str, &str, &str)] = &[
    ("source", "SOURCE", "mock:640x480@30"),
    ("camera", "CAMERA", "/dev/video0"),
    ("memory", "MEMORY", "import"),
    ("camera-size", "CAMERA_SIZE", "1280 720"),
    ("camera-fps", "CAMERA_FPS", "15"),
    ("camera-mode", "CAMERA_MODE", "720p30"),
    ("camera-buffers", "CAMERA_BUFFERS", "6"),
    ("exposure", "EXPOSURE", "auto"),
    ("gain", "GAIN", "2.5"),
    ("white-balance", "WHITE_BALANCE", "4000"),
    ("mirror", "MIRROR", "none"),
    ("frame-topic", "FRAME_TOPIC", "cam/frame"),
    ("info-topic", "INFO_TOPIC", "cam/info"),
    ("jpeg", "JPEG", "true"),
    ("jpeg-topic", "JPEG_TOPIC", "cam/jpeg"),
    ("jpeg-quality", "JPEG_QUALITY", "70"),
    ("h264", "H264", "false"),
    ("h264-topic", "H264_TOPIC", "cam/h264"),
    ("h264-bitrate", "H264_BITRATE", "mbps5"),
    ("h264-tiles", "H264_TILES", "true"),
    ("h264-tiles-topics", "H264_TILES_TOPICS", "a b c d"),
    ("h264-tiles-fps", "H264_TILES_FPS", "5"),
    ("record", "RECORD", "out.h264"),
    ("replay-loop", "REPLAY_LOOP", "true"),
    ("replay-fps", "REPLAY_FPS", "12"),
    ("stream-size", "STREAM_SIZE", "640 480"),
    ("cam-info-path", "CAM_INFO_PATH", "cal.json"),
    ("cam-tf-vec", "CAM_TF_VEC", "1 2 3"),
    ("cam-tf-quat", "CAM_TF_QUAT", "0 0 0 1"),
    ("base-frame-id", "BASE_FRAME_ID", "base"),
    ("camera-frame-id", "CAMERA_FRAME_ID", "cam"),
    ("tokio-console", "TOKIO_CONSOLE", "true"),
    ("tracy", "TRACY", "true"),
    ("mode", "MODE", "client"),
    ("connect", "CONNECT", "tcp/127.0.0.1:7447"),
    ("listen", "LISTEN", "tcp/127.0.0.1:7448"),
    ("no-multicast-scouting", "NO_MULTICAST_SCOUTING", "true"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Env-bound arguments with a non-empty default where we have
    /// consciously decided that an empty value is NOT meaningful (so
    /// scrubbing to the default is correct).
    const SCRUB_REVIEWED: &[&str] = &[
        "CAMERA",
        "MEMORY",
        "CAMERA_BUFFERS",
        "MIRROR",
        "FRAME_TOPIC",
        "INFO_TOPIC",
        "JPEG_TOPIC",
        "JPEG_QUALITY",
        "H264",
        "H264_TOPIC",
        "H264_BITRATE",
        "H264_TILES_TOPICS",
        "H264_TILES_FPS",
        "REPLAY_LOOP",
        "STREAM_SIZE",
        "CAM_TF_VEC",
        "CAM_TF_QUAT",
        "BASE_FRAME_ID",
        "CAMERA_FRAME_ID",
        "MODE",
    ];

    /// Drift guard: adding an env-bound option with a non-empty default
    /// forces a decision about whether "" means "use the default" (add
    /// it to `SCRUB_REVIEWED`) or is a meaningful sentinel (add it to
    /// `KEEP`).
    #[test]
    fn every_env_arg_is_either_scrubbable_or_explicitly_kept() {
        for arg in Args::command().get_arguments() {
            let Some(env) = arg.get_env() else { continue };
            let name = env.to_string_lossy().into_owned();
            let has_nonempty_default = arg
                .get_default_values()
                .first()
                .is_some_and(|d| !d.is_empty());
            if has_nonempty_default && !KEEP.contains(&name.as_str()) {
                assert!(
                    SCRUB_REVIEWED.contains(&name.as_str()),
                    "{name} has a non-empty default; decide whether empty is meaningful \
                     and add it to KEEP or SCRUB_REVIEWED"
                );
            }
        }
    }

    /// Fake environment lookup for `empty_env_vars`: only the listed
    /// names are "set", everything else reads as unset.
    fn lookup<'a>(env: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            env.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    /// A numeric, a boolean, an enum and an optional variable exported as
    /// "" are selected for scrubbing.
    #[test]
    fn empty_env_vars_are_selected() {
        let env = [
            ("JPEG_QUALITY", ""),
            ("H264", ""),
            ("MIRROR", ""),
            ("REPLAY_FPS", ""),
        ];
        let mut found = empty_env_vars::<Args>(KEEP, lookup(&env));
        found.sort_unstable();
        assert_eq!(found, ["H264", "JPEG_QUALITY", "MIRROR", "REPLAY_FPS"]);
    }

    #[test]
    fn non_empty_env_vars_are_not_selected() {
        let env = [("H264_TILES_FPS", "7"), ("JPEG_QUALITY", "")];
        assert_eq!(
            empty_env_vars::<Args>(KEEP, lookup(&env)),
            ["JPEG_QUALITY"],
            "a real value must survive untouched"
        );
    }

    #[test]
    fn unset_env_vars_are_not_selected() {
        assert!(empty_env_vars::<Args>(KEEP, lookup(&[])).is_empty());
    }

    #[test]
    fn kept_env_vars_are_not_selected_even_when_empty() {
        let env = [("JPEG_QUALITY", ""), ("H264", "")];
        assert_eq!(
            empty_env_vars::<Args>(&["JPEG_QUALITY"], lookup(&env)),
            ["H264"]
        );
    }

    #[test]
    fn unbound_env_vars_are_never_selected() {
        let env = [("EDGEFIRST_CAMERA_NOT_AN_ARG", ""), ("PATH", "")];
        assert!(empty_env_vars::<Args>(KEEP, lookup(&env)).is_empty());
    }

    #[test]
    fn h264_is_enabled_by_default() {
        // A bare manual run publishes video, as the service does.
        assert!(Args::parse_from(["edgefirst-camera"]).h264);
    }

    #[test]
    fn h264_can_still_be_turned_off() {
        let args = Args::try_parse_from(["edgefirst-camera", "--h264", "false"])
            .expect("--h264 must accept an explicit value so it can be disabled");
        assert!(!args.h264);
    }

    #[test]
    fn jpeg_quality_defaults_below_lossless() {
        assert_eq!(Args::parse_from(["edgefirst-camera"]).jpeg_quality, 85);
    }

    #[test]
    fn jpeg_quality_rejects_values_outside_the_jpeg_range() {
        for bad in ["0", "101"] {
            assert!(
                Args::try_parse_from(["edgefirst-camera", "--jpeg-quality", bad]).is_err(),
                "quality {bad} must be rejected"
            );
        }
    }

    #[test]
    fn zenoh_config_sets_namespace() {
        let args = Args::parse_from(["edgefirst-camera"]);
        let cfg = Config::from(args);
        let ns: String = serde_json::from_str(&cfg.to_string())
            .ok()
            .and_then(|v: serde_json::Value| {
                v.pointer("/namespace")
                    .and_then(|n| n.as_str().map(String::from))
            })
            .expect("namespace should be set in config");
        assert!(!ns.is_empty(), "namespace should be non-empty");
        assert!(!ns.contains('/'), "namespace must not contain '/'");
    }

    #[test]
    fn default_topics_have_no_rt_prefix() {
        let args = Args::parse_from(["edgefirst-camera"]);
        assert_eq!(args.frame_topic, "camera/frame");
        assert_eq!(args.info_topic, "camera/info");
        assert_eq!(args.jpeg_topic, "camera/jpeg");
        assert_eq!(args.h264_topic, "camera/h264");
        for topic in &args.h264_tiles_topics {
            assert!(
                !topic.starts_with("rt/"),
                "tile topic {topic} still has rt/"
            );
        }
    }

    #[test]
    fn named_sizes_map_to_pixels() {
        let cases = [
            ("VGA30", 640, 480, 30),
            ("wvga15", 800, 480, 15),
            ("540p24", 960, 540, 24),
            ("720p30", 1280, 720, 30),
            ("1080p30", 1920, 1080, 30),
            ("4K60", 3840, 2160, 60),
            ("4k1", 3840, 2160, 1),
        ];
        for (raw, w, h, fps) in cases {
            let mode = CameraMode::from_str(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(mode.size, (w, h), "{raw}");
            assert_eq!(mode.fps, fps, "{raw}");
        }
    }

    #[test]
    fn fps_only_modes_point_to_camera_fps() {
        for raw in ["30FPS", "30fps", " 60FPS "] {
            let err = CameraMode::from_str(raw).unwrap_err();
            assert!(err.contains("CAMERA_FPS"), "{raw}: {err}");
        }
    }

    #[test]
    fn malformed_modes_are_rejected() {
        for raw in [
            "",
            "1080p",
            "30",
            "FPS30",
            "1080p0",
            "4K2147483648",
            "QSXGA30",
            "1080p30fps",
        ] {
            assert!(CameraMode::from_str(raw).is_err(), "{raw} must be rejected");
        }
    }

    #[test]
    fn source_defaults_to_the_camera_device() {
        let args = Args::parse_from(["edgefirst-camera"]);
        assert_eq!(args.source_spec(), "/dev/video3");
        assert!(!args.is_file_source());
        let args = Args::parse_from(["edgefirst-camera", "--camera", "/dev/video0"]);
        assert_eq!(args.source_spec(), "/dev/video0");
    }

    #[test]
    fn source_wins_over_camera() {
        let args = Args::parse_from([
            "edgefirst-camera",
            "--camera",
            "/dev/video0",
            "--source",
            "mock:640x480@30",
        ]);
        assert_eq!(args.source_spec(), "mock:640x480@30");
    }

    #[test]
    fn replay_is_a_file_source() {
        let args = Args::parse_from(["edgefirst-camera", "--replay", "rec/run.h264"]);
        assert_eq!(args.source_spec(), "file:rec/run.h264");
        assert!(args.is_file_source());
        assert!(
            Args::try_parse_from(["edgefirst-camera", "--replay", "a.h264", "--source", "mock"])
                .is_err(),
            "--replay and --source name the same thing"
        );
    }

    #[test]
    fn replay_fps_applies_to_file_sources_only() {
        let file = Args::parse_from([
            "edgefirst-camera",
            "--source",
            "file:a.h264",
            "--replay-fps",
            "15",
        ]);
        assert_eq!(file.explicit_fps(), Some(15.0));
        let live = Args::parse_from(["edgefirst-camera", "--replay-fps", "15"]);
        assert_eq!(live.explicit_fps(), None);
        let both = Args::parse_from([
            "edgefirst-camera",
            "--source",
            "file:a.h264",
            "--replay-fps",
            "15",
            "--camera-fps",
            "10",
        ]);
        assert_eq!(both.explicit_fps(), Some(10.0), "CAMERA_FPS wins");
    }

    #[test]
    fn camera_fps_accepts_fractional_positive_rates() {
        let args = Args::parse_from(["edgefirst-camera", "--camera-fps", "29.97"]);
        assert_eq!(args.explicit_fps(), Some(29.97));
        for bad in ["0", "-1", "fast", "inf"] {
            assert!(
                Args::try_parse_from(["edgefirst-camera", "--camera-fps", bad]).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn neither_size_nor_fps_keeps_the_camera_setting() {
        let args = Args::parse_from(["edgefirst-camera"]);
        assert_eq!(args.explicit_size(), None);
        assert_eq!(args.explicit_fps(), None);
        assert_eq!(args.camera_mode, None);
    }

    #[test]
    fn camera_buffers_defaults_to_four() {
        assert_eq!(Args::parse_from(["edgefirst-camera"]).camera_buffers, 4);
    }

    #[test]
    fn camera_buffers_accepts_the_supported_range() {
        let args = Args::parse_from(["edgefirst-camera", "--camera-buffers", "6"]);
        assert_eq!(args.camera_buffers, 6);
        for bad in ["0", "1", "33", "four"] {
            assert!(
                Args::try_parse_from(["edgefirst-camera", "--camera-buffers", bad]).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn controls_take_auto_or_a_value() {
        let args = Args::parse_from([
            "edgefirst-camera",
            "--exposure",
            "10000",
            "--gain",
            "AUTO",
            "--white-balance",
            "5600",
        ]);
        assert_eq!(args.exposure, Some(Setting::Manual(10_000)));
        assert_eq!(args.gain, Some(Setting::Auto));
        assert_eq!(args.white_balance, Some(Setting::Manual(5600)));
        for (flag, bad) in [
            ("--exposure", "0"),
            ("--exposure", "long"),
            ("--gain", "-2"),
            ("--white-balance", "0"),
        ] {
            assert!(
                Args::try_parse_from(["edgefirst-camera", flag, bad]).is_err(),
                "{flag} {bad} must be rejected"
            );
        }
    }

    #[test]
    fn memory_defaults_to_auto() {
        assert_eq!(
            Args::parse_from(["edgefirst-camera"]).memory,
            MemorySetting::Auto
        );
        let args = Args::parse_from(["edgefirst-camera", "--memory", "export"]);
        assert_eq!(args.memory, MemorySetting::Export);
    }

    /// Arguments bound to the environment but deliberately absent from
    /// `FLAG_ENV`: hidden 2.x aliases.
    const UNLISTED: &[&str] = &["REPLAY"];

    #[test]
    fn every_flag_is_bound_to_its_environment_variable() {
        let cmd = Args::command();
        let mut bound: Vec<String> = cmd
            .get_arguments()
            .filter_map(|a| a.get_env().map(|e| e.to_string_lossy().into_owned()))
            .filter(|e| !UNLISTED.contains(&e.as_str()))
            .collect();
        bound.sort();
        let mut listed: Vec<String> = FLAG_ENV.iter().map(|(_, e, _)| (*e).into()).collect();
        listed.sort();
        assert_eq!(bound, listed, "FLAG_ENV must list every env-bound argument");
        for (flag, env, _) in FLAG_ENV {
            let arg = cmd
                .get_arguments()
                .find(|a| a.get_long() == Some(*flag))
                .unwrap_or_else(|| panic!("no --{flag}"));
            assert_eq!(
                arg.get_env()
                    .map(|e| e.to_string_lossy().into_owned())
                    .as_deref(),
                Some(*env),
                "--{flag} must be settable via {env}"
            );
        }
    }

    #[test]
    fn every_flag_accepts_its_sample_value() {
        let cmd = Args::command();
        for (flag, _, value) in FLAG_ENV {
            let mut argv = vec!["edgefirst-camera".to_string(), format!("--{flag}")];
            let switch = cmd
                .get_arguments()
                .find(|a| a.get_long() == Some(*flag))
                .is_some_and(|a| !a.get_action().takes_values());
            // A switch takes no value on the command line, only from the
            // environment.
            if !switch {
                argv.extend(value.split(' ').map(str::to_string));
            }
            Args::try_parse_from(&argv).unwrap_or_else(|e| panic!("--{flag} {value}: {e}"));
        }
    }
}

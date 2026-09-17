// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

use clap::{ArgAction, CommandFactory, Parser};
use serde_json::json;
use std::{fmt, path::PathBuf, str::FromStr};
use zenoh::config::{Config, WhatAmI};

/// A camera mode: named `SIZExFPS` (`1080p30`) or FPS-only (`30FPS`).
///
/// Size names: VGA (640×480), WVGA (800×480), 540p (960×540), 720p
/// (1280×720), 1080p (1920×1080), 4K (3840×2160). Any positive whole
/// FPS is accepted. Combined modes supply both size and FPS; FPS-only
/// leaves resolution to `CAMERA_SIZE` or the live device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraMode {
    /// Named capture size, if this was a combined mode.
    pub size: Option<(u32, u32)>,
    /// Requested frames per second.
    pub fps: u32,
}

impl CameraMode {
    /// Capture size from a combined mode, if any.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.size
    }

    /// Requested frames per second.
    pub fn fps(&self) -> u32 {
        self.fps
    }
}

impl fmt::Display for CameraMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.size {
            Some((w, h)) => write!(f, "{w}x{h}@{fps}", fps = self.fps),
            None => write!(f, "{fps}FPS", fps = self.fps),
        }
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
        "" => Err("camera mode must be SIZExFPS (e.g. 1080p30) or FPS-only (e.g. 30FPS)".into()),
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

    if let Some(fps_str) = lower.strip_suffix("fps") {
        if !fps_str.is_empty() && fps_str.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(CameraMode {
                size: None,
                fps: parse_fps(fps_str)?,
            });
        }
    }

    let fps_start = lower
        .bytes()
        .rposition(|b| !b.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    if fps_start == lower.len() {
        return Err(format!(
            "camera mode '{raw}' is missing a frame rate; use e.g. 1080p30 or 30FPS"
        ));
    }
    if fps_start == 0 {
        return Err(format!(
            "camera mode '{raw}' needs a size name or an FPS suffix; use e.g. 1080p30 or 30FPS"
        ));
    }

    Ok(CameraMode {
        size: Some(named_size(&lower[..fps_start])?),
        fps: parse_fps(&lower[fps_start..])?,
    })
}

/// Camera image mirroring options.
///
/// Determines how the camera image should be flipped before processing.
/// Useful for correcting camera orientation.
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
/// edgefirst-camera --camera /dev/video0 --jpeg --h264
///
/// # Via environment variables
/// export CAMERA=/dev/video0
/// export JPEG=true
/// edgefirst-camera
/// ```
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Camera capture device path (e.g., /dev/video0)
    #[arg(short, long, env = "CAMERA", default_value = "/dev/video3")]
    pub camera: String,

    /// Camera capture resolution in pixels (width height).
    ///
    /// Unset means use the size from `--camera-mode` when that is a
    /// combined `SIZExFPS` value, otherwise probe the live device.
    #[arg(long, env = "CAMERA_SIZE", value_delimiter = ' ', num_args = 2)]
    pub camera_size: Option<Vec<u32>>,

    /// Capture mode: named `SIZExFPS` (`1080p30`) or FPS-only (`30FPS`).
    ///
    /// Combined modes override `--camera-size`. FPS-only leaves resolution
    /// to `--camera-size` or the live device. Unset probes the device for
    /// both size and rate.
    #[arg(long, env = "CAMERA_MODE")]
    pub camera_mode: Option<CameraMode>,

    /// Camera image mirroring setting
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

    /// Record the live H.264 stream to this file (raw Annex-B `.h264`).
    ///
    /// A matching `<path>.json` sidecar is written alongside at startup
    /// carrying colorimetry, `/camera/info`, and `/tf_static` — every
    /// piece of producer-global state that is not recoverable from the
    /// bitstream. Requires `--h264`; mutually exclusive with `--replay`.
    #[arg(long, env = "RECORD", conflicts_with = "replay")]
    pub record: Option<PathBuf>,

    /// Replay a previously recorded H.264 file instead of opening a V4L2
    /// camera device.
    ///
    /// Requires the matching `<path>.json` sidecar alongside the `.h264`
    /// file. Mutually exclusive with `--record`. When enabled, `--jpeg`
    /// and `--h264-tiles` are rejected because the recorded file carries
    /// only the main H.264 bitstream.
    #[arg(long, env = "REPLAY")]
    pub replay: Option<PathBuf>,

    /// Loop the replay back to the start on EOF instead of exiting.
    ///
    /// The `CameraFrame.seq` counter continues to increment across loop
    /// boundaries so consumers see one continuous monotonic stream,
    /// matching the contract of a live camera session.
    #[arg(long, env = "REPLAY_LOOP", default_value_t = false)]
    pub replay_loop: bool,

    /// Override the playback frame rate. Defaults to the sidecar's `fps`.
    #[arg(long, env = "REPLAY_FPS")]
    pub replay_fps: Option<u32>,

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
    /// Capture size requested by configuration, if any.
    ///
    /// A combined `CAMERA_MODE` size wins over `CAMERA_SIZE`. `None`
    /// means probe the live device.
    pub fn requested_capture_size(&self) -> Option<(u32, u32)> {
        if let Some(mode) = &self.camera_mode {
            if let Some(size) = mode.size {
                return Some(size);
            }
        }
        self.camera_size
            .as_ref()
            .and_then(|v| (v.len() >= 2).then_some((v[0], v[1])))
    }

    /// Frame rate requested by `CAMERA_MODE`, if any.
    pub fn requested_capture_fps(&self) -> Option<u32> {
        self.camera_mode.as_ref().map(CameraMode::fps)
    }
}

/// Environment variables where an empty value is meaningful and must be
/// preserved (i.e. the argument has a non-empty default but "" is a
/// documented "disable" sentinel).
///
/// Empty for this service: every option that documents "leave empty to
/// disable" (`CAM_INFO_PATH`, `CONNECT`, `LISTEN`, `RECORD`, `REPLAY`,
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

        config
            .insert_json5("scouting/multicast/interface", &json!("lo").to_string())
            .unwrap();

        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env-bound arguments with a non-empty default where we have
    /// consciously decided that an empty value is NOT meaningful (so
    /// scrubbing to the default is correct).
    const SCRUB_REVIEWED: &[&str] = &[
        "CAMERA",
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

    /// Behavioural check for EDGEAI-1094 on the pure selection: a
    /// numeric, a boolean, an enum and an optional variable exported as
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
        // EDGEAI-1230: running the binary with no options published no
        // camera/h264 stream, because the flag defaulted to off. The
        // service never saw it -- /etc/default/camera sets H264=true --
        // so it only ever bit manual runs.
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
        // EDGEAI-1230: JPEG encoding was a hardcoded quality-100 CPU
        // encode with no knob, which is the bulk of its cost.
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

    /// Every configurable option must be reachable from
    /// `/etc/default/camera`, which systemd applies as an
    /// `EnvironmentFile`. An option with a CLI flag but no `env` binding
    /// is silently unconfigurable there -- it does not error, it just
    /// ignores the setting (EDGEAI-1438).
    #[test]
    fn topic_and_frame_args_are_env_bound() {
        let cmd = Args::command();
        for (id, env) in [
            ("frame_topic", "FRAME_TOPIC"),
            ("info_topic", "INFO_TOPIC"),
            ("jpeg_topic", "JPEG_TOPIC"),
            ("h264_topic", "H264_TOPIC"),
            ("h264_tiles_topics", "H264_TILES_TOPICS"),
            ("base_frame_id", "BASE_FRAME_ID"),
            ("camera_frame_id", "CAMERA_FRAME_ID"),
        ] {
            let arg = cmd
                .get_arguments()
                .find(|a| a.get_id() == id)
                .unwrap_or_else(|| panic!("no such argument: {id}"));
            let bound = arg.get_env().map(|e| e.to_string_lossy().into_owned());
            assert_eq!(
                bound.as_deref(),
                Some(env),
                "--{} must be settable via {env}",
                id.replace('_', "-")
            );
        }
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
            assert_eq!(mode.size, Some((w, h)), "{raw}");
            assert_eq!(mode.fps, fps, "{raw}");
        }
    }

    #[test]
    fn fps_only_modes_leave_size_unset() {
        for raw in ["30FPS", "30fps", "5FPS", " 60FPS "] {
            let mode = CameraMode::from_str(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(mode.size, None, "{raw}");
            assert!(mode.fps > 0, "{raw}");
        }
        assert_eq!(CameraMode::from_str("30FPS").unwrap().fps, 30);
    }

    #[test]
    fn malformed_modes_are_rejected() {
        for raw in [
            "",
            "1080p",
            "30",
            "FPS30",
            "1080p0",
            "QSXGA30",
            "1080p30fps",
        ] {
            assert!(CameraMode::from_str(raw).is_err(), "{raw} must be rejected");
        }
    }

    #[test]
    fn combined_mode_overrides_camera_size() {
        let args = Args::parse_from([
            "edgefirst-camera",
            "--camera-mode",
            "720p30",
            "--camera-size",
            "1920",
            "1080",
        ]);
        assert_eq!(args.requested_capture_size(), Some((1280, 720)));
        assert_eq!(args.requested_capture_fps(), Some(30));
    }

    #[test]
    fn fps_only_mode_uses_camera_size() {
        let args = Args::parse_from([
            "edgefirst-camera",
            "--camera-mode",
            "30FPS",
            "--camera-size",
            "1440",
            "1080",
        ]);
        assert_eq!(args.requested_capture_size(), Some((1440, 1080)));
        assert_eq!(args.requested_capture_fps(), Some(30));
    }

    #[test]
    fn neither_mode_nor_size_means_probe_the_device() {
        let args = Args::parse_from(["edgefirst-camera"]);
        assert_eq!(args.camera_size, None);
        assert_eq!(args.camera_mode, None);
        assert_eq!(args.requested_capture_size(), None);
        assert_eq!(args.requested_capture_fps(), None);
    }

    #[test]
    fn camera_size_alone_requests_size_but_not_fps() {
        let args = Args::parse_from(["edgefirst-camera", "--camera-size", "800", "600"]);
        assert_eq!(args.requested_capture_size(), Some((800, 600)));
        assert_eq!(args.requested_capture_fps(), None);
    }

    #[test]
    fn camera_mode_is_env_bound() {
        let cmd = Args::command();
        let arg = cmd
            .get_arguments()
            .find(|a| a.get_id() == "camera_mode")
            .expect("camera_mode");
        assert_eq!(
            arg.get_env()
                .map(|e| e.to_string_lossy().into_owned())
                .as_deref(),
            Some("CAMERA_MODE")
        );
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

mod args;
mod encode;
mod jpeg;
mod messages;
mod pipeline;
mod source;

use std::{
    env,
    error::Error,
    sync::atomic::{AtomicBool, Ordering},
};

use args::Args;
use clap::Parser;
use edgefirst_image::ImageProcessor;
use messages::{CameraInfoFields, TfStaticFields};
use pipeline::Settings;
use source::{Request, SourceKind};
use tracing::{info, level_filters::LevelFilter, warn};
use tracing_subscriber::{layer::SubscriberExt as _, EnvFilter, Layer as _, Registry};
use zenoh::Wait;

/// Set by SIGTERM or SIGINT (Ctrl+C or Ctrl+Break on Windows); the pipeline
/// stops at the next frame.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "profiling")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(std::alloc::System, 100);

fn get_env_filter() -> EnvFilter {
    tracing_subscriber::EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy()
}

fn main() -> Result<(), Box<dyn Error>> {
    // systemd's EnvironmentFile exports `KEY=""` as an empty string rather
    // than leaving it unset, and clap treats a present-but-empty variable
    // as a supplied value, so `REPLAY_FPS=""` or `JPEG=""` in
    // /etc/default/camera would abort argument parsing. Drop those so the
    // declared defaults apply.
    //
    // SAFETY: single-threaded here; runs before the runtime is built below.
    unsafe { args::scrub_empty_env::<Args>(args::KEEP) };
    let args = Args::parse();
    validate(&args)?;
    if args.tokio_console && env::var("TOKIO_CONSOLE_BIND").is_err() {
        // SAFETY: still single-threaded.
        unsafe { env::set_var("TOKIO_CONSOLE_BIND", "localhost:7000") };
    }

    // SIGTERM and SIGINT are blocked in every thread and taken by one
    // thread with sigwait, so a library that installs its own handler (the
    // Vivante GL driver does) cannot kill the process before it shuts down.
    // Threads inherit the mask, so this runs before any is spawned.
    shutdown_on_signals()?;

    // The runtime serves the tokio console only. Capture runs on this
    // thread, outside it: the image crate's GL converter blocks, which a
    // runtime thread may not.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    args.tracy.then(tracy_client::Client::start);
    let stdout_log = tracing_subscriber::fmt::layer()
        .pretty()
        .with_filter(get_env_filter());
    #[cfg(target_os = "linux")]
    let journald = tracing_journald::layer()
        .ok()
        .map(|j| j.with_filter(get_env_filter()));
    #[cfg(not(target_os = "linux"))]
    let journald: Option<tracing_subscriber::layer::Identity> = None;
    let (console, console_server) = if args.tokio_console {
        let (console, server) = runtime.block_on(async {
            console_subscriber::ConsoleLayer::builder()
                .with_default_env()
                .build()
        });
        (Some(console), Some(server))
    } else {
        (None, None)
    };
    let tracy = args
        .tracy
        .then(|| tracing_tracy::TracyLayer::default().with_filter(get_env_filter()));
    let subscriber = Registry::default()
        .with(stdout_log)
        .with(journald)
        .with(console)
        .with(tracy);
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
    tracing_log::LogTracer::init()?;
    if let Some(server) = console_server {
        runtime.spawn(async move {
            if let Err(e) = server.serve().await {
                warn!("tokio console: {e}");
            }
        });
    }
    if args.replay.is_some() {
        warn!("REPLAY is deprecated; use SOURCE=file:<path>");
    }

    let session = zenoh::open(args.clone())
        .wait()
        .map_err(|e| format!("cannot open the Zenoh session: {e}"))?;

    let request = Request::from_args(&args, source::probe_device)?;
    let processor = match request.kind {
        SourceKind::File => None,
        _ => ImageProcessor::new()
            .map_err(|e| warn!("image crate unavailable for the capture pool: {e}"))
            .ok(),
    };
    let Some(opened) = source::open(&request, processor.as_ref(), &SHUTDOWN)? else {
        return Ok(());
    };
    drop(processor);

    let settings = settings(&args, &request, opened.camera.extras());
    pipeline::run(
        opened,
        &session,
        &settings,
        encode::open_hardware,
        &SHUTDOWN,
    )?;
    info!("Shutdown complete");
    Ok(())
}

/// Blocks SIGTERM and SIGINT in this thread, and so in every thread it
/// spawns, and sets [`SHUTDOWN`] from a thread that waits for them.
#[cfg(unix)]
fn shutdown_on_signals() -> std::io::Result<()> {
    // SAFETY: the set is initialised by sigemptyset before use, and
    // pthread_sigmask only reads it.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc));
        }
        set
    };
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || loop {
            let mut signal = 0;
            // SAFETY: `set` and `signal` are valid for the call.
            if unsafe { libc::sigwait(&set, &mut signal) } != 0 {
                continue;
            }
            let name = if signal == libc::SIGTERM {
                "SIGTERM"
            } else {
                "SIGINT"
            };
            info!("Received {name}, initiating graceful shutdown");
            SHUTDOWN.store(true, Ordering::SeqCst);
        })?;
    Ok(())
}

/// Sets [`SHUTDOWN`] from a thread that waits for Ctrl+C or Ctrl+Break.
#[cfg(windows)]
fn shutdown_on_signals() -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    // Registering before the thread starts means an early Ctrl+C is not
    // missed.
    let mut ctrl_c = runtime.block_on(async { tokio::signal::windows::ctrl_c() })?;
    let mut ctrl_break = runtime.block_on(async { tokio::signal::windows::ctrl_break() })?;
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || loop {
            let name = runtime.block_on(async {
                tokio::select! {
                    _ = ctrl_c.recv() => "Ctrl+C",
                    _ = ctrl_break.recv() => "Ctrl+Break",
                }
            });
            info!("Received {name}, initiating graceful shutdown");
            SHUTDOWN.store(true, Ordering::SeqCst);
        })?;
    Ok(())
}

/// Rejects argument combinations that cannot work, before anything opens.
fn validate(args: &Args) -> Result<(), Box<dyn Error>> {
    if let Some(path) = &args.record {
        if !args.h264 {
            return Err(format!(
                "--record {} requires --h264 (record writes the main H.264 stream)",
                path.display()
            )
            .into());
        }
        if args.is_file_source() {
            return Err(format!(
                "--record {} needs a camera; {} is a recording already",
                path.display(),
                args.source_spec()
            )
            .into());
        }
    }
    Ok(())
}

/// The pipeline settings for `args`. A recording's `camera_info` and
/// `tf_static` (its `extras`) win over the command line's.
pub(crate) fn settings(args: &Args, request: &Request, extras: Option<&str>) -> Settings {
    let mut info = CameraInfoFields::from_args(args);
    let mut tf = TfStaticFields::from_args(args);
    if let Some(extras) = extras {
        let (recorded_info, recorded_tf) = messages::from_extras(extras);
        if let Some(i) = recorded_info {
            info = i;
        }
        if let Some(t) = recorded_tf {
            tf = t;
        }
    }
    Settings {
        frame_topic: args.frame_topic.clone(),
        info_topic: args.info_topic.clone(),
        jpeg_topic: args.jpeg_topic.clone(),
        h264_topic: args.h264_topic.clone(),
        tile_topics: args.h264_tiles_topics.clone(),
        frame_id: args.camera_frame_id.clone(),
        jpeg: args.jpeg.then_some(args.jpeg_quality),
        h264: args.h264,
        tiles: args.h264_tiles,
        tile_fps: args.h264_tiles_fps,
        bitrate: encode::bitrate(args.h264_bitrate),
        stream_size: (args.stream_size[0], args.stream_size[1]),
        record: args.record.clone(),
        requested_fps: request.fps,
        info,
        tf,
        tracy: args.tracy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(argv: &[&str]) -> Args {
        Args::parse_from([&["edgefirst-camera"][..], argv].concat())
    }

    #[test]
    fn plain_capture_validates() {
        validate(&args(&[])).unwrap();
    }

    #[test]
    fn record_requires_h264() {
        let err = validate(&args(&["--record", "a.h264", "--h264", "false"])).unwrap_err();
        assert!(err.to_string().contains("--h264"), "{err}");
        validate(&args(&["--record", "a.h264"])).unwrap();
    }

    #[test]
    fn record_needs_a_camera() {
        let err = validate(&args(&["--source", "file:b.h264", "--record", "a.h264"])).unwrap_err();
        assert!(err.to_string().contains("recording already"), "{err}");
    }

    #[test]
    fn a_recordings_metadata_wins() {
        let a = args(&["--camera-frame-id", "live"]);
        let r = Request::from_args(&a, |_| unreachable!()).unwrap();
        let extras = r#"{"tf_static": {"base_frame_id": "b", "child_frame_id": "rec",
            "translation": [1, 2, 3], "rotation": [0, 0, 0, 1]}}"#;
        let s = settings(&a, &r, Some(extras));
        assert_eq!(s.tf.child_frame_id, "rec");
        assert_eq!(s.info.frame_id, "live", "no recorded camera_info");
        assert_eq!(settings(&a, &r, None).tf.child_frame_id, "live");
    }
}

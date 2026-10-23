// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

//! End-to-end checks of the environment: `KEY=""` behaves as unset, and
//! every variable parses as its flag does.
//!
//! Runs with `harness = false` so this `main` is the only thread in the
//! process when the environment is mutated, which `scrub_empty_env` and
//! `set_var` require.
//!
//! `main` speaks the small subset of the libtest CLI that `cargo test` and
//! `cargo nextest` use to enumerate (`--list --format terse`) and select
//! (`--exact <name>`, `--ignored`, `--skip <pattern>`, positional filters)
//! tests, so the target is discovered and reported like any other test.
#![allow(dead_code, unused_imports)] // args.rs's own #[cfg(test)] unit tests are compiled but never run here

// `Args` lives in a private module of the binary, so include it directly.
#[path = "../src/args.rs"]
mod args;
use args::{scrub_empty_env, Args, MemorySetting, MirrorSetting, FLAG_ENV, KEEP};
use clap::{CommandFactory, Parser};

/// The tests this binary provides, as reported to the harness.
const TESTS: [(&str, fn()); 3] = [
    (
        "empty_env_is_treated_as_unset",
        empty_env_is_treated_as_unset,
    ),
    (
        "every_env_var_parses_as_its_flag",
        every_env_var_parses_as_its_flag,
    ),
    (
        "default_files_start_the_service",
        default_files_start_the_service,
    ),
];

/// The `/etc/default/camera` shipped with 2.12, and the current one.
const DEFAULT_FILES: [(&str, &str); 2] = [
    ("2.12", include_str!("data/camera-2.12.default")),
    ("current", include_str!("../camera.default")),
];

/// Numeric, boolean, enum and optional (no default) arguments, all
/// written as `KEY=""` in /etc/default/camera.
const VARS: [&str; 6] = [
    "JPEG_QUALITY",
    "H264",
    "MIRROR",
    "REPLAY_FPS",
    "CAMERA_MODE",
    "CAMERA_SIZE",
];
const ARGV: [&str; 1] = ["edgefirst-camera"];

/// libtest flags that consume the following argument, so it is not a filter.
const VALUE_FLAGS: [&str; 5] = [
    "--test-threads",
    "--format",
    "--logfile",
    "--color",
    "--shuffle-seed",
];

/// What the harness asked this binary to do.
struct Request {
    list: bool,
    ignored: bool,
    exact: bool,
    filters: Vec<String>,
    skips: Vec<String>,
}

fn parse_request(argv: impl IntoIterator<Item = String>) -> Request {
    let mut req = Request {
        list: false,
        ignored: false,
        exact: false,
        filters: Vec::new(),
        skips: Vec::new(),
    };
    let mut argv = argv.into_iter();
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--list" => req.list = true,
            "--ignored" => req.ignored = true,
            "--exact" => req.exact = true,
            "--skip" => req.skips.extend(argv.next()),
            flag if flag.starts_with("--skip=") => {
                req.skips.push(flag["--skip=".len()..].to_owned());
            }
            flag if VALUE_FLAGS.contains(&flag) => {
                argv.next();
            }
            flag if flag.starts_with('-') => {}
            filter => req.filters.push(filter.to_owned()),
        }
    }
    req
}

/// Whether `pattern` matches test `name`: equality under `--exact`,
/// otherwise a substring match, as libtest does for both filters and
/// `--skip` patterns.
fn matches(req: &Request, name: &str, pattern: &str) -> bool {
    if req.exact {
        pattern == name
    } else {
        name.contains(pattern)
    }
}

/// Selected when it passes the positional filter (any filter matches, or
/// there is none) and no `--skip` pattern matches.
fn selected(req: &Request, name: &str) -> bool {
    (req.filters.is_empty() || req.filters.iter().any(|f| matches(req, name, f)))
        && !req.skips.iter().any(|s| matches(req, name, s))
}

/// Unsets every variable the tests touch.
fn clear_env() {
    for (_, name, _) in FLAG_ENV {
        // SAFETY: single-threaded — this is `main` before any thread is spawned.
        std::env::remove_var(name);
    }
    for name in VARS.iter().chain(["REPLAY"].iter()) {
        // SAFETY: as above.
        std::env::remove_var(name);
    }
}

fn main() {
    let req = parse_request(std::env::args().skip(1));
    // This binary has no #[ignore]d tests, so `--ignored` selects nothing.
    for (name, test) in TESTS {
        if req.ignored || !selected(&req, name) {
            continue;
        }
        if req.list {
            println!("{name}: test");
            continue;
        }
        clear_env();
        test();
        clear_env();
        println!("{name}: ok");
    }
}

fn empty_env_is_treated_as_unset() {
    for name in VARS {
        // SAFETY: single-threaded — this is `main` before any thread is spawned.
        std::env::set_var(name, "");
    }
    // A real value must survive scrubbing untouched.
    std::env::set_var("H264_TILES_FPS", "7");
    let before = Args::try_parse_from(ARGV);
    assert!(
        before.is_err(),
        "empty vars must fail to parse before scrubbing: {before:?}"
    );

    // SAFETY: still single-threaded.
    unsafe { scrub_empty_env::<Args>(KEEP) };
    for name in VARS {
        assert!(
            std::env::var_os(name).is_none(),
            "{name} should have been removed"
        );
    }
    let args = Args::try_parse_from(ARGV).expect("defaults must apply after scrubbing");
    assert_eq!(args.jpeg_quality, 85);
    assert!(args.h264);
    assert_eq!(args.mirror, MirrorSetting::Both);
    assert_eq!(args.replay_fps, None);
    assert_eq!(args.camera_mode, None);
    assert_eq!(args.camera_size, None);
    assert_eq!(args.h264_tiles_fps, 7);
}

/// Each variable alone gives the same arguments as its flag alone.
fn every_env_var_parses_as_its_flag() {
    let cmd = Args::command();
    for (flag, env, value) in FLAG_ENV {
        let mut argv = vec!["edgefirst-camera".to_string(), format!("--{flag}")];
        let switch = cmd
            .get_arguments()
            .find(|a| a.get_long() == Some(*flag))
            .is_some_and(|a| !a.get_action().takes_values());
        if !switch {
            argv.extend(value.split(' ').map(str::to_string));
        }
        let by_flag = Args::try_parse_from(&argv).unwrap_or_else(|e| panic!("--{flag}: {e}"));
        // SAFETY: single-threaded — this is `main` before any thread is spawned.
        std::env::set_var(env, value);
        let by_env = Args::try_parse_from(ARGV).unwrap_or_else(|e| panic!("{env}={value}: {e}"));
        // SAFETY: as above.
        std::env::remove_var(env);
        assert_eq!(
            format!("{by_flag:?}"),
            format!("{by_env:?}"),
            "{env}={value} must parse as --{flag}"
        );
    }
}

/// The `KEY=VALUE` pairs of an EnvironmentFile, quotes stripped as systemd
/// strips them.
fn environment_file(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with(';'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.trim_matches('"').to_string()))
        .collect()
}

/// Both default files start the service as 2.12 did: the camera device,
/// H.264 on, mirrored, four buffers. Every variable they set is one this
/// service reads, or the libcamera one.
fn default_files_start_the_service() {
    let bound: Vec<String> = Args::command()
        .get_arguments()
        .filter_map(|a| a.get_env().map(|e| e.to_string_lossy().into_owned()))
        .collect();
    for (name, text) in DEFAULT_FILES {
        clear_env();
        for (key, value) in environment_file(text) {
            assert!(
                bound.contains(&key) || key == "LIBCAMERA_PIPELINES_MATCH_LIST",
                "{name}: {key} is not read by this service"
            );
            // SAFETY: single-threaded — this is `main` before any thread is spawned.
            std::env::set_var(&key, &value);
        }
        // SAFETY: still single-threaded.
        unsafe { scrub_empty_env::<Args>(KEEP) };
        let args = Args::try_parse_from(ARGV).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(args.source_spec(), "/dev/video3", "{name}");
        assert!(args.h264, "{name}");
        assert!(!args.jpeg, "{name}");
        assert_eq!(args.mirror, MirrorSetting::Both, "{name}");
        assert_eq!(args.camera_buffers, 4, "{name}");
        assert_eq!(args.memory, MemorySetting::Auto, "{name}");
        assert_eq!(args.stream_size, [1920, 1080], "{name}");
        assert_eq!(args.explicit_size(), None, "{name}");
        assert_eq!(args.explicit_fps(), None, "{name}");
        assert_eq!(args.camera_mode, None, "{name}");
        for (key, _) in environment_file(text) {
            // SAFETY: as above.
            std::env::remove_var(key);
        }
    }
}

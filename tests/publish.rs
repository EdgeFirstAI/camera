// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Runs the `edgefirst-camera` binary on the mock source and subscribes to
//! it over Zenoh, as a downstream service would. Needs no camera, so it runs
//! on every host, macOS and Windows included.

use edgefirst_schemas::edgefirst_msgs::CameraFrame;
use std::{
    collections::BTreeMap,
    error::Error,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use zenoh::{
    bytes::Encoding,
    qos::{CongestionControl, Priority},
    Wait,
};

mod common;

use common::{camera_bin, free_endpoint, stop, subscriber_session};

type BoxError = Box<dyn Error + Send + Sync>;

const FRAMES: usize = 100;
/// Covers a cold debug build's startup on a hosted runner.
const DEADLINE: Duration = Duration::from_secs(60);

#[test]
fn a_subscriber_receives_camera_frames() -> Result<(), BoxError> {
    let endpoint = free_endpoint()?;
    let topic = format!("edgefirst-camera-test-{}/frame", std::process::id());
    let mut child = Command::new(camera_bin())
        .args([
            "--source",
            "mock:320x240@30",
            "--h264",
            "false",
            "--frame-topic",
            &topic,
            "--listen",
            &endpoint,
            "--no-multicast-scouting",
        ])
        .stdin(Stdio::null())
        .spawn()?;

    let received = (|| -> Result<Vec<CameraFrame<Vec<u8>>>, BoxError> {
        let session = subscriber_session(&endpoint)?;
        // The binary namespaces its keys with the host name.
        let subscriber = session.declare_subscriber(format!("**/{topic}")).wait()?;
        let deadline = Instant::now() + DEADLINE;
        let mut frames = Vec::with_capacity(FRAMES);
        while frames.len() < FRAMES {
            if let Some(status) = child.try_wait()? {
                return Err(format!("exited early with {status}").into());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("{} of {FRAMES} frames in {DEADLINE:?}", frames.len()).into());
            }
            if let Ok(Some(sample)) = subscriber.recv_timeout(left.min(Duration::from_secs(1))) {
                frames.push(CameraFrame::from_cdr(
                    sample.payload().to_bytes().into_owned(),
                )?);
            }
        }
        Ok(frames)
    })();
    let stopped = stop(child);
    let frames = received?;
    stopped?;

    for frame in &frames {
        let tensor = frame.tensor();
        assert_eq!(tensor.shape().collect::<Vec<_>>(), [240, 320, 2]);
        assert!(tensor.num_planes() >= 1);
        for plane in tensor.planes() {
            // Off Linux the capture pool is process memory, which only
            // travels inline. On Linux it may be DMA-BUF, sent by reference.
            if cfg!(not(target_os = "linux")) {
                assert!(plane.is_inline(), "plane sent by reference: {plane:?}");
            }
            if plane.is_inline() {
                assert!(!plane.data.is_empty(), "inline plane without data");
            }
        }
    }
    let seqs: Vec<_> = frames.iter().map(CameraFrame::seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "out of order: {seqs:?}"
    );
    Ok(())
}

/// No H.264 encoder off Linux: the default `--h264` stops the service at
/// startup and says how to run without it.
#[cfg(not(target_os = "linux"))]
#[test]
fn h264_without_an_encoder_fails_at_startup() -> Result<(), BoxError> {
    use std::io::Read as _;

    let mut child = Command::new(camera_bin())
        .args([
            "--source",
            "mock:64x48@30",
            "--stream-size",
            "64",
            "48",
            "--listen",
            &free_endpoint()?,
            "--no-multicast-scouting",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let Some(status) = common::wait_with_deadline(&mut child, DEADLINE) else {
        child.kill()?;
        child.wait()?;
        return Err(format!("still running after {DEADLINE:?} with no encoder").into());
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .ok_or("stderr not captured")?
        .read_to_string(&mut stderr)?;
    assert!(!status.success(), "started without an encoder: {stderr}");
    assert!(stderr.contains("--h264 false"), "{stderr}");
    Ok(())
}

/// Whether `info` finds an H.264 encoder on this host.
fn encoder_available() -> Result<bool, BoxError> {
    let out = Command::new(camera_bin())
        .args(["info", "--json"])
        .output()?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned().into());
    }
    let info: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    Ok(info["encoder"]["available"]
        .as_bool()
        .ok_or("info has no encoder flag")?)
}

/// What a subscriber sees of one topic.
#[derive(Debug, Clone, PartialEq)]
struct Published {
    encoding: Encoding,
    priority: Priority,
    congestion: CongestionControl,
}

/// Every topic on its default key under the host namespace, with its
/// schema in the encoding and the QoS the service declares for it.
#[test]
fn topics_carry_their_schema_key_and_qos() -> Result<(), BoxError> {
    let h264 = encoder_available()?;
    let endpoint = free_endpoint()?;
    let mut expected = BTreeMap::from([
        (
            "camera/frame",
            Published {
                encoding: Encoding::APPLICATION_CDR.with_schema("edgefirst_msgs/msg/CameraFrame"),
                priority: Priority::Data,
                congestion: CongestionControl::Drop,
            },
        ),
        (
            "camera/info",
            Published {
                encoding: Encoding::APPLICATION_CDR.with_schema("sensor_msgs/msg/CameraInfo"),
                priority: Priority::Background,
                congestion: CongestionControl::Drop,
            },
        ),
        (
            "camera/jpeg",
            Published {
                encoding: Encoding::APPLICATION_CDR.with_schema("sensor_msgs/msg/CompressedImage"),
                priority: Priority::Data,
                congestion: CongestionControl::Drop,
            },
        ),
    ]);
    if h264 {
        expected.insert(
            "camera/h264",
            Published {
                encoding: Encoding::APPLICATION_CDR
                    .with_schema("foxglove_msgs/msg/CompressedVideo"),
                priority: Priority::Data,
                congestion: CongestionControl::Drop,
            },
        );
    }
    let tf_static = Encoding::APPLICATION_CDR.with_schema("geometry_msgs/msg/TransformStamped");

    let h264_flag = if h264 { "true" } else { "false" };
    let mut child = Command::new(camera_bin())
        .args([
            "--source",
            "mock:320x240@30",
            "--stream-size",
            "320",
            "240",
            "--jpeg",
            "--h264",
            h264_flag,
            "--listen",
            &endpoint,
            "--no-multicast-scouting",
        ])
        .stdin(Stdio::null())
        .spawn()?;

    let seen = (|| -> Result<BTreeMap<String, Published>, BoxError> {
        let session = subscriber_session(&endpoint)?;
        let subscriber = session.declare_subscriber("**").wait()?;
        let deadline = Instant::now() + DEADLINE;
        let mut seen = BTreeMap::new();
        let complete = |seen: &BTreeMap<String, Published>| {
            seen.len() == expected.len() + 1
                && seen.keys().any(|k| k.ends_with("/tf_static"))
                && expected
                    .keys()
                    .all(|t| seen.keys().any(|k| k.ends_with(&format!("/{t}"))))
        };
        while !complete(&seen) {
            if let Some(status) = child.try_wait()? {
                return Err(format!("exited early with {status}").into());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "in {DEADLINE:?} saw only {:?}",
                    seen.keys().collect::<Vec<_>>()
                )
                .into());
            }
            if let Ok(Some(sample)) = subscriber.recv_timeout(left.min(Duration::from_secs(1))) {
                seen.entry(sample.key_expr().as_str().to_owned())
                    .or_insert_with(|| Published {
                        encoding: sample.encoding().clone(),
                        priority: sample.priority(),
                        congestion: sample.congestion_control(),
                    });
            }
        }
        Ok(seen)
    })();
    let stopped = stop(child);
    let seen = seen?;
    stopped?;

    // The service namespaces its keys with the host name, which is one key
    // chunk, so every key is `<namespace>/<topic>`.
    let mut namespaces = std::collections::BTreeSet::new();
    for (key, published) in &seen {
        let (namespace, topic) = key
            .split_once('/')
            .ok_or_else(|| format!("{key} has no namespace"))?;
        namespaces.insert(namespace);
        if topic == "tf_static" {
            assert_eq!(published.encoding, tf_static, "{key}");
            continue;
        }
        let want = expected
            .get(topic)
            .ok_or_else(|| format!("unexpected key {key}"))?;
        assert_eq!(published, want, "{key}");
    }
    assert_eq!(
        namespaces.len(),
        1,
        "keys under several namespaces: {seen:?}"
    );
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    if !host.is_empty() && !host.contains('/') {
        assert_eq!(namespaces.first(), Some(&host.as_str()), "{seen:?}");
    }
    Ok(())
}

/// SIGTERM and SIGINT are blocked in every thread, so they reach only the
/// thread that waits for them and no library handler can act on them first.
/// That thread, `signals`, reads as unblocked while it sits in `sigwait`,
/// which unblocks the awaited signals for the duration of the wait.
#[cfg(target_os = "linux")]
#[test]
fn every_thread_blocks_the_shutdown_signals() -> Result<(), BoxError> {
    let endpoint = free_endpoint()?;
    let mut child = Command::new(camera_bin())
        .args([
            "--source",
            "mock:320x240@30",
            "--stream-size",
            "320",
            "240",
            "--jpeg",
            "--h264",
            "false",
            "--listen",
            &endpoint,
            "--no-multicast-scouting",
        ])
        .stdin(Stdio::null())
        .spawn()?;
    let pid = child.id();

    let unblocked = (|| -> Result<Vec<String>, BoxError> {
        // A published frame means every pipeline thread has started.
        let session = subscriber_session(&endpoint)?;
        let subscriber = session.declare_subscriber("**/camera/frame").wait()?;
        let deadline = Instant::now() + DEADLINE;
        let mut frames = 0;
        while frames < 10 {
            if let Some(status) = child.try_wait()? {
                return Err(format!("exited early with {status}").into());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("{frames} frames in {DEADLINE:?}").into());
            }
            if let Ok(Some(_)) = subscriber.recv_timeout(left.min(Duration::from_secs(1))) {
                frames += 1;
            }
        }

        let wanted: u64 = (1 << (libc::SIGTERM - 1)) | (1 << (libc::SIGINT - 1));
        let mut unblocked = Vec::new();
        let mut threads = 0;
        let mut waiter = false;
        for task in std::fs::read_dir(format!("/proc/{pid}/task"))? {
            let status = match std::fs::read_to_string(task?.path().join("status")) {
                Ok(status) => status,
                // The thread exited while the tasks were listed.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let field = |name: &str| {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix(name))
                    .map(str::trim)
                    .unwrap_or_default()
                    .to_owned()
            };
            let blocked = u64::from_str_radix(&field("SigBlk:"), 16)?;
            threads += 1;
            if field("Name:") == "signals" {
                waiter = true;
                continue;
            }
            if blocked & wanted != wanted {
                unblocked.push(format!("{} (SigBlk {:016x})", field("Name:"), blocked));
            }
        }
        assert!(threads > 1, "only {threads} thread found");
        assert!(waiter, "no `signals` thread waits for the shutdown signals");
        Ok(unblocked)
    })();
    let stopped = stop(child);
    // An unblocked thread is also why a stop fails, so it is reported first.
    let unblocked = unblocked?;
    assert!(
        unblocked.is_empty(),
        "threads that do not block SIGTERM and SIGINT: {unblocked:?}"
    );
    stopped?;
    Ok(())
}

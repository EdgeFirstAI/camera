// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Measures a running camera service from the outside, so 2.x and 3.0 are
//! measured the same way, and compares two measurements.
//!
//! `parity measure` subscribes to the camera's topics for a while, as a
//! downstream service would, and writes a JSON report: each topic's key,
//! encoding, rate and latency (arrival time minus header stamp, both
//! `CLOCK_REALTIME`), whether its stamps share the frame clock, the frame
//! tensor's layout and the H.264 stream's structure. With `--pid` it also
//! samples that process's CPU time and resident memory.
//!
//! `parity compare` prints two reports side by side as Markdown, with the
//! acceptance targets. `scripts/parity.sh` drives both on a board.

use clap::{Parser, Subcommand};
use edgefirst_schemas::{edgefirst_msgs::CameraFrame, foxglove_msgs::FoxgloveCompressedVideo};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    error::Error,
    fs::File,
    io::Write as _,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zenoh::Wait;

type BoxError = Box<dyn Error + Send + Sync>;

/// Frames per second the frame and H.264 topics must sustain.
const TARGET_FPS: f64 = 29.5;
/// 95th-percentile latency targets in milliseconds.
const TARGET_FRAME_MS: f64 = 5.0;
const TARGET_JPEG_MS: f64 = 50.0;
const TARGET_H264_MS: f64 = 100.0;
const TARGET_RSS_MB: f64 = 200.0;
/// Percent of one core.
const TARGET_CPU: f64 = 25.0;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Subscribe to the camera's topics and write a JSON report to stdout.
    Measure {
        /// How long to measure, in seconds.
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        /// The camera service's process, for CPU time and resident memory.
        #[arg(long)]
        pid: Option<u32>,
        /// Write the H.264 stream here, for decoding elsewhere.
        #[arg(long)]
        h264_out: Option<PathBuf>,
        /// Key expressions to subscribe to; by default this host's
        /// `camera/**` and `tf_static`, so other boards on the network are
        /// left out.
        #[arg(long, num_args = 1..)]
        keys: Vec<String>,
        /// A label for the report, such as the version measured.
        #[arg(long, default_value = "")]
        label: String,
    },
    /// Print two reports side by side as Markdown.
    Compare {
        before: PathBuf,
        after: PathBuf,
        /// The SDK `probe` example's JSON from the same camera with the
        /// service stopped: the driver's delivery delay, which
        /// `camera/frame`'s latency includes but the service cannot change.
        #[arg(long)]
        floor: Option<PathBuf>,
    },
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Stats {
    count: usize,
    mean: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

impl Stats {
    fn of(mut values: Vec<f64>) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        values.sort_by(f64::total_cmp);
        let at = |q: f64| values[((values.len() - 1) as f64 * q).round() as usize];
        Some(Self {
            count: values.len(),
            mean: values.iter().sum::<f64>() / values.len() as f64,
            p50: at(0.50),
            p95: at(0.95),
            p99: at(0.99),
            max: values[values.len() - 1],
        })
    }
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct FrameLayout {
    shape: Vec<u64>,
    format: String,
    planes: u32,
    inline_planes: u32,
    /// Frames missing from the sequence numbers received.
    seq_gaps: u64,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct H264Layout {
    format: String,
    messages: usize,
    idr: usize,
    /// IDR messages that carry their SPS and PPS in band.
    idr_with_parameter_sets: usize,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct TopicReport {
    key: String,
    encoding: String,
    count: usize,
    bytes_mean: f64,
    fps: f64,
    /// Arrival minus header stamp, in milliseconds.
    latency_ms: Option<Stats>,
    /// Largest difference between the Zenoh sample timestamp and the header
    /// stamp, in microseconds; `None` when samples carry no timestamp.
    sample_vs_header_us: Option<f64>,
    /// Share of header stamps equal to a `camera/frame` stamp.
    frame_stamp_match: Option<f64>,
    publishers: usize,
    frame: Option<FrameLayout>,
    h264: Option<H264Layout>,
    errors: BTreeSet<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct ProcessReport {
    pid: u32,
    cpu_percent: f64,
    rss_mb_mean: f64,
    rss_mb_max: f64,
    threads: u64,
    exited: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct Report {
    label: String,
    seconds: u64,
    topics: Vec<TopicReport>,
    process: Option<ProcessReport>,
}

struct Received {
    key: String,
    encoding: String,
    payload: Vec<u8>,
    arrival_ns: i128,
    timestamp_ns: Option<(i128, String)>,
}

#[derive(Default)]
struct Topic {
    report: TopicReport,
    first: Option<Instant>,
    last: Option<Instant>,
    bytes: u64,
    latency: Vec<f64>,
    stamps: Vec<i128>,
    publishers: HashSet<String>,
    last_seq: Option<u64>,
}

fn realtime_ns() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i128)
}

/// The header stamp of a CDR message whose header leads it, as
/// nanoseconds; `TFMessage` leads with its transform count.
fn header_stamp(encoding: &str, payload: &[u8]) -> Option<i128> {
    let at = if encoding.contains("TFMessage") { 8 } else { 4 };
    let little = *payload.get(1)? == 1;
    let word = |o: usize| -> Option<[u8; 4]> { payload.get(o..o + 4)?.try_into().ok() };
    let (sec, nsec) = if little {
        (
            i32::from_le_bytes(word(at)?),
            u32::from_le_bytes(word(at + 4)?),
        )
    } else {
        (
            i32::from_be_bytes(word(at)?),
            u32::from_be_bytes(word(at + 4)?),
        )
    };
    Some(i128::from(sec) * 1_000_000_000 + i128::from(nsec))
}

/// NAL unit types in an Annex-B buffer.
fn nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            types.push(data[i + 3] & 0x1f);
            i += 3;
        } else {
            i += 1;
        }
    }
    types
}

struct ProcSampler {
    pid: u32,
    start: Instant,
    start_ticks: Option<u64>,
    end_ticks: Option<u64>,
    rss_kb: Vec<f64>,
    threads: u64,
    exited: bool,
}

impl ProcSampler {
    fn new(pid: u32) -> Self {
        let mut s = Self {
            pid,
            start: Instant::now(),
            start_ticks: None,
            end_ticks: None,
            rss_kb: Vec::new(),
            threads: 0,
            exited: false,
        };
        s.start_ticks = s.ticks();
        s
    }

    /// utime plus stime, from the fields after the parenthesised name.
    fn ticks(&self) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.pid)).ok()?;
        let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
        Some(fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?)
    }

    fn sample(&mut self) {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", self.pid)) else {
            self.exited = true;
            return;
        };
        for line in status.lines() {
            let mut words = line.split_whitespace();
            match words.next() {
                Some("VmRSS:") => {
                    if let Some(kb) = words.next().and_then(|v| v.parse().ok()) {
                        self.rss_kb.push(kb);
                    }
                }
                Some("Threads:") => {
                    if let Some(n) = words.next().and_then(|v| v.parse().ok()) {
                        self.threads = n;
                    }
                }
                _ => {}
            }
        }
        if let Some(t) = self.ticks() {
            self.end_ticks = Some(t);
        }
    }

    fn report(self) -> ProcessReport {
        #[cfg(unix)]
        // SAFETY: sysconf reads a constant and has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        #[cfg(not(unix))]
        let hz = 100.0;
        let ticks = match (self.start_ticks, self.end_ticks) {
            (Some(a), Some(b)) => b.saturating_sub(a) as f64,
            _ => 0.0,
        };
        let rss = Stats::of(self.rss_kb).unwrap_or_default();
        ProcessReport {
            pid: self.pid,
            cpu_percent: 100.0 * ticks / hz / self.start.elapsed().as_secs_f64(),
            rss_mb_mean: rss.mean / 1024.0,
            rss_mb_max: rss.max / 1024.0,
            threads: self.threads,
            exited: self.exited,
        }
    }
}

fn record(topic: &mut Topic, msg: Received, h264_out: &mut Option<File>) {
    let now = Instant::now();
    let t = &mut topic.report;
    t.key.clone_from(&msg.key);
    t.encoding.clone_from(&msg.encoding);
    t.count += 1;
    topic.first.get_or_insert(now);
    topic.last = Some(now);
    topic.bytes += msg.payload.len() as u64;

    let stamp = header_stamp(&msg.encoding, &msg.payload);
    if let Some(stamp) = stamp {
        topic.latency.push((msg.arrival_ns - stamp) as f64 / 1e6);
        topic.stamps.push(stamp);
    }
    if let Some((ts, id)) = msg.timestamp_ns {
        topic.publishers.insert(id);
        if let Some(stamp) = stamp {
            let us = ((ts - stamp) as f64 / 1e3).abs();
            let worst = t.sample_vs_header_us.get_or_insert(0.0);
            *worst = worst.max(us);
        }
    }

    if msg.key.ends_with("camera/frame") {
        match CameraFrame::from_cdr(msg.payload.as_slice()) {
            Ok(frame) => {
                let tensor = frame.tensor();
                let layout = t.frame.get_or_insert_with(Default::default);
                layout.shape = tensor.shape().collect();
                layout.format = tensor.format().to_owned();
                layout.planes = tensor.num_planes();
                layout.inline_planes = tensor.planes().filter(|p| p.is_inline()).count() as u32;
                let seq = frame.seq();
                if let Some(last) = topic.last_seq {
                    layout.seq_gaps += seq.saturating_sub(last + 1);
                }
                topic.last_seq = Some(seq);
            }
            Err(e) => {
                t.errors.insert(format!("CameraFrame: {e}"));
            }
        }
    } else if msg.encoding.contains("CompressedVideo") {
        match FoxgloveCompressedVideo::from_cdr(msg.payload.as_slice()) {
            Ok(video) => {
                let layout = t.h264.get_or_insert_with(Default::default);
                layout.format = video.format().to_owned();
                layout.messages += 1;
                let types = nal_types(video.data());
                if types.contains(&5) {
                    layout.idr += 1;
                    if types.contains(&7) && types.contains(&8) {
                        layout.idr_with_parameter_sets += 1;
                    }
                }
                if msg.key.ends_with("camera/h264") {
                    if let Some(out) = h264_out {
                        if let Err(e) = out.write_all(video.data()) {
                            t.errors.insert(format!("writing H.264: {e}"));
                        }
                    }
                }
            }
            Err(e) => {
                t.errors.insert(format!("CompressedVideo: {e}"));
            }
        }
    }
}

fn measure(
    seconds: u64,
    pid: Option<u32>,
    h264_out: Option<PathBuf>,
    keys: &[String],
    label: String,
) -> Result<Report, BoxError> {
    let keys = if keys.is_empty() {
        let host = gethostname::gethostname().to_string_lossy().into_owned();
        vec![format!("{host}/camera/**"), format!("{host}/tf_static")]
    } else {
        keys.to_vec()
    };
    let session = zenoh::open(zenoh::Config::default()).wait()?;
    let (tx, rx) = mpsc::channel::<Received>();
    let mut subscribers = Vec::new();
    for key in &keys {
        let tx = tx.clone();
        subscribers.push(
            session
                .declare_subscriber(key.as_str())
                .callback(move |sample| {
                    let timestamp_ns = sample.timestamp().map(|ts| {
                        let ntp = ts.get_time();
                        let ns = i128::from(ntp.as_secs()) * 1_000_000_000
                            + i128::from(ntp.subsec_nanos());
                        (ns, ts.get_id().to_string())
                    });
                    let _ = tx.send(Received {
                        key: sample.key_expr().to_string(),
                        encoding: sample.encoding().to_string(),
                        payload: sample.payload().to_bytes().into_owned(),
                        arrival_ns: realtime_ns(),
                        timestamp_ns,
                    });
                })
                .wait()?,
        );
    }
    drop(tx);

    let mut h264_out = h264_out.map(File::create).transpose()?;
    let mut topics: BTreeMap<String, Topic> = BTreeMap::new();
    let mut proc = pid.map(ProcSampler::new);
    let start = Instant::now();
    let end = start + Duration::from_secs(seconds);
    let mut next_sample = start;
    while Instant::now() < end {
        if Instant::now() >= next_sample {
            if let Some(p) = proc.as_mut() {
                p.sample();
            }
            next_sample += Duration::from_secs(1);
        }
        let wait = next_sample
            .min(end)
            .saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(msg) => record(
                topics.entry(msg.key.clone()).or_default(),
                msg,
                &mut h264_out,
            ),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(subscribers);

    let frame_stamps: HashSet<i128> = topics
        .iter()
        .filter(|(k, _)| k.ends_with("camera/frame"))
        .flat_map(|(_, t)| t.stamps.iter().copied())
        .collect();
    let topics = topics
        .into_values()
        .map(|t| {
            let mut r = t.report;
            if let (Some(first), Some(last)) = (t.first, t.last) {
                let span = last.duration_since(first).as_secs_f64();
                if span > 0.0 {
                    r.fps = (r.count - 1) as f64 / span;
                }
            }
            r.bytes_mean = t.bytes as f64 / r.count.max(1) as f64;
            r.latency_ms = Stats::of(t.latency);
            r.publishers = t.publishers.len();
            if !frame_stamps.is_empty() && !t.stamps.is_empty() && !r.key.ends_with("tf_static") {
                let hits = t.stamps.iter().filter(|s| frame_stamps.contains(s)).count();
                r.frame_stamp_match = Some(hits as f64 / t.stamps.len() as f64);
            }
            r
        })
        .collect();
    Ok(Report {
        label,
        seconds,
        topics,
        process: proc.map(ProcSampler::report),
    })
}

/// A topic's key without the host namespace in front of it.
fn short_key(key: &str) -> &str {
    ["camera/", "tf_static", "model/", "fusion/"]
        .iter()
        .find_map(|k| key.find(k).map(|i| &key[i..]))
        .unwrap_or(key)
}

/// The driver's delivery delay in milliseconds, from the `probe` example.
#[derive(Deserialize)]
struct Floor {
    latency_ms_p50: f64,
    latency_ms_p99: f64,
}

fn compare(before: &Report, after: &Report, floor: Option<&Floor>) -> String {
    let mut out = String::new();
    let index = |r: &Report| -> BTreeMap<String, TopicReport> {
        r.topics
            .iter()
            .map(|t| (short_key(&t.key).to_owned(), t.clone()))
            .collect()
    };
    let (a, b) = (index(before), index(after));
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    let pass = |ok: bool| if ok { "pass" } else { "FAIL" };
    let opt = |v: Option<f64>, digits: usize| v.map_or("-".to_owned(), |v| format!("{v:.digits$}"));

    out += &format!(
        "## Topics\n\n| Topic | {0} encoding | {1} encoding | {0} fps | {1} fps | {0} p50/p95 ms | {1} p50/p95 ms | {1} target |\n|---|---|---|---|---|---|---|---|\n",
        before.label, after.label
    );
    for key in &keys {
        let (x, y) = (a.get(*key), b.get(*key));
        let enc = |t: Option<&TopicReport>| {
            t.map_or("absent".to_owned(), |t| format!("`{}`", t.encoding))
        };
        let fps = |t: Option<&TopicReport>| opt(t.map(|t| t.fps), 2);
        let lat = |t: Option<&TopicReport>| {
            t.and_then(|t| t.latency_ms.as_ref())
                .map_or("-".to_owned(), |l| format!("{:.2} / {:.2}", l.p50, l.p95))
        };
        let target = y.map_or("-".to_owned(), |t| {
            let p95 = t.latency_ms.as_ref().map_or(f64::INFINITY, |l| l.p95);
            match short_key(&t.key) {
                "camera/frame" => match (floor, &t.latency_ms) {
                    (Some(f), Some(l)) => {
                        let own = l.p50 - f.latency_ms_p50;
                        format!(
                            "{} (≥{TARGET_FPS} fps, {own:.2} ms after delivery at p50, <{TARGET_FRAME_MS} ms)",
                            pass(t.fps >= TARGET_FPS && own < TARGET_FRAME_MS)
                        )
                    }
                    _ => format!(
                        "{} (≥{TARGET_FPS} fps, p95 <{TARGET_FRAME_MS} ms)",
                        pass(t.fps >= TARGET_FPS && p95 < TARGET_FRAME_MS)
                    ),
                },
                "camera/jpeg" => {
                    format!("{} (p95 <{TARGET_JPEG_MS} ms)", pass(p95 < TARGET_JPEG_MS))
                }
                "camera/h264" => format!(
                    "{} (≥{TARGET_FPS} fps, p95 <{TARGET_H264_MS} ms)",
                    pass(t.fps >= TARGET_FPS && p95 < TARGET_H264_MS)
                ),
                _ => "-".to_owned(),
            }
        });
        out += &format!(
            "| `{key}` | {} | {} | {} | {} | {} | {} | {target} |\n",
            enc(x),
            enc(y),
            fps(x),
            fps(y),
            lat(x),
            lat(y)
        );
    }

    out += &format!(
        "\n## Clock domain\n\n| Topic | {0} stamps on a frame | {1} stamps on a frame | {0} sample−header µs | {1} sample−header µs |\n|---|---|---|---|---|\n",
        before.label, after.label
    );
    for key in &keys {
        let (x, y) = (a.get(*key), b.get(*key));
        let m = |t: Option<&TopicReport>| {
            opt(t.and_then(|t| t.frame_stamp_match).map(|v| v * 100.0), 1)
        };
        let s = |t: Option<&TopicReport>| opt(t.and_then(|t| t.sample_vs_header_us), 0);
        out += &format!(
            "| `{key}` | {}% | {}% | {} | {} |\n",
            m(x),
            m(y),
            s(x),
            s(y)
        );
    }

    out += "\n## Frame and H.264 layout\n\n";
    for (label, r) in [(&before.label, &a), (&after.label, &b)] {
        if let Some(f) = r.get("camera/frame").and_then(|t| t.frame.as_ref()) {
            out += &format!(
                "- {label} `camera/frame`: shape {:?}, format `{}`, {} plane(s), {} inline, {} sequence gaps\n",
                f.shape, f.format, f.planes, f.inline_planes, f.seq_gaps
            );
        }
        if let Some(h) = r.get("camera/h264").and_then(|t| t.h264.as_ref()) {
            out += &format!(
                "- {label} `camera/h264`: format `{}`, {} messages, {} IDR, {} IDR with SPS and PPS in band\n",
                h.format, h.messages, h.idr, h.idr_with_parameter_sets
            );
        }
        for t in r.values() {
            for e in &t.errors {
                out += &format!("- {label} `{}`: error: {e}\n", short_key(&t.key));
            }
        }
    }

    if let Some(f) = floor {
        out += &format!(
            "\nDriver delivery, capture stamp to dequeue with the service stopped: p50 {:.2} ms, p99 {:.2} ms. `camera/frame` latency includes it.\n",
            f.latency_ms_p50, f.latency_ms_p99
        );
    }

    if before.process.is_none() && after.process.is_none() {
        return out;
    }
    out += &format!(
        "\n## Process\n\n| | {0} | {1} | {1} target |\n|---|---|---|---|\n",
        before.label, after.label
    );
    let p = |r: &Report, f: fn(&ProcessReport) -> f64| opt(r.process.as_ref().map(f), 1);
    out += &format!(
        "| CPU, % of one core | {} | {} | {} (<{TARGET_CPU}%) |\n",
        p(before, |p| p.cpu_percent),
        p(after, |p| p.cpu_percent),
        pass(
            after
                .process
                .as_ref()
                .is_some_and(|p| p.cpu_percent < TARGET_CPU)
        )
    );
    out += &format!(
        "| Resident memory, max MB | {} | {} | {} (<{TARGET_RSS_MB} MB) |\n",
        p(before, |p| p.rss_mb_max),
        p(after, |p| p.rss_mb_max),
        pass(
            after
                .process
                .as_ref()
                .is_some_and(|p| p.rss_mb_max < TARGET_RSS_MB)
        )
    );
    out += &format!(
        "| Threads | {} | {} | |\n",
        p(before, |p| p.threads as f64),
        p(after, |p| p.threads as f64)
    );
    out
}

fn main() -> Result<(), BoxError> {
    match Cli::parse().command {
        Cmd::Measure {
            seconds,
            pid,
            h264_out,
            keys,
            label,
        } => {
            let report = measure(seconds, pid, h264_out, &keys, label)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::Compare {
            before,
            after,
            floor,
        } => {
            let read = |p: &PathBuf| -> Result<Report, BoxError> {
                Ok(serde_json::from_slice(&std::fs::read(p)?)?)
            };
            let floor: Option<Floor> = floor
                .map(|p| -> Result<Floor, BoxError> {
                    Ok(serde_json::from_slice(&std::fs::read(p)?)?)
                })
                .transpose()?;
            print!(
                "{}",
                compare(&read(&before)?, &read(&after)?, floor.as_ref())
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_stamp_reads_little_endian_cdr() {
        let mut cdr = vec![0, 1, 0, 0];
        cdr.extend(5i32.to_le_bytes());
        cdr.extend(7u32.to_le_bytes());
        assert_eq!(header_stamp("x", &cdr), Some(5_000_000_007));
        let mut tf = vec![0, 1, 0, 0, 1, 0, 0, 0];
        tf.extend(2i32.to_le_bytes());
        tf.extend(0u32.to_le_bytes());
        assert_eq!(
            header_stamp("tf2_msgs/msg/TFMessage", &tf),
            Some(2_000_000_000)
        );
    }

    #[test]
    fn nal_types_finds_both_start_code_lengths() {
        let data = [0, 0, 0, 1, 0x67, 9, 0, 0, 1, 0x68, 9, 0, 0, 1, 0x65, 9];
        assert_eq!(nal_types(&data), [7, 8, 5]);
    }

    #[test]
    fn short_key_drops_the_namespace() {
        assert_eq!(short_key("rt/host/camera/frame"), "camera/frame");
        assert_eq!(short_key("rt/tf_static"), "tf_static");
        assert_eq!(short_key("host/model/info"), "model/info");
    }
}

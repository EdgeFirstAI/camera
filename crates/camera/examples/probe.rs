// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Probes a camera for platform results pages and pull requests.
//!
//! ```text
//! cargo run -p edgefirst-camera --example probe -- --source /dev/video3 \
//!     --frames 300 --close-test --json
//! ```
//!
//! Reports the driver and BSP identity, the negotiated format and pitch, what
//! `Auto` resolved to, contiguity, the frame rate and drops measured over
//! `--frames`, dequeue latency (time from the capture timestamp to the frame
//! reaching the caller), and the timestamp clock and source. `--close-test`
//! holds a frame across close, writes a canary into it, reopens and
//! captures for a second, and checks the canary survived.
//!
//! Options: `--source <spec>` (required), `--frames <n>` (300),
//! `--size <WxH>`, `--format <fourcc>` (`YUYV`, `NV12`, ...), `--fps <f>`,
//! `--memory auto|import|export`, `--contiguity required|any`,
//! `--buffers <n>`, `--close-test`, `--json`.

use std::time::{Duration, Instant};

use edgefirst_camera::{
    CameraBuilder, CaptureClock, Contiguity, Frame, MemoryStrategy, ResolvedMemory, Result,
};
use edgefirst_tensor::{CpuAccess, PixelFormat};

#[derive(Debug)]
struct Args {
    source: String,
    frames: usize,
    size: Option<(u32, u32)>,
    format: Option<PixelFormat>,
    fps: Option<f64>,
    memory: MemoryStrategy,
    contiguity: Contiguity,
    buffers: usize,
    close_test: bool,
    json: bool,
}

fn usage(why: &str) -> ! {
    eprintln!("probe: {why}");
    eprintln!(
        "usage: probe --source <spec> [--frames N] [--size WxH] [--format FOURCC] [--fps F] \
         [--memory auto|import|export] [--contiguity required|any] [--buffers N] \
         [--close-test] [--json]"
    );
    std::process::exit(2);
}

fn args() -> Args {
    let mut a = Args {
        source: String::new(),
        frames: 300,
        size: None,
        format: None,
        fps: None,
        memory: MemoryStrategy::Auto,
        contiguity: Contiguity::Required,
        buffers: 4,
        close_test: false,
        json: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .unwrap_or_else(|| usage(&format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "--source" => a.source = value(),
            "--frames" => a.frames = value().parse().unwrap_or_else(|_| usage("bad --frames")),
            "--size" => {
                let v = value();
                let (w, h) = v.split_once('x').unwrap_or_else(|| usage("--size is WxH"));
                a.size = Some((
                    w.parse().unwrap_or_else(|_| usage("bad width")),
                    h.parse().unwrap_or_else(|_| usage("bad height")),
                ));
            }
            "--format" => {
                let v = value();
                a.format = Some(
                    PixelFormat::from_str_code(&v)
                        .unwrap_or_else(|| usage(&format!("unknown format {v}"))),
                );
            }
            "--fps" => a.fps = Some(value().parse().unwrap_or_else(|_| usage("bad --fps"))),
            "--memory" => {
                a.memory = match value().as_str() {
                    "auto" => MemoryStrategy::Auto,
                    "import" => MemoryStrategy::Import,
                    "export" => MemoryStrategy::Export,
                    _ => usage("--memory is auto, import or export"),
                }
            }
            "--contiguity" => {
                a.contiguity = match value().as_str() {
                    "required" => Contiguity::Required,
                    "any" => Contiguity::Any,
                    _ => usage("--contiguity is required or any"),
                }
            }
            "--buffers" => a.buffers = value().parse().unwrap_or_else(|_| usage("bad --buffers")),
            "--close-test" => a.close_test = true,
            "--json" => a.json = true,
            "-h" | "--help" => usage("help"),
            other => usage(&format!("unknown argument {other}")),
        }
    }
    if a.source.is_empty() {
        usage("--source is required");
    }
    a
}

fn builder(a: &Args) -> Result<CameraBuilder> {
    let mut b = CameraBuilder::source(&a.source)?
        .memory(a.memory)
        .contiguity(a.contiguity)
        .buffers(a.buffers);
    if let Some((w, h)) = a.size {
        b = b.size(w, h);
    }
    if let Some(f) = a.format {
        b = b.format(f);
    }
    if let Some(fps) = a.fps {
        b = b.frame_rate(fps);
    }
    Ok(b)
}

/// `CLOCK_MONOTONIC` now, in nanoseconds.
fn monotonic_nanos() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid timespec for the call.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

fn os_release() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_owned())
        })
        .unwrap_or_default()
}

fn kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// A minimal JSON object writer: strings, numbers, booleans and nulls.
struct Json(Vec<(String, String)>);

impl Json {
    fn str(&mut self, k: &str, v: &str) {
        let escaped: String = v
            .chars()
            .flat_map(|c| match c {
                '"' => vec!['\\', '"'],
                '\\' => vec!['\\', '\\'],
                c if c.is_control() => format!("\\u{:04x}", c as u32).chars().collect(),
                c => vec![c],
            })
            .collect();
        self.0.push((k.into(), format!("\"{escaped}\"")));
    }
    fn num(&mut self, k: &str, v: f64) {
        self.0.push((
            k.into(),
            if v.is_finite() {
                format!("{v}")
            } else {
                "null".into()
            },
        ));
    }
    fn raw(&mut self, k: &str, v: impl std::fmt::Display) {
        self.0.push((k.into(), v.to_string()));
    }
    fn render(&self) -> String {
        let body: Vec<String> = self
            .0
            .iter()
            .map(|(k, v)| format!("  \"{k}\": {v}"))
            .collect();
        format!("{{\n{}\n}}", body.join(",\n"))
    }
}

fn close_test(a: &Args, held: Frame) -> Result<(bool, f64)> {
    // The camera that produced `held` is already closed. Nothing may write
    // into the frame any more, so a canary written now must survive a
    // reopen and a second of capture.
    let canary: Vec<u8> = (0..4096u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let n = {
        let mut map = held.map_bytes(CpuAccess::ReadWrite)?;
        let n = canary.len().min(map.len());
        map[..n].copy_from_slice(&canary[..n]);
        n
    };
    let t = Instant::now();
    let mut camera = builder(a)?.open()?;
    let reopen_ms = t.elapsed().as_secs_f64() * 1e3;
    camera.start()?;
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        drop(camera.next_frame(Some(Duration::from_secs(2)))?);
    }
    drop(camera);
    let intact = held.map_bytes(CpuAccess::Read)?[..n] == canary[..n];
    Ok((intact, reopen_ms))
}

fn main() -> Result<()> {
    let a = args();
    let mut camera = builder(&a)?.open()?;
    let d = camera.descriptor().clone();
    camera.start()?;
    let c = camera.config().clone();

    let mut latencies = Vec::with_capacity(a.frames);
    let mut first: Option<(i64, Instant)> = None;
    let mut last_ts = 0i64;
    let mut clock = CaptureClock::Unknown;
    let mut source = edgefirst_camera::TimestampSource::Unknown;
    let mut held = None;
    for i in 0..a.frames {
        let f = camera.next_frame(Some(Duration::from_secs(5)))?;
        let ts = f.timestamp();
        (clock, source) = (ts.clock, ts.source);
        if ts.clock == CaptureClock::Monotonic {
            latencies.push((monotonic_nanos() - ts.nanos) as f64 / 1e6);
        }
        first.get_or_insert((ts.nanos, Instant::now()));
        last_ts = ts.nanos;
        if i + 1 == a.frames {
            held = Some(f);
        }
    }
    let stats = camera.stats();
    let span_s = first.map_or(0.0, |(t0, _)| (last_ts - t0) as f64 / 1e9);
    let fps = if span_s > 0.0 {
        (a.frames.saturating_sub(1)) as f64 / span_s
    } else {
        f64::NAN
    };
    drop(camera);

    let close = match (a.close_test, held) {
        (true, Some(f)) => Some(close_test(&a, f)?),
        _ => None,
    };

    latencies.sort_by(f64::total_cmp);
    let mut j = Json(Vec::new());
    j.str("source", &a.source);
    j.str("backend", &format!("{:?}", d.backend));
    j.str("driver", &d.driver);
    j.str("card", &d.name);
    j.str("kernel", &kernel());
    j.str("os", &os_release());
    j.str("format", &format!("{:?}", c.format));
    j.raw("width", c.width);
    j.raw("height", c.height);
    j.raw("row_stride", c.row_stride);
    j.raw("planes", c.planes);
    j.raw("buffers", c.buffer_count);
    j.str("memory_requested", &format!("{:?}", a.memory));
    j.str(
        "memory",
        match c.memory {
            ResolvedMemory::Import => "Import",
            ResolvedMemory::Export => "Export",
        },
    );
    j.raw(
        "contiguous",
        c.contiguous.map_or("null".into(), |b| b.to_string()),
    );
    j.num("fps_negotiated", c.frame_rate.unwrap_or(f64::NAN));
    j.num("fps_measured", fps);
    j.raw("frames", stats.frames);
    j.raw("dropped", stats.dropped);
    j.raw("errored", stats.errors);
    j.num("latency_ms_p50", percentile(&latencies, 0.5));
    j.num("latency_ms_p99", percentile(&latencies, 0.99));
    j.str("timestamp_clock", &format!("{clock:?}"));
    j.str("timestamp_source", &format!("{source:?}"));
    if let Some((intact, reopen_ms)) = close {
        j.raw("close_canary_intact", intact);
        j.num("reopen_ms", reopen_ms);
    }
    if a.json {
        println!("{}", j.render());
    } else {
        for (k, v) in &j.0 {
            println!("{k:>20}: {}", v.trim_matches('"'));
        }
    }
    if close.is_some_and(|(intact, _)| !intact) {
        std::process::exit(1);
    }
    Ok(())
}

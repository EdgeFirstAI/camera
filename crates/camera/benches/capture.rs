// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Capture benchmarks against a live V4L2 camera.
//!
//! ```text
//! EDGEFIRST_CAMERA_BENCH_SOURCE=/dev/video3 cargo bench -p edgefirst-camera --bench capture
//! ```
//!
//! - `frame_interval`: one `next_frame` call, so the wait for the next frame;
//!   at a steady rate this is the frame period and its spread is the jitter.
//! - `delivery_latency`: time from the frame's capture timestamp to
//!   `next_frame` returning it. Only measured on a `CLOCK_MONOTONIC`
//!   timestamp; vivid stamps frames about one period ahead of delivery, so
//!   it reads near zero there.
//! - `import`: importing a captured frame's DMA-BUF as a tensor (`dup` and
//!   `TensorDyn::from_fd`), the step a consumer in another process repeats.
//! - `cpu_read`: mapping a frame and reading every byte, which shows whether
//!   the capture memory is CPU-cached.
//!
//! The source is `EDGEFIRST_CAMERA_BENCH_SOURCE`, or else the first V4L2
//! capture node found. `EDGEFIRST_CAMERA_BENCH_SIZE` (`WxH`) and
//! `EDGEFIRST_CAMERA_BENCH_FORMAT` (`NV12`, `YUYV`, ...) are requests the
//! device may adjust; the benchmark names carry what was applied. Buffers
//! are contiguous when the platform has a CMA heap and system-heap
//! otherwise. With no camera the benchmarks print `SKIPPED` and exit.

#[cfg(all(target_os = "linux", feature = "v4l2"))]
mod linux {
    use std::hint::black_box;
    use std::time::Duration;

    use criterion::{Criterion, SamplingMode};
    use edgefirst_camera::{
        Backend, Camera, CameraBuilder, CaptureClock, Contiguity, ErrorKind, ResolvedMemory,
    };
    use edgefirst_tensor::{CpuAccess, DType, PixelFormat, TensorDyn, TensorMemory};

    const TIMEOUT: Option<Duration> = Some(Duration::from_secs(5));

    fn source() -> Option<String> {
        if let Ok(s) = std::env::var("EDGEFIRST_CAMERA_BENCH_SOURCE") {
            return Some(s);
        }
        edgefirst_camera::enumerate()
            .ok()?
            .into_iter()
            .find(|d| d.backend == Backend::V4l2)
            .map(|d| d.id)
    }

    fn builder(source: &str) -> CameraBuilder {
        let mut b = CameraBuilder::source(source).expect("a V4L2 source");
        if let Ok(size) = std::env::var("EDGEFIRST_CAMERA_BENCH_SIZE") {
            let (w, h) = size
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                .expect("EDGEFIRST_CAMERA_BENCH_SIZE is WxH");
            b = b.size(w, h);
        }
        if let Ok(f) = std::env::var("EDGEFIRST_CAMERA_BENCH_FORMAT") {
            let format = PixelFormat::from_str_code(&f)
                .unwrap_or_else(|| panic!("unknown EDGEFIRST_CAMERA_BENCH_FORMAT {f}"));
            b = b.format(format);
        }
        b
    }

    /// Opens and starts the camera with contiguous buffers, or with any
    /// buffers on a platform without a CMA heap.
    fn start(source: &str) -> Box<dyn Camera> {
        let mut camera = builder(source).open().expect("open the camera");
        match camera.start() {
            Ok(()) => camera,
            Err(e) if e.kind() == ErrorKind::ContiguousUnavailable => {
                eprintln!("note: no contiguous buffers ({e}); benchmarking system-heap buffers");
                drop(camera);
                let mut camera = builder(source)
                    .contiguity(Contiguity::Any)
                    .open()
                    .expect("open the camera");
                camera.start().expect("start the camera");
                camera
            }
            Err(e) => panic!("start the camera: {e}"),
        }
    }

    fn monotonic_nanos() -> i64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: a valid timespec for the call.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        ts.tv_sec * 1_000_000_000 + ts.tv_nsec
    }

    pub fn main() {
        let Some(source) = source() else {
            eprintln!("SKIPPED: no V4L2 camera; set EDGEFIRST_CAMERA_BENCH_SOURCE");
            return;
        };
        let mut camera = start(&source);
        let c = camera.config().clone();
        let memory = match c.memory {
            ResolvedMemory::Import => "import",
            ResolvedMemory::Export => "export",
        };
        let label = format!(
            "{:?}-{}x{}-{memory}-{}",
            c.format,
            c.width,
            c.height,
            match c.contiguous {
                Some(true) => "cma",
                Some(false) => "system",
                None => "unknown-heap",
            }
        );
        eprintln!(
            "benchmarking {source} ({}): {label}",
            camera.descriptor().driver
        );

        let mut criterion = Criterion::default().sample_size(20).configure_from_args();

        let mut group = criterion.benchmark_group(format!("capture/{label}"));
        group.sampling_mode(SamplingMode::Flat);
        group.measurement_time(Duration::from_secs(10));
        group.bench_function("frame_interval", |b| {
            b.iter(|| drop(camera.next_frame(TIMEOUT).expect("a frame")))
        });
        // Frames that filled while nothing dequeued are as late as the pause
        // was long; dequeue them so latency is measured on fresh frames.
        let queued = c.buffer_count;
        let drain = |camera: &mut Box<dyn Camera>| {
            for _ in 0..queued {
                drop(camera.next_frame(TIMEOUT).expect("a frame"));
            }
        };
        drain(&mut camera);
        // The latency of a few frames, or `None` when timestamps are not
        // CLOCK_MONOTONIC and cannot be compared with now.
        let lateness: Option<Vec<i64>> = (0..5)
            .map(|_| {
                let f = camera.next_frame(TIMEOUT).expect("a frame");
                let ts = f.timestamp();
                (ts.clock == CaptureClock::Monotonic)
                    .then(|| monotonic_nanos().saturating_sub(ts.nanos))
            })
            .collect();
        match lateness {
            None => eprintln!("note: timestamps are not CLOCK_MONOTONIC; no delivery_latency"),
            Some(l) if l.iter().any(|&n| n <= 0) => {
                eprintln!("note: timestamps do not all precede delivery ({l:?} ns late); no delivery_latency")
            }
            Some(_) => {
                group.bench_function("delivery_latency", |b| {
                    b.iter_custom(|iters| {
                        drain(&mut camera);
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let f = camera.next_frame(TIMEOUT).expect("a frame");
                            let late = monotonic_nanos().saturating_sub(f.timestamp().nanos);
                            total += Duration::from_nanos(late.max(0) as u64);
                        }
                        total
                    })
                });
            }
        }
        group.finish();

        let frame = camera.next_frame(TIMEOUT).expect("a frame");
        let mut group = criterion.benchmark_group(format!("frame/{label}"));
        if frame.tensor().memory() == TensorMemory::DmaBuf {
            let len = frame.bytes_used();
            group.bench_function("import", |b| {
                b.iter(|| {
                    let fd = frame.tensor().dmabuf_clone().expect("dup the DMA-BUF");
                    black_box(TensorDyn::from_fd(fd, &[len], DType::U8, None).expect("import"))
                })
            });
        } else {
            eprintln!(
                "note: frames are {:?}, not DMA-BUF; no import",
                frame.tensor().memory()
            );
        }
        group.bench_function("cpu_read", |b| {
            b.iter(|| {
                let bytes = frame.map_bytes(CpuAccess::Read).expect("map the frame");
                black_box(
                    bytes
                        .iter()
                        .fold(0u32, |a, &v| a.wrapping_add(u32::from(v))),
                )
            })
        });
        group.finish();
        drop(frame);
        drop(camera);
        criterion.final_summary();
    }
}

fn main() {
    #[cfg(all(target_os = "linux", feature = "v4l2"))]
    linux::main();
    #[cfg(not(all(target_os = "linux", feature = "v4l2")))]
    eprintln!("SKIPPED: the capture benchmarks need Linux and the v4l2 feature");
}

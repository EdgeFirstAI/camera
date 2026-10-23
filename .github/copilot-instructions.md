# AI Assistant Development Guidelines - EdgeFirst Camera

**Purpose:** Project-specific instructions for AI coding assistants (GitHub Copilot, Claude, Cursor, etc.)

**Organization Standards:** See SPS repository for Au-Zone universal rules (Git/JIRA workflow, license policy, security practices)

**Version:** 3.0
**Last Updated:** 2026-03-12

---

## Overview

This file provides **project-specific** guidelines for the EdgeFirst Camera project. ALL projects must also follow:

- **Organization-wide:** SPS `05-copilot-instructions.md` - License policy, security, Git/JIRA workflow
- **Process docs:** SPS `00-README.md` through `11-cicd-pipelines.md`
- **This file:** Camera-specific conventions, architecture, hardware details, testing

**Hierarchy:** Org standards (mandatory) → SPS processes (required) → This file (camera-specific)

---

## Git Workflow

**Branch:** `<type>/EDGEAI-###[-desc]` (feature/bugfix/hotfix/release, JIRA key required)
**Commit:** `EDGEAI-###: Brief description` (50-72 chars, what done not how)
**PR:** main=2 approvals, develop=1. Link JIRA, squash features, merge commits for releases.

---

## CRITICAL RULES

### #1: NEVER Use cd Commands

```bash
# Modern tools work from project root
cargo build --release
cargo test --workspace
cargo clippy -- -D warnings

# AI loses context with cd
cd src && cargo build  # Where are we now?
```

### #2: Code Quality Standards

- **Rust:** Latest stable (edition 2021), `cargo fmt`, `cargo clippy -- -D warnings`
- **Performance:** Edge-first (512MB-2GB RAM, ARM64, <50ms latency, 5-10yr lifecycle)
- **Testing:** 70% min coverage, 80%+ for core modules (pipeline, encode, source)
- **Hardware:** Profile on target NXP i.MX8 platforms

---

## License Policy (ZERO TOLERANCE)

**Allowed:** MIT, Apache-2.0, BSD-2/3, ISC, 0BSD, Unlicense, Zlib, EPL-2.0
**Conditional:** MPL-2.0 (deps ONLY), LGPL (**FORBIDDEN in Rust**)
**BLOCKED:** GPL, AGPL, SSPL, Commons Clause

**SBOM:** `make sbom` (scancode→CycloneDX). CI/CD blocks violations.

---

## Security

**Input:** Validate all, allowlists, size limits
**Creds:** NEVER hardcode. Env vars (ephemeral <48h) or vaults.
**Scans:** cargo audit, gitleaks. All MUST pass.
**Report:** support@au-zone.com "Security Vulnerability"

---

## Documentation

**Mandatory:** README, ARCHITECTURE, SECURITY, CHANGELOG, LICENSE, NOTICE
**API:** 100% public API coverage (rustdoc) with Args/Returns/Errors/Examples
**Comments:** Public APIs, complex logic, performance, thread safety, hardware-specific

---

## EdgeFirst Camera - Project-Specific Guidelines

### Technology Stack

- **Language**: Rust (edition 2021)
- **Build system**: Cargo with workspace configuration
- **Key dependencies**:
  - `zenoh 1.10.1` - Transport and ROS2 bridge compatibility
  - `tokio 1.53.1` - Async runtime with multi-threaded executor
  - `edgefirst-camera` (workspace crate `crates/camera`) - Camera SDK: V4L2, `file:` and `mock` sources, capture buffers, frame-to-`CameraFrame` mapping
  - `edgefirst-image 0.36` - HAL image conversion (`ImageProcessor` picks OpenGL, then G2D, then CPU)
  - `edgefirst-codec 0.36` - HAL H.264 `VideoEncoder` over V4L2 memory-to-memory (vsi_v4l2 on i.MX 8M Plus); the SDK uses its decoder for `file:` sources
  - `edgefirst-tensor 0.36` - HAL tensors that frames are delivered as
  - `edgefirst-schemas 4.0.0` - ROS2 message schemas with zero-copy CDR builders
  - `tracing-tracy` - Performance profiling (Tracy profiler integration)
  - `turbojpeg` - JPEG encoding with SIMD (require-simd feature)
  - `kanal` - Fast bounded SPSC/MPMC channels for inter-thread communication
  - `clap` - CLI argument parsing with derive and env features
- **Target platforms**:
  - Primary: Linux aarch64 (NXP i.MX8M Plus, ARM Cortex-A53/A72)
  - Secondary: Linux x86_64 (development and testing)
  - Development: macOS and Windows, on the `mock` source with `--h264 false`
  - **Not supported**: RISC-V
- **Cross-platform**: Camera capture and H.264 need Linux (V4L2 capture, a V4L2 H.264 encoder); image conversion uses whatever the HAL finds (OpenGL, G2D or CPU). On macOS and Windows the application runs on the `mock` source; keep journald behind `cfg(target_os = "linux")` and Unix signal code behind `cfg(unix)`

### Architecture

- **Pattern**: Producer-consumer pipeline with hardware acceleration
- **Components**:
  - Source (SDK) → Capture loop → Encoder threads → Publishing
  - The capture loop runs on the main thread, outside the tokio runtime (which serves signals and tokio-console only); JPEG, H.264 and each 4K tile encoder run on threads of their own, fed over bounded channels
- **Data flow**:
  - Camera (SDK, V4L2) → DMA-BUF frame → `camera/frame` by reference, and → HAL conversion where needed → JPEG (turbojpeg) / H.264 (HAL `VideoEncoder`) → Zenoh
- **Key source files** (the application is binary-only, package `edgefirst-camera-app`, binary `edgefirst-camera`):
  - `src/main.rs` - Startup, argument validation, logging, Zenoh session
  - `src/args.rs` - CLI arguments with clap (env var support for all options)
  - `src/source.rs` - Opening the source, ISP-not-ready retry, capture pool, mirroring (camera first, encoders when the camera cannot)
  - `src/pipeline.rs` - Capture loop, publishers, encoder threads, drop reports, tf_static
  - `src/encode.rs` - H.264 main stream and 4K tiles
  - `src/jpeg.rs` - JPEG compression
  - `src/messages.rs` - camera/info, tf_static, compressed payloads, Zenoh sample timestamps
  - `crates/camera/` - The `edgefirst-camera` SDK (see its README and ARCHITECTURE)
- **Error handling**:
  - Uses `Result<T, Box<dyn Error>>` pattern
  - Logging via `tracing` with a journald layer on Linux
  - Periodic drop reports for camera and encoder drops
  - Graceful shutdown on SIGTERM/SIGINT (Ctrl+C/Ctrl+Break on Windows)
  - Bounded retry of consecutive camera read timeouts

### Build and Deployment

```bash
# ALWAYS run before committing:
cargo fmt
cargo clippy -- -D warnings

# Build for target (use zigbuild on non-Linux or when cross-compiling):
cargo zigbuild --target aarch64-unknown-linux-gnu --release  # cross-compile (macOS/x86_64 → aarch64)
cargo build --release                                        # native build (Linux x86_64 or aarch64)

# Run tests (hardware tests are ignored by default)
cargo test --workspace

# Run only the application's unit tests (no hardware required)
cargo test --bins

# Generate documentation
cargo doc --no-deps --open

# Run the SDK capture benchmarks (requires a V4L2 camera)
cargo bench -p edgefirst-camera --bench capture

# Build with profiling enabled
cargo zigbuild --target aarch64-unknown-linux-gnu --release --profile=profiling --features=profiling

# Generate SBOM and check license policy
make sbom

# Pre-release checks (format, lint, test, sbom, version verify)
make pre-release

# Coverage report
cargo llvm-cov --all-features --workspace --html
```

**Build Verification:** Always run `cargo fmt` and `cargo clippy -- -D warnings` before building. Use `cargo zigbuild --target aarch64-unknown-linux-gnu` when developing on a non-Linux platform (macOS, Windows) or when cross-compiling for the aarch64 target. Native `cargo build` works on Linux, macOS and Windows; off Linux the application runs only on the `mock` source with `--h264 false`.

**Yocto Integration:**

- Deployed via Yocto recipes in separate meta-edgefirst repository
- Binary installed to `/usr/bin/edgefirst-camera`
- Typically run as systemd service on Maivin/Raivin platforms
- Configuration via `camera.default` EnvironmentFile for systemd

**CI/CD Workflows** (`.github/workflows/`):

- `ci.yml` - Tiered CI: **Quick** on every PR including drafts (`ci-gate` before review), **Full** with label `ci:full`, board lane with `ci:hardware` (not on ordinary pushes)
- `nightly.yml` - Scheduled Full + advisories + Sonar main baseline
- `camera-full.yml` - Full-tier macOS and Windows lanes for the SDK and the application on the `mock` source (`tests/publish.rs`), called by `ci.yml` (`ci:full`) and `nightly.yml`; the shared macOS and Windows lanes stay skipped because their nextest-args are the Linux set
- `build.yml` - Release binaries on version tags (native aarch64 + x86_64)
- `sbom.yml` - Full scancode SBOM on release tags
- `release.yml` - Automated release with binary artifacts

**Development Tips:**

- Use `tracy` profiler for performance analysis (feature enabled by default)
- Optional `tokio-console` for async task debugging
- Most hardware-dependent tests are `#[ignore]` by default - use `cargo test -- --ignored` on device
- Cross-compilation uses `cargo-zigbuild` (`.cargo/config.toml` is no longer used)
- **Always run `cargo fmt` and `cargo clippy -- -D warnings`** before building or committing
- **Always use `cargo zigbuild --target aarch64-unknown-linux-gnu`** when on a non-Linux platform or cross-compiling

### CLI Arguments and Environment Variables

All options can be set via command line or environment variables. Environment variables use short names (no prefix). See `camera.default` for the complete reference.

**Key environment variable mapping:**

| CLI Flag | Env Var | Default | Description |
|----------|---------|---------|-------------|
| `--source` | `SOURCE` | unset (uses `CAMERA`) | Device path, `file:<name>.h264` or `mock[:WxH@fps]` |
| `--camera` | `CAMERA` | `/dev/video3` | V4L2 device path, used when `SOURCE` is unset |
| `--memory` | `MEMORY` | `auto` | Capture buffer allocation (auto/import/export) |
| `--camera-size` | `CAMERA_SIZE` | unset (keep device's) | Requested capture resolution |
| `--camera-fps` | `CAMERA_FPS` | unset (keep device's) | Requested capture rate; playback rate for `file:` sources |
| `--camera-mode` | `CAMERA_MODE` | unset | `SIZE+FPS` such as `1080p30`, checked against the camera's probed modes; devices only |
| `--camera-buffers` | `CAMERA_BUFFERS` | `4` | Capture buffers (2-32); 2 more are added while H.264, tiles or JPEG is on |
| `--exposure` | `EXPOSURE` | unset | `auto` or microseconds |
| `--gain` | `GAIN` | unset | `auto` or a multiplier |
| `--white-balance` | `WHITE_BALANCE` | unset | `auto` or kelvin |
| `--stream-size` | `STREAM_SIZE` | `1920 1080` | Output encoding resolution |
| `--mirror` | `MIRROR` | `both` | Image mirroring (none/horizontal/vertical/both) |
| `--jpeg` | `JPEG` | false | Enable JPEG streaming |
| `--h264` | `H264` | true | Enable H.264 streaming |
| `--h264-bitrate` | `H264_BITRATE` | `auto` | Bitrate preset (auto/mbps5/mbps25/mbps50/mbps100) |
| `--h264-tiles` | `H264_TILES` | false | Enable 4K tiling |
| `--h264-tiles-fps` | `H264_TILES_FPS` | `15` | Tile FPS limit |
| `--record` | `RECORD` | unset | Record H.264 (Annex-B + JSON sidecar); needs `H264` and a camera source |
| `--replay-loop` | `REPLAY_LOOP` | false | Loop a `file:` source |
| `--replay-fps` | `REPLAY_FPS` | unset | Alias of `CAMERA_FPS` for `file:` sources; `CAMERA_FPS` wins |
| `--cam-info-path` | `CAM_INFO_PATH` | `""` | Camera calibration JSON path |
| `--cam-tf-vec` | `CAM_TF_VEC` | `0 0 0` | Camera translation (x y z meters) |
| `--cam-tf-quat` | `CAM_TF_QUAT` | `-1 1 -1 1` | Camera rotation quaternion (x y z w) |
| `--mode` | `MODE` | `peer` | Zenoh mode (peer/client/router) |
| `--connect` | `CONNECT` | `""` | Zenoh connect endpoints |
| `--listen` | `LISTEN` | `""` | Zenoh listen endpoints |
| `--no-multicast-scouting` | `NO_MULTICAST_SCOUTING` | false | Disable Zenoh multicast |
| `--tokio-console` | `TOKIO_CONSOLE` | false | Enable async debugging |
| `--tracy` | `TRACY` | false | Enable Tracy profiler |

### Performance Targets

- **Frame rate**: 30 FPS sustained for 1080p camera
- **Latency**:
  - DMA publish: < 5ms (zero-copy)
  - JPEG publish: < 50ms (including encoding)
  - H264 publish: < 100ms (including encoding)
- **Memory footprint**:
  - < 200MB resident for single 1080p camera
  - < 500MB for 4K camera with tiling
- **CPU usage**:
  - < 25% single core for 1080p (with hardware conversion and encoding)
  - < 40% single core for 4K with tiling
- **Power consumption**: < 3W additional power draw on NXP i.MX8

**Performance Profiling:**

- Use Tracy profiler (connect with tracy-profiler GUI)
- V4L2 frame timestamps (CLOCK_MONOTONIC) converted to wall-clock by the SDK's `RealtimeClock` for ROS2 Header stamps
- FPS monitoring with `Instant::now()` (monotonic), plotted in Tracy

### Hardware Specifics

- **Platform**: NXP i.MX8M Plus (ARM Cortex-A53)
- **Image conversion** (HAL `edgefirst-image`, never G2D directly):
  - `ImageProcessor` picks OpenGL, then G2D, then CPU
  - Format conversion (to RGBA for JPEG; for H.264 only when the encoder does not take the capture format at the stream size), scaling, and mirroring when the camera cannot mirror
- **H264 Encoder** (HAL `edgefirst-codec`):
  - `VideoEncoder` over the V4L2 memory-to-memory interface (vsi_v4l2 on i.MX 8M Plus)
  - Max resolution 1920×1080 (4K requires tiling into 4× 1080p tiles)
- **Camera interfaces**:
  - MIPI CSI-2 (primary, via V4L2)
  - USB UVC (secondary, via V4L2)
- **DMA Buffers**:
  - Capture pool allocated contiguous by the service through the HAL and imported by the camera (`MEMORY=import`), or the driver's own exported buffers (`MEMORY=export`); `auto` falls back from import to export
  - Subscribers open the producer's DMA-BUF fds with `pidfd_getfd` for zero-copy between processes
- **Timestamp / Clock Management**:
  - **CLOCK_REALTIME** for all ROS2 Header stamps (ROS2 convention, human-readable, log-correlatable)
  - **CLOCK_MONOTONIC** only for internal duration/interval measurements (FPS tracking, rate limiting via `Instant::now()`)
  - V4L2 provides frame timestamps in CLOCK_MONOTONIC; the SDK converts them to CLOCK_REALTIME with `RealtimeClock::to_realtime` and exposes the result as `Frame::realtime()`
  - Offset formula: `offset = CLOCK_REALTIME - CLOCK_MONOTONIC`, measured at every conversion (monotonic read bracketed by two realtime reads, midpoint used); **never cache the offset**, since CLOCK_REALTIME can step after the service starts
  - Conversion: `wall_time = v4l2_monotonic_timestamp + offset`
  - Convert once per frame in the capture loop and pass the resulting `Time` to every encoder and topic, so all representations of a frame carry an identical stamp; the Zenoh sample timestamp must equal `header.stamp`
  - On embedded systems without battery-backed RTC (i.MX8MP), CLOCK_REALTIME may step after boot when NTP or GNSS syncs, possibly after the camera service has started
  - **Never use CLOCK_MONOTONIC_RAW** for message timestamps — it is not NTP-adjusted and not compatible with ROS2
- **Platform quirks**:
  - Hardware conversion and encoding need physically contiguous (CMA) capture buffers
  - H264 encoder requires specific V4L2 controls for bitrate/GOP
  - Empty string env vars are treated as unset (`args::scrub_empty_env`)

**Supported Hardware:**

- **Maivin & Raivin**: NXP i.MX8M Plus, MIPI CSI-2 cameras
- **NXP i.MX 8M Plus EVK**: i.MX8M Plus evaluation kits
- **Testing**: x86_64 for host tests (`mock` source; the SDK's V4L2 tests need the vivid module)
- **CI**: Native `ubuntu-24.04-arm` runners for aarch64 builds, `camera-v4l2` boards for on-target hardware tests

### Testing Conventions

**Rust-specific:**

- **Unit tests**: Co-located in `#[cfg(test)] mod tests` at end of implementation files
  - Location: `src/args.rs`, `src/source.rs`, `src/pipeline.rs`, `src/encode.rs`, `src/jpeg.rs`, `src/messages.rs`, `src/main.rs`; most run against the SDK's `mock` source
- **Integration tests**: `tests/` directory at project root
  - `tests/env_scrub.rs` - every environment variable parses as its flag, and `KEY=""` behaves as unset (`harness = false`)
  - `tests/on_target.rs` - end-to-end runs of the binary on a board: JPEG, H.264, record then replay; `#[ignore]`, run with `--include-ignored` on a board with a V4L2 camera
- **SDK tests**: `crates/camera/tests/` (`mock.rs`, `schema.rs`, `file.rs`, `v4l2_vivid.rs`), and the SDK capture benchmark `crates/camera/benches/capture.rs`; see `crates/camera/TESTING.md`
- **Test naming**: descriptive snake_case names stating the behaviour (`record_requires_h264`)
- **Serial tests**: Use `serial_test` crate for tests that require exclusive hardware access

**Test Organization:**

```plaintext
camera/
├── src/                  # Unit tests at the bottom of each module with #[cfg(test)]
├── tests/
│   ├── env_scrub.rs      # Environment variable parsing (harness = false)
│   └── on_target.rs      # End-to-end runs on a board (#[ignore])
└── crates/camera/
    ├── tests/            # SDK tests: mock, schema, file, v4l2_vivid
    └── benches/
        └── capture.rs    # SDK capture benchmark (V4L2 camera)
```

**Running Tests:**

```bash
# Application unit tests only (no hardware)
cargo test --bins

# Environment variable tests
cargo test --test env_scrub

# SDK tests
cargo test -p edgefirst-camera

# Hardware-dependent tests (run on device)
cargo test -- --ignored

# All tests including ignored
cargo test -- --include-ignored

# Single test
cargo test record_requires_h264

# With coverage
cargo llvm-cov --all-features --workspace --html
```

**Test Requirements:**

- Minimum coverage: 70% overall, 80% for core modules (pipeline, encode, source)
- All public APIs must have unit tests
- Hardware-specific code should have both mocked and hardware tests
- The SDK capture benchmark should be run on a board before releases

**Hardware Testing:**

- **CI**: Native aarch64 testing on `ubuntu-24.04-arm` runner
- **On-target**: self-hosted boards labelled `camera-v4l2` for JPEG, H.264, and integration tests; libcamera-only boards are not supported
- **Coverage**: Collected from on-target tests via `cargo llvm-cov`

---

## AI Assistant Best Practices

**Verify:** APIs exist, licenses OK, linters pass, test edges, match patterns
**Avoid:** Hallucinated APIs, GPL/AGPL, cd commands, hardcoded secrets, over-engineering
**Review:** ALL code. YOU are author (AI = tool). Test thoroughly on target hardware.

---

## Quick Reference

**Branch:** `feature/EDGEAI-123-desc`
**Commit:** `EDGEAI-123: Brief description`
**PR:** 2 approvals (main), 1 (develop)
**Build:** `cargo fmt`, `cargo clippy -- -D warnings`, `cargo zigbuild --target aarch64-unknown-linux-gnu --release`
**Test:** `cargo test --bins` (unit), `cargo test --workspace` (all host tests), `cargo test -- --ignored` (hardware)
**Licenses:** MIT/Apache/BSD/EPL-2.0 | GPL/AGPL BLOCKED
**Security:** support@au-zone.com
**Coverage:** 70% min, 80%+ core modules
**Env vars:** Short names, no prefix (e.g., `CAMERA`, `H264`, `JPEG`)
**Config file:** `camera.default` for systemd EnvironmentFile reference

---

**SPS Version:** 3.0 (2026-03-12)
**Maintained by:** Sébastien Taylor <sebastien@au-zone.com>

*This document helps AI assistants contribute effectively to EdgeFirst Camera while maintaining quality, security, and consistency. For Au-Zone organization-wide standards, see SPS repository.*

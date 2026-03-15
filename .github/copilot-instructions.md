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
- **Testing:** 70% min coverage, 80%+ for core modules (image, video)
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
- **Build system**: Cargo with workspace configuration (includes `g2d-sys` member)
- **Key dependencies**:
  - `zenoh 1.6.2` - Transport and ROS2 bridge compatibility
  - `tokio 1.48.0` - Async runtime with multi-threaded executor
  - `videostream 2.1.4` - Camera interface (V4L2 backend with codec API support)
  - `edgefirst-schemas 1.5.2` - ROS2 message schemas with serde_cdr
  - `tracing-tracy` - Performance profiling (Tracy profiler integration)
  - `turbojpeg` - JPEG encoding with SIMD (require-simd feature)
  - `dma-heap/dma-buf` - Zero-copy DMA buffer management
  - `g2d-sys` - NXP G2D hardware acceleration bindings (local crate, MIT licensed)
  - `kanal` - Fast bounded SPSC/MPMC channels for inter-thread communication
  - `clap` - CLI argument parsing with derive and env features
- **Target platforms**:
  - Primary: Linux aarch64 (NXP i.MX8M Plus, ARM Cortex-A53/A72)
  - Secondary: Linux x86_64 (development and testing)
  - **Not supported**: macOS, Windows, RISC-V (hardware-specific dependencies)
- **Cross-platform**: This is a hardware-specific camera service requiring Linux V4L2, DMA support, and NXP G2D acceleration

### Architecture

- **Pattern**: Producer-consumer pipeline with hardware acceleration
- **Components**:
  - Camera Interface → Image Processing → Video Encoding → Publishing
  - Main loop (tokio async) + dedicated encoder threads (JPEG, H264, 4K tiles)
- **Data flow**:
  - Camera (V4L2) → DMA Buffer → G2D Conversion → JPEG/H264 Encoding → Zenoh Publishing
- **Key source files**:
  - `src/image.rs` - DMA buffer allocation, G2D operations, JPEG encoding (public module via lib.rs)
  - `src/video.rs` - H264 encoding, 4K tiling (binary-only, not exported via lib.rs)
  - `src/main.rs` - Main loop, Zenoh publishers, thread coordination, frame capture
  - `src/args.rs` - CLI arguments with clap (env var support for all options)
  - `src/lib.rs` - Public library interface (exports `image` module only)
  - `g2d-sys/` - NXP G2D FFI bindings (unsafe, platform-specific)
- **Error handling**:
  - Uses `Result<T, Box<dyn Error>>` pattern
  - Logging via `tracing` with journald backend
  - Frame drop warnings when FPS falls below threshold
  - Graceful shutdown on SIGTERM/SIGINT signals
  - EINTR handling during camera reads

### Build and Deployment

```bash
# ALWAYS run before committing:
cargo fmt
cargo clippy -- -D warnings

# Build for target (use zigbuild on non-Linux or when cross-compiling):
cargo zigbuild --target aarch64-unknown-linux-gnu --release  # cross-compile (macOS/x86_64 → aarch64)
cargo build --release                                        # native build (Linux x86_64 or aarch64)

# Run tests (requires hardware on device, most tests are ignored by default)
cargo test --workspace

# Run only unit tests (no hardware required)
cargo test --lib

# Generate documentation
cargo doc --no-deps --open

# Run benchmarks (requires hardware)
cargo bench

# Build with profiling enabled
cargo zigbuild --target aarch64-unknown-linux-gnu --release --profile=profiling --features=profiling

# Generate SBOM and check license policy
make sbom

# Pre-release checks (format, lint, test, sbom, version verify)
make pre-release

# Coverage report
cargo llvm-cov --all-features --workspace --html
```

**Build Verification:** Always run `cargo fmt` and `cargo clippy -- -D warnings` before building. Use `cargo zigbuild --target aarch64-unknown-linux-gnu` when developing on a non-Linux platform (macOS, Windows) or when cross-compiling for the aarch64 target. Native `cargo build` only works on Linux aarch64 due to platform-specific dependencies (V4L2, DMA, G2D).

**Yocto Integration:**

- Deployed via Yocto recipes in separate meta-edgefirst repository
- Binary installed to `/usr/bin/edgefirst-camera`
- Typically run as systemd service on Maivin/Raivin platforms
- Configuration via `camera.default` EnvironmentFile for systemd

**CI/CD Workflows** (`.github/workflows/`):

- `test.yml` - Run tests and linting
- `build.yml` - Build release binaries (native aarch64 + x86_64)
- `sbom.yml` - SBOM generation and license compliance
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
| `--camera` | `CAMERA` | `/dev/video3` | V4L2 device path |
| `--camera-size` | `CAMERA_SIZE` | `1920 1080` | Capture resolution |
| `--stream-size` | `STREAM_SIZE` | `1920 1080` | Output encoding resolution |
| `--mirror` | `MIRROR` | `both` | Image mirroring (none/horizontal/vertical/both) |
| `--jpeg` | `JPEG` | false | Enable JPEG streaming |
| `--h264` | `H264` | false | Enable H.264 streaming |
| `--h264-bitrate` | `H264_BITRATE` | `auto` | Bitrate preset (auto/mbps5/mbps25/mbps50/mbps100) |
| `--h264-tiles` | `H264_TILES` | false | Enable 4K tiling |
| `--h264-tiles-fps` | `H264_TILES_FPS` | `15` | Tile FPS limit |
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
  - < 25% single core for 1080p (with G2D acceleration)
  - < 40% single core for 4K with tiling
- **Power consumption**: < 3W additional power draw on NXP i.MX8

**Performance Profiling:**

- Use Tracy profiler (connect with tracy-profiler GUI)
- V4L2 frame timestamps (CLOCK_MONOTONIC) converted to wall-clock via `ClockOffset` for ROS2 Header stamps
- FPS monitoring with `Instant::now()` (monotonic) with warnings when below threshold

### Hardware Specifics

- **Platform**: NXP i.MX8M Plus (ARM Cortex-A53)
- **G2D Acceleration**:
  - Format conversion (YUYV → NV12/RGB/RGBA)
  - Image scaling and rotation
  - FFI bindings in `g2d-sys` crate (dynamically loaded via `libloading`)
- **H264 Encoder**:
  - Hardware encoder via V4L2 M2M interface (`/dev/mxc-hantro-h1`)
  - Backend selectable via `VSL_CODEC_BACKEND` environment variable
  - Max resolution 1920×1080 (4K requires tiling into 4× 1080p tiles)
- **Camera interfaces**:
  - MIPI CSI-2 (primary, via V4L2)
  - USB UVC (secondary, via V4L2)
- **DMA Buffers**:
  - Allocated from `/dev/dma_heap/linux,cma`
  - File descriptor passing for zero-copy between processes
  - RAII cleanup via `dma-buf` crate
- **Timestamp / Clock Management**:
  - **CLOCK_REALTIME** for all ROS2 Header stamps (ROS2 convention, human-readable, log-correlatable)
  - **CLOCK_MONOTONIC** only for internal duration/interval measurements (FPS tracking, rate limiting via `Instant::now()`)
  - V4L2 provides frame timestamps in CLOCK_MONOTONIC; convert to CLOCK_REALTIME via a cached offset (`ClockOffset` struct)
  - Offset formula: `offset = CLOCK_REALTIME - CLOCK_MONOTONIC` (computed once at startup, stable after NTP settles)
  - Conversion: `wall_time = v4l2_monotonic_timestamp + offset` (same pattern as ROS2 `usb_cam` / `image_transport`)
  - On embedded systems without battery-backed RTC (i.MX8MP), CLOCK_REALTIME may jump once at boot when NTP syncs; after initial correction NTP only slews (gradual adjustment)
  - **Never use CLOCK_MONOTONIC_RAW** for message timestamps — it is not NTP-adjusted and not compatible with ROS2
- **Platform quirks**:
  - G2D requires physically contiguous memory (DMA heap)
  - V4L2 MMAP buffers for camera capture
  - H264 encoder requires specific V4L2 controls for bitrate/GOP
  - Empty string env vars must be handled (v2.5.1 fix)

**Supported Hardware:**

- **Maivin & Raivin**: NXP i.MX8M Plus, MIPI CSI-2 cameras
- **NXP i.MX 8M Plus EVK**: i.MX8M Plus evaluation kits
- **Testing**: x86_64 for software-only tests (no hardware acceleration)
- **CI**: Native `ubuntu-22.04-arm` runners for aarch64 builds, `nxp-imx8mp-latest` for on-target hardware tests

### Testing Conventions

**Rust-specific:**

- **Unit tests**: Co-located in `#[cfg(test)] mod tests` at end of implementation files
  - Location: `src/image.rs`, `src/video.rs`, etc.
  - Most hardware tests are `#[ignore]` - run on device with `cargo test -- --ignored`
- **Integration tests**: Separate `tests/` directory at project root
  - Current: `tests/test_image.rs` (image allocation and processing tests)
- **Benchmarks**: `benches/` directory with criterion
  - `benches/encode.rs` - JPEG/H264 encoding performance
  - `benches/convert.rs` - G2D format conversion performance
- **Test naming**: `test_<module>_<scenario>` format
- **Serial tests**: Use `serial_test` crate for tests that require exclusive hardware access

**Test Organization:**

```plaintext
camera/
├── src/
│   ├── image.rs          # Unit tests at bottom with #[cfg(test)]
│   ├── video.rs          # Unit tests at bottom with #[cfg(test)]
│   └── main.rs           # Minimal tests (mostly integration)
├── tests/
│   └── test_image.rs     # Integration tests for image library
└── benches/
    ├── encode.rs         # Performance benchmarks
    └── convert.rs        # G2D conversion benchmarks
```

**Running Tests:**

```bash
# Unit tests only (no hardware)
cargo test --lib

# Integration tests (may require hardware)
cargo test --test test_image

# Hardware-dependent tests (run on device)
cargo test -- --ignored

# All tests including ignored
cargo test -- --include-ignored

# Single test
cargo test test_image_allocation

# With coverage
cargo llvm-cov --all-features --workspace --html
```

**Test Requirements:**

- Minimum coverage: 70% overall, 80% for core modules (image, video)
- All public APIs must have unit tests
- Hardware-specific code should have both mocked and hardware tests
- Performance benchmarks should be run before releases

**Hardware Testing:**

- **CI**: Native aarch64 testing on `ubuntu-22.04-arm` runner
- **On-target**: `nxp-imx8mp-latest` self-hosted runner for JPEG, H.264, and integration tests
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
**Test:** `cargo test --lib` (unit), `cargo test -- --ignored` (hardware)
**Licenses:** MIT/Apache/BSD/EPL-2.0 | GPL/AGPL BLOCKED
**Security:** support@au-zone.com
**Coverage:** 70% min, 80%+ core modules
**Env vars:** Short names, no prefix (e.g., `CAMERA`, `H264`, `JPEG`)
**Config file:** `camera.default` for systemd EnvironmentFile reference

---

**SPS Version:** 3.0 (2026-03-12)
**Maintained by:** Sébastien Taylor <sebastien@au-zone.com>

*This document helps AI assistants contribute effectively to EdgeFirst Camera while maintaining quality, security, and consistency. For Au-Zone organization-wide standards, see SPS repository.*

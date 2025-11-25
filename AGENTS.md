# AGENTS.md - AI Assistant Development Guidelines

**Purpose:** Project-specific instructions for AI coding assistants (GitHub Copilot, Claude, Cursor, etc.)

**Organization Standards:** See SPS repository for Au-Zone universal rules (Git/JIRA workflow, license policy, security practices)

**Version:** 2.0
**Last Updated:** 2025-11-24

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

## ⚠️ CRITICAL RULES

### #1: NEVER Use cd Commands

```bash
# ✅ Modern tools work from root
cargo build --release
cargo test --workspace
cargo clippy -- -D warnings

# ❌ AI loses context
cd src && cargo build  # Where are we now?
```

### #2: Code Quality Standards

- **Rust:** Latest stable (1.90.0+), `cargo fmt`, `cargo clippy -- -D warnings`
- **Performance:** Edge-first (512MB-2GB RAM, ARM64, <50ms latency, 5-10yr lifecycle)
- **Testing:** 70% min coverage, 80%+ for core modules (image, video)
- **Hardware:** Profile on target NXP i.MX8 platforms

---

## License Policy (ZERO TOLERANCE)

**✅ Allowed:** MIT, Apache-2.0, BSD-2/3, ISC, 0BSD, Unlicense, Zlib
**⚠️ Conditional:** MPL-2.0 (deps ONLY), LGPL (**FORBIDDEN in Rust**)
**❌ BLOCKED:** GPL, AGPL, SSPL, Commons Clause

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

- **Language**: Rust 1.90.0+
- **Build system**: Cargo with workspace configuration (includes `g2d-sys` member)
- **Key dependencies**:
  - `zenoh 1.5.1` - Transport and ROS2 bridge compatibility
  - `tokio 1.47.1` - Async runtime with multi-threaded executor
  - `videostream 0.9.1` - Camera interface (V4L2 backend)
  - `edgefirst-schemas 1.3.1` - ROS2 message schemas
  - `tracing-tracy` - Performance profiling
  - `turbojpeg` - JPEG encoding with SIMD
  - `dma-heap/dma-buf` - Zero-copy DMA buffer management
  - `g2d-sys` - NXP G2D hardware acceleration bindings (local crate)
- **Target platforms**:
  - Primary: Linux aarch64 (NXP i.MX8, ARM Cortex-A53/A72)
  - Secondary: Linux x86_64 (development and testing)
  - **Not supported**: macOS, Windows, RISC-V (hardware-specific dependencies)
- **Cross-platform**: This is a hardware-specific camera service requiring Linux V4L2, DMA support, and NXP G2D acceleration

### Architecture

- **Pattern**: Producer-consumer pipeline with hardware acceleration
- **Components**:
  - Camera Interface → Image Processing → Video Encoding → Publishing
  - Main loop (tokio async) + dedicated encoder threads (JPEG, H264)
- **Data flow**:
  - Camera (V4L2) → DMA Buffer → G2D Conversion → JPEG/H264 Encoding → Zenoh Publishing
- **Key abstractions**:
  - `src/image.rs` - DMA buffer allocation, G2D operations, JPEG encoding
  - `src/video.rs` - H264 encoding, 4K tiling
  - `src/main.rs` - Main loop, Zenoh publishers, frame capture
  - `src/args.rs` - CLI arguments with clap
- **Error handling**:
  - Uses `Result<T, Box<dyn Error>>` pattern
  - Logging via `tracing` with journald backend
  - Frame drop warnings when FPS falls below threshold

### Build and Deployment

```bash
# Build release binary (native)
cargo build --release

# Cross-compile for ARM64 (primary target)
cargo build --target=aarch64-unknown-linux-gnu --release

# Run tests (requires hardware on device, most tests are ignored by default)
cargo test --workspace

# Run only unit tests (no hardware required)
cargo test --lib

# Generate documentation
cargo doc --no-deps --open

# Run benchmarks (requires hardware)
cargo bench

# Build with profiling enabled
cargo build --release --profile=profiling --features=profiling

# Format code
cargo fmt

# Lint code
cargo clippy -- -D warnings
```

**Yocto Integration:**

- Deployed via Yocto recipes in separate meta-edgefirst repository
- Binary installed to `/usr/bin/edgefirst-camera`
- Typically run as systemd service on Maivin/Raivin platforms

**Development Tips:**

- Use `tracy` profiler for performance analysis (feature enabled by default)
- Optional `tokio-console` for async task debugging
- Most hardware-dependent tests are `#[ignore]` by default - use `cargo test -- --ignored` on device

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
- Frame timing tracked with `unix-ts` crate for precise timestamps
- FPS monitoring with warnings when below threshold

### Hardware Specifics

- **Platform**: NXP i.MX8M Plus (ARM Cortex-A53)
- **G2D Acceleration**:
  - Format conversion (YUYV → NV12/RGB/RGBA)
  - Image scaling and rotation
  - FFI bindings in `g2d-sys` crate
- **H264 Encoder**:
  - Hardware encoder via V4L2 M2M interface
  - 4K tiling: splits 3840x2160 into 4× 1920x1080 tiles
- **Camera interfaces**:
  - MIPI CSI-2 (primary, via V4L2)
  - USB UVC (secondary, via V4L2)
- **DMA Buffers**:
  - Allocated from `/dev/dma_heap/linux,cma`
  - File descriptor passing for zero-copy between processes
  - RAII cleanup via `dma-buf` crate
- **Platform quirks**:
  - G2D requires physically contiguous memory (DMA heap)
  - V4L2 MMAP buffers for camera capture
  - H264 encoder requires specific V4L2 controls for bitrate/GOP

**Supported Hardware:**

- **Maivin & Raivin**: NXP i.MX8M Plus, MIPI CSI-2 cameras
- **NXP i.MX 8M Plus EVK**: i.MX8M Plus evaluation kits
- **Testing**: x86_64 for software-only tests (no hardware acceleration)

### Testing Conventions

**Rust-specific:**

- **Unit tests**: Co-located in `#[cfg(test)] mod tests` at end of implementation files
  - Location: `src/image.rs`, `src/video.rs`, etc.
  - Most hardware tests are `#[ignore]` - run on device with `cargo test -- --ignored`
- **Integration tests**: Separate `tests/` directory at project root
  - Current: `tests/test_image.rs` (7 tests for image allocation and processing)
  - **TODO**: Add Zenoh publisher/subscriber integration tests
- **Benchmarks**: `benches/` directory with criterion
  - `benches/encode.rs` - JPEG/H264 encoding performance
  - `benches/convert.rs` - G2D format conversion performance
- **Test naming**: `test_<module>_<scenario>` format
- **Hardware mocking**:
  - Use `#[cfg(test)]` to conditionally compile mock implementations
  - Future: Add DMA buffer mocks for CI testing without hardware

**Test Organization:**

```plaintext
camera/
├── src/
│   ├── image.rs          # Unit tests at bottom with #[cfg(test)]
│   ├── video.rs          # Unit tests at bottom with #[cfg(test)]
│   └── main.rs           # Minimal tests (mostly integration)
├── tests/
│   ├── test_image.rs     # Integration tests for image library
│   └── common/           # Shared test fixtures (TODO)
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
- Integration tests should verify Zenoh publishing (TODO)
- Performance benchmarks should be run before releases

**Hardware Testing:**

- **QA Team**: Manual testing on Maivin/Raivin platforms
- **Self-hosted runners**: Planned for automated on-target testing
- **Test plan**: See TODO.md for integration testing strategy

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
**Build:** `cargo build --release`, `cargo clippy -- -D warnings`
**Test:** `cargo test --lib` (unit), `cargo test -- --ignored` (hardware)
**Licenses:** ✅ MIT/Apache/BSD | ❌ GPL/AGPL
**Security:** support@au-zone.com
**Coverage:** 70% min, 80%+ core modules

---

**SPS Version:** 2.0 (2025-11-24)
**Maintained by:** Sébastien Taylor <sebastien@au-zone.com>

*This document helps AI assistants contribute effectively to EdgeFirst Camera while maintaining quality, security, and consistency. For Au-Zone organization-wide standards, see SPS repository.*

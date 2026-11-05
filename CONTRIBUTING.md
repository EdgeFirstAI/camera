# Contributing to EdgeFirst Camera Node

Thank you for your interest in contributing to the EdgeFirst Camera Node! This document provides guidelines for contributing to the project.

---

## Table of Contents

1. [Code of Conduct](#code-of-conduct)
2. [Getting Started](#getting-started)
3. [Development Workflow](#development-workflow)
4. [Profiling and Performance Analysis](#profiling-and-performance-analysis)
5. [Coding Standards](#coding-standards)
6. [Testing Requirements](#testing-requirements)
7. [Pull Request Process](#pull-request-process)
8. [License Policy](#license-policy)

---

## Code of Conduct

This project adheres to the [Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md). By participating, you are expected to uphold this code. Please report unacceptable behavior to support@au-zone.com.

---

## Getting Started

### Prerequisites

**Development Environment:**

- Linux (Ubuntu 20.04+ or Debian 11+ recommended)
- Rust 1.94 or later (`rust-toolchain.toml` pins 1.94.0)
- For ARM64 cross-compilation: `cargo-zigbuild` and Zig
- For hardware testing: a board such as the NXP i.MX8M Plus (V4L2) or i.MX 95 (libcamera)

**Install Rust:**

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup update
```

**Install Development Tools:**

```bash
# Debian/Ubuntu
sudo apt-get update
sudo apt-get install build-essential pkg-config libv4l-dev

# For cross-compilation
cargo install cargo-zigbuild
rustup target add aarch64-unknown-linux-gnu
```

Install Zig from <https://ziglang.org/download/>, or with `pip install ziglang`.

### Clone Repository

```bash
git clone https://github.com/EdgeFirstAI/camera.git
cd camera
```

### Workspace

The repository is a Cargo workspace:

- The root package, `edgefirst-camera-app`, is the service. Its binary is `edgefirst-camera`.
- `crates/camera` is the `edgefirst-camera` SDK: the V4L2, libcamera, file and mock backends. See its [architecture](crates/camera/ARCHITECTURE.md) and [testing guide](crates/camera/TESTING.md).
- `crates/libcamera-sys` is `edgefirst-libcamera-sys`: Rust bindings that load a C shim over libcamera at run time. The shim is C++ in `crates/libcamera-sys/shim`, built with meson against the installed libcamera (`meson setup build crates/libcamera-sys/shim && ninja -C build`). Nothing links libcamera, so `cargo build` needs neither libcamera nor the shim; see the crate's [README](crates/libcamera-sys/README.md) for building and finding the shim.

### Build and Run

```bash
# Build debug binary
cargo build

# Run tests for the whole workspace
cargo test --workspace

# Build release binary
cargo build --release

# Run (requires camera hardware, or use --source mock --h264 false)
cargo run -- --camera /dev/video0 --jpeg
```

---

## Development Workflow

### Branch Strategy

Development is trunk-based. `main` is the only long-lived branch and is protected. Create short-lived branches from `main`, named `<type>/EDGEAI-###[-description]`:

```text
feature/EDGEAI-123-add-udp-streaming
bugfix/EDGEAI-124-tile-frame-drops
hotfix/EDGEAI-125-fix-description
```

```bash
git checkout main
git pull origin main
git checkout -b feature/EDGEAI-123-add-udp-streaming
```

### Commit Messages

Commits use the JIRA key and a short description of what was done, and are signed off with `-s`:

```text
EDGEAI-123: Brief description of the change
```

```bash
git commit -s -m "EDGEAI-123: Add UDP streaming for JPEG frames"
```

Keep the first line to 50 to 72 characters. Add a body when the reason for the change is not obvious from the diff.

### Development Loop

1. **Create a branch** from `main`
2. **Make changes** with atomic commits
3. **Write tests** for new functionality
4. **Run linters** and formatters
5. **Test locally** on hardware if applicable
6. **Push branch** to GitHub
7. **Open a pull request** to `main`
8. **Address review** feedback
9. **Merge** after approval

---

## Profiling and Performance Analysis

### Tracy Profiler Setup

The camera node includes Tracy profiler integration. The Tracy GUI is available from <https://github.com/wolfpld/tracy/releases>. Match the GUI to the version of `tracy-client` in `Cargo.lock`.

Tracy is enabled by default (the `tracy` feature). The `profiling` feature adds allocation tracking, sampling and system tracing:

```bash
cargo build --release --profile=profiling --features=profiling
```

Run the service with `--tracy` (or `TRACY=true`) and connect the Tracy GUI to the device:

```bash
./target/release/edgefirst-camera --tracy --camera /dev/video0 --jpeg --h264
```

What the service emits:

- **Frame markers:** the capture loop marks each frame, and the encoders mark secondary frames named `h264` and `jpeg`.
- **Plots:** `fps` (capture rate), `h264_kb` (H.264 access unit size in kilobytes) and `jpeg_kb` (JPEG size in kilobytes).
- **Zones:** `camera_read` and `camera_publish` on the capture thread, and `h264` (with the stage name) and `jpeg` on the encoder threads.

Profile on the target hardware rather than a development machine, and compare a trace before and after a change.

#### Tokio Console (Async Debugging)

For debugging async task issues (separate from Tracy):

**1. Install tokio-console:**

```bash
cargo install tokio-console
```

**2. Run camera with console enabled:**

```bash
./edgefirst-camera --tokio-console --camera /dev/video0 --jpeg
```

**3. Connect tokio-console:**

```bash
# In separate terminal
tokio-console
```

**Use Cases:**

- Identify blocking async tasks
- Find deadlocks
- Monitor task wake/sleep patterns
- Check for slow futures

#### Performance Best Practices

**During Development:**

- Profile early and often
- Establish baseline performance
- Use Tracy to validate assumptions
- Check both average and worst-case timings

**Before Release:**

- Capture Tracy trace of typical workload
- Verify frame timing meets requirements
- Check for memory leaks (long-running test)
- Profile on target hardware (i.MX8), not development machine

**Optimization Guidelines:**

- Profile before optimizing (measure, don't guess)
- Focus on hot paths (zones that appear frequently)
- Prefer hardware acceleration (HAL image conversion, H.264 encoder)
- Avoid allocations in encoding threads

---

## Coding Standards

### Rust Style Guidelines

**Follow Official Style:**

- Use `rustfmt` for formatting (CI enforces this)
- Use `clippy` for linting (CI enforces this)
- Follow [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)

**Format Code:**

```bash
cargo fmt --all
```

**Lint Code:**

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

### Code Quality Principles

**Readability:**

- Prefer clear, self-documenting code over clever code
- Use descriptive variable names (`frame_timestamp` not `ft`)
- Keep functions small and focused (< 50 lines ideal)
- Add comments for complex algorithms or hardware-specific quirks

**Error Handling:**

- Use `Result<T, Box<dyn Error>>` for fallible operations
- Provide context with error messages:

  ```rust
  .with_context(|| format!("Failed to open camera {}", device_path))?
  ```

- Log errors before returning:

  ```rust
  error!("Camera initialization failed: {}", e);
  ```

**Performance:**

- Avoid unnecessary allocations in hot paths
- Use zero-copy patterns where possible (DMA buffers)
- Profile before optimizing (use `--tracy` flag)
- Document performance-critical sections

**Documentation:**

- Public APIs must have rustdoc comments
- Include examples for complex functions
- Document panic conditions
- Explain hardware-specific behavior

**Example:**

```rust
/// Opens the platform's hardware H.264 encoder for `config`.
///
/// # Arguments
/// * `config` - Stream size, input format, frame rate and bitrate
///
/// # Returns
/// The encoder, ready for its first frame.
///
/// # Errors
/// Returns the codec error when no V4L2 memory-to-memory encoder is present
/// or it refuses the configuration.
///
/// # Platform Requirements
/// Linux with a V4L2 memory-to-memory H.264 encoder (vsi_v4l2 on
/// i.MX 8M Plus). There is no software fallback.
pub(crate) fn open_hardware(config: EncoderConfig) -> Result<Box<dyn Encode>, CodecError> {
    // Implementation
}
```

### Hardware-Specific Code

**Platform Conditionals:**

```rust
let clock = if cfg!(unix) {
    CaptureClock::Monotonic
} else {
    CaptureClock::Realtime
};
```

**Fallbacks:**

- Fall back only where the design says to, and log it: `warn!("cannot allocate the capture pool, using the camera's: {e}")`
- Report a missing hardware encoder or decoder as an error. There is no software H.264 fallback.

---

## Testing Requirements

### Test Categories

**Unit Tests:**

- Test individual functions in isolation
- Located in same file as implementation
- Use `#[cfg(test)]` module

**Integration Tests:**

- Test component interactions
- Located in `tests/` directory
- May require hardware (use `#[ignore]` for hardware-dependent tests)

**Benchmarks:**

- The SDK capture benchmark is `crates/camera/benches/capture.rs`
- Use `criterion` framework
- Run on a board with `EDGEFIRST_CAMERA_BENCH_SOURCE=/dev/video3 cargo bench -p edgefirst-camera --bench capture`

### Running Tests

**All Tests (no hardware):**

```bash
cargo test --workspace
```

**Integration Tests:**

```bash
cargo test --test env_scrub
```

**Hardware-Dependent Tests (on device):**

The SDK's V4L2 tests run on a host against the kernel's vivid driver; see [`crates/camera/TESTING.md`](crates/camera/TESTING.md) for loading it. Both board suites below run against `EDGEFIRST_CAMERA_SOURCE`: a V4L2 node (default `/dev/video3`), `libcamera:<id>`, or `libcamera` for the first libcamera camera. On the i.MX 95, also set `LIBCAMERA_PIPELINES_MATCH_LIST=nxp/neo`, and `EDGEFIRST_CAMERA_LIBCAMERA_SHIM_DIR` unless the shim is installed. In CI, `.github/scripts/board-setup.sh` sets these per board.

```bash
cargo test --test on_target -- --include-ignored

# Platform validation of the SDK with the image and codec crates; see the
# file's header for EDGEFIRST_SOAK_SECS (600 for the ten-minute run) and
# EDGEFIRST_CAMERA_RECORDING (a 2.x recording to decode).
cargo test --test board -- --include-ignored --test-threads 1
```

**Parity with the installed release (Maivin, from a host):**

```bash
# Cross-build the service, the parity probe and the SDK's probe, then
# compare them with the board's installed edgefirst-camera under its own
# camera.service. Results and Markdown tables land in the output directory.
cargo zigbuild --target aarch64-unknown-linux-gnu.2.35 --release --locked \
    --bin edgefirst-camera --example parity
cargo zigbuild --target aarch64-unknown-linux-gnu.2.35 --release --locked \
    -p edgefirst-camera --example probe
scripts/parity.sh torizon@<maivin> parity-results
```

**With Coverage:**

```bash
cargo llvm-cov --all-features --workspace --html
open target/llvm-cov/html/index.html
```

### Writing Tests

**Unit Test Example:**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_requires_h264() {
        let err = validate(&args(&["--record", "a.h264", "--h264", "false"])).unwrap_err();
        assert!(err.to_string().contains("--h264"), "{err}");
        validate(&args(&["--record", "a.h264"])).unwrap();
    }
}
```

**Integration Test Example:**

```rust
// tests/on_target.rs
#[test]
#[serial]
#[ignore = "requires a V4L2 camera (run with --include-ignored on a board)"]
fn jpeg_capture() -> Result<(), Box<dyn Error>> {
    run_for(&capture(&["--jpeg"]), Duration::from_secs(15))
}
```

### Coverage Requirements

- **Minimum overall**: 70%
- **Core modules** (`pipeline.rs`, `encode.rs`, `source.rs`): 80%+
- **Public APIs**: 100%
- **Hardware paths**: Best effort (requires device)

**Coverage is measured in CI** by the nightly **Full** run, which covers host, board and SonarCloud every day `main` changes. Every PR runs the **Quick** tier (`ci-gate`), **including draft PRs** — CI is not gated on “Ready for review”; the expected flow is a green `ci-gate` before reviewers start. Full and hardware runs on a PR are opt-in, decided per change: apply `ci:full` for host coverage, board tests and Sonar when a change warrants it. Apply `ci:hardware` when on-target validation alone is needed: it runs on an i.MX 8M Plus through V4L2 (runner labels `camera-v4l2` and `imx8mp`) and on the i.MX 95 through libcamera (`camera-libcamera` and `imx95`), and covers JPEG/H.264 capture, record and replay (`tests/on_target.rs`) and the platform checks in `tests/board.rs` with a 20-second soak.

---

## Pull Request Process

### Before Submitting

**Checklist:**

- [ ] Code follows Rust style guidelines (`cargo fmt`)
- [ ] All lints pass (`cargo clippy --workspace --all-targets -- -D warnings`)
- [ ] Tests pass (`cargo test --workspace`)
- [ ] New functionality has tests
- [ ] Documentation is updated (README, rustdoc)
- [ ] Commits follow `EDGEAI-###: Description` and are signed off with `-s`
- [ ] No secrets or credentials committed
- [ ] SBOM license policy compliance verified

### Creating Pull Request

1. **Push your branch** to GitHub:

   ```bash
   git push origin feature/EDGEAI-123-your-feature
   ```

2. **Open PR** via GitHub web interface:
   - Base: `main`
   - Provide clear title and description
   - Link related issues: "Fixes #123" or "Related to #456"

3. **Fill out the [pull request template](.github/pull_request_template.md).** GitHub loads it when you open the PR.

### Review Process

**Reviewers will check:**

- Code quality and style
- Test coverage
- Documentation completeness
- Performance impact
- Hardware compatibility
- License compliance

**Address feedback:**

- Make requested changes
- Push additional commits to same branch
- Respond to review comments
- Re-request review when ready

**Approval requirements:**

- **2 approvals** for merges to `main`
- All CI checks must pass (required check: **`ci-gate`**)

### Merge

Once approved, a maintainer will merge using **squash and merge** to keep history clean.

---

## License Policy

### CRITICAL: License Compliance

This project is licensed under **Apache-2.0**. All dependencies must comply with Au-Zone's license policy.

**Allowed Licenses:**

- MIT, Apache-2.0, BSD-2/3-Clause, ISC, 0BSD, Unlicense
- EPL-2.0, MPL-2.0 (for dependencies only)

**Disallowed Licenses:**

- GPL, AGPL (all versions)
- LGPL (requires review)
- Creative Commons with NC/ND/SA restrictions

**Before adding dependencies:**

1. Check license in `Cargo.toml` or repository
2. Verify compatibility with Apache-2.0
3. Run SBOM generation to detect issues:

   ```bash
   .github/scripts/generate_sbom.sh
   python3 .github/scripts/check_license_policy.py sbom.json
   ```

**If license checker fails, DO NOT merge**

---

## Development Tips

### Debugging with Tokio Console

```bash
# Set environment variable
export TOKIO_CONSOLE_BIND=localhost:7000

# Run with console enabled
cargo run -- --tokio-console

# Connect with tokio-console CLI
cargo install tokio-console
tokio-console
```

### Cross-Compilation for ARM64

```bash
# Build against glibc 2.35, the floor for board binaries
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.35

# Binary at: target/aarch64-unknown-linux-gnu/release/edgefirst-camera
```

### Testing on Hardware

```bash
# Build the ARM64 binary
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.35

# Copy to the device
scp target/aarch64-unknown-linux-gnu/release/edgefirst-camera user@device:/tmp/

# SSH and test
ssh user@device
cd /tmp
./edgefirst-camera --camera /dev/video0 --jpeg
```

---

## Community

### Getting Help

**Questions:**

- [GitHub Discussions](https://github.com/EdgeFirstAI/camera/discussions) - Q&A, ideas
- [Documentation](https://doc.edgefirst.ai/test/perception/) - Guides and tutorials

**Issues:**

- [Bug Reports](https://github.com/EdgeFirstAI/camera/issues/new?template=bug_report.md)
- [Feature Requests](https://github.com/EdgeFirstAI/camera/issues/new?template=feature_request.md)

**Chat:**

- EdgeFirst Community Discord (coming soon)

### Recognition

Contributors will be acknowledged in:

- Release notes for significant contributions
- GitHub contribution graph

---

## Thank You

Every contribution, no matter how small, helps make EdgeFirst Camera better for everyone. We appreciate your time and effort!

**Questions about contributing?** Open a [discussion](https://github.com/EdgeFirstAI/camera/discussions) or email support@au-zone.com

---

_Last updated: 2026-11-04_

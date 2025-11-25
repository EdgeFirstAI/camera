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
- Rust 1.90.0 or later
- For ARM64 cross-compilation: `gcc-aarch64-linux-gnu`
- For hardware testing: NXP i.MX8M Plus device

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
sudo apt-get install gcc-aarch64-linux-gnu
rustup target add aarch64-unknown-linux-gnu
```

### Clone Repository

```bash
git clone https://github.com/EdgeFirstAI/camera.git
cd camera
```

### Build and Run

```bash
# Build debug binary
cargo build

# Run tests
cargo test

# Build release binary
cargo build --release

# Run (requires camera hardware)
cargo run -- --camera /dev/video0 --jpeg
```

---

## Development Workflow

### Branch Strategy

**Main Branches:**

- `main` - Stable release branch (protected)
- `develop` - Integration branch for features (protected)

**Feature Branches:**
Create branches from `develop` using the pattern:

```
feature/<description>
bugfix/<description>
hotfix/<description>
```

**Example:**

```bash
git checkout develop
git pull origin develop
git checkout -b feature/add-udp-streaming
```

### Commit Messages

Use clear, descriptive commit messages:

```
<type>: <short summary>

<detailed description if needed>
```

**Types:**

- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation only
- `style`: Code style/formatting (no logic change)
- `refactor`: Code refactoring (no functional change)
- `perf`: Performance improvement
- `test`: Add or update tests
- `chore`: Build process, dependencies, etc.

**Examples:**

```
feat: add UDP streaming support for JPEG frames

Implements direct UDP multicast streaming as alternative to Zenoh for
low-latency local network distribution.

fix: prevent frame drops during 4K tile encoding

Increased tile channel buffer from 1 to 3 frames to accommodate
parallel encoding latency variance.
```

### Development Loop

1. **Create feature branch** from `develop`
2. **Make changes** with atomic commits
3. **Write tests** for new functionality
4. **Run linters** and formatters
5. **Test locally** on hardware if applicable
6. **Push branch** to GitHub
7. **Open pull request** to `develop`
8. **Address review** feedback
9. **Merge** after approval

---

## Profiling and Performance Analysis

### Tracy Profiler Setup

The camera node includes Tracy profiler integration for detailed performance analysis. Tracy is a real-time profiler that provides frame timing, CPU profiling, memory tracking, and GPU activity visualization.

#### Installing Tracy Profiler

**Download Tracy:**

Tracy profiler GUI is available from the official repository:

```bash
# Clone Tracy repository
git clone https://github.com/wolfpld/tracy.git
cd tracy

# Build profiler GUI (requires X11, OpenGL)
cd profiler/build/unix
make release

# Binary will be in: tracy/profiler/build/unix/Tracy-release
```

**Platform Requirements:**
- Linux: X11, OpenGL 3.2+
- macOS: Supported via XQuartz
- Windows: Native support

**Pre-built Binaries:**

Pre-built Tracy releases are available at: https://github.com/wolfpld/tracy/releases

#### Using Tracy with Camera Node

**1. Build with Tracy Support:**

Tracy is enabled by default. For standard profiling:

```bash
# Build with basic Tracy support (default)
cargo build --release

# Build with advanced profiling (memory, sampling)
cargo build --release --profile=profiling --features=profiling
```

**2. Run Camera Node with Tracy:**

```bash
# Start camera with Tracy enabled
./target/release/edgefirst-camera --tracy --camera /dev/video0 --jpeg --h264
```

**3. Connect Tracy Profiler:**

```bash
# In separate terminal, start Tracy GUI
./Tracy-release

# Tracy will automatically discover and connect to the camera node
```

**4. Alternative: Manual Connection:**

If automatic discovery doesn't work:
- In Tracy GUI, click "Connect" button
- Enter IP address of device running camera node
- Default port: 8086

#### Tracy Features Available

**Frame Markers:**

The camera node emits frame markers for different processing stages:
- **Main frames**: Camera capture rate (30 FPS)
- **H.264 frames**: H.264 encoding rate
- **JPEG frames**: JPEG encoding rate
- **Tile frames**: 4K tile encoding rate (if enabled)

In Tracy, switch between frame views using the frame dropdown menu.

**Performance Plots:**

Real-time plots visible in Tracy:
- `fps`: Camera frames per second
- `jpeg_kb`: JPEG compressed size in kilobytes

**Zones and Spans:**

All instrumented functions appear as zones in Tracy timeline:
- Camera read operations
- G2D conversions
- JPEG/H.264 encoding
- Zenoh publishing
- Frame distribution

Zones are automatically named from Rust function names via the `#[instrument]` attribute.

**Memory Profiling:**

When built with `profiling` feature:
- Memory allocations tracked
- Call stacks recorded
- Memory leaks detected
- Allocation statistics available

#### Profiling Workflow

**1. Identify Performance Issues:**

```bash
# Run with Tracy enabled
./edgefirst-camera --tracy --camera /dev/video0 --jpeg --h264

# In Tracy GUI:
# - Check frame timing consistency
# - Look for dropped frames (gaps in timeline)
# - Identify long-running zones
# - Monitor FPS plot
```

**2. Investigate Specific Functions:**

- Click on zones in timeline to see call stacks
- Use statistics view to find slowest functions
- Compare frame timing between good/bad frames
- Check memory allocations in hot paths

**3. Validate Optimizations:**

After making changes:
- Capture new Tracy trace
- Compare zone timings before/after
- Verify frame rate improvements
- Check for regressions in other areas

#### Common Profiling Scenarios

**Scenario 1: Dropped Frames**

If you see gaps in the main frame timeline:

1. Check FPS plot - should be steady at 30 FPS
2. Look for long-running zones (>33ms for 30 FPS)
3. Identify which encoding thread is slow:
   - Check secondary frame markers (h264, jpeg, h264_tile)
   - Compare zone timings across threads

**Scenario 2: High CPU Usage**

1. Use Tracy's CPU sampling (profiling feature)
2. Identify functions consuming most CPU time
3. Check for unexpected software fallbacks (should use G2D hardware)
4. Verify encoder threads aren't blocking each other

**Scenario 3: Memory Growth**

1. Enable memory profiling (profiling feature)
2. Capture trace over several minutes
3. Check memory plot for growth trend
4. Use allocation list to find leaks

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
- Prefer hardware acceleration (G2D, H.264 encoder)
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
cargo clippy --all-targets --all-features -- -D warnings
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
/// Encodes a frame using NXP G2D hardware acceleration.
///
/// # Arguments
/// * `src` - Source DMA buffer in YUYV format
/// * `dst_format` - Target format (NV12, RGBA, etc.)
///
/// # Returns
/// Encoded frame in destination DMA buffer
///
/// # Errors
/// Returns error if G2D device is unavailable or operation fails.
///
/// # Platform Requirements
/// Requires NXP i.MX8 G2D hardware. Falls back to software on other platforms.
///
/// # Example
/// ```no_run
/// let dst = encode_frame(&src_buffer, G2D_NV12)?;
/// ```
pub fn encode_frame(src: &Image, dst_format: G2dFormat) -> Result<Image> {
    // Implementation
}
```

### Hardware-Specific Code

**Platform Conditionals:**

```rust
#[cfg(target_arch = "aarch64")]
fn use_g2d_acceleration() -> bool {
    // Check for NXP hardware
    std::path::Path::new("/dev/galcore").exists()
}

#[cfg(not(target_arch = "aarch64"))]
fn use_g2d_acceleration() -> bool {
    false
}
```

**Fallback Implementations:**

- Always provide software fallback for hardware features
- Gracefully degrade when hardware unavailable
- Log when using fallback: `warn!("G2D unavailable, using software conversion")`

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

- Performance tests in `benches/` directory
- Use `criterion` framework
- Run with `cargo bench`

### Running Tests

**All Tests (no hardware):**

```bash
cargo test --lib
```

**Integration Tests:**

```bash
cargo test --test test_image
```

**Hardware-Dependent Tests (on device):**

```bash
cargo test -- --ignored --test-threads=1
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
    fn test_timestamp_conversion() {
        let ros_time = unix_to_ros_time(1234567890);
        assert_eq!(ros_time.sec, 1234567890);
        assert_eq!(ros_time.nanosec, 0);
    }

    #[test]
    #[ignore]  // Requires hardware
    fn test_g2d_conversion() {
        let g2d = G2dContext::new().expect("G2D device required");
        // Hardware test
    }
}
```

**Integration Test Example:**

```rust
// tests/test_jpeg_encoding.rs
use edgefirst_camera::image::*;

#[test]
fn test_jpeg_round_trip() {
    let img = create_test_image(640, 480);
    let jpeg = encode_jpeg(&img, 95).unwrap();
    assert!(jpeg.len() > 0);
    assert!(jpeg.len() < img.data.len());  // Compressed
}
```

### Coverage Requirements

- **Minimum overall**: 70%
- **Core modules** (`image.rs`, `video.rs`): 80%+
- **Public APIs**: 100%
- **Hardware paths**: Best effort (requires device)

**Coverage is enforced in CI pipeline**

---

## Pull Request Process

### Before Submitting

**Checklist:**

- [ ] Code follows Rust style guidelines (`cargo fmt`)
- [ ] All lints pass (`cargo clippy -- -D warnings`)
- [ ] Tests pass (`cargo test`)
- [ ] New functionality has tests
- [ ] Documentation is updated (README, rustdoc)
- [ ] Commit messages are clear and descriptive
- [ ] No secrets or credentials committed
- [ ] SBOM license policy compliance verified

### Creating Pull Request

1. **Push your branch** to GitHub:

   ```bash
   git push origin feature/your-feature
   ```

2. **Open PR** via GitHub web interface:
   - Base: `develop` (or `main` for hotfixes)
   - Provide clear title and description
   - Link related issues: "Fixes #123" or "Related to #456"

3. **Fill out PR template:**

   ```markdown
   ## Summary
   Brief description of changes

   ## Changes
   - Added feature X
   - Fixed bug Y
   - Updated documentation Z

   ## Testing
   - [ ] Unit tests added/updated
   - [ ] Integration tests pass
   - [ ] Tested on hardware (if applicable)

   ## Checklist
   - [ ] Code formatted with `rustfmt`
   - [ ] No `clippy` warnings
   - [ ] Tests pass
   - [ ] Documentation updated
   ```

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

- **1 approval** for merges to `develop`
- **2 approvals** for merges to `main`
- All CI checks must pass

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

See [SBOM_PROCESS.md](SBOM_PROCESS.md) for details.

---

## Development Tips

### Profiling with Tracy

```bash
# Build with profiling
cargo build --release --features profiling

# Run with Tracy enabled
./target/release/edgefirst-camera --tracy --jpeg --h264

# Connect Tracy profiler GUI
# Download from: https://github.com/wolfpld/tracy
```

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
# Set linker in .cargo/config.toml
mkdir -p .cargo
cat > .cargo/config.toml <<EOF
[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"
EOF

# Build for ARM64
cargo build --release --target aarch64-unknown-linux-gnu

# Binary at: target/aarch64-unknown-linux-gnu/release/edgefirst-camera
```

### Testing on Hardware

**Copy to Device:**

```bash
# Build ARM64 binary
cargo build --release --target aarch64-unknown-linux-gnu

# Copy to device
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

- [CONTRIBUTORS.md](CONTRIBUTORS.md) - Hall of fame
- Release notes for significant contributions
- GitHub contribution graph

---

## Thank You

Every contribution, no matter how small, helps make EdgeFirst Camera better for everyone. We appreciate your time and effort!

**Questions about contributing?** Open a [discussion](https://github.com/EdgeFirstAI/camera/discussions) or email support@au-zone.com

---

_Last updated: 2025-11-14_

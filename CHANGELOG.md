# Changelog

All notable changes to EdgeFirst Camera will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.5.0] - 2026-02-26

### Changed
- Shortened environment variable names from `EDGEFIRST_CAMERA_*` prefix to direct names
  (e.g., `CAMERA`, `H264`, `JPEG`, `STREAM_SIZE`, `MODE`) for simpler systemd and shell configuration

### Added
- Complete `camera.default` configuration reference file for systemd EnvironmentFile usage
- Published `camera.default` as a release artifact alongside platform binaries

## [2.4.0] - 2026-01-30

### Changed
- Migrated repository from Bitbucket to GitHub (EdgeFirstAI/camera)
- Updated all documentation to reference GitHub URLs
- Renamed project to EdgeFirst Camera
- Updated videostream from 1.5.5 to 2.1.4 with V4L2 CODEC API support
  - Enables hardware encoder backend selection via `VSL_CODEC_BACKEND` env var
  - V4L2 backend provides an alternative to the Hantro backend
  - Auto-detects available backend, configurable via environment variable
- Updated edgefirst-schemas to 1.5.2 with serde_cdr support
- Updated license policy with verified MIT licenses for dma-buf and dma-heap
- Moved EPL-2.0 (Zenoh) to allowed licenses category
- CI: Switched aarch64 builds from cross-compilation to native `ubuntu-22.04-arm` runner
- CI: Added native aarch64 testing on `ubuntu-22.04-arm` runner

### Added
- Complete GitHub Actions CI/CD workflows (test, build, SBOM, release)
- GitHub issue templates (bug report, feature request, hardware compatibility)
- Pull request template with comprehensive checklist
- SBOM generation and license compliance automation
- Graceful shutdown handling for SIGTERM and SIGINT signals
  - Enables proper coverage collection during CI testing
  - Clean exit when terminated by process managers (systemd, timeout)
- CI: On-target hardware testing with `nxp-imx8mp-latest` runner
  - Tests JPEG capture, H.264 encoding, and integration tests
  - Validates real camera, G2D, and VPU hardware integration
- CI: Coverage collection from on-target hardware tests

### Fixed
- Handle EINTR (Interrupted system call) during camera read gracefully
- Improved error propagation during shutdown sequence
- License compliance: All dependencies now Apache-2.0/MIT/EPL-2.0 compatible
- Code formatting consistency across all source files

## [2.3.1] - 2025-10-14

### Fixed
- Fixed issues with 4K H.264 tile encoding (EDGEAI-717)

## [2.3.0] - 2025-09-19

### Added
- 4K H.264 camera support with automatic tiling (EDGEAI-715)
- Splits 3840x2160 video into 4× 1920x1080 tiles for hardware encoder
- Frame rate capping at 15 FPS for tiled streams

### Changed
- Updated dependencies (clippy and formatting fixes)
- G2D-based tile processing for efficient hardware acceleration

## [2.2.3] - 2025-05-21

### Changed
- Changed image dimensions to use u32 instead of i32 (EDGEAI-635)
- Removed incorrect comments in g2d-sys

### Added
- Memory-mapped image support
- G2D updates from maivin-replay integration

## [2.2.2] - 2025-05-13

### Changed
- Updated deepview/rust container with videostream installed
- Build system improvements

## [2.2.1] - 2025-05-10

### Changed
- Updated dependencies to latest versions

## [2.2.0] - 2025-02-24

### Changed
- Upgraded to Rust 1.84.1
- Bitbucket Pipelines updates

## [2.1.5] - 2025-01-25

### Fixed
- Disabled Sonar audit temporarily until upstream issues resolved (EDGEAI-428)

## [2.1.4] - 2024-11-27

### Changed
- Updates from EDGEAI-196 work

## [2.1.3] - 2024-08-30

### Changed
- Updates from RVN-291 work

## [2.1.2] - 2024-05-15

### Changed
- Use monotonic time for camera info topic publishing
- Improved timestamp consistency

## [2.1.1] - 2024-05-04

### Added
- H.264 bitrate control through environment variable
- Allows runtime configuration without recompilation

## [2.1.0] - 2024-05-03

### Changed
- Applied Clippy suggestions for code quality improvements

## [2.0.8] - 2024-03-07

### Changed
- Updates from EDGEAI-171 work

## [2.0.7] - 2024-03-05

### Changed
- Updated Cargo dependencies for latest Rust version compatibility

## [2.0.6] - 2024-03-01

### Added
- Environment variable control for JPEG streaming
- Environment variable control for H264 streaming
- Flexible runtime configuration

[Unreleased]: https://github.com/EdgeFirstAI/camera/compare/v2.5.0...HEAD
[2.5.0]: https://github.com/EdgeFirstAI/camera/compare/v2.4.0...v2.5.0
[2.4.0]: https://github.com/EdgeFirstAI/camera/compare/v2.3.1...v2.4.0
[2.3.1]: https://github.com/EdgeFirstAI/camera/compare/v2.3.0...v2.3.1
[2.3.0]: https://github.com/EdgeFirstAI/camera/compare/v2.2.3...v2.3.0
[2.2.3]: https://github.com/EdgeFirstAI/camera/compare/v2.2.2...v2.2.3
[2.2.2]: https://github.com/EdgeFirstAI/camera/compare/v2.2.1...v2.2.2
[2.2.1]: https://github.com/EdgeFirstAI/camera/compare/v2.2.0...v2.2.1
[2.2.0]: https://github.com/EdgeFirstAI/camera/compare/v2.1.5...v2.2.0
[2.1.5]: https://github.com/EdgeFirstAI/camera/compare/v2.1.4...v2.1.5
[2.1.4]: https://github.com/EdgeFirstAI/camera/compare/2.1.3...2.1.4
[2.1.3]: https://github.com/EdgeFirstAI/camera/compare/2.1.2...2.1.3
[2.1.2]: https://github.com/EdgeFirstAI/camera/compare/2.1.1...2.1.2
[2.1.1]: https://github.com/EdgeFirstAI/camera/compare/2.1.0...2.1.1
[2.1.0]: https://github.com/EdgeFirstAI/camera/compare/2.0.8...2.1.0
[2.0.8]: https://github.com/EdgeFirstAI/camera/compare/2.0.7...2.0.8
[2.0.7]: https://github.com/EdgeFirstAI/camera/compare/2.0.6...2.0.7
[2.0.6]: https://github.com/EdgeFirstAI/camera/releases/tag/2.0.6

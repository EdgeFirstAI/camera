// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025 Au-Zone Technologies. All Rights Reserved.

//! # EdgeFirst Camera Node Library
//!
//! This library provides the core image processing and hardware acceleration
//! functionality for the EdgeFirst Camera Node. It enables zero-copy DMA buffer
//! management, hardware-accelerated image operations using NXP G2D, and
//! efficient JPEG encoding for camera capture pipelines.
//!
//! ## Features
//!
//! - **DMA Buffer Management**: Allocate and manage DMA-backed image buffers
//!   for zero-copy inter-process communication.
//! - **Hardware Acceleration**: Leverage NXP i.MX8 G2D hardware for format
//!   conversion, scaling, cropping, and rotation operations.
//! - **JPEG Encoding**: Hardware-optimized JPEG compression using turbojpeg
//!   with SIMD.
//! - **V4L2 Integration**: Seamless integration with V4L2 camera buffers.
//!
//! ## Example
//!
//! ```no_run
//! use edgefirst_camera::image::{Image, ImageManager, Rotation, RGBA, YUYV};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Create image manager for G2D hardware operations
//! let imgmgr = ImageManager::new()?;
//!
//! // Allocate DMA buffers for source and destination images
//! let src = Image::new(1920, 1080, YUYV)?;
//! let dst = Image::new(1920, 1080, RGBA)?;
//!
//! // Convert YUYV to RGBA using hardware acceleration
//! imgmgr.convert(&src, &dst, None, Rotation::Rotation0)?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Platform Requirements
//!
//! - **Linux**: Kernel 5.10+ with V4L2 and DMA heap support
//! - **Hardware Acceleration**: NXP i.MX8M Plus for G2D operations (software
//!   fallback available on other platforms)
//!
//! ## Safety
//!
//! This library uses `unsafe` code for FFI interactions with hardware drivers
//! and DMA buffer operations. All unsafe operations are isolated to specific
//! modules and wrapped with safe APIs.

pub mod image;

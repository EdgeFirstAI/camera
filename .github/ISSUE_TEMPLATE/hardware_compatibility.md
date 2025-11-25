---
name: Hardware Compatibility Report
about: Report compatibility results for specific hardware platforms
title: '[HARDWARE] '
labels: hardware, compatibility
assignees: ''
---

## Hardware Platform

**Board/SoM:**
- Manufacturer: [e.g., NXP, Raspberry Pi, Custom]
- Model: [e.g., i.MX8M Plus EVK, Maivin 2.0, Raspberry Pi 5]
- CPU: [e.g., ARM Cortex-A53, x86_64]
- RAM: [e.g., 2GB, 4GB, 8GB]

**Camera:**
- Interface: [e.g., MIPI CSI-2, USB UVC]
- Model: [e.g., OV5640, IMX219, Logitech C920]
- Resolution: [e.g., 1920x1080, 3840x2160]
- V4L2 device: [e.g., /dev/video0]

**Operating System:**
- Distribution: [e.g., Yocto Kirkstone, Ubuntu 22.04]
- Kernel version: [e.g., 5.15.52, 6.1.0]
- Rust version: [e.g., 1.90.0]

## Test Results

**edgefirst-camera version:** [e.g., 0.1.0, commit SHA]

**Command tested:**
```bash
edgefirst-camera --jpeg --h264 --camera /dev/video0
```

**Results:**
- [ ] ✅ Builds successfully
- [ ] ✅ Runs without errors
- [ ] ✅ DMA publishing works
- [ ] ✅ JPEG encoding works
- [ ] ✅ H.264 encoding works
- [ ] ⚠️ Partial functionality (see notes)
- [ ] ❌ Does not work (see logs)

## Performance Metrics

**Frame Rate:**
- Camera capture: ___ FPS
- JPEG encoding: ___ FPS
- H.264 encoding: ___ FPS

**Latency:**
- DMA publish: ___ ms
- JPEG publish: ___ ms
- H.264 publish: ___ ms

**Resource Usage:**
- CPU usage: ___% (single core)
- Memory footprint: ___ MB
- Power consumption: ___ W (if measured)

## Known Issues

List any issues, workarounds, or limitations discovered on this platform.

**Example:**
> H.264 encoding fails at 4K resolution due to encoder buffer size limits. Works at 1080p.

## Logs

<details>
<summary>Full logs (click to expand)</summary>

```
# Paste journalctl or stderr output
```

</details>

## Hardware Acceleration

**G2D (NXP platforms):**
- [ ] ✅ Available and working
- [ ] ⚠️ Available but issues (see notes)
- [ ] ❌ Not available on this platform
- [ ] ❓ Not tested

**Hantro H.264 Encoder (NXP platforms):**
- [ ] ✅ Available and working
- [ ] ⚠️ Available but issues (see notes)
- [ ] ❌ Not available on this platform
- [ ] ❓ Not tested

## Additional Context

Any other details about hardware-specific behavior, configuration requirements, or platform quirks.

## Checklist

- [ ] I have tested with the latest version
- [ ] I have included all hardware details
- [ ] I have provided performance metrics
- [ ] I have attached relevant logs
- [ ] I confirm this report is for EdgeFirst Camera compatibility (not a bug report)

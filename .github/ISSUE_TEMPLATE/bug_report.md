---
name: Bug Report
about: Report a bug or unexpected behavior
title: '[BUG] '
labels: bug
assignees: ''
---

## Bug Description

A clear and concise description of what the bug is.

## Steps to Reproduce

1. Run command: `edgefirst-camera --jpeg --h264`
2. Observe behavior: ...
3. See error: ...

## Expected Behavior

A clear description of what you expected to happen.

## Actual Behavior

What actually happened (including error messages).

## Environment

**Hardware:**
- Platform: [e.g., NXP i.MX8M Plus, x86_64 desktop]
- Camera: [e.g., MIPI CSI-2, USB UVC]
- Camera resolution: [e.g., 1920x1080, 3840x2160]

**Software:**
- OS: [e.g., Linux 5.15, Yocto Kirkstone]
- Rust version: [e.g., 1.90.0]
- edgefirst-camera version: [e.g., 0.1.0, commit SHA]

**Configuration:**
```bash
# Paste your command-line arguments or configuration
edgefirst-camera --jpeg --h264 --camera /dev/video0
```

## Logs

<details>
<summary>Logs (click to expand)</summary>

```
# Paste journalctl output or stderr logs
journalctl -u edgefirst-camera -n 100
```

</details>

## Additional Context

Any other context about the problem (e.g., happens only with specific cameras, works on x86_64 but not ARM64).

## Checklist

- [ ] I have searched existing issues to avoid duplicates
- [ ] I have tested with the latest version
- [ ] I have included all relevant logs and environment details
- [ ] I have provided steps to reproduce the issue

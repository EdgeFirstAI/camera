// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Checks the Rust mirror of the shim ABI against `shim/efcam_lc.h`: a C
//! program compiled from the header prints each struct's size and field
//! offsets, and the constants, and they must equal the Rust ones.
//!
//! Needs a C compiler (`$CC`, else `cc`) for the host.

use std::mem::{offset_of, size_of};
use std::path::Path;
use std::process::Command;

use edgefirst_libcamera_sys::*;

macro_rules! layout {
    ($($t:ident { $($f:ident),* $(,)? })*) => {{
        let mut c = String::new();
        let mut rust = Vec::new();
        $(
            c.push_str(&format!(
                "printf(\"{0} %zu\\n\", sizeof(struct {0}));\n",
                stringify!($t)
            ));
            rust.push(format!("{} {}", stringify!($t), size_of::<$t>()));
            $(
                c.push_str(&format!(
                    "printf(\"{0}.{1} %zu\\n\", offsetof(struct {0}, {1}));\n",
                    stringify!($t),
                    stringify!($f)
                ));
                rust.push(format!(
                    "{}.{} {}",
                    stringify!($t),
                    stringify!($f),
                    offset_of!($t, $f)
                ));
            )*
        )*
        (c, rust)
    }};
}

macro_rules! constants {
    ($($k:ident),* $(,)?) => {{
        let mut c = String::new();
        let mut rust = Vec::new();
        $(
            c.push_str(&format!(
                "printf(\"{0} %lld\\n\", (long long)({0}));\n",
                stringify!($k)
            ));
            rust.push(format!("{} {}", stringify!($k), $k as i64));
        )*
        (c, rust)
    }};
}

#[test]
fn rust_mirror_matches_the_header() {
    let (structs_c, structs_rust) = layout! {
        efcam_lc_camera_info { size, id, model, location, rotation, num_devices, devices }
        efcam_lc_format {
            size, fourcc, modifier, width, height, min_width, min_height, max_width,
            max_height, h_step, v_step,
        }
        efcam_lc_stream_request { size, role, fourcc, width, height, buffer_count, orientation }
        efcam_lc_stream_info {
            size, fourcc, modifier, width, height, stride, frame_size, buffer_count,
            orientation, adjusted,
        }
        efcam_lc_plane { fd, offset, length }
        efcam_lc_buffer { size, num_planes, planes }
        efcam_lc_control_info { size, min, max, def }
        efcam_lc_controls {
            size, frame_duration_min_us, frame_duration_max_us, exposure_mode,
            exposure_time_us, gain_mode, analogue_gain, awb_enable, colour_temperature,
            colour_gains,
        }
        efcam_lc_completion {
            size, slot, status, sequence, sensor_timestamp_ns, buffer_timestamp_ns,
            exposure_time_us, analogue_gain, frame_duration_us, colour_temperature, bytes_used,
        }
    };
    let (consts_c, consts_rust) = constants!(
        EFCAM_LC_ABI_VERSION,
        EFCAM_LC_MAX_PLANES,
        EFCAM_LC_MAX_DEVICES,
        EFCAM_LC_ROLE_RAW,
        EFCAM_LC_ROLE_STILL,
        EFCAM_LC_ROLE_VIDEO,
        EFCAM_LC_ROLE_VIEWFINDER,
        EFCAM_LC_ORIENTATION_DEFAULT,
        EFCAM_LC_ORIENTATION_ROTATE0,
        EFCAM_LC_ORIENTATION_ROTATE0_MIRROR,
        EFCAM_LC_ORIENTATION_ROTATE180,
        EFCAM_LC_ORIENTATION_ROTATE180_MIRROR,
        EFCAM_LC_ORIENTATION_ROTATE90_MIRROR,
        EFCAM_LC_ORIENTATION_ROTATE270,
        EFCAM_LC_ORIENTATION_ROTATE270_MIRROR,
        EFCAM_LC_ORIENTATION_ROTATE90,
        EFCAM_LC_CONTROL_FRAME_DURATION_LIMITS,
        EFCAM_LC_CONTROL_EXPOSURE_TIME,
        EFCAM_LC_CONTROL_ANALOGUE_GAIN,
        EFCAM_LC_CONTROL_AWB_ENABLE,
        EFCAM_LC_CONTROL_COLOUR_TEMPERATURE,
        EFCAM_LC_CONTROL_COLOUR_GAINS,
        EFCAM_LC_STATUS_OK,
        EFCAM_LC_STATUS_ERROR,
        EFCAM_LC_STATUS_CANCELLED,
        EFCAM_LC_MODE_UNSET,
        EFCAM_LC_MODE_AUTO,
        EFCAM_LC_MODE_MANUAL,
    );

    let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("shim");
    let out = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let src = out.join("efcam_lc_layout.c");
    let exe = out.join("efcam_lc_layout");
    std::fs::write(
        &src,
        format!(
            "#include <stddef.h>\n#include <stdio.h>\n#include \"efcam_lc.h\"\n\
             int main(void) {{\n{structs_c}{consts_c}return 0;\n}}\n"
        ),
    )
    .unwrap();
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .arg("-I")
        .arg(&shim)
        .arg(&src)
        .arg("-o")
        .arg(&exe)
        .status()
        .unwrap_or_else(|e| panic!("cannot run the C compiler {cc}: {e}"));
    assert!(status.success(), "compiling the layout probe failed");
    let printed = Command::new(&exe).output().unwrap();
    let c: Vec<String> = String::from_utf8(printed.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let rust: Vec<String> = structs_rust.into_iter().chain(consts_rust).collect();
    for (c, r) in c.iter().zip(&rust) {
        assert_eq!(c, r, "header (left) and Rust (right) differ");
    }
    assert_eq!(c.len(), rust.len());
}

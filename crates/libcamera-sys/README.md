# edgefirst-libcamera-sys

Run-time bindings to the EdgeFirst libcamera shim: a small C library over libcamera's C++ API with a stable C ABI. The [`edgefirst-camera`](../camera) SDK uses it for its libcamera backend. Nothing here links libcamera: the shim is found and loaded with `dlopen` when a libcamera camera is first used, so a binary built once runs with or without libcamera installed.

## Why a shim

libcamera has only a C++ API, and its SONAME pins a minor version (`libcamera.so.0.7`). Between minors, symbols change (0.6 → 0.7 moved `Request::metadata()` out of line) and, before 0.5, control ids were renumbered. The shim keeps all of that on the C++ side:

- Only plain C types cross the ABI (`shim/efcam_lc.h`), and every struct starts with its size, so fields can be added without an ABI change.
- libcamera control ids never cross it. Controls are named fields that the shim resolves against the headers it was built with.
- Completions arrive on libcamera's thread, which only queues them and signals an eventfd; every call into libcamera is made from the caller's thread.
- `EFCAM_LC_ABI_VERSION`, the SONAME (`libedgefirst_camera_libcamera.so.1`) and the symbol version (`EFCAM_LC_1`) change only on incompatible changes. A shim speaking another ABI is refused.

One shim is built per libcamera minor, 0.5 and newer.

## Finding the shim

`Shim::load()` tries, in order:

1. the path in `EDGEFIRST_CAMERA_LIBCAMERA_SHIM`, and nothing else;
2. `libedgefirst_camera_libcamera.so.1` on the library search path, as a Yocto image or distribution package installs it;
3. `libcamera-0.<minor>/libedgefirst_camera_libcamera.so.1` for each installed libcamera minor, newest first, under `EDGEFIRST_CAMERA_LIBCAMERA_SHIM_DIR`, the executable's directory, its `../lib/edgefirst-camera`, `/usr/local/lib/edgefirst-camera` and `/usr/lib/edgefirst-camera`.

A shim built for another minor fails to load because its `DT_NEEDED` names that minor, so the first shim that loads matches the installed libcamera.

## Building the shim

On a system or Yocto image with libcamera's development files:

```sh
meson setup build crates/libcamera-sys/shim
ninja -C build
```

The portable prebuilts are built per minor in Debian bookworm (GCC 12, so they need no newer libstdc++ than `GLIBCXX_3.4.29` and glibc 2.32), against the upstream `vX.Y.0` tag:

```sh
docker run --rm -v $PWD/crates/libcamera-sys/shim:/shim:ro -v $PWD/out:/out debian:bookworm \
    /shim/build-prebuilt.sh v0.7.0 /out [aarch64-linux-gnu]
```

The script builds libcamera's core library without pipelines, builds the shim against it, and runs `check-symbols.sh`: the shim exports only `efcam_lc_*`, every libcamera symbol it imports is exported by that libcamera, and its glibc and libstdc++ requirements stay within the oldest supported target. A native build also loads it and starts a `CameraManager`. CI does this for 0.5, 0.6 and 0.7 on x86_64 and aarch64. The 0.7 prebuilt runs unchanged on the NXP i.MX 95 (libcamera 0.7.1 with the `nxp/neo` pipeline) and the Raspberry Pi 5 (0.7.0+rpt).

zig cannot build the shim: its libc++ is ABI-incompatible with the libstdc++ libcamera is built with.

## Testing without libcamera

The `mock` feature (Linux) provides `mock::functions()`, an in-process implementation of the ABI over memfd buffers, and `mock::configure()` to set the cameras it presents. `tests/layout.rs` compiles the header with the host C compiler and checks every struct size, field offset and constant against the Rust mirror in `abi.rs`.

## License

Apache-2.0. libcamera is LGPL-2.1-or-later and is loaded dynamically, never linked or redistributed.

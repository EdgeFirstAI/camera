# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

project('edgefirst-camera-libcamera', 'cpp',
    version : '1.0.0',
    license : 'Apache-2.0',
    meson_version : '>= 0.63',
    default_options : ['cpp_std=c++17', 'warning_level=2'])

libcamera_dep = dependency('libcamera', version : '>= 0.5')

shared_library('edgefirst_camera_libcamera',
    'efcam_lc.cpp',
    dependencies : libcamera_dep,
    gnu_symbol_visibility : 'hidden',
    link_args : [
        '-Wl,--version-script=' + meson.current_source_dir() / 'efcam_lc.map',
        '-Wl,-z,defs',
        '-Wl,--as-needed',
    ],
    link_depends : 'efcam_lc.map',
    soversion : '1',
    install : true)

install_headers('efcam_lc.h', subdir : 'edgefirst')

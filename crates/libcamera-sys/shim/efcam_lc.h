/* SPDX-License-Identifier: Apache-2.0 */
/* Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved. */

/*
 * C ABI between the edgefirst-camera libcamera backend, which loads this
 * library at run time, and libcamera.
 *
 * - Only plain C types cross the boundary; no libcamera or STL types.
 * - Functions return 0 or a count on success and -errno on failure.
 * - Every struct starts with `size`, which the caller sets to the size of
 *   the struct as it was compiled. The shim reads and writes only that many
 *   bytes, so either side can append fields without an ABI change.
 * - libcamera control ids never cross the boundary. Controls are named
 *   fields resolved against the headers the shim was built with, so the
 *   numbering of each libcamera release stays inside the shim.
 * - EFCAM_LC_ABI_VERSION, the SONAME and the symbol version node change
 *   only on incompatible changes.
 * - Completions are recorded on libcamera's thread and handed out through
 *   an eventfd; no call into this library runs on libcamera's thread.
 */
#ifndef EFCAM_LC_H
#define EFCAM_LC_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define EFCAM_LC_API __attribute__((visibility("default")))

#define EFCAM_LC_ABI_VERSION 1u
#define EFCAM_LC_MAX_PLANES 4
#define EFCAM_LC_MAX_DEVICES 8

typedef struct efcam_lc_mgr efcam_lc_mgr;
typedef struct efcam_lc_cam efcam_lc_cam;

/* libcamera::StreamRole. */
enum efcam_lc_role {
	EFCAM_LC_ROLE_RAW = 0,
	EFCAM_LC_ROLE_STILL = 1,
	EFCAM_LC_ROLE_VIDEO = 2,
	EFCAM_LC_ROLE_VIEWFINDER = 3,
};

/* libcamera::Orientation, with 0 for "leave the pipeline's default". */
enum efcam_lc_orientation {
	EFCAM_LC_ORIENTATION_DEFAULT = 0,
	EFCAM_LC_ORIENTATION_ROTATE0 = 1,
	EFCAM_LC_ORIENTATION_ROTATE0_MIRROR = 2,
	EFCAM_LC_ORIENTATION_ROTATE180 = 3,
	EFCAM_LC_ORIENTATION_ROTATE180_MIRROR = 4,
	EFCAM_LC_ORIENTATION_ROTATE90_MIRROR = 5,
	EFCAM_LC_ORIENTATION_ROTATE270 = 6,
	EFCAM_LC_ORIENTATION_ROTATE270_MIRROR = 7,
	EFCAM_LC_ORIENTATION_ROTATE90 = 8,
};

/* Controls efcam_lc_cam_control_info() reports. */
enum efcam_lc_control {
	/* Microseconds; min and max are the shortest and longest frame. */
	EFCAM_LC_CONTROL_FRAME_DURATION_LIMITS = 0,
	/* Microseconds. */
	EFCAM_LC_CONTROL_EXPOSURE_TIME = 1,
	EFCAM_LC_CONTROL_ANALOGUE_GAIN = 2,
	EFCAM_LC_CONTROL_AWB_ENABLE = 3,
	/* Kelvin. */
	EFCAM_LC_CONTROL_COLOUR_TEMPERATURE = 4,
	/* Red and blue gains; min and max apply to both. */
	EFCAM_LC_CONTROL_COLOUR_GAINS = 5,
};

enum efcam_lc_status {
	EFCAM_LC_STATUS_OK = 0,
	EFCAM_LC_STATUS_ERROR = 1,
	EFCAM_LC_STATUS_CANCELLED = 2,
};

enum efcam_lc_mode {
	EFCAM_LC_MODE_UNSET = -1,
	EFCAM_LC_MODE_AUTO = 0,
	EFCAM_LC_MODE_MANUAL = 1,
};

struct efcam_lc_camera_info {
	uint32_t size;
	char id[256];
	char model[64];
	/* properties::Location, or -1 when the camera does not report it. */
	int32_t location;
	/* properties::Rotation in degrees, or -1. */
	int32_t rotation;
	/* properties::SystemDevices: the dev_t of each V4L2 node in use. */
	uint32_t num_devices;
	uint64_t devices[EFCAM_LC_MAX_DEVICES];
};

/*
 * One entry of the stream formats a role offers: a discrete size, or a
 * range (width and height 0) when the pipeline lists none.
 */
struct efcam_lc_format {
	uint32_t size;
	uint32_t fourcc; /* DRM fourcc */
	uint64_t modifier;
	uint32_t width;
	uint32_t height;
	uint32_t min_width;
	uint32_t min_height;
	uint32_t max_width;
	uint32_t max_height;
	uint32_t h_step;
	uint32_t v_step;
};

struct efcam_lc_stream_request {
	uint32_t size;
	uint32_t role;         /* enum efcam_lc_role */
	uint32_t fourcc;       /* DRM fourcc, 0 for the pipeline default */
	uint32_t width;        /* 0 for the pipeline default */
	uint32_t height;
	uint32_t buffer_count; /* 0 for the pipeline default */
	uint32_t orientation;  /* enum efcam_lc_orientation */
};

struct efcam_lc_stream_info {
	uint32_t size;
	uint32_t fourcc;
	uint64_t modifier;
	uint32_t width;
	uint32_t height;
	uint32_t stride; /* bytes per line of plane 0 */
	uint32_t frame_size;
	uint32_t buffer_count;
	uint32_t orientation;
	/* 1 when validate() adjusted the request. */
	uint32_t adjusted;
};

struct efcam_lc_plane {
	int32_t fd; /* borrowed; dup() it to keep it */
	uint32_t offset;
	uint32_t length;
};

struct efcam_lc_buffer {
	uint32_t size;
	uint32_t num_planes;
	struct efcam_lc_plane planes[EFCAM_LC_MAX_PLANES];
};

struct efcam_lc_control_info {
	uint32_t size;
	double min;
	double max;
	double def;
};

/* Controls for a start or a request. Unset fields leave the control alone. */
struct efcam_lc_controls {
	uint32_t size;
	int64_t frame_duration_min_us; /* 0 = unset */
	int64_t frame_duration_max_us;
	int32_t exposure_mode;         /* enum efcam_lc_mode */
	int32_t exposure_time_us;      /* 0 = unset */
	int32_t gain_mode;             /* enum efcam_lc_mode */
	float analogue_gain;           /* 0 = unset */
	int32_t awb_enable;            /* -1 = unset */
	int32_t colour_temperature;    /* 0 = unset */
	float colour_gains[2];         /* red, blue; 0 = unset */
};

struct efcam_lc_completion {
	uint32_t size;
	uint32_t slot;
	uint32_t status; /* enum efcam_lc_status */
	uint32_t sequence;
	int64_t sensor_timestamp_ns; /* controls::SensorTimestamp, -1 if absent */
	uint64_t buffer_timestamp_ns; /* FrameMetadata::timestamp */
	int64_t exposure_time_us;     /* -1 if absent */
	double analogue_gain;         /* < 0 if absent */
	int64_t frame_duration_us;    /* -1 if absent */
	int32_t colour_temperature;   /* -1 if absent */
	uint32_t bytes_used[EFCAM_LC_MAX_PLANES];
};

EFCAM_LC_API uint32_t efcam_lc_abi_version(void);
/* The libcamera version the shim was built against, e.g. "0.7.0". */
EFCAM_LC_API const char *efcam_lc_build_version(void);
/* The loaded libcamera's version string (CameraManager::version()). */
EFCAM_LC_API const char *efcam_lc_runtime_version(void);

/* Starts the process's CameraManager; libcamera allows one at a time. */
EFCAM_LC_API int efcam_lc_mgr_create(efcam_lc_mgr **out);
EFCAM_LC_API void efcam_lc_mgr_destroy(efcam_lc_mgr *mgr);
EFCAM_LC_API int efcam_lc_mgr_camera_count(efcam_lc_mgr *mgr);
EFCAM_LC_API int efcam_lc_mgr_camera_info(efcam_lc_mgr *mgr, uint32_t index,
					  struct efcam_lc_camera_info *info);
/*
 * Writes up to `capacity` formats of camera `id` for `role` into `out`, an
 * array whose element size is out[0].size, and returns how many exist.
 */
EFCAM_LC_API int efcam_lc_mgr_formats(efcam_lc_mgr *mgr, const char *id,
				      uint32_t role, struct efcam_lc_format *out,
				      uint32_t capacity);

/* Acquires camera `id`: -ENODEV when absent, -EBUSY when held. */
EFCAM_LC_API int efcam_lc_cam_open(efcam_lc_mgr *mgr, const char *id,
				   efcam_lc_cam **out);
/* Configures one stream; -EINVAL when invalid, -EBUSY with buffers allocated. */
EFCAM_LC_API int efcam_lc_cam_configure(efcam_lc_cam *cam,
					const struct efcam_lc_stream_request *req,
					struct efcam_lc_stream_info *info);
/* 0 and `info` when the configured camera offers `control`, else -ENOENT. */
EFCAM_LC_API int efcam_lc_cam_control_info(efcam_lc_cam *cam, uint32_t control,
					   struct efcam_lc_control_info *info);
/* Allocates the stream's buffers, one request per buffer; returns the count. */
EFCAM_LC_API int efcam_lc_cam_alloc(efcam_lc_cam *cam);
EFCAM_LC_API int efcam_lc_cam_buffer(efcam_lc_cam *cam, uint32_t slot,
				     struct efcam_lc_buffer *buf);
/* Starts the camera and queues every request. */
EFCAM_LC_API int efcam_lc_cam_start(efcam_lc_cam *cam,
				    const struct efcam_lc_controls *ctrls);
/* An EFD_NONBLOCK eventfd, readable while completions are pending. */
EFCAM_LC_API int efcam_lc_cam_event_fd(efcam_lc_cam *cam);
/* Returns 1 and the oldest completion, or 0 when none is pending. */
EFCAM_LC_API int efcam_lc_cam_dequeue(efcam_lc_cam *cam,
				      struct efcam_lc_completion *c);
/* Reuses the request of `slot`, keeping its buffer, and queues it. */
EFCAM_LC_API int efcam_lc_cam_requeue(efcam_lc_cam *cam, uint32_t slot,
				      const struct efcam_lc_controls *ctrls);
/* Stops the camera; in-flight requests complete or are cancelled first. */
EFCAM_LC_API int efcam_lc_cam_stop(efcam_lc_cam *cam);
/* Stops if needed, destroys the requests and frees the buffers. */
EFCAM_LC_API int efcam_lc_cam_free(efcam_lc_cam *cam);
/* Frees everything and releases the camera. */
EFCAM_LC_API void efcam_lc_cam_close(efcam_lc_cam *cam);

#ifdef __cplusplus
}
#endif

#endif

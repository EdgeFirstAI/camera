/* SPDX-License-Identifier: Apache-2.0 */
/* Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved. */

#include "efcam_lc.h"

#include <algorithm>
#include <array>
#include <cerrno>
#include <cstring>
#include <deque>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include <sys/eventfd.h>
#include <unistd.h>

#include <libcamera/libcamera.h>

using namespace libcamera;

struct efcam_lc_mgr {
	std::unique_ptr<CameraManager> cm;
};

struct efcam_lc_cam {
	std::shared_ptr<Camera> camera;
	std::unique_ptr<CameraConfiguration> config;
	Stream *stream = nullptr;
	std::unique_ptr<FrameBufferAllocator> allocator;
	std::vector<std::unique_ptr<Request>> requests;
	std::mutex lock;
	std::deque<efcam_lc_completion> done;
	int efd = -1;
	bool running = false;

	void requestComplete(Request *request);
};

namespace {

/* Copies the caller's struct, which may be shorter or longer than ours. */
template<typename T>
T read_in(const T *in)
{
	T v{};
	if (in)
		std::memcpy(&v, in, std::min<size_t>(in->size, sizeof(T)));
	v.size = sizeof(T);
	return v;
}

/* Writes as much of `v` as the caller's struct holds. */
template<typename T>
void write_out(T *out, T v)
{
	uint32_t size = out->size;
	v.size = size;
	std::memcpy(out, &v, std::min<size_t>(size, sizeof(T)));
}

const StreamRole roles[] = { StreamRole::Raw, StreamRole::StillCapture,
			     StreamRole::VideoRecording, StreamRole::Viewfinder };

/* The first element of a scalar or array control value, as a double. */
double to_double(const ControlValue &v)
{
	if (v.isNone())
		return 0.0;
	bool arr = v.isArray();
	switch (v.type()) {
	case ControlTypeBool:
		return arr ? v.get<Span<const bool>>()[0] : v.get<bool>();
	case ControlTypeByte:
		return arr ? v.get<Span<const uint8_t>>()[0] : v.get<uint8_t>();
	case ControlTypeInteger32:
		return arr ? v.get<Span<const int32_t>>()[0] : v.get<int32_t>();
	case ControlTypeInteger64:
		return double(arr ? v.get<Span<const int64_t>>()[0] : v.get<int64_t>());
	case ControlTypeFloat:
		return arr ? v.get<Span<const float>>()[0] : v.get<float>();
	default:
		return 0.0;
	}
}

const ControlId *control_id(uint32_t control)
{
	switch (control) {
	case EFCAM_LC_CONTROL_FRAME_DURATION_LIMITS:
		return &controls::FrameDurationLimits;
	case EFCAM_LC_CONTROL_EXPOSURE_TIME:
		return &controls::ExposureTime;
	case EFCAM_LC_CONTROL_ANALOGUE_GAIN:
		return &controls::AnalogueGain;
	case EFCAM_LC_CONTROL_AWB_ENABLE:
		return &controls::AwbEnable;
	case EFCAM_LC_CONTROL_COLOUR_TEMPERATURE:
		return &controls::ColourTemperature;
	case EFCAM_LC_CONTROL_COLOUR_GAINS:
		return &controls::ColourGains;
	default:
		return nullptr;
	}
}

void apply_controls(const Camera &camera, ControlList &list,
		    const efcam_lc_controls *in)
{
	if (!in)
		return;
	efcam_lc_controls c = read_in(in);
	const ControlInfoMap &info = camera.controls();

	if (c.frame_duration_min_us > 0 && c.frame_duration_max_us > 0) {
		std::array<int64_t, 2> lim{ c.frame_duration_min_us,
					    c.frame_duration_max_us };
		list.set(controls::FrameDurationLimits, Span<const int64_t, 2>(lim));
	}

	/*
	 * Pipelines with per-parameter modes take them directly. Elsewhere
	 * AeEnable covers both, and is on unless either is manual.
	 */
	if (info.count(&controls::ExposureTimeMode) ||
	    info.count(&controls::AnalogueGainMode)) {
		if (c.exposure_mode != EFCAM_LC_MODE_UNSET)
			list.set(controls::ExposureTimeMode,
				 c.exposure_mode == EFCAM_LC_MODE_MANUAL
					 ? controls::ExposureTimeModeManual
					 : controls::ExposureTimeModeAuto);
		if (c.gain_mode != EFCAM_LC_MODE_UNSET)
			list.set(controls::AnalogueGainMode,
				 c.gain_mode == EFCAM_LC_MODE_MANUAL
					 ? controls::AnalogueGainModeManual
					 : controls::AnalogueGainModeAuto);
	} else if (c.exposure_mode != EFCAM_LC_MODE_UNSET ||
		   c.gain_mode != EFCAM_LC_MODE_UNSET) {
		list.set(controls::AeEnable,
			 c.exposure_mode != EFCAM_LC_MODE_MANUAL &&
				 c.gain_mode != EFCAM_LC_MODE_MANUAL);
	}
	if (c.exposure_time_us > 0)
		list.set(controls::ExposureTime, c.exposure_time_us);
	if (c.analogue_gain > 0)
		list.set(controls::AnalogueGain, c.analogue_gain);
	if (c.awb_enable >= 0)
		list.set(controls::AwbEnable, c.awb_enable != 0);
	if (c.colour_temperature > 0)
		list.set(controls::ColourTemperature, c.colour_temperature);
	if (c.colour_gains[0] > 0 && c.colour_gains[1] > 0) {
		std::array<float, 2> g{ c.colour_gains[0], c.colour_gains[1] };
		list.set(controls::ColourGains, Span<const float, 2>(g));
	}
}

void copy_str(char *dst, size_t len, const std::string &src)
{
	std::memset(dst, 0, len);
	std::strncpy(dst, src.c_str(), len - 1);
}

} /* namespace */

/* Runs on libcamera's thread: record and signal only. */
void efcam_lc_cam::requestComplete(Request *request)
{
	efcam_lc_completion c{};
	c.size = sizeof(c);
	c.slot = uint32_t(request->cookie());
	c.status = request->status() == Request::RequestCancelled
			   ? EFCAM_LC_STATUS_CANCELLED
			   : EFCAM_LC_STATUS_OK;

	const ControlList &md = request->metadata();
	c.sensor_timestamp_ns = md.get(controls::SensorTimestamp).value_or(-1);
	c.exposure_time_us = md.get(controls::ExposureTime).value_or(-1);
	c.analogue_gain = md.get(controls::AnalogueGain).value_or(-1.0f);
	c.frame_duration_us = md.get(controls::FrameDuration).value_or(-1);
	c.colour_temperature = md.get(controls::ColourTemperature).value_or(-1);

	FrameBuffer *fb = request->findBuffer(stream);
	if (fb) {
		const FrameMetadata &fm = fb->metadata();
		if (fm.status == FrameMetadata::FrameError &&
		    c.status == EFCAM_LC_STATUS_OK)
			c.status = EFCAM_LC_STATUS_ERROR;
		c.sequence = fm.sequence;
		c.buffer_timestamp_ns = fm.timestamp;
		auto planes = fm.planes();
		for (size_t i = 0; i < planes.size() && i < EFCAM_LC_MAX_PLANES; i++)
			c.bytes_used[i] = planes[i].bytesused;
	}

	{
		std::lock_guard<std::mutex> g(lock);
		done.push_back(c);
	}
	uint64_t one = 1;
	ssize_t r = write(efd, &one, sizeof(one));
	(void)r;
}

extern "C" {

uint32_t efcam_lc_abi_version(void)
{
	return EFCAM_LC_ABI_VERSION;
}

const char *efcam_lc_build_version(void)
{
	static const std::string v = std::to_string(LIBCAMERA_VERSION_MAJOR) + "." +
				     std::to_string(LIBCAMERA_VERSION_MINOR) + "." +
				     std::to_string(LIBCAMERA_VERSION_PATCH);
	return v.c_str();
}

const char *efcam_lc_runtime_version(void)
{
	return CameraManager::version().c_str();
}

int efcam_lc_mgr_create(efcam_lc_mgr **out)
{
	auto m = std::make_unique<efcam_lc_mgr>();
	m->cm = std::make_unique<CameraManager>();
	int ret = m->cm->start();
	if (ret < 0)
		return ret;
	*out = m.release();
	return 0;
}

void efcam_lc_mgr_destroy(efcam_lc_mgr *mgr)
{
	if (!mgr)
		return;
	mgr->cm->stop();
	delete mgr;
}

int efcam_lc_mgr_camera_count(efcam_lc_mgr *mgr)
{
	return int(mgr->cm->cameras().size());
}

int efcam_lc_mgr_camera_info(efcam_lc_mgr *mgr, uint32_t index,
			     struct efcam_lc_camera_info *info)
{
	auto cams = mgr->cm->cameras();
	if (index >= cams.size())
		return -ENOENT;
	const auto &cam = cams[index];
	const ControlList &props = cam->properties();

	efcam_lc_camera_info v{};
	copy_str(v.id, sizeof(v.id), cam->id());
	if (auto model = props.get(properties::Model))
		copy_str(v.model, sizeof(v.model), std::string(*model));
	v.location = props.get(properties::Location).value_or(-1);
	v.rotation = props.get(properties::Rotation).value_or(-1);
	if (auto devs = props.get(properties::SystemDevices)) {
		for (int64_t d : *devs) {
			if (v.num_devices == EFCAM_LC_MAX_DEVICES)
				break;
			v.devices[v.num_devices++] = uint64_t(d);
		}
	}
	write_out(info, v);
	return 0;
}

int efcam_lc_mgr_formats(efcam_lc_mgr *mgr, const char *id, uint32_t role,
			 struct efcam_lc_format *out, uint32_t capacity)
{
	if (role > EFCAM_LC_ROLE_VIEWFINDER)
		return -EINVAL;
	std::shared_ptr<Camera> camera = mgr->cm->get(id);
	if (!camera)
		return -ENODEV;
	auto config = camera->generateConfiguration({ roles[role] });
	if (!config || config->size() != 1)
		return -EINVAL;

	std::vector<efcam_lc_format> all;
	const StreamFormats &formats = config->at(0).formats();
	for (const PixelFormat &pf : formats.pixelformats()) {
		efcam_lc_format f{};
		f.size = sizeof(f);
		f.fourcc = pf.fourcc();
		f.modifier = pf.modifier();
		std::vector<Size> sizes = formats.sizes(pf);
		if (sizes.empty()) {
			SizeRange r = formats.range(pf);
			f.min_width = r.min.width;
			f.min_height = r.min.height;
			f.max_width = r.max.width;
			f.max_height = r.max.height;
			f.h_step = r.hStep;
			f.v_step = r.vStep;
			all.push_back(f);
			continue;
		}
		for (const Size &s : sizes) {
			f.width = s.width;
			f.height = s.height;
			all.push_back(f);
		}
	}

	if (capacity > 0 && out) {
		uint32_t stride = out->size;
		if (stride == 0)
			return -EINVAL;
		auto *base = reinterpret_cast<unsigned char *>(out);
		for (size_t i = 0; i < all.size() && i < capacity; i++) {
			auto *e = reinterpret_cast<efcam_lc_format *>(base + i * stride);
			e->size = stride;
			write_out(e, all[i]);
		}
	}
	return int(all.size());
}

int efcam_lc_cam_open(efcam_lc_mgr *mgr, const char *id, efcam_lc_cam **out)
{
	std::shared_ptr<Camera> camera = mgr->cm->get(id);
	if (!camera)
		return -ENODEV;
	int ret = camera->acquire();
	if (ret < 0)
		return ret;
	auto c = std::make_unique<efcam_lc_cam>();
	c->camera = camera;
	c->efd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
	if (c->efd < 0) {
		ret = -errno;
		camera->release();
		return ret;
	}
	c->camera->requestCompleted.connect(c.get(), &efcam_lc_cam::requestComplete);
	*out = c.release();
	return 0;
}

int efcam_lc_cam_configure(efcam_lc_cam *cam,
			   const struct efcam_lc_stream_request *in,
			   struct efcam_lc_stream_info *info)
{
	efcam_lc_stream_request req = read_in(in);
	if (req.role > EFCAM_LC_ROLE_VIEWFINDER ||
	    req.orientation > EFCAM_LC_ORIENTATION_ROTATE90)
		return -EINVAL;
	if (cam->running || cam->allocator)
		return -EBUSY;

	auto config = cam->camera->generateConfiguration({ roles[req.role] });
	if (!config || config->size() != 1)
		return -EINVAL;
	StreamConfiguration &sc = config->at(0);
	if (req.fourcc)
		sc.pixelFormat = PixelFormat(req.fourcc);
	if (req.width && req.height)
		sc.size = Size(req.width, req.height);
	if (req.buffer_count)
		sc.bufferCount = req.buffer_count;
	if (req.orientation != EFCAM_LC_ORIENTATION_DEFAULT)
		config->orientation = Orientation(req.orientation);

	CameraConfiguration::Status st = config->validate();
	if (st == CameraConfiguration::Invalid)
		return -EINVAL;
	int ret = cam->camera->configure(config.get());
	if (ret < 0)
		return ret;

	cam->config = std::move(config);
	StreamConfiguration &applied = cam->config->at(0);
	cam->stream = applied.stream();

	efcam_lc_stream_info v{};
	v.fourcc = applied.pixelFormat.fourcc();
	v.modifier = applied.pixelFormat.modifier();
	v.width = applied.size.width;
	v.height = applied.size.height;
	v.stride = applied.stride;
	v.frame_size = applied.frameSize;
	v.buffer_count = applied.bufferCount;
	v.orientation = uint32_t(cam->config->orientation);
	v.adjusted = st == CameraConfiguration::Adjusted;
	write_out(info, v);
	return 0;
}

int efcam_lc_cam_control_info(efcam_lc_cam *cam, uint32_t control,
			      struct efcam_lc_control_info *info)
{
	const ControlId *id = control_id(control);
	if (!id)
		return -EINVAL;
	const ControlInfoMap &map = cam->camera->controls();
	auto it = map.find(id);
	if (it == map.end())
		return -ENOENT;
	efcam_lc_control_info v{};
	v.min = to_double(it->second.min());
	v.max = to_double(it->second.max());
	v.def = to_double(it->second.def());
	write_out(info, v);
	return 0;
}

int efcam_lc_cam_alloc(efcam_lc_cam *cam)
{
	if (!cam->stream)
		return -EINVAL;
	if (cam->allocator)
		return -EBUSY;
	auto allocator = std::make_unique<FrameBufferAllocator>(cam->camera);
	int ret = allocator->allocate(cam->stream);
	if (ret < 0)
		return ret;
	const auto &bufs = allocator->buffers(cam->stream);
	std::vector<std::unique_ptr<Request>> requests;
	for (size_t i = 0; i < bufs.size(); i++) {
		auto r = cam->camera->createRequest(i);
		if (!r)
			return -ENOMEM;
		ret = r->addBuffer(cam->stream, bufs[i].get());
		if (ret < 0)
			return ret;
		requests.push_back(std::move(r));
	}
	cam->allocator = std::move(allocator);
	cam->requests = std::move(requests);
	return int(cam->requests.size());
}

int efcam_lc_cam_buffer(efcam_lc_cam *cam, uint32_t slot,
			struct efcam_lc_buffer *buf)
{
	if (!cam->allocator)
		return -EINVAL;
	const auto &bufs = cam->allocator->buffers(cam->stream);
	if (slot >= bufs.size())
		return -ENOENT;
	const auto &planes = bufs[slot]->planes();
	if (planes.size() > EFCAM_LC_MAX_PLANES)
		return -E2BIG;
	efcam_lc_buffer v{};
	v.num_planes = planes.size();
	for (size_t i = 0; i < planes.size(); i++) {
		v.planes[i].fd = planes[i].fd.get();
		v.planes[i].offset = planes[i].offset;
		v.planes[i].length = planes[i].length;
	}
	write_out(buf, v);
	return 0;
}

int efcam_lc_cam_start(efcam_lc_cam *cam, const struct efcam_lc_controls *ctrls)
{
	if (cam->running)
		return -EBUSY;
	if (cam->requests.empty())
		return -EINVAL;
	ControlList start(controls::controls);
	apply_controls(*cam->camera, start, ctrls);
	int ret = cam->camera->start(&start);
	if (ret < 0)
		return ret;
	cam->running = true;
	/* Requests that completed or were cancelled before must be reused. */
	for (auto &r : cam->requests) {
		r->reuse(Request::ReuseBuffers);
		ret = cam->camera->queueRequest(r.get());
		if (ret < 0) {
			cam->camera->stop();
			cam->running = false;
			return ret;
		}
	}
	return 0;
}

int efcam_lc_cam_event_fd(efcam_lc_cam *cam)
{
	return cam->efd;
}

int efcam_lc_cam_dequeue(efcam_lc_cam *cam, struct efcam_lc_completion *c)
{
	uint64_t v;
	ssize_t r = read(cam->efd, &v, sizeof(v));
	(void)r;
	efcam_lc_completion next;
	{
		std::lock_guard<std::mutex> g(cam->lock);
		if (cam->done.empty())
			return 0;
		next = cam->done.front();
		cam->done.pop_front();
		if (!cam->done.empty()) {
			uint64_t one = 1;
			r = write(cam->efd, &one, sizeof(one));
		}
	}
	write_out(c, next);
	return 1;
}

int efcam_lc_cam_requeue(efcam_lc_cam *cam, uint32_t slot,
			 const struct efcam_lc_controls *ctrls)
{
	if (slot >= cam->requests.size())
		return -ENOENT;
	if (!cam->running)
		return -ESHUTDOWN;
	Request *r = cam->requests[slot].get();
	r->reuse(Request::ReuseBuffers);
	apply_controls(*cam->camera, r->controls(), ctrls);
	return cam->camera->queueRequest(r);
}

int efcam_lc_cam_stop(efcam_lc_cam *cam)
{
	if (!cam->running)
		return 0;
	int ret = cam->camera->stop();
	cam->running = false;
	return ret;
}

int efcam_lc_cam_free(efcam_lc_cam *cam)
{
	int ret = efcam_lc_cam_stop(cam);
	cam->requests.clear();
	if (cam->allocator && cam->stream) {
		int r = cam->allocator->free(cam->stream);
		if (ret == 0)
			ret = r;
	}
	cam->allocator.reset();
	std::lock_guard<std::mutex> g(cam->lock);
	cam->done.clear();
	return ret;
}

void efcam_lc_cam_close(efcam_lc_cam *cam)
{
	if (!cam)
		return;
	efcam_lc_cam_free(cam);
	cam->camera->requestCompleted.disconnect(cam);
	cam->config.reset();
	cam->camera->release();
	cam->camera.reset();
	if (cam->efd >= 0)
		close(cam->efd);
	delete cam;
}

} /* extern "C" */

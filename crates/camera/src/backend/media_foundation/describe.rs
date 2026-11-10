// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Devices, native media types and their description.

use edgefirst_tensor::{ColorEncoding, ColorRange, Colorimetry};
use windows::core::GUID;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaSource, IMFMediaType, IMFSourceReader, MFCreateAttributes,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFNominalRange_0_255,
    MFNominalRange_16_235, MFVideoFormat_ARGB32, MFVideoFormat_I420, MFVideoFormat_IYUV,
    MFVideoFormat_NV12, MFVideoFormat_RGB24, MFVideoFormat_RGB32, MFVideoFormat_UYVY,
    MFVideoFormat_YUY2, MFVideoFormat_YV12, MFVideoTransferMatrix_BT601,
    MFVideoTransferMatrix_BT709, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_E_NO_MORE_TYPES, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX,
};
use windows::Win32::System::Com::CoTaskMemFree;

use super::com::{error, string, Agile};
use super::select::{self, NativeType, Packing, Subtype};
use super::STREAM;
use crate::{Backend, CameraDescriptor, Error, ErrorKind, FormatInfo, Result};

/// The uncompressed subtypes the SDK captures, and how each is laid out.
const PACKINGS: [(GUID, Packing); 9] = [
    (MFVideoFormat_NV12, Packing::Nv12),
    (MFVideoFormat_I420, Packing::I420),
    (MFVideoFormat_IYUV, Packing::I420),
    (MFVideoFormat_YV12, Packing::Yv12),
    (MFVideoFormat_YUY2, Packing::Yuy2),
    (MFVideoFormat_UYVY, Packing::Uyvy),
    (MFVideoFormat_RGB24, Packing::Rgb24),
    (MFVideoFormat_RGB32, Packing::Rgb32),
    (MFVideoFormat_ARGB32, Packing::Argb32),
];

pub(super) fn subtype(g: &GUID) -> Subtype {
    PACKINGS
        .iter()
        .find(|(s, _)| s == g)
        .map_or(Subtype::Unusable, |(_, p)| Subtype::Native(*p))
}

fn pair(t: &IMFMediaType, key: &GUID) -> Option<(u32, u32)> {
    // SAFETY: a live media type.
    let v = unsafe { t.GetUINT64(key) }.ok()?;
    Some(((v >> 32) as u32, v as u32))
}

pub(super) fn native_type(index: u32, t: &IMFMediaType) -> NativeType {
    // SAFETY: a live media type.
    let sub = unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
    let (width, height) = pair(t, &MF_MT_FRAME_SIZE).unwrap_or_default();
    NativeType {
        index,
        subtype: subtype(&sub),
        width,
        height,
        rate: pair(t, &MF_MT_FRAME_RATE).unwrap_or_default(),
    }
}

/// The colorimetry a media type states; `None` when it states none.
pub(super) fn colorimetry(t: &IMFMediaType) -> Option<Colorimetry> {
    // SAFETY: a live media type.
    let matrix = unsafe { t.GetUINT32(&MF_MT_YUV_MATRIX) }.ok();
    // SAFETY: as above.
    let range = unsafe { t.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) }.ok();
    let encoding = match matrix.map(|m| m as i32) {
        Some(m) if m == MFVideoTransferMatrix_BT709.0 => Some(ColorEncoding::Bt709),
        Some(m) if m == MFVideoTransferMatrix_BT601.0 => Some(ColorEncoding::Bt601),
        _ => None,
    };
    let range = match range.map(|r| r as i32) {
        Some(r) if r == MFNominalRange_0_255.0 => Some(ColorRange::Full),
        Some(r) if r == MFNominalRange_16_235.0 => Some(ColorRange::Limited),
        _ => None,
    };
    (encoding.is_some() || range.is_some()).then(|| Colorimetry {
        encoding,
        range,
        ..Colorimetry::default()
    })
}

/// The camera's native media types, in Media Foundation's order.
pub(super) fn native_types(reader: &IMFSourceReader) -> Result<Vec<(NativeType, IMFMediaType)>> {
    let mut out = Vec::new();
    for index in 0.. {
        // SAFETY: a live reader.
        match unsafe { reader.GetNativeMediaType(STREAM, index) } {
            Ok(t) => out.push((native_type(index, &t), t)),
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
            Err(e) => return Err(error(e, "list the camera's media types")),
        }
    }
    Ok(out)
}

/// A video capture device Media Foundation lists.
#[derive(Debug)]
pub(super) struct Device {
    pub(super) activate: Agile<IMFActivate>,
    /// Identity, without formats.
    pub(super) descriptor: CameraDescriptor,
}

impl Device {
    /// Creates the device's media source.
    pub(super) fn activate(&self) -> Result<IMFMediaSource> {
        // SAFETY: a live activation object.
        unsafe { self.activate.ActivateObject() }
            .map_err(|e| error(e, &format!("open {}", self.descriptor.name)))
    }

    /// The device's formats, sizes and rates.
    pub(super) fn formats(&self) -> Result<Vec<FormatInfo>> {
        let source = self.activate()?;
        // SAFETY: a live media source and no attributes.
        let types = unsafe { MFCreateSourceReaderFromMediaSource(&source, None) }
            .map_err(|e| error(e, &format!("read {}", self.descriptor.name)))
            .and_then(|reader| native_types(&reader));
        // SAFETY: a live source and activation object, unused after this.
        unsafe {
            let _ = source.Shutdown();
            let _ = self.activate.ShutdownObject();
        }
        let types: Vec<NativeType> = types?.into_iter().map(|(t, _)| t).collect();
        Ok(select::formats(&types))
    }
}

/// The video capture devices, in Media Foundation's order.
pub(super) fn devices() -> Result<Vec<Device>> {
    let mut attributes = None;
    // SAFETY: a valid out-pointer.
    unsafe { MFCreateAttributes(&mut attributes, 1) }
        .map_err(|e| error(e, "create device attributes"))?;
    let attributes = attributes.ok_or_else(|| {
        Error::new(
            ErrorKind::Backend,
            "Media Foundation returned no attributes",
        )
    })?;
    // SAFETY: a live attribute store.
    unsafe {
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
    }
    .map_err(|e| error(e, "select video capture devices"))?;
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: valid out-pointers; the array is taken and freed below.
    unsafe { MFEnumDeviceSources(&attributes, &mut list, &mut count) }
        .map_err(|e| error(e, "list video capture devices"))?;
    if list.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: Media Foundation returned `count` activation objects, each
    // with a reference this takes over.
    let activations: Vec<IMFActivate> =
        unsafe { std::slice::from_raw_parts_mut(list, count as usize) }
            .iter_mut()
            .filter_map(Option::take)
            .collect();
    // SAFETY: the array was allocated with the COM allocator and is empty.
    unsafe { CoTaskMemFree(Some(list as *const _)) };
    Ok(activations
        .into_iter()
        .map(|a| {
            let link = string(&a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)
                .unwrap_or_default();
            let name = string(&a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME).unwrap_or_default();
            Device {
                activate: Agile(a),
                descriptor: CameraDescriptor::new(
                    Backend::MediaFoundation,
                    link,
                    name,
                    "Media Foundation",
                ),
            }
        })
        .collect())
}

/// The device `id` names: its symbolic link, compared without case, or its
/// index in [`devices`].
pub(super) fn find(id: &str) -> Result<Device> {
    let mut all = devices()?;
    let at = all
        .iter()
        .position(|d| d.descriptor.id.eq_ignore_ascii_case(id))
        .or_else(|| id.parse::<usize>().ok().filter(|&i| i < all.len()));
    match at {
        Some(i) => Ok(all.swap_remove(i)),
        None => Err(Error::new(
            ErrorKind::NotFound,
            format!(
                "no Media Foundation camera {id}; {} camera(s) are attached",
                all.len()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtypes_map_to_their_packing() {
        assert_eq!(subtype(&MFVideoFormat_NV12), Subtype::Native(Packing::Nv12));
        assert_eq!(subtype(&MFVideoFormat_IYUV), Subtype::Native(Packing::I420));
        assert_eq!(
            subtype(&windows::Win32::Media::MediaFoundation::MFVideoFormat_MJPG),
            Subtype::Unusable
        );
        for f in select::FORMATS {
            assert!(PACKINGS.iter().any(|(_, p)| p.delivers() == f), "{f:?}");
        }
    }
}

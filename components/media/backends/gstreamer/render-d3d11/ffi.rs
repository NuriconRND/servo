/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! gstd3d11-1.0-0.dll 공개 C API에 대한 동적 FFI.
//!
//! crates.io에 gstreamer-d3d11 바인딩이 없어(2026-07-09 확인) 필요한 최소 심볼만
//! libloading으로 로드한다. 시그니처는 GStreamer 1.26 msvc 헤더
//! (gst/d3d11/gstd3d11device.h, gstd3d11memory.h, gstd3d11utils.h)에서 확정.

use std::sync::OnceLock;

use winapi::um::d3d11::{ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D};

/// GObject 기반 불투명 타입 (참조만 주고받음).
#[repr(C)]
pub struct GstD3D11Device {
    _private: [u8; 0],
}

/// GstMemory를 첫 필드로 내장하는 불투명 타입. gst_is_d3d11_memory 확인 후
/// *mut GstMemory에서 캐스팅해 사용한다.
#[repr(C)]
pub struct GstD3D11Memory {
    _private: [u8; 0],
}

/// YUV→RGBA 변환 엔진 (d3d11convert 엘리먼트 내부와 동일). GstObject 파생 —
/// 해제는 g_object_unref.
#[repr(C)]
pub struct GstD3D11Converter {
    _private: [u8; 0],
}

#[repr(C)]
pub struct GstD3D11Allocator {
    _private: [u8; 0],
}

type Gboolean = i32;
// GType — glib ABI상 usize.
type GType = usize;

pub struct GstD3D11Api {
    // Library는 fn 포인터 수명 유지를 위해 보관 (drop 금지)
    _lib: libloading::Library,
    pub device_new: unsafe extern "C" fn(u32, u32) -> *mut GstD3D11Device,
    pub device_get_device_handle: unsafe extern "C" fn(*mut GstD3D11Device) -> *mut ID3D11Device,
    pub device_get_device_context_handle:
        unsafe extern "C" fn(*mut GstD3D11Device) -> *mut ID3D11DeviceContext,
    pub device_lock: unsafe extern "C" fn(*mut GstD3D11Device),
    pub device_unlock: unsafe extern "C" fn(*mut GstD3D11Device),
    pub context_new: unsafe extern "C" fn(*mut GstD3D11Device) -> *mut gstreamer::ffi::GstContext,
    pub is_d3d11_memory: unsafe extern "C" fn(*mut gstreamer::ffi::GstMemory) -> Gboolean,
    pub memory_get_resource_handle:
        unsafe extern "C" fn(*mut GstD3D11Memory) -> *mut ID3D11Resource,
    pub memory_get_subresource_index: unsafe extern "C" fn(*mut GstD3D11Memory) -> u32,
    pub converter_new: unsafe extern "C" fn(
        *mut GstD3D11Device,
        *const gstreamer_video::ffi::GstVideoInfo,
        *const gstreamer_video::ffi::GstVideoInfo,
        *mut gstreamer::ffi::GstStructure,
    ) -> *mut GstD3D11Converter,
    pub converter_convert_buffer: unsafe extern "C" fn(
        *mut GstD3D11Converter,
        *mut gstreamer::ffi::GstBuffer,
        *mut gstreamer::ffi::GstBuffer,
    ) -> Gboolean,
    pub allocator_get_type: unsafe extern "C" fn() -> GType,
    pub allocator_alloc_wrapped: unsafe extern "C" fn(
        *mut GstD3D11Allocator,
        *mut GstD3D11Device,
        *mut ID3D11Texture2D,
        usize,
        *mut std::ffi::c_void,
        gstreamer::glib::ffi::GDestroyNotify,
    ) -> *mut gstreamer::ffi::GstMemory,
}

impl GstD3D11Api {
    /// 프로세스 전역 1회 로드. 실패하면 warn 로그 후 None (호출측은 Raw 폴백).
    pub fn load() -> Option<&'static GstD3D11Api> {
        static API: OnceLock<Option<&'static GstD3D11Api>> = OnceLock::new();
        *API.get_or_init(|| match Self::load_impl() {
            Ok(api) => Some(Box::leak(Box::new(api))),
            Err(error) => {
                log::warn!("D3D11 video: gstd3d11-1.0-0.dll 로드 실패, Raw 경로 폴백: {error}");
                None
            },
        })
    }

    fn load_impl() -> Result<GstD3D11Api, libloading::Error> {
        unsafe {
            let lib = libloading::Library::new("gstd3d11-1.0-0.dll")?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get($name)?
                };
            }
            Ok(GstD3D11Api {
                device_new: sym!(b"gst_d3d11_device_new\0"),
                device_get_device_handle: sym!(b"gst_d3d11_device_get_device_handle\0"),
                device_get_device_context_handle: sym!(
                    b"gst_d3d11_device_get_device_context_handle\0"
                ),
                device_lock: sym!(b"gst_d3d11_device_lock\0"),
                device_unlock: sym!(b"gst_d3d11_device_unlock\0"),
                context_new: sym!(b"gst_d3d11_context_new\0"),
                is_d3d11_memory: sym!(b"gst_is_d3d11_memory\0"),
                memory_get_resource_handle: sym!(b"gst_d3d11_memory_get_resource_handle\0"),
                memory_get_subresource_index: sym!(b"gst_d3d11_memory_get_subresource_index\0"),
                converter_new: sym!(b"gst_d3d11_converter_new\0"),
                converter_convert_buffer: sym!(b"gst_d3d11_converter_convert_buffer\0"),
                allocator_get_type: sym!(b"gst_d3d11_allocator_get_type\0"),
                allocator_alloc_wrapped: sym!(b"gst_d3d11_allocator_alloc_wrapped\0"),
                _lib: lib,
            })
        }
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! GstD3D11Device 공유 래퍼 + 공유 텍스처 링.
//!
//! 디바이스는 어댑터당 1개를 프로세스 전역으로 공유한다(gst 권장, 스펙 §7).
//! immediate context 접근은 반드시 gst_d3d11_device_lock 하에서 수행
//! (d3d11upload/convert가 같은 디바이스를 다른 스레드에서 사용).

use std::sync::{Arc, OnceLock};

use gstreamer::glib::translate::from_glib_full;
use winapi::um::d3d11::{ID3D11Device, ID3D11DeviceContext};

use crate::ffi::{GstD3D11Allocator, GstD3D11Api, GstD3D11Device};

// D3D11_CREATE_DEVICE_BGRA_SUPPORT
const D3D11_DEVICE_FLAGS: u32 = 0x20;

pub struct SharedGstD3D11Device {
    api: &'static GstD3D11Api,
    device: *mut GstD3D11Device,
    allocator: *mut GstD3D11Allocator,
}

// 안전성: GstD3D11Device는 스레드 안전(GObject + 내부 뮤텍스). immediate context는
// lock()/DeviceLockGuard로 직렬화해서만 접근한다.
unsafe impl Send for SharedGstD3D11Device {}
unsafe impl Sync for SharedGstD3D11Device {}

pub struct DeviceLockGuard<'a> {
    device: &'a SharedGstD3D11Device,
}

impl Drop for DeviceLockGuard<'_> {
    fn drop(&mut self) {
        unsafe { (self.device.api.device_unlock)(self.device.device) }
    }
}

impl SharedGstD3D11Device {
    /// 어댑터 0의 전역 공유 디바이스. 최초 호출에서 생성, 실패 시 None 고정.
    pub fn get_or_create() -> Option<Arc<SharedGstD3D11Device>> {
        static DEVICE: OnceLock<Option<Arc<SharedGstD3D11Device>>> = OnceLock::new();
        DEVICE
            .get_or_init(|| {
                let api = GstD3D11Api::load()?;
                // 멀티GPU 후속: 어댑터 인덱스/LUID를 여기서 주입 (스펙 §4.5)
                let device = unsafe { (api.device_new)(0, D3D11_DEVICE_FLAGS) };
                if device.is_null() {
                    log::warn!("D3D11 video: gst_d3d11_device_new(adapter=0) 실패");
                    return None;
                }
                // 링 텍스처를 GstD3D11Memory로 래핑하기 위한 allocator.
                // 등록된 기본 allocator를 우선 찾고, 미등록이면 새 인스턴스 생성
                // (프로세스 수명 동안 보유 — 의도적 비해제).
                let allocator = unsafe {
                    let name = c"D3D11Memory";
                    let mut allocator = gstreamer::ffi::gst_allocator_find(name.as_ptr())
                        as *mut GstD3D11Allocator;
                    if allocator.is_null() {
                        allocator = gstreamer::glib::gobject_ffi::g_object_new(
                            (api.allocator_get_type)(),
                            std::ptr::null(),
                        ) as *mut GstD3D11Allocator;
                    }
                    allocator
                };
                if allocator.is_null() {
                    log::warn!("D3D11 video: GstD3D11Allocator 획득 실패");
                    return None;
                }
                Some(Arc::new(SharedGstD3D11Device { api, device, allocator }))
            })
            .clone()
    }

    pub fn api(&self) -> &'static GstD3D11Api {
        self.api
    }

    /// gstd3d11 FFI에 넘길 원시 디바이스 포인터.
    pub fn raw(&self) -> *mut GstD3D11Device {
        self.device
    }

    pub fn allocator(&self) -> *mut GstD3D11Allocator {
        self.allocator
    }

    pub fn d3d11_device(&self) -> *mut ID3D11Device {
        unsafe { (self.api.device_get_device_handle)(self.device) }
    }

    /// 주의: 반환 컨텍스트 사용은 반드시 lock() 가드 하에서.
    pub fn immediate_context(&self) -> *mut ID3D11DeviceContext {
        unsafe { (self.api.device_get_device_context_handle)(self.device) }
    }

    pub fn lock(&self) -> DeviceLockGuard<'_> {
        unsafe { (self.api.device_lock)(self.device) };
        DeviceLockGuard { device: self }
    }

    /// 파이프라인 주입용 GstContext ("gst.d3d11.device.handle").
    pub fn gst_context(&self) -> Option<gstreamer::Context> {
        let ptr = unsafe { (self.api.context_new)(self.device) };
        if ptr.is_null() {
            return None;
        }
        Some(unsafe { from_glib_full(ptr) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::GstD3D11Api;

    // 요구: PATH에 C:\gstreamer\1.0\msvc_x86_64\bin (gstd3d11-1.0-0.dll)
    #[test]
    fn load_api_and_create_shared_device() {
        gstreamer::init().expect("gstreamer init 실패 — PATH에 gstreamer bin 필요");
        let _api = GstD3D11Api::load().expect("gstd3d11-1.0-0.dll 로드/심볼 해석 실패");
        let device = SharedGstD3D11Device::get_or_create().expect("GstD3D11Device 생성 실패");
        assert!(!device.d3d11_device().is_null());
        assert!(!device.immediate_context().is_null());
        assert!(!device.allocator().is_null());
        let context = device.gst_context().expect("gst_d3d11_context_new 실패");
        assert_eq!(context.context_type(), "gst.d3d11.device.handle");
        // 전역 공유: 두 번째 호출은 같은 디바이스
        let device2 = SharedGstD3D11Device::get_or_create().expect("재호출 실패");
        assert_eq!(device.d3d11_device(), device2.d3d11_device());
    }
}

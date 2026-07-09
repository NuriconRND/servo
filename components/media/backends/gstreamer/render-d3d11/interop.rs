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

// 안전성:
// - `device`: GstD3D11Device는 스레드 안전(GObject + 내부 뮤텍스). immediate context는
//   lock()/DeviceLockGuard로 직렬화해서만 접근한다.
// - `allocator`: GstD3D11Allocator는 GstAllocator(GObject) 파생 — 참조계수와 alloc API가
//   스레드 안전하다. 우리는 이 포인터를 gst_d3d11_allocator_alloc_wrapped 호출 인자로
//   전달만 하고 내부 상태를 직접 건드리지 않는다.
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

// winapi 0.3.9 실제 모듈명은 `dxgifmt`가 아니라 `dxgiformat` (Step 3 컴파일러 에러로 확인,
// 브리프 주석의 폴백 경로 채택).
use winapi::shared::dxgi::IDXGIResource;
use winapi::shared::dxgiformat::DXGI_FORMAT_R8G8B8A8_UNORM;
use winapi::shared::dxgitype::DXGI_SAMPLE_DESC;
use winapi::shared::winerror::{S_FALSE, S_OK};
use winapi::um::d3d11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_QUERY_DESC,
    D3D11_QUERY_EVENT, D3D11_RESOURCE_MISC_SHARED, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    ID3D11Asynchronous, ID3D11Query, ID3D11Resource, ID3D11Texture2D,
};
use wio::com::ComPtr;

const RING_SLOTS: usize = 4;

struct RingSlot {
    texture: ComPtr<ID3D11Texture2D>,
    shared_handle: u64,
    query: ComPtr<ID3D11Query>,
    /// 링 텍스처를 GstD3D11Memory로 감싼 버퍼 — GstD3D11Converter의 출력 대상.
    /// clone은 미니오브젝트 참조 증가일 뿐이라 저렴.
    wrapped_buffer: gstreamer::Buffer,
}

/// 플레이어별 공유 텍스처 링.
///
/// 사용 순서: acquire(슬롯 확보) → GstD3D11Converter가 슬롯 버퍼에 직접 변환-렌더
/// (또는 폴백 write_from_resource의 복사) → finish(완료 fence 후 핸들 발행).
///
/// 동기화 설계(스펙 §4.4의 keyed mutex를 완료-대기+발행으로 대체, 계획 문서 "의도적
/// 차이 3" 참조): finish()는 GPU 작업 완료를 D3D11_QUERY_EVENT로 확인한 뒤에만 핸들을
/// 반환하므로, 소비자(렌더러)는 발행된 슬롯을 동기화 없이 읽어도 완료된 내용을 본다.
/// 4슬롯 라운드로빈이라 발행된 최신 슬롯이 다시 써지려면 3프레임(30fps 기준 100ms)의
/// 마진이 있다 — 렌더러 lock 유지 시간(정상 <20ms, in-flight 게이트로 백로그 없음)
/// 대비 충분. 문제가 관측되면 MISC_SHARED_KEYEDMUTEX + 매 lock import/destroy로 폴백.
pub struct SharedTextureRing {
    device: Arc<SharedGstD3D11Device>,
    slots: Vec<RingSlot>,
    next_slot: usize,
    epoch: u32,
    width: i32,
    height: i32,
}

// 안전성: ComPtr 원시 포인터는 이 구조체가 단독 소유하며, 컨텍스트 작업은 전부
// 디바이스 락 하에서 수행된다. 스트리밍 스레드 1개에서만 write()가 불린다.
unsafe impl Send for SharedTextureRing {}

impl SharedTextureRing {
    pub fn new(device: Arc<SharedGstD3D11Device>) -> Self {
        SharedTextureRing {
            device,
            slots: Vec::new(),
            next_slot: 0,
            epoch: 0,
            width: 0,
            height: 0,
        }
    }

    /// 다음 슬롯 확보. 반환: (변환 출력 대상으로 쓸 래핑 GstBuffer, 슬롯 인덱스).
    pub fn acquire(&mut self, width: i32, height: i32) -> Option<(gstreamer::Buffer, usize)> {
        if width <= 0 || height <= 0 {
            return None;
        }
        if self.slots.is_empty() || width != self.width || height != self.height {
            self.recreate(width, height)?;
        }
        let slot_index = self.next_slot;
        self.next_slot = (self.next_slot + 1) % self.slots.len();
        Some((self.slots[slot_index].wrapped_buffer.clone(), slot_index))
    }

    /// 슬롯에 대한 GPU 작업 제출 후 호출: 완료 fence 대기 → (공유 핸들, epoch) 발행.
    pub fn finish(&mut self, slot_index: usize) -> Option<(u64, u32)> {
        let slot = self.slots.get(slot_index)?;
        unsafe {
            let _guard = self.device.lock();
            let context = self.device.immediate_context();
            (*context).End(slot.query.as_raw() as *mut ID3D11Asynchronous);
            (*context).Flush();
        }
        // GPU 완료 대기 — 폴마다 락을 짧게 잡아 다른 파이프라인을 막지 않는다.
        loop {
            let hr = unsafe {
                let _guard = self.device.lock();
                let context = self.device.immediate_context();
                (*context).GetData(
                    slot.query.as_raw() as *mut ID3D11Asynchronous,
                    std::ptr::null_mut(),
                    0,
                    0,
                )
            };
            match hr {
                S_OK => break,
                S_FALSE => std::thread::yield_now(),
                _ => {
                    log::warn!("D3D11 video: 완료 쿼리 실패 hr={hr:#x}");
                    return None;
                },
            }
        }
        Some((slot.shared_handle, self.epoch))
    }

    /// 테스트·폴백용: 원본 D3D11 텍스처를 슬롯에 복사(변환 없음, 동일 포맷 전제).
    pub fn write_from_resource(
        &mut self,
        src: *mut ID3D11Resource,
        subresource: u32,
        width: i32,
        height: i32,
    ) -> Option<(u64, u32)> {
        if src.is_null() {
            return None;
        }
        let (_wrapped_buffer, slot_index) = self.acquire(width, height)?;
        let src_box = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width as u32,
            bottom: height as u32,
            back: 1,
        };
        unsafe {
            let _guard = self.device.lock();
            let context = self.device.immediate_context();
            (*context).CopySubresourceRegion(
                self.slots[slot_index].texture.as_raw() as *mut ID3D11Resource,
                0,
                0,
                0,
                0,
                src,
                subresource,
                &src_box,
            );
        }
        self.finish(slot_index)
    }

    fn recreate(&mut self, width: i32, height: i32) -> Option<()> {
        self.slots.clear();
        self.next_slot = 0;
        self.epoch = self.epoch.wrapping_add(1);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width as u32,
            Height: height as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            // ANGLE pbuffer 래핑(EGL_ANGLE_d3d_texture_client_buffer) 요건 충족을 위해
            // RENDER_TARGET 포함.
            BindFlags: D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED,
        };
        let query_desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        let device = self.device.d3d11_device();
        for _ in 0..RING_SLOTS {
            unsafe {
                let mut texture = std::ptr::null_mut();
                let hr = (*device).CreateTexture2D(&desc, std::ptr::null(), &mut texture);
                if hr != S_OK {
                    log::warn!("D3D11 video: 링 텍스처 생성 실패 hr={hr:#x}");
                    self.slots.clear();
                    return None;
                }
                let texture = ComPtr::from_raw(texture);
                let dxgi: ComPtr<IDXGIResource> = match texture.cast() {
                    Ok(dxgi) => dxgi,
                    Err(hr) => {
                        log::warn!("D3D11 video: IDXGIResource 캐스트 실패 hr={hr:#x}");
                        self.slots.clear();
                        return None;
                    },
                };
                let mut handle = std::ptr::null_mut();
                let hr = dxgi.GetSharedHandle(&mut handle);
                if hr != S_OK || handle.is_null() {
                    log::warn!("D3D11 video: GetSharedHandle 실패 hr={hr:#x}");
                    self.slots.clear();
                    return None;
                }
                let mut query = std::ptr::null_mut();
                let hr = (*device).CreateQuery(&query_desc, &mut query);
                if hr != S_OK {
                    log::warn!("D3D11 video: 이벤트 쿼리 생성 실패 hr={hr:#x}");
                    self.slots.clear();
                    return None;
                }
                // 슬롯 텍스처를 GstD3D11Memory로 래핑해 변환기 출력 버퍼로 준비.
                let api = self.device.api();
                let memory_ptr = (api.allocator_alloc_wrapped)(
                    self.device.allocator(),
                    self.device.raw(),
                    texture.as_raw(),
                    (width as usize) * (height as usize) * 4,
                    std::ptr::null_mut(),
                    None,
                );
                if memory_ptr.is_null() {
                    log::warn!("D3D11 video: alloc_wrapped 실패");
                    self.slots.clear();
                    return None;
                }
                let memory: gstreamer::Memory = from_glib_full(memory_ptr);
                let mut wrapped_buffer = gstreamer::Buffer::new();
                wrapped_buffer
                    .get_mut()
                    .expect("새 버퍼는 유일 참조")
                    .append_memory(memory);
                self.slots.push(RingSlot {
                    texture,
                    shared_handle: handle as usize as u64,
                    query: ComPtr::from_raw(query),
                    wrapped_buffer,
                });
            }
        }
        self.width = width;
        self.height = height;
        Some(())
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

    // 아래 dxgiformat 경로: winapi 0.3.9 실제 모듈명은 `dxgifmt`가 아니라 `dxgiformat`
    // (브리프 주석대로 컴파일러 에러에 맞춰 조정).
    use winapi::Interface;
    use winapi::shared::dxgiformat::DXGI_FORMAT_R8G8B8A8_UNORM;
    use winapi::shared::dxgitype::DXGI_SAMPLE_DESC;
    use winapi::um::d3d11 as d3d;
    use winapi::um::d3dcommon::D3D_DRIVER_TYPE_HARDWARE;
    use wio::com::ComPtr;

    fn create_test_source_texture(
        device: &SharedGstD3D11Device,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> ComPtr<d3d::ID3D11Texture2D> {
        let desc = d3d::D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: d3d::D3D11_USAGE_DEFAULT,
            BindFlags: d3d::D3D11_BIND_SHADER_RESOURCE,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = d3d::D3D11_SUBRESOURCE_DATA {
            pSysMem: rgba.as_ptr() as *const _,
            SysMemPitch: width * 4,
            SysMemSlicePitch: 0,
        };
        unsafe {
            let mut texture = std::ptr::null_mut();
            let hr = (*device.d3d11_device()).CreateTexture2D(&desc, &init, &mut texture);
            assert_eq!(hr, 0, "소스 텍스처 생성 실패 hr={hr:#x}");
            ComPtr::from_raw(texture)
        }
    }

    fn open_and_read_on_second_device(handle: u64, width: u32, height: u32) -> Vec<u8> {
        unsafe {
            let mut device = std::ptr::null_mut();
            let mut context = std::ptr::null_mut();
            let hr = d3d::D3D11CreateDevice(
                std::ptr::null_mut(),
                D3D_DRIVER_TYPE_HARDWARE,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
                0,
                d3d::D3D11_SDK_VERSION,
                &mut device,
                std::ptr::null_mut(),
                &mut context,
            );
            assert_eq!(hr, 0, "두 번째 디바이스 생성 실패 hr={hr:#x}");
            let device = ComPtr::from_raw(device);
            let context = ComPtr::from_raw(context);

            let mut opened: *mut winapi::ctypes::c_void = std::ptr::null_mut();
            let hr = device.OpenSharedResource(
                handle as winapi::shared::ntdef::HANDLE,
                &d3d::ID3D11Texture2D::uuidof(),
                &mut opened,
            );
            assert_eq!(hr, 0, "OpenSharedResource 실패 hr={hr:#x}");
            let opened = ComPtr::from_raw(opened as *mut d3d::ID3D11Texture2D);

            let desc = d3d::D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: d3d::D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: d3d::D3D11_CPU_ACCESS_READ,
                MiscFlags: 0,
            };
            let mut staging = std::ptr::null_mut();
            let hr = device.CreateTexture2D(&desc, std::ptr::null(), &mut staging);
            assert_eq!(hr, 0, "staging 생성 실패 hr={hr:#x}");
            let staging = ComPtr::from_raw(staging);

            context.CopyResource(staging.as_raw() as *mut _, opened.as_raw() as *mut _);
            let mut mapped = std::mem::zeroed::<d3d::D3D11_MAPPED_SUBRESOURCE>();
            let hr = context.Map(staging.as_raw() as *mut _, 0, d3d::D3D11_MAP_READ, 0, &mut mapped);
            assert_eq!(hr, 0, "Map 실패 hr={hr:#x}");
            let mut out = vec![0u8; (width * height * 4) as usize];
            for row in 0..height as usize {
                let src_row = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
                let dst = &mut out[row * width as usize * 4..(row + 1) * width as usize * 4];
                std::ptr::copy_nonoverlapping(src_row, dst.as_mut_ptr(), width as usize * 4);
            }
            context.Unmap(staging.as_raw() as *mut _, 0);
            out
        }
    }

    // 같은 디바이스에 단색 소스 텍스처 생성 → ring.write → 두 번째(별개) D3D11
    // 디바이스에서 공유 핸들 open → staging 판독 → 픽셀 일치 확인.
    #[test]
    fn ring_write_and_cross_device_readback() {
        gstreamer::init().expect("gstreamer init 실패");
        let device = SharedGstD3D11Device::get_or_create().expect("디바이스 없음");

        const W: usize = 64;
        const H: usize = 64;
        // RGBA (R=0x11, G=0x22, B=0x33, A=0xFF)
        let pixels: Vec<u8> = std::iter::repeat([0x11u8, 0x22, 0x33, 0xFF])
            .take(W * H)
            .flatten()
            .collect();
        let src = create_test_source_texture(&device, W as u32, H as u32, &pixels);

        let mut ring = SharedTextureRing::new(device.clone());
        let (handle, epoch) = ring
            .write_from_resource(src.as_raw() as *mut _, 0, W as i32, H as i32)
            .expect("ring write 실패");
        assert_eq!(epoch, 1);
        assert_ne!(handle, 0);

        let readback = open_and_read_on_second_device(handle, W as u32, H as u32);
        assert_eq!(&readback[0..4], &[0x11, 0x22, 0x33, 0xFF], "첫 픽셀 불일치");
        let mid = (H / 2 * W + W / 2) * 4;
        assert_eq!(&readback[mid..mid + 4], &[0x11, 0x22, 0x33, 0xFF], "중앙 픽셀 불일치");

        // 크기 변경 → epoch 증가
        let src2 = create_test_source_texture(&device, 32, 32, &pixels[..32 * 32 * 4]);
        let (_h2, epoch2) = ring
            .write_from_resource(src2.as_raw() as *mut _, 0, 32, 32)
            .expect("ring write(2) 실패");
        assert_eq!(epoch2, 2);

        // 변환기 출력 대상용 래핑 버퍼가 슬롯마다 존재
        let (wrapped, _slot) = ring.acquire(32, 32).expect("acquire 실패");
        assert_eq!(wrapped.n_memory(), 1);
    }
}

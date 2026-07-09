/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! GstD3D11Device 래퍼 + 공유 텍스처 링.
//!
//! 디바이스는 파이프라인(플레이어)별로 1개씩 생성한다 — 프로세스 전역 공유 디바이스는
//! 45+ 파이프라인 동기그룹 lockstep의 루프 경계 버스트에서 단일 GRecMutex+커맨드 큐가
//! 포화되어 락 콘보이(주기 스톨 + lockstep 붕괴)를 일으킴이 실측으로 확정됨
//! (2026-07-10 조사, `.superpowers/sdd/investigation-loop-stall-report.md`). 렌더러(ANGLE)
//! 와는 공유 텍스처 핸들로만 교차하므로 디바이스 자체를 나누는 것은 크로스 디바이스
//! 안전성에 영향 없음(레거시 공유 핸들은 동일 어댑터의 다른 디바이스에서
//! OpenSharedResource 가능 — PoC로 검증됨).
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
    /// 어댑터 0의 신규 디바이스 생성 (비캐시 — 호출마다 새 GstD3D11Device).
    /// 파이프라인(플레이어)별로 이 함수를 호출해 전용 디바이스를 갖는다 — 근거는
    /// 위 모듈 doc comment 참조.
    pub fn create() -> Option<Arc<SharedGstD3D11Device>> {
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
            let mut allocator =
                gstreamer::ffi::gst_allocator_find(name.as_ptr()) as *mut GstD3D11Allocator;
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
    }

    /// 어댑터 0의 전역 공유 디바이스. 최초 호출에서 생성, 실패 시 None 고정.
    /// 기존 유닛테스트·PoC 예제 호환용으로 유지 — 파이프라인 생산 경로는
    /// [`Self::create`]를 직접 호출한다(위 모듈 doc comment 참조).
    pub fn get_or_create() -> Option<Arc<SharedGstD3D11Device>> {
        static DEVICE: OnceLock<Option<Arc<SharedGstD3D11Device>>> = OnceLock::new();
        DEVICE.get_or_init(Self::create).clone()
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

impl Drop for SharedGstD3D11Device {
    fn drop(&mut self) {
        // gst_d3d11_device_new와 gst_allocator_find/g_object_new는 모두 소유 참조를
        // 반환한다(transfer full). 파이프라인별 디바이스는 플레이어 해체 시 해제.
        // (전역 캐시본은 OnceLock의 Arc가 프로세스 종료까지 유지 — 사실상 불호출.)
        // 해제 순서: allocator 먼저, device 나중.
        // RenderD3D11.device와 PlayerState.ring.device는 같은 SharedGstD3D11Device를
        // 가리키는 별개의 Arc 클론이지만, Arc 참조계수 덕분에 이 Drop::drop 본체는
        // 마지막 강한 참조가 사라질 때 정확히 1번만 실행된다 — 두 소유자 중 어느 쪽이
        // 먼저/나중에 해체되든 순서 무관하게 안전.
        unsafe {
            gstreamer::glib::gobject_ffi::g_object_unref(self.allocator as *mut _);
            gstreamer::glib::gobject_ffi::g_object_unref(self.device as *mut _);
        }
    }
}

// winapi 0.3.9 실제 모듈명은 `dxgifmt`가 아니라 `dxgiformat` (Step 3 컴파일러 에러로 확인,
// 브리프 주석의 폴백 경로 채택).
use winapi::shared::dxgi::IDXGIResource;
use winapi::shared::dxgiformat::DXGI_FORMAT_R8G8B8A8_UNORM;
use winapi::shared::dxgitype::DXGI_SAMPLE_DESC;
use winapi::shared::winerror::{S_FALSE, S_OK};
use winapi::um::d3d11::{
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BOX, D3D11_QUERY_DESC, D3D11_QUERY_EVENT, D3D11_RESOURCE_MISC_SHARED,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Asynchronous, ID3D11Query, ID3D11Resource,
    ID3D11Texture2D,
};
use wio::com::ComPtr;

const RING_SLOTS: usize = 4;

/// finish()의 GPU 완료 폴링 예산. GPU 행/TDR/디바이스 제거 시 스트리밍 스레드가
/// 영원히 스핀하지 않도록 초과 시 슬롯 발행을 포기한다(해당 프레임만 드롭).
const FINISH_POLL_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

// ============================================================================
// D3D11PROF: 임시 진단 계측 (loop-boundary stall 조사, env SERVO_D3D11_PROFILE=1
// 게이트, 기본 off). 조사 종료 후 제거 예정. off일 때 오버헤드는 build_frame당
// 정수 증가 + Instant::now 수회로 무시 가능; 폴 루프의 lock-wait 타이밍은 prof에서만
// 켜져 스핀 루프 타이밍을 교란하지 않는다.
// ============================================================================

/// SERVO_D3D11_PROFILE=1 이면 단계별 타이밍을 측정/로깅한다 (프로세스 1회 판정).
pub fn profile_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("SERVO_D3D11_PROFILE")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on"))
    })
}

/// build_frame 총시간이 이 임계(ms) 이상일 때만 로깅 (로그 폭주 방지).
/// SERVO_D3D11_PROFILE_MS 로 조정, 기본 8ms.
pub fn profile_threshold_ms() -> f64 {
    static T: OnceLock<f64> = OnceLock::new();
    *T.get_or_init(|| {
        std::env::var("SERVO_D3D11_PROFILE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8.0)
    })
}

/// finish() 단계별 계측 결과. profile on일 때만 채워지며 build_frame이 판독해 로깅.
/// - endflush_lock_wait: End+Flush 직전 디바이스 락 획득 대기 (H1 콘보이 신호)
/// - poll_lock_wait: fence 폴 루프에서 디바이스 락 획득 대기 누계 (H1/H2 신호)
/// - fence_loop: fence 폴 루프 전체 소요 (H3 GPU 큐 신호: 폴은 값싼데 완료가 늦음)
/// - poll_count: GetData 폴 횟수 (스핀 폭풍 규모)
#[derive(Clone, Copy, Default)]
pub struct FinishStats {
    pub endflush_lock_wait: std::time::Duration,
    pub poll_lock_wait: std::time::Duration,
    pub fence_loop: std::time::Duration,
    pub poll_count: u32,
}

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
    /// 직전 acquire()가 내준 (슬롯, epoch). finish()는 이와 일치할 때만 발행 —
    /// acquire와 finish 사이에 recreate가 끼거나(epoch 변경으로 슬롯·쿼리가 전부
    /// 교체됨) acquire 없이 finish가 불리면 엉뚱한 슬롯의 쿼리를 대기/발행하게
    /// 되므로 None으로 거절한다.
    pending: Option<(usize, u32)>,
    /// D3D11PROF: 파이프라인 식별자 (lib.rs가 매 build_frame에서 설정, 기본 0).
    pub profile_id: u32,
    /// D3D11PROF: 직전 finish()의 단계별 계측 (profile on일 때만 갱신).
    pub last_stats: FinishStats,
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
            pending: None,
            profile_id: 0,
            last_stats: FinishStats::default(),
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
        self.pending = Some((slot_index, self.epoch));
        Some((self.slots[slot_index].wrapped_buffer.clone(), slot_index))
    }

    /// 슬롯에 대한 GPU 작업 제출 후 호출: 완료 fence 대기 → (공유 핸들, epoch) 발행.
    ///
    /// 직전 acquire()가 내준 슬롯(같은 epoch)에 대해서만 유효 — 사이에 recreate가
    /// 끼었거나 acquire 없이 불리면 None.
    pub fn finish(&mut self, slot_index: usize) -> Option<(u64, u32)> {
        let pending = self.pending.take();
        if pending != Some((slot_index, self.epoch)) {
            log::warn!(
                "D3D11 video: finish(slot={slot_index}) — 직전 acquire와 불일치 \
                 (pending={pending:?}, epoch={}), 발행 거부",
                self.epoch
            );
            return None;
        }
        let slot = self.slots.get(slot_index)?;
        let prof = profile_enabled(); // D3D11PROF
        // D3D11PROF: End+Flush 직전 디바이스 락 획득 대기 측정 (콘보이 신호 H1).
        let endflush_lock_wait;
        unsafe {
            let lock_t = std::time::Instant::now();
            let _guard = self.device.lock();
            endflush_lock_wait = lock_t.elapsed();
            let context = self.device.immediate_context();
            (*context).End(slot.query.as_raw() as *mut ID3D11Asynchronous);
            (*context).Flush();
        }
        // GPU 완료 대기 — 폴마다 락을 짧게 잡아 다른 파이프라인을 막지 않는다.
        // 위에서 명시적으로 Flush했으므로 DONOTFLUSH로 폴링마다의 암묵적 flush를 막고,
        // GPU 행/TDR 대비 예산 초과 시 포기한다(무한 스핀 방지).
        let poll_start = std::time::Instant::now();
        let mut poll_count: u32 = 0; // D3D11PROF
        let mut poll_lock_wait = std::time::Duration::ZERO; // D3D11PROF
        loop {
            let hr = unsafe {
                // D3D11PROF: 락 획득 대기는 prof에서만 측정(스핀 루프 교란 방지).
                let lock_t = if prof { Some(std::time::Instant::now()) } else { None };
                let _guard = self.device.lock();
                if let Some(lock_t) = lock_t {
                    poll_lock_wait += lock_t.elapsed();
                }
                let context = self.device.immediate_context();
                (*context).GetData(
                    slot.query.as_raw() as *mut ID3D11Asynchronous,
                    std::ptr::null_mut(),
                    0,
                    D3D11_ASYNC_GETDATA_DONOTFLUSH,
                )
            };
            poll_count += 1; // D3D11PROF
            match hr {
                S_OK => break,
                S_FALSE => {
                    if poll_start.elapsed() >= FINISH_POLL_BUDGET {
                        let removed_reason =
                            unsafe { (*self.device.d3d11_device()).GetDeviceRemovedReason() };
                        log::warn!(
                            "D3D11 video: 완료 폴링 타임아웃 ({:?} 경과, hr={hr:#x}, \
                             DeviceRemovedReason={removed_reason:#x}), 슬롯 발행 포기",
                            poll_start.elapsed()
                        );
                        return None;
                    }
                    // 정착 상태는 수십 폴 내 완료(µs 단위) — 처음엔 yield로 빠르게,
                    // 이후엔 sleep으로 물러나 경계 버스트에서 N파이프라인 스핀 폭풍을
                    // 방지한다. poll_count는 위(D3D11PROF)에서 루프당 1회 이미 증가하므로
                    // 여기서 재증가하지 않고 그 값을 그대로 재사용 — 계측 카운트 의미를
                    // 바꾸지 않는다(SERVO_D3D11_PROFILE 재측정 전제, 건드리지 말 것 지시).
                    if poll_count < 64 {
                        std::thread::yield_now();
                    } else {
                        std::thread::sleep(std::time::Duration::from_micros(200));
                    }
                },
                _ => {
                    log::warn!("D3D11 video: 완료 쿼리 실패 hr={hr:#x}");
                    return None;
                },
            }
        }
        let shared_handle = slot.shared_handle; // copy out → slot 차용 종료
        // D3D11PROF: 단계별 계측 저장 (build_frame이 판독해 임계 초과 시 로깅).
        if prof {
            self.last_stats = FinishStats {
                endflush_lock_wait,
                poll_lock_wait,
                fence_loop: poll_start.elapsed(),
                poll_count,
            };
        }
        Some((shared_handle, self.epoch))
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
        self.pending = None;
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

    // finish()는 직전 acquire가 내준 (슬롯, epoch)에 대해서만 발행해야 한다 —
    // acquire 없이 부르거나, acquire와 finish 사이에 recreate(크기 변경)가 끼면 거부.
    #[test]
    fn finish_requires_matching_acquire() {
        gstreamer::init().expect("gstreamer init 실패");
        let device = SharedGstD3D11Device::get_or_create().expect("디바이스 없음");
        let mut ring = SharedTextureRing::new(device.clone());

        // acquire 없이 finish → None
        assert!(ring.finish(0).is_none(), "acquire 없는 finish는 거부돼야 함");

        // slot 0, 1 순서로 확보 (pending은 마지막 acquire인 slot 1)
        let (_b0, s0) = ring.acquire(64, 64).expect("acquire 실패");
        let (_b1, s1) = ring.acquire(64, 64).expect("acquire(2) 실패");
        assert_ne!(s0, s1);

        // 크기 변경 acquire → recreate(슬롯 전부 교체 + epoch 증가, next_slot 리셋)
        let (_b2, s2) = ring.acquire(32, 32).expect("acquire(3) 실패");
        assert_ne!(s1, s2);

        // recreate 이전에 확보한 슬롯으로 finish → 불일치 → None
        assert!(
            ring.finish(s1).is_none(),
            "recreate 이전 슬롯의 finish는 거부돼야 함"
        );

        // 정상 경로는 여전히 동작: 새로 acquire한 슬롯은 finish 가능
        let (_b3, s3) = ring.acquire(32, 32).expect("acquire(4) 실패");
        assert!(ring.finish(s3).is_some(), "정상 acquire→finish는 성공해야 함");
    }
}

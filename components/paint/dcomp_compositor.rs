/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! WR Native Compositor의 DirectComposition 구현 (스펙 2026-07-13).
//! 창(painter)당 인스턴스 1개. WR이 picture cache 타일을 이 모듈이 만든
//! DComp 가상 서피스에 직접 그리고, DWM이 화면에 합성한다(②단 draw 소멸).
//!
//! COM 호출 시퀀스·winapi 오버로드 이름(`_1` 접미사)·HRESULT 처리는 실기에서
//! 4/4 PASS한 PoC(`components/shared/paint/examples/dcomp_native_poc.rs`)를 정본으로
//! 이식했다. 좌표 규약: `DrawTarget::NativeSurface`는 WR 내부에서 무조건 top-left이므로
//! 이 모듈에는 y-flip이 없다(PoC G4가 표시 측 정합을 증명).
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ptr;
use std::rc::Rc;

use euclid::default::Size2D as UntypedSize2D;
use log::warn;
use paint_api::rendering_context::RenderingContext;
use webrender::api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};
use webrender::api::{ColorF, ExternalImageId, ImageRendering};
use webrender::{
    ClipRadius, Compositor, CompositorCapabilities, CompositorSurfaceTransform, Device,
    NativeSurfaceId, NativeSurfaceInfo, NativeTileId, WindowVisibility,
};
use winapi::Interface;
use winapi::shared::dxgi::{DXGI_SWAP_EFFECT_FLIP_DISCARD, IDXGIAdapter, IDXGIDevice};
use winapi::shared::dxgi1_2::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, IDXGIFactory2, IDXGISwapChain1,
};
use winapi::shared::dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM;
use winapi::shared::dxgitype::{DXGI_SAMPLE_DESC, DXGI_USAGE_RENDER_TARGET_OUTPUT};
use winapi::shared::minwindef::TRUE;
use winapi::shared::windef::{HWND, POINT, RECT};
use winapi::um::d2dbasetypes::D2D_RECT_F;
use winapi::um::d3d11::{D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11Texture2D};
use winapi::um::dcomp::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget,
    IDCompositionVirtualSurface, IDCompositionVisual,
};
use winapi::um::unknwnbase::IUnknown;

/// Gecko DCLayerTree 관례와 동일한 가상 표면 크기. WR은 타일 그리드를 이
/// 가상공간 중심(vss/2) 부근에 배치한다(picture.rs:2477).
const VIRTUAL_SURFACE_SIZE: i32 = 1024 * 32;

/// Temporary coordinate diagnostics (env `SERVO_DCOMP_DEBUG`). Task 5 smoke debugging.
/// Cached behind a `OnceLock` — this is read from `bind`/`add_surface` per tile per frame
/// (45-tile wall = thousands of calls/sec), and repeated `std::env::var` there would pollute
/// the Task 6 performance gate. The diagnostic itself stays available for Task 6.
fn dcomp_debug() -> bool {
    static DCOMP_DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DCOMP_DEBUG.get_or_init(|| std::env::var("SERVO_DCOMP_DEBUG").is_ok())
}

/// 타일 (x,y) 그리드 좌표 → 가상 서피스 내 픽셀 rect.
/// virtual_offset은 create_surface 때 WR이 준 이 서피스의 가상공간 원점.
fn tile_virtual_rect(
    virtual_offset: DeviceIntPoint,
    tile_size: DeviceIntSize,
    tile_x: i32,
    tile_y: i32,
) -> DeviceIntRect {
    let origin = DeviceIntPoint::new(
        virtual_offset.x + tile_x * tile_size.width,
        virtual_offset.y + tile_y * tile_size.height,
    );
    DeviceIntRect::from_origin_and_size(origin, tile_size)
}

/// SERVO_COMPOSITOR_DCOMP 값의 세부 모드. "surface"=가상 서피스 전용(구 경로 A/B),
/// 그 외 truthy=하이브리드(전면 갱신 서피스를 스왑체인으로 승격).
#[allow(dead_code)] // Task 3에서 결선 시 제거(하이브리드 승격 분기가 이 값을 사용).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StorageMode {
    Hybrid,
    SurfaceOnly,
}

#[allow(dead_code)] // Task 3에서 결선 시 제거(하이브리드 승격 분기가 이 값을 사용).
fn storage_mode() -> StorageMode {
    static MODE: std::sync::OnceLock<StorageMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        match std::env::var("SERVO_COMPOSITOR_DCOMP") {
            Ok(v) if v.eq_ignore_ascii_case("surface") => StorageMode::SurfaceOnly,
            _ => StorageMode::Hybrid,
        }
    })
}

/// 진단: 컬링만 끄는 스위치(요소 소실 의심 시 즉시 판별용).
#[allow(dead_code)] // Task 4에서 결선 시 제거(컬링 배선이 이 스위치를 참조).
fn cull_disabled() -> bool {
    static NO_CULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NO_CULL.get_or_init(|| std::env::var("SERVO_DCOMP_NO_CULL").is_ok())
}

/// 스왑체인 백버퍼의 유효 피복(타일 단위, Present까지 누적).
/// Present를 안 하면 flip이라도 GetBuffer(0)가 같은 버퍼이므로 누적이 성립한다.
/// 판정은 보수적: bind의 dirty가 그 타일의 valid_rect 전체를 덮을 때만 집계
/// (부분 dirty 타일은 미피복 취급 — 잔상 불허, 지연 허용. 스펙 §7 정제).
#[allow(dead_code)] // Task 3에서 결선 시 제거(하이브리드 Present 규칙의 근거).
#[derive(Default)]
struct FrameCoverage {
    covered_tiles: std::collections::HashSet<(i32, i32)>,
}

#[allow(dead_code)] // Task 3에서 결선 시 제거(하이브리드 Present 규칙의 근거).
impl FrameCoverage {
    fn reset(&mut self) {
        self.covered_tiles.clear();
    }
    fn note_tile(&mut self, tile: (i32, i32), dirty: DeviceIntRect, valid: DeviceIntRect) {
        if dirty.contains_box(&valid) {
            self.covered_tiles.insert(tile);
        }
    }
    fn is_full(&self, tiles: &std::collections::HashSet<(i32, i32)>) -> bool {
        !tiles.is_empty() && tiles.iter().all(|t| self.covered_tiles.contains(t))
    }
}

/// 서피스의 타일 집합 → 가상공간 extent(스왑체인 크기·anchor의 근거).
#[allow(dead_code)] // Task 3에서 결선 시 제거(스왑체인 크기·anchor 산출에 사용).
fn surface_extent(
    tiles: &std::collections::HashSet<(i32, i32)>,
    virtual_offset: DeviceIntPoint,
    tile_size: DeviceIntSize,
) -> Option<DeviceIntRect> {
    let mut it = tiles.iter();
    let first = *it.next()?;
    let mut rect = tile_virtual_rect(virtual_offset, tile_size, first.0, first.1);
    for &(x, y) in it {
        rect = rect.union(&tile_virtual_rect(virtual_offset, tile_size, x, y));
    }
    Some(rect)
}

/// 레이어 컬링: entries는 add_surface 순서(z 아래→위)의 (device 클립, 불투명 여부).
/// 최상위부터 훑어, 불투명 클립이 완전히 포함하는 하위 항목을 숨긴다.
/// 알파(불투명 아님) 항목은 절대 숨겨지지 않는 것이 아니라 — 알파 항목도 "위의
/// 불투명"에 완전히 덮이면 안 보이므로 숨김 대상이 될 수 있다. 숨기는 주체가
/// 불투명이어야 한다는 것이 안전 조건이다.
#[allow(dead_code)] // Task 4에서 결선 시 제거(AddVisual 이연 컬링에 사용).
fn cull_covered(entries: &[(DeviceIntRect, bool)]) -> Vec<bool> {
    let mut visible = vec![true; entries.len()];
    for top in (0..entries.len()).rev() {
        if !entries[top].1 || !visible[top] {
            continue;
        }
        for below in 0..top {
            if visible[below] && entries[top].0.contains_box(&entries[below].0) {
                visible[below] = false;
            }
        }
    }
    visible
}

/// 소유한 COM 포인터의 RAII 래퍼(Drop에서 Release). Send/Sync 아님 —
/// 렌더러 스레드 전용(WR Compositor 계약과 일치).
struct ComOwned<T>(ptr::NonNull<T>);

impl<T> ComOwned<T> {
    /// Safety: `ptr`은 소유권이 이전되는 유효한 COM 인터페이스 포인터여야 한다.
    unsafe fn from_raw(raw: *mut T) -> Option<Self> {
        ptr::NonNull::new(raw).map(Self)
    }

    fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }
}

impl<T> Drop for ComOwned<T> {
    fn drop(&mut self) {
        // Safety: from_raw 계약에 의해 유효한 COM 포인터를 소유 중.
        unsafe {
            (*(self.0.as_ptr() as *mut IUnknown)).Release();
        }
    }
}

/// BeginDraw로 얻은 스왑체인 백버퍼(GetBuffer가 AddRef해 돌려준 것). 파기 시 Release.
/// Task 3에서 결선(bind가 스왑체인 백버퍼를 pbuffer로 감쌀 때 채운다) — 그 전까지 dead_code.
#[allow(dead_code)] // Task 3에서 결선 시 제거.
struct FramePbuffer {
    pbuffer: usize,
    /// GetBuffer가 AddRef해 돌려준 백버퍼 텍스처. 파기 시 Release.
    texture: *mut ID3D11Texture2D,
}

/// 하이브리드 승격된 서피스의 스왑체인 저장소. Task 3에서 결선 — 그 전까지 dead_code.
#[allow(dead_code)] // Task 3에서 결선 시 제거.
struct SwapChainStorage {
    swapchain: ComOwned<IDXGISwapChain1>,
    /// 가상공간에서 백버퍼 (0,0)이 대응하는 지점(= 승격 시점 extent.min).
    anchor: DeviceIntPoint,
    size: DeviceIntSize,
    coverage: FrameCoverage,
    frame_pbuffer: Option<FramePbuffer>,
    drawn_this_frame: bool,
    /// 첫 Present 후 visual.SetContent(swapchain)를 완료했는가.
    /// false인 동안 visual은 fallback_virtual(마지막 완전 화면)을 계속 표시한다.
    content_attached: bool,
    withheld_frames: u32,
    /// content_attached 전까지 유지되는 구 가상 서피스(글리치 없는 전환용).
    fallback_virtual: Option<ComOwned<IDCompositionVirtualSurface>>,
}

/// 서피스 콘텐츠의 백엔드. 지금은 항상 `Virtual`(동작 불변 리팩터) — Task 3이
/// 전면 갱신 서피스를 `SwapChain`으로 승격하는 분기를 결선한다.
#[allow(dead_code)] // SwapChain variant: Task 3에서 결선 시 제거.
enum SurfaceStorage {
    Virtual {
        virtual_surface: ComOwned<IDCompositionVirtualSurface>,
    },
    SwapChain(SwapChainStorage),
}

/// 창당 하나의 picture cache 슬라이스에 대응하는 DComp 서피스 저장소 + 비주얼.
struct SurfaceEntry {
    storage: SurfaceStorage,
    visual: ComOwned<IDCompositionVisual>,
    virtual_offset: DeviceIntPoint,
    tile_size: DeviceIntSize,
    #[allow(dead_code)]
    is_opaque: bool,
    /// bind/unbind된 타일 좌표 부기(Task 3의 승격 판단·surface_extent 근거).
    tiles: std::collections::HashSet<(i32, i32)>,
    /// 연속으로 승격 조건을 만족한 프레임 수(Task 3의 히스테리시스 근거).
    #[allow(dead_code)] // Task 3에서 결선 시 제거.
    promote_streak: u32,
}

/// bind()가 BeginDraw로 연 타일 상태. unbind()에서 EndDraw + 자원 정리.
/// (WR은 bind↔unbind를 1:1로 짝지어 호출하므로 동시에 하나만 존재.)
struct BoundTile {
    /// EndDraw를 걸 서피스를 다시 찾기 위한 키(bind~unbind 사이 서피스 파괴 없음).
    surface_id: NativeSurfaceId,
    /// bind가 만든 EGL pbuffer(EGLSurface as usize). unbind에서 destroy.
    pbuffer: usize,
    /// BeginDraw가 돌려준 텍스처(AddRef됨). WR의 GL draw 동안 살려두고 unbind에서 Release.
    texture: *mut ID3D11Texture2D,
}

/// 창(painter)당 하나. `webrender::Compositor`를 구현해 picture cache 타일을
/// DComp 가상 서피스에 직접 그리게 한다. 전역 상태 없음.
pub struct DCompNativeCompositor {
    rendering_context: Rc<dyn RenderingContext>,
    dcomp_device: Option<ComOwned<IDCompositionDevice>>,
    /// HWND에 귀속된 컴포지션 타깃. 생성 후 직접 호출은 없고 수명 유지 목적으로만 보관하며,
    /// deinit/Drop에서 명시 Release한다.
    _target: Option<ComOwned<IDCompositionTarget>>,
    root_visual: Option<ComOwned<IDCompositionVisual>>,
    surfaces: HashMap<NativeSurfaceId, SurfaceEntry>,
    bound: Option<BoundTile>,
    /// ANGLE D3D11 디바이스(비소유 — rendering_context가 수명 보유). 스왑체인 생성에 사용.
    #[allow(dead_code)] // Task 3에서 결선 시 제거(create_composition_swapchain 호출부가 사용).
    d3d11_device: *mut ID3D11Device,
    /// 스왑체인 생성용 DXGI 팩토리. 확보 실패(None)면 하이브리드 승격 불가 — Virtual만 사용.
    #[allow(dead_code)] // Task 3에서 결선 시 제거(create_composition_swapchain이 사용).
    dxgi_factory: Option<ComOwned<IDXGIFactory2>>,
    warned_scale: bool,
    warned_rounded_clip: bool,
    warned_external_surface: bool,
    warned_enable_native: bool,
}

/// `SERVO_COMPOSITOR_DCOMP`가 truthy면 네이티브 컴포지터 사용 요청. 판정 정본은 surfman
/// 공개 함수(paint_api 경유 재수출) — surfman은 같은 판정으로 창 서피스를 DComp 속성 없이
/// 만들고(Task 1) present-path-fast를 끈다(ppf는 pbuffer 렌더에도 발동해 타일 방향을
/// 깨뜨림). 따라서 painter가 RenderingContext를 만들기 전에 켜져 있어야 전체 구성이
/// 정합한다. (ANGLE이 아닌 빌드에서는 항상 false — 네이티브 컴포지터 불성립.)
pub fn enabled() -> bool {
    paint_api::rendering_context::dcomp_native_compositor_requested()
}

/// 컴포지터를 생성한다. 실패(HWND/디바이스 없음, HRESULT 실패)면 warn 후 None을 돌려
/// 호출자(Task 5 painter)가 기본 Draw 경로로 폴백하게 한다. 절대 패닉하지 않는다.
pub fn maybe_create(
    rendering_context: &Rc<dyn RenderingContext>,
) -> Option<DCompNativeCompositor> {
    let hwnd = rendering_context.window_hwnd().or_else(|| {
        warn!("[dcomp-native] no HWND; falling back to Draw");
        None
    })?;
    let d3d = rendering_context.angle_d3d11_device_ptr().or_else(|| {
        warn!("[dcomp-native] no ANGLE D3D11 device; falling back to Draw");
        None
    })? as *mut ID3D11Device;

    // Safety: d3d는 렌더링 컨텍스트가 보유한 살아있는 ANGLE D3D11 디바이스(Task 2 계약).
    // AddRef하지 않으므로 여기서 Release하지 않는다.
    unsafe {
        // QI IDXGIDevice → DCompositionCreateDevice → CreateTargetForHwnd(topmost=TRUE)
        // → CreateVisual(root) → SetRoot. 각 HRESULT 실패면 warn + None (PoC G1 시퀀스).
        let mut dxgi_raw: *mut IDXGIDevice = ptr::null_mut();
        let hr = (*d3d).QueryInterface(
            &IDXGIDevice::uuidof(),
            &mut dxgi_raw as *mut _ as *mut _,
        );
        if hr < 0 || dxgi_raw.is_null() {
            warn!("[dcomp-native] QI IDXGIDevice failed (hr=0x{:08x}); falling back to Draw", hr as u32);
            return None;
        }
        // dxgi는 dcomp 디바이스 생성 + 팩토리 확보(아래)에 쓰고, 함수 종료 시 Drop으로 Release된다.
        let dxgi = ComOwned::from_raw(dxgi_raw)?;

        // 스왑체인 생성용 팩토리: dxgi 디바이스 → 어댑터 → 부모 팩토리(IDXGIFactory2).
        // 실패해도 컴포지터는 성립(하이브리드 승격만 불가 → Virtual 유지) — None 허용.
        let dxgi_factory = {
            let mut adapter_raw: *mut IDXGIAdapter = ptr::null_mut();
            let hr = (*dxgi.as_ptr()).GetAdapter(&mut adapter_raw);
            if hr < 0 || adapter_raw.is_null() {
                warn!("[dcomp-native] GetAdapter failed (hr=0x{:08x}); swapchain promotion disabled", hr as u32);
                None
            } else {
                let adapter = ComOwned::from_raw(adapter_raw);
                adapter.and_then(|adapter| {
                    let mut factory_raw: *mut IDXGIFactory2 = ptr::null_mut();
                    let hr = (*adapter.as_ptr()).GetParent(
                        &IDXGIFactory2::uuidof(),
                        &mut factory_raw as *mut _ as *mut _,
                    );
                    if hr < 0 || factory_raw.is_null() {
                        warn!("[dcomp-native] GetParent(IDXGIFactory2) failed (hr=0x{:08x}); swapchain promotion disabled", hr as u32);
                        None
                    } else {
                        ComOwned::from_raw(factory_raw)
                    }
                })
            }
        };

        let mut dcomp_raw: *mut IDCompositionDevice = ptr::null_mut();
        let hr = DCompositionCreateDevice(
            dxgi.as_ptr(),
            &IDCompositionDevice::uuidof(),
            &mut dcomp_raw as *mut _ as *mut _,
        );
        if hr < 0 || dcomp_raw.is_null() {
            warn!("[dcomp-native] DCompositionCreateDevice failed (hr=0x{:08x}); falling back to Draw", hr as u32);
            return None;
        }
        let dcomp_device = ComOwned::from_raw(dcomp_raw)?;

        let mut target_raw: *mut IDCompositionTarget = ptr::null_mut();
        let hr = (*dcomp_device.as_ptr()).CreateTargetForHwnd(hwnd as HWND, TRUE, &mut target_raw);
        if hr < 0 || target_raw.is_null() {
            warn!("[dcomp-native] CreateTargetForHwnd failed (hr=0x{:08x}); falling back to Draw", hr as u32);
            return None;
        }
        let target = ComOwned::from_raw(target_raw)?;

        let mut root_raw: *mut IDCompositionVisual = ptr::null_mut();
        let hr = (*dcomp_device.as_ptr()).CreateVisual(&mut root_raw);
        if hr < 0 || root_raw.is_null() {
            warn!("[dcomp-native] CreateVisual(root) failed (hr=0x{:08x}); falling back to Draw", hr as u32);
            return None;
        }
        let root_visual = ComOwned::from_raw(root_raw)?;

        let hr = (*target.as_ptr()).SetRoot(root_visual.as_ptr());
        if hr < 0 {
            warn!("[dcomp-native] SetRoot failed (hr=0x{:08x}); falling back to Draw", hr as u32);
            return None;
        }

        Some(DCompNativeCompositor {
            rendering_context: rendering_context.clone(),
            dcomp_device: Some(dcomp_device),
            _target: Some(target),
            root_visual: Some(root_visual),
            surfaces: HashMap::new(),
            bound: None,
            d3d11_device: d3d,
            dxgi_factory,
            warned_scale: false,
            warned_rounded_clip: false,
            warned_external_surface: false,
            warned_enable_native: false,
        })
    }
}

impl DCompNativeCompositor {
    fn dcomp_device_ptr(&self) -> Option<*mut IDCompositionDevice> {
        self.dcomp_device.as_ref().map(ComOwned::as_ptr)
    }

    fn root_visual_ptr(&self) -> Option<*mut IDCompositionVisual> {
        self.root_visual.as_ref().map(ComOwned::as_ptr)
    }

    /// 컴포지션용 flip 스왑체인 생성. 실패 시 None(호출자는 Virtual 유지 폴백).
    /// FLIP_DISCARD + BufferCount 2: 이전 버퍼를 읽지 않는다 — 정확성은
    /// FrameCoverage의 full-coverage Present 규칙이 보장(계획 Global Constraints).
    /// (현재 호출자 없음 — Task 3에서 결선하며 이 allow를 제거한다.)
    #[allow(dead_code)]
    fn create_composition_swapchain(
        &self,
        size: DeviceIntSize,
        is_opaque: bool,
    ) -> Option<ComOwned<IDXGISwapChain1>> {
        let factory = self.dxgi_factory.as_ref()?.as_ptr();
        if size.width <= 0 || size.height <= 0 {
            return None;
        }
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.width as u32,
            Height: size.height as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: 0,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH, // CreateSwapChainForComposition 필수값
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: if is_opaque { DXGI_ALPHA_MODE_IGNORE } else { DXGI_ALPHA_MODE_PREMULTIPLIED },
            Flags: 0,
        };
        // Safety: factory/디바이스는 살아있는 COM 포인터(생성 시 확보). out-param은 ComOwned로.
        unsafe {
            let mut sc_raw: *mut IDXGISwapChain1 = ptr::null_mut();
            let hr = (*factory).CreateSwapChainForComposition(
                self.d3d11_device as *mut IUnknown,
                &desc,
                ptr::null_mut(),
                &mut sc_raw,
            );
            if hr < 0 || sc_raw.is_null() {
                warn!("[dcomp-native] CreateSwapChainForComposition {}x{} failed (hr=0x{:08x})",
                    size.width, size.height, hr as u32);
                return None;
            }
            ComOwned::from_raw(sc_raw)
        }
    }

    /// 외부/백드롭 서피스 요청에 대한 warn-once. Servo는 prefer_compositor_surface를
    /// 설정하지 않으므로 이 경로는 도달하지 않는다(grep 확정).
    fn warn_external_surface_once(&mut self) {
        if !self.warned_external_surface {
            warn!(
                "[dcomp-native] external/backdrop compositor surface requested but not \
                 implemented (unreachable in Servo); ignoring"
            );
            self.warned_external_surface = true;
        }
    }

    /// mid-bind 상태였던 pbuffer/텍스처를 EndDraw 없이 정리한다(서피스가 사라지는 경로용).
    fn drop_bound_without_enddraw(&mut self) {
        if let Some(bound) = self.bound.take() {
            self.rendering_context.destroy_render_pbuffer(bound.pbuffer);
            if !bound.texture.is_null() {
                // Safety: BeginDraw가 돌려준 AddRef된 텍스처를 한 번 Release.
                unsafe {
                    (*(bound.texture as *mut IUnknown)).Release();
                }
            }
        }
    }

    /// 모든 COM/EGL 자원을 명시 해제한다(deinit·Drop 공용, 멱등).
    /// §3-q UAF 교훈: 자식(pbuffer→서피스)부터 디바이스 순으로 해제한다. WR renderer
    /// deinit이 egl.Terminate보다 먼저 이 경로를 호출하므로 EGL 자원이 아직 살아있다.
    fn release_all(&mut self) {
        self.drop_bound_without_enddraw();
        // ComOwned Drop이 각 visual + virtual_surface를 Release.
        self.surfaces.clear();
        self.root_visual = None;
        self._target = None;
        self.dcomp_device = None;
    }
}

impl Drop for DCompNativeCompositor {
    fn drop(&mut self) {
        self.release_all();
    }
}

impl Compositor for DCompNativeCompositor {
    fn create_surface(
        &mut self,
        _device: &mut Device,
        id: NativeSurfaceId,
        virtual_offset: DeviceIntPoint,
        tile_size: DeviceIntSize,
        is_opaque: bool,
    ) {
        let Some(dcomp_device) = self.dcomp_device_ptr() else {
            return;
        };
        let alpha_mode = if is_opaque {
            DXGI_ALPHA_MODE_IGNORE
        } else {
            DXGI_ALPHA_MODE_PREMULTIPLIED
        };

        // Safety: dcomp_device는 살아있는 IDCompositionDevice. 각 out-param을 ComOwned로
        // 감싸 실패 시 조기 반환에서 자동 Release된다.
        let entry = unsafe {
            let mut vsurf_raw: *mut IDCompositionVirtualSurface = ptr::null_mut();
            let hr = (*dcomp_device).CreateVirtualSurface(
                VIRTUAL_SURFACE_SIZE as u32,
                VIRTUAL_SURFACE_SIZE as u32,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                alpha_mode,
                &mut vsurf_raw,
            );
            if hr < 0 || vsurf_raw.is_null() {
                warn!("[dcomp-native] CreateVirtualSurface failed (hr=0x{:08x})", hr as u32);
                return;
            }
            let Some(virtual_surface) = ComOwned::from_raw(vsurf_raw) else {
                return;
            };

            let mut visual_raw: *mut IDCompositionVisual = ptr::null_mut();
            let hr = (*dcomp_device).CreateVisual(&mut visual_raw);
            if hr < 0 || visual_raw.is_null() {
                warn!("[dcomp-native] CreateVisual(content) failed (hr=0x{:08x})", hr as u32);
                return;
            }
            let Some(visual) = ComOwned::from_raw(visual_raw) else {
                return;
            };

            let hr = (*visual.as_ptr()).SetContent(virtual_surface.as_ptr() as *const IUnknown);
            if hr < 0 {
                warn!("[dcomp-native] SetContent failed (hr=0x{:08x})", hr as u32);
                return;
            }

            SurfaceEntry {
                storage: SurfaceStorage::Virtual { virtual_surface },
                visual,
                virtual_offset,
                tile_size,
                is_opaque,
                tiles: std::collections::HashSet::new(),
                promote_streak: 0,
            }
        };

        // 비주얼은 매 프레임 begin_frame(RemoveAllVisuals) 후 add_surface에서 트리에 추가한다.
        if dcomp_debug() {
            log::info!(
                "[dcomp-dbg] create_surface id={:?} virtual_offset=({},{}) tile_size={}x{} opaque={}",
                id, virtual_offset.x, virtual_offset.y, tile_size.width, tile_size.height, is_opaque
            );
        }
        self.surfaces.insert(id, entry);
    }

    fn create_tile(&mut self, _device: &mut Device, id: NativeTileId) {
        // 가상 서피스는 BeginDraw 시 지연 할당 — 여기서는 Task 3 승격 판단(surface_extent)의
        // 근거가 될 타일 집합만 부기한다.
        if let Some(entry) = self.surfaces.get_mut(&id.surface_id) {
            entry.tiles.insert((id.x, id.y));
        }
    }

    fn destroy_tile(&mut self, _device: &mut Device, id: NativeTileId) {
        // Trim 최적화는 후속(고정 크기 월 창에서는 타일 집합이 안정적) — 여기서는 부기만 갱신.
        if let Some(entry) = self.surfaces.get_mut(&id.surface_id) {
            entry.tiles.remove(&(id.x, id.y));
        }
    }

    fn bind(
        &mut self,
        _device: &mut Device,
        id: NativeTileId,
        dirty_rect: DeviceIntRect,
        _valid_rect: DeviceIntRect,
    ) -> NativeSurfaceInfo {
        let fail = NativeSurfaceInfo {
            origin: DeviceIntPoint::zero(),
            fbo_id: 0,
        };

        // 서피스에서 필요한 Copy 값만 뽑아 borrow를 즉시 끝낸다(이후 self를 mutate하기 위함).
        let Some(entry) = self.surfaces.get(&id.surface_id) else {
            warn!("[dcomp-native] bind: unknown surface {:?}", id.surface_id);
            return fail;
        };
        let vsurf = match &entry.storage {
            SurfaceStorage::Virtual { virtual_surface } => virtual_surface.as_ptr(),
            SurfaceStorage::SwapChain(_) => {
                // Task 3까지 도달 불가(create_surface가 항상 Virtual만 만든다).
                warn!("[dcomp-native] bind: SwapChain storage not yet wired (surface {:?})", id.surface_id);
                return fail;
            },
        };
        let tile_rect = tile_virtual_rect(entry.virtual_offset, entry.tile_size, id.x, id.y);

        // BeginDraw는 가상공간 절대좌표 RECT를 받는다: 타일 rect를 타일-로컬 dirty로 오프셋.
        let update = RECT {
            left: tile_rect.min.x + dirty_rect.min.x,
            top: tile_rect.min.y + dirty_rect.min.y,
            right: tile_rect.min.x + dirty_rect.max.x,
            bottom: tile_rect.min.y + dirty_rect.max.y,
        };

        // Safety: vsurf는 위 서피스의 살아있는 IDCompositionVirtualSurface. BeginDraw는
        // AddRef된 텍스처와 아틀라스 오프셋을 돌려준다(PoC G2).
        let (texture, update_offset, desc) = unsafe {
            let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
            let mut update_offset = POINT { x: 0, y: 0 };
            let hr = (*vsurf).BeginDraw(
                &update,
                &ID3D11Texture2D::uuidof(),
                &mut tex as *mut _ as *mut _,
                &mut update_offset,
            );
            if hr < 0 || tex.is_null() {
                warn!("[dcomp-native] BeginDraw failed (hr=0x{:08x}); giving up tile", hr as u32);
                return fail;
            }
            // BeginDraw 텍스처의 실제 크기(아틀라스일 수 있음)로 pbuffer를 만든다.
            let mut desc: D3D11_TEXTURE2D_DESC = std::mem::zeroed();
            (*tex).GetDesc(&mut desc);
            (tex, update_offset, desc)
        };

        let pbuffer = match self.rendering_context.create_render_pbuffer_from_d3d_texture(
            texture as usize,
            UntypedSize2D::new(desc.Width as i32, desc.Height as i32),
        ) {
            Some(pbuffer) => pbuffer,
            None => {
                warn!("[dcomp-native] pbuffer wrap failed; giving up tile");
                // Safety: 실패 경로 정리 — 텍스처 Release + EndDraw로 서피스 상태 복구.
                unsafe {
                    (*(texture as *mut IUnknown)).Release();
                    let _ = (*vsurf).EndDraw();
                }
                return fail;
            },
        };

        if !self.rendering_context.make_render_pbuffer_current(pbuffer) {
            warn!("[dcomp-native] make_render_pbuffer_current failed; giving up tile");
            self.rendering_context.destroy_render_pbuffer(pbuffer);
            // Safety: 위와 동일한 실패 경로 정리.
            unsafe {
                (*(texture as *mut IUnknown)).Release();
                let _ = (*vsurf).EndDraw();
            }
            return fail;
        }

        // 텍스처는 WR의 GL draw 동안 살려두고 unbind의 EndDraw 이후 Release한다.
        self.bound = Some(BoundTile {
            surface_id: id.surface_id,
            pbuffer,
            texture,
        });

        // WR 타일-로컬 좌표계 성립: origin = update_offset - dirty_rect.min (Gecko DCLayerTree 동일).
        //
        // 좌표 규약 전제(Task 5 스모크에서 규명·해결): WR은 `DrawTarget::NativeSurface`를
        // top-left 원점으로 그리며(webrender device/gl.rs `surface_origin_is_top_left`=true,
        // ortho bottom=0/top=h + 시저 무반전), 이는 **stock ANGLE**(viewScale −1) 전제에서만
        // D3D row 0=top으로 정합한다. ANGLE의 present-path-fast는 디스플레이 전역이라
        // pbuffer(GL_FRAMEBUFFER_DEFAULT)에도 발동해(renderer11_utils.cpp UsePresentPathFast)
        // viewScale +1 + 시저 y 자동 반전으로 이 정합을 깨뜨리고 타일을 수직으로 흩뜨렸다.
        // 해결: 게이트 on이면 surfman이 EGL 디스플레이 속성에서 ppf 쌍을 제외한다
        // (luid_display_attribs, 두 호출부 동일 판정 = LUID 디스플레이 캐시 일관).
        // 시도했다 무효였던 것: EGL_SURFACE_ORIENTATION_INVERT_Y_ANGLE — ANGLE이
        // client-buffer pbuffer에 EGL_BAD_ATTRIBUTE(0x3004)로 거부.
        let origin = DeviceIntPoint::new(
            update_offset.x - dirty_rect.min.x,
            update_offset.y - dirty_rect.min.y,
        );
        if dcomp_debug() {
            log::info!(
                "[dcomp-dbg] bind surface={:?} tile=({},{}) dirty=({},{})-({},{}) tile_virt=({},{}) \
                 update_off=({},{}) tex={}x{} -> origin=({},{})",
                id.surface_id, id.x, id.y,
                dirty_rect.min.x, dirty_rect.min.y, dirty_rect.max.x, dirty_rect.max.y,
                tile_rect.min.x, tile_rect.min.y,
                update_offset.x, update_offset.y, desc.Width, desc.Height,
                origin.x, origin.y
            );
        }
        NativeSurfaceInfo { origin, fbo_id: 0 }
    }

    fn unbind(&mut self, _device: &mut Device) {
        let Some(bound) = self.bound.take() else {
            return;
        };
        // 현재 bind 중이던 서피스에 EndDraw.
        if let Some(entry) = self.surfaces.get(&bound.surface_id) {
            match &entry.storage {
                SurfaceStorage::Virtual { virtual_surface } => {
                    // Safety: 서피스는 bind~unbind 사이 파괴되지 않는다(WR 계약).
                    let hr = unsafe { (*virtual_surface.as_ptr()).EndDraw() };
                    if hr < 0 {
                        warn!("[dcomp-native] EndDraw failed (hr=0x{:08x})", hr as u32);
                    }
                },
                SurfaceStorage::SwapChain(_) => {
                    // Task 3까지 도달 불가(bind가 SwapChain 진입 전에 이미 fail 반환).
                    warn!(
                        "[dcomp-native] unbind: SwapChain storage not yet wired (surface {:?})",
                        bound.surface_id
                    );
                },
            }
        } else {
            warn!(
                "[dcomp-native] unbind: bound surface {:?} gone before EndDraw",
                bound.surface_id
            );
        }
        // EGL은 현재 서피스 파괴를 유예하므로 unbind 직후 destroy가 안전(Task 1 주석).
        self.rendering_context.destroy_render_pbuffer(bound.pbuffer);
        if !bound.texture.is_null() {
            // Safety: BeginDraw가 돌려준 AddRef된 텍스처를 EndDraw 이후 한 번 Release.
            unsafe {
                (*(bound.texture as *mut IUnknown)).Release();
            }
        }
    }

    fn begin_frame(&mut self, _device: &mut Device) {
        let Some(root) = self.root_visual_ptr() else {
            return;
        };
        // 매 프레임 z-order를 add_surface 호출 순서로 재구성하기 위해 전부 제거(트레이트 계약).
        // Safety: root는 살아있는 IDCompositionVisual.
        let hr = unsafe { (*root).RemoveAllVisuals() };
        if hr < 0 {
            warn!("[dcomp-native] RemoveAllVisuals failed (hr=0x{:08x})", hr as u32);
        }
    }

    fn add_surface(
        &mut self,
        _device: &mut Device,
        id: NativeSurfaceId,
        transform: CompositorSurfaceTransform,
        clip_rect: DeviceIntRect,
        _image_rendering: ImageRendering,
        _rounded_clip_rect: DeviceIntRect,
        rounded_clip_radii: ClipRadius,
    ) {
        // 월 시나리오는 scale=1·직사각 클립만 발생 예상 — 벗어나면 1회만 warn(스펙 §비범위).
        if (transform.scale.x - 1.0).abs() > f32::EPSILON ||
            (transform.scale.y - 1.0).abs() > f32::EPSILON
        {
            if !self.warned_scale {
                warn!(
                    "[dcomp-native] surface transform scale {:?} != 1.0 unsupported; \
                     applying offset only",
                    transform.scale
                );
                self.warned_scale = true;
            }
        }
        if rounded_clip_radii != ClipRadius::EMPTY {
            if !self.warned_rounded_clip {
                warn!("[dcomp-native] rounded clip radii unsupported; applying rectangular clip only");
                self.warned_rounded_clip = true;
            }
        }

        let Some(root) = self.root_visual_ptr() else {
            return;
        };
        let Some(entry) = self.surfaces.get(&id) else {
            warn!("[dcomp-native] add_surface: unknown surface {:?}", id);
            return;
        };
        let visual = entry.visual.as_ptr();
        let virtual_offset = entry.virtual_offset;

        // 콘텐츠는 가상공간 절대좌표(virtual_offset + 타일격자)에 그려졌다. 비주얼 오프셋을
        // (transform.offset - virtual_offset)으로 주면 콘텐츠가 창 device 좌표 transform.offset에
        // 놓인다(Gecko DCLayerTree 동일 보정, scale=1 가정).
        let offset_x = transform.offset.x - virtual_offset.x as f32;
        let offset_y = transform.offset.y - virtual_offset.y as f32;

        // DComp SetClip은 비주얼-로컬(오프셋 적용 전) 좌표를 받아 오프셋으로 변환되므로
        // (MS docs: "The clip is transformed by the OffsetX, OffsetY..."), device 클립에서
        // 비주얼 오프셋을 빼 로컬로 환산한다.
        let clip = D2D_RECT_F {
            left: clip_rect.min.x as f32 - offset_x,
            top: clip_rect.min.y as f32 - offset_y,
            right: clip_rect.max.x as f32 - offset_x,
            bottom: clip_rect.max.y as f32 - offset_y,
        };

        if dcomp_debug() {
            log::info!(
                "[dcomp-dbg] add_surface id={:?} transform.offset=({},{}) scale=({},{}) \
                 clip=({},{})-({},{}) virt_off=({},{}) -> visual_off=({},{})",
                id, transform.offset.x, transform.offset.y, transform.scale.x, transform.scale.y,
                clip_rect.min.x, clip_rect.min.y, clip_rect.max.x, clip_rect.max.y,
                virtual_offset.x, virtual_offset.y, offset_x, offset_y
            );
        }

        // Safety: visual/root는 살아있는 IDCompositionVisual. SetOffsetX/Y·SetClip은 `_1`
        // (값) 오버로드를 쓴다(PoC winapi 대조).
        unsafe {
            let hr = (*visual).SetOffsetX_1(offset_x);
            if hr < 0 {
                warn!("[dcomp-native] SetOffsetX failed (hr=0x{:08x})", hr as u32);
            }
            let hr = (*visual).SetOffsetY_1(offset_y);
            if hr < 0 {
                warn!("[dcomp-native] SetOffsetY failed (hr=0x{:08x})", hr as u32);
            }
            let hr = (*visual).SetClip_1(&clip);
            if hr < 0 {
                warn!("[dcomp-native] SetClip failed (hr=0x{:08x})", hr as u32);
            }
            // insertAbove=TRUE, reference=null → 형제 최상단에 추가. 호출 순서 = z-order(아래→위).
            let hr = (*root).AddVisual(visual, TRUE, ptr::null());
            if hr < 0 {
                warn!("[dcomp-native] AddVisual failed (hr=0x{:08x})", hr as u32);
            }
        }
    }

    fn end_frame(&mut self, _device: &mut Device) {
        let Some(dcomp_device) = self.dcomp_device_ptr() else {
            return;
        };
        // Safety: dcomp_device는 살아있는 IDCompositionDevice. Commit은 DWM 반영을 비동기 요청.
        let hr = unsafe { (*dcomp_device).Commit() };
        if hr < 0 {
            warn!("[dcomp-native] Commit failed (hr=0x{:08x})", hr as u32);
        }
    }

    fn destroy_surface(&mut self, _device: &mut Device, id: NativeSurfaceId) {
        // 방어적: 이 서피스가 mid-bind면(정상 흐름에선 없음) EndDraw 없이 bound 상태를 정리.
        if self.bound.as_ref().is_some_and(|bound| bound.surface_id == id) {
            warn!("[dcomp-native] destroy_surface on bound surface {:?}; dropping bound state", id);
            self.drop_bound_without_enddraw();
        }
        // remove → ComOwned Drop이 visual + virtual_surface를 Release.
        self.surfaces.remove(&id);
    }

    fn create_external_surface(&mut self, _device: &mut Device, _id: NativeSurfaceId, _is_opaque: bool) {
        self.warn_external_surface_once();
    }

    fn attach_external_image(
        &mut self,
        _device: &mut Device,
        _id: NativeSurfaceId,
        _external_image: ExternalImageId,
    ) {
        self.warn_external_surface_once();
    }

    fn create_backdrop_surface(&mut self, _device: &mut Device, _id: NativeSurfaceId, _color: ColorF) {
        self.warn_external_surface_once();
    }

    fn enable_native_compositor(&mut self, _device: &mut Device, _enable: bool) {
        // 디버그 커맨드 전용 경로(renderer mod.rs:1619) — Servo는 발행하지 않는다. warn-once.
        if !self.warned_enable_native {
            warn!("[dcomp-native] enable_native_compositor is a debug-only path unused by Servo; ignoring");
            self.warned_enable_native = true;
        }
    }

    fn get_capabilities(&self, _device: &mut Device) -> CompositorCapabilities {
        CompositorCapabilities {
            virtual_surface_size: VIRTUAL_SURFACE_SIZE,
            // max_update_rects=1 등 나머지는 플랫폼 기본값 유지.
            ..CompositorCapabilities::default()
        }
    }

    fn get_window_visibility(&self, _device: &mut Device) -> WindowVisibility {
        WindowVisibility::default()
    }

    fn deinit(&mut self, _device: &mut Device) {
        // WR renderer deinit 내부(= egl.Terminate 이전)에 호출됨 — 여기서 전부 명시 해제한다.
        // Drop도 같은 release_all을 부르지만, 명시 호출이 §3-q UAF 회귀 가드 역할을 겸한다.
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_virtual_rect_positions_tiles_on_grid() {
        let vo = DeviceIntPoint::new(16384, 16384);
        let ts = DeviceIntSize::new(1024, 512);
        let r = tile_virtual_rect(vo, ts, 0, 0);
        assert_eq!(r.min, vo);
        let r = tile_virtual_rect(vo, ts, 2, -1);
        assert_eq!(r.min, DeviceIntPoint::new(16384 + 2048, 16384 - 512));
        assert_eq!(r.size(), ts);
    }

    fn r(x0: i32, y0: i32, x1: i32, y1: i32) -> DeviceIntRect {
        DeviceIntRect::new(DeviceIntPoint::new(x0, y0), DeviceIntPoint::new(x1, y1))
    }

    #[test]
    fn coverage_full_only_when_every_tile_fully_drawn() {
        let tiles: std::collections::HashSet<_> = [(0, 0), (1, 0)].into_iter().collect();
        let mut cov = FrameCoverage::default();
        let valid = r(0, 0, 100, 100);
        cov.note_tile((0, 0), r(0, 0, 100, 100), valid);
        assert!(!cov.is_full(&tiles)); // 타일 하나 남음
        cov.note_tile((1, 0), r(0, 0, 50, 100), valid); // 부분 dirty → 미집계
        assert!(!cov.is_full(&tiles));
        cov.note_tile((1, 0), r(0, 0, 100, 100), valid); // 누적 프레임에서 완전 갱신
        assert!(cov.is_full(&tiles));
        cov.reset();
        assert!(!cov.is_full(&tiles));
    }

    #[test]
    fn cull_hides_fully_covered_below_opaque_top() {
        // 월 실측 구조: 전면 불투명 2장 → 하위 숨김
        let v = cull_covered(&[(r(0, 0, 1920, 1080), true), (r(0, 0, 1920, 1080), true)]);
        assert_eq!(v, vec![false, true]);
        // 최상위가 알파면 아무도 못 숨김
        let v = cull_covered(&[(r(0, 0, 1920, 1080), true), (r(0, 0, 1920, 1080), false)]);
        assert_eq!(v, vec![true, true]);
        // 부분 겹침은 유지
        let v = cull_covered(&[(r(0, 0, 1920, 1080), true), (r(0, 0, 900, 1080), true)]);
        assert_eq!(v, vec![true, true]);
        // 3겹: 최상 불투명이 아래 둘 다 덮음
        let v = cull_covered(&[
            (r(0, 0, 100, 100), true),
            (r(0, 0, 100, 100), false),
            (r(0, 0, 100, 100), true),
        ]);
        assert_eq!(v, vec![false, false, true]);
    }

    #[test]
    fn surface_extent_unions_tiles() {
        let vo = DeviceIntPoint::new(16384, 16384);
        let ts = DeviceIntSize::new(1024, 512);
        let tiles: std::collections::HashSet<_> = [(0, 0), (1, 0), (0, 1)].into_iter().collect();
        let e = surface_extent(&tiles, vo, ts).unwrap();
        assert_eq!(e, r(16384, 16384, 16384 + 2048, 16384 + 1024));
        assert!(surface_extent(&Default::default(), vo, ts).is_none());
    }
}

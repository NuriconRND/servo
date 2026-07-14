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

/// 연속 이만큼의 프레임이 전면 갱신이면 Virtual -> SwapChain 승격(히스테리시스).
const PROMOTE_STREAK: u32 = 3;
/// 부분 dirty 누적으로 Present가 이만큼 보류되면 1회 warn(표시 지연 가시화).
const WITHHOLD_WARN_FRAMES: u32 = 60;
/// 스펙 §4: 그려진 지 이만큼의 프레임이 지나야 승격 streak을 인정(시작 과도기 배제).
const PROMOTE_MIN_AGE_FRAMES: u32 = 30;
/// 스펙 §6.1: withhold가 이만큼 연속되면 가상 서피스로 강등.
const DEMOTE_AFTER_WITHHOLD: u32 = 30;
/// 스펙 §6.3: 강등 n회째 재승격 쿨다운 = BASE × 2^(n−1), 상한 CAP (프레임).
const DEMOTE_COOLDOWN_BASE: u64 = 300;
const DEMOTE_COOLDOWN_CAP: u64 = 3600;

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

/// 부분 Present catch-up용 상수(스펙 §5.2). 힌트 렉트 상한 / stale 목록 붕괴 상한.
#[allow(dead_code)] // Task 4가 소비
const MAX_PRESENT_DIRTY_RECTS: usize = 16;
const MAX_STALE_RECTS: usize = 32;

/// minuend − sub: 겹치면 최대 4조각(상/하/좌/우 밴드)으로 분해. 정확 연산(근사 금지 —
/// 차집합이 넓으면 stale 픽셀 잔존, 좁으면 신규 콘텐츠를 구본으로 덮어씀. 스펙 §5.2-2).
fn subtract_rect(minuend: DeviceIntRect, sub: DeviceIntRect) -> Vec<DeviceIntRect> {
    let Some(ix) = minuend.intersection(&sub) else {
        return vec![minuend];
    };
    let mut out = Vec::with_capacity(4);
    // 상단 밴드
    if minuend.min.y < ix.min.y {
        out.push(DeviceIntRect::new(
            DeviceIntPoint::new(minuend.min.x, minuend.min.y),
            DeviceIntPoint::new(minuend.max.x, ix.min.y),
        ));
    }
    // 하단 밴드
    if ix.max.y < minuend.max.y {
        out.push(DeviceIntRect::new(
            DeviceIntPoint::new(minuend.min.x, ix.max.y),
            DeviceIntPoint::new(minuend.max.x, minuend.max.y),
        ));
    }
    // 좌측 밴드(교차 세로 구간만)
    if minuend.min.x < ix.min.x {
        out.push(DeviceIntRect::new(
            DeviceIntPoint::new(minuend.min.x, ix.min.y),
            DeviceIntPoint::new(ix.min.x, ix.max.y),
        ));
    }
    // 우측 밴드(교차 세로 구간만)
    if ix.max.x < minuend.max.x {
        out.push(DeviceIntRect::new(
            DeviceIntPoint::new(ix.max.x, ix.min.y),
            DeviceIntPoint::new(minuend.max.x, ix.max.y),
        ));
    }
    out
}

/// 감수 목록 전체를 순차 차감. 빈/역전 렉트는 자연 소거(밴드 조건이 걸러냄).
fn region_subtract(
    minuend: &[DeviceIntRect],
    subtrahend: &[DeviceIntRect],
) -> Vec<DeviceIntRect> {
    let mut acc: Vec<DeviceIntRect> = minuend.to_vec();
    for sub in subtrahend {
        let mut next = Vec::with_capacity(acc.len());
        for m in acc {
            next.extend(subtract_rect(m, *sub));
        }
        acc = next;
    }
    acc
}

/// 버퍼 2개(FLIP_SEQUENTIAL) 기준 stale 영역 부기(버퍼-로컬 좌표, 스펙 §5.2).
/// stale[i] = 버퍼 i가 놓친 갱신 영역. Present(D) 시 현재 버퍼는 완성(∅), 반대
/// 버퍼에 D 누적, 쓰기 대상 교대. 과대(바운딩 붕괴)는 안전 — 이미 최신인 영역을
/// 한 번 더 복사할 뿐. Task 1 G4 실측 로테이션이 다르면 이 모델을 그에 맞춘다.
#[derive(Default)]
#[allow(dead_code)] // Task 4가 소비
struct StaleTracker {
    stale: [Vec<DeviceIntRect>; 2],
    cur: usize,
}

impl StaleTracker {
    /// 이번 프레임(더티 frame_dirty)을 Present한 직후 호출.
    #[allow(dead_code)] // Task 4가 소비
    fn on_present(&mut self, frame_dirty: &[DeviceIntRect], full: DeviceIntRect) {
        self.stale[self.cur].clear();
        let other = 1 - self.cur;
        self.stale[other].extend_from_slice(frame_dirty);
        if self.stale[other].len() > MAX_STALE_RECTS {
            let union = self.stale[other]
                .iter()
                .fold(None::<DeviceIntRect>, |acc, r| {
                    Some(acc.map_or(*r, |a| a.union(r)))
                })
                .unwrap_or(full);
            self.stale[other] = vec![union];
        }
        self.cur = other;
    }

    /// Present 직전 catch-up 복사 대상(= 현재 버퍼 stale − 이번 프레임 더티).
    #[allow(dead_code)] // Task 4가 소비
    fn catchup_rects(&self, frame_dirty: &[DeviceIntRect]) -> Vec<DeviceIntRect> {
        region_subtract(&self.stale[self.cur], frame_dirty)
    }

    #[allow(dead_code)] // Task 4가 소비
    fn reset(&mut self) {
        self.stale[0].clear();
        self.stale[1].clear();
        self.cur = 0;
    }
}

/// SERVO_COMPOSITOR_DCOMP 값의 세부 모드. "surface"=가상 서피스 전용(구 경로 A/B),
/// 그 외 truthy=하이브리드(전면 갱신 서피스를 스왑체인으로 승격).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StorageMode {
    Hybrid,
    SurfaceOnly,
}

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
fn cull_disabled() -> bool {
    static NO_CULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NO_CULL.get_or_init(|| std::env::var("SERVO_DCOMP_NO_CULL").is_ok())
}

/// 스왑체인 백버퍼의 유효 피복(타일 단위, Present까지 누적).
/// Present를 안 하면 flip이라도 GetBuffer(0)가 같은 버퍼이므로 누적이 성립한다.
/// 판정은 보수적: bind의 dirty가 그 타일의 valid_rect 전체를 덮을 때만 집계
/// (부분 dirty 타일은 미피복 취급 — 잔상 불허, 지연 허용. 스펙 §7 정제).
#[derive(Default)]
struct FrameCoverage {
    covered_tiles: std::collections::HashSet<(i32, i32)>,
}

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

/// 타일 집합이 extent(union)를 빈틈없이 채우는 조밀 사각형인지 판정.
/// 스왑체인 크기는 extent(union)로 만들지만 coverage 판정은 존재하는 타일만 훑으므로,
/// 타일 집합이 조밀 사각형이 아니면(구멍) union 내부 무타일 영역이 FLIP_DISCARD의
/// 미정의 픽셀인 채 Present될 수 있다(현 워크로드에서는 발생하지 않는다는 가정을
/// 여기서 강제한다). 오버플로 방지 위해 i64로 계산.
fn tiles_are_dense(tile_count: usize, tile_size: DeviceIntSize, extent_size: DeviceIntSize) -> bool {
    tile_count as i64 * tile_size.width as i64 * tile_size.height as i64
        == extent_size.width as i64 * extent_size.height as i64
}

/// 레이어 컬링: entries는 add_surface 순서(z 아래→위)의 (device 클립, 불투명 여부).
/// 최상위부터 훑어, 불투명 클립이 완전히 포함하는 하위 항목을 숨긴다.
/// 알파(불투명 아님) 항목은 절대 숨겨지지 않는 것이 아니라 — 알파 항목도 "위의
/// 불투명"에 완전히 덮이면 안 보이므로 숨김 대상이 될 수 있다. 숨기는 주체가
/// 불투명이어야 한다는 것이 안전 조건이다.
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

/// 스왑체인 백버퍼를 감싼 프레임 pbuffer. bind가 GetBuffer(0)로 백버퍼를 얻어 채우고,
/// end_frame(또는 파기 경로)이 release_frame_pbuffer로 정리한다.
struct FramePbuffer {
    pbuffer: usize,
    /// GetBuffer(0)가 AddRef해 돌려준 백버퍼 텍스처. 파기 시 Release.
    texture: *mut ID3D11Texture2D,
}

/// 스왑체인의 프레임 pbuffer(있으면)를 파기하고 백버퍼 텍스처를 Release한다.
/// end_frame·destroy_surface·release_all 공용 — 모든 정리 경로에서 pbuffer 1회,
/// 텍스처 1회만 해제됨을 보장한다(take()로 재진입 안전).
fn release_frame_pbuffer(rc: &Rc<dyn RenderingContext>, sc: &mut SwapChainStorage) {
    if let Some(fp) = sc.frame_pbuffer.take() {
        rc.destroy_render_pbuffer(fp.pbuffer);
        if !fp.texture.is_null() {
            // Safety: GetBuffer(0)가 AddRef한 백버퍼 텍스처를 한 번 반납.
            unsafe {
                (*(fp.texture as *mut IUnknown)).Release();
            }
        }
    }
}

/// visual 오프셋·클립 적용(add_surface·content-swap 공용 산식).
/// 콘텐츠는 가상공간 절대좌표(Virtual: content_anchor=0) 또는 스왑체인 0-기준 좌표
/// (content_anchor=그 스왑체인의 anchor)에 그려져 있다. 비주얼 오프셋을
/// (transform.offset - virtual_offset + content_anchor)로 주면 콘텐츠가 창 device 좌표
/// transform.offset에 놓인다(Gecko DCLayerTree 동일 보정, scale=1 가정).
/// DComp SetClip은 비주얼-로컬(오프셋 적용 전) 좌표를 받으므로(MS docs: "The clip is
/// transformed by the OffsetX, OffsetY...") device 클립에서 오프셋을 빼 환산한다.
/// SetOffsetX/Y·SetClip은 `_1`(값) 오버로드(PoC winapi 대조). 적용한 오프셋을 돌려준다.
fn apply_visual_placement(
    visual: &ComOwned<IDCompositionVisual>,
    placement: LastPlacement,
    virtual_offset: DeviceIntPoint,
    content_anchor: DeviceIntPoint,
) -> (f32, f32) {
    let offset_x = placement.transform_offset.0 - virtual_offset.x as f32 + content_anchor.x as f32;
    let offset_y = placement.transform_offset.1 - virtual_offset.y as f32 + content_anchor.y as f32;
    let clip = D2D_RECT_F {
        left: placement.clip_rect.min.x as f32 - offset_x,
        top: placement.clip_rect.min.y as f32 - offset_y,
        right: placement.clip_rect.max.x as f32 - offset_x,
        bottom: placement.clip_rect.max.y as f32 - offset_y,
    };
    // Safety: visual은 ComOwned가 수명을 보장하는 살아있는 IDCompositionVisual.
    unsafe {
        let hr = (*visual.as_ptr()).SetOffsetX_1(offset_x);
        if hr < 0 {
            warn!("[dcomp-native] SetOffsetX failed (hr=0x{:08x})", hr as u32);
        }
        let hr = (*visual.as_ptr()).SetOffsetY_1(offset_y);
        if hr < 0 {
            warn!("[dcomp-native] SetOffsetY failed (hr=0x{:08x})", hr as u32);
        }
        let hr = (*visual.as_ptr()).SetClip_1(&clip);
        if hr < 0 {
            warn!("[dcomp-native] SetClip failed (hr=0x{:08x})", hr as u32);
        }
    }
    (offset_x, offset_y)
}

/// 하이브리드 승격된 서피스의 스왑체인 저장소(전면 갱신 서피스를 flip 스왑체인으로).
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
    /// visual에 **현재 붙어 있는** 콘텐츠의 anchor(표시 오프셋 산식의 기준).
    /// None = fallback_virtual(가상좌표 콘텐츠) 표시 중 → 가상 산식(-virtual_offset).
    /// Some(a) = anchor a로 그려진 스왑체인 표시 중 → 0-기준 산식(+a).
    /// regen 후에는 옛 스왑체인이 그대로 붙어 있으므로 옛 anchor가 유지된다 —
    /// sc.anchor(현재 렌더 대상)와 다를 수 있다. content-swap에서만 갱신.
    displayed_anchor: Option<DeviceIntPoint>,
}

/// 서피스 콘텐츠의 백엔드. 기본은 `Virtual`(가상 서피스), 연속 전면 갱신 서피스는
/// end_frame에서 `SwapChain`(flip 스왑체인)으로 승격된다.
enum SurfaceStorage {
    Virtual {
        virtual_surface: ComOwned<IDCompositionVirtualSurface>,
    },
    SwapChain(SwapChainStorage),
}

/// add_surface가 마지막으로 기록한 배치(WR device 좌표). content-swap 시 같은 Commit에서
/// 오프셋·클립을 새 콘텐츠 산식으로 재적용하기 위해 보관한다(무글리치 원자 전환).
#[derive(Clone, Copy)]
struct LastPlacement {
    /// transform.offset (scale=1 가정, add_surface와 동일).
    transform_offset: (f32, f32),
    /// device 좌표 클립 rect.
    clip_rect: DeviceIntRect,
}

/// 창당 하나의 picture cache 슬라이스에 대응하는 DComp 서피스 저장소 + 비주얼.
struct SurfaceEntry {
    storage: SurfaceStorage,
    visual: ComOwned<IDCompositionVisual>,
    virtual_offset: DeviceIntPoint,
    tile_size: DeviceIntSize,
    is_opaque: bool,
    /// bind/unbind된 타일 좌표 부기(승격 판단·surface_extent 근거).
    tiles: std::collections::HashSet<(i32, i32)>,
    /// 이번 프레임 Virtual bind가 note_tile한 타일 피복(전면 갱신 승격 판정용).
    /// Virtual 전용 부기 — 스왑체인은 sc.coverage를 쓴다. end_frame Virtual arm에서 reset.
    frame_coverage: FrameCoverage,
    /// 연속으로 전면 갱신을 만족한 프레임 수(승격 히스테리시스). 승격 시 0으로 리셋.
    promote_streak: u32,
    /// 마지막 add_surface 배치(content-swap의 오프셋 재적용 근거). add_surface 전 None.
    last_placement: Option<LastPlacement>,
    /// 이 서피스가 그려진(bind된) 프레임의 누적 수 — PROMOTE_MIN_AGE_FRAMES 게이트.
    drawn_frames: u32,
    /// 강등 누적 횟수(쿨다운 지수의 n).
    demote_count: u32,
    /// 이 프레임 번호 전까지 승격 금지(재승격 쿨다운). 0 = 제한 없음.
    promote_blocked_until: u64,
    /// Virtual bind 프레임에서 부분 더티는 frame_coverage 집계에 미등록 — 별도 플래그로 "그려짐" 추적.
    frame_drawn_partial: bool,
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
    /// 이번 프레임 add_surface가 기록한 (서피스, device 클립, 불투명 여부) — z-order대로
    /// 누적(add_surface 호출 순 = 아래→위). AddVisual은 end_frame에서 컬링 후 일괄 수행한다.
    /// begin_frame에서 clear.
    frame_surfaces: Vec<(NativeSurfaceId, DeviceIntRect, bool)>,
    /// ANGLE D3D11 디바이스(비소유 — rendering_context가 수명 보유). 스왑체인 생성에 사용.
    d3d11_device: *mut ID3D11Device,
    /// 스왑체인 생성용 DXGI 팩토리. 확보 실패(None)면 하이브리드 승격 불가 — Virtual만 사용.
    dxgi_factory: Option<ComOwned<IDXGIFactory2>>,
    warned_scale: bool,
    warned_rounded_clip: bool,
    warned_external_surface: bool,
    warned_enable_native: bool,
    /// 스왑체인 생성이 한 번이라도 실패하면 이후 승격을 영구 중단(warn 1회, Virtual 유지).
    warned_promote_fail: bool,
    /// regen(리사이즈 재생성)용 스왑체인 생성이 실패하면 이후 regen을 영구 중단
    /// (warn 1회, 옛 스왑체인 콘텐츠 유지 — 매 프레임 재시도 스팸 방지).
    warned_regen_fail: bool,
    /// 누적된 프레임 번호 — begin_frame에서 증가. 쿨다운 만료 시점(frame_counter >= promote_blocked_until).
    frame_counter: u64,
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
            frame_surfaces: Vec::new(),
            d3d11_device: d3d,
            dxgi_factory,
            warned_scale: false,
            warned_rounded_clip: false,
            warned_external_surface: false,
            warned_enable_native: false,
            warned_promote_fail: false,
            warned_regen_fail: false,
            frame_counter: 0,
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
    fn create_composition_swapchain(
        &self,
        size: DeviceIntSize,
        is_opaque: bool,
    ) -> Option<ComOwned<IDXGISwapChain1>> {
        let Some(factory) = self.dxgi_factory.as_ref().map(ComOwned::as_ptr) else {
            warn!("[dcomp-native] create_composition_swapchain: no DXGI factory; giving up");
            return None;
        };
        if size.width <= 0 || size.height <= 0 {
            warn!("[dcomp-native] create_composition_swapchain: invalid size {}x{}", size.width, size.height);
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
        // SwapChain storage의 frame_pbuffer는 RAII가 아니므로 clear 전에 명시 정리한다.
        let rc = self.rendering_context.clone();
        for entry in self.surfaces.values_mut() {
            if let SurfaceStorage::SwapChain(sc) = &mut entry.storage {
                release_frame_pbuffer(&rc, sc);
            }
        }
        // ComOwned Drop이 각 visual + storage(virtual_surface 또는 swapchain/fallback)를 Release.
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
                frame_coverage: FrameCoverage::default(),
                promote_streak: 0,
                last_placement: None,
                drawn_frames: 0,
                demote_count: 0,
                promote_blocked_until: 0,
                frame_drawn_partial: false,
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
        valid_rect: DeviceIntRect,
    ) -> NativeSurfaceInfo {
        let fail = NativeSurfaceInfo {
            origin: DeviceIntPoint::zero(),
            fbo_id: 0,
        };
        // rendering_context 호출을 엔트리 borrow와 분리(같은 self의 다른 필드지만 명시
        // clone으로 borrow 충돌 여지를 없앤다 — 엔트리는 get_mut로 지속 borrow된다).
        let rc = self.rendering_context.clone();

        let Some(entry) = self.surfaces.get_mut(&id.surface_id) else {
            warn!("[dcomp-native] bind: unknown surface {:?}", id.surface_id);
            return fail;
        };
        let tile_rect = tile_virtual_rect(entry.virtual_offset, entry.tile_size, id.x, id.y);

        match &mut entry.storage {
            SurfaceStorage::SwapChain(sc) => {
                // 프레임 첫 bind에서 백버퍼 래핑(프레임 캐시). Present를 안 하면
                // GetBuffer(0)가 같은 버퍼이므로 미프레젠트 프레임 간 누적도 정확.
                if sc.frame_pbuffer.is_none() {
                    // Safety: swapchain은 살아있는 IDXGISwapChain1. GetBuffer는 AddRef된
                    // RENDER_TARGET 백버퍼를 돌려준다.
                    let texture = unsafe {
                        let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
                        let hr = (*sc.swapchain.as_ptr()).GetBuffer(
                            0,
                            &ID3D11Texture2D::uuidof(),
                            &mut tex as *mut _ as *mut _,
                        );
                        if hr < 0 || tex.is_null() {
                            warn!("[dcomp-native] GetBuffer(0) failed (hr=0x{:08x}); giving up tile", hr as u32);
                            return fail;
                        }
                        tex
                    };
                    let pbuffer = match rc.create_render_pbuffer_from_d3d_texture(
                        texture as usize,
                        UntypedSize2D::new(sc.size.width, sc.size.height),
                    ) {
                        Some(p) => p,
                        None => {
                            warn!("[dcomp-native] swapchain pbuffer wrap failed; giving up tile");
                            // Safety: GetBuffer가 AddRef한 텍스처 반납.
                            unsafe {
                                (*(texture as *mut IUnknown)).Release();
                            }
                            return fail;
                        },
                    };
                    sc.frame_pbuffer = Some(FramePbuffer { pbuffer, texture });
                }
                let Some(fp) = sc.frame_pbuffer.as_ref() else {
                    return fail;
                };
                if !rc.make_render_pbuffer_current(fp.pbuffer) {
                    warn!("[dcomp-native] make current (swapchain) failed; giving up tile");
                    return fail;
                }
                sc.coverage.note_tile((id.x, id.y), dirty_rect, valid_rect);
                sc.drawn_this_frame = true;
                // 스왑체인 bind는 BoundTile을 만들지 않는다(EndDraw/파기 대상 없음 —
                // unbind는 bound==None이라 자연히 no-op).
                self.bound = None;
                // 백버퍼는 0-기준 좌표: 타일의 가상공간 위치에서 anchor(백버퍼 (0,0)의
                // 가상좌표)를 빼 백버퍼-로컬 origin을 만든다. 표시 측 오프셋은 add_surface가
                // anchor를 더해 보정한다.
                let origin = DeviceIntPoint::new(
                    tile_rect.min.x - sc.anchor.x,
                    tile_rect.min.y - sc.anchor.y,
                );
                if dcomp_debug() {
                    log::info!(
                        "[dcomp-dbg] bind(swapchain) surface={:?} tile=({},{}) anchor=({},{}) -> origin=({},{})",
                        id.surface_id, id.x, id.y, sc.anchor.x, sc.anchor.y, origin.x, origin.y
                    );
                }
                NativeSurfaceInfo { origin, fbo_id: 0 }
            },
            SurfaceStorage::Virtual { virtual_surface } => {
                let vsurf = virtual_surface.as_ptr();

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

                let pbuffer = match rc.create_render_pbuffer_from_d3d_texture(
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

                if !rc.make_render_pbuffer_current(pbuffer) {
                    warn!("[dcomp-native] make_render_pbuffer_current failed; giving up tile");
                    rc.destroy_render_pbuffer(pbuffer);
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

                // 승격 판정용 프레임 피복 집계(dirty가 valid 전체를 덮는 전면 타일만).
                entry.frame_coverage.note_tile((id.x, id.y), dirty_rect, valid_rect);
                // 부분 더티 프레임도 "그려짐"으로 세기(나이 카운터에 포함).
                entry.frame_drawn_partial = true;

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
            },
        }
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
                    // 도달 불가: 스왑체인 bind는 BoundTile을 만들지 않으므로(self.bound=None)
                    // 그 타일의 unbind는 위 self.bound.take()에서 이미 조기 반환한다.
                    warn!(
                        "[dcomp-native] unbind: unexpected bound tile on swapchain surface {:?}",
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
        self.frame_counter += 1;
        // 매 프레임 z-order를 add_surface 호출 순서로 재구성하기 위해 전부 제거(트레이트 계약).
        // Safety: root는 살아있는 IDCompositionVisual.
        let hr = unsafe { (*root).RemoveAllVisuals() };
        if hr < 0 {
            warn!("[dcomp-native] RemoveAllVisuals failed (hr=0x{:08x})", hr as u32);
        }
        // add_surface의 AddVisual은 end_frame으로 이연(Task 4 컬링) — 이번 프레임 기록 초기화.
        self.frame_surfaces.clear();
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

        let Some(entry) = self.surfaces.get_mut(&id) else {
            warn!("[dcomp-native] add_surface: unknown surface {:?}", id);
            return;
        };
        let virtual_offset = entry.virtual_offset;
        // 오프셋 산식의 anchor는 storage가 아니라 **visual에 지금 붙어 있는 콘텐츠**
        // (displayed_anchor)를 따른다. 승격~첫 완전 Present 사이(및 regen 직후)에는
        // storage=SwapChain이어도 visual은 아직 구 콘텐츠(fallback_virtual=가상좌표,
        // 또는 옛 anchor의 스왑체인)를 표시하므로, sc.anchor(렌더 대상) 기준으로 계산하면
        // 표시 중인 콘텐츠가 ~16384px 화면 밖으로 밀린다. content-swap 시 end_frame이
        // displayed_anchor를 갱신하고 같은 Commit에서 오프셋을 재적용한다(무글리치).
        let content_anchor = match &entry.storage {
            SurfaceStorage::SwapChain(sc) => sc.displayed_anchor.unwrap_or(DeviceIntPoint::zero()),
            SurfaceStorage::Virtual { .. } => DeviceIntPoint::zero(),
        };

        // content-swap 시 재적용할 수 있게 배치를 기록.
        let placement = LastPlacement {
            transform_offset: (transform.offset.x, transform.offset.y),
            clip_rect,
        };
        entry.last_placement = Some(placement);

        let (offset_x, offset_y) =
            apply_visual_placement(&entry.visual, placement, virtual_offset, content_anchor);

        if dcomp_debug() {
            log::info!(
                "[dcomp-dbg] add_surface id={:?} transform.offset=({},{}) scale=({},{}) \
                 clip=({},{})-({},{}) virt_off=({},{}) anchor=({},{}) -> visual_off=({},{})",
                id, transform.offset.x, transform.offset.y, transform.scale.x, transform.scale.y,
                clip_rect.min.x, clip_rect.min.y, clip_rect.max.x, clip_rect.max.y,
                virtual_offset.x, virtual_offset.y, content_anchor.x, content_anchor.y,
                offset_x, offset_y
            );
        }

        // AddVisual은 end_frame으로 이연(컬링 후 일괄 조립) — 여기서는 z-order(호출 순서 =
        // 아래→위)를 보존한 기록만 남긴다.
        self.frame_surfaces.push((id, clip_rect, entry.is_opaque));
    }

    fn end_frame(&mut self, device: &mut Device) {
        // GL 커맨드를 D3D 큐에 확실히 제출한 뒤 Present(순서 보장).
        device.gl().flush();

        let mode = storage_mode();
        let rc = self.rendering_context.clone();
        // borrow 사정(iter_mut 중 self 메서드 호출 불가)상 스왑체인 생성이 필요한 요청을
        // 모아 루프 밖에서 처리한다.
        //  - promote_requests: Virtual → SwapChain 신규 승격 (id, 승격 extent)
        //  - regen_requests: 리사이즈 등으로 지오메트리가 바뀐 기존 SwapChain 재생성 (id, 새 extent)
        let mut promote_requests: Vec<(NativeSurfaceId, DeviceIntRect)> = Vec::new();
        let mut regen_requests: Vec<(NativeSurfaceId, DeviceIntRect)> = Vec::new();

        for (id, entry) in self.surfaces.iter_mut() {
            match &mut entry.storage {
                SurfaceStorage::Virtual { .. } => {
                    // 전면 갱신 = 이 프레임의 dirty가 전 타일의 valid를 덮음(Virtual bind 집계).
                    // frame_coverage는 Virtual 전용 부기(스왑체인은 sc.coverage 사용)라
                    // 계산·리셋을 이 arm에서만 수행한다.
                    let frame_drawn = !entry.frame_coverage.covered_tiles.is_empty()
                        || entry.frame_drawn_partial;
                    let frame_full = entry.frame_coverage.is_full(&entry.tiles);
                    entry.frame_coverage.reset();
                    entry.frame_drawn_partial = false;
                    if frame_drawn {
                        entry.drawn_frames = entry.drawn_frames.saturating_add(1);
                    }
                    // 승격 상태머신(스펙 §4): streak은 MIN_AGE 경과 후부터만 누적.
                    entry.promote_streak = if frame_full
                        && entry.drawn_frames > PROMOTE_MIN_AGE_FRAMES
                    {
                        entry.promote_streak + 1
                    } else {
                        0
                    };
                    // Only opaque slices are promoted to a flip swapchain. Per design
                    // spec §5.3 (2026-07-14-dcomp-swapchain-content-design), a separate
                    // alpha slice (e.g. a fixed caption overlay) must REMAIN a virtual
                    // surface: its updates are partial/sparse, and a flip swapchain would
                    // both defeat that (partial-dirty withhold pathology, Task 5-3) and
                    // require premultiplied-alpha correctness the virtual path already
                    // provides. Promotion targets the wall's full-repaint opaque video
                    // slices only. `is_opaque` is WR's per-slice opacity classification.
                    // MIN_AGE gate added: streak must accumulate only after sufficient draw history.
                    if mode == StorageMode::Hybrid
                        && entry.is_opaque
                        && !self.warned_promote_fail
                        && entry.promote_streak >= PROMOTE_STREAK
                        && self.frame_counter >= entry.promote_blocked_until
                        && self.dxgi_factory.is_some()
                    {
                        if let Some(extent) =
                            surface_extent(&entry.tiles, entry.virtual_offset, entry.tile_size)
                        {
                            // 조밀성 가드: 타일 집합이 extent를 빈틈없이 채우지 않으면
                            // 승격하지 않는다(스왑체인 구멍이 미정의 픽셀로 Present될 위험).
                            if tiles_are_dense(entry.tiles.len(), entry.tile_size, extent.size()) {
                                promote_requests.push((*id, extent));
                            }
                        }
                    }
                },
                SurfaceStorage::SwapChain(sc) => {
                    // 리사이즈 등으로 서피스 extent가 바뀌면 스왑체인이 스테일 — 루프 밖에서
                    // 새 크기로 재생성한다(옛 콘텐츠는 DComp가 SetContent 참조로 유지 → 무글리치).
                    // regen 생성이 한 번 실패했으면 재시도하지 않는다(warn 스팸 방지, 콘텐츠 동결).
                    let cur_extent =
                        surface_extent(&entry.tiles, entry.virtual_offset, entry.tile_size);
                    let geometry_changed = cur_extent
                        .is_some_and(|e| e.min != sc.anchor || e.size() != sc.size);

                    if geometry_changed {
                        if !self.warned_regen_fail {
                            if let Some(e) = cur_extent {
                                // 조밀성 가드: 타일 집합이 e를 빈틈없이 채우지 않으면 regen을
                                // 보류한다(과도기 구멍이 있는 채로 재생성하면 그 구멍이
                                // FLIP_DISCARD 미정의 픽셀로 Present될 위험 — 다음 프레임에
                                // 재판정).
                                if tiles_are_dense(entry.tiles.len(), entry.tile_size, e.size()) {
                                    regen_requests.push((*id, e));
                                }
                            }
                        }
                    } else if sc.drawn_this_frame && sc.coverage.is_full(&entry.tiles) {
                        // Safety: 살아있는 스왑체인. SyncInterval 0 = 비블로킹(페이싱은 기존 유지).
                        let hr = unsafe { (*sc.swapchain.as_ptr()).Present(0, 0) };
                        if hr < 0 {
                            warn!("[dcomp-native] Present failed (hr=0x{:08x})", hr as u32);
                        } else {
                            sc.coverage.reset();
                            sc.withheld_frames = 0;
                            if !sc.content_attached {
                                // 첫 완전 프레젠트 → visual 콘텐츠를 스왑체인으로 전환.
                                // Safety: visual/swapchain 살아있음.
                                let hr = unsafe {
                                    (*entry.visual.as_ptr())
                                        .SetContent(sc.swapchain.as_ptr() as *const IUnknown)
                                };
                                if hr >= 0 {
                                    sc.content_attached = true;
                                    sc.fallback_virtual = None; // 구 가상 서피스 해제
                                    // 표시 콘텐츠가 (가상좌표 또는 옛 anchor) → 새 anchor 스왑체인으로
                                    // 바뀌었다. 오프셋 산식도 같은 Commit에서 새 anchor로 재적용해야
                                    // 콘텐츠 전환과 오프셋 전환이 원자적으로 반영된다(무글리치).
                                    sc.displayed_anchor = Some(sc.anchor);
                                    if let Some(placement) = entry.last_placement {
                                        apply_visual_placement(
                                            &entry.visual,
                                            placement,
                                            entry.virtual_offset,
                                            sc.anchor,
                                        );
                                    }
                                    if dcomp_debug() {
                                        log::info!("[dcomp-dbg] content-swap id={:?} -> swapchain", id);
                                    }
                                } else {
                                    warn!("[dcomp-native] SetContent(swapchain) failed (hr=0x{:08x})", hr as u32);
                                }
                            }
                        }
                    } else if sc.drawn_this_frame {
                        // 부분 갱신 → Present 보류(마지막 완전 화면 유지, 다음 프레임 누적).
                        sc.withheld_frames += 1;
                        if sc.withheld_frames == WITHHOLD_WARN_FRAMES {
                            warn!(
                                "[dcomp-native] surface {:?}: swapchain present withheld for {} frames \
                                 (partial dirty accumulating; display update delayed)",
                                id, sc.withheld_frames
                            );
                        }
                        if dcomp_debug() {
                            log::info!("[dcomp-dbg] withhold id={:?} covered={}/{}",
                                id, sc.coverage.covered_tiles.len(), entry.tiles.len());
                        }
                    }
                    // 프레임 pbuffer 파기(미프레젠트여도 다음 프레임 GetBuffer(0)=같은 버퍼).
                    release_frame_pbuffer(&rc, sc);
                    sc.drawn_this_frame = false;
                },
            }
        }

        // 승격 실행(루프 밖): 스왑체인 생성 성공 시 storage 교체, visual 콘텐츠는
        // fallback_virtual(구 가상 서피스)로 유지 — 첫 완전 Present에서 SetContent 전환.
        for (surface_id, extent) in promote_requests {
            let is_opaque = match self.surfaces.get(&surface_id) {
                Some(e) => e.is_opaque,
                None => continue,
            };
            let size = extent.size();
            let Some(swapchain) = self.create_composition_swapchain(size, is_opaque) else {
                // 생성 실패 → 이후 승격 영구 중단(Virtual 유지). helper가 warn 1회를 남긴다.
                self.warned_promote_fail = true;
                continue;
            };
            let Some(entry) = self.surfaces.get_mut(&surface_id) else {
                continue;
            };
            // 구 Virtual 서피스를 새 SwapChain storage로 교체하며 fallback으로 이동.
            // displayed_anchor=None: visual에는 여전히 fallback(가상좌표 콘텐츠)이 붙어 있다 —
            // 첫 완전 Present의 content-swap에서 Some(anchor)로 전환된다.
            let old = std::mem::replace(
                &mut entry.storage,
                SurfaceStorage::SwapChain(SwapChainStorage {
                    swapchain,
                    anchor: extent.min,
                    size,
                    coverage: FrameCoverage::default(),
                    frame_pbuffer: None,
                    drawn_this_frame: false,
                    content_attached: false,
                    withheld_frames: 0,
                    fallback_virtual: None,
                    displayed_anchor: None,
                }),
            );
            if let SurfaceStorage::Virtual { virtual_surface } = old {
                if let SurfaceStorage::SwapChain(sc) = &mut entry.storage {
                    sc.fallback_virtual = Some(virtual_surface);
                }
            }
            entry.promote_streak = 0; // 승격 완료 — Virtual 전용 부기 정리
            if dcomp_debug() {
                log::info!(
                    "[dcomp-dbg] promote id={:?} extent={}x{} anchor=({},{})",
                    surface_id, size.width, size.height, extent.min.x, extent.min.y
                );
            }
        }

        // 지오메트리 변화(리사이즈) 재생성(루프 밖): 스왑체인만 새 extent로 교체.
        // 옛 스왑체인 ComOwned를 대체(Release)해도 visual의 SetContent 참조를 DComp가 유지하므로
        // 다음 완전 Present에서 SetContent(new)까지 옛 콘텐츠가 계속 표시된다.
        // displayed_anchor는 그대로 둔다 — 붙어 있는 옛 콘텐츠(옛 anchor 또는 fallback)의
        // 표시 산식이 유지돼야 하며, content-swap에서 새 anchor로 함께 전환된다.
        for (surface_id, extent) in regen_requests {
            let is_opaque = match self.surfaces.get(&surface_id) {
                Some(e) => e.is_opaque,
                None => continue,
            };
            let size = extent.size();
            let Some(swapchain) = self.create_composition_swapchain(size, is_opaque) else {
                // 재생성 실패 → 이후 regen 영구 중단(옛 콘텐츠 동결 표시). warn 1회.
                warn!(
                    "[dcomp-native] swapchain regen failed; keeping stale swapchain and \
                     disabling further regen"
                );
                self.warned_regen_fail = true;
                continue;
            };
            if let Some(entry) = self.surfaces.get_mut(&surface_id) {
                if let SurfaceStorage::SwapChain(sc) = &mut entry.storage {
                    release_frame_pbuffer(&rc, sc);
                    sc.swapchain = swapchain; // 옛 스왑체인 Release(DComp 참조는 유지)
                    sc.anchor = extent.min;
                    sc.size = size;
                    sc.coverage.reset();
                    sc.content_attached = false; // 다음 완전 Present에서 새 스왑체인으로 SetContent
                    sc.withheld_frames = 0;
                    sc.drawn_this_frame = false;
                    if dcomp_debug() {
                        log::info!(
                            "[dcomp-dbg] regen id={:?} extent={}x{} anchor=({},{})",
                            surface_id, size.width, size.height, extent.min.x, extent.min.y
                        );
                    }
                }
            }
        }

        // 레이어 컬링: 최상위 불투명 클립이 완전히 덮는 하위 visual을 트리에서 제외.
        // 진단: SERVO_DCOMP_NO_CULL로 끌 수 있다(요소 소실 의심 시 즉시 판별).
        let entries: Vec<(DeviceIntRect, bool)> = self
            .frame_surfaces
            .iter()
            .map(|(_, clip, opaque)| (*clip, *opaque))
            .collect();
        let visible = if cull_disabled() {
            vec![true; entries.len()]
        } else {
            cull_covered(&entries)
        };
        if let Some(root) = self.root_visual_ptr() {
            for (i, (id, _, _)) in self.frame_surfaces.iter().enumerate() {
                if !visible[i] {
                    if dcomp_debug() {
                        log::info!("[dcomp-dbg] cull id={:?} (covered by opaque above)", id);
                    }
                    continue;
                }
                let Some(entry) = self.surfaces.get(id) else { continue; };
                // Safety: visual/root 살아있음. 순서 = add_surface 순서(z 아래→위) 유지.
                let hr = unsafe { (*root).AddVisual(entry.visual.as_ptr(), TRUE, ptr::null()) };
                if hr < 0 {
                    warn!("[dcomp-native] AddVisual failed (hr=0x{:08x})", hr as u32);
                }
            }
        }

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
        // SwapChain storage의 frame_pbuffer는 RAII가 아니므로 remove 전에 명시 정리한다.
        let rc = self.rendering_context.clone();
        if let Some(entry) = self.surfaces.get_mut(&id) {
            if let SurfaceStorage::SwapChain(sc) = &mut entry.storage {
                release_frame_pbuffer(&rc, sc);
            }
        }
        // remove → ComOwned Drop이 visual + storage(virtual_surface 또는 swapchain/fallback)를 Release.
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
    fn region_subtract_cases() {
        // 비겹침: 그대로
        assert_eq!(region_subtract(&[r(0,0,10,10)], &[r(20,20,30,30)]), vec![r(0,0,10,10)]);
        // 완전 포함: 공집합
        assert!(region_subtract(&[r(0,0,10,10)], &[r(0,0,10,10)]).is_empty());
        assert!(region_subtract(&[r(2,2,8,8)], &[r(0,0,10,10)]).is_empty());
        // 부분 겹침(우하단 조각): 면적 보존 검증 — 결과 면적 = 10*10 - 5*5
        let out = region_subtract(&[r(0,0,10,10)], &[r(5,5,15,15)]);
        let area: i32 = out.iter().map(|q| (q.max.x-q.min.x)*(q.max.y-q.min.y)).sum();
        assert_eq!(area, 100 - 25);
        // 결과 조각이 서로 겹치지 않고 subtrahend와도 겹치지 않는다
        for (i, a) in out.iter().enumerate() {
            assert!(a.intersection(&r(5,5,15,15)).is_none());
            for b in out.iter().skip(i + 1) {
                assert!(a.intersection(b).is_none());
            }
        }
        // 여러 감수 렉트 순차 차감
        let out = region_subtract(&[r(0,0,100,10)], &[r(10,0,20,10), r(30,0,40,10)]);
        let area: i32 = out.iter().map(|q| (q.max.x-q.min.x)*(q.max.y-q.min.y)).sum();
        assert_eq!(area, 1000 - 100 - 100);
    }

    #[test]
    fn stale_tracker_bookkeeping() {
        let full = r(0, 0, 100, 100);
        let mut st = StaleTracker::default();
        // 첫 전면 Present: 반대 버퍼가 전면 stale
        st.on_present(&[full], full);
        // 이번 프레임 좌반만 갱신 → catch-up = 전면 − 좌반 = 우반
        let catchup = st.catchup_rects(&[r(0,0,50,100)]);
        let area: i32 = catchup.iter().map(|q| (q.max.x-q.min.x)*(q.max.y-q.min.y)).sum();
        assert_eq!(area, 100*100 - 50*100);
        // 그 부분 프레임 Present 후: 반대 버퍼(방금 전면이었던 쪽)의 stale = 좌반
        st.on_present(&[r(0,0,50,100)], full);
        let catchup = st.catchup_rects(&[]);
        let area: i32 = catchup.iter().map(|q| (q.max.x-q.min.x)*(q.max.y-q.min.y)).sum();
        assert_eq!(area, 50*100);
        // 전면 더티 프레임의 catch-up은 공집합 (Global Constraints: 순수 월 0바이트)
        assert!(st.catchup_rects(&[full]).is_empty());
        // 단일 프레임 더티가 32개 초과 → 반대 버퍼 stale이 바운딩 유니온 1개로 붕괴(과대=안전)
        let mut st = StaleTracker::default();
        let many: Vec<DeviceIntRect> = (0..40).map(|i| r(i * 2, 0, i * 2 + 1, 1)).collect();
        st.on_present(&many, full);
        assert_eq!(st.stale[st.cur].len(), 1);
        assert_eq!(st.stale[st.cur][0], r(0, 0, 79, 1)); // 40개 렉트의 바운딩 유니온
        // 붕괴된 stale은 catch-up에서 과대(안전) 복사 대상으로 그대로 나온다
        assert_eq!(st.catchup_rects(&[]), vec![r(0, 0, 79, 1)]);
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

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
use paint_api::VideoFrameLease;
use paint_api::rendering_context::RenderingContext;
use rustc_hash::FxHashMap;
use webrender::api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};
use webrender::api::{ColorF, ExternalImageId, ImageRendering};
use webrender::{
    ClipRadius, Compositor, CompositorCapabilities, CompositorSurfaceTransform, Device,
    NativeSurfaceId, NativeSurfaceInfo, NativeTileId, WindowVisibility,
};
use winapi::Interface;
use winapi::shared::dxgi::{DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, IDXGIAdapter, IDXGIDevice};
use winapi::shared::dxgi1_2::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_PRESENT_PARAMETERS,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, IDXGIFactory2, IDXGISwapChain1,
};
use winapi::shared::dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM;
use winapi::shared::dxgitype::{DXGI_SAMPLE_DESC, DXGI_USAGE_RENDER_TARGET_OUTPUT};
use winapi::shared::minwindef::{FALSE, TRUE};
use winapi::shared::windef::{HWND, POINT, RECT};
use winapi::um::d2dbasetypes::D2D_RECT_F;
use winapi::um::d3d11::{
    D3D11_BOX, D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView,
    ID3D11Resource, ID3D11Texture2D,
};
use winapi::um::d3d11_1::ID3D11DeviceContext1;
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

/// Task 6 defect-2 diagnosis (spec 2026-07-15): env-gated per-tile pixel readback in
/// `unbind` (pbuffer still current). Cached behind a `OnceLock` — read once per unbind
/// while active; zero overhead when the env var is unset (never does glReadPixels, which
/// stalls the pipeline). Frame-limited to <=120 by the caller (spec §7.1 stall amplification).
fn readback_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SERVO_DCOMP_READBACK").is_ok())
}

/// Task 9 defect diagnosis (spec 2026-07-15): env-gated (`SERVO_DCOMP_VALIDPROBE`) per-Virtual-bind
/// coverage log for NON-OPAQUE surfaces only. Logs the raw WR dirty_rect (min AND max), the
/// valid_rect (min AND max), the computed BeginDraw update RECT (virtual coords), the atlas
/// update_offset and the returned origin — one line per bind, frame-bounded by the caller (<=300).
/// Purpose: determine whether WR ever dirties the ticker-bar region below the scrolling text
/// (tile-local y beyond the text sliver) and what valid_rect WR assigns it — the two candidate
/// mechanisms for the never-redrawn strip showing uninitialised virtual-surface memory.
fn valid_probe_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SERVO_DCOMP_VALIDPROBE").is_ok())
}

/// external 비디오 present 파이프라인 프로파일러 게이트(env `SERVO_VIDEO_ESCAPE_PROF`,
/// OnceLock 캐시, dcomp_debug와 별개). 켜지면 렌더러 스레드가 초당 1회 `[vesc-prof]` 집계
/// 라인(info)을 낸다 — AMD 실기에서 어느 단계(acquire/convert/present)가 프레임 예산을 먹는지
/// 판독용. 꺼지면 비용 0(단일 bool 체크 — 타이밍/카운터 자체를 만지지 않음).
fn video_escape_prof() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SERVO_VIDEO_ESCAPE_PROF").is_ok())
}

/// external present 파이프라인의 초당 집계 카운터(video_escape_prof 게이트에서만 갱신).
/// 렌더러 스레드 단일 인스턴스(DCompNativeCompositor 소유). end_frame이 매 프레임 frames++
/// 후 maybe_flush로 1초 경과 시 라인 출력 + 리셋한다. 게이트 off면 전 필드가 0으로 유휴.
struct EscProf {
    last_flush: std::time::Instant,
    frames: u64,
    converts: u64,
    presents: u64,
    srv_creates: u64,
    acquires: u64,
    batch_swaps: u64,
    acquire_dur: std::time::Duration,
    convert_dur: std::time::Duration,
    present_dur: std::time::Duration,
}

impl EscProf {
    fn new() -> Self {
        EscProf {
            last_flush: std::time::Instant::now(),
            frames: 0,
            converts: 0,
            presents: 0,
            srv_creates: 0,
            acquires: 0,
            batch_swaps: 0,
            acquire_dur: std::time::Duration::ZERO,
            convert_dur: std::time::Duration::ZERO,
            present_dur: std::time::Duration::ZERO,
        }
    }

    /// 1초 경과 시 집계 라인을 info로 출력하고 리셋한다. 시간 필드는 창(1초) 동안 누적된
    /// 총 ms(단계별 비교용). 호출측(end_frame)이 video_escape_prof 게이트 안에서만 부른다.
    fn maybe_flush(&mut self) {
        if self.last_flush.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        log::info!(
            "[vesc-prof] frames={} converts={} presents={} srv_creates={} acquires={} \
             acquire_ms={:.1} convert_ms={:.1} present_ms={:.1} batch_swaps={}",
            self.frames,
            self.converts,
            self.presents,
            self.srv_creates,
            self.acquires,
            ms(self.acquire_dur),
            ms(self.convert_dur),
            ms(self.present_dur),
            self.batch_swaps,
        );
        *self = EscProf::new();
    }
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

/// Task 9: 타일의 "가시" 가상 rect = 타일 가상 rect ∩ (서피스 클립을 가상좌표로 환산).
/// 비불투명 valid==dirty 타일에서 BeginDraw 업데이트 렉트를 이 영역으로 확장해, WR가 그리지
/// 않는 [타일 ∩ 클립 − dirty] 밴드까지 투명 클리어를 커밋하기 위한 것이다(bind Virtual arm).
/// 클립이 없거나(add_surface 이전 첫 프레임) 교차가 비면 전체 타일로 폴백한다 — 과대 확장은
/// off-screen 내부 타일 낭비뿐, 정확성엔 안전(합성되지 않는 영역까지 투명으로 커밋할 뿐).
/// scale=1 가정(add_surface와 동일): device 좌표 D → 가상좌표 D − transform_offset + virtual_offset
/// (visual offset = transform_offset − virtual_offset의 역변환). transform_offset은 반올림.
fn tile_visible_virtual_rect(
    tile_rect: DeviceIntRect,
    clip_device: Option<DeviceIntRect>,
    transform_offset: (f32, f32),
    virtual_offset: DeviceIntPoint,
) -> DeviceIntRect {
    let Some(clip) = clip_device else {
        return tile_rect;
    };
    let dx = virtual_offset.x - transform_offset.0.round() as i32;
    let dy = virtual_offset.y - transform_offset.1.round() as i32;
    let clip_v = DeviceIntRect::new(
        DeviceIntPoint::new(clip.min.x + dx, clip.min.y + dy),
        DeviceIntPoint::new(clip.max.x + dx, clip.max.y + dy),
    );
    tile_rect.intersection(&clip_v).unwrap_or(tile_rect)
}

/// Task 10: 불투명 서피스의 visual 클립을 이 서피스가 실제 그린 영역(content_valid_union,
/// device 좌표)으로 좁힌다. 비불투명이거나 유니온 미집계(None)면 원 클립 그대로. 유니온과의
/// 교차가 비면(정상 워크로드에선 발생 안 함) 원 클립으로 폴백해 과소 클립(콘텐츠 소멸)을 피한다.
fn refine_opaque_clip(
    clip_rect: DeviceIntRect,
    is_opaque: bool,
    content_valid_union: Option<DeviceIntRect>,
) -> DeviceIntRect {
    if !is_opaque {
        return clip_rect;
    }
    match content_valid_union {
        Some(u) => clip_rect.intersection(&u).unwrap_or(clip_rect),
        None => clip_rect,
    }
}

/// 부분 Present catch-up용 상수(스펙 §5.2). 힌트 렉트 상한 / stale 목록 붕괴 상한.
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
struct StaleTracker {
    stale: [Vec<DeviceIntRect>; 2],
    cur: usize,
}

impl StaleTracker {
    /// 이번 프레임(더티 frame_dirty)을 Present한 직후 호출.
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
    fn catchup_rects(&self, frame_dirty: &[DeviceIntRect]) -> Vec<DeviceIntRect> {
        region_subtract(&self.stale[self.cur], frame_dirty)
    }

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

/// 진단: 부분 Present만 끄는 스위치(스펙 §3) — 강등 폴백 경로 검증용.
fn partial_present_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var("SERVO_DCOMP_NO_PARTIAL_PRESENT").is_ok())
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
/// 타일 집합이 조밀 사각형이 아니면(구멍) union 내부 무타일 영역이 FLIP_SEQUENTIAL의
/// 미정의 픽셀인 채 Present될 수 있다(현 워크로드에서는 발생하지 않는다는 가정을
/// 여기서 강제한다). 오버플로 방지 위해 i64로 계산.
fn tiles_are_dense(tile_count: usize, tile_size: DeviceIntSize, extent_size: DeviceIntSize) -> bool {
    tile_count as i64 * tile_size.width as i64 * tile_size.height as i64
        == extent_size.width as i64 * extent_size.height as i64
}

/// 스펙 §6.3: 강등 n회째(1부터 시작, `entry.demote_count`를 saturating_add한 이후 값)의
/// 재승격 쿨다운(프레임) = BASE × 2^min(n−1,4), 상한 CAP. n=0은 saturating_add 이후
/// 정상 경로로는 도달하지 않지만(항상 ≥1) 방어적으로 시프트 0(BASE)으로 처리한다.
fn demote_cooldown(demote_count: u32) -> u64 {
    let shift = demote_count.saturating_sub(1).min(4);
    (DEMOTE_COOLDOWN_BASE << shift).min(DEMOTE_COOLDOWN_CAP)
}

/// 보강 항목 1: 렉트 벡터가 상한을 넘으면 바운딩 유니온 1개로 붕괴(과대=안전).
/// 전면 Present 실패로 인한 frame_dirty 복원이 지속 실패 하에서 무한정 커지는 것을
/// 막는다(StaleTracker::on_present와 동일 원칙 — 근사 아님, 손상 없음).
fn collapse_dirty_if_oversized(mut rects: Vec<DeviceIntRect>, limit: usize) -> Vec<DeviceIntRect> {
    if rects.len() <= limit {
        return rects;
    }
    let union = rects
        .drain(..)
        .fold(None::<DeviceIntRect>, |acc, r| Some(acc.map_or(r, |a| a.union(&r))));
    union.into_iter().collect()
}

/// 소유한 COM 포인터의 RAII 래퍼(Drop에서 Release). Send/Sync 아님 —
/// 렌더러 스레드 전용(WR Compositor 계약과 일치). Task 4(dcomp_video_convert)가
/// 재사용하므로 `pub(crate)`(가시성만 승격 — 정의 위치와 Drop 구현은 이동하지 않는다).
pub(crate) struct ComOwned<T>(ptr::NonNull<T>);

impl<T> ComOwned<T> {
    /// Safety: `ptr`은 소유권이 이전되는 유효한 COM 인터페이스 포인터여야 한다.
    pub(crate) unsafe fn from_raw(raw: *mut T) -> Option<Self> {
        ptr::NonNull::new(raw).map(Self)
    }

    pub(crate) fn as_ptr(&self) -> *mut T {
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
    /// 이번 프레임 bind가 쌓은 버퍼-로컬 더티 렉트(Present1 힌트 + stale 부기 재료).
    frame_dirty: Vec<DeviceIntRect>,
    /// 버퍼별 stale 부기(스펙 §5.2). content_attached 후 부분 Present에 사용.
    stale: StaleTracker,
    /// 이 스왑체인에서 부분 Present 사용 가능(GetBuffer(1) 프로브 성공 + env 미차단).
    partial_present: bool,
    /// 스펙 §8/보강 항목 2: 전면 Present(Present(0,0)) 실패 warn을 이 스왑체인 인스턴스당
    /// 1회로 제한한다. 지속 실패(예: 디바이스 로스트) 시 coverage가 계속 "전면"으로 남아
    /// 이 분기가 매 프레임 재진입되므로, 무가드면 기존에 기록된 로그 폭주 함정이 재발한다.
    /// 강등 성공(storage 교체) 또는 재승격(새 인스턴스)에서 자연 리셋된다.
    warned_present_fail: bool,
    /// 보강 항목 2: 강등 시딩(§6.2) 실패 warn을 이 스왑체인 인스턴스당 1회로 제한한다.
    /// 시딩이 지속 실패하면(예: 디바이스 로스트) 강등 자체가 매 프레임 재시도되므로
    /// 무가드면 동일하게 로그 폭주한다.
    warned_demote_seed_fail: bool,
    /// 최종 리뷰 Important #1: "제3상태"(regen 후 pre-attach인데 fallback_virtual이
    /// None — regen은 fallback을 복원하지 않는다, 첫 content-swap에서 이미 소모됨)를
    /// 만나면 시딩이 구조적으로 불가능하다. 단발 경고 후 이 스왑체인의 강등 처리
    /// 자체를 억제해 매 프레임 재시도(=warn 폭주 은폐된 재시도 루프)를 막는다.
    /// content-swap 성공(content_attached=true) 또는 regen에서 리셋 — 그 시점에
    /// 상태가 바뀌어 재판정이 의미 있어지기 때문("자연 회복" 조건).
    demote_blocked: bool,
    /// 최종 리뷰 Minor #4: content-swap 시 SetContent(swapchain) 실패 warn을 이
    /// 스왑체인 인스턴스당 1회로 제한한다. 실패해도 content_attached는 그대로
    /// false라 이 분기가 coverage 전면인 매 프레임 재진입되므로, 무가드면 다른
    /// warned_* 필드들과 동일한 로그 폭주가 재발한다. regen에서 재무장.
    warned_setcontent_fail: bool,
}

/// content-swap 시 1회: GetBuffer(1)이 이 환경에서 열리는지 프로브(스펙 §3 '런타임 자격').
fn probe_partial_present(swapchain: &ComOwned<IDXGISwapChain1>) -> bool {
    // Safety: 살아있는 스왑체인. 성공 시 AddRef된 텍스처 즉시 Release.
    unsafe {
        let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
        let hr = (*swapchain.as_ptr()).GetBuffer(
            1,
            &ID3D11Texture2D::uuidof(),
            &mut tex as *mut _ as *mut _,
        );
        if hr < 0 || tex.is_null() {
            warn!("[dcomp-native] GetBuffer(1) probe failed (hr=0x{:08x}); partial present off", hr as u32);
            return false;
        }
        (*(tex as *mut IUnknown)).Release();
        true
    }
}

/// catch-up 복사: GetBuffer(1)→frame_pbuffer.texture(=GetBuffer(0)) 렉트들.
/// 전면 더티 프레임이면 rects가 공집합 → 복사 0(Global Constraints).
fn self_copy_catchup(
    ctx: &Option<ComOwned<ID3D11DeviceContext>>,
    sc: &SwapChainStorage,
    rects: &[DeviceIntRect],
) -> bool {
    if rects.is_empty() {
        return true;
    }
    let Some(ctx) = ctx.as_ref() else { return false; };
    let Some(fp) = sc.frame_pbuffer.as_ref() else { return false; };
    // Safety: 살아있는 스왑체인/컨텍스트. src는 AddRef → 사용 후 Release.
    unsafe {
        let mut src: *mut ID3D11Texture2D = ptr::null_mut();
        let hr = (*sc.swapchain.as_ptr()).GetBuffer(
            1,
            &ID3D11Texture2D::uuidof(),
            &mut src as *mut _ as *mut _,
        );
        if hr < 0 || src.is_null() {
            warn!("[dcomp-native] GetBuffer(1) failed at copy (hr=0x{:08x})", hr as u32);
            return false;
        }
        for rc in rects {
            // 버퍼 경계로 클램프(stale 바운딩 붕괴가 경계 밖을 물 수 있음).
            let x0 = rc.min.x.max(0);
            let y0 = rc.min.y.max(0);
            let x1 = rc.max.x.min(sc.size.width);
            let y1 = rc.max.y.min(sc.size.height);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let src_box = D3D11_BOX {
                left: x0 as u32, top: y0 as u32, front: 0,
                right: x1 as u32, bottom: y1 as u32, back: 1,
            };
            (*ctx.as_ptr()).CopySubresourceRegion(
                fp.texture as *mut _, 0, x0 as u32, y0 as u32, 0,
                src as *mut _, 0, &src_box,
            );
        }
        (*(src as *mut IUnknown)).Release();
        true
    }
}

/// Present1 + DirtyRects 힌트(스펙 §5.2-3). 16 초과·공집합이면 힌트 없이 전체.
fn present1_partial(sc: &SwapChainStorage, dirty: &[DeviceIntRect]) -> bool {
    // 클램프 먼저, 그 결과가 퇴화(폭/높이 0 이하)면 스킵 — self_copy_catchup과 동일 순서.
    // (완전히 버퍼 밖인 렉트가 클램프 후 퇴화 RECT로 Present1에 전달되는 것을 방지.)
    let rects: Vec<RECT> = dirty
        .iter()
        .filter_map(|r| {
            let left = r.min.x.max(0);
            let top = r.min.y.max(0);
            let right = r.max.x.min(sc.size.width);
            let bottom = r.max.y.min(sc.size.height);
            if right <= left || bottom <= top {
                return None;
            }
            Some(RECT { left, top, right, bottom })
        })
        .collect();
    let use_hint = !rects.is_empty() && rects.len() <= MAX_PRESENT_DIRTY_RECTS;
    let params = DXGI_PRESENT_PARAMETERS {
        DirtyRectsCount: if use_hint { rects.len() as u32 } else { 0 },
        pDirtyRects: if use_hint { rects.as_ptr() as *mut RECT } else { ptr::null_mut() },
        pScrollRect: ptr::null_mut(),
        pScrollOffset: ptr::null_mut(),
    };
    // Safety: 살아있는 스왑체인. SyncInterval 0 = 기존 Present와 동일 페이싱.
    let hr = unsafe { (*sc.swapchain.as_ptr()).Present1(0, 0, &params) };
    if hr < 0 {
        warn!("[dcomp-native] Present1 failed (hr=0x{:08x})", hr as u32);
        return false;
    }
    true
}

/// §6.2-1: 승격 후 스왑체인 buffer 0에 그려진 적 있는 영역을 fallback_virtual로 복사해
/// 최신화한다(첫 콘텐츠 Present 전 강등 — visual은 계속 fallback을 표시하므로 끊김 없음).
/// 시딩 대상 타일 = coverage.covered_tiles(전면 갱신 누적) ∪ frame_dirty(전면 Present
/// 실패 복원분)가 겹치는 타일 — 둘 다 `tiles`(entry의 현재 유효 타일)로 필터링한다.
/// 타일 단위 과대 복사는 안전(그 안의 미갱신 픽셀도 함께 복사될 뿐 — 승격 후 한 번도
/// 그려진 적 없는 타일은 애초에 이 목록에 없다). 반환: 실패 시 fallback을 sc에 되돌리고
/// None(스왑체인 유지) — 어느 경우든 EndDraw는 매 BeginDraw 성공 이후 반드시 호출한다.
fn demote_seed_into_fallback(
    ctx: *mut ID3D11DeviceContext,
    sc: &mut SwapChainStorage,
    tiles: &std::collections::HashSet<(i32, i32)>,
    virtual_offset: DeviceIntPoint,
    tile_size: DeviceIntSize,
) -> Option<ComOwned<IDCompositionVirtualSurface>> {
    let fallback = sc.fallback_virtual.take()?;

    let mut seed_tiles: std::collections::HashSet<(i32, i32)> = sc
        .coverage
        .covered_tiles
        .iter()
        .copied()
        .filter(|t| tiles.contains(t))
        .collect();
    for buf_rect in &sc.frame_dirty {
        // 버퍼-로컬(가상 − anchor) → 가상좌표로 되돌려 겹치는 타일을 시딩 대상에 합류.
        //
        // 불변량(리뷰 Minor #2): 이 "겹치는 타일 전체" 복사가 안전한 것은 현재의 모든
        // 타입-1 도달 경로에서 frame_dirty가 (a) 빈 상태(withhold 분기가 push 전에
        // clear)이거나 (b) 전면 커버(전면 Present 실패 복원 = coverage.is_full 프레임의
        // 더티)이기 때문이다 — 즉 frame_dirty가 걸치는 타일은 buffer 0에 그 타일 전체가
        // 기록돼 있다. 부분 frame_dirty를 가진 신규 타입-1 트리거를 추가할 경우 이
        // 전체-타일 복사는 buffer 0의 미기록 영역(쓰레기)을 fallback에 시딩할 수 있다 —
        // 그때는 복사 범위를 frame_dirty 교집합으로 제한하거나 이 불변량을 유지할 것.
        let virt = DeviceIntRect::new(
            DeviceIntPoint::new(buf_rect.min.x + sc.anchor.x, buf_rect.min.y + sc.anchor.y),
            DeviceIntPoint::new(buf_rect.max.x + sc.anchor.x, buf_rect.max.y + sc.anchor.y),
        );
        for &t in tiles {
            if seed_tiles.contains(&t) {
                continue;
            }
            let tr = tile_virtual_rect(virtual_offset, tile_size, t.0, t.1);
            if tr.intersection(&virt).is_some() {
                seed_tiles.insert(t);
            }
        }
    }

    if seed_tiles.is_empty() {
        // 승격 후 전면 타일이 한 번도 기록되지 않았고 실패-복원 잔여도 없음 → fallback이
        // 이미 최신(승격 시점 콘텐츠 그대로) — 복사 없이 성공 처리.
        return Some(fallback);
    }

    // buffer 0(승격 이후 Present가 한 번도 성공하지 않아 로테이션 0 = 모든 부분 draw 누적).
    let buffer0 = unsafe {
        let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
        let hr = (*sc.swapchain.as_ptr()).GetBuffer(
            0,
            &ID3D11Texture2D::uuidof(),
            &mut tex as *mut _ as *mut _,
        );
        if hr < 0 || tex.is_null() {
            warn!("[dcomp-native] demote fallback seed: GetBuffer(0) failed (hr=0x{:08x})", hr as u32);
            sc.fallback_virtual = Some(fallback);
            return None;
        }
        tex
    };

    let vsurf = fallback.as_ptr();
    let mut ok = true;
    for &(tx, ty) in &seed_tiles {
        let tile_rect = tile_virtual_rect(virtual_offset, tile_size, tx, ty);
        // BeginDraw는 가상공간 절대좌표 RECT(기존 bind Virtual arm과 동일 규약).
        let update = RECT {
            left: tile_rect.min.x,
            top: tile_rect.min.y,
            right: tile_rect.max.x,
            bottom: tile_rect.max.y,
        };
        // Safety: fallback은 살아있는 IDCompositionVirtualSurface(위에서 take한 소유물).
        let (dst_tex, update_offset) = unsafe {
            let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
            let mut off = POINT { x: 0, y: 0 };
            let hr = (*vsurf).BeginDraw(&update, &ID3D11Texture2D::uuidof(), &mut tex as *mut _ as *mut _, &mut off);
            if hr < 0 || tex.is_null() {
                warn!("[dcomp-native] demote fallback seed: BeginDraw failed (hr=0x{:08x})", hr as u32);
                ok = false;
                break;
            }
            (tex, off)
        };
        // 소스 박스 = 버퍼-로컬(가상 − anchor). 타일은 항상 스왑체인 extent(=coverage 판정
        // 근거인 entry.tiles의 union) 내부이므로 클램프 없이 그대로 사용한다.
        let src_box = D3D11_BOX {
            left: (tile_rect.min.x - sc.anchor.x) as u32,
            top: (tile_rect.min.y - sc.anchor.y) as u32,
            front: 0,
            right: (tile_rect.max.x - sc.anchor.x) as u32,
            bottom: (tile_rect.max.y - sc.anchor.y) as u32,
            back: 1,
        };
        // Safety: ctx/buffer0/dst_tex 모두 살아있음. dst_tex는 BeginDraw가 AddRef했으므로
        // 사용 후 Release. target rect == BeginDraw rect라 dest 좌표는 update_offset 그대로
        // (target − BeginDraw origin = 0).
        unsafe {
            (*ctx).CopySubresourceRegion(
                dst_tex as *mut _, 0, update_offset.x as u32, update_offset.y as u32, 0,
                buffer0 as *mut _, 0, &src_box,
            );
            (*(dst_tex as *mut IUnknown)).Release();
            let hr = (*vsurf).EndDraw();
            if hr < 0 {
                warn!("[dcomp-native] demote fallback seed: EndDraw failed (hr=0x{:08x})", hr as u32);
                ok = false;
            }
        }
        if !ok {
            break;
        }
    }
    // Safety: GetBuffer(0)가 AddRef한 버퍼를 한 번 반납.
    unsafe {
        (*(buffer0 as *mut IUnknown)).Release();
    }

    if !ok {
        sc.fallback_virtual = Some(fallback);
        return None;
    }
    Some(fallback)
}

/// §6.2-2: 새 가상 서피스를 만들어 buffer 0 전체를 복사하고, visual.SetContent(virtual) +
/// 오프셋/클립 재적용까지 같은 Commit에서 원자적으로 전환한다(content-swap과 동일 정본).
/// 반환: 실패 시 각 단계에서 확보한 자원을 정리(ComOwned Drop) 후 None(스왑체인 유지).
fn demote_seed_new_virtual(
    dcomp_device: *mut IDCompositionDevice,
    ctx: *mut ID3D11DeviceContext,
    sc: &SwapChainStorage,
    visual: &ComOwned<IDCompositionVisual>,
    last_placement: Option<LastPlacement>,
    is_opaque: bool,
    virtual_offset: DeviceIntPoint,
) -> Option<ComOwned<IDCompositionVirtualSurface>> {
    let alpha_mode = if is_opaque {
        DXGI_ALPHA_MODE_IGNORE
    } else {
        DXGI_ALPHA_MODE_PREMULTIPLIED
    };
    // Safety: dcomp_device는 살아있는 IDCompositionDevice(호출자가 확보).
    let vsurf = unsafe {
        let mut raw: *mut IDCompositionVirtualSurface = ptr::null_mut();
        let hr = (*dcomp_device).CreateVirtualSurface(
            VIRTUAL_SURFACE_SIZE as u32,
            VIRTUAL_SURFACE_SIZE as u32,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            alpha_mode,
            &mut raw,
        );
        if hr < 0 || raw.is_null() {
            warn!("[dcomp-native] demote new-virtual seed: CreateVirtualSurface failed (hr=0x{:08x})", hr as u32);
            return None;
        }
        ComOwned::from_raw(raw)?
    };

    // buffer 0의 완전성은 강등 트리거별로 다르다(리뷰 Important #1):
    //  - withhold 임계·Present1 실패·전면 Present 실패 경로: 마지막 성공 Present 이후
    //    버퍼 미로테이트 — buffer 0 = 마지막 전면 상태 + 이후 누적 부분 draw = 완전.
    //  - catch-up 복사 실패 후 즉시 강등 경로만 예외: 직전 Present1로 로테이트된
    //    buffer 0의 미catch-up 영역(직전 프레임 더티)이 1프레임 구본이다. 스펙 §6.2
    //    수용 사항("직전 프레임 더티 영역이 1프레임 구본으로 시딩될 수 있음") — 해당
    //    영역은 직전에 갱신되던 콘텐츠라 재갱신 시 자가 치유되는 알려진 미세 한계.
    let buffer0 = unsafe {
        let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
        let hr = (*sc.swapchain.as_ptr()).GetBuffer(
            0,
            &ID3D11Texture2D::uuidof(),
            &mut tex as *mut _ as *mut _,
        );
        if hr < 0 || tex.is_null() {
            warn!("[dcomp-native] demote new-virtual seed: GetBuffer(0) failed (hr=0x{:08x})", hr as u32);
            return None; // vsurf(ComOwned) Drop이 Release
        }
        tex
    };

    // BeginDraw는 가상공간 절대좌표: 스왑체인 extent(anchor..anchor+size)를 그대로 이식.
    let update = RECT {
        left: sc.anchor.x,
        top: sc.anchor.y,
        right: sc.anchor.x + sc.size.width,
        bottom: sc.anchor.y + sc.size.height,
    };
    // Safety: vsurf/ctx/buffer0 모두 살아있음.
    let drew = unsafe {
        let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
        let mut off = POINT { x: 0, y: 0 };
        let hr = (*vsurf.as_ptr()).BeginDraw(&update, &ID3D11Texture2D::uuidof(), &mut tex as *mut _ as *mut _, &mut off);
        if hr < 0 || tex.is_null() {
            warn!("[dcomp-native] demote new-virtual seed: BeginDraw failed (hr=0x{:08x})", hr as u32);
            false
        } else {
            let src_box = D3D11_BOX {
                left: 0, top: 0, front: 0,
                right: sc.size.width as u32, bottom: sc.size.height as u32, back: 1,
            };
            (*ctx).CopySubresourceRegion(
                tex as *mut _, 0, off.x as u32, off.y as u32, 0,
                buffer0 as *mut _, 0, &src_box,
            );
            (*(tex as *mut IUnknown)).Release();
            let hr = (*vsurf.as_ptr()).EndDraw();
            if hr < 0 {
                warn!("[dcomp-native] demote new-virtual seed: EndDraw failed (hr=0x{:08x})", hr as u32);
                false
            } else {
                true
            }
        }
    };
    // Safety: GetBuffer(0)가 AddRef한 버퍼를 한 번 반납.
    unsafe {
        (*(buffer0 as *mut IUnknown)).Release();
    }
    if !drew {
        return None; // vsurf(ComOwned) Drop이 Release
    }

    // SetContent + 오프셋/클립 재적용(같은 Commit에서 원자 전환 — content-swap :1370-1393 정본).
    // Safety: visual/vsurf 살아있음.
    let hr = unsafe { (*visual.as_ptr()).SetContent(vsurf.as_ptr() as *const IUnknown) };
    if hr < 0 {
        warn!("[dcomp-native] demote new-virtual seed: SetContent failed (hr=0x{:08x})", hr as u32);
        return None;
    }
    if let Some(placement) = last_placement {
        // 강등 후 표시는 가상좌표 콘텐츠(content_anchor=zero) — Virtual arm과 동일 산식.
        apply_visual_placement(visual, placement, virtual_offset, DeviceIntPoint::zero());
    }
    Some(vsurf)
}

/// 서피스 콘텐츠의 백엔드. 기본은 `Virtual`(가상 서피스), 연속 전면 갱신 서피스는
/// end_frame에서 `SwapChain`(flip 스왑체인)으로 승격된다.
enum SurfaceStorage {
    Virtual {
        virtual_surface: ComOwned<IDCompositionVirtualSurface>,
    },
    SwapChain(SwapChainStorage),
    /// 비디오 external compositor surface(비디오 WR 탈출, Task 5). WR가 이 서피스에
    /// 타일을 그리지 않는다 — 대신 add_surface에서 provider 링을 대여해 raw D3D11
    /// 변환 1-draw로 비디오별 flip 스왑체인 백버퍼를 직접 채우고 Present한다.
    External(ExternalStorage),
}

/// External compositor surface의 저장소. 스왑체인은 클립 크기 확정 시 지연 생성하며,
/// 첫 성공 Present 후에만 visual.SetContent(스왑체인)한다(그 전까지 visual은 콘텐츠
/// 없음). last_presented로 (ring_id, frame_seq) 세대 dedup — 같은 프레임 재변환 스킵.
struct ExternalStorage {
    /// 크기 확정 시 지연 생성되는 비디오별 flip 스왑체인. None = 미생성/생성 실패.
    swapchain: Option<ComOwned<IDXGISwapChain1>>,
    /// 현재 스왑체인의 백버퍼 크기(= 마지막 생성 시 clip.size()). 클립이 바뀌면 재생성.
    swapchain_size: DeviceIntSize,
    /// 첫 성공 Present 후 visual.SetContent(스왑체인)를 완료했는가(1회).
    content_attached: bool,
    /// attach_external_image가 기록한 이번 서피스의 ExternalImageId(.0). provider.acquire 인자.
    attached_external_id: Option<u64>,
    /// 마지막으로 Present한 (ring_id, frame_seq). external_needs_present 세대 dedup 근거.
    last_presented: Option<(u64, u64)>,
    /// 스왑체인 생성/GetBuffer/RTV/Present/SetContent 실패 warn-once(로그 폭주 방지).
    /// 성공적인 스왑체인 재생성 시 재무장(false).
    warned_fail: bool,
    /// 브링업 계약 로그(서피스당 최초 5프레임)용 카운터(진단 전용, dcomp_debug 게이트).
    frames_logged: u32,
    /// flip 스왑체인 백버퍼별 RTV 캐시(키 = GetBuffer(0)가 준 백버퍼 텍스처 포인터 usize).
    /// FLIP_SEQUENTIAL 2버퍼가 번갈아 반환되므로 엔트리 ≤2. 매 present GetBuffer(0)→조회/생성으로
    /// per-present CreateRenderTargetView(GCN1 등 구형 AMD에서 프레임 예산 비용)를 제거한다.
    ///
    /// ★SAFETY: 스왑체인 백버퍼는 CPU-Map(WRITE_DISCARD) 대상이 절대 아니므로(DYNAMIC이 아님)
    /// rename이 없다 — 캐시된 RTV가 무효화되지 않는다. 이는 DYNAMIC plane 텍스처 SRV 캐시가
    /// 금지인 것(rename→dangling SRV→드라이버 AV, 크래시 061c7f5d0/재발방지 원칙; 위
    /// VideoConvertPass 주석)과 정반대 상황이라 안전하다. 스왑체인 (재)생성/파기 시 clear해
    /// 옛(해제된) 버퍼 포인터의 스테일 RTV 재사용을 막는다.
    rtv_cache: FxHashMap<usize, ComOwned<ID3D11RenderTargetView>>,
}

/// external surface의 이번 프레임 lease가 지난 Present와 다른 세대인가(재변환/재Present
/// 필요 판정). ring_id 변화 = 소스 전환(다른 비디오), frame_seq 변화 = 새 프레임.
/// 순수 함수 — TDD 대상(tests::external_needs_present_dedups_by_ring_and_seq).
fn external_needs_present(last: Option<(u64, u64)>, ring_id: u64, seq: u64) -> bool {
    last != Some((ring_id, seq))
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
    /// Task 10: 이 서피스가 실제로 bind한 타일 valid 영역(device 좌표)의 바운딩 유니온(누적).
    /// 불투명 phantom 서피스의 그려지지 않은 영역 합성을 억제하기 위한 clip 좁힘 근거
    /// (add_surface의 refine_opaque_clip). None=아직 미집계. destroy_tile(지오메트리 변경)에서 리셋.
    content_valid_union: Option<DeviceIntRect>,
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
    /// Task 6 diagnosis only (cheap ints, always populated). Tile grid coords, the atlas
    /// offset BeginDraw returned, the dirty-rect size, and the returned texture size — the
    /// coordinates the readback needs to sample the drawn sub-region. Unused when
    /// SERVO_DCOMP_READBACK is off (readback_log_bound is called only under the gate).
    tile: (i32, i32),
    update_offset: (i32, i32),
    dirty_size: (i32, i32),
    tex_size: (i32, i32),
}

/// Task 6 defect-2 diagnosis: read the just-drawn tile pixels from the pbuffer (still
/// current at `unbind`, before EndDraw) and log per-tile alpha/RGB statistics. glReadPixels
/// stalls the pipeline (spec §7.1) so the caller gates this on `readback_enabled()` AND
/// `frame_counter <= 120`. One whole-texture read per bound tile, then two sample passes:
///  (a) an 8x8 grid over the drawn dirty sub-region [update_offset .. +dirty_size], reported
///      as alpha_min/max + rgb_nonzero/64 — the primary "is the video/text black?" signal;
///  (b) a coarse 32x32 scan over the ENTIRE returned texture — cancels flip/atlas ambiguity
///      (if content exists anywhere in the allocation the whole-scan catches it even if the
///      sub-region indexing is mirrored or the tile sits at a non-zero atlas offset).
/// GL error is sampled before/after the read for the decision tree (draw-layer GL failure).
fn readback_log_bound(device: &Device, bound: &BoundTile, is_opaque: Option<bool>) {
    use gleam::gl;
    let (tw, th) = bound.tex_size;
    if tw <= 0 || th <= 0 {
        return;
    }
    let g = device.gl();
    let err_before = g.get_error();
    // Read the pbuffer's default framebuffer (bind returned fbo_id 0 => WR drew the tile
    // there). Bind READ_FRAMEBUFFER 0 explicitly so a leftover WR FBO can't be sampled.
    g.bind_framebuffer(gl::READ_FRAMEBUFFER, 0);
    let pixels = g.read_pixels(0, 0, tw, th, gl::RGBA, gl::UNSIGNED_BYTE);
    let err_after = g.get_error();
    let need = (tw as usize) * (th as usize) * 4;
    if pixels.len() < need {
        warn!(
            "[dcomp-readback] short read {} < {}x{}x4 for surface {:?}",
            pixels.len(), tw, th, bound.surface_id
        );
        return;
    }
    let sample = |sx: i32, sy: i32| -> (u8, u8, u8, u8) {
        let x = sx.clamp(0, tw - 1) as usize;
        let y = sy.clamp(0, th - 1) as usize;
        let idx = (y * tw as usize + x) * 4;
        (pixels[idx], pixels[idx + 1], pixels[idx + 2], pixels[idx + 3])
    };
    // (a) 8x8 grid over the drawn dirty sub-region.
    let (ox, oy) = bound.update_offset;
    let dw = bound.dirty_size.0.max(1);
    let dh = bound.dirty_size.1.max(1);
    let (mut a_min, mut a_max, mut rgb_nz) = (255u8, 0u8, 0u32);
    for j in 0..8 {
        for i in 0..8 {
            let (r, gc, b, a) = sample(ox + (i * dw) / 8, oy + (j * dh) / 8);
            a_min = a_min.min(a);
            a_max = a_max.max(a);
            if r != 0 || gc != 0 || b != 0 {
                rgb_nz += 1;
            }
        }
    }
    // (b) coarse 32x32 whole-texture scan (flip/atlas ambiguity guard).
    const SCAN: i32 = 32;
    let (mut whole_nz, mut whole_a_max) = (0u32, 0u8);
    for j in 0..SCAN {
        for i in 0..SCAN {
            let (r, gc, b, a) = sample((i * (tw - 1)) / (SCAN - 1), (j * (th - 1)) / (SCAN - 1));
            whole_a_max = whole_a_max.max(a);
            if r != 0 || gc != 0 || b != 0 {
                whole_nz += 1;
            }
        }
    }
    log::info!(
        "[dcomp-readback] surface={:?} tile=({},{}) opaque={:?} tex={}x{} update_off=({},{}) \
         dirty={}x{} alpha_min={} alpha_max={} rgb_nonzero={}/64 whole_rgb_nonzero={}/{} \
         whole_alpha_max={} gl_err=0x{:04x}->0x{:04x}",
        bound.surface_id, bound.tile.0, bound.tile.1, is_opaque, tw, th, ox, oy,
        bound.dirty_size.0, bound.dirty_size.1, a_min, a_max, rgb_nz, whole_nz,
        SCAN * SCAN, whole_a_max, err_before, err_after
    );
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
    /// 이번 프레임 add_surface가 기록한 서피스 id — z-order대로 누적(add_surface 호출 순 =
    /// 아래→위). AddVisual은 end_frame에서 이 순서 그대로 일괄 수행한다(컬링 없음 — A-1,
    /// 아래 end_frame 주석 참조). begin_frame에서 clear.
    frame_surfaces: Vec<NativeSurfaceId>,
    /// ANGLE D3D11 디바이스(비소유 — rendering_context가 수명 보유). 스왑체인 생성에 사용.
    d3d11_device: *mut ID3D11Device,
    /// d3d11_device의 즉시 컨텍스트(AddRef 소유). 부분 Present catch-up 복사
    /// (CopySubresourceRegion)에 사용 — GetImmediateContext 실패는 없다고 간주되지만
    /// (COM 계약상 실패하지 않음) 방어적으로 Option.
    d3d11_context: Option<ComOwned<ID3D11DeviceContext>>,
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
    /// 비디오 external surface 변환 패스(Task 4). 최초 external present 시 지연 생성 —
    /// 비-비디오 월은 셰이더 컴파일 비용을 아예 안 낸다. None + init_failed=false = 미시도.
    convert_pass: Option<crate::dcomp_video_convert::VideoConvertPass>,
    /// convert_pass 지연 생성이 실패했으면 재시도하지 않는다(new()가 이미 warn — 매 프레임
    /// 재컴파일/재warn 스팸 방지). "warn-once 후 skip"(브리프 Step 4)의 구현 기제.
    convert_pass_init_failed: bool,
    /// convert()에 넘길 ID3D11DeviceContext1(즉시 컨텍스트 QI, AddRef 소유). maybe_create에서
    /// 확보 실패 시 None — external present 스킵(비주얼만).
    d3d11_context1: Option<ComOwned<ID3D11DeviceContext1>>,
    /// external surface 요청 시 provider 미등록 warn-once(비주얼만 배치, 마지막 프레임 유지).
    warned_no_provider: bool,
    /// 이번 프레임에 external convert 배치가 열려 있는가. present_external의 첫 external convert에서
    /// convert_pass.begin_batch로 열고(1회), start_compositing에서 close_external_batch로 닫는다
    /// (WR passes-루프 타일 GL 전 — 아래 close_external_batch/start_compositing 주석의 렌더러 앵커
    /// 참조). begin_frame/end_frame이 방어적으로 닫아 잔류를 막는다.
    external_batch_active: bool,
    /// external present 파이프라인 초당 프로파일러 누산기(video_escape_prof 게이트에서만 갱신).
    esc_prof: EscProf,
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
        // 부분 Present catch-up 복사(CopySubresourceRegion)용 즉시 컨텍스트.
        // GetImmediateContext는 HRESULT가 없고(COM 계약상 항상 성공) AddRef된 포인터를 돌려준다.
        let d3d11_context = {
            let mut ctx_raw: *mut ID3D11DeviceContext = ptr::null_mut();
            (*d3d).GetImmediateContext(&mut ctx_raw);
            ComOwned::from_raw(ctx_raw)
        };

        // external surface 변환 패스(Task 4)가 요구하는 ID3D11DeviceContext1. 즉시 컨텍스트를
        // QI한다 — 실패해도 컴포지터는 성립(external 경로만 불가 → 비주얼만). None 허용.
        let d3d11_context1 = d3d11_context.as_ref().and_then(|ctx| {
            let mut raw: *mut ID3D11DeviceContext1 = ptr::null_mut();
            let hr = (*ctx.as_ptr())
                .QueryInterface(&ID3D11DeviceContext1::uuidof(), &mut raw as *mut _ as *mut _);
            if hr < 0 || raw.is_null() {
                warn!(
                    "[dcomp-native] QI ID3D11DeviceContext1 failed (hr=0x{:08x}); video escape unavailable",
                    hr as u32
                );
                None
            } else {
                ComOwned::from_raw(raw)
            }
        });

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
            d3d11_context,
            dxgi_factory,
            warned_scale: false,
            warned_rounded_clip: false,
            warned_external_surface: false,
            warned_enable_native: false,
            warned_promote_fail: false,
            warned_regen_fail: false,
            frame_counter: 0,
            convert_pass: None,
            convert_pass_init_failed: false,
            d3d11_context1,
            warned_no_provider: false,
            external_batch_active: false,
            esc_prof: EscProf::new(),
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
    /// FLIP_SEQUENTIAL + BufferCount 2: FLIP_DISCARD와 달리 Present 후에도 버퍼 콘텐츠가
    /// 보존된다 — 부분 Present의 catch-up 복사(GetBuffer(1)에서 복사)가 이를 전제한다
    /// (스펙 §5.1-1; PoC G4가 정확 2버퍼 핑퐁 로테이션을 실기 확인).
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
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
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

    /// 백드롭 서피스 요청에 대한 warn-once(external은 Task 5에서 실구현 — 아래 별도 경로).
    /// Servo는 backdrop을 요청하지 않으므로 이 경로는 도달하지 않는다(grep 확정).
    fn warn_external_surface_once(&mut self) {
        if !self.warned_external_surface {
            warn!(
                "[dcomp-native] backdrop compositor surface requested but not \
                 implemented (unreachable in Servo); ignoring"
            );
            self.warned_external_surface = true;
        }
    }

    /// external surface(비디오)의 비주얼 배치. Virtual/SwapChain의 apply_visual_placement와
    /// 산식이 다르다: 스왑체인 백버퍼는 이미 clip 크기로 비디오를 담고 있으므로 비주얼
    /// 오프셋 = clip.min(device 좌표), 클립 = 비주얼-로컬 (0,0)-(w,h). scale은 무시한다
    /// (dest=clip에 스케일이 이미 반영 — 브리프 계약). provider 유무와 무관하게 매 프레임
    /// 호출해 마지막 프레임을 유지한다.
    fn place_external_visual(&self, id: NativeSurfaceId, clip_rect: DeviceIntRect) {
        let Some(entry) = self.surfaces.get(&id) else {
            return;
        };
        let offset_x = clip_rect.min.x as f32;
        let offset_y = clip_rect.min.y as f32;
        // 비주얼-로컬 클립(SetClip은 오프셋 적용 전 좌표) = clip − offset = (0,0)-(w,h).
        let local_clip = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: (clip_rect.max.x - clip_rect.min.x) as f32,
            bottom: (clip_rect.max.y - clip_rect.min.y) as f32,
        };
        // Safety: visual은 ComOwned가 수명을 보장하는 살아있는 IDCompositionVisual.
        unsafe {
            let hr = (*entry.visual.as_ptr()).SetOffsetX_1(offset_x);
            if hr < 0 {
                warn!("[dcomp-native] external SetOffsetX failed (hr=0x{:08x})", hr as u32);
            }
            let hr = (*entry.visual.as_ptr()).SetOffsetY_1(offset_y);
            if hr < 0 {
                warn!("[dcomp-native] external SetOffsetY failed (hr=0x{:08x})", hr as u32);
            }
            let hr = (*entry.visual.as_ptr()).SetClip_1(&local_clip);
            if hr < 0 {
                warn!("[dcomp-native] external SetClip failed (hr=0x{:08x})", hr as u32);
            }
        }
    }

    /// external surface(비디오)의 add_surface 전용 경로(브리프 Step 4). 비주얼을 항상
    /// 배치(마지막 프레임 유지)하고 z-order에 기록한 뒤, provider 링을 대여해
    /// present_external로 이번 프레임을 변환+Present한다. lease는 acquire↔release를 반드시
    /// 짝맞춘다 — present_external은 release 책임을 지지 않으므로(정상 반환만) 아래 단일
    /// release가 모든 경로를 커버한다.
    fn add_external_surface(
        &mut self,
        id: NativeSurfaceId,
        transform: CompositorSurfaceTransform,
        clip_rect: DeviceIntRect,
        rounded_clip_radii: ClipRadius,
    ) {
        if rounded_clip_radii != ClipRadius::EMPTY && !self.warned_rounded_clip {
            warn!("[dcomp-native] rounded clip radii unsupported; applying rectangular clip only");
            self.warned_rounded_clip = true;
        }
        // Step 4-1: 빈 클립이면 skip(frame_surfaces 미기록 — 이 프레임 비합성).
        if clip_rect.is_empty() {
            return;
        }
        let size = clip_rect.size();

        // Step 4-7/8(앞당김): 비주얼 배치 + z-order 기록은 provider 유무와 무관하게 항상.
        self.place_external_visual(id, clip_rect);
        self.frame_surfaces.push(id);

        let rc = self.rendering_context.clone();
        let resize_active = rc.dcomp_resize_active();

        // is_opaque + 이번 서피스에 attach된 ExternalImageId 조회(짧은 borrow).
        let (is_opaque, attached) = match self.surfaces.get(&id).map(|e| (e.is_opaque, &e.storage)) {
            Some((op, SurfaceStorage::External(ext))) => (op, ext.attached_external_id),
            // 도달 불가(호출 전 External 판정) — 방어적.
            _ => return,
        };

        // Step 4-3: provider 획득. 미등록/미attach/lease 실패는 모두 비주얼만(마지막 프레임 유지).
        let Some(provider) = paint_api::video_external_surface_provider() else {
            if !self.warned_no_provider {
                warn!(
                    "[dcomp-native] external surface: no VideoExternalSurfaceProvider registered; \
                     placing visual only (last frame retained)"
                );
                self.warned_no_provider = true;
            }
            return;
        };
        let Some(external_id) = attached else {
            // attach_external_image 전 — 표시할 프레임 없음(비주얼만).
            return;
        };
        // acquire는 링을 잠그고 소비계획을 실행한다 — Some이면 반드시 release로 짝맞춘다.
        // None(대여 실패)이면 아무것도 안 잠겼으므로 release 불필요(브리프 계약).
        let acq_start = if video_escape_prof() { Some(std::time::Instant::now()) } else { None };
        let maybe_lease = provider.acquire(&*rc, external_id);
        if let Some(start) = acq_start {
            self.esc_prof.acquires += 1;
            self.esc_prof.acquire_dur += start.elapsed();
        }
        let Some(lease) = maybe_lease else {
            return;
        };
        let ring_id = lease.ring_id;

        // Step 4-4/5: 스왑체인 보장 + 변환 + Present. 어떤 경로로 끝나도(정상 반환) 아래
        // release가 lease를 반납한다(present_external은 release를 호출하지 않는다).
        self.present_external(id, &lease, size, is_opaque, resize_active, transform, clip_rect);

        // Step 4-6: release 짝맞춤(Present 성공/실패/스킵 무관).
        provider.release(&*rc, ring_id);
    }

    /// external surface의 스왑체인 보장 + YUV 변환 1-draw + Present(브리프 Step 4-4/5).
    /// lease의 release는 호출측(add_external_surface)이 책임진다 — 이 메서드는 정상 반환만
    /// 하며 release를 호출하지 않는다(어떤 조기 반환도 release 짝맞춤을 깨지 않는다).
    fn present_external(
        &mut self,
        id: NativeSurfaceId,
        lease: &VideoFrameLease,
        size: DeviceIntSize,
        is_opaque: bool,
        resize_active: bool,
        transform: CompositorSurfaceTransform,
        clip_rect: DeviceIntRect,
    ) {
        // convert_pass 지연 초기화(최초 external present 시). 실패하면 재시도하지 않는다
        // (new()가 이미 warn — 재컴파일/재warn 스팸 방지). 비-비디오 월은 여기 도달 안 함.
        if self.convert_pass.is_none() && !self.convert_pass_init_failed {
            // Safety: d3d11_device는 rendering_context가 수명을 보장하는 살아있는 ANGLE 디바이스.
            self.convert_pass =
                unsafe { crate::dcomp_video_convert::VideoConvertPass::new(self.d3d11_device) };
            if self.convert_pass.is_none() {
                self.convert_pass_init_failed = true;
                warn!(
                    "[dcomp-native] external surface: VideoConvertPass unavailable; placing visual only"
                );
            }
        }
        let d3d11_device = self.d3d11_device;

        // Step 4-4: 스왑체인 필요 판정(없거나 클립 크기 변화). create_composition_swapchain은
        // &self라 entry borrow 밖에서 호출한다. 드래그 중(resize_active)엔 재생성 억제 —
        // 매 틱 재생성은 content_attached 리셋→Present 보류→블랙(§3-y 정합). 기존 스왑체인 유지.
        let need_new = match self.surfaces.get(&id).map(|e| &e.storage) {
            Some(SurfaceStorage::External(ext)) => {
                ext.swapchain.is_none() || ext.swapchain_size != size
            },
            _ => return,
        };
        let created: Option<Option<ComOwned<IDXGISwapChain1>>> = if need_new && !resize_active {
            Some(self.create_composition_swapchain(size, is_opaque))
        } else {
            None
        };

        // entry 재-borrow: 스왑체인 설치 + 브링업 로그 + 세대 dedup + 변환/Present.
        let Some(entry) = self.surfaces.get_mut(&id) else {
            return;
        };
        let SurfaceStorage::External(ext) = &mut entry.storage else {
            return;
        };

        if let Some(created) = created {
            // 스왑체인 교체 → 옛 백버퍼 무효. 캐시된 RTV를 버려 스테일 포인터 재사용을 막는다
            // (새 버퍼가 옛 주소를 재활용해도 오조회 없음).
            ext.rtv_cache.clear();
            match created {
                Some(sc) => {
                    ext.swapchain = Some(sc);
                    ext.swapchain_size = size;
                    ext.content_attached = false;
                    ext.last_presented = None;
                    ext.warned_fail = false; // 재생성 성공 → 실패 warn 재무장
                    if dcomp_debug() {
                        log::info!(
                            "[dcomp-dbg] external swapchain (re)create id={:?} {}x{}",
                            id, size.width, size.height
                        );
                    }
                },
                None => {
                    // 생성 실패(OOM급): swapchain=None 유지 → 다음 프레임 자연 재시도(스펙 §9).
                    ext.swapchain = None;
                    ext.swapchain_size = DeviceIntSize::zero();
                    ext.content_attached = false;
                    ext.last_presented = None;
                    if !ext.warned_fail {
                        warn!(
                            "[dcomp-native] external surface {:?}: swapchain {}x{} create failed; \
                             retrying next frame",
                            id, size.width, size.height
                        );
                        ext.warned_fail = true;
                    }
                },
            }
        }

        // Step 4-2: 브링업 계약 로그(서피스당 최초 5프레임, dcomp_debug 게이트). src=Y plane 크기.
        // 기대(월): scale=(1,1), clip≈타일 rect, src=1920x1080.
        if dcomp_debug() && ext.frames_logged < 5 {
            ext.frames_logged += 1;
            let (sw, sh) = lease.planes[0].map(|p| (p.width, p.height)).unwrap_or((0, 0));
            log::info!(
                "[dcomp-dbg] external add id={:?} scale=({},{}) offset=({},{}) \
                 clip=({},{})-({},{}) src={}x{}",
                id, transform.scale.x, transform.scale.y, transform.offset.x, transform.offset.y,
                clip_rect.min.x, clip_rect.min.y, clip_rect.max.x, clip_rect.max.y, sw, sh
            );
        }

        // Step 4-5: 세대 dedup — 같은 (ring_id, frame_seq)면 재변환/재Present 스킵.
        if !external_needs_present(ext.last_presented, lease.ring_id, lease.frame_seq) {
            return;
        }
        // dst는 스왑체인 백버퍼 크기(= swapchain_size)를 쓴다. 드래그 중엔 기존(옛 크기)
        // 스왑체인을 유지하므로 clip(새 크기)이 아닌 실제 버퍼 크기로 변환해야 뷰포트가
        // 버퍼를 넘지 않는다(정착 후 재생성으로 정확 크기 복원).
        let dst = ext.swapchain_size;
        if dst.width <= 0 || dst.height <= 0 {
            return;
        }
        let (Some(swapchain), Some(convert_pass), Some(ctx1)) = (
            ext.swapchain.as_ref(),
            self.convert_pass.as_mut(),
            self.d3d11_context1.as_ref(),
        ) else {
            // 스왑체인/변환패스/컨텍스트 중 하나라도 없으면 present 스킵(비주얼만).
            return;
        };

        // flip 스왑체인 present: GetBuffer(0)로 이번 백버퍼를 얻고, RTV는 백버퍼 포인터로 캐시
        // 조회(FLIP 2버퍼 → 엔트리 ≤2)해 per-present CreateRenderTargetView를 없앤다. 이번 프레임
        // 첫 external convert면 begin_batch로 컨텍스트-상태 배치를 연다(프레임당 스왑 2N→2).
        // Safety: 살아있는 스왑체인/디바이스/컨텍스트/백버퍼.
        let prof_on = video_escape_prof();
        let mut d_converts = 0u64;
        let mut d_convert_dur = std::time::Duration::ZERO;
        let mut d_srv = 0u64;
        let mut d_presents = 0u64;
        let mut d_present_dur = std::time::Duration::ZERO;
        let mut d_batch_swaps = 0u64;
        unsafe {
            let mut back: *mut ID3D11Texture2D = ptr::null_mut();
            let hr = (*swapchain.as_ptr()).GetBuffer(
                0,
                &ID3D11Texture2D::uuidof(),
                &mut back as *mut _ as *mut _,
            );
            if hr < 0 || back.is_null() {
                if !ext.warned_fail {
                    warn!("[dcomp-native] external {:?}: GetBuffer(0) failed (hr=0x{:08x})", id, hr as u32);
                    ext.warned_fail = true;
                }
                return;
            }
            // RTV 캐시 조회(키 = 백버퍼 텍스처 포인터). 미스면 생성해 소유권을 캐시로 이전한다.
            // 스왑체인 백버퍼는 Map rename이 없어 캐시 RTV가 유효 유지(rtv_cache 필드 주석).
            let back_key = back as usize;
            if !ext.rtv_cache.contains_key(&back_key) {
                let mut raw: *mut ID3D11RenderTargetView = ptr::null_mut();
                let hr = (*d3d11_device).CreateRenderTargetView(
                    back as *mut ID3D11Resource,
                    ptr::null(),
                    &mut raw,
                );
                if hr < 0 || raw.is_null() {
                    if !ext.warned_fail {
                        warn!("[dcomp-native] external {:?}: CreateRenderTargetView failed (hr=0x{:08x})", id, hr as u32);
                        ext.warned_fail = true;
                    }
                    (*(back as *mut IUnknown)).Release();
                    return;
                }
                // Safety: raw는 방금 만든 유효한 RTV — 소유권을 캐시로 이전.
                let Some(owned) = ComOwned::from_raw(raw) else {
                    (*(back as *mut IUnknown)).Release();
                    return;
                };
                ext.rtv_cache.insert(back_key, owned);
            }
            // 캐시된(또는 방금 생성한) RTV. as_ptr는 비소유 포인터 — Release 금지(캐시가 소유).
            let rtv = ext.rtv_cache.get(&back_key).map(|c| c.as_ptr()).unwrap();

            // 이번 프레임 첫 external convert에서 배치 오픈(스왑 1회). 이후 convert는 스왑 생략.
            if !self.external_batch_active {
                convert_pass.begin_batch(ctx1.as_ptr());
                self.external_batch_active = true;
                d_batch_swaps += 1;
            }
            // convert: 배치 활성이면 스왑 없이 per-draw 자원만 바인딩+draw(begin_batch 주석).
            let c_start = if prof_on { Some(std::time::Instant::now()) } else { None };
            let converted =
                convert_pass.convert(ctx1.as_ptr(), lease, rtv, dst.width as u32, dst.height as u32);
            if let Some(s) = c_start {
                d_convert_dur += s.elapsed();
                d_converts += 1;
                d_srv += lease.plane_count.min(3) as u64;
            }
            // RTV는 캐시가 소유 — 여기서 Release하지 않는다(백버퍼는 아래에서 Release).
            if converted {
                let p_start = if prof_on { Some(std::time::Instant::now()) } else { None };
                let hr = (*swapchain.as_ptr()).Present(0, 0);
                if let Some(s) = p_start {
                    d_present_dur += s.elapsed();
                    d_presents += 1;
                }
                if hr < 0 {
                    if !ext.warned_fail {
                        warn!("[dcomp-native] external {:?}: Present failed (hr=0x{:08x})", id, hr as u32);
                        ext.warned_fail = true;
                    }
                } else {
                    ext.last_presented = Some((lease.ring_id, lease.frame_seq));
                    // 첫 성공 Present 후 visual 콘텐츠를 스왑체인으로 전환(1회). 같은
                    // add_surface 안에서 convert+present+SetContent가 end_frame Commit 전
                    // 완결되므로 플래시 없음(Commit 원자성).
                    if !ext.content_attached {
                        let hr = (*entry.visual.as_ptr())
                            .SetContent(swapchain.as_ptr() as *const IUnknown);
                        if hr >= 0 {
                            ext.content_attached = true;
                            if dcomp_debug() {
                                log::info!("[dcomp-dbg] external content-attach id={:?}", id);
                            }
                        } else if !ext.warned_fail {
                            warn!("[dcomp-native] external {:?}: SetContent failed (hr=0x{:08x})", id, hr as u32);
                            ext.warned_fail = true;
                        }
                    }
                }
            }
            (*(back as *mut IUnknown)).Release();
        }

        // 프로파일 누산(게이트 on일 때만). 여기선 ext/entry/convert_pass/ctx1 borrow가 끝나
        // self.esc_prof 갱신이 안전하다. batch_swaps의 end 쪽은 close_external_batch에서 카운트.
        if prof_on {
            self.esc_prof.converts += d_converts;
            self.esc_prof.convert_dur += d_convert_dur;
            self.esc_prof.srv_creates += d_srv;
            self.esc_prof.presents += d_presents;
            self.esc_prof.present_dur += d_present_dur;
            self.esc_prof.batch_swaps += d_batch_swaps;
        }
    }

    /// external convert 배치가 열려 있으면 닫는다(멱등). ★반드시 이 스레드의 다음 ANGLE/GL
    /// 호출 전에 불려야 한다 — 배치가 열린 동안 우리 ID3DDeviceContextState가 활성이라 ANGLE의
    /// GL→D3D11 상태 설정이 어긋난다(begin_batch 주석). 정본 호출 지점은 start_compositing:
    /// webrender 0.68 renderer/mod.rs에서 composite_native(:5329)가 add_surface 루프(:6667) 직후
    /// start_compositing(:6677)을 부르고, 그 다음 draw_frame의 passes 루프가 picture-cache 타일을
    /// compositor.bind(:5380)로 GL 렌더한다 — 즉 start_compositing은 add_surface 루프의 모든
    /// external convert '뒤', 타일 GL '앞'이라 배치를 닫을 정확한 위치다. begin_frame/end_frame은
    /// 방어적 멱등 넷(정상 경로에선 이미 닫혀 no-op).
    fn close_external_batch(&mut self) {
        if !self.external_batch_active {
            return;
        }
        if let (Some(cp), Some(ctx1)) = (self.convert_pass.as_mut(), self.d3d11_context1.as_ref()) {
            // Safety: cp/ctx1은 begin_batch를 연 바로 그 패스/컨텍스트(프레임 중 교체 없음).
            unsafe {
                cp.end_batch(ctx1.as_ptr());
            }
            if video_escape_prof() {
                self.esc_prof.batch_swaps += 1;
            }
        }
        self.external_batch_active = false;
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
                content_valid_union: None,
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
            // Task 10: 타일 집합(지오메트리) 변경 → 누적 valid 유니온 무효화 후 재집계.
            entry.content_valid_union = None;
        }
    }

    fn bind(
        &mut self,
        device: &mut Device,
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
        // Captured before the &mut entry.storage borrow. `is_opaque` gates the Task 9 fix
        // (transparent-fill of the never-drawn band) and the env-gated valid_probe log; the
        // last add_surface placement (clip + transform offset) drives the visible-region
        // expansion of the BeginDraw update rect (Task 9 fix, Virtual arm).
        let entry_is_opaque = entry.is_opaque;
        let entry_virtual_offset = entry.virtual_offset;
        let (entry_clip, entry_transform_offset) = match entry.last_placement {
            Some(p) => (Some(p.clip_rect), p.transform_offset),
            None => (None, (0.0, 0.0)),
        };

        // Task 10: 이 bind의 device valid 영역을 서피스 누적 유니온에 합류(스케일=1).
        // device 타일 원점 = (id.x*tile_w, id.y*tile_h) (virtual_offset은 device 변환에서 상쇄),
        // valid_rect는 타일-로컬. 퇴화(빈) valid는 무시. 불투명 phantom clip 좁힘의 근거.
        if valid_rect.max.x > valid_rect.min.x && valid_rect.max.y > valid_rect.min.y {
            let dev_ox = id.x * entry.tile_size.width;
            let dev_oy = id.y * entry.tile_size.height;
            let dvalid = DeviceIntRect::new(
                DeviceIntPoint::new(dev_ox + valid_rect.min.x, dev_oy + valid_rect.min.y),
                DeviceIntPoint::new(dev_ox + valid_rect.max.x, dev_oy + valid_rect.max.y),
            );
            entry.content_valid_union = Some(match entry.content_valid_union {
                Some(u) => u.union(&dvalid),
                None => dvalid,
            });
        }

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
                // 이번 프레임 bind가 그린 버퍼-로컬 더티 렉트 누적(스펙 §5.2 — 부분 Present
                // 힌트 + stale 부기 재료). dirty_rect는 타일-로컬이므로 타일 가상 rect를 더해
                // 가상좌표로 만든 뒤, 백버퍼 (0,0)에 대응하는 anchor를 빼 버퍼-로컬로 환산한다.
                let dirty_buf = DeviceIntRect::new(
                    DeviceIntPoint::new(
                        tile_rect.min.x + dirty_rect.min.x - sc.anchor.x,
                        tile_rect.min.y + dirty_rect.min.y - sc.anchor.y,
                    ),
                    DeviceIntPoint::new(
                        tile_rect.min.x + dirty_rect.max.x - sc.anchor.x,
                        tile_rect.min.y + dirty_rect.max.y - sc.anchor.y,
                    ),
                );
                sc.frame_dirty.push(dirty_buf);
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

                // Task 9 근본원인(스펙 2026-07-15 실측): WR은 비불투명 티커 슬라이스에서 매 프레임
                // valid_rect == dirty_rect == "움직이는 텍스트 밴드"만 넘긴다(스크롤 텍스트 = 전체
                // 재도색). 그런데 이 서피스 visual의 클립은 티커 바 전체다(WR composite.rs:1043 —
                // native 경로 클립 = device_clip ∩ 모든 타일 valid_rect의 바운딩 유니온 = 바 전체;
                // Draw 경로는 renderer/mod.rs:3739에서 타일별로 device_valid_rect까지 클립하므로
                // 결함이 없다). 따라서 DWM은 가상 서피스의 [타일 ∩ 클립] 전체를 합성하지만 우리는
                // 텍스트 밴드만 그리므로, 텍스트 아래 띠(그려진 적 없는 영역)가 DComp 가상 서피스의
                // 미초기화/스테일 메모리(검정·이전 텍스트 잔상·청색 밴드)를 노출한다.
                //
                // 수정: valid==dirty 인 비불투명 타일은 BeginDraw 업데이트 렉트를 "가시 타일 영역"
                // (타일 ∩ 서피스 클립)으로 확장하고, WR draw 전에 pbuffer 전체를 투명으로 클리어
                // 한다(아래 make_current 직후). WR은 Windows/ANGLE에서 clear를 dirty로 시저링하므로
                // (device/gl.rs: prefers_clear_scissor=true) 스스로는 밴드를 지우지 않는다 — 우리가
                // 투명 클리어 후 EndDraw가 확장 렉트를 커밋하면, 텍스트 밖 밴드가 투명이 되어 뒤
                // 서피스(불투명 바 배경)가 비친다. valid==dirty 가드로 지속(valid−dirty) 콘텐츠는
                // 절대 지우지 않는다(전체 재도색 슬라이스에만 적용). 불투명/스왑체인 경로 무영향.
                let expand_transparent = !entry_is_opaque && dirty_rect == valid_rect;
                let update_rect_v = if expand_transparent {
                    tile_visible_virtual_rect(
                        tile_rect,
                        entry_clip,
                        entry_transform_offset,
                        entry_virtual_offset,
                    )
                } else {
                    // 기존: 타일 rect를 타일-로컬 dirty로 오프셋한 가상 절대좌표 렉트.
                    DeviceIntRect::new(
                        DeviceIntPoint::new(
                            tile_rect.min.x + dirty_rect.min.x,
                            tile_rect.min.y + dirty_rect.min.y,
                        ),
                        DeviceIntPoint::new(
                            tile_rect.min.x + dirty_rect.max.x,
                            tile_rect.min.y + dirty_rect.max.y,
                        ),
                    )
                };
                // BeginDraw는 가상공간 절대좌표 RECT를 받는다.
                let update = RECT {
                    left: update_rect_v.min.x,
                    top: update_rect_v.min.y,
                    right: update_rect_v.max.x,
                    bottom: update_rect_v.max.y,
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

                // Task 9 수정: 확장 모드면 WR draw 전에 pbuffer(BeginDraw scratch 영역)를
                // 투명으로 클리어한다. WR draw_picture_cache_target이 이후 clear_target/
                // set_blend/scissor를 매번 새로 세팅하므로 상태 desync 없음(WR device gl.rs:
                // 시저·clear color·color write 모두 캐시 없는 &self 직접 호출).
                //
                // 리뷰 하드닝(1) — 아틀라스 공유 위험: BeginDraw 텍스처는 DComp 아틀라스일 수
                // 있어(다른 타일의 커밋된 영역이 다른 오프셋에 함께 들어있을 수 있음) 텍스처
                // 전체 클리어는 미검증 구성에서 이웃 타일의 이미 커밋된 픽셀을 파괴할 위험이
                // 있다. 이 BeginDraw가 실제로 소유한 영역만 [update_offset ..
                // update_offset+update_rect_v.size()]으로 시저를 제한해 클리어하고, 클리어 후
                // 시저를 다시 비활성화해 기존 앰비언트 상태(WR이 draw마다 자체 시저를 세팅)를
                // 보존한다. 텍스처는 wrap 1:1이라 update_offset을 시저 y에 그대로 써도
                // top-left 규약과 정합한다(unbind readback도 update_offset을 텍스처 좌표로
                // 그대로 샘플링 — 위 주석 참조).
                if expand_transparent {
                    use gleam::gl;
                    let g = device.gl();
                    // 리뷰 하드닝(2) — 잔류 FBO 바인딩 함정: make_render_pbuffer_current는
                    // 단순 eglMakeCurrent라 GL 프레임버퍼 바인딩을 리셋하지 않는다. WR의 직전
                    // 패스(텍스처 캐시 렌더 타깃)가 FBO를 바인딩한 채 남아있으면 클리어가 그
                    // FBO를 때릴 수 있으므로 pbuffer의 기본 프레임버퍼(0)로 명시 바인딩한다.
                    g.bind_framebuffer(gl::FRAMEBUFFER, 0);
                    let sw = update_rect_v.max.x - update_rect_v.min.x;
                    let sh = update_rect_v.max.y - update_rect_v.min.y;
                    g.enable(gl::SCISSOR_TEST);
                    g.scissor(update_offset.x, update_offset.y, sw, sh);
                    g.color_mask(true, true, true, true);
                    g.clear_color(0.0, 0.0, 0.0, 0.0);
                    g.clear(gl::COLOR_BUFFER_BIT);
                    g.disable(gl::SCISSOR_TEST);
                }

                // 업데이트 렉트의 타일-로컬 top-left(확장 시 클립이 타일 좌상단을 잘라내면 >0).
                // 비확장 시엔 (dirty.min)과 동일 → 원래 origin·readback 좌표를 그대로 보존한다.
                let r_min_x = update_rect_v.min.x - tile_rect.min.x;
                let r_min_y = update_rect_v.min.y - tile_rect.min.y;

                // 텍스처는 WR의 GL draw 동안 살려두고 unbind의 EndDraw 이후 Release한다.
                self.bound = Some(BoundTile {
                    surface_id: id.surface_id,
                    pbuffer,
                    texture,
                    // Task 6 diagnosis: sampling coordinates for the unbind readback.
                    // dirty의 scratch 내 위치 = update_off + (dirty.min - r_min). 비확장 시
                    // r_min==dirty.min이라 update_off 그대로(기존 readback 좌표 불변).
                    tile: (id.x, id.y),
                    update_offset: (
                        update_offset.x + dirty_rect.min.x - r_min_x,
                        update_offset.y + dirty_rect.min.y - r_min_y,
                    ),
                    dirty_size: (
                        dirty_rect.max.x - dirty_rect.min.x,
                        dirty_rect.max.y - dirty_rect.min.y,
                    ),
                    tex_size: (desc.Width as i32, desc.Height as i32),
                });

                // 승격 판정용 프레임 피복 집계(dirty가 valid 전체를 덮는 전면 타일만).
                entry.frame_coverage.note_tile((id.x, id.y), dirty_rect, valid_rect);
                // 부분 더티 프레임도 "그려짐"으로 세기(나이 카운터에 포함).
                entry.frame_drawn_partial = true;

                // WR 타일-로컬 좌표계 성립: origin = update_offset - r_min (Gecko DCLayerTree 동일).
                // r_min = 업데이트 렉트의 타일-로컬 top-left. 비확장 경로에선 r_min == dirty_rect.min
                // 이라 기존 산식(update_offset - dirty.min)과 바이트 동일하다. WR은 타일-로컬 좌표 C를
                // scratch 위치 update_offset + (C - r_min)에 그리므로, 이 origin으로 dirty가 정위치에
                // 놓이고 EndDraw가 확장 렉트를 커밋한다.
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
                    update_offset.x - r_min_x,
                    update_offset.y - r_min_y,
                );
                // Task 9 diagnosis: per-bind coverage for non-opaque surfaces (first 300 frames).
                if valid_probe_enabled() && !entry_is_opaque && self.frame_counter <= 300 {
                    log::info!(
                        "[dcomp-validprobe] surface={:?} tile=({},{}) dirty=({},{})-({},{}) \
                         valid=({},{})-({},{}) update_rect=({},{})-({},{}) update_off=({},{}) \
                         tex={}x{} origin=({},{})",
                        id.surface_id, id.x, id.y,
                        dirty_rect.min.x, dirty_rect.min.y, dirty_rect.max.x, dirty_rect.max.y,
                        valid_rect.min.x, valid_rect.min.y, valid_rect.max.x, valid_rect.max.y,
                        update.left, update.top, update.right, update.bottom,
                        update_offset.x, update_offset.y, desc.Width, desc.Height,
                        origin.x, origin.y,
                    );
                }
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
            SurfaceStorage::External(_) => {
                // WR은 external surface에 타일을 그리지 않는다(create_tile/bind 미호출) — 도달 불가.
                warn!("[dcomp-native] bind on external surface {:?}; ignoring", id.surface_id);
                NativeSurfaceInfo { origin: DeviceIntPoint::zero(), fbo_id: 0 }
            },
        }
    }

    fn unbind(&mut self, device: &mut Device) {
        let Some(bound) = self.bound.take() else {
            return;
        };
        // Task 6 defect-2 diagnosis: read the tile pixels BEFORE EndDraw/destroy while the
        // pbuffer is still current. Gated (env off => no glReadPixels) and frame-limited to
        // <=120 (spec §7.1 stall amplification). Only the Virtual arm sets `bound`, which is
        // exactly the alpha-slice path under scrutiny.
        if readback_enabled() && self.frame_counter <= 120 {
            let is_opaque = self.surfaces.get(&bound.surface_id).map(|e| e.is_opaque);
            readback_log_bound(device, &bound, is_opaque);
        }
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
                SurfaceStorage::External(_) => {
                    // 도달 불가: external도 bind되지 않는다(self.bound 미설정 → 위 take()가 조기 반환).
                    warn!(
                        "[dcomp-native] unbind: unexpected bound tile on external surface {:?}",
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
        // external 배치는 정상적으로 직전 프레임 start_compositing에서 닫힌다. 만약 어떤 이유로
        // 잔류했다면(방어) 이 프레임 작업 전에 닫아 external_batch_active를 리셋한다 — begin_frame은
        // DComp(RemoveAllVisuals)만 하고 GL을 안 쓰므로 여기서 닫아도 안전하다.
        self.close_external_batch();
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
        // add_surface의 AddVisual은 end_frame으로 이연 — 이번 프레임 기록 초기화.
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
        // 비디오 external surface(Task 5)는 전용 경로 — 기존 Virtual/SwapChain 경로 위 조기
        // 분기. scale 경고는 external에 미적용(브리프 계약: external은 dest=clip, scale 무시).
        if matches!(
            self.surfaces.get(&id).map(|e| &e.storage),
            Some(SurfaceStorage::External(_))
        ) {
            self.add_external_surface(id, transform, clip_rect, rounded_clip_radii);
            return;
        }

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
            // external은 add_surface 진입부에서 이미 add_external_surface로 분기·반환 — 도달 불가.
            SurfaceStorage::External(_) => DeviceIntPoint::zero(),
        };

        // Task 10 근본원인(스펙 2026-07-15 실측): 티커 바의 불투명 슬라이스(바 배경 + 속보
        // 라벨)는 "phantom"이다 — 스크롤 텍스트가 타일에 겹쳐 WR이 그 타일들을 알파로 분류하고
        // 콘텐츠 대부분을 알파 동반 서피스로 보내므로, 불투명 서피스는 타일 1개 남짓만 bind되는데
        // 그 DXGI_ALPHA_MODE_IGNORE visual은 WR 슬라이스 클립(= device_clip ∩ 슬라이스 전체 타일
        // valid의 유니온, composite.rs:1043)으로 바 전체를 덮는다. 결과: 그려지지 않은 영역이
        // DComp 가상 서피스의 미초기화/재활용 pool 메모리를 불투명하게 노출(검정·이전 텍스트 잔상).
        //
        // 수정: 불투명 서피스는 visual 클립을 WR 슬라이스 클립 대신 "이 서피스가 실제로 bind한
        // 타일 valid 영역의 device 바운딩 유니온"(content_valid_union)으로 교차해 좁힌다. 이는 Draw
        // 경로가 타일별 device_valid_rect로 per-tile 클립하는 것(renderer/mod.rs:3737)과 등가 근사이며,
        // phantom(미도색) 영역이 합성되지 않게 한다. 전면 피복 불투명 서피스(비디오 그리드 등)는
        // 유니온 == 전체 클립이라 무변화(순수 월 무회귀). 비불투명 서피스는 알파 블렌딩으로 미도색
        // 영역이 자연 투명(Task 9 처리)이라 대상 아님.
        let clip_rect = refine_opaque_clip(clip_rect, entry.is_opaque, entry.content_valid_union);

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

        // AddVisual은 end_frame으로 이연 — 여기서는 z-order(호출 순서 = 아래→위)를 보존한
        // 기록만 남긴다(컬링 없음 — A-1).
        self.frame_surfaces.push(id);
    }

    fn start_compositing(
        &mut self,
        _device: &mut Device,
        _clear_color: ColorF,
        _dirty_rects: &[DeviceIntRect],
        _opaque_rects: &[DeviceIntRect],
    ) {
        // ★external convert 배치를 닫는 정본 위치. WR 렌더러는 composite_native(add_surface 루프)
        // 직후 start_compositing을 부르고, 그 '뒤'에 draw_frame의 passes 루프가 picture-cache 타일을
        // compositor.bind로 GL 렌더한다(webrender 0.68 renderer/mod.rs: :6667 루프 → :6677 이 콜 →
        // :5380 타일 bind). 즉 여기가 이 프레임 모든 external convert '뒤', 타일 GL '앞'이라 배치를
        // 닫아 우리 ID3DDeviceContextState를 ANGLE 상태로 되돌릴 정확한 지점이다. end_frame(:1913)은
        // passes 루프 '뒤'라 너무 늦다(그래서 end_frame의 닫기는 방어 넷일 뿐).
        self.close_external_batch();
    }

    fn end_frame(&mut self, device: &mut Device) {
        // 방어: 정상 경로는 start_compositing이 이미 배치를 닫았다(no-op). 혹시 열려 있으면
        // 아래 device.gl().flush()를 포함한 어떤 GL보다 먼저 닫아야 한다 — 배치가 열린 채 GL이
        // 돌면 ANGLE 상태가 어긋난다(close_external_batch/begin_batch 주석).
        self.close_external_batch();
        // GL 커맨드를 D3D 큐에 확실히 제출한 뒤 Present(순서 보장).
        device.gl().flush();

        let mode = storage_mode();
        let rc = self.rendering_context.clone();
        // task-12b: painter가 세운 드래그-리사이즈 활성 신호(공유 RenderingContext Cell).
        // true인 동안: 스왑체인을 즉시 가상으로 강등 + 승격/regen 억제(가상 서피스가 드래그
        // 운반). 프레임 내내 불변이므로 여기서 1회 읽어 아래 게이트에 사용한다.
        let resize_active = rc.dcomp_resize_active();
        // borrow 사정(iter_mut 중 self 메서드 호출 불가)상 스왑체인 생성이 필요한 요청을
        // 모아 루프 밖에서 처리한다.
        //  - promote_requests: Virtual → SwapChain 신규 승격 (id, 승격 extent)
        //  - regen_requests: 리사이즈 등으로 지오메트리가 바뀐 기존 SwapChain 재생성 (id, 새 extent)
        //  - demote_requests: withhold 임계·전면/부분 Present 런타임 실패 → 강등 대상
        //    (스펙 §6.1). 처리(가상 서피스 시딩 + 쿨다운)는 이 함수 뒤쪽 강등 루프.
        let mut promote_requests: Vec<(NativeSurfaceId, DeviceIntRect)> = Vec::new();
        let mut regen_requests: Vec<(NativeSurfaceId, DeviceIntRect)> = Vec::new();
        let mut demote_requests: Vec<NativeSurfaceId> = Vec::new();

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
                    // 스펙 §6.3 "쿨다운 중에는 streak 누적 자체를 중단" — 강등 쿨다운이
                    // 남아 있으면 누적하지 않는다(만료 시점에 즉시 재승격하지 않고, 만료
                    // 후 새로 PROMOTE_STREAK 프레임의 전면 갱신을 다시 채워야 승격).
                    entry.promote_streak = if frame_full
                        && entry.drawn_frames > PROMOTE_MIN_AGE_FRAMES
                        && self.frame_counter >= entry.promote_blocked_until
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
                    // task-12b: 리사이즈 중에는 승격을 억제한다 — 드래그 중 스왑체인으로
                    // 올리면 매 틱 extent 변경 → regen 처닝(content_attached 리셋 → Present
                    // 보류 → 비디오 블랙)이 재발한다. 정착(resize_active=false) 후 자연 재승격.
                    if !resize_active
                        && mode == StorageMode::Hybrid
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

                    if resize_active {
                        // task-12b: 리사이즈 중(드래그) — regen/present/withhold 정상 로직을
                        // 전부 건너뛰고 스왑체인을 즉시 강등 큐에 넣는다. regen은 매 틱
                        // 스왑체인을 재생성하며 content_attached를 리셋 → 새 full-coverage
                        // Present까지 Present 보류 = 비디오 블랙의 근본원인. 드래그 동안에는
                        // 승격 상태를 버리고 가상 서피스(BeginDraw가 임의 지오메트리를 매 프레임
                        // 처리 = 블랙 없음)로 운반한다. 아래 강등 루프가 buffer 0을 시딩해
                        // 현재 픽셀을 가상으로 이관하므로 표시 연속성도 유지된다.
                        demote_requests.push(*id);
                    } else if geometry_changed {
                        if !self.warned_regen_fail {
                            if let Some(e) = cur_extent {
                                // 조밀성 가드: 타일 집합이 e를 빈틈없이 채우지 않으면 regen을
                                // 보류한다(과도기 구멍이 있는 채로 재생성하면 그 구멍이
                                // FLIP_SEQUENTIAL 미정의 픽셀로 Present될 위험 — 다음 프레임에
                                // 재판정).
                                if tiles_are_dense(entry.tiles.len(), entry.tile_size, e.size()) {
                                    regen_requests.push((*id, e));
                                }
                            }
                        }
                        // 최종 리뷰 Important #2: geometry_changed가 여러 프레임 지속되면(조밀성
                        // 가드 미충족 또는 warned_regen_fail로 regen 영구 중단) bind()가 매 프레임
                        // frame_dirty를 계속 push해 무한 성장할 수 있다. 규정 위 안전 방향으로
                        // 상한 초과분을 붕괴 — stale은 regen에서 리셋되고 다음 content-swap이
                        // 항상 전면 시딩하므로 정보 손실 없음.
                        sc.frame_dirty =
                            collapse_dirty_if_oversized(std::mem::take(&mut sc.frame_dirty), MAX_STALE_RECTS);
                    } else if sc.drawn_this_frame && sc.coverage.is_full(&entry.tiles) {
                        // 첫 콘텐츠 Present 여부는 성공 처리 전에 계산(성공 시 content_attached가
                        // 바뀌므로 stale 시딩 판정에 원본 값이 필요).
                        let first_content_present = !sc.content_attached;
                        // frame_dirty는 hr 판정 전에 take — Present 실패 시 복원한다(버퍼가
                        // 로테이트되지 않았으므로 이 더티 영역은 이후 성공하는 Present의 부기에
                        // 반드시 합류해야 함 — 드롭하면 stale 과소 기록으로 잔상 결함).
                        let dirty = std::mem::take(&mut sc.frame_dirty);
                        // Safety: 살아있는 스왑체인. SyncInterval 0 = 비블로킹(페이싱은 기존 유지).
                        let hr = unsafe { (*sc.swapchain.as_ptr()).Present(0, 0) };
                        if hr < 0 {
                            if !sc.warned_present_fail {
                                warn!("[dcomp-native] Present failed (hr=0x{:08x})", hr as u32);
                                sc.warned_present_fail = true;
                            }
                            // 실패 → 미로테이트. 누적 더티를 복원해 다음 프레임 push와 병합.
                            // 보강 항목 1: coverage가 계속 "전면"으로 남아 지속 실패 시 이 분기가
                            // 매 프레임 재진입 → frame_dirty가 매 프레임 누적되어 무한정 커질 수
                            // 있다. 상한 초과분은 바운딩 유니온 1개로 붕괴(과대 복사는 안전).
                            sc.frame_dirty = collapse_dirty_if_oversized(dirty, MAX_STALE_RECTS);
                            // 스펙 §8: DXGI 호출 실패는 per-surface warn-once + 즉시 강등 대상
                            // (§6.1 표에 이 Present(0,0) 실패가 명시 나열되진 않았지만, §8의
                            // 일반 정책이 모든 DXGI/DComp 호출 실패에 적용된다 — 보강 항목 2).
                            // 강등 시딩이 실패해도(스왑체인 유지) coverage.is_full은 그대로라
                            // 다음 프레임 이 분기가 재시도한다 — warn은 위에서 이미 1회로 제한.
                            demote_requests.push(*id);
                        } else {
                            sc.coverage.reset();
                            sc.withheld_frames = 0;
                            sc.warned_present_fail = false;
                            // stale 부기(스펙 §5.2): Present한 갱신 영역을 반대 버퍼가 놓친
                            // 것으로 기록(부분 Present 재개 시 catch-up 재료).
                            let full = DeviceIntRect::new(
                                DeviceIntPoint::zero(),
                                DeviceIntPoint::new(sc.size.width, sc.size.height),
                            );
                            // 첫 콘텐츠 Present는 반대 버퍼가 한 번도 백버퍼였던 적이 없어
                            // 전면 stale로 시딩한다. coverage는 withhold 누적으로도 full에
                            // 도달할 수 있어(withhold는 frame_dirty를 비우고 coverage만 유지)
                            // 이번 프레임 더티가 extent의 진부분집합일 수 있기 때문. 매 전면
                            // Present마다 full 시딩하면 과복사(효율 목표 훼손)라 이 프레임에만 한정.
                            let full_seed = [full];
                            let seed: &[DeviceIntRect] = if first_content_present {
                                &full_seed
                            } else {
                                &dirty
                            };
                            sc.stale.on_present(seed, full);
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
                                    // 제3상태(리뷰 Important #1) 해제 조건 도달: content_attached가
                                    // true가 됐으니 다음에 강등이 필요해지면 재판정할 가치가 있다.
                                    sc.demote_blocked = false;
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
                                    // content-swap 시 1회: 이 스왑체인에서 부분 Present가 성립하는지
                                    // 프로브(GetBuffer(1) 개방 여부, 스펙 §3 '런타임 자격'). regen은
                                    // partial_present를 false로 되돌려 다음 content-swap에서 재프로브.
                                    sc.partial_present = !partial_present_disabled()
                                        && probe_partial_present(&sc.swapchain);
                                    if dcomp_debug() {
                                        log::info!(
                                            "[dcomp-dbg] content-swap id={:?} -> swapchain partial_present={}",
                                            id, sc.partial_present
                                        );
                                    }
                                } else {
                                    // 최종 리뷰 Minor #4: content_attached가 false로 남아 이
                                    // 분기가(coverage 전면인 한) 매 프레임 재진입한다 — warn-once로
                                    // 로그 폭주를 막는다(regen에서 재무장).
                                    if !sc.warned_setcontent_fail {
                                        warn!(
                                            "[dcomp-native] SetContent(swapchain) failed (hr=0x{:08x})",
                                            hr as u32
                                        );
                                        sc.warned_setcontent_fail = true;
                                    }
                                }
                            }
                        }
                    } else if sc.drawn_this_frame && sc.content_attached && sc.partial_present {
                        // 부분 Present(스펙 §5.2): catch-up 복사(정확 차집합) 후 매 프레임 Present1.
                        // self.d3d11_context는 self.surfaces와 별개 필드라 sc(=entry.storage 내부,
                        // self.surfaces.iter_mut() 대여 중)와 동시 대여 가능(디스조인트 필드 대여).
                        let full = DeviceIntRect::new(
                            DeviceIntPoint::zero(),
                            DeviceIntPoint::new(sc.size.width, sc.size.height),
                        );
                        let dirty = std::mem::take(&mut sc.frame_dirty);
                        let catchup = sc.stale.catchup_rects(&dirty);
                        let ok = self_copy_catchup(&self.d3d11_context, sc, &catchup)
                            && present1_partial(sc, &dirty);
                        if ok {
                            sc.coverage.reset();
                            sc.withheld_frames = 0;
                            sc.stale.on_present(&dirty, full);
                            if dcomp_debug() {
                                log::info!(
                                    "[dcomp-dbg] present-partial id={:?} dirty={} catchup={}",
                                    id, dirty.len(), catchup.len()
                                );
                            }
                        } else {
                            // 스펙 §5.3: 런타임 실패 → 즉시 강등(warn-once는 강등부에서, Task 5).
                            // 최종 리뷰 Minor #3: 위 full-Present 실패 분기와 달리 여기서는 take된
                            // `dirty`를 sc.frame_dirty로 복원하지 않고 그냥 버린다 — 비대칭이지만
                            // 안전하다: partial_present=false가 됐으니 다음 Present는 (withhold
                            // 경로를 거치든 곧장 강등되든) 반드시 누적 전면 커버리지를 요구하게
                            // 되고, 그 경로는 buffer 0의 전 픽셀을 다시 채우는 것을 전제한다 —
                            // 이번에 버린 dirty 정보가 없어도 최종적으로 픽셀 손실이 없다.
                            // partial_present가 재활성화되는 유일한 경로는 regen 재프로브뿐이고,
                            // regen은 stale까지 리셋하므로 그 시점에도 stale 부기와 어긋나지 않는다.
                            sc.partial_present = false;
                            demote_requests.push(*id);
                        }
                    } else if sc.drawn_this_frame {
                        // 부분 갱신 + 부분 Present 불가 → 종전 withhold(강등 카운터, Task 5).
                        // coverage는 비우지 않는다 — 누적 의미론 유지(스펙 §6.2-1 시딩 근거).
                        sc.frame_dirty.clear();
                        sc.withheld_frames += 1;
                        if sc.withheld_frames >= DEMOTE_AFTER_WITHHOLD {
                            // 스펙 §6.1: 첫 Present 전이면 무조건, 후면 부분 Present 불가
                            // 상태에서만 이 분기에 도달한다(가능 상태는 위 분기가 소비).
                            demote_requests.push(*id);
                        }
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
                SurfaceStorage::External(_) => {
                    // Present는 add_surface(present_external)에서 이미 완결됐다. external은
                    // 승격/강등/withhold/regen 상태머신 대상이 아니므로 여기서 할 일 없음
                    // (컴파일러가 이 arm을 강제 — 전 지점 커버리지 확인).
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
                    frame_dirty: Vec::new(),
                    stale: StaleTracker::default(),
                    partial_present: false,
                    warned_present_fail: false,
                    warned_demote_seed_fail: false,
                    demote_blocked: false,
                    warned_setcontent_fail: false,
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
                    sc.frame_dirty.clear();
                    sc.stale.reset();
                    sc.partial_present = false; // 재프로브(새 스왑체인의 GetBuffer(1) 자격 재확인)
                    // 새 스왑체인 인스턴스 — 보강 항목 2의 per-swapchain warn 가드도 재무장.
                    sc.warned_present_fail = false;
                    sc.warned_demote_seed_fail = false;
                    sc.warned_setcontent_fail = false; // 최종 리뷰 Minor #4 재무장
                    // 최종 리뷰 Important #1: regen 시점에 제3상태 판정을 다시 할 기회를
                    // 준다(fallback_virtual은 여전히 None일 수 있으므로 즉시 재검출·재차단될
                    // 수 있으나, 그 경우도 새 경고 1건 + 재차단으로 계약을 satisfy한다).
                    sc.demote_blocked = false;
                    if dcomp_debug() {
                        log::info!(
                            "[dcomp-dbg] regen id={:?} extent={}x{} anchor=({},{})",
                            surface_id, size.width, size.height, extent.min.x, extent.min.y
                        );
                    }
                }
            }
        }

        // 강등(스펙 §6, 루프 밖): 스왑체인 → 가상 서피스 복귀. 시딩으로 표시 최신성을
        // 보장한 뒤에만 storage를 교체한다(시딩 실패 시 스왑체인 동결 유지 — 화면에
        // 나타나는 콘텐츠는 항상 완전·최신이어야 한다는 불변조건).
        // 발동원(§6.1): (a) 첫 콘텐츠 Present 전 withhold 30 (b) Present 후 부분 Present
        // 불가 상태의 withhold 30 (c) 부분 Present 런타임 실패(즉시, Task 4) (d) 전면
        // Present DXGI 실패(즉시, 보강 항목 2 — §8 일반 정책).
        for surface_id in demote_requests {
            let Some(dcomp_device) = self.dcomp_device_ptr() else { break };
            let ctx = self.d3d11_context.as_ref().map(ComOwned::as_ptr);
            let Some(entry) = self.surfaces.get_mut(&surface_id) else { continue };
            let is_opaque = entry.is_opaque;
            let virtual_offset = entry.virtual_offset;
            let tile_size = entry.tile_size;
            let last_placement = entry.last_placement;
            let SurfaceStorage::SwapChain(sc) = &mut entry.storage else { continue };
            release_frame_pbuffer(&rc, sc);

            // 최종 리뷰 Important #1: 이미 제3상태로 판정·경고된 서피스는 매 프레임
            // 재시도하지 않는다(state change — content-swap 또는 regen — 까지 억제).
            if sc.demote_blocked {
                continue;
            }
            // 제3상태 판정: regen 후 pre-attach(content_attached=false)인데
            // fallback_virtual이 None이면 §6.2-1 시딩(demote_seed_into_fallback)이
            // take()?에서 구조적으로 실패한다 — regen이 fallback_virtual을 복원하지
            // 않기 때문(첫 content-swap에서 이미 소모됨). 이는 "시딩이 지속 실패"하는
            // warned_demote_seed_fail 케이스와 달리 재시도해도 절대 성공할 수 없는
            // 별개 상태이므로, 여기서 명시적으로 분리해 단발 경고만 남기고 더 이상
            // 시딩을 시도하지 않는다. 자연 회복 조건 = 다음 full-coverage Present가
            // 성공해 content_attached=true가 되는 것(그 즉시 위 §6.2-2 경로로 전환).
            if !sc.content_attached && sc.fallback_virtual.is_none() {
                // task-12b: 리사이즈 강등은 예외. 제3상태(regen 후 fallback 없음)여도
                // buffer 0에서 새 가상 서피스로 시딩해 진행한다(드래그 표시 우선 — 완벽한
                // 연속성보다 "무언가 현재 픽셀"을 가상으로 이관해 드래그를 이어가는 게 낫다;
                // 최종 연속성은 정착 시 재구축이 보장). 아래 시딩 결정이 fallback 부재를
                // 감지해 demote_seed_new_virtual(buffer 0 복사) 경로를 택한다.
                if !resize_active {
                    warn!(
                        "[dcomp-native] demote skipped: regen'd surface {:?} has no fallback; \
                         will recover at next full-coverage present",
                        surface_id
                    );
                    sc.demote_blocked = true;
                    continue;
                }
            }

            let Some(ctx) = ctx else {
                // GetImmediateContext는 COM 계약상 실패하지 않는다고 간주되지만(구조체
                // 필드 주석 참조) 방어적으로 처리 — 시딩 불가, 스왑체인 동결 유지.
                if !sc.warned_demote_seed_fail {
                    warn!(
                        "[dcomp-native] demote {:?}: no D3D11 context; keeping swapchain (frozen)",
                        surface_id
                    );
                    sc.warned_demote_seed_fail = true;
                }
                continue;
            };

            let seeded = if !sc.content_attached && sc.fallback_virtual.is_some() {
                // §6.2-1: fallback_virtual 생존 — 누적 더티 영역(타일 단위, 과대 안전)만
                // buffer 0에서 복사해 최신화. visual은 계속 fallback을 표시하므로 끊김 없음.
                // (task-12b: fallback 부재 제3상태는 위 가드가 비-리사이즈에서 이미 걸러냈고,
                // 리사이즈에서는 이 조건이 false가 되어 아래 new_virtual 경로로 간다.)
                demote_seed_into_fallback(ctx, sc, &entry.tiles, virtual_offset, tile_size)
            } else {
                // §6.2-2: 새 가상 서피스 생성 + buffer 0 전체 복사 + SetContent 원자 전환.
                demote_seed_new_virtual(
                    dcomp_device, ctx, sc, &entry.visual, last_placement, is_opaque, virtual_offset,
                )
            };
            let Some(new_virtual) = seeded else {
                if !sc.warned_demote_seed_fail {
                    warn!(
                        "[dcomp-native] demote seeding failed for {:?}; keeping swapchain (frozen)",
                        surface_id
                    );
                    sc.warned_demote_seed_fail = true;
                }
                continue;
            };

            // storage 교체(스펙 §6.4): ComOwned Drop이 옛 스왑체인을 Release한다 — visual에는
            // 이미 fallback(케이스 1, 승격 후 한 번도 SetContent 안 됨) 또는 새 virtual
            // (케이스 2, 방금 SetContent 완료)이 붙어 있으므로 안전. displayed_anchor는
            // storage와 함께 소멸하고 Virtual arm의 content_anchor=zero 산식이 자동 적용된다.
            entry.storage = SurfaceStorage::Virtual { virtual_surface: new_virtual };
            if resize_active {
                // task-12b (C): 리사이즈 강등은 병리(withhold/present 실패)가 아니라 의도된
                // 모드 전환이다. 지수 쿨다운/demote_count 증가를 적용하면 정착 후 재승격이
                // 수 초 지연되므로(§6.3), demote_count는 건드리지 않고 promote_blocked_until을
                // 리셋해 정착(resize_active=false) 즉시 자연 재승격(MIN_AGE/streak)이 가능하게
                // 한다. (정착 시 Task 12 재구축이 서피스를 새로 만들면 MIN_AGE는 재적용된다 —
                // 이는 쿨다운 페널티가 아니라 신규 서피스의 정상 히스테리시스.)
                entry.promote_blocked_until = 0;
                if dcomp_debug() {
                    log::info!("[dcomp-dbg] resize-demote id={:?} (no cooldown)", surface_id);
                }
            } else {
                entry.demote_count = entry.demote_count.saturating_add(1);
                let cooldown = demote_cooldown(entry.demote_count);
                entry.promote_blocked_until = self.frame_counter + cooldown;
                if dcomp_debug() {
                    log::info!(
                        "[dcomp-dbg] demote id={:?} count={} cooldown={}",
                        surface_id, entry.demote_count, cooldown
                    );
                }
            }
            entry.promote_streak = 0;
            entry.frame_coverage.reset();
        }

        // 레이어 컬링 없음(A-1 결정, task-6b-report.md 수정 웨이브): A-2("실측 타일
        // extent가 조밀할 때만 컬" — b4ea92c18)는 그 extent가 WR의 VIRTUAL 좌표계
        // (virtual_offset ~16384 가산, tile_virtual_rect)이고 비교 대상인 clip_rect는
        // WR이 준 DEVICE 좌표계(0..~1920)라서 contains_box가 항상 false — 즉 이
        // 서브시스템은 처음부터 한 번도 발동한 적이 없었다(월 실측 정상상태 cull=0건은
        // 좌표계 불일치로 인한 원천 비활성이었지, 표본 편향이 아니었다). 애초에 컬을
        // 되살리지 않는 이유는 좌표계 버그를 고쳐도 마찬가지다:
        //  (1) WR의 is_opaque는 "이 슬라이스의 backdrop이 불투명하다"는 힌트일 뿐 클립
        //      영역 전체가 실제로 painted됐다는 보장이 아니다 — 진단에서 확정(결함②:
        //      하트비트 점 2x2만 그리는 전면 클립 슬라이스가 하위 비디오를 컬).
        //  (2) 월 실측(정상상태) 편익이 애초에 0건 — 시작 ~1초 과도기의 42건뿐이라
        //      지속 편익이 없다.
        //  (3) virtual/device 좌표 이중성이 만드는 결함 표면적을 통째로 제거한다.
        // 따라서 add_surface가 기록한 z-order(아래→위) 그대로, 컬 없이 전부 합성한다.
        if let Some(root) = self.root_visual_ptr() {
            for id in self.frame_surfaces.iter() {
                let Some(entry) = self.surfaces.get(id) else { continue; };
                // Safety: visual/root 살아있음. 순서 = add_surface 순서(z 아래→위) 유지.
                // insertAbove 인자는 MS 문서(IDCompositionVisual::AddVisual Remarks)의
                // referenceVisual=NULL 특칙에서 직관과 반대로 동작한다: "If insertAbove is
                // TRUE, the new child visual is above no sibling, therefore it is rendered
                // BELOW all of its siblings." 즉 TRUE+NULL은 매번 최하단 삽입이라 add
                // 순서를 그대로 뒤집는다(아래→위 add → 최종 z 전체 역전). FALSE+NULL은
                // 반대로 "below no sibling → rendered ABOVE all siblings"이므로 아래→위
                // add 순서가 그대로 올바른 최종 z가 된다 — 진단 보고서(alpha-slice-diagnosis
                // §z-역전 증거) 확정.
                let hr = unsafe { (*root).AddVisual(entry.visual.as_ptr(), FALSE, ptr::null()) };
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

        // external present 프로파일: 프레임 카운트 후 1초 경과 시 [vesc-prof] 집계 라인 출력.
        if video_escape_prof() {
            self.esc_prof.frames += 1;
            self.esc_prof.maybe_flush();
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

    fn create_external_surface(&mut self, _device: &mut Device, id: NativeSurfaceId, is_opaque: bool) {
        let Some(dcomp_device) = self.dcomp_device_ptr() else {
            return;
        };
        // create_surface(:1277) 패턴과 동일하되 가상 서피스/SetContent는 만들지 않는다 —
        // 콘텐츠(스왑체인)는 첫 성공 Present 후 add_surface(present_external)에서 붙인다.
        // Safety: dcomp_device는 살아있는 IDCompositionDevice.
        let entry = unsafe {
            let mut visual_raw: *mut IDCompositionVisual = ptr::null_mut();
            let hr = (*dcomp_device).CreateVisual(&mut visual_raw);
            if hr < 0 || visual_raw.is_null() {
                warn!("[dcomp-native] create_external_surface: CreateVisual failed (hr=0x{:08x})", hr as u32);
                return;
            }
            let Some(visual) = ComOwned::from_raw(visual_raw) else {
                return;
            };
            SurfaceEntry {
                storage: SurfaceStorage::External(ExternalStorage {
                    swapchain: None,
                    swapchain_size: DeviceIntSize::zero(),
                    content_attached: false,
                    attached_external_id: None,
                    last_presented: None,
                    warned_fail: false,
                    frames_logged: 0,
                    rtv_cache: FxHashMap::default(),
                }),
                visual,
                virtual_offset: DeviceIntPoint::zero(),
                tile_size: DeviceIntSize::zero(),
                is_opaque,
                tiles: std::collections::HashSet::new(),
                frame_coverage: FrameCoverage::default(),
                promote_streak: 0,
                last_placement: None,
                drawn_frames: 0,
                demote_count: 0,
                promote_blocked_until: 0,
                frame_drawn_partial: false,
                content_valid_union: None,
            }
        };
        if dcomp_debug() {
            log::info!("[dcomp-dbg] create_external_surface id={:?} opaque={}", id, is_opaque);
        }
        self.surfaces.insert(id, entry);
    }

    fn attach_external_image(
        &mut self,
        _device: &mut Device,
        id: NativeSurfaceId,
        external_image: ExternalImageId,
    ) {
        let Some(entry) = self.surfaces.get_mut(&id) else {
            warn!("[dcomp-native] attach_external_image: unknown surface {:?}", id);
            return;
        };
        let SurfaceStorage::External(ext) = &mut entry.storage else {
            warn!("[dcomp-native] attach_external_image: surface {:?} is not external", id);
            return;
        };
        let first = ext.attached_external_id != Some(external_image.0);
        ext.attached_external_id = Some(external_image.0);
        if first && dcomp_debug() {
            log::info!(
                "[dcomp-dbg] attach_external_image id={:?} external_id={}",
                id, external_image.0
            );
        }
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
    fn external_needs_present_dedups_by_ring_and_seq() {
        assert!(external_needs_present(None, 1, 5));
        assert!(!external_needs_present(Some((1, 5)), 1, 5));
        assert!(external_needs_present(Some((1, 5)), 1, 6));   // 새 프레임
        assert!(external_needs_present(Some((1, 5)), 2, 5));   // 링 교체(소스 전환)
    }

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
    fn tile_visible_virtual_rect_clips_to_surface_clip() {
        // Task 9: 티커 실측 지오메트리 재현. virtual_offset=(16384,16384), tile_size=1024x512,
        // transform_offset=(0,0). 바닥 행(y=2) 오른쪽(x=1) 타일 = 가상 (17408,17408)-(18432,17920).
        let vo = DeviceIntPoint::new(16384, 16384);
        let ts = DeviceIntSize::new(1024, 512);
        let tile = tile_virtual_rect(vo, ts, 1, 2);
        // 티커 바 클립(device) = (0,972)-(1920,1080). 가상으로는 +virtual_offset.
        let clip = r(0, 972, 1920, 1080);
        let vis = tile_visible_virtual_rect(tile, Some(clip), (0.0, 0.0), vo);
        // 가시 영역 = 타일 ∩ 클립_virtual: x[17408,18304], y[17408,17464].
        assert_eq!(vis, r(17408, 17408, 18304, 17464));
        // 확장이 dirty(텍스트 밴드 y[0,27] = 가상 y[17408,17435])를 세로로 포함해야
        // 텍스트가 잘리지 않는다.
        assert!(vis.min.y <= 17408 && vis.max.y >= 17435);
        // 클립 없음(첫 프레임) → 전체 타일 폴백.
        assert_eq!(tile_visible_virtual_rect(tile, None, (0.0, 0.0), vo), tile);
        // 타일이 클립 밖 → 교차 비면 전체 타일 폴백(정확성 안전).
        let far = r(0, 0, 10, 10);
        assert_eq!(tile_visible_virtual_rect(tile, Some(far), (0.0, 0.0), vo), tile);
        // transform_offset 반영: transform_offset=(100,0)이면 가상 클립이 −100 이동 →
        // 가시 우변이 18304에서 18204로 100 줄어든다(좌변은 타일 좌변 17408이 지배).
        let vis2 = tile_visible_virtual_rect(tile, Some(clip), (100.0, 0.0), vo);
        assert_eq!(vis2, r(17408, 17408, 18204, 17464));
    }

    #[test]
    fn refine_opaque_clip_clips_phantom_to_drawn_region() {
        // Task 10: 티커 불투명 phantom 재현. WR 슬라이스 클립 = 바 전체(0,937)-(1904,1041),
        // 이 서피스가 실제 그린 영역(라벨 바닥 슬라이버) = (0,1024)-(121,1041).
        let wr_clip = r(0, 937, 1904, 1041);
        let drawn = r(0, 1024, 121, 1041);
        // 불투명 + 유니온 있음 → 그린 영역으로 좁힘(phantom 미도색 영역 합성 방지).
        assert_eq!(refine_opaque_clip(wr_clip, true, Some(drawn)), r(0, 1024, 121, 1041));
        // 비불투명 → 원 클립 유지(알파는 Task 9 투명 처리 담당).
        assert_eq!(refine_opaque_clip(wr_clip, false, Some(drawn)), wr_clip);
        // 유니온 미집계(None, add_surface 이전 첫 프레임) → 원 클립 폴백.
        assert_eq!(refine_opaque_clip(wr_clip, true, None), wr_clip);
        // 전면 피복 불투명(비디오 그리드: 유니온 == 클립) → 무변화(순수 월 무회귀 보장).
        assert_eq!(refine_opaque_clip(wr_clip, true, Some(wr_clip)), wr_clip);
        // 교차 비면(비정상) → 원 클립 폴백(콘텐츠 소멸 방지).
        assert_eq!(refine_opaque_clip(wr_clip, true, Some(r(5000, 5000, 5100, 5100))), wr_clip);
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
    fn surface_extent_unions_tiles() {
        let vo = DeviceIntPoint::new(16384, 16384);
        let ts = DeviceIntSize::new(1024, 512);
        let tiles: std::collections::HashSet<_> = [(0, 0), (1, 0), (0, 1)].into_iter().collect();
        let e = surface_extent(&tiles, vo, ts).unwrap();
        assert_eq!(e, r(16384, 16384, 16384 + 2048, 16384 + 1024));
        assert!(surface_extent(&Default::default(), vo, ts).is_none());
    }

    #[test]
    fn demote_cooldown_exponential_with_cap() {
        // 스펙 §6.3: BASE × 2^min(n-1,4), 상한 CAP.
        assert_eq!(demote_cooldown(1), 300);
        assert_eq!(demote_cooldown(2), 600);
        assert_eq!(demote_cooldown(3), 1200);
        assert_eq!(demote_cooldown(4), 2400);
        assert_eq!(demote_cooldown(5), 3600); // 4800 -> 3600으로 상한 클램프
        assert_eq!(demote_cooldown(10), 3600); // 시프트가 4에서 더 안 자람 -> 동일 클램프
        // 방어적 경로(정상 흐름에선 도달 안 함): n=0도 시프트 0(BASE)으로 처리.
        assert_eq!(demote_cooldown(0), 300);
    }

    #[test]
    fn collapse_dirty_if_oversized_bounds_growth() {
        // 상한 이하면 그대로 통과.
        let small = vec![r(0, 0, 10, 10), r(20, 20, 30, 30)];
        assert_eq!(collapse_dirty_if_oversized(small.clone(), 32), small);
        // 상한 초과면 바운딩 유니온 1개로 붕괴(과대=안전).
        let many: Vec<DeviceIntRect> = (0..40).map(|i| r(i * 2, 0, i * 2 + 1, 1)).collect();
        let out = collapse_dirty_if_oversized(many, 32);
        assert_eq!(out, vec![r(0, 0, 79, 1)]);
    }
}

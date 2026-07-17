# 비디오 WR 콘텐츠 패스 탈출 (External Compositor Surface) 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 비디오 YuvImage를 WR external compositor surface로 승격시켜, 영상 프레임을 디코더→plane 링→raw D3D11 변환 1-draw→비디오별 DComp 스왑체인 비주얼로 직결한다 (비디오 프레임당 ANGLE 0·WR draw 0·콘텐츠 패스 재실행 0).

**Architecture:** C 단계(`SERVO_VIDEO_ESCAPE=native`, 레이아웃 플래그만 — WR이 전용 서피스에 자기가 그림)로 프로모션/컷아웃/z 거동을 먼저 실기 검증한 뒤, A 단계(`=external`)에서 dcomp_compositor.rs의 stub 3종을 실구현한다. plane 접근은 paint_api의 신규 `VideoExternalSurfaceProvider` trait(전역 OnceLock 슬롯)을 media-thread가 구현·등록. 스펙: `docs/superpowers/specs/2026-07-17-video-wr-escape-design.md`.

**Tech Stack:** Rust (servo-paint / servo-paint-api / servo-media-thread / servo-media-player / servo-layout), raw D3D11 + HLSL(D3DCompile), DirectComposition, WebRender 0.68 (무수정 — 공식 API만).

## Global Constraints

- 게이트: `SERVO_VIDEO_ESCAPE` ∈ {`native`, `external`}, 기본 unset=off. `SERVO_COMPOSITOR_DCOMP`(=1 또는 =surface) 게이트 on일 때만 발효. Draw 모드에선 플래그 미설정(디스플레이 리스트 무변경).
- WR 크레이트 소스 수정 절대 금지 (cargo registry 직접 수정은 리빌드 안 됨 — 재발 방지 항목). vendoring 없이 공식 API만 사용.
- 빌드(mach): `cd D:\2_TechReview\20260606_multigpu_browser\servo` 후 `. .\etc\multigpu\servo_env.ps1; $ErrorActionPreference='Continue'; . .\.venv\Scripts\Activate.ps1; python mach build --release`. 출력은 파일로 리다이렉트(파이프 배압), 같은 target에 cargo 동시 2개 금지, bare `cargo check`(workspace/script 계열) 절대 금지(mozjs_sys 행).
- 테스트 명령: paint = `cargo test -p servo-paint --lib --features paint_api/no-wgl <필터>` / paint-api = `cargo test -p servo-paint-api --lib --features no-wgl <필터>` / player = `$env:PKG_CONFIG_PATH="C:/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"; cargo test -p servo-media-player --lib <필터>`.
- `components/media/media-thread`는 `#![deny(unsafe_code)]` — D3D 호출/unsafe는 전부 paint 크레이트 또는 RenderingContext 경유.
- 커밋 메시지는 한국어, 큰따옴표 금지(PowerShell here-string 함정), Claude 서명/Co-Authored-By 제외.
- HLSL/C++ 주석은 영어. Rust 주석은 기존 파일 관례(한국어 허용).
- 표출 실행은 `etc\multigpu\run_video_wall_d3d11.ps1` 사용. 로깅 폭주가 스톨을 7-10× 증폭시키므로 성능 판정 런은 진단 env 없이 clean으로.

---

### Task 1: 게이트 헬퍼 + 레이아웃 플래그 + 런처 스위치 (C 단계 골격)

**Files:**
- Modify: `components/shared/paint/rendering_context.rs` (`dcomp_native_compositor_requested()`가 정의된 파일 — 없으면 같은 크레이트 lib.rs의 해당 함수 옆)
- Modify: `components/layout/display_list/mod.rs:765-793` (yuv_image 분기)
- Modify: `etc/multigpu/run_video_wall_d3d11.ps1` (`-VideoEscape` 스위치)
- Test: paint-api 크레이트 내 `#[cfg(test)]` (같은 파일 하단)

**Interfaces:**
- Consumes: `dcomp_native_compositor_requested()` (paint_api, 기존), `wr::PrimitiveFlags::{PREFER_COMPOSITOR_SURFACE, SUPPORTS_EXTERNAL_COMPOSITOR_SURFACE}` (webrender_api display_item.rs:51,55)
- Produces: `paint_api::VideoEscapeMode { Off, Native, External }`, `paint_api::video_escape_mode() -> VideoEscapeMode` (프로세스당 1회 캐시), `parse_video_escape_token(Option<&str>) -> VideoEscapeMode` — Task 5의 dcomp_compositor도 이 mode를 읽는다.

- [ ] **Step 1: 파서 실패 테스트 작성**

`dcomp_native_compositor_requested()`가 있는 파일 하단 `#[cfg(test)] mod` (없으면 신설)에:

```rust
#[test]
fn video_escape_token_parses_native_external_only() {
    use super::{parse_video_escape_token, VideoEscapeMode};
    assert_eq!(parse_video_escape_token(Some("native")), VideoEscapeMode::Native);
    assert_eq!(parse_video_escape_token(Some("external")), VideoEscapeMode::External);
    assert_eq!(parse_video_escape_token(Some("1")), VideoEscapeMode::Off);   // 미정의 값은 off
    assert_eq!(parse_video_escape_token(Some("")), VideoEscapeMode::Off);
    assert_eq!(parse_video_escape_token(None), VideoEscapeMode::Off);
}
```

- [ ] **Step 2: 실패 확인**

Run: `cargo test -p servo-paint-api --lib --features no-wgl video_escape_token`
Expected: 컴파일 실패 (`parse_video_escape_token` 미정의)

- [ ] **Step 3: 헬퍼 구현**

```rust
/// SERVO_VIDEO_ESCAPE 게이트 모드. Native=PREFER만(C 단계), External=PREFER|SUPPORTS(최종).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoEscapeMode {
    Off,
    Native,
    External,
}

pub fn parse_video_escape_token(value: Option<&str>) -> VideoEscapeMode {
    match value {
        Some("native") => VideoEscapeMode::Native,
        Some("external") => VideoEscapeMode::External,
        _ => VideoEscapeMode::Off,
    }
}

/// DComp 네이티브 컴포지터 게이트가 켜져 있을 때만 발효. 프로세스당 1회 캐시.
pub fn video_escape_mode() -> VideoEscapeMode {
    static MODE: std::sync::OnceLock<VideoEscapeMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        if !dcomp_native_compositor_requested() {
            return VideoEscapeMode::Off;
        }
        parse_video_escape_token(std::env::var("SERVO_VIDEO_ESCAPE").ok().as_deref())
    })
}
```

주의: `dcomp_native_compositor_requested()`와 같은 모듈에 두고 pub re-export를 크레이트 루트에 추가(기존 함수의 export 방식을 그대로 따를 것).

- [ ] **Step 4: 테스트 통과 확인**

Run: `cargo test -p servo-paint-api --lib --features no-wgl video_escape_token`
Expected: PASS 1건

- [ ] **Step 5: 레이아웃 플래그 적용**

`components/layout/display_list/mod.rs` yuv_image 분기(:765). `let common = self.common_properties(...)`(:763 부근)로 만든 `common`을 비디오에 한해 수정:

```rust
if let Some(yuv_image) = fragment.yuv_image {
    // 비디오 WR 탈출 게이트: DComp on + SERVO_VIDEO_ESCAPE 설정 시에만 프로모션 힌트 부여.
    let mut common = common;
    match paint_api::video_escape_mode() {
        paint_api::VideoEscapeMode::Native => {
            common.flags |= wr::PrimitiveFlags::PREFER_COMPOSITOR_SURFACE;
        },
        paint_api::VideoEscapeMode::External => {
            common.flags |= wr::PrimitiveFlags::PREFER_COMPOSITOR_SURFACE |
                wr::PrimitiveFlags::SUPPORTS_EXTERNAL_COMPOSITOR_SURFACE;
        },
        paint_api::VideoEscapeMode::Off => {},
    }
    let yuv_data = match yuv_image.format { /* 기존 코드 무변경 */ };
    self.wr().push_yuv_image(&common, ...);  // &common만 새 바인딩을 가리키게
}
```

`CommonItemProperties`는 Copy — `let mut common = common;`이 안 되면 `.clone()`. import는 파일 상단 기존 `paint_api` 사용처 확인 후 추가 (layout은 `paint_api = { workspace = true }` 의존 보유, Cargo.toml:43).

주의: RGBA 폴백 분기(`push_image`, :795)는 플래그 미설정 유지 (YUV 비디오만 대상).

- [ ] **Step 6: 런처 스위치 추가**

`etc/multigpu/run_video_wall_d3d11.ps1`: 파라미터 블록에 `[string]$VideoEscape = ""` 추가, env 설정부(기존 -DComp/-TileSize set-or-clear 관례와 동일 위치)에:

```powershell
if ($VideoEscape -ne "") {
    $env:SERVO_VIDEO_ESCAPE = $VideoEscape
} else {
    Remove-Item Env:SERVO_VIDEO_ESCAPE -ErrorAction SilentlyContinue
}
```

- [ ] **Step 7: mach 빌드**

Run (Global Constraints의 빌드 명령, 백그라운드+로그 리다이렉트): `python mach build --release *> build_task1.log`
Expected: 성공 (exit 0, servoshell.exe 갱신)

- [ ] **Step 8: 커밋**

```powershell
git add components/shared/paint components/layout/display_list/mod.rs etc/multigpu/run_video_wall_d3d11.ps1
git commit -m @'
비디오 WR 탈출 게이트(SERVO_VIDEO_ESCAPE) + YuvImage 프로모션 플래그 + 런처 -VideoEscape 추가
'@
```

---

### Task 2: C 단계 실기 검증 (native 모드)

**Files:** 코드 변경 없음 (검증 전용). 문제 발견 시 이 태스크에서 멈추고 원인을 보고할 것 (원인 층 = 프로모션/레이아웃 플래그로 격리됨).

**Interfaces:**
- Consumes: Task 1의 `-VideoEscape native`
- Produces: C 단계 게이트 판정 (스펙 §10 C-1~3) — Task 5 진행의 전제

- [ ] **Step 1: 월 45타일 native 모드 승격 확인**

```powershell
$env:SERVO_DCOMP_DEBUG = "1"
powershell etc\multigpu\run_video_wall_d3d11.ps1 -Cols 9 -Rows 5 -Sync -1 -DComp -VideoEscape native *> wall_native.log
```
2분 재생 후 종료. 판정:
```powershell
(Select-String -Path wall_native.log -Pattern 'create_surface').Count
```
Expected: 게이트 off 런 대비 **+45개 이상의 create_surface** (비디오별 전용 서피스; WR 프로파일러 카운터 COMPOSITOR_SURFACE_UNDERLAYS에 대응). 육안: 45타일 전부 표시·lockstep(영상 내장 프레임 카운터 Total# 동일)·잔상 0.

- [ ] **Step 2: 콘텐츠 타일 무효화 정지 확인**

동일 로그에서 큰 콘텐츠 슬라이스(3840x3240급 tile_size)의 bind/dirty 로그 빈도를 비교: native 모드에선 하트비트 점 갱신분만 남아야 함 (게이트 off 런은 매 프레임 전면 dirty). 빈도가 그대로면 프로모션 미발동 — `SERVO_DCOMP_DEBUG` 로그와 WR `report_promotion_failure` 사유(RUST_LOG=warn에 표출)를 수집해 보고.

- [ ] **Step 3: 복합 페이지 2종 native 모드**

```powershell
powershell etc\multigpu\run_video_wall_d3d11.ps1 -DComp -VideoEscape native -Page tests/html/mixed_media_demo.html -Sync 6
powershell etc\multigpu\run_video_wall_d3d11.ps1 -DComp -VideoEscape native -Page tests/html/complex_media_stress.html -Sync 13
```
(함정: 그리드 외 비디오가 있으면 -Sync는 **총 비디오 수** 명시 필수.)
Expected: 전 요소 정상 — 영상 재생, JS 시계 1초 갱신, 티커 정/역방향 스크롤, 자막 블렌딩(비디오 위), PiP 표시(승격이든 폴백이든 무관), 잔상/검정 0. CopyFromScreen 픽셀 샘플 2초 간격 2회로 재생/시계 동작 판정(§3-v 확립 기법).

- [ ] **Step 4: 게이트 off 무회귀 + =surface 호환**

```powershell
powershell etc\multigpu\run_video_wall_d3d11.ps1 -Cols 9 -Rows 5 -Sync -1 -DComp            # off
powershell etc\multigpu\run_video_wall_d3d11.ps1 -Cols 6 -Rows 6 -Sync -1 -DCompSurface -VideoEscape native  # =surface 진단 모드
```
Expected: off = 기존과 동일 동작(45/45 마커, lockstep). =surface = 표시 정상(프로모션은 Native 컴포지터 공통 경로라 동일 발동).

- [ ] **Step 5: 결과 기록**

관측 수치(승격 수/실패 사유/육안)를 `.superpowers/sdd/progress.md`에 기록. 코드 변경 없으므로 커밋은 progress 기록만:
```powershell
git add .superpowers/sdd/progress.md
git commit -m @'
C 단계(native 모드) 실기 검증 결과 기록
'@
```

---

### Task 3: plane 접근 interop — trait + 링 접근자 + provider 구현/등록

**Files:**
- Modify: `components/shared/paint/lib.rs` (trait/lease/전역 슬롯 — `WebRenderExternalImageApi`(:601) 부근)
- Modify: `components/media/player/d3d11_ring.rs` (접근자 2종 + 테스트)
- Modify: `components/media/media-thread/lib.rs` (binding 확장 + provider 구현 + 등록)
- Modify: `components/script/dom/html/htmlmediaelement.rs:904-913` (update_plane 호출부에 색정보 전달)
- Test: `components/media/player/d3d11_ring.rs` 기존 `#[cfg(test)]`에 추가

**Interfaces:**
- Consumes: `D3d11VideoFrameExternalImages::binding_for(id) -> Option<D3d11PlaneBinding>` (media-thread lib.rs:177), `D3d11PlaneRings::{note_plane_lock_and_plan, presenting_plane, note_plane_unlock}` + media-thread 내부 `consume_plan` (lib.rs lock_d3d11 :582-645와 동일 규율), `RenderingContext` trait (paint_api)
- Produces (paint_api, Task 5가 소비):

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoLeaseFormat { I420, I420_10, Nv12, P010 }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoLeaseColorSpace { Rec601, Rec709, Rec2020 }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoLeaseColorRange { Limited, Full }

#[derive(Clone, Copy, Debug)]
pub struct VideoLeasePlane {
    pub texture: usize,   // AddRef 유지 중인 ID3D11Texture2D (DYNAMIC, ANGLE 디바이스 소속)
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct VideoFrameLease {
    pub ring_id: u64,
    pub planes: [Option<VideoLeasePlane>; 3],
    pub plane_count: usize,
    pub format: VideoLeaseFormat,
    pub color_space: VideoLeaseColorSpace,
    pub color_range: VideoLeaseColorRange,
    pub frame_seq: u64,   // presenting 슬롯 filled_seq — 변화 없으면 재변환 스킵
}

/// 렌더러 스레드 전용. acquire는 링 잠금(0→1)+소비 계획 실행까지 수행 — release와 반드시 짝맞춤.
pub trait VideoExternalSurfaceProvider: Send + Sync {
    fn acquire(&self, rc: &dyn RenderingContext, external_id: u64) -> Option<VideoFrameLease>;
    fn release(&self, rc: &dyn RenderingContext, ring_id: u64);
}

pub fn set_video_external_surface_provider(p: std::sync::Arc<dyn VideoExternalSurfaceProvider>);
pub fn video_external_surface_provider() -> Option<&'static std::sync::Arc<dyn VideoExternalSurfaceProvider>>;
```

- d3d11_ring 신규 접근자 (Task 내 구현): `D3d11PlaneRings::plane_count(ring_id: u64) -> Option<usize>`, `D3d11PlaneRings::presenting_filled_seq(ring_id: u64) -> Option<u64>`

- [ ] **Step 1: d3d11_ring 접근자 실패 테스트 작성**

`d3d11_ring.rs` 기존 테스트 모듈에 (기존 테스트들의 링 생성/전이 헬퍼 관례를 따라):

```rust
#[test]
fn plane_count_and_presenting_seq_accessors() {
    let ring = D3d11PlaneRings::create_ring(3, SLOT_COUNT);
    assert_eq!(D3d11PlaneRings::plane_count(ring), Some(3));
    assert_eq!(D3d11PlaneRings::plane_count(ring + 999), None);
    // 초기엔 Presenting 슬롯 없음
    assert_eq!(D3d11PlaneRings::presenting_filled_seq(ring), None);
    // 기존 테스트의 fill→consume 시퀀스 헬퍼로 슬롯 하나를 Presenting까지 전이 후:
    // (기존 테스트 stale_tracker/consume 계열이 쓰는 상태 전이 함수를 재사용할 것)
    // assert!(D3d11PlaneRings::presenting_filled_seq(ring).is_some());
    D3d11PlaneRings::remove_ring(ring);
    let _ = D3d11PlaneRings::take_removed_rings();
}
```
(전이 헬퍼가 없으면 기존 테스트의 최소 시퀀스를 복제해 Presenting 상태를 만든 뒤 seq Some 단언 추가.)

- [ ] **Step 2: 실패 확인**

Run: `$env:PKG_CONFIG_PATH="C:/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"; cargo test -p servo-media-player --lib plane_count_and_presenting`
Expected: 컴파일 실패 (접근자 미정의)

- [ ] **Step 3: 접근자 구현**

`d3d11_ring.rs`의 기존 `registry()` 잠금 관례로:

```rust
pub fn plane_count(ring_id: u64) -> Option<usize> {
    let reg = registry().lock().unwrap();
    reg.rings.get(&ring_id).map(|r| r.planes_per_slot)
}

pub fn presenting_filled_seq(ring_id: u64) -> Option<u64> {
    let reg = registry().lock().unwrap();
    let ring = reg.rings.get(&ring_id)?;
    // 기존 presenting 슬롯 탐색 관례(presenting_plane 구현부와 동일 술어) 재사용
    ring.slots.iter()
        .find(|s| matches!(s.state, SlotState::Presenting))
        .map(|s| s.filled_seq)
}
```
(필드/술어 이름은 `presenting_plane` 기존 구현(:653)을 열어 그대로 맞출 것 — 이 계획의 이름과 다르면 기존 소스가 정본.)

- [ ] **Step 4: 테스트 통과 확인 + 기존 테스트 무회귀**

Run: `$env:PKG_CONFIG_PATH="C:/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"; cargo test -p servo-media-player --lib d3d11_ring`
Expected: 신규 포함 전부 PASS

- [ ] **Step 5: paint_api에 trait/lease/전역 슬롯 추가**

Interfaces 블록의 코드를 `components/shared/paint/lib.rs`의 `WebRenderExternalImageApi` 정의(:601) 아래에 그대로 추가하고:

```rust
static VIDEO_EXTERNAL_PROVIDER: std::sync::OnceLock<std::sync::Arc<dyn VideoExternalSurfaceProvider>> =
    std::sync::OnceLock::new();

pub fn set_video_external_surface_provider(p: std::sync::Arc<dyn VideoExternalSurfaceProvider>) {
    // 단일 프로세스/단일 등록 전제 (CONSUMER_DEVICE와 동일 한계 — §4.5 다중창 이월과 정합)
    let _ = VIDEO_EXTERNAL_PROVIDER.set(p);
}

pub fn video_external_surface_provider() -> Option<&'static std::sync::Arc<dyn VideoExternalSurfaceProvider>> {
    VIDEO_EXTERNAL_PROVIDER.get()
}
```

- [ ] **Step 6: D3d11PlaneBinding에 색정보 확장 + htmlmediaelement 전달**

`media-thread/lib.rs:127-134` `D3d11PlaneBinding`에 필드 추가:

```rust
pub yuv_format: paint_api::VideoLeaseFormat,
pub color_space: paint_api::VideoLeaseColorSpace,
pub color_range: paint_api::VideoLeaseColorRange,
```

`htmlmediaelement.rs` `render_d3d11_yuv_frame`(:871)의 `update_plane` 호출부(:904-913)에서 채움 — 기존 `wr_yuv_color_space`(:227)/`wr_yuv_color_range`(:235)/`media_frame_yuv_format`(:246) 입력과 동일한 원천(`VideoFrameYuvFormat`)에서 매핑:

```rust
let lease_format = match d3d11_yuv.format {
    VideoFrameYuvFormat::I420 => paint_api::VideoLeaseFormat::I420,
    VideoFrameYuvFormat::I420_10 => paint_api::VideoLeaseFormat::I420_10,
    VideoFrameYuvFormat::NV12 => paint_api::VideoLeaseFormat::Nv12,
    VideoFrameYuvFormat::P010 => paint_api::VideoLeaseFormat::P010,
};
```
(색공간/레인지도 같은 match 방식. 필드명/원천 변수는 :871-913 실코드에 맞출 것.) 컴파일러가 나머지 struct literal 사이트를 잡아준다. script 크레이트에 paint_api 직접 의존이 없다면 media-thread가 해당 타입 3종을 `pub use paint_api::{VideoLeaseFormat, VideoLeaseColorSpace, VideoLeaseColorRange};`로 re-export하고 htmlmediaelement는 media-thread 경로로 참조.

- [ ] **Step 7: provider 구현 + 등록**

`media-thread/lib.rs` (lock_d3d11과 같은 파일 — `consume_plan` 직접 재사용):

```rust
/// DComp external surface 경로가 렌더러 스레드에서 plane 링을 직접 소비하기 위한 provider.
/// lock_d3d11(:582)과 동일한 링 잠금 규율(0→1 소비 계획, 짝맞춤 unlock)을 그대로 쓴다.
pub struct MediaVideoExternalSurfaceProvider;

impl paint_api::VideoExternalSurfaceProvider for MediaVideoExternalSurfaceProvider {
    fn acquire(&self, rc: &dyn RenderingContext, external_id: u64) -> Option<paint_api::VideoFrameLease> {
        let binding = D3d11VideoFrameExternalImages::binding_for(external_id)?;
        if let Some(plan) = D3d11PlaneRings::note_plane_lock_and_plan(binding.ring_id) {
            consume_plan(rc, binding.ring_id, plan);   // 시그니처는 lock_d3d11 호출부(:602-605)와 동일하게
        }
        let plane_count = match D3d11PlaneRings::plane_count(binding.ring_id) {
            Some(n) => n,
            None => { D3d11PlaneRings::note_plane_unlock(binding.ring_id); return None; },
        };
        let mut planes = [None; 3];
        for i in 0..plane_count {
            match D3d11PlaneRings::presenting_plane(binding.ring_id, i) {
                Some(p) => planes[i] = Some(paint_api::VideoLeasePlane {
                    texture: p.texture, width: p.width, height: p.height,
                }),
                None => { D3d11PlaneRings::note_plane_unlock(binding.ring_id); return None; },
            }
        }
        let frame_seq = match D3d11PlaneRings::presenting_filled_seq(binding.ring_id) {
            Some(s) => s,
            None => { D3d11PlaneRings::note_plane_unlock(binding.ring_id); return None; },
        };
        Some(paint_api::VideoFrameLease {
            ring_id: binding.ring_id, planes, plane_count,
            format: binding.yuv_format, color_space: binding.color_space,
            color_range: binding.color_range, frame_seq,
        })
    }

    fn release(&self, _rc: &dyn RenderingContext, ring_id: u64) {
        D3d11PlaneRings::note_plane_unlock(ring_id);
    }
}
```

등록: `initialize_image_handler`(:302-331) 말미에:
```rust
paint_api::set_video_external_surface_provider(
    std::sync::Arc::new(MediaVideoExternalSurfaceProvider));
```

주의: acquire의 모든 실패 경로에서 unlock 짝맞춤(위 코드처럼) — 링 계약은 never-skip(d3d11_ring.rs:103-128).

- [ ] **Step 8: 빌드 + 무회귀**

Run: `python mach build --release *> build_task3.log` → 성공. 이어 게이트 off로 월 스모크 1회(45/45 마커, import 실패 0) — provider 등록 자체는 무해함 확인.

- [ ] **Step 9: 커밋**

```powershell
git add components/shared/paint components/media components/script/dom/html/htmlmediaelement.rs
git commit -m @'
VideoExternalSurfaceProvider interop 추가: paint_api trait+전역 슬롯, d3d11_ring 접근자, media-thread provider 구현/등록
'@
```

---

### Task 4: raw D3D11 YUV→RGBA 변환 패스 (VideoConvertPass) + WARP E2E 테스트

**Files:**
- Create: `components/paint/dcomp_video_convert.rs`
- Modify: `components/paint/lib.rs` (mod 등록), `components/paint/Cargo.toml:68-` (winapi features에 `"d3dcompiler"`, `"d3d11_1"` 추가 — `"d3d11",` 항목 옆)
- Modify: `components/paint/dcomp_compositor.rs:344-364` (`ComOwned`를 `pub(crate)`로 승격해 재사용 — 새 모듈로 이동하지 말고 가시성만 변경, dcomp_video_convert에서 `use crate::dcomp_compositor::ComOwned;`)
- Test: `dcomp_video_convert.rs` 하단 `#[cfg(test)]` (WARP 디바이스 E2E)

**Interfaces:**
- Consumes: `paint_api::{VideoFrameLease, VideoLeaseFormat, VideoLeaseColorSpace, VideoLeaseColorRange}` (Task 3)
- Produces (Task 5가 소비):

```rust
pub(crate) struct VideoConvertPass { /* vs, ps, sampler, cbuffer, blend, raster, srv_cache, context_state */ }
impl VideoConvertPass {
    /// ANGLE 하부 D3D11 디바이스로 1회 생성. D3DCompile 런타임 컴파일(d3dcompiler_47, OS 인박스).
    pub(crate) unsafe fn new(device: *mut ID3D11Device) -> Option<VideoConvertPass>;
    /// lease의 plane들을 rtv(dst_size)로 스케일+색변환 1-draw.
    /// 내부에서 SwapDeviceContextState로 ANGLE 상태와 완전 격리 (draw 전 스왑, 후 복원).
    pub(crate) unsafe fn convert(&mut self, context: *mut ID3D11DeviceContext1,
        lease: &VideoFrameLease, rtv: *mut ID3D11RenderTargetView,
        dst_width: u32, dst_height: u32) -> bool;
    /// 링 제거 등으로 사라진 텍스처의 SRV 캐시 정리.
    pub(crate) fn evict_srvs_not_in(&mut self, live_textures: &FxHashSet<usize>);
}
/// cbuffer 파라미터 (Step 3 상수표 기반). to_cbuffer()가 HLSL 레이아웃(5×float4)으로 패킹.
pub(crate) struct ConvertParams {
    pub y_coef: f32, pub rv: f32, pub gu: f32, pub gv: f32, pub bu: f32,
    pub y_off: f32, pub rescale: f32, pub interleaved_uv: bool,
}
impl ConvertParams {
    pub(crate) fn to_cbuffer(&self) -> [[f32; 4]; 5] {
        [
            [self.y_coef, 0.0, self.rv, 0.0],                 // mat_r
            [self.y_coef, self.gu, self.gv, 0.0],             // mat_g
            [self.y_coef, self.bu, 0.0, 0.0],                 // mat_b
            [self.y_off, 0.5, 0.5, 0.0],                      // offs
            [self.rescale, if self.interleaved_uv { 1.0 } else { 0.0 }, 0.0, 0.0], // misc
        ]
    }
}
pub(crate) fn yuv_rgb_params(format: VideoLeaseFormat, space: VideoLeaseColorSpace,
    range: VideoLeaseColorRange) -> ConvertParams;
```

- [ ] **Step 1: 변환 수치 테스트 작성 (파라미터 빌더 + WARP E2E)**

`dcomp_video_convert.rs`를 테스트와 함께 신설 (모듈 뼈대 + 테스트 먼저):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // WARP 디바이스 생성 → DYNAMIC plane 텍스처 생성/기록 → convert → STAGING readback
    // (D3D11CreateDevice(D3D_DRIVER_TYPE_WARP, ...), FeatureLevel 11_0)
    // 헬퍼: fn warp_device() -> (*mut ID3D11Device, *mut ID3D11DeviceContext1)
    //       fn make_plane_r8(dev, w, h, fill: u8) -> *mut ID3D11Texture2D  (DYNAMIC|SHADER_RESOURCE, Map WRITE_DISCARD)
    //       fn make_plane_r16 / make_plane_rg8 동형
    //       fn readback_center(dev, ctx, rtv_tex, w, h) -> [u8; 4]  (CopyResource→STAGING→Map)

    #[test]
    fn convert_i420_bt709_limited_white_and_black() {
        // Y=235,U=V=128 → (255,255,255)±3 / Y=16 → (0,0,0)±3
    }
    #[test]
    fn convert_i420_bt601_limited_red() {
        // Y=81,U=90,V=240 → (255,0,0)±3
    }
    #[test]
    fn convert_nv12_interleaved_uv() {
        // RG8 plane, U=90,V=240,Y=81 (BT.601 limited) → (255,0,0)±3 — 인터리브 샘플 경로 검증
    }
    #[test]
    fn convert_i420_10_rescale_white() {
        // R16 raw Y=940, U=V=512 (10-bit limited white), rescale 65535/1023 → (255,255,255)±3
    }
    #[test]
    fn params_matrix_selection() {
        let p = yuv_rgb_params(VideoLeaseFormat::I420, VideoLeaseColorSpace::Rec709,
                               VideoLeaseColorRange::Limited);
        assert!((p.rv - 1.79274).abs() < 1e-4);
        assert!((p.rescale - 1.0).abs() < 1e-6);
        let p10 = yuv_rgb_params(VideoLeaseFormat::I420_10, VideoLeaseColorSpace::Rec709,
                                 VideoLeaseColorRange::Limited);
        assert!((p10.rescale - 64.0616).abs() < 1e-3);
    }
}
```
E2E 테스트 4건은 본문에 실제 코드로 완성할 것(헬퍼 포함 — unsafe winapi 직접 호출, dcomp_compositor.rs의 기존 unsafe 관례 준수). plane 4x4, RT 8x8, 중앙 픽셀 판독.

- [ ] **Step 2: 실패 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl video_convert 2>&1 | Select-Object -Last 20`
Expected: 컴파일 실패 (구현 미존재)

- [ ] **Step 3: 구현 — HLSL + 파이프라인 + 상수**

HLSL (const 문자열, 주석 영어):

```hlsl
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VSOut vs_main(uint id : SV_VertexID) {
    // Fullscreen triangle, no vertex buffer.
    VSOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    o.uv = uv;
    return o;
}

Texture2D texY : register(t0);
Texture2D texUorUV : register(t1);
Texture2D texV : register(t2);
SamplerState sampLinear : register(s0);
cbuffer ConvertParams : register(b0) {
    float4 mat_r;   // xyz = row, w unused
    float4 mat_g;
    float4 mat_b;
    float4 offs;    // xyz = (y_off, 0.5, 0.5) or (0, 0.5, 0.5)
    float4 misc;    // x = rescale, y = interleaved_uv (0/1)
};
float4 ps_main(VSOut i) : SV_Target {
    float y = texY.Sample(sampLinear, i.uv).r;
    float u, v;
    if (misc.y > 0.5) {
        float2 uv2 = texUorUV.Sample(sampLinear, i.uv).rg;
        u = uv2.x; v = uv2.y;
    } else {
        u = texUorUV.Sample(sampLinear, i.uv).r;
        v = texV.Sample(sampLinear, i.uv).r;
    }
    float3 yuv = float3(y, u, v) * misc.x - offs.xyz;
    float3 rgb = float3(dot(mat_r.xyz, yuv), dot(mat_g.xyz, yuv), dot(mat_b.xyz, yuv));
    return float4(saturate(rgb), 1.0);
}
```

파라미터 상수 (WR yuv.glsl·A-dyn ColorDepth 페어링과 동일 의미론):

| space/range | y_coef | rv | gu | gv | bu | y_off |
|---|---|---|---|---|---|---|
| 601 limited | 1.16438 | 1.59603 | -0.39176 | -0.81297 | 2.01723 | 16/255 |
| 709 limited | 1.16438 | 1.79274 | -0.21325 | -0.53291 | 2.11240 | 16/255 |
| 2020 limited | 1.16438 | 1.67867 | -0.18733 | -0.65042 | 2.14177 | 16/255 |
| 601 full | 1.0 | 1.402 | -0.34414 | -0.71414 | 1.772 | 0 |
| 709 full | 1.0 | 1.5748 | -0.18733 | -0.46813 | 1.8556 | 0 |
| 2020 full | 1.0 | 1.4746 | -0.16455 | -0.57135 | 1.8814 | 0 |

행 구성: `mat_r=(y_coef, 0, rv)`, `mat_g=(y_coef, gu, gv)`, `mat_b=(y_coef, bu, 0)`. y_off는 offs.x에 y_coef 적용 전 원값(16/255)으로 두고 셰이더가 `yuv - offs` 후 행렬 — 즉 행렬에 y_coef 포함, offs=(16/255, 0.5, 0.5).
rescale: I420/NV12=1.0, I420_10=65535/1023(=64.0616), P010=65535/65472(=1.000962).

파이프라인 생성(new): `D3DCompile`(vs_5_0/ps_5_0) → CreateVertexShader/CreatePixelShader, 샘플러(LINEAR, CLAMP), cbuffer(DYNAMIC 16-float), blend off, rasterizer(CULL_NONE), **`ID3D11Device1::CreateDeviceContextState`(FeatureLevel 11_0)** — QI 실패 시 None 반환(호출측 warn-once).

convert(): `SwapDeviceContextState(우리 state, &mut prev)` → RTV/viewport(dst) 설정 → SRV 획득(srv_cache: texture usize→SRV, `CreateShaderResourceView(texture, null)`) → cbuffer Map/WRITE_DISCARD로 params 기록 → `Draw(3, 0)` → `SwapDeviceContextState(prev)` 복원+prev Release. ANGLE 상태와 완전 격리.

- [ ] **Step 4: 테스트 통과 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl video_convert`
Expected: 5건 전부 PASS (WARP라 GPU 무관 CI-safe)

- [ ] **Step 5: 기존 dcomp 테스트 무회귀**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl dcomp`
Expected: 기존 8~9건 PASS (ComOwned 가시성 변경 영향 없음)

- [ ] **Step 6: 커밋**

```powershell
git add components/paint
git commit -m @'
raw D3D11 YUV 변환 패스(VideoConvertPass) 추가: HLSL 1-draw, SwapDeviceContextState 격리, WARP E2E 테스트 5종
'@
```

---

### Task 5: dcomp_compositor external surface 실구현 (stub 3종)

**Files:**
- Modify: `components/paint/dcomp_compositor.rs` — SurfaceStorage(:825-830), create_external_surface(:2374), attach_external_image(:2378), add_surface(:1731-1816), end_frame 서피스 루프(:1838-2092), destroy_surface(:2357), tests(:2418-)

**Interfaces:**
- Consumes: `paint_api::video_external_surface_provider()` + `VideoFrameLease`(Task 3), `VideoConvertPass`(Task 4), 기존 `create_composition_swapchain(size, is_opaque)`(:1144-1186), `ComOwned`, `dcomp_debug()`(:68), `self.rendering_context`(:968)
- Produces: external surface 표시 경로 완성 (`SERVO_VIDEO_ESCAPE=external` 동작). 신규 헬퍼 `fn external_needs_present(last: Option<(u64,u64)>, ring_id: u64, seq: u64) -> bool` (순수 함수, 테스트 대상)

- [ ] **Step 1: 순수 헬퍼 실패 테스트**

기존 테스트 모듈(:2419)에:

```rust
#[test]
fn external_needs_present_dedups_by_ring_and_seq() {
    assert!(external_needs_present(None, 1, 5));
    assert!(!external_needs_present(Some((1, 5)), 1, 5));
    assert!(external_needs_present(Some((1, 5)), 1, 6));   // 새 프레임
    assert!(external_needs_present(Some((1, 5)), 2, 5));   // 링 교체(소스 전환)
}
```

- [ ] **Step 2: 실패 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl external_needs_present`
Expected: 컴파일 실패

- [ ] **Step 3: storage/필드/헬퍼 구현**

```rust
// SurfaceStorage에 변형 추가 (:825)
External(ExternalStorage),

struct ExternalStorage {
    swapchain: Option<ComOwned<IDXGISwapChain1>>,   // 크기 확정 시 지연 생성
    swapchain_size: DeviceIntSize,
    content_attached: bool,                          // SetContent 1회 수행 여부
    attached_external_id: Option<u64>,               // 이번 프레임 attach된 ExternalImageId
    last_presented: Option<(u64, u64)>,              // (ring_id, frame_seq)
    warned_fail: bool,
}

fn external_needs_present(last: Option<(u64, u64)>, ring_id: u64, seq: u64) -> bool {
    last != Some((ring_id, seq))
}
```

DCompNativeCompositor에 필드 추가: `convert_pass: Option<crate::dcomp_video_convert::VideoConvertPass>`, `d3d11_context1: Option<ComOwned<ID3D11DeviceContext1>>` (maybe_create에서 QI, 실패 시 None — external 사용 시 warn-once 후 skip), `warned_no_provider: bool`.

stub 교체:

```rust
fn create_external_surface(&mut self, _device: &mut Device, id: NativeSurfaceId, is_opaque: bool) {
    // Visual 생성은 create_surface(:1277-1286)와 동일 패턴, SetContent는 스왑체인 생성 후로 이연.
    // SurfaceEntry { storage: SurfaceStorage::External(...), visual, virtual_offset: zero,
    //                tile_size: zero, is_opaque, ...기본값 }
}

fn attach_external_image(&mut self, _device: &mut Device, id: NativeSurfaceId,
    external_image: ExternalImageId) {
    // surfaces[id]가 External이면 attached_external_id = Some(external_image.0)
}
```

- [ ] **Step 4: add_surface external 분기**

add_surface(:1731) 진입부에서 storage가 External이면 전용 경로 (기존 Virtual/SwapChain 경로 위에 조기 분기):

```rust
// 1) clip_rect 비면 skip (frame_surfaces 미기록).
// 2) 브링업 계약 로그 (서피스당 최초 5프레임, dcomp_debug 게이트):
//    [dcomp-dbg] external add id=.. scale=(sx,sy) offset=(ox,oy) clip=.. src=WxH
//    기대(월): scale=(1.0,1.0), clip≈타일 rect(213²), src=1920x1080 — v1 계약: dest=clip, UV 0..1.
//    scale≠1.0 관측 시(HiDPI 등)에도 dest=clip 산식은 유효(스케일이 clip에 이미 반영됨).
// 3) provider 획득: paint_api::video_external_surface_provider() 없으면 warn-once 후
//    비주얼만 배치(마지막 프레임 유지). lease 획득 실패도 동일.
// 4) 스왑체인 보장: 없거나 clip.size() != swapchain_size이며 !resize_active면
//    create_composition_swapchain(clip.size(), entry.is_opaque)로 (재)생성,
//    content_attached=false, last_presented=None.
//    생성 실패(OOM급) 시 swapchain=None 유지+warned_fail warn-once → 다음 프레임 자연 재시도(스펙 §9).
//    (드래그 중(resize_active)엔 기존 스왑체인 유지 — 크기 안정화 후 재생성, §3-y 정합)
// 5) external_needs_present(last, lease.ring_id, lease.frame_seq)면:
//    GetBuffer(0)→CreateRenderTargetView→convert_pass.convert(ctx1, &lease, rtv, w, h)
//    →rtv/buffer Release→Present(0, 0)→last_presented 갱신.
//    첫 Present 후 content_attached=false면 SetContent(swapchain)(:1983 패턴)+true.
// 6) provider.release(rc, lease.ring_id) — 실패 경로 포함 짝맞춤.
// 7) 비주얼 배치: SetOffsetX_1/SetOffsetY_1(clip.min), SetClip_1(비주얼-로컬 clip),
//    rounded_clip_radii는 기존 warn-once 관례.
// 8) self.frame_surfaces.push(id) — z-순서는 WR 호출 순서(renderer mod.rs:6663) 그대로.
```

convert_pass 지연 초기화: 최초 external add 시 `VideoConvertPass::new(self.d3d11_device)` — None이면 warn-once 후 skip-present(비주얼만).

- [ ] **Step 5: end_frame/destroy 통합**

- end_frame 서피스 루프(:1838-2092)의 match에 `SurfaceStorage::External(_) => {}` arm 추가 (승격/강등/withhold 스캔 대상 아님 — Present는 add_surface에서 완료).
- destroy_surface(:2357): External은 `surfaces.remove(&id)`로 충분(ComOwned Drop이 visual/swapchain Release; frame_pbuffer 없음). SRV 캐시는 convert_pass에 남을 수 있으나 texture AddRef는 링 소유 — 주기 정리로 충분: end_frame 말미에 60프레임마다 `convert_pass.evict_srvs_not_in(현존 lease 텍스처 집합)` 대신 **단순화: 링 제거 시점을 모르는 컴포지터 특성상, SRV 캐시 상한(128) 초과 시 전체 clear**로 구현(주석에 근거 명시).
- 진단: `SERVO_DCOMP_DEBUG=1`에 external 수명주기(create/attach 첫회/스왑체인 재생성/Present 수) 로그 — 기존 `[dcomp-dbg]` 관례.

- [ ] **Step 6: 테스트 통과 + 무회귀**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl` (전체)
Expected: 신규 external_needs_present + 기존 dcomp/video_convert 전부 PASS

- [ ] **Step 7: mach 빌드**

Run: `python mach build --release *> build_task5.log`
Expected: 성공

- [ ] **Step 8: 커밋**

```powershell
git add components/paint
git commit -m @'
DComp external compositor surface 실구현: 비디오별 스왑체인 비주얼 직결, 변환 1-draw, 세대 dedup, 드래그 중 재생성 억제
'@
```

---

### Task 6: A 단계 통합 검증 (external 모드)

**Files:** 코드 변경 없음 (스펙 §10 A-게이트). 결함 발견 시 수정 후 해당 태스크 테스트 재실행.

**Interfaces:**
- Consumes: Task 1~5 전부
- Produces: A 단계 게이트 판정 → Task 7(패키지) 전제

- [ ] **Step 1: 월 45타일 external**

```powershell
powershell etc\multigpu\run_video_wall_d3d11.ps1 -Cols 9 -Rows 5 -Sync -1 -DComp -VideoEscape external
```
Expected: 45/45 마커, lockstep ±1(내장 프레임 카운터 — 샘플 간격이 소스 루프 주기 배수면 앨리어싱 함정 주의), 육안 색/방향 정상, 잔상 0. 이어 30분 소크: 메모리 플랫(WS 관측), gapless 루프 경계 무결, 크래시 0.

- [ ] **Step 2: 브링업 계약 로그 판독**

`$env:SERVO_DCOMP_DEBUG=1` 단발 런에서 `[dcomp-dbg] external add` 로그의 scale/clip/src 값이 Task 5 Step 4의 기대와 일치하는지 확인. 불일치(예: clip이 소스 크기 스케일로 관측) 시 **중지하고 관측값을 보고** — v1 산식(dest=clip)의 전제 검증이 목적.

- [ ] **Step 3: 복합 2종 + 폴백 혼재**

```powershell
powershell etc\multigpu\run_video_wall_d3d11.ps1 -DComp -VideoEscape external -Page tests/html/mixed_media_demo.html -Sync 6
powershell etc\multigpu\run_video_wall_d3d11.ps1 -DComp -VideoEscape external -Page tests/html/complex_media_stress.html -Sync 13
powershell etc\multigpu\run_video_wall_d3d11.ps1 -DComp -VideoEscape external -Page tests/html/video_4k_grid_play.html -DecoderThreads 6   # 회전=ComplexTransform 전부 폴백 경로
```
Expected: mixed/stress 전 요소 정상(시계·티커·자막·PiP), 4K 회전 페이지는 폴백(현 A-dyn 경로)으로 전 타일 회전 표시 정상.

- [ ] **Step 4: 10-bit + 리사이즈/드래그 + WebGPU + off/=surface**

```powershell
powershell etc\multigpu\run_video_wall_d3d11.ps1 -Cols 2 -Rows 2 -DComp -VideoEscape external -Src tests/jellyfish-60-mbps-hd-hevc-10bit.mp4
```
Expected: 10-bit 색 정상(I420_10 rescale 경로). 창 드래그/리사이즈 §3-y 시나리오: 드래그 중 영상 연속 재생, 종료 후 잔상/블랙 0. WebGPU 월 무회귀(기존 절차), 게이트 off 무회귀, `-DCompSurface -VideoEscape external` 표시 정상.

- [ ] **Step 5: PresentMon + TileSize 재측정**

`D:\PresentMon-2.3.1-x64.exe` (관리자, servoshell 전면 필수 — 가림 시 가짜 ~9fps 함정): external 모드에서 비디오별 스왑체인 Present 카데이던스 관측(총 Present 수가 45×30fps 근사 + 콘텐츠 소량). `-TileSize 3840x3240` 유무 A/B로 운영 레시피에서 TileSize 확대 필요 여부 판정 — 결과를 progress.md에 기록.

- [ ] **Step 6: 결과 기록 커밋**

```powershell
git add .superpowers/sdd/progress.md
git commit -m @'
A 단계(external 모드) 통합 검증 결과 기록
'@
```

---

### Task 7: 패키지/AMD 가이드/문서 마감

**Files:**
- Modify: `D:\ServoWallPackage\run_wall.ps1` (-VideoEscape 스위치 + AMD 3중 A/B 가이드 주석), `docs/superpowers/specs/2026-07-17-video-wr-escape-design.md` (§12 구현 결과/이탈 추가)
- 재생성: `D:\ServoWallPackage.zip`

**Interfaces:**
- Consumes: Task 6 게이트 통과, 최신 servoshell.exe
- Produces: AMD 실측 인계물 (사용자 몫)

- [ ] **Step 1: 패키지 런처 갱신**

`D:\ServoWallPackage\run_wall.ps1`에 Task 1 Step 6과 동일한 `-VideoEscape` set-or-clear 블록 추가 + 헤더 주석에 AMD 3중 A/B 절차:

```
# AMD A/B (순수 월 페이지로 측정, 복합 페이지는 정확성 확인용):
#   1) .\run_wall.ps1 -Cols 9 -Rows 5 -Sync -1 -DComp                          (기준)
#   2) .\run_wall.ps1 -Cols 9 -Rows 5 -Sync -1 -DComp -VideoEscape native      (콘텐츠 패스 소멸 이득)
#   3) .\run_wall.ps1 -Cols 9 -Rows 5 -Sync -1 -DComp -VideoEscape external    (ANGLE 제출세까지 소멸)
# 판독: 3>2>1 이면 제출 오버헤드 가설 확증. 3에서 GPU%/fps 동시 관측.
# 주의: external에서 DWM 비주얼 46개 합성 비용이 구형 GPU에서 역효과일 수 있음 — 1과 3의 GPU% 비교 필수.
```

- [ ] **Step 2: 패키지 재생성**

`D:\ServoWallPackage`에 최신 `target\release\servoshell.exe` 복사(기존 DLL/resources/tests 구성 유지 — 신규 DLL 의존 없음: d3dcompiler_47은 OS 인박스), zip 재생성:
```powershell
Compress-Archive -Path D:\ServoWallPackage\* -DestinationPath D:\ServoWallPackage.zip -Force
```
Expected: zip 갱신(≈1.1GB대), 패키지 단독 실행 스모크 1회(-DComp -VideoEscape external 2x2).

- [ ] **Step 3: 스펙 §12 구현 결과 기록**

스펙 문서에 §12 추가: 구현 커밋 목록, 스펙 대비 이탈(예: v1 dest=clip 계약과 부분 클립 한계, SRV 캐시 정리 방식), 검증 수치(Task 2/6 게이트 결과), AMD 판독 가이드 포인터.

- [ ] **Step 4: 커밋**

```powershell
git add docs/superpowers/specs/2026-07-17-video-wr-escape-design.md
git commit -m @'
비디오 WR 탈출 사이클 마감: 스펙 구현 결과 기록, 패키지 재생성(-VideoEscape 포함)
'@
```
(패키지 파일은 리포 밖 — 커밋 대상 아님.)

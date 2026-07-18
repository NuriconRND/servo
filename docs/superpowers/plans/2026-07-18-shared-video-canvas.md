# 공유 비디오 캔버스 (SERVO_VIDEO_ESCAPE=canvas) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** underlay external 비디오 전체를 창 크기 단일 premultiplied flip 스왑체인(공유 캔버스)에 매 컴포짓 전량 재드로우하고 프레임당 Present1 1회로 마감해, AMD Present×N 직렬화 절벽(§3-ac)을 해소한다.

**Architecture:** 스펙 `docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md` 승인본. WR/레이아웃 변경 0 — 게이트 값 `canvas`는 external과 동일 플래그(PREFER|SUPPORTS)를 걸고, 차이는 dcomp_compositor의 external surface 처리뿐. add_surface 호출 순서로 underlay/overlay를 판정(`content_seen`), underlay는 기록만 하고 start_compositing의 `canvas_flush()`가 acquire→더티 판정(순수 함수)→전체 투명 클리어→add 순서 전량 draw(`convert_to_rect`)→Present1(더티 힌트)를 수행한다. overlay(PiP류 ≤4)는 기존 per-video 경로 무변경.

**Tech Stack:** Rust (servo-paint crate), D3D11/DXGI/DirectComposition (winapi), WARP E2E 유닛테스트, PowerShell 런처.

## Global Constraints

- 브랜치 `multigpu-tiled-wall`, 리포 `D:\2_TechReview\20260606_multigpu_browser\servo`. **푸시 금지(사용자 지시: 보류) — 로컬 커밋만.**
- 커밋 메시지는 한국어, Claude 서명/Co-Authored-By 금지 (사용자 전역 CLAUDE.md).
- 이 Rust 파일들의 기존 주석 관례는 한국어 — 따른다.
- **빌드 (PowerShell, 매번 이 순서 그대로):**
  ```powershell
  cd D:\2_TechReview\20260606_multigpu_browser\servo
  . .\etc\multigpu\servo_env.ps1
  $ErrorActionPreference = 'Continue'   # CRITICAL: servo_env가 Stop으로 바꿔놓음
  . .\.venv\Scripts\Activate.ps1        # 시스템 python에 toml 없음
  python mach build --release
  ```
  incremental paint 변경 ≈ 1–3분. 산출물 `target\release\servoshell.exe`. **빌드 전 잔존 servoshell.exe kill 확인**(`Stop-Process -Name servoshell -Force -ErrorAction SilentlyContinue` — 링크 잠금 os error 5 방지).
- **유닛테스트 정확 명령:** `cargo test -p servo-paint --lib --features paint_api/no-wgl <필터>` (feature 없으면 의존 크레이트 컴파일 실패).
- **★DYNAMIC(WRITE_DISCARD) plane 텍스처의 뷰(SRV/RTV/UAV) 캐싱 절대 금지★** — rename마다 뷰가 낡아 NVIDIA 드라이버 AV(근본수정 061c7f5d0). plane SRV는 매 draw 신선 생성. 스왑체인 백버퍼 RTV 캐시는 반대로 안전(rename 없음).
- 게이트 기본 off: `SERVO_VIDEO_ESCAPE` 미설정/기타 값 = Off. `canvas`는 `SERVO_COMPOSITOR_DCOMP` 게이트 하에서만 발효(기존 `video_escape_mode()`가 이미 보장). off/native/external 거동 무변경.
- 표출 런처: `etc\multigpu\run_video_wall_d3d11.ps1` (`-VideoEscape` 값은 env로 그대로 전달되므로 `canvas`는 기계적으로 이미 동작 — 문서/가이드만 갱신).
- 서브에이전트가 mach 빌드를 백그라운드로 걸면 반환 시 고아 종료됨 — 빌드는 포그라운드로 돌리거나 컨트롤러가 수행(§3-aa 함정 (3)).

---

### Task 1: 게이트 — `VideoEscapeMode::Canvas`

**Files:**
- Modify: `components/shared/paint/rendering_context.rs:56-70` (enum + 파서)
- Modify: `components/layout/display_list/mod.rs:768-777` (match arm)
- Test: `components/paint/dcomp_compositor.rs` 기존 `#[cfg(test)] mod tests` (line ~3050대, `external_needs_present_dedups_by_ring_and_seq` 옆)

**Interfaces:**
- Consumes: 기존 `parse_video_escape_token(Option<&str>) -> VideoEscapeMode`, `video_escape_mode()`.
- Produces: `VideoEscapeMode::Canvas` variant — Task 4가 `video_escape_mode() == VideoEscapeMode::Canvas`로 캔버스 모드를 판정한다. 레이아웃은 Canvas를 External과 동일 취급(PREFER|SUPPORTS).

- [ ] **Step 1: 실패하는 테스트 작성**

`components/paint/dcomp_compositor.rs`의 tests 모듈에 추가 (기존 `external_needs_present_dedups_by_ring_and_seq` 테스트 아래):

```rust
    #[test]
    fn parse_video_escape_token_accepts_canvas() {
        use paint_api::rendering_context::{parse_video_escape_token, VideoEscapeMode};
        assert_eq!(parse_video_escape_token(Some("canvas")), VideoEscapeMode::Canvas);
        // 기존 값 무변경 보증
        assert_eq!(parse_video_escape_token(Some("native")), VideoEscapeMode::Native);
        assert_eq!(parse_video_escape_token(Some("external")), VideoEscapeMode::External);
        assert_eq!(parse_video_escape_token(Some("bogus")), VideoEscapeMode::Off);
        assert_eq!(parse_video_escape_token(None), VideoEscapeMode::Off);
    }
```

(참고: tests 모듈 상단 use에 `paint_api`가 없으면 함수 안 use로 충분 — 위 코드가 그렇게 되어 있음.)

- [ ] **Step 2: 테스트 실패 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl parse_video_escape`
Expected: **컴파일 실패** — `no variant named 'Canvas' found for enum 'VideoEscapeMode'`

- [ ] **Step 3: 최소 구현**

`components/shared/paint/rendering_context.rs` — enum과 파서에 variant 추가:

```rust
/// `SERVO_VIDEO_ESCAPE` 게이트 모드. Native=PREFER만(C 단계), External=PREFER|SUPPORTS(최종),
/// Canvas=External과 동일 플래그 + 컴포지터가 underlay를 공유 캔버스 1스왑체인으로 합침
/// (스펙 2026-07-18-shared-video-canvas-design.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoEscapeMode {
    Off,
    Native,
    External,
    Canvas,
}

pub fn parse_video_escape_token(value: Option<&str>) -> VideoEscapeMode {
    match value {
        Some("native") => VideoEscapeMode::Native,
        Some("external") => VideoEscapeMode::External,
        Some("canvas") => VideoEscapeMode::Canvas,
        _ => VideoEscapeMode::Off,
    }
}
```

`components/layout/display_list/mod.rs:768-777` — Canvas를 External과 같은 arm으로 (기존 match를 아래로 교체):

```rust
            match paint_api::rendering_context::video_escape_mode() {
                paint_api::rendering_context::VideoEscapeMode::Native => {
                    common.flags |= PrimitiveFlags::PREFER_COMPOSITOR_SURFACE;
                },
                paint_api::rendering_context::VideoEscapeMode::External |
                paint_api::rendering_context::VideoEscapeMode::Canvas => {
                    common.flags |= PrimitiveFlags::PREFER_COMPOSITOR_SURFACE |
                        PrimitiveFlags::SUPPORTS_EXTERNAL_COMPOSITOR_SURFACE;
                },
                paint_api::rendering_context::VideoEscapeMode::Off => {},
            }
```

주의: `VideoEscapeMode`에 match하는 다른 지점이 있는지 확인 — `Grep pattern:"VideoEscapeMode::" path:components` 로 전수 확인하고, 새 variant 때문에 non-exhaustive 컴파일 에러가 나는 곳은 External과 동일 arm으로 묶는다(현재 알려진 곳: layout 1곳뿐).

- [ ] **Step 4: 테스트 통과 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl parse_video_escape`
Expected: PASS (1 passed)

- [ ] **Step 5: 커밋**

```powershell
git add components/shared/paint/rendering_context.rs components/layout/display_list/mod.rs components/paint/dcomp_compositor.rs
git commit -m "공유 비디오 캔버스 게이트: SERVO_VIDEO_ESCAPE=canvas 모드값 추가(레이아웃 플래그는 external과 동일)"
```

---

### Task 2: `VideoConvertPass::convert_to_rect` — dest-rect(부분 뷰포트) draw

**Files:**
- Modify: `components/paint/dcomp_video_convert.rs` (`convert` :424, `draw_locked` :462, tests 모듈)

**Interfaces:**
- Consumes: 기존 `VideoConvertPass::convert(context, lease, rtv, dst_width, dst_height) -> bool`, `draw_locked(...)`, 테스트 헬퍼 `warp_device()/make_plane_r8()/make_render_target()/readback_center()/assert_close()/lease3()`.
- Produces: `pub(crate) unsafe fn convert_to_rect(&mut self, context: *mut ID3D11DeviceContext1, lease: &VideoFrameLease, rtv: *mut ID3D11RenderTargetView, dst_left: i32, dst_top: i32, dst_width: u32, dst_height: u32) -> bool` — Task 4의 `canvas_flush`가 캔버스 백버퍼의 각 비디오 rect에 draw할 때 사용. 뷰포트가 rect를 벗어나도 RT 밖 픽셀은 래스터라이저가 버리므로 안전(드래그 중 클램프 근거). 테스트 헬퍼 `readback_px(dev, ctx, rtv_tex, w, h, x, y) -> [u8;4]`.

- [ ] **Step 1: 실패하는 테스트 작성**

tests 모듈에 픽셀 좌표 readback 헬퍼와 테스트 추가. 먼저 `readback_center`를 일반화한 `readback_px` 추가(중복 제거 — `readback_center`는 위임으로 교체):

```rust
    /// rtv_tex의 (x,y) 픽셀을 STAGING readback으로 읽어 (R,G,B,A)로 반환(BGRA 스위즐).
    fn readback_px(
        dev: *mut ID3D11Device,
        ctx: *mut ID3D11DeviceContext1,
        rtv_tex: *mut ID3D11Texture2D,
        w: u32,
        h: u32,
        x: u32,
        y: u32,
    ) -> [u8; 4] {
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ,
                MiscFlags: 0,
            };
            let mut staging: *mut ID3D11Texture2D = ptr::null_mut();
            let hr = (*dev).CreateTexture2D(&desc, ptr::null(), &mut staging);
            assert!(hr >= 0, "CreateTexture2D(staging) failed (hr=0x{:08x})", hr as u32);
            (*ctx).CopyResource(staging as *mut ID3D11Resource, rtv_tex as *mut ID3D11Resource);
            let mut mapped: D3D11_MAPPED_SUBRESOURCE = std::mem::zeroed();
            let hr = (*ctx).Map(staging as *mut ID3D11Resource, 0, D3D11_MAP_READ, 0, &mut mapped);
            assert!(hr >= 0, "Map(staging) failed (hr=0x{:08x})", hr as u32);
            let row = (mapped.pData as *const u8).add(y as usize * mapped.RowPitch as usize);
            let px = row.add(x as usize * 4);
            let (b, g, r, a) = (*px, *px.add(1), *px.add(2), *px.add(3));
            (*ctx).Unmap(staging as *mut ID3D11Resource, 0);
            (*(staging as *mut IUnknown)).Release();
            [r, g, b, a]
        }
    }
```

기존 `readback_center` 본문을 위임으로 교체:

```rust
    fn readback_center(
        dev: *mut ID3D11Device,
        ctx: *mut ID3D11DeviceContext1,
        rtv_tex: *mut ID3D11Texture2D,
        w: u32,
        h: u32,
    ) -> [u8; 4] {
        readback_px(dev, ctx, rtv_tex, w, h, w / 2, h / 2)
    }
```

테스트 본체 — 16×8 타깃을 투명 클리어 후, 좌반(0,0,8x8)에 흰색 비디오·우반(8,0,8x8)에 검정 비디오를 `convert_to_rect`로 draw. 각 반의 중앙 색과, 세로 아래 여백이 없으므로 **클리어 검증은 다른 케이스**로: 16×16 타깃 상반부만 draw하고 하반부가 투명(A=0)인지 확인:

```rust
    #[test]
    fn convert_to_rect_draws_two_videos_side_by_side_and_preserves_clear() {
        let (dev, ctx1) = warp_device();
        let mut pass = unsafe { VideoConvertPass::new(dev) }.expect("VideoConvertPass::new failed on WARP");
        // 16x16 캔버스형 타깃: 상단에 좌(흰)/우(검) 두 비디오, 하반부는 클리어(투명) 유지.
        let (rtv, rtv_tex) = make_render_target(dev, 16, 16);
        unsafe { (*ctx1).ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 0.0]) };

        // 좌상 (0,0)-(8,8): 흰색 (Y=235,U=V=128, BT.709 limited)
        let yw = make_plane_r8(dev, 4, 4, 235);
        let u = make_plane_r8(dev, 4, 4, 128);
        let v = make_plane_r8(dev, 4, 4, 128);
        let lease_w = lease3(yw, u, v, VideoLeaseFormat::I420, VideoLeaseColorSpace::Rec709, VideoLeaseColorRange::Limited);
        let ok = unsafe { pass.convert_to_rect(ctx1, &lease_w, rtv, 0, 0, 8, 8) };
        assert!(ok);

        // 우상 (8,0)-(16,8): 검정 (Y=16)
        let yb = make_plane_r8(dev, 4, 4, 16);
        let lease_b = lease3(yb, u, v, VideoLeaseFormat::I420, VideoLeaseColorSpace::Rec709, VideoLeaseColorRange::Limited);
        let ok = unsafe { pass.convert_to_rect(ctx1, &lease_b, rtv, 8, 0, 8, 8) };
        assert!(ok);

        // 좌상 중앙 = 흰색, 우상 중앙 = 검정 (draw 위치·독립성 검증)
        assert_close(readback_px(dev, ctx1, rtv_tex, 16, 16, 4, 4), [255, 255, 255], 3);
        assert_close(readback_px(dev, ctx1, rtv_tex, 16, 16, 12, 4), [0, 0, 0], 3);
        // 우상 draw가 좌상을 덮지 않았는가(뷰포트 격리) — 좌상 재확인은 위와 동일 픽셀로 이미 커버.
        // 하반부는 클리어 그대로 투명(A=0) — 전량 재드로우 캔버스의 빈 영역 계약.
        let below = readback_px(dev, ctx1, rtv_tex, 16, 16, 8, 12);
        assert_eq!(below[3], 0, "cleared area must stay transparent, got {:?}", below);
    }
```

- [ ] **Step 2: 테스트 실패 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl convert_to_rect`
Expected: **컴파일 실패** — `no method named 'convert_to_rect'`

- [ ] **Step 3: 최소 구현**

`draw_locked`에 뷰포트 원점 파라미터 2개를 추가하고(호출부 2곳 갱신), `convert_to_rect`를 추가한다.

`draw_locked` 시그니처/뷰포트만 변경 (그 외 본문 무변경):

```rust
    unsafe fn draw_locked(
        &mut self,
        context: *mut ID3D11DeviceContext1,
        lease: &VideoFrameLease,
        rtv: *mut ID3D11RenderTargetView,
        dst_left: i32,
        dst_top: i32,
        dst_width: u32,
        dst_height: u32,
        bind_static_state: bool,
    ) -> bool {
```

뷰포트 구성부(:523-530)를 아래로 교체 — 풀스크린 트라이앵글은 뷰포트 전체를 덮고 래스터가 뷰포트로 클립하므로 시저 불요, RT 경계 밖 픽셀은 자동 폐기(드래그 클램프 안전 근거):

```rust
        let viewport = D3D11_VIEWPORT {
            TopLeftX: dst_left as f32,
            TopLeftY: dst_top as f32,
            Width: dst_width as f32,
            Height: dst_height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
```

`convert()` 내부의 `draw_locked` 호출 2곳(:435, :443)은 `self.draw_locked(context, lease, rtv, 0, 0, dst_width, dst_height, false/true)`로 갱신.

새 메서드(`convert` 바로 아래에 추가 — 배치/비배치 상태 스왑 로직은 convert와 동일해야 하므로 convert에 위임):

```rust
    /// convert의 dest-rect 변형(공유 캔버스용, 스펙 2026-07-18 §5.4): rtv의
    /// (dst_left,dst_top)-(+dst_width,+dst_height) 영역에만 draw한다. 뷰포트 오프셋 외
    /// 의미론(색변환/배치/상태 격리)은 convert와 동일.
    pub(crate) unsafe fn convert_to_rect(
        &mut self,
        context: *mut ID3D11DeviceContext1,
        lease: &VideoFrameLease,
        rtv: *mut ID3D11RenderTargetView,
        dst_left: i32,
        dst_top: i32,
        dst_width: u32,
        dst_height: u32,
    ) -> bool {
        if self.batch.is_some() {
            return self.draw_locked(context, lease, rtv, dst_left, dst_top, dst_width, dst_height, false);
        }
        let mut prev_state: *mut ID3DDeviceContextState = ptr::null_mut();
        (*context).SwapDeviceContextState(self.context_state.as_ptr(), &mut prev_state);
        let ok = self.draw_locked(context, lease, rtv, dst_left, dst_top, dst_width, dst_height, true);
        let mut discarded: *mut ID3DDeviceContextState = ptr::null_mut();
        (*context).SwapDeviceContextState(prev_state, &mut discarded);
        if !discarded.is_null() {
            (*(discarded as *mut IUnknown)).Release();
        }
        if !prev_state.is_null() {
            (*(prev_state as *mut IUnknown)).Release();
        }
        ok
    }
```

리팩토링 여지: `convert`의 본문을 `self.convert_to_rect(context, lease, rtv, 0, 0, dst_width, dst_height)` 위임으로 교체해 스왑 로직 중복을 없앤다(동작 등가 — 기존 convert 테스트 5종이 회귀 그물).

- [ ] **Step 4: 전체 변환 테스트 통과 확인 (기존 5종 + 신규 1종)**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl convert`
Expected: PASS — 기존 `convert_i420_bt709_limited_white_and_black`, `convert_i420_bt601_limited_red`, `convert_nv12_interleaved_uv`, `convert_i420_10_rescale_white`, `convert_batched_matches_unbatched` 전부 + `convert_to_rect_draws_two_videos_side_by_side_and_preserves_clear` PASS

- [ ] **Step 5: 커밋**

```powershell
git add components/paint/dcomp_video_convert.rs
git commit -m "VideoConvertPass dest-rect 변형(convert_to_rect): 공유 캔버스용 부분 뷰포트 draw + WARP E2E"
```

---

### Task 3: 캔버스 더티 판정 순수 함수 + Present1 헬퍼 일반화

**Files:**
- Modify: `components/paint/dcomp_compositor.rs` (`present1_partial` :616 일반화, 순수 함수/구조체 추가는 `external_needs_present` :940 아래, 테스트는 tests 모듈)

**Interfaces:**
- Consumes: `DeviceIntRect`, `NativeSurfaceId`(Copy+Eq+Hash), `FxHashMap`, 기존 `present1_partial(&SwapChainStorage, &[DeviceIntRect])`, `MAX_PRESENT_DIRTY_RECTS` 상수.
- Produces (Task 4가 사용):
  - `struct CanvasFrameItem { id: NativeSurfaceId, rect: DeviceIntRect, updated: bool, drawable: bool }`
  - `fn canvas_dirty_rects(prev: &FxHashMap<NativeSurfaceId, (DeviceIntRect, bool)>, current: &[CanvasFrameItem]) -> Vec<DeviceIntRect>` — prev 값 = (지난 표시 rect, 지난 프레임 draw 성공 여부)
  - `fn present1_with_dirty(swapchain: *mut IDXGISwapChain1, size: DeviceIntSize, dirty: &[DeviceIntRect]) -> bool` — 기존 `present1_partial`은 이 함수 위임으로 축소(힌트 >16/공집합 = 전체 규칙 그대로)

- [ ] **Step 1: 실패하는 테스트 작성**

tests 모듈에 추가:

```rust
    #[test]
    fn canvas_dirty_updated_and_new_items() {
        use webrender::NativeSurfaceId;
        let r = |x: i32| DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(x, 0), DeviceIntSize::new(10, 10));
        let mut prev = FxHashMap::default();
        prev.insert(NativeSurfaceId(1), (r(0), true));
        let current = vec![
            // id1: 자리·draw상태 동일, 세대 갱신 → rect 1건
            CanvasFrameItem { id: NativeSurfaceId(1), rect: r(0), updated: true, drawable: true },
            // id2: 신규 등장 → rect 1건
            CanvasFrameItem { id: NativeSurfaceId(2), rect: r(20), updated: false, drawable: true },
        ];
        let dirty = canvas_dirty_rects(&prev, &current);
        assert_eq!(dirty, vec![r(0), r(20)]);
    }

    #[test]
    fn canvas_dirty_moved_rect_includes_old_and_new() {
        use webrender::NativeSurfaceId;
        let r = |x: i32| DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(x, 0), DeviceIntSize::new(10, 10));
        let mut prev = FxHashMap::default();
        prev.insert(NativeSurfaceId(1), (r(0), true));
        let current = vec![
            // 이동(스케일/이동 애니): 세대 무갱신이어도 옛 자리+새 자리 둘 다 더티
            CanvasFrameItem { id: NativeSurfaceId(1), rect: r(5), updated: false, drawable: true },
        ];
        assert_eq!(canvas_dirty_rects(&prev, &current), vec![r(0), r(5)]);
    }

    #[test]
    fn canvas_dirty_vacated_and_draw_state_transition() {
        use webrender::NativeSurfaceId;
        let r = |x: i32| DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(x, 0), DeviceIntSize::new(10, 10));
        let mut prev = FxHashMap::default();
        prev.insert(NativeSurfaceId(1), (r(0), true));  // 소멸 예정
        prev.insert(NativeSurfaceId(2), (r(20), false)); // 지난 프레임 draw 실패(구멍)
        let current = vec![
            // id2: 같은 자리·세대 무갱신이지만 draw 가능 전이 → 구멍 복구 위해 더티
            CanvasFrameItem { id: NativeSurfaceId(2), rect: r(20), updated: false, drawable: true },
        ];
        // id1 소멸 → 옛 자리(투명 전환) 더티. 순서: current 규칙들 먼저, 소멸분 나중.
        assert_eq!(canvas_dirty_rects(&prev, &current), vec![r(20), r(0)]);
    }

    #[test]
    fn canvas_dirty_empty_when_nothing_changed() {
        use webrender::NativeSurfaceId;
        let r = DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(0, 0), DeviceIntSize::new(10, 10));
        let mut prev = FxHashMap::default();
        prev.insert(NativeSurfaceId(1), (r, true));
        let current = vec![
            CanvasFrameItem { id: NativeSurfaceId(1), rect: r, updated: false, drawable: true },
        ];
        assert!(canvas_dirty_rects(&prev, &current).is_empty());
    }
```

(주의: `NativeSurfaceId`/`DeviceIntPoint`의 실제 import 경로는 이 파일 상단 use 블록을 따른다 — tests 모듈이 이미 `super::*`를 쓰면 위 `use webrender::NativeSurfaceId;`는 불필요할 수 있음. 컴파일 에러 기준으로 맞춘다.)

- [ ] **Step 2: 테스트 실패 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl canvas_dirty`
Expected: **컴파일 실패** — `cannot find struct CanvasFrameItem` / `cannot find function canvas_dirty_rects`

- [ ] **Step 3: 최소 구현**

`external_needs_present`(:940) 아래에 추가:

```rust
/// 공유 캔버스(스펙 2026-07-18 §5.3)의 이번 프레임 underlay 항목. add 순서 = draw 순서 = z.
#[derive(Clone, Copy)]
struct CanvasFrameItem {
    id: NativeSurfaceId,
    rect: DeviceIntRect,
    /// 이번 lease가 지난 Present와 다른 세대인가(external_needs_present 판정 결과).
    updated: bool,
    /// lease 확보 성공 여부. false = 이번 프레임 이 자리는 투명 구멍(해체 경합 등).
    drawable: bool,
}

/// 공유 캔버스 Present1 더티 렉트 계산(순수 함수 — TDD 대상). prev = 지난 '표시된' 프레임의
/// id → (rect, draw 성공 여부). 규칙(스펙 §5.3): ①세대 갱신 ②rect 이동/크기 변경(옛+새)
/// ③draw 성공↔실패 전이(구멍 생성/복구) ④신규 등장 ⑤소멸(옛 자리 투명 전환).
/// 공집합 = 캔버스 무접촉(Present 스킵). 전량 재드로우 전제라 이 힌트가 정합하다:
/// 무갱신 비디오는 같은 소스 프레임을 재드로우해 픽셀 동일이 보장된다.
fn canvas_dirty_rects(
    prev: &FxHashMap<NativeSurfaceId, (DeviceIntRect, bool)>,
    current: &[CanvasFrameItem],
) -> Vec<DeviceIntRect> {
    let mut dirty = Vec::new();
    for it in current {
        match prev.get(&it.id) {
            None => dirty.push(it.rect),
            Some((old, _)) if *old != it.rect => {
                dirty.push(*old);
                dirty.push(it.rect);
            },
            Some((_, was_drawn)) if *was_drawn != it.drawable => dirty.push(it.rect),
            Some(_) if it.updated => dirty.push(it.rect),
            Some(_) => {},
        }
    }
    for (id, (old, _)) in prev.iter() {
        if !current.iter().any(|it| it.id == *id) {
            dirty.push(*old);
        }
    }
    dirty
}
```

`present1_partial`(:616)을 일반화 — 본문을 스왑체인 포인터/크기 기반 헬퍼로 옮기고 위임:

```rust
/// Present1 + DirtyRects 힌트(스펙 §5.2-3). 16 초과·공집합이면 힌트 없이 전체.
/// 콘텐츠 스왑체인(present1_partial)과 공유 캔버스(canvas_flush)가 공용.
fn present1_with_dirty(
    swapchain: *mut IDXGISwapChain1,
    size: DeviceIntSize,
    dirty: &[DeviceIntRect],
) -> bool {
    let rects: Vec<RECT> = dirty
        .iter()
        .filter_map(|r| {
            let left = r.min.x.max(0);
            let top = r.min.y.max(0);
            let right = r.max.x.min(size.width);
            let bottom = r.max.y.min(size.height);
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
    let hr = unsafe { (*swapchain).Present1(0, 0, &params) };
    if hr < 0 {
        warn!("[dcomp-native] Present1 failed (hr=0x{:08x})", hr as u32);
        return false;
    }
    true
}

fn present1_partial(sc: &SwapChainStorage, dirty: &[DeviceIntRect]) -> bool {
    present1_with_dirty(sc.swapchain.as_ptr(), sc.size, dirty)
}
```

- [ ] **Step 4: 신규 4종 + 기존 dcomp 스위트 전체 통과 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl canvas_dirty`
Expected: PASS (4 passed)
Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl dcomp`
Expected: PASS (기존 부분 Present/캐치업 계열 무회귀 — present1_partial 위임 등가 확인)

- [ ] **Step 5: 커밋**

```powershell
git add components/paint/dcomp_compositor.rs
git commit -m "캔버스 더티 판정 순수 함수(canvas_dirty_rects)+Present1 헬퍼 일반화(present1_with_dirty)"
```

---

### Task 4: 컴포지터 캔버스 통합 (`canvas_flush`)

**Files:**
- Modify: `components/paint/dcomp_compositor.rs`:
  - 필드/스토리지: `DCompNativeCompositor` struct(:1091 부근) + 생성자(:1263 부근)
  - `begin_frame`(:2248), `add_surface`(:2267), `add_external_surface`(:1393), `start_compositing`(:2366), `end_frame` AddVisual 루프(:2901-2918)
  - 신규 메서드 `ensure_canvas`, `canvas_flush`

**Interfaces:**
- Consumes: Task 1 `VideoEscapeMode::Canvas`; Task 2 `convert_to_rect(ctx, lease, rtv, left, top, w, h)`; Task 3 `CanvasFrameItem`/`canvas_dirty_rects`/`present1_with_dirty`; 기존 `create_composition_swapchain(size, is_opaque)`(is_opaque=false → PREMULTIPLIED), `external_needs_present`, `paint_api::video_external_surface_provider()`(`acquire(&dyn RenderingContext, u64) -> Option<VideoFrameLease>` / `release(&dyn RenderingContext, u64)`), `rc.size2d()`, `rc.dcomp_resize_active()`, `self.external_batch_active`+`begin_batch`/`close_external_batch`, `dcomp_debug()`, `video_escape_prof()`+`EscProf`.
- Produces: 동작 — canvas 모드에서 underlay external은 per-video 스왑체인/비주얼/Present 미사용, 캔버스 1개로 표시. `ExternalStorage.last_presented`는 캔버스 Present 성공 후 갱신(세대 dedup 재사용). off/native/external 모드 코드 경로 무변경(모든 신규 분기는 `self.canvas_mode` 뒤).

- [ ] **Step 1: 스토리지/필드/생성자 추가**

`ExternalStorage`(:909) 아래에 추가:

```rust
/// 공유 비디오 캔버스(SERVO_VIDEO_ESCAPE=canvas, 스펙 2026-07-18) 저장소 — 컴포지터당 1개.
/// underlay external 전체를 창 크기 스왑체인 하나에 전량 재드로우하고 Present1 1회/프레임.
/// visual은 최초 1회 생성해 유지하고, 트리 추가는 end_frame이 프레임별로(최초 underlay
/// 위치=최하단) 수행한다. 스왑체인은 premultiplied(비디오 없는 영역 투명).
struct CanvasStorage {
    visual: ComOwned<IDCompositionVisual>,
    /// None = 미생성/생성 실패(다음 프레임 재시도) — external과 동일 정책.
    swapchain: Option<ComOwned<IDXGISwapChain1>>,
    /// 현재 스왑체인 크기(= 생성 시 창 크기). 창 크기 변경 시 재생성(드래그 중 억제).
    size: DeviceIntSize,
    /// 첫 성공 Present 후 visual.SetContent 완료 여부(1회). 재생성 시 false로 리셋 —
    /// canvas_flush가 이 플래그로 '전체 강제(full)' Present를 판정한다.
    content_attached: bool,
    /// 백버퍼별 RTV 캐시(키=GetBuffer(0) 포인터, FLIP 2버퍼 → 엔트리 ≤2). 스왑체인
    /// 백버퍼는 rename이 없어 안전(ExternalStorage.rtv_cache와 동일 근거). (재)생성 시 clear.
    rtv_cache: FxHashMap<usize, ComOwned<ID3D11RenderTargetView>>,
    /// 브링업 계약 로그(최초 5프레임)용 카운터(dcomp_debug 게이트).
    frames_logged: u32,
}
```

`DCompNativeCompositor` struct에 필드 추가(`external_batch_active: bool`(:1126) 근처):

```rust
    /// SERVO_VIDEO_ESCAPE=canvas 여부(프로세스 불변 — 생성 시 1회 캐시).
    canvas_mode: bool,
    /// 공유 비디오 캔버스(지연 생성, canvas_mode에서만 사용).
    canvas: Option<CanvasStorage>,
    /// 이번 프레임 underlay external 기록(add 순서 = draw 순서 = z). begin_frame에서 clear.
    frame_canvas_items: Vec<(NativeSurfaceId, DeviceIntRect)>,
    /// 지난 '표시된' 캔버스 프레임의 id → (rect, draw 성공). canvas_dirty_rects의 prev.
    canvas_prev_rects: FxHashMap<NativeSurfaceId, (DeviceIntRect, bool)>,
    /// 이번 프레임 add_surface에서 비-external(콘텐츠) 서피스를 봤는가 —
    /// false에 온 external = underlay(캔버스), true에 온 external = overlay(per-video 유지).
    content_seen: bool,
    /// 캔버스 생성 실패 warn-once.
    warned_canvas_fail: bool,
```

생성자(:1263 부근, `external_batch_active: false` 옆)에 초기화 추가:

```rust
            canvas_mode: paint_api::rendering_context::video_escape_mode()
                == paint_api::rendering_context::VideoEscapeMode::Canvas,
            canvas: None,
            frame_canvas_items: Vec::new(),
            canvas_prev_rects: FxHashMap::default(),
            content_seen: false,
            warned_canvas_fail: false,
```

(파일 상단 use에 `VideoEscapeMode`가 없으면 풀패스 사용 — 위 코드가 그렇게 되어 있음.)

Run: `cargo check -p servo-paint --features paint_api/no-wgl` → 필드 미사용 warning 외 에러 0 확인.

- [ ] **Step 2: 프레임 훅 — begin_frame / add_surface / add_external_surface 분기**

`begin_frame`(:2248) 끝(`self.frame_surfaces.clear();` 다음)에 추가:

```rust
        // 공유 캔버스: 이번 프레임 기록 초기화(underlay/overlay 판정 포함).
        self.content_seen = false;
        self.frame_canvas_items.clear();
```

`add_surface`(:2267)에서 External 조기 분기(:2279-2285) **다음** 줄에 추가(= 비-external 서피스 도달 지점):

```rust
        // 공유 캔버스 underlay/overlay 판정: 콘텐츠(비-external) 서피스가 등장한 뒤에 오는
        // external은 overlay다(WR add_surface는 underlay→콘텐츠→overlay z순, 스펙 §5.1).
        self.content_seen = true;
```

`add_external_surface`(:1393)의 빈 클립 skip(:1405-1407) **다음**, `place_external_visual`(:1411) **앞**에 분기 추가:

```rust
        // 공유 캔버스(스펙 2026-07-18 §5.3 1단계): underlay external은 기록만 하고
        // start_compositing의 canvas_flush가 일괄 draw+Present1한다. per-video
        // 스왑체인/비주얼/Present 미사용(Present×N 직렬화 소멸 — §3-ac).
        // overlay(content_seen=true, PiP류 ≤4)는 아래 기존 per-video 경로 유지.
        if self.canvas_mode && !self.content_seen {
            self.frame_canvas_items.push((id, clip_rect));
            self.frame_surfaces.push(id); // z-order 기록 — end_frame이 캔버스 비주얼로 치환
            return;
        }
```

- [ ] **Step 3: `ensure_canvas` + `canvas_flush` 구현, start_compositing 연결**

`start_compositing`(:2366)을 다음으로 교체:

```rust
    fn start_compositing(
        &mut self,
        _device: &mut Device,
        _clear_color: ColorF,
        _dirty_rects: &[DeviceIntRect],
        _opaque_rects: &[DeviceIntRect],
    ) {
        // 공유 캔버스 플러시: add_surface 루프의 모든 underlay 기록 뒤·타일 GL 앞인 여기가
        // 정확한 지점이다(close_external_batch 주석의 근거와 동일). 배치를 쓸 수 있게
        // close보다 먼저 호출한다.
        self.canvas_flush();
        // ★external convert 배치를 닫는 정본 위치(기존 주석 유지).
        self.close_external_batch();
    }
```

`present_external` 아래에 신규 메서드 2개 추가:

```rust
    /// 캔버스 visual(1회)+스왑체인(창 크기, premultiplied)을 (재)생성한다. 실패 시 None 유지
    /// → 다음 프레임 자연 재시도(스펙 §6). 재생성 시 rtv_cache clear + content_attached 리셋.
    fn ensure_canvas(&mut self, size: DeviceIntSize) {
        if self.canvas.is_none() {
            let Some(dcomp_device) = self.dcomp_device_ptr() else {
                return;
            };
            // Safety: dcomp_device는 살아있는 IDCompositionDevice.
            let visual = unsafe {
                let mut raw: *mut IDCompositionVisual = ptr::null_mut();
                let hr = (*dcomp_device).CreateVisual(&mut raw);
                if hr < 0 || raw.is_null() {
                    if !self.warned_canvas_fail {
                        warn!("[dcomp-native] canvas: CreateVisual failed (hr=0x{:08x})", hr as u32);
                        self.warned_canvas_fail = true;
                    }
                    return;
                }
                match ComOwned::from_raw(raw) {
                    Some(v) => v,
                    None => return,
                }
            };
            self.canvas = Some(CanvasStorage {
                visual,
                swapchain: None,
                size: DeviceIntSize::zero(),
                content_attached: false,
                rtv_cache: FxHashMap::default(),
                frames_logged: 0,
            });
        }
        // 스왑체인 (재)생성. premultiplied(is_opaque=false) — 비디오 없는 영역 투명(스펙 §5.2).
        let created = self.create_composition_swapchain(size, false);
        let Some(canvas) = self.canvas.as_mut() else {
            return;
        };
        canvas.rtv_cache.clear();
        canvas.content_attached = false;
        match created {
            Some(sc) => {
                canvas.swapchain = Some(sc);
                canvas.size = size;
                self.warned_canvas_fail = false; // 성공 → 실패 warn 재무장
                if dcomp_debug() {
                    log::info!("[dcomp-dbg] canvas swapchain (re)create {}x{}", size.width, size.height);
                }
            },
            None => {
                canvas.swapchain = None;
                canvas.size = DeviceIntSize::zero();
                if !self.warned_canvas_fail {
                    warn!(
                        "[dcomp-native] canvas swapchain {}x{} create failed; retrying next frame",
                        size.width, size.height
                    );
                    self.warned_canvas_fail = true;
                }
            },
        }
    }

    /// 공유 캔버스 2단계(스펙 §5.3): 전 underlay lease acquire → 더티 판정 → (더티 시)
    /// 전체 투명 클리어 + add 순서 전량 draw + Present1 1회. 더티 공집합이면 캔버스 무접촉.
    /// lease는 이 함수 안에서 acquire↔release 짝맞춤(모든 반환 경로에서 release).
    fn canvas_flush(&mut self) {
        if !self.canvas_mode {
            return;
        }
        if self.frame_canvas_items.is_empty() {
            // underlay 없음(비디오 페이지 이탈 등). 캔버스 비주얼은 이번 프레임 트리에
            // 추가되지 않으므로 표시 없음 — 지난 rect 부기만 비운다.
            self.canvas_prev_rects.clear();
            return;
        }
        let rc = self.rendering_context.clone();
        let Some(provider) = paint_api::video_external_surface_provider() else {
            return; // 미등록 — 기존 경로가 warn-once를 이미 냄(add_external_surface와 동일 수용)
        };
        // convert_pass 지연 초기화(present_external과 동일 래치·실패 시 재시도 안 함).
        if self.convert_pass.is_none() && !self.convert_pass_init_failed {
            self.convert_pass =
                unsafe { crate::dcomp_video_convert::VideoConvertPass::new(self.d3d11_device) };
            if self.convert_pass.is_none() {
                self.convert_pass_init_failed = true;
                warn!("[dcomp-native] canvas: VideoConvertPass unavailable; skipping");
            }
        }
        if self.convert_pass.is_none() || self.d3d11_context1.is_none() {
            return;
        }

        let prof_on = video_escape_prof();
        let items: Vec<(NativeSurfaceId, DeviceIntRect)> = self.frame_canvas_items.clone();

        // ── 1) acquire 패스: 항목별 lease + 세대 판정(짧은 surfaces borrow) ──
        let acq_start = if prof_on { Some(std::time::Instant::now()) } else { None };
        let mut leases: Vec<Option<paint_api::VideoFrameLease>> = Vec::with_capacity(items.len());
        let mut frame_items: Vec<CanvasFrameItem> = Vec::with_capacity(items.len());
        for (id, rect) in items.iter() {
            let (attached, last) = match self.surfaces.get(id).map(|e| &e.storage) {
                Some(SurfaceStorage::External(ext)) => (ext.attached_external_id, ext.last_presented),
                _ => (None, None),
            };
            let lease = attached.and_then(|eid| provider.acquire(&*rc, eid));
            let updated = lease
                .as_ref()
                .is_some_and(|l| external_needs_present(last, l.ring_id, l.frame_seq));
            frame_items.push(CanvasFrameItem {
                id: *id,
                rect: *rect,
                updated,
                drawable: lease.is_some(),
            });
            leases.push(lease);
        }
        if let Some(s) = acq_start {
            self.esc_prof.acquires += leases.iter().flatten().count() as u64;
            self.esc_prof.acquire_dur += s.elapsed();
        }
        // 이후 어느 경로로 빠져도 전 lease를 반납한다(짝맞춤). 매크로 대신 클로저 불가
        // (borrow) — 반환 직전마다 아래 블록을 복붙하지 말고 flow를 단일 출구로 유지한다.

        // ── 2) 더티 판정(순수 함수) + Present 스킵 ──
        let mut force_full = self.canvas.as_ref().map_or(true, |c| !c.content_attached);
        let any_lease = leases.iter().any(Option::is_some);
        let dirty = canvas_dirty_rects(&self.canvas_prev_rects, &frame_items);
        let mut proceed = !dirty.is_empty() || (force_full && any_lease);

        // ── 3) 캔버스 보장(창 크기·드래그 중 재생성 억제 — §3-y 정합) ──
        if proceed {
            let s = rc.size2d();
            let target = DeviceIntSize::new(s.width as i32, s.height as i32);
            let resize_active = rc.dcomp_resize_active();
            // 드래그 중(resize_active)엔 크기 불일치여도 기존 캔버스 유지(재생성 억제,
            // §3-y 정합). 스왑체인이 아예 없으면(최초/생성 실패) 드래그 중이라도 생성 시도.
            let have_usable = self
                .canvas
                .as_ref()
                .is_some_and(|c| c.swapchain.is_some() && (c.size == target || resize_active));
            if !have_usable {
                self.ensure_canvas(target);
            }
            force_full = force_full
                || self.canvas.as_ref().map_or(true, |c| !c.content_attached);
            proceed = self
                .canvas
                .as_ref()
                .is_some_and(|c| c.swapchain.is_some());
        }

        // ── 4) 클리어 + 전량 draw + Present1 (proceed일 때만) ──
        let mut drawn_ok: Vec<bool> = vec![false; frame_items.len()];
        let mut presented = false;
        if proceed {
            // begin_batch: 이번 프레임 overlay가 이미 열었을 수 있음(재사용). convert_pass /
            // ctx1은 위에서 존재 확인 완료.
            let ctx1_ptr = self.d3d11_context1.as_ref().unwrap().as_ptr();
            if !self.external_batch_active {
                // Safety: 살아있는 컨텍스트/패스.
                unsafe { self.convert_pass.as_mut().unwrap().begin_batch(ctx1_ptr) };
                self.external_batch_active = true;
                if prof_on {
                    self.esc_prof.batch_swaps += 1;
                }
            }
            let canvas = self.canvas.as_mut().unwrap();
            let swapchain_ptr = canvas.swapchain.as_ref().unwrap().as_ptr();
            let canvas_size = canvas.size;
            // Safety: 살아있는 스왑체인. GetBuffer(0) → RTV 캐시(백버퍼 포인터 키).
            unsafe {
                let mut back: *mut ID3D11Texture2D = ptr::null_mut();
                let hr = (*swapchain_ptr).GetBuffer(
                    0,
                    &ID3D11Texture2D::uuidof(),
                    &mut back as *mut _ as *mut _,
                );
                if hr >= 0 && !back.is_null() {
                    let back_key = back as usize;
                    if !canvas.rtv_cache.contains_key(&back_key) {
                        let mut raw: *mut ID3D11RenderTargetView = ptr::null_mut();
                        let hr = (*self.d3d11_device).CreateRenderTargetView(
                            back as *mut ID3D11Resource,
                            ptr::null(),
                            &mut raw,
                        );
                        if hr >= 0 && !raw.is_null() {
                            if let Some(owned) = ComOwned::from_raw(raw) {
                                canvas.rtv_cache.insert(back_key, owned);
                            }
                        }
                    }
                    if let Some(rtv) = canvas.rtv_cache.get(&back_key).map(|c| c.as_ptr()) {
                        // 전체 투명 클리어(전량 재드로우 전제 — catch-up 불요 근거, 스펙 §5.3).
                        (*ctx1_ptr).ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 0.0]);
                        let c_start = if prof_on { Some(std::time::Instant::now()) } else { None };
                        let cp = self.convert_pass.as_mut().unwrap();
                        for (i, it) in frame_items.iter().enumerate() {
                            let Some(lease) = leases[i].as_ref() else { continue };
                            let w = (it.rect.max.x - it.rect.min.x).max(0) as u32;
                            let h = (it.rect.max.y - it.rect.min.y).max(0) as u32;
                            if w == 0 || h == 0 {
                                continue;
                            }
                            drawn_ok[i] =
                                cp.convert_to_rect(ctx1_ptr, lease, rtv, it.rect.min.x, it.rect.min.y, w, h);
                        }
                        if let Some(s) = c_start {
                            self.esc_prof.converts += frame_items.len() as u64;
                            self.esc_prof.convert_dur += s.elapsed();
                            self.esc_prof.srv_creates +=
                                leases.iter().flatten().map(|l| l.plane_count.min(3) as u64).sum::<u64>();
                        }
                        // Present1: 재생성 직후(force_full)는 힌트 없이 전체(빈 슬라이스 → 헬퍼가 전체 처리).
                        let p_start = if prof_on { Some(std::time::Instant::now()) } else { None };
                        presented = if force_full {
                            present1_with_dirty(swapchain_ptr, canvas_size, &[])
                        } else {
                            present1_with_dirty(swapchain_ptr, canvas_size, &dirty)
                        };
                        if let Some(s) = p_start {
                            self.esc_prof.presents += 1;
                            self.esc_prof.present_dur += s.elapsed();
                        }
                        if presented && !canvas.content_attached {
                            // 첫 성공 Present 후 SetContent 1회 — end_frame Commit 전 완결(원자성).
                            let hr = (*canvas.visual.as_ptr())
                                .SetContent(swapchain_ptr as *const IUnknown);
                            if hr >= 0 {
                                canvas.content_attached = true;
                                if dcomp_debug() {
                                    log::info!("[dcomp-dbg] canvas content-attach");
                                }
                            } else if !self.warned_canvas_fail {
                                warn!("[dcomp-native] canvas SetContent failed (hr=0x{:08x})", hr as u32);
                                self.warned_canvas_fail = true;
                            }
                        }
                        if dcomp_debug() && canvas.frames_logged < 5 {
                            canvas.frames_logged += 1;
                            log::info!(
                                "[dcomp-dbg] canvas present items={} dirty={} full={} size={}x{}",
                                frame_items.len(), dirty.len(), force_full,
                                canvas_size.width, canvas_size.height
                            );
                        }
                    }
                    (*(back as *mut IUnknown)).Release();
                }
            }
        }

        // ── 5) 성공 시 부기 갱신: last_presented(세대 dedup) + prev_rects ──
        if presented {
            for (i, it) in frame_items.iter().enumerate() {
                if drawn_ok[i] {
                    if let Some(lease) = leases[i].as_ref() {
                        if let Some(SurfaceStorage::External(ext)) =
                            self.surfaces.get_mut(&it.id).map(|e| &mut e.storage)
                        {
                            ext.last_presented = Some((lease.ring_id, lease.frame_seq));
                        }
                    }
                }
            }
            self.canvas_prev_rects.clear();
            for (i, it) in frame_items.iter().enumerate() {
                self.canvas_prev_rects.insert(it.id, (it.rect, drawn_ok[i]));
            }
        }

        // ── 6) release 짝맞춤(모든 경로 공통 단일 출구) ──
        for lease in leases.iter().flatten() {
            provider.release(&*rc, lease.ring_id);
        }
    }
```

구현 시 borrow 조정 재량 허용(예: `canvas`/`self.esc_prof` 동시 접근을 지역 변수 수집 후 일괄 반영으로 풀기). **의미론은 위 코드가 정본**: (1) acquire→판정→클리어→전량 draw(add 순서)→Present1 1회→부기→release 짝맞춤, (2) 더티 공집합+비강제면 캔버스 무접촉, (3) 드래그 중 재생성 억제, (4) plane SRV는 convert_to_rect 내부에서 매 draw 신선 생성(캐시 금지).

- [ ] **Step 4: end_frame AddVisual 루프 — 캔버스 비주얼 치환**

`end_frame`의 AddVisual 루프(:2901-2918)를 다음으로 교체:

```rust
        if let Some(root) = self.root_visual_ptr() {
            let mut canvas_added = false;
            for id in self.frame_surfaces.iter() {
                // 공유 캔버스 underlay id → per-video 비주얼 대신 캔버스 비주얼을 최초
                // 1회(=최하단 위치)만 추가한다. add 순서가 z이므로 최초 underlay 위치 삽입이
                // 전체 순서를 보존한다(스펙 §5.2).
                if self.canvas_mode && self.frame_canvas_items.iter().any(|(cid, _)| cid == id) {
                    if !canvas_added {
                        canvas_added = true;
                        if let Some(canvas) = self.canvas.as_ref() {
                            if canvas.content_attached {
                                let hr = unsafe {
                                    (*root).AddVisual(canvas.visual.as_ptr(), FALSE, ptr::null())
                                };
                                if hr < 0 {
                                    warn!("[dcomp-native] canvas AddVisual failed (hr=0x{:08x})", hr as u32);
                                }
                            }
                        }
                    }
                    continue;
                }
                let Some(entry) = self.surfaces.get(id) else { continue; };
                // Safety: visual/root 살아있음. 순서 = add_surface 순서(z 아래→위) 유지.
                // (AddVisual(FALSE, NULL) 특칙 주석은 기존 그대로 유지 — 삭제 금지.)
                let hr = unsafe { (*root).AddVisual(entry.visual.as_ptr(), FALSE, ptr::null()) };
                if hr < 0 {
                    warn!("[dcomp-native] AddVisual failed (hr=0x{:08x})", hr as u32);
                }
            }
        }
```

(기존 A-1/AddVisual-FALSE 장문 주석 블록(:2887-2912)은 그대로 유지하고 루프 본문만 교체한다.)

- [ ] **Step 5: 빌드 + 유닛 전체 + off/external 무회귀 컴파일 확인**

```powershell
Stop-Process -Name servoshell -Force -ErrorAction SilentlyContinue
cd D:\2_TechReview\20260606_multigpu_browser\servo
. .\etc\multigpu\servo_env.ps1
$ErrorActionPreference = 'Continue'
. .\.venv\Scripts\Activate.ps1
python mach build --release
```
Expected: 빌드 성공.
Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl dcomp`
Expected: 전부 PASS.

- [ ] **Step 6: 2×2 스모크 (개발기 A5000)**

```powershell
$env:SERVO_DCOMP_DEBUG = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 2 -Rows 2 -DComp -VideoEscape canvas
```
로그 판독 기준 (`servoshell` 콘솔/로그):
- `[dcomp-dbg] canvas swapchain (re)create <W>x<H>` **정확히 1회** (창 크기와 일치)
- `[dcomp-dbg] canvas content-attach` 1회, `canvas present items=4 ...` 5줄
- `[dcomp-dbg] external swapchain (re)create` **0회** (per-video 스왑체인 미생성 증거)
- 육안: 4비디오 재생 정상, 잔상/검정/z 이상 없음
- 종료 후 `$env:SERVO_DCOMP_DEBUG=""` 정리. 이상 시 여기서 수정 후 재확인(다음 단계 진행 금지).

- [ ] **Step 7: 커밋**

```powershell
git add components/paint/dcomp_compositor.rs
git commit -m "공유 비디오 캔버스 본체: underlay external 일괄 draw+Present1 1회(canvas_flush), 캔버스 비주얼 z 치환, 드래그 중 재생성 억제"
```

---

### Task 5: 런타임 검증 배터리 (A5000, 스펙 §8)

**Files:**
- 없음(코드 변경 0 — 검증 기록만). 결과는 `.superpowers/sdd/canvas-task-5-report.md`에 기록.

**Interfaces:**
- Consumes: Task 4까지의 servoshell.exe, 기존 페이지들(`tests/html/video_grid_6x6_perf.html`(런처 기본), `mixed_media_demo.html`, `complex_media_stress.html`, `complex_media_transforms.html`), `D:\PresentMon-2.3.1-x64.exe`(관리자, servoshell 포그라운드 필수 — 가려지면 가짜 ~9fps).
- Produces: PASS/FAIL 판정 리포트. FAIL 항목은 수정 후 해당 항목 재실행.

- [ ] **Step 1: 45타일 월 + vesc-prof 판독**

```powershell
$env:SERVO_VIDEO_ESCAPE_PROF = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 9 -Rows 5 -DComp -VideoEscape canvas -Sync 45
```
판독 기준: `[vesc-prof]` 라인에서 **presents ≈ frames**(프레임당 1회 — external은 ~45×frames였음), converts ≈ 45×frames, fps는 external 대비 동등 이상(fps 측정은 15~20s 간격 — 소스 루프 30s 배수 앨리어싱 금지). lockstep 육안 ±1. 45타일 전부 표시.

- [ ] **Step 2: PresentMon 스왑체인 46→2 확인**

servoshell 포그라운드 상태에서 관리자 PowerShell:
```powershell
D:\PresentMon-2.3.1-x64.exe -process_name servoshell.exe -output_file D:\canvas_presentmon.csv -timed 20
```
판독 기준: 고유 SwapChainAddress **2개**(캔버스+콘텐츠 — external은 46), PresentMode 전부 `Composed: Flip`.

- [ ] **Step 3: 복합 3종**

각각 실행(비디오 수에 맞는 -Sync 필수 — §3-aa 함정 (5), 커스텀 페이지는 -Cols/-Rows 명시):
```powershell
$env:SERVO_DCOMP_DEBUG = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 2 -DComp -VideoEscape canvas -Page mixed_media_demo.html -Sync 6
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 3 -DComp -VideoEscape canvas -Page complex_media_stress.html -Sync 13
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 3 -DComp -VideoEscape canvas -Page complex_media_transforms.html -Sync 13
```
판독 기준:
- mixed: 전 요소 정상(영상·시계·티커·자막), 티커/시계만 갱신되는 순간 캔버스 Present 스킵 동작(vesc-prof presents < frames)
- stress: 13/13 표시, **PiP(overlay)는 per-video 경로 로그**(`external swapchain (re)create` 소수 발생 = overlay 정상) + PiP 프레임 진행(동결 없음)
- transforms: external 유지 조합(이동·스케일) 정상 + **스케일 애니 중 `canvas swapchain (re)create` 0회**(§12.4 처닝 소멸 증거), Z-회전/3D플립은 WR 폴백 정상
- (참고: 페이지 파일명이 런처 -Page 규약과 다르면 `tests/html/` 실제 파일명 확인 후 조정)

- [ ] **Step 4: 리사이즈/드래그 (§3-y 시나리오)**

2×2 canvas 실행 상태에서 창 모서리 드래그 40스텝(연속 크기 변경) → 정착.
판독 기준: 드래그 중 잔상/블랙 0, 드래그 중 `canvas swapchain (re)create` 0회(억제), 정착 후 1회 재생성 + 정상 표시.

- [ ] **Step 5: 무회귀 4종 + WebGPU 월**

```powershell
# off (DComp만)
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 3 -DComp
# native / external (기존 거동 그대로)
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 3 -DComp -VideoEscape native
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 3 -DComp -VideoEscape external
# =surface 조합
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 3 -DCompSurface -VideoEscape canvas
```
판독 기준: off/native/external 각각 기존과 동일 거동(external은 per-video 스왑체인 로그 존재), `=surface`+canvas 조합 표시 정상. WebGPU 월은 메모리 정본 명령(`servo-build-run-commands` — threejs webgpu_compute_birds, wall_layout.example_2x1_dualgpu.json)으로 기동 확인.

- [ ] **Step 6: 30분 소크**

45타일 canvas 상태로 30분 방치, 5분 간격 Working Set 기록:
```powershell
while ($true) { Get-Process servoshell | Select-Object -ExpandProperty WorkingSet64; Start-Sleep -Seconds 300 }
```
판독 기준: WS 플랫(±2%), 크래시 0, 신규 WARN/ERROR 0.

- [ ] **Step 7: 리포트 커밋**

`.superpowers/sdd/canvas-task-5-report.md`에 각 단계 수치/판정 기록 후:
```powershell
git add .superpowers/sdd/canvas-task-5-report.md
git commit -m "공유 캔버스 A5000 검증: 45타일 Present 1/프레임, 스왑체인 46→2, 복합 3종/리사이즈/무회귀/소크 판정 기록"
```

---

### Task 6: 패키지·AMD 가이드 + 스펙 결과 기록

**Files:**
- Modify: `etc/multigpu/package_run_wall.ps1` (헤더 가이드), `etc/multigpu/run_video_wall_d3d11.ps1` (헤더 주석 1줄)
- Modify: `docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md` (§11 구현 결과 추가)
- 패키지: `D:\ServoWallPackage\run_wall.ps1` + `D:\ServoWallPackage.zip` (리포 밖 — 커밋 대상 아님)

**Interfaces:**
- Consumes: Task 5 PASS 리포트, `package_run_wall.ps1`(패키지 런처 정본 백업 — 66e448511 관례).
- Produces: AMD 4자 A/B 가이드가 담긴 패키지. 판독 절차는 이 가이드가 정본.

- [ ] **Step 1: 런처/패키지 런처 가이드 갱신**

`etc/multigpu/package_run_wall.ps1` 헤더의 A/B 절차(:15-17 부근)를 4자로 교체(영어 유지 — 패키지는 무개발환경 PC용):

```powershell
#   1) .\run_wall.ps1 -Cols 9 -Rows 5 -Sync -1 -DComp                           (baseline)
#   2) .\run_wall.ps1 -Cols 9 -Rows 5 -Sync -1 -DComp -VideoEscape native       (diagnostic ONLY - do not use for PiP/complex pages)
#   3) .\run_wall.ps1 -Cols 9 -Rows 5 -Sync -1 -DComp -VideoEscape external     (per-video swapchains - N Presents/frame)
#   4) .\run_wall.ps1 -Cols 9 -Rows 5 -Sync -1 -DComp -VideoEscape canvas       (shared canvas - 1 Present/frame, RECOMMENDED)
#
# Readout (set SERVO_VIDEO_ESCAPE_PROF=1 for [vesc-prof] lines):
#   - Key A/B is (3) vs (4): if (4) recovers fps at 36+ tiles while (3) collapses,
#     the Present-per-swapchain serialization diagnosis is confirmed (present_ms should
#     drop to near zero in (4); compare GPU% too).
#   - (4) uses ONE window-size swapchain for all videos: PresentMon should show 2
#     swapchains total (canvas + content) instead of N+1.
#   - -TileSize tuning is NOT needed for external/canvas modes.
```

`etc/multigpu/run_video_wall_d3d11.ps1`의 `-VideoEscape` 관련 주석에 `canvas` 값 설명 1줄 추가(파라미터 선언부 :43 위 주석 블록).

- [ ] **Step 2: 스펙에 구현 결과 기록**

`docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md` 말미에 `## 11. 구현 결과 (2026-MM-DD)` 섹션 추가 — 커밋 체인 표, 스펙 대비 이탈(있으면 항목별), Task 5 검증 수치 요약(Present 1/프레임·스왑체인 46→2·복합 3종·소크), 이월 사항. (전 사이클 스펙 §12 형식을 따른다 — 이탈 없으면 "이탈 없음" 명기.)

- [ ] **Step 3: 패키지 재생성**

```powershell
Copy-Item D:\2_TechReview\20260606_multigpu_browser\servo\target\release\servoshell.exe D:\ServoWallPackage\ -Force
Copy-Item D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\package_run_wall.ps1 D:\ServoWallPackage\run_wall.ps1 -Force
Compress-Archive -Path D:\ServoWallPackage\* -DestinationPath D:\ServoWallPackage.zip -Force
```
Expected: zip 재생성(기존 ~1.16GB급). 스모크: 패키지 폴더에서 `.\run_wall.ps1 -Cols 2 -Rows 2 -DComp -VideoEscape canvas` 기동 확인.

- [ ] **Step 4: 커밋**

```powershell
git add etc/multigpu/package_run_wall.ps1 etc/multigpu/run_video_wall_d3d11.ps1 docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md
git commit -m "공유 캔버스 마감: AMD 4자 A/B 가이드(캔버스 권장), 스펙 구현 결과 기록, 패키지 재생성"
```

---

## Self-Review 결과 (계획 확정 전 점검)

1. **스펙 커버리지**: §4 게이트→Task 1, §5.4 변환 패스→Task 2, §5.3 더티/Present1→Task 3+4, §5.1-5.3 아키텍처 본체→Task 4, §7 수명(리사이즈/드래그/=surface/진단)→Task 4 Step 3+Task 5 Step 4-5, §6 에러 표→Task 4(생성 실패 재시도/lease 실패 구멍+더티 포함/provider 부재), §8 검증→Task 5, §8-6 패키지→Task 6. 갭 없음.
2. **플레이스홀더**: 없음(모든 코드 스텝에 실제 코드, 모든 실행 스텝에 명령+판독 기준).
3. **타입 일관성**: `convert_to_rect(ctx, lease, rtv, i32, i32, u32, u32)` Task 2 정의 = Task 4 사용처 일치. `CanvasFrameItem{id,rect,updated,drawable}`/`canvas_dirty_rects(prev: &FxHashMap<NativeSurfaceId,(DeviceIntRect,bool)>, current: &[CanvasFrameItem])` Task 3 정의 = Task 4 사용처 일치. `present1_with_dirty(*mut IDXGISwapChain1, DeviceIntSize, &[DeviceIntRect])` 일치.

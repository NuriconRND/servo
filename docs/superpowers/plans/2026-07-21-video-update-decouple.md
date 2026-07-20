# external 비디오 갱신 분리 (video-update-decouple) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** external 승격된 비디오 프레임 갱신이 WebRender 전체 씬 빌드(`transaction.generate_frame`)를 트리거하지 않고 DComp present만으로 화면에 도달하게 하여, 큰 창·다중 타일에서 RenderBackend 스레드 포화(=AMD 프레임 하락)를 제거한다.

**Architecture:** DComp 컴포지터를 `Rc<RefCell<>>` 공유 핸들로 만들어 painter가 접근 가능하게 한다(WR엔 위임 래퍼). 비디오 프레임 도착 시 painter의 즉시-합성 게이트(painter.rs:2027)가 `generate_frame`(전체 빌드) 대신 컴포지터의 신규 경량 경로 `present_external_only`(승격 external만 present + Commit 1회, 빌드 없음)를 refresh 케이던스로 호출한다. 콘텐츠·미승격 비디오는 기존 경로 유지.

**Tech Stack:** Rust, WebRender 0.68(vendored), DirectComposition/D3D11(winapi), servo-paint 크레이트.

## Global Constraints

- 대상 크레이트: `servo-paint` (`components/paint`). 유닛 테스트: `cargo test -p servo-paint --lib --features paint_api/no-wgl <name>` (pkg-config 함정 회피 위해 `etc/servo_env.ps1` 로드된 셸에서 실행).
- 빌드: `./mach build -r` (`.venv` activate + `$ErrorActionPreference='Continue'` + `etc/servo_env.ps1` 필요 — memory [[servo-build-run-commands]]).
- 커밋 메시지: **한국어**, Claude 어트리뷰션(Co-Authored-By) **금지**(사용자 CLAUDE.md). 커밋 후 `git log -1 --format=%B`로 어트리뷰션 부재 확인.
- 킬스위치: `SERVO_VIDEO_DECOUPLE=0` → 현재(비디오당 `generate_frame`) 동작 복귀. 기본 on.
- 스레드 불변조건: 신규 경로는 painter/Renderer 스레드에서만 실행(ANGLE D3D11 컨텍스트 소유 스레드). `Rc<RefCell>` 이중 대여 금지 — WR render()의 트레이트 대여와 painter의 fast-path 대여는 절대 중첩 안 됨(동일 스레드, 프레임당 단일 경로).
- 스타일: cpp 아닌 Rust 주석은 한국어 허용. 기존 dcomp_compositor.rs 주석 밀도/스타일에 맞춘다.
- 운영/검증 모드: `-DComp -VideoEscape external`.

---

## File Structure

- **Modify** `components/paint/dcomp_compositor.rs` (3341줄): 킬스위치 `decouple_enabled()`, 순수 라우팅 판정 `should_fast_present(...)` + 유닛테스트, 컴포지터 메서드 `escaped_external_count()` / `present_external_only()`, 공유 래퍼 `SharedDComp`, `maybe_create` 반환형 변경.
- **Modify** `components/paint/painter.rs` (2610줄): `Painter`에 공유 핸들 필드 + fast-present 페이싱 상태 추가, `CompositorConfig::Native` 배선(:445-462) 변경, 즉시-합성 게이트(:2027-2036)에 fast-path 분기.

기존 파일만 수정(신규 파일 없음) — 컴포지터/painter의 확립된 패턴을 따른다.

---

## Task 1: 킬스위치 + 순수 라우팅 판정 (TDD)

**Files:**
- Modify: `components/paint/dcomp_compositor.rs` (게이트 헬퍼 근처 ~72-118, 테스트 모듈 ~3140)
- Test: `components/paint/dcomp_compositor.rs` `#[cfg(test)]` 모듈

**Interfaces:**
- Produces:
  - `fn decouple_enabled() -> bool` — `SERVO_VIDEO_DECOUPLE != "0"` (기본 true).
  - `fn should_fast_present(immediate_image_update: bool, generated_frame: bool, pending_zero: bool, renderer_behind: bool, raf_driving: bool, escaped_count: usize, resize_active: bool, decouple_enabled: bool) -> bool` — 순수 판정.

- [ ] **Step 1: 실패 테스트 작성**

`dcomp_compositor.rs`의 `#[cfg(test)] mod tests` 안에 추가:

```rust
#[test]
fn fast_present_gated_on_escaped_and_flags() {
    // 표준 fast-path 케이스: 비디오 도착 + 프레임 미생성 + pending 0 + rAF 없음 +
    // 렌더러 안 밀림 + 승격 external 있음 + 리사이즈 아님 + 기능 on.
    assert!(should_fast_present(true, false, true, false, false, 36, false, true));
    // 승격 external 0 → fast-path 불가(기존 generate_frame로).
    assert!(!should_fast_present(true, false, true, false, false, 0, false, true));
    // 기능 off(킬스위치) → 불가.
    assert!(!should_fast_present(true, false, true, false, false, 36, false, false));
    // 리사이즈 중 → 불가(빌드 경로에 양보).
    assert!(!should_fast_present(true, false, true, false, false, 36, true, true));
    // 이미 프레임 생성됨 → 불가.
    assert!(!should_fast_present(true, true, true, false, false, 36, false, true));
    // rAF가 합성 구동 중 → 불가(기존 게이트 규약과 동일).
    assert!(!should_fast_present(true, false, true, false, true, 36, false, true));
    // 비디오 도착 아님 → 불가.
    assert!(!should_fast_present(false, false, true, false, false, 36, false, true));
    // pending != 0 → 불가.
    assert!(!should_fast_present(true, false, false, false, false, 36, false, true));
    // 렌더러 밀림 → 불가.
    assert!(!should_fast_present(true, false, true, true, false, 36, false, true));
}
```

- [ ] **Step 2: 테스트 실패 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl fast_present_gated`
Expected: FAIL — `should_fast_present` 미정의(컴파일 에러).

- [ ] **Step 3: 최소 구현**

`dcomp_compositor.rs`의 다른 게이트 헬퍼(`stable_swapchain()` ~80, `video_escape_prof()` ~110)와 같은 스타일로 추가:

```rust
/// external 비디오 갱신 분리 킬스위치. 기본 on; "0"이면 현재(비디오당 generate_frame) 복귀.
fn decouple_enabled() -> bool {
    std::env::var("SERVO_VIDEO_DECOUPLE").map(|v| v != "0").unwrap_or(true)
}

/// 즉시-합성 게이트에서 fast-path(present_external_only)를 택할지의 순수 판정.
/// 기존 generate_frame 게이트(painter.rs:2027)와 같은 전제(비디오 도착·미생성·pending 0·
/// rAF 없음·렌더러 안 밀림)에, 승격 external 존재 + 리사이즈 아님 + 기능 on을 더한다.
#[allow(clippy::too_many_arguments)]
fn should_fast_present(
    immediate_image_update: bool,
    generated_frame: bool,
    pending_zero: bool,
    renderer_behind: bool,
    raf_driving: bool,
    escaped_count: usize,
    resize_active: bool,
    decouple_enabled: bool,
) -> bool {
    immediate_image_update
        && !generated_frame
        && pending_zero
        && !renderer_behind
        && !raf_driving
        && escaped_count > 0
        && !resize_active
        && decouple_enabled
}
```

- [ ] **Step 4: 테스트 통과 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl fast_present_gated`
Expected: PASS.

- [ ] **Step 5: 커밋**

```bash
git add components/paint/dcomp_compositor.rs
git commit -m "external 비디오 분리: 킬스위치(SERVO_VIDEO_DECOUPLE) + 순수 라우팅 판정 should_fast_present + 유닛테스트"
git log -1 --format=%B  # 어트리뷰션 부재 확인
```

---

## Task 2: 컴포지터 경량 경로 — escaped_external_count + present_external_only

**Files:**
- Modify: `components/paint/dcomp_compositor.rs` (`impl DCompNativeCompositor` 블록 ~1328-1832, `present_external` ~1520 뒤에 추가)

**Interfaces:**
- Consumes: 기존 `present_external`(1520), `close_external_batch`(1784), `dcomp_device_ptr`(1329), `paint_api::video_external_surface_provider`, `SurfaceStorage::External`, `entry.last_placement`(`LastPlacement{ transform_offset, clip_rect }`), `ext.swapchain_size`, `entry.is_opaque`.
- Produces:
  - `pub(crate) fn escaped_external_count(&self) -> usize`
  - `pub(crate) fn present_external_only(&mut self)`

- [ ] **Step 1: escaped_external_count 구현**

`impl DCompNativeCompositor` 안에 추가:

```rust
/// 현재 승격되어 있는(External storage) 서피스 수. painter의 fast-path 게이트가
/// "present할 external이 있는가"를 질의하는 데 쓴다.
pub(crate) fn escaped_external_count(&self) -> usize {
    self.surfaces
        .values()
        .filter(|e| matches!(e.storage, SurfaceStorage::External(_)))
        .count()
}
```

- [ ] **Step 2: present_external_only 구현**

`present_external`(1520) 바로 뒤에 추가. **빌드/트리 재구성/콘텐츠/promote·demote를 전부 스킵**하고 승격 external만 present + Commit 1회. `present_external`이 요구하는 입력(`size`=ref_size, `is_opaque`, `transform`, `clip_rect`)은 전부 캐시(`ext.swapchain_size`, `entry.is_opaque`, `entry.last_placement`)에서 복원한다. transform은 present_external 내부에서 디버그 로그에만 쓰이므로 scale=1 + 캐시 offset으로 복원해도 무해(placement는 직전 실합성의 place_external_visual이 이미 적용):

```rust
/// external 갱신 분리 fast-path(설계 §4.2): WR 프레임 빌드/트리 재구성 없이, 승격된
/// external 서피스만 캐시된 placement로 present하고 Commit 1회. painter의 즉시-합성
/// 게이트가 refresh 케이던스로 호출한다. 리사이즈 중엔 호출측이 억제한다(설계 §4.5).
pub(crate) fn present_external_only(&mut self) {
    let rc = self.rendering_context.clone();
    let Some(provider) = paint_api::video_external_surface_provider() else {
        return;
    };
    let resize_active = rc.dcomp_resize_active();
    if resize_active {
        return; // 방어: 게이트가 이미 걸러야 하나, 이중 안전.
    }

    // borrow 사정: External 서피스의 present 입력을 먼저 스냅샷으로 수집한 뒤 present.
    // (present_external은 &mut self라 surfaces iter 중 호출 불가.)
    struct PendingExt {
        id: NativeSurfaceId,
        external_id: u64,
        is_opaque: bool,
        ref_size: DeviceIntSize,
        clip_rect: DeviceIntRect,
        transform_offset: (f32, f32),
    }
    let mut pending: Vec<PendingExt> = Vec::new();
    for (id, entry) in self.surfaces.iter() {
        let SurfaceStorage::External(ext) = &entry.storage else {
            continue;
        };
        let (Some(external_id), Some(placement)) =
            (ext.attached_external_id, entry.last_placement)
        else {
            continue; // attach 전 / placement 없음 → 표시할 프레임 없음.
        };
        pending.push(PendingExt {
            id: *id,
            external_id,
            is_opaque: entry.is_opaque,
            ref_size: ext.swapchain_size,
            clip_rect: placement.clip_rect,
            transform_offset: placement.transform_offset,
        });
    }
    if pending.is_empty() {
        return;
    }

    for p in pending {
        // acquire↔release 짝맞춤(add_external_surface와 동일 계약).
        let Some(lease) = provider.acquire(&*rc, p.external_id) else {
            continue;
        };
        let ring_id = lease.ring_id;
        // transform 복원: external은 scale 무시(dest=clip), offset은 디버그 로그용.
        let transform = CompositorSurfaceTransform::translation(
            p.transform_offset.0,
            p.transform_offset.1,
            0.0,
        );
        self.present_external(
            p.id, &lease, p.ref_size, p.is_opaque, false, transform, p.clip_rect,
        );
        provider.release(&*rc, ring_id);
    }

    // external convert 배치 닫기(정확성 필수, 설계 §4.2): begin_batch로 활성화된
    // ID3DDeviceContextState를 반드시 닫아 다음 프레임/경로 GL 안전성 보장.
    self.close_external_batch();

    // Commit 1회.
    if let Some(dcomp_device) = self.dcomp_device_ptr() {
        let hr = unsafe { (*dcomp_device).Commit() };
        if hr < 0 {
            warn!("[dcomp-native] present_external_only Commit failed (hr=0x{:08x})", hr as u32);
        }
    }
}
```

주의: `CompositorSurfaceTransform::translation` 시그니처가 이 WR 버전과 다르면(예: `Transform3D::translation`) 빌드 에러로 드러난다 — 그 경우 `place_external_visual`(1402)이 transform을 쓰는 방식과 동일하게 맞춘다. `NativeSurfaceId`/`DeviceIntSize`/`DeviceIntRect`는 이 파일에서 이미 import됨.

- [ ] **Step 3: 빌드 확인**

Run: `./mach build -r 2>&1 | tail -20`
Expected: 컴파일 성공(경고 무방). 실패 시 위 transform 복원부/타입 정합만 수정.

- [ ] **Step 4: 커밋**

```bash
git add components/paint/dcomp_compositor.rs
git commit -m "external 비디오 분리: 컴포지터 경량 경로 present_external_only(빌드 없이 승격 external만 present+Commit) + escaped_external_count"
git log -1 --format=%B
```

---

## Task 3: 공유 핸들 — Rc<RefCell> + SharedDComp 위임 래퍼

**Files:**
- Modify: `components/paint/dcomp_compositor.rs` (`impl Compositor for DCompNativeCompositor` 블록 ~1839-3140 뒤에 `SharedDComp` 추가; `maybe_create` 반환형 ~1186-1327)
- Modify: `components/paint/painter.rs` (`CompositorConfig::Native` 배선 ~445-462, `Painter` 필드 ~147-230, 생성 ~585)

**Interfaces:**
- Consumes: 기존 `impl Compositor for DCompNativeCompositor`의 전 메서드, `webrender::Compositor` 트레이트, `maybe_create`.
- Produces:
  - `pub struct SharedDComp(pub Rc<RefCell<DCompNativeCompositor>>)` + `impl Compositor for SharedDComp`(전 메서드 위임) + `impl SharedDComp { pub(crate) fn present_external_only(&self); pub(crate) fn escaped_external_count(&self) -> usize; }`
  - `maybe_create` → `Option<Rc<RefCell<DCompNativeCompositor>>>`
  - `Painter.dcomp_shared: Option<Rc<RefCell<DCompNativeCompositor>>>`

- [ ] **Step 1: SharedDComp 래퍼 추가**

`dcomp_compositor.rs`의 `impl Compositor for DCompNativeCompositor { ... }` 블록 **직후**에 추가. `webrender::Compositor`(또는 이 파일이 `use`한 `Compositor` 경로)의 **모든** 메서드를 `self.0.borrow_mut().<method>(..)`로 위임한다. 아래 목록은 기존 impl(파일 내 존재)과 1:1 대응하며 시그니처를 그대로 복사한다:

```rust
use std::cell::RefCell;
use std::rc::Rc;

/// WR은 Box<dyn Compositor>로 컴포지터를 소유한다. painter도 external fast-path를
/// 위해 같은 인스턴스에 접근해야 하므로, 실상태를 Rc<RefCell>에 두고 WR엔 이 얇은
/// 위임 래퍼를 넘긴다. 전부 단일 렌더러 스레드에서 돌아 Rc<RefCell> 안전(WR render()의
/// 트레이트 대여와 painter의 fast-path 대여는 동일 스레드·프레임당 단일 경로라 비중첩).
pub struct SharedDComp(pub Rc<RefCell<DCompNativeCompositor>>);

impl SharedDComp {
    pub(crate) fn present_external_only(&self) {
        self.0.borrow_mut().present_external_only();
    }
    pub(crate) fn escaped_external_count(&self) -> usize {
        self.0.borrow().escaped_external_count()
    }
}

impl Compositor for SharedDComp {
    fn create_surface(
        &mut self, device: &mut Device, id: NativeSurfaceId,
        virtual_offset: DeviceIntPoint, tile_size: DeviceIntSize, is_opaque: bool,
    ) {
        self.0.borrow_mut().create_surface(device, id, virtual_offset, tile_size, is_opaque)
    }
    fn create_tile(&mut self, device: &mut Device, id: NativeTileId) {
        self.0.borrow_mut().create_tile(device, id)
    }
    fn destroy_tile(&mut self, device: &mut Device, id: NativeTileId) {
        self.0.borrow_mut().destroy_tile(device, id)
    }
    fn bind(
        &mut self, device: &mut Device, id: NativeTileId,
        dirty_rect: DeviceIntRect, valid_rect: DeviceIntRect,
    ) -> NativeSurfaceInfo {
        self.0.borrow_mut().bind(device, id, dirty_rect, valid_rect)
    }
    fn unbind(&mut self, device: &mut Device) {
        self.0.borrow_mut().unbind(device)
    }
    fn begin_frame(&mut self, device: &mut Device) {
        self.0.borrow_mut().begin_frame(device)
    }
    fn add_surface(
        &mut self, device: &mut Device, id: NativeSurfaceId,
        transform: CompositorSurfaceTransform, clip_rect: DeviceIntRect,
        image_rendering: ImageRendering, rounded_clip_rect: DeviceIntRect,
        rounded_clip_radii: ClipRadius,
    ) {
        self.0.borrow_mut().add_surface(
            device, id, transform, clip_rect, image_rendering, rounded_clip_rect, rounded_clip_radii,
        )
    }
    fn start_compositing(
        &mut self, device: &mut Device, clear_color: ColorF,
        dirty_rects: &[DeviceIntRect], opaque_rects: &[DeviceIntRect],
    ) {
        self.0.borrow_mut().start_compositing(device, clear_color, dirty_rects, opaque_rects)
    }
    fn end_frame(&mut self, device: &mut Device) {
        self.0.borrow_mut().end_frame(device)
    }
    fn destroy_surface(&mut self, device: &mut Device, id: NativeSurfaceId) {
        self.0.borrow_mut().destroy_surface(device, id)
    }
    fn create_external_surface(&mut self, device: &mut Device, id: NativeSurfaceId, is_opaque: bool) {
        self.0.borrow_mut().create_external_surface(device, id, is_opaque)
    }
    fn attach_external_image(
        &mut self, device: &mut Device, id: NativeSurfaceId, external_image: ExternalImageId,
    ) {
        self.0.borrow_mut().attach_external_image(device, id, external_image)
    }
    fn create_backdrop_surface(&mut self, device: &mut Device, id: NativeSurfaceId, color: ColorF) {
        self.0.borrow_mut().create_backdrop_surface(device, id, color)
    }
    fn enable_native_compositor(&mut self, device: &mut Device, enable: bool) {
        self.0.borrow_mut().enable_native_compositor(device, enable)
    }
    fn get_capabilities(&self, device: &mut Device) -> CompositorCapabilities {
        self.0.borrow().get_capabilities(device)
    }
    fn get_window_visibility(&self, device: &mut Device) -> WindowVisibility {
        self.0.borrow().get_window_visibility(device)
    }
    fn deinit(&mut self, device: &mut Device) {
        self.0.borrow_mut().deinit(device)
    }
}
```

각 메서드의 정확한 파라미터명/타입은 이 파일의 기존 `impl Compositor for DCompNativeCompositor`에서 그대로 복사한다(위 목록이 어긋나면 빌드 에러로 드러남 — 기존 impl에 맞춘다). `get_capabilities`/`get_window_visibility`는 `&self`라 `borrow()` 사용.

- [ ] **Step 2: maybe_create 반환형 변경**

`maybe_create`(1186)의 마지막 `Some(DCompNativeCompositor { ... })`(1300)를 감싼다:

```rust
pub fn maybe_create(
    rendering_context: &Rc<dyn RenderingContext>,
) -> Option<Rc<RefCell<DCompNativeCompositor>>> {
    // ... 기존 본문 그대로 ...
    Some(Rc::new(RefCell::new(DCompNativeCompositor {
        // ... 기존 필드 초기화 그대로 ...
    })))
}
```

- [ ] **Step 3: painter 배선 변경**

`painter.rs`:
1. `Painter` 구조체(147)에 필드 추가(webrender_renderer:220 부근):
```rust
    /// external fast-path용 DComp 컴포지터 공유 핸들(WR가 Box<SharedDComp>로 같은 인스턴스 소유).
    pub(crate) dcomp_shared: Option<std::rc::Rc<std::cell::RefCell<crate::dcomp_compositor::DCompNativeCompositor>>>,
```
2. `compositor_config` 배선(445-462)을 수정해 Rc를 보관하고 WR엔 래퍼를 넘긴다:
```rust
        let (compositor_config, dcomp_shared) = if crate::dcomp_compositor::enabled() {
            match crate::dcomp_compositor::maybe_create(&rendering_context) {
                Some(shared) => {
                    log::info!("[dcomp-native] native compositor enabled");
                    (
                        webrender::CompositorConfig::Native {
                            compositor: Box::new(crate::dcomp_compositor::SharedDComp(shared.clone())),
                        },
                        Some(shared),
                    )
                },
                None => (webrender::CompositorConfig::default(), None),
            }
        } else {
            (webrender::CompositorConfig::default(), None)
        };
```
(기존 445-469의 `let compositor_config = ...` 두 분기와 `let compositor_config = webrender::CompositorConfig::default();` 라인을 위 형태로 통합. `is_native` 판정 472는 `compositor_config` 그대로 사용.)
3. 생성부(585 `webrender_renderer: Some(webrender_renderer),`)에 `dcomp_shared,` 추가.

- [ ] **Step 4: 빌드 확인**

Run: `./mach build -r 2>&1 | tail -20`
Expected: 컴파일 성공. 래퍼 시그니처가 어긋나면 여기서 드러남 — 기존 impl과 정합.

- [ ] **Step 5: 런타임 무회귀 스모크(승격/합성 정상)**

Run(3x3 축소, memory 검증 커맨드):
```
etc/multigpu/run_video_wall_d3d11.ps1 -DComp -VideoEscape external -Sync 9
```
Expected: 9타일 비디오 정상 표출(기존과 동일). 아직 fast-path 미배선이라 동작 변화 없음 — SharedDComp 위임이 무회귀인지만 확인.

- [ ] **Step 6: 커밋**

```bash
git add components/paint/dcomp_compositor.rs components/paint/painter.rs
git commit -m "external 비디오 분리: 컴포지터 Rc<RefCell> 공유 핸들 + SharedDComp 위임 래퍼(WR 소유 유지, painter 접근 가능)"
git log -1 --format=%B
```

---

## Task 4: painter 게이트 배선 + refresh 페이싱

**Files:**
- Modify: `components/paint/painter.rs` (`Painter` 필드 ~147, 즉시-합성 게이트 ~2027-2036)

**Interfaces:**
- Consumes: `SharedDComp::present_external_only`/`escaped_external_count`(Task 2/3), `should_fast_present`/`decouple_enabled`(Task 1), 게이트의 기존 지역값 `immediate_image_update`, `generated_frame`, `self.pending_frames`, `self.renderer_behind()`, `raf_driving_composites`, `rc.dcomp_resize_active()`.

- [ ] **Step 1: 페이싱 상태 필드 추가**

`Painter`(147)에 추가:
```rust
    /// external fast-present 마지막 시각(refresh 페이싱: ~60/s로 coalesce, 도착률 ~1080/s 방지).
    pub(crate) last_external_present: std::cell::Cell<Option<std::time::Instant>>,
```
생성부(585 부근)에 `last_external_present: std::cell::Cell::new(None),` 추가.

- [ ] **Step 2: 게이트에 fast-path 분기**

`painter.rs:2027-2036`의 기존 블록을 다음으로 교체. **fast-path 우선**: 조건 충족 시 `generate_frame` 대신 `present_external_only`(refresh 페이싱). 미충족 시 기존 `generate_frame` 경로 그대로:

```rust
        let raf_driving_composites = self.animation_callbacks_running();
        let escaped_count = self
            .dcomp_shared
            .as_ref()
            .map(|c| crate::dcomp_compositor::SharedDComp(c.clone()).escaped_external_count())
            .unwrap_or(0);
        let resize_active = self.rendering_context.dcomp_resize_active();
        if crate::dcomp_compositor::should_fast_present(
            immediate_image_update,
            generated_frame,
            self.pending_frames.get() == 0,
            self.renderer_behind(),
            raf_driving_composites,
            escaped_count,
            resize_active,
            crate::dcomp_compositor::decouple_enabled(),
        ) {
            // refresh 페이싱: 직전 fast-present 후 ~14ms 경과 시에만(≈60/s). 도착마다
            // present하면 ~1080 Commit/s가 되므로 coalesce. dedup(external_needs_present)이
            // 프레임 안 바뀐 비디오는 스킵하므로 이 present가 모든 최신 프레임을 반영.
            let now = std::time::Instant::now();
            let due = self
                .last_external_present
                .get()
                .map(|t| now.duration_since(t) >= std::time::Duration::from_millis(14))
                .unwrap_or(true);
            if due {
                if let Some(shared) = self.dcomp_shared.as_ref() {
                    crate::dcomp_compositor::SharedDComp(shared.clone()).present_external_only();
                    self.last_external_present.set(Some(now));
                }
            }
            // fast-path를 탔으므로 generate_frame(전체 빌드)은 하지 않는다.
        } else if immediate_image_update &&
            !generated_frame &&
            self.pending_frames.get() == 0 &&
            !raf_driving_composites &&
            !self.renderer_behind() &&
            !*VIDEO_IMMEDIATE_COMPOSITE_DISABLED
        {
            // 기존 경로(미승격/하이브리드/킬스위치 off): 비디오당 전체 씬 재합성.
            self.generate_frame(&mut txn, RenderReasons::SCENE);
            self.display_composite_in_flight.set(true);
        }
```

주의: `SharedDComp(c.clone())`를 매번 만드는 대신, `escaped_external_count`/`present_external_only`를 `Rc<RefCell<..>>`에서 직접 부를 수 있게 Task 3의 `SharedDComp` 메서드를 `DCompNativeCompositor`의 pub(crate) 메서드에 위임하는 형태이므로, 여기서는 `self.dcomp_shared.as_ref()`의 `borrow()/borrow_mut()`를 직접 써도 된다(더 간결). 예:
```rust
        let escaped_count = self.dcomp_shared.as_ref().map(|c| c.borrow().escaped_external_count()).unwrap_or(0);
        // ...
        if let Some(shared) = self.dcomp_shared.as_ref() { shared.borrow_mut().present_external_only(); ... }
```
빌드가 통과하는 더 간결한 쪽을 택한다(임시 SharedDComp 생성 회피 권장).

- [ ] **Step 3: 빌드 확인**

Run: `./mach build -r 2>&1 | tail -20`
Expected: 컴파일 성공.

- [ ] **Step 4: 기능 스모크(fast-path 발동 + 킬스위치)**

Run A (fast-path on, 3x3):
```
etc/multigpu/run_video_wall_d3d11.ps1 -DComp -VideoEscape external -Sync 9
```
Expected: 9타일 비디오 정상 재생(정지·검정 없음), 시계/티커가 있으면 정상 갱신.

Run B (킬스위치로 기존 동작):
```
$env:SERVO_VIDEO_DECOUPLE=0; etc/multigpu/run_video_wall_d3d11.ps1 -DComp -VideoEscape external -Sync 9
```
Expected: 동일 표출(기존 generate_frame 경로). A/B 육안 등가.

- [ ] **Step 5: 커밋**

```bash
git add components/paint/painter.rs
git commit -m "external 비디오 분리: 즉시-합성 게이트에 fast-path 배선(승격 external present, refresh 페이싱) — 비디오당 전체 씬 빌드 제거"
git log -1 --format=%B
```

---

## Task 5: 라이브 검증 (AMD 주 게이트 + A5000 무회귀)

**Files:** 없음(관찰·측정만). 결함 발견 시 해당 Task로 회귀.

- [ ] **Step 1: AMD 주 게이트 — RenderBackend 포화 해소 + fps**

AMD 실기에서 큰 창·36타일:
```
etc/multigpu/run_video_wall_d3d11.ps1 -DComp -VideoEscape external -Sync 36
```
관찰: (a) 작업관리자/프로파일러에서 **RenderBackend(WR) 스레드가 더는 100% 포화가 아님**, (b) 큰 창에서 fps 유지/개선. `SERVO_VIDEO_DECOUPLE=0` A/B로 대조(off=하락, on=개선).
Expected: on에서 큰 창 하락이 사라지거나 유의미 개선.

- [ ] **Step 2: 콘텐츠 정합**

시계/티커/자막 포함 페이지(예: mixed/complex 데모)에서 비디오+콘텐츠 동시 표출. 비디오 60fps, 콘텐츠(시계/티커) 정상 갱신·정지 없음. 복합 페이지의 회전(미승격) 타일도 정지 없이(script 케이던스로) 갱신.
Expected: 콘텐츠 스테일/정지 없음.

- [ ] **Step 3: 리사이즈/드래그 무회귀**

창 드래그 리사이즈 중 비디오 검정·잔상 없음(task-12b 경로 유지 — fast-path는 resize_active에 양보).
Expected: 기존과 동일한 드래그 표시.

- [ ] **Step 4: A5000 무회귀**

A5000에서 45타일:
```
etc/multigpu/run_video_wall_d3d11.ps1 -DComp -VideoEscape external -Sync 45
```
관찰: lockstep/fps 무회귀, 짧은 소크 클린.
Expected: 기존 대비 무회귀.

- [ ] **Step 5: (선택) Present×N 2단계 판정**

fast-path 후 `[vesc-prof]`(SERVO_VIDEO_ESCAPE_PROF=1) 또는 프로파일러로 present/Commit 부하 확인. 만약 present×N이 새 병목으로 드러나면(빌드 노이즈 제거로 이제 격리 측정 가능) 설계 §4.6 2단계(공유 캔버스 붕괴)를 **별도 계획**으로 착수. 아니면 종료.
Expected: 대개 종료. 2단계는 조건부.

- [ ] **Step 6: 검증 결과 기록**

결과를 메모리([[video-grid-play-heartbeat]])에 반영: 근본원인(per-video generate_frame 빌드 플러드) + fast-path 해소 여부 + AMD/A5000 수치.

---

## Self-Review (작성자 체크)

- **스펙 커버리지**: §4.1 공유핸들→Task 3, §4.2 present_external_only(+배치 닫기)→Task 2, §4.3 게이트 라우팅→Task 1(판정)+Task 4(배선), §4.4 vsync 페이싱→Task 4 Step 2(14ms throttle), §4.5 리사이즈 양보→Task 1(resize_active 판정)+Task 2/4(방어), §4.6 Present×N 2단계→Task 5 Step 5(조건부), §4.7 킬스위치→Task 1. §7 검증→Task 5. 누락 없음.
- **플레이스홀더 스캔**: TODO/TBD/"적절히 처리" 없음. 각 코드 스텝에 실제 코드 존재.
- **타입 일관성**: `should_fast_present`/`decouple_enabled`/`escaped_external_count`/`present_external_only`/`SharedDComp`/`dcomp_shared`/`last_external_present` 명칭이 Task 1→4 전반에서 동일.
- **알려진 확인점(빌드가 강제)**: (a) `CompositorSurfaceTransform` 생성 시그니처(Task 2), (b) `impl Compositor` 메서드 시그니처 정합(Task 3), (c) 게이트에서 borrow vs 임시 SharedDComp 중 간결한 쪽(Task 4). 셋 다 빌드 에러로 즉시 드러나며 수정 방향 명시함.

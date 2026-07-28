# 월 가드밴드 크롭 present_inset 통합 + 세로축 수정 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 월 타일 가드밴드(`overlapPx`) 크롭을 `RenderingContext::present_inset` 하나로 정본화하고, DComp 네이티브 컴포지터 경로에도 적용하며, blit 경로의 세로 inset 규약 오류를 함께 고친다.

**Architecture:** 크롭 값은 `WindowRenderingContext`의 `Cell<DeviceIntSideOffsets>`에 창당 1회 저장된다(servoshell이 창 생성 시 세팅, `OffscreenRenderingContext`는 부모로 위임). 소비자는 둘 — DComp 컴포지터는 root visual에 `SetOffsetX/Y(-left, -top)`를 걸고, 오프스크린→창 blit은 source rect 원점에 `(left, bottom)`을 쓴다. y 규약이 다른 이유는 DComp가 top-left 원점, GL 프레임버퍼가 bottom-left 원점이기 때문이다.

**Tech Stack:** Rust 1.95.0 / Windows / Servo 포크 / WebRender native compositor(DirectComposition) / surfman+ANGLE / euclid

**설계 문서:** `docs/superpowers/specs/2026-07-28-wall-guard-band-present-inset-design.md` — 배경·원인 규명·기각한 대안은 거기 있다. 이 계획은 실행 절차만 담는다.

## Global Constraints

- 대상 워크트리 `W:\servo_multigpu-tiled-wall`(= `subst W:` 한 리포 루트 아래), 브랜치 `nonstandard-media-display-port`. 긴 경로에서 빌드하면 mozangle이 `Os error 206`으로 죽으므로 **반드시 `W:` 경로에서 작업한다.**
- 모든 빌드/실행 전에 리포 루트에서 `. .\scripts\servo_env.ps1`을 **점-소싱**한다(앞의 `.` 필수).
- 빌드는 `.\mach build -r`(릴리스). `cargo build -p servoshell`은 media-gstreamer 피처가 빠져 더미 미디어 백엔드가 되므로 런타임 검증에 쓰지 않는다. 타입 체크만 필요한 단계에서는 `cargo check -p servoshell` / `cargo check -p servo-paint-api`를 쓴다.
- 런타임 검증은 **release 빌드**로만 한다. debug 빌드는 월 + 동적 video src에서 `MakeCurrentFailed`로 죽는다.
- 창을 **닫아서** 종료해야 stderr가 flush된다. 강제 종료하면 `.err.log`가 빈 파일이 된다.
- `SERVO_COMPOSITOR_DCOMP` 게이트는 **기본 off**이며 off 경로는 무회귀여야 한다(Task 5의 y 수정만 예외 — 상하 오버랩이 0인 현행 구성에서는 동작 동일).
- 새 로그 줄을 추가할 때 `Wall repaint target:` / `Wall window present:` 두 줄의 형식은 **건드리지 않는다** — `tools/wall_perf_analyzer/analyze_wall_perf.py:28,34`가 정규식으로 파싱한다. 시작 진단 줄(`Wall tile N display ...`)은 파싱 대상이 아니라 확장해도 안전하다.
- 커밋 메시지는 리포 관례대로 한국어 + `feat:`/`fix:`/`test:`/`docs:` 접두.

---

## File Structure

| 파일 | 역할 | Task |
|---|---|---|
| `ports/servoshell/wall_layout.rs` | inset 순수 계산 + 단위 테스트(정본 산식) | 1 |
| `ports/servoshell/desktop/headed_window.rs` | layout으로 위임, 컨텍스트에 inset 주입, blit source rect | 1, 2, 5 |
| `components/shared/paint/rendering_context.rs` | `present_inset` 트레잇 + Window 저장 + Offscreen 위임 | 2 |
| `tests/html/multigpu_wall_ruler_probe.html` | **신규** 절대 가상좌표 눈금 프로브(모든 런타임 검증 공용) | 3 |
| `etc/multigpu/config/wall_layout.test_1x2_vertical.json` | **신규** 세로 2타일 layout(y축 검증용) | 3 |
| `components/paint/dcomp_compositor.rs` | root visual 가드밴드 오프셋 | 4 |
| `CLAUDE.md` | 단위 테스트 개수 갱신 | 6 |

---

### Task 1: `wall_layout::tile_render_insets` — inset 산식의 정본화

**Files:**
- Modify: `ports/servoshell/wall_layout.rs` (import 9행, `impl WallLayout` 끝 `:139` 앞, `mod tests`)
- Modify: `ports/servoshell/desktop/headed_window.rs:17-19` (import), `:847-881` (`wall_tile_render_insets` / `webview_rendering_size` / `webview_visible_source_rect`)

**Interfaces:**
- Produces: `WallLayout::tile_render_insets(&self, tile_index: usize, hidpi_scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>) -> Option<SideOffsets2D<i32, DevicePixel>>` — Task 2가 servoshell에서, Task 4/5가 그 값을 컨텍스트 경유로 소비한다. `SideOffsets2D::new(top, right, bottom, left)` 인자 순서에 주의.
- Produces: `HeadedWindow::wall_tile_render_insets(&self) -> Option<SideOffsets2D<i32, DevicePixel>>` — 튜플 `(left, top, right, bottom)`을 반환하던 기존 시그니처를 대체한다.

**이 Task는 동작을 바꾸지 않는다.** 계산 위치만 옮기고 반환 타입을 구조체로 바꾼다. Task 5에서 실제 y 수정이 들어간다.

- [ ] **Step 1: 실패하는 단위 테스트 4개를 작성한다**

`ports/servoshell/wall_layout.rs`의 `mod tests` 안, 마지막 테스트(`rejects_tile_without_display_or_monitor`) 뒤에 추가:

```rust
    /// 스케일 1.0에서 타일의 가드밴드 여백을 뽑는 테스트 헬퍼.
    fn insets(layout: &WallLayout, tile_index: usize) -> SideOffsets2D<i32, DevicePixel> {
        layout
            .tile_render_insets(tile_index, Scale::new(1.0))
            .expect("tile index should be valid")
    }

    #[test]
    fn tile_render_insets_2x2() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 3840, "height": 2160 },
                "tiles": [
                    { "display": 0, "rect": [0, 0, 1920, 1080] },
                    { "display": 1, "rect": [1920, 0, 1920, 1080] },
                    { "display": 2, "rect": [0, 1080, 1920, 1080] },
                    { "display": 3, "rect": [1920, 1080, 1920, 1080] }
                ],
                "overlapPx": 32
            }"#,
        )
        .expect("valid layout should parse");

        // SideOffsets2D::new(top, right, bottom, left).
        // 가드밴드는 항상 가상 뷰포트 '안쪽' 변에만 붙는다(바깥 변은 클램프).
        assert_eq!(insets(&layout, 0), SideOffsets2D::new(0, 32, 32, 0));
        assert_eq!(insets(&layout, 1), SideOffsets2D::new(0, 0, 32, 32));
        assert_eq!(insets(&layout, 2), SideOffsets2D::new(32, 32, 0, 0));
        assert_eq!(insets(&layout, 3), SideOffsets2D::new(32, 0, 0, 32));
    }

    #[test]
    fn tile_render_insets_1x2_vertical() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 1920, "height": 2160 },
                "tiles": [
                    { "display": 0, "rect": [0, 0, 1920, 1080] },
                    { "display": 1, "rect": [0, 1080, 1920, 1080] }
                ],
                "overlapPx": 32
            }"#,
        )
        .expect("valid layout should parse");

        assert_eq!(insets(&layout, 0), SideOffsets2D::new(0, 0, 32, 0));
        assert_eq!(insets(&layout, 1), SideOffsets2D::new(32, 0, 0, 0));
    }

    #[test]
    fn tile_render_insets_zero_without_overlap() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 3840, "height": 1080 },
                "tiles": [
                    { "display": 0, "rect": [0, 0, 1920, 1080] },
                    { "display": 1, "rect": [1920, 0, 1920, 1080] }
                ]
            }"#,
        )
        .expect("valid layout should parse");

        assert_eq!(insets(&layout, 0), SideOffsets2D::zero());
        assert_eq!(insets(&layout, 1), SideOffsets2D::zero());
    }

    #[test]
    fn tile_render_insets_clamped_when_overlap_exceeds_viewport() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 200, "height": 100 },
                "tiles": [
                    { "display": 0, "rect": [0, 0, 100, 100] },
                    { "display": 1, "rect": [100, 0, 100, 100] }
                ],
                "overlapPx": 500
            }"#,
        )
        .expect("valid layout should parse");

        // 오버랩이 뷰포트를 넘으면 render rect가 뷰포트 전체로 클램프된다.
        assert_eq!(insets(&layout, 0), SideOffsets2D::new(0, 100, 0, 0));
        assert_eq!(insets(&layout, 1), SideOffsets2D::new(0, 0, 0, 100));
    }

    #[test]
    fn tile_render_insets_none_for_out_of_range_tile() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 1920, "height": 1080 },
                "tiles": [ { "display": 0, "rect": [0, 0, 1920, 1080] } ]
            }"#,
        )
        .expect("valid layout should parse");

        assert!(layout.tile_render_insets(1, Scale::new(1.0)).is_none());
    }
```

- [ ] **Step 2: 테스트가 컴파일 실패하는지 확인한다**

```powershell
cd W:\servo_multigpu-tiled-wall
cargo test -p servoshell wall_layout --lib
```
기대: 컴파일 에러 — ``no method named `tile_render_insets` found for struct `WallLayout` `` 및 `cannot find type SideOffsets2D`.

- [ ] **Step 3: import에 `SideOffsets2D`를 추가한다**

`ports/servoshell/wall_layout.rs:9`:

```rust
use euclid::{Point2D, Rect, Scale, SideOffsets2D, Size2D, Vector2D};
```

- [ ] **Step 4: `tile_render_insets`를 구현한다**

`ports/servoshell/wall_layout.rs`, `tile_render_device_rect`(`:131-138`) 바로 뒤, `impl WallLayout`의 닫는 `}`(`:139`) 앞에 추가:

```rust
    /// 이 타일의 가드밴드 여백 — 오버랩 확장된 render rect 대비 visible rect의 각 변 간격.
    /// device px, **top-left 기준**. 표출 경로가 렌더 서피스에서 잘라내야 할 양이다.
    ///
    /// 소비자마다 y 규약이 다르다: GL blit(프레임버퍼 bottom-left 원점)은 src y 원점에
    /// `bottom`을, DComp(top-left 원점)는 root visual 오프셋에 `-top`을 쓴다
    /// (§`RenderingContext::present_inset`).
    pub(crate) fn tile_render_insets(
        &self,
        tile_index: usize,
        hidpi_scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,
    ) -> Option<SideOffsets2D<i32, DevicePixel>> {
        let visible = self.tile_device_rect(tile_index, hidpi_scale_factor)?;
        let render = self.tile_render_device_rect(tile_index, hidpi_scale_factor)?;
        Some(SideOffsets2D::new(
            (visible.min.y - render.min.y).max(0),
            (render.max.x - visible.max.x).max(0),
            (render.max.y - visible.max.y).max(0),
            (visible.min.x - render.min.x).max(0),
        ))
    }
```

- [ ] **Step 5: 단위 테스트가 통과하는지 확인한다**

```powershell
cargo test -p servoshell wall_layout --lib
```
기대: PASS. 기존 6개 + 신규 5개 = **11 passed**.

- [ ] **Step 6: `headed_window`를 새 API로 위임시킨다**

`ports/servoshell/desktop/headed_window.rs:17-19` import에 `SideOffsets2D` 추가:

```rust
use euclid::{
    Angle, Length, Point2D, Rect, Rotation3D, Scale, SideOffsets2D, Size2D, UnknownUnit, Vector2D,
    Vector3D,
};
```

`:847-881`의 세 메서드를 통째로 교체:

```rust
    fn wall_tile_render_insets(&self) -> Option<SideOffsets2D<i32, DevicePixel>> {
        self.wall_layout
            .as_ref()?
            .tile_render_insets(self.wall_tile_index, self.hidpi_scale_factor())
    }

    pub(crate) fn webview_rendering_size(
        &self,
        visible_size: Size2D<f32, DevicePixel>,
    ) -> Size2D<f32, DevicePixel> {
        let Some(inset) = self.wall_tile_render_insets() else {
            return visible_size;
        };
        Size2D::new(
            visible_size.width + inset.left as f32 + inset.right as f32,
            visible_size.height + inset.top as f32 + inset.bottom as f32,
        )
    }

    pub(crate) fn webview_visible_source_rect(
        &self,
        visible_size: Size2D<i32, DevicePixel>,
    ) -> Rect<i32, DevicePixel> {
        let Some(inset) = self.wall_tile_render_insets() else {
            return Rect::new(Point2D::origin(), visible_size);
        };
        Rect::new(Point2D::new(inset.left, inset.top), visible_size)
    }
```

`.max(0)` 호출이 사라진 것은 의도적이다 — `tile_render_insets`가 이미 클램프한다. `inset.top`은 **아직 그대로 둔다**(Task 5에서 `inset.bottom`으로 고친다). 이 Task는 순수 리팩터링이다.

- [ ] **Step 7: 타입 체크가 통과하는지 확인한다**

```powershell
cargo check -p servoshell
```
기대: 에러 0.

- [ ] **Step 8: 포맷 확인 후 커밋**

```powershell
rustfmt --edition 2024 --check ports\servoshell\wall_layout.rs ports\servoshell\desktop\headed_window.rs
git diff --check
git add ports/servoshell/wall_layout.rs ports/servoshell/desktop/headed_window.rs
git commit -m "refactor(wall): 타일 가드밴드 inset 계산을 wall_layout으로 승격 + 단위 테스트 5개

headed_window의 창 인스턴스 메서드라 테스트 불가였던 산식을 WallLayout::
tile_render_insets로 옮기고 SideOffsets2D로 반환한다. 동작 무변경."
```

---

### Task 2: `RenderingContext::present_inset` — 크롭 값의 단일 정본

**Files:**
- Modify: `components/shared/paint/rendering_context.rs:41` (import), `:122` 뒤(트레잇), `:1293-1310`(필드), `:1402-1412`(생성자), `:1489` 앞(Window impl), `:2026-2028` 뒤(Offscreen 위임)
- Modify: `ports/servoshell/desktop/headed_window.rs:439-466` (창 생성 시 주입 + 진단 로그)

**Interfaces:**
- Consumes: Task 1의 `WallLayout::tile_render_insets`.
- Produces: `RenderingContext::present_inset(&self) -> DeviceIntSideOffsets` (기본 `zero`), `RenderingContext::set_present_inset(&self, inset: DeviceIntSideOffsets)` (기본 no-op). Task 4가 `present_inset()`을 읽는다. `DeviceIntSideOffsets`는 `webrender_api::units`의 별칭이며 servoshell의 `SideOffsets2D<i32, DevicePixel>`과 **같은 타입**이다.

- [ ] **Step 1: 트레잇에 두 메서드를 추가한다**

`components/shared/paint/rendering_context.rs:41` import 확장:

```rust
use webrender_api::units::{DeviceIntRect, DeviceIntSideOffsets, DevicePixel};
```

`:122`의 `fn present(&self);` 바로 뒤에 삽입:

```rust
    /// 이 컨텍스트가 렌더한 서피스 중 실제로 화면에 표출되는 sub-rect를 정의하는 가드밴드
    /// 여백. device px, **top-left 기준**. 월 타일에서 `overlapPx`로 확장한 render rect와
    /// visible rect의 차이이며, 비월 모드는 zero(트레잇 기본값).
    ///
    /// 소비자는 둘이고 y 규약이 서로 다르다:
    /// - 오프스크린→창 blit(servoshell `gui.rs`): GL 프레임버퍼는 bottom-left 원점이라
    ///   source rect y 원점에 `bottom`을 쓴다.
    /// - DComp 네이티브 컴포지터: top-left 원점이라 root visual 오프셋에 `-top`을 쓴다.
    fn present_inset(&self) -> DeviceIntSideOffsets {
        DeviceIntSideOffsets::zero()
    }
    /// [`RenderingContext::present_inset`]을 설정한다. 월 타일 창은 non-resizable이므로
    /// servoshell이 창 생성 시 1회만 호출한다.
    fn set_present_inset(&self, _inset: DeviceIntSideOffsets) {}
```

- [ ] **Step 2: `WindowRenderingContext`에 저장소를 추가한다**

`:1309`의 `dcomp_resize_active: Cell<bool>,` 뒤, 구조체 닫는 `}`(`:1310`) 앞에 필드 추가. **`#[cfg(windows)]`를 붙이지 않는다** — blit 경로는 모든 플랫폼에서 이 값을 읽는다:

```rust
    /// 월 타일 가드밴드 여백 — §`RenderingContext::present_inset`. servoshell이 창 생성 시
    /// 1회 설정하고, DComp 경로(root visual 오프셋)와 blit 경로(source rect)가 함께 읽는다.
    present_inset: Cell<DeviceIntSideOffsets>,
```

`:1402-1412`의 유일한 생성자 `Ok(Self { ... })`에 초기값 추가(`dcomp_resize_active` 줄 뒤):

```rust
            present_inset: Cell::new(DeviceIntSideOffsets::zero()),
```

- [ ] **Step 3: `WindowRenderingContext`의 트레잇 구현을 추가한다**

`:1489`의 `fn size(&self) -> PhysicalSize<u32> {` 바로 앞에 삽입:

```rust
    fn present_inset(&self) -> DeviceIntSideOffsets {
        self.present_inset.get()
    }

    fn set_present_inset(&self, inset: DeviceIntSideOffsets) {
        self.present_inset.set(inset);
    }
```

- [ ] **Step 4: `OffscreenRenderingContext` 위임을 추가한다**

`:2026-2028`의 `fn dcomp_native_active(&self)` 구현 뒤에 삽입. **cfg 게이트 없음**:

```rust
    // servoshell이 Window를 Offscreen으로 감싸므로 painter/DComp 컴포지터/gui가 모두 이
    // Offscreen 래퍼 위에서 present_inset을 읽고 쓴다. 위임이 없으면 트레잇 기본값(zero)에
    // 흡수돼 가드밴드 크롭이 통째로 사라진다(dcomp_native_active와 동일 위임 패턴).
    fn present_inset(&self) -> DeviceIntSideOffsets {
        self.parent_context.present_inset()
    }

    fn set_present_inset(&self, inset: DeviceIntSideOffsets) {
        self.parent_context.set_present_inset(inset)
    }
```

- [ ] **Step 5: paint-api가 컴파일되는지 확인한다**

```powershell
cargo check -p servo-paint-api
```
기대: 에러 0.

- [ ] **Step 6: servoshell이 창 생성 시 inset을 주입하게 한다**

`ports/servoshell/desktop/headed_window.rs:444-447`의 `visible_rect`/`render_rect` 계산 뒤, `:458`의 `info!` 앞에 삽입:

```rust
            // 가드밴드 크롭의 정본. DComp 경로(root visual 오프셋)와 blit 경로(source rect)가
            // 모두 이 값을 읽는다. 월 타일 창은 non-resizable이므로 1회 설정으로 충분하다.
            let inset = layout
                .tile_render_insets(servoshell_preferences.wall_tile_index, hidpi_factor)
                .unwrap_or_else(SideOffsets2D::zero);
            rendering_context.set_present_inset(inset);
```

이어서 `:458-465`의 `info!`를 교체(끝에 `inset` 추가):

```rust
            info!(
                "Wall tile {} display {} {} visible rect {:?}, render rect {:?}, inset {:?}",
                servoshell_preferences.wall_tile_index,
                tile.display,
                gpu_label,
                visible_rect,
                render_rect,
                inset,
            );
```

이 줄은 `tools/wall_perf_analyzer/analyze_wall_perf.py`의 파싱 대상이 아니므로(그쪽은 `Wall repaint target:` / `Wall window present:` 두 줄만 본다) 확장해도 안전하다.

- [ ] **Step 7: 빌드하고 값이 실제로 실린 것을 로그로 확인한다**

```powershell
. .\scripts\servo_env.ps1
cd W:\servo_multigpu-tiled-wall
# servoshell의 SideOffsets2D<i32, DevicePixel>과 트레잇의 DeviceIntSideOffsets가
# 동일 타입으로 맞물리는지 여기서 먼저 걸린다(servo::DevicePixel == webrender_api::DevicePixel).
cargo check -p servoshell
.\mach build -r
.\target\release\servoshell.exe --wall-all-tiles `
  --wall-layout etc\multigpu\config\wall_layout.example_2x1_display.json `
  tests\html\multigpu_wall_sync_probe.html 2> ..\wall_inset_log.err.log
```
창 두 개가 뜨면 몇 초 뒤 **창을 닫아** 종료한 다음:

```powershell
Select-String -Path ..\wall_inset_log.err.log -Pattern "Wall tile \d+ display"
```
기대 출력 2줄:
- tile 0 → `inset SideOffsets2D { top: 0, right: 32, bottom: 0, left: 0 ... }`
- tile 1 → `inset SideOffsets2D { top: 0, right: 0, bottom: 0, left: 32 ... }`

tile 1의 `left`가 32가 아니면 여기서 멈추고 원인을 찾는다. 아직 화면 밀림은 **고쳐지지 않은 상태가 정상**이다(소비자가 없음).

- [ ] **Step 8: 포맷 확인 후 커밋**

```powershell
rustfmt --edition 2024 --check components\shared\paint\rendering_context.rs ports\servoshell\desktop\headed_window.rs
git diff --check
git add components/shared/paint/rendering_context.rs ports/servoshell/desktop/headed_window.rs
git commit -m "feat(paint): RenderingContext::present_inset — 월 가드밴드 크롭 값의 정본

WindowRenderingContext가 창당 1회 보관하고 Offscreen이 부모로 위임한다.
servoshell이 창 생성 시 wall_layout에서 계산해 주입하고 시작 진단 로그에 찍는다.
소비자(DComp root 오프셋 / blit source rect) 배선은 후속 커밋."
```

---

### Task 3: 검증 자산 — 눈금 프로브 + 세로 2타일 layout

**Files:**
- Create: `tests/html/multigpu_wall_ruler_probe.html`
- Create: `etc/multigpu/config/wall_layout.test_1x2_vertical.json`

**Interfaces:**
- Produces: 절대 가상좌표를 화면에서 읽을 수 있는 페이지. Task 4/5/6의 모든 런타임 검증이 이걸 쓴다. 판정 규칙 — **각 타일 창의 좌상단 구석에 보이는 좌표 라벨이 그 타일 `rect`의 원점과 일치해야 한다.**

- [ ] **Step 1: 눈금 프로브 페이지를 만든다**

`tests/html/multigpu_wall_ruler_probe.html`:

```html
<!doctype html>
<meta charset="utf-8">
<title>multigpu wall ruler probe</title>
<style>
  html, body { margin: 0; padding: 0; overflow: hidden; background: #101014; }
  /* 8px 미세 눈금 + 64px 주눈금. 가드밴드 32px = 미세 4칸 = 주눈금 반칸이라
     한 칸만 어긋나도 즉시 보인다. */
  #ruler {
    position: absolute; inset: 0;
    background-image:
      repeating-linear-gradient(to right,  #26263a 0 1px, transparent 1px 8px),
      repeating-linear-gradient(to bottom, #26263a 0 1px, transparent 1px 8px),
      repeating-linear-gradient(to right,  #5a5ab4 0 1px, transparent 1px 64px),
      repeating-linear-gradient(to bottom, #5a5ab4 0 1px, transparent 1px 64px);
  }
  .lbl {
    position: absolute; font: 11px/1 monospace; color: #7fb2d9;
    white-space: nowrap; pointer-events: none;
  }
  /* 640px 격자는 강조 — 타일 경계(보통 1920/1080의 배수)가 여기에 걸린다. */
  .lbl.major { color: #ffd75f; font-weight: bold; }
</style>
<div id="ruler"></div>
<script>
  // 가상 뷰포트 전체에 절대 좌표 라벨을 128px 간격으로 깐다. 라벨이 모든 타일에
  // 나타나야 하므로 한 축 가장자리가 아니라 격자 교차점마다 찍는다.
  const STEP = 128, MAJOR = 640;
  const w = document.documentElement.clientWidth;
  const h = document.documentElement.clientHeight;
  let html = '';
  for (let y = 0; y < h; y += STEP) {
    for (let x = 0; x < w; x += STEP) {
      const major = (x % MAJOR === 0) && (y % MAJOR === 0);
      html += `<span class="lbl${major ? ' major' : ''}" style="left:${x + 3}px;top:${y + 3}px">${x},${y}</span>`;
    }
  }
  document.getElementById('ruler').insertAdjacentHTML('beforeend', html);
  document.title = `ruler ${w}x${h}`;
</script>
```

- [ ] **Step 2: 세로 2타일 layout을 만든다**

`etc/multigpu/config/wall_layout.test_1x2_vertical.json`:

```json
{
  "virtualViewport": {
    "width": 1920,
    "height": 2160
  },
  "tiles": [
    {
      "display": 0,
      "rect": [0, 0, 1920, 1080]
    },
    {
      "display": 1,
      "rect": [0, 1080, 1920, 1080]
    }
  ],
  "overlapPx": 32
}
```

두 창은 물리적으로 좌우에 놓이므로 이음매가 연속으로 보이지는 않는다. 목적은 **세로 크롭 정확성 검증**이다 — display 1의 최상단이 가상 y=1080이어야 한다.

- [ ] **Step 3: 프로브 페이지 자체가 올바른지 단일 타일로 확인한다**

```powershell
. .\scripts\servo_env.ps1
cd W:\servo_multigpu-tiled-wall
$env:SERVO_COMPOSITOR_DCOMP=0
.\target\release\servoshell.exe --wall-all-tiles `
  --wall-layout etc\multigpu\config\wall_layout.example_1x1.json `
  tests\html\multigpu_wall_ruler_probe.html
```

기대: 창 좌상단 구석에 노란 `0,0` 라벨, 8px/64px 격자, 오른쪽으로 갈수록 `128,0` `256,0` … 증가. 1x1 layout은 `overlapPx: 32`지만 타일이 뷰포트 전체라 inset이 전부 0이므로 **크롭 없이 정확히 `0,0`에서 시작해야 한다.**

이 단계에서 라벨이 안 보이면 프로브 페이지 문제이지 크롭 문제가 아니다 — 여기서 잡고 간다.

- [ ] **Step 4: 커밋**

```powershell
git add tests/html/multigpu_wall_ruler_probe.html etc/multigpu/config/wall_layout.test_1x2_vertical.json
git commit -m "test(wall): 절대 가상좌표 눈금 프로브 + 세로 2타일 검증 layout

가드밴드 크롭 정확성을 눈대중이 아니라 좌표 라벨로 판정하기 위한 자산.
1x2 세로 layout은 blit(bottom)/DComp(top) y 규약 차이를 실기로 검증한다."
```

---

### Task 4: DComp root visual 가드밴드 오프셋

**Files:**
- Modify: `components/paint/dcomp_compositor.rs:1200-1203`(필드), `:1330-1354`(생성자), `:1952-1966`(`release_all`), `:2464-2481`(`begin_frame`)

**Interfaces:**
- Consumes: Task 2의 `RenderingContext::present_inset()`. `DCompNativeCompositor`는 `rendering_context: Rc<dyn RenderingContext>`를 이미 보유한다(`:1154`) — 이것이 servoshell이 넘긴 `OffscreenRenderingContext`이고 Task 2의 위임을 통해 부모 창 컨텍스트의 값을 읽는다.
- Produces: 없음(터미널 소비자).

**핵심 근거(구현자용):** 모든 비주얼 — WR picture 타일(`:2579`)과 external 비디오 서피스(`:1499`) — 이 같은 `frame_surfaces`에 쌓이고 `end_frame`(`:3117-3133`)이 전부 `root.AddVisual`한다. 따라서 root 한 곳의 오프셋이 비디오 포함 균일하게 적용된다. 자식 `SetClip`은 비주얼-로컬(오프셋 적용 전) 좌표라(`:538` 주석) 영향받지 않으므로 `apply_visual_placement`는 **건드리지 않는다.**

- [ ] **Step 1: 상태 필드를 추가한다**

`components/paint/dcomp_compositor.rs:1202`의 `esc_prof: EscProf,` 앞에 삽입:

```rust
    /// 마지막으로 root visual에 적용한 가드밴드 오프셋. `None` = 미적용(생성 직후).
    /// root visual은 `maybe_create`에서 1회 생성되고 `release_all`(deinit/Drop = teardown
    /// 전용)에서만 버려지므로, 런타임 중 재적용이 필요한 경로는 없다.
    last_root_offset: Option<(f32, f32)>,
```

`:1353`의 `esc_prof: EscProf::new(),` 앞에 초기값 추가:

```rust
            last_root_offset: None,
```

- [ ] **Step 2: `release_all`에 상태 리셋 가드를 넣는다**

`:1963`의 `self.root_visual = None;` 바로 뒤에 추가:

```rust
        // root visual을 버리는 유일한 지점 — 상태 정합을 위해 함께 리셋한다. 현재 이 경로는
        // teardown 전용이라 도달 후 객체가 죽지만, in-place 재구축 경로가 생기면 여기가
        // 재적용 지점이 된다.
        self.last_root_offset = None;
```

- [ ] **Step 3: `begin_frame`에서 root 오프셋을 적용한다**

`:2469-2471`의

```rust
        let Some(root) = self.root_visual_ptr() else {
            return;
        };
```

바로 뒤, `self.frame_counter += 1;`(`:2472`) 앞에 삽입:

```rust
        // 월 가드밴드 크롭: 렌더 서피스는 render rect(오버랩 확장) 크기인데 창은 visible rect
        // 크기다. root visual을 -inset만큼 밀어 visible 영역이 창 원점에 오게 한다. 전 비주얼이
        // root의 직속 자식이므로(end_frame의 AddVisual) WR 타일과 external 비디오에 균일하게
        // 적용되고, 자식 SetClip은 비주얼-로컬 좌표라 영향을 받지 않는다. 창 밖으로 밀려난
        // 가드밴드는 HWND 경계에서 DWM이 클립하므로 root 클립은 불필요하다.
        //
        // DComp는 top-left 원점이라 y에 `-top`을 쓴다 — GL blit 경로가 `bottom`을 쓰는 것과
        // 규약이 반대다(§`RenderingContext::present_inset`).
        let inset = self.rendering_context.present_inset();
        let offset = (-inset.left as f32, -inset.top as f32);
        if self.last_root_offset != Some(offset) {
            // Safety: root는 살아있는 IDCompositionVisual.
            unsafe {
                let hr = (*root).SetOffsetX_1(offset.0);
                if hr < 0 {
                    warn!("[dcomp-native] root SetOffsetX failed (hr=0x{:08x})", hr as u32);
                }
                let hr = (*root).SetOffsetY_1(offset.1);
                if hr < 0 {
                    warn!("[dcomp-native] root SetOffsetY failed (hr=0x{:08x})", hr as u32);
                }
            }
            // 이 파일은 `use log::warn;`만 import하고 info는 항상 경로 한정으로 쓴다(:184 등).
            log::info!(
                "[dcomp-native] root guard-band offset ({}, {}) applied from present_inset {:?}",
                offset.0, offset.1, inset
            );
            self.last_root_offset = Some(offset);
        }
```

- [ ] **Step 4: 빌드한다**

```powershell
. .\scripts\servo_env.ps1
cd W:\servo_multigpu-tiled-wall
cargo check -p servo-paint
.\mach build -r
```
기대: 성공.

- [ ] **Step 5: 런타임 A — DComp on에서 가로 밀림이 사라졌는지 확인한다**

```powershell
$env:SERVO_MEDIA_D3D11_VIDEO=1
$env:SERVO_MEDIA_GAPLESS_LOOP=1
$env:SERVO_MEDIA_SYNC_GROUP=-1
$env:SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF=1
$env:SERVO_COMPOSITOR_DCOMP=1
.\target\release\servoshell.exe --wall-all-tiles `
  --wall-layout etc\multigpu\config\wall_layout.example_2x1_display.json `
  tests\html\multigpu_wall_ruler_probe.html 2> ..\wall_dcomp_on.err.log
```

판정:
- display 0(왼쪽) 좌상단 = 노란 `0,0` — 창 구석에 정확히 붙어야 한다
- **display 1(오른쪽) 좌상단 = 노란 `1920,0`** — 창 구석에 정확히 붙어야 한다
- 수정 전에는 여기에 `1888` 근방 라벨이 32px 안쪽으로 들어와 보였다

창을 닫고 로그 확인:
```powershell
Select-String -Path ..\wall_dcomp_on.err.log -Pattern "root guard-band offset"
```
기대: 2줄 — tile 0 `(0, 0)`, tile 1 `(-32, 0)`.

- [ ] **Step 6: 런타임 B — DComp off 무회귀를 확인한다**

```powershell
$env:SERVO_COMPOSITOR_DCOMP=0
.\target\release\servoshell.exe --wall-all-tiles `
  --wall-layout etc\multigpu\config\wall_layout.example_2x1_display.json `
  tests\html\multigpu_wall_ruler_probe.html 2> ..\wall_dcomp_off.err.log
```
기대: A와 동일하게 display 1 좌상단이 `1920,0`. `root guard-band offset` 로그는 **없어야** 한다(컴포지터 미발동).

- [ ] **Step 7: 포맷 확인 후 커밋**

```powershell
rustfmt --edition 2024 --check components\paint\dcomp_compositor.rs
git diff --check
git add components/paint/dcomp_compositor.rs
git commit -m "fix(dcomp): 월 가드밴드 크롭을 root visual 오프셋으로 적용

DComp 발동 시 gui.rs가 오프스크린→창 blit을 스킵해 크롭이 통째로 사라지고
타일 콘텐츠가 overlapPx만큼 밀려 표시되던 문제. root visual에
SetOffsetX/Y(-left,-top)를 걸어 draw 추가 없이 해소한다(DWM 합성 시점 변환).
전 비주얼이 root 직속 자식이라 external 비디오에도 균일 적용된다."
```

---

### Task 5: blit 경로 세로 inset 수정

**Files:**
- Modify: `ports/servoshell/desktop/headed_window.rs` — `webview_visible_source_rect`(Task 1에서 교체한 버전)

**Interfaces:**
- Consumes: Task 1의 `wall_tile_render_insets`, Task 3의 세로 layout + 눈금 프로브.

**버그 설명(구현자용):** `render_to_parent_callback_for_source_rect`는 최종적으로 `glBlitFramebuffer`(`components/shared/paint/rendering_context.rs:1876`)를 호출한다. GL 프레임버퍼는 **bottom-left 원점**이므로 source rect의 y 원점은 "프레임버퍼 아래쪽에서부터의 거리"다. visible 영역은 render rect의 **위쪽**에 붙어 있으므로 그 거리는 `top`이 아니라 `bottom`이다.

검산 — 2×2의 tile 0: render rect 1952×1112(우·하 가드밴드), visible 1920×1080이 그 좌상단. GL row 0 = 가상 y 1112이므로 visible은 GL row 32..1112 → src y 원점 = `bottom`(=32). 현재는 `top`(=0)이라 32px 어긋난다.

- [ ] **Step 1: y 원점을 `bottom`으로 고친다**

`ports/servoshell/desktop/headed_window.rs`의 `webview_visible_source_rect`를 교체:

```rust
    pub(crate) fn webview_visible_source_rect(
        &self,
        visible_size: Size2D<i32, DevicePixel>,
    ) -> Rect<i32, DevicePixel> {
        let Some(inset) = self.wall_tile_render_insets() else {
            return Rect::new(Point2D::origin(), visible_size);
        };
        // GL 프레임버퍼는 bottom-left 원점이므로 source rect의 y 원점은 "아래쪽에서부터의
        // 거리" = `bottom`이다(`top`이 아니다 — glBlitFramebuffer, rendering_context.rs).
        // visible 영역은 render rect의 위쪽에 붙어 있다.
        // DComp 경로는 top-left 원점이라 root visual에 `-top`을 쓴다 — 규약이 반대다
        // (§`RenderingContext::present_inset`).
        Rect::new(Point2D::new(inset.left, inset.bottom), visible_size)
    }
```

- [ ] **Step 2: 빌드한다**

```powershell
. .\scripts\servo_env.ps1
cd W:\servo_multigpu-tiled-wall
.\mach build -r
```
기대: 성공.

- [ ] **Step 3: 런타임 C — 세로 크롭을 양쪽 경로에서 확인한다**

DComp off(= blit 경로, 이번 수정의 대상):
```powershell
$env:SERVO_COMPOSITOR_DCOMP=0
.\target\release\servoshell.exe --wall-all-tiles `
  --wall-layout etc\multigpu\config\wall_layout.test_1x2_vertical.json `
  tests\html\multigpu_wall_ruler_probe.html 2> ..\wall_vert_off.err.log
```

DComp on(= Task 4의 root 오프셋 y 성분):
```powershell
$env:SERVO_COMPOSITOR_DCOMP=1
.\target\release\servoshell.exe --wall-all-tiles `
  --wall-layout etc\multigpu\config\wall_layout.test_1x2_vertical.json `
  tests\html\multigpu_wall_ruler_probe.html 2> ..\wall_vert_on.err.log
```

**두 실행 모두** 판정:
- display 0 좌상단 = `0,0`
- **display 1 좌상단 = `0,1080`** (`0,1048`이면 실패 — 32px 어긋남)

DComp on 로그에서 오프셋도 확인:
```powershell
Select-String -Path ..\wall_vert_on.err.log -Pattern "root guard-band offset"
```
기대: tile 0 `(0, 0)`, tile 1 `(0, -32)`.

DComp on에서 y가 반대 방향(`0,1112` 근처)으로 어긋나면 WR native surface의 원점 규약이 전제와 다른 것이므로 Task 4의 `-inset.top`을 `-inset.bottom`으로 바꿔 재검증한다 — 구조 변경은 필요 없다.

- [ ] **Step 4: 가로 회귀가 없는지 재확인한다**

```powershell
$env:SERVO_COMPOSITOR_DCOMP=1
.\target\release\servoshell.exe --wall-all-tiles `
  --wall-layout etc\multigpu\config\wall_layout.example_2x1_display.json `
  tests\html\multigpu_wall_ruler_probe.html
```
기대: display 1 좌상단 `1920,0` 유지(Task 4 결과가 깨지지 않았을 것).

- [ ] **Step 5: 포맷 확인 후 커밋**

```powershell
rustfmt --edition 2024 --check ports\servoshell\desktop\headed_window.rs
git diff --check
git add ports/servoshell/desktop/headed_window.rs
git commit -m "fix(wall): blit source rect의 세로 가드밴드 원점을 bottom으로 수정

GL 프레임버퍼는 bottom-left 원점이라 source rect y 원점은 top이 아니라 bottom
이어야 한다. 상하 오버랩이 0인 2x1/3x1에서는 드러나지 않았고 2x2/세로 구성에서
타일마다 overlapPx만큼 어긋났다. 신규 1x2 세로 layout으로 양쪽 경로 검증."
```

---

### Task 6: 회귀 검증 + 문서 갱신

**Files:**
- Modify: `CLAUDE.md` (`cargo test -p servoshell wall_layout --lib` 설명의 테스트 개수)

**Interfaces:**
- Consumes: Task 1~5 전부.

- [ ] **Step 1: 정적 검사를 전부 돌린다**

```powershell
. .\scripts\servo_env.ps1
cd W:\servo_multigpu-tiled-wall
cargo test -p servoshell wall_layout --lib
cargo check -p servoshell
cargo check -p servo-paint-api
git diff --check
```
기대: 테스트 11 passed, check 에러 0, `git diff --check` 출력 없음.

- [ ] **Step 2: 월 동기화 회귀를 확인한다**

```powershell
$env:SERVO_COMPOSITOR_DCOMP=1
.\target\release\servoshell.exe --wall-all-tiles `
  --wall-layout etc\multigpu\config\wall_layout.example_2x1_display.json `
  tests\html\multigpu_wall_sync_probe.html 2> ..\wall_regress_sync.err.log
```
6~8초 두고 창을 닫은 뒤:

```powershell
Select-String -Path ..\wall_regress_sync.err.log -Pattern "scroll_offsets=matched" | Measure-Object
Select-String -Path ..\wall_regress_sync.err.log -Pattern "panicked|missed frame|pending frame"
```
기대: `scroll_offsets=matched` 다수, 패닉/미스프레임 매치 0줄.

- [ ] **Step 3: 비디오 경로 회귀를 확인한다 (external 비주얼이 root 오프셋을 함께 받는지)**

```powershell
$env:SERVO_COMPOSITOR_DCOMP=1
.\target\release\servoshell.exe --wall-all-tiles `
  --wall-layout etc\multigpu\config\wall_layout.example_2x1_display.json `
  tests\html\multigpu_wall_video_file_probe.html 2> ..\wall_regress_video.err.log
```
기대: 비디오가 두 타일에 걸쳐 **이음매에서 어긋나지 않고** 재생된다. 비디오만 32px 밀리면 external 비주얼이 root 자식이 아니라는 뜻이므로 `end_frame`의 `AddVisual` 경로를 다시 확인한다.

```powershell
Select-String -Path ..\wall_regress_video.err.log -Pattern "panicked|SetOffsetX failed|SetOffsetY failed"
```
기대: 0줄.

- [ ] **Step 4: 스트레스 케이스로 이음매 아티팩트를 확인한다**

```powershell
$env:SERVO_COMPOSITOR_DCOMP=1
.\target\release\servoshell.exe --wall-all-tiles `
  --wall-layout etc\multigpu\config\wall_layout.example_2x1_display.json `
  tests\html\multigpu_wall_stress_cases.html
```
기대: 이음매를 지나는 blur/shadow/transform 요소가 잘리거나 밴딩 없이 이어진다. 가드밴드가 제 역할을 하고 있는지 보는 단계다(크롭이 맞아야 비로소 검증 가능해진 항목).

- [ ] **Step 5: `CLAUDE.md`의 테스트 개수를 갱신한다**

`CLAUDE.md`의 "Test, format, validate" 섹션에서:

```
cargo test -p servoshell wall_layout --lib      # the wall layout unit tests (currently 3)
```
을 다음으로 교체:

```
cargo test -p servoshell wall_layout --lib      # the wall layout unit tests (currently 11)
```

- [ ] **Step 6: 커밋**

```powershell
git add CLAUDE.md
git commit -m "docs: CLAUDE.md의 wall_layout 단위 테스트 개수 갱신 (3 -> 11)"
```

---

## 완료 기준

1. `cargo test -p servoshell wall_layout --lib` — 11 passed
2. 2×1 + DComp **on/off 양쪽**에서 display 1 좌상단 라벨이 `1920,0`
3. 1×2 세로 + DComp **on/off 양쪽**에서 display 1 좌상단 라벨이 `0,1080`
4. 비디오/동기화/스트레스 회귀에서 패닉·미스프레임 0, `scroll_offsets=matched` 유지
5. `rustfmt --check` / `git diff --check` 클린

## 알려진 별개 이슈 (이 계획의 범위 밖)

`tools/wall_perf_analyzer/analyze_wall_perf.py:34`의 `Wall window present:` 정규식이 `monitor=(?P<monitor>\d+)`를 기대하는데 현재 코드는 `display=`를 출력한다(display-only 마이그레이션 때 어긋난 것). 이 계획은 해당 로그 줄을 건드리지 않으므로 상태가 악화되지도, 개선되지도 않는다. 별도로 고칠 항목.

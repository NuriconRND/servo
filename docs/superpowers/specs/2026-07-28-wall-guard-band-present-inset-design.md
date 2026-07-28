# 월 가드밴드 크롭을 present_inset으로 통합 — DComp 경로 누락 + 세로축 수정 설계 (2026-07-28)

## 목표

월 타일의 **가드밴드(`overlapPx`) 크롭**을 렌더링 컨텍스트의 단일 속성(`present_inset`)으로
정본화하고, 그 값을 **DComp 네이티브 컴포지터 경로에도** 적용한다. 동시에 blit 경로의
**세로 inset 규약 오류**(GL bottom-left 원점인데 top inset을 씀)를 함께 고친다.

대상 워크트리: `servo_multigpu-tiled-wall`, 브랜치 `nonstandard-media-display-port`.

## 배경 — 실측으로 확정한 현재 상태

### 증상

```powershell
$env:SERVO_COMPOSITOR_DCOMP=1
.\target\release\servoshell.exe --wall-all-tiles `
  --wall-layout etc\multigpu\config\wall_layout.example_2x1_display.json `
  "file:///W:/servo_multigpu-tiled-wall/tests/html/multigpu_wall_iframe_probe.html"
```

`(1920,0)` 창(tile 1)의 콘텐츠가 **오른쪽으로 32px 밀리고**, 창 왼쪽 32px에는 display 0의
오른쪽 끝이 중복 표시된다. `(0,0)` 창(tile 0)은 정상.
**`SERVO_COMPOSITOR_DCOMP=0`으로 돌리면 밀림이 사라짐(사용자 실측 확인).**

### 원인

가드밴드 크롭이 **오프스크린→창 백버퍼 blit 안에만** 존재한다.

- `ports/servoshell/desktop/gui.rs:638` — `webview_visible_source_rect()`로 source rect 계산
- `components/shared/paint/rendering_context.rs:1833` `render_to_parent_callback_for_source_rect`
  → `:1852` `blit_framebuffer`가 src x=`left`부터 읽어 크롭

DComp 게이트가 켜지면 이 경로가 통째로 사라진다.

- `gui.rs:646` — `dcomp_native_active()`이면 웹뷰 blit을 **스킵**(§3-t에서 "②단 소멸"로 의도적 제거)
- `rendering_context.rs:1943` — `OffscreenRenderingContext::present()`는 no-op. 오프스크린 FB는
  표시에 전혀 기여하지 않음
- 실제 표시는 WR → `components/paint/dcomp_compositor.rs` → DWM. 비주얼 오프셋은
  `apply_visual_placement`(`:541-548`)가 **WR 프레임버퍼 좌표 = 창 device 좌표**로 두고 계산 →
  프레임버퍼 x=0이 창 x=0에 놓인다

프레임버퍼 x=0은 tile 1에서 **가상좌표 1888**이다(`webview_paint_origin` = render rect 원점,
`headed_window.rs:1292`; render rect는 `wall_layout.rs:106` `tile_render_rect`). 따라서 창은
가상 1888..3808을 표시한다 → 오른쪽 32px 밀림.

tile 0이 멀쩡한 것은 **좌·상 inset이 0**이기 때문이다(가드밴드가 우측에만 있어 크롭이 없어도
창 밖으로 자연히 벗어남). 즉 2×1에서는 tile 0이 우연히 정상이고 tile 1만 깨진다.

### 이미 올바른 것 (변경 대상 아님)

월의 "창 = display 크기, 서피스 = render rect, 표출은 창만큼" 구조 자체는 이미 설계대로다.

| 단계 | 코드 | tile 1 값 |
|---|---|---|
| 창 크기 = visible rect | `desktop/app.rs:224` (`tile.rect.size`) | 1920×1080 |
| 오프스크린 서피스 = render rect | `gui.rs:611` → `headed_window.rs:860` | 1952×1080 |
| WR 씬 원점 = render rect 원점 | `headed_window.rs:1292` | x = 1888 |
| 입력 좌표 원점 = visible rect 원점 | `headed_window.rs:838` | x = 1920 |

창이 display를 넘지 않으므로 **멀티GPU 크로스어댑터 트래픽은 발생하지 않는다**. 가드밴드는
같은 GPU의 같은 서피스 안에만 존재한다. (창을 `overlapPx`만큼 겹쳐 배치하는 대안은 borderless
풀스크린 자격 상실 + 창이 두 어댑터 출력에 걸침 → 비목표. 아래 §비목표 참조.)

### 세로축 잠재 버그 (별개, 함께 수정)

`headed_window.rs:873 webview_visible_source_rect`가 **top** inset을 GL blit의 src y 원점으로
쓴다. GL 프레임버퍼는 bottom-left 원점이므로 들어가야 할 값은 **bottom** inset이다.

검산 — 2×2의 tile 0: render rect 1952×1112(우·하 가드), visible 1920×1080이 그 좌상단.
GL row 0 = 가상 y 1112이므로 visible은 GL row 32..1112 → src y 원점 = bottom inset 32.
현재 코드는 top(=0)을 써서 32px 어긋난다.

2×1/3×1처럼 상하 inset이 0인 구성에서는 드러나지 않고, **v1 목표인 2×2 4K 월에서 터진다.**

## 사용자 결정 사항

- **정본 위치**: 페인트 타깃 메타데이터(`add_paint_target`/`update_paint_target`/
  `viewport_origin_override` 3지점 시그니처 변경)가 아니라 **`RenderingContext`의 속성**.
  근거: (1) inset은 "이 서피스 중 표출되는 sub-rect"라는 서피스/창의 성질이고 그 단위가 정확히
  타일 하나, (2) DComp 컴포지터가 **이미 `rendering_context`를 보유**(`dcomp_compositor.rs:2455`
  에서 사용)해 painter 배선이 불필요, (3) libservo 공개 API 시그니처 변경 0, (4) 값이 하나이므로
  blit 경로와 DComp 경로가 서로 어긋날 수 없다.
- **세로축 수정을 같은 변경에 포함**한다(경로별 y 규약이 다르다는 사실이 이 설계의 핵심 중 하나라
  분리하면 오히려 위험).

## 설계

### 1. `RenderingContext::present_inset` — 정본

`components/shared/paint/rendering_context.rs`, 트레잇에 추가:

```rust
/// 이 컨텍스트가 렌더한 서피스 중 실제로 화면에 표출되는 sub-rect를 정의하는 가드밴드 여백.
/// device px, **top-left 기준**. 월 타일에서 `overlapPx`로 확장한 render rect와 visible rect의
/// 차이이며, 비월 모드는 zero.
///
/// 소비자는 두 곳이고 y 규약이 서로 다르다:
/// - 오프스크린→창 blit(`gui.rs`): GL은 bottom-left 원점이라 src y 원점에 `bottom`을 쓴다.
/// - DComp 네이티브 컴포지터: top-left 원점이라 root visual 오프셋에 `-top`을 쓴다.
fn present_inset(&self) -> DeviceIntSideOffsets { DeviceIntSideOffsets::zero() }
fn set_present_inset(&self, _inset: DeviceIntSideOffsets) {}
```

`DeviceIntSideOffsets`는 `webrender_api::units`에서 import(`:41`의 `DeviceIntRect`와 같은 경로).

- `WindowRenderingContext`: `present_inset: Cell<DeviceIntSideOffsets>` 필드 + getter/setter 구현
- `OffscreenRenderingContext`: 부모로 위임. `dcomp_native_active`/`set_dcomp_native_active`
  등 기존 위임 8지점(`:2008-2040`)과 동일 패턴 — servoshell이 Window를 Offscreen으로 감싸므로
  위임이 없으면 트레잇 기본값(zero)에 흡수된다
- 그 외 `RenderingContext` 구현체는 트레잇 기본값(zero) 그대로 → 비월/비Windows 경로 무영향

### 2. inset 계산을 `wall_layout`으로 승격

현재 `headed_window.rs:847 wall_tile_render_insets`는 창 인스턴스 메서드라 단위 테스트가
불가능하다. 순수 계산이므로 layout으로 옮긴다.

```rust
// ports/servoshell/wall_layout.rs
pub(crate) fn tile_render_insets(
    &self,
    tile_index: usize,
    hidpi_scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,
) -> Option<DeviceIntSideOffsets>
```

산식은 현행과 동일(`tile_device_rect` − `tile_render_device_rect`), 음수는 0으로 클램프:

```
left   = visible.min.x - render.min.x
top    = visible.min.y - render.min.y
right  = render.max.x  - visible.max.x
bottom = render.max.y  - visible.max.y
```

`headed_window.rs`의 `wall_tile_render_insets`는 이 메서드로 위임하는 한 줄이 된다.
`webview_rendering_size`(`:860`)는 `left+right`/`top+bottom`을 그대로 쓰므로 무변경.

### 3. servoshell — 창 생성 시 1회 세팅

`HeadedWindow::new`, 오프스크린 컨텍스트 생성 직후(`headed_window.rs:438` 부근, 기존 월 진단
`info!` 블록 `:439-466`과 같은 자리):

```rust
if let Some(inset) = layout.tile_render_insets(servoshell_preferences.wall_tile_index, hidpi_factor) {
    rendering_context.set_present_inset(inset);   // Offscreen → 부모 Window로 위임
}
```

- inset은 layout + hidpi에서 유도되며 타일당 고정이므로 **생성 시 1회로 충분**하다
  (월 타일 창은 non-resizable — `headed_window.rs:148`)
- 기존 월 진단 로그에 `inset` 필드를 추가해 실행 즉시 값 확인이 가능하게 한다
- **단일 타일 프리뷰 모드(`--wall-tile-index N`, `--wall-all-tiles` 없음)에서도 동일하게 적용된다.**
  이 모드는 임의 타일 N이 *primary* painter를 갖기 때문에 "primary면 inset 0"이라는 가정은
  성립하지 않는다 — 컨텍스트 속성으로 두면 이 구분 자체가 사라진다

### 4. DComp 경로 — root visual 오프셋

`dcomp_compositor.rs`. `begin_frame`(`:2464`)의 `RemoveAllVisuals()` **직전**:

```rust
let inset = self.rendering_context.present_inset();
let offset = (-inset.left as f32, -inset.top as f32);
if offset != self.last_root_offset {
    // Safety: root는 살아있는 IDCompositionVisual.
    unsafe {
        (*root).SetOffsetX_1(offset.0);
        (*root).SetOffsetY_1(offset.1);
    }
    self.last_root_offset = offset;
}
```

근거:

- **전 비주얼이 root의 직속 자식**이다. `end_frame`(`:3117-3133`)이 `frame_surfaces`를 순회하며
  `root.AddVisual(...)`하고, WR 타일(`:2579`)과 external 비디오 서피스(`:1499`) 모두 같은
  `frame_surfaces`에 들어간다 → root 한 곳의 오프셋이 **비디오 포함 균일 적용**
- `RemoveAllVisuals`는 자식만 제거하고 root 자신의 오프셋은 유지 → 매 프레임 재설정 불필요.
  변화 시에만 COM 호출(정상 운영 중에는 0회)
- 자식 `SetClip`은 **비주얼-로컬(오프셋 적용 전) 좌표**이므로(`:538` 주석, MS docs) root 오프셋의
  영향을 받지 않는다 → `apply_visual_placement` 산식 **무변경**
- 창 밖으로 밀려난 가드밴드는 HWND 경계에서 DWM이 클립 → root 클립 불필요

상태 필드:

```rust
/// 마지막으로 root visual에 적용한 오프셋. `None` = 미적용(생성 직후/재구축 직후).
last_root_offset: Option<(f32, f32)>,
```

`root_visual`이 재생성되는 경로(리사이즈 디바운스 재구축 — `painter.rs:2323-2339` →
`dcomp_compositor.rs:1963`에서 `root_visual = None`)에서 **`last_root_offset`도 `None`으로
리셋**해 다음 `begin_frame`에 재적용되게 한다. 이 리셋 누락이 이 설계의 유일한 상태 함정이므로
해당 지점에 주석을 남긴다.

### 5. blit 경로 — 세로 inset 수정

```rust
// headed_window.rs:873 webview_visible_source_rect
// GL 프레임버퍼는 bottom-left 원점이므로 src y 원점은 top이 아니라 **bottom** inset이다.
// (DComp는 top-left 원점이라 root visual에는 -top을 쓴다 — 두 경로의 y 규약이 다르다.)
Rect::new(Point2D::new(left.max(0), bottom.max(0)), visible_size)
```

x는 무변경. 따라서 **DComp off + 상하 오버랩 0**(현행 2×1/3×1 구성)에서는 동작이 완전히 동일하다.

## 테스트

### 단위 — `cargo test -p servoshell wall_layout --lib`

`wall_layout.rs`의 기존 6개 테스트에 추가:

- `tile_render_insets_2x2`: virtual 3840×2160, 4타일, `overlapPx: 32`
  - tile 0 → `(top 0, right 32, bottom 32, left 0)`
  - tile 1 → `(0, 0, 32, 32)`
  - tile 2 → `(32, 32, 0, 0)`
  - tile 3 → `(32, 0, 0, 32)`
- `tile_render_insets_1x2_vertical`: virtual 1920×2160, 세로 2타일 → tile 0 bottom=32,
  tile 1 top=32, 좌우 모두 0
- `tile_render_insets_clamped_when_overlap_exceeds_viewport`: `overlapPx`가 가상 뷰포트를 넘는
  경우 클램프 확인(`tile_render_rect`의 기존 클램프와 정합)
- `tile_render_insets_zero_without_overlap`: `overlapPx` 미지정 → 전부 0

### 런타임

| # | 구성 | 기대 |
|---|---|---|
| A | 현행 2×1 + `SERVO_COMPOSITOR_DCOMP=1` | 32px 밀림 소멸, tile 0 무회귀 |
| B | 현행 2×1 + `SERVO_COMPOSITOR_DCOMP=0` | 무회귀(x 경로 무변경) |
| C | **신규** `wall_layout.test_1x2_vertical.json` + DComp `1`/`0` 양쪽 | display 1이 정확히 가상 y=1080에서 시작 |
| D | 45타일 비디오 그리드 회귀(DComp on) | lockstep, barrier 완료, `scroll_offsets=matched`, panic 0 |

**C가 이 변경의 핵심 검증이다.** 세로 규약(DComp `-top` vs GL `bottom`)을 양쪽 경로에서 직접
검증하는 유일한 테스트이고, 2×2 4K 실기가 없어도 세로축을 커버한다. 신규 config:

```json
{
  "virtualViewport": { "width": 1920, "height": 2160 },
  "tiles": [
    { "display": 0, "rect": [0, 0, 1920, 1080] },
    { "display": 1, "rect": [0, 1080, 1920, 1080] }
  ],
  "overlapPx": 32
}
```

두 창은 물리적으로 좌우에 놓이므로 이음매가 연속으로 **보이지는** 않지만, 크롭 정확성은 완전히
검증된다 — display 1의 최상단 행이 가상 y=1080이어야 하고, 1048이면 실패다.

판정을 눈대중이 아니라 숫자로 하기 위해 **눈금 프로브 `tests/html/multigpu_wall_ruler_probe.html`을
신규 추가**한다. 요구사항은 최소한이다: 가상 뷰포트 전체 높이·너비에 걸쳐 8px마다 눈금선을 긋고
64px마다 절대 가상좌표 숫자를 렌더한다(가로/세로 양쪽). 각 타일 창의 최상단·최좌단에 읽히는
숫자가 그 타일 `rect`의 원점과 정확히 일치하면 통과. 32px 어긋남은 눈금 4칸이라 육안으로도
즉시 판별되고, 스크린샷으로 증거를 남길 수 있다. 이 페이지는 A~C 모든 런타임 케이스에 공통으로
쓴다(가로 밀림 = 최좌단 숫자, 세로 밀림 = 최상단 숫자).

### 정적 검사 (프로젝트 관례)

```powershell
cd servo_multigpu-tiled-wall
cargo test -p servoshell wall_layout --lib
cargo check -p servoshell
mach build            # 미디어 피처 포함 — cargo build -p servoshell 아님
rustfmt --edition 2024 --check <touched .rs>
git diff --check
```

## 리스크

- **DComp y 부호**: WR native surface의 top-left 규약을 전제한다. 런타임 C가 직접 검증하며,
  틀리면 부호만 뒤집으면 된다(구조 변경 없음).
- **root 오프셋이 external 비디오 비주얼에도 적용**되는 것은 의도다(`:1499`/`:3117-3133`로 확인).
  비디오만 어긋나는 경우는 구조상 생길 수 없다.
- **재구축 후 재적용 누락**: §4의 `last_root_offset` 리셋이 유일한 상태 함정. 리사이즈 디바운스
  경로에 주석 + 리셋을 함께 넣는다.
- **DComp off 무회귀**: x 경로 무변경, y만 수정. 상하 오버랩이 0인 현행 구성에서는 바이트 동일한
  동작이어야 한다(런타임 B로 확인).

## 비목표

- **창을 `overlapPx`만큼 겹쳐 배치**(검토 후 기각): borderless 풀스크린 → flip-model present
  자격 상실(`headed_window.rs:278-292`는 창 크기 == display 크기일 때만 승격), 멀티GPU에서 창이
  두 어댑터 출력에 걸쳐 DWM 크로스어댑터 복사 유발. 현행 "창=display, 서피스=render rect" 구조가
  이미 올바른 해법이다.
- `overlapPx` 자동 산정(콘텐츠의 blur/shadow 반경에서 유도).
- 2×2 4K 실기 검증 — 하드웨어 부재. 기존 Phase 4~8 이월 사항 그대로.
- 파셜 프레젠트/damage rect와의 상호작용 최적화. root 오프셋은 합성 시점 변환이라 WR의
  프레임버퍼 좌표계 damage에 영향을 주지 않으므로 이번 범위에서 다룰 것이 없다.

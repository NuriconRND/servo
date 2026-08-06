# winit_wall 표출 전용 최소 셸 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 경량 임베더 `winit_wall`을 `nonstandard-media-display-port` 브랜치로 옮기고, 표출에 필요한 임베더 기능 4건(가드밴드, DPI 재주입, borderless fullscreen, vsync opt-in)을 채워 표출 전용 최소 셸로 만든다.

**Architecture:** 월 레이아웃 파서를 `servo-paint-api`로 승격해 servoshell과 winit_wall이 **같은 코드**를 쓰게 한다(현재는 사본 2개). winit_wall은 `video-perf-investigation`에서 파일을 가져오되 `Dx11RenderingContext` 경로를 제거해 surfman/ANGLE 단일 경로로 고정한다 — DComp 네이티브 컴포지터가 ANGLE 빌드에서만 성립하기 때문이다. servoshell은 폐기하지 않고 UI/UX 전용으로 병행 유지한다.

**Tech Stack:** Rust 2024, Cargo workspace, winit, surfman/ANGLE, WebRender, euclid, serde_json

## Global Constraints

- 스펙: `docs/superpowers/specs/2026-08-06-winit-wall-presentation-shell-design.md`
- 브랜치: `nonstandard-media-display-port` (정본). `video-perf-investigation`은 읽기 전용 소스로만 참조
- **빌드는 반드시 짧은 경로에서** — `subst W: F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser` 후 `W:\servo_multigpu-tiled-wall`에서. (mozangle `build.rs:155`가 Os error 206으로 실패)
- 빌드 전 `. ..\scripts\servo_env.ps1` 소싱 (dot-source)
- 예제 빌드 명령: `cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl` — `-p servo`와 `no-wgl` 둘 다 필수
- 런타임 검증은 **release 빌드로만** — debug는 월 + 동적 video src에서 `MakeCurrentFailed` 크래시
- 실행 전 ANGLE/gstreamer DLL을 exe 옆에 복사 (아니면 egl 패닉)
- 로그 캡처는 `Start-Process -RedirectStandardError` + `CloseMainWindow()` — PowerShell `2>`는 0바이트가 되는 사례가 있음
- **GUI 육안 판정은 사용자 몫.** 서브에이전트에 위임하지 않는다. 각 태스크의 육안 항목은 사용자에게 요청하고 응답을 기다린다
- 커밋 메시지에 큰따옴표를 넣지 않는다 (PowerShell here-string에서 pathspec 에러 유발 이력)
- `ports/servoshell/desktop/dcomp_compositor.rs` 등 기존 rustfmt drift 파일이 있으므로, rustfmt 판정은 **신규 줄에 drift 0**인지로 한다 (파일 전체 clean은 성립하지 않음)

---

### Task 1: `wall_layout`을 `servo-paint-api`로 승격

servoshell 전용 `pub(crate)` 모듈이라 winit_wall이 임포트할 수 없다(winit_wall의 인라인 사본 주석이 이 사실을 명시하고 있다). 두 임베더가 공유하도록 옮긴다. 단위 테스트 13개가 함께 이동해 회귀 가드가 된다.

**Files:**
- Create: `components/shared/paint/wall_layout.rs` (`ports/servoshell/wall_layout.rs` 661줄을 이동)
- Delete: `ports/servoshell/wall_layout.rs`
- Modify: `components/shared/paint/lib.rs:25-28` (모듈 선언 추가)
- Modify: `components/shared/paint/Cargo.toml` (`serde_json` 추가)
- Modify: `components/servo/lib.rs:48-51` (재수출)
- Modify: `ports/servoshell/lib.rs:25`, `ports/servoshell/prefs.rs:28`, `ports/servoshell/desktop/headed_window.rs:61`, `ports/servoshell/desktop/headless_window.rs:24`

**Interfaces:**
- Produces: `servo::wall_layout::{WallLayout, WallTile, WallLayoutError}`. 주요 메서드 —
  - `WallLayout::from_path(path: &Path) -> Result<Self, WallLayoutError>`
  - `WallLayout::validate_tile_index(&self, tile_index: usize) -> Result<(), WallLayoutError>`
  - `WallLayout::virtual_viewport_css_size(&self) -> Size2D<f32, CSSPixel>`
  - `WallLayout::tile_origin_device_vector(&self, tile_index: usize, hidpi: Scale<f32, DeviceIndependentPixel, DevicePixel>) -> Option<Vector2D<f32, DevicePixel>>`
  - `WallLayout::tile_device_rect(&self, tile_index: usize, hidpi: …) -> Option<DeviceIntRect>`
  - `WallLayout::tile_render_device_rect(&self, tile_index: usize, hidpi: …) -> Option<DeviceIntRect>`
  - `WallLayout::tile_render_insets(&self, tile_index: usize, hidpi: …) -> Option<SideOffsets2D<i32, DevicePixel>>`
  - 필드: `virtual_viewport`, `tiles: Vec<WallTile>`, `overlap_px: u32`; `WallTile { display: usize, rect, gpu_override }`

- [ ] **Step 1: 파일을 이동한다 (히스토리 보존)**

```bash
git mv ports/servoshell/wall_layout.rs components/shared/paint/wall_layout.rs
```

- [ ] **Step 2: 임포트를 원산지 크레이트로 바꾼다**

`components/shared/paint/wall_layout.rs`의 9-10행:

```rust
use euclid::{Point2D, Rect, Scale, SideOffsets2D, Size2D, Vector2D};
use serde_json::Value;
use servo::{CSSPixel, DeviceIndependentPixel, DeviceIntRect, DevicePixel};
```

를 다음으로 교체한다. `paint_api`는 `servo`에 의존할 수 없다(순환):

```rust
use euclid::{Point2D, Rect, Scale, SideOffsets2D, Size2D, Vector2D};
use serde_json::Value;
use servo_geometry::DeviceIndependentPixel;
use style_traits::CSSPixel;
use webrender_api::units::{DeviceIntRect, DevicePixel};
```

원산지 근거: `DeviceIndependentPixel`은 `servo_geometry`(`components/servo/lib.rs:65-67`), `CSSPixel`은 `style_traits`(같은 파일 71행), `DeviceIntRect`/`DevicePixel`은 `webrender_api::units`(72-74행). 셋 다 `paint_api`의 기존 의존성이다(`stylo_traits` 패키지의 lib 이름이 `style_traits`).

- [ ] **Step 3: 가시성을 `pub`으로 올린다**

같은 파일에서 `pub(crate)`를 전부 `pub`으로 바꾼다. 대상은 `struct WallLayout`(14행), `struct WallTile`(21행), `enum WallLayoutError`(34행), 그리고 41·46·59·69·76·90·97·106·131·146행의 메서드다.

```bash
# W:\servo_multigpu-tiled-wall 에서
sed -i 's/pub(crate) /pub /g' components/shared/paint/wall_layout.rs
```

- [ ] **Step 4: 모듈을 선언하고 `serde_json` 의존성을 추가한다**

`components/shared/paint/lib.rs`의 모듈 선언 블록(25-28행)에 알파벳 순으로 끼워 넣는다:

```rust
pub mod display_list;
pub mod largest_contentful_paint_candidate;
pub mod rendering_context;
pub mod viewport_description;
pub mod wall_layout;
```

`components/shared/paint/Cargo.toml`의 `[dependencies]`에서 `serde_bytes` 다음 줄에 추가한다(워크스페이스에 `serde_json = "1.0"`이 이미 있다, 루트 `Cargo.toml:224`):

```toml
serde_json = { workspace = true }
```

- [ ] **Step 5: 테스트가 새 위치에서 통과하는지 확인한다**

```bash
cargo test -p servo-paint-api wall_layout --lib
```

기대: 13 passed. (스펙 §테스트 — 3x1 중간 타일 양쪽 가드밴드와 분수 DPI 케이스 포함)

- [ ] **Step 6: `servo`에서 재수출한다**

`components/servo/lib.rs:48-51`의 재수출 블록 바로 아래에 한 줄 추가한다:

```rust
pub use paint_api::rendering_context::{
    DisplayTopology, OffscreenRenderingContext, RenderingContext, SoftwareRenderingContext,
    WindowRenderingContext, dxgi_luid_for_gpu_index, enumerate_display_topology, spatial_order,
};
pub use paint_api::wall_layout;
```

모듈 통째로 재수출한다 — winit_wall과 servoshell이 `servo::wall_layout::WallLayout`으로 접근한다.

- [ ] **Step 7: servoshell의 임포트를 갱신한다**

`ports/servoshell/lib.rs:25`의 `mod wall_layout;` 줄을 삭제한다. 그리고 세 임포트 사이트를 바꾼다:

- `ports/servoshell/prefs.rs:28`: `use crate::wall_layout::{WallLayout, WallLayoutError};` → `use servo::wall_layout::{WallLayout, WallLayoutError};`
- `ports/servoshell/desktop/headed_window.rs:61`: `use crate::wall_layout::WallLayout;` → `use servo::wall_layout::WallLayout;`
- `ports/servoshell/desktop/headless_window.rs:24`: `use crate::wall_layout::WallLayout;` → `use servo::wall_layout::WallLayout;`

- [ ] **Step 8: servoshell이 빌드되는지 확인한다**

```bash
cargo check -p servoshell
```

기대: 에러 0. `app.rs`/`window.rs`도 `wall_layout`을 참조하지만 타입 이름만 쓰므로 위 세 곳 수정으로 해결된다 — 만약 경로 에러가 더 나오면 같은 방식으로 `servo::wall_layout::`로 바꾼다.

- [ ] **Step 9: 정적 검사**

```bash
rustfmt --edition 2024 --check components/shared/paint/wall_layout.rs components/shared/paint/lib.rs components/servo/lib.rs ports/servoshell/lib.rs ports/servoshell/prefs.rs ports/servoshell/desktop/headed_window.rs ports/servoshell/desktop/headless_window.rs
git diff --check
```

기대: 둘 다 출력 없음(신규 drift 0).

- [ ] **Step 10: 커밋**

```bash
git add -A components/shared/paint components/servo/lib.rs ports/servoshell
git commit -F - <<'EOF'
refactor(paint): wall_layout을 servo-paint-api로 승격 — 임베더 간 공유

winit_wall(components/servo 예제)은 servoshell의 pub(crate) 모듈을 임포트할 수
없어 인라인 사본을 따로 갖고 있었다. 두 임베더가 같은 코드를 쓰도록 공유
크레이트로 올린다. present_inset의 소비처와 같은 크레이트가 되어 가드밴드
값의 정본이 한 곳에 모인다.

단위 테스트 13개도 함께 이동해 회귀 가드가 된다.
EOF
```

---

### Task 2: winit_wall 이식 + d3d11 백엔드 제거 + 디렉터리형 분할

**Files:**
- Create: `components/servo/examples/winit_wall/main.rs` (`video-perf-investigation`의 906줄에서 파생)
- Create: `components/servo/examples/winit_wall/tile.rs`

**Interfaces:**
- Consumes: Task 1의 `servo::wall_layout::WallLayout`
- Produces: `pub(crate) struct TileWindow { window: Window, rendering_context: Rc<dyn RenderingContext>, paint_target: Cell<Option<WebViewPaintTarget>> }` (원본 198-202행의 기존 타입을 `tile.rs`로 이동). `AppState`(204행)는 `main.rs`에 남는다 — 앱 상태이지 타일이 아니다. 후속 태스크가 `TileWindow` 생성 경로에 inset 주입·fullscreen·vsync를 붙인다

- [ ] **Step 1: 원본을 가져온다**

```bash
mkdir -p components/servo/examples/winit_wall
git show video-perf-investigation:components/servo/examples/winit_wall.rs > components/servo/examples/winit_wall/main.rs
```

- [ ] **Step 2: 인라인 `mod wall`을 삭제하고 공유 모듈을 쓴다**

`main.rs`의 667행부터 파일 끝(906행)까지가 인라인 `mod wall { ... }`이다. 통째로 삭제한다. 그리고 45행의

```rust
use crate::wall::WallLayout;
```

을 다음으로 바꾼다:

```rust
use servo::wall_layout::WallLayout;
```

- [ ] **Step 3: d3d11 백엔드를 제거한다**

네 곳을 지운다.

(a) 29행의 임포트:
```rust
use servo::Dx11RenderingContext;
```
→ 삭제.

(b) 49-58행의 `Backend` 열거 전체 → 삭제.

(c) `struct Config`(59행)의 `backend: Backend,` 필드(64행)와 `parse_args`의 `let mut backend = Backend::Gl;`(77행), `"--backend" => { ... }` arm(97-104행), 그리고 `Config` 생성 시 `backend,` 필드 → 전부 삭제.

(d) 460-497행의 렌더링 컨텍스트 생성 `match config.backend { ... }`를 단일 경로로 평탄화한다:

```rust
            let rendering_context: Rc<dyn RenderingContext> = Rc::new(
                WindowRenderingContext::new_with_target_gpu(
                    display_handle,
                    window_handle,
                    window.inner_size(),
                    gpu_index,
                )
                .expect("Could not create RenderingContext for tile window"),
            );
```

`RawWindowHandle` 임포트(41행)가 d3d11 분기에서만 쓰였다면 함께 지운다 — `cargo check`의 unused import 경고로 확인한다.

- [ ] **Step 4: 사용법 주석을 갱신한다**

파일 상단 doc 주석(14행 부근)의 사용법에서 `--backend` 설명을 지우고, 남은 플래그만 남긴다:

```rust
//!   winit_wall --wall-layout <layout.json> [--wall-all-tiles] [--wall-tile-index N]
//!              [--capture <path.png>] [URL]
```

- [ ] **Step 5: 타일 수명을 `tile.rs`로 분리한다**

원본 198-202행의 `struct TileWindow`와, 타일 창·렌더링 컨텍스트를 만드는 로직(386행의 `WindowAttributes` 구성부터 528행 `let tiles: Vec<TileWindow> = built` 직전까지)을 `components/servo/examples/winit_wall/tile.rs`로 옮긴다. **`AppState`(204행)와 `render_all_tiles`(281행), 이벤트 루프는 `main.rs`에 남긴다** — 경계는 "타일 하나의 생성·수명"과 "앱 전체 상태·루프"다. `main.rs` 상단에 선언을 추가한다:

```rust
mod tile;

use tile::Tile;
```

`tile.rs`가 쓰는 항목은 `pub(crate)`로 노출한다. 경계 기준: **창/컨텍스트/paint target의 생성과 파괴는 `tile.rs`, 인자 파싱과 이벤트 루프는 `main.rs`.**

- [ ] **Step 6: 빌드되는지 확인한다**

```bash
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl
```

기대: exit 0, 에러 0. (경고는 다음 스텝에서 정리)

- [ ] **Step 7: 경고를 정리하고 정적 검사**

```bash
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl 2>&1 | grep -E "^warning" | head -20
rustfmt --edition 2024 --check components/servo/examples/winit_wall/main.rs components/servo/examples/winit_wall/tile.rs
git diff --check
```

기대: unused import 경고 0, rustfmt 출력 없음.

- [ ] **Step 8: 커밋**

```bash
git add components/servo/examples/winit_wall
git commit -F - <<'EOF'
feat(winit_wall): 표출 셸 기반 이식 — 공유 wall_layout 사용, d3d11 백엔드 제거

video-perf-investigation의 winit_wall을 이 브랜치로 가져온다. 인라인 wall
레이아웃 사본을 지우고 Task 1에서 승격한 servo::wall_layout을 쓴다.

Dx11RenderingContext 경로는 제거하고 surfman/ANGLE 단일 경로로 고정한다.
DComp 네이티브 컴포지터가 ANGLE 빌드에서만 성립하고(dcomp_compositor.rs:1218)
제로카피도 angle_d3d11_device_ptr 경유라, wr-d3d11에서는 표출 스택이 통째로
불성립하기 때문이다. 계측용 d3d11 백엔드는 video-perf-investigation에 남는다.

타일 수명(창·컨텍스트·paint target)을 tile.rs로 분리했다.
EOF
```

---

### Task 3: 가드밴드 `present_inset` 주입 + DPI 변경 재주입

winit_wall은 `overlapPx`를 파싱만 하고 쓰지 않아, 겹침이 설정된 레이아웃에서 타일이 밀린다. servoshell과 동일하게 `present_inset`으로 크롭한다.

**Files:**
- Modify: `components/servo/examples/winit_wall/tile.rs` (창 생성 직후 주입)
- Modify: `components/servo/examples/winit_wall/main.rs` (`ScaleFactorChanged` 핸들러)

**Interfaces:**
- Consumes: `WallLayout::tile_render_insets(tile_index, hidpi) -> Option<SideOffsets2D<i32, DevicePixel>>` (Task 1), `RenderingContext::set_present_inset(&self, inset: DeviceIntSideOffsets)` (`components/shared/paint/rendering_context.rs:146`). `DeviceIntSideOffsets`는 `SideOffsets2D<i32, DevicePixel>`이므로 변환이 필요 없다
- Produces: `pub(crate) fn apply_present_inset(layout: &WallLayout, tile_index: usize, hidpi: Scale<f32, DeviceIndependentPixel, DevicePixel>, rendering_context: &Rc<dyn RenderingContext>)` — 창 생성 시(Step 2)와 DPI 변경 시(Step 3) 두 곳에서 호출된다. 이 함수 밖에서 inset을 계산하는 코드는 만들지 않는다

- [ ] **Step 1: 주입 헬퍼를 `tile.rs`에 추가한다**

```rust
/// 타일의 가드밴드 여백을 렌더링 컨텍스트에 주입한다.
///
/// 값의 산출처는 `WallLayout::tile_render_insets` 하나뿐이다 — winit_wall은
/// 어떤 경로에서도 inset을 자체 계산하지 않는다. (servoshell 최종 리뷰가
/// "공유된 건 산식이고 값이 아니었다"로 지적한 발산의 재연 방지.)
pub(crate) fn apply_present_inset(
    layout: &WallLayout,
    tile_index: usize,
    hidpi: Scale<f32, DeviceIndependentPixel, DevicePixel>,
    rendering_context: &Rc<dyn RenderingContext>,
) {
    let Some(inset) = layout.tile_render_insets(tile_index, hidpi) else {
        return;
    };
    rendering_context.set_present_inset(inset);
    // 로거 초기화 이전에 창이 생성될 수 있어 info!는 유실된다 — servoshell 관례대로 eprintln.
    eprintln!(
        "wall: tile {tile_index} present_inset (top {}, right {}, bottom {}, left {})",
        inset.top, inset.right, inset.bottom, inset.left,
    );
}
```

- [ ] **Step 2: 창 생성 직후 호출한다**

`tile.rs`에서 `rendering_context`를 만든 직후(Task 2 Step 3의 `WindowRenderingContext::new_with_target_gpu(...)` 다음)에 추가한다:

```rust
            let hidpi = Scale::new(window.scale_factor() as f32);
            apply_present_inset(&config.layout, tile_index, hidpi, &rendering_context);
```

- [ ] **Step 3: DPI 변경 시 재주입한다**

`main.rs`의 `window_event`(612행 부근) `match event` 블록에서 `WindowEvent::CloseRequested` arm 앞에 추가한다:

```rust
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Self::Running(state) = self {
                    state.reapply_present_insets();
                }
            },
```

`AppState`(원본 204행)에는 레이아웃 필드가 없으므로 추가한다. `tiles: Vec<TileWindow>` 다음 줄:

```rust
    tiles: Vec<TileWindow>,
    // DPI 변경 시 가드밴드 여백을 다시 계산하려면 레이아웃이 필요하다.
    layout: WallLayout,
```

`AppState`를 만드는 지점에서 `layout: config.layout.clone(),`을 채운다(`WallLayout`은 `#[derive(Clone)]`이다 — Task 1에서 옮긴 파일 13행에서 확인).

그리고 `render_all_tiles`(281행)와 같은 `impl AppState` 블록에 메서드를 추가한다:

```rust
    /// DPI가 바뀌면 가드밴드 여백을 다시 계산해 주입한다. 창 생성 시의 스냅샷을
    /// 그대로 두면 혼합 DPI 환경에서 blit 경로와 값이 갈린다.
    ///
    /// 이벤트는 창 하나에 대해 오지만 전 타일을 각자의 scale factor로 다시
    /// 계산한다 — 타일마다 DPI가 다를 수 있기 때문이다.
    fn reapply_present_insets(&self) {
        for (tile_index, tile) in self.tiles.iter().enumerate() {
            let hidpi = Scale::new(tile.window.scale_factor() as f32);
            tile::apply_present_inset(&self.layout, tile_index, hidpi, &tile.rendering_context);
        }
    }
```

Step 3의 이벤트 arm도 인자 없이 호출하도록 맞춘다:

```rust
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Self::Running(state) = self {
                    state.reapply_present_insets();
                }
            },
```

- [ ] **Step 4: 빌드 확인**

```bash
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl
```

기대: exit 0.

- [ ] **Step 5: 릴리스 빌드로 스모크 — inset 로그 확인**

```bash
cargo build --release -p servo --example winit_wall --features media-gstreamer,no-wgl
```

DLL을 exe 옆에 복사한 뒤(`target\release\examples\`), 2x1 레이아웃으로 실행해 stderr를 받는다:

```powershell
$p = Start-Process -PassThru -FilePath .\target\release\examples\winit_wall.exe `
  -ArgumentList '--wall-all-tiles','--wall-layout','etc\multigpu\config\wall_layout.example_2x1_display.json' `
  -RedirectStandardError inset.log
Start-Sleep -Seconds 8
$p.CloseMainWindow() | Out-Null; $p.WaitForExit(20000) | Out-Null
Select-String -Path inset.log -Pattern 'present_inset'
```

기대 2줄 — servoshell 실측과 같은 값이어야 한다:
```
wall: tile 0 present_inset (top 0, right 32, bottom 0, left 0)
wall: tile 1 present_inset (top 0, right 0, bottom 0, left 32)
```

- [ ] **Step 6: 사용자 육안 판정 요청**

눈금 프로브로 크롭 정확성을 확인한다. **판정은 사용자가 한다** — 다음을 요청하고 응답을 기다린다:

```powershell
.\target\release\examples\winit_wall.exe --wall-all-tiles `
  --wall-layout etc\multigpu\config\wall_layout.example_2x1_display.json `
  "file:///W:/servo_multigpu-tiled-wall/tests/html/multigpu_wall_ruler_probe.html"
```

판정 기준: 각 타일 창 구석의 좌표 라벨이 그 타일 `rect` 원점과 일치할 것. 이음매에서 `1920,480` 라벨이 `1800,480`과 정확히 120px 간격일 것(밀리면 152px).

- [ ] **Step 7: 커밋**

```bash
git add components/servo/examples/winit_wall
git commit -F - <<'EOF'
feat(winit_wall): 가드밴드 크롭 present_inset 주입 + DPI 변경 재주입

overlapPx를 파싱만 하고 쓰지 않아 겹침 레이아웃에서 타일이 밀리던 것을
고친다. 값의 산출처는 WallLayout::tile_render_insets 하나뿐이고 winit_wall은
자체 계산하지 않는다.

ScaleFactorChanged에서 재주입한다 — 창 생성 시 스냅샷만 두면 혼합 DPI에서
blit 경로와 값이 갈린다(servoshell 최종 리뷰 지적사항).
EOF
```

---

### Task 4: borderless fullscreen

flip-model present 자격을 얻기 위해, 타일이 디스플레이를 정확히 채울 때 borderless fullscreen으로 만든다.

**Files:**
- Modify: `components/servo/examples/winit_wall/tile.rs`

**Interfaces:**
- Consumes: Task 2의 타일 창 생성 경로, `spatial` 토폴로지 목록
- Produces: 없음 (창 속성만 바뀜)

- [ ] **Step 1: 창 생성 직후 조건부로 fullscreen을 건다**

`tile.rs`에서 `event_loop.create_window(attributes)` 직후에 추가한다. 조건은 servoshell `desktop/headed_window.rs:283-287`과 동일하다 — **타일 `rect` 크기가 대상 디스플레이 크기와 정확히 일치할 때만**:

```rust
            // 타일이 디스플레이를 정확히 채울 때만 borderless fullscreen으로 만든다.
            // flip-model present 자격을 얻기 위한 것으로, 크기가 다르면 일반 창으로 둔다.
            // 토폴로지 폴백 경로에서는 winit 모니터 핸들이 없을 수 있으므로 그때도 일반 창.
            if let Some(disp) = spatial.get(tile.display) {
                let tile_size = window.inner_size();
                // DisplayTopology의 width/height는 i32, winit inner_size는 u32라 캐스팅한다.
                let fills_display = tile_size.width as i32 == disp.width &&
                    tile_size.height as i32 == disp.height;
                if fills_display && let Some(monitor) = window.current_monitor() {
                    eprintln!(
                        "wall: tile {tile_index} matches display {} size {}x{}; using borderless \
                         fullscreen for flip-model present eligibility.",
                        tile.display, tile_size.width, tile_size.height,
                    );
                    window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(Some(
                        monitor,
                    ))));
                }
            }
```

필드는 `DisplayTopology { left: i32, top: i32, width: i32, height: i32, adapter_index: usize, luid: (i32, u32), device_name: String, attached_to_desktop: bool }`(`components/shared/paint/rendering_context.rs:414-423`)다. 창 위치 지정에 이미 `disp.left`/`disp.top`을 쓰고 있는 같은 구조체다.

- [ ] **Step 2: 빌드 확인**

```bash
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl
```

기대: exit 0.

- [ ] **Step 3: 릴리스 스모크 — fullscreen 로그 확인**

Task 3 Step 5와 같은 방식으로 실행해 stderr에서 확인한다:

```powershell
Select-String -Path fs.log -Pattern 'borderless fullscreen'
```

기대: 타일 크기가 디스플레이와 일치하는 레이아웃에서 타일 수만큼 출력. 크기가 다른 레이아웃(예: 단일 타일이 가상 뷰포트 전체를 덮는 경우)에서는 0줄.

- [ ] **Step 4: 커밋**

```bash
git add components/servo/examples/winit_wall
git commit -F - <<'EOF'
feat(winit_wall): 타일이 디스플레이를 채울 때 borderless fullscreen

flip-model present 자격 확보. 조건은 servoshell headed_window.rs:283-287과
동일하게 타일 rect 크기 == 디스플레이 크기일 때로 한정하고, 크기가 다르거나
토폴로지 폴백으로 모니터 핸들이 없으면 일반 창으로 둔다.
EOF
```

---

### Task 5: vsync refresh driver (opt-in)

멀티GPU 간 vsync 동기화의 기반. `DwmFlush`가 이 환경에서 스핀-웨이트로 동작해 코어 1개를 상시 소모하므로 **기본 off**로 둔다.

**Files:**
- Create: `components/servo/examples/winit_wall/vsync_refresh_driver.rs` (servoshell 파일을 무수정 복사)
- Modify: `components/servo/examples/winit_wall/main.rs` (모듈 선언 + 드라이버 1개 생성)
- Modify: `components/servo/examples/winit_wall/tile.rs` (Task 2에서 만든 컨텍스트 생성자 교체)

**Interfaces:**
- Consumes: `servo::RefreshDriver`(공개 트레잇), `WindowRenderingContext::new_with_optional_refresh_driver_and_target_gpu(display_handle, window_handle, size, refresh_driver: Option<Rc<dyn RefreshDriver>>, requested_gpu_index: Option<usize>)` (`components/shared/paint/rendering_context.rs:1392-1398`)
- Produces: `vsync_refresh_driver::DwmVsyncRefreshDriver::new() -> DwmVsyncRefreshDriver`. 타일 루프 밖에서 1개만 만들어 `Rc`로 공유한다

- [ ] **Step 1: 드라이버 파일을 그대로 복사한다**

`ports/servoshell/desktop/vsync_refresh_driver.rs`(112줄)는 servoshell 고유 타입에 의존하지 않는다 — 임포트가 `std::sync`, `std::thread`, `servo::RefreshDriver`뿐이다(17-20행). 수정 없이 복사한다.

```bash
cp ports/servoshell/desktop/vsync_refresh_driver.rs components/servo/examples/winit_wall/vsync_refresh_driver.rs
```

`main.rs`에 모듈을 선언한다:

```rust
#[cfg(target_os = "windows")]
mod vsync_refresh_driver;
```

- [ ] **Step 2: 게이트 판정 — servoshell과 문자열 허용 범위까지 동일하게**

`main.rs`의 타일 생성 루프 **앞에서 한 번만** 만든다. 드라이버는 `Shared { state: Mutex<(Vec<StartFrameCallback>, bool)> }`로 콜백을 **누적**하므로(`vsync_refresh_driver.rs:31-35`), 하나를 전 타일이 공유해도 각 타일의 콜백이 모두 등록되고 DwmFlush 스레드는 1개만 뜬다. 타일마다 만들면 스레드가 타일 수만큼 늘어난다.

```rust
    // SERVO_WIN_VSYNC=1일 때만 DWM 합성 클럭에 프레임 생산을 페이싱한다.
    // 기본 off인 이유: 이 환경에서 DwmFlush가 스핀-웨이트로 동작해 코어 1개를
    // 상시 소모한다. 멀티GPU 간 vsync 동기화 작업의 기반으로 두되 표출 기본값은 아니다.
    //
    // 드라이버는 콜백을 누적하므로 전 타일이 하나를 공유한다 — 타일마다 만들면
    // DwmFlush 스레드가 타일 수만큼 뜬다.
    #[cfg(target_os = "windows")]
    let vsync_driver: Option<Rc<dyn servo::RefreshDriver>> = {
        let enabled = std::env::var("SERVO_WIN_VSYNC").is_ok_and(|value| {
            value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("on")
        });
        if enabled {
            eprintln!("wall: SERVO_WIN_VSYNC=1: pacing frame production to DWM vsync (DwmFlush).");
            Some(Rc::new(vsync_refresh_driver::DwmVsyncRefreshDriver::new()))
        } else {
            None
        }
    };
    #[cfg(not(target_os = "windows"))]
    let vsync_driver: Option<Rc<dyn servo::RefreshDriver>> = None;
```

판정식은 servoshell `desktop/headed_window.rs:394-396`과 글자 그대로 같다(`"1"`, `"true"`, `"on"` 허용).

- [ ] **Step 3: 렌더링 컨텍스트 생성자를 교체한다**

Task 2 Step 3(d)에서 평탄화한 `WindowRenderingContext::new_with_target_gpu(...)` 호출을, refresh driver를 받는 변형으로 바꾼다. 시그니처는 `rendering_context.rs:1392-1398`이며 servoshell도 같은 형태로 쓴다(`headed_window.rs:408-415`):

```rust
            #[cfg(target_os = "windows")]
            let rendering_context_result =
                WindowRenderingContext::new_with_optional_refresh_driver_and_target_gpu(
                    display_handle,
                    window_handle,
                    window.inner_size(),
                    vsync_driver.clone(),
                    gpu_index,
                );
            #[cfg(not(target_os = "windows"))]
            let rendering_context_result = WindowRenderingContext::new_with_target_gpu(
                display_handle,
                window_handle,
                window.inner_size(),
                gpu_index,
            );
            let rendering_context: Rc<dyn RenderingContext> = Rc::new(
                rendering_context_result
                    .expect("Could not create RenderingContext for tile window"),
            );
```

`vsync_driver`는 타일 루프 밖에서 만들어졌으므로 타일마다 `.clone()`(Rc 복제)해 넘긴다.

- [ ] **Step 4: 빌드 확인**

```bash
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl
```

기대: exit 0.

- [ ] **Step 5: on/off A/B 스모크**

```powershell
# off (기본)
Remove-Item Env:\SERVO_WIN_VSYNC -ErrorAction SilentlyContinue
# ... 실행 후 로그에 'DWM vsync' 줄이 없어야 함

# on
$env:SERVO_WIN_VSYNC = '1'
# ... 실행 후 로그에 'DWM vsync' 줄이 있어야 하고, 타일 표출이 정상이어야 함
```

기대: off에서 0줄 / on에서 1줄, 양쪽 모두 panic 0이고 타일이 정상 표출.

드라이버 공유가 실제로 됐는지 스레드 수로 확인한다 — 타일이 몇 개든 **`DwmVsyncRefresh` 스레드는 1개**여야 한다:

```powershell
# on 상태로 실행 중일 때
Get-Process winit_wall | ForEach-Object { $_.Threads.Count }
# 정밀 확인이 필요하면 Process Explorer 등으로 DwmVsyncRefresh 이름 스레드 개수를 센다
```

2개 이상이면 드라이버가 타일마다 생성된 것이므로 Step 2의 생성 위치(타일 루프 **밖**)를 다시 확인한다.

- [ ] **Step 6: 커밋**

```bash
git add components/servo/examples/winit_wall
git commit -F - <<'EOF'
feat(winit_wall): DWM vsync refresh driver (SERVO_WIN_VSYNC opt-in)

멀티GPU 간 vsync 동기화의 기반으로 배선한다. servoshell과 같은 환경변수로
opt-in하며 기본 off다 — 이 환경에서 DwmFlush가 스핀-웨이트로 동작해 코어
1개를 상시 소모하기 때문이다.
EOF
```

---

### Task 6: 통합 검증 + 문서 갱신

**Files:**
- Modify: `CLAUDE.md` (저장소 루트의 상위 — **git 밖이라 커밋 불가, 편집만**)
- Modify: `docs/superpowers/specs/2026-08-06-winit-wall-presentation-shell-design.md` (구현 결과 절 추가)

- [ ] **Step 1: 정적 검사 일괄**

```bash
cargo test -p servo-paint-api wall_layout --lib
cargo check -p servoshell
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl
git diff --check
```

기대: 테스트 13 passed, check/build exit 0, diff --check 출력 없음.

- [ ] **Step 2: servoshell 무회귀 (필수)**

`wall_layout`이 크레이트 밖으로 나갔으므로 servoshell이 그대로인지 확인한다. release로 빌드해 눈금 프로브를 띄우고 stderr를 받는다:

```powershell
Select-String -Path servoshell_regress.log -Pattern 'scroll_offsets=matched|panic|present_inset'
```

기대: `scroll_offsets=matched` 다수, panic 0, `present_inset` 2줄이 이전과 동일한 값.
**육안 판정(사용자)**: 이음매 라벨 간격 120px 유지.

- [ ] **Step 3: winit_wall 기능 검증**

- 2x1 월 `ready=2/2` 배리어 (로그에서 확인)
- DComp on/off A/B — `SERVO_COMPOSITOR_DCOMP=1`에서 정상 표출되고, servoshell에서 관측되던 하단 밴드·잔여 blit이 **없어야** 정상(egui 부재)
- 미디어 표출 1건 — 비디오 그리드 페이지로 `SYNC_GROUP` lockstep 성립 확인

**육안 판정은 사용자에게 요청하고 응답을 기다린다.**

- [ ] **Step 4: CLAUDE.md 갱신**

"Test, format, validate" 절의 다음 줄을

```
cargo test -p servoshell wall_layout --lib      # the wall layout unit tests (currently 11)
```

다음으로 바꾼다:

```
cargo test -p servo-paint-api wall_layout --lib  # the wall layout unit tests (currently 13)
```

같은 절에 winit_wall 빌드/실행 명령도 추가한다. **루트 CLAUDE.md는 이 저장소의 git 밖이라 커밋할 수 없다** — 편집만 하고 커밋 대상에서 제외한다.

- [ ] **Step 5: 스펙에 구현 결과를 추기한다**

`docs/superpowers/specs/2026-08-06-winit-wall-presentation-shell-design.md` 끝에 `## 구현 결과` 절을 추가한다: 실제 커밋 범위, 설계에서 이탈한 부분(있다면 이유), 이월 항목.

- [ ] **Step 6: 커밋**

```bash
git add docs/superpowers/specs/2026-08-06-winit-wall-presentation-shell-design.md
git commit -F - <<'EOF'
docs: winit_wall 표출 셸 이식 구현 결과 기록

스펙에 실제 커밋 범위와 설계 이탈 사항, 이월 항목을 추기한다.
CLAUDE.md의 wall_layout 테스트 명령도 새 크레이트 기준으로 갱신했다(루트
CLAUDE.md는 git 밖이라 이 커밋에 포함되지 않는다).
EOF
```

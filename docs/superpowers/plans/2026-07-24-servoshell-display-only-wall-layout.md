# servoshell display-only 공간배치 + auto-GPU wall_layout 이식 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** servoshell 월 타일이 `monitor`+`gpu` 대신 공간 `display` 인덱스를 갖고, 창 생성 시 DXGI 디스플레이 토폴로지를 해석해 실제 desktop 좌표에 배치 + 구동 adapter에 렌더링 컨텍스트를 자동 바인딩하게 한다.

**Architecture:** winit_wall 예제에만 있던 display-only 공간배치 방식을 servoshell에 이식. `rendering_context.rs`(servo-paint-api)에 토폴로지 헬퍼(`enumerate_display_topology`/`spatial_order`/`DisplayTopology`)를 verbatim 이식하고 lib.rs로 re-export, `wall_layout.rs`를 display 스키마(레거시 monitor/gpu 별칭·무시)로 바꾸고, `headed_window.rs`가 `HeadedWindow::new`에서 창마다 토폴로지를 1회 해석해 배치+auto-GPU한다.

**Tech Stack:** Rust, winapi(DXGI EnumAdapters1/EnumOutputs), surfman, winit, serde_json.

## Global Constraints

- 작업 디렉터리: **`W:\servo_multigpu-tiled-wall`** (subst 필수 — 긴 경로에서 mozangle build.rs가 Os error 206). W: 없으면 `subst W: F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser`.
- 모든 빌드/검사 전 PowerShell에서 `. W:\scripts\servo_env.ps1` 소싱 후 `$ErrorActionPreference='Continue'`. (PowerShell 도구가 exit 1로 깨지면 bash에서 `powershell.exe -NoProfile -Command "..."`로 우회.)
- servoshell은 `no-wgl` 피처로 빌드됨(`ports/servoshell/Cargo.toml:138`) → 토폴로지 헬퍼(`#[cfg(all(target_os="windows", feature="no-wgl"))]`)가 **실제 활성**. 이 게이트를 헬퍼·비헬퍼 폴백 양쪽에 그대로 유지.
- 미디어 무관 변경이라 런타임 검증은 `cargo build -p servoshell`로 충분(미디어 피처 불필요). 단 full 확인이 필요하면 `mach build`.
- release 최종 링크에서 lld-link 0xc0000005 시: `$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER`를 MSVC link.exe 풀경로로 설정.
- 렌더 필요한 실행은 반드시 `--wall-all-tiles`.
- `display` = 공간 인덱스(좌상단=0, 좌→우 그다음 위→아래). 레거시 `monitor`는 `display` 별칭(deprecation warn), `gpu`는 무시(warn).
- 완료 조건: `cargo check -p servoshell` + `rustfmt --edition 2024 --check <touched .rs>` + `git diff --check`.
- 커밋 메시지 한국어 + 트레일러:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
  `Claude-Session: https://claude.ai/code/session_01UKn1GSL7Cq5V3ipA1eMcMt`

---

### Task 1: 토폴로지 헬퍼 이식 — rendering_context.rs + lib.rs re-export

**Files:**
- Modify: `components/shared/paint/rendering_context.rs` (기존 `dxgi_luid_for_gpu_index` 정의 뒤, `SurfmanRenderingContext` 정의 앞에 삽입 — 현재 그 경계는 `dxgi_luid_for_gpu_index`의 비-windows 폴백 `pub fn dxgi_luid_for_gpu_index(_gpu_index: usize) -> Option<(i32, u32)> { None }` 직후)
- Modify: `components/servo/lib.rs:48-51` (paint_api::rendering_context re-export 블록)

**Interfaces:**
- Produces: `paint_api::rendering_context::DisplayTopology { pub left:i32, pub top:i32, pub width:i32, pub height:i32, pub adapter_index:usize, pub luid:(i32,u32), pub device_name:String, pub attached_to_desktop:bool }`, `pub fn enumerate_display_topology() -> Vec<DisplayTopology>`, `pub fn spatial_order(topology: &[DisplayTopology]) -> Vec<DisplayTopology>`; servo crate에서 `servo::{DisplayTopology, enumerate_display_topology, spatial_order}`로 사용 가능 — Task 2가 소비.

- [ ] **Step 1: 헬퍼 + 테스트를 rendering_context.rs에 삽입 (TDD: 테스트 포함 이식)**

`dxgi_luid_for_gpu_index`의 비-windows 폴백 정의 직후에 아래를 삽입(wall-spatial-display-autogpu에서 verbatim; DisplayTopology 앞 doc-comment 첫 줄은 이 파일 맥락에 맞게 시작):

```rust
/// A physical display (DXGI output) paired with the GPU adapter that drives it.
///
/// `adapter_index` is the DXGI `EnumAdapters1` order — the same value
/// `create_dxgi_adapter_by_index` / a `requested_gpu_index` consume — so a tile shown on
/// this display can bind to the GPU that drives it by passing `adapter_index` straight
/// through. `luid` is the matching `AdapterLuid` (see [`dxgi_luid_for_gpu_index`]);
/// `left/top/width/height` are the output's desktop virtual coordinates in physical pixels.
#[derive(Clone, Debug)]
pub struct DisplayTopology {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub adapter_index: usize,
    pub luid: (i32, u32),
    pub device_name: String,
    pub attached_to_desktop: bool,
}

/// Enumerate every DXGI output (physical display) together with the adapter that drives it.
///
/// Returns the displays in raw enumeration order (adapter-major); call [`spatial_order`]
/// to turn this into a row-major spatial index. Returns an empty vector off Windows, on
/// non-`no-wgl` builds, or if DXGI enumeration fails — callers should then fall back to
/// their previous (winit monitor index) behaviour.
#[cfg(all(target_os = "windows", feature = "no-wgl"))]
#[expect(unsafe_code)]
pub fn enumerate_display_topology() -> Vec<DisplayTopology> {
    use std::os::raw::c_void;
    use std::ptr;

    use winapi::shared::dxgi::{DXGI_ADAPTER_DESC1, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIOutput};

    fn utf16_z_to_string(buffer: &[u16]) -> String {
        let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..len])
    }

    let mut displays = Vec::new();
    // SAFETY: standard DXGI COM enumeration; every returned pointer is checked non-null and
    // wrapped in a `ComPtr` that releases it, mirroring `create_dxgi_adapter_by_index`.
    unsafe {
        let mut dxgi_factory: *mut IDXGIFactory1 = ptr::null_mut();
        let result = dxgi::CreateDXGIFactory1(
            &IDXGIFactory1::uuidof(),
            &mut dxgi_factory as *mut *mut IDXGIFactory1 as *mut *mut c_void,
        );
        if !winerror::SUCCEEDED(result) || dxgi_factory.is_null() {
            return displays;
        }
        let dxgi_factory = ComPtr::from_raw(dxgi_factory);

        let mut adapter_index: u32 = 0;
        loop {
            let mut adapter1: *mut IDXGIAdapter1 = ptr::null_mut();
            if !winerror::SUCCEEDED((*dxgi_factory).EnumAdapters1(adapter_index, &mut adapter1))
                || adapter1.is_null()
            {
                break;
            }
            let adapter1 = ComPtr::from_raw(adapter1);

            let mut desc: DXGI_ADAPTER_DESC1 = std::mem::zeroed();
            let luid = if winerror::SUCCEEDED((*adapter1).GetDesc1(&mut desc)) {
                (desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart)
            } else {
                (0, 0)
            };

            let mut output_index: u32 = 0;
            loop {
                let mut output: *mut IDXGIOutput = ptr::null_mut();
                if !winerror::SUCCEEDED((*adapter1).EnumOutputs(output_index, &mut output))
                    || output.is_null()
                {
                    break;
                }
                let output = ComPtr::from_raw(output);

                let mut output_desc: DXGI_OUTPUT_DESC = std::mem::zeroed();
                if winerror::SUCCEEDED((*output).GetDesc(&mut output_desc)) {
                    let rect = output_desc.DesktopCoordinates;
                    displays.push(DisplayTopology {
                        left: rect.left,
                        top: rect.top,
                        width: rect.right - rect.left,
                        height: rect.bottom - rect.top,
                        adapter_index: adapter_index as usize,
                        luid,
                        device_name: utf16_z_to_string(&output_desc.DeviceName),
                        attached_to_desktop: output_desc.AttachedToDesktop != 0,
                    });
                }
                output_index += 1;
            }
            adapter_index += 1;
        }
    }
    displays
}

#[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
pub fn enumerate_display_topology() -> Vec<DisplayTopology> {
    Vec::new()
}

/// Order displays spatially: top-left first, left→right within a row, then top→bottom.
///
/// The returned vector's position is the spatial index a wall layout references as
/// `display`. Only desktop-attached displays are included. Displays are grouped into rows
/// by vertical overlap (≥50% of the shorter height, with a median-height tolerance) so
/// mixed-resolution rows still band together.
pub fn spatial_order(topology: &[DisplayTopology]) -> Vec<DisplayTopology> {
    let mut displays: Vec<DisplayTopology> = topology
        .iter()
        .filter(|display| display.attached_to_desktop)
        .cloned()
        .collect();
    if displays.is_empty() {
        return displays;
    }

    let mut heights: Vec<i32> = displays
        .iter()
        .map(|display| display.height.max(1))
        .collect();
    heights.sort_unstable();
    let median_height = heights[heights.len() / 2];
    let tolerance = (median_height / 2).max(1);

    // Pre-sort by (top, left) so each new row's first member is its topmost-leftmost.
    displays.sort_by(|a, b| a.top.cmp(&b.top).then(a.left.cmp(&b.left)));

    let mut rows: Vec<Vec<DisplayTopology>> = Vec::new();
    for display in displays {
        let mut placed = None;
        for (row_index, row) in rows.iter().enumerate() {
            let representative = &row[0];
            let overlap_top = display.top.max(representative.top);
            let overlap_bottom =
                (display.top + display.height).min(representative.top + representative.height);
            let overlap = (overlap_bottom - overlap_top).max(0);
            let shorter = display.height.min(representative.height).max(1);
            if overlap * 2 >= shorter || (display.top - representative.top).abs() <= tolerance {
                placed = Some(row_index);
                break;
            }
        }
        match placed {
            Some(row_index) => rows[row_index].push(display),
            None => rows.push(vec![display]),
        }
    }

    rows.sort_by_key(|row| row.iter().map(|display| display.top).min().unwrap_or(0));
    let mut ordered = Vec::new();
    for mut row in rows {
        row.sort_by_key(|display| display.left);
        ordered.append(&mut row);
    }
    ordered
}

#[cfg(test)]
mod display_topology_tests {
    use super::{DisplayTopology, spatial_order};

    fn display(left: i32, top: i32, width: i32, height: i32, adapter: usize) -> DisplayTopology {
        DisplayTopology {
            left,
            top,
            width,
            height,
            adapter_index: adapter,
            luid: (0, adapter as u32),
            device_name: format!("\\\\.\\DISPLAY{adapter}"),
            attached_to_desktop: true,
        }
    }

    fn order(displays: &[DisplayTopology]) -> Vec<(i32, i32)> {
        spatial_order(displays)
            .iter()
            .map(|display| (display.left, display.top))
            .collect()
    }

    #[test]
    fn row_major_2x1() {
        let displays = vec![
            display(1920, 0, 1920, 1080, 1),
            display(0, 0, 1920, 1080, 0),
        ];
        assert_eq!(order(&displays), vec![(0, 0), (1920, 0)]);
    }

    #[test]
    fn row_major_2x2() {
        let displays = vec![
            display(1920, 1080, 1920, 1080, 3),
            display(0, 1080, 1920, 1080, 2),
            display(1920, 0, 1920, 1080, 1),
            display(0, 0, 1920, 1080, 0),
        ];
        assert_eq!(
            order(&displays),
            vec![(0, 0), (1920, 0), (0, 1080), (1920, 1080)]
        );
    }

    #[test]
    fn three_in_a_row() {
        let displays = vec![
            display(3840, 0, 1920, 1080, 2),
            display(0, 0, 1920, 1080, 0),
            display(1920, 0, 1920, 1080, 1),
        ];
        assert_eq!(order(&displays), vec![(0, 0), (1920, 0), (3840, 0)]);
    }

    #[test]
    fn mixed_resolution_row_bands_together() {
        let displays = vec![
            display(3840, 0, 1920, 1080, 1),
            display(0, 0, 3840, 2160, 0),
        ];
        assert_eq!(order(&displays), vec![(0, 0), (3840, 0)]);
    }

    #[test]
    fn skips_unattached_displays() {
        let mut detached = display(9999, 9999, 1920, 1080, 5);
        detached.attached_to_desktop = false;
        let displays = vec![display(0, 0, 1920, 1080, 0), detached];
        assert_eq!(order(&displays), vec![(0, 0)]);
    }
}
```

주의: `dxgi`, `IDXGIFactory1`, `winerror`, `ComPtr`는 이 파일에 이미 `dxgi_luid_for_gpu_index`가 쓰는 심볼이므로 추가 `use`가 불필요(같은 스코프). 만약 컴파일러가 특정 심볼(예: `DXGI_OUTPUT_DESC`)을 못 찾으면 그 함수 내부 `use winapi::shared::dxgi::{...}`에 추가(이미 함수-로컬 import로 넣어둠).

- [ ] **Step 2: 테스트가 FAIL→PASS 하는지 확인**

Run: `powershell.exe -NoProfile -Command ". W:\scripts\servo_env.ps1; \$ErrorActionPreference='Continue'; cd W:\servo_multigpu-tiled-wall; cargo test -p servo-paint-api spatial_order"`
Expected: 5 passed (`row_major_2x1`, `row_major_2x2`, `three_in_a_row`, `mixed_resolution_row_bands_together`, `skips_unattached_displays`).
(테스트 exe가 링크/DLL 문제로 실행 불가하면 그 사실을 기록하고 `cargo check -p servo-paint-api` 성공으로 대체 + Task 3 런타임에서 보완.)

- [ ] **Step 3: lib.rs re-export 확장**

`components/servo/lib.rs`의 re-export 블록을 다음으로 교체:

```rust
pub use paint_api::rendering_context::{
    DisplayTopology, OffscreenRenderingContext, RenderingContext, SoftwareRenderingContext,
    WindowRenderingContext, enumerate_display_topology, spatial_order,
};
```

- [ ] **Step 4: 컴파일 확인**

Run: `cargo check -p servoshell`
Expected: 성공 (새 심볼 추가만 — 아무 것도 제거 안 함).

- [ ] **Step 5: Commit**

```bash
git add components/shared/paint/rendering_context.rs components/servo/lib.rs
git commit -m "feat(paint): DXGI 디스플레이 토폴로지 헬퍼(enumerate_display_topology/spatial_order) 이식 + servo re-export"
```

---

### Task 2: servoshell display-only 배선 — wall_layout.rs + headed_window.rs + app.rs

**Files:**
- Modify: `ports/servoshell/wall_layout.rs` (WallTile struct `:20-25`, `parse_tiles` `:180-188`, 테스트 `:303-360+`)
- Modify: `ports/servoshell/desktop/headed_window.rs` (wall 타일 배치 블록 `:165-243`, present 로그 `:754-763`, servo import `:22-30`)
- Modify: `ports/servoshell/desktop/app.rs` (타일 plan 로그 `:207-215`)

**Interfaces:**
- Consumes: Task 1 `servo::{DisplayTopology, enumerate_display_topology, spatial_order}`
- Produces: `WallTile { pub(crate) display: usize, pub(crate) rect: Rect<i32, DeviceIndependentPixel> }` (monitor/gpu 제거) — 이후 태스크 없음(servoshell 내부 최종 소비).

> **컴파일 경계 주의**: monitor/gpu 필드를 제거하면 headed_window.rs·app.rs가 즉시 깨지므로 세 파일을 한 태스크로 함께 바꾼다. Step 순서: 파서 TDD → headed_window → app.rs → present 로그 → crate 컴파일.

- [ ] **Step 1: wall_layout 파서 테스트를 display 스키마로 갱신 (실패 유도)**

`ports/servoshell/wall_layout.rs`의 `mod tests`에서 3개 테스트의 JSON을 display 스키마로 바꾸고, 레거시 별칭 테스트 1개를 추가. 먼저 `parses_valid_wall_layout`의 tiles를:

```rust
                "tiles": [
                    { "display": 0, "rect": [0, 0, 3840, 2160] },
                    { "display": 1, "rect": [3840, 0, 3840, 2160] }
                ],
```
그리고 그 아래 assert에 `assert_eq!(layout.tiles[0].display, 0);` `assert_eq!(layout.tiles[1].display, 1);` 추가.

`rejects_out_of_bounds_tile`의 tile을 `{ "display": 0, "rect": [90, 0, 20, 20] }`로, `calculates_overlap_render_rect_clamped_to_virtual_viewport`의 3 tiles를 각각 `{ "display": 0, "rect": [0,0,1920,1080] }` / `{ "display": 1, "rect": [1920,0,1920,1080] }` / `{ "display": 2, "rect": [3840,0,1920,1080] }`로 변경.

`mod tests` 안에 새 테스트 추가:

```rust
    #[test]
    fn accepts_legacy_monitor_alias_and_ignores_gpu() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 3840, "height": 1080 },
                "tiles": [
                    { "monitor": 2, "gpu": 7, "rect": [0, 0, 1920, 1080] },
                    { "monitor": 0, "gpu": 3, "rect": [1920, 0, 1920, 1080] }
                ]
            }"#,
        )
        .expect("legacy monitor+gpu layout should still parse");

        assert_eq!(layout.tiles[0].display, 2);
        assert_eq!(layout.tiles[1].display, 0);
    }

    #[test]
    fn rejects_tile_without_display_or_monitor() {
        let error = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 3840, "height": 1080 },
                "tiles": [ { "rect": [0, 0, 1920, 1080] } ]
            }"#,
        )
        .expect_err("tile without display/monitor should fail");
        assert!(error.to_string().contains("display"));
    }
```

- [ ] **Step 2: 테스트 실행 → 컴파일 실패(필드 없음) 확인**

Run: `cargo test -p servoshell wall_layout --lib`
Expected: 컴파일 에러 — `layout.tiles[0].display` / `WallTile`에 `display` 필드 없음, 또는 `monitor`/`gpu` 제거 전이라 아직 통과할 수도 있음. 이 단계 목적은 이후 Step에서 GREEN 만들기.

- [ ] **Step 3: WallTile 스키마 + parse_tiles 교체**

`WallTile` 정의(`:20-25`)를:

```rust
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WallTile {
    /// Spatial display index (top-left = 0, left→right then top→bottom). Resolved at
    /// window-creation time against the DXGI display topology; the GPU that drives that
    /// display is auto-assigned. The legacy `monitor` field is accepted as an alias.
    pub(crate) display: usize,
    pub(crate) rect: Rect<i32, DeviceIndependentPixel>,
}
```

`parse_tiles`의 루프 본문(`:181-186`)을:

```rust
    for (index, tile) in tiles.iter().enumerate() {
        let display = match get_usize(tile, "display") {
            Ok(display) => display,
            Err(_) => {
                let monitor = get_usize(tile, "monitor").map_err(|_| {
                    WallLayoutError::Invalid(format!(
                        "tile {index} must have a 'display' (spatial index) field"
                    ))
                })?;
                log::warn!(
                    "wall tile {index}: 'monitor' is deprecated; use 'display' (spatial index, \
                     top-left = 0)"
                );
                monitor
            },
        };
        if tile.get("gpu").is_some() {
            log::warn!(
                "wall tile {index}: 'gpu' is ignored; the GPU is auto-assigned from the adapter \
                 that drives the chosen display"
            );
        }
        let rect = get_rect(tile, "rect")?;
        validate_tile_rect(index, rect, virtual_viewport)?;
        parsed_tiles.push(WallTile { display, rect });
    }
```

(파일 상단에 `use log;`가 없으면 `log::warn!` 대신 이미 임포트된 매크로를 사용 — 현재 wall_layout.rs는 log 매크로 미사용이므로 `log::warn!` 풀경로가 안전. `log`는 워크스페이스 의존.)

- [ ] **Step 4: headed_window.rs 배치 + auto-GPU 교체**

먼저 servo import(`:22-30`)에 토폴로지 심볼 추가 — 기존 `use servo::{ ... };` 블록에 `DisplayTopology, enumerate_display_topology, spatial_order,`를 알파벳 순서에 맞게 삽입(중복 주의).

그다음 wall 타일 블록(`:165-243`)을 다음으로 교체. 핵심: 토폴로지 1회 해석 → display 매칭 시 실좌표 배치 + adapter 자동, 미스 시 winit-nth 폴백. borderless 풀스크린은 해석된 display 좌표에 걸친 winit monitor를 찾아 유지.

```rust
        let mut wall_tile_fullscreen = false;
        let mut wall_tile_monitor = None;
        let mut wall_auto_gpu_index: Option<usize> = None;
        if let Some(layout) = &servoshell_preferences.wall_layout
            && let Some(tile) = layout.tiles.get(servoshell_preferences.wall_tile_index)
        {
            // Resolve the physical display topology once. `tile.display` is a spatial index
            // (top-left = 0, row-major); place the window at that display's real desktop origin
            // and bind its rendering context to the adapter that drives it. Fall back to the
            // winit monitor index when topology is unavailable (non-no-wgl build / enumeration
            // failed / index out of range).
            let spatial = spatial_order(&enumerate_display_topology());
            let requested_physical_size = |scale: f64| {
                PhysicalSize::new(
                    (inner_size.width as f64 * scale).round() as u32,
                    (inner_size.height as f64 * scale).round() as u32,
                )
            };
            match spatial.get(tile.display) {
                Some(disp) => {
                    // Find the winit monitor whose desktop position matches this display's
                    // origin, so the borderless-fullscreen path (which needs a winit monitor
                    // handle) can still engage; fall back to positioning by raw coordinates.
                    let matching_monitor = winit_window.available_monitors().find(|monitor| {
                        let position = monitor.position();
                        position.x == disp.left && position.y == disp.top
                    });
                    let scale = matching_monitor
                        .as_ref()
                        .map_or_else(|| winit_window.scale_factor(), |monitor| monitor.scale_factor());
                    let display_physical =
                        PhysicalSize::new(disp.width.max(0) as u32, disp.height.max(0) as u32);
                    info!(
                        "Positioning wall tile {} on spatial display {} (desktop [{},{} {}x{}], \
                         adapter {}).",
                        servoshell_preferences.wall_tile_index,
                        tile.display,
                        disp.left,
                        disp.top,
                        disp.width,
                        disp.height,
                        disp.adapter_index,
                    );
                    if let Some(monitor) = &matching_monitor
                        && requested_physical_size(scale) == display_physical
                    {
                        info!(
                            "Wall tile {} matches display {} size {:?}; using borderless \
                             fullscreen for flip-model present eligibility.",
                            servoshell_preferences.wall_tile_index, tile.display, display_physical,
                        );
                        winit_window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(
                            Some(monitor.clone()),
                        )));
                        wall_tile_fullscreen = true;
                    } else {
                        winit_window
                            .set_outer_position(PhysicalPosition::new(disp.left, disp.top));
                    }
                    wall_tile_monitor = matching_monitor;
                    wall_auto_gpu_index = Some(disp.adapter_index);
                },
                None => {
                    // Topology unavailable or display index out of range: fall back to the
                    // legacy winit-monitor-nth placement and let surfman pick the default
                    // adapter.
                    if let Some(target_monitor) =
                        winit_window.available_monitors().nth(tile.display)
                    {
                        let target_monitor_size = target_monitor.size();
                        info!(
                            "No DXGI topology for wall tile {}; falling back to winit monitor {} \
                             at {:?}.",
                            servoshell_preferences.wall_tile_index,
                            tile.display,
                            target_monitor.position(),
                        );
                        if requested_physical_size(target_monitor.scale_factor())
                            == target_monitor_size
                        {
                            winit_window.set_fullscreen(Some(
                                winit::window::Fullscreen::Borderless(Some(target_monitor.clone())),
                            ));
                            wall_tile_fullscreen = true;
                        } else {
                            winit_window.set_outer_position(target_monitor.position());
                        }
                        wall_tile_monitor = Some(target_monitor);
                    } else {
                        warn!(
                            "Wall tile {} requested display {}, but only {} monitor(s) are \
                             available and no DXGI topology was found.",
                            servoshell_preferences.wall_tile_index,
                            tile.display,
                            winit_window.available_monitors().count(),
                        );
                    }
                },
            }
        }
```

그리고 `requested_gpu_index` 계산부(`:239-243`)를:

```rust
        // The GPU is the adapter that drives the tile's spatial display (auto-assigned above);
        // `None` when topology was unavailable, letting surfman pick the default adapter.
        let requested_gpu_index = wall_auto_gpu_index;
```

주의: `PhysicalPosition`/`PhysicalSize`는 이미 import돼 있음(`headed_window.rs:33` `use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize};`) — 추가 import 불필요.

- [ ] **Step 5: present 로그 교체 (`:754-763`)**

```rust
        info!(
            "Wall window present: window {:?} tile={} display={} requested_gpu={:?} \
             present_ms={:.3} window_size={:?}",
            self.winit_window.id(),
            self.wall_tile_index,
            tile.display,
            self.window_rendering_context.requested_gpu_index(),
            present_duration.as_secs_f64() * 1000.0,
            self.window_rendering_context.size(),
        );
```

- [ ] **Step 6: app.rs 타일 plan 로그 교체 (`:207-215`)**

```rust
            info!(
                "Wall tile {} plan: display {} (auto-GPU), visible rect {:?}, render rect {:?}.",
                tile_index,
                tile.display,
                tile.rect,
                layout.tile_render_rect(*tile_index)
            );
```

- [ ] **Step 7: 컴파일 + 파서 테스트 GREEN**

Run: `cargo test -p servoshell wall_layout --lib`
Expected: 기존 3 + 신규 2 = 5 passed (`parses_valid_wall_layout`, `rejects_out_of_bounds_tile`, `calculates_overlap_render_rect_clamped_to_virtual_viewport`, `accepts_legacy_monitor_alias_and_ignores_gpu`, `rejects_tile_without_display_or_monitor`).
Run: `cargo check -p servoshell`
Expected: 성공 (monitor/gpu 참조 잔존 없음 — 있으면 컴파일 에러로 드러남).

- [ ] **Step 8: 잔존 monitor/gpu 참조 스캔**

Run: `powershell.exe -NoProfile -Command "Select-String -Path W:\servo_multigpu-tiled-wall\ports\servoshell\*.rs,W:\servo_multigpu-tiled-wall\ports\servoshell\desktop\*.rs -Pattern 'tile\.monitor|tile\.gpu|\.monitor,|\.gpu,'"`
Expected: 무출력(또는 wall_layout과 무관한 winit `current_monitor`/`available_monitors`만 — 그건 정상).

- [ ] **Step 9: Commit**

```bash
git add ports/servoshell/wall_layout.rs ports/servoshell/desktop/headed_window.rs ports/servoshell/desktop/app.rs
git commit -m "feat(servoshell): wall_layout을 display-only 공간배치+auto-GPU로 전환 (레거시 monitor/gpu 별칭·무시)"
```

---

### Task 3: 빌드 + 실기 스모크

**Files:** 산출물 `servoshell_wall_display_*.err.log` (검증 후 삭제 가능)

- [ ] **Step 1: 정적 검증**

```powershell
. W:\scripts\servo_env.ps1
$ErrorActionPreference='Continue'
cd W:\servo_multigpu-tiled-wall
rustfmt --edition 2024 --check components\shared\paint\rendering_context.rs components\servo\lib.rs ports\servoshell\wall_layout.rs ports\servoshell\desktop\headed_window.rs ports\servoshell\desktop\app.rs
git diff --check
cargo test -p servo-paint-api spatial_order
cargo test -p servoshell wall_layout --lib
```

Expected: rustfmt 무출력, diff 무출력, spatial_order 5 passed, wall_layout 5 passed.

- [ ] **Step 2: 빌드**

```powershell
cargo build -p servoshell
```

Expected: exit 0. (미디어 무관 변경이라 -p 빌드로 충분. lld-link 0xc0000005 시 Global Constraints 링커 오버라이드.)

- [ ] **Step 3: 스모크 — display 스키마 config로 wall-all-tiles**

이 브랜치에서 파싱 가능한 display 스키마 config가 있는지 확인하고(없으면 임시 생성), 개발기(단일 GPU·2모니터) 2타일로 실행:

```powershell
# display 스키마 임시 config (scratchpad; 미커밋)
$cfg = "W:\servo_multigpu-tiled-wall\.superpowers\sdd\wall_display_2x1.json"
@'
{ "virtualViewport": { "width": 3840, "height": 1080 },
  "tiles": [ { "display": 0, "rect": [0, 0, 1920, 1080] },
             { "display": 1, "rect": [1920, 0, 1920, 1080] } ] }
'@ | Set-Content -Encoding utf8 $cfg
$env:RUST_LOG="warn,paint=info,servoshell=info"
target\debug\servoshell.exe --wall-layout $cfg --wall-all-tiles tests\html\multigpu_wall_sync_probe.html 2> ..\servoshell_wall_display_smoke.err.log
```

(창을 닫아서 종료 — 강제 kill 시 로그 유실.) 로그 확인:

```powershell
Select-String -Path ..\servoshell_wall_display_smoke.err.log -Pattern "Positioning wall tile|spatial display|adapter|barrier|ready=|panic"
```

Expected:
- `Positioning wall tile 0 on spatial display 0 (desktop [0,0 ...], adapter N)` + tile 1 on display 1 — 좌측 물리 디스플레이가 spatial 0.
- 배리어 ready=2/2, panic 없음.
- (단일 GPU 머신이면 두 타일 adapter 동일, 그래도 배치·바인딩 정상.)

- [ ] **Step 4: 레거시 config 별칭 경로 확인**

```powershell
target\debug\servoshell.exe --wall-layout etc\multigpu\config\wall_layout.test_2x1_samegpu.json --wall-all-tiles tests\html\multigpu_wall_sync_probe.html 2> ..\servoshell_wall_legacy_smoke.err.log
Select-String -Path ..\servoshell_wall_legacy_smoke.err.log -Pattern "deprecated|'gpu' is ignored|spatial display|panic"
```

Expected: `'monitor' is deprecated` / `'gpu' is ignored` 경고 출력 후 정상 배치, panic 없음. (test_2x1_samegpu.json이 monitor+gpu 스키마임을 먼저 `Get-Content`으로 확인; display 스키마면 이 스텝은 다른 monitor+gpu config로 대체하거나 생략하고 그 사실을 보고.)

- [ ] **Step 5: 스펙 검증 결과 추기 + 커밋**

`docs/superpowers/specs/2026-07-24-servoshell-display-only-wall-layout-design.md` 말미에 검증 결과(정적/빌드/스모크 좌측=spatial0/레거시 경고) 5-8줄 한국어로 추기:

```bash
git add docs/superpowers/specs/2026-07-24-servoshell-display-only-wall-layout-design.md
git commit -m "docs: servoshell display-only wall_layout 검증 결과 기록"
```

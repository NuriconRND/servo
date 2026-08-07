# wall_view 스트리밍 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 월 표출 영역 전체를 캡처·합성·H.264 인코딩해 WebSocket으로 송출하고, 브라우저가 WebCodecs로 디코드해 보는 독립 프로세스 `wall_view`를 만든다.

**Architecture:** servo 워크스페이스와 분리된 standalone Rust 바이너리. wall_layout JSON + DXGI 토폴로지로 캡처 계획을 세우고, `d3d11screencapturesrc`(모니터별) → `d3d11compositor`(1080p 캔버스에 직접 배치·축소·NV12) → `amfh264enc`/`mfh264enc`(D3D11 메모리 그대로) → `h264parse`(avc) → `appsink` 파이프라인을 동적 구성한다. appsink에서 나온 AU를 클라이언트별 바운디드 큐로 팬아웃한다.

**Tech Stack:** Rust 2021, gstreamer-rs 0.25, tokio, tokio-tungstenite, windows(DXGI), serde/serde_json, WebCodecs(클라이언트)

## Global Constraints

- 스펙: `docs/superpowers/specs/2026-08-07-wall-view-streaming-design.md`
- 배치: `etc/multigpu/tools/wall_view/` — **servo 워크스페이스 멤버가 아니다.** `Cargo.toml`에 빈 `[workspace]` 테이블을 넣어 자체 워크스페이스 루트로 만든다(없으면 cargo가 servo 워크스페이스 소속으로 오인해 실패)
- 빌드/실행은 **servo 빌드와 무관**하다. `cargo build`를 이 디렉터리에서 직접 돌린다. servo용 `mach`/`servo_env.ps1`은 **필요 없다**
- 단, GStreamer 런타임과 pkg-config 경로는 필요하다:
  ```powershell
  $env:PKG_CONFIG_PATH = "F:/gstreamer-inhouse/1.28.4.100/1.0/msvc_x86_64/lib/pkgconfig"
  $env:PATH = "F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64\bin;$env:PATH"
  $env:GST_PLUGIN_PATH = "F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64\lib\gstreamer-1.0"
  ```
- 인코딩 파라미터(스펙에서 확정): `gop-size = fps`, `ref=1`, `cabac=0`, `rc-mode=0`, `low-latency=true`
- 바인드 기본값은 **`127.0.0.1:8787`** — 외부 노출은 명시적 `--bind`로만
- 출력 기본값 **1920x1080**, fps 기본 **30**, bitrate 기본 **8000**(kbps)
- 커밋 메시지에 큰따옴표(`"`)를 넣지 않는다. 각 커밋 끝에 다음 두 줄:
  ```
  Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01GHG3p4nt6sHudkycas4R8E
  ```
- **GUI 육안 판정은 사용자 몫** — 서브에이전트가 판정하지 않는다
- 런타임 검증 시 실행 프로세스를 남기지 않는다. 빌드는 포그라운드로 돌리고 `Finished`/`error`를 실제로 본 뒤 진행한다

## File Structure

| 파일 | 책임 |
|---|---|
| `Cargo.toml` | 패키지 + 자체 `[workspace]` 선언 |
| `src/main.rs` | CLI 파싱, 구성 요소 조립, 수명 관리 |
| `src/layout.rs` | wall_layout JSON 파싱 (읽기 전용) |
| `src/topology.rs` | DXGI 열거 + `spatial_order` 포팅 + HMONITOR 조회 |
| `src/plan.rs` | 레이아웃 + 토폴로지 + 출력크기 → 캡처 계획 (순수 함수) |
| `src/pipeline.rs` | GStreamer 파이프라인 동적 구성, 인코더 선택·caps 검증 |
| `src/proto.rs` | avcC → 코덱 문자열, 바이너리 프레이밍 |
| `src/server.rs` | WebSocket 서버, 클라이언트별 큐, 정렬 게이트, 인증 seam |
| `web/index.html` | WebCodecs 디코드 + canvas 렌더 |

---

### Task 1: 크레이트 뼈대 + 레이아웃 파싱

**Files:**
- Create: `etc/multigpu/tools/wall_view/Cargo.toml`
- Create: `etc/multigpu/tools/wall_view/src/main.rs`
- Create: `etc/multigpu/tools/wall_view/src/layout.rs`

**Interfaces:**
- Produces: `layout::WallLayout { virtual_viewport: Size, tiles: Vec<TileConfig>, overlap_px: u32 }`, `layout::TileConfig { display: usize, rect: Rect }`, `layout::Size { width: u32, height: u32 }`, `layout::Rect { x: i32, y: i32, width: i32, height: i32 }`, `WallLayout::from_path(&Path) -> anyhow::Result<WallLayout>`

- [ ] **Step 1: Cargo.toml을 만든다**

```toml
[package]
name = "wall_view"
version = "0.1.0"
edition = "2021"

# servo 워크스페이스 안에 있지만 멤버가 아니다. 이 빈 테이블이 없으면
# cargo가 상위 Cargo.toml의 워크스페이스에 속한다고 오인해 빌드가 실패한다.
[workspace]

[dependencies]
anyhow = "1.0"
clap = { version = "4", features = ["derive"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
```

- [ ] **Step 2: 실패하는 테스트를 쓴다**

`src/layout.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_2X1: &str = r#"{
      "virtualViewport": { "width": 3840, "height": 1080 },
      "tiles": [
        { "display": 0, "rect": [0, 0, 1920, 1080] },
        { "display": 1, "rect": [1920, 0, 1920, 1080] }
      ],
      "overlapPx": 32
    }"#;

    #[test]
    fn parses_2x1_display_layout() {
        let layout = WallLayout::from_json_str(EXAMPLE_2X1).expect("parse");
        assert_eq!(layout.virtual_viewport.width, 3840);
        assert_eq!(layout.virtual_viewport.height, 1080);
        assert_eq!(layout.overlap_px, 32);
        assert_eq!(layout.tiles.len(), 2);
        assert_eq!(layout.tiles[1].display, 1);
        assert_eq!(layout.tiles[1].rect.x, 1920);
        assert_eq!(layout.tiles[1].rect.width, 1920);
    }

    #[test]
    fn overlap_px_defaults_to_zero_when_absent() {
        let json = r#"{
          "virtualViewport": { "width": 1920, "height": 1080 },
          "tiles": [{ "display": 0, "rect": [0, 0, 1920, 1080] }]
        }"#;
        let layout = WallLayout::from_json_str(json).expect("parse");
        assert_eq!(layout.overlap_px, 0);
    }

    #[test]
    fn rejects_layout_without_tiles() {
        let json = r#"{ "virtualViewport": { "width": 1920, "height": 1080 }, "tiles": [] }"#;
        assert!(WallLayout::from_json_str(json).is_err());
    }
}
```

- [ ] **Step 3: 테스트가 실패하는지 확인한다**

```powershell
cd F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\servo_multigpu-tiled-wall\etc\multigpu\tools\wall_view
cargo test
```
기대: 컴파일 실패(`WallLayout` 미정의).

- [ ] **Step 4: 최소 구현을 쓴다**

`src/layout.rs` 상단:

```rust
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TileConfig {
    /// 공간 디스플레이 인덱스(좌상단=0, 행 우선). wall_layout의 `display`.
    pub display: usize,
    /// 가상 뷰포트 안에서 이 타일이 차지하는 영역. JSON은 [x, y, w, h] 배열.
    #[serde(deserialize_with = "deserialize_rect")]
    pub rect: Rect,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WallLayout {
    pub virtual_viewport: Size,
    pub tiles: Vec<TileConfig>,
    #[serde(default)]
    pub overlap_px: u32,
}

fn deserialize_rect<'de, D>(deserializer: D) -> Result<Rect, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = <[i32; 4]>::deserialize(deserializer)?;
    Ok(Rect {
        x: values[0],
        y: values[1],
        width: values[2],
        height: values[3],
    })
}

impl WallLayout {
    pub fn from_path(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read wall layout: {}", path.display()))?;
        Self::from_json_str(&text)
    }

    pub fn from_json_str(text: &str) -> Result<Self> {
        let layout: WallLayout = serde_json::from_str(text).context("invalid wall layout JSON")?;
        if layout.tiles.is_empty() {
            bail!("wall layout has no tiles");
        }
        if layout.virtual_viewport.width == 0 || layout.virtual_viewport.height == 0 {
            bail!("virtualViewport must be non-zero");
        }
        Ok(layout)
    }
}
```

`src/main.rs`:

```rust
mod layout;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "wall_view", about = "Stream the wall display area over WebSocket")]
struct Args {
    /// wall_layout JSON 경로
    #[arg(long)]
    layout: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let layout = layout::WallLayout::from_path(&args.layout)?;
    println!(
        "layout: {}x{}, {} tile(s), overlapPx={}",
        layout.virtual_viewport.width,
        layout.virtual_viewport.height,
        layout.tiles.len(),
        layout.overlap_px
    );
    for (index, tile) in layout.tiles.iter().enumerate() {
        println!(
            "  tile {index}: display {} rect [{}, {}, {}, {}]",
            tile.display, tile.rect.x, tile.rect.y, tile.rect.width, tile.rect.height
        );
    }
    Ok(())
}
```

- [ ] **Step 5: 테스트가 통과하는지 확인한다**

```powershell
cargo test
```
기대: `test result: ok. 3 passed`.

- [ ] **Step 6: 실기 레이아웃으로 동작을 확인한다**

```powershell
cargo run -- --layout ..\..\config\wall_layout.example_2x1_display.json
```
기대: 타일 2개가 display 0/1과 rect와 함께 출력.

- [ ] **Step 7: 커밋**

```bash
git add etc/multigpu/tools/wall_view
git commit -F - <<'EOF'
feat(wall_view): 크레이트 뼈대 + wall_layout 파싱

servo 워크스페이스와 분리된 standalone 바이너리로 시작한다. Cargo.toml의
빈 workspace 테이블이 servo 워크스페이스 소속 오인을 막는다.

레이아웃은 읽기 전용 소비자이므로 자체 serde 구조체로 파싱한다(스펙의
사용자 결정 사항 5 - 중복 감수).

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01GHG3p4nt6sHudkycas4R8E
EOF
```

---

### Task 2: DXGI 토폴로지 열거 + `spatial_order` 포팅

winit_wall과 **같은 공간 순서**를 내야 한다. 순서가 어긋나면 타일이 엉뚱한 모니터에 매핑되고, 화면은 나오므로 정상처럼 보인다. 원본(`components/shared/paint/rendering_context.rs:524-573`)은 **테스트가 없으므로**, 이 포팅이 그 알고리즘의 첫 검증이 된다.

**Files:**
- Create: `etc/multigpu/tools/wall_view/src/topology.rs`
- Modify: `etc/multigpu/tools/wall_view/Cargo.toml` (windows 크레이트 추가)
- Modify: `etc/multigpu/tools/wall_view/src/main.rs` (`--dump-topology`)

**Interfaces:**
- Produces: `topology::Display { index: usize, adapter_index: usize, device_name: String, left: i32, top: i32, width: i32, height: i32, attached_to_desktop: bool, monitor_handle: u64 }`, `topology::enumerate() -> Vec<Display>`, `topology::spatial_order(&[Display]) -> Vec<Display>`

- [ ] **Step 1: windows 크레이트를 추가한다**

`Cargo.toml`의 `[dependencies]`에:

```toml
windows = { version = "0.62.2", features = [
    "Win32_Foundation",
    "Win32_Graphics_Dxgi",
    "Win32_Graphics_Dxgi_Common",
    "Win32_Graphics_Gdi",
] }
```

- [ ] **Step 2: `spatial_order`의 실패하는 테스트를 쓴다**

`src/topology.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn display(index: usize, left: i32, top: i32, width: i32, height: i32) -> Display {
        Display {
            index,
            adapter_index: 0,
            device_name: format!("\\\\.\\DISPLAY{}", index + 1),
            left,
            top,
            width,
            height,
            attached_to_desktop: true,
            monitor_handle: 0,
        }
    }

    #[test]
    fn orders_a_horizontal_row_left_to_right() {
        // 열거 순서를 일부러 뒤집어 둔다.
        let input = vec![
            display(0, 1920, 0, 1920, 1080),
            display(1, 0, 0, 1920, 1080),
        ];
        let ordered = spatial_order(&input);
        assert_eq!(ordered[0].left, 0, "spatial 0은 가장 왼쪽이어야 한다");
        assert_eq!(ordered[1].left, 1920);
    }

    #[test]
    fn orders_rows_top_to_bottom_then_left_to_right() {
        let input = vec![
            display(0, 1920, 1080, 1920, 1080),
            display(1, 0, 1080, 1920, 1080),
            display(2, 1920, 0, 1920, 1080),
            display(3, 0, 0, 1920, 1080),
        ];
        let ordered = spatial_order(&input);
        let coords: Vec<(i32, i32)> = ordered.iter().map(|d| (d.left, d.top)).collect();
        assert_eq!(coords, vec![(0, 0), (1920, 0), (0, 1080), (1920, 1080)]);
    }

    #[test]
    fn groups_mixed_height_displays_into_one_row() {
        // 높이가 달라도 수직으로 충분히 겹치면 같은 행이다.
        let input = vec![
            display(0, 1920, 0, 1280, 720),
            display(1, 0, 0, 1920, 1080),
        ];
        let ordered = spatial_order(&input);
        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[0].left, 0);
        assert_eq!(ordered[1].left, 1920);
    }

    #[test]
    fn excludes_displays_not_attached_to_desktop() {
        let mut detached = display(1, 1920, 0, 1920, 1080);
        detached.attached_to_desktop = false;
        let input = vec![display(0, 0, 0, 1920, 1080), detached];
        let ordered = spatial_order(&input);
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].left, 0);
    }
}
```

- [ ] **Step 3: 테스트가 실패하는지 확인한다**

```powershell
cargo test
```
기대: 컴파일 실패(`Display`/`spatial_order` 미정의).

- [ ] **Step 4: 원본을 충실히 포팅한다**

`components/shared/paint/rendering_context.rs`의 `spatial_order`(524-573행)를 읽고 **알고리즘을 그대로** 옮긴다. 규칙은 다음과 같다:

1. `attached_to_desktop`인 것만 남긴다
2. 높이들의 **중앙값**을 구한다
3. `top` 기준으로 정렬한 뒤, 앞 디스플레이와의 **수직 겹침이 짧은 쪽 높이의 50% 이상**이면 같은 행으로 묶는다(중앙값 높이를 허용오차로 사용)
4. 각 행 안에서 `left` 오름차순
5. 행은 위에서 아래로

`src/topology.rs` 상단:

```rust
/// 공간 순서: 좌상단이 0, 행 안에서는 좌→우, 행은 위→아래.
///
/// **이 함수는 servo `components/shared/paint/rendering_context.rs`의 `spatial_order`를
/// 충실히 옮긴 것이다.** winit_wall과 같은 순서를 내야 타일이 올바른 모니터에 매핑된다.
/// 원본이 바뀌면 여기도 함께 바꿀 것. 원본에는 테스트가 없으므로 아래 테스트가
/// 이 알고리즘의 유일한 검증이다.
pub fn spatial_order(topology: &[Display]) -> Vec<Display> {
    let mut displays: Vec<Display> = topology
        .iter()
        .filter(|display| display.attached_to_desktop)
        .cloned()
        .collect();
    if displays.is_empty() {
        return displays;
    }

    let mut heights: Vec<i32> = displays.iter().map(|d| d.height.max(1)).collect();
    heights.sort_unstable();
    let median_height = heights[heights.len() / 2];

    displays.sort_by_key(|d| (d.top, d.left));

    let mut rows: Vec<Vec<Display>> = Vec::new();
    for display in displays {
        let placed = rows.last_mut().is_some_and(|row| {
            let reference = &row[0];
            let overlap = (reference.top + reference.height).min(display.top + display.height)
                - reference.top.max(display.top);
            let shorter = reference.height.min(display.height).max(1);
            overlap * 2 >= shorter || (display.top - reference.top).abs() * 2 < median_height
        });
        if placed {
            rows.last_mut().expect("row exists").push(display);
        } else {
            rows.push(vec![display]);
        }
    }

    let mut ordered = Vec::new();
    for row in rows.iter_mut() {
        row.sort_by_key(|d| d.left);
        ordered.extend(row.iter().cloned());
    }
    ordered
}
```

**구현자 주의**: 위 코드는 규칙을 옮긴 것이다. 원본 524-573행을 **직접 읽고** 조건식·허용오차 처리가 일치하는지 대조한 뒤, 다르면 원본을 따르고 테스트를 그에 맞게 조정하라. 원본이 정본이다.

- [ ] **Step 5: DXGI 열거를 구현한다**

`src/topology.rs`에 이어서. `topology_probe`(`etc/multigpu/tools/topology_probe/src/main.rs:180-234`)와 같은 패턴이다:

```rust
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL};

#[derive(Debug, Clone)]
pub struct Display {
    pub index: usize,
    pub adapter_index: usize,
    pub device_name: String,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub attached_to_desktop: bool,
    /// 이 디스플레이의 HMONITOR. d3d11screencapturesrc의 monitor-handle에 넣는다.
    pub monitor_handle: u64,
}

pub fn enumerate() -> Vec<Display> {
    let mut displays = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return displays;
        };
        let mut adapter_index = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
            let mut output_index = 0u32;
            while let Ok(output) = adapter.EnumOutputs(output_index) {
                if let Ok(desc) = output.GetDesc() {
                    let rect = desc.DesktopCoordinates;
                    // 데스크톱 좌표 중심점으로 HMONITOR를 얻는다. monitor-index는
                    // 열거 순서 가정이 필요하지만 핸들은 모호함이 없다.
                    let center = POINT {
                        x: (rect.left + rect.right) / 2,
                        y: (rect.top + rect.bottom) / 2,
                    };
                    let hmonitor = MonitorFromPoint(center, MONITOR_DEFAULTTONULL);
                    displays.push(Display {
                        index: displays.len(),
                        adapter_index: adapter_index as usize,
                        device_name: utf16_z_to_string(&desc.DeviceName),
                        left: rect.left,
                        top: rect.top,
                        width: rect.right - rect.left,
                        height: rect.bottom - rect.top,
                        attached_to_desktop: desc.AttachedToDesktop.as_bool(),
                        monitor_handle: hmonitor.0 as u64,
                    });
                }
                output_index += 1;
            }
            adapter_index += 1;
        }
    }
    displays
}

fn utf16_z_to_string(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}
```

- [ ] **Step 6: `--dump-topology`를 배선한다**

`src/main.rs`의 `Args`에 추가:

```rust
    /// 공간 순서로 정렬된 디스플레이 토폴로지를 출력하고 종료한다
    #[arg(long)]
    dump_topology: bool,
```

`main()`의 레이아웃 출력 앞에:

```rust
    if args.dump_topology {
        let displays = topology::spatial_order(&topology::enumerate());
        println!("spatially ordered displays ({}):", displays.len());
        for (spatial_index, display) in displays.iter().enumerate() {
            println!(
                "  spatial {spatial_index}: {} rect[{},{} {}x{}] adapter {} hmonitor 0x{:x}",
                display.device_name,
                display.left,
                display.top,
                display.width,
                display.height,
                display.adapter_index,
                display.monitor_handle
            );
        }
        return Ok(());
    }
```

`mod topology;`를 `mod layout;` 옆에 추가한다.

- [ ] **Step 7: 테스트와 실기 확인**

```powershell
cargo test
cargo run -- --layout ..\..\config\wall_layout.example_2x1_display.json --dump-topology
```
기대: 테스트 7개(1번 3개 + 이번 4개) 통과. 실기 출력에서 **가장 왼쪽 디스플레이가 spatial 0**이고 hmonitor가 0이 아니어야 한다.

- [ ] **Step 8: 커밋**

```bash
git add etc/multigpu/tools/wall_view
git commit -F - <<'EOF'
feat(wall_view): DXGI 토폴로지 열거 + spatial_order 포팅

winit_wall과 같은 공간 순서를 내야 타일이 올바른 모니터에 매핑된다. 순서가
어긋나면 화면은 나오므로 정상처럼 보이는 종류의 버그다.

원본(components/shared/paint/rendering_context.rs)에는 이 알고리즘의 테스트가
없어, 이 포팅에 붙인 4개가 첫 검증이다. 행 그룹핑(수직 겹침 50퍼센트, 중앙값
높이 허용오차)과 혼합 해상도 케이스를 고정한다.

모니터 지정은 monitor-index 대신 HMONITOR를 쓴다 - 열거 순서 가정이 필요없다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01GHG3p4nt6sHudkycas4R8E
EOF
```

---

### Task 3: 캡처 계획 (순수 함수)

**Files:**
- Create: `etc/multigpu/tools/wall_view/src/plan.rs`
- Modify: `etc/multigpu/tools/wall_view/src/main.rs` (`--dry-run`)

**Interfaces:**
- Consumes: `layout::WallLayout`(Task 1), `topology::Display`(Task 2)
- Produces: `plan::CaptureSlot { monitor_handle: u64, device_name: String, xpos: i32, ypos: i32, width: i32, height: i32 }`, `plan::plan_capture(&WallLayout, &[Display], output: Size) -> anyhow::Result<Vec<CaptureSlot>>`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`src/plan.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Rect, Size, TileConfig, WallLayout};
    use crate::topology::Display;

    fn display(index: usize, left: i32, top: i32, width: i32, height: i32) -> Display {
        Display {
            index,
            adapter_index: 0,
            device_name: format!("\\\\.\\DISPLAY{}", index + 1),
            left,
            top,
            width,
            height,
            attached_to_desktop: true,
            monitor_handle: 0x1000 + index as u64,
        }
    }

    fn layout_2x1() -> WallLayout {
        WallLayout {
            virtual_viewport: Size { width: 3840, height: 1080 },
            tiles: vec![
                TileConfig { display: 0, rect: Rect { x: 0, y: 0, width: 1920, height: 1080 } },
                TileConfig { display: 1, rect: Rect { x: 1920, y: 0, width: 1920, height: 1080 } },
            ],
            overlap_px: 32,
        }
    }

    #[test]
    fn scales_2x1_wall_into_1080p_canvas() {
        let displays = vec![display(0, 0, 0, 1920, 1080), display(1, 1920, 0, 1920, 1080)];
        let slots = plan_capture(&layout_2x1(), &displays, Size { width: 1920, height: 1080 })
            .expect("plan");
        assert_eq!(slots.len(), 2);
        // 3840 -> 1920 이므로 가로 1/2, 세로도 같은 계수로 1080 -> 540.
        assert_eq!((slots[0].xpos, slots[0].ypos), (0, 0));
        assert_eq!((slots[0].width, slots[0].height), (960, 540));
        assert_eq!((slots[1].xpos, slots[1].ypos), (960, 0));
        assert_eq!((slots[1].width, slots[1].height), (960, 540));
    }

    #[test]
    fn maps_each_tile_to_its_spatial_display_handle() {
        let displays = vec![display(0, 0, 0, 1920, 1080), display(1, 1920, 0, 1920, 1080)];
        let slots = plan_capture(&layout_2x1(), &displays, Size { width: 1920, height: 1080 })
            .expect("plan");
        assert_eq!(slots[0].monitor_handle, 0x1000);
        assert_eq!(slots[1].monitor_handle, 0x1001);
    }

    #[test]
    fn scales_2x2_wall_into_1080p_canvas() {
        let layout = WallLayout {
            virtual_viewport: Size { width: 3840, height: 2160 },
            tiles: vec![
                TileConfig { display: 0, rect: Rect { x: 0, y: 0, width: 1920, height: 1080 } },
                TileConfig { display: 1, rect: Rect { x: 1920, y: 0, width: 1920, height: 1080 } },
                TileConfig { display: 2, rect: Rect { x: 0, y: 1080, width: 1920, height: 1080 } },
                TileConfig { display: 3, rect: Rect { x: 1920, y: 1080, width: 1920, height: 1080 } },
            ],
            overlap_px: 0,
        };
        let displays = vec![
            display(0, 0, 0, 1920, 1080),
            display(1, 1920, 0, 1920, 1080),
            display(2, 0, 1080, 1920, 1080),
            display(3, 1920, 1080, 1920, 1080),
        ];
        let slots = plan_capture(&layout, &displays, Size { width: 1920, height: 1080 })
            .expect("plan");
        assert_eq!((slots[3].xpos, slots[3].ypos), (960, 540));
        assert_eq!((slots[3].width, slots[3].height), (960, 540));
    }

    #[test]
    fn errors_when_a_tile_references_a_missing_display() {
        let displays = vec![display(0, 0, 0, 1920, 1080)];
        let error = plan_capture(&layout_2x1(), &displays, Size { width: 1920, height: 1080 })
            .expect_err("display 1 이 없으므로 실패해야 한다");
        let message = error.to_string();
        assert!(message.contains("display 1"), "메시지에 없는 인덱스가 있어야 한다: {message}");
    }

    #[test]
    fn ignores_overlap_px_because_capture_is_desktop_side() {
        // overlapPx는 winit_wall의 렌더 확장용이다. 데스크톱 캡처는 화면에 보이는
        // 것을 그대로 찍으므로 가드밴드와 무관하다.
        let mut layout = layout_2x1();
        layout.overlap_px = 512;
        let displays = vec![display(0, 0, 0, 1920, 1080), display(1, 1920, 0, 1920, 1080)];
        let slots = plan_capture(&layout, &displays, Size { width: 1920, height: 1080 })
            .expect("plan");
        assert_eq!((slots[0].width, slots[0].height), (960, 540));
    }
}
```

- [ ] **Step 2: 테스트가 실패하는지 확인한다**

```powershell
cargo test
```
기대: 컴파일 실패(`plan_capture` 미정의).

- [ ] **Step 3: 구현한다**

`src/plan.rs` 상단:

```rust
use anyhow::{bail, Result};

use crate::layout::{Size, WallLayout};
use crate::topology::Display;

/// 캡처 소스 하나와, 그 결과를 출력 캔버스 어디에 놓을지.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSlot {
    /// d3d11screencapturesrc의 monitor-handle에 넣을 HMONITOR.
    pub monitor_handle: u64,
    /// 진단 로그용.
    pub device_name: String,
    /// d3d11compositor 싱크 패드 좌표(출력 캔버스 기준).
    pub xpos: i32,
    pub ypos: i32,
    pub width: i32,
    pub height: i32,
}

/// 레이아웃 + 공간 정렬된 디스플레이 + 출력 크기 → 캡처 계획.
///
/// 가상 뷰포트를 중간에 만들지 않는다. 각 타일의 가상 좌표에 스케일 계수를 곱해
/// 출력 캔버스 상의 좌표를 바로 낸다.
///
/// `overlapPx`는 여기서 쓰지 않는다 — 그것은 winit_wall이 이음매 아티팩트를 흡수하려고
/// 렌더 영역을 넓히는 값이고, 데스크톱 캡처는 화면에 보이는 결과만 찍는다.
pub fn plan_capture(
    layout: &WallLayout,
    displays: &[Display],
    output: Size,
) -> Result<Vec<CaptureSlot>> {
    let scale_x = output.width as f64 / layout.virtual_viewport.width as f64;
    let scale_y = output.height as f64 / layout.virtual_viewport.height as f64;

    let mut slots = Vec::with_capacity(layout.tiles.len());
    for tile in &layout.tiles {
        let Some(display) = displays.get(tile.display) else {
            bail!(
                "wall layout references display {} but only {} desktop display(s) were found",
                tile.display,
                displays.len()
            );
        };
        slots.push(CaptureSlot {
            monitor_handle: display.monitor_handle,
            device_name: display.device_name.clone(),
            xpos: (tile.rect.x as f64 * scale_x).round() as i32,
            ypos: (tile.rect.y as f64 * scale_y).round() as i32,
            width: (tile.rect.width as f64 * scale_x).round() as i32,
            height: (tile.rect.height as f64 * scale_y).round() as i32,
        });
    }
    Ok(slots)
}
```

- [ ] **Step 4: 테스트가 통과하는지 확인한다**

```powershell
cargo test
```
기대: 12개 통과(1번 3 + 2번 4 + 이번 5).

- [ ] **Step 5: `--dry-run`을 배선한다**

`Args`에 추가:

```rust
    /// 캡처 계획만 출력하고 종료한다(파이프라인을 만들지 않는다)
    #[arg(long)]
    dry_run: bool,

    /// 출력 해상도, WxH
    #[arg(long, default_value = "1920x1080")]
    output: String,
```

출력 문자열 파싱과 dry-run 출력을 `main()`에 추가한다:

```rust
fn parse_size(text: &str) -> Result<layout::Size> {
    let (width, height) = text
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("--output must look like 1920x1080"))?;
    Ok(layout::Size {
        width: width.trim().parse()?,
        height: height.trim().parse()?,
    })
}
```

```rust
    let output = parse_size(&args.output)?;
    let displays = topology::spatial_order(&topology::enumerate());
    let slots = plan::plan_capture(&layout, &displays, output)?;
    if args.dry_run {
        println!("capture plan ({}x{} canvas):", output.width, output.height);
        for (index, slot) in slots.iter().enumerate() {
            println!(
                "  slot {index}: {} hmonitor 0x{:x} -> xpos {} ypos {} {}x{}",
                slot.device_name, slot.monitor_handle, slot.xpos, slot.ypos, slot.width, slot.height
            );
        }
        return Ok(());
    }
```

`mod plan;`을 추가한다.

- [ ] **Step 6: 실기 확인**

```powershell
cargo run -- --layout ..\..\config\wall_layout.example_2x1_display.json --dry-run
```
기대: 슬롯 2개, 각각 960x540, xpos 0과 960.

- [ ] **Step 7: 커밋**

```bash
git add etc/multigpu/tools/wall_view
git commit -F - <<'EOF'
feat(wall_view): 캡처 계획 순수 함수 + dry-run

레이아웃과 공간 정렬된 디스플레이를 받아 각 캡처 소스를 출력 캔버스의 어느
자리에 놓을지 계산한다. 가상 뷰포트를 중간에 만들지 않고 스케일 계수를 바로
곱한다.

overlapPx는 쓰지 않는다 - winit_wall이 이음매 아티팩트를 흡수하려고 렌더
영역을 넓히는 값이고, 데스크톱 캡처는 화면에 보이는 결과만 찍는다. 테스트로
고정했다.

이 파이프라인에서 자동 검증이 가능한 핵심부라 2x1/2x2/누락 디스플레이/
스케일 반올림을 전부 테스트한다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01GHG3p4nt6sHudkycas4R8E
EOF
```

---

### Task 4: 프로토콜 — avcC → 코덱 문자열, 바이너리 프레이밍

**Files:**
- Create: `etc/multigpu/tools/wall_view/src/proto.rs`

**Interfaces:**
- Produces: `proto::codec_string_from_avcc(&[u8]) -> anyhow::Result<String>`, `proto::FrameHeader { keyframe: bool, timestamp_us: i64 }`, `proto::encode_frame(&FrameHeader, &[u8]) -> Vec<u8>`, `proto::decode_frame(&[u8]) -> anyhow::Result<(FrameHeader, &[u8])>`, `proto::InitMessage { codec: String, description: String, coded_width: u32, coded_height: u32, framerate: u32 }`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`src/proto.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_codec_string_from_avcc_bytes() {
        // avcC: [0]=configurationVersion, [1]=AVCProfileIndication,
        //       [2]=profile_compatibility, [3]=AVCLevelIndication
        let avcc = [0x01u8, 0x42, 0xE0, 0x1F, 0xFF, 0xE1];
        assert_eq!(codec_string_from_avcc(&avcc).unwrap(), "avc1.42E01F");
    }

    #[test]
    fn derives_codec_string_for_high_profile_level_4() {
        let avcc = [0x01u8, 0x64, 0x00, 0x28];
        assert_eq!(codec_string_from_avcc(&avcc).unwrap(), "avc1.640028");
    }

    #[test]
    fn rejects_too_short_avcc() {
        assert!(codec_string_from_avcc(&[0x01, 0x42]).is_err());
    }

    #[test]
    fn frame_header_round_trips() {
        let header = FrameHeader { keyframe: true, timestamp_us: 1_234_567 };
        let payload = [0xAAu8, 0xBB, 0xCC];
        let encoded = encode_frame(&header, &payload);
        assert_eq!(encoded.len(), HEADER_LEN + payload.len());
        let (decoded, body) = decode_frame(&encoded).expect("decode");
        assert_eq!(decoded.keyframe, true);
        assert_eq!(decoded.timestamp_us, 1_234_567);
        assert_eq!(body, &payload);
    }

    #[test]
    fn frame_header_round_trips_delta() {
        let header = FrameHeader { keyframe: false, timestamp_us: -1 };
        let encoded = encode_frame(&header, &[]);
        let (decoded, body) = decode_frame(&encoded).expect("decode");
        assert_eq!(decoded.keyframe, false);
        assert_eq!(decoded.timestamp_us, -1);
        assert!(body.is_empty());
    }

    #[test]
    fn rejects_truncated_frame() {
        assert!(decode_frame(&[0u8; 4]).is_err());
    }
}
```

- [ ] **Step 2: 테스트가 실패하는지 확인한다**

```powershell
cargo test
```
기대: 컴파일 실패.

- [ ] **Step 3: 구현한다**

`src/proto.rs` 상단:

```rust
use anyhow::{bail, Result};
use serde::Serialize;

/// 바이너리 미디어 프레임 헤더 길이(바이트).
pub const HEADER_LEN: usize = 16;

/// 미디어 메시지 타입. 지금은 비디오 AU 하나뿐이다.
const MSG_TYPE_VIDEO_AU: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub keyframe: bool,
    pub timestamp_us: i64,
}

/// 접속 직후 보내는 JSON 제어 메시지.
#[derive(Debug, Clone, Serialize)]
pub struct InitMessage {
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub codec: String,
    /// base64로 인코딩한 avcC 박스. WebCodecs의 description으로 그대로 들어간다.
    pub description: String,
    pub coded_width: u32,
    pub coded_height: u32,
    pub framerate: u32,
}

/// avcC 박스 앞부분에서 WebCodecs 코덱 문자열을 만든다.
///
/// avcC: [0] configurationVersion, [1] AVCProfileIndication,
///       [2] profile_compatibility, [3] AVCLevelIndication
/// 결과: `avc1.` + 위 [1][2][3]의 대문자 hex 6자리
pub fn codec_string_from_avcc(avcc: &[u8]) -> Result<String> {
    if avcc.len() < 4 {
        bail!("avcC is too short ({} bytes, need at least 4)", avcc.len());
    }
    Ok(format!("avc1.{:02X}{:02X}{:02X}", avcc[1], avcc[2], avcc[3]))
}

/// 헤더(16바이트, 리틀엔디언) + AVCC 페이로드.
pub fn encode_frame(header: &FrameHeader, payload: &[u8]) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(HEADER_LEN + payload.len());
    buffer.push(MSG_TYPE_VIDEO_AU);
    buffer.push(if header.keyframe { 1 } else { 0 });
    buffer.extend_from_slice(&0u16.to_le_bytes()); // reserved
    buffer.extend_from_slice(&header.timestamp_us.to_le_bytes());
    buffer.extend_from_slice(&0u32.to_le_bytes()); // reserved
    debug_assert_eq!(buffer.len(), HEADER_LEN);
    buffer.extend_from_slice(payload);
    buffer
}

pub fn decode_frame(bytes: &[u8]) -> Result<(FrameHeader, &[u8])> {
    if bytes.len() < HEADER_LEN {
        bail!("frame is shorter than the {HEADER_LEN}-byte header");
    }
    if bytes[0] != MSG_TYPE_VIDEO_AU {
        bail!("unknown message type {}", bytes[0]);
    }
    let timestamp_us = i64::from_le_bytes(bytes[4..12].try_into().expect("8 bytes"));
    Ok((
        FrameHeader { keyframe: bytes[1] & 1 == 1, timestamp_us },
        &bytes[HEADER_LEN..],
    ))
}
```

- [ ] **Step 4: 테스트가 통과하는지 확인한다**

```powershell
cargo test
```
기대: 18개 통과(이전 12 + 이번 6).

- [ ] **Step 5: 커밋**

```bash
git add etc/multigpu/tools/wall_view
git commit -F - <<'EOF'
feat(wall_view): WebCodecs 프로토콜 - 코덱 문자열 유도 + 바이너리 프레이밍

AVCC 패키징을 쓴다. 파라미터 세트가 접속 시 description으로 전달되므로 늦은
참여자는 다음 IDR 하나만 기다리면 된다.

코덱 문자열은 avcC의 profile/compat/level 3바이트에서 유도한다. 프레이밍은
16바이트 리틀엔디언 헤더 + AVCC 페이로드. 둘 다 순수 함수라 테스트로 고정한다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01GHG3p4nt6sHudkycas4R8E
EOF
```

---

### Task 5: GStreamer 파이프라인 + 인코더 선택

**Files:**
- Create: `etc/multigpu/tools/wall_view/src/pipeline.rs`
- Modify: `etc/multigpu/tools/wall_view/Cargo.toml` (gstreamer 의존성)
- Modify: `etc/multigpu/tools/wall_view/src/main.rs` (파이프라인 스모크 경로)

**Interfaces:**
- Consumes: `plan::CaptureSlot`(Task 3), `layout::Size`(Task 1)
- Produces: `pipeline::PipelineConfig { slots: Vec<CaptureSlot>, output: Size, fps: u32, bitrate_kbps: u32, encoder: EncoderChoice, capture_api: String, show_cursor: bool }`, `pipeline::EncoderChoice { Auto, Amf, MediaFoundation, Software }`, `pipeline::WallPipeline::new(PipelineConfig, on_sample) -> Result<WallPipeline>`, `WallPipeline::play()`, `WallPipeline::stop()`, `WallPipeline::force_keyframe()`, `pipeline::select_encoder(EncoderChoice, output: Size) -> Result<EncoderSpec>`, `pipeline::EncoderSpec { element: &'static str, max_width: i32, max_height: i32, takes_d3d11: bool }`

`on_sample`은 `Fn(&FrameHeader, &[u8], Option<&[u8]>)` 형태다 — 세 번째 인자는 caps가 바뀌었을 때만 `Some(avcC)`다.

- [ ] **Step 1: gstreamer 의존성을 추가한다**

`Cargo.toml`의 `[dependencies]`에:

```toml
gstreamer = "0.25"
gstreamer-app = "0.25"
gstreamer-video = "0.25"
glib = "0.21"
```

`src/pipeline.rs` 상단에 별칭을 둔다:

```rust
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
```

- [ ] **Step 2: 인코더 선택의 실패하는 테스트를 쓴다**

`src/pipeline.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Size;

    #[test]
    fn rejects_output_larger_than_encoder_limit() {
        let spec = EncoderSpec {
            element: "amfh264enc",
            max_width: 1920,
            max_height: 1920,
            takes_d3d11: true,
        };
        let error = check_encoder_limits(&spec, Size { width: 3840, height: 1080 })
            .expect_err("3840 은 1920 한계를 넘는다");
        let message = error.to_string();
        assert!(message.contains("amfh264enc"), "메시지에 엘리먼트 이름: {message}");
        assert!(message.contains("1920"), "메시지에 한계값: {message}");
    }

    #[test]
    fn accepts_output_within_encoder_limit() {
        let spec = EncoderSpec {
            element: "amfh264enc",
            max_width: 1920,
            max_height: 1920,
            takes_d3d11: true,
        };
        assert!(check_encoder_limits(&spec, Size { width: 1920, height: 1080 }).is_ok());
    }
}
```

- [ ] **Step 3: 테스트가 실패하는지 확인한다**

```powershell
cargo test
```
기대: 컴파일 실패(`EncoderSpec`/`check_encoder_limits` 미정의).

- [ ] **Step 4: 인코더 선택과 한계 검증을 구현한다**

`src/pipeline.rs` 상단:

```rust
use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gst::prelude::*;

use crate::layout::Size;
use crate::plan::CaptureSlot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderChoice {
    Auto,
    Amf,
    MediaFoundation,
    Software,
}

#[derive(Debug, Clone)]
pub struct EncoderSpec {
    pub element: &'static str,
    pub max_width: i32,
    pub max_height: i32,
    /// false면 D3D11 메모리를 받지 못해 CPU 다운로드가 한 번 들어간다.
    pub takes_d3d11: bool,
}

/// 출력 크기가 인코더 한계를 넘으면 구성 시점에 실패시킨다.
///
/// 이 머신에서 amfh264enc가 [64, 1920]으로 열거되는 것을 실측했다. 런타임에
/// 이상하게 죽는 것보다 여기서 명확히 실패하는 편이 낫다.
pub fn check_encoder_limits(spec: &EncoderSpec, output: Size) -> Result<()> {
    if output.width as i32 > spec.max_width || output.height as i32 > spec.max_height {
        bail!(
            "{} supports at most {}x{} but --output is {}x{}; reduce --output or pick another --encoder",
            spec.element,
            spec.max_width,
            spec.max_height,
            output.width,
            output.height
        );
    }
    Ok(())
}
```

인코더 후보 조회는 GStreamer 레지스트리에서 caps를 읽어 채운다:

```rust
/// 등록된 엘리먼트의 SINK caps에서 최대 width/height와 D3D11 메모리 수용 여부를 읽는다.
fn probe_encoder(element: &'static str) -> Option<EncoderSpec> {
    let factory = gst::ElementFactory::find(element)?;
    let mut max_width = 0;
    let mut max_height = 0;
    let mut takes_d3d11 = false;
    for pad_template in factory.static_pad_templates() {
        if pad_template.direction() != gst::PadDirection::Sink {
            continue;
        }
        let caps = pad_template.caps();
        for index in 0..caps.size() {
            let structure = caps.structure(index).expect("structure in range");
            if caps
                .features(index)
                .is_some_and(|features| features.contains("memory:D3D11Memory"))
            {
                takes_d3d11 = true;
            }
            if let Ok(range) = structure.value("width") {
                if let Ok(int_range) = range.get::<gst::IntRange<i32>>() {
                    max_width = max_width.max(int_range.max());
                }
            }
            if let Ok(range) = structure.value("height") {
                if let Ok(int_range) = range.get::<gst::IntRange<i32>>() {
                    max_height = max_height.max(int_range.max());
                }
            }
        }
    }
    if max_width == 0 || max_height == 0 {
        return None;
    }
    Some(EncoderSpec { element, max_width, max_height, takes_d3d11 })
}

pub fn select_encoder(choice: EncoderChoice, output: Size) -> Result<EncoderSpec> {
    let candidates: &[&'static str] = match choice {
        EncoderChoice::Auto => &["amfh264enc", "mfh264enc", "openh264enc"],
        EncoderChoice::Amf => &["amfh264enc"],
        EncoderChoice::MediaFoundation => &["mfh264enc"],
        EncoderChoice::Software => &["openh264enc"],
    };
    let mut last_error = None;
    for element in candidates {
        let Some(spec) = probe_encoder(element) else { continue };
        match check_encoder_limits(&spec, output) {
            Ok(()) => {
                if !spec.takes_d3d11 {
                    eprintln!(
                        "warning: {} does not accept D3D11 memory; a CPU download will be inserted \
                         and the zero-copy path is lost",
                        spec.element
                    );
                }
                return Ok(spec);
            },
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        Some(error) => Err(error),
        None => bail!("no usable H.264 encoder found; is the in-house GStreamer on GST_PLUGIN_PATH?"),
    }
}
```

- [ ] **Step 5: 테스트가 통과하는지 확인한다**

```powershell
$env:PKG_CONFIG_PATH = "F:/gstreamer-inhouse/1.28.4.100/1.0/msvc_x86_64/lib/pkgconfig"
$env:PATH = "F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64\bin;$env:PATH"
cargo test
```
기대: 20개 통과.

- [ ] **Step 6: 파이프라인을 구성한다**

같은 파일에 이어서. `d3d11compositor`의 싱크 패드는 요청 패드이므로 슬롯마다 `request_pad_simple("sink_%u")`로 얻고 좌표를 설정한다:

```rust
pub struct PipelineConfig {
    pub slots: Vec<CaptureSlot>,
    pub output: Size,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub encoder: EncoderChoice,
    pub capture_api: String,
    pub show_cursor: bool,
}

pub struct WallPipeline {
    pipeline: gst::Pipeline,
    encoder: gst::Element,
}

impl WallPipeline {
    pub fn new<F>(config: PipelineConfig, on_sample: F) -> Result<Self>
    where
        F: Fn(&crate::proto::FrameHeader, &[u8], Option<&[u8]>) + Send + Sync + 'static,
    {
        gst::init().context("gst::init")?;
        let spec = select_encoder(config.encoder, config.output)?;
        eprintln!(
            "wall_view: encoder {} (max {}x{}, d3d11={})",
            spec.element, spec.max_width, spec.max_height, spec.takes_d3d11
        );

        let pipeline = gst::Pipeline::new();
        let compositor = gst::ElementFactory::make("d3d11compositor").build()?;
        let encoder = gst::ElementFactory::make(spec.element)
            .property("bitrate", config.bitrate_kbps)
            .build()?;
        // 저지연 튜닝(참고 솔루션에서 이식). 엘리먼트마다 없는 속성이 있으므로
        // 존재할 때만 설정한다.
        for (name, value) in [
            ("gop-size", (config.fps as i32).into()),
            ("ref", 1i32.into()),
        ] as [(&str, glib::Value); 2]
        {
            if encoder.has_property(name) {
                encoder.set_property_from_value(name, &value);
            }
        }
        if encoder.has_property("low-latency") {
            encoder.set_property("low-latency", true);
        }
        if encoder.has_property("cabac") {
            encoder.set_property("cabac", false);
        }

        let parser = gst::ElementFactory::make("h264parse")
            .property("config-interval", -1i32)
            .build()?;
        let appsink = gst_app::AppSink::builder()
            .caps(
                &gst::Caps::builder("video/x-h264")
                    .field("stream-format", "avc")
                    .field("alignment", "au")
                    .build(),
            )
            .sync(false)
            .max_buffers(4u32)
            .drop(true)
            .build();

        pipeline.add_many([&compositor, &encoder, &parser, appsink.upcast_ref()])?;

        let caps = gst::Caps::builder("video/x-raw")
            .features(["memory:D3D11Memory"])
            .field("format", "NV12")
            .field("width", config.output.width as i32)
            .field("height", config.output.height as i32)
            .field("framerate", gst::Fraction::new(config.fps as i32, 1))
            .build();
        compositor.link_filtered(&encoder, &caps)?;
        gst::Element::link_many([&encoder, &parser, appsink.upcast_ref()])?;

        for slot in &config.slots {
            let mut builder = gst::ElementFactory::make("d3d11screencapturesrc")
                .property("monitor-handle", slot.monitor_handle)
                .property("show-cursor", config.show_cursor);
            // capture-api 는 enum 속성이라 문자열로 넘긴다(dxgi | wgc).
            builder = builder.property_from_str("capture-api", &config.capture_api);
            let source = builder.build()?;
            pipeline.add(&source)?;
            let pad = compositor
                .request_pad_simple("sink_%u")
                .context("d3d11compositor sink pad")?;
            pad.set_property("xpos", slot.xpos);
            pad.set_property("ypos", slot.ypos);
            pad.set_property("width", slot.width);
            pad.set_property("height", slot.height);
            source
                .static_pad("src")
                .context("source src pad")?
                .link(&pad)?;
        }

        // appsink 콜백은 절대 블록하지 않는다. 여기서 막히면 파이프라인이 밀리고
        // 캡처와 인코딩까지 밀린다.
        let mut last_avcc: Option<Vec<u8>> = None;
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                    let timestamp_us = buffer
                        .pts()
                        .map(|pts| (pts.nseconds() / 1000) as i64)
                        .unwrap_or(-1);

                    let mut new_avcc = None;
                    if let Some(caps) = sample.caps() {
                        if let Ok(structure) = caps.structure(0).ok_or(gst::FlowError::Error) {
                            if let Ok(codec_data) = structure.get::<gst::Buffer>("codec_data") {
                                if let Ok(codec_map) = codec_data.map_readable() {
                                    let bytes = codec_map.as_slice().to_vec();
                                    if last_avcc.as_deref() != Some(bytes.as_slice()) {
                                        last_avcc = Some(bytes.clone());
                                        new_avcc = Some(bytes);
                                    }
                                }
                            }
                        }
                    }

                    on_sample(
                        &crate::proto::FrameHeader { keyframe, timestamp_us },
                        map.as_slice(),
                        new_avcc.as_deref(),
                    );
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        Ok(Self { pipeline, encoder })
    }

    pub fn play(&self) -> Result<()> {
        self.pipeline.set_state(gst::State::Playing)?;
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        self.pipeline.set_state(gst::State::Null)?;
        Ok(())
    }

    /// 새 클라이언트가 붙었을 때 즉시 IDR을 만들도록 요청한다.
    pub fn force_keyframe(&self) {
        let event = gst_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        let _ = self.encoder.send_event(event);
    }
}
```

`Cargo.toml`에 `gstreamer-video = "0.25"`와 `glib = "0.21"`을 추가하고 `use gstreamer_app as gst_app; use gstreamer_video as gst_video;`를 파일 상단에 둔다.

- [ ] **Step 7: 스모크 — 클라이언트 없이 파이프라인만 돌린다**

`Args`에 추가:

```rust
    /// 파이프라인을 N초 돌려 AU 통계만 출력하고 종료한다(WebSocket 없이)
    #[arg(long)]
    smoke_sec: Option<u64>,
```

`main()`에서 `--smoke-sec`가 있으면 파이프라인을 만들고 AU 수·keyframe 수를 세어 출력한다:

```rust
    if let Some(seconds) = args.smoke_sec {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        let total = Arc::new(AtomicU64::new(0));
        let keys = Arc::new(AtomicU64::new(0));
        let (total_cb, keys_cb) = (total.clone(), keys.clone());
        let pipeline = pipeline::WallPipeline::new(
            pipeline::PipelineConfig {
                slots,
                output,
                fps: args.fps,
                bitrate_kbps: args.bitrate,
                encoder: pipeline::EncoderChoice::Auto,
                capture_api: args.capture_api.clone(),
                show_cursor: args.show_cursor,
            },
            move |header, _payload, _avcc| {
                total_cb.fetch_add(1, Ordering::Relaxed);
                if header.keyframe {
                    keys_cb.fetch_add(1, Ordering::Relaxed);
                }
            },
        )?;
        pipeline.play()?;
        std::thread::sleep(std::time::Duration::from_secs(seconds));
        pipeline.stop()?;
        println!(
            "smoke: {} AU, {} keyframes in {}s",
            total.load(Ordering::Relaxed),
            keys.load(Ordering::Relaxed),
            seconds
        );
        return Ok(());
    }
```

`Args`에 `--fps`(기본 30), `--bitrate`(기본 8000), `--capture-api`(기본 `dxgi`), `--show-cursor` 플래그도 함께 추가한다.

- [ ] **Step 8: 스모크를 돌린다**

```powershell
$env:GST_PLUGIN_PATH = "F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64\lib\gstreamer-1.0"
cargo run --release -- --layout ..\..\config\wall_layout.example_2x1_display.json --smoke-sec 5
```
기대: `smoke: N AU, K keyframes in 5s`. fps 30이면 N은 대략 140~155, K는 대략 5(GOP 1초). 인코더 선택 로그(`wall_view: encoder ...`)도 나와야 한다.

- [ ] **Step 9: 커밋**

```bash
git add etc/multigpu/tools/wall_view
git commit -F - <<'EOF'
feat(wall_view): GStreamer 파이프라인 + 인코더 선택 + 스모크

캡처 계획대로 소스를 만들고 d3d11compositor 싱크 패드 좌표로 1080p 캔버스에
직접 배치한다. 가상 뷰포트를 중간에 만들지 않고, 캡처부터 인코더까지 D3D11
메모리를 유지한다.

인코더는 구성 시점에 SINK caps에서 크기 한계와 D3D11 수용 여부를 읽어
검증한다. 이 머신 amfh264enc가 1920x1920으로 열거되므로 실재하는 위험이다.
소프트웨어 폴백은 제로카피가 깨지므로 경고를 낸다.

appsink 콜백은 블록하지 않는다. 거기서 막히면 캡처와 인코딩까지 밀린다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01GHG3p4nt6sHudkycas4R8E
EOF
```

---

### Task 6: WebSocket 서버 — 흐름 제어 + 정렬 게이트 + 인증 seam

**Files:**
- Create: `etc/multigpu/tools/wall_view/src/server.rs`
- Modify: `etc/multigpu/tools/wall_view/Cargo.toml` (tokio 계열)

**Interfaces:**
- Consumes: `proto::{InitMessage, FrameHeader, encode_frame, codec_string_from_avcc}`(Task 4)
- Produces: `server::Authenticator` (trait, `authenticate(&self, path: &str, headers: &HeaderMap) -> Result<(), AuthError>`), `server::NoAuth`, `server::ClientQueue::push(&mut self, keyframe: bool, bytes: Arc<Vec<u8>>) -> PushOutcome`, `server::PushOutcome { Sent, DroppedNeedsKeyframe, Fatal }`, `server::Broadcaster::{new, add_client, broadcast, client_count}`

- [ ] **Step 1: 의존성을 추가한다**

```toml
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "sync", "time"] }
tokio-tungstenite = "0.24"
futures-util = "0.3"
base64 = "0.22"
```

- [ ] **Step 2: 흐름 제어의 실패하는 테스트를 쓴다**

`src/server.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn au(size: usize) -> Arc<Vec<u8>> {
        Arc::new(vec![0u8; size])
    }

    #[test]
    fn holds_frames_until_the_first_keyframe_arrives() {
        let mut queue = ClientQueue::new(4, 1_000_000);
        assert_eq!(queue.push(false, au(10)), PushOutcome::DroppedNeedsKeyframe,
            "정렬 전에는 delta 를 보내지 않는다");
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.push(true, au(10)), PushOutcome::Sent);
        assert_eq!(queue.push(false, au(10)), PushOutcome::Sent);
    }

    #[test]
    fn drops_deltas_when_the_queue_is_full_and_asks_for_a_keyframe() {
        let mut queue = ClientQueue::new(2, 1_000_000);
        assert_eq!(queue.push(true, au(10)), PushOutcome::Sent);
        assert_eq!(queue.push(false, au(10)), PushOutcome::Sent);
        // 큐가 찼다.
        assert_eq!(queue.push(false, au(10)), PushOutcome::DroppedNeedsKeyframe);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn recovers_on_the_next_keyframe_by_clearing_the_backlog() {
        let mut queue = ClientQueue::new(2, 1_000_000);
        queue.push(true, au(10));
        queue.push(false, au(10));
        queue.push(false, au(10)); // 드롭 + 재정렬 대기
        let outcome = queue.push(true, au(10));
        assert_eq!(outcome, PushOutcome::Sent);
        assert_eq!(queue.len(), 1, "keyframe 이 오면 백로그를 버리고 거기서 다시 시작한다");
    }

    #[test]
    fn gives_up_after_repeated_failures() {
        let mut queue = ClientQueue::new(1, 1_000_000);
        queue.push(true, au(10));
        for _ in 0..MAX_CONSECUTIVE_DROPS {
            queue.push(false, au(10));
        }
        assert_eq!(queue.push(false, au(10)), PushOutcome::Fatal);
    }

    #[test]
    fn enforces_the_byte_budget_as_well_as_the_count() {
        let mut queue = ClientQueue::new(100, 50);
        assert_eq!(queue.push(true, au(40)), PushOutcome::Sent);
        assert_eq!(queue.push(false, au(40)), PushOutcome::DroppedNeedsKeyframe,
            "바이트 상한을 넘으면 개수가 남아도 드롭한다");
    }
}
```

- [ ] **Step 3: 테스트가 실패하는지 확인한다**

```powershell
cargo test
```
기대: 컴파일 실패.

- [ ] **Step 4: 흐름 제어를 구현한다**

`src/server.rs` 상단:

```rust
use std::collections::VecDeque;
use std::sync::Arc;

/// 연속 드롭이 이 횟수를 넘으면 클라이언트를 포기한다.
pub const MAX_CONSECUTIVE_DROPS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Sent,
    /// 이 프레임은 버렸다. 다음 keyframe에서 복구한다.
    DroppedNeedsKeyframe,
    /// 반복해서 못 따라온다. 연결을 끊어야 한다.
    Fatal,
}

/// 클라이언트 하나의 송신 큐.
///
/// 불변식: 한 클라이언트의 느림이 다른 클라이언트나 캡처에 전파되지 않는다.
/// 이 큐는 절대 블록하지 않고, 넘치면 버린다.
pub struct ClientQueue {
    frames: VecDeque<Arc<Vec<u8>>>,
    max_frames: usize,
    max_bytes: usize,
    queued_bytes: usize,
    /// 첫 keyframe을 받기 전에는 아무것도 보내지 않는다.
    aligned: bool,
    consecutive_drops: u32,
}

impl ClientQueue {
    pub fn new(max_frames: usize, max_bytes: usize) -> Self {
        Self {
            frames: VecDeque::new(),
            max_frames,
            max_bytes,
            queued_bytes: 0,
            aligned: false,
            consecutive_drops: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn pop(&mut self) -> Option<Arc<Vec<u8>>> {
        let frame = self.frames.pop_front();
        if let Some(ref bytes) = frame {
            self.queued_bytes -= bytes.len();
        }
        frame
    }

    pub fn push(&mut self, keyframe: bool, bytes: Arc<Vec<u8>>) -> PushOutcome {
        if keyframe {
            // keyframe이 오면 밀린 백로그를 버리고 거기서 다시 시작한다.
            // 늦게 따라가느니 최신 화면부터 보여주는 편이 모니터링에 맞다.
            self.frames.clear();
            self.queued_bytes = 0;
            self.aligned = true;
            self.consecutive_drops = 0;
        } else if !self.aligned {
            return PushOutcome::DroppedNeedsKeyframe;
        }

        let would_exceed = self.frames.len() >= self.max_frames
            || self.queued_bytes + bytes.len() > self.max_bytes;
        if would_exceed && !keyframe {
            self.consecutive_drops += 1;
            if self.consecutive_drops > MAX_CONSECUTIVE_DROPS {
                return PushOutcome::Fatal;
            }
            return PushOutcome::DroppedNeedsKeyframe;
        }

        self.queued_bytes += bytes.len();
        self.frames.push_back(bytes);
        self.consecutive_drops = 0;
        PushOutcome::Sent
    }
}
```

- [ ] **Step 5: 인증 seam을 만든다**

같은 파일에:

```rust
#[derive(Debug)]
pub struct AuthError(pub String);

/// 접속 인증. v1은 통과 구현만 쓴다.
///
/// **모든 WebSocket 업그레이드가 반드시 이 지점을 지난다.** 제품 배포 시
/// id/password가 필요해지면 여기에 구현체를 하나 더하고 설정에서 고르면 되고,
/// 접속 처리 전체를 뜯을 필요가 없다.
pub trait Authenticator: Send + Sync + 'static {
    fn authenticate(&self, path: &str, headers: &tokio_tungstenite::tungstenite::http::HeaderMap)
        -> Result<(), AuthError>;
}

/// v1 기본 — 항상 통과.
pub struct NoAuth;

impl Authenticator for NoAuth {
    fn authenticate(
        &self,
        _path: &str,
        _headers: &tokio_tungstenite::tungstenite::http::HeaderMap,
    ) -> Result<(), AuthError> {
        Ok(())
    }
}
```

- [ ] **Step 6: 테스트가 통과하는지 확인한다**

```powershell
cargo test
```
기대: 25개 통과(이전 20 + 이번 5).

- [ ] **Step 7: 브로드캐스터를 구현한다**

`src/server.rs`에 이어서. 클라이언트마다 `tokio::sync::mpsc` 채널을 두고, 브로드캐스터는 큐 판정 후 채널로 넘긴다. 채널이 가득 차는 상황은 큐 판정이 이미 막으므로 `try_send` 실패는 곧 연결 종료다.

```rust
use tokio::sync::mpsc;

pub struct ClientHandle {
    id: u64,
    queue: ClientQueue,
    sender: mpsc::Sender<Arc<Vec<u8>>>,
}

pub struct Broadcaster {
    clients: Vec<ClientHandle>,
    next_id: u64,
    max_clients: usize,
    /// 마지막으로 만든 init 메시지(JSON 직렬화 완료본). 새 접속자에게 바로 보낸다.
    init_json: Option<String>,
    max_frames: usize,
    max_bytes: usize,
}

impl Broadcaster {
    pub fn new(max_clients: usize, max_frames: usize, max_bytes: usize) -> Self {
        Self {
            clients: Vec::new(),
            next_id: 1,
            max_clients,
            init_json: None,
            max_frames,
            max_bytes,
        }
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    pub fn set_init(&mut self, init_json: String) {
        self.init_json = Some(init_json);
    }

    pub fn init_json(&self) -> Option<&str> {
        self.init_json.as_deref()
    }

    /// 새 클라이언트를 등록한다. 정원이 찼으면 None.
    pub fn add_client(&mut self, sender: mpsc::Sender<Arc<Vec<u8>>>) -> Option<u64> {
        if self.clients.len() >= self.max_clients {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.clients.push(ClientHandle {
            id,
            queue: ClientQueue::new(self.max_frames, self.max_bytes),
            sender,
        });
        Some(id)
    }

    pub fn remove_client(&mut self, id: u64) {
        self.clients.retain(|client| client.id != id);
    }

    /// 모든 클라이언트에 프레임을 넣는다. 못 따라오는 클라이언트는 제거한다.
    ///
    /// 이 함수는 절대 블록하지 않는다 — appsink 콜백에서 불리기 때문이다.
    pub fn broadcast(&mut self, keyframe: bool, bytes: Arc<Vec<u8>>) {
        let mut doomed = Vec::new();
        for client in &mut self.clients {
            match client.queue.push(keyframe, bytes.clone()) {
                PushOutcome::Sent => {
                    while let Some(frame) = client.queue.pop() {
                        if client.sender.try_send(frame).is_err() {
                            doomed.push(client.id);
                            break;
                        }
                    }
                },
                PushOutcome::DroppedNeedsKeyframe => {},
                PushOutcome::Fatal => doomed.push(client.id),
            }
        }
        for id in doomed {
            eprintln!("wall_view: dropping client {id} (cannot keep up)");
            self.remove_client(id);
        }
    }
}
```

**접속 처리**(`serve` 함수): `tokio::net::TcpListener`로 `--bind`에 바인딩하고, 요청 경로가 `/stream`이면 `Authenticator::authenticate`를 통과시킨 뒤 WebSocket으로 업그레이드한다. 업그레이드 직후 `init_json`을 텍스트로 보내고, 파이프라인에 `force_keyframe()`을 요청한다(직전 요청으로부터 200ms 이내면 생략). 그다음 채널 수신 태스크가 큐에서 나온 바이트를 바이너리로 흘린다.

- [ ] **Step 8: `--max-clients`와 `--bind`를 배선한다**

`Args`에 추가:

```rust
    /// WebSocket 바인드 주소. 기본은 로컬 전용이다.
    #[arg(long, default_value = "127.0.0.1:8787")]
    bind: String,

    /// 동시 접속 상한
    #[arg(long, default_value_t = 8)]
    max_clients: usize,
```

- [ ] **Step 9: 접속 스모크**

```powershell
cargo run --release -- --layout ..\..\config\wall_layout.example_2x1_display.json
```
다른 창에서:
```powershell
Test-NetConnection -ComputerName 127.0.0.1 -Port 8787
```
기대: `TcpTestSucceeded : True`. 서버 로그에 클라이언트 접속/해제가 찍혀야 한다. **확인 후 서버를 종료한다** — 실행 중인 프로세스를 남기지 않는다.

- [ ] **Step 10: 커밋**

```bash
git add etc/multigpu/tools/wall_view
git commit -F - <<'EOF'
feat(wall_view): WebSocket 서버 - 흐름 제어, 정렬 게이트, 인증 seam

불변식은 한 클라이언트의 느림이 다른 클라이언트나 캡처에 전파되지 않는 것이다.
클라이언트마다 바운디드 큐를 두고 넘치면 delta 를 버린다. keyframe 이 오면
백로그를 버리고 거기서 다시 시작한다 - 늦게 따라가느니 최신 화면부터 보여주는
편이 모니터링에 맞다. 반복해서 못 따라오면 연결을 끊는다.

첫 keyframe 전에는 아무것도 보내지 않는다. delta 부터 받아 디코더 에러를
내는 상황을 서버에서 막는다.

인증은 v1 에 없지만 모든 업그레이드가 Authenticator 를 지나게 해두어, 나중에
id-password 를 넣을 때 접속 처리를 뜯지 않아도 되게 한다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01GHG3p4nt6sHudkycas4R8E
EOF
```

---

### Task 7: 클라이언트 페이지 + 정적 서빙

**Files:**
- Create: `etc/multigpu/tools/wall_view/web/index.html`
- Modify: `etc/multigpu/tools/wall_view/src/server.rs` (정적 서빙)

- [ ] **Step 1: 클라이언트 페이지를 만든다**

`web/index.html`:

```html
<!doctype html>
<meta charset="utf-8">
<title>wall_view</title>
<style>
  html, body { margin: 0; background: #101014; color: #ddd;
               font: 13px system-ui, sans-serif; }
  #status { position: fixed; left: 8px; top: 8px; padding: 4px 8px;
            background: rgba(0,0,0,.6); border-radius: 4px; }
  canvas { display: block; width: 100vw; height: auto; }
</style>
<div id="status">connecting…</div>
<canvas id="view"></canvas>
<script>
const statusEl = document.getElementById("status");
const canvas = document.getElementById("view");
const ctx = canvas.getContext("2d");
const HEADER_LEN = 16;

let decoder = null;
let decoded = 0;

function setStatus(text) { statusEl.textContent = text; }

function base64ToBytes(text) {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

function configure(init) {
  canvas.width = init.coded_width;
  canvas.height = init.coded_height;
  decoder = new VideoDecoder({
    output: (frame) => {
      ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
      frame.close();
      decoded++;
      if (decoded % 30 === 0) setStatus(`${init.codec} ${canvas.width}x${canvas.height} — ${decoded} frames`);
    },
    error: (error) => setStatus("decoder error: " + error.message),
  });
  decoder.configure({
    codec: init.codec,
    description: base64ToBytes(init.description),
    codedWidth: init.coded_width,
    codedHeight: init.coded_height,
    optimizeForLatency: true,
    hardwareAcceleration: "prefer-hardware",
  });
  setStatus(`configured ${init.codec}`);
}

const socket = new WebSocket(`ws://${location.host}/stream`);
socket.binaryType = "arraybuffer";
socket.onopen = () => setStatus("connected, waiting for init…");
socket.onclose = () => setStatus("disconnected");
socket.onerror = () => setStatus("connection error");
socket.onmessage = (event) => {
  if (typeof event.data === "string") {
    const message = JSON.parse(event.data);
    if (message.type === "init") configure(message);
    return;
  }
  if (!decoder) return;
  const view = new DataView(event.data);
  const keyframe = (view.getUint8(1) & 1) === 1;
  const timestamp = Number(view.getBigInt64(4, true));
  const payload = new Uint8Array(event.data, HEADER_LEN);
  decoder.decode(new EncodedVideoChunk({
    type: keyframe ? "key" : "delta",
    timestamp,
    data: payload,
  }));
};
</script>
```

- [ ] **Step 2: 정적 서빙을 붙인다**

같은 TCP 리스너에서 경로로 갈라 준다. 페이지는 `include_str!`로 바이너리에 넣어 배포 시 파일이 따라다니지 않게 한다.

`src/server.rs`에 추가:

```rust
/// 클라이언트 페이지. 바이너리에 포함시켜 배포 시 파일 의존이 없게 한다.
const INDEX_HTML: &str = include_str!("../web/index.html");

/// 업그레이드 전에 첫 요청 줄을 읽어 경로를 얻는다.
/// `/stream`이면 WebSocket, 그 외는 정적 페이지.
async fn dispatch(stream: tokio::net::TcpStream, broadcaster: SharedBroadcaster) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();

    if path == "/stream" {
        // 남은 헤더는 tungstenite 가 읽는다.
        return serve_websocket(reader.into_inner(), broadcaster).await;
    }

    // 정적 페이지: 나머지 헤더를 흘려버리고 응답한다.
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 || line.trim().is_empty() {
            break;
        }
    }
    let body = INDEX_HTML.as_bytes();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut stream = reader.into_inner();
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}
```

`SharedBroadcaster`는 `Arc<Mutex<Broadcaster>>`의 별칭이다(`std::sync::Mutex`로 충분하다 — 잠금 구간이 짧고 `.await`를 걸치지 않는다).

- [ ] **Step 3: 사용자 육안 판정 요청**

**이 단계는 사용자가 한다.** 서브에이전트는 판정하지 않는다. 다음을 요청하고 응답을 기다린다:

```powershell
cargo run --release -- --layout ..\..\config\wall_layout.example_2x1_display.json
# 브라우저에서 http://127.0.0.1:8787/
```

판정 기준:
- 월 화면이 보이는가
- 타일 배치가 레이아웃과 맞는가(좌/우가 뒤바뀌지 않았는가)
- 체감 지연이 제어용으로 쓸 만한가

- [ ] **Step 4: 커밋**

```bash
git add etc/multigpu/tools/wall_view
git commit -F - <<'EOF'
feat(wall_view): WebCodecs 클라이언트 페이지 + 정적 서빙

브라우저가 init 메시지로 VideoDecoder 를 설정하고 바이너리 프레임을 디코드해
canvas 에 그린다. 페이지는 include_str 로 바이너리에 넣어 배포 시 파일이
따라다니지 않게 했다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01GHG3p4nt6sHudkycas4R8E
EOF
```

---

### Task 8: 에러 처리 — 재구성, idle-stop, 문서

**Files:**
- Modify: `etc/multigpu/tools/wall_view/src/main.rs`
- Create: `etc/multigpu/tools/wall_view/README.md`

- [ ] **Step 1: 파이프라인 버스 에러 시 재구성한다**

모니터 분리·해상도 변경은 Desktop Duplication에서 access-lost로 나타나 버스 `Error`가 된다. 뷰어가 끊기지 않는 게 모니터링 도구의 기본 요건이므로, 클라이언트 연결은 유지한 채 파이프라인만 다시 만든다.

`src/main.rs`에 감시 루프를 둔다:

```rust
/// 파이프라인을 만들고, 버스 에러가 나면 토폴로지를 다시 읽어 재구성한다.
/// 클라이언트 연결은 이 루프 밖에서 유지되므로 영향받지 않는다.
fn run_pipeline_supervised(
    layout: &layout::WallLayout,
    output: layout::Size,
    make_config: impl Fn(Vec<plan::CaptureSlot>) -> pipeline::PipelineConfig,
    on_sample: impl Fn(&proto::FrameHeader, &[u8], Option<&[u8]>) + Clone + Send + Sync + 'static,
    shutdown: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use std::sync::atomic::Ordering;

    while !shutdown.load(Ordering::Relaxed) {
        let displays = topology::spatial_order(&topology::enumerate());
        let slots = plan::plan_capture(layout, &displays, output)?;
        let pipeline = pipeline::WallPipeline::new(make_config(slots), on_sample.clone())?;
        pipeline.play()?;

        // 버스를 감시한다. Error/Eos 면 빠져나와 재구성한다.
        let bus = pipeline.bus().context("pipeline bus")?;
        for message in bus.iter_timed(gstreamer::ClockTime::NONE) {
            use gstreamer::MessageView;
            match message.view() {
                MessageView::Error(error) => {
                    eprintln!(
                        "wall_view: pipeline error from {:?}: {} — rebuilding in 1s",
                        error.src().map(|s| s.path_string()),
                        error.error()
                    );
                    break;
                },
                MessageView::Eos(..) => {
                    eprintln!("wall_view: pipeline reached EOS — rebuilding in 1s");
                    break;
                },
                _ => {},
            }
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
        }

        let _ = pipeline.stop();
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    Ok(())
}
```

`WallPipeline`에 버스 접근자를 추가한다(`src/pipeline.rs`):

```rust
    pub fn bus(&self) -> Option<gst::Bus> {
        self.pipeline.bus()
    }
```

새 avcC가 나오면 `on_sample`의 세 번째 인자로 들어오므로, 재구성 후 첫 프레임에서 자동으로 `InitMessage`가 갱신되고 기존 클라이언트에게 재전송된다 — 별도 처리가 필요 없다.

- [ ] **Step 2: idle-stop을 구현한다**

아무도 안 볼 때 월 머신의 GPU와 전력을 쓸 이유가 없다.

`Args`에 추가:

```rust
    /// 시청자가 없으면 파이프라인을 정지한다
    #[arg(long, default_value_t = true)]
    idle_stop: bool,
```

브로드캐스터의 클라이언트 수 변화를 파이프라인 상태에 반영한다. `Broadcaster::add_client`/`remove_client` 호출 지점에서 전이를 판정한다:

```rust
/// 클라이언트 수 변화에 따라 파이프라인 상태를 전환한다.
/// 0 -> 1 이면 Playing, 1 -> 0 이면 Null.
fn apply_idle_policy(
    idle_stop: bool,
    previous_count: usize,
    current_count: usize,
    pipeline: &pipeline::WallPipeline,
) -> Result<()> {
    if !idle_stop {
        return Ok(());
    }
    if previous_count == 0 && current_count > 0 {
        eprintln!("wall_view: first viewer connected — starting pipeline");
        pipeline.play()?;
    } else if previous_count > 0 && current_count == 0 {
        eprintln!("wall_view: last viewer left — stopping pipeline");
        pipeline.stop()?;
    }
    Ok(())
}
```

`--idle-stop=false`면 프로세스 시작 시 바로 `play()`하고 계속 돌린다.

- [ ] **Step 3: README를 쓴다**

`README.md`에 다음을 적는다: 무엇을 하는 도구인지, 실행 전 필요한 GStreamer 환경변수 3개, CLI 플래그 전체, 브라우저 접속 주소, 그리고 **알려진 한계**(인증 없음·바인드 기본 로컬, 오디오 없음, 서브스트림 없음, 소프트웨어 폴백 시 제로카피 상실, AMF 크기 한계).

- [ ] **Step 4: 전체 검사**

```powershell
cargo test
cargo build --release
cargo fmt --check
cargo clippy -- -D warnings
```
기대: 테스트 25개 통과, 빌드 exit 0, fmt/clippy 무출력.

- [ ] **Step 5: 커밋**

```bash
git add etc/multigpu/tools/wall_view
git commit -F - <<'EOF'
feat(wall_view): 파이프라인 재구성, idle-stop, README

모니터 분리나 해상도 변경은 Desktop Duplication 에서 access-lost 로 나타난다.
버스 에러를 잡아 teardown 후 재구성하고 클라이언트 연결은 유지한다 - 뷰어가
끊기지 않는 게 모니터링 도구의 기본 요건이다.

시청자가 0명이면 파이프라인을 정지한다. 아무도 안 볼 때 월 머신의 GPU 와
전력을 쓸 이유가 없다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01GHG3p4nt6sHudkycas4R8E
EOF
```

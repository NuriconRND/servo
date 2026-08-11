# 설정 표면 정리 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 흩어진 환경변수 39 개를 "운용 설정은 pref, 조사용은 등록된 env" 두 갈래로 정리하고, 표류가 다시 생기지 않도록 테스트로 못박는다.

**Architecture:** 운용 19 개는 servo 의 기존 pref 체계(`gfx.*` / `media.*`)로 옮긴다 — 포크가 이미 `media_wall_region_upload` 등을 같은 방식으로 추가한 선례를 따른다. 조사용 17 개는 `servo_config` 안의 단일 테이블에 등록하고 호출부에서 이름 문자열을 없앤다. 표류는 규칙이 아니라 **양방향 테스트**로 막는다.

**Tech Stack:** Rust, servo fork(`nonstandard-media-display-port`), `servo_config` + `servo_config_macro`(`ServoPreferences` derive), bpaf(servoshell 인자), GStreamer/D3D11/DirectComposition(Windows).

**설계 문서:** `docs/multigpu/multigpu_config_surface_consolidation_design.md` — 분류 근거와 결정 사유는 전부 거기 있다. 이 계획서는 그것을 이행한다.

## Global Constraints

- **작업 위치**: 워크트리 `F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\servo_multigpu-tiled-wall`, 브랜치 `nonstandard-media-display-port`. 다른 워크트리(`servo`, `servo_study`)를 건드리지 않는다.
- **워크트리에 이미 미커밋 변경이 있다** (`etc/multigpu/config/wall_layout.example_1x1.json`, `tests/html/multigpu_standard_video_extended_probe.html`, `tests/html/multigpu_standard_video_rtsp_probe.html`). **절대 커밋하지 마라** — 마지막 파일에는 하드코딩된 RTSP 자격증명이 들어 있다. `git add <경로>` 로 자기 파일만 담고 `git add -A` 를 쓰지 마라.
- ★**빌드는 반드시 `W:\servo_multigpu-tiled-wall` 에서 한다.**★ `F:\…` 원경로(82 자)로 빌드하면 **mozangle 이 긴 경로 때문에 실패**한다. `W:` 는 프로젝트 루트로 이미 `subst` 매핑돼 있다(`subst` 로 확인 가능). 실측: `W:` 경유 `cargo check -p servoshell` → 3 분 32 초, exit 0.
- **환경 설정은 워크트리가 아니라 프로젝트 루트에 있다**: `. W:\scripts\servo_env.ps1` (앞의 점이 중요하다 — 없으면 자식 프로세스에만 적용된다). MSVC 개발자 환경까지 불러온다. `.\scripts\servo_env.ps1` 는 **없다.**
- ★**개별 크레이트 `cargo check -p <crate>` 로 검증하지 마라.**★ Windows 에서 surfman 백엔드가 WGL 로 선택돼 `create_isolated_device` 미존재로 깨진다. `servoshell` 은 `Cargo.toml:138` 에서 `servo` 를 `features = ["no-wgl"]` 로 당기므로 ANGLE 백엔드가 선택된다 — **검증은 `cargo check -p servoshell` 로 한다.** `servo-config` 처럼 그래픽에 의존하지 않는 크레이트만 단독 check 가 가능하다.
- **추측 금지**: 기본값·문법을 코드에서 읽지 않고 적으면 안 된다. 설계 문서 §4 에 반례가 있다 — `SERVO_VIDEO_DECOUPLE` 과 `SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN` 은 **기본 on 인 킬스위치**다.
- **기대값과 계산이 맞지 않으면 데이터나 테스트를 고치지 말고 모순을 보고하라.**
- 주석은 **한국어**로 **왜**를 설명한다(기존 파일 어투를 맞춘다).
- 커밋 메시지에 큰따옴표(`"`)를 넣지 않는다. 각 커밋 끝에 두 줄:
  ```
  Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01GHG3p4nt6sHudkycas4R8E
  ```
- 각 태스크 종료 시: `cargo test -p servo_config`, `cargo check -p servoshell`, `rustfmt --edition 2024 --check <손댄 .rs>`, `git diff --check`.
- **GUI 육안 판정은 사용자 몫**이다. 서브에이전트가 판정하지 않는다.
- 실행 프로세스를 남기지 않는다. 빌드는 포그라운드로 돌리고 `Finished`/`error` 를 실제로 본 뒤 진행한다.

## File Structure

| 파일 | 책임 |
|---|---|
| `components/config/debug_env.rs` | **신규.** 조사용 env 17 개의 단일 테이블 + 타입별 접근자 + `OnceLock` 캐싱 + 깨진 값 경고 |
| `components/config/tests/config_surface.rs` | **신규.** 양방향 표류 테스트, 문서 테스트, "그 검사가 무엇을 잡는지" 고정 테스트 |
| `components/config/prefs.rs` | 운용 19 개를 `gfx_*` / `media_*` 필드로 추가 + `const_default()` |
| `components/config/lib.rs` | `pub mod debug_env;` 노출 |
| `components/config/config_dump.rs` | **신규.** 기본값과 다른 pref + 설정된 debug env 만 찍는 기동 덤프 |
| `components/config/removed_env.rs` | **신규.** 제거된 env 이름 → 대응 pref 안내 표 + 기동 차단 판정 |
| `third_party/surfman/src/platform/windows/angle/surface.rs` | DComp 게이트를 주입값으로 전환(공개 시그니처 유지) |
| `components/paint/dcomp_compositor.rs` | `storage_mode()` 를 주입값에서 유도 |
| `components/shared/paint/wall_args.rs` | **신규.** 두 셸이 공유하는 `WallArgs` 검증·해석 |
| `docs/multigpu/configuration.md` | **신규.** pref 19 + 조사 노브 17 전량 표 |

---

### Task 1: `debug_env` 테이블과 양방향 표류 테스트

조사용 env 17 개를 한 테이블에 등록하고, 호출부에서 이름 문자열을 없앤다. 표류 방지 테스트를 여기서 세운다 — 이후 태스크들이 이 테스트 위에서 안전해진다.

**Files:**
- Create: `components/config/debug_env.rs`
- Create: `components/config/tests/config_surface.rs`
- Modify: `components/config/lib.rs` (모듈 노출)
- Modify: 호출부 — `components/paint/{paint.rs,painter.rs,dcomp_compositor.rs}`, `components/media/backends/gstreamer/{player.rs,render-d3d11/lib.rs}`, `components/media/media-thread/lib.rs`, `components/script/dom/html/htmlmediaelement.rs`

**Interfaces:**
- Produces: `servo_config::debug_env::{DebugFlag, Kind, ALL}`, `debug_env::enabled(&DebugFlag) -> bool`, `debug_env::int(&DebugFlag) -> Option<i64>`, `debug_env::string(&DebugFlag) -> Option<String>`, 그리고 17 개 상수(`DCOMP_READBACK` 등)

- [ ] **Step 1: 등록 대상 17 개의 현재 판정을 읽어 적는다**

아래 이름을 `git grep -n` 으로 찾아 **각 호출부의 실제 판정식과 기본값을 메모**한다. 추측하지 마라.

```
SERVO_WALL_FRAME_DELAY_TARGET_INDEX  SERVO_WALL_FRAME_DELAY_AFTER  SERVO_WALL_FRAME_DELAY_COUNT
SERVO_D3D11_PROFILE  SERVO_D3D11_PROFILE_MS
SERVO_DCOMP_DEBUG  SERVO_DCOMP_READBACK  SERVO_DCOMP_VALIDPROBE
SERVO_DCOMP_NO_PARTIAL_PRESENT  SERVO_DCOMP_DISABLE_RESIZE_REBUILD  SERVO_DCOMP_DISABLE_RESIZE_VIRTUAL
SERVO_DISABLE_VIDEO_IMMEDIATE_COMPOSITE  SERVO_DISABLE_VIDEO_UPDATE_COALESCE
SERVO_LOG_PRESENT_CADENCE  SERVO_VIDEO_ESCAPE_PROF
SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF  SERVO_WR_PICTURE_TILE_SIZE
```

`SERVO_D3D11_PROFILE` 과 `SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF` 는 **두 크레이트에서 각각 읽힌다**. 두 곳의 판정이 서로 다르면 **고치지 말고 보고하라** — 그 자체가 발견 사항이다.

- [ ] **Step 2: 실패하는 테스트를 쓴다**

`components/config/tests/config_surface.rs`:

```rust
//! 설정 표면이 흩어지지 않도록 강제하는 테스트.
//!
//! 규칙만 정해 두면 다음 병합에서 무너진다 — 이미 SERVO_WR_PICTURE_TILE 과
//! SERVO_WR_PICTURE_TILE_SIZE 로 한 번 겪었다. 그래서 테스트로 건다.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// upstream servo 가 소유한 이름. 우리가 관리하지 않는다.
const UPSTREAM_OWNED: &[&str] = &[
    "SERVO_TRACING",
    "SERVO_DIAGNOSTICS",
    "SERVO_STYLE_THREAD_STACK_SIZE_KB",
];

fn repo_root() -> PathBuf {
    // components/config -> 워크트리 루트
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// 소스에서 env::var("SERVO_…") 로 읽는 이름을 모은다.
///
/// third_party/stylo 는 제외한다 — 2026-07-16 에 upstream 체크아웃을 통째로
/// 복사한 것이라 그 안의 노브는 upstream 소유다. third_party/surfman 은 포함한다
/// — 서브모듈이 아니라 이 포크가 직접 수정해 온 코드다.
fn env_names_read_in_sources() -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let root = repo_root();
    for entry in walk_rust_files(&root) {
        let text = std::fs::read_to_string(&entry).unwrap_or_default();
        let mut rest = text.as_str();
        while let Some(at) = rest.find("\"SERVO_") {
            rest = &rest[at + 1..];
            if let Some(end) = rest.find('"') {
                let name = &rest[..end];
                if name.starts_with("SERVO_") && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
                    found.insert(name.to_string());
                }
            }
        }
    }
    found
}

#[test]
fn every_env_name_read_in_sources_is_registered() {
    let registered: BTreeSet<String> = servo_config::debug_env::ALL
        .iter()
        .map(|flag| flag.name.to_string())
        .collect();
    let allowed: BTreeSet<String> = UPSTREAM_OWNED.iter().map(|s| s.to_string()).collect();

    let unregistered: Vec<String> = env_names_read_in_sources()
        .into_iter()
        .filter(|name| !registered.contains(name) && !allowed.contains(name))
        .collect();

    assert!(
        unregistered.is_empty(),
        "테이블에 없는 환경변수를 읽고 있다: {unregistered:?}\n\
         조사용이면 debug_env::ALL 에 등록하고, 운용 설정이면 pref 로 옮겨라."
    );
}

#[test]
fn every_registered_flag_is_actually_read_somewhere() {
    // 반대 방향. 이것이 없으면 죽은 항목이 테이블에 쌓인다.
    let read = env_names_read_in_sources();
    let dead: Vec<&str> = servo_config::debug_env::ALL
        .iter()
        .map(|flag| flag.name)
        .filter(|name| !read.contains(*name))
        .collect();
    assert!(dead.is_empty(), "테이블에 있는데 아무도 읽지 않는다: {dead:?}");
}
```

`walk_rust_files` 는 같은 파일에 둔다: `root` 아래 `*.rs` 를 재귀 수집하되 `target/`, `third_party/stylo/`, `.git/` 을 건너뛴다.

- [ ] **Step 3: 테스트가 실패하는지 확인한다**

```powershell
cargo test -p servo_config --test config_surface
```
기대: 컴파일 실패(`debug_env` 미정의).

- [ ] **Step 4: 테이블을 구현한다**

`components/config/debug_env.rs`:

```rust
//! 조사용 환경변수의 단일 등록처.
//!
//! 운용 설정은 pref 다(설계 문서 §3 의 판정 기준). 여기 있는 것은 실패 주입·
//! 프로파일링·이분탐색용이며, 조사가 끝나면 지운다.
//!
//! 호출부가 이름 문자열을 갖지 않는 것이 핵심이다 — 이름이 두 곳에 있으면
//! 한쪽만 바뀌어 조용히 갈라진다.

use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// 설정돼 있기만 하면 켜짐(값은 보지 않는다).
    Presence,
    Int,
    Str,
}

#[derive(Debug)]
pub struct DebugFlag {
    pub name: &'static str,
    pub kind: Kind,
    /// 무엇을 조사하려고 만든 노브인가. 문서 표가 이 문장을 쓴다.
    pub doc: &'static str,
}

pub const DCOMP_READBACK: DebugFlag = DebugFlag {
    name: "SERVO_DCOMP_READBACK",
    kind: Kind::Presence,
    doc: "DComp 서피스를 CPU 로 읽어 내려 실제로 그려졌는지 확인한다(진단 전용, 느리다)",
};

// … Step 1 에서 메모한 17 개를 같은 형태로 …

pub const ALL: &[&DebugFlag] = &[&DCOMP_READBACK /* … */];

pub fn enabled(flag: &'static DebugFlag) -> bool {
    debug_assert_eq!(flag.kind, Kind::Presence);
    *cache(flag).presence.get_or_init(|| std::env::var(flag.name).is_ok())
}

pub fn int(flag: &'static DebugFlag) -> Option<i64> {
    debug_assert_eq!(flag.kind, Kind::Int);
    *cache(flag).int.get_or_init(|| match std::env::var(flag.name) {
        Ok(raw) => match raw.trim().parse::<i64>() {
            Ok(value) => Some(value),
            Err(_) => {
                // 조용히 기본값으로 돌아가면 켰다고 믿는데 안 켜진 상태가 된다.
                // 조사용 노브라 기동을 막지는 않고 경고만 한다.
                eprintln!("servo: {}={raw} is not an integer; ignoring", flag.name);
                None
            },
        },
        Err(_) => None,
    })
}
```

`string()` 도 같은 모양이다. `cache()` 는 이름별 `OnceLock` 을 돌려주면 되고, 상수 개수가 고정이므로 `ALL` 인덱스 기반 정적 배열로 잡는다.

- [ ] **Step 5: 호출부를 교체한다**

Step 1 에서 찾은 자리마다:

```rust
// before
static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
*OFF.get_or_init(|| std::env::var("SERVO_DCOMP_NO_PARTIAL_PRESENT").is_ok())

// after
debug_env::enabled(&debug_env::DCOMP_NO_PARTIAL_PRESENT)
```

호출부의 `OnceLock` 은 지운다 — 캐싱이 `debug_env` 안으로 들어갔다. 지금 이 패턴이 17 번 복제돼 있다.

`components/config/lib.rs` 에 `pub mod debug_env;` 를 추가한다.

- [ ] **Step 6: 테스트가 통과하는지 확인한다**

```powershell
cargo test -p servo_config --test config_surface
cargo check -p servoshell
```
기대: 두 테스트 통과. 실패하면 메시지가 지목하는 이름을 등록하거나 pref 대상으로 분류한다.

- [ ] **Step 7: 검사가 무엇을 잡는지 고정한다**

통과만 하는 검사는 거짓 안심이다. 합성 입력으로 양방향을 확인하는 테스트를 같은 파일에 추가한다.

```rust
#[test]
fn the_drift_check_actually_catches_an_unregistered_name() {
    // 스캐너가 실제로 이름을 뽑아내는지 — 합성 소스로 확인한다.
    let text = r#" let x = std::env::var("SERVO_MADE_UP_KNOB").is_ok(); "#;
    assert!(extract_env_names(text).contains("SERVO_MADE_UP_KNOB"));
    // 리터럴이 아닌 조립 형태는 못 잡는다는 것도 함께 고정한다(알려진 한계).
    let assembled = r#" let n = format!("SERVO_{}", suffix); std::env::var(n) "#;
    assert!(extract_env_names(assembled).is_empty());
}
```

`extract_env_names(&str) -> BTreeSet<String>` 을 Step 2 의 스캐너에서 분리해 순수 함수로 만든다.

- [ ] **Step 8: 커밋**

```
refactor(config): 조사용 환경변수를 단일 테이블로 등록

이름 문자열이 호출부마다 박혀 있어 한쪽만 바뀌면 조용히 갈라졌다. 실제로
SERVO_D3D11_PROFILE 과 SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF 는 서로 다른
크레이트에서 각자 읽고 있다.

이름과 타입과 설명을 debug_env 의 한 테이블에 두고 호출부는 상수를 참조한다.
OnceLock 캐싱도 그 안으로 옮겼다 - 같은 패턴이 17 번 복제돼 있었다.

표류는 양방향 테스트로 막는다. 테이블에 없는 이름을 읽으면 실패하고, 아무도
읽지 않는 항목이 테이블에 있어도 실패한다. 한 방향만 걸면 죽은 노브가 쌓인다.
스캐너가 무엇을 잡고 무엇을 못 잡는지도 테스트로 고정했다.
```

---

### Task 2: `gfx.*` pref 6 개 추가와 기동 덤프

**Files:**
- Modify: `components/config/prefs.rs`
- Create: `components/config/config_dump.rs`
- Modify: `components/config/lib.rs`
- Modify: 호출부 — `components/paint/paint.rs`(3), `components/paint/refresh_driver.rs`(1), `components/servo/examples/winit_wall/main.rs` + `ports/servoshell/desktop/headed_window.rs`(`SERVO_WIN_VSYNC`)

**Interfaces:**
- Consumes: `servo_config::debug_env`(Task 1)
- Produces: pref 필드 `gfx_dcomp_mode: String`, `gfx_vsync_enabled: bool`, `gfx_refresh_hz: i64`, `gfx_wall_frame_pacing_enabled: bool`, `gfx_wall_frame_max_pending: i64`, `gfx_wall_frame_min_interval_ms: i64`; `servo_config::config_dump::log_effective_config()`

- [ ] **Step 1: 현재 기본값을 코드에서 읽어 확정한다**

이미 확인된 값(설계 문서 §4):

| pref | 기본값 | 근거 |
|---|---|---|
| `gfx.refresh.hz` | `120` | `refresh_driver.rs` 의 `unwrap_or(120)`, `[1,1000]` 필터 |
| `gfx.vsync.enabled` | `false` | `is_ok_and(1\|true\|on)` |
| `gfx.dcomp.mode` | `"off"` | env 미설정 시 `dcomp_native_compositor_requested()==false` |

`gfx.wall.frame.*` 3 개는 `components/paint/paint.rs` 의 해당 상수/판정을 읽어 확정한다. **추측 금지.**

- [ ] **Step 2: 실패하는 테스트를 쓴다**

`components/config/tests/config_surface.rs` 에 추가:

```rust
#[test]
fn gfx_defaults_match_the_behaviour_before_the_migration() {
    // Preferences 는 prefs 모듈에 있다(pref_util 이 아니다 - 거기에는 PrefValue 만 있다).
    let defaults = servo_config::prefs::Preferences::const_default();
    // 이전 동작을 그대로 유지해야 한다 - 기본값이 바뀌면 조용히 화면이 달라진다.
    assert_eq!(defaults.gfx_refresh_hz, 120, "refresh_driver.rs 의 unwrap_or(120)");
    assert!(!defaults.gfx_vsync_enabled, "DwmFlush 가 코어를 상시 소모해 기본 off 다");
    assert_eq!(defaults.gfx_dcomp_mode, "off");
}
```

- [ ] **Step 3: 테스트가 실패하는지 확인한다**

```powershell
cargo test -p servo_config --test config_surface
```
기대: 컴파일 실패(필드 미정의).

- [ ] **Step 4: pref 필드를 추가한다**

`components/config/prefs.rs` 의 알파벳 순서에 맞는 자리에(`fonts_*` 와 `image_*` 사이) 넣는다. 포크가 추가한 기존 pref(`dom_webgpu_multigpu_fanout` 등)와 같은 방식으로 doc 주석을 단다.

```rust
    /// DirectComposition 네이티브 컴포지터 모드.
    /// `off` = 기존 Draw 경로, `on` = 하이브리드(전면 갱신 서피스를 스왑체인으로 승격),
    /// `surface` = 가상 서피스 전용. 3 상태이므로 bool 두 개로 쪼개지 않는다
    /// (on 과 surface 가 배타적이라는 것을 타입이 막아야 한다).
    pub gfx_dcomp_mode: String,
```

`const_default()` 에 Step 1 에서 확정한 값을 적는다.

- [ ] **Step 5: 호출부를 pref 로 바꾼다**

`paint.rs` 의 `WALL_FRAME_*_ENV` 상수와 그 판정, `refresh_driver.rs` 의 `SERVO_REFRESH_TIMER_HZ` 읽기, 두 셸의 `SERVO_WIN_VSYNC` 읽기를 `prefs::get().gfx_*` 로 교체한다.

`gfx.refresh.hz` 의 `[1,1000]` 클램프는 **유지한다** — pref 로 옮긴다고 검증이 사라지면 안 된다. 범위를 벗어나면 경고 후 기본값을 쓴다.

`gfx.dcomp.mode` 는 **Task 3 에서** 배선한다. 여기서는 필드만 추가한다.

- [ ] **Step 6: 기동 덤프를 만든다**

`components/config/config_dump.rs`:

```rust
/// 기본값과 다른 pref, 그리고 설정된 조사용 env 만 찍는다.
///
/// 전량을 매번 찍으면 아무도 읽지 않는다. 조용한 것이 기본이어야 무언가 떴을 때
/// 의미가 생긴다. 이 덤프가 있으면 로그만 보고 그때 어떤 설정으로 돌렸는지 알 수
/// 있다 - 지금은 실행 명령을 따로 보관해야만 안다.
pub fn log_effective_config() {
    let current = crate::prefs::get();
    let defaults = Preferences::const_default();
    for (name, value, default) in current.diff_from(&defaults) {
        eprintln!("servo: config: {name}={value} (default {default})");
    }
    let set: Vec<&str> = crate::debug_env::ALL
        .iter()
        .filter(|flag| std::env::var(flag.name).is_ok())
        .map(|flag| flag.name)
        .collect();
    if !set.is_empty() {
        eprintln!("servo: config: debug env: {}", set.join(", "));
    }
}
```

`diff_from` 은 `prefs.rs` 에 이미 있는 `diff()` 를 재사용하거나 같은 방식으로 만든다. 두 셸의 기동 경로에서 한 번 호출한다.

- [ ] **Step 7: 통과 확인과 커밋**

```powershell
cargo test -p servo_config --test config_surface
cargo check -p servoshell
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl --release
```

```
feat(config): gfx 운용 노브 6 개를 pref 로 + 기동 덤프

환경변수로 흩어져 있던 컴포지터/표출 노브를 servo pref 체계로 옮긴다. 포크가
이미 media_wall_region_upload 등을 같은 방식으로 추가한 선례를 따랐고, 별도
포크 전용 네임스페이스는 만들지 않았다.

기본값은 전부 코드에서 읽어 확정했다. refresh 는 120Hz 에 [1,1000] 클램프이고
그 검증은 pref 로 옮긴 뒤에도 유지한다. vsync 는 DwmFlush 가 코어를 상시
소모해 기본 off 다.

기동 덤프는 기본값과 다른 것만 찍는다. 전량을 매번 찍으면 아무도 읽지 않는다.
```

---

### Task 3: DComp 게이트 파싱을 한 곳으로

설계 문서 §5. 한 변수를 세 곳이 서로 다른 문법으로 읽는 문제를 닫는다.

**Files:**
- Modify: `third_party/surfman/src/platform/windows/angle/surface.rs:43-59`
- Modify: `components/paint/dcomp_compositor.rs:400-408`(`storage_mode`)
- Modify: `components/shared/paint/examples/dcomp_native_poc.rs:197`
- Modify: 기동 경로(주입 지점)

**Interfaces:**
- Consumes: `gfx_dcomp_mode: String`(Task 2)
- Produces: `surfman::set_dcomp_mode(DcompMode)`, `surfman::dcomp_native_compositor_requested() -> bool`(**시그니처 불변**)

- [ ] **Step 1: 실패하는 테스트를 쓴다**

파싱이 한 곳으로 모였는지는 **순수 함수**로 검증할 수 있다. `surfman` 에 모드 파서를 두고 테스트한다.

```rust
#[test]
fn dcomp_mode_parsing_lives_in_one_place() {
    assert_eq!(DcompMode::parse("off"), DcompMode::Off);
    assert_eq!(DcompMode::parse("on"), DcompMode::Hybrid);
    assert_eq!(DcompMode::parse("surface"), DcompMode::SurfaceOnly);
    // 기존 env 문법을 그대로 받아야 한다 - 옛 스크립트가 1/true/yes 를 쓴다.
    for truthy in ["1", "true", "TRUE", "yes", "on"] {
        assert_eq!(DcompMode::parse(truthy), DcompMode::Hybrid, "{truthy}");
    }
    // 모르는 값은 Off 로 떨어지지 않는다 - 조용히 꺼지면 켰다고 믿게 된다.
    assert_eq!(DcompMode::parse("bogus"), DcompMode::Invalid);
}

#[test]
fn hybrid_and_surface_are_both_native_compositor() {
    // dcomp_native_compositor_requested() 의 기존 의미: surface 도 truthy 였다.
    assert!(DcompMode::Hybrid.native_compositor_requested());
    assert!(DcompMode::SurfaceOnly.native_compositor_requested());
    assert!(!DcompMode::Off.native_compositor_requested());
}
```

- [ ] **Step 2: 실패 확인**

```powershell
cargo test -p surfman dcomp_mode
```
기대: 컴파일 실패.

- [ ] **Step 3: 구현한다**

```rust
/// DComp 게이트 값의 3 상태. 파싱은 여기 한 곳뿐이다 - 예전에는 surfman 의
/// truthy 판정과 paint 의 surface 판정이 서로 다른 문법으로 같은 변수를 읽어,
/// 새 모드를 추가하면 두 곳을 다 고쳐야 했고 한쪽을 잊으면 조용히 하이브리드로
/// 떨어졌다.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DcompMode { Off, Hybrid, SurfaceOnly, Invalid }

static MODE: OnceLock<DcompMode> = OnceLock::new();

/// 기동 시 embedder/paint 가 pref 값으로 한 번 부른다.
pub fn set_dcomp_mode(mode: DcompMode) { let _ = MODE.set(mode); }

/// 공개 시그니처를 그대로 둔다 - 기존 호출부를 손대지 않는다.
pub fn dcomp_native_compositor_requested() -> bool {
    effective_mode().native_compositor_requested()
}

fn effective_mode() -> DcompMode {
    // 주입되지 않은 경우에만 env 로 폴백한다. surfman 단독 예제(dcomp_native_poc)가
    // servo_config 없이 도는 경로이며, 그 외에는 항상 주입된다.
    *MODE.get().unwrap_or(&env_fallback())
}
```

`Invalid` 는 경고 후 `Off` 로 취급하되 **경고를 반드시 찍는다**.

- [ ] **Step 4: paint 쪽을 유도값으로 바꾼다**

`dcomp_compositor.rs` 의 `storage_mode()` 가 env 를 읽지 않고 `surfman::effective_mode()`(또는 그것을 노출하는 공개 함수)에서 유도하게 한다. `StorageMode::{Hybrid, SurfaceOnly}` 는 그대로 두고 매핑만 한다.

- [ ] **Step 5: 주입 지점을 배선한다**

두 셸의 기동 경로에서 `prefs::get().gfx_dcomp_mode` 를 파싱해 `surfman::set_dcomp_mode()` 를 호출한다. **`RenderingContext` 생성 전에** 불러야 한다 — 창 서피스 생성 시 이미 이 값을 본다.

`dcomp_native_poc.rs:197` 의 `set_var` 는 `set_dcomp_mode(DcompMode::Hybrid)` 로 바꾼다.

- [ ] **Step 6: 통과 확인과 커밋**

```powershell
cargo test -p surfman dcomp_mode
cargo check -p servoshell
```

```
refactor(dcomp): 게이트 3 상태 파싱을 한 곳으로 모은다

surfman 의 truthy 판정과 paint 의 surface 판정이 같은 환경변수를 서로 다른
문법으로 읽고 있었다. 값은 bool 이 아니라 3 상태인데 두 함수가 나눠 읽어서,
새 모드를 추가하면 두 곳을 다 고쳐야 하고 한쪽을 잊으면 조용히 하이브리드로
떨어졌다.

DcompMode 하나로 파싱하고 pref 값을 기동 시 주입한다. surfman 은 저수준
크레이트라 servo_config 에 의존시키면 의존이 역류하므로 읽지 않고 받는다.
dcomp_native_compositor_requested 의 공개 시그니처는 그대로 두어 기존 호출부를
건드리지 않았다. 주입되지 않는 단독 예제만 env 로 폴백한다.
```

---

### Task 4: `gfx.video.*` 4 개

**Files:**
- Modify: `components/config/prefs.rs`
- Modify: `components/shared/paint/rendering_context.rs`(`video_escape_mode`)
- Modify: `components/paint/dcomp_compositor.rs`(`stable_swapchain`, `decouple_enabled`)
- Modify: `components/layout/display_list/mod.rs`(`promote_hysteresis_frames`)

**Interfaces:**
- Produces: `gfx_video_escape_mode: String`, `gfx_video_escape_stable_swapchain: bool`, `gfx_video_escape_promote_hysteresis: i64`, `gfx_video_decouple_enabled: bool`

- [ ] **Step 1: `VideoEscapeMode` 의 실제 variant 를 전부 열거한다**

★`SERVO_VIDEO_ESCAPE` 는 bool 이 아니다.★ `components/shared/paint/rendering_context.rs` 의 `video_escape_mode()` 와 `VideoEscapeMode` 정의를 읽어 **모든 variant 와 각 variant 를 고르는 env 값**을 적는다. 설계 문서가 `External` 하나만 확인했으므로 나머지는 코드에서 읽어야 한다.

- [ ] **Step 2: 실패하는 테스트를 쓴다**

```rust
#[test]
fn video_defaults_preserve_the_kill_switches() {
    let defaults = Preferences::const_default();
    // 이 둘은 기본 on 인 킬스위치다. env 미설정 = off 로 추측하면 기능이 꺼진다.
    assert!(defaults.gfx_video_decouple_enabled, "SERVO_VIDEO_DECOUPLE 은 != 0 판정이라 기본 on");
    assert!(defaults.gfx_video_escape_stable_swapchain, "as_deref() != Ok(0) 이라 기본 on");
    assert_eq!(defaults.gfx_video_escape_promote_hysteresis, 10);
}
```

- [ ] **Step 3: 실패 확인** — `cargo test -p servo_config --test config_surface`

- [ ] **Step 4: pref 추가와 호출부 교체**

Step 1 에서 확정한 문법으로 `gfx_video_escape_mode` 를 String pref 로 만든다. 파싱은 **한 곳**(`rendering_context.rs`)에서만 하고 나머지는 그 결과를 쓴다 — Task 3 에서 DComp 에 적용한 것과 같은 원칙이다.

- [ ] **Step 5: 통과 확인과 커밋**

```
feat(config): 비디오 탈출/분리 노브 4 개를 pref 로

SERVO_VIDEO_ESCAPE 는 bool 이 아니라 모드 열거형이라 String pref 로 옮기고
파싱을 rendering_context 한 곳으로 모았다.

기본값 두 개가 반직관적이라 테스트로 못박았다. SERVO_VIDEO_DECOUPLE 과
SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN 은 값이 0 이 아니면 켜지는 킬스위치라
기본이 on 이다. env 미설정 = off 로 추측했으면 기능 두 개가 꺼졌을 것이다.
```

---

### Task 5: `media.*` 9 개

**Files:**
- Modify: `components/config/prefs.rs`
- Modify: `components/media/backends/gstreamer/{player.rs,lib.rs,render.rs,webrtc.rs}`, `render-d3d11/lib.rs`
- Modify: `components/paint/painter.rs`(`SERVO_MEDIA_D3D11_VIDEO` 두 번째 읽기 지점)

**Interfaces:**
- Produces: `media_d3d11_enabled`, `media_sync_group_enabled`, `media_gapless_loop_enabled`, `media_direct_file_enabled`, `media_avdec_max_threads: i64`, `media_audio_enabled: bool`, `media_video_decoder_policy: String`, `media_video_sink_policy: String`, `media_webrtc_jitter_latency_ms: i64`

- [ ] **Step 1: 9 개의 현재 기본값과 문법을 읽어 적는다**

`SERVO_MEDIA_D3D11_VIDEO` 는 **media 와 paint 두 곳에서 읽힌다.** 두 판정이 다르면 **고치지 말고 보고하라.**

정책 두 개(`VIDEO_DECODER_POLICY`, `VIDEO_SINK_POLICY`)는 문자열 값이므로 **인정하는 값 전부**를 열거한다.

- [ ] **Step 2: 실패하는 테스트를 쓴다**

```rust
#[test]
fn disable_audio_is_inverted_into_a_positive_pref() {
    let defaults = Preferences::const_default();
    // SERVO_GSTREAMER_DISABLE_AUDIO 는 부정형이었다. servo 관례가 *_enabled
    // 긍정형이고 이중부정은 실수의 단골이라 뒤집는다. 기본은 오디오 켜짐.
    assert!(defaults.media_audio_enabled);
}
```

- [ ] **Step 3: 실패 확인** — `cargo test -p servo_config --test config_surface`

- [ ] **Step 4: pref 추가와 호출부 교체**

`media_audio_enabled` 는 **의미가 뒤집힌다**. 호출부에서 `if disable_audio` → `if !prefs.media_audio_enabled` 로 바꾸되, **부정이 두 번 겹치지 않도록** 조건을 다시 읽어 확인한다.

- [ ] **Step 5: 미디어는 `mach build` 로 검증한다**

★`cargo build -p servoshell` 로는 미디어 경로가 더미 백엔드로 빠진다.★ 미디어 관련 변경은 **full `mach build` 가 필수**다.

```powershell
. .\scripts\servo_env.ps1
.\mach build -j 8
```

- [ ] **Step 6: 커밋**

```
feat(config): 미디어 운용 노브 9 개를 pref 로

SERVO_GSTREAMER_DISABLE_AUDIO 는 media.audio.enabled 로 뒤집었다. servo 관례가
긍정형이고 이중부정은 실수의 단골 자리다 - 의미가 뒤집히는 변경이라 마이그레이션
표에 별도로 적었다.

SERVO_MEDIA_D3D11_VIDEO 는 media 와 paint 두 크레이트가 각자 읽고 있었다.
이제 한 pref 를 본다.
```

---

### Task 6: 제거된 env 는 기동을 막는다

**Files:**
- Create: `components/config/removed_env.rs`
- Modify: `components/config/lib.rs`
- Modify: 두 셸의 기동 경로

**Interfaces:**
- Produces: `servo_config::removed_env::check() -> Result<(), Vec<String>>`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
#[test]
fn a_removed_env_name_names_its_replacement() {
    let entry = removed_env::lookup("SERVO_COMPOSITOR_DCOMP").expect("등록돼 있어야 한다");
    assert!(entry.message.contains("gfx.dcomp.mode"), "무엇으로 바꿀지 알려줘야 한다: {}", entry.message);
}

#[test]
fn every_migrated_env_is_listed_as_removed() {
    // pref 로 옮긴 19 개가 전부 차단 목록에 있어야 한다. 하나라도 빠지면
    // 그 스크립트는 조용히 옛 설정을 잃는다.
    for name in MIGRATED_19 {
        assert!(removed_env::lookup(name).is_some(), "{name} 이 차단 목록에 없다");
    }
}
```

`MIGRATED_19` 는 테스트 파일에 명시적으로 적는다(설계 문서 §4 표 그대로).

- [ ] **Step 2: 실패 확인** — `cargo test -p servo_config --test config_surface`

- [ ] **Step 3: 구현한다**

```rust
/// pref 로 옮겨 더 이상 읽지 않는 환경변수. 설정돼 있으면 기동을 막는다.
///
/// 경고 후 통과가 아니다 - 켰다고 믿는데 실제로는 안 켜진 상태가 아예 안 켠
/// 것보다 나쁘기 때문이다(wall_view 인증 설정에서 같은 판단을 했다).
pub struct RemovedEnv { pub name: &'static str, pub message: &'static str }

pub const REMOVED: &[RemovedEnv] = &[
    RemovedEnv {
        name: "SERVO_COMPOSITOR_DCOMP",
        message: "use --pref gfx.dcomp.mode=surface (or gfx.dcomp.mode=on); \
                  see docs/multigpu/configuration.md",
    },
    // … 19 개 …
];

pub fn check() -> Result<(), Vec<String>> { /* 설정된 것들을 모아 Err */ }
```

- [ ] **Step 4: 기동 경로에 건다**

두 셸에서 **인자 파싱 직후, 파이프라인/창 생성 전에** 부른다. 실패 시 메시지 전량을 찍고 종료한다.

- [ ] **Step 5: 실기 확인**

```powershell
$env:SERVO_COMPOSITOR_DCOMP="1"
target\debug\servoshell.exe --help
```
기대: 기동이 막히고 `gfx.dcomp.mode` 를 안내하는 메시지. 확인 후 `Remove-Item Env:SERVO_COMPOSITOR_DCOMP`.

- [ ] **Step 6: 커밋**

```
feat(config): pref 로 옮긴 환경변수가 설정돼 있으면 기동을 막는다

그냥 삭제하면 기존 스크립트가 조용히 옛 설정을 잃고, 계속 인정하면 정본이
둘이 되어 없애려던 표류가 남는다. 무엇을 무엇으로 바꾸라고 알려주고 멈추는
편이 낫다 - wall_view 인증 설정에서 이미 같은 판단을 했다.

차단 목록은 한시적이다. 옮긴 19 개에 대해서만 두고 정리가 안정되면 걷어낸다.
```

---

### Task 7: 두 셸의 `WallArgs` 공유

**Files:**
- Create: `components/shared/paint/wall_args.rs`
- Modify: `components/shared/paint/lib.rs`
- Modify: `ports/servoshell/prefs.rs:306-320,712-730`
- Modify: `components/servo/examples/winit_wall/main.rs:85-131`

**Interfaces:**
- Produces: `paint_api::wall_args::{WallArgs, WallArgsError}`, `WallArgs::validate()`, `WallArgs::resolve() -> Result<Option<WallLayout>, WallArgsError>`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
#[test]
fn tile_index_without_a_layout_is_rejected() {
    let args = WallArgs { layout: None, tile_index: 2, all_tiles: false };
    assert!(matches!(args.validate(), Err(WallArgsError::TileIndexWithoutLayout)));
}

#[test]
fn all_tiles_makes_the_tile_index_meaningless() {
    let args = WallArgs { layout: Some("x.json".into()), tile_index: 3, all_tiles: true };
    assert!(matches!(args.validate(), Err(WallArgsError::TileIndexWithAllTiles)));
}
```

지금 servoshell 은 이 두 경우를 `warn!` 으로 넘기고 winit_wall 은 아예 검사하지 않는다. **경고를 오류로 올릴지는 기존 동작을 확인한 뒤 정하라** — 올린다면 그 사실을 보고하라(동작 변경이다).

- [ ] **Step 2: 실패 확인** — `cargo test -p servo-paint-api wall_args`

- [ ] **Step 3: 구현하고 두 셸을 그 위에 얹는다**

파서는 통일하지 않는다(비목표). servoshell 은 bpaf 로, winit_wall 은 기존 `match arg` 루프로 `WallArgs` 를 **채우기만** 하고 검증·해석은 공유 코드가 한다.

- [ ] **Step 4: 기존 `wall_layout` 테스트가 깨지지 않는지 확인한다**

```powershell
cargo test -p servo-paint-api wall_layout --lib
```
기대: 기존 13 개 + 신규 통과.

- [ ] **Step 5: 커밋**

```
refactor(wall): 두 셸이 wall 인자 검증을 공유한다

같은 플래그를 servoshell 과 winit_wall 이 따로 파싱해 이미 갈라져 있었다.
파서는 그대로 두고(bpaf 대 수제 루프) 검증과 해석만 공유한다. servoshell 에만
있던 규칙이 winit_wall 에도 생긴다.
```

---

### Task 8: `configuration.md` 와 문서 표류 방지

**Files:**
- Create: `docs/multigpu/configuration.md`
- Modify: `components/config/tests/config_surface.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
#[test]
fn every_knob_appears_in_the_configuration_doc() {
    let doc = std::fs::read_to_string(repo_root().join("docs/multigpu/configuration.md")).unwrap();
    let missing: Vec<&str> = debug_env::ALL.iter().map(|f| f.name)
        .filter(|name| !doc.contains(*name)).collect();
    assert!(missing.is_empty(), "문서에 없는 노브: {missing:?}");
}

#[test]
fn the_doc_check_actually_catches_a_missing_entry() {
    // 검사가 느슨해져 통과만 하는 것을 막는다 - wall_view 에서 자체완결성 검사가
    // 두 번 그렇게 무너졌다.
    let synthetic = "SERVO_DCOMP_READBACK 만 적힌 문서";
    assert!(!names_missing_from(synthetic, debug_env::ALL).is_empty());
}
```

`names_missing_from(doc: &str, flags: &[&DebugFlag]) -> Vec<&'static str>` 은 앞 테스트의 필터를 순수 함수로 분리한 것이다. 같은 파일에 정의하고 두 테스트가 공유한다 — 검사 본체와 "검사가 무엇을 잡는지" 를 같은 코드로 확인해야 의미가 있다.

- [ ] **Step 2: 실패 확인** — `cargo test -p servo_config --test config_surface`

- [ ] **Step 3: 문서를 쓴다**

pref 19 개(이름·타입·기본값·설명)와 조사 노브 17 개(이름·종류·설명) 표. `debug_env` 의 `doc` 필드를 그대로 옮긴다. 마이그레이션 표(옛 env → 새 pref)도 포함하고, **의미가 뒤집힌 `media.audio.enabled`** 와 **`=0` 이 이제 진짜 off 로 동작**하는 두 가지를 눈에 띄게 적는다.

- [ ] **Step 4: 통과 확인과 커밋**

```
docs(multigpu): 설정 노브 전량 표와 문서 표류 방지 테스트

노브를 추가하고 문서를 잊는 것을 테스트로 막는다. 검사 자체가 느슨해지는
함정을 wall_view 에서 두 번 겪었으므로, 검사가 무엇을 잡는지도 함께 고정했다.
```

---

### Task 9: 죽은 노브 삭제

**Files:** 삭제 대상에 따라 결정된다.

- [ ] **Step 1: 삭제 후보와 근거를 모은다**

기준(설계 문서 §10):

> 조사가 종결됐고 그 결론이 "미적용" 이거나 기본값으로 확정된 노브는 삭제한다. **분기 자체를 코드에서 걷어낸다.**

각 후보에 대해 **어느 문서 어느 절이 종결을 말하는지**를 적는다. `docs/multigpu/*.md` 와 `docs/ai-notes.md` 를 근거로 삼는다. 근거를 못 찾으면 후보에서 뺀다.

- [ ] **Step 2: 목록을 제시하고 확인받는다**

★사용자 확인 없이 삭제하지 마라.★ 후보와 근거를 표로 제시한다.

- [ ] **Step 3: 승인된 것만 삭제한다**

env 읽기, `debug_env` 항목, 문서 행, 그리고 **그 노브가 게이트하던 분기 자체**를 함께 지운다. 값을 고정한 채 분기만 남기면 죽은 코드가 남는다.

- [ ] **Step 4: 양방향 테스트가 통과하는지 확인한다**

`every_registered_flag_is_actually_read_somewhere` 가 삭제 누락을 잡아 준다.

- [ ] **Step 5: 커밋**

```
refactor(config): 조사가 끝난 노브를 걷어낸다

각 항목의 종결 근거를 커밋 본문에 적는다.
```

---

## 검증 (전체)

- `cargo test -p servo_config`, `cargo test -p servo-paint-api`, `cargo test -p surfman dcomp_mode`
- `cargo check -p servoshell`
- `cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl --release`
- **미디어 변경이 포함되면 `.\mach build -j 8`** (cargo `-p` 로는 더미 백엔드로 빠진다)
- 옮긴 pref 중 최소 다음 둘은 **옛 env 와 새 pref 의 동작이 같은지** 스모크 1 회씩: `gfx.dcomp.mode=surface`, `media.d3d11.enabled=true`
- 제거된 env 를 설정한 상태에서 기동이 실제로 막히는지
- `rustfmt --edition 2024 --check <손댄 .rs>`, `git diff --check`

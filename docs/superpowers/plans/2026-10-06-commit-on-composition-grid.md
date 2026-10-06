# B3 — 커밋을 공통 합성 격자 위 고정 위상에 내보낸다 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** DComp `Commit()` 이 나가는 시각을 렌더 패스의 길이에서 떼어, DWM 공통 합성 격자 위의 고정된 위상에 앉힌다.

**Architecture:** 마감을 `틱 직후의 첫 CommitAlign% 격자점`으로 계산해 기존 `commit_scheduler` 워커에 넘긴다. 접는 산식은 시계도 COM 도 만지지 않는 순수 함수로 떼어 단위 테스트한다. 스케줄러 스레드·큐·`upsert`·`device_guard`·폴백 경로는 건드리지 않는다 — 바뀌는 것은 **마감을 누가 어떻게 계산하느냐** 하나다.

**Tech Stack:** Rust 1.95.0 (in-tree rustup toolchain), Windows 전용(`#[cfg(windows)]`), DWM/DirectComposition, `servo-paint` 크레이트.

**Spec:** `docs/superpowers/specs/2026-10-06-commit-on-composition-grid-design.md`

## Global Constraints

- 플랫폼: Windows 전용. 손대는 모듈 전부가 `#[cfg(windows)] mod` 안이다(`components/paint/lib.rs:67-74`).
- `components/paint/lib.rs` 는 `#![deny(unsafe_code)]`. 이 작업에서 새 `unsafe` 는 필요 없다.
- **"꺼져 있으면 오늘과 같다"가 이 작업 전체의 전제다.** `CommitAlign = -1`(기본)에서 동작·비용이 모두 오늘과 같아야 한다.
- pref 를 묻는 비용은 0 이어야 한다. `pref!` 는 RwLock 획득이고 이 값을 묻는 자리가 벽의 가장 뜨거운 경로에 있다 — 반드시 `LazyLock` 으로 굳힌다(기존 `ALIGN_PCT` 와 같은 방식·같은 이유).
- 마감은 틱으로부터 **최대 한 주기** 뒤여야 한다. 그보다 멀면 다음 스케줄이 먼저 도착해 `upsert` 가 계속 뒤로 밀고, 한 번 걸린 타일은 영원히 걸린다(`log_ani_debug_02/03`: DISPLAY22 가 초당 60건을 스케줄하고 8건만 커밋).
- 커밋 실패 로그와 계측은 **디바이스 가드 밖**에서 찍는다. 임계구역에 남는 것은 `Commit()` 하나여야 한다(규칙 C3).
- `output_grid::grid_for_monitor` 와 프로브 스레드는 **지우지 않는다**. `OUTPHASE`/`vs_comp_ms` 가 거기서 나오고 그것이 위상 법칙의 계측이다.
- 커밋 메시지·코드 주석·문서는 기존 평어체를 유지한다.
- PowerShell 5.1 에서 `cargo`/`git` 뒤에 `2>&1` 또는 `2>$null` 을 붙이지 않는다 — 성공이 실패로 보인다.

## Review Focus

스펙이 함축하지만 어느 태스크의 기본 테스트도 건드리지 않는 입력들. 다섯 줄 전부 Task 1 의 테스트로 못 박는다.

1. **`vblank` 가 `tick` 보다 미래다.** 드라이버에 따라 `qpcVBlank` 는 직전일 수도 다음일 수도 있다. 음수 나머지를 접지 않으면 마감이 과거로 나와 매 프레임 즉시 커밋이 된다 — 기능이 켜진 채로 아무 일도 안 한다.
2. **`tick` 이 목표 격자점 **정확히** 위다.** 0 을 돌려주면 그 패스가 돌기도 전에 커밋해 이전 프레임을 내보낸다. 한 주기 뒤여야 한다.
3. **`pct` 가 0 또는 99.** 경계에서 접기가 한 칸 어긋나면 커밋이 엉뚱한 주기에 떨어진다.
4. **`period == 0`.** DWM 이 0 을 돌려주는 경우가 있다(`composition_grid` 이 이미 그때 `None` 을 낸다). 나눗셈/나머지가 패닉하면 벽이 죽는다.
5. **주사율이 실행 중에 바뀐다.** 격자를 매 패스 다시 조회하므로 다음 패스부터 새 주기를 따라야 한다 — 옛 주기로 고착되면 마감이 영원히 틀린다.

---

## 빌드·테스트 환경 (먼저 한 번)

이 저장소는 git worktree 이고 툴체인·의존성은 본 체크아웃에 있다. 아래를 `build.cmd` 로 저장해 쓴다 (저장소 밖, 예: `%TEMP%\build.cmd`).

```bat
@echo off
set "MAIN=F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\servo"
set "REPO=W:\servo_multigpu-tiled-wall"
set "GST=F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64"
set "RUSTUP_HOME=%MAIN%\.rustup"
set "CARGO_HOME=%MAIN%\.servo\cargo-home"
set "GSTREAMER_1_0_ROOT_MSVC_X86_64=%GST%"
set "PKG_CONFIG_PATH=F:/gstreamer-inhouse/1.28.4.100/1.0/msvc_x86_64/lib/pkgconfig"
set "PYTHONUTF8=1"
set "PYTHONIOENCODING=utf-8"
set "CC=clang-cl.exe"
set "CXX=clang-cl.exe"
call "C:\Program Files\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvars64.bat" >nul
if errorlevel 1 exit /b 1
set "PATH=%GST%\bin;%REPO%\.venv\Scripts;C:\Program Files\LLVM\bin;%MAIN%\.rustup\toolchains\1.95.0-x86_64-pc-windows-msvc\bin;%PATH%"
cd /d %REPO%
cargo %*
```

★`PKG_CONFIG_PATH` 는 슬래시다★ — 역슬래시는 pkg-config 의 `${pcfiledir}` 전개에서 먹힌다.

세 명령:

```
build.cmd test  --release -p servo-paint -p servo --features servo/no-wgl,servo/media-gstreamer,servo/webgpu --lib
build.cmd build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
build.cmd fmt   -p servo -p servo-paint --check
```

왰미트 메시지는 파일로 넘긴다(`git commit -F`). ★PowerShell 5.1 의 `Out-File -Encoding utf8` 은
BOM 을 붙인다★ -- 그러면 커밋 제목 앞에 보이지 않는 문자가 들어간다. 메모장이나
`git commit` 의 에디터를 쓰거나, BOM 없는 UTF-8 로 쓰는 도구를 쓴다.

★`fmt --check` 는 깨끗하지 않다.★ 이 저장소는 rustfmt-clean 이 아니다. 작업 전 기준선을 재고(현재 `output_grid.rs` 5건, `paint.rs` 1건, `commit_scheduler.rs`·`dcomp_compositor.rs`·`prefs.rs` 는 재서 적어 둘 것) **자기가 만든 diff 만** 고친다.

---

## 파일 구조

| 파일 | 이 작업에서의 책임 |
|---|---|
| `components/paint/output_grid.rs` | 접기 산식(`next_grid_point`)과 틱 시각 접근자(`tick_qpc`). 순수 함수와 그 테스트가 여기 산다 — 이미 `composition_target` 테스트가 있는 자리다. |
| `components/paint/paint.rs` | 세 입력(pref·격자·틱)을 모아 `commit_deadline()` 을 만들고, `flush_deferred_dcomp_commits` 가 패스당 한 번 쓴다. |
| `components/paint/commit_scheduler.rs` | pref 캐시 이름과 출처. 스케줄러 본체는 불변. |
| `components/paint/dcomp_compositor.rs` | 디바이스 가드 4곳의 pref 캐시 이름만. |
| `components/config/prefs.rs` | pref 선언 교체. |
| `etc/multigpu/run_wall_dist.ps1` | `-CommitAlign` 파라미터·검증·요약줄. |

---

### Task 1: `next_grid_point` — 접기 산식과 틱 접근자

순수 함수와 테스트만. **아무도 아직 부르지 않는다** — 이 태스크가 끝나도 동작은 오늘과 같다.

**Files:**
- Modify: `components/paint/output_grid.rs` (`note_present_tick_now` 아래에 함수 둘, 파일 끝 `mod tests` 에 테스트)

**Interfaces:**
- Consumes: 기존 `static TICK_QPC: AtomicU64`
- Produces:
  - `pub(crate) fn next_grid_point(tick: u64, vblank: u64, period: u64, pct: u64) -> Option<u64>`
  - `pub(crate) fn tick_qpc() -> Option<u64>`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`components/paint/output_grid.rs` 의 `mod tests` 안. `use super::{composition_target, period_from_pair};` 를 `use super::{composition_target, next_grid_point, period_from_pair};` 로 고친 뒤 아래를 추가한다.

```rust
/// 10MHz QPC 에서 60Hz 한 주기. 실기 값(`COMPWALK period_ms=16.667`)과 같은 규모다.
const P: u64 = 166_667;
/// 임의의 vblank 기준점. 절대값은 결과에 영향이 없어야 한다.
const V: u64 = 1_000_000_000;

/// 목표점 바로 앞의 틱은 **그** 격자점을 겨냥한다.
#[test]
fn a_tick_just_before_the_target_aims_at_it() {
    let base = V + P * 25 / 100;
    assert_eq!(next_grid_point(base - 1, V, P, 25), Some(base));
}

/// 목표점을 막 지난 틱은 **다음** 격자점을 겨냥한다.
#[test]
fn a_tick_just_past_the_target_aims_at_the_next_one() {
    let base = V + P * 25 / 100;
    assert_eq!(next_grid_point(base + 1, V, P, 25), Some(base + P));
}

/// ★목표점 정확히 위의 틱은 한 주기 뒤다 -- 0 이 아니다.★
///
/// 틱 시점에는 그 패스가 아직 돌지도 않았다. 그 자리에서 커밋하면 이전 프레임을 내보낸다.
#[test]
fn a_tick_exactly_on_the_target_waits_a_full_period() {
    let base = V + P * 25 / 100;
    assert_eq!(next_grid_point(base, V, P, 25), Some(base + P));
}

/// 몇 주기가 지났든 바로 다음 격자점 하나만 건너뛴다.
#[test]
fn many_periods_later_still_lands_on_the_very_next_point() {
    let base = V + P * 25 / 100;
    assert_eq!(next_grid_point(base + 3 * P + 5, V, P, 25), Some(base + 4 * P));
}

/// ★`vblank` 가 틱보다 **미래**일 수 있다.★ (Review Focus 1)
///
/// `qpcVBlank` 는 드라이버에 따라 직전일 수도 다음일 수도 된다. 음수 나머지를 접지 않으면
/// 마감이 과거로 나오고, 그러면 매 프레임 즉시 커밋이 되어 기능이 켜진 채 아무 일도 안 한다.
#[test]
fn a_vblank_in_the_future_of_the_tick_still_folds_correctly() {
    // 틱이 vblank 보다 반 주기 **앞**에 있다. pct=0 이므로 목표점은 vblank 격자 자체다.
    assert_eq!(next_grid_point(V - P / 2, V, P, 0), Some(V));
    // 두 주기도 더 앞이어도 같은 격자 위의 다음 점으로 간다.
    assert_eq!(next_grid_point(V - 2 * P - 7, V, P, 0), Some(V - 2 * P));
}

/// `pct` 경계. (Review Focus 3)
#[test]
fn pct_boundaries_land_on_the_right_point() {
    // pct = 0 -> vblank 격자점 자체.
    assert_eq!(next_grid_point(V - 1, V, P, 0), Some(V));
    // pct = 99 -> 주기의 99% 지점.
    let base99 = V + P * 99 / 100;
    assert_eq!(next_grid_point(base99 - 1, V, P, 99), Some(base99));
}

/// ★`period == 0` 에 패닉하지 않는다.★ (Review Focus 4)
#[test]
fn a_zero_period_yields_none_instead_of_dividing_by_zero() {
    assert_eq!(next_grid_point(V, V, 0, 25), None);
}

/// ★주사율이 바뀌면 다음 호출부터 새 주기를 따른다.★ (Review Focus 5)
///
/// 격자는 패스마다 다시 조회되므로 이 함수는 상태를 들지 않아야 한다. 같은 틱에 다른
/// 주기를 주면 다른 답이 나와야 한다 -- 같으면 어딘가에 옛 주기가 남은 것이다.
#[test]
fn a_changed_period_is_followed_immediately() {
    const P75: u64 = 133_333; // 75Hz
    let tick = V + 1_000;
    let at60 = next_grid_point(tick, V, P, 50).expect("60Hz");
    let at75 = next_grid_point(tick, V, P75, 50).expect("75Hz");
    assert_ne!(at60, at75);
    assert_eq!(at60, V + P * 50 / 100);
    assert_eq!(at75, V + P75 * 50 / 100);
}
```

- [ ] **Step 2: 실패를 확인한다**

```
build.cmd test --release -p servo-paint --features servo/no-wgl --lib next_grid_point
```

Expected: 컴파일 실패 — ``cannot find function `next_grid_point` in module `super` ``

- [ ] **Step 3: 최소 구현을 쓴다**

`components/paint/output_grid.rs` 의 `note_present_tick_now` 함수 **바로 아래**에 추가한다.

```rust
/// 이 패스의 틱 시각(QPC). 아직 틱이 없었으면 `None`.
///
/// 0 을 "없음" 으로 쓰는 것이 안전한 이유: QPC 는 부팅부터 단조 증가하고, 0 은 부팅
/// 순간뿐이다. 그 시각에 벽이 프레임을 내고 있을 수는 없다.
pub(crate) fn tick_qpc() -> Option<u64> {
    let tick = TICK_QPC.load(Ordering::Relaxed);
    (tick != 0).then_some(tick)
}

/// ★`tick` 직후의 첫 `pct%` 격자점.★ 순수 함수 -- 시계도 COM 도 만지지 않는다.
///
/// `vblank`/`period` 는 `DwmGetCompositionTimingInfo` 가 준 **공통** 합성 격자이고, `pct` 는
/// 주기 안에서 겨냥할 백분율이다. 돌려주는 것은 그 격자점의 절대 QPC 다.
///
/// ★"직후" 는 엄격하다.★ `tick` 이 정확히 목표점 위에 있으면 한 주기 뒤를 돌려준다 -- 틱
/// 시점에는 그 패스가 아직 돌지도 않았으므로, 그 자리에서 커밋하면 **이전 프레임**을
/// 내보내게 된다.
///
/// `vblank` 는 드라이버에 따라 `tick` 보다 과거일 수도 미래일 수도 있다. 나머지 연산을 두 번
/// 걸어 어느 쪽이든 격자 위의 같은 점으로 접는다(`snap_to_dwm_grid_at` 과 같은 이유).
///
/// 돌려주는 값은 `tick` 으로부터 **최대 한 주기** 뒤다. 그보다 멀면 다음 스케줄이 먼저
/// 도착해 `commit_scheduler::upsert` 가 마감을 계속 뒤로 밀고, 한 번 걸린 타일은 영원히
/// 걸린다(`log_ani_debug_02/03` 에서 DISPLAY22 가 초당 60 건 중 8 건만 커밋했다).
pub(crate) fn next_grid_point(tick: u64, vblank: u64, period: u64, pct: u64) -> Option<u64> {
    if period == 0 {
        return None;
    }
    // `period` 는 한 주기의 QPC 틱 수(10MHz 에서 ~1.7e5)이고 `pct <= 99` 이므로 u64 에서
    // 넘치지 않는다.
    let base = vblank.wrapping_add(period * pct / 100);
    let period_i = period as i128;
    let delta = tick as i128 - base as i128;
    let ahead = period_i - (((delta % period_i) + period_i) % period_i);
    Some(tick.wrapping_add(ahead as u64))
}
```

- [ ] **Step 4: 통과를 확인한다**

```
build.cmd test --release -p servo-paint --features servo/no-wgl --lib next_grid_point
```

Expected: `test result: ok. 8 passed`

- [ ] **Step 5: 전체 테스트와 포맷을 확인한다**

```
build.cmd test --release -p servo-paint -p servo --features servo/no-wgl,servo/media-gstreamer,servo/webgpu --lib
build.cmd fmt  -p servo -p servo-paint --check
```

Expected: 기존 48+3 에 새 테스트가 더해져 전부 통과. `output_grid.rs` 의 fmt diff 수가 작업 전 기준선(5)과 같아야 한다 — 늘었으면 자기 코드를 rustfmt 제안대로 고친다.

- [ ] **Step 6: 커밋**

```bash
git add components/paint/output_grid.rs
git commit -F "%TEMP%\msg.txt"
```

메시지 (제목은 그대로, 본문은 실제로 한 일에 맞게 다듬을 것):

```
feat(wall): 커밋 마감의 접기 산식을 순수 함수로 떼어 테스트한다

B3 의 첫 조각이다. `next_grid_point(tick, vblank, period, pct)` 은 틱 직후의 첫
`pct%` 격자점을 돌려준다. 시계도 COM 도 만지지 않으므로 단위 테스트로 전부 덮인다.

★아직 아무도 부르지 않는다 -- 동작은 오늘과 같다.★

테스트가 못 박는 것:
* `vblank` 가 틱보다 **미래**일 수 있다(드라이버마다 다르다). 음수 나머지를 안 접으면
  마감이 과거로 나와 매 프레임 즉시 커밋이 된다 -- 기능이 켜진 채 아무 일도 안 한다.
* 틱이 목표점 정확히 위면 한 주기 뒤다. 0 을 돌려주면 그 패스가 돌기도 전에 커밋해
  이전 프레임을 내보낸다.
* `period == 0` 에 패닉하지 않는다.
* 주사율이 바뀌면 다음 호출부터 새 주기를 따른다(상태를 들지 않는다).

마감은 틱으로부터 최대 한 주기 뒤다. 그보다 멀면 다음 스케줄이 먼저 도착해 `upsert` 가
계속 뒤로 밀고, 한 번 걸린 타일은 영원히 걸린다(log_ani_debug_02/03 에서 DISPLAY22 가
초당 60 건 중 8 건만 커밋했다).
```

---

### Task 2: 마감을 공통 격자에서 계산한다

동작이 바뀌는 태스크다. pref **이름은 아직 `gfx_present_align_per_output_pct` 그대로** 두고 의미만 바꾼다 — 이름 변경은 Task 3 이다. 그래야 이 태스크가 끝난 시점에 실기로 바로 측정할 수 있다(`-PerOutputAlign 25` 가 곧 새 동작이다).

**Files:**
- Modify: `components/paint/paint.rs` — `deadline_for_monitor` 를 `commit_deadline` 으로 교체, `flush_deferred_dcomp_commits` 의 스케줄 루프

**Interfaces:**
- Consumes: `output_grid::next_grid_point`, `output_grid::tick_qpc` (Task 1), 기존 `dcomp_compositor::composition_grid() -> Option<(u64, u64)>`, 기존 `commit_scheduler::ALIGN_PCT`
- Produces: `fn commit_deadline() -> Option<u64>` (`impl Paint` 의 연관 함수, `#[cfg(windows)]`)

- [ ] **Step 1: `deadline_for_monitor` 를 `commit_deadline` 으로 바꾼다**

`components/paint/paint.rs` 의 `#[cfg(windows)] fn deadline_for_monitor(monitor: usize) -> Option<u64> { ... }` 전체를 아래로 **교체**한다.

```rust
    /// ★이 패스의 커밋이 떨어져야 할 격자점(절대 QPC).★
    ///
    /// 커밋은 지금까지 렌더 패스의 **끝**에서 나갔다. `-DwmAlign` 이 고정하는 것은 패스의
    /// **시작**(틱)이므로 커밋 시각 = `틱 + 패스 길이` 였고, 커밋 위상이 패스 길이를 그대로
    /// 물려받았다. 그 산포가 저더의 두 증상을 모두 만든다 -- 큰 탈선은 합성 마감을 스치고
    /// (`MISSEVENT nc=1`), 그 흔들림이 커밋 간격을 흔들어 합성 경계를 넘나든다(`nc=0`).
    /// 설계 문서 §21~27 과 `2026-10-06-commit-on-composition-grid-design.md` §1.
    ///
    /// ★타일과 무관하다 -- 패스당 하나다.★ DWM 은 네 타일을 하나의 합성 패스에서 함께
    /// 올리므로(네 디바이스의 `lastFrameTime` 이 동일하다) 맞출 격자도 하나다. 예전
    /// `deadline_for_monitor` 는 출력별 격자에 맞췄고, 그것이 B1 이 효과를 내지 못한 이유다.
    ///
    /// 묵은 격자 검사가 없는 것이 맞다. `grid_for_monitor` 는 프로브 스레드의 캐시라 최대
    /// ~100ms 낡을 수 있어 검사가 필요했지만, `composition_grid()` 는 매번 조회하는 시스템
    /// 콜이라 묵을 수가 없다. 들고 가면 영원히 거짓인 분기가 남는다.
    #[cfg(windows)]
    fn commit_deadline() -> Option<u64> {
        // 캐시된 값이다 -- `pref!` 는 RwLock 획득이고 이 함수는 패스마다 돈다
        // (`commit_scheduler::ALIGN_PCT` 주석).
        let pct = (*crate::commit_scheduler::ALIGN_PCT)?;
        let (vblank, period) = crate::dcomp_compositor::composition_grid()?;
        let tick = crate::output_grid::tick_qpc()?;
        crate::output_grid::next_grid_point(tick, vblank, period, pct)
    }
```

- [ ] **Step 2: flush 의 스케줄 루프를 고친다**

같은 파일 `flush_deferred_dcomp_commits` 안. `let mut fallback: Vec<usize> = Vec::new();` 부터 그 `for` 루프 끝까지를 아래로 교체한다.

```rust
            let mut fallback: Vec<usize> = Vec::new();
            // ★마감은 패스당 하나다.★ 공통 격자에서 나오므로 타일마다 다시 셀 것이 없고,
            // 네 타일이 **같은** 마감을 받아야 한 합성에 함께 실린다. 루프 안에서 재면
            // 타일마다 조회 시각이 달라 미세하게 다른 답이 나온다.
            let deadline = Self::commit_deadline();
            for painter_id in self.painter_ids() {
                let pending = self
                    .with_painter(painter_id, |painter| {
                        painter
                            .take_pending_dcomp_commit()
                            .map(|device| (device, painter.pending_dcomp_commit_monitor()))
                    })
                    .flatten();
                let Some((device, monitor)) = pending else {
                    continue;
                };
                // `monitor` 는 이제 마감 계산에 쓰이지 않는다 -- `OUTCOMMIT` 이 "어느 타일이
                // 죽었나" 를 말할 수 있게 하는 기록용으로만 스케줄러에 넘긴다
                // (`output_grid::DEVICE_MONITOR` 주석).
                match (monitor, deadline) {
                    (Some(monitor), Some(deadline)) => {
                        crate::commit_scheduler::schedule(device, monitor, deadline)
                    },
                    _ => fallback.push(device),
                }
            }
```

★마감 초과에 쓸 코드는 없다.★ `schedule()` 이 이미 지난 마감을 받으면 스케줄러의 기존
`deadline <= now` 경로가 다음 깨어남에 바로 커밋한다. 그리고 그 초과는 스케줄러가 이미
재는 `SLIP_N`/`SLIP_SUM`/`SLIP_MAX`(= `now - deadline`)로 `OUTCOMMIT` 에 나온다 -- 새
카운터도 두지 않는다.


- [ ] **Step 3: 빌드하고 경고를 확인한다**

```
build.cmd build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
```

Expected: 성공. `paint.rs` 에서 나오는 경고 0 — 특히 `grid_for_monitor` 가 이제 `paint.rs` 에서 안 쓰이지만 `output_grid` 의 프로브가 계속 쓰므로 dead-code 경고가 **나오면 안 된다**. 나오면 프로브 쪽에서 쓰이지 않게 된 것이니 멈추고 확인한다.

- [ ] **Step 4: 테스트와 포맷**

```
build.cmd test --release -p servo-paint -p servo --features servo/no-wgl,servo/media-gstreamer,servo/webgpu --lib
build.cmd fmt  -p servo -p servo-paint --check
```

Expected: 전부 통과. `paint.rs` fmt diff 가 기준선과 같아야 한다.

- [ ] **Step 5: 커밋**

```bash
git add components/paint/paint.rs
git commit -F "%TEMP%\msg.txt"
```

메시지 (제목은 그대로, 본문은 실제로 한 일에 맞게 다듬을 것):

```
feat(wall): 커밋 마감을 공통 합성 격자 위 고정 위상으로 옮긴다

커밋은 그동안 렌더 패스의 **끝**에서 나갔다. `-DwmAlign` 이 고정하는 것은 패스의
**시작**이므로 커밋 위상이 패스 길이를 그대로 물려받았고, 그 산포가 저더의 두 증상을
모두 만든다(`MISSEVENT nc=1` 마감 스침, `nc=0` 합성 경계 넘나듬). 설계 문서 §21~27.

  deadline = 틱 직후의 첫 pct% 격자점        (공통 DWM 격자)

★타일과 무관하다 -- 패스당 하나다.★ DWM 은 네 타일을 하나의 합성 패스에서 함께
올리므로 맞출 격자도 하나다. 예전 `deadline_for_monitor` 는 출력별 격자에 맞춤고,
그것이 B1 이 효과를 내지 못한 이유다. 네 타일이 **같은** 마감을 받아야 한 합성에
함께 실린다.

묵은 격자 검사를 지웭다. `grid_for_monitor` 는 프로브 스레드의 캐시라 최대 ~100ms 낡을
수 있어 검사가 필요했지만, `composition_grid()` 는 매번 조회하는 시스템 콜이라 묵을 수가
없다. 들고 가면 영원히 거짓인 분기가 남는다.

마감을 놓치면 즉시 커밋한다 -- ★새 코드가 없다★. 스케줄러의 기존 `deadline <= now`
경로가 그대로 한다. 보류하면 위상은 완벽하지만 프레임이 한 주기 늦고 기아 되먹임이
되살아난다.

pref 이름은 아직 `gfx_present_align_per_output_pct` 그대로다 -- 다음 커밋에서 바꿈다.
이 상태에서 이미 `-PerOutputAlign 25` 로 실기 측정이 된다.
```

- [ ] **Step 6: 실기로 한 번 본다 (선택이지만 강력 권장)**

`-PerOutputAlign 25 -DwmAlign 8 -SampleLead 1` 로 프로브 페이지를 띄우고 로그에서 확인한다:

| 지표 | 기대 |
|---|---|
| `TICKCOMMIT ... spread=` | ★0 에 가깝게★ (기준 0.70) — **이것이 성공의 정의다** |
| `OUTCOMMIT ... slip_us` | 마감 초과의 빈도·크기 |
| `MISSEVENT ... nc=0 / nc=1` | 둘 다 감소 |

`spread` 가 안 줄면 Task 3 으로 넘어가지 말고 멈춘다 — 설계가 틀린 것이다.

---

### Task 3: pref 를 `-CommitAlign` 으로 바꾸고 B1 이름을 지운다

순수 이름 변경 + 기동 경고. 동작은 Task 2 에서 이미 끝났다.

**Files:**
- Modify: `components/config/prefs.rs` (선언 둘: `:592` 부근, 기본값 `:1229`)
- Modify: `components/paint/commit_scheduler.rs` (`ALIGN_PCT` 정의와 그 문서 주석, `:186` 의 사용처)
- Modify: `components/paint/dcomp_compositor.rs` (`:1761`, `:2679`, `:2836`, `:3788`)
- Modify: `components/paint/paint.rs` (`commit_deadline` 과 `:3017` 의 분기)
- Modify: `etc/multigpu/run_wall_dist.ps1` (`:167`, `:655`, `:661`, `:754`, `:826`)

**Interfaces:**
- Consumes: Task 2 의 `commit_deadline`
- Produces: `commit_scheduler::COMMIT_ALIGN_PCT: LazyLock<Option<u64>>`, pref `gfx_present_align_commit_pct`, 스크립트 파라미터 `-CommitAlign`

- [ ] **Step 1: pref 를 교체한다**

`components/config/prefs.rs` 의 `pub gfx_present_align_per_output_pct: i64,` 와 그 위 문서 주석 전체를 아래로 바꾼다.

```rust
    /// ★DComp Commit 을 공통 합성 격자 위의 고정된 위상에 내보낸다(B3).★ `-1`(기본) = 끔,
    /// `0..99` = 합성 주기의 그 백분율 지점을 겨냥한다.
    ///
    /// 커밋은 그동안 렌더 패스의 **끝**에서 나갔다. `gfx_present_align_dwm_pct` 가 고정하는
    /// 것은 패스의 **시작**이므로 커밋 위상이 패스 길이를 그대로 물려받았고, 그 산포가
    /// 저더의 두 증상을 모두 만들었다 -- 큰 탈선은 합성 마감을 스치고, 그 흔들림이 커밋
    /// 간격을 흔들어 합성 경계를 넘나든다. 실측에서 패스 산포와 커밋 위상 산포가 네 실행
    /// 모두 같은 크기였다(`docs/superpowers/specs/2026-10-06-commit-on-composition-grid-design.md`).
    ///
    /// 마감은 `틱 직후의 첫 pct% 격자점` 이고 틱으로부터 최대 한 주기 뒤다. 패스가 그때까지
    /// 못 끝내면 **즉시 커밋한다** -- 늦더라도 최신 내용을 내보낸다.
    ///
    /// ★`pct` 는 `gfx_present_align_dwm_pct` 보다 패스 p95 만큼 커야 한다.★ 작으면 마감이
    /// 패스보다 먼저 와서 거의 매 프레임 초과하고, 기능이 켜진 채 아무 일도 하지 않는다.
    /// 프로브의 패스 p95 는 주기의 13.4% 다.
    ///
    /// 켜면 `gfx_dcomp_parallel_commit` 은 무시된다(목적이 겹친다). 기동 로그에 남는다.
    pub gfx_present_align_commit_pct: i64,
```

`:1229` 의 `gfx_present_align_per_output_pct: -1,` 를 `gfx_present_align_commit_pct: -1,` 로 바꾼다. ★`Default` 구현의 필드 순서는 선언 순서를 따라야 하므로 자리를 옮기지 말 것.★

- [ ] **Step 2: 캐시 이름과 출처를 바꾼다**

`components/paint/commit_scheduler.rs`:

```rust
pub(crate) static COMMIT_ALIGN_PCT: LazyLock<Option<u64>> = LazyLock::new(|| {
    let raw = servo_config::pref!(gfx_present_align_commit_pct);
    (0..=99).contains(&raw).then_some(raw as u64)
});
```

그 위 문서 주석에서 `gfx_present_align_per_output_pct` 를 `gfx_present_align_commit_pct` 로 바꾸고, "painter 마다 `end_frame` 당 하나 + flush 당 하나" 라는 호출 빈도 서술은 그대로 둔다(가드 쪽 호출은 그대로다).

- [ ] **Step 3: 나머지 사용처를 전부 바꾼다**

```bash
grep -rn "ALIGN_PCT" --include=*.rs components/
```

나오는 모든 `crate::commit_scheduler::ALIGN_PCT` 를 `crate::commit_scheduler::COMMIT_ALIGN_PCT` 로 바꾼다. 위치: `commit_scheduler.rs:186`, `dcomp_compositor.rs` 4곳(`:1761`, `:2679`, `:2836`, `:3788`), `paint.rs` 2곳(`commit_deadline`, `:3017`). ★가드 4곳은 이름만 바뀐다★ — "스케줄러가 살아 있으면 그 디바이스를 잠근다" 는 의미는 그대로다.

`paint.rs:2909` 부근과 `:3017` 주석의 `(commit_scheduler::ALIGN_PCT 주석)` 참조도 같이 고친다.

- [ ] **Step 4: 기동 경고를 넣는다**

`components/paint/paint.rs` 의 `flush_deferred_dcomp_commits` 안, `start_probe()` 호출 **바로 아래**에 추가한다.

```rust
            // ★마감이 패스보다 먼저 오면 기능이 켜진 채 아무 일도 하지 않는다.★ 거의 매
            // 프레임 즉시 커밋으로 떨어지는데, 로그에는 정렬이 켜진 것으로 보인다.
            //
            // 15% 는 프로브의 패스 p95(주기의 13.4%)에 여유를 조금 준 값이다. 패스 p95 는
            // 기동 시점에 알 수 없으므로 상수로 둔다. 막지는 않는다 -- 스윕에서 일부러 좁게
            // 주는 경우가 있고, 그때 무슨 일이 생기는지 보는 것이 측정의 일부다.
            {
                static WARNED_GAP: std::sync::Once = std::sync::Once::new();
                WARNED_GAP.call_once(|| {
                    let commit = *crate::commit_scheduler::COMMIT_ALIGN_PCT;
                    let tick = servo_config::pref!(gfx_present_align_dwm_pct);
                    if let Some(commit) = commit
                        && (0..=99).contains(&tick)
                        && commit.saturating_sub(tick as u64) < 15
                    {
                        warn!(
                            "[commitsched] gfx_present_align_commit_pct={commit} 가 \
                             gfx_present_align_dwm_pct={tick} 보다 15%p 넘게 크지 않다 -- \
                             마감이 패스보다 먼저 와서 거의 매 프레임 즉시 커밋으로 떨어질 \
                             수 있다(프로브의 패스 p95 가 주기의 13.4%). OUTCOMMIT 의 \
                             slip_us 로 확인할 것"
                        );
                    }
                });
            }
```

- [ ] **Step 5: 스크립트를 고친다**

`etc/multigpu/run_wall_dist.ps1`:

1. `:167` 의 `[int] $PerOutputAlign = -1,` 와 그 위 주석 블록을 아래로 바꿄다.

```powershell
    # gfx_present_align_commit_pct -- B3. DComp Commit 을 공통 합성 격자 위의 고정된
    # 위상에 내보낸다. -1 = 꺼짐(기본), 0..99 = 합성 주기의 그 백분율 지점.
    #
    # 커밋은 그동안 렌더 패스의 **끝**에서 나갔고, -DwmAlign 은 패스의 **시작**만
    # 고정한다. 그래서 커밋 위상이 패스 길이를 그대로 물려받았고, 그 산포가 저더의
    # 두 증상을 모두 만들었다 -- 큰 탈선은 합성 마감을 스치고, 그 흔들림이 커밋
    # 간격을 흔들어 합성 경계를 넘나든다.
    #
    # ★-DwmAlign 보다 15%p 이상 커야 한다.★ 작으면 마감이 패스보다 먼저 와서 거의
    # 매 프레임 즉시 커밋으로 떨어지고, 기능이 켜진 채 아무 일도 하지 않는다
    # (프로브의 패스 p95 가 주기의 13.4%). 엔진이 기동 로그에 경고를 한 줄 낸다.
    #
    # 켜면 -DcompParallelCommit 은 무시된다(목적이 겹친다). 기동 로그에 남는다.
    # ★판정은 TICKCOMMIT 의 spread 다★ -- 0 에 가까워져야 성공이다.
    [ValidateRange(-1, 99)]
    [int]    $CommitAlign = -1,
```
2. `:655` 의 `throw` 조건과 문구에서 `PerOutputAlign` → `CommitAlign`.
3. `:661` 의 경고 조건과 문구에서 `PerOutputAlign` → `CommitAlign`.
4. `:754` 를 `"--pref", "gfx_present_align_commit_pct=$CommitAlign",` 로.
5. `:826` 요약줄의 `per_output_align=$PerOutputAlign` 을 `commit_align=$CommitAlign` 으로.

```bash
grep -n "PerOutputAlign\|per_output_align\|gfx_present_align_per_output_pct" etc/multigpu/run_wall_dist.ps1
```

Expected: 0 건.

- [ ] **Step 6: 남은 흔적이 없는지 확인한다**

```bash
grep -rn "per_output_pct\|PerOutputAlign" --include=*.rs --include=*.ps1 --include=*.json components/ etc/
```

Expected: 0 건. ★설계 문서(`docs/`)의 과거 기록은 고치지 않는다★ — 그때의 기록이고, 바꾸면 로그와 대조가 안 된다.

- [ ] **Step 7: 빌드·테스트·포맷**

```
build.cmd build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
build.cmd test  --release -p servo-paint -p servo --features servo/no-wgl,servo/media-gstreamer,servo/webgpu --lib
build.cmd fmt   -p servo -p servo-paint --check
```

Expected: 빌드 성공·경고 0, 테스트 전부 통과, fmt diff 가 각 파일 기준선과 같음.

- [ ] **Step 8: 스크립트가 도는지 확인한다**

★이 스크립트는 `-WhatIf` 를 지원하지 않는다★ -- `[CmdletBinding()]` 만 있고 `SupportsShouldProcess` 가
없어 `-WhatIf` 는 "파라미터를 찾을 수 없다" 로 죽는다. 짧게 한 번 띄운다.

```powershell
etc\multigpu
un_wall_dist.ps1 -CommitAlign 25 -DwmAlign 8 -DurationSec 5
```

파라미터 오류 없이 지나가고, 기동 요약줄에 `commit_align=25` 가 보여야 한다. 잘못된 조합도 한 번
확인한다 -- `-CommitAlign 25 -DcompCommitInFrame` 은 `throw` 로 멈춰야 한다.

- [ ] **Step 9: 커밋**

```bash
git add components/config/prefs.rs components/paint/commit_scheduler.rs \
        components/paint/dcomp_compositor.rs components/paint/paint.rs \
        etc/multigpu/run_wall_dist.ps1
git commit -F "%TEMP%\msg.txt"
```

메시지 (제목은 그대로, 본문은 실제로 한 일에 맞게 다듬을 것):

```
refactor(wall): B1 의 -PerOutputAlign 을 -CommitAlign 으로 교체한다

동작은 앞 커밋에서 이미 바뀜다. 이것은 이름과 기동 경고다.

이름이 거짓말을 하고 있었다 -- "per output" 인데 공통 격자를 잡는다. 스크립트·로그를
나중에 읽는 사람이 틀리게 된다. 경로를 하나로 두고 분기를 늘리지 않는다.

★`grid_for_monitor` 와 프로브 스레드는 남긴다.★ `OUTPHASE`/`vs_comp_ms` 가 거기서
나오고 그것이 위상 법칙의 계측이다. 그리고 "DWM 이 네 타일을 한 합성 패스에 올린다" 는
전제가 깨지면 출력별 견냥이 다시 후보가 된다.

기동 경고를 하나 넣었다. `CommitAlign - DwmAlign` 이 15%p 미만이면 마감이 패스보다
먼저 와서 거의 매 프레임 즉시 커밋으로 떨어지는데, 로그에는 정렬이 켜진 것으로 보인다.
15% 는 프로브의 패스 p95(주기의 13.4%)에 여유를 조금 준 값이다. ★막지는 않는다★ --
스윈에서 일부러 좁게 주는 경우가 있고, 그때 무슨 일이 생기는지 보는 것이 측정의 일부다.

설계 문서의 과거 기록은 고치지 않는다 -- 그때의 기록이고, 바꾸면 로그와 대조가 안 된다.
```

★`Cargo.lock`, `etc/multigpu/config/wall_layout.example_1x1.json`, `tests/html/multigpu_standard_video_*_probe.html` 는 절대 스테이징하지 않는다.★ `git add -A` / `git add .` 금지.

---

## 실기 판정 (전체 완료 후)

설계 문서 §8.2 를 그대로 따른다. 새 계측은 만들지 않는다.

| 지표 | 현재 (probe / output, align 8) | 기대 |
|---|---|---|
| ★`TICKCOMMIT spread`★ | 0.70 / 7.40 | ★0 에 가깝게 — **성공의 정의**★ |
| `DCOMPSTAT commit_lead_ms` p95−p05 | 0.63 / 5.74 | 같이 축소 |
| `MISSEVENT nc=1` /s | 0.07 / 0.42 | 축소 |
| `MISSEVENT nc=0` /s | 0.02 / 1.23 | 축소 |
| `COMPREFRESH d0−d2` /1k | 0.3 / 30.6 | 축소 |
| `OUTCOMMIT slip_us` | — | 마감 초과의 빈도·크기 |

**스윕:** `-CommitAlign` 22 / 30 / 40 / 50 / 70. 너무 작으면 초과가 잦고, 너무 크면 합성까지의 여유가 줄어 effect (ii) 가 커진다.

**검산:** `-DwmAlign` 을 8 과 24 로 바꿔도 위 지표가 **변하지 않아야** 한다. 커밋 위상이 틱 위치와 무관하게 고정되므로, 설계 문서 §9 의 `-DwmAlign` 7 배 차이가 사라져야 한다.

**미리 정해 두는 실패 판정:**
- `TICKCOMMIT spread` 가 안 줄면 ★설계가 틀린 것이다★ — 다른 지표가 좋아져도 그렇다.
- ★output 페이지의 `nc=1` 이 남는 것은 실패가 아니다.★ 창의 39% 에서 패스가 한 주기를 넘으므로 마감을 구조적으로 지킬 수 없다. 그것은 패스 단축이라는 별건이다.

## 범위 밖

- output 페이지의 `flush` 가 4.53ms 인 이유(프로브는 1.00ms인데 `paint` 는 오히려 더 적다).
- 패스 단축 일반.
- B2(`-SampleLead`)의 출력별 `vs_comp_ms` 보정.
- genlock.

# TickAnimation 프레임 시각 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 애니메이션 값의 기준 시각을 "script 가 그 함수에 도달한 순간" 에서 "그 프레임의 시각" 으로 바꾼다.

**Architecture:** 셸이 페이싱 틱 시각을 `servo::note_frame_time()` 으로 paint 에 알린다. paint 는 그것을 `TickAnimation` 에 실어 보내고, constellation 이 `TickAllAnimations` 로 전달하며, script 는 다음 `update_the_rendering` 에서 그 값을 타임라인과 rAF 양쪽에 쓴다. ***세그먼트의 시간 원점인 `built_at` 도 같은 값으로 잡아야*** 지터가 값에서 예측 위상으로 옮겨가지 않는다.

**Tech Stack:** Rust 1.95.0 / 크레이트 `servo-paint`, `servo-constellation-traits`, `servo-constellation`, `servo-script-traits`, `servo-script`, `servo-layout`, `servo-config` / 셸 `winit_wall`

**Spec:** `docs/superpowers/specs/2026-10-15-tick-animation-frame-time-design.md` — 이 계획의 근거 전부가 거기 있다. 섹션 번호로 참조한다.

## Global Constraints

- pref 이름은 ★`dom_animation_timeline_use_frame_time`★, 기본 ★`true`★ (§3.8).
- pref 가 꺼지면 타임라인과 rAF ***양쪽 모두*** 지금 경로로 간다. 한쪽만 꺼지면 둘이 어긋나 §3.7 이 막으려는 증상이 그대로 생긴다.
- 메시지에 싣는 값은 ★`CrossProcessInstant`(비-Option)★ — paint 가 항상 채운다. script 안에서만 `Option` 이다 (§3.1).
- 신선도 가드: 저장값이 ★`4 × paint_timer_period()`★ 보다 오래되면 `now()` 폴백 (§3.3).
- `layout_animations_test_enabled` 경로는 건드리지 않는다 — `document.rs:4581` 가 `timeline.update` 를 아예 부르지 않는다.
- 주석·커밋 메시지는 한국어 평어체. ★PowerShell 파일의 콘솔 출력은 ASCII★, 배열 splat 금지(해시테이블 splat).
- rustfmt: 손대는 파일마다 ***변경 전 건수를 먼저 재고*** 그 수를 유지한다. 개별 파일로 재라 — `lib.rs` 를 지정하면 모듈 트리 전체를 끌고 와 무관한 기존 위반이 섞인다.
- 커밋 트레일러: `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`
- 다음 파일은 ★절대 스테이징하지 않는다★: `Cargo.lock`, `etc/multigpu/config/wall_layout.example_1x1.json`, `tests/html/multigpu_standard_video_extended_probe.html`, `tests/html/multigpu_standard_video_rtsp_probe.html`. `git add -A` / `git add .` 금지.

## Review Focus

사양이 함축하지만 어느 태스크의 테스트도 자동으로 건드리지 않는, 사람을 물 가능성이 큰 것들. 각 줄은 그 코드를 가진 태스크에 테스트로 들어간다.

1. **`built_at` 과 타임라인 값이 어긋난다.** 값만 `frame_time` 으로 바꾸고 원점을 `now()` 로 두면, 제거한 큐 지연이 예측 위상으로 옮겨갈 뿐이다. `divisor=4` 는 리스트 하나로 네 프레임을 외삽하므로 거기서 가장 크게 나타난다. → Task 3, Task 6
2. **셸이 `note_frame_time` 을 부르다 멈춘다.** 페이싱 스레드가 죽으면 static 값이 굳어 ***애니메이션 시각이 정지한다*** — 저더보다 훨씬 나쁘다. → Task 2
3. **셸이 아예 부르지 않는다.** `servoshell` 과 다른 임베더가 그렇다. 저장값이 없으면 `now()` 폴백이라 지금 동작 그대로여야 한다. → Task 2
4. **pref 가 꺼져 있다.** 기본은 켜짐이지만, 끄면 타임라인과 rAF ***양쪽***이 지금 경로여야 한다. 한쪽만 꺼지면 둘이 어긋난다. → Task 5
5. **틱이 아닌 이유로 `update_the_rendering` 이 돈다.** scroll·resize·input·display-list·observer 등 열 가지가 넘는다(`document.rs:2979-3021`). 그때 `frame_time` 은 `None` 이고 지금처럼 `now()` 를 써야 한다. → Task 5

---

## 확인해 둔 사실

계획이 기대는 것을 미리 대조했다. 틀리면 해당 태스크가 통째로 막힌다.

| 가정 | 확인 |
|---|---|
| `CrossProcessInstant` 가 메시지에 실린다 | `Serialize + Deserialize` (`cross_process_instant.rs:26`) |
| `TickAnimation` 발신자 | ★두 곳뿐★, 둘 다 `refresh_driver.rs:169, :241` |
| 타임라인 값이 layout 까지 가는 길 | `Document::current_animation_timeline_value()`(`document.rs:4544`) → `ReflowRequest.animation_timeline_value`(`window.rs:2764`) → `layout_impl.rs:834` |
| ★타임라인 값은 별도 배선이 필요 없다★ | 위 접근자가 타임라인에서 읽으므로, `DocumentTimeline::update` 만 고치면 자동으로 흐른다 |
| `built_at` 은 ***따로*** 잡힌다 | `paint_animation.rs:487, :648` 의 `CrossProcessInstant::now()` — 그래서 Task 6 이 필요하다 |
| `built_at` 소비처 | 앵커(`web_content_animation.rs:565`), 진단 로그(`painter.rs:3398`), `TIMELINESTEP`(`:517`), 테스트(`:716`) |
| paint 크레이트 테스트 | 71 개 — ★여기에 진짜 테스트를 둔다★ |
| script 크레이트 테스트 | ★0 개★ (`#[cfg(test)]` 없음) — 검증은 빌드와 실기뿐 |
| `Performance::to_dom_high_res_time_stamp` | 있다 (`dom/performance/performance.rs:179`) |
| `MAX_DISPLAY_LIST_TRANSIT` | 250ms (`web_content_animation.rs:36`) — 0.4ms 증가는 여유 안 |

빌드·테스트는 전부 `build_wall.cmd` 래퍼로 돈다(벤더 툴체인 + MSVC 환경). rustfmt 는
`F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\servo\.rustup\toolchains\1.95.0-x86_64-pc-windows-msvc\bin\rustfmt.exe`.

## 파일 구조

| 파일 | 책임 | 태스크 |
|---|---|---|
| `components/paint/frame_time.rs` (신규, 약 110줄) | 셸이 알려준 프레임 시각의 저장소 + 신선도 가드 + 테스트 | 2 |
| `components/paint/lib.rs` | `pub fn note_frame_time` 공개 + 모듈 선언 | 2 |
| `components/paint/web_content_animation.rs` | `zero` 계산을 순수 함수로 분리 + 테스트 | 3 |
| `components/paint/refresh_driver.rs` | 프레임 시각을 읽어 `TickAnimation` 에 싣는다 | 4 |
| `components/shared/constellation/lib.rs` | `TickAnimation` 페이로드 | 4 |
| `components/constellation/constellation.rs` | 전달 | 4 |
| `components/shared/script/lib.rs` | `TickAllAnimations` 페이로드 | 4 |
| `components/script/script_thread.rs` | 보관(최신 우선) + `update_the_rendering` 으로 전달 | 4, 5 |
| `components/script/dom/document/document.rs` | 타임라인·rAF 에 적용 | 5 |
| `components/script/dom/animations/documenttimeline.rs` | `update(window, frame_time)` | 5 |
| `components/config/prefs.rs` | 새 pref 추가, 평활기 pref 제거 | 1, 5 |
| `components/shared/layout/lib.rs` | `ReflowRequest.animation_frame_time` | 6 |
| `components/script/dom/window.rs` | 그 필드를 채운다 | 6 |
| `components/layout/layout_impl.rs` 외 3 | layout 안에서 배선 | 6 |
| `components/layout/display_list/paint_animation.rs` | `built_at` 을 인자로 받는다 | 6 |
| `components/servo/examples/winit_wall/main.rs` | `servo::note_frame_time()` 호출 | 7 |
| `etc/multigpu/run_wall_dist.ps1`, `tools/run_frame_time_ab.ps1` | 스위치와 A/B | 7 |
| 삭제: `components/shared/base/animation_timeline_smoothing.rs` 외 | 평활기 | 1 |

---

### Task 1: 평활기를 걷어낸다

§3.9. 이 교정의 국소 근사였다. 둘 다 두면 같은 값을 두 번 만져 실기에서 어느 쪽이 효과를 냈는지 가릴 수 없고, 실기 검증을 한 번도 통과하지 않았으므로 남길 근거도 없다.

**Files:**
- Delete: `components/shared/base/animation_timeline_smoothing.rs`
- Delete: `etc/multigpu/tools/run_timeline_smoothing_ab.ps1`
- Modify: `components/shared/base/lib.rs` (모듈 선언)
- Modify: `components/config/prefs.rs` (`dom_animation_timeline_smoothing_hz` 선언 + 기본값)
- Modify: `components/script/dom/animations/documenttimeline.rs` (평활 호출 제거, 원래 `update` 로)
- Modify: `etc/multigpu/run_wall_dist.ps1` (`-TimelineSmoothing` 스위치)
- Modify: `etc/multigpu/make_wall_dist.ps1` (A/B 복사 줄)

**Interfaces:**
- Consumes: 없음
- Produces: `DocumentTimeline::update(&self, window: &Window)` 가 변경 전 원형으로 돌아온다 — Task 5 가 여기서 다시 시작한다

- [ ] **Step 1: 지울 것을 전부 찾는다**

```
grep -rn "animation_timeline_smoothing\|smooth_animation_timeline\|TimelineSmoothingState\|smoothing_state\|dom_animation_timeline_smoothing_hz\|TimelineSmoothing\|run_timeline_smoothing_ab" --include=*.rs --include=*.ps1 components/ etc/
```

Expected: `components/shared/base/animation_timeline_smoothing.rs`(파일 전체), `components/shared/base/lib.rs`(1줄), `components/config/prefs.rs`(2줄), `components/script/dom/animations/documenttimeline.rs`(여러 줄), `etc/multigpu/run_wall_dist.ps1`(2줄), `etc/multigpu/make_wall_dist.ps1`(1줄), `etc/multigpu/tools/run_timeline_smoothing_ab.ps1`(파일 전체).

★이 목록에 없는 파일이 나오면 멈추고 확인하라★ — 평활기가 예상보다 번져 있다는 뜻이다.

- [ ] **Step 2: `documenttimeline.rs` 를 변경 전으로 되돌린다**

`update` 를 이 모양으로 바꾼다. ★이것이 `9f79244e80a` 이전의 원형이다.★

```rust
    /// Updates the value of the `AnimationTimeline` to the current clock time.
    pub(crate) fn update(&self, window: &Window) {
        let duration_since_time_origin =
            CrossProcessInstant::now() - window.navigation_start() - self.origin_offset;
        self.animation_timeline
            .set_current_time(duration_since_time_origin);
    }
```

그리고 같은 파일에서 지운다:
- `smoothing_state: Cell<Option<TimelineSmoothingState>>` 필드와 그 위 주석, `#[no_trace]`
- 생성자 두 곳의 `smoothing_state: Cell::new(None),`
- `fn smoothing_period_seconds()` 전체
- `use servo_base::animation_timeline_smoothing::{...};`
- `use std::cell::Cell;` ★단, 파일에 다른 `Cell` 사용이 남아 있으면 두라★ — `grep -c "Cell<" components/script/dom/animations/documenttimeline.rs` 로 확인한다

- [ ] **Step 3: 나머지를 지운다**

```bash
git rm components/shared/base/animation_timeline_smoothing.rs
git rm etc/multigpu/tools/run_timeline_smoothing_ab.ps1
```

`components/shared/base/lib.rs` 에서 `pub mod animation_timeline_smoothing;` 한 줄 삭제.

`components/config/prefs.rs` 에서 `dom_animation_timeline_smoothing_hz` 의 선언(doc 주석 블록 포함)과 `const_default` 의 기본값 줄 삭제.

`etc/multigpu/run_wall_dist.ps1` 에서 `[ValidateRange(0, 1000)]` + `[int] $TimelineSmoothing = 0,` 와 그 위 주석 블록, 그리고 `if ($TimelineSmoothing -gt 0) { ... }` 줄 삭제.

`etc/multigpu/make_wall_dist.ps1` 에서 `run_timeline_smoothing_ab.ps1` 복사 줄과 그 위 주석 2줄 삭제.

★PowerShell 파일을 Python 으로 고칠 때는 줄끝을 보존하라★ — 이 두 파일은 LF 다. `open(p,'rb')` 로 읽어 `b'\r\n' in raw` 로 판정하고 같은 줄끝으로 쓴다.

- [ ] **Step 4: 빌드와 테스트**

```
build_wall.cmd test -p servo-base
build_wall.cmd build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
```

Expected: `servo-base` ★33 passed★(평활기 10 개가 빠진 수), 빌드 성공, ★내가 손댄 파일을 가리키는 경고 0 건★.

경고 세기: 로그에서 `documenttimeline.rs|config\prefs.rs|base\lib.rs` 를 grep 해 0 이어야 한다.

- [ ] **Step 5: PowerShell 두 파일이 아직 파싱되는지 확인한다**

```powershell
foreach ($f in @('etc\multigpu\run_wall_dist.ps1','etc\multigpu\make_wall_dist.ps1')) {
    $e = $null
    $null = [System.Management.Automation.Language.Parser]::ParseFile((Resolve-Path $f), [ref]$null, [ref]$e)
    "$f : " + $(if ($e) { "FAIL " + $e[0].Message } else { "OK" })
}
```

Expected: 둘 다 `OK`.

- [ ] **Step 6: 포맷과 커밋**

손대기 전 건수를 먼저 재고(Global Constraints) 같은 수인지 확인한 뒤:

```bash
git add components/shared/base/animation_timeline_smoothing.rs components/shared/base/lib.rs \
        components/config/prefs.rs components/script/dom/animations/documenttimeline.rs \
        etc/multigpu/run_wall_dist.ps1 etc/multigpu/make_wall_dist.ps1 \
        etc/multigpu/tools/run_timeline_smoothing_ab.ps1
git commit -F - <<'EOF'
revert(script): 타임라인 평활기를 걷어낸다

설계 문서 51 의 평활기는 ***프레임 시각을 싣는 교정의 국소 근사***였다. 실기 검증을 한 번도
통과하지 않았고, 둘 다 두면 같은 값을 두 번 만져 어느 쪽이 효과를 냈는지 가릴 수 없다.

`DocumentTimeline::update` 가 9f79244e80a 이전 원형으로 돌아간다. 다음 커밋들이 같은
자리를 ***프레임 시각***으로 고친다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 2: 셸이 알려준 프레임 시각을 담는 저장소

§3.2, §3.3. `servo::note_present_tick()`(`paint/lib.rs:62`)의 선례를 따른다 — 관찰자는 `Painter::new`(`painter.rs:843`) 안에서 만들어져 셸이 닿을 수 없으므로, `Servo → Paint → Painter → observer` 로 배선하는 것보다 자유 함수가 작다. 기존 `note_present_tick` 은 Windows 전용 QPC 를 저장하므로(`output_grid.rs:560`) 재사용할 수 없다.

**Files:**
- Create: `components/paint/frame_time.rs`
- Modify: `components/paint/lib.rs`
- Test: 같은 파일의 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 없음
- Produces:
  - `pub fn note_frame_time(frame_time: CrossProcessInstant)` — `paint/lib.rs` 에서 `pub` 으로 재노출. 셸이 부른다.
  - `pub(crate) fn frame_time_for_tick(now: CrossProcessInstant, max_age: Duration) -> Option<CrossProcessInstant>` — Task 4 가 부른다.
  - `pub(crate) fn fresh_frame_time(stored: Option<CrossProcessInstant>, now: CrossProcessInstant, max_age: Duration) -> Option<CrossProcessInstant>` — 순수 함수, 위 둘이 공유하고 테스트가 겨냥한다.

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`components/paint/frame_time.rs` 를 만들고 테스트만 넣는다.

```rust
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 셸이 알려준 프레임 시각.

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use servo_base::cross_process_instant::CrossProcessInstant;

    use super::*;

    fn at(seconds: f64) -> CrossProcessInstant {
        CrossProcessInstant::epoch() + time::Duration::seconds_f64(seconds)
    }

    const MAX_AGE: Duration = Duration::from_millis(67); // 4 x 16.7ms

    /// ★셸이 아예 부르지 않으면 `None` 이다.★ `servoshell` 과 다른 임베더가 그렇다.
    /// 호출자는 이 경우 `now()` 를 쓰므로 ***지금 동작 그대로***여야 한다.
    #[test]
    fn nothing_stored_is_none() {
        assert_eq!(fresh_frame_time(None, at(100.0), MAX_AGE), None);
    }

    /// 방금 받은 값은 그대로 쓴다.
    #[test]
    fn a_recent_value_is_used() {
        let stored = at(100.0);
        assert_eq!(
            fresh_frame_time(Some(stored), at(100.005), MAX_AGE),
            Some(stored)
        );
    }

    /// ★셸이 부르다 멈추면 버린다.★ 페이싱 스레드가 죽었을 때 굳은 값을 계속 쓰면
    /// ***애니메이션 시각이 정지한다*** -- 저더보다 훨씬 나쁘다. 네 주기가 지나면
    /// `None` 을 돌려주어 호출자가 `now()` 로 떨어지게 한다.
    #[test]
    fn a_stale_value_is_dropped() {
        let stored = at(100.0);
        assert_eq!(fresh_frame_time(Some(stored), at(100.100), MAX_AGE), None);
    }

    /// 경계: 정확히 `max_age` 만큼 지난 값은 아직 쓴다. 한 쪽으로 정해 두지 않으면
    /// 주기와 정확히 맞아떨어지는 실행에서 매 프레임 폴백이 일어날 수 있다.
    #[test]
    fn exactly_max_age_is_still_fresh() {
        let stored = at(100.0);
        let now = stored + time::Duration::try_from(MAX_AGE).unwrap();
        assert_eq!(fresh_frame_time(Some(stored), now, MAX_AGE), Some(stored));
    }

    /// ★미래의 값도 쓴다.★ 셸은 틱 시각을 재고 ***그 다음에*** 렌더를 부르므로 보통
    /// 과거지만, 두 시계 읽기 사이에 역전이 보일 수 있다. 음수 나이를 "오래됐다" 로
    /// 잘못 판정하면 그 프레임만 폴백해 거기서 값이 튄다.
    #[test]
    fn a_value_slightly_in_the_future_is_used() {
        let stored = at(100.010);
        assert_eq!(
            fresh_frame_time(Some(stored), at(100.0), MAX_AGE),
            Some(stored)
        );
    }

    /// 저장과 조회가 왕복한다. 전역 상태라 다른 테스트와 간섭하지 않도록 이 테스트만
    /// 실제 저장소를 건드린다.
    #[test]
    fn store_and_read_round_trip() {
        let t = CrossProcessInstant::now();
        note_frame_time(t);
        assert_eq!(stored_frame_time(), Some(t));
    }
}
```

- [ ] **Step 2: 실패를 확인한다**

```
build_wall.cmd test -p servo-paint frame_time
```

Expected: ★컴파일 실패★ — `cannot find function 'fresh_frame_time' in this scope`.

모듈이 아직 `lib.rs` 에 선언되지 않아 "0 passed" 로 통과한다면 그것은 RED 가 아니다. 그 경우 Step 3 의 `lib.rs` 선언을 먼저 넣고 이 단계를 다시 돌린다.

- [ ] **Step 3: 구현한다**

`#[cfg(test)] mod tests` ★위★에 넣는다.

```rust
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use servo_base::cross_process_instant::CrossProcessInstant;

/// 셸이 마지막으로 알려준 프레임 시각, epoch 로부터의 나노초. `i64::MIN` 은 "없음".
///
/// ★전역인 이유.★ 이 값을 쓰는 `AnimationRefreshDriverObserver` 는 `Painter::new` 안에서
/// 만들어져(`painter.rs:843`) 셸이 닿을 수 없다. `Servo -> Paint -> Painter -> observer`
/// 로 배선하는 것보다 `note_present_tick`(`lib.rs:62`)의 선례를 따르는 편이 작다.
static FRAME_TIME_NANOS: AtomicI64 = AtomicI64::new(i64::MIN);

/// 셸이 표출 틱 시각을 알린다. ***틱을 낸 맨 앞에서*** 부르는 것이 요점이다 -- 렌더 준비
/// 비용이 섞이면 그만큼이 애니메이션 값의 지터가 된다.
///
/// 부르지 않는 임베더는 `stored_frame_time()` 이 `None` 이라 지금 동작 그대로다.
pub fn note_frame_time(frame_time: CrossProcessInstant) {
    let nanos = (frame_time - CrossProcessInstant::epoch()).whole_nanoseconds();
    // `i128 -> i64`. 범위를 넘으면(292 년) 저장하지 않는다 -- `i64::MIN` 과 충돌해
    // "없음" 으로 읽히는 것보다 조용히 무시하는 편이 낫다.
    let Ok(nanos) = i64::try_from(nanos) else {
        return;
    };
    if nanos == i64::MIN {
        return;
    }
    FRAME_TIME_NANOS.store(nanos, Ordering::Relaxed);
}

/// 저장된 값. 셸이 한 번도 부르지 않았으면 `None`.
pub(crate) fn stored_frame_time() -> Option<CrossProcessInstant> {
    let nanos = FRAME_TIME_NANOS.load(Ordering::Relaxed);
    if nanos == i64::MIN {
        return None;
    }
    Some(CrossProcessInstant::epoch() + time::Duration::nanoseconds(nanos))
}

/// 저장값이 아직 쓸 만한가.
///
/// ★셸이 부르다 멈추면 버려야 한다.★ 페이싱 스레드가 죽었을 때 굳은 값을 계속 쓰면
/// ***애니메이션 시각이 정지한다*** -- 저더보다 훨씬 나쁘다.
///
/// ★미래의 값은 버리지 않는다.★ 셸은 틱 시각을 재고 그 다음에 렌더를 부르므로 보통
/// 과거지만, 두 시계 읽기 사이에 역전이 보일 수 있다. 그것까지 폴백시키면 그 프레임만
/// 값이 튄다.
pub(crate) fn fresh_frame_time(
    stored: Option<CrossProcessInstant>,
    now: CrossProcessInstant,
    max_age: Duration,
) -> Option<CrossProcessInstant> {
    let stored = stored?;
    let age = now - stored;
    if age.is_negative() {
        return Some(stored);
    }
    (age <= time::Duration::try_from(max_age).ok()?).then_some(stored)
}

/// 틱에 실을 프레임 시각. 없거나 오래됐으면 `None` -- 호출자가 `now()` 로 떨어진다.
pub(crate) fn frame_time_for_tick(
    now: CrossProcessInstant,
    max_age: Duration,
) -> Option<CrossProcessInstant> {
    fresh_frame_time(stored_frame_time(), now, max_age)
}
```

`components/paint/lib.rs` 에 모듈을 선언하고 재노출한다. `mod tracing;` 근처의 모듈 목록을 보고 알파벳 순 자리에 넣는다:

```rust
mod frame_time;
pub use frame_time::note_frame_time;
```

★`pub use` 가 필요하다★ — 셸은 `servo::note_frame_time` 으로 부르고, `servo` 크레이트가 `paint` 의 것을 다시 내보낸다. `note_present_tick` 이 어떻게 노출되는지 보고 같은 방식을 따르라: `grep -rn "note_present_tick" components/servo/`.

- [ ] **Step 4: 통과를 확인한다**

```
build_wall.cmd test -p servo-paint frame_time
```

Expected: ★6 passed★.

`a_value_slightly_in_the_future_is_used` 가 실패하면 `age.is_negative()` 가지가 빠진 것이다. `time::Duration` 은 음수를 표현하므로 `now - stored` 가 음수일 수 있다.

- [ ] **Step 5: 포맷과 커밋**

```bash
git add components/paint/frame_time.rs components/paint/lib.rs
git commit -F - <<'EOF'
feat(paint): 셸이 알려주는 프레임 시각 저장소를 만든다

아직 아무도 읽지 않는다 -- 다음 커밋이 틱에 싣는다.

`servo::note_present_tick()` 의 선례를 따라 자유 함수 + static 이다. 이 값을 쓸
`AnimationRefreshDriverObserver` 는 `Painter::new` 안에서 만들어져 셸이 닿을 수 없고,
기존 `note_present_tick` 은 Windows 전용 QPC 를 저장하므로 재사용할 수 없다.

★신선도 가드가 핵심이다.★ 셸이 부르다 멈추면(페이싱 스레드 사망 등) 굳은 값을 계속 쓰게
되고, 그러면 ***애니메이션 시각이 정지한다*** -- 저더보다 훨씬 나쁘다. 네 주기가 지나면
`None` 을 돌려 호출자가 `now()` 로 떨어지게 한다.

미래의 값은 버리지 않는다. 셸은 틱 시각을 재고 그 다음에 렌더를 부르므로 보통 과거지만,
두 시계 읽기 사이에 역전이 보일 수 있고 그것까지 폴백시키면 그 프레임만 값이 튄다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 3: 예측의 시간 원점 계산을 순수 함수로 뺀다

Review Focus #1 을 ***테스트로 고정***하는 태스크다. 아직 동작을 바꾸지 않는다 — Task 6 이 이 함수에 `frame_time` 을 먹일 때 무엇이 보장되어야 하는지를 먼저 못박는다.

**Files:**
- Modify: `components/paint/web_content_animation.rs:552-582` (`install_paint_animations` 안의 `zero` 계산)
- Test: 같은 파일의 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 없음
- Produces: `fn animation_zero(built_at: CrossProcessInstant, now: CrossProcessInstant, received_at: Instant, offset_from_display_list: f64) -> Instant` — `install_paint_animations` 가 쓴다. Task 6 의 보장이 이 함수에 걸린다.

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`components/paint/web_content_animation.rs` 의 두 번째 `#[cfg(test)] mod tests`(약 689행) 안, 기존 헬퍼들 아래에 넣는다.

```rust
    /// ★원점은 "리스트가 만들어진 시각" 이어야 한다.★ 받은 시각에 박으면 그 사이에 든
    /// 시간(리플로 잔여 + 전송 + 씬 빌드)만큼 예측 전체가 과거로 밀린다.
    #[test]
    fn zero_is_anchored_at_build_time_not_receipt() {
        let built = CrossProcessInstant::epoch() + time::Duration::seconds_f64(100.0);
        let now = built + time::Duration::milliseconds(3);
        let received_at = Instant::now();
        let zero = animation_zero(built, now, received_at, 0.0);
        let behind = received_at.saturating_duration_since(zero);
        assert!(
            (behind.as_secs_f64() - 0.003).abs() < 1e-4,
            "받은 시각보다 3ms 앞이어야 한다: {behind:?}"
        );
    }

    /// ★이 테스트가 Task 6 의 이유다.★ 연달아 오는 리스트의 `built_at` 이 고른 격자 위에
    /// 있으면, 전송 시간이 매번 달라도 ***원점이 정확히 같은 간격으로*** 늘어서야 한다.
    /// 그래야 예측들이 서로 맞물린다. 원점이 흔들리면 그 흔들림이 그대로 화면의 떨림이다.
    #[test]
    fn even_build_times_give_even_zeros_despite_jittery_transit() {
        let base = CrossProcessInstant::epoch() + time::Duration::seconds_f64(100.0);
        let period = time::Duration::seconds_f64(1.0 / 60.0);
        let start = Instant::now();
        // 전송 지연을 0.2ms ~ 2.0ms 로 흔든다. 실측 범위다(log_ani_debug/08 SCRIPTBUSY).
        let transits_ms = [0.2, 2.0, 0.5, 1.7, 0.3];
        let zeros: Vec<Instant> = transits_ms
            .iter()
            .enumerate()
            .map(|(index, transit_ms)| {
                let built = base + period * (index as i32);
                let now = built + time::Duration::seconds_f64(transit_ms / 1000.0);
                let received_at = start + std::time::Duration::from_secs_f64(
                    (index as f64) / 60.0 + transit_ms / 1000.0,
                );
                animation_zero(built, now, received_at, 0.0)
            })
            .collect();
        for pair in zeros.windows(2) {
            let gap = pair[1].saturating_duration_since(pair[0]).as_secs_f64();
            assert!(
                (gap - 1.0 / 60.0).abs() < 1e-4,
                "원점 간격이 고르지 않다: {gap}"
            );
        }
    }

    /// 오래된 트리의 애니메이션은 과거에서 시작한다 -- `offset_from_display_list` 가 음수다.
    #[test]
    fn a_negative_offset_moves_zero_into_the_past() {
        let built = CrossProcessInstant::epoch() + time::Duration::seconds_f64(100.0);
        let received_at = Instant::now();
        let zero = animation_zero(built, built, received_at, -0.5);
        let behind = received_at.saturating_duration_since(zero);
        assert!(
            (behind.as_secs_f64() - 0.5).abs() < 1e-4,
            "0.5초 과거여야 한다: {behind:?}"
        );
    }

    /// ★전송 시간이 터무니없으면 자른다.★ 콘텐츠 전환이나 스톨 뒤에 몇 초짜리 transit 이
    /// 나오면 원점이 그만큼 과거로 가 애니메이션이 순간이동한다.
    #[test]
    fn an_absurd_transit_is_clamped() {
        let built = CrossProcessInstant::epoch() + time::Duration::seconds_f64(100.0);
        let now = built + time::Duration::seconds(10);
        let received_at = Instant::now();
        let zero = animation_zero(built, now, received_at, 0.0);
        let behind = received_at.saturating_duration_since(zero);
        assert!(
            behind <= MAX_DISPLAY_LIST_TRANSIT,
            "{behind:?} 가 상한 {MAX_DISPLAY_LIST_TRANSIT:?} 를 넘었다"
        );
    }
```

- [ ] **Step 2: 실패를 확인한다**

```
build_wall.cmd test -p servo-paint web_content_animation
```

Expected: ★컴파일 실패★ — `cannot find function 'animation_zero' in this scope`.

- [ ] **Step 3: 함수를 빼낸다**

`install_paint_animations` ★위★에 넣는다. 본문은 지금 `install_paint_animations` 안에 있는 것을 그대로 옮긴 것이다(`web_content_animation.rs:565-575`).

```rust
/// 세그먼트의 시간 원점.
///
/// ★원점은 이 리스트가 **만들어진** 시각이지 **받은** 시각이 아니다.★
///
/// 세그먼트는 레이아웃이 샘플링을 시작한 순간을 0 으로 갖고, `offset_from_display_list` 도
/// 그 순간에서 잰 값이다. 받은 시각에 0 을 박으면 그 사이에 든 시간(리플로 잔여 + 전송 +
/// 씬 빌드)만큼 예측 전체가 과거로 밀린다. 그 시간이 일정하다면 일정한 지연일 뿐이지만
/// 일정하지 않다 -- 같은 벽에서 한가할 때 0.4ms, 콘텐츠 전환 중 2.0ms 다
/// (log_ani_debug/08 `SCRIPTBUSY`). 만들어진 시각에 박으면 연달아 오는 예측들이 서로
/// 정확히 맞물린다.
fn animation_zero(
    built_at: CrossProcessInstant,
    now: CrossProcessInstant,
    received_at: Instant,
    offset_from_display_list: f64,
) -> Instant {
    let transit = (now - built_at)
        .as_seconds_f64()
        .clamp(0.0, MAX_DISPLAY_LIST_TRANSIT.as_secs_f64());
    let created_at = received_at
        .checked_sub(Duration::from_secs_f64(transit))
        .unwrap_or(received_at);
    let offset = Duration::from_secs_f64(offset_from_display_list.abs());
    if offset_from_display_list <= 0.0 {
        created_at.checked_sub(offset).unwrap_or(created_at)
    } else {
        created_at + offset
    }
}
```

`install_paint_animations` 의 그 블록을 호출로 바꾼다:

```rust
                let zero = animation_zero(
                    animation.built_at,
                    now,
                    received_at,
                    animation.offset_from_display_list,
                );
                ActivePaintAnimation {
                    property: animation.property,
                    zero,
                    complete: animation.complete,
                }
```

- [ ] **Step 4: 통과를 확인한다**

```
build_wall.cmd test -p servo-paint
```

Expected: ★75 passed★ (기존 71 + 새 4). 기존 테스트가 하나라도 깨지면 옮기는 과정에서 식을 바꾼 것이다 — 원본과 글자 단위로 대조하라.

- [ ] **Step 5: 포맷과 커밋**

```bash
git add components/paint/web_content_animation.rs
git commit -F - <<'EOF'
refactor(paint): 예측의 시간 원점 계산을 순수 함수로 뺀다

동작은 그대로다. ★다음 커밋들이 `built_at` 을 프레임 시각으로 바꾸는데, 그때 무엇이
보장되어야 하는지를 먼저 테스트로 못박는다.***

핵심 테스트는 `even_build_times_give_even_zeros_despite_jittery_transit` 이다 --
`built_at` 이 고른 격자 위에 있으면 전송 시간이 0.2~2.0ms 로 흔들려도 원점이 정확히 같은
간격으로 늘어서야 한다. 그래야 연달아 오는 예측이 서로 맞물린다. 원점이 흔들리면 그
흔들림이 그대로 화면의 떨림이다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 4: 프레임 시각을 틱에 실어 script 까지 보낸다

§3. 네 크레이트의 메시지를 지난다. script 는 아직 ***보관만*** 하고 쓰지 않는다 — 그래야 이 태스크가 동작을 바꾸지 않고, 다음 태스크의 A/B 가 깨끗해진다.

**Files:**
- Modify: `components/paint/refresh_driver.rs:169, :241` (발신 두 곳)
- Modify: `components/shared/constellation/lib.rs:60` (`TickAnimation`)
- Modify: `components/constellation/constellation.rs:1490, :3808-3828` (전달)
- Modify: `components/shared/script/lib.rs:249` (`TickAllAnimations`)
- Modify: `components/script/script_thread.rs:2169` (보관)

**Interfaces:**
- Consumes: Task 2 의 `frame_time_for_tick(now, max_age) -> Option<CrossProcessInstant>`
- Produces:
  - `EmbedderToConstellationMessage::TickAnimation(Vec<WebViewId>, CrossProcessInstant)`
  - `ScriptThreadMessage::TickAllAnimations(Vec<WebViewId>, CrossProcessInstant)`
  - `ScriptThread::pending_frame_time: Cell<Option<CrossProcessInstant>>` — Task 5 가 읽는다

- [ ] **Step 1: 메시지 두 개에 필드를 더한다**

`components/shared/constellation/lib.rs`:

```rust
    /// Requests that the constellation instruct script/layout to try to layout again and tick
    /// animations.
    ///
    /// ★두 번째 값은 이 프레임의 시각이다.★ script 는 "자기가 이 메시지를 처리한 순간"
    /// 대신 이 값을 애니메이션의 기준으로 쓴다 -- 도착 지연의 변동이 그대로 화면의 떨림이
    /// 되기 때문이다(설계 문서 50).
    TickAnimation(Vec<WebViewId>, CrossProcessInstant),
```

`components/shared/script/lib.rs` 의 `TickAllAnimations` 도 같은 모양으로:

```rust
    /// Notifies the script thread to tick all animations, with the time of the frame
    /// these animations are being ticked for. See `EmbedderToConstellationMessage::TickAnimation`.
    TickAllAnimations(Vec<WebViewId>, CrossProcessInstant),
```

두 파일 모두 `use servo_base::cross_process_instant::CrossProcessInstant;` 가 없으면 더한다.

- [ ] **Step 2: 컴파일 실패로 고칠 자리를 전부 찾는다**

```
build_wall.cmd check -p servo-constellation-traits -p servo-script-traits -p servo-paint -p servo-constellation -p servo-script
```

Expected: ★패턴 불일치 에러★. 컴파일러가 짚는 자리가 고쳐야 할 자리의 전부다 — 짐작하지 말고 그 목록을 따르라. `components/constellation/tracing.rs:58` 의 `Self::TickAnimation(..)` 는 `..` 라 그대로 통과한다.

- [ ] **Step 3: 발신 두 곳을 고친다**

`components/paint/refresh_driver.rs` 의 `notify_animation_state_changed`(약 169행):

```rust
        let frame_time = crate::frame_time::frame_time_for_tick(
            CrossProcessInstant::now(),
            frame_time_max_age(),
        )
        .unwrap_or_else(CrossProcessInstant::now);
        if let Err(error) =
            self.constellation_sender
                .send(EmbedderToConstellationMessage::TickAnimation(
                    vec![webview_renderer.id],
                    frame_time,
                ))
        {
            warn!("Sending tick to constellation failed ({error:?}).");
        }
```

`frame_started`(약 241행)도 같은 방식으로 `frame_time` 을 만들어 싣는다.

같은 파일에 상한을 계산하는 헬퍼를 둔다:

```rust
/// 셸이 알려준 프레임 시각을 버리는 나이. ★네 주기다*** -- 그보다 오래됐으면 셸이 틱을
/// 내지 않고 있다는 뜻이므로 지금 시각으로 떨어진다(설계 문서 3.3).
fn frame_time_max_age() -> Duration {
    paint_timer_period() * 4
}
```

`use servo_base::cross_process_instant::CrossProcessInstant;` 를 더한다.

- [ ] **Step 4: constellation 이 전달하게 한다**

`constellation.rs:1490`:

```rust
            EmbedderToConstellationMessage::TickAnimation(webview_ids, frame_time) => {
                self.handle_tick_animation(webview_ids, frame_time)
            },
```

`handle_tick_animation`(3808행)의 시그니처에 `frame_time: CrossProcessInstant` 를 더하고, 3824 행의 전송을 바꾼다:

```rust
                    let _ = event_loop.send(ScriptThreadMessage::TickAllAnimations(
                        webview_ids.clone(),
                        frame_time,
                    ));
```

- [ ] **Step 5: script 가 보관하게 한다**

`script_thread.rs:2169`:

```rust
            ScriptThreadMessage::TickAllAnimations(_webviews, frame_time) => {
                note_rendering_update_reason("renderer-tick");
                // ★최신이 이긴다.★ 한 번의 `update_the_rendering` 전에 틱이 여러 번
                // 도착하면, 지금 만드는 디스플레이 리스트는 ***가장 최근 프레임***을 위한
                // 것이다. 밀린 프레임은 어차피 표출되지 않는다 -- 디스플레이가 하는 일과
                // 같다.
                self.pending_frame_time.set(Some(frame_time));
                self.set_needs_rendering_update();
            },
```

`ScriptThread` 구조체(`needs_rendering_update` 가 있는 자리, 약 469행)에 필드를 더한다:

```rust
    /// 가장 최근 틱이 알려준 프레임 시각. `update_the_rendering` 이 소비하며 비운다.
    /// 틱이 아닌 이유로 도는 회차에서는 `None` 이고, 그때는 지금 시각을 쓴다.
    pending_frame_time: Cell<Option<CrossProcessInstant>>,
```

생성자(약 1120행, `needs_rendering_update: Arc::new(AtomicBool::new(false)),` 가 있는 자리)에 `pending_frame_time: Cell::new(None),` 을 더한다.

- [ ] **Step 6: 빌드와 전체 테스트**

```
build_wall.cmd build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
build_wall.cmd test --release -p servo-paint -p servo -p servo-base --features servo/no-wgl,servo/media-gstreamer,servo/webgpu --lib
```

Expected: 빌드 성공, ★내가 손댄 파일을 가리키는 경고 0 건★, 테스트 75 + 3 + 33 통과.

`pending_frame_time` 에 ★`#[no_trace]` 를 붙이지 마라★ — `ScriptThread` 는 `#[dom_struct]` 가 아니다. (붙여야 하는 쪽은 `#[dom_struct]` 안의 비-`JSTraceable` 타입뿐이고, 그 판정은 타입을 보고 한다.)

- [ ] **Step 7: 포맷과 커밋**

```bash
git add components/paint/refresh_driver.rs components/shared/constellation/lib.rs \
        components/constellation/constellation.rs components/shared/script/lib.rs \
        components/script/script_thread.rs
git commit -F - <<'EOF'
feat(paint): 프레임 시각을 TickAnimation 에 싣는다

아직 아무도 쓰지 않는다 -- script 는 보관만 하고, 다음 커밋이 적용한다. 그래야 이 커밋이
동작을 바꾸지 않고 실기 A/B 가 깨끗해진다.

paint -> constellation -> script 세 홉을 지난다. ★마지막 홉이 문제의 자리다*** --
`TickAllAnimations` 는 reflow 를 바로 돌리지 않고 플래그만 세우고, 실제 실행은 script 가
큐에 쌓인 다른 메시지를 모두 처리한 뒤다. "도달 시각" 이 큐 길이에 좌우되는 곳이 거기다.

한 번의 `update_the_rendering` 전에 틱이 여러 번 도착하면 ★최신이 이긴다*** -- 지금 만드는
리스트는 가장 최근 프레임을 위한 것이고, 밀린 프레임은 어차피 표출되지 않는다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 5: 타임라인과 rAF 가 프레임 시각을 쓴다

§3.7, §3.8. ★둘을 한 커밋에서 바꾼다★ — 한쪽만 바꾸면 큐 지연만큼 어긋나, 없애려는 증상이 섞인 페이지에서 그대로 재현된다.

**Files:**
- Modify: `components/config/prefs.rs` (새 pref)
- Modify: `components/script/script_thread.rs:1347` (`update_the_rendering` 이 소비)
- Modify: `components/script/dom/document/document.rs` (`update_the_rendering`, `update_animations_and_send_events`, `run_the_animation_frame_callbacks`)
- Modify: `components/script/dom/animations/documenttimeline.rs` (`update`)
- Modify: `components/script/dom/window.rs:2922` (`Document::update_the_rendering` 호출)
- Modify: `components/script/dom/testing/servotestutils.rs:40, :82` (같은 호출)

**Interfaces:**
- Consumes: Task 4 의 `ScriptThread::pending_frame_time`
- Produces:
  - pref `dom_animation_timeline_use_frame_time: bool` (기본 `true`)
  - `DocumentTimeline::update(&self, window: &Window, frame_time: Option<CrossProcessInstant>)`
  - `Document::update_the_rendering(&self, cx, frame_time: Option<CrossProcessInstant>)`

- [ ] **Step 1: pref 를 더한다**

`components/config/prefs.rs` 에서 `dom_allow_scripts_to_close_windows` ★앞★에 넣는다. ★`// feature:` 주석과 그것이 설명하는 pref 사이에 끼우지 마라★ — 그 주석은 바로 다음 pref 를 가리킨다.

```rust
    /// 애니메이션의 기준 시각을 "script 가 도달한 순간" 대신 ***"그 프레임의 시각"***으로
    /// 쓴다. 타임라인(`document.timeline.currentTime`)과 rAF 콜백의 timestamp ***양쪽***에
    /// 걸린다.
    ///
    /// ★끄면 지금 동작 그대로다.★ 왜 필요한지는 설계 문서 50 -- 틱 발송은 페이싱 클럭이라
    /// 고른데, script 가 그 메시지를 처리하기까지의 큐 지연이 매번 달라 그 변동이 그대로
    /// 애니메이션 값이 된다.
    ///
    /// ★한쪽만 끄는 선택지를 두지 않았다.★ 타임라인과 rAF 가 갈라지면 둘을 섞은 페이지에서
    /// 최대 한 프레임의 흔들리는 어긋남이 생긴다 -- 이 기능이 없애려는 바로 그 증상이다.
    pub dom_animation_timeline_use_frame_time: bool,
```

기본값(`const_default` 의 `dom_allow_scripts_to_close_windows: false,` 앞):

```rust
            dom_animation_timeline_use_frame_time: true,
```

- [ ] **Step 2: `DocumentTimeline::update` 를 바꾼다**

```rust
    /// Updates the value of the `AnimationTimeline` to the current clock time.
    ///
    /// ★`frame_time` 이 있고 pref 가 켜져 있으면 그것을 쓴다.★ 이 값이 애니메이션 전체의
    /// 기준 시각이고, 여기 실린 지터가 그대로 화면의 떨림이 된다 -- 그 인과는 설계 문서
    /// 50 이 측정으로 닫았다. `None` 은 틱이 아닌 이유(scroll, resize, input, …)로 도는
    /// 회차이고, 그때는 지금 시각이 맞다.
    pub(crate) fn update(&self, window: &Window, frame_time: Option<CrossProcessInstant>) {
        let now = match frame_time {
            Some(frame_time) if pref!(dom_animation_timeline_use_frame_time) => frame_time,
            _ => CrossProcessInstant::now(),
        };
        let duration_since_time_origin = now - window.navigation_start() - self.origin_offset;
        self.animation_timeline
            .set_current_time(duration_since_time_origin);
    }
```

- [ ] **Step 3: 호출 사슬에 인자를 더한다**

컴파일러가 짚는 순서대로 고친다.

`script_thread.rs:1347` `ScriptThread::update_the_rendering`:

```rust
        // ★소비하며 비운다.★ 다음 회차가 틱이 아닌 이유로 돌면 이 값을 재사용하면 안 된다
        // -- 그때는 이미 지나간 프레임의 시각이다.
        let frame_time = self.pending_frame_time.take();
```

그리고 그 함수 안에서 `document.update_the_rendering(cx)` 를 부르는 자리(1504행)에 `frame_time` 을 넘긴다.

`Document::update_the_rendering` 시그니처:

```rust
    pub(crate) fn update_the_rendering(
        &self,
        cx: &mut js::context::JSContext,
        frame_time: Option<CrossProcessInstant>,
    ) -> (ReflowPhasesRun, ReflowStatistics) {
```

그 안에서 `update_animations_and_send_events` 와 `run_the_animation_frame_callbacks` 에 전달한다.

`update_animations_and_send_events`:

```rust
    pub(crate) fn update_animations_and_send_events(
        &self,
        cx: &mut CurrentRealm,
        frame_time: Option<CrossProcessInstant>,
    ) {
        // Only update the time if it isn't being managed by a test.
        if !self.layout_animations_test_enabled {
            self.timeline.update(self.window(), frame_time);
        }
```

`run_the_animation_frame_callbacks`:

```rust
    pub(crate) fn run_the_animation_frame_callbacks(
        &self,
        cx: &mut CurrentRealm,
        frame_time: Option<CrossProcessInstant>,
    ) {
        self.running_animation_callbacks.set(true);
        // ★타임라인과 같은 순간을 가리켜야 한다.★ 표준은 한 회차의 "update the rendering"
        // 이 하나의 timestamp 를 쓰라고 한다. 갈라지면 rAF 로 움직이는 요소와
        // `Element.animate()` 로 움직이는 요소 사이에 최대 한 프레임의 흔들리는 어긋남이
        // 생긴다 -- 이 기능이 없애려는 바로 그 증상이다.
        let timing = match frame_time {
            Some(frame_time) if pref!(dom_animation_timeline_use_frame_time) => self
                .global()
                .performance()
                .to_dom_high_res_time_stamp(frame_time),
            _ => *self.global().performance().Now(),
        };
```

★`to_dom_high_res_time_stamp` 의 반환 타입을 확인하라★ — `Finite<f64>` 인지 `f64` 인지에 따라 아래 `callback.call(cx, self, *timing)` 의 역참조가 달라진다. 선언은 `dom/performance/performance.rs:179`.

나머지 호출부 세 곳(`window.rs:2922`, `servotestutils.rs:40`, `:82`)은 ★`None` 을 넘긴다★ — 틱이 아닌 경로다.

- [ ] **Step 4: 빌드**

```
build_wall.cmd build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
```

Expected: 성공, ★내가 손댄 파일을 가리키는 경고 0 건★.

- [ ] **Step 5: 꺼진 경로와 테스트 경로를 눈으로 확인한다**

자동 테스트가 닿지 않는 두 가지다(script 크레이트에 테스트 인프라가 없다).

1. ★pref 가 꺼져 있을 때★: `DocumentTimeline::update` 와 `run_the_animation_frame_callbacks` 의 `match` 가 둘 다 `_` 가지로 떨어져 `CrossProcessInstant::now()` / `performance().Now()` 를 쓴다. ***두 자리 모두*** 같은 pref 를 본다는 것을 코드로 확인하라.
2. ★`layout_animations_test_enabled`★: 다음이 여전히 `timeline.update` 를 감싸고 있어야 한다.

```
grep -n "layout_animations_test_enabled" -A 2 components/script/dom/document/document.rs | grep -A 2 "if !"
```

Expected: `if !self.layout_animations_test_enabled { self.timeline.update(self.window(), frame_time); }`

- [ ] **Step 6: 포맷과 커밋**

```bash
git add components/config/prefs.rs components/script/script_thread.rs \
        components/script/dom/document/document.rs \
        components/script/dom/animations/documenttimeline.rs \
        components/script/dom/window.rs components/script/dom/testing/servotestutils.rs
git commit -F - <<'EOF'
feat(script): 타임라인과 rAF 가 프레임 시각을 쓴다

`DocumentTimeline::update` 가 `CrossProcessInstant::now()` 를 쓰던 자리에 틱이 실어 온
프레임 시각을 넣는다. 그 `now()` 는 ***"script 가 그 함수에 도달한 순간"*** 이고 표출
격자와 무관했다.

★타임라인과 rAF 를 한 커밋에서 바꾼다.*** 표준은 한 회차의 "update the rendering" 이
하나의 timestamp 를 쓰라고 하고, 갈라지면 둘을 섞은 페이지에서 최대 한 프레임의 흔들리는
어긋남이 생긴다 -- 이 기능이 없애려는 바로 그 증상이다. 그래서 pref 도 하나다.

★웹에 관측되는 값이 바뀐다*** -- rAF 콜백의 timestamp 다. 표준에 가까워지는 방향이지만
변경임은 분명하다.

`pending_frame_time` 은 소비하며 비운다. 다음 회차가 틱이 아닌 이유(scroll, resize,
input, display-list, …)로 돌면 이미 지나간 프레임의 시각을 재사용하게 되기 때문이다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 6: `built_at` 도 프레임 시각으로 잡는다

§3.4. ★이것을 빠뜨리면 지터가 값에서 예측 위상으로 자리만 옮긴다.★ Task 3 의 `even_build_times_give_even_zeros_despite_jittery_transit` 가 보장하려던 성질이 여기서 완성된다.

**Files:**
- Modify: `components/shared/layout/lib.rs:742` (`ReflowRequest`)
- Modify: `components/script/dom/window.rs:2764` (필드 채우기)
- Modify: `components/layout/layout_impl.rs` (배선)
- Modify: `components/layout/display_list/mod.rs:147, :236, :569` (빌더 필드 + 전달)
- Modify: `components/layout/display_list/stacking_context.rs:123, :655` (트리 필드 + 전달)
- Modify: `components/layout/display_list/paint_animation.rs:409, :487, :582, :648` (인자로 받기)

**Interfaces:**
- Consumes: Task 5 의 pref `dom_animation_timeline_use_frame_time`(bool, 기본 `true`)와 `Document::update_the_rendering(&self, cx, frame_time: Option<CrossProcessInstant>)`
- Produces: `ReflowRequest.animation_frame_time: CrossProcessInstant`

- [ ] **Step 1: `Document` 에 프레임 시각을 보관한다**

Task 5 에서 `update_the_rendering` 이 `frame_time` 을 받지만, 리플로는 그보다 뒤에 일어나고 다른 경로에서도 일어난다. 그래서 `Document` 에 마지막 값을 둔다.

`Document` 구조체에:

```rust
    /// 마지막 "update the rendering" 회차의 프레임 시각. 리플로가 `built_at` 을 이 값으로
    /// 잡으면 ***세그먼트의 값과 시간 원점이 같은 순간을 가리킨다***(설계 문서 3.4).
    #[no_trace]
    last_frame_time: Cell<Option<CrossProcessInstant>>,
```

★`#[no_trace]` 가 필요하다★ — `CrossProcessInstant` 는 `JSTraceable` 이 아니다. 반대로 `f64` 나 `Cell<f64>` 처럼 이미 `JSTraceable` 인 타입에 붙이면 `E0283` 이 난다. 같은 파일의 다른 `#[no_trace]` 필드가 선례다.

생성자에 `last_frame_time: Cell::new(None),` 을 더하고, `update_the_rendering` 머리에서 세운다:

```rust
        if frame_time.is_some() {
            self.last_frame_time.set(frame_time);
        }
```

★`is_some()` 검사가 필요하다★ — 틱이 아닌 회차의 `None` 이 직전 값을 지우면, 그 직후 리플로가 `now()` 로 떨어져 한 프레임만 원점이 튄다.

접근자:

```rust
    /// 리플로가 `built_at` 으로 쓸 시각. pref 가 꺼져 있거나 아직 틱을 못 받았으면 지금
    /// 시각이다 -- 그것이 변경 전 동작이다.
    pub(crate) fn animation_frame_time(&self) -> CrossProcessInstant {
        match self.last_frame_time.get() {
            Some(frame_time) if pref!(dom_animation_timeline_use_frame_time) => frame_time,
            _ => CrossProcessInstant::now(),
        }
    }
```

- [ ] **Step 2: `ReflowRequest` 에 필드를 더하고 채운다**

`components/shared/layout/lib.rs`, `animation_timeline_value` 바로 아래:

```rust
    /// `PaintAnimation::built_at` 으로 쓸 시각.
    ///
    /// ★`animation_timeline_value` 와 같은 순간이어야 한다.★ 전자는 세그먼트의 값을,
    /// 이것은 세그먼트의 ***시간 원점***을 정한다. 둘이 어긋나면 그 차이가 그대로 페인트측
    /// 예측의 위상 오차가 된다(설계 문서 3.4).
    pub animation_frame_time: CrossProcessInstant,
```

`components/script/dom/window.rs:2764` 의 `ReflowRequest { .. }` 에:

```rust
            animation_frame_time: document.animation_frame_time(),
```

- [ ] **Step 3: layout 안에서 `paint_animation` 까지 배선한다**

컴파일러가 짚는 자리를 따른다. 구조는 `animation_timeline_value` 와 ★나란히★ 간다 — 그 이름을 grep 해 나오는 자리마다 형제 필드를 더한다고 생각하면 된다.

```
grep -n "animation_timeline_value" components/layout/layout_impl.rs \
    components/layout/display_list/mod.rs components/layout/display_list/stacking_context.rs
```

- `display_list/mod.rs` 의 빌더 구조체(147행)와 생성자(236·264행)에 `animation_frame_time: CrossProcessInstant`
- `display_list/stacking_context.rs` 의 트리 구조체(123행)와 생성자에 같은 필드
- `layout_impl.rs` 의 `reflow_request.animation_timeline_value` 를 넘기는 자리(1003·1166·1316행)마다 `reflow_request.animation_frame_time` 도 함께

★`layout_impl.rs:823-834` 의 `SharedStyleContext` 는 건드리지 마라★ — 거기는 값(`current_time_for_animations`)이지 원점이 아니다.

- [ ] **Step 4: `paint_animation.rs` 가 인자로 받게 한다**

`opacity_binding`(409행)과 `transform_binding`(582행) 양쪽에 인자를 더한다:

```rust
    built_at: CrossProcessInstant,
```

`now: f64` ★바로 뒤★에 둔다 — 둘은 같은 순간의 두 표현이므로 떨어뜨리면 한쪽만 고치는 실수가 난다.

그리고 487·648 행의 `built_at: CrossProcessInstant::now(),` 를 `built_at,` 로 바꾼다.

호출부 둘(`mod.rs:569`, `stacking_context.rs:655`)에서 각각 `self.animation_frame_time` / `stacking_context_tree.animation_frame_time` 을 넘긴다.

- [ ] **Step 5: 빌드와 전체 테스트**

```
build_wall.cmd build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
build_wall.cmd test --release -p servo-paint -p servo -p servo-base --features servo/no-wgl,servo/media-gstreamer,servo/webgpu --lib
```

Expected: 빌드 성공, 경고 0 건, 75 + 3 + 33 통과.

★`CrossProcessInstant::now()` 가 `paint_animation.rs` 에 남아 있지 않은지 확인하라:★

```
grep -n "CrossProcessInstant::now" components/layout/display_list/paint_animation.rs
```

Expected: ★출력 없음★. 하나라도 남으면 원점이 거기서 다시 흔들린다.

- [ ] **Step 6: `TIMELINESTEP` 의 의미가 바뀌었음을 적는다**

§3.6. `web_content_animation.rs` 의 `note_timeline_step` doc 주석 끝에 더한다:

```rust
    /// ★이 지표는 더 이상 검증에 쓸 수 없다.★ `built_at` 이 프레임 시각(= 페이싱 클럭)이
    /// 된 뒤로는 그 간격이 정의상 고르게 나오므로, 구조적으로 성공을 보고한다. "값의
    /// 출처가 떤다" 를 보이기 위한 진단이었고 그 역할은 끝났다. 판정은 `ANIMSTEP` 의
    /// `dx spread` 로 한다.
```

- [ ] **Step 7: 포맷과 커밋**

```bash
git add components/shared/layout/lib.rs components/script/dom/window.rs \
        components/script/dom/document/document.rs components/layout/layout_impl.rs \
        components/layout/display_list/mod.rs components/layout/display_list/stacking_context.rs \
        components/layout/display_list/paint_animation.rs \
        components/paint/web_content_animation.rs
git commit -F - <<'EOF'
feat(layout): built_at 도 프레임 시각으로 잡는다

★이것을 빠뜨리면 지터가 값에서 예측 위상으로 자리만 옮긴다.***

페인트측 예측의 시간 원점은 `built_at` 인데(`web_content_animation.rs` 의
`animation_zero`), 그것은 `current_time_for_animations` 와 ***따로*** 잡혔다 --
`paint_animation.rs` 의 `CrossProcessInstant::now()` 두 군데. 오늘은 같은 리플로 안이라
간격이 거의 0 이지만, 값만 프레임 시각으로 되돌리면 그 간격이 ***없애려던 큐 지연 그
자체***가 된다. `divisor=4` 는 리스트 하나로 네 프레임을 외삽하므로 거기서 가장 크게
나타난다.

이제 `transit = 수신 − 프레임 시각` 이고 원점이 정확히 페이싱 틱에 대응한다. 한 커밋 앞의
`even_build_times_give_even_zeros_despite_jittery_transit` 가 보장하려던 성질이 여기서
완성된다.

`TIMELINESTEP` 은 검증에 쓸 수 없게 됐다는 것도 주석에 적었다 -- `built_at` 이 페이싱
클럭이 되면 그 간격은 정의상 고르다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 7: 셸 배선과 실기 A/B

§3.2, §3.5, §5. 여기까지는 셸이 `note_frame_time` 을 부르지 않으므로 ***모든 경로가 `now()` 폴백***이다. 이 태스크가 비로소 값을 흐르게 한다.

**Files:**
- Modify: `components/servo/examples/winit_wall/main.rs:1306` (`fire_present_tick`)
- Modify: `components/servo/lib.rs` 또는 `servo.rs` (`note_frame_time` 재노출 — `note_present_tick` 과 같은 자리)
- Modify: `etc/multigpu/run_wall_dist.ps1` (`-FrameTime` 스위치)
- Create: `etc/multigpu/tools/run_frame_time_ab.ps1`
- Modify: `etc/multigpu/make_wall_dist.ps1` (복사 목록)

**Interfaces:**
- Consumes: Task 2 의 `servo::note_frame_time(CrossProcessInstant)`
- Produces: 없음

- [ ] **Step 1: 재노출 경로를 확인한다**

```
grep -rn "note_present_tick" components/servo/
```

Expected: `servo` 크레이트가 `paint` 의 것을 재노출하는 줄. ★같은 자리에 `note_frame_time` 을 더한다.★ 그 줄이 없으면 셸의 `servo::note_frame_time` 이 컴파일되지 않는다.

- [ ] **Step 2: 셸이 부르게 한다**

`winit_wall/main.rs` 의 `fire_present_tick` ★맨 앞★:

```rust
    fn fire_present_tick(&self) {
        // ★이 순간이 이 프레임의 시각이다.★ 여기서 재야 렌더 준비 비용이 섞이지 않는다 --
        // 섞이면 그만큼이 애니메이션 값의 지터가 된다(설계 문서 3). 엔진은 이 값을
        // `TickAnimation` 에 실어 script 까지 보내고, script 는 "자기가 메시지를 처리한
        // 순간" 대신 이것을 애니메이션의 기준으로 쓴다.
        servo::note_frame_time(servo::CrossProcessInstant::now());
        servo::note_present_tick();
```

★`CrossProcessInstant` 를 셸이 어떻게 얻는지 확인하라★ — `servo` 가 재노출하지 않으면 `servo_base` 를 셸의 `Cargo.toml` 에 더하는 대신 `servo::note_frame_time()` 을 ***인자 없는*** 함수로 만들어 안에서 `now()` 를 재는 편이 낫다. 그 경우 Task 2 의 시그니처를 그렇게 바꾸고 이 계획의 다른 자리도 함께 고친 뒤 ledger 에 Ruling 으로 남긴다.

- [ ] **Step 3: `-FrameTime` 스위치를 더한다**

`run_wall_dist.ps1` 의 `param(` 블록, `[switch] $AnimStepSeq,` 아래:

```powershell
    # dom_animation_timeline_use_frame_time: 애니메이션의 기준 시각을 "script 가 도달한
    # 순간" 대신 "그 프레임의 시각" 으로 쓴다. 엔진 기본은 켜짐이므로, 이 스위치는 끌 때
    # 쓴다. 왜 필요한지는 설계 문서 50 과 2026-10-15 설계.
    [ValidateSet("on", "off", "")]
    [string] $FrameTime = "",
```

pref 를 붙이는 자리(`if ($DcompAlwaysFlush) { ... }` 아래):

```powershell
if ($FrameTime -ne "") { $argList += @("--pref", "dom_animation_timeline_use_frame_time=$($FrameTime -eq 'on')") }
```

★`$true`/`$false` 가 아니라 `True`/`False` 문자열이 된다.★ Servo 의 pref 파서가 그것을 받는지 확인하고, 안 받으면 `$(if ($FrameTime -eq 'on') { 'true' } else { 'false' })` 로 바꾼다.

- [ ] **Step 4: A/B 스크립트를 만든다**

`etc/multigpu/tools/run_frame_time_ab.ps1`. ★전부 ASCII, 해시테이블 splat★(Global Constraints). §5 의 여섯 칸이다.

```powershell
# Frame-time A/B -- six runs. ASCII only (PowerShell 5.1 reads a BOM-less .ps1 as the
# system ANSI codepage) and hashtable splatting (an array splat passes every element
# positionally, so "-Layout" would become a value instead of a parameter name).
#
# JUDGE ON dx_px spread from ANIMSTEP. TIMELINESTEP is NO LONGER valid here: built_at is
# now the pacing clock, so its gaps are even by construction.
#
#   run  divisor  frame_time  SampleLead   what it answers
#    1      1        off          on       judder baseline
#    2      1        ON           on       ★does this change fix it★
#    3      4        off          on       already-clean control
#    4      4        ON           on       does it make the clean side worse
#    5      1        off          OFF      SampleLead's existing benefit
#    6      1        ON           OFF      is SampleLead still needed once the anchor is fixed
#
# dx_px p50 must be 38.4 in all six. If one is lower, the timeline fell behind and stuck.

[CmdletBinding()]
param([int] $DurationSec = 120)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

$base = @{
    Layout = "wall_layout.multigpu.json"; Serve = $true
    Url = "wall_anim_jitter_probe.html"
    IgnoreCertErrors = $true; PageFeatures = $true
    DComp = "surface"; RefreshHz = 60; DwmAlign = 14; CommitAlign = 8
    DirectRender = $true; DcompParallelCommit = $true; PacingThread = $true
    ParallelTiles = $true; PipelineMode = "uridecodebin3"; DecoderThreads = 1
    SinkPacing = "thread"; NumaNode = "1"; NoSyncGroup = $true
    DurationSec = $DurationSec; PresentCadence = $true; DcompBindProf = $true
    AnimStepSeq = $true
}
$prefs = @("dom_webgl2_enabled=true", "dom_webgpu_enabled=true",
           "gfx_precache_shaders=true", "media_wrap_init_max_per_frame=2")
$judder = $prefs + "gfx_paint_side_animation_tick_divisor=1"

$runs = @(
    @{ N = "1/6 div=1 ft=off lead=on";  Pref = $judder; Ft = "off"; Lead = 1;  Log = "ft_1_off.err.log" }
    @{ N = "2/6 div=1 ft=ON  lead=on";  Pref = $judder; Ft = "on";  Lead = 1;  Log = "ft_2_on.err.log" }
    @{ N = "3/6 div=4 ft=off lead=on";  Pref = $prefs;  Ft = "off"; Lead = 1;  Log = "ft_3_ctrl.err.log" }
    @{ N = "4/6 div=4 ft=ON  lead=on";  Pref = $prefs;  Ft = "on";  Lead = 1;  Log = "ft_4_ctrl_on.err.log" }
    @{ N = "5/6 div=1 ft=off lead=OFF"; Pref = $judder; Ft = "off"; Lead = -1; Log = "ft_5_nolead.err.log" }
    @{ N = "6/6 div=1 ft=ON  lead=OFF"; Pref = $judder; Ft = "on";  Lead = -1; Log = "ft_6_on_nolead.err.log" }
)

foreach ($run in $runs) {
    Write-Host "`n$($run.N)  -> $($run.Log)" -ForegroundColor Cyan
    & .\run_wall_dist.ps1 @base -Pref $run.Pref -FrameTime $run.Ft -SampleLead $run.Lead -LogPath $run.Log
}

Write-Host "`nDone. Hand over the six .err.log files." -ForegroundColor Green
```

`make_wall_dist.ps1` 의 복사 목록에 한 줄 더한다(`tools\serve_http.ps1` 복사 줄 아래). ★경로의 역슬래시를 Python 으로 쓸 때 `\r` 이 캐리지 리턴이 되지 않게 하라★ — `'tools' + chr(92) + 'run_frame_time_ab.ps1'` 처럼 조립한다.

- [ ] **Step 5: 바인딩을 실제로 검증한다**

★파싱만으로는 부족하다.★ `run_wall_dist.ps1` 의 param 블록을 그대로 쓴 스텁에 통과시킨다.

```powershell
$real = 'etc\multigpu\run_wall_dist.ps1'
$tmp = "$env:TEMP\ft_ab"; Remove-Item $tmp -Recurse -Force -EA SilentlyContinue
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
Copy-Item 'etc\multigpu\tools\run_frame_time_ab.ps1' "$tmp\" -Force
$ast = [System.Management.Automation.Language.Parser]::ParseFile((Resolve-Path $real), [ref]$null, [ref]$null)
# ★[CmdletBinding()] 을 반드시 넣어라★ -- 없으면 모르는 파라미터가 $args 로 조용히 새서
# 검증이 거짓으로 통과한다.
("[CmdletBinding()]`n" + $ast.ParamBlock.Extent.Text +
 "`n`"  bound `$(`$PSBoundParameters.Count)  FrameTime=`$(`$PSBoundParameters['FrameTime']) Lead=`$(`$PSBoundParameters['SampleLead'])`"") |
    Set-Content "$tmp\run_wall_dist.ps1" -Encoding utf8
& "$tmp\run_frame_time_ab.ps1" -DurationSec 5
Remove-Item $tmp -Recurse -Force -EA SilentlyContinue
```

Expected: 여섯 줄이 찍히고 `FrameTime` 이 off/on/off/on/off/on, `Lead` 가 1/1/1/1/-1/-1.

ASCII 확인:

```powershell
(@([System.IO.File]::ReadAllBytes('etc\multigpu\tools\run_frame_time_ab.ps1') | Where-Object { $_ -gt 127 })).Count
```

Expected: ★0★.

- [ ] **Step 6: dist 를 만든다**

```
powershell -NoProfile -ExecutionPolicy Bypass -File etc\multigpu\make_wall_dist.ps1 -Force
```

`target\wall_dist\BUILD.txt` 의 `commit` 이 Task 6 의 커밋인지, `run_frame_time_ab.ps1` 이 들어갔는지 확인한다. 그리고 엔진에 pref 가 박혔는지:

```powershell
$d = [System.IO.File]::ReadAllBytes('target\wall_dist\engine\winit_wall.exe')
# 문자열 검색
([System.Text.Encoding]::ASCII.GetString($d).Contains('dom_animation_timeline_use_frame_time'))
```

Expected: `True`.

- [ ] **Step 7: 커밋하고 실기 결과를 기다린다**

```bash
git add components/servo/examples/winit_wall/main.rs components/servo/lib.rs \
        etc/multigpu/run_wall_dist.ps1 etc/multigpu/tools/run_frame_time_ab.ps1 \
        etc/multigpu/make_wall_dist.ps1
git commit -F - <<'EOF'
feat(wall): 셸이 페이싱 틱 시각을 엔진에 알린다

여기까지는 셸이 부르지 않아 모든 경로가 `now()` 폴백이었다. 이 커밋이 비로소 값을
흐르게 한다.

`fire_present_tick` 맨 앞에서 잰다. 렌더 준비 비용이 섞이면 그만큼이 애니메이션 값의
지터가 되기 때문이다.

A/B 는 ★여섯 칸★이다. 5·6 번이 `-SampleLead` 를 끈 칸인데, 그 이득의 일부가 ***흔들리는
원점을 규칙적인 합성 격자에서 평가해 가려준 것***이었는지 확인한다(설계 문서 §3.5).
6 이 2 와 같으면 그렇고, 6 이 2 보다 나쁘면 네 모니터의 vblank 가 11.2ms 에 흩어진 문제를
푸는 역할이 따로 남아 있는 것이다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

★여기서 멈춘다.★ 벽은 다른 기계에 있다. 여섯 로그를 받으면 설계 문서에 §7 로 결과를 적고, 그때 pref 기본값을 유지할지 정한다.

---

## 비-목표

- `update_intersection_observer_steps(cx, CrossProcessInstant::now())`(`script_thread.rs:1498`). 같은 성질이고 업스트림 TODO 가 달려 있지만 저더와 무관하다.
- 앵커 갱신 빈도 자체를 바꾸는 것(divisor=1 에서도 예측이 여러 프레임을 덮게 하기). ★이 교정이 실패하면 그때 간다.★
- 기동 구간(첫 40~50초)의 불안정.
- WPT 가 rAF timestamp 에 의존하는지 확인하는 것. ★실기 검증과 별개로 봐야 하지만, 이 계획의 범위는 아니다.★

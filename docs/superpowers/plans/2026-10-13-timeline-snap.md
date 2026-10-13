# 애니메이션 타임라인 격자 스냅 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 애니메이션 타임라인 값을 "스크립트가 도착한 순간" 이 아니라 "리프레시 주기의 격자점" 으로 잡아, 틱 도착 지터가 애니메이션 값에 실리지 않게 한다.

**Architecture:** `DocumentTimeline::update` 가 `CrossProcessInstant::now()` 를 그대로 쓰는 자리에 **단조 격자 스냅**을 끼운다. 직전 값에 주기를 더해 나가되 실제 시각을 넘어서지 않게 따라잡는 방식(`while next <= raw { next += period }`)이라 장기 드리프트가 없다. 순수 함수는 테스트가 도는 `servo-base` 에 두고, `script` 는 그것을 부르기만 한다. pref 로 끄면 지금 동작 그대로다.

**Tech Stack:** Rust 1.95.0 / `servo-base`(`CrossProcessInstant`) / `servo-script` / `cargo test -p servo-base`

**Spec:** `docs/superpowers/specs/2026-09-21-per-output-commit-phase-design.md` §50 — 이 계획의 근거 전부가 거기 있다. 아래 "배경" 이 그 요약이다.

## 배경 — 측정이 특정한 것

`DocumentTimeline::update`(`components/script/dom/animations/documenttimeline.rs:71`):

```rust
pub(crate) fn update(&self, window: &Window) {
    let duration_since_time_origin =
        CrossProcessInstant::now() - window.navigation_start() - self.origin_offset;
    self.animation_timeline.set_current_time(duration_since_time_origin);
}
```

`CrossProcessInstant::now()` 는 ***"스크립트가 이 함수에 도달한 순간"*** 이고 표출 격자와
무관하다. 도달은 `TickAnimation` → constellation → script thread 큐를 거치므로 경로 지연이
매번 다르다. 그 지터가 그대로 애니메이션 값이 된다.

실측(log_ani_debug_04/20, 각 300 초):

```
                  dt_ms (표출)        built_at (값의 출처)   dx_px (화면값)
divisor=4 (깨끗)  p50 16.70  이탈 11.89%   p50 66.70  이탈 ★0.00%★   이탈 1.81%
divisor=1 (저더)  p50 16.70  이탈  5.69%   p50 16.80  이탈 ★3.81%★   이탈 3.46%
```

★`built_at` 이탈이 `dx` 이탈을 거의 1:1 로 끌고 간다.★ 그리고 표출 간격은 깨끗한 쪽이
오히려 더 거칠다 — 표출 격자는 저더와 상관이 없다.

15Hz 가 고른 것은 ***여유*** 때문이다. 66.7ms 틱이면 리플로(1~2ms)를 마치고도 64ms 가 남아
지연이 몇 ms 흔들려도 간격이 유지된다. 16.7ms 틱이면 같은 변동이 주기의 상당 비율이 된다.
부하가 아니다 — `busy_ms` 는 0.35 와 0.39 로 같다.

## Global Constraints

- 기본은 **꺼짐**. `dom_animation_timeline_snap_hz` 가 `0`(기본)이면 **지금과 완전히 동일한 코드 경로**여야 한다.
- 장기 드리프트가 없어야 한다. 스냅 값은 실제 시각을 따라가되 앞서지 않는다.
- `layout_animations_test_enabled` 경로는 건드리지 않는다 — 테스트가 타임라인을 직접 조종한다(`documenttimeline.rs:55`, `document.rs:4580`).
- 순수 함수는 `servo-base` 에 둔다. `servo-script` 에는 테스트 인프라가 없다(`#[cfg(test)]` 0 건이고 `cargo test -p servo-script` 는 무관한 의존성 `aws-lc-sys` 에서 막힌다).
- 주석은 평어체, 한국어. 커밋 메시지도 평어체.
- rustfmt 기준선: 손대는 파일마다 변경 전 건수를 먼저 재고 그 수를 유지한다.
- 커밋 트레일러: `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`
- 다음 파일은 **절대 스테이징하지 않는다**: `Cargo.lock`, `etc/multigpu/config/wall_layout.example_1x1.json`, `tests/html/multigpu_standard_video_extended_probe.html`, `tests/html/multigpu_standard_video_rtsp_probe.html`. `git add -A` / `git add .` 금지.

## Review Focus

사양이 함축하지만 어느 태스크의 테스트도 건드리지 않는, 사람을 물 가능성이 큰 것들. 각 줄은
그 코드를 가진 태스크에 테스트로 들어간다.

1. **애니메이션이 느려지거나 빨라진다.** 스냅이 실제 시각보다 뒤처진 채 고정되면 모든 애니메이션이 영구히 느려진다 — 저더보다 훨씬 나쁘다. 장기적으로 raw 를 따라가야 한다. → Task 1
2. **긴 정지 뒤 폭주.** 탭 전환·디버거·콘텐츠 전환으로 수 초가 비면, 밀린 격자점을 하나씩 밟느라 애니메이션이 빨리 감기로 돈다. 디스플레이가 하는 일은 그 칸들을 버리는 것이다. → Task 1
3. **시각이 뒤로 간다.** 타임라인이 역행하면 애니메이션이 되감기고, WAAPI 의 `currentTime` 이 음수 델타를 본다. 스냅은 단조여야 한다. → Task 1
4. **첫 호출.** 직전 값이 없다. 거기서 0 이나 epoch 를 쓰면 애니메이션이 과거에서 시작해 순간이동한다. → Task 1
5. **pref 가 꺼져 있다.** 기본값이고, 그 경로는 지금 코드와 바이트 단위로 같아야 한다 — 이 변경의 위험을 되돌릴 수 있는 유일한 수단이다. → Task 2
6. **`layout_animations_test_enabled`.** 테스트가 `advance_specific` 으로 타임라인을 직접 민다. 스냅이 그 값을 덮으면 레이아웃 테스트가 조용히 깨진다. → Task 2

---

## 확인해 둔 사실

계획이 기대는 API 를 미리 확인했다 — 틀리면 Task 1 이 통째로 막힌다.

| 가정 | 확인 |
|---|---|
| `servo-base` 에서 테스트가 돈다 | `#[test]` ★33 개★ |
| `CrossProcessInstant - CrossProcessInstant` | `type Output = time::Duration` (`cross_process_instant.rs:47`) |
| 그 결과에 `.as_seconds_f64()` | 있다 (`web_content_animation.rs:517` 에서 이미 쓴다) |
| `Duration::seconds_f64` | 있다 (`documenttimeline.rs:95` 에서 이미 쓴다) |
| `lib.rs` 모듈 선언 자리 | `pub mod cross_process_instant;` 가 12 행 — 그 **앞**이 알파벳 순 |

## 파일 구조

| 파일 | 책임 |
|---|---|
| `components/shared/base/animation_timeline_snap.rs` (신규, 약 90줄) | 순수 스냅 함수 + 단위 테스트 |
| `components/shared/base/lib.rs` (수정) | 모듈 선언 |
| `components/config/prefs.rs` (수정) | `dom_animation_timeline_snap_hz` pref |
| `components/script/dom/animations/documenttimeline.rs` (수정) | 스냅을 끼우고 직전 값을 들고 있음 |
| `etc/multigpu/run_wall_dist.ps1` (수정) | `-TimelineSnap <hz>` 스위치 |

`servo-base` 에 두는 이유: `CrossProcessInstant` 가 거기 있고, 테스트가 돈다. 함수는
`CrossProcessInstant` 를 모르는 **순수 산술**이라 `f64` 초 단위로만 다룬다 — 호출자가 변환한다.

---

### Task 1: 스냅 함수

**Files:**
- Create: `components/shared/base/animation_timeline_snap.rs`
- Modify: `components/shared/base/lib.rs`
- Test: 같은 파일의 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 없음(순수 산술)
- Produces: `pub fn snap_animation_timeline(previous: Option<f64>, raw: f64, period: f64) -> f64`
  - `previous`: 직전에 돌려준 값(초). 첫 호출이면 `None`.
  - `raw`: 지금 시각(초, time origin 기준).
  - `period`: 격자 간격(초). `0` 이하면 스냅하지 않는다.
  - 반환: 쓸 타임라인 값(초).

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`components/shared/base/animation_timeline_snap.rs` 를 만들고 테스트만 넣는다.

```rust
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 애니메이션 타임라인 값을 리프레시 격자에 맞춘다.

#[cfg(test)]
mod tests {
    use super::*;

    const P: f64 = 1.0 / 60.0; // 16.667ms

    /// 첫 호출은 기준점이다. ★여기서 0 이나 epoch 를 쓰면 애니메이션이 과거에서 시작해
    /// 순간이동한다.★
    #[test]
    fn first_call_uses_the_raw_time() {
        assert_eq!(snap_animation_timeline(None, 123.456, P), 123.456);
    }

    /// 꺼져 있으면(period <= 0) 손대지 않는다. ★이 경로가 지금 동작이고, 되돌릴 수 있는
    /// 유일한 수단이다.★
    #[test]
    fn period_zero_passes_the_raw_time_through() {
        assert_eq!(snap_animation_timeline(Some(1.0), 1.019, 0.0), 1.019);
        assert_eq!(snap_animation_timeline(Some(1.0), 1.019, -1.0), 1.019);
    }

    /// ★지터를 흡수한다.★ 직전 값에서 한 주기 뒤가 raw 를 막 넘었으면 그 격자점을 준다 --
    /// raw 가 16.0ms 뒤였든 17.5ms 뒤였든 같은 값이 나온다. 이것이 이 함수의 전부다.
    #[test]
    fn absorbs_arrival_jitter_within_one_period() {
        let base = 1.0;
        let early = snap_animation_timeline(Some(base), base + 0.0160, P);
        let late = snap_animation_timeline(Some(base), base + 0.0175, P);
        assert_eq!(early, late, "같은 격자점이어야 한다");
        assert!((early - (base + P)).abs() < 1e-9, "한 주기 뒤여야 한다: {early}");
    }

    /// ★단조.★ 시각이 뒤로 가도 타임라인은 되감기지 않는다 -- 되감기면 애니메이션이
    /// 역행하고 WAAPI 의 `currentTime` 이 음수 델타를 본다.
    #[test]
    fn never_goes_backwards() {
        let previous = 5.0;
        assert_eq!(snap_animation_timeline(Some(previous), 4.9, P), previous);
        assert_eq!(snap_animation_timeline(Some(previous), 5.0, P), previous);
    }

    /// ★긴 정지 뒤에는 밀린 칸을 버린다.★ 하나씩 밟으면 애니메이션이 빨리 감기로 돈다 --
    /// 디스플레이가 하는 일은 지나간 칸을 버리는 것이다.
    #[test]
    fn a_long_stall_skips_the_missed_grid_points() {
        let previous = 1.0;
        let raw = previous + 2.0; // 2 초 정지 = 120 칸
        let got = snap_animation_timeline(Some(previous), raw, P);
        assert!(got > raw - P, "raw 바로 앞 격자점이어야 한다: {got}");
        assert!(got <= raw, "raw 를 넘으면 안 된다: {got}");
    }

    /// ★장기 드리프트가 없다.★ 뒤처진 채 고정되면 모든 애니메이션이 영구히 느려진다 --
    /// 저더보다 훨씬 나쁘다. 지터를 섞어 1,000 스텝을 돌려도 raw 를 따라가야 한다.
    #[test]
    fn does_not_drift_over_a_thousand_steps() {
        let mut previous = 0.0;
        let mut raw = 0.0;
        for index in 0..1000 {
            // 실측 범위의 지터: 주기 ±1ms 를 번갈아.
            raw += P + if index % 2 == 0 { 0.001 } else { -0.001 };
            previous = snap_animation_timeline(Some(previous), raw, P);
        }
        let lag = raw - previous;
        assert!(lag >= 0.0, "앞서면 안 된다: {lag}");
        assert!(lag < P, "한 주기 이상 뒤처지면 안 된다: {lag}");
    }
}
```

- [ ] **Step 2: 실패를 확인한다**

```
cargo test -p servo-base animation_timeline_snap
```

빌드 환경은 `scripts/servo_env.ps1` 을 dot-source 한 세션에서 `servo/` 안에서 돌린다
(CLAUDE.md 의 "Build & run"). 모듈이 아직 선언되지 않았으므로 기대는 **컴파일 실패** —
`cannot find function 'snap_animation_timeline'`. 모듈 선언이 없어 파일이 아예 안 보이면
테스트가 0 개로 통과하는데, 그것도 RED 가 아니다. 그 경우 Step 3 의 `lib.rs` 선언을 먼저
넣고 Step 2 를 다시 돌린다.

- [ ] **Step 3: `lib.rs` 에 모듈을 선언하고 함수를 쓴다**

`components/shared/base/lib.rs` 의 모듈 목록에 알파벳 순으로 넣는다. 기존 줄을 먼저 확인한다:

```
grep -n "^pub mod " components/shared/base/lib.rs
```

`cross_process_instant` 앞에 들어간다:

```rust
pub mod animation_timeline_snap;
pub mod cross_process_instant;
```

그리고 `animation_timeline_snap.rs` 의 `#[cfg(test)] mod tests` **위**에 넣는다:

```rust
/// 타임라인 값을 리프레시 격자에 맞춘다.
///
/// ★왜 필요한가.★ 애니메이션 값은 `DocumentTimeline::update` 가 `CrossProcessInstant::now()`
/// 로 잡는데, 그것은 ***"스크립트가 그 함수에 도달한 순간"*** 이고 표출 격자와 무관하다.
/// 도달은 `TickAnimation` -> constellation -> script thread 큐를 거치므로 경로 지연이 매번
/// 다르고, 그 지터가 그대로 애니메이션 값이 된다. 실측(설계 문서 50): 틱이 60Hz 일 때 값의
/// 출처가 3.81% 이탈하고 화면값이 3.46% 이탈한다 -- 거의 1:1 이다. 15Hz 일 때는 여유가
/// 지연을 흡수해 이탈이 0.00% 다.
///
/// ★`while next <= raw { next += period }` 패턴.★ 셸의 표출 클럭(`drive_present_clock_timer`)
/// 과 `TimerRefreshDriver::observe_next_frame` 이 같은 모양이고, 같은 이유다 -- "지금부터 한
/// 주기" 로 재면 매 사이클에 처리 지연이 누적되어 ***구조적으로 느려진다.*** 기준점에 주기를
/// 더해 나가면 표류하지 않는다.
///
/// ★밀린 칸은 버린다.★ 긴 정지 뒤에 하나씩 밟으면 이미 지나간 순간들을 위한 프레임이
/// 쏟아져 애니메이션이 빨리 감기로 돈다. 디스플레이가 하는 일이 그것이다.
///
/// `period` 가 0 이하면 스냅하지 않고 `raw` 를 그대로 준다 -- pref 가 꺼진 경로다.
pub fn snap_animation_timeline(previous: Option<f64>, raw: f64, period: f64) -> f64 {
    if period <= 0.0 {
        return raw;
    }
    let Some(previous) = previous else {
        // 첫 호출은 기준점이다. 격자를 여기서 시작한다.
        return raw;
    };
    if raw <= previous {
        // ★단조.★ 시각이 뒤로 갔거나 같은 순간에 두 번 불렸다. 되감지 않는다.
        return previous;
    }
    let mut next = previous;
    while next + period <= raw {
        next += period;
    }
    next
}
```

- [ ] **Step 4: 통과를 확인한다**

```
cargo test -p servo-base animation_timeline_snap
```

기대: `6 passed`.

`does_not_drift_over_a_thousand_steps` 가 "한 주기 이상 뒤처짐" 으로 실패하면 `while` 조건이
틀린 것이다 — `next + period <= raw` 여야 하고 `next <= raw` 가 아니다. 후자는 raw 를 넘는
격자점을 준다.

- [ ] **Step 5: 포맷과 커밋**

```
rustfmt --edition 2024 --check components/shared/base/animation_timeline_snap.rs components/shared/base/lib.rs
```

기대: 새 파일 0 건. `lib.rs` 는 변경 전 건수를 먼저 재서 그 수를 유지한다.

```bash
git add components/shared/base/animation_timeline_snap.rs components/shared/base/lib.rs
git commit -F - <<'EOF'
feat(base): 애니메이션 타임라인을 리프레시 격자에 맞추는 함수를 만든다

아직 아무도 쓰지 않는다 -- 다음 커밋이 끼운다.

애니메이션 값은 `DocumentTimeline::update` 가 `CrossProcessInstant::now()` 로 잡는데,
그것은 ***"스크립트가 그 함수에 도달한 순간"*** 이고 표출 격자와 무관하다. 그 도달 지연의
변동이 그대로 애니메이션 값이 된다. 실측(설계 문서 50): 틱이 60Hz 일 때 값의 출처가
3.81% 이탈하고 화면값이 3.46% 이탈한다 -- 거의 1:1 이다.

`while next + period <= raw { next += period }` 패턴이다. 셸의 표출 클럭과
`TimerRefreshDriver` 가 같은 모양이고 같은 이유다 -- "지금부터 한 주기" 로 재면 처리 지연이
누적되어 구조적으로 느려진다.

테스트 여섯은 이 자리에서 실제로 틀릴 수 있는 것들을 잡는다 -- ★장기 드리프트★(뒤처진 채
고정되면 모든 애니메이션이 영구히 느려진다, 저더보다 나쁘다), ★긴 정지 뒤 폭주★(밀린 칸을
버려야 한다), ★단조★(역행하면 WAAPI 의 currentTime 이 음수 델타를 본다), 첫 호출,
꺼진 경로, 그리고 본래 목적인 지터 흡수.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 2: pref 와 타임라인 배선

**Files:**
- Modify: `components/config/prefs.rs`
- Modify: `components/script/dom/animations/documenttimeline.rs`

**Interfaces:**
- Consumes: Task 1 의 `servo_base::animation_timeline_snap::snap_animation_timeline(Option<f64>, f64, f64) -> f64`
- Produces: pref `dom_animation_timeline_snap_hz: i64`(기본 `0` = 꺼짐)

- [ ] **Step 1: pref 를 더한다**

`components/config/prefs.rs` 에서 `dom_` 으로 시작하는 pref 들 사이, 알파벳 순 자리에 넣는다.
선언부와 기본값 두 군데다 — 기존 pref 하나를 `grep -n dom_webgpu_enabled components/config/prefs.rs`
로 찾아 그 두 자리의 모양을 그대로 따른다.

선언:

```rust
    /// 애니메이션 타임라인 값을 이 주파수의 격자에 맞춘다. `0`(기본)이면 끈다.
    ///
    /// ★끄면 지금 동작 그대로다.★ `DocumentTimeline::update` 가 `CrossProcessInstant::now()`
    /// 를 그대로 쓰는데, 그것은 "스크립트가 그 함수에 도달한 순간" 이라 틱 도착 지연의
    /// 변동이 그대로 애니메이션 값이 된다(설계 문서 50). 켜면 그 값을 격자에 스냅해
    /// 지터를 흡수한다.
    ///
    /// 보통 `gfx_refresh_hz` 와 같은 값을 준다. 다르게 주면 애니메이션이 그 격자로 양자화
    /// 되므로, 표출 주기보다 낮게 주면 눈에 보인다.
    pub dom_animation_timeline_snap_hz: i64,
```

기본값:

```rust
            dom_animation_timeline_snap_hz: 0,
```

- [ ] **Step 2: 실패하는 테스트를 쓴다**

`servo-script` 에는 테스트 인프라가 없으므로(Global Constraints) 여기서 검증하는 것은
**배선이 컴파일되고 pref 가 보인다**는 것뿐이다. 실패를 보는 방법은 컴파일이다.

`components/script/dom/animations/documenttimeline.rs` 의 `update` 를 아래로 바꾼다.
이 단계에서는 **필드를 아직 더하지 않는다** — 그래야 컴파일이 실패하고, 그 실패가
"필드가 없다" 임을 확인할 수 있다.

```rust
    /// Updates the value of the `AnimationTimeline` to the current clock time.
    ///
    /// ★`dom_animation_timeline_snap_hz` 가 켜져 있으면 격자에 맞춘다.★ 이 값이 애니메이션
    /// 전체의 기준 시각이고, 여기 실린 지터가 그대로 화면의 떨림이 된다 -- 그 인과는
    /// 설계 문서 50 이 측정으로 닫았다. 꺼져 있으면(기본) 아래 `snap_animation_timeline` 이
    /// `raw` 를 그대로 돌려주므로 ***지금 동작과 같다.***
    pub(crate) fn update(&self, window: &Window) {
        let raw = CrossProcessInstant::now() - window.navigation_start() - self.origin_offset;
        let hz = pref!(dom_animation_timeline_snap_hz);
        let period = if (1..=1000).contains(&hz) {
            1.0 / hz as f64
        } else {
            0.0
        };
        let snapped = servo_base::animation_timeline_snap::snap_animation_timeline(
            self.snap_previous.get(),
            raw.as_seconds_f64(),
            period,
        );
        self.snap_previous.set(Some(snapped));
        self.animation_timeline
            .set_current_time(Duration::seconds_f64(snapped));
    }
```

- [ ] **Step 3: 실패를 확인한다**

```
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
```

기대: **컴파일 실패** — `no field 'snap_previous' on type '&DocumentTimeline'`.

- [ ] **Step 4: 필드를 더한다**

`DocumentTimeline` 구조체(`documenttimeline.rs:21` 부근)에 넣는다. 그 구조체는
`#[dom_struct]` 라 GC 추적 대상이므로, `f64` 같은 평범한 값에는 `#[no_trace]` 가 필요하다 —
같은 파일의 `AnimationTimeline` 이 `current_time` 에 그렇게 하고 있다.

```rust
    /// 직전에 돌려준 스냅 값(초). `dom_animation_timeline_snap_hz` 가 꺼져 있으면 쓰이지
    /// 않는다. ★격자는 이 값에서 이어진다★ -- 매번 지금 시각에서 다시 계산하면 스냅이
    /// 의미가 없다.
    #[no_trace]
    snap_previous: Cell<Option<f64>>,
```

`Cell` import 가 없으면 더한다(`use std::cell::Cell;`). 그리고 이 구조체를 만드는 두
자리(`new_with_duration` 과 `new`, 각각 `Self { animation_timeline: ..., origin_offset: ... }`)에
`snap_previous: Cell::new(None),` 을 더한다.

- [ ] **Step 5: 통과를 확인한다**

```
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
cargo test -p servo-base animation_timeline_snap
cargo test --release -p servo-paint -p servo --features servo/no-wgl,servo/media-gstreamer,servo/webgpu --lib
```

기대: 빌드 성공, **새 경고 0 건**, `servo-base` 6 passed, paint/servo 71+3 passed.

- [ ] **Step 6: 꺼진 경로와 테스트 경로를 눈으로 확인한다**

자동 테스트가 닿지 않는 두 가지라 코드를 직접 읽어 확인한다.

1. **pref 가 꺼져 있을 때**: `hz` 가 `0` 이면 `period` 가 `0.0` 이고
   `snap_animation_timeline` 의 첫 줄이 `raw` 를 그대로 돌려준다. `snap_previous` 는
   갱신되지만 읽히지 않는다. ***값 경로가 지금과 같다.***
2. **`layout_animations_test_enabled`**: `Document::update_animations_and_send_events`
   (`document.rs:4579`)가 그 pref 일 때 `self.timeline.update(..)` 를 **아예 부르지 않는다**.
   그러므로 스냅이 `advance_specific` 의 값을 덮지 않는다. 그 분기를 열어 확인한다:

```
grep -n "layout_animations_test_enabled" -A 3 components/script/dom/document/document.rs | head -8
```

기대: `if !self.layout_animations_test_enabled { self.timeline.update(self.window()); }`

- [ ] **Step 7: 포맷과 커밋**

손대기 전 기준선을 먼저 잰다:

```
rustfmt --edition 2024 --check components/config/prefs.rs components/script/dom/animations/documenttimeline.rs
```

변경 후 같은 수여야 한다.

```bash
git add components/config/prefs.rs components/script/dom/animations/documenttimeline.rs
git commit -F - <<'EOF'
feat(script): 애니메이션 타임라인을 격자에 맞춘다 (dom_animation_timeline_snap_hz)

`DocumentTimeline::update` 가 `CrossProcessInstant::now()` 를 그대로 쓰던 자리에 스냅을
끼운다. 그 값은 ***"스크립트가 그 함수에 도달한 순간"*** 이고 표출 격자와 무관하다 --
도달은 `TickAnimation` -> constellation -> script thread 큐를 거치므로 경로 지연이 매번
다르고, 그 지터가 그대로 애니메이션 값이 된다.

실측(설계 문서 50, log_ani_debug_04/20):

                  built_at (값의 출처)   dx_px (화면값)
    divisor=4     p50 66.70  이탈 0.00%     이탈 1.81%
    divisor=1     p50 16.80  이탈 ★3.81%★   이탈 ★3.46%★

★기본은 꺼짐이다.★ `0` 이면 `snap_animation_timeline` 이 첫 줄에서 `raw` 를 돌려주므로 값
경로가 지금과 같다 -- 이 변경의 위험을 되돌릴 수 있는 유일한 수단이다.

`layout_animations_test_enabled` 경로는 닿지 않는다. `update_animations_and_send_events`
가 그 pref 일 때 `timeline.update` 를 아예 부르지 않으므로, 스냅이 `advance_specific` 의
값을 덮을 수 없다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 3: dist 스위치와 실기 검증

**Files:**
- Modify: `etc/multigpu/run_wall_dist.ps1`
- Modify: `docs/superpowers/specs/2026-09-21-per-output-commit-phase-design.md`

**Interfaces:**
- Consumes: Task 2 의 pref `dom_animation_timeline_snap_hz`
- Produces: 없음

- [ ] **Step 1: 스위치를 더한다**

`run_wall_dist.ps1` 의 `param(` 블록에서 `[switch] $AnimStepSeq,` 바로 아래에 넣는다
(그 스위치는 §50 의 계측용으로 이미 있다):

```powershell
    # dom_animation_timeline_snap_hz: 애니메이션 타임라인 값을 이 주파수의 격자에 맞춘다.
    # 0 이면 끈다(기본). 보통 -RefreshHz 와 같은 값을 준다. 왜 필요한지는 설계 문서 50 --
    # 타임라인 값이 "스크립트가 도달한 순간" 이라 틱 도착 지연의 변동이 그대로 화면의
    # 떨림이 된다.
    [int]    $TimelineSnap = 0,
```

그리고 pref 를 넘기는 자리 — `--pref gfx_wall_pacing_thread=...` 가 있는 `$argList` 근처,
조건부 pref 들이 모인 곳(`if ($SinkPacing -ne "") { ... }` 패턴)에 넣는다:

```powershell
if ($TimelineSnap -gt 0) { $argList += @("--pref", "dom_animation_timeline_snap_hz=$TimelineSnap") }
```

- [ ] **Step 2: 스크립트가 파싱되고 스위치가 보이는지 확인한다**

```powershell
$errs = $null
$null = [System.Management.Automation.Language.Parser]::ParseFile(
    'W:\servo_multigpu-tiled-wall\etc\multigpu\run_wall_dist.ps1', [ref]$null, [ref]$errs)
if ($errs) { $errs | Select-Object -First 3 } else { 'OK' }
(Get-Command 'W:\servo_multigpu-tiled-wall\etc\multigpu\run_wall_dist.ps1').Parameters['TimelineSnap'].ParameterType.Name
```

기대: `OK` 와 `Int32`.

- [ ] **Step 3: dist 를 만든다**

```
powershell -NoProfile -ExecutionPolicy Bypass -File etc\multigpu\make_wall_dist.ps1 -Force
```

`target\wall_dist\BUILD.txt` 의 `commit` 이 Task 2 의 커밋인지 확인한다.

- [ ] **Step 4: 실기에서 A/B 를 돌린다**

벽은 이 저장소에서 자동으로 돌릴 수 없다(CLAUDE.md: "There is no interactive GUI test
harness"). 판정은 로그와 육안이다. **세 번** 돌린다 — 두 번은 저더 조건에서 스냅 on/off,
한 번은 기존 대조군이다.

```powershell
# ① 저더 조건 + 스냅 끔 (= 지금까지의 저더 재현)
... --pref gfx_paint_side_animation_tick_divisor=1 -AnimStepSeq -LogPath snap_off.err.log
# ② 저더 조건 + 스냅 켬
... --pref gfx_paint_side_animation_tick_divisor=1 -TimelineSnap 60 -AnimStepSeq -LogPath snap_on.err.log
# ③ 대조군 (기본 divisor=4, 스냅 끔)
... -AnimStepSeq -LogPath snap_control.err.log
```

나머지 플래그는 평소 조합 그대로다:

```
-Layout wall_layout.multigpu.json -Serve -Url "wall_anim_jitter_probe.html"
-IgnoreCertErrors -PageFeatures
-Pref dom_webgl2_enabled=true,dom_webgpu_enabled=true,gfx_precache_shaders=true,media_wrap_init_max_per_frame=2
-DComp surface -RefreshHz 60 -DwmAlign 14 -CommitAlign 8 -SampleLead 1 -DirectRender
-DcompParallelCommit -PacingThread -ParallelTiles -PipelineMode uridecodebin3 -DecoderThreads 1
-SinkPacing thread -NumaNode 1 -NoSyncGroup -DurationSec 120 -PresentCadence -DcompBindProf
```

읽을 지표 — `TIMELINESTEP` 과 `ANIMSTEPSEQ` 를 §50 과 같은 방식으로 낸다:

| 지표 | ① 스냅 끔 | ② 스냅 켬 (기대) | ③ 대조군 |
|---|---|---|---|
| `built_at_gap_ms` 이탈 | 약 3.8% | ★0% 에 가깝게★ | 0.00% |
| `dx_px` 이탈 | 약 3.5% | ★1.8% 수준으로★ | 1.81% |
| `built_at_gap_ms` p50 | 16.8 | ★16.7 (격자)★ | 66.7 |
| 육안 | 저더 | ★저더 없음★ | 저더 없음 |

★②에서 `dx` 이탈이 ③ 수준으로 내려가면 성공이다.★ 안 내려가면 타임라인이 원인의 전부가
아니라는 뜻이고, 그때는 `built_at_gap` 이 실제로 고르게 됐는지부터 본다 — 고른데 `dx` 가
떨면 `built_at` 과 `current_time_for_animations` 사이에서 어긋나는 것이므로 stylo 쪽으로
내려가야 한다.

★애니메이션이 느려지지 않았는지도 본다.★ `dx_px` 의 **p50** 이 ①②③ 모두 38.4 여야 한다.
②에서만 낮으면 스냅이 뒤처진 채 고정된 것이고, 그건 Task 1 의 드리프트 테스트가 놓친
경우다 — 그 테스트를 실제 수열로 다시 쓴다.

- [ ] **Step 5: 결과를 기록한다**

설계 문서 끝에 `## 51. 타임라인을 격자에 맞춘다` 를 추가한다. 담을 것:

- 무엇을 바꿨는지(`DocumentTimeline::update` 의 한 줄 + pref)
- Step 4 의 표를 **실측값으로** 채운 것
- `dx_px` p50 이 세 실행 모두 38.4 인지(= 애니메이션 속도가 유지되는지)
- 켜는 것을 기본으로 할지에 대한 판단과 근거

- [ ] **Step 6: 커밋**

```bash
git add etc/multigpu/run_wall_dist.ps1 docs/superpowers/specs/2026-09-21-per-output-commit-phase-design.md
git commit -F - <<'EOF'
docs(wall): 타임라인 격자 스냅의 실기 결과를 기록한다

51. `-TimelineSnap` 스위치와 실측 A/B. 켜는 것을 기본으로 할지에 대한 판단도 적는다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## 비-목표

- **`TickAnimation` 메시지에 타임스탬프를 싣는 것.** HTML 표준의 `frameTimestamp` 에 더 가까운 해법이고 장기적으로는 그쪽이 옳지만, paint → shared → constellation → script 네 크레이트를 지나간다. 이 계획은 **script 안에서 닫히는 최소 교정**만 한다. 실측으로 효과가 확인되면 그때 표준형으로 옮기는 것이 순서다.
- **`rAF` 콜백의 timestamp.** 스냅된 타임라인을 쓰게 되므로 자연히 같이 움직이지만, 그 값을 따로 검증하지는 않는다.
- **스냅을 기본으로 켜는 것.** Task 3 의 실측 전에는 결정하지 않는다.
- **§49 의 남은 한계**(`-ParallelTiles` 에서 타일 0 만 한 시계, `PaintTimerThread` 가 caret 용으로 잔존). 이 계획과 무관하다.

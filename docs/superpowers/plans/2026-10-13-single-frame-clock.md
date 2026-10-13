# 표출 클럭 일원화 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `winit_wall` 에서 같은 주기로 독립해 도는 두 프레임 클럭(페이싱 스레드와 `PaintTimerThread`)을 하나로 합쳐, rAF 틱과 표출 틱이 같은 시계를 타게 한다.

**Architecture:** Servo 의 `RefreshDriver` 훅에 끼울 `PacingRefreshDriver` 를 새로 만든다. 이 드라이버는 스레드를 띄우지 않는다 — 콜백을 모아 두었다가 **이미 존재하는 페이싱 틱이 발사**한다. 기존 `DwmVsyncRefreshDriver`(같은 디렉터리, 113줄)가 그대로 모델이고, 차이는 "자기 스레드에서 `DwmFlush` 를 기다린다" 대신 "남이 부르면 발사한다" 하나다.

**Tech Stack:** Rust 1.95.0 / Windows / `servo::RefreshDriver` trait / `cargo test -p servo --example winit_wall`

**Spec:** `docs/superpowers/specs/2026-09-21-per-output-commit-phase-design.md` — 이 변경의 근거는 §44~48, 특히 §47(남긴 두 커밋)과 §48(남은 질문). 아래 "배경" 이 그 요약이다.

## 배경 — 왜 지금 두 시계인가

`BaseRefreshDriver::notify_will_paint` 는 `Painter::render` 안에서 불리고(`painter.rs:2021`,
`last_render_started_at` 를 찍는 바로 다음 줄), rAF 틱을 거기서 발사한다. **그 부분은 이미
페이싱 박자를 탄다.** 문제는 그 함수의 끝이다:

```rust
pub(crate) fn notify_will_paint(&self, painter: &mut Painter) {
    observers.retain(|observer| observer.frame_started(painter));   // rAF 틱
    if still_has_observers { self.observe_next_frame(); }           // ★여기★
}

fn observe_next_frame(&self) {
    self.waiting_for_frame.store(true, Ordering::Relaxed);
    self.refresh_driver.observe_next_frame(Box::new(move || {
        waiting_for_frame.store(false, Ordering::Relaxed);
        event_loop_waker.wake();                                    // ★WakerEvent::Engine★
    }));
}
```

기본 `RefreshDriver` 는 `TimerRefreshDriver` 이고 전용 스레드(`PaintTimerThread`)에서
`gfx_refresh_hz` 주기로 돈다. 그래서 **렌더가 끝날 때마다 한 주기 뒤에 자기를 깨우도록
예약**하고, 그 주기가 페이싱 스레드와 **같다**:

```
WallPacing 스레드   : period 마다  WakerEvent::Tick
PaintTimerThread   : period 마다  WakerEvent::Engine      ← 완전 중복
```

둘 다 `while next <= now { next += period }` 격자라 표류하지는 않지만 **위상은 임의**다.
그래서 Engine 이 Tick 보다 먼저 도착하는 비율이 실행마다 달라지고, 실측에서
`WALLCLOCK` 의 `user/s` 가 0.02 ↔ 6.65 사이를 오갔다(log_ani_debug_03/19, 04/01, 07_temp,
08_temp). 지금은 `8de1ff6d90b` 의 게이트가 Engine wake 를 박자에서 떼어 증상을 막고 있지만
**중복 자체는 그대로**다.

## Global Constraints

- Windows 전용 경로는 `#[cfg(target_os = "windows")]`/`#[cfg(windows)]` 로 가린다. 비-Windows 빌드가 깨지면 안 된다.
- 기존 `gfx_vsync_enabled`(`DwmVsyncRefreshDriver`)가 우선한다. 드라이버 선택 순서는 **vsync > pacing > 기본 타이머**.
- `gfx_wall_pacing_thread=false`(기본) 이면 동작이 **지금과 완전히 같아야** 한다 — 기본 `TimerRefreshDriver` 를 그대로 쓴다.
- 콜백은 `Box<dyn Fn() + Send + 'static>` 이다. 모아 두는 자료구조도 `Send` 여야 한다.
- 주석은 평어체, 한국어. 커밋 메시지도 평어체.
- rustfmt 기준선: `components/servo/examples/winit_wall/main.rs` 는 6건, 새 파일은 0건이어야 한다.
- 커밋 트레일러: `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`
- 다음 파일은 **절대 스테이징하지 않는다**: `Cargo.lock`, `etc/multigpu/config/wall_layout.example_1x1.json`, `tests/html/multigpu_standard_video_extended_probe.html`, `tests/html/multigpu_standard_video_rtsp_probe.html`. `git add -A` / `git add .` 금지.

## Review Focus

사양이 함축하지만 어느 태스크의 테스트도 건드리지 않는, 사람을 물 가능성이 큰 입력/실패 모드
다섯. 각 줄은 그 코드를 가진 태스크에 테스트로 들어간다.

1. **페이싱 드라이버가 셸의 표출 클럭 자리로 샌다** — 기존 코드는 `vsync_driver` 를 복제해 `AppState::present_vsync` 에 넣고, `drive_present_clock` 은 그것이 `Some` 이면 표출 클럭을 통째로 `pump_vsync` 경로로 바꾼다. `PacingRefreshDriver` 는 `vsync_due` 를 세우지 않으므로 그 경로에 들어가면 `vsync_stalled` 로 떨어진다. **이 플랜에서 가장 비싼 실수다.** → Task 2
2. **콜백 안에서 `observe_next_frame` 이 다시 불린다** — 이것이 **정상 흐름**이다(`notify_will_paint` 가 매 렌더마다 재등록). 발사 중에 큐를 잠근 채 콜백을 부르면 교착한다. 락을 놓고 불러야 한다. → Task 1
3. **`gfx_wall_pacing_thread=false` 인데 페이싱 드라이버를 끼운다** — 그러면 아무도 발사하지 않아 Servo 가 영원히 `waiting_for_frame=true` 로 멈춘다. 선택 로직이 그 조합을 만들 수 없어야 한다. → Task 2
4. **한 틱에 콜백이 여러 개 쌓여 있다** — trait 주석이 "Multiple callbacks may be registered for the same frame. It is up to the implementation to call *all* callbacks" 라고 명시한다. 하나만 부르고 나머지를 버리면 그 관찰자는 영영 굶는다. → Task 1
5. **콜백이 하나도 등록되지 않은 채 틱이 온다** — 애니메이션이 없는 정적 페이지의 매 프레임이 이 경우다. 발사가 아무 일도 하지 않고 패닉하지 않아야 한다. → Task 1
6. **발사 없이 틱만 여러 번 지나간다** — 페이싱이 돌지만 Servo 가 아직 관찰자를 안 붙인 기동 구간이다. 큐가 비어 있는 동안 뒤늦게 몰아 발사하면 안 된다. → Task 1

---

## 파일 구조

| 파일 | 책임 |
|---|---|
| `components/servo/examples/winit_wall/pacing_refresh_driver.rs` (신규, 약 110줄) | 콜백 큐 + 발사. 스레드 없음. 단위 테스트를 여기 둔다. |
| `components/servo/examples/winit_wall/main.rs` (수정) | 모듈 선언, 드라이버 선택(vsync > pacing > 기본), 틱 지점에서 발사 |

`DwmVsyncRefreshDriver` 와 나란히 두는 이유: 둘 다 "이 셸이 Servo 에 끼우는 `RefreshDriver`"
라는 같은 책임이고, 선택 로직이 한 곳에서 둘을 고른다.

---

### Task 1: `PacingRefreshDriver` — 콜백 큐와 발사

**Files:**
- Create: `components/servo/examples/winit_wall/pacing_refresh_driver.rs`
- Test: 같은 파일의 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `servo::RefreshDriver` (trait, `components/shared/embedder/lib.rs:287`)
- Produces:
  - `pub(crate) struct PacingRefreshDriver`
  - `pub(crate) fn PacingRefreshDriver::new() -> Self`
  - `pub(crate) fn PacingRefreshDriver::handle(&self) -> PacingFireHandle`
  - `pub(crate) struct PacingFireHandle` (`Clone`)
  - `pub(crate) fn PacingFireHandle::fire(&self)`
  - `impl servo::RefreshDriver for PacingRefreshDriver`

`PacingFireHandle` 을 따로 두는 이유: Servo 생성자가 `Rc<dyn RefreshDriver>` 를 **가져가
버리므로**, 셸이 발사하려면 그 전에 떠 둔 별도 핸들이 필요하다. `DwmVsyncRefreshDriver` 에서
`shell_vsync_driver = vsync_driver.clone()` 로 핸들을 떠 두는 것과 같은 사정이고, 거기서는
`Rc` 를 복제했지만 여기서는 **발사만 할 수 있는 좁은 핸들**을 준다.

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`components/servo/examples/winit_wall/pacing_refresh_driver.rs` 를 만들고 테스트만 먼저 넣는다.

```rust
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 페이싱 틱이 발사하는 [`RefreshDriver`].

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use servo::RefreshDriver;

    use super::*;

    /// ★콜백이 없는 틱은 아무 일도 하지 않는다.★ 애니메이션이 없는 정적 페이지의 매
    /// 프레임이 이 경우다 -- 여기서 패닉하면 화면이 멈춘다.
    #[test]
    fn fire_with_no_callbacks_is_a_noop() {
        let driver = PacingRefreshDriver::new();
        let handle = driver.handle();
        handle.fire();
        handle.fire();
    }

    /// ★한 틱이 그때까지 쌓인 것을 **전부** 부른다.★ trait 주석이 그렇게 요구한다 --
    /// 하나만 부르고 나머지를 버리면 그 관찰자는 영영 굶는다.
    #[test]
    fn fire_calls_every_pending_callback_once() {
        let driver = PacingRefreshDriver::new();
        let handle = driver.handle();
        let count = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let count = count.clone();
            driver.observe_next_frame(Box::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
            }));
        }

        handle.fire();
        assert_eq!(count.load(Ordering::SeqCst), 3, "셋 다 불려야 한다");

        // 두 번째 틱에는 남은 것이 없다 -- 발사는 큐를 비운다.
        handle.fire();
        assert_eq!(count.load(Ordering::SeqCst), 3, "다시 부르면 안 된다");
    }

    /// ★콜백 안에서 다시 등록하는 것이 정상 흐름이다.★ `notify_will_paint` 가 매 렌더마다
    /// `observe_next_frame` 을 다시 건다. 발사가 큐를 잠근 채 콜백을 부르면 여기서 교착한다.
    #[test]
    fn callback_may_register_again_without_deadlock() {
        let driver = Arc::new(PacingRefreshDriver::new());
        let handle = driver.handle();
        let count = Arc::new(AtomicUsize::new(0));

        let again = driver.clone();
        let hits = count.clone();
        driver.observe_next_frame(Box::new(move || {
            hits.fetch_add(1, Ordering::SeqCst);
            // 다음 프레임을 다시 관찰한다 -- 실제 Servo 가 하는 일.
            again.observe_next_frame(Box::new(|| {}));
        }));

        handle.fire();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        // 재등록된 것이 다음 틱에 불릴 수 있어야 한다(= 큐에 들어갔다).
        handle.fire();
    }

    /// ★빈 틱이 몇 번 지나가도 나중에 몰아 발사하지 않는다.★ 페이싱은 돌지만 Servo 가 아직
    /// 관찰자를 안 붙인 기동 구간이 이 경우다.
    #[test]
    fn idle_ticks_do_not_accumulate() {
        let driver = PacingRefreshDriver::new();
        let handle = driver.handle();
        for _ in 0..10 {
            handle.fire();
        }

        let count = Arc::new(AtomicUsize::new(0));
        let hits = count.clone();
        driver.observe_next_frame(Box::new(move || {
            hits.fetch_add(1, Ordering::SeqCst);
        }));
        handle.fire();
        assert_eq!(count.load(Ordering::SeqCst), 1, "앞선 빈 틱이 쌓여 있으면 안 된다");
    }
}
```

- [ ] **Step 2: 실패를 확인한다**

```
cargo test -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
```

빌드 환경은 `scripts/servo_env.ps1` 을 먼저 dot-source 한 세션에서 `servo/` 안에서 돌린다
(CLAUDE.md 의 "Build & run"). 기대: **컴파일 실패** — `PacingRefreshDriver` 가 없다
(`cannot find type 'PacingRefreshDriver' in this scope`).

- [ ] **Step 3: 최소 구현을 쓴다**

같은 파일의 `#[cfg(test)] mod tests` **위**에 넣는다.

```rust
use std::sync::{Arc, Mutex};

use servo::RefreshDriver;

type StartFrameCallback = Box<dyn Fn() + Send + 'static>;

/// 다음 발사에서 부를 콜백들.
///
/// ★`Mutex` 하나면 된다 -- 조건변수가 없다.★ `DwmVsyncRefreshDriver` 는 자기 스레드가
/// 할 일이 생길 때까지 자야 해서 `Condvar` 가 필요했지만, 이 드라이버는 **스레드를 띄우지
/// 않는다.** 발사는 이미 존재하는 페이싱 틱이 하므로 기다릴 것이 없다.
#[derive(Default)]
struct Pending {
    callbacks: Mutex<Vec<StartFrameCallback>>,
}

impl Pending {
    fn fire(&self) {
        // ★락을 놓고 부른다.★ 콜백 안에서 `observe_next_frame` 이 다시 불리는 것이 정상
        // 흐름이고(`notify_will_paint` 가 매 렌더마다 재등록한다), 잠근 채 부르면 거기서
        // 교착한다. 그래서 큐를 통째로 꺼내 놓고 락을 놓은 뒤에 돈다.
        let callbacks: Vec<StartFrameCallback> = {
            let Ok(mut guard) = self.callbacks.lock() else {
                // 락이 중독됐다면 이미 다른 스레드가 콜백 안에서 패닉한 것이다. 표출을
                // 멈추는 것보다 이 틱을 건너뛰는 편이 낫다.
                return;
            };
            std::mem::take(&mut *guard)
        };
        for callback in callbacks {
            callback();
        }
    }
}

/// 페이싱 틱이 발사하는 [`RefreshDriver`].
///
/// ★기본 `TimerRefreshDriver` 는 전용 스레드에서 `gfx_refresh_hz` 주기로 도는 **두 번째**
/// 시계다.★ 이 셸에는 이미 페이싱 스레드가 같은 주기로 돌고 있으므로, 둘은 같은 공칭
/// 주기를 임의의 위상으로 각자 센다. 실측에서 그 위상에 따라 `WALLCLOCK` 의 `user/s` 가
/// 0.02 와 6.65 사이를 오갔다(log_ani_debug_03/19, 04/08_temp).
///
/// 그래서 시계를 하나로 만든다. 이 드라이버는 **스레드를 띄우지 않고**, 콜백을 모아
/// 두었다가 페이싱 틱이 `PacingFireHandle::fire` 를 부를 때 전부 발사한다.
pub(crate) struct PacingRefreshDriver {
    pending: Arc<Pending>,
}

impl PacingRefreshDriver {
    pub(crate) fn new() -> Self {
        Self {
            pending: Arc::new(Pending::default()),
        }
    }

    /// 발사용 핸들. ★Servo 생성자가 드라이버를 가져가 버리므로 그 전에 떠 둬야 한다.★
    pub(crate) fn handle(&self) -> PacingFireHandle {
        PacingFireHandle {
            pending: self.pending.clone(),
        }
    }
}

impl RefreshDriver for PacingRefreshDriver {
    fn observe_next_frame(&self, start_frame_callback: StartFrameCallback) {
        if let Ok(mut guard) = self.pending.callbacks.lock() {
            guard.push(start_frame_callback);
        }
    }
}

/// 페이싱 틱이 드라이버를 깨우는 손잡이. 발사만 할 수 있다.
#[derive(Clone)]
pub(crate) struct PacingFireHandle {
    pending: Arc<Pending>,
}

impl PacingFireHandle {
    /// 이 틱까지 등록된 콜백을 전부 부른다. 없으면 아무 일도 하지 않는다.
    pub(crate) fn fire(&self) {
        self.pending.fire();
    }
}
```

- [ ] **Step 4: 통과를 확인한다**

```
cargo test -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
```

기대: `4 passed`.

`callback_may_register_again_without_deadlock` 테스트는 `Arc<PacingRefreshDriver>` 를 쓰므로
`PacingRefreshDriver` 가 `Send + Sync` 여야 한다. `Arc<Pending>` 이고 `Pending` 의 필드가
`Mutex<Vec<Box<dyn Fn() + Send>>>` 이므로 성립한다. 컴파일이 거기서 막히면 구현이 아니라
`StartFrameCallback` 의 경계를 의심한다.

- [ ] **Step 5: 포맷과 커밋**

```
rustfmt --edition 2024 --check components/servo/examples/winit_wall/pacing_refresh_driver.rs
```

기대: 출력 없음(0건). 나오면 그대로 반영한다.

```bash
git add components/servo/examples/winit_wall/pacing_refresh_driver.rs
git commit -F - <<'EOF'
feat(winit_wall): 페이싱 틱이 발사하는 RefreshDriver 를 만든다

아직 아무도 쓰지 않는다 -- 다음 커밋이 끼운다.

★기본 `TimerRefreshDriver` 는 전용 스레드에서 `gfx_refresh_hz` 주기로 도는 두 번째
시계다.★ 이 셸에는 이미 페이싱 스레드가 같은 주기로 돌고 있어서, 둘은 같은 공칭 주기를
임의의 위상으로 각자 센다. 실측에서 그 위상에 따라 `WALLCLOCK` 의 `user/s` 가 0.02 와
6.65 사이를 오갔다(log_ani_debug_03/19, 04/08_temp).

이 드라이버는 스레드를 띄우지 않는다. 콜백을 모아 두었다가 페이싱 틱이 발사한다.
`DwmVsyncRefreshDriver` 와 같은 모양이고, 차이는 "자기 스레드에서 DwmFlush 를 기다린다"
대신 "남이 부르면 발사한다" 하나다.

테스트 넷은 이 자리에서 실제로 틀릴 수 있는 것들을 잡는다 -- 빈 틱, 여러 콜백을 전부
부르기, ★콜백 안에서의 재등록(정상 흐름이고 락을 잡은 채 부르면 교착한다)★, 빈 틱이
쌓이지 않기.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 2: 드라이버를 끼우고 페이싱 틱이 발사하게 한다

**Files:**
- Modify: `components/servo/examples/winit_wall/main.rs`

**Interfaces:**
- Consumes: Task 1 의 `PacingRefreshDriver::new()`, `::handle()`, `PacingFireHandle::fire()`
- Produces: 없음(셸 내부 배선)

**배경 — 지금 코드의 세 지점**

1. `mod vsync_refresh_driver;` 가 `main.rs:69` 에 있다. 모듈 선언을 그 옆에 둔다.
2. 드라이버 선택은 `main.rs:1616` 부근 `let vsync_driver: Option<Rc<dyn servo::RefreshDriver>> = { ... }` 다.
3. 페이싱 틱은 `fire_present_tick`(`main.rs` 의 `fn fire_present_tick(&self)`)에서 `servo::note_present_tick()` 을 부른 직후 렌더로 간다. 발사를 **그 사이**에 넣는다 — 렌더 전에 `waiting_for_frame` 이 풀려 있어야 하고, 두 발화 경로(타이머/vsync)가 모두 이 함수를 지나므로 한 자리면 충분하다.

**★함정: `vsync_driver` 는 두 곳으로 간다.★**

```rust
let shell_vsync_driver = vsync_driver.clone();   // main.rs:1634
...
present_vsync: shell_vsync_driver,               // main.rs:1757  ← AppState
```

그리고 `drive_present_clock`(`main.rs:996`)은 `present_vsync` 가 `Some` 이면 **셸의 표출
클럭을 통째로 `pump_vsync` 경로로 바꾼다** — `vsync_due` 를 기다리고, 안 오면
`vsync_stalled` 로 떨어진다. `PacingRefreshDriver` 는 `vsync_due` 를 세우지 않으므로,
***같은 변수에 넣으면 표출 클럭이 깨진다.***

그래서 **변수를 둘로 나눈다**: Servo 에 넘길 것과 셸의 표출 클럭용. 후자는 **vsync 일
때만** `Some` 이다.

- [ ] **Step 1: 실패하는 테스트를 쓴다**

선택 로직만 순수 함수로 떼어 테스트한다. `main.rs` 에 `mod tests` 가 아직 없으므로 파일 끝에
새로 만든다. **이 단계에서는 테스트만 넣는다** — `FrameClock` 과 `choose_frame_clock` 은
Step 3 에서 넣으므로 지금은 컴파일되지 않아야 한다.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// ★vsync 가 켜져 있으면 그것이 이긴다.★ 이미 있는 동작이고 바뀌면 안 된다.
    #[test]
    fn vsync_wins_over_pacing() {
        assert_eq!(choose_frame_clock(true, true), FrameClock::Vsync);
        assert_eq!(choose_frame_clock(true, false), FrameClock::Vsync);
    }

    /// 페이싱이 돌고 vsync 가 꺼져 있을 때만 페이싱 드라이버를 쓴다.
    #[test]
    fn pacing_only_when_the_thread_runs() {
        assert_eq!(choose_frame_clock(false, true), FrameClock::Pacing);
    }

    /// ★페이싱이 없으면 기본 타이머로 떨어진다.★ 이 조합에서 `Pacing` 을 고르면 아무도
    /// 발사하지 않아 화면이 영원히 멈춘다.
    #[test]
    fn no_pacing_thread_falls_back_to_timer() {
        assert_eq!(choose_frame_clock(false, false), FrameClock::Timer);
    }

    /// ★셸의 표출 클럭은 vsync 일 때만 드라이버를 탄다.★ 페이싱 드라이버가 그 자리
    /// (`AppState::present_vsync`)로 새면 `vsync_due` 를 세우는 사람이 없어
    /// `drive_present_clock` 이 `vsync_stalled` 로 떨어진다 -- 표출 클럭이 깨진다.
    /// 기존 코드의 `let shell_vsync_driver = vsync_driver.clone();` 가 정확히 그 사고였다.
    #[test]
    fn only_vsync_drives_the_shell_present_clock() {
        assert!(FrameClock::Vsync.drives_shell_present_clock());
        assert!(!FrameClock::Pacing.drives_shell_present_clock());
        assert!(!FrameClock::Timer.drives_shell_present_clock());
    }
}
```

- [ ] **Step 2: 실패를 확인한다**

```
cargo test -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
```

기대: **컴파일 실패** — `cannot find type 'FrameClock' in this scope`,
`cannot find function 'choose_frame_clock' in this scope`, 그리고
`no method named 'drives_shell_present_clock'`.

- [ ] **Step 3: 선택 로직과 모듈을 넣고 드라이버를 끼운다**

먼저 `mod tests` **바로 위**에 선택 로직을 넣는다.

```rust
/// `gfx_vsync_enabled` / `gfx_wall_pacing_thread` 조합에서 어느 프레임 클럭을 쓸까.
///
/// ★순수 함수로 뗀 이유.★ 실제 선택은 `Rc<dyn RefreshDriver>` 를 만들어야 해서 테스트에서
/// 부르기 어렵다. 틀리면 화면이 멈추는 조합이 하나 있으므로(`Timer` 의 주석) 그 판정만
/// 따로 재현 가능하게 둔다.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum FrameClock {
    /// `gfx_vsync_enabled` -- DwmFlush 스레드가 발사한다. ★셸의 표출 클럭도 이것을 탄다★
    /// (`AppState::present_vsync`).
    Vsync,
    /// 페이싱 틱이 발사한다. 셸의 표출 클럭은 ***타이머 경로 그대로*** 다.
    Pacing,
    /// Servo 기본 `TimerRefreshDriver`(전용 스레드).
    Timer,
}

impl FrameClock {
    /// 셸의 표출 클럭(`AppState::present_vsync` -> `pump_vsync`)이 이 드라이버를 타는가.
    ///
    /// ★`Vsync` 만 참이다.★ 페이싱 드라이버를 그 자리에 넣으면 `vsync_due` 를 세우는 사람이
    /// 없어 `vsync_stalled` 로 떨어진다 -- 표출 클럭이 깨진다.
    fn drives_shell_present_clock(self) -> bool {
        matches!(self, FrameClock::Vsync)
    }
}

fn choose_frame_clock(vsync_enabled: bool, pacing_thread: bool) -> FrameClock {
    if vsync_enabled {
        FrameClock::Vsync
    } else if pacing_thread {
        FrameClock::Pacing
    } else {
        // ★페이싱이 없으면 `Pacing` 을 고르면 안 된다.★ 아무도 발사하지 않아 Servo 가
        // 영원히 `waiting_for_frame=true` 로 멈춘다.
        FrameClock::Timer
    }
}
```

`main.rs:69` 의 `mod vsync_refresh_driver;` 바로 위에 한 줄을 넣는다(알파벳 순).

```rust
mod pacing_refresh_driver;
mod vsync_refresh_driver;
```

`main.rs:1616` 부근의 드라이버 선택을 아래로 바꾼다. 기존 `#[cfg(target_os = "windows")]`
블록과 그 아래 `#[cfg(not(target_os = "windows"))] let vsync_driver ... = None;` 를 함께
교체한다. 변수 이름 `vsync_driver` 를 유지한다 — Servo 생성으로 넘어가는 코드가 그 이름을 쓴다.

**세 값을 함께 만든다.** `shell_vsync_driver` 를 여기서 직접 정하는 것이 이 태스크의 핵심
수정이다 — 기존의 `let shell_vsync_driver = vsync_driver.clone();`(`main.rs:1634`)을
**지운다.** 그 복제가 남아 있으면 `PacingRefreshDriver` 가 `present_vsync` 로 들어가
표출 클럭이 `pump_vsync` 경로로 빠진다.

```rust
        // ★시계를 하나로 고른다.★ `FrameClock` 의 주석과 `choose_frame_clock` 의 테스트가
        // 조합을 정한다.
        //
        // ★세 값이 나오는 이유.★ `vsync_driver` 는 Servo 가 가져가고(프레임 클럭),
        // `shell_vsync_driver` 는 셸의 표출 클럭이 되며(`AppState::present_vsync` ->
        // `drive_present_clock` 의 `pump_vsync` 경로), `pacing_fire` 는 페이싱 틱이
        // 드라이버를 깨우는 손잡이다. ***앞의 둘은 vsync 일 때만 같다*** -- 페이싱
        // 드라이버를 셸의 표출 클럭 자리에 넣으면 `vsync_due` 를 세우는 사람이 없어
        // `vsync_stalled` 로 떨어진다.
        #[cfg(target_os = "windows")]
        let clock = choose_frame_clock(
            servo::prefs::get().gfx_vsync_enabled,
            servo_config::pref!(gfx_wall_pacing_thread),
        );
        #[cfg(not(target_os = "windows"))]
        let clock = FrameClock::Timer;

        let (vsync_driver, pacing_fire): (
            Option<Rc<dyn servo::RefreshDriver>>,
            Option<pacing_refresh_driver::PacingFireHandle>,
        ) = match clock {
            #[cfg(target_os = "windows")]
            FrameClock::Vsync => {
                eprintln!(
                    "wall: gfx_vsync_enabled=true: DWM vsync(DwmFlush) 에 표출과 값 표본을 묶는다 (phase_pct={}).",
                    servo_config::pref!(gfx_vsync_phase_pct).clamp(0, 99)
                );
                (
                    Some(Rc::new(vsync_refresh_driver::DwmVsyncRefreshDriver::new())),
                    None,
                )
            },
            // 비-Windows 에는 `DwmVsyncRefreshDriver` 가 없다. `choose_frame_clock` 이
            // 거기서 `Timer` 만 돌려주므로 이 팔은 도달하지 않는다.
            #[cfg(not(target_os = "windows"))]
            FrameClock::Vsync => (None, None),
            FrameClock::Pacing => {
                eprintln!(
                    "wall: 표출 틱이 프레임 클럭이다 -- Servo 의 PaintTimerThread 를 쓰지 않는다."
                );
                let driver = pacing_refresh_driver::PacingRefreshDriver::new();
                let handle = driver.handle();
                (Some(Rc::new(driver)), Some(handle))
            },
            FrameClock::Timer => (None, None),
        };

        // ★셸의 표출 클럭은 vsync 일 때만 드라이버를 탄다.★ 기존 코드는 여기서 무조건
        // `vsync_driver.clone()` 했는데, 그러면 페이싱 드라이버가 `present_vsync` 로 들어가
        // `drive_present_clock` 이 `pump_vsync` 경로로 빠진다 -- 그 드라이버는 `vsync_due` 를
        // 세우지 않으므로 `vsync_stalled` 로 떨어진다. 판정을 `FrameClock` 에 두고 테스트를
        // 붙였다(`only_vsync_drives_the_shell_present_clock`).
        let shell_vsync_driver = if clock.drives_shell_present_clock() {
            vsync_driver.clone()
        } else {
            None
        };
```

`pacing_fire` 를 `AppState` 가 들어야 한다. 필드 선언부(`present_vsync` 바로 아래,
`main.rs:304` 부근)에 넣는다:

```rust
    /// 페이싱 틱이 Servo 의 프레임 관찰자를 깨우는 손잡이. `FrameClock::Pacing` 일 때만 Some.
    ///
    /// ★`present_vsync` 와 배타적이다.★ 전자는 셸의 표출 클럭을 vsync 에 묶고, 이것은
    /// 표출 틱을 Servo 의 프레임 클럭으로 쓴다 -- 방향이 반대다.
    pacing_fire: Option<pacing_refresh_driver::PacingFireHandle>,
```

그리고 `AppState` 를 만드는 자리(`present_vsync: shell_vsync_driver,` 가 있는 초기화 블록,
`main.rs:1757` 부근)에 `pacing_fire,` 를 추가한다.

- [ ] **Step 4: 페이싱 틱이 발사하게 한다**

`fire_present_tick` 의 `servo::note_present_tick();` 바로 **다음**에 넣는다.

```rust
    fn fire_present_tick(&self) {
        servo::note_present_tick();
        // ★Servo 의 프레임 관찰자를 여기서 깨운다.★ `FrameClock::Pacing` 이면 Servo 의
        // `PaintTimerThread` 가 돌지 않으므로, `notify_will_paint` 가 등록해 둔 콜백을
        // 부를 사람이 이 자리뿐이다. ***렌더 전에 불러야 한다*** -- 그 콜백이
        // `waiting_for_frame` 을 내리고, 그것이 내려가야 `needs_repaint()` 가 참이 된다.
        if let Some(fire) = self.pacing_fire.as_ref() {
            fire.fire();
        }
        if self.direct_render {
            self.note_redraw_requested();
            self.charge_main(MainSlot::Render, || self.render_all_tiles());
            return;
        }
        if let Some(tile) = self.tiles.first() {
            tile.window.request_redraw();
        }
    }
```

- [ ] **Step 5: 통과를 확인한다**

```
cargo test -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
```

기대: 전체 **`8 passed`**(Task 1 의 4개 + 이 태스크의 4개), 빌드 성공, **새 경고 0건**.

`drives_shell_present_clock` 이 `never used` 로 경고되면 배선이 빠진 것이다 — 그 함수는
`shell_vsync_driver` 를 정하는 자리에서 **실제로** 불려야 한다.

- [ ] **Step 6: 포맷과 커밋**

```
rustfmt --edition 2024 --check components/servo/examples/winit_wall/main.rs
```

기대: 6건(기준선). 7건 이상이면 늘어난 줄이 내 코드이므로 반영한다.

```bash
git add components/servo/examples/winit_wall/main.rs
git commit -F - <<'EOF'
feat(winit_wall): 표출 틱을 Servo 의 프레임 클럭으로 끼운다

★두 시계를 하나로 합친다.★ `BaseRefreshDriver::notify_will_paint` 는 매 렌더마다
`observe_next_frame` 을 다시 걸고, 기본 `TimerRefreshDriver` 는 그것을 전용 스레드에서
`gfx_refresh_hz` 주기로 받아 `event_loop_waker.wake()` 를 쏜다. 이 셸에는 이미 페이싱
스레드가 같은 주기로 `WakerEvent::Tick` 을 올리고 있으므로 ***완전히 중복***이고, 둘의
위상이 임의라 실행마다 `WALLCLOCK` 의 `user/s` 가 0.02 와 6.65 사이를 오갔다.

이제 `gfx_wall_pacing_thread` 가 켜져 있으면 `PacingRefreshDriver` 를 끼우고,
`fire_present_tick` 이 렌더 직전에 발사한다. Servo 의 `PaintTimerThread` 는 돌지 않는다.

선택 순서는 vsync > pacing > 기본 타이머다. ★페이싱이 없는데 `Pacing` 을 고르면 아무도
발사하지 않아 화면이 영원히 멈추므로★, 그 판정만 `choose_frame_clock` 으로 떼어 테스트를
붙였다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 3: 실기 검증과 기록

**Files:**
- Modify: `docs/superpowers/specs/2026-09-21-per-output-commit-phase-design.md`

**Interfaces:**
- Consumes: Task 2 의 배선
- Produces: 없음

이 태스크에는 자동 테스트가 없다. **벽은 이 저장소에서 자동으로 돌릴 수 없고**, 판정은 timed
스모크 런의 stderr 로그로 한다(CLAUDE.md 의 "There is no interactive GUI test harness").

- [ ] **Step 1: dist 를 만든다**

```
powershell -NoProfile -ExecutionPolicy Bypass -File etc\multigpu\make_wall_dist.ps1 -Force
```

`target\wall_dist\BUILD.txt` 의 `commit` 이 Task 2 의 커밋인지 확인한다.

- [ ] **Step 2: 실기에서 돌리고 지표를 읽는다**

평소 조합으로 120초. `-PacingThread` 가 들어 있어야 이 변경이 작동한다.

```
.\run_wall_dist.ps1 -Layout wall_layout.multigpu.json -Serve -Url "wall_anim_jitter_probe.html" `
  -IgnoreCertErrors -PageFeatures `
  -Pref dom_webgl2_enabled=true,dom_webgpu_enabled=true,gfx_precache_shaders=true,media_wrap_init_max_per_frame=2 `
  -DComp surface -RefreshHz 60 -DwmAlign 14 -CommitAlign 8 -SampleLead 1 -DirectRender `
  -DcompParallelCommit -PacingThread -ParallelTiles -PipelineMode uridecodebin3 -DecoderThreads 1 `
  -SinkPacing thread -NumaNode 1 -NoSyncGroup -DurationSec 120 -PresentCadence -DcompBindProf `
  -LogPath one_clock.err.log
```

받아야 할 지표:

| 지표 | 기대 | 뜻 |
|---|---|---|
| 기동 줄 | `wall: 표출 틱이 프레임 클럭이다` | 드라이버가 끼워졌다 |
| 기동 줄 | `gfx_vsync_enabled=true` 가 **없다** | 표출 클럭이 vsync 경로로 안 빠졌다 |
| `WALLCLOCK ... renders=` | **60** | 생산량이 그대로다 |
| `WALLCLOCK ... backstop=` | **0** | `vsync_stalled` 폴백이 안 돈다 |
| `WALLCLOCK ... user=` | **0** | 중복 깨우기가 사라졌다 |
| `WALLCLOCK ... pace=` | **60** | 전부 페이싱이 끈다 |
| `PAINTANIMANCHOR ... Epoch(N)` ÷ 초 | **약 15** | 틱 분주기가 그대로다 |
| 육안 | 불안정 구간 뒤 저더 없음 | 회귀가 없다 |

`backstop` 이 0 이 아니면 **표출 클럭이 `pump_vsync` 경로로 빠진 것**이다 — Review Focus 1 의
사고가 실제로 난 것이고, `shell_vsync_driver` 배선을 다시 본다.

★`user=0` 과 `pace=60` 이 이 변경의 직접 증거다.★ `8de1ff6d90b` 의 게이트가 Engine wake 를
막고 있으므로 그 전에도 `user` 는 0 에 가까웠지만, 이번에는 **깨우기 자체가 발생하지
않는다**. 둘을 가르려면 `PaintTimerThread` 가 뜨지 않는 것을 확인한다 — 프로세스 탐색기나
`Get-Process winit_wall | Select-Object -ExpandProperty Threads` 의 수가 하나 줄어야 한다.

`epoch/s` 가 15 가 아니라 60 이면 **회귀다** — `6d279788750`(되돌린 커밋)이 막아 둔
리플로 4배가 다시 열린 것이고, 저더가 재발한다. 그 경우 `choose_frame_clock` 이 아니라
`AnimationRefreshDriverObserver::frame_started` 의 분주기 판정을 의심한다.

- [ ] **Step 3: 결과를 설계 문서에 기록한다**

`docs/superpowers/specs/2026-09-21-per-output-commit-phase-design.md` 끝에 `## 49. 시계를
하나로` 를 추가한다. 담을 것:

- 왜 둘이었는지(`notify_will_paint` → `observe_next_frame` → `TimerRefreshDriver`)
- 무엇으로 합쳤는지(`PacingRefreshDriver`, 스레드 없음)
- Step 2 의 지표 표를 **실측값으로** 채운 것
- `8de1ff6d90b` 의 게이트가 이제 불필요해졌는지 — 불필요하면 그 사실만 적고 **제거는 하지
  않는다**(백스톱으로 남겨도 해가 없고, 제거는 별건이다)

- [ ] **Step 4: 커밋**

```bash
git add docs/superpowers/specs/2026-09-21-per-output-commit-phase-design.md
git commit -F - <<'EOF'
docs(wall): 시계를 하나로 합친 결과를 기록한다

§49. 실측 지표와, 남은 것(8de1ff6d90b 의 게이트가 이제 백스톱일 뿐이라는 것)을 적는다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## 비-목표

- **`8de1ff6d90b`(엔진 wake 게이트) 제거.** 이 변경이 성공하면 그 게이트는 할 일이 없어지지만,
  `FrameClock::Timer` 경로(페이싱 꺼짐)에서는 여전히 유효하다. 남긴다.
- **`gfx_refresh_hz` 와 페이싱 주기의 통합.** 지금은 `paint_timer_period()` 와 `present_period`
  가 둘 다 `gfx_refresh_hz` 에서 나오므로 값은 같다. 한쪽만 바꿀 수 있게 하는 것은 별건이다.
- **§48 의 질문**(디스플레이 리스트가 60/s 일 때 왜 15/s 보다 나쁜가). 이 플랜은 **클럭의
  개수**만 다루고 **틱의 빈도**는 건드리지 않는다.
- **servoshell.** 이 변경은 `winit_wall` 전용이다. servoshell 에는 페이싱 스레드가 없다.

# 표출 클럭 일원화 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** winit_wall 이 프레임을 그리는 권한이 표출 클럭 하나인지 계수로 확인하고, 새는 자리를 특정해 막는다.

**Architecture:** 셸(`winit_wall`)은 이미 `gfx_refresh_hz` 기반 단일 표출 클럭을 구현해 두었고 그 자체는 올바르다. 그런데 타일별 실제 그리기 패스가 클럭이 요청하는 60 을 넘어 61~75 다. ★1단계는 수정이 아니라 계수다★ — `WALLCLOCK` 한 줄이 네 갈래를 일의적으로 가르고, 2단계는 그 결과에 따라 조건부로만 실행한다.

**Tech Stack:** Rust 2024, winit 이벤트 루프, Servo 임베더 API. 셸 한 파일(`components/servo/examples/winit_wall/main.rs`)에 한정.

**Spec:** `docs/superpowers/specs/2026-09-17-single-present-clock-design.md`

## Global Constraints

- **범위**: `components/servo/examples/winit_wall/` 에 한정한다. `components/paint/` 는 servoshell 과 공유되고 같은 자리에서 낸 회귀 기록이 여럿 있다. 분기 2 에서 필요해지면 **멈추고 범위 확대를 묻는다** — 말없이 넘지 않는다.
- 저장소 `W:\servo_multigpu-tiled-wall`, 브랜치 `wall-animation-perf`. 브랜치를 만들거나 바꾸지 않는다.
- **★빌드 환경★**: 맨 셸에 cargo 가 없다. 모든 명령은 **PowerShell 한 번의 호출** 안에서 프렐류드를 먼저 실행한다:

  ```powershell
  $env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = 'F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64'
  . W:\scripts\servo_env.ps1
  Set-Location W:\servo_multigpu-tiled-wall
  ```

  환경은 호출 간에 유지되지 않으므로 매번 반복한다.
- **★cargo 를 `2>&1` 로 파이프하지 말 것★**: PowerShell 5.1 이 네이티브 exe 의 stderr 를 `NativeCommandError` 로 감싸 경고 한 줄에도 exit 1 이 된다. 출력을 파일로 받으려면 `cmd /c "cargo ... > out.txt 2>&1"` 처럼 cmd 에 맡긴다. 성공 여부는 `$LASTEXITCODE` 로 본다.
- **`cargo test -p script_tests` 를 실행하지 말 것**: 이 장비에는 Visual Studio 18.10 만 있고 `mozjs_sys` 가 2026-09-08 이후 빌드된 적이 없다. SpiderMonkey 재빌드를 유발하는 것은 전부 실패한다. servo 빌드는 캐시된 산출물로 돈다.
- **★rustfmt 와 모듈 루트★**: `main.rs` 는 `mod crash_report; mod tile; mod vsync_refresh_driver;` 를 선언하는 **모듈 루트**다. rustfmt 는 `mod` 선언을 따라가 형제 파일까지 포맷하고, `--skip-children` 은 rustfmt 1.9.0-stable 에 **존재하지 않는다**. 포맷 후 반드시 `git status --porcelain` 으로 확인하고 `crash_report.rs`/`tile.rs`/`vsync_refresh_driver.rs` 가 움직였으면 되돌린다.
- **★스테이징 규율★**: 아래 네 파일은 이 작업과 무관한 기존 더티 파일이다. 절대 스테이징하지 않는다 — `Cargo.lock`, `etc/multigpu/config/wall_layout.example_1x1.json`, `tests/html/multigpu_standard_video_extended_probe.html`, `tests/html/multigpu_standard_video_rtsp_probe.html`. **`git add -A` 나 `git add .` 를 쓰지 않는다.** 추적되지 않는 스크래치 파일도 57 개 있다. 전부 그대로 둔다.
- **커밋 트레일러**: 각자 세션의 지시가 정하는 `Co-Authored-By:` 를 쓴다. 브랜치의 커밋마다 모델 이름이 다른 것은 의도된 것이다.
- 커밋 전 `git diff --check`.
- **실기 측정은 운영자 몫**이다. 벽과 사람 눈이 필요하다. 구현자는 빌드까지 하고, **기기 결과를 지어내지 않는다.**
- A/B 는 `-NumaNode 1` 고정. 그룹 0 에 착지하면 측정이 통째로 무효다.

## File Structure

| 파일 | 책임 | 태스크 |
|---|---|---|
| `components/servo/examples/winit_wall/main.rs` | 표출 클럭, 렌더 진입, 초당 보고 — 이 계획의 **모든 코드 변경**이 여기 있다 | 1, 3 |
| `docs/superpowers/specs/2026-09-17-single-present-clock-design.md` | 진단 결과를 기록 | 2 |

새 파일은 없다. 셸의 기존 패턴(`MainBusy` + `report_main_busy`)을 그대로 따라 `ClockStats` + `report_clock_stats` 를 더한다 — 같은 파일에 같은 모양이 이미 있으므로 새 구조를 발명하지 않는다.

---

### Task 1: `WALLCLOCK` 계수

**Files:**
- Modify: `components/servo/examples/winit_wall/main.rs`

**Interfaces:**
- Consumes: 없음
- Produces:
  - `struct ClockStats` — 창 하나의 집계
  - `State::note_present_tick(&self)` / `note_redraw_requested(&self)` / `note_render_pass(&self)` / `note_redraw_suppressed(&self)`
  - `State::report_clock_stats(&self)` — 창이 1초를 넘으면 `WALLCLOCK` 한 줄을 내고 리셋
  - 로그 태그 `WALLCLOCK`

---

- [ ] **Step 1: 집계 구조체를 더한다**

`main.rs` 의 `struct MainBusy { ... }` 정의(대략 292행) **바로 뒤**에 넣는다. 같은 파일의 같은 패턴을 따르는 것이 요점이다.

```rust
/// 표출 클럭이 **유일한** 렌더 권한인지 판정하는 한 창(1초)의 집계.
///
/// ★셸은 이미 올바르다.★ `request_redraw()` 를 부르는 곳은 클럭 틱과 `--capture`
/// 둘뿐이고 `RedrawRequested` 는 `render_all_tiles()` 하나로 간다. 그런데 실측에서
/// 타일별 WRRATE 가 61~75 다(2026-09-17, log_ani_debug/25: 408 표본 중 304 개가
/// 60 초과, p75=72.0, 최고 75.1). 60Hz 디스플레이가 그것을 고르게 보여줄 수 없고,
/// 프레임당 변위가 달라지는 것이 등속 애니메이션을 떨리게 만든다.
///
/// 이 넷을 한 줄에 같이 내야 갈린다 -- 따로 보면 어느 단계에서 새는지 알 수 없다.
#[derive(Default)]
struct ClockStats {
    window_start: Option<std::time::Instant>,
    /// `drive_present_clock()` 이 틱을 발화한 횟수.
    ticks: u32,
    /// `WindowEvent::RedrawRequested` 진입 횟수. `ticks` 보다 크면 셸 밖에서 온다.
    redraw: u32,
    /// `render_all_tiles()` 진입 횟수. `redraw` 하나가 렌더 하나여야 한다.
    renders: u32,
    /// 클럭이 허가하지 않아 억제한 횟수. 2단계 전에는 항상 0 이다.
    suppressed: u32,
    /// `render_all_tiles()` 진입 간격(ms).
    ///
    /// ***평균이 아니라 분포를 낸다.*** 끊김은 정의상 꼬리에만 있고, 균일한 57fps 와
    /// "60,60,60,20,60" 은 평균이 같다. 이 표본이 성공 기준 1 그 자체다.
    gaps_ms: Vec<f64>,
    last_render_at: Option<std::time::Instant>,
}

/// 한 창에 담을 간격 표본의 상한.
///
/// 60Hz 에서 한 창은 60 개다. 스톨로 창이 길어져도 메모리가 늘지 않게 막는다 --
/// 실측된 최악의 정지가 3.2 초이므로(`MAINBUSY window_ms=3239`) 여유를 크게 둔다.
const MAX_CLOCK_GAP_SAMPLES: usize = 4096;
```

- [ ] **Step 2: `State` 에 필드를 단다**

`main_busy: RefCell<MainBusy>,`(대략 275행) 바로 뒤에 넣는다.

```rust
    /// 표출 클럭이 유일한 렌더 권한인지 재는 집계. [`ClockStats`] 참고.
    clock_stats: RefCell<ClockStats>,
```

그리고 `State` 를 만드는 자리에서 `main_busy: Default::default(),` 가 있는 줄 옆에 초기화를 더한다. `main_busy` 초기화가 명시적으로 없고 구조체 리터럴이 모든 필드를 나열한다면, 같은 자리에 다음을 더한다:

```rust
            clock_stats: Default::default(),
```

- [ ] **Step 3: 기록 함수 넷을 더한다**

`fn report_main_busy(&self)`(대략 525행) **바로 앞**에 넣는다.

```rust
    /// 클럭이 틱을 발화했다.
    fn note_present_tick(&self) {
        let mut stats = self.clock_stats.borrow_mut();
        stats.window_start.get_or_insert_with(std::time::Instant::now);
        stats.ticks += 1;
    }

    /// winit 이 `RedrawRequested` 를 전달했다.
    fn note_redraw_requested(&self) {
        let mut stats = self.clock_stats.borrow_mut();
        stats.window_start.get_or_insert_with(std::time::Instant::now);
        stats.redraw += 1;
    }

    /// 클럭이 허가하지 않아 그리지 않았다. 2단계 전에는 불리지 않는다.
    #[allow(dead_code, reason = "2단계 분기 1 에서만 쓴다")]
    fn note_redraw_suppressed(&self) {
        self.clock_stats.borrow_mut().suppressed += 1;
    }

    /// 한 패스를 그리기 시작했다. 직전 패스와의 간격을 함께 남긴다.
    fn note_render_pass(&self) {
        let now = std::time::Instant::now();
        let mut stats = self.clock_stats.borrow_mut();
        stats.window_start.get_or_insert(now);
        stats.renders += 1;
        if let Some(last) = stats.last_render_at
            && stats.gaps_ms.len() < MAX_CLOCK_GAP_SAMPLES
        {
            let gap = now.duration_since(last).as_secs_f64() * 1000.0;
            stats.gaps_ms.push(gap);
        }
        stats.last_render_at = Some(now);
    }
```

- [ ] **Step 4: 보고 함수를 더한다**

Step 3 의 함수들 바로 뒤, `report_main_busy` 앞에 넣는다.

```rust
    /// 창이 1 초를 넘으면 `WALLCLOCK` 한 줄을 내고 리셋한다.
    ///
    /// `last_render_at` 은 **리셋하지 않는다** -- 창 경계에서 간격 하나가 통째로
    /// 사라지면 그 자리가 정확히 측정에서 빠진다.
    fn report_clock_stats(&self) {
        let mut stats = self.clock_stats.borrow_mut();
        let Some(start) = stats.window_start else {
            return;
        };
        let window_ms = start.elapsed().as_secs_f64() * 1000.0;
        if window_ms < 1000.0 {
            return;
        }

        let period_ms = self.present_period.as_secs_f64() * 1000.0;
        let mut sorted = stats.gaps_ms.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pct = |p: f64| -> f64 {
            if sorted.is_empty() {
                return 0.0;
            }
            let index = ((sorted.len() as f64) * p) as usize;
            sorted[index.min(sorted.len() - 1)]
        };
        // ±20% 밖. p50/p95 만 보면 "대체로 괜찮은데 가끔 튐" 을 놓친다.
        let (low, high) = (period_ms * 0.8, period_ms * 1.2);
        let off = stats.gaps_ms.iter().filter(|g| **g < low || **g > high).count();
        let off_pct = if stats.gaps_ms.is_empty() {
            0.0
        } else {
            100.0 * off as f64 / stats.gaps_ms.len() as f64
        };

        log::info!(
            "WALLCLOCK window_ms={:.0} ticks={} redraw={} renders={} suppressed={} period_ms={:.1} gap_ms p50={:.1} p95={:.1} max={:.1} off={}({:.0}%) n={}",
            window_ms,
            stats.ticks,
            stats.redraw,
            stats.renders,
            stats.suppressed,
            period_ms,
            pct(0.50),
            pct(0.95),
            sorted.last().copied().unwrap_or(0.0),
            off,
            off_pct,
            stats.gaps_ms.len(),
        );

        let last_render_at = stats.last_render_at;
        *stats = ClockStats::default();
        stats.last_render_at = last_render_at;
    }
```

- [ ] **Step 5: 세 자리에 호출을 심는다**

**(a)** `drive_present_clock()` 안, 틱이 발화하는 `if now >= next {` 블록에서 `self.next_present_tick.set(next);` **바로 뒤**:

```rust
            self.note_present_tick();
```

**(b)** `WindowEvent::RedrawRequested` 처리(대략 1109행)를 아래로 바꾼다. 지금은 이렇다:

```rust
            WindowEvent::RedrawRequested => {
                if let Self::Running(state) = self {
                    state.charge_main(MainSlot::Render, || state.render_all_tiles());
                }
            },
```

이렇게 바꾼다:

```rust
            WindowEvent::RedrawRequested => {
                if let Self::Running(state) = self {
                    state.note_redraw_requested();
                    state.charge_main(MainSlot::Render, || state.render_all_tiles());
                }
            },
```

**(c)** `fn render_all_tiles(&self)`(대략 580행) 본문 **첫 줄**:

```rust
        self.note_render_pass();
```

- [ ] **Step 6: 보고를 매 창마다 돌린다**

`about_to_wait` 안의 `state.report_main_busy();` **바로 뒤**에 넣는다.

```rust
        state.report_clock_stats();
```

- [ ] **Step 7: 빌드**

```powershell
$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = 'F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64'
. W:\scripts\servo_env.ps1
Set-Location W:\servo_multigpu-tiled-wall
cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu
"CHECK_EXIT=$LASTEXITCODE"
```

Expected: `CHECK_EXIT=0`. 포그라운드로 돌리고 기다린다.

흔한 실패:
- `let ... && ...` 체인이 안 되면 이 크레이트의 edition 이 2024 가 아닌 것이다. `if let Some(last) = stats.last_render_at { if stats.gaps_ms.len() < MAX_CLOCK_GAP_SAMPLES { ... } }` 로 풀어 쓴다.
- `#[allow(dead_code, reason = ...)]` 가 거부되면 `reason` 을 떼고 `#[allow(dead_code)]` 만 쓴다.

- [ ] **Step 8: 포맷 — ★모듈 루트 주의★**

```powershell
$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = 'F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64'
. W:\scripts\servo_env.ps1
Set-Location W:\servo_multigpu-tiled-wall
rustfmt --edition 2024 components\servo\examples\winit_wall\main.rs
git status --porcelain
```

`main.rs` 는 `mod crash_report; mod tile; mod vsync_refresh_driver;` 를 선언하는 모듈 루트이므로 rustfmt 가 **형제 파일까지** 포맷한다. `git status` 에 `crash_report.rs`/`tile.rs`/`vsync_refresh_driver.rs` 가 나타나면 되돌린다:

```bash
git checkout -- components/servo/examples/winit_wall/crash_report.rs \
                components/servo/examples/winit_wall/tile.rs \
                components/servo/examples/winit_wall/vsync_refresh_driver.rs
```

그다음 `git diff --check`.

- [ ] **Step 9: 커밋**

```bash
git add components/servo/examples/winit_wall/main.rs
git commit   # 아래 메시지 + 자기 세션의 Co-Authored-By 트레일러
```

```
diag: 표출 클럭이 유일한 렌더 권한인지 재는 WALLCLOCK

셸은 이미 gfx_refresh_hz 기반 단일 클럭을 구현해 두었고 request_redraw 는
클럭 틱과 --capture 두 곳뿐인데, 타일별 WRRATE 가 61~75 다(log_ani_debug/25:
408 표본 중 304 개가 60 초과, p75=72.0, 최고 75.1). 클럭이 유일한 권한이
아니라는 뜻이고, 60Hz 디스플레이가 그것을 고르게 보여줄 수 없어 프레임당
변위가 달라지는 것이 등속 애니메이션을 떨리게 만든다.

ticks/redraw/renders 를 한 줄에 같이 내야 어느 단계에서 새는지 갈린다.
gap 분포는 진단용이 아니라 영구 계측이다 -- 성공 기준이 그 값이고, 평균을
내지 않는 이유는 평균이 끊김을 정의상 가리기 때문이다.

수정은 아직 하지 않는다. 이 증상을 쫓으며 네 번 잘못 짚었고 그중 한 번은
"찾았다" 고 단언한 것이 로그 위상 아티팩트였다.
```

---

### Task 2: 진단 실행과 분기 판정

**Files:**
- Modify: `docs/superpowers/specs/2026-09-17-single-present-clock-design.md` (판정 결과 기록)

**Interfaces:**
- Consumes: Task 1 의 `WALLCLOCK` 로그
- Produces: 네 갈래 중 하나의 확정. Task 3 의 실행 여부가 여기서 정해진다.

★이 태스크에는 코드가 없다.★ 실기 실행은 **운영자 몫**이고, 구현자는 기기 결과를 지어내지 않는다.

- [ ] **Step 1: 운영자에게 실행을 요청한다**

배포본을 다시 만든 뒤(엔진이 바뀌었으므로) 다음을 60 초 돌린다:

```powershell
.\run_wall_dist.ps1 -Serve -Url wall_anim_jitter_probe.html -NumaNode 1 -DurationSec 60
```

- [ ] **Step 2: 로그에서 한 줄을 읽는다**

```
WALLCLOCK window_ms=1001 ticks=60 redraw=70 renders=70 suppressed=0 period_ms=16.7 gap_ms p50=14.3 p95=20.1 max=33.2 off=18(30%) n=70
```

같은 런의 타일별 `WRRATE frames` 와 대조한다.

- [ ] **Step 3: 갈래를 확정하고 문서에 적는다**

읽을 때 알아 둘 구조적 사실: 클럭은 `tiles.first()` **한 창에만** `request_redraw()`
를 걸고, `RedrawRequested` 핸들러는 **어느 창에서 왔는지 보지 않고** 매번
`render_all_tiles()` 로 전 타일을 그린다. 그러므로 정상이라면 `ticks = redraw =
renders` 여야 한다. 창이 넷이므로, OS 가 나머지 세 창 중 하나에 expose·damage 를
보내면 그 한 번이 **전 타일 패스 하나를 통째로** 더 만든다 — 분기 1 이 유력한 구조적
근거가 이것이다.

| 관찰 | 갈래 | 다음 |
|---|---|---|
| `ticks≈60` 인데 `redraw>ticks` | **분기 1** — 셸 밖에서 들어온다 | Task 3 실행 |
| `ticks≈redraw≈renders≈60` 인데 타일별 WRRATE > 60 | **분기 2** — 셸 아래에서 샌다 | ★범위 밖★ — 멈추고 범위 확대를 묻는다 |
| `ticks>60` 또는 `period_ms≈8.3` | **분기 3** — 클럭/주기가 틀렸다 | `gfx_refresh_hz` 전달 경로 확인. 셸은 pref 를 직접 읽으므로 가능성 낮다 |
| 전부 60 이고 `off` 도 낮다 | **분기 4** — 과잉 생산이 원인이 아니었다 | 이 계획의 전제가 틀렸다. 멈추고 다시 설계한다 |

확정한 갈래와 근거 수치를 설계 문서 §2 아래에 한 문단으로 적고 커밋한다. **판정 근거를 남기지 않으면 다음 사람이 같은 측정을 다시 한다.**

---

### Task 3: 분기 1 수정 — 클럭이 허가한 틱만 그린다

★**조건부**★ — Task 2 가 분기 1 을 확정했을 때만 실행한다. 다른 갈래면 이 태스크를 건너뛴다.

**Files:**
- Modify: `components/servo/examples/winit_wall/main.rs`

**Interfaces:**
- Consumes: Task 1 의 `note_redraw_suppressed()`, `clock_stats`
- Produces: `State::present_due: Cell<bool>` — 클럭이 허가한 틱이 소비되지 않은 채 남아 있는지

---

- [ ] **Step 1: 허가 플래그를 더한다**

`clock_stats: RefCell<ClockStats>,` 바로 뒤:

```rust
    /// 클럭이 이번 틱의 그리기를 허가했고 아직 소비되지 않았다.
    ///
    /// ***`replace(false)` 로만 소비한다.*** 세우고 지우지 않는 실수를 구조로 막는다 --
    /// 읽고 따로 지우는 형태면 한쪽을 빠뜨릴 수 있다.
    present_due: Cell<bool>,
```

초기화 자리에 `present_due: Cell::new(false),` 를 더한다.

- [ ] **Step 2: 클럭이 틱을 발화할 때 허가한다**

`drive_present_clock()` 안, Task 1 에서 넣은 `self.note_present_tick();` 바로 뒤:

```rust
            self.present_due.set(true);
```

- [ ] **Step 3: 허가된 틱만 그린다**

`WindowEvent::RedrawRequested` 처리를 아래로 바꾼다:

```rust
            WindowEvent::RedrawRequested => {
                if let Self::Running(state) = self {
                    state.note_redraw_requested();
                    // ★클럭이 유일한 렌더 권한이다.★ winit/OS 가 보낸 expose·damage 는
                    // 여기서 걸러진다 -- 클럭이 매 주기 **무조건** 전 타일을 그리므로
                    // 억제해도 한 주기(16.7ms) 안에 복구된다. 클럭이 멈추는 경우는 셸이
                    // 이미 따로 다룬다(`drive_present_clock` 을 `about_to_wait` 과 이벤트
                    // 처리 양쪽에서 부르는 이유이고, 실측된 최악의 정지가 3.2 초다).
                    if state.present_due.replace(false) {
                        state.charge_main(MainSlot::Render, || state.render_all_tiles());
                    } else {
                        state.note_redraw_suppressed();
                    }
                }
            },
```

- [ ] **Step 4: `#[allow(dead_code)]` 를 뗀다**

`note_redraw_suppressed` 가 이제 호출되므로 Task 1 에서 붙인 어트리뷰트를 지운다. 남겨 두면 다음 사람이 "안 쓰는 함수" 로 읽는다.

- [ ] **Step 5: 빌드**

```powershell
$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = 'F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64'
. W:\scripts\servo_env.ps1
Set-Location W:\servo_multigpu-tiled-wall
cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu
"CHECK_EXIT=$LASTEXITCODE"
```

Expected: `CHECK_EXIT=0`.

- [ ] **Step 6: 포맷 + 커밋**

Task 1 Step 8 과 **같은 모듈 루트 주의**가 적용된다 — 포맷 후 `git status` 로 형제 파일이 움직였는지 확인하고 되돌린다.

```bash
git add components/servo/examples/winit_wall/main.rs
git commit   # 아래 메시지 + 자기 세션의 Co-Authored-By 트레일러
```

```
gfx: 표출 클럭이 허가한 틱만 그린다

WALLCLOCK 진단에서 ticks 보다 redraw 가 많았다(근거 수치는 커밋 본문에
실측값을 적을 것). winit/OS 가 보낸 expose·damage 가 클럭 밖에서 렌더를
유발하고 있었고, 60Hz 디스플레이가 초당 60 개 넘게 받으면 어떤 vsync 구간엔
두 개가 들어가 앞의 것이 버려진다. 프레임당 변위가 달라지고, 그것이 등속
애니메이션을 떨리게 만든다.

클럭이 틱마다 present_due 를 세우고 RedrawRequested 가 replace(false) 로만
소비한다. 억제해도 클럭이 매 주기 무조건 전 타일을 그리므로 한 주기 안에
복구된다.
```

---

### Task 4: 검증

★**조건부**★ — Task 3 을 실행했을 때만. 실기 측정은 **운영자 몫**이다.

**Files:** 없음(측정과 판정만)

**Interfaces:**
- Consumes: Task 1 의 `WALLCLOCK`, 기존 `WRRATE`·`SCRIPTBUSY`

---

- [ ] **Step 1: 배포본을 다시 만든다**

```powershell
$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = 'F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64'
. W:\scripts\servo_env.ps1
Set-Location W:\servo_multigpu-tiled-wall
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
.\etc\multigpu\make_wall_dist.ps1 -Force
```

- [ ] **Step 2: 운영자에게 두 실행을 요청한다**

```powershell
# 프로브 — 비디오 없음, 과잉 생산이 드러나는 조건
.\run_wall_dist.ps1 -Serve -Url wall_anim_jitter_probe.html -NumaNode 1 -DurationSec 60

# output — 회귀 방지선
.\run_wall_dist.ps1 -Url <output URL> -NumaNode 1 -DurationSec 180 -PageFeatures
```

- [ ] **Step 3: 네 기준으로 판정한다**

Task 2 의 진단 실행이 **기준선**이다. 같은 바이너리 안에서 전후를 비교한다.

| 기준 | 합격선 | 출처 |
|---|---|---|
| 1. 간격 균일성 (주 판정) | `p50 ≈ period_ms`, `p95 ≤ period_ms × 1.2`, `off` 비율이 기준선보다 감소 | `WALLCLOCK` |
| 2. 렌더 수 | 타일별 `WRRATE frames` = `gfx_refresh_hz` ±1 | `WRRATE` |
| 3. 육안 | 1/16x 행이 떨지 않음 | 프로브 페이지 |
| 4. 전환 불변 | `SCRIPTBUSY reflow_ms`·`WRRATE` 가 기준선과 같음 | output 페이지 |

★기준 1 이 주 판정인 이유★: 기준 2 가 충족돼도 기준 1 이 깨질 수 있다. 초당 60 개를 내면서 간격이 8/25/8/25ms 면 수치는 합격이고 화면은 떨린다.

- [ ] **Step 4: 기준 3 이 불합격이면 — ★실패가 아니다★**

기준 1·2 가 합격인데 육안으로 여전히 떨린다면, 과잉 생산은 고쳐졌고 **다음 층이 남은 것**이다.

`gfx_vsync_enabled` 가 기본 꺼짐이라 자유 구동 소프트 타이머가 정확히 60 개를 내도 60Hz 디스플레이와 미끄러진다. 어떤 vsync 엔 두 개가 들어가고 어떤 vsync 엔 하나도 안 들어간다. 그것은 이 계획의 범위가 아니고 설계 문서 §5 에 적혀 있다.

이 경우 **"수정을 되돌린다" 가 아니라 "vsync/present 페이싱으로 내려간다"** 가 맞다. 판정 결과를 설계 문서에 적고 멈춘다.

---

## 비목표

`components/paint/` 수정(범위 밖 — 분기 2 에서 필요해지면 별도 합의), vsync 활성화와 present 페이싱(설계 §5), 애니메이션 값 계산 경로 수정(이미 정상으로 확인 — 네 타일이 0.4px 안에서 일치하고 앵커도 0.13ms 이내다), `gfx_paint_side_animation_tick_divisor` 조정(rAF 15Hz 는 의도된 동작이다), 자동 테스트 추가(winit 이벤트 루프라 계측 자체가 검증 도구다).

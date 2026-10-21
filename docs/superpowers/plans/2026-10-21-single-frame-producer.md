# 프레임 생산자 일원화 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 합성을 요청하는 11 개 지점을 하나로 줄인다 — 페이싱 틱이 주기마다 벽 프레임 하나를
내고, 스크립트·비디오·애니메이션은 값만 적재한다.

**Architecture:** 생산자는 새로 만들지 않는다. `Paint::issue_wall_frame_request` 가 이미
"벽 번호 하나 · 전 타일 팬아웃 · 배리어" 이고, `fire_present_tick` 이 이미 프레임 시각 ·
커밋 기준점 · rAF 틱을 렌더 앞에서 내보낸다. 빠진 것은 그 둘을 잇는 한 줄이다. 나머지 작업은
기존 생산자를 값 적재로 격하하고, 그 과정에서 그들이 함께 지고 있던 의무(캔버스 ack 해제,
liveness 프로브)를 틱으로 옮기는 일이다.

**Tech Stack:** Rust / Windows / `components/paint` / `components/servo/examples/winit_wall` /
WebRender(`vendor_local/webrender`) / DirectComposition

**Spec:** `docs/superpowers/specs/2026-10-21-single-frame-producer-design.md` — 특히 3 절의
불변식 17 개와 4 절의 게이트 처분표. ★그 둘을 읽지 않고는 이 계획의 어느 태스크도 안전하게
수행할 수 없다.★

## Global Constraints

- ★규칙 1★ 프레임 생산자는 단일하다 — 벽 전체에 하나, 주기마다 번호 하나, 전 타일 팬아웃, 배리어.
- ★규칙 2★ 프레임의 구성 요소는 그 프레임의 생산 트리거가 될 수 없다.
- ★규칙 3★ 생산은 구성 요소를 가져다 렌더할 뿐이고, 구성 요소 생산자는 자기 값만 업데이트한다.
- 생산 회계(`pending_frames`, `anim_ready_queue`)의 writer 는 공유 에필로그 하나뿐이다. 격하된
  경로는 `set_pending_frame` 을 포함해 생산 회계에 일절 쓰지 않는다.
- `renderer_behind()` 는 매 틱 전 타일에 ***호출***되어야 한다 — 2 초 강제 해제가 거절 경로에서만
  돈다(`painter.rs:3219-3222`).
- 테스트: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`
- 빌드: `cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release`
- ★PowerShell 5.1 에서 `cargo`/`git` 뒤에 `2>&1` 이나 `2>$null` 을 붙이지 말 것★ — 성공이 실패로 보인다.
- 빌드 전 환경 프렐류드: `$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = [Environment]::GetEnvironmentVariable('GSTREAMER_1_0_ROOT_MSVC_X86_64','User'); . W:\scripts\servo_env.ps1; $env:PATH = (Join-Path $env:GSTREAMER_1_0_ROOT_MSVC_X86_64 'bin') + ';' + $env:PATH`
- ★스테이징 금지★: `Cargo.lock`, `etc/multigpu/config/wall_layout.example_1x1.json`,
  `tests/html/multigpu_standard_video_*_probe.html`,
  `docs/superpowers/plans/2026-10-15-tick-animation-frame-time.md`. `git add -A` / `git add .` 금지.
- 커밋 트레일러: `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`
- 코드 주석·커밋 메시지는 평어체.
- 삭제하는 게이트의 역사 주석은 지우지 않는다 — 설계 문서를 가리키는 한 줄로 바꾼다(Task 10).

## Review Focus

태스크 테스트가 건드리지 않지만 사람을 물 수 있는 다섯. 각 줄의 시험은 괄호 안 태스크가 진다.

1. ★캔버스를 그리는 페이지에서 비디오가 선다★ — `needs_new_frame()` 이 `pending_canvas_images`
   때문에 거짓이면 그 타일은 매 틱 거절되고, 비디오 바닥(불변식 4)이 사라진다. 기대: 캔버스
   대기 중에도 비디오가 페인트 박자로 올라간다. (Task 7)
2. ★4 타일 중 일부만 역압에 걸린 주기★ — 배리어가 생성한 타일만 기다리고 거절한 타일은
   그 논리 프레임의 keep-previous 로 기록돼야 한다. 기대: 타일이 서로 다른 논리 프레임을
   표출하지 않는다. (Task 8)
3. ★스크린샷 요청이 떠 있는 동안★ — 매 틱 생산하면 `has_pending_frames()` 가 영영 참이다.
   기대: `winit_wall --capture` 가 찍힌다. (Task 9)
4. ★셸이 틱을 멈춘 경우★ — 수요 구동 임베더에서 깨우기가 끊기면 생산도 끊긴다. 기대:
   화면이 멈추되 ack 와 스크린샷은 풀린다(얼어붙지 않는다). (Task 5)
5. ★파이프라인 부트스트랩★ — 첫 틱 이전의 디스플레이 리스트가 화면에 나와야 한다. 기대:
   기동 화면이 검게 남지 않는다. (Task 9)

---

## File Structure

| 파일 | 책임 | 변경 |
|---|---|---|
| `components/paint/wall_tick.rs` | ★신규★ — 틱 생산의 순수 로직: 타일 결과 타입, 생산 허용 판정, 계측 | 생성 |
| `components/paint/paint.rs` | 벽 단위 생산 진입점, 배리어, keep-previous 등록 | 수정 |
| `components/paint/painter.rs` | 타일 단위 진입점, 격하되는 생산 지점 9 개, ack 해제 | 수정 |
| `components/paint/lib.rs` | 셸이 부르는 전역 통지 + 생산 진입점 재수출 | 수정 |
| `components/paint/output_grid.rs` | `FRAMEREASON` 에 안 잡히던 경로의 계수 | 수정 |
| `components/servo/examples/winit_wall/main.rs` | `fire_present_tick` 이 생산을 부른다, 콜백 손잡이 | 수정 |
| `components/servo/webview.rs` | 생산 진입점 경유 | 수정 |

`wall_tick.rs` 를 새로 두는 이유: 판정과 결과 타입이 `painter.rs`(5,000 줄)에 또 들어가면
다음 사람이 그것을 찾지 못한다. 그리고 ★규칙 1~3 을 강제하는 타입이 한 파일에 모여 있어야★
규칙을 어기는 변경이 리뷰에서 눈에 띈다.

---

### Task 1: `with_painter_mut` 블로킹 계측

설계 6 절의 미확인 항목. ★측정 결과는 틱이 painter 에 닿는 *방식*만 바꿀 수 있고, 생산자의
수와 단일성은 바꾸지 않는다.★

**Files:**
- Modify: `components/paint/paint.rs` (`with_painter_mut`)
- Modify: `components/config/debug_env.rs` (플래그 등록)
- Modify: `docs/multigpu/configuration.md` (doc 복제 — `the_doc_repeats_the_knob_descriptions_verbatim` 이 강제한다)

**Interfaces:**
- Produces: `SERVO_WALL_PAINTER_LOCK_PROF` 플래그, `PAINTERLOCK` 로그 줄

- [ ] **Step 1: 플래그를 등록하는 시험을 쓴다**

`components/config/tests/config_surface.rs` 는 이미 전수 검사를 한다. 새 시험은 쓰지 않고
기존 `every_registered_flag_is_actually_read_somewhere` 와
`the_doc_repeats_the_knob_descriptions_verbatim` 이 게이트다.

- [ ] **Step 2: 시험이 실패하는 것을 본다**

Run: `cargo test -p servo-config --test config_surface`
Expected: `every_registered_flag_is_actually_read_somewhere` 가 새 플래그를 들어 FAIL
(먼저 `debug_env.rs` 에만 추가했을 때)

- [ ] **Step 3: `with_painter_mut` 안에서 잠금 대기를 잰다**

`paint.rs` 의 `with_painter_mut` 에서 painter 핸들을 얻기까지 걸린 시간을 painter 별로 모아
1 초에 한 줄 찍는다. 집계는 `servo_base::interval_summary::percentiles` 를 쓴다(이미 있다).

```
PAINTERLOCK painter=PainterId(1) n=60 wait_ms p50=0.01 p95=0.12 max=3.40 over_1ms=2
```

`over_1ms` 는 1ms 이상 기다린 횟수다 — ★최댓값 하나로는 "3ms 한 번" 과 "3ms 를 60 번" 이
구분되지 않고, 그 둘은 결론이 정반대다★(`DCOMP_SLOW_COMMIT_MS` 의 doc 이 같은 말을 한다).

- [ ] **Step 4: 시험이 통과하는 것을 본다**

Run: `cargo test -p servo-config --test config_surface`
Expected: `every_env_name_read_in_sources_is_registered` 를 제외하고 PASS
(그 하나는 이 작업 이전부터 실패 중 — 미등록 이름 다섯이 `vendor_local/webrender` ·
`script_thread.rs` · `media-thread` 에서 읽힌다)

- [ ] **Step 5: 빌드하고 배포본을 만든다**

Run: `cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release`
Expected: 성공

- [ ] **Step 6: 실기에서 재고 결론을 설계 6 절에 적는다**

4 타일 벽에서 `-DcompParallelCommit -ParallelTiles -PacingThread` 로 120 초.
판정: `p95 < 1ms` 면 블로킹 호출로 간다. `p95 >= 1ms` 면 Task 4 의 팬아웃을 비블로킹
포스트로 바꾼다 — ★어느 쪽이든 생산자는 하나다★.

- [ ] **Step 7: Commit**

```bash
git add components/paint/paint.rs components/config/debug_env.rs docs/multigpu/configuration.md docs/superpowers/specs/2026-10-21-single-frame-producer-design.md
git commit -m "feat(paint): with_painter_mut 의 잠금 대기를 잰다"
```

---

### Task 2: `FRAMEREASON` 가시화

★이후 모든 태스크의 판정이 이것에 의존한다.★ 지금 `FRAMEREASON` 에 안 잡히는 경로가 둘이라,
"생산 지점이 하나다" 를 세어 봐야 거짓으로 하나가 나온다.

**Files:**
- Modify: `components/paint/painter.rs` (`present_external_only` 부근)
- Modify: `components/paint/paint.rs` (벽 페이서 합류 경로)

**Interfaces:**
- Produces: `FRAMEREASON` 에 `external_surface` 와 `wall_pacer_join` 두 값

- [ ] **Step 1: 실패하는 시험을 쓴다**

`painter.rs` 의 테스트 모듈에 추가한다. `note_frame_reason` 은 `Location::caller()` 를 쓰므로
직접 시험하기 어렵다 — 대신 ★집계 키가 두 종류를 구분하는지★ 를 시험한다.

```rust
#[test]
fn a_non_webrender_present_gets_its_own_reason_key() {
    let mut counts = FrameReasonCounts::default();
    counts.note(FrameSource::WebRender { line: 1565 }, "SCENE");
    counts.note(FrameSource::ExternalSurface, "PRESENT");
    assert_eq!(counts.distinct_sources(), 2);
}
```

- [ ] **Step 2: 시험이 실패하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu a_non_webrender_present`
Expected: FAIL — `FrameSource` 가 없다

- [ ] **Step 3: `FrameSource` 를 넣고 두 경로에서 센다**

`enum FrameSource { WebRender { line: u32 }, ExternalSurface, WallPacerJoin }` 를
`painter.rs` 에 두고 `frame_reason_counts` 의 키를 그것으로 바꾼다. `present_external_only`
와 벽 페이서 합류 지점에서 각각 `note_frame_reason` 에 해당하는 값으로 센다.
★기존 줄 포맷은 바꾸지 않는다★ — 런처 파서(`run_wall_dist.ps1`)가 `painter.rs:<line>/<reason>`
을 읽는다. 새 값은 `external_surface/PRESENT` 꼴로 같은 자리에 들어간다.

- [ ] **Step 4: 시험이 통과하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`
Expected: PASS (전체 통과)

- [ ] **Step 5: 런처 요약이 새 값을 읽는지 본다**

`etc/multigpu/run_wall_dist.ps1` 의 `FRAMEREASON` 블록에 `external_surface` 와
`wall_pacer_join` 행을 더하고, 설계 8 절의 판정(★구별 값이 정확히 둘★)을 넣는다.

Run: `powershell -Command "$e=$null; [System.Management.Automation.Language.Parser]::ParseFile('W:\servo_multigpu-tiled-wall\etc\multigpu\run_wall_dist.ps1',[ref]$null,[ref]$e); $e.Count"`
Expected: `0`

- [ ] **Step 6: Commit**

```bash
git add components/paint/painter.rs components/paint/paint.rs etc/multigpu/run_wall_dist.ps1
git commit -m "feat(paint): 계측에 안 잡히던 두 표출 경로에 이름을 준다"
```

---

### Task 3: 콜백 손잡이 일원화

불변식 9·12. 세 구성(Pacing / Timer / Vsync)에서 refresh-driver 콜백을 누가 쏘는지가 달라
"콜백 → 생산 → 렌더" 가 한 틱 안의 순서가 되지 않는다. ★그 상태로 Task 5 를 하면 두 클럭이
맥놀이한다.★

**Files:**
- Modify: `components/servo/examples/winit_wall/main.rs` (`fire_present_tick`, `choose_frame_clock`)
- Modify: `components/paint/refresh_driver.rs`

**Interfaces:**
- Produces: `fire_present_tick` 이 세 구성 모두에서 refresh-driver 콜백을 발사한다

- [ ] **Step 1: 실패하는 시험을 쓴다**

`refresh_driver.rs` 의 테스트 모듈에:

```rust
#[test]
fn every_clock_has_a_handle_the_tick_can_fire() {
    for clock in [FrameClock::Pacing, FrameClock::Timer, FrameClock::Vsync] {
        assert!(clock.fire_handle().is_some(), "{clock:?} 에 손잡이가 없다");
    }
}
```

- [ ] **Step 2: 시험이 실패하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu every_clock_has_a_handle`
Expected: FAIL

- [ ] **Step 3: `Timer` 와 `Vsync` 에도 손잡이를 단다**

`PacingRefreshDriver` 가 이미 모델이다(스레드를 띄우지 않고 남이 부르면 발사). 같은 모양의
손잡이를 나머지 둘에 두고, 자기 스레드는 ★손잡이를 부르는 쪽★ 으로 바꾼다 — 콜백을 쏘는
주체가 언제나 틱이 되도록.

- [ ] **Step 4: 시험이 통과하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`
Expected: PASS

- [ ] **Step 5: 세 구성에서 회귀가 없는지 실기 확인**

각 구성으로 60 초. 보는 것: `RAFCLOCK p50=16.67` 유지, `ANIMSTEP dup/skip = 0`,
`epoch/s` 가 `gfx_refresh_hz`/divisor.

- [ ] **Step 6: Commit**

```bash
git add components/servo/examples/winit_wall/main.rs components/paint/refresh_driver.rs
git commit -m "feat(paint): 세 클럭 구성 모두에서 틱이 콜백을 소유한다"
```

---

### Task 4: 틱 진입점과 생산 배선 (아직 아무도 부르지 않는다)

**Files:**
- Create: `components/paint/wall_tick.rs`
- Modify: `components/paint/painter.rs` (틱 전용 진입점)
- Modify: `components/paint/paint.rs` (전 타일 `WallFrameRequest` 생성자, 벽 수준 진입점)
- Modify: `components/paint/lib.rs` (재수출)

**Interfaces:**
- Produces:
  - `pub(crate) enum TileOutcome { Generated, KeepPrevious }` — `wall_tick.rs`
  - `pub(crate) fn tile_may_generate(renderer_behind: bool, waiting_on_canvas: bool) -> TileOutcome` — `wall_tick.rs`
  - `Painter::generate_frame_for_wall_tick(&mut self, logical_frame_id: u64, requested_at: Instant) -> TileOutcome` — `painter.rs`
  - `Paint::produce_wall_frame(&self) -> Option<u64>` — `paint.rs`, 전 타일 대상 `WallFrameRequest` 를 만들어 `issue_wall_frame_request` 를 부른다
  - `Paint::gather_components(&self)` — `paint.rs`, 적재된 값(색·float·transform·held)을 모아
    각 painter 에 푸시한다. ★`produce_wall_frame` 보다 먼저, 독립된 호출로★
  - `FrameDelayer::waiting_on_canvas_images(&self) -> bool` — `painter.rs`, `pending_canvas_images`
    만 본다(`needs_new_frame` 과 달리 `pending_frame` 은 보지 않는다)

- [ ] **Step 1: 실패하는 시험을 쓴다**

`wall_tick.rs` 의 테스트 모듈에:

```rust
#[test]
fn a_tile_that_is_behind_keeps_its_previous_frame() {
    assert_eq!(tile_may_generate(true, false), TileOutcome::KeepPrevious);
}

/// ★캔버스를 기다려도 liveness 는 물어본 뒤다.★ 순서가 뒤집히면 2 초 강제 해제가
/// 영영 안 돈다 -- 그것이 "4 개 중 2 개 타일 44 초 사망" 조건이다.
#[test]
fn a_tile_waiting_on_canvas_keeps_its_previous_frame() {
    assert_eq!(tile_may_generate(false, true), TileOutcome::KeepPrevious);
}

#[test]
fn a_free_tile_generates() {
    assert_eq!(tile_may_generate(false, false), TileOutcome::Generated);
}
```

- [ ] **Step 2: 시험이 실패하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu wall_tick`
Expected: FAIL — 모듈이 없다

- [ ] **Step 3: `wall_tick.rs` 와 두 진입점을 만든다**

`Painter::generate_frame_for_wall_tick` 의 몸통 순서는 ★바꿀 수 없다★:

```
1. let behind = self.renderer_behind();        // ★무조건 먼저 호출.★ liveness 프로브다
2. let waiting = !self.frame_delayer.needs_new_frame_ignoring_pending_flag();
3. match tile_may_generate(behind, waiting) { KeepPrevious => return, Generated => ... }
4. generate_frame_with_diagnostic_id(.., Some(logical_frame_id), Some(requested_at))
5. set_display_composite_in_flight(true)
```

2 번이 `needs_new_frame()` 이 아닌 이유: 그것은 `pending_frame` 도 같이 보는데, 틱 생산은
★스크립트의 빚과 무관하게★ 돈다(규칙 2). 캔버스 이미지 대기만 보는
`FrameDelayer::waiting_on_canvas_images` 를 새로 낸다.

★`gather_components` 는 `produce_wall_frame` 안에 묻지 않는다.★ 불변식 8 은 합집합을 합성
***전에*** 모으라 하고, 불변식 9 는 순서가 틀렸을 때의 실측(요청 120 / 폐기 60)을 걸어 둔다.
묻어 두면 나중에 재배치가 가능해지므로 독립된 `pub(crate)` 메서드로 둔다.

`Paint::produce_wall_frame` 은 전 타일을 대상으로 `WallFrameRequest` 를 만들고
`issue_wall_frame_request` 를 부른다. ★`WebView` 별로 부르지 않는다★ — 벽 수준 연산이다.

- [ ] **Step 4: 시험이 통과하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add components/paint/wall_tick.rs components/paint/painter.rs components/paint/paint.rs components/paint/lib.rs
git commit -m "feat(paint): 틱 전용 생산 진입점을 만든다 (아직 부르지 않는다)"
```

---

### Task 5: 틱이 부르기 시작 — ★플래그 뒤의 중간 상태★

**Files:**
- Modify: `components/servo/examples/winit_wall/main.rs` (`fire_present_tick`)
- Modify: `components/servo/webview.rs` (경유)
- Modify: `components/config/prefs.rs` (`gfx_single_frame_producer`, 기본 `false`)

**Interfaces:**
- Consumes: `Paint::produce_wall_frame`
- Produces: `gfx_single_frame_producer` pref

★이 상태는 플래그 뒤에서만 존재하고 Task 9 가 끝나기 전에는 기본값을 켜지 않는다. 이 단계의
`FRAMEID` 하락은 기대값이며 판정에 쓰지 않는다 — 생산자가 일부러 둘이다.★

- [ ] **Step 1: 실패하는 시험을 쓴다**

`prefs.rs` 의 테스트 모듈에, 되돌림 방지:

```rust
#[test]
fn the_single_frame_producer_starts_off() {
    assert!(!Preferences::default().gfx_single_frame_producer);
}
```

- [ ] **Step 2: 시험이 실패하는 것을 본다**

Run: `cargo test -p servo-config --lib the_single_frame_producer`
Expected: FAIL — 필드가 없다

- [ ] **Step 3: pref 와 호출을 넣는다**

`fire_present_tick` 에서 `note_present_tick()` 과 렌더 사이에, pref 가 켜져 있을 때만
`produce_wall_frame()` 을 부른다. ★"한 `note_present_tick` 당 생산 1 회" 디버그 어서션★ 을
같이 넣는다(`debug_assert!`).

- [ ] **Step 4: 시험이 통과하는 것을 본다**

Run: `cargo test -p servo-config --lib`
Expected: PASS

- [ ] **Step 5: 배선이 됐는지 실기로 확인한다**

`--pref gfx_single_frame_producer=true` 로 60 초. 보는 것: ★합성이 대략 두 배★ 로 난다.
그것이 배선이 됐다는 증거다. `FRAMEREASON` 에 `tick` 값이 새로 보인다.

- [ ] **Step 6: 수요 구동 임베더에서 깨우기가 에지를 예약하게 한다**

설계 2-4. servoshell / WPT / Android 에는 자유 실행 표출 클럭이 없다. ★깨우기는 프레임을
내지 않고 다음 틱 에지에 생산 1 회를 예약한다★ — 그래야 생산자가 어디서나 하나이고, 조용한
페이지는 깨우기가 없어 생산도 없으므로 불변식 16 의 조용한 창이 생긴다.

예약은 `AtomicBool` 하나면 된다(`frame_time.rs` 의 전역 통지가 같은 모양이다). 틱이 그것을
`swap(false)` 로 집어 가고, 참이었으면 생산한다. 자유 실행 클럭이 있는 구성에서는 그 비트를
보지 않고 매 주기 생산한다 — 구성 판정은 Task 3 이 만든 손잡이의 유무로 한다.

- [ ] **Step 7: Review Focus 4 — 셸이 틱을 멈춘 경우**

`-DurationSec 60` 중간에 페이싱 스레드를 죽일 수 없으므로, pref 를 켠 채
`gfx_wall_pacing_thread=false` + 창 이벤트 없는 상태로 30 초 방치한다.
기대: 화면은 멈추되 프로세스가 살아 있고, `WALLACKLATCH` 가 안 뜬다.

- [ ] **Step 8: Commit**

```bash
git add components/servo/examples/winit_wall/main.rs components/servo/webview.rs components/config/prefs.rs components/paint/lib.rs docs/multigpu/configuration.md
git commit -m "feat(paint): 페이싱 틱이 벽 프레임을 낸다 (gfx_single_frame_producer, 기본 off)"
```

---

### Task 6: 캔버스 ack 해제를 틱으로 이설 — ★가장 위험★

설계 2-7. 해제 지점이 넷이고, 새 구조에서는 ★모든 스크립트 요청이 거절★ 이 되므로 해제가
생산에 붙어 있으면 영영 안 풀린다(실측: divisor=1 실행이 117 초에서 멎음).

**Files:**
- Modify: `components/paint/painter.rs` (`:2131` `flush_owed_canvas_ack`, `:3835`, `:3809`, `:3962`)
- Modify: `components/paint/wall_tick.rs` (판정)

**Interfaces:**
- Produces: `pub(crate) fn ack_is_releasable(holds_debt: bool, pending_canvas_images: usize) -> bool` — `wall_tick.rs`

- [ ] **Step 1: 실패하는 시험을 쓴다**

```rust
/// ★해제는 생산과 무관하다.★ 이 타일이 이번 틱에 합성했든 안 했든 같다 --
/// 그러지 않으면 "모든 요청이 거절" 인 새 구조에서 빚이 영영 안 풀린다.
#[test]
fn a_debt_with_no_outstanding_images_is_releasable() {
    assert!(ack_is_releasable(true, 0));
}

/// 캔버스가 아직 안 올라갔으면 풀지 않는다 -- FrameDelayer 계약이다.
#[test]
fn a_debt_with_outstanding_images_waits() {
    assert!(!ack_is_releasable(true, 1));
}

#[test]
fn no_debt_releases_nothing() {
    assert!(!ack_is_releasable(false, 0));
}
```

- [ ] **Step 2: 시험이 실패하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu ack_is_releasable`
Expected: FAIL

- [ ] **Step 3: 해제를 틱마다 돌린다**

`produce_wall_frame` 이 타일마다 — 생산 여부와 무관하게 — `ack_is_releasable` 이 참이면
`take_waiting_pipelines()` + `NoLongerWaitingOnAsynchronousImageUpdates` 를 보낸다.
`flush_owed_canvas_ack` 에서 ★합성만 뗀다★(ack 해제는 남긴다). `:3835` / `:3809` / `:3962`
의 해제는 삭제한다 — 틱이 대신한다.

- [ ] **Step 4: 시험이 통과하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`
Expected: PASS

- [ ] **Step 5: ★117 초 스톨이 돌아오지 않는지 확인한다★**

`--pref gfx_single_frame_producer=true,gfx_paint_side_animation_tick_divisor=1` 로
★120 초★. 캔버스를 쓰는 페이지로. 보는 것: `WALLACKLATCH` 0, 끝까지 프레임 생산 지속.
주입으로도 확인한다: `-CanvasAckSkip 1` 로 `WALLACKFLUSH` 가 뜨는지.

- [ ] **Step 6: Commit**

```bash
git add components/paint/painter.rs components/paint/wall_tick.rs
git commit -m "feat(paint): 캔버스 ack 해제를 생산에서 떼어 틱마다 돌린다"
```

---

### Task 7: 아홉 생산 경로를 값 적재로 격하

설계 2-6 의 표. ★`generate_frame` 만 빼고 그 자리의 트랜잭션은 그대로 보낸다.★

**Files:**
- Modify: `components/paint/painter.rs` (`:1565`, `:2854`, `:3060`, `:3119`, `:3141`, `:3635`, `:3826`, `:3957`, `:4086`)

**Interfaces:**
- Consumes: Task 4 의 틱 생산

- [ ] **Step 1: 실패하는 시험을 쓴다**

격하된 경로가 생산 회계에 쓰지 않는 것은 ★시험이 아니라 컴파일러에 맡긴다★ —
소스 문자열을 세는 시험은 주석 한 줄에도 깨지고, 깨진 뒤에는 아무도 안 고친다.

`FrameDelayer::set_pending_frame` 을 ***지운다***. 호출이 하나라도 남아 있으면 컴파일이
안 되고, 그것이 불변식 6 을 지키는 가장 단단한 방법이다. `pending_frame` 필드도 함께 지운다
— `needs_new_frame()` 은 Task 4 의 `waiting_on_canvas_images` 로 대체됐다.

대신 거동 시험을 쓴다:

```rust
/// 스크립트가 디스플레이 리스트를 올려도 그것만으로는 합성이 나지 않는다(규칙 2).
#[test]
fn a_new_display_list_does_not_request_a_composite() {
    let before = frame_request_count();
    handle_new_display_list_for_test();
    assert_eq!(frame_request_count(), before);
}
```

- [ ] **Step 2: 시험이 실패하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu a_new_display_list_does_not`
Expected: FAIL — 지금은 요청이 하나 는다

- [ ] **Step 3: 아홉 자리에서 `generate_frame` 과 `set_pending_frame` 을 뺀다**

설계 2-6 의 표를 한 줄씩 따라간다. `:3826` 의 `generate_frame_for_script` 는 함수째 삭제하고
`PaintMessage::GenerateFrame` 은 ★아무 일도 하지 않게★ 한다(스크립트는 디스플레이 리스트를
이미 별도 메시지로 올린다). `:1565` 의 깨우기 동반자(`:1571-1577`)는 ★다음 틱 에지 예약★
으로만 남긴다.

- [ ] **Step 4: 시험이 통과하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`
Expected: PASS

- [ ] **Step 5: `present_external_only` 를 틱 소유로 옮긴다**

설계 7 절. 자체 14ms 스로틀(`painter.rs:4026-4056`)을 버리고 ★틱마다 최대 1 회★ 로 바꾼다.
틱은 WR 의 바쁨과 무관하게 매 주기 도므로 원래 요구("fast-path 는 WR 빌드와 무관하므로 WR 이
바쁠 때에도 발동해야 한다")가 그대로 충족된다. Task 2 에서 준 `external_surface` 계수가
★틱당 1 회 이하★ 인지로 확인한다.

- [ ] **Step 6: Review Focus 1 — 캔버스 + 비디오 페이지**

캔버스와 비디오가 같이 있는 페이지로 60 초. 기대: 비디오가 페인트 박자로 올라간다
(`IMGUPDINNER vidgap_max_ms` 가 한 주기 수준).

- [ ] **Step 7: Commit**

```bash
git add components/paint/painter.rs
git commit -m "feat(paint): 생산 경로를 값 적재로 격하하고 외부 서피스 표출을 틱에 넘긴다"
```

---

### Task 8: 역압을 벽 단위로, 롤백 셋을 동시에 삭제

설계 2-5. 거절한 타일을 조용히 빼면 `paint.rs:2002` 때문에 배리어가 통째로 날아간다.

**Files:**
- Modify: `components/paint/paint.rs` (`issue_wall_frame_request`, `WallFrameCoordinator`)
- Modify: `components/paint/painter.rs` (`:1564-1566`, `:2853-2855`, `:4085-4087` 롤백 삭제)

**Interfaces:**
- Produces: `WallFrameCoordinator::arm_keep_previous(&mut self, logical_frame_id: u64, painter_ids: &[PainterId])`

- [ ] **Step 1: 실패하는 시험을 쓴다**

`paint.rs` 의 `wall_frame_keep_previous_tests` 모듈에 추가한다(이미 있다):

```rust
/// 역압으로 거절한 타일은 그 논리 프레임에 keep-previous 로 참여한다 --
/// 배리어에서 조용히 빠지면 화면에 프레임 N 과 N-1 이 섞인다.
#[test]
fn a_back_pressured_tile_joins_the_frame_as_keep_previous() {
    let mut c = WallFrameCoordinator::default();
    c.arm_keep_previous(7, &[PainterId::from(2)]);
    assert_eq!(c.keep_previous_logical_frame(PainterId::from(2)), Some(7));
}

/// 한 번도 표출한 적 없는 painter 에게는 유지를 지시하지 않는다(불변식 14).
#[test]
fn a_painter_that_never_presented_is_not_armed() {
    let mut c = WallFrameCoordinator::default();
    c.arm_keep_previous(7, &[PainterId::from(3)]);
    assert_eq!(c.keep_previous_logical_frame(PainterId::from(3)), None);
}
```

- [ ] **Step 2: 시험이 실패하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu a_back_pressured_tile`
Expected: FAIL

- [ ] **Step 3: `arm_keep_previous` 를 넣고 `issue_wall_frame_request` 에서 부른다**

`TileOutcome::KeepPrevious` 를 돌려준 타일을 모아 `arm_keep_previous(logical_frame_id, ..)`.
`register_frame` 에는 ★생성한 타일만★ 넘긴다(배리어가 기다릴 대상이므로).
`presented_painters` 면제(`17a26e5ed26`)는 그대로 적용된다.

- [ ] **Step 4: 롤백 셋을 ★한꺼번에★ 삭제한다**

`last_frame_by_other_source_at` 과 세 자리의 롤백. ★하나라도 남기면 측정된 진동이
재현된다★(1 초 페이드에 렌더 61 → 72).

- [ ] **Step 5: 시험이 통과하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`
Expected: PASS

- [ ] **Step 6: Review Focus 2 — 일부 타일만 역압**

4 타일 벽, 한 타일에만 무거운 콘텐츠. 보는 것: `FRAMEID distinct_ids/renders` 가 1.0 에
근접하고, 배리어 등록이 틱당 1 회.

- [ ] **Step 7: Commit**

```bash
git add components/paint/paint.rs components/paint/painter.rs
git commit -m "feat(paint): 역압을 벽 단위 결정으로 올리고 자기 제외 롤백을 걷어낸다"
```

---

### Task 9: 스크린샷 규칙과 부트스트랩 예외

**Files:**
- Modify: `components/paint/paint.rs` (`produce_wall_frame`)
- Modify: `components/paint/painter.rs` (`:3592` 부트스트랩)
- Modify: `components/paint/wall_tick.rs`

**Interfaces:**
- Produces: `pub(crate) fn tick_should_produce(screenshot_pending: bool) -> bool` — `wall_tick.rs`

- [ ] **Step 1: 실패하는 시험을 쓴다**

```rust
/// 스크린샷 요청이 떠 있으면 생산을 쉰다 -- 매 틱 생산하면
/// `has_pending_frames()` 가 영영 참이라 `--capture` 가 안 찍힌다.
/// ★이것은 프레임의 *내용* 을 묻는 것이 아니라 외부 모드이므로 규칙 3 을 어기지 않는다.★
#[test]
fn a_pending_screenshot_suspends_production() {
    assert!(!tick_should_produce(true));
}

#[test]
fn production_resumes_once_the_screenshot_is_taken() {
    assert!(tick_should_produce(false));
}
```

- [ ] **Step 2: 시험이 실패하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu tick_should_produce`
Expected: FAIL

- [ ] **Step 3: 규칙과 예외를 넣는다**

`produce_wall_frame` 맨 앞에서 `tick_should_produce` 로 걸러내고, 쉰 횟수를 1 초에 한 번
로그로 남긴다. 부트스트랩(`:3592`)은 ★파이프라인당 정확히 1 회, 그 파이프라인의 첫 틱 생산
이전에만★ 허용하고 계수기를 붙인다.

- [ ] **Step 4: 시험이 통과하는 것을 본다**

Run: `cargo test -p servo-paint --lib --features webgl/no-wgl,webgpu`
Expected: PASS

- [ ] **Step 5: Review Focus 3·5 — 스크린샷과 기동**

`winit_wall --capture` 가 찍히는지, 기동 화면이 검게 남지 않는지.

- [ ] **Step 6: Commit**

```bash
git add components/paint/paint.rs components/paint/painter.rs components/paint/wall_tick.rs
git commit -m "feat(paint): 스크린샷 중에는 생산을 쉬고, 부트스트랩 예외에 종료 조건을 준다"
```

---

### Task 10: 게이트 정리, 기본값 전환, 주석 갱신

**Files:**
- Modify: `components/paint/painter.rs` (게이트 다섯)
- Modify: `components/config/prefs.rs` (`gfx_single_frame_producer` 기본 `true`)
- Modify: `docs/multigpu/configuration.md`

- [ ] **Step 1: 설계 4 절의 처분표대로 게이트를 지운다**

`script_frame_is_redundant` / `script_frame_rides_the_painter`(술어 삭제, ack 부작용은
Task 6 에서 이미 이설), `raf_is_driving_composites` / `raf_is_producing_frames`,
`deferred_animation_frame_is_due` / `flush_deferred_animation_frame`,
`video_composite_due` / `video_composite_owed`.
`should_fast_present` 는 틱에서 직접 답을 받는다.

- [ ] **Step 2: 역사 주석을 설계 문서로 넘긴다**

지우지 말고 한 줄로 바꾼다:
`// 이 게이트는 2026-10-21 에 삭제됐다. 그것이 막던 증상(<한 줄>)은 생산자가 하나라서 구조적으로 불가능하다 — docs/superpowers/specs/2026-10-21-single-frame-producer-design.md 4 절.`

- [ ] **Step 3: 전체 시험**

Run: `cargo test -p servo-paint -p servo-config --lib --features webgl/no-wgl,webgpu`
Expected: PASS

- [ ] **Step 4: 기본값을 켜고 되돌림 방지 시험을 뒤집는다**

Task 5 의 `the_single_frame_producer_starts_off` 를 `..._is_on` 으로 바꾸고 기본값을 `true` 로.

- [ ] **Step 5: 설계 8 절의 검증표를 전부 돈다**

★네 구성★(페이싱 on / off / vsync / servoshell `--wall-all-tiles`)과 WPT.
`FRAMEREASON` 구별 값이 정확히 둘인지가 첫 판정이다.

- [ ] **Step 6: Commit**

```bash
git add components/paint/painter.rs components/config/prefs.rs docs/multigpu/configuration.md
git commit -m "feat(paint): 중재 게이트 넷을 걷어내고 단일 생산자를 기본으로 켠다"
```

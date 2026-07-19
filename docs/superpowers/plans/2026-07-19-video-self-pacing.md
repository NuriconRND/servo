# 비디오 자체 페이싱 v2 (임베더 wake 수정) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **v1 기각됨(2026-07-19)**: "도착 신호 추가" 접근은 전제 오류로 스모크 게이트에서 기각(스펙 §14 v1 기각 기록). 본 문서는 v2 전면 재작성본.

**Goal:** 무rAF 페이지의 비디오 재생 동결을 근본수정하고(임베더 wake 폭풍 → RedrawRequested 기아 해소) 무애니 월에서 컴포짓을 비디오 카데이던스(~30Hz)로 수렴시킨다 (스펙 §14 v2, 승인됨).

**Architecture:** 실측 근본원인(§14.1, pacing-investigation-report.md): rAF 없으면 `is_animating=false` → WakeCoalescer 억제 비활성 → frame-ready마다 무조건 wake post → Win32 posted 메시지가 WM_PAINT에 우선 → RedrawRequested 영구 기아 → 페인트 부재 → `display_composite_in_flight` 웨지 → §3-m 즉시 컴포짓 전량 차단. 수정 2본: **② frame-ready wake를 상태 전이 시에만 post**(자기지속 폭풍 차단, 일반 방어) + **① 재생 중 미디어를 animating 판정에 반영**(script 원천 → constellation → Paint → 임베더의 기성 사슬 재사용 — rAF가 우연히 제공하던 마스킹의 정식화).

**Tech Stack:** Rust (servoshell / servo-paint / script / constellation 기성 사슬), PowerShell 검증.

## Global Constraints

- 리포 `D:\2_TechReview\20260606_multigpu_browser\servo`, 브랜치 `multigpu-tiled-wall`. **푸시 금지.** 커밋 한국어, Claude 서명 금지.
- **§3-2 제약(절대)**: wake post 총량은 증가 금지 — 감소 방향만. 입력 처리 경로(WakeCoalescer의 억제 로직 자체, PeekMessage 순서 대응) 무접촉. 입력 무회귀는 Task 3에서 실측 필수.
- 킬스위치 2종(각 리터럴 "1"만 발동, 순수 함수+OnceLock 관례): `SERVO_DISABLE_WAKE_EDGE=1`(② 복귀), `SERVO_DISABLE_MEDIA_ANIMATING=1`(① 복귀).
- 근거 문서: `docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md` §14 v2, `.superpowers/sdd/pacing-investigation-report.md`(체인 표 file:line·계측 수치 — **구현 전 필독, 정확 지점의 정본**).
- 셸 포그라운드만(백그라운드 대기 금지 — 대기는 같은 호출 안 Start-Sleep), 타임아웃 600000ms, 실행 후 servoshell kill+env 정리.
- **빌드**: `Stop-Process servoshell` → `. .\etc\multigpu\servo_env.ps1` → `$ErrorActionPreference='Continue'` → `. .\.venv\Scripts\Activate.ps1` → `python mach build --release` (script/constellation 변경 시 오래 걸림 — 10분 타임아웃).
- 유닛: `cargo test -p servo-paint --lib --features paint_api/no-wgl <필터>` (paint 크레이트 대상일 때). servoshell 순수 함수는 `cargo test -p servoshell <필터>` — 실패 시 해당 크레이트 테스트 관례를 확인해 맞출 것.
- 런처 `-Page`는 리포 루트 상대경로. winshot은 `scratchpad\shot.png` 고정 → 복사.
- **무rAF 스모크 페이지**: `tests\html\video_grid_wall_clean.html`에서 rAF 블록만 제거한 사본을 `tests\html\zz_noraf_smoke.html`로 생성해 사용(각 태스크에서 생성/삭제 반복 대신 Task 1이 만들고 Task 3 마지막에 삭제).

---

### Task 1: ② frame-ready wake 전이 억제 (edge-trigger)

**Files:**
- Modify: 조사 보고서 체인 표의 **F 지점**(frame-ready → 무조건 `event_loop_waker.wake()` post 위치 — components/servo 또는 components/paint 쪽, 보고서 file:line이 정본) — wake를 "미결(outstanding) 상태 false→true 전이"에서만 post하도록.
- Modify: 같은 보고서의 **H 지점**(임베더 페인트/RedrawRequested 처리 완료 지점)에서 미결 상태 리셋 — 전이 부기의 해제 짝.
- Test: 전이 판정을 순수 함수/작은 타입으로 분리해 유닛(해당 크레이트 tests 모듈).

**Interfaces:**
- Consumes: pacing-investigation-report.md의 F/H 지점 file:line, 기존 wake 경로.
- Produces: `fn wake_edge_suppressed(token: Option<&str>) -> bool`(킬스위치 순수 판정 — "1"만 true=복귀) + 전이 부기 타입(예: `WakeEdge { outstanding: AtomicBool }` — set-if-clear가 post 허용을 반환, 페인트 시 clear). Task 2·3이 킬스위치 이름에 의존: `SERVO_DISABLE_WAKE_EDGE`.

- [ ] **Step 1: 조사 보고서 정독** — `.superpowers/sdd/pacing-investigation-report.md`의 체인 표(A~I 지점 file:line)와 WORKING/BROKEN 로그 대비를 읽고 F/H의 정확 위치·주변 코드를 파악한다.

- [ ] **Step 2: 실패하는 유닛테스트 작성** — 전이 부기 의미론(핵심 계약: 연속 set은 첫 번째만 post 허용, clear 후 set은 다시 허용 — "마지막 wake 유실 없음"):

```rust
    #[test]
    fn wake_edge_posts_only_on_transition() {
        let edge = WakeEdge::default();
        assert!(edge.should_post());   // 첫 래치 → post
        assert!(!edge.should_post());  // 미결 유지 중 → 억제
        assert!(!edge.should_post());
        edge.clear();                  // 페인트 완료
        assert!(edge.should_post());   // 새 전이 → 다시 post (유실 없음)
    }

    #[test]
    fn wake_edge_killswitch_literal_one_only() {
        assert!(!wake_edge_suppressed(None));
        assert!(wake_edge_suppressed(Some("1")));
        assert!(!wake_edge_suppressed(Some("0")));
        assert!(!wake_edge_suppressed(Some("true")));
    }
```

(타입/함수 배치는 F 지점이 속한 크레이트의 관례를 따르되 이름은 위와 동일하게. 킬스위치=1이면 `should_post()`를 항상 true로 우회 — 현행 무조건 post 복귀.)

- [ ] **Step 3: 실패 확인** — 해당 크레이트 test 명령으로 컴파일 실패 확인.

- [ ] **Step 4: 구현** — F 지점: `if edge.should_post() { waker.wake() }` (킬스위치 우회 포함). H 지점: 페인트 완료 시 `edge.clear()`. ★주의: clear는 "페인트가 실제로 일어난" 지점이어야 함 — 큐에서 Waker 이벤트를 꺼낸 시점이 아님(그러면 폭풍이 재발). 조사 보고서 H(=RedrawRequested→repaint 처리) 기준.★ 부기 접근이 스레드 경계를 넘으면 AtomicBool(SeqCst — WakeCoalescer 주석의 순서 논증 참조) 사용.

- [ ] **Step 5: 유닛 PASS + 빌드** — 신규 2테스트 PASS, `cargo test -p servo-paint --lib --features paint_api/no-wgl dcomp` 무회귀, 릴리스 빌드 성공.

- [ ] **Step 6: 무rAF 스모크 (② 단독 효과 판독)** — `tests\html\zz_noraf_smoke.html` 생성(위 Global Constraints), 실행:
```powershell
$env:SERVO_DCOMP_DEBUG="1"; $env:SERVO_VIDEO_ESCAPE_PROF="1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 2 -Rows 2 -DComp -VideoEscape canvas -Page tests\html\zz_noraf_smoke.html -Sync 4
# 같은 호출: Start-Sleep 40 → winshot → 복사(edge_on_a.png) → Start-Sleep 5 → winshot → 복사(edge_on_b.png) → Stop-Process
```
판독: 카운터 전진 여부(② 단독으로 동결이 풀리는지 — 예상: 풀림) + vesc-prof frames/presents(수치 기록 — ~30/s 수렴 여부는 ① 이후 최종 판정). `$env:SERVO_DISABLE_WAKE_EDGE="1"` 재실행 → 동결 재현(킬스위치 실효). env 정리. **어느 쪽이든 관측 결과를 있는 그대로 리포트에** — ②로 안 풀리면 DONE_WITH_CONCERNS로 보고(①이 주 수정).

- [ ] **Step 7: 커밋**
```powershell
git commit -m "frame-ready wake 전이 억제: 자기지속 wake 폭풍 차단(RedrawRequested 기아 해소 1/2, 스펙 14 v2 ②, 킬스위치 SERVO_DISABLE_WAKE_EDGE)"
```
(git add 대상은 실제 수정 파일 — F/H 지점 크레이트에 따름.)

---

### Task 2: ① 미디어 재생 → animating 전파

**Files:**
- Modify: `components/script/dom/html/htmlmediaelement.rs` — 재생 상태 전이(재생 시작/일시정지/종료/제거) 지점에서 문서 단위 재생 카운터 증감. 전이 지점은 요소의 기존 상태 머신에서 "실제 프레임을 렌더하기 시작/중단"에 해당하는 대칭쌍을 찾아 사용(예: playing 전이와 pause/ended/teardown — 파일 내 기존 상태 전이 메서드 정독 후 대칭 보장).
- Modify: `components/script/animations.rs:192-205` `handle_animation_presence_or_pending_events_change` — 상태 계산에 재생 중 미디어 반영.
- Modify: 카운터 보관처 — `Document`(또는 Window) 필드 + 증감 메서드 + 증감 시 상태 재전송 트리거.
- Test: 상태 판정 순수 함수 유닛(가능한 형태로) + 킬스위치 순수 함수.

**Interfaces:**
- Consumes: 기성 사슬 — script `ChangeRunningAnimationsState`(animations.rs:202) → constellation(:1817→:3737 `PaintMessage::ChangeRunningAnimationsState`) → Paint → 임베더 `WebView::set_animating` → servoshell `animating_webviews`/`is_animating`(running_app_state.rs:335) → WakeCoalescer(app.rs:280-295). **이 사슬은 전부 무수정** — 원천(script 상태 계산)에만 미디어를 더한다.
- Produces: `fn media_animating_disabled(token: Option<&str>) -> bool`("1"만 true) — env `SERVO_DISABLE_MEDIA_ANIMATING`; Document의 `note_playing_video_delta(i32)`(이름 재량, 카운터 0↔양수 전이 시 `handle_animation_presence_or_pending_events_change` 재평가 트리거 필수).

- [ ] **Step 1: 실패하는 테스트** — 상태 계산 순수화가 가능하면:
```rust
    #[test]
    fn animations_present_includes_playing_media() {
        // (기존 계산이 순수 함수로 분리 가능한 형태일 때 — 아니면 킬스위치 함수만 유닛하고
        //  상태 반영은 Task 3 실기 배터리에서 검증한다고 리포트에 명시)
        assert!(animations_present_state(false, false, 1));  // 미디어만 재생 → Present
        assert!(!animations_present_state(false, false, 0)); // 전부 없음 → NoAnimations
        assert!(animations_present_state(true, false, 0));   // 기존 애니 경로 보존
    }
```
+ 킬스위치 리터럴 테스트(Task 1 Step 2와 동형, 이름만 `media_animating_disabled`).

- [ ] **Step 2: 실패 확인 → 구현** — 계산부(:198-201)를 `has_running_animations || has_pending_events || (media_animating_enabled && playing_video_count > 0)`로. 카운터 증감의 대칭성(누수 시 영구 animating = Poll 상시 = CPU 회귀)을 요소 해체 경로까지 포함해 보장. 킬스위치는 env 1회 읽기.

- [ ] **Step 3: 유닛 PASS + 빌드** (script 변경 — 풀빌드 김).

- [ ] **Step 4: 스모크 (①+② 합산 최종 판독)** — Task 1과 동일 무rAF 페이지:
판독 3종: (a) 카운터 전진(동결 소멸) (b) **vesc-prof frames/presents ≈ 30/s 수렴**(§14.2 성능 목표 — ①로 §3-m 훅이 도착률 구동) (c) `SERVO_DISABLE_MEDIA_ANIMATING="1"` 시 ② 단독 거동으로 복귀(Task 1 Step 6 관측과 일치). 재생 종료/일시정지 후 animating 해제 확인(로그 또는 창 유휴 CPU — Poll 잔류 없음).

- [ ] **Step 5: 커밋**
```powershell
git commit -m "재생 중 미디어를 animating 판정에 반영: WakeCoalescer 억제 정식화(동결 근본수정 2/2, 스펙 14 v2 ①, 킬스위치 SERVO_DISABLE_MEDIA_ANIMATING)"
```

---

### Task 3: 검증 배터리(입력 무회귀 포함) + 페이지/가이드 되돌림 + 패키지 + 스펙 기록

**Files:**
- Modify: `tests/html/video_grid_wall_clean.html`(빈 rAF 제거 → "자체 페이싱(스펙 §14 v2)" 주석), `etc/multigpu/package_run_wall.ps1`(rAF caution → v2 안내+AMD 판독 항목), 스펙 `§14.6 구현 결과`.
- 패키지: exe/런처/페이지 재복사+zip. 삭제: `tests\html\zz_noraf_smoke.html`.

**Interfaces:**
- Consumes: Task 1·2의 킬스위치 2종, 기존 도구(winshot/vesc-prof/`tests/html/mouse_count_probe.html`/scratchpad의 synth mouse wiggle 스크립트 — §3-2 검증에 쓰였던 것들).

- [ ] **Step 1: 클린 페이지 rAF 제거** — 빈 rAF 블록+주석 삭제, 교체 주석: `<!-- 스펙 §14 v2: 재생 중 미디어가 animating으로 반영되고 frame-ready wake가 전이 억제되어, 이 페이지는 rAF 없이 임베더가 페인트를 정상 순환한다. 킬스위치 SERVO_DISABLE_MEDIA_ANIMATING=1 + SERVO_DISABLE_WAKE_EDGE=1 동시 설정 시에만 동결 재현(진단). -->`

- [ ] **Step 2: 45타일 핵심 게이트** — `-Cols 9 -Rows 5 -DComp -VideoEscape canvas -Page tests\html\video_grid_wall_clean.html -Sync 45`, PROF on, 60초: 재생 정상(winshot 2매 lockstep·전진) + **frames/presents ≈ 30/s** + canvas 재생성 1회.

- [ ] **Step 3: ★입력 무회귀 (§3-2 재발 방지 — 필수 게이트)★** — ①로 비디오 재생 중 animating=true가 되어 wake 억제 구간이 넓어졌으므로, §3-2 당시 도구로 마우스 전달률 재확인: 2×2 canvas + `tests/html/mouse_count_probe.html`(비디오 없는 프로브 페이지라면 비디오 페이지에서 synth wiggle과 함께 — scratchpad의 `wall_synth_mouse_wiggle.ps1` 계열 존재, ★SetCursorPos 방식이어야 함: 상대 SendInput은 가짜 0 함정(§3-2 기록)★). 판독: 재생 중 마우스 이벤트 전달률이 §3-2 수정 후 수준(0이 아닌 정상 카운트)인지. 프로브 페이지가 비디오와 동시 로드 불가 구조면 비디오 월 위에서 wiggle→로그의 입력 이벤트 카운트로 대체하고 방법을 리포트에 명시.

- [ ] **Step 4: 무회귀 배터리** — perf 페이지(rAF 월: 컴포짓률 이상 상승 없음·재생 정상), mixed_media_demo(-Sync 6: 전 요소 정상), play 페이지 escape off(`-Cols 3 -Rows 3 -DComp`: WR 경로 정상), off(비DComp 2×2: 소프트웨어 경로 정상), 킬스위치 2종 각각=1 조합 스모크(각 복귀 거동 — Task 1/2 관측 인용 가능).

- [ ] **Step 5: 가이드/패키지** — package_run_wall.ps1의 rAF caution 블록 교체(영어):
```powershell
#   NOTE (self-pacing v2, 2026-07-19): playing media now counts as "animating" and
#   frame-ready wakes are edge-triggered, so wall pages need NO rAF/heartbeat and
#   composites converge to the video cadence (~30/s on the wall). Diagnosis levers:
#   SERVO_DISABLE_MEDIA_ANIMATING=1 / SERVO_DISABLE_WAKE_EDGE=1 (set both to reproduce
#   the old rAF-dependent freeze). Expected on AMD at 6x6: canvas GPU% ~90% -> ~56%.
```
패키지: exe+run_wall.ps1+클린 페이지 복사 → zip 재생성 → 패키지 스모크(2×2 클린 페이지 재생). `zz_noraf_smoke.html` 삭제.

- [ ] **Step 6: 스펙 §14.6 결과 + 커밋** — `### 14.6 구현 결과 (2026-07-19, v2)`: 커밋 체인, ②단독/①+② 스모크 수치, 45타일 30/s, 입력 무회귀 수치, 무회귀 4종, 이탈(없으면 "이탈 없음").
```powershell
git commit -m "자체 페이싱 v2 마감: 클린 페이지 rAF 제거(동결 수정 실증), 입력 무회귀 실측, AMD 가이드 갱신, 패키지 재생성(스펙 14.6)"
```

---

## Self-Review 결과

1. **스펙 커버리지(v2)**: §14.1 근본원인→계획 전제(조사 보고서 필독 지시), §14.2 ②→Task 1, ①→Task 2, 킬스위치 2종→각 태스크, §14.3 페이지 되돌림→Task 3 Step 1, §14.4 검증 1~4→Task 1/2 유닛+Task 3 Step 2-4(입력 무회귀 Step 3 명시), §14.4-5 AMD 가이드→Task 3 Step 5. 갭 없음.
2. **플레이스홀더**: F/H 지점 file:line은 pacing-investigation-report.md(실존 증거 문서, 체인 표 보유)를 정본으로 지정 — 계획 내 재전사 대신 참조(구현자 필독 Step 1). Task 2 Step 1 테스트에 "순수화 불가 시 대체 검증 명시" 분기 포함 — 조건부이지 공백 아님.
3. **타입 일관성**: `WakeEdge{should_post/clear}`, `wake_edge_suppressed`/`media_animating_disabled`(둘 다 Some("1")만 true), env 2종 이름 — 태스크 간 일치.

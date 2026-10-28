# 계측 전면 제거 — 경과 (브랜치 `wall-strip-instrumentation`)

계획서: `docs/superpowers/plans/2026-10-27-instrumentation-strip.md`
인벤토리·재검증 전문: `docs/superpowers/plans/notes/strip-inventory/`

**이 파일은 단계마다 갱신한다.** 한 화면에서 (1) 무엇을 지웠나 (2) 게이트 결과
(3) 계획서가 틀린 곳 (4) 피한 함정을 보기 위한 것이다.

## 요약

| | |
|---|---|
| 진행 | **20 / 20 단계 — 완료** |
| 누적 제거 | **약 20,489 줄** |
| 계획서 오류 (발견) | **83 건** (실행 중 55 + 재검증 28) |
| 조용한 회귀 회피 | **26 건** |
| ★합쳐진 doc 함정★ | **23 회** — doc 주석이 설명 대상이 아닌 다른 항목에 물리적으로 붙어 있는 경우. 이제 패턴으로 취급한다 |
| 기준선 | `servo-paint --lib` 130/130 → 현재 **93/93** (계측 테스트 37 개 제거분) |
| `config_surface` | 19/20 (기존 RED) → Step 14 에서 **20/20 달성** ★제거가 완결됐다는 유일한 독립 증거★ |
| 게이트 ★G4 불능★ | `cargo check -p servo-layout -p servo-script` 는 이 장비에서 못 돈다 — `mozjs_sys` 의 `jsapi.cpp` 가 설치된 MSVC 와 안 맞는다. 제거와 무관(손 안 댄 crate 로도 재현). **G2(`winit_wall` check)가 두 crate 를 소스에서 타입체크하므로 대체된다.** |

---

## 단계별

### Step 1 — `note_commit_failure` 의 `out=` 장식 · 디바이스→모니터 맵 `3269fc19b0d`

`dcomp_compositor::note_commit_failure` 의 `out=` 필드와 그것을 만드는 두 문장,
`commit_guard` 의 `DEVICE_MONITOR`/`remember_device_monitor`/`monitor_for_device`.

**왜 1 번인가 (강제)** — `output_grid::publish_names` 가 `NAMES` 의 유일한 writer 인데
Step 12 에서 `probe_loop` 와 함께 죽는다. 그 뒤로 `out=` 은 영원히 `?` 다. 그래서
"이 에러 보고를 남길 것인가" 를 그 전에 정해야 했다. **남겼다** — 초당 통계가 아니라
TDR / device-removed / SURFACE_BEING_RENDERED 보고다.

게이트 130/130.

### Step 2 — `components/webgl` 계측 `0bea8248130` (419 줄)

`spawn_webgl_watchdog`(1 Hz 샘플링 전용 스레드)와 그것이 보던 원자 다섯 개, `PHASE_*`,
`phase_name` / `log_live_contexts`(WEBGLLIVE) / WEBGLFANOUT 일체(게이트, 14 필드 누적기,
스톱워치 여덟) / WEBGLRECLAIM 의 계측부 / `adapter_local_usage_bytes` + winapi `dxgi1_4`.

**피한 함정** — `apply_webgl_command_to_surface` 에서
`let _angle_gl_guard = paint_api::angle_gl_lock(...)` 가 **스톱워치 두 문장 사이**에 끼어
있었다. 범위 삭제했으면 RAII 락이 사라져 WebGL 스레드가 타일 렌더와 같은 ANGLE 디바이스를
경합한다 — `libGLESv2` 접근 위반이고 **단일 GPU 개발기에서는 재현되지 않는다.**

**관측** — `WEBGLLIVE` 는 초당 한 줄로 보였지만 게이트 없이 **모든 메시지마다** 돌고
있었다. 맵 두 번 순회 + `RwLock::read` 가 메시지당.

**하중 카운트 대조** — `angle_gl_lock` 8/8, `swap_buffers(` 4/4, `clear_surface(` 2/2,
`gl.flush()/finish()` 4/4. 게이트 130/130.

### Step 3 — dcomp 자립 로그 게이트 · DWMPHASE · `dwm_timing()` `26225f22647` (786 줄)

`[vesc-prof]` 171 / readback+validprobe 174 / `[dcomp-dbg]` 184 / DWMPHASE 141 / `dwm_timing()` 28
+ `output_grid` 쪽 81.

**계획서 오류 2 건**
- `[dcomp-dbg]` 는 17 이 아니라 **16** (중괄호 대조로 전수 확인).
- ★`dwm_timing()` 의 소비자가 둘이 아니라 **셋**★ — `outvb_after_compose_ms` 가 `qpcCompose`
  를 읽고 그 필드는 `dwm_timing` 만 공급한다. 둘만 지우면 **`output_grid.rs` 가 컴파일되지
  않는다.** 셋째와 그것만 쓰던 `fold_centered` 까지 제거.

**남긴 것** — `composition_grid()`. 호출처 일곱 중 여섯은 로그지만 일곱째가
`lib.rs:64 dwm_composition_grid` → `winit_wall/main.rs:1235 snap_to_dwm_grid_at`,
즉 페이싱 클럭의 타이머 폴백이다. `DwmGetCompositionTimingInfo` 를 감싼 거의 똑같은 함수
둘인데 판정이 반대다.

하중 카운트 13 종 불변. 게이트 130/130.

### Step 4 — dcomp 커밋 스탬핑 · COMMITSITE/COMMITWORK · DCOMPBIND `79855e8f0ae` (1,121 줄)

`CommitOutcome`(→ `raw_commit` 이 `i32` 직접 반환) 55 / COMMITSITE·COMMITWORK 440 /
DCOMPBIND 420 / 계측 테스트 15 개 201.

**계획서 오류 3 건**
- 게이트는 113 이 아니라 **115**. 인벤토리의 "17 items" 에 테스트가 아닌 헬퍼 둘이
  섞여 있었고 계획서가 그걸 "테스트 17 개" 로 옮겨 적었다. 실제 `#[test]` 는 15.
  남은 22 개가 전부 동작 커버리지임을 **이름 단위로 대조**했다.
- `raw_commit` 호출처는 넷이 아니라 **셋**("넷" 은 `CommitSite` 변종 수).
- `commit_device_guarded` 도 `site` 인자를 들고 있었는데 계획서에 없다.

**남긴 것** — `DCOMP_BIND_PROF` static. `COLLECT` 의 우변이라 지금 지우면 Step 13 까지의
중간 커밋들에서 **`COLLECT` 가 기본 ON 으로 뒤집힌다.** 커밋 사이를 육안 비교하는 동안
계측이 켜지면 안 된다.

하중 카운트 18 종 불변. 게이트 **115**.

### Step 5 — `painter.rs` 애니메이션·FRAMEREASON·렌더 경로 `e31d4042933` (1,519 줄)

애니메이션 텔레메트리 505(필드 25 개 포함) / FRAMEREASON 200 / `render()` 로그 블록 넷 215 /
GPUMEM 190 / 캔버스 ack 래치 115 / IMGUPDINNER 85 / WALLBOOTSTRAP 42 / PAINTANIMANCHOR 34 /
`PumpSource::note` 13.

**계획서 오류·누락 4 건**
- 게이트는 111 이 아니라 **113**(다섯 단계 연속 집계 오류).
- `last_render_end` 의 doc 주석이 **엉뚱한 필드에 붙어 있었다** — 선언보다 14 줄 위,
  `owns_presentation` 의 doc 머리다.
- ★`painter_busy` 는 단순히 "안 읽히는 값" 이 아니다★ — `pending_frames > 0 ||
  renderer_behind()` 가 **단락 평가**라 `renderer_behind()` 안의 2 초 강제 해제는
  `pending_frames == 0` 일 때만 돈다. 맨 호출로 접었으면 지금은 닿지 않는 상태에서
  강제 해제가 발화한다.
- `log_gpu_memory` 호출처는 맨 호출이 아니라 `if let Some(device) = ...` 3 줄 래퍼.

하중 심볼 41 종 중 35 불변, 변한 여섯은 전부 로그 인자였음을 백업과 대조. 게이트 **113**.

### Step 6 — 프레임 요청/완료 진단 축약 · `paint.rs` 집계기 `0a5d990a884` (756 줄)

★삭제가 아니라 **축약**인 것 넷★ — 이름에 "diagnostic" 이 붙어 있지만 벽 프레임 barrier 의
요청→완료 대응을 나르는 하중이다: `record_frame_request`(본문이 push_back 하나로),
`PendingFrameDiagnostic`, `note_webrender_frame_ready`, `FrameReadyDiagnostic`.

`paint.rs` 에서 제거: `note_demand_tick`, `log_wall_frame_metadata`,
`log_wall_frame_pacing_summary` 와 `WallFramePacingBlockTally`, PAINTDRAIN 전후,
IMGUPDRATE 일체, media image fanout summary, WALLSHOTPAUSE 꼬리, barrier 로그 줄.
`shared/paint/lib.rs` 에서 `impl PaintMessage { kind() }`.

**★피한 함정 — barrier `if/else-if` 체인★** 인벤토리의 "1000-1032 삭제" 는 쓰지 않았다.
그 범위가 `else if !barrier.presentation_decision_made { complete_before_deadline = true; }`
를 자르는데 그 팔이 하중이다. 범위대로 지우면 `} else if` 가 매달리고, 중괄호를 "고치려고"
그 팔을 떨구면 **모든 before-deadline barrier 가 after-deadline 으로 조용히 바뀐다.**
최종 모양(직접 확인):

```rust
if barrier.ready_painter_ids.len() == barrier.expected_painter_ids.len() {
    barrier.completed_at = Some(diagnostic.ready_at);
    if !barrier.missed_deadline && !barrier.presentation_decision_made {
        complete_before_deadline = true;
    }
}
```

**계획서 오류 4 건** — ① `PaintMessage::kind()` 의 호출자가 둘(둘째가 살아남는 테스트 안의
`fn kinds()`). 첫 빌드가 `E0599` 로 죽었고 `strum IntoStaticStr` 로 고쳤다.
② ★`WALL_FRAME_PACING_INFO_INTERVAL` 은 지우면 안 된다★ — 인벤토리가 적은 두 용처 중 하나가
실은 "keep-previous expired" warn 의 rate limit 이고 그건 실제 조건 보고다.
③ `diagnostic.local_frame_id` 독자 목록 불완전(고장 주입 warn 누락, 그건 KEEP).
④ `WallFrameBarrier.requested_at` 이 고아가 되는데 계획서에 없다.

게이트 113/113(변동 없음).

### Step 7 — paint 나머지: LOCKWAIT·FRAMETIME·ANIMTICK·PAINTERLOCK·WEBGLEXTIMG·TIMELINESTEP `36df08f56fe` (869 줄, 9 파일)

`commit_guard` −153 / `web_content_animation` −287 / `frame_time` −119 /
`webrender_external_images` −110 / `paint` −84 / `painter` −62 / `refresh_driver` −28 /
`dcomp_compositor` −14 / `output_grid` −1.

**남긴 것 (전부 직접 확인)**
- ★`commit_guard` 의 뮤텍스 독 복구 둘★ — `DEVICE_LOCKS.lock().unwrap_or_else(|e| e.into_inner())`
  와 `device_guard` 의 `lock.lock().unwrap_or_else(|e| e.into_inner())`.
  `WORKERS`/`DONE`/`CommitWorker`/`ensure_workers` 와 `commit_all_at_tick` 의 디스패치·recv·
  순차 폴백, `SAMPLE_LEAD_PERIODS` 전부.
- ★`frame_time.rs` 의 rAF 배관 전부★ — `FRAME_TIME_NANOS`·`note_frame_time`·`stored_frame_time`·
  `fresh_frame_time`·`frame_time_for_tick`. 페이지가 받는 타임스탬프이므로 기능이다.
- ★`MAX_DISPLAY_LIST_TRANSIT`★ — `animation_zero` 가 그것으로 클램프하고 그 값이 그려진다.

`output_grid.rs` 는 **한 줄만** 건드렸다(`emit_lockwait()` 호출). 이중 소유권을 이 단계가
가져가기로 계획서가 정했다. 나머지는 Step 12/13 소유.

**계획서 오류·누락 5 건** — ① 게이트 101 이 아니라 **103**(일곱 번째 집계 오류, 델타 −10 은 맞음).
② ★`install_paint_animations` 의 doc 이 물리적으로 `note_timeline_step` 에 붙어 있었다★ —
한 줄 위에서 잘랐으면 **컴파일 에러 없이** 그 doc 이 사라진다(Step 5 의 `last_render_end` 와 같은 부류).
③ `device_guard` 의 독 복구가 셋이었다(`COLLECT` 조기 반환 팔에 중복).
④ `frame_time` 의 doc 두 줄이 이미 죽은 `FRAMETIME`·`CommitSite` 를 인용.
⑤ `dcomp_compositor::commit_at_tick_locked` doc 에 죽은 문단.

**ANIMTICK warn 셋 중 둘은 `if self.animating.get()` 래퍼의 유일한 문장**이라 래퍼를 같이
지웠다. 바깥 `if`/`return`/`set` 은 전부 유지.

★Step 15 참고★ — 이 단계 뒤 `components/paint` 에 `interval_summary` 사용자가 **0** 이다.
트리에 남은 소비자는 `script/raf_clock.rs` 뿐이고 Step 10 이 지운다.

게이트 113 → **103**. 지운 테스트 10 개는 전부 계측(LOCKWAIT 히스토그램 1, TIMELINESTEP
포맷터 4, TRANSITCLAMP 보고기 1, ANIMSTEPSEQ 포맷터 4). 픽셀을 바꾸는 클램프는
`an_absurd_transit_is_clamped` 가 계속 덮는다.

### Step 8 — winit_wall 패스 타이밍·MAINBUSY·Present perf · SPINSLOW · TileRenderTiming · LOG_PRESENT_CADENCE `c1bddad8c8b` (559 줄, 8 파일)

`main.rs` 2739 → 2300. 패스 타이밍 250(`note_render_pass` 가 PASSEVENT·LOG_PASS·
WALLPASS/WALLQUEUE/WALLSPLIT 방출기를 품고 있었다) / MAINBUSY 91 / Present perf 36 /
`render_all_tiles` 내부 스톱워치 68. `servo.rs` 의 SPINSLOW 27.
`TileRenderTiming` 과 재수출 둘. `LOG_PRESENT_CADENCE` 삼종.

**★전체에서 가장 위험했던 한 줄 — 살았다★** barrier 분기에서
`dispatched[index] = true;` 가 계측 `skipped += 1`(위)과 하중 `continue;`(아래) **사이**에
끼어 있었다. 떨궜으면 barrier 가 건너뛴 타일을 두 번째 루프가 전부 다시 칠하는데
**컴파일 에러도 실패 테스트도 없다.** `dispatched[` 3/3, `continue;` 7/7 로 대조.

★`cache_index` 15 개 재번호는 파일 순서가 아니라 `ALL` 을 파싱해 유도했다★ —
`:275` 의 const 가 index 24 인데 17 과 18 사이에 선언돼 있다.

**계획서 오류** — `embedder_messages` 가 세 자리인데 선언만 적혀 있었다(하드 에러);
`TileRenderTiming` 의 `join()` 반환형을 계획서·인벤토리 **둘 다** 빠뜨렸다(재검증이 잡음);
`configuration.md` 의 참조가 둘이었다.

**★제거 시작 이후 처음으로 엔진 example 이 컴파일 검증됐다★** —
`cargo check -p servo --example winit_wall` exit 0, 경고 0. 앞선 7 단계는
`servo-paint --lib` 만으로 검증했는데 그 게이트는 `winit_wall` 을 컴파일하지 않는다.
`config_surface` 18/2 — 두 RED 는 예고된 것(Step 14 까지).

### Step 9 — winit_wall WALLCLOCK 과 틱 이슈 감사 `9276feb51be` (354 줄)

`main.rs` 2300 → 1993. `ClockStats`/`TickIssueAudit`/`MAX_CLOCK_GAP_SAMPLES` 100 /
note·report 메서드 일곱과 32 인자 `WALLCLOCK info!` 170 / 호출처 여덟 / 감사와 그 테스트 둘.

**★래치를 먼저 꺼냈다★** `note_tick_site` 안에 묻혀 있던 `last_pacing_tick_at` 쓰기를
`note_pacing_tick` 으로 분리. 호출 경로 둘(`pump_vsync`, `drive_present_clock_timer`)과
독자 둘(`WakerEvent::Engine` 가드, `needs_pacing_backstop`) 불변.
★이 쓰기를 덮는 테스트가 **없다**★ — 계획서가 가드라고 지목한 테스트는 불리언 둘짜리
자유 함수만 부른다. 떨구면 **모든 게이트가 초록인 채로** `pacing_is_driving()` 이 영구
false 가 되고 백스톱이 매 wake 마다 열린다. 그 사실을 테스트 doc 에 네 줄로 박아 넣었다.

**★삭제 후 재측정이 near-miss 를 잡았다★** — 계획서는 `note_present_tick` 을 심볼 하나로
적었는데 `pump_vsync` 에 둘째 호출이 있었다(래치 호출 한 줄 위). 카운트가 0 이 아니라 1 로
나와 발견. 방법 3 단계가 선택이 아님을 보여 준다.

낡은 doc 인용 13 곳 정정(재검증 목록 밖이 여섯). 그중 하나는 **사실과 반대**였다 —
`ClockSite::AboutToWait` 의 doc 이 "더는 구성되지 않는다" 인데 백스톱에서 구성된다.

게이트: G2 exit 0 / example 테스트 **20 → 18** / `servo-paint` 103 불변.
계획서의 "남는 clock 테스트 다섯" 은 틀렸다(아홉 중 일곱 생존).

### Step 10 — `components/layout` 와 `components/script` `619f93b2494` (1,440 줄, 13 파일)

layout: `paint_animation.rs` 의 `BindTally`/`tally()` 12 곳·엣지 로그·`describe_animations`
−451, `mod.rs` −12, `stacking_context.rs` −18.
script: `raf_clock.rs` 파일째 −135, `script_thread.rs` −211,
`htmlmediaelement.rs` −254, `webglrenderingcontext.rs` −113, `document.rs` −70,
`window.rs` −50, `animations.rs` −58, `globalscope.rs` −37, `microtask.rs` −31.

**★조용한 회귀 셋 회피 — 재검증이 지목한 것이 전부 실재했다★**
- `display_list/mod.rs:560` 이 계획서가 "유지" 라고 한 바로 그 `return false;` 였다.
- `stacking_context.rs:675` 는 `let animated_transform` 바인딩의 닫는 `});` 였다.
- `animations.rs` 의 ANIMUNBIND/ANIMDROP 는 `if` 래퍼의 유일한 문장이었다 —
  래퍼를 지우고 `set.cancel_all_animations()` 둘을 남겼다.

`LIVE_CONTEXTS` 의 prune 을 삭제 **전에** `register_for_reporting` 으로 옮겼다
(Vec 이 자라는 유일한 자리, 이미 `borrow_mut()`).

**★재검증 자체가 같은 off-by-one 을 반복했다★** — `animations.rs` 범위를 196-211 /
280-296 으로 적었는데 **211 과 296 이 바로 `cancel_all_animations()`** 다. 그대로
따랐으면 경고하려던 그 회귀를 냈다. 올바른 범위는 196-210 / 280-295.

**★합쳐진 doc 함정 3·4 회차★** — `script_thread.rs:1224-1254` 가 **세 함수의 doc** 이
붙은 덩어리였다(맨 위 6 줄이 `update_the_rendering` 의 spec doc). 심볼 단위로 지웠으면
그 문서가 조용히 사라지거나 E0585 가 난다. `webglrenderingcontext.rs:184-213` 도 같은 모양.
Step 5·7 에 이은 세 번째·네 번째다. **이제 패턴으로 취급한다: 함수를 지우기 전에 위 doc
블록이 그 함수를 설명하는지 확인한다.**

**★게이트 G4 불능 발견★** — `cargo check -p servo-layout -p servo-script` 가 이 장비에서
`mozjs_sys` 네이티브 빌드로 죽는다. 손 안 댄 crate 로도 재현되므로 제거와 무관하고,
Steps 1-9 가 G4 를 쓴 적이 없어 이제 드러났다. G2 가 두 crate 를 소스에서 타입체크한다.

게이트: G2 exit 0(`servo-layout` 경고 0) / G1 103 불변. fmt 드리프트 0.
★Step 15 인계★: 이 단계 뒤 `shared/base/interval_summary.rs` 사용자가 트리에 **0**.

### Step 11 — `components/media` `913bd0e0a87` (1,107 줄, 9 파일)

비디오 경로의 초당 통계 일체. `d3d11_ring.rs` 가 가장 컸다.

**★가장 위험했던 것 — 존재하지 않는 이름★** 계획서는 뮤텍스 독 복구 헬퍼를
`lock_registry` 라고 적었다. 그런 이름은 저장소에 없다. 실제 이름으로 찾아내 살렸다 —
놓치고 `.unwrap()` 으로 돌아갔으면 한 번의 독(poison) 으로 **모든 타일 비디오가 영구
정지**하고, 로그가 없으니 원인도 안 보인다. `lock_recover` 로 재배치하고 호출처
**28 곳** 전부 교체. 옛 이름 여섯은 저장소 전체 0, `unwrap_or_else(|poison|` 총수 8 불변.

**★합쳐진 doc 함정 3 회(누적 8·9·10)★** 그중 하나가 ★독 복구 설명 doc 이 `LockStats`
(삭제 대상)에 붙어 있던 것★ — 범위로 지웠으면 살아남는 코드의 유일한 설명이 사라진다.

범위 어서션 5 회 발화. 전부 삭제 전에 잡혀 파일 무손상.

### Step 11b — media-thread `Slow raw plane *` warn 셋 `23eb91f394b` (31 줄)

★계획서에 아예 없던 것★. Step 11 을 커밋한 뒤 같은 영역을 다시 훑다가 발견했다.
프레임마다 경과를 재서 임계 넘으면 `warn!` 하는 셋 — 정확히 제거 대상의 모양인데
인벤토리 어느 줄에도 없다. 인벤토리는 심볼 이름으로 찾았고 이것들은 이름이 없었다.

### Step 12 — output_grid 프로브 스레드·모든 emit_*·statics·`comp_stats.rs` `7a9c72c306e` (2,236 줄)

**단일 단계 최대.** `output_grid.rs` 2,853 → 851. 1 Hz 프로브 스레드와 그것이 쓰던
전역 전부, `comp_stats.rs` 파일째.

**★어서션은 범위를 증명하지만 손으로 쓴 대체는 증명하지 못한다★** — 에이전트가
`use` 블록을 손으로 접다 `IDXGIFactory1` 을 빠뜨렸다. 어서션은 전부 통과했고
**빌드만이** 잡았다(`E0425`/`E0433`). 이후 규칙: 손으로 쓴 교체가 있으면 반드시 빌드한다.

합쳐진 doc 함정 4 회(누적 11~14).

### Step 13 — output_grid `note_*` 진입점과 `COLLECT`/`PROBE_ENABLED`/`DCOMP_BIND_PROF` `e5033e2f697` (328 줄)

`output_grid.rs` 851 → **570**. 집계로 들어가던 `note_*` 와 게이트 셋.

**★하중이 계측 게이트 뒤에 있었다★** — `note_dcomp_stat` 이 `COMPOSITION` 에 저장하는
것은 **그리는 값을 정하는 B2 체인**(`GetFrameStatistics` → `lead_to_next_composition`)
인데, 그 저장이 `PROBE_ENABLED` 뒤에 있었다. 게이트와 함께 지웠으면 프로브가 꺼진
기본 실행에서 lead 계산이 조용히 틀어진다. 최종형은 **어떤 게이트 뒤에도 없다.**

합쳐진 doc 함정 2 회(누적 15·16). 둘 다 `grep -w` 로는 안 잡히고 죽은 심볼 스캔이 잡았다.

### Step 14 — `debug_env` 레지스트리 `d95df704bd2` (380 줄) ★`config_surface` 20/20★

`debug_env` 플래그 **15 개 제거, 9 개 생존**. 남은 아홉은 전부 사용자가 유지하기로 한
고장 주입·킬스위치이고 live reader 를 확인했다. **pref 는 하나도 안 지웠다.**

**★계획서 최대 오류 — grep 이 `vendor_local` 을 안 봤다★** `SERVO_WR_SLOW_MS` 가
"런처 전용, `components` 에 독자 없음" 으로 적혀 있었으나 실제로는
**`vendor_local/webrender` 에 `LazyLock` 독자**가 있었다. 계획서대로 grep 했으면
"아무것도 안 남았다" 고 **잘못 확인**했을 것이다. env 노브만 상수로 바꾸고 `WRSLOW`
로그 블록은 Step 20 소유라 그대로 뒀다 — 여기서 지우면 다른 단계 파일에 지역변수
아홉이 고아로 남는다.

★이 테스트는 제거 시작 **전부터** 19/20 이었고 빨간 둘이 **모두** 계측 때문이었다★ —
미등록 env 와 독자 없는 플래그. 둘 다 해소돼 **20/20**. 컴파일이 되는 것과 제거가
끝난 것은 다른 얘기인데, 이것이 후자를 증명하는 유일한 검사다.

### Step 15 — 엔진 고아: `interval_summary` 와 좌초한 winapi 피처 `3fcbdd421f0` (188 줄)

`components/shared/base/interval_summary.rs` 파일째(185 줄, `#[test]` 8 개) +
`lib.rs` 의 `pub mod` + `components/paint/Cargo.toml` 의 winapi `psapi`·`dxgi1_4`.

★Step 14 의 교훈을 그대로 적용했다★ — 사용자 0 임을 **`vendor_local/` 을 별도 실행으로
포함한** 트리 전체 ripgrep 두 번으로 확인했다. glob 재수출 경로도 없음을 확인.
삭제 후 워크스페이스 어느 `Cargo.toml` 도 `psapi`·`dxgi1_4` 를 켜지 않으므로 피처
통합이 제거를 가려 줄 여지가 없다 — 초록이 실제 증명이다.

**★`processthreadsapi`·`profileapi` 는 유지★** — 전자는 `painter.rs:508` 의
`SetThreadPriority`, 후자는 `output_grid.rs:29` 의 QPC/QPF 로 둘 다 하중이다.
(계획서는 `processthreadsapi` 의 사용처를 셋으로 적었으나 Steps 1-14 뒤 **하나**다.
KEEP 판정은 유효하고 근거만 낡았다.)

**계획서 오류** — 게이트 기대값 "91/91" 이 틀렸다. `interval_summary` 의 테스트 8 개는
`servo-base` 에 있어서 `servo-paint` 수치는 **움직일 수가 없다**. 이 단계의 델타는 0 이다.
게이트 수 오차 **15 연속**.

### Step 17 — 타일 스레드 스파이크 하네스(TILESPIKE) `f8757464f4a` (194 줄, 5 파일)

`tile.rs` 474 → 327, `main.rs` 1993 → 1961, 런처에서 `-TileThreadSpike` 배선.
★타일 스레딩 자체는 production 코드라 유지★ — `split_plan_for_own_thread`,
`create_tile_windows`, `bind_context`, `plan_tile_windows` 전부 생존.

**★계획서가 블록 크기 자리에 파일 크기를 적었다★** — 인벤토리 행
`| tile.rs | 474 | … | only the spike harness |` 의 474 는 **파일** 길이다. 블록은
`:331-474` 의 **144 줄**이고 파일은 327 줄을 남긴다. 그대로 믿었으면 파일을 통째로
지워 **타일 창 생성 전부**가 함께 사라진다.

**계획서는 `main.rs` 배선을 두 자리로 적었으나 실제로는 일곱 자리였다** —
`App::SpikeFinished` variant 와 프로세스 종료 코드 arm 을 포함해 다섯이 목록에 없었다.

**★합쳐진 doc 함정 17 회차 — 처음으로 PowerShell 에서★**
`run_wall_dist.ps1:472-476` 은 `-CanvasAckSkip` 의 다섯 줄 주석인데 `-TileThreadSpike`
주석 바로 **위**에 있고, 자기 파라미터는 여덟 줄 **아래**(`:490`)에 있다. "파라미터 위
주석 블록" 기준으로 지웠으면 **남아야 할 스위치의 문서가 조용히 사라진다.**
Rust 가 아닌 곳에서 같은 패턴이 처음 나왔다.

### Step 16 — 런처 분석부·계측 스위치·분석 도구 일체 `a5a9dd81cb0` (5,711 줄, 15 파일)

**단일 단계 최대.** `run_wall_dist.ps1` 2706 → 980, `make_wall_dist.ps1` 284 → 262,
추적 중인 분석 도구 12 파일 삭제.

**★유지 — 1b 절이 계획서를 이긴다★** `-CanvasAckSkip` 과 `-NoCanvasAckRecovery`.
계획서 item 6 은 둘 다 제거 목록에 넣었으나, 편집 전 `:915`/`:918` 이
`SERVO_WALL_CANVAS_ACK_SKIP` 과 `SERVO_WALL_DISABLE_CANVAS_ACK_RECOVERY` 의 **유일한
writer** 였고 세 줄 위의 무조건 `SERVO_*` wipe 때문에 런처 스위치가 유일한 도달 경로다.
지웠으면 유지하기로 한 킬스위치 둘이 못 닿는 것이 된다.

**★계획서가 못 본 것 — `-LegacyLogRedirect` 제거가 launch 분기를 좌초시킨다★**
`$LegacyLogRedirect` 가 live 조건 두 곳에 있었고 `Set-StrictMode` 가 없으니
`-not $null` 은 `$true` 다. 스위치만 지웠으면 `else { Start-Process -RedirectStandardError }`
가 **살아 보이는 죽은 코드**가 된다. if/else 를 걷고 cmd 래퍼 경로만 남겼다.

**범위 오류 다섯, 그중 셋은 살아야 할 코드를 지운다** — 특히 `-ThreadCpu` 샘플링
범위 `:1292-1318` 의 시작 줄이 `if ($DurationSec -gt 0) {` 다. 그대로 지우면
duration/stop-process 경로가 통째로 사라진다.

**★합쳐진 doc 함정 5 회(누적 18~22)★** — `-ThreadCpu` 의 doc 이 자기 파라미터보다
**103 줄 위**에 있다. `[switch] $KeepRustLog,` 에는 **자기 doc 이 없고** 위 블록이
`-NoLog`(유지) 것이다.

게이트: 두 ps1 AST 파싱 0 errors, BOM `ef bb bf` 보존 / 파라미터 75 → 55 이고
집합 차이가 의도한 20 개와 정확히 일치 / 제거된 파라미터의 live 참조 0 /
`config_surface` 20 passed.

### Step 18 — `ports/servoshell` 타이밍 계층 `0e4c94e913e` (78 줄, 3 파일)

`window.rs` 676 → 638, `gui.rs` 851 → 848, `headed_window.rs` 1855 → 1822.

**★계획서가 지시한 `231-284` 는 삭제 범위가 아니라 페인트 본문 전체다★** —
`webview.paint_target()/paint()`, `flush_deferred_dcomp_commits()`,
`rendering_context.present()` 가 그 안에 있다. 글자 그대로 따랐으면 셸의 재페인트
경로에서 페인트·DComp 플러시·present 가 통째로 사라진다.

**★합쳐진 doc 함정 23 회차★** `window.rs:237-244` 의 여덟 줄 주석은 아래 계측이
아니라 **위** 줄(245)의 `flush_deferred_dcomp_commits()` 를 설명하는, 그 호출의
저장소 내 유일한 문서다. 함께 지웠으면 **컴파일 에러 없이** 셸에 픽셀 회귀가 남는다.

**★계획서에 `headed_window.rs` 가 아예 없다★** — `Gui::paint` 의 유일한 호출자이자
그 `Duration` 의 유일한 소비자다. 계획서가 적은 `gui.rs` 조각보다 크고, 그것 없이는
컴파일되지 않는다. 경로도 틀렸다(`desktop/window.rs` 가 아니라 `window.rs`).

**★`desktop/app.rs:72`·`:345` 는 지우지 않았다★** — `git blame` 상 둘 다 업스트림
(2024-12-19)이고 업스트림 매크로 `trace_winit_event!` 만 먹인다. 같은 단계가 out of
scope 로 선언한 `PerfettoLayer` 와 같은 범주다. 1 절이 범위를 "포크가 추가한 수단" 으로
한정한다.

게이트: `cargo check -p servoshell` exit 0(경고 줄 자체가 안 난다) / winit_wall
check exit 0 / `servo-paint` 93 / `config_surface` 20.

### Step 19 — `shared/paint` POC 예제 3 개 `854f80351f5` (2,102 줄)

`d3d11_dyn_poc.rs` 618 + `dcomp_native_poc.rs` 584 + `dcomp_partial_present_poc.rs` 846
= **2,048 — 계획서 수치가 정확히 맞은 드문 경우다.** `Cargo.toml` 108 → 54.

**계획서 정정 둘** — ★`[[example]]` 선언은 애초에 없다★(별표까지 달아 경고한 함정에
해당 사항이 없었다). `:56-108` 은 winapi 블록이 아니다 — `:62` 가 `env_logger` 다.

**★계획서의 게이트로는 이 제거가 검증되지 않는다★** — resolver = "2" 에서 dev-target 을
안 지으면 dev-dependency 피처는 통합되지 않는다.
`cargo check -p servo-paint-api --all-targets --features no-wgl` 를 추가했고 그것이
exit 0 인 것이 실제 증명이다.

### Step 20 — `vendor_local/webrender` 계측 제거 (패치는 유지)

**★계획서의 전제가 깨졌다 — 사용자 판단으로 방향을 바꿨다★**
1b 절은 "upstream 으로 되돌린다" 였으나, crates.io 0.68.0 원본과 전수 비교한 결과
이 사본은 계측 전용이 아니라 **동작 변경 20 줄**을 함께 싣고 있었다:

- `tile_cache.rs` 의 `picture_caching_disabled()` (`WR_DISABLE_PICTURE_CACHING`)
- `picture.rs` 의 multi-GPU video-wall workaround — picture caching 이 꺼졌을 때
  O(prim × quadtree) 의존성 비교를 건너뛰고 타일을 통째로 무효화한다
  (2026-07-13 WR native compositor 설계 §1 의 그 비용)

★이 둘은 **초기 vendoring 커밋이 끌고 들어온 것**이라 이후 커밋 아홉 개를 아무리 읽어도
안 보인다★ — 계획서가 지시한 "커밋 9 개를 읽어 확인한다" 로는 못 잡는다. 잡은 방법은
레지스트리 캐시의 `.crate` 를 풀어 **트리 전체를 원본과 대조**한 것이다.

사용자 결정: **계측만 걷고 패치 유지.**

**방법 — 범위 삭제가 아니라 원본 복원.** 세 파일에서 upstream 줄이 바뀐 것은 각각
1/1/0 개이고 나머지는 전부 추가분임을 먼저 확인한 뒤, `renderer/mod.rs` 와
`spatial_tree.rs` 는 upstream 원본 그대로 덮고 `tile_cache.rs` 는 원본 + helper 여덟 줄로
재구성했다. **손편집이 0 이므로 증명이 `diff -r` 한 번으로 끝난다** — 복원 후 트리 전체
대조에서 차이는 `picture.rs` 와 `tile_cache.rs` 둘뿐이고, 그 둘이 정확히 유지 대상이다.

루트 `Cargo.toml` 의 patch 주석도 고쳤다. "느린 프레임에 한 줄 더한 계측 사본, 원인이
잡히면 되돌린다" 는 이제 사실이 아니다. ★`Cargo.lock` 은 바뀌지 않는다★ — 패치 경로를
그대로 두었으므로 스테이징 금지 목록과 충돌할 일도 없다.

게이트: cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release ★exit 0, 11m 36s 풀빌드★. webrender 가 vendor_local 에서 컴파일됐고, ★직전까지 모든 빌드 로그에 나오던 wall_prepare_gpu_cache_ms / wall_pre_draw_ms / wall_texture_cache_ms / wall_native_surfaces_ms 의 value

---

## Steps 8~11 재검증 `78c392d4fbc`

Step 5 실행과 **병렬로** 읽기 전용 재검증(파일 집합이 겹치지 않음). 계획서 8 절에 고정.
**오류 28 · 하중 함정 12 · 누락 계측 14.** 가장 중요한 것:

- ★`winit_wall/main.rs:1727`★ `dispatched[index] = true;` 가 계측 `skipped += 1`(1726)과
  하중 `continue;`(1728) **사이**에 끼어 있다. 떨구면 barrier 가 건너뛴 타일을 두 번째
  루프가 전부 다시 칠한다. **컴파일 에러 없음.** 이 제거 전체에서 가장 위험한 한 줄.
- ★`layout/display_list/mod.rs:560`★ 이 계획서가 "유지" 라고 한 바로 그 `return false;` 다.
- ★`d3d11_ring` 의 `lock_registry` 는 존재하지 않는다★ — 셋째 헬퍼는 `fn lock`(:504).
  놓쳐서 `.unwrap()` 으로 바뀌면 패닉 스레드 하나가 `CONSUMER_DEVICE` 를 오염시켜
  **모든 타일의 비디오가 영구 정지**한다. 교체 대상 28 곳.
- ★`last_pacing_tick_at` 래치를 어느 테스트도 덮지 않는다★ — 계획서가 가드라고 지목한
  테스트는 `needs_pacing_backstop(bool, bool)` 자유 함수만 부른다. 망쳐도 게이트는 초록.
- `note_rendering_update_reason` 호출처 **11**/9, `rendering_update_from_timer` **5**/3,
  `script_thread` import 는 삭제가 아니라 **트림**, 계획서의 `MEDILOCKDEFER` 는 **오타**.

---

## 방법 (단계마다 동일)

1. 편집 전 하중 심볼 `grep -c`(주석 제외) 기준선.
2. **심볼 단위 삭제.** 범위가 불가피하면 파이썬 스크립트가 각 범위의 **첫 줄·끝 줄 내용**,
   **범위 간 비중첩**, **반드시 살아야 할 줄 목록이 범위 밖**임을 전부 검증한 뒤
   아래에서 위로 삭제. 어서션이 터지면 **범위를 고치지 어서션을 고치지 않는다.**
3. 편집 후 카운트 재측정 — 하중은 전부 불변이어야 한다.
4. 삭제로 인자가 고아가 되면 `_name` 또는 `let _ = ...`, 부수효과가 하중이면 호출은 보존.
5. 단계마다 커밋 하나. `git add -A` 금지, 금지 목록 파일은 절대 스테이징하지 않는다.

**어서션이 실제로 잡은 것**: Step 2 에서 범위 계산이 두 번 어긋났고(끝줄, 빈 줄),
Step 3 에서 두 번, Step 4 에서 열한 번 터졌다. 전부 삭제 전에 잡혀 파일은 무손상.


---

## 남은 것 / 알려진 빚 (2026-10-28, 20/20 완료 시점)

### 사용자가 처리할 것

- ★`Cargo.lock`★ — Step 19 가 dev-dependency 를 지우면서 `env_logger` 줄 하나가
  빠져야 한다. 스테이징 금지 목록이라 커밋에 넣지 않았다. (같은 파일의 `time` 추가
  한 줄은 이 작업과 무관한 기존 변경이다.)
- ★`etc/multigpu/check_freeze_log.ps1`★ — **미추적 파일**이라 지우지 않았다.
  `WALLTICKRATE`/`FRAMEREASON`/`ANIMSTEP` 을 읽는데 셋 다 없어졌으므로 죽은 도구다.
- ★`tests/html/ticker_slice_probe.html` 이 미추적인데 `make_wall_dist.ps1:195` 가
  `-EA SilentlyContinue` 로 복사한다★ — 육안 프로브 넷 중 하나인데, 깨끗한
  체크아웃에서 패키징하면 **조용히 빠진다.** 추적에 넣든 복사를 빼든 결정이 필요하다.

### 일부러 남긴 것

- `ports/servoshell/desktop/app.rs:72`·`:345` 와 `lib.rs:67` 의 `PerfettoLayer` —
  업스트림 tracing 통합이다. 지우려면 셋을 함께, 별건으로.
- `docs/multigpu/` 의 조사 기록 다섯 편이 삭제된 런처 스위치 이름을 인용한다
  (`multigpu_tile_scaling_diagnosis.md`, `capture_card_shared_connection.md`,
  `wall_animation_end_black_frame.md`, `wall_output_animation_frontend_handoff.md`).
  전부 "그때 이렇게 돌렸다" 는 과거 기록이지 지시가 아니라 history 로 둔다.
- 삭제된 POC 예제를 가리키는 doc 참조 넷(`dcomp_compositor.rs:10`,
  `shared/paint/rendering_context.rs:132`, `third_party/surfman/.../surface.rs:55`,
  `config/tests/config_surface.rs:40`). 설계 근거의 출처 표기다.
- `vendor_local/webrender` 자체 — 20 줄짜리 포크로 남았다. 자세한 것은 Step 20.

### 우리 잔재가 **아닌** 경고 (확인 완료, 고치지 말 것)

릴리스 빌드에 남는 경고는 전부 손 안 댄 것이다.

| 경고 | 왜 우리 것이 아닌가 |
|---|---|
| `make_current_if_needed_mut` never used | `webxr.rs:306` 이 쓴다. `webxr` 피처가 꺼져 있을 뿐이다 |
| `gamepad_task_source` / `GlobalScope` unused | 같은 cfg 블록. gamepad 쪽이 꺼져 있다 |
| `GenericSender` unused | `servo.rs:1249` 가 쓴다. bluetooth 가 꺼져 있다 |
| `Trusted` import unused (`webglrenderingcontext.rs:62`) | ★제거 **이전부터** 안 쓰였다★ — `619f93b2494^` 에서도 import 와 주석뿐이다 |
| `third_party/surfman` 9 건, `AngleGlGuard`, `servo-canvas-traits` | 손 안 댄 코드 |

### 다음 할 일 (제거와 무관, 원래 하던 추적)

★`-PacingOutput` 핀 스윕이 아직 안 돌았다.★ 2026-10-26 커밋 `d3536e6092e` 가
"핀으로 인과를 먼저 가른다 — 한 세션에 출력마다 한 번씩 돌리면 결판난다" 를 지시해
두었고 운영자도 `log_fullthrottle_arch/01·06·07` 에 같은 말을 적었는데, run 11 은
`pacing_output=primary` 로 돌아 그 지시를 실행하지 않았다. 스위치는 유지했다.

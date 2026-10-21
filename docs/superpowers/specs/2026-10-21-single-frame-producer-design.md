# 프레임 생산자 일원화 설계

**날짜**: 2026-10-21
**대상**: `components/paint`, `components/servo/examples/winit_wall`
**상태**: 설계 확정, 구현 대기
**개정**: 초고를 규칙 준수·불변식 감사에 걸어 전면 개정(2026-10-21). 초고의 5 절 범위
결정이 규칙 1 위반이었고, 사실 오류 8 건과 불변식 6 칸의 미답이 있었다. 그 지적을 반영한
판이다.

---

## 0. 먼저 — 이 문서가 서 있지 **않은** 근거

발단은 "비디오 재생 중 회전 애니메이션이 30Hz 로 깎인다" 였다. ★그 진단은 틀렸다.★

1. **디스플레이 리스트는 합성 요청과 별개 트랜잭션으로 나간다.** `PaintMessage::SendDisplayList`
   는 `GenerateFrame` 과 다른 메시지이고(`components/shared/paint/lib.rs:232`, `:480`),
   `Painter::handle_new_display_list`(`painter.rs:3669`)가 자기 트랜잭션으로
   `set_display_list` 후 즉시 보낸다(`painter.rs:3761-3770`). ★비디오가 낸 합성도 스크립트가
   방금 올린 최신 장면을 집어 간다.★
2. **`FRAMEID distinct_ids` 는 내용의 새로움을 세지 않는다.** 벽 논리 프레임 번호는 벽 프레임
   요청으로 생긴 프레임에만 붙고(`painter.rs:3435-3438`), `last_ready_wall_logical_frame_id`
   는 한 번 붙으면 지워지지 않는다. `output_grid.rs:292-305` 의 주석이 이미 그렇게 적고
   있다. 0.54 는 ★배리어 적용률★ 이다.
3. **배리어가 안 걸리는 자리는 `paint.rs:2002` 다.** `if generated_painter_ids.len() > 1`
   — 요청 대상 수가 아니라 ★실제로 생성한★ painter 수를 본다. 그래서 4 타일 벽에서 3 타일이
   거절하면 다중 타일인데도 배리어가 안 걸린다. ★이 사실이 2-5 와 8 절을 지배한다.★
   (초고는 `paint.rs:1774` 를 인용했는데 그 자리는 배리어가 아니라 페이서의 통과 판정이다.)

★그래서 이 설계는 "회전 저더가 고쳐진다" 를 약속하지 않는다.★ 저더의 원인은 미특정이고,
스크립트가 인라인 스타일로 미는 변환의 화면 케이던스를 재는 수단이 아직 없다
(`docs/multigpu/wall_output_animation_cadence.md` §6-c).

근거는 구조 자체다. 코드 자신이 그 주장을 하고 있다(`painter.rs:1541-1557`):

> 애니메이션이 끝나는 순간 **생산자가 갈린다.** 그 갈아타는 자리가 화면에서 한 박자 끊김으로
> 보였다. ★프레임을 내는 주체가 바뀌지 않으면 경계도 없다.★

---

## 1. 구속 규칙

사용자 확정(2026-10-21). ★이 셋을 어기는 설계는 그 자리에서 틀렸다.★

1. **프레임 생산자는 단일하다.** 벽 전체에 하나. 주기마다 벽 프레임 번호 하나를 내고 전
   타일에 팬아웃해 배리어로 묶는다.
2. **프레임의 구성 요소는 그 프레임의 생산 트리거가 될 수 없다.** 비디오 도착, 애니메이션
   값, 스크립트의 새 디스플레이 리스트 — 어느 것도 합성을 요청하지 않는다.
3. **생산은 구성 요소를 가져다 렌더할 뿐이고, 구성 요소 생산자는 정해진 자리에 자기 값만
   업데이트한다.** 생산자는 프레임에 무엇이 들었는지 묻지 않는다.

### 지금이 왜 셋을 다 어기나

`generate_frame*` 호출 지점이 ★11 군데★ 다(주석들은 아직 "아홉" 이라고 적고 있다 —
`painter.rs:2733`, `:2778`): 1565, **2146**, 2854, 3060, 3119, 3141, 3592, 3635, 3826,
3957, 4086. `display_composite_in_flight` 를 세우는 것은 다섯(1567, 2147, 3832, 3960,
4090). 그리고 WebRender 프레임이 아닌 ★12 번째 경로★ — `present_external_only`(4046)는
승격된 외부 서피스를 DComp 에 직접 커밋하고 어떤 계수기에도 안 잡힌다.

이들이 `display_composite_in_flight` 하나를 두고 경합하고 다섯 휴리스틱이 중재한다.
★다섯 다 "누가 낼 차례인가" 를 다투느라 생긴 것이고, 생산자가 하나면 다툴 일이 없다.★

---

## 2. 목표 구조

### 2-1. 생산자는 이미 있다 — 부르는 사람만 틀렸다

`Paint::issue_wall_frame_request`(`paint.rs:1967`)가 규칙 1 그 자체다: 벽 프레임 번호 하나,
전 타일 팬아웃, 배리어 등록. ★단 배리어 등록은 조건부다★ — `paint.rs:2002` 의
`if generated_painter_ids.len() > 1` 안에 있고, 팬아웃 루프 본문은
`self.with_painter_mut(*painter_id, |p| p.generate_frame_for_script(..))`(`paint.rs:1992-1994`)
이다. 그 줄이 6 절의 미확인 항목이다.

문제는 이 함수가 ★스크립트가 요청할 때만★ 불린다는 것 하나다.

### 2-2. 부르는 사람은 틱이다

```
tick edge:                                   ← 유일한 생산자. 내용물을 묻지 않는다.
    note_frame_time(now)                     이 프레임의 시각
    note_present_tick()                      커밋 기준점
    fire_refresh_driver_callbacks()          rAF 틱 — 렌더보다 먼저(불변식 9)
    gather_components()                      ★독립 단계★ — 적재된 값을 모은다(불변식 8)
    produce_wall_frame()                     벽 번호 하나 · 전 타일 · 배리어
    render
```

`gather_components()` 를 독립된 줄로 둔다. 규칙 3 의 "구성 요소를 가져다" 에 해당하는
유일한 단계이고, `produce_wall_frame()` 안에 묻으면 재배치가 가능해진다 — 불변식 9 가
그 순서가 틀렸을 때의 실측(요청 120 / 폐기 60)을 걸어 두고 있다.

### 2-3. 자유 실행, 그리고 ★dirty 비트는 만들지 않는다★

사용자 결정: 콘텐츠 판정을 하지 않는다. 매 틱 에지에 낸다.

따라서 ★"바뀌었다" 를 기록하는 비트를 두지 않는다.★ 소비자 없는 상태 비트는 반드시 나중에
소비자를 얻고, 그 소비자는 십중팔구 "안 바뀌었으면 거르자"(규칙 3 위반)다. 격하된 경로는
`set_pending_frame` 을 비롯해 ★생산 회계에 일절 쓰지 않는다★(2-6).

### 2-4. 틱 에지가 생기는 방식만 임베더마다 다르다 — ★생산자 수는 어디서나 하나★

초고는 "클럭을 알린 셸에서만 새 구조" 라고 썼는데 그것이 규칙 1 위반이었다. 기본 winit_wall
빌드(`gfx_wall_pacing_thread` 기본 `false`, `prefs.rs:1280`)와 servoshell `--wall-all-tiles`
가 구 구조에 남기 때문이다.

★"수요 구동" 과 "생산자가 여럿" 은 별개다.★ 분리한다:

| 구성 | 틱 에지의 출처 | 생산자 |
|---|---|---|
| winit_wall 페이싱 on | 페이싱 스레드 | 틱 1 개 |
| winit_wall 페이싱 off(기본, `FrameClock::Timer`) | `PaintTimerThread` | 틱 1 개 |
| winit_wall vsync | DwmFlush 스레드 | 틱 1 개 |
| servoshell / WPT / Android | ★깨우기가 다음 틱 에지에 생산 1 회를 예약★ | 틱 1 개 |

수요 구동 임베더에서 깨우기는 ★프레임을 내지 않는다★ — 다음 에지에 벽 생산 1 회를
예약할 뿐이다. 그래서 규칙 1·2 가 어디서나 성립하고, 조용한 페이지는 깨우기가 없으므로
생산도 없어 불변식 16 의 조용한 창이 생긴다. ★구 구조로 되돌아가는 런타임 전환은 없다★ —
되돌아갈 다중 생산자 레짐 자체를 남기지 않으므로 초고의 staleness 폴백 조항은 삭제한다.

구성별로 콜백을 누가 쏘는지가 달랐다 -- `pacing_fire` 는 `FrameClock::Pacing` 일 때만
`Some` 이었고 나머지 둘은 `PaintTimerThread` 나 DwmFlush 스레드가 자기 클럭으로 쐈다.
모으지 않으면 "콜백 → 생산 → 렌더" 가 한 틱 안의 순서가 아니라 두 클럭의 경합이 되고,
그것이 8 절이 실패로 세는 "두 시계 맥놀이" 다.

★2026-10-21 `9b6cd8dfcf3` 에서 셋 다 틱이 쏘도록 모았다(계획 Task 3).★ 그때 드러난 것이
하나 있다 -- 콜백을 틱으로 옮기면 `PaintTimerThread` 가 프레임 경로에서 빠지고, 그러면
***기본 구성(`FrameClock::Timer`)에서 이벤트 루프를 깨울 것이 하나도 없어진다***. rAF 만
도는 페이지가 영구 정지한다. 그래서 `embedder_traits::RefreshDriver` 에 기본값 거짓인
`needs_paint_timer_heartbeat` 를 두고, 참인 구성에서만 같은 타이머 스레드가 같은 주기·같은
절대 격자로 ★깨우기만★ 낸다. ***틱이 언제 나는지는 바뀌지 않는다*** -- 깨우기는 생산이
아니다(규칙 2).

★`gfx_wall_direct_render` 기본값도 `false` 다★(`prefs.rs:1279`). 그 경로에서
`fire_present_tick` 은 `render_all_tiles` 를 부르지 않고 `request_redraw` 만 한다
(`main.rs:1422-1429`). 즉 ★"틱 = 렌더 패스" 는 `direct_render=true` 전용★ 이다. 생산은
틱에 붙고 렌더는 뒤따라올 수 있다 — 2-2 의 순서는 "생산이 그 틱의 렌더보다 앞" 까지만
보장하며, 렌더가 같은 호출인지는 구성에 달렸다. 복구 경로(2-6)는 그래서 렌더가 아니라
★틱★ 에 붙인다.

### 2-5. 역압은 ★벽 단위 결정★ 이다

`renderer_behind()` 가 참인 타일을 조용히 빼면 그 주기의 벽 프레임이 전 타일을 덮지 않고,
화면에 프레임 N 인 타일과 N-1 인 타일이 섞인다 — 규칙 1 의 목적이 바로 거기서 깨진다.
게다가 `paint.rs:2002` 때문에 거절이 몇 개만 생겨도 ★배리어 자체가 안 걸린다★.

**결정**: 거절한 타일은 그 논리 프레임에 ★keep-previous 로 배리어에 등록★ 한다. 벽 프레임은
언제나 전 타일을 덮는다. "거른다" 가 아니라 "이번 벽 프레임에 keep-previous 로 참여한다" 다.
그러면 `generated_painter_ids` 에서 조용히 빠지는 구현이 원천 봉쇄된다.

그리고 ★`renderer_behind()` 는 전 타일에 매 주기 호출된다★ — 2 초 강제 해제가 거절
경로에서만 돌기 때문이다(`painter.rs:3219-3222`, 실측 "4 개 중 2 개 타일 44 초 사망").

★진입점을 바꿔야 한다.★ 지금의 `generate_frame_for_script` 는 `!needs_new_frame()` 에서
`renderer_behind()` 에 ★닿기 전에★ 빠진다(`painter.rs:3780-3782`). 캔버스 이미지를 하나라도
기다리는 painter 에서는 틱이 매 주기 불려도 2 초 탈출구가 돌지 않는다. 그래서 틱 전용
진입점 `generate_frame_for_wall_tick` 을 만들고 순서를 못박는다:

```
generate_frame_for_wall_tick(logical_frame_id, requested_at) -> TileOutcome
    1. renderer_behind()            ← ★무조건 먼저.★ liveness 프로브이자 역압 판정
       참이면 TileOutcome::KeepPrevious 로 돌아간다
    2. 캔버스 이미지 대기 중이면      TileOutcome::KeepPrevious
    3. 생성                          TileOutcome::Generated
```

이름도 바꾼다 — `..._for_script` 는 "이 프레임은 스크립트 것" 이라는 낡은 소유권 관념을
이름에 남긴다.

### 2-6. 나머지 경로의 처분 — 11 개 생산 지점 전부

| 생산 지점 | 지금 | 바뀐 뒤 |
|---|---|---|
| 3826 `generate_frame_for_script` | 스크립트 요청 → 벽 프레임 | ★삭제★. 스크립트는 디스플레이 리스트만 올린다. 생산 회계에 쓰지 않는다 |
| 1565 `pump_paint_animation` 비디오 분기 | 비디오 있으면 합성 | ★삭제★. 깨우기 동반자(`:1571-1577`)는 ★다음 틱 에지 예약으로만★ 한정(2-4) |
| 4086 `video_composite_owed` 도착 경로 | 도착 시 합성 | ★삭제★. `pending_video_frame_updates` 적재만 |
| 2854 `flush_deferred_animation_frame` | 턴 끝에 합성 | ★삭제★. 값만 푸시. `painter_busy` 역압과 "값 먼저" 순서는 틱으로 이설 |
| **2146 `flush_owed_canvas_ack`** | ack 해제 + 합성 | ★분리★ — ack 해제만 남기고 합성은 뗀다. 2-7 |
| 3957 `update_images` 의 빚 상환 | 조건부 합성 | ★삭제★. ack 해제는 2-7 로 |
| 3060 `send_root_pipeline_display_list` | 루트 파이프라인 재구성 | ★삭제★(합성만). 트랜잭션은 그대로 보낸다 |
| 3119 줌/스크롤 | 합성 | ★삭제★(합성만) |
| 3141 `toggle_webrender_debug` | 디버그 토글 합성 | ★삭제★(합성만). 다음 틱이 반영한다 |
| 3635 `scroll_node_by_delta` | 합성 | ★삭제★(합성만) |
| 3592 `send_initial_pipeline_transaction` | 부트스트랩 | ★예외 — 7 절★ |
| (12 번째) 4046 `present_external_only` | 자체 14ms 스로틀로 DComp 직접 커밋 | ★틱 소유로 이관 — 7 절★ |

"합성만 삭제" 는 그 자리의 트랜잭션(디스플레이 리스트, 스크롤 오프셋, 리소스 갱신)은 그대로
보내고 `generate_frame` 만 빼라는 뜻이다. 적재가 안전한 근거는 이미 보장돼 있다 —
`generate_frame_with_diagnostic_id` 가 모든 호출에서
`flush_pending_video_frame_updates(transaction)` 를 무조건 부른다(`painter.rs:2802`).

★생산 회계의 유일한 writer 는 틱의 생산이다.★ `pending_frames` 와 `anim_ready_queue` 에
쓰는 곳은 공유 에필로그 하나뿐이고(`painter.rs:2814-2821`), 격하된 경로는 자기 슬롯만
건드린다 — 비디오는 `pending_video_frame_updates`, 페인트측 애니메이션은 값 스택, 스크립트는
디스플레이 리스트. ★`set_pending_frame` 호출은 전부 사라진다.★

### 2-7. 캔버스 ack — 해제 지점 넷을 각각 정한다

불변식 1 의 문장을 코드에 맞게 좁힌다: ★영구 거절 경로만★ 해제 의무를 진다. `renderer_behind`
는 일시적이라 안 풀어도 된다는 것이 코드의 명시적 근거다(`painter.rs:3802-3805`).

| 해제 지점 | 처분 |
|---|---|
| `generate_frame_for_script` 성공(`painter.rs:3835-3841`) | 틱의 생산으로 이관 |
| 같은 함수 `rides_the_painter` 경로(`:3809-3814`) | 함수와 함께 삭제 |
| `update_images`(`:3962-3968`) | 틱으로 이관 |
| `flush_owed_canvas_ack`(`:2149-2155`) | ★남긴다 — 단 합성은 뗀다★ |

**규칙**: ★틱마다, 타일마다 — 그 타일이 이번 틱에 합성했든 안 했든 — 빚이 있고
`pending_canvas_images` 가 비었으면 ack 를 해제한다.★ 해제가 생산과 분리되므로 "모든 스크립트
요청이 거절이 되어 ack 가 영영 안 풀린다"(117 초 스톨)가 구조적으로 불가능해진다. 그리고
`pending_canvas_images` 조건을 유지하므로 FrameDelayer 계약(캔버스가 올라간 뒤에 rAF)도
그대로다 — ★진입점 무조건 해제는 택하지 않는다★.

복구 자리가 렌더가 아니라 틱에 붙으므로, `direct_render=false` 에서 렌더가 밀려도 복구는
매 틱 닿는다(2-4). keep-previous 타일이 렌더를 못 받는 구멍(불변식 5)도 이것으로 닫힌다.

### 2-8. 스크린샷

`has_pending_frames()` 는 `pending_frames != 0 || frame_delayer.pending_frame`
(`painter.rs:1982-1984`)이고 스크린샷 두 경로가 그것으로 즉시 빠진다
(`screenshot.rs:140`, `:155`). 매 틱 생산하면 ★벽 자신의 `--capture` 도 영영 안 찍힌다.★
범위 결정으로는 안 걸러진다.

**결정**: ★스크린샷 요청이 떠 있는 동안 틱은 생산을 쉰다.★ 요청이 소진되면 재개한다. 이것은
프레임 ***내용*** 을 묻는 것이 아니라 외부 모드이므로 규칙 3 을 어기지 않는다. 1 회 로그와
계수기를 붙이고, 8 절에 `winit_wall --capture` 를 명시한다.

---

## 3. 불변식

정찰이 코드 주석과 실측에서 확정한 17 개. ★게이트를 지우려면 그것이 막던 증상이 새 구조에서
왜 불가능한지를 답해야 한다.★

| # | 불변식 | 새 구조에서 |
|---|---|---|
| 1 | ★영구★ 거절 경로는 캔버스 ack 를 해제해야 한다 | ★2-7★ — 해제를 생산에서 떼어 틱마다 조건부로 돌린다. 네 지점 각각 처분됨 |
| 2 | `display_composite_in_flight` 강제 해제는 계속 묻는 호출자의 거절 경로에서 | ★2-5★ — 틱 전용 진입점이 `renderer_behind()` 를 ***무조건 먼저*** 부른다. 단일 writer·에지 타임스탬프 규칙(`:3244-3258`) 그대로 |
| 3 | 모든 합성이 비디오 스태시를 flush, 도착은 적재 | 공유 에필로그가 보장. 빈 트랜잭션 생략(`:4097-4104`)도 유지 |
| 4 | 비디오는 도착당 하나도 아예 없음도 아닌 페인트 박자 | 틱이 천장과 바닥을 동시에 준다. ★2-4 로 모든 임베더에서★ — 깨우기가 에지를 예약하므로 "틱이 없는 곳" 이 없다. ★2026-10-22 확정된 구멍 둘★: (a) 외부 서피스 fast-path 에는 바닥이 없다 — 7 절의 인용 블록, (b) 캔버스 대기 타일은 ***틱 전체를*** 거절하므로 승격되지 않은 비디오의 바닥도 같이 빠진다 — `wall_tick::tile_may_generate` 주석 |
| 5 | ack 복구는 생산이 멎어도 도는 자리에서 | ★2-7★ — 렌더가 아니라 틱에 붙인다. `direct_render=false` 와 keep-previous 타일 모두 커버 |
| 6 | `pending_frames` ↔ `anim_ready_queue` 1:1 | 공유 에필로그가 ***무조건*** 둘 다 올린다(`:2814-2821`) — 초고가 "토큰 없이 올리는 경로가 있다" 고 쓴 것은 틀렸다. ★2-6 이 writer 를 틱 하나로 못박는다★ |
| 7 | 값 표본은 렌더 클럭이 몰고 패스당 한 번, 타이머 폴백은 렌더 정지 전용 | 표본은 `pump_paint_animation` 자리에 둔다. 틱이 더는 그 자리에서 ***생성하지 않으므로*** `painter_busy` 억제가 표본을 막지 않는다 — `:2415-2435` 의 15.2fps 회귀는 생성과 묶였을 때의 것이다 |
| 8 | 한 트랜잭션이 모든 동적 속성 + 끝난 애니메이션 값 재첨부 | `gather_components()` 가 독립 단계로 합집합을 합성 ***전에*** 모은다(2-2) |
| 9 | 콜백은 렌더보다 먼저, 값은 합성보다 먼저 | 2-2 의 순서. ★세 구성의 콜백 발사를 2026-10-21 `9b6cd8dfcf3` 에서 틱으로 모았다★(2-4) |
| 10 | 표출은 콘텐츠 완료 시점의 함수가 아니다 | 벽 그대로. 수요 구동 임베더에서도 깨우기는 ***에지를 예약할 뿐*** 이므로 표출이 콘텐츠 함수가 되지 않는다 |
| 11 | 놓친 틱은 버리고, 주기는 정확히 | 기존 클럭 그대로. ★정리★: `gfx_refresh_hz` 클램프 사본이 다섯이다 — `refresh_driver.rs:353-363`, `main.rs:1941-1947`, `main.rs:1961-1964`, `script/raf_clock.rs:45-53`, `script_thread.rs:1651` |
| 12 | 틱 솎기와 합성률은 분리 | divisor 가 나누는 것은 ★표출 틱이 아니라 `RefreshDriver` 틱★ 이다(`refresh_driver.rs:230-234`). 둘이 같은 주기인 것은 `FrameClock::Pacing` 뿐. 2-4 가 콜백을 한 손잡이로 모으면 둘이 같아진다 — ★그러면 8 절의 `epoch/s` 기대값은 `gfx_refresh_hz`/divisor 다★ |
| 13 | WebView 소유 painter 만 틱 요청 | ★문구가 코드와 다르다 — 2026-10-22 정정★. `Paint::wall_frame_request_for_all_tiles` 의 원천 집합은 `self.painter_ids()`, 즉 ***등록된 모든 painter*** 다. "WebView 소유 painter 만" 은 아니다. 지금 있는 셸에서는 둘이 같은 집합이라 무해하고(추가 paint target 은 전부 어느 `WebView` 의 것이다), 소유자 없는 painter 가 생기면 그때 좁히면 된다. ★팬아웃이 있다고 페이싱 틱이 있는 것은 아니다★ |
| 14 | keep-previous 는 지명된 논리 프레임에만 유효 | ★2-5 가 이것을 적극적으로 쓴다★ — 역압 거절이 곧 그 프레임의 keep-previous 참여다 |
| 15 | 배리어 16ms 마감은 주사율에서 유도하지 않은 허용 오차 | 틱 주기와 묶지 않는다 |
| 16 | 스크린샷이 가능해야 한다 | ★2-8★ — 수요 구동 임베더는 깨우기 없으면 생산 없음, 벽은 요청 중 생산 일시 중지 |
| 17 | 캐럿은 진짜 500ms 타이머, 값 표본은 표출과 같은 클럭 | 그대로. ★스레드 수로 클럭 일원화를 확인하지 말 것★ — `PaintTimerThread` 는 어차피 뜬다 |

---

## 4. 게이트 처분

| 게이트 | 처분 | 근거 |
|---|---|---|
| `script_frame_is_redundant` / `script_frame_rides_the_painter` | ★술어 삭제★ | "이 프레임의 내용이 중복인가" 는 규칙 3 이 금지한 질문이다. 후신을 남기지 않는다. 그 함수가 하던 ★ack 해제 부작용만★ 2-7 로 이설 |
| `raf_is_driving_composites` / `raf_is_producing_frames` | ★삭제★ | 답이 상수 참이 된다. `should_fast_present`(`dcomp_compositor.rs:151-177`)는 틱에서 직접 받는다. ★시간 창을 다시 도입하지 말 것★ |
| `deferred_animation_frame_is_due` / `flush_deferred_animation_frame` | ★삭제★ | 이 설계의 배아. 역압과 "값 먼저" 순서는 틱으로 이설 |
| `pump_paint_animation` 비디오 분기 | ★삭제★ | 규칙 2 위반 그 자체 |
| `video_composite_due` / `video_composite_owed` | ★삭제★ | 도착은 순수 적재. ★주의★: `gfx_video_immediate_composite_enabled` 의 유일한 런타임 사용처가 이 블록(`painter.rs:4066`)이라 ★8 절의 A/B 대조군이 함께 사라진다★ — 대조군을 바꿔야 한다(8 절) |
| `renderer_behind()` / `display_composite_in_flight` | ★유지★ | liveness 역할은 대체 불가. 2-5 가 호출 순서를 못박는다 |
| `FrameDelayer::needs_new_frame` + 캔버스 ack | ★유지★ | 2-7 |
| `gfx_paint_side_animation_tick_divisor` | ★유지★ | 불변식 12 |
| 벽 페이서 Active/Pending/TooSoon 합류 | ★계수기를 붙여 남긴다★ | 프레임을 받는 두 번째 입구가 살아 있으므로 ★전용 `FRAMEREASON` 값을 주고 벽 구성에서 0 이 아니면 실패★(8 절) |
| `last_frame_by_other_source_at` 롤백 3 곳(`:1564-1566`, `:2853-2855`, `:4085-4087`) | ★한꺼번에 삭제★ | 하나라도 남기면 측정된 진동 재현(1 초 페이드에 렌더 61 → 72) |

---

## 5. 범위

★새 구조는 모든 임베더에 적용된다.★ 생산자는 어디서나 하나다. 다른 것은 틱 에지의 출처뿐이고
그 표가 2-4 에 있다. 반드시 계속 동작해야 하는 구성: winit_wall 페이싱 on / 페이싱
off(기본) / vsync / servoshell `--wall-all-tiles` / WPT(servoshell headless).

(참고: `RefreshDriver` 구현은 ★다섯★ 이다 — `refresh_driver.rs:367`,
`winit_wall/tick_refresh_driver.rs`(구 `pacing_refresh_driver.rs`), `winit_wall/vsync_refresh_driver.rs:92`,
`servoshell/desktop/vsync_refresh_driver.rs:92`, `servoshell/egl/app.rs:269`. servoshell
데스크톱에도 DwmVsync 드라이버가 있고 `gfx_vsync_enabled` 에서 설치된다.)

---

## 6. 미확인으로 남기는 것

- ★`with_painter_mut`(`paint.rs:1992-1994`)의 블로킹★ — 스레드 타일에서 그 타일의 현재 렌더
  뒤에 막힐 수 있다. 사용자 지시로 미확인으로 두고 구현 1 단계에서 계측으로 답한다.
  ★측정 결과는 틱이 painter 에 닿는 *방식*(블로킹 호출 대 비블로킹 포스트)만 바꿀 수 있고,
  생산자의 수와 단일성은 바꾸지 않는다.★
- 캔버스 ack 빚이 painter 별인지 벽 프레임별인지(빚은 전 painter 에 퍼지는데 ack 하나가
  문서를 푼다, `paint.rs:2438-2451`)

---

## 7. 두 예외 — 새 구조에서 다시 정당화한다

- **`present_external_only`(`painter.rs:4046`)** — 초고는 "그 게이트를 그렇게 만든 이유가
  재설계 뒤에도 선다" 로 남겼는데, 그 문장은 구 구조를 전제로 쓰인 것이라 자동 승계되지
  않는다. ★틱 소유로 이관한다★: 자체 14ms 스로틀을 버리고 틱마다 최대 1 회. 틱은 WR 의
  바쁨과 무관하게 매 주기 도므로 원래 요구("WR 이 바쁠 때에도 발동")가 그대로 충족된다.
  그리고 ★`FRAMEREASON` 에 자기 값을 갖는다★ — 지금 안 보이는 경로가 하나 있다는 사실
  자체가 조사를 반복시킨다.

  > ★착지한 것은 이관이 아니다 — 2026-10-22 전수 리뷰에서 확정한 미달 사항.★
  > `present_external_only` 의 호출자는 여전히 `Painter::update_images` 하나이고
  > (`painter.rs` 의 즉시-합성 게이트), 즉 ***외부 서피스가 도착한 틱***에만 불린다.
  > 바뀐 것은 스로틀의 단위뿐이다: 14ms 상수 → `wall_tick::ExternalPresentBudget` 의 틱
  > 번호. 그래서 ★천장("틱당 한 번 이하")은 섰고 바닥("매 틱 한 번")은 서지 않았다.★
  > `FRAMEREASON` 의 `external_surface` 값은 들어갔다(그쪽 요구는 충족).
  >
  > ★왜 좁히지 않았나★: 호출을 틱으로 옮기는 것은 DComp commit 경로의 거동 변경이고, 이
  > 저장소에는 ***commit 이 렌더 스레드로 새면 눈에 보이는 저더가 난다*** 는 실측 기록이
  > 있다. 실기 4-GPU 벽 없이는 그 변경을 잴 수 없어, 2026-10-22 에는 코드·시험·이 문서의
  > ★주장만 바로잡고 거동은 두었다★. 근거 한 벌은 `wall_tick::ExternalPresentBudget` 주석.
- **`send_initial_pipeline_transaction`(`painter.rs:3592`)** — 틱 루프를 믿을 수 있기 전의
  부트스트랩. ★파이프라인당 정확히 1 회, 그 파이프라인의 첫 틱 생산 이전에만★ 허용하고
  계수기를 붙인다. 종료 조건이 있는 예외다.

---

## 8. 검증

★약속하는 것은 구조이지 저더가 아니다.★

| 보는 것 | 성공 | 실패 |
|---|---|---|
| `FRAMEREASON` 구별 값 | ★정확히 둘★ — `tick`, `external_surface` | 그 외 값이 하나라도 등장 |
| `external_surface` 계수 | 틱당 1 회 이하 (★이하이지 "매 틱" 이 아니다 — 7 절★) | 초과 |
| `bootstrap` 계수 | 파이프라인당 정확히 1 | 2 이상 |
| 벽 페이서 합류 계수 | 벽 구성에서 0 | 0 이 아님 |
| `FRAMEID distinct_ids / renders` | 다중 타일에서 1.0 | 0.5 대 유지 |
| 배리어 등록률 | 틱당 1 회(전 타일 커버, keep-previous 포함) | 누락 = 2-5 미구현 |
| `WRRATE frames/onscreen/superseded` | 60~61 / 같음 / 0 | superseded 증가 |
| `WALLSPLIT pass_ms` | 현행 이하 | 상승 |
| `RAFCLOCK` | p50 16.67, 이상 1% 미만 | 악화 |
| `ANIMSTEP` dup/skip | 0 | 발생 = 두 시계 맥놀이 |
| `epoch/s` | ★`gfx_refresh_hz`/divisor★ (기본 pref 면 120/4 = 30) | 그 값의 divisor 배 = 분리 깨짐 |
| `winit_wall --capture` | 찍힌다 | 안 찍힘 = 2-8 미구현 |
| WPT | 변화 없음 | 지연·타임아웃 |
| 육안 | 기동·전환·정지에서 회귀 없음 | — |

★A/B 대조군★: 초고가 쓰려던 `gfx_video_immediate_composite_enabled` 는 4 절이 삭제하는
블록 안에 있어 쓸 수 없다. 대신 ★틱 생산을 끄는 새 디버그 플래그★ 를 두어 "생산자가 없으면
무엇이 멈추는가" 를 보이는 음성 대조군으로 쓴다(구 생산자는 이미 없으므로 화면이 서야
정상이고, 그것이 배선이 됐다는 증거다).

---

## 9. 작업 분할 개요

상세는 구현 계획으로 뺀다. 순서의 근거만 적는다.

1. **`with_painter_mut` 블로킹 계측** — 6 절. 틱이 painter 에 닿는 방식만 바꿀 수 있다.
2. **`FRAMEREASON` 가시화** — `present_external_only` 와 벽 페이서 합류 경로에 자기 값을
   준다. ★이후 모든 단계의 판정이 이것에 의존한다★ 므로 먼저 한다.
3. **콜백 손잡이 일원화** — Pacing / Timer / Vsync 세 구성의 refresh-driver 발사를 틱이
   소유하게 한다(2-4, 불변식 9·12).
4. **틱 진입점과 생산 배선** — `generate_frame_for_wall_tick`(2-5), 전 타일
   `WallFrameRequest` 생성자, 수요 구동 임베더의 에지 예약. 진입점은 ★벽(`Paint`) 수준
   연산★ 이고 `WebView` 핸들은 경유로만 쓴다. ★"한 `note_present_tick` 당 생산 1 회" 디버그
   어서션을 포함한다.★
5. **틱이 부르기 시작** — 기존 생산자를 그대로 둔 채. ★이 중간 상태는 플래그 뒤에서만
   존재하고 8 단계 전에는 릴리스하지 않는다. 이 단계의 `FRAMEID` 하락은 기대값이며 판정에
   쓰지 않는다.★
6. **ack 해제를 틱으로 이설**(2-7) — 네 지점 전부. ★가장 위험★. divisor=1 로 120 초 돌려
   117 초 스톨이 없는지 확인한다.
7. **스크립트·비디오·애니메이션·대역외 경로 격하**(2-6) — `set_pending_frame` 호출 제거 포함.
8. **역압을 벽 단위로**(2-5) — keep-previous 등록, 롤백 3 곳 동시 삭제.
9. **스크린샷 규칙**(2-8)과 부트스트랩 예외 계수기(7 절).
10. **게이트 정리와 주석 갱신** — 삭제되는 게이트의 역사 주석은 지우지 말고 이 문서로
    넘긴다("이 게이트는 ~에 삭제됐다. 그것이 막던 증상은 ~ 때문에 구조적으로 불가능하다").

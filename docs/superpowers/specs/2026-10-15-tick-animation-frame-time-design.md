# TickAnimation 에 프레임 시각을 싣는다 — 설계

2026-10-15. 선행 문서: `2026-09-21-per-output-commit-phase-design.md` §50(원인 특정),
§51(격자 스냅의 실패와 평활기).

## 1. 무엇을 고치는가

애니메이션 값의 기준 시각이 ***"script 스레드가 그 함수에 도달한 순간"*** 이다.
`DocumentTimeline::update`(`components/script/dom/animations/documenttimeline.rs:91`):

```rust
let raw = CrossProcessInstant::now() - window.navigation_start() - self.origin_offset;
```

이 값은 표출 격자와 무관하다. 틱이 발송된 뒤 script 가 거기 도달하기까지의 경로 지연이
매번 다르고, 그 변동이 그대로 화면의 떨림이 된다.

★그 지연이 어디서 생기는지 경로를 끝까지 읽어 확인했다.★ 홉이 셋이고, 마지막이 예상 밖이다.

```
paint   AnimationRefreshDriverObserver::frame_started          refresh_driver.rs:194
          send TickAnimation(Vec<WebViewId>)                              :169, :241
   ↓ crossbeam channel
const.  handle_tick_animation                                   constellation.rs:3808
          send TickAllAnimations(Vec<WebViewId>)                            :3824
   ↓ IPC
script  ScriptThreadMessage::TickAllAnimations                 script_thread.rs:2169
          ★set_needs_rendering_update() -- 플래그만 세운다★
   ↓ ***다음 메시지 펌프 회차***
script  needs_rendering_update && update_the_rendering(cx)                  :1789
   ↓
        Document::update_the_rendering → update_animations_and_send_events
          → DocumentTimeline::update → CrossProcessInstant::now()   ← 값의 출처
```

★마지막 홉이 핵심이다.★ 틱은 플래그만 세우고, 실제 실행은 script 가 큐에 쌓인 다른
메시지를 모두 처리한 뒤다. "도달 시각" 은 그 큐 길이에 좌우된다.

## 2. 측정이 말하는 것

| | `built_at_gap` p50 | stdev | `dx spread` | 육안 |
|---|---|---|---|---|
| divisor=4 | 66.70 | 0.41 | ★0.007★ | 저더 없음 |
| divisor=1 | 16.70 | 0.40 | ★0.014★ | ★저더★ |

(log_ani_debug_04/20 과 04/22, 각 네 실행, 기동 50초 제외. 두 날짜가 동일.)

노이즈 자체는 양쪽이 같다(stdev 0.4ms). 다른 것은 ***그것이 주기에서 차지하는 비율***이다.
`0.4 / 16.7 = 2.4%` 대 `0.4 / 66.7 = 0.6%`. 그리고 divisor=4 는 한 앵커로 네 프레임을
선형 예측해 메우므로 더 고르다 -- `PAINTANIMANCHOR` 가 d=4 6324 대 d=1 ★25012★.

★고칠 대상은 0.4ms 노이즈다.★ 그래서 격자 스냅(§51)이 실패했다 -- 16.67ms 양자로 0.4ms
노이즈를 없앨 수 없고, 경계를 넘을 때만 16.67ms 통째로 틀린다. 실측에서 `spread` 가
0.014 -> 0.053 으로 ***늘기만*** 했다.

## 3. 무엇을 하는가

★값의 출처를 "도달 시각" 에서 "그 프레임의 시각" 으로 바꾼다.★ HTML 표준이
"update the rendering" 한 회차에 하나의 timestamp 를 쓰라고 하는 바로 그 구조다.

프레임 시각은 ★셸의 페이싱 틱 시각★ 으로 잡는다. `fire_present_tick` 맨 앞에서 재므로
렌더 준비 비용이 섞이지 않는다.

```
셸      fire_present_tick()                      winit_wall/main.rs:1306
          let t = CrossProcessInstant::now();
          servo::note_frame_time(t);             ★신규 공개 함수★
          pacing_fire.fire(); render_all_tiles();
   ↓ paint 의 static (note_present_tick 과 같은 모양)
paint   frame_started()
          let frame_time = stored().filter(신선함).unwrap_or_else(now);
          send TickAnimation(webviews, frame_time)
   ↓
const.  send TickAllAnimations(webviews, frame_time)
   ↓
script  TickAllAnimations(_, frame_time)
          self.pending_frame_time.set(Some(frame_time));   ★최신이 덮어쓴다★
          set_needs_rendering_update()
   ↓ 다음 펌프 회차
script  update_the_rendering(cx)
          let frame_time = self.pending_frame_time.take();  ★Option, 소비★
          → Document::update_the_rendering(cx, frame_time)
              → update_animations_and_send_events(cx, frame_time)
                  → DocumentTimeline::update(window, frame_time)
              → run_the_animation_frame_callbacks(cx, frame_time)
```

### 3.1 타입

- 메시지에 싣는 값은 ★`CrossProcessInstant`(비-Option)★ 다. paint 가 항상 채운다.
  `Serialize + Deserialize` 가 이미 있다(`cross_process_instant.rs:26`).
- script 안에서는 ★`Option<CrossProcessInstant>`★ 다. `update_the_rendering` 은 틱 말고도
  scroll, resize, input, display-list, observer, animated-image, dirty-canvas 등
  ***열 가지 넘는 이유***로 돈다(`document.rs:2979-3021`). 그 경우 `None` 이고 지금처럼
  `CrossProcessInstant::now()` 를 쓴다.

### 3.2 왜 자유 함수 + static 인가

`servo::note_present_tick()`(`paint/lib.rs:62`)이 이미 같은 일을 한다 -- 셸이 틱 순간을
paint 에 알리는 통로다. 관찰자는 `Painter::new` 안에서 만들어져(`painter.rs:843`) 셸이
닿을 수 없으므로, `Servo -> Paint -> Painter -> observer` 로 배선하는 것보다 선례를 따르는
편이 작다.

기존 `note_present_tick` 은 ***재사용할 수 없다*** -- Windows 전용 QPC 를 저장하고
(`output_grid.rs:560`) DComp 정렬 전용이다.

### 3.3 신선도 가드 ★(없으면 화면이 멈춘다)★

셸이 부르다 멈추면(페이싱 스레드 사망 등) static 값이 굳어 ***애니메이션 시각이 정지***한다.
`frame_started` 에서 저장값이 `4 × paint_timer_period()` 보다 오래됐으면 `now()` 로
폴백한다. 셸이 애초에 부르지 않으면(servoshell, 다른 임베더) 저장값이 없어 같은 폴백을
타므로 ★지금 동작 그대로★ 다.

### 3.4 rAF 도 같은 값을 쓴다

`run_the_animation_frame_callbacks` 는 지금 `performance().Now()` 를 쓴다
(`document.rs:1754`). 타임라인만 바꾸면 ***둘이 큐 지연만큼 어긋나***, rAF 로 움직이는
요소와 `Element.animate()` 로 움직이는 요소를 섞은 페이지에서 지금 없애려는 증상이 그대로
생긴다. 표준은 한 회차에 같은 timestamp 를 쓰라고 한다.

변환은 이미 있다: `Performance::to_dom_high_res_time_stamp(CrossProcessInstant)`
(`dom/performance/performance.rs:179`).

★웹에 관측되는 값이 바뀐다★ -- 표준에 가까워지는 방향이지만, 변경임은 분명하다.

### 3.5 pref

`dom_animation_timeline_use_frame_time`, ★기본 `true`★. 끄면 `frame_time` 을 무시하고
타임라인과 rAF 모두 지금 경로로 간다. 실기 A/B 와 즉시 되돌리기를 위한 것이다.

### 3.6 평활기를 걷어낸다

`components/shared/base/animation_timeline_smoothing.rs` 와
`dom_animation_timeline_smoothing_hz`, `-TimelineSmoothing` 스위치,
`etc/multigpu/tools/run_timeline_smoothing_ab.ps1` 을 제거한다.

평활기는 이 교정의 국소 근사였다. 둘 다 두면 ***같은 값을 두 번 만지게*** 되고, 실기에서
어느 쪽이 효과를 냈는지 가릴 수 없다. 실기 검증을 한 번도 통과하지 않았으므로 남길 근거도
없다.

## 4. 바뀌지 않는 것

- ★`layout_animations_test_enabled` 경로.★ `Document::update_animations_and_send_events`
  가 그 pref 일 때 `timeline.update` 를 아예 부르지 않는다(`document.rs:4581`). 레이아웃
  테스트가 `advance_specific` 으로 미는 값은 건드려지지 않는다.
- ★`TickAnimation` 의 역압 구조.★ 틱은 지금처럼 `notify_will_paint` 에서 나간다. 실어
  보내는 값만 는다.
- ★다른 셸.★ `servoshell` 은 `note_frame_time` 을 부르지 않으므로 `now()` 폴백을 탄다.

## 5. 성공 기준

실기(`wall_anim_jitter_probe.html`, `divisor=1`)에서:

- ★`dx spread` 가 0.014 -> 0.007 수준(divisor=4 와 같음)으로 내려간다★
- ★육안 저더가 사라진다★
- `dx p50` 이 38.4 로 유지된다(느려지지 않았다는 증거)
- `divisor=4` 에서 켜도 `spread` 가 0.007 에서 나빠지지 않는다

A/B 는 네 칸이다: divisor 1/4 × frame_time on/off.

## 6. 비-목표

- `update_intersection_observer_steps(cx, CrossProcessInstant::now())`
  (`script_thread.rs:1498`). 같은 성질의 문제이고 업스트림에 TODO 가 달려 있지만, 저더와
  무관하므로 건드리지 않는다.
- 앵커 갱신 빈도 자체를 바꾸는 것(divisor=1 에서도 예측이 여러 프레임을 덮게 하기).
  근본적이지만 페인트 경로 변경이라 범위가 다르다. ★이 교정이 실패하면 그때 간다.★
- 기동 구간(첫 40~50초)의 불안정. 별개의 미해결 항목이다.

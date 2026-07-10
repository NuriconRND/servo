# SERVO_WALL_VIDEO_PACE_HZ stall 회귀 수정 — 설계

작성일: 2026-07-10
브랜치: `video-perf-investigation`
관련 커밋: `0ab43fa`(회귀 유발), 메모리 `wall-video-pace-refresh-driver-gap`

## 1. 배경 / 문제

`SERVO_WALL_VIDEO_PACE_HZ`는 비디오 월에서 compositor 재합성 레이트를 제한해 CPU/GPU 부하를
줄이려는 opt-in 노브다. 커밋 `0ab43fa`는 이 pace를 refresh-driver 애니메이션 틱까지 확장했는데,
적용 후 **배치된 영상 다수가 몇 프레임만 재생되고 정지하거나 전부 정지하는 stall 회귀**가 발생했다.

## 2. 근본 원인 (코드+측정 정합)

refresh-driver 애니메이션 루프는 **`AnimationRefreshDriverObserver::frame_started`가 매 틱 보내는
`TickAnimation`으로 자기 자신을 지탱**한다:

```
composite → notify_will_paint → frame_started → TickAnimation → script GenerateFrame
  → 새 repaint reason 세팅 → observe_next_frame(타이머 재장전) → 타이머 fire(waiting_for_frame=false)
  → needs_repaint()=true → 다음 composite → ...
```

핵심 게이트: `Painter::needs_repaint()`(painter.rs:536)는 `reason가 있고 && !wait_to_paint()`일 때만
true. `wait_to_paint()`(refresh_driver.rs:91)는 "animating이고 refresh 프레임 대기 중"이면 true →
composite 보류.

`0ab43fa`는 frame_started에서 `TickAnimation`을 조건부로 스킵했다. 그 틱엔 새 repaint reason이
생성되지 않으므로 타이머가 fire해도 `needs_repaint()=false` → composite 없음 → `observe_next_frame`
미호출 → **타이머 재장전 안 됨 → self-sustaining 루프 사망**. 자율적 GStreamer appsink(update_images,
~270/s)만으론 이 타이머 루프를 안정적으로 되살리지 못해 정지/멈칫이 발생한다.

정합 증거: p30 측정에서 composite_fps 60 중 immediate(비디오 image-update) 경로는 21/s뿐이고
나머지 ~39/s가 frame_started(TickAnimation)발이었다 → 지배 프레임 소스를 게이팅으로 끊었으니 루프가
죽는 게 당연. (이전 "비디오 update가 heartbeat" 가정은 정반대였다 — **TickAnimation이 heartbeat**.)

## 3. 설계: 루프를 끊지 말고 클럭을 늦춘다

프레임 **생산**을 게이팅하면 루프가 죽는다. 그러므로 게이팅을 제거하고 **루프의 클럭(refresh 타이머)만
늦춰** 루프 구조를 self-sustaining 그대로 두되 coherent하게 감속시킨다.

### 변경 4가지

1. **frame_started 게이팅 되돌리기** (`refresh_driver.rs`) — `TickAnimation`을 매 틱 정상 전송. 루프
   사망 원인 제거.

2. **`TimerRefreshDriver`의 틱 간격을 pace-aware로** (`refresh_driver.rs`, 현재 `FRAME_DURATION =
   1000/120` const 사용부) — `SERVO_WALL_VIDEO_PACE_HZ` 설정 시 `observe_next_frame` 간격을
   `max(FRAME_DURATION, pace_interval)`로. **임베더 공개 트레이트 `RefreshDriver`(embedder/lib.rs:287)는
   변경하지 않고**, in-tree 기본 구현체 `TimerRefreshDriver`만 수정. → `TickAnimation`·composite·present가
   모두 coherent하게 pace 레이트(예 30Hz)로 함께 감속. composite 30Hz 제한의 실제 게이트는 기존
   `wait_to_paint()`(타이머가 pace 간격마다만 waiting_for_frame을 풀어줌) → 소스(타이머/appsink) 무관하게
   composite가 pace로 눌린다.

3. **immediate-path pace 게이트 유지** (`painter.rs` `update_images`) — composite 사이 RenderBackend
   중복 빌드 방지(appsink가 pace보다 빠르게 도착해도 빌드는 pace로 coalesce). `pace_due()`/
   `mark_paced_frame()`/`last_paced_frame_at` 유지.

4. **불필요해진 헬퍼 제거** — `video_recently_active()`/`last_video_update_at`. `0ab43fa`의 게이팅
   전용이었고 이 설계에선 쓰이지 않음.

### 배관 세부
- `wall_video_pace_interval()`(현재 painter.rs 내 free fn)을 `refresh_driver.rs`에서 참조 가능하도록
  `pub(crate)` 노출(같은 `servo-paint` 크레이트). 필요 시 crate 루트로 이동.
- `TimerRefreshDriver::observe_next_frame`: `let interval = wall_video_pace_interval().filter(|p| *p >
  FRAME_DURATION).unwrap_or(FRAME_DURATION); self.queue_timer(interval, callback);`

## 4. 트레이드오프

`SERVO_WALL_VIDEO_PACE_HZ` 설정 시 **진짜 CSS/rAF 애니메이션도 pace 레이트로 감속**한다. per-content
(비디오-only) 감지를 생략한 대가이며, 그 덕에 stall 위험이 0이고 구현이 단순·견고하다. opt-in 진단
노브(기본 off)이므로 수용하고 pace 함수 doc에 명시한다. env 미설정 시 완전 무영향
(`wall_video_pace_interval()`이 None → 기존 `FRAME_DURATION`).

## 5. 검증 계획

구현 계획의 1단계에 계측·확인을 포함한다(근본 원인이 이전에 한 번 틀렸으므로 실행 확증 필수):
- `cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl` exit 0 + rustfmt.
- winit_wall 9x FHD30 그리드로 off vs pace=30 재측정(stderr `Render perf`, PowerShell `2>`는 UTF-16LE):
  - **성공 기준 1**: pace=30에서 `composite_fps` ~60→~30 하강.
  - **성공 기준 2**: 영상이 **정지 없이** 매끄럽게 재생(회귀 해소). 육안 + `video_uploads_per_s` 유지 확인.
  - decode CPU는 불변(SW 디코드 제약 — 미변경).

## 6. 범위 밖 (non-goals)

- SW 디코드 정책 변경(프로젝트 제약, 절대 미변경).
- per-content "비디오-only" 애니 판별(정식판, 후속 과제).
- `RefreshDriver` 임베더 트레이트 시그니처 변경.
- 부분 damage 재합성 등 별도 compositor 최적화([[video-grid-composite-bottleneck]] 소관).

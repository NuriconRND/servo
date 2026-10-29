# paint-side 애니메이션이 반복 경계에서 멈춘다 — 원인과 구현안

> 날짜: 2026-10-29 · 대상: `components/layout/display_list/paint_animation.rs`,
> `components/shared/paint/display_list.rs`
> 증상: 무한 반복 자막이 ★사이클당 정확히 한 번★ 멈칫하고, 반복해도 **전혀 완화되지 않는다**.

---

## ★상태: 이 문서의 기구는 2026-10-29 에 반증됐다★

`gfx_paint_side_animation_tick_divisor=1`(4 절 Task 0-a)을 개발 장비에서 돌린 결과
★육안 변화가 전혀 없었다★ — 같은 위치, 같은 타이밍, 사이클당 1 회 그대로다. 기구가
"경계 이후 값이 얼어붙고 ***다음 스크립트 틱***을 기다린다" 였다면 대기가 250ms → 17ms 로
줄어 반드시 변화가 있어야 했다.

★그 음성이 사는 제약:★ (b)는 ***디스플레이 리스트를 기다리는 것이 아니다.*** 따라서 "한
디스플레이 리스트 간격을 기다린다" 류의 후보가 전부 죽는다 — 아래 2 절의 기구, 그리고
"상수 run 이라 바인딩이 풀려 WebRender 가 마지막 값을 유지한다" 도 같은 이유로 죽는다.

2~4 절은 ★기록으로 남긴다★(반증된 가설과 그 근거, 그리고 3 절의 교훈은 유효하다).
새 선두 가설은 7 절이다.

---

## 0. 한 줄

★paint-side 애니메이션의 샘플 런웨이가 지평(3초)이 아니라 **다음 반복 경계**에서 끝난다.★
stylo 샘플러가 그 너머를 끝 키프레임으로 **클램프**하기 때문이다. 그래서 경계 이후 값이
얼어붙고, 스크립트의 다음 애니메이션 틱(`divisor` 프레임마다)이 와서야 풀린다 —
`divisor 15` 면 ★최대 250ms★ 정지다.

---

## 1. 증상 (실측, 운영자 육안)

`tests/html/ticker_slice_probe.html` — 가로 자막 띠 5 개, 각각
`element.animate([{transform: translate(from)}, {transform: translate(to)}],
{duration: dur, iterations: Infinity, easing: 'linear'})`,
`from = box.clientWidth`(뷰포트 우단), `to = -span.offsetWidth`(좌측 완전 이탈),
`dur = max(1500, 24000/speed)`.

| 관찰 | |
|---|---|
| 텍스트가 완전히 빠져나온 직후 멈칫 | ★사이클당 정확히 1 회★ |
| 빈도 | `1/dur` 를 따라감(`speed=1/4/10` 로 확인) |
| 반복 수십 회 | ★전혀 완화되지 않음★ |
| Chrome 같은 페이지·같은 speed | ★주기적 judder 없음★ |
| 페이싱 클럭 / 커밋 모드 / 프레임 배리어 변경 | 전부 **무영향** |

★이것과 별개의 judder 가 하나 더 있다★(진입 시점, 반복되면 **완화됨**) — 그쪽은 글리프
캐시 워밍으로 보이고 이 문서의 대상이 아니다. 섞어 보지 말 것.

---

## 2. 기구 (코드로 확정) — ★2026-10-29 반증됨, 기록으로 남긴다★

### 2-1. 런웨이는 반복 경계에서 끝난다

`components/layout/display_list/paint_animation.rs` 가 디스플레이 리스트를 만들 때
스타일 시스템을 `SAMPLE_SECONDS = 1/60` 간격으로 지평(`gfx_paint_side_animation_horizon_ms`,
기본 3000ms, `MAX_SAMPLES = 256`)까지 질의한다(`:125`, `:225`).

그런데 `third_party/stylo/style/servo/animation.rs` 의 샘플러:

```rust
// Progress clamped to the current iteration [0.0, 1.0].
let total_progress = progress.min(self.current_iteration_end_progress()).max(0.0);

// At/near 1.0 there is nothing left to interpolate. Return end keyframe.
if (total_progress as f32) >= 1.0 {
    let keyframe = match self.current_direction {
        AnimationDirection::Normal => self.computed_steps.last().unwrap(),
        AnimationDirection::Reverse => self.computed_steps.first().unwrap(),
        ...
    };
    add_declarations_to_map(keyframe);
    return;
}
```

`Infinite` 이면 `current_iteration_end_progress()` 가 `1.0`(`:698-704`)이므로 ★미래 시각을
질의하면 다음 반복으로 **감기지 않고 끝 키프레임에 고정된다.**★

그리고 `started_at` 을 전진시키는 `iterate()`(`:666`, `started_at += duration`)는 `&mut` 가
필요해 `components/script/animations.rs:166` 의 스크립트 틱에서만 불린다. layout 은
`&DocumentAnimationSet` 만 읽으므로 ***샘플링이 iterate 를 일으킬 수 없다.***

★결과: 샘플 열은 `선형 하강 → 끝값(좌측 밖)에 평탄` 이다.★

### 2-2. 그래서 화면이 얼어붙는다

`components/paint/web_content_animation.rs` 는 그 세그먼트를 자기 시계로 보간한다. 평탄
구간에서는 끝값을 그대로 낸다 — 자막이 **좌측 밖에 주차된 채** 멈춘다.

풀리는 것은 스크립트의 다음 애니메이션 틱이다. `gfx_paint_side_animation_tick_divisor` doc:

> Ask script for an animation tick once every N display frames … ***A tick is a full
> rendering update, and a rendering update reflows the whole document.*** … ticks are only
> needed often enough to ***rebase that prediction*** and to fire the animation's own events.

★정지 길이 = 반복 경계부터 다음 스크립트 틱까지 = 최대 `divisor` 프레임.★
`divisor 15` → 최대 **250ms**. 사이클당 1 회, 캐시 무관, 클럭·커밋·배리어 무관 — ★1 절의
관찰 여섯 개가 전부 맞는다.★

### 2-3. 리포의 doc 이 이미 이 모양을 적었다 (원인은 다르게 짚었다)

`gfx_paint_side_animation_horizon_ms` doc:

> ***This is a bet on how long script might stop producing display lists.*** Past the end of
> what was sampled the paint thread holds the last value, ★which looks exactly like the
> freeze this feature exists to remove.★ 실측(4-GPU 벽, 2026-09-08 `log_ani_perf/07`):
> 1 초 스크립트 블록 대 1 초 지평에서 애니메이션이 매 블록마다 중간에 멈췄다.

같은 정지다. 다만 doc 은 그것을 ***스크립트 블록***의 결과로 보았고, ★반복 경계가 스크립트
블록 없이도 런웨이를 잘라낸다는 것은 적혀 있지 않다.★

그리고 `a18686dbd77`(2026-09-08, 이 기능 도입)의 메시지와 `paint_animation.rs:55-58` 주석은

> 샘플하고 병합하면 easing·iteration·direction·fill 이 이미 구현한 코드에서 처리된다

고 적는데, ★iteration 은 처리되지 않는다 — 샘플되지도 않고 클램프된다.★ 그 주석을 고쳐야
한다.

### 2-4. Chrome 이 안 그러는 이유

Chromium 은 compositor 에 ★키프레임과 타이밍(iterations/direction/fill)을 보내고
compositor 가 시간을 모듈로로 감는다.★ 재샘플하지 않는다.

- `cc/animation/keyframe_model.cc` — `CreateImplInstance()` 가
  `set_iterations/iteration_start/direction/fill_mode` 를 복제하고 `curve()->Clone()`
- `ui/gfx/animation/keyframe/keyframe_model.cc` — 무한 반복이면
  `repeated_duration = TimeDelta::Max()`, `iteration_time = active_time % curve_->Duration()`

즉 Chromium 에서는 반복이 **곡선 평가 자체**에 들어 있어 이 문제가 생길 수 없다.

---

## 3. ★폐기된 수정안 (내 첫 제안)★

제안했던 것: `from_samples` 가 반복 경계에서 만드는 ★1 스텝 브리지 세그먼트★의 easing 을
`Linear` 대신 `Steps(1, false)` 로 바꿔 "유지 후 점프" 로 만든다.

적대적 검토(3 렌즈)가 세 군데에서 꺾었고, 그중 둘은 코드로 직접 확인했다:

| # | 판정 | 근거 |
|---|---|---|
| 1 | ★그 브리지 세그먼트가 **존재하지 않는다**★ | 2-1 의 클램프. 샘플에 점프가 없으므로 `from_samples` 는 브리지를 만들지 않는다. 제안은 ***아무것도 바꾸지 못한다.*** |
| 2 | ★`span == 1` 은 불연속의 판별자가 아니다★ | `display_list.rs:972-1010`·`:1034-1075` 의 탐욕 병합은 straight 실패 시 `start_index = index-1` 로 전진하므로, 허용오차(`f32` 1/2048, transform 0.01px) 밖인 **모든 곡선 구간**이 1 스텝 세그먼트가 된다. transform doc(`:1027-1033`)이 "a rotation simply fails the straightness test and keeps more segments" 라고 적는다. 제안대로면 ★비선형 애니메이션 전체가 60Hz 계단으로 퇴행한다.★ |
| 3 | `Steps` 가 애초에 필요 없다 | 맞닿은 두 세그먼트로 점프가 **정확히** 표현된다. `find_segment`(`web_content_animation.rs:271-285`)가 `elapsed < segment.end` 로 고르므로 `elapsed < T` → 세그먼트1(값 A), `elapsed == T` → 세그먼트2(progress 0 → 값 B). 유령 위치 없음 |

부수 발견 하나: ★테스트 헬퍼 `replay`(`display_list.rs:1300-1316`)가 `segment.easing.ease` 를
부르지 않는다★ — 선형 보간만 한다. 그래서 easing 회귀를 기존 테스트가 잡지 못한다.

---

## 4. 구현안

### Task 0 — 기구를 실측으로 못 박는다 (선행)

**0-a. ★실행했다 — 음성.★** `gfx_paint_side_animation_tick_divisor=1`, 개발 장비, 육안
변화 없음(2026-10-29). 이 문서의 기구는 여기서 죽는다. 아래 예측은 ★틀린 예측으로 남긴다★.

정지 길이가 `divisor` 프레임에 비례한다는 예측대로면 ★(b) 가 거의 사라져야 한다★
(250ms → 17ms). 사라지면 기구 확정이다.

★이것은 확인 수단이고 수정이 아니다★ — `divisor 1` 은 초당 122 리플로를 되돌리고
(`log_ani_perf/13`), 콘텐츠 전환 때 한 리플로가 14.5ms 라 벽이 2 초간 안 그려진 적이 있다.
그래서 Task 1 이 필요하다.

**0-b. 클램프를 테스트로 고정.** `Infinite` 애니메이션을 경계 너머 시각에 샘플해 값이 끝
키프레임에 머무는지 단언한다. 로그가 아니라 ***영구 테스트***다. 이 전제가 깨지면 Task 1·2
가 무의미해지므로 가장 먼저 고정한다.

### Task 1 — 샘플 시각을 반복 경계 너머로 접는다

`paint_animation.rs` 의 샘플 루프(`:128`, `:228`)가 질의하는 시각을 접는다:

```text
t_i = now + i * SAMPLE_SECONDS
경계를 넘으면  t_i' = started_at + ((t_i - started_at) mod current_iteration_duration)
```

`get_value_map_for_active_animations(time)` 는 `started_at` 기준으로 progress 를 계산하므로
★질의 시각만 접으면 stylo 를 건드리지 않는다.★ 필요한 필드는 전부 `pub` 다:
`Animation::{started_at, duration, direction, current_direction, iteration_state, fill_mode}`
와 `current_iteration_duration()`(`animation.rs:707`).

적용 조건:
- `KeyframesIterationState::Infinite` — 항상 접는다
- `Finite(current, max)` — 남은 반복 수만큼만 접고 그 뒤는 지금처럼 클램프(= `fill` 동작)
- `direction` 이 `Alternate`/`AlternateReverse` — 접을 때 방향을 교대시킨다. ★그 경계는
  불연속이 아니다★(값이 되돌아온다) → Task 2 의 break 를 기록하지 않는다
- `gfx_paint_side_animations_enabled` 가 꺼져 있으면 기존 조기 반환 그대로

★한 요소에 애니메이션이 여럿 겹칠 수 있다.★ `get_value_map_for_active_animations` 는
세트 전체를 합성하므로, 접기를 **애니메이션별**로 해야 한다 — 이 지점이 이 Task 의 유일한
구조적 난점이고, 세트 단위 API 로는 부족하면 애니메이션별 질의 경로를 추가해야 한다.
★착수 전에 그 API 모양을 먼저 확정할 것.★

### Task 2 — 불연속을 세그먼트 경계에 정확히 놓는다

Task 1 이 ***진짜 점프***를 샘플 열에 만든다. 그 위치를 ★우리가 정확히 안다★(접은 인덱스)
— 휴리스틱이 필요 없다.

`from_samples` 에 `breaks: &[usize]`(새 반복이 시작하는 샘플 인덱스)를 더하고:
- 병합이 break 를 **넘지 않게** 한다
- break 에서 맞닿은 두 세그먼트를 낸다: 앞 세그먼트 `end = break*step`(값 A),
  뒤 세그먼트 `start = break*step`(값 B)
- `f32` 판과 `LayoutTransform` 판 ★둘 다★(알고리즘이 동일하다)

★`Steps` 도, 크기 임계도 쓰지 않는다.★ 3 절 2·3 항이 그 둘을 각각 기각했다.

### Task 3 — 테스트

먼저 ★`replay` 가 `segment.easing.ease(raw)` 를 거치게 고친다★(3 절 부수 발견). 그러지
않으면 아래 (d)가 통과해 버린다.

| # | 테스트 | 못 박는 것 |
|---|---|---|
| (a) | 경계 너머 샘플이 끝 키프레임에 머문다 | Task 0-b, 전제 |
| (b) | 접은 샘플 run 이 ★지평 전체★를 덮는다(평탄 꼬리가 없다) | Task 1 |
| (c) | break 가 있는 run → 점프가 정확히 `break*step` 에 있고 ★중간 값이 하나도 없다★ | Task 2 |
| (d) | 곡선(smoothstep·회전) run → break 0 개, 세그먼트 수가 지금과 같다 | ★3 절 2 항의 회귀 방지★ |
| (e) | `alternate` → break 0 개, 값이 연속 | Task 1 의 방향 교대 |
| (f) | `Finite` 이 끝난 뒤 → 지금처럼 클램프(fill) | 회귀 방지 |

게이트: `cargo test -p servo-paint-api --lib`, `cargo test -p servo-paint --lib`,
`cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu`.

### Task 4 — 주석 정정

`paint_animation.rs:55-58` 과 `horizon_ms` doc 이 "iteration 은 이미 구현한 코드가
처리한다" / "스크립트 블록이 런웨이를 자른다" 고 적는다. ★반복 경계가 스크립트 블록 없이도
런웨이를 자른다★를 적는다. 커밋 메시지에 Task 0-a 의 전후 측정을 남긴다(리포 관례).

---

## 5. 장기 대안 (지금 하지 않는다)

Chromium 방식: `PaintAnimationProperty` 에 `period`/`iterations`/`direction` 을 실어 보내고
paint 가 `elapsed % period` 로 감는다. 재샘플이 사라지므로 Task 1·2 가 통째로 불필요해지고
런웨이 개념 자체가 없어진다. ★와이어 포맷 확장이라 더 큰 변경★이고, Task 1·2 로 증상이
해소되는지 먼저 본다.

---

## 6. 내가 지어낸 기구 셋, 전부 반증

| # | 기구 | 무엇이 죽였나 |
|---|---|---|
| 1 | 브리지 세그먼트의 유령 위치 | stylo 클램프(2-1) — 샘플에 점프가 없다 |
| 2 | "무한 반복 패턴 자체의 성질" | Chrome 이 같은 페이지에서 안 그런다 |
| 3 | 런웨이가 경계에서 끊겨 스크립트 틱 대기 | ★`divisor=1` 음성★ |

셋 모두 ★코드 판독으로 기구를 세운 것★이 공통 원인이다. 리포 메모리가 적어 둔 규칙
(`stop-on-design-overturning-review`, `search-history-before-redesigning`)대로, 다음부터는
판별 관측이 먼저다.

---

## 7. 새 선두 가설 — ★리포에 이미 기록되어 있다★

`tests/html/wall_anim_jitter_probe.html` 의 주석(2026-09, `log_ani_debug_03/18`):

> 키프레임 안에 불연속(offset w -> w+1e-6)을 박아 위상을 만들었다 — ★CSS 쪽에는 없는
> 잔상이 WAAPI 객체마다 생겼다.★ ★큰 transform 점프가 떠난 자리의 picture cache 를
> 무효화하지 않는 것으로 보이고, Edge/Chrome 에서는 나지 않는다.★

(b)의 성질과 전부 맞는다:

| (b) 의 성질 | 이 기록 |
|---|---|
| Chrome 무영향 | ★"Edge/Chrome 에서는 나지 않는다"★ |
| `divisor` 무관 | WebRender 무효화 문제이고 디스플레이 리스트 빈도와 무관 |
| 사이클당 1 회 | 큰 점프가 사이클당 한 번이다(`to` → `from`) |
| 감쇠 없음 | 캐시 워밍이 아니라 무효화 누락 |
| 클럭·커밋·배리어 무관 | 페이싱보다 아래 층 |

★그리고 2026-10-29 의 두 관측이 이것을 더 좁힌다:★

- `phase=spread` 로 다섯 띠의 경계를 엇갈리면 ★judder 가 사라진다★(첫 실행 제외).
  즉 경계 작업은 ***애니메이션 단위***이고, `phase=same` 에서 다섯이 한 프레임에 겹친
  것이다.
- `spread` 에서 새로 보인 것: 글자가 오른쪽 끝에서부터 들어오지 않고 ★일부가 1 프레임
  사이에 갑자기 등장★한다. `phase=same` 에서도 간헐적으로 있었다. ★이것은 타이밍 끊김이
  아니라 "덜 그려졌다" 의 모양이다★ — 위 잔상 기록과 같다.

### 다음 판별

1. ★`?marquee=4`★ — 2026-10-29 에 `ticker_slice_probe.html` 에 넣었다(커밋 `97dacad6294`).
   같은 글자를 네 벌 겹쳐 음수 delay 로 흩으므로 ★띠가 비는 구간이 사라진다★. 운영자가
   `wall_anim_jitter_probe.html`(marquee)은 개발 환경에서 judder 가 없다고 확인했다. 같은
   페이지 안에서 벌 수만 바꾸는 A/B 다.
2. marquee 에서도 (b)가 남으면 "띠가 비는 전이" 가설이 죽고, 남은 것은 ★점프 자체★다.
   그때는 `log_ani_debug_03/18` 의 런을 다시 찾아 그때 무엇을 쟀는지부터 읽는다.
3. 기구가 picture cache 무효화로 좁혀지면 ★범위가 WebRender 쪽★이다 —
   `vendor_local/webrender` 가 이미 동작 변경 포크이므로 손댈 자리가 있다
   (`vendored-webrender-is-a-behavior-fork`).

---

## 8. 범위 주의

이 결함이 ★4 타일 벽 judder 의 주범인지★는 별개 문제다. 벽에서는 타일이 넷이고 진입
가장자리 쪽(1·3)에서 관찰됐으므로, (b) 를 고친 뒤 벽을 다시 봐야 한다.

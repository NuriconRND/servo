# Web Animations 최소 구현 설계 (2026-09-17)

대상 브랜치: `wall-transition-flicker`
관련 조사: `docs/multigpu/wall_transition_prereplay.md`

## 목표

`Element.animate()` 를 최소 범위로 구현해 `/output` 페이지가 CSS 폴백 대신 네이티브
경로를 타게 한다. 페이지는 **한 줄도 바뀌지 않는다** — `animateEl` 이
`typeof el.animate === 'function'` 으로 경로를 고르므로, 메서드가 생기는 순간 자동으로
전환된다.

## 배경 — 왜 필요한가

`frontend/packages/shared/ui/lib/cssAnimation.ts` 첫머리:

> 표출 엔진(Servo)에는 `Element.animate`가 없다. … **네이티브가 있으면 손대지 않는다.**
> Chrome은 지금까지와 같은 경로로 돌아, 검증된 동작과 정확한 취소를 그대로 쓴다.
> **위험은 엔진 쪽 경로에만 둔다.**

Chrome은 WAAPI, Servo는 CSS `@keyframes` 폴백으로 **서로 다른 코드를 돈다.** 그래서
"Chrome에서 정상" 은 폴백을 검증해 주지 않는다.

폴백은 `fill: forwards` 일 때 인라인 `style.animation` 을 의도적으로 남기고(최종 위치를
붙들기 위해), 퇴장만 `fill: 'forwards'` 로 걸린다(`useBoardHandover.ts:369`). 그 요소가
다음 판으로 재사용되거나 DOM에서 이동하면 남아 있던 **퇴장 키프레임이 그대로 다시
재생된다** — 전환 대상 구성이 표출되기 전에 한 번 빠져나가는 증상이 이것이다. 요소가
제거·재삽입될 때 CSS 애니메이션이 처음부터 도는 것은 명세상 정상이므로 엔진이 막을
일이 아니다. 폴백을 없애는 것이 해법이다.

폴백이 존재하는 이유는 `Element.animate` 부재 **하나뿐**이다.

## 사용자 결정 사항

- **표면 범위**: 페이지가 쓰는 것만 — `Element.animate()` 와 `Animation.cancel()`.
- **안전장치**: pref 게이트, **기본 OFF**. 켜서 검증하고, 검증되면 기본값을 뒤집는다.
- **배치**: 안 1 — 스크립트 애니메이션을 기존 `ElementAnimationSet` 에 넣고 출처
  플래그로 수명만 예외 처리한다.

## 페이지가 실제로 쓰는 표면

전체 프론트엔드에서 호출처는 `cssAnimation.ts:137` **한 곳**이고, 반환 객체에서 쓰는
것은 `cancel()` **뿐**이다. 그 한 곳으로 들어오는 호출은 넷이다.

| 호출처 | 옵션 | 키프레임 |
|---|---|---|
| `useBoardHandover.ts:360` 등장 | `{durationMs, fill:'none'}` | offset 명시, transform/transformOrigin/opacity/easing |
| `useBoardHandover.ts:369` 퇴장 | `{durationMs, fill:'forwards'}` | 같음 |
| `TickerContent.tsx:116` 마퀴 | `{durationMs, iterations:Infinity, easing:'linear'}` | **offset 없음**, 2프레임, transform |
| `TickerContent.tsx:185` 줄 등장 | `{durationMs, easing:'ease-out'}` | **offset 없음**, 2프레임, transform/opacity |

즉 필요한 것은 네 옵션(`duration`·`easing`·`iterations`·`fill`), 세 속성(`transform`·
`transform-origin`·`opacity`), 프레임별 `offset`·`easing`(둘 다 선택), `Infinity` 반복,
그리고 반복 호출되는 `cancel()` 이다.

## 대안 검토

**안 2 — 스크립트 전용 목록을 따로 두고 캐스케이드에 별도 주입.** CSS 애니메이션에 대한
위험은 0이지만 **페인트측 바인딩을 못 받는다**(`transform_binding` 이 애니메이션 세트를
읽는다). 스크립트가 전환 작업으로 300~500ms 막히는 동안 애니메이션이 얼어붙는데
(`SCRIPTBUSY longest_ms=285~511` 실측), 그건 지금 CSS 폴백보다 나쁘다. 탈락.

**안 3 — 내부적으로 `@keyframes` 를 합성해 `animation-name` 을 심는다.** 폴백이 JS로
하는 일을 Rust로 옮기는 것이라 폴백의 결함을 그대로 물려받는다. 스타일 변경이 스크립트에
관측되는 것도 문제다. 탈락.

## 설계

### 1. 데이터 흐름

```
el.animate(frames, opts)
   │
   ├─ [스크립트]  키프레임 파싱 → KeyframesAnimation { steps, properties_changed }
   │              ElementAnimationSet.pending_script 에 적재, 노드 더티
   │
   ├─ [리스타일]  pending_script 드레인
   │              ComputedKeyframe::generate_for_keyframes(…, context, new_style)  ← 기존
   │              Animation { origin: Script, state: Running, … } 를 animations 에 push
   │
   └─ 반환        DOM Animation (대상 요소 + 합성 이름)

이하 전부 기존 경로:
   get_value_map_for_active_animations → 캐스케이드
   transform_binding                   → 페인트측 바인딩
   update_animations_and_send_events   → 타임라인 전진
```

### 2. 왜 2단계인가

`ComputedKeyframe::generate_for_keyframes` 는 `SharedStyleContext` 와 요소의
`ComputedValues` 를 요구하는데 **스크립트에는 둘 다 없다.** 그것들은 리스타일 중에만
존재하므로, 계산을 CSS 애니메이션이 만들어지는 바로 그 자리로 미룬다.

파싱은 스타일 컨텍스트가 필요 없으므로(`parse_one_declaration_into`) 스크립트가 하고,
계산만 레이아웃이 한다.

`ElementAnimationSet` 에 필드 하나를 더한다:

```rust
pub struct ElementAnimationSet {
    pub animations: Vec<Animation>,
    pub transitions: Vec<Transition>,
    pub pending_script: Vec<PendingScriptAnimation>,   // 신규
    pub dirty: bool,
}
```

양쪽이 이미 공유하는 구조라 `TElement` 에 새 훅을 뚫지 않아도 된다 — `matching.rs` 는
이미 `shared_context.animations.sets.write().remove(&key)` 로 이 세트를 집어 온다.

드레인 위치는 `needs_animations_update` 게이트 **바깥**이다. 스크립트 애니메이션은
스타일이 바뀌지 않아도 만들어져야 하는데 그 게이트는 스타일 변화가 없으면 통과시키지
않는다.

### 3. 수명 예외 — 유일한 위험 구간

```rust
pub enum AnimationOrigin { Css, Script }
```

스타일이 애니메이션 수명을 모는 자리는 정확히 셋이고, 각각에서 `Script` 를 건너뛴다.

| 자리 | 그대로 두면 | 조치 |
|---|---|---|
| `is_cancelled_in_new_style` (`servo/animation.rs:1308`) | 스타일에 이름이 없으므로 **생성 즉시 취소** | `origin == Script` 면 검사 생략 |
| `maybe_start_animations` 의 기존 애니메이션 순회 (`servo/animation.rs:1941`) | 합성 이름은 원래 매칭되지 않지만 방어적으로 | `Canceled` 를 건너뛰는 줄 옆에서 `Script` 도 건너뜀 |
| `Finished` retain (`matching.rs:797`) | 스타일이 지명할 수 없으므로 끝나는 즉시 제거 → `fill: forwards` 최종 값 소실 = **검은 화면 재현** | `origin == Script` 면 무조건 남김 |

세 자리 모두 `if origin == Script { … }` 한 줄짜리 분기이고, `Css` 일 때의 동작은
지금과 동일하다. pref 가 꺼져 있으면 `Script` 애니메이션이 생성되지 않으므로 분기에
도달조차 하지 않는다 — **pref OFF 상태에서 이 변경의 실질 영향은 0이다.**

정리는 기존 경로가 한다: `cancel()` 또는 `cancel_animations_for_node`(요소가 DOM에서
빠질 때) 가 `Canceled` 로 바꾸면 `handle_canceled_animations` → `sets.retain` 이 치운다.
**새 정리 경로를 만들지 않는다.**

### 4. 키프레임 처리

배열 형태만 받는다. 각 프레임에서 `offset`·`easing` 을 떼고 나머지를 속성으로 본다.

- **offset**: 명시되면 `[0,1]` 범위와 비감소를 검사하고 위반 시 `TypeError`. 누락되면
  첫 0, 마지막 1, 중간은 알려진 이웃 사이 균등 분배. 마퀴가 offset 없이 2프레임을
  넘기므로 실제로 쓰인다.
- **속성 이름**: camelCase(`transformOrigin`) → CSS 이름(`transform-origin`). CSSOM 이
  쓰는 동일 매핑을 재사용한다.
- **값 파싱**: `parse_one_declaration_into` 로 프레임마다
  `PropertyDeclarationBlock` 을 만든다. 파싱 실패한 속성은 조용히 버린다(명세 동작).
- **per-frame easing**: 그 스텝의 `animation-timing-function` 선언으로 넣는다 —
  `KeyframesStep.declared_timing_function` 이 이미 그 용도다.

### 5. 옵션 매핑

딕셔너리에는 네 멤버만 정의한다. WebIDL 은 선언되지 않은 멤버를 무시하므로 지원 범위를
딕셔너리 자체가 문서화한다.

| 옵션 | → `stylo::Animation` |
|---|---|
| `duration` (ms) | `duration = ms / 1000.0` |
| `easing` | 전체 기본 타이밍 함수 |
| `iterations` | `Finite(0.0, n)`, `Infinity` 면 `Infinite(0.0)` |
| `fill` | `fill_mode` (`auto` → `none`) |

고정값: `direction = Normal`, `delay = 0`, `started_at` = 생성 시점 타임라인 값,
**`state = Running`**.

`Pending` 이 아니라 `Running` 으로 시작하는 것이 중요하다. `start_pending_animations`
가 승급하면서 `animationstart` **CSS 이벤트를 쏘는데**, 스크립트 애니메이션에 그것이
나가면 안 된다. 처음부터 `Running` 이면 그 경로에 들어가지 않는다.

### 6. DOM `Animation` 객체

대상 요소와 합성 이름(`-servo-script-<N>`)만 들고 있는 얇은 핸들이다. 합성 이름은
페이지의 `@keyframes` 이름과 충돌할 수 없고 `cancel()` 이 세트에서 자기 것을 찾는 키가
된다.

```
cancel():
  pending_script 에 있으면  → 제거 (아직 실체가 없다)
  animations 에 있으면      → state = Canceled
  둘 다 아니면              → no-op
  노드 더티 → 다음 리스타일이 반영
```

`cancel()` 은 마퀴에서 ResizeObserver 재시작마다 불리므로 **반복 호출과 이미 끝난
애니메이션에 대한 호출이 흔하다.** 둘 다 터지지 않아야 한다.

끝난(`Finished`) 스크립트 애니메이션은 §3에 따라 세트에 그대로 남아 있으므로, 그때의
`cancel()` 은 no-op 이 아니라 `Canceled` 로 바꿔 **붙들고 있던 `fill: forwards` 최종 값을
푼다** — 명세와 같은 동작이고, 이후 정리도 `handle_canceled_animations` 가 맡는다.
이미 `Canceled` 인 것에 대한 두 번째 호출부터가 no-op 이다.

### 7. pref 게이트

```
[Pref="dom_web_animations_enabled"]   // 기본 false
Element.animate(...)
```

꺼져 있으면 메서드가 정의되지 않고, 페이지의 `typeof el.animate === 'function'` 검사가
실패해 지금의 CSS 폴백이 그대로 돈다. 같은 배포본으로 A/B 가 되고 재빌드 없이 되돌린다.

### 8. 에러 처리

**하드 가드: `duration <= 0`.** `Animation` 은 진행도를 `(now - started_at) / duration`
으로 계산하므로 0이면 나눗셈이 깨진다. CSS 경로는 `maybe_start_animations` 에
`if duration == 0. { continue; }` 로 이미 막혀 있고 같은 가드를 둔다 — 애니메이션을
만들지 않고 비활성 핸들만 돌려준다.

| 입력 | 처리 |
|---|---|
| `offset` 범위 밖 / 감소 | `TypeError` |
| `iterations` 음수·NaN | `TypeError` |
| 파싱 실패한 속성 값 | 조용히 버림 |
| 빈 키프레임 배열 | 스텝 없는 애니메이션 — 값이 안 나오고 무해 |
| 애니메이션 불가 속성 | 파싱은 되고 보간에서 무시 |
| 문서에 없는 요소 | 요청이 대기하다 세트와 함께 소멸 |
| 끝난 애니메이션에 `cancel()` | `Canceled` — 붙들던 최종 값이 풀린다 (§6) |
| 이미 `Canceled` 인 것에 `cancel()` | no-op |

### 9. 검증

**단위 테스트** — 순수 함수로 떼어낸다(이 저장소의 기존 방식; decouple 계획의
`should_fast_present` 와 같은 패턴):

- offset 확정: 명시/누락 혼합, 균등 분배, 범위·순서 위반 거부
- 옵션 매핑: ms→초, `Infinity`→`Infinite`, `fill` enum, **`duration<=0` 가드**

JS 컨텍스트가 필요한 부분(딕셔너리 변환, 실제 파싱)은 실기로 검증한다.

빌드 관례: `cargo check -p servo --example winit_wall --features
media-gstreamer,no-wgl,webgpu`, 손댄 파일 rustfmt, `git diff --check`.

**실기 — pref OFF 무해성을 먼저 증명한다.**

- `Element.animate` 가 정의되지 않아야 함
- 로그에 `ANIMSTART name=sd-anim-N` 이 그대로 나와야 함(페이지가 CSS 폴백을 탄다는 뜻)

**실기 — pref ON.** 경로가 바뀌었는지는 로그가 한 줄로 말한다: WAAPI 를 타면 페이지가
`@keyframes sd-anim-N` 을 주입하지 않으므로 `ANIMSTART` 의 이름이 `sd-anim-N` →
`-servo-script-N` 으로 통째로 바뀐다.

| 항목 | 기대 |
|---|---|
| **전환 대상 구성 사전 재생** | **사라짐** (이 작업의 목적) |
| 마퀴 무한 스크롤 | 정상 |
| 등장/퇴장 애니메이션 모양 | 지금과 동일 |
| `PAINTANIM built tx_bound` | 0 아님 — 페인트측 바인딩을 탄다는 증거 |
| `SCRIPTBUSY reflow_display`/`reflow_ms` | 변화 없음 |
| `WRRATE` fps | 변화 없음 |

`tx_bound` 가 0이면 스크립트 애니메이션이 페인트측에 안 묶인 것이고, 그러면 스크립트
정체 중 얼어붙는다 — 안 1을 택한 근거가 무너지는 것이라 반드시 확인한다.

### 10. 롤아웃

1. pref 기본 OFF 로 머지 → 실질 영향 0
2. OFF 무해성 확인
3. `-Pref dom_web_animations_enabled=true` 로 실기 검증(같은 배포본 A/B)
4. 검증되면 **별도 커밋으로** 기본값을 뒤집는다

## 비목표

`getAnimations()`, `Animation` 의 재생 제어(`play`/`pause`/`reverse`/`finish`)·프라미스
(`ready`/`finished`)·이벤트(`finish`/`cancel`/`remove`), 객체 형태 키프레임,
`delay`/`direction`/`iterationStart`/`endDelay`/`composite`, `KeyframeEffect`·
`AnimationEffect` 노출, WPT 적합성.

**이것은 명세 준수 WAAPI 가 아니다.** 딕셔너리에 선언된 네 옵션이 지원 범위의 전부이고
그 사실을 딕셔너리 자체가 문서화한다. 페이지가 나중에 `playState` 나 `finished` 를 쓰면
그때 확장한다.

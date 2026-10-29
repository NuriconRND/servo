# paint-side 런웨이가 반복 경계에서 끝난다 — 원인, 수정, 남은 것

> 2026-10-29 · 대상: `components/layout/display_list/paint_animation.rs`,
> `components/shared/paint/display_list.rs`, `third_party/stylo/style/servo/animation.rs`
>
> ★이 문서는 두 번 뒤집혔다.★ 처음에 기구를 세우고, `divisor=1` 음성으로 기각했다가,
> 증상이 둘이라는 것을 알고 되살렸다. 그 경위는 5-1 절에 남긴다 — 같은 오독을 반복하지
> 않기 위해서다.

---

## 1. 한 줄

★stylo 가 반복 경계 너머를 끝 키프레임으로 클램프하므로, paint-side 샘플 런웨이가
지평(3초)이 아니라 **다음 반복 경계**에서 끝난다.★ 경계 이후 paint 는 다음 디스플레이
리스트까지 마지막 값을 붙들고, 그것이 사이클당 한 번의 정지로 보인다.

---

## 2. 증상은 둘이고, 한 기구의 두 얼굴이다

`tests/html/ticker_slice_probe.html` — 무한 반복 자막 띠 5 개, `phase=same` 이라 다섯이
같은 순간에 경계를 지난다. 재현이 확실한 설정은 `speed=10`(`dur` 2400ms).

| | 증상 A "오른쪽에서 갑자기 나타남" | 증상 B 주기적 judder |
|---|---|---|
| `divisor=1` | ★사라짐★ | 그대로 |
| `-FrameTime off` | — | 그대로 |
| `gfx_paint_side_animations_enabled=false` | 사라짐 | ★사라짐★ |

★하나의 기구가 둘을 낸다.★ 붙드는 길이는 다음 디스플레이 리스트까지 = `divisor` 프레임:

| `divisor` | 붙듦 | 위치 오차(A) | 동작 정지(B) |
|---|---|---|---|
| 15 | ≤250ms | travel 의 약 10% → ★보임★ | ≤15 프레임 |
| 1 | ≤17ms | 약 0.4% → ★안 보임★ | ★1 프레임 — 그대로★ |

★1 프레임 정지는 틱을 더 자주 받는 것으로 없앨 수 없다.★ 런웨이가 경계를 넘어가야 한다.

---

## 3. 기구 (코드 + 실측)

### 3-1. 런웨이가 경계에서 끝난다

`paint_animation.rs` 가 스타일 시스템을 `SAMPLE_SECONDS = 1/60` 간격으로
`gfx_paint_side_animation_horizon_ms`(기본 3000ms, `MAX_SAMPLES = 256`)까지 질의한다.
그런데 `third_party/stylo/style/servo/animation.rs`:

```rust
// Progress clamped to the current iteration [0.0, 1.0].
let total_progress = progress.min(self.current_iteration_end_progress()).max(0.0);
if (total_progress as f32) >= 1.0 {
    add_declarations_to_map(/* 끝 키프레임 */);
    return;
}
```

`Infinite` 이면 `current_iteration_end_progress()` 가 `1.0` 이므로 ★미래 시각 질의가 다음
반복으로 감기지 않는다.★ 그리고 `started_at` 을 전진시키는 `iterate()` 는 `&mut` 라
`components/script/animations.rs` 의 스크립트 틱에서만 불리고, 샘플링은
`&DocumentAnimationSet` 만 들고 있으므로 ***일으킬 수 없다.***

결과: 샘플 열이 `선형 → 끝값에 평탄` 이 되고, `running()`(= `elapsed < last.end`)이 거짓이
되면 paint 는 마지막 값을 낸다.

### 3-2. 실측이 이 기구를 지목한다 (2026-10-29, 개발 장비)

- 전 띠가 ★같이★ 멈칫한다 → 프레임 단위, per-element 값 분기가 아니다.
- fps 는 60 그대로다 → 프레임이 늦는 게 아니라 ★값이 안 변한다.★
- ★`dir=alternate` 로 경계에서 값이 **연속**인데도 양 끝 모두에서 같은 증상이 난다.★
  위치 불연속이 아니라 ***붙듦***이 원인이라는 결정적 증거다.
- `enabled=false` 가 없애고, `-FrameTime off`(위상 정합,
  `2026-10-15-tick-animation-frame-time-design.md`)는 무영향이다.

무영향으로 확인된 것: 페이싱 클럭 종류, `-DwmAlign`, 커밋 모드(`-DcompCommitInFrame`·
`-DcompParallelCommit`), 프레임 배리어, `overflow: hidden`→`clip`, 디바이스/타일 수,
marquee 복제, `clip` 역할 조합.

### 3-3. 리포 doc 이 이 정지의 모양을 적었다 (원인은 다르게 짚었다)

`gfx_paint_side_animation_horizon_ms` doc: *"Past the end of what was sampled the paint
thread holds the last value, ★which looks exactly like the freeze this feature exists to
remove★"*. 같은 정지인데 ***스크립트 블록***의 결과로 보았고, ★반복 경계가 블록 없이도
런웨이를 자른다는 것은 없다.★ 그리고 `a18686dbd77` 와 `paint_animation.rs` 의 설계 주석이
"샘플하면 iteration 도 이미 구현한 코드가 처리한다" 고 적는데 ★틀렸다 — 샘플되지도 않고
클램프된다.★ (이번 변경에서 고쳤다.)

### 3-4. Chrome 이 안 그러는 이유

Chromium 은 compositor 에 ★키프레임과 타이밍을 보내고 compositor 가 시간을 모듈로로
감는다★ — 재샘플하지 않는다. `cc/animation/keyframe_model.cc`(`CreateImplInstance` 가
`iterations`/`direction`/`fill_mode` 를 복제), `ui/gfx/animation/keyframe/keyframe_model.cc`
(`iteration_time = active_time % curve_->Duration()`).

---

## 4. 수정 — Task 1 ★구현했다★

### 4-1. 질의 시각을 반복 경계 너머로 접는다

`components/shared/paint/display_list.rs` 에 순수 함수:

```rust
pub fn fold_animation_sample_time(started_at, iteration, alternating, time) -> f64
```

`paint_animation.rs` 가 애니메이션마다 이것을 거쳐 질의한다. ★접기가 애니메이션
단위★인 것이 핵심이다 — 한 요소에 주기가 다른 애니메이션이 여럿 걸릴 수 있으므로 세트
단위 API(`get_value_map_for_active_animations`, 모두에게 같은 `now`)로는 안 된다.
그래서 stylo 의 `get_property_declaration_at_time` 을 ★`pub` 한 단어★만 열었다(동작 불변).

★`alternating` 을 시간 거울반사로 정확히 다룬다 — 사본도 방향 뒤집기도 없다.★
역방향 반복의 값은 `keyframe(ease(1 − τ/주기))` 이고, 0번째 반복을 `주기 − τ` 에 질의한
값이 `keyframe(ease((주기−τ)/주기))` 로 ***easing 까지 같다***(Web Animations 가 directed
progress 를 뒤집은 뒤 easing 을 적용한다). `alternate` 는 0번째가 정방향,
`alternate-reverse` 는 0번째가 역방향이라 ★둘 다 홀수 번째 반복에서만 거울반사★하면 맞는다.

★`Infinite` 에만 접는다.★ `Finite` 는 남은 반복 수와 비정수 마지막 반복
(`current_iteration_end_progress`)을 함께 따져야 하고, 끝난 뒤의 클램프는 `fill` 의 정의된
동작이다 — 지금 동작을 그대로 둔다. 보고된 결함은 무한 반복이라 덮인다.

### 4-2. 시험 (`cargo test -p servo-paint-api --release --lib`)

★`cargo test -p servo-layout` 은 이 환경에서 못 돈다★ — 테스트 프로파일이 `aws-lc-sys`
(NASM 필요)를 끌어오고 NASM 이 없다. 그래서 순수 산수를 `servo-paint-api` 로 뒀다. 리포가
`wall_layout` 을 같은 이유로 거기 승격한 것과 같은 패턴이다(CLAUDE.md).

여섯 개: 경계 이전 무개입 / normal 한 주기 접기 / ★alternate 홀수만 거울반사★ /
★alternate 경계 연속 + normal 경계 불연속★ / 주기 0·음수·NaN 무개입 / 시작 앞 시각 무개입.

★이 트리는 release 전용이다.★ `debug` 로 검사하면 네이티브 의존성을 처음부터 빌드하려
들고 그 전제(NASM, `make`)가 없다.

---

## 5. 남은 것 — Task 2, ★경계의 정확한 절단★

Task 1 이 `Normal`/`Reverse` 에서 ***진짜 점프***를 샘플 열에 만든다. `from_samples` 의
탐욕 병합이 그 점프를 ★한 스텝 선형 스윕★으로 바꾸므로 ***한 프레임 동안 잘못된 중간
위치***가 나온다. 250ms 정지가 한 프레임 위치 오차로 줄어드는 ★엄격한 개선★이고
`alternate` 에는 점프가 없지만, 정확하지는 않다.

★경계는 샘플 색인으로 자를 수 없다 — 경계 시각이 1/60 격자에 맞지 않는다.★ 그러므로
`from_samples` 가 받아야 하는 것은 ***경계 시각***(런웨이 기준 초)이고, 각 경계에서:

- 앞 세그먼트: `end = 경계시각`, `to = 그 반복의 끝 값`
- 뒤 세그먼트: `start = 경계시각`, `from = 다음 반복의 시작 값`

맞닿은 두 세그먼트가 점프를 ★정확히★ 표현한다 — `find_segment` 가 `elapsed < end` 로
고르므로 경계 직전은 앞, 경계부터는 뒤가 선택된다. ★`Steps` easing 도, 크기 임계도
쓰지 않는다.★ 경계의 양쪽 값은 스타일 질의 두 번으로 얻는다(경계당, 애니메이션당).

함께 할 것: ★테스트 헬퍼 `replay` 가 `segment.easing.ease` 를 부르지 않는다★
(`display_list.rs`) — 선형 보간만 해서 easing 회귀를 못 잡는다.

### 5-1. ★폐기된 제안과 그 이유★ (반복 방지)

| # | 제안/기구 | 무엇이 죽였나 |
|---|---|---|
| 1 | 브리지 세그먼트의 easing 을 `Steps(1,false)` 로 | 그 브리지가 **존재하지 않았다** — 클램프 때문에 샘플에 점프가 없었다 |
| 2 | `span == 1` 을 불연속 판별자로 | 허용오차(f32 1/2048, transform 0.01px) 밖인 ***모든 곡선 구간***이 1 스텝이다. 그대로면 비선형 애니메이션 전체가 60Hz 계단으로 퇴행한다 |
| 3 | "무한 반복 패턴 자체의 성질" | Chrome 이 같은 페이지에서 안 그런다 |
| 4 | 반복 경계 → ★`divisor=1` 음성으로 기각★ | ★오독이었다★ — divisor 는 재기준 빈도만 바꾸고 보간은 켜져 있다. 그 음성이 반증한 것은 "재기준을 기다린다" 하나뿐이고, 증상 A 는 실제로 사라졌다(증상 둘을 구분하지 못했다) |
| 5 | 커밋 토폴로지(디바이스당 `IDCompositionDevice`) | DWM 합성 격자는 하나이고 규칙적이라, 일정하게 늦은 커밋은 ★오프셋이고 저더가 아니다.★ 분산을 보일 책임이 가설 쪽에 있고 리포 수치가 지지하지 않는다(운용 설정에서 커밋 이상치 런당 9 건) |
| 6 | `overflow: hidden` 스크롤 프레임 | `clip=all` 음성 |
| 7 | "띠가 비는 구간의 전이" | `marquee=4` 음성(그 구현은 띠끼리 위상을 그대로 두므로 다섯이 여전히 동시에 wrap 한다 — 변별력이 없었다) |
| 8 | picture cache 무효화 누락 | `alternate` 가 값 연속인데도 발생시켜, 위치 점프 계열 전체를 배제 |

★교훈:★ 4·7 은 기구를 ***증상 하나로 시험하고 전체를 닫은*** 것이다. 증상이 둘 이상일 수
있다는 것을 먼저 확인해야 한다("전 띠가 같이 멈칫하나?" 가 그 질문이었고, 그것을 묻는 데
며칠이 걸렸다).

---

## 6. 장기 대안

Chromium 방식(3-4 절): `PaintAnimationProperty` 에 `period`/`iterations`/`direction` 을
실어 보내고 paint 가 `elapsed % period` 로 감는다. 재샘플이 사라지므로 Task 1·2 가 통째로
불필요해지고 런웨이 개념 자체가 없어진다. ★와이어 포맷 확장이라 더 큰 변경★이다.

---

## 7. 검증

★개발 장비 시험(기본 설정 = divisor 15, FrameTime on, enabled true):★

```
ticker_slice_probe.html?speed=10               A·B 모두 사라져야 한다
ticker_slice_probe.html?speed=10&dir=alternate 양 끝 모두 깨끗해야 한다
```

벽에서는 리플로 비용이 개발 장비보다 무거우므로 따로 확인한다. 참고로
`gfx_paint_side_animations_enabled=false` 는 ★완화책이지만 공짜가 아니다★ — paint 가
재생하지 않는 애니메이션은 divisor 와 무관하게 매 프레임 틱하므로(`prefs.rs:535`) 리플로가
초당 122 로 돌아간다(`log_ani_perf/13`).

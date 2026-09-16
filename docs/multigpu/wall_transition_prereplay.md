# 전환 대상 구성이 먼저 한 번 재생되는 현상 (브랜치 `wall-transition-flicker`)

기준 브랜치: `wall-animation-end-black-frame`
상태: **원인 규명 완료. 엔진 쪽 수정 대상이 아니다.**

---

## 1. 증상 (사용자 보고 원문 기준)

정상이라면:

1. next(전환 대상 구성)의 영상 src 준비가 끝나고
2. next에 입장 애니메이션, prev(표출 중인 구성)에 퇴장 애니메이션이 **동시에** 재생되고
3. prev의 영상 src가 해제된다

관측된 것:

1. next의 영상 src 준비가 **시작**되고
2. ★next의 준비된 일부가 prev 표출 중에 **퇴장 애니메이션이 적용된 채** 등장해 화면 안에서
   밖으로 빠져나가고★
3. 이후 next가 모두 준비되면 그때 2)의 정상 전환이 일어난다

2)에서 보이는 것은 **곧바로 준비되는 것들** — 단일 이미지, 그리고 output 페이지 로드 전에
이미 enumeration이 끝난 캡처카드 포트. video는 아직 없어 그 자리가 검은 사각형으로 함께
빠져나간다(초록 수정 전에는 초록이었다 — `wall_video_neutral_plane` 참고).

next가 WebGL 컨텐츠일 때 "입장 애니메이션 직전에 프레임이 끼어드는" 현상도 같은 사건이다.

---

## 2. 확정된 사실

### 2-a. 들어올 컨테이너가 2~3초 먼저 혼자 퇴장 방향으로 한 번 슬라이드한다

`PAINTANIM painter ... transforms=[...]` 에 바인딩 키를 붙여(커밋 `9c0e950d612`) 값을
컨테이너에 귀속시키자 바로 드러났다(log_ani_debug/20, 가상 뷰포트 폭 11520):

```
16:05:45  a8abb52d:426.3                      <- 혼자, 중앙에서
16:05:46  a8abb52d:6191.4                     <-   오른쪽 밖으로 (퇴장 방향)
16:05:47  (없음)
16:05:48  d7711171:5523.2,  a8abb52d:-1230.0  <- 진짜 전환
16:05:49  d7711171:11288.6, a8abb52d:18.8     <-   d7711171 퇴장 / a8abb52d 중앙 진입
```

한 런에 같은 패턴이 세 번 반복됐다(`16:05:59~16:06:02`, `16:06:17~16:06:20`).

### 2-b. 그 한 번은 **같은 애니메이션의 재시작**이다

```
16:05:40  ANIMSTART   sd-anim-5  node=5825317741960  started_at=17.050
16:05:44  ANIMUNBIND  node=5825317741960  animations=[sd-anim-5:Finished/fill=Forwards]
16:05:44  ANIMSTART   sd-anim-5  node=5825317741960  started_at=22.026   <- 재시작
16:05:45~46  (위 2-a의 혼자 슬라이드)
16:05:47  ANIMCANCEL  sd-anim-5 (name_gone)  +  sd-anim-8/9 시작        <- 진짜 전환
```

`ANIMUNBIND` 를 부르는 자리는 `node.rs`의 **`complete_remove_subtree`** — DOM에서 요소가
실제로 제거되는 경로다. 제거로 애니메이션이 취소되고, 요소가 다시 붙으면서 같은
`animation-name` 이 그대로 적용되어 있어 **새 애니메이션이 처음부터 시작**된다.

### 2-c. ★Chrome이 정상인 것은 이 경로를 검증하지 않는다★

`frontend/packages/shared/ui/lib/cssAnimation.ts` 첫머리:

> 표출 엔진(Servo)에는 `Element.animate`가 없다. … **네이티브가 있으면 손대지 않는다.**
> Chrome은 지금까지와 같은 경로로 돌아, 검증된 동작과 정확한 취소를 그대로 쓴다.
> **위험은 엔진 쪽 경로에만 둔다.**

```js
export function animateEl(el, frames, opts) {
  if (typeof el.animate === 'function') { /* WAAPI */ }
  return cssAnimate(el, frames, opts)   // Servo 만 여기로 온다
}
```

**Chrome은 WAAPI, Servo는 CSS `@keyframes` 폴백으로 서로 다른 코드를 돈다.** "Chrome에서
정상"은 이 폴백을 검증해 주지 않는다 — Chrome은 그 코드를 실행하지 않는다.

### 2-d. 폴백이 `fill: forwards` 에서 인라인 `animation` 을 의도적으로 남긴다

`cssAnimation.ts`:

```js
const name = `sd-anim-${(seq += 1)}`
el.style.animation = `${name} ${durationMs}ms ${easing} 0s ${count} normal ${fill}`
...
const clean = (force) => {
  const holding = (fill === 'forwards' || fill === 'both') && el.isConnected
  if (!force && holding) return    // 지우면 마지막 상태가 풀려 원래 자리로 튄다
  el.style.animation = ''
  style.remove()
}
```

그리고 `widgets/output-render/handover/useBoardHandover.ts`:

```js
animateEl(el,      entranceCssFrames(...), { durationMs, fill: 'none' })      // 들어오는 판
animateEl(leaving, exitCssFrames(...),     { durationMs, fill: 'forwards' })  // 나가는 판
```

★**퇴장만 `fill: forwards` 다.**★ 그래서 퇴장이 끝난 뒤에도 그 요소에는
`style.animation = "sd-anim-N … forwards"` 와 `<style>` 규칙이 **남는다**(최종 위치를 붙들기
위한 의도된 동작이다). 그 요소가 다음 판으로 재사용되거나 DOM에서 이동하면, 남아 있던
**퇴장 키프레임이 그대로 다시 재생된다** — 2-a에서 본 "혼자 퇴장" 이다.

WAAPI에는 이 문제가 없다. 애니메이션이 스타일 속성으로 남지 않고 객체로 관리되며,
DOM 이동으로 재시작되지 않는다.

---

## 3. 왜 엔진 수정 대상이 아닌가

요소가 문서에서 제거되면 애니메이션이 끝나고, 다시 삽입되면서 `animation-name` 이 적용되어
있으면 새 애니메이션이 시작된다 — **CSS 명세상 정상 동작이다.** 엔진이 이를 막으면 명세
위반이고, 실제로 그렇게 막으려다 회귀를 냈다(§5 참고).

근본 해법은 둘 중 하나다.

- **엔진**: `Element.animate`(Web Animations API)를 구현한다. 그러면 폴백 자체가 필요 없어지고
  Chrome과 같은 경로를 돈다. 폴백이 존재하는 이유가 이것뿐이다.
- **페이지**: 폴백 경로에서 `fill: forwards` 대신 최종 상태를 명시적 transform으로 굳히고
  `animation` 속성을 걷어낸다. 그러면 남은 키프레임이 재생될 여지가 없다.

★이 저장소의 제약: 수정은 엔진에 한한다. `frontend`/`backend` 는 **참고용이며 수정 금지**다
(열람은 제약이 아니다 — 이 문서의 §2-c/2-d가 그 열람으로 나왔다).★

---

## 4. 이 조사에서 함께 고친 것 (엔진 쪽 진짜 결함)

증상 2를 쫓는 과정에서 나온 것들이고, 모두 실측으로 확정됐다.

| 커밋 | 내용 | 근거 |
|---|---|---|
| `3a79a9fcc22` | `Invalid` 외부 이미지가 초록으로 그려지던 것을 중립 텍스처로 | `Invalid ext-image` 11,307 → 20 |
| `002647b8248` | 플레이어 해체를 단일 스레드 → 풀 4개 | rtsp 해체 최대 1442 → 247ms |
| `7d56bda7af0` | 끝난 애니메이션을 세트에 남긴다 | **애니메이션 종료 후 검은 화면 해소** |

`7d56bda7af0` 은 `fill: forwards` 의 최종 값이 버려지던 진짜 결함이었다. 같은 수정으로
재시작 반복이 4회 → 2회로 줄었고, 남은 2회가 §2-b의 DOM 제거·재삽입이다.

---

## 5. ★반증된 가설 — 다시 하지 말 것★

전부 실측으로 잘렸다. 공통 원인은 **증상의 정의를 잘못 잡은 것**이다 — "등장 직전 한두
프레임"(`unbound_dls` 1~2)을 쫓았는데 실제는 "끝난 뒤 통째로 한 번 더 재생"이었다.

| 가설 | 반증 |
|---|---|
| 예측 원점을 도착 시각에 박아 위상이 튄다 (`e81236c6e8a`) | 그룹 고정 A/B에서 fps·`reflow_ms` 동일. 수정은 옳으나 이 증상과 무관 |
| `Pending` 승급이 디스플레이 리스트보다 늦다 (`5679427ad7b`) | `get_property_declaration_at_time` 이 `Pending` 을 `Running` 과 동일 취급한다(animation.rs:845). 값은 이미 나오고 있었다 |
| 폴백 타이머가 렌더링 갱신을 두 배로 돌린다 | 하한을 줘도 `reflow_display` mean 64.7 → 62.4(노이즈). 계측해 보니 비율 중앙값은 1.00 |
| 애니메이션이 아직 없는 요소를 안 그리면 된다 | ★회귀★ — 끝난 뒤 세트가 정리된 상태와 구분이 안 돼 검은 화면이 **모든 구성에서 빈발**. 즉시 되돌림 |
| `OpaqueNode` 주소 재사용으로 남의 애니메이션을 물려받는다 | 애니메이션을 받는 노드가 전체 런에 3개뿐이고 각각 10개씩 이름을 받는다 — 두 슬롯 컨테이너의 정상 교대 |

### ★반복한 방법론 오류★

1. **증상의 정의를 사용자와 맞추기 전에 지표부터 골랐다.** `unbound_dls` 1~2가 90건 중
   75건이라는 것에 매달렸는데, 그 분포는 이 증상과 무관했다. 사용자가 3-2로 순서를 풀어
   설명해 준 뒤에야 "끝난 뒤 한 번 더"라는 것이 드러났다.
2. **"Chrome에서 정상이므로 페이지는 옳다"를 전제로 썼다.** 페이지가 엔진에 따라 다른
   코드를 돈다는 것을 확인하지 않았다(§2-c). 그 전제 위에서 세운 가설은 전부 빗나갔다.
3. **"페이지 수정 금지"를 "페이지 열람 금지"로 오독했다.** 원문은 "참고용이며 건드리면 안
   된다"이다. 열람했다면 §2-c/2-d가 훨씬 일찍 나왔다.

---

## 6. 계측 (이 브랜치에 들어 있음)

| 태그 | 무엇을 답하나 |
|---|---|
| `PAINTANIM painter … transforms=[key:x/y@scale]` | ★컨테이너별 위치 시계열.★ 키는 노드 해시로 고정이라 `ANIMSTART node=` 와 맞출 수 있다 |
| `PAINTANIMEDGE edge=first` | 한 번도 묶인 적 없는 노드의 첫 디스플레이 리스트. `was_bound` 조건이 새로 등장하는 요소를 통째로 가리고 있었다 |
| `ANIMUNBIND` | `complete_remove_subtree` 로 요소가 빠질 때. §2-b가 이 줄로 갈렸다 |
| `MEDIATEARDOWN disposed … ms=` | 해체 실제 소요. stream_type별로 갈린다 |
| `update_reasons=[renderer-tick, fallback-timer]` | 렌더링 갱신을 누가 깨웠는지 |
| `Invalid ext-image` (webrender) | WR이 초록을 칠한 횟수. 중립 텍스처 수정의 판정 지표 |

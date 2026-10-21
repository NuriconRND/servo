# 표출 페이지 회전 애니메이션의 화면 케이던스 (`log_ani_debug_05/09~11`)

기준 브랜치: `wall-sample-index-slip`
빌드: `17a26e5ed26`(09·10), `25eb69bdf0d`(11)
상태: **원인 절반 확정. 1차 조치는 프런트 쪽이고, 엔진 쪽에 남은 의문이 둘 있다(6 절).**

조사 대상 페이지: `https://192.168.1.86:7245/IPWALL/#/OUTPUT`
(저장본이 `log_ani_debug_05/09/IPWall_rotation.html` 에 있다. 번들과 프런트 원본을 둘 다 읽었다.)

---

## 1. 증상 (운영자 보고 원문)

- **09** -- 화면 중앙에 FHD 30fps 영상이 재생되면서(표출 크기는 4K) 360도 회전을 반복.
  영상은 30fps 를 유지하는 것처럼 보이나 ★회전 애니메이션이 60fps 로 관찰되는 것은
  `DurationSec` 구간 중 극히 일부였음★
- **10** (비디오 대신 이미지 1개) -- 예상보다 jutter 가 빈번하게 발생해서 60fps 였는지는
  확신할 수 없으나 video 재생 중의 경우보다는 확실히 부드러운 애니메이션을 볼 수 있었다
- **11** (`dom_web_animations_enabled=false`) -- 애니메이션 재생 상태는 육안상 변화 없음

---

## 2. 파이프라인은 어디도 막혀 있지 않다

| | 09 비디오 | 10 이미지 | 11 비디오 + WAAPI off |
|---|---|---|---|
| composites/s (avg) | 59.8 | 57.5 | 59.1 |
| ├ 스크립트 `painter.rs:3826/SCENE` | 32.9 | 56.9 | 32.9 |
| ├ 비디오 `painter.rs:1565/SCENE` | 26.2 | -- | 26.1 |
| └ `painter.rs:2854/ANIMATED_PROPERTY` | **0.3** | **0.5** | **없음** |
| `ANIMWIN took_anim / took_script` | 0 / 61 | 0 / 58 | 0 / 58 |
| `SCRIPTBUSY reflow_display` | 60~61 | 60~61 | 59~60 |
| `SCRIPTBUSY busy_ms` | 11.1 | 10.5 | 10.0 |
| `MAINBUSY busy` | ~13% | ~13% | 13~24% |
| `WRRATE frames / onscreen / superseded` | 60 / 60 / 0 | 61 / 61 / 0 | 60~61 / 동일 / 0 |

스크립트 스레드는 1% 쓰고 있고, 디스플레이 리스트는 세 런 모두 초당 60~61 개가 만들어지며,
WebRender 가 만든 프레임은 전부 화면에 나가고 버려진 것이 0 이다. ★속도가 모자란 곳은 없다.
어긋나는 것은 어느 프레임이 어느 순간에 나가느냐다.★

---

## 3. ★`ANIMREADY` 는 회전을 재지 않는다★ -- 11 번 런이 확정했다

`anim_ready_pending_x` 는 `animated_property_frame` 일 때만 채워진다
(`components/paint/painter.rs:1527`). 즉 `ANIMREADY`·`ANIMSTEPSEQ` 의 `n`/`dt`/`dx` 는
★페인트측 애니메이션 전용★ 이다.

11 번 런은 09 에서 `dom_web_animations_enabled=false` 한 줄만 바꾼 것이다. 결과:

| | 09 | 11 |
|---|---|---|
| `ANIMREADY` 줄 수 | 16 | **0** |
| `ANIMSTEPSEQ` 줄 수 | 4 | **0** |
| `2854/ANIMATED_PROPERTY` | 0.3/s | **없음** |
| 스크립트·비디오 합성 분담 | 32.9 / 26.2 | **32.9 / 26.1** |
| 회전 육안 | 떨림 | **변화 없음** |

⇒ 09·10 의 `ANIMREADY n=20~33` 은 회전이 아니라 ★페이지가 `Element.animate` 로 건
전환 효과★ 였다. 회전은 이 계측에 한 번도 잡힌 적이 없다.

★스크립트가 인라인 스타일로 미는 변환의 화면 케이던스를 재는 계측은 엔진에 없다★(6 절).

---

## 4. 회전의 실제 구동 -- 프런트 원본에서 확인

`F:\20260609_SDWall_BrowserTest\frontend` (참조 전용) 을 읽었다.

### 4-a. 회전은 매 프레임 JS 가 다시 계산해 인라인으로 쓴다

`packages/widgets/output-render/composite/sequenceClock.ts`:

```ts
export function regionPose(r: RenderSubItem, localMs: number) {
  const srt = samplePose(r.anim, localMs, baseFromQuad(r.quad, r.rotation))
  return composeEntrance(r.preset, srt, localMs)        // <- JS 로 보간
}

export function useSequenceClock(total: number, loop: boolean, started = true): number {
  const [t, setT] = useState(0)
  const step = (now: number) => { ...; setT(...); raf = requestAnimationFrame(step) }
  ...
}
```

`samplePose` 는 키프레임 둘을 찾아 `rotation` 을 lerp 한 뒤 `transform: rotate(Ndeg)` 로
나간다. 즉 ★rAF -> `setState` -> 전체 리렌더 -> 인라인 스타일★ 이 매 프레임 돈다.
문서 주석에 적힌 설계 의도는 "씬 렌더링은 이 시계 t 의 순수 함수" 다.

그래서 **엔진의 페인트측 애니메이션 경로는 이 회전을 구조적으로 실어 나를 수 없다.**
> ★낡음(2026-10-21 기록, 같은 날 반증 — 반증 쪽이 아직 미커밋이라 무효 커밋 불명)★ — 디스플레이 리스트는 별개 트랜잭션(painter.rs:3767)이라 비디오가 낸 합성도 최신 각도를 집는다.
스크립트가 만들지 않은 합성(이 페이지에서는 비디오가 26.2/s)은 ★같은 각도를 다시 표출★ 한다.

### 4-b. ★CSS keyframes 대체 경로는 없다★ -- 2026-09-29 에 프런트가 걷어냈다

`packages/shared/ui/PROPOSAL-waapi-only.md`:

> `animateEl` 의 CSS 대체 경로를 걷어낸다. 애니메이션은 **Web Animations API(WAAPI)로만** 돈다.
> 규칙 2: `el.animate` 가 없으면 `animateEl` 은 아무것도 하지 않는 핸들을 돌려준다.
> `<style>` 도 만들지 않고 인라인 `animation` 도 쓰지 않는다.

`packages/shared/ui/lib/animation.ts:62`:

```ts
export function animateEl(el, frames, opts): RunningAnimation {
  ...
  if (!canAnimate(el)) return NOOP        // canAnimate = typeof el.animate === 'function'
  const native = el.animate(frames, { duration, easing, iterations, fill })
```

번들에도 `insertRule` 0 회, `style.animation` 대입 없음, `@keyframes` 는 tailwind `spin` /
radix `enter`·`exit` / sonner 뿐으로 전부 UI 위젯용이다.

⇒ `dom_web_animations_enabled=false` 는 "CSS keyframes 팔" 이 아니라 ★"효과 끄기 팔"★ 이다.
`Element.animate` 를 쓰는 곳은 wall 전체 전환 · 아이템 표출/종료 · 슬롯 전환 · 자막 넷뿐이고,
★영역 회전은 거기 없다★.

### 4-c. 엔진 쪽 플래그가 무엇을 가리는가

`[Pref="dom_web_animations_enabled"]` 가 `Element.animate`
(`components/script_bindings/webidls/Element.webidl:205`) 와 `Animation` 인터페이스
(`.../Animation.webidl:10`) 를 가린다. 구현은 `2169e8f66cb`, `0f7829f0356`.

---

## 5. ★페인트측 경로 자체는 멀쩡하다. 그런데 이 페이지에서는 그것도 떨린다★

### 5-a. 단독 프로브에서는 완벽하다 (`log_ani_debug_05/02`, 2026-10-19)

`wall_anim_jitter_probe` 를 divisor 다섯 값으로 돌린 결과(painter 4, 마지막 창):

| divisor | n | one | skip | dt p50 | dt p95 | dt max | dx | spread |
|---|---|---|---|---|---|---|---|---|
| 1 | 61 | 60 | 0 | 16.59 | 18.41 | 21.55 | 38.40 | 0.000 |
| 4 | 60 | 60 | 0 | 16.73 | 18.36 | 23.42 | 38.40 | 0.000 |
| 15 | 60 | 60 | 0 | 16.64 | 18.26 | 20.70 | 38.40 | 0.000 |
| 30 | 61 | 61 | 0 | 16.69 | 17.71 | 23.72 | 38.40 | 0.000 |
| 60 | 61 | 61 | 0 | 16.78 | 17.91 | 20.88 | 38.40 | 0.000 |

★`gfx_paint_side_animation_tick_divisor` 는 화면 케이던스에 영향이 없다.★ 당연하다 --
이 값은 스크립트 틱만 묶고(`components/paint/refresh_driver.rs:230`) 화면 값은 페인트측
예측이 매 프레임 채운다. 지금 운영 값 15 는 그대로 두면 된다.

### 5-b. 같은 경로가 IPWall 위에서는 무너진다 (09)

09 의 `ANIMSTEPSEQ`(painter 3) `dt_ms` 앞부분:

```
16.7 16.7 16.8 16.8 16.7 16.7 18.7 18.7 15.3 15.3 17.2 17.2 16.0 16.0 18.3 18.3 15.0 15.0
124.2 124.2  3.8 3.8  16.9 16.9  5.3 5.3  17.4 17.4  31.9 31.9  11.5 11.5 ...
```

9 스텝은 정확히 한 프레임 간격으로 가다가 ★124.2 ms 멈췄다가 3.8 ms 만에 따라붙는다★.
`dx_px` 쪽도 같은 자리에서 128.0 -> 585.9 로 튄다 -- 값은 시간 기준으로 맞게 전진했고
★전달만 늦었다★. `ANIMREADY` 요약: `dt p50=18.23 p95=56.35 max=129.79, skipN=5`.
`lat_ms p50=0.17 p95=4.17 max=6.16` 이라 "값이 준비된 뒤의 지연" 은 아니다 --
★그 124 ms 동안 나간 프레임들이 새 값을 안 실었다★.

⇒ 이 페이지에서는 ★rAF 회전과 페인트측 WAAPI 효과가 ***둘 다*** 떨린다.★ 그러므로
"회전을 WAAPI 로 옮긴다" 는 방향은 맞지만 ★그것만으로 충분하다는 증거는 아직 없다★.

---

## 6. 남는 의문 둘 (엔진 쪽)

### 6-a. 프로브는 60/60 인데 IPWall 은 skipN=5 다 -- 무엇이 다른가

같은 페인트측 경로, 같은 divisor, 같은 빌드다. 다른 것은 페이지의 부하다(초당 61 회
전체 문서 리플로 + 4K 비디오 합성). 이것이 2 절의 "속도는 남는데 타이밍이 어긋난다" 와
같은 현상인지, 별개인지 아직 갈리지 않았다.

★판정법★: 프런트가 회전을 WAAPI 로 옮긴 뒤 같은 페이지에서 `ANIMREADY` 를 본다.
`n≈60, one≈60, skip 0` 으로 가면 원인은 매 프레임 리렌더였고 끝이다. 여전히 `skipN` 이
남으면 ★두 번째 원인이 있고 그때는 프로브와 IPWall 의 차이가 다음 표적★ 이다.

### 6-b. 리플로 한 번의 비용이 4.5 배 차이 난다

| | 09 | 10 | 11 |
|---|---|---|---|
| `reflow_display` | 61 | 61 | 60 |
| `reflow_ms` | **47.1** | **46.4** | **10.3** |
| 리플로 1 회당 | 0.77 ms | 0.76 ms | **0.17 ms** |

같은 횟수인데 11 이 4.5 배 싸다. 변수가 ★둘 묶여 있다★ -- `dom_web_animations_enabled=false`
와 빌드 변경(`17a26e5ed26` -> `25eb69bdf0d`). 전자라면 "WAAPI 애니메이션이 존재하는 것만으로
리플로마다 0.6 ms" 라는 뜻이고 그건 그대로 둘 수 없는 비용이다. 후자라면 무관하다.

★판정법★: `25eb69bdf0d` 한 빌드에서 `dom_web_animations_enabled` 만 true/false 로 A/B.

### 6-c. 계측 공백

스크립트가 인라인 스타일로 미는 변환의 화면 케이던스를 재는 수단이 없다. 6-a 의 판정이
"WAAPI 로 옮긴 뒤" 를 전제로 하는 것도 그 때문이다. 프런트가 옮기지 못하는 사정이 생기면
★그 계측부터 만들어야 한다★ -- 그 전에는 회전에 대한 모든 판단이 육안이다.

---

## 7. 프런트에 넘긴 것

`docs/multigpu/wall_output_animation_frontend_handoff.md` 로 따로 정리했다.
요지는 셋이다.

1. `Element.animate` 가 엔진에서 ★실측으로 동작 확인★ 됐다 -- 그쪽
   `DECISION-video-sync.md` 의 "미실측" 행을 갱신할 것.
2. 플래그로 CSS keyframes 와 WAAPI 가 갈린다는 이해는 2026-09-29 이후로 사실이 아니다.
3. `r.anim` 의 SRT 키프레임을 `samplePose` 로 매 프레임 보간하지 말고
   `animateEl(el, frames, { durationMs, iterations: Infinity })` 로 한 번 걸 것.
   변환기(`useEntranceAnimation.ts:39~105`)도 `iterations` 옵션도 이미 있다.

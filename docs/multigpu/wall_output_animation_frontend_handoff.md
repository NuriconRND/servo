# 표출 애니메이션 -- 엔진팀 -> 프런트 전달 (2026-10-21)

대상: `packages/widgets/output-render`, `packages/shared/ui`
엔진 빌드: `25eb69bdf0d` (4-GPU 2x2 벽, 36 타일, 60Hz)
측정 원본: `log_ani_debug_05/09`(비디오+회전), `/10`(이미지+회전), `/11`(09 에서 WAAPI 만 끈 것)

---

## 0. 세 줄 요약

1. ★`Element.animate`(WAAPI) 는 이 엔진에서 동작한다 -- 실측됐다.★
   `DECISION-video-sync.md` 의 "미실측" 행을 갱신해 주세요.
2. ★`dom_web_animations_enabled` 로 CSS keyframes 와 WAAPI 가 갈리지 않습니다.★
   2026-09-29 `PROPOSAL-waapi-only.md` 로 CSS 경로를 걷어낸 뒤로는 "WAAPI 아니면 없음" 입니다.
   운영자가 그 플래그를 false 로 주고 돌린 런은 "CSS 팔" 이 아니라 ★"효과 끄기 팔"★ 이었습니다.
3. ★표출 영역 회전이 떨리는 1 차 원인은 `useSequenceClock` 의 매 프레임 리렌더입니다.★
   제안은 4 절.

---

## 1. `Element.animate` 실측 결과

`DECISION-video-sync.md` 엔진 표의 현재 값:

> | `Element.animate`(WAAPI) | 엔진 쪽에서 추가했다고 전달받음(2026-09-29, 미실측). 없으면 효과 생략 |

갱신할 수 있는 실측값입니다.

| 항목 | 결과 |
|---|---|
| `Element.animate` | **동작**. `Animation` 인터페이스, `cancel`, `delay`, `direction`, `iterations`, `fill` |
| 끄는 법 | `--pref dom_web_animations_enabled=false` -> `typeof el.animate !== 'function'` 이 됩니다 |
| 엔진 내부 경로 | ★WAAPI 애니메이션은 엔진의 "페인트측 애니메이션" 경로를 탑니다★ |

마지막 줄이 중요합니다. 엔진에는 두 가지 애니메이션 구동 방식이 있습니다.

- **스크립트 구동** -- 페이지가 매 프레임 스타일을 바꾸고, 그때마다 문서 전체를 다시 계산합니다.
- **페인트측 구동** -- 애니메이션을 한 번 등록해 두면 ★렌더 스레드가 매 프레임 값을 직접
  전진시킵니다★. 스크립트는 초당 4 번만 깨어나 시각을 다시 맞춥니다. 문서 재계산이 없습니다.

`animateEl` 로 건 것은 전부 후자로 들어갔습니다(09 런에서 확인). `dom_web_animations_enabled=false`
로 끄니 그 경로의 계측이 전부 0 이 됐습니다(11 런). 즉 ★`animateEl` 은 벽에서 "싼 경로" 입니다.★

참고로 같은 표의 CSS `@keyframes`·`transition` 행("동작, 중간값 정상")도 여전히 맞습니다 --
그 경로를 안 쓰기로 한 것은 그쪽 설계 결정이고 엔진 제약이 아닙니다.

---

## 2. 회전이 떨리는 이유

운영자 보고:

> (비디오 + 회전) 영상은 30fps 를 유지하는 것처럼 보이나 회전 애니메이션이 60fps 로
> 관찰되는 것은 구간 중 극히 일부였음
> (이미지 + 회전) jutter 가 빈번하나 video 재생 중보다는 확실히 부드러웠다

측정값입니다. 숫자는 초당, 타일 하나 기준입니다.

| | 비디오 + 회전 | 이미지 + 회전 |
|---|---|---|
| 화면에 나간 합성 | 59.8 | 57.5 |
| ├ ★스크립트가 일으킨 것★ | **32.9** | **56.9** |
| └ 비디오가 일으킨 것 | 26.2 | -- |
| 디스플레이 리스트 생성 | 61 | 61 |
| 스크립트 스레드 점유 | 1.1% | 1.1% |

읽는 법:

- ★페이지는 두 경우 모두 초당 61 개의 장면을 제대로 만들고 있습니다.★ 느린 곳은 없습니다.
- 비디오가 있으면 합성의 **절반(26.2/s)** 을 비디오가 일으킵니다. 그런데 회전 각도는
  > ★낡음(2026-10-21 기록, 같은 날 반증 — 반증 쪽이 미커밋이라 무효 커밋 불명)★ — 비디오가 낸 합성도 최신 씬을 집는다(painter.rs:3767). 프런트에 정정 전달 필요.
  스크립트가 `setState` 할 때만 바뀌므로, ★비디오가 일으킨 합성은 직전과 같은 각도를
  다시 그립니다★. 그래서 비디오가 있을 때 더 나빠집니다 -- 운영자 관찰과 일치합니다.
- 비디오를 빼도 완전히 매끄럽지는 않습니다. 61 개를 만드는데 어느 것이 어느 순간
  나가느냐가 고르지 않기 때문입니다.

근본은 이것입니다.

```ts
// packages/widgets/output-render/composite/sequenceClock.ts
export function useSequenceClock(total, loop, started = true): number {
  const [t, setT] = useState(0)
  const step = (now) => { setT(...); raf = requestAnimationFrame(step) }   // 매 프레임 setState
}
export function regionPose(r, localMs) {
  const srt = samplePose(r.anim, localMs, baseFromQuad(r.quad, r.rotation))
  return composeEntrance(r.preset, srt, localMs)                          // JS 로 보간
}
```

회전은 ★rAF -> `setState` -> 전체 리렌더 -> 인라인 `transform: rotate(Ndeg)`★ 입니다.
엔진 입장에서 이것은 "스크립트 구동" 이고, 1 절의 싼 경로를 탈 수 없습니다.
`animateEl` 을 쓰는 네 자리(wall 전환 · 아이템 표출/종료 · 슬롯 전환 · 자막)에
★영역 회전은 들어 있지 않습니다★.

---

## 3. 그래서 그 플래그는 회전과 무관합니다

운영자가 `dom_web_animations_enabled=false` 로 돌린 11 번 런은, 09 번에서 그 한 줄만 바꾼 것입니다.

| | 09 (WAAPI on) | 11 (WAAPI off) |
|---|---|---|
| 스크립트 합성 / 비디오 합성 | 32.9 / 26.2 | **32.9 / 26.1** |
| 페인트측 애니메이션 계측 | 있음 | **전부 0** |
| 회전 육안 | 떨림 | **변화 없음** |

`animateEl` 이 `canAnimate` 에서 걸러져 `NOOP` 을 돌려주므로 전환 효과만 사라졌고,
회전은 양쪽 다 rAF 구동이라 그대로였습니다.

---

## 4. 제안

> ★`r.anim` 의 SRT 키프레임을 매 프레임 `samplePose` 로 보간하지 말고,
> `animateEl(el, frames, { durationMs: 주기, iterations: Infinity })` 로 한 번 걸어 둔다.★

이미 있는 것들로 됩니다.

| 필요한 것 | 어디에 있나 |
|---|---|
| `PoseKeyframe[] -> Keyframe[]` 변환 | `packages/widgets/output-render/sync/useEntranceAnimation.ts:39~105` |
| 무한 반복 옵션 | `animateEl` 의 `iterations` (주석에 "무한 마퀴는 `Infinity`") |
| 요소당 하나 보장 | `animateEl` 의 `running` WeakMap |

`useSequenceClock` 은 ★남깁니다★. 씬 시퀀싱(`frameAt`, 전환창)은 초 단위 이벤트라
매 프레임 리렌더가 필요 없습니다. 걷어내는 것은 ★영역 포즈의 per-frame 재계산★ 뿐입니다.

> ★낡음(2026-10-21 기록, 같은 날 반증 — 반증 쪽이 미커밋이라 무효 커밋 불명)★ — 둘째 기대 효과는 성립하지 않는다. 비디오가 낸 합성은 이미 최신 씬을 집고, 남는 이득은 리플로 제거뿐이다.
기대 효과: 초당 61 회의 전체 문서 재계산이 사라지고, 비디오가 일으킨 합성에도
최신 각도가 실려 나갑니다.

### 미리 말씀드리는 걸림돌 셋

1. **`composeEntrance` 와의 합성** -- 지금은 SRT 포즈 위에 표출 효과를 JS 로 겹칩니다.
   `animateEl` 은 요소당 하나만 걸도록 설계돼 있어(`running` WeakMap) 두 애니메이션을
   어떻게 겹칠지 정해야 합니다(래퍼 요소를 하나 더 두는 방법 등).
2. **"시계 t 의 순수 함수" 성질** -- `sequenceClock.ts` 머리 주석의 설계 성질을 영역 포즈에
   한해 포기하게 됩니다. 벽은 문서 하나라 화면 간 위상 문제는 생기지 않지만, 설계 판단이라
   그쪽에서 결정하셔야 합니다.
3. ★**이것만으로 충분하다는 보장은 없습니다.**★ 09 런에서 `animateEl` 로 건 효과 --
   즉 이미 페인트측 경로를 타고 있던 애니메이션 -- 도 같은 페이지 위에서 떨렸습니다
   (정상 16.7 ms 간격으로 가다가 ★124 ms 멈췄다가 따라붙음★). 단독 프로브에서는 같은
   경로가 60/60 으로 완벽합니다. 그러니 IPWall 페이지 특유의 두 번째 원인이 남아 있을
   수 있고, 그건 엔진 쪽에서 계속 봅니다.

### 바뀐 뒤에 저희가 볼 것

같은 페이지에서 엔진 계측(`ANIMREADY`)이 ★초당 60 회, 건너뜀 0★ 으로 가면 원인은
매 프레임 리렌더였던 것으로 종결됩니다. 건너뜀이 남으면 두 번째 원인을 저희가 추적합니다.
어느 쪽이든 측정으로 갈립니다.

---

## 5. 참고 -- 운영자 실행 명령 (11 번 런)

```
.\run_wall_dist.ps1 -Layout wall_layout.multigpu.json -Url "https://192.168.1.86:7245/IPWALL/#/OUTPUT" `
  -IgnoreCertErrors -PageFeatures `
  -Pref dom_webgl2_enabled=true,dom_webgpu_enabled=true,dom_web_animations_enabled=false,`
gfx_precache_shaders=false,gfx_paint_side_animation_tick_divisor=15,media_wrap_init_max_per_frame=2 `
  -DComp surface -RefreshHz 60 -DwmAlign 14 -CommitAlign 8 -SampleLead 1 -DirectRender `
  -DcompParallelCommit -PacingThread -ParallelTiles -PipelineMode uridecodebin3 -DecoderThreads 1 `
  -SinkPacing thread -NumaNode 1 -FrameTime on -NoSyncGroup -DurationSec 120 -PresentCadence `
  -DcompBindProf -AnimStepSeq -FrameReason -WrSlowMs 20
```

`dom_web_animations_enabled` 는 ★기본값이 true★ 입니다. 위 줄에서 빼면 WAAPI 가 켜집니다.

엔진 쪽 조사 기록은 `docs/multigpu/wall_output_animation_cadence.md` 에 있습니다.

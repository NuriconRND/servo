# 페이싱 클럭을 DWM 컴포지터 클럭으로 — 검토와 구현 계획

> 날짜: 2026-10-28 · 대상: `components/paint`, `components/servo/examples/winit_wall`
> 촉발: 테스트 장비 A/B — `-PacingOutput "\\.\DISPLAY1"` 은 전 구간 judder, `-PacingDwmQuery` 는 judder 없음
>
> ★진행★: Task 0~3 완료. 남은 것은 ★벽 런(Task 4)★ 하나다 --
> 개발 장비에서는 컴포지터 클럭이 vblank 와 구분되지 않는다(4 회 재현).
> 상세는 문서 마지막 절 「실측 기록」.

---

## 0. 한 줄

★앵커를 DXGI 출력 vblank 에서 DWM 합성 클럭으로 되돌리는 것이 옳다.★ `DCompositionWaitForCompositorClock`
은 그 앵커를 **질의 없이, 대기 타이머 없이** 주는 유일한 수단이고, 이 리포가 "맞출 격자는 하나"라고
이미 실측해 둔 결론과 정확히 들어맞는다. 다만 ***기본값 전환이 아니라 세 번째 선택지***로 넣고,
도입 전에 ***판정 도구를 먼저 되살려야*** 한다.

---

## 1. 중심 긴장은 해소된다 — `output_grid.rs` 가 자기를 반박한다

검토를 시작할 때 가장 큰 반대 근거는 `output_grid.rs` 모듈 doc 이었다:

> "4-GPU 벽에서 네 모니터의 vblank 는 실측으로 주기의 0.67 에 흩어져 있고 … ★하나의 클럭으로는
> 넷을 만족시킬 수 없으므로★ 출력마다 따로 안다." — `output_grid.rs:5-11`

★같은 파일 119-128 행이 그것을 뒤집는다.★

> "실기(log_ani_debug_02/09·10)에서 네 DComp 디바이스가 **같은** `lastFrameTime` 과 `rate=60.000Hz`
> 를 돌려주는 것이 확인됐다. DWM 은 네 타일을 하나의 데스크톱 합성 패스에서 함께 올린다 --
> 출력마다 다른 것은 합성이 아니라 **스캔아웃 시점**뿐이다. 그래서 맞출 격자는 출력별이 아니라
> **이 하나**다. 이 값이 B1·B2 가 왜 둘 다 아무 효과가 없었는지를 설명한다: 둘 다 출력별 vblank
> 에 맞췄는데, 맞출 대상인 합성이 이미 공통이었다." — `output_grid.rs:119-128`

두 문단은 시기가 다르다. 모듈 doc 의 0.67 흩어짐(+1.17 / +6.57 / −4.63ms, 퍼짐 11.2ms)은
***스캔아웃(패널 vblank) 위상***을 잰 `OUTPHASE` 값이고
(`docs/superpowers/specs/2026-09-17-single-present-clock-design.md:748-760`,
`2026-09-21-per-output-commit-phase-design.md:14-23`), ***커밋 위상이 아니다.***
그리고 저더를 정하는 변수는 커밋 위상 하나다.

**결론:** "하나의 클럭" 은 결함이 아니라 ***요구사항***이다. 모듈 doc 의 첫 문단은 낡았고,
이 작업에서 같이 고쳐야 한다.

★남는 것★: 패널 간 스캔아웃 skew(최대 11.2ms, 타일 이음매의 시간차)는 ***어떤 클럭으로도
풀리지 않는다*** — genlock 이 유일한 수단이다(`2026-09-21-per-output-commit-phase-design.md` §2~§8).
두 효과를 섞어 판정하면 안 된다.

---

## 2. 금지 주석은 이 설계를 판정한 적이 없다

`components/paint/lib.rs:34` 가 `DCompositionWaitForCompositorClock` 을 금지한다. 이력을 보면:

| 사실 | 근거 |
|---|---|
| 이 API 는 이 리포에서 ***한 번도 호출·선언·측정된 적이 없다*** | `git log -S` 가 `d1ee16f67d9`(2026-09-18), `1276de4feb0`(같은 날), `26225f22647`(계측 제거) 셋만 돌려준다. 현재 트리의 문자열은 그 주석 한 줄뿐 |
| 금지의 근거는 "API 가 느리다" 가 아니라 ***"프로세스 싱글턴에 여러 생산 스레드가 줄 선다"*** | `d1ee16f67d9` 메시지: "`gfx_wall_frame_max_pending` 이 1 이라 메인이 한 프레임만 막혀도 생산자가 곧장 뒤에 선다" |
| 금지를 쓸 당시 페이싱은 ***자유 구동 소프트 타이머***였다. 전용 스레드 안은 2026-10-23 에야 나왔다 | `f71e367fc58` |
| 금지는 ***이미 스스로 좁혀졌다*** | `lib.rs:37-48` `wait_primary_vblank` doc: "저쪽 주석의 '대기를 넣지 말 것' 은 생산 스레드에 대한 규칙이고 … 이 함수는 틱을 내는 것이 유일한 일인 전용 스레드 하나만 부른다" |
| `GstSystemClock` 근거 자체도 벽에서 약화됐다 | `docs/multigpu/multigpu_tile_scaling_diagnosis.md:2412-2424` — "공유 클럭 대기 자체는 비용이 아니었다"(clock 12.7 vs none 12.5 ms/프레임). 이득은 "재우는 행위가 동시성을 낮춘 것"에서 왔다 |

★판정★: 금지는 **유추에 기반한 방어적 규칙**이지 기록된 실측이 아니다. 전용 페이싱 스레드
하나가 부르는 설계에는 그 논증이 성립하지 않으며, 이는 `WaitForVBlank` 를 허용할 때 이 리포가
이미 쓴 논리와 같다. ★다만 금지를 깨는 것이 공짜라는 뜻은 아니다 — 5 절의 위험을 보라.★

---

## 3. 실측 (1) 대 (2) 는 이 리포의 위상 항등식으로 설명된다

`docs/superpowers/plans/notes/2026-10-27-log-arm-gate.md:44-72` 의 9 런 회귀:

```
phase_pct(ms)  =  vs_dwmvb  +  t2c_p50  +  0.12ms          잔차 폭 0.084ms
                  ^^^^^^^^
                  앵커로 쓴 출력의 vblank 와 DWM 격자의 위상차
안전대: phase ≳ +0.9ms          함정: −1.5 ~ +0.5ms
```

그리고 `prefs.rs:593-612` 에 적힌 2026-10-26 실측: ★같은 바이너리·같은 명령의 네 런에서 DWM
격자에 붙은 출력이 런마다 달랐다★ — `spike_off` 는 DISPLAY1 0.05ms / DISPLAY22 7.4ms,
`spike_on_03` 은 DISPLAY1 8.2~9.3ms / DISPLAY22 0.07ms. 어긋난 런은 커밋 위상 52% 에 dup 4.025%,
붙은 런은 0.902%.

따라서:

- **(1) `-PacingOutput DISPLAY1`**: `vs_dwmvb` 가 0 이 아니었다 → 항등식이 커밋 위상을 함정 구간
  바깥(또는 안)으로 밀었다. 전 구간 지속된 모양도 "위상은 런 내내 고정(±1.5점)" 이라는 기존 관측과
  같다.
- **(2) `-PacingDwmQuery`**: 앵커가 DWM 격자 자신이므로 `vs_dwmvb ≡ 0` 이다. 항등식의 변동 항이
  사라진다.

★컴포지터 클럭은 `vs_dwmvb` 를 구조적으로 0 으로 만든다★ — 앵커가 DWM 의 클럭 그 자체이므로.
그것이 (2) 의 이점을 가져오면서 (2) 의 비용(매 틱 `DwmGetCompositionTimingInfo` IPC + `thread::sleep`)을
없앤다.

### 세 경로의 비교

| 경로 | 앵커 | 질의 지연 | 대기 타이머 지터 | `vs_dwmvb` |
|---|---|---|---|---|
| `WaitForVBlank` (현 기본) | DXGI 출력 — ★런마다 어느 출력이 맞는지 다르다★ | 없음 | `DwmAlign>0` 일 때만 | ≠ 0, 미지 |
| `DwmGetCompositionTimingInfo` + sleep (`-PacingDwmQuery`) | DWM 격자 ✔ | ★매 틱 IPC★ | ★항상★ | ≡ 0 |
| **`DCompositionWaitForCompositorClock`** | DWM 클럭 ✔ | 없음 | `DwmAlign>0` 일 때만 | ≡ 0 |

세 번째 줄이 비어 있던 칸이다.

---

## 4. 컴포지터 클럭이 **풀지 않는** 것

1. **`-DwmAlign` 은 사라지지 않는다.** 항등식의 안전대가 `≳ +0.9ms` 인데 `vs_dwmvb = 0` 이면
   `phase = t2c + 0.12ms` 이고 이것은 ***함정 구간(−1.5 ~ +0.5)에 들어갈 수 있다.*** 즉 앵커만
   바뀌고 위상 조정은 그대로 필요하다. ★기존 최적값(`-DwmAlign 8`)은 그대로 옮겨오지 않는다 — 다시
   스윕해야 한다.★
2. **틱의 위상이 1차 문서로 보증되지 않는다.** MS 문서는 "컴포지터의 작업 주기는 vblank 에 정렬될
   수도 아닐 수도 있다(워크로드·디스플레이 수에 따라)", 용어집 "Compositor frame 은 반드시 vblank 가
   아니다" 라고 적는다. vblank 나 인테이크 마감 대비 오프셋 수치는 ***문서에 없다.*** 유일한 수치는
   제3자 실측 하나(Uno Platform PR #25000: 단일 120Hz 모니터에서 `DwmFlush` 보다 약 0.9ms 이름).
3. **패널 간 스캔아웃 skew.** 1 절 참조. 11.2ms 는 그대로 남는다.
4. **출력별 선택 불가.** 이 API 에는 출력 인자가 없다. `-PacingOutput` 은 이 경로에서 무의미해진다.

---

## 5. 위험 넷 — 전부 계획에 흡수한다

### R1. ★Windows 11 build 22000+ 전용★ → 정적 링크 금지

MS 문서가 최소 빌드를 22000 으로 적는다. 벽은 Windows 11 Enterprise build 26100
(`docs/multigpu/multigpu_tile_scaling_diagnosis.md:2745`), 개발 장비는 26200 이라 둘 다 해당하지만,
★`#[link]` 로 정적 선언하면 심볼이 없는 Windows 10 에서 프로세스가 ***로드 자체에 실패***한다.★
`components/paint` 는 `servoshell` 도 쓰므로 이것은 포크 전체의 회귀다.

→ **`LoadLibraryW("dcomp.dll")` + `GetProcAddress` 로 런타임 해석.** 이 리포에 선례가 없는
관용구지만(`grep GetProcAddress components ports` → 0 건), 선례가 없는 이유는 Win11 전용 API 를
쓴 적이 없어서다. Chromium·Zed·Uno 가 전부 이 방식을 쓴다.

(참고로 링크 자체는 이미 가능하다 — `winapi` 의 `dcomp` 피처가 `build.rs:176` 에서
`cargo:rustc-link-lib=dcomp` 를 내보내므로 `dcomp.lib` 은 이미 링크 라인에 있다. 문제는 링크가
아니라 ***로드 시점 심볼 해석***이다.)

### R2. ★스핀 전례★ — `DwmFlush` 가 코어 하나를 먹었다

`docs/ai-notes.md:283`, `docs/multigpu/configuration.md:87`: DwmFlush 전용 스레드가 이 프로젝트에서
스핀-웨이트로 코어 하나를 상시 소모했고, 그래서 `gfx_vsync_enabled` 기본값이 off 다.
("제거함" 기록은 ≤40 타일 시점의 것이고 `ai-notes.md:90` 은 이후 최종 레시피에 복원된 것을 보이므로
***실격 사유는 아니지만***, "DWM 계열 블록 대기는 싸다" 는 보장도 이 리포에 없다.)

→ **계획에 페이싱 스레드 CPU 시간 측정을 필수 항목으로 넣는다. 스핀이면 폐기한다.**

### R3. ★업계 선례가 미온적★

- Chromium: `VSyncThreadWinDComp` 가 있으나 `UseCompositorClockVSyncInterval` 플래그 ***기본 꺼짐***.
  주석에 "어댑터 분리 시 WILL hang" → 100ms 타임아웃.
- Zed(GPUI): 채택했다가 2025-10 에 ***철회***(`Use DwmFlush unconditionally for Windows vsync`,
  #39913), 커밋 메시지가 "이 API 에서 여전히 버그를 겪었다" 고 적는다.
- Uno Platform: Win11 에서 사용, Win10 은 `DwmFlush`.

→ **기본값을 바꾸지 않는다. 런처 스위치로 같은 바이너리에서 A/B 한다.**

### R4. ★판정 도구가 없다★ — 이것이 선행 조건이다

계측이 2026-10-28 에 전부 제거됐다(약 20,489 줄). 지금 (1) 과 (2) 의 차이를 ***수치로 재현할 수단이
하나도 없다.*** `vs_dwmvb`, `phase_pct`, `dup%` 전부 사라졌다. 육안만 남았다.

→ **Task 0 이 이것이다.** 위상 하나만 재는 최소 계측을 먼저 되살린다. 그것 없이 이 작업을 하면
"바꿨는데 좋아진 것 같다" 밖에 말할 수 없다.

---

## 6. 구현 계획

**전제:** 새 브랜치. 기본 동작은 바뀌지 않는다. 각 Task 는 독립 커밋이고 커밋 메시지에 수치를 적는다.

### Task 0 — ★판정 도구★ — ✅ 2026-10-28 완료 (`d97a01ee889`)

목표였던 것: 틱 위상 하나를 수치로 말할 수 있게 한다. 들어간 것:

```
-TickPhase   (pref gfx_wall_tick_phase_probe, 기본 꺼짐)

wall: tick phase [30s] p05/p50/p95 = 8.1/8.4/9.2 % 폭 1.1점 R=0.998 (n=1798, 타이머경로 0, 주기 16.667ms)
wall: tick gap   [30s] p05/p50/p95 = 0.99/1.00/1.01 주기 (n=1797, 1.5주기초과 0, 유실 0, 낡음 0, …)
```

| 스레드 | 하는 일 | 치르는 값 |
|---|---|---|
| 페이싱 | 틱마다 QPC 하나를 링에 넣는다 | ★15.9 ns + QPC 한 번★ (실측, 200k 회) |
| 리포터(전용) | 접고, 10 초마다 누적을 찍는다 | 이 스레드가 존재하는 이유 |
| 메인(렌더) | ★아무것도 하지 않는다★ | 0 |

★적대적 검토가 설계를 네 군데 바꿨다 — 셋은 내가 틀렸던 것이다.★

1. ★원래의 "종료 시 한 번 찍기" 는 한 줄도 내지 못했을 것이다.★ 런처가 `-DurationSec` 을
   `Stop-Process -Force`(`TerminateProcess`)로 끝내고(`run_wall_dist.ps1:1015-1019`),
   무제한 런도 같은 방법으로 죽이며(`:352-354`), 엔진에 자체 종료 타이머가 없다.
   `run_app` 이 반환하지 않는다. → 리포터 스레드가 주기적으로 누적을 찍는다.
   ★로그의 마지막 줄이 그 런의 답이다.★
2. ★위상은 원형 변수다.★ 선형 백분위는 틱이 격자점 근처(위상 ≈ 0)에 모일 때 표본이 양 끝으로
   갈려 "폭 99 점" 으로 읽힌다 — ***그리고 그 경우가 바로 우리가 가장 보고 싶은 경우다.***
   → 원형 평균으로 좌표를 돌려 백분위를 뽑고 집중도 `R` 을 같이 찍는다.
3. 접는 일을 메인에 둔 것이 틀렸다. "100ms 면 해상도 아래" 라는 계산에 ★메인(렌더) 스레드
   부하가 빠져 있었다.★ → 리포터 스레드로 옮겨 트레이드오프 자체를 없앴다.
4. DWM 질의 실패 시 백오프가 없어 초당 60 회로 폭주할 수 있었다. → 앵커의 *나이*가 아니라
   *마지막 시도 시각*으로 게이트.

★자기 검증은 타이머 경로에서만 성립한다.★ `-PacingDwmQuery` 런은 `snap_to_dwm_grid_at` 이
DWM 격자의 `-DwmAlign` % 를 겨냥해 자므로 p50 이 그 값 + 깨기 지연(+2~6 점)이어야 한다.
★vblank 경로에는 "나와야 할 값" 이 없다★ — p50 = (X + DwmAlign) mod 100 이고 `X` 가 바로
재려는 미지수(`vs_dwmvb`)다. 그리고 이 검증은 프로브와 `snap` 이 같은 앵커를 쓰므로
***순환***이다: 산술이 맞는지는 말해 주지만 DWM 격자가 합성 마감과 맞는지는 말해 주지 않는다.

★이 프로브가 재지 ***않는*** 것★: 커밋 위상(= 틱 위상 + 파이프라인 지연). 클럭 원천끼리
비교할 때는 파이프라인이 같으므로 틱 위상 차이가 곧 커밋 위상 차이다. 절대값이 필요해지면
그때 `t2c` 를 따로 재야 한다.

### Task 1 — FFI 와 대기 함수

`components/paint/output_grid.rs` (이미 `#![allow(unsafe_code)]`):

```rust
// 런타임 해석 — 이 export 는 Windows 11 build 22000+ 에만 있다(R1).
type WaitFn = unsafe extern "system" fn(u32, *const HANDLE, u32) -> u32;
static WAIT_FN: OnceLock<Option<WaitFn>> = OnceLock::new();
```

`pub(crate) fn wait_compositor_clock(timeout_ms: u32) -> CompositorWait`

- `DCompositionWaitForCompositorClock(0, null, timeout_ms)` 호출 직후 `qpc_now()`.
- ★반환값 해석을 단정하지 않는다★ — MS 함수 페이지는 `STATUS_SUCCESS`(=0), 같은 문서의 샘플은
  `WAIT_OBJECT_0 + count` 라 적어 ***서로 충돌한다.*** `count=0` 이면 둘 다 0 이므로 `0` 을 성공으로
  보고 ***그 밖의 모든 값을 실패로 처리***한다. `STATUS_GRAPHICS_PRESENT_OCCLUDED`(디스플레이 꺼짐)는
  ★즉시 반환★이므로 반환이 1ms 미만이면 백오프해 스핀을 막는다.
- 타임아웃은 100ms(Chromium 선례, 어댑터 분리 시 hang 대비). 타임아웃·실패는 `Unavailable` 로
  돌려 호출자가 기존 경로로 떨어진다.
- 주기는 ***관측 간격의 중앙값***으로 자체 추정한다(질의 없음). 기동 시드만 `dwm_composition_grid()`
  1 회.
- 바인딩 한 줄은 기존 `publish_pacing_binding` 재사용:
  `pacing clock: DComp 컴포지터 클럭 (DWM) 59.94Hz 측정 -- -PacingOutput 는 이 경로에서 무시된다`

`components/paint/lib.rs` 에 `pub fn wait_compositor_clock()` 래퍼 + `cfg(not(windows))` → `None`.
`components/servo/lib.rs` 의 `pub use` 목록에 추가.
★`lib.rs:34` 의 금지 주석과 `output_grid.rs:5-11` 모듈 doc 을 같이 고친다★(1·2 절의 결론).

### Task 2 — 페이싱 스레드의 세 번째 경로

- pref 를 `bool gfx_wall_pacing_vblank` 에서 ***문자열*** `gfx_wall_pacing_clock` 으로 바꾼다:
  `"vblank"`(기본, 오늘 동작) / `"dwmquery"` / `"compositor"`. bool 둘을 더 늘리면 우선순위가
  세 곳에 흩어진다.
- `spawn_pacing_thread` 가 기동에 한 번 읽고 `match` 에 arm 을 더한다. `VblankPace` →
  `PaceOutcome` 로 이름만 바꾸고 `Unavailable` 폴백 계약(실패해도 블록하지 않고 타이머로 낙하)을
  그대로 지킨다.
- `align_after_vblank` 를 재사용한다 — 4 절 1 번 때문에 `-DwmAlign` 은 그대로 필요하다.
- 런처: `-PacingCompositorClock` 스위치. `-PacingDwmQuery` 와 동시 지정은 throw.
  `-PacingOutput` 과 함께면 Write-Warning(핀이 읽히지 않는 세 번째 구성).

### Task 3 — ★메인 게이트를 앵커와 일치시킨다★

`drive_present_clock_timer`(`main.rs:915-932`)는 틱을 받아도 `now >= next` 일 때만 발화하고,
`next` 는 `snap_to_dwm_grid_at` 가 ***DWM 격자***에서 다시 잡는다. 즉 ★페이싱 스레드가 무엇으로
깨든 메인은 DWM 격자로 되돌린다.★

- `-PacingDwmQuery` 에서는 두 경로가 같은 산식이라 항상 통과한다.
- `WaitForVBlank` 에서는 두 앵커가 달라, 틱이 격자점보다 이르면 ***그 발화가 다음 깨우기로
  미뤄진다***(한 프레임 지연). 두 클럭이 드리프트하면 간격이 불규칙해진다.
- 컴포지터 클럭에서도 틱이 `qpcVBlank` 보다 이르면(Uno 실측이 그렇다고 보고한다) 같은 일이 난다.

→ 페이싱 스레드가 클럭을 쥐고 있을 때(`pacing_is_driving()`)는 게이트를 ***최소 간격 검사***로
바꾼다 — "DWM 격자점을 지났나" 가 아니라 "직전 발화로부터 주기의 절반이 지났나". 이중 발화 방지와
몰아치기 방지라는 게이트의 두 목적은 그대로 만족된다.

★이 Task 는 컴포지터 클럭과 독립적으로 가치가 있다 — 실측 (1) 의 증상 일부를 이것이 설명할 수
있고, 그렇다면 현 `WaitForVBlank` 경로의 버그다.★

### Task 4 — 측정

Task 0 의 도구로, 같은 바이너리에서 같은 레이아웃·같은 `-DwmAlign` 스윕:

| 런 | 스위치 |
|---|---|
| A | `-PacingOutput "\\.\DISPLAY1"` (실측 1 재현) |
| B | `-PacingDwmQuery` (실측 2 재현) |
| C | `-PacingCompositorClock` |

각각 `-DwmAlign 0 / 4 / 8 / 16` 네 점. 판정은 ★틱 위상 p50 과 육안★ 둘 다
(`trust-the-eye-before-my-counter`, `visible-judder-is-ticker-entry`). 그리고 각 런의
**페이싱 스레드 CPU 시간**(R2).

벽(4 타일)에서 같은 것을 한 번 더 — 단일 출력 결과는 4 출력으로 일반화되지 않는다.

---

## 7. 답하지 못한 것

1. **컴포지터 클럭 틱이 `qpcVBlank`·`lastFrameTime` 대비 어디인가.** 1 차 문서에 없다. Task 0 의
   도구로 처음 잰다.
2. **테스트 장비에 모니터가 몇 개 붙어 있었나.** (1) 의 `vs_dwmvb ≠ 0` 을 설명하는 가장 싼 가설은
   "DWM 격자가 DISPLAY1 이 아닌 다른 출력에 붙어 있었다" 이고, 그것은 출력이 둘 이상일 때만
   성립한다. ★이미 들어간 `wall: pacing vblank: … (출력 N/M)` 줄의 `M` 이 답을 준다★ — `M = 1` 이면
   이 가설은 죽고, 단일 출력에서도 DXGI vblank 와 DWM 격자 사이에 오프셋 항이 따로 있다는 뜻이 된다.
3. **혼합 주사율 벽에서 클럭이 어느 출력을 따르는가.** MS 문서는 "어떤 모니터의 vblank 또는 다른
   타이머" 라고만 적는다. 사용자 보고 1 건(60Hz primary + 120Hz 보조 → primary 주기만)이 있을 뿐이다.
4. **Zed 가 철회한 "버그" 의 구체 증상.**

---

## 8. 실측 기록 (2026-10-28, 개발 장비 단일 출력)

`-TickPhase` 로 다섯 묶음을 돌렸다. `log_DwmClock/01`~`05`.
모든 런 공통: `wall_layout.singlegpu_local.json`, 3840x1080 타일 하나, 30 초, 60Hz.

### 최종 상태 (`05`, Task 0~3 적용 후)

```
런            위상 p50   폭      발화        거절   차단
A0 vblank     0.1 %      0.6 점  1800 = n    0      없음
B0 dwmquery   1.7 %      2.8 점  1794        8      없음
C0 compositor 0.1 %      0.7 점  1799 = n    0      없음
C8 compositor 9.5 %      1.9 점  1800 = n    0      없음
```

### ★확정된 것 여섯★

1. **신호 경로는 구조적으로 깨끗하다.** 발화 = n, 거절 0. 비교를 하지 않으므로 거절이
   생길 수 없다. 메인(렌더) 스레드의 DWM 질의도 0 이다.
2. ★**래치 자기 유지 고장 -- 45%.**★ `03` 에서 `-PacingDwmQuery` 가 페이싱 발화 989,
   `about_to_wait` 발화 805 였다. 원인은 `note_pacing_tick`(생존 래치)이 게이트 **안**에
   있었던 것: 거절 → 래치 식음 → 백스톱 열림 → `next` 전진 → 또 거절. `04`·`05` 에서
   989 → 1794 로 사라졌다. 커밋 `94a08c2752e`.
3. ★**dwmquery 의 고유 비용(3 회 재현).**★ 위상 p50 +1.7%(283µs), 폭 2.7~2.8 점
   (450µs). 신호 경로는 0.6~0.7 점(100µs)이다. ***그 4 배가 프로세스 간 호출 지연의
   지터다.***
4. ★**`-DwmAlign > 0` 은 위상 폭 약 200µs 를 더한다.**★ C0 0.7 점 → C8 1.9 점,
   R 1.000 → 0.992. `thread::sleep` 자신의 지터이고, `spawn_pacing_thread` doc 이
   정성적으로 적어 둔 것의 수치다.
5. **`차단 [없음]`.** `WindowEvent` 가 게이트 없이 `drive_present_clock` 을 부르는 것은
   ***이론적 위험이었다*** -- 30 초 런에 윈도 이벤트가 1 개뿐이다. 완전 차단은 구멍을
   막은 안전장치이고, 측정 가능한 이득은 없었다.
6. ★**단일 출력에서 compositor ≡ vblank (4 회 재현).**★ 위상·폭·거절이 세 자리까지 같다.

### ★계획이 두 번 고쳐졌다★

- 계획의 Task 3 은 "게이트를 최소 간격 검사로 바꾼다" 였다. 실측은 ***더 작고 더 정확한***
  것을 가리켰다(래치 위치). 그것만으로 `여유부족` 이 0 이 됐다.
- 그 뒤 운영자가 ★구조적 중복★을 짚었다: "신호까지 기다렸다가 깨어나는 수단인데 왜
  격자를 다시 물어 앞뒤를 따지나." 그것이 진짜 Task 3 이었다 -- 증상이 아니라
  ***신호를 받고도 다른 시계에 다시 묻는 것*** 자체가 문제였다. 커밋 `ca8aebd1655`.

### ★틀린 것으로 판정된 가설 둘 (내 것)★

- **"게이트 여유가 모자라다."** 반증. 래치를 고치자 `여유부족` 이 네 런 모두 0 이 됐다.
  `-DwmAlign` 은 게이트 여유를 사지 않는다 -- 커밋 위상만 조정한다. 기존 기록
  ("DwmAlign 0 이 최적")과의 충돌은 ***기존 기록 쪽으로*** 해소됐다.
- **"vblank/compositor 는 질의가 없다."** 절반만 참이었다. 페이싱 스레드에는 없었지만
  메인이 매 발화마다 `DwmGetCompositionTimingInfo` 를 불렀다(`-DwmAlign` 이 `0..99` 인
  모든 런). Task 3 이 그것을 없앴다.

### 남은 것: Task 4 = 벽

★확정 6 번이 이 장비의 한계다.★ 컴포지터 클럭의 명제는 "앵커가 구조적으로 옳다" 이고,
그 명제는 ***앵커가 틀릴 수 있는 곳에서만*** 시험된다. 단일 출력에서는 vblank 가 곧 DWM
클럭의 원천이므로 둘이 같을 수밖에 없다. 4 출력 벽에서는 어느 출력이 DWM 격자에 붙는지가
런마다 다르고(`gfx_wall_pacing_output` doc 의 2026-10-26 실측: dup 0.902% 대 4.025%),
거기서만 차이가 난다.

벽 런 설계: 같은 네 조합(A0/B0/C0/C8)에 `-PacingOutput` 스윕을 더한다. 판정은 틱 위상
p50·폭과 ★육안★ 둘 다(`visible-judder-is-ticker-entry`,
`trust-the-eye-before-my-counter`). 그리고 패널 간 스캔아웃 skew(최대 11.2ms)는 어떤
클럭으로도 풀리지 않으므로 그것과 섞어 판정하지 말 것(1 절).

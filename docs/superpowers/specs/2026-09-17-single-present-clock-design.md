# 표출 클럭 일원화 설계 (2026-09-17)

대상 브랜치: `wall-animation-perf`
관련 조사: 이 문서 §배경의 실측 로그 `log_ani_debug/21~25`

## 목표

winit_wall 이 프레임을 그리는 권한을 **하나로 만든다.** 지금은 타일마다 초당
61~75 번 그려서 60Hz 디스플레이가 고르게 보여주지 못하고, 그 불균일이 등속
애니메이션을 떨리게 만든다.

★1단계는 수정이 아니라 계수다.★ 새는 자리를 특정한 뒤에 고친다.

## 배경 — 무엇이 이미 옳고 무엇이 어긋났나

셸은 이미 단일 클럭 모델을 **선언하고 구현해 두었다.** `main.rs` 의
`present_period`(= `gfx_refresh_hz`)와 `drive_present_clock()` 이 그것이고,
주석이 의도를 명시한다:

> ***The wall draws on a clock, not when content says it is ready.*** 비디오
> 디코드든 WebGL rAF 든 스크립트 애니메이션이든 자기 상태는 아무 때나 갱신하고,
> **클럭이 언제 그릴지 정한다.**

그리고 셸 안에서 `request_redraw()` 를 부르는 곳은 둘뿐이다 — 클럭 틱과
`--capture` 경로. `RedrawRequested` 는 `render_all_tiles()` 하나로 간다.
**셸은 올바르다.**

그런데 실측은 다르다.

| 실측 | 값 |
|---|---|
| 타일별 WRRATE (프로브, 애니메이션 4개) | 평균 66.9, 중앙값 66.0, p75 **72.0**, 최고 **75.1** |
| 60 초과 비율 | 408 표본 중 **304 (75%)** |
| 같은 페이지, 애니메이션 2개 | 평균 60.7 |

`WRRATE frames` 는 `render_impl` 호출 수이고 thread_local 카운터이므로
**타일당 실제 그리기 패스**다. 클럭이 60 을 요청하는데 타일마다 61~75 가
그려지고 있다. 즉 **클럭이 유일한 권한이 아니다.**

애니메이션 개수에 비례해 늘어나는 것이 단서다. 비디오가 있는 output 페이지에서는
애니메이션이 프레임을 거의 만들지 않고(`frames=0~1 rode_along=11~78`) WRRATE 도
56.8 인 반면, 비디오가 없는 프로브에서는 애니메이션이 클럭 노릇을 하며
(`frames=32~44`) 66.9 까지 간다.

### 왜 이것이 떨림으로 보이나

디스플레이는 초당 60 번만 보여준다. 67 개를 만들면 어떤 vsync 구간에는 두 개가
들어가 앞의 것이 버려지고 어떤 구간에는 하나만 들어간다. 화면에 반영되는 변위가
프레임마다 달라진다 — 등속 운동인데 "순간마다 이동하는 정도가 다르고 덜덜
떨린다"는 관측이 이것이다.

★속도와 무관하다.★ 1/16x(프레임당 2.4px)에서도 같은 비율로 불균일하므로 똑같이
떨린다. 실제로 그렇게 관측됐고, 그것이 이 문제를 표본화 한계가 아니라 결함으로
가르는 근거다.

## 사용자 결정 사항

- **범위**: 셸(`components/servo/examples/winit_wall/`)에 한정한다. `paint.rs` 나
  엔진 통지 경로는 건드리지 않는다 — servoshell 과 공유되고, 그 파일에는 같은
  자리에서 낸 회귀 기록이 여럿 있다.
- **순서**: 클럭 기준을 먼저 세우고, 그 위에서 애니메이션 문제를 본다.
- **1단계는 계수**: 원인을 특정한 뒤 고친다.

## 대안 검토

**안 B — `render_all_tiles` 진입부에서 최소 간격 강제(레이트 리밋).** 한 줄이면
되지만 ★위험하다★. 위상이 어긋난 호출이 틱 자리를 먼저 차지하면 간격이 **더**
불균일해진다. `painter.rs` 주석이 기록한 과거 회귀 — 같은 주기 다른 위상의 두
판정이 맞물렸다 어긋나며 초당 10~13 개가 샜고(61→72) "move, stall and jump" 로
보였다 — 가 정확히 이 형태다. 탈락.

**안 A 단독 — 원인을 모른 채 게이트부터 건다.** 증상은 막히지만 정당한
재그리기(OS expose, 리사이즈)까지 막고, 새는 자리가 셸 밖이면 아무 효과가 없다.
아래 1단계를 거치면 A 를 걸어야 할지 아닌지가 결정되므로, 단독으로 택할 이유가
없다. 탈락(1단계 뒤 조건부로 채택).

## 설계

### 1. 1단계 — 계수 한 줄

`main.rs` 에 계수기를 넣고 초당 한 줄을 `info!` 로 낸다(런처의 `RUST_LOG` 에
`winit_wall=info` 가 이미 있다).

```
WALLCLOCK ticks=60 redraw=70 renders=70 suppressed=0 period_ms=16.7
          gap_ms p50=14.3 p95=20.1 max=33.2 off=18(30%)
```

| 필드 | 증가 지점 | 답하는 질문 |
|---|---|---|
| `ticks` | `drive_present_clock()` 이 틱을 발화할 때 | 클럭이 제 주기로 도는가 |
| `redraw` | `WindowEvent::RedrawRequested` 진입 | 요청한 것보다 많이 오는가 |
| `renders` | `render_all_tiles()` 진입 | redraw 하나가 렌더 하나인가 |
| `suppressed` | 2단계에서만 증가(1단계에서는 항상 0) | 얼마나 새고 있었나 |
| `period_ms` | `present_period` | `gfx_refresh_hz` 가 도달했는가 |
| `gap_ms` | `render_all_tiles()` 진입 간격 | **성공 기준 1** |
| `off` | 간격이 `period_ms` ±20% 밖인 개수와 비율 | 균일성 |

`gap_ms` 는 진단용이 아니라 **영구 계측**이다. 성공 기준 1 이 이 값이고, 수정 후
같은 줄로 회귀를 판정한다. 평균이 아니라 분포와 `off` 를 내는 이유는 **평균이
끊김을 정의상 가리기** 때문이다 — 균일한 57fps 와 "60,60,60,20,60" 은 평균이 같다.

`renders` 와 타일별 WRRATE 의 관계가 열쇠다. `render_all_tiles` 한 번이 타일 넷을
그리므로 정상이라면 **타일별 WRRATE ≈ renders** 여야 한다.

### 2. 2단계 — 조건부 수정

1단계 결과가 갈래를 일의적으로 정한다.

#### 분기 1 — `ticks=60` 인데 `redraw>60`

셸 밖에서 들어온다(winit/OS 의 expose·damage). 클럭이 허가한 틱만 그린다:

```rust
// drive_present_clock() 이 틱을 발화할 때
self.present_due.set(true);

// RedrawRequested
if state.present_due.replace(false) {
    state.charge_main(MainSlot::Render, || state.render_all_tiles());
} else {
    state.redraw_suppressed.set(state.redraw_suppressed.get() + 1);
}
```

`present_due` 를 `Cell<bool>` 로 두고 `replace(false)` 로만 소비하면 "세우고 안
지우는" 실수가 구조적으로 불가능하다.

***위험과 그 한계***: OS 가 보낸 정당한 재그리기도 억제된다. 다만 클럭이 매 주기
**무조건** 전 타일을 그리므로 한 주기(16.7ms) 안에 복구된다. 클럭이 멈추면
문제이지만 그건 셸이 이미 따로 다루는 실패 모드다 — `drive_present_clock` 을
`about_to_wait` 과 이벤트 처리 **양쪽**에서 부르는 이유가 그것이고, 주석에 실측된
최악(3.2초 정지, `MAINBUSY window_ms=3239`)이 적혀 있다.

#### 분기 2 — `ticks=redraw=renders=60` 인데 타일별 WRRATE > 60

셸 아래에서 샌다. 먼저 한 패스 안에서 타일이 중복으로 그려지는지 본다 —
`render_all_tiles` 에 타일별 카운터를 붙이면 즉시 갈린다.

중복이 아니라면 `paint.rs` 의 다른 렌더 진입점이 별도로 발화하는 것이고, ★이는
합의한 범위를 벗어난다.★ 그때는 멈추고 범위 확대를 묻는다. 말없이 넘지 않는다.

#### 분기 3 — `ticks>60`

`drive_present_clock` 은 `now >= next` 를 보고 미래 틱까지 건너뛰므로 구조상
과발화가 안 된다. 그렇다면 `present_period` 가 틀린 것이다.

다만 이 갈래는 가능성이 낮다. 셸은 `gfx_refresh_hz` 를 `servo_config::pref!` 로
**직접 읽고** 페인트 타이머와 같은 클램프를 쓴다 — 그 자리 주석이 "one refresh rate
for the machine, not two that can disagree" 라고 명시한다. 120 은 값이 `[1, 1000]`
밖일 때만 적용되는 폴백이므로, 런처가 `-RefreshHz 60` 을 넘기는 한 8.3ms 가 나올
일이 없다. `period_ms` 한 필드로 한눈에 확인된다.

#### 분기 4 — 셸 계수가 전부 정상이고 간격도 균일하다

프레임 과잉 생산은 원인이 아니었고, WRRATE 61~75 를 잘못 읽은 것이다(예: WebRender
가 한 합성에서 서브패스마다 `render_impl` 을 세는 경우).

★이 갈래를 일부러 남긴다.★ 이 증상을 쫓으며 네 번 잘못 짚었고, 그중 한 번은
"찾았다" 고 단언한 타일 간 74px 어긋남이 로그 위상 아티팩트로 판명났다. 가설이
틀렸을 때 갈 곳을 설계에 미리 적어 두는 것이 이번에는 필요하다. 이 경우 떨림은
present 이후 — DComp 커밋/스캔아웃 — 에 있고, §5 와 같은 자리로 수렴한다.

### 3. 검증

1단계 실행이 진단이자 **기준선**을 겸한다. 별도 기준선 런이 필요 없다.

| 기준 | 합격선 | 출처 |
|---|---|---|
| 1. 간격 균일성 (주 판정) | `p50 ≈ period_ms`, `p95 ≤ period_ms × 1.2`, `off` 비율 감소 | `WALLCLOCK` |
| 2. 렌더 수 | 타일별 WRRATE = `gfx_refresh_hz` ±1 | `WRRATE` |
| 3. 육안 | 1/16x 행이 떨지 않음 | `wall_anim_jitter_probe.html` |
| 4. 전환 불변 | `SCRIPTBUSY reflow_ms`·`WRRATE` 가 수정 전과 같음 | output 페이지 |

기준 1 이 주 판정인 이유: **기준 2 가 충족돼도 기준 1 이 깨질 수 있다.** 초당 60 개를
내면서 간격이 8/25/8/25ms 면 수치는 합격이고 화면은 떨린다.

실행 둘, 수정 전후로 같은 바이너리에서:

```powershell
# 프로브 — 비디오 없음, 과잉 생산이 드러나는 조건
.\run_wall_dist.ps1 -Serve -Url wall_anim_jitter_probe.html -NumaNode 1 -DurationSec 60

# output — 회귀 방지선
.\run_wall_dist.ps1 -Url <output URL> -NumaNode 1 -DurationSec 180 -PageFeatures
```

`-NumaNode 1` 은 고정한다. 그룹 0 에 착지하면 측정이 통째로 무효다.

### 4. 단위 테스트

셸의 winit 이벤트 루프라 자동 테스트가 어렵다. 백분위 계산 같은 순수 함수를 떼어낼
수는 있으나 이 규모에 그럴 가치는 없다고 본다 — **계측 자체가 검증 도구**이고 판정은
로그 수치로 한다. 구조로 막을 수 있는 한 가지(`present_due` 의 소비)는 위에 적었다.

### 5. ★이 설계가 못 고치는 것★

과잉 생산을 없애도 **자유 구동 클럭과 디스플레이 vsync 의 위상 문제는 그대로다.**
`gfx_vsync_enabled` 가 기본 꺼짐이라, 소프트 타이머가 정확히 60 개를 내도 60Hz
디스플레이와 미끄러지면서 어떤 vsync 엔 두 개가 들어가고 어떤 vsync 엔 하나도 안
들어간다.

그래서 **`gap_ms` 가 완벽하게 균일해졌는데도 눈으로 떨린다면**, 그때가 vsync/present
페이싱 차례다. 이 작업의 범위가 아니고 분기 4 와 같은 자리로 수렴한다.

이것을 미리 적는 이유는, 기준 1·2 가 합격인데 기준 3 이 불합격일 때 **"수정이
실패했다" 가 아니라 "다음 층으로 내려갈 차례다" 로 읽어야** 하기 때문이다.

## 비목표

`paint.rs` 와 엔진 통지 경로 수정(범위 밖, 분기 2 에서 필요해지면 별도 합의),
vsync 활성화와 present 페이싱(§5), 애니메이션 값 계산 경로 수정(§배경에서 이미
정상으로 확인 — 네 타일이 0.4px 안에서 일치하고 앵커도 0.13ms 이내다),
`gfx_paint_side_animation_tick_divisor` 조정(rAF 15Hz 는 의도된 동작이다).

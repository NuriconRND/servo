# 커밋을 공통 합성 격자 위 고정 위상에 내보낸다 (B3)

**목표:** DComp `Commit()` 이 나가는 시각을 렌더 패스의 길이에서 떼어, DWM 공통 합성
격자 위의 **고정된 위상**에 앉힌다.

**근거 문서:** `docs/superpowers/specs/2026-09-21-per-output-commit-phase-design.md`
§21~27 (2026-10-06 계측). 이 문서는 그 §27 "다음" 을 설계로 옮긴 것이다.

---

## 1. 왜 하는가

커밋은 지금 렌더 패스의 **끝**에서 나간다. `-DwmAlign` 이 고정하는 것은 패스의
**시작**(틱)이므로, 커밋 시각 = `틱 + 패스 길이` 이고 커밋 위상이 패스 길이를 그대로
물려받는다.

측정이 이것을 직접 보여 준다(§23). 커밋 위상의 산포(`DCOMPSTAT commit_lead_ms` 의
p95−p05)와 패스 길이의 산포(`TICKCOMMIT spread`)가 네 실행 모두에서 같은 크기다:

| | `t2c_spread` | `commit_lead` 산포 |
|---|---|---|
| probe a8 | 0.80 | 0.617 |
| probe a24 | 0.94 | 0.792 |
| output a8 | 7.38 | 5.740 |
| output a24 | 7.10 | 4.750 |

그 산포가 저더의 두 증상을 **모두** 만든다(§22, §23):

* `nc=1` — 큰 탈선이 합성 마감을 스친다. 놓침 순간의 패스 길이가 정상의 2.7~5.2 배.
* `nc=0` — 커밋 간격이 흔들려 16.667ms 합성 경계를 넘나든다. 어떤 합성은 커밋을 둘 받고
  어떤 합성은 하나도 못 받는다. 그때 `c2s` 가 **1 주기 + 정상 lead** 와 0.1~0.6ms 안에서
  일치한다 -- 정확히 한 합성이 통째로 건너뛰어진 것이다.

커밋을 격자 위 고정 지점에서 내면 둘 다 사라진다: 항상 같은 안전한 위상(→ `nc=1` 없음),
커밋 간격이 정확히 한 주기(→ `nc=0` 없음).

## 2. 기계는 이미 있다

`commit_scheduler` 가 절대 QPC 마감에 커밋하는 워커 스레드를 이미 갖고 있고, 구현·리뷰·
실기 검증을 거쳤다. B1(`-PerOutputAlign`)이 기각된 것은 **기계가 틀려서가 아니라 겨냥이
틀려서**다 -- 출력별 vblank 에 맞췄는데, DWM 은 네 타일을 하나의 합성 패스에서 함께
올리므로 맞출 대상이 아니었다.

이 작업은 겨냥만 바꾼다. 스케줄러 스레드, 큐, `upsert`, `device_guard`, 폴백 경로는
건드리지 않는다.

## 3. 마감 규칙

```
deadline = 틱 직후의 첫 CommitAlign% 격자점          (공통 DWM 격자)
```

```
          0%        8%              25%                   100%
격자  ────┼─────────┼────────────────┼──────────────────────┤
                    ▲틱             ▲커밋 마감
                    └── 패스 ───────┘
                    DwmAlign         CommitAlign
```

입력 셋:

* **공통 격자** -- `dcomp_compositor::composition_grid()` → `(qpcVBlank, qpcRefreshPeriod)`.
  `DwmGetCompositionTimingInfo(NULL)` 조회다.
* **틱** -- `output_grid::TICK_QPC`. 셸이 `fire_present_tick` 머리에서 찍는다. 계측
  작업(`MISSEVENT`)에서 이미 들어와 있으므로 새 배선이 없다.
* **`CommitAlign`** -- `gfx_present_align_commit_pct`, `0..=99` 가 켬, `-1` 이 끔(기본).

산식. ★접는 부분은 순수 함수로 떼어 `output_grid` 에 둔다★ -- 그래야 시계나 COM 없이
테스트할 수 있고, 기존 `composition_target` 테스트와 같은 자리에 모인다(§8.1).

```rust
// output_grid.rs -- 순수 함수, 테스트 대상
pub(crate) fn next_grid_point(tick: u64, vblank: u64, period: u64, pct: u64) -> Option<u64>

// paint.rs -- 세 입력을 모으고 그 함수를 부른다
fn commit_deadline() -> Option<u64> {
    let pct = (*crate::commit_scheduler::COMMIT_ALIGN_PCT)?;
    let (vblank, period) = crate::dcomp_compositor::composition_grid()?;
    let tick = crate::output_grid::tick_qpc()?;
    crate::output_grid::next_grid_point(tick, vblank, period, pct)
}
```

`next_grid_point` 의 속:

```rust
let (vblank, period) = composition_grid()?;      // period == 0 이면 None
let tick = tick_qpc()?;                          // 0 이면 None (첫 틱 전)
let base  = vblank.wrapping_add(period * pct / 100);
let delta = tick as i128 - base as i128;
let ahead = period as i128 - (((delta % period as i128) + period as i128) % period as i128);
Some(tick.wrapping_add(ahead as u64))
```

`ahead ∈ (0, P]` 이므로 **마감은 틱으로부터 최대 한 주기 뒤**다. 이것이 설계 문서에 기록된
기아 사고를 구조적으로 막는다 -- 마감을 프레임 간격보다 멀리 밀면 다음 스케줄이 먼저
도착해 `upsert` 가 계속 뒤로 밀고, 한 번 걸린 타일은 영원히 걸린다(DISPLAY22 가 초당
60 건을 스케줄하고 8 건만 커밋했다, `log_ani_debug_02/03`).

**"직후" 는 엄격하다.** 틱이 정확히 `CommitAlign%` 위에 있으면 `ahead = P` 가 되어 한
주기 뒤를 겨냥한다. 이것이 맞다 -- 틱 시점에는 그 패스가 아직 돌지도 않았으므로, 그
자리에서 커밋하면 **이전 프레임**을 내보내게 된다.

**`CommitAlign < DwmAlign` 도 적법하다.** 그러면 첫 `CommitAlign%` 지점이 다음 주기에
있으므로 마감이 틱 + 거의 한 주기가 된다. 패스에 주는 여유는 최대, 합성까지의 여유는
최소다. 산식이 그대로 처리한다.

### 3.1 패스당 하나다

마감이 공통 격자에서 나오므로 **타일과 무관**하다. `Paint::deadline_for_monitor(monitor)`
가 인자 없는 `Paint::commit_deadline()` 이 되고, `flush_deferred_dcomp_commits` 는 타일마다
네 번 계산하던 것을 루프 **앞에서 한 번** 계산한다. 네 타일이 같은 마감을 받으므로
`upsert` 의 `min` 은 무해해진다(여전히 남겨 둔다 -- 늦게 도착한 타일이 마감을 뒤로 밀지
않는다는 보장이다).

### 3.2 묵은 격자 검사는 없앤다

`deadline_for_monitor` 에는 "격자가 60 주기보다 묵었으면 `None`" 검사가 있었다.
`grid_for_monitor` 가 프로브 스레드의 캐시라 최대 ~100ms 낡을 수 있었기 때문이다.
`composition_grid()` 는 **매번 조회하는 시스템 콜**이라 묵을 수가 없다. 검사를 들고 가면
영원히 거짓인 분기가 남으므로 지운다.

대신 비용이 생긴다: `DwmGetCompositionTimingInfo` 가 패스당 한 번(초당 60 회) 추가된다.
셸의 `snap_to_dwm_grid_at` 이 이미 틱당 한 번 부르므로 합계 초당 120 회다. 메인 루프가
초당 343 만 회 도는 것에 비하면 무시할 수 있고, 타일마다 부르지 않으므로 네 배가 되지도
않는다.

## 4. 마감 초과

**패스가 마감까지 못 끝내면 즉시 커밋한다.** 늦더라도 최신 내용을 내보낸다.

새 코드가 필요 없다. `schedule()` 이 이미 지난 마감을 받으면 스케줄러의 기존
`deadline <= now` 경로가 다음 깨어남에 바로 커밋한다.

**보류하지 않는 이유:** 다음 격자점까지 기다리면 위상은 완벽해지지만 그 프레임이 한 주기
늦게 뜨고, §3 의 기아 되먹임을 되살릴 위험이 있다. output 페이지는 창의 39% 에서 패스가
한 주기를 넘으므로 그 위험이 이론이 아니다.

**가시성:** 별도 카운터를 두지 않는다. 스케줄러의 `SLIP_N`/`SLIP_SUM`/`SLIP_MAX` 가
`now − deadline` 을 재어 `OUTCOMMIT` 으로 내므로 초과의 빈도와 크기가 그대로 잡힌다.

## 5. 폴백 = 오늘 동작

이 작업 전체의 전제는 "꺼져 있으면 오늘과 같다" 이고, 그것을 깨지 않는다.

| 상황 | 동작 |
|---|---|
| `CommitAlign = -1`(기본) | `COMMIT_ALIGN_PCT` 가 `None` → flush 가 기존 즉시 커밋 경로 |
| 격자 조회 실패 / `period == 0` | `commit_deadline()` → `None` → `fallback` → `commit_now_guarded` |
| 첫 틱 전 (`TICK_QPC == 0`) | 같음 |
| 스케줄러 스레드 기동 실패 | `SCHEDULER_ALIVE = false` → `schedule()` 이 즉시 커밋 |

`COMMIT_ALIGN_PCT` 는 기존 `ALIGN_PCT` 와 같이 `LazyLock` 으로 굳힌다. `pref!` 는 RwLock
획득이고 이 값을 묻는 자리가 벽의 가장 뜨거운 경로에 있다 -- 꺼져 있을 때 비용이 0 이어야
한다. 이 pref 는 기동 시 커맨드라인에서 한 번 정해지고 실행 중에 바뀌지 않는다.

## 6. 바뀌는 면

| 파일 | 변경 |
|---|---|
| `components/config/prefs.rs` | `gfx_present_align_per_output_pct` **삭제**, `gfx_present_align_commit_pct` 추가(기본 `-1`) |
| `components/paint/commit_scheduler.rs` | `ALIGN_PCT` → `COMMIT_ALIGN_PCT`, 새 pref 를 읽음. 문서 주석 갱신 |
| `components/paint/paint.rs` | `deadline_for_monitor(monitor)` → `commit_deadline()`; `flush_deferred_dcomp_commits` 가 루프 앞에서 한 번 계산 |
| `components/paint/dcomp_compositor.rs` | 디바이스 가드 4 곳(`:1761`, `:2679`, `:2836`, `:3788`) -- **이름만**. 의미("스케줄러가 살아 있으면 잠근다")는 그대로 |
| `components/paint/output_grid.rs` | `pub(crate) fn tick_qpc() -> Option<u64>` 노출 (`TICK_QPC` 를 읽어 0 이면 `None`) |
| `etc/multigpu/run_wall_dist.ps1` | `-PerOutputAlign` → `-CommitAlign`; 검증 둘과 요약줄 갱신 |

**지우는 것:** B1 의 출력별 겨냥 경로. `output_grid::grid_for_monitor` 와 프로브 스레드는
**남긴다** -- `OUTPHASE`/`vs_comp_ms` 가 거기서 나오고 그것이 위상 법칙(§1)의 계측이다.

**스크립트 검증 둘:**

1. `-CommitAlign` 이 `0..99` 인데 `-DcompCommitInFrame` 이면 `throw`. 기존 `-PerOutputAlign`
   검사와 같은 이유다 -- 커밋이 `end_frame` 안에서 나가면 스케줄할 것이 없다.
2. `-CommitAlign` 이 `-1` 도 `0..99` 도 아니면 경고 후 off 로 취급. 기존과 같다.

## 7. 기동 시 경고

```
CommitAlign% − DwmAlign%  가 패스 p95 보다 작으면 경고 한 줄
```

작으면 마감이 패스보다 먼저 와서 거의 매 프레임 초과한다 -- 그러면 이 기능이 아무 일도
하지 않으면서 켜진 것처럼 보인다. 프로브의 패스 p95 는 2.24ms = 주기의 13.4% 이므로
`DwmAlign 8` 이면 `CommitAlign ≥ 22` 쯤이 하한이다.

**하드 에러로 막지 않는다.** 스윕에서 일부러 좁게 주는 경우가 있고, 그때 무슨 일이
생기는지 보는 것이 측정의 일부다. 경고는 엔진의 기동 로그에 한 줄 남긴다(`warn!`,
`Once`). 패스 p95 는 기동 시점에 알 수 없으므로 경고의 기준은 **고정 상수 15%** 로 둔다 --
프로브 p95(13.4%)에 여유를 조금 준 값이고, 주석에 그 출처를 적는다.

### 7.1 `DwmAlign` 과의 관계

`CommitAlign` 이 켜지면 **`DwmAlign` 의 중요도가 떨어진다.** 커밋 위상이 틱 위치와 무관하게
고정되므로 `DwmAlign` 은 "패스를 언제 시작하나" 만 정한다. `DwmAlign = -1`(off, 틱이 격자에
안 붙음)이어도 동작한다 -- "틱 직후의 CommitAlign% 격자점" 은 틱이 어디에 있든 잘 정의되고,
오히려 틱 지터를 흡수한다.

이것은 §9 의 성공 판정과 맞물린다: 성공하면 `-DwmAlign` 스윕의 7 배 차이(설계 문서 §9)가
**사라져야** 한다.

## 8. 테스트

### 8.1 단위 테스트

§3 의 `next_grid_point` 를 테스트한다. 순수 함수라 시계도 COM 도 필요 없다. 기존
`composition_target` 테스트 옆(`output_grid.rs` 의 테스트 모듈)에 둔다.

| 경우 | 입력 | 기대 |
|---|---|---|
| 틱이 목표점 직전 | tick = base − 1 | deadline = base (ahead = 1) |
| 틱이 목표점 직후 | tick = base + 1 | deadline = base + P (ahead = P−1) |
| 틱이 목표점 정확히 위 | tick = base | deadline = base + P (ahead = P) |
| 틱이 여러 주기 뒤 | tick = base + 3P + 5 | deadline = base + 4P |
| `pct` 가 `DwmAlign` 보다 작음 | vblank 기준 tick 25%, pct 8 | 다음 주기의 8% 지점 |
| `pct = 0` | | vblank 격자점 자체 |
| 틱이 vblank 보다 **이전** | tick = vblank − P/2 | 음수 나머지를 접어 올바른 다음 점 |

마지막 것이 중요하다 -- `qpcVBlank` 는 드라이버에 따라 직전일 수도 다음일 수도 있다
(`snap_to_dwm_grid_at` 과 `deadline_for_monitor` 둘 다 같은 이유로 나머지를 두 번 건다).

경계: `period == 0` 이면 `commit_deadline()` 이 `None`.

### 8.2 실기 판정

기존 계측이 전부 답한다. 새 계측을 만들지 않는다.

> **★정정 (2026-10-06, 구현 후 리뷰): 성공 지표를 바꾼다.★**
> `TICKCOMMIT spread` 를 "성공의 정의" 로 박아 두었는데, 이 변경 뒤에는 그 값이 0 에 갈 수가
> **없다**. 커밋이 `tick + ahead` 에 나가므로 `t2c = ahead` 이고, `ahead` 는 틱의 격자 위
> 위상과 1:1 로(반대 방향으로) 움직인다. 즉 ★`t2c` 의 산포는 틱 지터에서 바닥을 친다★
> (실측 `jit_ms p95` 0.62~0.67). 게다가 분포가 두 봉우리가 된다 -- 정상 패스는 `ahead`,
> 마감 초과 패스는 패스 길이. 둘이 겹치면 절반이 즉시 커밋인데도 "좁다" 로 읽힌다.
>
> ★`commit_lead_ms`(커밋 → 합성) 의 `p95−p05` 가 1 차 지표다.★ 커밋이 격자 위 고정점에
> 앉으면 틱이 흔들려도 이 값이 0 으로 간다 -- 그것이 이 변경이 실제로 주장하는 성질이다.
> `TICKCOMMIT spread` 는 보조로 남긴다(틱 지터의 측정값으로 읽는다).

| 지표 | 현재 (probe / output, align 8) | 기대 |
|---|---|---|
| ★`DCOMPSTAT commit_lead_ms` p95−p05★ | 0.63 / 5.74 | ★0 에 가깝게 -- **성공의 정의**★ |
| `TICKCOMMIT spread` | 0.70 / 7.40 | 틱 지터 수준에서 바닥(≈ `jit_ms`), 0 이 되지 않는다 |
| `MISSEVENT nc=1` /s | 0.07 / 0.42 | 축소 |
| `MISSEVENT nc=0` /s | 0.02 / 1.23 | 축소 |
| `COMPREFRESH d0−d2` /1k | 0.3 / 30.6 | 축소 |
| `OUTCOMMIT slip_us` | -- | 마감 초과의 빈도·크기 |

**스윕:** `CommitAlign` 을 22/30/40/50/70 으로. 너무 작으면 초과가 잦고, 너무 크면 합성까지의
여유가 줄어 §24 의 effect (ii) 가 커진다. 좁은 최적 구간이 있을 것으로 예상한다.

**검산:** `-DwmAlign` 을 8 과 24 로 바꿔도 위 지표가 **변하지 않아야** 한다(§7.1).

### 8.3 미리 정해 두는 실패 판정

* **`commit_lead_ms` 의 산포가 안 줄면 설계가 틀린 것이다.** 다른 지표가 좋아져도 그렇다.
* ★사전 등록 검사 하나(리뷰 지적).★ 이 설계의 머리 주장은 "네 타일이 한 합성에 함께 실린다"
  인데, 코드는 그것을 **보장하지 않는다** -- 스케줄러 스레드 하나가 마감에 깨어나 네 커밋을
  순차로 내고, 이 경로에서는 `gfx_dcomp_parallel_commit` 이 일부러 무시된다. 그러니 그
  성질은 `4 × Commit()` 이 여유 안에 들어갈 때만 성립한다. 트리에 두 수치가 공존한다 --
  `commit_scheduler` 모듈 문서의 "커밋 0.02ms" 와 `paint.rs` 병렬 커밋 근거의 "4×2.44 =
  9.8ms". 전자면 안전하고(0.08ms ≪ 16.67ms), 후자면 네 번째 타일이 첫 번째보다 반 주기
  가까이 늦어 ★이 변경이 B1 보다 나빠진다★.
  **스윕 전에 먼저 본다**: `MISSEVENT` 의 `span_ms`(= 그 패스의 first↔last 커밋 폭)와
  `OUTCOMMIT lock_wait_painter_us_max`. `span_ms` 가 한 주기의 몇 %인지가 그 답이다.
* **output 페이지의 `nc=1` 은 남는다.** 창의 39% 에서 패스가 한 주기를 넘으므로 마감을
  구조적으로 지킬 수 없다. ★이것은 이 변경의 실패가 아니다★ -- 설계 문서 §27 의 두 번째
  과제(패스 단축)이고, 이 spec 의 범위 밖이다.

## 9. 범위 밖

* output 페이지의 `flush` 가 4.53ms 인 이유(프로브는 1.00ms인데 `paint` 는 오히려 더 적다).
  미해결이고 별건이다.
* 패스 단축 일반.
* B2(`-SampleLead`)의 출력별 보정. 설계 문서 §19 에 적힌 대로 `vs_comp_ms` 만큼의 출력별
  오프셋을 얹는 별개 작업이다.
* genlock.

## 10. 전제와 그것이 깨지는 경우

* **DWM 은 네 타일을 하나의 합성 패스에서 올린다.** 네 디바이스의 `lastFrameTime` 이
  동일하다는 측정이 근거다. 깨지면 공통 격자라는 전제 자체가 무너지고 B1 의 출력별 겨냥이
  다시 후보가 된다 -- 그래서 `grid_for_monitor` 와 프로브를 지우지 않는다.
* **틱 → 패스 → flush 가 메인 스레드에서 동기적으로 이어진다.** `TICK_QPC` 가 그 패스의
  틱이라는 것이 여기에 달려 있다. `-DcompCommitInFrame` 은 이 사슬을 끊으므로 스크립트가
  조합을 막는다(§6).
* **합성 주기는 실행 중 바뀌지 않는다.** 바뀌면 격자가 매 패스 다시 조회되므로 다음
  패스부터 따라간다 -- 틀린 값으로 고착되지 않는다.

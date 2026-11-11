# 애니메이션 예측 영역을 scissoring 에 반영한다 — 설계

**지시(2026-11-11, 운영자).** "브랜치 새로 만들어서 애니메이션 예측 영역이 scissoring 에
반영되도록 구조 변경하는 설계 및 구현 진행하라."

브랜치: `wall-scissor-follows-animation` (base `ecc38935dc3`).

---

## 1. 무엇을 고치는가

벽은 WebGL 캔버스를 painter 마다 복제해 두고, 각 백엔드가 **자기 타일이 실제로 표출하는
부분만** 셰이딩한다(`gfx_wall_webgl_tile_scissor_from_webrender`). 그 사각형은 WebRender 가
프레임 빌드에서 적어 둔다(`webrender::wall_canvas_rect`).

**증상.** 캔버스가 화면상 움직이는 동안(레이아웃 전환 애니메이션) 타일 경계에 **검은 띠**가
보인다. 운영자 관찰: 정지 상태에서는 없고, 재생 중에만, **이동 변화량에 비례하는 크기**로.

**정착 상태의 scissoring 자체는 정상이다.** `SCISSORGATE` 가 `applied 99.7~100%`,
`stale 0~4`, `no_key 0`, `no_isect 0` 을 낸다(log_webgl_perf_01/28). 즉 이 설계가 고치는
것은 **움직이는 동안의 사각형이 어느 시점의 것인가** 하나다.

---

## 2. 어긋남의 출처 — 캔버스가 비동기라는 것

```
t0:  painter 가 애니메이션 값 A(N) 을 WR 에 보낸다   (append_dynamic_properties, painter.rs:1190)
t0:  WR 이 프레임 N 을 빌드하며 A(N) 기준 박스를 적는다 (note_canvas_tile_rect, picture.rs:3196)
t1:  WebGL 스레드가 그것을 읽어 scissor 를 걸고 캔버스 프레임을 그린다
                                                      (tile_scissor_from_webrender, webgl_thread.rs)
t2:  그 비트맵이 프레임 M 에서 표출된다 (M > N). 그때 값은 A(M).
---------------------------------------------------------------------------
     D = A(M) - A(N)
```

`D` 폭의 띠는 **그려진 적이 없다** — 소비자 쪽 보정으로는 메울 수 없다. 그리고 WR 이 적는
박스는 **그 프레임의 값으로는 정확하다**. 틀린 것은 "그 캔버스가 언제 표출되는지를 아무도
고려하지 않는다" 는 점이다.

`M > N` 인 이유는 캔버스 파이프라인이다. painter 가 프레임을 합성할 때 쓰는 비트맵은 WebGL
스레드가 **그 전에** 완성한 것이고, 그 비트맵의 scissor 는 **더 전의** WR 보고로 걸렸다.

**`-SampleLead`(`gfx_sample_lead_periods`) 는 이 문제를 고치지 않는다.** 그쪽은 painter 가
애니메이션을 **샘플하는 시각**을 그 타일의 표출 시각으로 옮긴다. 궤적 전체가 같이 밀리므로
`A(M) - A(N)` 의 차는 그대로다. 두 기능은 축이 다르고 독립적이다.

---

## 3. 쓸 수 있는 것 — 두 조각이 이미 있다

```rust
// ① 애니메이션 보간기 -- 순수 함수이고 임의의 시각을 받는다
components/paint/web_content_animation.rs:360
    fn sample(&self, now: Instant,
              floats: &mut Vec<PropertyValue<f32>>,
              transforms: &mut Vec<PropertyValue<LayoutTransform>>)

// ② 그 "미래 시각" 을 계산하는 코드
components/paint/output_grid.rs:174   lead_to_next_composition(lead_periods)
components/paint/commit_guard.rs:96   gfx_sample_lead_periods
```

①이 순수 함수라는 점이 이 설계의 토대다. **미래 값은 추정이 아니라 애니메이션 정의에서
나온 정확한 값이다.**

### 전에 제거된 "외삽" 과의 차이

| | 2026-11-10 에 제거된 것 | 이 설계 |
|---|---|---|
| 미래를 어떻게 아는가 | 과거 두 점으로 **선형 외삽** | **애니메이션 정의에서 계산** |
| 곡선·가감속에서 | 틀린다 | 맞는다 |
| 전환 시작/끝에서 | 틀린다(궤적이 꺾임) | 맞는다(세그먼트를 안다) |
| 운영자 평가 | "tradeoff 가 심하다" → 제거 | — |

운영자 지시의 "애니메이션 예측 영역" 이 가리키는 것이 이 차이다.

---

## 4. 두 가지 방식을 비교했다

### 4.1 기각 — WR 안에서 박스를 두 번 계산하기

처음 설계는 "painter 가 예측 값을 `DynamicProperties` 와 나란히 보내고, WR 이 두 변환으로
박스를 둘 계산해 합집합을 적는다" 였다. 조사해 보니 침습이 크다:

WebRender 는 애니메이션 정의를 모르고 그 프레임의 **값만** 받는다
(`painter.rs:1190` → `render_backend.rs:488` `AppendDynamicProperties` →
`SceneProperties`). 그 값으로 `spatial_tree.update_tree(&dynamic_properties)`
(`render_backend.rs:645`)가 변환을 세우고, `note_canvas_tile_rect` 는 그
`spatial_tree` 를 써서 `SpaceMapper` 로 박스를 구한다.

즉 예측 박스를 WR 안에서 얻으려면 **`spatial_tree` 를 두 벌 세우거나**, `SpaceMapper` 가
쓰는 변환을 그 자리에서 예측값으로 갈아끼워야 한다. 전자는 비싸고 후자는 WR 내부를 깊게
건드린다. 이 리포는 `vendor_local/webrender` 를 20 줄 규모의 동작 포크로만 유지하고 있고,
그 선을 넘을 이유가 없다.

### 4.2 채택 — 예측 이동량만 싣는다

캔버스가 화면에서 움직이면 **캔버스 안에서 타일에 보이는 부분이 옮겨 간다.** 그 옮겨 간
양은 화면상 이동량을 캔버스 좌표로 환산한 것이다. 그러면 WR 이 적은 박스를 **그 양만큼
넓히는 것으로 충분하다** — 박스를 다시 계산할 필요가 없다.

환산 비율은 이미 소비자가 갖고 있다. `wall_canvas_rect` 는 `rect`(캔버스 좌표)와
`dest_size`(화면상 목적지 크기)를 함께 적고, `webgl_thread::scale_canvas_rect` 가 그 둘로
캔버스 백킹 좌표로 환산한다. 예측 이동량도 같은 비율을 탄다.

```
painter (틱마다, 애니메이션이 있을 때만)
    now_values  = sample(now)            -- 지금처럼 WR 로 보낸다
    lead_values = sample(now + lead)     -- 보내지 않는다. 차이만 쓴다
    shift       = lead_values 와 now_values 의 ★평행이동 성분 차이★ (layout px)
    wall_canvas_rect::set_predicted_shift(shift)      -- 원자 둘

WebGL 스레드 (캔버스 프레임마다)
    scissor 박스를 그 shift 방향으로 ★넓힌다★ (합집합)
```

**WR 수정은 0 이다.** painter 쪽 샘플 한 번과 원자 쓰기 둘, 소비자 쪽 덧셈 몇 번이 전부다.

### 4.3 정확도와 그 한계

* **이동(translate)** — 정확하다. 평행이동 성분의 차이가 곧 화면 이동량이다.
* **스케일·회전** — 근사다. 평행이동 성분만 보므로 크기 변화와 회전으로 생기는 경계 이동을
  놓친다. 다만 **합집합으로 넓히는 방향이라 빈 칸을 만들지 않는다** — 놓친 만큼 띠가 남을
  뿐이고, 그때는 실기에서 `lead` 를 올려 덮는다.
* 전환 애니메이션은 이 리포에서 **이동이 지배적**이다(레이아웃이 슬라이드한다). 운영자가
  "이동 변화량에 비례하는 크기" 로 띠를 관찰한 것이 그 증거다.
* 여러 애니메이션이 동시에 돌면 **가장 큰 이동량**을 쓴다. 캔버스가 어느 것에 붙어 있는지
  painter 는 모르므로, 큰 쪽으로 넓히는 것이 안전한 방향이다.

### 4.4 전에 제거된 "외삽" 과의 차이

| | 2026-11-10 에 제거된 것 | 이 설계 |
|---|---|---|
| 미래를 어떻게 아는가 | 과거 두 점으로 **선형 외삽** | **애니메이션 정의에서 계산** |
| 곡선·가감속에서 | 틀린다 | 맞는다 |
| 전환 시작/끝에서 | 틀린다(궤적이 꺾임) | 맞는다(세그먼트를 안다) |
| 어디서 계산하나 | 소비자(WebGL 스레드)가 과거 값으로 | **생산자(painter)가 정의로** |
| 운영자 평가 | "tradeoff 가 심하다" → 제거 | — |

운영자 지시의 "애니메이션 예측 영역" 이 가리키는 것이 이 차이다.

---

## 5. 설계

### 5.1 `lead` 를 무엇으로 하는가

`lead` 는 **캔버스 파이프라인 지연**이다. 그 값을 지금 아무도 재지 않는다
(`SURFIMPORT` 는 합과 개수만, `RENDERCOST` 는 포함하지만 귀속 불가). 그래서:

* pref `gfx_wall_canvas_scissor_lead_periods`, 기본 **1**(한 주기 = 16.67ms at 60Hz).
* `0` = 예측 끄기(= 현재 동작). 범위 `0..=4`.
* 실기에서 0/1/2 를 쓸어 **경계 띠가 사라지는 최소값**을 찾는다. 그 값이 곧 지연의
  측정값이기도 하다.

주기는 `gfx_refresh_hz` 에서 온다 -- painter 가 이미 그 값으로 틱을 센다.

### 5.2 넓히는 규약

`shift = (dx, dy)` 를 캔버스 백킹 px 로 환산한 뒤:

```
x0 -= max(0, -dx).ceil()      x1 += max(0,  dx).ceil()
y0 -= max(0, -dy).ceil()      y1 += max(0,  dy).ceil()
```

그 다음 **캔버스 경계로 자른다**(`0..canvas.w`, `0..canvas.h`). 기존
`note_canvas_tile_rect` 의 `floor`/`ceil` 바깥쪽 반올림과 같은 원칙이다 — **scissor 는
필요보다 큰 쪽이 안전하다.** 작으면 빈다.

### 5.3 경계 조건

| 조건 | 동작 |
|---|---|
| 애니메이션이 없다 | `shift = (0, 0)` → **변화 없음** |
| 지평(`gfx_paint_side_animation_horizon_ms`, 3000) 밖 | `sample` 이 값을 주지 않음 → `shift = (0,0)` |
| 캔버스가 멈췄다 | WR 의 `note` 가 끊기므로 기존 `max_age_ms` 경로 그대로 |
| 전환으로 새 컨텍스트 | 첫 프레임은 `Missing` — 변화 없음 |
| `lead = 0` | painter 가 예측 샘플을 건너뛴다(원자 로드 하나) |
| `dest_size` 가 0 | 기존 가드가 먼저 걸러낸다 |

### 5.4 비용

* **painter**: 틱마다 `sample` 한 번 더. 애니메이션 개수에 비례하고, 보간은 세그먼트 찾기
  + 선형 보간이다. 그 뒤 원자 둘(`AtomicI32` x2). `lead = 0` 이면 원자 로드 하나로 끝난다.
* **WR**: **변화 없음.**
* **WebGL 스레드**: 원자 로드 둘과 덧셈 몇 번. 기존 `scale_canvas_rect` 안이다.
* 타이밍 경로 규칙(2026-10-27, 계측 20,489 줄을 걷어낸 이유)을 지킨다 — I/O 없음, 새 락
  없음, 새 할당 없음.

---

## 6. 작업 분할

1. **`wall_canvas_rect` 에 예측 이동량 슬롯** — `set_predicted_shift(dx, dy)` /
   `predicted_shift()`, `AtomicI32` 둘. 단위는 **layout px**(소비자가 환산한다).
   (`vendor_local/webrender/src/wall_canvas_rect.rs` — 이 파일은 이미 이 포크의 것이다)
2. **pref + 런처 스위치** `gfx_wall_canvas_scissor_lead_periods` / `-CanvasScissorLead`.
3. **painter 가 예측 이동량을 계산해 싣는다** — `sample(now + lead·period)` 와
   `sample(now)` 의 평행이동 성분 차이, 여러 애니메이션이면 최대값.
   (`components/paint/painter.rs` 의 값 수집 자리, `web_content_animation.rs`)
4. **소비자가 박스를 넓힌다** — `webgl_thread::scale_canvas_rect` 에서 `dest_size` 비율로
   환산해 5.2 의 규약대로. 캔버스 경계로 자른다.
5. **단위 테스트** — 넓히기 기하(네 방향, 경계 클램프), `lead = 0` 의 무변화, 지평 밖 폴백.
   `wall_canvas_rect` 와 `scale_canvas_rect` 둘 다 순수 함수로 뽑아 테스트 가능하게.
6. **실기 검증** — `-CanvasScissorLead 0/1/2`, 전환 구성 고정, 판정은 **경계 띠의 육안
   유무**와 `SCISSORGATE applied%`.

---

## 7. 판정 기준

* **성공**: 전환 재생 중 타일 경계의 검은 띠가 육안으로 사라진다. `SCISSORGATE applied%`
  가 떨어지지 않는다(합집합이 `no_isect` 를 늘리지 않아야 한다).
* **실패**: 띠가 남으면 `lead` 를 올려 본다. 2 까지 올려도 남으면 **전제가 틀린 것**이고,
  그때는 `D` 가 애니메이션 예측이 아닌 다른 지연(예: WR 프레임 큐)에서 온다는 뜻이다.
  그 경우 이 설계를 되돌리고 그 지연을 먼저 측정한다.
* **부작용 감시**: 셰이딩 증가가 `RENDERCOST` 의 히스토그램을 눈에 띄게 밀면 교체 방식
  (`box_lead` 만)을 A/B 한다.

---

## 8. 이 설계가 고치지 않는 것

* **WebGL 구성의 judder** — 별 축이다. `single_publish` 기본화 이후 judder 는 WebGL 이
  있는 구성에만 남았고, 그 원인은 `lock_swap_chain` 의 두 대기(블로킹 뮤텍스 +
  `AcquireSync`)로 좁혀져 있다. 그 대기의 길이를 재는 수치가 아직 없다.
* **`Missing` 과 "이 타일엔 안 보임" 이 같은 신호라는 것** — `note` 는 캔버스가 타일 밖이면
  아예 쓰지 않으므로, 소비자가 "안 보이니 아무것도 그리지 말라" 를 표현할 수단이 없다.
  이 설계는 그것을 건드리지 않는다.

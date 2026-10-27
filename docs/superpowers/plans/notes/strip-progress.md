# 계측 전면 제거 — 경과 (브랜치 `wall-strip-instrumentation`)

계획서: `docs/superpowers/plans/2026-10-27-instrumentation-strip.md`
인벤토리·재검증 전문: `docs/superpowers/plans/notes/strip-inventory/`

**이 파일은 단계마다 갱신한다.** 한 화면에서 (1) 무엇을 지웠나 (2) 게이트 결과
(3) 계획서가 틀린 곳 (4) 피한 함정을 보기 위한 것이다.

## 요약

| | |
|---|---|
| 진행 | **5 / 20 단계** |
| 누적 제거 | **약 3,845 줄** |
| 계획서 오류 (발견) | **33 건** (실행 중 5 + 재검증 28) |
| 조용한 회귀 회피 | **8 건** |
| 기준선 | `servo-paint --lib` 130/130 → 현재 **113/113** (계측 테스트 17 개 제거분) |
| `config_surface` | 19/20 (기존 RED) → Step 14 에서 **20/20** 이 되어야 함 ★독립 성공 신호★ |

---

## 단계별

### Step 1 — `note_commit_failure` 의 `out=` 장식 · 디바이스→모니터 맵 `3269fc19b0d`

`dcomp_compositor::note_commit_failure` 의 `out=` 필드와 그것을 만드는 두 문장,
`commit_guard` 의 `DEVICE_MONITOR`/`remember_device_monitor`/`monitor_for_device`.

**왜 1 번인가 (강제)** — `output_grid::publish_names` 가 `NAMES` 의 유일한 writer 인데
Step 12 에서 `probe_loop` 와 함께 죽는다. 그 뒤로 `out=` 은 영원히 `?` 다. 그래서
"이 에러 보고를 남길 것인가" 를 그 전에 정해야 했다. **남겼다** — 초당 통계가 아니라
TDR / device-removed / SURFACE_BEING_RENDERED 보고다.

게이트 130/130.

### Step 2 — `components/webgl` 계측 `0bea8248130` (419 줄)

`spawn_webgl_watchdog`(1 Hz 샘플링 전용 스레드)와 그것이 보던 원자 다섯 개, `PHASE_*`,
`phase_name` / `log_live_contexts`(WEBGLLIVE) / WEBGLFANOUT 일체(게이트, 14 필드 누적기,
스톱워치 여덟) / WEBGLRECLAIM 의 계측부 / `adapter_local_usage_bytes` + winapi `dxgi1_4`.

**피한 함정** — `apply_webgl_command_to_surface` 에서
`let _angle_gl_guard = paint_api::angle_gl_lock(...)` 가 **스톱워치 두 문장 사이**에 끼어
있었다. 범위 삭제했으면 RAII 락이 사라져 WebGL 스레드가 타일 렌더와 같은 ANGLE 디바이스를
경합한다 — `libGLESv2` 접근 위반이고 **단일 GPU 개발기에서는 재현되지 않는다.**

**관측** — `WEBGLLIVE` 는 초당 한 줄로 보였지만 게이트 없이 **모든 메시지마다** 돌고
있었다. 맵 두 번 순회 + `RwLock::read` 가 메시지당.

**하중 카운트 대조** — `angle_gl_lock` 8/8, `swap_buffers(` 4/4, `clear_surface(` 2/2,
`gl.flush()/finish()` 4/4. 게이트 130/130.

### Step 3 — dcomp 자립 로그 게이트 · DWMPHASE · `dwm_timing()` `26225f22647` (786 줄)

`[vesc-prof]` 171 / readback+validprobe 174 / `[dcomp-dbg]` 184 / DWMPHASE 141 / `dwm_timing()` 28
+ `output_grid` 쪽 81.

**계획서 오류 2 건**
- `[dcomp-dbg]` 는 17 이 아니라 **16** (중괄호 대조로 전수 확인).
- ★`dwm_timing()` 의 소비자가 둘이 아니라 **셋**★ — `outvb_after_compose_ms` 가 `qpcCompose`
  를 읽고 그 필드는 `dwm_timing` 만 공급한다. 둘만 지우면 **`output_grid.rs` 가 컴파일되지
  않는다.** 셋째와 그것만 쓰던 `fold_centered` 까지 제거.

**남긴 것** — `composition_grid()`. 호출처 일곱 중 여섯은 로그지만 일곱째가
`lib.rs:64 dwm_composition_grid` → `winit_wall/main.rs:1235 snap_to_dwm_grid_at`,
즉 페이싱 클럭의 타이머 폴백이다. `DwmGetCompositionTimingInfo` 를 감싼 거의 똑같은 함수
둘인데 판정이 반대다.

하중 카운트 13 종 불변. 게이트 130/130.

### Step 4 — dcomp 커밋 스탬핑 · COMMITSITE/COMMITWORK · DCOMPBIND `79855e8f0ae` (1,121 줄)

`CommitOutcome`(→ `raw_commit` 이 `i32` 직접 반환) 55 / COMMITSITE·COMMITWORK 440 /
DCOMPBIND 420 / 계측 테스트 15 개 201.

**계획서 오류 3 건**
- 게이트는 113 이 아니라 **115**. 인벤토리의 "17 items" 에 테스트가 아닌 헬퍼 둘이
  섞여 있었고 계획서가 그걸 "테스트 17 개" 로 옮겨 적었다. 실제 `#[test]` 는 15.
  남은 22 개가 전부 동작 커버리지임을 **이름 단위로 대조**했다.
- `raw_commit` 호출처는 넷이 아니라 **셋**("넷" 은 `CommitSite` 변종 수).
- `commit_device_guarded` 도 `site` 인자를 들고 있었는데 계획서에 없다.

**남긴 것** — `DCOMP_BIND_PROF` static. `COLLECT` 의 우변이라 지금 지우면 Step 13 까지의
중간 커밋들에서 **`COLLECT` 가 기본 ON 으로 뒤집힌다.** 커밋 사이를 육안 비교하는 동안
계측이 켜지면 안 된다.

하중 카운트 18 종 불변. 게이트 **115**.

### Step 5 — `painter.rs` 애니메이션·FRAMEREASON·렌더 경로 `e31d4042933` (1,519 줄)

애니메이션 텔레메트리 505(필드 25 개 포함) / FRAMEREASON 200 / `render()` 로그 블록 넷 215 /
GPUMEM 190 / 캔버스 ack 래치 115 / IMGUPDINNER 85 / WALLBOOTSTRAP 42 / PAINTANIMANCHOR 34 /
`PumpSource::note` 13.

**계획서 오류·누락 4 건**
- 게이트는 111 이 아니라 **113**(다섯 단계 연속 집계 오류).
- `last_render_end` 의 doc 주석이 **엉뚱한 필드에 붙어 있었다** — 선언보다 14 줄 위,
  `owns_presentation` 의 doc 머리다.
- ★`painter_busy` 는 단순히 "안 읽히는 값" 이 아니다★ — `pending_frames > 0 ||
  renderer_behind()` 가 **단락 평가**라 `renderer_behind()` 안의 2 초 강제 해제는
  `pending_frames == 0` 일 때만 돈다. 맨 호출로 접었으면 지금은 닿지 않는 상태에서
  강제 해제가 발화한다.
- `log_gpu_memory` 호출처는 맨 호출이 아니라 `if let Some(device) = ...` 3 줄 래퍼.

하중 심볼 41 종 중 35 불변, 변한 여섯은 전부 로그 인자였음을 백업과 대조. 게이트 **113**.

---

## Steps 8~11 재검증 `78c392d4fbc`

Step 5 실행과 **병렬로** 읽기 전용 재검증(파일 집합이 겹치지 않음). 계획서 8 절에 고정.
**오류 28 · 하중 함정 12 · 누락 계측 14.** 가장 중요한 것:

- ★`winit_wall/main.rs:1727`★ `dispatched[index] = true;` 가 계측 `skipped += 1`(1726)과
  하중 `continue;`(1728) **사이**에 끼어 있다. 떨구면 barrier 가 건너뛴 타일을 두 번째
  루프가 전부 다시 칠한다. **컴파일 에러 없음.** 이 제거 전체에서 가장 위험한 한 줄.
- ★`layout/display_list/mod.rs:560`★ 이 계획서가 "유지" 라고 한 바로 그 `return false;` 다.
- ★`d3d11_ring` 의 `lock_registry` 는 존재하지 않는다★ — 셋째 헬퍼는 `fn lock`(:504).
  놓쳐서 `.unwrap()` 으로 바뀌면 패닉 스레드 하나가 `CONSUMER_DEVICE` 를 오염시켜
  **모든 타일의 비디오가 영구 정지**한다. 교체 대상 28 곳.
- ★`last_pacing_tick_at` 래치를 어느 테스트도 덮지 않는다★ — 계획서가 가드라고 지목한
  테스트는 `needs_pacing_backstop(bool, bool)` 자유 함수만 부른다. 망쳐도 게이트는 초록.
- `note_rendering_update_reason` 호출처 **11**/9, `rendering_update_from_timer` **5**/3,
  `script_thread` import 는 삭제가 아니라 **트림**, 계획서의 `MEDILOCKDEFER` 는 **오타**.

---

## 방법 (단계마다 동일)

1. 편집 전 하중 심볼 `grep -c`(주석 제외) 기준선.
2. **심볼 단위 삭제.** 범위가 불가피하면 파이썬 스크립트가 각 범위의 **첫 줄·끝 줄 내용**,
   **범위 간 비중첩**, **반드시 살아야 할 줄 목록이 범위 밖**임을 전부 검증한 뒤
   아래에서 위로 삭제. 어서션이 터지면 **범위를 고치지 어서션을 고치지 않는다.**
3. 편집 후 카운트 재측정 — 하중은 전부 불변이어야 한다.
4. 삭제로 인자가 고아가 되면 `_name` 또는 `let _ = ...`, 부수효과가 하중이면 호출은 보존.
5. 단계마다 커밋 하나. `git add -A` 금지, 금지 목록 파일은 절대 스테이징하지 않는다.

**어서션이 실제로 잡은 것**: Step 2 에서 범위 계산이 두 번 어긋났고(끝줄, 빈 줄),
Step 3 에서 두 번, Step 4 에서 열한 번 터졌다. 전부 삭제 전에 잡혀 파일은 무손상.

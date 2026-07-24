# servoshell display-only 공간배치 + auto-GPU wall_layout 이식 설계 (2026-07-24)

## 목표

winit_wall 예제에만 구현돼 있던 **display-only 공간배치 + GPU 자동할당** wall_layout 방식을
servoshell(`wall_layout.rs` / `headed_window.rs`)에 이식한다. 타일이 `monitor`(winit
`available_monitors().nth()` 인덱스 = 비공간·플랫폼의존) + `gpu`(명시 어댑터) 대신 공간
`display` 인덱스(좌상단=0, 좌→우 그다음 위→아래) 하나를 갖고, 창 생성 시 DXGI 디스플레이
토폴로지를 해석해 실제 desktop 좌표에 배치하고 그 디스플레이를 구동하는 adapter에 렌더링
컨텍스트를 자동 바인딩한다.

대상 워크트리: `servo_multigpu-tiled-wall`, 브랜치 `nonstandard-media-display-port`.

## 배경 — 실측으로 확정한 현재 상태

- 참조 구현은 `wall-spatial-display-autogpu` 브랜치의 `components/servo/examples/winit_wall.rs`에
  존재. 그 브랜치는 이 방식을 **winit_wall 예제에만** 적용했고 servoshell은 "후속"으로 남겼다
  ([[wall-spatial-display-autogpu]] 메모리).
- 현재 브랜치 `ports/servoshell/wall_layout.rs`: `WallTile { monitor: usize, gpu: usize, rect }`.
  파서는 `monitor`/`gpu`를 필수로 읽는다(`wall_layout.rs:182-186`).
- `ports/servoshell/desktop/headed_window.rs`:
  - `available_monitors().nth(tile.monitor)`로 창 배치(`headed_window.rs:170`).
  - `requested_gpu_index = tile.gpu`(`headed_window.rs:239-243`) → `WindowRenderingContext::
    new_with_target_gpu`/`new_with_optional_refresh_driver_and_target_gpu`에 전달.
  - 타일 크기==모니터 크기면 borderless 풀스크린(flip-model present 적격)으로 전환하는
    servoshell 특유 로직(`headed_window.rs:182-202`).
  - present 로그가 `tile.monitor`/`tile.gpu`를 출력(`headed_window.rs:755-761`).
- `ports/servoshell/desktop/app.rs`: 타일 plan 로그가 `monitor`/`gpu` 출력(`app.rs:208-214`).
- `components/shared/paint/rendering_context.rs`(패키지 servo-paint-api): `dxgi_luid_for_gpu_index`는
  **이미 있음**(`:344`, windows + 비-windows 폴백). `DisplayTopology`/`enumerate_display_topology`/
  `spatial_order`는 **없음**(spatial 브랜치에만: `:246`/`:265`/`:350`).
- 설정 JSON(`config/*.json` + `etc/multigpu/config/*.json`)은 이미 display-only로 마이그레이션돼
  있어 현재 파서가 파싱 실패한다(브랜치-스키마 테스트 config `test_2x1_samegpu.json`은
  monitor+gpu 유지 — 레거시 경로로 계속 지원해야 함).

## 사용자 결정 사항

- **토폴로지 해석 위치**: `HeadedWindow::new` 내부에서 창마다 1회 해석(플러밍 없음, headed_window.rs에
  변경 국한). 조회는 시작 시 1회/창, EnumAdapters1 기반이라 저렴.
- **레거시 호환**: `monitor`를 `display` 별칭으로 수용(deprecation 경고) + `gpu` 있으면 무시(경고).
  기존 monitor+gpu 설정이 마이그레이션 없이 계속 동작.
- **borderless 풀스크린 로직**: 유지하되 판정 기준을 spatial로 해석된 display 크기에 맞게 적응.

## 설계

### 1. 토폴로지 헬퍼 — `components/shared/paint/rendering_context.rs`

wall-spatial-display-autogpu 브랜치에서 verbatim 이식:
- `pub struct DisplayTopology { left, top, width, height: i32, adapter_index: usize, luid: (i32, u32),
  device_name: String, attached_to_desktop: bool }`.
- `pub fn enumerate_display_topology() -> Vec<DisplayTopology>`: `#[cfg(all(windows, no-wgl))]`는
  winapi `EnumAdapters1`→`EnumOutputs`→`DXGI_OUTPUT_DESC.DesktopCoordinates`+LUID; 그 외 빈 벡터 폴백.
  `adapter_index`는 EnumAdapters1 순서 = 기존 `requested_gpu_index`/`create_dxgi_adapter_by_index`와 동일.
- `pub fn spatial_order(topology: &[DisplayTopology]) -> Vec<DisplayTopology>`: 순수함수(좌상단=0,
  좌→우 그다음 위→아래; 행 밴딩=수직겹침 ≥ 짧은 높이 50% + median 허용오차). 기존 spatial_order
  단위테스트 5개 포함.

기존 `dxgi_luid_for_gpu_index`와 공존(중복 정의 없이 추가만).

### 2. re-export — `components/servo/lib.rs`

paint_api::rendering_context에서 `DisplayTopology`/`enumerate_display_topology`/`spatial_order`
re-export(기존 `dxgi_luid_for_gpu_index` re-export 옆에; 없으면 함께 추가).

### 3. 스키마/파서 — `ports/servoshell/wall_layout.rs`

- `WallTile { display: usize, rect }` (monitor/gpu 제거).
- `parse_tiles`(또는 해당 파싱 지점): `display` 필드를 읽되, 없으면 레거시 `monitor`를 별칭으로
  수용(`warn!` deprecation). `gpu` 키가 있으면 `warn!` 후 무시. `display`도 `monitor`도 없으면
  기존과 같이 파싱 에러.
- 유닛테스트: 기존 3개(valid parse / out-of-bounds reject / overlap render rect)를 display 스키마로
  갱신 + **레거시 monitor 별칭 수용 + gpu 무시** 테스트 1개 추가. doc-comment의 예시 JSON도 갱신.

### 4. 배치 + auto-GPU — `ports/servoshell/desktop/headed_window.rs`

`HeadedWindow::new`의 wall 타일 블록(현 `:165-243`):
- 타일 활성 시 `let spatial = spatial_order(&enumerate_display_topology());` 1회.
- `spatial.get(tile.display)`가 `Some(disp)`:
  - 창을 `disp.left/top`(실제 desktop 좌표)에 배치.
  - borderless 풀스크린 판정을 `disp` 크기(또는 그 좌표에 걸친 winit monitor) 기준으로 유지 —
    타일 요청 크기 == display 크기면 borderless fullscreen, 아니면 `set_outer_position(disp origin)`.
  - `requested_gpu_index = Some(disp.adapter_index)`.
- `None`(토폴로지 없음 / display 범위초과): 현행 폴백 = `available_monitors().nth(tile.display)`
  배치(`warn!`) + `requested_gpu_index = None`(surfman 기본 adapter).
- `requested_gpu_index` 계산부(현 `:239-243`)를 위 해석 결과로 대체.
- present 로그(`:755-761`)의 `tile.monitor`/`tile.gpu` → `tile.display` + 실제 adapter(있으면).

### 5. 로그 — `ports/servoshell/desktop/app.rs`

타일 plan 로그(`:208-214`)의 `monitor`/`gpu` → `display`(+ "auto-GPU" 표기).

## 데이터 흐름

config `display` → `WallTile.display` → `HeadedWindow::new`가 토폴로지 해석 → 창을 실제 display
좌표에 배치 + 렌더링 컨텍스트를 구동 adapter에 바인딩. 입력/페인트 좌표 모델(rect 기반, 가상
뷰포트)은 무변경 — `display`는 물리 배치·GPU 선택에만 영향.

## 오류 처리

- 토폴로지 비었거나 `display` 인덱스 범위초과 → `warn!` + winit-monitor-nth 폴백(현행 동작 보존).
- 레거시 `monitor` → `warn!`(deprecated) + display로 해석. `gpu` → `warn!` + 무시.
- `display`/`monitor` 둘 다 없음 → 파싱 에러(기존과 동일).

## 테스트

1. `cargo test -p servo-paint-api spatial_order`(또는 해당 크레이트) — spatial_order 5 pass.
2. `cargo test -p servoshell wall_layout --lib` — display 스키마 갱신 + 레거시 별칭 테스트 pass.
3. `cargo check -p servoshell` + `cargo build -p servoshell`.
4. `rustfmt --edition 2024 --check` + `git diff --check`.
5. 실기(단일GPU 2모니터): `Wall display topology` 로그가 좌측(x=0) 물리 디스플레이를 spatial 0으로
   정렬. `--wall-all-tiles`로 타일이 실제 display 좌표에 배치, 배리어 ready=N/N, panic 0.
   레거시 monitor+gpu config(`test_2x1_samegpu.json`)로도 별칭 경로 동작 확인(경고 + 정상 배치).

## 비범위

- cross-GPU 텍스처 복사/워크로드 스틸링(무변경 — v1 비목표 유지).
- winit_wall(이미 구현됨).
- 좌표/페인트/입력 모델 변경.
- 설정 JSON 재마이그레이션(레거시 경로로 계속 지원하므로 불필요).

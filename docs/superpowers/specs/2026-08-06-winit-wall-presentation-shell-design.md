# winit_wall을 표출 전용 최소 셸로 — 브랜치 성과 이식 설계 (2026-08-06)

## 목표

경량 임베더 `winit_wall`을 `nonstandard-media-display-port` 브랜치 위로 옮겨, **표출 전용 최소 셸**로 쓸 수 있게 한다. servoshell은 폐기하지 않고 **사용자 UI/UX 전용**으로 병행 유지한다.

핵심은 "기능을 옮기는 것"이 아니라 **브랜치를 정렬하는 것**이다. 이 브랜치 성과의 대부분은 엔진 측에 있어, winit_wall이 이 브랜치 위에 올라오는 것만으로 따라온다.

## 배경 — 실측으로 확정한 현재 상태

### 두 작업선이 갈라져 있다

| 항목 | 값 |
|---|---|
| 공통 조상 | `6694c14ea` (2026-06-15) |
| `video-perf-investigation` (winit_wall 보유) | +65 커밋, **`multigpu-tiled-wall` 미포함** |
| `nonstandard-media-display-port` (HEAD, 정본) | +277 커밋 |

`winit_wall.rs`는 **이 브랜치에 존재하지 않는다.** `components/servo/examples/winit_wall.rs`(906줄)로 저쪽 브랜치에만 있다.

### 이 브랜치 277커밋의 변경 분포 — 대부분 엔진 측

| 영역 | 파일 수 | 성격 |
|---|---|---|
| `third_party/stylo` | 378 | stylo 로컬 벤더링(`de1b323b9da`) + 패닉 수정. 빌드 인프라 |
| `components/*` (media 22, shared 9, paint 6, script 5, script_bindings 4, servo 3, pixels 2, net/layout/config 각 1) | ~54 | **엔진 — 임베더 무관하게 따라옴** |
| `third_party/surfman` | 5 | present-path-fast, EGLImage interop, 해체 UAF 수정 |
| `ports/servoshell` | 7 | **임베더 고유 — 실제 이식 대상** |

`ports/servoshell` 7개 = `Cargo.toml`, `desktop/app.rs`, `desktop/gui.rs`, `desktop/headed_window.rs`, `desktop/mod.rs`, `desktop/vsync_refresh_driver.rs`, `wall_layout.rs`. 이 중 `gui.rs`는 egui 크롬 전용이라 winit_wall에 해당 사항이 없다.

### winit_wall이 이미 갖춘 것

저쪽 브랜치가 `wall-spatial-display-autogpu` 위에 있어, 다음은 이미 구현돼 있다.

- display-only 레이아웃 스키마 + 레거시 `monitor` 별칭 + `gpu` 무시 경고 (파일 내 인라인 `mod wall`, 667행~)
- `spatial_order`/`enumerate_display_topology` 기반 타일별 어댑터 바인딩 + LUID 교차검증
- 토폴로지 부재 시 winit 모니터 인덱스 폴백
- `WebViewPaintTarget` 팬아웃, `paint_target_keep_previous_logical_frame`, 전 창 강제 redraw

### winit_wall의 갭 6건

| # | 갭 | 근거 |
|---|---|---|
| 1 | 가드밴드 크롭 | `overlapPx`를 파싱만 하고 사용처가 없음. `tile_render_rect`/`tile_render_insets`/`present_inset` 전부 부재 |
| 2 | DPI 변경 시 inset 재주입 | `ScaleFactorChanged` 핸들러 없음 |
| 3 | borderless fullscreen | flip-model present 자격 확보용 설정 없음 |
| 4 | vsync refresh driver | `SERVO_WIN_VSYNC` 배선 없음 |
| 5 | `wall_layout` 중복 | winit_wall 인라인 `mod wall` ↔ `ports/servoshell/wall_layout.rs`(661줄)가 별개. 복사하면 세 번째 사본 |
| 6 | d3d11 백엔드 | `--backend` 플래그와 `Dx11RenderingContext` 분기 제거 대상 |

### 심볼 가용성 — A안이 컴파일 관점에서 성립한다

winit_wall이 쓰는 `servo::` 심볼 중 이 브랜치에 **없는 것은 `Dx11RenderingContext` 하나뿐**이다. `enumerate_display_topology`, `spatial_order`, `dxgi_luid_for_gpu_index`, `WebViewPaintTarget`, `paint_target_keep_previous_logical_frame`, `present_inset`은 모두 존재한다.

### 상호배타 제약 — d3d11 백엔드는 표출 스택과 공존할 수 없다

```rust
// components/paint/dcomp_compositor.rs:1218
/// (ANGLE이 아닌 빌드에서는 항상 false — 네이티브 컴포지터 불성립.)
```

DComp 네이티브 컴포지터는 ANGLE 빌드에서만 성립하고, 제로카피 업로드도 `angle_d3d11_device_ptr()`(`dcomp_compositor.rs:1232`) 경유다. winit_wall의 `--backend d3d11`(wr-d3d11, surfman 비경유)에서는 둘 다 불성립한다. 이 백엔드는 원래 ANGLE 오버헤드 분리 측정용 실험이었다.

## 사용자 결정 사항

1. **winit_wall의 역할 = 실제 표출용 최소 셸** (계측 하니스가 아님)
2. **통합 방향 = A안** — winit_wall만 이 브랜치로 이식하고 d3d11 백엔드는 버린다. 계측이 필요하면 `video-perf-investigation`이 그대로 남아 있다
3. **vsync refresh driver = 탑재하되 opt-in** — 멀티GPU 간 vsync 동기화는 어차피 구현해야 하므로 넣되, 기본 활성은 하지 않는다
4. **`wall_layout` = `servo-paint-api`로 승격** (권고 채택)
5. **파일 구조 = 디렉터리형 예제로 분할** (권고 채택)
6. **입력 이벤트 = 비범위** — 표출 전용이므로 winit_wall 창을 통한 입력 감지는 넣지 않는다
7. **servoshell = 폐기하지 않음** — 사용자 UI/UX 전용으로 병행 유지

## 설계

### 1. `wall_layout`을 `servo-paint-api`로 승격

`ports/servoshell/wall_layout.rs`(661줄, 단위 테스트 13개 포함)를 `components/shared/paint/wall_layout.rs`로 옮기고 `servo`에서 재수출한다. winit_wall의 인라인 `mod wall`과 servoshell의 로컬 파일은 **둘 다 삭제**한다.

**이 위치인 이유.** 선례가 있다 — winit_wall이 이미 쓰는 `enumerate_display_topology`/`spatial_order`가 같은 크레이트의 `rendering_context.rs:433,518`에 있고 `servo`가 재수출한다. 더 중요하게는 가드밴드 값의 소비처인 `RenderingContext::present_inset`이 같은 크레이트에 있어, **정본이 한 곳에 모인다.** 이 브랜치 최종 리뷰가 지적한 "공유된 건 산식이고 값이 아니었다"는 발산을 구조적으로 막는다.

**구현 시 주의.** 현재 파일은 `use servo::{CSSPixel, DeviceIndependentPixel, DeviceIntRect, DevicePixel}`로 파사드 크레이트에 의존한다. `paint_api`는 `servo`에 의존할 수 없으므로(순환), 이 타입들을 원산지 크레이트(`webrender_api`/`euclid` 단위)에서 직접 가져오도록 바꾼다. `paint_api`의 `Cargo.toml`에 `serde_json`을 추가한다(현재 `serde`만 있음). 가시성은 `pub(crate)` → `pub`.

**문서 갱신.** CLAUDE.md의 검증 명령 `cargo test -p servoshell wall_layout --lib`가 새 크레이트 기준으로 바뀐다. 테스트 개수 표기도 정정한다 — CLAUDE.md에는 11로 적혀 있으나 실제는 13개다(확인함). 단 **루트 CLAUDE.md는 이 저장소의 git 밖에 있어 커밋할 수 없다**. 편집만 하고 완료 기준에서는 커밋을 요구하지 않는다.

### 2. winit_wall 이식 + d3d11 제거

`video-perf-investigation:components/servo/examples/winit_wall.rs`를 이 브랜치로 가져오되:

- `use servo::Dx11RenderingContext`(29행)와 `Backend` 열거, `--backend` 인자 파싱, `Dx11RenderingContext::new*` 분기를 제거한다. 렌더링 컨텍스트는 `WindowRenderingContext`(surfman/ANGLE) 단일 경로로 고정
- 인라인 `mod wall`(667행~)을 삭제하고 `servo::wall_layout`을 쓴다
- `--capture` 경로는 유지한다(리드백 검증에 유용, d3d11 비의존)

### 3. 가드밴드 — `present_inset` 주입

타일 창 생성 시 `WallLayout::tile_render_insets(tile_index, hidpi)`의 결과를 그 타일의 `RenderingContext::set_present_inset`으로 주입한다. **이 함수가 가드밴드 값의 유일한 산출처**이며, winit_wall은 어떤 경로에서도 inset을 자체 계산하지 않는다 — 이 브랜치 최종 리뷰가 "공유된 건 산식이고 값이 아니었다"로 지적한 발산을 그대로 재연하지 않기 위함이다.

진단 로그는 servoshell 관례대로 `eprintln!("wall: ...")`로 낸다 — 로거 초기화 이전에 창이 생성될 수 있어 `info!`는 유실된다.

### 4. DPI 변경 시 재주입

`WindowEvent::ScaleFactorChanged`에서 해당 타일의 inset을 재계산해 다시 주입한다. 이 핸들러는 현재 winit_wall에 없다.

### 5. borderless fullscreen

타일의 `rect` 크기가 대상 디스플레이 크기와 정확히 일치할 때만, 그 디스플레이에 대응하는 winit 모니터로 `set_fullscreen(Some(Fullscreen::Borderless(monitor)))`를 건다(flip-model present 자격). servoshell `desktop/headed_window.rs:283-287`과 동일 조건이며, 크기가 다르면 일반 창으로 둔다. 토폴로지 폴백 경로에서는 winit 모니터 핸들이 없을 수 있으므로 그때도 일반 창으로 둔다.

### 6. vsync refresh driver — opt-in

`ports/servoshell/desktop/vsync_refresh_driver.rs`에 해당하는 배선을 winit_wall에 넣되, **`SERVO_WIN_VSYNC=1`일 때만 활성화**한다(기본 off). servoshell도 같은 변수로 opt-in한다(`desktop/headed_window.rs:394`). 알려진 부작용은 이 환경에서 `DwmFlush`가 스핀-웨이트로 동작해 코어 1개를 상시 소모한다는 점이며, 기본 off로 두는 근거다. 멀티GPU 간 vsync 동기화 작업의 기반으로 삼는다.

### 7. 파일 구조

단일 파일 906줄에서 인라인 `mod wall`(약 240줄)과 d3d11 분기가 빠지고 갭 1~4가 채워지면 다시 700줄대가 된다. 디렉터리형 예제로 나눈다.

- `components/servo/examples/winit_wall/main.rs` — 인자 파싱, 이벤트 루프, 앱 수명
- `components/servo/examples/winit_wall/tile.rs` — 타일 창·렌더링 컨텍스트·paint target 수명, inset 주입

Cargo가 지원하는 표준 형태이며, 타일 수명 관리가 이 파일에서 가장 복잡한 부분이라 경계가 자연스럽다.

## 테스트

### 정적

- `wall_layout` 단위 테스트 13개가 새 위치에서 통과 (3x1 중간 타일 양쪽 가드밴드, 분수 DPI 케이스 포함)
- `cargo check`/`cargo build`: servoshell + `--example winit_wall` 양쪽
- `rustfmt --edition 2024 --check` (touched 파일), `git diff --check`

### servoshell 무회귀 (필수 — `wall_layout`이 크레이트 밖으로 나가므로)

가드밴드 눈금 프로브로 이음매 라벨 위치 확인(경계 `1920,480` 라벨이 `1800,480`과 정확히 120px 간격 — 밀리면 152px), `scroll_offsets=matched`, 프레임 배리어 완료, panic/missed/pending 0.

### winit_wall 기능

- 2x1 월 `ready=2/2` 배리어 (기존 winit_wall 검증 기준)
- 가드밴드: 눈금 프로브에서 각 타일 구석 라벨이 그 타일 `rect` 원점과 일치
- DComp on/off A/B — egui가 없으므로 servoshell에서 관측된 하단 밴드·잔여 blit이 **없어야** 정상
- vsync opt-in on/off 양쪽 정상 동작
- 미디어 표출 회귀 1건 — 비디오 그리드 + `SYNC_GROUP` lockstep 성립 확인

### 운영 제약 (전부 기왕에 기록된 함정)

- 빌드는 `subst W: <repo루트>` 후 짧은 경로에서 (mozangle `build.rs:155` Os error 206)
- 예제 빌드: `cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl` — `-p servo`·`no-wgl` 필수
- 실행 전 ANGLE/gstreamer DLL을 exe 옆에 복사 (아니면 egl 패닉)
- **release로 검증** — debug는 월 + 동적 video src에서 `MakeCurrentFailed` 크래시
- 로그 캡처는 `Start-Process -RedirectStandardError` + `CloseMainWindow()` (PowerShell `2>`는 0바이트가 되는 사례 있음)
- **GUI 육안 판정은 사용자 몫** — 서브에이전트에 위임하지 않는다

## 리스크

| 리스크 | 완화 |
|---|---|
| `wall_layout` 이동이 servoshell을 깨뜨림 | 단위 테스트 13개가 함께 이동해 회귀 가드가 됨 + servoshell 무회귀 검증을 완료 기준에 포함 |
| `paint_api`에 `serde_json` 유입 | 같은 크레이트에 이미 DXGI 열거가 있어 선례상 무리 없음. 전용 크레이트는 포크 델타가 더 큼 |
| winit_wall이 servoshell 대비 검증량이 적음 | servoshell을 병행 유지하므로 표출 실패 시 되돌아갈 곳이 있음 |
| 저쪽 브랜치 64커밋과 영구 분기 | 의도된 결정. 계측 자산은 `video-perf-investigation`에 보존 |

## 비목표

- `video-perf-investigation`의 나머지 64커밋(WR picture-caching 실험 패치, video pace 등) 이식
- `Dx11RenderingContext` / `--backend d3d11`
- winit_wall 창의 입력 이벤트 처리 (마우스/휠 가상 뷰포트 좌표 리맵)
- servoshell 폐기 — UI/UX 전용으로 계속 유지
- 포터블 패키지(ServoWallPackage 상당)의 winit_wall 판 재구성
- 멀티GPU 간 vsync 동기화 알고리즘 자체 (이번엔 driver 배선까지만; 동기화는 후속 과제)

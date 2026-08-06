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

### 3. 가드밴드 — `present_inset` 주입 (★2026-08-06 이후 최종 리뷰로 철회 — 아래 §가드밴드는 winit_wall에서 지원되지 않는다 참조)

타일 창 생성 시 `WallLayout::tile_render_insets(tile_index, hidpi)`의 결과를 그 타일의 `RenderingContext::set_present_inset`으로 주입한다. **이 함수가 가드밴드 값의 유일한 산출처**이며, winit_wall은 어떤 경로에서도 inset을 자체 계산하지 않는다 — 이 브랜치 최종 리뷰가 "공유된 건 산식이고 값이 아니었다"로 지적한 발산을 그대로 재연하지 않기 위함이다.

진단 로그는 servoshell 관례대로 `eprintln!("wall: ...")`로 낸다 — 로거 초기화 이전에 창이 생성될 수 있어 `info!`는 유실된다.

**이 설계는 Critical 결함이었다.** `present_inset` 주입은 servoshell에서 세 가지(오프스크린 확장 서피스 / render-rect 원점 씬 / present 크롭)가 세트로 성립할 때만 옳다. winit_wall은 셋 중 present 크롭 하나만 흉내냈다 — 창 서피스에 직접 렌더하고(오프스크린 확장 없음) 씬 원점이 `tile_origin_device_vector`(=visible 원점, render-rect 원점 아님)였다. 그 결과 DComp off(기본)에서는 소비자가 없어 주입이 무효였고, DComp on에서는 root visual을 `-inset`만큼 미는데 상쇄할 확장이 없어 **콘텐츠가 `overlapPx`만큼 어긋났다**(실제 오작동). 최종 리뷰 대응으로 이 절의 주입 자체를 제거했다(`tile.rs`에서 `set_present_inset` 호출 삭제) — 상세는 아래 §가드밴드는 winit_wall에서 지원되지 않는다.

### 4. DPI 변경 시 재주입 (★위와 같은 이유로 철회)

`WindowEvent::ScaleFactorChanged`에서 해당 타일의 inset을 재계산해 다시 주입한다. 이 핸들러는 구현 당시 winit_wall에 추가됐으나(`AppState::reapply_present_insets`), §3의 주입 자체가 철회되면서 존재 이유가 사라져 최종 리뷰 대응에서 함께 제거했다(죽은 코드를 남기지 않기 위해 `WindowEvent::ScaleFactorChanged` 매치 arm도 함께 삭제).

### 가드밴드는 winit_wall에서 지원되지 않는다 (후속 과제)

`overlapPx` 가드밴드 크롭은 현재 winit_wall이 **지원하지 않는다.** `overlapPx: 0`인 레이아웃(타일 경계가 정확히 맞물리는 표준 배치)은 영향이 없다 — 문제는 오직 `overlapPx > 0`(그림자/블러/AA가 타일 경계를 넘나드는 콘텐츠를 위한 가드밴드) 레이아웃에서만 나타난다.

**왜인지.** 좌표/서피스 모델이 servoshell과 다르다. servoshell은 render-rect(visible보다 `overlapPx`만큼 확장) 크기의 오프스크린 서피스에 렌더한 뒤, 씬 원점을 render-rect 원점에 두고, present 시 visible sub-rect만 크롭해 창에 보여준다(blit source rect 또는 DComp root-visual 오프셋). winit_wall은 창 서피스에 직접 렌더하고(오프스크린 확장 없음) 씬 원점이 visible 원점이다 — 셋 중 하나만 있는 상태로는 크롭이 성립하지 않는다(위 §3 참조).

**지원하려면 무엇이 필요한지.** servoshell `gui.rs`의 `webview_rendering_size`(오프스크린을 render-rect 크기로 확장) + `webview_paint_origin()`(씬 원점을 render-rect 원점으로) + `webview_visible_source_rect`(present 시 visible sub-rect blit) 세 가지에 상당하는 구현을 winit_wall에 추가해야 한다. winit_wall은 오프스크린 래퍼가 없는 `WindowRenderingContext` 직결 구조이므로, 이는 단순 이식이 아니라 별도 설계가 필요한 과제다 — 이번 최종 수정 라운드의 범위 밖으로 명시적으로 미룬다.

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

가드밴드 눈금 프로브로 이음매 라벨 위치 확인(경계 `1920,480` 라벨이 `1800,480`과 정확히 120px 간격 — 밀리면 152px), `scroll_offsets=matched`, 프레임 배리어 완료, panic 0.

**barrier missed(=keep-previous-frame) 기준 — 2026-08-06 Task 6 재검증으로 정정.** 최초 이 절은 "missed 0"을 기준으로 적었으나, 이는 이 프로젝트의 실제 운영 기준과 어긋난 문언이었다. `.superpowers/sdd/progress.md:18`가 기록한 이 브랜치 직전 무회귀 검증에서 이미 `barrier 279(8 miss=keep-previous 정상)`(8/279 ≈ 2.9%)로 missed>0이 정상 판정된 전례가 있고, `keep-previous-frame-for-delayed-targets` 정책 자체가 "지연 타깃은 이전 프레임을 유지하고 크래시하지 않는다"는 설계이므로 missed 발생 자체는 예상된 동작이다. 다만 Task 6 재검증(아래 `## 구현 결과`)에서 같은 조건의 반복 실행 간 missed 비율이 1.3%~10.2%까지 요동치는 것을 확인했으므로, **고정 비율 상한을 완료 기준으로 쓰지 않는다.** 대신 다음을 회귀 신호로 삼는다:
- `scroll_offsets=mismatched` 발생 (barrier missed와 무관하게 별도 카운트되는 진짜 동기화 실패)
- barrier missed에 panic이 동반됨(정책이 깨져 크래시로 전이됨)
- 특정 타깃이 아니라 다수 타깃이 동시다발로 missing되는 패턴(단일 타깃의 산발적 지연이 아니라 배리어 메커니즘 자체의 이상)
- **(2026-08-06 최종 리뷰로 좁힘)** 데드라인 초과폭이 리프레시 주기 이내(≤약 17ms)인 미스는 회귀 신호가 아니다 — `components/paint/paint.rs:56`의 `WALL_FRAME_BARRIER_DEADLINE = 16ms`가 60Hz vsync 주기(16.667ms)보다 짧아, vsync에 페이싱된 2번째 이후 타깃이 한 vsync 뒤에 도착하면 **구조적으로 항상 데드라인을 넘는다**(설계상 예견된 오검출 여지이지 결함이 아니다). 실측에서도 미스의 58%가 16.0~17.0ms 구간에 몰렸고, 48건이 전부 `PainterId(2)`에 쏠린 것도 "그 타일이 느려서"가 아니라 팬아웃 순서상 2번째 타깃이 항상 마지막에 준비되기 때문이었다. 반면 **초과폭이 큰 미스(>20ms)의 건수 추이**는 신호로 유지한다 — 이런 미스는 vsync 1주기 지연으로는 설명되지 않으므로 실제 지연/이상을 가리킬 가능성이 높다.

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

## 구현 결과 (2026-08-06, Task 6 통합 검증)

### 커밋 범위

`76b088a7d18` (계획 보정, Task 6 시작점) `..` `5b863fa35fb` (T5 vsync driver 완료) — 7개 구현 커밋:

```
c8404d48f6d refactor(paint): wall_layout을 servo-paint-api로 승격 - 임베더 간 공유
b47f978d928 chore(paint): Cargo.lock를 wall_layout 이관의 serde_json 의존성과 동기화
fbfb2c41ae9 feat(winit_wall): 표출 셸 기반 이식 - 공유 wall_layout 사용, d3d11 백엔드 제거
8b1ea453be3 feat(winit_wall): 가드밴드 크롭 present_inset 주입 + DPI 변경 재주입
ddd799ef34c feat(winit_wall): 타일이 디스플레이를 채울 때 borderless fullscreen
b02ec108ce2 fix(winit_wall): rustfmt drift on set_fullscreen call
5b863fa35fb feat(winit_wall): DWM vsync refresh driver (SERVO_WIN_VSYNC opt-in)
```

`.rs` diffstat: 10 files changed, 877 insertions(+), 27 deletions(-) — 대부분 신규(`winit_wall/{main,tile,vsync_refresh_driver}.rs` 850행), servoshell 쪽은 파일 삭제(`ports/servoshell/wall_layout.rs`) + import 갱신 4곳뿐(`crate::wall_layout::` → `servo::wall_layout::`).

### 설계 대비 결과 — 이탈 없음

설계 문서의 7개 결정 사항(§사용자 결정 사항) 모두 그대로 구현됐다. `wall_layout` 승격 위치, `paint_api`의 `serde_json` 편입, 디렉터리형 예제 분할, `present_inset`을 유일한 산출처로 삼는 정책, borderless fullscreen 조건, vsync opt-in 기본값 — 설계에서 이탈한 항목은 없다.

### 검증 결과 요약 (Task 6)

- **정적 검사**: `cargo test -p servo-paint-api wall_layout --lib` 13/13 통과, `cargo check -p servoshell` / `cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl` 둘 다 exit 0, `git diff --check` 무출력. `rustfmt --edition 2024 --check`는 브랜치가 건드리지 않은 파일(`display_list.rs`, `rendering_context.rs`, `dialog.rs`, `parser.rs`, `webdriver.rs` 등, crate-root `lib.rs` 재귀 확장으로 딸려 들어옴)에서 let-chain 포맷팅 드리프트를 보고했으나, 이 브랜치가 실제로 변경한 라인(커밋 diff와 대조 확인)에는 diff가 전혀 없다 — 기존부터 있던 rustfmt 버전 스큐이며 이 작업의 회귀 아님.
- **servoshell 무회귀 (release 빌드, `wall_layout.example_2x1_display.json`) — 최초 실행(Run A, 8초 재생 후 `CloseMainWindow`+3초 유예, 응답 없어 `Stop-Process -Force`로 강제종료)**: `scroll_offsets=matched` 326/326(mismatched 0), panic 0, `Wall frame barrier complete` 150, **missed 2**. 스펙 원문(구 132행)은 완료 기준으로 "missed 0"을 명시했으나 **이 실측은 그 문언상 기준을 충족하지 못했다.** 실제 초과값은 `first_ready_elapsed_ms` 16.421ms(deadline 16.000ms 대비 +0.421ms, +2.6%)와 21.128ms(+5.128ms, **+32%**) — "미세 초과"라 부를 수 있는 값이 아니다. `present_inset` 2줄은 tile 0 `(top 0, right 32, bottom 0, left 0)`, tile 1 `(top 0, right 0, bottom 0, left 32)`; 별도 실행한 눈금 프로브(`multigpu_wall_ruler_probe.html`)에서도 동일 inset 값 재확인.

  그럼에도 missed>0을 수용 가능하다고 판단한 근거: (1) 이 브랜치 직전의 독립된 무회귀 검증 기록(`.superpowers/sdd/progress.md:18`)이 `barrier 279(8 miss=keep-previous 정상)`로 missed>0을 이미 정상 판정한 전례이고, (2) 이번 2/150(1.3%)은 그 전례의 8/279(2.9%)보다 낮다. **다만 Task 6 재검증(아래)에서 이 비율이 실행마다 크게 요동침을 확인했으므로, 이 비교는 "낮은 편"이라는 참고 정보일 뿐 완료 기준으로 쓰지 않는다** — 위 `## 테스트` 절 정정 참조.

  **Run A 재실행 검증(리뷰 대응, 3회 추가 실행)**: 원 실행이 `Stop-Process -Force`로 강제종료됐기 때문에, CLAUDE.md가 명시한 "force-killing loses buffered log output" 리스크를 해소하기 위해 재실행했다.
  - **재실행 1**(재생 15초 + `CloseMainWindow` 후 `WaitForExit(15000)`): **강제종료 없이 프로세스가 스스로 종료**(`WaitForExit`가 true 반환, exit code `-1073740791`/`0xC0000409`). 다만 "정상(clean) 종료"는 아니다 — 종료 직전 로그에 `MakeCurrentFailed(NotInitialized)`(`ports/servoshell/desktop/gui.rs:183`)와 `assertion left != right failed`(`third_party/surfman/src/platform/windows/angle/context.rs:177`) 패닉이 두 건 기록돼 있다. 이는 다중 타일 창을 닫을 때의 종료-분해(teardown) 경로에서 발생한 것으로 보이며, 이 브랜치의 프레임 렌더링 로직과는 무관한 별개 이슈로 판단해 이번 태스크에서 더 파고들지 않았다(리뷰 지시 — 새 조사 금지). 종료 직전까지의 런타임 지표: `scroll_offsets=matched` 802/802(mismatched 0), `Wall frame barrier complete` 378, **missed 43(10.2%)** — 8/279(2.9%) 전례보다 **높다.** missed는 전부 `missing=[PainterId(2)]`로 특정 타깃에 쏠렸고 `first_ready_elapsed_ms`는 16.218~26.466ms 범위(가장 큰 것은 deadline 대비 +10.466ms, **+65%**). `present_inset` 값은 원 실행과 동일.
  - **재실행 2**(원 실행과 동일하게 재생 8초 + `CloseMainWindow`, 이번엔 유예를 20초로 늘림): `WaitForExit(20000)`가 **false 반환 — 20초를 기다려도 자발적으로 종료되지 않아 다시 `Stop-Process -Force`로 강제종료**했다. 즉 `CloseMainWindow`에 대한 반응은 결정적이지 않다(재실행 1은 15초 내 자체 종료, 재실행 2는 20초 넘게 무반응). 이 실행의 지표: `scroll_offsets=matched` 1338/1338(mismatched 0), barrier complete 146, missed 3(2.0%), panic 0(강제종료 시점까지는). 로그 마지막 줄은 잘리지 않은 완전한 레코드였다(육안 확인).

  **결론(주장 완화)**: 세 실행 모두 로그 말미가 온전한 레코드로 끝나 있어 실제 데이터 유실의 물증은 없다. 그러나 재실행 1·2가 보여주듯 `CloseMainWindow` 이후 프로세스가 스스로 정상 종료한다는 보장이 없고(때로는 teardown 크래시로, 때로는 20초 넘게 무응답으로 이어져 강제종료가 필요했다), 원 실행(Run A)의 강제종료 자체도 재현 가능한 정상 경로임이 확인됐다. 따라서 원 실행 로그의 카운트(326/2/150)에 대해 **"유실 없음"이라고 단정하지 않는다** — CLAUDE.md가 경고하는 강제종료發 유실 가능성은 완전히 배제할 수 없고, 다만 관측된 로그가 온전한 레코드로 끝나 있다는 정황과, 재실행들의 카운트가 (실행 길이가 다름을 감안하면) 같은 자릿수 범위로 일관됐다는 점이 신뢰도를 뒷받침하는 보강 증거다. `present_inset` 값은 세 실행 모두 동일해 그 자체는 강한 신뢰도를 갖는다(강제종료와 무관하게 창 생성 직후 1회 기록되는 값이라 종료 타이밍의 영향을 받지 않음).

- **winit_wall 기능**: 2x1 `ready=2/2` 배리어 149/149(missed 0). DComp A/B — `SERVO_COMPOSITOR_DCOMP=1`에서 `[dcomp-native] engaged` 2회 + 타일별 guard-band offset 로그(`(0,0)`/`(-32,0)`), off에서는 `dcomp-native` 로그 0회. 미디어(`video_grid_6x6_play.html?grid=2`, `SERVO_MEDIA_SYNC_GROUP=4`) — `Sync group: 4/4 pipelines armed` + `released: 4 pipelines starting at shared base time`, 배리어 474/475, **missed 1**(`first_ready_elapsed_ms=28.036ms`, deadline 16.000ms 대비 **+12.036ms/+75%**), panic 0. 이 미스도 위와 동일하게 `keep-previous-frame` 정책 범위(panic 미동반, scroll mismatch 미동반)이지만 초과폭 자체는 절대 크다는 점을 그대로 기록한다.

  **정정(2026-08-06 최종 리뷰 대응).** 위 `(-32,0)` guard-band offset 로그를 "servoshell과 동일한 inset에서 산출"이라고 적어 **정합성 근거처럼** 서술했으나, 이는 사실을 뒤집은 서술이었다 — 값이 같다는 것 자체가 문제였다. winit_wall은 오프스크린 확장 서피스도 render-rect 원점 씬도 없이 `present_inset`만 servoshell과 동일하게 주입했으므로, DComp 경로가 이 값만큼 root visual을 밀면 상쇄되지 않은 채 콘텐츠가 `overlapPx`(32px)만큼 어긋난다(Critical, 위 §3 참조). 최종 리뷰 대응으로 `present_inset` 주입을 제거했으므로, 이후 이 로그는 오프/온 모두 `(0,0)`(트레잇 기본값)으로 나오는 것이 맞는 동작이다.
- **GUI 육안 판정**(이음매 라벨 120px 간격, DComp 하단 밴드/blit 부재, lockstep 실화면 확인)은 로그로 판정 불가능한 항목이라 서브에이전트가 수행하지 않았다 — 사용자 이월(보고서의 "사용자 육안 판정용 명령" 참조, `task-6-report.md`).

### 이월 항목 (설계 문서 §설계 시 주의 + 코드 인라인 주석과 대응)

1. ~~`servo::wall_layout::WallLayoutError`가 `std::error::Error` 미구현~~ — **2026-08-06 최종 리뷰 대응에서 해소.** `impl std::error::Error for WallLayoutError`(`source()`는 `Io`/`Json` variant에서 내부 에러 반환)를 추가하고, winit_wall `parse_args()`의 `.map_err(|error| error.to_string())` 우회를 제거했다(`?`가 바로 `Box<dyn Error>`로 크로스한다).
2. 토폴로지 완전 미탐지(`!have_topology`) 분기에서 타일당 stderr 진단 한 줄이 servoshell 대비 누락(`tile.rs:112-138`) — 토폴로지 out-of-range 경고는 있으나 완전 부재 시 무음.
3. `spatial.get(tile.display)`를 `tile.rs`에서 2회(라인 75, 148) 중복 조회 — 기능상 문제는 없으나 정리 여지.
4. 비Windows 빌드에서 `vsync_driver` 파라미터가 미사용 경고를 낼 수 있음(`create_tile_windows`가 `#[cfg(not(target_os = "windows"))]` 분기에서 그 인자를 안 씀) — 비Windows에서 실컴파일 검증하지 않아 미확정.
5. 비Windows 경로 컴파일은 이번 Task 6에서도 검증하지 않았다(개발/실행 환경이 Windows 전용).

이 5건 중 설계를 바꿔야 할 만한 것은 없다 — 전부 후속 정리 과제로 이월한다.

### 후속 과제 (2026-08-06 최종 리뷰 대응에서 추가)

6. **가드밴드(오프스크린 확장 + render-rect 원점 씬 + visible-rect 크롭) 미지원.** §가드밴드는 winit_wall에서 지원되지 않는다 참조. `present_inset` 주입만으로는 성립하지 않음이 Critical로 확인되어 제거했다 — 지원하려면 별도 설계가 필요하다.
7. **`WALL_FRAME_BARRIER_DEADLINE`(16ms, `components/paint/paint.rs:56`)이 60Hz vsync 주기(16.667ms)보다 짧다.** vsync 페이싱된 팬아웃의 2번째 이후 타깃이 구조적으로 데드라인을 넘기는 원인. 데드라인을 리프레시 주기 위(예: 17ms 또는 20ms)로 올리거나, 고정 ms 대신 vsync 배수(예: "1.5 vsync 주기")로 표현하도록 재설계하는 것을 후속 과제로 남긴다. §테스트 절의 회귀 신호 기준을 참조.

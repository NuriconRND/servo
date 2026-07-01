# WebRender picture-caching 우회 설계 (멀티-GPU 비디오 월)

- 날짜: 2026-07-01
- 브랜치: `video-perf-investigation`
- 관련 조사: `video_grid_perf_summary.txt`, 메모리 `video-grid-composite-bottleneck.md`

## 1. 문제 (Problem)

한 GPU가 담당하는 큰 단일 surface(예: 3840×3240)에 16개 FHD30 `<video>` 그리드를 렌더하면
프레임레이트가 ~1fps로 붕괴한다. 같은 영상을 16개 독립 프로세스로 돌리면 정상이다.

측정으로 확정된 근본 원인:
- 병목은 `renderer.update()` 내부의 **WebRender picture caching(타일 캐시) 매 프레임 dirty/의존성 추적**.
- 비용은 (매 프레임 변하는 독립 primitive 수) × (뷰포트 면적)으로 폭발, ~8–10 Mpx에서 절벽.
- all-dynamic 콘텐츠(16영상 전부 매 프레임 변경)에선 캐싱 이득이 **0**인데 추적 비용만 폭발.
- 근거: `picture_tile_size` 스윕(256px→~3366ms, 1타일→~400-570ms 회복 안 됨),
  glFinish 펜스(GPU 래스터·present 배제), 1영상@12.4Mpx는 ~3ms(면적 단독 아님).

## 2. 목표와 제약 (Goals / Constraints)

- **목표:** 큰 단일 per-GPU surface에서 all-dynamic 비디오 월의 프레임레이트 회복.
- **제약(설계 확정):**
  - "GPU 하나당 타일 1개" 원칙 유지 → 한 GPU 영역을 서브surface로 쪼개는 우회는 **금지**.
    따라서 picture caching 자체를 우회해야 함.
  - Servo / Paint 등 다른 모듈 수정 **0**. 변경은 WebRender 포크 내부로 격리.
  - 정적/혼합 콘텐츠(맵, WebGL 등 다른 월)의 캐싱 이득은 **기본값으로 유지**(회귀 없음).
  - 런타임 토글 가능.
- **성공 기준:** 16영상@3840×3240에서 `Render perf`의 `avg_update_ms`가 수백 ms→수십 ms 수준으로
  급감하고 `composite_fps`가 회복(목표 ≥ 24, 이상적으로 ~30+). env off 시 기존과 동일(회귀 없음).
  화면 렌더 결과 정상(누락/깨짐 없음).

## 3. 채택 접근 (Chosen Approach)

**WebRender 포크 + env 변수 게이트로 picture caching 비활성.**

검토한 대안:
- (기각) 서브surface 타일 분할 — "GPU당 타일 1개" 원칙 위반.
- (기각) Servo pref → WebRenderOptions → FrameBuilderConfig 배선 — WR+Servo+Paint 3곳 수정, blast radius 큼.
- (채택) WebRender 포크 내부 env 게이트 — Servo/Paint 무수정, 변경이 포크 1곳에 격리, 런타임 토글,
  정적 콘텐츠 기본 캐싱 유지.

WebRender 0.68에는 picture caching을 끄는 설정 필드가 없음(`enable_picture_caching`는 이전 버전에서 제거).
따라서 소형 포크 패치가 불가피하며, 이것이 다른 모듈 영향이 가장 작은 선택.

## 4. 변경 표면 (Change Surface)

- **수정 크레이트: 포크된 `webrender` 1개** (`src/tile_cache.rs`, 필요 시 작은 env 헬퍼).
  `webrender_api` / `wr_malloc_size_of`는 레지스트리 그대로 → 포크 표면 최소화.
- **통합:** `servo/Cargo.toml`의 기존 `[patch.crates-io]`에 준비된 주석 슬롯 활성화:
  ```
  [patch.crates-io]
  webrender = { path = "../webrender/webrender" }
  ```
  (webrender_api/wr_malloc_size_of 패치는 넣지 않음 — 코드 변경이 `webrender`에만 있으므로.)
- **Servo/Paint: 무수정.**

## 5. 동작 (Mechanism)

1. 포크의 `TileCacheBuilder::build()`(또는 `create_tile_cache`, `tile_cache.rs`) 진입 시 env
   `WR_DISABLE_PICTURE_CACHING`를 확인.
2. 켜져 있으면 루트 slice 콘텐츠를 `PictureCompositeMode::TileCache { slice_id }` 대신
   **`None`(passthrough) 모드** picture로 생성하고 `tile_caches` 삽입을 스킵(`tile_cache.rs:637-652` 부근).
3. 결과: `picture.rs:5415-6009`의 매 프레임 타일 dirty/occlusion/descriptor 머신러리가 **실행 안 됨**.
   콘텐츠는 매 프레임 재래스터(all-dynamic엔 어차피 필요, 측정상 draw ~15-40ms).
4. env는 **scene 빌드 시점**에 읽힘(프레임마다 아님) → 오버헤드 무의미, 실행 시작 시 결정.

## 6. 리스크 & 폴백 (Risks / Fallback)

- **핵심 리스크:** `None`-모드 루트 picture가 프레임버퍼에 올바로 렌더되는지.
  - agent Option A(early-return 빈 tile cache)는 primitive 유실 위험 → 채택 안 함.
  - 채택 Option B(`None` passthrough + tile_caches 삽입 스킵)는 primitive를 일반 picture로 유지 → 렌더됨.
  - **구현 시 실측 검증 필수**(화면 정상 + fps 회복).
- **폴백:** `None`-모드가 렌더 문제를 일으키면, TileCache 구조는 유지하되 per-frame dirty-tracking만
  우회하는 지점(`picture.rs` tile 업데이트 진입부)을 대안으로 재탐색.
- subpixel AA: `None` composite picture는 `SubpixelMode::Allow`로 이미 처리됨(소스 확인).
- 호환: compositor / hit-testing / native surface 모두 TileCache 분기 안에서만 `tile_caches` 참조 →
  타일 캐시가 없으면 해당 경로 자체가 실행 안 됨(안전).

## 7. 검증 계획 (Verification) — 기존 계측 재사용

- `RUST_LOG=warn,paint=info` + `WR_DISABLE_PICTURE_CACHING=1`로 16영상@3840×3240 실행,
  `Render perf`에서 `avg_update_ms` 급감 + `composite_fps` 회복 확인.
- env **off**로 동일 실행 → 기존 수치와 동일(회귀 없음) 확인.
- 화면 육안(또는 스크린샷)으로 렌더 정상 확인.
- 정적/맵 페이지에서 env off 기본값이 캐싱 유지(성능 회귀 없음) 스모크.
- 실행 스크립트: `scratchpad/run_perf_measure.ps1` 재사용(env 추가만).

## 8. 포크 로지스틱스 (Fork Logistics)

1. 레지스트리 캐시의 `webrender-0.68.0` 소스를 `../webrender/webrender`로 복사(읽기전용 캐시 → 쓰기 가능 위치).
   - 필요한 최소: `webrender` 크레이트 하나. 그 `Cargo.toml`이 `webrender_api`/`wr_malloc_size_of`를
     crates.io 버전으로 의존하면 그대로 두고 `webrender`만 path 패치.
2. `tile_cache.rs`에 env 게이트 패치 적용.
3. `servo/Cargo.toml`의 `[patch.crates-io]`에서 webrender path 슬롯 주석 해제.
4. 증분 재빌드(`cargo build --release -p servo --example winit_wall --features media-gstreamer,no-wgl`).

## 9. 범위 밖 (Out of Scope)

- picture caching의 근본 알고리즘 개선/업스트림 기여.
- WebRender 버전 업그레이드.
- 정적 콘텐츠 자동 감지로 캐싱 on/off 자동화(수동 env 토글로 충분).
- 멀티-GPU 실제 fan-out 검증(별도 Phase 작업).

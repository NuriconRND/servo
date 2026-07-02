# 6×6 비디오 그리드 성능 테스트 — 설계

날짜: 2026-07-02
대상 저장소: `servo` (nuriconrnd/multigpu-tiled-wall)

## 목적

`Wildlife_FHD30fps_counter_10Mbitrate.mp4`(1920×1080, 30fps, ~10Mbps H.264)를
**6×6 = 36개 독립 `<video>` 디코더**로 모니터 1개에 꽉 채워 표출하고,
Servo(servoshell)의 다중 비디오 디코드 + 합성 성능을 측정한다.

핵심 질문: "동일 로컬 파일 36개 스트림을 동시에 디코딩/합성할 때 Servo가
디코드 스루풋(초당 디코드 프레임)과 표시 프레임레이트(rAF FPS)를 유지하는가?"

## 산출물

1. `servo/tests/html/video_grid_6x6_perf.html` — 테스트 페이지 (주 산출물)
2. `servo/etc/multigpu/run_video_grid_6x6.ps1` — 단일 창 런처

## 구조

### 페이지 (`video_grid_6x6_perf.html`)

- **레이아웃**: CSS Grid, `grid-template-columns: repeat(N, 1fr)`,
  `grid-template-rows: repeat(N, 1fr)`, `width:100vw; height:100vh`.
  모니터 해상도와 무관하게 창을 꽉 채운다(FHD/4K 자동 대응). 배경 검정.
- **타일**: `N*N`개 `<video autoplay muted loop playsinline>`,
  `object-fit: cover`, `src="../Wildlife_FHD30fps_counter_10Mbitrate.mp4"`.
  각 엘리먼트가 동일 파일을 **개별 디코딩**(36개 디코드 파이프라인 = 실제 부하).
  타일 사이 1px 구분선(시각 확인용, 성능 영향 무시 가능).
- **DOM 생성**: 스크립트로 `N*N`개 `<video>`를 동적 생성해 그리드에 삽입.

### 지표 오버레이 (좌상단, 매 1초 갱신)

`requestAnimationFrame` 루프로 rAF tick을 세고, 1초마다 아래를 갱신한다.

| 지표 | 계산 | 이상값(6×6) |
|------|------|-------------|
| `tiles` | 생성된 video 수 | 36 |
| `rAF FPS` | 최근 1초 rAF tick 수 | ~60 |
| `decoded/s` | 전체 video `getVideoPlaybackQuality().totalVideoFrames` 델타 합 | ~1080 (30fps×36) |
| `dropped/s` | 전체 `droppedVideoFrames` 델타 합 (**핵심 부하 지표**) | 0 |
| `playing` | `!paused && readyState>=3`인 video 수 | 36/36 |
| `elapsed` | 경과 초 | — |

`getVideoPlaybackQuality()` 미지원 시 `webkitDecodedFrameCount` 폴백,
둘 다 없으면 `decoded/s: n/a`.

### 설정 (URL 파라미터, 기본값 6×6)

- `?grid=N` → N×N 그리드 (기본 6). 4×4·8×8 등 비교 테스트를 코드 수정 없이 수행.
- 모든 비디오는 로드되는 대로 즉시 동시 재생(스태거 없음 = 최악부하 기준).

### 런처 (`run_video_grid_6x6.ps1`)

`run_video_4k_wall.ps1` 패턴을 재사용하되 **단일 창**으로:

- `--wall-layout` / `--wall-all-tiles` **제거** (월 아님, 모니터 1개).
- `--window-size 1920x1080` (파라미터로 조정 가능; 대상 모니터 해상도에 맞춤).
- `SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF=1` 설정 — servosrc EnoughData/NeedData
  백오프로 인한 ~400ms 주기 스터터 방지(로컬 파일이라 안전, 메모리 bounded).
- `file:///.../tests/html/video_grid_6x6_perf.html` 로드. HTTP 서버 불필요(순수 HTML).
- 파라미터: `-Profile release|debug`, `-Grid N`(URL `?grid=` 전달),
  `-WindowSize WxH`, `-DurationSec`(스모크), `-Detach`.
- stdout/stderr를 `target/multigpu_logs/`에 기록(기존 스크립트와 동일).

## 측정 방법

1. `run_video_grid_6x6.ps1` 실행 → 단일 servoshell 창.
2. 창을 대상 모니터로 이동/최대화(또는 `--window-size`로 지정).
3. 오버레이의 `rAF FPS`, `decoded/s`, `dropped/s`로 실시간 판정.
4. (선택) 외부 계측: `PresentMon-2.3.1-x64.exe`(관리자, servoshell 포그라운드
   필수 — 가려지면 present 오탐), Bandicam.

## 판정 기준(참고)

- **정상**: rAF FPS ≈ 60, decoded/s ≈ 1080, dropped/s ≈ 0, playing 36/36.
- **디코드 병목**: decoded/s < 1080, dropped/s 증가.
- **합성 병목**: decoded/s 정상인데 rAF FPS < 60.

## 범위 밖 (YAGNI)

- 다중 GPU 팬아웃/월 레이아웃(이건 단일 모니터 테스트).
- 서로 다른 영상 파일 혼합, 오디오, 컨트롤 UI.
- 자동 CSV/리포트 저장(오버레이 육안 + 외부 계측으로 충분).

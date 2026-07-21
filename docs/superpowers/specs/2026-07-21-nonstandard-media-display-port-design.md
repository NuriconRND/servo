# 비표준 미디어 표출 기능 재이식 설계

- 날짜: 2026-07-21
- 대상 브랜치: `multigpu-tiled-wall` (HEAD `b0a7ce85ed5`)
- 출처: `nonstandard-media-formats`(31bb7d8) → `video-perf-investigation`(c8826e5) 체인

## 1. 목표

표준 브라우저가 지원하지 않는 미디어를 월(video wall)에서 표출하기 위해 별도 feature 브랜치 체인에 개발된 기능을, 현재 브랜치에 **표준 `<img>`/`<video>`/`navigator.mediaDevices` 경로로** 재이식한다. 커스텀 DOM 엘리먼트는 도입하지 않는다.

### 이번 사이클 범위 (IN)
- **#1 확장 이미지 포맷**: tiff / exr / qoi / JPEG XL 등을 표준 `<img>`로 표출
- **#2 확장 비디오 컨테이너 + RTSP**: mkv/avi/wmv 등 컨테이너를 표준 `<video>`로, `rtsp://`/`rtsps://` 스트림을 `<video src>`로 표출
- **#4 getDisplayMedia**: 화면 캡처(`navigator.mediaDevices.getDisplayMedia`)

### 제약
- **GStreamer 1.22.8 유지** (현재 브랜치의 DComp/비디오 월 파이프라인 검증 기반). 버전 스왑 없음.
- 모든 신규 동작은 **pref 게이트, 기본 off** — pref off 시 현재 브랜치 동작과 바이트 동등(월 파이프라인 무회귀).

### 범위 밖 (OUT — 이번 사이클)
- 커스텀 엘리먼트: `ximageelement.rs`, `rtspstreamelement.rs` 및 그 webidl / `create.rs`·`element.rs`·`node.rs`·`virtualmethods.rs`·`html/mod.rs` 배선. → 표준 태그 분기로 대체됨.
- **#5 캡처카드(getUserMedia)** + GStreamer 커스텀 번들 스왑. (후속: 1.28.4 커스텀 GStreamer로 전면 전환 — 별도 사이클)
- **#6 성능/하니스**: `painter.rs`의 `media_wall_region_upload`(per-tile 영역 업로드), winit_wall, D3D11 백엔드, WR picture-caching 패치, video-pace 등. (후속: 현 렌더러 구조와 통합. winit_wall은 servoshell와 별개인 표출용 최소 임베더로 별도 구현)

## 2. 통합 방식

**깨끗한 재이식(clean re-port)** — git merge/cherry-pick이 아니라 현재 브랜치 위에 파일 단위로 새로 적용한다.

근거:
- 출처 체인은 6/15 base(`6694c14ea56`) 위에 있고 현재 브랜치는 그 뒤 DComp 재작성·stylo 벤더링 등으로 크게 갈라졌다. merge는 무관/충돌 이력을 대량 끌어온다.
- 사용자가 커스텀 엘리먼트를 제외하기로 했는데, 그 엘리먼트는 초기 이력에 얽혀 있어 cherry-pick 시 끌려오거나 충돌한다.
- 미디어 기능은 대부분 `script/dom` + `pixels` + `media/gstreamer` + `net` 계층이라 DComp가 재작성한 `components/paint/` 와 겹침이 낮다 → 파일 단위 재적용이 안전.

## 3. 아키텍처 (3계층, 전부 기본 off)

### 계층 1 — 디코드/소스 인프라 (엔진, DOM 변경 없음)
- `components/pixels/lib.rs` (+ `Cargo.toml` 의존성): `load_extended_from_memory` — 확장 이미지 디코드. JPEG XL은 jxl-oxide의 `Render::image_all_channels()` 사용(`image()` 아님).
- `components/media/backends/gstreamer/player.rs` (+ `lib.rs`): `rtsp://`용 NetworkUri 플레이어 경로 및 확장 컨테이너 autoplug.
- `components/media/backends/gstreamer/media_capture.rs`·`media_stream.rs`·`media_stream_source.rs`: getDisplayMedia 소스(`d3d11screencapturesrc`).
- `components/media/servo-media/lib.rs`, `components/media/streams/capture.rs`: 캡처 스트림 배선.
- 플러그인 목록(`ports/servo/gstreamer_plugin_lists/*`) + `python/servo/gstreamer.py`: `gstmpegts`(컨테이너), `gstcodecs`/`gstd3d11`(캡처), **+ RTSP 플러그인(§5 P0에서 가용성 검증 후 추가)**.

### 계층 2 — 분기 (DOM, 표준 태그)
- `components/net/image_cache.rs` + `components/script/dom/html/htmlimageelement.rs`: 표준 디코드 실패/타입 감지 시 `pixels::load_extended_from_memory`로 라우팅. 게이트 `dom_image_extended_formats_enabled`. 디코드 불가 데이터는 기존 broken-image/`error` 경로 유지.
- `components/script/dom/html/htmlmediaelement.rs`:
  - 확장 컨테이너 MIME(`is_extended_container_type`) → 일반 GStreamer autoplug. 게이트 `dom_video_extended_containers_enabled`.
  - `rtsp://`/`rtsps://` scheme(`is_direct_uri_scheme`) → NetworkUri 플레이어. 게이트 `dom_video_network_uri_enabled`.
  - **현재 브랜치의 기존 변경(+23줄)과 손으로 병합** — 자동 병합 아님.
- `components/script/dom/media/mediadevices.rs` + `MediaDevices.webidl` + `codegen/Bindings.conf`: `getDisplayMedia()`.

### 계층 3 — Prefs (`components/config/prefs.rs`)
- `dom_image_extended_formats_enabled` (기본 false)
- `dom_video_extended_containers_enabled` (기본 false)
- `dom_video_network_uri_enabled` (기본 false)
- getDisplayMedia 게이트 pref (screen-capture 브랜치의 pref명 그대로, 기본 false)

## 4. 검증 전략

프로젝트 관례: GUI 테스트 하니스 없음 → probe HTML 페이지 + stderr 로그.

- **probe 페이지 이식(표준 태그 계열만)**: `multigpu_standard_img_extended_probe.html`, `multigpu_standard_video_extended_probe.html`, RTSP `<video>` 변형 probe. `rtsp_testsrc.mp4` + 로컬 RTSP 소스 재사용. (커스텀 `x_image`/`x_media` probe는 이식하지 않음)
- **기능별 게이트**(엔진 관례): `cargo check -p servoshell` / `cargo build -p servoshell` / 관련 `cargo test` / `rustfmt --edition 2024 --check <touched>` / `git diff --check`.
- **런타임 스모크**: pref on → probe 페이지에서 디코드/재생을 stderr 로그로 확인. **pref off → 동작 무변화(월 회귀 가드)**.

## 5. 리스크 및 하위 기능 분해

각 하위 기능은 독립 검증 게이트를 가지며 아래 순서로 진행한다.

- **P0 — 리스크 체크(선행)**
  - RTSP 플러그인이 번들 1.22.8에 있는지 확인(현재 브랜치 플러그인 목록엔 rtsp/rtp/rtpmanager/udp/mpegts 없음 확인됨). 없으면 플러그인 목록 + `GSTREAMER_BASE_LIBS` 양쪽 갱신 필요.
  - `htmlmediaelement.rs` 현재 브랜치 변경과 dispatch 변경의 병합면 조사.
  - 테스트 자산 확보: 확장 포맷 샘플 파일(tiff/exr/qoi/jxl, mkv/avi/wmv) + 도달 가능한 RTSP 엔드포인트.
- **P1 — 확장 이미지(`<img>`)**: 가장 격리됨. pixels + net/image_cache + htmlimageelement + pref.
- **P2 — 확장 비디오 컨테이너(`<video>`)**: gstreamer player + htmlmediaelement + pref + gstmpegts.
- **P3 — RTSP(`<video src=rtsp://>`)**: NetworkUri 경로 + P0의 플러그인 배선 결과 반영.
- **P4 — getDisplayMedia(#4)**: media_capture/stream + mediadevices + webidl + Bindings.conf + gstcodecs/gstd3d11.

## 6. 알려진 함정(참조)

- 새 GStreamer element는 plugin_lists(`windows.rs.in`)와 `GSTREAMER_BASE_LIBS`(`gstreamer.py`) **둘 다** 갱신해야 등록됨. 한쪽만 하면 미등록.
- 빌드는 `mach` 필수(신규 plugin/DLL 배선 반영). `cargo build`만으로는 DLL 복사/플러그인 등록 누락 가능.
- pref off 무회귀는 이 브랜치의 검증된 DComp/비디오 월 파이프라인을 지키는 핵심 계약.

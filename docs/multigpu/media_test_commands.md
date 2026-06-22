# 미디어 표출 테스트 — 통합 cmd 레퍼런스

Servo 포크에서 브랜치별로 추가한 미디어 표출 기능과 그 테스트 명령을 한곳에 모은 문서.
각 기능의 설계/구현 상세는 해당 도메인 문서를 참조(맨 아래 "문서 맵").

> ⚠️ 이 문서는 **여러 브랜치에 걸친** 기능을 정리한다. 특정 프로브 페이지/pref/엘리먼트는
> 그 기능을 도입한 브랜치(또는 그 이후 브랜치)에서만 존재한다. "브랜치" 열을 확인할 것.

---

## 0. 공통 전제 (모든 실행에 해당)

```powershell
# PowerShell 세션마다 1회 — 빌드 환경 + GStreamer/DLL PATH 설정 (점 소싱 필수)
. .\scripts\servo_env.ps1
cd servo
```

- **빌드는 `mach` 필수**: `.\mach build`(debug) / `.\mach build --release`. `cargo build -p servoshell`은
  (1) `media-gstreamer` feature가 빠져 DummyBackend가 되고 (2) GStreamer DLL/플러그인을
  `target\…`로 복사하지 않아 미디어가 동작하지 않는다.
- **URL/페이지는 positional 인자**(맨 끝). `file://` 절대경로도 가능.
- **월 모드**: `--wall-layout <layout.json> --wall-all-tiles` (한 로직 페이지를 타일로 팬아웃).
  일반 창으로 보려면 `--wall-*`·`-f`를 **전부 빼기**.
- **`-f`는 `--headless`** (파일 플래그 아님). 화면 확인용 실행엔 절대 넣지 말 것.
- **거의 모든 기능은 pref 게이트가 기본 off** → 해당 `--pref ...=true` 필요(아래 표).
- **로그 확인**: GUI는 화면에 거의 안 나오므로 `2> run.err.log`로 stderr 리다이렉트.
  **반드시 창을 닫아 종료**해야 버퍼가 flush됨(force-kill 시 빈 로그).
- **wall layout 파일**: 저장소 `config\wall_layout.local_{1x1,2x1,3x1}.json`(개발기 레이아웃) 사용.
  in-tree 예시는 `servo\etc\multigpu\config\wall_layout.example_*.json`.

---

## 1. 빠른 인덱스 (브랜치 × 미디어 × pref)

| # | 브랜치 | 표출 미디어 | DOM 진입점 | 핵심 pref(기본 off) |
|---|---|---|---|---|
| 1 | `multigpu-tiled-wall` | 일반 웹/비디오/WebGL/WebGPU 월 팬아웃 | 표준 페이지 | (없음 — 월 플래그만) |
| 2 | `rtsp-custom-element` | RTSP 라이브 스트림 | `<rtsp-stream>` | `dom_rtsp_stream_enabled` |
| 3 | `webrtc-video-test` | WebRTC 수신 비디오 ⚠️미완 | `RTCPeerConnection` | `dom_webrtc_enabled` (+`dom_webrtc_transceiver_enabled`) |
| 4 | `nonstandard-media-formats` | 비표준 컨테이너 비디오 / 확장 이미지 | `<x-media>`, `<x-image>` | `dom_rtsp_stream_enabled`(x-media), `dom_x_image_enabled`(x-image) |
| 5 | `standard-tag-media-dispatch` | 표준 태그가 타입 감지·분기 | 표준 `<img>`/`<video>` | `dom_image_extended_formats_enabled`, `dom_video_extended_containers_enabled`, `dom_video_network_uri_enabled` |
| 6 | `screen-capture-getdisplaymedia` | 모니터/창(HWND) 화면 캡처 | `getDisplayMedia()` | `dom_webrtc_enabled` + `dom_screen_capture_enabled` |
| 7 | `capture-card-getusermedia` | 캡처카드 `videoinput` 입력 | `getUserMedia()` | `dom_webrtc_enabled` |

> pref 게이트가 `dom_webrtc_enabled`인 이유: `navigator.mediaDevices` 인터페이스 전체가 그
> pref 뒤에 있어, getDisplayMedia/getUserMedia도 이 펜을 같이 켜야 메서드에 도달한다.

---

## 2. 브랜치별 테스트 명령

### 1) multigpu-tiled-wall — 기반(월 인프라)
한 로직 페이지를 타일로 팬아웃. 비디오 월(4K 60fps), 스크롤/애니 동기, WebGL/WebGPU, 스트레스.
```powershell
target\debug\servoshell.exe --wall-layout ..\config\wall_layout.local_3x1.json --wall-all-tiles `
  tests\html\multigpu_wall_sync_probe.html 2> run.err.log
```
- 프로브: `multigpu_wall_sync_probe` / `multigpu_wall_stress_cases` / `multigpu_virtual_viewport_probe` /
  `multigpu_wall_gpu_load_probe` / `multigpu_wall_video_4k_*`.
- 단일 타일 미리보기: `--wall-tile-index <n>` (팬아웃 대신 한 타일만).
- 헬퍼 스크립트: `etc\multigpu\run_video_4k_wall.ps1`, `run_kakao_map_wall.ps1`,
  `run_three_retargeting_wall.ps1`.

### 2) rtsp-custom-element — RTSP (`<rtsp-stream>`)
`<rtsp-stream src="rtsp://…">` 라이브 비디오. 먼저 RTSP 서버 기동 필요.
```powershell
# (RTSP 서버가 rtsp://127.0.0.1:8554/test 로 게시 중이라고 가정)
target\debug\servoshell.exe --pref dom_rtsp_stream_enabled=true `
  tests\html\multigpu_wall_rtsp_probe.html 2> rtsp_run.err.log
```
- 월 팬아웃: 위에 `--wall-layout ..\config\wall_layout.local_1x1.json --wall-all-tiles` 추가.
- 프로브: `multigpu_wall_rtsp_probe`(라이브), `multigpu_wall_rtsp_probe_file`(파일 소스),
  `multigpu_wall_rtsp_boundary(_mixed)_probe`(타일 경계), `multigpu_wall_rtsp_mixed_content_probe`.
- 서버 사전 점검: `gst-launch-1.0 playbin3 uri=rtsp://127.0.0.1:8554/test video-sink=fakesink`.
- 문서: `multigpu_rtsp_playback.md`.

### 3) webrtc-video-test — WebRTC 수신 ⚠️ 미완(abort)
원격 비디오 트랙 수신. *협상·첫 프레임까지는 가나 `webrtc.rs` 'media' caps 패닉으로 abort —
gstsrtp 등 추가 필요. 완성 전 참고용.*
```powershell
target\release\servoshell.exe --wall-layout ..\config\wall_layout.local_2x1.json --wall-all-tiles `
  --pref dom_webrtc_enabled=true --pref dom_webrtc_transceiver_enabled=true `
  --pref dom_rtsp_stream_enabled=true `
  tests\html\multigpu_wall_rtsp_webrtc_mixed_probe.html
```
- 참고: `multigpu_wall_webrtc_video_probe.html`은 `getUserMedia` 전용이라 수신 테스트엔 부적합.
- 문서: `multigpu_webrtc_video_test.md`, `multigpu_webrtc_video_notes.md`.

### 4) nonstandard-media-formats — `<x-media>` / `<x-image>`
```powershell
# x-media: 비표준 컨테이너 비디오(mkv/avi/mov/wmv/ts/flv …)
target\release\servoshell.exe --wall-layout ..\config\wall_layout.local_1x1.json --wall-all-tiles `
  --pref dom_rtsp_stream_enabled=true `
  tests\html\multigpu_x_media_containers_probe.html

# x-image: 확장 이미지(tiff/exr/qoi … + JPEG XL)
target\release\servoshell.exe --pref dom_x_image_enabled=true `
  tests\html\multigpu_x_image_formats_probe.html
```
- 프로브: `multigpu_x_media_containers_probe`(3-up), `multigpu_x_image_formats_probe`(4-up).
- 문서: `multigpu_nonstandard_media_formats.md`.

### 5) standard-tag-media-dispatch — 표준 `<img>`/`<video>` 자동 분기
분리 엘리먼트(x-media/x-image/rtsp-stream) 대신 **표준 태그**가 타입을 감지해 확장 디코드/
gstreamer/rtsp로 분기.
```powershell
target\release\servoshell.exe --wall-layout ..\config\wall_layout.local_1x1.json --wall-all-tiles `
  --pref dom_image_extended_formats_enabled=true `
  --pref dom_video_extended_containers_enabled=true `
  --pref dom_video_network_uri_enabled=true `
  tests\html\multigpu_standard_video_extended_probe.html
```
- pref: `dom_image_extended_formats_enabled`(`<img>` 확장 디코드),
  `dom_video_extended_containers_enabled`(`<video>` 비표준 컨테이너),
  `dom_video_network_uri_enabled`(`<video src="rtsp://…">`).
- 프로브: `multigpu_standard_img_extended_probe`, `multigpu_standard_video_extended_probe`.
- 문서: `multigpu_standard_tag_media_dispatch.md`.

### 6) screen-capture-getdisplaymedia — 화면/창 캡처
```powershell
# 모니터 캡처 (monitor-index=-1 = 주 모니터)
target\debug\servoshell.exe --wall-layout ..\config\wall_layout.local_1x1.json --wall-all-tiles `
  --pref dom_webrtc_enabled=true --pref dom_screen_capture_enabled=true `
  --pref media_screen_capture_monitor_index=-1 `
  tests\html\multigpu_screen_capture_probe.html

# 특정 창 캡처 (제목 부분일치, 대소문자 무시) — monitor_index 대신
  --pref media_screen_capture_window_title="메모장"
```
- 부가 pref: `media_screen_capture_show_cursor=true|false`.
- ⚠️ servoshell 창이 있는 모니터를 캡처하면 **무한 미러** → 다른 모니터/창 지정.
- 표출 경로: raw passthrough(VP8 미경유). 문서: `screen_capture_getdisplaymedia_notes.md`.

### 7) capture-card-getusermedia — 캡처카드 입력
HDMI 캡처카드 등 `videoinput` 라이브 입력. **사내 커스텀 GStreamer 1.26.8 + 커스텀
ksvideosrc 필요**(기본 번들 1.22.8 ksvideosrc는 일부 카드 caps probe에서 abort).
```powershell
target\release\servoshell.exe --wall-layout ..\config\wall_layout.local_1x1.json --wall-all-tiles `
  --pref dom_webrtc_enabled=true `
  tests\html\multigpu_capture_card_probe.html
```
- 프로브 `multigpu_capture_card_probe`: HUD에 `enumerateDevices` 목록 + 라이브 상태 표시.
- ⚠️ 캡처카드 다중 입력이 동일 deviceId면 첫 입력만 잡힘(deviceId 선택은 미배선).
- GStreamer 교체/빌드 주의(슬롯#1 정션 등): `gstreamer_inhouse_1268.md`.

---

## 3. 자주 쓰는 보조 플래그 / 환경변수

| 항목 | 의미 |
|---|---|
| `--wall-tile-index <n>` | 팬아웃 대신 타일 n만 렌더(단일 타일 미리보기) |
| `media_capture_mocking_enabled` (pref) | 호스트 장치 대신 합성 스트림 반환(getUserMedia/getDisplayMedia 목) |
| `SERVO_MEDIA_VIDEO_SINK_POLICY=low-latency` | 비디오 sink를 드롭/qos 활성(기본 smooth) |
| `SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF=1` | servosrc EnoughData 백오프로 인한 주기적 끊김 완화 |
| `SERVO_WALL_FRAME_DELAY_*` (env) | Phase 5 배리어 실패 주입(렌더 지연 시뮬레이션) |

---

## 4. 문서 맵 (기능별 상세)

| 기능 | 상세 문서 |
|---|---|
| 공통 빌드/실행 규칙·함정 | 루트 `CLAUDE.md` (단일 권위 출처) |
| 월 기반/Phase 진행 | `docs/multigpu/multigpu_tiled_present_implementation_plan.md` |
| 공유 씬 팬아웃 설계 | `docs/multigpu/multigpu_shared_scene_fanout_design.md` |
| RTSP | `docs/multigpu/multigpu_rtsp_playback.md` |
| WebRTC 수신 | `docs/multigpu/multigpu_webrtc_video_test.md`, `…_webrtc_video_notes.md`, `…_webrtc_video_distribution_plan.md` |
| 비표준 컨테이너/이미지 | `docs/multigpu/multigpu_nonstandard_media_formats.md` |
| 표준 태그 분기 | `docs/multigpu/multigpu_standard_tag_media_dispatch.md` |
| 화면/창 캡처 | `docs/multigpu/screen_capture_getdisplaymedia_notes.md` |
| 캡처카드 / GStreamer 1.26.8 교체 | `docs/multigpu/gstreamer_inhouse_1268.md` |
| YUV zero-copy / WebGL·WebGPU 팬아웃 | `docs/multigpu/multigpu_yuv_zero_copy_video_plan.md`, `…_webgl_gpu_fanout_plan.md` |

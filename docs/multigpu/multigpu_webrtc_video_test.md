# WebRTC 원격 영상 수신 테스트 — 절차 / 빌드 / 설정 가이드

> 상태: 2026-06-12 기준 **동작 검증 완료**.
> gst-plugins-rs 기본 시그널링 서버로 publish된 프로듀서의 원격 영상을 Servo가
> `RTCPeerConnection`으로 협상·수신·디코드하고, `<video srcObject>` → WebRender로
> 표출(wall 모드, 타일 경계 걸침 포함)하는 것을 확인.
> 브랜치: `webrtc-video-test` (servo 저장소, `rtsp-custom-element`에서 분기).

## 1. 무엇을 검증하나

표준 `RTCPeerConnection` 시그널링으로 원격 WebRTC 영상을 받아, RTSP(`<rtsp-stream>`)·
표준 웹 콘텐츠와 **한 페이지에서 동시에** 렌더링한다. 미디어 표출 경로는 RTSP/`<video>`와
동일(MediaStream → GStreamer → YUV external image → WebRender), wall 타일로 fan-out된다.

## 2. 이 검증에 필요했던 변경 (이미 브랜치에 반영)

> Servo 엔진 핵심 로직은 거의 무변경이고, 아래 3가지가 필요했다.

| 항목 | 파일 | 내용 |
|---|---|---|
| 1. 미디어 플러그인 | `components/servo/gstreamer_plugin_lists/common.rs.in` | **`gstsrtp` 추가**. WebRTC 미디어는 DTLS-SRTP 필수인데 로드 목록에 빠져 있었음(`gstnice`/`gstdtls`/`gstwebrtc`/`gstrtp`/`gstrtpmanager`/`gstudp`는 있었음). DLL은 1.22.8 번들에 있어 `mach build`가 자동 복사. |
| 2. 수신부 패닉 수정 | `components/media/backends/gstreamer/webrtc.rs` (`on_incoming_stream`) | 수신 pad의 `query_caps(None)`에 `media` 필드가 없을 때 `.expect()`가 **non-unwinding 콜백에서 패닉 → 프로세스 abort**하던 버그. `current_caps()` 우선 + 누락 시 안전 폴백(`"video"`)으로 수정. |
| 3. pref | (런타임 플래그) | `--pref dom_webrtc_enabled=true` (+ `dom_webrtc_transceiver_enabled=true`). |

추가로 시그널링용 프로브 페이지를 작성: `tests/html/multigpu_wall_rtsp_webrtc_mixed_probe.html`
(기존 `multigpu_wall_webrtc_video_probe.html`은 `getUserMedia` 전용이라 부적합).

## 3. 빌드

`gstsrtp` 추가가 반영되도록 **`mach build`로 빌드**(media-gstreamer feature + 플러그인 자동 복사):
```powershell
. .\scripts\servo_env.ps1
cd servo
.\mach build --release -j 8       # 부드러운 framerate 위해 release 권장 (debug도 가능)
# 확인: target\release\gstsrtp.dll 가 복사돼 있어야 함
```
> `cargo build`는 DummyBackend + 플러그인 미복사라 미디어가 안 됨(반드시 `mach build`).

## 4. 시그널링 프로토콜 (gst-plugins-rs 기본)

WebSocket JSON, 기본 주소 **`ws://127.0.0.1:8443`**. **프로듀서가 offerer, 브라우저가 answerer.**
```
서버→: welcome{peerId}
클라→: setPeerStatus{roles:["Listener"]}, list
서버→: list{producers:[{id, meta}]}
클라→: startSession{peerId:<producerId>}
서버→: sessionStarted{peerId, sessionId}
서버→: peer{sessionId, sdp:{type:"offer", sdp}}      // 프로듀서의 offer
클라→: peer{sessionId, sdp:{type:"answer", sdp}}     // 브라우저의 answer
양방향: peer{sessionId, ice:{candidate, sdpMLineIndex}}
→ RTCPeerConnection.ontrack → MediaStream → <video srcObject>
```
> 참고: Servo의 `RTCTrackEvent.streams`가 비어있을 수 있어, 프로브는 `e.track`으로
> `MediaStream`을 구성한다.

## 5. 실행 절차 (터미널 3개)

```powershell
# (1) 시그널링 서버 (gst-plugins-rs). 기본 :8443
gst-webrtc-signalling-server

# (2) 프로듀서 — 영상을 publish (예: 테스트 패턴; 실제 소스로 교체 가능)
gst-launch-1.0 videotestsrc is-live=true ! videoconvert ! webrtcsink signaller::uri=ws://127.0.0.1:8443

# (3) servoshell (release, wall 2x1)
cd F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\servo
target\release\servoshell.exe --wall-layout ..\config\wall_layout.local_2x1.json --wall-all-tiles `
  --pref dom_webrtc_enabled=true --pref dom_webrtc_transceiver_enabled=true `
  --pref dom_rtsp_stream_enabled=true `
  tests\html\multigpu_wall_rtsp_webrtc_mixed_probe.html
```
- 프로브는 시그널링에서 producer를 자동 발견(없으면 등장 대기). `?producer=<id>` / `?signaling=ws://host:port`로 지정 가능.
- 프로브 레이아웃: 중앙 seam(x=1920)에 **RTSP(위)·WebRTC(아래) 영상이 둘 다 걸침**, 양옆에 표준 콘텐츠.
- RTSP까지 함께 보려면 별도 터미널에서 RTSP 서버도 띄움(`gst-validate-rtsp-server-1.0.exe file:///.../rtsp_testsrc.mp4`). WebRTC만 볼 거면 생략 가능.

## 6. PASS 신호 (stderr / `webrtc:` 상태줄)

- `RTSP_WEBRTC ws open` → `welcome id=...` → `startSession -> producer ...` → `session ...` →
  `sent answer; negotiating…` → **`ontrack (video) — attached`**
- `Wall media frame ... frame_backend=yuv_i420_external_raw size=...` 가 다수(원격 영상 디코드·업로드)
- **panic / abort 없음** (수정 전에는 `webrtc.rs:727` 패닉으로 `0xC0000409` abort)
- wall 타일 창에 영상이 경계를 가로질러 표출

`RUST_LOG=info`로 stderr를 파일에 캡처하면 위 신호와 프레임 카운트를 확인 가능
(콘솔로 리다이렉트 시 블로킹 주의 — `Start-Process -RedirectStandardError`나 파일 리다이렉트 사용).

## 7. 알려진 한계 / 후속 과제

- **좌/우 타일 WebRTC framerate 비대칭**: 경계 걸침 시 한쪽 타일의 WebRTC 영상이 더 저하되어 보임.
  webrtcbin 수신부 이하(지터버퍼/디코드/타일별 업로드·present) 최적화가 더 필요한 영역.
  (단일 GPU 다타일의 공유 present-clock 특성과도 연결됨 — `multigpu_rtsp_playback.md` 7절 참고.)
- **수신 경로 성숙도**: 이번에 `webrtc.rs` 패닉 1건을 고쳤으나, 이 경로는 덜 검증된 영역이라
  다른 코덱/협상 조건에서 추가 이슈 가능. 현재 검증은 H.264(NVENC 프로듀서) 수신 기준.
- cross-GPU(서로 다른 물리 GPU) 분산은 v1 non-goal이며 이 머신(전 모니터 GPU 0)에선 미검증.
- 프로브의 `[RTSP-DIAG]`/`RTSP_WEBRTC` 진단 로그는 유지 중(머지 전 정리 대상).

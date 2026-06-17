# 화면/장치 캡처 표출 element 조사: getDisplayMedia / getUserMedia (2026-06-16)

표준 웹 브라우저에서 화면(창/데스크톱) 캡처와 캡처 카드 영상 입력이 어떻게
구현되어 있는지 조사한 노트. 향후 Servo 멀티-GPU 월에 캡처 소스를 붙일 때의
참고 자료.

조사 방식: 공개 문서(Chromium media capture README, MDN Screen Capture API,
Microsoft Media Foundation, OBS WGC/DXGI 비교 등) + 일반 지식. 코드 레벨
Servo 실태 확인은 아직 안 함(별도 후속 작업).

---

## 0. 핵심 전제 — 두 요구는 서로 다른 API

| 요구 | 표준 API | 정체 |
| --- | --- | --- |
| OS 창 / 데스크톱 전체 캡처 | `getDisplayMedia()` (Screen Capture API) | 화면 합성 결과를 읽음 |
| 캡처 카드 영상 입력 | `getUserMedia({video})` (Media Capture API) | 카메라와 동일한 "영상 입력 장치" |

캡처 카드(HDMI→USB 등)는 화면 캡처가 **아니다.** OS가 UVC 웹캠처럼
`videoinput` 장치로 인식하므로 `enumerateDevices()`에 `videoinput`으로 나오고
`getUserMedia()`로 받는다. `getDisplayMedia()`로는 접근 불가. 두 경로는 브라우저
내부 구현이 완전히 다르다.

---

## 1. getDisplayMedia() — 창/데스크톱 캡처 구현

### 1-1. 스펙·보안 레이어
- `navigator.mediaDevices.getDisplayMedia(constraints)` → `MediaStream` 반환.
  Blink: `third_party/blink/renderer/modules/mediastream/`.
- 반드시 user gesture(클릭 등)에 대한 응답으로만 호출 가능. 그리고 브라우저가
  강제로 picker UI(화면/창/탭 선택창)를 띄운다. 웹페이지는 "무엇을 캡처할지"
  직접 지정할 수 없다 — 핵심 보안 모델(스파이 캡처 방지).
- `getUserMedia`와 달리 영구 권한 부여가 없고, 매 호출마다 picker가 뜬다.

### 1-2. 프로세스 구조 (Chromium 기준)
렌더러 프로세스는 OS 캡처 권한이 없다. 흐름:

```
Renderer (Blink, getDisplayMedia)
   │  mojo
   ▼
Browser process — MediaStreamManager
   (content/browser/renderer_host/media/media_stream_manager.cc)
   │  ← picker UI 표시, 사용자 선택 수집
   ▼
캡처 백엔드 (선택 대상에 따라 분기)
```

선택 대상에 따라 백엔드가 셋으로 갈린다:

1. **탭 캡처** → 브라우저 내부 합성기 **Viz**의 frame sink에서 직접 읽음
   (`components/viz/service/frame_sinks/video_capture`). OS 캡처 API를 안 거치고
   전 플랫폼 공통. 가장 효율적·고품질.
2. **창(window) 캡처** / 3. **화면(screen) 캡처** → WebRTC 데스크톱 캡처 모듈
   (`third_party/webrtc/modules/desktop_capture`, `DesktopCapturer` 인터페이스).
   여기서 OS API에 연결.

### 1-3. OS별 실제 캡처 백엔드 (창/데스크톱)

**Windows**
- **Windows.Graphics.Capture (WGC)** — Win10 1903+ 기본. 캡처 대상에 노란 테두리.
  **cross-GPU로 동작하고, 대상이 어느 GPU에 있든 사용자 개입 없이 캡처된다.**
  (멀티-GPU 월 관심사와 직접 연결되는 포인트)
- **DXGI Desktop Duplication API** — 폴백. 저지연·고성능 전체화면 캡처지만
  애플리케이션이 디스플레이와 **같은 GPU에서 실행돼야 한다.** 창 단위 캡처에는
  부적합(화면 전체를 떠서 잘라냄).
- **GDI BitBlt** — 레거시 창 캡처 폴백.

**macOS**
- 전통적으로 **CoreGraphics**(`CGDisplayStream`, `CGWindowListCreateImage`).
  Chromium은 2026년 초 기준으로도 macOS에서 여전히 레거시 CoreGraphics 경로 사용,
  **ScreenCaptureKit**(macOS 12.3+, Apple 권장)으로의 마이그레이션이 트래커에서
  논의 중. 관련 코드: `content/browser/media/capture/desktop_capture_device_mac.cc`.

**Linux**
- **X11**: 클라이언트가 다른 윈도우 표면을 직접 읽을 수 있음 →
  XComposite/XShm/XDamage로 직접 캡처.
- **Wayland**: 보안상 클라이언트가 다른 surface를 직접 읽을 수 없음. 화면 공유는
  오직 **`xdg-desktop-portal`(`org.freedesktop.portal.ScreenCast` D-Bus 인터페이스)
  + PipeWire 스트림**을 통해서만 가능. 선택 UI/권한 다이얼로그는 브라우저가 아니라
  컴포지터/포털이 띄우고, PipeWire가 단일 경로로 화면·창 프레임을 전달.

---

## 2. 캡처 카드 영상 입력 구현 (getUserMedia)

### 2-1. 장치로서의 인식
- 캡처 카드가 **UVC(USB Video Class) 표준**을 따르면 OS가 별도 드라이버 없이 일반
  카메라로 인식 → 브라우저 `enumerateDevices()`에 `kind: "videoinput"`으로 노출.
  `getUserMedia({video: {deviceId}})`로 선택.
- UVC 비표준 카드는 벤더 드라이버가 OS 미디어 프레임워크에 장치를 등록해야만
  브라우저에 보인다.

### 2-2. 프로세스·모듈 구조 (Chromium)
화면 캡처와 별개 경로:
- `media/capture/video` + 별도 **`services/video_capture` 프로세스** (capture
  service). 렌더러와 mojo + shared memory로 프레임 전달.
- `VideoCaptureDevice` 추상화 아래 OS별 구현이 꽂힌다.

### 2-3. OS별 백엔드 (영상 입력 장치)
- **Windows**: **Media Foundation**(`IMFSourceReader`) — UVC 1.1+ 클래스 드라이버
  기반. 레거시는 DirectShow.
- **macOS**: **AVFoundation**(`AVCaptureSession`) — 내장 카메라, USB/FireWire 포함.
- **Linux**: **V4L2**(`/dev/videoN`).

### 2-4. 캡처 카드 특유의 실무 함정
- 포맷 협상: 카드가 MJPEG/YUY2/NV12 등을 노출, 해상도·프레임레이트가 장치
  capability에 묶임 (`getCapabilities()`/constraints로 협상).
- 일부 카드는 **배타적 접근(exclusive)** — 다른 앱이 점유 중이면 못 잡음.
- **HDCP 보호 입력**(게임콘솔/블루레이 HDMI 등)은 캡처 카드 단에서 차단되어 검은
  화면이 될 수 있음(브라우저 문제 아님).

---

## 3. 두 경로 비교 요약

| | getDisplayMedia() (화면/창) | getUserMedia() (캡처카드/카메라) |
| --- | --- | --- |
| 권한 | 매번 picker 강제, gesture 필수, 영구권한 없음 | 도메인별 영구 권한 부여 가능 |
| 선택 | 브라우저/OS picker가 대상 선택 | 페이지가 deviceId로 직접 지정 |
| Chromium 백엔드 | 탭=Viz frame sink / 창·화면=WebRTC `desktop_capture` | `services/video_capture` + `VideoCaptureDevice` |
| OS API | WGC/DXGI(Win), CoreGraphics·ScreenCaptureKit(mac), PipeWire+portal(Wayland) | Media Foundation(Win), AVFoundation(mac), V4L2(Linux) |
| 출력 | `MediaStreamTrack` (kind=video, displaySurface 라벨) | `MediaStreamTrack` (kind=video, deviceId) |

---

## 4. Servo 멀티-GPU 월 관점 메모

- Windows에서 WGC가 cross-GPU 무개입인 반면 DXGI Desktop Duplication은 "같은
  GPU여야 함" 제약이 있다는 점은, WebGPU GPU-direct(LUID별 공유 텍스처)·ANGLE
  LUID 분산 이슈와 동일한 **cross-GPU 표면 공유 문제의 다른 얼굴**이다.
- Servo에 `getDisplayMedia`를 붙인다면 백엔드는 WebRTC `desktop_capture` 모듈
  (Servo는 이미 WebRTC 스택 일부 사용)을 재사용하는 게 자연스러운 출발점.
---

## 5. Servo 코드 실태 (2026-06-16 확인)

기준 트리: 이 워크트리(`servo/`), gstreamer 백엔드. servo-media는 외부 git
의존성이 아니라 `components/media/`에 path 의존성으로 vendored.

### 5-1. 요약

| 기능 | Servo 지원 상태 |
| --- | --- |
| `getDisplayMedia()` (화면/창) | **전무 — WebIDL·메서드·백엔드 어디에도 없음** |
| `getUserMedia()` (장치/캡처카드) | 구현됐으나 최소한, 기본 비활성 |
| `enumerateDevices()` | 구현됨(장치 그룹화 없음) |

### 5-2. getDisplayMedia() — 미구현
- 코드 전체에서 `getDisplayMedia` 문자열은 이 문서에만 존재. Servo 소스에는 없음.
- `components/script_bindings/webidls/MediaDevices.webidl`(8-22행):
  `enumerateDevices()`와 `getUserMedia()`만 정의. `getDisplayMedia` 선언 자체가 없음.
- `components/script/dom/media/mediadevices.rs`: `MediaDevicesMethods`에
  `GetUserMedia`/`EnumerateDevices`만 구현.
- 화면 캡처 소스 클래스(screencast)를 다루는 백엔드 경로 없음.
- → 화면/창/데스크톱 캡처 현재 불가능. 붙이려면 WebIDL + 메서드 + 캡처 소스
  (GStreamer `d3d11screencapturesrc`/`gdiscreencapsrc`, 또는 WebRTC
  `desktop_capture`)를 신규 배선해야 함.

### 5-3. getUserMedia() — 구현됐지만 제약 많음
- **기본 비활성**: `MediaDevices.webidl:8`에서 인터페이스 전체가
  `Pref="dom_webrtc_enabled"` 게이트, `components/config/prefs.rs:469` 기본값
  `false`. 기본 빌드에서 `navigator.mediaDevices` 자체가 `undefined`.
- **동작**(`mediadevices.rs:53-77`): 제약 변환 → `create_videoinput_stream` /
  `create_audioinput_stream` 호출 → 즉시 resolve. **권한 프롬프트·picker UI·
  user-gesture 요구가 전혀 없음**(표준 보안 모델 미구현).
- **캡처 백엔드**(`components/media/backends/gstreamer/media_capture.rs:120-143`):
  GStreamer `DeviceMonitor`에 `Video/Source` 필터 + 제약 caps를 걸고
  **`devices.front()`(첫 번째 매칭 장치)** 만 선택. Windows에서는 GStreamer가
  `mfvideosrc`/`ksvideosrc`로 비디오 소스를 열거하므로 **UVC 캡처 카드가 비디오
  소스로 잡히면 getUserMedia로 받을 수 있음.** 단 OS API(Media Foundation)를
  Servo가 직접 부르지 않고 **GStreamer를 경유.**
- **캡처 카드 선택 불가**: `MediaTrackConstraintSet`(webidl:69-86)에서 `deviceId`가
  주석 처리. 지원 제약은 `width/height/aspectRatio/frameRate/sampleRate`뿐.
  특정 카드 지정 불가 → 항상 첫 번째 열거 장치만 잡힘.
- **모킹**: `media_capture_mocking_enabled`(prefs.rs:544, 기본 `false`). 켜면
  호스트 장치 대신 합성 스트림 반환(gstreamer `lib.rs:342-348`).

### 5-4. enumerateDevices()
- 구현됨(`mediadevices.rs:80-116`). `GStreamerDeviceMonitor`에서 목록을 받아
  `MediaDeviceInfo` 반환. 단 **groupId 항상 빈 문자열**(백엔드가 장치 그룹화
  미지원), 병렬 실행 스텝 미구현.

### 5-5. 월 표출 경로 함의
- getUserMedia 비디오 트랙은 `GStreamerMediaStream`을 만들고 이는
  `<video>`/MediaStream 소비 경로를 탐 → **캡처가 동작하면 기존 비디오 팬아웃
  (external image) 경로를 그대로 재사용** 가능성이 높음(별도 타일 표출 로직 불필요).
- 단 현재로선 (1) `dom_webrtc_enabled`를 켜야 하고, (2) 화면 캡처는 아예 없으며,
  (3) 캡처 카드는 deviceId 선택이 안 됨.

### 5-6. 후속 작업 후보
1. **캡처 카드 경로 보강**: `deviceId` 제약을 WebIDL/`media_capture.rs`에 추가해
   특정 카드 선택 가능하게(현재 `devices.front()` 고정). `dom_webrtc_enabled`
   기본 활성 여부 검토.
2. **getDisplayMedia 신설**: WebIDL + 메서드 + 화면 캡처 소스 배선. Windows는
   GStreamer screencapture src 또는 WebRTC `desktop_capture`(WGC/DXGI) 중 택일.
3. 캡처 프레임이 layout에서 `<video>` external image로 들어가 기존 비디오 팬아웃을
   재사용하는지 런타임 확인.

---

## 6. getDisplayMedia 신설 타당성 결론 (2026-06-17)

**결론: 타당성 높음 (중간 난이도).** 가장 어려운 두 조각이 이미 동작한다:

1. **MediaStream → `<video>` 렌더링 (기구현)**: `srcObject`(htmlmediaelement.rs:3571)
   → `player.set_stream()`(2084) → external image 렌더(250-290). 캡처 트랙은 일반
   `<video>`처럼 표출됨.
2. **월 타일 팬아웃 (기구현)**: `<video>` 미디어 프레임은
   `PaintMessage::UpdateImages`(paint.rs:1768-1831) →
   `target_painter_ids_for_source_painter`(1074) → 전 타일 painter에
   `update_images(updates.clone())` 브로드캐스트. 즉 캡처 `<video>`는 **렌더/팬아웃
   코드 변경 0**으로 모든 타일에 표출. 프로젝트가 이미 "4K 비디오 월 60fps"를 검증.

따라서 실제 신규 작업은 (a) DOM/바인딩 미러링, (b) GStreamer 화면 캡처 element를
MediaStream으로 감싸는 백엔드뿐. 번들 GStreamer 1.22.8에 필요한 element가 모두 존재함을
`gst-inspect`로 확인: `d3d11screencapturesrc`(WGC/DXGI, `adapter` 인덱스로 GPU 선택 →
멀티-GPU LUID 모델과 직결, `monitor-index`/`window-handle`/`crop` 지원),
`gdiscreencapsrc`, `wasapi2src`(시스템 오디오 loopback).

핵심 비용은 표준 picker UX(소스 선택 UI)에 몰려 있으나, **월/키오스크 용도에선 고정
소스(설정 기반 monitor-index)로 우회 가능** — 이 경우 며칠 규모의 in-tree 작업.

## 7. 구현 (브랜치 `screen-capture-getdisplaymedia`, MVP)

베이스: `standard-tag-media-dispatch`(멀티-GPU 월 + 비디오 팬아웃 포함).
범위(확정): **MVP 고정 소스(picker 없음), 모니터+창(HWND) 캡처, 권한/제스처 생략.**

변경 지점:
- **pref**(`components/config/prefs.rs`): `dom_screen_capture_enabled`(게이트),
  `media_screen_capture_monitor_index`(-1=주), `media_screen_capture_window_title`
  (비어있지 않으면 창 캡처 우선), `media_screen_capture_show_cursor`.
- **WebIDL**(`MediaDevices.webidl`): `[Pref="dom_screen_capture_enabled"]
  getDisplayMedia()` + `DisplayMediaStreamConstraints` dict.
  `Bindings.conf`의 `MediaDevices`에 `GetDisplayMedia`를 realm/inRealms에 추가.
- **DOM**(`mediadevices.rs`): `GetUserMedia` 미러한 `GetDisplayMedia` — pref로
  `DisplayCaptureSource` 구성 → `create_display_stream` → Video 트랙.
- **servo-media**: `DisplayCaptureSource`(streams/capture.rs) + `Backend` 트레잇에
  기본구현(`None`) 메서드 `create_display_stream`(servo-media/lib.rs).
- **GStreamer 백엔드**: `media_capture::create_display_stream` —
  `d3d11screencapturesrc` 생성 + monitor-index/window-handle 설정 →
  `GStreamerMediaStream::create_video_from`. 창 제목→HWND는 `windows-sys`
  (`EnumWindows`/`GetWindowTextW`), Cargo.toml에 Windows 타깃 의존 추가.
  - **모니터 캡처**: 기본 `capture-api=dxgi`(Desktop Duplication) + `monitor-index`.
  - **창 캡처**: DXGI는 개별 창 캡처 불가 → `window-handle`은 `capture-api=wgc`
    (Windows Graphics Capture)에서만 유효하고 그 속성은 `conditionally available`이라
    **wgc를 먼저 설정**해야 함(안 그러면 `set_property("window-handle")` 패닉). WGC는
    캡처된 창에 노란 테두리를 그림(정상). 창 매칭은 `find_window_by_title`의
    대소문자 무시 **부분일치**(보이는 top-level 창 중 첫 매칭).

**사용법 (창 지정 캡처)**: `--pref media_screen_capture_window_title=<제목 일부>`를
추가하면 monitor-index보다 우선. 예:
`--pref dom_webrtc_enabled=true --pref dom_screen_capture_enabled=true
--pref media_screen_capture_window_title=메모장 <probe.html>`. 매칭 창이 없으면
경고 후 트랙 0개. 제목은 getDisplayMedia 호출(페이지 로드) 시점에 읽음.
- **GStreamer 플러그인 등록 (필수 — 처음 누락했던 지점)**: servoshell은 플러그인
  폴더를 스캔하지 않고 `components/servo/servo.rs:113`의 `init_with_plugins`가
  명시적 목록(`gstreamer_plugin_lists/*.rs.in`, `gstreamer_plugins.rs`에서
  `include!`)에 적힌 DLL만 exe 폴더에서 `load_file` 한다. 따라서 새 element를
  쓰면 **반드시** 두 곳을 같이 갱신해야 함:
  1. `components/servo/gstreamer_plugin_lists/windows.rs.in` ← `"gstd3d11"`
     (d3d11은 Windows 전용이라 common이 아닌 windows 목록). 런타임 등록 +
     빌드 시 `gstd3d11.dll` 복사를 둘 다 담당.
  2. `python/servo/gstreamer.py`의 `GSTREAMER_BASE_LIBS` ← `"gstd3d11"`, `"gstcodecs"`
     (`gstd3d11.dll`이 `gstd3d11-1.0-0.dll` + `gstcodecs-1.0-0.dll`에 의존 →
     llvm-objdump로 확인. `nonstandard-media-formats`의 `gstmpegts`→
     `gstmpegts-1.0-0.dll` 사례와 동일 — 하나라도 빠지면 `load_file`가 실패해
     `ErrorLoadingPlugins(["gstd3d11.dll"])`로 servoshell이 init 단계에서 abort).
  누락 시 `ElementFactory::make("d3d11screencapturesrc")`가 "Failed to find element
  factory" 로 실패 → `create_display_stream`이 `None` → 트랙 0개 (`videoSize 0x0`,
  STALLED). gst-inspect로 번들 *설치* 디렉터리에 element가 보여도 런타임 위치/목록과는
  별개임에 주의.
- **변경 불필요**: 렌더링·월 팬아웃·더미/ohos 백엔드(트레잇 기본구현).
- **테스트 페이지**: `tests/html/multigpu_screen_capture_probe.html`.

빌드: 위 플러그인 변경으로 새 DLL을 런타임 폴더로 복사해야 하므로 **`mach build`
필수** (`cargo build -p servoshell`은 `include!` 목록 재컴파일은 하지만 DLL 복사
package 단계가 없어 부적절).

주의: servoshell 창이 있는 모니터를 monitor-index로 캡처하면 무한 미러 → 다른
모니터/창으로 테스트. 제약(width/height/fps)은 v1 미적용. picker/제스처/권한/
`track.stop`은 후속 Phase.

---

## 출처

- Chromium Docs — Media Capture (README):
  https://chromium.googlesource.com/chromium/src/+/lkgr/docs/media/capture/README.md
- MDN — Screen Capture API:
  https://developer.mozilla.org/en-US/docs/Web/API/Screen_Capture_API
- MDN — Using the Screen Capture API:
  https://developer.mozilla.org/en-US/docs/Web/API/Screen_Capture_API/Using_Screen_Capture
- Windows Graphics Capture vs DXGI Desktop Duplication (OBS Forums):
  https://obsproject.com/forum/threads/windows-graphics-capture-vs-dxgi-desktop-duplication.149320/
- Desktop Duplication API / DXGI (Wikipedia):
  https://en.wikipedia.org/wiki/DirectX_Graphics_Infrastructure
- Audio/Video Capture in Media Foundation (Microsoft Learn):
  https://learn.microsoft.com/en-us/windows/win32/medfound/audio-video-capture-in-media-foundation
- MediaDevices.getUserMedia() for camera (api.video):
  https://api.video/blog/tutorials/grabbing-camera-streams-with-mediadevices-getusermedia/
- Standards Compliant Screen Capture in Chrome 72 (addpipe):
  https://blog.addpipe.com/standards-compliant-screen-capture-in-chrome-72/

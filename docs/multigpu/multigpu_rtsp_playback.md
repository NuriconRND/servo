# RTSP 라이브 스트림 재생 (`<rtsp-stream>`) — 빌드 / 테스트 / 설정 가이드

> 상태: 2026-06-12 기준 단일 창 수직 슬라이스 **동작 검증 완료**.
> rtspsrc 연결 → `MetadataUpdated`(640×360, H.264 High, `is_live=true`, `duration=None`)
> → `Playing` → 약 200 프레임/7.5초 표출, panic 0, 정상 종료(exit 0).
> 브랜치: `rtsp-custom-element` (servo 저장소, `multigpu-tiled-wall`에서 분기).

## 1. 무엇인가

웹 비표준 라이브 영상(RTSP)을 재생하는 **별도 커스텀 빌트인 엘리먼트 `<rtsp-stream>`**.
표준 `<video>`/`HTMLMediaElement` 경로는 전혀 건드리지 않는다(추가 분기/다운캐스트만).
라이브 스트림은 `duration=Inf`/seek 불가 같은 타임라인 의미론이 표준 JS를 깨므로,
별도 엘리먼트로 분리해 그 오염을 원천 차단하는 것이 설계 의도다.

```html
<rtsp-stream src="rtsp://127.0.0.1:8554/test" width="640" height="360"></rtsp-stream>
<script>
  document.querySelector('rtsp-stream').play();   // src의 rtsp:// 스트림 재생
</script>
```
API(최소): `src`(USVString), `width`/`height`, `videoWidth`/`videoHeight`(readonly),
`playing`(readonly), `play()`, `stop()`. **pref `dom_rtsp_stream_enabled`로 게이팅**(기본 off).

## 2. 빌드 방법 (중요)

### 반드시 `mach build`를 쓸 것 — `cargo build`는 미디어가 안 돈다

```powershell
. .\scripts\servo_env.ps1     # 항상 먼저 dot-source
cd servo
.\mach build -j 8             # debug 빌드
```

`mach build`는 Windows에서 자동으로 다음을 한다:
1. `--media-stack`가 `auto`→`gstreamer`로 해석되어 **`media-gstreamer` feature를 켠다**
   (`command_base.py`). 이게 없으면 servo는 **DummyBackend**(미디어 no-op)를 쓴다.
2. 빌드 후 `package_gstreamer_dlls`가 GStreamer 코어 DLL + **플러그인 목록의 플러그인 DLL**을
   `target/debug`로 복사한다. 플러그인 목록은 `components/servo/gstreamer_plugin_lists/*.rs.in`에서
   읽으며, 여기에 `gstrtsp`/`gstudp`가 포함돼 있으므로 **rtsp 플러그인이 자동 복사**된다.
   복사 출처는 servo 자기 번들(`target/dependencies/gstreamer/1.0/msvc_X86_64`, 1.22.8)이라
   **코어와 버전이 일치**한다.

### ⚠️ `cargo build -p servoshell`를 쓰면 안 되는 이유
- `media-gstreamer` feature가 servoshell 기본 feature에 없어서 **DummyBackend**가 된다
  (play()가 조용히 no-op → 이벤트/프레임 0, 에러도 0).
- DLL 복사 단계(packaging)가 실행되지 않아 rtsp 플러그인이 `target/debug`에 안 생긴다.
- 굳이 cargo로 빠르게 반복하려면 수동 조치가 필요하다(아래 6절).

## 3. 손봐야 하는 설정/소스 파일 (이미 브랜치에 반영됨)

| 파일 | 변경 내용 | 목적 |
|---|---|---|
| `components/servo/gstreamer_plugin_lists/common.rs.in` | `gstrtsp`, `gstudp` 추가 | **rtsp 플러그인을 로드 목록 + 자동 복사 대상에 포함** (핵심 1줄 설정) |
| `components/config/prefs.rs` | `dom_rtsp_stream_enabled: bool`(기본 false) 필드 + 기본값 | 엘리먼트 게이팅 |
| `components/script_bindings/webidls/RtspStreamElement.webidl` | 신규 인터페이스(`[Pref="dom_rtsp_stream_enabled"]`) | DOM API 정의 |
| `components/script/dom/html/rtspstreamelement.rs` | 신규 `#[dom_struct]` 엘리먼트 | 엘리먼트 본체 + 플레이어 구동 |
| `components/script/dom/create.rs` | `rtsp-stream` 런타임 LocalName guard arm | 태그→엘리먼트 등록 |
| `components/script/dom/node/node.rs` | `media_data()`에 다운캐스트 1줄 | 레이아웃의 video replaced 경로 재사용 |
| `components/script/dom/element/element.rs` | width/height presentational hints에 추가 | 박스 크기 |
| `components/script/dom/virtualmethods.rs` | `vtable_for`에 arm 추가 | 속성 파싱(parse_plain_attribute) 동작 |
| `components/script/dom/html/mod.rs`, `Bindings.conf` | 모듈/바인딩 등록 | 빌드 |
| `components/media/player/lib.rs` | `StreamType::NetworkUri` variant | 네트워크 직접 URI 모드 |
| `components/media/servo-media/lib.rs`, `backends/gstreamer/{lib,player}.rs`, `backends/{dummy,ohos}/lib.rs` | `network_uri` 파라미터 + playbin3에 rtsp URI 직접 지정, source-setup early-return + is_ready 즉시, rtspsrc 사전점검 | rtspsrc pull 경로 |

> 참고: `[RTSP-DIAG]` eprintln 진단 로그가 `player.rs`/`rtspstreamelement.rs`에 남아 있다(요청에 따라 유지).
> 머지 전 정리 대상.

## 4. 테스트용 RTSP 서버 (설치된 도구만, 다운로드 불필요)

시스템 GStreamer(1.26.8)에 포함된 RTSP 서버 바이너리를 쓴다.

```powershell
# (1) 테스트 영상 파일 생성 (ffmpeg, 1회)
ffmpeg -y -f lavfi -i "testsrc=size=640x360:rate=25:d=30" `
  -c:v libx264 -pix_fmt yuv420p -g 25 servo\rtsp_testsrc.mp4

# (2) RTSP 서버 기동 → rtsp://127.0.0.1:8554/test 로 게시 ("stream ready at ..." 출력)
& "C:\Program Files\gstreamer\1.0\msvc_x86_64\bin\gst-validate-rtsp-server-1.0.exe" `
  "file:///F:/20260609_SDWall_BrowserTest/20260606_multigpu_browser/servo/rtsp_testsrc.mp4"
```

> 서버가 잘 도는지는 시스템 `gst-launch-1.0 playbin3 uri=rtsp://127.0.0.1:8554/test video-sink=fakesink`로 먼저 확인 가능.

## 5. 실행 / 검증 명령

```powershell
. .\scripts\servo_env.ps1
cd servo
target\debug\servoshell.exe --pref dom_rtsp_stream_enabled=true `
  tests\html\multigpu_wall_rtsp_probe.html 2> rtsp_run.err.log
```

프로브 페이지(`tests/html/multigpu_wall_rtsp_probe.html`)는 `play()` 호출 후 상태를 폴링/로그한다.
**PASS 신호**(stderr에서 확인):
- `MetadataUpdated(... width: 640, height: 360 ... is_live: true)`
- `state-changed: Playing`
- `VideoFrameUpdated` 다수(수 초간 ~25fps 단조 증가)
- panic / `Missing dependency: rtspsrc` 없음

빠른 집계 예:
```powershell
$log = "rtsp_run.err.log"
"frames=$((Select-String $log -Pattern 'VideoFrameUpdated').Count) " +
"meta=$((Select-String $log -Pattern 'MetadataUpdated.*width: 640').Count) " +
"playing=$((Select-String $log -Pattern 'state-changed: Playing').Count)"
```

## 6. (선택) `cargo build`로 빠른 반복을 하려면

`mach build` 없이 `cargo build`로 반복할 경우 두 가지를 수동 보강해야 한다:

```powershell
# (a) feature 켜서 빌드
cargo build -p servoshell --features media-gstreamer

# (b) 버전 일치 플러그인 수동 복사 (mach build이 해주는 일)
$bundle = "target\dependencies\gstreamer\1.0\msvc_X86_64\lib\gstreamer-1.0"
Copy-Item "$bundle\gstrtsp.dll" target\debug\ -Force
Copy-Item "$bundle\gstudp.dll"  target\debug\ -Force
```

> 주의: **시스템(`C:\Program Files\gstreamer`, 1.26.8) 플러그인을 복사하면 안 된다.**
> servoshell 코어는 1.22.8이라 1.26.8 플러그인은 "procedure could not be found"로 로드 실패하고,
> `init_with_plugins`가 치명적 에러로 간주해 servoshell이 `exit(1)`한다.
> 반드시 `target/dependencies` 번들(1.22.8)에서 복사할 것.
>
> 또 하나의 함정: 이 머신(F: 드라이브)에서 cargo가 의존 rlib만 재컴파일하고 `servoshell.exe`를
> 재링크하지 않는 경우가 있다. 미디어 크레이트 수정이 반영 안 되면
> `Remove-Item target\debug\servoshell.exe` 후 다시 빌드.

## 7. 알려진 한계 / 다음 단계

- 현재 범위: **단일 창** 재생 검증. wall 타일 fan-out(멀티 GPU 분산)은 보류.
- 오디오/완성형 API(에러 재시도, 상태 이벤트 노출 등) 미구현.
- `[RTSP-DIAG]` 진단 로그 정리 필요(머지 전).
- 표준 `<video>` 경로는 무손상(추가 분기만) — 회귀 없음.

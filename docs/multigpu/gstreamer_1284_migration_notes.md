# GStreamer 1.28.4.100 마이그레이션 노트

- 날짜: 2026-07-22
- 브랜치: `nonstandard-media-display-port`
- 설계: `docs/superpowers/specs/2026-07-22-gstreamer-1.28.4-migration-design.md`
- 계획: `docs/superpowers/plans/2026-07-22-gstreamer-1.28.4-migration.md`
- 커밋: `ed38ad7e10c`(gstreamer.py DLL 이름), `fd318f9dc33`(카메라 소스 플러그인)

## 환경 (머신 상태 — 비커밋)

| 항목 | 값 |
|---|---|
| 로컬 설치 | `F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64` (공유 `\\192.168.1.214\share\NuriconCommon\gstreamer\1.28.4.100\msvc_x86_64`에서 robocopy, `.pdb` 제외 = 716 dirs/7192 files/3.88GB) |
| env var `GSTREAMER_1_0_ROOT_MSVC_X86_64` (Machine) | **이전**: `C:\Program Files\gstreamer\1.0\msvc_x86_64\` (1.26.8.101) → **현재**: `F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64` |
| 슬롯#1 정션 | `<worktree>\target\dependencies\gstreamer\1.0\msvc_x86_64` → `F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64` (mklink /J) |
| 1.22.8 백업 | `<worktree>\target\dependencies\gstreamer\1.0\msvc_x86_64.bak-1228` |
| 시스템 1.26.8 | 미영향(보존) |

전환 전 상태는 **컴파일(시스템 1.26.8) / 런타임(슬롯#1 1.22.8) 스큐**였고, 이번 전환으로 컴파일·DLL복사·런타임 전부 1.28.4.100로 통일.

## (a) 슬롯#1 핫스왑 side effect & 해소

슬롯#1 내용만 1.28.4.100로 바꿨을 때 발생하는 부작용과 해소:

1. **DLL 복사 단계 실패** — `gstreamer.py`가 여전히 1.22.8 DLL 이름(`avcodec-59` 등)을 찾아 `could not find required GStreamer DLL`. → **해소**: `GSTREAMER_WIN_DEPENDENCY_LIBS`를 1.28.4.100 실제 이름으로 갱신(FFmpeg 7 `avcodec-61`/`avformat-61`/`avfilter-10`/`avutil-59`/`swresample-5` + `swscale-8` 신규, OpenSSL 3 `libcrypto-3-x64`/`libssl-3-x64`, 코덱 lib 접두사 제거 `jpeg8`/`ogg-0`/`png16`/`vorbis-0`/`vorbisenc-2`). 커밋 `ed38ad7e10c`.
2. **컴파일 스큐 잔존** — env var/servo_env를 안 바꾸면 컴파일은 여전히 시스템 1.26.8. → **해소**: env var를 1.28.4.100로 통일(servo_env는 `SERVO_ROOT=W:\servo`엔 슬롯#1이 없어 env var로 폴백 → 자동 반영).
3. **슬롯#1을 *비우면* release 링크 붕괴** — cargo `-sys` 빌드스크립트가 link-search를 슬롯#1 경로로 캐시. 슬롯#1을 지우면 `could not open gst*-1.0.lib`(debug는 캐시 exe라 안 걸림). → **해소**: 비우지 말고 **정션**으로 오버라이드(경로 유효 유지).
4. **stale 옛 DLL 잔존** — mach는 복사만 하고 옛 DLL 삭제 안 함 → `target\{debug,release}`에 1.22.8 `avcodec-59` 등 orphan 잔존(새 플러그인은 `-61`/`-3`만 import하므로 무해하나 혼동 유발). → **해소**: 겹치지 않는 옛 이름 12종 수동 삭제(각 빌드 후).

## (b) from-scratch(target 없는) 빌드 변경점

`target/`을 지우면 슬롯#1 정션도 사라진다. 재현을 보장하는 것은:
- **env var `GSTREAMER_1_0_ROOT_MSVC_X86_64`(Machine, 영구)** → `gstreamer_root()`가 슬롯#1 부재 시 #2(env var)로 해석 → 컴파일·DLL복사 모두 1.28.4.100.
- **커밋된 코드**: `gstreamer.py`(DLL 이름), `windows.rs.in`(플러그인).
- **`mach bootstrap-gstreamer` 금지** — 슬롯#1에 1.22.8 재설치함.

즉 새 클론/클린 빌드는 env var + 커밋된 두 파일만으로 1.28.4.100을 사용한다(정션은 현재 target을 보존한 채 전환할 때만 필요한 편의장치).

**세션 함정**: env var를 `SetEnvironmentVariable(...,'Machine')`로 바꿔도 *이미 실행 중인 프로세스와 그 자식*은 stale 환경 블록을 상속 → 그 세션의 빌드는 스크립트에서 `$env:GSTREAMER_1_0_ROOT_MSVC_X86_64`를 명시 지정해야 함. 레지스트리(Machine)엔 정상 저장되므로 **새 터미널/from-scratch는 올바르게 읽음**.

**부수 함정**: 시스템 `python`이 3.10 미만이면 `servo.gstreamer` import가 `X | Y` 타입 문법에서 실패 → `windows_dlls()` 검증은 두 리스트를 stdlib로 직접 대조.

## (c) 회귀 결과 (2026-07-22)

빌드: **debug** full mach `Finished dev in 13m42s` exit 0 / **release** `Finished release in 4m07s` exit 0. release에서 `could not open 'gstreamer-1.0.lib'` 없음(PKG_CONFIG forward-slash 방어), lld access violation 없음(link.exe 우회). 모든 실행에서 `ErrorLoadingPlugins`/`Failed to find element` 0.

| # | 기능 | 결과 |
|---|---|---|
| 0 | 플러그인 로드 | **PASS** (전 실행 ErrorLoadingPlugins 0) |
| 1 | 확장 이미지 | **PASS** (tiff/tga/exr/ppm/pgm/qoi/hdr/jxl/dds 전부 real=true 640x360) |
| 2 | 확장 컨테이너 | **PASS** (mkv/avi/wmv/ts/flv/mov err=none rs=4 320x240; ts t=0.75 advancing) |
| 3 | RTSP | **보류** (RTSP 소스 부재; rtsp err=4 = 소스 미제공 예상 동작) |
| 4 | getDisplayMedia | **PASS** (release 월, videoSize 1920x1080 advancing) |
| 5 | 캡처카드(getUserMedia) | **PASS** (2026-07-23, MZ0380 설치 후: 4 videoinput, 라이브 1920x1080 advancing, g_assert 0 — 단 I420 고정 수정 필요했음, 아래 (d)) |
| 6 | 코어 비디오 월(DComp/D3D11) | **PASS** (barrier ready=3/3, 패닉0) |
| 7 | 월 stress | **PASS** (클린 렌더; 종료 teardown만) |
| 8 | 월 스크롤·배리어 무회귀 | **PASS** (scroll matched 332, barrier complete 331) |

**신규 회귀 0.** 관측된 패닉은 전부 **월 종료(close) teardown 레이스**(`MakeCurrentFailed` gui.rs:183 + surfman `context.rs:177` assertion) — 정상 렌더 **후 종료 시각에만** 발생, 1.22.8·더미 백엔드에서도 재현되는 사전 존재 이슈로 이번 전환과 무관. (별개 이슈: debug 전용 webrender `GL error 500 at invalidate_framebuffer` 패닉은 release에 **0건** 재확인 — release는 `cfg!(debug_assertions)` GL 에러체크 미실행.)

## (d) 캡처카드 회귀 결과 (2026-07-23, 계획 Task 8)

MZ0380 PCI 캡처카드(4입력) 설치 후 재개. 프로브 `tests/html/multigpu_capture_card_probe.html`을
capture-card-getusermedia 브랜치에서 복원, `--wall-all-tiles` + `--pref dom_webrtc_enabled=true`로 실행.

- **enumerateDevices**: 4 videoinput("MZ0380 PCI"), **g_assert/abort 없음** — 1.28.4.100 커스텀
  winks가 1.22.8 `ks_video_probe_filter_for_caps` abort를 회피함을 실기 확인(전환의 원 동기 해소).
- **getUserMedia 초기 결과 = 스톨**(트랙은 열리나 videoSize 0x0): GST_DEBUG로 규명 —
  `<video_0:proxypad10> caps ... format=YUY2 not accepted`. 카메라 스트림 체인
  (`create_video_from`: device element → videoconvert → queue → proxysink)에 I420 고정이 없어
  videoconvert가 YUY2 passthrough로 협상 → `servomediastreamsrc`의 I420 전용 src pad 템플릿이
  proxy 경계에서 거부(NOT_NEGOTIATED). **getDisplayMedia에서 이미 기록된 함정**("I420 템플릿
  요구는 proxy 경계를 역전파하지 않음")의 카메라 경로 누락분.
- **수정**: `media_stream.rs::create_video_from`에 `capsfilter(video/x-raw,format=I420)`를
  videoconvert 뒤에 삽입(디스플레이 캡처 빈과 동일 패턴). 전 사용처(카메라/디스플레이 빈/mock/
  WebRTC proxy) 안전 — Servo appsink는 어차피 I420만 수용.
- **수정 후**: 라이브 트랙 **1920x1080 advancing**(currentTime 초당 전진, 월 3타일 팬아웃 표출) → **PASS**.
- 잔여(비차단, 사전 존재): `GStreamer-CRITICAL gst_pad_set_chain_function_full: GST_PAD_IS_SINK`
  ×2 — `media_stream_source.rs`가 ghost **src** pad에 `chain_function`을 설정(체인 함수는 sink 전용,
  업스트림 원형은 `proxy_pad_chain_function`이었을 것으로 추정). 데이터 흐름 무영향, 후속 정리 후보.
- 알려진 한계(이월): 4입력이 전부 동일 deviceId("MZ0380 PCI")로 노출, `get_track`은
  `devices.front()`만 사용 → 특정 입력 선택 불가(webidl deviceId 배선 필요, 별도 과제).

## 롤백 (비파괴)

1. 정션 제거: `Remove-Item <slot> -Force`(정션만 제거, 타깃 미영향) 후 `Rename-Item <slot>.bak-1228 msvc_x86_64`(1.22.8 복원).
2. env var 원복: `GSTREAMER_1_0_ROOT_MSVC_X86_64`(Machine) → `C:\Program Files\gstreamer\1.0\msvc_x86_64\`.
3. 코드 revert: `git revert ed38ad7e10c fd318f9dc33`.
4. 재빌드(cargo `-sys` 재실행 위해 PKG_CONFIG_PATH 변경 감지; 안 되면 관련 크레이트 clean).

## 미결

- ~~**#5 캡처카드**~~ → **완료(2026-07-23, (d) 참조)**. 이월 과제: deviceId 선택 배선(4입력 동일 id).
- **#3 RTSP**: 로컬 RTSP 소스(mediamtx 등) 준비 후 실측.
- **월 종료 teardown 크래시(②)**: 별개 사전 존재 이슈로 후속 검토 대상.
- **GST_PAD_IS_SINK CRITICAL ×2**: ghost src pad chain_function 오설정((d) 참조), cosmetic 정리 후보.

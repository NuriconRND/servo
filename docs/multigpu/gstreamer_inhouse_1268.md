# 사내 커스텀 GStreamer 1.26.8로 번들 교체 (2026-06-18)

월의 캡처카드 입력(getUserMedia/`ksvideosrc`)을 위해 servo의 번들 GStreamer를
기본 1.22.8 → **시스템에 설치된 사내 커스텀 1.26.8**(pkg-config `1.26.8.101`)로 교체.
1.22.8 `gstwinks`(ksvideosrc)는 이 캡처카드의 하드웨어 인코딩 포맷(H264/X264, KS 핀
`sample_size==0`) caps 탐색에서 `g_assert`로 abort하지만, 사내 커스텀 winks는 정상 처리.

## 메커니즘 — 모든 것이 `gstreamer_root()` 한 곳에서 갈림

`python/servo/platform/windows.py:138` 해상도 우선순위(`bin/ffi-7.dll` 존재로 탐지):
1. `servo/target/dependencies/gstreamer/1.0/msvc_X86_64`  ← 최우선 (bootstrap 슬롯)
2. `%GSTREAMER_1_0_ROOT_MSVC_X86_64%`  ← 시스템 설치(보통 여기 가리킴)
3. `C:\gstreamer\1.0\msvc_x86_64`

이 경로가 **빌드 링크**(`command_base.py:376` build_env가 `<root>/bin`을 PATH 앞에
추가 → gstreamer-rs `-sys`가 `<root>/bin/pkg-config`로 링크)와 **런타임 DLL 복사**
(`build_commands.py:351` copy_windows_dlls_to_build_directory가 `<root>`에서
`windows_dlls()`+`windows_plugins()`를 `target/debug`로 복사)를 **동시에** 결정.

## 적용한 변경

1. **슬롯#1을 커스텀 설치로 향하는 정션(junction)으로 (권장·견고)**: 기존 1.22.8
   `target/dependencies/gstreamer`를 치우고(rename/삭제), 그 자리에 시스템 설치를
   가리키는 디렉터리 정션을 만든다(관리자 불필요):
   ```
   mklink /J "<repo>\servo\target\dependencies\gstreamer\1.0\msvc_x86_64" ^
             "C:\Program Files\gstreamer\1.0\msvc_x86_64"
   ```
   ⚠️ **rename만 하고 env(슬롯#2)에 의존하지 말 것** — release를 한 번이라도
   1.22.8(슬롯#1)로 빌드했으면 cargo가 `-sys` 빌드스크립트의 link-search를
   `target/dependencies/gstreamer/.../lib`로 **캐시**해 두는데, 슬롯#1을 그냥 지우면
   그 경로가 사라져 release 링크가 `could not open 'gstplay-1.0.lib'`로 실패한다
   (cargo는 외부 설치 변경으로 -sys 빌드스크립트를 자동 재실행하지 않음; debug는
   캐시된 .exe라 안 걸려 보일 수 있음). 정션을 슬롯#1에 두면 캐시·신규 경로가 모두
   1.26.8로 resolve돼 이 문제를 회피한다. (대안: 커스텀 MSI를 슬롯#1에 직접 `/a`
   설치. 또는 깨끗이 하려면 `cargo clean` 후 재빌드.)
   ⚠️ `mach bootstrap-gstreamer`(또는 force)는 슬롯#1에 1.22.8을 다시 깔아 정션/시스템을
   덮어쓴다 — 사내 셋업에선 **bootstrap 하지 말 것**.
2. **`GSTREAMER_1_0_ROOT_MSVC_X86_64`** = `C:\Program Files\gstreamer\1.0\msvc_x86_64\`
   (시스템 설치가 보통 자동 설정. 정션을 쓰면 슬롯#1이 우선이라 필수는 아니나 빌드
   env에 명시 권장.) 시스템 설치엔 devel(pkg-config.exe / lib/pkgconfig/*.pc /
   include / *.lib)이 포함돼야 함.
3. **`python/servo/gstreamer.py` `GSTREAMER_WIN_DEPENDENCY_LIBS`**: 1.22.8 기준
   버전 스탬프 이름을 1.26.8 실제 DLL로 갱신(llvm-objdump로 확인):
   - FFmpeg 5→7: `avcodec-59→61`, `avformat-59→61`, `avfilter-8→10`,
     `avutil-57→59`, `swresample-4→5`, **`swscale-8` 추가**(avfilter-10 의존).
   - OpenSSL 1.1→3: `libcrypto-1_1-x64→libcrypto-3-x64`, `libssl-1_1-x64→libssl-3-x64`.
   - 코덱 lib 접두사 제거: `libjpeg-8→jpeg8`, `libogg-0→ogg-0`,
     `libpng16-16→png16`, `libvorbis-0→vorbis-0`, `libvorbisenc-2→vorbisenc-2`.
   - 나머지(glib/gobject/ffi-7/graphene/nice/opus/orc/theora/z 등)는 이름 동일.
   - `GSTREAMER_BASE_LIBS`(gst*-1.0-0.dll)는 버전 무관 이름이라 변경 불필요.
4. **`gstreamer_plugin_lists/windows.rs.in`**: 비디오 캡처 device provider/소스 등록
   `gstmediafoundation`, `gstwinks`(커스텀 ksvideosrc), `gstdirectshow` 추가
   (+ 기존 gstd3d11, gstwasapi). 의존 base lib는 전부 GSTREAMER_BASE_LIBS에 이미 존재.
5. `Cargo.toml`의 `gstreamer = {version="0.25", features=["v1_18"]}`는 1.26 dev와
   상위호환이라 변경 불필요. servoshell.exe는 GStreamer 1.x ABI 하위호환으로 1.26.8
   런타임에서 동작(전체 relink 불필요했음 — 필요 시 `cargo clean -p` 후 재빌드).

## 검증 (2026-06-18)

캡처카드 **MZ0380 PCI**(YUY2/YV12/NV12/BGR/BGRx/**H264/X264** 노출, 4 아날로그 입력):
- `enumerateDevices`: 11 total, **4 videoinput** 열거, assert 크래시 없음.
- `getUserMedia({video})`: tracks 1, **videoSize 1920x1080**, currentTime advancing.
- 테스트: `tests/html/multigpu_capture_card_probe.html`.
- **debug + release 둘 다** 빌드/실행 확인(release는 슬롯#1 정션으로 -sys
  link-search 캐시 경로가 1.26.8로 resolve돼 링크 통과).

## 슬롯#1 채우는 방법 변형 (정션 / MSI / Inno .exe / 직접 복사)

위 "적용한 변경" 1번은 **정션**을 썼지만, 슬롯#1
(`target/dependencies/gstreamer/1.0/msvc_x86_64`)을 채우는 방법은 여러 가지이며 결과는 동등하다.
공통 전제: 어느 방법이든 끝에 (a) `bin/ffi-7.dll` + `bin/pkg-config.exe --modversion`으로 해상도
확인, (b) `gstreamer.py` 의존 DLL 이름 버전 재확인, (c) `cargo clean` 후 `mach build`,
(d) **`mach bootstrap-gstreamer` 금지**(슬롯#1에 1.22.8 재설치됨). 어느 방법이든 `cargo clean`이
`target/`을 지우면 재구성 필요.

### A. 정션 (현재 방식) — 시스템 설치를 가리킴
디스크 중복 없음·즉시. 시스템 설치에 의존. 위 1번 참조:
```
rmdir "<repo>\servo\target\dependencies\gstreamer\1.0\msvc_x86_64"   # 기존 제거
mklink /J "<repo>\servo\target\dependencies\gstreamer\1.0\msvc_x86_64" ^
          "C:\Program Files\gstreamer\1.0\msvc_x86_64"
```

### B. MSI (런타임/devel 각 .msi) — administrative install
1.22.8~1.26.x는 런타임·devel이 **별도 MSI 2개**. `/a`(administrative install = 파일 추출만,
관리자/UAC 불필요)로 슬롯#1 상위(`target/dependencies`)에 추출하면 그 아래
`gstreamer\1.0\msvc_x86_64`가 생긴다. ⚠️ `mach bootstrap-gstreamer`로 하지 말 것
(UAC 승격으로 감싸 1603 실패 + 1.22.8 설치 — CLAUDE.md gotcha #3·#4). **비승격 직접 실행**:
```
$dep = "<repo>\servo\target\dependencies"
msiexec /a "<gstreamer-1.0-msvc-x86_64-X.Y.Z.msi>"       /qn TARGETDIR="$dep"   # 런타임
msiexec /a "<gstreamer-1.0-devel-msvc-x86_64-X.Y.Z.msi>" /qn TARGETDIR="$dep"   # devel
```

### C. Inno Setup .exe (GStreamer 1.28+, 단일 패키지)
1.28부터는 runtime/devel/debug를 **하나의 Inno Setup 6 `.exe`** + 플래그로 선택. `.exe` 위치는
설치 결과와 무관(어디서 실행하든 됨) — **`/DIR=`이 목적지**. `.exe`는 `target/` *밖*에 보관 권장
(`cargo clean`이 안 지우게). 비승격 무인 설치:
```
& "<gstreamer-custom-1.28.x.exe>" /VERYSILENT /SUPPRESSMSGBOXES /NORESTART `
    /DIR="<repo>\servo\target\dependencies\gstreamer" `
    /COMPONENTS="runtime,devel" /LOG="$env:TEMP\gst_install.log"
```
확인사항(설치본 의존, gstreamer.freedesktop.org/download Windows 가이드 참조):
- **`/DIR` nesting**: 설치 후 `...\gstreamer\1.0\msvc_x86_64\bin\ffi-7.dll`이 실제로 생기는지.
  installer가 `/DIR` 아래 `gstreamer\`를 또 만들면 `/DIR`을 한 단계 위(`...\dependencies`)로.
- **컴포넌트 ID**: `runtime`/`devel`/`debug`는 installer 작성 방식별로 다름(Inno엔 목록 출력
  CLI 없음). 공식 가이드/사내 빌더/1회 대화형 실행으로 확인. **devel 필수**(pkg-config/.pc/.lib/
  include). debug 심볼 필요 시 `,debug` 추가.
- **권한**: `target/dependencies`는 사용자 쓰기 가능이라 보통 비승격 OK. installer가
  `PrivilegesRequired=admin`이면 승격 또는 `/CURRENTUSER`(허용 시).
- (대안) `innoextract`로 순수 추출(레지스트리/권한 없이, MSI `/a`에 가장 근접) 후 슬롯#1 배치.

### D. 직접 복사 (이미 빌드/추출된 `msvc_x86_64` 트리가 있을 때) — 가장 단순
MSI/Inno는 결국 `msvc_x86_64`(runtime+devel 합본) 트리를 까는 포장지일 뿐. **이미 그 트리가
있으면 슬롯#1에 그대로 복사하면 동일 결과** (`mach bootstrap-gstreamer`만 안 하면 됨):
```
$slot1 = "<repo>\servo\target\dependencies\gstreamer\1.0\msvc_x86_64"
rmdir "$slot1"                                          # 기존 정션/폴더 제거
robocopy "<커스텀>\msvc_x86_64" "$slot1" /E /NFL /NDL /NJH /NJS
```
- **트리는 runtime+devel 합본**이어야 함(bin: DLL+ffi-7+pkg-config / lib: *.lib + pkgconfig/*.pc +
  gstreamer-1.0 플러그인 / include). 따로면 한 `msvc_x86_64`로 합쳐(devel을 runtime 위에) 복사.
- **`.pc` relocatable 확인**: `lib\pkgconfig\gstreamer-1.0.pc`의 `prefix=`가 `${pcfiledir}/../..`
  (상대)이면 어디로 복사해도 OK. 현재 1.26.8 트리는 relocatable 확인됨. 절대경로로 하드코딩된
  커스텀 빌드라면 그 경로로 깔거나 prefix를 고쳐야 pkg-config 링크가 됨(드묾).

## 함정: 1.28.x 커스텀 번들 — pkg-config 백슬래시로 release 링크 실패 (2026-06-25)

증상: 1.28.x 커스텀 트리로 교체 후 `mach build --release`가 마지막 `servoshell` 링크에서
```
lld-link: error: could not open 'gstreamer-1.0.lib': no such file or directory
```
로 실패. **`.lib`은 `...\msvc_x86_64\lib`에 분명히 존재.** 링크 커맨드의 gstreamer LIBPATH를 보면
백슬래시가 전부 사라져 있음:
`/LIBPATH:F:20260609_..._x86_64libpkgconfig/../../lib` (정상은 `F:\...\lib`).

원인: 호스트 Windows 타깃엔 servo가 `PKG_CONFIG_PATH`를 설정하지 않으므로(OHOS만 설정,
`build_target.py:433`), pkg-config가 **컴파일 내장 검색경로**로 폴백. 1.28.x 커스텀 번들의
pkg-config(0.29.2)는 그 내장 경로(`--variable pc_path`)를 **백슬래시**로 들고 있어
(`F:\...\lib\pkgconfig`), `.pc`의 relocatable `prefix=${pcfiledir}/../..`이 백슬래시 pcfiledir로
전개됨 → 내보내는 `-L` 경로가 백슬래시 → cargo/rustc→lld-link로 전달되며 **백슬래시가 escape로
소거**되어 경로 붕괴. (1.26.8 번들 pkg-config는 forward-slash/정규화를 해서 안 걸렸음.)

해결: 빌드 env에 **`PKG_CONFIG_PATH`를 forward-slash로** 설정(pkg-config가 이걸 내장경로보다
먼저 검색 → forward-slash pcfiledir → forward-slash `-L`, lld-link 통과). `scripts/servo_env.ps1`에
반영함(gstreamer_root 해상도와 동일하게 슬롯#1→env→C:\gstreamer 도출 후
`<root>/lib/pkgconfig;<root>/share/pkgconfig`를 `\`→`/` 치환해 설정):
```powershell
$env:PKG_CONFIG_PATH = "F:/.../msvc_x86_64/lib/pkgconfig;F:/.../msvc_x86_64/share/pkgconfig"
```
검증: 설정 후 `pkg-config --libs-only-L gstreamer-1.0 ...` 가 전부 forward-slash(백슬래시 0개).
적용 절차: **servo_env.ps1 재소싱 후 재빌드**. pkg-config `-sys` 빌드스크립트는
`rerun-if-env-changed=PKG_CONFIG_PATH`라 env 변경 시 재실행되어 링크캐시가 갱신됨(안 되면
`cargo clean` 후 재빌드).

## 남은 한계 / 후속

- **deviceId 선택 불가**: 4개 입력이 전부 `id="MZ0380 PCI"`로 동일 라벨/ID. 현재
  `get_track`(media_capture.rs)이 `devices.front()`만 잡아 특정 입력 선택 불가 →
  webidl `deviceId` 배선 + `get_track` device 매칭 필요(후속).
- 표출 경로는 getDisplayMedia와 동일 → raw passthrough(VP8 미경유) 그대로 적용.
- `windows.py`의 `GSTREAMER_URL`(1.22.8)은 bootstrap 시에만 쓰이며 사내 셋업에선
  미사용 — 필요하면 커스텀 MSI로 갱신.

## 표시 fps가 60에 못 미치는 이유 + GPU zero-copy 보류 (2026-06-18)

캡처카드는 60fps인데 servoshell 표시는 그보다 낮음. **CPU 경로 바운드**가 원인:
- 비디오 sink 정책 기본 **Smooth**(drop=false, qos=false, `render.rs` VideoSinkPolicy):
  appsink가 프레임을 안 버리고 backpressure → 라이브 src(ksvideosrc/mfvideosrc)가
  **캡처 단에서 프레임 드롭**. 즉 표시 fps = 소비자(렌더) 처리율.
- Windows는 `platform::Render = RenderDummy`(render.rs)라 **GPU zero-copy 경로 부재** →
  프레임마다 **YUY2→I420 CPU 변환**(videoconvert) + Y/U/V **3평면 CPU→GPU 업로드** + 씬 합성.
- 대조군 `ksvideosrc ! autovideosink`는 d3d11videosink(GPU)로 네이티브 YUY2 직결이라 60fps.

**GPU zero-copy(B1: RenderUnix 미러, Servo ANGLE EGL ↔ gstgl 공유) 보류.** 다운스트림
(`VideoFrameData::Texture`→`NativeTexture`→webrender)과 Windows ANGLE EGL 컨텍스트는
준비돼 있으나, **사내 커스텀 gstgl이 WGL 전용**(EGL/ANGLE 백엔드 없음: `gstgl-1.0-0.dll`이
`OPENGL32`만 import, `libEGL`/`libGLESv2` 부재, `GST_GL_PLATFORM=egl` 컨텍스트 생성 실패)
이라 ANGLE EGL 컨텍스트 공유가 불가 → B1 차단. 하려면 사내 GStreamer를 **gstgl
EGL/ANGLE 포함으로 재빌드**해야 함. 대안 B2(`d3d11upload!d3d11convert` →
`EGL_ANGLE_d3d_texture_client_buffer`로 D3D11 텍스처를 ANGLE GL로 import)는 신규 interop
작업 + surfman DXGI 미비 리스크. 추가로 `media_glvideo_enabled` pref도 기본 off(prefs.rs:560).

**보류 사유**: capture card 입력도 multi-GPU wall에선 어차피 **GPU간 분배**가 필요해
단일 GPU zero-copy가 최종 아키텍처가 아니며, 월 구현의 다른 고려사항과 함께 다룰 사안.

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

1. **슬롯#1 비활성화 (필수)**: `target/dependencies/gstreamer`를 비워야(없어야)
   해상도가 시스템 설치(슬롯#2)로 내려간다. 작업 시
   `target/dependencies/gstreamer` → `gstreamer.1228bak`로 rename.
   ⚠️ `mach bootstrap-gstreamer`(또는 force)는 슬롯#1에 1.22.8을 다시 깔아 시스템을
   가린다 — 사내 셋업에선 **bootstrap 하지 말 것**(시스템 1.26.8 사용).
2. **`GSTREAMER_1_0_ROOT_MSVC_X86_64`** = `C:\Program Files\gstreamer\1.0\msvc_x86_64\`
   (시스템 설치가 보통 자동 설정. 빌드 env에 명시 권장.) 시스템 설치엔 devel(
   pkg-config.exe / lib/pkgconfig/*.pc / include / *.lib)이 포함돼야 함.
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

## 남은 한계 / 후속

- **deviceId 선택 불가**: 4개 입력이 전부 `id="MZ0380 PCI"`로 동일 라벨/ID. 현재
  `get_track`(media_capture.rs)이 `devices.front()`만 잡아 특정 입력 선택 불가 →
  webidl `deviceId` 배선 + `get_track` device 매칭 필요(후속).
- 표출 경로는 getDisplayMedia와 동일 → raw passthrough(VP8 미경유) 그대로 적용.
- `windows.py`의 `GSTREAMER_URL`(1.22.8)은 bootstrap 시에만 쓰이며 사내 셋업에선
  미사용 — 필요하면 커스텀 MSI로 갱신.

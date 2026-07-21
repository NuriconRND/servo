# 비표준 미디어 표출 기능 재이식 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `nonstandard-media-formats`→`video-perf-investigation` 체인의 비표준 미디어 표출 기능(확장 이미지, 확장 비디오 컨테이너, RTSP, getDisplayMedia)을 표준 `<img>`/`<video>`/`navigator.mediaDevices` 경로로 현재 브랜치에 재이식한다.

**Architecture:** 커스텀 DOM 엘리먼트 없이 표준 태그가 타입을 감지해 확장 디코드/GStreamer/NetworkUri로 분기한다. 3계층(디코드/소스 인프라 → DOM 분기 → prefs), 전부 pref 게이트 기본 off. git merge/cherry-pick이 아니라 소스 브랜치에서 hunk를 추출해 현재 브랜치에 파일 단위로 재적용한다.

**Tech Stack:** Rust(Servo 워크스페이스), GStreamer 1.22.8(번들), `image` 0.25 크레이트, `jxl-oxide` 0.11, WebIDL codegen, PowerShell(빌드/스모크).

## Global Constraints

- **GStreamer 1.22.8 유지.** 번들 버전 스왑 금지. (#5 캡처카드 + 1.28.4 전환은 이 계획 범위 밖)
- **모든 신규 동작은 pref 게이트, 기본 off.** pref off 시 현재 브랜치와 동작 무변화(월 파이프라인 무회귀)가 계약이다.
- **커스텀 엘리먼트 제외.** `ximageelement.rs`/`rtspstreamelement.rs` 및 관련 webidl·`create.rs`·`element.rs`·`node.rs`·`virtualmethods.rs`·`html/mod.rs` 배선, 그리고 `dom_rtsp_stream_enabled`·`dom_x_image_enabled` pref는 이식하지 않는다.
- **소스 참조(verbatim 추출용 ref):**
  - base: `6694c14ea56`
  - 확장 이미지/RTSP 인프라: `nonstandard-media-formats`
  - 표준 태그 분기: `standard-tag-media-dispatch`
  - getDisplayMedia: `screen-capture-getdisplaymedia`
- **엔진 검증 관례(모든 코드 태스크의 종료 게이트):** `cargo check -p servoshell` → `cargo build -p servoshell`(또는 신규 플러그인/DLL 배선 시 `.\mach build -j 8`) → 관련 `cargo test` → `rustfmt --edition 2024 --check <touched .rs>` → `git diff --check`.
- **빌드 전 반드시** `. ..\scripts\servo_env.ps1` 소싱(이 워크트리는 repo 루트이고 `scripts/`는 워크트리 상위에 있음 — P0 확인). 엔진 명령은 이 워크트리 루트에서 실행.
- **경로 정정(P0):** 이 워크트리가 곧 servo repo 루트다. plugin_lists = `components/servo/gstreamer_plugin_lists/{common,windows,macos}.rs.in` (`ports/servo/...` 아님). 번들 GStreamer = `target/dependencies/gstreamer/1.0/msvc_x86_64/`(슬롯#1, 최우선 로드).
- **새 GStreamer 플러그인(element)은 `components/servo/gstreamer_plugin_lists/*.rs.in`의 로드 목록에 추가**해야 등록된다. `python/servo/gstreamer.py`의 `GSTREAMER_BASE_LIBS`는 플러그인이 링크하는 베이스 SO 목록으로, 새 플러그인이 이미 목록에 있는 base lib(gstrtsp/gstrtp 등)에만 의존하면 손댈 필요 없다. 플러그인 목록이 바뀌면 `cargo build`가 아니라 `mach build`로 DLL 복사 스텝을 재실행해야 한다.

---

## Task 0 (P0): 리스크 체크 & 선행 조사

**목적:** 이후 태스크의 전제(플러그인 가용성, 병합면, 테스트 자산)를 코드 착수 전에 확정한다. 코드 변경 없음. 산출물 = 조사 노트 커밋.

**Files:**
- Create: `docs/superpowers/plans/notes/2026-07-21-p0-findings.md`

- [ ] **Step 1: 번들 1.22.8의 RTSP 플러그인 가용성 확인**

Run:
```powershell
. ..\scripts\servo_env.ps1
$gst = "$PWD\servo\target\dependencies\gstreamer\1.0\msvc_x86_64\lib\gstreamer-1.0"
Get-ChildItem $gst -Filter '*.dll' | Where-Object { $_.Name -match 'rtsp|rtpmanager|gstrtp|udp|libav' } | Select-Object Name
```
Expected: `gstrtsp.dll`, `gstrtpmanager.dll`, `gstrtp.dll`, `gstudp.dll` 존재 여부를 기록. (번들 경로가 다르면 `gstreamer_root()` 해석 순서대로 슬롯 확인: `target/dependencies/...` → `GSTREAMER_1_0_ROOT_MSVC_X86_64` → `C:\gstreamer\1.0\msvc_x86_64`.)

- [ ] **Step 2: RTSP 플러그인이 현재 plugin_lists에 등록되는지 확인**

Run:
```powershell
Select-String -Path ports\servo\gstreamer_plugin_lists\*.in -Pattern 'rtsp|rtpmanager|gstrtp|udp' 
Select-String -Path python\servo\gstreamer.py -Pattern 'gstrtsp|gstrtpmanager|gstrtp|gstudp'
```
Expected: 현재 브랜치엔 없음(확인됨). Step 1에서 DLL이 존재하면 → Task 3에서 plugin_lists + `GSTREAMER_BASE_LIBS` 양쪽에 추가한다고 노트. DLL이 없으면 → Task 3 RTSP는 "플러그인 확보 필요"로 블로킹하고 사용자에게 보고.

- [ ] **Step 3: htmlmediaelement.rs 병합면 조사**

Run:
```powershell
git diff nonstandard-media-formats..standard-tag-media-dispatch -- components/script/dom/html/htmlmediaelement.rs > docs\superpowers\plans\notes\htmlmediaelement-dispatch.patch
git diff 6694c14ea56..nonstandard-media-display-port -- components/script/dom/html/htmlmediaelement.rs > docs\superpowers\plans\notes\htmlmediaelement-current.patch
```
두 패치를 열어 dispatch 변경(canPlayType, resource selection의 direct-URI 분기, `<source type>` 선택)이 현재 브랜치의 DComp 변경과 **같은 함수/줄을 건드리는지** 표로 기록. 겹치면 Task 3의 수동 병합 지점으로 명시.

- [ ] **Step 4: 테스트 자산 확보 확인**

확장 포맷 샘플(tiff/exr/qoi/jxl 각 1개, mkv/avi/wmv 각 1개)과 도달 가능한 RTSP 엔드포인트(`rtsp://...`)를 목록화. 소스 브랜치에 있던 `rtsp_testsrc.mp4`는 `git show nonstandard-media-formats:rtsp_testsrc.mp4 > rtsp_testsrc.mp4`로 복원 가능. RTSP 소스가 없으면 로컬 `mediamtx`/`gst-rtsp-server` 기동 방법을 노트에 기록. 자산이 없는 기능은 해당 태스크의 런타임 스모크를 "자산 대기"로 표시.

- [ ] **Step 5: 조사 노트 커밋**

```powershell
git add docs/superpowers/plans/notes/2026-07-21-p0-findings.md
git commit -m "docs(p0): RTSP 플러그인 가용성·htmlmediaelement 병합면·테스트 자산 조사"
```

---

## Task 1 (P1): 확장 이미지 포맷 (`<img>`)

가장 격리된 계층. 워크스페이스 의존성 → pixels 디코드 → DOM 분기 순.

### Task 1.1: 워크스페이스/크레이트 의존성 + prefs

**Files:**
- Modify: `Cargo.toml`(워크스페이스 루트, `[workspace.dependencies]`)
- Modify: `components/pixels/Cargo.toml`
- Modify: `components/config/prefs.rs`

**Interfaces:**
- Produces: pref `dom_image_extended_formats_enabled: bool`(기본 false); 워크스페이스 dep `jxl-oxide`; 확장된 `image` features.

- [ ] **Step 1: 워크스페이스 `image` features 확장 + `jxl-oxide` 추가**

`Cargo.toml`의 `image = { ... }` 라인을 교체하고 그 아래 `jxl-oxide`를 추가:
```toml
image = { version = "0.25", default-features = false, features = ["avif", "bmp", "dds", "exr", "ff", "gif", "hdr", "ico", "jpeg", "png", "pnm", "qoi", "rayon", "tga", "tiff", "webp"] }
```
`indexmap = ...` 다음 줄에:
```toml
jxl-oxide = "0.11"
```

- [ ] **Step 2: pixels 크레이트에 jxl-oxide 의존성 추가**

`components/pixels/Cargo.toml`의 `[dependencies]`에서 `image = { workspace = true }` 다음 줄에:
```toml
jxl-oxide = { workspace = true }
```

- [ ] **Step 3: pref 추가 (커스텀 엘리먼트용 pref는 제외)**

`components/config/prefs.rs` `pub struct Preferences`에서 `dom_resize_observer_enabled` 다음, `dom_sanitizer_enabled` 앞에 삽입:
```rust
    /// Let the STANDARD `<img>` element transparently decode image formats
    /// beyond the browser-standard allowlist (TIFF/EXR/HDR/TGA/DDS/QOI/PNM/JPEG
    /// XL/…) via `pixels::load_extended_from_memory`. Truly undecodable data
    /// still falls back to the standard broken-image/`error` path. Off by
    /// default; standard formats are unaffected.
    pub dom_image_extended_formats_enabled: bool,
```
그리고 `impl Preferences`의 기본값 블록에서 `dom_resize_observer_enabled: true,` 다음에:
```rust
            dom_image_extended_formats_enabled: false,
```
(주의: `dom_rtsp_stream_enabled`/`dom_x_image_enabled`는 추가하지 않는다.)

- [ ] **Step 4: 의존성 해소 확인**

Run: `. ..\scripts\servo_env.ps1; cargo check -p pixels`
Expected: 컴파일 성공(신규 `image` features/jxl-oxide 링크). 실패 시 `cargo update -p jxl-oxide` 후 재시도.

- [ ] **Step 5: 커밋**

```powershell
git add Cargo.toml Cargo.lock components/pixels/Cargo.toml components/config/prefs.rs
git commit -m "feat(pixels): 확장 이미지 디코드용 image features·jxl-oxide dep + dom_image_extended_formats_enabled pref"
```

### Task 1.2: pixels 확장 디코드 진입점 (+ 단위 테스트)

**Files:**
- Modify: `components/pixels/lib.rs`
- Test: `components/pixels/lib.rs`(하단 `#[cfg(test)]` 모듈)

**Interfaces:**
- Produces: `pub fn load_extended_from_memory(buffer: &[u8], extension: Option<&str>, cors_status: CorsStatus) -> Option<RasterImage>`; `fn decode_jxl(...)`; `fn is_jxl(buffer: &[u8]) -> bool`.
- Consumes: 기존 `load_from_memory`, `detect_image_format`, `decode_static_image`, `raster_from_rgba8_dynamic_image`(현재 pixels/lib.rs에 존재).

- [ ] **Step 1: 실패 테스트 작성**

`components/pixels/lib.rs` 하단 테스트 모듈에 추가(없으면 `#[cfg(test)] mod tests { use super::*; ... }` 생성). pixels 크레이트는 mozjs를 링크하지 않으므로 `cargo test -p pixels`가 정상 동작한다. 픽스처는 P0가 확인한 실경로의 JPEG XL 파일(`tests/wpt/tests/jpegxl/resources/*.jxl` 중 하나 — 구체 파일명은 구현 시 `ls`로 확정) 사용:
```rust
#[test]
fn extended_decode_rejects_empty() {
    assert!(load_extended_from_memory(&[], None, CorsStatus::Unsafe).is_none());
}

#[test]
fn extended_decode_handles_jpeg_xl() {
    // JPEG XL은 표준 `image` 크레이트가 지원하지 않으므로 신규 jxl-oxide 경로를 탄다.
    let jxl = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/wpt/tests/jpegxl/resources/<확정파일명>.jxl"
    ));
    let raster = load_extended_from_memory(jxl, Some("jxl"), CorsStatus::Unsafe);
    assert!(raster.is_some(), "jxl decode should succeed via jxl-oxide");
}
```

- [ ] **Step 2: 테스트 실패 확인**

Run: `. ..\scripts\servo_env.ps1; cargo test -p pixels extended_decode -- --nocapture`
Expected: FAIL — `load_extended_from_memory` 미정의.

- [ ] **Step 3: 소스에서 디코드 함수 이식**

`nonstandard-media-formats:components/pixels/lib.rs`에서 다음 심볼의 정의를 **verbatim 이식**한다:
`load_extended_from_memory`, `decode_jxl`, `is_jxl`, 그리고 이들이 참조하는데 현재 파일에 없는 헬퍼(`decode_static_image` 등 — `git diff 6694c14ea56..nonstandard-media-formats -- components/pixels/lib.rs`로 신규분만 선별). 본문은 이 저장소 소스가 정본이며 아래는 핵심부(전문은 diff 참조):
```rust
pub fn load_extended_from_memory(
    buffer: &[u8],
    extension: Option<&str>,
    cors_status: CorsStatus,
) -> Option<RasterImage> {
    if buffer.is_empty() { return None; }
    if detect_image_format(buffer).is_ok() { return load_from_memory(buffer, cors_status); }
    if is_jxl(buffer) { return decode_jxl(buffer, cors_status); }
    let mut reader = match image::ImageReader::new(Cursor::new(buffer)).with_guessed_format() {
        Ok(reader) => reader,
        Err(error) => { debug!("x-image: could not guess image format: {error}"); return None; },
    };
    if reader.format().is_none()
        && let Some(format) = extension.and_then(ImageFormat::from_extension)
    { reader.set_format(format); }
    let decoder = match reader.into_decoder() {
        Ok(decoder) => decoder,
        Err(error) => { debug!("x-image: could not create image decoder: {error}"); return None; },
    };
    decode_static_image(cors_status, decoder)
}
```
`decode_jxl`은 반드시 `render.image_all_channels()`(NOT `image()`)를 사용하는 소스 버전을 그대로 이식(채널 1/2/3/4 분기 포함).

- [ ] **Step 4: 테스트 통과 확인**

Run: `. ..\scripts\servo_env.ps1; cargo test -p pixels extended_decode -- --nocapture`
Expected: PASS.

- [ ] **Step 5: rustfmt + 커밋**

```powershell
rustfmt --edition 2024 --check components/pixels/lib.rs
git add components/pixels/lib.rs
git commit -m "feat(pixels): load_extended_from_memory + jxl-oxide 디코드(image_all_channels)"
```

### Task 1.3: `<img>` 분기 배선 + probe

**Files:**
- Modify: `components/net/image_cache.rs`
- Modify: `components/script/dom/html/htmlimageelement.rs`
- Create: `tests/html/multigpu_standard_img_extended_probe.html`(소스에서 복사)

**Interfaces:**
- Consumes: `pixels::load_extended_from_memory`(Task 1.2), pref `dom_image_extended_formats_enabled`(Task 1.1).

- [ ] **Step 1: image_cache 분기 이식**

`git diff nonstandard-media-formats..standard-tag-media-dispatch -- components/net/image_cache.rs`의 hunk를 현재 파일에 적용. 요지: 표준 `load_from_memory`가 `None`이고 `pref!(dom_image_extended_formats_enabled)`일 때 `load_extended_from_memory(bytes, extension, cors)`로 재시도. extension은 요청 URL 경로에서 파생.

- [ ] **Step 2: htmlimageelement 분기 이식**

`git diff nonstandard-media-formats..standard-tag-media-dispatch -- components/script/dom/html/htmlimageelement.rs`의 hunk 적용. 현재 브랜치와 충돌 시 P0 노트의 병합 지점 참조.

- [ ] **Step 3: probe 페이지 복사**

```powershell
git show standard-tag-media-dispatch:tests/html/multigpu_standard_img_extended_probe.html > tests\html\multigpu_standard_img_extended_probe.html
```

- [ ] **Step 4: 빌드 + 무회귀(pref off) 확인**

Run:
```powershell
. ..\scripts\servo_env.ps1; cargo build -p servoshell
target\debug\servoshell.exe tests\html\multigpu_standard_img_extended_probe.html 2> img_off.err.log
```
Expected: pref off 기본값에서 확장 이미지는 broken-image, 표준 이미지는 정상. 패닉 없음.

- [ ] **Step 5: pref on 스모크**

Run:
```powershell
target\debug\servoshell.exe --pref dom_image_extended_formats_enabled=true tests\html\multigpu_standard_img_extended_probe.html 2> img_on.err.log
```
Expected: probe가 tiff/exr/qoi/jxl 디코드 성공을 리포트(창을 닫아 stderr flush 후 로그 확인).

- [ ] **Step 6: 게이트 + 커밋**

```powershell
rustfmt --edition 2024 --check components/net/image_cache.rs components/script/dom/html/htmlimageelement.rs
git diff --check
git add components/net/image_cache.rs components/script/dom/html/htmlimageelement.rs tests/html/multigpu_standard_img_extended_probe.html
git commit -m "feat(img): 표준 <img> 확장 포맷 디코드 분기(pref 게이트) + probe"
```

---

## Task 2 (P2): 확장 비디오 컨테이너 (`<video>`)

**Files:**
- Modify: `components/config/prefs.rs`
- Modify: `components/script/dom/html/htmlmediaelement.rs`
- Modify: `python/servo/gstreamer.py`(`gstmpegts`)
- Create: `tests/html/multigpu_standard_video_extended_probe.html`

**Interfaces:**
- Produces: `fn is_extended_container_type(type_: &str) -> bool`; `const EXTENDED_CONTAINER_MIME_TYPES`; pref `dom_video_extended_containers_enabled`.

- [ ] **Step 1: pref 추가**

`components/config/prefs.rs`에 Task 1.1과 동일 방식으로 삽입(struct + 기본값):
```rust
    /// Let the STANDARD `<video>` element report non-standard containers
    /// (Matroska/AVI/WMV/MPEG-TS/FLV/…) as playable for `<source type>`
    /// selection and `canPlayType()`. Off by default.
    pub dom_video_extended_containers_enabled: bool,
```
기본값: `dom_video_extended_containers_enabled: false,`

- [ ] **Step 2~3: (단위 테스트 생략 — P0 확정)**

P0 결과: `components/script` 크레이트는 mozjs 전체를 링크해 `cargo test -p script` unit test가 비현실적이고 기존 전례도 0건. 따라서 `is_extended_container_type` 등 순수 헬퍼는 **런타임 probe(Step 6)로 검증**한다. `htmlmediaelement.rs`에 `#[test]`를 추가하지 말 것. (헬퍼 로직의 정확성은 소스 브랜치에서 verbatim 이식하므로 이미 검증된 코드다.)

- [ ] **Step 4: 헬퍼 + 분기 이식**

`standard-tag-media-dispatch:components/script/dom/html/htmlmediaelement.rs`에서 `EXTENDED_CONTAINER_MIME_TYPES`, `is_extended_container_type` 정의를 파일 하단에 verbatim 이식. 그리고 `git diff nonstandard-media-formats..standard-tag-media-dispatch -- components/script/dom/html/htmlmediaelement.rs`의 컨테이너 관련 hunk(canPlayType / `<source type>` 선택에서 `pref!(dom_video_extended_containers_enabled) && is_extended_container_type(&type_)` 분기)를 현재 파일의 대응 함수에 적용. 현재 브랜치 DComp 변경과의 충돌은 P0 노트 기준으로 수동 해소.

- [ ] **Step 5: gstmpegts 플러그인 등록**

`python/servo/gstreamer.py`의 `GSTREAMER_BASE_LIBS`에서 `gstcodecparsers` 다음에 `"gstmpegts",` 추가. plugin_lists는 `components/servo/gstreamer_plugin_lists/common.rs.in`(P0 정정 경로)에 컨테이너 demuxer 플러그인이 이미 있는지 확인(mkv=`gstmatroska`, avi=`gstavi`, asf/wmv=`gstasf`) 후 없는 것만 추가.

- [ ] **Step 6: probe 복사 + 빌드(mach) + 스모크**

```powershell
git show standard-tag-media-dispatch:tests/html/multigpu_standard_video_extended_probe.html > tests\html\multigpu_standard_video_extended_probe.html
. ..\scripts\servo_env.ps1; .\mach build -j 8    # 플러그인 배선 반영을 위해 mach 사용
target\debug\servoshell.exe --pref dom_video_extended_containers_enabled=true tests\html\multigpu_standard_video_extended_probe.html 2> vid_on.err.log
```
Expected: mkv/avi/wmv가 canPlayType/재생에서 인식. pref off 대조군(`vid_off.err.log`)에서 무변화.

- [ ] **Step 7: 게이트 + 커밋**

```powershell
rustfmt --edition 2024 --check components/config/prefs.rs components/script/dom/html/htmlmediaelement.rs
git add components/config/prefs.rs components/script/dom/html/htmlmediaelement.rs python/servo/gstreamer.py tests/html/multigpu_standard_video_extended_probe.html
git commit -m "feat(video): 표준 <video> 확장 컨테이너 인식(pref 게이트) + gstmpegts + probe"
```

---

## Task 3 (P3): RTSP (`<video src=rtsp://>`)

**전제:** Task 0 Step 1~2에서 RTSP 플러그인 DLL이 번들에 존재해야 한다. 없으면 이 태스크는 블로킹하고 사용자에게 보고.

**Files:**
- Modify: `components/config/prefs.rs`
- Modify: `components/media/backends/gstreamer/player.rs`(+ `lib.rs` 필요 시)
- Modify: `components/script/dom/html/htmlmediaelement.rs`
- Modify: `components/servo/gstreamer_plugin_lists/common.rs.in`(P0 정정 경로)
- Create: RTSP `<video>` probe(소스의 RTSP probe에서 표준 태그 변형)

**Interfaces:**
- Produces: `fn is_direct_uri_scheme(url: &ServoUrl) -> bool`; NetworkUri 플레이어 경로; pref `dom_video_network_uri_enabled`.

**P0 병합 충돌 지점(반드시 수동 확인):** htmlmediaelement.rs의 `update_media_state`(set_looping 호출 vs pause 가드 순서)와 `create_media_player`(set_resource_url 힌트 vs network_uri 파라미터)가 현재 브랜치 DComp 변경과 인접 블록. 라인은 안 겹쳐 3-way merge는 통과하겠으나 두 로직이 함께 옳게 동작하는지 검증.

- [ ] **Step 1: pref 추가**

```rust
    /// Let the STANDARD `<video>` element play direct-URI network streams
    /// (`rtsp://`/`rtsps://`) by routing to a GStreamer `NetworkUri` player
    /// instead of the AppSrc fetch path. Off by default.
    pub dom_video_network_uri_enabled: bool,
```
기본값 `dom_video_network_uri_enabled: false,`.

- [ ] **Step 2: (단위 테스트 생략 — P0 확정)**

Task 2와 동일: script 크레이트 mozjs 링크로 unit test 비현실적. `is_direct_uri_scheme`는 소스에서 verbatim 이식하고 런타임 RTSP probe(Step 6)로 검증. `#[test]` 추가하지 말 것.

- [ ] **Step 3: NetworkUri 플레이어 경로 이식**

`git diff 6694c14ea56..nonstandard-media-formats -- components/media/backends/gstreamer/player.rs`에서 **NetworkUri/rtsp 관련 hunk만** 선별 이식(커스텀 엘리먼트 전용 코드는 제외). 요지: `NetworkUri` 소스 종류를 받아 `uridecodebin`/playbin에 URI를 직접 넘기는 경로 추가. `lib.rs`에 노출 타입 변경이 있으면 함께 이식.

- [ ] **Step 4: htmlmediaelement direct-URI 분기 이식**

`is_direct_uri_scheme` 정의를 하단에 이식하고, resource fetch 알고리즘에서 `pref!(dom_video_network_uri_enabled) && is_direct_uri_scheme(&url)`일 때 AppSrc fetch 대신 NetworkUri 플레이어로 라우팅(소스 hunk 적용). P0 노트의 병합 지점 반영.

- [ ] **Step 5: RTSP 플러그인 등록 (P0로 범위 축소)**

P0 확정 결과: 필요한 플러그인 DLL은 번들 1.22.8에 전부 존재하고, `gstrtp`/`gstrtpmanager`는 이미 등록됨. **`components/servo/gstreamer_plugin_lists/common.rs.in`에 `"gstrtsp"`, `"gstudp"` 두 줄만 추가**(기존 `"gstrtp"`/`"gstrtpmanager"` 옆). `gstreamer.py`는 **변경 불필요**(gstrtsp base lib이 이미 `GSTREAMER_BASE_LIBS`에 있고 자동 계산됨).

- [ ] **Step 6: probe + mach build + 라이브 스모크**

```powershell
git show nonstandard-media-formats:rtsp_testsrc.mp4 > rtsp_testsrc.mp4   # 필요 시 로컬 RTSP 소스로 서빙
. ..\scripts\servo_env.ps1; .\mach build -j 8
target\debug\servoshell.exe --pref dom_video_network_uri_enabled=true "tests\html\<rtsp_probe>.html" 2> rtsp_on.err.log
```
Expected: `rtsp://` 소스가 재생(첫 프레임 도달 로그). pref off 대조군에서 무변화. RTSP 엔드포인트 미확보 시 "자산 대기"로 표시하고 사용자 보고.

- [ ] **Step 7: 게이트 + 커밋**

```powershell
rustfmt --edition 2024 --check components/config/prefs.rs components/script/dom/html/htmlmediaelement.rs components/media/backends/gstreamer/player.rs
git add -A
git commit -m "feat(video): 표준 <video>의 rtsp:// NetworkUri 재생(pref 게이트) + RTSP 플러그인 등록 + probe"
```

---

## Task 4 (P4): getDisplayMedia (화면 캡처)

**Files:**
- Modify: `components/config/prefs.rs`
- Create/Modify: `components/media/backends/gstreamer/media_capture.rs`, `media_stream.rs`, `media_stream_source.rs`, `lib.rs`, `Cargo.toml`
- Modify: `components/media/servo-media/lib.rs`, `components/media/streams/capture.rs`
- Modify: `components/script/dom/media/mediadevices.rs`
- Modify: `components/script_bindings/webidls/MediaDevices.webidl`, `components/script_bindings/codegen/Bindings.conf`
- Modify: `components/servo/gstreamer_plugin_lists/*.rs.in`(P0 정정 경로: `gstcodecs`, `gstd3d11` 추가), `python/servo/gstreamer.py`(필요 시 base lib)

**Interfaces:**
- Produces: `MediaDevices.getDisplayMedia()`; `servo_media::streams::capture::{DisplayCaptureSource, ...}`; `media.create_display_stream(source, constraints)`; prefs `dom_screen_capture_enabled`, `media_screen_capture_{monitor_index,show_cursor,window_title}`.

- [ ] **Step 1: prefs 추가**

`screen-capture-getdisplaymedia`의 prefs hunk(위 설계 §3에 명시)를 그대로 이식: `dom_screen_capture_enabled`(struct는 `dom_sanitizer_enabled` 다음), `media_screen_capture_monitor_index: i64 = -1`, `media_screen_capture_show_cursor: bool = true`, `media_screen_capture_window_title: String = String::new()`. 각 기본값 블록 위치는 소스 hunk의 라인 컨텍스트대로.

- [ ] **Step 2: servo-media capture 계층 이식**

`git diff standard-tag-media-dispatch..screen-capture-getdisplaymedia`의 다음 파일 hunk를 verbatim 이식: `components/media/streams/capture.rs`(`DisplayCaptureSource` 등 타입), `components/media/servo-media/lib.rs`(`create_display_stream`), backends/gstreamer `media_capture.rs`(`d3d11screencapturesrc`), `media_stream.rs`, `media_stream_source.rs`, `lib.rs`, `Cargo.toml`.

- [ ] **Step 3: DOM/webidl 이식**

`components/script/dom/media/mediadevices.rs`에 `GetDisplayMedia` 메서드 이식(위 §설계에 발췌한 소스 본문). `MediaDevices.webidl`에 `getDisplayMedia` 시그니처 추가, `codegen/Bindings.conf` 대응 항목 추가.

- [ ] **Step 4: 플러그인 등록**

`components/servo/gstreamer_plugin_lists/*.rs.in`(P0 정정 경로)에 `gstcodecs`, `gstd3d11` 추가. `gstreamer.py`는 이들이 새 base lib에 의존할 때만 갱신(P0 방식으로 확인).

- [ ] **Step 5: mach build + 스모크**

```powershell
. ..\scripts\servo_env.ps1; .\mach build -j 8
git show screen-capture-getdisplaymedia:tests/html/<getdisplaymedia_probe>.html > tests\html\getdisplaymedia_probe.html
target\debug\servoshell.exe --pref dom_screen_capture_enabled=true tests\html\getdisplaymedia_probe.html 2> gdm_on.err.log
```
Expected: `getDisplayMedia()`가 track 1개 반환, 캡처 프레임 도달 로그. pref off 시 API 미노출/거부.

- [ ] **Step 6: 게이트 + 커밋**

```powershell
rustfmt --edition 2024 --check <touched .rs>
git add -A
git commit -m "feat(mediadevices): getDisplayMedia() 화면 캡처(d3d11screencapturesrc, pref 게이트) + probe"
```

---

## Task 5: 통합 검증 & 무회귀 확인

**Files:** 없음(검증만). 산출물 = 검증 로그 요약 커밋.

- [ ] **Step 1: 전 pref off 무회귀 스모크**

Run:
```powershell
. ..\scripts\servo_env.ps1
target\debug\servoshell.exe --wall-layout ..\config\wall_layout.local_3x1.json --wall-all-tiles tests\html\multigpu_wall_sync_probe.html 2> wall_regress.err.log
```
Expected: 모든 신규 pref off 기본값에서 월 파이프라인 정상(scroll matched, barrier complete, 패닉 0). `tools/wall_perf_analyzer/analyze_wall_perf.py`로 확인.

- [ ] **Step 2: 전체 게이트 일괄**

Run:
```powershell
cargo test -p servoshell wall_layout --lib
cargo check -p servoshell
cargo build -p servoshell
git diff --check
```
Expected: 전부 통과.

- [ ] **Step 3: 검증 요약 노트 커밋**

각 기능(P1~P4)의 on/off 로그 요지를 `docs/superpowers/plans/notes/2026-07-21-verification.md`에 기록 후 커밋.

---

## Self-Review 결과

- **Spec coverage:** #1→Task1, #2→Task2, #4→Task4, RTSP→Task3, 무회귀 계약→Task5, P0 리스크→Task0. 스펙 전 항목 매핑됨.
- **제외 항목 준수:** 커스텀 엘리먼트/`dom_rtsp_stream_enabled`/`dom_x_image_enabled`/`media_wall_region_upload`/#6 하니스 미포함 확인.
- **Type consistency:** `load_extended_from_memory`/`is_extended_container_type`/`is_direct_uri_scheme`/`create_display_stream`/`DisplayCaptureSource` 명칭이 태스크 간 일치.
- **알려진 열림점:** script 크레이트 유닛 테스트 가능 여부(Task2/3 Step2~3)는 프로젝트 관례에 따라 런타임 probe로 대체 가능 — P0에서 확정. RTSP 플러그인 가용성은 P0 게이트.

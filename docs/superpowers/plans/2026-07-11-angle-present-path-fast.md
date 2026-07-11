# ANGLE present-path-fast (매 프레임 present 복사 제거) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** surfman의 ANGLE EGL 디스플레이 생성에 `EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE` 속성을 추가해, ANGLE이 오프스크린 텍스처 대신 스왑체인 백버퍼에 직접 렌더하게 만들어 `eglSwapBuffers`마다의 전창 `CopyResource`를 제거한다.

**Architecture:** LUID로 키된 두 `GetPlatformDisplay` 호출부(`Device::new`, `Device::new_isolated`의 어댑터 프로브)가 공용 헬퍼 `luid_display_attribs`로 동일한 속성 벡터(present-path-fast 포함)를 만들게 통일한다 — ANGLE이 LUID로 디스플레이를 캐시하므로 두 경로의 속성이 일치해야 같은 디스플레이로 수렴한다. 코드 변경은 `device.rs` 한 파일. Y축 등 나머지는 ANGLE이 내부(viewScale)에서 처리하므로 WR/surfman 다른 코드 변경 없음.

**Tech Stack:** Rust (vendored surfman 크레이트), winapi, ANGLE(mozangle) EGL 디스플레이 속성.

**스펙:** `docs/superpowers/specs/2026-07-11-angle-present-path-fast-design.md`

## Global Constraints

- 변경 범위: `third_party/surfman/src/platform/windows/angle/device.rs` 한 파일만. WebRender·servoshell·GStreamer·다른 surfman 파일 무수정.
- 게이트 없음 — 무조건 적용(기본 on). env/pref 분기 도입 금지.
- 추가 속성 (정확히 이 순서로, `egl::NONE` 종료 직전):
  `EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE`(0x33a4), `EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE`(0x33a9).
  두 상수는 `platform/generic/egl/ffi.rs:34,37`에 이미 정의됨 — import만 추가.
- `device.rs:372`의 `EGL_PLATFORM_DEVICE_EXT` 디스플레이(격리 WebGL)는 무변경 — 윈도우 서피스가 없어 present-path-fast 무효과.
- Y-flip 보정 코드를 WR/surfman에 억지로 넣지 않는다. 반전이 관측되면 present-path-fast를 철회(헬퍼 되돌림)한다 (휴리스틱 금지 원칙, 스펙 §6).
- Rust 주석은 이 파일 관례상 영어 허용(surfman은 영어 주석 크레이트). 커밋 메시지 한국어, Claude 서명 없이.

### 공통 명령 환경 (모든 cargo 명령 앞에 1회, PowerShell)

```powershell
. "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\servo_env.ps1"
$ErrorActionPreference = 'Continue'
Set-Location "D:\2_TechReview\20260606_multigpu_browser\servo"
$env:PATH = "C:\gstreamer\1.0\msvc_x86_64\bin;$env:PATH"
$env:GST_PLUGIN_PATH = "C:\gstreamer\1.0\msvc_x86_64\lib\gstreamer-1.0"
$env:PKG_CONFIG_PATH = "C:/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
```

---

### Task 1: `luid_display_attribs` 헬퍼 + present-path-fast 속성 + 두 LUID 경로 통일

**Files:**
- Modify: `third_party/surfman/src/platform/windows/angle/device.rs` (import 블록, `Device::new` 206–223, `Device::new_isolated` 프로브 285–293, 파일 하단에 헬퍼 + 테스트)

**Interfaces:**
- Consumes: 기존 상수 `EGL_PLATFORM_ANGLE_TYPE_ANGLE`, `EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE`, `EGL_PLATFORM_ANGLE_DEVICE_TYPE_ANGLE`, `EGL_PLATFORM_ANGLE_DEVICE_TYPE_D3D_WARP_ANGLE`, `EGL_PLATFORM_ANGLE_D3D_LUID_HIGH_ANGLE`, `EGL_PLATFORM_ANGLE_D3D_LUID_LOW_ANGLE` (이미 import됨); 신규 import `EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE`, `EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE`.
- Produces: `fn luid_display_attribs(d3d_driver_type: D3D_DRIVER_TYPE, luid_high: i32, luid_low: u32) -> Vec<EGLAttrib>` — `egl::NONE` 종료된 속성 벡터. WARP면 device-type WARP, 아니면 LUID high/low. 항상 present-path-fast 쌍 포함.

- [ ] **Step 1: 실패하는 단위 테스트 작성 (RED)**

`device.rs` 파일 맨 끝에 추가:

```rust
#[cfg(test)]
mod present_path_tests {
    use super::*;

    // 헬퍼가 present-path-fast 쌍을 NONE 종료 직전에 포함하고, LUID/WARP 분기가
    // 올바른지 확인한다. ANGLE 속성 배열은 반드시 EGL_NONE으로 끝나야 하며(미종료 시
    // ANGLE이 배열 끝을 지나쳐 읽음), 값 오타(FAST 대신 COPY 등)는 복사가 그대로
    // 남는 회귀이므로 값까지 검증한다.
    #[test]
    fn luid_branch_has_present_path_fast_and_luid() {
        let a = luid_display_attribs(D3D_DRIVER_TYPE_UNKNOWN, 0x1234, 0x5678);
        // NONE 종료
        assert_eq!(*a.last().unwrap(), egl::NONE as EGLAttrib);
        // present-path-fast 쌍 (key 다음에 FAST 값)
        let key = EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE as EGLAttrib;
        let pos = a.iter().position(|&x| x == key).expect("present-path key 없음");
        assert_eq!(a[pos + 1], EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE as EGLAttrib);
        // LUID high/low 존재
        let hk = EGL_PLATFORM_ANGLE_D3D_LUID_HIGH_ANGLE as EGLAttrib;
        let hp = a.iter().position(|&x| x == hk).expect("LUID high key 없음");
        assert_eq!(a[hp + 1], 0x1234 as EGLAttrib);
        // WARP device-type 속성은 LUID 분기에 없어야 함
        assert!(!a.contains(&(EGL_PLATFORM_ANGLE_DEVICE_TYPE_D3D_WARP_ANGLE as EGLAttrib)));
    }

    #[test]
    fn warp_branch_has_present_path_fast_and_warp() {
        let a = luid_display_attribs(D3D_DRIVER_TYPE_WARP, 0, 0);
        assert_eq!(*a.last().unwrap(), egl::NONE as EGLAttrib);
        let key = EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE as EGLAttrib;
        let pos = a.iter().position(|&x| x == key).expect("present-path key 없음");
        assert_eq!(a[pos + 1], EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE as EGLAttrib);
        // WARP 분기: device-type WARP 존재, LUID high 부재
        assert!(a.contains(&(EGL_PLATFORM_ANGLE_DEVICE_TYPE_D3D_WARP_ANGLE as EGLAttrib)));
        assert!(!a.contains(&(EGL_PLATFORM_ANGLE_D3D_LUID_HIGH_ANGLE as EGLAttrib)));
    }
}
```

- [ ] **Step 2: 테스트 실행 — 컴파일 실패 확인 (RED)**

Run (공통 환경 설정 후):
```powershell
cargo test -p surfman --features "sm-angle-default sm-angle-builtin" present_path_tests -- --nocapture
```
Expected: FAIL — `cannot find function 'luid_display_attribs'` (또는 신규 상수 미해결) 컴파일 에러.

만약 위 명령이 mozangle 재빌드/피처 문제로 실패하면(테스트 하네스가 standalone으로 안 뜨는 경우), Step 4에서 `cargo build`로 대체 검증하고 그 사실을 보고에 남긴다.

- [ ] **Step 3: import 추가 + 헬퍼 구현 + 두 호출부 배선**

(a) import 블록(파일 상단 `use crate::platform::generic::egl::ffi::{...}` — 현재 10–15행)에 두 상수 추가. 교체 전:

```rust
use crate::platform::generic::egl::ffi::{
    EGL_D3D11_DEVICE_ANGLE, EGL_EXTENSION_FUNCTIONS, EGL_NO_DEVICE_EXT, EGL_PLATFORM_ANGLE_ANGLE,
    EGL_PLATFORM_ANGLE_D3D_LUID_HIGH_ANGLE, EGL_PLATFORM_ANGLE_D3D_LUID_LOW_ANGLE,
    EGL_PLATFORM_ANGLE_DEVICE_TYPE_ANGLE, EGL_PLATFORM_ANGLE_DEVICE_TYPE_D3D_WARP_ANGLE,
    EGL_PLATFORM_ANGLE_TYPE_ANGLE, EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE, EGL_PLATFORM_DEVICE_EXT,
};
```

교체 후 (present-path 두 상수 추가):

```rust
use crate::platform::generic::egl::ffi::{
    EGL_D3D11_DEVICE_ANGLE, EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE,
    EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE, EGL_EXTENSION_FUNCTIONS, EGL_NO_DEVICE_EXT,
    EGL_PLATFORM_ANGLE_ANGLE, EGL_PLATFORM_ANGLE_D3D_LUID_HIGH_ANGLE,
    EGL_PLATFORM_ANGLE_D3D_LUID_LOW_ANGLE, EGL_PLATFORM_ANGLE_DEVICE_TYPE_ANGLE,
    EGL_PLATFORM_ANGLE_DEVICE_TYPE_D3D_WARP_ANGLE, EGL_PLATFORM_ANGLE_TYPE_ANGLE,
    EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE, EGL_PLATFORM_DEVICE_EXT,
};
```

(b) 헬퍼 추가 — `impl Device {` 블록 시작 직전(현재 194행 `impl Device {` 위)에 삽입:

```rust
// Build the EGL display attributes for an ANGLE D3D11 display keyed by adapter LUID
// (or WARP). Both LUID-keyed GetPlatformDisplay call sites use this so ANGLE, which
// caches displays by LUID, resolves them to the SAME cached display: mismatched
// attributes would split the cache key and hand the isolated-WebGL probe a different
// display than the compositor's, breaking cross-GPU shared-handle import (black tiles).
//
// The present-path-fast pair makes ANGLE render straight into the swapchain backbuffer
// instead of an offscreen texture + a per-frame CopyResource on eglSwapBuffers
// (see docs/superpowers/specs/2026-07-11-angle-present-path-fast-design.md).
fn luid_display_attribs(
    d3d_driver_type: D3D_DRIVER_TYPE,
    luid_high: i32,
    luid_low: u32,
) -> Vec<EGLAttrib> {
    let mut attribs = vec![
        EGL_PLATFORM_ANGLE_TYPE_ANGLE as EGLAttrib,
        EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE as EGLAttrib,
    ];
    if d3d_driver_type == D3D_DRIVER_TYPE_WARP {
        attribs.extend_from_slice(&[
            EGL_PLATFORM_ANGLE_DEVICE_TYPE_ANGLE as EGLAttrib,
            EGL_PLATFORM_ANGLE_DEVICE_TYPE_D3D_WARP_ANGLE as EGLAttrib,
        ]);
    } else {
        attribs.extend_from_slice(&[
            EGL_PLATFORM_ANGLE_D3D_LUID_HIGH_ANGLE as EGLAttrib,
            luid_high as EGLAttrib,
            EGL_PLATFORM_ANGLE_D3D_LUID_LOW_ANGLE as EGLAttrib,
            luid_low as EGLAttrib,
        ]);
    }
    attribs.extend_from_slice(&[
        EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE as EGLAttrib,
        EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE as EGLAttrib,
    ]);
    attribs.push(egl::NONE as EGLAttrib);
    attribs
}
```

(c) `Device::new`의 인라인 속성 빌드(206–223행)를 헬퍼 호출로 교체. 교체 전:

```rust
                let mut attribs = vec![
                    EGL_PLATFORM_ANGLE_TYPE_ANGLE as EGLAttrib,
                    EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE as EGLAttrib,
                ];
                if d3d_driver_type == D3D_DRIVER_TYPE_WARP {
                    attribs.extend_from_slice(&[
                        EGL_PLATFORM_ANGLE_DEVICE_TYPE_ANGLE as EGLAttrib,
                        EGL_PLATFORM_ANGLE_DEVICE_TYPE_D3D_WARP_ANGLE as EGLAttrib,
                    ]);
                } else {
                    attribs.extend_from_slice(&[
                        EGL_PLATFORM_ANGLE_D3D_LUID_HIGH_ANGLE as EGLAttrib,
                        adapter_desc.AdapterLuid.HighPart as EGLAttrib,
                        EGL_PLATFORM_ANGLE_D3D_LUID_LOW_ANGLE as EGLAttrib,
                        adapter_desc.AdapterLuid.LowPart as EGLAttrib,
                    ]);
                }
                attribs.push(egl::NONE as EGLAttrib);
                let egl_display = egl.GetPlatformDisplay(
                    EGL_PLATFORM_ANGLE_ANGLE,
                    ptr::null_mut(),
                    attribs.as_ptr(),
                );
```

교체 후:

```rust
                let attribs = luid_display_attribs(
                    d3d_driver_type,
                    adapter_desc.AdapterLuid.HighPart,
                    adapter_desc.AdapterLuid.LowPart,
                );
                let egl_display = egl.GetPlatformDisplay(
                    EGL_PLATFORM_ANGLE_ANGLE,
                    ptr::null_mut(),
                    attribs.as_ptr(),
                );
```

(d) `Device::new_isolated`의 프로브 속성 배열(285–293행)을 헬퍼 호출로 교체. 교체 전:

```rust
                            let attribs = [
                                EGL_PLATFORM_ANGLE_TYPE_ANGLE as EGLAttrib,
                                EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE as EGLAttrib,
                                EGL_PLATFORM_ANGLE_D3D_LUID_HIGH_ANGLE as EGLAttrib,
                                adapter_desc.AdapterLuid.HighPart as EGLAttrib,
                                EGL_PLATFORM_ANGLE_D3D_LUID_LOW_ANGLE as EGLAttrib,
                                adapter_desc.AdapterLuid.LowPart as EGLAttrib,
                                egl::NONE as EGLAttrib,
                            ];
                            let display = egl.GetPlatformDisplay(
                                EGL_PLATFORM_ANGLE_ANGLE,
                                ptr::null_mut(),
                                attribs.as_ptr(),
                            );
```

교체 후 (이 경로는 항상 non-WARP LUID지만, 헬퍼에 driver_type을 그대로 넘겨 `new`와 동일 로직 보장):

```rust
                            let attribs = luid_display_attribs(
                                d3d_driver_type,
                                adapter_desc.AdapterLuid.HighPart,
                                adapter_desc.AdapterLuid.LowPart,
                            );
                            let display = egl.GetPlatformDisplay(
                                EGL_PLATFORM_ANGLE_ANGLE,
                                ptr::null_mut(),
                                attribs.as_ptr(),
                            );
```

주의: `attribs`가 `Vec`가 되어 `.as_ptr()`로 넘기므로, `GetPlatformDisplay` 호출이 끝날 때까지 `attribs`가 살아 있어야 한다 — 위 코드에서 `attribs`는 같은 스코프에 남아 호출 이후까지 유효하므로 안전(기존 `new`도 동일 패턴).

- [ ] **Step 4: 테스트 실행 (GREEN)**

Run:
```powershell
cargo test -p surfman --features "sm-angle-default sm-angle-builtin" present_path_tests -- --nocapture
```
Expected: `luid_branch_has_present_path_fast_and_luid ... ok`, `warp_branch_has_present_path_fast_and_warp ... ok` (2 passed).

Step 2에서 standalone 테스트 하네스가 안 떴다면 대신:
```powershell
cargo build -p surfman --features "sm-angle-default sm-angle-builtin"
```
Expected: 컴파일 성공(warning 0), 그리고 헬퍼가 순수 함수임을 코드로 확인했다고 보고에 명시.

- [ ] **Step 5: 커밋**

```bash
cd /d/2_TechReview/20260606_multigpu_browser/servo
git add third_party/surfman/src/platform/windows/angle/device.rs
git commit -m "surfman ANGLE: present-path-fast 활성화 - 매 프레임 present 복사 제거 (LUID 디스플레이 속성 통일 헬퍼)"
```

---

### Task 2: 전체 빌드 + 런타임 검증 (RenderDoc 복사 소멸 + 반전/회귀)

**Files:** 없음 (검증 전용 — 결함 발견 시 Task 1로 복귀). 코드 커밋 없음.

**Interfaces:**
- Consumes: Task 1의 device.rs 변경, `etc/multigpu/run_video_wall_d3d11.ps1`, RenderDoc(`C:\Program Files\RenderDoc\renderdoccmd.exe` / `qrenderdoc.exe`), 분석 스크립트 `...\scratchpad\analyze_rdc.py`(present 이벤트에 CopyResource가 있는지 리포트).

- [ ] **Step 1: 전체 빌드**

Run (공통 환경 설정 후):
```powershell
.\mach build --release
```
Expected: `Finished \`release\`` + `Succeeded` (DLL 복사까지).

- [ ] **Step 2: RenderDoc 재캡처 — present 복사 소멸 확인 (핵심 지표)**

RenderDoc으로 servoshell 2×2를 주입 실행하고 한 프레임 캡처한다:
```powershell
Set-Location "D:\2_TechReview\20260606_multigpu_browser\servo"
$env:SERVO_MEDIA_D3D11_VIDEO="1"; $env:SERVO_MEDIA_DIRECT_FILE="1"; $env:SERVO_MEDIA_GAPLESS_LOOP="1"
$env:SERVO_MEDIA_SYNC_GROUP="4"; $env:SERVO_WIN_VSYNC="1"; $env:SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF="1"
$env:SERVO_GSTREAMER_AVDEC_MAX_THREADS="1"; $env:GST_PLUGIN_PATH=""; $env:GST_PLUGIN_SYSTEM_PATH_1_0=""; $env:RUST_LOG="warn"
$page="D:\2_TechReview\20260606_multigpu_browser\servo\tests\html\video_grid_6x6_play.html"
$url="file:///" + ($page -replace '\\','/') + "?cols=2`&rows=2"
$cap="C:\Users\ILWOON~1\AppData\Local\Temp\claude\D--2-TechReview-20260606-multigpu-browser\74d50e96-feb0-42fc-866a-d1a337cfb8aa\scratchpad"
& "C:\Program Files\RenderDoc\renderdoccmd.exe" capture -d "D:\2_TechReview\20260606_multigpu_browser\servo" -c "$cap\pp_fast" --opt-hook-children "D:\2_TechReview\20260606_multigpu_browser\servo\target\release\servoshell.exe" --window-size 1920x1080 $url
```
25초쯤 재생 후, servoshell 창을 포그라운드로 올리고 F12(캡처 트리거)를 보낸다 — 이 세션에서 쓴 방식(Win32 `SetForegroundWindow` + `keybd_event(0x7B)`; 창 클래스 "Window Class") 재사용. 캡처 `.rdc` 생성 후 servoshell 종료.

그 다음 `qrenderdoc --python`으로 분석(스크립트는 `os._exit(0)`로 UI 스킵). 새 캡처 경로로 `analyze_rdc.py`의 CAPTURE를 바꿔 실행하고, **present 그룹에 `CopyResource`/`copyOffscreenToBackbuffer`가 없는지** 확인한다.
Expected: 이전 캡처에 있던 `[COPY]`/present-time 복사가 **사라짐**(present 직전에 콘텐츠가 백버퍼로 복사되지 않음). 픽셀 히스토리로 백버퍼 중앙이 draw로 직접 채워지면(clear+copy 2회가 아니라 draw 포함) 성공.

만약 복사가 그대로 남아 있으면: present-path-fast가 활성화되지 않은 것 → RUST_LOG에 surfman을 넣어(아래) 폴백/미적용 여부를 조사하고 BLOCKED로 보고.

- [ ] **Step 3: 육안 — 상하/좌우 반전·색 확인 (Y-flip 리스크)**

```powershell
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 2 -Rows 2 -Detach -LogPrefix ppfast_visual
```
30초 재생하며 화면을 직접 확인:
- 비디오가 **상하 반전되지 않았는지**(카운터/자막이 바로 보이는지), 좌우 정상, 색 정상.
- 이어 45타일도 `-Cols 9 -Rows 5`로 육안 확인.
Expected: 반전 없음, 색 정상. 반전이 보이면 즉시 중단하고 스펙 §6대로 **present-path-fast 철회 검토**를 BLOCKED로 보고(헬퍼 되돌림 필요 — Task 1 재작업).

- [ ] **Step 4: 45타일 회귀**

```powershell
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 9 -Rows 5 -Detach -LogPrefix ppfast_reg45
```
60초 후 최신 로그 판정:
```powershell
$log = Get-ChildItem target\multigpu_logs\ppfast_reg45_*_stderr.log | Sort-Object LastWriteTime | Select-Object -Last 1
"FAIL=" + (Select-String -Path $log.FullName -Pattern "비 D3D11 메모리|convert_buffer 실패|생성 실패|프레임 map 실패" | Measure-Object).Count
"IMPORT_WARN=" + (Select-String -Path $log.FullName -Pattern "import" | Measure-Object).Count
"MARKERS=" + (Select-String -Path $log.FullName -Pattern "GPU " -SimpleMatch | Measure-Object).Count
Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force -Confirm:$false
```
Expected: FAIL=0, IMPORT_WARN=0, MARKERS=45. 육안으로 lockstep(타일 간 프레임 ±1)·블랙 타일 0·시작 곡선(동기 릴리즈 후 5초 내 60fps).

- [ ] **Step 5: 격리 WebGL 월 스모크 — 캐시 일관성 (블랙 타일 0)**

WebGL 월 런처(`etc/multigpu`에 있는 WebGL/three 계열, 예: `run_three_retargeting_wall.ps1`)를 실행해 격리 디바이스 경로(`new_isolated`)를 태우고, **블랙 타일이 없는지**(캐시 키 불일치 회귀 부재) 육안 확인.
```powershell
.\etc\multigpu\run_three_retargeting_wall.ps1
```
Expected: 모든 타일 정상 렌더(블랙 타일 0), import/OpenSharedResource 경고 0. (이 런처 인자·페이지는 파일 헤더 참조.)

- [ ] **Step 6: 창 리사이즈 1회**

2×2 실행 중 창을 드래그로 키웠다 줄였다 1회씩 하고 깨짐(검은 영역/찢김/크래시)이 없는지 확인.
Expected: 리사이즈 후에도 정상 렌더.

- [ ] **Step 7: 결과 기록**

RenderDoc 복사 소멸 여부(핵심), 반전 유무, 회귀 결과를 세션 노트/메모리에 남기고, AMD 실기 창확대 fps 측정을 후속 항목으로 사용자에게 보고. 코드 커밋 없음(Task 1이 유일한 코드 변경).

---

## Self-Review (작성 후 점검 완료)

1. **스펙 커버리지**: §5.1 헬퍼(Task 1 Step 3b), §5.2 두 LUID 경로 통일 + device-ext 무변경(Step 3c/3d + Global Constraints), §5.3 동작 결과(런타임 검증 Task 2 Step 2), §5.4 게이트 없음(Global Constraints), §6 리스크(Task 2 Step 2/3/5/6 각각 대응 + 반전 시 철회), §7 검증 5단계(Task 2 Step 2–6), §3 성립조건(테스트/무변경 근거로 반영). §8 비범위 준수(env·WR·DirectComposition 무변경).
2. **플레이스홀더 스캔**: TBD/TODO/"적절히" 없음. 모든 코드 스텝에 실제 교체 전/후 코드, 실행 스텝에 명령+기대. RenderDoc/육안 검증은 본질적으로 런타임이라 서술형이나, 판정 기준(복사 소멸/반전 없음/FAIL=0)은 구체.
3. **타입 일관성**: `luid_display_attribs(d3d_driver_type: D3D_DRIVER_TYPE, luid_high: i32, luid_low: u32) -> Vec<EGLAttrib>`가 정의(Step 3b)·호출(Step 3c `new`, Step 3d `new_isolated`)·테스트(Step 1)에서 동일. `HighPart`(i32/LONG)·`LowPart`(u32/DWORD) 타입이 기존 캐스팅과 일치. 상수 import 이름이 ffi.rs와 일치(`EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE`/`_FAST_ANGLE`).

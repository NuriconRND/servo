# WR Native Compositor (DirectComposition) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `SERVO_COMPOSITOR_DCOMP=1` 게이트 on 시 WR picture cache 타일을 DirectComposition 서피스에 직접 그리고 DWM이 합성 — 타일→백버퍼 draw(②단, 창면적 읽기+쓰기) 소멸로 구형 AMD 창 확대 급락 해소.

**Architecture:** WR 0.68이 제공하는 `Compositor` trait(`CompositorConfig::Native`)를 Servo embedder 쪽에서 구현. `DCompNativeCompositor`(components/paint)가 DComp 디바이스·비주얼 트리를 소유하고, 타일 bind 시 DComp `BeginDraw` 텍스처를 EGL pbuffer로 래핑(surfman 경유)해 WR이 fbo 0으로 그리게 한다. WR 수정 없음(crates.io 0.68 그대로).

**Tech Stack:** Rust, webrender 0.68 (crates.io), vendored surfman(third_party/surfman, ANGLE 백엔드), winapi 0.3.9 `um::dcomp`, DirectComposition/DWM.

**Spec:** `docs/superpowers/specs/2026-07-13-wr-native-compositor-design.md`

## Global Constraints

- 게이트 `SERVO_COMPOSITOR_DCOMP` 기본 off. **off면 현행과 바이트 동일 경로** (compositor_config 미설정 = Draw).
- 전역 상태(싱글턴/static) 금지 — 컴포지터는 창(painter) 단위 인스턴스.
- 초기화 어느 단계든 실패 시 warn + Draw 폴백. 화면이 안 나오는 상황 금지.
- `components/paint`는 clippy `unwrap_used = "deny"`, `panic = "deny"` — unwrap/panic 금지, 실패는 Option/로그로.
- 커밋 메시지는 한국어, Claude 부기(Co-Authored-By 등) 금지.
- ★빌드 함정(§3-q 원장 검증됨): `cargo check --workspace` 및 script 계열 bare check **금지**(mozjs_sys 행). 워크스페이스 게이트는 `.\mach build --release`(etc/multigpu/servo_env.ps1 소싱 + `$ErrorActionPreference='Continue'`). leaf 크레이트(-p surfman / -p servo-paint-api / -p servo-paint)는 cargo check/test 가능하되 gstreamer 의존이 걸리면 `$env:PKG_CONFIG_PATH="C:/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"` 필요.
- 같은 target에 cargo 2개 동시 실행 금지. 장기 명령 출력은 파일 리다이렉트.
- 한글 포함 파일 수정은 Edit 도구로만 (PS5.1 Get-Content→Set-Content 인코딩 오염).
- ★HALT 규칙: Task 3 PoC 게이트 4개 중 하나라도 FAIL이면 이후 태스크 중단 + 사용자 문의.

## 확정된 코드 앵커 (조사 완료, 2026-07-13)

| 항목 | 사실 |
|---|---|
| WR 옵션 | `WebRenderOptions.compositor_config: CompositorConfig` (webrender-0.68 renderer/init.rs:186, 기본 Draw). Servo painter.rs:361 `create_webrender_instance`는 현재 미설정 |
| Compositor trait | composite.rs:1446, `webrender::Device` pub export(lib.rs:180) |
| Y방향 | `DrawTarget::NativeSurface`는 WR이 **무조건 top-left** 취급(device/gl.rs:1296, to_framebuffer_rect flip 없음 :1288). 전역 `surface_origin_is_top_left` 변경 불필요(Layer 모드만 자동 set, init.rs:336-341) |
| 렌더러 호출 순서 | render_impl: kind-switch 체크(1619, `enable_native_compositor`는 디버그 토글시에만) → `update_native_surfaces`(1670→5127: Create/Destroy Surface/Tile/External 옵 드레인) → `begin_frame`(1687) → update_debug_overlay → 타일별 bind/unbind(3187/3308) → `composite_native`(6656: 서피스별 add_surface + start_compositing) → `end_frame`(1913) |
| 외부 서피스 | `prefer_compositor_surface` — Servo 전체 grep 0건 → CreateExternalSurface/AttachExternalImage/Backdrop 옵 미발생, stub 안전 |
| virtual_surface_size | WR은 타일 그리드를 가상공간 중심에 배치(picture.rs:2477 `vss/2` 사용). 0이면 비활성. Gecko 관례 = 1024*32 |
| winapi | 0.3.9 `um::dcomp`에 DCompositionCreateDevice/IDCompositionDevice/CreateTargetForHwnd/IDCompositionVisual/IDCompositionVirtualSurface 전부 존재. paint 크레이트 winapi features 현재 `["processthreadsapi","winbase"]` |
| surfman pbuffer | `CreatePbufferFromClientBuffer(EGL_D3D_TEXTURE_ANGLE=0x33a3)` 코드 기존재(angle/surface.rs:202,:438). 단 기존 `create_pbuffer_surface`는 share-handle/keyed-mutex assert가 있어 DComp 텍스처에 부적합 → 경량 변형 신설 |
| surfman 디바이스 | `d3d11_device_ptr()` 기존재(angle/device.rs:486). §3-q 인터롭 패턴(map_d3d11_dynamic_texture 등, rendering_context.rs:122-140 trait 기본 + :550 Surfman 구현 + :820/:1091/:1463 위임) 그대로 따를 것 |
| ★HWND 충돌 | surfman `create_window_surface`(angle/surface.rs:279-372)가 `EGL_DIRECT_COMPOSITION_ANGLE` 요청 → ANGLE이 자체 DComp 타깃 생성 가능. **게이트 on이면 이 속성 없이 생성해야** 우리 타깃이 유일 (`CreateTargetForHwnd`는 (hwnd,topmost)당 1개). present-path-fast는 디스플레이 속성이라 무영향(별개 축, §3-o) |
| HWND 획득 | `WindowRenderingContext::new`(rendering_context.rs:867-)가 `WindowHandle` 수신 — Win32 hwnd 추출·보관 (connection.rs:193 `handle.hwnd.get()` 패턴) |
| painter 흐름 | make_current(:640) → prepare_for_rendering → renderer.update() → clear_background() → renderer.render(:665) — 전부 `paint_api::ANGLE_GL_LOCK` 하. 복원 지점 = render() 직후 |
| 판정 지표 | WR 프로파일러에 composite 전용 카운터 없음 → Renderer time + GPU% A/B + PresentMon으로 판정 |
| PoC 선례 | `components/shared/paint/examples/d3d11_dyn_poc.rs`, 실행 `cargo run --release -p servo-paint-api --example d3d11_dyn_poc --features no-wgl` |

## File Structure

- Modify: `third_party/surfman/src/platform/windows/angle/surface.rs` — 창 서피스 DComp opt-out
- Modify: `third_party/surfman/src/platform/windows/angle/device.rs` — render-pbuffer 프리미티브 3종
- Modify: `components/shared/paint/rendering_context.rs` — 인터롭 trait 메서드 5종 + WindowRenderingContext hwnd 보관
- Create: `components/shared/paint/examples/dcomp_native_poc.rs` — PoC 게이트
- Modify: `components/shared/paint/Cargo.toml` — winapi dcomp features (dev)
- Create: `components/paint/dcomp_compositor.rs` — DCompNativeCompositor (핵심 신규 모듈)
- Modify: `components/paint/lib.rs` — 모듈 등록
- Modify: `components/paint/Cargo.toml` — winapi features 추가
- Modify: `components/paint/painter.rs` — 게이트 + compositor_config + 렌더 후 복원
- Modify: `etc/multigpu/run_video_wall_d3d11.ps1` — `-DComp` 스위치 (Task 7)

---

### Task 1: surfman — 창 서피스 DComp opt-out + render-pbuffer 프리미티브

**Files:**
- Modify: `third_party/surfman/src/platform/windows/angle/surface.rs:279-372` (create_window_surface)
- Modify: `third_party/surfman/src/platform/windows/angle/device.rs` (Device impl 끝부분, :486 d3d11_device_ptr 근처)

**Interfaces:**
- Produces (Task 2·3이 사용):
  - `pub(crate) fn dcomp_native_compositor_requested() -> bool` (surface.rs)
  - `Device::create_render_pbuffer_from_d3d_texture(&self, context: &Context, texture: *mut c_void, size: Size2D<i32>) -> Option<EGLSurface>` (unsafe)
  - `Device::make_render_pbuffer_current(&self, context: &Context, egl_surface: EGLSurface) -> bool` (unsafe)
  - `Device::destroy_render_pbuffer(&self, egl_surface: EGLSurface)` (unsafe)

- [ ] **Step 1: 게이트 헬퍼 + 창 서피스 opt-out 작성**

surface.rs — 파일 상단 use 아래에 추가:

```rust
/// SERVO_COMPOSITOR_DCOMP 게이트. on이면 창 서피스는 DirectComposition 속성 없이
/// 만들어 HWND의 DComp 타깃을 Native Compositor 전용으로 남긴다
/// (CreateTargetForHwnd는 (hwnd, topmost)당 1개만 허용).
pub(crate) fn dcomp_native_compositor_requested() -> bool {
    std::env::var("SERVO_COMPOSITOR_DCOMP").is_ok_and(|v| {
        v == "1"
            || v.eq_ignore_ascii_case("true")
            || v.eq_ignore_ascii_case("yes")
            || v.eq_ignore_ascii_case("on")
    })
}
```

`create_window_surface`의 분기(:296)를 다음으로 교체:

```rust
                let egl_surface = if ensure_dcomp_loaded() && !dcomp_native_compositor_requested() {
                    // (기존 direct_composition_attributes 경로 그대로)
```

그리고 기존 `} else {` 블록(dcomp.dll 없음 경로) 진입 직전에 로그 분기 추가:

```rust
                } else {
                    if dcomp_native_compositor_requested() {
                        info!(
                            "SERVO_COMPOSITOR_DCOMP=1: creating plain HWND window surface \
                             (window DComp target reserved for the native compositor)"
                        );
                    } else {
                        warn!(
                            "dcomp.dll is unavailable; creating default ANGLE HWND surface without \
                             DirectComposition flip-model request"
                        );
                    }
                    egl.CreateWindowSurface(/* 기존 default_attributes 경로 그대로 */)
                };
```

- [ ] **Step 2: device.rs에 render-pbuffer 프리미티브 3종 추가**

`d3d11_device_ptr`(:486) 아래에 추가. 주의: attribs는 검증된 기존 `create_pbuffer_surface`(:185-198)와 동일 구성을 쓰되, share-handle/keyed-mutex 질의(assert 포함)는 **하지 않는다**(DComp BeginDraw 텍스처는 공유 핸들이 없어 assert가 죽는다). `Context`의 EGL 컨텍스트 필드명은 `context.rs`의 구조체 정의를 따를 것(egl_context — Drop 경로 :176 부근에서 확인 가능).

```rust
    /// DComp BeginDraw가 돌려준 RENDER_TARGET 텍스처를 WR이 그릴 EGL pbuffer로
    /// 래핑한다. 공유 핸들 질의 없음(DComp 텍스처는 비공유). 실패 시 None + 로그.
    pub unsafe fn create_render_pbuffer_from_d3d_texture(
        &self,
        context: &Context,
        texture: *mut c_void,
        size: Size2D<i32>,
    ) -> Option<EGLSurface> {
        let context_descriptor = self.context_descriptor(context);
        let egl_config = self.context_descriptor_to_egl_config(&context_descriptor);
        let attributes = [
            egl::WIDTH as EGLint,
            size.width as EGLint,
            egl::HEIGHT as EGLint,
            size.height as EGLint,
            egl::TEXTURE_FORMAT as EGLint,
            egl::TEXTURE_RGBA as EGLint,
            egl::TEXTURE_TARGET as EGLint,
            egl::TEXTURE_2D as EGLint,
            egl::NONE as EGLint,
        ];
        EGL_FUNCTIONS.with(|egl_fns| {
            let surface = egl_fns.CreatePbufferFromClientBuffer(
                self.egl_display,
                EGL_D3D_TEXTURE_ANGLE,
                texture as *const _,
                egl_config,
                attributes.as_ptr(),
            );
            if surface == egl::NO_SURFACE {
                let error = egl_fns.GetError();
                log::warn!("create_render_pbuffer_from_d3d_texture failed (EGL 0x{error:x})");
                return None;
            }
            Some(surface)
        })
    }

    /// pbuffer를 현재 컨텍스트의 draw/read 서피스로 바인딩한다(컨텍스트는 유지).
    pub unsafe fn make_render_pbuffer_current(
        &self,
        context: &Context,
        egl_surface: EGLSurface,
    ) -> bool {
        EGL_FUNCTIONS.with(|egl_fns| {
            let ok = egl_fns.MakeCurrent(self.egl_display, egl_surface, egl_surface, context.egl_context);
            if ok == egl::FALSE {
                let error = egl_fns.GetError();
                log::warn!("make_render_pbuffer_current failed (EGL 0x{error:x})");
            }
            ok != egl::FALSE
        })
    }

    /// render pbuffer 해제. EGL은 현재 바인딩 중이면 실제 파괴를 유예하므로
    /// unbind 직후 호출해도 안전하다.
    pub unsafe fn destroy_render_pbuffer(&self, egl_surface: EGLSurface) {
        EGL_FUNCTIONS.with(|egl_fns| {
            egl_fns.DestroySurface(self.egl_display, egl_surface);
        });
    }
```

필요 use(EGL 타입/함수/상수)는 같은 파일 기존 항목과 surface.rs 상단을 참조해 보충한다 (`EGL_D3D_TEXTURE_ANGLE`는 `platform::generic::egl::ffi`).

- [ ] **Step 3: 컴파일 확인**

Run: `cd D:\2_TechReview\20260606_multigpu_browser\servo; cargo check -p surfman 2>&1 | Select-Object -Last 5`
Expected: `Finished` (에러 0). (§3-q Task 1과 동일 게이트 — surfman은 leaf, bare check 가능)

- [ ] **Step 4: Commit**

```
git add third_party/surfman/src/platform/windows/angle/surface.rs third_party/surfman/src/platform/windows/angle/device.rs
git commit -m "surfman: DComp 네이티브 컴포지터용 프리미티브 추가

- SERVO_COMPOSITOR_DCOMP=1이면 창 서피스를 DComp 속성 없이 생성
  (HWND DComp 타깃을 네이티브 컴포지터 전용으로 확보)
- D3D 텍스처를 그리기용 EGL pbuffer로 래핑/현재화/해제하는 3종 추가
  (공유핸들 질의 없음 - DComp BeginDraw 텍스처는 비공유)"
```

---

### Task 2: RenderingContext 인터롭 확장 (hwnd + pbuffer 5종)

**Files:**
- Modify: `components/shared/paint/rendering_context.rs`

**Interfaces:**
- Consumes: Task 1의 surfman Device 메서드 3종.
- Produces (Task 3·4·5가 사용) — `RenderingContext` trait 메서드:
  - `fn window_hwnd(&self) -> Option<usize>` (기본 None)
  - `fn angle_d3d11_device_ptr(&self) -> Option<usize>` (기본 None; AddRef 없음 — 수명은 컨텍스트 소유)
  - `fn create_render_pbuffer_from_d3d_texture(&self, texture: usize, size: Size2D<i32>) -> Option<usize>` (기본 None; 반환=EGLSurface)
  - `fn make_render_pbuffer_current(&self, egl_surface: usize) -> bool` (기본 false)
  - `fn destroy_render_pbuffer(&self, egl_surface: usize)` (기본 no-op)

- [ ] **Step 1: trait 기본 메서드 5종 추가**

기존 D3D11 인터롭 기본 메서드 블록(:122-140, `map_d3d11_dynamic_texture` 부근) 바로 아래에 추가:

```rust
    /// Native compositor(DirectComposition) 인터롭: 이 컨텍스트가 붙은 창의 HWND.
    /// Windows 창 컨텍스트에서만 Some.
    fn window_hwnd(&self) -> Option<usize> {
        None
    }
    /// ANGLE의 D3D11 디바이스 raw 포인터. AddRef 하지 않는다 — 수명은 이
    /// 렌더링 컨텍스트가 보유하므로 컨텍스트보다 오래 들고 있으면 안 된다.
    fn angle_d3d11_device_ptr(&self) -> Option<usize> {
        None
    }
    /// RENDER_TARGET D3D 텍스처를 그리기용 EGL pbuffer로 래핑. 반환값=EGLSurface.
    fn create_render_pbuffer_from_d3d_texture(&self, _texture: usize, _size: Size2D<i32>) -> Option<usize> {
        None
    }
    /// pbuffer를 현재 draw/read 서피스로 바인딩(컨텍스트 유지). 성공 여부 반환.
    fn make_render_pbuffer_current(&self, _egl_surface: usize) -> bool {
        false
    }
    /// [`RenderingContext::create_render_pbuffer_from_d3d_texture`]의 짝.
    fn destroy_render_pbuffer(&self, _egl_surface: usize) {}
```

- [ ] **Step 2: Surfman 실구현 + 위임 지점**

§3-q에서 확립한 패턴 그대로 — `map_d3d11_dynamic_texture`의 실구현(:550-568)이 있는 impl 블록에 실제 구현을 추가하고(디바이스/컨텍스트 borrow 방식은 그 함수와 동일하게), `:820`, `:1091`, `:1463` 부근의 위임 impl 3곳에 동일 시그니처 위임을 추가한다. 예 — Surfman 실구현부:

```rust
    fn angle_d3d11_device_ptr(&self) -> Option<usize> {
        // map_d3d11_dynamic_texture(:550)와 동일한 디바이스 접근 패턴을 따른다.
        let device = self.device.borrow();
        let ptr = device.d3d11_device_ptr();
        if ptr.is_null() { None } else { Some(ptr as usize) }
    }

    fn create_render_pbuffer_from_d3d_texture(&self, texture: usize, size: Size2D<i32>) -> Option<usize> {
        let device = self.device.borrow();
        let context = self.context.borrow();
        // 안전성: texture는 호출자가 유효한 RENDER_TARGET ID3D11Texture2D를 보장
        // (DComp BeginDraw 반환값). surfman은 포인터를 저장하지 않는다.
        unsafe {
            device
                .create_render_pbuffer_from_d3d_texture(&context, texture as *mut _, size)
                .map(|s| s as usize)
        }
    }

    fn make_render_pbuffer_current(&self, egl_surface: usize) -> bool {
        let device = self.device.borrow();
        let context = self.context.borrow();
        unsafe { device.make_render_pbuffer_current(&context, egl_surface as EGLSurface) }
    }

    fn destroy_render_pbuffer(&self, egl_surface: usize) {
        let device = self.device.borrow();
        unsafe { device.destroy_render_pbuffer(egl_surface as EGLSurface) }
    }
```

(실제 필드명/borrow 형태는 :550-575 기존 코드와 반드시 일치시킬 것 — RefCell 여부 등은 그 코드가 정본.)

- [ ] **Step 3: WindowRenderingContext에 hwnd 보관 + window_hwnd 구현**

`WindowRenderingContext::new` 계열(:874-)에서 `window_handle: WindowHandle`로부터 추출해 구조체 필드에 보관:

```rust
        // connection.rs:193과 동일 패턴으로 Win32 HWND를 추출해 보관한다.
        #[cfg(windows)]
        let win32_hwnd: Option<usize> = match window_handle.as_raw() {
            raw_window_handle::RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as usize),
            _ => None,
        };
```

구조체(:867)에 `#[cfg(windows)] win32_hwnd: Option<usize>` 필드 추가, `RenderingContext for WindowRenderingContext` impl(:776 부근)에:

```rust
    #[cfg(windows)]
    fn window_hwnd(&self) -> Option<usize> {
        self.win32_hwnd
    }
```

나머지 4종은 내부 surfman 컨텍스트로 위임(:820 위임 블록과 같은 스타일). Offscreen/Software 컨텍스트는 trait 기본값(None/false/no-op) 그대로 둔다.

- [ ] **Step 4: 컴파일 확인**

Run: `cargo check -p servo-paint-api --features no-wgl 2>&1 | Select-Object -Last 5`
Expected: `Finished` (에러 0)

- [ ] **Step 5: Commit**

```
git add components/shared/paint/rendering_context.rs
git commit -m "paint-api: 네이티브 컴포지터 인터롭 5종 + 창 HWND 노출

DComp 컴포지터가 쓸 ANGLE 디바이스 포인터/pbuffer 래핑/현재화와
WindowRenderingContext의 Win32 HWND 접근자. 기본 구현은 전부
no-op(None/false)라 비Windows/오프스크린 경로 무영향."
```

---

### Task 3: PoC 게이트 — DComp 가상 서피스 + pbuffer 렌더 검증 (★HALT 게이트★)

**Files:**
- Create: `components/shared/paint/examples/dcomp_native_poc.rs`
- Modify: `components/shared/paint/Cargo.toml` (dev용 winapi features)

**Interfaces:**
- Consumes: Task 1·2의 인터롭 전부.
- Produces: 없음(검증 전용). PASS 4/4가 Task 4 진행 조건.

**게이트 4개** (스펙 §8-1):
- G1: ANGLE D3D11 디바이스로부터 `DCompositionCreateDevice` + `CreateTargetForHwnd` 성공
- G2: `IDCompositionVirtualSurface::BeginDraw` 텍스처의 pbuffer 래핑 성공 (핵심 리스크)
- G3: GL로 그린 내용이 화면에 표시됨 (Commit 경로 성립)
- G4: 방향 정합 — 좌상단에 그린 색이 화면 좌상단에 보임 (top-left 원점)

- [ ] **Step 1: winapi dev 피처 추가**

`components/shared/paint/Cargo.toml`의 `[target.'cfg(windows)'.dependencies]` winapi features에 다음을 보충(기존 목록 유지 + 추가): `"dcomp"`, `"dcomptypes"`, `"dxgi"`, `"d3d11"`, `"winerror"`, `"wingdi"`, `"winuser"`. (d3d11_dyn_poc가 이미 쓰는 피처는 그대로.)

- [ ] **Step 2: PoC 작성**

`d3d11_dyn_poc.rs`의 보일러플레이트(Win32 창 생성, surfman Connection/Device/Context 초기화, `--features no-wgl` 전제)를 복사해 시작하고, 본체를 다음 순서로 구성한다. 창 크기 512×512 고정.

```rust
//! DComp Native Compositor PoC 게이트 (스펙 §8-1).
//! G1 DComp 디바이스/타깃, G2 BeginDraw 텍스처 pbuffer 래핑,
//! G3 화면 표시, G4 top-left 방향. 각 게이트 PASS/FAIL 출력, 전부 PASS면 exit 0.

// (d3d11_dyn_poc.rs와 동일한 창/디바이스 초기화 후:)

const SIZE: i32 = 512;

unsafe fn run_poc(rc: &WindowRenderingContext, hwnd: HWND) -> Result<(), String> {
    // --- G1: DComp 디바이스 + 타깃 + 루트 비주얼 ---
    let d3d = rc.angle_d3d11_device_ptr().ok_or("no d3d11 device ptr")? as *mut ID3D11Device;
    let mut dxgi: *mut IDXGIDevice = ptr::null_mut();
    hr_check((*d3d).QueryInterface(&IDXGIDevice::uuidof(), &mut dxgi as *mut _ as *mut _), "QI IDXGIDevice")?;
    let mut dcomp: *mut IDCompositionDevice = ptr::null_mut();
    hr_check(DCompositionCreateDevice(dxgi, &IDCompositionDevice::uuidof(), &mut dcomp as *mut _ as *mut _), "DCompositionCreateDevice")?;
    let mut target: *mut IDCompositionTarget = ptr::null_mut();
    hr_check((*dcomp).CreateTargetForHwnd(hwnd, TRUE, &mut target), "CreateTargetForHwnd")?;
    let mut root: *mut IDCompositionVisual = ptr::null_mut();
    hr_check((*dcomp).CreateVisual(&mut root), "CreateVisual(root)")?;
    hr_check((*target).SetRoot(root), "SetRoot")?;
    println!("G1 PASS: DComp device/target/root visual");

    // --- 가상 서피스 + 콘텐츠 비주얼 ---
    let mut vsurf: *mut IDCompositionVirtualSurface = ptr::null_mut();
    hr_check((*dcomp).CreateVirtualSurface(SIZE as u32, SIZE as u32,
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ALPHA_MODE_IGNORE, &mut vsurf), "CreateVirtualSurface")?;
    let mut visual: *mut IDCompositionVisual = ptr::null_mut();
    hr_check((*dcomp).CreateVisual(&mut visual), "CreateVisual(content)")?;
    hr_check((*visual).SetContent(vsurf as *mut IUnknown), "SetContent")?;
    hr_check((*root).AddVisual(visual, TRUE, ptr::null_mut()), "AddVisual")?;

    // --- G2: BeginDraw → 텍스처 → pbuffer 래핑 ---
    let update = RECT { left: 0, top: 0, right: SIZE, bottom: SIZE };
    let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
    let mut offset = POINT { x: 0, y: 0 };
    hr_check((*vsurf).BeginDraw(&update, &ID3D11Texture2D::uuidof(),
        &mut tex as *mut _ as *mut _, &mut offset), "BeginDraw")?;
    // BeginDraw 텍스처의 실제 크기(아틀라스일 수 있음)로 pbuffer를 만든다.
    let mut desc: D3D11_TEXTURE2D_DESC = mem::zeroed();
    (*tex).GetDesc(&mut desc);
    let pbuf = rc.create_render_pbuffer_from_d3d_texture(
        tex as usize, Size2D::new(desc.Width as i32, desc.Height as i32))
        .ok_or("pbuffer wrap failed")?;
    println!("G2 PASS: BeginDraw texture wrapped as pbuffer (offset=({},{}), tex={}x{})",
        offset.x, offset.y, desc.Width, desc.Height);

    // --- 4분면 렌더 (좌상 R, 우상 G, 좌하 B, 우하 W; top-left 좌표 기준) ---
    if !rc.make_render_pbuffer_current(pbuf) { return Err("make current failed".into()); }
    let gl = /* d3d11_dyn_poc와 동일한 gleam GL 함수 획득 */;
    let half = SIZE / 2;
    // GL 시저는 bottom-left 원점 → top-left 논리 좌표를 y-flip해 지정한다.
    // (offset은 update rect 원점의 텍스처 내 위치)
    let quads: [(i32, i32, f32, f32, f32); 4] = [
        (0, 0, 1.0, 0.0, 0.0),      // 논리 좌상 = red
        (half, 0, 0.0, 1.0, 0.0),   // 논리 우상 = green
        (0, half, 0.0, 0.0, 1.0),   // 논리 좌하 = blue
        (half, half, 1.0, 1.0, 1.0),// 논리 우하 = white
    ];
    gl.enable(gleam::gl::SCISSOR_TEST);
    for (lx, ly, r, g, b) in quads {
        // 텍스처는 top-left 행 순서(D3D). WR도 NativeSurface를 top-left로 취급하므로
        // PoC는 GL 좌표를 y-flip해 top-left 논리 좌표로 그린다.
        let gl_y = desc.Height as i32 - (offset.y + ly + half);
        gl.scissor(offset.x + lx, gl_y, half, half);
        gl.clear_color(r, g, b, 1.0);
        gl.clear(gleam::gl::COLOR_BUFFER_BIT);
    }
    gl.disable(gleam::gl::SCISSOR_TEST);
    gl.finish();
    hr_check((*vsurf).EndDraw(), "EndDraw")?;
    rc.destroy_render_pbuffer(pbuf);

    // --- G3/G4: Commit + 화면 픽셀 판독 ---
    hr_check((*dcomp).Commit(), "Commit")?;
    // DWM 반영 대기 후 클라이언트 영역 4분면 중앙 픽셀을 GetPixel로 판독.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let mut origin = POINT { x: 0, y: 0 };
    ClientToScreen(hwnd, &mut origin);
    let dc = GetDC(ptr::null_mut());
    let sample = |sx: i32, sy: i32| -> u32 {
        GetPixel(dc, origin.x + sx, origin.y + sy)
    };
    let q = half / 2;
    let (tl, tr, bl, br) = (sample(q, q), sample(half + q, q), sample(q, half + q), sample(half + q, half + q));
    ReleaseDC(ptr::null_mut(), dc);
    // COLORREF = 0x00BBGGRR
    let expect = [(tl, 0x0000_00FFu32, "TL=red"), (tr, 0x0000_FF00, "TR=green"),
                  (bl, 0x00FF_0000, "BL=blue"), (br, 0x00FF_FFFF, "BR=white")];
    let mut display_ok = false;
    let mut orient_ok = true;
    for (got, want, label) in expect {
        let pass = colors_close(got, want);
        if pass { display_ok = true; } else { orient_ok = false; }
        println!("  pixel {label}: got=0x{got:06x} want=0x{want:06x} {}", if pass {"OK"} else {"MISMATCH"});
    }
    if !display_ok { return Err("G3 FAIL: nothing composited to screen".into()); }
    println!("G3 PASS: DComp commit visible on screen");
    if !orient_ok { return Err("G4 FAIL: orientation mismatch (check y-flip assumptions)".into()); }
    println!("G4 PASS: top-left orientation confirmed");
    Ok(())
}
```

보조 `hr_check(hr, label) -> Result<(), String>`(FAILED면 Err), `colors_close`(채널당 ±16 허용 — DWM 컬러 변환 여유)를 포함할 것. COM 해제는 종료 직전 명시 Release(§3-q PoC처럼 `std::process::exit` 사용 시 누수 무방하나 G1-G4 판정 후 Release 시도). **winapi 메서드명(오버로드 접미사 `_1`/`_2` 등)은 로컬 소스로 확정할 것:**

Run: `Select-String -Path "C:\Users\ilwoonam75\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\winapi-0.3.9\src\um\dcomp.rs" -Pattern "fn SetContent|fn AddVisual|fn SetRoot|fn BeginDraw|fn EndDraw|fn Commit|fn SetOffsetX|fn SetClip"`

- [ ] **Step 3: PoC 실행 (게이트 판정)**

Run: `cargo run --release -p servo-paint-api --example dcomp_native_poc --features no-wgl 2>&1 | Tee-Object poc_dcomp_output.txt`
Expected: `G1 PASS` ~ `G4 PASS` 4줄 전부 + exit code 0.

**★FAIL 시 HALT: 이후 태스크 진행 금지, 출력 첨부해 사용자에게 방향 문의.** (특히 G2 실패 = pbuffer 래핑 불가 → 설계 재검토 필요; G4 실패 = 방향 보정 전략 변경 필요 — 어느 쪽도 임의 우회 금지.)

- [ ] **Step 4: Commit**

```
git add components/shared/paint/examples/dcomp_native_poc.rs components/shared/paint/Cargo.toml
git commit -m "PoC: DComp 가상 서피스 + ANGLE pbuffer 렌더 게이트 4종

G1 디바이스/타깃, G2 BeginDraw 텍스처 pbuffer 래핑, G3 Commit 표시,
G4 top-left 방향. RTX A5000 실기 4/4 PASS 확인."
```

---

### Task 4: DCompNativeCompositor 모듈

**Files:**
- Create: `components/paint/dcomp_compositor.rs`
- Modify: `components/paint/lib.rs` (모듈 등록: `#[cfg(windows)] mod dcomp_compositor;`)
- Modify: `components/paint/Cargo.toml` (winapi features에 `"dcomp"`, `"dcomptypes"`, `"dxgi"`, `"d3d11"`, `"winerror"`, `"d2dbasetypes"`, `"unknwnbase"` 추가)

**Interfaces:**
- Consumes: Task 2 인터롭 5종 (`Rc<dyn RenderingContext>` 경유).
- Produces (Task 5가 사용):
  - `pub fn enabled() -> bool` — 게이트 판정
  - `pub fn maybe_create(rendering_context: &Rc<dyn RenderingContext>) -> Option<DCompNativeCompositor>` — 실패 시 None + warn (Draw 폴백 신호)
  - `pub struct DCompNativeCompositor` — `webrender::Compositor` 구현체

- [ ] **Step 1: 모듈 골격 + 순수 로직 (타일 가상좌표 계산) + 단위 테스트 작성**

```rust
//! WR Native Compositor의 DirectComposition 구현 (스펙 2026-07-13).
//! 창(painter)당 인스턴스 1개. WR이 picture cache 타일을 이 모듈이 만든
//! DComp 가상 서피스에 직접 그리고, DWM이 화면에 합성한다(②단 draw 소멸).
#![allow(unsafe_code)]

use webrender::{Compositor, Device};
use webrender::api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};
// (composite.rs 타입들: NativeSurfaceId, NativeTileId, NativeSurfaceInfo,
//  CompositorCapabilities, CompositorSurfaceTransform, ClipRadius, WindowVisibility
//  — webrender re-export 경로는 lib.rs pub use 목록에서 확인해 import)

/// Gecko DCLayerTree 관례와 동일한 가상 표면 크기. WR은 타일 그리드를 이
/// 가상공간 중심(vss/2) 부근에 배치한다(picture.rs:2477).
const VIRTUAL_SURFACE_SIZE: i32 = 1024 * 32;

/// 타일 (x,y) 그리드 좌표 → 가상 서피스 내 픽셀 rect.
/// virtual_offset은 create_surface 때 WR이 준 이 서피스의 가상공간 원점.
fn tile_virtual_rect(
    virtual_offset: DeviceIntPoint,
    tile_size: DeviceIntSize,
    tile_x: i32,
    tile_y: i32,
) -> DeviceIntRect {
    let origin = DeviceIntPoint::new(
        virtual_offset.x + tile_x * tile_size.width,
        virtual_offset.y + tile_y * tile_size.height,
    );
    DeviceIntRect::from_origin_and_size(origin, tile_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_virtual_rect_positions_tiles_on_grid() {
        let vo = DeviceIntPoint::new(16384, 16384);
        let ts = DeviceIntSize::new(1024, 512);
        let r = tile_virtual_rect(vo, ts, 0, 0);
        assert_eq!(r.min, vo);
        let r = tile_virtual_rect(vo, ts, 2, -1);
        assert_eq!(r.min, DeviceIntPoint::new(16384 + 2048, 16384 - 512));
        assert_eq!(r.size(), ts);
    }
}
```

- [ ] **Step 2: 단위 테스트 실행 (RED→GREEN 확인)**

Run: `$env:PKG_CONFIG_PATH="C:/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"; cargo test -p servo-paint --lib dcomp 2>&1 | Select-Object -Last 5`
Expected: `test result: ok. 1 passed` (paint 크레이트 test가 이 머신에서 불가로 판명되면 — 행/의존 실패 — 테스트는 유지한 채 게이트를 Task 5의 mach build로 대체하고 그 사실을 진행 원장에 기록)

- [ ] **Step 3: COM 소유 래퍼 + 본체 구현**

같은 파일에 이어서 (전체 구조 — HRESULT 체크·필드는 그대로 구현할 것):

```rust
/// 소유한 COM 포인터의 RAII 래퍼(Drop에서 Release). Send/Sync 아님 —
/// 렌더러 스레드 전용(WR Compositor 계약과 일치).
struct ComOwned<T>(std::ptr::NonNull<T>);
impl<T> ComOwned<T> {
    /// 안전성: ptr은 소유권이 이전되는 유효한 COM 인터페이스 포인터여야 한다.
    unsafe fn from_raw(ptr: *mut T) -> Option<Self> { std::ptr::NonNull::new(ptr).map(Self) }
    fn as_ptr(&self) -> *mut T { self.0.as_ptr() }
}
impl<T> Drop for ComOwned<T> {
    fn drop(&mut self) {
        // 안전성: from_raw 계약에 의해 유효한 COM 포인터를 소유 중.
        unsafe { (*(self.0.as_ptr() as *mut winapi::um::unknwnbase::IUnknown)).Release(); }
    }
}

struct SurfaceEntry {
    virtual_surface: ComOwned<IDCompositionVirtualSurface>,
    visual: ComOwned<IDCompositionVisual>,
    virtual_offset: DeviceIntPoint,
    tile_size: DeviceIntSize,
    is_opaque: bool,
}

pub struct DCompNativeCompositor {
    rendering_context: Rc<dyn RenderingContext>,
    dcomp_device: ComOwned<IDCompositionDevice>,
    _target: ComOwned<IDCompositionTarget>, // HWND 귀속, 수명 유지용
    root_visual: ComOwned<IDCompositionVisual>,
    surfaces: HashMap<NativeSurfaceId, SurfaceEntry>,
    /// bind 중인 pbuffer (unbind에서 해제)
    bound_pbuffer: Option<usize>,
    warned_rounded_clip: bool,
    warned_external_surface: bool,
}

pub fn enabled() -> bool {
    std::env::var("SERVO_COMPOSITOR_DCOMP").is_ok_and(|v| {
        v == "1" || v.eq_ignore_ascii_case("true")
            || v.eq_ignore_ascii_case("yes") || v.eq_ignore_ascii_case("on")
    })
}

pub fn maybe_create(rendering_context: &Rc<dyn RenderingContext>) -> Option<DCompNativeCompositor> {
    let hwnd = rendering_context.window_hwnd().or_else(|| { log::warn!("[dcomp-native] no HWND; falling back to Draw"); None })?;
    let d3d = rendering_context.angle_d3d11_device_ptr().or_else(|| { log::warn!("[dcomp-native] no ANGLE D3D11 device; falling back to Draw"); None })?;
    // 안전성: d3d는 렌더링 컨텍스트가 보유한 살아있는 디바이스(위 계약).
    unsafe {
        // QI IDXGIDevice → DCompositionCreateDevice → CreateTargetForHwnd(topmost=TRUE)
        // → CreateVisual(root) → SetRoot. 각 HRESULT FAILED면 warn + None.
        // (PoC Task 3의 G1 시퀀스와 동일 — 그 코드를 정본으로 이식)
    }
}
```

- [ ] **Step 4: Compositor trait 구현**

핵심 메서드 요지 (전부 HRESULT 체크, unwrap 금지):

```rust
impl Compositor for DCompNativeCompositor {
    fn create_surface(&mut self, _device: &mut Device, id: NativeSurfaceId,
        virtual_offset: DeviceIntPoint, tile_size: DeviceIntSize, is_opaque: bool) {
        // CreateVirtualSurface(VIRTUAL_SURFACE_SIZE², B8G8R8A8,
        //   is_opaque ? DXGI_ALPHA_MODE_IGNORE : DXGI_ALPHA_MODE_PREMULTIPLIED)
        // + CreateVisual + SetContent(vsurf). surfaces.insert(id, entry).
    }

    fn create_tile(&mut self, _device: &mut Device, _id: NativeTileId) {
        // 가상 서피스는 BeginDraw 시 지연 할당 — 부기 불요, no-op.
    }
    fn destroy_tile(&mut self, _device: &mut Device, _id: NativeTileId) {
        // no-op (Trim 최적화는 후속: 고정 크기 월 창에서는 타일 집합이 안정적).
    }

    fn bind(&mut self, _device: &mut Device, id: NativeTileId,
        dirty_rect: DeviceIntRect, _valid_rect: DeviceIntRect) -> NativeSurfaceInfo {
        let fail = NativeSurfaceInfo { origin: DeviceIntPoint::zero(), fbo_id: 0 };
        let Some(entry) = self.surfaces.get(&id.surface_id) else { return fail; };
        let tile_rect = tile_virtual_rect(entry.virtual_offset, entry.tile_size, id.x, id.y);
        // BeginDraw는 가상공간 절대좌표 RECT를 받는다: 타일 rect ∩ dirty(타일-로컬) 오프셋.
        let update = RECT {
            left: tile_rect.min.x + dirty_rect.min.x,
            top: tile_rect.min.y + dirty_rect.min.y,
            right: tile_rect.min.x + dirty_rect.max.x,
            bottom: tile_rect.min.y + dirty_rect.max.y,
        };
        // BeginDraw → tex + update_offset. GetDesc로 실크기 획득 →
        // create_render_pbuffer_from_d3d_texture → make_render_pbuffer_current.
        // WR 타일-로컬 좌표계가 성립하도록 origin = update_offset - dirty_rect.min
        // (Gecko DCLayerTree와 동일 보정).
        // 실패 시: EndDraw 시도 후 fail 반환(해당 프레임 타일 포기 — 스펙 §7).
        // 성공 시: self.bound_pbuffer = Some(pbuf);
        //   NativeSurfaceInfo { origin, fbo_id: 0 }
    }

    fn unbind(&mut self, _device: &mut Device) {
        // 현재 bind 중 서피스 EndDraw + destroy_render_pbuffer(bound_pbuffer.take())
        // (EGL이 현재 서피스 파괴를 유예하므로 안전 — Task 1 주석 참조)
    }

    fn begin_frame(&mut self, _device: &mut Device) {
        // root_visual.RemoveAllVisuals() — 매 프레임 z-order 재구성(트레이트 계약)
    }

    fn add_surface(&mut self, _device: &mut Device, id: NativeSurfaceId,
        transform: CompositorSurfaceTransform, clip_rect: DeviceIntRect,
        _image_rendering: ImageRendering, rounded_clip_rect: DeviceIntRect,
        rounded_clip_radii: ClipRadius) {
        // visual.SetOffsetX/Y(transform.offset - virtual_offset) —
        //   가상공간에 그린 콘텐츠를 창 device 좌표로 배치(Gecko 동일 보정).
        // transform.scale != 1.0이면 warn-once(월 시나리오 미발생 예상, 스펙 §비범위).
        // SetClip(D2D_RECT_F = clip_rect를 비주얼-로컬로 변환).
        // rounded_clip_radii != ClipRadius::EMPTY면 warn-once(rect 클립만 적용).
        // root.AddVisual(visual, TRUE, null) — 호출 순서 = z-order (아래→위).
    }

    fn end_frame(&mut self, _device: &mut Device) {
        // dcomp_device.Commit() — DWM 반영 요청(비동기)
    }

    fn destroy_surface(&mut self, _device: &mut Device, id: NativeSurfaceId) {
        // surfaces.remove(&id) — ComOwned Drop이 Release
    }

    fn create_external_surface(&mut self, ...) { /* warn-once: unreachable (Servo는 prefer_compositor_surface 미설정) */ }
    fn attach_external_image(&mut self, ...) { /* warn-once 동일 */ }
    fn create_backdrop_surface(&mut self, ...) { /* warn-once 동일 */ }

    fn enable_native_compositor(&mut self, _device: &mut Device, enable: bool) {
        // 디버그 커맨드 전용 경로(렌더러 mod.rs:1619) — Servo는 발행 안 함. warn-once.
    }

    fn get_capabilities(&self, _device: &mut Device) -> CompositorCapabilities {
        CompositorCapabilities {
            virtual_surface_size: VIRTUAL_SURFACE_SIZE,
            ..CompositorCapabilities::default()   // max_update_rects=1 등 기본값 유지
        }
    }

    fn get_window_visibility(&self, _device: &mut Device) -> WindowVisibility {
        WindowVisibility::default()
    }

    fn deinit(&mut self, _device: &mut Device) {
        // WR renderer deinit 내부(= egl.Terminate 이전)에 호출됨 — 여기서 전부 해제.
        // 순서: surfaces.clear() → root/target/device는 struct Drop에 위임해도
        // deinit 시점 명시 해제가 회귀 가드 겸함(§3-q UAF 교훈): mem::take/drop 명시.
    }
}
```

- [ ] **Step 5: 컴파일 + 테스트**

Run: `$env:PKG_CONFIG_PATH="C:/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"; cargo check -p servo-paint 2>&1 | Select-Object -Last 5`
Expected: 에러 0. (불가 판명 시 mach build로 대체 — Task 5에서 어차피 수행)

- [ ] **Step 6: Commit**

```
git add components/paint/dcomp_compositor.rs components/paint/lib.rs components/paint/Cargo.toml
git commit -m "paint: DCompNativeCompositor 구현 (WR Compositor trait)

창당 인스턴스. 서피스=DComp 가상서피스+비주얼, bind=BeginDraw 텍스처
pbuffer 래핑 후 fbo 0, end_frame=Commit. 외부서피스/백드롭은 warn stub
(Servo는 prefer_compositor_surface 미설정 - grep 확정). 실패는 전부
로그+프레임 포기(패닉 금지)."
```

---

### Task 5: painter 통합 (게이트 → compositor_config → 복원) + 첫 표시 스모크

**Files:**
- Modify: `components/paint/painter.rs` (:361 옵션 구성부, :640-672 렌더 블록)

**Interfaces:**
- Consumes: Task 4 `dcomp_compositor::{enabled, maybe_create}`.
- Produces: env `SERVO_COMPOSITOR_DCOMP=1` 동작 경로 + 로그 마커 `[dcomp-native] engaged` (Task 6·7 검증/런처가 사용).

- [ ] **Step 1: 렌더러 생성부에 게이트 결선**

`create_webrender_instance` 호출 직전(:361 위)에:

```rust
        // Native Compositor(DComp) 게이트: on이면 WR이 타일을 DComp 서피스에 직접
        // 그리고 DWM이 합성한다(②단 draw 소멸 — 스펙 2026-07-13). 실패 시 Draw 폴백.
        #[cfg(windows)]
        let compositor_config = if crate::dcomp_compositor::enabled() {
            match crate::dcomp_compositor::maybe_create(&rendering_context) {
                Some(compositor) => {
                    log::info!("[dcomp-native] engaged: WR native compositor (DirectComposition)");
                    webrender::CompositorConfig::Native { compositor: Box::new(compositor) }
                },
                None => {
                    log::warn!("[dcomp-native] init failed; falling back to Draw compositor");
                    webrender::CompositorConfig::default()
                },
            }
        } else {
            webrender::CompositorConfig::default()
        };
        #[cfg(not(windows))]
        let compositor_config = webrender::CompositorConfig::default();
        #[cfg(windows)]
        let dcomp_native_active = matches!(compositor_config, webrender::CompositorConfig::Native { .. });
```

`WebRenderOptions { ... }` 리터럴(:364-)에 `compositor_config,` 필드 추가. painter 구조체에 `#[cfg(windows)] dcomp_native_active: bool` 저장(초기화 지점은 다른 필드와 동일 위치).

- [ ] **Step 2: 렌더 후 창 서피스 복원**

renderer.render() 직후(:669 부근, ANGLE_GL_LOCK 블록 내부):

```rust
                    // Native Compositor는 bind가 pbuffer를 current로 바꿔 놓는다.
                    // 다음 GL 사용자(clear_background/egui/스크린샷)를 위해 창 서피스 복원.
                    #[cfg(windows)]
                    if self.dcomp_native_active {
                        if let Err(error) = self.rendering_context.make_current() {
                            log::warn!("[dcomp-native] restore make_current failed: {error:?}");
                        }
                    }
```

- [ ] **Step 3: 워크스페이스 빌드**

Run (servo 루트): `. .\etc\multigpu\servo_env.ps1; $ErrorActionPreference='Continue'; .\mach build --release 2>&1 | Tee-Object build_dcomp_task5.log | Select-Object -Last 3`
Expected: `Finished` / EXIT 0

- [ ] **Step 4: 게이트 off 무회귀 스모크**

Run: 기존 방식으로 servoshell 실행(아무 페이지). Expected: 표시 정상 + 로그에 `[dcomp-native]` 마커 **없음**.

- [ ] **Step 5: 게이트 on 첫 표시 스모크 (정적 페이지)**

Run: `$env:SERVO_COMPOSITOR_DCOMP="1"; .\target\release\servoshell.exe --window-size 1280x720 "data:text/html,<body style='margin:0'><div style='position:fixed;left:0;top:0;width:50vw;height:50vh;background:red'></div><div style='position:fixed;right:0;top:0;width:50vw;height:50vh;background:lime'></div><div style='position:fixed;left:0;bottom:0;width:50vw;height:50vh;background:blue'></div><div style='position:fixed;right:0;bottom:0;width:50vw;height:50vh;background:white'></div>"`
Expected: ①로그 `[dcomp-native] engaged` ②4분면 색이 **좌상 red / 우상 lime / 좌하 blue / 우하 white** (방향 정합) ③리사이즈 시 추종(서피스 재구성) ④종료 시 크래시/AV 없음. 픽셀 판정은 CopyFromScreen 샘플링(§3-m 확립 기법)으로 증거 캡처.

문제 발견 시 이 태스크 안에서 수정(예상 조정 지점: add_surface 오프셋 부호, BeginDraw rect, 클립 좌표계). **방향 반전이 나타나면 임의 flip 핵 금지 — 원인(어느 단계 좌표계인지) 규명 후 수정.**

- [ ] **Step 6: Commit**

```
git add components/paint/painter.rs
git commit -m "paint: SERVO_COMPOSITOR_DCOMP 게이트로 WR Native Compositor 결선

게이트 on이면 CompositorConfig::Native(DComp), 실패/off면 Draw(현행
바이트 동일). 렌더 직후 창 서피스 make_current 복원. 마커
[dcomp-native] engaged로 발동 확인 가능."
```

---

### Task 6: 통합 검증 (기능 무결 + ②단 소멸 계측 + 무회귀)

**Files:**
- Create: `.superpowers/sdd/evidence/` 아래 캡처/로그 (커밋 대상 아님 — 원장 기록용)
- 필요 시 수정: Task 4·5 파일 (발견 결함 근본수정)

**Interfaces:**
- Consumes: Task 5까지의 전체 경로 + 기존 도구(run_video_wall_d3d11.ps1, WR 프로파일러 Ctrl+F12 WrProfCap 기법, PresentMon D:\PresentMon-2.3.1-x64.exe, blacktile/CopyFromScreen 샘플링).
- Produces: 스펙 §9 판정 결과 전체 (Task 7 문서화의 입력).

각 항목 순서대로, 실패는 근본수정 후 재검증 (수정 커밋은 항목별로):

- [ ] **6-1 비디오 2×2 (게이트 on + 표출 레시피)**: `etc/multigpu/run_video_wall_d3d11.ps1 -Cols 2 -Rows 2` 실행 전 `$env:SERVO_COMPOSITOR_DCOMP="1"` 주입(런처 스위치는 Task 7). Expected: 색/방향/모션 정상(스크린샷 판독), d3d11 마커 4/4, `[dcomp-native] engaged`.
- [ ] **6-2 45타일**: `-Cols 9 -Rows 5 -Sync -1`. Expected: 45/45 재생·lockstep ±1프레임(영상 내장 프레임카운터)·루프 경계 무결·블랙타일 0(픽셀 샘플링)·5분 메모리 플랫(WS 관찰).
- [ ] **6-3 ②단 소멸 계측 (존재 증명)**: 동일 45타일 게이트 on/off × WR 프로파일러(Ctrl+F12, WrProfCap 캡처): Renderer time 및 GPU time 비교 기록 + `Rendered picture tiles` 수치 유지 확인. 추가로 창 크기 2단(1280→1920) GPU%(작업관리자/typeperf) A/B — on에서 창면적 증가에 따른 GPU% 기울기 감소 방향성 기록. Expected: on에서 합성 몫 감소가 관측되고 회귀 없음. (RenderDoc은 §3-p 함정으로 판정 도구에서 제외)
- [ ] **6-4 WebGPU 월 무회귀**: 기존 WebGPU 월 페이지(게이트 on) 표시·60fps·블랙 0. (external image 경로는 콘텐츠 패스 무변경이므로 회귀 없어야 정상)
- [ ] **6-5 Ctrl+F12 오버레이**: 게이트 on에서 프로파일러 오버레이 표시 확인 (DEBUG_OVERLAY 서피스 경로).
- [ ] **6-6 PresentMon 저더**: 45타일 게이트 on 40초 캡처 — DisplayedTime 1리프레시 비율·>53ms 홀드 수가 §3-f 기준(99%+/0) 동급. (presentmon_run/parse 스크립트 재사용, servoshell 포그라운드 유지 함정 주의)
- [ ] **6-7 게이트 off 회귀**: off로 45타일 스팟 — fps/마커 기존 동일, `[dcomp-native]` 마커 부재.
- [ ] **6-8 리사이즈/창 이동/종료**: 게이트 on에서 창 드래그 리사이즈 연속, 모니터 간 이동, 정상 종료 3회 — 크래시/AV/블랙 0. (서피스 destroy/create 폭주 경로 + deinit 경로 검증)
- [ ] **6-9 검증 결과를 진행 원장(.superpowers/sdd/progress.md)에 기록 + 수정 커밋들 정리**

---

### Task 7: 런처/패키징/문서

**Files:**
- Modify: `etc/multigpu/run_video_wall_d3d11.ps1` — `-DComp` 스위치 (env 주입 + 실행 후 `[dcomp-native] engaged` 마커 검증, 기존 d3d11 마커 검증과 동일 패턴; 매 실행 시 env 초기화로 stale 차단 — §3-n run_wall 관례)
- Modify: `D:\ServoWallPackage\run_wall.ps1` — 동일 `-DComp` 스위치 (A/B: 스위치 없음=off)
- Modify: `docs/superpowers/specs/2026-07-13-wr-native-compositor-design.md` — §10 아래 "구현 결과/이탈" 절 추가 (검증 수치, 스펙과 달라진 점)
- ServoWallPackage 재패키징 (exe 교체 + zip 재생성 — §3-m 구성 그대로)

- [ ] **Step 1: 런처 2종에 -DComp 스위치 + 마커 검증 추가** (기존 `-LegacyUpload`가 있던 스위치 패턴 참조 — run_wall.ps1은 env를 매 실행 초기화)
- [ ] **Step 2: 스모크**: `run_video_wall_d3d11.ps1 -Cols 2 -Rows 2 -DComp` → 마커 2종(d3d11, dcomp-native) PASS 출력 확인
- [ ] **Step 3: ServoWallPackage 재패키징**: 새 servoshell.exe 복사 → zip 재생성 → 패키지 단독 실행 스모크(`run_wall.ps1 -DComp`). AMD 판독 가이드(창 확대 GPU% A/B 절차: ①-DComp 없이 창 1080p→전체화면 GPU% 기록 ②-DComp로 동일 ③probe와 비교)를 패키지 README 또는 run_wall 주석에 추가
- [ ] **Step 4: 스펙 §10 구현결과 절 + 커밋**

```
git add docs/superpowers/specs/2026-07-13-wr-native-compositor-design.md etc/multigpu/run_video_wall_d3d11.ps1
git commit -m "dcomp: 런처 -DComp 스위치 + 스펙 구현결과 부기

ServoWallPackage 재패키징(AMD 창확대 A/B 판독 가이드 포함)."
```

- [ ] **Step 5: 최종 whole-branch 리뷰 요청(관례) 후 사용자에게 푸시 여부 확인**

---

## Self-Review 결과 (계획 작성 후 점검)

1. **스펙 커버리지**: §5 아키텍처=Task 4·5, §6 컴포넌트=Task 1-5, §7 에러/해체=Task 4(bind 실패/deinit)+5(폴백), §8 리스크=Task 3(PoC HALT)·6-3(RenderDoc 대체 판정), §9 검증=Task 5(스모크)·6(전체)·7(패키징), §3 범위(off 무회귀)=Task 5-4·6-7. 갭 없음.
2. **플레이스홀더**: Task 4 Step 3-4의 축약 주석은 모두 "PoC Task 3 코드가 정본" 또는 winapi 로컬 소스 확인 스텝으로 해소 경로가 명시됨 — 미정의 참조 없음.
3. **타입/시그니처 일관성**: 인터롭 5종 시그니처가 Task 1(surfman) ↔ Task 2(trait) ↔ Task 4(사용부)에서 동일(usize 핸들 규약 포함). `enabled`/`maybe_create`는 Task 4 정의 ↔ Task 5 사용 일치.

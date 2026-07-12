# WR YUV 직접 샘플 (A-dyn) 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 비디오 표시 경로의 GPU 작업을 "WR 합성 draw에서 YUV plane 샘플" 하나로 줄인다 — GPU 복사 명령 0회, 변환 draw 0회, fence 0회 (스펙: `docs/superpowers/specs/2026-07-12-wr-yuv-direct-sample-design.md`).

**Architecture:** plane DYNAMIC 텍스처 링(4슬롯×2~3plane)을 **ANGLE 디바이스에** 생성(생성은 free-threaded라 플레이어 스레드 가능). gst 스트리밍 스레드는 사전 Map된 포인터에 행 memcpy만(D3D 호출 0). Map/Unmap은 렌더러 스레드(WR lock() 콜백)에서만. WR은 `EGL_ANGLE_image_d3d11_texture`로 래핑한 GL 텍스처를 기존 `push_yuv_image` 경로로 직접 샘플(1패스). GstD3D11Converter/RGBA 링/fence/플레이어별 gst 디바이스/legacy 경로는 전부 삭제.

**Tech Stack:** Rust, winapi(D3D11), surfman ANGLE(EGLImage — `CreateImageKHR`/`ImageTargetTexture2DOES` 이미 로드됨), WebRender 0.68 YUV external(TextureHandle), gstreamer-rs(appsink sysmem).

## Global Constraints

- env 게이트 `SERVO_MEDIA_D3D11_VIDEO=1` 유지 (off = 기존 raw Buffer 경로, 무변경).
- 단일 프로세스 전용 (기존과 동일 — multiprocess/force_ipc면 비활성).
- **★PoC HALT 규칙 (사용자 지시 2026-07-12)★: Task 3의 게이트 1·2 중 하나라도 FAIL이면 이후 태스크를 진행하지 말고 중단, 결과를 정리해 사용자에게 진행 방향을 문의한다.**
- 스펙 §9 비범위 준수: P010/10-bit 제외(appsink caps에서 제거 → videoconvert 강등), HW 디코드 금지, raw Buffer 경로 무변경.
- `create_texture_from_shared_handle`(pbuffer import)은 **WebGPU gpu-direct가 사용 — 삭제 금지** (`components/webgpu/canvas_context.rs:370`).
- media-thread(`components/media/media-thread`)는 `#![deny(unsafe_code)]` — unsafe는 전부 paint_api/surfman/render-d3d11에.
- 커밋 메시지 한국어, Claude 서명/Co-Authored-By 제외. `git add`는 변경 파일 경로만(`-A`/`.` 금지). **커밋 메시지에 큰따옴표 금지**(PowerShell here-string 함정).
- Rust 주석 한국어 허용(이 포크 관례), 신규 C/C++ 없음.
- 빌드: `etc/multigpu/servo_env.ps1` 소싱 후 `$ErrorActionPreference='Continue'`, `.\mach build --release` (BACKGROUND, 수분).
- cargo test/example: gstreamer 크레이트가 있는 크레이트는 `$env:PKG_CONFIG_PATH="C:/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"` 필요(§3-n 함정). 실행 시 `$env:PATH += ";C:\gstreamer\1.0\msvc_x86_64\bin"`.
- 표출 검증은 `etc/multigpu/run_video_wall_d3d11.ps1` 사용.

## 코드 앵커 (2026-07-12 탐색 확정 — 전부 실측)

**ANGLE (mozangle 0.5.5, `.servo\cargo-home\registry\src\index.crates.io-1949cf8c6b5b557f\mozangle-0.5.5\gfx\angle\checkout\src\libANGLE\`):**
- `EGL_D3D11_TEXTURE_ANGLE 0x3484` (include/EGL/eglext_angle.h:315) — **EGLImage 타깃** (client-buffer 타깃 `EGL_D3D_TEXTURE_ANGLE 0x33a3`과 다름!)
- EGLImage 경로: DisplayD3D.cpp:339 `case EGL_D3D11_TEXTURE_ANGLE` → `ExternalImageSiblingImpl11`. `initialize()`가 바인드 플래그로 능력 판정: `mIsTexturable` = `BIND_SHADER_RESOURCE && DXGI_USAGE_SHADER_INPUT`(:49-54), RTV는 `mIsRenderable`일 때만(:125) → **SRV-only(DYNAMIC) 합법**
- 포맷 검증 `getD3DTextureInfo`(Renderer11.cpp:1483-1666): R8/RG8/R16/RG16 허용(:1576-1579), **텍스처가 ANGLE 자기 디바이스 소속 필수**(:1500-1505), usage/bind 검사 없음
- pbuffer 경로는 무조건 RTV 생성(SwapChain11.cpp:313-320) → DYNAMIC 불가 — 사용 금지
- 확장 광고: Caps.cpp:1283 `EGL_ANGLE_image_d3d11_texture`

**surfman (`third_party/surfman/src/`):**
- generic/egl/ffi.rs: `CreateImageKHR`/`DestroyImageKHR`/`ImageTargetTexture2DOES` **이미 로드됨**(:55-63, ubiquitous), `EGL_NO_IMAGE_KHR`(:47). `EGL_D3D11_TEXTURE_ANGLE`(0x3484) 상수는 **없음 — 추가 필요**
- angle/surface.rs: GL 텍스처 생성·바인딩 원형 = `create_surface_texture_from_local_surface`(:477-527) — `context.gl` 사용, `temporarily_make_context_current`, tex params 4종. EGLImage용 미러 대상
- angle/surface.rs:532 `create_surface_texture_from_texture`(pbuffer 경로 — WebGPU용, 유지), :555 `create_surface_texture_from_shared_handle`(유지)
- generic/egl/surface.rs:408 `bind_egl_image_to_gl_texture(gl, egl_image)` — 타 플랫폼 선례
- angle/device.rs: `self.d3d11_device`(ComPtr<ID3D11Device>) 필드, `d3d11_adapter_luid()` 메서드 존재(surface.rs:569에서 사용). immediate context는 필드 없음 — `GetImmediateContext`로 획득

**paint_api (`components/shared/paint/rendering_context.rs`):**
- trait `RenderingContext` :35-117, default-None 패턴 (:94 `create_texture_from_shared_handle` 원형). winapi 사용 중(:460 `winapi::shared::ntdef::HANDLE`) → winapi dep 있음
- 구현 3곳 위임 패턴: SurfmanRenderingContext(:451 실구현), WindowRenderingContext(:601), OffscreenRenderingContext(:834), 그 외(:1169) — **§3-k 함정 ②: trait 위임 3곳 전부 추가할 것** (빠뜨리면 조용히 default None)
- `SurfmanRenderingContext` 필드: `device: RefCell<Device>`, `context: RefCell<Context>` (:407-410 사용례)

**media-thread (`components/media/media-thread/lib.rs`, 크레이트명 `media`, `#![deny(unsafe_code)]`):**
- `D3d11VideoFrameInfo` :123-128 {shared_handle, ring_epoch, width, height} — 교체 대상
- `D3d11VideoFrameExternalImages` :141-172 — allocate_id/update/remove/info_for/take_removed_ids (전역 static Mutex 패턴 :130-139)
- `MediaExternalImages` :422-501 — `d3d11_texture_cache`, `purge_removed_d3d11_entries` :442, `lock_d3d11` :454-500(재작성 대상), Drop :503-518, `WebRenderExternalImageApi::lock` :521
- `initialize_image_handler` 존재(rendering_context 보유; 호출처 `components/paint/painter.rs:291`)
- raw plane 선례: `RawVideoFrameExternalImages::allocate_plane_ids/update_plane/remove_plane/frame_for_plane` (htmlmediaelement.rs:463,596 사용례)

**player (`components/media/player/`, 크레이트 `servo-media-player`, lib `servo_media_player`; htmlmediaelement은 `servo_media::player::video::*`로 접근):**
- video.rs: `VideoFrameYuvFormat`(:10 I420/NV12), `VideoFrameYuvColorSpace`(:25), `VideoFrameYuvColorRange`(:32), `VideoFrameYuvData`(:45), `VideoFrameD3D11Data`(:66 — 교체), `VideoFrameData` enum(:72 — D3D11 variant 교체), `Buffer` trait(:80), `VideoFrame`(:93)
- 모듈 등록: lib.rs에 `pub mod video;` 형태 — 신규 `pub mod d3d11_ring;` 추가

**render-d3d11 (`components/media/backends/gstreamer/render-d3d11/`):**
- lib.rs: env 게이트 :45,:65-72, UploadMode :74-89(삭제), D3D11FrameBuffer :91-99, RenderD3D11::new :135-174, build_video_sink_legacy :176-218(삭제), build_frame :226-432(재작성), build_video_sink :434-460(caps 수정: :448 `P010_10LE` 제거·디바이스 주입 :453-457 삭제)
- interop.rs: SharedGstD3D11Device(:27-173 삭제), SharedTextureRing(:229-, 삭제), DynamicUploadSet(:560-, 삭제 — 행 복사 로직만 신규 모듈로 이식), profile_enabled/threshold(:176-186 유지·이동)
- ffi.rs: gstd3d11 FFI 전체 삭제
- examples/d3d11_upload_poc.rs: 삭제
- Cargo.toml: libloading·servo-media-gstreamer-render 등 정리(:16-41)

**htmlmediaelement (`components/script/dom/html/htmlmediaelement.rs`):**
- import :24-28 (`layout_api::{MediaFrame,...}`, `media::{D3d11VideoFrameExternalImages,...}`, `servo_media::player::video::*` :45-48)
- `MediaYuvExternalIds` :198-226, `wr_yuv_color_space/range` :228-241, `yuv_plane_descriptor` :243-257(NV12 plane1→RG8 :246), `external_yuv_plane_data` :259-264(Buffer 타입 — TextureHandle 변형 신설)
- `ensure_yuv_external_ids` :452-467, `ensure_d3d11_external_id` :469-476(교체), `media_frame_yuv_image` :554-572, **`render_yuv_frame` :574-734(미러 원형)**, `render_d3d11_frame` :736-849(재작성), `render()` 디스패치 :858-869+, reset()/push_delete_frame_images(:418-427 D3D11 정리 분기)
- colorimetry 매핑 선례: `components/media/backends/gstreamer/render.rs:240-246`

**layout (`components/shared/layout/lib.rs` + `components/layout/display_list/mod.rs`):**
- `MediaFrameYuvFormat`(:234)/`MediaFrameYuvImage`(:240-251 color_depth/space/range 포함)/`MediaFrame`(:254)/`image_keys()`(:262) — **무변경 재사용**
- `push_yuv_image` 소비: display_list/mod.rs:765-784 — **무변경**

**WR 0.68 (registry `webrender-0.68.0/`):** TextureHandle external은 텍스처 캐시 미경유(resource_cache.rs:162-163,:1451), YUV 배치 plane별 해석(batch.rs:2338-2374, 전 plane 동일 buffer kind 필요 — 전부 Texture2D external이라 충족). **무변경.**

**수직 플립:** 외부 이미지 uv는 Media/WebGpu/WebGl 공통 `TexelRect::new(0.0, h, w, 0.0)`(수직 플립, `components/shared/paint/lib.rs:687-746`) + §3-k에서 D3D11 텍스처에 `needs_vertical_flip` 예외 도입(커밋 e66da7a78). Task 7에서 `grep -rn "needs_vertical_flip" components/`로 현 메커니즘을 확인하고 **plane external id들이 기존 D3D11(RGBA)과 같은 예외 분기를 타게** 연결한다. 최종 판정은 Task 8 육안(영상 내장 프레임카운터).

**슬롯 상태기계 (스펙 §4.2 — 모든 태스크 공통 참조):**
```
Unmapped ──(렌더러: 초기 Map)──> Free(mapped)
Free ──(프로듀서: claim)──> Writing ──(memcpy 후 publish)──> Filled(여전히 mapped)
Filled(최신) ──(렌더러: Unmap)──> Presenting
Filled(구형, 최신에 밀림) ──(D3D 호출 없이)──> Free
Presenting(다음 소비 시) ──(렌더러: Map WRITE_DISCARD)──> Free
```
- plane 2~3개는 슬롯 인덱스 공유, 한 몸으로 전이 (프레임 원자성)
- 소비 시점 = 합성당 1회: WR은 plane 3개를 lock→(render)→unlock 하므로, **"lock 카운트 0→1 전이 시에만 소비"**로 plane 간 티어링 차단
- 프로듀서가 Free 없으면 memcpy 전에 드롭(카운터만 증가 — 숨겨진 비디오 비용 ≈0)
- 첫 프레임: 슬롯이 전부 Unmapped인 동안 CPU 스테이징(Vec) → 렌더러 첫 lock에서 전 슬롯 Map + 스테이징을 슬롯0에 복사 + 즉시 Presenting

---

### Task 1: surfman — EGLImage 래핑 + DYNAMIC Map/Unmap + 디바이스 핸들

**Files:**
- Modify: `third_party/surfman/src/platform/generic/egl/ffi.rs` (상수 1개)
- Modify: `third_party/surfman/src/platform/windows/angle/device.rs` (메서드 3개)
- Modify: `third_party/surfman/src/platform/windows/angle/surface.rs` (메서드 2개)

**Interfaces:**
- Produces (angle `Device`):
  - `pub fn d3d11_device_ptr(&self) -> *mut c_void` — ANGLE ID3D11Device 원시 포인터(AddRef 안 함; 수명=Device)
  - `pub unsafe fn map_d3d11_dynamic_texture(&self, texture: *mut c_void) -> Result<(*mut c_void, u32), Error>` — immediate context `Map(WRITE_DISCARD)`; (데이터 ptr, RowPitch) 반환
  - `pub unsafe fn unmap_d3d11_texture(&self, texture: *mut c_void)`
  - `pub unsafe fn create_gl_texture_from_d3d11_texture(&self, context: &mut Context, texture: *mut c_void) -> Result<(usize /*EGLImageKHR*/, u32 /*GL texture*/), Error>`
  - `pub unsafe fn destroy_gl_texture_and_egl_image(&self, context: &mut Context, egl_image: usize, gl_texture: u32)`
- Consumes: 기존 `EGL_EXTENSION_FUNCTIONS`, `temporarily_make_context_current`, `context.gl`.

- [ ] **Step 1: ffi.rs에 EGLImage 타깃 상수 추가**

`generic/egl/ffi.rs`의 `EGL_D3D_TEXTURE_ANGLE`(:33) 아래에:
```rust
/// EGL_ANGLE_image_d3d11_texture의 eglCreateImageKHR 타깃 (eglext_angle.h:315).
/// client-buffer 타깃 EGL_D3D_TEXTURE_ANGLE(0x33a3)과 다른 값이다.
pub const EGL_D3D11_TEXTURE_ANGLE: EGLenum = 0x3484;
```

- [ ] **Step 2: angle/device.rs 메서드 3개**

`d3d11_adapter_luid` 근처에 추가 (use에 `winapi::um::d3d11::{D3D11_MAP_WRITE_DISCARD, D3D11_MAPPED_SUBRESOURCE, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D}` 등 보강):
```rust
/// ANGLE(D3D11) 디바이스 원시 포인터. AddRef하지 않는다 — 호출자가 저장 시 AddRef 책임.
pub fn d3d11_device_ptr(&self) -> *mut c_void {
    self.d3d11_device.as_raw() as *mut c_void
}

/// DYNAMIC 텍스처를 WRITE_DISCARD로 Map. **렌더러(ANGLE GL 호출) 스레드에서만 호출**
/// — immediate context는 단일 스레드 규칙이며, 이 스레드에서는 ANGLE GL 호출과 자연
/// 직렬화된다. 반환: (데이터 포인터, RowPitch).
pub unsafe fn map_d3d11_dynamic_texture(
    &self,
    texture: *mut c_void,
) -> Result<(*mut c_void, u32), Error> {
    let mut ctx: *mut ID3D11DeviceContext = ptr::null_mut();
    self.d3d11_device.GetImmediateContext(&mut ctx);
    if ctx.is_null() {
        return Err(Error::Failed);
    }
    let ctx = ComPtr::from_raw(ctx); // Drop에서 Release
    let mut mapped: D3D11_MAPPED_SUBRESOURCE = mem::zeroed();
    let hr = ctx.Map(
        texture as *mut ID3D11Resource,
        0,
        D3D11_MAP_WRITE_DISCARD,
        0,
        &mut mapped,
    );
    if !winerror::SUCCEEDED(hr) {
        warn!("map_d3d11_dynamic_texture: Map 실패 hr={hr:#x}");
        return Err(Error::Failed);
    }
    Ok((mapped.pData, mapped.RowPitch))
}

/// Map의 짝. 렌더러 스레드 전용 (동일 근거).
pub unsafe fn unmap_d3d11_texture(&self, texture: *mut c_void) {
    let mut ctx: *mut ID3D11DeviceContext = ptr::null_mut();
    self.d3d11_device.GetImmediateContext(&mut ctx);
    if ctx.is_null() {
        return;
    }
    let ctx = ComPtr::from_raw(ctx);
    ctx.Unmap(texture as *mut ID3D11Resource, 0);
}
```
(`self.d3d11_device`의 실제 타입/형은 device.rs 정의에 맞춰 조정 — ComPtr이면 `.as_raw()`, GetImmediateContext는 `(*self.d3d11_device.as_raw()).GetImmediateContext(...)` 형태일 수 있음. 컴파일 에러 기준으로 맞출 것.)

- [ ] **Step 3: angle/surface.rs 래핑 2개**

`create_surface_texture_from_shared_handle`(:555) 아래에. `create_surface_texture_from_local_surface`(:477-527)의 GL 텍스처 생성부를 미러하되 BindTexImage 대신 EGLImage:
```rust
/// D3D11 텍스처(이 디바이스 소속, BIND_SHADER_RESOURCE)를 EGLImage로 GL 텍스처에
/// 바인딩한다 (EGL_ANGLE_image_d3d11_texture). pbuffer 경로와 달리 RTV를 만들지
/// 않으므로 DYNAMIC(SRV-only) 텍스처를 수용한다. 반환된 (EGLImage, GL 텍스처)는
/// destroy_gl_texture_and_egl_image로 파기할 것. 텍스처 수명은 호출자 책임.
pub unsafe fn create_gl_texture_from_d3d11_texture(
    &self,
    context: &mut Context,
    texture: *mut c_void,
) -> Result<(usize, u32), Error> {
    let _guard = self.temporarily_make_context_current(context)?;
    let attribs = [egl::NONE as EGLint, 0];
    let egl_image = (EGL_EXTENSION_FUNCTIONS.CreateImageKHR)(
        self.egl_display,
        egl::NO_CONTEXT,
        EGL_D3D11_TEXTURE_ANGLE,
        texture as EGLClientBuffer,
        attribs.as_ptr(),
    );
    if egl_image == EGL_NO_IMAGE_KHR {
        EGL_FUNCTIONS.with(|egl| {
            warn!("CreateImageKHR(EGL_D3D11_TEXTURE_ANGLE) 실패 err={:#x}", egl.GetError());
        });
        return Err(Error::Failed);
    }
    let gl = &context.gl;
    let previous_texture = gl.get_parameter_texture(gl::TEXTURE_BINDING_2D);
    let gl_texture = gl.create_texture().map_err(|_| Error::Failed)?;
    gl.bind_texture(gl::TEXTURE_2D, Some(gl_texture));
    (EGL_EXTENSION_FUNCTIONS.ImageTargetTexture2DOES)(gl::TEXTURE_2D, egl_image);
    gl.tex_parameter_i32(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as _);
    gl.tex_parameter_i32(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as _);
    gl.tex_parameter_i32(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as _);
    gl.tex_parameter_i32(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as _);
    let gl_err = gl.get_error();
    gl.bind_texture(gl::TEXTURE_2D, previous_texture);
    if gl_err != gl::NO_ERROR {
        warn!("ImageTargetTexture2DOES 후 GL 에러 {gl_err:#x}");
        (EGL_EXTENSION_FUNCTIONS.DestroyImageKHR)(self.egl_display, egl_image);
        gl.delete_texture(gl_texture);
        return Err(Error::Failed);
    }
    Ok((egl_image as usize, gl_texture.0.get()))
}

pub unsafe fn destroy_gl_texture_and_egl_image(
    &self,
    context: &mut Context,
    egl_image: usize,
    gl_texture: u32,
) {
    if let Ok(_guard) = self.temporarily_make_context_current(context) {
        let gl = &context.gl;
        // glow 텍스처 타입 복원은 surface.rs의 기존 delete 경로 관례를 따른다
        // (NativeTexture 재구성 — destroy_surface_texture의 gl.delete_texture 사용례 참조).
        if let Some(tex) = std::num::NonZeroU32::new(gl_texture) {
            gl.delete_texture(glow::NativeTexture(tex));
        }
        (EGL_EXTENSION_FUNCTIONS.DestroyImageKHR)(self.egl_display, egl_image as EGLImageKHR);
    }
}
```
필요 use: `crate::platform::generic::egl::ffi::{EGL_D3D11_TEXTURE_ANGLE, EGL_NO_IMAGE_KHR, EGLClientBuffer, EGLImageKHR, EGL_EXTENSION_FUNCTIONS}` (이미 일부 임포트됨 — 파일 상단 확인). glow 텍스처 타입(`gl.create_texture()` 반환형)은 :494와 동일하게.

- [ ] **Step 4: 컴파일 확인**

Run: `cargo check -p surfman` (servo_env 소싱 상태). Expected: 에러 0 (경고 허용).

- [ ] **Step 5: 커밋**

```bash
git add third_party/surfman/src/platform/generic/egl/ffi.rs third_party/surfman/src/platform/windows/angle/device.rs third_party/surfman/src/platform/windows/angle/surface.rs
git commit -m 'surfman ANGLE: EGLImage 기반 D3D11 텍스처 GL 래핑 + DYNAMIC Map/Unmap (WR YUV 직접 샘플 1단계)'
```

---

### Task 2: paint_api RenderingContext — 미디어 D3D11 인터롭 메서드

**Files:**
- Modify: `components/shared/paint/rendering_context.rs`

**Interfaces:**
- Produces (trait `RenderingContext`, 전부 default 구현 = None/no-op):
  - `fn media_d3d11_device_handle(&self) -> Option<usize>` — **AddRef된** ID3D11Device 포인터(프로세스 수명 누수 허용 — 전역 레지스트리 저장용)
  - `fn map_d3d11_dynamic_texture(&self, texture: usize) -> Option<(usize, u32)>` (데이터 ptr, RowPitch)
  - `fn unmap_d3d11_texture(&self, texture: usize)`
  - `fn wrap_d3d11_texture_as_gl_texture(&self, texture: usize) -> Option<D3d11GlWrappedTexture>`
  - `fn destroy_d3d11_gl_wrap(&self, wrap: D3d11GlWrappedTexture)`
  - `fn release_d3d11_texture(&self, texture: usize)` — IUnknown::Release (링 해체용)
  - `pub struct D3d11GlWrappedTexture { pub egl_image: usize, pub gl_texture: u32 }` (Copy/Clone/Debug)
- Consumes: Task 1의 surfman 메서드.

- [ ] **Step 1: trait에 default 메서드 + 구조체 추가** (:100 `create_texture_from_shared_handle` 아래, 동일 문서화 스타일. 전부 `_` 파라미터로 default None/no-op)

- [ ] **Step 2: SurfmanRenderingContext에 실구현** (:451 이웃, `#[cfg(all(target_os = "windows", feature = "no-wgl"))]` 게이트 동일)

```rust
fn media_d3d11_device_handle(&self) -> Option<usize> {
    #[cfg(all(target_os = "windows", feature = "no-wgl"))]
    {
        let ptr = self.device.borrow().d3d11_device_ptr();
        if ptr.is_null() {
            return None;
        }
        // 전역 레지스트리에 저장되므로 AddRef (프로세스 수명 — Release 안 함).
        unsafe {
            (*(ptr as *mut winapi::um::unknwnbase::IUnknown)).AddRef();
        }
        Some(ptr as usize)
    }
    #[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
    None
}

fn map_d3d11_dynamic_texture(&self, texture: usize) -> Option<(usize, u32)> {
    #[cfg(all(target_os = "windows", feature = "no-wgl"))]
    {
        let device = self.device.borrow();
        match unsafe { device.map_d3d11_dynamic_texture(texture as *mut _) } {
            Ok((ptr, pitch)) => Some((ptr as usize, pitch)),
            Err(_) => None,
        }
    }
    #[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
    { let _ = texture; None }
}
```
`unmap_d3d11_texture`/`release_d3d11_texture`(Release 호출)/`wrap_d3d11_texture_as_gl_texture`/`destroy_d3d11_gl_wrap`(device+context borrow 후 Task 1 메서드 위임)도 동일 패턴.

- [ ] **Step 3: 위임 3곳 추가** — WindowRenderingContext(:601 이웃), OffscreenRenderingContext(:834 이웃), :1169 이웃의 세 impl에 6개 메서드 전부 `self.<inner>.<method>(...)` 위임. **§3-k 함정 ②(dead trait 경로) 재발 방지 — 하나라도 빠지면 런타임에 조용히 None.**

- [ ] **Step 4: 컴파일** — `cargo check -p paint_api` (패키지명은 `components/shared/paint/Cargo.toml`의 `[package] name` 확인 후 사용). Expected: 에러 0.

- [ ] **Step 5: 커밋**

```bash
git add components/shared/paint/rendering_context.rs
git commit -m 'RenderingContext: 미디어 D3D11 인터롭 메서드 6종 (디바이스 핸들/Map/Unmap/EGLImage 래핑)'
```

---

### Task 3: PoC 게이트 예제 ★실패 시 HALT★

**Files:**
- Create: `components/shared/paint/examples/d3d11_dyn_poc.rs`
- Modify: `components/shared/paint/Cargo.toml` (dev-dependencies 필요 시)

**Interfaces:**
- Consumes: Task 1·2 전부.
- Produces: 게이트 판정 출력 `GATE1(R8 texturable)=PASS/FAIL`, `GATE1(RG8)=...`, `GATE2(discard-rename)=...`

**검증 내용 (스펙 §6):**
1. 게이트 1: ANGLE 디바이스에 `CreateTexture2D`(DYNAMIC+BIND_SHADER_RESOURCE+CPU_WRITE, R8 4×4 및 RG8 4×4) → `wrap_d3d11_texture_as_gl_texture` 성공 + 샘플 값 일치
2. 게이트 2: `map(WRITE_DISCARD)` → 다른 값 기록 → unmap → 재샘플 → 새 값 일치 (rename이 EGLImage SRV에 투명)

- [ ] **Step 1: 예제 작성**

구성 (단일 main, ~200줄):
```rust
// PoC: DYNAMIC D3D11 텍스처의 EGLImage 래핑 + WRITE_DISCARD rename 투명성 게이트.
// 스펙 docs/superpowers/specs/2026-07-12-wr-yuv-direct-sample-design.md §6.
// 실패 시 구현 중단 + 사용자 문의 (Global Constraints HALT 규칙).
fn main() {
    // 1. SurfmanRenderingContext 생성(오프스크린): rendering_context.rs의
    //    SoftwareRenderingContext::new(:503-527) 구성을 미러하되
    //    connection.create_adapter()(하드웨어)로. make_current().
    // 2. media_d3d11_device_handle()로 디바이스 획득.
    // 3. winapi로 CreateTexture2D: Width/Height=4, MipLevels=ArraySize=1,
    //    Format=DXGI_FORMAT_R8_UNORM, SampleDesc{1,0}, Usage=D3D11_USAGE_DYNAMIC,
    //    BindFlags=D3D11_BIND_SHADER_RESOURCE, CPUAccessFlags=D3D11_CPU_ACCESS_WRITE,
    //    MiscFlags=0.
    // 4. map_d3d11_dynamic_texture → 패턴 A(0xA5) 행 기록(RowPitch 준수) → unmap.
    // 5. wrap_d3d11_texture_as_gl_texture → 실패면 GATE1 FAIL.
    // 6. gleam으로 4×4 RGBA 텍스처+FBO 생성, 트리비얼 셰이더로 풀스크린 쿼드에
    //    샘플(vec4(texture2D(t,uv).r,0,0,1)) 렌더 → read_pixels → R==0xA5 확인.
    //    (gleam_gl_api()는 RenderingContext에서 획득. GLES2 셰이더로 작성.)
    // 7. map(DISCARD) → 패턴 B(0x5A) → unmap → 재렌더 → R==0x5A → GATE2.
    // 8. RG8로 3~7 반복 (Format=DXGI_FORMAT_R8G8_UNORM, .rg 채널 확인).
    // 9. 결과 println! 및 실패 시 exit code 1.
}
```
전체 코드는 실행자가 위 골격+rendering_context.rs의 기존 GL 사용례(Framebuffer 유틸 :400-405)를 참고해 완성한다. 셰이더/쿼드는 예제 내 상수 문자열.

- [ ] **Step 2: 실행**

Run: `cargo run --release -p <paint 패키지명> --example d3d11_dyn_poc`
Expected: `GATE1(R8)=PASS GATE1(RG8)=PASS GATE2=PASS`

- [ ] **Step 3: ★판정 분기★**

- **전부 PASS** → Step 4로.
- **하나라도 FAIL** → 예제와 현재까지 커밋만 남기고 **작업 전체 중단**. 실패 게이트·에러 코드·GetError 값을 정리해 사용자에게 보고하고 진행 방향을 문의한다. **이후 태스크 착수 금지.**

- [ ] **Step 4: 커밋**

```bash
git add components/shared/paint/examples/d3d11_dyn_poc.rs components/shared/paint/Cargo.toml
git commit -m 'PoC: DYNAMIC 텍스처 EGLImage 래핑 + WRITE_DISCARD rename 게이트 통과 확인'
```

---

### Task 4: player 크레이트 — plane 링 레지스트리(상태기계) + 새 프레임 타입

**Files:**
- Create: `components/media/player/d3d11_ring.rs`
- Modify: `components/media/player/lib.rs` (`pub mod d3d11_ring;`)
- Modify: `components/media/player/video.rs` (variant 추가)
- Modify: `components/script/dom/html/htmlmediaelement.rs` (신규 variant 스텁 arm — cargo check가 알려주는 모든 exhaustive match에)
- Test: `d3d11_ring.rs` 내 `#[cfg(test)]` (D3D 없음 — 순수 상태 전이)

**Interfaces:**
- Produces (`servo_media_player::d3d11_ring`):
```rust
pub const SLOT_COUNT: usize = 4;
pub const MAX_PLANES: usize = 3;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RingPlaneFormat { R8, Rg8 } // DXGI_FORMAT_R8_UNORM / R8G8_UNORM 대응

#[derive(Clone, Copy, Debug)]
pub struct PlaneDesc {
    pub texture: usize,      // AddRef된 ID3D11Texture2D (프로듀서 생성, 렌더러 해제)
    pub width: i32,
    pub height: i32,
    pub format: RingPlaneFormat,
    pub row_bytes: usize,    // 복사할 유효 바이트/행 (width×bpp)
}

// 전역 레지스트리 (media-thread 전역 static 패턴 :130-139 미러)
pub struct D3d11PlaneRings;
impl D3d11PlaneRings {
    // 소비자(ANGLE) 디바이스 — media-thread가 시작 시 publish
    pub fn set_consumer_device(device: usize);
    pub fn consumer_device() -> Option<usize>;

    // 프로듀서 (gst 스레드)
    pub fn create_ring(planes_per_slot: usize,
                       slots: [[Option<PlaneDesc>; MAX_PLANES]; SLOT_COUNT]) -> u64; // ring_id (epoch=1)
    pub fn remove_ring(ring_id: u64);
    /// mapped FREE 슬롯 claim. 반환된 포인터에 행 복사 후 publish_slot.
    /// FREE 없으면 None (호출자는 드롭 카운터만 — memcpy 전 드롭).
    pub fn claim_free_slot(ring_id: u64) -> Option<ClaimedSlot>; // {slot: usize, planes: [Option<MappedPlane>; MAX_PLANES]}
    pub fn publish_slot(ring_id: u64, slot: usize);
    pub fn abandon_slot(ring_id: u64, slot: usize); // claim 후 실패 시 FREE 복귀
    /// 전 슬롯 Unmapped인 초기 구간용 — 첫 프레임 CPU 스테이징 (plane별 연속 바이트)
    pub fn stage_first_frame(ring_id: u64, planes: Vec<Vec<u8>>);
    pub fn dropped_frames(ring_id: u64) -> u64;

    // 소비자 (media-thread, 렌더러 스레드에서 호출)
    /// plane lock 카운트 0→1이면 소비 계획 반환 (그 외/새 프레임 없음 = None).
    pub fn note_plane_lock_and_plan(ring_id: u64) -> Option<ConsumePlan>;
    pub fn note_plane_unlock(ring_id: u64);
    /// ConsumePlan의 D3D 작업(Unmap/Map/스테이징 복사)을 마친 뒤 상태 커밋.
    pub fn commit_consume(ring_id: u64, commit: ConsumeCommit);
    /// 현재 Presenting 슬롯의 plane 텍스처 (lock 반환용)
    pub fn presenting_plane(ring_id: u64, plane: usize) -> Option<PlaneDesc>;
    pub fn take_removed_rings() -> Vec<RemovedRing>; // {textures: Vec<usize>, mapped: Vec<usize>} — 렌더러 정리용
}

pub struct MappedPlane { pub data_ptr: usize, pub row_pitch: u32, pub rows: usize, pub row_bytes: usize }
pub enum ConsumePlan {
    /// 초기화: 전 슬롯 Map 필요 + 스테이징 복사 (plane별 desc + staged bytes)
    InitialMapAll { slots: [[Option<PlaneDesc>; MAX_PLANES]; SLOT_COUNT], staged: Option<Vec<Vec<u8>>> },
    /// 정상: newest Filled를 Unmap, prev Presenting을 Map(DISCARD)
    Advance { unmap: Vec<usize /*texture*/>, map: Vec<usize /*texture*/>, filled_slot: usize },
}
pub struct ConsumeCommit { /* Advance: map 결과 [(texture, ptr, pitch)]; Initial: 전 슬롯 map 결과 + presenting=0 */ }
```
정확한 필드는 구현 시 다듬되 **상태기계 의미(코드 앵커 절)와 API 이름은 고정** — Task 5·6이 이 이름을 사용한다.
- video.rs 추가 (기존 :66-69 아래, 기존 타입은 이 태스크에서 삭제하지 않음):
```rust
/// A-dyn 경로: plane DYNAMIC 링(레지스트리 d3d11_ring) 참조 + 표시 메타데이터.
/// 슬롯 인덱스는 싣지 않는다 — 렌더러가 레지스트리에서 최신 Filled를 소비(latest-wins).
#[derive(Clone, Copy, Debug, MallocSizeOf, PartialEq)]
pub struct VideoFrameD3D11YuvData {
    pub ring_id: u64,
    pub ring_epoch: u32,
    pub format: VideoFrameYuvFormat,
    pub color_space: VideoFrameYuvColorSpace,
    pub color_range: VideoFrameYuvColorRange,
}
```
+ `VideoFrameData::D3D11Yuv(VideoFrameD3D11YuvData)` variant + `get_d3d11_yuv_data()` 접근자(기존 :164 `get_d3d11_data` 미러). (파생 트레이트는 VideoFrameYuvFormat 등에 이미 있는 것 확인 후 필요 시 보강.)

- [ ] **Step 1: 상태기계 실패 테스트 먼저 작성** — `d3d11_ring.rs`의 `#[cfg(test)]`에 최소 6개:
  1. `claim_when_all_unmapped_returns_none_and_stage_accepts` (초기 스테이징)
  2. `initial_map_all_then_presenting_slot0` (InitialMapAll commit 후 presenting_plane 확인)
  3. `claim_fill_publish_then_first_lock_advances` (Advance plan: unmap=슬롯 텍스처, prev presenting map)
  4. `two_filled_latest_wins_older_returns_free_without_d3d` (구형 Filled → Free, unmap 목록에 없음)
  5. `second_plane_lock_same_composite_no_advance` (lock 카운트 게이트: 두 번째 lock은 plan None)
  6. `unlock_resets_gate_next_lock_advances`
  텍스처는 가짜 usize(1000+i). Run: `cargo test -p servo-media-player d3d11_ring` → 컴파일 실패(모듈 없음) 확인.
- [ ] **Step 2: 레지스트리 구현** (전역 `OnceLock<Mutex<HashMap<u64, RingState>>>`, ring_id는 `AtomicU64`; 잠금 구간엔 memcpy 없음 — 포인터만 반환)
- [ ] **Step 3: 테스트 통과 확인** — Run: `cargo test -p servo-media-player d3d11_ring` Expected: 6 passed
- [ ] **Step 4: video.rs variant + htmlmediaelement 스텁 arm** — `cargo check -p script`(또는 워크스페이스 check)가 지목하는 모든 exhaustive match에 `VideoFrameData::D3D11Yuv(..) => { warn!("D3D11Yuv 프레임: 소비자 미구현(Task 7 전) — 드롭"); return; }` 류 스텁. (media-thread/render-d3d11는 아직 참조 안 함.)
- [ ] **Step 5: 워크스페이스 check** — Run: `cargo check --workspace` (mach 환경). Expected: 에러 0
- [ ] **Step 6: 커밋**
```bash
git add components/media/player/d3d11_ring.rs components/media/player/lib.rs components/media/player/video.rs components/script/dom/html/htmlmediaelement.rs
git commit -m 'D3D11 plane 링 레지스트리(상태기계+테스트) + VideoFrameData::D3D11Yuv 타입'
```

---

### Task 5: render-d3d11 프로듀서 재작성 + 변환기/FFI/legacy 대삭제

**Files:**
- Modify: `components/media/backends/gstreamer/render-d3d11/lib.rs` (사실상 재작성)
- Create: `components/media/backends/gstreamer/render-d3d11/ring_producer.rs` (텍스처 생성 + 행 복사)
- Delete: `render-d3d11/ffi.rs`, `render-d3d11/interop.rs`, `render-d3d11/examples/d3d11_upload_poc.rs`
- Modify: `render-d3d11/Cargo.toml` (libloading 제거; winapi feature 정리), 루트 `Cargo.toml`(libloading workspace 항목은 타 사용처 없으면 제거)
- Test: `ring_producer.rs` 내 `#[cfg(test)]` (행 복사 로직 — 가짜 버퍼)

**Interfaces:**
- Consumes: `servo_media_player::d3d11_ring::*` (Task 4), `VideoFrameD3D11YuvData`.
- Produces: `RenderD3D11`(기존 trait `Render` 구현 유지 — `is_gl()=false`, `build_frame`, `build_video_sink`).

**핵심 코드 골격 (build_frame 대체):**
```rust
fn build_frame(&self, sample: gstreamer::Sample) -> Option<VideoFrame> {
    let buffer = sample.buffer()?;
    let caps = sample.caps()?;
    let info = gstreamer_video::VideoInfo::from_caps(caps).ok()?;
    let (width, height) = (info.width() as i32, info.height() as i32);
    let format = match info.format() {
        gstreamer_video::VideoFormat::I420 => VideoFrameYuvFormat::I420,
        gstreamer_video::VideoFormat::Yv12 => VideoFrameYuvFormat::I420, // 표시 계약은 I420; U/V 스왑은 아래 plane 매핑에서
        gstreamer_video::VideoFormat::Nv12 => VideoFrameYuvFormat::NV12,
        other => { log::warn!("D3D11 video: 미지원 포맷 {other:?} — 드롭"); return None; }
    };
    // colorimetry: render.rs:240-246 매핑을 이 크레이트로 복제(동일 match)
    let (color_space, color_range) = colorimetry_from_info(&info);

    let mut state = self.state.lock().unwrap();
    // caps 변경 시 링 교체: 기존 remove_ring(구 링은 렌더러가 정리) 후 새로 생성
    let ring_id = state.ensure_ring(&info, format)?; // 내부: consumer_device()로
        // CreateTexture2D ×(SLOT_COUNT×planes) — ring_producer::create_plane_textures,
        // YV12면 plane 1↔2를 스왑해 PlaneDesc를 U,V 순서(I420 계약)로 등록
    let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
    match D3d11PlaneRings::claim_free_slot(ring_id) {
        Some(claimed) => {
            ring_producer::copy_planes(&frame, &claimed); // 행 단위, RowPitch/stride 준수, YV12 스왑 반영
            D3d11PlaneRings::publish_slot(ring_id, claimed.slot);
        },
        None if state.ring_never_consumed => {
            // 초기 구간(전 슬롯 Unmapped): 첫 프레임 스테이징 1회
            D3d11PlaneRings::stage_first_frame(ring_id, ring_producer::planes_to_vecs(&frame));
        },
        None => { state.drop_count += 1; return None; }, // 배압 드롭 (memcpy 전)
    }
    VideoFrame::new(width, height, Arc::new(D3D11YuvFrameBuffer {
        data: VideoFrameD3D11YuvData { ring_id, ring_epoch: 1, format, color_space, color_range },
    }))
}
```
- `build_video_sink`: caps 목록 `["I420", "YV12", "NV12"]`(**P010_10LE 제거** — 스펙 §4.5), `pipeline.set_property("video-sink", appsink)`만 (디바이스 주입 :453-457 삭제).
- `RenderD3D11::new`: env 게이트+단일프로세스 검사 유지; gst 디바이스 생성(:157) 삭제; `D3d11PlaneRings::consumer_device()`가 아직 None이어도 생성 허용(ensure_ring 시점 재확인 — 렌더러 초기화가 선행되므로 실전에서는 항상 존재).
- `ring_producer.rs`: `create_plane_textures(device: usize, info) -> [[Option<PlaneDesc>;3];4]` (winapi CreateTexture2D, DYNAMIC+SHADER_RESOURCE+CPU_WRITE, R8/RG8, plane 치수 = I420: Y w×h·U/V ⌈w/2⌉×⌈h/2⌉, NV12: UV ⌈w/2⌉×⌈h/2⌉ RG8), `copy_planes`(행 루프: `copy_nonoverlapping(src+row*stride, dst+row*pitch, row_bytes)`), `planes_to_vecs`. 실패 시 생성한 텍스처 Release 후 None.
- PROF 게이트(SERVO_D3D11_PROFILE) 유지: claim/copy/publish 타이밍 + 드롭 카운터로 재조준. profile 헬퍼는 lib.rs로 이동.
- **삭제 확인 목록**: `SERVO_MEDIA_D3D11_UPLOAD`(UploadMode :74-89), `build_video_sink_legacy`(:176-218), ConverterHandle(:101-111), ffi.rs·interop.rs 전체, d3d11_upload_poc 예제, Cargo libloading.

- [ ] **Step 1: 행 복사 실패 테스트 작성** (`ring_producer.rs` `#[cfg(test)]` — 가짜 src stride 96/dst pitch 128에서 row_bytes 64 복사 검증, YV12 스왑 매핑 검증). Run: `cargo test -p servo-media-gstreamer-render-d3d11` (PKG_CONFIG_PATH 필요) → 컴파일 실패 확인
- [ ] **Step 2: ring_producer.rs 구현 + lib.rs 재작성 + 삭제 수행**
- [ ] **Step 3: 테스트/체크** — Run: 위 cargo test → passed; `cargo check --workspace` → 에러 0
- [ ] **Step 4: 커밋**
```bash
git add components/media/backends/gstreamer/render-d3d11/ Cargo.toml Cargo.lock
git commit -m 'render-d3d11: 변환기/RGBA링/fence/legacy 삭제 — plane 링 프로듀서(memcpy 전용)로 재작성'
```
(삭제 파일은 `git add`가 스테이징함 — 경로 지정 add로 충분. `git status --short`로 의도 외 파일 없는지 확인.)

---

### Task 6: media-thread — plane 바인딩 + lock 재작성 + 디바이스 publish

**Files:**
- Modify: `components/media/media-thread/lib.rs`

**Interfaces:**
- Produces (htmlmediaelement이 사용):
  - `D3d11VideoFrameExternalImages::allocate_plane_ids(count: usize) -> Option<Vec<ExternalImageId>>` (raw의 :463 사용례 미러)
  - `D3d11VideoFrameExternalImages::update_plane(id, binding: D3d11PlaneBinding)` / `remove_plane(id)`
  - `pub struct D3d11PlaneBinding { pub ring_id: u64, pub ring_epoch: u32, pub plane_index: usize, pub width: i32, pub height: i32 }`
- Consumes: `servo_media::player::d3d11_ring::*`(재수출 경유 — servo-media facade lib.rs에 재수출 없으면 추가), Task 2 trait 메서드.

- [ ] **Step 1: 기존 D3d11VideoFrameInfo/API를 plane 바인딩으로 교체** (:123-172 재작성; 기존 update/remove/info_for 시그니처는 삭제 — 사용처는 htmlmediaelement뿐이라 Task 7에서 정리되지만, 이 태스크의 컴파일을 위해 htmlmediaelement의 D3D11 구경로 호출(:756, :418-427의 d3d11 분기, ensure_d3d11_external_id)을 **이 태스크에서 함께 스텁화**(호출 제거+drop 로그)한다. — Task 7이 정식 구현으로 대체)
- [ ] **Step 2: 디바이스 publish** — `MediaExternalImages::new`(:429) 끝에:
```rust
if let Some(rc) = rendering_context.as_ref() {
    if let Some(device) = rc.media_d3d11_device_handle() {
        servo_media::player::d3d11_ring::D3d11PlaneRings::set_consumer_device(device);
    }
}
```
- [ ] **Step 3: lock_d3d11 재작성** (:454-500 대체):
```rust
fn lock_d3d11(&mut self, id: u64, binding: D3d11PlaneBinding) -> (ExternalImageSource<'_>, Size2D<i32>) {
    let Some(rc) = self.rendering_context.as_ref() else { return (Invalid, zero) };
    // 1) 합성당 1회 소비: 0→1 lock에서만 plan이 나온다
    if let Some(plan) = D3d11PlaneRings::note_plane_lock_and_plan(binding.ring_id) {
        match plan {
            ConsumePlan::InitialMapAll { slots, staged } => {
                // 전 슬롯 전 plane Map → 스테이징을 슬롯0에 행 복사(스테이징은 연속
                // 바이트, 복사는 rc가 준 (ptr,pitch)로 이 스레드에서) → commit
            },
            ConsumePlan::Advance { unmap, map, filled_slot } => {
                for tex in unmap { rc.unmap_d3d11_texture(tex); }
                let mapped = map.iter().map(|&t| (t, rc.map_d3d11_dynamic_texture(t))).collect();
                D3d11PlaneRings::commit_consume(binding.ring_id, /*mapped, filled_slot*/);
            },
        }
    }
    // 2) EGLImage 캐시 (texture usize → D3d11GlWrappedTexture)
    let Some(plane) = D3d11PlaneRings::presenting_plane(binding.ring_id, binding.plane_index)
        else { return (Invalid, zero) };
    let wrap = match self.d3d11_wrap_cache.entry(plane.texture) {
        Occupied(e) => *e.get(),
        Vacant(v) => match rc.wrap_d3d11_texture_as_gl_texture(plane.texture) {
            Some(w) => *v.insert(w),
            None => { warn!(...); return (Invalid, zero); },
        },
    };
    (ExternalImageSource::NativeTexture(wrap.gl_texture), Size2D::new(plane.width, plane.height))
}
```
(스테이징 복사는 `#![deny(unsafe_code)]` 하에서 불가 — **rc에 위임**: Task 2에 보조 메서드 `copy_rows_to_mapped(dst_ptr: usize, dst_pitch: u32, src: &[u8], row_bytes: usize, rows: usize)`를 추가(안전 시그니처, 내부 unsafe)하고 여기서 사용. Task 2 커밋에 포함해도 되고 이 태스크에서 paint_api에 추가 커밋해도 된다.)
- [ ] **Step 4: unlock/정리** — `WebRenderExternalImageApi::unlock`에서 d3d11 바인딩이면 `note_plane_unlock`; `purge_removed_d3d11_entries`를 `take_removed_rings` 기반으로 재작성(각 링: mapped 텍스처 unmap → 캐시 wrap destroy → 텍스처 release); Drop(:503-518)도 동일 정리 + 캐시 전체.
- [ ] **Step 5: check** — `cargo check --workspace` → 에러 0
- [ ] **Step 6: 커밋**
```bash
git add components/media/media-thread/lib.rs components/shared/paint/rendering_context.rs
git commit -m 'media-thread: plane 바인딩 레지스트리 + 링 소비 lock (EGLImage 캐시, 렌더러 스레드 Map/Unmap)'
```

---

### Task 7: htmlmediaelement — D3D11 YUV 표시 경로 + 구 타입 삭제

**Files:**
- Modify: `components/script/dom/html/htmlmediaelement.rs`
- Modify: `components/media/player/video.rs` (구 `VideoFrameD3D11Data`/`D3D11` variant 삭제)

**Interfaces:**
- Consumes: Task 6의 `allocate_plane_ids/update_plane/remove_plane/D3d11PlaneBinding`, `VideoFrameD3D11YuvData`.
- Produces: 없음(말단).

- [ ] **Step 1: `render_d3d11_yuv_frame` 구현** — **`render_yuv_frame`(:574-734)을 원형으로 이식**, 차이만:
  - external id 확보: `ensure_yuv_external_ids`(:452) 미러의 d3d11판 `ensure_d3d11_plane_ids` (MediaYuvExternalIds 구조 재사용 가능 — 필드 의미 동일)
  - plane 데이터 갱신: `RawVideoFrameExternalImages::update_plane` 대신 `D3d11VideoFrameExternalImages::update_plane(id, D3d11PlaneBinding{ring_id, ring_epoch, plane_index, width: plane_w, plane_h})` — plane 치수는 `yuv_plane_descriptor`(:243)와 동일 규칙(Y=w×h, 그 외 ⌈/2⌉; NV12 plane1=RG8)
  - image data: `external_yuv_plane_data`(:259)의 TextureHandle 변형 신설:
    ```rust
    fn external_d3d11_plane_data(id: ExternalImageId) -> SerializableImageData {
        SerializableImageData::External(ExternalImageData {
            id, channel_index: 0,
            image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
            normalized_uvs: false,
        })
    }
    ```
  - `media_frame_yuv_image`(:554) 재사용 (color_depth=Color8 고정 — P010 비범위)
- [ ] **Step 2: 디스패치·수명주기 연결** — `render()`의 스텁 arm(:Task 4)을 정식 호출로 교체; `reset()`/`push_delete_frame_images`의 d3d11 분기(:418-427)를 plane id 기반 `remove_plane`으로; 구 `render_d3d11_frame`(:736-849)·`ensure_d3d11_external_id`(:469-476)·`d3d11_external_id` 필드 삭제.
- [ ] **Step 3: video.rs 구 타입 삭제** — `VideoFrameD3D11Data`(:66-69), `VideoFrameData::D3D11` variant, `get_d3d11_data` — cargo check가 지목하는 잔여 사용처 정리.
- [ ] **Step 4: 수직 플립 확인** — `grep -rn "needs_vertical_flip" components/` 로 §3-k 예외 메커니즘 위치 확인, **plane external id들이 기존 D3D11 RGBA와 동일 분기**를 타도록 연결(대개 media-thread lock 반환 또는 paint dispatch에서 id/타입 판별 — 실코드에 맞춰 최소 수정). 판정은 Task 8 육안.
- [ ] **Step 5: 전체 빌드** — Run(BACKGROUND): `.\mach build --release` Expected: 성공
- [ ] **Step 6: 커밋**
```bash
git add components/script/dom/html/htmlmediaelement.rs components/media/player/video.rs components/media/media-thread/lib.rs components/shared/paint/lib.rs
git commit -m 'htmlmediaelement: D3D11 프레임을 plane별 YUV external(TextureHandle)로 발행 - WR 직접 샘플 완성'
```

---

### Task 8: 통합 검증 (스펙 §8)

**Files:** 신규 코드 없음(수정은 발견 결함 한정). 도구: `etc/multigpu/run_video_wall_d3d11.ps1`, scratchpad 픽셀 검사 스크립트(§3-m 셀 휘도 샘플링 방법), WR 프로파일러(Ctrl+F12), PresentMon(`D:\PresentMon-2.3.1-x64.exe`).

- [ ] **Step 1: 2×2 스모크** — `run_video_wall_d3d11.ps1 -Cols 2 -Rows 2` → (a) 4/4 재생·d3d11 마커, (b) 스크린샷 픽셀: 색 정상(BT.709 limited 기준 — 기존 RGBA 경로 스크린샷과 동일 프레임 비교가 가장 확실), (c) 상하 방향(내장 프레임카운터 텍스트 정립), (d) `RUST_LOG` 경고에 import/래핑 실패 0
- [ ] **Step 2: WR 구조 확인** — Ctrl+F12: Texture cache update ≈0 유지, GPU 시간·draw 구성 기록(전후 비교용)
- [ ] **Step 3: 45타일 회귀** — `-Cols 9 -Rows 5 -Sync -1` 150s: FAIL 0·마커 45/45·lockstep ±1(프레임카운터)·시작 안착 t≤기존(~6s — 스테이징 45×3.1MB 영향 확인)·메모리 플래토·루프 경계 무결(gapless). PROF로 드롭 카운터·claim/copy 시간 확인
- [ ] **Step 4: WebGPU 월 무회귀** — WebGPU 월 페이지 실행(§3-p 검증 때와 동일 방식), 블랙 타일 0 — `create_texture_from_shared_handle` 경로 보존 확인
- [ ] **Step 5: 결함 있으면 수정 후 해당 태스크 커밋 컨벤션으로 개별 커밋, 전부 통과 시 검증 결과를 커밋 메시지에 요약해 문서 커밋(Task 9와 병합 가능)**

---

### Task 9: 문서·마무리

- [ ] **Step 1: 스펙 상태 갱신** — 스펙 문서 머리 `상태: 구현 완료 (2026-07-XX, 검증 요약)` + 구현 중 스펙과 달라진 점 추기
- [ ] **Step 2: 패키징 노트** — `etc/multigpu/`의 관련 문서(런처 README류)에: gstd3d11.dll/gstd3d11-1.0-0.dll 번들 의무 소멸(§3-n 함정 2 폐기), `-LegacyUpload` 스위치 무효화 명시. ServoWallPackage 재패키징은 사용자 지시 시 별도
- [ ] **Step 3: 최종 whole-branch 리뷰** — superpowers:requesting-code-review 관례(§3-k 선례: opus 리뷰로 Critical/Important 0 확인) 후 커밋
```bash
git add docs/superpowers/specs/2026-07-12-wr-yuv-direct-sample-design.md etc/multigpu/
git commit -m 'WR YUV 직접 샘플 구현 완료: 스펙 상태 갱신 + 패키징 노트'
```

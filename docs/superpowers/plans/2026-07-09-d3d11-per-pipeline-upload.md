# 파이프라인별 D3D11 직접 업로드 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 렌더러 스레드의 비디오 업로드를 0으로 만든다 — 각 gst 파이프라인이 자기 스트리밍 스레드에서 D3D11 GPU 텍스처로 업로드/변환하고, WR에는 GPU 상주 텍스처 핸들만 전달한다 (스펙: `docs/superpowers/specs/2026-07-09-d3d11-per-pipeline-upload-design.md`).

**Architecture:** playbin3의 video-sink를 `d3d11upload ! appsink(memory:D3D11Memory, 디코더 원 포맷)` bin으로 교체(신규 `render-d3d11` 크레이트, 기존 render-unix 패턴의 Windows 대응물). appsink 콜백(스트리밍 스레드)에서 gstd3d11 라이브러리의 공개 `GstD3D11Converter`로 YUV→RGBA 변환을 **플레이어별 공유 텍스처 링(4슬롯, `D3D11_RESOURCE_MISC_SHARED`) 슬롯에 직접 렌더**(추가 GPU 복사 0회 — d3d11convert 엘리먼트의 내부 엔진을 직접 사용), 완료 fence 후 공유 핸들(u64)을 `VideoFrameData::D3D11`로 전달. htmlmediaelement가 단일 ImageKey + `ExternalImageType::TextureHandle`로 발행, 렌더러 lock()에서 **기존 검증된** `RenderingContext::create_texture_from_shared_handle`(surfman ANGLE, WebGPU gpu-direct가 사용)로 1회 래핑 후 캐시 → 프레임당 렌더러 비용 ≈ 0.

**Tech Stack:** Rust, GStreamer 0.25(gstreamer-rs) + gstd3d11-1.0-0.dll 공개 C API(`GstD3D11Converter`/`alloc_wrapped` 포함, libloading 동적 FFI — **Rust 바인딩 crates.io에 부재 확인됨**), winapi/wio(D3D11 COM), surfman ANGLE(`EGL_ANGLE_d3d_texture_client_buffer` 경유, 기존 구현 재사용), WebRender external image(TextureHandle).

## Global Constraints

- env 게이트 `SERVO_MEDIA_D3D11_VIDEO=1` (기본 off). off면 기존 Raw 경로와 **동작 동일**(A/B 공존, 스펙 §5). 사전 점검 실패(플러그인/DLL/디바이스) 시 플레이어 단위로 Raw 폴백 + 경고 로그.
- 단일 프로세스 전용 (기존 raw YUV 경로와 동일 제약 — multiprocess/force_ipc면 비활성).
- 스펙 §8 비범위 준수: HW 디코드(d3d11h264dec) 전환 금지, DirectComposition 금지, 오디오 경로 무변경, Raw 경로 제거 금지.
- 멀티GPU(스펙 §4.5)는 이번 단계 비활성 — 단, 디바이스 생성 API는 어댑터 인덱스를 받는 형태로 작성(기본 0). 후속 단계에서 LUID 주입만 추가.
- 사용자 원칙: 휴리스틱/완화책 금지 — 구조적 해결만.
- 커밋 메시지 한국어, Claude 서명/Co-Authored-By 제외. `git add`는 변경 파일 경로만(`-A`/`.` 금지). **커밋 메시지에 큰따옴표 금지**(PowerShell here-string 함정 — 메모리 §3-g).
- Rust 주석 한국어 허용 (이 포크 관례).
- 빌드: `etc/multigpu/servo_env.ps1` 소싱 후 `$ErrorActionPreference='Continue'` 설정, `.\mach build --release` (BACKGROUND 실행, 수분).
- 시스템 GStreamer: `C:\gstreamer\1.0\msvc_x86_64` (gstd3d11-1.0-0.dll·gstd3d11.dll 존재 확인됨). cargo test/example 실행 시 `$env:PATH`에 `C:\gstreamer\1.0\msvc_x86_64\bin` 추가 + `$env:GST_PLUGIN_PATH = "C:\gstreamer\1.0\msvc_x86_64\lib\gstreamer-1.0"`.

## 스펙과의 의도적 차이 3건 (근거 포함 — 실행 전 사용자 확인 사항)

1. **ANGLE 래핑 = `EGL_ANGLE_d3d_texture_client_buffer` 경로 (스펙 §4.3의 "대안(폴백)")를 1순위로 채택.**
   근거: 트리에 이미 구현·**월에서 검증**된 `RenderingContext::create_texture_from_shared_handle`(`components/shared/paint/rendering_context.rs:451-480`, WebGPU gpu-direct가 사용)이 정확히 이 경로다. 스펙 1순위(`EGL_ANGLE_image_d3d11_texture`)는 vendored surfman 수정(EGL_D3D11_TEXTURE_ANGLE=0x3484 추가 + 신규 Device 메서드)이 필요하고 기능 이득이 없다. 성능 목표(렌더러 ≈0)는 래핑 캐시(아래 3)로 달성.
2. **gst d3d11 버퍼 풀 공유 할당 + d3d11convert 엘리먼트(스펙 §4.1) 대신: 자체 공유 텍스처 링(4슬롯) + 라이브러리 `GstD3D11Converter`로 링 슬롯에 직접 변환-렌더 (추가 복사 0회).**
   근거: ① `gstreamer-d3d11` Rust 바인딩이 crates.io에 **존재하지 않음**(2026-07-09 API 조회 확정)이고, appsink는 allocation query에 풀을 제안하지 않아 풀 공유 할당은 쿼리 가로채기가 필요. ② gstd3d11 라이브러리가 d3d11convert의 내부 엔진을 공개 API로 노출(`gst_d3d11_converter_new/convert_buffer`, 헤더 확정) — 어차피 필요한 YUV→RGBA 변환 패스의 **출력 대상을 링 슬롯으로 지정**하면 변환=핸드오프가 되어 별도 복사가 아예 없다(2026-07-09 사용자 지적으로 채택; 초안의 "복사 1회" 설계를 대체). ③ 링이면 gst 버퍼를 변환 직후 즉시 반환(디코더 풀 건강이 렌더러 지연과 분리)하고 **핸들이 플레이어 수명 동안 안정** → 렌더러 측 SurfaceTexture 캐시 가능(프레임당 재래핑 0 — 스펙 §4.3 캐시 요구를 더 강하게 충족). **폴백 사다리**(PoC 실패 시 순서대로): ⓐ 래핑 버퍼에 VideoMeta 추가 ⓑ out 포맷 BGRA 전환 ⓒ d3d11convert 엘리먼트 복귀 + 링으로 `CopySubresourceRegion` 1회(초안 A 방식 — 45타일 기준 VRAM 내부 11GB/s ≈ 대역폭 2~3%로 여전히 실용적).
3. **keyed mutex(스펙 §4.4) 대신 producer 측 완료 대기(D3D11_QUERY_EVENT) 후 발행 + 4슬롯 라운드로빈 재사용 마진.**
   근거: 트리 내 검증 선례(WebGPU gpu-direct 공유 핸들 — keyed mutex 없음, 2×A4000 월 60fps 검증). 렌더러는 절대 대기하지 않음(스펙 §4.4의 목표 그대로). surfman의 기존 import 경로가 keyed mutex 텍스처에 `AcquireSync(0, INFINITE)`를 걸어 캐시와 충돌하는 문제도 회피. **폴백(문제 발생 시)**: 슬롯 텍스처를 `MISC_SHARED_KEYEDMUTEX`로 바꾸고 캐시 대신 매 lock마다 import/destroy(WebGPU 방식) — surfman이 acquire/release를 자동 처리, 코드 변경 국소적.

## 코드 앵커 (2026-07-09 탐색 확정 — 전부 실측)

**gstd3d11 C API (헤더 `C:\gstreamer\1.0\msvc_x86_64\include\gstreamer-1.0\gst\d3d11\` 에서 시그니처 확정):**
- `GstD3D11Device * gst_d3d11_device_new(guint adapter_index, guint flags)` (gstd3d11device.h:86)
- `GstD3D11Device * gst_d3d11_device_new_for_adapter_luid(gint64, guint)` (:90 — 멀티GPU 후속용)
- `ID3D11Device * gst_d3d11_device_get_device_handle(GstD3D11Device*)` (:97)
- `ID3D11DeviceContext * gst_d3d11_device_get_device_context_handle(GstD3D11Device*)` (:100)
- `void gst_d3d11_device_lock / _unlock(GstD3D11Device*)` (:112/:115) — immediate context 접근은 반드시 이 락 하에서
- `GstContext * gst_d3d11_context_new(GstD3D11Device*)` (gstd3d11utils.h:55), 컨텍스트 타입 문자열 `"gst.d3d11.device.handle"` (gstd3d11device.h:49)
- `gboolean gst_is_d3d11_memory(GstMemory*)` (gstd3d11memory.h:195)
- `ID3D11Resource * gst_d3d11_memory_get_resource_handle(GstD3D11Memory*)` (:201)
- `guint gst_d3d11_memory_get_subresource_index(GstD3D11Memory*)` (:216)
- caps feature `"memory:D3D11Memory"` (gstd3d11memory.h:67)
- `GstD3D11Converter * gst_d3d11_converter_new(GstD3D11Device*, const GstVideoInfo* in, const GstVideoInfo* out, GstStructure* config /*NULL 허용*/)` (gstd3d11converter.h:183) — d3d11convert 엘리먼트의 내부 변환 엔진(공개 API)
- `gboolean gst_d3d11_converter_convert_buffer(GstD3D11Converter*, GstBuffer* in, GstBuffer* out)` (:189; `_unlocked` 변형 :194. converter는 GstObject — 해제는 g_object_unref)
- `GstMemory * gst_d3d11_allocator_alloc_wrapped(GstD3D11Allocator*, GstD3D11Device*, ID3D11Texture2D*, gsize, gpointer user_data, GDestroyNotify)` (gstd3d11memory.h:308) — 우리 링 텍스처를 GstD3D11Memory로 래핑
- allocator 획득: `gst_allocator_find("D3D11Memory")`(gstreamer core FFI) → null이면 `g_object_new(gst_d3d11_allocator_get_type(), NULL)` 폴백 (`gst_d3d11_allocator_get_type` gstd3d11memory.h:295)
- (참고: 풀 공유 할당용 `gst_d3d11_buffer_pool_new`(gstd3d11bufferpool.h:74)·`gst_d3d11_allocation_params_new(device, info, flags, bind_flags, misc_flags)`(gstd3d11memory.h:143)도 존재하나 직접 변환 채택으로 불사용)

**servo-media (전부 in-tree vendored — `components/media/`):**
- `Render` trait: `render/lib.rs:22-51` — `is_gl()`, `build_frame(sample) -> Option<VideoFrame>`, `build_video_sink(appsink: &Element, pipeline: &Element) -> Result<(), PlayerError>`
- 플랫폼 선택: `backends/gstreamer/render.rs:88-156` — Windows는 현재 `RenderDummy`(create_render → None → CPU I420 경로). CPU caps 분기 `:301-320`.
- `VideoFrameData` enum: `player/video.rs:62-68` — `Raw/Yuv/Texture/OESTexture`; `Buffer` trait `:70-81`; `VideoFrame::new` `:92-97`
- appsink 콜백 → `render.get_frame_from_sample(sample)` → `video_renderer.render(frame)`: `backends/gstreamer/player.rs:1454-1507`
- env 헬퍼 `env_flag_enabled`: `player.rs:210-217`

**Servo 측:**
- `RenderingContext::create_texture_from_shared_handle(handle: u64, size: UntypedSize2D<i32>) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)>`: trait `components/shared/paint/rendering_context.rs:94-100`, ANGLE 구현 `:451-480` (내부: `OpenSharedResource` → pbuffer → `eglBindTexImage`; keyed mutex 없는 텍스처면 mutex 무개입). `destroy_texture(SurfaceTexture)`: `:87/:442-449`. `SurfaceTexture` 재수출 `:25`.
- WebGPU 선례(미러 대상): `components/webgpu/canvas_context.rs:351-420` — lock에서 import→`NativeTexture(gl_texture)`, unlock에서 destroy. import는 `paint_api::rendering_context::{RenderingContext, SurfaceTexture}` (canvas_context.rs:14,16).
- 미디어 핸들러: `components/media/media-thread/lib.rs` — `MediaExternalImages` `:330-447`(raw 분기 + GLPlayer 분기), `RawVideoFrameExternalImages` `:30-115`, `initialize_image_handler` `:237-260` (**유일 호출처 = `components/paint/painter.rs:291`**), `#![deny(unsafe_code)]` 주의(추가 코드 전부 safe).
- 외부 이미지 디스패치: `components/shared/paint/lib.rs:687-746` — Media/WebGpu/WebGl 모두 uv `TexelRect::new(0.0, h, w, 0.0)`(수직 플립). **WebGPU gpu-direct(D3D 텍스처)가 이 플립으로 정상 표시 검증됨 → 방향 문제 없음이 유력.**
- htmlmediaelement: `components/script/dom/html/htmlmediaelement.rs` — `MediaFrameRenderer` 구조체 `:265-287`, `reset()` `:382-431`, `push_delete_frame_images` `:433-440`, `render_yuv_frame` `:555-715`(미러 원형), `render()` 디스패치 `:724-770`, GL TextureHandle 원형 `:824-845`.
- 번들: `components/servo/gstreamer_plugin_lists/windows.rs.in`(현재 gstwasapi만), `python/servo/gstreamer.py:50-81` `GSTREAMER_WIN_DEPENDENCY_LIBS`. **`mach build`가 복사 수행**(`python/servo/build_commands.py:205→340→350`). 런타임 플러그인 로딩도 같은 목록(`components/servo/gstreamer_plugins.rs:6-16`).
- gstd3d11.dll 플러그인 의존(임포트 테이블 실측): `gstd3d11-1.0-0.dll`, `gstd3dshader-1.0-0.dll`, `gstdxva-1.0-0.dll`, `gstcodecs-1.0-0.dll` (+ 시스템 d3d11/dxgi). 현재 번들 목록에 전부 없음.
- 하드웨어 디코더 강등(`backends/gstreamer/lib.rs:111-146`)은 `DECODER` 타입만 순회 → d3d11upload/d3d11convert는 **무관, 수정 불필요**.
- 워크스페이스: `winapi = "0.3"`(:281), `wio = "0.2"`(:285), `euclid`(:87) 있음. **`libloading` 워크스페이스 항목 없음(추가 필요)**. gstreamer 0.25.

---

### Task 1: `render-d3d11` 크레이트 뼈대 + gstd3d11 FFI + 공유 디바이스

**Files:**
- Create: `components/media/backends/gstreamer/render-d3d11/Cargo.toml`
- Create: `components/media/backends/gstreamer/render-d3d11/lib.rs`
- Create: `components/media/backends/gstreamer/render-d3d11/ffi.rs`
- Create: `components/media/backends/gstreamer/render-d3d11/interop.rs`
- Modify: `Cargo.toml` (루트 — workspace.dependencies 2줄)
- Test: `render-d3d11/interop.rs` 내 `#[cfg(test)]`

**Interfaces:**
- Produces: `ffi::GstD3D11Api::load() -> Option<&'static GstD3D11Api>` (libloading, 실패 시 warn 로그 + None);
  `interop::SharedGstD3D11Device::get_or_create() -> Option<Arc<SharedGstD3D11Device>>` (프로세스 전역 1개, 어댑터 0),
  메서드 `raw() -> *mut GstD3D11Device`, `d3d11_device() -> *mut ID3D11Device`, `immediate_context() -> *mut ID3D11DeviceContext`, `allocator() -> *mut GstD3D11Allocator`, `lock() -> DeviceLockGuard`, `gst_context() -> Option<gstreamer::Context>`, `api() -> &'static GstD3D11Api`.
- Consumes: 없음 (신규 크레이트).

- [ ] **Step 1: 워크스페이스 등록**

루트 `Cargo.toml`의 `[workspace.dependencies]`에서 line 346(`servo-media-gstreamer-render = ...`) 다음 줄에 삽입:
```toml
servo-media-gstreamer-render-d3d11 = { version = "=0.3.0", path = "components/media/backends/gstreamer/render-d3d11" }
```
그리고 알파벳 위치(line 144 `log = "0.4.30"` 근처, `libc`류 항목 부근)에 삽입:
```toml
libloading = "0.8"
```

- [ ] **Step 2: 크레이트 매니페스트 작성**

`components/media/backends/gstreamer/render-d3d11/Cargo.toml`:
```toml
[package]
name = "servo-media-gstreamer-render-d3d11"
version.workspace = true
authors.workspace = true
license.workspace = true
edition.workspace = true
publish.workspace = true
rust-version.workspace = true
repository.workspace = true
description.workspace = true

[lib]
name = "servo_media_gstreamer_render_d3d11"
path = "lib.rs"

[dependencies]
gstreamer = { workspace = true }
gstreamer-video = { workspace = true }
libloading = { workspace = true }
log = { workspace = true }
servo-config = { workspace = true }
servo-media-gstreamer-render = { workspace = true }
servo-media-player = { workspace = true }
winapi = { workspace = true, features = [
    "d3d11",
    "dxgi",
    "guiddef",
    "handleapi",
    "unknwnbase",
    "winerror",
] }
wio = { workspace = true }

[dev-dependencies]
gstreamer-app = { workspace = true }
```

- [ ] **Step 3: lib.rs 뼈대 (모듈 선언 + 재수출만)**

`render-d3d11/lib.rs`:
```rust
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Windows용 D3D11 비디오 렌더 경로.
//!
//! 파이프라인의 스트리밍 스레드에서 d3d11upload/d3d11convert로 GPU 업로드·RGBA 변환을
//! 수행하고, 공유 텍스처 링에 복사한 뒤 공유 핸들만 Servo로 전달한다. 렌더러 스레드의
//! 비디오 업로드(glTexSubImage2D)를 제거하는 것이 목적. env `SERVO_MEDIA_D3D11_VIDEO=1`
//! 게이트 (기본 off). 설계: docs/superpowers/specs/2026-07-09-d3d11-per-pipeline-upload-design.md

pub mod ffi;
pub mod interop;

pub use interop::{SharedGstD3D11Device, SharedTextureRing};
```
(RenderD3D11 본체는 Task 4에서 추가 — 이 태스크는 FFI/디바이스까지.)

- [ ] **Step 4: 실패하는 테스트 작성 (RED)**

`render-d3d11/interop.rs` 말미에 (모듈 본문은 Step 6에서 작성하므로 이 시점에는 파일에 테스트와 최소 스텁만 있어도 됨 — 권장: Step 5·6의 코드를 작성한 뒤 테스트가 "컴파일은 되나 로직 미완으로 실패"하는 상태를 확인하는 순서로 진행):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::GstD3D11Api;

    // 요구: PATH에 C:\gstreamer\1.0\msvc_x86_64\bin (gstd3d11-1.0-0.dll)
    #[test]
    fn load_api_and_create_shared_device() {
        gstreamer::init().expect("gstreamer init 실패 — PATH에 gstreamer bin 필요");
        let _api = GstD3D11Api::load().expect("gstd3d11-1.0-0.dll 로드/심볼 해석 실패");
        let device = SharedGstD3D11Device::get_or_create().expect("GstD3D11Device 생성 실패");
        assert!(!device.d3d11_device().is_null());
        assert!(!device.immediate_context().is_null());
        assert!(!device.allocator().is_null());
        let context = device.gst_context().expect("gst_d3d11_context_new 실패");
        assert_eq!(context.context_type(), "gst.d3d11.device.handle");
        // 전역 공유: 두 번째 호출은 같은 디바이스
        let device2 = SharedGstD3D11Device::get_or_create().expect("재호출 실패");
        assert_eq!(device.d3d11_device(), device2.d3d11_device());
    }
}
```

- [ ] **Step 5: ffi.rs 구현**

`render-d3d11/ffi.rs`:
```rust
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! gstd3d11-1.0-0.dll 공개 C API에 대한 동적 FFI.
//!
//! crates.io에 gstreamer-d3d11 바인딩이 없어(2026-07-09 확인) 필요한 최소 심볼만
//! libloading으로 로드한다. 시그니처는 GStreamer 1.26 msvc 헤더
//! (gst/d3d11/gstd3d11device.h, gstd3d11memory.h, gstd3d11utils.h)에서 확정.

use std::sync::OnceLock;

use winapi::um::d3d11::{ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D};

/// GObject 기반 불투명 타입 (참조만 주고받음).
#[repr(C)]
pub struct GstD3D11Device {
    _private: [u8; 0],
}

/// GstMemory를 첫 필드로 내장하는 불투명 타입. gst_is_d3d11_memory 확인 후
/// *mut GstMemory에서 캐스팅해 사용한다.
#[repr(C)]
pub struct GstD3D11Memory {
    _private: [u8; 0],
}

/// YUV→RGBA 변환 엔진 (d3d11convert 엘리먼트 내부와 동일). GstObject 파생 —
/// 해제는 g_object_unref.
#[repr(C)]
pub struct GstD3D11Converter {
    _private: [u8; 0],
}

#[repr(C)]
pub struct GstD3D11Allocator {
    _private: [u8; 0],
}

type Gboolean = i32;
// GType — glib ABI상 usize.
type GType = usize;

pub struct GstD3D11Api {
    // Library는 fn 포인터 수명 유지를 위해 보관 (drop 금지)
    _lib: libloading::Library,
    pub device_new: unsafe extern "C" fn(u32, u32) -> *mut GstD3D11Device,
    pub device_get_device_handle: unsafe extern "C" fn(*mut GstD3D11Device) -> *mut ID3D11Device,
    pub device_get_device_context_handle:
        unsafe extern "C" fn(*mut GstD3D11Device) -> *mut ID3D11DeviceContext,
    pub device_lock: unsafe extern "C" fn(*mut GstD3D11Device),
    pub device_unlock: unsafe extern "C" fn(*mut GstD3D11Device),
    pub context_new: unsafe extern "C" fn(*mut GstD3D11Device) -> *mut gstreamer::ffi::GstContext,
    pub is_d3d11_memory: unsafe extern "C" fn(*mut gstreamer::ffi::GstMemory) -> Gboolean,
    pub memory_get_resource_handle:
        unsafe extern "C" fn(*mut GstD3D11Memory) -> *mut ID3D11Resource,
    pub memory_get_subresource_index: unsafe extern "C" fn(*mut GstD3D11Memory) -> u32,
    pub converter_new: unsafe extern "C" fn(
        *mut GstD3D11Device,
        *const gstreamer_video::ffi::GstVideoInfo,
        *const gstreamer_video::ffi::GstVideoInfo,
        *mut gstreamer::ffi::GstStructure,
    ) -> *mut GstD3D11Converter,
    pub converter_convert_buffer: unsafe extern "C" fn(
        *mut GstD3D11Converter,
        *mut gstreamer::ffi::GstBuffer,
        *mut gstreamer::ffi::GstBuffer,
    ) -> Gboolean,
    pub allocator_get_type: unsafe extern "C" fn() -> GType,
    pub allocator_alloc_wrapped: unsafe extern "C" fn(
        *mut GstD3D11Allocator,
        *mut GstD3D11Device,
        *mut ID3D11Texture2D,
        usize,
        *mut std::ffi::c_void,
        gstreamer::glib::ffi::GDestroyNotify,
    ) -> *mut gstreamer::ffi::GstMemory,
}

impl GstD3D11Api {
    /// 프로세스 전역 1회 로드. 실패하면 warn 로그 후 None (호출측은 Raw 폴백).
    pub fn load() -> Option<&'static GstD3D11Api> {
        static API: OnceLock<Option<&'static GstD3D11Api>> = OnceLock::new();
        *API.get_or_init(|| match Self::load_impl() {
            Ok(api) => Some(Box::leak(Box::new(api))),
            Err(error) => {
                log::warn!("D3D11 video: gstd3d11-1.0-0.dll 로드 실패, Raw 경로 폴백: {error}");
                None
            },
        })
    }

    fn load_impl() -> Result<GstD3D11Api, libloading::Error> {
        unsafe {
            let lib = libloading::Library::new("gstd3d11-1.0-0.dll")?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get($name)?
                };
            }
            Ok(GstD3D11Api {
                device_new: sym!(b"gst_d3d11_device_new\0"),
                device_get_device_handle: sym!(b"gst_d3d11_device_get_device_handle\0"),
                device_get_device_context_handle: sym!(
                    b"gst_d3d11_device_get_device_context_handle\0"
                ),
                device_lock: sym!(b"gst_d3d11_device_lock\0"),
                device_unlock: sym!(b"gst_d3d11_device_unlock\0"),
                context_new: sym!(b"gst_d3d11_context_new\0"),
                is_d3d11_memory: sym!(b"gst_is_d3d11_memory\0"),
                memory_get_resource_handle: sym!(b"gst_d3d11_memory_get_resource_handle\0"),
                memory_get_subresource_index: sym!(b"gst_d3d11_memory_get_subresource_index\0"),
                converter_new: sym!(b"gst_d3d11_converter_new\0"),
                converter_convert_buffer: sym!(b"gst_d3d11_converter_convert_buffer\0"),
                allocator_get_type: sym!(b"gst_d3d11_allocator_get_type\0"),
                allocator_alloc_wrapped: sym!(b"gst_d3d11_allocator_alloc_wrapped\0"),
                _lib: lib,
            })
        }
    }
}
```
(참고: `sym!` 매크로가 `_lib` 초기화 전에 `lib`를 빌리므로 위처럼 `_lib: lib`를 마지막 필드 초기화로 두면 된다. 매크로 전개 순서 문제로 컴파일 에러가 나면 각 심볼을 `let` 바인딩으로 풀어쓸 것.)

- [ ] **Step 6: interop.rs — 공유 디바이스 구현**

`render-d3d11/interop.rs` (테스트 모듈 위에):
```rust
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! GstD3D11Device 공유 래퍼 + 공유 텍스처 링.
//!
//! 디바이스는 어댑터당 1개를 프로세스 전역으로 공유한다(gst 권장, 스펙 §7).
//! immediate context 접근은 반드시 gst_d3d11_device_lock 하에서 수행
//! (d3d11upload/convert가 같은 디바이스를 다른 스레드에서 사용).

use std::sync::{Arc, OnceLock};

use gstreamer::glib::translate::from_glib_full;
use winapi::um::d3d11::{ID3D11Device, ID3D11DeviceContext};

use crate::ffi::{GstD3D11Allocator, GstD3D11Api, GstD3D11Device};

// D3D11_CREATE_DEVICE_BGRA_SUPPORT
const D3D11_DEVICE_FLAGS: u32 = 0x20;

pub struct SharedGstD3D11Device {
    api: &'static GstD3D11Api,
    device: *mut GstD3D11Device,
    allocator: *mut GstD3D11Allocator,
}

// 안전성: GstD3D11Device는 스레드 안전(GObject + 내부 뮤텍스). immediate context는
// lock()/DeviceLockGuard로 직렬화해서만 접근한다.
unsafe impl Send for SharedGstD3D11Device {}
unsafe impl Sync for SharedGstD3D11Device {}

pub struct DeviceLockGuard<'a> {
    device: &'a SharedGstD3D11Device,
}

impl Drop for DeviceLockGuard<'_> {
    fn drop(&mut self) {
        unsafe { (self.device.api.device_unlock)(self.device.device) }
    }
}

impl SharedGstD3D11Device {
    /// 어댑터 0의 전역 공유 디바이스. 최초 호출에서 생성, 실패 시 None 고정.
    pub fn get_or_create() -> Option<Arc<SharedGstD3D11Device>> {
        static DEVICE: OnceLock<Option<Arc<SharedGstD3D11Device>>> = OnceLock::new();
        DEVICE
            .get_or_init(|| {
                let api = GstD3D11Api::load()?;
                // 멀티GPU 후속: 어댑터 인덱스/LUID를 여기서 주입 (스펙 §4.5)
                let device = unsafe { (api.device_new)(0, D3D11_DEVICE_FLAGS) };
                if device.is_null() {
                    log::warn!("D3D11 video: gst_d3d11_device_new(adapter=0) 실패");
                    return None;
                }
                // 링 텍스처를 GstD3D11Memory로 래핑하기 위한 allocator.
                // 등록된 기본 allocator를 우선 찾고, 미등록이면 새 인스턴스 생성
                // (프로세스 수명 동안 보유 — 의도적 비해제).
                let allocator = unsafe {
                    let name = c"D3D11Memory";
                    let mut allocator = gstreamer::ffi::gst_allocator_find(name.as_ptr())
                        as *mut GstD3D11Allocator;
                    if allocator.is_null() {
                        allocator = gstreamer::glib::gobject_ffi::g_object_new(
                            (api.allocator_get_type)(),
                            std::ptr::null(),
                        ) as *mut GstD3D11Allocator;
                    }
                    allocator
                };
                if allocator.is_null() {
                    log::warn!("D3D11 video: GstD3D11Allocator 획득 실패");
                    return None;
                }
                Some(Arc::new(SharedGstD3D11Device { api, device, allocator }))
            })
            .clone()
    }

    pub fn api(&self) -> &'static GstD3D11Api {
        self.api
    }

    /// gstd3d11 FFI에 넘길 원시 디바이스 포인터.
    pub fn raw(&self) -> *mut GstD3D11Device {
        self.device
    }

    pub fn allocator(&self) -> *mut GstD3D11Allocator {
        self.allocator
    }

    pub fn d3d11_device(&self) -> *mut ID3D11Device {
        unsafe { (self.api.device_get_device_handle)(self.device) }
    }

    /// 주의: 반환 컨텍스트 사용은 반드시 lock() 가드 하에서.
    pub fn immediate_context(&self) -> *mut ID3D11DeviceContext {
        unsafe { (self.api.device_get_device_context_handle)(self.device) }
    }

    pub fn lock(&self) -> DeviceLockGuard<'_> {
        unsafe { (self.api.device_lock)(self.device) };
        DeviceLockGuard { device: self }
    }

    /// 파이프라인 주입용 GstContext ("gst.d3d11.device.handle").
    pub fn gst_context(&self) -> Option<gstreamer::Context> {
        let ptr = unsafe { (self.api.context_new)(self.device) };
        if ptr.is_null() {
            return None;
        }
        Some(unsafe { from_glib_full(ptr) })
    }
}
```

- [ ] **Step 7: 테스트 실행 (GREEN)**

Run (PowerShell):
```powershell
. "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\servo_env.ps1"
$ErrorActionPreference = 'Continue'
Set-Location "D:\2_TechReview\20260606_multigpu_browser\servo"
$env:PATH = "C:\gstreamer\1.0\msvc_x86_64\bin;$env:PATH"
$env:GST_PLUGIN_PATH = "C:\gstreamer\1.0\msvc_x86_64\lib\gstreamer-1.0"
cargo test -p servo-media-gstreamer-render-d3d11 --release -- --nocapture
```
Expected: `load_api_and_create_shared_device ... ok` (1 passed).

- [ ] **Step 8: 커밋**
```bash
cd /d/2_TechReview/20260606_multigpu_browser/servo
git add Cargo.toml Cargo.lock components/media/backends/gstreamer/render-d3d11
git commit -m 'render-d3d11 크레이트 신설: gstd3d11 동적 FFI + 공유 GstD3D11Device'
```

---

### Task 2: 공유 텍스처 링 (SharedTextureRing) — 복사·완료대기·타 디바이스 판독 검증

**Files:**
- Modify: `components/media/backends/gstreamer/render-d3d11/interop.rs`
- Test: 같은 파일 `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 1의 `SharedGstD3D11Device`, `GstD3D11Api`.
- Produces: `SharedTextureRing::new(device: Arc<SharedGstD3D11Device>) -> SharedTextureRing`;
  `acquire(&mut self, width: i32, height: i32) -> Option<(gstreamer::Buffer /*슬롯 래핑 버퍼*/, usize /*slot_index*/)>` — GstD3D11Converter의 출력 대상 확보(크기 변경 시 링 재생성 + epoch 증가);
  `finish(&mut self, slot_index: usize) -> Option<(u64 /*shared_handle*/, u32 /*ring_epoch*/)>` — GPU 작업 완료 fence 후에만 반환(발행 후 읽기 안전);
  `write_from_resource(&mut self, src: *mut ID3D11Resource, subresource: u32, width: i32, height: i32) -> Option<(u64, u32)>` — 테스트·폴백용(acquire→CopySubresourceRegion→finish).

- [ ] **Step 1: 실패하는 테스트 작성 (RED)**

`interop.rs` tests 모듈에 추가:
```rust
    use winapi::shared::dxgifmt::DXGI_FORMAT_R8G8B8A8_UNORM;
    use winapi::um::d3d11 as d3d;

    // 같은 디바이스에 단색 소스 텍스처 생성 → ring.write → 두 번째(별개) D3D11
    // 디바이스에서 공유 핸들 open → staging 판독 → 픽셀 일치 확인.
    #[test]
    fn ring_write_and_cross_device_readback() {
        gstreamer::init().expect("gstreamer init 실패");
        let device = SharedGstD3D11Device::get_or_create().expect("디바이스 없음");

        const W: usize = 64;
        const H: usize = 64;
        // RGBA (R=0x11, G=0x22, B=0x33, A=0xFF)
        let pixels: Vec<u8> = std::iter::repeat([0x11u8, 0x22, 0x33, 0xFF])
            .take(W * H)
            .flatten()
            .collect();
        let src = create_test_source_texture(&device, W as u32, H as u32, &pixels);

        let mut ring = SharedTextureRing::new(device.clone());
        let (handle, epoch) = ring
            .write_from_resource(src.as_raw() as *mut _, 0, W as i32, H as i32)
            .expect("ring write 실패");
        assert_eq!(epoch, 1);
        assert_ne!(handle, 0);

        let readback = open_and_read_on_second_device(handle, W as u32, H as u32);
        assert_eq!(&readback[0..4], &[0x11, 0x22, 0x33, 0xFF], "첫 픽셀 불일치");
        let mid = (H / 2 * W + W / 2) * 4;
        assert_eq!(&readback[mid..mid + 4], &[0x11, 0x22, 0x33, 0xFF], "중앙 픽셀 불일치");

        // 크기 변경 → epoch 증가
        let src2 = create_test_source_texture(&device, 32, 32, &pixels[..32 * 32 * 4]);
        let (_h2, epoch2) = ring
            .write_from_resource(src2.as_raw() as *mut _, 0, 32, 32)
            .expect("ring write(2) 실패");
        assert_eq!(epoch2, 2);

        // 변환기 출력 대상용 래핑 버퍼가 슬롯마다 존재
        let (wrapped, _slot) = ring.acquire(32, 32).expect("acquire 실패");
        assert_eq!(wrapped.n_memory(), 1);
    }
```

- [ ] **Step 2: 테스트 헬퍼 2개 작성** (tests 모듈 안, winapi 직접 사용)

```rust
    use winapi::Interface;
    use winapi::shared::dxgitype::DXGI_SAMPLE_DESC;
    use winapi::um::d3dcommon::D3D_DRIVER_TYPE_HARDWARE;
    use wio::com::ComPtr;

    fn create_test_source_texture(
        device: &SharedGstD3D11Device,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> ComPtr<d3d::ID3D11Texture2D> {
        let desc = d3d::D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: d3d::D3D11_USAGE_DEFAULT,
            BindFlags: d3d::D3D11_BIND_SHADER_RESOURCE,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = d3d::D3D11_SUBRESOURCE_DATA {
            pSysMem: rgba.as_ptr() as *const _,
            SysMemPitch: width * 4,
            SysMemSlicePitch: 0,
        };
        unsafe {
            let mut texture = std::ptr::null_mut();
            let hr = (*device.d3d11_device()).CreateTexture2D(&desc, &init, &mut texture);
            assert_eq!(hr, 0, "소스 텍스처 생성 실패 hr={hr:#x}");
            ComPtr::from_raw(texture)
        }
    }

    fn open_and_read_on_second_device(handle: u64, width: u32, height: u32) -> Vec<u8> {
        unsafe {
            let mut device = std::ptr::null_mut();
            let mut context = std::ptr::null_mut();
            let hr = d3d::D3D11CreateDevice(
                std::ptr::null_mut(),
                D3D_DRIVER_TYPE_HARDWARE,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
                0,
                d3d::D3D11_SDK_VERSION,
                &mut device,
                std::ptr::null_mut(),
                &mut context,
            );
            assert_eq!(hr, 0, "두 번째 디바이스 생성 실패 hr={hr:#x}");
            let device = ComPtr::from_raw(device);
            let context = ComPtr::from_raw(context);

            let mut opened: *mut winapi::ctypes::c_void = std::ptr::null_mut();
            let hr = device.OpenSharedResource(
                handle as winapi::shared::ntdef::HANDLE,
                &d3d::ID3D11Texture2D::uuidof(),
                &mut opened,
            );
            assert_eq!(hr, 0, "OpenSharedResource 실패 hr={hr:#x}");
            let opened = ComPtr::from_raw(opened as *mut d3d::ID3D11Texture2D);

            let desc = d3d::D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: d3d::D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: d3d::D3D11_CPU_ACCESS_READ,
                MiscFlags: 0,
            };
            let mut staging = std::ptr::null_mut();
            let hr = device.CreateTexture2D(&desc, std::ptr::null(), &mut staging);
            assert_eq!(hr, 0, "staging 생성 실패 hr={hr:#x}");
            let staging = ComPtr::from_raw(staging);

            context.CopyResource(staging.as_raw() as *mut _, opened.as_raw() as *mut _);
            let mut mapped = std::mem::zeroed::<d3d::D3D11_MAPPED_SUBRESOURCE>();
            let hr = context.Map(staging.as_raw() as *mut _, 0, d3d::D3D11_MAP_READ, 0, &mut mapped);
            assert_eq!(hr, 0, "Map 실패 hr={hr:#x}");
            let mut out = vec![0u8; (width * height * 4) as usize];
            for row in 0..height as usize {
                let src_row = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
                let dst = &mut out[row * width as usize * 4..(row + 1) * width as usize * 4];
                std::ptr::copy_nonoverlapping(src_row, dst.as_mut_ptr(), width as usize * 4);
            }
            context.Unmap(staging.as_raw() as *mut _, 0);
            out
        }
    }
```

- [ ] **Step 3: 실행해 실패 확인**

Task 1 Step 7과 같은 명령. Expected: `SharedTextureRing` 미정의로 컴파일 실패 (RED).

- [ ] **Step 4: SharedTextureRing 구현**

`interop.rs`에 추가 (SharedGstD3D11Device 아래):
```rust
use winapi::shared::dxgi::IDXGIResource;
use winapi::shared::dxgifmt::DXGI_FORMAT_R8G8B8A8_UNORM;
use winapi::shared::dxgitype::DXGI_SAMPLE_DESC;
use winapi::shared::winerror::{S_FALSE, S_OK};
use winapi::um::d3d11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_QUERY_DESC,
    D3D11_QUERY_EVENT, D3D11_RESOURCE_MISC_SHARED, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    ID3D11Asynchronous, ID3D11Query, ID3D11Resource, ID3D11Texture2D,
};
use winapi::Interface;
use wio::com::ComPtr;

const RING_SLOTS: usize = 4;

struct RingSlot {
    texture: ComPtr<ID3D11Texture2D>,
    shared_handle: u64,
    query: ComPtr<ID3D11Query>,
    /// 링 텍스처를 GstD3D11Memory로 감싼 버퍼 — GstD3D11Converter의 출력 대상.
    /// clone은 미니오브젝트 참조 증가일 뿐이라 저렴.
    wrapped_buffer: gstreamer::Buffer,
}

/// 플레이어별 공유 텍스처 링.
///
/// 사용 순서: acquire(슬롯 확보) → GstD3D11Converter가 슬롯 버퍼에 직접 변환-렌더
/// (또는 폴백 write_from_resource의 복사) → finish(완료 fence 후 핸들 발행).
///
/// 동기화 설계(스펙 §4.4의 keyed mutex를 완료-대기+발행으로 대체, 계획 문서 "의도적
/// 차이 3" 참조): finish()는 GPU 작업 완료를 D3D11_QUERY_EVENT로 확인한 뒤에만 핸들을
/// 반환하므로, 소비자(렌더러)는 발행된 슬롯을 동기화 없이 읽어도 완료된 내용을 본다.
/// 4슬롯 라운드로빈이라 발행된 최신 슬롯이 다시 써지려면 3프레임(30fps 기준 100ms)의
/// 마진이 있다 — 렌더러 lock 유지 시간(정상 <20ms, in-flight 게이트로 백로그 없음)
/// 대비 충분. 문제가 관측되면 MISC_SHARED_KEYEDMUTEX + 매 lock import/destroy로 폴백.
pub struct SharedTextureRing {
    device: Arc<SharedGstD3D11Device>,
    slots: Vec<RingSlot>,
    next_slot: usize,
    epoch: u32,
    width: i32,
    height: i32,
}

// 안전성: ComPtr 원시 포인터는 이 구조체가 단독 소유하며, 컨텍스트 작업은 전부
// 디바이스 락 하에서 수행된다. 스트리밍 스레드 1개에서만 write()가 불린다.
unsafe impl Send for SharedTextureRing {}

impl SharedTextureRing {
    pub fn new(device: Arc<SharedGstD3D11Device>) -> Self {
        SharedTextureRing {
            device,
            slots: Vec::new(),
            next_slot: 0,
            epoch: 0,
            width: 0,
            height: 0,
        }
    }

    /// 다음 슬롯 확보. 반환: (변환 출력 대상으로 쓸 래핑 GstBuffer, 슬롯 인덱스).
    pub fn acquire(&mut self, width: i32, height: i32) -> Option<(gstreamer::Buffer, usize)> {
        if width <= 0 || height <= 0 {
            return None;
        }
        if self.slots.is_empty() || width != self.width || height != self.height {
            self.recreate(width, height)?;
        }
        let slot_index = self.next_slot;
        self.next_slot = (self.next_slot + 1) % self.slots.len();
        Some((self.slots[slot_index].wrapped_buffer.clone(), slot_index))
    }

    /// 슬롯에 대한 GPU 작업 제출 후 호출: 완료 fence 대기 → (공유 핸들, epoch) 발행.
    pub fn finish(&mut self, slot_index: usize) -> Option<(u64, u32)> {
        let slot = self.slots.get(slot_index)?;
        unsafe {
            let _guard = self.device.lock();
            let context = self.device.immediate_context();
            (*context).End(slot.query.as_raw() as *mut ID3D11Asynchronous);
            (*context).Flush();
        }
        // GPU 완료 대기 — 폴마다 락을 짧게 잡아 다른 파이프라인을 막지 않는다.
        loop {
            let hr = unsafe {
                let _guard = self.device.lock();
                let context = self.device.immediate_context();
                (*context).GetData(
                    slot.query.as_raw() as *mut ID3D11Asynchronous,
                    std::ptr::null_mut(),
                    0,
                    0,
                )
            };
            match hr {
                S_OK => break,
                S_FALSE => std::thread::yield_now(),
                _ => {
                    log::warn!("D3D11 video: 완료 쿼리 실패 hr={hr:#x}");
                    return None;
                },
            }
        }
        Some((slot.shared_handle, self.epoch))
    }

    /// 테스트·폴백용: 원본 D3D11 텍스처를 슬롯에 복사(변환 없음, 동일 포맷 전제).
    pub fn write_from_resource(
        &mut self,
        src: *mut ID3D11Resource,
        subresource: u32,
        width: i32,
        height: i32,
    ) -> Option<(u64, u32)> {
        if src.is_null() {
            return None;
        }
        let (_wrapped_buffer, slot_index) = self.acquire(width, height)?;
        let src_box = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width as u32,
            bottom: height as u32,
            back: 1,
        };
        unsafe {
            let _guard = self.device.lock();
            let context = self.device.immediate_context();
            (*context).CopySubresourceRegion(
                self.slots[slot_index].texture.as_raw() as *mut ID3D11Resource,
                0,
                0,
                0,
                0,
                src,
                subresource,
                &src_box,
            );
        }
        self.finish(slot_index)
    }

    fn recreate(&mut self, width: i32, height: i32) -> Option<()> {
        self.slots.clear();
        self.next_slot = 0;
        self.epoch = self.epoch.wrapping_add(1);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width as u32,
            Height: height as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            // ANGLE pbuffer 래핑(EGL_ANGLE_d3d_texture_client_buffer) 요건 충족을 위해
            // RENDER_TARGET 포함.
            BindFlags: D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED,
        };
        let query_desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        let device = self.device.d3d11_device();
        for _ in 0..RING_SLOTS {
            unsafe {
                let mut texture = std::ptr::null_mut();
                let hr = (*device).CreateTexture2D(&desc, std::ptr::null(), &mut texture);
                if hr != S_OK {
                    log::warn!("D3D11 video: 링 텍스처 생성 실패 hr={hr:#x}");
                    self.slots.clear();
                    return None;
                }
                let texture = ComPtr::from_raw(texture);
                let dxgi: ComPtr<IDXGIResource> = match texture.cast() {
                    Ok(dxgi) => dxgi,
                    Err(hr) => {
                        log::warn!("D3D11 video: IDXGIResource 캐스트 실패 hr={hr:#x}");
                        self.slots.clear();
                        return None;
                    },
                };
                let mut handle = std::ptr::null_mut();
                let hr = dxgi.GetSharedHandle(&mut handle);
                if hr != S_OK || handle.is_null() {
                    log::warn!("D3D11 video: GetSharedHandle 실패 hr={hr:#x}");
                    self.slots.clear();
                    return None;
                }
                let mut query = std::ptr::null_mut();
                let hr = (*device).CreateQuery(&query_desc, &mut query);
                if hr != S_OK {
                    log::warn!("D3D11 video: 이벤트 쿼리 생성 실패 hr={hr:#x}");
                    self.slots.clear();
                    return None;
                }
                // 슬롯 텍스처를 GstD3D11Memory로 래핑해 변환기 출력 버퍼로 준비.
                let api = self.device.api();
                let memory_ptr = (api.allocator_alloc_wrapped)(
                    self.device.allocator(),
                    self.device.raw(),
                    texture.as_raw(),
                    (width as usize) * (height as usize) * 4,
                    std::ptr::null_mut(),
                    None,
                );
                if memory_ptr.is_null() {
                    log::warn!("D3D11 video: alloc_wrapped 실패");
                    self.slots.clear();
                    return None;
                }
                let memory: gstreamer::Memory = from_glib_full(memory_ptr);
                let mut wrapped_buffer = gstreamer::Buffer::new();
                wrapped_buffer
                    .get_mut()
                    .expect("새 버퍼는 유일 참조")
                    .append_memory(memory);
                self.slots.push(RingSlot {
                    texture,
                    shared_handle: handle as usize as u64,
                    query: ComPtr::from_raw(query),
                    wrapped_buffer,
                });
            }
        }
        self.width = width;
        self.height = height;
        Some(())
    }
}
```
(winapi 모듈 경로 주의: `DXGI_FORMAT_R8G8B8A8_UNORM`가 `winapi::shared::dxgifmt`에 없다면 `winapi::shared::dxgiformat`이 올바른 모듈명이다 — 컴파일러 에러 메시지에 따라 조정. `D3D11_SDK_VERSION`은 `winapi::um::d3d11::D3D11_SDK_VERSION`.)

- [ ] **Step 5: 테스트 실행 (GREEN)**

Task 1 Step 7과 같은 명령. Expected: 2 passed (`load_api_and_create_shared_device`, `ring_write_and_cross_device_readback`).

- [ ] **Step 6: 커밋**
```bash
git add components/media/backends/gstreamer/render-d3d11/interop.rs
git commit -m '공유 텍스처 링 구현: 변환 출력 슬롯 + 완료 fence + 타 디바이스 판독 테스트'
```

---

### Task 3: PoC 예제 — 실제 mp4 → d3d11upload/convert → D3D11 appsink → 링 → 타 디바이스 판독

스펙 §6-1의 핵심(상호운용 최대 리스크: caps 협상, 디바이스 주입 일치, **GstD3D11Converter의 래핑-버퍼 출력**)을 servoshell 없이 검증한다. "ANGLE 래핑·화면 표시"는 검증된 기존 경로(`create_texture_from_shared_handle` — WebGPU gpu-direct 선례)를 쓰므로 Task 8의 E2E에서 완결한다.

**Files:**
- Create: `components/media/backends/gstreamer/render-d3d11/examples/d3d11_upload_poc.rs`

**Interfaces:**
- Consumes: Task 1·2의 `GstD3D11Api`, `SharedGstD3D11Device`, `SharedTextureRing` + 테스트 헬퍼와 동일한 판독 로직.
- Produces: 없음 (검증 전용 바이너리). 성공 시 stdout에 `POC OK`.

- [ ] **Step 1: 테스트용 mp4 경로 확인**

```powershell
Select-String -Path "D:\2_TechReview\20260606_multigpu_browser\servo\tests\html\video_grid_6x6_play.html" -Pattern "\.mp4"
```
Expected: 월 표출에 쓰는 mp4 상대경로가 나온다. `tests/html/` 기준 절대경로를 Step 3 실행 인자로 사용. (없으면 `Get-ChildItem D:\2_TechReview\20260606_multigpu_browser\servo\tests\html -Recurse -Include *.mp4`로 탐색.)

- [ ] **Step 2: PoC 예제 작성**

`render-d3d11/examples/d3d11_upload_poc.rs`:
```rust
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! PoC: mp4 → decodebin → d3d11upload → appsink(memory:D3D11Memory, 디코더 원 포맷)
//! → 디바이스 일치 assert → GstD3D11Converter로 공유 링 슬롯에 직접 YUV→RGBA 렌더 →
//! 별개 D3D11 디바이스에서 판독해 비검정 확인.
//!
//! 실행: cargo run -p servo-media-gstreamer-render-d3d11 --release --example d3d11_upload_poc -- <mp4 경로>

use std::sync::Arc;

use gstreamer::glib::translate::ToGlibPtr;
use gstreamer::prelude::*;
use servo_media_gstreamer_render_d3d11::ffi::{GstD3D11Api, GstD3D11Converter, GstD3D11Memory};
use servo_media_gstreamer_render_d3d11::{SharedGstD3D11Device, SharedTextureRing};
use winapi::Interface;
use winapi::shared::dxgiformat::DXGI_FORMAT_R8G8B8A8_UNORM;
use winapi::shared::dxgitype::DXGI_SAMPLE_DESC;
use winapi::um::d3d11 as d3d;
use winapi::um::d3dcommon::D3D_DRIVER_TYPE_HARDWARE;
use wio::com::ComPtr;

const FRAMES_TO_CHECK: usize = 60;

fn main() {
    let path = std::env::args().nth(1).expect("사용법: d3d11_upload_poc <mp4 경로>");
    gstreamer::init().expect("gstreamer init 실패");

    let api = GstD3D11Api::load().expect("gstd3d11-1.0-0.dll 로드 실패");
    let device = SharedGstD3D11Device::get_or_create().expect("GstD3D11Device 생성 실패");

    // format 미지정: 디코더 원 포맷(I420/NV12)을 D3D11 메모리로 그대로 받고,
    // RGBA 변환은 아래에서 GstD3D11Converter가 링 슬롯에 직접 수행한다.
    let desc = format!(
        "filesrc location=\"{}\" ! decodebin ! d3d11upload ! \
         video/x-raw(memory:D3D11Memory) ! \
         appsink name=sink sync=false max-buffers=4",
        path.replace('\\', "/")
    );
    let pipeline = gstreamer::parse::launch(&desc)
        .expect("파이프라인 생성 실패")
        .downcast::<gstreamer::Pipeline>()
        .expect("Pipeline 다운캐스트 실패");

    // 프로덕션과 동일: 우리 디바이스를 파이프라인에 주입 (d3d11 엘리먼트가 이걸 쓰는지
    // 아래 GetDevice 비교로 확증)
    let context = device.gst_context().expect("gst_d3d11_context_new 실패");
    pipeline.set_context(&context);

    let appsink = pipeline
        .by_name("sink")
        .expect("appsink 없음")
        .downcast::<gstreamer_app::AppSink>()
        .expect("AppSink 다운캐스트 실패");

    pipeline
        .set_state(gstreamer::State::Playing)
        .expect("PLAYING 전환 실패");

    let mut ring = SharedTextureRing::new(device.clone());
    let mut converter: Option<*mut GstD3D11Converter> = None; // PoC라 해제 생략(프로세스 종료 회수)
    let mut last = None;
    for i in 0..FRAMES_TO_CHECK {
        let sample = appsink
            .pull_sample()
            .unwrap_or_else(|_| panic!("샘플 {i} 획득 실패 (caps 협상 실패 가능성)"));
        let caps = sample.caps().expect("caps 없음");
        let info = gstreamer_video::VideoInfo::from_caps(caps).expect("VideoInfo 실패");
        let buffer = sample.buffer().expect("버퍼 없음");
        let width = info.width() as i32;
        let height = info.height() as i32;

        unsafe {
            let mem_ptr = buffer.peek_memory(0).as_mut_ptr();
            assert_ne!(
                (api.is_d3d11_memory)(mem_ptr),
                0,
                "프레임 {i}: D3D11Memory가 아님 — caps 협상 확인 필요"
            );
            // 디바이스 일치 확증: 디코드 텍스처의 디바이스 == 우리가 주입한 디바이스
            let resource =
                (api.memory_get_resource_handle)(mem_ptr as *mut GstD3D11Memory);
            assert!(!resource.is_null(), "프레임 {i}: resource 핸들 null");
            let mut frame_device: *mut d3d::ID3D11Device = std::ptr::null_mut();
            (*resource).GetDevice(&mut frame_device);
            let frame_device = ComPtr::from_raw(frame_device);
            assert_eq!(
                frame_device.as_raw(),
                device.d3d11_device(),
                "프레임 {i}: 파이프라인이 주입 디바이스를 쓰지 않음 (context 주입 실패)"
            );

            // 변환기 lazy 생성 (in = 디코더 원 포맷, out = RGBA 동일 크기)
            if converter.is_none() {
                let out_info = gstreamer_video::VideoInfo::builder(
                    gstreamer_video::VideoFormat::Rgba,
                    info.width(),
                    info.height(),
                )
                .build()
                .expect("out VideoInfo 실패");
                // VideoInfo가 ToGlibPtr 미구현으로 컴파일 실패하면
                // `&info as *const _ as *const gstreamer_video::ffi::GstVideoInfo`로 조정.
                let conv = (api.converter_new)(
                    device.raw(),
                    info.to_glib_none().0,
                    out_info.to_glib_none().0,
                    std::ptr::null_mut(),
                );
                assert!(!conv.is_null(), "gst_d3d11_converter_new 실패");
                converter = Some(conv);
            }

            // 링 슬롯 확보 → 변환 렌더(YUV→RGBA, 슬롯에 직접) → 완료 fence
            let (out_buffer, slot_index) =
                ring.acquire(width, height).expect("슬롯 확보 실패");
            let ok = (api.converter_convert_buffer)(
                converter.unwrap(),
                buffer.as_mut_ptr(),
                out_buffer.as_mut_ptr(),
            );
            assert_ne!(ok, 0, "프레임 {i}: convert_buffer 실패 (Step 3 실패 분기 참조)");
            let (handle, epoch) = ring.finish(slot_index).expect("완료 fence 실패");
            last = Some((handle, epoch, info.width(), info.height()));
        }
    }

    let (handle, epoch, width, height) = last.expect("프레임 0개");
    let readback = open_and_read_on_second_device(handle, width, height);
    let non_black = readback
        .chunks_exact(4)
        .filter(|px| px[0] > 8 || px[1] > 8 || px[2] > 8)
        .count();
    let pct = non_black as f64 * 100.0 / (width as f64 * height as f64);
    pipeline.set_state(gstreamer::State::Null).ok();

    println!(
        "POC OK: frames={FRAMES_TO_CHECK} size={width}x{height} epoch={epoch} nonblack={pct:.1}%"
    );
    assert!(pct > 30.0, "판독 결과가 거의 검정 ({pct:.1}%) — 복사/동기화 문제");
}

fn open_and_read_on_second_device(handle: u64, width: u32, height: u32) -> Vec<u8> {
    // Task 2 테스트 헬퍼와 동일 구현 (여기 복제 — 예제는 테스트 코드를 import 못 함)
    unsafe {
        let mut device = std::ptr::null_mut();
        let mut context = std::ptr::null_mut();
        let hr = d3d::D3D11CreateDevice(
            std::ptr::null_mut(),
            D3D_DRIVER_TYPE_HARDWARE,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            0,
            d3d::D3D11_SDK_VERSION,
            &mut device,
            std::ptr::null_mut(),
            &mut context,
        );
        assert_eq!(hr, 0, "두 번째 디바이스 생성 실패 hr={hr:#x}");
        let device = ComPtr::from_raw(device);
        let context = ComPtr::from_raw(context);

        let mut opened: *mut winapi::ctypes::c_void = std::ptr::null_mut();
        let hr = device.OpenSharedResource(
            handle as winapi::shared::ntdef::HANDLE,
            &d3d::ID3D11Texture2D::uuidof(),
            &mut opened,
        );
        assert_eq!(hr, 0, "OpenSharedResource 실패 hr={hr:#x}");
        let opened = ComPtr::from_raw(opened as *mut d3d::ID3D11Texture2D);

        let desc = d3d::D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: d3d::D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: d3d::D3D11_CPU_ACCESS_READ,
            MiscFlags: 0,
        };
        let mut staging = std::ptr::null_mut();
        let hr = device.CreateTexture2D(&desc, std::ptr::null(), &mut staging);
        assert_eq!(hr, 0, "staging 생성 실패 hr={hr:#x}");
        let staging = ComPtr::from_raw(staging);

        context.CopyResource(staging.as_raw() as *mut _, opened.as_raw() as *mut _);
        let mut mapped = std::mem::zeroed::<d3d::D3D11_MAPPED_SUBRESOURCE>();
        let hr = context.Map(staging.as_raw() as *mut _, 0, d3d::D3D11_MAP_READ, 0, &mut mapped);
        assert_eq!(hr, 0, "Map 실패 hr={hr:#x}");
        let mut out = vec![0u8; (width * height * 4) as usize];
        for row in 0..height as usize {
            let src_row = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
            let dst = &mut out[row * width as usize * 4..(row + 1) * width as usize * 4];
            std::ptr::copy_nonoverlapping(src_row, dst.as_mut_ptr(), width as usize * 4);
        }
        context.Unmap(staging.as_raw() as *mut _, 0);
        out
    }
}
```
(Task 2 테스트의 판독 헬퍼가 `dxgifmt`로 컴파일됐다면 여기의 `dxgiformat` import를 그에 맞출 것 — 두 파일이 같은 모듈 경로를 쓰도록 통일.)

- [ ] **Step 3: 실행 (핵심 리스크 관문)**

```powershell
. "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\servo_env.ps1"
$ErrorActionPreference = 'Continue'
Set-Location "D:\2_TechReview\20260606_multigpu_browser\servo"
$env:PATH = "C:\gstreamer\1.0\msvc_x86_64\bin;$env:PATH"
$env:GST_PLUGIN_PATH = "C:\gstreamer\1.0\msvc_x86_64\lib\gstreamer-1.0"
cargo run -p servo-media-gstreamer-render-d3d11 --release --example d3d11_upload_poc -- "<Step 1의 mp4 절대경로>"
```
Expected: `POC OK: frames=60 size=1920x1080 epoch=1 nonblack=..%` (30% 초과).

실패 분기(스펙 §7 리스크표 + 폴백 사다리, "의도적 차이 2" 참조):
- "샘플 획득 실패/협상 실패" → `$env:GST_DEBUG="3,d3d11*:5"`로 재실행해 협상 로그 확인 (d3d11upload가 디코더 포맷을 거부하는 경우 caps에 `format=(string){I420,NV12}` 명시 시도).
- "디바이스 불일치 assert" → `pipeline.set_context` 시점 문제. 파이프라인 생성 직후·PLAYING 전에 호출하는지 확인, 필요시 bus의 NeedContext 메시지에 응답하는 sync 핸들러 추가로 전환.
- "convert_buffer 실패" → ⓐ 래핑 버퍼에 VideoMeta 추가: `gstreamer_video::VideoMeta::add(out_buffer.get_mut().unwrap(), gstreamer_video::VideoFrameFlags::empty(), gstreamer_video::VideoFormat::Rgba, w, h)` (링 recreate에서 슬롯별 1회) ⓑ out_info를 `VideoFormat::Bgra`로 전환(이후 태스크 `ImageFormat::RGBA8`→`BGRA8` 일괄 변경 노트) ⓒ 최후: d3d11convert 엘리먼트 복귀 + `write_from_resource` 복사 1회(초안 A 방식 — 파이프라인 caps에 convert+RGBA 복원).
- "nonblack 실패" → 완료 fence 로직 재검토 (Task 2 테스트는 통과했으므로 변환기 출력 대상/뷰 문제 가능성 — `gst_d3d11_memory_get_texture_desc`로 실제 desc를 덤프해 비교).

- [ ] **Step 4: 커밋**
```bash
git add components/media/backends/gstreamer/render-d3d11/examples
git commit -m 'D3D11 업로드 PoC: gst 파이프라인-공유링-타 디바이스 판독 검증'
```

---

### Task 4: `VideoFrameData::D3D11` + `RenderD3D11`(Render 구현) + render.rs 배선

**Files:**
- Modify: `components/media/player/video.rs`
- Modify: `components/media/backends/gstreamer/render-d3d11/lib.rs`
- Modify: `components/media/backends/gstreamer/render.rs:116-156` (windows 플랫폼 분기)
- Modify: `components/media/backends/gstreamer/Cargo.toml` (windows target dep)

**Interfaces:**
- Consumes: Task 1·2 (`SharedGstD3D11Device`, `SharedTextureRing`, `GstD3D11Api`), `Render` trait(`render/lib.rs:22-51`).
- Produces:
  - `VideoFrameD3D11Data { pub shared_handle: u64, pub ring_epoch: u32 }` (Clone/Copy), `VideoFrameData::D3D11(VideoFrameD3D11Data)`, `VideoFrame::is_d3d11() -> bool`, `VideoFrame::get_d3d11_data() -> Option<VideoFrameD3D11Data>` — Task 6이 사용.
  - `RenderD3D11::new() -> Option<RenderD3D11>` — env `SERVO_MEDIA_D3D11_VIDEO` off·사전점검 실패 시 None(기존 CPU 경로 폴백).

- [ ] **Step 1: VideoFrame에 D3D11 변형 추가**

`components/media/player/video.rs`의 `VideoFrameData` enum(line 62-68)을 아래로 교체:
```rust
/// D3D11 공유 텍스처 프레임의 페이로드. shared_handle은 렌더러가
/// OpenSharedResource로 열 수 있는 레거시 DXGI 공유 핸들, ring_epoch는
/// 링 재생성(크기 변경) 세대 — 렌더러측 래핑 캐시 무효화에 쓴다.
#[derive(Clone, Copy, Debug, Eq, MallocSizeOf, PartialEq)]
pub struct VideoFrameD3D11Data {
    pub shared_handle: u64,
    pub ring_epoch: u32,
}

#[derive(Clone, MallocSizeOf)]
pub enum VideoFrameData {
    Raw(#[conditional_malloc_size_of] Arc<Vec<u8>>),
    Yuv(VideoFrameYuvData),
    Texture(u32),
    OESTexture(u32),
    D3D11(VideoFrameD3D11Data),
}
```
그리고 `impl VideoFrame`(`is_yuv` 근처, line 148-152)에 접근자 2개 추가:
```rust
    pub fn is_d3d11(&self) -> bool {
        matches!(self.data, VideoFrameData::D3D11(_))
    }

    pub fn get_d3d11_data(&self) -> Option<VideoFrameD3D11Data> {
        match self.data {
            VideoFrameData::D3D11(data) => Some(data),
            _ => None,
        }
    }
```

- [ ] **Step 2: RenderD3D11 구현**

`render-d3d11/lib.rs`를 아래 내용으로 확장 (기존 모듈 선언 유지):
```rust
use std::env;
use std::sync::{Arc, Mutex};

use gstreamer::glib::translate::ToGlibPtr;
use gstreamer::prelude::*;
use servo_media_gstreamer_render::Render;
use servo_media_player::PlayerError;
use servo_media_player::video::{Buffer, VideoFrame, VideoFrameD3D11Data, VideoFrameData};

use crate::ffi::GstD3D11Converter;

const D3D11_VIDEO_ENV: &str = "SERVO_MEDIA_D3D11_VIDEO";

fn env_flag_enabled(name: &str) -> bool {
    env::var(name).is_ok_and(|value| {
        value == "1"
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
            || value.eq_ignore_ascii_case("on")
    })
}

struct D3D11FrameBuffer {
    data: VideoFrameD3D11Data,
}

impl Buffer for D3D11FrameBuffer {
    fn frame_data(&self) -> Option<VideoFrameData> {
        Some(VideoFrameData::D3D11(self.data))
    }
}

/// GstD3D11Converter 소유 핸들. GstObject 파생이라 g_object_unref로 해제.
struct ConverterHandle(*mut GstD3D11Converter);

impl Drop for ConverterHandle {
    fn drop(&mut self) {
        unsafe { gstreamer::glib::gobject_ffi::g_object_unref(self.0 as *mut _) }
    }
}

// 안전성: 변환기는 스트리밍 스레드 1개에서만 사용하며 PlayerState Mutex로 보호된다.
unsafe impl Send for ConverterHandle {}

struct PlayerState {
    ring: Option<SharedTextureRing>,
    converter: Option<ConverterHandle>,
    /// 변환기 무효화 판정용 — 포맷/크기/colorimetry 변경 감지.
    in_caps: Option<gstreamer::Caps>,
}

pub struct RenderD3D11 {
    device: Arc<SharedGstD3D11Device>,
    // 플레이어당 링+변환기. build_frame은 스트리밍 스레드 1개에서만 불리지만
    // Render는 &self라 내부 가변성 필요.
    state: Mutex<PlayerState>,
}

impl RenderD3D11 {
    /// env 게이트 + 사전 점검. 하나라도 실패하면 None → 기존 CPU(Raw) 경로 폴백.
    pub fn new() -> Option<RenderD3D11> {
        if !env_flag_enabled(D3D11_VIDEO_ENV) {
            return None;
        }
        let options = servo_config::opts::get();
        if options.multiprocess || options.force_ipc {
            log::warn!("D3D11 video: 단일 프로세스 전용 — Raw 경로 폴백");
            return None;
        }
        // 변환은 라이브러리 GstD3D11Converter를 직접 쓰므로 엘리먼트는 d3d11upload만 필요.
        if gstreamer::ElementFactory::find("d3d11upload").is_none() {
            log::warn!("D3D11 video: d3d11upload 플러그인 없음 (gstd3d11.dll 번들 확인) — Raw 경로 폴백");
            return None;
        }
        let device = SharedGstD3D11Device::get_or_create()?;
        log::info!("D3D11 video: 파이프라인별 GPU 업로드 경로 활성");
        Some(RenderD3D11 {
            device,
            state: Mutex::new(PlayerState {
                ring: None,
                converter: None,
                in_caps: None,
            }),
        })
    }
}

impl Render for RenderD3D11 {
    fn is_gl(&self) -> bool {
        false
    }

    fn build_frame(&self, sample: gstreamer::Sample) -> Option<VideoFrame> {
        let buffer = sample.buffer()?;
        if buffer.n_memory() == 0 {
            return None;
        }
        let caps = sample.caps()?;
        let info = gstreamer_video::VideoInfo::from_caps(caps).ok()?;
        let width = info.width() as i32;
        let height = info.height() as i32;
        let api = self.device.api();

        if unsafe { (api.is_d3d11_memory)(buffer.peek_memory(0).as_mut_ptr()) } == 0 {
            log::warn!("D3D11 video: 비 D3D11 메모리 샘플 — 프레임 드롭");
            return None;
        }

        let mut state = self.state.lock().unwrap();
        let state = &mut *state;

        // caps 변경(포맷/크기/색상 정보) 시 변환기 재생성
        if state.in_caps.as_deref() != Some(caps) {
            state.converter = None;
            state.in_caps = Some(caps.to_owned());
        }
        let ring = state
            .ring
            .get_or_insert_with(|| SharedTextureRing::new(self.device.clone()));
        let (out_buffer, slot_index) = ring.acquire(width, height)?;

        if state.converter.is_none() {
            let out_info = gstreamer_video::VideoInfo::builder(
                gstreamer_video::VideoFormat::Rgba,
                info.width(),
                info.height(),
            )
            .build()
            .ok()?;
            // VideoInfo가 ToGlibPtr 미구현으로 컴파일 실패하면
            // `&info as *const _ as *const gstreamer_video::ffi::GstVideoInfo`로 조정.
            let raw = unsafe {
                (api.converter_new)(
                    self.device.raw(),
                    info.to_glib_none().0,
                    out_info.to_glib_none().0,
                    std::ptr::null_mut(),
                )
            };
            if raw.is_null() {
                log::warn!("D3D11 video: converter 생성 실패 — 프레임 드롭");
                return None;
            }
            state.converter = Some(ConverterHandle(raw));
        }
        let converter = state.converter.as_ref()?;

        // YUV→RGBA 변환을 공유 링 슬롯에 직접 렌더 (추가 복사 없음)
        let ok = unsafe {
            (api.converter_convert_buffer)(
                converter.0,
                buffer.as_mut_ptr(),
                out_buffer.as_mut_ptr(),
            )
        };
        if ok == 0 {
            log::warn!("D3D11 video: convert_buffer 실패 — 프레임 드롭");
            return None;
        }
        let (shared_handle, ring_epoch) = ring.finish(slot_index)?;
        // gst 버퍼(sample)는 여기서 스코프를 벗어나며 즉시 풀로 반환된다 — 프레임
        // 수명이 렌더러와 분리되는 것이 링 설계의 핵심 이점.
        VideoFrame::new(
            width,
            height,
            Arc::new(D3D11FrameBuffer {
                data: VideoFrameD3D11Data {
                    shared_handle,
                    ring_epoch,
                },
            }),
        )
    }

    fn build_video_sink(
        &self,
        appsink: &gstreamer::Element,
        pipeline: &gstreamer::Element,
    ) -> Result<(), PlayerError> {
        let bin = gstreamer::Bin::builder().name("servo-d3d11-video-sink").build();
        let upload = gstreamer::ElementFactory::make("d3d11upload")
            .build()
            .map_err(|error| PlayerError::Backend(format!("d3d11upload 생성 실패: {error:?}")))?;

        // format 미지정: 디코더 원 포맷(I420/NV12 등)을 D3D11 메모리로 그대로 받는다.
        // RGBA 변환은 build_frame의 GstD3D11Converter가 링 슬롯에 직접 수행.
        let caps = gstreamer::Caps::builder("video/x-raw")
            .features(["memory:D3D11Memory"])
            .field("pixel-aspect-ratio", gstreamer::Fraction::from((1, 1)))
            .build();
        appsink.set_property("caps", &caps);

        bin.add_many([&upload, appsink])
            .map_err(|error| PlayerError::Backend(format!("bin add 실패: {error:?}")))?;
        upload
            .link(appsink)
            .map_err(|error| PlayerError::Backend(format!("bin link 실패: {error:?}")))?;

        let upload_sink = upload
            .static_pad("sink")
            .ok_or_else(|| PlayerError::Backend("d3d11upload sink pad 없음".to_owned()))?;
        let ghost_pad = gstreamer::GhostPad::builder_with_target(&upload_sink)
            .map_err(|error| PlayerError::Backend(format!("ghost pad 실패: {error:?}")))?
            .name("sink")
            .build();
        bin.add_pad(&ghost_pad)
            .map_err(|error| PlayerError::Backend(format!("ghost pad add 실패: {error:?}")))?;

        // 우리 디바이스를 파이프라인 전체에 주입 — PoC(Task 3)에서 검증된 방식.
        if let Some(context) = self.device.gst_context() {
            pipeline.set_context(&context);
        }
        pipeline.set_property("video-sink", &bin);
        Ok(())
    }
}
```

- [ ] **Step 3: render.rs 플랫폼 배선**

`components/media/backends/gstreamer/Cargo.toml`의 `[target...]` 섹션들(line 47-53) 다음에 추가:
```toml
[target.'cfg(target_os = "windows")'.dependencies]
servo-media-gstreamer-render-d3d11 = { workspace = true }
```

`components/media/backends/gstreamer/render.rs`: RenderDummy 모듈의 cfg(line 116-123)에 `target_os = "windows"` 추가 + windows 전용 모듈 삽입. line 116-123의
```rust
#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "android",
)))]
```
을 아래로 교체:
```rust
#[cfg(target_os = "windows")]
mod platform {
    extern crate servo_media_gstreamer_render_d3d11;
    pub use self::servo_media_gstreamer_render_d3d11::RenderD3D11 as Render;
    use super::*;

    // env SERVO_MEDIA_D3D11_VIDEO 게이트 + 사전 점검은 RenderD3D11::new 내부.
    // None이면 기존 CPU(I420 borrowed) 경로가 그대로 쓰인다.
    pub fn create_render(_gl_context: Box<dyn PlayerGLContext>) -> Option<Render> {
        Render::new()
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "android",
    target_os = "windows",
)))]
```

- [ ] **Step 4: lib.rs 재수출 확인 + 빌드**

`render-d3d11/lib.rs` 말미 재수출에 `RenderD3D11`이 포함되는지 확인 (Step 2 코드가 lib.rs 본문이므로 자동). 빌드 (BACKGROUND, 수분):
```powershell
. "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\servo_env.ps1"
$ErrorActionPreference = 'Continue'
Set-Location "D:\2_TechReview\20260606_multigpu_browser\servo"
.\mach build --release
```
Expected: `Finished`. (media 크레이트 변경이라 다운스트림 재빌드 — 수분 소요.)

- [ ] **Step 5: env-off 회귀 스모크**

기존 표출 스크립트로 env 없이 실행(예: `etc\multigpu\run_video_grid_6x6.ps1` — 없으면 기존 세션에 쓰던 실행 스크립트를 `Get-ChildItem etc\multigpu\*.ps1`로 확인). 36타일 페이지가 이전과 동일하게 재생되는지(로그에 `frame_backend=yuv_i420_external_raw` 유지, `D3D11 video:` 로그 부재) 확인 후 종료:
```powershell
Get-Process servo* -ErrorAction SilentlyContinue | Stop-Process -Force
```
Expected: 기존 동작 그대로 (게이트 off = 코드 경로 무변경).

- [ ] **Step 6: 커밋**
```bash
git add components/media/player/video.rs components/media/backends/gstreamer/render-d3d11/lib.rs components/media/backends/gstreamer/render.rs components/media/backends/gstreamer/Cargo.toml Cargo.lock
git commit -m 'D3D11 비디오 렌더 경로: VideoFrame D3D11 변형 + RenderD3D11 + 플랫폼 배선'
```

---

### Task 5: media-thread — D3D11 레지스트리 + MediaExternalImages 분기 + painter 배관

**Files:**
- Modify: `components/media/media-thread/lib.rs`
- Modify: `components/paint/painter.rs:291`

**Interfaces:**
- Consumes: `RenderingContext::create_texture_from_shared_handle` / `destroy_texture` (`paint_api::rendering_context`), `SurfaceTexture`.
- Produces (Task 6이 사용):
  - `pub struct D3d11VideoFrameInfo { pub shared_handle: u64, pub ring_epoch: u32, pub width: i32, pub height: i32 }`
  - `D3d11VideoFrameExternalImages::allocate_id() -> Option<ExternalImageId>` / `update(id, info)` / `remove(id)`
  - 변경된 시그니처: `WindowGLContext::initialize_image_handler(external_image_handlers: &mut WebRenderExternalImageHandlers, rendering_context: Rc<dyn RenderingContext>)`

- [ ] **Step 1: 레지스트리 추가**

`components/media/media-thread/lib.rs`의 `RawVideoFrameExternalImages` impl 종료 지점(line 115 `}`) 다음에 삽입:
```rust
/// D3D11 GPU 상주 비디오 프레임 레지스트리 (raw YUV 레지스트리의 대칭물).
/// external image ID → 최신 프레임 (latest-wins). 값은 스칼라뿐이라 gst 참조 보유 없음
/// — 프레임 수명은 render-d3d11의 공유 링이 소유한다.
#[derive(Clone, Copy, Debug)]
pub struct D3d11VideoFrameInfo {
    pub shared_handle: u64,
    pub ring_epoch: u32,
    pub width: i32,
    pub height: i32,
}

fn d3d11_video_frames() -> &'static Mutex<FxHashMap<u64, D3d11VideoFrameInfo>> {
    static D3D11_VIDEO_FRAMES: OnceLock<Mutex<FxHashMap<u64, D3d11VideoFrameInfo>>> =
        OnceLock::new();
    D3D11_VIDEO_FRAMES.get_or_init(|| Mutex::new(FxHashMap::default()))
}

fn d3d11_removed_ids() -> &'static Mutex<Vec<u64>> {
    static D3D11_REMOVED_IDS: OnceLock<Mutex<Vec<u64>>> = OnceLock::new();
    D3D11_REMOVED_IDS.get_or_init(|| Mutex::new(Vec::new()))
}

pub struct D3d11VideoFrameExternalImages;

impl D3d11VideoFrameExternalImages {
    pub fn allocate_id() -> Option<ExternalImageId> {
        if opts::get().multiprocess || opts::get().force_ipc {
            return None;
        }
        let mut id_manager = raw_video_external_image_id_manager()?;
        Some(id_manager.next_id(WebRenderImageHandlerType::Media))
    }

    pub fn update(id: ExternalImageId, info: D3d11VideoFrameInfo) {
        d3d11_video_frames().lock().unwrap().insert(id.0, info);
    }

    pub fn remove(id: ExternalImageId) {
        d3d11_video_frames().lock().unwrap().remove(&id.0);
        // 렌더러 스레드의 래핑 캐시는 다음 lock 때 이 목록을 보고 정리한다.
        d3d11_removed_ids().lock().unwrap().push(id.0);
        if let Some(mut id_manager) = raw_video_external_image_id_manager() {
            id_manager.remove(&id);
        }
    }

    fn info_for(id: u64) -> Option<D3d11VideoFrameInfo> {
        d3d11_video_frames().lock().unwrap().get(&id).copied()
    }

    fn take_removed_ids() -> Vec<u64> {
        std::mem::take(&mut *d3d11_removed_ids().lock().unwrap())
    }
}
```

- [ ] **Step 2: MediaExternalImages에 D3D11 분기 추가**

같은 파일. imports(line 17-26 부근)에 추가:
```rust
use std::rc::Rc;

use paint_api::rendering_context::{RenderingContext, SurfaceTexture};
```
`MediaExternalImages` 구조체(line 330-346)를 아래로 교체:
```rust
#[derive(Default)]
struct D3d11TextureCacheEntry {
    ring_epoch: u32,
    /// shared_handle → (SurfaceTexture 유지용, GL 텍스처 id). 링 슬롯이 안정적이라
    /// 플레이어당 최대 4개 — 정상 상태에서 프레임당 재래핑 0.
    textures: FxHashMap<u64, (SurfaceTexture, u32)>,
}

struct MediaExternalImages {
    glplayer_images: Option<GLPlayerExternalImages>,
    locked_raw_planes: FxHashMap<u64, RawVideoPlane>,
    rendering_context: Option<Rc<dyn RenderingContext>>,
    d3d11_texture_cache: FxHashMap<u64, D3d11TextureCacheEntry>,
}

impl MediaExternalImages {
    fn new(
        glplayer_sender: Option<IpcSender<GLPlayerMsg>>,
        rendering_context: Option<Rc<dyn RenderingContext>>,
    ) -> Self {
        Self {
            glplayer_images: glplayer_sender.map(GLPlayerExternalImages::new),
            locked_raw_planes: Default::default(),
            rendering_context,
            d3d11_texture_cache: Default::default(),
        }
    }

    fn purge_removed_d3d11_entries(&mut self) {
        for removed in D3d11VideoFrameExternalImages::take_removed_ids() {
            if let Some(entry) = self.d3d11_texture_cache.remove(&removed) {
                if let Some(rendering_context) = self.rendering_context.as_ref() {
                    for (_, (surface_texture, _)) in entry.textures {
                        rendering_context.destroy_texture(surface_texture);
                    }
                }
            }
        }
    }

    fn lock_d3d11(
        &mut self,
        id: u64,
        info: D3d11VideoFrameInfo,
    ) -> (ExternalImageSource<'_>, Size2D<i32>) {
        let Some(rendering_context) = self.rendering_context.as_ref() else {
            return (ExternalImageSource::Invalid, Size2D::zero());
        };
        let entry = self.d3d11_texture_cache.entry(id).or_default();
        if entry.ring_epoch != info.ring_epoch {
            // 링 재생성(크기 변경) — 이전 세대 래핑 전부 폐기
            for (_, (surface_texture, _)) in std::mem::take(&mut entry.textures) {
                rendering_context.destroy_texture(surface_texture);
            }
            entry.ring_epoch = info.ring_epoch;
        }
        if !entry.textures.contains_key(&info.shared_handle) {
            let size = Size2D::new(info.width, info.height);
            match rendering_context.create_texture_from_shared_handle(info.shared_handle, size) {
                Some((surface_texture, gl_texture, _)) => {
                    entry.textures.insert(info.shared_handle, (surface_texture, gl_texture));
                },
                None => {
                    warn!("D3D11 video: 공유 핸들 import 실패 (id={id})");
                    return (ExternalImageSource::Invalid, Size2D::zero());
                },
            }
        }
        let (_, gl_texture) = entry.textures[&info.shared_handle];
        (
            ExternalImageSource::NativeTexture(gl_texture),
            Size2D::new(info.width, info.height),
        )
    }
}
```
`impl WebRenderExternalImageApi for MediaExternalImages`의 `lock()` 본문 맨 앞(기존 `if let Some(raw_plane) = ...` 이전)에 삽입:
```rust
        // GPU 상주 D3D11 프레임: 렌더러는 캐시된 GL 텍스처를 돌려줄 뿐 업로드하지 않는다.
        if let Some(info) = D3d11VideoFrameExternalImages::info_for(id) {
            self.purge_removed_d3d11_entries();
            return self.lock_d3d11(id, info);
        }
```
`unlock()` 본문 맨 앞에 삽입:
```rust
        // D3D11 캐시는 unlock에서 유지한다 (링 슬롯 재사용 — 폐기는 epoch 변경/제거 시).
        if self.d3d11_texture_cache.contains_key(&id) {
            return;
        }
```

- [ ] **Step 3: initialize_image_handler 시그니처 변경 + 호출처**

같은 파일 line 237과 258을 수정:
```rust
    pub fn initialize_image_handler(
        external_image_handlers: &mut WebRenderExternalImageHandlers,
        rendering_context: Rc<dyn RenderingContext>,
    ) {
```
```rust
        let image_handler = Box::new(MediaExternalImages::new(
            thread_sender,
            Some(rendering_context),
        ));
```
`components/paint/painter.rs:291`을 아래로 교체:
```rust
        WindowGLContext::initialize_image_handler(&mut external_image_handlers, rendering_context.clone());
```

- [ ] **Step 4: 빌드**

Task 4 Step 4와 같은 명령 (BACKGROUND). Expected: `Finished`. (media-thread는 `#![deny(unsafe_code)]` — 추가 코드는 전부 safe라 통과해야 정상.)

- [ ] **Step 5: 커밋**
```bash
git add components/media/media-thread/lib.rs components/paint/painter.rs
git commit -m 'D3D11 비디오 외부이미지: 레지스트리 + 렌더러측 래핑 캐시 + 배관'
```

---

### Task 6: htmlmediaelement — D3D11 프레임 → 단일 ImageKey TextureHandle 발행

**Files:**
- Modify: `components/script/dom/html/htmlmediaelement.rs`

**Interfaces:**
- Consumes: Task 4의 `VideoFrame::is_d3d11()/get_d3d11_data()`, `VideoFrameD3D11Data`; Task 5의 `D3d11VideoFrameExternalImages`, `D3d11VideoFrameInfo`.
- Produces: 없음 (경로 완결).

- [ ] **Step 1: import + 필드 추가**

기존 `use media::{... RawVideoFrameExternalImages ...};` 줄(파일 상단 imports)에 `D3d11VideoFrameExternalImages, D3d11VideoFrameInfo`를 추가. `use servo_media::player::video::{...}` 계열 import에 `VideoFrameD3D11Data` 추가.

`MediaFrameRenderer` 구조체(line 283-284, `yuv_external_ids` 아래)에 필드 추가:
```rust
    #[ignore_malloc_size_of = "WebRender external image identifiers are scalar handles"]
    d3d11_external_id: Option<ExternalImageId>,
```
`MediaFrameRenderer::new`(line 309 `yuv_external_ids: None,` 다음)에 `d3d11_external_id: None,` 추가.

- [ ] **Step 2: reset() 정리 추가**

`reset()`의 `yuv_external_ids` 정리 블록(line 410-413) 다음에 삽입:
```rust
        if let Some(external_id) = self.d3d11_external_id.take() {
            D3d11VideoFrameExternalImages::remove(external_id);
        }
```

- [ ] **Step 3: ensure + render_d3d11_frame 메서드 추가**

`ensure_yuv_external_ids`(line 442-457) 다음에 삽입:
```rust
    fn ensure_d3d11_external_id(&mut self) -> Option<ExternalImageId> {
        if let Some(id) = self.d3d11_external_id {
            return Some(id);
        }
        let id = D3d11VideoFrameExternalImages::allocate_id()?;
        self.d3d11_external_id = Some(id);
        Some(id)
    }
```
`render_yuv_frame`(line 555-715) 다음에 삽입 (구조는 render_yuv_frame의 단일 키 버전):
```rust
    fn render_d3d11_frame(
        &mut self,
        frame: VideoFrame,
        d3d11: VideoFrameD3D11Data,
        rendered_frame_count: u64,
        frame_backend: &'static str,
        inter_frame_ms: Option<f64>,
        mut updates: smallvec::SmallVec<[ImageUpdate; 1]>,
    ) {
        let frame_width = frame.get_width();
        let frame_height = frame.get_height();
        let Some(external_id) = self.ensure_d3d11_external_id() else {
            warn!("Dropping D3D11 media frame because external image ID is unavailable");
            if !updates.is_empty() {
                self.paint_api
                    .update_images(self.webview_id.into(), updates);
            }
            return;
        };

        D3d11VideoFrameExternalImages::update(
            external_id,
            D3d11VideoFrameInfo {
                shared_handle: d3d11.shared_handle,
                ring_epoch: d3d11.ring_epoch,
                width: frame_width,
                height: frame_height,
            },
        );

        let descriptor = ImageDescriptor::new(
            frame_width,
            frame_height,
            ImageFormat::RGBA8,
            ImageDescriptorFlags::empty(),
        );
        let image_data = SerializableImageData::External(ExternalImageData {
            id: external_id,
            channel_index: 0,
            image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
            normalized_uvs: false,
        });

        let image_update_for_log;
        let image_key_for_log;

        let current_frame_is_compatible = self.current_frame.is_some_and(|current_frame| {
            current_frame.width == frame_width
                && current_frame.height == frame_height
                && current_frame.yuv.is_none()
        });

        if current_frame_is_compatible {
            let current_frame = self
                .current_frame
                .as_ref()
                .expect("Current frame should be present");
            image_key_for_log = Some(current_frame.image_key);
            updates.push(ImageUpdate::UpdateImage(
                current_frame.image_key,
                descriptor,
                image_data,
                None,
            ));
            image_update_for_log = "update";

            self.current_frame_holder
                .get_or_insert_with(|| FrameHolder::new(frame.clone()))
                .set(frame);

            if let Some(old_frame) = self.old_frame.take() {
                Self::push_delete_frame_images(&mut updates, old_frame);
            }
        } else {
            if let Some(current_frame) = self.current_frame.take() {
                self.old_frame = Some(current_frame);
            }
            let Some(image_key) = self.generate_image_key() else {
                return;
            };
            image_key_for_log = Some(image_key);
            image_update_for_log = "add";
            self.current_frame = Some(MediaFrame {
                image_key,
                yuv: None,
                width: frame_width,
                height: frame_height,
            });

            self.current_frame_holder = Some(FrameHolder::new(frame));

            updates.push(ImageUpdate::AddImage(
                image_key, descriptor, image_data, false,
            ));
        }

        let delete_update_count = updates
            .iter()
            .filter(|update| matches!(update, ImageUpdate::DeleteImage(..)))
            .count();
        self.log_media_frame(
            rendered_frame_count,
            frame_backend,
            frame_width,
            frame_height,
            image_key_for_log,
            image_update_for_log,
            delete_update_count,
            updates.len(),
            inter_frame_ms,
        );
        self.paint_api
            .update_images(self.webview_id.into(), updates);
    }
```

- [ ] **Step 4: render() 디스패치 연결**

`render()`의 frame_backend 결정부(line 741 `} else if frame.is_gl_texture() {` 직전)에 분기 추가:
```rust
        } else if frame.is_d3d11() {
            "d3d11_texture"
```
YUV 디스패치 블록(line 760-770) **다음**에 삽입:
```rust
        if let Some(d3d11) = frame.get_d3d11_data() {
            self.render_d3d11_frame(
                frame,
                d3d11,
                rendered_frame_count,
                frame_backend,
                inter_frame_ms,
                updates,
            );
            return;
        }
```
(이 분기가 없으면 D3D11 프레임이 BGRA 꼬리로 흘러 `frame.get_data()`에서 panic — 반드시 YUV 블록과 `let descriptor = ...`(line 772) 사이에 위치.)

- [ ] **Step 5: 빌드**

Task 4 Step 4와 같은 명령 (BACKGROUND). Expected: `Finished`.

- [ ] **Step 6: 커밋**
```bash
git add components/script/dom/html/htmlmediaelement.rs
git commit -m 'htmlmediaelement: D3D11 프레임을 단일 ImageKey TextureHandle로 발행'
```

---

### Task 7: gstd3d11 플러그인 번들 (배포/런타임 로딩)

`mach build`가 이 목록으로 DLL을 빌드 디렉터리에 복사하고(build_commands.py:205→350), 런타임 플러그인 로딩도 같은 목록을 쓴다(gstreamer_plugins.rs:8). E2E(Task 8) 전에 완료 필수.

**Files:**
- Modify: `components/servo/gstreamer_plugin_lists/windows.rs.in`
- Modify: `python/servo/gstreamer.py:50-81`

**Interfaces:**
- Consumes/Produces: 없음 (배포 목록). Task 8의 `ElementFactory::find("d3d11upload")` 성공이 수용 기준.

- [ ] **Step 1: 플러그인 목록에 gstd3d11 추가**

`components/servo/gstreamer_plugin_lists/windows.rs.in` 전체를 아래로 교체:
```
[
// gst-plugins-bad
"gstwasapi",
"gstd3d11"
]
```

- [ ] **Step 2: 의존 DLL 추가**

`python/servo/gstreamer.py`의 `GSTREAMER_WIN_DEPENDENCY_LIBS`(line 50-81)에서 `"graphene-1.0-0.dll",` 다음 줄에 삽입 (gstd3d11.dll 임포트 테이블 실측 근거 — 코드 앵커 참조):
```python
    "gstcodecs-1.0-0.dll",
    "gstd3d11-1.0-0.dll",
    "gstd3dshader-1.0-0.dll",
    "gstdxva-1.0-0.dll",
```

- [ ] **Step 3: 빌드 + 복사 확인**

Task 4 Step 4의 빌드 명령 (BACKGROUND) 후:
```powershell
Get-ChildItem "D:\2_TechReview\20260606_multigpu_browser\servo\target\release" -Recurse -Include gstd3d11*.dll,gstd3dshader*.dll,gstdxva*.dll,gstcodecs*.dll | Select-Object FullName
```
Expected: `gstd3d11.dll`(플러그인), `gstd3d11-1.0-0.dll`, `gstd3dshader-1.0-0.dll`, `gstdxva-1.0-0.dll`, `gstcodecs-1.0-0.dll`가 servoshell 실행 파일 디렉터리(또는 그 gstreamer 하위 디렉터리)에 존재. 복사 실패로 빌드가 에러를 내면 메시지의 누락 DLL 이름을 목록과 대조해 수정.

- [ ] **Step 4: 커밋**
```bash
git add components/servo/gstreamer_plugin_lists/windows.rs.in python/servo/gstreamer.py
git commit -m 'Windows 번들에 gstd3d11 플러그인과 의존 라이브러리 추가'
```

---

### Task 8: E2E 1타일 — 화면 표시 + A/B (스펙 §6-1 완결)

**Files:** 없음 (검증 전용; 문제 발견 시 해당 태스크 파일로 돌아가 수정)

**Interfaces:**
- Consumes: Task 1-7 전부.

- [ ] **Step 1: 실행 스크립트/도구 확인**

```powershell
Get-ChildItem "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\run_video*.ps1"
Get-ChildItem "C:\Users\ILWOON~1\AppData\Local\Temp\claude" -Recurse -Include blacktile_check.ps1,gridperf_sweep.ps1,wrprof_capture.ps1 -ErrorAction SilentlyContinue | Select-Object FullName
```
기존 세션 스크립트(blacktile_check 등)가 남아있으면 재사용, 없으면 해당 검증은 로그+사용자 육안으로 대체.

- [ ] **Step 2: env on, 1타일 실행**

기존 표출 스크립트(run_video_grid_6x6.ps1 계열)에 `?cols=1&rows=1` 파라미터와 env를 얹어 실행. 스크립트가 env 주입을 지원하지 않으면 세션 env로 설정 후 실행:
```powershell
$env:SERVO_MEDIA_D3D11_VIDEO = "1"
$env:RUST_LOG = "warn,servo_media_gstreamer=info,media=info,script::dom::html::htmlmediaelement=info"
# (표출 레시피 env — 메모리 §3-f: ENOUGHDATA_BACKOFF/AVDEC_MAX_THREADS/GAPLESS/SYNC_GROUP/WIN_VSYNC — 은
#  1타일 확인에는 불필요; 기본만으로 실행)
& "D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\run_video_grid_6x6.ps1" <스크립트의 cols/rows 지정 방식으로 1x1>
```

- [ ] **Step 3: 로그로 경로 활성 확증**

stderr 로그(스크립트의 로그 파일 위치)에서:
```powershell
Select-String -Path <로그경로> -Pattern "D3D11 video: 파이프라인별 GPU 업로드 경로 활성" | Select-Object -First 2
Select-String -Path <로그경로> -Pattern "frame_backend=d3d11_texture" | Select-Object -First 3
Select-String -Path <로그경로> -Pattern "D3D11 video: .*(실패|폴백)" | Select-Object -First 10
```
Expected: 활성 로그 존재, `frame_backend=d3d11_texture` 존재, 실패/폴백 로그 0건.

- [ ] **Step 4: 화면 검증 (사용자 육안 + 픽셀 체크)**

- blacktile_check.ps1가 있으면 1×1 대상으로 실행 → 비검정 확인.
- **사용자 육안**: 영상이 정상 색(색 반전/틴트 없음), 정상 방향(상하 반전 없음), 정상 재생(멈춤 없음)인지 확인. 프레임카운터 내장 영상이면 카운터 진행 확인.
- 상하 반전이면: 디스패치의 Media uv 플립(코드 앵커 참조)이 원인 — 1순위 수정 = `gst_d3d11_converter_set_transform_matrix`(gstd3d11converter.h:199)로 변환 시 수직 플립 행렬 적용(Task 4 build_frame의 converter 생성 직후). WebGPU 선례상 발생 가능성 낮음.
- 색이 틀리면(R/B 스왑): PoC 폴백 ⓑ대로 out_info `VideoFormat::Bgra` + `ImageFormat::BGRA8`로 일괄 전환.

- [ ] **Step 5: A/B — env off 동일성**

```powershell
Remove-Item Env:\SERVO_MEDIA_D3D11_VIDEO
```
후 같은 1타일 실행 → 로그에 `frame_backend=yuv_i420_external_raw`(기존 경로 복원), `D3D11 video:` 로그 0건. Expected: 게이트가 실제로 경로를 바꾼다는 A/B 확증.

- [ ] **Step 6: (통과 시) 검증 결과 기록 커밋 없음 — 다음 태스크 진행. 실패 시 원인 태스크로 복귀.**

---

### Task 9: 스케일 검증 — 45타일 성공 기준 + 경계 재측정 + 회귀 (스펙 §2/§6)

**Files:** 없음 (검증 전용)

**Interfaces:**
- Consumes: Task 8 통과 상태 + 기존 도구(gridperf_sweep.ps1, PresentMon, blacktile_check.ps1, WR 프로파일러 Ctrl+F12).

측정 전 공통 env (표출 레시피 — 메모리 §3-f/§3-g):
```powershell
$env:SERVO_MEDIA_D3D11_VIDEO = "1"
$env:SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF = "1"
$env:SERVO_GSTREAMER_AVDEC_MAX_THREADS = "1"   # 기존 레시피값; D3D11 경로에선 2~3 재평가 여지
$env:SERVO_MEDIA_GAPLESS_LOOP = "1"
$env:SERVO_MEDIA_SYNC_GROUP = "45"             # 타일 수에 맞춤
$env:SERVO_WIN_VSYNC = "1"
```

- [ ] **Step 1: 4타일 스모크** — 정상 재생 + `frame_backend=d3d11_texture` + 블랙타일 0.

- [ ] **Step 2: 36타일** — 기존 기준(61fps대) 유지 확인 (gridperf_sweep 또는 perf 페이지 `?log=1`).

- [ ] **Step 3: 45타일 — 성공 기준 판정 (스펙 §2)**
  - 시작 슬로우모드 소멸: 동기 그룹 릴리즈 후 **5초 내 60fps 안착** (perf 페이지 로그의 fps 시계열로 판정).
  - lockstep ±1프레임 유지 (프레임카운터 육안/캡처), gapless 루프 경계 저더 0, 스톨 0 (maxGap < 100ms).
  - blacktile_check 통과.

- [ ] **Step 4: WR 프로파일러 — 렌더러 업로드 ≈ 0 확증**

45타일 재생 중 Ctrl+F12 (wrprof_capture.ps1 있으면 활용): `Texture cache update`가 **≈0ms** (기존 27ms/frame 대비). Renderer 프레임 시간이 vsync 예산(16.7ms) 내.

- [ ] **Step 5: 경계 재측정 — 42/45/50/54타일 시작 곡선**

각 구성으로 시작 슬로우모드 탈출 시간 측정(기존 42타일 12초+ 고착 대비). Expected: **42 경계 소멸**, 50~54에서도 시작 매끄러움 (SYNC_GROUP 값은 타일 수에 맞출 것). 54 초과에서 새 병목이 보이면 그 지점을 기록만 하고 이번 범위 종료.

- [ ] **Step 6: 회귀 — env off 기준 재현**

env 게이트만 제거하고 36타일·45타일 시작 곡선 재측정 → 기존 지표(36타일 61fps / 45타일 기존 시작 곡선) 재현. Expected: off 경로 무영향 확증.

- [ ] **Step 7: 자원 점검** — 45타일에서 servoshell 메모리 안정(링 45×4×8.3MB ≈ 1.5GB VRAM 추가는 예상 범위), 핸들 누수 없음(작업 관리자 GPU 메모리·핸들 수 관찰, 5분).

---

### Task 10: 메모리 갱신 + 마무리

**Files:**
- Modify: `C:\Users\ilwoonam75\.claude\projects\D--2-TechReview-20260606-multigpu-browser\memory\video-grid-play-heartbeat.md`
- Modify: `C:\Users\ilwoonam75\.claude\projects\D--2-TechReview-20260606-multigpu-browser\memory\MEMORY.md`

- [ ] **Step 1: 메모리 기록**

`video-grid-play-heartbeat.md`에 §3-k 추가: D3D11 파이프라인별 업로드 구현 결과 — 실측 수치(45타일 시작 곡선, Texture cache update, 경계 재측정 값), 커밋 sha 목록, 스펙과의 차이 3건(클라이언트 버퍼 래핑/자체 링/완료대기 동기화)이 실제로 채택된 형태, 발견된 함정(있다면: winapi 모듈 경로, caps 협상, 방향/색 등). **플레이스홀더 금지 — 실측값으로 채울 것.** `MEMORY.md`의 해당 훅 라인도 "다음 시작점"을 갱신 (D3D11 구현 완료 → 다음 과제: 멀티GPU 어댑터 정합(스펙 §4.5) 또는 HW 디코드 후속).

- [ ] **Step 2: 최종 커밋 확인**

```bash
git log --oneline -8
git status
```
Expected: Task 1-7 커밋 존재, 작업 트리 클린(계획 문서 자체는 이미 커밋됨).

---

## Self-Review

**Spec coverage (스펙 섹션 → 태스크):**
- §2 성공 기준 5항목 → Task 9 Step 3(슬로우모드/lockstep/gapless/스톨/블랙), Step 4(Texture cache ≈0), Step 5(경계 확장), Step 6(env off 동일). ✓
- §4.1 파이프라인 모양 → Task 4 build_video_sink(d3d11upload→appsink D3D11 caps). d3d11convert 엘리먼트와 풀 공유 할당은 의도적 차이 2로 대체(라이브러리 GstD3D11Converter가 링 슬롯에 직접 변환-렌더, 복사 0회) — 문서 상단에 근거 명시, RGBA 1장 전달·WR YUV 셰이더 의존 제거라는 §4.1의 목적은 동일하게 달성. ✓
- §4.1 명시적 GstD3D11Device 생성/주입 → Task 1(디바이스) + Task 4(set_context) + PoC 디바이스 일치 assert(Task 3). ✓
- §4.2 핸드오프 표(레지스트리/단일 ImageKey/TextureHandle/lock→NativeTexture) → Task 5(레지스트리+lock)+Task 6(단일 키 발행). 코얼레싱·in-flight 게이트·gapless·동기그룹 직교 → 무변경(어느 태스크도 painter의 해당 로직을 건드리지 않음). ✓
- §4.3 래핑+캐시 → 의도적 차이 1(검증된 클라이언트 버퍼 경로) + Task 5의 epoch 기반 캐시(재래핑 방지 — 스펙 §4.3의 캐시 무효화 요구는 ring_epoch+removed_ids로 충족). ✓
- §4.4 동기화/수명 → 의도적 차이 3(완료대기+발행, keyed mutex 폴백 문서화). "acquire 실패 시 이전 프레임 유지"에 대응하는 상황(쓰기 중 슬롯 읽기)은 발행-후-읽기 설계상 발생하지 않음. 수명: 링이 소유, gst 버퍼 즉시 반환(Task 4 주석), 렌더러 캐시가 ANGLE측 참조 유지. ✓
- §4.5 멀티GPU → 비활성이되 `gst_d3d11_device_new_for_adapter_luid` FFI 확보(Task 1) + 주입 지점 주석. 후속 단계로 명시. ✓
- §5 게이트/폴백 → env 게이트(Task 4 RenderD3D11::new), 사전 점검 실패 시 플레이어 단위 Raw 폴백(create_render→None), A/B 검증(Task 8 Step 5). 협상 실패 런타임 폴백은 사전 점검(플러그인/DLL/디바이스)으로 발생 확률을 낮추는 방식 — d3d11upload는 임의 video/x-raw를 받으므로 잔여 위험은 낮음. ✓
- §6 검증 단계 1-4 → PoC(Task 3, ANGLE/표시 제외분은 Task 8로 완결 — §6-1의 "화면 표시까지"를 두 태스크로 분할, 근거: ANGLE 래핑이 이미 WebGPU로 검증된 기존 코드라 PoC 리스크가 아님), 스윕(Task 9 Step 1-3), 회귀(Step 6), 경계(Step 5). §6-5 멀티GPU는 후속. ✓
- §7 리스크표 → 상호운용(Task 3 PoC 최우선 + 실패 분기 명시), 번들(Task 7, 의존 DLL 실측), VideoFrame 변형(Task 4, 기존 GL 변형 패턴 준수), 디바이스 공유(어댑터당 1개, Task 1), 렌더러 대기(발행-후-읽기로 원천 제거). ✓

**Placeholder scan:** TBD/TODO/"적절히 처리" 없음. 모든 코드 스텝에 실제 코드. 미확정 지점 4곳은 명시적 검증/폴백 분기로 처리(winapi `dxgifmt` vs `dxgiformat` 모듈명, `VideoInfo`의 ToGlibPtr 구현 여부 — 캐스트 대안 주석, allocator 획득 find→g_object_new 폴백, convert_buffer 실패 사다리 ⓐⓑⓒ) — 실행 시 컴파일러/PoC가 판정. Task 10의 실측값 기입은 실행 시 채움을 명시. ✓

**Type consistency:**
- `SharedTextureRing::acquire(i32, i32) -> Option<(gstreamer::Buffer, usize)>` / `finish(usize) -> Option<(u64, u32)>`: Task 2 정의 = Task 3 PoC 호출 = Task 4 build_frame 호출. `write_from_resource`: Task 2 정의 = Task 2 테스트 호출(폴백용). ✓
- `GstD3D11Api::{converter_new, converter_convert_buffer, allocator_alloc_wrapped, allocator_get_type}`: Task 1 정의 = Task 2 recreate(alloc_wrapped) = Task 3/4 converter 호출. ✓
- `VideoFrameD3D11Data { shared_handle: u64, ring_epoch: u32 }`: Task 4 정의 = Task 6 사용(`d3d11.shared_handle`, `d3d11.ring_epoch`). ✓
- `D3d11VideoFrameInfo { shared_handle, ring_epoch, width, height }`: Task 5 정의 = Task 6 `update()` 호출 필드 일치. ✓
- `initialize_image_handler(&mut ..., Rc<dyn RenderingContext>)`: Task 5 정의 = painter.rs 호출(유일 호출처 확인됨). ✓
- `create_texture_from_shared_handle(u64, Size2D<i32>) -> Option<(SurfaceTexture, u32, ...)>`: 실측 시그니처(rendering_context.rs:94) = Task 5 사용. ✓
- env 이름 `SERVO_MEDIA_D3D11_VIDEO` 전 태스크 동일(스펙 §5와 일치). ✓
- 크레이트/모듈명 `servo-media-gstreamer-render-d3d11` / `servo_media_gstreamer_render_d3d11`: Cargo.toml·render.rs extern·PoC use 일치. ✓

**실행 시 최대 리스크 집중점:** Task 3 (gst d3d11 ↔ 자체 링 상호운용 + GstD3D11Converter의 래핑-버퍼 출력). 여기서 막히면 PoC 실패 분기(VideoMeta → BGRA → d3d11convert 엘리먼트+복사 1회 폴백)를 순차 판정 — Task 4 이후로 넘어가기 전에 반드시 통과할 것. 폴백 ⓒ로 확정될 경우 Task 4의 build_video_sink에 convert 엘리먼트를 복원하고 build_frame을 `write_from_resource` 경로로 바꾼다(링·레지스트리·핸들러·htmlmediaelement는 무변경).

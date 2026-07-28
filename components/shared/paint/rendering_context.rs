/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![deny(unsafe_code)]

use std::cell::{Cell, RefCell, RefMut};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;

use dpi::PhysicalSize;
use embedder_traits::RefreshDriver;
use euclid::default::{Rect, Size2D as UntypedSize2D};
use euclid::{Point2D, Size2D};
use gleam::gl::{self, Gl};
use glow::{HasContext, NativeFramebuffer};
use image::RgbaImage;
use log::{debug, info, trace, warn};
use raw_window_handle::{DisplayHandle, WindowHandle};
#[cfg(windows)]
use raw_window_handle::RawWindowHandle;
pub use surfman::Error;
// Re-exported so external-image consumers (e.g. the WebGPU GPU-direct present path) can hold
// the `SurfaceTexture` returned by `create_texture_from_shared_handle` without depending on
// surfman directly.
pub use surfman::SurfaceTexture;
use surfman::chains::{PreserveBuffer, SwapChain};
use surfman::{
    Adapter, Connection, Context, ContextAttributeFlags, ContextAttributes, Device, GLApi,
    GLVersion, NativeContext, NativeWidget, Surface, SurfaceAccess, SurfaceInfo, SurfaceType,
};
#[cfg(all(target_os = "windows", feature = "no-wgl"))]
use winapi::Interface;
#[cfg(all(target_os = "windows", feature = "no-wgl"))]
use winapi::shared::dxgi::{self, IDXGIAdapter, IDXGIFactory1};
#[cfg(all(target_os = "windows", feature = "no-wgl"))]
use winapi::shared::winerror;
#[cfg(all(target_os = "windows", feature = "no-wgl"))]
use wio::com::ComPtr;
use webrender_api::units::{DeviceIntRect, DeviceIntSideOffsets, DevicePixel};

/// Native Compositor(DirectComposition) 게이트(`SERVO_COMPOSITOR_DCOMP`) 판정의 단일
/// 정본 — surfman의 공개 함수를 재수출한다(surfman은 같은 판정으로 창 서피스 DComp 속성
/// 억제 + present-path-fast 비활성). 중복 env 파싱 금지.
#[cfg(all(target_os = "windows", feature = "no-wgl"))]
pub use surfman::dcomp_native_compositor_requested;

/// ANGLE(no-wgl) 밖에서는 DComp 네이티브 컴포지터가 성립하지 않으므로 항상 false
/// (surfman의 angle 백엔드가 없어 정본 함수도 존재하지 않는다).
#[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
pub fn dcomp_native_compositor_requested() -> bool {
    false
}

/// `SERVO_VIDEO_ESCAPE` 게이트 모드. `external`만 유효(PREFER|SUPPORTS) — 그 외 토큰은
/// 전부 Off로 취급한다(과거 `native` 진단 모드는 미표출 결함 확정으로 제거됨).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoEscapeMode {
    Off,
    External,
}

pub fn parse_video_escape_token(value: Option<&str>) -> VideoEscapeMode {
    match value {
        Some("external") => VideoEscapeMode::External,
        _ => VideoEscapeMode::Off,
    }
}

/// DComp 네이티브 컴포지터 게이트가 켜져 있을 때만 발효. 프로세스당 1회 캐시.
pub fn video_escape_mode() -> VideoEscapeMode {
    static MODE: std::sync::OnceLock<VideoEscapeMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        if !dcomp_native_compositor_requested() {
            return VideoEscapeMode::Off;
        }
        parse_video_escape_token(std::env::var("SERVO_VIDEO_ESCAPE").ok().as_deref())
    })
}

/// A GL texture created by wrapping a D3D11 texture via `EGL_ANGLE_image_d3d11_texture`
/// (media D3D11 interop for WR YUV direct sampling). Returned by
/// [`RenderingContext::wrap_d3d11_texture_as_gl_texture`] and consumed by
/// [`RenderingContext::destroy_d3d11_gl_wrap`] to tear the wrap down again.
#[derive(Clone, Copy, Debug)]
pub struct D3d11GlWrappedTexture {
    /// The `EGLImage` backing `gl_texture`, encoded as `usize` (opaque handle).
    pub egl_image: usize,
    /// The GL texture name that samples the wrapped D3D11 texture.
    pub gl_texture: u32,
}

/// The `RenderingContext` trait defines a set of methods for managing
/// an OpenGL or GLES rendering context.
/// Implementors of this trait are responsible for handling the creation,
/// management, and destruction of the rendering context and its associated
/// resources.
pub trait RenderingContext {
    /// Prepare this [`RenderingContext`] to be rendered upon by Servo. For instance,
    /// by binding a framebuffer to the current OpenGL context.
    fn prepare_for_rendering(&self) {}
    /// Read the contents of this [`Renderingcontext`] into an in-memory image. If the
    /// image cannot be read (for instance, if no rendering has taken place yet), then
    /// `None` is returned.
    ///
    /// In a double-buffered [`RenderingContext`] this is expected to read from the back
    /// buffer. That means that once Servo renders to the context, this should return those
    /// results, even before [`RenderingContext::present`] is called.
    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage>;
    /// Get the current size of this [`RenderingContext`].
    fn size(&self) -> PhysicalSize<u32>;
    /// Get the current size of this [`RenderingContext`] as [`Size2D`].
    fn size2d(&self) -> Size2D<u32, DevicePixel> {
        let size = self.size();
        Size2D::new(size.width, size.height)
    }
    /// Resizes the rendering surface to the given size.
    fn resize(&self, size: PhysicalSize<u32>);
    /// Presents the rendered frame to the screen. In a double-buffered context, this would
    /// swap buffers.
    fn present(&self);
    /// 이 컨텍스트가 렌더한 서피스 중 실제로 화면에 표출되는 sub-rect를 정의하는 가드밴드
    /// 여백. device px, **top-left 기준**. 월 타일에서 `overlapPx`로 확장한 render rect와
    /// visible rect의 차이이며, 비월 모드는 zero(트레잇 기본값).
    ///
    /// 소비자는 둘이고 y 규약이 서로 다르다:
    /// - 오프스크린→창 blit(servoshell `gui.rs`): GL 프레임버퍼는 bottom-left 원점이라
    ///   source rect y 원점에 `bottom`을 쓴다.
    /// - DComp 네이티브 컴포지터: top-left 원점이라 root visual 오프셋에 `-top`을 쓴다.
    fn present_inset(&self) -> DeviceIntSideOffsets {
        DeviceIntSideOffsets::zero()
    }
    /// [`RenderingContext::present_inset`]을 설정한다. 월 타일 창은 non-resizable이므로
    /// servoshell이 창 생성 시 1회만 호출한다.
    fn set_present_inset(&self, _inset: DeviceIntSideOffsets) {}
    /// Makes the context the current OpenGL context for this thread.
    /// After calling this function, it is valid to use OpenGL rendering
    /// commands.
    fn make_current(&self) -> Result<(), Error>;
    /// Returns the `gleam` version of the OpenGL or GLES API.
    fn gleam_gl_api(&self) -> Rc<dyn gleam::gl::Gl>;
    /// Returns the OpenGL or GLES API.
    fn glow_gl_api(&self) -> Arc<glow::Context>;
    /// Creates a texture from a given surface and returns the surface texture,
    /// the OpenGL texture object, and the size of the surface. Default to `None`.
    fn create_texture(
        &self,
        surface: Surface,
    ) -> Result<(SurfaceTexture, u32, UntypedSize2D<i32>), Surface> {
        Err(surface)
    }
    /// Destroys the texture and returns the surface. Default to `None`.
    fn destroy_texture(&self, _surface_texture: SurfaceTexture) -> Option<Surface> {
        None
    }
    /// Wrap an external DXGI shared-resource handle (e.g. a D3D12 texture the WebGPU thread
    /// shares for GPU-direct present) as a GL texture on this context's device. Returns the
    /// [`SurfaceTexture`] (keep alive while sampling), the GL texture name, and the size.
    /// Default `None`; only the surfman/ANGLE (Windows) backend implements it.
    fn create_texture_from_shared_handle(
        &self,
        _handle: u64,
        _size: UntypedSize2D<i32>,
    ) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)> {
        None
    }
    /// AddRef된 프로세스 전역 `ID3D11Device` 포인터를 `usize`로 인코딩해 반환한다. 미디어
    /// D3D11 인터롭(WR YUV 직접 샘플)이 이 값을 전역 레지스트리에 저장해 프로세스 수명
    /// 동안 유지하므로, 호출자는 반환값을 Release하지 않는다(의도적인 프로세스 수명 누수).
    /// Default `None`; only the surfman/ANGLE (Windows) backend implements it.
    fn media_d3d11_device_handle(&self) -> Option<usize> {
        None
    }
    /// DYNAMIC D3D11 텍스처를 `WRITE_DISCARD`로 매핑한다. 반환은 (데이터 포인터,
    /// RowPitch). **렌더러(ANGLE GL 호출) 스레드에서만 호출.** Default `None`.
    fn map_d3d11_dynamic_texture(&self, _texture: usize) -> Option<(usize, u32)> {
        None
    }
    /// [`RenderingContext::map_d3d11_dynamic_texture`]의 짝. Default no-op.
    fn unmap_d3d11_texture(&self, _texture: usize) {}
    /// D3D11 텍스처(이 컨텍스트의 디바이스 소속)를 EGLImage로 GL 텍스처에 바인딩한다
    /// (`EGL_ANGLE_image_d3d11_texture`). Default `None`.
    fn wrap_d3d11_texture_as_gl_texture(&self, _texture: usize) -> Option<D3d11GlWrappedTexture> {
        None
    }
    /// [`RenderingContext::wrap_d3d11_texture_as_gl_texture`]의 짝. Default no-op.
    fn destroy_d3d11_gl_wrap(&self, _wrap: D3d11GlWrappedTexture) {}
    /// D3D11 텍스처에 대해 `IUnknown::Release`를 호출한다(공유 텍스처 링 해체용).
    /// Default no-op.
    fn release_d3d11_texture(&self, _texture: usize) {}
    /// Copy `rows` rows of `row_bytes` bytes each from `src` into a mapped D3D11
    /// texture at `dst_ptr` honoring `dst_pitch`. Safe wrapper for callers that
    /// forbid unsafe code; the caller guarantees dst is a live mapping from
    /// map_d3d11_dynamic_texture with pitch `dst_pitch` and at least `rows` rows.
    fn copy_rows_to_mapped(
        &self,
        _dst_ptr: usize,
        _dst_pitch: u32,
        _src: &[u8],
        _row_bytes: usize,
        _rows: usize,
    ) {
    }
    /// Native compositor(DirectComposition) 인터롭: 이 컨텍스트가 붙은 창의 HWND.
    /// Windows 창 컨텍스트에서만 Some.
    fn window_hwnd(&self) -> Option<usize> {
        None
    }
    /// Native compositor(DirectComposition)가 이 창에서 실제로 발동했는지를 painter가
    /// 알려준다(`dcomp_compositor::maybe_create` 성공 시 `true`). env 게이트만으로
    /// `present()`의 스킵 여부를 판단하면, env는 켜졌지만 발동에 실패해 Draw 컴포지터로
    /// 폴백한 경우 present가 잘못 스킵돼 블랭크 윈도우가 된다 — 이 신호로 실제 상태를
    /// 반영한다. Default no-op; `WindowRenderingContext`만 의미 있게 구현한다.
    ///
    /// 계약: `true`로의 전이는 발동 성공 시(`maybe_create` 성공) 단 1회뿐이며, 폴백
    /// 경로는 초기값에 암묵 의존하지 않고 항상 명시적으로 `false`를 유지/설정한다.
    fn set_dcomp_native_active(&self, _active: bool) {}
    /// DComp 네이티브 컴포지터가 이 창에서 실제로 발동 중인지 — `set_dcomp_native_active`의
    /// 짝(getter). servoshell egui 통합이 게이트 on에서 콘텐츠 없는 오프스크린 프레임버퍼를
    /// 창 백버퍼로 매 리페인트 blit하는 잔여 트래픽을 스킵할지 판단할 때 읽는다. Default
    /// `false`; `WindowRenderingContext`만 자신의 `Cell`을 읽어 의미 있게 구현하고,
    /// `OffscreenRenderingContext`는 부모에 위임한다.
    fn dcomp_native_active(&self) -> bool {
        false
    }
    /// task-12b: DComp 네이티브 경로에서 사용자가 창을 드래그-리사이즈 중인지 painter가
    /// 컴포지터에 알린다. painter가 드래그 첫 크기 변경에서 `true`, Task 12 디바운스가
    /// 정착할 때 `false`로 설정한다. 컴포지터(`DCompNativeCompositor::end_frame`)는 이 값이
    /// `true`인 동안 (1) 모든 승격 스왑체인을 즉시 가상 서피스로 강등하고 (2) 승격·regen을
    /// 억제한다 — 드래그마다 스왑체인이 재생성되며 content_attached가 리셋돼 새 full-coverage
    /// Present까지 표시가 보류되는(=비디오 블랙) 병리를 회피하고, 임의 지오메트리를 매 프레임
    /// 처리하는 가상 서피스(BeginDraw)가 드래그를 운반하게 한다. Default no-op;
    /// `WindowRenderingContext`만 자신의 `Cell`을 읽고, `OffscreenRenderingContext`는 위임한다.
    fn set_dcomp_resize_active(&self, _active: bool) {}
    /// `set_dcomp_resize_active`의 짝(getter). 컴포지터가 매 end_frame에서 읽어 리사이즈
    /// 중 강등/억제 게이트를 판단한다. Default `false`.
    fn dcomp_resize_active(&self) -> bool {
        false
    }
    /// ANGLE의 D3D11 디바이스 raw 포인터. AddRef 하지 않는다 — 수명은 이
    /// 렌더링 컨텍스트가 보유하므로 컨텍스트보다 오래 들고 있으면 안 된다.
    fn angle_d3d11_device_ptr(&self) -> Option<usize> {
        None
    }
    /// RENDER_TARGET D3D 텍스처를 그리기용 EGL pbuffer로 래핑. 반환값=EGLSurface.
    fn create_render_pbuffer_from_d3d_texture(
        &self,
        _texture: usize,
        _size: UntypedSize2D<i32>,
    ) -> Option<usize> {
        None
    }
    /// pbuffer를 현재 draw/read 서피스로 바인딩(컨텍스트 유지). 성공 여부 반환.
    fn make_render_pbuffer_current(&self, _egl_surface: usize) -> bool {
        false
    }
    /// [`RenderingContext::create_render_pbuffer_from_d3d_texture`]의 짝.
    fn destroy_render_pbuffer(&self, _egl_surface: usize) {}
    /// The connection to the display server for WebGL. Default to `None`.
    fn connection(&self) -> Option<Connection> {
        None
    }
    /// Return the [`RefreshDriver`] for this [`RenderingContext`]. If `None` is returned,
    /// then the default timer-based [`RefreshDriver`] will be used.
    fn refresh_driver(&self) -> Option<Rc<dyn RefreshDriver>> {
        None
    }
    /// The wall-layout GPU index requested by the embedder for this context, if any.
    ///
    /// This is currently diagnostic plumbing for ServoShell tiled-present work. Backends that can
    /// bind a context to a specific adapter should use this value when creating the adapter/device.
    fn requested_gpu_index(&self) -> Option<usize> {
        None
    }
}

pub fn create_adapter_for_requested_gpu(
    connection: &Connection,
    requested_gpu_index: Option<usize>,
) -> Result<Adapter, Error> {
    #[cfg(all(target_os = "windows", feature = "no-wgl"))]
    if let Some(gpu_index) = requested_gpu_index {
        match create_dxgi_adapter_by_index(gpu_index) {
            Ok(adapter) => {
                info!("Selected DXGI adapter index {gpu_index} for requested target GPU");
                return Ok(adapter);
            },
            Err(error) => {
                warn!(
                    "Could not select requested DXGI adapter index {gpu_index}: {error:?}; \
                     falling back to surfman default adapter"
                );
            },
        }
    }

    #[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
    if let Some(gpu_index) = requested_gpu_index {
        info!(
            "Requested target GPU index {gpu_index}; current surfman backend exposes only the \
             default adapter selection path"
        );
    }

    connection.create_adapter()
}

#[cfg(all(target_os = "windows", feature = "no-wgl"))]
#[expect(unsafe_code)]
fn create_dxgi_adapter_by_index(gpu_index: usize) -> Result<Adapter, Error> {
    use std::os::raw::c_void;
    use std::ptr;

    let gpu_index = u32::try_from(gpu_index).map_err(|_| Error::NoAdapterFound)?;
    unsafe {
        let mut dxgi_factory: *mut IDXGIFactory1 = ptr::null_mut();
        let result = dxgi::CreateDXGIFactory1(
            &IDXGIFactory1::uuidof(),
            &mut dxgi_factory as *mut *mut IDXGIFactory1 as *mut *mut c_void,
        );
        if !winerror::SUCCEEDED(result) {
            return Err(Error::Failed);
        }
        assert!(!dxgi_factory.is_null());
        let dxgi_factory = ComPtr::from_raw(dxgi_factory);

        let mut dxgi_adapter_1 = ptr::null_mut();
        let result = (*dxgi_factory).EnumAdapters1(gpu_index, &mut dxgi_adapter_1);
        if !winerror::SUCCEEDED(result) {
            return Err(Error::NoAdapterFound);
        }
        assert!(!dxgi_adapter_1.is_null());
        let dxgi_adapter_1 = ComPtr::from_raw(dxgi_adapter_1);

        let mut dxgi_adapter: *mut IDXGIAdapter = ptr::null_mut();
        let result = (*dxgi_adapter_1).QueryInterface(
            &IDXGIAdapter::uuidof(),
            &mut dxgi_adapter as *mut *mut IDXGIAdapter as *mut *mut c_void,
        );
        if !winerror::SUCCEEDED(result) {
            return Err(Error::Failed);
        }
        assert!(!dxgi_adapter.is_null());

        Ok(Adapter::from_dxgi_adapter(ComPtr::from_raw(dxgi_adapter)))
    }
}

/// Read the DXGI adapter LUID `(HighPart, LowPart)` for a wall-layout GPU index.
///
/// This is the same per-physical-GPU key the WebGPU fan-out reads from wgpu's DX12
/// adapters (`adapter_as_hal -> GetDesc1().AdapterLuid`), so it lets the compositor map a
/// painter (which selects its GPU by DXGI `EnumAdapters1` index) to the matching per-GPU
/// WebGPU device. Returns `None` off Windows or if the adapter can't be queried.
#[cfg(all(target_os = "windows", feature = "no-wgl"))]
#[expect(unsafe_code)]
pub fn dxgi_luid_for_gpu_index(gpu_index: usize) -> Option<(i32, u32)> {
    use std::os::raw::c_void;
    use std::ptr;

    use winapi::shared::dxgi::{DXGI_ADAPTER_DESC1, IDXGIAdapter1};

    let gpu_index = u32::try_from(gpu_index).ok()?;
    unsafe {
        let mut dxgi_factory: *mut IDXGIFactory1 = ptr::null_mut();
        let result = dxgi::CreateDXGIFactory1(
            &IDXGIFactory1::uuidof(),
            &mut dxgi_factory as *mut *mut IDXGIFactory1 as *mut *mut c_void,
        );
        if !winerror::SUCCEEDED(result) || dxgi_factory.is_null() {
            return None;
        }
        let dxgi_factory = ComPtr::from_raw(dxgi_factory);

        let mut adapter1: *mut IDXGIAdapter1 = ptr::null_mut();
        let result = (*dxgi_factory).EnumAdapters1(gpu_index, &mut adapter1);
        if !winerror::SUCCEEDED(result) || adapter1.is_null() {
            return None;
        }
        let adapter1 = ComPtr::from_raw(adapter1);

        let mut desc: DXGI_ADAPTER_DESC1 = std::mem::zeroed();
        if !winerror::SUCCEEDED((*adapter1).GetDesc1(&mut desc)) {
            return None;
        }
        Some((desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart))
    }
}

#[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
pub fn dxgi_luid_for_gpu_index(_gpu_index: usize) -> Option<(i32, u32)> {
    None
}

/// A physical display (DXGI output) paired with the GPU adapter that drives it.
///
/// `adapter_index` is the DXGI `EnumAdapters1` order — the same value
/// `create_dxgi_adapter_by_index` / a `requested_gpu_index` consume — so a tile shown on
/// this display can bind to the GPU that drives it by passing `adapter_index` straight
/// through. `luid` is the matching `AdapterLuid` (see [`dxgi_luid_for_gpu_index`]);
/// `left/top/width/height` are the output's desktop virtual coordinates in physical pixels.
#[derive(Clone, Debug)]
pub struct DisplayTopology {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub adapter_index: usize,
    pub luid: (i32, u32),
    pub device_name: String,
    pub attached_to_desktop: bool,
}

/// Enumerate every DXGI output (physical display) together with the adapter that drives it.
///
/// Returns the displays in raw enumeration order (adapter-major); call [`spatial_order`]
/// to turn this into a row-major spatial index. Returns an empty vector off Windows, on
/// non-`no-wgl` builds, or if DXGI enumeration fails — callers should then fall back to
/// their previous (winit monitor index) behaviour.
#[cfg(all(target_os = "windows", feature = "no-wgl"))]
#[expect(unsafe_code)]
pub fn enumerate_display_topology() -> Vec<DisplayTopology> {
    use std::os::raw::c_void;
    use std::ptr;

    use winapi::shared::dxgi::{DXGI_ADAPTER_DESC1, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIOutput};

    fn utf16_z_to_string(buffer: &[u16]) -> String {
        let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..len])
    }

    let mut displays = Vec::new();
    // SAFETY: standard DXGI COM enumeration; every returned pointer is checked non-null and
    // wrapped in a `ComPtr` that releases it, mirroring `create_dxgi_adapter_by_index`.
    unsafe {
        let mut dxgi_factory: *mut IDXGIFactory1 = ptr::null_mut();
        let result = dxgi::CreateDXGIFactory1(
            &IDXGIFactory1::uuidof(),
            &mut dxgi_factory as *mut *mut IDXGIFactory1 as *mut *mut c_void,
        );
        if !winerror::SUCCEEDED(result) || dxgi_factory.is_null() {
            return displays;
        }
        let dxgi_factory = ComPtr::from_raw(dxgi_factory);

        let mut adapter_index: u32 = 0;
        loop {
            let mut adapter1: *mut IDXGIAdapter1 = ptr::null_mut();
            if !winerror::SUCCEEDED((*dxgi_factory).EnumAdapters1(adapter_index, &mut adapter1))
                || adapter1.is_null()
            {
                break;
            }
            let adapter1 = ComPtr::from_raw(adapter1);

            let mut desc: DXGI_ADAPTER_DESC1 = std::mem::zeroed();
            let luid = if winerror::SUCCEEDED((*adapter1).GetDesc1(&mut desc)) {
                (desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart)
            } else {
                (0, 0)
            };

            let mut output_index: u32 = 0;
            loop {
                let mut output: *mut IDXGIOutput = ptr::null_mut();
                if !winerror::SUCCEEDED((*adapter1).EnumOutputs(output_index, &mut output))
                    || output.is_null()
                {
                    break;
                }
                let output = ComPtr::from_raw(output);

                let mut output_desc: DXGI_OUTPUT_DESC = std::mem::zeroed();
                if winerror::SUCCEEDED((*output).GetDesc(&mut output_desc)) {
                    let rect = output_desc.DesktopCoordinates;
                    displays.push(DisplayTopology {
                        left: rect.left,
                        top: rect.top,
                        width: rect.right - rect.left,
                        height: rect.bottom - rect.top,
                        adapter_index: adapter_index as usize,
                        luid,
                        device_name: utf16_z_to_string(&output_desc.DeviceName),
                        attached_to_desktop: output_desc.AttachedToDesktop != 0,
                    });
                }
                output_index += 1;
            }
            adapter_index += 1;
        }
    }
    displays
}

#[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
pub fn enumerate_display_topology() -> Vec<DisplayTopology> {
    Vec::new()
}

/// Order displays spatially: top-left first, left→right within a row, then top→bottom.
///
/// The returned vector's position is the spatial index a wall layout references as
/// `display`. Only desktop-attached displays are included. Displays are grouped into rows
/// by vertical overlap (≥50% of the shorter height, with a median-height tolerance) so
/// mixed-resolution rows still band together.
pub fn spatial_order(topology: &[DisplayTopology]) -> Vec<DisplayTopology> {
    let mut displays: Vec<DisplayTopology> = topology
        .iter()
        .filter(|display| display.attached_to_desktop)
        .cloned()
        .collect();
    if displays.is_empty() {
        return displays;
    }

    let mut heights: Vec<i32> = displays
        .iter()
        .map(|display| display.height.max(1))
        .collect();
    heights.sort_unstable();
    let median_height = heights[heights.len() / 2];
    let tolerance = (median_height / 2).max(1);

    // Pre-sort by (top, left) so each new row's first member is its topmost-leftmost.
    displays.sort_by(|a, b| a.top.cmp(&b.top).then(a.left.cmp(&b.left)));

    let mut rows: Vec<Vec<DisplayTopology>> = Vec::new();
    for display in displays {
        let mut placed = None;
        for (row_index, row) in rows.iter().enumerate() {
            let representative = &row[0];
            let overlap_top = display.top.max(representative.top);
            let overlap_bottom =
                (display.top + display.height).min(representative.top + representative.height);
            let overlap = (overlap_bottom - overlap_top).max(0);
            let shorter = display.height.min(representative.height).max(1);
            if overlap * 2 >= shorter || (display.top - representative.top).abs() <= tolerance {
                placed = Some(row_index);
                break;
            }
        }
        match placed {
            Some(row_index) => rows[row_index].push(display),
            None => rows.push(vec![display]),
        }
    }

    rows.sort_by_key(|row| row.iter().map(|display| display.top).min().unwrap_or(0));
    let mut ordered = Vec::new();
    for mut row in rows {
        row.sort_by_key(|display| display.left);
        ordered.append(&mut row);
    }
    ordered
}

#[cfg(test)]
mod display_topology_tests {
    use super::{DisplayTopology, spatial_order};

    fn display(left: i32, top: i32, width: i32, height: i32, adapter: usize) -> DisplayTopology {
        DisplayTopology {
            left,
            top,
            width,
            height,
            adapter_index: adapter,
            luid: (0, adapter as u32),
            device_name: format!("\\\\.\\DISPLAY{adapter}"),
            attached_to_desktop: true,
        }
    }

    fn order(displays: &[DisplayTopology]) -> Vec<(i32, i32)> {
        spatial_order(displays)
            .iter()
            .map(|display| (display.left, display.top))
            .collect()
    }

    #[test]
    fn row_major_2x1() {
        let displays = vec![
            display(1920, 0, 1920, 1080, 1),
            display(0, 0, 1920, 1080, 0),
        ];
        assert_eq!(order(&displays), vec![(0, 0), (1920, 0)]);
    }

    #[test]
    fn row_major_2x2() {
        let displays = vec![
            display(1920, 1080, 1920, 1080, 3),
            display(0, 1080, 1920, 1080, 2),
            display(1920, 0, 1920, 1080, 1),
            display(0, 0, 1920, 1080, 0),
        ];
        assert_eq!(
            order(&displays),
            vec![(0, 0), (1920, 0), (0, 1080), (1920, 1080)]
        );
    }

    #[test]
    fn three_in_a_row() {
        let displays = vec![
            display(3840, 0, 1920, 1080, 2),
            display(0, 0, 1920, 1080, 0),
            display(1920, 0, 1920, 1080, 1),
        ];
        assert_eq!(order(&displays), vec![(0, 0), (1920, 0), (3840, 0)]);
    }

    #[test]
    fn mixed_resolution_row_bands_together() {
        let displays = vec![
            display(3840, 0, 1920, 1080, 1),
            display(0, 0, 3840, 2160, 0),
        ];
        assert_eq!(order(&displays), vec![(0, 0), (3840, 0)]);
    }

    #[test]
    fn skips_unattached_displays() {
        let mut detached = display(9999, 9999, 1920, 1080, 5);
        detached.attached_to_desktop = false;
        let displays = vec![display(0, 0, 1920, 1080, 0), detached];
        assert_eq!(order(&displays), vec![(0, 0)]);
    }
}

/// A rendering context that uses the Surfman library to create and manage
/// the OpenGL context and surface. This struct provides the default implementation
/// of the `RenderingContext` trait, handling the creation, management, and destruction
/// of the rendering context and its associated resources.
///
/// The `SurfmanRenderingContext` struct encapsulates the necessary data and methods
/// to interact with the Surfman library, including creating surfaces, binding surfaces,
/// resizing surfaces, presenting rendered frames, and managing the OpenGL context state.
struct SurfmanRenderingContext {
    gleam_gl: Rc<dyn Gl>,
    glow_gl: Arc<glow::Context>,
    device: RefCell<Device>,
    context: RefCell<Context>,
    refresh_driver: Option<Rc<dyn RefreshDriver>>,
}

impl Drop for SurfmanRenderingContext {
    fn drop(&mut self) {
        let device = &mut self.device.borrow_mut();
        let context = &mut self.context.borrow_mut();
        let _ = device.destroy_context(context);
    }
}

impl SurfmanRenderingContext {
    fn new(
        connection: &Connection,
        adapter: &Adapter,
        refresh_driver: Option<Rc<dyn RefreshDriver>>,
    ) -> Result<Self, Error> {
        let device = connection.create_device(adapter)?;

        let flags = ContextAttributeFlags::ALPHA |
            ContextAttributeFlags::DEPTH |
            ContextAttributeFlags::STENCIL;
        let gl_api = connection.gl_api();
        let version = match &gl_api {
            GLApi::GLES => surfman::GLVersion { major: 3, minor: 0 },
            GLApi::GL => surfman::GLVersion { major: 3, minor: 2 },
        };
        let context_descriptor =
            device.create_context_descriptor(&ContextAttributes { flags, version })?;

        let context = device
            .create_context(&context_descriptor, None)
            .inspect_err(|_| {
                print_diagnostics_information_on_context_creation_failure(&device, gl_api, version)
            })?;

        #[expect(unsafe_code)]
        let gleam_gl = {
            match gl_api {
                GLApi::GL => unsafe {
                    gl::GlFns::load_with(|func_name| device.get_proc_address(&context, func_name))
                },
                GLApi::GLES => unsafe {
                    gl::GlesFns::load_with(|func_name| device.get_proc_address(&context, func_name))
                },
            }
        };

        #[expect(unsafe_code)]
        let glow_gl = unsafe {
            glow::Context::from_loader_function(|function_name| {
                device.get_proc_address(&context, function_name)
            })
        };

        Ok(SurfmanRenderingContext {
            gleam_gl,
            glow_gl: Arc::new(glow_gl),
            device: RefCell::new(device),
            context: RefCell::new(context),
            refresh_driver,
        })
    }

    fn create_surface(&self, surface_type: SurfaceType<NativeWidget>) -> Result<Surface, Error> {
        let device = &mut self.device.borrow_mut();
        let context = &self.context.borrow();
        device.create_surface(context, SurfaceAccess::GPUOnly, surface_type)
    }

    fn bind_surface(&self, surface: Surface) -> Result<(), Error> {
        let device = &self.device.borrow();
        let context = &mut self.context.borrow_mut();
        device
            .bind_surface_to_context(context, surface)
            .map_err(|(err, mut surface)| {
                let _ = device.destroy_surface(context, &mut surface);
                err
            })?;
        Ok(())
    }

    fn create_attached_swap_chain(&self) -> Result<SwapChain<Device>, Error> {
        let device = &mut self.device.borrow_mut();
        let context = &mut self.context.borrow_mut();
        SwapChain::create_attached(device, context, SurfaceAccess::GPUOnly)
    }

    fn resize_surface(&self, size: PhysicalSize<u32>) -> Result<(), Error> {
        if size.width == 0 || size.height == 0 {
            log::error!("Unable to resize to size under 1x1 ({size:?} provided)");
            return Err(Error::Failed);
        }

        let size = Size2D::new(size.width as i32, size.height as i32);
        let device = &mut self.device.borrow_mut();
        let context = &mut self.context.borrow_mut();

        let mut surface = device.unbind_surface_from_context(context)?.unwrap();
        device.resize_surface(context, &mut surface, size)?;
        device
            .bind_surface_to_context(context, surface)
            .map_err(|(err, mut surface)| {
                let _ = device.destroy_surface(context, &mut surface);
                err
            })
    }

    fn present_bound_surface(&self) -> Result<(), Error> {
        let device = &self.device.borrow();
        let context = &mut self.context.borrow_mut();

        let mut surface = device
            .unbind_surface_from_context(context)?
            // todo: proper error type. This probably should be done in surfman.
            .ok_or(Error::Failed)
            .inspect_err(|_| log::error!("Unable to present bound surface: no surface bound"))?;
        device.present_surface(context, &mut surface)?;
        device
            .bind_surface_to_context(context, surface)
            .map_err(|(err, mut surface)| {
                let _ = device.destroy_surface(context, &mut surface);
                err
            })
    }

    #[expect(dead_code)]
    fn native_context(&self) -> NativeContext {
        let device = &self.device.borrow();
        let context = &self.context.borrow();
        device.native_context(context)
    }

    fn framebuffer(&self) -> Option<NativeFramebuffer> {
        let device = &self.device.borrow();
        let context = &self.context.borrow();
        device
            .context_surface_info(context)
            .unwrap_or(None)
            .and_then(|info| info.framebuffer_object)
    }

    fn prepare_for_rendering(&self) {
        let framebuffer_id = self
            .framebuffer()
            .map_or(0, |framebuffer| framebuffer.0.into());
        self.gleam_gl
            .bind_framebuffer(gleam::gl::FRAMEBUFFER, framebuffer_id);
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        let framebuffer_id = self
            .framebuffer()
            .map_or(0, |framebuffer| framebuffer.0.into());
        Framebuffer::read_framebuffer_to_image(&self.gleam_gl, framebuffer_id, source_rectangle)
    }

    fn make_current(&self) -> Result<(), Error> {
        let device = &self.device.borrow();
        let context = &mut self.context.borrow();
        device.make_context_current(context)
    }

    fn create_texture(
        &self,
        surface: Surface,
    ) -> Result<(SurfaceTexture, u32, UntypedSize2D<i32>), Surface> {
        let device = &self.device.borrow();
        let context = &mut self.context.borrow_mut();
        let SurfaceInfo {
            id: front_buffer_id,
            size,
            ..
        } = device.surface_info(&surface);
        debug!("... getting texture for surface {:?}", front_buffer_id);
        let surface_texture = match device.create_surface_texture(context, surface) {
            Ok(surface_texture) => surface_texture,
            Err((error, surface)) => {
                debug!(
                    "Unable to create texture for surface {:?}: {:?}",
                    front_buffer_id, error
                );
                return Err(surface);
            },
        };
        let gl_texture = device
            .surface_texture_object(&surface_texture)
            .map(|tex| tex.0.get())
            .unwrap_or(0);
        Ok((surface_texture, gl_texture, size))
    }

    fn destroy_texture(&self, surface_texture: SurfaceTexture) -> Option<Surface> {
        let device = &self.device.borrow();
        let context = &mut self.context.borrow_mut();
        device
            .destroy_surface_texture(context, surface_texture)
            .map_err(|(error, _)| error)
            .ok()
    }

    fn create_texture_from_shared_handle(
        &self,
        handle: u64,
        size: UntypedSize2D<i32>,
    ) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)> {
        #[cfg(all(target_os = "windows", feature = "no-wgl"))]
        {
            let device = &self.device.borrow();
            let context = &mut self.context.borrow_mut();
            let handle = handle as winapi::shared::ntdef::HANDLE;
            let surface_texture =
                match device.create_surface_texture_from_shared_handle(context, &size, handle) {
                    Ok(surface_texture) => surface_texture,
                    Err(error) => {
                        warn!("GPU-direct: importing shared handle as texture failed: {error:?}");
                        return None;
                    },
                };
            let gl_texture = device
                .surface_texture_object(&surface_texture)
                .map(|tex| tex.0.get())
                .unwrap_or(0);
            Some((surface_texture, gl_texture, size))
        }
        #[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
        {
            let _ = (handle, size);
            None
        }
    }

    #[cfg_attr(all(target_os = "windows", feature = "no-wgl"), expect(unsafe_code))]
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

    #[cfg_attr(all(target_os = "windows", feature = "no-wgl"), expect(unsafe_code))]
    fn map_d3d11_dynamic_texture(&self, texture: usize) -> Option<(usize, u32)> {
        #[cfg(all(target_os = "windows", feature = "no-wgl"))]
        {
            let device = self.device.borrow();
            match unsafe { device.map_d3d11_dynamic_texture(texture as *mut _) } {
                Ok((ptr, pitch)) => Some((ptr as usize, pitch)),
                Err(error) => {
                    warn!("media D3D11 interop: map_d3d11_dynamic_texture failed: {error:?}");
                    None
                },
            }
        }
        #[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
        {
            let _ = texture;
            None
        }
    }

    #[cfg_attr(all(target_os = "windows", feature = "no-wgl"), expect(unsafe_code))]
    fn unmap_d3d11_texture(&self, texture: usize) {
        #[cfg(all(target_os = "windows", feature = "no-wgl"))]
        {
            let device = self.device.borrow();
            unsafe {
                device.unmap_d3d11_texture(texture as *mut _);
            }
        }
        #[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
        {
            let _ = texture;
        }
    }

    #[cfg_attr(all(target_os = "windows", feature = "no-wgl"), expect(unsafe_code))]
    fn wrap_d3d11_texture_as_gl_texture(&self, texture: usize) -> Option<D3d11GlWrappedTexture> {
        #[cfg(all(target_os = "windows", feature = "no-wgl"))]
        {
            let device = &self.device.borrow();
            let context = &mut self.context.borrow_mut();
            match unsafe { device.create_gl_texture_from_d3d11_texture(context, texture as *mut _) }
            {
                Ok((egl_image, gl_texture)) => Some(D3d11GlWrappedTexture {
                    egl_image,
                    gl_texture,
                }),
                Err(error) => {
                    warn!(
                        "media D3D11 interop: create_gl_texture_from_d3d11_texture failed: {error:?}"
                    );
                    None
                },
            }
        }
        #[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
        {
            let _ = texture;
            None
        }
    }

    #[cfg_attr(all(target_os = "windows", feature = "no-wgl"), expect(unsafe_code))]
    fn destroy_d3d11_gl_wrap(&self, wrap: D3d11GlWrappedTexture) {
        #[cfg(all(target_os = "windows", feature = "no-wgl"))]
        {
            let device = &self.device.borrow();
            let context = &mut self.context.borrow_mut();
            unsafe {
                device.destroy_gl_texture_and_egl_image(context, wrap.egl_image, wrap.gl_texture);
            }
        }
        #[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
        {
            let _ = wrap;
        }
    }

    #[cfg_attr(all(target_os = "windows", feature = "no-wgl"), expect(unsafe_code))]
    fn release_d3d11_texture(&self, texture: usize) {
        #[cfg(all(target_os = "windows", feature = "no-wgl"))]
        {
            if texture == 0 {
                return;
            }
            unsafe {
                (*(texture as *mut winapi::um::unknwnbase::IUnknown)).Release();
            }
        }
        #[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
        {
            let _ = texture;
        }
    }

    /// See [`RenderingContext::angle_d3d11_device_ptr`]. No AddRef — lifetime is owned by
    /// this rendering context's device.
    fn angle_d3d11_device_ptr(&self) -> Option<usize> {
        #[cfg(all(target_os = "windows", feature = "no-wgl"))]
        {
            let device = &self.device.borrow();
            let ptr = device.d3d11_device_ptr();
            if ptr.is_null() { None } else { Some(ptr as usize) }
        }
        #[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
        None
    }

    /// See [`RenderingContext::create_render_pbuffer_from_d3d_texture`].
    #[cfg_attr(all(target_os = "windows", feature = "no-wgl"), expect(unsafe_code))]
    fn create_render_pbuffer_from_d3d_texture(
        &self,
        texture: usize,
        size: UntypedSize2D<i32>,
    ) -> Option<usize> {
        #[cfg(all(target_os = "windows", feature = "no-wgl"))]
        {
            let device = &self.device.borrow();
            let context = &self.context.borrow();
            // Safety: the caller guarantees `texture` is a live RENDER_TARGET
            // ID3D11Texture2D (as returned by DComp BeginDraw). surfman does not retain
            // the pointer.
            unsafe {
                device
                    .create_render_pbuffer_from_d3d_texture(context, texture as *mut _, size)
                    .map(|surface| surface as usize)
            }
        }
        #[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
        {
            let _ = (texture, size);
            None
        }
    }

    /// See [`RenderingContext::make_render_pbuffer_current`].
    #[cfg_attr(all(target_os = "windows", feature = "no-wgl"), expect(unsafe_code))]
    fn make_render_pbuffer_current(&self, egl_surface: usize) -> bool {
        #[cfg(all(target_os = "windows", feature = "no-wgl"))]
        {
            let device = &self.device.borrow();
            let context = &self.context.borrow();
            unsafe { device.make_render_pbuffer_current(context, egl_surface as *const _) }
        }
        #[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
        {
            let _ = egl_surface;
            false
        }
    }

    /// See [`RenderingContext::destroy_render_pbuffer`].
    #[cfg_attr(all(target_os = "windows", feature = "no-wgl"), expect(unsafe_code))]
    fn destroy_render_pbuffer(&self, egl_surface: usize) {
        #[cfg(all(target_os = "windows", feature = "no-wgl"))]
        {
            let device = &self.device.borrow();
            unsafe { device.destroy_render_pbuffer(egl_surface as *const _) }
        }
        #[cfg(not(all(target_os = "windows", feature = "no-wgl")))]
        {
            let _ = egl_surface;
        }
    }

    /// See [`RenderingContext::copy_rows_to_mapped`]. Does not need surfman: this is
    /// plain pointer arithmetic over a caller-owned mapping.
    #[expect(unsafe_code)]
    fn copy_rows_to_mapped(
        &self,
        dst_ptr: usize,
        dst_pitch: u32,
        src: &[u8],
        row_bytes: usize,
        rows: usize,
    ) {
        if dst_ptr == 0 {
            warn!("copy_rows_to_mapped: null dst_ptr");
            return;
        }
        if row_bytes > dst_pitch as usize {
            warn!("copy_rows_to_mapped: row_bytes ({row_bytes}) exceeds dst_pitch ({dst_pitch})");
            return;
        }
        let Some(required) = row_bytes.checked_mul(rows) else {
            warn!("copy_rows_to_mapped: row_bytes * rows overflowed");
            return;
        };
        if src.len() < required {
            warn!(
                "copy_rows_to_mapped: src too small ({} bytes < {required} required)",
                src.len()
            );
            return;
        }
        let dst_pitch = dst_pitch as usize;
        for row in 0..rows {
            let src_start = row * row_bytes;
            let src_row = &src[src_start..src_start + row_bytes];
            // SAFETY: the caller guarantees `dst_ptr` is a live mapping returned by
            // `map_d3d11_dynamic_texture` with pitch `dst_pitch`, valid for at least
            // `rows` rows; `src_row` bounds were validated above.
            unsafe {
                let dst_row = (dst_ptr as *mut u8).add(row * dst_pitch);
                std::ptr::copy_nonoverlapping(src_row.as_ptr(), dst_row, row_bytes);
            }
        }
    }

    fn connection(&self) -> Option<Connection> {
        Some(self.device.borrow().connection())
    }

    fn refresh_driver(&self) -> Option<Rc<dyn RefreshDriver>> {
        self.refresh_driver.clone()
    }
}

/// A software rendering context that uses a software OpenGL implementation to render
/// Servo. This will generally have bad performance, but can be used in situations where
/// it is more convenient to have consistent, but slower display output.
///
/// The results of the render can be accessed via [`RenderingContext::read_to_image`].
pub struct SoftwareRenderingContext {
    size: Cell<PhysicalSize<u32>>,
    surfman_rendering_info: SurfmanRenderingContext,
    swap_chain: SwapChain<Device>,
}

impl SoftwareRenderingContext {
    pub fn new(size: PhysicalSize<u32>) -> Result<Self, Error> {
        if size.width == 0 || size.height == 0 {
            log::error!(
                "Unable to create SoftwareRenderingContext with size under 1x1 ({size:?} provided)"
            );
            return Err(Error::Failed);
        }

        let connection = Connection::new()?;
        let adapter = connection.create_software_adapter()?;
        let surfman_rendering_info = SurfmanRenderingContext::new(&connection, &adapter, None)?;

        let surfman_size = Size2D::new(size.width as i32, size.height as i32);
        let surface =
            surfman_rendering_info.create_surface(SurfaceType::Generic { size: surfman_size })?;
        surfman_rendering_info.bind_surface(surface)?;
        surfman_rendering_info.make_current()?;

        let swap_chain = surfman_rendering_info.create_attached_swap_chain()?;
        Ok(SoftwareRenderingContext {
            size: Cell::new(size),
            surfman_rendering_info,
            swap_chain,
        })
    }
}

impl Drop for SoftwareRenderingContext {
    fn drop(&mut self) {
        let device = &mut self.surfman_rendering_info.device.borrow_mut();
        let context = &mut self.surfman_rendering_info.context.borrow_mut();
        let _ = self.swap_chain.destroy(device, context);
    }
}

impl RenderingContext for SoftwareRenderingContext {
    fn prepare_for_rendering(&self) {
        self.surfman_rendering_info.prepare_for_rendering();
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        self.surfman_rendering_info.read_to_image(source_rectangle)
    }

    fn size(&self) -> PhysicalSize<u32> {
        self.size.get()
    }

    fn resize(&self, size: PhysicalSize<u32>) {
        assert!(
            size.width > 0 && size.height > 0,
            "Dimensions must be at least 1x1, got {size:?}",
        );

        if self.size.get() == size {
            return;
        }

        self.size.set(size);

        let device = &mut self.surfman_rendering_info.device.borrow_mut();
        let context = &mut self.surfman_rendering_info.context.borrow_mut();
        let size = Size2D::new(size.width as i32, size.height as i32);
        let _ = self.swap_chain.resize(device, context, size);
    }

    #[servo_tracing::instrument(skip_all, name = "SoftwareRenderingContext::present")]
    fn present(&self) {
        let device = &mut self.surfman_rendering_info.device.borrow_mut();
        let context = &mut self.surfman_rendering_info.context.borrow_mut();
        let _ = self
            .swap_chain
            .swap_buffers(device, context, PreserveBuffer::No);
    }

    fn make_current(&self) -> Result<(), Error> {
        self.surfman_rendering_info.make_current()
    }

    fn gleam_gl_api(&self) -> Rc<dyn gleam::gl::Gl> {
        self.surfman_rendering_info.gleam_gl.clone()
    }

    fn glow_gl_api(&self) -> Arc<glow::Context> {
        self.surfman_rendering_info.glow_gl.clone()
    }

    fn create_texture(
        &self,
        surface: Surface,
    ) -> Result<(SurfaceTexture, u32, UntypedSize2D<i32>), Surface> {
        self.surfman_rendering_info.create_texture(surface)
    }

    fn destroy_texture(&self, surface_texture: SurfaceTexture) -> Option<Surface> {
        self.surfman_rendering_info.destroy_texture(surface_texture)
    }

    fn create_texture_from_shared_handle(
        &self,
        handle: u64,
        size: UntypedSize2D<i32>,
    ) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)> {
        self.surfman_rendering_info
            .create_texture_from_shared_handle(handle, size)
    }

    fn media_d3d11_device_handle(&self) -> Option<usize> {
        self.surfman_rendering_info.media_d3d11_device_handle()
    }

    fn map_d3d11_dynamic_texture(&self, texture: usize) -> Option<(usize, u32)> {
        self.surfman_rendering_info
            .map_d3d11_dynamic_texture(texture)
    }

    fn unmap_d3d11_texture(&self, texture: usize) {
        self.surfman_rendering_info.unmap_d3d11_texture(texture)
    }

    fn wrap_d3d11_texture_as_gl_texture(&self, texture: usize) -> Option<D3d11GlWrappedTexture> {
        self.surfman_rendering_info
            .wrap_d3d11_texture_as_gl_texture(texture)
    }

    fn destroy_d3d11_gl_wrap(&self, wrap: D3d11GlWrappedTexture) {
        self.surfman_rendering_info.destroy_d3d11_gl_wrap(wrap)
    }

    fn release_d3d11_texture(&self, texture: usize) {
        self.surfman_rendering_info.release_d3d11_texture(texture)
    }

    fn angle_d3d11_device_ptr(&self) -> Option<usize> {
        self.surfman_rendering_info.angle_d3d11_device_ptr()
    }

    fn create_render_pbuffer_from_d3d_texture(
        &self,
        texture: usize,
        size: UntypedSize2D<i32>,
    ) -> Option<usize> {
        self.surfman_rendering_info
            .create_render_pbuffer_from_d3d_texture(texture, size)
    }

    fn make_render_pbuffer_current(&self, egl_surface: usize) -> bool {
        self.surfman_rendering_info
            .make_render_pbuffer_current(egl_surface)
    }

    fn destroy_render_pbuffer(&self, egl_surface: usize) {
        self.surfman_rendering_info.destroy_render_pbuffer(egl_surface)
    }

    fn copy_rows_to_mapped(
        &self,
        dst_ptr: usize,
        dst_pitch: u32,
        src: &[u8],
        row_bytes: usize,
        rows: usize,
    ) {
        self.surfman_rendering_info
            .copy_rows_to_mapped(dst_ptr, dst_pitch, src, row_bytes, rows)
    }

    fn connection(&self) -> Option<Connection> {
        self.surfman_rendering_info.connection()
    }
}

/// A [`RenderingContext`] that uses the `surfman` library to render to a
/// `raw-window-handle` identified window. `surfman` will attempt to create an
/// OpenGL context and surface for this window. This is a simple implementation
/// of the [`RenderingContext`] trait, but by default it paints to the entire
/// window surface.
///
/// If you would like to paint to only a portion of the window, consider using
/// [`OffscreenRenderingContext`] by calling [`WindowRenderingContext::offscreen_context`].
pub struct WindowRenderingContext {
    /// The inner size of the window in physical pixels which excludes OS decorations.
    size: Cell<PhysicalSize<u32>>,
    surfman_context: SurfmanRenderingContext,
    requested_gpu_index: Option<usize>,
    /// Native compositor(DirectComposition) 인터롭용 Win32 HWND. Windows에서만 보관.
    #[cfg(windows)]
    win32_hwnd: Option<usize>,
    /// Native compositor(DirectComposition)가 이 창에서 실제로 발동했는지. painter가
    /// `dcomp_compositor::maybe_create` 성공 시 `set_dcomp_native_active(true)`로 갱신한다.
    /// `present()`의 스킵 판정은 env가 아닌 이 값을 본다(§`RenderingContext::set_dcomp_native_active`).
    #[cfg(windows)]
    dcomp_native_active: Cell<bool>,
    /// task-12b: 사용자가 창을 드래그-리사이즈 중인지(painter가 설정). 컴포지터가 매
    /// end_frame에서 읽어 리사이즈 중 스왑체인 강등·승격/regen 억제를 판단한다.
    #[cfg(windows)]
    dcomp_resize_active: Cell<bool>,
    /// 월 타일 가드밴드 여백 — §`RenderingContext::present_inset`. servoshell이 창 생성 시
    /// 1회 설정하고, DComp 경로(root visual 오프셋)와 blit 경로(source rect)가 함께 읽는다.
    present_inset: Cell<DeviceIntSideOffsets>,
}

impl WindowRenderingContext {
    pub fn new(
        display_handle: DisplayHandle,
        window_handle: WindowHandle,
        size: PhysicalSize<u32>,
    ) -> Result<Self, Error> {
        Self::new_with_optional_refresh_driver(display_handle, window_handle, size, None)
    }

    pub fn new_with_target_gpu(
        display_handle: DisplayHandle,
        window_handle: WindowHandle,
        size: PhysicalSize<u32>,
        requested_gpu_index: Option<usize>,
    ) -> Result<Self, Error> {
        Self::new_with_optional_refresh_driver_and_target_gpu(
            display_handle,
            window_handle,
            size,
            None,
            requested_gpu_index,
        )
    }

    pub fn new_with_refresh_driver(
        display_handle: DisplayHandle,
        window_handle: WindowHandle,
        size: PhysicalSize<u32>,
        refresh_driver: Rc<dyn RefreshDriver>,
    ) -> Result<Self, Error> {
        Self::new_with_optional_refresh_driver(
            display_handle,
            window_handle,
            size,
            Some(refresh_driver),
        )
    }

    fn new_with_optional_refresh_driver(
        display_handle: DisplayHandle,
        window_handle: WindowHandle,
        size: PhysicalSize<u32>,
        refresh_driver: Option<Rc<dyn RefreshDriver>>,
    ) -> Result<Self, Error> {
        Self::new_with_optional_refresh_driver_and_target_gpu(
            display_handle,
            window_handle,
            size,
            refresh_driver,
            None,
        )
    }

    pub fn new_with_optional_refresh_driver_and_target_gpu(
        display_handle: DisplayHandle,
        window_handle: WindowHandle,
        size: PhysicalSize<u32>,
        refresh_driver: Option<Rc<dyn RefreshDriver>>,
        requested_gpu_index: Option<usize>,
    ) -> Result<Self, Error> {
        if size.width == 0 || size.height == 0 {
            log::error!(
                "Unable to create WindowRenderingContext with size under 1x1 ({size:?} provided)"
            );
            return Err(Error::Failed);
        }

        let connection = Connection::from_display_handle(display_handle)?;
        let adapter = create_adapter_for_requested_gpu(&connection, requested_gpu_index)?;
        let surfman_context = SurfmanRenderingContext::new(&connection, &adapter, refresh_driver)?;

        // connection.rs:193과 동일 패턴으로 Win32 HWND를 추출해 보관한다. `window_handle`은
        // 아래 `create_native_widget_from_window_handle`로 이동되므로 그 전에 추출한다.
        #[cfg(windows)]
        let win32_hwnd: Option<usize> = match window_handle.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as usize),
            _ => None,
        };

        let native_widget = connection
            .create_native_widget_from_window_handle(
                window_handle,
                Size2D::new(size.width as i32, size.height as i32),
            )
            .expect("Failed to create native widget");

        let surface = surfman_context.create_surface(SurfaceType::Widget { native_widget })?;
        surfman_context.bind_surface(surface)?;
        surfman_context.make_current()?;

        Ok(Self {
            size: Cell::new(size),
            surfman_context,
            requested_gpu_index,
            #[cfg(windows)]
            win32_hwnd,
            #[cfg(windows)]
            dcomp_native_active: Cell::new(false),
            #[cfg(windows)]
            dcomp_resize_active: Cell::new(false),
            present_inset: Cell::new(DeviceIntSideOffsets::zero()),
        })
    }

    pub fn offscreen_context(
        self: &Rc<Self>,
        size: PhysicalSize<u32>,
    ) -> OffscreenRenderingContext {
        OffscreenRenderingContext::new(self.clone(), size)
    }

    /// Stop rendering to the window that was used to create this `WindowRenderingContext`
    /// or last set with [`Self::set_window`].
    ///
    /// TODO: This should be removed once `WebView`s can replace their `RenderingContext`s.
    pub fn take_window(&self) -> Result<(), Error> {
        let device = self.surfman_context.device.borrow_mut();
        let mut context = self.surfman_context.context.borrow_mut();
        let mut surface = device.unbind_surface_from_context(&mut context)?.unwrap();
        device.destroy_surface(&mut context, &mut surface)?;
        Ok(())
    }

    /// Replace the window that this [`WindowRenderingContext`] renders to and give it a new
    /// size.
    ///
    /// TODO: This should be removed once `WebView`s can replace their `RenderingContext`s.
    pub fn set_window(
        &self,
        window_handle: WindowHandle,
        size: PhysicalSize<u32>,
    ) -> Result<(), Error> {
        let device = self.surfman_context.device.borrow_mut();
        let mut context = self.surfman_context.context.borrow_mut();

        let native_widget = device
            .connection()
            .create_native_widget_from_window_handle(
                window_handle,
                Size2D::new(size.width as i32, size.height as i32),
            )
            .expect("Failed to create native widget");

        let surface_access = SurfaceAccess::GPUOnly;
        let surface_type = SurfaceType::Widget { native_widget };
        let surface = device.create_surface(&context, surface_access, surface_type)?;

        device
            .bind_surface_to_context(&mut context, surface)
            .map_err(|(err, mut surface)| {
                let _ = device.destroy_surface(&mut context, &mut surface);
                err
            })?;
        device.make_context_current(&context)?;
        Ok(())
    }

    pub fn surfman_details(&self) -> (RefMut<'_, Device>, RefMut<'_, Context>) {
        (
            self.surfman_context.device.borrow_mut(),
            self.surfman_context.context.borrow_mut(),
        )
    }

    pub fn requested_gpu_index(&self) -> Option<usize> {
        self.requested_gpu_index
    }
}

impl RenderingContext for WindowRenderingContext {
    fn prepare_for_rendering(&self) {
        self.surfman_context.prepare_for_rendering();
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        self.surfman_context.read_to_image(source_rectangle)
    }

    fn present_inset(&self) -> DeviceIntSideOffsets {
        self.present_inset.get()
    }

    fn set_present_inset(&self, inset: DeviceIntSideOffsets) {
        self.present_inset.set(inset);
    }

    fn size(&self) -> PhysicalSize<u32> {
        self.size.get()
    }

    fn resize(&self, size: PhysicalSize<u32>) {
        match self.surfman_context.resize_surface(size) {
            Ok(..) => self.size.set(size),
            Err(error) => warn!("Error resizing surface: {error:?}"),
        }
    }

    #[servo_tracing::instrument(skip_all, name = "WindowRenderingContext::present")]
    fn present(&self) {
        // Native Compositor(DComp)가 실제로 발동 중이면(`set_dcomp_native_active(true)` —
        // painter의 `maybe_create` 성공 분기에서만 호출됨) 웹콘텐츠는 WR이 DComp 비주얼
        // 트리에 직접 그려 DWM이 합성하고, 창 백버퍼에는 그 트리에 가려지는 egui 크롬뿐이다.
        // 이 경우 ppf도 꺼져 있어(§surfman luid_display_attribs) 이 present는 표시에
        // 기여하지 않는 offscreen→backbuffer 복사+스왑 비용만 남는다 → 스킵.
        //
        // env 게이트(`SERVO_COMPOSITOR_DCOMP`)만으로 판단하면 안 된다: env는 켜졌지만
        // `maybe_create`가 실패해 Draw 컴포지터로 폴백한 경우 present가 실제 표시 경로인데
        // 잘못 스킵되어 블랭크 윈도우가 된다.
        #[cfg(windows)]
        if self.dcomp_native_active.get() {
            static PRESENT_SKIP_LOGGED: std::sync::Once = std::sync::Once::new();
            PRESENT_SKIP_LOGGED.call_once(|| {
                info!("[dcomp-native] window present skipped (content composited via DComp)");
            });
            return;
        }
        if let Err(error) = self.surfman_context.present_bound_surface() {
            warn!("Error presenting surface: {error:?}");
        }
    }

    #[cfg(windows)]
    fn set_dcomp_native_active(&self, active: bool) {
        self.dcomp_native_active.set(active);
    }

    // On non-Windows the `dcomp_native_active` field does not exist (cfg(windows)); the trait
    // default `false` covers those targets safely.
    #[cfg(windows)]
    fn dcomp_native_active(&self) -> bool {
        self.dcomp_native_active.get()
    }

    // task-12b: 드래그-리사이즈 활성 신호(painter → 컴포지터 공유 채널).
    #[cfg(windows)]
    fn set_dcomp_resize_active(&self, active: bool) {
        self.dcomp_resize_active.set(active);
    }

    #[cfg(windows)]
    fn dcomp_resize_active(&self) -> bool {
        self.dcomp_resize_active.get()
    }

    fn make_current(&self) -> Result<(), Error> {
        self.surfman_context.make_current()
    }

    fn gleam_gl_api(&self) -> Rc<dyn gleam::gl::Gl> {
        self.surfman_context.gleam_gl.clone()
    }

    fn glow_gl_api(&self) -> Arc<glow::Context> {
        self.surfman_context.glow_gl.clone()
    }

    fn create_texture(
        &self,
        surface: Surface,
    ) -> Result<(SurfaceTexture, u32, UntypedSize2D<i32>), Surface> {
        self.surfman_context.create_texture(surface)
    }

    fn destroy_texture(&self, surface_texture: SurfaceTexture) -> Option<Surface> {
        self.surfman_context.destroy_texture(surface_texture)
    }

    fn create_texture_from_shared_handle(
        &self,
        handle: u64,
        size: UntypedSize2D<i32>,
    ) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)> {
        self.surfman_context
            .create_texture_from_shared_handle(handle, size)
    }

    fn media_d3d11_device_handle(&self) -> Option<usize> {
        self.surfman_context.media_d3d11_device_handle()
    }

    fn map_d3d11_dynamic_texture(&self, texture: usize) -> Option<(usize, u32)> {
        self.surfman_context.map_d3d11_dynamic_texture(texture)
    }

    fn unmap_d3d11_texture(&self, texture: usize) {
        self.surfman_context.unmap_d3d11_texture(texture)
    }

    fn wrap_d3d11_texture_as_gl_texture(&self, texture: usize) -> Option<D3d11GlWrappedTexture> {
        self.surfman_context
            .wrap_d3d11_texture_as_gl_texture(texture)
    }

    fn destroy_d3d11_gl_wrap(&self, wrap: D3d11GlWrappedTexture) {
        self.surfman_context.destroy_d3d11_gl_wrap(wrap)
    }

    fn release_d3d11_texture(&self, texture: usize) {
        self.surfman_context.release_d3d11_texture(texture)
    }

    #[cfg(windows)]
    fn window_hwnd(&self) -> Option<usize> {
        self.win32_hwnd
    }

    fn angle_d3d11_device_ptr(&self) -> Option<usize> {
        self.surfman_context.angle_d3d11_device_ptr()
    }

    fn create_render_pbuffer_from_d3d_texture(
        &self,
        texture: usize,
        size: UntypedSize2D<i32>,
    ) -> Option<usize> {
        self.surfman_context
            .create_render_pbuffer_from_d3d_texture(texture, size)
    }

    fn make_render_pbuffer_current(&self, egl_surface: usize) -> bool {
        self.surfman_context.make_render_pbuffer_current(egl_surface)
    }

    fn destroy_render_pbuffer(&self, egl_surface: usize) {
        self.surfman_context.destroy_render_pbuffer(egl_surface)
    }

    fn copy_rows_to_mapped(
        &self,
        dst_ptr: usize,
        dst_pitch: u32,
        src: &[u8],
        row_bytes: usize,
        rows: usize,
    ) {
        self.surfman_context
            .copy_rows_to_mapped(dst_ptr, dst_pitch, src, row_bytes, rows)
    }

    fn connection(&self) -> Option<Connection> {
        self.surfman_context.connection()
    }

    fn refresh_driver(&self) -> Option<Rc<dyn RefreshDriver>> {
        self.surfman_context.refresh_driver()
    }

    fn requested_gpu_index(&self) -> Option<usize> {
        self.requested_gpu_index
    }
}

struct Framebuffer {
    gl: Rc<dyn Gl>,
    framebuffer_id: gl::GLuint,
    renderbuffer_id: gl::GLuint,
    texture_id: gl::GLuint,
}

impl Framebuffer {
    fn bind(&self) {
        trace!("Binding FBO {}", self.framebuffer_id);
        self.gl
            .bind_framebuffer(gl::FRAMEBUFFER, self.framebuffer_id)
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        self.gl.bind_framebuffer(gl::FRAMEBUFFER, 0);
        self.gl.delete_textures(&[self.texture_id]);
        self.gl.delete_renderbuffers(&[self.renderbuffer_id]);
        self.gl.delete_framebuffers(&[self.framebuffer_id]);
    }
}

impl Framebuffer {
    fn new(gl: Rc<dyn Gl>, size: PhysicalSize<u32>) -> Self {
        let framebuffer_ids = gl.gen_framebuffers(1);
        gl.bind_framebuffer(gl::FRAMEBUFFER, framebuffer_ids[0]);

        let texture_ids = gl.gen_textures(1);
        gl.bind_texture(gl::TEXTURE_2D, texture_ids[0]);
        gl.tex_image_2d(
            gl::TEXTURE_2D,
            0,
            gl::RGBA as gl::GLint,
            size.width as gl::GLsizei,
            size.height as gl::GLsizei,
            0,
            gl::RGBA,
            gl::UNSIGNED_BYTE,
            None,
        );
        gl.tex_parameter_i(
            gl::TEXTURE_2D,
            gl::TEXTURE_MAG_FILTER,
            gl::NEAREST as gl::GLint,
        );
        gl.tex_parameter_i(
            gl::TEXTURE_2D,
            gl::TEXTURE_MIN_FILTER,
            gl::NEAREST as gl::GLint,
        );

        gl.framebuffer_texture_2d(
            gl::FRAMEBUFFER,
            gl::COLOR_ATTACHMENT0,
            gl::TEXTURE_2D,
            texture_ids[0],
            0,
        );

        gl.bind_texture(gl::TEXTURE_2D, 0);

        let renderbuffer_ids = gl.gen_renderbuffers(1);
        let depth_rb = renderbuffer_ids[0];
        gl.bind_renderbuffer(gl::RENDERBUFFER, depth_rb);
        gl.renderbuffer_storage(
            gl::RENDERBUFFER,
            gl::DEPTH_COMPONENT24,
            size.width as gl::GLsizei,
            size.height as gl::GLsizei,
        );
        gl.framebuffer_renderbuffer(
            gl::FRAMEBUFFER,
            gl::DEPTH_ATTACHMENT,
            gl::RENDERBUFFER,
            depth_rb,
        );

        Self {
            gl,
            framebuffer_id: *framebuffer_ids
                .first()
                .expect("Guaranteed by GL operations"),
            renderbuffer_id: *renderbuffer_ids
                .first()
                .expect("Guaranteed by GL operations"),
            texture_id: *texture_ids.first().expect("Guaranteed by GL operations"),
        }
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        Self::read_framebuffer_to_image(&self.gl, self.framebuffer_id, source_rectangle)
    }

    fn read_framebuffer_to_image(
        gl: &Rc<dyn Gl>,
        framebuffer_id: u32,
        source_rectangle: DeviceIntRect,
    ) -> Option<RgbaImage> {
        gl.bind_framebuffer(gl::FRAMEBUFFER, framebuffer_id);

        // For some reason, OSMesa fails to render on the 3rd
        // attempt in headless mode, under some conditions.
        // I think this can only be some kind of synchronization
        // bug in OSMesa, but explicitly un-binding any vertex
        // array here seems to work around that bug.
        // See https://github.com/servo/servo/issues/18606.
        gl.bind_vertex_array(0);

        let mut pixels = gl.read_pixels(
            source_rectangle.min.x,
            source_rectangle.min.y,
            source_rectangle.width(),
            source_rectangle.height(),
            gl::RGBA,
            gl::UNSIGNED_BYTE,
        );
        let gl_error = gl.get_error();
        if gl_error != gl::NO_ERROR {
            warn!("GL error code 0x{gl_error:x} set after read_pixels");
        }

        // flip image vertically (texture is upside down)
        let source_rectangle = source_rectangle.to_usize();
        let orig_pixels = pixels.clone();
        let stride = source_rectangle.width() * 4;
        for y in 0..source_rectangle.height() {
            let dst_start = y * stride;
            let src_start = (source_rectangle.height() - y - 1) * stride;
            let src_slice = &orig_pixels[src_start..src_start + stride];
            pixels[dst_start..dst_start + stride].clone_from_slice(&src_slice[..stride]);
        }

        RgbaImage::from_raw(
            source_rectangle.width() as u32,
            source_rectangle.height() as u32,
            pixels,
        )
    }
}

pub struct OffscreenRenderingContext {
    parent_context: Rc<WindowRenderingContext>,
    size: Cell<PhysicalSize<u32>>,
    framebuffer: RefCell<Framebuffer>,
}

type RenderToParentCallback = Box<dyn Fn(&glow::Context, Rect<i32>) + Send + Sync>;

impl OffscreenRenderingContext {
    fn new(parent_context: Rc<WindowRenderingContext>, size: PhysicalSize<u32>) -> Self {
        assert!(
            size.width != 0 && size.height != 0,
            "Dimensions must be at least 1x1, got {size:?}",
        );

        let framebuffer = RefCell::new(Framebuffer::new(parent_context.gleam_gl_api(), size));
        Self {
            parent_context,
            size: Cell::new(size),
            framebuffer,
        }
    }

    pub fn parent_context(&self) -> &WindowRenderingContext {
        &self.parent_context
    }

    pub fn render_to_parent_callback(&self) -> Option<RenderToParentCallback> {
        let size = self.size.get();
        let size = Size2D::new(size.width as i32, size.height as i32);
        self.render_to_parent_callback_for_source_rect(Rect::new(
            Point2D::origin(),
            size.to_i32(),
        ))
    }

    pub fn render_to_parent_callback_for_source_rect(
        &self,
        source_rect: Rect<i32>,
    ) -> Option<RenderToParentCallback> {
        // Don't accept a `None` context for the source framebuffer.
        let front_framebuffer_id =
            NonZeroU32::new(self.framebuffer.borrow().framebuffer_id).map(NativeFramebuffer)?;
        let parent_context_framebuffer_id = self.parent_context.surfman_context.framebuffer();
        Some(Box::new(move |gl, target_rect| {
            Self::blit_framebuffer(
                gl,
                source_rect,
                front_framebuffer_id,
                target_rect,
                parent_context_framebuffer_id,
            );
        }))
    }

    #[expect(unsafe_code)]
    fn blit_framebuffer(
        gl: &glow::Context,
        source_rect: Rect<i32>,
        source_framebuffer_id: NativeFramebuffer,
        target_rect: Rect<i32>,
        target_framebuffer_id: Option<NativeFramebuffer>,
    ) {
        use glow::HasContext as _;
        unsafe {
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.scissor(
                target_rect.origin.x,
                target_rect.origin.y,
                target_rect.width(),
                target_rect.height(),
            );
            gl.enable(gl::SCISSOR_TEST);
            gl.clear(gl::COLOR_BUFFER_BIT);
            gl.disable(gl::SCISSOR_TEST);

            gl.bind_framebuffer(gl::READ_FRAMEBUFFER, Some(source_framebuffer_id));
            gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, target_framebuffer_id);

            gl.blit_framebuffer(
                source_rect.origin.x,
                source_rect.origin.y,
                source_rect.origin.x + source_rect.width(),
                source_rect.origin.y + source_rect.height(),
                target_rect.origin.x,
                target_rect.origin.y,
                target_rect.origin.x + target_rect.width(),
                target_rect.origin.y + target_rect.height(),
                gl::COLOR_BUFFER_BIT,
                gl::NEAREST,
            );
            gl.bind_framebuffer(gl::FRAMEBUFFER, target_framebuffer_id);
        }
    }
}

impl RenderingContext for OffscreenRenderingContext {
    fn size(&self) -> PhysicalSize<u32> {
        self.size.get()
    }

    fn resize(&self, new_size: PhysicalSize<u32>) {
        assert!(
            new_size.width != 0 && new_size.height != 0,
            "Dimensions must be at least 1x1, got {new_size:?}",
        );

        let old_size = self.size.get();
        if old_size == new_size {
            return;
        }

        let gl = self.parent_context.gleam_gl_api();
        let new_framebuffer = Framebuffer::new(gl.clone(), new_size);

        let old_framebuffer =
            std::mem::replace(&mut *self.framebuffer.borrow_mut(), new_framebuffer);
        self.size.set(new_size);

        let blit_size = new_size.min(old_size);
        let rect = Rect::new(
            Point2D::origin(),
            Size2D::new(blit_size.width, blit_size.height),
        )
        .to_i32();

        let Some(old_framebuffer_id) =
            NonZeroU32::new(old_framebuffer.framebuffer_id).map(NativeFramebuffer)
        else {
            return;
        };
        let new_framebuffer_id =
            NonZeroU32::new(self.framebuffer.borrow().framebuffer_id).map(NativeFramebuffer);
        Self::blit_framebuffer(
            &self.glow_gl_api(),
            rect,
            old_framebuffer_id,
            rect,
            new_framebuffer_id,
        );
    }

    fn prepare_for_rendering(&self) {
        self.framebuffer.borrow().bind();
    }

    fn present(&self) {}

    fn make_current(&self) -> Result<(), surfman::Error> {
        self.parent_context.make_current()
    }

    fn gleam_gl_api(&self) -> Rc<dyn gleam::gl::Gl> {
        self.parent_context.gleam_gl_api()
    }

    fn glow_gl_api(&self) -> Arc<glow::Context> {
        self.parent_context.glow_gl_api()
    }

    fn create_texture(
        &self,
        surface: Surface,
    ) -> Result<(SurfaceTexture, u32, UntypedSize2D<i32>), Surface> {
        self.parent_context.create_texture(surface)
    }

    fn destroy_texture(&self, surface_texture: SurfaceTexture) -> Option<Surface> {
        self.parent_context.destroy_texture(surface_texture)
    }

    fn create_texture_from_shared_handle(
        &self,
        handle: u64,
        size: UntypedSize2D<i32>,
    ) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)> {
        self.parent_context
            .create_texture_from_shared_handle(handle, size)
    }

    fn media_d3d11_device_handle(&self) -> Option<usize> {
        self.parent_context.media_d3d11_device_handle()
    }

    fn map_d3d11_dynamic_texture(&self, texture: usize) -> Option<(usize, u32)> {
        self.parent_context.map_d3d11_dynamic_texture(texture)
    }

    fn unmap_d3d11_texture(&self, texture: usize) {
        self.parent_context.unmap_d3d11_texture(texture)
    }

    fn wrap_d3d11_texture_as_gl_texture(&self, texture: usize) -> Option<D3d11GlWrappedTexture> {
        self.parent_context
            .wrap_d3d11_texture_as_gl_texture(texture)
    }

    fn destroy_d3d11_gl_wrap(&self, wrap: D3d11GlWrappedTexture) {
        self.parent_context.destroy_d3d11_gl_wrap(wrap)
    }

    fn release_d3d11_texture(&self, texture: usize) {
        self.parent_context.release_d3d11_texture(texture)
    }

    // Native compositor(DirectComposition) interop is delegated to the parent window
    // context. The painter renders into this offscreen context, but the DComp target must
    // be created on the parent window's HWND. Without this forward `window_hwnd()` returns
    // the trait default `None`, so `dcomp_compositor::maybe_create` fails and the native
    // compositor never engages (the pbuffer/device forwards below already delegate; this
    // completes the 5-point Window/Offscreen delegation from the design).
    #[cfg(windows)]
    fn window_hwnd(&self) -> Option<usize> {
        self.parent_context.window_hwnd()
    }

    // servoshell이 Window를 Offscreen으로 감싸므로, 이 위임이 없으면 painter가 이
    // OffscreenRenderingContext 위에서 `set_dcomp_native_active`를 호출해도 트레잇
    // 기본(no-op)에 흡수되어 부모 WindowRenderingContext의 `present()` 스킵 판정에
    // 절대 반영되지 않는다(다른 7종 인터롭 위임과 동일 패턴).
    #[cfg(windows)]
    fn set_dcomp_native_active(&self, active: bool) {
        self.parent_context.set_dcomp_native_active(active);
    }

    // servoshell이 Window를 Offscreen으로 감싸므로, blit 스킵 판정(gui.rs)이 이
    // OffscreenRenderingContext 위에서 `dcomp_native_active()`를 호출한다. 위임이 없으면
    // 트레잇 기본(false)에 흡수돼 스킵이 절대 발동하지 않는다(setter와 동일 위임 패턴).
    #[cfg(windows)]
    fn dcomp_native_active(&self) -> bool {
        self.parent_context.dcomp_native_active()
    }

    // servoshell이 Window를 Offscreen으로 감싸므로 painter/DComp 컴포지터/gui가 모두 이
    // Offscreen 래퍼 위에서 present_inset을 읽고 쓴다. 위임이 없으면 트레잇 기본값(zero)에
    // 흡수돼 가드밴드 크롭이 통째로 사라진다(dcomp_native_active와 동일 위임 패턴).
    fn present_inset(&self) -> DeviceIntSideOffsets {
        self.parent_context.present_inset()
    }

    fn set_present_inset(&self, inset: DeviceIntSideOffsets) {
        self.parent_context.set_present_inset(inset)
    }

    // task-12b: painter가 이 Offscreen 래퍼 위에서 리사이즈 활성 신호를 설정/조회하므로,
    // 부모 WindowRenderingContext의 `Cell`로 위임한다(dcomp_native_active와 동일 패턴).
    #[cfg(windows)]
    fn set_dcomp_resize_active(&self, active: bool) {
        self.parent_context.set_dcomp_resize_active(active);
    }

    #[cfg(windows)]
    fn dcomp_resize_active(&self) -> bool {
        self.parent_context.dcomp_resize_active()
    }

    fn angle_d3d11_device_ptr(&self) -> Option<usize> {
        self.parent_context.angle_d3d11_device_ptr()
    }

    fn create_render_pbuffer_from_d3d_texture(
        &self,
        texture: usize,
        size: UntypedSize2D<i32>,
    ) -> Option<usize> {
        self.parent_context
            .create_render_pbuffer_from_d3d_texture(texture, size)
    }

    fn make_render_pbuffer_current(&self, egl_surface: usize) -> bool {
        self.parent_context.make_render_pbuffer_current(egl_surface)
    }

    fn destroy_render_pbuffer(&self, egl_surface: usize) {
        self.parent_context.destroy_render_pbuffer(egl_surface)
    }

    fn copy_rows_to_mapped(
        &self,
        dst_ptr: usize,
        dst_pitch: u32,
        src: &[u8],
        row_bytes: usize,
        rows: usize,
    ) {
        self.parent_context
            .copy_rows_to_mapped(dst_ptr, dst_pitch, src, row_bytes, rows)
    }

    fn connection(&self) -> Option<Connection> {
        self.parent_context.connection()
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        self.framebuffer.borrow().read_to_image(source_rectangle)
    }

    fn refresh_driver(&self) -> Option<Rc<dyn RefreshDriver>> {
        self.parent_context().refresh_driver()
    }

    fn requested_gpu_index(&self) -> Option<usize> {
        self.parent_context().requested_gpu_index()
    }
}

fn print_diagnostics_information_on_context_creation_failure(
    device: &Device,
    desired_api: GLApi,
    desired_version: GLVersion,
) {
    println!("===============================================================");
    println!(
        "Could not create a {desired_api:?} {:?}.{:?} context when starting Servo.",
        desired_version.major, desired_version.minor
    );

    let version = surfman::GLVersion { major: 1, minor: 0 };
    match device
        .create_context_descriptor(&ContextAttributes {
            flags: ContextAttributeFlags::empty(),
            version,
        })
        .and_then(|context_descriptor| device.create_context(&context_descriptor, None))
    {
        Ok(mut context) => {
            #[expect(unsafe_code)]
            let glow_gl = unsafe {
                glow::Context::from_loader_function(|function_name| {
                    device.get_proc_address(&context, function_name)
                })
            };

            println!(
                "It's likely that your version of OpenGL ({:?}.{:?}) is too old.",
                glow_gl.version().major,
                glow_gl.version().minor
            );
            println!("If not, please file a bug at https://github.com/servo/servo/issues.");
            let _ = device.destroy_context(&mut context);
        },
        Err(_) => {
            println!("Could not create any {desired_api:?} context.");
            println!("Ensure that OpenGL is working on your system.");
            println!("If it is, please file a bug at https://github.com/servo/servo/issues.");
        },
    }
    println!("===============================================================\n");
}

#[cfg(test)]
mod test {
    use dpi::PhysicalSize;
    use euclid::{Box2D, Point2D, Size2D};
    use gleam::gl;
    use image::Rgba;
    use surfman::{Connection, ContextAttributeFlags, ContextAttributes, Error, GLApi, GLVersion};

    use super::Framebuffer;
    use crate::rendering_context::SoftwareRenderingContext;

    #[test]
    #[expect(unsafe_code)]
    fn test_read_pixels() -> Result<(), Error> {
        let connection = Connection::new()?;
        let adapter = connection.create_software_adapter()?;
        let device = connection.create_device(&adapter)?;
        let context_descriptor = device.create_context_descriptor(&ContextAttributes {
            version: GLVersion::new(3, 0),
            flags: ContextAttributeFlags::empty(),
        })?;
        let mut context = device.create_context(&context_descriptor, None)?;

        let gl = match connection.gl_api() {
            GLApi::GL => unsafe { gl::GlFns::load_with(|s| device.get_proc_address(&context, s)) },
            GLApi::GLES => unsafe {
                gl::GlesFns::load_with(|s| device.get_proc_address(&context, s))
            },
        };

        device.make_context_current(&context)?;

        {
            const SIZE: u32 = 16;
            let framebuffer = Framebuffer::new(gl, PhysicalSize::new(SIZE, SIZE));
            framebuffer.bind();
            framebuffer
                .gl
                .clear_color(12.0 / 255.0, 34.0 / 255.0, 56.0 / 255.0, 78.0 / 255.0);
            framebuffer.gl.clear(gl::COLOR_BUFFER_BIT);

            let rect = Box2D::from_origin_and_size(Point2D::zero(), Size2D::new(SIZE, SIZE));
            let img = framebuffer
                .read_to_image(rect.to_i32())
                .expect("Should have been able to read back image.");
            assert_eq!(img.width(), SIZE);
            assert_eq!(img.height(), SIZE);

            let expected_pixel: Rgba<u8> = Rgba([12, 34, 56, 78]);
            assert!(img.pixels().all(|&p| p == expected_pixel));
        }

        device.destroy_context(&mut context)?;

        Ok(())
    }

    #[test]
    fn test_minimum_size_error() {
        let result = SoftwareRenderingContext::new(PhysicalSize {
            width: 0,
            height: 1,
        });
        match result {
            Err(surfman::Error::Failed) => (),
            _ => panic!("Expected {:?}", surfman::Error::Failed),
        }
    }

    #[test]
    fn video_escape_token_parses_external_only() {
        use super::{VideoEscapeMode, parse_video_escape_token};
        assert_eq!(
            parse_video_escape_token(Some("external")),
            VideoEscapeMode::External
        );
        // 제거된 토큰(native)은 안전하게 no-op(off)으로 폴백한다.
        assert_eq!(parse_video_escape_token(Some("native")), VideoEscapeMode::Off);
        assert_eq!(parse_video_escape_token(Some("1")), VideoEscapeMode::Off); // 미정의 값은 off
        assert_eq!(parse_video_escape_token(Some("")), VideoEscapeMode::Off);
        assert_eq!(parse_video_escape_token(None), VideoEscapeMode::Off);
    }
}

// surfman/surfman/src/platform/windows/angle/surface.rs
//
//! Surface management for Direct3D 11 on Windows using the ANGLE library as a frontend.

use super::context::{Context, ContextDescriptor};
use super::device::Device;
use crate::context::ContextID;
use crate::egl::types::EGLNativeWindowType;
use crate::egl::types::EGLSurface;
use crate::egl::{self, EGLint};
use crate::gl;
use crate::platform::generic::egl::device::EGL_FUNCTIONS;
use crate::platform::generic::egl::error::ToWindowingApiError;
use crate::platform::generic::egl::ffi::EGL_D3D11_TEXTURE_ANGLE;
use crate::platform::generic::egl::ffi::EGL_D3D_TEXTURE_2D_SHARE_HANDLE_ANGLE;
use crate::platform::generic::egl::ffi::EGL_D3D_TEXTURE_ANGLE;
use crate::platform::generic::egl::ffi::EGL_DIRECT_COMPOSITION_ANGLE;
use crate::platform::generic::egl::ffi::EGL_DXGI_KEYED_MUTEX_ANGLE;
use crate::platform::generic::egl::ffi::EGL_EXTENSION_FUNCTIONS;
use crate::platform::generic::egl::ffi::EGL_NO_IMAGE_KHR;
use crate::platform::generic::egl::ffi::{EGLClientBuffer, EGLImageKHR};
use crate::{Error, SurfaceAccess, SurfaceID, SurfaceInfo, SurfaceType};

use euclid::default::Size2D;
use glow::HasContext;
use log::{info, warn};
use std::fmt::{self, Debug, Formatter};
use std::marker::PhantomData;
use std::os::raw::c_void;
use std::ptr;
use std::sync::OnceLock;
use std::thread;
use winapi::shared::dxgi::IDXGIKeyedMutex;
use winapi::shared::winerror::{self, S_OK};
use winapi::um::d3d11;
use winapi::Interface;
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::um::libloaderapi::{GetModuleHandleW, LoadLibraryW};
use winapi::um::winbase::INFINITE;
use winapi::um::winnt::HANDLE;
use wio::com::ComPtr;

/// `SERVO_COMPOSITOR_DCOMP`/`gfx_dcomp_mode` 게이트가 요청된 상태(불리언)의 단일 저장소.
/// surfman은 저수준 크레이트라 `servo_config`에 의존시키면 의존이 역류하므로, pref
/// 문자열/3 상태(off/hybrid/surface) 파싱은 하지 않는다 — 그건
/// `paint_api::rendering_context::DcompMode::parse` 한 곳뿐이고, surfman은 그 결과인
/// 불리언만 주입받는다(리뷰 결과 이관 — 예전엔 이 파일이 3 상태 enum까지 들고 있었으나,
/// 파싱은 플랫폼 무관 순수 로직이라 paint_api로 올리고 저수준 크레이트는 최종 결과만
/// 받게 했다).
static DCOMP_NATIVE_COMPOSITOR_REQUESTED: OnceLock<bool> = OnceLock::new();

/// 기동 시 `paint_api::rendering_context::set_dcomp_mode()`(정상 경로) 또는 surfman
/// 단독 예제(`dcomp_native_poc`)가 직접 한 번 호출한다. **`RenderingContext` 생성 전에**
/// 불러야 한다 — 창 서피스 생성이 이미 이 값을 본다(아래 `dcomp_native_compositor_requested()`
/// 호출부 참고). 두 번째 호출은 무시된다(`OnceLock`) — 프로세스 수명 동안 한 번만
/// 확정된다.
pub fn set_dcomp_native_compositor(requested: bool) {
    let _ = DCOMP_NATIVE_COMPOSITOR_REQUESTED.set(requested);
}

/// `SERVO_COMPOSITOR_DCOMP` 게이트 판정의 단일 정본(공개 API — paint_api/paint가 재사용).
/// 공개 시그니처는 그대로 둔다 — 기존 호출부(device.rs 등)를 건드리지 않는다. 주입되지
/// 않았으면 기본 false(미구성 = off, env 폴백 없음 — 모든 소비자가 기동 시 명시
/// 주입한다). on이면 (1) 창 서피스는 DirectComposition 속성 없이 만들어 HWND의 DComp
/// 타깃을 Native Compositor 전용으로 남기고(CreateTargetForHwnd는 (hwnd, topmost)당 1개만
/// 허용), (2) EGL 디스플레이의 present-path-fast를 끈다 — ppf는
/// pbuffer(GL_FRAMEBUFFER_DEFAULT) 렌더링에도 발동해(ANGLE UsePresentPathFast) 시저 y
/// 자동 반전으로 WR NativeSurface의 top-left 규약을 깨뜨린다.
pub fn dcomp_native_compositor_requested() -> bool {
    DCOMP_NATIVE_COMPOSITOR_REQUESTED
        .get()
        .copied()
        .unwrap_or(false)
}

const SURFACE_GL_TEXTURE_TARGET: u32 = gl::TEXTURE_2D;
const DCOMP_DLL: [u16; 10] = [
    b'd' as u16,
    b'c' as u16,
    b'o' as u16,
    b'm' as u16,
    b'p' as u16,
    b'.' as u16,
    b'd' as u16,
    b'l' as u16,
    b'l' as u16,
    0,
];
static DCOMP_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Represents a hardware buffer of pixels that can be rendered to via the CPU or GPU and either
/// displayed in a native widget or bound to a texture for reading.
///
/// Surfaces come in two varieties: generic and widget surfaces. Generic surfaces can be bound to a
/// texture but cannot be displayed in a widget (without using other APIs such as Core Animation,
/// DirectComposition, or XPRESENT). Widget surfaces are the opposite: they can be displayed in a
/// widget but not bound to a texture.
///
/// Surfaces are specific to a given context and cannot be rendered to from any context other than
/// the one they were created with. However, they can be *read* from any context on any thread (as
/// long as that context shares the same adapter and connection), by wrapping them in a
/// `SurfaceTexture`.
///
/// Depending on the platform, each surface may be internally double-buffered.
///
/// Surfaces must be destroyed with the `destroy_surface()` method, or a panic will occur.
pub struct Surface {
    pub(crate) egl_surface: EGLSurface,
    pub(crate) size: Size2D<i32>,
    pub(crate) context_id: ContextID,
    pub(crate) context_descriptor: ContextDescriptor,
    pub(crate) win32_objects: Win32Objects,
}

/// Represents an OpenGL texture that wraps a surface.
///
/// Reading from the associated OpenGL texture reads from the surface. It is undefined behavior to
/// write to such a texture (e.g. by binding it to a framebuffer and rendering to that
/// framebuffer).
///
/// Surface textures are local to a context, but that context does not have to be the same context
/// as that associated with the underlying surface. The texture must be destroyed with the
/// `destroy_surface_texture()` method, or a panic will occur.
pub struct SurfaceTexture {
    pub(crate) surface: Surface,
    pub(crate) local_egl_surface: EGLSurface,
    pub(crate) local_keyed_mutex: Option<ComPtr<IDXGIKeyedMutex>>,
    pub(crate) gl_texture: Option<glow::Texture>,
    pub(crate) phantom: PhantomData<*const ()>,
}

unsafe impl Send for Surface {}

impl Debug for Surface {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        write!(f, "Surface({:x})", self.id().0)
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        if self.egl_surface != egl::NO_SURFACE && !thread::panicking() {
            panic!("Should have destroyed the surface first with `destroy_surface()`!")
        }
    }
}

impl Debug for SurfaceTexture {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        write!(f, "SurfaceTexture({:?})", self.surface)
    }
}

pub(crate) enum Win32Objects {
    Window,
    Pbuffer {
        share_handle: HANDLE,
        synchronization: Synchronization,
        // We keep a reference to the ComPtr in order to keep its refcount from becoming zero
        texture: Option<ComPtr<d3d11::ID3D11Texture2D>>,
    },
}

pub(crate) enum Synchronization {
    KeyedMutex(ComPtr<IDXGIKeyedMutex>),
    GLFinish,
    None,
}

/// Wraps an `EGLNativeWindowType`
#[repr(C)]
pub struct NativeWidget {
    /// A native window
    ///
    /// This can be a top-level window or a control.
    pub egl_native_window: EGLNativeWindowType,
}

fn ensure_dcomp_loaded() -> bool {
    *DCOMP_AVAILABLE.get_or_init(|| unsafe {
        if !GetModuleHandleW(DCOMP_DLL.as_ptr()).is_null() {
            return true;
        }
        !LoadLibraryW(DCOMP_DLL.as_ptr()).is_null()
    })
}

impl Device {
    /// Creates either a generic or a widget surface, depending on the supplied surface type.
    ///
    /// Only the given context may ever render to the surface, but generic surfaces can be wrapped
    /// up in a `SurfaceTexture` for reading by other contexts.
    pub fn create_surface(
        &self,
        context: &Context,
        _: SurfaceAccess,
        surface_type: SurfaceType<NativeWidget>,
    ) -> Result<Surface, Error> {
        match surface_type {
            SurfaceType::Generic { ref size } => self.create_pbuffer_surface(context, size, None),
            SurfaceType::Widget { ref native_widget } => {
                self.create_window_surface(context, native_widget)
            },
        }
    }

    #[allow(non_snake_case)]
    fn create_pbuffer_surface(
        &self,
        context: &Context,
        size: &Size2D<i32>,
        texture: Option<ComPtr<d3d11::ID3D11Texture2D>>,
    ) -> Result<Surface, Error> {
        let context_descriptor = self.context_descriptor(context);
        let egl_config = self.context_descriptor_to_egl_config(&context_descriptor);

        unsafe {
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
                0,
                0,
                0,
            ];

            EGL_FUNCTIONS.with(|egl| {
                let egl_surface = if let Some(ref texture) = texture {
                    let surface = egl.CreatePbufferFromClientBuffer(
                        self.egl_display,
                        EGL_D3D_TEXTURE_ANGLE,
                        texture.as_raw() as *const _,
                        egl_config,
                        attributes.as_ptr(),
                    );
                    assert_ne!(surface, egl::NO_SURFACE);
                    surface
                } else {
                    let surface =
                        egl.CreatePbufferSurface(self.egl_display, egl_config, attributes.as_ptr());
                    assert_ne!(surface, egl::NO_SURFACE);
                    surface
                };

                let eglQuerySurfacePointerANGLE =
                    EGL_EXTENSION_FUNCTIONS.QuerySurfacePointerANGLE.expect(
                        "Where's the `EGL_ANGLE_query_surface_pointer` \
                                                 extension?",
                    );

                let mut share_handle = INVALID_HANDLE_VALUE;
                let result = eglQuerySurfacePointerANGLE(
                    self.egl_display,
                    egl_surface,
                    EGL_D3D_TEXTURE_2D_SHARE_HANDLE_ANGLE as EGLint,
                    &mut share_handle,
                );
                assert_ne!(result, egl::FALSE);
                assert_ne!(share_handle, INVALID_HANDLE_VALUE);

                // `mozangle` builds ANGLE with keyed mutexes for sharing. Use the
                // `EGL_ANGLE_keyed_mutex` extension to fetch the keyed mutex so we can grab it.
                let mut keyed_mutex: *mut IDXGIKeyedMutex = ptr::null_mut();
                let result = eglQuerySurfacePointerANGLE(
                    self.egl_display,
                    egl_surface,
                    EGL_DXGI_KEYED_MUTEX_ANGLE as EGLint,
                    &mut keyed_mutex as *mut *mut IDXGIKeyedMutex as *mut *mut c_void,
                );
                let synchronization = if result != egl::FALSE && !keyed_mutex.is_null() {
                    let keyed_mutex = ComPtr::from_raw(keyed_mutex);
                    keyed_mutex.AddRef();
                    Synchronization::KeyedMutex(keyed_mutex)
                } else if texture.is_none() {
                    Synchronization::GLFinish
                } else {
                    Synchronization::None
                };

                Ok(Surface {
                    egl_surface,
                    size: *size,
                    context_id: context.id,
                    context_descriptor,
                    win32_objects: Win32Objects::Pbuffer {
                        share_handle,
                        synchronization,
                        texture,
                    },
                })
            })
        }
    }

    /// Given a D3D11 texture, create a surface that wraps that texture. This method is unsafe
    /// in that the resulting surface is only valid on the current thread.
    pub unsafe fn create_surface_from_texture(
        &self,
        context: &Context,
        size: &Size2D<i32>,
        texture: ComPtr<d3d11::ID3D11Texture2D>,
    ) -> Result<Surface, Error> {
        self.create_pbuffer_surface(context, size, Some(texture))
    }

    fn create_window_surface(
        &self,
        context: &Context,
        native_widget: &NativeWidget,
    ) -> Result<Surface, Error> {
        let context_descriptor = self.context_descriptor(context);
        let egl_config = self.context_descriptor_to_egl_config(&context_descriptor);

        unsafe {
            EGL_FUNCTIONS.with(|egl| {
                let default_attributes = [egl::NONE as EGLint];
                let direct_composition_attributes = [
                    EGL_DIRECT_COMPOSITION_ANGLE as EGLint,
                    egl::TRUE as EGLint,
                    egl::NONE as EGLint,
                ];

                let egl_surface = if ensure_dcomp_loaded() && !dcomp_native_compositor_requested() {
                    let surface = egl.CreateWindowSurface(
                        self.egl_display,
                        egl_config,
                        native_widget.egl_native_window,
                        direct_composition_attributes.as_ptr(),
                    );
                    if surface != egl::NO_SURFACE {
                        info!(
                            "Created ANGLE DirectComposition window surface for flip-model present"
                        );
                        surface
                    } else {
                        let error = egl.GetError();
                        warn!(
                            "Failed to create ANGLE DirectComposition window surface \
                             (EGL error 0x{error:x}); falling back to default HWND surface"
                        );
                        egl.CreateWindowSurface(
                            self.egl_display,
                            egl_config,
                            native_widget.egl_native_window,
                            default_attributes.as_ptr(),
                        )
                    }
                } else {
                    if dcomp_native_compositor_requested() {
                        info!(
                            "dcomp native compositor requested (gfx_dcomp_mode): creating plain \
                             HWND window surface (window DComp target reserved for the native \
                             compositor)"
                        );
                    } else {
                        warn!(
                            "dcomp.dll is unavailable; creating default ANGLE HWND surface without \
                             DirectComposition flip-model request"
                        );
                    }
                    egl.CreateWindowSurface(
                        self.egl_display,
                        egl_config,
                        native_widget.egl_native_window,
                        default_attributes.as_ptr(),
                    )
                };
                assert_ne!(egl_surface, egl::NO_SURFACE);

                let mut direct_composition = 0;
                if egl.QuerySurface(
                    self.egl_display,
                    egl_surface,
                    EGL_DIRECT_COMPOSITION_ANGLE as EGLint,
                    &mut direct_composition,
                ) != egl::FALSE
                {
                    info!("ANGLE window surface DirectComposition={direct_composition}");
                }

                let mut width = 0;
                let mut height = 0;
                egl.QuerySurface(
                    self.egl_display,
                    egl_surface,
                    egl::WIDTH as EGLint,
                    &mut width,
                );
                egl.QuerySurface(
                    self.egl_display,
                    egl_surface,
                    egl::HEIGHT as EGLint,
                    &mut height,
                );
                assert_ne!(width, 0);
                assert_ne!(height, 0);

                Ok(Surface {
                    egl_surface,
                    size: Size2D::new(width, height),
                    context_id: context.id,
                    context_descriptor,
                    win32_objects: Win32Objects::Window,
                })
            })
        }
    }

    /// Creates a surface texture from an existing generic surface for use with the given context.
    ///
    /// The surface texture is local to the supplied context and takes ownership of the surface.
    /// Destroying the surface texture allows you to retrieve the surface again.
    ///
    /// *The supplied context does not have to be the same context that the surface is associated
    /// with.* This allows you to render to a surface in one context and sample from that surface
    /// in another context.
    ///
    /// Calling this method on a widget surface returns a `WidgetAttached` error.
    #[allow(non_snake_case)]
    pub fn create_surface_texture(
        &self,
        context: &mut Context,
        surface: Surface,
    ) -> Result<SurfaceTexture, (Error, Surface)> {
        let share_handle = match surface.win32_objects {
            Win32Objects::Window => return Err((Error::WidgetAttached, surface)),
            Win32Objects::Pbuffer { share_handle, .. } => share_handle,
        };

        let local_egl_config = self.context_descriptor_to_egl_config(&surface.context_descriptor);
        EGL_FUNCTIONS.with(|egl| {
            unsafe {
                // First, create an EGL surface local to this thread.
                let pbuffer_attributes = [
                    egl::WIDTH as EGLint,
                    surface.size.width,
                    egl::HEIGHT as EGLint,
                    surface.size.height,
                    egl::TEXTURE_FORMAT as EGLint,
                    egl::TEXTURE_RGBA as EGLint,
                    egl::TEXTURE_TARGET as EGLint,
                    egl::TEXTURE_2D as EGLint,
                    egl::NONE as EGLint,
                    0,
                    0,
                    0,
                ];

                // Import the shared texture on THIS device explicitly. The
                // EGL_D3D_TEXTURE_2D_SHARE_HANDLE_ANGLE path resolves the share handle against
                // ANGLE's process-default device (the primary GPU's). When the consumer device
                // lives on a different GPU than the primary, that path returns EGL_BAD_PARAMETER and
                // the tile renders black. Open the shared resource on `self.d3d11_device` (the
                // consumer/compositor device) so it stays on the correct adapter, then wrap the
                // resulting texture via EGL_D3D_TEXTURE_ANGLE.
                let mut local_d3d11_texture: *mut c_void = ptr::null_mut();
                let open_result = self.d3d11_device.OpenSharedResource(
                    share_handle,
                    &d3d11::ID3D11Texture2D::uuidof(),
                    &mut local_d3d11_texture,
                );
                if !winerror::SUCCEEDED(open_result) || local_d3d11_texture.is_null() {
                    let (lh, ll) = self.d3d11_adapter_luid();
                    warn!(
                        "WebGL surface import: OpenSharedResource failed hr={open_result:#x} \
                         importer_adapter_luid={lh}:{ll}"
                    );
                    return Err((Error::Failed, surface));
                }
                let local_d3d11_texture =
                    ComPtr::from_raw(local_d3d11_texture as *mut d3d11::ID3D11Texture2D);

                let local_egl_surface = egl.CreatePbufferFromClientBuffer(
                    self.egl_display,
                    EGL_D3D_TEXTURE_ANGLE,
                    local_d3d11_texture.as_raw() as *const c_void,
                    local_egl_config,
                    pbuffer_attributes.as_ptr(),
                );
                if local_egl_surface == egl::NO_SURFACE {
                    let windowing_api_error = egl.GetError().to_windowing_api_error();
                    return Err((Error::SurfaceImportFailed(windowing_api_error), surface));
                }

                let mut local_keyed_mutex: *mut IDXGIKeyedMutex = ptr::null_mut();
                let eglQuerySurfacePointerANGLE =
                    EGL_EXTENSION_FUNCTIONS.QuerySurfacePointerANGLE.unwrap();
                let result = eglQuerySurfacePointerANGLE(
                    self.egl_display,
                    local_egl_surface,
                    EGL_DXGI_KEYED_MUTEX_ANGLE as EGLint,
                    &mut local_keyed_mutex as *mut *mut IDXGIKeyedMutex as *mut *mut c_void,
                );
                let local_keyed_mutex = if result != egl::FALSE && !local_keyed_mutex.is_null() {
                    let local_keyed_mutex = ComPtr::from_raw(local_keyed_mutex);
                    local_keyed_mutex.AddRef();

                    let result = local_keyed_mutex.AcquireSync(0, INFINITE);
                    assert_eq!(result, S_OK);

                    Some(local_keyed_mutex)
                } else {
                    None
                };
                self.create_surface_texture_from_local_surface(
                    context,
                    surface,
                    local_egl_surface,
                    local_keyed_mutex,
                )
            }
        })
    }

    fn create_surface_texture_from_local_surface(
        &self,
        context: &Context,
        surface: Surface,
        local_egl_surface: EGLSurface,
        local_keyed_mutex: Option<ComPtr<IDXGIKeyedMutex>>,
    ) -> Result<SurfaceTexture, (Error, Surface)> {
        EGL_FUNCTIONS.with(|egl| {
            unsafe {
                let _guard = match self.temporarily_make_context_current(context) {
                    Ok(guard) => guard,
                    Err(error) => return Err((error, surface)),
                };

                let gl = &context.gl;
                let previous_texture = gl.get_parameter_texture(gl::TEXTURE_BINDING_2D);
                // Then bind that surface to the texture.
                let texture = gl.create_texture().unwrap();

                gl.bind_texture(gl::TEXTURE_2D, Some(texture));
                if egl.BindTexImage(self.egl_display, local_egl_surface, egl::BACK_BUFFER as _)
                    == egl::FALSE
                {
                    let windowing_api_error = egl.GetError().to_windowing_api_error();
                    gl.bind_texture(gl::TEXTURE_2D, previous_texture);
                    gl.delete_texture(texture);
                    return Err((
                        Error::SurfaceTextureCreationFailed(windowing_api_error),
                        surface,
                    ));
                }

                // Initialize the texture, for convenience.
                gl.tex_parameter_i32(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as _);
                gl.tex_parameter_i32(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as _);
                gl.tex_parameter_i32(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as _);
                gl.tex_parameter_i32(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as _);

                gl.bind_texture(gl::TEXTURE_2D, previous_texture);
                debug_assert_eq!(gl.get_error(), gl::NO_ERROR);

                Ok(SurfaceTexture {
                    surface,
                    local_egl_surface,
                    local_keyed_mutex,
                    gl_texture: Some(texture),
                    phantom: PhantomData,
                })
            }
        })
    }

    /// Given a D3D11 texture, create a surface texture that wraps that texture. This method is unsafe
    /// in that the resulting surface is only valid on the current thread, for the lifetime of `texture`.
    /// It is the caller's responsibility to ensure that `texture` is not freed while the `SurfaceTexture` is live.
    pub unsafe fn create_surface_texture_from_texture(
        &self,
        context: &mut Context,
        size: &Size2D<i32>,
        texture: ComPtr<d3d11::ID3D11Texture2D>,
    ) -> Result<SurfaceTexture, Error> {
        let surface = self.create_pbuffer_surface(context, size, Some(texture))?;
        let local_egl_surface = surface.egl_surface;
        self.create_surface_texture_from_local_surface(context, surface, local_egl_surface, None)
            .map_err(|(err, mut surface)| {
                let _ = self.destroy_surface(context, &mut surface);
                err
            })
    }

    /// Import an external DXGI shared resource handle (e.g. a D3D12 texture shared by the
    /// WebGPU thread for GPU-direct present) and wrap it as a [`SurfaceTexture`] (GL texture)
    /// on this compositor device. The handle is opened on `self.d3d11_device` so it stays on
    /// the consumer GPU, then wrapped via `EGL_D3D_TEXTURE_ANGLE` (same path the cross-GPU
    /// WebGL surface import uses).
    ///
    /// Caller is responsible for synchronization (the producer must have finished writing the
    /// shared texture before it is sampled) and for not freeing the source while live.
    pub fn create_surface_texture_from_shared_handle(
        &self,
        context: &mut Context,
        size: &Size2D<i32>,
        handle: HANDLE,
    ) -> Result<SurfaceTexture, Error> {
        unsafe {
            let mut local_d3d11_texture: *mut c_void = ptr::null_mut();
            let open_result = self.d3d11_device.OpenSharedResource(
                handle,
                &d3d11::ID3D11Texture2D::uuidof(),
                &mut local_d3d11_texture,
            );
            if !winerror::SUCCEEDED(open_result) || local_d3d11_texture.is_null() {
                let (lh, ll) = self.d3d11_adapter_luid();
                warn!(
                    "WebGPU GPU-direct import: OpenSharedResource failed hr={open_result:#x} \
                     importer_adapter_luid={lh}:{ll}"
                );
                return Err(Error::Failed);
            }
            let local_d3d11_texture =
                ComPtr::from_raw(local_d3d11_texture as *mut d3d11::ID3D11Texture2D);
            self.create_surface_texture_from_texture(context, size, local_d3d11_texture)
        }
    }

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
                warn!(
                    "CreateImageKHR(EGL_D3D11_TEXTURE_ANGLE) 실패 err={:#x}",
                    egl.GetError()
                );
            });
            return Err(Error::Failed);
        }
        let gl = &context.gl;
        let previous_texture = gl.get_parameter_texture(gl::TEXTURE_BINDING_2D);
        let gl_texture = match gl.create_texture() {
            Ok(tex) => tex,
            Err(_) => {
                (EGL_EXTENSION_FUNCTIONS.DestroyImageKHR)(self.egl_display, egl_image);
                return Err(Error::Failed);
            }
        };
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

    /// create_gl_texture_from_d3d11_texture의 짝. EGLImage와 GL 텍스처를 함께 파기한다.
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

    /// Destroys a surface.
    ///
    /// The supplied context must be the context the surface is associated with, or this returns
    /// an `IncompatibleSurface` error.
    ///
    /// You must explicitly call this method to dispose of a surface. Otherwise, a panic occurs in
    /// the `drop` method.
    pub fn destroy_surface(
        &self,
        context: &mut Context,
        surface: &mut Surface,
    ) -> Result<(), Error> {
        if context.id != surface.context_id {
            return Err(Error::IncompatibleSurface);
        }

        EGL_FUNCTIONS.with(|egl| {
            unsafe {
                // If the surface is currently bound, unbind it.
                if egl.GetCurrentSurface(egl::READ as EGLint) == surface.egl_surface
                    || egl.GetCurrentSurface(egl::DRAW as EGLint) == surface.egl_surface
                {
                    self.make_no_context_current()?;
                }

                egl.DestroySurface(self.egl_display, surface.egl_surface);
                surface.egl_surface = egl::NO_SURFACE;
                if let Win32Objects::Pbuffer {
                    ref mut texture, ..
                } = surface.win32_objects
                {
                    texture.take();
                }
            }
            Ok(())
        })
    }

    /// Destroys a surface texture and returns the underlying surface.
    ///
    /// The supplied context must be the same context the surface texture was created with, or an
    /// `IncompatibleSurfaceTexture` error is returned.
    ///
    /// All surface textures must be explicitly destroyed with this function, or a panic will
    /// occur.
    pub fn destroy_surface_texture(
        &self,
        context: &mut Context,
        mut surface_texture: SurfaceTexture,
    ) -> Result<Surface, (Error, SurfaceTexture)> {
        // The external-texture import path (`create_surface_texture_from_texture`, used by
        // `create_surface_texture_from_shared_handle`) reuses the surface's own pbuffer as
        // `local_egl_surface`, so the `DestroySurface` below also destroys the returned
        // surface's EGL surface. Detect that aliasing up front (while both handles are still
        // live; the regular `create_surface_texture` path allocates a distinct local pbuffer,
        // so equality uniquely identifies the import path) and neuter the returned `Surface`
        // afterwards, mirroring `destroy_surface`. Otherwise dropping the returned husk
        // panics in `Surface::drop`.
        let is_import_path =
            surface_texture.surface.egl_surface == surface_texture.local_egl_surface;
        unsafe {
            let _guard = match self.temporarily_make_context_current(context) {
                Ok(guard) => guard,
                Err(error) => return Err((error, surface_texture)),
            };

            let result = EGL_FUNCTIONS.with(|egl| {
                if egl.ReleaseTexImage(
                    self.egl_display,
                    surface_texture.local_egl_surface,
                    egl::BACK_BUFFER as _,
                ) == egl::FALSE
                {
                    return Err(Error::SurfaceTextureCreationFailed(
                        egl.GetError().to_windowing_api_error(),
                    ));
                }

                if let Some(texture) = surface_texture.gl_texture.take() {
                    context.gl.delete_texture(texture);
                }

                if let Some(ref local_keyed_mutex) = surface_texture.local_keyed_mutex {
                    let result = local_keyed_mutex.ReleaseSync(0);
                    assert_eq!(result, S_OK);
                }

                egl.DestroySurface(self.egl_display, surface_texture.local_egl_surface);
                Ok(())
            });
            if let Err(error) = result {
                return Err((error, surface_texture));
            }
        }

        if is_import_path {
            // Same cleanup `destroy_surface` performs: clearing `egl_surface` makes the
            // returned husk drop-safe (its EGL surface was just destroyed above), and taking
            // the wrapped D3D11 texture eagerly releases the COM reference now. The `ComPtr`
            // would also be released by the struct's natural drop, so the `take()` is for
            // symmetry with `destroy_surface` rather than for correctness.
            surface_texture.surface.egl_surface = egl::NO_SURFACE;
            if let Win32Objects::Pbuffer {
                ref mut texture, ..
            } = surface_texture.surface.win32_objects
            {
                texture.take();
            }
        }

        Ok(surface_texture.surface)
    }

    /// Returns the OpenGL texture target needed to read from this surface texture.
    ///
    /// This will be `GL_TEXTURE_2D` or `GL_TEXTURE_RECTANGLE`, depending on platform.
    #[inline]
    pub fn surface_gl_texture_target(&self) -> u32 {
        SURFACE_GL_TEXTURE_TARGET
    }

    /// Returns a pointer to the underlying surface data for reading or writing by the CPU.
    #[inline]
    pub fn lock_surface_data<'s>(
        &self,
        _surface: &'s mut Surface,
    ) -> Result<SurfaceDataGuard<'s>, Error> {
        Err(Error::Unimplemented)
    }

    /// Displays the contents of a widget surface on screen.
    ///
    /// Widget surfaces are internally double-buffered, so changes to them don't show up in their
    /// associated widgets until this method is called.
    ///
    /// The supplied context must match the context the surface was created with, or an
    /// `IncompatibleSurface` error is returned.
    pub fn present_surface(&self, _: &Context, surface: &mut Surface) -> Result<(), Error> {
        match surface.win32_objects {
            Win32Objects::Window { .. } => {},
            _ => return Err(Error::NoWidgetAttached),
        }

        EGL_FUNCTIONS.with(|egl| unsafe {
            let ok = egl.SwapBuffers(self.egl_display, surface.egl_surface);
            assert_ne!(ok, egl::FALSE);
            Ok(())
        })
    }

    /// Resizes a widget surface.
    pub fn resize_surface(
        &self,
        _context: &Context,
        surface: &mut Surface,
        size: Size2D<i32>,
    ) -> Result<(), Error> {
        surface.size = size;
        Ok(())
    }

    /// Returns various information about the surface, including the framebuffer object needed to
    /// render to this surface.
    ///
    /// Before rendering to a surface attached to a context, you must call `glBindFramebuffer()`
    /// on the framebuffer object returned by this function. This framebuffer object may or not be
    /// 0, the default framebuffer, depending on platform.
    #[inline]
    pub fn surface_info(&self, surface: &Surface) -> SurfaceInfo {
        SurfaceInfo {
            size: surface.size,
            id: surface.id(),
            context_id: surface.context_id,
            framebuffer_object: None,
        }
    }

    /// Returns the OpenGL texture object containing the contents of this surface.
    ///
    /// It is only legal to read from, not write to, this texture object.
    #[inline]
    pub fn surface_texture_object(
        &self,
        surface_texture: &SurfaceTexture,
    ) -> Option<glow::Texture> {
        surface_texture.gl_texture
    }
}

impl Surface {
    #[inline]
    fn id(&self) -> SurfaceID {
        SurfaceID(self.egl_surface as usize)
    }

    #[inline]
    pub(crate) fn uses_gl_finish(&self) -> bool {
        match self.win32_objects {
            Win32Objects::Pbuffer {
                synchronization: Synchronization::GLFinish,
                ..
            } => true,
            _ => false,
        }
    }

    /// Returns the DXGI share handle if it has one.
    #[inline]
    pub fn share_handle(&self) -> Option<HANDLE> {
        match self.win32_objects {
            Win32Objects::Pbuffer { share_handle, .. } => Some(share_handle),
            _ => None,
        }
    }
}

/// Represents the CPU view of the pixel data of this surface.
pub struct SurfaceDataGuard<'a> {
    phantom: PhantomData<&'a ()>,
}

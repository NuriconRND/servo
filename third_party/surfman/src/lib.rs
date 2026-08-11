// surfman/surfman/src/lib.rs
//
//! Cross-platform GPU device and surface management.
//!
//! You can use this crate to multithread a graphics application so that rendering happens on
//! multiple threads, sharing textures among them in the most efficient manner. It may also be
//! useful as a lightweight framework for *just* initializing rendering in native applications.
//! This is in contrast to crates like SDL, GLFW, winit, and Glutin, all of which have a broader
//! focus in that they manage windowing and the event loop as well.

#![warn(missing_docs)]

#[macro_use]
extern crate bitflags;
#[allow(unused_imports)]
#[macro_use]
extern crate log;

pub mod platform;
pub use platform::default::connection::{Connection, NativeConnection};
pub use platform::default::context::{Context, ContextDescriptor, NativeContext};
pub use platform::default::device::{Adapter, Device, NativeDevice};
pub use platform::default::surface::{NativeWidget, Surface, SurfaceTexture};

// SERVO_COMPOSITOR_DCOMP/gfx.dcomp.mode 게이트 판정의 단일 정본은
// `paint_api::rendering_context::DcompMode`(파싱, 3 상태)다 — surfman은 저수준 크레이트라
// 그 결과인 불리언만 받는다(`set_dcomp_native_compositor`). surfman 내부(창 서피스 DComp
// 속성 억제 + present-path-fast 비활성)는 이 불리언을 `dcomp_native_compositor_requested()`
// 로 읽는다(중복 env/문자열 파싱 금지 — surfman은 문자열을 아예 보지 않는다).
#[cfg(all(target_os = "windows", feature = "sm-angle"))]
pub use platform::windows::angle::surface::{
    dcomp_native_compositor_requested, set_dcomp_native_compositor,
};

// TODO(pcwalton): Fill this in with other OS's.
#[cfg(target_os = "macos")]
pub use platform::system::connection::Connection as SystemConnection;
#[cfg(target_os = "macos")]
pub use platform::system::device::{Adapter as SystemAdapter, Device as SystemDevice};
#[cfg(target_os = "macos")]
pub use platform::system::surface::Surface as SystemSurface;

#[cfg(feature = "chains")]
pub mod chains;
pub mod connection;
pub mod device;

pub mod error;
pub use crate::error::{Error, WindowingApiError};

mod context;
pub use crate::context::{ContextAttributeFlags, ContextAttributes, ContextID};

mod info;
pub use crate::info::{GLApi, GLVersion};

mod surface;
pub use crate::surface::{SurfaceAccess, SurfaceID, SurfaceInfo, SurfaceType, SystemSurfaceInfo};

pub mod macros;
pub(crate) use macros::implement_interfaces;

pub(crate) use glow::{self as gl, Context as Gl};

mod gl_utils;
mod renderbuffers;

#[cfg(any(
    target_os = "android",
    target_env = "ohos",
    all(target_os = "windows", feature = "sm-angle"),
    unix
))]
#[allow(non_camel_case_types)]
#[allow(clippy::all)]
mod egl {
    use std::os::raw::{c_long, c_void};
    pub type khronos_utime_nanoseconds_t = khronos_uint64_t;
    pub type khronos_uint64_t = u64;
    pub type khronos_ssize_t = c_long;
    pub type EGLint = i32;
    pub type EGLNativeDisplayType = *const c_void;
    pub type EGLNativePixmapType = *const c_void;
    pub type EGLNativeWindowType = *const c_void;
    pub type NativeDisplayType = EGLNativeDisplayType;
    pub type NativePixmapType = EGLNativePixmapType;
    pub type NativeWindowType = EGLNativeWindowType;
    include!(concat!(env!("OUT_DIR"), "/egl_bindings.rs"));
}

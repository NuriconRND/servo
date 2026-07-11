// surfman/surfman/src/platform/windows/angle/device.rs
//
//! A thread-local handle to the device.

use super::connection::Connection;
use crate::egl;
use crate::egl::types::{EGLAttrib, EGLDeviceEXT, EGLDisplay, EGLint};
use crate::platform::generic::egl::device::EGL_FUNCTIONS;
use crate::platform::generic::egl::ffi::EGL_DEVICE_EXT;
use crate::platform::generic::egl::ffi::{
    EGL_D3D11_DEVICE_ANGLE, EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE,
    EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE, EGL_EXTENSION_FUNCTIONS, EGL_NO_DEVICE_EXT,
    EGL_PLATFORM_ANGLE_ANGLE, EGL_PLATFORM_ANGLE_D3D_LUID_HIGH_ANGLE,
    EGL_PLATFORM_ANGLE_D3D_LUID_LOW_ANGLE, EGL_PLATFORM_ANGLE_DEVICE_TYPE_ANGLE,
    EGL_PLATFORM_ANGLE_DEVICE_TYPE_D3D_WARP_ANGLE, EGL_PLATFORM_ANGLE_TYPE_ANGLE,
    EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE, EGL_PLATFORM_DEVICE_EXT,
};
use crate::{Error, GLApi};

use log::{info, warn};
use std::cell::{RefCell, RefMut};
use std::mem;
use std::os::raw::c_void;
use std::ptr;
use winapi::Interface;
use winapi::shared::dxgi::{self, IDXGIAdapter, IDXGIDevice, IDXGIFactory1};
use winapi::shared::guiddef::GUID;
use winapi::shared::minwindef::{BOOL, TRUE, UINT};
use winapi::shared::winerror::{self, S_OK};
use winapi::um::d3d11::{D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_SDK_VERSION};
use winapi::um::d3dcommon::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP};
use winapi::um::unknwnbase::{IUnknown, IUnknownVtbl};
use wio::com::ComPtr;

const IID_ID3D11_MULTITHREAD: GUID = GUID {
    Data1: 0x9b7e4e00,
    Data2: 0x342c,
    Data3: 0x4106,
    Data4: [0xa1, 0x9f, 0x4f, 0x27, 0x04, 0xf6, 0x89, 0xf0],
};

#[repr(C)]
#[allow(non_snake_case)]
struct ID3D11Multithread {
    lpVtbl: *const ID3D11MultithreadVtbl,
}

#[repr(C)]
#[allow(non_snake_case)]
struct ID3D11MultithreadVtbl {
    parent: IUnknownVtbl,
    Enter: unsafe extern "system" fn(this: *mut ID3D11Multithread),
    Leave: unsafe extern "system" fn(this: *mut ID3D11Multithread),
    SetMultithreadProtected:
        unsafe extern "system" fn(this: *mut ID3D11Multithread, bMTProtect: BOOL) -> BOOL,
    GetMultithreadProtected: unsafe extern "system" fn(this: *mut ID3D11Multithread) -> BOOL,
}

thread_local! {
    static DXGI_FACTORY: RefCell<Option<ComPtr<IDXGIFactory1>>> = RefCell::new(None);
}

/// Represents a hardware display adapter that can be used for rendering (including the CPU).
///
/// Adapters can be sent between threads. To render with an adapter, open a thread-local `Device`.
#[derive(Clone)]
pub struct Adapter {
    pub(crate) dxgi_adapter: ComPtr<IDXGIAdapter>,
    pub(crate) d3d_driver_type: D3D_DRIVER_TYPE,
}

unsafe impl Send for Adapter {}

/// A thread-local handle to a device.
///
/// Devices contain most of the relevant surface management methods.
pub struct Device {
    pub(crate) egl_display: EGLDisplay,
    pub(crate) d3d11_device: ComPtr<ID3D11Device>,
    pub(crate) d3d_driver_type: D3D_DRIVER_TYPE,
    pub(crate) display_is_owned: bool,
}

pub(crate) enum VendorPreference {
    None,
    Prefer(UINT),
    Avoid(UINT),
}

/// Wraps a Direct3D 11 device and its associated EGL display.
#[derive(Clone)]
pub struct NativeDevice {
    /// The ANGLE EGL display.
    pub egl_display: EGLDisplay,
    /// The Direct3D 11 device that the ANGLE EGL display was created with.
    pub d3d11_device: *mut ID3D11Device,
    /// The Direct3D driver type that the device was created with.
    pub d3d_driver_type: D3D_DRIVER_TYPE,
}

impl Adapter {
    pub(crate) fn new(
        d3d_driver_type: D3D_DRIVER_TYPE,
        vendor_preference: VendorPreference,
    ) -> Result<Adapter, Error> {
        unsafe {
            let dxgi_factory = DXGI_FACTORY.with(|dxgi_factory_slot| {
                let mut dxgi_factory_slot: RefMut<Option<ComPtr<IDXGIFactory1>>> =
                    dxgi_factory_slot.borrow_mut();
                if dxgi_factory_slot.is_none() {
                    let mut dxgi_factory: *mut IDXGIFactory1 = ptr::null_mut();
                    let result = dxgi::CreateDXGIFactory1(
                        &IDXGIFactory1::uuidof(),
                        &mut dxgi_factory as *mut *mut IDXGIFactory1 as *mut *mut c_void,
                    );
                    if !winerror::SUCCEEDED(result) {
                        return Err(Error::Failed);
                    }
                    assert!(!dxgi_factory.is_null());
                    *dxgi_factory_slot = Some(ComPtr::from_raw(dxgi_factory));
                }
                Ok((*dxgi_factory_slot).clone().unwrap())
            })?;

            // Find the first adapter that matches the vendor preference.
            let mut adapter_index = 0;
            loop {
                let mut dxgi_adapter_1 = ptr::null_mut();
                let result = (*dxgi_factory).EnumAdapters1(adapter_index, &mut dxgi_adapter_1);
                if !winerror::SUCCEEDED(result) {
                    break;
                }
                assert!(!dxgi_adapter_1.is_null());
                let dxgi_adapter_1 = ComPtr::from_raw(dxgi_adapter_1);

                let mut adapter_desc = mem::zeroed();
                let result = (*dxgi_adapter_1).GetDesc1(&mut adapter_desc);
                assert_eq!(result, S_OK);

                let choose_this = match vendor_preference {
                    VendorPreference::Prefer(vendor_id) => vendor_id == adapter_desc.VendorId,
                    VendorPreference::Avoid(vendor_id) => vendor_id != adapter_desc.VendorId,
                    VendorPreference::None => true,
                };
                if choose_this {
                    let mut dxgi_adapter: *mut IDXGIAdapter = ptr::null_mut();
                    let result = (*dxgi_adapter_1).QueryInterface(
                        &IDXGIAdapter::uuidof(),
                        &mut dxgi_adapter as *mut *mut IDXGIAdapter as *mut *mut c_void,
                    );
                    assert_eq!(result, S_OK);
                    let dxgi_adapter = ComPtr::from_raw(dxgi_adapter);

                    return Ok(Adapter {
                        dxgi_adapter,
                        d3d_driver_type,
                    });
                }

                adapter_index += 1;
            }

            // Fallback: Go with the first adapter.
            let mut dxgi_adapter_1 = ptr::null_mut();
            let result = (*dxgi_factory).EnumAdapters1(0, &mut dxgi_adapter_1);
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
            assert!(winerror::SUCCEEDED(result));
            let dxgi_adapter = ComPtr::from_raw(dxgi_adapter);

            Ok(Adapter {
                dxgi_adapter,
                d3d_driver_type,
            })
        }
    }

    /// Create an Adapter instance wrapping an existing DXGI adapter.
    pub fn from_dxgi_adapter(adapter: ComPtr<IDXGIAdapter>) -> Adapter {
        Adapter {
            dxgi_adapter: adapter,
            d3d_driver_type: D3D_DRIVER_TYPE_UNKNOWN,
        }
    }
}

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

impl Device {
    #[allow(non_snake_case)]
    pub(crate) fn new(adapter: &Adapter) -> Result<Device, Error> {
        let d3d_driver_type = adapter.d3d_driver_type;
        unsafe {
            let mut adapter_desc = mem::zeroed();
            let result = (*adapter.dxgi_adapter).GetDesc(&mut adapter_desc);
            if !winerror::SUCCEEDED(result) {
                return Err(Error::DeviceOpenFailed);
            }

            EGL_FUNCTIONS.with(|egl| {
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
                if egl_display == egl::NO_DISPLAY {
                    return Err(Error::DeviceOpenFailed);
                }

                // I don't think this should ever fail.
                let (mut major_version, mut minor_version) = (0, 0);
                let result = egl.Initialize(egl_display, &mut major_version, &mut minor_version);
                if result == egl::FALSE {
                    return Err(Error::DeviceOpenFailed);
                }
                let d3d11_device = query_d3d11_device_for_egl_display(egl_display)?;
                enable_d3d11_multithread_protection(&d3d11_device);

                Ok(Device {
                    egl_display,
                    d3d11_device,
                    d3d_driver_type,
                    display_is_owned: true,
                })
            })
        }
    }

    /// Like `new`, but creates a brand-new, dedicated D3D11 device for the adapter instead of
    /// reusing ANGLE's per-LUID cached `EGLDisplay`.
    ///
    /// `new` selects the display with `EGL_PLATFORM_ANGLE_ANGLE` keyed by the adapter LUID, which
    /// ANGLE caches: every `Device` opened for the same GPU then shares one `EGLDisplay` and one
    /// underlying D3D11 device. That is fine for cooperating consumers, but a WebGL backend context
    /// that renders on its own thread while the compositor reads its surfaces on another thread ends
    /// up driving the same ANGLE renderer from two threads. The shared internal state gets corrupted
    /// (StateManager11 dirty-bit invariant violation -> access violation in libGLESv2). Creating a
    /// dedicated D3D11 device here, wrapped via `EGL_PLATFORM_DEVICE_EXT`, gives such a context a
    /// fully isolated ANGLE display; surfaces are still shared with the compositor's device through
    /// the usual DXGI shared-handle path.
    pub(crate) fn new_isolated(adapter: &Adapter) -> Result<Device, Error> {
        let d3d_driver_type = adapter.d3d_driver_type;
        unsafe {
            // Resolve which adapter ANGLE actually binds the requested LUID to. ANGLE may fall back
            // to the default GPU for a non-default LUID, and the compositor (which opens this
            // context's surface textures) uses that same LUID display. If we built our dedicated
            // device on a different adapter than the compositor's, the compositor's
            // OpenSharedResource would fail with E_INVALIDARG and the tile would render black. So
            // mirror the compositor: open the LUID display exactly as `new` does, take the adapter
            // of its D3D11 device, and build our dedicated device on that same adapter. (We do not
            // terminate this display; ANGLE caches and shares it with the compositor.)
            let resolved_dxgi_adapter: Option<ComPtr<IDXGIAdapter>> =
                if d3d_driver_type == D3D_DRIVER_TYPE_WARP {
                    None
                } else {
                    let mut adapter_desc = mem::zeroed();
                    if !winerror::SUCCEEDED((*adapter.dxgi_adapter).GetDesc(&mut adapter_desc)) {
                        return Err(Error::DeviceOpenFailed);
                    }
                    let luid_d3d11 =
                        EGL_FUNCTIONS.with(|egl| -> Result<ComPtr<ID3D11Device>, Error> {
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
                            if display == egl::NO_DISPLAY {
                                return Err(Error::DeviceOpenFailed);
                            }
                            let (mut maj, mut min) = (0, 0);
                            if egl.Initialize(display, &mut maj, &mut min) == egl::FALSE {
                                return Err(Error::DeviceOpenFailed);
                            }
                            query_d3d11_device_for_egl_display(display)
                        })?;

                    let mut dxgi_device: *mut IDXGIDevice = ptr::null_mut();
                    if !winerror::SUCCEEDED((*luid_d3d11).QueryInterface(
                        &IDXGIDevice::uuidof(),
                        &mut dxgi_device as *mut *mut IDXGIDevice as *mut *mut c_void,
                    )) {
                        return Err(Error::DeviceOpenFailed);
                    }
                    let dxgi_device = ComPtr::from_raw(dxgi_device);
                    let mut dxgi_adapter: *mut IDXGIAdapter = ptr::null_mut();
                    if !winerror::SUCCEEDED((*dxgi_device).GetAdapter(&mut dxgi_adapter)) {
                        return Err(Error::DeviceOpenFailed);
                    }
                    Some(ComPtr::from_raw(dxgi_adapter))
                };

            let (d3d11_adapter, create_driver_type) = match resolved_dxgi_adapter {
                Some(ref a) => (a.as_raw(), D3D_DRIVER_TYPE_UNKNOWN),
                None => (ptr::null_mut(), D3D_DRIVER_TYPE_WARP),
            };

            let mut d3d11_device = ptr::null_mut();
            let mut d3d11_feature_level = 0;
            let result = D3D11CreateDevice(
                d3d11_adapter,
                create_driver_type,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                0,
                D3D11_SDK_VERSION,
                &mut d3d11_device,
                &mut d3d11_feature_level,
                ptr::null_mut(),
            );
            if !winerror::SUCCEEDED(result) {
                return Err(Error::DeviceOpenFailed);
            }
            let d3d11_device = ComPtr::from_raw(d3d11_device);
            if let Some(ref a) = resolved_dxgi_adapter {
                let mut rdesc = mem::zeroed();
                if winerror::SUCCEEDED((**a).GetDesc(&mut rdesc)) {
                    info!(
                        "Opened isolated ANGLE D3D11 device on resolved adapter LUID {}:{} \
                         (feature_level={:#x})",
                        rdesc.AdapterLuid.HighPart, rdesc.AdapterLuid.LowPart, d3d11_feature_level,
                    );
                }
            }

            let create_device_angle = EGL_EXTENSION_FUNCTIONS
                .CreateDeviceANGLE
                .expect("Where's the `EGL_ANGLE_device_creation` extension?");
            let egl_device = create_device_angle(
                EGL_D3D11_DEVICE_ANGLE as EGLint,
                d3d11_device.as_raw() as *mut c_void,
                ptr::null_mut(),
            );
            if egl_device == EGL_NO_DEVICE_EXT {
                return Err(Error::DeviceOpenFailed);
            }

            EGL_FUNCTIONS.with(|egl| {
                let attribs = [egl::NONE as EGLAttrib];
                let egl_display = egl.GetPlatformDisplay(
                    EGL_PLATFORM_DEVICE_EXT,
                    egl_device as *mut c_void,
                    attribs.as_ptr(),
                );
                if egl_display == egl::NO_DISPLAY {
                    return Err(Error::DeviceOpenFailed);
                }

                let (mut major_version, mut minor_version) = (0, 0);
                let result = egl.Initialize(egl_display, &mut major_version, &mut minor_version);
                if result == egl::FALSE {
                    return Err(Error::DeviceOpenFailed);
                }

                enable_d3d11_multithread_protection(&d3d11_device);

                Ok(Device {
                    egl_display,
                    d3d11_device,
                    d3d_driver_type,
                    display_is_owned: true,
                })
            })
        }
    }

    pub(crate) fn from_native_device(native_device: NativeDevice) -> Result<Device, Error> {
        unsafe {
            (*native_device.d3d11_device).AddRef();
            Ok(Device {
                egl_display: native_device.egl_display,
                d3d11_device: ComPtr::from_raw(native_device.d3d11_device),
                d3d_driver_type: native_device.d3d_driver_type,
                display_is_owned: false,
            })
        }
    }

    #[allow(non_snake_case)]
    pub(crate) fn from_egl_display(egl_display: EGLDisplay) -> Result<Device, Error> {
        let d3d11_device = query_d3d11_device_for_egl_display(egl_display)?;
        Ok(Device {
            egl_display,
            d3d11_device,
            d3d_driver_type: D3D_DRIVER_TYPE_UNKNOWN,
            display_is_owned: false,
        })
    }

    /// Returns the display server connection that this device was created with.
    #[inline]
    pub fn connection(&self) -> Connection {
        Connection
    }

    /// The adapter LUID (high, low) backing this device's D3D11 device. Diagnostic helper.
    pub(crate) fn d3d11_adapter_luid(&self) -> (i32, u32) {
        unsafe {
            let mut dxgi_device: *mut IDXGIDevice = ptr::null_mut();
            if !winerror::SUCCEEDED((*self.d3d11_device).QueryInterface(
                &IDXGIDevice::uuidof(),
                &mut dxgi_device as *mut *mut IDXGIDevice as *mut *mut c_void,
            )) {
                return (0, 0);
            }
            let dxgi_device = ComPtr::from_raw(dxgi_device);
            let mut adapter: *mut IDXGIAdapter = ptr::null_mut();
            if !winerror::SUCCEEDED((*dxgi_device).GetAdapter(&mut adapter)) {
                return (0, 0);
            }
            let adapter = ComPtr::from_raw(adapter);
            let mut desc = mem::zeroed();
            if !winerror::SUCCEEDED((*adapter).GetDesc(&mut desc)) {
                return (0, 0);
            }
            (desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart)
        }
    }

    /// Returns the adapter that this device was created with.
    pub fn adapter(&self) -> Adapter {
        unsafe {
            let mut dxgi_device: *mut IDXGIDevice = ptr::null_mut();
            let result = (*self.d3d11_device).QueryInterface(
                &IDXGIDevice::uuidof(),
                &mut dxgi_device as *mut *mut IDXGIDevice as *mut *mut c_void,
            );
            assert!(winerror::SUCCEEDED(result));
            let dxgi_device = ComPtr::from_raw(dxgi_device);

            let mut dxgi_adapter = ptr::null_mut();
            let result = (*dxgi_device).GetAdapter(&mut dxgi_adapter);
            assert!(winerror::SUCCEEDED(result));
            let dxgi_adapter = ComPtr::from_raw(dxgi_adapter);

            Adapter {
                dxgi_adapter,
                d3d_driver_type: self.d3d_driver_type,
            }
        }
    }

    /// Returns the underlying native device type.
    ///
    /// The reference count on the underlying Direct3D device is increased before returning it.
    #[inline]
    pub fn native_device(&self) -> NativeDevice {
        NativeDevice {
            egl_display: self.egl_display,
            d3d11_device: self.d3d11_device.clone().into_raw(),
            d3d_driver_type: self.d3d_driver_type,
        }
    }

    /// Returns the OpenGL API flavor that this device supports (OpenGL or OpenGL ES).
    #[inline]
    pub fn gl_api(&self) -> GLApi {
        GLApi::GLES
    }
}

unsafe fn set_multithread_protected(unknown: *mut IUnknown, target: &str) {
    let mut multithread: *mut ID3D11Multithread = ptr::null_mut();
    let result = (*unknown).QueryInterface(
        &IID_ID3D11_MULTITHREAD,
        &mut multithread as *mut *mut ID3D11Multithread as *mut *mut c_void,
    );
    if !winerror::SUCCEEDED(result) || multithread.is_null() {
        warn!(
            "ANGLE D3D11 {target} does not expose ID3D11Multithread; \
             multi-thread protection unavailable"
        );
        return;
    }

    let was_enabled = ((*(*multithread).lpVtbl).SetMultithreadProtected)(multithread, TRUE);
    ((*(*multithread).lpVtbl).parent.Release)(multithread as *mut IUnknown);
    info!(
        "Enabled ANGLE D3D11 {target} multithread protection; previously_enabled={}",
        was_enabled != 0
    );
}

fn enable_d3d11_multithread_protection(d3d11_device: &ComPtr<ID3D11Device>) {
    unsafe {
        set_multithread_protected(d3d11_device.as_raw() as *mut IUnknown, "device");

        let mut immediate_context: *mut ID3D11DeviceContext = ptr::null_mut();
        d3d11_device.GetImmediateContext(&mut immediate_context);
        if immediate_context.is_null() {
            warn!("ANGLE D3D11 device returned a null immediate context");
            return;
        }
        set_multithread_protected(immediate_context as *mut IUnknown, "immediate context");
        ((*(*(immediate_context as *mut IUnknown)).lpVtbl).Release)(
            immediate_context as *mut IUnknown,
        );
    }
}

fn query_d3d11_device_for_egl_display(
    egl_display: EGLDisplay,
) -> Result<ComPtr<ID3D11Device>, Error> {
    unsafe {
        let eglQueryDisplayAttribEXT = EGL_EXTENSION_FUNCTIONS
            .QueryDisplayAttribEXT
            .expect("Where's the `EGL_EXT_device_query` extension?");
        let eglQueryDeviceAttribEXT = EGL_EXTENSION_FUNCTIONS
            .QueryDeviceAttribEXT
            .expect("Where's the `EGL_EXT_device_query` extension?");
        let mut angle_device: EGLAttrib = 0;
        let result = eglQueryDisplayAttribEXT(
            egl_display,
            EGL_DEVICE_EXT as EGLint,
            &mut angle_device as *mut EGLAttrib,
        );
        if result == egl::FALSE {
            return Err(Error::DeviceOpenFailed);
        }
        let mut device: EGLAttrib = 0;
        let result = eglQueryDeviceAttribEXT(
            angle_device as EGLDeviceEXT,
            EGL_D3D11_DEVICE_ANGLE as EGLint,
            &mut device as *mut EGLAttrib,
        );
        if result == egl::FALSE {
            return Err(Error::DeviceOpenFailed);
        }
        let d3d11_device = device as *mut ID3D11Device;

        (*d3d11_device).AddRef();
        Ok(ComPtr::from_raw(d3d11_device))
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            if self.display_is_owned {
                EGL_FUNCTIONS.with(|egl| {
                    let result = egl.Terminate(self.egl_display);
                    assert_ne!(result, egl::FALSE);
                    self.egl_display = egl::NO_DISPLAY;
                })
            }
        }
    }
}

#[cfg(test)]
mod present_path_tests {
    use super::*;

    // Verify the helper places the present-path-fast pair right before the NONE
    // terminator, and that the LUID/WARP branching is correct. ANGLE attribute
    // arrays MUST be NONE-terminated (otherwise ANGLE reads past the end of the
    // array), and a value typo (COPY instead of FAST) would be a silent regression,
    // so the value itself is checked too.
    #[test]
    fn luid_branch_has_present_path_fast_and_luid() {
        let a = luid_display_attribs(D3D_DRIVER_TYPE_UNKNOWN, 0x1234, 0x5678);
        // NONE terminated
        assert_eq!(*a.last().unwrap(), egl::NONE as EGLAttrib);
        // present-path-fast pair (key followed by FAST value)
        let key = EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE as EGLAttrib;
        let pos = a.iter().position(|&x| x == key).expect("present-path key missing");
        assert_eq!(a[pos + 1], EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE as EGLAttrib);
        // LUID high/low present
        let hk = EGL_PLATFORM_ANGLE_D3D_LUID_HIGH_ANGLE as EGLAttrib;
        let hp = a.iter().position(|&x| x == hk).expect("LUID high key missing");
        assert_eq!(a[hp + 1], 0x1234 as EGLAttrib);
        // WARP device-type attribute must not appear on the LUID branch
        assert!(!a.contains(&(EGL_PLATFORM_ANGLE_DEVICE_TYPE_D3D_WARP_ANGLE as EGLAttrib)));
    }

    #[test]
    fn warp_branch_has_present_path_fast_and_warp() {
        let a = luid_display_attribs(D3D_DRIVER_TYPE_WARP, 0, 0);
        assert_eq!(*a.last().unwrap(), egl::NONE as EGLAttrib);
        let key = EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE as EGLAttrib;
        let pos = a.iter().position(|&x| x == key).expect("present-path key missing");
        assert_eq!(a[pos + 1], EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE as EGLAttrib);
        // WARP branch: device-type WARP present, LUID high absent
        assert!(a.contains(&(EGL_PLATFORM_ANGLE_DEVICE_TYPE_D3D_WARP_ANGLE as EGLAttrib)));
        assert!(!a.contains(&(EGL_PLATFORM_ANGLE_D3D_LUID_HIGH_ANGLE as EGLAttrib)));
    }
}

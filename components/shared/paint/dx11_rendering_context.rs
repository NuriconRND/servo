/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A [`RenderingContext`] that drives WebRender through the native D3D11 backend
//! (`wr-d3d11`) instead of surfman/ANGLE.
//!
//! This wraps the exact device + swapchain + RTV/DSV + present logic from the
//! `wr-d3d11-sample` `D3d11Backend`: it creates a hardware D3D11 device
//! ([`wr_d3d11::context::D3d11Context`]), a `FLIP_DISCARD` swapchain for an HWND, wraps
//! the backbuffer in an RTV and an owned D24S8 depth texture in a DSV, and registers both
//! as WebRender's default framebuffer (FBO 0) via [`wr_d3d11::Gld3d11::set_default_target`].
//! [`RenderingContext::gleam_gl_api`] then returns the [`wr_d3d11::Gld3d11`] so WebRender
//! renders natively on D3D11.
//!
//! v1 scope: single tile / single GPU, no offscreen readback (`read_to_image` -> `None`),
//! no `glow` (the `glow` API is only used inside surfman's own context, never here).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use dpi::PhysicalSize;
use image::RgbaImage;
use log::error;
use webrender_api::units::DeviceIntRect;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;
use wr_d3d11::{Gld3d11, context::D3d11Context};

use crate::rendering_context::{Error, RenderingContext};

/// A native D3D11 [`RenderingContext`]. See the module docs.
///
/// `present`/`resize` take `&self` (per the trait), so the mutable D3D state — the owned
/// depth texture and the current size — lives behind `RefCell`/`Cell`. The swapchain
/// itself needs no interior mutability: `Present` and `ResizeBuffers` are `&self` COM
/// methods, and `ResizeBuffers` is only ever called from `resize`.
pub struct Dx11RenderingContext {
    gl: Rc<Gld3d11>,
    swapchain: IDXGISwapChain1,
    /// The DSV's backing depth texture. The swapchain only manages the color backbuffer,
    /// so we own the D24S8 texture and recreate it on every `resize`.
    depth: RefCell<Option<ID3D11Texture2D>>,
    size: Cell<PhysicalSize<u32>>,
}

impl Dx11RenderingContext {
    /// Create a native D3D11 rendering context for a window.
    ///
    /// `hwnd` is the raw Win32 window handle (as an `isize`, e.g. from
    /// `RawWindowHandle::Win32`), so callers don't need to depend on the `windows` crate.
    pub fn new(hwnd: isize, size: PhysicalSize<u32>) -> Result<Self, String> {
        let width = size.width.max(1);
        let height = size.height.max(1);

        let ctx = D3d11Context::new_hardware()
            .map_err(|error| format!("D3D11 hardware device creation failed: {error:?}"))?;
        let dxgi_device: IDXGIDevice = ctx
            .device
            .cast()
            .map_err(|error| format!("ID3D11Device -> IDXGIDevice cast failed: {error:?}"))?;
        let adapter = unsafe { dxgi_device.GetAdapter() }
            .map_err(|error| format!("IDXGIAdapter query failed: {error:?}"))?;
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }
            .map_err(|error| format!("IDXGIFactory2 query failed: {error:?}"))?;

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            ..Default::default()
        };
        let hwnd = HWND(hwnd as *mut _);
        let swapchain =
            unsafe { factory.CreateSwapChainForHwnd(&ctx.device, hwnd, &desc, None, None) }
                .map_err(|error| format!("CreateSwapChainForHwnd failed: {error:?}"))?;

        let gl = Gld3d11::new(ctx);
        let context = Dx11RenderingContext {
            gl,
            swapchain,
            depth: RefCell::new(None),
            size: Cell::new(PhysicalSize::new(width, height)),
        };
        context.configure_target(width, height);
        Ok(context)
    }

    /// Wrap swapchain buffer 0 in an RTV, build a fresh D24S8 depth texture + DSV, and
    /// register them as WebRender's default target. Shared by `new` and `resize`.
    fn configure_target(&self, width: u32, height: u32) {
        let device = self.gl.device();

        let backbuffer: ID3D11Texture2D =
            unsafe { self.swapchain.GetBuffer(0) }.expect("swapchain backbuffer(0) query failed");
        let mut rtv = None;
        unsafe { device.CreateRenderTargetView(&backbuffer, None, Some(&mut rtv)) }
            .expect("backbuffer RTV creation failed");

        let depth_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_D24_UNORM_S8_UINT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_DEPTH_STENCIL.0 as u32,
            ..Default::default()
        };
        let mut depth = None;
        unsafe { device.CreateTexture2D(&depth_desc, None, Some(&mut depth)) }
            .expect("depth texture creation failed");
        let depth = depth.unwrap();
        let mut dsv = None;
        unsafe { device.CreateDepthStencilView(&depth, None, Some(&mut dsv)) }
            .expect("DSV creation failed");

        self.gl.set_default_target(rtv.unwrap(), dsv, width, height);
        *self.depth.borrow_mut() = Some(depth);
    }
}

impl RenderingContext for Dx11RenderingContext {
    fn read_to_image(&self, _source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        // v1 stub: offscreen readback / screenshots are not needed for the wall demo.
        None
    }

    fn size(&self) -> PhysicalSize<u32> {
        self.size.get()
    }

    fn resize(&self, size: PhysicalSize<u32>) {
        let width = size.width.max(1);
        let height = size.height.max(1);
        let new_size = PhysicalSize::new(width, height);
        if self.size.get() == new_size {
            return;
        }

        // All views referencing the backbuffer (RTV) must be released before ResizeBuffers.
        self.gl.release_default_target();
        *self.depth.borrow_mut() = None;
        unsafe {
            self.swapchain.ResizeBuffers(
                2,
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        }
        .expect("ResizeBuffers failed");
        self.configure_target(width, height);
        self.size.set(new_size);
    }

    fn present(&self) {
        let hr = unsafe { self.swapchain.Present(1, DXGI_PRESENT(0)) };
        if hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET {
            // Per spec: device removed/reset is not recovered — leave diagnostics and abort.
            let reason = unsafe { self.gl.device().GetDeviceRemovedReason() };
            panic!(
                "D3D11 device removed/reset: Present hr={hr:?}, GetDeviceRemovedReason={reason:?}"
            );
        } else if hr.is_err() {
            error!("D3D11 Present failed: {hr:?}");
        }
    }

    fn make_current(&self) -> Result<(), Error> {
        // The D3D11 immediate context is always current on its owning thread; nothing to do.
        Ok(())
    }

    fn gleam_gl_api(&self) -> Rc<dyn gleam::gl::Gl> {
        self.gl.clone()
    }

    fn glow_gl_api(&self) -> Arc<glow::Context> {
        // `glow` is only consumed inside surfman's own context (e.g. the offscreen blit
        // path); the native D3D11 backend never routes through it.
        unimplemented!("glow not supported by the wr-d3d11 D3D11 rendering context")
    }
}

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
//! v1 scope: single tile / single GPU, no `glow` (the `glow` API is only used inside surfman's
//! own context, never here). `read_to_image` reads the backbuffer back via a STAGING texture for
//! render validation.

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
    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        // Read the swapchain backbuffer back to CPU for render validation (e.g. winit_wall's
        // `--capture`). Called after WebRender has drawn into FBO 0 but *before* `present()`
        // (FLIP_DISCARD discards the backbuffer on Present), matching the GL backend's
        // read-then-present ordering. We copy the backbuffer into a STAGING texture, Map it, and
        // convert BGRA8 -> RGBA8. The backbuffer is stored top-down (row 0 = top of screen), the
        // same orientation the GL backend produces after its flip, so gl/d3d11 captures compare
        // directly.
        let device = self.gl.device();
        let context = unsafe { device.GetImmediateContext() }.ok()?;

        let backbuffer: ID3D11Texture2D = unsafe { self.swapchain.GetBuffer(0) }.ok()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { backbuffer.GetDesc(&mut desc) };
        let full_w = desc.Width;
        let full_h = desc.Height;

        // Clamp the requested rect to the backbuffer bounds.
        let x0 = source_rectangle.min.x.max(0) as u32;
        let y0 = source_rectangle.min.y.max(0) as u32;
        if x0 >= full_w || y0 >= full_h {
            return None;
        }
        let req_w = source_rectangle.width().max(0) as u32;
        let req_h = source_rectangle.height().max(0) as u32;
        let out_w = if req_w == 0 { full_w - x0 } else { req_w.min(full_w - x0) };
        let out_h = if req_h == 0 { full_h - y0 } else { req_h.min(full_h - y0) };
        if out_w == 0 || out_h == 0 {
            return None;
        }

        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut staging = None;
        unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }.ok()?;
        let staging = staging?;
        unsafe { context.CopyResource(&staging, &backbuffer) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.ok()?;

        let row_pitch = mapped.RowPitch as usize;
        let src = mapped.pData as *const u8;
        let mut out = RgbaImage::new(out_w, out_h);
        for row in 0..out_h {
            let src_row = unsafe { src.add((y0 + row) as usize * row_pitch) };
            for col in 0..out_w {
                // Backbuffer is DXGI_FORMAT_B8G8R8A8_UNORM: bytes are B, G, R, A.
                let px = unsafe { src_row.add((x0 + col) as usize * 4) };
                let b = unsafe { *px };
                let g = unsafe { *px.add(1) };
                let r = unsafe { *px.add(2) };
                // Force opaque: WebRender may leave alpha at 0 in the backbuffer.
                out.put_pixel(col, row, image::Rgba([r, g, b, 255]));
            }
        }
        unsafe { context.Unmap(&staging, 0) };
        Some(out)
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

    fn surface_origin_is_top_left(&self) -> bool {
        // The swapchain backbuffer is a top-down D3D texture (row 0 = top), unlike a GL window
        // surface (bottom-left origin). WebRender must project FBO 0 accordingly, or the whole
        // frame is presented upside down. `wr-d3d11` keeps the render target in GL physical
        // layout internally, so this flag (passed to `WebRenderOptions`) is what makes the final
        // present come out right way up — matching the `wr-d3d11-sample` D3D11 backend.
        true
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

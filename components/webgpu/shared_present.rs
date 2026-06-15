/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! GPU-direct present support for the multi-GPU wall (Phase 3, M1).
//!
//! The default WebGPU present path copies the canvas texture to a staging buffer and maps
//! it to CPU memory, so every tile's compositor uploads the same CPU image. On a multi-GPU
//! wall that funnels all displayed pixels through one GPU's readback.
//!
//! Instead, this module creates a *shared* D3D12 texture per GPU (DXGI shareable, exported
//! as an NT handle). Each tile's compositor (a surfman/ANGLE D3D11 device on the same GPU)
//! can `OpenSharedResource1` the handle and sample it directly as a GL texture — no CPU
//! round-trip, each tile displaying its own GPU's render (the WebGL fan-out model).
//!
//! This is the wgpu-side half (create the shared texture + export the handle + wrap it as a
//! wgpu texture so we can GPU-copy the canvas into it). The compositor-side import lives in
//! the paint crate.

#![cfg(windows)]
// M1 scaffolding: created and exported here; wired into the present path in a later milestone.
#![allow(dead_code)]

use webrender_api::ImageFormat;
use wgpu_core::global::Global;
use wgpu_core::id::{CommandBufferId, CommandEncoderId, DeviceId, QueueId, TextureId};
use wgpu_types as wgt;
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_CPU_PAGE_PROPERTY_UNKNOWN, D3D12_HEAP_FLAG_SHARED, D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_TYPE_DEFAULT, D3D12_MEMORY_POOL_UNKNOWN, D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_DIMENSION_TEXTURE2D, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
    D3D12_RESOURCE_STATE_COPY_DEST, D3D12_TEXTURE_LAYOUT_UNKNOWN, ID3D12Device, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::core::PCWSTR;

/// Map the canvas present format to the matching (D3D12, wgpu) texture formats for the shared
/// texture. Returns `None` for formats GPU-direct doesn't handle (caller falls back to CPU).
fn formats_for(image_format: ImageFormat) -> Option<(DXGI_FORMAT, wgt::TextureFormat)> {
    match image_format {
        ImageFormat::BGRA8 => Some((DXGI_FORMAT_B8G8R8A8_UNORM, wgt::TextureFormat::Bgra8Unorm)),
        ImageFormat::RGBA8 => Some((DXGI_FORMAT_R8G8B8A8_UNORM, wgt::TextureFormat::Rgba8Unorm)),
        _ => None,
    }
}

/// A shared D3D12 present texture: registered as a wgpu texture (so the WebGPU thread can
/// GPU-copy the canvas into it) and exported as an NT handle (so the compositor can import
/// it on its own D3D11 device).
pub(crate) struct SharedPresentTexture {
    /// wgpu texture id of the shared texture (minted from a reserved id range).
    pub texture_id: TextureId,
    /// DXGI shared NT handle, valid for `OpenSharedResource1` on another device.
    pub shared_handle: HANDLE,
    pub width: u32,
    pub height: u32,
}

// The exported NT handle is just an opaque value we hand to the compositor; it is safe to
// move between threads.
unsafe impl Send for SharedPresentTexture {}

impl Drop for SharedPresentTexture {
    fn drop(&mut self) {
        // Close the exported NT handle. The compositor's `OpenSharedResource`'d D3D11 texture
        // holds its own reference to the underlying resource, so closing the handle here is
        // safe. (The wgpu texture id is dropped separately by the owner.)
        let _ = unsafe { CloseHandle(self.shared_handle) };
    }
}

/// Create a DXGI-shareable D3D12 texture on `device_id`'s GPU within `global`, export an NT
/// handle for cross-device import, and register it in wgpu-core under `texture_id`.
///
/// `texture_id` must be minted from a reserved id range (e.g. via `TextureId::zip`) that the
/// script-side identity hub never allocates, so it cannot collide with page resources.
///
/// Returns `None` on any failure; callers fall back to the CPU-readback present path.
pub(crate) fn create_shared_present_texture(
    global: &Global,
    device_id: DeviceId,
    texture_id: TextureId,
    width: u32,
    height: u32,
    image_format: ImageFormat,
) -> Option<SharedPresentTexture> {
    let (dxgi_format, wgt_format) = formats_for(image_format)?;
    // SAFETY: we only read the raw device handle; it is owned by wgpu for the call's duration.
    let hal_device = unsafe { global.device_as_hal::<wgpu_core::api::Dx12>(device_id) }?;
    let raw_device: &ID3D12Device = hal_device.raw_device();

    let heap_props = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let resource_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: width as u64,
        Height: height,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: dxgi_format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        // ALLOW_RENDER_TARGET maps to D3D11 BIND_RENDER_TARGET|BIND_SHADER_RESOURCE when the
        // handle is opened, which is what ANGLE needs to wrap it as an EGL/GL texture.
        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
    };

    let mut resource: Option<ID3D12Resource> = None;
    // SAFETY: descriptors are valid for the call; resource is an out-param.
    unsafe {
        raw_device.CreateCommittedResource(
            &heap_props,
            D3D12_HEAP_FLAG_SHARED,
            &resource_desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut resource,
        )
    }
    .ok()?;
    let resource = resource?;

    // SAFETY: `resource` is a valid shared resource; null name/attributes are allowed.
    let shared_handle =
        unsafe { raw_device.CreateSharedHandle(&resource, None, GENERIC_ALL.0, PCWSTR::null()) }
            .ok()?;

    let extent = wgt::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    // SAFETY: the resource matches the descriptor below; wgpu takes ownership of the resource.
    let hal_texture = unsafe {
        wgpu_hal::dx12::Device::texture_from_raw(
            resource,
            wgt_format,
            wgt::TextureDimension::D2,
            extent,
            1,
            1,
        )
    };
    let desc = wgt::TextureDescriptor {
        label: Some(std::borrow::Cow::Borrowed("wall-gpu-direct-present")),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format: wgt_format,
        usage: wgt::TextureUsages::COPY_DST |
            wgt::TextureUsages::RENDER_ATTACHMENT |
            wgt::TextureUsages::TEXTURE_BINDING,
        view_formats: vec![],
    };
    // SAFETY: `hal_texture` was created from this global's device.
    let (id, error) = unsafe {
        global.create_texture_from_hal(Box::new(hal_texture), device_id, &desc, Some(texture_id))
    };
    if let Some(error) = error {
        log::warn!("GPU-direct: create_texture_from_hal failed: {error:?}");
        return None;
    }

    Some(SharedPresentTexture {
        texture_id: id,
        shared_handle,
        width,
        height,
    })
}

// Reserved wgpu id index range for GPU-direct present resources.
//
// IMPORTANT: wgpu-core's per-resource-type registry is a `Vec` indexed by the id INDEX, so
// `prepare(Some(id))` grows that Vec to `index + 1`. The index must therefore stay modest —
// a huge index (e.g. 0xF000_0000) allocates a ~90 GB Vec and aborts. These bases sit above
// the script identity hub's realistic live range (it allocates from 0 and reuses freed
// slots) while keeping the registry Vec small (~256k entries ≈ a few MB). We always pass
// `Some(id)`, so no internal/external allocation mixing occurs.
const GPU_DIRECT_TEXTURE_INDEX_BASE: u32 = 1 << 18; // 262144
// Per-frame copy command ids reuse FIXED indices with the frame number as the epoch (and are
// dropped after submit), so the registry slot is recycled instead of growing every frame.
const GPU_DIRECT_CMD_ENCODER_INDEX: u32 = (1 << 18) + 1;
const GPU_DIRECT_CMD_BUFFER_INDEX: u32 = (1 << 18) + 2;

/// Hands out distinct small registry slots for shared present textures (one per created
/// texture), so each gets its own slot without per-frame growth.
static NEXT_TEXTURE_SLOT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Mint a fresh shared present texture id from the reserved range.
pub(crate) fn next_shared_texture_id() -> TextureId {
    let slot = NEXT_TEXTURE_SLOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    TextureId::zip(GPU_DIRECT_TEXTURE_INDEX_BASE.wrapping_add(slot), 1)
}

/// Copy the page's canvas texture into the shared present texture on `global` and submit it
/// to the device's queue. Uses freshly-minted command encoder/buffer ids each frame (from the
/// reserved range) to avoid id-lifecycle reuse, dropping them after submit.
///
/// This is additive to the existing CPU-readback present: the shared texture now holds an
/// up-to-date GPU copy that the compositor can sample directly (wired in a later milestone).
pub(crate) fn copy_canvas_to_shared(
    global: &Global,
    device_id: DeviceId,
    queue_id: QueueId,
    canvas_texture_id: TextureId,
    shared_texture_id: TextureId,
    size: wgt::Extent3d,
    frame: u32,
) {
    // Fixed indices, frame number as the (nonzero, monotonic) epoch: the slot is recycled
    // each frame (created → submitted → dropped), so the registry Vec never grows per frame.
    let epoch = frame.wrapping_add(1);
    let enc_id = CommandEncoderId::zip(GPU_DIRECT_CMD_ENCODER_INDEX, epoch);
    let buf_id = CommandBufferId::zip(GPU_DIRECT_CMD_BUFFER_INDEX, epoch);

    let (_, error) = global.device_create_command_encoder(
        device_id,
        &wgt::CommandEncoderDescriptor { label: None },
        Some(enc_id),
    );
    if error.is_some() {
        return;
    }

    let source = wgt::TexelCopyTextureInfo {
        texture: canvas_texture_id,
        mip_level: 0,
        origin: wgt::Origin3d::ZERO,
        aspect: wgt::TextureAspect::All,
    };
    let destination = wgt::TexelCopyTextureInfo {
        texture: shared_texture_id,
        mip_level: 0,
        origin: wgt::Origin3d::ZERO,
        aspect: wgt::TextureAspect::All,
    };
    if global
        .command_encoder_copy_texture_to_texture(enc_id, &source, &destination, &size)
        .is_err()
    {
        global.command_encoder_drop(enc_id);
        return;
    }

    let (_, error) = global.command_encoder_finish(
        enc_id,
        &wgt::CommandBufferDescriptor::default(),
        Some(buf_id),
    );
    if error.is_some() {
        global.command_encoder_drop(enc_id);
        return;
    }

    let _ = global.queue_submit(queue_id, &[buf_id]);
    global.command_buffer_drop(buf_id);
    global.command_encoder_drop(enc_id);
}

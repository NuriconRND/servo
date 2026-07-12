/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Plane ring producer: DYNAMIC plane texture creation + CPU row copy.
//!
//! The producer creates one DYNAMIC (`CPU_WRITE` + `SHADER_RESOURCE`) texture
//! per plane per slot on the consumer (ANGLE) device — the only D3D call the
//! producer makes — and registers them with [`servo_media_player::d3d11_ring`].
//! Every frame it then `memcpy`s decoded planes into the renderer-mapped slot
//! pointers. It never touches the D3D immediate context (Map/Unmap live on the
//! renderer side, Task 6).
//!
//! Plane layout follows the I420 display contract (plane 0 = Y, 1 = U, 2 = V;
//! NV12 plane 1 = interleaved UV). YV12 sources (gst plane order Y,V,U) are
//! reconciled to this contract by [`src_plane_index`] at copy time — the ring
//! textures are always built and indexed in Y,U,V order.

use log::warn;
use servo_media_player::d3d11_ring::{ClaimedSlot, MAX_PLANES, PlaneDesc, RingPlaneFormat, SLOT_COUNT};
use servo_media_player::video::VideoFrameYuvFormat;
use winapi::shared::dxgiformat::{DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM};
use winapi::shared::dxgitype::DXGI_SAMPLE_DESC;
use winapi::shared::winerror::S_OK;
use winapi::um::d3d11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_WRITE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DYNAMIC,
    ID3D11Device, ID3D11Texture2D,
};
use wio::com::ComPtr;

/// Per-plane geometry for the DYNAMIC ring textures, in I420 display order.
pub struct PlaneGeom {
    pub format: RingPlaneFormat,
    pub width: i32,
    pub height: i32,
    /// Valid bytes per row = texel width × bytes-per-texel.
    pub row_bytes: usize,
}

/// Plane geometry for `format` at `width`×`height` (display/I420 contract).
/// Chroma planes use ceil(w/2)×ceil(h/2); NV12's single UV plane is Rg8.
pub fn plane_geoms(format: VideoFrameYuvFormat, width: i32, height: i32) -> Vec<PlaneGeom> {
    let w = width.max(0);
    let h = height.max(0);
    let cw = (w + 1) / 2; // ceil(w/2)
    let ch = (h + 1) / 2; // ceil(h/2)
    match format {
        VideoFrameYuvFormat::I420 => vec![
            PlaneGeom { format: RingPlaneFormat::R8, width: w, height: h, row_bytes: w as usize },
            PlaneGeom { format: RingPlaneFormat::R8, width: cw, height: ch, row_bytes: cw as usize },
            PlaneGeom { format: RingPlaneFormat::R8, width: cw, height: ch, row_bytes: cw as usize },
        ],
        VideoFrameYuvFormat::NV12 => vec![
            PlaneGeom { format: RingPlaneFormat::R8, width: w, height: h, row_bytes: w as usize },
            PlaneGeom {
                format: RingPlaneFormat::Rg8,
                width: cw,
                height: ch,
                row_bytes: cw as usize * 2,
            },
        ],
    }
}

/// Maps a display plane index (0=Y, 1=U, 2=V) to the source gst plane index.
/// YV12's gst plane order is Y,V,U, so U/V are swapped relative to I420.
pub fn src_plane_index(display_plane: usize, swap_uv: bool) -> usize {
    match (display_plane, swap_uv) {
        (1, true) => 2,
        (2, true) => 1,
        (i, _) => i,
    }
}

/// Copy `rows` rows of `row_bytes` valid bytes each, honoring the (possibly
/// different) source stride and destination pitch. Never reads source padding
/// nor writes destination padding; bounds are clamped defensively.
pub fn copy_rows(
    src: &[u8],
    src_stride: usize,
    dst: &mut [u8],
    dst_pitch: usize,
    row_bytes: usize,
    rows: usize,
) {
    let n = row_bytes.min(src_stride).min(dst_pitch);
    if n == 0 {
        return;
    }
    for row in 0..rows {
        let s = row * src_stride;
        let d = row * dst_pitch;
        if s + n > src.len() || d + n > dst.len() {
            break;
        }
        dst[d..d + n].copy_from_slice(&src[s..s + n]);
    }
}

/// Create `SLOT_COUNT × planes` DYNAMIC plane textures on `device` (the AddRef'd
/// consumer ID3D11Device, passed as `usize`) and return their static descriptors
/// for [`servo_media_player::d3d11_ring::D3d11PlaneRings::create_ring`].
///
/// Each texture is created with refcount 1 and is NEVER released here on success
/// — on the success path every texture is `into_raw()`-leaked so the renderer
/// owns the sole reference and releases it via `take_removed_rings`. If a later
/// texture fails, the `ComPtr`s created so far in this batch drop and release
/// their textures before returning `None` (the renderer must never observe a
/// partially-created ring).
pub fn create_plane_textures(
    device: usize,
    format: VideoFrameYuvFormat,
    width: i32,
    height: i32,
) -> Option<[[Option<PlaneDesc>; MAX_PLANES]; SLOT_COUNT]> {
    let device = device as *mut ID3D11Device;
    if device.is_null() {
        return None;
    }
    let geoms = plane_geoms(format, width, height);
    let mut slots: [[Option<PlaneDesc>; MAX_PLANES]; SLOT_COUNT] = [[None; MAX_PLANES]; SLOT_COUNT];
    // Hold owning ComPtrs until the whole batch succeeds — on early return they
    // drop and Release (RAII cleanup of the partial batch).
    let mut created: Vec<ComPtr<ID3D11Texture2D>> = Vec::with_capacity(SLOT_COUNT * geoms.len());

    for slot in slots.iter_mut() {
        for (p, g) in geoms.iter().enumerate() {
            // `None` returns immediately; `created` drops and Releases the batch.
            let texture = create_dynamic_texture(device, g)?;
            slot[p] = Some(PlaneDesc {
                texture: texture.as_raw() as usize,
                width: g.width,
                height: g.height,
                format: g.format,
                row_bytes: g.row_bytes,
            });
            created.push(texture);
        }
    }
    // Success: leak every reference (keep refcount 1) — the renderer owns them now.
    for texture in created {
        let _ = texture.into_raw();
    }
    Some(slots)
}

/// Create one DYNAMIC (CPU_WRITE + SHADER_RESOURCE) texture for a plane.
/// Returns an owning `ComPtr` (refcount 1); the caller decides whether to leak
/// it (success) or let it drop-Release (failure elsewhere in the batch).
fn create_dynamic_texture(
    device: *mut ID3D11Device,
    g: &PlaneGeom,
) -> Option<ComPtr<ID3D11Texture2D>> {
    let dxgi_format = match g.format {
        RingPlaneFormat::R8 => DXGI_FORMAT_R8_UNORM,
        RingPlaneFormat::Rg8 => DXGI_FORMAT_R8G8_UNORM,
    };
    let desc = D3D11_TEXTURE2D_DESC {
        Width: g.width.max(1) as u32,
        Height: g.height.max(1) as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: dxgi_format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE,
        MiscFlags: 0,
    };
    unsafe {
        let mut texture = std::ptr::null_mut();
        let hr = (*device).CreateTexture2D(&desc, std::ptr::null(), &mut texture);
        if hr != S_OK || texture.is_null() {
            warn!(
                "D3D11 video: DYNAMIC plane 텍스처 생성 실패 hr={hr:#x} ({}x{} dxgi={dxgi_format})",
                g.width, g.height
            );
            return None;
        }
        Some(ComPtr::from_raw(texture))
    }
}

/// Copy each decoded plane of `frame` into the renderer-mapped pointers of a
/// claimed slot, row by row (honoring gst source stride and mapped RowPitch),
/// with YV12 U/V reconciled to the I420 display order via [`src_plane_index`].
pub fn copy_planes(
    frame: &gstreamer_video::VideoFrameRef<&gstreamer::BufferRef>,
    format: VideoFrameYuvFormat,
    swap_uv: bool,
    claimed: &ClaimedSlot,
) {
    use gstreamer_video::VideoFrameExt;

    for p in 0..format.plane_count().min(MAX_PLANES) {
        let Some(mapped) = claimed.planes[p] else {
            continue;
        };
        if mapped.data_ptr == 0 {
            // Renderer failed to (re)map this plane — skip rather than write to null.
            continue;
        }
        let src_plane = src_plane_index(p, swap_uv);
        let src = match frame.plane_data(src_plane as u32) {
            Ok(data) => data,
            Err(_) => {
                warn!("D3D11 video: plane_data({src_plane}) 실패 — plane 스킵");
                continue;
            },
        };
        let src_stride = frame.info().stride()[src_plane] as usize;
        let dst_pitch = mapped.row_pitch as usize;
        let dst_len = dst_pitch * mapped.rows;
        // SAFETY: data_ptr/row_pitch are the renderer's most recent D3D11 Map
        // result for this slot's plane texture; the mapped region spans
        // row_pitch × rows bytes. This slot is Writing (claimed by us) so the
        // renderer will not remap it concurrently.
        let dst = unsafe { std::slice::from_raw_parts_mut(mapped.data_ptr as *mut u8, dst_len) };
        copy_rows(src, src_stride, dst, dst_pitch, mapped.row_bytes, mapped.rows);
    }
}

/// Pack each decoded plane of `frame` into a tightly-packed `Vec<u8>` (rows at
/// exactly `row_bytes`, no padding) for
/// [`servo_media_player::d3d11_ring::D3d11PlaneRings::stage_first_frame`]. Used
/// only in the initial phase (all slots Unmapped) so the renderer's
/// InitialMapAll can show the first frame.
pub fn planes_to_vecs(
    frame: &gstreamer_video::VideoFrameRef<&gstreamer::BufferRef>,
    format: VideoFrameYuvFormat,
    swap_uv: bool,
) -> Vec<Vec<u8>> {
    use gstreamer_video::VideoFrameExt;

    let geoms = plane_geoms(format, frame.info().width() as i32, frame.info().height() as i32);
    let mut out = Vec::with_capacity(geoms.len());
    for (p, g) in geoms.iter().enumerate() {
        let rows = g.height.max(0) as usize;
        let mut packed = vec![0u8; g.row_bytes * rows];
        let src_plane = src_plane_index(p, swap_uv);
        if let Ok(src) = frame.plane_data(src_plane as u32) {
            let src_stride = frame.info().stride()[src_plane] as usize;
            // Destination is tightly packed: dst pitch == row_bytes.
            copy_rows(src, src_stride, &mut packed, g.row_bytes, g.row_bytes, rows);
        }
        out.push(packed);
    }
    out
}

#[cfg(test)]
mod tests {
    use servo_media_player::d3d11_ring::RingPlaneFormat;
    use servo_media_player::video::VideoFrameYuvFormat;

    // (1) Row copy honors differing src stride / dst pitch.
    #[test]
    fn copy_rows_honors_differing_stride_and_pitch() {
        let src_stride = 96usize;
        let dst_pitch = 128usize;
        let row_bytes = 64usize;
        let rows = 4usize;

        let mut src = vec![0xEEu8; src_stride * rows];
        for r in 0..rows {
            for b in 0..row_bytes {
                src[r * src_stride + b] = r as u8 + 1;
            }
        }
        let mut dst = vec![0u8; dst_pitch * rows];
        super::copy_rows(&src, src_stride, &mut dst, dst_pitch, row_bytes, rows);

        for r in 0..rows {
            for b in 0..row_bytes {
                assert_eq!(dst[r * dst_pitch + b], r as u8 + 1, "row {r} byte {b}");
            }
            for b in row_bytes..dst_pitch {
                assert_eq!(dst[r * dst_pitch + b], 0, "dst padding row {r} byte {b}");
            }
        }
    }

    // (2) YV12 plane swap: display plane 1 = U, 2 = V regardless of gst order.
    #[test]
    fn yv12_swaps_u_and_v_source_planes() {
        // I420 (no swap) — identity.
        assert_eq!(super::src_plane_index(0, false), 0);
        assert_eq!(super::src_plane_index(1, false), 1);
        assert_eq!(super::src_plane_index(2, false), 2);
        // YV12: gst plane order is Y,V,U — display plane 1 (U) reads gst 2,
        // display plane 2 (V) reads gst 1.
        assert_eq!(super::src_plane_index(0, true), 0);
        assert_eq!(super::src_plane_index(1, true), 2);
        assert_eq!(super::src_plane_index(2, true), 1);
    }

    // (3) Plane dims / row_bytes incl. odd sizes.
    #[test]
    fn plane_geoms_dims_and_row_bytes() {
        // I420 even 64x64.
        let g = super::plane_geoms(VideoFrameYuvFormat::I420, 64, 64);
        assert_eq!(g.len(), 3);
        assert_eq!(
            (g[0].width, g[0].height, g[0].row_bytes, g[0].format),
            (64, 64, 64, RingPlaneFormat::R8)
        );
        assert_eq!(
            (g[1].width, g[1].height, g[1].row_bytes, g[1].format),
            (32, 32, 32, RingPlaneFormat::R8)
        );
        assert_eq!(
            (g[2].width, g[2].height, g[2].row_bytes, g[2].format),
            (32, 32, 32, RingPlaneFormat::R8)
        );

        // I420 odd 5x3 -> chroma ceil = 3x2.
        let g = super::plane_geoms(VideoFrameYuvFormat::I420, 5, 3);
        assert_eq!((g[0].width, g[0].height, g[0].row_bytes), (5, 3, 5));
        assert_eq!((g[1].width, g[1].height, g[1].row_bytes), (3, 2, 3));
        assert_eq!((g[2].width, g[2].height, g[2].row_bytes), (3, 2, 3));

        // NV12 odd 5x3 -> UV Rg8 3x2 row_bytes = 6.
        let g = super::plane_geoms(VideoFrameYuvFormat::NV12, 5, 3);
        assert_eq!(g.len(), 2);
        assert_eq!(
            (g[0].width, g[0].height, g[0].row_bytes, g[0].format),
            (5, 3, 5, RingPlaneFormat::R8)
        );
        assert_eq!(
            (g[1].width, g[1].height, g[1].row_bytes, g[1].format),
            (3, 2, 6, RingPlaneFormat::Rg8)
        );

        // NV12 65x37 -> UV 33x19 row_bytes = 66.
        let g = super::plane_geoms(VideoFrameYuvFormat::NV12, 65, 37);
        assert_eq!((g[1].width, g[1].height, g[1].row_bytes), (33, 19, 66));
    }
}

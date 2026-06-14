/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::rc::Rc;

use euclid::default::Size2D;
use log::{debug, info, warn};
use paint_api::rendering_context::RenderingContext;
use paint_api::{ExternalImageSource, WebRenderExternalImageApi};
use rustc_hash::{FxHashMap, FxHashSet};
use servo_base::id::PainterId;
use servo_canvas_traits::webgl::{WebGLContextId, WebGLSurfaceId, WebGLThreads};
use surfman::chains::{SwapChainAPI, SwapChains, SwapChainsAPI};
use surfman::{Device, SurfaceTexture};
use webgl::webgl_thread::WebGLContextBusyMap;

/// Bridge between the webrender::ExternalImage callbacks and the WebGLThreads.
pub struct WebGLExternalImages {
    painter_id: PainterId,
    webgl_threads: WebGLThreads,
    rendering_context: Rc<dyn RenderingContext>,
    swap_chains: SwapChains<WebGLSurfaceId, Device>,
    busy_webgl_context_map: WebGLContextBusyMap,
    locked_front_buffers: FxHashMap<WebGLSurfaceId, Vec<SurfaceTexture>>,
    logged_locked_surfaces: FxHashSet<WebGLSurfaceId>,
}

impl WebGLExternalImages {
    pub fn new(
        painter_id: PainterId,
        webgl_threads: WebGLThreads,
        rendering_context: Rc<dyn RenderingContext>,
        swap_chains: SwapChains<WebGLSurfaceId, Device>,
        busy_webgl_context_map: WebGLContextBusyMap,
    ) -> Self {
        Self {
            painter_id,
            webgl_threads,
            rendering_context,
            swap_chains,
            busy_webgl_context_map,
            locked_front_buffers: FxHashMap::default(),
            logged_locked_surfaces: FxHashSet::default(),
        }
    }

    fn lock_swap_chain(&mut self, id: WebGLContextId) -> Option<(u32, Size2D<i32>)> {
        let surface_id = WebGLSurfaceId::new(id, self.painter_id);
        debug!("... locking chain {:?} for surface {:?}", id, surface_id);

        {
            let mut busy_webgl_context_map = self.busy_webgl_context_map.write();
            *busy_webgl_context_map.entry(surface_id).or_default() += 1;
        }

        let Some(swap_chain) = self.swap_chains.get(surface_id) else {
            warn!("WebGL external image lock failed: missing swap chain for {surface_id:?}");
            self.mark_surface_not_busy(surface_id);
            let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
            return None;
        };
        let Some(front_buffer) = swap_chain.take_surface() else {
            warn!("WebGL external image lock failed: no front buffer for {surface_id:?}");
            self.mark_surface_not_busy(surface_id);
            let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
            return None;
        };
        let (surface_texture, gl_texture, size) =
            match self.rendering_context.create_texture(front_buffer) {
                Ok(texture) => texture,
                Err(front_buffer) => {
                    self.swap_chains
                        .get(surface_id)
                        .expect("Should always have a SwapChain after taking a surface")
                        .recycle_surface(front_buffer);
                    self.mark_surface_not_busy(surface_id);
                    let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
                    return None;
                },
            };
        let locked_buffers = self.locked_front_buffers.entry(surface_id).or_default();
        if !locked_buffers.is_empty() {
            warn!(
                "WebGL external image nested lock: surface={surface_id:?} depth_before={}",
                locked_buffers.len(),
            );
        }
        locked_buffers.push(surface_texture);
        if self.logged_locked_surfaces.insert(surface_id) {
            info!(
                "WebGL external image lock routed: surface={surface_id:?} painter={:?} texture={gl_texture} size={size:?}",
                self.painter_id,
            );
        }

        Some((gl_texture, size))
    }

    fn mark_surface_not_busy(&self, surface_id: WebGLSurfaceId) {
        let mut busy_webgl_context_map = self.busy_webgl_context_map.write();
        let Some(count) = busy_webgl_context_map.get_mut(&surface_id) else {
            warn!("WebGL external image busy counter underflow: missing entry for {surface_id:?}");
            return;
        };

        if *count == 0 {
            warn!("WebGL external image busy counter underflow: zero count for {surface_id:?}");
        } else {
            *count -= 1;
        }
    }

    fn unlock_swap_chain(&mut self, id: WebGLContextId) -> Option<()> {
        let surface_id = WebGLSurfaceId::new(id, self.painter_id);
        debug!("... unlocked chain {:?} for surface {:?}", id, surface_id);

        let locked_front_buffer = match self.locked_front_buffers.get_mut(&surface_id) {
            Some(locked_buffers) => {
                let locked_front_buffer = locked_buffers.pop();
                if locked_buffers.is_empty() {
                    self.locked_front_buffers.remove(&surface_id);
                }
                locked_front_buffer
            },
            None => None,
        };
        let Some(locked_front_buffer) = locked_front_buffer else {
            // The matching lock did not acquire a front buffer (e.g. none was ready),
            // and it already released the busy count on that failure path. There is
            // nothing locked to release here, so return without decrementing the busy
            // counter again (which would underflow).
            return None;
        };

        // Only release the busy count once an actual locked buffer is being unlocked,
        // keeping lock/unlock balanced.
        self.mark_surface_not_busy(surface_id);
        let locked_front_buffer = self
            .rendering_context
            .destroy_texture(locked_front_buffer)?;

        self.swap_chains
            .get(surface_id)
            .expect("Should always have a SwapChain for a busy WebGLContext")
            .recycle_surface(locked_front_buffer);

        let _ = self.webgl_threads.finished_rendering_to_context(surface_id);

        Some(())
    }
}

impl WebRenderExternalImageApi for WebGLExternalImages {
    fn lock(&mut self, id: u64) -> (ExternalImageSource<'_>, Size2D<i32>) {
        match self.lock_swap_chain(WebGLContextId(id)) {
            Some((texture_id, size)) => (ExternalImageSource::NativeTexture(texture_id), size),
            None => (ExternalImageSource::Invalid, Size2D::zero()),
        }
    }

    fn unlock(&mut self, id: u64) {
        self.unlock_swap_chain(WebGLContextId(id));
    }
}

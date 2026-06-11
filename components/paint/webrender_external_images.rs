/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::rc::Rc;

use euclid::default::Size2D;
use log::{debug, info};
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
    locked_front_buffers: FxHashMap<WebGLSurfaceId, SurfaceTexture>,
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

        let front_buffer = self.swap_chains.get(surface_id)?.take_surface()?;
        let (surface_texture, gl_texture, size) =
            match self.rendering_context.create_texture(front_buffer) {
                Ok(texture) => texture,
                Err(front_buffer) => {
                    self.swap_chains
                        .get(surface_id)
                        .expect("Should always have a SwapChain after taking a surface")
                        .recycle_surface(front_buffer);
                    let mut busy_webgl_context_map = self.busy_webgl_context_map.write();
                    *busy_webgl_context_map.entry(surface_id).or_insert(1) -= 1;
                    let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
                    return None;
                },
            };
        self.locked_front_buffers
            .insert(surface_id, surface_texture);
        if self.logged_locked_surfaces.insert(surface_id) {
            info!(
                "WebGL external image lock routed: surface={surface_id:?} painter={:?} texture={gl_texture} size={size:?}",
                self.painter_id,
            );
        }

        Some((gl_texture, size))
    }

    fn unlock_swap_chain(&mut self, id: WebGLContextId) -> Option<()> {
        let surface_id = WebGLSurfaceId::new(id, self.painter_id);
        debug!("... unlocked chain {:?} for surface {:?}", id, surface_id);

        {
            let mut busy_webgl_context_map = self.busy_webgl_context_map.write();
            *busy_webgl_context_map.entry(surface_id).or_insert(1) -= 1;
        }

        let locked_front_buffer = self.locked_front_buffers.remove(&surface_id)?;
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
        let (texture_id, size) = self.lock_swap_chain(WebGLContextId(id)).unwrap_or_default();
        (ExternalImageSource::NativeTexture(texture_id), size)
    }

    fn unlock(&mut self, id: u64) {
        self.unlock_swap_chain(WebGLContextId(id));
    }
}

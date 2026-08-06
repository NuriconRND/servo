/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! One wall tile's window, rendering context (GPU), and paint target: creation and
//! GPU/display binding. Everything here is about a single tile's lifetime; app-wide state
//! (the shared `Servo`/`WebView`, the event loop) lives in `main.rs`.

use std::cell::Cell;
use std::rc::Rc;

use servo::wall_layout::WallLayout;
use servo::{
    DisplayTopology, RenderingContext, WebViewPaintTarget, WindowRenderingContext,
    dxgi_luid_for_gpu_index,
};
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use winit::raw_window_handle::{DisplayHandle, HasWindowHandle};
use winit::window::Window;

/// One wall tile: its own window, rendering context (GPU), and paint target.
/// `paint_target == None` marks the primary tile, which is painted via
/// [`servo::WebView::paint`] (the WebView's own rendering context).
pub(crate) struct TileWindow {
    pub(crate) window: Window,
    pub(crate) rendering_context: Rc<dyn RenderingContext>,
    pub(crate) paint_target: Cell<Option<WebViewPaintTarget>>,
}

/// Create one window + rendering context per wall tile, each optionally bound to the GPU
/// adapter that drives its display. `spatial` is the physical display topology resolved
/// once by the caller (top-left = spatial index 0, left→right then top→bottom);
/// `have_topology` distinguishes "no topology available at all" from "topology available
/// but this tile's display index is out of range" for the fallback diagnostics.
pub(crate) fn create_tile_windows(
    event_loop: &winit::event_loop::ActiveEventLoop,
    display_handle: DisplayHandle,
    layout: &WallLayout,
    tile_indices: &[usize],
    spatial: &[DisplayTopology],
    have_topology: bool,
) -> Vec<TileWindow> {
    let mut tiles = Vec::new();
    for &tile_index in tile_indices {
        let tile = &layout.tiles[tile_index];
        let mut attributes = Window::default_attributes()
            .with_title(format!("winit_wall tile {tile_index}"))
            .with_decorations(false)
            .with_resizable(false);

        let gpu_index = match spatial.get(tile.display) {
            Some(disp) => {
                // Position the window at the real display's desktop origin, and bind its
                // rendering context to the adapter that drives that display. The window is
                // sized to the TILE RECT (not the display) so a tile larger than one display
                // — e.g. a single tile covering the whole virtual viewport — gets a window
                // big enough to show all of it.
                attributes = attributes
                    .with_position(PhysicalPosition::new(disp.left, disp.top))
                    .with_inner_size(LogicalSize::new(
                        tile.rect.size.width as f64,
                        tile.rect.size.height as f64,
                    ));
                if let Some(luid) = dxgi_luid_for_gpu_index(disp.adapter_index)
                    && luid != disp.luid
                {
                    eprintln!(
                        "warning: tile {tile_index}: adapter {} LUID mismatch (topology \
                         {:08x}:{:08x} vs rendering-context {:08x}:{:08x})",
                        disp.adapter_index, disp.luid.0, disp.luid.1, luid.0, luid.1,
                    );
                }
                eprintln!(
                    "tile {tile_index}: display {} -> {} rect[{},{} {}x{}] adapter {} \
                     luid {:08x}:{:08x}",
                    tile.display,
                    disp.device_name,
                    disp.left,
                    disp.top,
                    disp.width,
                    disp.height,
                    disp.adapter_index,
                    disp.luid.0,
                    disp.luid.1,
                );
                Some(disp.adapter_index)
            },
            None => {
                // No topology, or the spatial index is out of range: keep the previous
                // winit-monitor-index placement and let surfman pick the default adapter.
                if have_topology {
                    eprintln!(
                        "warning: tile {tile_index}: display index {} out of range ({} \
                         display(s)); using winit monitor fallback",
                        tile.display,
                        spatial.len()
                    );
                }
                attributes = attributes.with_inner_size(LogicalSize::new(
                    tile.rect.size.width as f64,
                    tile.rect.size.height as f64,
                ));
                if let Some(monitor) = event_loop.available_monitors().nth(tile.display) {
                    let position = monitor.position();
                    attributes =
                        attributes.with_position(PhysicalPosition::new(position.x, position.y));
                } else {
                    attributes = attributes.with_position(LogicalPosition::new(
                        tile.rect.origin.x as f64,
                        tile.rect.origin.y as f64,
                    ));
                }
                None
            },
        };

        let window = event_loop
            .create_window(attributes)
            .expect("Failed to create tile window");
        let window_handle = window.window_handle().expect("Failed to get window handle");
        let rendering_context: Rc<dyn RenderingContext> = Rc::new(
            WindowRenderingContext::new_with_target_gpu(
                display_handle,
                window_handle,
                window.inner_size(),
                gpu_index,
            )
            .expect("Could not create RenderingContext for tile window"),
        );

        tiles.push(TileWindow {
            window,
            rendering_context,
            paint_target: Cell::new(None),
        });
    }
    tiles
}

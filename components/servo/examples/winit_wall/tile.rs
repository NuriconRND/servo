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
///
/// NOTE: this does **not** inject `overlapPx` guard-band `present_inset`, unlike
/// servoshell (`headed_window.rs` around `tile_render_insets`/`set_present_inset`). In
/// servoshell the guard band is a three-piece deal: an offscreen surface expanded by the
/// inset, a scene origin at the *render rect* origin (visible − overlap), and a present-time
/// crop back down to the visible rect (blit source rect or DComp root-visual offset).
/// winit_wall only ever built the third piece — it renders directly into the window
/// surface (no offscreen expansion) and its scene origin is
/// [`WallLayout::tile_origin_device_vector`], which is the *visible* origin, not the render
/// rect origin. Injecting `present_inset` alone was therefore either inert (default/blit-less
/// path: nothing consumes it) or actively wrong (DComp path: it shifted the root visual by
/// `inset` against a scene that was never expanded to compensate, offsetting content by
/// `overlapPx`). Supporting the guard band here needs the offscreen-expansion + render-rect
/// origin + visible-rect blit equivalent of servoshell's `gui.rs`; that's tracked as
/// follow-up work, not done in this shell. Layouts with `overlapPx: 0` are unaffected.
pub(crate) fn create_tile_windows(
    event_loop: &winit::event_loop::ActiveEventLoop,
    display_handle: DisplayHandle,
    layout: &WallLayout,
    tile_indices: &[usize],
    spatial: &[DisplayTopology],
    have_topology: bool,
    vsync_driver: Option<Rc<dyn servo::RefreshDriver>>,
) -> Vec<TileWindow> {
    let mut tiles = Vec::new();
    for &tile_index in tile_indices {
        let tile = &layout.tiles[tile_index];
        let mut attributes = Window::default_attributes()
            .with_title(format!("winit_wall tile {tile_index}"))
            .with_decorations(false)
            .with_resizable(false);

        let auto_gpu_index = match spatial.get(tile.display) {
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
        // An explicit `gpu` in the layout JSON always wins over the auto-assigned adapter, in
        // both the topology-hit and fallback paths above -- matches servoshell's
        // `tile.gpu_override.or(wall_auto_gpu_index)` (`headed_window.rs`), so the same layout
        // JSON behaves the same way in both shells.
        let gpu_index = tile.gpu_override.or(auto_gpu_index);
        if let Some(gpu_override) = tile.gpu_override {
            eprintln!(
                "tile {tile_index}: 'gpu' override -> adapter {gpu_override} (auto-GPU would \
                 have used {auto_gpu_index:?})",
            );
        }

        let window = event_loop
            .create_window(attributes)
            .expect("Failed to create tile window");

        // 타일이 디스플레이를 정확히 채울 때만 borderless fullscreen으로 만든다.
        // flip-model present 자격을 얻기 위한 것으로, 크기가 다르면 일반 창으로 둔다.
        // 토폴로지 폴백 경로에서는 winit 모니터 핸들이 없을 수 있으므로 그때도 일반 창.
        if let Some(disp) = spatial.get(tile.display) {
            let tile_size = window.inner_size();
            // DisplayTopology의 width/height는 i32, winit inner_size는 u32라 캐스팅한다.
            let fills_display =
                tile_size.width as i32 == disp.width && tile_size.height as i32 == disp.height;
            if fills_display && let Some(monitor) = window.current_monitor() {
                eprintln!(
                    "wall: tile {tile_index} matches display {} size {}x{}; using borderless \
                     fullscreen for flip-model present eligibility.",
                    tile.display, tile_size.width, tile_size.height,
                );
                window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(Some(monitor))));
            }
        }

        let window_handle = window.window_handle().expect("Failed to get window handle");
        #[cfg(target_os = "windows")]
        let rendering_context_result =
            WindowRenderingContext::new_with_optional_refresh_driver_and_target_gpu(
                display_handle,
                window_handle,
                window.inner_size(),
                vsync_driver.clone(),
                gpu_index,
            );
        #[cfg(not(target_os = "windows"))]
        let rendering_context_result = WindowRenderingContext::new_with_target_gpu(
            display_handle,
            window_handle,
            window.inner_size(),
            gpu_index,
        );
        let rendering_context: Rc<dyn RenderingContext> = Rc::new(
            rendering_context_result.expect("Could not create RenderingContext for tile window"),
        );

        tiles.push(TileWindow {
            window,
            rendering_context,
            paint_target: Cell::new(None),
        });
    }
    tiles
}

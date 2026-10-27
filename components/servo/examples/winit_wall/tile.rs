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
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::raw_window_handle::{DisplayHandle, HasWindowHandle};
use winit::window::Window;

/// One wall tile: its own window, rendering context (GPU), and paint target.
/// `paint_target == None` marks the primary tile, which is painted via
/// [`servo::WebView::paint`] (the WebView's own rendering context).
pub(crate) struct TileWindow {
    pub(crate) window: Window,
    /// ★병렬 타일 경로(`gfx_wall_parallel_tiles`)에서는 `None` 이다.★ 그 경로에서는 컨텍스트가
    /// 타일 스레드에 살고 셸은 그것을 아예 갖지 않는다 — 만들지도, current 로 삼지도,
    /// present 하지도 않는다. 표출은 그 스레드의 `Painter::render` 가 한다.
    pub(crate) rendering_context: Option<Rc<dyn RenderingContext>>,
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
    let plans = plan_tile_windows(event_loop, layout, tile_indices, spatial, have_topology);
    plans
        .into_iter()
        .map(|plan| bind_context(plan, display_handle, vsync_driver.clone()))
        .collect()
}

/// 창과 그 창이 쓸 GPU 까지만 정해 둔 것. ★컨텍스트는 아직 없다.★
///
/// 창 생성과 컨텍스트 생성을 가른 이유는, A 안에서 컨텍스트는 **타일 스레드가** 만들어야
/// 하는데(surfman `Device` 는 스레드 로컬) 창은 메인 스레드에서만 만들 수 있기 때문이다.
pub(crate) struct TilePlan {
    pub(crate) window: Window,
    pub(crate) gpu_index: Option<usize>,
    pub(crate) tile_index: usize,
}

fn bind_context(
    plan: TilePlan,
    display_handle: DisplayHandle,
    vsync_driver: Option<Rc<dyn servo::RefreshDriver>>,
) -> TileWindow {
    let TilePlan {
        window, gpu_index, ..
    } = plan;
    let window_handle = window.window_handle().expect("Failed to get window handle");
    #[cfg(target_os = "windows")]
    let rendering_context_result =
        WindowRenderingContext::new_with_optional_refresh_driver_and_target_gpu(
            display_handle,
            window_handle,
            window.inner_size(),
            vsync_driver,
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
    TileWindow {
        window,
        rendering_context: Some(rendering_context),
        paint_target: Cell::new(None),
    }
}

/// 창은 만들되 컨텍스트는 만들지 않고, **타일 스레드가 쓸 factory** 와 함께 돌려준다.
/// `gfx_wall_parallel_tiles` 경로 전용이다.
///
/// factory 로 건너가는 것은 HWND 와 hinstance 정수, 창 크기, GPU 인덱스뿐이다 — winit
/// `Window` 자체는 메인 스레드에 남는다(설계 §6.1). 이것이 실제로 되는지는 1 단계
/// 스파이크가 확인했다(`log_webgpu/62`).
#[cfg(target_os = "windows")]
pub(crate) fn split_plan_for_own_thread(
    plan: TilePlan,
) -> (TileWindow, servo::RenderingContextFactory) {
    use std::num::NonZeroIsize;

    use winit::raw_window_handle::{
        RawDisplayHandle, RawWindowHandle, Win32WindowHandle, WindowHandle, WindowsDisplayHandle,
    };

    let raw = plan
        .window
        .window_handle()
        .expect("Failed to get window handle")
        .as_raw();
    // 이 함수는 Windows 전용이고 이 셸은 winit 로만 창을 만든다 — 다른 종류가 나오면
    // 가정이 깨진 것이므로 조용히 되돌아가지 않는다.
    let RawWindowHandle::Win32(win32) = raw else {
        panic!("tile {}: expected a Win32 window handle", plan.tile_index);
    };
    let hwnd = win32.hwnd.get();
    let hinstance = win32.hinstance.map(|value| value.get());
    let size = plan.window.inner_size();
    let gpu_index = plan.gpu_index;

    let factory: servo::RenderingContextFactory = Box::new(move || {
        let mut win32 =
            Win32WindowHandle::new(NonZeroIsize::new(hwnd).expect("HWND must be non-zero"));
        win32.hinstance = hinstance.and_then(NonZeroIsize::new);
        // SAFETY: 창은 메인 스레드가 프로세스 수명 동안 들고 있다(`AppState::tiles`).
        let window_handle = unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(win32)) };
        // SAFETY: Windows 의 디스플레이 핸들은 내용이 없다.
        let display_handle = unsafe {
            DisplayHandle::borrow_raw(RawDisplayHandle::Windows(WindowsDisplayHandle::new()))
        };
        let context = WindowRenderingContext::new_with_optional_refresh_driver_and_target_gpu(
            display_handle,
            window_handle,
            size,
            // vsync 드라이버는 `Rc` 라 건너오지 못한다. 이 경로에서는 표출 클럭이 셸에 있고
            // 타일은 요청받을 때만 그리므로 없어도 된다.
            None,
            gpu_index,
        )
        .expect("Could not create the tile thread's RenderingContext");
        Rc::new(context) as Rc<dyn RenderingContext>
    });

    (
        TileWindow {
            window: plan.window,
            rendering_context: None,
            paint_target: Cell::new(None),
        },
        factory,
    )
}

/// [`plan_tile_windows`] 를 밖에서 부를 수 있게 한 것(병렬 경로가 창을 먼저 만들어야 한다).
pub(crate) fn plan_tile_windows_pub(
    event_loop: &winit::event_loop::ActiveEventLoop,
    layout: &WallLayout,
    tile_indices: &[usize],
    spatial: &[DisplayTopology],
    have_topology: bool,
) -> Vec<TilePlan> {
    plan_tile_windows(event_loop, layout, tile_indices, spatial, have_topology)
}

/// [`TilePlan`] 을 인라인 경로의 [`TileWindow`] 로 만든다(컨텍스트를 메인에서 만든다).
pub(crate) fn bind_context_pub(
    plan: TilePlan,
    display_handle: DisplayHandle,
    vsync_driver: Option<Rc<dyn servo::RefreshDriver>>,
) -> TileWindow {
    bind_context(plan, display_handle, vsync_driver)
}

fn plan_tile_windows(
    event_loop: &winit::event_loop::ActiveEventLoop,
    layout: &WallLayout,
    tile_indices: &[usize],
    spatial: &[DisplayTopology],
    have_topology: bool,
) -> Vec<TilePlan> {
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
                    // ★`tile.rect.origin` 을 창 위치로 쓰면 안 된다.★ 파싱이 끝난 rect 는
                    // **페이지 공간**이다 -- `shift_tiles_to_bounding_box_origin` 이 bounding
                    // box 를 페이지 (0,0) 으로 옮겨 놓으므로 그 값은 데스크톱 좌표가 아니다.
                    // (음수 좌표 레이아웃에서는 전부터 이미 틀린 값이었다.) 여기는 DXGI
                    // 토폴로지도 비었고 winit 에도 그 색인의 모니터가 없는 막다른 길이라,
                    // servoshell 이 같은 자리에서 하는 대로(headed_window.rs) 창 위치를
                    // 창 관리자에 맡기고 사실만 남긴다.
                    eprintln!(
                        "warning: wall tile {tile_index}: no DXGI topology and no winit monitor \
                         at display index {}; letting the window manager place this window",
                        tile.display
                    );
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

        tiles.push(TilePlan {
            window,
            gpu_index,
            tile_index,
        });
    }
    tiles
}

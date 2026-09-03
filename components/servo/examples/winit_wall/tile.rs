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
    let plans = plan_tile_windows(event_loop, layout, tile_indices, spatial, have_topology);
    plans
        .into_iter()
        .map(|plan| bind_context(plan, display_handle, vsync_driver.clone()))
        .collect()
}

/// 창과 그 창이 쓸 GPU 까지만 정해 둔 것. ★컨텍스트는 아직 없다.★
///
/// 창 생성과 컨텍스트 생성을 가른 이유가 둘이다. (1) A 안에서 컨텍스트는 **타일 스레드가**
/// 만들어야 하는데(surfman `Device` 는 스레드 로컬), 창은 메인 스레드에서만 만들 수 있다.
/// (2) [`run_tile_thread_spike`] 가 그 전제를 시험하려면 컨텍스트가 **아직 없는** 창이 필요하다
/// — 이미 컨텍스트가 붙은 HWND 에 두 번째를 만들면 `-DComp on` 에서는 HWND 당 DComp 타깃이
/// 하나뿐이라 실패하고, 그 실패가 "스레드 때문" 으로 오독된다.
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
        rendering_context,
        paint_target: Cell::new(None),
    }
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

        tiles.push(TilePlan {
            window,
            gpu_index,
            tile_index,
        });
    }
    tiles
}

/// ★병렬 타일 렌더(A 안) 1 단계의 관문★: 타일 스레드가 자기 `RenderingContext` 를 **직접
/// 만들고 current 로 삼을 수 있는가**. 여기서 깨지면 A 안은 설계대로 성립하지 않는다
/// (`docs/multigpu/parallel_tile_render_design.md` §6.2-1).
///
/// 왜 "절반만 배선" 이 아니라 스파이크인가: surfman `Device` 는 스레드 로컬이라(설계 §3-1)
/// 만든 스레드가 소멸까지 소유해야 한다. 메인이 만든 컨텍스트를 워커가 쓸 수 없고 그 반대도
/// 안 되므로, 본 경로를 그대로 둔 채 답을 얻으려면 **본 경로 대신** 워커에서 만들어 보고
/// 끝내는 수밖에 없다. 그래서 이 함수는 성공/실패만 보고하고 호출자는 종료한다.
///
/// 창은 메인 스레드가 만들고 **HWND 만** 넘긴다(설계 §6.1) — winit `Window` 는 옮기지
/// 않는다. 그래서 스레드로 건너가는 것은 정수 두 개뿐이다.
/// 창만 만들어 [`run_tile_thread_spike`] 에 넘긴다. ★컨텍스트는 메인 스레드에서 한 번도
/// 만들지 않는다★ — 그래야 워커의 실패가 스레드 탓인지 HWND 재사용 탓인지 헷갈리지 않는다.
#[cfg(target_os = "windows")]
pub(crate) fn plan_and_run_tile_thread_spike(
    event_loop: &winit::event_loop::ActiveEventLoop,
    layout: &WallLayout,
    tile_indices: &[usize],
    spatial: &[DisplayTopology],
    have_topology: bool,
) -> bool {
    let plans = plan_tile_windows(event_loop, layout, tile_indices, spatial, have_topology);
    run_tile_thread_spike(&plans, layout)
}

#[cfg(target_os = "windows")]
fn run_tile_thread_spike(plans: &[TilePlan], layout: &WallLayout) -> bool {
    use std::num::NonZeroIsize;

    use winit::raw_window_handle::{
        RawDisplayHandle, RawWindowHandle, Win32WindowHandle, WindowHandle, WindowsDisplayHandle,
    };

    // GL_VENDOR / GL_RENDERER. `gleam` 은 이 예제의 직접 의존이 아니라 상수를 이름으로 쓸 수
    // 없다 — 반환된 트레이트 객체의 메서드는 그대로 부를 수 있다.
    const GL_VENDOR: u32 = 0x1F00;
    const GL_RENDERER: u32 = 0x1F01;

    let mut workers = Vec::new();
    for plan in plans {
        let tile_index = plan.tile_index;
        let raw = match plan.window.window_handle() {
            Ok(handle) => handle.as_raw(),
            Err(error) => {
                eprintln!("TILESPIKE tile {tile_index}: no window handle ({error:?})");
                return false;
            },
        };
        let RawWindowHandle::Win32(win32) = raw else {
            eprintln!("TILESPIKE tile {tile_index}: not a Win32 window handle");
            return false;
        };
        // 정수로 낮춘다 — `RawWindowHandle` 은 `Send` 가 아니고, 그럴 필요도 없다.
        let hwnd = win32.hwnd.get();
        let hinstance = win32.hinstance.map(|value| value.get());
        let size = plan.window.inner_size();
        let gpu_index = plan.gpu_index;
        let display = layout.tiles.get(tile_index).map(|tile| tile.display);

        workers.push(std::thread::spawn(move || {
            let mut win32 =
                Win32WindowHandle::new(NonZeroIsize::new(hwnd).expect("HWND must be non-zero"));
            win32.hinstance = hinstance.and_then(NonZeroIsize::new);
            // SAFETY: 호출자가 반환 전에 모든 워커를 join 하고, 창은 호출자의 프레임에 살아
            // 있으므로 이 스레드보다 오래 산다.
            let window_handle = unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(win32)) };
            // SAFETY: Windows 의 디스플레이 핸들은 내용이 없다(프로세스 전역).
            let display_handle = unsafe {
                DisplayHandle::borrow_raw(RawDisplayHandle::Windows(WindowsDisplayHandle::new()))
            };

            let started = std::time::Instant::now();
            let created = WindowRenderingContext::new_with_optional_refresh_driver_and_target_gpu(
                display_handle,
                window_handle,
                size,
                // ★vsync 드라이버는 `Rc` 라 `Send` 가 아니다★ — A 안에서는 타일 스레드가
                // 자기 것을 만들어야 한다. 스파이크는 없이 간다.
                None,
                gpu_index,
            );
            let context = match created {
                Ok(context) => context,
                Err(error) => {
                    eprintln!(
                        "TILESPIKE tile {tile_index}: CREATE FAILED on a worker thread \
                         (display={display:?} gpu={gpu_index:?}): {error:?}"
                    );
                    return false;
                },
            };
            let create_ms = started.elapsed().as_secs_f64() * 1000.0;

            if let Err(error) = context.make_current() {
                eprintln!(
                    "TILESPIKE tile {tile_index}: MAKE_CURRENT FAILED on a worker thread: \
                     {error:?}"
                );
                return false;
            }

            // 생성만 되고 current 가 안 되는 경우와 구분하려면 GL 에 직접 물어야 한다.
            let gl = context.gleam_gl_api();
            let vendor = gl.get_string(GL_VENDOR);
            let renderer = gl.get_string(GL_RENDERER);
            let context_size = context.size();
            eprintln!(
                "TILESPIKE tile {tile_index}: OK on worker {:?} -- display={display:?} \
                 gpu={gpu_index:?} size={}x{} create_ms={create_ms:.1} vendor={vendor:?} \
                 renderer={renderer:?}",
                std::thread::current().id(),
                context_size.width,
                context_size.height,
            );
            // 만든 스레드에서 소멸시킨다(surfman 의 스레드 로컬 계약).
            drop(context);
            true
        }));
    }

    let mut all_ok = true;
    for worker in workers {
        match worker.join() {
            Ok(ok) => all_ok &= ok,
            Err(_) => {
                eprintln!("TILESPIKE: a worker thread PANICKED");
                all_ok = false;
            },
        }
    }
    if all_ok {
        eprintln!(
            "TILESPIKE VERDICT: PASS -- every tile created and made current its own \
             RenderingContext on its own thread. Plan A's blocking precondition holds."
        );
    } else {
        eprintln!(
            "TILESPIKE VERDICT: FAIL -- surfman/ANGLE will not do this off the main thread, \
             so plan A does not stand as designed. Read the per-tile lines above."
        );
    }
    all_ok
}

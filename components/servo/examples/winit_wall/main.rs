/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Lightweight multi-GPU "video wall" test harness, derived from `winit_minimal`.
//!
//! One logical [`WebView`] (laid out against a large *virtual viewport*) is fanned out
//! to N tile windows, each with its own [`WindowRenderingContext`] (optionally on its
//! own GPU). This is the same `--wall-all-tiles` / `--wall-layout` model that
//! `ports/servoshell` implements, but without the egui UI / multi-WebView / input
//! plumbing — just the core paint-target fan-out, for rendering/perf testing.
//!
//! Usage:
//!   winit_wall --wall-layout <layout.json> [--wall-all-tiles] [--wall-tile-index N]
//!              [--capture <path.png>] [URL]
//!
//! NOTE: input coordinate remapping is intentionally omitted (clicks won't land
//! correctly); use servoshell for interactive testing. Real per-GPU placement needs a
//! DXGI-capable build (the `no-wgl` feature); on a single GPU all tiles use GPU 0 but
//! the fan-out / frame barrier still exercise.

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::path::Path;
use std::rc::Rc;

use euclid::{Point2D, Scale, Size2D};
use servo::wall_layout::WallLayout;
use servo::{
    DeviceIntRect, PrefValue, Preferences, Servo, ServoBuilder, ViewportDetails, WebView,
    WebViewBuilder, enumerate_display_topology, spatial_order,
};
use url::Url;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::EventLoop;
use winit::raw_window_handle::HasDisplayHandle;

mod tile;
#[cfg(target_os = "windows")]
mod vsync_refresh_driver;

use tile::TileWindow;

const DEFAULT_URL: &str = "https://demo.servo.org/experiments/twgl-tunnel/";

struct Config {
    url: Url,
    layout: WallLayout,
    all_tiles: bool,
    tile_index: usize,
    /// `--capture <path>`: write the primary tile's rendered framebuffer to this PNG once,
    /// `capture_sec` seconds after startup, then exit. Used to validate rendering output.
    capture: Option<String>,
    capture_sec: f64,
    preferences: Preferences,
}

fn parse_args() -> Result<Config, Box<dyn Error>> {
    let mut url: Option<String> = None;
    let mut layout_path: Option<String> = None;
    let mut all_tiles = false;
    let mut tile_index = 0usize;
    let mut capture: Option<String> = None;
    let mut capture_sec = 3.0f64;
    let mut preferences = Preferences::default();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--wall-layout" => {
                layout_path = Some(args.next().ok_or("--wall-layout requires a path")?);
            },
            "--wall-all-tiles" => all_tiles = true,
            "--wall-tile-index" => {
                tile_index = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .ok_or("--wall-tile-index requires an integer")?;
            },
            // `--capture <path>` writes the primary tile's framebuffer to a PNG once (for
            // render validation), `--capture-sec <n>` seconds after startup (default 3), then exits.
            "--capture" => {
                capture = Some(args.next().ok_or("--capture requires a path")?);
            },
            "--capture-sec" => {
                capture_sec = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .ok_or("--capture-sec requires a number")?;
            },
            // `--pref name[=value]` overrides a Servo preference, exactly like servoshell:
            // a bare name (or `name=true`) sets a bool, otherwise the value is parsed
            // booleanish (bool / number / string). e.g. `--pref dom_webrtc_enabled=true`.
            "--pref" => {
                let spec = args.next().ok_or("--pref requires name[=value]")?;
                let mut parts = spec.splitn(2, '=');
                let name = parts.next().unwrap_or_default();
                let value = parts.next().unwrap_or("true");
                preferences.set_value(name, PrefValue::from_booleanish_str(value));
            },
            other if !other.starts_with("--") => url = Some(other.to_string()),
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    let layout_path = layout_path.ok_or("--wall-layout <path> is required")?;
    let layout = WallLayout::from_path(Path::new(&layout_path))?;
    if !all_tiles {
        layout.validate_tile_index(tile_index)?;
    }
    let url = parse_url_or_filename(url.as_deref().unwrap_or(DEFAULT_URL))?;

    Ok(Config {
        url,
        layout,
        all_tiles,
        tile_index,
        capture,
        capture_sec,
        preferences,
    })
}

/// Turn a command-line argument into a [`Url`]. A real URL (scheme longer than one
/// character, so a Windows `C:\…` drive letter is not mistaken for a scheme) is parsed
/// as-is; anything else is treated as a filesystem path (relative to the current
/// directory) and converted to a `file://` URL. This lets the example accept
/// `tests\html\foo.html` like servoshell does, instead of requiring a full `file://`.
fn parse_url_or_filename(input: &str) -> Result<Url, Box<dyn Error>> {
    if let Ok(url) = Url::parse(input) {
        if url.scheme().len() > 1 {
            return Ok(url);
        }
    }

    let path = Path::new(input);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    // Resolve `..`/symlinks and verify existence when possible; fall back to the
    // joined path otherwise. Strip Windows' `\\?\` verbatim prefix for a clean URL.
    let absolute = std::fs::canonicalize(&absolute).unwrap_or(absolute);
    let absolute = absolute
        .to_str()
        .and_then(|s| s.strip_prefix(r"\\?\"))
        .map(std::path::PathBuf::from)
        .unwrap_or(absolute);

    Url::from_file_path(&absolute)
        .map_err(|_| format!("could not build a file URL from {}", absolute.display()).into())
}

fn main() -> Result<(), Box<dyn Error>> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install crypto provider");

    let config = parse_args()?;

    let event_loop = EventLoop::with_user_event()
        .build()
        .expect("Failed to create EventLoop");
    let mut app = App::Initial(Waker::new(&event_loop), config);
    Ok(event_loop.run_app(&mut app)?)
}

struct AppState {
    servo: Servo,
    // Filled in after the tiles exist (the WebView's delegate is `AppState` itself).
    webview: RefCell<Option<WebView>>,
    tiles: Vec<TileWindow>,
    // Present-cost attribution (video-grid perf investigation): isolate the embedder-side
    // `present()` (surfman swap_buffers) cost from `Painter::render()`, so we can tell
    // whether a slow oversized present is the cause vs WebRender update/draw. Logged once/sec.
    present_ms_sum: Cell<f64>,
    present_count: Cell<u32>,
    present_window_start: Cell<Option<std::time::Instant>>,
    // `--capture`: read the primary tile's framebuffer to PNG once, at `capture_deadline`,
    // then request exit. Read happens before `present()` so the backbuffer still holds the frame.
    capture_path: Option<String>,
    capture_deadline: Option<std::time::Instant>,
    captured: Cell<bool>,
    should_exit: Cell<bool>,
}

impl AppState {
    fn note_present(&self, present_ms: f64) {
        let now = std::time::Instant::now();
        let start = match self.present_window_start.get() {
            Some(start) => start,
            None => {
                self.present_window_start.set(Some(now));
                now
            },
        };
        self.present_ms_sum
            .set(self.present_ms_sum.get() + present_ms);
        self.present_count.set(self.present_count.get() + 1);
        let elapsed = now.duration_since(start).as_secs_f64();
        if elapsed >= 1.0 {
            let count = self.present_count.get().max(1);
            eprintln!(
                "Present perf: presents_per_s={:.1} avg_present_ms={:.2}",
                self.present_count.get() as f64 / elapsed,
                self.present_ms_sum.get() / count as f64,
            );
            self.present_ms_sum.set(0.0);
            self.present_count.set(0);
            self.present_window_start.set(Some(now));
        }
    }

    /// If `--capture` is active and its deadline has passed, read the given (primary) tile's
    /// framebuffer to a PNG and request exit. Called once, before `present()`.
    fn maybe_capture(&self, tile: &TileWindow) {
        let (Some(path), Some(deadline)) = (self.capture_path.as_ref(), self.capture_deadline)
        else {
            return;
        };
        if self.captured.get() || std::time::Instant::now() < deadline {
            return;
        }
        self.captured.set(true);
        self.should_exit.set(true);

        let size = tile.rendering_context.size();
        let rect = DeviceIntRect::from_origin_and_size(
            Point2D::new(0, 0),
            Size2D::new(size.width as i32, size.height as i32),
        );
        match tile.rendering_context.read_to_image(rect) {
            Some(image) => match image.save(path) {
                Ok(()) => eprintln!(
                    "capture: wrote {}x{} framebuffer to {path}",
                    image.width(),
                    image.height()
                ),
                Err(error) => eprintln!("capture: failed to write {path}: {error}"),
            },
            None => eprintln!("capture: read_to_image returned None (no framebuffer readback)"),
        }
    }

    fn render_all_tiles(&self) {
        let webview = self.webview.borrow();
        let Some(webview) = webview.as_ref() else {
            return;
        };
        for tile in &self.tiles {
            match tile.paint_target.get() {
                Some(target) => {
                    // Wall frame barrier: skip if this target already rendered this
                    // logical frame (keep the previous frame on screen).
                    if webview
                        .paint_target_keep_previous_logical_frame(target)
                        .is_some()
                    {
                        continue;
                    }
                    let _ = tile.rendering_context.make_current();
                    webview.paint_target(target);
                    let present_start = std::time::Instant::now();
                    tile.rendering_context.present();
                    self.note_present(present_start.elapsed().as_secs_f64() * 1000.0);
                },
                None => {
                    let _ = tile.rendering_context.make_current();
                    webview.paint();
                    // Capture before present (FLIP_DISCARD discards the backbuffer on Present).
                    self.maybe_capture(tile);
                    let present_start = std::time::Instant::now();
                    tile.rendering_context.present();
                    self.note_present(present_start.elapsed().as_secs_f64() * 1000.0);
                },
            }
        }
    }
}

impl ::servo::WebViewDelegate for AppState {
    fn notify_new_frame_ready(&self, _: WebView) {
        // Drive a single repaint request; `render_all_tiles` then paints every tile.
        // winit may deliver `RedrawRequested` to only one window, so we never rely on
        // per-window redraw to paint the wall (matches servoshell).
        if let Some(tile) = self.tiles.first() {
            tile.window.request_redraw();
        }
    }
}

enum App {
    Initial(Waker, Config),
    Running(Rc<AppState>),
}

impl ApplicationHandler<WakerEvent> for App {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        let Self::Initial(waker, config) = self else {
            return;
        };

        // pref 를 여기서 전역으로 확정해 둔다. 원래는 `ServoBuilder::build()`(Servo::new())
        // 안에서만 `prefs::set()`이 불렸는데, 그건 타일 창(과 그 win-vsync 드라이버 선택)이
        // 이미 다 만들어진 *뒤*다 — 창을 만들기 전에 gfx_vsync_enabled 같은 pref 를 읽어야
        // 해서 닭이 먼저냐 문제가 생긴다. `config.preferences`는 이미 CLI 파싱으로 확정된
        // 값이라 그대로 재사용한다. 아래 `ServoBuilder::build()`가 같은 값으로 다시
        // `prefs::set()`을 불러도 diff 가 비어 있어 조용히 반환된다(멱등).
        servo::prefs::set(config.preferences.clone());
        servo::config_dump::log_effective_config();

        // DComp 게이트(`gfx_dcomp_mode`) 주입 — 위 prefs::set()과 같은 이유로 창 생성보다
        // 먼저 해야 한다: 아래 `tile::create_tile_windows()`가 이미 이 값을 보는
        // `RenderingContext`(창 서피스)를 타일마다 만든다. 파싱 정본은
        // `paint_api::rendering_context::DcompMode::parse`(components/shared/paint) 한
        // 곳뿐 — surfman 재수출이 아니다(surfman은 이 정규화가 끝난 불리언만 받는다).
        // 여기서 다시 문자열을 판정하지 않는다.
        paint_api::rendering_context::set_dcomp_mode(
            paint_api::rendering_context::DcompMode::parse(&config.preferences.gfx_dcomp_mode),
        );

        let display_handle = event_loop
            .display_handle()
            .expect("Failed to get display handle");

        let tile_indices: Vec<usize> = if config.all_tiles {
            (0..config.layout.tiles.len()).collect()
        } else {
            vec![config.tile_index]
        };

        // 1) Resolve the physical display topology once. Each tile's `display` is a spatial
        //    index (top-left = 0, row-major); the GPU is the adapter that drives that display.
        let spatial = spatial_order(&enumerate_display_topology());
        let have_topology = !spatial.is_empty();
        // These diagnostics are emitted on stderr (not the `log` crate) because the topology
        // is resolved before `servo.setup_logging()` installs a logger.
        if have_topology {
            eprintln!(
                "Wall display topology ({} desktop display(s)):",
                spatial.len()
            );
            for (spatial_index, disp) in spatial.iter().enumerate() {
                eprintln!(
                    "  display {spatial_index}: {} rect[{},{} {}x{}] adapter {} luid {:08x}:{:08x}",
                    disp.device_name,
                    disp.left,
                    disp.top,
                    disp.width,
                    disp.height,
                    disp.adapter_index,
                    disp.luid.0,
                    disp.luid.1,
                );
            }
        } else {
            eprintln!(
                "warning: no DXGI display topology (non-Windows / non-no-wgl build, or \
                 enumeration failed); falling back to winit monitor index for placement and \
                 the default GPU"
            );
        }

        // `gfx_vsync_enabled` pref(구 SERVO_WIN_VSYNC env)가 켜졌을 때만 DWM 합성 클럭에
        // 프레임 생산을 페이싱한다. 기본 off인 이유: 이 환경에서 DwmFlush가 스핀-웨이트로
        // 동작해 코어 1개를 상시 소모한다. 멀티GPU 간 vsync 동기화 작업의 기반으로 두되
        // 표출 기본값은 아니다.
        //
        // 드라이버는 콜백을 누적하므로 전 타일이 하나를 공유한다 — 타일마다 만들면
        // DwmFlush 스레드가 타일 수만큼 뜬다.
        //
        // pref 는 전역 싱글턴을 읽는다 — 아래에서 `servo_config::prefs::set()`을 창 생성보다
        // 먼저 명시적으로 호출해 뒀으므로(이 함수 최상단) 여기서 이미 확정된 값이 보인다.
        #[cfg(target_os = "windows")]
        let vsync_driver: Option<Rc<dyn servo::RefreshDriver>> = {
            let enabled = servo::prefs::get().gfx_vsync_enabled;
            if enabled {
                eprintln!(
                    "wall: gfx_vsync_enabled=true: pacing frame production to DWM vsync (DwmFlush)."
                );
                Some(Rc::new(vsync_refresh_driver::DwmVsyncRefreshDriver::new()))
            } else {
                None
            }
        };
        #[cfg(not(target_os = "windows"))]
        let vsync_driver: Option<Rc<dyn servo::RefreshDriver>> = None;

        // Create one window + rendering context per tile.
        let tiles = tile::create_tile_windows(
            event_loop,
            display_handle,
            &config.layout,
            &tile_indices,
            &spatial,
            have_topology,
            vsync_driver,
        );

        // 2) Build the shared Servo instance against tile 0's (primary) context.
        let _ = tiles[0].rendering_context.make_current();
        let servo = ServoBuilder::default()
            .event_loop_waker(Box::new(waker.clone()))
            .preferences(std::mem::take(&mut config.preferences))
            .build();
        servo.setup_logging();

        let primary_scale = tiles[0].window.scale_factor() as f32;
        let virtual_viewport_css = config.layout.virtual_viewport_css_size();

        let app_state = Rc::new(AppState {
            servo,
            webview: RefCell::new(None),
            tiles,
            present_ms_sum: Cell::new(0.0),
            present_count: Cell::new(0),
            present_window_start: Cell::new(None),
            capture_deadline: config.capture.as_ref().map(|_| {
                std::time::Instant::now() + std::time::Duration::from_secs_f64(config.capture_sec)
            }),
            capture_path: config.capture.clone(),
            captured: Cell::new(false),
            should_exit: Cell::new(false),
        });

        // 3) One logical WebView whose layout viewport is the whole virtual viewport.
        let webview = WebViewBuilder::new(
            &app_state.servo,
            app_state.tiles[0].rendering_context.clone(),
        )
        .url(config.url.clone())
        .hidpi_scale_factor(Scale::new(primary_scale))
        .viewport_size_override(virtual_viewport_css)
        .viewport_origin_override(
            config
                .layout
                .tile_origin_device_vector(tile_indices[0], Scale::new(primary_scale)),
        )
        .delegate(app_state.clone())
        .build();

        // 4) Register each remaining tile as a secondary paint target.
        for (slot, &tile_index) in tile_indices.iter().enumerate().skip(1) {
            let tile = &app_state.tiles[slot];
            let scale = tile.window.scale_factor() as f32;
            let viewport_details = ViewportDetails {
                size: virtual_viewport_css,
                hidpi_scale_factor: Scale::new(scale),
            };
            let origin = config
                .layout
                .tile_origin_device_vector(tile_index, Scale::new(scale));
            let target =
                webview.add_paint_target(tile.rendering_context.clone(), viewport_details, origin);
            tile.paint_target.set(Some(target));
        }

        *app_state.webview.borrow_mut() = Some(webview);
        *self = Self::Running(app_state);
    }

    fn user_event(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop, _event: WakerEvent) {
        if let Self::Running(state) = self {
            state.servo.spin_event_loop();
        }
    }

    fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        let Self::Running(state) = self else {
            return;
        };
        if state.should_exit.get() {
            event_loop.exit();
            return;
        }
        // While a `--capture` is pending, keep polling + redrawing so the capture deadline
        // fires even on a static page that has otherwise gone idle (no new frame-ready events).
        if state.capture_path.is_some() && !state.captured.get() {
            event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
            if let Some(tile) = state.tiles.first() {
                tile.window.request_redraw();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        if let Self::Running(state) = self {
            state.servo.spin_event_loop();
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                if let Self::Running(state) = self {
                    state.render_all_tiles();
                }
            },
            _ => (),
        }

        // `--capture` requests exit after writing its PNG.
        if let Self::Running(state) = self
            && state.should_exit.get()
        {
            event_loop.exit();
        }
    }
}

#[derive(Clone)]
struct Waker(winit::event_loop::EventLoopProxy<WakerEvent>);
#[derive(Debug)]
struct WakerEvent;

impl Waker {
    fn new(event_loop: &EventLoop<WakerEvent>) -> Self {
        Self(event_loop.create_proxy())
    }
}

impl embedder_traits::EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn embedder_traits::EventLoopWaker> {
        Box::new(Self(self.0.clone()))
    }

    fn wake(&self) {
        if let Err(error) = self.0.send_event(WakerEvent) {
            eprintln!("warning: failed to wake event loop: {error:?}");
        }
    }
}

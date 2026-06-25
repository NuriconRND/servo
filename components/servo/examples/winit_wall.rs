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
//!              [--pref name[=value]]... [URL]
//!
//! NOTE: input coordinate remapping is intentionally omitted (clicks won't land
//! correctly); use servoshell for interactive testing. Real per-GPU placement needs a
//! DXGI-capable build (the `no-wgl` feature); on a single GPU all tiles use GPU 0 but
//! the fan-out / frame barrier still exercise.

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::path::Path;
use std::rc::Rc;

use euclid::Scale;
use servo::{
    PrefValue, Preferences, RenderingContext, Servo, ServoBuilder, ViewportDetails, WebView,
    WebViewBuilder, WebViewPaintTarget, WindowRenderingContext,
};
use tracing::warn;
use url::Url;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use winit::event::WindowEvent;
use winit::event_loop::EventLoop;
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::Window;

use crate::wall::WallLayout;

const DEFAULT_URL: &str = "https://demo.servo.org/experiments/twgl-tunnel/";

struct Config {
    url: Url,
    layout: WallLayout,
    all_tiles: bool,
    tile_index: usize,
    preferences: Preferences,
}

fn parse_args() -> Result<Config, Box<dyn Error>> {
    let mut url: Option<String> = None;
    let mut layout_path: Option<String> = None;
    let mut all_tiles = false;
    let mut tile_index = 0usize;
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

/// One wall tile: its own window, rendering context (GPU), and paint target.
/// `paint_target == None` marks the primary tile, which is painted via
/// [`WebView::paint`] (the WebView's own rendering context).
struct TileWindow {
    window: Window,
    rendering_context: Rc<WindowRenderingContext>,
    paint_target: Cell<Option<WebViewPaintTarget>>,
}

struct AppState {
    servo: Servo,
    // Filled in after the tiles exist (the WebView's delegate is `AppState` itself).
    webview: RefCell<Option<WebView>>,
    tiles: Vec<TileWindow>,
}

impl AppState {
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
                    tile.rendering_context.present();
                },
                None => {
                    let _ = tile.rendering_context.make_current();
                    webview.paint();
                    tile.rendering_context.present();
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

        let display_handle = event_loop
            .display_handle()
            .expect("Failed to get display handle");

        let tile_indices: Vec<usize> = if config.all_tiles {
            (0..config.layout.tiles.len()).collect()
        } else {
            vec![config.tile_index]
        };

        // 1) Create one window + rendering context per tile (GPU affinity from layout).
        let mut built: Vec<(Window, Rc<WindowRenderingContext>)> = Vec::new();
        for &tile_index in &tile_indices {
            let tile = &config.layout.tiles[tile_index];
            let mut attributes = Window::default_attributes()
                .with_title(format!("winit_wall tile {tile_index}"))
                .with_decorations(false)
                .with_resizable(false)
                .with_inner_size(LogicalSize::new(
                    tile.rect.size.width as f64,
                    tile.rect.size.height as f64,
                ));
            if let Some(monitor) = event_loop.available_monitors().nth(tile.monitor) {
                let position = monitor.position();
                attributes =
                    attributes.with_position(PhysicalPosition::new(position.x, position.y));
            } else {
                // No such monitor: lay tiles out side-by-side at their virtual origin.
                attributes = attributes.with_position(LogicalPosition::new(
                    tile.rect.origin.x as f64,
                    tile.rect.origin.y as f64,
                ));
            }

            let window = event_loop
                .create_window(attributes)
                .expect("Failed to create tile window");
            let window_handle = window.window_handle().expect("Failed to get window handle");
            let rendering_context = Rc::new(
                WindowRenderingContext::new_with_target_gpu(
                    display_handle,
                    window_handle,
                    window.inner_size(),
                    Some(tile.gpu),
                )
                .expect("Could not create RenderingContext for tile window"),
            );
            built.push((window, rendering_context));
        }

        // 2) Build the shared Servo instance against tile 0's (primary) context.
        let _ = built[0].1.make_current();
        let servo = ServoBuilder::default()
            .event_loop_waker(Box::new(waker.clone()))
            .preferences(std::mem::take(&mut config.preferences))
            .build();
        servo.setup_logging();

        let primary_scale = built[0].0.scale_factor() as f32;
        let virtual_viewport_css = config.layout.virtual_viewport_css_size();

        let tiles: Vec<TileWindow> = built
            .into_iter()
            .map(|(window, rendering_context)| TileWindow {
                window,
                rendering_context,
                paint_target: Cell::new(None),
            })
            .collect();

        let app_state = Rc::new(AppState {
            servo,
            webview: RefCell::new(None),
            tiles,
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
            warn!(?error, "Failed to wake event loop");
        }
    }
}

/// Minimal wall-layout parser, trimmed from `ports/servoshell/wall_layout.rs`
/// (that module is `pub(crate)` to servoshell and cannot be imported here).
mod wall {
    use std::fmt;
    use std::path::Path;

    use euclid::{Point2D, Rect, Scale, Size2D, Vector2D};
    use serde_json::Value;
    use servo::{CSSPixel, DeviceIndependentPixel, DeviceIntRect, DevicePixel};

    #[derive(Clone, Debug)]
    pub struct WallLayout {
        pub virtual_viewport: Size2D<u32, DeviceIndependentPixel>,
        pub tiles: Vec<WallTile>,
        pub overlap_px: u32,
    }

    #[derive(Clone, Debug)]
    pub struct WallTile {
        pub monitor: usize,
        pub gpu: usize,
        pub rect: Rect<i32, DeviceIndependentPixel>,
    }

    #[derive(Debug)]
    pub enum WallLayoutError {
        Io(std::io::Error),
        Json(serde_json::Error),
        Invalid(String),
    }

    impl fmt::Display for WallLayoutError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                WallLayoutError::Io(error) => write!(f, "{error}"),
                WallLayoutError::Json(error) => write!(f, "{error}"),
                WallLayoutError::Invalid(message) => write!(f, "{message}"),
            }
        }
    }
    impl std::error::Error for WallLayoutError {}

    impl WallLayout {
        pub fn from_path(path: &Path) -> Result<Self, WallLayoutError> {
            let text = std::fs::read_to_string(path).map_err(WallLayoutError::Io)?;
            Self::from_json_str(&text)
        }

        pub fn from_json_str(text: &str) -> Result<Self, WallLayoutError> {
            let value: Value = serde_json::from_str(text).map_err(WallLayoutError::Json)?;
            let virtual_viewport = parse_virtual_viewport(&value)?;
            let tiles = parse_tiles(&value, virtual_viewport)?;
            let overlap_px = get_optional_u32(&value, "overlapPx")?.unwrap_or(0);
            Ok(Self {
                virtual_viewport,
                tiles,
                overlap_px,
            })
        }

        pub fn validate_tile_index(&self, tile_index: usize) -> Result<(), WallLayoutError> {
            if tile_index >= self.tiles.len() {
                return Err(WallLayoutError::Invalid(format!(
                    "wall tile index {tile_index} out of range; layout has {} tile(s)",
                    self.tiles.len()
                )));
            }
            Ok(())
        }

        pub fn virtual_viewport_css_size(&self) -> Size2D<f32, CSSPixel> {
            Size2D::new(
                self.virtual_viewport.width as f32,
                self.virtual_viewport.height as f32,
            )
        }

        pub fn tile_origin_device_vector(
            &self,
            tile_index: usize,
            hidpi_scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,
        ) -> Vector2D<f32, DevicePixel> {
            let Some(tile) = self.tiles.get(tile_index) else {
                return Vector2D::zero();
            };
            Vector2D::new(
                tile.rect.origin.x as f32 * hidpi_scale_factor.get(),
                tile.rect.origin.y as f32 * hidpi_scale_factor.get(),
            )
        }

        #[allow(dead_code)]
        pub fn tile_device_rect(
            &self,
            tile_index: usize,
            hidpi_scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,
        ) -> Option<DeviceIntRect> {
            let tile = self.tiles.get(tile_index)?;
            let origin = Point2D::new(
                (tile.rect.origin.x as f32 * hidpi_scale_factor.get()).round() as i32,
                (tile.rect.origin.y as f32 * hidpi_scale_factor.get()).round() as i32,
            );
            let size = (tile.rect.size.to_f32() * hidpi_scale_factor).to_i32();
            Some(DeviceIntRect::from_origin_and_size(origin, size))
        }
    }

    fn parse_virtual_viewport(
        value: &Value,
    ) -> Result<Size2D<u32, DeviceIndependentPixel>, WallLayoutError> {
        let viewport = value
            .get("virtualViewport")
            .and_then(Value::as_object)
            .ok_or_else(|| WallLayoutError::Invalid("virtualViewport must be an object".into()))?;
        let width = get_positive_u32(viewport, "width")?;
        let height = get_positive_u32(viewport, "height")?;
        Ok(Size2D::new(width, height))
    }

    fn parse_tiles(
        value: &Value,
        virtual_viewport: Size2D<u32, DeviceIndependentPixel>,
    ) -> Result<Vec<WallTile>, WallLayoutError> {
        let tiles = value
            .get("tiles")
            .and_then(Value::as_array)
            .ok_or_else(|| WallLayoutError::Invalid("tiles must be an array".into()))?;
        if tiles.is_empty() {
            return Err(WallLayoutError::Invalid("tiles must not be empty".into()));
        }
        let mut parsed = Vec::with_capacity(tiles.len());
        for (index, tile) in tiles.iter().enumerate() {
            let monitor = get_usize(tile, "monitor")?;
            let gpu = get_usize(tile, "gpu")?;
            let rect = get_rect(tile, "rect")?;
            if rect.size.width <= 0 || rect.size.height <= 0 {
                return Err(WallLayoutError::Invalid(format!(
                    "tile {index} rect width/height must be positive"
                )));
            }
            if rect.origin.x < 0
                || rect.origin.y < 0
                || rect.max_x() > virtual_viewport.width as i32
                || rect.max_y() > virtual_viewport.height as i32
            {
                return Err(WallLayoutError::Invalid(format!(
                    "tile {index} rect exceeds virtualViewport"
                )));
            }
            parsed.push(WallTile { monitor, gpu, rect });
        }
        Ok(parsed)
    }

    fn get_positive_u32(
        value: &serde_json::Map<String, Value>,
        key: &str,
    ) -> Result<u32, WallLayoutError> {
        let number = value
            .get(key)
            .and_then(Value::as_u64)
            .ok_or_else(|| WallLayoutError::Invalid(format!("{key} must be a positive integer")))?;
        if number == 0 || number > u32::MAX as u64 {
            return Err(WallLayoutError::Invalid(format!(
                "{key} must be a positive 32-bit integer"
            )));
        }
        Ok(number as u32)
    }

    fn get_optional_u32(value: &Value, key: &str) -> Result<Option<u32>, WallLayoutError> {
        let Some(number) = value.get(key) else {
            return Ok(None);
        };
        let number = number.as_u64().ok_or_else(|| {
            WallLayoutError::Invalid(format!("{key} must be a non-negative integer"))
        })?;
        if number > u32::MAX as u64 {
            return Err(WallLayoutError::Invalid(format!("{key} must be 32-bit")));
        }
        Ok(Some(number as u32))
    }

    fn get_usize(value: &Value, key: &str) -> Result<usize, WallLayoutError> {
        let number = value.get(key).and_then(Value::as_u64).ok_or_else(|| {
            WallLayoutError::Invalid(format!("{key} must be a non-negative integer"))
        })?;
        usize::try_from(number).map_err(|_| WallLayoutError::Invalid(format!("{key} is too large")))
    }

    fn get_rect(
        value: &Value,
        key: &str,
    ) -> Result<Rect<i32, DeviceIndependentPixel>, WallLayoutError> {
        let rect = value
            .get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| WallLayoutError::Invalid(format!("{key} must be [x, y, w, h]")))?;
        if rect.len() != 4 {
            return Err(WallLayoutError::Invalid(format!(
                "{key} must have exactly four integers"
            )));
        }
        let mut values = [0i32; 4];
        for (index, item) in rect.iter().enumerate() {
            let number = item.as_i64().ok_or_else(|| {
                WallLayoutError::Invalid(format!("{key}[{index}] must be integer"))
            })?;
            values[index] = i32::try_from(number).map_err(|_| {
                WallLayoutError::Invalid(format!("{key}[{index}] out of i32 range"))
            })?;
        }
        Ok(Rect::new(
            Point2D::new(values[0], values[1]),
            Size2D::new(values[2], values[3]),
        ))
    }
}

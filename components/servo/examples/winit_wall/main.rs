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
//!              [--capture <path.png>] [--ignore-certificate-errors]
//!              [--spike-overlay <URL> [--spike-overlay-rect x,y,w,h]] [URL]
//!
//! ★`--spike-overlay` is a THROWAWAY spike★ — it puts a second, independent top-level
//! `WebView` on the wall at a rect, to find out whether the paint layer already supports
//! placing a site as its own WebView instead of framing it in an `iframe` (which most
//! commercial sites refuse). Delete it and everything tagged SPIKE once answered.
//!
//! NOTE: input coordinate remapping is intentionally omitted (clicks won't land
//! correctly); use servoshell for interactive testing. Real per-GPU placement needs a
//! DXGI-capable build (the `no-wgl` feature); on a single GPU all tiles use GPU 0 but
//! the fan-out / frame barrier still exercise.

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::path::Path;
use std::rc::Rc;

use euclid::{Point2D, Scale, Size2D, Vector2D};
use servo::wall_args::WallArgs;
use servo::wall_layout::WallLayout;
use servo::{
    AllowOrDenyRequest, DeviceIntRect, Opts, PrefValue, Preferences, Servo, ServoBuilder,
    ServoDelegate, ViewportDetails, WebView, WebViewBuilder, enumerate_display_topology,
    spatial_order,
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
    /// `--ignore-certificate-errors`: TLS 검증 실패를 전부 수락한다.
    ///
    /// 켜지 않으면 Servo 는 Chrome 과 같이 인터스티셜(`badcert.html`)을 띄우고 운영자가
    /// `Allow certificate temporarily` 를 눌러야 진행한다. ★이 셸에서는 그 통과가 성립하지
    /// 않는다★ — 입력 좌표 리맵이 없어(파일 머리말 참고) 버튼을 정확히 누를 수 없고, 무인
    /// 표출 장비에는 누를 사람도 없다. 그래서 servoshell 과 달리 이 셸에서는 이 플래그가
    /// 사실상 유일한 우회로다.
    ignore_certificate_errors: bool,
    /// ★SPIKE — THROWAWAY★ `--spike-overlay <URL>`: register a SECOND, independent
    /// top-level `WebView` into the *same* tile rendering contexts as the base page, at
    /// `--spike-overlay-rect`, and see whether both composite at once.
    ///
    /// The question this answers: is approach B (a site placed on the wall as its own
    /// top-level `WebView` instead of an `iframe`, so `X-Frame-Options` and JS
    /// frame-busting never apply) supported by the paint layer as it stands? Delete this
    /// flag and everything tagged SPIKE once that is answered — the real feature needs a
    /// placement source of truth and deterministic z-order, neither of which is here.
    spike_overlay: Option<Url>,
    /// ★SPIKE — THROWAWAY★ `--spike-overlay-rect <x,y,w,h>` in **virtual-viewport device
    /// pixels**. Defaults to a 1000x600 box centred on the virtual viewport, which on a
    /// horizontally split wall straddles the seam — that is the interesting case, because
    /// it also tells us whether per-tile clipping of an overlay works for free.
    spike_overlay_rect: Option<(f32, f32, f32, f32)>,
}

fn parse_args() -> Result<Config, Box<dyn Error>> {
    let mut url: Option<String> = None;
    let mut layout_path: Option<String> = None;
    let mut all_tiles = false;
    // `Option` 인 이유는 `paint_api::wall_args` 모듈 doc 참고 — 기본값 0 으로 받으면
    // `--wall-all-tiles` 단독 실행이 "인덱스 0 을 줬다" 로 읽혀 충돌 검사에 걸린다.
    let mut tile_index: Option<usize> = None;
    let mut capture: Option<String> = None;
    let mut capture_sec = 3.0f64;
    let mut preferences = Preferences::default();
    let mut ignore_certificate_errors = false;
    // ★SPIKE — THROWAWAY★ see the `Config` fields of the same name.
    let mut spike_overlay: Option<String> = None;
    let mut spike_overlay_rect: Option<(f32, f32, f32, f32)> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--wall-layout" => {
                layout_path = Some(args.next().ok_or("--wall-layout requires a path")?);
            },
            "--wall-all-tiles" => all_tiles = true,
            "--wall-tile-index" => {
                tile_index = Some(
                    args.next()
                        .and_then(|value| value.parse().ok())
                        .ok_or("--wall-tile-index requires an integer")?,
                );
            },
            // `--capture <path>` writes the primary tile's framebuffer to a PNG once (for
            // render validation), `--capture-sec <n>` seconds after startup (default 3), then exits.
            "--capture" => {
                capture = Some(args.next().ok_or("--capture requires a path")?);
            },
            // servoshell 과 같은 이름/의미다(ports/servoshell/prefs.rs). 인자를 받지 않는
            // 순수 스위치다.
            "--ignore-certificate-errors" => ignore_certificate_errors = true,
            // ★SPIKE — THROWAWAY★
            "--spike-overlay" => {
                spike_overlay = Some(args.next().ok_or("--spike-overlay requires a URL")?);
            },
            "--spike-overlay-rect" => {
                let spec = args.next().ok_or("--spike-overlay-rect requires x,y,w,h")?;
                let parts: Vec<f32> = spec
                    .split(',')
                    .map(|part| part.trim().parse::<f32>())
                    .collect::<Result<_, _>>()
                    .map_err(|_| "--spike-overlay-rect wants four numbers: x,y,w,h")?;
                let [x, y, w, h] = parts[..] else {
                    return Err("--spike-overlay-rect wants exactly four numbers: x,y,w,h".into());
                };
                if w <= 0.0 || h <= 0.0 {
                    return Err("--spike-overlay-rect needs a positive width and height".into());
                }
                spike_overlay_rect = Some((x, y, w, h));
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

    // 검증·해석은 servoshell 과 같은 코드다(`paint_api::wall_args`, Task 7). 예전에는 이
    // 셸이 조합 검사를 아예 하지 않아 `--wall-all-tiles --wall-tile-index 3` 이 조용히
    // 무시됐다 — 이제 servoshell 과 동일하게 오류다.
    let wall_args = WallArgs::new(layout_path.as_deref().map(Path::new), tile_index, all_tiles);
    // 이 셸은 표출 전용이라 레이아웃이 필수다(servoshell 은 없으면 평범한 창으로 뜬다).
    // 그 차이만 여기서 판정하고 나머지는 공유 코드가 한다.
    let layout = wall_args
        .resolve()?
        .ok_or("--wall-layout <path> is required")?;
    let url = parse_url_or_filename(url.as_deref().unwrap_or(DEFAULT_URL))?;

    Ok(Config {
        url,
        layout,
        all_tiles,
        tile_index: wall_args.effective_tile_index(),
        capture,
        capture_sec,
        preferences,
        ignore_certificate_errors,
        // ★SPIKE — THROWAWAY★
        spike_overlay: spike_overlay
            .as_deref()
            .map(parse_url_or_filename)
            .transpose()?,
        spike_overlay_rect,
    })
}

/// Servo 레벨 델리게이트. 이 셸이 필요로 하는 것은 devtools 배선 두 개뿐이다.
///
/// 표출 전용 셸이라 UI 가 없다 — 승인 프롬프트를 띄울 자리가 없으므로 servoshell 과 같이
/// 즉시 허용한다. 서버 자체가 `devtools_server_enabled` pref 로만 뜨므로(기본 꺼짐) 이
/// 허용은 운영자가 명시적으로 켰을 때만 의미를 갖는다. 노출 범위는
/// `devtools_server_listen_address` 로 조인다(예: 127.0.0.1 바인딩).
struct WallServoDelegate;

impl ServoDelegate for WallServoDelegate {
    fn notify_devtools_server_started(&self, port: u16, _token: String) {
        log::info!("Devtools server running on port {port}");
    }

    fn request_devtools_connection(&self, request: AllowOrDenyRequest) {
        request.allow();
    }
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

    // pref 로 옮긴 env 가 아직 설정돼 있으면 여기서 멈춘다(config-surface-consolidation
    // Task 6). 인자 파싱 직후, 이벤트 루프·타일 창 생성 전이다 — servoshell 의 cli.rs 와
    // 같은 자리다. 이 셸도 같은 `etc/multigpu/*.ps1` 로 띄우므로 같은 함정에 걸린다.
    servo::removed_env::check_or_exit();

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
    /// ★SPIKE — THROWAWAY★ The second top-level `WebView` from `--spike-overlay`. Held
    /// only to keep it alive; `render_all_tiles` needs no changes because
    /// `Paint::render_paint_target` renders the whole painter, and a painter draws every
    /// `WebView` registered against its rendering context.
    spike_overlay: RefCell<Option<WebView>>,
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

        // 비디오 WR 탈출 게이트(`gfx_video_escape_mode`) 주입 — 위 DComp 게이트와 같은
        // 이유·같은 시점(Task 4). 파싱 정본은
        // `paint_api::rendering_context::parse_video_escape_token` 한 곳뿐이다.
        paint_api::rendering_context::set_video_escape_mode(
            paint_api::rendering_context::parse_video_escape_token(Some(
                &config.preferences.gfx_video_escape_mode,
            )),
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
        // 이 셸은 지금까지 Opts 를 넘기지 않아 항상 기본값이 쓰였다. 바꾸는 것은
        // `--ignore-certificate-errors` 하나뿐이고 나머지는 그대로 기본값이다
        // (`ServoBuilder` 가 opts 미지정 시 쓰던 값과 동일하다).
        let opts = Opts {
            ignore_certificate_errors: config.ignore_certificate_errors,
            ..Default::default()
        };
        if opts.ignore_certificate_errors {
            log::warn!("--ignore-certificate-errors: accepting ALL TLS certificate errors");
        }
        let servo = ServoBuilder::default()
            .opts(opts)
            .event_loop_waker(Box::new(waker.clone()))
            .preferences(std::mem::take(&mut config.preferences))
            .build();
        servo.setup_logging();
        // Servo 레벨 델리게이트. WebView 델리게이트(`AppState`)와는 별개다 — 이걸 설정하지
        // 않으면 `DefaultServoDelegate` 의 빈 구현이 쓰이고, devtools 연결 요청 객체가 그대로
        // drop 되면서 기본값 Deny 가 회신된다(responders.rs 의 IpcResponder::drop). 그러면
        // `devtools_server_enabled` 로 서버는 떠서 포트까지 바인딩되는데 클라이언트는 붙지
        // 못하는, 원인을 짐작하기 어려운 상태가 된다.
        servo.set_delegate(Rc::new(WallServoDelegate));

        let primary_scale = tiles[0].window.scale_factor() as f32;
        let virtual_viewport_css = config.layout.virtual_viewport_css_size();

        let app_state = Rc::new(AppState {
            servo,
            webview: RefCell::new(None),
            spike_overlay: RefCell::new(None),
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

        // ★SPIKE — THROWAWAY★ 5) A second, independent top-level WebView placed at a rect.
        //
        // Nothing in the engine is changed to make this work, and that is the whole point of
        // the spike. Three facts already in the paint layer carry it:
        //
        //  - `Paint::register_rendering_context` de-duplicates by `Rc::ptr_eq`, so handing it
        //    a tile's existing `RenderingContext` returns that tile's *existing* painter and
        //    merely adds this WebView to it. No second window, no second surface, no second
        //    present.
        //  - `Painter::send_root_pipeline_display_list` walks every `webview_renderer` and
        //    pushes each as its own WebRender iframe into one root display list, so they
        //    composite in a single scene and a single frame.
        //  - `WebViewRenderer::new` starts a WebView `hidden: false` with its rect derived
        //    from `viewport_details.size`, so a smaller viewport size *is* the placement box.
        //
        // Placement therefore needs no new API: the content sits at rect (0,0,w,h) inside a
        // reference frame translated by `-viewport_origin`, so
        //
        //     viewport_origin = <this tile's origin> - <where we want the box to land>
        //
        // puts the box at that virtual-viewport position on every tile at once, and the
        // painter's clip to its own rendering-context rect crops it per tile for free — a box
        // straddling a seam should appear split across two monitors with no extra work.
        if let Some(overlay_url) = config.spike_overlay.clone() {
            let virtual_device = config
                .layout
                .virtual_viewport_device_size(Scale::new(primary_scale))
                .to_f32();
            let (x, y, w, h) = config.spike_overlay_rect.unwrap_or_else(|| {
                let (w, h) = (1000.0f32, 600.0f32);
                (
                    (virtual_device.width - w) / 2.0,
                    (virtual_device.height - h) / 2.0,
                    w,
                    h,
                )
            });
            let placement = Vector2D::new(x, y);
            let overlay_css = Size2D::new(w / primary_scale, h / primary_scale);
            // `eprintln!`, not `log::info!` — the wall's other startup diagnostics
            // (`tile N: display ...`) print this way for a reason: the default log filter
            // swallows `info!`, so the first run of this spike produced no evidence line
            // at all.
            eprintln!(
                "[spike-overlay] second WebView at virtual device rect \
                 [{x},{y} {w}x{h}] ({}x{} css) -> {overlay_url}",
                overlay_css.width, overlay_css.height,
            );

            let overlay = WebViewBuilder::new(
                &app_state.servo,
                app_state.tiles[0].rendering_context.clone(),
            )
            .url(overlay_url)
            .hidpi_scale_factor(Scale::new(primary_scale))
            .viewport_size_override(overlay_css)
            .viewport_origin_override(
                config
                    .layout
                    .tile_origin_device_vector(tile_indices[0], Scale::new(primary_scale))
                    - placement,
            )
            .delegate(app_state.clone())
            .build();

            for (slot, &tile_index) in tile_indices.iter().enumerate().skip(1) {
                let tile = &app_state.tiles[slot];
                let scale = tile.window.scale_factor() as f32;
                let viewport_details = ViewportDetails {
                    size: overlay_css,
                    hidpi_scale_factor: Scale::new(scale),
                };
                let origin = config
                    .layout
                    .tile_origin_device_vector(tile_index, Scale::new(scale))
                    - placement;
                // The returned target is deliberately dropped: `render_all_tiles` paints each
                // tile through the *base* WebView's target, and that renders the whole
                // painter — this overlay included.
                let _ = overlay.add_paint_target(
                    tile.rendering_context.clone(),
                    viewport_details,
                    origin,
                );
                // The placement maths is the whole claim of this spike, so print it: a box
                // straddling a seam must get a *different* origin per tile, and the part of
                // it each tile shows follows from that origin plus the tile's own clip.
                eprintln!(
                    "[spike-overlay] tile {tile_index}: viewport_origin=({},{}) \
                     (tile origin - placement)",
                    origin.x, origin.y,
                );
            }

            *app_state.spike_overlay.borrow_mut() = Some(overlay);
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

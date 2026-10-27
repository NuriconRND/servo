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
//!              [--capture <path.png>] [--ignore-certificate-errors] [URL]
//!
//! NOTE: input coordinate remapping is intentionally omitted (clicks won't land
//! correctly); use servoshell for interactive testing. Real per-GPU placement needs a
//! DXGI-capable build (the `no-wgl` feature); on a single GPU all tiles use GPU 0 but
//! the fan-out / frame barrier still exercise.

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use euclid::{Point2D, Scale, Size2D};
use servo::wall_args::WallArgs;
use servo::wall_layout::WallLayout;
use servo::{
    AllowOrDenyRequest, ConsoleLogLevel, DeviceIntRect, Opts, PrefValue, Preferences, Servo,
    ServoBuilder, ServoDelegate, ViewportDetails, WebView, WebViewBuilder,
    enumerate_display_topology, spatial_order,
};
use url::Url;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::EventLoop;
use winit::raw_window_handle::HasDisplayHandle;


// QPC. `snap_to_dwm_grid` 가 DWM 이 준 QPC 값과 지금을 같은 단위로 비교해야 한다.
// `dwmapi` 와 같은 이유로 직접 링크한다 -- winapi 피처를 셸까지 끌고 오지 않는다.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(counter: *mut i64) -> i32;
    fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
}

/// QPC 주파수는 부팅 중 고정이므로 한 번만 읽는다.
fn qpc_frequency() -> Option<i64> {
    static FREQ: std::sync::OnceLock<Option<i64>> = std::sync::OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut freq: i64 = 0;
        // Safety: 순수 out-param.
        if unsafe { QueryPerformanceFrequency(&mut freq) } != 0 && freq > 0 {
            Some(freq)
        } else {
            None
        }
    })
}

mod crash_report;
mod device_auth_selftest;
mod tick_refresh_driver;
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
    /// `--device-auth-selftest` (시험 키 빌드 전용).
    device_auth_selftest: bool,
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
    let mut device_auth_selftest = false;

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
            // 시험 키 빌드에서만 받는다 -- 실제 키 빌드에서는 아래 `unknown argument` 로 간다.
            "--device-auth-selftest" if sdwall_device_auth::SIGNING_KEY_KIND == "test" => {
                device_auth_selftest = true
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
        device_auth_selftest,
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
    // 네이티브 크래시가 나면 스택을 모듈+오프셋으로 stderr 에 남긴다. 무엇보다 먼저 건다 --
    // 초기화 중에 죽어도 그 스택이 필요하다.
    crash_report::install();
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
    event_loop.run_app(&mut app)?;
    // 장비 인증 자가 시험도 판정을 종료 코드로 낸다(FAIL = 1). 엔진은 먼저 평소처럼 내린다.
    let selftest_exit_code = match &app {
        App::Running(state) => state.device_auth_selftest_exit_code.get(),
        _ => None,
    };
    drop(app);
    if let Some(code) = selftest_exit_code.filter(|code| *code != 0) {
        std::process::exit(code);
    }
    Ok(())
}

struct AppState {
    servo: Servo,
    // Filled in after the tiles exist (the WebView's delegate is `AppState` itself).
    webview: RefCell<Option<WebView>>,
    /// 장비 인증(servo-device-auth.md): 언제 넣을지. 평가 콜백도 만지므로 `Rc` 로 나눈다.
    device_auth: Rc<RefCell<sdwall_device_auth::InjectionTracker>>,
    /// 넘겨도 되는 페이지. 시작 URL 이 불투명 오리진이면 `None` -- 아무 데도 넘기지 않는다.
    device_auth_scope: Option<sdwall_device_auth::PageScope>,
    /// `--device-auth-selftest` (시험 키 빌드 전용).
    device_auth_selftest: Option<Rc<RefCell<device_auth_selftest::DeviceAuthSelfTest>>>,
    /// 자가 시험이 평가 콜백 안에서 정한 다음 동작 스크립트. `about_to_wait` 에서 실행한다.
    device_auth_selftest_next: Rc<Cell<Option<&'static str>>>,
    /// 자가 시험 판정의 종료 코드. 판정이 나면 채우고, `main` 이 이벤트 루프 뒤에 읽는다.
    device_auth_selftest_exit_code: Cell<Option<i32>>,
    tiles: Vec<TileWindow>,
    // `--capture`: read the primary tile's framebuffer to PNG once, at `capture_deadline`,
    // then request exit. Read happens before `present()` so the backbuffer still holds the frame.
    /// Presentation clock period, from `gfx_refresh_hz`.
    ///
    /// ***The wall draws on a clock, not when content says it is ready.*** Everything that
    /// produces pixels -- video decode, a WebGL rAF loop, script animation -- updates its own
    /// state whenever it likes; the clock decides when what exists gets drawn. That is the
    /// model a display wants, and it is not what this shell used to do.
    present_period: std::time::Duration,
    /// `gfx_wall_direct_render` 를 한 번만 읽어 둔다 -- 틱 판정이 초당 300 회 도는 자리라
    /// 거기서 pref 락을 잡을 이유가 없다.
    direct_render: bool,
    /// When the next presentation tick is due.
    next_present_tick: Cell<std::time::Instant>,
    /// 페이싱 스레드가 마지막으로 박자를 낸 시각. ★엔진 wake 가 박자를 가로채지 못하게
    /// 하는 판정에만 쓴다★ -- `user_event` 의 주석 참고.
    last_pacing_tick_at: Cell<Option<std::time::Instant>>,
    /// ★표출 클럭을 디스플레이 vsync 에 묶는다(`gfx_vsync_enabled`).★ 켜졌을 때만 Some.
    ///
    /// 소프트 타이머는 정확히 60 개를 내도 디스플레이와 **다른 발진기**다. 위상이
    /// 미끄러지면 어떤 scanout 엔 present 가 둘 들어가고(한 장은 영영 안 보인다) 어떤
    /// 구간엔 하나도 안 들어간다. 그러면 화면에 보이는 위치는 scanout 직전 0~한 주기
    /// 사이의 표본이 되어 프레임마다 흔들린다 -- 초당 2,304px 로 움직이는 물체에서
    /// 60Hz 면 0~38px 다. 실기에서 `gfx_refresh_hz` 를 60/120/240 으로 올릴수록 떨림이
    /// 단조 감소한 것이 이 산수였다(log_ani_debug/28, 29). 주기를 줄이는 것은 오차를
    /// 작게 만들 뿐이고, vsync 에 묶으면 상수(한 vsync 지연)가 되어 사라진다.
    ///
    /// ★이 드라이버는 이미 있다.★ `gfx_vsync_enabled` 가 `DwmVsyncRefreshDriver` 를
    /// 세우는데, 그것은 `RefreshDriver` 라 **콘텐츠 생산**만 DWM 합성에 맞췄고 셸의
    /// 표출 클럭은 건드린 적이 없다. 이전 실기에서 `-Vsync` 가 아무 변화를 안 낸 이유가
    /// 그것이다. 그래서 셸이 그것을 구독한다.
    ///
    /// ★2026-10-21 부터 Servo 는 이것을 쓰지 않는다.★ 프레임 콜백은 `tick_fire` 가 쏘고,
    /// 이 드라이버에 남은 일은 ***셸에 틱 에지를 주는 것*** 하나다(`pump_vsync` 가
    /// `vsync_due` 를 세우는 콜백을 여기 건다). DwmFlush 스레드는 여전히 하나뿐이다.
    present_vsync: Option<Rc<dyn servo::RefreshDriver>>,
    /// 표출 틱이 Servo 의 프레임 관찰자를 깨우는 손잡이.
    ///
    /// ★세 구성 모두 이것으로 쏜다★(2026-10-21). 예전에는 `FrameClock::Pacing` 일 때만
    /// `Some` 이었고 나머지 둘은 `PaintTimerThread` 와 DwmFlush 스레드가 자기 클럭으로
    /// 쐈다 -- 그러면 "콜백 → 생산 → 렌더" 가 한 틱 안의 순서가 아니라 두 시계의
    /// 경합이 된다(설계 문서 불변식 9).
    ///
    /// ★`present_vsync` 와 방향이 반대다.★ 전자는 셸의 표출 클럭(틱 에지)을 vsync 에
    /// 묶고, 이것은 그 틱을 Servo 의 프레임 클럭으로 쓴다.
    tick_fire: tick_refresh_driver::TickFireHandle,
    /// 고른 프레임 클럭. ★백스톱 판정에만 쓴다★(`needs_pacing_backstop`) -- 손잡이가
    /// 이제 세 구성 모두에 있으므로 "페이싱 구성인가" 를 그것으로 물을 수 없다.
    frame_clock: FrameClock,
    /// vsync 스레드가 세우고 메인이 내린다.
    vsync_due: Arc<AtomicBool>,
    /// 콜백이 걸려 있는가. 등록은 **메인에서만** 한다 -- 드라이버가 `Rc` 라 스레드를 못 건넌다.
    vsync_armed: Cell<bool>,
    /// vsync 신호가 멎었다고 판정한 상태. 이때만 자유 구동 타이머가 민다.
    vsync_stalled: Cell<bool>,
    /// vsync 틱과 실제 표출 사이의 위상 오프셋(`gfx_vsync_phase_pct`).
    /// 0 이면 vsync 가 오는 즉시 틱한다.
    vsync_phase: std::time::Duration,
    /// 오프셋 때문에 미뤄 둔 틱이 있는가.
    vsync_tick_pending: Cell<bool>,
    /// 그 틱을 낼 시각(`vsync + vsync_phase`).
    vsync_tick_at: Cell<Option<std::time::Instant>>,
    /// vsync 콜백이 이벤트 루프를 깨우는 데 쓰는 waker. 엔진과 같은 것을 공유하므로
    /// 중복 깨우기는 그쪽 `pending` 플래그가 흡수한다.
    vsync_waker: Waker,
    /// 마지막으로 틱이 발화한 시각. 백스톱 판정에만 쓴다.
    last_present_tick_at: Cell<std::time::Instant>,
    capture_path: Option<String>,
    capture_deadline: Option<std::time::Instant>,
    /// `gfx_wall_rotate_tile_order` 용 패스 카운터 — 시작 타일을 한 칸씩 돌린다.
    pass_counter: Cell<u64>,
    /// 아직 소비되지 않은 깨우기가 큐에 있는가.
    ///
    /// 엔진은 할 말이 생길 때마다 `EventLoopWaker::wake` 를 부르고, 그것은 winit 큐에
    /// 이벤트 하나를 올린다. 전환 순간 그 호출이 **초당 수천 번**이 되는데(실측 6,949회),
    /// 드레인은 어차피 한 번이면 전부 비우므로 나머지는 전부 군더더기다. 그런데 그
    /// 군더더기가 큐를 비지 않게 만들고, 큐가 비지 않으면 `about_to_wait` 이 돌지 않는다.
    ///
    /// 그래서 **동시에 하나만** 올린다. 이 플래그는 waker 와 공유되며, 소비하는 쪽
    /// (`user_event`)이 드레인 **전에** 내린다.
    wake_pending: Arc<AtomicBool>,
    captured: Cell<bool>,
    should_exit: Cell<bool>,
}

/// `drive_present_clock` 을 부른 자리.
///
/// ★`PacingThread` 하나만 하중이다★ -- `note_pacing_tick` 이 그것으로 페이싱 래치를 세우고,
/// 그 래치가 `pacing_is_driving()` → 엔진 wake 백스톱을 정한다. 나머지 셋은 호출자를
/// 구분해 두는 자리일 뿐이다.
#[derive(Clone, Copy)]
enum ClockSite {
    /// `about_to_wait` 의 마지막 보루(`needs_pacing_backstop`)가 돌린 틱.
    AboutToWait,
    UserEvent,
    WindowEvent,
    /// 전용 페이싱 스레드가 올린 틱.
    PacingThread,
}

impl AppState {
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

        let Some(rendering_context) = tile.rendering_context.as_ref() else {
            // 병렬 타일 경로에서는 셸에 컨텍스트가 없다. ★조용히 빈 파일을 만들지 않고
            // 말한다★ — `--capture` 가 아무 말 없이 아무것도 안 하면 원인을 찾기 어렵다.
            eprintln!(
                "--capture: this tile renders on its own thread (gfx_wall_parallel_tiles), so                  the shell cannot read its framebuffer. Capture is skipped."
            );
            self.captured.set(true);
            return;
        };
        let size = rendering_context.size();
        let rect = DeviceIntRect::from_origin_and_size(
            Point2D::new(0, 0),
            Size2D::new(size.width as i32, size.height as i32),
        );
        match rendering_context.read_to_image(rect) {
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

    /// ★페이싱 래치.★ 틱을 낸 사람이 페이싱 스레드면 그 시각을 남긴다 --
    /// `pacing_is_driving()` 이 읽는 **유일한** 값이고, 그것이 엔진 wake 백스톱을 막는다.
    /// 이 쓰기가 사라지면 `pacing_is_driving()` 이 영구히 false 가 되어 백스톱이 매 wake 마다
    /// 열린다. ★컴파일 에러도, 테스트 실패도 나지 않는다.★
    ///
    /// ★발화한 것만 센다.★ 이 함수는 `drive_present_clock_timer` 의 게이트를 통과한
    /// 뒤에만 불리므로, 여기 찍힌 시각은 "페이싱이 실제로 박자를 냈다" 는 뜻이다.
    fn note_pacing_tick(&self, site: ClockSite) {
        if matches!(site, ClockSite::PacingThread) {
            self.last_pacing_tick_at
                .set(Some(std::time::Instant::now()));
        }
    }

    /// 페이싱 스레드가 박자를 쥐고 있는가.
    ///
    /// 창을 네 주기로 잡은 것은 `pump_paint_animation` 의 `renderer_is_running` 과 같은
    /// 이유다 -- 한두 프레임 늦은 것을 "멎었다" 로 오판하지 않기 위해서다. 진짜로 멎으면
    /// 67ms 뒤에 엔진 wake 가 백스톱으로 받는다.
    fn pacing_is_driving(&self) -> bool {
        self.last_pacing_tick_at
            .get()
            .is_some_and(|at| at.elapsed() < self.present_period * 4)
    }

    /// 표출 클럭. 한 틱마다 엔진이 지금 들고 있는 것을 그린다 -- 무엇이 바뀌었는지
    /// 따지지 않는 것이 핵심이다. 박자가 내용의 함수가 되면 그것은 균일하지도 않고,
    /// 내용 쪽 신호가 하나 빠졌을 때 스스로 회복하지도 못한다.
    ///
    /// ★`about_to_wait` 에만 두면 안 된다.★ winit 은 이벤트 큐가 **빌 때만** 거기에
    /// 도달한다. 전환 순간 엔진이 초당 수천 번 깨우면 큐가 비지 않고, 그러면 이 클럭이
    /// 통째로 멈춘다 -- 2026-09-28 에 실측된 최악은 3.2 초다. 그동안 벽은
    /// 아무것도 새로 표출하지 않으므로, 화면은 애니메이션이 얼어붙은 것으로 보인다.
    /// 그래서 이벤트를 처리하는 쪽에서도 이 클럭을 돌린다.
    ///
    /// 다음 틱 시각을 돌려준다(호출자가 control flow 를 잡을 때 쓴다).
    fn drive_present_clock(&self, site: ClockSite) -> std::time::Instant {
        let now = std::time::Instant::now();

        // ★vsync 에 묶인 경로.★ 틱의 권한이 소프트 타이머가 아니라 DwmFlush 스레드에 있다.
        if let Some(driver) = self.present_vsync.as_ref() {
            self.pump_vsync(now, driver, site);
            if !self.vsync_stalled.get() {
                // 위상 오프셋이 걸려 있으면 **그 시각에** 깨어나야 한다. 콜백은 이미
                // 왔고 틱만 미뤄 둔 상태이므로, 이 시각을 놓치면 한 주기가 통째로 빈다.
                if self.vsync_tick_pending.get() {
                    if let Some(at) = self.vsync_tick_at.get() {
                        if at > now {
                            return at;
                        }
                    }
                }
                // 미뤄 둔 틱이 없으면 깨우기는 콜백이 하므로 이 시각은 마감이 아니라
                // **정지 감시 기한**이다.
                return now + self.vsync_stall_after();
            }
            // 멎었다 -- 아래 자유 구동 타이머가 대신 민다.
        }

        self.drive_present_clock_timer(now, site)
    }

    /// vsync 신호를 소모하고 다음 것을 걸며, 신호가 끊겼는지 판정한다.
    ///
    /// ***왜 스레드를 거치는가*** — `DwmFlush` 는 다음 합성까지 **블록한다.** 메인에서
    /// 부르면 그동안 이벤트를 하나도 처리하지 못하고, 이 셸은 메인이 전부를 돌린다.
    /// `DwmVsyncRefreshDriver` 가 이미 전용 스레드에서 그것을 돌며 등록된 콜백을 쏘므로,
    /// 셸은 콜백에서 플래그만 세우고 루프를 깨운다.
    fn pump_vsync(
        &self,
        now: std::time::Instant,
        driver: &Rc<dyn servo::RefreshDriver>,
        site: ClockSite,
    ) {
        let fired = self.vsync_due.swap(false, Ordering::AcqRel);
        if fired {
            self.vsync_armed.set(false);
        }

        // ★소모한 그 자리에서 다시 건다.★ 다음 호출까지 미루면 그 사이의 vsync 를 놓쳐
        // 표출이 절반으로 떨어진다. 드라이버는 매 vsync 에 콜백을 드레인하므로 등록은
        // 일회성이고 매번 새로 해야 한다.
        if !self.vsync_armed.get() {
            let due = self.vsync_due.clone();
            let waker = self.vsync_waker.clone();
            driver.observe_next_frame(Box::new(move || {
                due.store(true, Ordering::Release);
                <Waker as embedder_traits::EventLoopWaker>::wake(&waker);
            }));
            self.vsync_armed.set(true);
        }

        // ★위상 오프셋.★ 0 이면 vsync 가 오는 즉시 틱한다(종전). 0 이 아니면 vsync 를
        // **기준점**으로만 쓰고 틱은 `vsync + offset` 에 낸다 -- `gfx_vsync_phase_pct` 주석
        // 참고. 개수로는 보이지 않는 위상을 눈으로 이분 탐색하기 위한 손잡이다.
        if fired {
            if self.vsync_stalled.replace(false) {
                log::warn!("wall: vsync 신호가 돌아왔다 -- 표출 클럭을 다시 vsync 에 묶는다.");
            }
            self.vsync_tick_at.set(Some(now + self.vsync_phase));
            self.vsync_tick_pending.set(true);
        }

        // 기한이 찼으면 낸다. `fired` 와 분리해 둔 것은 오프셋이 0 이 아닐 때 이 자리가
        // **다음 호출**에서 돌기 때문이다(그때 깨워 줄 사람은 아래 반환값이 잡는 control flow).
        if self.vsync_tick_pending.get()
            && self.vsync_tick_at.get().is_some_and(|at| now >= at)
        {
            self.vsync_tick_pending.set(false);
            self.last_present_tick_at.set(now);
            self.note_pacing_tick(site);
            self.fire_present_tick();
        }

        if fired {
            return;
        }

        // ***정지 감시.*** vsync 신호가 멎으면(디스플레이 모드 전환, 원격 세션 전환, DWM
        // 재시작) 이 클럭이 통째로 멈추고 벽은 얼어붙는다. 24 시간 도는 장비에서 그것은
        // 허용할 수 없다. 멎었다고 판정하면 자유 구동 타이머로 내려앉고, 신호가 돌아오면
        // 그 자리에서 vsync 로 복귀한다.
        if !self.vsync_stalled.get()
            && now.duration_since(self.last_present_tick_at.get()) >= self.vsync_stall_after()
        {
            self.vsync_stalled.set(true);
            log::warn!(
                "wall: vsync 신호가 {:.0}ms 동안 없다 -- 타이머 페이싱으로 내려앉는다. \
                 복귀 로그가 뜨기 전까지 표출은 vsync 에 묶여 있지 않다.",
                self.vsync_stall_after().as_secs_f64() * 1000.0
            );
            // 타이머를 지금부터 다시 센다 -- 밀린 틱을 몰아 내지 않기 위해서다.
            self.next_present_tick.set(now);
        }
    }

    /// vsync 가 멎었다고 볼 시간.
    ///
    /// ★`present_period` 만으로 잡으면 안 된다.★ `-RefreshHz 240 -Vsync` 처럼 표출 주기를
    /// vsync 간격(60Hz 면 16.7ms)보다 짧게 잡으면 두 주기(8.3ms)가 vsync 보다 먼저 지나
    /// 매번 정지로 오판한다. 이 값은 **페이싱이 아니라 생존 판정**이므로, 어떤 흔한
    /// 리프레시에서도 한 간격보다 확실히 길도록 100ms 를 하한으로 둔다.
    fn vsync_stall_after(&self) -> std::time::Duration {
        std::cmp::max(
            self.present_period * 2,
            std::time::Duration::from_millis(100),
        )
    }

    /// 자유 구동 소프트 타이머 경로. vsync 를 안 쓰거나, 쓰다가 신호가 멎었을 때.
    /// ★다음 틱을 DWM 합성 격자에 스냅한다.★ `gfx_present_align_dwm_pct` 가 켜졌을 때만.
    ///
    /// 자유 구동으로 `next += period` 하면 기준점이 기동 순간에 임의로 정해지고 그대로
    /// 고정된다 -- 실측에서 깨끗한 런은 커밋이 주기의 8% 지점에, 저더 런은 60% 지점에
    /// 떨어졌고 둘 다 런 내내 그 자리였다(log_ani_debug/47). **상수 오프셋을 더하는 것으로는
    /// 고쳐지지 않는다**: 임의 기준점에 상수를 더하면 여전히 임의다.
    ///
    /// 그래서 매 틱 **측정된 vblank 로부터** 다시 센다. 기동 위상과 무관하게 같은 자리에
    /// 떨어지고, DWM 과 우리 타이머의 주파수 차이도 매 틱 흡수된다.
    ///
    /// 조회일 뿐 대기가 아니다 -- 타이머가 깨어나는 시각만 바뀐다.
    fn snap_to_dwm_grid(&self, free_running: std::time::Instant) -> std::time::Instant {
        snap_to_dwm_grid_at(free_running)
    }
}

/// `AppState::snap_to_dwm_grid` 의 본체. ★`self` 를 쓰지 않으므로 자유 함수로 뺀다★ --
/// 페이싱 스레드도 같은 격자에 맞춰 자야 하고, 그 스레드는 `AppState`(Send 아님)를 볼 수
/// 없다. 둘이 다른 산식을 쓰면 박자가 갈린다.
fn snap_to_dwm_grid_at(free_running: std::time::Instant) -> std::time::Instant {
        let pct = servo_config::pref!(gfx_present_align_dwm_pct);
        if !(0..=99).contains(&pct) {
            return free_running;
        }
        let Some((vblank_qpc, period_qpc)) = servo::dwm_composition_grid() else {
            return free_running;
        };
        let Some(freq) = qpc_frequency() else {
            return free_running;
        };
        // ★두 시계를 **붙여서** 읽는다.★ `ahead` 는 `qpc_now` 에서 격자점까지의 거리이므로
        // 더할 앵커도 같은 순간의 `Instant` 여야 한다.
        //
        // 예전에는 호출자가 넘긴 `now` 에 더했는데, 그 사이에 `pref!` 의 RwLock 획득과
        // `dwm_composition_grid()` 의 **시스템 콜**(`DwmGetCompositionTimingInfo`)이 있다.
        // 그만큼 돌려주는 시각이 **일렀고**, 페이싱 스레드는 그 값으로 잘 시간을 계산하므로
        // (`spawn_pacing_thread`) 오차가 Tick 송신 시각에 그대로 샜다. 상수면 거의 무해하지만
        // 시스템 콜 지연은 흔들리고, 그 변동분이 틱 위상의 지터가 된다 -- 위상 측정의 해상도를
        // 깎는 바로 그 양이다(설계 문서 §1 의 경계 ±0.5ms).
        let anchor = std::time::Instant::now();
        let mut qpc_now: i64 = 0;
        // Safety: 순수 out-param.
        if unsafe { QueryPerformanceCounter(&mut qpc_now) } == 0 || qpc_now < 0 {
            return free_running;
        }
        let period = period_qpc as i128;
        let target = period * pct as i128 / 100;
        // vblank 는 드라이버에 따라 직전일 수도 다음일 수도 있다. 나머지 연산을 두 번 걸어
        // 어느 쪽이든 격자 위의 같은 점으로 접는다.
        let base = vblank_qpc as i128 + target;
        let delta = qpc_now as i128 - base;
        // ★"너무 가까우면 한 칸 뒤로" 가 여기 있었다 -- 치웠다.★
        //
        // 그 근거는 "렌더를 시작할 틈이 없으면 그 칸을 놓치느니 다음 칸에 정확히 맞춘다"
        // 였고, 틈이 중요했던 이유는 **커밋이 패스 끝에서 나갔기 때문**이다. 틱이 격자점
        // 직전에 잡히면 커밋도 그만큼 당겨져 위상이 어긋났다.
        //
        // B3 이후 커밋은 틱 위치와 무관하게 격자 고정점에서 나간다
        // (`gfx_present_align_commit_pct`). 틱이 조금 일찍 잡히면 렌더가 그만큼 일찍
        // 시작할 뿐이고, 커밋 위상은 그대로다. 논증이 성립하지 않는다.
        //
        // 그리고 실측에서 이 가드는 ★한 번도 걸리지 않았다★ -- 걸렸다면 그 틱 간격이 2 주기로
        // 벌어졌을 텐데, log_ani_debug_03/12 의 두 실행 모두 건너뛴 간격이 없었고 틱은 초당
        // 60 이었다. 그러니 이 제거는 동작을 바꾸지 않는다 -- 죽은 근거를 치우는 것이다.
        let ahead = period - (((delta % period) + period) % period);
    let wait_s = ahead as f64 / freq as f64;
    anchor + std::time::Duration::from_secs_f64(wait_s)
}

/// ★박자를 이벤트 루프에서 뗀다.★
///
/// 실측(log_ani_debug_02/20, output 페이지): 틱이 초당 60 이 아니라 56(최악 41)까지
/// 떨어지고, `about_to_wait` 이 굶고, **틱의 37% 를 타이머가 아니라 엔진 wake 가
/// 알아챘다**(프로브에서는 2%). `WaitUntil` 이 깨워서 그리는 게 아니라 엔진이 마침
/// 깨워서 지나가다 "틱이 지났네" 하고 그리는 것이다. 렌더 간격이 그만큼 벌어지는 것이
/// 그 결과다.
///
/// 원인은 구조적이다: `about_to_wait` 은 winit 이 **이벤트 큐를 다 비운 뒤에만** 도달하는
/// 자리인데, 표출 클럭의 `WaitUntil` 이 거기 걸려 있다. 애니메이션 다섯 개가 엔진을 계속
/// 깨우면 큐가 비지 않고 박자가 엔진 활동에 얹힌다.
///
/// 그래서 **깨우는 일만** 전용 스레드로 옮긴다. 렌더는 메인에 남는다 -- 그것을 옮길 수 없는
/// 이유는 메시지 루프가 아니라 메인 스레드 친화성이다(`render_all_tiles` 가
/// `Rc<RefCell<..>>` 인 임베더 핸들을 만지고, 타일 0 의 GL 컨텍스트가 메인에서 current 다).
/// 바뀌는 것은 박자의 **권한**뿐이다.
///
/// 메인의 `WaitUntil` 은 그대로 둔다 -- 이 스레드가 죽거나 `send_event` 가 막혀도 벽이
/// 멈추면 안 된다. 두 경로가 같은 격자(`snap_to_dwm_grid_at`)를 쓰므로 박자가 갈리지 않고,
/// `drive_present_clock_timer` 는 `now >= next` 일 때만 발화하므로 이중 틱이 되지 않는다.
///
/// 알려진 약점: `send_event` 도 posted message 라 이미 큐에 쌓인 메시지 뒤에 선다
/// (`WM_PAINT` 보다는 앞이다). 그래도 틱 누락과 "엔진이 알아채는" 것은 사라진다.
///
/// ★깨우는 원천이 둘이다(`gfx_wall_pacing_vblank`, 기본 켜짐).★
///
///  * ***vblank*** -- `IDXGIOutput::WaitForVBlank` 로 primary 의 vblank 에서 깬다. 조회도
///    예측도 없고 깨우는 주체가 커널의 디스플레이 인터럽트다.
///  * ***타이머*** -- 예전 경로. `DwmGetCompositionTimingInfo` 로 격자를 **묻고**
///    (`snap_to_dwm_grid_at`) 그 지점까지 `thread::sleep` 한다.
///
/// ★왜 바꿨나.★ 타이머 경로의 오차는 둘이고 둘 다 그대로 ***틱 위상의 지터***가 된다 --
/// DWM 으로의 프로세스 간 호출 지연(변동한다)과 대기 타이머의 깨우기 지터다. 그리고 커밋
/// 위상 하나가 저더를 정한다(`commit_guard` 모듈 주석: 위상 13% -> 63% 에서 dup 6 ->
/// 2146). 즉 박자의 지터는 그대로 저더의 지터다.
///
/// ★`-DwmAlign` 과의 관계가 이 변경의 요점이다.★ `pct` 가 `1..=99` 면 vblank 뒤에 그만큼
/// **또 자야 하므로** 타이머 지터가 그대로 돌아온다 -- 좋아지는 것은 앵커뿐이고 구조는
/// 예전 그대로다. ★`pct <= 0` 일 때만 대기가 하나도 없는 순수 신호 경로가 된다★, 그리고
/// 실측된 최적점이 바로 거기다(6 구성에서 `DwmAlign` 0 이 dup 평균 8, 50 이 2146). 그래서
/// 이 클럭에서 `pct <= 0` 이 기본이자 권장이고, `1..=99` 는 A/B 를 위해 남겨 둔 것이다.
///
/// ★박자는 패널 주사율로 상한이 걸린다.★ 틱이 vblank 마다 나가므로 `-RefreshHz` 를
/// 패널보다 높게 줘도 그만큼 나오지 않는다. 낮게 주면 vblank 를 건너뛰어 맞춘다.
fn spawn_pacing_thread(
    proxy: winit::event_loop::EventLoopProxy<WakerEvent>,
    period: std::time::Duration,
) {
    let spawned = std::thread::Builder::new()
        .name(String::from("WallPacing"))
        .spawn(move || {
            // ★구성은 기동에 한 번 읽는다.★ `pref!` 는 전역 `RwLock` 을 잡으므로, 틱마다
            // 치면 그 획득이 박자 경로 안에 들어온다 -- 없애려는 것이 바로 그런 양이다.
            let vblank_clock = servo_config::pref!(gfx_wall_pacing_vblank);
            let align_pct = servo_config::pref!(gfx_present_align_dwm_pct);
            let mut next = std::time::Instant::now() + period;
            // vblank 경로에서 "다음에 틱을 낼 시각". `period` 가 패널 주기보다 길 때만
            // 실제로 무언가를 거른다.
            let mut due = std::time::Instant::now();
            loop {
                if vblank_clock {
                    match pace_on_vblank(period, align_pct, &mut due) {
                        // 루프가 끝나면 프록시가 닫힌다. 그때 이 스레드도 끝낸다.
                        VblankPace::Fire => {
                            if proxy.send_event(WakerEvent::Tick).is_err() {
                                return;
                            }
                            continue;
                        },
                        VblankPace::Skip => continue,
                        // vblank 를 못 받았다 -- ★이번 회전만★ 타이머로 떨어진다. 아래가 그
                        // 경로이고, 간격은 거기서 지킨다. 다음 회전은 다시 vblank 를
                        // 시도하므로 일시적 실패(세션 잠금, 토폴로지 변경)가 영구 폴백이
                        // 되지 않는다.
                        VblankPace::Unavailable => {
                            // ★`next` 를 여기서 당겨 놓는다.★ vblank 경로가 도는 동안
                            // 아무도 그것을 밀지 않았으므로 한참 과거이고, 아래의
                            // `while next <= now { next += period }` 는 ***그동안 지나간
                            // 틱 수만큼*** 돈다 -- 한 시간이면 21 만 번, 하루면 5 백만
                            // 번이다. 지나간 칸은 어차피 버리는 값이고 위상은 바로 아래
                            // `snap_to_dwm_grid_at` 이 격자에서 다시 잡으므로, 누적된
                            // `next` 가 나르는 정보는 없다.
                            let now = std::time::Instant::now();
                            if next <= now {
                                next = now + period;
                            }
                        },
                    }
                }
                let now = std::time::Instant::now();
                // 지나간 틱은 버린다 -- 몰아 치면 이미 지난 순간들을 위한 프레임이 쏟아진다
                // (`drive_present_clock_timer` 의 같은 규칙).
                while next <= now {
                    next += period;
                }
                next = snap_to_dwm_grid_at(next);
                let sleep = next.saturating_duration_since(std::time::Instant::now());
                if !sleep.is_zero() {
                    // Win10 1803+ 에서 `sleep` 은 고해상도 대기 타이머를 쓴다 -- winit 이
                    // 같은 이유로 그것을 쓴다고 자기 주석에 적어 두었다.
                    std::thread::sleep(sleep);
                }
                // 루프가 끝나면 프록시가 닫힌다. 그때 이 스레드도 끝낸다.
                if proxy.send_event(WakerEvent::Tick).is_err() {
                    return;
                }
            }
        });
    if let Err(error) = spawned {
        log::warn!("wall: 페이싱 스레드를 띄우지 못했다: {error}; 메인 타이머로 돈다");
    }
}

/// 페이싱 스레드의 vblank 회전 하나가 어떻게 끝났나.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum VblankPace {
    /// vblank 가 왔고 이번 것에 틱을 낸다.
    Fire,
    /// vblank 는 왔지만 이번 것은 거른다 -- 표출 주기가 패널 주기보다 길다.
    Skip,
    /// vblank 를 받지 못했다. 이번 회전은 타이머가 대신한다.
    Unavailable,
}

/// primary 의 다음 vblank 를 기다리고, 이번 것에 틱을 낼지 판정한다.
///
/// ★여기서 블록한다.★ 깨우는 것은 커널의 디스플레이 인터럽트이므로 이 경로에는 DWM
/// 조회도 대기 타이머도 없다 -- `align_pct` 가 `1..=99` 일 때만 뒤엣것이 돌아온다.
///
/// `due` 는 ***요청한 박자가 패널보다 뚜렷이 느릴 때만*** 쓰인다. 왜 그 경우로 좁혔는지는
/// 본문의 주석에 있다 -- 시각끼리 비교하면 두 주기의 미세한 차가 누적되어 몇 분에 한 번
/// 칸을 거르고, 그것이 곧 측정에 섞이는 거짓 dup 이다.
fn pace_on_vblank(
    period: std::time::Duration,
    align_pct: i64,
    due: &mut std::time::Instant,
) -> VblankPace {
    let Some((vblank_qpc, period_qpc)) = servo::wait_primary_vblank() else {
        return VblankPace::Unavailable;
    };
    // ★주기가 패널과 거의 같으면 ***아무것도 재지 않고*** 매 vblank 에 낸다.★
    //
    // 처음에는 `due` 에 반 주기의 여유를 두어 "둘이 같을 때는 안 거른다" 를 노렸다.
    // ***그것은 틀렸다 -- 여유는 첫 건너뜀을 늦출 뿐 막지 못한다.*** `due` 는 `period` 씩,
    // vblank 는 패널 주기씩 가므로 둘이 1ns 라도 다르면 간격이 ***선형으로*** 벌어진다.
    // 실제 값으로: `-RefreshHz 60` 은 16.6667ms 이고 이 패널은 60.003Hz = 16.6658ms 라
    // 프레임당 0.9µs 씩 어긋난다. 반 주기(8.33ms)를 다 쓰는 데 9,255 프레임 = ★154 초★ --
    // 즉 2.5 분마다 한 칸을 거르고, 그 한 칸이 곧 30Hz 프레임 하나다. 120 초 런에서 dup
    // 0~1 개로 나타날 양이고, ★저더를 dup 으로 세는 측정에 정확히 그만큼의 거짓 신호를
    // 섞는다.★ 없애려는 것을 스스로 만드는 꼴이다.
    //
    // 그래서 비교를 ***시각이 아니라 주기끼리*** 한다. 주기 비교에는 누적이 없다.
    // 1.5 배를 경계로 둔 것은 1 배 근처의 미세한 차이를 전부 "같다" 로 삼기 위해서다 --
    // `-RefreshHz` 가 패널보다 높을 때도 여기로 온다(패널보다 빨리 낼 방법은 없다).
    let vblank_period = qpc_frequency()
        .filter(|freq| *freq > 0)
        .map(|freq| std::time::Duration::from_secs_f64(period_qpc as f64 / freq as f64))
        .unwrap_or(period);
    if period * 2 <= vblank_period * 3 {
        align_after_vblank(vblank_qpc, period_qpc, align_pct);
        return VblankPace::Fire;
    }
    // 여기부터는 ***요청한 박자가 패널보다 뚜렷이 느리다***(`-RefreshHz 30` 따위). 그때는
    // 거르는 것이 의도이므로 `due` 로 센다.
    let now = std::time::Instant::now();
    if *due > now + vblank_period / 2 {
        return VblankPace::Skip;
    }
    *due += period;
    if *due <= now {
        // 멎었다 돌아온 경우. 지나간 칸을 몰아 치지 않는다
        // (`drive_present_clock_timer` 의 같은 규칙).
        *due = now + period;
    }
    align_after_vblank(vblank_qpc, period_qpc, align_pct);
    VblankPace::Fire
}

/// `-DwmAlign` 이 `1..=99` 면 vblank 뒤에 그만큼 더 잔다.
///
/// ★이 함수에 들어오는 순간 순수 신호 경로가 아니다.★ 대기 타이머의 깨우기 지터가 그대로
/// 돌아오고, 그 양은 예전 `snap_to_dwm_grid_at` 경로와 같다 -- 좋아진 것은 앵커(기다려서
/// **관측한** vblank 대 DWM 에 **물어본** 격자)뿐이고 구조는 그대로다. 그래서 `pct <= 0`
/// 이 이 클럭의 기본이자 권장이고, 실측된 최적점도 거기다(`DwmAlign` 0 에서 dup 평균 8,
/// 50 에서 2146). 남겨 둔 이유는 ***같은 바이너리에서 A/B 를 하기 위해서***다.
fn align_after_vblank(vblank_qpc: u64, period_qpc: u64, align_pct: i64) {
    if !(1..=99).contains(&align_pct) {
        return;
    }
    let Some(freq) = qpc_frequency().filter(|freq| *freq > 0) else {
        return;
    };
    let mut now_qpc: i64 = 0;
    // Safety: 순수 out-param.
    if unsafe { QueryPerformanceCounter(&mut now_qpc) } == 0 || now_qpc < 0 {
        return;
    }
    let target = vblank_qpc as i128 + period_qpc as i128 * align_pct as i128 / 100;
    let ahead = target - now_qpc as i128;
    if ahead <= 0 {
        return;
    }
    std::thread::sleep(std::time::Duration::from_secs_f64(ahead as f64 / freq as f64));
}

impl AppState {
    fn on_device_auth_state(&self, auth: sdwall_device_auth::AuthState) {
        // 지문은 로그에도 남기지 않는다 -- `AuthState` 에는 애초에 없다.
        log::warn!("device-auth: {}", auth.label());
        let pending = self.device_auth.borrow_mut().on_auth_state(auth);
        self.inject_device_auth(pending);
    }

    fn inject_device_auth(&self, pending: Option<sdwall_device_auth::AuthState>) {
        let (Some(auth), Some(scope)) = (pending, self.device_auth_scope.as_ref()) else {
            return;
        };
        // 이 주입이 어느 문서의 것인지 -- 늦게 온 실패가 새 문서의 주입을 무르지 않게 한다.
        let document = self.device_auth.borrow().document();
        let Some(webview) = self.webview.borrow().clone() else {
            self.device_auth.borrow_mut().on_injection_failed(document);
            return;
        };
        let tracker = self.device_auth.clone();
        let label = auth.label();
        let selftest = self.device_auth_selftest.clone();
        let selftest_next = self.device_auth_selftest_next.clone();
        webview.evaluate_javascript(scope.injection_script(&auth), move |result| match result {
            Ok(::servo::JSValue::String(outcome)) => {
                log::info!("device-auth: inject {label} -> {outcome}");
                // `same` = 이 문서에 같은 값이 이미 있어 다시 쓰지 않았다(page.rs). 자가 시험에는
                // `set` 과 똑같이 센다 -- `#` 이동 구간에 `same` 이 와도 "또 넘기려 했다"는
                // 회귀이므로 그대로 실패로 잡혀야 한다.
                if outcome == "set" || outcome == "same" {
                    let next = selftest.as_ref().and_then(|s| s.borrow_mut().on_injected());
                    if let Some(script) = next {
                        // Servo 는 완료 콜백을 `javascript_evaluator` 를 빌린 채 부른다. 콜백 안에서
                        // `evaluate_javascript` 를 또 부르면 `RefCell already borrowed` 로 죽으므로
                        // 스크립트만 적어 두고 `about_to_wait` 에서 실행한다.
                        selftest_next.set(Some(script));
                    }
                }
            },
            Ok(other) => log::warn!("device-auth: inject {label} -> unexpected {other:?}"),
            Err(error) => {
                log::warn!("device-auth: inject {label} failed: {error:?}");
                tracker.borrow_mut().on_injection_failed(document);
            },
        });
    }

    /// 자가 시험이 끝났으면 판정을 남기고 `true` -- 호출자가 이벤트 루프를 끝낸다.
    fn device_auth_selftest_finished(&self) -> bool {
        let Some(selftest) = &self.device_auth_selftest else {
            return false;
        };
        let verdict = selftest
            .borrow_mut()
            .take_verdict(std::time::Instant::now());
        let Some(verdict) = verdict else {
            return false;
        };
        match &verdict {
            device_auth_selftest::Verdict::Pass => {
                log::warn!("device-auth selftest: PASS documents=3")
            },
            device_auth_selftest::Verdict::Fail(reason) => {
                log::error!("device-auth selftest: FAIL {reason}")
            },
        }
        // 이벤트 루프가 끝난 뒤 `main` 이 이 값으로 종료 코드를 낸다(FAIL = 1).
        self.device_auth_selftest_exit_code
            .set(Some(verdict.exit_code()));
        true
    }

    fn drive_present_clock_timer(&self, now: std::time::Instant, site: ClockSite) -> std::time::Instant {
        let mut next = self.next_present_tick.get();
        if now >= next {
            // ***Advance to the first future tick rather than adding one period.*** After a
            // stall (a long render, a debugger break) `next` can be far in the past, and
            // stepping by one period would fire a burst of catch-up frames for moments that
            // have already gone by. Skipping them is what a display does.
            while next <= now {
                next += self.present_period;
            }
            // ★격자에 스냅한다.★ 자유 구동으로 센 다음 칸을 DWM 합성 격자 위의 같은
            // 지점으로 옮긴다. 꺼져 있으면 그대로 돌려준다.
            next = self.snap_to_dwm_grid(next);
            self.next_present_tick.set(next);
            self.note_pacing_tick(site);
            self.fire_present_tick();
        }
        next
    }

    /// 틱이 발화했다 -- 그릴 사람을 부른다.
    ///
    /// ★`gfx_wall_direct_render` 가 켜져 있으면 `request_redraw()` 를 거치지 않는다.★
    /// 화면에 내보내는 것은 `IDCompositionDevice::Commit`(또는 스왑체인 `Present`)이고 둘 다
    /// 순수 COM 호출이라 메시지 펌프와 무관하다. 그런데 `request_redraw()` 는 Windows 에서
    /// `RedrawWindow` 로 `WM_PAINT` 를 요청하고, `WM_PAINT` 는 posted message 큐가 다 빈
    /// 뒤에야 합성되는 최저 우선순위 메시지다 -- 엔진 wake 를 초당 300 회 받는 이 셸에서
    /// 렌더가 그 큐 뒤에 줄을 선다. 틱 간격이 p95 17.23ms(주기 16.667ms)로 늦는 것의 유력한
    /// 후보가 그 왕복이다(타이머 해상도는 아니다 -- winit 은 100ns 단위 고해상도 대기를 쓴다).
    ///
    /// 렌더를 스레드로 옮기는 것과는 다른 이야기다. 그것을 막는 것은 메시지 루프가 아니라
    /// 메인 스레드 친화성이다(`render_all_tiles` 가 `Rc<RefCell<..>>` 인 임베더 핸들을
    /// 만지고, 타일 0 의 GL 컨텍스트가 메인에서 current 다). 여기서 바꾸는 것은 **누가 언제
    /// 부르나**뿐이고, 부르는 스레드는 그대로 메인이다.
    fn fire_present_tick(&self) {
        // ★이 순간이 이 프레임의 시각이다.★ 여기서 재야 렌더 준비 비용이 섞이지 않는다 --
        // 섞이면 그만큼이 애니메이션 값의 지터가 된다(설계 문서 3). 엔진은 이 값을
        // `TickAnimation` 에 실어 script 까지 보내고, script 는 "자기가 메시지를 처리한
        // 순간" 대신 이것을 애니메이션의 기준으로 쓴다. 이 예제는 `servo` 패키지 안에
        // 있어 `servo_base` 를 그대로 쓸 수 있으므로 시그니처는 바꾸지 않는다.
        servo::note_frame_time(servo_base::cross_process_instant::CrossProcessInstant::now());
        // ★틱의 시각을 paint 쪽에 남긴다.★ 커밋은 저쪽에서 나가므로, 두 시각의 차를
        // 재려면 기준점이 거기까지 가야 한다. ★두 발화 경로(타이머 / vsync)가 모두 여기를
        // 지나므로 한 자리면 충분하다.★
        servo::note_present_tick();
        // ★지난 패스가 미뤄 둔 Commit 을 여기서 낸다 -- 렌더보다 앞이다.★
        //
        // 커밋 위상이 곧 이 틱의 위상이 되고, 그것은 `-DwmAlign` 이 정한다. 패스 끝에서 내면
        // 위상이 패스 길이를 물려받아 주기의 35~62% 에 흩어지는데 그 구간은 DWM 인테이크
        // 마감 너머라 저더가 수십 배가 된다(log_ani_debug_06/15·16).
        //
        // 네 디바이스가 한 호출에 함께 나가므로 네 타일이 같은 합성에 실린다.
        if let Some(webview) = self.webview.borrow().clone().as_ref() {
            webview.commit_deferred_dcomp_at_tick();
        }
        // ★Servo 의 프레임 관찰자를 여기서 깨운다.★ 세 구성(Pacing / Timer / Vsync)
        // 모두 이 자리에서 쏜다 -- `notify_will_paint` 가 등록해 둔 콜백을 부를 사람은
        // 여기뿐이다. 구성마다 다른 것은 ***틱 에지를 누가 내는가*** 뿐이고(페이싱 스레드 /
        // `PaintTimerThread` 의 깨우기 / DwmFlush 스레드), 발사는 언제나 틱이다.
        //
        // ***렌더 전에 불러야 한다*** -- 그 콜백이 `waiting_for_frame` 을 내리고, 그것이
        // 내려가야 `needs_repaint()` 가 참이 된다.
        self.tick_fire.fire();
        // ★틱이 벽 프레임을 낸다.★ 자리는 콜백 발사(`fire`) 뒤, 렌더 앞이다 -- 설계 2-2 의
        // 순서 그대로이고, `gather_components` 가 건 깨우기를 같은 틱의 `fire()` 가 먹어
        // 치워 `wake()` 가 두 번 나는 일도 없다. 구성 요소를 모으고 내는 순서는
        // `on_wall_tick` 안에 있어 여기서 틀릴 수 없다.
        //
        // ★`gfx_single_frame_producer` 게이트는 2026-10-22 에 사라졌다. 그것이 막던 증상
        // (구 생산자 열한 곳이 살아 있는 동안 틱까지 내서 한 틱에 합성이 두 번 나가던 것)은
        // 그 열한 곳이 2026-10-21 Task 7 에 삭제되어 구조적으로 불가능하다 —
        // docs/superpowers/specs/2026-10-21-single-frame-producer-design.md 4 절.★ 그 뒤로
        // 게이트의 off 는 폴백이 아니라 ***멈춘 벽***이었고(부트스트랩 외에 WR 프레임이 한
        // 장도 안 난다), servoshell 쪽은 2026-10-22 Task 9.5 부터 pref 와 무관하게 틱한다 --
        // 셸마다 다른 규칙을 남겨 둘 이유가 없다.
        //
        // 이 셸은 틱 에지가 어느 구성에서도 나온다(Pacing / Vsync 는 자기 스레드, Timer 는
        // `PaintTimerThread` 의 깨우기 -- 다만 Timer 의 박동은 `waiting_for_frame` 일 때만
        // 걸린다). 그래서 예약 비트(`on_demand_wall_tick`)를 보지 않고 에지마다 부른다.
        let webview = self.webview.borrow().clone();
        if let Some(webview) = webview {
            let _ = webview.on_wall_tick();
        }
        if self.direct_render {
            self.render_all_tiles();
            return;
        }
        if let Some(tile) = self.tiles.first() {
            tile.window.request_redraw();
        }
    }

    fn render_all_tiles(&self) {
        let webview = self.webview.borrow();
        let Some(webview) = webview.as_ref() else {
            return;
        };
        // `gfx_wall_rotate_tile_order`: 시작 타일을 패스마다 한 칸 돌린다.
        //
        // 한 타일이 패스 비용을 몰아서 낸다(큰 캔버스 + DComp off 에서 타일 1 이 42.2ms 중
        // 25.1ms, 나머지 셋은 각 5.7ms). GPU 는 30% 대라 처리량이 아니다. 돌려 보면 그 비용이
        // **첫 번째 위치**의 것인지(그러면 타일별 평균이 고르게 퍼진다) 특정 painter 의
        // 것인지(그 타일만 계속 느리다) 갈린다.
        let rotate = servo::pref!(gfx_wall_rotate_tile_order);
        let count = self.tiles.len();
        let offset = if rotate && count > 0 {
            (self.pass_counter.get() as usize) % count
        } else {
            0
        };
        self.pass_counter
            .set(self.pass_counter.get().wrapping_add(1));

        // ★자기 스레드를 가진 타일은 먼저 전부 발사한다.★ 그래야 그것들이 도는 동안 아래
        // 루프가 primary 타일을 그리고, 마지막에 한 번만 기다린다. 발사하고 곧장 기다리면
        // primary 는 그 뒤에 혼자 남아 겹치지 않는다 — 그러면 팬아웃의 이득이 절반이 된다.
        //
        // 배리어는 여기서도 그대로다: keep-previous 로 표시된 타일은 **아예 보내지 않는다**.
        // 판정을 메인에서 미리 하는 것이 설계 §3 의 결론이다(타일이 배리어를 볼 필요가 없다).
        let mut dispatched = vec![false; self.tiles.len()];
        let in_flight = {
            let mut targets = Vec::new();
            for (index, tile) in self.tiles.iter().enumerate() {
                let (Some(target), None) = (tile.paint_target.get(), &tile.rendering_context)
                else {
                    continue;
                };
                if webview
                    .paint_target_keep_previous_logical_frame(target)
                    .is_some()
                {
                    dispatched[index] = true;
                    continue;
                }
                targets.push(target);
                dispatched[index] = true;
            }
            (!targets.is_empty()).then(|| webview.dispatch_paint_targets(&targets))
        };

        for step in 0..count {
            let tile_index = (offset + step) % count;
            // 이미 발사한 타일은 여기서 다시 그리지 않는다(배리어로 건너뛴 것도 포함).
            if dispatched[tile_index] {
                continue;
            }
            let tile = &self.tiles[tile_index];
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
                    // ★컨텍스트가 없는 타일은 자기 스레드가 전부 한다★
                    // (`gfx_wall_parallel_tiles`): current 로 삼는 것도, 표출도 그쪽이다.
                    // 셸이 여기서 할 수 있는 것은 "그려라" 뿐이다.
                    let Some(rendering_context) = tile.rendering_context.as_ref() else {
                        webview.paint_target(target);
                        continue;
                    };
                    let _ = rendering_context.make_current();
                    webview.paint_target(target);
                    rendering_context.present();
                },
                None => {
                    let rendering_context = tile
                        .rendering_context
                        .as_ref()
                        .expect("the primary tile always owns its context on the shell side");
                    let _ = rendering_context.make_current();
                    webview.paint();
                    // Capture before present (FLIP_DISCARD discards the backbuffer on Present).
                    self.maybe_capture(tile);
                    rendering_context.present();
                },
            }
        }
        // ★낡음(2026-09-02 기록, 같은 날 `d3fa6dbd686` 이후 무효)★ — 미루기가 기본이다. 조건은 "`gfx_dcomp_commit_in_end_frame` 이 꺼져 있으면" 이다.
        // `gfx_dcomp_defer_commit` 이 켜져 있으면 각 타일의 `end_frame` 이 DComp Commit 을
        // 미뤄 두었다. 여기가 그것을 흘리는 자리다 — 페인트 밖이라 컴포지터 대여와 겹치지
        // 않는다. pref 가 꺼져 있으면 미뤄 둔 것이 없어 아무 일도 하지 않는다.
        //
        // 이 한 줄이 실험의 전부다: 지금까지 render→commit 이 타일마다 번갈아 돌았는데,
        // 이제 렌더 넷이 먼저 끝나고 Commit 넷이 몰려 나간다. 패스 시간이 줄면 Commit 의
        // 대기는 겹칠 수 있다는 뜻이고, 그대로면 DWM 이 직렬화한다는 뜻이다.
        // 발사해 둔 타일들을 여기서 기다린다 — primary 를 그리는 동안 나란히 돌았다.
        // ★반드시 Commit 플러시보다 먼저다★: 그 타일들의 `end_frame` 이 아직 안 끝났으면
        // 미뤄 둔 Commit 도 아직 없다.
        if let Some(in_flight) = in_flight {
            let _ = in_flight.join();
        }

        webview.flush_deferred_dcomp_commits();
    }
}

/// `LoadStatus` 는 최상위 문서에만 나온다 -- 장비 인증을 넘길 문서도 최상위뿐이다.
fn document_event(status: ::servo::LoadStatus) -> sdwall_device_auth::DocumentEvent {
    use sdwall_device_auth::DocumentEvent;
    match status {
        ::servo::LoadStatus::Started => DocumentEvent::Started,
        ::servo::LoadStatus::HeadParsed => DocumentEvent::HeadParsed,
        ::servo::LoadStatus::Complete => DocumentEvent::Complete,
    }
}

impl ::servo::WebViewDelegate for AppState {
    fn notify_load_status_changed(&self, _: WebView, status: ::servo::LoadStatus) {
        if matches!(status, ::servo::LoadStatus::Started) {
            if let Some(selftest) = &self.device_auth_selftest {
                selftest.borrow_mut().on_document_started();
            }
        }
        let pending = self
            .device_auth
            .borrow_mut()
            .on_document_event(document_event(status));
        self.inject_device_auth(pending);
    }

    fn notify_new_frame_ready(&self, _: WebView) {
        // ***Deliberately does nothing.*** This used to request a redraw, which made the
        // presentation cadence a function of when content happened to finish -- so the wall
        // ran at 138 passes/s on one page and 52 on another, neither of them the display's
        // 60. Worse, it made presentation depend on a content signal: when that signal was
        // lost (a stuck in-flight flag, a stale keep-previous entry) the tile stopped
        // updating for the rest of the session. Both were real bugs on this wall.
        //
        // `about_to_wait` now drives redraws from a clock, so a frame that has just been
        // built is simply what the next tick will draw.
    }

    /// Put the page's own `console` output in the log.
    ///
    /// ***Without this a page cannot report anything.*** The default delegate drops these,
    /// so every probe page written for this shell was talking to nobody -- and a probe
    /// that appears to say nothing is indistinguishable from a probe whose page never ran.
    /// Cost this a whole diagnostic round on 2026-09-08.
    fn show_console_message(&self, _: WebView, level: ConsoleLogLevel, message: String) {
        if let Some(selftest) = &self.device_auth_selftest {
            selftest.borrow_mut().on_console(&message);
        }
        match level {
            ConsoleLogLevel::Error => log::error!("[page] {message}"),
            // `warn!` for the rest deliberately: the wall launcher's RUST_LOG leads with
            // `warn`, and a page that cannot be heard is the problem being fixed here.
            _ => log::warn!("[page] {message}"),
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
        // ★시계를 하나로 고른다.★ `FrameClock` 의 주석과 `choose_frame_clock` 의 테스트가
        // 조합을 정한다.
        #[cfg(target_os = "windows")]
        let clock = choose_frame_clock(
            servo::prefs::get().gfx_vsync_enabled,
            servo_config::pref!(gfx_wall_pacing_thread),
        );
        #[cfg(not(target_os = "windows"))]
        let clock = FrameClock::Timer;

        // 구성별 배너. ★판정 자체는 `frame_callback_wiring` 에 있다★ -- 그쪽은 순수
        // 함수라 테스트에서 부르고, 여기는 로그만 낸다.
        match clock {
            FrameClock::Vsync => eprintln!(
                "wall: gfx_vsync_enabled=true: DWM vsync(DwmFlush) 에 표출과 값 표본을 묶는다 (phase_pct={}).",
                servo_config::pref!(gfx_vsync_phase_pct).clamp(0, 99)
            ),
            FrameClock::Pacing => {
                // ★"스레드가 안 뜬다" 가 아니라 "프레임을 끌지 않는다" 다.★
                // `Painter::new` 가 `WebContentAnimator::new(.., (*timer_refresh_driver)
                // .clone(), ..)` 를 무조건 평가하므로 `LazyCell` 이 forced 되고
                // ★낡음(2026-10-13 기록, 2026-10-16 `e96e9f71a06` 이후 줄 표류로 무효)★ — 실제 자리는 `painter.rs:1228`.
                // `PaintTimerThread` 는 어차피 뜬다(`painter.rs:1088`). 거기 남는 일은
                // caret 깜빡임뿐이다. 스레드 수를 세어 이 변경을 확인하려 하면 틀린 결론이
                // 나온다.
                eprintln!(
                    "wall: 표출 틱이 프레임 클럭이다 -- Servo 의 PaintTimerThread 가 프레임을 끌지 않는다."
                )
            },
            FrameClock::Timer => {},
        }

        // ★셸의 표출 클럭(= 틱 에지)이 타는 드라이버.★ `Vsync` 에서만 만든다. 페이싱
        // 드라이버나 틱 드라이버를 이 자리에 넣으면 `vsync_due` 를 세우는 사람이 없어
        // `drive_present_clock` 이 `vsync_stalled` 로 떨어진다 -- 표출 클럭이 깨진다.
        // 판정을 `FrameClock` 에 두고 테스트를 붙였다(`only_vsync_drives_the_shell_present_clock`).
        // 기존 코드의 `let shell_vsync_driver = vsync_driver.clone();` 가 정확히 그 사고였다.
        #[cfg(target_os = "windows")]
        let shell_vsync_driver: Option<Rc<dyn servo::RefreshDriver>> =
            clock.drives_shell_present_clock().then(|| {
                Rc::new(vsync_refresh_driver::DwmVsyncRefreshDriver::new())
                    as Rc<dyn servo::RefreshDriver>
            });
        // 비-Windows 에는 `DwmVsyncRefreshDriver` 가 없다. `choose_frame_clock` 이 거기서
        // `Timer` 만 돌려주므로 이 값은 언제나 `None` 이다.
        #[cfg(not(target_os = "windows"))]
        let shell_vsync_driver: Option<Rc<dyn servo::RefreshDriver>> = None;

        // ★두 값이 나오는 이유.★ 드라이버는 Servo 가 가져가고(프레임 클럭), 손잡이는
        // 표출 틱이 그것을 쏘는 데 쓴다. Servo 생성자가 드라이버를 가져가 버리므로
        // 손잡이는 여기서 떠 둬야 한다.
        let (frame_driver, tick_fire) = frame_callback_wiring(clock);
        let Some(tick_fire) = tick_fire else {
            // 도달하지 않는다(`every_clock_has_a_handle_the_tick_can_fire`). 그래도
            // 조용히 지나가지 않는 이유는, 손잡이가 없으면 아무도 콜백을 쏘지 않아
            // Servo 가 `waiting_for_frame=true` 로 영원히 멈추기 때문이다 -- 까만 벽이
            // 24 시간 서 있는 것보다 기동에 죽는 편이 낫다.
            panic!("프레임 클럭 {clock:?} 에 틱 손잡이가 없다 -- 아무도 콜백을 쏘지 않는다");
        };

        // `gfx_wall_parallel_tiles`: 타일마다 자기 스레드를 준다. ★그 타일들은 셸이
        // 컨텍스트를 만들지 않는다★ — 스레드가 만들고, current 로 삼고, 표출까지 한다.
        // 타일 0 만은 예외다: Servo 자체가 그 컨텍스트 위에 세워지므로 셸이 소유한다.
        let parallel_tiles = servo::pref!(gfx_wall_parallel_tiles);
        let mut tile_factories: Vec<Option<servo::RenderingContextFactory>> = Vec::new();

        // Create one window (and, unless the tile gets its own thread, a rendering context).
        let tiles = if parallel_tiles {
            let plans = tile::plan_tile_windows_pub(
                event_loop,
                &config.layout,
                &tile_indices,
                &spatial,
                have_topology,
            );
            let mut tiles = Vec::with_capacity(plans.len());
            for (slot, plan) in plans.into_iter().enumerate() {
                if slot == 0 {
                    tiles.push(tile::bind_context_pub(
                        plan,
                        display_handle,
                        frame_driver.clone(),
                    ));
                    tile_factories.push(None);
                    continue;
                }
                let (tile, factory) = tile::split_plan_for_own_thread(plan);
                tiles.push(tile);
                tile_factories.push(Some(factory));
            }
            tiles
        } else {
            tile::create_tile_windows(
                event_loop,
                display_handle,
                &config.layout,
                &tile_indices,
                &spatial,
                have_topology,
                frame_driver,
            )
        };

        // 2) Build the shared Servo instance against tile 0's (primary) context.
        // ★타일 0(primary)은 언제나 셸이 컨텍스트를 소유한다★ — Servo 자체가 그 위에
        // 세워지므로 스레드로 보낼 수 없다. 병렬 경로에서도 나머지 타일만 스레드를 갖는다.
        let primary_context = tiles[0]
            .rendering_context
            .clone()
            .expect("the primary tile always owns its context on the shell side");
        let _ = primary_context.make_current();
        // 이 셸은 지금까지 Opts 를 넘기지 않아 항상 기본값이 쓰였다. 바꾸는 것은
        // `--ignore-certificate-errors` 하나뿐이고 나머지는 그대로 기본값이다
        // (`ServoBuilder` 가 opts 미지정 시 쓰던 값과 동일하다).
        let opts = Opts {
            ignore_certificate_errors: config.ignore_certificate_errors,
            ..Default::default()
        };
        let ignore_certificate_errors = opts.ignore_certificate_errors;
        let servo = ServoBuilder::default()
            .opts(opts)
            .event_loop_waker(Box::new(waker.clone()))
            .preferences(std::mem::take(&mut config.preferences))
            .build();
        servo.setup_logging();
        // ***After `setup_logging()`, not before.*** This used to sit above the builder,
        // where env_logger does not exist yet, so the line was swallowed and a run that
        // trusted every certificate left no trace of it in its own log. Verified 2026-08-31:
        // the flag was applied and the warning was nowhere in the captured stderr.
        if ignore_certificate_errors {
            log::warn!("--ignore-certificate-errors: accepting ALL TLS certificate errors");
        }
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
            device_auth: Rc::default(),
            device_auth_scope: sdwall_device_auth::PageScope::from_start_url(&config.url),
            device_auth_selftest_next: Rc::default(),
            device_auth_selftest_exit_code: Cell::new(None),
            device_auth_selftest: config.device_auth_selftest.then(|| {
                Rc::new(RefCell::new(device_auth_selftest::DeviceAuthSelfTest::new(
                    std::time::Instant::now(),
                )))
            }),
            tiles,
            direct_render: servo_config::pref!(gfx_wall_direct_render),
            present_period: {
                // Same knob and the same clamp the paint timer uses; one refresh rate for the
                // machine, not two that can disagree.
                let raw_hz = servo_config::pref!(gfx_refresh_hz);
                let hz = if (1..=1000).contains(&raw_hz) {
                    raw_hz
                } else {
                    120
                };
                std::time::Duration::from_secs_f64(1.0 / hz as f64)
            },
            next_present_tick: Cell::new(std::time::Instant::now()),
            last_pacing_tick_at: Cell::new(None),
            present_vsync: shell_vsync_driver,
            tick_fire,
            frame_clock: clock,
            vsync_due: Arc::new(AtomicBool::new(false)),
            vsync_armed: Cell::new(false),
            vsync_stalled: Cell::new(false),
            vsync_phase: {
                // vsync 주기는 미리 알 수 없으므로 `gfx_refresh_hz` 의 주기를 자로 쓴다.
                // 60Hz 디스플레이에 `-RefreshHz 60` 이면 둘이 같으니 백분율이 곧 위상이다.
                let pct = servo_config::pref!(gfx_vsync_phase_pct).clamp(0, 99) as u32;
                let period = {
                    let raw_hz = servo_config::pref!(gfx_refresh_hz);
                    let hz = if (1..=1000).contains(&raw_hz) { raw_hz } else { 120 };
                    std::time::Duration::from_secs_f64(1.0 / hz as f64)
                };
                period * pct / 100
            },
            vsync_tick_pending: Cell::new(false),
            vsync_tick_at: Cell::new(None),
            vsync_waker: waker.clone(),
            last_present_tick_at: Cell::new(std::time::Instant::now()),
            capture_deadline: config.capture.as_ref().map(|_| {
                std::time::Instant::now() + std::time::Duration::from_secs_f64(config.capture_sec)
            }),
            capture_path: config.capture.clone(),
            captured: Cell::new(false),
            pass_counter: Cell::new(0),
            wake_pending: waker.pending.clone(),
            should_exit: Cell::new(false),
        });

        // 3) One logical WebView whose layout viewport is the whole virtual viewport.
        // ★URL 을 여기서 주지 않는다.★ 주면 그 순간 로드가 시작되고, 페이지가 캔버스를
        // 만드는 시점이 아래의 타일 등록과 경합한다. WebGL 팬아웃 대상은 컨텍스트 생성
        // 시점에 고정되므로(`WebGLMsg::CreateContext` 가 painter 목록을 받는다), 아직
        // 등록되지 않은 타일은 그 캔버스를 영영 못 받는다 — 그 타일은 캔버스만 빠진 채로
        // 그려진다. 2026-09-03 `log_webgpu/64` 에서 정확히 그 모양이 나왔다: 격리 WebGL
        // 디바이스가 4 개가 아니라 1 개만 생기고, painter 2~4 는 "missing swap chain".
        //
        // 직렬 경로에서도 같은 경합이 있었고 창이 좁아 안 걸렸을 뿐이다. 타일을 전부
        // 세운 뒤에 로드를 시작하면 두 경로 모두에서 사라진다.
        let webview = WebViewBuilder::new(&app_state.servo, primary_context)
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
            // 이 타일이 자기 스레드를 받기로 되어 있으면 factory 로 등록한다 — 컨텍스트는
            // 그 스레드가 만든다. 아니면 셸이 이미 만들어 둔 컨텍스트를 넘긴다.
            let target = match tile_factories.get_mut(slot).and_then(Option::take) {
                Some(factory) => webview
                    .add_paint_target_on_own_thread(factory, viewport_details, origin)
                    .expect("Could not give tile its own painter thread"),
                None => {
                    let rendering_context = tile
                        .rendering_context
                        .clone()
                        .expect("inline tiles own their context on the shell side");
                    webview.add_paint_target(rendering_context, viewport_details, origin)
                },
            };
            tile.paint_target.set(Some(target));
        }

        // 모든 타일이 등록된 뒤에 로드를 시작한다(위 주석 참고).
        webview.load(config.url.clone());

        *app_state.webview.borrow_mut() = Some(webview);

        // ★박자를 이벤트 루프에서 떼는 경로.★ 타일과 webview 가 다 준비된 뒤에 띄운다 --
        // 더 일찍 띄우면 아직 `Running` 이 아닌 상태로 틱이 올라와 버려진다.
        if servo_config::pref!(gfx_wall_pacing_thread) {
            eprintln!("wall: gfx_wall_pacing_thread=true: 표출 틱을 전용 스레드가 올린다.");
            spawn_pacing_thread(waker.proxy.clone(), app_state.present_period);
        }

        if sdwall_device_auth::SIGNING_KEY_KIND != "release" {
            log::warn!(
                "device-auth: built with the {} signing key -- not for deployment",
                sdwall_device_auth::SIGNING_KEY_KIND
            );
        }
        // 시작 URL 이 불투명 오리진이면 폴러는 돌아도 어디에도 넘기지 않는다 -- 왜 값이 안
        // 오는지 현장에서 알 수 있게 시작할 때 한 번 남긴다.
        if app_state.device_auth_scope.is_none() {
            log::warn!(
                "device-auth: start URL has an opaque origin; device auth will not be delivered to any page"
            );
        }
        let device_auth_proxy = waker.proxy.clone();
        sdwall_device_auth::spawn_poller(move |auth| {
            let _ = device_auth_proxy.send_event(WakerEvent::DeviceAuth(auth));
        });

        *self = Self::Running(app_state);
    }

    fn user_event(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop, event: WakerEvent) {
        if let Self::Running(state) = self {
            // 장비 인증은 표출 박자와 상관없다 -- 세지도, 렌더를 끌지도 않는다.
            if let WakerEvent::DeviceAuth(auth) = &event {
                state.on_device_auth_state(auth.clone());
                return;
            }
            // 페이싱 스레드가 올린 틱과 엔진 wake 를 구분한다 -- `note_pacing_tick` 이
            // 페이싱 래치를 세울지 말지가 여기서 갈린다.
            let site = match event {
                WakerEvent::Tick => ClockSite::PacingThread,
                WakerEvent::Engine => ClockSite::UserEvent,
                WakerEvent::DeviceAuth(_) => unreachable!("handled above"),
            };
            // ★깨우기가 합쳐졌음을 먼저 표시한다★ -- 비우기 전에. 이 드레인이 도는 동안
            // 들어온 깨우기는 새 이벤트를 올려야 하고, 순서를 뒤집으면 그것을 잃는다.
            //
            // 단 **엔진 wake 일 때만** 내린다. 페이싱 틱이 내리면 아직 소비되지 않은 엔진
            // 깨우기를 소비한 것처럼 표시해, 합침이 무의미해진다(같은 wake 가 두 번 올라온다).
            if matches!(event, WakerEvent::Engine) {
                state.wake_pending.store(false, Ordering::SeqCst);
            }
            state.servo.spin_event_loop();
            // ★페이싱이 살아 있으면 페이싱만이 박자를 끈다.★
            //
            // `drive_present_clock_timer` 의 게이트는 `now >= next` 하나이고 ***호출자를
            // 보지 않는다.*** 그래서 엔진 wake 가 격자점을 조금이라도 지나서 도착하면 그
            // 자리에서 렌더가 나가고, 제시간에 깬 페이싱 틱은 이미 지나간 격자점을 보고
            // 아무것도 하지 않는다. 발화 시각이 ***격자점 + 엔진 wake 가 올 때까지의 랜덤
            // 지연***이 되는데, 이 셸은 엔진 wake 를 초당 300 회 받으므로(그 주석은
            // `fire_present_tick` 에 있다) 격자점 직후 아무 때나 하나가 온다.
            //
            // ★같은 버그를 페인트 쪽에서는 이미 고쳤다.★ `pump_paint_animation` 주석:
            // "타이머 폴백은 '아무도 렌더하지 않을 때' 전용이다. 렌더가 돌고 있는데도 이
            // disjunct 가 같은 주기로 같이 돌면 ***두 시계가 맥놀이한다.***" 셸의 박자에만
            // 그 수정이 빠져 있었다.
            //
            // 실측(log_ani_debug_04/07_temp, 08_temp): 엔진 wake 가 끈 틱이 초당
            // 0.02 -> 1.76 -> 3.38 -> 4.18 -> 6.65 로 가는 동안 저더가 재발했고, 같은
            // 방향으로 틱 지터의 p50 이 0.41 -> 0.47ms 로, 프레임당 이동 `dx_px` 의 창별
            // max 가 38.4(기대값) 근처에서 41.6 까지 올랐다가 가끔 제자리로 돌아오는
            // ***드리프트·리셋***을 보였다. 프레임을 놓친 흔적은 없다(건너뛴 간격 0,
            // 렌더 초당 60) -- 늦은 프레임이 아니라 ***고르지 않은 간격***이다.
            //
            // 페이싱이 이르게 깨느냐 늦게 깨느냐는 경계에 걸린 양자택일이라 환경 변화로
            // 뒤집힌다. 그래서 상관이 아니라 ***구조***로 막는다.
            //
            // `spin_event_loop` 은 그대로 돈다 -- 엔진 메시지 처리는 아무것도 바뀌지
            // 않는다. 바뀌는 것은 ***누가 렌더를 끄나*** 뿐이다.
            match event {
                // 페이싱 틱은 언제나 박자를 끈다.
                WakerEvent::Tick => {
                    state.drive_present_clock(site);
                },
                // 엔진 wake 는 ★페이싱이 멎었을 때만★ 받는다. 큐가 비지 않아도 박자는
                // 흘러야 한다는 원래 요구는 이 백스톱이 그대로 만족시킨다.
                WakerEvent::Engine if !state.pacing_is_driving() => {
                    state.drive_present_clock(site);
                },
                WakerEvent::Engine => {},
                WakerEvent::DeviceAuth(_) => unreachable!("handled above"),
            }
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
        if let Some(script) = state.device_auth_selftest_next.take() {
            if let Some(webview) = state.webview.borrow().clone() {
                log::warn!("device-auth selftest: next step");
                webview.evaluate_javascript(script, |_| {});
            }
        }
        if state.device_auth_selftest_finished() {
            event_loop.exit();
            return;
        }
        // While a `--capture` is pending, keep polling + redrawing so the capture deadline
        // fires even on a static page that has otherwise gone idle.
        if state.capture_path.is_some() && !state.captured.get() {
            event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
            if let Some(tile) = state.tiles.first() {
                tile.window.request_redraw();
            }
            return;
        }

        // ★여기서는 박자를 내지 않는다.★ `about_to_wait` 는 이벤트 큐가 **다 빌 때만**
        // 닿으므로, 엔진이 바쁘면 박자가 그 뒤에 선다 -- 2026-09-28 계측에서 최악 3.2s 까지
        // 밀렸다. 틱의 권한은 `-PacingThread` 가 보내는 `WakerEvent::Tick` 에 있고, 그것은
        // `user_event` 에서 `drive_present_clock(ClockSite::PacingThread)` 로 들어온다.
        //
        // ★`-PacingThread` 없이 돌리면 박자를 낼 사람이 엔진 wake(`WakerEvent::Engine`)와
        // 윈도우 이벤트뿐이다.★ 애니메이션이 도는 동안은 painter 가 매 펌프마다 깨우므로
        // 끊기지는 않지만, 그것은 보장이 아니라 부작용이다.
        //
        // ★기다리지 않고 돈다.★ `WaitUntil(next_present_tick)` 은 타이머가 만료될 때까지
        // 루프를 재운다. 그러면 페이싱 스레드가 그 사이에 올린 `WakerEvent::Tick` 이
        // **깨우기 신호로 소비되고 나서야** 처리되고, 그 왕복(프록시 -> PostMessage ->
        // 메시지 큐 -> user_event)이 박자에 그대로 얹힌다.
        //
        // `Poll` 이면 루프가 자지 않으므로 `Tick` 이 올라온 즉시 다음 바퀴에서 잡힌다.
        // ★대가는 메인 스레드가 코어 하나를 계속 태우는 것이다★ -- 이 실험에서 박자가
        // 개선되지 않으면 되돌려야 하는 이유가 그것이다. 이 셸은 이미 `--capture` 대기
        // 중에 같은 방식으로 돈다(바로 위 분기).
        // ★마지막 보루.★ 위 문단이 "여기서는 박자를 내지 않는다" 고 못박은 것은 페이싱이
        // **돌고 있을 때** 의 이야기다. `FrameClock::Pacing` 을 골랐는데 페이싱 스레드가
        // 뜨지 못하면 `WakerEvent::Tick` 이 영영 오지 않고, 그 구성에는 주기적 엔진 wake 도
        // 없다 -- `needs_pacing_backstop` 의 주석이 왜인지 적는다. 그러면 clock 을 돌릴
        // 사람이 하나도 없어 화면이 멈춘다.
        //
        // `Poll` 이라 이 함수는 계속 불리므로 폴백으로 충분하다. 페이싱이 정상이면
        // `pacing_is_driving()` 이 참이라 끼어들지 않는다 -- ★그 판정이 읽는 래치를 세우는
        // 것은 `note_pacing_tick` 하나뿐이다★(`ClockSite::PacingThread` 일 때만).
        if needs_pacing_backstop(
            matches!(state.frame_clock, FrameClock::Pacing),
            state.pacing_is_driving(),
        ) {
            state.drive_present_clock(ClockSite::AboutToWait);
        }

        event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
    }

    fn window_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        if let Self::Running(state) = self {
            state.servo.spin_event_loop();
            state.drive_present_clock(ClockSite::WindowEvent);
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
struct Waker {
    proxy: winit::event_loop::EventLoopProxy<WakerEvent>,
    /// `AppState::wake_pending` 와 같은 플래그. 여기 올린 이벤트가 아직 소비되지
    /// 않았으면 다시 올리지 않는다.
    pending: Arc<AtomicBool>,
}
/// 이벤트 루프에 올리는 두 종류.
///
/// ★엔진 wake 와 표출 틱을 구분한다.★ 하나로 묶으면 "타이머가 깨웠나 엔진이 깨웠나" 를
/// 더 이상 가릴 수 없고, `user_event` 의 페이싱 우선 판정도 설 자리가 없다.
#[derive(Debug)]
enum WakerEvent {
    /// 엔진 스레드가 메인에 처리할 일이 있다고 알린다.
    Engine,
    /// 페이싱 스레드가 표출 틱 시각을 알린다.
    Tick,
    /// 장비 인증 폴러가 바뀐 결과를 알린다.
    DeviceAuth(sdwall_device_auth::AuthState),
}

impl Waker {
    fn new(event_loop: &EventLoop<WakerEvent>) -> Self {
        Self {
            proxy: event_loop.create_proxy(),
            pending: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl embedder_traits::EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn embedder_traits::EventLoopWaker> {
        // ★복제본도 같은 플래그를 공유한다★ -- 엔진은 waker 를 복제해 여러 스레드에
        // 나눠 갖는다. 복제마다 플래그가 따로면 합쳐지는 것이 스레드 하나뿐이다.
        Box::new(Self {
            proxy: self.proxy.clone(),
            pending: self.pending.clone(),
        })
    }

    fn wake(&self) {
        // ★큐에 이미 소비되지 않은 깨우기가 있으면 올리지 않는다.★ 드레인은 한 번이면
        // 전부 비우므로 두 번째부터는 아무 일도 하지 않는데, 그 빈 이벤트들이 큐를
        // 비지 않게 만들어 `about_to_wait` 을 굶긴다 -- 그리고 표출 클럭이 거기 있다.
        // 자세한 것은 `AppState::wake_pending`.
        if self.pending.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(error) = self.proxy.send_event(WakerEvent::Engine) {
            // 올리지 못했으면 표시도 되돌린다 -- 아니면 이후의 모든 깨우기가 막힌다.
            self.pending.store(false, Ordering::SeqCst);
            eprintln!("warning: failed to wake event loop: {error:?}");
        }
    }
}

/// `gfx_vsync_enabled` / `gfx_wall_pacing_thread` 조합에서 어느 프레임 클럭을 쓸까.
///
/// ★순수 함수로 뗀 이유.★ 실제 선택은 창·컨텍스트가 다 선 뒤 `resumed` 한가운데서
/// 일어나 테스트에서 부를 수 없다. 틀리면 화면이 멈추는 조합이 하나 있으므로(`Timer` 의
/// 주석) 그 판정만 따로 재현 가능하게 둔다.
///
/// ★이름이 가리키는 것은 틱 에지의 출처다 -- 콜백을 쏘는 주체가 아니다.★ 2026-10-21
/// 부터 세 구성 모두 표출 틱이 콜백을 쏜다(`frame_callback_wiring`).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum FrameClock {
    /// `gfx_vsync_enabled` -- DwmFlush 스레드가 틱 에지를 낸다. ★셸의 표출 클럭도 이것을
    /// 탄다★(`AppState::present_vsync`).
    Vsync,
    /// 페이싱 스레드가 틱 에지를 낸다(`WakerEvent::Tick`). 셸의 표출 클럭은
    /// ***타이머 경로 그대로*** 다.
    Pacing,
    /// `PaintTimerThread` 가 틱 에지를 낸다 -- ★깨우기만★ 이고 콜백은 쏘지 않는다
    /// (`TickRefreshDriver::driven_by_paint_timer`). 이 구성에는 주기적으로 이벤트 루프를
    /// 깨울 것이 그것 말고 없다.
    Timer,
}

impl FrameClock {
    /// 셸의 표출 클럭(`AppState::present_vsync` -> `pump_vsync`)이 이 드라이버를 타는가.
    ///
    /// ★`Vsync` 만 참이다.★ 페이싱 드라이버를 그 자리에 넣으면 `vsync_due` 를 세우는 사람이
    /// 없어 `vsync_stalled` 로 떨어진다 -- 표출 클럭이 깨진다.
    fn drives_shell_present_clock(self) -> bool {
        matches!(self, FrameClock::Vsync)
    }
}

/// `FrameClock::Pacing` 을 골랐는데 페이싱이 돌지 않을 때, `about_to_wait` 이 대신 박자를
/// 내야 하는가.
///
/// ★`FrameClock::Pacing` 에는 주기적 엔진 wake 가 없다.★ `PaintTimerThread` 는 여전히
/// 뜨지만(아래 배너 주석) 프레임 경로에서 빠졌고, 그 타이머에 남은 일은 caret 깜빡임뿐이라
/// 편집 caret 이 없는 벽 콘텐츠에서는 아무것도 걸지 않는다. ★`FrameClock::Timer` 와
/// 갈리는 자리가 여기다★ -- 그쪽은 같은 타이머에게 깨우기를 계속 받으므로
/// (`TickRefreshDriver::driven_by_paint_timer`) 이 보루가 필요 없다. 페이싱에는 그 요청을
/// 걸지 않는다: 걸면 없애려던 두 번째 시계가 깨우기로 돌아온다.
///
/// 그래서 `spawn_pacing_thread` 가 실패하면 ***`WakerEvent::Tick` 도 주기적 `Engine` 도
/// 오지 않아 clock 을 돌릴 사람이 하나도 없다*** -- 화면이 영구히 멈추고 `about_to_wait`
/// 이 `Poll` 로 코어만 태운다. 변경 전에는 그 타이머가 저하된 박자라도 냈다.
///
/// 선택은 `spawn_pacing_thread` 보다 260 줄 앞서 일어나므로 그 성패를 볼 수 없다. 그래서
/// 런타임에 판정한다.
fn needs_pacing_backstop(pacing_clock: bool, pacing_alive: bool) -> bool {
    pacing_clock && !pacing_alive
}

/// 이 프레임 클럭에서 Servo 에 설치할 프레임 드라이버와, 표출 틱이 그것을 쏘는 손잡이.
///
/// ★여기서 정하는 것은 "콜백을 누가 쏘나" 뿐이다.★ 세 구성 모두 틱이 쏜다. 틱 에지가
/// 언제 생기는가는 구성마다 다르고(페이싱 스레드 / `PaintTimerThread` / DwmFlush 스레드)
/// 이 함수가 건드리지 않는다.
///
/// ★스레드를 띄우지 않으므로 테스트에서 그대로 부를 수 있다.★ 셸의 표출 클럭이 타는
/// vsync 드라이버는 여기서 만들지 않는다 -- 그것은 틱 에지 쪽이고 DwmFlush 스레드를 띄운다.
///
/// 손잡이를 `Option` 으로 돌려준다. ★세 구성 모두 `Some` 이고 그것이 요점이다★ --
/// 구성을 하나 더하면서 손잡이를 빼먹는 것은 컴파일러가 잡지 못하고
/// `every_clock_has_a_handle_the_tick_can_fire` 가 잡는다.
fn frame_callback_wiring(
    clock: FrameClock,
) -> (
    Option<Rc<dyn servo::RefreshDriver>>,
    Option<tick_refresh_driver::TickFireHandle>,
) {
    let driver = match clock {
        // 틱 에지를 DwmFlush 스레드(`AppState::present_vsync`)와 페이싱 스레드가 낸다.
        FrameClock::Vsync | FrameClock::Pacing => tick_refresh_driver::TickRefreshDriver::new(),
        // ★이 구성에는 주기적으로 이벤트 루프를 깨울 것이 `PaintTimerThread` 말고 없다.★
        // 그 타이머가 쏘던 콜백을 틱으로 옮겼으므로, 깨우기만 계속 내 달라고 알린다 --
        // `TickRefreshDriver::driven_by_paint_timer` 가 왜인지 적는다.
        FrameClock::Timer => tick_refresh_driver::TickRefreshDriver::driven_by_paint_timer(),
    };
    let handle = driver.handle();
    (Some(Rc::new(driver)), Some(handle))
}

fn choose_frame_clock(vsync_enabled: bool, pacing_thread: bool) -> FrameClock {
    if vsync_enabled {
        FrameClock::Vsync
    } else if pacing_thread {
        FrameClock::Pacing
    } else {
        // ★페이싱이 없으면 `Pacing` 을 고르면 안 된다.★ 아무도 발사하지 않아 Servo 가
        // 영원히 `waiting_for_frame=true` 로 멈춘다.
        FrameClock::Timer
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// ★세 구성 모두에서 표출 틱이 refresh-driver 콜백을 쏜다.★ 한 구성이라도 자기
    /// 스레드가 쏘면 "콜백 → 생산 → 렌더" 가 한 틱 안의 순서가 아니라 ***두 시계의
    /// 경합*** 이 된다 -- 설계 문서 불변식 9.
    ///
    /// 손잡이가 있는 것만으로는 모자라서, 그 손잡이가 ***그 드라이버***를 쏘는지까지 본다.
    /// 틱 전에는 아무도 쏘지 않아야 한다 -- 쏘면 발사 주체가 다시 둘이다.
    #[test]
    fn every_clock_has_a_handle_the_tick_can_fire() {
        for clock in [FrameClock::Pacing, FrameClock::Timer, FrameClock::Vsync] {
            let (driver, fire) = frame_callback_wiring(clock);
            let Some(fire) = fire else {
                panic!("{clock:?} 에 손잡이가 없다");
            };
            let Some(driver) = driver else {
                panic!("{clock:?} 에 틱이 쏠 드라이버가 없다");
            };

            let fired = Arc::new(AtomicUsize::new(0));
            let hits = fired.clone();
            driver.observe_next_frame(Box::new(move || {
                hits.fetch_add(1, Ordering::SeqCst);
            }));
            assert_eq!(
                fired.load(Ordering::SeqCst),
                0,
                "{clock:?}: 틱 전에 쏘면 안 된다"
            );

            fire.fire();
            assert_eq!(fired.load(Ordering::SeqCst), 1, "{clock:?}: 틱이 쏴야 한다");
        }
    }

    #[test]
    fn load_status_maps_to_document_event() {
        use sdwall_device_auth::DocumentEvent;
        assert_eq!(
            document_event(::servo::LoadStatus::Started),
            DocumentEvent::Started
        );
        assert_eq!(
            document_event(::servo::LoadStatus::HeadParsed),
            DocumentEvent::HeadParsed
        );
        assert_eq!(
            document_event(::servo::LoadStatus::Complete),
            DocumentEvent::Complete
        );
    }

    /// ★vsync 가 켜져 있으면 그것이 이긴다.★ 이미 있는 동작이고 바뀌면 안 된다.
    #[test]
    fn vsync_wins_over_pacing() {
        assert_eq!(choose_frame_clock(true, true), FrameClock::Vsync);
        assert_eq!(choose_frame_clock(true, false), FrameClock::Vsync);
    }

    /// 페이싱이 돌고 vsync 가 꺼져 있을 때만 페이싱 드라이버를 쓴다.
    #[test]
    fn pacing_only_when_the_thread_runs() {
        assert_eq!(choose_frame_clock(false, true), FrameClock::Pacing);
    }

    /// ★페이싱이 없으면 기본 타이머로 떨어진다.★ 이 조합에서 `Pacing` 을 고르면 아무도
    /// 발사하지 않아 화면이 영원히 멈춘다.
    #[test]
    fn no_pacing_thread_falls_back_to_timer() {
        assert_eq!(choose_frame_clock(false, false), FrameClock::Timer);
    }

    /// ★셸의 표출 클럭은 vsync 일 때만 드라이버를 탄다.★ 페이싱 드라이버가 그 자리
    /// (`AppState::present_vsync`)로 새면 `vsync_due` 를 세우는 사람이 없어
    /// `drive_present_clock` 이 `vsync_stalled` 로 떨어진다 -- 표출 클럭이 깨진다.
    /// 기존 코드의 `let shell_vsync_driver = vsync_driver.clone();` 가 정확히 그 사고였다.
    #[test]
    fn only_vsync_drives_the_shell_present_clock() {
        assert!(FrameClock::Vsync.drives_shell_present_clock());
        assert!(!FrameClock::Pacing.drives_shell_present_clock());
        assert!(!FrameClock::Timer.drives_shell_present_clock());
    }

    /// ★페이싱이 멎으면 누군가 대신 박자를 내야 한다.★ `FrameClock::Pacing` 에서는
    /// `PaintTimerThread` 가 프레임 경로에서 빠지므로 주기적 엔진 wake 가 없다 -- 그 타이머는
    /// caret 깜빡임에만 쓰이고, 편집 caret 이 없는 벽 콘텐츠에서는 아무것도 걸지 않는다.
    /// 그래서 `spawn_pacing_thread` 가 실패하면 ***아무도 clock 을 돌리지 않아 화면이
    /// 영구히 멈춘다.*** 변경 전에는 그 타이머가 저하된 박자라도 냈다.
    ///
    /// ★이 테스트가 덮지 **않는** 것:★ `pacing_is_driving()` 과 그것이 읽는
    /// `last_pacing_tick_at` 래치다. 여기 있는 것은 불리언 둘을 받는 자유 함수뿐이라
    /// `AppState` 를 만들지 않는다. `note_pacing_tick` 의 `.set(..)` 을 떨구면 이 테스트는
    /// 그대로 초록인 채 백스톱이 매 엔진 wake 마다 열린다 -- 그쪽은 눈으로 지켜야 한다.
    #[test]
    fn backstop_only_when_pacing_was_installed_but_is_not_running() {
        // 페이싱이 돌고 있으면 백스톱은 끼어들지 않는다 -- 두 시계로 되돌아간다.
        assert!(!needs_pacing_backstop(true, true));
        // ★페이싱 구성인데 돌지 않는다 = 아무도 박자를 내지 않는다.★
        assert!(needs_pacing_backstop(true, false));
        // 나머지 두 구성은 틱 에지를 낼 스레드가 따로 있다 -- `Timer` 는
        // `PaintTimerThread` 의 깨우기, `Vsync` 는 DwmFlush 스레드.
        assert!(!needs_pacing_backstop(false, false));
        assert!(!needs_pacing_backstop(false, true));
    }
}

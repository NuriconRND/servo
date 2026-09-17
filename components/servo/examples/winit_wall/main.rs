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

mod crash_report;
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
    /// `--tile-thread-spike`: 병렬 타일 렌더(A 안) 1 단계의 관문만 시험하고 종료한다.
    /// 자세한 것은 [`tile::run_tile_thread_spike`].
    tile_thread_spike: bool,
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
    let mut tile_thread_spike = false;

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
            "--tile-thread-spike" => tile_thread_spike = true,
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
        tile_thread_spike,
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
    // 스파이크는 판정을 종료 코드로도 낸다(FAIL = 1). 일반 실행은 여기 걸리지 않는다.
    if let App::SpikeFinished { passed: false } = app {
        std::process::exit(1);
    }
    Ok(())
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
    // ***Is the serial tile loop actually costing us anything?*** `render_all_tiles` paints
    // and presents each tile one after another on this thread, so a wall frame can never be
    // faster than the SUM of the tiles. Parallelising would bring that down to the MAX, so
    // the whole prize is sum/max -- and if the four GPUs already overlap their work, even
    // that is optimistic. Accumulated per pass and reported once a second so the ratio can be
    // read off instead of argued about. Enabled by SERVO_LOG_PRESENT_CADENCE.
    pass_stats: RefCell<PassStats>,
    /// Presentation clock period, from `gfx_refresh_hz`.
    ///
    /// ***The wall draws on a clock, not when content says it is ready.*** Everything that
    /// produces pixels -- video decode, a WebGL rAF loop, script animation -- updates its own
    /// state whenever it likes; the clock decides when what exists gets drawn. That is the
    /// model a display wants, and it is not what this shell used to do.
    present_period: std::time::Duration,
    /// When the next presentation tick is due.
    next_present_tick: Cell<std::time::Instant>,
    capture_path: Option<String>,
    capture_deadline: Option<std::time::Instant>,
    /// `gfx_wall_rotate_tile_order` 용 패스 카운터 — 시작 타일을 한 칸씩 돌린다.
    pass_counter: Cell<u64>,
    /// 메인 스레드의 1초를 어디에 썼는지.
    ///
    /// ★기계에 코어가 남아도는데 애니메이션이 죽는다면, 모자란 것은 처리량이 아니라 이
    /// 한 스레드다.★ 벽의 렌더·표출·엔진 메시지 처리가 전부 여기서 직렬로 돌기 때문에,
    /// 이 1초를 쪼개지 않으면 전환 구간에 패스가 60회에서 2회로 떨어진 이유가 렌더가
    /// 길어서인지, `spin_event_loop` 이 붙잡아서인지, 아무도 그리라고 하지 않아서인지
    /// 구분할 수 없다 -- 셋은 고쳐야 할 곳이 완전히 다르다.
    ///
    /// 계측을 pref 뒤에 두지 않는다: 초당 한 줄이고, 이것이 없는 로그는 이 질문에
    /// 대해서는 다시 재야 하는 로그다.
    main_busy: RefCell<MainBusy>,
    /// 표출 클럭이 유일한 렌더 권한인지 재는 집계. [`ClockStats`] 참고.
    clock_stats: RefCell<ClockStats>,
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

/// 메인 스레드 한 창(1초)의 시간 배분.
#[derive(Default)]
struct MainBusy {
    window_start: Option<std::time::Instant>,
    spin_ms: f64,
    spin_ms_max: f64,
    spin_calls: u32,
    render_ms: f64,
    render_ms_max: f64,
    render_calls: u32,
}

/// 표출 클럭이 **유일한** 렌더 권한인지 판정하는 한 창(1초)의 집계.
///
/// ★셸은 이미 올바르다.★ `request_redraw()` 를 부르는 곳은 클럭 틱과 `--capture`
/// 둘뿐이고 `RedrawRequested` 는 `render_all_tiles()` 하나로 간다. 이 계수는 그것을
/// 실기에서 확인하려고 붙였고, 확인됐다 -- log_ani_debug/26 의 103 창 전부에서
/// `ticks=redraw=renders` 이고 간격은 p95 17.1ms(주기 16.7)에 `off=0%` 였다.
///
/// ★WRRATE 61~75 는 과잉 표출이 아니다.★ 이 계수를 붙인 이유가 그 숫자였는데,
/// WRRATE 는 present 가 아니라 `render_impl` 진입을 센다. 다섯 진입점 중 넷이
/// `Renderer::update()` 안에서 프레임버퍼에 `None` 을 넘기는 오프스크린 렌더다
/// (텍스처 캐시 플러시 -- 그 자리 주석이 "this render will not be presented" 라고
/// 적어 두었다). 실제로 표출되는 프레임은 타일당 60.0/s 로 클럭과 같다.
/// 근거 수치는 설계 문서 2026-09-17-single-present-clock-design.md §2 판정 절에 있다.
///
/// 계수는 남긴다 -- 클럭이 유일한 렌더 권한이라는 것을 다음 회귀 때 다시 재는
/// 비용이 이 일곱 필드보다 크다.
#[derive(Default)]
struct ClockStats {
    window_start: Option<std::time::Instant>,
    /// `drive_present_clock()` 이 틱을 발화한 횟수.
    ticks: u32,
    /// `WindowEvent::RedrawRequested` 진입 횟수. `ticks` 보다 크면 셸 밖에서 온다.
    redraw: u32,
    /// `render_all_tiles()` 진입 횟수. `redraw` 하나가 렌더 하나여야 한다.
    renders: u32,
    /// 클럭이 허가하지 않아 억제한 횟수. 2단계 전에는 항상 0 이다.
    suppressed: u32,
    /// `render_all_tiles()` 진입 간격(ms).
    ///
    /// ***평균이 아니라 분포를 낸다.*** 끊김은 정의상 꼬리에만 있고, 균일한 57fps 와
    /// "60,60,60,20,60" 은 평균이 같다. 이 표본이 성공 기준 1 그 자체다.
    gaps_ms: Vec<f64>,
    last_render_at: Option<std::time::Instant>,
}

/// 한 창에 담을 간격 표본의 상한.
///
/// 60Hz 에서 한 창은 60 개다. 스톨로 창이 길어져도 메모리가 늘지 않게 막는다 --
/// 실측된 최악의 정지가 3.2 초이므로(`MAINBUSY window_ms=3239`) 여유를 크게 둔다.
const MAX_CLOCK_GAP_SAMPLES: usize = 4096;

/// `charge_main` 이 시간을 적립할 칸.
#[derive(Clone, Copy)]
enum MainSlot {
    /// `Servo::spin_event_loop` -- 엔진에서 올라온 메시지 처리.
    Spin,
    /// `render_all_tiles` -- 타일 페인트 + 표출.
    Render,
}

/// One second of `render_all_tiles` timings.
#[derive(Default)]
struct PassStats {
    window_start: Option<std::time::Instant>,
    passes: u32,
    /// Wall clock of the whole loop, i.e. what a wall frame actually costs.
    pass_ms_sum: f64,
    pass_ms_max: f64,
    /// Per tile, so the slowest one is visible: it is the floor a parallel version would hit.
    tile_ms_sum: Vec<f64>,
    tile_ms_max: Vec<f64>,
    /// Tiles skipped by the keep-previous barrier, which shorten a pass for a different reason.
    skipped: u32,
    /// 타일 스텝을 셋으로 쪼갠 것. ★WALLPASS 와 **같은 창·같은 분모**여야 한다★ — 기존에는
    /// `render_ms`(샘플 평균)와 WALLPASS per-tile(창 평균)을 빼려다 음수가 나왔다. DComp on 이
    /// 패스당 +6.7ms 인데 그 중 타일 루프 **밖**은 1.5ms 뿐이라, 나머지 5.2ms 가 이 셋 중
    /// 어디인지가 지금의 질문이다(2026-09-03, `log_webgpu/45` gap_off/gap_on).
    make_current_ms_sum: f64,
    paint_ms_sum: f64,
    present_ms_sum: f64,
}

/// 한 패스에서 타일 스텝을 셋으로 나눈 합계(타일 전부를 더한 값).
#[derive(Default)]
struct PassSplit {
    make_current_ms: f64,
    paint_ms: f64,
    present_ms: f64,
}

impl AppState {
    fn note_render_pass(&self, pass_ms: f64, tile_ms: &[f64], skipped: u32, split: PassSplit) {
        // ***`string`, not `enabled`.*** This flag is `Kind::Str`, and `enabled` asserts the
        // flag is `Kind::Presence` -- calling it here panicked on startup. Same truthiness
        // test painter.rs uses for the same flag, and cached because this runs every pass.
        static LOG_PASS: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
            servo_config::debug_env::string(&servo_config::debug_env::LOG_PRESENT_CADENCE)
                .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        });
        if !*LOG_PASS {
            return;
        }
        let now = std::time::Instant::now();
        let mut stats = self.pass_stats.borrow_mut();
        if stats.tile_ms_sum.len() != tile_ms.len() {
            stats.tile_ms_sum = vec![0.0; tile_ms.len()];
            stats.tile_ms_max = vec![0.0; tile_ms.len()];
        }
        let start = *stats.window_start.get_or_insert(now);
        stats.passes += 1;
        stats.pass_ms_sum += pass_ms;
        stats.pass_ms_max = stats.pass_ms_max.max(pass_ms);
        stats.skipped += skipped;
        stats.make_current_ms_sum += split.make_current_ms;
        stats.paint_ms_sum += split.paint_ms;
        stats.present_ms_sum += split.present_ms;
        for (index, ms) in tile_ms.iter().enumerate() {
            stats.tile_ms_sum[index] += ms;
            stats.tile_ms_max[index] = stats.tile_ms_max[index].max(*ms);
        }
        if now.duration_since(start).as_secs_f64() < 1.0 {
            return;
        }

        let passes = stats.passes.max(1) as f64;
        let avg: Vec<f64> = stats.tile_ms_sum.iter().map(|ms| ms / passes).collect();
        let serial: f64 = avg.iter().sum();
        let slowest = avg.iter().cloned().fold(0.0_f64, f64::max);
        // The ceiling on parallelising this loop, stated so nobody has to guess: with four
        // tiles the hopeful number is 4x, but one heavy tile caps it far below that.
        //
        // ★`gfx_wall_parallel_tiles` 가 켜져 있으면 이 값은 "천장" 이 아니라 **아직 남은
        // 여지** 다★ — 그때는 `serial_sum` 이 겹쳐서 돈 시간의 합이므로 `pass_ms` 보다 크고,
        // `serial_sum / pass_ms` 가 **이미 얻은 병렬도**다. 둘의 차이가 남은 몫이다.
        let ceiling = if slowest > 0.0 { serial / slowest } else { 0.0 };
        log::info!(
            "WALLPASS passes/s={:.1} pass_ms avg={:.2} max={:.2} | per-tile avg=[{}]              max=[{}] | serial_sum={:.2} slowest={:.2} parallel_ceiling={:.2}x skipped={}",
            passes / now.duration_since(start).as_secs_f64(),
            stats.pass_ms_sum / passes,
            stats.pass_ms_max,
            avg.iter()
                .map(|ms| format!("{ms:.2}"))
                .collect::<Vec<_>>()
                .join(","),
            stats
                .tile_ms_max
                .iter()
                .map(|ms| format!("{ms:.2}"))
                .collect::<Vec<_>>()
                .join(","),
            serial,
            slowest,
            ceiling,
            stats.skipped,
        );
        // 같은 창, 같은 분모(패스 수)로 나눈 타일 스텝의 내역. `outside` 는 패스 전체에서 이
        // 셋을 뺀 것 = 지연 Commit 플러시와 루프 자체의 비용이다.
        let make_current = stats.make_current_ms_sum / passes;
        let paint = stats.paint_ms_sum / passes;
        let present = stats.present_ms_sum / passes;
        log::info!(
            "WALLSPLIT pass_ms={:.2} = make_current={:.2} + paint={:.2} + present={:.2} \
             + outside={:.2} (all per pass, {} tiles summed)",
            stats.pass_ms_sum / passes,
            make_current,
            paint,
            present,
            stats.pass_ms_sum / passes - make_current - paint - present,
            stats.tile_ms_sum.len(),
        );
        *stats = PassStats {
            window_start: Some(now),
            ..Default::default()
        };
    }

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

    /// 메인 스레드가 `body` 에서 보낸 시간을 한 칸에 적립한다.
    ///
    /// `body` 가 끝난 뒤에야 `main_busy` 를 빌린다 -- `body` 안에서 다시 이 함수가
    /// 불릴 수 있으므로(렌더 안에서 표출이 도는 식), 겹쳐 빌리면 패닉이다.
    fn charge_main<T>(&self, slot: MainSlot, body: impl FnOnce() -> T) -> T {
        let start = std::time::Instant::now();
        let value = body();
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        let mut busy = self.main_busy.borrow_mut();
        if busy.window_start.is_none() {
            busy.window_start = Some(start);
        }
        match slot {
            MainSlot::Spin => {
                busy.spin_ms += ms;
                busy.spin_ms_max = busy.spin_ms_max.max(ms);
                busy.spin_calls += 1;
            },
            MainSlot::Render => {
                busy.render_ms += ms;
                busy.render_ms_max = busy.render_ms_max.max(ms);
                busy.render_calls += 1;
            },
        }
        value
    }

    /// 클럭이 틱을 발화했다.
    fn note_present_tick(&self) {
        let mut stats = self.clock_stats.borrow_mut();
        stats
            .window_start
            .get_or_insert_with(std::time::Instant::now);
        stats.ticks += 1;
    }

    /// winit 이 `RedrawRequested` 를 전달했다.
    fn note_redraw_requested(&self) {
        let mut stats = self.clock_stats.borrow_mut();
        stats
            .window_start
            .get_or_insert_with(std::time::Instant::now);
        stats.redraw += 1;
    }

    /// 클럭이 허가하지 않아 그리지 않았다. 2단계 전에는 불리지 않는다.
    #[allow(dead_code, reason = "2단계 분기 1 에서만 쓴다")]
    fn note_redraw_suppressed(&self) {
        self.clock_stats.borrow_mut().suppressed += 1;
    }

    /// 한 패스를 그리기 시작했다. 직전 패스와의 간격을 함께 남긴다.
    fn note_clock_render_pass(&self) {
        let now = std::time::Instant::now();
        let mut stats = self.clock_stats.borrow_mut();
        stats.window_start.get_or_insert(now);
        stats.renders += 1;
        if let Some(last) = stats.last_render_at
            && stats.gaps_ms.len() < MAX_CLOCK_GAP_SAMPLES
        {
            let gap = now.duration_since(last).as_secs_f64() * 1000.0;
            stats.gaps_ms.push(gap);
        }
        stats.last_render_at = Some(now);
    }

    /// 창이 1 초를 넘으면 `WALLCLOCK` 한 줄을 내고 리셋한다.
    ///
    /// `last_render_at` 은 **리셋하지 않는다** -- 창 경계에서 간격 하나가 통째로
    /// 사라지면 그 자리가 정확히 측정에서 빠진다.
    fn report_clock_stats(&self) {
        let mut stats = self.clock_stats.borrow_mut();
        let Some(start) = stats.window_start else {
            return;
        };
        let window_ms = start.elapsed().as_secs_f64() * 1000.0;
        if window_ms < 1000.0 {
            return;
        }

        let period_ms = self.present_period.as_secs_f64() * 1000.0;
        let mut sorted = stats.gaps_ms.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pct = |p: f64| -> f64 {
            if sorted.is_empty() {
                return 0.0;
            }
            let index = ((sorted.len() as f64) * p) as usize;
            sorted[index.min(sorted.len() - 1)]
        };
        // ±20% 밖. p50/p95 만 보면 "대체로 괜찮은데 가끔 튐" 을 놓친다.
        let (low, high) = (period_ms * 0.8, period_ms * 1.2);
        let off = stats
            .gaps_ms
            .iter()
            .filter(|g| **g < low || **g > high)
            .count();
        let off_pct = if stats.gaps_ms.is_empty() {
            0.0
        } else {
            100.0 * off as f64 / stats.gaps_ms.len() as f64
        };

        log::info!(
            "WALLCLOCK window_ms={:.0} ticks={} redraw={} renders={} suppressed={} period_ms={:.1} gap_ms p50={:.1} p95={:.1} max={:.1} off={}({:.0}%) n={}",
            window_ms,
            stats.ticks,
            stats.redraw,
            stats.renders,
            stats.suppressed,
            period_ms,
            pct(0.50),
            pct(0.95),
            sorted.last().copied().unwrap_or(0.0),
            off,
            off_pct,
            stats.gaps_ms.len(),
        );

        let last_render_at = stats.last_render_at;
        *stats = ClockStats::default();
        stats.last_render_at = last_render_at;
    }

    /// 1초가 찼으면 메인 스레드의 시간 배분을 한 줄로 찍고 창을 비운다.
    ///
    /// `idle` 은 창 길이에서 적립분을 뺀 나머지, 즉 **표출 클럭을 기다리며 논 시간**이다.
    /// 이것이 큰데 패스가 적으면 붙잡힌 게 아니라 아무도 그리라고 하지 않은 것이고,
    /// 0 에 가까우면 이 스레드가 포화된 것이다. 둘은 고칠 곳이 다르다.
    fn report_main_busy(&self) {
        let mut busy = self.main_busy.borrow_mut();
        let Some(start) = busy.window_start else {
            return;
        };
        let window_ms = start.elapsed().as_secs_f64() * 1000.0;
        if window_ms < 1000.0 {
            return;
        }
        let accounted = busy.spin_ms + busy.render_ms;
        log::info!(
            "MAINBUSY window_ms={:.0} busy={:.1}% spin_ms={:.1} (n={} max={:.1}) render_ms={:.1} (n={} max={:.1}) idle_ms={:.1}",
            window_ms,
            100.0 * accounted / window_ms,
            busy.spin_ms,
            busy.spin_calls,
            busy.spin_ms_max,
            busy.render_ms,
            busy.render_calls,
            busy.render_ms_max,
            (window_ms - accounted).max(0.0),
        );
        *busy = MainBusy::default();
    }

    /// 표출 클럭. 한 틱마다 엔진이 지금 들고 있는 것을 그린다 -- 무엇이 바뀌었는지
    /// 따지지 않는 것이 핵심이다. 박자가 내용의 함수가 되면 그것은 균일하지도 않고,
    /// 내용 쪽 신호가 하나 빠졌을 때 스스로 회복하지도 못한다.
    ///
    /// ★`about_to_wait` 에만 두면 안 된다.★ winit 은 이벤트 큐가 **빌 때만** 거기에
    /// 도달한다. 전환 순간 엔진이 초당 수천 번 깨우면 큐가 비지 않고, 그러면 이 클럭이
    /// 통째로 멈춘다 -- 실측된 최악은 3.2초다(`MAINBUSY window_ms=3239`). 그동안 벽은
    /// 아무것도 새로 표출하지 않으므로, 화면은 애니메이션이 얼어붙은 것으로 보인다.
    /// 그래서 이벤트를 처리하는 쪽에서도 이 클럭을 돌린다.
    ///
    /// 다음 틱 시각을 돌려준다(호출자가 control flow 를 잡을 때 쓴다).
    fn drive_present_clock(&self) -> std::time::Instant {
        let now = std::time::Instant::now();
        let mut next = self.next_present_tick.get();
        if now >= next {
            // ***Advance to the first future tick rather than adding one period.*** After a
            // stall (a long render, a debugger break) `next` can be far in the past, and
            // stepping by one period would fire a burst of catch-up frames for moments that
            // have already gone by. Skipping them is what a display does.
            while next <= now {
                next += self.present_period;
            }
            self.next_present_tick.set(next);
            self.note_present_tick();
            if let Some(tile) = self.tiles.first() {
                tile.window.request_redraw();
            }
        }
        next
    }

    fn render_all_tiles(&self) {
        self.note_clock_render_pass();
        let webview = self.webview.borrow();
        let Some(webview) = webview.as_ref() else {
            return;
        };
        let pass_start = std::time::Instant::now();
        let mut tile_ms = vec![0.0f64; self.tiles.len()];
        let mut split = PassSplit::default();
        let mut skipped = 0u32;
        // `gfx_wall_rotate_tile_order`: 시작 타일을 패스마다 한 칸 돌린다.
        //
        // 한 타일이 패스 비용을 몰아서 낸다(큰 캔버스 + DComp off 에서 타일 1 이 42.2ms 중
        // 25.1ms, 나머지 셋은 각 5.7ms). GPU 는 30% 대라 처리량이 아니다. 돌려 보면 그 비용이
        // **첫 번째 위치**의 것인지(그러면 타일별 평균이 고르게 퍼진다) 특정 painter 의
        // 것인지(그 타일만 계속 느리다) 갈린다.
        //
        // `tile_ms` 는 계속 **타일 번호**로 기록한다 — 위치가 아니라. 그래야 WALLPASS 의
        // per-tile 평균이 그대로 판별식이 된다.
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
        // 발사한 타일의 인덱스를 순서대로 들고 있어야 조인이 돌려준 시간을 `tile_ms` 의
        // 제자리에 넣을 수 있다. ★이게 없으면 WALLPASS 의 타일별 값이 0 이 되어 어느 타일이
        // 패스를 붙잡는지 볼 수 없다.★
        let mut dispatched_indices = Vec::new();
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
                    skipped += 1;
                    dispatched[index] = true;
                    continue;
                }
                targets.push(target);
                dispatched[index] = true;
                dispatched_indices.push(index);
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
            let tile_start = std::time::Instant::now();
            match tile.paint_target.get() {
                Some(target) => {
                    // Wall frame barrier: skip if this target already rendered this
                    // logical frame (keep the previous frame on screen).
                    if webview
                        .paint_target_keep_previous_logical_frame(target)
                        .is_some()
                    {
                        skipped += 1;
                        continue;
                    }
                    // ★컨텍스트가 없는 타일은 자기 스레드가 전부 한다★
                    // (`gfx_wall_parallel_tiles`): current 로 삼는 것도, 표출도 그쪽이다.
                    // 셸이 여기서 할 수 있는 것은 "그려라" 뿐이다.
                    let Some(rendering_context) = tile.rendering_context.as_ref() else {
                        let paint_start = std::time::Instant::now();
                        webview.paint_target(target);
                        split.paint_ms += paint_start.elapsed().as_secs_f64() * 1000.0;
                        tile_ms[tile_index] = tile_start.elapsed().as_secs_f64() * 1000.0;
                        continue;
                    };
                    let current_start = std::time::Instant::now();
                    let _ = rendering_context.make_current();
                    split.make_current_ms += current_start.elapsed().as_secs_f64() * 1000.0;
                    let paint_start = std::time::Instant::now();
                    webview.paint_target(target);
                    split.paint_ms += paint_start.elapsed().as_secs_f64() * 1000.0;
                    let present_start = std::time::Instant::now();
                    rendering_context.present();
                    let present_ms = present_start.elapsed().as_secs_f64() * 1000.0;
                    split.present_ms += present_ms;
                    self.note_present(present_ms);
                },
                None => {
                    let rendering_context = tile
                        .rendering_context
                        .as_ref()
                        .expect("the primary tile always owns its context on the shell side");
                    let current_start = std::time::Instant::now();
                    let _ = rendering_context.make_current();
                    split.make_current_ms += current_start.elapsed().as_secs_f64() * 1000.0;
                    let paint_start = std::time::Instant::now();
                    webview.paint();
                    split.paint_ms += paint_start.elapsed().as_secs_f64() * 1000.0;
                    // Capture before present (FLIP_DISCARD discards the backbuffer on Present).
                    self.maybe_capture(tile);
                    let present_start = std::time::Instant::now();
                    rendering_context.present();
                    let present_ms = present_start.elapsed().as_secs_f64() * 1000.0;
                    split.present_ms += present_ms;
                    self.note_present(present_ms);
                },
            }
            tile_ms[tile_index] = tile_start.elapsed().as_secs_f64() * 1000.0;
        }
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
            // ★`tile_ms` 에만 넣고 `split.paint_ms` 에는 더하지 않는다.★ 이 타일들은
            // 서로, 그리고 primary 와 **겹쳐서** 돌았으므로 그 합은 패스가 실제로 쓴
            // 시간이 아니다. 더하면 `WALLSPLIT` 의 `outside` 가 음수가 된다.
            //
            // 겹침은 대신 `WALLPASS` 에서 읽는다: `serial_sum` 이 `pass_ms` 보다 크면
            // 그 배수가 실제로 얻은 병렬도다(겹치지 않으면 둘이 같다).
            for (index, tile_render_ms) in dispatched_indices.iter().zip(in_flight.join()) {
                tile_ms[*index] = tile_render_ms;
            }
        }

        webview.flush_deferred_dcomp_commits();
        self.note_render_pass(
            pass_start.elapsed().as_secs_f64() * 1000.0,
            &tile_ms,
            skipped,
            split,
        );
    }
}

impl ::servo::WebViewDelegate for AppState {
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
    /// `--tile-thread-spike` 가 끝난 상태. 판정을 프로세스 종료 코드로도 내보내기 위해
    /// 들고 있는다 — 로그를 눈으로 읽는 것 말고도 스크립트가 갈라볼 수 있어야 한다.
    SpikeFinished {
        passed: bool,
    },
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

        // ★A 안 1 단계의 관문만 시험하고 끝낸다.★ 본 경로보다 먼저 갈라지는 이유는, 컨텍스트가
        // 이미 붙은 HWND 에 워커가 두 번째를 만들면 `-DComp on` 에서 HWND 당 DComp 타깃이
        // 하나뿐이라 실패하고, 그 실패가 "스레드 때문" 으로 오독되기 때문이다.
        #[cfg(target_os = "windows")]
        if config.tile_thread_spike {
            let passed = tile::plan_and_run_tile_thread_spike(
                event_loop,
                &config.layout,
                &tile_indices,
                &spatial,
                have_topology,
            );
            event_loop.exit();
            *self = Self::SpikeFinished { passed };
            return;
        }

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
                        vsync_driver.clone(),
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
                vsync_driver,
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
            tiles,
            present_ms_sum: Cell::new(0.0),
            present_count: Cell::new(0),
            present_window_start: Cell::new(None),
            pass_stats: RefCell::new(PassStats::default()),
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
            capture_deadline: config.capture.as_ref().map(|_| {
                std::time::Instant::now() + std::time::Duration::from_secs_f64(config.capture_sec)
            }),
            capture_path: config.capture.clone(),
            captured: Cell::new(false),
            pass_counter: Cell::new(0),
            main_busy: RefCell::new(MainBusy::default()),
            clock_stats: RefCell::new(ClockStats::default()),
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
        *self = Self::Running(app_state);
    }

    fn user_event(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop, _event: WakerEvent) {
        if let Self::Running(state) = self {
            // ★깨우기가 합쳐졌음을 먼저 표시한다★ -- 비우기 전에. 이 드레인이 도는 동안
            // 들어온 깨우기는 새 이벤트를 올려야 하고, 순서를 뒤집으면 그것을 잃는다.
            state.wake_pending.store(false, Ordering::SeqCst);
            state.charge_main(MainSlot::Spin, || state.servo.spin_event_loop());
            // 큐가 비지 않아도 박자는 흘러야 한다(`drive_present_clock` 참조).
            state.drive_present_clock();
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
        state.report_main_busy();
        state.report_clock_stats();
        // While a `--capture` is pending, keep polling + redrawing so the capture deadline
        // fires even on a static page that has otherwise gone idle.
        if state.capture_path.is_some() && !state.captured.get() {
            event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
            if let Some(tile) = state.tiles.first() {
                tile.window.request_redraw();
            }
            return;
        }

        let next = state.drive_present_clock();
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(next));
    }

    fn window_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        if let Self::Running(state) = self {
            state.charge_main(MainSlot::Spin, || state.servo.spin_event_loop());
            state.drive_present_clock();
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                if let Self::Running(state) = self {
                    state.note_redraw_requested();
                    state.charge_main(MainSlot::Render, || state.render_all_tiles());
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
#[derive(Debug)]
struct WakerEvent;

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
        if let Err(error) = self.proxy.send_event(WakerEvent) {
            // 올리지 못했으면 표시도 되돌린다 -- 아니면 이후의 모든 깨우기가 막힌다.
            self.pending.store(false, Ordering::SeqCst);
            eprintln!("warning: failed to wake event loop: {error:?}");
        }
    }
}

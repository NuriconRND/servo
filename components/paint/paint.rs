/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::create_dir_all;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bitflags::bitflags;
use crossbeam_channel::Sender;
use dpi::PhysicalSize;
use embedder_traits::{
    EventLoopWaker, InputEventAndId, InputEventId, InputEventResult, ScreenshotCaptureError,
    Scroll, ShutdownState, ViewportDetails, WebViewPoint, WebViewRect,
};
use euclid::{Scale, Size2D};
use image::RgbaImage;
use ipc_channel::ipc::{self};
use log::{debug, info, warn};
use paint_api::display_list::PaintDisplayListInfo;
use paint_api::rendering_context::{RenderingContext, create_adapter_for_requested_gpu};
use paint_api::{
    ImageUpdate, PaintMessage, PaintProxy, PainterSurfmanDetails, PainterSurfmanDetailsMap,
    SerializableDisplayListPayload, WebRenderExternalImageIdManager, WebViewTrait,
};
use profile_traits::mem::{
    ProcessReports, ProfilerRegistration, Report, ReportKind, perform_memory_report,
};
use profile_traits::path;
use profile_traits::time::{self as profile_time};
use servo_base::generic_channel::{self, GenericReceiver, GenericSender, RoutedReceiver};
use servo_base::id::{PainterId, PipelineId, WebViewId};
use servo_canvas_traits::webgl::{WebGLSurfaceId, WebGLThreads};
use servo_config::debug_env;
use servo_config::pref;
use servo_constellation_traits::EmbedderToConstellationMessage;
use servo_geometry::DeviceIndependentPixel;
use smallvec::SmallVec;
use style_traits::CSSPixel;
use surfman::Device;
use surfman::chains::SwapChains;
use webgl::WebGLComm;
use webgl::webgl_thread::WebGLContextBusyMap;
#[cfg(feature = "webgpu")]
use webgpu::canvas_context::WebGpuExternalImageMap;
use webrender::{CaptureBits, MemoryReport};
use webrender_api::units::{DevicePixel, DevicePoint, DeviceVector2D};
use webrender_api::{BuiltDisplayListDescriptor, FontInstanceKey, FontKey, ImageKey};

use crate::InitialPaintState;
use crate::painter::{FrameReadyDiagnostic, Painter, PainterInputs, PumpSource};
use crate::wall_tick::TileOutcome;
use crate::webview_renderer::UnknownWebView;

/// How long the barrier waits for the remaining tiles after the first one is ready.
///
/// ***Deliberately NOT derived from `gfx_refresh_hz`, unlike the two rate limiters.*** This
/// is a tolerance, not a cadence: it answers "how far apart may the tiles be before we give
/// up on agreeing this frame", and that has no reason to track the display rate exactly. A
/// flat, readable 16ms is the point. (It happens to be near a 60Hz frame, which is why the
/// number was chosen, but nothing breaks if the display is 75Hz.)
const WALL_FRAME_BARRIER_DEADLINE: Duration = Duration::from_millis(16);
const WALL_FRAME_BARRIER_RETENTION: Duration = Duration::from_secs(2);
// task-2: SERVO_WALL_FRAME_PACING/_MAX_PENDING/_MIN_INTERVAL_MS 는 pref 로 옮겼다
// (gfx_wall_frame_pacing_enabled/_max_pending/_min_interval_ms, WallFramePacingConfig::from_prefs).
const WALL_FRAME_PACING_INFO_INTERVAL: u64 = 120;
const WALL_MEDIA_IMAGE_FANOUT_INFO_INTERVAL: u64 = 120;

static WALL_MEDIA_IMAGE_FANOUT_COUNT: AtomicU64 = AtomicU64::new(0);

// -------------------------------------------------------------------------------------
// PAINTERLOCK -- `with_painter_mut` 가 painter 스레드의 큐에서 기다린 시간.
//
// ★왜 재는가.★ 표출 틱이 단일 프레임 생산자가 되면 틱은 각 painter 에 이 호출로 닿는다.
// 타일 스레드가 렌더 중이면 호출은 그 뒤에 서므로, 얼마나 서는지 모르면 틱 자신의 클럭이
// 흔들릴 수 있다 -- 설계 6 절에서 확인되지 않은 유일한 항목이었다. 결과는 틱이 painter 에
// 닿는 ***방식***(블로킹 호출 / 비블로킹 포스트)만 정한다. 생산자의 수는 정하지 않는다.
//
// ★재는 구간은 호출 순간부터 콜백이 painter 스레드에서 시작하는 순간까지다.★ 콜백 자신의
// 실행 시간은 뺀다 -- 그것은 큐 대기가 아니라 일의 크기다. 스레드 없는 painter 는 기다릴
// 것이 없어 재지 않는다.
//
// ★`max` 하나로는 "3ms 한 번" 과 "3ms 를 60 번" 이 구분되지 않고 그 둘은 결론이 정반대다★
// (`DCOMP_SLOW_COMMIT_MS` 의 doc 이 같은 말을 한다). 그래서 `over_1ms` 를 같이 센다.
// -------------------------------------------------------------------------------------

/// `SERVO_WALL_PAINTER_LOCK_PROF` 게이트. 호출마다 묻는 자리라 env 읽기를 캐시한다.
static PAINTER_LOCK_PROF: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    servo_config::debug_env::string(&servo_config::debug_env::WALL_PAINTER_LOCK_PROF)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
});

/// painter 하나의 한 창.
struct PainterLockWindow {
    start: Instant,
    wait_ms: Vec<f64>,
}

/// painter 별 창. 콜백은 painter 스레드들에서 불리고 전역을 같이 쓰므로 뮤텍스다 -- 초당
/// 수백 번이라 경합이 문제되지 않는다. 게이트가 꺼져 있으면 아예 잠그지 않는다.
static PAINTER_LOCK_WINDOWS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<PainterId, PainterLockWindow>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn record_painter_lock_wait(painter_id: PainterId, waited: Duration) {
    let Ok(mut windows) = PAINTER_LOCK_WINDOWS.lock() else {
        return;
    };
    let now = Instant::now();
    let window = windows.entry(painter_id).or_insert_with(|| PainterLockWindow {
        start: now,
        wait_ms: Vec::new(),
    });
    window.wait_ms.push(waited.as_secs_f64() * 1000.0);
    if now.duration_since(window.start) < Duration::from_secs(1) {
        return;
    }
    window.start = now;
    let mut wait_ms = std::mem::take(&mut window.wait_ms);
    // 로그는 락 밖에서 찍는다 -- 다른 painter 의 콜백을 막지 않는다.
    drop(windows);

    let over_1ms = wait_ms.iter().filter(|wait| **wait >= 1.0).count();
    let n = wait_ms.len();
    let Some(wait) = servo_base::interval_summary::percentiles(&mut wait_ms) else {
        return;
    };
    log::warn!(
        "PAINTERLOCK painter={painter_id:?} n={n} wait_ms p50={:.2} p95={:.2} max={:.2}          over_1ms={over_1ms}",
        wait.p50,
        wait.p95,
        wait.max,
    );
}

/// An option to control what kind of WebRender debugging is enabled while Servo is running.
#[derive(Copy, Clone)]
pub enum WebRenderDebugOption {
    Profiler,
    TextureCacheDebug,
    RenderTargetDebug,
}

/// [`Paint`] is Servo's rendering subsystem. It has a few responsibilities:
///
/// 1. Maintain a WebRender instance for each [`RenderingContext`] that Servo knows about.
///    [`RenderingContext`]s are per-`WebView`, but more than one `WebView` can use the same
///    [`RenderingContext`]. This allows multiple `WebView`s to share the same WebRender
///    instance which is more efficient. This is useful for tabbed web browsers.
/// 2. Receive display lists from the layout of all of the currently active `Pipeline`s
///    (frames). These display lists are sent to WebRender, and new frames are generated.
///    Once the frame is ready the [`Painter`] for the WebRender instance will ask libservo
///    to inform the embedder that a new frame is ready so that it can trigger a paint.
/// 3. Drive animation and animation callback updates. Animation updates should ideally be
///    coordinated with the system vsync signal, so the `RefreshDriver` is exposed in the
///    API to allow the embedder to do this. The [`Painter`] then asks its `WebView`s to
///    update their rendering, which triggers layouts.
/// 4. Eagerly handle scrolling and touch events. In order to avoid latency when handling
///    these kind of actions, each [`Painter`] will eagerly process touch events and
///    perform panning and zooming operations on their WebRender contents -- informing the
///    WebView contents asynchronously.
///
/// `Paint` and all of its contained structs should **never** block on the Constellation,
/// because sometimes the Constellation blocks on us.
pub struct Paint {
    /// All of the [`Painters`] for this [`Paint`]. Each [`Painter`] handles painting to
    /// a single [`RenderingContext`].
    painters: Vec<PainterHost>,

    /// The set of [`Painter`] targets that should render each logical [`WebViewId`].
    ///
    /// Today this normally contains a single target whose [`PainterId`] is embedded in the
    /// [`WebViewId`]. Wall rendering will extend this table so one logical `WebView` can be
    /// rendered into multiple tile `RenderingContext`s.
    webview_painter_targets: RefCell<HashMap<WebViewId, Vec<PainterId>>>,

    /// Logical frame id used to correlate one scene frame across multiple paint targets.
    next_logical_frame_id: Cell<u64>,

    /// Software barrier diagnostics for wall frames that fan out to multiple paint targets.
    /// 임베더 쪽 `WebView` 핸들. ★메인 스레드 전용이다★ — `Weak<RefCell<..>>` 기반이라
    /// `Send` 가 아니고, 그래서 `Painter`/`WebViewRenderer` 에는 넘기지 않는다. 그쪽은
    /// `PaintMessage::SetWebViewAnimating` 으로 통보만 보내고 이 표를 거쳐 실제 호출이
    /// 일어난다(설계 §3.2).
    webviews: RefCell<HashMap<WebViewId, Box<dyn WebViewTrait>>>,

    wall_frame_coordinator: RefCell<WallFrameCoordinator>,

    /// Wall frame pacing policy used to avoid stacking stale logical frames while a
    /// previous wall frame is still pending in WebRender.
    wall_frame_pacing_config: WallFramePacingConfig,

    /// Latest coalesced wall frame request per logical `WebView`.
    coalesced_wall_frame_requests: RefCell<HashMap<WebViewId, CoalescedWallFrameRequest>>,

    /// Last issued wall frame request per logical `WebView`, used to cap wall frame pacing.
    last_wall_frame_issue_at: RefCell<HashMap<WebViewId, Instant>>,

    /// Next scheduled wake for releasing a wall frame held only by the minimum pacing interval.
    wall_frame_pacing_next_wake_at: Cell<Option<Instant>>,

    /// Total wall frame requests coalesced by the latest-first pacer.
    wall_frame_pacing_coalesced_count: Cell<u64>,

    /// Total wall frame requests released by the latest-first pacer.
    wall_frame_pacing_released_count: Cell<u64>,

    /// Which pacing gate blocked each coalesced request. See [`WallFramePacingBlockTally`]
    /// for why a zero in one of these does not mean that gate never binds.
    wall_frame_pacing_block_tally: WallFramePacingBlockTally,

    /// ★스크린샷 폴링 래치.★ `Paint::request_screenshot` 이 세우고, 전 타일에 요청이
    /// 하나도 안 남은 것을 확인한 틱이 내린다. 2026-10-22 Task 9.
    ///
    /// 판정이 아니라 ***비용*** 장치다. 생산 여부를 정하려면 매 틱 전 타일에 물어야 하는데
    /// (`with_painter` 는 스레드 타일에서 채널 왕복이다 -- 바쁠 때 호출당 250µs),
    /// 스크린샷을 한 번도 요청하지 않은 실행에서는 그 왕복이 통째로 낭비다. 래치가 내려가
    /// 있으면 아예 묻지 않는다.
    ///
    /// ★세우는 자리가 요청 진입점 하나뿐이라 "요청이 있는데 안 묻는" 경우는 없다.★
    /// 반대로 잘못 세워져 있으면 손해는 한 틱 한 번의 질의뿐이고, 그 질의가 곧 래치를
    /// 내린다 -- 틀리는 방향이 안전한 쪽이다.
    screenshot_poll_latch: Cell<bool>,

    /// 스크린샷 때문에 생산을 쉰 틱 수. 현재 1 초 창 / 누계 / 창 시작.
    production_suspended_in_window: Cell<u64>,
    production_suspended_total: Cell<u64>,
    production_suspended_last_report: Cell<Option<Instant>>,
    /// 현재 창에서 걷은 keep-previous 지시의 누계. ★창 단위다 -- 틱 단위가 아니다.★
    /// 같은 줄의 `ticks_in_window` 와 단위를 맞춰야 설계 8-1 이 지목한 교착 탐지기로
    /// 읽힌다(둘을 섞어 두면 "이 창에 몇 개를 걷었나" 를 답할 수 없다).
    keep_previous_released_in_window: Cell<u64>,

    /// ★수요 구동 틱의 진입 수와 그중 생산한 수.★ 현재 1 초 창 / 창 시작. 2026-10-22.
    ///
    /// ***왜 재는가*** -- `Paint::on_demand_wall_tick` 은 이벤트 루프 스핀마다 불리므로
    /// 진입 수는 표출 주기가 아니라 ***루프의 폴링 속도***다. 그 둘이 벌어져도 WR 프레임은
    /// `renderer_behind` 거절이 막아 주어 fps 에는 안 나타나고, 메인 스레드 CPU 로만
    /// 나타난다. 그러면 로그만 보는 사람은 폭주를 영영 못 본다. 실제로 그랬다:
    /// `gather_components` 안의 깨우기가 다음 에지를 다시 예약해 ★틱 N 이 틱 N+1 을
    /// 되먹임하는 고리★ 가 있었고(2026-10-22 전수 리뷰 5 번), 아무 지표도 그것을 안 보였다.
    ///
    /// ★판정은 `produced` 혼자 한다 -- 비(ratio)가 아니다.★ 건강한 상태에서 `produced` 는
    /// 표출률(60)이고 `edges` 는 폴링 속도(수만)라, 정상일수록 비는 0 에 가깝다. 비를
    /// 판정으로 쓰면 고친 상태가 고장으로 읽힌다. 읽는 법은 `Paint::note_demand_tick`.
    demand_tick_edges_in_window: Cell<u64>,
    demand_tick_produced_in_window: Cell<u64>,
    demand_tick_window_start: Cell<Option<Instant>>,

    /// A [`PaintProxy`] which can be used to allow other parts of Servo to communicate
    /// with this [`Paint`].
    pub(crate) paint_proxy: PaintProxy,

    /// An [`EventLoopWaker`] used to wake up the main embedder event loop when the renderer needs
    /// to run.
    pub(crate) event_loop_waker: Box<dyn EventLoopWaker>,

    /// Tracks whether we are in the process of shutting down, or have shut down and
    /// should shut down `Paint`. This is shared with the `Servo` instance.
    shutdown_state: Rc<Cell<ShutdownState>>,

    /// The port on which we receive messages.
    paint_receiver: RoutedReceiver<PaintMessage>,

    /// The channel on which messages can be sent to the constellation.
    pub(crate) embedder_to_constellation_sender: Sender<EmbedderToConstellationMessage>,

    /// The [`WebRenderExternalImageIdManager`] used to generate new `ExternalImageId`s.
    webrender_external_image_id_manager: WebRenderExternalImageIdManager,

    /// A [`HashMap`] of [`PainterId`] to the Surfaman types (`Device`, `Adapter`) that
    /// are specific to a particular [`Painter`].
    pub(crate) painter_surfman_details_map: PainterSurfmanDetailsMap,

    /// A [`HashMap`] of `WebGLContextId` to a usage count. This count indicates when
    /// WebRender is still rendering the context. This is used to ensure properly clean
    /// up of all Surfman `Surface`s.
    pub(crate) busy_webgl_contexts_map: WebGLContextBusyMap,

    /// The [`WebGLThreads`] for this renderer.
    webgl_threads: WebGLThreads,

    /// The shared [`SwapChains`] used by [`WebGLThreads`] for this renderer.
    pub(crate) swap_chains: SwapChains<WebGLSurfaceId, Device>,

    /// The channel on which messages can be sent to the time profiler.
    time_profiler_chan: profile_time::ProfilerChan,

    /// A handle to the memory profiler which will automatically unregister
    /// when it's dropped.
    _mem_profiler_registration: ProfilerRegistration,

    /// Some XR devices want to run on the main thread.
    #[cfg(feature = "webxr")]
    webxr_main_thread: RefCell<webxr::MainThreadRegistry>,

    /// An map of external images shared between all `WebGpuExternalImages`.
    #[cfg(feature = "webgpu")]
    webgpu_image_map: std::cell::OnceCell<WebGpuExternalImageMap>,
}

/// 한 `Painter` 를 소유하는 주체.
///
/// ★지금은 인라인 하나뿐이다★ — 병렬 타일 렌더(A 안)에서 여기에 스레드 변형이 붙고,
/// 그때 바뀌는 것은 이 구조체와 `Paint::with_painter*` 의 속뿐이다.
///
/// `painter_id` 와 `rendering_context` 를 **밖에 들고 있는 것이 요점**이다. 예전에는 둘 다
/// painter 를 빌려서 읽었는데(`painters.iter().map(borrow)`), painter 가 다른 스레드로 가면
/// id 를 알려고 스레드를 왕복하는 꼴이 된다. 등록·조회·제거는 painter 를 건드리지 않고
/// 끝나야 한다.
struct PainterHost {
    painter_id: PainterId,
    /// 컨텍스트 동일성 판정 전용(`register_rendering_context`). ★스레드 변형에는 `None` 이다★
    /// — 컨텍스트가 타일 스레드에서 만들어지므로 메인에 사본이 없고, 등록 경로도 다르다.
    rendering_context: Option<Rc<dyn RenderingContext>>,
    kind: PainterHostKind,
}

enum PainterHostKind {
    /// 오늘의 경로. painter 가 호출자와 같은 스레드에 있다.
    Inline(Rc<RefCell<Painter>>),
    /// 병렬 타일 렌더(A 안). painter 가 자기 스레드에 살고, 여기 있는 것은 그 스레드로
    /// 일을 보내는 손잡이뿐이다.
    Threaded(ThreadedPainter),
}

/// 아직 끝나지 않은 타일 렌더들. [`Paint::dispatch_paint_targets`] 가 돌려준다.
///
/// ★들고 있는 동안 그 타일들이 나란히 돈다.★ 떨어뜨리면(`join` 없이) 결과를 안 기다리는
/// 것이 되는데, 그러면 다음 패스가 이전 패스와 겹쳐 버리므로 반드시 `join` 할 것.
pub struct PaintTargetsInFlight {
    in_flight: Vec<crossbeam_channel::Receiver<TileRenderTiming>>,
}

/// 스레드 타일 하나가 돌려주는 시간. ★둘로 나뉘어 있는 것이 요점이다.★
///
/// 셸이 보는 조인 대기는 `queued_ms + render_ms` 인데, 이 둘은 원인이 완전히 다르다.
/// `render_ms` 가 크면 그 타일의 렌더가 무거운 것이고, `queued_ms` 가 크면 **요청이 줄에서
/// 기다린 것**이다 — 요청 채널이 `dispatch_detached`(이미지 갱신)와 같은 무한 큐라서, 그쪽
/// 일이 밀려 있으면 렌더 요청이 그 뒤에 선다.
///
/// 하나로 합쳐 놓았던 동안 2026-09-28 계측에서 타일별 렌더가 각 0.5~0.8ms 인데 패스는
/// 13.1ms 였다. 그 차이가 어느 쪽인지 볼 수 없었다.
#[derive(Clone, Copy, Default)]
pub struct TileRenderTiming {
    /// 발사에서 타일 스레드가 실제로 그 요청을 집기까지 — 큐에서 기다린 시간(ms).
    pub queued_ms: f64,
    /// `painter.render` 가 실제로 쓴 시간(ms).
    pub render_ms: f64,
}

impl PaintTargetsInFlight {
    /// 발사해 둔 타일들이 전부 끝나기를 기다리고, **각자의 시간** 을 발사 순서대로 돌려준다.
    ///
    /// ★시간을 돌려주는 것이 요점이다.★ 팬아웃 이후 셸의 루프는 스레드 타일을 건너뛰므로
    /// `WALLPASS` 의 타일별 값이 0 이 되고, 그러면 "어느 타일이 패스를 붙잡는가" 를 볼 수
    /// 없다. 실제로 그 눈이 없어 처음 팬아웃 측정에서 타일별 분포가
    /// `[25.87, 0, 0, 0]` 으로 나왔다(2026-09-03, `log_webgpu/68`).
    pub fn join(self) -> Vec<TileRenderTiming> {
        self.in_flight
            .into_iter()
            // 스레드가 죽었으면 `Err` 다. 그때는 기다릴 것이 없다 — 그 사실은 요청을 보낼 때
            // 이미 경고했으므로 0 으로 둔다.
            .map(|done| done.recv().unwrap_or_default())
            .collect()
    }
}

/// 타일 스레드로 일을 보내는 손잡이.
struct ThreadedPainter {
    requests: Sender<PainterRequest>,
    join_handle: Option<std::thread::JoinHandle<()>>,
    /// 아직 처리되지 않은 **기다리지 않는** 요청 수.
    ///
    /// ★큐가 무한이라 배압이 없다.★ 결과를 기다리지 않게 바꾼 덕에 메인 스레드가 페인터
    /// 렌더 뒤에 줄서지 않게 됐지만(초당 263ms → 60ms), 페인터가 밀리면 그만큼 요청이
    /// 쌓이고 그 안의 프레임이 메모리를 잡는다. 기다리는 것으로 되돌리면 그 개선이 통째로
    /// 사라지므로, 기다리지 않되 **길이만 막는다**.
    detached_in_flight: Arc<AtomicUsize>,
}

enum PainterRequest {
    /// ★열거형 변형이 아니라 클로저를 그대로 보낸다.★ 3.1~3.2 에서 painter 접근을 클로저로
    /// 좁히고 `Send + 'static` 을 강제해 둔 덕에, 연산마다 메시지 변형을 만들 필요가 없다.
    /// 그 준비가 없었다면 여기서 25 개짜리 열거형을 손으로 써야 했다.
    Run(Box<dyn FnOnce(&mut Painter) + Send>),
    Exit,
}

/// 타일 스레드에서 `RenderingContext` 를 **만들어 내는 것**.
///
/// ★메인이 만든 것을 넘길 수 없다★ — surfman `Device` 는 스레드 로컬이라 만든 스레드가
/// 소멸까지 소유해야 한다(설계 §3-1). 그래서 만들어진 컨텍스트가 아니라 만드는 방법을
/// 넘긴다. 실제로 이것이 되는지는 스파이크로 확인했다(`log_webgpu/62`, `-TileThreadSpike`).
pub type RenderingContextFactory = Box<dyn FnOnce() -> Rc<dyn RenderingContext> + Send>;

impl ThreadedPainter {
    /// 클로저를 타일 스레드에서 돌리고 결과를 기다린다.
    ///
    /// ★제어 경로는 블로킹으로 둔다★ — `add_webview` 나 `set_page_zoom` 같은 것들은
    /// 프레임당 도는 것이 아니라 성능에 무관하고, 블로킹이면 **동기 의미론이 그대로 보존된다**
    /// (설계 §3.1). 이득이 나오는 매 프레임 경로만 나중에 팬아웃/조인으로 바꾼다.
    ///
    /// 스레드가 이미 죽었으면 `None` — 호출자에게는 "그 painter 가 없다" 와 같은 뜻이고,
    /// 인라인 경로에서 id 를 못 찾았을 때와 같은 값이다.
    fn run_blocking<R: Send + 'static>(
        &self,
        callback: impl FnOnce(&mut Painter) -> R + Send + 'static,
    ) -> Option<R> {
        self.dispatch(callback)?.recv().ok()
    }

    /// 일을 보내고 **기다리지 않는다.** 결과는 돌려받은 수신구로 나중에 받는다.
    ///
    /// ★팬아웃이 이득을 내는 지점이 여기다.★ 한 패스의 대부분이 타일마다의 `AcquireSync`
    /// 대기인데, 그 대기는 타일별·GPU별이라 서로 겹칠 수 있다. 보내고 바로 다음 타일을
    /// 보내면 네 번의 대기가 나란히 흐르고, 패스는 합이 아니라 **최댓값**이 된다.
    /// 결과를 기다리지 않는 요청. 큐가 이미 깊으면 **보내지 않는다**.
    ///
    /// 버려도 되는 이유는 이 경로로 오는 것이 이미지 갱신이고, 페인터가 그것을 키별
    /// 최신만 남기는 스태시에 넣기 때문이다 — 밀린 상태에서 옛 프레임을 보내 봐야 다음
    /// 것에 곧바로 덮인다. 버리는 것이 손해가 아니라 그게 합침이다.
    fn dispatch_detached(
        &self,
        limit: usize,
        callback: impl FnOnce(&mut Painter) + Send + 'static,
    ) -> bool {
        if limit > 0 && self.detached_in_flight.load(Ordering::Relaxed) >= limit {
            return false;
        }
        self.detached_in_flight.fetch_add(1, Ordering::Relaxed);
        let in_flight = self.detached_in_flight.clone();
        let request = PainterRequest::Run(Box::new(move |painter| {
            callback(painter);
            in_flight.fetch_sub(1, Ordering::Relaxed);
        }));
        if self.requests.send(request).is_err() {
            self.detached_in_flight.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    fn dispatch<R: Send + 'static>(
        &self,
        callback: impl FnOnce(&mut Painter) -> R + Send + 'static,
    ) -> Option<crossbeam_channel::Receiver<R>> {
        let (result_sender, result_receiver) = crossbeam_channel::bounded(1);
        let request = PainterRequest::Run(Box::new(move |painter| {
            let _ = result_sender.send(callback(painter));
        }));
        if self.requests.send(request).is_err() {
            warn!("Painter thread is gone; dropping a request for it");
            return None;
        }
        Some(result_receiver)
    }
}

impl Drop for ThreadedPainter {
    fn drop(&mut self) {
        // 스레드가 자기 컨텍스트를 소멸시켜야 한다(스레드 로컬 계약). 그래서 그냥 손잡이를
        // 버리는 것이 아니라 끝내라고 말하고 기다린다.
        let _ = self.requests.send(PainterRequest::Exit);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Default)]
struct WallFrameCoordinator {
    barriers: HashMap<u64, WallFrameBarrier>,
    active_webview_frames: HashMap<WebViewId, u64>,
    /// "이 논리 프레임에는 직전 프레임을 유지하라" 는 타일별 지시. ★무장 지점은 둘이다★ --
    /// `miss_barrier`(마감을 놓친 타일)와 `arm_keep_previous`(2026-10-22 Task 8).
    /// 지우는 쪽은 셋이다: `complete_barrier`, `miss_barrier` 의 준비된 타일, 그리고 다음
    /// 벽 프레임의 `expire_keep_previous_before`.
    ///
    /// ★`arm_keep_previous` 는 "역압으로 거절한 타일" 이 ***아니다*** -- 2026-10-22 정정.★
    /// 그렇게 적혀 있었고 그것이 "역압" 으로 grep 하면 제일 먼저 걸리는 자리였다. 실제로
    /// 무장까지 가는 거절은 `WaitingOnCanvasImages` 하나뿐이다
    /// (`wall_tick::decline_may_arm_keep_previous(RendererBehind)` 이 거짓 -- 역압에 무장하면
    /// 자기잠금이다). 설계 2-5 가 말하던 "역압 거절의 배리어 참여" 는 미구현이고, 그 전말은
    /// `WallFrameCoordinator::arm_keep_previous` 주석 한 벌에만 둔다.
    keep_previous_painters: HashMap<PainterId, KeepPreviousFrame>,
    /// 한 번이라도 프레임을 화면에 낸 painter. ★"직전 프레임 유지" 는 직전 프레임이 있는
    /// 타일에게만 말이 된다★ -- 없는 타일에게 그러면 검은 타일이 된다
    /// (`keep_previous_logical_frame` 주석). 기록 지점은 그 함수 하나다.
    presented_painters: HashSet<PainterId>,
    delay_injection: Option<WallFrameDelayInjection>,
    /// How many keep-previous instructions have been expired by a newer wall frame, for
    /// rate-limiting the log (see `expire_keep_previous_before`).
    keep_previous_expired_count: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum WallFramePacingMode {
    Latest,
    Legacy,
}

#[derive(Clone, Copy)]
struct WallFramePacingConfig {
    mode: WallFramePacingMode,
    max_pending: usize,
    min_interval: Duration,
}

#[derive(Clone)]
struct WallFrameRequest {
    target_painter_ids: Vec<PainterId>,
    wall_webview_targets: Vec<(WebViewId, Vec<PainterId>)>,
}

struct CoalescedWallFrameRequest {
    request: WallFrameRequest,
    coalesced_count: u64,
    max_pending_seen: usize,
}

/// 어느 페이싱 게이트가 요청을 막았나.
///
/// ★2026-10-21 에 payload 가 빠졌다.★ `webview_id` / `pending_max` / `elapsed_ms` 를 읽던
/// 유일한 자리가 `coalesce_wall_frame_request` 였고, 그 함수는 요청을 넣어 줄 사람과 함께
/// 사라졌다(`PaintMessage::GenerateFrame` 은 이제 아무 일도 하지 않는다).
enum WallFramePacingBlockReason {
    Active,
    Pending,
    TooSoon,
}

/// How many wall frame requests each pacing gate blocked.
///
/// `total_coalesced` alone says the pacer is holding frames back but not WHICH gate is
/// doing it, and the three gates lead to completely different conclusions: `Pending` means
/// WebRender is still busy (the pacer is a symptom, not the cause), `TooSoon` means the
/// `gfx_wall_frame_min_interval_ms` knob is the limit, and `Active` means a previous wall
/// frame's barrier has not completed.
///
/// ***These count the gate that blocked a request, not every gate that would have.***
/// `wall_frame_request_pacing_block_reason` returns the FIRST match in the order
/// Active -> Pending -> TooSoon, so an earlier gate masks the later ones: a zero here does
/// NOT mean that gate never binds. Active and Pending very nearly coincide at the default
/// `gfx_wall_frame_max_pending=1`, and Active is checked first, so read a low Pending count
/// as "Active got there first", not as "pending frames are not a constraint".
///
/// ★2026-10-21 부터 이 셋은 영구히 0 이다★ -- 세던 자리(`coalesce_wall_frame_request`)가
/// 요청을 넣어 줄 사람과 함께 사라졌다. 0 이 아닌 값이 나오면 coalesce 큐를 다시 채운
/// 사람이 생겼다는 뜻이고, 그것이 곧 두 번째 생산자다(설계 8 절).
#[derive(Default)]
struct WallFramePacingBlockTally {
    active: Cell<u64>,
    pending: Cell<u64>,
    too_soon: Cell<u64>,
}

struct KeepPreviousFrame {
    logical_frame_id: u64,
    remove_after_skip_query: bool,
}

struct WallFrameDelayInjection {
    target_index: usize,
    after_logical_frame_id: u64,
    remaining_frames: u64,
}

struct WallFrameBarrier {
    webview_ids: Vec<WebViewId>,
    expected_painter_ids: Vec<PainterId>,
    ready_painter_ids: Vec<PainterId>,
    requested_at: Instant,
    first_ready_at: Option<Instant>,
    completed_at: Option<Instant>,
    missed_deadline: bool,
    presentation_decision_made: bool,
    need_repaint: bool,
    injected_delayed_painter_id: Option<PainterId>,
    injected_ready_suppressed: bool,
}

struct WallFrameRepaintDecision {
    painter_id: PainterId,
    repaint_needed: bool,
}

#[derive(Default)]
struct WallFrameCoordinatorUpdate {
    repaint_decisions: Vec<WallFrameRepaintDecision>,
    released_webview_ids: Vec<WebViewId>,
}

impl WallFramePacingConfig {
    /// 예전엔 `SERVO_WALL_FRAME_PACING`/`_MAX_PENDING`/`_MIN_INTERVAL_MS` 를 프로세스 시작 시
    /// 한 번 env 로 읽었다. 이제 pref 다 - `gfx_wall_frame_pacing_enabled` 는 bool 이라
    /// 예전의 "legacy/latest/그 외 문자열은 경고 후 latest" 3갈래 판정이 "켜짐=Latest,
    /// 꺼짐=Legacy" 2갈래로 바뀐다(잘못된 문자열이라는 상태 자체가 pref 시스템에서는
    /// 사라진다). max_pending/min_interval_ms 의 "0 이하면 경고 후 기본값" 클램프는 그대로
    /// 유지한다 - pref 로 옮긴다고 검증이 사라지면 안 된다.
    fn from_prefs() -> Self {
        let mode = if pref!(gfx_wall_frame_pacing_enabled) {
            WallFramePacingMode::Latest
        } else {
            WallFramePacingMode::Legacy
        };

        let raw_max_pending = pref!(gfx_wall_frame_max_pending);
        let max_pending = if raw_max_pending > 0 {
            raw_max_pending as usize
        } else {
            warn!("Ignoring gfx_wall_frame_max_pending={raw_max_pending}; using default 1");
            1
        };

        // `0` now means "one paint-timer period", not "invalid, fall back to 16". It used to
        // warn and use 16ms, so nothing that worked before changes -- but 16ms is 62.5Hz, and
        // ***an integer-millisecond knob cannot express 60Hz at all***: 16 is 4.2% fast, 17 is
        // 2% slow. Deriving it gets the exact period and keeps this limiter in step with the
        // paint timer and the video composite coalescing instead of running 4% ahead of both.
        // An explicit positive value is still honoured verbatim.
        let raw_min_interval_ms = pref!(gfx_wall_frame_min_interval_ms);
        let min_interval = if raw_min_interval_ms > 0 {
            Duration::from_millis(raw_min_interval_ms as u64)
        } else {
            if raw_min_interval_ms < 0 {
                warn!(
                    "Ignoring gfx_wall_frame_min_interval_ms={raw_min_interval_ms} (negative); \
                     using one paint-timer period"
                );
            }
            crate::refresh_driver::paint_timer_period()
        };

        Self {
            mode,
            max_pending,
            min_interval,
        }
    }

    fn enabled(self) -> bool {
        self.mode == WallFramePacingMode::Latest
    }
}

impl WallFrameRequest {
    fn wall_webview_ids(&self) -> Vec<WebViewId> {
        let mut webview_ids = Vec::new();
        for (webview_id, _) in &self.wall_webview_targets {
            if !webview_ids.contains(webview_id) {
                webview_ids.push(*webview_id);
            }
        }
        webview_ids
    }

    fn pacing_key(&self) -> Option<WebViewId> {
        self.wall_webview_targets
            .first()
            .map(|(webview_id, _)| *webview_id)
    }
}

impl WallFrameCoordinatorUpdate {
    fn extend(&mut self, other: Self) {
        self.repaint_decisions.extend(other.repaint_decisions);
        for webview_id in other.released_webview_ids {
            if !self.released_webview_ids.contains(&webview_id) {
                self.released_webview_ids.push(webview_id);
            }
        }
    }
}

impl WallFrameBarrier {
    fn new(
        webview_ids: Vec<WebViewId>,
        expected_painter_ids: Vec<PainterId>,
        requested_at: Instant,
        injected_delayed_painter_id: Option<PainterId>,
    ) -> Self {
        Self {
            webview_ids,
            expected_painter_ids,
            ready_painter_ids: Vec::new(),
            requested_at,
            first_ready_at: None,
            completed_at: None,
            missed_deadline: false,
            presentation_decision_made: false,
            need_repaint: false,
            injected_delayed_painter_id,
            injected_ready_suppressed: false,
        }
    }

    fn deadline_at(&self) -> Option<Instant> {
        self.first_ready_at
            .map(|first_ready_at| first_ready_at + WALL_FRAME_BARRIER_DEADLINE)
    }

    fn missing_painter_ids(&self) -> Vec<PainterId> {
        self.expected_painter_ids
            .iter()
            .copied()
            .filter(|painter_id| !self.ready_painter_ids.contains(painter_id))
            .collect()
    }

    fn missed_deadline_at(&self, now: Instant) -> bool {
        self.deadline_at()
            .is_some_and(|deadline_at| now >= deadline_at)
    }
}

impl WallFrameDelayInjection {
    /// `debug_env::int()`가 돌려준 원시 값을 음수가 아닌 값으로 검증한다. `debug_env::int()`
    /// 자체는 정수로 파싱되지 않는 값만 진단하고(모듈 doc 참고) 부호는 보지 않으므로, 세
    /// 노브(target_index/after/count) 모두 이 함수로 감싸 음수를 놓치지 않게 한다 — 그렇지
    /// 않으면 `SERVO_WALL_FRAME_DELAY_AFTER=-5` 같은 오타가 아무 경고 없이 기본값으로
    /// 접혀, 배리어 실패 주입 도구가 조용히 무력화된다.
    fn require_non_negative(flag: &'static debug_env::DebugFlag, raw: i64) -> Option<u64> {
        match u64::try_from(raw) {
            Ok(value) => Some(value),
            Err(_) => {
                warn!(
                    "Ignoring wall frame delay injection: {}={raw} must not be negative",
                    flag.name
                );
                None
            },
        }
    }

    fn from_environment() -> Option<Self> {
        // 미설정 또는 정수로 파싱 안 됨(debug_env::int() 가 둘을 구분하지 않는다) -> 주입
        // 전체 비활성. 음수는 require_non_negative 가 별도로 경고한다.
        let raw_target_index = debug_env::int(&debug_env::WALL_FRAME_DELAY_TARGET_INDEX)?;
        let target_index = usize::try_from(Self::require_non_negative(
            &debug_env::WALL_FRAME_DELAY_TARGET_INDEX,
            raw_target_index,
        )?)
        .ok()?;

        // 미설정/비정수는 조용히 기본값 1(이전 parse_optional_u64 와 동일하게 미설정은
        // 무경고). 음수는 require_non_negative 가 경고한 뒤 기본값 1로 접힌다.
        let after_logical_frame_id = debug_env::int(&debug_env::WALL_FRAME_DELAY_AFTER)
            .and_then(|raw| Self::require_non_negative(&debug_env::WALL_FRAME_DELAY_AFTER, raw))
            .unwrap_or(1);
        let remaining_frames = debug_env::int(&debug_env::WALL_FRAME_DELAY_COUNT)
            .and_then(|raw| Self::require_non_negative(&debug_env::WALL_FRAME_DELAY_COUNT, raw))
            .unwrap_or(1);
        if remaining_frames == 0 {
            warn!(
                "Ignoring wall frame delay injection: {} must be > 0",
                debug_env::WALL_FRAME_DELAY_COUNT.name
            );
            return None;
        }

        warn!(
            "Wall frame delay injection enabled: target_index={} after_logical_frame_id={} \
             frame_count={}",
            target_index, after_logical_frame_id, remaining_frames
        );

        Some(Self {
            target_index,
            after_logical_frame_id,
            remaining_frames,
        })
    }

    fn target_for_frame(
        &mut self,
        logical_frame_id: u64,
        expected_painter_ids: &[PainterId],
    ) -> Option<PainterId> {
        if self.remaining_frames == 0 || logical_frame_id < self.after_logical_frame_id {
            return None;
        }

        let Some(painter_id) = expected_painter_ids.get(self.target_index).copied() else {
            warn!(
                "Disabling wall frame delay injection: target_index={} is outside expected \
                 paint targets {:?}",
                self.target_index, expected_painter_ids
            );
            self.remaining_frames = 0;
            return None;
        };

        self.remaining_frames -= 1;
        Some(painter_id)
    }
}

impl WallFrameCoordinator {
    fn from_environment() -> Self {
        Self {
            delay_injection: WallFrameDelayInjection::from_environment(),
            ..Default::default()
        }
    }

    /// 이 논리 프레임의 배리어를 연다. ★`expected_painter_ids` 는 ***생성한 타일***뿐이다★
    /// -- 호출자(`Paint::issue_wall_frame_request`)가 그렇게 넘긴다. 배리어가 기다리는
    /// 신호는 frame-ready 이고, 거절한 타일은 트랜잭션을 안 보냈으므로 그 신호를 영영 안
    /// 낸다.
    ///
    /// ## ★설계 2-5 의 요구는 2026-10-22 현재 미구현이다★
    ///
    /// 2-5 는 "거절한 타일도 keep-previous 로 ***배리어에 참여***한다. 벽 프레임은 언제나
    /// 전 타일을 덮는다" 를 요구한다. ★구현에는 그 참여가 없다.★ 거절한 타일에 일어나는
    /// 일은 `WallFrameCoordinator::arm_keep_previous` 가 거는 ***셸의 건너뛰기 지시*** 하나
    /// 뿐이고, 그 지시의 유일한 독자는 `keep_previous_logical_frame` 이다. 배리어 쪽에는
    /// 아무것도 들어오지 않는다. 자세한 것은 2026-10-22 Task 9.5 보고서 부록 A-3.
    ///
    /// 그 결과가 두 가지다:
    ///
    /// 1. ★생성한 타일이 하나 이하면 배리어 자체가 없다★(아래 이른 반환, 그리고 호출자의
    ///    `generated_painter_ids.len() > 1`). 그 주기에는 묶을 것이 없으므로 그 하나가
    ///    자기 frame-ready 를 받는 대로 표출한다.
    /// 2. ★어떤 타일이든 거절하면 그 주기에 화면이 섞인다.★ 아래 표.
    ///
    /// ## ★언제 N 과 N-1 이 섞이나★ -- 다음 사람이 필요로 할 사실
    ///
    /// 거절한 타일은 배리어 밖이라 ***생성한 타일들과 같은 순간에 올라가지 않는다***.
    /// 무장 여부는 "섞이느냐" 가 아니라 ★어느 쪽 낡은 그림을 보이느냐★ 만 바꾼다:
    ///
    /// | 거절 타일의 상태 | 그 주기에 화면에 있는 것 |
    /// |---|---|
    /// | 무장됨(`WaitingOnCanvasImages`) | 셸이 렌더를 건너뛴다 -- ★마지막으로 표출한 것★ |
    /// | 무장 안 됨(`RendererBehind`, 2026-10-22 Task 9.5) | 셸이 렌더한다 -- ★자기가 직전에 낸 프레임★(= N-1) |
    ///
    /// 둘 다 생성한 타일들의 N 과 다르다. ★무장을 빼는 것이 섞임을 만든 것이 아니다★ --
    /// 무장이 있을 때는 더 ***낡은*** 그림이 올라갔고, 그 타일이 이미 지은 프레임은 버려졌다.
    ///
    /// 지속 시간: 거절 1 회당 벽 프레임 1 장이다. 다음 벽 프레임이
    /// `expire_keep_previous_before` 로 지시를 걷고 그 타일을 다시 묻기 때문이다. 거절이
    /// 연속되면 그만큼 이어진다.
    ///
    /// ★이 `len() <= 1` 게이트는 이 계획이 만든 것이 아니다★ -- 2026-10-22 이전부터 있었다.
    fn register_frame(
        &mut self,
        logical_frame_id: u64,
        webview_ids: Vec<WebViewId>,
        expected_painter_ids: Vec<PainterId>,
        requested_at: Instant,
    ) {
        if expected_painter_ids.len() <= 1 {
            return;
        }

        let injected_delayed_painter_id = self.delay_injection.as_mut().and_then(|injection| {
            injection.target_for_frame(logical_frame_id, &expected_painter_ids)
        });
        if let Some(injected_delayed_painter_id) = injected_delayed_painter_id {
            warn!(
                "Wall frame delay injection scheduled: logical_frame_id={} delayed_painter={:?} \
                 expected={:?}",
                logical_frame_id, injected_delayed_painter_id, expected_painter_ids
            );
        }

        if self
            .barriers
            .insert(
                logical_frame_id,
                WallFrameBarrier::new(
                    webview_ids.clone(),
                    expected_painter_ids.clone(),
                    requested_at,
                    injected_delayed_painter_id,
                ),
            )
            .is_some()
        {
            warn!(
                "Replacing existing wall frame barrier for logical_frame_id={logical_frame_id}; \
                 expected={expected_painter_ids:?}"
            );
        }

        for webview_id in webview_ids {
            self.active_webview_frames
                .insert(webview_id, logical_frame_id);
        }
    }

    fn webview_has_active_frame(&self, webview_id: WebViewId) -> bool {
        self.active_webview_frames.contains_key(&webview_id)
    }

    fn remove_webview(&mut self, webview_id: WebViewId) {
        self.active_webview_frames.remove(&webview_id);
        self.barriers
            .retain(|_, barrier| !barrier.webview_ids.contains(&webview_id));
    }

    fn release_active_webviews_for_barrier(
        &mut self,
        logical_frame_id: u64,
        webview_ids: &[WebViewId],
    ) -> Vec<WebViewId> {
        let mut released_webview_ids = Vec::new();
        for webview_id in webview_ids {
            if self
                .active_webview_frames
                .get(webview_id)
                .is_some_and(|active_logical_frame_id| *active_logical_frame_id == logical_frame_id)
            {
                self.active_webview_frames.remove(webview_id);
                released_webview_ids.push(*webview_id);
            }
        }
        released_webview_ids
    }

    fn note_frame_ready(
        &mut self,
        diagnostic: &FrameReadyDiagnostic,
    ) -> Option<WallFrameCoordinatorUpdate> {
        let Some(logical_frame_id) = diagnostic.wall_logical_frame_id else {
            return None;
        };

        if !self.barriers.contains_key(&logical_frame_id) {
            debug!(
                "Ignoring wall frame barrier readiness for unregistered logical_frame_id={} \
                 painter={:?} local_frame_id={}",
                logical_frame_id, diagnostic.painter_id, diagnostic.local_frame_id
            );
            return None;
        }

        let mut update = WallFrameCoordinatorUpdate::default();
        let missed_before_recording_ready =
            self.barriers.get(&logical_frame_id).is_some_and(|barrier| {
                !barrier.presentation_decision_made
                    && barrier.missed_deadline_at(diagnostic.ready_at)
            });
        if missed_before_recording_ready {
            update.extend(self.miss_barrier(logical_frame_id, diagnostic.ready_at));
        }

        let mut complete_before_deadline = false;
        {
            let Some(barrier) = self.barriers.get_mut(&logical_frame_id) else {
                return Some(update);
            };

            if !barrier
                .expected_painter_ids
                .contains(&diagnostic.painter_id)
            {
                warn!(
                    "Ignoring wall frame barrier readiness for unexpected target: \
                     logical_frame_id={} painter={:?} expected={:?}",
                    logical_frame_id, diagnostic.painter_id, barrier.expected_painter_ids
                );
                return Some(update);
            }

            if barrier.injected_delayed_painter_id == Some(diagnostic.painter_id)
                && !barrier.injected_ready_suppressed
            {
                barrier.injected_ready_suppressed = true;
                warn!(
                    "Wall frame delay injection: logical_frame_id={} painter={:?} \
                     local_frame_id={} ready withheld from barrier \
                     policy=simulate-delayed-renderer",
                    logical_frame_id, diagnostic.painter_id, diagnostic.local_frame_id
                );
                return Some(update);
            }

            if !barrier.ready_painter_ids.contains(&diagnostic.painter_id) {
                barrier.ready_painter_ids.push(diagnostic.painter_id);
            }
            barrier.need_repaint |= diagnostic.need_repaint;
            if barrier.first_ready_at.is_none() {
                barrier.first_ready_at = Some(diagnostic.ready_at);
            }

            if barrier.ready_painter_ids.len() == barrier.expected_painter_ids.len() {
                barrier.completed_at = Some(diagnostic.ready_at);
                if barrier.missed_deadline {
                    let first_to_all_ready_ms = barrier
                        .first_ready_at
                        .map(|first_ready_at| {
                            diagnostic
                                .ready_at
                                .saturating_duration_since(first_ready_at)
                                .as_secs_f64()
                                * 1000.0
                        })
                        .unwrap_or_default();
                    let request_to_all_ready_ms = diagnostic
                        .ready_at
                        .saturating_duration_since(barrier.requested_at)
                        .as_secs_f64()
                        * 1000.0;

                    info!(
                        "Wall frame barrier complete: logical_frame_id={} \
                         status=completed_after_deadline ready={}/{} \
                         first_to_all_ready_ms={:.3} request_to_all_ready_ms={:.3} \
                         final_painter={:?} final_local_frame_id={} final_wait_ms={:.3} \
                         need_repaint={} policy=keep-previous-frame-for-delayed-targets",
                        logical_frame_id,
                        barrier.ready_painter_ids.len(),
                        barrier.expected_painter_ids.len(),
                        first_to_all_ready_ms,
                        request_to_all_ready_ms,
                        diagnostic.painter_id,
                        diagnostic.local_frame_id,
                        diagnostic.wait_ms,
                        barrier.need_repaint,
                    );
                } else if !barrier.presentation_decision_made {
                    complete_before_deadline = true;
                }
            } else if !barrier.presentation_decision_made {
                debug!(
                    "Wall frame barrier progress: logical_frame_id={} ready={}/{} \
                     painter={:?} local_frame_id={} wait_ms={:.3} missing={:?}",
                    logical_frame_id,
                    barrier.ready_painter_ids.len(),
                    barrier.expected_painter_ids.len(),
                    diagnostic.painter_id,
                    diagnostic.local_frame_id,
                    diagnostic.wait_ms,
                    barrier.missing_painter_ids(),
                );
            }
        }

        if complete_before_deadline {
            update.extend(self.complete_barrier(logical_frame_id, diagnostic));
        }

        Some(update)
    }

    fn complete_barrier(
        &mut self,
        logical_frame_id: u64,
        diagnostic: &FrameReadyDiagnostic,
    ) -> WallFrameCoordinatorUpdate {
        let Some(barrier) = self.barriers.get_mut(&logical_frame_id) else {
            return WallFrameCoordinatorUpdate::default();
        };
        if barrier.presentation_decision_made {
            return WallFrameCoordinatorUpdate::default();
        }

        barrier.presentation_decision_made = true;
        barrier.completed_at = Some(diagnostic.ready_at);
        let webview_ids = barrier.webview_ids.clone();
        let painter_ids = barrier.expected_painter_ids.clone();
        let repaint_needed = barrier.need_repaint;
        let ready_len = barrier.ready_painter_ids.len();
        let expected_len = barrier.expected_painter_ids.len();
        let first_to_all_ready_ms = barrier
            .first_ready_at
            .map(|first_ready_at| {
                diagnostic
                    .ready_at
                    .saturating_duration_since(first_ready_at)
                    .as_secs_f64()
                    * 1000.0
            })
            .unwrap_or_default();
        let request_to_all_ready_ms = diagnostic
            .ready_at
            .saturating_duration_since(barrier.requested_at)
            .as_secs_f64()
            * 1000.0;

        for painter_id in &painter_ids {
            if self
                .keep_previous_painters
                .get(painter_id)
                .is_some_and(|frame| frame.remove_after_skip_query)
            {
                continue;
            }
            self.keep_previous_painters.remove(painter_id);
        }

        info!(
            "Wall frame barrier complete: logical_frame_id={} status=completed_before_deadline \
             ready={}/{} first_to_all_ready_ms={:.3} request_to_all_ready_ms={:.3} \
             final_painter={:?} final_local_frame_id={} final_wait_ms={:.3} \
             need_repaint={} policy=present-current-frame",
            logical_frame_id,
            ready_len,
            expected_len,
            first_to_all_ready_ms,
            request_to_all_ready_ms,
            diagnostic.painter_id,
            diagnostic.local_frame_id,
            diagnostic.wait_ms,
            repaint_needed,
        );

        let repaint_decisions = painter_ids
            .into_iter()
            .map(|painter_id| WallFrameRepaintDecision {
                painter_id,
                repaint_needed,
            })
            .collect();
        WallFrameCoordinatorUpdate {
            repaint_decisions,
            released_webview_ids: self
                .release_active_webviews_for_barrier(logical_frame_id, &webview_ids),
        }
    }

    fn miss_barrier(&mut self, logical_frame_id: u64, now: Instant) -> WallFrameCoordinatorUpdate {
        let Some(barrier) = self.barriers.get_mut(&logical_frame_id) else {
            return WallFrameCoordinatorUpdate::default();
        };
        if barrier.presentation_decision_made {
            return WallFrameCoordinatorUpdate::default();
        }

        barrier.missed_deadline = true;
        barrier.presentation_decision_made = true;
        let webview_ids = barrier.webview_ids.clone();
        let ready_painter_ids = barrier.ready_painter_ids.clone();
        let missing_painter_ids = barrier.missing_painter_ids();
        let ready_len = barrier.ready_painter_ids.len();
        let expected_len = barrier.expected_painter_ids.len();
        let repaint_needed = barrier.need_repaint;
        let first_ready_elapsed_ms = barrier
            .first_ready_at
            .map(|first_ready_at| {
                now.saturating_duration_since(first_ready_at).as_secs_f64() * 1000.0
            })
            .unwrap_or_default();

        for painter_id in &ready_painter_ids {
            self.keep_previous_painters.remove(painter_id);
        }
        for painter_id in &missing_painter_ids {
            let remove_after_skip_query = barrier.injected_delayed_painter_id == Some(*painter_id);
            self.keep_previous_painters.insert(
                *painter_id,
                KeepPreviousFrame {
                    logical_frame_id,
                    remove_after_skip_query,
                },
            );
            if remove_after_skip_query {
                warn!(
                    "Wall frame delay injection: armed keep-previous skip observation for \
                     painter={:?} logical_frame_id={}",
                    painter_id, logical_frame_id
                );
            }
        }

        warn!(
            "Wall frame barrier missed: logical_frame_id={} ready={}/{} missing={:?} \
             first_ready_elapsed_ms={:.3} deadline_ms={:.3} \
             need_repaint={} policy=keep-previous-frame-for-delayed-targets",
            logical_frame_id,
            ready_len,
            expected_len,
            missing_painter_ids,
            first_ready_elapsed_ms,
            WALL_FRAME_BARRIER_DEADLINE.as_secs_f64() * 1000.0,
            repaint_needed,
        );

        let repaint_decisions = ready_painter_ids
            .into_iter()
            .map(|painter_id| WallFrameRepaintDecision {
                painter_id,
                repaint_needed,
            })
            .collect();
        WallFrameCoordinatorUpdate {
            repaint_decisions,
            released_webview_ids: self
                .release_active_webviews_for_barrier(logical_frame_id, &webview_ids),
        }
    }

    fn sweep_expired_barriers(&mut self, now: Instant) -> WallFrameCoordinatorUpdate {
        let expired_logical_frame_ids: Vec<_> = self
            .barriers
            .iter()
            .filter_map(|(logical_frame_id, barrier)| {
                (!barrier.presentation_decision_made && barrier.missed_deadline_at(now))
                    .then_some(*logical_frame_id)
            })
            .collect();

        let mut update = WallFrameCoordinatorUpdate::default();
        for logical_frame_id in expired_logical_frame_ids {
            update.extend(self.miss_barrier(logical_frame_id, now));
        }

        self.barriers.retain(|_, barrier| {
            if let Some(completed_at) = barrier.completed_at {
                return now.saturating_duration_since(completed_at) <= WALL_FRAME_BARRIER_RETENTION;
            }

            if barrier.missed_deadline {
                return barrier.first_ready_at.is_some_and(|first_ready_at| {
                    now.saturating_duration_since(first_ready_at) <= WALL_FRAME_BARRIER_RETENTION
                });
            }

            true
        });

        update
    }

    /// Drop keep-previous instructions that a newer wall frame has made moot.
    ///
    /// ***A keep-previous entry is about ONE logical frame -- it carries that frame's id --
    /// but nothing was comparing it.*** `render_all_tiles` only asks whether an entry exists
    /// (`main.rs`, `paint_target_keep_previous_logical_frame(..).is_some()`), so "keep the
    /// previous frame for logical frame N" was being obeyed forever.
    ///
    /// That turned one missed barrier into a permanently dark tile. The entry is otherwise
    /// removed only while walking a barrier's painter list (on complete, or for the painters
    /// that WERE ready on a miss), and a skipped painter renders nothing, so it never becomes
    /// ready, so it drops out of later barriers -- measured 2026-08-31 as `ready=2/4` then
    /// `2/3` then `1/2` -- and its entry is never reached again. One-way door.
    ///
    /// Expiring on the next wall frame restores the intended scope. The painters that still
    /// have nothing new simply get presented with what they already had, which is what any
    /// tile outside a barrier does anyway; the skip only ever saved a redundant present.
    ///
    /// Entries armed by the failure-injection path are left alone: that test exists to
    /// observe the skip, so consuming it here would defeat it.
    ///
    /// ★2026-10-22 Task 8 부터 만료는 드문 일이 아니다.★ 무장 지점이 둘이 됐고
    /// (`miss_barrier`, `arm_keep_previous`), 그중 뒤쪽은 캔버스 이미지를 기다리는 타일에서
    /// 주기마다 일어날 수 있다. 질의는 지시를 소비해도 **지우지 않으므로**(실패 주입 제외)
    /// 거절한 타일의 지시는 거의 언제나 다음 벽 프레임의 이 자리에서 걷힌다 -- 그것이 유지
    /// 지시가 한 프레임짜리임을 보장하는 자리다(불변식 14). 아래 경고문이 "배리어를
    /// 놓쳤다" 고 단정하지 않는 이유다.
    ///
    /// ★"역압 거절" 이라고 적혀 있던 것을 2026-10-22 에 바로잡았다★ -- 역압 거절은
    /// 무장되지 않는다(`wall_tick::decline_may_arm_keep_previous`). 그 주장은 철회된 설계
    /// 2-5 의 잔재다.
    fn expire_keep_previous_before(&mut self, logical_frame_id: u64, painter_ids: &[PainterId]) {
        for painter_id in painter_ids {
            let stale = self
                .keep_previous_painters
                .get(painter_id)
                .is_some_and(|frame| {
                    !frame.remove_after_skip_query && frame.logical_frame_id < logical_frame_id
                });
            if !stale {
                continue;
            }
            let previous = self.keep_previous_painters.remove(painter_id);
            self.keep_previous_expired_count += 1;
            let total = self.keep_previous_expired_count;
            // Same rate-limit shape as the pacing summary: a tile that misses every barrier
            // -- or waits on canvas images every tick -- would otherwise warn once per
            // wall frame.
            //
            // ★문자열에 "declined on back-pressure" 가 있었다 -- 2026-10-22 에 고쳤다.★
            // 역압 거절은 무장되지 않으므로(`wall_tick::decline_may_arm_keep_previous`)
            // 이 줄에 그 사유로 들어오는 타일은 없다. 로그로 나가는 문자열이라 읽는 사람을
            // 곧장 틀린 원인으로 보냈다.
            if total <= 3 || total % WALL_FRAME_PACING_INFO_INTERVAL == 0 {
                warn!(
                    "Wall frame keep-previous expired: painter={:?} kept_for_logical_frame_id={} \
                     superseded_by={} total_expired={}. The painter did not generate that frame \
                     (missed barrier, or declined while waiting on canvas images) and would \
                     otherwise have been skipped by the embedder for the rest of the session.",
                    painter_id,
                    previous
                        .map(|frame| frame.logical_frame_id)
                        .unwrap_or_default(),
                    logical_frame_id,
                    total,
                );
            }
        }
    }

    /// 거절한 타일에 ★이 논리 프레임 동안 직전 프레임을 유지하라고 지시한다.★
    ///
    /// ## ★이 함수가 하는 일은 하나뿐이다 -- 셸의 건너뛰기 지시★
    ///
    /// 세운 지시를 읽는 자리는 `keep_previous_logical_frame` 하나이고, 그것을 묻는 것은
    /// 임베더다(`winit_wall/main.rs` 의 `render_all_tiles`, servoshell `window.rs` 의
    /// `repaint_webviews`). `Some` 이면 그 타일의 `paint_target` 과 `present` 를 ★둘 다★
    /// 건너뛴다.
    ///
    /// ★무장 지점은 ***둘***이다 -- 이 함수가 유일하지 않다.★
    ///
    /// - 이 함수 -- 거절한 타일. 호출자에서 게이트 둘을 지난다(아래).
    /// - `WallFrameCoordinator::miss_barrier` -- 배리어 마감(16ms)을 놓친 타일.
    ///   ★`keep_previous_painters` 에 ***직접*** insert 하므로 이 함수를 지나지 않고,
    ///   따라서 아래 게이트 둘도 지나지 않는다.★ 잔여 위험은 맨 아래.
    ///
    /// 나머지 세 자리(`complete_barrier`, `expire_keep_previous_before`,
    /// `release_keep_previous_while_production_is_suspended`)는 ***지우기만*** 한다.
    /// `miss_barrier` 는 ★지우기도 하고 걸기도 한다★ -- 그 프레임에 ready 였던 타일은
    /// 지우고, 빠진 타일은 건다.
    ///
    /// ★배리어(`register_frame`)에는 넣지 않는다.★ 배리어가 기다리는 것은 frame-ready 이고
    /// 거절한 타일은 트랜잭션을 안 보냈으므로 그 신호를 영영 안 낸다 -- 넣으면 그 논리
    /// 프레임은 마감까지 안 닫힌다(`TileOutcome::KeepPrevious` 주석).
    ///
    /// ## ★설계 2-5 의 요구는 2026-10-22 현재 미구현이다★
    ///
    /// 2-5 는 거절한 타일이 keep-previous 로 ***배리어에 참여***하여 "벽 프레임이 언제나 전
    /// 타일을 덮게" 하라고 한다. 위에서 보듯 ★참여는 일어나지 않는다★ -- 거절한 타일은
    /// 배리어 밖이고, 생성한 타일이 하나 이하면 배리어 자체가 없다. 그래서 어떤 타일이든
    /// 거절하면 그 주기에 화면이 섞인다. ★무장 여부는 섞이느냐가 아니라 어느 쪽 낡은
    /// 그림을 보이느냐만 바꾼다★ -- 표와 지속 시간은 `register_frame` 의 주석에 있고,
    /// 왜 이 계획에서 고치지 않았는지는 Task 9.5 보고서 부록 A-3 에 있다.
    ///
    /// ## 무장 자격은 ★여기서 묻지 않는다★
    ///
    /// 호출자(`Paint::issue_wall_frame_request`)가 게이트 둘을 이미 지났다:
    /// `wall_tick::decline_may_arm_keep_previous`(자기잠금인 `RendererBehind` 제외)와
    /// `wall_tick::wall_frame_arms_keep_previous`(타일 하나짜리 벽 제외). 2026-10-22 Task 9.5.
    ///
    /// ## ★잔여 위험 -- `miss_barrier` 의 무게이트 무장★
    ///
    /// 위 게이트 둘은 ***이 함수를 지나는 무장에만*** 걸린다. `miss_barrier` 는 직접
    /// insert 하므로 ★사유도 타일 수도 묻지 않는다★. 그래서 같은 닫힌 고리가 그쪽으로
    /// 재현될 수 있다:
    ///
    /// ```text
    /// 다중 타일 수요 구동 셸에서 ***전 타일***이 배리어 마감을 놓친다
    ///   -> miss_barrier 가 전 타일을 무장 -> 셸이 전 타일의 렌더를 건너뛴다
    ///   -> display_composite_in_flight 가 아무 데서도 안 풀린다
    ///   -> 렌더가 없으니 rAF 도 `GenerateFrame` 도 없다 -> 예약도 틱도 없다
    ///   -> 무장을 걷을 다음 벽 프레임이 영영 안 온다
    /// ```
    ///
    /// 마감을 놓친 타일은 ***프레임을 냈지만 frame-ready 가 안 온*** 상태라
    /// `RendererBehind` 와 같은 모양이다. 자유 실행 벽은 매 주기 틱이 돌아 다음 프레임이
    /// 무장을 걷으므로 한 틱으로 끝나고, 수요 구동 셸에서도 타일 하나라도 마감을 지키면
    /// 그 타일의 렌더가 고리를 푼다. ★전 타일 동시 미스에서만 닫힌다.★
    ///
    /// ★2026-10-22 Task 9.5 에서 거동은 바꾸지 않았다★ -- 배리어 의미를 건드리는 변경이고
    /// 검증할 실기 벽이 없다(같은 이유로 설계 2-5 의 배리어 참여도 미구현으로 남겼다).
    /// 여기서 재현되면 `miss_barrier` 의 insert 에도 같은 게이트 둘을 끼우면 된다.
    ///
    /// ★"표출한 적 있는가" 는 여기서 보지 않는다.★ 그 면제는
    /// `keep_previous_logical_frame` 한 곳에만 둔다(불변식 14). 무장 지점이 둘이 된 지금
    /// 면제를 복사하면 두 벌이 어긋날 수 있고, 질의가 어차피 모든 지시를 지난다.
    ///
    /// 실패 주입이 세워 둔 관찰용 지시는 건드리지 않는다 --
    /// `expire_keep_previous_before` 와 같은 이유다(그 주석 마지막 문단).
    fn arm_keep_previous(&mut self, logical_frame_id: u64, painter_ids: &[PainterId]) {
        for painter_id in painter_ids {
            if self
                .keep_previous_painters
                .get(painter_id)
                .is_some_and(|frame| frame.remove_after_skip_query)
            {
                continue;
            }
            self.keep_previous_painters.insert(
                *painter_id,
                KeepPreviousFrame {
                    logical_frame_id,
                    remove_after_skip_query: false,
                },
            );
        }
    }

    /// ★생산을 쉬는 틱이 keep-previous 지시를 걷는다 -- 교착 방지.★ 2026-10-22 Task 9.
    ///
    /// 무장한 타일은 셸이 `paint_target` 과 `present` 를 ***둘 다*** 건너뛴다
    /// (`keep_previous_logical_frame`, `winit_wall/main.rs`). 그런데 스크린샷을 실제로
    /// 찍는 자리는 ★렌더 패스의 끝★ 이다(`Painter::render` ->
    /// `ScreenshotTaker::maybe_take_screenshots`). 요청을 쥔 타일이 무장한 채 생산이
    /// 멈추면 그 타일은 영영 렌더되지 않고, 지시를 걷는 다른 자리
    /// (`expire_keep_previous_before`)는 ***다음 생산***에 있다 -- 생산이 쉬는 이유가
    /// 스크린샷이므로 그대로 영구 정지다.
    ///
    /// ★닿을 수 있는 상태다.★ 둘 다 생산 밖에서 일어난다:
    ///
    /// - `handle_screenshot_readiness_reply` 가 디스플레이 리스트가 이미 다 와 있는 경우
    ///   그 자리에서 `WaitingOnFrame` 까지 민다. 그 순간 요청을 쥔 타일이 직전 벽
    ///   프레임에서 거절했다면 이미 무장해 있다.
    /// - `miss_barrier` 는 배리어 마감(16ms)에서 돌고, ***생산을 쉬는 동안에도*** 빠진
    ///   타일을 새로 무장시킨다.
    ///
    /// 그래서 한 번이 아니라 ★쉬는 틱마다★ 걷는다 -- 쉬는 중에 새로 생긴 무장도 다음
    /// 틱(8.3ms 뒤)에 걷힌다.
    ///
    /// ★화면상 손해는 없다.★ 쉬는 동안에는 어느 타일도 새 프레임을 내지 않으므로, 걷힌
    /// 타일이 다시 그려도 올라가는 내용은 같다. 불변식 14("지명된 논리 프레임에만 유효")
    /// 와도 어긋나지 않는다 -- 걷는 것은 유효 범위를 ***좁히는*** 쪽이다.
    ///
    /// 실패 주입이 세워 둔 관찰용 지시는 건드리지 않는다 -- `expire_keep_previous_before`
    /// 와 같은 이유다.
    fn release_keep_previous_while_production_is_suspended(
        &mut self,
        painter_ids: &[PainterId],
    ) -> usize {
        let mut released = 0;
        for painter_id in painter_ids {
            if self
                .keep_previous_painters
                .get(painter_id)
                .is_some_and(|frame| frame.remove_after_skip_query)
            {
                continue;
            }
            if self.keep_previous_painters.remove(painter_id).is_some() {
                released += 1;
            }
        }
        released
    }

    /// `render_all_tiles` 가 타일마다 패스에 한 번 묻는다. `Some` 이면 그 타일의
    /// `paint_target` 과 `present` 를 **둘 다** 건너뛴다(`main.rs`).
    ///
    /// ★`None` 을 돌려주는 것은 "이 타일이 이번 패스에 표출한다" 와 같은 말이다.★ 그래서
    /// 여기가 "한 번이라도 표출했는가" 를 기록할 자리다 -- 질의자가 곧 그릴 것이므로 다른
    /// 신호를 따로 받을 필요가 없다.
    fn keep_previous_logical_frame(&mut self, painter_id: PainterId) -> Option<u64> {
        let Some(keep_previous_frame) = self.keep_previous_painters.get(&painter_id) else {
            self.presented_painters.insert(painter_id);
            return None;
        };
        let logical_frame_id = keep_previous_frame.logical_frame_id;
        let remove_after_skip_query = keep_previous_frame.remove_after_skip_query;
        if remove_after_skip_query {
            // 실패 주입은 **스킵을 관찰하는 것이 목적**이므로 아래 면제보다 앞선다. 그걸
            // 면제해 버리면 주입이 재현하려던 바로 그 상태를 못 보게 된다.
            self.keep_previous_painters.remove(&painter_id);
            warn!(
                "Wall frame delay injection: consumed keep-previous skip observation for \
                 painter={:?} logical_frame_id={}",
                painter_id, logical_frame_id
            );
            return Some(logical_frame_id);
        }

        // ★유지할 직전 프레임이 없는 타일에게 유지하라고 말하지 않는다.★
        //
        // DComp 경로에서 화면에 남아 있는 것은 마지막으로 Commit 된 내용뿐이고(타일 창의
        // `present()` 는 DComp 가 뜨면 no-op 다 -- `rendering_context.rs`), 아직 한 번도
        // 커밋하지 않은 타일은 빈 서피스를 보인다. 그 타일을 건너뛰면 "직전 프레임 유지" 가
        // 아니라 **검은 타일**이다.
        //
        // 기동 때만 터지는 이유: 배리어 마감이 16ms(`WALL_FRAME_BARRIER_DEADLINE`)인데 첫
        // 렌더는 셰이더 컴파일로 46~94ms 다(2026-10-20 실측). 첫 몇 프레임은 마감을 지킬
        // 수가 없고, 그 사이 벽에는 아무것도 커밋된 적이 없다. 같은 자리의 옛 주석이
        // "permanently dark tile" 이라 부른 것의 첫 한 조각이 이것이다 --
        // `expire_keep_previous_before` 가 영구화만 막았고, 첫 발생은 남아 있었다.
        if !self.presented_painters.contains(&painter_id) {
            self.keep_previous_painters.remove(&painter_id);
            self.presented_painters.insert(painter_id);
            info!(
                "Wall frame barrier: drawing {painter_id:?} instead of holding \
                 logical_frame_id={logical_frame_id} -- it has never presented, so there is no \
                 previous frame to hold policy=draw-first-frame-for-never-presented-target"
            );
            return None;
        }

        Some(logical_frame_id)
    }
}

/// Why we need to be repainted. This is used for debugging.
#[derive(Clone, Copy, Default, PartialEq)]
pub(crate) struct RepaintReason(u8);

bitflags! {
    impl RepaintReason: u8 {
        /// We're performing the single repaint in headless mode.
        const ReadyForScreenshot = 1 << 0;
        /// We're performing a repaint to run an animation.
        const ChangedAnimationState = 1 << 1;
        /// A new WebRender frame has arrived.
        const NewWebRenderFrame = 1 << 2;
        /// The window has been resized and will need to be synchronously repainted.
        const Resize = 1 << 3;
        /// A fling has started and a repaint needs to happen to process the animation.
        const StartedFlinging = 1 << 4;
        /// A blinking text caret requires a redraw.
        const BlinkingCaret = 1 << 5;
    }
}

/// `UpdateImages` 한 건의 시간을 팬아웃 바깥과 페인터 안으로 가른 누계(초당 한 줄).
///
/// 드레인이 메시지 77건에 45.7ms 였다 — 건당 0.59ms 다. 건수가 적은데 건당이 비싸면
/// 줄이는 쪽이 아니라 안을 봐야 하고, 그 안이 어디인지는 재지 않으면 모른다.
/// 이 드레인은 임베더 메인 스레드에서만 돌므로 thread_local 로 족하다.
#[derive(Default)]
struct ImageUpdateStats {
    window_start: Option<Instant>,
    calls: u64,
    total_ms: f64,
    inner_ms: f64,
}

thread_local! {
    static IMAGE_UPDATE_STATS: RefCell<ImageUpdateStats> = RefCell::new(ImageUpdateStats::default());
    /// 큐가 깊어 보내지 않은 이미지 갱신 수(초당 한 줄에 실어 낸다).
    static DETACHED_DROPPED: Cell<u64> = const { Cell::new(0) };
}

impl Paint {
    pub fn new(state: InitialPaintState) -> Rc<RefCell<Self>> {
        let registration = state.mem_profiler_chan.prepare_memory_reporting(
            "paint".into(),
            state.paint_proxy.clone(),
            PaintMessage::CollectMemoryReport,
        );

        let webrender_external_image_id_manager = WebRenderExternalImageIdManager::default();
        let painter_surfman_details_map = PainterSurfmanDetailsMap::default();
        let WebGLComm {
            webgl_threads,
            swap_chains,
            busy_webgl_context_map,
            #[cfg(feature = "webxr")]
            webxr_layer_grand_manager,
        } = WebGLComm::new(
            state.paint_proxy.cross_process_paint_api.clone(),
            webrender_external_image_id_manager.clone(),
            painter_surfman_details_map.clone(),
        );

        // Create the WebXR main thread
        #[cfg(feature = "webxr")]
        let webxr_main_thread = {
            use servo_config::pref;

            let mut webxr_main_thread = webxr::MainThreadRegistry::new(
                state.event_loop_waker.clone(),
                webxr_layer_grand_manager,
            )
            .expect("Failed to create WebXR device registry");
            if pref!(dom_webxr_enabled) {
                state.webxr_registry.register(&mut webxr_main_thread);
            }
            webxr_main_thread
        };

        Rc::new(RefCell::new(Paint {
            painters: Default::default(),
            paint_proxy: state.paint_proxy,
            event_loop_waker: state.event_loop_waker,
            shutdown_state: state.shutdown_state,
            paint_receiver: state.receiver,
            embedder_to_constellation_sender: state.embedder_to_constellation_sender.clone(),
            webrender_external_image_id_manager,
            webgl_threads,
            swap_chains,
            time_profiler_chan: state.time_profiler_chan,
            _mem_profiler_registration: registration,
            painter_surfman_details_map,
            busy_webgl_contexts_map: busy_webgl_context_map,
            #[cfg(feature = "webxr")]
            webxr_main_thread: RefCell::new(webxr_main_thread),
            #[cfg(feature = "webgpu")]
            webgpu_image_map: Default::default(),
            webview_painter_targets: Default::default(),
            next_logical_frame_id: Default::default(),
            webviews: Default::default(),
            wall_frame_coordinator: RefCell::new(WallFrameCoordinator::from_environment()),
            wall_frame_pacing_config: WallFramePacingConfig::from_prefs(),
            coalesced_wall_frame_requests: Default::default(),
            last_wall_frame_issue_at: Default::default(),
            wall_frame_pacing_next_wake_at: Default::default(),
            wall_frame_pacing_coalesced_count: Default::default(),
            wall_frame_pacing_released_count: Default::default(),
            screenshot_poll_latch: Default::default(),
            production_suspended_in_window: Default::default(),
            production_suspended_total: Default::default(),
            production_suspended_last_report: Default::default(),
            keep_previous_released_in_window: Default::default(),
            demand_tick_edges_in_window: Default::default(),
            demand_tick_produced_in_window: Default::default(),
            demand_tick_window_start: Default::default(),
            wall_frame_pacing_block_tally: Default::default(),
        }))
    }

    pub fn register_rendering_context(
        &mut self,
        rendering_context: Rc<dyn RenderingContext>,
    ) -> PainterId {
        if let Some(painter_id) = self
            .painters
            .iter()
            .find(|host| {
                host.rendering_context
                    .as_ref()
                    .is_some_and(|context| Rc::ptr_eq(context, &rendering_context))
            })
            .map(|host| host.painter_id)
        {
            return painter_id;
        }

        let painter = Painter::new(rendering_context.clone(), self.painter_inputs());
        if let Some(gpu_index) = rendering_context.requested_gpu_index() {
            debug!(
                "Registering painter {:?} with requested target GPU index {gpu_index}",
                painter.painter_id,
            );
        }
        let connection = rendering_context
            .connection()
            .expect("Failed to get connection");
        let adapter =
            create_adapter_for_requested_gpu(&connection, rendering_context.requested_gpu_index())
                .expect("Failed to create adapter");

        let painter_surfman_details = PainterSurfmanDetails {
            connection,
            adapter,
        };
        self.painter_surfman_details_map
            .insert(painter.painter_id, painter_surfman_details);

        let painter_id = painter.painter_id;
        self.painters.push(PainterHost {
            painter_id,
            rendering_context: Some(rendering_context),
            kind: PainterHostKind::Inline(Rc::new(RefCell::new(painter))),
        });
        painter_id
    }

    /// 타일 스레드를 띄우고 그 스레드가 **자기 컨텍스트와 자기 `Painter` 를 직접 만들게**
    /// 한다. 만들어진 것을 옮기는 길은 없다 — `webrender::Renderer` 도 `RenderingContext` 도
    /// `!Send` 다(설계 §7-3, §3-1).
    ///
    /// `PainterId` 는 `Painter::new` 안에서 생기므로 만들어진 뒤에 돌려받는다. 여기서 한 번
    /// 기다리는 값은 컨텍스트 생성 비용뿐이고(스파이크 실측 218~305ms), 타일들을 잇달아
    /// 띄우면 그 대기는 서로 겹친다.
    ///
    /// 실패하면 `None` — 호출자는 인라인 경로로 되돌아갈 수 있어야 한다.
    pub fn spawn_painter_thread(&mut self, factory: RenderingContextFactory) -> Option<PainterId> {
        let inputs = self.painter_inputs();
        let surfman_details_map = self.painter_surfman_details_map.clone();
        let (requests, request_receiver) = crossbeam_channel::unbounded::<PainterRequest>();
        let (id_sender, id_receiver) = crossbeam_channel::bounded(1);

        let spawned = std::thread::Builder::new()
            .name(String::from("WallTilePainter"))
            .spawn(move || {
                let rendering_context = factory();

                // ★`Painter::new` 보다 먼저 등록해야 한다.★ WebGL 팬아웃은 이 표에 없는
                // painter 를 대상에서 조용히 걸러내므로, 늦게 넣으면 그 타일은 캔버스를
                // 영영 못 받는다(2026-09-03 `log_webgpu/64`~`66` 의 증상이 정확히 그것이다).
                let connection = rendering_context
                    .connection()
                    .expect("Failed to get the tile thread's surfman connection");
                let adapter = paint_api::rendering_context::create_adapter_for_requested_gpu(
                    &connection,
                    rendering_context.requested_gpu_index(),
                )
                .expect("Failed to create an adapter for the tile thread");

                let mut painter = Painter::new(rendering_context, inputs);
                surfman_details_map.insert(
                    painter.painter_id,
                    PainterSurfmanDetails {
                        connection,
                        adapter,
                    },
                );
                // 셸은 이 컨텍스트를 갖고 있지 않으므로 표출도 이 스레드가 한다.
                painter.take_over_presentation();
                if id_sender.send(painter.painter_id).is_err() {
                    // 아무도 안 기다린다 = 등록이 취소됐다. 컨텍스트를 이 스레드에서
                    // 소멸시키고 조용히 끝낸다(스레드 로컬 계약).
                    return;
                }
                while let Ok(request) = request_receiver.recv() {
                    match request {
                        PainterRequest::Run(work) => work(&mut painter),
                        PainterRequest::Exit => break,
                    }
                }
            });

        let join_handle = match spawned {
            Ok(handle) => handle,
            Err(error) => {
                warn!("Could not spawn a tile painter thread: {error:?}");
                return None;
            },
        };
        let painter_id = match id_receiver.recv() {
            Ok(painter_id) => painter_id,
            Err(_) => {
                warn!("Tile painter thread died before it could build its painter");
                let _ = join_handle.join();
                return None;
            },
        };

        self.painters.push(PainterHost {
            painter_id,
            rendering_context: None,
            kind: PainterHostKind::Threaded(ThreadedPainter {
                detached_in_flight: Arc::new(AtomicUsize::new(0)),
                requests,
                join_handle: Some(join_handle),
            }),
        });
        Some(painter_id)
    }

    /// 등록된 모든 `PainterId`. ★painter 를 빌리지 않는다★ — 전수 순회는 이 목록을 얻은 뒤
    /// `with_painter*` 로 하나씩 들어가는 형태여야 스레드 변형에서도 성립한다.
    fn painter_ids(&self) -> Vec<PainterId> {
        self.painters.iter().map(|host| host.painter_id).collect()
    }

    fn remove_painter(&mut self, painter_id: PainterId) {
        self.painters.retain(|host| host.painter_id != painter_id);
        self.painter_surfman_details_map.remove(painter_id);
    }

    /// `Painter` 에 닿는 통로. ★`Ref`/`RefMut` 를 돌려주지 않는 것이 요점이다.★
    ///
    /// 병렬 타일 렌더(A 안)에서 painter 는 **다른 스레드에 산다.** 빌린 참조를 호출자에게
    /// 건네는 형태는 그때 호출부까지 전부 다시 써야 하지만, 클로저로 좁혀 두면 **이 함수의
    /// 구현만** 채널 왕복으로 바꾸면 된다(설계 §3.2-1). 지금은 그대로 인라인 실행이므로
    /// 동작이 바뀌지 않는다.
    ///
    /// `None` 은 그 `PainterId` 가 없다는 뜻이고, 기존 `maybe_painter_mut` 의 `None` 과 같다.
    pub(crate) fn with_painter<R: Send + 'static>(
        &self,
        painter_id: PainterId,
        callback: impl FnOnce(&Painter) -> R + Send + 'static,
    ) -> Option<R> {
        let host = self
            .painters
            .iter()
            .find(|host| host.painter_id == painter_id)?;
        match &host.kind {
            PainterHostKind::Inline(painter) => Some(callback(&painter.borrow())),
            PainterHostKind::Threaded(threaded) => {
                threaded.run_blocking(move |painter| callback(painter))
            },
        }
    }

    /// 결과가 필요 없는 일을 페인터에 **보내고 기다리지 않는다.**
    ///
    /// ★기다리면 페인터가 렌더 중일 때 그 렌더 뒤에 줄을 선다★ — 실측(로그 43/44)에서
    /// `update_images` 는 함수 **안**이 초당 2~6ms 인데 감싼 시간은 263ms 였다. 즉 95% 가
    /// 일이 아니라 페인터 스레드를 기다린 시간이고, 평상시(렌더 3~8ms)에는 호출당 2µs 인
    /// 것이 애니메이션 때(렌더 50~85ms)는 250µs 가 된다. 영상 프레임은 초당 1,600건이
    /// 들어오므로 그 대기가 메인 스레드를, 따라서 표출 클럭을 통째로 먹는다.
    ///
    /// 순서는 유지된다 — 같은 큐로 들어가므로 뒤이은 blocking 호출도 이 뒤에 선다.
    pub(crate) fn with_painter_mut_detached(
        &self,
        painter_id: PainterId,
        callback: impl FnOnce(&mut Painter) + Send + 'static,
    ) {
        let Some(host) = self
            .painters
            .iter()
            .find(|host| host.painter_id == painter_id)
        else {
            return;
        };
        match &host.kind {
            // 스레드가 없으면 기다릴 것도 없다 — 예전과 같이 그 자리에서 돈다.
            PainterHostKind::Inline(painter) => callback(&mut painter.borrow_mut()),
            PainterHostKind::Threaded(threaded) => {
                let limit = servo_config::pref!(gfx_painter_detached_queue_limit).max(0) as usize;
                if !threaded.dispatch_detached(limit, callback) {
                    DETACHED_DROPPED.with(|dropped| dropped.set(dropped.get() + 1));
                }
            },
        }
    }

    pub(crate) fn with_painter_mut<R: Send + 'static>(
        &self,
        painter_id: PainterId,
        callback: impl FnOnce(&mut Painter) -> R + Send + 'static,
    ) -> Option<R> {
        let host = self
            .painters
            .iter()
            .find(|host| host.painter_id == painter_id)?;
        match &host.kind {
            PainterHostKind::Inline(painter) => Some(callback(&mut painter.borrow_mut())),
            PainterHostKind::Threaded(threaded) if *PAINTER_LOCK_PROF => {
                // ★콜백이 시작하는 자리에서 잰다★ -- 돌아온 뒤에 재면 콜백 실행 시간이 섞인다.
                let called_at = Instant::now();
                threaded.run_blocking(move |painter| {
                    record_painter_lock_wait(painter_id, called_at.elapsed());
                    callback(painter)
                })
            },
            PainterHostKind::Threaded(threaded) => threaded.run_blocking(callback),
        }
    }

    /// ★전 painter 에 같은 일을 보내고 **한꺼번에** 수거한다 -- 비용이 합이 아니라 최댓값이다.★
    ///
    /// `with_painter_mut` 를 루프로 돌면 타일마다 ***보내고 기다리므로*** 네 타일의 일이
    /// 줄을 서고, 비용이 합이 된다. 여기서는 스레드 타일에 먼저 전부 보내고(채널 send 뿐이라
    /// 즉시 돌아온다), 인라인 타일을 그 다음에 돌리고, 마지막에 결과를 모은다. 렌더 패스가
    /// 쓰는 것과 같은 형태이고 근거도 같다 -- [`ThreadedPainter::dispatch`] 의 주석:
    /// "보내고 바로 다음 타일을 보내면 네 번의 대기가 나란히 흐르고, 패스는 합이 아니라
    /// 최댓값이 된다".
    ///
    /// ★왜 틱 프롤로그에 이것이 필요한가★ -- 2026-10-22. 표출 틱이 단일 생산자가 되면서
    /// 셸 스레드가 매 틱 painter 를 ***세 번*** 훑게 됐다(`gather_components` /
    /// `issue_wall_frame_request` / `release_owed_canvas_acks`). 셋 다 블로킹 루프라
    /// 렌더 앞에 3N 번의 직렬 왕복이 생겼고, 그 각각이 painter 스레드에서 실제 일을 한다
    /// (DWM 통계 조회를 포함한다 -- `Painter::note_dcomp_frame_statistics`). 10-16 엔진에는
    /// 이 루프가 하나도 없었고 같은 비용이 타일별 렌더 안에서 ***나란히*** 흘렀다.
    ///
    /// ★순서는 보존된다.★ 돌려주는 벡터는 `painter_ids` 순서 그대로다 -- 배리어 등록과
    /// pacing 키가 실행마다 같아야 한다(`wall_frame_request_for_all_tiles`).
    ///
    /// 없는 painter 와 죽은 스레드는 `None` 이다 -- `with_painter_mut` 와 같은 뜻이다.
    fn with_each_painter_mut<R, C>(
        &self,
        painter_ids: &[PainterId],
        mut make_callback: impl FnMut(PainterId) -> C,
    ) -> Vec<Option<R>>
    where
        R: Send + 'static,
        C: FnOnce(&mut Painter) -> R + Send + 'static,
    {
        enum Slot<R, C> {
            /// 그런 painter 가 없거나 스레드가 죽었다.
            Missing,
            /// 인라인 타일. ★여기서 바로 돌리지 않는다★ -- 스레드 타일을 다 보낸 뒤라야
            /// 겹침을 깎지 않는다.
            Inline(Rc<RefCell<Painter>>, C),
            Waiting(crossbeam_channel::Receiver<R>),
        }

        // 1) 보내기만 한다. 이 루프 안에서 기다리면 팬아웃이 아니다.
        let mut slots = Vec::with_capacity(painter_ids.len());
        for painter_id in painter_ids {
            let painter_id = *painter_id;
            let callback = make_callback(painter_id);
            let Some(host) = self
                .painters
                .iter()
                .find(|host| host.painter_id == painter_id)
            else {
                slots.push(Slot::Missing);
                continue;
            };
            slots.push(match &host.kind {
                PainterHostKind::Inline(painter) => Slot::Inline(painter.clone(), callback),
                PainterHostKind::Threaded(threaded) if *PAINTER_LOCK_PROF => {
                    // `with_painter_mut` 와 같은 자리에서 잰다 -- 큐 대기만이고 콜백 실행은 뺀다.
                    let called_at = Instant::now();
                    match threaded.dispatch(move |painter| {
                        record_painter_lock_wait(painter_id, called_at.elapsed());
                        callback(painter)
                    }) {
                        Some(receiver) => Slot::Waiting(receiver),
                        None => Slot::Missing,
                    }
                },
                PainterHostKind::Threaded(threaded) => match threaded.dispatch(callback) {
                    Some(receiver) => Slot::Waiting(receiver),
                    None => Slot::Missing,
                },
            });
        }

        // 2) 인라인 타일을 돌린다. 스레드 타일들은 이미 나란히 돌고 있다.
        let mut results = Vec::with_capacity(slots.len());
        for slot in slots {
            results.push(match slot {
                Slot::Missing => None,
                Slot::Inline(painter, callback) => Some(callback(&mut painter.borrow_mut())),
                // 3) 남은 것을 수거한다. 가장 느린 타일 하나만큼만 기다린다.
                Slot::Waiting(receiver) => receiver.recv().ok(),
            });
        }
        results
    }

    /// ★스레드 준비가 안 된 경로 전용.★ 스크린샷 기계는 콜백
    /// (`Box<dyn FnOnce(Result<RgbaImage, _>)>`)과 `Rc<FxHashMap<..>>`
    /// (`ScreenshotRequestPhase::WaitingOnPipelineDisplayLists`)를 들고 있어 아직 타일
    /// 스레드로 갈 수 없다 — 콜백은 메인에 남기고 결과만 돌려받는 형태로 바꿔야 하고, 그것은
    /// 별도 하위 과제다(설계 §3.2).
    ///
    /// 그렇다고 `with_primary_painter` 의 `Send` 경계를 내리지는 않는다. **보증을 통째로
    /// 포기하는 것보다 예외를 하나 이름으로 남기는 편이 낫다** — 이 함수를 쓰는 곳이 늘어나면
    /// 그만큼 A 안에서 멀어진다는 뜻이고, 이름이 그것을 말해 준다. 지금 호출자는 하나다.
    fn with_primary_painter_not_thread_ready<R>(
        &self,
        webview_id: WebViewId,
        reason: &str,
        callback: impl FnOnce(&Painter) -> R,
    ) -> Option<R> {
        let painter_id = self.primary_painter_id_for_webview(webview_id);
        self.with_painter_not_thread_ready(painter_id, reason, callback)
    }

    /// [`Self::with_primary_painter_not_thread_ready`] 와 같은 예외를 `PainterId` 로 건다.
    fn with_painter_not_thread_ready<R>(
        &self,
        painter_id: PainterId,
        reason: &str,
        callback: impl FnOnce(&Painter) -> R,
    ) -> Option<R> {
        let host = self
            .painters
            .iter()
            .find(|host| host.painter_id == painter_id)?;
        match &host.kind {
            PainterHostKind::Inline(painter) => Some(callback(&painter.borrow())),
            PainterHostKind::Threaded(_) => {
                // 스레드에 있는 painter 에는 이 경로로 닿을 수 없다. ★조용히 건너뛰지
                // 않고 말한다★ — 스크린샷이 그냥 안 찍히는 것으로 보이면 원인을 찾는 데
                // 시간이 든다.
                warn!(
                    "{reason} reached {painter_id:?}, which runs on a tile thread, and that \
                     path is not thread-ready. Skipping it for this painter."
                );
                None
            },
        }
    }

    fn with_primary_painter<R: Send + 'static>(
        &self,
        webview_id: WebViewId,
        callback: impl FnOnce(&Painter) -> R + Send + 'static,
    ) -> Option<R> {
        self.with_painter(self.primary_painter_id_for_webview(webview_id), callback)
    }

    fn with_primary_painter_mut<R: Send + 'static>(
        &self,
        webview_id: WebViewId,
        callback: impl FnOnce(&mut Painter) -> R + Send + 'static,
    ) -> Option<R> {
        self.with_painter_mut(self.primary_painter_id_for_webview(webview_id), callback)
    }

    fn register_webview_painter_target(&self, webview_id: WebViewId, painter_id: PainterId) {
        let mut targets = self.webview_painter_targets.borrow_mut();
        let target_painter_ids = targets.entry(webview_id).or_default();
        if target_painter_ids.contains(&painter_id) {
            return;
        }

        target_painter_ids.push(painter_id);
        debug!(
            "Registered paint target {painter_id:?} for {webview_id:?}; targets={target_painter_ids:?}"
        );
    }

    fn remove_webview_painter_targets(&self, webview_id: WebViewId) -> Vec<PainterId> {
        // 임베더 핸들도 같이 놓아준다 — 안 그러면 `WebView` 가 사라진 뒤에도 이 표가
        // 마지막 참조를 붙들고 있게 된다.
        self.webviews.borrow_mut().remove(&webview_id);
        self.webview_painter_targets
            .borrow_mut()
            .remove(&webview_id)
            .unwrap_or_else(|| vec![webview_id.into()])
    }

    fn painter_targets_for_webview(&self, webview_id: WebViewId) -> Vec<PainterId> {
        self.webview_painter_targets
            .borrow()
            .get(&webview_id)
            .cloned()
            .unwrap_or_else(|| vec![webview_id.into()])
    }

    fn webview_has_painter_target(&self, webview_id: WebViewId, painter_id: PainterId) -> bool {
        self.painter_targets_for_webview(webview_id)
            .contains(&painter_id)
    }

    fn target_painter_ids_for_source_painter(
        &self,
        source_painter_id: PainterId,
    ) -> Vec<PainterId> {
        self.webview_painter_targets
            .borrow()
            .values()
            .find(|target_painter_ids| {
                target_painter_ids.first().copied() == Some(source_painter_id)
            })
            .cloned()
            .unwrap_or_else(|| vec![source_painter_id])
    }

    fn source_webview_id_for_primary_painter(
        &self,
        source_painter_id: PainterId,
    ) -> Option<WebViewId> {
        self.webview_painter_targets
            .borrow()
            .iter()
            .find_map(|(webview_id, target_painter_ids)| {
                (target_painter_ids.first().copied() == Some(source_painter_id))
                    .then_some(*webview_id)
            })
    }

    fn log_wall_frame_metadata(
        &self,
        logical_frame_id: u64,
        wall_webview_targets: &[(WebViewId, Vec<PainterId>)],
    ) {
        for (webview_id, target_painter_ids) in wall_webview_targets {
            let mut snapshots = Vec::new();
            let mut missing_targets = Vec::new();
            let webview_id = *webview_id;
            for painter_id in target_painter_ids {
                let Some(signature) = self.with_painter(*painter_id, move |painter| {
                    painter.wall_scroll_offsets_signature(webview_id)
                }) else {
                    missing_targets.push(format!("{painter_id:?}:missing-painter"));
                    continue;
                };
                let Some(signature) = signature else {
                    missing_targets.push(format!("{painter_id:?}:missing-webview"));
                    continue;
                };
                snapshots.push((*painter_id, signature));
            }

            let mut unique_signatures = Vec::new();
            for (_, signature) in &snapshots {
                if !unique_signatures.contains(signature) {
                    unique_signatures.push(signature.clone());
                }
            }

            if missing_targets.is_empty() && unique_signatures.len() <= 1 {
                let scroll_signature = unique_signatures
                    .first()
                    .map(String::as_str)
                    .unwrap_or("<no-scroll-tree>");
                info!(
                    "Wall frame metadata: logical_frame_id={} webview={:?} target_count={} \
                     scroll_offsets=matched scroll_signature=\"{}\" targets={:?} \
                     timestamp_source=single-script-update",
                    logical_frame_id,
                    webview_id,
                    target_painter_ids.len(),
                    scroll_signature,
                    target_painter_ids,
                );
            } else {
                warn!(
                    "Wall frame metadata mismatch: logical_frame_id={} webview={:?} \
                     target_count={} scroll_offsets=mismatched snapshots={:?} \
                     missing_targets={:?} timestamp_source=single-script-update",
                    logical_frame_id,
                    webview_id,
                    target_painter_ids.len(),
                    snapshots,
                    missing_targets,
                );
            }
        }
    }

    fn for_each_webview_painter_mut(
        &self,
        webview_id: WebViewId,
        callback: impl Fn(&mut Painter) + Clone + Send + 'static,
    ) {
        for painter_id in self.painter_targets_for_webview(webview_id) {
            self.with_painter_mut(painter_id, callback.clone());
        }
    }

    fn for_each_source_painter_target_mut(
        &self,
        source_painter_id: PainterId,
        callback: impl Fn(&mut Painter) + Clone + Send + 'static,
    ) {
        for painter_id in self.target_painter_ids_for_source_painter(source_painter_id) {
            self.with_painter_mut(painter_id, callback.clone());
        }
    }

    fn primary_painter_id_for_webview(&self, webview_id: WebViewId) -> PainterId {
        self.painter_targets_for_webview(webview_id)
            .first()
            .copied()
            .unwrap_or_else(|| webview_id.into())
    }

    fn next_logical_frame_id(&self) -> u64 {
        let frame_id = self.next_logical_frame_id.get() + 1;
        self.next_logical_frame_id.set(frame_id);
        frame_id
    }

    fn wall_frame_request_from_source_painters(
        &self,
        painter_ids: Vec<PainterId>,
    ) -> WallFrameRequest {
        let mut target_painter_ids = Vec::new();
        let mut wall_webview_targets = Vec::new();
        for painter_id in painter_ids {
            let targets_for_source = self.target_painter_ids_for_source_painter(painter_id);
            if targets_for_source.len() > 1 {
                if let Some(webview_id) = self.source_webview_id_for_primary_painter(painter_id) {
                    if !wall_webview_targets
                        .iter()
                        .any(|(existing_webview_id, _)| *existing_webview_id == webview_id)
                    {
                        wall_webview_targets.push((webview_id, targets_for_source.clone()));
                    }
                } else {
                    warn!(
                        "Could not resolve source WebViewId for wall frame source painter \
                         {painter_id:?}; scroll metadata comparison skipped"
                    );
                }
            }
            for target_painter_id in targets_for_source {
                if !target_painter_ids.contains(&target_painter_id) {
                    target_painter_ids.push(target_painter_id);
                }
            }
        }

        WallFrameRequest {
            target_painter_ids,
            wall_webview_targets,
        }
    }

    /// ★전 타일을 대상으로 한 요청.★ 틱 생산은 벽 수준 연산이라 `WebView` 별로 나누지
    /// 않는다 -- 한 주기에 논리 프레임 하나를 떠서 벽 전체에 뿌린다(규칙 1).
    ///
    /// 스크립트 경로의 생성자를 그대로 쓰고 **원천 집합만 "등록된 모든 painter"** 로
    /// 바꾼다. 그래야 `wall_webview_targets`(스크롤 메타데이터 비교 대상)가 두 경로에서
    /// 같은 규칙으로 채워지고, 순회가 `painters` 의 등록 순서를 따라 ★결정적★ 이다 --
    /// `webview_painter_targets` 를 직접 훑으면 `HashMap` 순서라 배리어 등록과 pacing
    /// 키가 실행마다 달라진다.
    ///
    /// ★설계 불변식 13 의 문구("WebView 소유 painter 만 틱 요청")와 다르다 -- 2026-10-22
    /// 전수 리뷰에서 확인했다.★ 여기 원천은 `self.painter_ids()`, 즉 ***등록된 모든
    /// painter*** 이고 소유 `WebView` 를 묻지 않는다. 지금 있는 셸에서는 두 집합이 같아
    /// 무해하다 -- 추가 paint target 도 전부 어느 `WebView` 가 `add_paint_target` 으로
    /// 등록한 것이다. 소유자 없는 painter 가 생기면 그때 `webview_painter_targets` 의
    /// 키 집합으로 좁히면 되고, 불변식 표에도 같은 정정을 적어 두었다.
    fn wall_frame_request_for_all_tiles(&self) -> WallFrameRequest {
        self.wall_frame_request_from_source_painters(self.painter_ids())
    }

    /// ★구성 요소를 모은다. 생산보다 먼저, 독립된 호출로.★
    ///
    /// 불변식 8 은 동적 속성(색·float·transform·유지값)의 합집합을 합성을 만들기 ***전에***
    /// 모으라 하고, 불변식 9 는 순서가 틀렸을 때의 실측(요청 120 / 폐기 60)을 걸어 둔다.
    /// `produce_wall_frame` 안에 묻어 두면 나중에 재배치가 가능해지므로 별도 메서드로 둔다.
    ///
    /// 값이 밀려 나가는 자리는 `Painter::pump_paint_animation` 하나이므로 그것을 부른다.
    /// ★2026-10-21 Task 7 로 그 함수에서 생산이 빠졌다★ -- 거기 있던 비디오 분기와 유보
    /// 애니메이션 프레임은 둘 다 삭제됐다(설계 2-6).
    ///
    /// ★다만 "순수한 값 푸시" 는 아니다★ -- 아직 움직이는 것이 있으면 그 함수가 깨우기
    /// 동반자를 건다(`wake_for_paint_animation`, `arm_paint_timer_heartbeat`). 규칙 2
    /// 위반은 아니다: 깨우기는 프레임을 내지 않고 ***이벤트 루프를 한 번 더 돌릴 뿐***
    /// 이다(설계 2-4).
    ///
    /// ★그러나 ***생산 예약***은 여기서 걸지 않는다 -- `PumpSource::Tick` 을 넘기는 이유가
    /// 그것이다.★ 2026-10-22 전수 리뷰. 깨우기와 달리 예약
    /// (`wall_tick::schedule_production_at_next_edge`)은 ***다음 에지에 생산 1 회***를
    /// 뜻하고, 생산의 구성 요소 수집 단계에서 그것을 걸면 ★틱 N 이 틱 N+1 을 되먹인다★.
    /// 그 고리에는 박자를 주는 것이 없어 생산이 이벤트 루프의 폴링 속도로 돌았다
    /// (실측: 초당 13,000 논리 프레임 대 WR 합성 60). 근거와 대안 비교는
    /// `painter::PumpSource::may_schedule`.
    ///
    /// 한 주기에 두 번 밀어 넣지 않을까: 안 그런다. 안쪽 `push_due` 게이트가 ★마지막
    /// 밀어넣기 이후 렌더가 있었는가★ 로 판정하므로, 렌더 없이 또 불러도 값이 다시
    /// 나가지 않는다.
    ///
    /// ★크레이트 밖으로 내보내지 않는다.★ 유일한 호출자는 `on_wall_tick` 이고, 그래야
    /// 순서를 틀리게 부를 방법이 애초에 없다.
    pub(crate) fn gather_components(&self) {
        // ★팬아웃이다 -- 루프로 돌리지 말 것.★ 이 단계는 타일마다 DWM 통계 조회를 포함하고
        // (`Painter::note_dcomp_frame_statistics`), 블로킹 루프로 돌리면 그 비용이 네 번
        // 더해진 채로 렌더 앞을 막는다. 근거는 `with_each_painter_mut`.
        let painter_ids = self.painter_ids();
        self.with_each_painter_mut(&painter_ids, |_| {
            |painter: &mut Painter| painter.pump_paint_animation(PumpSource::Tick)
        });
    }

    /// ★벽 프레임 하나를 낸다 -- 페이싱 틱의 단일 생산 지점.★ 그리고 그 뒤에 빚진 캔버스
    /// ack 를 푼다(`release_owed_canvas_acks`) -- ★해제는 생산과 무관하므로★ 생산이 거절된
    /// 타일에서도, 낼 타일이 하나도 없던 주기에도 돈다. 설계 2-7.
    ///
    /// 돌려주는 값은 이번 주기의 논리 프레임 번호다. 타일이 하나도 없으면 `None` 이고,
    /// 그때는 번호를 태우지 않는다 -- 틱은 내용과 무관하게 매 주기 도므로, 받을 타일이
    /// 없는 동안 번호만 올라가면 배리어 진단이 읽을 수 없게 된다.
    ///
    /// ★pacing 게이트를 거치지 않고 `issue_wall_frame_request` 로 바로 간다.★ 틱 자체가
    /// 페이서다. coalesce 는 "스크립트가 주기보다 빨리 조른다" 를 막는 장치이고, 주기마다
    /// 정확히 한 번 부르는 호출자에게는 미룰 것이 없다.
    ///
    /// ★게이트를 우회해도 게이트가 읽는 상태는 남긴다.★ 공유하는
    /// `issue_wall_frame_request` 가 `last_wall_frame_issue_at` 을 찍고 `register_frame` 으로
    /// `active_webview_frames` 에 넣는다(둘 다 ★생성한 타일이 둘 이상일 때만★ 돈다).
    /// 2026-10-21 Task 7 전에는 그 상태가 스크립트의 `PaintMessage::GenerateFrame` 을 더
    /// 자주 coalesce 시켰는데, 그 메시지가 아무 일도 하지 않게 되면서 상호작용 자체가
    /// 없어졌다 -- coalesce 큐에 들어갈 요청이 없다.
    ///
    /// ★`gather_components` 와 마찬가지로 크레이트 밖에 내지 않는다★ — 밖에서 쓰는 문은
    /// `on_wall_tick` 하나다.
    ///
    /// ★생산을 쉬는 사유는 하나뿐이다 -- 스크린샷★(`wall_tick::tick_should_produce`,
    /// 설계 2-8 / 불변식 16, 2026-10-22 Task 9). ★쉬는 것은 생산뿐이다★: 캔버스 ack 해제도
    /// liveness 프로브도 그대로 매 틱 돈다 -- 자세한 것은
    /// `hold_the_wall_while_a_screenshot_is_taken`.
    pub(crate) fn produce_wall_frame(&self) -> Option<u64> {
        let should_produce =
            crate::wall_tick::tick_should_produce(self.a_screenshot_is_waiting_on_a_frame());
        let logical_frame_id = if should_produce {
            let request = self.wall_frame_request_for_all_tiles();
            if request.target_painter_ids.is_empty() {
                None
            } else {
                self.issue_wall_frame_request(request)
            }
        } else {
            self.hold_the_wall_while_a_screenshot_is_taken();
            None
        };
        self.release_owed_canvas_acks();
        logical_frame_id
    }

    /// 전 타일을 통틀어 ★`WaitingOnFrame` 에 닿은 스크린샷 요청★ 이 있는가. 이것 하나가
    /// 생산을 쉬게 한다 -- "요청이 떠 있는가" 를 쓰면 교착이다
    /// (`ScreenshotRequestPhase::suspends_production`).
    ///
    /// 요청이 전부 소진된 것을 본 틱이 폴링 래치를 내린다. 그래서 스크린샷을 안 쓰는
    /// 실행에서 이 함수의 비용은 `Cell` 읽기 하나다.
    fn a_screenshot_is_waiting_on_a_frame(&self) -> bool {
        if !self.screenshot_poll_latch.get() {
            return false;
        }
        let mut any_request = false;
        for painter_id in self.painter_ids() {
            let Some(summary) =
                self.with_painter(painter_id, |painter| painter.screenshot_wait_summary())
            else {
                continue;
            };
            if summary.waiting_on_frame {
                // 기다리는 요청이 있으면 요청도 있는 것이므로 래치는 그대로 둔다.
                return true;
            }
            any_request |= summary.any_request;
        }
        if !any_request {
            self.screenshot_poll_latch.set(false);
        }
        false
    }

    /// 생산을 쉬는 틱. ★쉬는 것은 생산뿐이고, 타일을 살려 두는 일 셋은 그대로 돈다.★
    ///
    /// 1. ★keep-previous 지시를 걷는다 -- 이것이 교착을 막는 자리다.★ 무장한 타일은 셸이
    ///    `paint_target` 과 `present` 를 ***둘 다*** 건너뛰는데(`winit_wall/main.rs`),
    ///    스크린샷을 실제로 찍는 자리는 렌더 패스의 끝이다(`Painter::render` ->
    ///    `ScreenshotTaker::maybe_take_screenshots`). 자세한 것은
    ///    `WallFrameCoordinator::release_keep_previous_while_production_is_suspended`.
    /// 2. ★liveness 프로브는 쉬지 않는다.★ 2 초 강제 해제는 `renderer_behind()` 안에
    ///    있고, 그것을 매 틱 불러 주던 `generate_frame_for_wall_tick` 이 이 틱에는 안
    ///    돈다. 실측(2026-08-31): 그 호출을 잃자 네 페인터 중 둘이 44 초 동안 죽었다.
    /// 3. 캔버스 ack 해제는 이 함수 밖, `produce_wall_frame` 의 꼬리에 있다 -- ★생산
    ///    여부와 무관하게 돌기 때문에 거기 있는 것이고★, 그래서 여기서 다시 부르지
    ///    않는다(실측 117 초 스톨, `log_ani_debug 36`).
    fn hold_the_wall_while_a_screenshot_is_taken(&self) {
        let painter_ids = self.painter_ids();

        let released = self
            .wall_frame_coordinator
            .borrow_mut()
            .release_keep_previous_while_production_is_suspended(&painter_ids);

        for painter_id in &painter_ids {
            let _ = self.with_painter(*painter_id, |painter| painter.run_liveness_probe());
        }

        let in_window = self.production_suspended_in_window.get() + 1;
        let total = self.production_suspended_total.get() + 1;
        // ★창 단위로 누적한다★ -- 같은 줄의 `ticks_in_window` 와 단위를 맞춘다. 이 틱의
        // 값만 싣던 초안은 "1 초 동안 몇 개를 걷었나" 에 답하지 못했다.
        let released_in_window = self.keep_previous_released_in_window.get() + released as u64;
        self.production_suspended_in_window.set(in_window);
        self.production_suspended_total.set(total);
        self.keep_previous_released_in_window
            .set(released_in_window);
        let now = Instant::now();
        let due = self
            .production_suspended_last_report
            .get()
            .is_none_or(|last| now.duration_since(last).as_secs_f64() >= 1.0);
        if !due {
            return;
        }
        self.production_suspended_last_report.set(Some(now));
        self.production_suspended_in_window.set(0);
        self.keep_previous_released_in_window.set(0);
        info!(
            "WALLSHOTPAUSE: wall tick production is suspended while a screenshot waits for \
             frames to drain; ticks_in_window={in_window} total={total} \
             keep_previous_released_in_window={released_in_window} targets={}. Canvas ack \
             release and the display-composite liveness probe keep running every tick.",
            painter_ids.len(),
        );
    }

    /// ★틱마다, 타일마다 — 그 타일이 이번 틱에 합성했든 안 했든 — 빚이 있고 캔버스가 다
    /// 올라갔으면 ack 를 푼다.★ 설계 2-7.
    ///
    /// 생산 뒤에 도는 이유: 이번 틱의 합성이 그 캔버스를 화면에 올리는 합성이다. 앞에서
    /// 풀면 스크립트가 ***합성되기 전에*** 다음 rAF 를 돌 수 있다.
    ///
    /// 생산 집합이 아니라 ★등록된 모든 painter★ 를 돈다. 거절한 타일이야말로 빚을 쥔 채
    /// 남는 타일이고(실측: divisor=1 에서 스크립트 요청이 매번 거절로 빠져 117 초에
    /// 멎었다), 생성 집합으로 키를 잡으면 그 타일에 영영 안 닿는다. 낼 타일이 하나도
    /// 없어 논리 프레임을 태우지 않은 주기에도 돈다 — 해제는 생산과 무관하다.
    ///
    /// 결과가 필요 없지만 ★`with_painter_mut_detached` 를 쓰지 않는다★: 그쪽은 큐가 차면
    /// 콜백을 ***버리고***, 버려진 해제는 곧 멎은 스크립트다.
    fn release_owed_canvas_acks(&self) {
        // ★팬아웃이되 **버리지는 않는다**.★ `with_painter_mut_detached` 는 큐가 차면 콜백을
        // 버리고, 버려진 해제는 곧 멎은 스크립트다. `with_each_painter_mut` 는 보내고 모두
        // 수거하므로 그 보장은 그대로 두고 직렬화만 없앤다.
        let painter_ids = self.painter_ids();
        self.with_each_painter_mut(&painter_ids, |_| {
            |painter: &mut Painter| painter.release_owed_canvas_ack()
        });
    }

    /// ★페이싱 틱이 벽에 들어오는 유일한 문.★ 셸이 표출 주기마다 한 번 부른다.
    ///
    /// 하는 일은 순서 둘뿐이다: 구성 요소를 모으고(`gather_components`), 그 다음 프레임을
    /// 낸다(`produce_wall_frame`). 둘을 각각 내놓지 않고 이 문 하나만 공개하는 이유가
    /// 그 순서다 — 불변식 8 은 동적 속성의 합집합을 합성 ***전에*** 모으라 하고, 불변식 9 는
    /// 뒤집었을 때의 실측(요청 120 / 폐기 60)을 걸어 둔다. 바깥에 둘 다 공개하면 호출자가
    /// 순서를 틀릴 수 있지만, 문이 하나면 ★틀릴 방법 자체가 없다.★
    ///
    /// 돌려주는 값은 이번 주기의 논리 프레임 번호다(낼 타일이 없으면 `None`).
    pub fn on_wall_tick(&self) -> Option<u64> {
        self.gather_components();
        self.produce_wall_frame()
    }

    /// ★자유 실행 표출 클럭이 없는 임베더의 틱 문.★ 깨우기가 예약해 둔 에지에서만 생산한다
    /// (`wall_tick::schedule_production_at_next_edge`). 예약이 없으면 아무것도 하지 않고
    /// `None` 이다 -- 조용한 페이지는 깨우기가 없으므로 생산도 없다(불변식 16).
    ///
    /// 자유 실행 클럭이 있는 구성(`winit_wall`)은 이것이 아니라 `on_wall_tick` 을 매 주기
    /// 부른다. 어느 쪽이든 ★생산은 `on_wall_tick` 하나를 지난다★ -- 이 함수는 그 앞에
    /// 예약 비트 하나를 거는 것뿐이라 생산자가 둘이 되지 않는다.
    ///
    /// ## ★알려진 좁은 창: 예약이 소비되고 프레임은 안 날 수 있다★ (2026-10-22 Task 9.5)
    ///
    /// 이 함수는 비트를 `swap(false)` 로 ***먼저*** 가져간 다음 생산을 시도한다. 그래서 그
    /// 주기에 낼 타일이 하나도 없으면(전 타일이 역압·캔버스 대기로 거절, 또는 스크린샷으로
    /// 생산을 쉬는 틱) ★예약은 사라지고 프레임은 안 난다★. 뒤이어 렌더가 역압을 풀어도
    /// 다시 예약하는 자리가 없다.
    ///
    /// ★증상★: 조용한 페이지에서 ***한 번만*** 일어난 변경(스크립트가 연달아 두 번
    /// 갱신한 뒤의 두 번째 것 등)이 화면에 안 올라오고, 다음 `GenerateFrame` 이 올 때까지
    /// 낡은 그림이 남는다. 애니메이션 페이지에서는 다음 rAF 가 곧 다시 예약하므로 안 보인다.
    ///
    /// ★재현되지 않았다★ -- 2026-10-22 에 `setTimeout` 으로 1.5s / 3.0s 에 한 번씩만 DOM 을
    /// 바꾸는 페이지로 6 초를 돌렸고 두 변경이 모두 프레임이 됐다. 조용한 페이지에서는 틱이
    /// 올 때 painter 가 대개 한가하기 때문이다.
    ///
    /// ★고치려면★: "아무 타일도 생성하지 못한 틱은 예약을 되돌린다" 가 필요한데 두 가지가
    /// 없다 -- (1) `produce_wall_frame` 의 반환값으로는 "대상이 없었다" 와 "전부 거절했다" 가
    /// 구분되지 않는다, (2) 되돌린 비트를 집어 갈 ***깨우기***가 헤드 모드에서 따로 필요하다
    /// (렌더가 끝났다고 이벤트 루프가 깨지는 않는다).
    pub fn on_demand_wall_tick(&self) -> Option<u64> {
        let produced = self.on_demand_wall_tick_inner();
        self.note_demand_tick(produced.is_some());
        produced
    }

    /// ★진입 수와 생산 수를 초당 한 줄로 남긴다 -- `WALLTICKRATE`.★ 2026-10-22.
    ///
    /// 이 셸의 에지는 표출 클럭이 아니라 ***이벤트 루프의 스핀***이라, 진입이 초당 수천
    /// 번이 되어도 WR 프레임은 `renderer_behind` 거절이 막아 fps 에 안 나타난다. 그러면
    /// 남는 증상이 메인 스레드 CPU 뿐이고, 로그에는 아무것도 안 보인다. ★그 침묵이
    /// 2026-10-22 전수 리뷰 5 번을 세 작업에 걸쳐 숨겼다★ -- `gather_components` 안의
    /// 깨우기가 다음 에지를 되먹여 틱이 폴링 속도로 돌고 있었다.
    ///
    /// 읽는 법: ★`produced` 가 판정이고 `edges` 는 배경이다.★
    ///
    /// - `produced` 는 표출률이어야 한다 -- 애니메이션 페이지에서 60, 조용한 페이지에서 0
    ///   (불변식 16). 이 값이 세 자리를 넘으면 ***생산자가 되먹임 고리 안에 있다***.
    /// - `edges` 는 생산과 무관하다. 이 셸의 에지는 이벤트 루프의 스핀이라
    ///   `ControlFlow::Poll` 의 속도를 그대로 적는다. 실측(2026-10-22, headed):
    ///   애니메이션 페이지 ~27,000/s, ★조용한 페이지 ~130,000/s(프로세스 CPU 0.97 코어)★.
    ///   후자는 틱과 무관한 ***이 셸의 별개 문제***다 -- 조용한데도 `Wait` 로 안 내려간다.
    ///   틱은 그 스핀마다 `swap` 하나를 하고 돌아선다.
    ///
    /// ★줄은 스핀이 도는 한 매초 나간다★ -- 조용한 페이지에서도. 침묵으로 "고쳐졌다" 와
    /// "이 코드가 안 돈다" 를 구별할 수 없게 만들지 않는다(이 저장소가 같은 함정에 네 번
    /// 빠진 기록이 `WALLACKFLUSH` 주석에 있다).
    fn note_demand_tick(&self, produced: bool) {
        let now = Instant::now();
        let edges = self.demand_tick_edges_in_window.get() + 1;
        self.demand_tick_edges_in_window.set(edges);
        if produced {
            self.demand_tick_produced_in_window
                .set(self.demand_tick_produced_in_window.get() + 1);
        }
        let Some(started) = self.demand_tick_window_start.get() else {
            self.demand_tick_window_start.set(Some(now));
            return;
        };
        let elapsed = now.duration_since(started);
        if elapsed < Duration::from_secs(1) {
            return;
        }
        self.demand_tick_window_start.set(Some(now));
        self.demand_tick_edges_in_window.set(0);
        let frames = self.demand_tick_produced_in_window.replace(0);
        info!(
            "WALLTICKRATE window_ms={:.0} edges={edges} produced={frames}",
            elapsed.as_secs_f64() * 1000.0
        );
    }

    fn on_demand_wall_tick_inner(&self) -> Option<u64> {
        if !crate::wall_tick::take_scheduled_edge() {
            return None;
        }
        // ★틱 번호를 여기서도 올린다 -- 이것이 `TICK_SEQ` 의 ***두 번째 writer*** 다.★
        // 자유 실행 셸은 `paint::note_present_tick()` 에서 올리지만, 수요 구동 임베더는 그
        // 함수를 부르지 않는다 -- 그러면 번호가 0 에 멈춰 `ExternalPresentBudget` 이
        // ***최초 1 회 뒤 영구 정지***한다. 이 함수가 그 구성의 틱 에지이므로 여기가 그
        // 자리다.
        //
        // ★둘 다 부르는 셸은 아직 없고, 강제 장치도 두지 않는다.★ 생기면 한 에지에 번호가
        // 두 번 올라 외부 서피스 표출의 천장이 깨진다(틱당 2 회). 그 증상은 설계 8 절의
        // `external_surface` "틱당 1 회 이하" 판정이 잡는다 -- `wall_tick::TICK_SEQ` 주석.
        crate::wall_tick::note_tick_edge();
        self.on_wall_tick()
    }

    fn max_pending_frames_for_targets(&self, target_painter_ids: &[PainterId]) -> usize {
        target_painter_ids
            .iter()
            .filter_map(|painter_id| {
                self.with_painter(*painter_id, move |painter| painter.pending_frames())
            })
            .max()
            .unwrap_or_default()
    }

    fn wall_frame_request_pacing_block_reason(
        &self,
        request: &WallFrameRequest,
    ) -> Option<WallFramePacingBlockReason> {
        if !self.wall_frame_pacing_config.enabled()
            || request.target_painter_ids.len() <= 1
            || request.pacing_key().is_none()
        {
            return None;
        }

        {
            let wall_frame_coordinator = self.wall_frame_coordinator.borrow();
            for webview_id in request.wall_webview_ids() {
                if wall_frame_coordinator.webview_has_active_frame(webview_id) {
                    return Some(WallFramePacingBlockReason::Active);
                }
            }
        }

        let pending_max = self.max_pending_frames_for_targets(&request.target_painter_ids);
        if pending_max >= self.wall_frame_pacing_config.max_pending {
            return Some(WallFramePacingBlockReason::Pending);
        }

        let now = Instant::now();
        let last_issue_at = self.last_wall_frame_issue_at.borrow();
        for webview_id in request.wall_webview_ids() {
            let Some(last_issue_at) = last_issue_at.get(&webview_id) else {
                continue;
            };
            let elapsed = now.saturating_duration_since(*last_issue_at);
            if elapsed < self.wall_frame_pacing_config.min_interval {
                return Some(WallFramePacingBlockReason::TooSoon);
            }
        }

        None
    }

    // ★`coalesce_wall_frame_request` 와 `schedule_wall_frame_pacing_wake` 는 2026-10-21 에
    // 삭제됐다.★ 벽 페이서는 "스크립트가 주기보다 빨리 조른다" 를 막는 장치였고, 요청을
    // 넣어 주던 유일한 자리(`PaintMessage::GenerateFrame`)가 아무 일도 하지 않게 되면서
    // 넣을 것이 없어졌다. 틱 자체가 페이서다 -- 주기마다 정확히 한 번 부르는 호출자에게는
    // 미룰 것이 없다(`produce_wall_frame` 주석).
    //
    // ★나머지 페이싱 장치는 남긴다 -- 그것이 설계 4 절이 요구하는 탐지기다.★
    // `try_release_coalesced_wall_frame_requests` 와 `note_wall_pacer_join` 은 "프레임을
    // 받는 두 번째 문" 이 열리는지 보는 계측이고, 벽 구성에서 `FRAMEREASON` 의
    // `wall_pacer_join` 이 0 이 아니면 생산자가 둘이라는 뜻이다(설계 8 절).

    fn log_wall_frame_pacing_summary(
        &self,
        event: &str,
        webview_id: WebViewId,
        request: &WallFrameRequest,
        pending_max: usize,
        coalesced_for_webview: u64,
        logical_frame_id: Option<u64>,
    ) {
        let total_coalesced = self.wall_frame_pacing_coalesced_count.get();
        let total_released = self.wall_frame_pacing_released_count.get();
        let event_total = match event {
            "released" => total_released,
            _ => total_coalesced,
        };
        if event_total > 3 && event_total % WALL_FRAME_PACING_INFO_INTERVAL != 0 {
            return;
        }

        // The tally goes AFTER `policy=`, which is where the log parser's regex stops
        // (wall_perf_analyzer/analyze_wall_perf.py, `PACING_SUMMARY_RE`). Appending keeps
        // older parsers matching; inserting in the middle would break them.
        info!(
            "Wall frame pacing summary: event={} webview={:?} logical_frame_id={:?} \
             target_painters={:?} pending_max={} coalesced_for_webview={} \
             total_coalesced={} total_released={} policy=latest-first \
             blocked_by_active={} blocked_by_pending={} blocked_by_min_interval={}",
            event,
            webview_id,
            logical_frame_id,
            request.target_painter_ids,
            pending_max,
            coalesced_for_webview,
            total_coalesced,
            total_released,
            self.wall_frame_pacing_block_tally.active.get(),
            self.wall_frame_pacing_block_tally.pending.get(),
            self.wall_frame_pacing_block_tally.too_soon.get(),
        );
    }

    /// ★타일에 닿는 문은 하나다 -- 틱 전용 진입점.★ 2026-10-21 전에는 스크립트 경로
    /// (`Painter::generate_frame_for_script`)와 둘이었고, 어느 문으로 들어가는지를
    /// `TileEntryPoint` 가 골랐다. 그 경로가 값 적재로 격하되면서 고를 것이 없어졌다.
    fn issue_wall_frame_request(&self, request: WallFrameRequest) -> Option<u64> {
        let wall_frame_requested_at = Instant::now();
        let logical_frame_id = self.next_logical_frame_id();
        // Before anything else: a newer wall frame supersedes any "keep the previous frame"
        // instruction left over from an older one. This runs over the REQUESTED targets, not
        // the ones that end up generating -- a painter stuck behind a stale skip cannot
        // generate, so keying off the generated set would never release it.
        self.wall_frame_coordinator
            .borrow_mut()
            .expire_keep_previous_before(logical_frame_id, &request.target_painter_ids);
        if request.target_painter_ids.len() > 1 {
            info!(
                "Wall logical frame {logical_frame_id} fan-out to paint targets \
                 {:?}",
                request.target_painter_ids
            );
            self.log_wall_frame_metadata(logical_frame_id, &request.wall_webview_targets);
        } else {
            debug!(
                "Logical frame {logical_frame_id} routed to paint targets \
                 {:?}",
                request.target_painter_ids
            );
        }

        // ★타일은 둘 중 하나로 이 논리 프레임에 참여한다 -- 생성 아니면 keep-previous.★
        // 거절한 타일을 아무 데도 안 넣으면 그 타일만 전 주기의 내용을 들고 남아 화면에
        // 프레임 N 과 N-1 이 섞인다(설계 2-5). 호스트가 없어진 타일(`None`)만 어느 쪽도
        // 아니다 -- 유지할 화면 자체가 없다.
        let mut generated_painter_ids = Vec::new();
        let mut kept_previous_painter_ids = Vec::new();
        // ★거절했다고 다 무장하지는 않는다 -- 사유가 가른다.★ 2026-10-22 Task 9.5.
        // `RendererBehind` 거절에 무장하면 자기잠금이다(무장이 막는 렌더가 곧 그 사유를 푸는
        // 자리다). 근거와 실측은 `wall_tick::decline_may_arm_keep_previous`.
        let mut armable_painter_ids = Vec::new();
        // ★생성도 팬아웃이다.★ 타일마다 보내고 기다리면 네 타일의 `generate_frame_for_wall_tick`
        // 이 줄을 서고, 그 합이 렌더 시작을 뒤로 민다. 순서는 보존되므로 아래 집합들은
        // 그대로 결정적이다(`with_each_painter_mut`).
        let outcomes =
            self.with_each_painter_mut(&request.target_painter_ids, |_| {
                move |painter: &mut Painter| {
                    painter.generate_frame_for_wall_tick(logical_frame_id, wall_frame_requested_at)
                }
            });
        for (painter_id, outcome) in request.target_painter_ids.iter().zip(outcomes) {
            match outcome {
                Some(TileOutcome::Generated) => generated_painter_ids.push(*painter_id),
                Some(TileOutcome::KeepPrevious(reason)) => {
                    kept_previous_painter_ids.push(*painter_id);
                    if crate::wall_tick::decline_may_arm_keep_previous(reason) {
                        armable_painter_ids.push(*painter_id);
                    }
                },
                None => {},
            }
        }

        // ★두 게이트를 다 지난 타일만 무장한다★ -- 사유 게이트는 위에서, 벽 모양 게이트는
        // 여기서(`wall_tick::wall_frame_arms_keep_previous`, 2026-10-22 Task 9.5). 둘이 막는
        // 것이 다르다: 앞은 자기잠금인 사유를, 뒤는 섞일 상대가 없는 한 타일짜리 벽을.
        if crate::wall_tick::wall_frame_arms_keep_previous(
            request.target_painter_ids.len(),
            !armable_painter_ids.is_empty(),
        ) {
            self.wall_frame_coordinator
                .borrow_mut()
                .arm_keep_previous(logical_frame_id, &armable_painter_ids);
        }

        // ★배리어에는 ***생성한*** 타일만 넘긴다.★ 배리어가 기다리는 것은 frame-ready 이고,
        // keep-previous 타일은 트랜잭션을 안 보냈으므로 그 신호를 안 낸다.
        if generated_painter_ids.len() > 1 {
            let mut last_wall_frame_issue_at = self.last_wall_frame_issue_at.borrow_mut();
            for webview_id in request.wall_webview_ids() {
                last_wall_frame_issue_at.insert(webview_id, wall_frame_requested_at);
            }
            self.wall_frame_coordinator.borrow_mut().register_frame(
                logical_frame_id,
                request.wall_webview_ids(),
                generated_painter_ids,
                wall_frame_requested_at,
            );
        } else if request.target_painter_ids.len() > 1 {
            // ★`info!` 다 -- `debug!` 는 릴리스에서 컴파일 단계에 지워진다★(servoshell 의
            // 기본 피처 `max_log_level` = `log/release_max_level_info`). 이 줄이 필드에서
            // ***거절***과 ***무장***을 가르는 유일한 자리인데 정작 읽을 빌드에서 사라져
            // 있었다. 2026-10-22 Task 9.5.
            //
            // ★두 집합은 다르다.★ `kept_previous` 는 이번 주기에 프레임을 못 낸 타일
            // 전부이고, `armed` 는 그중 셸이 ***렌더까지 건너뛸*** 타일이다 -- 게이트 둘
            // (`wall_tick::decline_may_arm_keep_previous`,
            // `wall_tick::wall_frame_arms_keep_previous`)을 지난 부분집합이다. 역압으로
            // 거절한 타일은 `kept_previous` 에 있고 `armed` 에는 없다.
            //
            // 빈도: 이 갈래는 ***다중 타일 벽이 한 장 이하를 냈을 때***만 돈다 -- 정상
            // 운영에서는 안 나온다. 같은 함수의 fan-out 줄이 이미 매 벽 프레임 `info!` 다.
            info!(
                "Wall logical frame {logical_frame_id} barrier skipped: generated_targets={:?} \
                 kept_previous_targets={:?} armed_targets={:?} requested_targets={:?}",
                generated_painter_ids,
                kept_previous_painter_ids,
                armable_painter_ids,
                request.target_painter_ids
            );
        }

        Some(logical_frame_id)
    }

    fn try_release_coalesced_wall_frame_requests(&self) {
        if self
            .wall_frame_pacing_next_wake_at
            .get()
            .is_some_and(|wake_at| Instant::now() >= wake_at)
        {
            self.wall_frame_pacing_next_wake_at.set(None);
        }

        loop {
            let releasable_webview_id = {
                let coalesced_requests = self.coalesced_wall_frame_requests.borrow();
                coalesced_requests.iter().find_map(|(webview_id, request)| {
                    self.wall_frame_request_pacing_block_reason(&request.request)
                        .is_none()
                        .then_some(*webview_id)
                })
            };
            let Some(webview_id) = releasable_webview_id else {
                return;
            };

            let Some(coalesced_request) = self
                .coalesced_wall_frame_requests
                .borrow_mut()
                .remove(&webview_id)
            else {
                continue;
            };

            let total_released = self.wall_frame_pacing_released_count.get() + 1;
            self.wall_frame_pacing_released_count.set(total_released);
            // ★coalesce 됐다 풀려 나가는 요청은 직접 낸 요청과 다른 문으로 나간다★ — 이어서
            // WR 프레임이 되어 painter.rs 줄로도 세어지지만, 어느 쪽 문이었는지는 거기서 안 보인다.
            if crate::painter::frame_reason_prof_enabled() {
                if let Some(painter_id) = coalesced_request.request.target_painter_ids.first() {
                    self.with_painter(*painter_id, |painter| painter.note_wall_pacer_join());
                }
            }
            let logical_frame_id =
                self.issue_wall_frame_request(coalesced_request.request.clone());
            debug!(
                "Wall frame pacing released: webview={:?} logical_frame_id={:?} \
                 target_painters={:?} pending_max={} coalesced_for_webview={} \
                 total_released={} policy=latest-first",
                webview_id,
                logical_frame_id,
                coalesced_request.request.target_painter_ids,
                coalesced_request.max_pending_seen,
                coalesced_request.coalesced_count,
                total_released,
            );
            self.log_wall_frame_pacing_summary(
                "released",
                webview_id,
                &coalesced_request.request,
                coalesced_request.max_pending_seen,
                coalesced_request.coalesced_count,
                logical_frame_id,
            );
        }
    }

    pub fn painter_id(&self) -> PainterId {
        self.painters[0].painter_id
    }

    pub fn rendering_context_size(&self, painter_id: PainterId) -> Size2D<u32, DevicePixel> {
        self.with_painter(painter_id, move |painter| {
            painter.rendering_context.size2d()
        })
        .expect("painter_id not found")
    }

    /// ★병렬 타일 렌더(A 안) 2 단계의 컴파일 타임 관문.★
    ///
    /// ★낡음(2026-09-03 기록, 같은 날 `b2480f98270` 이후 무효)★ — `painter_surfman_details_map` 가 늘어 아홉 가지다.
    /// A 안은 `Painter` 를 타일 스레드가 소유한다. `Painter::new` 는 `&Paint` 에서 여덟 가지를
    /// 읽는데, 그것들이 전부 `Send` 여야 타일 스레드가 `Painter` 를 **직접 만들** 수 있다
    /// (만들어 놓고 옮기는 것은 안 된다 — `webrender::Renderer` 도 `RenderingContext` 도
    /// `!Send` 다. 설계 §7-3, §3-1).
    ///
    /// 눈으로 하나씩 따지는 대신 컴파일러가 판정하게 두고, 남겨서 회귀를 막는다 — 여기에
    /// `Rc` 를 하나 끼워 넣는 변경이 들어오면 A 안이 조용히 깨지는 대신 빌드가 깨진다.
    /// 호출되지 않는다. 존재 자체가 검사다.
    #[allow(dead_code)]
    fn assert_painter_inputs_are_send(&self) {
        fn require_send<T: Send>(_value: &T) {}
        require_send(&self.painter_inputs());
    }

    /// [`Painter::new`] 이 필요로 하는 것 전부를 `Send` 값으로 떼어낸다.
    fn painter_inputs(&self) -> PainterInputs {
        PainterInputs {
            webrender_external_image_id_manager: self.webrender_external_image_id_manager(),
            webgl_threads: self.webgl_threads(),
            swap_chains: self.swap_chains.clone(),
            busy_webgl_contexts_map: self.busy_webgl_contexts_map.clone(),
            webgpu_image_map: self.webgpu_image_map(),
            embedder_to_constellation_sender: self.embedder_to_constellation_sender.clone(),
            event_loop_waker: self.event_loop_waker.clone_box(),
            paint_proxy: self.paint_proxy.clone(),
            painter_surfman_details_map: self.painter_surfman_details_map.clone(),
        }
    }

    pub fn webgl_threads(&self) -> WebGLThreads {
        self.webgl_threads.clone()
    }

    pub fn webrender_external_image_id_manager(&self) -> WebRenderExternalImageIdManager {
        self.webrender_external_image_id_manager.clone()
    }

    pub fn webxr_running(&self) -> bool {
        #[cfg(feature = "webxr")]
        {
            self.webxr_main_thread.borrow().running()
        }
        #[cfg(not(feature = "webxr"))]
        {
            false
        }
    }

    #[cfg(feature = "webxr")]
    pub fn webxr_main_thread_registry(&self) -> webxr_api::Registry {
        self.webxr_main_thread.borrow().registry()
    }

    #[cfg(feature = "webgpu")]
    pub fn webgpu_image_map(&self) -> WebGpuExternalImageMap {
        self.webgpu_image_map.get_or_init(Default::default).clone()
    }

    pub fn webviews_needing_repaint(&self) -> Vec<WebViewId> {
        self.painter_ids()
            .into_iter()
            .flat_map(|painter_id| {
                self.with_painter(painter_id, |painter| painter.webviews_needing_repaint())
                    .unwrap_or_default()
            })
            .collect()
    }

    fn record_painter_ready_for_repaint(
        frame_ready_for_painter: &mut HashMap<PainterId, bool>,
        painter_id: PainterId,
        repaint_needed: bool,
    ) {
        *frame_ready_for_painter
            .entry(painter_id)
            .or_insert(repaint_needed) |= repaint_needed;
    }

    fn record_wall_frame_repaint_decisions(
        frame_ready_for_painter: &mut HashMap<PainterId, bool>,
        decisions: Vec<WallFrameRepaintDecision>,
    ) {
        for decision in decisions {
            Self::record_painter_ready_for_repaint(
                frame_ready_for_painter,
                decision.painter_id,
                decision.repaint_needed,
            );
        }
    }

    fn handle_painters_ready_for_repaint(&self, frame_ready_for_painter: HashMap<PainterId, bool>) {
        for (painter_id, repaint_needed) in frame_ready_for_painter {
            self.with_painter(painter_id, move |painter| {
                painter.handle_new_webrender_frame_ready(repaint_needed)
            });
        }
    }

    pub fn paint_target_keep_previous_logical_frame(
        &self,
        webview_id: WebViewId,
        painter_id: PainterId,
    ) -> Option<u64> {
        if !self.webview_has_painter_target(webview_id, painter_id) {
            warn!(
                "Ignoring keep-previous query for unregistered paint target {painter_id:?} \
                 of {webview_id:?}"
            );
            return None;
        }

        self.wall_frame_coordinator
            .borrow_mut()
            .keep_previous_logical_frame(painter_id)
    }

    pub fn finish_shutting_down(&self) {
        // Drain paint port, sometimes messages contain channels that are blocking
        // another thread from finishing (i.e. SetFrameTree).
        while self.paint_receiver.try_recv().is_ok() {}

        let (webgl_exit_sender, webgl_exit_receiver) =
            generic_channel::channel().expect("Failed to create IPC channel!");
        if !self
            .webgl_threads
            .exit(webgl_exit_sender)
            .is_ok_and(|_| webgl_exit_receiver.recv().is_ok())
        {
            warn!("Could not exit WebGLThread.");
        }

        // Tell the profiler, memory profiler, and scrolling timer to shut down.
        if let Ok((sender, receiver)) = ipc::channel() {
            self.time_profiler_chan
                .send(profile_time::ProfilerMsg::Exit(sender));
            let _ = receiver.recv();
        }
    }

    fn handle_browser_message(&self, msg: PaintMessage) {
        trace_msg_from_constellation!(msg, "{msg:?}");

        match self.shutdown_state() {
            ShutdownState::NotShuttingDown => {},
            ShutdownState::ShuttingDown => {
                self.handle_browser_message_while_shutting_down(msg);
                return;
            },
            ShutdownState::FinishedShuttingDown => {
                // Messages to Paint are ignored after shutdown is complete.
                return;
            },
        }

        match msg {
            PaintMessage::CollectMemoryReport(sender) => {
                self.collect_memory_report(sender);
            },
            PaintMessage::SetWebViewAnimating(webview_id, animating) => {
                // 트레이트 객체는 메인(여기)에만 있다. painter 스레드는 통보만 보낸다.
                if let Some(webview) = self.webviews.borrow().get(&webview_id) {
                    webview.set_animating(animating);
                }
            },
            PaintMessage::ChangeRunningAnimationsState(
                webview_id,
                pipeline_id,
                animation_state,
            ) => {
                self.for_each_webview_painter_mut(webview_id, move |painter| {
                    painter.change_running_animations_state(
                        webview_id,
                        pipeline_id,
                        animation_state,
                    );
                });
            },
            PaintMessage::SetFrameTreeForWebView(webview_id, frame_tree) => {
                self.for_each_webview_painter_mut(webview_id, move |painter| {
                    painter.set_frame_tree_for_webview(&frame_tree);
                });
            },
            PaintMessage::SetThrottled(webview_id, pipeline_id, throttled) => {
                self.for_each_webview_painter_mut(webview_id, move |painter| {
                    painter.set_throttled(webview_id, pipeline_id, throttled);
                });
            },
            PaintMessage::PipelineExited(webview_id, pipeline_id, pipeline_exit_source) => {
                self.for_each_webview_painter_mut(webview_id, move |painter| {
                    painter.notify_pipeline_exited(webview_id, pipeline_id, pipeline_exit_source);
                });
            },
            PaintMessage::NewWebRenderFrameReady(..) => {
                unreachable!("New WebRender frames should be handled in the caller.");
            },
            PaintMessage::SendInitialTransaction(webview_id, pipeline_id) => {
                self.for_each_webview_painter_mut(webview_id, move |painter| {
                    painter.send_initial_pipeline_transaction(webview_id, pipeline_id);
                });
            },
            PaintMessage::ScrollNodeByDelta(
                webview_id,
                pipeline_id,
                offset,
                external_scroll_id,
            ) => {
                self.for_each_webview_painter_mut(webview_id, move |painter| {
                    painter.scroll_node_by_delta(
                        webview_id,
                        pipeline_id,
                        offset,
                        external_scroll_id,
                    );
                });
            },
            PaintMessage::ScrollViewportByDelta(webview_id, delta) => {
                self.for_each_webview_painter_mut(webview_id, move |painter| {
                    painter.scroll_viewport_by_delta(webview_id, delta);
                });
            },
            PaintMessage::UpdateEpoch {
                webview_id,
                pipeline_id,
                epoch,
            } => {
                self.for_each_webview_painter_mut(webview_id, move |painter| {
                    painter.update_epoch(webview_id, pipeline_id, epoch);
                });
            },
            PaintMessage::SendDisplayList {
                webview_id,
                display_list_descriptor,
                display_list_info_receiver,
                display_list_data_receiver,
            } => {
                self.handle_new_display_list(
                    webview_id,
                    display_list_descriptor,
                    display_list_info_receiver,
                    display_list_data_receiver,
                );
            },
            PaintMessage::GenerateFrame(_painter_ids) => {
                // ★스크립트의 프레임 요청은 2026-10-21 부터 프레임을 내지 않는다(규칙 2).★
                //
                // 프레임의 구성 요소는 그 프레임의 생산 트리거가 될 수 없다. 스크립트가 올린
                // 장면은 ***별개 전송***으로 이미 WR 에 가 있고(`PaintMessage::SendDisplayList`
                // -> `handle_new_display_list`), 그것을 화면에 올리는 프레임은 다음 틱이
                // 낸다(`Paint::on_wall_tick`).
                //
                // ★메시지 자체는 지우지 않는다.★ `script` 쪽 계약(렌더링 기회마다 보낸다)과
                // WPT 를 건드리지 않고 벽의 생산만 일원화하는 것이 이 변경의 범위다.
                // 설계 2-6: docs/superpowers/specs/2026-10-21-single-frame-producer-design.md
                //
                // ★2026-10-22 Task 9.5 — 여기는 "아무 일도 안 하는 자리" 가 아니라
                // ***깨우기의 자리***다.★ 자유 실행 표출 클럭이 없는 임베더(servoshell /
                // WPT / Android)는 틱 에지를 깨우기에서 받는다(설계 2-4). 그 임베더에서
                // 이 메시지가 비트를 세우지 않으면 ***부트스트랩 한 장 뒤로 영영 프레임이
                // 없다*** -- 2026-10-22 실측: 헤드리스 `-z -x -o` 스크린샷이
                // `WALLBOOTSTRAP ... tick_frames_produced=0` 한 줄 뒤에 1024x740 순백을
                // 저장했다.
                //
                // ★규칙 2 는 그대로다 -- 세우기만 하고 내지 않는다.★ 비트는 ***다음***
                // 틱 에지의 생산 1 회를 예약할 뿐이고, 그 에지에서 집어 가 내는 것은
                // `Paint::on_demand_wall_tick` 이다. 그래서 생산자는 여전히 틱 하나이고
                // (규칙 1), 깨우기가 없는 조용한 페이지는 생산도 없다(불변식 16) --
                // 스크립트는 `needs_frame()` 일 때만 이 메시지를 보낸다
                // (`script_thread.rs` 의 `update_the_rendering`).
                //
                // 몇 번 와도 에지당 생산은 한 번이다(`swap(false)`). 자유 실행 클럭이 있는
                // 구성(`winit_wall`)은 이 비트를 보지 않고 매 에지에 낸다.
                crate::wall_tick::schedule_production_at_next_edge();
            },
            PaintMessage::GetWebViewPainterTargets(webview_id, sender) => {
                let targets = self.painter_targets_for_webview(webview_id);
                // 질문과 답을 양쪽에서 볼 수 있어야 한다 — WebGL 팬아웃 대상이 이 답으로
                // **고정**되므로(`WEBGLTARGETS` 참고), 짧게 답하면 빠진 타일은 캔버스를
                // 영영 못 받는다. 이 줄이 없으면 "누가 짧게 답했나" 를 추론해야 한다.
                warn!(
                    "PAINTTARGETS: answering {webview_id:?} with {} painter(s): {targets:?}",
                    targets.len()
                );
                let _ = sender.send(targets);
            },
            PaintMessage::GenerateImageKey(webview_id, result_sender) => {
                self.handle_generate_image_key(webview_id, result_sender);
            },
            PaintMessage::GenerateImageKeysForPipeline(webview_id, pipeline_id) => {
                self.handle_generate_image_keys_for_pipeline(webview_id, pipeline_id);
            },
            PaintMessage::UpdateImages(painter_id, updates) => {
                // 이 한 건이 0.59ms 로 측정됐다(드레인 77건에 45.7ms). 그 시간이 팬아웃
                // 바깥(타겟 찾기·복제)인지 페인터 안(스태시·합성 요청)인지 갈라야 고칠
                // 곳이 정해진다. 초당 한 줄로 낸다.
                let arm_started = Instant::now();
                let target_painter_ids = self.target_painter_ids_for_source_painter(painter_id);
                if target_painter_ids.len() > 1 {
                    let mut add_count = 0;
                    let mut update_count = 0;
                    let mut delete_count = 0;
                    let mut animation_update_count = 0;
                    for update in &updates {
                        match update {
                            ImageUpdate::AddImage(..) => add_count += 1,
                            ImageUpdate::UpdateImage(..) => update_count += 1,
                            ImageUpdate::DeleteImage(..) => delete_count += 1,
                            ImageUpdate::UpdateImageForAnimation(..) => animation_update_count += 1,
                        }
                    }
                    // ★요약을 찍기로 정한 뒤에 그 인자를 만든다.★ 예전에는 타겟마다 painter 를 찾아
                    // `requested_gpus` 를 만드는 일을 **메시지마다** 했는데, 그 값은 걸러지는 `debug!`
                    // 와 드물게 나가는 아래 `info!` 에서만 쓰인다. 전환 순간 이 자리를 초당 수천 번
                    // 지나므로, 쓰지도 않을 값을 위해 타일 수만큼 painter 를 찾는 왕복이 그대로 쌓인다.
                    let fanout_id =
                        WALL_MEDIA_IMAGE_FANOUT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                    if add_count > 0
                        || delete_count > 0
                        || fanout_id <= 3
                        || fanout_id % WALL_MEDIA_IMAGE_FANOUT_INFO_INTERVAL == 0
                    {
                        let requested_gpus: Vec<_> = target_painter_ids
                            .iter()
                            .map(|target_painter_id| {
                                self.with_painter(*target_painter_id, move |painter| {
                                    painter.rendering_context.requested_gpu_index()
                                })
                                .flatten()
                            })
                            .collect();
                        info!(
                            "Wall media image fanout summary: fanout_id={fanout_id} source_painter={painter_id:?} target_painters={target_painter_ids:?} requested_gpus={requested_gpus:?} updates_total={} adds={add_count} updates={update_count} deletes={delete_count} animation_updates={animation_update_count}",
                            updates.len(),
                        );
                    }
                }
                let mut inner_ms = 0.0_f64;
                for target_painter_id in target_painter_ids {
                    let updates = updates.clone();
                    let inner_started = Instant::now();
                    self.with_painter_mut_detached(target_painter_id, move |painter| {
                        painter.update_images(updates)
                    });
                    inner_ms += inner_started.elapsed().as_secs_f64() * 1000.0;
                }
                // ★이미지 도착도 깨우기다 -- 다음 틱 에지에 생산 1 회를 예약한다.★
                // 2026-10-22 Task 9.5 2 차. 규칙 2 를 어기지 않는다: 비트를 세우는 것은
                // 생산이 아니고(설계 2-4), 내는 것은 그 에지의 틱이다. 같은 모양의 깨우기가
                // 이미 둘 있다 -- `PaintMessage::GenerateFrame` 과
                // `Painter::pump_paint_animation`. ★구성 요소는 종을 울릴 수 있다. 방아쇠를
                // 당길 수 없을 뿐이다.★ 새 이미지 데이터는 스크립트 쪽 `needs_frame()` 의
                // `UpdatedImageData` 와 같은 신호이기도 하다.
                //
                // ★이것이 없으면 캔버스 페이지가 영구 정지한다.★ 닫힌 고리는 이렇다:
                //
                // ```text
                // 스크립트가 캔버스 지연을 건다 -> 문서가 잠긴다(`script_thread.rs` 의
                //   `waiting_on_canvas_image_updates()` 가 그 문서를 건너뛴다 = 더는
                //   `GenerateFrame` 이 없다)
                //   -> 예약돼 있던 틱이 발화 -> `KeepPrevious(WaitingOnCanvasImages)`
                //   -> 프레임도 렌더도 없음
                //   -> `release_owed_canvas_acks` 는 `pending_canvas_images > 0` 이라 못 푼다
                //   -> ★예약 비트는 이미 소비됐다★(`swap(false)`)
                //   -> 이미지가 여기 도착해 `pending_canvas_images` 를 비우지만 예약이 없다
                //   -> 다시 올 틱이 없다 -> ack 가 영영 안 나가고 문서가 영영 잠긴다
                // ```
                //
                // 실측(2026-10-22, servoshell `-z`, 주입·킬스위치 없음, 1000x700 캔버스 4 개를
                // rAF 마다 다시 그리는 페이지): 이 줄이 없으면 10 초 동안 `FRAMEREASON` 0 줄
                // (= 프레임 0), 있으면 초당 수십 장으로 계속 돈다.
                //
                // ★비디오도 같은 문에 걸려 있었다.★ 비디오 프레임 도착은 Task 7 이후 순수
                // 적재이고(설계 2-6), 그 적재를 화면에 올리는 것은 틱이다. 수요 구동
                // 임베더에서 이 예약이 없으면 그 틱이 안 와서 불변식 4 의 ***바닥***("비디오는
                // 아예 없음이 아닌 페인트 박자")이 무너진다. 천장은 그대로 틱이 준다 --
                // 비트는 에지당 한 번만 소비되므로 도착이 초당 1,080 번이어도 생산은 틱마다
                // 한 장이다.
                crate::wall_tick::schedule_production_at_next_edge();
                IMAGE_UPDATE_STATS.with(|stats| {
                    let mut stats = stats.borrow_mut();
                    stats.calls += 1;
                    stats.total_ms += arm_started.elapsed().as_secs_f64() * 1000.0;
                    stats.inner_ms += inner_ms;
                    let window = stats.window_start.get_or_insert_with(Instant::now);
                    let elapsed = window.elapsed();
                    if elapsed >= Duration::from_secs(1) {
                        warn!(
                            "IMGUPDRATE window_ms={:.0} calls={} total_ms={:.1} inner_ms={:.1} fanout_ms={:.1} dropped={}",
                            elapsed.as_secs_f64() * 1000.0,
                            stats.calls,
                            stats.total_ms,
                            stats.inner_ms,
                            (stats.total_ms - stats.inner_ms).max(0.0),
                            DETACHED_DROPPED.with(|dropped| dropped.replace(0)),
                        );
                        *stats = ImageUpdateStats {
                            window_start: Some(Instant::now()),
                            ..Default::default()
                        };
                    }
                });
            },
            PaintMessage::DelayNewFrameForCanvas(
                webview_id,
                pipeline_id,
                canvas_epoch,
                image_keys,
            ) => {
                self.for_each_webview_painter_mut(webview_id, move |painter| {
                    painter.delay_new_frames_for_canvas(
                        pipeline_id,
                        canvas_epoch,
                        image_keys.clone(),
                    );
                });
            },
            PaintMessage::AddFont(painter_id, font_key, data, index) => {
                debug_assert!(painter_id == font_key.into());

                self.for_each_source_painter_target_mut(painter_id, move |painter| {
                    painter.add_font(font_key, data.clone(), index);
                });
            },
            PaintMessage::AddSystemFont(painter_id, font_key, native_handle) => {
                debug_assert!(painter_id == font_key.into());

                self.for_each_source_painter_target_mut(painter_id, move |painter| {
                    painter.add_system_font(font_key, native_handle.clone());
                });
            },
            PaintMessage::AddFontInstance(
                painter_id,
                font_instance_key,
                font_key,
                size,
                flags,
                variations,
            ) => {
                debug_assert!(painter_id == font_key.into());
                debug_assert!(painter_id == font_instance_key.into());

                self.for_each_source_painter_target_mut(painter_id, move |painter| {
                    painter.add_font_instance(
                        font_instance_key,
                        font_key,
                        size,
                        flags,
                        variations.clone(),
                    );
                });
            },
            PaintMessage::RemoveFonts(painter_id, keys, instance_keys) => {
                self.for_each_source_painter_target_mut(painter_id, move |painter| {
                    painter.remove_fonts(keys.clone(), instance_keys.clone());
                });
            },
            PaintMessage::GenerateFontKeys(
                number_of_font_keys,
                number_of_font_instance_keys,
                result_sender,
                painter_id,
            ) => {
                self.handle_generate_font_keys(
                    number_of_font_keys,
                    number_of_font_instance_keys,
                    result_sender,
                    painter_id,
                );
            },
            PaintMessage::Viewport(webview_id, viewport_description) => {
                self.for_each_webview_painter_mut(webview_id, move |painter| {
                    painter.set_viewport_description(webview_id, viewport_description.clone());
                });
            },
            PaintMessage::ScreenshotReadinessReponse(webview_id, pipelines_and_epochs) => {
                self.with_primary_painter(webview_id, move |painter| {
                    painter.handle_screenshot_readiness_reply(webview_id, pipelines_and_epochs)
                });
            },
            PaintMessage::SendLCPCandidate(lcp_candidate, webview_id, pipeline_id, epoch) => {
                self.for_each_webview_painter_mut(webview_id, move |painter| {
                    painter.append_lcp_candidate(
                        lcp_candidate.clone(),
                        webview_id,
                        pipeline_id,
                        epoch,
                    );
                });
            },
            PaintMessage::EnableLCPCalculation(webview_id) => {
                self.for_each_webview_painter_mut(webview_id, move |painter| {
                    painter.enable_lcp_calculation(&webview_id);
                });
            },
        }
    }

    pub fn remove_webview(&mut self, webview_id: WebViewId) {
        self.coalesced_wall_frame_requests
            .borrow_mut()
            .remove(&webview_id);
        self.last_wall_frame_issue_at
            .borrow_mut()
            .remove(&webview_id);
        self.wall_frame_coordinator
            .borrow_mut()
            .remove_webview(webview_id);
        let painter_ids = self.remove_webview_painter_targets(webview_id);

        for painter_id in painter_ids {
            // `drop(painter)` 로 빌림을 끊던 자리다 — 클로저가 끝나면서 같은 일을 하므로
            // `remove_painter` 가 `painters` 를 다시 빌려도 겹치지 않는다.
            let should_remove_painter = self.with_painter_mut(painter_id, move |painter| {
                painter.remove_webview(webview_id);
                painter.is_empty()
            });

            if should_remove_painter == Some(true) {
                self.remove_painter(painter_id);
            }
        }
    }

    fn collect_memory_report(&self, sender: profile_traits::mem::ReportsChan) {
        let mut memory_report = MemoryReport::default();
        for painter_id in self.painter_ids() {
            if let Some(report) = self.with_painter(painter_id, |painter| painter.report_memory()) {
                memory_report += report;
            }
        }

        let mut reports = vec![
            Report {
                path: path!["webrender", "fonts"],
                kind: ReportKind::ExplicitJemallocHeapSize,
                size: memory_report.fonts,
            },
            Report {
                path: path!["webrender", "images"],
                kind: ReportKind::ExplicitJemallocHeapSize,
                size: memory_report.images,
            },
            Report {
                path: path!["webrender", "display-list"],
                kind: ReportKind::ExplicitJemallocHeapSize,
                size: memory_report.display_list,
            },
        ];

        perform_memory_report(|ops| {
            let scroll_trees_memory_usage = self
                .painter_ids()
                .into_iter()
                .map(|painter_id| {
                    // `MallocSizeOfOps` 는 `dyn FnMut` 을 품고 있어 스레드를 건너지 못한다.
                    self.with_painter_not_thread_ready(painter_id, "Memory report", |painter| {
                        painter.scroll_trees_memory_usage(ops)
                    })
                    .unwrap_or_default()
                })
                .sum();
            reports.push(Report {
                path: path!["paint", "scroll-tree"],
                kind: ReportKind::ExplicitJemallocHeapSize,
                size: scroll_trees_memory_usage,
            });
        });

        sender.send(ProcessReports::new(reports));
    }

    fn handle_new_display_list(
        &self,
        webview_id: WebViewId,
        display_list_descriptor: BuiltDisplayListDescriptor,
        display_list_info_receiver: GenericReceiver<PaintDisplayListInfo>,
        display_list_data_receiver: GenericReceiver<SerializableDisplayListPayload>,
    ) {
        let Ok(display_list_info) = display_list_info_receiver.recv() else {
            return log::error!("Could not receive display list info");
        };
        let Ok(display_list_data) = display_list_data_receiver.recv() else {
            return log::error!("Could not receive display list data");
        };

        for painter_id in self.painter_targets_for_webview(webview_id) {
            let display_list_descriptor = display_list_descriptor.clone();
            let display_list_info = display_list_info.clone();
            let display_list_data = display_list_data.clone();
            self.with_painter_mut(painter_id, move |painter| {
                painter.handle_new_display_list(
                    webview_id,
                    display_list_descriptor,
                    display_list_info,
                    display_list_data,
                )
            });
        }
    }

    /// Handle messages sent to `Paint` during the shutdown process. In general,
    /// the things `Paint` can do in this state are limited. It's very important to
    /// answer any synchronous messages though as other threads might be waiting on the
    /// results to finish their own shut down process. We try to do as little as possible
    /// during this time.
    ///
    /// When that involves generating WebRender ids, our approach here is to simply
    /// generate them, but assume they will never be used, since once shutting down
    /// `Paint` no longer does any WebRender frame generation.
    fn handle_browser_message_while_shutting_down(&self, msg: PaintMessage) {
        match msg {
            PaintMessage::PipelineExited(webview_id, pipeline_id, pipeline_exit_source) => {
                self.with_primary_painter_mut(webview_id, move |painter| {
                    painter.notify_pipeline_exited(webview_id, pipeline_id, pipeline_exit_source)
                });
            },
            PaintMessage::GenerateImageKey(webview_id, result_sender) => {
                self.handle_generate_image_key(webview_id, result_sender);
            },
            PaintMessage::GenerateImageKeysForPipeline(webview_id, pipeline_id) => {
                self.handle_generate_image_keys_for_pipeline(webview_id, pipeline_id);
            },
            PaintMessage::GenerateFontKeys(
                number_of_font_keys,
                number_of_font_instance_keys,
                result_sender,
                painter_id,
            ) => {
                self.handle_generate_font_keys(
                    number_of_font_keys,
                    number_of_font_instance_keys,
                    result_sender,
                    painter_id,
                );
            },
            _ => {
                debug!("Ignoring message ({:?} while shutting down", msg);
            },
        }
    }

    pub fn add_webview(
        &self,
        webview: Box<dyn WebViewTrait>,
        viewport_details: ViewportDetails,
        viewport_origin: DeviceVector2D,
    ) {
        let webview_id = webview.id();
        self.webviews.borrow_mut().insert(webview_id, webview);
        let painter_id: PainterId = webview_id.into();
        self.register_webview_painter_target(webview_id, painter_id);
        self.with_painter_mut(painter_id, move |painter| {
            painter.add_webview(webview_id, viewport_details, viewport_origin)
        })
        .expect("painter_id not found");
    }

    pub fn add_webview_paint_target(
        &mut self,
        webview: Box<dyn WebViewTrait>,
        rendering_context: Rc<dyn RenderingContext>,
        viewport_details: ViewportDetails,
        viewport_origin: DeviceVector2D,
    ) -> PainterId {
        let webview_id = webview.id();
        self.webviews.borrow_mut().insert(webview_id, webview);
        let painter_id = self.register_rendering_context(rendering_context);
        self.register_webview_painter_target(webview_id, painter_id);
        self.with_painter_mut(painter_id, move |painter| {
            painter.add_webview(webview_id, viewport_details, viewport_origin)
        })
        .expect("painter_id not found");
        painter_id
    }

    /// [`Self::add_webview_paint_target`] 과 같되, 이 타일은 **자기 스레드**를 갖고 그
    /// 스레드가 컨텍스트와 `Painter` 를 직접 만든다(설계 §6.1).
    pub fn add_webview_paint_target_on_own_thread(
        &mut self,
        webview: Box<dyn WebViewTrait>,
        factory: RenderingContextFactory,
        viewport_details: ViewportDetails,
        viewport_origin: DeviceVector2D,
    ) -> Option<PainterId> {
        let webview_id = webview.id();
        let painter_id = self.spawn_painter_thread(factory)?;
        // 스레드가 살아난 뒤에만 임베더 핸들을 등록한다 — 실패하면 아무것도 남기지 않는다.
        self.webviews.borrow_mut().insert(webview_id, webview);
        self.register_webview_painter_target(webview_id, painter_id);
        self.with_painter_mut(painter_id, move |painter| {
            painter.add_webview(webview_id, viewport_details, viewport_origin)
        })
        .expect("painter_id not found");
        Some(painter_id)
    }

    pub fn show_webview(&self, webview_id: WebViewId) -> Result<(), UnknownWebView> {
        self.set_webview_hidden_on_all_painters(webview_id, false)
    }

    pub fn hide_webview(&self, webview_id: WebViewId) -> Result<(), UnknownWebView> {
        self.set_webview_hidden_on_all_painters(webview_id, true)
    }

    /// ★브로드캐스트 헬퍼를 쓰지 않는 이유★: 이 둘은 painter 들의 결과를 **모아서** 하나의
    /// `Result` 를 낸다. 클로저가 바깥 지역변수를 갱신하는 형태는 painter 가 스레드로 가면
    /// 성립하지 않으므로(클로저는 건너가고 지역변수는 남는다), 애초에 **각 호출의 반환값을
    /// 받는** 모양으로 둔다. 그러면 나중에 팬아웃/조인으로 바뀌어도 이 코드는 그대로다.
    fn set_webview_hidden_on_all_painters(
        &self,
        webview_id: WebViewId,
        hidden: bool,
    ) -> Result<(), UnknownWebView> {
        let mut result = Ok(());
        let mut saw_painter = false;
        for painter_id in self.painter_targets_for_webview(webview_id) {
            let Some(painter_result) = self.with_painter_mut(painter_id, move |painter| {
                painter.set_webview_hidden(webview_id, hidden)
            }) else {
                continue;
            };
            saw_painter = true;
            if result.is_ok() {
                result = painter_result;
            }
        }
        if saw_painter {
            result
        } else {
            Err(UnknownWebView(webview_id))
        }
    }

    pub fn set_hidpi_scale_factor(
        &self,
        webview_id: WebViewId,
        new_scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,
    ) {
        if self.shutdown_state() != ShutdownState::NotShuttingDown {
            return;
        }
        self.with_primary_painter_mut(webview_id, move |painter| {
            painter.set_hidpi_scale_factor(webview_id, new_scale_factor)
        })
        .expect("painter_id not found");
    }

    pub fn set_viewport_details(&self, webview_id: WebViewId, viewport_details: ViewportDetails) {
        if self.shutdown_state() != ShutdownState::NotShuttingDown {
            return;
        }
        self.for_each_webview_painter_mut(webview_id, move |painter| {
            painter.set_viewport_details(webview_id, viewport_details);
        });
    }

    pub fn resize_rendering_context(&self, webview_id: WebViewId, new_size: PhysicalSize<u32>) {
        if self.shutdown_state() != ShutdownState::NotShuttingDown {
            return;
        }
        self.with_primary_painter_mut(webview_id, move |painter| {
            painter.resize_rendering_context(new_size)
        })
        .expect("painter_id not found");
    }

    pub fn update_webview_paint_target(
        &self,
        webview_id: WebViewId,
        painter_id: PainterId,
        new_size: PhysicalSize<u32>,
        viewport_details: ViewportDetails,
        viewport_origin: DeviceVector2D,
    ) {
        if self.shutdown_state() != ShutdownState::NotShuttingDown {
            return;
        }
        if !self.webview_has_painter_target(webview_id, painter_id) {
            warn!("Ignoring update for unregistered paint target {painter_id:?} on {webview_id:?}");
            return;
        }

        self.with_painter_mut(painter_id, move |painter| {
            painter.resize_rendering_context(new_size);
            painter.set_viewport_details_and_origin(webview_id, viewport_details, viewport_origin);
        })
        .expect("painter_id not found");
    }

    pub fn set_page_zoom(&self, webview_id: WebViewId, new_zoom: f32) {
        if self.shutdown_state() != ShutdownState::NotShuttingDown {
            return;
        }
        self.for_each_webview_painter_mut(webview_id, move |painter| {
            painter.set_page_zoom(webview_id, new_zoom);
        });
    }

    pub fn page_zoom(&self, webview_id: WebViewId) -> f32 {
        self.with_primary_painter(webview_id, move |painter| painter.page_zoom(webview_id))
            .expect("painter_id not found")
    }

    /// Render the WebRender scene to the active `RenderingContext`.
    pub fn render(&self, webview_id: WebViewId) {
        // `&self` 를 통째로 캡처하지 않도록 채널만 떼어 온다 — `Paint` 는 `Cell`/`RefCell`
        // 투성이라 스레드로 건널 수 없다.
        let time_profiler_chan = self.time_profiler_chan.clone();
        self.with_primary_painter_mut(webview_id, move |painter| {
            painter.render(&time_profiler_chan)
        })
        .expect("painter_id not found");
    }

    /// 여러 paint target 을 **한꺼번에 보내고**, 아직 기다리지 않는다.
    ///
    /// 자기 스레드를 가진 타일은 발사되어 곧장 그리기 시작하고, 메인에 있는 타일은 그
    /// 자리에서 그린다. 돌려받은 토큰을 [`PaintTargetsInFlight::join`] 할 때까지 스레드
    /// 타일들은 나란히 돈다 — 호출자는 그 사이에 **자기 일(예: primary 타일)** 을 할 수 있고,
    /// 그것이 이 두 단계 API 의 존재 이유다. 한 번에 보내고 바로 기다리면 primary 는 겹치지
    /// 못한다.
    ///
    /// 등록되지 않은 target 은 조용히 건너뛰지 않고 경고한다(기존 `render_paint_target` 과
    /// 같은 판정).
    pub fn dispatch_paint_targets(
        &self,
        webview_id: WebViewId,
        painter_ids: &[PainterId],
    ) -> PaintTargetsInFlight {
        let registered = self.painter_targets_for_webview(webview_id);
        let mut in_flight = Vec::new();
        let mut inline_painter_ids = Vec::new();

        for &painter_id in painter_ids {
            if !registered.contains(&painter_id) {
                warn!(
                    "Ignoring render for unregistered paint target {painter_id:?} of {webview_id:?}"
                );
                continue;
            }
            let Some(host) = self
                .painters
                .iter()
                .find(|host| host.painter_id == painter_id)
            else {
                continue;
            };
            match &host.kind {
                PainterHostKind::Threaded(threaded) => {
                    let time_profiler_chan = self.time_profiler_chan.clone();
                    // 타일이 자기 시간을 재서 돌려준다 — 메인은 이 타일이 언제 시작하고
                    // 끝났는지 볼 수 없다.
                    //
                    // ★보낸 시점도 같이 넘긴다.★ 그래야 타일 스레드가 이 요청을 **집기까지**
                    // 걸린 시간을 알 수 있다. 요청 채널은 무한 큐이고 이미지 갱신
                    // (`dispatch_detached`)과 공유하므로, 렌더 요청이 그 뒤에 서 있었는지가
                    // 조인 대기의 큰 몫일 수 있다.
                    let dispatched_at = Instant::now();
                    let dispatched = threaded.dispatch(move |painter| {
                        let started = Instant::now();
                        let queued_ms =
                            started.duration_since(dispatched_at).as_secs_f64() * 1000.0;
                        painter.render(&time_profiler_chan);
                        TileRenderTiming {
                            queued_ms,
                            render_ms: started.elapsed().as_secs_f64() * 1000.0,
                        }
                    });
                    if let Some(done) = dispatched {
                        in_flight.push(done);
                    }
                },
                // 메인에 있는 타일은 발사할 곳이 없다. 스레드들이 이미 돌기 시작한 뒤에
                // 그리도록 **모아 두었다가 아래에서** 실행한다.
                PainterHostKind::Inline(_) => inline_painter_ids.push(painter_id),
            }
        }

        for painter_id in inline_painter_ids {
            let time_profiler_chan = self.time_profiler_chan.clone();
            self.with_painter_mut(painter_id, move |painter| {
                painter.render(&time_profiler_chan)
            });
        }

        PaintTargetsInFlight { in_flight }
    }

    /// Render one registered paint target for the given logical `WebView`.
    pub fn render_paint_target(&self, webview_id: WebViewId, painter_id: PainterId) {
        if !self
            .painter_targets_for_webview(webview_id)
            .contains(&painter_id)
        {
            warn!("Ignoring render for unregistered paint target {painter_id:?} of {webview_id:?}");
            return;
        }

        let time_profiler_chan = self.time_profiler_chan.clone();
        self.with_painter_mut(painter_id, move |painter| {
            painter.render(&time_profiler_chan)
        });
    }

    /// ★이 패스의 커밋이 떨어져야 할 격자점(절대 QPC).★
    ///
    /// 커밋은 지금까지 렌더 패스의 **끝**에서 나갔다. `-DwmAlign` 이 고정하는 것은 패스의
    /// **시작**(틱)이므로 커밋 시각 = `틱 + 패스 길이` 였고, 커밋 위상이 패스 길이를 그대로
    /// 물려받았다. 그 산포가 저더의 두 증상을 모두 만든다 -- 큰 탈선은 합성 마감을 스치고
    /// (`MISSEVENT nc=1`), 그 흔들림이 커밋 간격을 흔들어 합성 경계를 넘나든다(`nc=0`).
    /// 설계 문서 §21~27 과 `2026-10-06-commit-on-composition-grid-design.md` §1.
    ///
    /// ★타일과 무관하다 -- 패스당 하나다.★ DWM 은 네 타일을 하나의 합성 패스에서 함께
    /// 올리므로(네 디바이스의 `lastFrameTime` 이 동일하다) 맞출 격자도 하나다. 예전
    /// `deadline_for_monitor` 는 출력별 격자에 맞췄고, 그것이 B1 이 효과를 내지 못한 이유다.
    ///
    /// 묵은 격자 검사가 없는 것이 맞다. `grid_for_monitor` 는 프로브 스레드의 캐시라 최대
    /// ~100ms 낡을 수 있어 검사가 필요했지만, `composition_grid()` 는 매번 조회하는 시스템
    /// 콜이라 묵을 수가 없다. 들고 가면 영원히 거짓인 분기가 남는다.
    #[cfg(windows)]
    fn commit_deadline() -> Option<u64> {
        // 캐시된 값이다 -- `pref!` 는 RwLock 획득이고 이 함수는 패스마다 돌다
        // (`commit_scheduler::COMMIT_ALIGN_PCT` 주석).
        let pct = (*crate::commit_scheduler::COMMIT_ALIGN_PCT)?;
        let (vblank, period) = crate::dcomp_compositor::composition_grid()?;
        let tick = crate::output_grid::tick_qpc()?;
        crate::output_grid::next_grid_point(tick, vblank, period, pct)
    }

    /// 마감을 못 구해 즉시 커밋으로 떨어진 타일 수를 세고, 초당 한 번 알린다.
    ///
    /// ★원인이 셋이다.★ 마감은 pref·공통 격자·틱 셋을 모두 갖춰야 나오므로, 폴백은 그중 아무
    /// 하나가 비었다는 뜻이다 -- (1) `composition_grid()` 실패(DWM 합성 꺼짐 등), (2) `tick_qpc()`
    /// 가 `None`(셸이 `servo::note_present_tick()` 을 부르지 않는다), (3) 그 타일의 `monitor` 가
    /// `None`. ★어느 것인지 말하지 않으면 엉뚱한 곳을 보게 된다★ -- 예전 이 줄은 "출력 격자" 를
    /// 지목하고 `[outgrid]`/`OUTPHASE` 를 보라고 했는데, 지금 마감은 그 프로브 격자를 전혀 읽지
    /// 않으므로 그 로그는 멀쩡해 보인다.
    ///
    /// ★once-warn 이 아니라 지속 카운터다.★ 기동 지후 한두 프레임은 정상이지만, 셸이 틱을 안
    /// 찍는 구성(원인 (2))이면 **영구**가 된다. 그 둘은 "한 번 났다" 로는 구분되지 않고, 초당
    /// 카운트가 계속 찍히느냐로만 구분된다. 조용히 폴백하면 정렬이 아무 일도 안 하는 채로 벽이
    /// 계속 돌고 아무도 모른다.
    #[cfg(windows)]
    fn note_grid_fallback(count: u64) {
        static FALLBACKS: AtomicU64 = AtomicU64::new(0);
        static LAST_WARN: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
        FALLBACKS.fetch_add(count, Ordering::Relaxed);
        let Ok(mut last) = LAST_WARN.lock() else {
            return;
        };
        let now = Instant::now();
        let due = match *last {
            Some(at) => now.duration_since(at) >= Duration::from_secs(1),
            None => true,
        };
        if !due {
            return;
        }
        *last = Some(now);
        let taken = FALLBACKS.swap(0, Ordering::Relaxed);
        // 어느 입력이 비었는지를 줄에 집어 넣는다. 세 원인은 서로 전혀 다른 곳을 가리킨다.
        let grid = crate::dcomp_compositor::composition_grid().is_some();
        let tick = crate::output_grid::tick_qpc().is_some();
        warn!(
            "[commitsched] 마감을 못 구해 즉시 커밋으로 폴백한 타일 {taken} 건/초 -- \
             정렬이 이 타일들에는 걸리지 않는다. dwm_grid={grid} tick={tick} -- \
             grid=false 는 DWM 합성이 꺼졌다는 뜻이고, tick=false 는 셸이 \
             servo::note_present_tick() 을 부르지 않는다는 뜻이다(그러면 이 기능은 \
             아무 일도 하지 않는다). 둘 다 true 면 그 타일의 monitor 를 못 구한 것이다"
        );
    }

    /// 격자 폴백 타일들을 **지금** 커밋한다. 병렬 커밋 설정을 그대로 따른다 -- 이 경로는
    /// 정의상 "정렬이 안 걸린 타일" 이므로 정렬이 꺼져 있을 때와 같게 나가야 한다.
    ///
    /// ★그래도 디바이스 가드가 걸린다.★ 이 폴백이 나오는 바로 그 상황(핫플러그 직후
    /// `monitor_for_hwnd` 가 새 HMONITOR 를 주는데 `grid_for_monitor` 가 아직 못 채웠다)에서,
    /// 옛 모니터를 향해 이전에 건 스케줄이 이 디바이스에 대해 아직 큐에 남아 있을 수 있다
    /// (최대 두 주기). `schedule()` 은 upsert 라 이 경로가 다시 `schedule()` 을 부르지 않는 한
    /// 그 옛 항목을 밀어내지 못한다 -- 즉 이 즉시 커밋은 스케줄러 스레드와 이 디바이스를 두고
    /// 겹칠 수 있다.
    ///
    /// 가드를 여기서 직접 잡지 않는 것에 주의하라: `commit_device_ptr` 이 정문이고 그 안에서
    /// 잡는다(`dcomp_compositor::raw_commit` 주석). 커밋 지점마다 가드를 따로 챙기는 방식이
    /// 세 번 새는 바람에 그 책임을 한 곳으로 옮겼다 -- 이 함수가 할 일은 "지금 낸다" 와
    /// "병렬 설정을 따른다" 뿐이다.
    #[cfg(windows)]
    fn commit_now_guarded(devices: Vec<usize>) {
        if devices.len() > 1 && servo_config::pref!(gfx_dcomp_parallel_commit) {
            std::thread::scope(|scope| {
                for device in devices {
                    scope.spawn(move || {
                        crate::dcomp_compositor::commit_device_ptr(device);
                    });
                }
            });
            return;
        }
        for device in devices {
            crate::dcomp_compositor::commit_device_ptr(device);
        }
    }

    /// ★낡음(2026-09-02 기록, 같은 날 `d3fa6dbd686` 이후 무효)★ — pref 는 `gfx_dcomp_commit_in_end_frame`(기본 false = 미룸)이다. 옛 이름으로는 아무것도 안 잡힌다.
    /// 미뤄 둔 DComp Commit 을 모든 painter 에서 흘린다(`gfx_dcomp_defer_commit`).
    ///
    /// 셸이 타일 루프를 마친 뒤 한 번 부른다. 목적은 render→commit 을 네 번 번갈아 도는
    /// 대신 렌더 넷을 먼저 몰고 Commit 넷을 몰아, Commit 의 대기가 서로 겹칠 수 있는지를
    /// 스레드 없이 확인하는 것이다.
    pub fn flush_deferred_dcomp_commits(&self) {
        // ★네 타일을 **하나의** 공통 합성 격자에 맞춘다.★ DWM 은 네 타일을 한 합성 패스에서
        // 함께 올린다(네 디바이스의 `lastFrameTime` 이 동일하다). 그러므로 맞출 격자도 하나고,
        // 마감도 패스당 하나다.
        //
        // ★예전에는 여기에 그 반대가 적혀 있었다★ -- "타일마다 자기 출력의 격자에 맞춘다, 네
        // 모니터의 vblank 가 주기의 0.67 에 흩어져 있으니 교집합이 공집합이다". 그것이 B1 의
        // 생각이었고, 그렇게 해서 **듣지 않았다**. 출력별 vblank 는 맞출 대상이 아니다 -- 맞출
        // 것은 DWM 의 합성 시점이고 그것은 하나다. 출력별 위상차는 여전히 있지만 그것은 이
        // 함수가 아니라 B2(`gfx_sample_lead_periods`)의 일이다.
        // 설계 문서 §21~27 과 `2026-10-06-commit-on-composition-grid-design.md`.
        //
        // 여기서는 **거는 것만** 한다. 실제 Commit 은 스케줄러 스레드가 그 시각에 낸다.
        // 이 함수는 메인(또는 스레드된 painter 로의 왕복)에서 도므로 여기서 기다리면
        // 그대로 패스가 길어진다.
        #[cfg(windows)]
        if crate::commit_scheduler::COMMIT_ALIGN_PCT.is_some() {
            // ★프로브는 이 기능이 켜지면 뜬다.★ 단, 마감은 이제 프로브의 격자를 전혀 읽지
            // 않는다(공통 격자만 쓴다). 그래도 띄우는 이유는 `OUTCOMMIT`/`OUTPHASE` 의 위상
            // 표본이 거기서 나오기 때문이다 -- 그것이 위상 법칙의 계측이다.
            //
            // 아래는 그 함수가 전에 다른 이유로 필요했던 사정이다(지금은 해당 없다): 예전에는 유일한 호출처가 `note_dwm_phase`
            // 안이었고 그 함수는 `SERVO_DCOMP_BIND_PROF` 뒤에서 시작한다 -- 즉 진단 플래그
            // 없이 정렬 pref 만 준 운영 구성에서는 격자가 영영 비어 전 타일이 매
            // 프레임 즉시 커밋 폴백을 탔고, 그 사실을 알리는 로그조차 없었다. `start_probe`
            // 는 `Once` 라 여러 곳에서 불러도 스레드는 하나다.
            crate::output_grid::start_probe();
            // ★마감이 패스보다 먼저 오면 기능이 켜진 채 아무 일도 하지 않는다.★ 거의 매
            // 프레임 즉시 커밋으로 떨어지는데, 로그에는 정렬이 켜진 것으로 보인다.
            //
            // 15% 는 프로브의 패스 p95(주기의 13.4%)에 여유를 조금 준 값이다. 패스 p95 는
            // 기동 시점에 알 수 없으므로 상수로 둔다. 막지는 않는다 -- 스윈에서 일부러 좁게
            // 주는 경우가 있고, 그때 무슨 일이 생기는지 보는 것이 측정의 일부다.
            {
                static WARNED_GAP: std::sync::Once = std::sync::Once::new();
                WARNED_GAP.call_once(|| {
                    let Some(commit) = *crate::commit_scheduler::COMMIT_ALIGN_PCT else {
                        return;
                    };
                    let tick = servo_config::pref!(gfx_present_align_dwm_pct);
                    // `-DwmAlign` 이 꺼지면 틱에 고정 위상이 없으니 보할 것이 없다.
                    if !(0..=99).contains(&tick) {
                        return;
                    }
                    let slack = crate::output_grid::commit_slack_pct(commit, tick as u64);
                    if slack < 15 {
                        warn!(
                            "[commitsched] gfx_present_align_commit_pct={commit} 은 \
                             gfx_present_align_dwm_pct={tick} 에서 주기의 {slack}% 뿐 떨어져 \
                             있다 -- 마감이 패스보다 먼저 와서 거의 매 프레임 즉시 \
                             커밋으로 떨어질 수 있다(프로브의 패스 p95 가 주기의 13.4%). \
                             OUTCOMMIT 의 slip_us 로 확인할 것"
                        );
                    }
                });
            }
            // ★예전에는 여기서 "무시한다" 고 경고했다.★ B1 때는 커밋 시각이 타일마다
            // 달라 목적이 겹친다고 봤는데, 지금은 네 타일이 **같은** 마감을 받으므로 겹치지
            // 않는다 -- 넷을 동시에 내는 것이 바로 이 설계가 원하는 것이다. 그래서 이 pref 는
            // 이제 스케줄 경로에서도 **적용된다**(`commit_scheduler::PARALLEL_COMMIT`).
            //
            // 끄면 커밋이 순차로 나가고, 2·3·4 번째 타일의 slip 에 앞 타일들의 `Commit()`
            // 시간이 들어간다(실측 상수 ~360µs). A/B 를 재려면 그쪽으로 끄면 된다.
            if !servo_config::pref!(gfx_dcomp_parallel_commit) {
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    warn!(
                        "[commitsched] gfx_present_align_commit_pct 가 켜졌는데 \
                         gfx_dcomp_parallel_commit 이 꺼져 있다 -- 네 커밋이 순차로 나가 \
                         2·3·4 번째 타일이 앞 타일들의 Commit 시간만큼 늦는다(실측 ~360µs). \
                         OUTCOMMIT 의 slip_us_avg 로 확인할 것"
                    );
                });
            }
            // 격자를 못 구한 타일들. ★모아서 기존 병렬 경로로 흘린다★ -- 예전에는 여기서
            // 타일마다 그 자리에서 커밋했는데, 그러면 병렬 커밋이 켜져 있어도 네 커밋이
            // 호출 스레드에서 순차로 나가 4×2.44 = 9.8ms 를 물었다. 폴백이 기능 off 보다
            // 나빠지는 셈이고, "폴백 = 오늘 동작" 이라는 전제가 깨진다.
            let mut fallback: Vec<usize> = Vec::new();
            // ★마감은 패스당 하나다.★ 공통 격자에서 나오므로 타일마다 다시 셀 것이 없고,
            // 네 타일이 **같은** 마감을 받아야 한 합성에 함께 실린다. 루프 안에서 재면
            // 타일마다 조회 시각이 달라 미세하게 다른 답이 나온다.
            let deadline = Self::commit_deadline();
            for painter_id in self.painter_ids() {
                let pending = self
                    .with_painter(painter_id, |painter| {
                        painter
                            .take_pending_dcomp_commit()
                            .map(|device| (device, painter.pending_dcomp_commit_monitor()))
                    })
                    .flatten();
                let Some((device, monitor)) = pending else {
                    continue;
                };
                // `monitor` 는 이제 마감 계산에 쓰이지 않는다 -- `OUTCOMMIT` 이 "어느 타일이
                // 죽었나" 를 말할 수 있게 하는 기록용으로만 스케줄러에 넘긴다.
                match (monitor, deadline) {
                    (Some(monitor), Some(deadline)) => {
                        // ★이 커밋이 어느 패스의 것인지 남긴다.★ 마감이 다음 틱 가까이에
                        // 있으면 커밋이 그 틱을 넘어가고, 그러면 `TICK_QPC` 로는 기준이
                        // 뒤집힌다(`output_grid::COMMIT_TICK` 주석).
                        if let Some(tick) = crate::output_grid::tick_qpc() {
                            crate::output_grid::note_scheduled_for_tick(device, tick);
                        }
                        crate::commit_scheduler::schedule(device, monitor, deadline)
                    },
                    _ => fallback.push(device),
                }
            }
            if !fallback.is_empty() {
                Self::note_grid_fallback(fallback.len() as u64);
                Self::commit_now_guarded(fallback);
            }
            return;
        }
        // `gfx_dcomp_parallel_commit`: 넷을 동시에 건다.
        //
        // 왜: 정상상태 패스 15.4ms 중 Commit 이 4×2.44 = 9.8ms 이고, 60Hz 예산 16.67ms 에
        // 여유가 1.3ms 뿐이라 한 번 넘기면 되먹임으로 42fps 에 눌러앉는다. Commit 은 대기이고
        // 그 대기는 겹치므로(배치 실험 24.90→9.83ms) 동시에 걸면 9.8→~2.4ms 가 되어 여유가
        // 8ms 로 벌어진다. DComp off 가 13.8ms 로 안 빠지는 그 함정을 같은 방식으로 피한다.
        //
        // 디바이스는 painter 마다 다르고, `scope` 가 join 을 보장하므로 이 호출이 돌아올 때
        // 어떤 워커도 남아 있지 않다 — 즉 한 디바이스를 두 스레드가 동시에 만지는 일은 없다.
        #[cfg(windows)]
        if servo_config::pref!(gfx_dcomp_parallel_commit) {
            let pending: Vec<usize> = self
                .painter_ids()
                .into_iter()
                .filter_map(|painter_id| {
                    self.with_painter(painter_id, |painter| painter.take_pending_dcomp_commit())
                        .flatten()
                })
                .collect();
            if pending.len() > 1 {
                std::thread::scope(|scope| {
                    for device in pending {
                        scope.spawn(move || crate::dcomp_compositor::commit_device_ptr(device));
                    }
                });
                return;
            }
            // 하나 이하면 스레드를 띄울 값어치가 없다 — 인계받은 것을 여기서 부른다.
            for device in pending {
                crate::dcomp_compositor::commit_device_ptr(device);
            }
            return;
        }
        for painter_id in self.painter_ids() {
            self.with_painter(painter_id, |painter| painter.flush_deferred_dcomp_commit());
        }
    }

    /// Get the message receiver for this [`Paint`].
    pub fn receiver(&self) -> &RoutedReceiver<PaintMessage> {
        &self.paint_receiver
    }

    #[servo_tracing::instrument(skip_all)]
    /// 한 드레인 안에서 **같은 이미지 키의 낡은 비디오 프레임 갱신을 버린다.** 버린 개수를
    /// 돌려준다.
    ///
    /// 전환 순간 한 번의 드레인이 409건이었고 그 중 385건이 `UpdateImages` 였다. 그런데
    /// `Painter::update_images` 는 epoch 없는 갱신(=비디오 프레임)을 이미 키별 latest-wins
    /// 로 스태시한다 — 같은 키의 앞선 프레임은 **어차피 버려질 운명인데**, 그 앞선 것들까지
    /// 타일 수만큼 팬아웃되어 페인터마다 스태시와 합성 판정을 한 바퀴씩 돌고 있었다.
    /// 여기서 미리 버리면 그 왕복이 통째로 사라진다.
    ///
    /// ★표시되는 프레임 수는 줄지 않는다★ — 한 번의 표출에 같은 영상의 프레임 두 장을
    /// 보여 줄 수는 없고, 이 합침의 범위는 드레인 한 번(수 ms 분량의 도착)이다.
    ///
    /// 의미가 흔들릴 수 있는 키는 손대지 않는다: 이 배치 안에서 추가·삭제·애니메이션
    /// 갱신이 있었거나 epoch 가 붙은(캔버스처럼 스크립트와 짝이 맞아야 하는) 키는 그대로
    /// 둔다. 그런 키에서는 순서가 곧 정확성이다.
    fn drop_superseded_video_frame_updates(messages: &mut Vec<PaintMessage>) -> usize {
        let mut protected: HashSet<ImageKey> = HashSet::new();
        for message in messages.iter() {
            let PaintMessage::UpdateImages(_, updates) = message else {
                continue;
            };
            for update in updates.iter() {
                match update {
                    ImageUpdate::UpdateImage(_, _, _, None) => {},
                    ImageUpdate::AddImage(key, ..)
                    | ImageUpdate::DeleteImage(key)
                    | ImageUpdate::UpdateImageForAnimation(key, _)
                    | ImageUpdate::UpdateImage(key, _, _, Some(_)) => {
                        protected.insert(*key);
                    },
                }
            }
        }

        // 뒤에서 앞으로 훑으며 (페인터, 키)마다 **가장 나중 것**만 남긴다.
        let mut seen: HashSet<(PainterId, SmallVec<[ImageKey; 3]>)> = HashSet::new();
        let mut keep = vec![true; messages.len()];
        let mut dropped = 0;
        for (index, message) in messages.iter().enumerate().rev() {
            let PaintMessage::UpdateImages(painter_id, updates) = message else {
                continue;
            };
            // ★비디오 프레임 한 장은 메시지 하나에 갱신 **여러 건**으로 온다★ — YUV 는
            // 면이 셋이라 `updates_total=3` 이다(실측). 그래서 한 건짜리만 보면 아무것도
            // 잡히지 않는다. 메시지를 **통째로** 버릴 수 있을 때만 버리는 것이 이 합침의
            // 안전 조건이므로, 안의 갱신이 전부 epoch 없는 이미지 갱신일 때만 후보다.
            let mut keys = SmallVec::<[ImageKey; 3]>::new();
            let all_plain_frames = updates.iter().all(|update| {
                if let ImageUpdate::UpdateImage(key, _, _, None) = update {
                    keys.push(*key);
                    true
                } else {
                    false
                }
            });
            if !all_plain_frames || keys.is_empty() {
                continue;
            }
            if keys.iter().any(|key| protected.contains(key)) {
                continue;
            }
            // 같은 프레임의 면들은 함께 와야 뜻이 있으므로, 면 하나가 아니라 **면들의
            // 조합**이 이 메시지의 신원이다.
            if !seen.insert((*painter_id, keys)) {
                keep[index] = false;
                dropped += 1;
            }
        }
        if dropped > 0 {
            let mut index = 0;
            messages.retain(|_| {
                let keep_this = keep[index];
                index += 1;
                keep_this
            });
        }
        dropped
    }

    pub fn handle_messages(&self, mut messages: Vec<PaintMessage>) {
        // ★이 함수는 임베더의 메인 스레드에서 돈다★(`Servo::spin_event_loop`), 그리고 그
        // 스레드에 표출 클럭이 얹혀 있다. 전환 순간 한 번의 드레인이 698건 469ms 로
        // 측정되었는데 -- 건당 0.67ms -- 무엇이 쏟아진 것인지는 아무도 세지 않고 있었다.
        // 세는 비용은 건당 match 하나이므로 건당 0.67ms 옆에서 무시할 수 있다.
        let drain_start = Instant::now();
        let messages_seen = messages.len();
        let mut kinds: Vec<(&'static str, u32)> = Vec::new();
        for message in &messages {
            let kind = message.kind();
            match kinds.iter_mut().find(|(name, _)| *name == kind) {
                Some((_, count)) => *count += 1,
                None => kinds.push((kind, 1)),
            }
        }
        let superseded = Self::drop_superseded_video_frame_updates(&mut messages);
        // Pull out the `NewWebRenderFrameReady` messages from the list of messages and handle them
        // at the end of this function. This prevents overdraw when more than a single message of
        // this type of received. In addition, if any of these frames need a repaint, that reflected
        // when calling `handle_new_webrender_frame_ready`.
        let mut frame_ready_for_painter = HashMap::new();
        let mut frame_ready_diagnostics = Vec::new();
        messages.retain(|message| match message {
            PaintMessage::NewWebRenderFrameReady(painter_id, _document_id, need_repaint) => {
                let need_repaint = *need_repaint;
                let diagnostic = self.with_painter(*painter_id, move |painter| {
                    painter.note_webrender_frame_ready(need_repaint)
                });
                if let Some(diagnostic) = diagnostic {
                    if let Some(diagnostic) = diagnostic {
                        frame_ready_diagnostics.push(diagnostic);
                    } else {
                        Self::record_painter_ready_for_repaint(
                            &mut frame_ready_for_painter,
                            *painter_id,
                            need_repaint,
                        );
                    }
                }

                false
            },
            _ => true,
        });

        for message in messages {
            self.handle_browser_message(message);
            if self.shutdown_state() == ShutdownState::FinishedShuttingDown {
                return;
            }
        }

        {
            let mut wall_frame_coordinator = self.wall_frame_coordinator.borrow_mut();
            for diagnostic in &frame_ready_diagnostics {
                match wall_frame_coordinator.note_frame_ready(diagnostic) {
                    Some(update) => Self::record_wall_frame_repaint_decisions(
                        &mut frame_ready_for_painter,
                        update.repaint_decisions,
                    ),
                    None => Self::record_painter_ready_for_repaint(
                        &mut frame_ready_for_painter,
                        diagnostic.painter_id,
                        diagnostic.need_repaint,
                    ),
                }
            }
            let update = wall_frame_coordinator.sweep_expired_barriers(Instant::now());
            Self::record_wall_frame_repaint_decisions(
                &mut frame_ready_for_painter,
                update.repaint_decisions,
            );
        }

        self.try_release_coalesced_wall_frame_requests();
        self.handle_painters_ready_for_repaint(frame_ready_for_painter);

        // 30ms 는 표출 주기(16.7ms)의 두 배 -- 프레임 두 장을 놓치는 지점부터 관심 대상이다.
        let drain_ms = drain_start.elapsed().as_secs_f64() * 1000.0;
        if drain_ms > 30.0 {
            kinds.sort_by(|left, right| right.1.cmp(&left.1));
            let breakdown = kinds
                .iter()
                .map(|(name, count)| format!("{name}:{count}"))
                .collect::<Vec<_>>()
                .join(",");
            warn!(
                "PAINTDRAIN drain_ms={drain_ms:.1} msgs={messages_seen} superseded={superseded} [{breakdown}]"
            );
        }
    }

    #[servo_tracing::instrument(skip_all)]
    pub fn perform_updates(&self) -> bool {
        if self.shutdown_state() == ShutdownState::FinishedShuttingDown {
            return false;
        }

        // Run the WebXR main thread
        #[cfg(feature = "webxr")]
        self.webxr_main_thread.borrow_mut().run_one_frame();

        for painter_id in self.painter_ids() {
            self.with_painter_mut(painter_id, |painter| painter.perform_updates());
        }

        let wall_frame_repaint_decisions = self
            .wall_frame_coordinator
            .borrow_mut()
            .sweep_expired_barriers(Instant::now());
        let mut frame_ready_for_painter = HashMap::new();
        Self::record_wall_frame_repaint_decisions(
            &mut frame_ready_for_painter,
            wall_frame_repaint_decisions.repaint_decisions,
        );
        self.try_release_coalesced_wall_frame_requests();
        self.handle_painters_ready_for_repaint(frame_ready_for_painter);

        // ★애니메이션 프레임을 정하던 자리는 2026-10-21 에 사라졌다.★ 턴 끝으로 미뤘던
        // 것은 "같은 턴 뒤쪽에서 월 프레임이 날지" 를 그 자리에서 알 수 없었기 때문인데,
        // 생산자가 틱 하나가 되자 그 물음 자체가 없어졌다 -- 애니메이션은 값만 밀어 넣고
        // (`gather_components`), 프레임은 같은 주기의 틱이 하나만 낸다
        // (`produce_wall_frame`). 설계 2-6.

        self.shutdown_state() != ShutdownState::FinishedShuttingDown
    }

    pub fn toggle_webrender_debug(&self, option: WebRenderDebugOption) {
        for painter_id in self.painter_ids() {
            self.with_painter_mut(painter_id, move |painter| {
                painter.toggle_webrender_debug(option)
            });
        }
    }

    pub fn capture_webrender(&self, webview_id: WebViewId) {
        let capture_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        let available_path = [env::current_dir(), Ok(env::temp_dir())]
            .iter()
            .filter_map(|val| {
                val.as_ref()
                    .map(|dir| dir.join("webrender-captures").join(&capture_id))
                    .ok()
            })
            .find(|val| create_dir_all(val).is_ok());

        let Some(capture_path) = available_path else {
            log::error!("Couldn't create a path for WebRender captures.");
            return;
        };

        log::info!("Saving WebRender capture to {capture_path:?}");
        self.with_primary_painter(webview_id, move |painter| {
            painter
                .webrender_api
                .save_capture(capture_path, CaptureBits::all())
        })
        .expect("painter_id not found");
    }

    /// Returning `false` means this is not going to reach the Constellation,
    /// and we need to directly notify the embedder that input event is handled.
    pub fn notify_input_event(&self, webview_id: WebViewId, event: InputEventAndId) -> bool {
        if self.shutdown_state() != ShutdownState::NotShuttingDown {
            return false;
        }
        // Route positional events (mouse/touch) to the painter whose tile viewport contains the
        // point. On the multi-GPU wall a WebView is fanned out to one painter per tile, each with
        // its own `viewport_origin`; without this all input went to the primary painter (tile 0),
        // so points over a secondary tile missed that painter's hit-test region and were dropped.
        let painter_id = event
            .event
            .point()
            .and_then(|point| self.painter_id_containing_input_point(webview_id, point))
            .unwrap_or_else(|| self.primary_painter_id_for_webview(webview_id));
        self.with_painter_mut(painter_id, move |painter| {
            painter.notify_input_event(webview_id, event)
        })
        .expect("painter_id not found")
    }

    /// Finds the fanned-out painter whose tile viewport contains `point` (a positional input
    /// point in the virtual WebView viewport). Returns `None` if no tile contains it.
    fn painter_id_containing_input_point(
        &self,
        webview_id: WebViewId,
        point: WebViewPoint,
    ) -> Option<PainterId> {
        self.painter_targets_for_webview(webview_id)
            .into_iter()
            .find(|&painter_id| {
                self.with_painter(painter_id, move |painter| {
                    painter.rendered_tile_contains_input_point(webview_id, point)
                }) == Some(true)
            })
    }

    pub fn notify_scroll_event(&self, webview_id: WebViewId, scroll: Scroll, point: WebViewPoint) {
        if self.shutdown_state() != ShutdownState::NotShuttingDown {
            return;
        }
        self.with_primary_painter_mut(webview_id, move |painter| {
            painter.notify_scroll_event(webview_id, scroll, point)
        })
        .expect("painter_id not found");
    }

    pub fn adjust_pinch_zoom(
        &self,
        webview_id: WebViewId,
        pinch_zoom_delta: f32,
        center: DevicePoint,
    ) {
        if self.shutdown_state() != ShutdownState::NotShuttingDown {
            return;
        }
        self.with_primary_painter_mut(webview_id, move |painter| {
            painter.adjust_pinch_zoom(webview_id, pinch_zoom_delta, center)
        })
        .expect("painter_id not found");
    }

    pub fn pinch_zoom(&self, webview_id: WebViewId) -> f32 {
        self.with_primary_painter(webview_id, move |painter| painter.pinch_zoom(webview_id))
            .expect("painter_id not found")
    }

    pub fn device_pixels_per_page_pixel(
        &self,
        webview_id: WebViewId,
    ) -> Scale<f32, CSSPixel, DevicePixel> {
        self.with_primary_painter_mut(webview_id, move |painter| {
            painter.device_pixels_per_page_pixel(webview_id)
        })
        .expect("painter_id not found")
    }

    pub(crate) fn shutdown_state(&self) -> ShutdownState {
        self.shutdown_state.get()
    }

    pub fn request_screenshot(
        &self,
        webview_id: WebViewId,
        rect: Option<WebViewRect>,
        callback: Box<dyn FnOnce(Result<RgbaImage, ScreenshotCaptureError>) + 'static>,
    ) {
        // ★여기가 폴링 래치를 세우는 유일한 자리다★ — 요청이 생기는 문이 이것 하나이므로
        // "요청이 있는데 틱이 안 묻는" 경우가 생기지 않는다(`screenshot_poll_latch`).
        // 래치를 내리는 것은 요청이 다 소진된 것을 본 틱이다.
        self.screenshot_poll_latch.set(true);
        self.with_primary_painter_not_thread_ready(
            webview_id,
            // 콜백(`Box<dyn FnOnce(Result<RgbaImage, _>)>`)과
            // `ScreenshotRequestPhase::WaitingOnPipelineDisplayLists(Rc<..>)` 때문이다.
            "Screenshot request",
            move |painter| painter.request_screenshot(webview_id, rect, callback),
        )
        .expect("painter_id not found");
    }

    pub fn notify_input_event_handled(
        &self,
        webview_id: WebViewId,
        input_event_id: InputEventId,
        result: InputEventResult,
    ) {
        self.with_primary_painter_mut(webview_id, move |painter| {
            painter.notify_input_event_handled(webview_id, input_event_id, result)
        });
    }

    /// Generate an image key from the appropriate [`Painter`] or, if it is unknown, generate
    /// a dummy image key. The unknown case needs to be handled because requests for keys
    /// could theoretically come after a [`Painter`] has been released. A dummy key is okay
    /// in this case because we will never render again in that case.
    fn handle_generate_image_key(
        &self,
        webview_id: WebViewId,
        result_sender: GenericSender<ImageKey>,
    ) {
        let painter_id = self.primary_painter_id_for_webview(webview_id);
        let image_key = self
            .with_painter(painter_id, move |painter| {
                painter.webrender_api.generate_image_key()
            })
            .unwrap_or_else(|| ImageKey::new(painter_id.into(), 0));
        let _ = result_sender.send(image_key);
    }

    /// Generate image keys from the appropriate [`Painter`] or, if it is unknown, generate
    /// dummy image keys. The unknown case needs to be handled because requests for keys
    /// could theoretically come after a [`Painter`] has been released. A dummy key is okay
    /// in this case because we will never render again in that case.
    fn handle_generate_image_keys_for_pipeline(
        &self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
    ) {
        let painter_id = self.primary_painter_id_for_webview(webview_id);
        // 한 번의 `with_painter` 안에서 전부 만든다 — painter 가 스레드로 가면 키 하나마다
        // 왕복하는 형태는 그대로 쓸 수 없다.
        let image_keys = self
            .with_painter(painter_id, move |painter| {
                (0..pref!(image_key_batch_size))
                    .map(|_| painter.webrender_api.generate_image_key())
                    .collect()
            })
            .unwrap_or_else(|| {
                (0..pref!(image_key_batch_size))
                    .map(|_| ImageKey::new(painter_id.into(), 0))
                    .collect()
            });

        let _ = self.embedder_to_constellation_sender.send(
            EmbedderToConstellationMessage::SendImageKeysForPipeline(pipeline_id, image_keys),
        );
    }

    /// Generate font keys from the appropriate [`Painter`] or, if it is unknown, generate
    /// dummy font keys. The unknown case needs to be handled because requests for keys
    /// could theoretically come after a [`Painter`] has been released. A dummy key is okay
    /// in this case because we will never render again in that case.
    fn handle_generate_font_keys(
        &self,
        number_of_font_keys: usize,
        number_of_font_instance_keys: usize,
        result_sender: GenericSender<(Vec<FontKey>, Vec<FontInstanceKey>)>,
        painter_id: PainterId,
    ) {
        // 두 벌 모두 한 번의 `with_painter` 안에서 만든다 — 키 하나마다 왕복하는 형태는
        // painter 가 스레드로 가면 그대로 쓸 수 없다.
        let (font_keys, font_instance_keys) = self
            .with_painter(painter_id, move |painter| {
                (
                    (0..number_of_font_keys)
                        .map(|_| painter.webrender_api.generate_font_key())
                        .collect(),
                    (0..number_of_font_instance_keys)
                        .map(|_| painter.webrender_api.generate_font_instance_key())
                        .collect(),
                )
            })
            .unwrap_or_else(|| {
                (
                    (0..number_of_font_keys)
                        .map(|_| FontKey::new(painter_id.into(), 0))
                        .collect(),
                    (0..number_of_font_instance_keys)
                        .map(|_| FontInstanceKey::new(painter_id.into(), 0))
                        .collect(),
                )
            });

        let _ = result_sender.send((font_keys, font_instance_keys));
    }
}

#[cfg(test)]
mod superseded_video_frame_tests {
    use paint_api::{ImageUpdate, PaintMessage, SerializableImageData};
    use servo_base::id::PainterId;
    use webrender_api::{
        ExternalImageData, ExternalImageId, ExternalImageType, ImageBufferKind, ImageDescriptor,
        ImageDescriptorFlags, ImageFormat, ImageKey,
    };

    use super::Paint;

    fn painter(index: u32) -> PainterId {
        webrender_api::IdNamespace(index).into()
    }

    fn key(painter: PainterId, index: u32) -> ImageKey {
        ImageKey(painter.into(), index)
    }

    fn descriptor() -> ImageDescriptor {
        ImageDescriptor::new(16, 16, ImageFormat::BGRA8, ImageDescriptorFlags::IS_OPAQUE)
    }

    fn external() -> SerializableImageData {
        SerializableImageData::External(ExternalImageData {
            id: ExternalImageId(1),
            channel_index: 0,
            image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
            normalized_uvs: false,
        })
    }

    /// 비디오 프레임 한 장(epoch 없음). 면이 하나인 경우.
    fn frame(painter: PainterId, image: ImageKey) -> PaintMessage {
        planar_frame(painter, &[image])
    }

    /// 비디오 프레임 한 장을 **면 여러 개**로. 실제로 오는 모양이다 — YUV 는 면이 셋이라
    /// 메시지 하나에 갱신 세 건이 실려 온다(실측 `updates_total=3`).
    fn planar_frame(painter: PainterId, planes: &[ImageKey]) -> PaintMessage {
        PaintMessage::UpdateImages(
            painter,
            planes
                .iter()
                .map(|plane| ImageUpdate::UpdateImage(*plane, descriptor(), external(), None))
                .collect(),
        )
    }

    fn kinds(messages: &[PaintMessage]) -> Vec<&'static str> {
        messages.iter().map(PaintMessage::kind).collect()
    }

    #[test]
    fn keeps_only_the_last_frame_of_each_key() {
        let p = painter(1);
        let (a, b) = (key(p, 10), key(p, 11));
        let mut messages = vec![frame(p, a), frame(p, b), frame(p, a), frame(p, a)];
        assert_eq!(Paint::drop_superseded_video_frame_updates(&mut messages), 2);
        assert_eq!(messages.len(), 2);
        // 남은 것은 각 키의 **마지막** 것이고, 서로의 상대 순서는 그대로다.
        assert!(matches!(
            &messages[0],
            PaintMessage::UpdateImages(_, updates)
                if matches!(updates.as_slice(), [ImageUpdate::UpdateImage(k, ..)] if *k == b)
        ));
        assert!(matches!(
            &messages[1],
            PaintMessage::UpdateImages(_, updates)
                if matches!(updates.as_slice(), [ImageUpdate::UpdateImage(k, ..)] if *k == a)
        ));
    }

    #[test]
    fn does_not_merge_across_painters() {
        // 팬아웃의 각 타일은 자기 몫을 받아야 한다 — 같은 키라도 페인터가 다르면 별개다.
        let (p1, p2) = (painter(1), painter(2));
        let image = key(p1, 10);
        let mut messages = vec![frame(p1, image), frame(p2, image)];
        assert_eq!(Paint::drop_superseded_video_frame_updates(&mut messages), 0);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn leaves_a_key_alone_once_the_batch_adds_or_deletes_it() {
        // 추가·삭제가 섞인 키에서는 순서가 곧 정확성이다. 하나라도 있으면 그 키는 통째로
        // 손대지 않는다 — 삭제 뒤에 낡은 갱신이 남거나 그 반대가 되면 안 된다.
        let p = painter(1);
        let image = key(p, 10);
        for extra in [
            ImageUpdate::DeleteImage(image),
            ImageUpdate::AddImage(image, descriptor(), external(), false),
        ] {
            let mut messages = vec![
                frame(p, image),
                frame(p, image),
                PaintMessage::UpdateImages(p, [extra].into()),
            ];
            assert_eq!(Paint::drop_superseded_video_frame_updates(&mut messages), 0);
            assert_eq!(messages.len(), 3);
        }
    }

    #[test]
    fn leaves_canvas_updates_alone() {
        // epoch 가 붙은 갱신은 스크립트와 짝이 맞아야 한다(캔버스). 버리면 ack 가 어긋난다.
        let p = painter(1);
        let image = key(p, 10);
        let paired = PaintMessage::UpdateImages(
            p,
            [ImageUpdate::UpdateImage(
                image,
                descriptor(),
                external(),
                Some(servo_base::Epoch(7)),
            )]
            .into(),
        );
        let mut messages = vec![frame(p, image), paired, frame(p, image)];
        assert_eq!(Paint::drop_superseded_video_frame_updates(&mut messages), 0);
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn leaves_other_messages_in_place() {
        let p = painter(1);
        let image = key(p, 10);
        let mut messages = vec![
            frame(p, image),
            PaintMessage::GenerateFrame(vec![p]),
            frame(p, image),
        ];
        assert_eq!(Paint::drop_superseded_video_frame_updates(&mut messages), 1);
        assert_eq!(kinds(&messages), vec!["GenerateFrame", "UpdateImages"]);
    }

    #[test]
    fn keeps_only_the_last_multi_plane_frame() {
        // YUV 세 면이 한 메시지로 오는, 실제로 오는 모양.
        let p = painter(1);
        let planes = [key(p, 20), key(p, 21), key(p, 22)];
        let other = [key(p, 30), key(p, 31), key(p, 32)];
        let mut messages = vec![
            planar_frame(p, &planes),
            planar_frame(p, &other),
            planar_frame(p, &planes),
        ];
        assert_eq!(Paint::drop_superseded_video_frame_updates(&mut messages), 1);
        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[1],
            PaintMessage::UpdateImages(_, updates)
                if matches!(updates.first(), Some(ImageUpdate::UpdateImage(k, ..)) if *k == planes[0])
        ));
    }

    #[test]
    fn a_protected_plane_saves_the_whole_frame() {
        // 한 면이라도 추가·삭제가 섞였으면 그 프레임은 통째로 남겨야 한다 — 면을 쪼개
        // 버리면 남은 면들과 짝이 맞지 않는다.
        let p = painter(1);
        let planes = [key(p, 20), key(p, 21), key(p, 22)];
        let mut messages = vec![
            planar_frame(p, &planes),
            planar_frame(p, &planes),
            PaintMessage::UpdateImages(p, [ImageUpdate::DeleteImage(planes[1])].into()),
        ];
        assert_eq!(Paint::drop_superseded_video_frame_updates(&mut messages), 0);
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn empty_input_is_a_no_op() {
        let mut messages: Vec<PaintMessage> = Vec::new();
        assert_eq!(Paint::drop_superseded_video_frame_updates(&mut messages), 0);
        assert!(messages.is_empty());
    }
}

/// 배리어가 마감을 놓쳤을 때 늦은 타일에 내리는 "직전 프레임 유지" 지시의 경계 조건.
#[cfg(test)]
mod wall_frame_keep_previous_tests {
    use std::time::Instant;

    use servo_base::id::PainterId;

    use super::{WallFrameCoordinator, WallFrameDelayInjection};

    fn painter(index: u32) -> PainterId {
        webrender_api::IdNamespace(index).into()
    }

    /// 한 번 표출한 타일을 만든다.
    ///
    /// ★질의 자체가 "표출했다" 가 된다★ -- `None` 을 돌려준 타일은 호출자가 곧 그리기
    /// 때문이다(`keep_previous_logical_frame` 주석). 그래서 이 모듈에서 "직전 프레임이 있는
    /// 타일" 의 설정은 한 번 묻는 것이다.
    fn has_presented(coordinator: &mut WallFrameCoordinator, painter_id: PainterId) {
        assert_eq!(
            coordinator.keep_previous_logical_frame(painter_id),
            None,
            "setup: the first query must draw, which is what records 'has presented'"
        );
    }

    /// 배리어 하나를 열고 마감을 놓치게 한다. `late` 는 준비 신호를 내지 않은 타일이다.
    fn missed_barrier(
        coordinator: &mut WallFrameCoordinator,
        logical_frame_id: u64,
        all: &[PainterId],
    ) {
        let now = Instant::now();
        coordinator.register_frame(logical_frame_id, Vec::new(), all.to_vec(), now);
        coordinator.miss_barrier(logical_frame_id, now);
    }

    /// ★한 번도 표출한 적 없는 타일에게 "직전 프레임을 유지하라" 고 말하면 안 된다.★
    ///
    /// 유지할 직전 프레임이 없기 때문이다. `render_all_tiles` 는 그 지시를 보고 그 타일의
    /// `paint_target` 과 `present` 를 둘 다 건너뛰는데(`main.rs`), DComp 경로에서 화면에
    /// 남아 있는 것은 마지막으로 **Commit** 된 내용뿐이라 아직 아무것도 커밋하지 않은
    /// 타일은 빈 서피스를 보인다 -- 검은 타일 한 장이다.
    ///
    /// 이것이 기동 때만 터지는 이유: 배리어 마감이 16ms(`WALL_FRAME_BARRIER_DEADLINE`)인데
    /// 첫 렌더는 셰이더 컴파일로 46~94ms 다(2026-10-20 실측, `log_ani_debug_05\08`).
    /// 첫 몇 프레임은 마감을 지킬 수가 없다.
    #[test]
    fn a_painter_that_never_presented_is_not_told_to_keep_a_frame_it_does_not_have() {
        let mut coordinator = WallFrameCoordinator::default();
        let (early, late) = (painter(1), painter(2));

        missed_barrier(&mut coordinator, 1, &[early, late]);

        assert_eq!(
            coordinator.keep_previous_logical_frame(late),
            None,
            "a tile with nothing committed must draw, not hold a frame it never had"
        );
    }

    /// 한 번 표출한 뒤로는 원래 정책대로 직전 프레임을 유지한다. 그때는 유지할 것이 있다.
    #[test]
    fn a_painter_that_has_presented_keeps_its_previous_frame() {
        let mut coordinator = WallFrameCoordinator::default();
        let (early, late) = (painter(1), painter(2));

        // 첫 패스: 늦었지만 보여 줄 것이 없으므로 그린다 -- 이 질의가 "표출했다" 가 된다.
        missed_barrier(&mut coordinator, 1, &[early, late]);
        assert_eq!(coordinator.keep_previous_logical_frame(late), None);

        // 두 번째 패스에서 또 늦으면, 이제는 유지할 직전 프레임이 있다.
        missed_barrier(&mut coordinator, 2, &[early, late]);
        assert_eq!(
            coordinator.keep_previous_logical_frame(late),
            Some(2),
            "once a frame is on screen, holding it is the right call"
        );
    }

    /// 지시를 받지 않은 타일은 전과 같이 그냥 그린다.
    #[test]
    fn a_painter_with_no_instruction_just_draws() {
        let mut coordinator = WallFrameCoordinator::default();
        assert_eq!(coordinator.keep_previous_logical_frame(painter(1)), None);
    }

    /// 무장한 타일은 그 논리 프레임 동안 직전 프레임을 유지한다.
    ///
    /// ★이 시험이 거는 사유는 캔버스 대기다 -- 역압이 아니다.★ HEAD 에서 무장까지 가는
    /// 거절은 `WaitingOnCanvasImages` 하나뿐이고
    /// (`wall_tick::decline_may_arm_keep_previous(RendererBehind)` 가 거짓이다),
    /// 역압 거절은 ***어떤 경로로도 무장되지 않는다***. 이름이 "역압" 이던 동안 이 세 줄이
    /// 역압 경로의 증거로 읽혔고, 2026-10-22 에 바로잡았다.
    ///
    /// ★설계 2-5 의 "배리어 참여" 를 이 시험이 보이지는 않는다★ -- 그 요구는 미구현이고,
    /// 무장이 실제로 바꾸는 것이 무엇인지는
    /// [`WallFrameCoordinator::arm_keep_previous`] 의 주석에 있다(거기 한 벌만 둔다).
    #[test]
    fn an_armed_tile_joins_the_frame_as_keep_previous() {
        let mut coordinator = WallFrameCoordinator::default();
        let busy = painter(2);
        has_presented(&mut coordinator, busy);

        coordinator.arm_keep_previous(7, &[busy]);

        assert_eq!(
            coordinator.keep_previous_logical_frame(busy),
            Some(7),
            "a declined tile must hold frame 7, not drop out of it"
        );
    }

    /// 캔버스 대기 거절에도 불변식 14 가 그대로 선다 -- 한 번도 표출한 적 없는 타일은 유지
    /// 지시를 받지 않는다.
    ///
    /// `a_painter_that_never_presented_is_not_told_to_keep_a_frame_it_does_not_have` 가
    /// 배리어 실패 경로에서 같은 것을 못박는다. `arm_keep_previous` 는 ★그 경로를 안 지나는
    /// 두 번째 무장 지점★ 이므로 여기서 따로 못박는다 -- 면제를 무장 자리에 복사하지 않고
    /// `keep_previous_logical_frame` 한 곳에만 두기 때문에, 새 무장 지점이 그 한 곳을
    /// 지나는지는 시험으로만 확인된다.
    #[test]
    fn a_canvas_waiting_tile_that_never_presented_is_not_held() {
        let mut coordinator = WallFrameCoordinator::default();
        let never_presented = painter(3);

        coordinator.arm_keep_previous(7, &[never_presented]);

        assert_eq!(
            coordinator.keep_previous_logical_frame(never_presented),
            None,
            "a tile with nothing committed must draw, not hold a frame it never had"
        );
    }

    /// 유지 지시는 ★지명된 그 논리 프레임에만★ 유효하다(불변식 14).
    ///
    /// 거절은 틱마다 다시 판정되므로, 한 번의 거절이 다음 주기까지 타일을 붙잡으면 그것이
    /// `expire_keep_previous_before` 주석이 말하는 일방통행 문이 된다. 다음 벽 프레임이
    /// 그 지시를 만료시키는지를 새 무장 경로에서 확인한다.
    #[test]
    fn a_canvas_wait_hold_expires_with_the_next_wall_frame() {
        let mut coordinator = WallFrameCoordinator::default();
        let busy = painter(2);
        has_presented(&mut coordinator, busy);

        coordinator.arm_keep_previous(7, &[busy]);
        // 만료 ***전***에 한 번 본다. 질의는 비주입 지시를 소비해도 지우지 않으므로 이
        // 단언이 성립하고, 이것이 있어야 아래 `None` 이 "만료됐다" 를 뜻한다 -- 없으면
        // 애초에 무장이 안 된 구현도 이 시험을 통과한다.
        assert_eq!(
            coordinator.keep_previous_logical_frame(busy),
            Some(7),
            "the hold must exist before the next wall frame can expire it"
        );

        coordinator.expire_keep_previous_before(8, &[busy]);

        assert_eq!(
            coordinator.keep_previous_logical_frame(busy),
            None,
            "a decline holds one logical frame, not every frame after it"
        );
    }

    /// ★생산을 쉬는 틱은 유지 지시를 걷는다 -- 그러지 않으면 스크린샷이 영영 안 찍힌다.★
    ///
    /// 무장한 타일은 셸이 `paint_target` 과 `present` 를 **둘 다** 건너뛰는데
    /// (`main.rs`), 스크린샷을 실제로 찍는 자리는 ***렌더 패스의 끝***이다
    /// (`Painter::render` -> `ScreenshotTaker::maybe_take_screenshots`, 호출자 1 곳).
    /// 그리고 지시를 걷는 다른 자리(`expire_keep_previous_before`)는 ***다음 생산***에
    /// 있다. 그래서 요청을 쥔 타일이 무장한 채 생산이 멈추면 ★정지에서 벗어나는 데 생산이
    /// 필요한 상태★ 가 되고, 그것이 정확히 `tick_should_produce` 가 피하려던 교착이다.
    ///
    /// 이 시험이 지키는 것은 기능이 아니라 그 교착의 부재다. 지우거나 무력화하기 전에
    /// `ScreenshotRequestPhase::suspends_production` 의 주석을 읽을 것.
    #[test]
    fn a_suspended_tick_releases_the_keep_previous_hold() {
        let mut coordinator = WallFrameCoordinator::default();
        let busy = painter(2);
        has_presented(&mut coordinator, busy);

        coordinator.arm_keep_previous(7, &[busy]);
        // 걷기 ***전***에 한 번 본다 -- 없으면 애초에 무장이 안 된 구현도 통과한다.
        assert_eq!(
            coordinator.keep_previous_logical_frame(busy),
            Some(7),
            "the hold must exist before a suspended tick can release it"
        );

        let released = coordinator.release_keep_previous_while_production_is_suspended(&[busy]);

        assert_eq!(
            released, 1,
            "the suspended tick must report the hold it released (실제 {released})"
        );
        let held = coordinator.keep_previous_logical_frame(busy);
        assert_eq!(
            held, None,
            "a tile held out of the render pass cannot take the screenshot the tick is \
             waiting for (실제 {held:?})"
        );
    }

    /// ★실패 주입이 세워 둔 관찰용 지시는 살아남는다.★
    ///
    /// 그 주입은 **스킵을 관찰하는 것이 목적**이라, 걷어 버리면 재현하려던 상태를 못 보게
    /// 된다 -- `expire_keep_previous_before` 가 같은 예외를 두는 이유와 같다. 걷는 자리가
    /// 둘이 된 지금, 새 자리가 그 예외를 따르는지는 시험으로만 확인된다.
    #[test]
    fn a_suspended_tick_leaves_the_injected_skip_observation_alone() {
        let mut coordinator = WallFrameCoordinator::default();
        let injected = painter(4);
        has_presented(&mut coordinator, injected);

        // 주입 경로의 무장은 `miss_barrier` 가 `injected_delayed_painter_id` 로 세운다.
        // 그 값을 넣어 주는 것은 `register_frame` 의 `delay_injection` 이고, 배리어는
        // 타일이 둘 이상이어야 열린다.
        let other = painter(5);
        coordinator.delay_injection = Some(WallFrameDelayInjection {
            target_index: 0,
            after_logical_frame_id: 0,
            remaining_frames: 1,
        });
        coordinator.register_frame(9, Vec::new(), vec![injected, other], Instant::now());
        coordinator.miss_barrier(9, Instant::now());

        let released = coordinator.release_keep_previous_while_production_is_suspended(&[injected]);

        assert_eq!(
            released, 0,
            "the injected skip observation is not ours to release (실제 {released})"
        );
        let held = coordinator.keep_previous_logical_frame(injected);
        assert_eq!(
            held,
            Some(9),
            "consuming it here would defeat the very test that arms it (실제 {held:?})"
        );
    }
}

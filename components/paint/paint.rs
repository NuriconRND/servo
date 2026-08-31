/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::env;
use std::fs::create_dir_all;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
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
use crate::painter::{FrameReadyDiagnostic, Painter};
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
    painters: Vec<Rc<RefCell<Painter>>>,

    /// The set of [`Painter`] targets that should render each logical [`WebViewId`].
    ///
    /// Today this normally contains a single target whose [`PainterId`] is embedded in the
    /// [`WebViewId`]. Wall rendering will extend this table so one logical `WebView` can be
    /// rendered into multiple tile `RenderingContext`s.
    webview_painter_targets: RefCell<HashMap<WebViewId, Vec<PainterId>>>,

    /// Logical frame id used to correlate one scene frame across multiple paint targets.
    next_logical_frame_id: Cell<u64>,

    /// Software barrier diagnostics for wall frames that fan out to multiple paint targets.
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

#[derive(Default)]
struct WallFrameCoordinator {
    barriers: HashMap<u64, WallFrameBarrier>,
    active_webview_frames: HashMap<WebViewId, u64>,
    keep_previous_painters: HashMap<PainterId, KeepPreviousFrame>,
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

enum WallFramePacingBlockReason {
    Active(WebViewId),
    Pending(usize),
    TooSoon(WebViewId, f64),
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
#[derive(Default)]
struct WallFramePacingBlockTally {
    active: Cell<u64>,
    pending: Cell<u64>,
    too_soon: Cell<u64>,
}

impl WallFramePacingBlockTally {
    fn record(&self, reason: &WallFramePacingBlockReason) {
        let counter = match reason {
            WallFramePacingBlockReason::Active(_) => &self.active,
            WallFramePacingBlockReason::Pending(_) => &self.pending,
            WallFramePacingBlockReason::TooSoon(_, _) => &self.too_soon,
        };
        counter.set(counter.get() + 1);
    }
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
            // would otherwise warn once per wall frame.
            if total <= 3 || total % WALL_FRAME_PACING_INFO_INTERVAL == 0 {
                warn!(
                    "Wall frame keep-previous expired: painter={:?} kept_for_logical_frame_id={} \
                     superseded_by={} total_expired={}. The painter missed a barrier and would \
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

    fn keep_previous_logical_frame(&mut self, painter_id: PainterId) -> Option<u64> {
        let keep_previous_frame = self.keep_previous_painters.get(&painter_id)?;
        let logical_frame_id = keep_previous_frame.logical_frame_id;
        let remove_after_skip_query = keep_previous_frame.remove_after_skip_query;
        if remove_after_skip_query {
            self.keep_previous_painters.remove(&painter_id);
            warn!(
                "Wall frame delay injection: consumed keep-previous skip observation for \
                 painter={:?} logical_frame_id={}",
                painter_id, logical_frame_id
            );
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
            wall_frame_coordinator: RefCell::new(WallFrameCoordinator::from_environment()),
            wall_frame_pacing_config: WallFramePacingConfig::from_prefs(),
            coalesced_wall_frame_requests: Default::default(),
            last_wall_frame_issue_at: Default::default(),
            wall_frame_pacing_next_wake_at: Default::default(),
            wall_frame_pacing_coalesced_count: Default::default(),
            wall_frame_pacing_released_count: Default::default(),
            wall_frame_pacing_block_tally: Default::default(),
        }))
    }

    pub fn register_rendering_context(
        &mut self,
        rendering_context: Rc<dyn RenderingContext>,
    ) -> PainterId {
        if let Some(painter_id) = self.painters.iter().find_map(|painter| {
            let painter = painter.borrow();
            if Rc::ptr_eq(&painter.rendering_context, &rendering_context) {
                Some(painter.painter_id)
            } else {
                None
            }
        }) {
            return painter_id;
        }

        let painter = Painter::new(rendering_context.clone(), self);
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
        self.painters.push(Rc::new(RefCell::new(painter)));
        painter_id
    }

    fn remove_painter(&mut self, painter_id: PainterId) {
        self.painters
            .retain(|painter| painter.borrow().painter_id != painter_id);
        self.painter_surfman_details_map.remove(painter_id);
    }

    pub(crate) fn maybe_painter<'a>(&'a self, painter_id: PainterId) -> Option<Ref<'a, Painter>> {
        self.painters
            .iter()
            .map(|painter| painter.borrow())
            .find(|painter| painter.painter_id == painter_id)
    }

    pub(crate) fn painter<'a>(&'a self, painter_id: PainterId) -> Ref<'a, Painter> {
        self.maybe_painter(painter_id)
            .expect("painter_id not found")
    }

    pub(crate) fn maybe_painter_mut<'a>(
        &'a self,
        painter_id: PainterId,
    ) -> Option<RefMut<'a, Painter>> {
        self.painters
            .iter()
            .map(|painter| painter.borrow_mut())
            .find(|painter| painter.painter_id == painter_id)
    }

    pub(crate) fn painter_mut<'a>(&'a self, painter_id: PainterId) -> RefMut<'a, Painter> {
        self.maybe_painter_mut(painter_id)
            .expect("painter_id not found")
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
            for painter_id in target_painter_ids {
                let Some(painter) = self.maybe_painter(*painter_id) else {
                    missing_targets.push(format!("{painter_id:?}:missing-painter"));
                    continue;
                };

                let Some(signature) = painter.wall_scroll_offsets_signature(*webview_id) else {
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
        mut callback: impl FnMut(&mut Painter),
    ) {
        for painter_id in self.painter_targets_for_webview(webview_id) {
            if let Some(mut painter) = self.maybe_painter_mut(painter_id) {
                callback(&mut painter);
            }
        }
    }

    fn for_each_source_painter_target_mut(
        &self,
        source_painter_id: PainterId,
        mut callback: impl FnMut(&mut Painter),
    ) {
        for painter_id in self.target_painter_ids_for_source_painter(source_painter_id) {
            if let Some(mut painter) = self.maybe_painter_mut(painter_id) {
                callback(&mut painter);
            }
        }
    }

    fn primary_painter_id_for_webview(&self, webview_id: WebViewId) -> PainterId {
        self.painter_targets_for_webview(webview_id)
            .first()
            .copied()
            .unwrap_or_else(|| webview_id.into())
    }

    fn maybe_primary_painter<'a>(&'a self, webview_id: WebViewId) -> Option<Ref<'a, Painter>> {
        self.maybe_painter(self.primary_painter_id_for_webview(webview_id))
    }

    fn maybe_primary_painter_mut<'a>(
        &'a self,
        webview_id: WebViewId,
    ) -> Option<RefMut<'a, Painter>> {
        self.maybe_painter_mut(self.primary_painter_id_for_webview(webview_id))
    }

    fn primary_painter<'a>(&'a self, webview_id: WebViewId) -> Ref<'a, Painter> {
        self.painter(self.primary_painter_id_for_webview(webview_id))
    }

    fn primary_painter_mut<'a>(&'a self, webview_id: WebViewId) -> RefMut<'a, Painter> {
        self.painter_mut(self.primary_painter_id_for_webview(webview_id))
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

    fn max_pending_frames_for_targets(&self, target_painter_ids: &[PainterId]) -> usize {
        target_painter_ids
            .iter()
            .filter_map(|painter_id| {
                self.maybe_painter(*painter_id)
                    .map(|painter| painter.pending_frames())
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
                    return Some(WallFramePacingBlockReason::Active(webview_id));
                }
            }
        }

        let pending_max = self.max_pending_frames_for_targets(&request.target_painter_ids);
        if pending_max >= self.wall_frame_pacing_config.max_pending {
            return Some(WallFramePacingBlockReason::Pending(pending_max));
        }

        let now = Instant::now();
        let last_issue_at = self.last_wall_frame_issue_at.borrow();
        for webview_id in request.wall_webview_ids() {
            let Some(last_issue_at) = last_issue_at.get(&webview_id) else {
                continue;
            };
            let elapsed = now.saturating_duration_since(*last_issue_at);
            if elapsed < self.wall_frame_pacing_config.min_interval {
                return Some(WallFramePacingBlockReason::TooSoon(
                    webview_id,
                    elapsed.as_secs_f64() * 1000.0,
                ));
            }
        }

        None
    }

    fn coalesce_wall_frame_request(
        &self,
        request: WallFrameRequest,
        block_reason: WallFramePacingBlockReason,
    ) {
        let Some(webview_id) = request.pacing_key() else {
            let _ = self.issue_wall_frame_request(request);
            return;
        };

        self.wall_frame_pacing_block_tally.record(&block_reason);

        let pending_max = match block_reason {
            WallFramePacingBlockReason::Active(_) => {
                self.max_pending_frames_for_targets(&request.target_painter_ids)
            },
            WallFramePacingBlockReason::Pending(pending_max) => pending_max,
            WallFramePacingBlockReason::TooSoon(_, _) => {
                self.max_pending_frames_for_targets(&request.target_painter_ids)
            },
        };
        let reason = match block_reason {
            WallFramePacingBlockReason::Active(active_webview_id) => {
                format!("active_webview={active_webview_id:?}")
            },
            WallFramePacingBlockReason::Pending(pending_max) => {
                format!("pending_max={pending_max}")
            },
            WallFramePacingBlockReason::TooSoon(webview_id, elapsed_ms) => {
                let remaining = self
                    .wall_frame_pacing_config
                    .min_interval
                    .saturating_sub(Duration::from_secs_f64(elapsed_ms / 1000.0));
                self.schedule_wall_frame_pacing_wake(remaining);
                format!("min_interval_webview={webview_id:?} elapsed_ms={elapsed_ms:.3}")
            },
        };

        let total_coalesced = self.wall_frame_pacing_coalesced_count.get() + 1;
        self.wall_frame_pacing_coalesced_count.set(total_coalesced);
        let coalesced_for_webview = {
            let mut coalesced_requests = self.coalesced_wall_frame_requests.borrow_mut();
            let coalesced_request = coalesced_requests
                .entry(webview_id)
                .and_modify(|coalesced_request| {
                    coalesced_request.request = request.clone();
                    coalesced_request.coalesced_count += 1;
                    coalesced_request.max_pending_seen =
                        coalesced_request.max_pending_seen.max(pending_max);
                })
                .or_insert_with(|| CoalescedWallFrameRequest {
                    request: request.clone(),
                    coalesced_count: 1,
                    max_pending_seen: pending_max,
                });
            coalesced_request.coalesced_count
        };

        debug!(
            "Wall frame pacing coalesced: webview={:?} target_painters={:?} \
             pending_max={} coalesced_for_webview={} total_coalesced={} reason={} \
             policy=latest-first",
            webview_id,
            request.target_painter_ids,
            pending_max,
            coalesced_for_webview,
            total_coalesced,
            reason,
        );
        self.log_wall_frame_pacing_summary(
            "coalesced",
            webview_id,
            &request,
            pending_max,
            coalesced_for_webview,
            None,
        );
    }

    fn schedule_wall_frame_pacing_wake(&self, delay: Duration) {
        let wake_at = Instant::now() + delay;
        if self
            .wall_frame_pacing_next_wake_at
            .get()
            .is_some_and(|scheduled_wake_at| scheduled_wake_at <= wake_at)
        {
            return;
        }

        self.wall_frame_pacing_next_wake_at.set(Some(wake_at));
        let event_loop_waker = self.event_loop_waker.clone_box();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            event_loop_waker.wake();
        });
    }

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

        let mut generated_painter_ids = Vec::new();
        for painter_id in &request.target_painter_ids {
            if let Some(mut painter) = self.maybe_painter_mut(*painter_id) {
                if painter.generate_frame_for_script(logical_frame_id, wall_frame_requested_at) {
                    generated_painter_ids.push(*painter_id);
                }
            }
        }

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
            debug!(
                "Wall logical frame {logical_frame_id} barrier skipped: generated_targets={:?} \
                 requested_targets={:?}",
                generated_painter_ids, request.target_painter_ids
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
            let logical_frame_id = self.issue_wall_frame_request(coalesced_request.request.clone());
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
        self.painters[0].borrow().painter_id
    }

    pub fn rendering_context_size(&self, painter_id: PainterId) -> Size2D<u32, DevicePixel> {
        self.painter(painter_id).rendering_context.size2d()
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
        self.painters
            .iter()
            .flat_map(|painter| painter.borrow().webviews_needing_repaint())
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
            if let Some(painter) = self.maybe_painter(painter_id) {
                painter.handle_new_webrender_frame_ready(repaint_needed);
            }
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
            PaintMessage::ChangeRunningAnimationsState(
                webview_id,
                pipeline_id,
                animation_state,
            ) => {
                self.for_each_webview_painter_mut(webview_id, |painter| {
                    painter.change_running_animations_state(
                        webview_id,
                        pipeline_id,
                        animation_state,
                    );
                });
            },
            PaintMessage::SetFrameTreeForWebView(webview_id, frame_tree) => {
                self.for_each_webview_painter_mut(webview_id, |painter| {
                    painter.set_frame_tree_for_webview(&frame_tree);
                });
            },
            PaintMessage::SetThrottled(webview_id, pipeline_id, throttled) => {
                self.for_each_webview_painter_mut(webview_id, |painter| {
                    painter.set_throttled(webview_id, pipeline_id, throttled);
                });
            },
            PaintMessage::PipelineExited(webview_id, pipeline_id, pipeline_exit_source) => {
                self.for_each_webview_painter_mut(webview_id, |painter| {
                    painter.notify_pipeline_exited(webview_id, pipeline_id, pipeline_exit_source);
                });
            },
            PaintMessage::NewWebRenderFrameReady(..) => {
                unreachable!("New WebRender frames should be handled in the caller.");
            },
            PaintMessage::SendInitialTransaction(webview_id, pipeline_id) => {
                self.for_each_webview_painter_mut(webview_id, |painter| {
                    painter.send_initial_pipeline_transaction(webview_id, pipeline_id);
                });
            },
            PaintMessage::ScrollNodeByDelta(
                webview_id,
                pipeline_id,
                offset,
                external_scroll_id,
            ) => {
                self.for_each_webview_painter_mut(webview_id, |painter| {
                    painter.scroll_node_by_delta(
                        webview_id,
                        pipeline_id,
                        offset,
                        external_scroll_id,
                    );
                });
            },
            PaintMessage::ScrollViewportByDelta(webview_id, delta) => {
                self.for_each_webview_painter_mut(webview_id, |painter| {
                    painter.scroll_viewport_by_delta(webview_id, delta);
                });
            },
            PaintMessage::UpdateEpoch {
                webview_id,
                pipeline_id,
                epoch,
            } => {
                self.for_each_webview_painter_mut(webview_id, |painter| {
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
            PaintMessage::GenerateFrame(painter_ids) => {
                let request = self.wall_frame_request_from_source_painters(painter_ids);
                if let Some(block_reason) = self.wall_frame_request_pacing_block_reason(&request) {
                    self.coalesce_wall_frame_request(request, block_reason);
                } else {
                    let _ = self.issue_wall_frame_request(request);
                }
            },
            PaintMessage::GetWebViewPainterTargets(webview_id, sender) => {
                let _ = sender.send(self.painter_targets_for_webview(webview_id));
            },
            PaintMessage::GenerateImageKey(webview_id, result_sender) => {
                self.handle_generate_image_key(webview_id, result_sender);
            },
            PaintMessage::GenerateImageKeysForPipeline(webview_id, pipeline_id) => {
                self.handle_generate_image_keys_for_pipeline(webview_id, pipeline_id);
            },
            PaintMessage::UpdateImages(painter_id, updates) => {
                let target_painter_ids = self.target_painter_ids_for_source_painter(painter_id);
                if target_painter_ids.len() > 1 {
                    let requested_gpus: Vec<_> = target_painter_ids
                        .iter()
                        .map(|target_painter_id| {
                            self.maybe_painter(*target_painter_id)
                                .and_then(|painter| painter.rendering_context.requested_gpu_index())
                        })
                        .collect();
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
                    let fanout_id =
                        WALL_MEDIA_IMAGE_FANOUT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                    debug!(
                        "Wall media image fanout: source_painter={:?} target_painters={:?} \
                         requested_gpus={:?} updates_total={} adds={} updates={} deletes={} \
                         animation_updates={} fanout_id={}",
                        painter_id,
                        target_painter_ids,
                        requested_gpus,
                        updates.len(),
                        add_count,
                        update_count,
                        delete_count,
                        animation_update_count,
                        fanout_id,
                    );
                    if add_count > 0
                        || delete_count > 0
                        || fanout_id <= 3
                        || fanout_id % WALL_MEDIA_IMAGE_FANOUT_INFO_INTERVAL == 0
                    {
                        info!(
                            "Wall media image fanout summary: fanout_id={} source_painter={:?} \
                             target_painters={:?} requested_gpus={:?} updates_total={} \
                             adds={} updates={} deletes={} animation_updates={}",
                            fanout_id,
                            painter_id,
                            target_painter_ids,
                            requested_gpus,
                            updates.len(),
                            add_count,
                            update_count,
                            delete_count,
                            animation_update_count,
                        );
                    }
                }
                for target_painter_id in target_painter_ids {
                    if let Some(mut painter) = self.maybe_painter_mut(target_painter_id) {
                        painter.update_images(updates.clone());
                    }
                }
            },
            PaintMessage::DelayNewFrameForCanvas(
                webview_id,
                pipeline_id,
                canvas_epoch,
                image_keys,
            ) => {
                self.for_each_webview_painter_mut(webview_id, |painter| {
                    painter.delay_new_frames_for_canvas(
                        pipeline_id,
                        canvas_epoch,
                        image_keys.clone(),
                    );
                });
            },
            PaintMessage::AddFont(painter_id, font_key, data, index) => {
                debug_assert!(painter_id == font_key.into());

                self.for_each_source_painter_target_mut(painter_id, |painter| {
                    painter.add_font(font_key, data.clone(), index);
                });
            },
            PaintMessage::AddSystemFont(painter_id, font_key, native_handle) => {
                debug_assert!(painter_id == font_key.into());

                self.for_each_source_painter_target_mut(painter_id, |painter| {
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

                self.for_each_source_painter_target_mut(painter_id, |painter| {
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
                self.for_each_source_painter_target_mut(painter_id, |painter| {
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
                self.for_each_webview_painter_mut(webview_id, |painter| {
                    painter.set_viewport_description(webview_id, viewport_description.clone());
                });
            },
            PaintMessage::ScreenshotReadinessReponse(webview_id, pipelines_and_epochs) => {
                if let Some(painter) = self.maybe_primary_painter(webview_id) {
                    painter.handle_screenshot_readiness_reply(webview_id, pipelines_and_epochs);
                }
            },
            PaintMessage::SendLCPCandidate(lcp_candidate, webview_id, pipeline_id, epoch) => {
                self.for_each_webview_painter_mut(webview_id, |painter| {
                    painter.append_lcp_candidate(
                        lcp_candidate.clone(),
                        webview_id,
                        pipeline_id,
                        epoch,
                    );
                });
            },
            PaintMessage::EnableLCPCalculation(webview_id) => {
                self.for_each_webview_painter_mut(webview_id, |painter| {
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
            let Some(mut painter) = self.maybe_painter_mut(painter_id) else {
                continue;
            };
            painter.remove_webview(webview_id);
            let should_remove_painter = painter.is_empty();
            drop(painter);

            if should_remove_painter {
                self.remove_painter(painter_id);
            }
        }
    }

    fn collect_memory_report(&self, sender: profile_traits::mem::ReportsChan) {
        let mut memory_report = MemoryReport::default();
        for painter in &self.painters {
            memory_report += painter.borrow().report_memory();
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
                .painters
                .iter()
                .map(|painter| painter.borrow().scroll_trees_memory_usage(ops))
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
            if let Some(mut painter) = self.maybe_painter_mut(painter_id) {
                painter.handle_new_display_list(
                    webview_id,
                    display_list_descriptor.clone(),
                    display_list_info.clone(),
                    display_list_data.clone(),
                );
            }
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
                if let Some(mut painter) = self.maybe_primary_painter_mut(webview_id) {
                    painter.notify_pipeline_exited(webview_id, pipeline_id, pipeline_exit_source);
                }
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
        let painter_id: PainterId = webview_id.into();
        self.register_webview_painter_target(webview_id, painter_id);
        self.painter_mut(painter_id)
            .add_webview(webview, viewport_details, viewport_origin);
    }

    pub fn add_webview_paint_target(
        &mut self,
        webview: Box<dyn WebViewTrait>,
        rendering_context: Rc<dyn RenderingContext>,
        viewport_details: ViewportDetails,
        viewport_origin: DeviceVector2D,
    ) -> PainterId {
        let webview_id = webview.id();
        let painter_id = self.register_rendering_context(rendering_context);
        self.register_webview_painter_target(webview_id, painter_id);
        self.painter_mut(painter_id)
            .add_webview(webview, viewport_details, viewport_origin);
        painter_id
    }

    pub fn show_webview(&self, webview_id: WebViewId) -> Result<(), UnknownWebView> {
        let mut result = Ok(());
        let mut saw_painter = false;
        self.for_each_webview_painter_mut(webview_id, |painter| {
            saw_painter = true;
            if result.is_ok() {
                result = painter.set_webview_hidden(webview_id, false);
            }
        });
        if saw_painter {
            result
        } else {
            Err(UnknownWebView(webview_id))
        }
    }

    pub fn hide_webview(&self, webview_id: WebViewId) -> Result<(), UnknownWebView> {
        let mut result = Ok(());
        let mut saw_painter = false;
        self.for_each_webview_painter_mut(webview_id, |painter| {
            saw_painter = true;
            if result.is_ok() {
                result = painter.set_webview_hidden(webview_id, true);
            }
        });
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
        self.primary_painter_mut(webview_id)
            .set_hidpi_scale_factor(webview_id, new_scale_factor);
    }

    pub fn set_viewport_details(&self, webview_id: WebViewId, viewport_details: ViewportDetails) {
        if self.shutdown_state() != ShutdownState::NotShuttingDown {
            return;
        }
        self.for_each_webview_painter_mut(webview_id, |painter| {
            painter.set_viewport_details(webview_id, viewport_details);
        });
    }

    pub fn resize_rendering_context(&self, webview_id: WebViewId, new_size: PhysicalSize<u32>) {
        if self.shutdown_state() != ShutdownState::NotShuttingDown {
            return;
        }
        self.primary_painter_mut(webview_id)
            .resize_rendering_context(new_size);
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

        let mut painter = self.painter_mut(painter_id);
        painter.resize_rendering_context(new_size);
        painter.set_viewport_details_and_origin(webview_id, viewport_details, viewport_origin);
    }

    pub fn set_page_zoom(&self, webview_id: WebViewId, new_zoom: f32) {
        if self.shutdown_state() != ShutdownState::NotShuttingDown {
            return;
        }
        self.for_each_webview_painter_mut(webview_id, |painter| {
            painter.set_page_zoom(webview_id, new_zoom);
        });
    }

    pub fn page_zoom(&self, webview_id: WebViewId) -> f32 {
        self.primary_painter(webview_id).page_zoom(webview_id)
    }

    /// Render the WebRender scene to the active `RenderingContext`.
    pub fn render(&self, webview_id: WebViewId) {
        self.primary_painter_mut(webview_id)
            .render(&self.time_profiler_chan);
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

        if let Some(mut painter) = self.maybe_painter_mut(painter_id) {
            painter.render(&self.time_profiler_chan);
        }
    }

    /// Get the message receiver for this [`Paint`].
    pub fn receiver(&self) -> &RoutedReceiver<PaintMessage> {
        &self.paint_receiver
    }

    #[servo_tracing::instrument(skip_all)]
    pub fn handle_messages(&self, mut messages: Vec<PaintMessage>) {
        // Pull out the `NewWebRenderFrameReady` messages from the list of messages and handle them
        // at the end of this function. This prevents overdraw when more than a single message of
        // this type of received. In addition, if any of these frames need a repaint, that reflected
        // when calling `handle_new_webrender_frame_ready`.
        let mut frame_ready_for_painter = HashMap::new();
        let mut frame_ready_diagnostics = Vec::new();
        messages.retain(|message| match message {
            PaintMessage::NewWebRenderFrameReady(painter_id, _document_id, need_repaint) => {
                if let Some(painter) = self.maybe_painter(*painter_id) {
                    if let Some(diagnostic) = painter.note_webrender_frame_ready(*need_repaint) {
                        frame_ready_diagnostics.push(diagnostic);
                    } else {
                        Self::record_painter_ready_for_repaint(
                            &mut frame_ready_for_painter,
                            *painter_id,
                            *need_repaint,
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
    }

    #[servo_tracing::instrument(skip_all)]
    pub fn perform_updates(&self) -> bool {
        if self.shutdown_state() == ShutdownState::FinishedShuttingDown {
            return false;
        }

        // Run the WebXR main thread
        #[cfg(feature = "webxr")]
        self.webxr_main_thread.borrow_mut().run_one_frame();

        for painter in &self.painters {
            painter.borrow_mut().perform_updates();
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

        self.shutdown_state() != ShutdownState::FinishedShuttingDown
    }

    pub fn toggle_webrender_debug(&self, option: WebRenderDebugOption) {
        for painter in &self.painters {
            painter.borrow_mut().toggle_webrender_debug(option);
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
        self.primary_painter(webview_id)
            .webrender_api
            .save_capture(capture_path, CaptureBits::all());
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
        self.painter_mut(painter_id)
            .notify_input_event(webview_id, event)
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
                self.maybe_painter(painter_id).is_some_and(|painter| {
                    painter.rendered_tile_contains_input_point(webview_id, point)
                })
            })
    }

    pub fn notify_scroll_event(&self, webview_id: WebViewId, scroll: Scroll, point: WebViewPoint) {
        if self.shutdown_state() != ShutdownState::NotShuttingDown {
            return;
        }
        self.primary_painter_mut(webview_id)
            .notify_scroll_event(webview_id, scroll, point);
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
        self.primary_painter_mut(webview_id).adjust_pinch_zoom(
            webview_id,
            pinch_zoom_delta,
            center,
        );
    }

    pub fn pinch_zoom(&self, webview_id: WebViewId) -> f32 {
        self.primary_painter(webview_id).pinch_zoom(webview_id)
    }

    pub fn device_pixels_per_page_pixel(
        &self,
        webview_id: WebViewId,
    ) -> Scale<f32, CSSPixel, DevicePixel> {
        self.primary_painter_mut(webview_id)
            .device_pixels_per_page_pixel(webview_id)
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
        self.primary_painter(webview_id)
            .request_screenshot(webview_id, rect, callback);
    }

    pub fn notify_input_event_handled(
        &self,
        webview_id: WebViewId,
        input_event_id: InputEventId,
        result: InputEventResult,
    ) {
        if let Some(mut painter) = self.maybe_primary_painter_mut(webview_id) {
            painter.notify_input_event_handled(webview_id, input_event_id, result);
        }
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
        let image_key = self.maybe_painter(painter_id).map_or_else(
            || ImageKey::new(painter_id.into(), 0),
            |painter| painter.webrender_api.generate_image_key(),
        );
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
        let painter = self.maybe_painter(painter_id);
        let image_keys = (0..pref!(image_key_batch_size))
            .map(|_| {
                painter.as_ref().map_or_else(
                    || ImageKey::new(painter_id.into(), 0),
                    |painter| painter.webrender_api.generate_image_key(),
                )
            })
            .collect();

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
        let painter = self.maybe_painter(painter_id);
        let font_keys = (0..number_of_font_keys)
            .map(|_| {
                painter.as_ref().map_or_else(
                    || FontKey::new(painter_id.into(), 0),
                    |painter| painter.webrender_api.generate_font_key(),
                )
            })
            .collect();
        let font_instance_keys = (0..number_of_font_instance_keys)
            .map(|_| {
                painter.as_ref().map_or_else(
                    || FontInstanceKey::new(painter_id.into(), 0),
                    |painter| painter.webrender_api.generate_font_instance_key(),
                )
            })
            .collect();

        let _ = result_sender.send((font_keys, font_instance_keys));
    }
}

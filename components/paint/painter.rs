/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, LazyCell, RefCell};
use std::collections::{VecDeque, hash_map::Entry};
use std::rc::Rc;
use std::sync::{Arc, LazyLock};
use std::panic::Location;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use dpi::PhysicalSize;
use embedder_traits::{
    InputEvent, InputEventAndId, InputEventId, InputEventResult, PaintHitTestResult,
    ScreenshotCaptureError, Scroll, ViewportDetails, WebViewPoint, WebViewRect,
};
use euclid::{Point2D, Rect, Scale, Size2D};
use gleam::gl::RENDERER;
use image::RgbaImage;
use log::{debug, error, info, warn};
use media::WindowGLContext;
use paint_api::display_list::{PaintDisplayListInfo, ScrollType};
use paint_api::largest_contentful_paint_candidate::LCPCandidate;
use paint_api::rendering_context::RenderingContext;
use paint_api::viewport_description::ViewportDescription;
use paint_api::{
    ImageUpdate, PipelineExitSource, SendableFrameTree, SerializableDisplayListPayload,
    SerializableImageData, WebRenderExternalImageHandlers, WebRenderImageHandlerType, WebViewTrait,
};
use profile_traits::time::{ProfilerCategory, ProfilerChan};
use profile_traits::time_profile;
use rustc_hash::{FxHashMap, FxHashSet};
use servo_base::Epoch;
use servo_base::cross_process_instant::CrossProcessInstant;
use servo_base::generic_channel::GenericSharedMemory;
use servo_base::id::{PainterId, PipelineId, WebViewId};
use servo_config::debug_env;
use servo_config::{opts, pref};
use servo_constellation_traits::{EmbedderToConstellationMessage, PaintMetricEvent};
use servo_geometry::DeviceIndependentPixel;
use smallvec::SmallVec;
use style_traits::CSSPixel;
use webrender::{
    MemoryReport, ONE_TIME_USAGE_HINT, RenderApi, ShaderPrecacheFlags, Transaction, UploadMethod,
};
use webrender_api::units::{
    DevicePixel, DevicePoint, DeviceVector2D, LayoutPoint, LayoutRect, LayoutSize, LayoutTransform,
    LayoutVector2D, WorldPoint,
};
use webrender_api::{
    self, BuiltDisplayList, BuiltDisplayListDescriptor, ColorF, DirtyRect, DisplayListPayload,
    DocumentId, DynamicProperties, Epoch as WebRenderEpoch, ExternalScrollId, FontInstanceFlags,
    FontInstanceKey, FontInstanceOptions, FontKey, FontVariation, ImageData, ImageDescriptor,
    ImageKey,
    NativeFontHandle, PipelineId as WebRenderPipelineId, PropertyBinding, ReferenceFrameKind,
    RenderReasons, SampledScrollOffset, SpaceAndClipInfo, SpatialId, TransformStyle,
};
use wr_malloc_size_of::MallocSizeOfOps;

// A/B gate for the FPS-jitter investigation: when set, disable the per-video-arrival
// immediate re-composite in `update_images` (falls back to script rendering-opportunity
// pacing). Read once (cached inside `debug_env`). Default = enabled (current behavior).
// Values "1"/"true" disable it.
// Diagnostic: log the ACTUAL engine present cadence (frame-ready rate + worst inter-frame gap)
// once per second per painter. This is the ground-truth displayed cadence, independent of the
// page's requestAnimationFrame count and of external capture tools (Bandicam/PresentMon).
static FRAME_REASON_PROF: LazyLock<bool> = LazyLock::new(|| {
    debug_env::string(&debug_env::FRAME_REASON_PROF)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
});

static LOG_PRESENT_CADENCE: LazyLock<bool> = LazyLock::new(|| {
    debug_env::string(&debug_env::LOG_PRESENT_CADENCE)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
});

/// 캔버스 ack 교착 실패 주입. painter 마다 처음 N 번의 ack 전송 기회를 건너뛴다.
static WALL_CANVAS_ACK_SKIP: LazyLock<u64> = LazyLock::new(|| {
    debug_env::int(&debug_env::WALL_CANVAS_ACK_SKIP)
        .filter(|value| *value > 0)
        .map(|value| value as u64)
        .unwrap_or(0)
});

/// [`Painter::flush_owed_canvas_ack`] kill switch. 주입이 진짜로 교착을 만드는지 보이는
/// 대조군이자, 운영에서 복구를 끌 수 있는 스위치.
static WALL_DISABLE_CANVAS_ACK_RECOVERY: LazyLock<bool> = LazyLock::new(|| {
    debug_env::string(&debug_env::WALL_DISABLE_CANVAS_ACK_RECOVERY)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
});

// DComp Native 경로에서 런타임 리사이즈(사용자 드래그/최대화) 후 picture-cache를 재구축하기
// 전에 크기가 안정되어야 하는 연속 프레임 수. 드래그는 매 프레임 resize 이벤트를 쏟아내므로
// 마지막 크기 변경 이후 이만큼 프레임이 흐른 뒤에 단 한 번만 재구축을 발동한다(≈170ms@60fps).
// 타이머 없이 프레임 카운터만으로 디바운스한다.
#[cfg(windows)]
const DCOMP_RESIZE_DEBOUNCE_FRAMES: u32 = 10;

/// How long `display_composite_in_flight` may stay set before it is force-released.
///
/// ***This exists because losing one redraw used to kill a wall tile permanently.***
/// The flag is set when a display-paced composite is requested and cleared only by the
/// render pass that follows (or by a frame-ready that will not repaint). If that render
/// never arrives, nothing else clears it: `renderer_behind` stays true, so every later
/// request is declined, so no transaction is sent, so no frame-ready arrives, so no render
/// happens. The painter is dead with no way back.
///
/// Measured 2026-08-31 on the 4-GPU wall: two of four painters stopped rendering mid-run
/// and never resumed for the remaining 44 seconds, while the paint side kept fanning out to
/// all four. Their last line was a frame-ready with `need_repaint=true` and no render after
/// it. It is intermittent but frequent, which is what a lost-redraw race looks like.
///
/// 2 seconds is deliberately far above any legitimate request->ready->render cycle (worst
/// observed ~300ms: ~100ms to ready plus a ~200ms render). It also matches the media ring's
/// demand TTL: past that the tile has already lost its plane ring and is showing green, so
/// holding the gate shut protects nothing that is still intact.
const DISPLAY_COMPOSITE_IN_FLIGHT_TIMEOUT: Duration = Duration::from_secs(2);

// 킬 스위치(기본 = 활성). "1"/"true"이면 런타임 리사이즈 시 picture-cache 재구축을 끈다.
// A/B 검증(잔상 재현) 및 회귀 시 안전 밸브. 기본값에서는 재구축이 동작해 잔상을 제거한다.
// 이 마스터 스위치는 Task 12(정착 재구축)와 12b(드래그 중 가상 모드+시작 재구축)를 모두 끈다.
#[cfg(windows)]
static DCOMP_RESIZE_REBUILD_DISABLED: LazyLock<bool> = LazyLock::new(|| {
    debug_env::string(&debug_env::DCOMP_DISABLE_RESIZE_REBUILD)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
});

// task-12b 전용 킬 스위치(기본 = 활성). "1"/"true"이면 "드래그 중 가상 서피스 모드(A:
// 강등/승격·regen 억제)"만 끄고, Task 12의 정착 재구축은 그대로 유지한다. 드래그 시작
// 재구축(B)은 이 스위치와 무관하게 마스터 스위치에만 걸린다 — 결정론 수렴 방식에서 정착
// 재구축은 current(=alternate) != steady 전이에 의존하므로, 시작-발동까지 끄면 정착이
// desired==current no-op이 되어 Task 12마저 무력화되기 때문(리뷰 수정; picture.rs:2320).
// RED(12b off, Task 12 on) ↔ GREEN(둘 다 on) A/B를 같은 바이너리에서 재현하기 위한 스위치.
#[cfg(windows)]
static DCOMP_RESIZE_VIRTUAL_DISABLED: LazyLock<bool> = LazyLock::new(|| {
    debug_env::string(&debug_env::DCOMP_DISABLE_RESIZE_VIRTUAL)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
});

use crate::Paint;
use crate::largest_contentful_paint_calculator::LargestContentfulPaintCalculator;
use crate::paint::{RepaintReason, WebRenderDebugOption};
use crate::refresh_driver::{AnimationRefreshDriverObserver, BaseRefreshDriver};
use crate::render_notifier::RenderNotifier;
use crate::screenshot::ScreenshotTaker;
use crate::web_content_animation::WebContentAnimator;
use crate::webrender_external_images::WebGLExternalImages;
use crate::webview_renderer::{PinchZoomResult, ScrollResult, UnknownWebView, WebViewRenderer};

/// `gfx_wr_picture_tile_size` pref(구 env `SERVO_WR_PICTURE_TILE_SIZE`)를 해석한다.
///
/// 인정하는 값 셋: 빈 문자열 = 오버라이드 없음(`None`), `display` = `surface`(이 painter 창의
/// 실제 크기), 그 외는 `WxH` 리터럴(예 `1920x1080`).
///
/// `display` 가 있는 이유는 이 노브의 주 용도가 **타일 크기를 그 타일 창 해상도에 맞추는
/// 것**이기 때문이다(그러면 슬라이스당 타일이 1 장이 된다). 리터럴만 있으면 타일 해상도가
/// 섞인 월에서 한 값으로 표현이 안 되고, 레이아웃을 바꿀 때마다 손으로 다시 적어야 한다.
///
/// 기동 시 정상상태 오버라이드 적용부와 DComp 리사이즈 재구축의 steady/alternate 계산부가
/// 이 해석 결과를 공유한다 — 해석은 여기 한 번뿐이다.
fn resolve_wr_tile_size(
    value: &str,
    surface: webrender_api::units::DeviceIntSize,
) -> Option<webrender_api::units::DeviceIntSize> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("display") {
        // 창 크기를 아직 모르는(0 인) 시점이면 오버라이드하지 않는다 — 0 짜리 타일 크기는
        // WR 에 그대로 전달되면 안 된다(WR 은 이 값을 검사하지 않는다, 아래 주석 참고).
        if surface.width <= 0 || surface.height <= 0 {
            log::warn!(
                "[wr-tile-size] gfx_wr_picture_tile_size=display 인데 창 크기가 {}x{} 다 - \
                 오버라이드하지 않는다",
                surface.width,
                surface.height
            );
            return None;
        }
        return Some(surface);
    }
    let lower = trimmed.to_ascii_lowercase();
    let mut split = lower.split('x');
    match (
        split.next().and_then(|v| v.trim().parse::<i32>().ok()),
        split.next().and_then(|v| v.trim().parse::<i32>().ok()),
    ) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            Some(webrender_api::units::DeviceIntSize::new(w, h))
        },
        _ => None,
    }
}

/// A [`Painter`] is responsible for all of the painting to a particular [`RenderingContext`].
/// This holds all of the WebRender specific data structures and state necessary for painting
/// and handling events that happen to `WebView`s that use a particular [`RenderingContext`].
/// Notable is that a [`Painter`] might be responsible for painting more than a single
/// [`WebView`] as long as they share the same [`RenderingContext`].
///
/// Each [`Painter`] also has its own [`RefreshDriver`] as well, which may be shared with
/// other [`Painter`]s. It's up to the embedder to decide which [`RefreshDriver`]s are associated
/// with a particular [`RenderingContext`].
pub(crate) struct Painter {
    /// The [`RenderingContext`] instance that webrender targets, which is the viewport.
    pub(crate) rendering_context: Rc<dyn RenderingContext>,

    /// The ID of this painter.
    pub(crate) painter_id: PainterId,

    /// Our [`WebViewRenderer`]s, one for every `WebView`.
    pub(crate) webview_renderers: FxHashMap<WebViewId, WebViewRenderer>,

    /// Tracks whether or not the view needs to be repainted.
    pub(crate) needs_repaint: Cell<RepaintReason>,

    /// The number of frames pending to receive from WebRender.
    pub(crate) pending_frames: Cell<usize>,

    /// Frame diagnostics waiting for the corresponding WebRender frame-ready notification.
    pending_frame_diagnostics: RefCell<VecDeque<PendingFrameDiagnostic>>,

    /// Local fallback frame counter used when a shared logical frame id is not provided.
    next_diagnostic_frame_id: Cell<u64>,

    /// The most recent local diagnostic frame id reported ready by WebRender.
    last_ready_local_frame_id: Cell<Option<u64>>,

    /// The most recent shared wall logical frame id reported ready by WebRender.
    last_ready_wall_logical_frame_id: Cell<Option<u64>>,

    /// Count of frames requested while this target still had older frames pending.
    overlapping_frame_request_count: Cell<u64>,

    /// Count of WebRender frame-ready notifications that arrived without a pending frame.
    unexpected_frame_ready_count: Cell<u64>,

    /// Count of render passes executed for this target.
    render_count: Cell<u64>,

    /// True while a display-paced composite (script rAF composite, per-video-arrival
    /// composite, deferred frame-delayer composite) has been requested but its render pass
    /// has not completed yet. While set, further display-paced composite requests are
    /// skipped (see `renderer_behind`), which keeps WebRender's publish queue depth at <= 1
    /// by construction. Reset when a render pass completes, or when a frame-ready arrives
    /// that will not trigger a repaint.
    /// Always written through `set_display_composite_in_flight`, never directly, so the
    /// timestamp below cannot drift out of step with it.
    display_composite_in_flight: Cell<bool>,

    /// When `display_composite_in_flight` was last set, for the force-release in
    /// `renderer_behind` (see [`DISPLAY_COMPOSITE_IN_FLIGHT_TIMEOUT`]).
    display_composite_in_flight_since: Cell<Option<Instant>>,

    /// When a video arrival last drove a composite, for the coalescing in
    /// [`Painter::video_composite_due`].
    last_video_driven_frame_at: Cell<Option<Instant>>,

    /// A video arrival asked for a composite and the painter could not issue one yet. Kept
    /// until it can, so a busy painter defers the request instead of dropping it.
    video_composite_owed: Cell<bool>,

    /// Diagnostic present-cadence accumulator (env `SERVO_LOG_PRESENT_CADENCE`). Measures the
    /// ACTUAL engine frame-ready rate and worst inter-frame gap per second, independent of the
    /// page's requestAnimationFrame count. `_start` is the current 1s window start, `_last` the
    /// previous frame-ready instant, `_count` frames this window, `_max_gap_ms` worst gap.
    present_cadence_start: Cell<Option<Instant>>,
    present_cadence_last: Cell<Option<Instant>>,
    present_cadence_count: Cell<u32>,
    present_cadence_max_gap_ms: Cell<f64>,

    /// Diagnostic (env `SERVO_LOG_PRESENT_CADENCE`): end instant of the previous render pass.
    /// A large gap since this instant at the START of a render means the stall happened
    /// upstream of the renderer (no render was requested at all), as opposed to a slow render
    /// pass itself, which is logged separately as "Slow paint frame".
    last_render_end: Cell<Option<Instant>>,

    /// 이 painter 가 스크립트의 캔버스 ack 를 붙잡기 시작한 시각, 그리고 마지막 보고 시각.
    /// ★스톨 진단용★ — [`Painter::check_canvas_ack_latch`] 참고.
    canvas_ack_held_since: Cell<Option<Instant>>,
    canvas_ack_last_report: Cell<Option<Instant>>,

    /// [`Painter::flush_owed_canvas_ack`] 가 실제로 보낸 횟수와 그 집계 창의 시작 시각.
    /// ★양성 대조용★ — 고쳐진 것과 코드가 안 도는 것을 구분하기 위한 것이다.
    owed_ack_flush_count: Cell<u64>,
    owed_ack_window_start: Cell<Option<Instant>>,

    /// 남은 ack 주입 횟수(`SERVO_WALL_CANVAS_ACK_SKIP`). 0 이면 주입 없음 = 기본값.
    canvas_ack_skips_left: Cell<u64>,

    /// 합성 요청 출처 집계(`SERVO_FRAME_REASON_PROF`). 키는 `generate_frame` 을 부른
    /// 호출 지점의 줄 번호와 RenderReasons 문자열이다. 요청 경로가 9 곳이라, 초당 200
    /// 회를 누가 만드는지 총합만으로는 가려낼 수 없다.
    frame_reason_counts: RefCell<FxHashMap<(u32, String), u64>>,
    frame_reason_window_start: Cell<Option<Instant>>,

    /// The [`BaseRefreshDriver`] which manages the painting of `WebView`s during animations.
    refresh_driver: Rc<BaseRefreshDriver>,

    /// A [`RefreshDriverObserver`] for WebView content animations.
    animation_refresh_driver_observer: Rc<AnimationRefreshDriverObserver>,

    /// The WebRender [`RenderApi`] interface used to communicate with WebRender.
    pub(crate) webrender_api: RenderApi,

    /// The active webrender document.
    pub(crate) webrender_document: DocumentId,

    /// The webrender renderer.
    pub(crate) webrender_renderer: Option<webrender::Renderer>,

    /// external fast-path(Task 4)용 DComp 컴포지터 공유 핸들. WR은 같은 인스턴스를
    /// `Box<SharedDComp>`(`CompositorConfig::Native`)로 소유하고, painter는 이 `Rc` 클론으로
    /// `present_external_only()`를 직접 호출한다(WR 프레임 빌드를 거치지 않는 경로).
    /// Native 컴포지터 미발동(Draw 폴백 포함)이면 `None`.
    #[cfg(windows)]
    pub(crate) dcomp_shared: Option<Rc<RefCell<crate::dcomp_compositor::DCompNativeCompositor>>>,

    /// external fast-present 마지막 시각(refresh 페이싱: ~60/s로 coalesce, 도착률 ~1080/s
    /// 방지). `dcomp_shared`의 fast-path에서만 읽으므로 같이 `#[cfg(windows)]`로 게이트한다.
    #[cfg(windows)]
    last_external_present: Cell<Option<Instant>>,

    /// The GL bindings for webrender
    webrender_gl: Rc<dyn gleam::gl::Gl>,

    /// The last position in the rendered view that the mouse moved over. This becomes `None`
    /// when the mouse leaves the rendered view.
    pub(crate) last_mouse_move_position: Option<DevicePoint>,

    /// A [`ScreenshotTaker`] responsible for handling all screenshot requests.
    pub(crate) screenshot_taker: ScreenshotTaker,

    /// A [`FrameRequestDelayer`] which is used to wait for canvas image updates to
    /// arrive before requesting a new frame, as these happen asynchronously with
    /// `ScriptThread` display list construction.
    pub(crate) frame_delayer: FrameDelayer,

    /// The channel on which messages can be sent to the constellation.
    embedder_to_constellation_sender: Sender<EmbedderToConstellationMessage>,

    /// Calculater for largest-contentful-paint.
    lcp_calculator: LargestContentfulPaintCalculator,

    /// A cache that stores data for all animating images uploaded to WebRender. This is used
    /// for animated images, which only need to update their offset in the data.
    animation_image_cache: FxHashMap<ImageKey, Arc<Vec<u8>>>,

    /// Latest-wins staging for immediate (epoch-less, i.e. video) image updates. Instead of
    /// forwarding every arriving video frame to WebRender, the newest update per image key is
    /// held here and flushed into the next composite (any `generate_frame_with_diagnostic_id`
    /// call). Rationale: WebRender cannot skip a published-but-unrendered frame whose resource
    /// updates touch the texture cache (`must_be_drawn` forces a full offscreen render+upload
    /// per queued frame), and with many videos the upload demand sits at the renderer-thread
    /// throughput limit, so any hiccup (e.g. a synchronized loop-restart burst) amplifies into
    /// multi-second stalls unless stale frames are dropped here at the source. Only the newest
    /// frame of each video has display value.
    ///
    /// 킬 스위치(`SERVO_DISABLE_VIDEO_UPDATE_COALESCE`)가 있었지만 걷어냈다 — 병합은
    /// 확정 동작이다. 이 조사의 **최종 fix 는 병합이 아니라 in-flight 합성 게이트**였고
    /// (2026-07-09 검증 완료, 45타일 63.7fps/스톨 0), 병합은 백로그 드레인을 빠르게 하는
    /// 보조로 남아 상시 켜져 있다. A/B 가 끝난 게이트를 남기면 죽은 분기가 쌓인다.
    pending_video_frame_updates: RefCell<FxHashMap<ImageKey, (ImageDescriptor, SerializableImageData)>>,

    /// A [`WebContentAnimator`] used to manage web content-derived animations. Currently this only
    /// manages blinking caret animations.
    web_content_animator: WebContentAnimator,

    /// True when the WR Native Compositor (DirectComposition) is engaged for this window
    /// (env `SERVO_COMPOSITOR_DCOMP` on and `maybe_create` succeeded). Off = current Draw path.
    /// Used to restore the window surface as current after `renderer.render()`, because the
    /// native compositor's `bind` leaves a pbuffer current.
    #[cfg(windows)]
    dcomp_native_active: bool,

    // --- Runtime-resize stale-content 방지 상태 (DComp Native 경로 전용) ---
    // 근본원인(task-10/10b): 리사이즈 이전에 DComp 가상 서피스에 그려진 콘텐츠는, 재레이아웃
    // 후 그 타일 영역에 프리미티브가 사라지면 WR(webrender-0.68 native path)이 valid로 간주해
    // 재-bind하지 않아 옛 픽셀이 영원히 남는다(시계/패널 테두리 잔상). task-10b는 이를
    // FORCE_PICTURE_INVALIDATION으로 못 고침을 증명했다(그 플래그는 "재계산"만 강제할 뿐
    // vacated 영역을 재도색하지 않음). 해법: SetPictureTileSize로 타일 크기를 실제로 바꿔
    // picture-cache 슬라이스를 통째로 destroy_surface/재생성(picture.rs:2320-2332)시켜 옛
    // 콘텐츠를 물리적으로 소멸시킨다.

    /// 리사이즈가 발생해 재구축을 기다리는 중(디바운스 진행 중).
    #[cfg(windows)]
    dcomp_resize_pending: Cell<bool>,
    /// 마지막 크기 변경 이후 흐른 프레임 수(안정화 카운터).
    #[cfg(windows)]
    dcomp_resize_stable_frames: Cell<u32>,
    /// 기동 시 정상상태(steady-state) picture 타일 크기 오버라이드. `SERVO_WR_PICTURE_TILE_SIZE`가
    /// 설정돼 있으면 그 파싱값(Some), 아니면 None(= WR 기본값 사용 — 콘텐츠는 TILE_SIZE_DEFAULT
    /// 1024x512, 스크롤바는 TILE_SIZE_SCROLLBAR_*로 WR이 자체 분기). 재구축이 항상 이 값으로
    /// 수렴해야 `-TileSize` A/B 실험과 스크롤바 특수 타일 크기가 훼손되지 않는다(리뷰 지적).
    #[cfg(windows)]
    dcomp_tile_size_steady: Option<webrender_api::units::DeviceIntSize>,
    /// steady와 반드시 다른 유효 타일 크기(내부 크기 기준). 드래그 시작 때 이 값을 먼저 보내
    /// desired != current를 보장해 파괴/재생성을 강제한다. steady가 None이면 WR 기본
    /// 1024x512와 비교해 다른 값을 고른다(picture.rs:2306-2311의 default_tile_size와 충돌 방지).
    #[cfg(windows)]
    dcomp_tile_size_alternate: webrender_api::units::DeviceIntSize,
}

struct PendingFrameDiagnostic {
    local_frame_id: u64,
    wall_logical_frame_id: Option<u64>,
    requested_at: Instant,
    wall_requested_at: Option<Instant>,
    reason: String,
}

pub(crate) struct FrameReadyDiagnostic {
    pub(crate) painter_id: PainterId,
    pub(crate) local_frame_id: u64,
    pub(crate) wall_logical_frame_id: Option<u64>,
    pub(crate) ready_at: Instant,
    pub(crate) wait_ms: f64,
    pub(crate) need_repaint: bool,
}

impl Drop for Painter {
    fn drop(&mut self) {
        if let Err(error) = self.rendering_context.make_current() {
            error!("Failed to make the rendering context current: {error:?}");
        }

        self.webrender_api.stop_render_backend();
        self.webrender_api.shut_down(true);

        if let Some(renderer) = self.webrender_renderer.take() {
            renderer.deinit();
        }
    }
}

impl Painter {
    pub(crate) fn new(rendering_context: Rc<dyn RenderingContext>, paint: &Paint) -> Self {
        let webrender_gl = rendering_context.gleam_gl_api();

        // Make sure the gl context is made current.
        if let Err(err) = rendering_context.make_current() {
            warn!("Failed to make the rendering context current: {:?}", err);
        }
        debug_assert_eq!(webrender_gl.get_error(), gleam::gl::NO_ERROR,);

        // D3D11 비디오 경로에서는 업로드/변환이 파이프라인 스트리밍 스레드로 분산되어,
        // 시작 릴리즈·루프 경계에 N개 프로듀서 버스트가 CPU를 초과구독하면 이 스레드
        // (합성·출력)가 굶어 화면이 수 초 정지한다(2026-07-10 조사 §10 실측). 출력은
        // 지연 민감 경로이므로 프로듀서보다 높은 우선순위를 준다. 기존(Raw) 경로는
        // 렌더러가 업로드 주체라 동작이 검증된 그대로 두기 위해 게이트로 한정.
        // media_d3d11_enabled pref(config-surface-consolidation Task 5, 구 env
        // SERVO_MEDIA_D3D11_VIDEO) — media(render-d3d11)와 이 크레이트가 각자 읽던 것을
        // 하나의 pref 로 합쳤다. 두 판정식이 문자 그대로 동일함을 확인했다(2026-08-12).
        #[cfg(windows)]
        if pref!(media_d3d11_enabled) {
            // SetThreadPriority는 FFI 호출 — 인자는 현재 스레드 핸들(GetCurrentThread)과
            // 유효한 우선순위 상수뿐이라 안전성 불변식 위반 여지가 없다.
            #[allow(unsafe_code)]
            unsafe {
                use winapi::um::processthreadsapi::{GetCurrentThread, SetThreadPriority};
                use winapi::um::winbase::THREAD_PRIORITY_ABOVE_NORMAL;
                if SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL as i32) == 0 {
                    log::warn!("D3D11 video: 렌더러 스레드 우선순위 설정 실패");
                } else {
                    log::info!("D3D11 video: 렌더러 스레드 우선순위 ABOVE_NORMAL 적용");
                }
            }
        }

        let painter_id = PainterId::next();
        let id_manager = paint.webrender_external_image_id_manager();
        let mut external_image_handlers = Box::new(WebRenderExternalImageHandlers::new(id_manager));

        // Set WebRender external image handler for WebGL textures.
        let image_handler = Box::new(WebGLExternalImages::new(
            painter_id,
            paint.webgl_threads(),
            rendering_context.clone(),
            paint.swap_chains.clone(),
            paint.busy_webgl_contexts_map.clone(),
        ));
        external_image_handlers.set_handler(image_handler, WebRenderImageHandlerType::WebGl);

        // GPU-direct present: this painter's GPU LUID, so its WebGPU external-image handler
        // samples the shared texture from its own GPU.
        #[cfg(feature = "webgpu")]
        let webgpu_painter_luid = rendering_context
            .requested_gpu_index()
            .and_then(paint_api::rendering_context::dxgi_luid_for_gpu_index);
        #[cfg(feature = "webgpu")]
        external_image_handlers.set_handler(
            Box::new(webgpu::WebGpuExternalImages::new(
                paint.webgpu_image_map(),
                rendering_context.clone(),
                webgpu_painter_luid,
            )),
            WebRenderImageHandlerType::WebGpu,
        );

        WindowGLContext::initialize_image_handler(
            &mut external_image_handlers,
            rendering_context.clone(),
        );

        let embedder_to_constellation_sender = paint.embedder_to_constellation_sender.clone();
        let timer_refresh_driver = LazyCell::default();
        let refresh_driver = Rc::new(BaseRefreshDriver::new(
            paint.event_loop_waker.clone_box(),
            rendering_context.refresh_driver(),
            &timer_refresh_driver,
        ));
        let animation_refresh_driver_observer = Rc::new(AnimationRefreshDriverObserver::new(
            embedder_to_constellation_sender.clone(),
        ));

        rendering_context.prepare_for_rendering();
        let clear_color = servo_config::pref!(shell_background_color_rgba);
        let clear_color = ColorF::new(
            clear_color[0] as f32,
            clear_color[1] as f32,
            clear_color[2] as f32,
            clear_color[3] as f32,
        );

        // Use same texture upload method as Gecko with ANGLE:
        // https://searchfox.org/mozilla-central/source/gfx/webrender_bindings/src/bindings.rs#1215-1219
        let upload_method = if webrender_gl.get_string(RENDERER).starts_with("ANGLE") {
            UploadMethod::Immediate
        } else {
            UploadMethod::PixelBuffer(ONE_TIME_USAGE_HINT)
        };
        let worker_threads = std::thread::available_parallelism()
            .map(|i| i.get())
            .unwrap_or(pref!(thread_pool_fallback_workers) as usize)
            .min(pref!(thread_pool_webrender_workers_max).max(1) as usize);
        let workers = Some(Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(worker_threads)
                .thread_name(|idx| format!("WRWorker#{}", idx))
                .build()
                .expect("Unable to initialize WebRender worker pool."),
        ));

        // Native Compositor(DComp) gate: when on, WR draws its picture-cache tiles directly
        // into DComp surfaces and DWM composites them (eliminates the tile->backbuffer draw
        // pass — spec 2026-07-13). On failure fall back to the Draw compositor (byte-identical
        // to current behaviour). Off (default) leaves compositor_config at its Draw default.
        #[cfg(windows)]
        let (compositor_config, dcomp_shared) = if crate::dcomp_compositor::enabled() {
            match crate::dcomp_compositor::maybe_create(&rendering_context) {
                Some(shared) => {
                    log::info!("[dcomp-native] engaged: WR native compositor (DirectComposition)");
                    // 실제 발동을 rendering_context에 알린다 — present()의 스킵 판정은
                    // env 게이트가 아닌 이 신호를 본다(리뷰 픽스: env on + 발동 실패로
                    // Draw 폴백된 경우 present가 잘못 스킵되면 블랭크 윈도우가 된다).
                    rendering_context.set_dcomp_native_active(true);
                    // WR엔 얇은 위임 래퍼(SharedDComp)를 넘기고, painter는 같은 인스턴스의
                    // Rc를 보관해 external fast-path(Task 4)에서 직접 접근한다.
                    (
                        webrender::CompositorConfig::Native {
                            compositor: Box::new(crate::dcomp_compositor::SharedDComp(
                                shared.clone(),
                            )),
                        },
                        Some(shared),
                    )
                },
                None => {
                    log::warn!("[dcomp-native] init failed; falling back to Draw compositor");
                    // 초기값 false에 암묵 의존하지 않고 폴백임을 명시한다(§set_dcomp_native_active
                    // 계약: true로의 전이는 발동 성공 시 1회뿐이고, 폴백은 항상 false를 유지).
                    rendering_context.set_dcomp_native_active(false);
                    (webrender::CompositorConfig::default(), None)
                },
            }
        } else {
            (webrender::CompositorConfig::default(), None)
        };
        #[cfg(not(windows))]
        let compositor_config = webrender::CompositorConfig::default();
        #[cfg(windows)]
        let dcomp_native_active =
            matches!(compositor_config, webrender::CompositorConfig::Native { .. });

        let (mut webrender_renderer, webrender_api_sender) = webrender::create_webrender_instance(
            webrender_gl.clone(),
            Box::new(RenderNotifier::new(painter_id, paint.paint_proxy.clone())),
            webrender::WebRenderOptions {
                compositor_config,
                // We force the use of optimized shaders here because rendering is broken
                // on Android emulators with unoptimized shaders. This is due to a known
                // issue in the emulator's OpenGL emulation layer.
                // See: https://github.com/servo/servo/issues/31726
                use_optimized_shaders: true,
                resource_override_path: opts::get().shaders_path.clone(),
                debug_flags: webrender::DebugFlags::empty(),
                precache_flags: if pref!(gfx_precache_shaders) {
                    ShaderPrecacheFlags::FULL_COMPILE
                } else {
                    ShaderPrecacheFlags::empty()
                },
                enable_aa: pref!(gfx_text_antialiasing_enabled),
                enable_subpixel_aa: pref!(gfx_subpixel_text_antialiasing_enabled),
                allow_texture_swizzling: pref!(gfx_texture_swizzling_enabled),
                enable_dithering: true,
                clear_color,
                upload_method,
                workers,
                size_of_op: Some(servo_allocator::usable_size),
                // This ensures that we can use the `PainterId` as the `IdNamespace`, which allows mapping
                // from `FontKey`, `FontInstanceKey`, and `ImageKey` back to `PainterId`.
                namespace_alloc_by_client: true,
                shared_font_namespace: Some(painter_id.into()),
                ..Default::default()
            },
            None,
        )
        .expect("Unable to initialize WebRender.");

        webrender_renderer.set_external_image_handler(external_image_handlers);

        let webrender_api = webrender_api_sender.create_api_by_client(painter_id.into());
        let surface_size = rendering_context.size2d().to_i32();
        let webrender_document = webrender_api.add_document(surface_size);

        // WR picture cache 타일 크기 오버라이드 (`gfx_wr_picture_tile_size` pref, 구 env
        // SERVO_WR_PICTURE_TILE_SIZE). 빈 문자열이면 WR 기본(콘텐츠 1024x512, 스크롤바는 WR이
        // 자체 특수 크기 분기 — picture.rs:2306-2311)을 그대로 쓴다. 타일 수·무효화 입도·
        // per-tile 오버헤드(DComp bind/unbind 횟수)가 함께 달라진다 — 값이 창 이상이면
        // 슬라이스당 타일 1장이고, `display` 가 정확히 그 상태를 노린다.
        //
        // ★WR 은 이 값을 검사하지도 클램프하지도 않는다★(2026-08-12 확인: render_backend.rs 가
        // frame_config.tile_size_override 에 그대로 넣고 picture.rs:2302 가 그대로
        // desired_tile_size 로 쓴다). 실질 상한은 GPU 텍스처 크기다.
        let steady_tile_size_override = {
            let value = pref!(gfx_wr_picture_tile_size);
            let trimmed = value.trim();
            let resolved = resolve_wr_tile_size(trimmed, surface_size);
            // `display` 가 None 으로 떨어지는 경우는 창 크기가 0 일 때뿐이고 그때는
            // resolve_wr_tile_size 가 자체 경고를 찍는다 — 여기서는 형식 오류만 보고한다.
            let unparsed = resolved.is_none()
                && !trimmed.is_empty()
                && !trimmed.eq_ignore_ascii_case("display");
            if unparsed {
                log::warn!(
                    "[wr-tile-size] gfx_wr_picture_tile_size 형식 오류(WxH 또는 display 기대): {value}"
                );
            }
            resolved
        };
        if let Some(size) = steady_tile_size_override {
            webrender_api.send_debug_cmd(webrender::DebugCommand::SetPictureTileSize(Some(size)));
            log::info!(
                "[wr-tile-size] picture tile size override: {}x{}",
                size.width,
                size.height
            );
        }

        let gl_renderer = webrender_gl.get_string(gleam::gl::RENDERER);
        let gl_version = webrender_gl.get_string(gleam::gl::VERSION);
        info!("Running on {gl_renderer} with OpenGL version {gl_version}");

        // Runtime-resize 재구축용 steady/alternate picture 타일 크기 계산.
        // steady = 기동 시 결정된 정상상태(위 steady_tile_size_override 그대로) — 재구축은
        // 항상 이 값으로 수렴해야 한다(리뷰 지적: 하드코딩된 512x512에 눌러앉으면 이 노브가
        // 무력화되고 WR 스크롤바 특수 타일 크기도 깨진다). steady가 None이면 SetPictureTileSize
        // 자체를 None으로 되돌려 WR 기본 분기(TILE_SIZE_DEFAULT/스크롤바)를 복원한다.
        // alternate = steady의 실제 크기와 **반드시 달라야** 한다(그것이 재구축의 트리거다) —
        // steady가 None이면 WR 기본 콘텐츠 크기 1024x512와 비교해 고른다. 드래그 시작 발동은
        // 항상 alternate를 먼저 보내 desired != current(=steady)를 보장한다.
        // (이 주석은 예전에 "WR 클램프 128..=4096 내"라고 적고 있었으나 근거가 없다 —
        // 2026-08-12 확인: WR 은 tile_size_override 를 검사도 클램프도 하지 않는다.)
        #[cfg(windows)]
        let (dcomp_tile_size_steady, dcomp_tile_size_alternate) = {
            use webrender_api::units::DeviceIntSize;
            let steady_effective_default = steady_tile_size_override.unwrap_or(DeviceIntSize::new(1024, 512));
            let alternate = if steady_effective_default.width == 512 && steady_effective_default.height == 512 {
                DeviceIntSize::new(1024, 512)
            } else {
                DeviceIntSize::new(512, 512)
            };
            (steady_tile_size_override, alternate)
        };

        let painter = Painter {
            painter_id,
            embedder_to_constellation_sender,
            webview_renderers: Default::default(),
            rendering_context,
            needs_repaint: Cell::default(),
            pending_frames: Default::default(),
            pending_frame_diagnostics: Default::default(),
            next_diagnostic_frame_id: Cell::new(0),
            last_ready_local_frame_id: Default::default(),
            last_ready_wall_logical_frame_id: Default::default(),
            overlapping_frame_request_count: Default::default(),
            unexpected_frame_ready_count: Default::default(),
            render_count: Default::default(),
            display_composite_in_flight: Default::default(),
            display_composite_in_flight_since: Default::default(),
            last_video_driven_frame_at: Default::default(),
            video_composite_owed: Default::default(),
            present_cadence_start: Default::default(),
            present_cadence_last: Default::default(),
            present_cadence_count: Default::default(),
            present_cadence_max_gap_ms: Default::default(),
            last_render_end: Default::default(),
            canvas_ack_held_since: Default::default(),
            canvas_ack_last_report: Default::default(),
            owed_ack_flush_count: Default::default(),
            owed_ack_window_start: Default::default(),
            canvas_ack_skips_left: Cell::new(*WALL_CANVAS_ACK_SKIP),
            frame_reason_counts: Default::default(),
            frame_reason_window_start: Default::default(),
            screenshot_taker: Default::default(),
            refresh_driver,
            animation_refresh_driver_observer,
            webrender_renderer: Some(webrender_renderer),
            #[cfg(windows)]
            dcomp_shared,
            #[cfg(windows)]
            last_external_present: Cell::new(None),
            webrender_api,
            webrender_document,
            webrender_gl,
            last_mouse_move_position: None,
            frame_delayer: Default::default(),
            lcp_calculator: LargestContentfulPaintCalculator::new(),
            animation_image_cache: FxHashMap::default(),
            pending_video_frame_updates: RefCell::new(FxHashMap::default()),
            web_content_animator: WebContentAnimator::new(
                paint.event_loop_waker.clone_box(),
                (*timer_refresh_driver).clone(),
            ),
            #[cfg(windows)]
            dcomp_native_active,
            #[cfg(windows)]
            dcomp_resize_pending: Cell::new(false),
            #[cfg(windows)]
            dcomp_resize_stable_frames: Cell::new(0),
            #[cfg(windows)]
            dcomp_tile_size_steady,
            #[cfg(windows)]
            dcomp_tile_size_alternate,
        };
        painter.assert_gl_framebuffer_complete();
        painter.clear_background();
        painter
    }

    pub(crate) fn perform_updates(&mut self) {
        // The WebXR thread may make a different context current
        if let Err(err) = self.rendering_context.make_current() {
            warn!("Failed to make the rendering context current: {:?}", err);
        }

        let mut need_zoom = false;
        let scroll_offset_updates: Vec<_> = self
            .webview_renderers
            .values_mut()
            .filter_map(|webview_renderer| {
                let (zoom, scroll_result) = webview_renderer
                    .process_pending_scroll_and_pinch_zoom_events(&self.webrender_api);
                need_zoom = need_zoom || (zoom == PinchZoomResult::DidPinchZoom);
                scroll_result
            })
            .collect();

        self.send_zoom_and_scroll_offset_updates(need_zoom, scroll_offset_updates);

        if let Some(colors) = self.web_content_animator.update(&self.webview_renderers) {
            let mut transaction = Transaction::new();
            transaction.reset_dynamic_properties();
            transaction.append_dynamic_properties(DynamicProperties {
                transforms: Vec::new(),
                floats: Vec::new(),
                colors,
            });
            self.generate_frame(&mut transaction, RenderReasons::ANIMATED_PROPERTY);
            self.send_transaction(transaction);
        }
    }

    #[track_caller]
    fn assert_no_gl_error(&self) {
        debug_assert_eq!(self.webrender_gl.get_error(), gleam::gl::NO_ERROR);
    }

    #[track_caller]
    fn assert_gl_framebuffer_complete(&self) {
        debug_assert_eq!(
            (
                self.webrender_gl.get_error(),
                self.webrender_gl
                    .check_frame_buffer_status(gleam::gl::FRAMEBUFFER)
            ),
            (gleam::gl::NO_ERROR, gleam::gl::FRAMEBUFFER_COMPLETE)
        );
    }

    pub(crate) fn webview_renderer(&self, webview_id: WebViewId) -> Option<&WebViewRenderer> {
        self.webview_renderers.get(&webview_id)
    }

    /// Whether the visible tile this painter actually renders (its rendering-context-sized
    /// slice of the virtual WebView viewport, at the renderer's `viewport_origin`) contains
    /// `point`. Used to route positional input to the correct tile painter on the multi-GPU
    /// wall. Note the renderer's `rect` is the *virtual* viewport (shared by all tiles), so we
    /// must test against the rendering-context size, not `rect`.
    pub(crate) fn rendered_tile_contains_input_point(
        &self,
        webview_id: WebViewId,
        point: WebViewPoint,
    ) -> bool {
        self.webview_renderer(webview_id).is_some_and(|renderer| {
            let device_point = point.as_device_point(renderer.device_pixels_per_page_pixel());
            let render_point = renderer.render_point_from_viewport_point(device_point);
            let size = self.rendering_context.size2d();
            render_point.x >= 0.0 &&
                render_point.y >= 0.0 &&
                render_point.x < size.width as f32 &&
                render_point.y < size.height as f32
        })
    }

    pub(crate) fn webview_renderer_mut(
        &mut self,
        webview_id: WebViewId,
    ) -> Option<&mut WebViewRenderer> {
        self.webview_renderers.get_mut(&webview_id)
    }

    /// Whether or not the renderer is waiting on a frame, either because it has been sent
    /// to WebRender and is not ready yet or because the [`FrameDelayer`] is delaying a frame
    /// waiting for asynchronous (canvas) image updates to complete.
    pub(crate) fn has_pending_frames(&self) -> bool {
        self.pending_frames.get() != 0 || self.frame_delayer.pending_frame
    }

    pub(crate) fn set_needs_repaint(&self, reason: RepaintReason) {
        let mut needs_repaint = self.needs_repaint.get();
        needs_repaint.insert(reason);
        self.needs_repaint.set(needs_repaint);
    }

    pub(crate) fn needs_repaint(&self) -> bool {
        let repaint_reason = self.needs_repaint.get();
        if repaint_reason.is_empty() {
            return false;
        }

        !self.refresh_driver.wait_to_paint()
    }

    /// Returns true if any animation callbacks (ie `requestAnimationFrame`) are waiting for a response.
    pub(crate) fn animation_callbacks_running(&self) -> bool {
        self.webview_renderers
            .values()
            .any(WebViewRenderer::animation_callbacks_running)
    }

    pub(crate) fn animating_webviews(&self) -> Vec<WebViewId> {
        self.webview_renderers
            .values()
            .filter_map(|webview_renderer| {
                if webview_renderer.animating() {
                    Some(webview_renderer.id)
                } else {
                    None
                }
            })
            .collect()
    }

    pub(crate) fn send_to_constellation(&self, message: EmbedderToConstellationMessage) {
        if let Err(error) = self.embedder_to_constellation_sender.send(message) {
            warn!("Could not send message to constellation ({error:?})");
        }
    }

    /// 스크립트에 빚진 캔버스 ack 를 보낼 수 있으면 **지금** 보낸다.
    ///
    /// ★없으면 월이 영구히 멈춘다.★ 캔버스를 그린 문서는 `waiting_on_canvas_image_updates`
    /// 로 잠기고 `script_thread.rs:1200` 에서 rAF 가 통째로 건너뛰어진다. 그 잠금은 이
    /// painter 가 `NoLongerWaitingOnAsynchronousImageUpdates` 를 보내야만 풀리는데, 그것을
    /// 보내는 두 자리(`generate_frame_for_script`, `update_images`)는 각각 조건이 안 맞으면
    /// 그냥 빠져나가고 **재확인을 "다음 `update_images` 호출"에 맡긴다**. 그 약속이
    /// 캔버스만 있는 페이지에서는 거짓이다 — 이미지를 만드는 것이 스크립트인데 그 스크립트가
    /// 바로 이 ack 를 기다리고 있으므로 다음 호출이 영원히 오지 않는다.
    ///
    /// 실측(2026-09-02, `log_webgpu/51` `ack_34`): 네 painter 전부 46.5 초 동안
    /// `pending_frame=true pending_canvas_images=0 renderer_behind=false` 였다. ★보낼 조건이
    /// 다 갖춰진 ack 를 부를 사람이 없어서 못 보낸 것★이지 무엇에 막힌 것이 아니었다.
    /// 비디오 페이지가 멀쩡한 이유도 같다 — `update_images` 가 끊임없이 와서 재확인이
    /// 저절로 일어난다.
    ///
    /// 그래서 매 페인트 기회마다 한 번 되묻는다. 조건이 안 맞으면 즉시 빠지므로 정상 경로의
    /// 타이밍은 그대로다(빚이 없으면 첫 줄에서 끝난다). 보내고 나면 빚이 사라져 다음 렌더에서는
    /// 다시 첫 줄에서 끝나므로, 한 번만 나간다.
    /// ack 전송을 한 번 일부러 건너뛸지(`SERVO_WALL_CANVAS_ACK_SKIP`). 기본값 0 = 항상 false.
    ///
    /// 건너뛸 때 빚(`waiting_pipelines`)과 `pending_frame` 을 ★그대로 남기는 것이 요점★이다 —
    /// 게이트가 닫혀 있어서 못 보낸 것과 똑같은 상태가 되어야 실측된 교착이 재현된다.
    /// [`Painter::flush_owed_canvas_ack`] 는 이것을 소비하지 않는다. 복구까지 같이 막으면
    /// 주입이 무엇을 증명하는지가 사라진다.
    fn consume_canvas_ack_skip(&self) -> bool {
        let left = self.canvas_ack_skips_left.get();
        if left == 0 {
            return false;
        }

        // ★빚이 없는 순간에 예산을 쓰면 아무것도 보류하지 못한다.★ 처음엔 이 검사가 없어서
        // 네 painter 전부 기동 직후 첫 기회에 예산을 날렸고(2026-09-03, `log_webgpu/53`:
        // 28 회 중 stall 2 회 = 주입 없을 때의 자연 발생률 그대로, 그나마도 skip 과 22 초
        // 떨어져 있었다), 주입이 **아무 일도 하지 않았다**. 보류할 것이 실제로 있을 때만 쓴다.
        let owed = self.frame_delayer.waiting_pipeline_count();
        if owed == 0 {
            return false;
        }

        self.canvas_ack_skips_left.set(left - 1);
        warn!(
            "WALLACKSKIP: painter {:?} withheld an ack owed to {} pipeline(s) on purpose \
             ({} skip(s) left). Failure injection only -- SERVO_WALL_CANVAS_ACK_SKIP is set.",
            self.painter_id,
            owed,
            left - 1,
        );
        true
    }

    fn flush_owed_canvas_ack(&mut self) {
        if *WALL_DISABLE_CANVAS_ACK_RECOVERY {
            return;
        }
        if !self.frame_delayer.is_holding_ack() {
            return;
        }
        if !self.frame_delayer.needs_new_frame() || self.renderer_behind() {
            return;
        }

        // `update_images` 의 같은 블록과 하는 일이 정확히 같아야 한다 — 여기서만 다르게 하면
        // 두 경로가 갈라진다.
        let mut transaction = Transaction::new();
        self.frame_delayer.set_pending_frame(false);
        self.generate_frame(&mut transaction, RenderReasons::SCENE);
        self.set_display_composite_in_flight(true);

        let waiting_pipelines = self.frame_delayer.take_waiting_pipelines();
        self.send_to_constellation(
            EmbedderToConstellationMessage::NoLongerWaitingOnAsynchronousImageUpdates(
                waiting_pipelines,
            ),
        );

        self.screenshot_taker
            .prepare_screenshot_requests_for_render(&*self);
        if !transaction.is_empty() {
            self.send_transaction(transaction);
        }

        // ★양성 대조★: 고친 뒤에는 스톨이 안 나므로 "아무 줄도 없음"이 남는데, 그것만으로는
        // **고쳐진 것**과 **이 코드가 아예 안 도는 것**이 구분되지 않는다. 이 조사에서만 네 번
        // 같은 함정에 빠졌으므로, 실제로 보낸 횟수를 초당 한 줄로 남긴다. 0 이면 안 찍는다.
        let count = self.owed_ack_flush_count.get() + 1;
        self.owed_ack_flush_count.set(count);
        let now = Instant::now();
        let window_start = self.owed_ack_window_start.get().unwrap_or_else(|| {
            self.owed_ack_window_start.set(Some(now));
            now
        });
        if now.duration_since(window_start).as_secs_f64() >= 1.0 {
            warn!(
                "WALLACKFLUSH: painter {:?} sent {} owed canvas ack(s) in the last {:.1}s. \
                 Each one is a frame script would otherwise have waited for forever.",
                self.painter_id,
                count,
                now.duration_since(window_start).as_secs_f64(),
            );
            self.owed_ack_flush_count.set(0);
            self.owed_ack_window_start.set(Some(now));
        }
    }

    /// 스크립트가 이 painter 의 캔버스 ack 를 기다리다 잠긴 채로 남아 있는지 본다.
    ///
    /// ★여기에 거는 이유★: 스톨이 나면 스크립트도 WebGL 스레드도 논리 프레임도 전부 멈추는데
    /// **렌더 패스만은 계속 돈다**(winit_wall 의 표출 클럭이 내용과 무관하게 그리므로,
    /// 2026-09-02 스톨 실행에서 초당 185회). 그래서 살아 있는 유일한 자리가 여기다.
    ///
    /// 잠기는 구조: 캔버스를 그린 문서는 `waiting_on_canvas_image_updates` 로 잠기고
    /// (`script_thread.rs:1200` 에서 rAF 가 통째로 건너뛰어진다), 그 잠금은 painter 가
    /// `NoLongerWaitingOnAsynchronousImageUpdates` 를 보내야만 풀린다. 그런데 그것을 보내는
    /// 두 자리(`generate_frame_for_script`, `update_images`)는 **둘 다 외부 트리거를 요구**하고
    /// 그 트리거는 전부 스크립트 하류에 있다. 한 번 어긋나면 재시도할 주체가 없다.
    ///
    /// 건강할 때는 한 줄도 안 찍는다. 찍혔다면 그 자체가 병증이다.
    fn check_canvas_ack_latch(&self) {
        if !self.frame_delayer.is_holding_ack() {
            self.canvas_ack_held_since.set(None);
            self.canvas_ack_last_report.set(None);
            return;
        }
        let now = Instant::now();
        let held_since = match self.canvas_ack_held_since.get() {
            Some(since) => since,
            None => {
                self.canvas_ack_held_since.set(Some(now));
                return;
            },
        };
        let held_ms = now.duration_since(held_since).as_secs_f64() * 1000.0;
        if held_ms < 1000.0 {
            return;
        }
        if let Some(last) = self.canvas_ack_last_report.get() {
            if now.duration_since(last).as_secs_f64() < 1.0 {
                return;
            }
        }
        self.canvas_ack_last_report.set(Some(now));
        // 두 게이트의 값을 그대로 싣는다 — 어느 쪽이 막았는지가 이 줄의 존재 이유다.
        warn!(
            "WALLACKLATCH: painter {:?} held the canvas ack for {:.0}ms; \
             pending_frame={} pending_canvas_images={} renderer_behind={} \
             composite_in_flight={}. Script cannot run another rAF until it is sent.",
            self.painter_id,
            held_ms,
            self.frame_delayer.pending_frame,
            self.frame_delayer.pending_canvas_image_count(),
            self.renderer_behind(),
            self.display_composite_in_flight.get(),
        );
    }

    #[servo_tracing::instrument(skip_all)]
    pub(crate) fn render(&mut self, time_profiler_channel: &ProfilerChan) {
        // 복구가 먼저, 진단이 나중. 이 순서라면 아래 경고가 찍힌다는 것은 곧 "복구조차 할 수
        // 없는 상태"라는 뜻이므로, 남겨 두면 회귀 탐지기가 된다.
        self.flush_owed_canvas_ack();
        self.check_canvas_ack_latch();
        let render_count = self.render_count.get() + 1;
        self.render_count.set(render_count);
        let local_frame_id = self.last_ready_local_frame_id.get();
        let wall_logical_frame_id = self.last_ready_wall_logical_frame_id.get();
        let render_start = Instant::now();
        if self.rendering_context.requested_gpu_index().is_some() {
            info!(
                "Wall render start: painter {:?} render_count={} local_frame_id={:?} \
                 logical_frame_id={:?} pending={} overlapping_request_count={} \
                 unexpected_ready_count={} requested_gpu={:?} size={:?}",
                self.painter_id,
                render_count,
                local_frame_id,
                wall_logical_frame_id,
                self.pending_frames.get(),
                self.overlapping_frame_request_count.get(),
                self.unexpected_frame_ready_count.get(),
                self.rendering_context.requested_gpu_index(),
                self.rendering_context.size(),
            );
        } else {
            debug!(
                "Render start: painter {:?} render_count={} local_frame_id={:?} \
                 logical_frame_id={:?} pending={} size={:?}",
                self.painter_id,
                render_count,
                local_frame_id,
                wall_logical_frame_id,
                self.pending_frames.get(),
                self.rendering_context.size(),
            );
        }

        let refresh_driver = self.refresh_driver.clone();
        refresh_driver.notify_will_paint(self);

        // Diagnostic (SERVO_LOG_PRESENT_CADENCE): a large gap since the END of the previous
        // render pass means the stall happened upstream of the renderer (no render was
        // requested during the gap), as opposed to a slow render pass itself ("Slow paint
        // frame" below).
        if *LOG_PRESENT_CADENCE {
            if let Some(last_end) = self.last_render_end.get() {
                let gap_ms = last_end.elapsed().as_secs_f64() * 1000.0;
                if gap_ms > 100.0 {
                    info!(
                        "Paint gap: painter {:?} gap_since_last_render_end_ms={:.1}",
                        self.painter_id, gap_ms,
                    );
                }
            }
        }

        // Diagnostic breakdown (env SERVO_LOG_PRESENT_CADENCE): WebRender update() applies
        // pending resource updates (notably per-frame video texture uploads); render() draws
        // and composites the scene. Splitting them tells whether an over-budget frame is
        // upload-bound or draw/composite-bound.
        let mut wr_update_ms = 0.0_f64;
        let mut wr_render_ms = 0.0_f64;
        let mut wr_stats: Option<webrender::RendererStats> = None;
        // ANGLE 락을 기다린 시간. 이 락은 타일 렌더 전체를 감싸므로, 타일이 느린 것이
        // 제 일 때문인지 남을 기다린 것인지는 이 값으로만 갈린다.
        //
        // 2026-09-01 이후 락은 ANGLE D3D11 디바이스당 하나다(`paint_api::angle_gl_lock`).
        // 전역이던 시절 이 값은 WebGL 스레드와의 경합까지 포함했지만, 이제 같은 디바이스를
        // 쓰는 상대하고만 부딪친다 — 그래서 이 수치가 여전히 크다면 그건 GPU 를 공유하는
        // 다른 타일이지 WebGL 이 아니다.
        let angle_lock_ms;
        {
            let lock_start = Instant::now();
            let _angle_gl_guard =
                paint_api::angle_gl_lock(self.rendering_context.angle_d3d11_device_ptr());
            angle_lock_ms = lock_start.elapsed().as_secs_f64() * 1000.0;

            if let Err(error) = self.rendering_context.make_current() {
                error!("Failed to make the rendering context current: {error:?}");
            }
            self.assert_no_gl_error();

            self.rendering_context.prepare_for_rendering();

            time_profile!(
                ProfilerCategory::Painting,
                None,
                time_profiler_channel.clone(),
                || {
                    if let Some(renderer) = self.webrender_renderer.as_mut() {
                        let update_start = Instant::now();
                        renderer.update();
                        wr_update_ms = update_start.elapsed().as_secs_f64() * 1000.0;
                    }

                    // Paint the scene.
                    // TODO(gw): Take notice of any errors the renderer returns!
                    self.clear_background();
                    if let Some(renderer) = self.webrender_renderer.as_mut() {
                        let size = self.rendering_context.size2d().to_i32();
                        let draw_start = Instant::now();
                        wr_stats = renderer
                            .render(size, 0 /* buffer_age */)
                            .ok()
                            .map(|results| results.stats);
                        wr_render_ms = draw_start.elapsed().as_secs_f64() * 1000.0;
                    }

                    // The native compositor's `bind` leaves an EGL pbuffer current. Restore the
                    // window surface as current so the next GL user (clear_background, egui
                    // overlay, screenshot readback) targets the window, not a stale tile pbuffer.
                    #[cfg(windows)]
                    if self.dcomp_native_active {
                        if let Err(error) = self.rendering_context.make_current() {
                            log::warn!("[dcomp-native] restore make_current failed: {error:?}");
                        }
                    }
                }
            );
        }

        // We've painted the default target, which means that from the embedder's perspective,
        // the scene no longer needs to be repainted.
        self.needs_repaint.set(RepaintReason::empty());

        self.screenshot_taker.maybe_take_screenshots(self);
        self.send_pending_paint_metrics_messages_after_composite();

        let render_ms = render_start.elapsed().as_secs_f64() * 1000.0;
        // This render pass consumed every frame published so far (renderer.update() above
        // drained the whole publish queue), so the in-flight display composite is done.
        self.set_display_composite_in_flight(false);
        if *LOG_PRESENT_CADENCE {
            self.last_render_end.set(Some(Instant::now()));
        }
        // Diagnostic: surface frames that blew the ~16.7ms vsync budget, with the WebRender
        // upload-vs-draw split plus renderer stats (video texture upload MB / texture cache
        // update ms / draw calls), to localize the bottleneck under heavy load (many videos).
        // A stall frame with normal upload_mb points at a driver/GPU sync or cache
        // reallocation; one with a big upload_mb spike points at an upload burst.
        if *LOG_PRESENT_CADENCE && render_ms > 16.0 {
            let (upload_mb, upload_ms, draw_calls) =
                wr_stats.as_ref().map_or((0.0, 0.0, 0), |stats| {
                    (
                        stats.texture_upload_mb,
                        stats.resource_upload_time,
                        stats.total_draw_calls,
                    )
                });
            info!(
                "Slow paint frame: painter {:?} total_ms={:.2} \
                 angle_lock_ms={:.2} wr_update_ms={:.2} wr_render_ms={:.2} \
                 upload_mb={:.1} upload_ms={:.1} draw_calls={} pending_frames={}",
                self.painter_id,
                render_ms,
                angle_lock_ms,
                wr_update_ms,
                wr_render_ms,
                upload_mb,
                upload_ms,
                draw_calls,
                self.pending_frames.get(),
            );
        }
        if self.rendering_context.requested_gpu_index().is_some() {
            info!(
                "Wall render end: painter {:?} render_count={} local_frame_id={:?} \
                 logical_frame_id={:?} render_ms={:.3} angle_lock_ms={:.3} pending={} \
                 overlapping_request_count={} unexpected_ready_count={} \
                 requested_gpu={:?} size={:?}",
                self.painter_id,
                render_count,
                local_frame_id,
                wall_logical_frame_id,
                render_ms,
                angle_lock_ms,
                self.pending_frames.get(),
                self.overlapping_frame_request_count.get(),
                self.unexpected_frame_ready_count.get(),
                self.rendering_context.requested_gpu_index(),
                self.rendering_context.size(),
            );
        } else {
            debug!(
                "Render end: painter {:?} render_count={} local_frame_id={:?} \
                 logical_frame_id={:?} render_ms={:.3} pending={}",
                self.painter_id,
                render_count,
                local_frame_id,
                wall_logical_frame_id,
                render_ms,
                self.pending_frames.get(),
            );
        }
    }

    fn clear_background(&self) {
        self.assert_gl_framebuffer_complete();

        // Always clear the entire RenderingContext, regardless of how many WebViews there are
        // or where they are positioned. This is so WebView actually clears even before the
        // first WebView is ready.
        let color = servo_config::pref!(shell_background_color_rgba);
        self.webrender_gl.clear_color(
            color[0] as f32,
            color[1] as f32,
            color[2] as f32,
            color[3] as f32,
        );
        self.webrender_gl.clear(gleam::gl::COLOR_BUFFER_BIT);
    }

    /// Send all pending paint metrics messages after a composite operation, which may advance
    /// the epoch for pipelines in the WebRender scene.
    ///
    /// If there are pending paint metrics, we check if any of the painted epochs is one
    /// of the ones that the paint metrics recorder is expecting. In that case, we get the
    /// current time, inform the constellation about it and remove the pending metric from
    /// the list.
    fn send_pending_paint_metrics_messages_after_composite(&mut self) {
        let paint_time = CrossProcessInstant::now();
        for webview_renderer in self.webview_renderers.values() {
            for (pipeline_id, pipeline) in webview_renderer.pipelines.iter() {
                let Some(current_epoch) = self
                    .webrender_renderer
                    .as_ref()
                    .and_then(|wr| wr.current_epoch(self.webrender_document, pipeline_id.into()))
                else {
                    continue;
                };

                match pipeline.first_paint_metric.get() {
                    // We need to check whether the current epoch is later, because
                    // CrossProcessPaintMessage::SendInitialTransaction sends an
                    // empty display list to WebRender which can happen before we receive
                    // the first "real" display list.
                    PaintMetricState::Seen(epoch, first_reflow) if epoch <= current_epoch => {
                        assert!(epoch <= current_epoch);
                        #[cfg(feature = "tracing")]
                        tracing::info!(
                            name: "FirstPaint",
                            servo_profiling = true,
                            epoch = ?epoch,
                            paint_time = ?paint_time,
                            pipeline_id = ?pipeline_id,
                        );

                        self.send_to_constellation(EmbedderToConstellationMessage::PaintMetric(
                            *pipeline_id,
                            PaintMetricEvent::FirstPaint(paint_time, first_reflow),
                        ));

                        pipeline.first_paint_metric.set(PaintMetricState::Sent);
                    },
                    _ => {},
                }

                match pipeline.first_contentful_paint_metric.get() {
                    PaintMetricState::Seen(epoch, first_reflow) if epoch <= current_epoch => {
                        #[cfg(feature = "tracing")]
                        tracing::info!(
                            name: "FirstContentfulPaint",
                            servo_profiling = true,
                            epoch = ?epoch,
                            paint_time = ?paint_time,
                            pipeline_id = ?pipeline_id,
                        );
                        self.send_to_constellation(EmbedderToConstellationMessage::PaintMetric(
                            *pipeline_id,
                            PaintMetricEvent::FirstContentfulPaint(paint_time, first_reflow),
                        ));
                        pipeline
                            .first_contentful_paint_metric
                            .set(PaintMetricState::Sent);
                    },
                    _ => {},
                }

                match pipeline.largest_contentful_paint_metric.get() {
                    PaintMetricState::Seen(epoch, _) if epoch <= current_epoch => {
                        if let Some(lcp) = self
                            .lcp_calculator
                            .calculate_largest_contentful_paint(paint_time, pipeline_id.into())
                        {
                            #[cfg(feature = "tracing")]
                            tracing::info!(
                                name: "LargestContentfulPaint",
                                servo_profiling = true,
                                paint_time = ?paint_time,
                                area = ?lcp.area,
                                pipeline_id = ?pipeline_id,
                            );
                            self.send_to_constellation(
                                EmbedderToConstellationMessage::PaintMetric(
                                    *pipeline_id,
                                    PaintMetricEvent::LargestContentfulPaint(
                                        lcp.paint_time,
                                        lcp.area,
                                        lcp.url.clone(),
                                    ),
                                ),
                            );
                        }
                        pipeline
                            .largest_contentful_paint_metric
                            .set(PaintMetricState::Sent);
                    },
                    _ => {},
                }
            }
        }
    }

    fn next_diagnostic_frame_id(&self) -> u64 {
        let frame_id = self.next_diagnostic_frame_id.get() + 1;
        self.next_diagnostic_frame_id.set(frame_id);
        frame_id
    }

    fn record_frame_request(
        &self,
        local_frame_id: u64,
        wall_logical_frame_id: Option<u64>,
        wall_requested_at: Option<Instant>,
        reason: String,
        pending_frames_before_request: usize,
    ) {
        let requested_at = Instant::now();
        let wall_request_delay_ms = wall_requested_at
            .map(|wall_requested_at| {
                requested_at
                    .saturating_duration_since(wall_requested_at)
                    .as_secs_f64()
                    * 1000.0
            })
            .unwrap_or_default();

        if pending_frames_before_request > 0 {
            let overlapping_frame_request_count = self.overlapping_frame_request_count.get() + 1;
            self.overlapping_frame_request_count
                .set(overlapping_frame_request_count);
            info!(
                "Wall frame overlap: painter {:?} local_frame_id={} logical_frame_id={:?} \
                 requested while {} frame(s) were still pending; overlapping_request_count={} \
                 requested_gpu={:?}",
                self.painter_id,
                local_frame_id,
                wall_logical_frame_id,
                pending_frames_before_request,
                overlapping_frame_request_count,
                self.rendering_context.requested_gpu_index(),
            );
        }

        self.pending_frame_diagnostics
            .borrow_mut()
            .push_back(PendingFrameDiagnostic {
                local_frame_id,
                wall_logical_frame_id,
                requested_at,
                wall_requested_at,
                reason: reason.clone(),
            });

        if self.rendering_context.requested_gpu_index().is_some() {
            info!(
                "Wall frame requested: painter {:?} local_frame_id={} logical_frame_id={:?} \
                 reason={} pending_before={} shared_request_delay_ms={:.3} \
                 requested_gpu={:?} size={:?}",
                self.painter_id,
                local_frame_id,
                wall_logical_frame_id,
                reason,
                pending_frames_before_request,
                wall_request_delay_ms,
                self.rendering_context.requested_gpu_index(),
                self.rendering_context.size(),
            );
        } else {
            debug!(
                "Frame requested: painter {:?} local_frame_id={} logical_frame_id={:?} reason={} \
                 pending_before={} shared_request_delay_ms={:.3} size={:?}",
                self.painter_id,
                local_frame_id,
                wall_logical_frame_id,
                reason,
                pending_frames_before_request,
                wall_request_delay_ms,
                self.rendering_context.size(),
            );
        }
    }

    /// Tally one composite request against the source line that asked for it, and dump the
    /// tally once a second.
    ///
    /// ★왜★ — 20 영상 월에서 단일 painter 가 초당 200 회 이상 합성했는데,
    /// `gfx_refresh_hz` 는 렌더러 틱(60Hz)만 페이싱하고 script 는 자체 20/30ms 타이머를
    /// 따로 돌린다. 둘을 합쳐도 상한이 ~110 이라 남은 몫이 어디서 오는지 총합만으로는
    /// 알 수 없다. `generate_frame` 호출 지점이 9 곳이므로 출처를 줄 번호로 가른다.
    ///
    /// 기본 off(`SERVO_FRAME_REASON_PROF`). 프레임마다 불리는 자리라 켜져 있을 때만
    /// 해시맵을 건드린다.
    fn note_frame_reason(&self, caller: &'static Location<'static>, reason: RenderReasons) {
        if !*FRAME_REASON_PROF {
            return;
        }
        let now = Instant::now();
        let start = match self.frame_reason_window_start.get() {
            Some(start) => start,
            None => {
                self.frame_reason_window_start.set(Some(now));
                now
            },
        };
        *self
            .frame_reason_counts
            .borrow_mut()
            .entry((caller.line(), format!("{reason:?}")))
            .or_insert(0) += 1;

        let elapsed = now.duration_since(start);
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let mut counts: Vec<((u32, String), u64)> =
            self.frame_reason_counts.borrow_mut().drain().collect();
        counts.sort_by(|a, b| b.1.cmp(&a.1));
        let total: u64 = counts.iter().map(|(_, n)| n).sum();
        let breakdown = counts
            .iter()
            .map(|((line, reason), n)| format!("painter.rs:{line}/{reason}={n}"))
            .collect::<Vec<_>>()
            .join(" ");
        info!(
            "FRAMEREASON total={total} window_ms={:.0} {breakdown}",
            elapsed.as_secs_f64() * 1000.0
        );
        self.frame_reason_window_start.set(Some(now));
    }

    /// Queue a new frame in the transaction and increase the pending frames count.
    ///
    /// `#[track_caller]`: so `note_frame_reason` can name the call site without every one of
    /// the nine callers having to pass a label.
    #[track_caller]
    pub(crate) fn generate_frame(&self, transaction: &mut Transaction, reason: RenderReasons) {
        self.generate_frame_with_diagnostic_id(transaction, reason, None, None);
    }

    /// Queue a new frame using a shared logical diagnostic id supplied by `Paint`.
    #[track_caller]
    pub(crate) fn generate_frame_with_diagnostic_id(
        &self,
        transaction: &mut Transaction,
        reason: RenderReasons,
        wall_logical_frame_id: Option<u64>,
        wall_requested_at: Option<Instant>,
    ) {
        self.note_frame_reason(Location::caller(), reason);
        // Every composite carries the newest coalesced video frames, so held updates wait at
        // most until the next generated frame (see `pending_video_frame_updates`).
        self.flush_pending_video_frame_updates(transaction);
        let reason_diagnostic = format!("{reason:?}");
        transaction.generate_frame(0, true /* present */, false /* tracked */, reason);
        let pending_frames_before_request = self.pending_frames.get();
        let local_frame_id = self.next_diagnostic_frame_id();
        self.record_frame_request(
            local_frame_id,
            wall_logical_frame_id,
            wall_requested_at,
            reason_diagnostic,
            pending_frames_before_request,
        );
        self.pending_frames.set(pending_frames_before_request + 1);
    }

    pub(crate) fn pending_frames(&self) -> usize {
        self.pending_frames.get()
    }

    /// Move all held latest-wins video frame updates into `transaction`.
    /// See `pending_video_frame_updates` for the rationale.
    fn flush_pending_video_frame_updates(&self, transaction: &mut Transaction) {
        let mut pending_updates = self.pending_video_frame_updates.borrow_mut();
        for (key, (descriptor, data)) in pending_updates.drain() {
            transaction.update_image(key, descriptor, data.into(), &DirtyRect::All);
        }
    }

    pub(crate) fn wall_scroll_offsets_signature(&self, webview_id: WebViewId) -> Option<String> {
        let webview_renderer = self.webview_renderers.get(&webview_id)?;
        let mut pipeline_signatures = Vec::new();
        for (pipeline_id, details) in &webview_renderer.pipelines {
            let mut scroll_offsets = details
                .scroll_tree
                .scroll_offsets()
                .into_iter()
                .map(|(external_id, offset)| {
                    format!("{external_id:?}:{:.3},{:.3}", offset.x, offset.y)
                })
                .collect::<Vec<_>>();
            scroll_offsets.sort();
            pipeline_signatures.push(format!("{pipeline_id:?}[{}]", scroll_offsets.join(";")));
        }
        pipeline_signatures.sort();
        Some(pipeline_signatures.join("|"))
    }

    pub(crate) fn hit_test_at_point_with_api_and_document(
        webrender_api: &RenderApi,
        webrender_document: DocumentId,
        point: DevicePoint,
    ) -> Vec<PaintHitTestResult> {
        // DevicePoint and WorldPoint are the same for us.
        let world_point = WorldPoint::from_untyped(point.to_untyped());
        let results = webrender_api.hit_test(webrender_document, world_point);

        results
            .items
            .iter()
            .map(|item| {
                let pipeline_id = item.pipeline.into();
                let external_scroll_id = ExternalScrollId(item.tag.0, item.pipeline);
                PaintHitTestResult {
                    pipeline_id,
                    point_in_viewport: Point2D::from_untyped(item.point_in_viewport.to_untyped()),
                    external_scroll_id,
                }
            })
            .collect()
    }

    pub(crate) fn send_transaction(&mut self, transaction: Transaction) {
        let _ = self.rendering_context.make_current();
        self.webrender_api
            .send_transaction(self.webrender_document, transaction);
    }

    /// Set the root pipeline for our WebRender scene to a display list that consists of an iframe
    /// for each visible top-level browsing context, applying a transformation on the root for
    /// pinch zoom, page zoom, and HiDPI scaling.
    fn send_root_pipeline_display_list_in_transaction(&self, transaction: &mut Transaction) {
        // Every display list needs a pipeline, but we'd like to choose one that is unlikely
        // to conflict with our content pipelines, which start at (1, 1). (0, 0) is WebRender's
        // dummy pipeline, so we choose (0, 1).
        let root_pipeline = WebRenderPipelineId(0, 1);
        transaction.set_root_pipeline(root_pipeline);

        let mut builder = webrender_api::DisplayListBuilder::new(root_pipeline);
        builder.begin();

        let root_reference_frame = SpatialId::root_reference_frame(root_pipeline);

        let viewport_size = self.rendering_context.size2d().to_f32().to_untyped();
        let viewport_rect = LayoutRect::from_origin_and_size(
            LayoutPoint::zero(),
            LayoutSize::from_untyped(viewport_size),
        );

        let root_clip_id = builder.define_clip_rect(root_reference_frame, viewport_rect);
        let clip_chain_id = builder.define_clip_chain(None, [root_clip_id]);

        // ★Paint order must be deterministic★ `webview_renderers` is an `FxHashMap`, so
        // `.values()` yields an arbitrary, hash-dependent order — and that order *is* the
        // paint order, because later display-list items draw over earlier ones.
        //
        // With one `WebView` per painter this never mattered, and with servoshell's tabs it
        // is masked because every inactive tab is hidden. It bites as soon as two are visible
        // at once: a full-viewport opaque `WebView` that happens to be iterated last covers
        // everything pushed before it, so the other one loads, animates and shows up in
        // devtools while painting nothing on screen.
        //
        // Order by `WebViewId`, which is allocated in creation order, so a `WebView` added
        // later paints on top — the same rule window stacking uses.
        let mut ordered_renderers: Vec<&WebViewRenderer> =
            self.webview_renderers.values().collect();
        ordered_renderers.sort_by_key(|renderer| renderer.id);

        for webview_renderer in ordered_renderers {
            if webview_renderer.hidden() {
                continue;
            }
            let Some(pipeline_id) = webview_renderer.root_pipeline_id else {
                continue;
            };

            let pinch_zoom_transform = webview_renderer.pinch_zoom().transform().to_untyped();
            let device_pixels_per_page_pixel_not_including_pinch_zoom = webview_renderer
                .device_pixels_per_page_pixel_not_including_pinch_zoom()
                .get();

            let transform = LayoutTransform::scale(
                device_pixels_per_page_pixel_not_including_pinch_zoom,
                device_pixels_per_page_pixel_not_including_pinch_zoom,
                1.0,
            )
            .then(&LayoutTransform::from_untyped(
                &pinch_zoom_transform.to_3d(),
            ));

            let viewport_origin = webview_renderer.viewport_origin();
            let webview_reference_frame = builder.push_reference_frame(
                LayoutPoint::new(-viewport_origin.x, -viewport_origin.y),
                root_reference_frame,
                TransformStyle::Flat,
                PropertyBinding::Value(transform),
                ReferenceFrameKind::Transform {
                    is_2d_scale_translation: true,
                    should_snap: true,
                    paired_with_perspective: false,
                },
                webview_renderer.id.into(),
            );

            let scaled_webview_rect = webview_renderer.rect
                / webview_renderer.device_pixels_per_page_pixel_not_including_pinch_zoom();
            builder.push_iframe(
                LayoutRect::from_untyped(&scaled_webview_rect.to_untyped()),
                LayoutRect::from_untyped(&scaled_webview_rect.to_untyped()),
                &SpaceAndClipInfo {
                    spatial_id: webview_reference_frame,
                    clip_chain_id,
                },
                pipeline_id.into(),
                true,
            );
        }

        let built_display_list = builder.end();

        // NB: We are always passing 0 as the epoch here, but this doesn't seem to
        // be an issue. WebRender will still update the scene and generate a new
        // frame even though the epoch hasn't changed.
        transaction.set_display_list(WebRenderEpoch(0), built_display_list);
        self.update_transaction_with_all_scroll_offsets(transaction);
    }

    /// Set the root pipeline for our WebRender scene to a display list that consists of an iframe
    /// for each visible top-level browsing context, applying a transformation on the root for
    /// pinch zoom, page zoom, and HiDPI scaling.
    fn send_root_pipeline_display_list(&mut self) {
        let mut transaction = Transaction::new();
        self.send_root_pipeline_display_list_in_transaction(&mut transaction);
        self.generate_frame(&mut transaction, RenderReasons::SCENE);
        self.send_transaction(transaction);
    }

    /// Update the given transaction with the scroll offsets of all active scroll nodes in
    /// the WebRender scene. This is necessary because WebRender does not preserve scroll
    /// offsets between scroll tree modifications. If a display list could potentially
    /// modify a scroll tree branch, WebRender needs to have scroll offsets for that
    /// branch.
    ///
    /// TODO(mrobinson): Could we only send offsets for the branch being modified
    /// and not the entire scene?
    fn update_transaction_with_all_scroll_offsets(&self, transaction: &mut Transaction) {
        for webview_renderer in self.webview_renderers.values() {
            for details in webview_renderer.pipelines.values() {
                for node in details.scroll_tree.nodes.iter() {
                    let (Some(offset), Some(external_id)) = (node.offset(), node.external_id())
                    else {
                        continue;
                    };
                    // Skip scroll offsets that are zero, as they are the default.
                    if offset == LayoutVector2D::zero() {
                        continue;
                    }
                    transaction.set_scroll_offsets(
                        external_id,
                        vec![SampledScrollOffset {
                            offset,
                            generation: 0,
                        }],
                    );
                }
            }
        }
    }

    fn send_zoom_and_scroll_offset_updates(
        &mut self,
        need_zoom: bool,
        scroll_offset_updates: Vec<ScrollResult>,
    ) {
        if !need_zoom && scroll_offset_updates.is_empty() {
            return;
        }

        let mut transaction = Transaction::new();
        if need_zoom {
            self.send_root_pipeline_display_list_in_transaction(&mut transaction);
        }
        for update in scroll_offset_updates {
            transaction.set_scroll_offsets(
                update.external_scroll_id,
                vec![SampledScrollOffset {
                    offset: update.offset,
                    generation: 0,
                }],
            );
        }

        self.generate_frame(&mut transaction, RenderReasons::APZ);
        self.send_transaction(transaction);
    }

    pub(crate) fn toggle_webrender_debug(&mut self, option: WebRenderDebugOption) {
        let Some(renderer) = self.webrender_renderer.as_mut() else {
            return;
        };
        let mut flags = renderer.get_debug_flags();
        let flag = match option {
            WebRenderDebugOption::Profiler => {
                webrender::DebugFlags::PROFILER_DBG
                    | webrender::DebugFlags::GPU_TIME_QUERIES
                    | webrender::DebugFlags::GPU_SAMPLE_QUERIES
            },
            WebRenderDebugOption::TextureCacheDebug => webrender::DebugFlags::TEXTURE_CACHE_DBG,
            WebRenderDebugOption::RenderTargetDebug => webrender::DebugFlags::RENDER_TARGET_DBG,
        };
        flags.toggle(flag);
        renderer.set_debug_flags(flags);

        let mut txn = Transaction::new();
        self.generate_frame(&mut txn, RenderReasons::TESTING);
        self.send_transaction(txn);
    }

    /// True while a display-paced composite is in flight (requested but not yet rendered);
    /// see `display_composite_in_flight`. Requesting more display composites in this state
    /// makes things strictly worse: every publish queued behind the renderer whose resource
    /// updates touch the texture cache `must_be_drawn`, so `Renderer::update()` fully
    /// renders it offscreen (~60-100ms each with a 45-video grid), amplifying a small
    /// hiccup into multi-second stalls. Skipped requests lose nothing: the next composite
    /// carries the newest coalesced video frames. In the healthy steady state requests and
    /// renders alternate 1:1 on this thread, so this gate never throttles.
    /// Whether a video arrival may ask for a composite yet.
    ///
    /// ***Video arrivals drive composites, but at the paint-timer cadence, not one per
    /// arrival.*** Those are two different things and the code used to offer only the
    /// extremes, both of which are wrong:
    ///
    /// * One composite per arrival: the request rate is the SUM of every video's frame rate
    ///   (36 x FHD30 = 1080/s aimed at a 60Hz wall). Requests land while the renderer is
    ///   still draining, and each queued publish marks the texture cache `must_be_drawn` so
    ///   `Renderer::update()` re-renders it offscreen -- see `renderer_behind`. Measured
    ///   2026-08-31 on the 4-GPU wall: 39.19 cores vs 22.72 with the path off, and on a
    ///   heterogeneous page the primary tile's render was 17.42ms vs 6.02ms.
    /// * No video-driven composites at all: then something ELSE has to drive them, and on a
    ///   page whose only motion is video there is nothing. Measured on the same wall with a
    ///   page holding one 30fps stream and stills: composites settled at **28/s, below the
    ///   content's own 30fps**, so frames were built and never shown. It looked fine only
    ///   while a CSS animation happened to be running (86/s), and collapsed when it ended.
    ///   Adding a 60fps video "fixed" it by accident -- that video became the clock.
    ///
    /// Coalescing gives both: a floor at the paint cadence so video-only pages keep
    /// updating, and a ceiling so a high aggregate arrival rate cannot amplify.
    ///
    /// `gfx_refresh_hz` is the period because it is exactly this: the free-running paint
    /// timer whose job is to keep production near the display rate.
    fn video_composite_due(&self) -> bool {
        let interval = crate::refresh_driver::paint_timer_period();
        self.last_video_driven_frame_at
            .get()
            .is_none_or(|last| last.elapsed() >= interval)
    }

    fn renderer_behind(&self) -> bool {
        if !self.display_composite_in_flight.get() {
            return false;
        }
        // Force-release rather than stay shut forever. See the constant's doc comment: with
        // no escape here, a single lost redraw takes the tile out for the rest of the run.
        //
        // Checking on the DECLINE path is what makes this self-healing: a stuck painter
        // sends no transactions, so it gets no frame-ready and no render -- but the wall
        // keeps asking it for frames, so this runs. Recovery costs at most one timeout.
        let stuck_for = self
            .display_composite_in_flight_since
            .get()
            .map(|since| since.elapsed());
        if stuck_for.is_none_or(|elapsed| elapsed < DISPLAY_COMPOSITE_IN_FLIGHT_TIMEOUT) {
            return true;
        }
        warn!(
            "Painter {:?}: display composite stuck in flight for {:.1}ms with no render pass; \
             force-releasing. The tile would otherwise stop rendering permanently. \
             A lost redraw for this painter is the known cause.",
            self.painter_id,
            stuck_for
                .map(|e| e.as_secs_f64() * 1000.0)
                .unwrap_or_default(),
        );
        self.set_display_composite_in_flight(false);
        false
    }

    /// Single writer for `display_composite_in_flight` and its timestamp, so the two cannot
    /// disagree. The timestamp is only refreshed on a false->true edge: re-arming it while
    /// already set would push the deadline out forever and defeat the whole guard.
    fn set_display_composite_in_flight(&self, in_flight: bool) {
        if !in_flight {
            self.display_composite_in_flight.set(false);
            self.display_composite_in_flight_since.set(None);
            return;
        }
        if !self.display_composite_in_flight.get() {
            self.display_composite_in_flight_since
                .set(Some(Instant::now()));
        }
        self.display_composite_in_flight.set(true);
    }

    /// DComp Native 경로에서 런타임 리사이즈 후 picture-cache를 물리적으로 재구축한다.
    /// 프레임 준비마다 호출되어 마지막 크기 변경 이후 안정 프레임 수를 세고, 임계값에 도달하면
    /// SetPictureTileSize를 기동 시 정상상태(steady)로 되돌려 보낸다. 드래그 시작에서 이미
    /// alternate로 보내둔 상태이므로(resize_rendering_context 참조) steady로의 이 전환은
    /// desired != current를 다시 만들어(steady != alternate가 되도록 계산해 두었으므로 항상
    /// 참 — 아래 fire_dcomp_tile_rebuild_settle 문서 참조) WR이 모든 picture-cache 슬라이스의
    /// 네이티브 서피스를 destroy_surface하고 타일을 비운 뒤 재생성하게 만들어(webrender-0.68
    /// picture.rs:2297-2332 → resource_cache::destroy_compositor_surface → renderer/mod.rs:5163
    /// compositor.destroy_surface), 리사이즈 이전 가상 서피스에 남은 옛 픽셀(잔상)을 물리적으로
    /// 소멸시킨다. task-10b가 증명한 FORCE_PICTURE_INVALIDATION의 한계(재계산만 강제, vacated
    /// 영역 미재도색)를 이 방식이 우회한다.
    ///
    /// 옛 픽셀이 이 정착 재구축까지(~170ms, DCOMP_RESIZE_DEBOUNCE_FRAMES@60fps) 화면에 남는
    /// 것은 의도된 트레이드오프다(매 프레임 재구축은 리사이즈 중 지속적인 서피스 파괴/재생성
    /// 비용이 너무 크다).
    #[cfg(windows)]
    fn tick_dcomp_resize_rebuild(&self) {
        if !self.dcomp_native_active ||
            !self.dcomp_resize_pending.get() ||
            *DCOMP_RESIZE_REBUILD_DISABLED
        {
            return;
        }

        let stable = self.dcomp_resize_stable_frames.get() + 1;
        if stable < DCOMP_RESIZE_DEBOUNCE_FRAMES {
            self.dcomp_resize_stable_frames.set(stable);
            // 디바운스 창이 끝날 때까지 프레임이 계속 흐르도록 재도색을 요청한다(애니메이션이
            // 없는 페이지에서도 카운터가 확정적으로 임계값에 도달하게 보장).
            self.set_needs_repaint(RepaintReason::Resize);
            return;
        }

        // 크기가 멎었다(정착). task-12b (C): 리사이즈 활성 신호를 해제해 컴포지터의 승격
        // 억제를 풀고, 강등됐던 서피스가 MIN_AGE/streak 규칙으로 자연 재승격되게 한다
        // (리사이즈 강등은 지수 쿨다운을 적용하지 않으므로 수 초 지연 없음 — dcomp_compositor
        // 강등 루프 참조). 그런 다음 Task 12 재구축(steady로 수렴, alternate→steady)으로
        // 잔상을 소멸.
        self.rendering_context.set_dcomp_resize_active(false);
        let next = self.fire_dcomp_tile_rebuild_settle();
        self.dcomp_resize_pending.set(false);
        self.dcomp_resize_stable_frames.set(0);
        // 드문 이벤트(정착 리사이즈당 1회)라 info로 남겨 런처 기본 RUST_LOG(paint=info)에서
        // 보이게 한다 — 매 프레임 debug 스팸과 달리 로그 부하가 없다.
        match next {
            Some(size) => info!(
                "[dcomp-native] runtime resize settled after {} stable frames; forcing \
                 picture-cache rebuild via SetPictureTileSize={}x{} (converges to startup \
                 steady state; destroys/recreates native surfaces to purge stale \
                 virtual-surface ghosts; task-10b: FORCE_PICTURE_INVALIDATION could not)",
                DCOMP_RESIZE_DEBOUNCE_FRAMES, size.width, size.height,
            ),
            None => info!(
                "[dcomp-native] runtime resize settled after {} stable frames; forcing \
                 picture-cache rebuild via SetPictureTileSize=None (converges to startup \
                 steady state = WR default tile size, restores scrollbar special sizing; \
                 destroys/recreates native surfaces to purge stale virtual-surface ghosts; \
                 task-10b: FORCE_PICTURE_INVALIDATION could not)",
                DCOMP_RESIZE_DEBOUNCE_FRAMES,
            ),
        }
    }

    /// task-12b: 드래그 "시작" 재구축 1회 발동 — 항상 alternate 타일 크기를 보낸다. 정상상태는
    /// 항상 steady이므로(정착 시 fire_dcomp_tile_rebuild_settle이 되돌린다) desired(alternate)
    /// != current(steady)가 보장되어 picture.rs:2320의 파괴/재생성 조건이 반드시 참이 된다.
    /// 이전의 primary↔alternate "교대" 방식은 시작/정착 호출이 짝을 이루지 못하면(예: 12b
    /// 전용 킬 스위치로 시작 발동만 스킵된 경우) 정상상태가 하드코딩된 대체값 512x512에
    /// 눌러앉을 수 있었다(리뷰 지적) — 이 함수는 그 회귀를 구조적으로 없앤다: 시작은 항상
    /// alternate, 정착은 항상 steady로 결정론적으로 수렴한다. 같은 이유로 이 시작-발동은
    /// 마스터 스위치에만 걸려 RED(RESIZE_VIRTUAL disabled) 모드에서도 발동한다 — 정착
    /// 재구축이 current(=alternate) != steady 전이를 전제하기 때문(호출부 게이트 주석 참조).
    #[cfg(windows)]
    fn fire_dcomp_tile_rebuild_start(&self) -> webrender_api::units::DeviceIntSize {
        let next = self.dcomp_tile_size_alternate;
        self.webrender_api
            .send_debug_cmd(webrender::DebugCommand::SetPictureTileSize(Some(next)));
        next
    }

    /// task-12b: "정착" 재구축 1회 발동 — 항상 기동 시 정상상태(steady)로 되돌린다. steady가
    /// None이면 SetPictureTileSize(None)을 보내 WR 기본 분기(콘텐츠는 TILE_SIZE_DEFAULT
    /// 1024x512, 스크롤바는 TILE_SIZE_SCROLLBAR_*)를 복원한다 — 이는 override를
    /// `Some(alternate)`에 영구 고정해두던 이전 구현이 깨뜨리던 WR 스크롤바 특수 타일 크기를
    /// 되살린다(리뷰 지적). 재구축이 실제로 발동함은 render_backend.rs:1139-1141
    /// (SetPictureTileSize → frame_config.tile_size_override 갱신)과 picture.rs:2297-2332
    /// (override 변경 시 즉시 재평가 → desired_tile_size != current_tile_size 이면 서피스
    /// destroy/재생성)로 확인된다: 시작 발동 직후 current_tile_size == alternate이고, 이 함수가
    /// override를 steady(대개 alternate와 다른 값, None 포함)로 바꾸므로 desired != current가
    /// 성립해 반드시 재구축이 실행된다. 보낸 크기를 반환(호출자 로깅용, None이면 WR 기본).
    #[cfg(windows)]
    fn fire_dcomp_tile_rebuild_settle(&self) -> Option<webrender_api::units::DeviceIntSize> {
        let steady = self.dcomp_tile_size_steady;
        self.webrender_api
            .send_debug_cmd(webrender::DebugCommand::SetPictureTileSize(steady));
        steady
    }

    pub(crate) fn note_webrender_frame_ready(
        &self,
        need_repaint: bool,
    ) -> Option<FrameReadyDiagnostic> {
        // 프레임 준비마다 리사이즈 재구축 디바운스를 진행한다(DComp Native 경로 전용).
        #[cfg(windows)]
        self.tick_dcomp_resize_rebuild();

        if !need_repaint {
            // No render pass will follow this frame-ready, so an in-flight display
            // composite (if any) must be considered consumed here.
            self.set_display_composite_in_flight(false);
        }
        let pending_frames = self.pending_frames.get();
        if pending_frames == 0 {
            let unexpected_frame_ready_count = self.unexpected_frame_ready_count.get() + 1;
            self.unexpected_frame_ready_count
                .set(unexpected_frame_ready_count);
            warn!(
                "Wall frame diagnostic: painter {:?} received a WebRender frame-ready \
                 notification with no pending frame; need_repaint={} unexpected_ready_count={} \
                 requested_gpu={:?}",
                self.painter_id,
                need_repaint,
                unexpected_frame_ready_count,
                self.rendering_context.requested_gpu_index(),
            );
            return None;
        }

        self.pending_frames.set(pending_frames - 1);

        // Diagnostic: accumulate the actual frame-ready cadence and log once per second.
        if *LOG_PRESENT_CADENCE {
            let now = Instant::now();
            if let Some(last) = self.present_cadence_last.get() {
                let gap_ms = now.duration_since(last).as_secs_f64() * 1000.0;
                if gap_ms > self.present_cadence_max_gap_ms.get() {
                    self.present_cadence_max_gap_ms.set(gap_ms);
                }
            }
            self.present_cadence_last.set(Some(now));
            self.present_cadence_count.set(self.present_cadence_count.get() + 1);
            let start = self.present_cadence_start.get().unwrap_or_else(|| {
                self.present_cadence_start.set(Some(now));
                now
            });
            let elapsed = now.duration_since(start).as_secs_f64();
            if elapsed >= 1.0 {
                info!(
                    "Present cadence: painter {:?} presents/s={:.1} max_gap_ms={:.2} pending={}",
                    self.painter_id,
                    self.present_cadence_count.get() as f64 / elapsed,
                    self.present_cadence_max_gap_ms.get(),
                    self.pending_frames.get(),
                );
                self.present_cadence_start.set(Some(now));
                self.present_cadence_count.set(0);
                self.present_cadence_max_gap_ms.set(0.0);
            }
        }

        let frame = self.pending_frame_diagnostics.borrow_mut().pop_front();
        let Some(frame) = frame else {
            warn!(
                "Wall frame diagnostic: painter {:?} marked a frame ready but had no queued \
                 diagnostic metadata; pending_after={} need_repaint={} requested_gpu={:?}",
                self.painter_id,
                pending_frames - 1,
                need_repaint,
                self.rendering_context.requested_gpu_index(),
            );
            return None;
        };

        self.last_ready_local_frame_id
            .set(Some(frame.local_frame_id));
        if frame.wall_logical_frame_id.is_some() {
            self.last_ready_wall_logical_frame_id
                .set(frame.wall_logical_frame_id);
        }
        let ready_at = Instant::now();
        let wait_ms = ready_at.duration_since(frame.requested_at).as_secs_f64() * 1000.0;
        let shared_request_to_ready_ms = frame
            .wall_requested_at
            .map(|wall_requested_at| {
                ready_at
                    .saturating_duration_since(wall_requested_at)
                    .as_secs_f64()
                    * 1000.0
            })
            .unwrap_or(wait_ms);
        if self.rendering_context.requested_gpu_index().is_some() {
            info!(
                "Wall frame ready: painter {:?} local_frame_id={} logical_frame_id={:?} \
                 reason={} wait_ms={:.3} shared_request_to_ready_ms={:.3} \
                 pending_after={} need_repaint={} \
                 overlapping_request_count={} unexpected_ready_count={} requested_gpu={:?}",
                self.painter_id,
                frame.local_frame_id,
                frame.wall_logical_frame_id,
                frame.reason,
                wait_ms,
                shared_request_to_ready_ms,
                pending_frames - 1,
                need_repaint,
                self.overlapping_frame_request_count.get(),
                self.unexpected_frame_ready_count.get(),
                self.rendering_context.requested_gpu_index(),
            );
        } else {
            debug!(
                "Frame ready: painter {:?} local_frame_id={} logical_frame_id={:?} reason={} \
                 wait_ms={:.3} shared_request_to_ready_ms={:.3} pending_after={} \
                 need_repaint={}",
                self.painter_id,
                frame.local_frame_id,
                frame.wall_logical_frame_id,
                frame.reason,
                wait_ms,
                shared_request_to_ready_ms,
                pending_frames - 1,
                need_repaint,
            );
        }

        Some(FrameReadyDiagnostic {
            painter_id: self.painter_id,
            local_frame_id: frame.local_frame_id,
            wall_logical_frame_id: frame.wall_logical_frame_id,
            ready_at,
            wait_ms,
            need_repaint,
        })
    }

    pub(crate) fn report_memory(&self) -> MemoryReport {
        self.webrender_api
            .report_memory(MallocSizeOfOps::new(servo_allocator::usable_size, None))
    }

    pub(crate) fn change_running_animations_state(
        &mut self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        animation_state: embedder_traits::AnimationState,
    ) {
        let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) else {
            return;
        };
        if !webview_renderer.change_pipeline_running_animations_state(pipeline_id, animation_state)
        {
            return;
        }
        if !self
            .animation_refresh_driver_observer
            .notify_animation_state_changed(webview_renderer)
        {
            return;
        }

        self.refresh_driver
            .add_observer(self.animation_refresh_driver_observer.clone());
    }

    pub(crate) fn set_frame_tree_for_webview(&mut self, frame_tree: &SendableFrameTree) {
        debug!("{}: Setting frame tree for webview", frame_tree.pipeline.id);

        let webview_id = frame_tree.pipeline.webview_id;
        let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) else {
            warn!(
                "Attempted to set frame tree on unknown WebView (perhaps closed?): {webview_id:?}"
            );
            return;
        };

        webview_renderer.set_frame_tree(frame_tree);
        self.send_root_pipeline_display_list();
    }

    pub(crate) fn set_throttled(
        &mut self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        throttled: bool,
    ) {
        let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) else {
            return;
        };
        if !webview_renderer.set_throttled(pipeline_id, throttled) {
            return;
        }

        if self
            .animation_refresh_driver_observer
            .notify_animation_state_changed(webview_renderer)
        {
            self.refresh_driver
                .add_observer(self.animation_refresh_driver_observer.clone());
        }
    }

    pub(crate) fn notify_pipeline_exited(
        &mut self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        pipeline_exit_source: PipelineExitSource,
    ) {
        debug!("Paint got pipeline exited: {webview_id:?} {pipeline_id:?}",);
        if let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) {
            webview_renderer.pipeline_exited(pipeline_id, pipeline_exit_source);
        }
        self.lcp_calculator
            .remove_lcp_candidates_for_pipeline(&pipeline_id.into());
    }

    pub(crate) fn send_initial_pipeline_transaction(
        &mut self,
        webview_id: WebViewId,
        pipeline_id: WebRenderPipelineId,
    ) {
        let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) else {
            return warn!("Could not find WebView for incoming display list");
        };

        let starting_epoch = Epoch(0);
        let details = webview_renderer.ensure_pipeline_details(pipeline_id.into());
        details.display_list_epoch = Some(starting_epoch);

        let mut txn = Transaction::new();
        txn.set_display_list(starting_epoch.into(), (pipeline_id, Default::default()));

        self.generate_frame(&mut txn, RenderReasons::SCENE);
        self.send_transaction(txn);
    }

    pub(crate) fn scroll_node_by_delta(
        &mut self,
        webview_id: WebViewId,
        pipeline_id: WebRenderPipelineId,
        offset: LayoutVector2D,
        external_scroll_id: webrender_api::ExternalScrollId,
    ) {
        let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) else {
            return;
        };

        let pipeline_id = pipeline_id.into();
        let Some(pipeline_details) = webview_renderer.pipelines.get_mut(&pipeline_id) else {
            return;
        };

        let Some(offset) = pipeline_details
            .scroll_tree
            .set_scroll_offset_for_node_with_external_scroll_id(
                external_scroll_id,
                offset,
                ScrollType::Script,
            )
        else {
            // The renderer should be fully up-to-date with script at this point and script
            // should never try to scroll to an invalid location.
            warn!("Could not scroll node with id: {external_scroll_id:?}");
            return;
        };

        let mut transaction = Transaction::new();
        transaction.set_scroll_offsets(
            external_scroll_id,
            vec![SampledScrollOffset {
                offset,
                generation: 0,
            }],
        );

        self.generate_frame(&mut transaction, RenderReasons::APZ);
        self.send_transaction(transaction);
    }

    pub(crate) fn scroll_viewport_by_delta(
        &mut self,
        webview_id: WebViewId,
        delta: LayoutVector2D,
    ) {
        let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) else {
            return;
        };
        let (pinch_zoom_result, scroll_results) = webview_renderer.scroll_viewport_by_delta(delta);
        self.send_zoom_and_scroll_offset_updates(
            pinch_zoom_result == PinchZoomResult::DidPinchZoom,
            scroll_results,
        );
    }

    pub(crate) fn update_epoch(
        &mut self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        epoch: Epoch,
    ) {
        let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) else {
            return warn!("Could not find WebView for Epoch update.");
        };
        webview_renderer
            .ensure_pipeline_details(pipeline_id)
            .display_list_epoch = Some(Epoch(epoch.0));
    }

    #[servo_tracing::instrument(skip_all)]
    pub(crate) fn handle_new_display_list(
        &mut self,
        webview_id: WebViewId,
        display_list_descriptor: BuiltDisplayListDescriptor,
        display_list_info: PaintDisplayListInfo,
        display_list_data: SerializableDisplayListPayload,
    ) {
        let items_data = display_list_data.items_data;
        let cache_data = display_list_data.cache_data;
        let spatial_tree = display_list_data.spatial_tree;

        let built_display_list = BuiltDisplayList::from_data(
            DisplayListPayload {
                items_data,
                cache_data,
                spatial_tree,
            },
            display_list_descriptor,
        );
        let _span = profile_traits::trace_span!("PaintMessage::SendDisplayList").entered();
        let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) else {
            return warn!("Could not find WebView for incoming display list");
        };

        let old_scale = webview_renderer.device_pixels_per_page_pixel();
        let pipeline_id = display_list_info.pipeline_id;
        let details = webview_renderer.ensure_pipeline_details(pipeline_id.into());

        details.install_new_scroll_tree(display_list_info.scroll_tree);
        details.viewport_scale = Some(display_list_info.viewport_details.hidpi_scale_factor);

        let epoch = display_list_info.epoch.into();
        let first_reflow = display_list_info.first_reflow;
        if details.first_paint_metric.get() == PaintMetricState::Waiting
            && display_list_info.is_paintable
        {
            details
                .first_paint_metric
                .set(PaintMetricState::Seen(epoch, first_reflow));
        }

        if details.first_contentful_paint_metric.get() == PaintMetricState::Waiting
            && display_list_info.is_paintable
            && display_list_info.is_contentful
        {
            details
                .first_contentful_paint_metric
                .set(PaintMetricState::Seen(epoch, first_reflow));
        }

        details.animations.handle_new_display_list(
            display_list_info.caret_property_binding,
            &self.web_content_animator,
        );

        let mut transaction = Transaction::new();
        let is_root_pipeline = Some(pipeline_id.into()) == webview_renderer.root_pipeline_id;
        if is_root_pipeline && old_scale != webview_renderer.device_pixels_per_page_pixel() {
            self.send_root_pipeline_display_list_in_transaction(&mut transaction);
        }

        transaction.set_display_list(epoch, (pipeline_id, built_display_list));

        self.update_transaction_with_all_scroll_offsets(&mut transaction);
        self.send_transaction(transaction);
    }

    pub(crate) fn generate_frame_for_script(
        &mut self,
        diagnostic_frame_id: u64,
        wall_requested_at: Instant,
    ) -> bool {
        self.frame_delayer.set_pending_frame(true);

        if !self.frame_delayer.needs_new_frame() {
            return false;
        }

        // Skip requesting this composite while the renderer is still draining previously
        // published frames (see `renderer_behind`). The pending-frame flag stays set, so a
        // subsequent `update_images` call regenerates once the renderer has caught up.
        if self.renderer_behind() {
            return false;
        }

        // 실패 주입(기본 off). 게이트가 닫혔던 것과 같은 자리에서 같은 상태로 빠진다.
        if self.consume_canvas_ack_skip() {
            return false;
        }

        let mut transaction = Transaction::new();
        self.generate_frame_with_diagnostic_id(
            &mut transaction,
            RenderReasons::SCENE,
            Some(diagnostic_frame_id),
            Some(wall_requested_at),
        );
        self.set_display_composite_in_flight(true);
        self.send_transaction(transaction);

        let waiting_pipelines = self.frame_delayer.take_waiting_pipelines();

        self.send_to_constellation(
            EmbedderToConstellationMessage::NoLongerWaitingOnAsynchronousImageUpdates(
                waiting_pipelines,
            ),
        );

        self.frame_delayer.set_pending_frame(false);
        self.screenshot_taker
            .prepare_screenshot_requests_for_render(self);
        true
    }

    fn serializable_image_data_to_image_data_maybe_caching(
        &mut self,
        key: ImageKey,
        data: SerializableImageData,
        is_animated_image: bool,
    ) -> ImageData {
        match data {
            SerializableImageData::Raw(shared_memory) => {
                let data = shared_memory.into_arc_vec();
                if is_animated_image {
                    self.animation_image_cache.insert(key, Arc::clone(&data));
                }
                ImageData::Raw(data)
            },
            SerializableImageData::External(image) => ImageData::External(image),
        }
    }

    pub(crate) fn update_images(&mut self, updates: SmallVec<[ImageUpdate; 1]>) {
        let mut txn = Transaction::new();
        // Task 3: track content image updates that arrive WITHOUT a canvas epoch (notably video
        // frames). These are not paced by the script rendering-opportunity, so they otherwise wait
        // for the next GenerateFrame (~48fps) before being composited.
        let mut immediate_image_update = false;
        for update in updates {
            match update {
                ImageUpdate::AddImage(key, description, data, is_animated_image) => {
                    txn.add_image(
                        key,
                        description,
                        self.serializable_image_data_to_image_data_maybe_caching(
                            key,
                            data,
                            is_animated_image,
                        ),
                        None,
                    );
                },
                ImageUpdate::DeleteImage(key) => {
                    txn.delete_image(key);
                    self.frame_delayer.delete_image(key);
                    self.animation_image_cache.remove(&key);
                    // A held (not yet flushed) update for a deleted key must never reach
                    // WebRender after the delete.
                    self.pending_video_frame_updates.borrow_mut().remove(&key);
                },
                ImageUpdate::UpdateImage(key, desc, data, epoch) => {
                    if let Some(epoch) = epoch {
                        self.frame_delayer.update_image(key, epoch);
                        txn.update_image(key, desc, data.into(), &DirtyRect::All);
                    } else {
                        // Latest wins: overwrite any not-yet-composited frame for this key.
                        // The stash is flushed into the next composite by
                        // `generate_frame_with_diagnostic_id`, so stale video frames are
                        // dropped here instead of piling up in WebRender's queues (which
                        // cannot skip them; see `pending_video_frame_updates`).
                        immediate_image_update = true;
                        self.pending_video_frame_updates
                            .borrow_mut()
                            .insert(key, (desc, data));
                    }
                },
                ImageUpdate::UpdateImageForAnimation(image_key, desc) => {
                    let Some(image) = self.animation_image_cache.get(&image_key) else {
                        error!("Could not find image key in image cache.");
                        continue;
                    };
                    txn.update_image(
                        image_key,
                        desc,
                        ImageData::new_shared(image.clone()),
                        &DirtyRect::All,
                    );
                },
            }
        }

        let mut generated_frame = false;
        // `renderer_behind`: while the renderer is draining published frames, defer this
        // composite; the pending-frame flag stays set and one of the frequent subsequent
        // `update_images` calls regenerates once the renderer has caught up.
        if self.frame_delayer.needs_new_frame() &&
            !self.renderer_behind() &&
            // 실패 주입(기본 off). 여기가 실측된 교착의 2 단계 — 이미지는 도착했는데 그 순간
            // 합성이 in-flight 라 ack 없이 지나간 자리다.
            !self.consume_canvas_ack_skip()
        {
            self.frame_delayer.set_pending_frame(false);
            self.generate_frame(&mut txn, RenderReasons::SCENE);
            self.set_display_composite_in_flight(true);
            generated_frame = true;
            let waiting_pipelines = self.frame_delayer.take_waiting_pipelines();

            self.send_to_constellation(
                EmbedderToConstellationMessage::NoLongerWaitingOnAsynchronousImageUpdates(
                    waiting_pipelines,
                ),
            );

            self.screenshot_taker
                .prepare_screenshot_requests_for_render(&*self);
        }

        // Present content image updates (notably video frames) at their arrival rate by
        // re-compositing the current scene now, instead of waiting for the script's
        // rendering-opportunity GenerateFrame (which paces well below the video frame rate).
        // generate_frame re-renders the full current display list, so all other DOM composites
        // together with the updated image (z-order/clip preserved). Only when no frame is already
        // in flight, so we don't stack composites or double up with the script's own GenerateFrame
        // (which also increments pending_frames); this self-limits to roughly the present rate.
        // Only push a per-arrival immediate composite when no requestAnimationFrame loop is
        // already driving a steady composite cadence for this painter. When rAF is active (an
        // overlay, a three.js/WebGL render loop, ...) video image updates ride that regular
        // cadence instead. Otherwise two composite sources (rAF-driven and video-arrival-driven)
        // race through the `pending_frames` gate at irregular phase, making the rAF/compositor
        // cadence jitter — worsening as the number of simultaneous videos grows (36 tiles =>
        // ~1080 arrivals/s). With no rAF (a pure <video> page, e.g. a single 4K wall video) we
        // still composite per arrival so it presents at full frame rate rather than the slower
        // script rendering-opportunity rate. `animation_callbacks_running` tracks rAF only, so a
        // plain playing <video> (which sets `animations_running`) does not suppress this path.
        let raf_driving_composites = self.animation_callbacks_running();

        // external 갱신 분리(Task 1-4, 설계 §4): 승격된 external 비디오만 도착했다면
        // WR 프레임 빌드(generate_frame, 씬 트리 재구성)를 완전히 건너뛰고 DComp 레벨의
        // 값싼 present만 낸다. `took_fast_path`는 아래 기존 generate_frame 분기가 이
        // 프레임을 이중으로 처리하지 않도록 하는 스위치이며, non-Windows 빌드/기능
        // off/승격 external 없음에서는 항상 false로 남아 기존 동작이 그대로 유지된다.
        //
        // 주의(설계 필수 준수): 이 fast-path는 여기(update_images의 즉시-합성 게이트)에서만
        // 호출한다. WR `render()` 안에서는 절대 부르지 않는다 — WR이 `SharedDComp`
        // (동일 `Rc<RefCell<DCompNativeCompositor>>`의 트레이트 위임 래퍼)를 통해 이미
        // 컴포지터를 대여 중일 때 여기서도 `dcomp_shared.borrow_mut()`를 걸면 이중 대여로
        // 패닉한다. `update_images`는 WR `render()` 호출 경로 밖이므로 겹치지 않는다.
        // non-Windows에서는 이 아래 `#[cfg(windows)]` 블록 전체가 통째로 잘려나가
        // `took_fast_path`가 재대입되지 않으므로, `mut` 여부도 cfg로 나눠 unused_mut
        // 경고를 피한다(플랫폼별 `mut` 필요성이 다름 — 동작은 항상 false로 동일).
        #[cfg(windows)]
        let mut took_fast_path = false;
        #[cfg(not(windows))]
        let took_fast_path = false;
        #[cfg(windows)]
        {
            let escaped_count = self
                .dcomp_shared
                .as_ref()
                .map(|c| crate::dcomp_compositor::SharedDComp(c.clone()).escaped_external_count())
                .unwrap_or(0);
            let resize_active = self.rendering_context.dcomp_resize_active();
            // ★pending_frames==0 / renderer_behind을 넘기지 않는다: fast-path는 WR 빌드와
            // 무관하므로 WR이 바쁠 때(절벽)에도 발동해야 한다(should_fast_present 주석).
            if crate::dcomp_compositor::should_fast_present(
                immediate_image_update,
                generated_frame,
                raf_driving_composites,
                escaped_count,
                resize_active,
                crate::dcomp_compositor::decouple_enabled(),
            ) {
                // refresh 페이싱: 직전 fast-present 후 ~14ms(≈60/s) 경과 시에만 실제
                // present를 낸다. 도착마다 present하면 36타일에서 ~1080 Commit/s가 되므로
                // coalesce한다. dedup(external_needs_present)이 프레임이 안 바뀐 비디오는
                // 이미 걸러주므로, due한 이 present가 그 시점의 최신 프레임을 반영한다.
                let now = std::time::Instant::now();
                let due = self
                    .last_external_present
                    .get()
                    .map(|t| now.duration_since(t) >= std::time::Duration::from_millis(14))
                    .unwrap_or(true);
                if due {
                    if let Some(shared) = self.dcomp_shared.as_ref() {
                        crate::dcomp_compositor::SharedDComp(shared.clone())
                            .present_external_only();
                        self.last_external_present.set(Some(now));
                    }
                }
                // 스로틀에 막혀 이번 호출에서 실제 present를 안 냈어도, 판정 자체는
                // 성립했으므로 fast-path를 택한 것으로 취급한다 — 값비싼 generate_frame을
                // 이 external-전용 프레임에는 절대 내지 않는다(다음 due 틱이 최신 프레임을
                // present한다).
                took_fast_path = true;
            }
        }

        // ***Owing a composite and being able to issue one are separate questions.*** The
        // cadence decides the first, the painter's state decides the second, and mixing them
        // into one condition is what kept the floor from working: an arrival that landed
        // while a composite was in flight was DROPPED, not deferred, so the rate came out as
        // "how often an arrival happens to coincide with an idle painter" -- measured at
        // ~20/s on a wall whose paint cadence is 60.
        if immediate_image_update
            && pref!(gfx_video_immediate_composite_enabled)
            && self.video_composite_due()
        {
            self.video_composite_owed.set(true);
        }
        if self.video_composite_owed.get() &&
            !took_fast_path &&
            !generated_frame &&
            self.pending_frames.get() == 0 &&
            // rAF already drives a steady cadence for this painter; the video rides that
            // instead of adding a second source. The debt stays owed and is paid the moment
            // rAF stops.
            !raf_driving_composites &&
            !self.renderer_behind()
        {
            self.generate_frame(&mut txn, RenderReasons::SCENE);
            self.set_display_composite_in_flight(true);
            self.video_composite_owed.set(false);
            // Stamped when the composite is ISSUED, not when it was owed, so a painter that
            // was busy for a while does not immediately owe another one.
            self.last_video_driven_frame_at.set(Some(Instant::now()));
        }

        // With coalescing, a call that only stashed video frames produces an empty
        // transaction (the stash is flushed by the next composite); skip the send to avoid
        // pushing hundreds of no-op transactions per second through the scene builder.
        if !txn.is_empty() {
            self.send_transaction(txn);
        }
    }

    /// 미뤄 둔 DComp Commit 의 디바이스를 인계한다(병렬 Commit 용). 인계했으면 이 painter 는
    /// 그 프레임의 Commit 을 더 이상 책임지지 않는다.
    pub(crate) fn take_pending_dcomp_commit(&self) -> Option<usize> {
        #[cfg(windows)]
        {
            return self
                .dcomp_shared
                .as_ref()
                .and_then(|shared| shared.borrow_mut().take_pending_commit());
        }
        #[cfg(not(windows))]
        None
    }

    /// 미뤄 둔 DComp Commit 을 흘린다(`gfx_dcomp_defer_commit`). WR `render()` 밖에서만
    /// 불러야 한다 — 안에서 부르면 `dcomp_shared` 이중 대여로 패닉한다(위 fast-path 주석).
    pub(crate) fn flush_deferred_dcomp_commit(&self) {
        #[cfg(windows)]
        if let Some(shared) = self.dcomp_shared.as_ref() {
            shared.borrow_mut().flush_deferred_commit();
        }
    }

    pub(crate) fn delay_new_frames_for_canvas(
        &mut self,
        pipeline_id: PipelineId,
        canvas_epoch: Epoch,
        image_keys: Vec<ImageKey>,
    ) {
        self.frame_delayer
            .add_delay(pipeline_id, canvas_epoch, image_keys);
    }

    pub(crate) fn add_font(
        &mut self,
        font_key: FontKey,
        data: Arc<GenericSharedMemory>,
        index: u32,
    ) {
        let mut transaction = Transaction::new();
        transaction.add_raw_font(font_key, (**data).into(), index);
        self.send_transaction(transaction);
    }

    pub(crate) fn add_system_font(&mut self, font_key: FontKey, native_handle: NativeFontHandle) {
        let mut transaction = Transaction::new();
        transaction.add_native_font(font_key, native_handle);
        self.send_transaction(transaction);
    }

    pub(crate) fn add_font_instance(
        &mut self,
        instance_key: FontInstanceKey,
        font_key: FontKey,
        size: f32,
        flags: FontInstanceFlags,
        variations: Vec<FontVariation>,
    ) {
        let variations = if pref!(layout_variable_fonts_enabled) {
            variations
        } else {
            vec![]
        };

        let mut transaction = Transaction::new();

        let font_instance_options = FontInstanceOptions {
            flags,
            ..Default::default()
        };
        transaction.add_font_instance(
            instance_key,
            font_key,
            size,
            Some(font_instance_options),
            None,
            variations,
        );

        self.send_transaction(transaction);
    }

    pub(crate) fn remove_fonts(&mut self, keys: Vec<FontKey>, instance_keys: Vec<FontInstanceKey>) {
        let mut transaction = Transaction::new();

        for instance in instance_keys.into_iter() {
            transaction.delete_font_instance(instance);
        }
        for key in keys.into_iter() {
            transaction.delete_font(key);
        }

        self.send_transaction(transaction);
    }

    pub(crate) fn set_viewport_description(
        &mut self,
        webview_id: WebViewId,
        viewport_description: ViewportDescription,
    ) {
        if let Some(webview) = self.webview_renderers.get_mut(&webview_id) {
            webview.set_viewport_description(viewport_description);
        }
    }

    pub(crate) fn handle_screenshot_readiness_reply(
        &self,
        webview_id: WebViewId,
        expected_epochs: FxHashMap<PipelineId, Epoch>,
    ) {
        self.screenshot_taker
            .handle_screenshot_readiness_reply(webview_id, expected_epochs, self);
    }

    pub(crate) fn add_webview(
        &mut self,
        webview: Box<dyn WebViewTrait>,
        viewport_details: ViewportDetails,
        viewport_origin: DeviceVector2D,
    ) {
        self.webview_renderers
            .entry(webview.id())
            .or_insert(WebViewRenderer::new(
                webview,
                viewport_details,
                viewport_origin,
                self.embedder_to_constellation_sender.clone(),
                self.refresh_driver.clone(),
                self.webrender_document,
            ));
    }

    pub(crate) fn remove_webview(&mut self, webview_id: WebViewId) {
        if self.webview_renderers.remove(&webview_id).is_none() {
            warn!("Tried removing unknown WebView: {webview_id:?}");
            return;
        };

        self.send_root_pipeline_display_list();
        self.lcp_calculator.enable_for_webview(&webview_id);
    }

    pub(crate) fn is_empty(&mut self) -> bool {
        self.webview_renderers.is_empty()
    }

    pub(crate) fn set_webview_hidden(
        &mut self,
        webview_id: WebViewId,
        hidden: bool,
    ) -> Result<(), UnknownWebView> {
        debug!("Setting WebView visiblity for {webview_id:?} to hidden={hidden}");
        let Some(webview_renderer) = self.webview_renderer_mut(webview_id) else {
            return Err(UnknownWebView(webview_id));
        };
        if !webview_renderer.set_hidden(hidden) {
            return Ok(());
        }
        self.send_root_pipeline_display_list();
        Ok(())
    }

    pub(crate) fn set_hidpi_scale_factor(
        &mut self,
        webview_id: WebViewId,
        new_scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,
    ) {
        let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) else {
            return;
        };
        if !webview_renderer.set_hidpi_scale_factor(new_scale_factor) {
            return;
        }

        self.send_root_pipeline_display_list();
        self.set_needs_repaint(RepaintReason::Resize);
    }

    pub(crate) fn set_viewport_details(
        &mut self,
        webview_id: WebViewId,
        viewport_details: ViewportDetails,
    ) {
        let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) else {
            return;
        };
        if !webview_renderer.set_viewport_details(viewport_details) {
            return;
        }

        self.send_root_pipeline_display_list();
        self.set_needs_repaint(RepaintReason::Resize);
    }

    pub(crate) fn set_viewport_details_and_origin(
        &mut self,
        webview_id: WebViewId,
        viewport_details: ViewportDetails,
        viewport_origin: DeviceVector2D,
    ) {
        let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) else {
            return;
        };

        let viewport_details_changed = webview_renderer.set_viewport_details(viewport_details);
        let viewport_origin_changed = webview_renderer.set_viewport_origin(viewport_origin);
        if !viewport_details_changed && !viewport_origin_changed {
            return;
        }

        self.send_root_pipeline_display_list();
        self.set_needs_repaint(RepaintReason::Resize);
    }

    pub(crate) fn resize_rendering_context(&mut self, new_size: PhysicalSize<u32>) {
        if self.rendering_context.size() == new_size {
            return;
        }

        // 여기 도달 = 실제 client 크기 변경(위 early-return이 무변화를 걸러냄). DComp Native
        // 경로에서만, 리사이즈 재구축 디바운스를 시작/리셋한다. 드래그는 매 프레임 이 경로를
        // 여러 번 호출하므로 pending을 세우고 안정화 카운터를 0으로 되돌려, 크기가 멎은 뒤
        // (DCOMP_RESIZE_DEBOUNCE_FRAMES 프레임) 단 한 번만 재구축이 발동하게 한다. Draw 경로는
        // 자신의 버퍼를 자연스럽게 재생성하므로 이 비용을 지불하지 않는다.
        #[cfg(windows)]
        if self.dcomp_native_active {
            // 드래그의 "첫" 크기 변경(pending false→true 전이) 감지. 연속 드래그는 pending을
            // true로 유지하므로 아래 12b 처리는 드래그당 정확히 1회만 발동한다.
            let first_change = !self.dcomp_resize_pending.get();
            self.dcomp_resize_pending.set(true);
            self.dcomp_resize_stable_frames.set(0);
            // task-12b: 드래그 시작 시 (A) 리사이즈 활성 신호 → 컴포지터가 다음 end_frame부터
            // 전 스왑체인을 가상으로 강등하고 승격/regen을 억제(드래그 중 비디오 블랙 방지),
            // (B) 재구축 1회 선발동(항상 alternate) → 드래그 이전 가상 서피스에 남은 스테일
            // 잔상을 즉시 제거(정착 시 Task 12 재구축이 항상 steady로 되돌려 드래그당 총 2회,
            // alternate→steady 순서 고정). first_change 래치가 드래그당 시작-발동을 정확히
            // 1회로 제한한다.
            //
            // 게이트 구조(리뷰 수정): (B) 시작-발동은 마스터 스위치에만 걸린다 — 결정론 수렴
            // 방식에서 정착 재구축(Task 12)은 current(=alternate) != steady 전이에 의존하므로,
            // 시작-발동이 빠지면 정착 시 desired==current가 되어 picture.rs:2320이 no-op =
            // Task 12까지 무력화된다. (A) 가상 모드(강등/억제)만 12b 전용 스위치로 추가
            // 게이트한다. 따라서 RED(SERVO_DCOMP_DISABLE_RESIZE_VIRTUAL=1) = 정확히 Task 12
            // 동작(시작 alternate → 정착 steady 재구축, 강등/억제 없음), 기본 = 12b 전체 동작,
            // 마스터 스위치 = 12/12b 전부 비활성.
            if first_change && !*DCOMP_RESIZE_REBUILD_DISABLED {
                let virtual_mode = !*DCOMP_RESIZE_VIRTUAL_DISABLED;
                if virtual_mode {
                    self.rendering_context.set_dcomp_resize_active(true);
                }
                let next = self.fire_dcomp_tile_rebuild_start();
                info!(
                    "[dcomp-native] runtime resize started; virtual-only mode {} + start-rebuild \
                     via SetPictureTileSize={}x{} (purges pre-drag ghosts{}) — task-12b",
                    if virtual_mode { "ON" } else { "OFF (RESIZE_VIRTUAL disabled)" },
                    next.width,
                    next.height,
                    if virtual_mode {
                        "; demotes swapchains, suppresses promotion/regen"
                    } else {
                        ""
                    },
                );
            }
        }

        if let Err(error) = self.rendering_context.make_current() {
            error!("Failed to make the rendering context current: {error:?}");
        }
        self.rendering_context.resize(new_size);

        let new_size = Size2D::new(new_size.width as f32, new_size.height as f32);
        let new_viewport_rect = Rect::from(new_size).to_box2d();
        for webview_renderer in self.webview_renderers.values_mut() {
            webview_renderer.set_rect(new_viewport_rect);
        }

        let mut transaction = Transaction::new();
        transaction.set_document_view(new_viewport_rect.to_i32());
        self.send_transaction(transaction);

        self.send_root_pipeline_display_list();
        self.set_needs_repaint(RepaintReason::Resize);
    }

    pub(crate) fn set_page_zoom(&mut self, webview_id: WebViewId, new_zoom: f32) {
        if let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) {
            webview_renderer.set_page_zoom(Scale::new(new_zoom));
        }
    }

    pub(crate) fn page_zoom(&self, webview_id: WebViewId) -> f32 {
        self.webview_renderers
            .get(&webview_id)
            .map(|webview_renderer| webview_renderer.page_zoom.get())
            .unwrap_or(1.)
    }

    pub(crate) fn notify_input_event(
        &mut self,
        webview_id: WebViewId,
        event: InputEventAndId,
    ) -> bool {
        self.webview_renderers
            .get_mut(&webview_id)
            .is_some_and(|webview_renderer| {
                match &event.event {
                    InputEvent::MouseMove(event) => {
                        // We only track the last mouse move position for non-touch events.
                        if !event.is_compatibility_event_for_touch {
                            let viewport_point = event
                                .point
                                .as_device_point(webview_renderer.device_pixels_per_page_pixel());
                            self.last_mouse_move_position = Some(
                                webview_renderer.render_point_from_viewport_point(viewport_point),
                            );
                        }
                    },
                    InputEvent::MouseLeftViewport(_) => {
                        self.last_mouse_move_position = None;
                    },
                    _ => {
                        // Disable LCP calculation on any other input event except mouse moves.
                        self.lcp_calculator.disable_for_webview(webview_id);
                    },
                }

                webview_renderer.notify_input_event(&self.webrender_api, &self.needs_repaint, event)
            })
    }

    pub(crate) fn notify_scroll_event(
        &mut self,
        webview_id: WebViewId,
        scroll: Scroll,
        point: WebViewPoint,
    ) {
        if let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) {
            webview_renderer.notify_scroll_event(scroll, point);
        }
        // Disable LCP calculation on any scroll event.
        self.lcp_calculator.disable_for_webview(webview_id);
    }

    pub(crate) fn enable_lcp_calculation(&mut self, webview_id: &WebViewId) {
        self.lcp_calculator.enable_for_webview(webview_id);
    }

    pub(crate) fn lcp_calculation_enabled_for_webview(&self, webview_id: &WebViewId) -> bool {
        self.lcp_calculator.enabled_for_webview(webview_id)
    }

    pub(crate) fn adjust_pinch_zoom(
        &mut self,
        webview_id: WebViewId,
        pinch_zoom_delta: f32,
        center: DevicePoint,
    ) {
        if let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) {
            webview_renderer.adjust_pinch_zoom(pinch_zoom_delta, center);
        }
    }

    pub(crate) fn pinch_zoom(&self, webview_id: WebViewId) -> f32 {
        self.webview_renderers
            .get(&webview_id)
            .map(|webview_renderer| webview_renderer.pinch_zoom().zoom_factor().0)
            .unwrap_or(1.)
    }

    pub(crate) fn device_pixels_per_page_pixel(
        &self,
        webview_id: WebViewId,
    ) -> Scale<f32, CSSPixel, DevicePixel> {
        self.webview_renderers
            .get(&webview_id)
            .map(WebViewRenderer::device_pixels_per_page_pixel)
            .unwrap_or_default()
    }

    pub(crate) fn request_screenshot(
        &self,
        webview_id: WebViewId,
        rect: Option<WebViewRect>,
        callback: Box<dyn FnOnce(Result<RgbaImage, ScreenshotCaptureError>) + 'static>,
    ) {
        let Some(webview) = self.webview_renderers.get(&webview_id) else {
            return;
        };

        let rect = rect.map(|rect| rect.as_device_rect(webview.device_pixels_per_page_pixel()));
        self.screenshot_taker
            .request_screenshot(webview_id, rect, callback);
        self.send_to_constellation(EmbedderToConstellationMessage::RequestScreenshotReadiness(
            webview_id,
        ));
    }

    pub(crate) fn notify_input_event_handled(
        &mut self,
        webview_id: WebViewId,
        input_event_id: InputEventId,
        result: InputEventResult,
    ) {
        let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) else {
            warn!("Handled input event for unknown webview: {webview_id}");
            return;
        };
        webview_renderer.notify_input_event_handled(
            &self.webrender_api,
            &self.needs_repaint,
            input_event_id,
            result,
        );
    }

    pub(crate) fn refresh_cursor(&self) {
        let Some(last_mouse_move_position) = self.last_mouse_move_position else {
            return;
        };

        let Some(hit_test_result) = Self::hit_test_at_point_with_api_and_document(
            &self.webrender_api,
            self.webrender_document,
            last_mouse_move_position,
        )
        .first()
        .cloned() else {
            return;
        };

        if let Err(error) = self.embedder_to_constellation_sender.send(
            EmbedderToConstellationMessage::RefreshCursor(hit_test_result.pipeline_id),
        ) {
            warn!("Sending event to constellation failed ({:?}).", error);
        }
    }

    pub(crate) fn handle_new_webrender_frame_ready(&self, repaint_needed: bool) {
        if repaint_needed {
            self.refresh_cursor()
        }

        if repaint_needed || self.animation_callbacks_running() {
            self.set_needs_repaint(RepaintReason::NewWebRenderFrame);
        }

        // If we received a new frame and a repaint isn't necessary, it may be that this
        // is the last frame that was pending. In that case, trigger a manual repaint so
        // that the screenshot can be taken at the end of the repaint procedure.
        if !repaint_needed {
            self.screenshot_taker
                .maybe_trigger_paint_for_screenshot(self);
        }
    }

    pub(crate) fn webviews_needing_repaint(&self) -> Vec<WebViewId> {
        if self.needs_repaint() {
            self.webview_renderers
                .values()
                .map(|webview_renderer| webview_renderer.id)
                .collect()
        } else {
            Vec::new()
        }
    }

    pub(crate) fn scroll_trees_memory_usage(
        &self,
        ops: &mut malloc_size_of::MallocSizeOfOps,
    ) -> usize {
        self.webview_renderers
            .values()
            .map(|renderer| renderer.scroll_trees_memory_usage(ops))
            .sum::<usize>()
    }

    pub(crate) fn append_lcp_candidate(
        &mut self,
        lcp_candidate: LCPCandidate,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        epoch: Epoch,
    ) {
        if self.lcp_calculation_enabled_for_webview(&webview_id) {
            self.lcp_calculator.append_lcp_candidate(
                lcp_candidate,
                pipeline_id.into(),
                &webview_id,
            );
            if let Some(webview_renderer) = self.webview_renderers.get_mut(&webview_id) {
                webview_renderer
                    .ensure_pipeline_details(pipeline_id)
                    .largest_contentful_paint_metric
                    .set(PaintMetricState::Seen(epoch.into(), false));
            }
        };
    }
}

/// A struct that is reponsible for delaying frame requests until all new canvas images
/// for a particular "update the rendering" call in the `ScriptThread` have been
/// sent to WebRender.
///
/// These images may be updated in WebRender asynchronously in the canvas task. A frame
/// is then requested if:
///
///  - The renderer has received a GenerateFrame message from a `ScriptThread`.
///  - All pending image updates have finished and have been noted in the [`FrameDelayer`].
#[derive(Default)]
pub(crate) struct FrameDelayer {
    /// The latest [`Epoch`] of canvas images that have been sent to WebRender. Note
    /// that this only records the `Epoch`s for canvases and only ones that are involved
    /// in "update the rendering".
    image_epochs: FxHashMap<ImageKey, Epoch>,
    /// A map of all pending canvas images
    pending_canvas_images: FxHashMap<ImageKey, Epoch>,
    /// Whether or not we have a pending frame.
    pub(crate) pending_frame: bool,
    /// A list of pipelines that should be notified when we are no longer waiting for
    /// canvas images.
    waiting_pipelines: FxHashSet<PipelineId>,
}

impl FrameDelayer {
    pub(crate) fn delete_image(&mut self, image_key: ImageKey) {
        self.image_epochs.remove(&image_key);
        self.pending_canvas_images.remove(&image_key);
    }

    pub(crate) fn update_image(&mut self, image_key: ImageKey, epoch: Epoch) {
        self.image_epochs.insert(image_key, epoch);
        let Entry::Occupied(entry) = self.pending_canvas_images.entry(image_key) else {
            return;
        };
        if *entry.get() <= epoch {
            entry.remove();
        }
    }

    pub(crate) fn add_delay(
        &mut self,
        pipeline_id: PipelineId,
        canvas_epoch: Epoch,
        image_keys: Vec<ImageKey>,
    ) {
        for image_key in image_keys.into_iter() {
            // If we've already seen the necessary epoch for this image, do not
            // start waiting for it.
            if self
                .image_epochs
                .get(&image_key)
                .is_some_and(|epoch_seen| *epoch_seen >= canvas_epoch)
            {
                continue;
            }
            self.pending_canvas_images.insert(image_key, canvas_epoch);
        }
        self.waiting_pipelines.insert(pipeline_id);
    }

    pub(crate) fn needs_new_frame(&self) -> bool {
        self.pending_frame && self.pending_canvas_images.is_empty()
    }

    pub(crate) fn set_pending_frame(&mut self, value: bool) {
        self.pending_frame = value;
    }

    /// 스크립트가 이 painter 의 ack 를 기다리는 중인가. ★스톨 진단용★ — 이 집합이 비지
    /// 않은 동안 그 파이프라인의 문서는 `waiting_on_canvas_image_updates` 로 잠겨 있어
    /// rAF 를 한 번도 더 돌리지 못한다(`script_thread.rs:1200`).
    pub(crate) fn is_holding_ack(&self) -> bool {
        !self.waiting_pipelines.is_empty()
    }

    pub(crate) fn pending_canvas_image_count(&self) -> usize {
        self.pending_canvas_images.len()
    }

    /// ack 를 기다리는 파이프라인 수. 주입이 **실제로 무엇을 보류했는지** 로그에 싣기 위한 것.
    pub(crate) fn waiting_pipeline_count(&self) -> usize {
        self.waiting_pipelines.len()
    }

    pub(crate) fn take_waiting_pipelines(&mut self) -> Vec<PipelineId> {
        self.waiting_pipelines.drain().collect()
    }
}

/// The paint status of a particular pipeline in a [`Painter`]. This is used to trigger metrics
/// in script (via the constellation) when display lists are received.
///
/// See <https://w3c.github.io/paint-timing/#first-contentful-paint>.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PaintMetricState {
    /// The painter is still waiting to process a display list which triggers this metric.
    Waiting,
    /// The painter has processed the display list which will trigger this event, marked the Servo
    /// instance ready to paint, and is waiting for the given epoch to actually be rendered.
    Seen(WebRenderEpoch, bool /* first_reflow */),
    /// The metric has been sent to the constellation and no more work needs to be done.
    Sent,
}

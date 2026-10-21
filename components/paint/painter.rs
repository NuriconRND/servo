/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, LazyCell, RefCell};
use std::collections::{VecDeque, hash_map::Entry};
use std::panic::Location;
use std::rc::Rc;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use dpi::PhysicalSize;
use embedder_traits::{
    EventLoopWaker, InputEvent, InputEventAndId, InputEventId, InputEventResult,
    PaintHitTestResult, RefreshDriver, ScreenshotCaptureError, Scroll, ViewportDetails, WebViewPoint,
    WebViewRect,
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
    ImageUpdate, PaintProxy, PainterSurfmanDetailsMap, PipelineExitSource, SendableFrameTree,
    SerializableDisplayListPayload, SerializableImageData, WebRenderExternalImageHandlers,
    WebRenderExternalImageIdManager, WebRenderImageHandlerType,
};
use profile_traits::time::{ProfilerCategory, ProfilerChan};
use profile_traits::time_profile;
use rustc_hash::{FxHashMap, FxHashSet};
use servo_base::Epoch;
use servo_base::cross_process_instant::CrossProcessInstant;
use servo_base::generic_channel::GenericSharedMemory;
use servo_base::id::{PainterId, PipelineId, WebViewId};
use servo_canvas_traits::webgl::{WebGLSurfaceId, WebGLThreads};
use servo_config::debug_env;
use servo_config::{opts, pref};
use servo_constellation_traits::{EmbedderToConstellationMessage, PaintMetricEvent};
use servo_geometry::DeviceIndependentPixel;
use smallvec::SmallVec;
use style_traits::CSSPixel;
use surfman::Device;
use surfman::chains::SwapChains;
use webgl::webgl_thread::WebGLContextBusyMap;
use webgpu::canvas_context::WebGpuExternalImageMap;
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
    ImageKey, NativeFontHandle, PipelineId as WebRenderPipelineId, PropertyBinding,
    ReferenceFrameKind, RenderReasons, SampledScrollOffset, SpaceAndClipInfo, SpatialId,
    TransformStyle,
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

/// `Paint` 쪽 계수 지점이 꺼져 있을 때 `with_painter`(스레드 왕복)를 아예 안 하도록 묻는 자리.
pub(crate) fn frame_reason_prof_enabled() -> bool {
    *FRAME_REASON_PROF
}

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
// ★낡음(2026-07-16 기록, 무효 시점 불명 — 당시 webrender 는 트리 밖 0.68)★ — 판정 자리는 `vendor_local/webrender/src/picture.rs:2332`.
// desired==current no-op이 되어 Task 12마저 무력화되기 때문(리뷰 수정; picture.rs:2320).
// RED(12b off, Task 12 on) ↔ GREEN(둘 다 on) A/B를 같은 바이너리에서 재현하기 위한 스위치.
#[cfg(windows)]
static DCOMP_RESIZE_VIRTUAL_DISABLED: LazyLock<bool> = LazyLock::new(|| {
    debug_env::string(&debug_env::DCOMP_DISABLE_RESIZE_VIRTUAL)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
});

use crate::largest_contentful_paint_calculator::LargestContentfulPaintCalculator;
use crate::paint::{RepaintReason, WebRenderDebugOption};
use crate::refresh_driver::{AnimationRefreshDriverObserver, BaseRefreshDriver};
use crate::render_notifier::RenderNotifier;
use crate::screenshot::ScreenshotTaker;
use crate::wall_tick::{TileOutcome, ack_is_releasable, tile_may_generate};
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
    /// GPU 메모리를 마지막으로 남긴 시각(초당 한 줄로 묶기 위한 것).
    last_gpu_memory_log_at: Cell<Option<Instant>>,

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
    /// 이 painter 가 **표출까지 책임지는가**.
    ///
    /// 인라인 경로에서는 셸이 `render()` 뒤에 직접 `present()` 한다(winit_wall 의
    /// `render_all_tiles`). 타일 스레드에 사는 painter 는 그럴 수 없다 — 셸에는 그 컨텍스트가
    /// 없고, 있어도 다른 스레드다. 그래서 그 경로에서는 `render()` 가 끝에 스스로 present 한다.
    ///
    /// ★기본값은 `false` 다★ — 켜져 있는데 셸도 present 하면 한 프레임을 두 번 내보낸다.
    owns_presentation: bool,

    /// `Paint` 로 되돌아가는 통로. `WebViewRenderer` 가 애니메이션 상태 통보를 여기로 보낸다
    /// — 예전에는 `Box<dyn WebViewTrait>` 를 직접 들고 불렀다(설계 §3.2).
    paint_proxy: PaintProxy,

    last_render_end: Cell<Option<Instant>>,

    /// 이 painter 가 스크립트의 캔버스 ack 를 붙잡기 시작한 시각, 그리고 마지막 보고 시각.
    /// ★스톨 진단용★ — [`Painter::check_canvas_ack_latch`] 참고.
    canvas_ack_held_since: Cell<Option<Instant>>,
    canvas_ack_last_report: Cell<Option<Instant>>,

    /// [`Painter::flush_owed_canvas_ack`] 가 실제로 보낸 횟수와 그 집계 창의 시작 시각.
    /// ★양성 대조용★ — 고쳐진 것과 코드가 안 도는 것을 구분하기 위한 것이다.
    owed_ack_flush_count: Cell<u64>,
    owed_ack_last_report: Cell<Option<Instant>>,

    /// 남은 ack 주입 횟수(`SERVO_WALL_CANVAS_ACK_SKIP`). 0 이면 주입 없음 = 기본값.
    canvas_ack_skips_left: Cell<u64>,

    /// 합성 출처 집계(`SERVO_FRAME_REASON_PROF`). 키는 `(FrameSource, 이유 문자열)` 이다.
    /// `FrameSource::WebRender` 는 `generate_frame` 을 부른 호출 지점의 줄 번호(2026-10-21
    /// Task 7 뒤로 둘 -- 틱 생산과 파이프라인 부트스트랩)이고,
    /// WR 프레임이 아닌 두 경로도 같은 집계에 들어온다 — `ExternalSurface`(`present_external_only`
    /// 의 DComp 직행 commit)와 `WallPacerJoin`(coalesce 됐다 풀린 벽 요청; total 에는 안 더한다).
    /// 요청 경로가 여럿이라, 초당 200 회를 누가 만드는지 총합만으로는 가려낼 수 없다.
    frame_reason_counts: RefCell<FrameReasonCounts>,
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

    /// ★이 타일이 external fast-present 를 쓴 마지막 틱.★ 틱마다 최대 1 회를 그 번호
    /// 하나로 정한다(설계 7 절, `wall_tick::ExternalPresentBudget`). 2026-10-21 전에는
    /// 직전 present 에서 14ms 라는 자기 상수였는데, 그 상수는 표출 주기와 무관했다.
    /// `dcomp_shared`의 fast-path에서만 읽으므로 같이 `#[cfg(windows)]`로 게이트한다.
    #[cfg(windows)]
    external_present_budget: RefCell<crate::wall_tick::ExternalPresentBudget>,

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

    /// 현재 `PAINTANIM` 창이 열린 시각.
    ///
    /// ★프레임 계수(`paint_animation_frames`)와 그 속도 제한(`last_paint_animation_frame_at`)
    /// 은 2026-10-21 에 사라졌다★ -- 페인트측 애니메이션은 더는 프레임을 내지 않는다(설계 2-6).
    paint_animation_window_start: RefCell<Option<Instant>>,
    /// 값을 밀어 넣을 때 이 타일이 바빴던 횟수(현재 `PAINTANIM` 창 기준). ★생산 판정이
    /// 아니라 표본의 사정이다★ -- 값은 바빠도 나간다(불변식 7).
    paint_animation_skipped_busy: Cell<u64>,
    /// How often the animation's values rode along on frames somebody else was producing.
    /// On a wall with video this should be nearly all of them.
    paint_animation_rode_along: Cell<u64>,
    /// 한 전송에 도로 얹힌 "끝난 애니메이션" 값의 최대 개수(현재 `PAINTANIM` 창 기준).
    ///
    /// ***이 수가 0이 아니었던 창이 곧 화면이 검어질 수 있었던 창이다.*** 이 값들을 얹지
    /// 않으면 그만큼의 키가 그 전송에서 지워져 시작값으로 되돌아간다 -- 검은 화면의 정체다.
    /// 그래서 이 수는 성능 지표가 아니라 **가설의 증거**다.
    paint_animation_held_max: Cell<usize>,
    /// When any frame was last generated for this painter, by any path.
    last_frame_generated_at: Cell<Option<Instant>>,
    /// 비디오 프레임이 마지막으로 **도착한** 시각(페인터에 이미지 갱신으로 들어온 시각).
    ///
    /// `last_video_presented_at` 과 짝이다. 둘을 같이 재야 "안 왔다"와 "왔는데 못 내보냈다"가
    /// 갈린다 — 화면이 멈춘 것만 보고는 그 둘을 구분할 수 없고, 고칠 곳은 정반대다.
    last_video_arrived_at: Cell<Option<Instant>>,

    /// 비디오 프레임이 마지막으로 **화면에 실려 나간** 시각(합성에 flush 된 시각).
    ///
    /// 재생이 매끄러운지는 도착률이 아니라 이 간격이 말해 준다 — 도착은 30/s 인데 표출이
    /// 한 번 300ms 를 건너뛰면 평균은 멀쩡해 보이고 눈에는 끊긴다.
    last_video_presented_at: Cell<Option<Instant>>,

    /// 이 페인터에 프레임을 낸 마지막 시각.
    ///
    /// "남이 이미 프레임을 내고 있으니 나는 값만 얹으면 된다"를 판정하는 데 쓴다
    /// (`pump_paint_animation` 의 `someone_else_is_producing`, `raf_is_producing_frames`).
    ///
    /// ★"자기 자신은 빼고" 가 2026-10-21 Task 7 에 사라졌다.★ 스스로 프레임을 내던 세 경로
    /// (페인트측 애니메이션, 유보 애니메이션 프레임, 비디오 도착)가 자기 프레임을 낸 뒤 이
    /// 시각을 되돌려 놓았었다. 그 셋이 ★한꺼번에★ 삭제되어 writer 가 이 함수 하나만 남았고,
    /// 되돌릴 "자기 프레임" 자체가 없다. 하나라도 남겨 두면 안 됐던 이유는 실측에 있다
    /// (log_presentation/00, 60Hz 벽): 되돌리지 않은 채로 페이드가 도는 1 초 동안 렌더가
    /// **61 -> 72 프레임**으로 늘었고, 그 초과분이 애니메이션이 스스로 낸 프레임과 정확히
    /// 맞았다. ★지금 "남" 은 언제나 틱이다★ -- 틱이 주기마다 찍으므로 이 두 판정은
    /// 사실상 "이번 주기의 틱 프레임이 아직 렌더되지 않았나" 를 묻는다.
    last_frame_by_other_source_at: Cell<Option<Instant>>,
    /// When the animation values were last pushed. They only matter at frame-build time,
    /// so pushing faster than frames are built is waste.
    last_paint_animation_push_at: Cell<Option<Instant>>,
    /// `note_animation_step` 의 상태. ★레인마다★ 직전 표본의 (x, 시각).
    ///
    /// ★전에는 `transforms.first()` 하나만 봤다.★ 그게 두 번 생각을 빗나가게 했다 --
    /// 프로브는 레인을 일곱 줄(A / B / B' / C / C' / D / D') 그리고 `PAINTANIM` 은
    /// animations=28 을 세는데, 지표는 그중 하나만 따라갔다. 그 하나가 균일하다는
    /// 것은 ***나머지가 균일하다는 뜻이 아니다.*** 운영자가 보는 레인이 추적하는
    /// 레인이라는 보장도 없다.
    ///
    /// 관측(log_ani_debug 32): divisor 를 15/30/60 으로 두자 저더 간격이 각각
    /// 0.25 / 0.5 / 1 초로 나왔다 -- ***`divisor / 60` 정확히 스크립트 틱 순간***이다.
    /// 모양은 객체가 잠시 앞으로 튀어나갔다 돌아온다. 그런데 추적 중인 레인은
    /// div=60 에서도 `dx max=12.80` 으로 ***튀지 않았다.*** 둘 중 하나다: 다른
    /// 레인이거나, 튀는 것이 샘플값이 아니거나. 앞에거를 먼저 닫는다.
    anim_step_previous: RefCell<FxHashMap<u32, (f32, Instant)>>,
    anim_step_window_start: RefCell<Option<Instant>>,
    anim_step_dt: RefCell<Vec<f64>>,
    anim_step_dx: RefCell<Vec<f64>>,
    /// ★`ANIMSTEP` 은 WebRender 에 넘기기 **전** 값이다.★ `transforms_log` 는
    /// `update_paint_animations` 가 막 채운 `transforms` 를 그대로 복사한 것이라, 그 수열이
    /// 고르다는 것은 ***샘플링이 고르다*** 는 뜻이지 화면이 고르다는 뜻이 아니다. 그 사이에
    /// WebRender 자신의 스레드들이 있다 -- `WRRenderBackend` / `WRSceneBuilder` 는 각자
    /// 큐를 갖고, 우리 페이싱 스레드도 커밋 스케줄러도 그 큐가 언제 프레임을 내놓는지
    /// 정하지 못한다. 요청은 `generate_frame` 이고 완료는 `notifier.wake_up` 역통지다.
    ///
    /// 그래서 ***완료 쪽***에서 한 번 더 센다. 밀어넣은 x 를 그 요청에 달아 두고, 프레임
    /// 준비 통지가 올 때 꺼내 직전 완료와의 차를 본다. 보낸 쪽 dx 가 12.80 으로 평평한데
    /// 완료 쪽 dx 가 벌어지면 ***범인은 이 구간***이고, 완료 쪽도 평평하면 이 구간은
    /// 결백하다. 지금까지 이 구간만 미계측이었다.
    anim_ready_queue: RefCell<VecDeque<(u64, Option<f32>, Instant)>>,
    /// 다음 `generate_frame` 에 달아 보낼 x. 밀어넣기 자리가 세우고 요청 자리가 가져간다.
    anim_ready_pending_x: Cell<Option<f32>>,
    /// 직전 **완료** 프레임의 (x, 시각).
    anim_ready_previous: Cell<Option<(f32, Instant)>>,
    anim_ready_window_start: RefCell<Option<Instant>>,
    anim_ready_dx: RefCell<Vec<f64>>,
    anim_ready_dt: RefCell<Vec<f64>>,
    /// 요청에서 완료까지 걸린 시간(ms). WebRender 큐가 밀리는지를 본다.
    anim_ready_latency: RefCell<Vec<f64>>,
    /// 애니메이션 값을 달지 않은 완료의 수. 그 사이 밀어넣은 값은 자기 프레임을 못 얻었다.
    anim_ready_unmatched: Cell<u64>,
    /// ★아직 렌더가 집어가지 않은 완료의 꼬리표.★ `true` = 애니메이션 값을 단 프레임.
    ///
    /// ★낡음(2026-10-16 `dfb6116e8ef` 기록) — 2026-10-21 Task 7 로 전제가 통째로 사라졌다.★
    /// 당시 프레임 원천은 둘이었고(ANIMATED_PROPERTY 와 `generate_frame_for_script` 의
    /// SCENE), div=1 이면 초당 120 개를 요청해 60 개만 화면에 갔다(`WRRATE onscreen`).
    /// ***둘 중 어느 쪽이 합성 슬롯을 가져가는지***가 화면 위치를 정했는데, 그 둘이 이제
    /// 하나다. 꼬리표는 남긴다 -- "값을 단 프레임이 렌더에 집히기 전에 덮였나"
    /// (`anim_win_overwritten`)는 생산자가 하나여도 물을 수 있는 값이다.
    ///
    /// 앞섬 가설은 반증됐다 -- `-SampleLead 0/1/2`(lead_ms 28.91/45.57/62.26)로 여섯 번
    /// ★낡음(2026-10-16 기록, 같은 날 `c515ae4c1df` 이후 무효)★ — 부호가 :2047 과 반대이고, 2026-10-19 실측에서 divisor 1~60 전부 60/60 이다.
    /// 돌렸는데 저더가 똑같았다. 그런데 divisor 에는 반응한다(d=4 가 더 나쁘다). 그러면
    /// 어긋남의 크기는 앞섬이 아니라 ***디스플레이 리스트의 나이***로 재야 하고, 그 나이를
    /// 실어 오는 것이 스크립트 프레임이다. 그 프레임이 슬롯을 가져가면 직전 애니메이션
    /// 위치는 ***화면에 닿지 못한다.***
    anim_win_pending_tag: Cell<Option<bool>>,
    /// 렌더가 집어간 완료를 꼬리표별로 센다.
    ///
    /// ★2026-10-21 Task 7 뒤로 `took_script` 는 구조적으로 ~0 이다.★ 꼬리표는
    /// `anim_ready_pending_x` 가 붙었는지로 갈리는데, 그 부착 조건이 "애니메이션이 자기
    /// 프레임을 낼 때" 에서 "값을 밀어 넣은 주기마다" 로 바뀌었다(생산자가 틱 하나가 되어
    /// 앞의 조건이 영영 거짓이기 때문이다). 그래서 값이 도는 동안의 완료는 거의 전부
    /// `took_anim` 으로 간다. ★둘의 비율을 2026-10-21 이전 로그와 비교하지 말 것.★
    /// 뜻이 남아 있는 것은 `anim_win_overwritten` 이다.
    anim_win_took_anim: Cell<u64>,
    anim_win_took_script: Cell<u64>,
    /// ★이 추적이 찾는 수.★ 애니메이션 완료가 렌더에 집히기 전에 다음 완료로 덮인
    /// 횟수 -- 그 위치는 영영 화면에 나오지 않았다.
    anim_win_overwritten: Cell<u64>,
    anim_win_window_start: RefCell<Option<Instant>>,
    /// 이 페인터가 마지막으로 **실제 렌더 패스를 시작한** 시각. 애니메이션 값을 밀어넣는
    /// 박자를 여기에 맞춘다 — 자기 타이머로 돌면 벽의 표출 주기와 위상이 어긋나 미끄러진다.
    last_render_started_at: Cell<Option<Instant>>,

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
    pending_video_frame_updates:
        RefCell<FxHashMap<ImageKey, (ImageDescriptor, SerializableImageData)>>,

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
    /// ★낡음(2026-07-16 기록, 무효 시점 불명 — 당시 webrender 는 트리 밖 0.68)★ — `default_tile_size()` 는 `vendor_local/webrender/src/picture.rs:2325`.
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

/// `Painter::new` 이 `Paint` 로부터 필요로 하는 전부. ★모든 필드가 `Send` 다★ —
/// [`Paint::assert_painter_inputs_are_send`] 가 컴파일 타임에 강제한다.
///
/// 왜 `&Paint` 대신 이 묶음인가: 병렬 타일 렌더(A 안)에서 `Painter` 는 **타일 스레드가
/// 직접 만든다.** 만들어 놓고 옮기는 것은 불가능하다 — `webrender::Renderer` 도
/// `RenderingContext` 도 `!Send` 이고(설계 §7-3, §3-1), `Paint` 자체도 `Rc` 투성이라
/// 참조를 스레드로 건넬 수 없다. 그래서 생성에 필요한 것만 `Send` 값으로 떼어낸다.
///
/// 지금은 여전히 메인 스레드가 이것을 만들어 넘긴다. 이 단계는 **소유권 경계를 그은 것**
/// 이지 스레드를 나눈 것이 아니다(설계 §6.2-2).
pub(crate) struct PainterInputs {
    pub(crate) webrender_external_image_id_manager: WebRenderExternalImageIdManager,
    pub(crate) webgl_threads: WebGLThreads,
    pub(crate) swap_chains: SwapChains<WebGLSurfaceId, Device>,
    pub(crate) busy_webgl_contexts_map: WebGLContextBusyMap,
    pub(crate) webgpu_image_map: WebGpuExternalImageMap,
    pub(crate) embedder_to_constellation_sender: Sender<EmbedderToConstellationMessage>,
    pub(crate) event_loop_waker: Box<dyn EventLoopWaker>,
    pub(crate) paint_proxy: PaintProxy,
    /// ★타일 스레드가 자기 것을 여기 등록해야 한다.★ WebGL 팬아웃은 이 표에 없는 painter 를
    /// 대상 목록에서 **조용히 걸러낸다**(`webgl_thread::create_webgl_context`). 인라인 경로는
    /// `Paint::register_rendering_context` 가 채우지만, 스레드 경로는 컨텍스트가 그쪽에서
    /// 만들어지므로 그쪽에서 채울 수밖에 없다.
    pub(crate) painter_surfman_details_map: PainterSurfmanDetailsMap,
}

/// 이 스레드가 지금까지 쓴 CPU 시간(ms). 커널 + 사용자.
///
/// ★벽시계로 320ms 걸린 렌더가 CPU 를 15ms 밖에 쓰지 않았다면 그 스레드는 일한 것이
/// 아니라 **기다린** 것이다★ — 드라이버 안에서 막혔거나, 기계가 포화라 스케줄을 못 받았거나.
/// 반대로 CPU 도 300ms 를 썼다면 그건 실제로 한 일이다. 이 둘은 고칠 곳이 완전히 다른데,
/// `render_ms` 만으로는 구분할 수 없다.
///
/// ★분해능은 스케줄러 틱(약 15.6ms)이다★ — `GetThreadTimes` 는 그 단위로만 올라간다.
/// 그러니 한 자릿수 ms 를 비교하는 데 쓰면 안 되고, 수백 ms 짜리 정체가 일인지 대기인지를
/// 가르는 데만 쓴다. 20ms 렌더에서 `cpu_ms=0.0` 은 "CPU 를 전혀 안 썼다"가 아니라
/// "틱 하나를 못 채웠다"는 뜻이다.
// 크레이트는 `unsafe` 를 금지한다. 여기서 한 번 여는 이유는 Win32 호출 하나뿐이고,
// 인자가 전부 out 파라미터라 안전 조건이 함수 안에서 닫힌다.
#[allow(unsafe_code)]
#[cfg(windows)]
fn thread_cpu_ms() -> f64 {
    use std::mem::zeroed;

    use winapi::shared::minwindef::FILETIME;
    use winapi::um::processthreadsapi::{GetCurrentThread, GetThreadTimes};

    // Safety: 모두 out 파라미터이고, 핸들은 항상 유효한 의사 핸들이다.
    unsafe {
        let (mut creation, mut exit, mut kernel, mut user): (
            FILETIME,
            FILETIME,
            FILETIME,
            FILETIME,
        ) = (zeroed(), zeroed(), zeroed(), zeroed());
        if GetThreadTimes(
            GetCurrentThread(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        ) == 0
        {
            return 0.0;
        }
        let to_ns100 =
            |time: FILETIME| ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64;
        // FILETIME 의 단위는 100ns 다.
        (to_ns100(kernel) + to_ns100(user)) as f64 / 10_000.0
    }
}

#[cfg(not(windows))]
fn thread_cpu_ms() -> f64 {
    0.0
}

/// rAF 가 비디오를 태워 줄 만큼 실제로 합성을 내고 있는가. 판정만 떼어 두어 시험한다
/// (`Painter::raf_is_producing_frames` 가 살아 있는 값으로 이것을 부른다).
///
/// `since_last_frame_by_others` 이 `None` 이면 남이 낸 프레임이 한 장도 없다 — 그때는
/// 태워 줄 것이 없으므로 거짓이다.
pub(crate) fn raf_is_driving_composites(
    animation_callbacks_running: bool,
    since_last_frame_by_others: Option<Duration>,
    refresh_period: Duration,
) -> bool {
    if !animation_callbacks_running {
        return false;
    }
    // ★한 주기 반★ — 위아래가 둘 다 막혀 있어서 그 사이다.
    //
    //   * 두 주기(처음에 잡았던 값)는 **너무 길다.** 애니메이션이 끝나는 경계에서 비디오가
    //     그만큼 더 기다렸고, 그것이 화면에서 "뚝 끊겼다가 이어지는" 한 박자로 보였다.
    //   * 정확히 한 주기는 **너무 짧다.** 60fps 로 도는 멀쩡한 rAF 도 도착이 다음 프레임
    //     직전에 걸리면 간격이 한 주기에 닿으므로, 그때마다 판정이 뒤집혀 두 번째 합성원이
    //     끼어든다 — 그것이 이 게이트가 애초에 막으려던 지터다.
    //
    // 그래서 멀쩡한 rAF 의 최대 간격(한 주기)보다는 확실히 크고, 사람이 한 박자로 느끼기에는
    // 작은 값을 쓴다. 실제로 얼마나 끊기는지는 `IMGUPDINNER ... vidgap_max_ms` 가 잰다.
    since_last_frame_by_others.is_some_and(|since| since < refresh_period * 3 / 2)
}

/// `update_images` 한 번의 시간을 트랜잭션 전송과 나머지로 가른 누계.
///
/// ★`frames` / `frame_ms` 는 2026-10-21 에 빠졌다★ -- 이 함수는 더는 합성을 요청하지
/// 않으므로 구조적으로 0 이다(설계 2-6).
///
/// 이 함수는 임베더 메인 스레드에서만 돌므로 thread_local 로 족하다.
#[derive(Default)]
struct UpdateImagesStats {
    window_start: Option<Instant>,
    calls: u64,
    total_ms: f64,
    send_ms: f64,
    /// rAF 는 돌고 있는데 프레임은 안 나오고 있어서 비디오가 스스로 합성해야 했던 횟수.
    /// 0 이 아니면 그 페이지에는 아무것도 그리지 않는 rAF 루프가 있다는 뜻이다.
    raf_idle_calls: u64,
    /// 비디오가 화면에 실려 나간 순간들 사이의 **최대** 간격(ms). 끊김의 길이가 곧 이 값이다.
    video_gap_max_ms: f64,
    /// 비디오 프레임이 **도착한** 순간들 사이의 최대 간격(ms). 위 값과 함께 봐야 뜻이 있다:
    /// 둘 다 크면 프레임이 안 온 것(디코더/파이프라인), 도착은 촘촘한데 표출만 크면 우리가
    /// 못 내보낸 것(합성)이다.
    video_arrival_gap_max_ms: f64,
}

thread_local! {
    static UPDATE_IMAGES_STATS: RefCell<UpdateImagesStats> =
        RefCell::new(UpdateImagesStats::default());
}

/// 이 painter 가 쓰는 어댑터의 GPU 메모리 사용량을 초당 한 줄로 남긴다.
///
/// ★전환 순간 드라이버 연산 전부가 25~100배 느려지고 CPU 는 거의 쓰지 않는다★ — D3D11
/// Map(0.08→2.3ms), 텍스처 파괴(0.2→5.9ms), 렌더(cpu_ms 15.6/270ms). 어느 한 경로가
/// 잘못된 모양이 아니라 드라이버가 막힌 모양이고, 그때 GPU 메모리가 100% 에 근접한다는
/// 것이 실측으로 보고됐다. 그런데 이 프로세스가 실제로 얼마를 들고 있는지는 아무도 재지
/// 않고 있었다 — 작업 관리자의 눈금 말고는.
///
/// DXGI 가 어댑터별로 예산과 현재 사용량을 알려 준다. 로컬(VRAM)과 논로컬(공유 시스템
/// 메모리)을 모두 낸다: 벽은 타일마다 다른 GPU 이므로 어느 어댑터가 차는지가 곧 어느
/// 타일이 막히는지다.
#[allow(unsafe_code)]
#[cfg(windows)]
fn log_gpu_memory(painter_id: PainterId, device: usize) {
    use winapi::Interface;
    use winapi::shared::dxgi::IDXGIDevice;
    use winapi::shared::dxgi1_4::{
        DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL,
        DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3,
    };
    use winapi::um::unknwnbase::IUnknown;

    // Safety: `device` 는 이 painter 의 살아 있는 ID3D11Device 포인터다(렌더링 컨텍스트가
    // 수명을 보장한다). 아래 QueryInterface/GetParent 는 얻은 포인터를 각각 Release 한다.
    unsafe {
        let unknown = device as *mut IUnknown;
        let mut dxgi_device: *mut IDXGIDevice = std::ptr::null_mut();
        if (*unknown).QueryInterface(
            &IDXGIDevice::uuidof(),
            &mut dxgi_device as *mut _ as *mut *mut _,
        ) < 0
            || dxgi_device.is_null()
        {
            return;
        }
        let mut adapter: *mut IDXGIAdapter3 = std::ptr::null_mut();
        let got_adapter = (*dxgi_device).GetParent(
            &IDXGIAdapter3::uuidof(),
            &mut adapter as *mut _ as *mut *mut _,
        );
        (*dxgi_device).Release();
        if got_adapter < 0 || adapter.is_null() {
            return;
        }
        let mut local: DXGI_QUERY_VIDEO_MEMORY_INFO = std::mem::zeroed();
        let mut non_local: DXGI_QUERY_VIDEO_MEMORY_INFO = std::mem::zeroed();
        let ok_local =
            (*adapter).QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local) >= 0;
        let ok_non_local =
            (*adapter).QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut non_local)
                >= 0;
        (*adapter).Release();
        if !ok_local && !ok_non_local {
            return;
        }
        let mb = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
        let used = mb(local.CurrentUsage);
        let budget = mb(local.Budget);
        // CPU 메모리도 같은 줄에 둔다 — 전환마다 순증한다는 보고가 있었는데, GPU 와
        // 나란히 놓지 않으면 어느 전환에서 무엇이 늘었는지 맞춰 보기 어렵다.
        let (working_set, private) = process_memory_mb();
        warn!(
            "GPUMEM painter={painter_id:?} local_mb={used:.0}/{budget:.0} ({:.0}%) \
             nonlocal_mb={:.0}/{:.0} reserved_mb={:.0} process_ws_mb={:.0} process_private_mb={:.0}",
            if budget > 0.0 {
                used / budget * 100.0
            } else {
                0.0
            },
            mb(non_local.CurrentUsage),
            mb(non_local.Budget),
            mb(local.CurrentReservation),
            working_set,
            private,
        );
    }
}

#[cfg(not(windows))]
fn log_gpu_memory(_painter_id: PainterId, _device: usize) {}

/// 이 프로세스의 워킹셋과 커밋(프라이빗) 바이트, MB 단위.
#[allow(unsafe_code)]
#[cfg(windows)]
fn process_memory_mb() -> (f64, f64) {
    use winapi::um::processthreadsapi::GetCurrentProcess;
    use winapi::um::psapi::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};

    // Safety: out 파라미터 하나뿐이고 크기를 정확히 넘긴다.
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        if GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters as *mut _ as *mut _,
            counters.cb,
        ) == 0
        {
            return (0.0, 0.0);
        }
        let mb = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
        (mb(counters.WorkingSetSize), mb(counters.PrivateUsage))
    }
}

#[cfg(not(windows))]
fn process_memory_mb() -> (f64, f64) {
    (0.0, 0.0)
}

// ★`deferred_animation_frame_is_due` 는 2026-10-21 에 삭제됐다.★
//
// 그것이 막던 증상: 페인트측 애니메이션과 스크립트가 같은 간격에 각자 프레임을 내
// ***초당 120 개를 요청하고 60 개를 버렸다***(log_ani_debug 30, divisor=1:
// `took_anim=0 took_script=61 overwritten=61 rode_along=0`, `WRRATE frames=120 onscreen=60`).
// 그래서 "값은 제 자리에서 보내고, 프레임을 낼지는 턴 끝에 정한다" 로 미뤘던 것이다.
//
// ★생산자가 틱 하나면 그 중복이 구조적으로 불가능하다.★ 요청하는 원천이 둘이어야 둘이
// 겹치는데, 애니메이션도 스크립트도 더는 요청하지 않는다 -- 값은 `pump_paint_animation` 이
// 자기 자리에 밀어 넣고, 그 값을 실어 가는 프레임은 같은 주기의 틱이 하나만 낸다
// (`Paint::on_wall_tick`: `gather_components` → `produce_wall_frame`).
// 설계 2-6 / 4 절: docs/superpowers/specs/2026-10-21-single-frame-producer-design.md

// ★`script_frame_is_redundant` 와 `script_frame_rides_the_painter` 는 2026-10-21 에
// 삭제됐다.★
//
// 그것이 막던 증상: 페인트측 애니메이션과 스크립트가 각자 프레임을 내 60Hz 화면에 초당
// 120 개를 요청하고 절반을 버렸다(log_ani_debug 28/30/32 --
// `divisor=1 FRAMEREASON total=120 (ANIMATED_PROPERTY=60 + SCENE=60)`,
// `WRRATE frames=120 onscreen=60`, `ANIMWIN overwritten=61`). 게다가 둘은 같은 값을 싣지
// 않아서(바인딩은 `grid + lead` 의 예측, 디스플레이 리스트는 `built_at` 의 값) 어느 쪽이
// 합성 슬롯을 가져가느냐가 화면 위치를 갈랐다.
//
// ★생산자가 틱 하나면 재현할 수 없다.★ 그 증상은 "두 원천이 한 표출 간격을 두고 맞붙는다"
// 가 전제인데, 맞붙을 둘째 원천이 없다. 스크립트의 `PaintMessage::GenerateFrame` 은
// 아무것도 하지 않고(`Paint::handle_message`), 애니메이션은 값만 밀어 넣는다. 한 표출
// 간격의 프레임은 정확히 틱이 낸 하나다.
//
// 이 술어가 지고 있던 ★ack 해제 부작용★ 은 사라지지 않고 틱으로 옮겨 갔다 --
// `Paint::produce_wall_frame` 이 타일마다 `release_owed_canvas_ack` 를 부른다(설계 2-7).
// 설계 2-6 / 4 절: docs/superpowers/specs/2026-10-21-single-frame-producer-design.md

impl Painter {
    pub(crate) fn new(rendering_context: Rc<dyn RenderingContext>, paint: PainterInputs) -> Self {
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
        let id_manager = paint.webrender_external_image_id_manager;
        let mut external_image_handlers = Box::new(WebRenderExternalImageHandlers::new(id_manager));

        // Set WebRender external image handler for WebGL textures.
        let image_handler = Box::new(WebGLExternalImages::new(
            painter_id,
            paint.webgl_threads,
            rendering_context.clone(),
            paint.swap_chains,
            paint.busy_webgl_contexts_map,
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
                paint.webgpu_image_map,
                rendering_context.clone(),
                webgpu_painter_luid,
            )),
            WebRenderImageHandlerType::WebGpu,
        );

        WindowGLContext::initialize_image_handler(
            &mut external_image_handlers,
            rendering_context.clone(),
        );

        let embedder_to_constellation_sender = paint.embedder_to_constellation_sender;
        let timer_refresh_driver = LazyCell::default();
        // ★표출과 애니메이션 표본이 같은 클럭을 타게 한다.★ `BaseRefreshDriver` 는
        // 이미 설치된 드라이버를 우선 쓰는데, `WebContentAnimator` 는 타이머를 직접
        // 받아 그것을 우회하고 있었다. 그 결과 `gfx_vsync_enabled` 로 표출만 vsync 에
        // 묶으면 값 표본은 자유 구동 타이머에 남아 둘이 매 프레임 어긋났다 -- 주기적
        // 딸꾹질이 내내 떨림으로 바뀐다(실기 확인, log_ani_debug/30).
        //
        // vsync 가 꺼져 있으면 결정 결과가 `TimerRefreshDriver` 이고 그것의
        // `observe_next_frame` 이 `queue_timer(paint_timer_period(), ..)` 이라
        // 종전 동작과 정확히 같다. `unwrap_or_else` 라 vsync 일 때는 타이머 스레드를
        // 띄우지도 않는다.
        let installed_refresh_driver = rendering_context.refresh_driver();
        let refresh_driver = Rc::new(BaseRefreshDriver::new(
            paint.event_loop_waker.clone_box(),
            installed_refresh_driver.clone(),
            &timer_refresh_driver,
        ));
        let animation_refresh_driver_observer = Rc::new(AnimationRefreshDriverObserver::new(
            embedder_to_constellation_sender.clone(),
            painter_id,
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
        let dcomp_native_active = matches!(
            compositor_config,
            webrender::CompositorConfig::Native { .. }
        );

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

        // ★낡음(2026-08-12 기록, 무효 시점 불명 — 당시 webrender 는 트리 밖 0.68)★ — 스크롤바 분기는 `vendor_local/webrender/src/picture.rs:2318-2323`, 기본값은 2325.
        // WR picture cache 타일 크기 오버라이드 (`gfx_wr_picture_tile_size` pref, 구 env
        // SERVO_WR_PICTURE_TILE_SIZE). 빈 문자열이면 WR 기본(콘텐츠 1024x512, 스크롤바는 WR이
        // 자체 특수 크기 분기 — picture.rs:2306-2311)을 그대로 쓴다. 타일 수·무효화 입도·
        // per-tile 오버헤드(DComp bind/unbind 횟수)가 함께 달라진다 — 값이 창 이상이면
        // 슬라이스당 타일 1장이고, `display` 가 정확히 그 상태를 노린다.
        //
        // ★WR 은 이 값을 검사하지도 클램프하지도 않는다★(2026-08-12 확인: render_backend.rs 가
        // ★낡음(2026-08-12 기록, 무효 시점 불명 — 당시 webrender 는 트리 밖 0.68)★ — 실제 자리는 `vendor_local/webrender/src/picture.rs:2313-2316`(2302 는 unused 표시 루프).
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
            let steady_effective_default =
                steady_tile_size_override.unwrap_or(DeviceIntSize::new(1024, 512));
            let alternate = if steady_effective_default.width == 512
                && steady_effective_default.height == 512
            {
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
            last_gpu_memory_log_at: Cell::new(None),
            pending_frame_diagnostics: Default::default(),
            next_diagnostic_frame_id: Cell::new(0),
            last_ready_local_frame_id: Default::default(),
            last_ready_wall_logical_frame_id: Default::default(),
            overlapping_frame_request_count: Default::default(),
            unexpected_frame_ready_count: Default::default(),
            render_count: Default::default(),
            display_composite_in_flight: Default::default(),
            display_composite_in_flight_since: Default::default(),
            present_cadence_start: Default::default(),
            present_cadence_last: Default::default(),
            present_cadence_count: Default::default(),
            present_cadence_max_gap_ms: Default::default(),
            owns_presentation: false,
            paint_proxy: paint.paint_proxy,
            last_render_end: Default::default(),
            canvas_ack_held_since: Default::default(),
            canvas_ack_last_report: Default::default(),
            owed_ack_flush_count: Default::default(),
            owed_ack_last_report: Default::default(),
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
            external_present_budget: Default::default(),
            webrender_api,
            webrender_document,
            webrender_gl,
            last_mouse_move_position: None,
            frame_delayer: Default::default(),
            paint_animation_window_start: Default::default(),
            paint_animation_skipped_busy: Default::default(),
            paint_animation_rode_along: Default::default(),
            paint_animation_held_max: Default::default(),
            last_frame_generated_at: Default::default(),
            last_video_arrived_at: Default::default(),
            last_video_presented_at: Default::default(),
            last_frame_by_other_source_at: Default::default(),
            last_paint_animation_push_at: Default::default(),
            anim_step_previous: Default::default(),
            anim_step_window_start: Default::default(),
            anim_step_dt: Default::default(),
            anim_step_dx: Default::default(),
            anim_ready_queue: Default::default(),
            anim_ready_pending_x: Default::default(),
            anim_ready_previous: Default::default(),
            anim_ready_window_start: Default::default(),
            anim_ready_dx: Default::default(),
            anim_ready_dt: Default::default(),
            anim_ready_latency: Default::default(),
            anim_ready_unmatched: Default::default(),
            anim_win_pending_tag: Default::default(),
            anim_win_took_anim: Default::default(),
            anim_win_took_script: Default::default(),
            anim_win_overwritten: Default::default(),
            anim_win_window_start: Default::default(),
            last_render_started_at: Default::default(),
            lcp_calculator: LargestContentfulPaintCalculator::new(),
            animation_image_cache: FxHashMap::default(),
            pending_video_frame_updates: RefCell::new(FxHashMap::default()),
            web_content_animator: WebContentAnimator::new(
                paint.event_loop_waker.clone_box(),
                (*timer_refresh_driver).clone(),
                installed_refresh_driver
                    .unwrap_or_else(|| (*timer_refresh_driver).clone() as Rc<dyn RefreshDriver>),
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

        // ★값 표본은 렌더가 끈다 -- 여기서는 렌더가 멈췄을 때만 돈다.★
        //
        // ***왜 옮겼나*** -- `perform_updates` 는 `spin_event_loop` 안에서 돌고, 셸은 그것을
        // 엔진이 루프를 깨울 때와 아무 창 이벤트가 올 때 부른다. **렌더는 그 목록에 없다.**
        // 그래서 표본을 뜨는 시점이 "렌더 뒤에 먼저 도착한 아무 spin" 이 되고, 그것이
        // 1ms 뒤의 엔진 깨우기일 수도, 16ms 뒤의 다음 재그리기일 수도 있었다. 엔진 메시지
        // 트래픽은 주기적이지 않고 뭉쳐서 오므로, 잘 맞는 구간이 한참 이어지다 어긋나는
        // 구간이 한참 이어진다 -- 운영자가 보고한 불규칙한 양상이 그것이다.
        //
        // 실측이 이것만 가리켰다(log_ani_debug/37): 렌더 간격은 73 창 중 1 창만 결함인데
        // (dup 0.04/s, skip1 0.00/s) 표본 간격은 25 창이 결함이었다(dup 0.44/s,
        // skip1 0.42/s). 렌더 클럭은 이미 고르고, 망가지는 것은 렌더와 표본 사이다.
        //
        // 이제 `Painter::render` 가 시작하면서 직접 부른다. 표본 간격이 **정의상** 렌더
        // 간격과 같아지고, 이벤트 루프 스케줄링에 의존하지 않으므로 dup/skip 이 구조적으로
        // 불가능해진다. 여기 남은 호출은 **렌더가 멈춘 화면** 전용이다 -- 움직이는 것이
        // 이 애니메이션뿐이면 첫 프레임을 만들 사람이 없고, 그 한 번을 이 자리가 만든다.
        let renderer_idle = self.last_render_started_at.get().is_none_or(|at| {
            Instant::now().duration_since(at) >= crate::refresh_driver::paint_timer_period() * 4
        });
        if renderer_idle {
            #[cfg(windows)]
            crate::output_grid::note_pump_from_idle();
            self.pump_paint_animation();
        }
    }

    /// 애니메이션 값을 뜨고 밀어 넣는다. ★한 번의 렌더에 정확히 한 번.★
    ///
    /// ★낡음(2026-09-18 `ca080974d42` 기록, 같은 날 `79d8875effe` 이후 무효)★ — 호출은 `Painter::render` 의 **끝**(:2444)이다. 시작에서는 프레임이 안 난다(:2422).
    /// 호출처는 둘이고 둘은 배타적이다: `Painter::render` 의 시작(렌더가 돌고 있을 때)과
    /// `perform_updates`(렌더가 멈췄을 때). 안쪽의 `push_due` 게이트가 한 렌더에 두 번
    /// 밀어 넣는 것을 다시 한 번 막는다.
    pub(crate) fn pump_paint_animation(&mut self) {

        // ***One transaction carries every dynamic property, because sending resets them
        // all.*** `reset_dynamic_properties` clears colors, floats and transforms
        // together, so sending the caret's color alone would blank whatever a CSS
        // animation had bound, and the reverse. They are collected together for that
        // reason, not for tidiness.
        let colors = self.web_content_animator.update(&self.webview_renderers);
        // ★B2 — 이 타일이 **자기 출력에 표시될 시각**으로 샘플한다.★
        //
        // 지금까지는 렌더가 끝난 순간의 벽시계로 값을 계산했다. 그 시각은 이 타일의 픽셀이
        // 실제로 화면에 나오는 시각과 무관하다. 네 모니터의 vblank 가 11.2ms 에 흩어져
        // 있으므로, 거의 같은 순간에 샘플한 값을 든 두 타일은 이음매에서 그만큼 어긋난 위치를
        // 보여 주고 그 차이가 매 프레임 흔들린다 -- B1(커밋 시각 정렬)로는 못 고친 부분이고,
        // 실기에서 B1 을 완전히 동작시켜도 저더가 남은 이유다(설계 문서 `## 실기 결과`).
        //
        // 여기서 앞당겨 보면 이음매 오차가 사라지는 대신 **상수가 된다**(속도 × 위상차).
        // 그 거래가 이 pref 의 내용이다. 꺼져 있으면(`-1`, 기본) 예전 그대로 지금 시각이다.
        //
        // ★★두 시계를 절대 섞지 않는다.★★ `now` 는 **진짜 지금**이고 아래의 게이트 판정
        // (`renderer_is_running`, `push_due`, `last_paint_animation_push_at`)이 전부 이것을
        // 쓴다. 그 판정들은 `last_render_started_at` 같은 **실제** 시각과 비교하므로, 여기에
        // 미래 시각을 넣으면 경과 시간이 lead 만큼 부풀어 판정이 뒤집힌다.
        //
        // 처음 구현에서 실제로 그렇게 했고 실기에서 **fps 가 절반으로 보였다**
        // (log_ani_debug_02/06, 137). lead 가 최대 두 주기(33ms)인데
        // `renderer_is_running` 의 창이 `animation_period * 4`(120Hz 기준 33ms)라, 렌더가
        // 멀쩡히 도는데도 "렌더가 멈췄다" 로 읽혀 타이머 폴백 경로로 넘어갔다.
        //
        // 샘플 시각은 오직 `update_paint_animations` 에만 들어간다 -- 그것이 B2 가 바꾸려는
        // 유일한 것이다: **언제 그릴까가 아니라, 어느 시각의 값을 그릴까.**
        // ★★두 시계 읽기 사이에 아무것도 두지 않는다.★★
        //
        // 아래 `sample_at` 은 `now + (target - qpc_now())` 이고 `target` 은 합성 격자 위의
        // 정수배다. ***두 시계를 같은 순간에 읽으면 `now` 가 상쇄되어 `sample_at` 이 정확히
        // `target` 이 된다*** -- 즉 샘플 시각이 격자 위에 떨어져 간격이 완벽히 고르다.
        // 그 사이에 무엇이든 끼면 그만큼 `sample_at` 이 뒤로 밀리고, ***그 일의 소요 시간
        // 변동이 그대로 샘플 간격의 지터가 된다.***
        //
        // `note_dcomp_frame_statistics` 가 바로 거기 있었다. DWM 통계 조회
        // (`DCompositionGetStatistics` + 타깃마다 `GetTargetStatistics`)라 비용이 일정하지
        // 않다. 샘플 시각과는 아무 상관이 없으므로 앞으로 옮긴다.
        //
        // 실측(log_ani_debug_04/09): 프레임당 이동 `dx_px` 의 폭이 4.22px 인데 샘플 간격
        // `dt_ms` 폭 1.13ms 로 설명되는 것은 2.60px 뿐이었다(2,304px/s). 나머지가 이
        // 틈이다. `SAMPLELEAD lead_ms` 와 `SAMPLESLIP phase_all_ms` 의 폭이 둘 다
        // 1.18ms 로 같은 것이 그 방증이다 -- `target` 이 고정이므로 lead 의 변동은 곧
        // 호출 시각의 변동이고, 그것은 상쇄되어야 하는 값이다.
        //
        // ★같은 버그를 `snap_to_dwm_grid_at` 에서 이미 고쳤다★ -- anchor 를 QPC 읽기 바로
        // 옆에서 읽게 바꿔 missed present 가 3~4 배 줄었다. 같은 형태가 여기 남아 있었다.
        #[cfg(windows)]
        self.note_dcomp_frame_statistics();
        let now = Instant::now();
        let sample_at = self.paint_animation_sample_at().unwrap_or(now);
        let mut floats = Vec::new();
        let mut transforms = Vec::new();
        let mut still_animating = false;
        for renderer in self.webview_renderers.values() {
            renderer.for_each_connected_pipeline(&mut |pipeline_details| {
                still_animating |= pipeline_details.animations.update_paint_animations(
                    sample_at,
                    &mut floats,
                    &mut transforms,
                );
            });
        }
        let floats_log: Vec<f32> = floats.iter().map(|property| property.value).collect();
        // ***The transform values too.*** The application animates `transform`, and a log
        // that only prints floats reports `floats=[]` for it -- which reads as "nothing is
        // animating" when the truth is "the thing that is animating is not being printed".
        // Cost a diagnostic round on 2026-09-08 (log_ani_perf/11).
        // ★어느 요소의 값인지 같이 찍는다.★ 값만 늘어놓으면 순서가 어디서 왔는지 알 수
        // 없어 "어느 컨테이너가 어느 방향으로 움직였나" 를 귀속시킬 수 없다. 전환 때 사용자가
        // 보는 것이 바로 그 질문이다 -- 들어와야 할 구성이 나가는 쪽으로 움직인다는 보고를
        // 이 줄로는 확인할 수 없었다(2026-09-17).
        //
        // 키는 노드 id 의 해시라 역산은 안 되지만 노드마다 고정이므로, `ANIMSTART node=N` 의
        // 해시를 같은 식으로 계산하면 맞출 수 있다(`paint_animation_binding_key`).
        let transforms_log: Vec<(u32, f32, f32, f32)> = transforms
            .iter()
            .map(|property| {
                let matrix = property.value.to_array();
                (property.key.id.uid, matrix[12], matrix[13], matrix[0])
            })
            .collect();

        // ***An animation is a value laid onto the frames this wall already produces, not
        // a reason to produce more.*** The painter renders and presents on a clock, and
        // video sources are composited with whatever frame they happen to have; the
        // animated value belongs in that same pass. Generating a frame *for* the animation
        // duplicates one that was coming anyway, and measured on the 4-GPU wall,
        // 2026-09-08 (log_ani_perf/09), 1410 of 2166 such frames were queued on top of a
        // frame still in flight -- which is what made the animation move, stall and jump.
        //
        // So the values go out every refresh period regardless, and a frame is generated
        // only when nobody else has produced one within that period: a page whose only
        // moving thing is a CSS animation still advances, and a wall full of video pays
        // nothing.
        let animation_period = crate::refresh_driver::paint_timer_period();
        let has_values = !floats.is_empty() || !transforms.is_empty();
        // ★박자는 밖에서 온다★ — 값을 밀어넣는 주기를 자기 타이머로 재면 벽의 표출 주기와
        // 위상이 어긋나 미끄러지고, 그 미끄러짐이 그대로 프레임 초과분이 된다. 값이 쓸모를
        // 갖는 시점은 프레임이 그려질 때이므로, 그 박자를 그대로 따라간다: **마지막 밀어넣기
        // 이후 렌더가 한 번이라도 시작됐으면** 다음 것을 밀어넣는다.
        //
        // 아무도 렌더하지 않는 페이지(움직이는 것이 이 애니메이션뿐인 화면)에서는 그 신호가
        // 영영 오지 않으므로 주기 타이머로 받쳐 준다 — 그때는 애니메이션이 스스로 프레임을
        // 내야 렌더가 생기고, 그 첫 한 번을 이 폴백이 만든다.
        let last_push = self.last_paint_animation_push_at.get();
        let rendered_since_push = match (self.last_render_started_at.get(), last_push) {
            (Some(rendered), Some(pushed)) => rendered > pushed,
            (Some(_), None) => true,
            (None, _) => false,
        };
        // ★타이머 폴백은 "아무도 렌더하지 않을 때" 전용이다.★ 렌더가 돌고 있는데도 이
        // disjunct 가 같은 주기로 같이 돌면 **두 시계가 맥놀이한다.** 타이머가 앞서면
        // 렌더 없이 표본이 하나 나가고(그 간격에 둘이 들어간다), 그 대가로 다음 간격이
        // 빈다. 1x(2,304px/s)에서 빈 간격은 화면에서 38.4px 이 아니라 **76.8px 로 튄다.**
        //
        // 실측(log_ani_debug/36): `dup` 과 `skip1` 이 **정확히 짝으로** 나온다 -- 1/1 이
        // 20 창, 2/2 가 3 창, 3/3 이 1 창. 무작위 손실이 아니라 맥놀이의 서명이다. 빈도는
        // 초당 0.6 회(≈1.7 초에 한 번)이고 73 창 중 48% 에 결함이 있었다. 두 시계의 위상이
        // 가까워지는 구간에서 결함이 몰리는 것이 "재생 중 품질이 변동" 으로 보고된 현상이다.
        //
        // 그래서 렌더가 돌고 있는 동안에는 **렌더만이** 표본을 끈다. `rendered_since_push`
        // 하나면 간격이 렌더 간격과 같아지고 dup/skip 이 구조적으로 불가능해진다.
        // 폴백은 렌더가 한동안 없을 때만 살아난다 -- 움직이는 것이 이 애니메이션뿐인
        // 화면에서 첫 프레임을 만드는 것이 그 자리이고, 그 뒤로는 자기가 낸 프레임이
        // 렌더를 만들므로 `rendered_since_push` 가 이어받는다.
        //
        // 네 주기로 잡은 것은 한두 프레임 늦는 렌더를 "렌더가 없다" 로 오판하지 않기
        // 위해서다. 진짜로 멈추면 67ms 뒤에 폴백이 받는다.
        let renderer_is_running = self
            .last_render_started_at
            .get()
            .is_some_and(|at| now.duration_since(at) < animation_period * 4);
        let push_due = rendered_since_push
            || (!renderer_is_running
                && last_push.is_none_or(|last| now.duration_since(last) >= animation_period));
        // ★"남"에서 자기를 뺀다★ — 여기서 `last_frame_generated_at`(자기 프레임도 찍힌다)을
        // 보고 있었던 것이 위 주석의 규칙을 무너뜨렸다. 애니메이션이 프레임을 하나 내면 그
        // 시각이 찍히고, 다음 밀어넣기가 그것을 "남이 내고 있다"로 읽는다. 두 판정이 서로
        // 다른 위상의 같은 주기로 도니 맞물렸다 어긋났다 하면서 초당 10~13 개의 여분 프레임이
        // 샜다(60Hz 벽에서 렌더 61 -> 72, log_presentation/00).
        //
        // ★판정을 시간 창에서 렌더 클럭으로 옮긴다(2026-09-17).★ 전에는 "한 주기 반 안에
        // 남의 프레임이 있었나" 로 봤는데, 그 창이 넓어 **낼 수 있는 프레임을 못 내고 있었다.**
        //
        // 실측(log_ani_debug/27, 412 창): 애니메이션 값은 초당 60.2 번 계산되는데 문서로
        // 발행된 것은 54.0 뿐이었다. 스크립트가 초당 15.2 프레임을 내고 그 하나하나가 1.5
        // 주기를 막으니 22.8 주기가 막혔고(15.2 x 1.5), 애니메이션 자체 프레임은 37.2 예상 /
        // 38.6 실측으로 모델이 그대로 맞았다. 잃은 6.2 개는 다음 밀어넣기가 값을 덮어써
        // 화면에 닿지 못했다. 화면에 도달한 위치는 초당 45.4 개인데 표출은 60.6 번이라,
        // 표출 넷 중 하나가 직전 위치를 다시 그렸다 -- 등속 운동이 떨려 보이는 것이 이것이다.
        //
        // ★창을 한 주기로 좁히는 것은 답이 아니다.★ `raf_is_driving_composites` 의 주석이
        // 이미 그 이유를 적어 두었다 -- 60fps 로 도는 남의 프레임은 간격이 한 주기에 닿을 수
        // 있어 판정이 매번 뒤집히고, 그 뒤집힘이 이 게이트가 애초에 막으려던 지터다. 창을
        // 조정하는 한 위상 문제는 남는다. 그래서 **위상을 없앤다.**
        //
        // 물음은 원래 "남이 이번 합성분의 프레임을 이미 냈는가" 이고, 그 경계는 시각이 아니라
        // **마지막 렌더가 시작된 순간**이다(`Painter::render` 맨 앞에서 매 패스 찍힌다).
        // 그 뒤에 남이 낸 프레임은 아직 합성되지 않았으니 다음 합성이 실어 간다 -- 애니메이션은
        // 값만 얹으면 된다. 그 전에 난 프레임은 이미 소비됐으니 이번 주기는 비어 있고,
        // 애니메이션이 내야 한다.
        //
        // `push_due` 가 쓰는 신호와 같은 것이므로 두 판정이 **같은 위상**으로 돈다 -- 위
        // 문단이 말한 "서로 다른 위상의 같은 주기" 가 구조적으로 불가능해진다.
        let someone_else_is_producing = match (
            self.last_frame_by_other_source_at.get(),
            self.last_render_started_at.get(),
        ) {
            (Some(other), Some(rendered)) => other > rendered,
            // 렌더가 아직 없으면 얹혀 갈 합성도 없다. 남이 낸 적이 없어도 마찬가지다.
            _ => false,
        };
        let painter_busy = self.pending_frames.get() > 0 || self.renderer_behind();
        // ★두 계수기는 남는다 -- 생산 판정이 아니라 ***표본의 사정***이기 때문이다.★
        // 값은 생산과 무관하게 매 주기 나가고(불변식 7), 이 둘은 그 주기에 이 타일이 어떤
        // 상태였는지를 적는다. 여기 있던 생산 판정(`animated_property_frame`)은 2026-10-21
        // 에 삭제됐다 -- 설계 2-6.
        if has_values && push_due && painter_busy && !someone_else_is_producing {
            self.paint_animation_skipped_busy
                .set(self.paint_animation_skipped_busy.get() + 1);
        }
        if has_values && push_due && someone_else_is_producing {
            self.paint_animation_rode_along
                .set(self.paint_animation_rode_along.get() + 1);
        }

        // ***이 전송이 남의 값을 지우지 않게 한다.***
        //
        // `reset_dynamic_properties` 는 이번에 실린 키만 남기고 나머지를 전부 지우며, 지워진
        // 키는 바인딩에 구워진 기본값 -- 디스플레이 리스트를 만든 순간의 값 -- 으로 돌아간다.
        // 그래서 엇갈려 끝나는 애니메이션들 중 먼저 끝난 것은, 아직 도는 것이 값을 밀 때마다
        // 시작값으로 되돌아간다. 페이드인이면 그게 검은 화면이고, 다음 디스플레이 리스트가 올
        // 때까지 그대로 있는다.
        //
        // 그러니 끝난 값을 여기서 도로 얹는다. 판정(`has_values`/`push_due`)은 이미 위에서
        // 끝났으므로 이 값들은 전송을 만들지도, 프레임을 만들지도 않는다 -- 나가기로 정해진
        // 전송에 얹혀 갈 뿐이다.
        let mut held_values = 0usize;
        if colors.is_some() || (has_values && push_due) {
            for renderer in self.webview_renderers.values() {
                renderer.for_each_connected_pipeline(&mut |pipeline_details| {
                    held_values += pipeline_details
                        .animations
                        .append_held_paint_values(&mut floats, &mut transforms);
                });
            }

            let mut transaction = Transaction::new();
            transaction.reset_dynamic_properties();
            transaction.append_dynamic_properties(DynamicProperties {
                transforms,
                floats,
                colors: colors.unwrap_or_default(),
            });
            // ★값은 지금 보내고, 프레임은 틱이 낸다.★
            //
            // 값이 먼저 나가야 같은 주기의 틱 생산이 ***최신 값***을 실어 간다 -- 그 순서를
            // `Paint::on_wall_tick` 이 `gather_components` → `produce_wall_frame` 로 못박고
            // 있다(불변식 8·9). 여기서 프레임을 정하던 자리(`deferred_animation_frame`)는
            // 2026-10-21 에 사라졌다.
            self.send_transaction(transaction);
            if has_values && push_due {
                // ★표를 붙이는 조건은 "값을 밀어 넣었나" 다.★ 전에는 "애니메이션이 자기
                // 프레임을 낼 때" 였는데, 생산자가 틱 하나가 된 뒤로 그 조건은 영영 거짓이다.
                // 안 붙이면 `ANIMREADY` 의 dt/dx 가 통째로 비어 -- 실기 판정의 "dx 공칭값" 이
                // 사라진다.
                self.anim_ready_pending_x
                    .set(transforms_log.first().map(|&(_, x, _, _)| x));
                self.last_paint_animation_push_at.set(Some(now));
                // ★간격을 잰다 -- 개수가 아니라.★ 이 자리가 값 표본을 뜨는 시점이고,
                // 사용자가 보고한 현상("순간마다 이동하는 정도가 다르다")은 정확히
                // 이 표본들의 간격 문제다. 초당 개수는 60 인데 화면이 매끄럽지 않다면
                // 남는 설명은 간격뿐이다 -- 60Hz 패널에 60 개를 고르게 넣으면
                // 매끄러워야 한다(패널 주사율은 실측 확인, log_ani_debug/35).
                self.note_animation_step(now, &transforms_log);
            }
        }

        // ★비디오 분기는 2026-10-21 에 삭제됐다 -- 규칙 2 위반 그 자체였다.★
        //
        // 여기 있던 것은 "비디오가 기다리고 이 주기에 아무도 안 냈으면 낸다" 였다. 그 판정이
        // 고치려던 증상은 ***생산자가 갈리는 자리***다 -- 애니메이션이 도는 동안은 애니메이션이
        // 프레임을 내고 끝나면 `update_images` 의 도착 경로가 이어받으며, 그 갈아타는 순간이
        // 화면에서 한 박자 끊김으로 보였다. ★생산자가 틱 하나면 갈릴 상대가 없다.★
        //
        // 비디오가 굶지도 않는다. 틱의 생산이 `flush_pending_video_frame_updates` 를 무조건
        // 거치므로(`generate_frame_with_diagnostic_id`), 쌓인 프레임은 늦어도 다음 틱에 실려
        // 나간다 -- 바닥과 천장을 같은 틱이 준다(불변식 4).
        // 설계 2-6 / 4 절: docs/superpowers/specs/2026-10-21-single-frame-producer-design.md
        let video_pending = !self.pending_video_frame_updates.borrow().is_empty();

        // Nothing else may be waking this painter: the point of these animations is that
        // they run while script is not producing anything.
        //
        // 비디오가 기다리는 동안에도 계속 깨운다 — 값 표본이 주기마다 돌아야 애니메이션이
        // 끝난 뒤에도 같은 자리에서 이어진다.
        if still_animating || video_pending {
            self.web_content_animator.wake_for_paint_animation();
            // ★그 깨우기를 쏘는 것은 임베더의 틱이다.★ 위 호출은 설치된 드라이버에 콜백을
            // 쌓을 뿐이고, 자기 클럭이 없는 드라이버(셸이 틱에서 쏘는 종류)에서는 그 틱을
            // 낼 깨우기도 여기서 걸어 줘야 한다 -- 안 걸면 `RefreshDriverObserver` 가 하나도
            // 없는 동안(스크립트가 아무것도 안 내는 바로 그 상황)에 이 애니메이션이 선다.
            self.refresh_driver.arm_paint_timer_heartbeat();
            // ★깨우기는 프레임을 내지 않는다 -- 다음 틱 에지에 생산 1 회를 예약할 뿐이다.★
            // 자유 실행 표출 클럭이 없는 임베더(servoshell / WPT / Android)에서 이 비트가
            // 없으면 `on_demand_wall_tick` 이 집어 갈 것이 없어 애니메이션과 비디오가 선다.
            // 자유 실행 클럭이 있는 구성은 이 비트를 보지 않고 매 주기 생산한다(설계 2-4).
            crate::wall_tick::schedule_production_at_next_edge();
        }
        self.paint_animation_held_max
            .set(self.paint_animation_held_max.get().max(held_values));
        self.log_paint_animation_activity(
            now,
            still_animating,
            &floats_log,
            &transforms_log,
        );
    }

    /// ★합성 슬롯을 어느 원천이 가져갔나.★ `anim_win_pending_tag` 참고.
    fn note_animation_win(&self) {
        let now = Instant::now();
        let window_start = *self.anim_win_window_start.borrow_mut().get_or_insert(now);
        match self.anim_win_pending_tag.take() {
            Some(true) => self.anim_win_took_anim.set(self.anim_win_took_anim.get() + 1),
            Some(false) => self
                .anim_win_took_script
                .set(self.anim_win_took_script.get() + 1),
            // 새 완료 없이 도는 렌더. 같은 내용을 다시 그린다.
            None => {},
        }
        if now.duration_since(window_start) < Duration::from_secs(1) {
            return;
        }
        *self.anim_win_window_start.borrow_mut() = Some(now);
        let anim = self.anim_win_took_anim.replace(0);
        let script = self.anim_win_took_script.replace(0);
        let overwritten = self.anim_win_overwritten.replace(0);
        log::warn!(
            "ANIMWIN painter={:?} took_anim={anim} took_script={script} \
             overwritten={overwritten} holding_ack={}",
            self.painter_id,
            // ★이것이 참인 채로 머물면 스크립트의 rAF 가 잠긴 것이다.★ 해제가 생산 경로에
            // 붙어 있던 시절에 그 경로가 ack 를 안 풀어 실제로 멎은 적이 있다
            // (log_ani_debug 36). 한 줄로 드러내 둔다. `script_rode_painter` 는
            // 2026-10-21 에 빠졌다 -- 그 게이트가 사라졌다.
            self.frame_delayer.is_holding_ack(),
        );
    }

    /// ★실제로 프레임이 된 값만 센다.★
    ///
    /// `note_animation_step` 의 짝이다. 그쪽은 `update_paint_animations` 가 채운 값을
    /// ***보내는 순간*** 재고, 이쪽은 WebRender 가 그 요청의 프레임을 다 만들었다고 알려 온
    /// 순간 잰다. 둘 사이에 `WRRenderBackend` / `WRSceneBuilder` 스레드가 있고, 그 큐가 언제
    /// 비는지는 우리 페이싱 스레드도 커밋 스케줄러도 정하지 않는다.
    ///
    /// 요청과 완료는 순서가 보장되므로 줄의 앞에서 꺼내면 된다 -- `pending_frames` 가 이미
    /// 그 전제로 돌아간다.
    fn note_animation_ready(&self) {
        let Some((_, x, requested_at)) = self.anim_ready_queue.borrow_mut().pop_front() else {
            return;
        };
        let now = Instant::now();
        // ★렌더가 집어가기 전에 덮이면 그 위치는 화면에 못 간다.★
        if self.anim_win_pending_tag.get() == Some(true) {
            self.anim_win_overwritten
                .set(self.anim_win_overwritten.get() + 1);
        }
        self.anim_win_pending_tag.set(Some(x.is_some()));
        let window_start = *self.anim_ready_window_start.borrow_mut().get_or_insert(now);
        match x {
            Some(x) => {
                self.anim_ready_latency
                    .borrow_mut()
                    .push(now.duration_since(requested_at).as_secs_f64() * 1000.0);
                if let Some((previous_x, previous_at)) = self.anim_ready_previous.get() {
                    self.anim_ready_dt
                        .borrow_mut()
                        .push(now.duration_since(previous_at).as_secs_f64() * 1000.0);
                    self.anim_ready_dx.borrow_mut().push((x - previous_x) as f64);
                }
                self.anim_ready_previous.set(Some((x, now)));
            },
            None => self
                .anim_ready_unmatched
                .set(self.anim_ready_unmatched.get() + 1),
        }

        if now.duration_since(window_start) < Duration::from_secs(1) {
            return;
        }
        *self.anim_ready_window_start.borrow_mut() = Some(now);
        let mut dt: Vec<f64> = self.anim_ready_dt.borrow_mut().drain(..).collect();
        let mut dx: Vec<f64> = self.anim_ready_dx.borrow_mut().drain(..).collect();
        let mut latency: Vec<f64> = self.anim_ready_latency.borrow_mut().drain(..).collect();
        let unmatched = self.anim_ready_unmatched.replace(0);
        if dt.len() < 2 {
            return;
        }
        // ★정렬 전에 찍는다.★ `ANIMSTEPSEQ` 와 같은 이유 -- 아래가 제자리 정렬이다.
        if *LOG_ANIM_STEP_SEQ {
            log::warn!(
                "ANIMREADYSEQ painter={:?} n={} dt_ms=[{}] dx_px=[{}]",
                self.painter_id,
                dt.len(),
                format_step_sequence(&dt),
                format_step_sequence(&dx),
            );
        }
        dt.sort_by(|a, b| a.partial_cmp(b).unwrap());
        dx.sort_by(|a, b| a.partial_cmp(b).unwrap());
        latency.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let at = |sorted: &[f64], p: f64| -> f64 {
            sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
        };
        let period_ms = crate::refresh_driver::paint_timer_period().as_secs_f64() * 1000.0;
        let band = |low: f64, high: f64| -> usize {
            dt.iter()
                .filter(|&&v| v >= low * period_ms && v < high * period_ms)
                .count()
        };
        let dx_median = at(&dx, 0.50);
        let spread = if dx_median.abs() > f64::EPSILON {
            (at(&dx, 0.95) - at(&dx, 0.05)) / dx_median.abs()
        } else {
            0.0
        };
        log::warn!(
            "ANIMREADY painter={:?} n={} per_interval dup={} one={} skip1={} skipN={} \
             dt_ms p05={:.2} p50={:.2} p95={:.2} max={:.2} \
             dx_px p05={:.2} p50={:.2} p95={:.2} max={:.2} spread={:.3} \
             lat_ms p50={:.2} p95={:.2} max={:.2} unmatched={}",
            self.painter_id,
            dt.len(),
            band(0.0, 0.5),
            band(0.5, 1.5),
            band(1.5, 2.5),
            dt.iter().filter(|&&v| v >= 2.5 * period_ms).count(),
            at(&dt, 0.05),
            at(&dt, 0.50),
            at(&dt, 0.95),
            at(&dt, 1.00),
            at(&dx, 0.05),
            at(&dx, 0.50),
            at(&dx, 0.95),
            at(&dx, 1.00),
            spread,
            at(&latency, 0.50),
            at(&latency, 0.95),
            at(&latency, 1.00),
            unmatched,
        );
    }

    /// ★한 표본과 다음 표본 사이의 **간격**을 잰다.★
    ///
    /// ***왜 이것이 없으면 안 되는가*** -- 이 추적은 열 라운드 동안 전부 **초당 개수**만
    /// 셌다(발행/합성/표출/화면 도달). 그 값들이 전부 60 에 맞았는데도 화면은 매끄럽지
    /// 않았고, 패널은 실측 60Hz 다(log_ani_debug/35). 60Hz 패널에 초당 60 개의 위치를
    /// **고르게** 넣으면 매끄러워야 하므로, 남는 설명은 고르지 않다는 것뿐이다.
    /// 사용자가 처음부터 말한 "순간마다 이동하는 정도가 다르게 보인다" 가 그 말이었다.
    ///
    /// 두 분포를 같이 낸다. 등속 애니메이션에서는 둘 다 한 점에 모여야 한다:
    ///
    ///   * `dt` -- 표본 시각의 간격. 이것이 흔들리면 **표본을 뜨는 박자**가 문제다.
    ///   * `dx` -- 그 사이 값의 변화량. `dt` 는 고른데 `dx` 가 흔들리면 **값 함수**가
    ///     문제다(보간·세그먼트 경계·외삽).
    ///
    /// 둘을 갈라 놓는 것이 요점이다. 개수만으로는 이 둘을 구분할 수 없고, 지금까지
    /// 세운 가설 셋이 전부 `dt` 쪽만 건드렸다.
    ///
    /// `dx` 는 첫 번째 변환 키 하나만 따라간다 -- 프로브의 1x 행이면 충분하고, 키가
    /// 바뀌면(요소가 갈리면) 그 표본은 버린다.
    fn note_animation_step(&self, now: Instant, transforms: &[(u32, f32, f32, f32)]) {
        if transforms.is_empty() {
            return;
        }
        let window_start = *self.anim_step_window_start.borrow_mut().get_or_insert(now);
        // ★레인마다 따로 센다.★ 키는 노드 id 의 해시라 레인을 가른다. 한 레인만 보면
        // 다른 레인의 튐을 통째로 놓친다 -- `anim_step_previous` 주석.
        {
            let mut previous_by_lane = self.anim_step_previous.borrow_mut();
            let mut dt_out = self.anim_step_dt.borrow_mut();
            let mut dx_out = self.anim_step_dx.borrow_mut();
            for &(key, x, _, _) in transforms {
                if let Some(&(previous_x, previous_at)) = previous_by_lane.get(&key) {
                    dt_out.push(now.duration_since(previous_at).as_secs_f64() * 1000.0);
                    dx_out.push((x - previous_x) as f64);
                }
                previous_by_lane.insert(key, (x, now));
            }
        }
        let lanes = transforms.len();

        if now.duration_since(window_start) < Duration::from_secs(1) {
            return;
        }
        *self.anim_step_window_start.borrow_mut() = Some(now);
        let mut dt: Vec<f64> = self.anim_step_dt.borrow_mut().drain(..).collect();
        let mut dx: Vec<f64> = self.anim_step_dx.borrow_mut().drain(..).collect();
        if dt.len() < 2 {
            return;
        }
        // ★정렬 전에 찍는다.★ 아래 분위수 계산이 두 벡터를 제자리에서 정렬하므로, 여기를
        // 지나면 도착 순서가 사라진다 -- 이 계측이 존재하는 이유가 그 순서다.
        if *LOG_ANIM_STEP_SEQ {
            log::warn!(
                "ANIMSTEPSEQ painter={:?} n={} dt_ms=[{}] dx_px=[{}]",
                self.painter_id,
                dt.len(),
                format_step_sequence(&dt),
                format_step_sequence(&dx),
            );
        }
        dt.sort_by(|a, b| a.partial_cmp(b).unwrap());
        dx.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let at = |sorted: &[f64], p: f64| -> f64 {
            // nearest-rank. 표본이 60 개 남짓이라 보간할 이유가 없다.
            let index = (((sorted.len() - 1) as f64) * p).round() as usize;
            sorted[index]
        };
        // ★판정은 이 한 값이다.★ 등속 운동에서 프레임당 이동량이 중앙값 대비 얼마나
        // 흔들리는가. 0 이면 완벽하고, 1.0 이면 어떤 프레임은 제자리이고 어떤 프레임은
        // 두 배로 튄다는 뜻이다 -- 그것이 화면에서 떨림으로 보인다.
        // ★한 표시 간격에 표본이 몇 개 들어갔나.★ 이것이 사용자가 물은 것이고,
        // 1x(2,304px/s)에서 한 스텝은 60Hz 기준 38.4px 다. 하나를 건너뛰면 화면에서
        // 76.8px 로 튀고, 둘이 한 간격에 몰리면 그중 하나는 영영 안 보인다.
        // 실기 보고가 "30px 보다 크게 튄다" 이므로 여기가 0 이 아니어야 한다.
        let period_ms = crate::refresh_driver::paint_timer_period().as_secs_f64() * 1000.0;
        let band = |low: f64, high: f64| -> usize {
            dt.iter().filter(|&&v| v >= low * period_ms && v < high * period_ms).count()
        };
        let dup = band(0.0, 0.5);
        let one = band(0.5, 1.5);
        let skip1 = band(1.5, 2.5);
        let skipn = dt.iter().filter(|&&v| v >= 2.5 * period_ms).count();

        let dx_median = at(&dx, 0.50);
        let spread = if dx_median.abs() > f64::EPSILON {
            (at(&dx, 0.95) - at(&dx, 0.05)) / dx_median.abs()
        } else {
            0.0
        };
        log::warn!(
            "ANIMSTEP painter={:?} lanes={lanes} n={} per_interval dup={} one={} skip1={} skipN={} \
             dt_ms p05={:.2} p50={:.2} p95={:.2} max={:.2} \
             dx_px p05={:.2} p50={:.2} p95={:.2} max={:.2} spread={:.3}",
            self.painter_id,
            dt.len(),
            dup,
            one,
            skip1,
            skipn,
            at(&dt, 0.05),
            at(&dt, 0.50),
            at(&dt, 0.95),
            dt[dt.len() - 1],
            at(&dx, 0.05),
            dx_median,
            at(&dx, 0.95),
            dx[dx.len() - 1],
            spread,
        );
    }

    /// One `PAINTANIM` line a second while anything is playing.
    ///
    /// ★`frames` 는 2026-10-21 에 사라졌다.★ 그것은 "페인트 스레드가 디스플레이 리스트 없이
    /// 스스로 낸 프레임" 의 수였고, 생산자가 틱 하나가 된 뒤로 구조적으로 0 이다. 남는 것은
    /// `rode_along` -- 값이 틱의 프레임에 얹혀 간 횟수이고, 그것이 건강한 수다.
    fn log_paint_animation_activity(
        &self,
        now: Instant,
        animating: bool,
        values: &[f32],
        transforms: &[(u32, f32, f32, f32)],
    ) {
        let started = *self
            .paint_animation_window_start
            .borrow_mut()
            .get_or_insert(now);
        if now.duration_since(started) < Duration::from_secs(1) {
            return;
        }
        let held = self.paint_animation_held_max.replace(0);
        *self.paint_animation_window_start.borrow_mut() = Some(now);
        // ***held 가 0이 아니면 아무것도 안 움직여도 찍는다.*** 화면이 검어지는 구간은 정확히
        // 아무 애니메이션도 안 도는 구간이라, `animating` 만 보고 입을 다물면 이 진단은 자기가
        // 설명해야 할 상태에서만 말이 없어진다.
        if !animating && held == 0 {
            return;
        }
        // `warn!` deliberately: the wall launcher's RUST_LOG leads with `warn`, and a
        // diagnostic nobody can see is a diagnostic that does not exist.
        // ***The values, not just the frame count.*** A frame count says the painter did
        // work; it does not say the work changed anything. Measured on the 4-GPU wall,
        // 2026-09-08 (log_ani_perf/08): frames flowed at 33-53 a second through every
        // script block and the picture did not move, and from the log alone there was no
        // way to tell a frozen value from a value that never reached the screen.
        let sample = values
            .iter()
            .take(3)
            .map(|value| format!("{value:.3}"))
            .collect::<Vec<_>>()
            .join(",");
        let skipped = self.paint_animation_skipped_busy.replace(0);
        let rode_along = self.paint_animation_rode_along.replace(0);
        // `rode_along` is the healthy number on a wall that is already drawing: the values
        // went out and somebody else's frame carried them. `frames` is what this animation
        // had to produce on its own, and on a page full of video it should be near zero.
        // Translation x/y and the x scale: enough to see a slide or a zoom move, without
        // printing sixteen numbers per animation.
        // `key` 는 노드마다 고정인 해시라, 값이 어느 요소의 것인지 이것으로 갈린다.
        // 상한을 6 으로 둔다 -- 이 페이지는 애니메이션되는 요소가 두셋이고, 3 에서 잘리면
        // 정작 궁금한 셋째가 사라진다.
        let transform_sample = transforms
            .iter()
            .take(6)
            .map(|(key, x, y, scale)| format!("{key:x}:{x:.1}/{y:.1}@{scale:.2}"))
            .collect::<Vec<_>>()
            .join(",");
        // ★값만으로는 타일을 비교할 수 없다 — 값을 만든 시각이 같이 있어야 한다.★
        //
        // 이 줄은 painter 마다 자기 1 초 창으로 나가고(`paint_animation_window_start` 가
        // painter 별이다), 로그 타임스탬프는 초 단위다. 그래서 2026-09-17 에 네 타일의 x 를
        // 나란히 놓고 "PainterId(1) 이 74px 뒤처진다" 고 읽었는데, 그 차이가 화면상의
        // 어긋남인지 **네 줄이 서로 다른 순간에 찍힌 것뿐인지 구분할 방법이 없었다.**
        // painter 1 이 먼저 올라오고 2/3/4 가 거의 동시에 올라오면, 창 위상이 그만큼
        // 어긋나 똑같은 모양이 나온다.
        //
        // 그래서 공유 시계(`CrossProcessInstant`, 프로세스·타일을 가로질러 비교 가능)의
        // 시각을 값과 함께 낸다. 이 함수는 샘플 직후에 불리고 `now` 가 그 샘플의 시각이므로,
        // 여기서 읽는 값은 샘플 시각과 1ms 안쪽이다. 이제 (t, x) 를 네 타일에 대해 찍어
        // 같은 직선 위에 있는지 보면 된다 -- 있으면 어긋남은 없고 로그 위상 차이였을 뿐이며,
        // 없으면 그때 비로소 실제 어긋남이다.
        let t_ms =
            (CrossProcessInstant::now() - CrossProcessInstant::epoch()).as_seconds_f64() * 1000.0;
        warn!(
            "PAINTANIM painter={:?} t_ms={:.1} playing={} rode_along={} skipped_busy={} held={}              floats=[{}] transforms=[{}]",
            self.painter_id,
            t_ms,
            animating,
            rode_along,
            skipped,
            held,
            sample,
            transform_sample
        );
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
            render_point.x >= 0.0
                && render_point.y >= 0.0
                && render_point.x < size.width as f32
                && render_point.y < size.height as f32
        })
    }

    pub(crate) fn webview_renderer_mut(
        &mut self,
        webview_id: WebViewId,
    ) -> Option<&mut WebViewRenderer> {
        self.webview_renderers.get_mut(&webview_id)
    }

    /// WebRender 에 보냈는데 아직 안 돌아온 프레임이 있는가. 스크린샷 두 경로가 이것으로
    /// 기다린다(`screenshot.rs`).
    ///
    /// ★`FrameDelayer::pending_frame` 분리절은 2026-10-21 에 빠졌다.★ 그 플래그는
    /// "스크립트가 프레임을 요청했고 아직 안 났다" 였고, 세우는 자리가
    /// `generate_frame_for_script` 하나라 그 함수와 함께 사라졌다 -- 남겨 두었어도 영영
    /// 거짓이다.
    ///
    /// ★틱이 매 주기 생산하면 이 값이 거의 항상 참이 되어 `--capture` 가 안 찍힌다★
    /// (불변식 16). 그것은 여기서 거르는 문제가 아니라 ***생산을 쉬는*** 문제이고,
    /// Task 9 가 `tick_should_produce` 로 처리한다(설계 2-8).
    pub(crate) fn has_pending_frames(&self) -> bool {
        self.pending_frames.get() != 0
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

    /// The `WebView`s this painter should ask animation ticks for.
    ///
    /// ***One tick per wall frame, not one per tile.*** A fan-out paints one logical
    /// `WebView` into several painters, and each painter runs its own refresh driver, so
    /// every tile asking for a tick turns one animation into four ticks a frame -- 240 a
    /// second on a four-tile 60Hz wall. Each tick becomes a rendering update and each
    /// rendering update reflows the animating document: measured on the 4-GPU wall,
    /// 2026-09-08 (log_ani_perf/12), 49 reflows a second with no animation against 260-285
    /// with one. During a content switch a reflow there costs 12-18 ms, so the script
    /// thread fell behind, display lists backed up, and frame production collapsed to zero
    /// for a second and a half.
    ///
    /// The tiles of one wall frame are one tick, and the painter that owns the `WebView`
    /// is the one that speaks for it. Every other painter stays quiet, and a shell with a
    /// single painter is unaffected because it owns everything it paints.
    pub(crate) fn animating_webviews(&self) -> Vec<(WebViewId, bool)> {
        self.webview_renderers
            .values()
            .filter_map(|webview_renderer| {
                if webview_renderer.animating()
                    && PainterId::from(webview_renderer.id) == self.painter_id
                {
                    // ★★"rAF 가 걸려 있으면 페인트측으로 치지 말자" 는 틀렸다.★★
                    //
                    // 한 번 그렇게 고쳤다가(87f593db7d2) 되돌렸다. 논리는 그럴듯했다 --
                    // 틱을 줄여도 되는 근거가 "화면의 값이 페인트측 예측에서 나온다" 인데
                    // rAF 에는 예측할 식이 없으니 줄이면 안 된다는 것이다. 맞는 말이지만
                    // ***"rAF 콜백이 있다" 와 "rAF 가 화면을 그린다" 는 다르고, 엔진은 그
                    // 둘을 구별할 수 없다.***
                    //
                    // `wall_anim_jitter_probe.html` 의 rAF 는 ***자기 호출 간격을 재서 1 초에
                    // 한 번 HUD 텍스트를 쓸 뿐*** 이다 -- 움직임은 전부 CSS/WAAPI 이고
                    // 페인트측이 전진시킨다. 그 장식용 rAF 하나 때문에 WebView 전체의
                    // 틱이 풀려 ***디스플레이 리스트가 15/s 에서 60/s 로, 리플로가 4 배***가
                    // 됐고, 저더가 났다.
                    //
                    // ★통제 실험(log_ani_debug_04/13, 14)★ -- 같은 빌드·같은 페이지에서
                    // pref 하나만 바꿨다:
                    //
                    //     tick_divisor=4 (기본)  epoch/s 14.9  PASSEV/s 0.21  저더 없음
                    //     tick_divisor=1         epoch/s 59.7  PASSEV/s 0.71  ***저더***
                    //
                    // ***`dx_px` 와 `dt_ms` 의 산포는 저더 쪽이 오히려 더 좋았다***
                    // (4.54/1.28 대 4.77/1.67). 다섯 라운드를 그 둘을 줄이는 데 썼는데
                    // 저더와는 음의 상관이었다 -- 움직인 것은 `epoch/s` 와 `PASSEVENT` 다.
                    //
                    // 고친 쪽의 이득은 진단용 프로브 한 장(`multigpu_wall_shape_anim_probe`
                    // 의 `anim=raf` 가 15 -> 25.5fps)이고, 깨진 쪽은 ***rAF 를 쓰는 모든
                    // 페이지의 표출 품질***이다. 실제 벽 콘텐츠가 통계 루프 하나만 가져도
                    // 똑같이 당한다. 교환이 명백히 나쁘다.
                    //
                    // 다시 손대려면 먼저 ***"이 rAF 가 실제로 프레임을 만드는가"***를
                    // 판정할 신호를 찾아야 한다. `animation_callbacks_running` 은 콜백의
                    // 존재만 말하므로 그 신호가 아니다.
                    let mut paint_side = false;
                    webview_renderer.for_each_connected_pipeline(&mut |pipeline_details| {
                        paint_side |= pipeline_details.animations.has_paint_animations();
                    });
                    Some((webview_renderer.id, paint_side))
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

    /// ack 전송을 한 번 일부러 건너뛸지(`SERVO_WALL_CANVAS_ACK_SKIP`). 기본값 0 = 항상 false.
    ///
    /// 건너뛸 때 빚(`waiting_pipelines`)을 ★그대로 남기는 것이 요점★이다 — 게이트가 닫혀
    /// 있어서 못 보낸 것과 똑같은 상태가 되어야 실측된 교착이 재현된다.
    /// [`Painter::flush_owed_canvas_ack`] 는 이것을 소비하지 않는다. 복구까지 같이 막으면
    /// 주입이 무엇을 증명하는지가 사라진다.
    ///
    /// ★부르는 자리는 [`Painter::release_owed_canvas_ack`] 하나다★ — 1 차 해제가 그 함수
    /// 하나뿐이므로 주입을 거기 걸면 1 차 해제 전부에 걸린다. 2026-10-21 전에는
    /// `generate_frame_for_script` 와 `update_images` 가 직접 불렀는데, 그 방식이면
    /// ★주입이 걸리지 않은 해제자가 먼저 풀어 버려서★ 보류가 아예 일어나지 않았다.
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

    /// 빚진 캔버스 ack 를 보낼 수 있으면 **지금** 보낸다. ★복구 경로★ — 1 차 해제는
    /// [`Painter::release_owed_canvas_ack`] 를 지나고, ★그 호출자는 페이싱 틱 하나다★
    /// (2026-10-21 Task 7 전에는 생산 경로 셋이 더 있었다).
    ///
    /// ★그래서 `gfx_single_frame_producer` 가 꺼진 구성에서는 1 차 해제가 아예 돌지
    /// 않는다★ — 그 구성에는 생산자도 없다(설계 8 절의 음성 대조군). 그때 빚을 푸는 것은
    /// 이 복구뿐이고, 렌더 패스는 표출 클럭이 내용과 무관하게 돌리므로 계속 닿는다.
    ///
    /// ★없으면 월이 영구히 멈춘다.★ 캔버스를 그린 문서는 `waiting_on_canvas_image_updates`
    /// 로 잠기고 `script_thread.rs:1441` 에서 rAF 가 ***통째로*** 건너뛰어진다. 그 잠금은
    /// 이 painter 가 `NoLongerWaitingOnAsynchronousImageUpdates` 를 보내야만 풀린다.
    ///
    /// 실측(2026-09-02, `log_webgpu/51` `ack_34`): 네 painter 전부 46.5 초 동안
    /// `pending_frame=true pending_canvas_images=0 renderer_behind=false` 였다. ★보낼 조건이
    /// 다 갖춰진 ack 를 부를 사람이 없어서 못 보낸 것★이지 무엇에 막힌 것이 아니었다.
    /// 비디오 페이지가 멀쩡한 이유도 같다 — `update_images` 가 끊임없이 와서 재확인이
    /// 저절로 일어난다.
    ///
    /// ★2026-10-21 로 달라진 것 둘.★
    ///
    /// 1. ***합성을 떼었다.*** 설계 2-6/2-7 — 생산자는 틱 하나다. 여기 남는 일은 전문 하나를
    ///    보내는 것뿐이고, 빚진 프레임은 ack 를 받은 스크립트가 바로 다시 요청한다
    ///    (그 46.5 초 교착도 "스크립트가 ack 를 기다리느라 멈춘" 것이지 합성이 모자란 것이
    ///    아니었다).
    /// 2. ***게이트를 `ack_is_releasable` 로 바꾸었다.*** 옛 게이트
    ///    (`needs_new_frame() && !renderer_behind()`)는 ★합성의 조건이지 해제의 조건이
    ///    아니다.★ 합성을 뗀 자리에 남겨 두면 정확히 이 복구가 필요한 상태를 걸러 낸다 —
    ///    합성을 이미 낸 직후에는 `pending_frame` 이 내려가 있어 `needs_new_frame()` 이
    ///    거짓이었고, 그래서 "합성은 했는데 ack 만 안 보낸" 빚이 영영 안 풀렸다.
    ///    `renderer_behind()` 도 마찬가지다 — 틱이 계속 바쁘게 만든 타일에서 빚이 그대로
    ///    남는다. (그 `needs_new_frame()` 은 Task 7 에서 필드째 사라졌다.)
    ///
    /// `renderer_behind()` 를 더는 부르지 않으므로 그 안의 2 초 강제 해제도 여기서는 안
    /// 돈다. ★그 프로브는 거절 경로의 것이다★ — `generate_frame_for_wall_tick` 이 매 틱
    /// ***무조건 먼저*** 부른다(`wall_tick::tile_may_generate`).
    fn flush_owed_canvas_ack(&mut self) {
        if *WALL_DISABLE_CANVAS_ACK_RECOVERY {
            return;
        }
        if !ack_is_releasable(
            self.frame_delayer.is_holding_ack(),
            self.frame_delayer.pending_canvas_image_count(),
        ) {
            return;
        }

        self.send_owed_canvas_ack();

        // ★양성 대조★: 고친 뒤에는 스톨이 안 나므로 "아무 줄도 없음"이 남는데, 그것만으로는
        // **고쳐진 것**과 **이 코드가 아예 안 도는 것**이 구분되지 않는다. 이 조사에서만 네 번
        // 같은 함정에 빠졌으므로, 실제로 보낸 횟수를 초당 한 줄로 남긴다. 0 이면 안 찍는다.
        // ★첫 건은 반드시 즉시 찍는다.★ 처음엔 1 초 창으로 합쳐서 찍었는데, 창 시작을 첫
        // 호출에서 잡고 같은 호출에서 경과를 보므로 **딱 한 번 일어나는 경우가 영영 안 찍혔다**
        // (2026-09-03, `log_webgpu/54`: painter 마다 1 회씩 복구했는데 `WALLACKFLUSH` 0 줄).
        // 모호함을 없애려고 만든 양성 대조가 정작 중요한 그 경우에 스스로 침묵한 것이다.
        // 드문 사건이므로 첫 건은 그대로 내보내고, 몰아칠 때만 1 초로 합친다.
        //
        // ★그런데 "드물다"가 더는 전부 참이 아니다.★ 1 차 해제(틱)는 보통 먼저 비우지만,
        // ***틱이 없는 구성***(`gfx_single_frame_producer` off = 설계 8 절의 음성 대조군)
        // 에서는 1 차가 아예 안 돌아 빚을 전부 여기가 잡는다. 그러면 1 초 합치기 상한까지 =
        // painter 당 초당 1 줄까지 나온다. 그 양은 병증이 아니라 이 갈래가 메워지고 있다는
        // 뜻이다. 병증은 `WALLACKLATCH` 쪽이다.
        let total = self.owed_ack_flush_count.get() + 1;
        self.owed_ack_flush_count.set(total);
        let now = Instant::now();
        let due = self
            .owed_ack_last_report
            .get()
            .is_none_or(|last| now.duration_since(last).as_secs_f64() >= 1.0);
        if due {
            self.owed_ack_last_report.set(Some(now));
            warn!(
                "WALLACKFLUSH: painter {:?} sent an owed canvas ack ({} so far). \
                 Each one is a frame script would otherwise have waited for forever.",
                self.painter_id, total,
            );
        }
    }

    /// ★1 차 캔버스 ack 해제.★ 판정은 `ack_is_releasable` 하나다: `renderer_behind()` 도,
    /// (2026-10-21 에 사라진) `pending_frame` 도 보지 않는다.
    ///
    /// ★호출자는 하나다 — 페이싱 틱.★ `Paint::produce_wall_frame` 이 ***타일마다, 생산
    /// 여부와 무관하게*** 부른다(설계 2-7). 2026-10-21 Task 7 전에는 생산 경로 셋이
    /// (`generate_frame_for_script` 의 성공 / rides-the-painter 경로, `update_images` 의
    /// 합성 블록) 같이 불렀고, 그 셋은 생산과 함께 사라졌다.
    ///
    /// ★왜 틱이어야 하나.★ 생산이 틱 하나로 모이면 저 세 자리는 ***전부 거절***이 되고,
    /// 해제가 거기 붙어 있으면 빚이 영영 안 풀린다. 실측(2026-10-16, `log_ani_debug 36`):
    /// divisor=1 실행이 117 초에서 멎었는데 그때까지 계수는 전부 건강했다 — 초당 60 프레임,
    /// dx 공칭값, overwritten=0. ★멈춘 것은 페인터가 아니라 스크립트였다.★
    ///
    /// 복구 자리([`Painter::flush_owed_canvas_ack`])는 이 함수를 지나지 않는다 — 실패
    /// 주입을 소비하면 안 되기 때문이다(`consume_canvas_ack_skip` 참고).
    pub(crate) fn release_owed_canvas_ack(&mut self) {
        if !ack_is_releasable(
            self.frame_delayer.is_holding_ack(),
            self.frame_delayer.pending_canvas_image_count(),
        ) {
            return;
        }
        // 실패 주입(기본 off). ★1 차 해제가 여기 하나이므로 이 한 자리면 전부에 걸린다.★
        // 복구는 소비하지 않으므로 주입 한 번은 "보류 → 복구가 보냄(`WALLACKFLUSH`)" 으로
        // 보인다.
        if self.consume_canvas_ack_skip() {
            return;
        }
        self.send_owed_canvas_ack();
    }

    /// ★스크린샷 상태 기계를 한 칸 민다 -- 반드시 ***생산 뒤***에.★ 설계 2-8 / 불변식 16.
    ///
    /// 호출자는 하나다: `generate_frame_for_wall_tick` 의 `Generated` 갈래, 그 타일의
    /// `generate_frame` + `send_transaction` ***뒤***.
    ///
    /// ★왜 생산 뒤여야 하는가.★ `WaitingOnPipelineDisplayLists -> WaitingOnFrame` 전이는
    /// `ReadyForScreenshot` 재도색을 걸 수 있고, 그 재도색은 `has_pending_frames()` 가
    /// 거짓일 때만 걸린다(`ScreenshotTaker::maybe_trigger_paint_for_screenshot`). 생산
    /// ***앞***에서 전진시키면 그 순간 `pending_frames == 0` 이라 전이가 곧바로 재도색을
    /// 걸고, 다음 렌더가 ***그 디스플레이 리스트를 아직 안 실은*** WebRender 의 직전
    /// 프레임을 다시 그려 그것을 찍는다.
    ///
    /// ★왜 벽 수준(`produce_wall_frame` 끝)이 아니라 타일 수준인가.★ 거절한 타일
    /// (`KeepPrevious`)은 이번 주기에 프레임을 ***요청하지 않았으므로*** `pending_frames`
    /// 가 0 일 수 있다. 벽 수준에서 전 타일을 돌면 그 타일에서 위 역전이 그대로 되살아난다.
    /// 생산한 타일에서만 전진시키는 것이 2026-10-21 Task 7 이전의 순서
    /// (`generate_frame_for_script` / `update_images` 가 ***자기*** 합성 뒤에 했다)를
    /// 글자 그대로 복원한다.
    ///
    /// 전진이 ***충분히 자주*** 도는 것도 이 자리라야 보장된다 -- 틱은 주기마다 돌고,
    /// 요청이 떠 있는 동안 매 주기 다시 확인한다.
    fn advance_screenshot_requests(&self) {
        self.screenshot_taker
            .prepare_screenshot_requests_for_render(self);
    }

    /// 빚진 파이프라인에 ack 를 보낸다. ★보낼지 말지는 호출자가 이미 정했다.★
    ///
    /// 1 차([`Painter::release_owed_canvas_ack`])와 복구
    /// ([`Painter::flush_owed_canvas_ack`])가 같은 전문을 보내게 하려고 둔다 — 전문을 만드는
    /// 자리가 넷이던 시절에 각자 조금씩 달라져 있었던 것이 이 조사의 비용 대부분이었다.
    fn send_owed_canvas_ack(&mut self) {
        let waiting_pipelines = self.frame_delayer.take_waiting_pipelines();
        self.send_to_constellation(
            EmbedderToConstellationMessage::NoLongerWaitingOnAsynchronousImageUpdates(
                waiting_pipelines,
            ),
        );
    }

    /// 스크립트가 이 painter 의 캔버스 ack 를 기다리다 잠긴 채로 남아 있는지 본다.
    ///
    /// ★여기에 거는 이유★: 스톨이 나면 스크립트도 WebGL 스레드도 논리 프레임도 전부 멈추는데
    /// **렌더 패스만은 계속 돈다**(winit_wall 의 표출 클럭이 내용과 무관하게 그리므로,
    /// 2026-09-02 스톨 실행에서 초당 185회). 그래서 살아 있는 유일한 자리가 여기다.
    ///
    /// 잠기는 구조: 캔버스를 그린 문서는 `waiting_on_canvas_image_updates` 로 잠기고
    /// `script_thread.rs:1441` 에서 rAF 가 통째로 건너뛰어진다. 그 잠금은 painter 가
    /// `NoLongerWaitingOnAsynchronousImageUpdates` 를 보내야만 풀린다.
    ///
    /// ★전문을 만드는 자리는 둘이다★(2026-10-21 이전에는 넷이었고, 전부 생산에 붙어
    /// 있었다): 1 차는 [`Painter::release_owed_canvas_ack`](호출자는 페이싱 틱 하나),
    /// 복구는 렌더 맨 앞의 [`Painter::flush_owed_canvas_ack`]. 이 줄이 뜬다는 것은 ★둘 다
    /// 못 돌았다★ 는 뜻이다 — 틱도 렌더도 멎었거나, 복구 킬 스위치
    /// (`SERVO_WALL_DISABLE_CANVAS_ACK_RECOVERY`)가 켜져 있다.
    ///
    /// 건강할 때는 한 줄도 안 찍는다. 찍혔다면 그 자체가 병증이다.
    ///
    /// ★실측 기록과 필드 대 필드로 비교하지 말 것.★ 실측된 ***자연*** 스톨(2026-09-02,
    /// `log_webgpu/51` `ack_34`)은 `pending_frame=true pending_canvas_images=0
    /// renderer_behind=false` 였는데, `pending_frame` 은 2026-10-21 에 필드째 사라져 이
    /// 줄에 더는 안 실린다. ★재현 여부는 영향받지 않는다★ — 잠긴다는 사실과 빚이 남는다는
    /// 사실은 `is_holding_ack()` 하나로 걸린다.
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
        // 게이트의 값을 그대로 싣는다 — 무엇이 막았는지가 이 줄의 존재 이유다.
        // `pending_frame` 은 2026-10-21 에 빠졌다 -- 필드 자체가 사라졌다.
        warn!(
            "WALLACKLATCH: painter {:?} held the canvas ack for {:.0}ms; \
             pending_canvas_images={} renderer_behind={} \
             composite_in_flight={}. Script cannot run another rAF until it is sent.",
            self.painter_id,
            held_ms,
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
        // 초당 한 번 GPU 메모리를 남긴다. 전환 구간의 드라이버 정체가 메모리 압박과
        // 같이 가는지를 보려면 그 눈금이 로그 안에 있어야 한다.
        {
            let now = Instant::now();
            let due = self
                .last_gpu_memory_log_at
                .get()
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1));
            if due {
                self.last_gpu_memory_log_at.set(Some(now));
                if let Some(device) = self.rendering_context.media_d3d11_device_handle_borrowed() {
                    log_gpu_memory(self.painter_id, device);
                }
                // 어느 타일에도 보이지 않는 영상의 D3D11 링을 회수한다.
                //
                // ★이 청소는 프레임마다 도는 이 자리에 있어야 한다★ — 링 회수는 원래
                // 그 영상의 프로듀서가 프레임을 만들 때만 돌았는데, 정작 회수해야 할 때는
                // 그 프로듀서가 멈춘 뒤다(앱이 요소를 남긴 채 재생만 멈추는 경우).
                // 실측: 그렇게 링이 321개, 3.8GB 까지 쌓였고 바닥값이 내려오지 않았다.
                let reclaimed =
                    servo_media::player::d3d11_ring::D3d11PlaneRings::expire_all_stale_demand(
                        servo_media::player::d3d11_ring::RING_DEMAND_TTL,
                    );
                if reclaimed > 0 {
                    warn!(
                        "MEDIARINGRECLAIM painter={:?} rings={reclaimed}",
                        self.painter_id
                    );
                }
            }
        }

        // 이 스레드의 렌더 프레임 경계를 알린다. 프레임 **안에서** 하는 일에 상한을
        // 두려는 쪽(새 비디오 링의 최초 소비)이 이 값으로 프레임이 바뀐 것을 안다.
        paint_api::render_frame::begin_render_frame();
        let render_start = Instant::now();
        let render_cpu_start = thread_cpu_ms();
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

        // 표출 주기의 실제 박자. 애니메이션 밀어넣기가 이 값을 따라간다(`push_due`).
        self.note_animation_win();
        self.last_render_started_at.set(Some(Instant::now()));

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
        //
        // ★정렬 pref(`gfx_present_align_commit_pct`)가 켜져 있으면 스케줄러의 커밋도 이
        // 값에 들어간다.★ 이 락 안에서 도는 `end_frame` 이 그 디바이스의 커밋 뮤텍스를
        // 잡으므로, 스케줄러가 같은 디바이스를 커밋하는 중이면 이 painter 는 ANGLE 락을 쥔 채
        // ★낡음(2026-09-21 기록, 2026-10-06 `1cdfd52cf3d` 가 반증)★ — 실측 1~3ms(나쁜 실행 8~10ms), 라이브 평균 2.04ms. ANGLE 락 안전 논증을 다시 세워야 한다.
        // 그 커밋이 끝나기를 기다린다. 그 대기의 상한은 `Commit()` 하나(~0.02ms)로 못 박혀
        // 있고(`commit_scheduler` 모듈 주석), 실제 값은 OUTCOMMIT total 의
        // `lock_wait_painter_us_max` 다 -- 반대 방향인 `lock_wait_sched_us_max`(스케줄러가
        // 페인터의 `end_frame` 임계구역을 기다린 시간)와 섞어 읽으면 안 된다.
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
        let render_cpu_ms = thread_cpu_ms() - render_cpu_start;
        // This render pass consumed every frame published so far (renderer.update() above
        // drained the whole publish queue), so the in-flight display composite is done.
        self.set_display_composite_in_flight(false);

        // ★표본은 렌더 **끝**에서 뜬다 -- 시작이 아니다.★
        //
        // 처음에는 `last_render_started_at` 을 찍은 직후, 즉 렌더 **시작**에서 불렀다.
        // 그 자리에서는 `pending_frames > 0` 이라 `painter_busy` 가 참이고, 그러면
        // `animated_property_frame` 이 거짓이 되어 **값만 나가고 프레임이 안 난다.**
        // 실측(log_ani_debug/38): 간격은 완벽해졌는데(dup/skip 0.44/0.42 -> 0.01/0.01)
        // `skipped_busy` 가 0 -> 15.2/s 로 뛰었고, 화면에 닿는 위치가 60.5 -> 45.3 으로
        // 떨어져 화면은 오히려 나빠졌다. 잃은 15.2 가 정확히 그 차이다.
        //
        // 여기는 렌더가 끝나 발행 큐가 비워진 자리다(바로 위 주석). `pending_frames`
        // 가 0 이므로 프레임이 정상적으로 나고, `last_render_started_at` 은 이 패스의
        // 것이라 `rendered_since_push` 도 참이다 -- 간격은 여전히 렌더당 정확히 하나다.
        #[cfg(windows)]
        crate::output_grid::note_pump_from_render();
        // ★이 렌더가 벽 프레임 번호를 달고 왔나.★ 대부분이 안 달고 온다면 이 경로는 벽
        // 프레임 조정(네 타일을 같은 프레임에 묶는 배리어) 밖에서 도는 것이고, 그것이
        // 샘플 시각보다 먼저 확인해야 할 사실이다(`output_grid::FRAME_IDS` 주석).
        #[cfg(windows)]
        crate::output_grid::note_frame_id(
            &format!("{:?}", self.painter_id),
            self.last_ready_wall_logical_frame_id.get(),
        );
        self.pump_paint_animation();
        if *LOG_PRESENT_CADENCE {
            self.last_render_end.set(Some(Instant::now()));
        }
        // Diagnostic: surface frames that blew the ~16.7ms vsync budget, with the WebRender
        // upload-vs-draw split plus renderer stats (video texture upload MB / texture cache
        // update ms / draw calls), to localize the bottleneck under heavy load (many videos).
        // A stall frame with normal upload_mb points at a driver/GPU sync or cache
        // reallocation; one with a big upload_mb spike points at an upload burst.
        if *LOG_PRESENT_CADENCE && render_ms > 16.0 {
            // ★`cpu_ms` 가 wall 의 절반을 넘는다는 것이 실측되었다★(340ms 중 140ms,
            // 280ms 중 219ms) — 이 정체는 GPU 대기가 아니라 이 스레드가 태우는 CPU 다.
            // 그런데 업로드는 0이고 draw call 은 26개다. 그러면 WebRender 가 이미 재고
            // 있는 나머지 시계들을 봐야 한다 — 특히 GPU 캐시 갱신은 프레임마다 CPU 로
            // 데이터를 채우는 자리라, 전환처럼 프리미티브가 통째로 바뀌면 커진다.
            let (upload_mb, upload_ms, gpu_cache_ms, draw_calls, alpha_targets, color_targets) =
                wr_stats.as_ref().map_or((0.0, 0.0, 0.0, 0, 0, 0), |stats| {
                    (
                        stats.texture_upload_mb,
                        stats.resource_upload_time,
                        stats.gpu_cache_upload_time,
                        stats.total_draw_calls,
                        stats.alpha_target_count,
                        stats.color_target_count,
                    )
                });
            info!(
                "Slow paint frame: painter {:?} total_ms={:.2} angle_lock_ms={:.2} wr_update_ms={:.2} wr_render_ms={:.2} upload_mb={:.1} upload_ms={:.1} draw_calls={} gpu_cache_ms={:.1} alpha_targets={} color_targets={} cpu_ms={:.1} pending_frames={}",
                self.painter_id,
                render_ms,
                angle_lock_ms,
                wr_update_ms,
                wr_render_ms,
                upload_mb,
                upload_ms,
                draw_calls,
                gpu_cache_ms,
                alpha_targets,
                color_targets,
                render_cpu_ms,
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

        // 타일 스레드에 사는 painter 는 표출까지 자기가 한다 — 셸에는 이 컨텍스트가 없다.
        // 인라인 경로에서는 셸이 present 하므로 여기서는 아무것도 하지 않는다.
        if self.owns_presentation {
            self.rendering_context.present();
        }
    }

    /// 이 painter 가 `render()` 끝에서 스스로 present 하게 한다. 타일 스레드 경로 전용이다.
    pub(crate) fn take_over_presentation(&mut self) {
        self.owns_presentation = true;
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
    /// 알 수 없다. 그래서 WR 출처를 줄 번호로 가른다 -- 호출 지점이 11 곳이던 시절에 그
    /// 가름이 필요했고, ★지금은 그 줄 번호가 "생산 지점이 하나인가" 를 판정하는 눈금이다★
    /// (설계 8 절: 구별 값이 정확히 둘이어야 한다). WR 을 안 거치는 `present_external_only`
    /// 는 `note_frame_source` 로 따로 센다.
    ///
    /// 기본 off(`SERVO_FRAME_REASON_PROF`). 프레임마다 불리는 자리라 켜져 있을 때만
    /// 해시맵을 건드린다.
    fn note_frame_reason(&self, caller: &'static Location<'static>, reason: RenderReasons) {
        self.note_frame_source(
            FrameSource::WebRender { line: caller.line() },
            format!("{reason:?}"),
        );
    }

    /// ★`generate_frame` 을 안 거치는 표출 경로의 자리다.★ `note_frame_reason` 과 같은 창·같은
    /// 줄에 같은 `<source>/<reason>=<n>` 꼴로 들어간다(런처 파서가 그 꼴을 읽는다).
    fn note_frame_source(&self, source: FrameSource, reason: String) {
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
        self.frame_reason_counts.borrow_mut().note(source, reason);

        let elapsed = now.duration_since(start);
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let (total, breakdown) = self.frame_reason_counts.borrow_mut().drain_report();
        info!(
            "FRAMEREASON total={total} window_ms={:.0} {breakdown}",
            elapsed.as_secs_f64() * 1000.0
        );
        self.frame_reason_window_start.set(Some(now));
    }

    /// 벽 페이서가 coalesce 해 두었던 요청을 풀어 낸 것을 센다(`wall_pacer_join`).
    /// 풀린 요청은 이어서 WR 프레임이 되어 그쪽에서 또 세어지므로, 이 값은 합성 총합
    /// (`total`)에 더하지 않는다 -- "두 번째 문" 이 열린 횟수만 보인다.
    ///
    /// ★2026-10-21 Task 7 뒤로 이 값은 ***영구히 0*** 이다★ — 울릴 수 있어서 0 인 것이
    /// 아니라, coalesce 큐를 채우던 유일한 자리(`Paint::coalesce_wall_frame_request`, 그
    /// 호출자는 `PaintMessage::GenerateFrame`)가 같이 사라져 ★채울 경로가 없기 때문★ 이다.
    /// 지금 이 계수기는 탐지기라기보다 ★걸림쇠의 위치 표시★ 다.
    ///
    /// ★언제 다시 울릴 수 있나:★ 누군가 `Paint::coalesced_wall_frame_requests` 에 다시 요청을 넣는
    /// 경로를 들이면(= 삭제된 `coalesce_wall_frame_request` 의 재추가, 또는
    /// `PaintMessage::GenerateFrame` 을 다시 생산으로 잇는 변경) 그때부터
    /// `try_release_coalesced_wall_frame_requests` 가 ★두 번째 문으로 프레임을 낸다★.
    /// 그 순간 이 값이 0 을 벗어나고, 그것이 설계 8 절의 실패 판정이다 — 생산자가 둘이다.
    pub(crate) fn note_wall_pacer_join(&self) {
        self.note_frame_source(FrameSource::WallPacerJoin, "RELEASE".to_string());
    }

    /// Queue a new frame in the transaction and increase the pending frames count.
    ///
    /// `#[track_caller]`: so `note_frame_reason` can name the call site without the callers
    /// having to pass a label. 호출자가 11 곳이던 시절에 생긴 장치이고, 지금은 그 줄 번호가
    /// "생산 지점이 하나인가" 의 눈금이다(설계 8 절).
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
        // Every frame, whatever asked for it. A paint-side animation needs to know whether
        // anyone else is already producing frames -- see `perform_updates`.
        self.last_frame_generated_at.set(Some(Instant::now()));
        // ★이 대입을 되돌리는 자리는 더 없다(2026-10-21 Task 7).★ 셋이 있었고
        // (`pump_paint_animation` 비디오 분기, `flush_deferred_animation_frame`,
        // `update_images` 의 비디오 도착 합성) 전부 자기 생산 블록과 함께 사라졌다 --
        // 필드 주석 참고. writer 는 이제 여기 하나다.
        self.last_frame_by_other_source_at.set(Some(Instant::now()));
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
        // ★완료와 짝지으려고 모든 요청을 줄 세운다.★ 애니메이션이 아닌 요청도 넣는다 --
        // `pending_frames` 가 종류를 가리지 않고 세므로, 줄도 같은 규칙이어야 완료 하나에
        // 요청 하나가 대응한다. 애니메이션 값이 없는 요청은 `None` 으로 들어간다.
        self.anim_ready_queue.borrow_mut().push_back((
            local_frame_id,
            self.anim_ready_pending_x.take(),
            Instant::now(),
        ));
    }

    pub(crate) fn pending_frames(&self) -> usize {
        self.pending_frames.get()
    }

    /// Move all held latest-wins video frame updates into `transaction`.
    /// See `pending_video_frame_updates` for the rationale.
    fn flush_pending_video_frame_updates(&self, transaction: &mut Transaction) {
        let mut pending_updates = self.pending_video_frame_updates.borrow_mut();
        let flushed = !pending_updates.is_empty();
        for (key, (descriptor, data)) in pending_updates.drain() {
            transaction.update_image(key, descriptor, data.into(), &DirtyRect::All);
        }
        if !flushed {
            return;
        }
        // 실제로 비디오가 실려 나간 순간들 사이의 간격. 최댓값만 남긴다 — 끊김은 평균이
        // 아니라 최악의 한 번이다.
        let now = Instant::now();
        if let Some(last) = self.last_video_presented_at.get() {
            let gap_ms = now.duration_since(last).as_secs_f64() * 1000.0;
            UPDATE_IMAGES_STATS.with(|stats| {
                let mut stats = stats.borrow_mut();
                if gap_ms > stats.video_gap_max_ms {
                    stats.video_gap_max_ms = gap_ms;
                }
            });
        }
        self.last_video_presented_at.set(Some(now));
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
    ///
    /// ★합성은 내지 않는다(2026-10-21).★ 루트 파이프라인이 바뀌었다는 사실은 전송으로 이미
    /// WR 에 가 있고, 그것을 화면에 올리는 프레임은 다음 틱이 낸다 -- 구성 요소는 자기 값만
    /// 갱신한다(규칙 3). 설계 2-6.
    fn send_root_pipeline_display_list(&mut self) {
        let mut transaction = Transaction::new();
        self.send_root_pipeline_display_list_in_transaction(&mut transaction);
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

        // ★합성은 내지 않는다(2026-10-21).★ 줌 배율과 스크롤 오프셋은 전송으로 나가고,
        // 다음 틱의 프레임이 그 값으로 그린다. 설계 2-6.
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

        // ★합성은 내지 않는다(2026-10-21).★ 디버그 플래그는 위에서 렌더러에 직접 들어갔고,
        // 다음 틱의 프레임이 그것을 반영한다. 여기 있던 전송은 프레임 하나뿐이었으므로
        // 전송 자체가 같이 없어진다 -- 실어 보낼 것이 남지 않았다. 설계 2-6.
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
    /// rAF 가 **실제로 합성을 내고 있는가.**
    ///
    /// ★rAF 가 돌고 있다는 것과 프레임이 나오고 있다는 것은 다른 말이다★ — 그런데
    /// 비디오 즉시-합성 게이트는 오랫동안 앞의 것으로 뒤의 것을 판정했다.
    ///
    /// 스크립트는 문서가 **새 디스플레이 리스트를 냈을 때만** `GenerateFrame` 을 보낸다
    /// (`script_thread::update_the_rendering`: `needs_frame()` 인 문서만 모아 보낸다).
    /// 그래서 rAF 콜백이 매 프레임 돌면서도 화면에 바뀌는 것이 없으면 — 자기 시계를 돌리며
    /// `setState` 하는데 그 값으로 그리는 스타일이 그대로인 루프가 정확히 그렇다 —
    /// **프레임은 한 장도 나오지 않는다.** 그동안 `animation_callbacks_running()` 은 참이므로
    /// 게이트는 "rAF 가 알아서 낸다"고 믿고 비디오의 합성을 막는다. 아무도 내지 않는다.
    ///
    /// 실측 증상: 소스를 일정 시간마다 바꿔 표출하는 화면(seamless)에서 **전환 애니메이션이
    /// 도는 동안에만** 영상이 흐르고 그 사이에는 정지했다. 전환 중에는 CSS 애니메이션이
    /// 매 프레임 새 디스플레이 리스트를 만들어 주기 때문이다. 상용 브라우저는 같은 페이지에서
    /// 정상이다 — 거기서는 비디오 표출이 rAF 와 묶여 있지 않다.
    ///
    /// 그래서 rAF 등록 여부가 아니라 **최근에 실제로 프레임이 나왔는지**로 판정한다. rAF 가
    /// 진짜로 화면을 돌리는 페이지(three.js·WebGL 루프·오버레이)에서는 프레임 간격이
    /// 새로고침 주기이므로 판정이 그대로 참이고, 예전 동작이 유지된다.
    fn raf_is_producing_frames(&self) -> bool {
        raf_is_driving_composites(
            self.animation_callbacks_running(),
            self.last_frame_by_other_source_at
                .get()
                .map(|last| last.elapsed()),
            crate::refresh_driver::paint_timer_period(),
        )
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
    /// ★낡음(2026-07-16 기록, 무효 시점 불명 — 당시 webrender 는 트리 밖 0.68)★ — 지금 트리의 자리는 `vendor_local/webrender/src/renderer/mod.rs:5402`.
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
        if !self.dcomp_native_active
            || !self.dcomp_resize_pending.get()
            || *DCOMP_RESIZE_REBUILD_DISABLED
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
    /// ★낡음(2026-07-16 기록, 무효 시점 불명 — 당시 webrender 는 트리 밖 0.68)★ — 조건은 `vendor_local/webrender/src/picture.rs:2332`, destroy 는 2337-2338.
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
        self.note_animation_ready();

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
            self.present_cadence_count
                .set(self.present_cadence_count.get() + 1);
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

        // ★합성은 내지 않는다(2026-10-21).★ 새 스크롤 오프셋은 전송으로 나가고, 다음 틱의
        // 프레임이 그 자리에서 그린다. 설계 2-6.
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
        // ★여기서 스크린샷 상태 기계를 밀지 않는다.★ `WaitingOnPipelineDisplayLists` 가
        // 기다리는 것은 이 대입이 맞지만, 이 시점은 ***그 리스트를 실은 프레임이 아직
        // 요청되지도 않은*** 자리다 -- `pending_frames == 0` 이라 전이가 곧바로
        // `ReadyForScreenshot` 재도색을 걸고, 다음 렌더가 WebRender 의 직전 프레임을 다시
        // 그려 그것을 찍는다. 전진은 생산 뒤에서 돈다
        // (`Painter::advance_screenshot_requests`, 틱의 `Generated` 갈래).
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
        // ★타일마다 같은 애니메이션이 다른 위치에 있는 것을 여기서 가른다.★
        //
        // 실측(2026-09-17, log_ani_debug/22): 등속 translateX 하나를 4 타일이 그리는데
        // PainterId(1) 이 나머지 셋보다 항상 50~74px(약 39ms) 뒤에 있었다. 부호가 한 번도
        // 바뀌지 않았고, 2/3/4 는 서로 0.4px 안에 들었다. 밀어넣기 주기는 넷이 동일했으므로
        // (frames=35 rode_along=25 skipped_busy=0) 원인은 박자가 아니라 **앵커**다.
        //
        // 앵커는 `created_at = received_at - (now - built_at)` 로 잡히므로 원리상 네 타일이
        // 같은 절대 시각에 수렴해야 한다. 수렴하지 않는다면 셋 중 하나다: `built_at` 이 타일마다
        // 다르거나(타일별로 디스플레이 리스트를 따로 만든다), transit 이
        // `MAX_DISPLAY_LIST_TRANSIT` 에 잘리거나, `offset_from_display_list` 가 다르거나.
        // 그 셋을 구분하려면 입력값을 그대로 봐야 한다 -- 결과인 좌표만으로는 되짚을 수 없다.
        if !display_list_info.paint_animations.is_empty() {
            let now = CrossProcessInstant::now();
            let detail = display_list_info
                .paint_animations
                .iter()
                .map(|animation| {
                    format!(
                        "offset={:.4} transit_ms={:.2}",
                        animation.offset_from_display_list,
                        (now - animation.built_at).as_seconds_f64() * 1000.0,
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            warn!(
                "PAINTANIMANCHOR painter={:?} epoch={:?} count={} {}",
                self.painter_id,
                epoch,
                display_list_info.paint_animations.len(),
                detail
            );
        }
        details
            .animations
            .install_paint_animations(display_list_info.paint_animations);

        let mut transaction = Transaction::new();
        let is_root_pipeline = Some(pipeline_id.into()) == webview_renderer.root_pipeline_id;
        if is_root_pipeline && old_scale != webview_renderer.device_pixels_per_page_pixel() {
            self.send_root_pipeline_display_list_in_transaction(&mut transaction);
        }

        transaction.set_display_list(epoch, (pipeline_id, built_display_list));

        self.update_transaction_with_all_scroll_offsets(&mut transaction);
        self.send_transaction(transaction);
    }

    /// 페이싱 틱이 이 타일에 벽 프레임 하나를 요구한다. ★이 타일이 WR 프레임을 내는 유일한
    /// 자리다★ (파이프라인 부트스트랩 `send_initial_pipeline_transaction` 제외 -- 설계 7 절,
    /// 종료 조건은 Task 9 가 붙인다).
    ///
    /// 2026-10-21 Task 7 전에는 스크립트 경로(`generate_frame_for_script`)가 따로 있었고,
    /// 저쪽은 `needs_new_frame()` 이 거짓이면 `renderer_behind()` 에 ***닿기 전에***
    /// 돌아섰다. 여기서는 그럴 수 없다 -- 아래 1 번의 이유를 보라.
    ///
    /// 몸통 순서는 바꿀 수 없다:
    ///
    /// 1. `renderer_behind()` 를 ★무조건 먼저★ 부른다. 그것은 조회가 아니라 liveness
    ///    프로브이고, 2 초 강제 해제가 **거절 경로에서만** 돈다("a stuck painter sends no
    ///    transactions, so it gets no frame-ready and no render -- but the wall keeps asking
    ///    it for frames, so this runs"). 답이 "건너뛴다" 인 타일에서도 매 틱 불러야 한다.
    ///    실측(2026-08-31): 이 호출을 잃자 네 페인터 중 둘이 렌더를 멈추고 남은 44 초 동안
    ///    돌아오지 않았다.
    /// 2. 캔버스 이미지 대기만 본다 -- ★스크립트의 빚은 보지 않는다★(규칙 2). 그 빚을 들고
    ///    있던 `FrameDelayer::pending_frame` 은 Task 7 에서 필드째 사라졌다.
    /// 3. 둘을 `tile_may_generate` 에 넘겨 판정한다.
    ///
    /// 돌려주는 `TileOutcome` 은 호출자가 이 타일을 배리어에 어떻게 넣을지 정하는 값이다.
    /// 거절한 타일을 조용히 빼면 화면에 프레임 N 과 N-1 이 섞이므로, 그 타일은
    /// `KeepPrevious` 로 같은 논리 프레임에 참여해야 한다 -- 지금은 `generated()` 가
    /// 거짓인 타일이 배리어의 기대 집합에서 빠질 뿐이고, 유지를 실제로 지시하는 배선은
    /// Task 8 이 넣는다.
    ///
    /// 호출자는 `Paint::issue_wall_frame_request` 하나다.
    pub(crate) fn generate_frame_for_wall_tick(
        &mut self,
        logical_frame_id: u64,
        requested_at: Instant,
    ) -> TileOutcome {
        let behind = self.renderer_behind();
        let waiting = self.frame_delayer.waiting_on_canvas_images();
        match tile_may_generate(behind, waiting) {
            TileOutcome::KeepPrevious => TileOutcome::KeepPrevious,
            TileOutcome::Generated => {
                let mut transaction = Transaction::new();
                // 쌓인 비디오 프레임은 이 안에서 무조건 실린다 -- 생산은 구성 요소를
                // **모을** 뿐이고, 무엇이 들었는지 묻지 않는다(규칙 3).
                self.generate_frame_with_diagnostic_id(
                    &mut transaction,
                    RenderReasons::SCENE,
                    Some(logical_frame_id),
                    Some(requested_at),
                );
                self.set_display_composite_in_flight(true);
                self.send_transaction(transaction);
                // ★스크린샷 상태 기계는 ***이 타일이 프레임을 요청한 뒤***에 민다.★
                // 그 순서가 전부다 -- 자세한 것은 `advance_screenshot_requests`.
                self.advance_screenshot_requests();
                TileOutcome::Generated
            },
        }
    }

    // ★`generate_frame_for_script` 는 2026-10-21 에 삭제됐다 -- 오늘의 열한 생산 지점 중
    // 하나였고, 규칙 2 를 정면으로 어겼다.★
    //
    // 스크립트의 `PaintMessage::GenerateFrame` 이 벽 프레임을 냈다. 그런데 디스플레이
    // 리스트는 ***별개 전송***으로 이미 올라가 있고(`PaintMessage::SendDisplayList` ->
    // `handle_new_display_list`), 그것을 화면에 올리는 프레임은 다음 틱이 낸다. 스크립트가
    // 프레임을 요청할 이유가 남지 않는다 -- 설계 0 절 1 번.
    //
    // ★여기 있던 ack 해제 둘(성공 경로 / rides-the-painter 경로)은 사라지지 않고 틱으로
    // 옮겨 갔다★ -- `Paint::produce_wall_frame` 이 ***타일마다, 생산 여부와 무관하게***
    // `release_owed_canvas_ack` 를 부른다(설계 2-7). 그것이 실측된 117 초 스톨
    // (log_ani_debug 36)을 구조적으로 불가능하게 만드는 자리다: 거절한 타일에서도 돈다.
    //
    // liveness 프로브(`renderer_behind()`)도 사라지지 않았다 -- 틱 전용 진입점
    // `generate_frame_for_wall_tick` 이 ***무조건 먼저*** 부른다(설계 2-5).

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
        // 이 함수 한 번이 45µs 로 측정됐고(초당 5,812회 = 메인 스레드의 287ms), 그 시간이
        // 스태시인지 합성 요청인지 트랜잭션 전송인지는 재지 않으면 모른다. pref 읽기는
        // 하나뿐이라 이미 배제됐다.
        let inner_started = Instant::now();
        let mut send_ms = 0.0_f64;
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
                        // 도착 간격. 표출 간격(`last_video_presented_at`)과 짝으로 본다.
                        let now = Instant::now();
                        if let Some(last) = self.last_video_arrived_at.get() {
                            let gap_ms = now.duration_since(last).as_secs_f64() * 1000.0;
                            UPDATE_IMAGES_STATS.with(|stats| {
                                let mut stats = stats.borrow_mut();
                                if gap_ms > stats.video_arrival_gap_max_ms {
                                    stats.video_arrival_gap_max_ms = gap_ms;
                                }
                            });
                        }
                        self.last_video_arrived_at.set(Some(now));
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

        // ★이미지 갱신은 합성을 요청하지 않는다(2026-10-21).★ 여기 있던 블록은
        // "스크립트가 빚진 프레임이 있고 캔버스가 다 올라왔으면 지금 낸다" 였다 -- 생산
        // 지점 열한 개 중 하나이고, 구성 요소(이미지 도착)가 방아쇠가 되는 자리라 규칙 2
        // 위반이다. 갱신은 위에서 `txn` 에 실렸고, 그것을 화면에 올리는 프레임은 다음 틱이
        // 낸다.
        //
        // ★여기 있던 ack 해제도 틱으로 옮겨 갔다★ -- `Paint::produce_wall_frame` 이
        // 타일마다 부른다(설계 2-7). 실패 주입(`consume_canvas_ack_skip`)은 여전히
        // `release_owed_canvas_ack` 안에 있으므로 주입의 효력도 그대로다.
        //
        // ★스크린샷 상태 기계를 미는 일도 같이 빠졌다★ -- 생산이 일어나는 유일한 자리인
        // 틱으로 옮겼다(`Painter::advance_screenshot_requests`, 틱의 `Generated` 갈래).
        // ***전진은 반드시 생산 뒤여야 한다*** -- 앞에서 밀면 아직 요청되지 않은 프레임을
        // 기다린 셈이 되어 직전 프레임이 찍힌다(그 함수의 주석).
        // 설계 2-6: docs/superpowers/specs/2026-10-21-single-frame-producer-design.md

        // ★여기 있던 "도착률로 즉시 합성" 설명은 그 경로와 함께 2026-10-21 에 사라졌다.★
        // 남은 것은 아래 external fast-path 가 쓰는 판정 하나다.
        //
        // ★"rAF 가 돌고 있다" 가 아니라 "rAF 가 프레임을 내고 있다" 로 판정한다★ --
        // 이유는 `raf_is_producing_frames` 주석. ★이 술어는 Task 10 이 지운다★ -- 생산자가
        // 틱 하나가 되면 답이 상수가 되고, `should_fast_present` 는 그 답을 틱에서 직접
        // 받는다(설계 4 절).
        let raf_driving_composites = self.raf_is_producing_frames();
        if self.animation_callbacks_running() && !raf_driving_composites {
            UPDATE_IMAGES_STATS.with(|stats| stats.borrow_mut().raf_idle_calls += 1);
        }

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
        // ★`took_fast_path` 는 2026-10-21 에 사라졌다.★ 그것은 "이 fast-path 를 택했으니
        // 아래 비디오 도착 합성은 내지 말라" 는 스위치였는데, 그 합성 자체가 없어졌다.
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
                // ★`generated_frame` 은 이제 언제나 거짓이다★ -- `update_images` 가 WR
                // 프레임을 내지 않으므로(위 주석). 인자는 남긴다: 이 판정이 "이 호출이
                // 이미 비싼 프레임을 냈나" 를 묻는 자리라는 사실이 서명에 남아야 한다.
                false,
                raf_driving_composites,
                escaped_count,
                resize_active,
                crate::dcomp_compositor::decouple_enabled(),
            ) {
                // ★박자는 틱이 준다 -- 틱마다 최대 1 회(설계 7 절).★
                //
                // 여기 있던 것은 직전 fast-present 에서 14ms 라는 자기 스로틀이었다. 그
                // 상수는 표출 주기와 아무 관계가 없어서 120Hz 벽(주기 8.33ms)에서는 한 틱
                // 걸러 한 번만 통과했다. 틱 번호로 바꾸면 천장("틱당 한 번")과 바닥("매 틱
                // 한 번")이 같은 값이 되고, 원래 요구 -- fast-path 는 WR 빌드와 무관하므로
                // WR 이 바쁠 때에도 발동해야 한다 -- 는 그대로 선다(틱은 WR 의 바쁨과
                // 무관하게 매 주기 돈다). dedup(`external_needs_present`)이 프레임이 안
                // 바뀐 비디오를 이미 걸러 주므로, 이 틱의 present 가 그 시점의 최신
                // 프레임을 반영한다.
                let due = self
                    .external_present_budget
                    .borrow_mut()
                    .take(crate::wall_tick::current_tick_seq());
                if due {
                    if let Some(shared) = self.dcomp_shared.as_ref() {
                        crate::dcomp_compositor::SharedDComp(shared.clone())
                            .present_external_only();
                        // ★WR 프레임이 아니라 DComp 직행 commit 이다★ — `generate_frame` 을
                        // 안 거쳐 FRAMEREASON 에 안 잡히던 경로(2026-10-21).
                        self.note_frame_source(FrameSource::ExternalSurface, "PRESENT".to_string());
                    }
                }
            }
        }

        // ★비디오 도착 경로의 합성은 2026-10-21 에 삭제됐다 -- 도착은 순수 적재다(규칙 2).★
        //
        // 여기 있던 것은 두 조각이었다. (1) `video_composite_due` -- 페인트 박자로 빚을
        // 지운다, (2) 그 빚을 갚는 합성. 둘이 막던 증상은 양 끝이었다: 도착마다 내면 요청률이
        // 모든 비디오의 프레임률 합이 되고(36 x FHD30 = 초당 1,080 개를 60Hz 벽에)
        // 2026-08-31 실측에서 39.19 코어 대 22.72 코어였으며, 아예 안 내면 비디오만 있는
        // 페이지에서 합성이 초당 28 로 가라앉아 ***콘텐츠의 30fps 보다 낮았다.***
        //
        // ★틱이 그 천장과 바닥을 동시에 준다(불변식 4).★ 틱은 주기마다 정확히 한 번 돌고,
        // 그 생산이 `flush_pending_video_frame_updates` 를 무조건 거치므로 쌓인 프레임은
        // 늦어도 다음 주기에 실려 나간다. 도착 수가 몇이든 합성은 주기당 하나이고, 도착이
        // 없어도 주기는 돈다 -- 어느 쪽 증상도 성립할 자리가 없다.
        //
        // ★단 그 바닥은 `gfx_single_frame_producer` 가 켜졌을 때의 것이고, 그 pref 는 아직
        // 기본 off 다★(Task 10 이 뒤집는다). 꺼져 있으면 틱이 생산하지 않으므로 비디오만
        // 있는 페이지의 합성은 지운 주석이 적은 28/s 가 아니라 ***0/s*** 다 -- 그것이
        // 설계 8 절의 음성 대조군(생산자가 없으면 화면이 선다)이고, 벽에 내보낼 구성이
        // 아니다. 바닥의 근거는 "틱이 돈다" 이지 "이 블록이 없다" 가 아니다.
        //
        // ★주의: `gfx_video_immediate_composite_enabled` 의 유일한 런타임 사용처가 여기였다★
        // -- 설계 8 절의 A/B 대조군이 이 블록과 함께 사라진다. 대조군은
        // `gfx_single_frame_producer` 를 끄는 음성 대조로 바뀐다.
        // 설계 2-6 / 4 절: docs/superpowers/specs/2026-10-21-single-frame-producer-design.md

        // With coalescing, a call that only stashed video frames produces an empty
        // transaction (the stash is flushed by the next composite); skip the send to avoid
        // pushing hundreds of no-op transactions per second through the scene builder.
        if !txn.is_empty() {
            let send_started = Instant::now();
            self.send_transaction(txn);
            send_ms = send_started.elapsed().as_secs_f64() * 1000.0;
        }
        UPDATE_IMAGES_STATS.with(|stats| {
            let mut stats = stats.borrow_mut();
            stats.calls += 1;
            stats.total_ms += inner_started.elapsed().as_secs_f64() * 1000.0;
            stats.send_ms += send_ms;
            let window = stats.window_start.get_or_insert_with(Instant::now);
            let elapsed = window.elapsed();
            if elapsed >= Duration::from_secs(1) {
                warn!(
                    "IMGUPDINNER window_ms={:.0} calls={} total_ms={:.1} send_ms={:.1} rest_ms={:.1} raf_idle={} vidgap_max_ms={:.1} vidarrive_max_ms={:.1}",
                    elapsed.as_secs_f64() * 1000.0,
                    stats.calls,
                    stats.total_ms,
                    stats.send_ms,
                    (stats.total_ms - stats.send_ms).max(0.0),
                    stats.raf_idle_calls,
                    stats.video_gap_max_ms,
                    stats.video_arrival_gap_max_ms,
                );
                *stats = UpdateImagesStats {
                    window_start: Some(Instant::now()),
                    ..Default::default()
                };
            }
        });
    }

    /// 이 painter 의 창이 올라가 있는 모니터(`HMONITOR`).
    ///
    /// ★매번 다시 묻는다.★ 디스플레이 구성이 바뀌면 `HMONITOR` 가 달라지므로 기동 시 한 번
    /// 캐시하면 핫플러그 뒤에 조용히 틀린 격자로 스케줄한다. `MonitorFromWindow` 는 API 한
    /// 번이라 프레임당 호출해도 비용이 없다.
    #[cfg(windows)]
    /// DWM 이 이 타일을 실제로 언제 합성했는지 한 표본 뜬다(`DCOMPSTAT`).
    ///
    /// ★계측이 없어서 세 번 틀렸다.★ 지금까지의 계측은 전부 DWM 에 넘기기 전까지만 봤고,
    /// 관찰된 타일 간 어긋남은 그 앞단으로 설명되는 크기의 20 배였다. 이 줄이 그 구간을
    /// 컴포지터 자신에게서 직접 받는다.
    ///
    /// 정렬·B2 와 무관하게 `-DcompBindProf` 만으로 켜진다 -- 대조군에서도 재야 비교가 된다.
    /// 컴포지터를 이미 대여 중일 수 있으므로 `try_borrow` 로 물러난다(빠진 표본은 통계에
    /// 영향을 주지 않을 만큼 드물고, 실패 횟수는 `failed` 가 센다).
    #[cfg(windows)]
    fn note_dcomp_frame_statistics(&self) {
        // ★B2 가 켜져 있으면 프로파일과 무관하게 떠야 한다.★ 이 호출이 공통 합성 격자를
        // 채우고 B2 가 그것을 쓴다 -- 진단 플래그 뒤에 두면 `-SampleLead` 만 준 운영 구성에서
        // 격자가 영영 비어 기능이 통째로 무력해진다. B1 에서 프로브를 그렇게 두어 똑같이
        // 당한 적이 있다(설계 문서 C5). 통계 히스토그램 쪽은 여전히 프로파일 뒤에 있다.
        if !*crate::dcomp_compositor::DCOMP_BIND_PROF &&
            crate::commit_scheduler::SAMPLE_LEAD_PERIODS.is_none()
        {
            return;
        }
        let Some(monitor) = self.tile_monitor() else {
            return;
        };
        let Some(shared) = self.dcomp_shared.as_ref() else {
            return;
        };
        match shared.try_borrow() {
            Ok(compositor) => compositor.note_frame_statistics(monitor),
            Err(_) => crate::output_grid::note_dcomp_stat_failed(),
        }
    }

    /// B2 의 샘플 시각 보정. 꺼져 있으면 0 이라 `Instant::now()` 그대로다.
    ///
    /// 격자나 모니터를 못 구하면(프로브 미가동·핫플러그 직후) 0 으로 떨어진다 -- 그 타일만
    /// 오늘 동작으로 돌아가는 것이고, 화면이 멈추는 쪽보다 낫다. 그 건수는 `SAMPLELEAD` 의
    /// `nogrid` 가 센다.
    /// 이 프레임의 애니메이션 값을 ★어느 시각으로★ 뽑을까. 꺼져 있으면 `None` -> 호출자가
    /// 지금 시각을 쓴다.
    ///
    /// ★`Duration` 이 아니라 `Instant` 를 돌려준다.★ 앵커(`Instant::now()`)를 QPC 읽기와
    /// 같은 순간에 잡아야 두 시계가 상쇄되어 샘플 시각이 격자 위에 정확히 떨어진다 --
    /// `lead_to_next_composition` 의 주석이 그 산수를 적고 있다. 호출자가 자기 `now` 를
    /// 더하면 그 사이의 `MonitorFromWindow` + `DwmGetCompositionTimingInfo` 가 그대로
    /// 지터가 된다.
    fn paint_animation_sample_at(&self) -> Option<Instant> {
        #[cfg(windows)]
        {
            let lead_periods = (*crate::commit_scheduler::SAMPLE_LEAD_PERIODS)?;
            // ★격자는 **공통 합성 격자** 하나다.★ 출력별 vblank 가 아니다 -- 계측으로
            // 확인된 바에 따르면 DWM 은 네 타일을 하나의 합성 패스에서 함께 올리므로
            // (`output_grid::COMPOSITION` 주석), 타일마다 다른 기준에 맞추는 것은 오차만
            // 더한다. B2 1 차가 그렇게 해서 `ANIMSTEP` 을 나쁘게 만들었다.
            //
            // 그 격자는 `note_dcomp_frame_statistics` 가 매 프레임 채운다. 첫 프레임 전이나
            // 조회가 실패하면 `None` -> 오늘 동작(지금 시각 샘플)으로 떨어진다.
            let monitor = self.tile_monitor().unwrap_or(0);
            match crate::output_grid::lead_to_next_composition(monitor, lead_periods) {
                Some((anchor, lead)) => {
                    crate::output_grid::note_sample_lead(monitor, lead);
                    Some(anchor + lead)
                },
                None => {
                    crate::output_grid::note_sample_lead_missing();
                    None
                },
            }
        }
        #[cfg(not(windows))]
        None
    }

    /// 이 타일이 올라가 있는 출력. 디스플레이 구성이 바뀌면 달라지므로 매번 다시 묻는다
    /// (`MonitorFromWindow` 한 번 + 캐시된 목록 대조라 비용이 없다).
    #[cfg(windows)]
    pub(crate) fn tile_monitor(&self) -> Option<usize> {
        let hwnd = self.rendering_context.window_hwnd()?;
        crate::output_grid::monitor_for_hwnd(hwnd)
    }

    pub(crate) fn pending_dcomp_commit_monitor(&self) -> Option<usize> {
        let hwnd = self.rendering_context.window_hwnd()?;
        crate::output_grid::monitor_for_hwnd(hwnd)
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

    /// ★낡음(2026-09-02 기록, 같은 날 `d3fa6dbd686` 이후 무효)★ — pref 는 `gfx_dcomp_commit_in_end_frame`(기본 false = 미룸)이다.
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
        webview_id: WebViewId,
        viewport_details: ViewportDetails,
        viewport_origin: DeviceVector2D,
    ) {
        self.webview_renderers
            .entry(webview_id)
            .or_insert(WebViewRenderer::new(
                webview_id,
                self.paint_proxy.clone(),
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
            // ★낡음(2026-07-16 기록, 무효 시점 불명 — 당시 webrender 는 트리 밖 0.68)★ — 판정 자리는 `vendor_local/webrender/src/picture.rs:2332`.
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
                    if virtual_mode {
                        "ON"
                    } else {
                        "OFF (RESIZE_VIRTUAL disabled)"
                    },
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

/// 캔버스 이미지가 WebRender 에 다 올라갔는지를 들고 있는 상태.
///
/// 이 이미지들은 캔버스 태스크에서 비동기로 갱신되므로, 다 올라오기 전에 그린 프레임에는
/// 직전 캔버스가 실린다. 그래서 틱 생산은 [`FrameDelayer::waiting_on_canvas_images`] 가
/// 참인 타일에서 그 주기를 keep-previous 로 넘긴다(`wall_tick::tile_may_generate`).
///
/// ★`pending_frame` 과 `needs_new_frame()` 은 2026-10-21 에 사라졌다.★ 그 둘은 "스크립트가
/// 프레임을 빚졌고 이제 낼 수 있는가" 를 답했고, 그 물음에 답할 이유는 스크립트가 프레임을
/// 요청할 때만 있었다. 생산자가 틱 하나가 된 뒤로 요청자는 없다(규칙 2) -- 남는 물음은
/// "이 타일의 캔버스가 올라왔는가" 하나뿐이다. 설계 2-6.
#[derive(Default)]
pub(crate) struct FrameDelayer {
    /// The latest [`Epoch`] of canvas images that have been sent to WebRender. Note
    /// that this only records the `Epoch`s for canvases and only ones that are involved
    /// in "update the rendering".
    image_epochs: FxHashMap<ImageKey, Epoch>,
    /// A map of all pending canvas images
    pending_canvas_images: FxHashMap<ImageKey, Epoch>,
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

    /// 틱 생산이 기다려야 하는가. 묻는 것은 하나다 -- "이 타일의 캔버스가 아직 WR 에 안
    /// 올라갔는가". 틱 생산은 ★스크립트의 빚과 무관하게★ 돈다(규칙 2).
    pub(crate) fn waiting_on_canvas_images(&self) -> bool {
        !self.pending_canvas_images.is_empty()
    }

    /// 스크립트가 이 painter 의 ack 를 기다리는 중인가. ★스톨 진단용★ — 이 집합이 비지
    /// 않은 동안 그 파이프라인의 문서는 `waiting_on_canvas_image_updates` 로 잠겨 있어
    /// ★낡음(2026-09-02 기록, 2026-09-07 `6d1542c888c` 이후 줄 표류로 무효)★ — 실제 자리는 `script_thread.rs:1441`.
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

/// 한 창의 표본을 ★도착 순서 그대로★ 한 줄에 넣는다.
///
/// ★왜 요약이 아니라 수열인가.★ `ANIMSTEP` 의 p05/p50/p95 는 1 초 창을 정렬해 요약하므로
/// ***버스트를 묻는다*** -- 실기에서 보고된 양상이 "0.5 초 간격으로 세 번 끊기고 1.5~2 초
/// 안정" 이고, 그런 구조는 분위수로는 보이지 않는다. 그리고 그 창 통계가 저더와 ***음의
/// 상관***이었다(§45): 저더가 나는 쪽의 `dx` 폭이 오히려 더 좁았다. 눈이 보는 것과 지표가
/// 재는 것이 다르다는 뜻이고, 그 간극을 메우려면 개별 값이 필요하다.
///
/// 한 줄에 한 창(60 개)을 담는다. 프레임마다 한 줄이면 300 초에 72,000 줄이 되고, 이
/// 로그는 이미 200MB 를 넘긴 적이 있다. 소수 한 자리면 충분하다 -- 1x(2,304px/s)에서
/// 0.1 은 0.1px 이고 0.006 주기다.
///
/// ★구현은 `servo_base::interval_summary` 에 있다★ -- `FRAMETIME`/`RAFCLOCK` 이 같은 꼴로
/// 찍어야 세 수열을 나란히 읽을 수 있고, 포맷이 두 벌이면 조용히 어긋난다.
pub(crate) fn format_step_sequence(samples: &[f64]) -> String {
    servo_base::interval_summary::format_intervals(samples)
}

/// `SERVO_LOG_ANIM_STEP_SEQ=1` 이면 창마다 `ANIMSTEPSEQ` 를 찍는다.
///
/// 기본은 꺼짐이다. 켜면 창당 한 줄(약 600 자)이 painter 수만큼 늘고, 벽에서는 초당 네
/// 줄이다 -- 300 초에 1,200 줄, 700KB 남짓. `ANIMSTEP` 과 같은 창, 같은 표본이므로 둘을
/// 나란히 읽으면 "요약이 무엇을 묻었나" 가 바로 드러난다.
pub(crate) static LOG_ANIM_STEP_SEQ: LazyLock<bool> =
    LazyLock::new(|| std::env::var("SERVO_LOG_ANIM_STEP_SEQ").is_ok_and(|value| value != "0"));

/// 합성 한 번을 만든 출처. `FRAMEREASON` 줄에서 `<source>/<reason>=<n>` 의 `<source>` 자리다.
///
/// ★`WebRender` 만 줄 번호를 가진다.★ 나머지는 WR 프레임이 아니거나(`ExternalSurface`: DComp
/// 직행 commit) 합성이 아니라 합류 자체다(`WallPacerJoin`). 줄 번호로 못 가르는 둘을 이름으로 가른다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FrameSource {
    WebRender { line: u32 },
    ExternalSurface,
    WallPacerJoin,
}

impl FrameSource {
    /// 런처 파서(`run_wall_dist.ps1`)가 읽는 이름. `painter.rs:<line>` 꼴은 바꾸지 않는다.
    fn label(&self) -> String {
        match self {
            FrameSource::WebRender { line } => format!("painter.rs:{line}"),
            FrameSource::ExternalSurface => "external_surface".to_string(),
            FrameSource::WallPacerJoin => "wall_pacer_join".to_string(),
        }
    }

    /// 화면에 닿는 합성 하나로 세는가. 합류는 같은 프레임이 WR 쪽에서 또 세어지므로 아니다.
    fn is_composite(&self) -> bool {
        !matches!(self, FrameSource::WallPacerJoin)
    }
}

/// `(출처, 이유)` 별 횟수. 창 하나(1초) 분량.
#[derive(Default)]
pub(crate) struct FrameReasonCounts {
    counts: FxHashMap<(FrameSource, String), u64>,
}

impl FrameReasonCounts {
    pub(crate) fn note(&mut self, source: FrameSource, reason: impl Into<String>) {
        *self.counts.entry((source, reason.into())).or_insert(0) += 1;
    }

    /// 서로 다른 출처의 수(이유는 세지 않는다).
    pub(crate) fn distinct_sources(&self) -> usize {
        let mut sources: Vec<FrameSource> = self.counts.keys().map(|(s, _)| *s).collect();
        sources.sort_by_key(|s| s.label());
        sources.dedup();
        sources.len()
    }

    /// 창을 비우며 `(total, "<source>/<reason>=<n> ...")` 를 낸다. 많은 순.
    /// ★`wall_pacer_join` 은 total 에서 뺀다★ — 목적이 "프레임을 받는 두 번째 문이 실제로 열리는가" 이지
    /// 합성 수가 아니고, 넣으면 같은 프레임이 WR 쪽에서 또 세어져 "생산 지점이 하나인가" 판정이 망가진다.
    fn drain_report(&mut self) -> (u64, String) {
        let mut counts: Vec<((FrameSource, String), u64)> = self.counts.drain().collect();
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.1.cmp(&b.0.1)));
        let total = counts
            .iter()
            .filter(|((s, _), _)| s.is_composite())
            .map(|(_, n)| n)
            .sum();
        let breakdown = counts
            .iter()
            .map(|((source, reason), n)| format!("{}/{reason}={n}", source.label()))
            .collect::<Vec<_>>()
            .join(" ");
        (total, breakdown)
    }
}

#[cfg(test)]
mod frame_reason_source_tests {
    use super::{FrameReasonCounts, FrameSource};

    /// ★WR 호출 지점과 DComp 직행 표출은 둘로 센다.★ 전에는 후자가 집계에 아예 없어
    /// "생산 지점이 하나" 가 거짓으로 나왔다.
    #[test]
    fn a_non_webrender_present_gets_its_own_reason_key() {
        let mut counts = FrameReasonCounts::default();
        counts.note(FrameSource::WebRender { line: 3811 }, "SCENE");
        counts.note(FrameSource::ExternalSurface, "PRESENT");
        assert_eq!(counts.distinct_sources(), 2);
    }

    /// ★런처 파서가 읽는 `<source>/<reason>=<n>` 꼴을 지킨다.★ 합류는 total 에 안 더한다.
    ///
    /// 줄 번호는 ★꼴만 보는 자리표★ 다 -- 어느 함수인지는 세지 않는다. 그래도 살아 있는
    /// 생산 지점의 번호를 쓴다(2026-10-21 기준 `generate_frame_for_wall_tick`): 삭제된
    /// 자리의 번호를 남기면 다음 사람이 그 줄을 찾아가 주석 블록을 만난다.
    #[test]
    fn report_keeps_the_launcher_parsed_shape() {
        let mut counts = FrameReasonCounts::default();
        counts.note(FrameSource::WebRender { line: 3811 }, "SCENE");
        counts.note(FrameSource::ExternalSurface, "PRESENT");
        counts.note(FrameSource::WallPacerJoin, "RELEASE");
        let (total, breakdown) = counts.drain_report();
        assert_eq!(total, 2);
        for part in ["painter.rs:3811/SCENE=1", "external_surface/PRESENT=1", "wall_pacer_join/RELEASE=1"] {
            assert!(breakdown.split(' ').any(|p| p == part), "{part} in {breakdown}");
        }
    }
}

#[cfg(test)]
mod anim_step_seq_tests {
    use super::format_step_sequence;

    /// ★원본 순서를 그대로 둔다.★ 이 계측이 존재하는 이유가 "창 통계가 버스트를 묻는다"
    /// 이므로, 정렬하거나 요약하면 찍을 이유가 없어진다.
    #[test]
    fn keeps_samples_in_arrival_order() {
        let got = format_step_sequence(&[16.7, 33.4, 8.1]);
        assert_eq!(got, "16.7,33.4,8.1");
    }

    /// 소수 한 자리면 충분하다 -- 60Hz 한 주기가 16.7ms 이고 1x 한 스텝이 38.4px 이라,
    /// 0.1 은 화면에서 0.1px 과 0.006 주기다. 자리를 늘리면 줄만 길어진다.
    #[test]
    fn rounds_to_one_decimal() {
        assert_eq!(format_step_sequence(&[16.666_7, 38.449]), "16.7,38.4");
    }

    /// 빈 창은 빈 문자열이다 -- 호출자가 그 창을 찍지 않는다.
    #[test]
    fn empty_is_empty() {
        assert_eq!(format_step_sequence(&[]), "");
    }

    /// ★음수도 그대로 찍는다.★ 애니메이션이 한 바퀴 돌면 dx 가 크게 음수가 되고,
    /// 그 프레임이 바로 이 계측이 찾는 것이다 -- 버리거나 절댓값을 씌우면 안 된다.
    #[test]
    fn keeps_negative_wrap_samples() {
        assert_eq!(
            format_step_sequence(&[38.4, -5721.6, 38.4]),
            "38.4,-5721.6,38.4"
        );
    }
}

#[cfg(test)]
mod raf_gate_tests {
    use std::time::Duration;

    use super::raf_is_driving_composites;

    const REFRESH: Duration = Duration::from_millis(16);

    /// rAF 가 없으면 비디오가 스스로 낸다(예전부터 그랬다).
    #[test]
    fn no_raf_means_the_video_composites_itself() {
        assert!(!raf_is_driving_composites(
            false,
            Some(Duration::from_millis(1)),
            REFRESH
        ));
    }

    /// rAF 가 실제로 프레임을 내고 있으면 비디오는 그 박자를 탄다 — 이 게이트의 본래 뜻이다.
    #[test]
    fn raf_producing_frames_still_carries_the_video() {
        // 60fps 로 도는 rAF 는 도착이 다음 프레임 직전에 걸려도 간격이 한 주기다.
        assert!(raf_is_driving_composites(
            true,
            Some(Duration::from_millis(16)),
            REFRESH
        ));
    }

    /// ★이 수정의 핵심★ — rAF 는 도는데 프레임이 안 나오는 상태.
    ///
    /// 아무것도 그리지 않는 rAF 루프(자기 시계만 돌리는 것)가 정확히 이 모습이고, 예전에는
    /// 이때도 참이라 비디오 합성이 막혀 화면이 정지했다.
    #[test]
    fn raf_that_produces_nothing_does_not_count() {
        assert!(!raf_is_driving_composites(
            true,
            Some(Duration::from_millis(200)),
            REFRESH
        ));
    }

    /// 경계는 한 주기 반이다 — 멀쩡한 rAF 의 최대 간격(한 주기)보다는 크고, 경계에서
    /// 한 박자로 느껴질 만큼 길지는 않다.
    #[test]
    fn the_boundary_sits_between_one_and_two_periods() {
        // 한 주기 간격은 아직 "남이 내고 있다"로 본다(60fps rAF 의 최악 위상).
        assert!(raf_is_driving_composites(
            true,
            Some(Duration::from_millis(23)),
            REFRESH
        ));
        // 한 주기 반을 넘기면 아무도 안 내고 있는 것이다.
        assert!(!raf_is_driving_composites(
            true,
            Some(Duration::from_millis(25)),
            REFRESH
        ));
    }

    /// 아직 한 장도 낸 적이 없으면 태워 줄 것이 없다.
    #[test]
    fn a_painter_that_never_drew_carries_nothing() {
        assert!(!raf_is_driving_composites(true, None, REFRESH));
    }
}

#[cfg(test)]
mod frame_delayer_canvas_wait_tests {
    use servo_base::Epoch;
    use servo_base::id::TEST_PIPELINE_ID;
    use webrender_api::ImageKey;

    use super::FrameDelayer;

    /// ★갓 만든 지연기는 아무것도 기다리지 않는다.★ 전에는 여기에 `set_pending_frame(true)`
    /// 로 "스크립트의 빚" 을 세워 두고 그것이 캔버스 대기로 읽히지 않는지를 봤는데, 그
    /// 플래그가 2026-10-21 에 사라졌다 -- 세울 사람이 없어졌기 때문이다(규칙 2). 묻는 것이
    /// 캔버스뿐이라는 사실은 이제 타입이 말한다.
    #[test]
    fn a_fresh_delayer_waits_on_nothing() {
        let delayer = FrameDelayer::default();
        let waiting = delayer.waiting_on_canvas_images();
        assert!(!waiting, "올라올 캔버스가 없으면 기다리지 않는다 (실제 {waiting})");
    }

    /// 캔버스가 아직 WR 에 안 올라갔으면 기다린다 -- `FrameDelayer` 계약 그대로다.
    #[test]
    fn an_outstanding_canvas_image_is_a_canvas_wait() {
        let mut delayer = FrameDelayer::default();
        delayer.add_delay(TEST_PIPELINE_ID, Epoch(1), vec![ImageKey::DUMMY]);
        assert!(delayer.waiting_on_canvas_images());
    }

    /// 그 이미지가 올라오면 대기가 풀린다.
    #[test]
    fn an_uploaded_canvas_image_ends_the_wait() {
        let mut delayer = FrameDelayer::default();
        delayer.add_delay(TEST_PIPELINE_ID, Epoch(1), vec![ImageKey::DUMMY]);
        delayer.update_image(ImageKey::DUMMY, Epoch(1));
        assert!(!delayer.waiting_on_canvas_images());
    }
}

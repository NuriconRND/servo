/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, LazyCell, RefCell};
use std::collections::{VecDeque, hash_map::Entry};
use std::rc::Rc;
use std::sync::{Arc, LazyLock};
use std::time::Instant;

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
// pacing). Read once. Default = enabled (current behavior). Values "1"/"true" disable it.
static VIDEO_IMMEDIATE_COMPOSITE_DISABLED: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("SERVO_DISABLE_VIDEO_IMMEDIATE_COMPOSITE")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
});

// Kill switch for the latest-wins coalescing of immediate (epoch-less, i.e. video) image
// updates in `update_images` (see `pending_video_frame_updates`). Read once. Default =
// coalescing enabled. Values "1"/"true" restore the previous forward-every-arrival behavior.
static VIDEO_UPDATE_COALESCE_DISABLED: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("SERVO_DISABLE_VIDEO_UPDATE_COALESCE")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
});

// Diagnostic: log the ACTUAL engine present cadence (frame-ready rate + worst inter-frame gap)
// once per second per painter. This is the ground-truth displayed cadence, independent of the
// page's requestAnimationFrame count and of external capture tools (Bandicam/PresentMon).
static LOG_PRESENT_CADENCE: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("SERVO_LOG_PRESENT_CADENCE")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
});

// DComp Native 경로에서 런타임 리사이즈(사용자 드래그/최대화) 후 picture-cache를 재구축하기
// 전에 크기가 안정되어야 하는 연속 프레임 수. 드래그는 매 프레임 resize 이벤트를 쏟아내므로
// 마지막 크기 변경 이후 이만큼 프레임이 흐른 뒤에 단 한 번만 재구축을 발동한다(≈170ms@60fps).
// 타이머 없이 프레임 카운터만으로 디바운스한다.
#[cfg(windows)]
const DCOMP_RESIZE_DEBOUNCE_FRAMES: u32 = 10;

// 킬 스위치(기본 = 활성). "1"/"true"이면 런타임 리사이즈 시 picture-cache 재구축을 끈다.
// A/B 검증(잔상 재현) 및 회귀 시 안전 밸브. 기본값에서는 재구축이 동작해 잔상을 제거한다.
#[cfg(windows)]
static DCOMP_RESIZE_REBUILD_DISABLED: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("SERVO_DCOMP_DISABLE_RESIZE_REBUILD")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
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
    display_composite_in_flight: Cell<bool>,

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
    /// frame of each video has display value. Kill switch:
    /// `SERVO_DISABLE_VIDEO_UPDATE_COALESCE`.
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
    /// 교대 상태: picture.rs:2320은 desired != current 일 때만 서피스를 파괴하므로 같은 값을
    /// 재전송하면 no-op이다. 그래서 재구축을 발동할 때마다 primary ↔ alternate 두 서로 다른
    /// 크기를 번갈아 보내 반드시 크기 변경(=파괴/재생성)을 만든다.
    #[cfg(windows)]
    dcomp_tile_toggle: Cell<bool>,
    /// 정상상태 picture 타일 크기(env SERVO_WR_PICTURE_TILE_SIZE 또는 WR 기본 1024x512).
    #[cfg(windows)]
    dcomp_tile_size_primary: webrender_api::units::DeviceIntSize,
    /// primary와 반드시 다른 유효 타일 크기. 첫 발동 시 이 값을 먼저 보내 desired != current를
    /// 보장한다(정상상태 = primary 이므로).
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
        #[cfg(windows)]
        if std::env::var("SERVO_MEDIA_D3D11_VIDEO").is_ok_and(|value| {
            value == "1"
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
                || value.eq_ignore_ascii_case("on")
        }) {
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
        let compositor_config = if crate::dcomp_compositor::enabled() {
            match crate::dcomp_compositor::maybe_create(&rendering_context) {
                Some(compositor) => {
                    log::info!("[dcomp-native] engaged: WR native compositor (DirectComposition)");
                    // 실제 발동을 rendering_context에 알린다 — present()의 스킵 판정은
                    // env 게이트가 아닌 이 신호를 본다(리뷰 픽스: env on + 발동 실패로
                    // Draw 폴백된 경우 present가 잘못 스킵되면 블랭크 윈도우가 된다).
                    rendering_context.set_dcomp_native_active(true);
                    webrender::CompositorConfig::Native {
                        compositor: Box::new(compositor),
                    }
                },
                None => {
                    log::warn!("[dcomp-native] init failed; falling back to Draw compositor");
                    // 초기값 false에 암묵 의존하지 않고 폴백임을 명시한다(§set_dcomp_native_active
                    // 계약: true로의 전이는 발동 성공 시 1회뿐이고, 폴백은 항상 false를 유지).
                    rendering_context.set_dcomp_native_active(false);
                    webrender::CompositorConfig::default()
                },
            }
        } else {
            webrender::CompositorConfig::default()
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
        let webrender_document = webrender_api.add_document(rendering_context.size2d().to_i32());

        // 실험 노브: WR picture cache 타일 크기 오버라이드 (SERVO_WR_PICTURE_TILE_SIZE=WxH,
        // 예 1920x1080). 미설정이면 WR 기본 1024x512. 타일 수·무효화 입도·per-tile
        // 오버헤드(DComp bind/unbind 횟수) A/B용 — 값이 창 이상이면 슬라이스당 타일 1장.
        if let Ok(value) = std::env::var("SERVO_WR_PICTURE_TILE_SIZE") {
            let lower = value.to_ascii_lowercase();
            let mut split = lower.split('x');
            let size = (
                split.next().and_then(|v| v.trim().parse::<i32>().ok()),
                split.next().and_then(|v| v.trim().parse::<i32>().ok()),
            );
            match size {
                (Some(w), Some(h)) if w > 0 && h > 0 => {
                    webrender_api.send_debug_cmd(webrender::DebugCommand::SetPictureTileSize(
                        Some(webrender_api::units::DeviceIntSize::new(w, h)),
                    ));
                    log::info!("[wr-tile-size] picture tile size override: {w}x{h}");
                },
                _ => {
                    log::warn!(
                        "[wr-tile-size] SERVO_WR_PICTURE_TILE_SIZE 형식 오류(WxH 기대): {value}"
                    );
                },
            }
        }

        let gl_renderer = webrender_gl.get_string(gleam::gl::RENDERER);
        let gl_version = webrender_gl.get_string(gleam::gl::VERSION);
        info!("Running on {gl_renderer} with OpenGL version {gl_version}");

        // Runtime-resize 재구축용 primary/alternate picture 타일 크기 계산.
        // primary = env 오버라이드(설정 시) 또는 WR 기본 1024x512(TILE_SIZE_DEFAULT) — 정상상태
        // 타일 크기와 동일. alternate = primary와 반드시 다른 유효 크기(WR 클램프 128..=4096 내).
        // 첫 발동은 alternate를 먼저 보내 desired != current(=primary)를 보장한다.
        #[cfg(windows)]
        let (dcomp_tile_size_primary, dcomp_tile_size_alternate) = {
            use webrender_api::units::DeviceIntSize;
            let primary = std::env::var("SERVO_WR_PICTURE_TILE_SIZE")
                .ok()
                .and_then(|value| {
                    let lower = value.to_ascii_lowercase();
                    let mut split = lower.split('x');
                    match (
                        split.next().and_then(|v| v.trim().parse::<i32>().ok()),
                        split.next().and_then(|v| v.trim().parse::<i32>().ok()),
                    ) {
                        (Some(w), Some(h)) if w > 0 && h > 0 => Some(DeviceIntSize::new(w, h)),
                        _ => None,
                    }
                })
                .unwrap_or_else(|| DeviceIntSize::new(1024, 512));
            let alternate = if primary.width == 512 && primary.height == 512 {
                DeviceIntSize::new(1024, 512)
            } else {
                DeviceIntSize::new(512, 512)
            };
            (primary, alternate)
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
            present_cadence_start: Default::default(),
            present_cadence_last: Default::default(),
            present_cadence_count: Default::default(),
            present_cadence_max_gap_ms: Default::default(),
            last_render_end: Default::default(),
            screenshot_taker: Default::default(),
            refresh_driver,
            animation_refresh_driver_observer,
            webrender_renderer: Some(webrender_renderer),
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
            dcomp_tile_toggle: Cell::new(false),
            #[cfg(windows)]
            dcomp_tile_size_primary,
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

    #[servo_tracing::instrument(skip_all)]
    pub(crate) fn render(&mut self, time_profiler_channel: &ProfilerChan) {
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
        {
            let _angle_gl_guard = paint_api::ANGLE_GL_LOCK.lock().unwrap();

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
        self.display_composite_in_flight.set(false);
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
                "Slow paint frame: painter {:?} total_ms={:.2} wr_update_ms={:.2} \
                 wr_render_ms={:.2} upload_mb={:.1} upload_ms={:.1} draw_calls={} \
                 pending_frames={}",
                self.painter_id, render_ms, wr_update_ms, wr_render_ms, upload_mb, upload_ms,
                draw_calls, self.pending_frames.get(),
            );
        }
        if self.rendering_context.requested_gpu_index().is_some() {
            info!(
                "Wall render end: painter {:?} render_count={} local_frame_id={:?} \
                 logical_frame_id={:?} render_ms={:.3} pending={} overlapping_request_count={} \
                 unexpected_ready_count={} requested_gpu={:?} size={:?}",
                self.painter_id,
                render_count,
                local_frame_id,
                wall_logical_frame_id,
                render_ms,
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

    /// Queue a new frame in the transaction and increase the pending frames count.
    pub(crate) fn generate_frame(&self, transaction: &mut Transaction, reason: RenderReasons) {
        self.generate_frame_with_diagnostic_id(transaction, reason, None, None);
    }

    /// Queue a new frame using a shared logical diagnostic id supplied by `Paint`.
    pub(crate) fn generate_frame_with_diagnostic_id(
        &self,
        transaction: &mut Transaction,
        reason: RenderReasons,
        wall_logical_frame_id: Option<u64>,
        wall_requested_at: Option<Instant>,
    ) {
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
        for webview_renderer in self.webview_renderers.values() {
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
    fn renderer_behind(&self) -> bool {
        self.display_composite_in_flight.get()
    }

    /// DComp Native 경로에서 런타임 리사이즈 후 picture-cache를 물리적으로 재구축한다.
    /// 프레임 준비마다 호출되어 마지막 크기 변경 이후 안정 프레임 수를 세고, 임계값에 도달하면
    /// SetPictureTileSize를 primary↔alternate로 교대 전송한다. 이는 WR이 desired != current
    /// 타일 크기를 감지해 모든 picture-cache 슬라이스의 네이티브 서피스를 destroy_surface하고
    /// 타일을 비운 뒤 재생성하게 만들어(webrender-0.68 picture.rs:2320-2332 →
    /// resource_cache::destroy_compositor_surface → renderer/mod.rs:5163 compositor.destroy_surface),
    /// 리사이즈 이전 가상 서피스에 남은 옛 픽셀(잔상)을 물리적으로 소멸시킨다. task-10b가 증명한
    /// FORCE_PICTURE_INVALIDATION의 한계(재계산만 강제, vacated 영역 미재도색)를 이 방식이 우회한다.
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

        // 크기가 멎었다. primary ↔ alternate 교대 전송으로 반드시 크기 변경을 만든다(같은 값
        // 재전송은 picture.rs:2320에서 no-op이므로 서로 다른 두 값을 번갈아야 파괴가 발생).
        let next = if self.dcomp_tile_toggle.get() {
            self.dcomp_tile_size_primary
        } else {
            self.dcomp_tile_size_alternate
        };
        self.dcomp_tile_toggle.set(!self.dcomp_tile_toggle.get());
        self.webrender_api
            .send_debug_cmd(webrender::DebugCommand::SetPictureTileSize(Some(next)));
        self.dcomp_resize_pending.set(false);
        self.dcomp_resize_stable_frames.set(0);
        // 드문 이벤트(정착 리사이즈당 1회)라 info로 남겨 런처 기본 RUST_LOG(paint=info)에서
        // 보이게 한다 — 매 프레임 debug 스팸과 달리 로그 부하가 없다.
        info!(
            "[dcomp-native] runtime resize settled after {} stable frames; forcing picture-cache \
             rebuild via SetPictureTileSize={}x{} (destroys/recreates native surfaces to purge \
             stale virtual-surface ghosts; task-10b: FORCE_PICTURE_INVALIDATION could not)",
            DCOMP_RESIZE_DEBOUNCE_FRAMES, next.width, next.height,
        );
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
            self.display_composite_in_flight.set(false);
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

        let mut transaction = Transaction::new();
        self.generate_frame_with_diagnostic_id(
            &mut transaction,
            RenderReasons::SCENE,
            Some(diagnostic_frame_id),
            Some(wall_requested_at),
        );
        self.display_composite_in_flight.set(true);
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
                    } else if *VIDEO_UPDATE_COALESCE_DISABLED {
                        immediate_image_update = true;
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
        if self.frame_delayer.needs_new_frame() && !self.renderer_behind() {
            self.frame_delayer.set_pending_frame(false);
            self.generate_frame(&mut txn, RenderReasons::SCENE);
            self.display_composite_in_flight.set(true);
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
        if immediate_image_update &&
            !generated_frame &&
            self.pending_frames.get() == 0 &&
            !raf_driving_composites &&
            !self.renderer_behind() &&
            !*VIDEO_IMMEDIATE_COMPOSITE_DISABLED
        {
            self.generate_frame(&mut txn, RenderReasons::SCENE);
            self.display_composite_in_flight.set(true);
        }

        // With coalescing, a call that only stashed video frames produces an empty
        // transaction (the stash is flushed by the next composite); skip the send to avoid
        // pushing hundreds of no-op transactions per second through the scene builder.
        if !txn.is_empty() {
            self.send_transaction(txn);
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
            self.dcomp_resize_pending.set(true);
            self.dcomp_resize_stable_frames.set(0);
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

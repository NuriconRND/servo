/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The interface to the `paint` crate, which helps to break dependency cycles.

use std::collections::HashMap;
use std::fmt::{Debug, Error, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use embedder_traits::{AnimationState, EventLoopWaker};
use euclid::{Rect, Scale, Size2D};
use log::warn;
use malloc_size_of_derive::MallocSizeOf;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use rustc_hash::FxHashMap;
use servo_base::Epoch;
use servo_base::id::{PainterId, PipelineId, WebViewId};
use smallvec::SmallVec;
use strum::IntoStaticStr;
use style_traits::CSSPixel;
use surfman::{Adapter, Connection};
use webrender_api::{DocumentId, FontVariation};

pub mod display_list;
pub mod largest_contentful_paint_candidate;
pub mod render_frame;
pub mod rendering_context;
pub mod viewport_description;
pub mod wall_args;
pub mod wall_layout;

use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use bitflags::bitflags;
use display_list::PaintDisplayListInfo;
use embedder_traits::ScreenGeometry;
use euclid::default::Size2D as UntypedSize2D;
use profile_traits::mem::{OpaqueSender, ReportsChan};
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::{
    self, GenericCallback, GenericReceiver, GenericSender, GenericSharedMemory,
};
pub use webrender_api::ExternalImageSource;
use webrender_api::units::{DeviceIntRect, DeviceIntSize, DevicePixel, LayoutVector2D, TexelRect};
use webrender_api::{
    BuiltDisplayList, BuiltDisplayListDescriptor, ExternalImage, ExternalImageData,
    ExternalImageHandler, ExternalImageId, ExternalScrollId, FontInstanceFlags, FontInstanceKey,
    FontKey, ImageData, ImageDescriptor, ImageKey, NativeFontHandle,
    PipelineId as WebRenderPipelineId,
};

use crate::largest_contentful_paint_candidate::LCPCandidate;
use crate::rendering_context::RenderingContext;
use crate::viewport_description::ViewportDescription;

/// ANGLE 의 D3D11 즉시 컨텍스트 접근을 직렬화하는 락. ★전역 하나가 아니라 디바이스당 하나★.
///
/// # 왜 있는가 (그리고 왜 이제 전역이 아닌가)
///
/// 원래는 `pub static ANGLE_GL_LOCK: Mutex<()>` 하나였다. `d39b5af1362`("Stabilize WebGL wall
/// rendering on ANGLE", 2026-06-13)가 ANGLE 을 만지는 7 곳을 한꺼번에 잠근 것으로, 당시
/// WebGL 백엔드 컨텍스트와 컴포지터가 **ANGLE 의 LUID 캐시 EGLDisplay(따라서 하나의 D3D11
/// 디바이스)를 공유**해 두 스레드가 한 렌더러를 구동하다 `libGLESv2.dll` 에서 access
/// violation(0xc0000005)이 나던 문제를 멈추기 위한 것이었다.
///
/// ★그 공유는 그 다음 날 `35b4c2ed799`("멀티 GPU 월 WebGL: 타일별 격리 D3D11 디바이스")가
/// 없앴다★ — WebGL 백엔드는 `create_isolated_device` 로 전용 D3D11 디바이스와 전용 ANGLE
/// 디스플레이를 받는다. 근본 원인이 사라진 뒤에도 전역 락은 남았고, 2026-09-01 실측에서
/// painter 들이 (드라이버 블로킹 중에) 락을 64 초 중 42.8 초 쥐는 바람에 WebGL 스레드가
/// 자기 시간의 46.7% 를 대기로 썼다. 정작 필요한 실작업은 초당 20ms 였다.
///
/// # 키
///
/// 키는 **ANGLE D3D11 디바이스 포인터**다([`RenderingContext::angle_d3d11_device_ptr`],
/// surfman 의 `Device::native_device().d3d11_device`). 이것이 정확히 공유 단위다 — 포인터가
/// 같으면 즉시 컨텍스트가 같으므로 반드시 직렬화해야 하고, 다르면 서로 독립이다.
///
/// ★요청 GPU 인덱스를 키로 쓰면 안 된다★: `create_adapter_for_requested_gpu` 는 인덱스
/// 선택이 실패하면 조용히 기본 어댑터로 폴백하므로, 요청한 인덱스가 실제로 열린 어댑터와
/// 다를 수 있다. 그러면 같은 디바이스를 쓰는 둘이 다른 슬롯으로 갈려 보호가 사라진다 —
/// 되돌아오는 증상이 하필 0xc0000005 라 가장 나쁜 실패 모드다.
///
/// # `None` 의 의미
///
/// 키를 모르는 호출부(아직 디바이스가 없는 컨텍스트 생성 경로, ANGLE 이 아닌 백엔드)는
/// `None` 을 준다. 그러면 **전량 배타**를 잡는다 — 무엇을 건드리는지 모르므로 전부와
/// 직렬화하는 것이 유일하게 안전한 선택이고, 모든 호출부가 `None` 인 비-ANGLE 빌드에서는
/// 예전 전역 뮤텍스와 동작이 완전히 같아진다.
///
/// 잠금 순서는 항상 `shared -> device` 한 방향이고 배타 측은 디바이스 락을 잡지 않으므로
/// 데드락이 생길 수 없다.
pub fn angle_gl_lock(d3d11_device: Option<usize>) -> AngleGlGuard {
    let Some(device) = d3d11_device.filter(|pointer| *pointer != 0) else {
        // ★배타를 기다린 시간과 잡고 있을 시간을 잰다.★ 근거는 `note_paint_gap` 의 doc
        // 주석. 전환당 몇 번이라 무조건 찍어도 된다.
        let began = Instant::now();
        let exclusive = ANGLE_GL_EXCLUSION.write();
        let waited = began.elapsed();
        return AngleGlGuard {
            device: None,
            shared: None,
            exclusive: Some(exclusive),
            held_since: Some(Instant::now()),
            exclusive_wait: waited,
        };
    };

    let gap_began = Instant::now();
    let shared = ANGLE_GL_EXCLUSION.read();
    note_paint_gap(gap_began.elapsed());
    let device_lock = {
        let mut registry = ANGLE_GL_DEVICE_LOCKS.lock().expect("poisoned");
        let known = registry.len();
        *registry.entry(device).or_insert_with(|| {
            // ★이 줄이 안 찍히면 이 변경은 아무 일도 하지 않은 것이다★ — 키가 하나도
            // 해석되지 않았다는 뜻이고(예: `no-wgl` 피처가 webgl 크레이트까지 전파되지
            // 않음), 그러면 모든 호출부가 `None` 으로 떨어져 예전 전역 락과 똑같이 동작한다.
            // 조용히 무효가 되는 것을 막으려고 디바이스당 한 번 찍는다.
            log::warn!(
                "ANGLE lock: separate slot for D3D11 device #{} (0x{device:x})",
                known + 1
            );
            // 디바이스 하나당 한 번만 새는데(프로세스 수명 동안 몇 개), 그 대가로 가드가
            // `'static` 이 되어 Arc 를 자기 참조로 들고 있을 필요가 없어진다.
            &*Box::leak(Box::new(Mutex::new(())))
        })
    };

    AngleGlGuard {
        device: Some(device_lock.lock().expect("poisoned")),
        shared: Some(shared),
        exclusive: None,
        held_since: None,
        exclusive_wait: Duration::ZERO,
    }
}

/// painter 가 `render()` 진입을 기다린 시간(= ANGLE 공유 락 획득 대기). 꼬리는 그때그때 한
/// 줄, 나머지는 1200 획득마다 모아서 한 줄.
///
/// ## 왜 이 수치인가 (2026-11-12, log_webgl_perf_01/44)
///
/// 운영자 관찰: "전환 애니메이션 ***시작 및 종료***간 멈칫하는 현상 큰 개선 없음".
///
/// `create_webgl_backend_context` 는 `angle_gl_lock(None)` = ★전역 배타★ 를 잡고, 그 구간에
/// 네 painter 가 `render()` 에 들어갈 수 없다. `pump_paint_animation(PumpSource::Render)` 이
/// `render` 안에 있으므로 ***어느 타일도 전환을 진행하지 못한다***
/// (`components/paint/wall_tick.rs:186-198`).
///
/// 그런데 지금 손에 있는 것은 "시작과 종료에 멈칫한다" 는 육안과 "전역 배타 락이 있다" 는
/// 코드 읽기뿐이고, ***둘을 잇는 수치가 없다***. `GLEXCL`(배타를 잡고 있던 시간)과
/// `PAINTGAP`(painter 가 기다린 시간)의 시각이 겹치면 인과가 확정된다.
///
/// 리사이즈 할당을 줄인 앞 커밋은 `LOCKTAIL`(= `AcquireSync`)을 ×0.70 으로 줄였지만 육안은
/// 그대로였다 -- 꼬리는 다른 락을 재고 있었다는 뜻이다.
///
/// ## 비용
///
/// painter 틱마다(4 x 60Hz = 240/s) 시계 둘과 원자 넷이므로 약 12us/s 다. 꼬리 줄은 8ms
/// 이상일 때만 나온다.
fn note_paint_gap(waited: Duration) {
    /// 꼬리로 보고 한 줄 찍는 기준. 한 주기(16.7ms)의 절반이다.
    const TAIL_US: u64 = 8_000;

    static SUM_US: AtomicU64 = AtomicU64::new(0);
    static MAX_US: AtomicU64 = AtomicU64::new(0);
    static ACQUIRES: AtomicU64 = AtomicU64::new(0);
    static TAILS: AtomicU64 = AtomicU64::new(0);

    let us = waited.as_micros() as u64;
    SUM_US.fetch_add(us, Ordering::Relaxed);
    MAX_US.fetch_max(us, Ordering::Relaxed);

    if us >= TAIL_US {
        let nth = TAILS.fetch_add(1, Ordering::Relaxed) + 1;
        log::warn!("PAINTGAP #{nth} waited={us}us for the shared ANGLE lock");
    }

    // 호출 수로 리포트 시점을 정한다 -- 시계를 또 읽지 않는다.
    let n = ACQUIRES.fetch_add(1, Ordering::Relaxed) + 1;
    if n % 1200 != 0 {
        return;
    }
    let count = ACQUIRES.swap(0, Ordering::Relaxed).max(1);
    log::warn!(
        "PAINTWAIT n={count} gap(avg={}us max={}us) tails={}",
        SUM_US.swap(0, Ordering::Relaxed) / count,
        MAX_US.swap(0, Ordering::Relaxed),
        TAILS.load(Ordering::Relaxed),
    );
}

/// 키를 모르는 작업(컨텍스트 생성 등)이 디바이스 락 전부를 배제하기 위한 상위 단계.
static ANGLE_GL_EXCLUSION: LazyLock<RwLock<()>> = LazyLock::new(|| RwLock::new(()));

/// ANGLE D3D11 디바이스 포인터 -> 그 디바이스의 락.
static ANGLE_GL_DEVICE_LOCKS: LazyLock<Mutex<HashMap<usize, &'static Mutex<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// [`angle_gl_lock`] 이 돌려주는 가드.
///
/// ★해제 순서는 `Drop` 이 명시한다★(2026-11-12) -- 전에는 필드 선언 순서에 기대고 있었는데,
/// 계측을 위해 `Drop` 을 구현하면서 그 본문이 필드 해제보다 먼저 돌게 됐다. 그래서 본문이
/// 획득의 역순(device -> shared -> exclusive)으로 직접 떨어뜨린다. 선언 순서도 그대로
/// 두었으므로 `Drop` 을 지우면 예전 동작으로 돌아간다.
pub struct AngleGlGuard {
    device: Option<MutexGuard<'static, ()>>,
    shared: Option<RwLockReadGuard<'static, ()>>,
    exclusive: Option<RwLockWriteGuard<'static, ()>>,
    /// 배타를 잡은 시각(배타일 때만). `Drop` 에서 보유 시간을 찍는다.
    held_since: Option<Instant>,
    /// 배타를 기다린 시간(배타일 때만).
    exclusive_wait: Duration,
}

impl Drop for AngleGlGuard {
    fn drop(&mut self) {
        // ★배타를 잡고 있던 시간을 찍는다.★ 근거는 `note_paint_gap` 의 doc 주석 -- 요점은
        // 이 구간에 네 painter 가 `render()` 에 못 들어가고, 그래서 어느 타일도 전환을
        // 진행하지 못한다는 것이다.
        //
        // 배타는 전환당 몇 번(컨텍스트 생성/복구 x 4 백엔드)이라 무조건 찍어도 I/O 가
        // 문제되지 않는다. 공유 쪽은 `note_paint_gap` 이 따로 센다.
        let Some(began) = self.held_since else {
            return;
        };
        let held = began.elapsed();

        // ★찍기 전에 락을 놓는다.★ `Drop` 본문은 필드 해제보다 ***먼저*** 돌므로, 그냥
        // 찍으면 `warn!` 의 I/O 가 배타 구간 안에 들어가 ***측정이 측정 대상을 늘린다***.
        // 이 구조체의 doc 주석이 "필드 선언 순서가 곧 해제 순서" 라고 적어 둔 것도 같은
        // 전제다 -- 획득의 역순으로 떨어뜨린다.
        drop(self.device.take());
        drop(self.shared.take());
        drop(self.exclusive.take());

        static EXCL: AtomicU64 = AtomicU64::new(0);
        let nth = EXCL.fetch_add(1, Ordering::Relaxed) + 1;
        log::warn!(
            "GLEXCL #{nth} held={}us (waited={}us) -- all painters were out of render()",
            held.as_micros(),
            self.exclusive_wait.as_micros(),
        );
    }
}

/// Sends messages to `Paint`.
#[derive(Clone)]
pub struct PaintProxy {
    pub sender: Sender<Result<PaintMessage, ipc_channel::IpcError>>,
    /// Access to [`Self::sender`] that is possible to send across an IPC
    /// channel. These messages are routed via the router thread to
    /// [`Self::sender`].
    pub cross_process_paint_api: CrossProcessPaintApi,
    pub event_loop_waker: Box<dyn EventLoopWaker>,
}

impl OpaqueSender<PaintMessage> for PaintProxy {
    fn send(&self, message: PaintMessage) {
        PaintProxy::send(self, message)
    }
}

impl PaintProxy {
    pub fn send(&self, msg: PaintMessage) {
        self.route_msg(Ok(msg))
    }

    /// Helper method to route a deserialized IPC message to the receiver.
    ///
    /// This method is a temporary solution, and will be removed when migrating
    /// to `GenericChannel`.
    pub fn route_msg(&self, msg: Result<PaintMessage, ipc_channel::IpcError>) {
        if let Err(err) = self.sender.send(msg) {
            warn!("Failed to send response ({:?}).", err);
        }
        self.event_loop_waker.wake();
    }
}

/// Messages from (or via) the constellation thread to `Paint`.
#[derive(Deserialize, IntoStaticStr, Serialize)]
pub enum PaintMessage {
    /// Alerts `Paint` that the given pipeline has changed whether it is running animations.
    ChangeRunningAnimationsState(WebViewId, PipelineId, AnimationState),
    /// Tell the embedder whether this `WebView` has animating content.
    ///
    /// ★`WebViewRenderer` 가 `Box<dyn WebViewTrait>` 를 직접 들고 부르던 것을 메시지로 바꾼
    /// 것이다.★ 그 트레이트 객체는 `Weak<RefCell<..>>` 기반이라 `Send` 가 될 수 없는데,
    /// 병렬 타일 렌더에서 `WebViewRenderer` 는 타일 스레드에 산다. 실제로 쓰던 메서드가
    /// `set_animating` 하나뿐이고 반환값도 없어(`id` 는 따로 들고 있고 `screen_geometry` 는
    /// paint 에서 한 번도 불리지 않는다) 통보 하나만 남기면 된다
    /// (`docs/multigpu/parallel_tile_render_design.md` §3.2).
    ///
    /// paint 내부 판정은 그대로 `WebViewRenderer::animating()` **필드**를 쓴다 — 이 메시지는
    /// 순수히 바깥(임베더 델리게이트)으로 나가는 통지이므로 한 홉 늦어도 판정이 흔들리지 않는다.
    SetWebViewAnimating(WebViewId, bool),
    /// Updates the frame tree for the given webview.
    SetFrameTreeForWebView(WebViewId, SendableFrameTree),
    /// Set whether to use less resources by stopping animations.
    SetThrottled(WebViewId, PipelineId, bool),
    /// WebRender has produced a new frame. This message informs `Paint` that
    /// the frame is ready. It contains a bool to indicate if it needs to composite, the
    /// `DocumentId` of the new frame and the `PainterId` of the associated painter.
    NewWebRenderFrameReady(PainterId, DocumentId, bool),
    /// Script or the Constellation is notifying the renderer that a Pipeline has finished
    /// shutting down. The renderer will not discard the Pipeline until both report that
    /// they have fully shut it down, to avoid recreating it due to any subsequent
    /// messages.
    PipelineExited(WebViewId, PipelineId, PipelineExitSource),
    /// Inform WebRender of the existence of this pipeline.
    SendInitialTransaction(WebViewId, WebRenderPipelineId),
    /// Scroll the given node ([`ExternalScrollId`]) by the provided delta. This
    /// will only adjust the node's scroll position and will *not* do panning in
    /// the pinch zoom viewport.
    ScrollNodeByDelta(
        WebViewId,
        WebRenderPipelineId,
        LayoutVector2D,
        ExternalScrollId,
    ),
    /// Scroll the WebView's viewport by the given delta. This will also do panning
    /// in the pinch zoom viewport if possible and the remaining delta will be used
    /// to scroll the root layer.
    ScrollViewportByDelta(WebViewId, LayoutVector2D),
    /// Update the rendering epoch of the given `Pipeline`.
    UpdateEpoch {
        /// The [`WebViewId`] that this display list belongs to.
        webview_id: WebViewId,
        /// The [`PipelineId`] of the `Pipeline` to update.
        pipeline_id: PipelineId,
        /// The new [`Epoch`] value.
        epoch: Epoch,
    },
    /// Inform WebRender of a new display list for the given pipeline.
    SendDisplayList {
        /// The [`WebViewId`] that this display list belongs to.
        webview_id: WebViewId,
        /// A descriptor of this display list used to construct this display list from raw data.
        display_list_descriptor: BuiltDisplayListDescriptor,
        /// A [`GenericReceiver`] used to send the [`PaintDisplayListInfo`].
        display_list_info_receiver: GenericReceiver<PaintDisplayListInfo>,
        /// A [`GenericReceiver`] used to send the serialized  version of `DisplayListPayload.
        display_list_data_receiver: GenericReceiver<SerializableDisplayListPayload>,
    },
    /// Ask the renderer to generate a frame for the current set of display lists
    /// from the given `PainterId`s that have been sent to the renderer.
    GenerateFrame(Vec<PainterId>),
    /// Query the paint targets currently registered for a logical `WebView`.
    GetWebViewPainterTargets(WebViewId, GenericSender<Vec<PainterId>>),
    /// Create a new image key. The result will be returned via the
    /// provided channel sender.
    GenerateImageKey(WebViewId, GenericSender<ImageKey>),
    /// The same as the above but it will be forwarded to the pipeline instead
    /// of send via a channel.
    GenerateImageKeysForPipeline(WebViewId, PipelineId),
    /// Perform a resource update operation.
    UpdateImages(PainterId, SmallVec<[ImageUpdate; 1]>),
    /// Pause all pipeline display list processing for the given pipeline until the
    /// following image updates have been received. This is used to ensure that canvas
    /// elements have had a chance to update their rendering and send the image update to
    /// the renderer before their associated display list is actually displayed.
    DelayNewFrameForCanvas(WebViewId, PipelineId, Epoch, Vec<ImageKey>),

    /// Generate a new batch of font keys which can be used to allocate
    /// keys asynchronously.
    GenerateFontKeys(
        usize,
        usize,
        GenericSender<(Vec<FontKey>, Vec<FontInstanceKey>)>,
        PainterId,
    ),
    /// Add a font with the given data and font key.
    AddFont(PainterId, FontKey, Arc<GenericSharedMemory>, u32),
    /// Add a system font with the given font key and handle.
    AddSystemFont(PainterId, FontKey, NativeFontHandle),
    /// Add an instance of a font with the given instance key.
    AddFontInstance(
        PainterId,
        FontInstanceKey,
        FontKey,
        f32,
        FontInstanceFlags,
        Vec<FontVariation>,
    ),
    /// Remove the given font resources from our WebRender instance.
    RemoveFonts(PainterId, Vec<FontKey>, Vec<FontInstanceKey>),
    /// Measure the current memory usage associated with `Paint`.
    /// The report must be sent on the provided channel once it's complete.
    CollectMemoryReport(ReportsChan),
    /// A top-level frame has parsed a viewport metatag and is sending the new constraints.
    Viewport(WebViewId, ViewportDescription),
    /// Let `Paint` know that the given WebView is ready to have a screenshot taken
    /// after the given pipeline's epochs have been rendered.
    ScreenshotReadinessReponse(WebViewId, FxHashMap<PipelineId, Epoch>),
    /// The candidate of largest-contentful-paint
    SendLCPCandidate(LCPCandidate, WebViewId, PipelineId, Epoch),
    /// Enable LCP calculation for the given WebView.
    EnableLCPCalculation(WebViewId),
}

impl Debug for PaintMessage {
    fn fmt(&self, formatter: &mut Formatter) -> Result<(), Error> {
        let string: &'static str = self.into();
        write!(formatter, "{string}")
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SendableFrameTree {
    pub pipeline: CompositionPipeline,
    pub children: Vec<SendableFrameTree>,
}

/// The subset of the pipeline that is needed for layer composition.
#[derive(Clone, Deserialize, Serialize)]
pub struct CompositionPipeline {
    pub id: PipelineId,
    pub webview_id: WebViewId,
}

/// A serializable version of `DisplayListPayload`.
#[derive(Clone, Serialize, Deserialize)]
pub struct SerializableDisplayListPayload {
    /// Serde encoded bytes of the display list' `DisplayItems` and their supporting data.
    #[serde(with = "serde_bytes")]
    pub items_data: Vec<u8>,

    /// Serde encoded `DisplayItemCache` structs
    #[serde(with = "serde_bytes")]
    pub cache_data: Vec<u8>,

    /// Serde encoded `SpatialTreeItem` structs.
    #[serde(with = "serde_bytes")]
    pub spatial_tree: Vec<u8>,
}

/// A mechanism to send messages from ScriptThread to the parent process' WebRender instance.
#[derive(Clone, Deserialize, MallocSizeOf, Serialize)]
pub struct CrossProcessPaintApi(GenericCallback<PaintMessage>);

impl CrossProcessPaintApi {
    /// Create a new [`CrossProcessPaintApi`] struct.
    pub fn new(callback: GenericCallback<PaintMessage>) -> Self {
        CrossProcessPaintApi(callback)
    }

    /// Create a new [`CrossProcessPaintApi`] struct that does not have a listener on the other
    /// end to use for unit testing.
    pub fn dummy() -> Self {
        Self::dummy_with_callback(None)
    }

    /// Create a new [`CrossProcessPaintApi`] struct for unit testing with an optional callback
    /// that can respond to `PaintMessage`s.
    pub fn dummy_with_callback(
        callback: Option<Box<dyn Fn(PaintMessage) + Send + 'static>>,
    ) -> Self {
        let callback = GenericCallback::new(move |msg| {
            if let Some(ref handler) = callback
                && let Ok(paint_message) = msg
            {
                handler(paint_message);
            }
        })
        .unwrap();
        Self(callback)
    }

    /// Inform WebRender of the existence of this pipeline.
    pub fn send_initial_transaction(&self, webview_id: WebViewId, pipeline: WebRenderPipelineId) {
        if let Err(e) = self
            .0
            .send(PaintMessage::SendInitialTransaction(webview_id, pipeline))
        {
            warn!("Error sending initial transaction: {}", e);
        }
    }

    /// Scroll the given node ([`ExternalScrollId`]) by the provided delta. This
    /// will only adjust the node's scroll position and will *not* do panning in
    /// the pinch zoom viewport.
    pub fn scroll_node_by_delta(
        &self,
        webview_id: WebViewId,
        pipeline_id: WebRenderPipelineId,
        delta: LayoutVector2D,
        scroll_id: ExternalScrollId,
    ) {
        if let Err(error) = self.0.send(PaintMessage::ScrollNodeByDelta(
            webview_id,
            pipeline_id,
            delta,
            scroll_id,
        )) {
            warn!("Error scrolling node: {error}");
        }
    }

    /// Scroll the WebView's viewport by the given delta. This will also do panning
    /// in the pinch zoom viewport if possible and the remaining delta will be used
    /// to scroll the root layer.
    ///
    /// Note the value provided here is in `DeviceIndependentPixels` and will first be
    /// converted to `DevicePixels` by the renderer.
    pub fn scroll_viewport_by_delta(&self, webview_id: WebViewId, delta: LayoutVector2D) {
        if let Err(error) = self
            .0
            .send(PaintMessage::ScrollViewportByDelta(webview_id, delta))
        {
            warn!("Error scroll viewport: {error}");
        }
    }

    pub fn delay_new_frame_for_canvas(
        &self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        canvas_epoch: Epoch,
        image_keys: Vec<ImageKey>,
    ) {
        if let Err(error) = self.0.send(PaintMessage::DelayNewFrameForCanvas(
            webview_id,
            pipeline_id,
            canvas_epoch,
            image_keys,
        )) {
            warn!("Error delaying frames for canvas image updates {error:?}");
        }
    }

    /// Inform the renderer that the rendering epoch has advanced. This typically happens after
    /// a new display list is sent and/or canvas and animated images are updated.
    pub fn update_epoch(&self, webview_id: WebViewId, pipeline_id: PipelineId, epoch: Epoch) {
        if let Err(error) = self.0.send(PaintMessage::UpdateEpoch {
            webview_id,
            pipeline_id,
            epoch,
        }) {
            warn!("Error updating epoch for pipeline: {error:?}");
        }
    }

    /// Inform WebRender of a new display list for the given pipeline.
    /// We send the `PaintDisplayListInfo` and `DisplayListPayload` separately to not overwhelm
    /// the ipc_channel (see <https://github.com/servo/servo/pull/36484>)
    #[servo_tracing::instrument(skip_all)]
    pub fn send_display_list(
        &self,
        webview_id: WebViewId,
        display_list_info: &PaintDisplayListInfo,
        list: BuiltDisplayList,
    ) {
        let (display_list_data, display_list_descriptor) = list.into_data();
        let (display_list_data_sender, display_list_data_receiver) =
            generic_channel::channel().unwrap();
        let (display_list_info_sender, display_list_info_receiver) =
            generic_channel::channel().unwrap();
        if let Err(e) = self.0.send(PaintMessage::SendDisplayList {
            webview_id,
            display_list_descriptor,
            display_list_info_receiver,
            display_list_data_receiver,
        }) {
            warn!("Error sending display list: {}", e);
        }

        if let Err(error) = display_list_info_sender.send(display_list_info.clone()) {
            warn!("Error sending display list info: {error}. Not sending the rest");
            return;
        }
        let display_list_data = SerializableDisplayListPayload {
            items_data: display_list_data.items_data,
            cache_data: display_list_data.cache_data,
            spatial_tree: display_list_data.spatial_tree,
        };

        if let Err(error) = display_list_data_sender.send(display_list_data) {
            warn!("Error sending display list: {error}");
        }
    }

    /// Send the largest contentful paint candidate to `Paint`.
    pub fn send_lcp_candidate(
        &self,
        lcp_candidate: LCPCandidate,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        epoch: Epoch,
    ) {
        if let Err(error) = self.0.send(PaintMessage::SendLCPCandidate(
            lcp_candidate,
            webview_id,
            pipeline_id,
            epoch,
        )) {
            warn!("Error sending LCPCandidate: {error}");
        }
    }

    /// Ask the Servo renderer to generate a new frame after having new display lists.
    pub fn generate_frame(&self, painter_ids: Vec<PainterId>) {
        if let Err(error) = self.0.send(PaintMessage::GenerateFrame(painter_ids)) {
            warn!("Error generating frame: {error}");
        }
    }

    /// Query registered paint targets for a logical `WebView`.
    ///
    /// Wall-all-tiles registers one target per tile; normal single-window
    /// rendering falls back to the primary painter id derived from `webview_id`.
    pub fn webview_painter_targets_blocking(&self, webview_id: WebViewId) -> Vec<PainterId> {
        let (sender, receiver) = generic_channel::channel().unwrap();
        if self
            .0
            .send(PaintMessage::GetWebViewPainterTargets(webview_id, sender))
            .is_err()
        {
            return vec![webview_id.into()];
        }
        receiver.recv().unwrap_or_else(|_| vec![webview_id.into()])
    }

    /// Create a new image key. Blocks until the key is available.
    pub fn generate_image_key_blocking(&self, webview_id: WebViewId) -> Option<ImageKey> {
        let (sender, receiver) = generic_channel::channel().unwrap();
        self.0
            .send(PaintMessage::GenerateImageKey(webview_id, sender))
            .ok()?;
        receiver.recv().ok()
    }

    /// Sends a message to `Paint` for creating new image keys.
    /// `Paint` will then send a batch of keys over the constellation to the script_thread
    /// and the appropriate pipeline.
    pub fn generate_image_key_async(&self, webview_id: WebViewId, pipeline_id: PipelineId) {
        if let Err(e) = self.0.send(PaintMessage::GenerateImageKeysForPipeline(
            webview_id,
            pipeline_id,
        )) {
            warn!("Could not send image keys to Paint {}", e);
        }
    }

    pub fn add_image(
        &self,
        key: ImageKey,
        descriptor: ImageDescriptor,
        data: SerializableImageData,
        is_animated_image: bool,
    ) {
        self.update_images(
            key.into(),
            [ImageUpdate::AddImage(
                key,
                descriptor,
                data,
                is_animated_image,
            )]
            .into(),
        );
    }

    pub fn update_image(
        &self,
        key: ImageKey,
        descriptor: ImageDescriptor,
        data: SerializableImageData,
        epoch: Option<Epoch>,
    ) {
        self.update_images(
            key.into(),
            [ImageUpdate::UpdateImage(key, descriptor, data, epoch)].into(),
        );
    }

    pub fn delete_image(&self, key: ImageKey) {
        self.update_images(key.into(), [ImageUpdate::DeleteImage(key)].into());
    }

    /// Perform an image resource update operation.
    pub fn update_images(&self, painter_id: PainterId, updates: SmallVec<[ImageUpdate; 1]>) {
        if let Err(e) = self.0.send(PaintMessage::UpdateImages(painter_id, updates)) {
            warn!("error sending image updates: {}", e);
        }
    }

    pub fn remove_unused_font_resources(
        &self,
        painter_id: PainterId,
        keys: Vec<FontKey>,
        instance_keys: Vec<FontInstanceKey>,
    ) {
        if keys.is_empty() && instance_keys.is_empty() {
            return;
        }
        let _ = self
            .0
            .send(PaintMessage::RemoveFonts(painter_id, keys, instance_keys));
    }

    pub fn add_font_instance(
        &self,
        font_instance_key: FontInstanceKey,
        font_key: FontKey,
        size: f32,
        flags: FontInstanceFlags,
        variations: Vec<FontVariation>,
    ) {
        let _x = self.0.send(PaintMessage::AddFontInstance(
            font_key.into(),
            font_instance_key,
            font_key,
            size,
            flags,
            variations,
        ));
    }

    pub fn add_font(&self, font_key: FontKey, data: Arc<GenericSharedMemory>, index: u32) {
        let _ = self.0.send(PaintMessage::AddFont(
            font_key.into(),
            font_key,
            data,
            index,
        ));
    }

    pub fn add_system_font(&self, font_key: FontKey, handle: NativeFontHandle) {
        let _ = self.0.send(PaintMessage::AddSystemFont(
            font_key.into(),
            font_key,
            handle,
        ));
    }

    pub fn fetch_font_keys(
        &self,
        number_of_font_keys: usize,
        number_of_font_instance_keys: usize,
        painter_id: PainterId,
    ) -> (Vec<FontKey>, Vec<FontInstanceKey>) {
        let (sender, receiver) = generic_channel::channel().expect("Could not create IPC channel");
        let _ = self.0.send(PaintMessage::GenerateFontKeys(
            number_of_font_keys,
            number_of_font_instance_keys,
            sender,
            painter_id,
        ));
        receiver.recv().unwrap()
    }

    pub fn viewport(&self, webview_id: WebViewId, description: ViewportDescription) {
        let _ = self.0.send(PaintMessage::Viewport(webview_id, description));
    }

    pub fn pipeline_exited(
        &self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        source: PipelineExitSource,
    ) {
        let _ = self.0.send(PaintMessage::PipelineExited(
            webview_id,
            pipeline_id,
            source,
        ));
    }
}

#[derive(Clone)]
pub struct PainterSurfmanDetails {
    pub connection: Connection,
    pub adapter: Adapter,
}

/// 한 painter 의 타일이 ★가상 뷰포트의 어느 사각형인가★, device px.
///
/// ★WebGL 스레드가 이것을 필요로 하는 이유.★ 이 포크는 모든 WebGL 명령을 painter 마다의
/// 별도 GL 컨텍스트(= 별도 GPU)에 복제하고, ***각 백엔드의 드로잉 버퍼는 캔버스 전체
/// 크기***다. 그런데 한 타일은 그중 자기 몫만 표출한다. 캔버스가 벽 전체를 덮으면 네 GPU
/// 가 각각 11520x4320 을 셰이딩하고 ★12.4Mpx 만 쓰고 75% 를 버린다.★
///
/// 실측(2026-11-03/09, log_webgl_perf_01/09, 4 타일 테스트 장비): 그 조건에서 저더가 나고
/// `RENDERCOST` 의 주기 초과가 정상 상태에서 10 초당 23 회였다. 페이지 쪽에서 백킹 스토어만
/// 1/2 로 줄여(면적 1/4) 돌리면 ***초과가 0 이 되고 저더가 사라진다.*** 즉 ★면적 축★ 이고,
/// 각 백엔드가 자기 타일 몫만 셰이딩하면 같은 양을 아낀다 -- 그쪽은 해상도를 잃지 않는다.
///
/// ★좌표계★: `tile` 과 `viewport` 는 둘 다 ***가상 뷰포트의 device px, 위-왼쪽 원점***이다.
/// GL scissor 는 아래-왼쪽 원점이므로 쓰는 쪽에서 뒤집어야 하고, 외부 이미지가
/// `needs_vertical_flip` 으로 합성되는 것까지 겹쳐 방향이 자명하지 않았다. 실기로 확정됐고
/// (2026-11-09, 진단 색칠로 네 타일의 색 배치를 확인) 뒤집기는 소비자가 고정값으로 한다 --
/// `webgl_thread::scale_canvas_rect`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PainterTileRect {
    /// 이 painter 의 타일 사각형, 가상 뷰포트 device px.
    pub tile: DeviceIntRect,
    /// 가상 뷰포트 전체 크기, device px. ★쓰는 쪽이 "캔버스가 뷰포트를 꽉 채우는가" 를
    /// 이것으로 검사한다★ -- 아니면 캔버스 좌표와 뷰포트 좌표가 1:1 이 아니므로 scissor 를
    /// 걸면 ***엉뚱한 사분면***이 보인다.
    pub viewport: DeviceIntSize,
}

/// `PainterId` -> [`PainterTileRect`]. ★WebGL 스레드가 읽는 유일한 타일 기하 통로다.★
///
/// [`PainterSurfmanDetailsMap`] 과 같은 모양이되 ★`insert` 가 덮어쓰기를 허용한다★ --
/// `update_webview_paint_target` 이 원점을 언제든 다시 쓸 수 있다. 그쪽은 중복 등록이
/// 버그라서 `assert` 를 두지만, 여기서는 갱신이 정상이다.
#[derive(Clone, Default)]
pub struct PainterTileRectMap(Arc<Mutex<HashMap<PainterId, PainterTileRect>>>);

impl PainterTileRectMap {
    pub fn get(&self, painter_id: PainterId) -> Option<PainterTileRect> {
        let map = self.0.lock().expect("poisoned");
        map.get(&painter_id).copied()
    }

    /// 등록하거나 갱신한다. ★덮어쓰기가 정상이다★(위 doc 참고).
    pub fn insert(&self, painter_id: PainterId, rect: PainterTileRect) {
        let mut map = self.0.lock().expect("poisoned");
        map.insert(painter_id, rect);
    }

    pub fn remove(&self, painter_id: PainterId) {
        let mut map = self.0.lock().expect("poisoned");
        map.remove(&painter_id);
    }
}

#[derive(Clone, Default)]
pub struct PainterSurfmanDetailsMap(Arc<Mutex<HashMap<PainterId, PainterSurfmanDetails>>>);

impl PainterSurfmanDetailsMap {
    pub fn get(&self, painter_id: PainterId) -> Option<PainterSurfmanDetails> {
        let map = self.0.lock().expect("poisoned");
        map.get(&painter_id).cloned()
    }

    pub fn insert(&self, painter_id: PainterId, details: PainterSurfmanDetails) {
        let mut map = self.0.lock().expect("poisoned");
        let existing = map.insert(painter_id, details);
        assert!(existing.is_none())
    }

    pub fn remove(&self, painter_id: PainterId) {
        let mut map = self.0.lock().expect("poisoned");
        let details = map.remove(&painter_id);
        assert!(details.is_some());
    }
}

/// This trait is used as a bridge between the different GL clients
/// in Servo that handles WebRender ExternalImages and the WebRender
/// ExternalImageHandler API.
//
/// This trait is used to notify lock/unlock messages and get the
/// required info that WR needs.
pub trait WebRenderExternalImageApi {
    fn lock(&mut self, id: u64) -> (ExternalImageSource<'_>, UntypedSize2D<i32>);
    fn unlock(&mut self, id: u64);
    /// WR에 전달할 uv가 수직 플립되어야 하는지. 기존 미디어/GL 텍스처는
    /// 하단-상단(GL 관례)이라 플립이 필요하고(기본값 true), GPU 상주 D3D11
    /// 비디오 텍스처는 상단-하단이라 플립하지 않는다.
    fn needs_vertical_flip(&mut self, _id: u64) -> bool {
        true
    }
}

/// D3D11 plane 텍스처의 데이터 레이아웃(WR YUV 직접 샘플 경로와 동일한
/// 4종). DComp external surface interop(Task 5)이 이 값으로 변환 셰이더/
/// 서피스 포맷을 고른다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoLeaseFormat {
    I420,
    I420_10,
    Nv12,
    P010,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoLeaseColorSpace {
    Rec601,
    Rec709,
    Rec2020,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoLeaseColorRange {
    Limited,
    Full,
}

/// plane 하나(텍스처+크기). `texture`는 AddRef 유지 중인 `ID3D11Texture2D`
/// (DYNAMIC, ANGLE 디바이스 소속)의 불투명 핸들이다.
#[derive(Clone, Copy, Debug)]
pub struct VideoLeasePlane {
    pub texture: usize,
    pub width: i32,
    pub height: i32,
}

/// [`VideoExternalSurfaceProvider::acquire`]가 반환하는, 렌더러 스레드가
/// 링에서 대여한 프레임 하나. `release`와 반드시 짝을 맞춰야 한다.
#[derive(Clone, Copy, Debug)]
pub struct VideoFrameLease {
    pub ring_id: u64,
    pub planes: [Option<VideoLeasePlane>; 3],
    pub plane_count: usize,
    pub format: VideoLeaseFormat,
    pub color_space: VideoLeaseColorSpace,
    pub color_range: VideoLeaseColorRange,
    /// presenting 슬롯의 filled_seq — 변화 없으면 재변환 스킵.
    pub frame_seq: u64,
}

/// 렌더러 스레드 전용. acquire는 링 잠금(0→1)+소비 계획 실행까지 수행 —
/// release와 반드시 짝맞춤.
pub trait VideoExternalSurfaceProvider: Send + Sync {
    fn acquire(&self, rc: &dyn RenderingContext, external_id: u64) -> Option<VideoFrameLease>;
    fn release(&self, rc: &dyn RenderingContext, ring_id: u64);
}

static VIDEO_EXTERNAL_PROVIDER: std::sync::OnceLock<
    std::sync::Arc<dyn VideoExternalSurfaceProvider>,
> = std::sync::OnceLock::new();

pub fn set_video_external_surface_provider(p: std::sync::Arc<dyn VideoExternalSurfaceProvider>) {
    // 단일 프로세스/단일 등록 전제 (CONSUMER_DEVICE와 동일 한계 — §4.5 다중창 이월과 정합)
    let _ = VIDEO_EXTERNAL_PROVIDER.set(p);
}

pub fn video_external_surface_provider()
-> Option<&'static std::sync::Arc<dyn VideoExternalSurfaceProvider>> {
    VIDEO_EXTERNAL_PROVIDER.get()
}

/// Type of WebRender External Image Handler.
#[derive(Clone, Copy)]
pub enum WebRenderImageHandlerType {
    WebGl,
    Media,
    WebGpu,
}

/// List of WebRender external images to be shared among all external image
/// consumers (WebGL, Media, WebGPU).
/// It ensures that external image identifiers are unique.
#[derive(Default)]
struct WebRenderExternalImageIdManagerInner {
    /// Map of all generated external images.
    external_images: FxHashMap<ExternalImageId, WebRenderImageHandlerType>,
    /// Id generator for the next external image identifier.
    next_image_id: u64,
}

#[derive(Default, Clone)]
pub struct WebRenderExternalImageIdManager(Arc<RwLock<WebRenderExternalImageIdManagerInner>>);

impl WebRenderExternalImageIdManager {
    pub fn next_id(&mut self, handler_type: WebRenderImageHandlerType) -> ExternalImageId {
        let mut inner = self.0.write();
        inner.next_image_id += 1;
        let key = ExternalImageId(inner.next_image_id);
        inner.external_images.insert(key, handler_type);
        key
    }

    pub fn remove(&mut self, key: &ExternalImageId) {
        self.0.write().external_images.remove(key);
    }

    pub fn get(&self, key: &ExternalImageId) -> Option<WebRenderImageHandlerType> {
        self.0.read().external_images.get(key).cloned()
    }
}

/// WebRender External Image Handler implementation.
pub struct WebRenderExternalImageHandlers {
    /// WebGL handler.
    webgl_handler: Option<Box<dyn WebRenderExternalImageApi>>,
    /// Media player handler.
    media_handler: Option<Box<dyn WebRenderExternalImageApi>>,
    /// WebGPU handler.
    webgpu_handler: Option<Box<dyn WebRenderExternalImageApi>>,
    /// A [`WebRenderExternalImageIdManager`] responsible for creating new [`ExternalImageId`]s.
    /// This is shared with the WebGL, WebGPU, and hardware-accelerated media threads and
    /// all other instances of [`WebRenderExternalImageHandlers`] -- one per WebRender instance.
    id_manager: WebRenderExternalImageIdManager,
}

impl WebRenderExternalImageHandlers {
    pub fn new(id_manager: WebRenderExternalImageIdManager) -> Self {
        Self {
            webgl_handler: Default::default(),
            media_handler: Default::default(),
            webgpu_handler: Default::default(),
            id_manager,
        }
    }

    pub fn id_manager(&self) -> WebRenderExternalImageIdManager {
        self.id_manager.clone()
    }

    pub fn set_handler(
        &mut self,
        handler: Box<dyn WebRenderExternalImageApi>,
        handler_type: WebRenderImageHandlerType,
    ) {
        match handler_type {
            WebRenderImageHandlerType::WebGl => self.webgl_handler = Some(handler),
            WebRenderImageHandlerType::Media => self.media_handler = Some(handler),
            WebRenderImageHandlerType::WebGpu => self.webgpu_handler = Some(handler),
        }
    }
}

/// 회수된 외부 이미지 ID 로 들어온 lock/unlock 을 보고한다.
///
/// 타일당 프레임당 불릴 수 있는 자리라 무제한으로 찍으면 로그가 무너진다. 앞의 몇 건만
/// 남기고 침묵하되, **누적 횟수를 함께 찍어** 한 번 스친 것인지 계속 새는 것인지 구분되게
/// 한다(마지막 한 줄이 총계 역할을 한다).
fn report_unknown_external_image(phase: &str, key: ExternalImageId) {
    use std::sync::atomic::{AtomicU32, Ordering};
    const REPORT_LIMIT: u32 = 8;
    static SEEN: AtomicU32 = AtomicU32::new(0);

    let seen = SEEN.fetch_add(1, Ordering::Relaxed) + 1;
    if seen > REPORT_LIMIT {
        return;
    }
    let tail = if seen == REPORT_LIMIT {
        " (further reports suppressed)"
    } else {
        ""
    };
    log::warn!(
        "[external-image] {phase}: id {} unregistered; skipping frame ({seen}{tail})",
        key.0,
    );
}

impl ExternalImageHandler for WebRenderExternalImageHandlers {
    /// Lock the external image. Then, WR could start to read the
    /// image content.
    /// The WR client should not change the image content until the
    /// unlock() call.
    fn lock(
        &mut self,
        key: ExternalImageId,
        _channel_index: u8,
        _is_composited: bool,
    ) -> ExternalImage<'_> {
        let Some(handler_type) = self.id_manager().get(&key) else {
            // ★외부 이미지 ID 가 이미 회수됐는데 WR 이 아직 그 ImageKey 를 참조하는 창★
            //
            // 정리 경로가 두 채널로 갈라져 있어서 생긴다(htmlmediaelement 의
            // MediaFrameRenderer::reset): `remove_plane` 은 전역 맵을 **즉시** 지우는데
            // `DeleteImage` 는 paint 채널로 **큐잉**된다. 그 사이에 합성이 한 번 끼면
            // 여기로 들어온다 — 페이지 새로고침으로 재생 중이던 <video> 가 헐릴 때가
            // 대표적이고, devtools 를 붙이면 스크립트 스레드가 느려져 창이 넓어진다.
            //
            // 예전에는 여기서 패닉해 **메인 스레드가 죽고 표출 전체가 멈췄다.** 한 프레임을
            // 건너뛰는 편이 낫다 — 바로 아래 WebGL 분기가 같은 이유로 이미 그렇게 한다.
            // 근본 수정은 회수를 DeleteImage 와 같은 채널로 보내 순서를 구조적으로
            // 보장하는 것이고, 이 강등은 그때까지의 안전망이다.
            report_unknown_external_image("lock", key);
            return ExternalImage {
                uv: TexelRect::new(0.0, 0.0, 0.0, 0.0),
                source: ExternalImageSource::Invalid,
            };
        };
        match handler_type {
            WebRenderImageHandlerType::WebGl => {
                let (source, size) = self.webgl_handler.as_mut().unwrap().lock(key.0);
                // A WebGL context may not have a presentable front buffer yet (its first
                // frame has not completed, or rendering failed). In that case lock()
                // returns ExternalImageSource::Invalid; pass it through so WebRender skips
                // compositing this frame instead of panicking.
                ExternalImage {
                    uv: TexelRect::new(0.0, size.height as f32, size.width as f32, 0.0),
                    source,
                }
            },
            WebRenderImageHandlerType::Media => {
                let media_handler = self.media_handler.as_mut().unwrap();
                let needs_vertical_flip = media_handler.needs_vertical_flip(key.0);
                let (source, size) = media_handler.lock(key.0);
                let uv = if needs_vertical_flip {
                    TexelRect::new(0.0, size.height as f32, size.width as f32, 0.0)
                } else {
                    TexelRect::new(0.0, 0.0, size.width as f32, size.height as f32)
                };
                ExternalImage { uv, source }
            },
            WebRenderImageHandlerType::WebGpu => {
                let (source, size) = self.webgpu_handler.as_mut().unwrap().lock(key.0);
                ExternalImage {
                    uv: TexelRect::new(0.0, size.height as f32, size.width as f32, 0.0),
                    source,
                }
            },
        }
    }

    /// Unlock the external image. The WR should not read the image
    /// content after this call.
    fn unlock(&mut self, key: ExternalImageId, _channel_index: u8) {
        let Some(handler_type) = self.id_manager().get(&key) else {
            // lock() 이 위에서 Invalid 를 돌려준 그 키다. 잠근 것이 없으니 풀 것도 없다.
            // 여기서도 패닉하면 강등이 무의미해진다(같은 프레임에서 바로 이어 불린다).
            report_unknown_external_image("unlock", key);
            return;
        };
        match handler_type {
            WebRenderImageHandlerType::WebGl => self.webgl_handler.as_mut().unwrap().unlock(key.0),
            WebRenderImageHandlerType::Media => self.media_handler.as_mut().unwrap().unlock(key.0),
            WebRenderImageHandlerType::WebGpu => {
                self.webgpu_handler.as_mut().unwrap().unlock(key.0)
            },
        };
    }
}

#[derive(Clone, Deserialize, Serialize)]
/// Serializable image updates that must be performed by WebRender.
pub enum ImageUpdate {
    /// Register a new image.
    AddImage(
        ImageKey,
        ImageDescriptor,
        SerializableImageData,
        bool, /* is_animated_image */
    ),
    /// Delete a previously registered image registration.
    DeleteImage(ImageKey),
    /// Update an existing image registration.
    UpdateImage(
        ImageKey,
        ImageDescriptor,
        SerializableImageData,
        Option<Epoch>,
    ),
    /// Update an [`ImageDescriptor`] for an existing image. This is used primarily
    /// to modify the data offset for image animations.
    UpdateImageForAnimation(ImageKey, ImageDescriptor),
}

impl Debug for ImageUpdate {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddImage(image_key, image_desc, _, is_animated_image) => f
                .debug_tuple("AddImage")
                .field(image_key)
                .field(image_desc)
                .field(is_animated_image)
                .finish(),
            Self::DeleteImage(image_key) => f.debug_tuple("DeleteImage").field(image_key).finish(),
            Self::UpdateImage(image_key, image_desc, _, epoch) => f
                .debug_tuple("UpdateImage")
                .field(image_key)
                .field(image_desc)
                .field(epoch)
                .finish(),
            Self::UpdateImageForAnimation(image_key, image_desc) => f
                .debug_tuple("UpdateAnimation")
                .field(image_key)
                .field(image_desc)
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
/// Serialized `ImageData`.
pub enum SerializableImageData {
    /// A simple series of bytes, provided by the embedding and owned by WebRender.
    /// The format is stored out-of-band, currently in ImageDescriptor.
    Raw(GenericSharedMemory),
    /// An image owned by the embedding, and referenced by WebRender. This may
    /// take the form of a texture or a heap-allocated buffer.
    External(ExternalImageData),
}

impl From<SerializableImageData> for ImageData {
    fn from(value: SerializableImageData) -> Self {
        match value {
            SerializableImageData::Raw(shared_memory) => {
                ImageData::Raw(shared_memory.into_arc_vec())
            },
            SerializableImageData::External(image) => ImageData::External(image),
        }
    }
}

/// A trait that exposes the embedding layer's `WebView` to the Servo renderer.
/// This is to prevent a dependency cycle between the renderer and the embedding
/// layer.
pub trait WebViewTrait {
    fn id(&self) -> WebViewId;
    fn screen_geometry(&self) -> Option<ScreenGeometry>;
    fn set_animating(&self, new_value: bool);
}

/// What entity is reporting that a `Pipeline` has exited. Only when all have
/// done this will the renderer discard its details.
#[derive(Clone, Copy, Default, Deserialize, PartialEq, Serialize)]
pub struct PipelineExitSource(u8);

bitflags! {
    impl PipelineExitSource: u8 {
        const Script = 1 << 0;
        const Constellation = 1 << 1;
    }
}

/// A [`PinchZoomInfos`] for a root [`Pipeline`] of an [`WebView`]. For any [`Pipeline`]
/// that is not a root, it should follow the viewport description of its pipeline since
/// pinch-zoom and resizing due to overlay UIs are not applicable there.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct PinchZoomInfos {
    /// The zoom factor (or pinch-zoom).
    pub zoom_factor: Scale<f32, DevicePixel, DevicePixel>,

    /// The size relative to layout viewport.
    pub rect: Rect<f32, CSSPixel>,
}

impl PinchZoomInfos {
    /// New initial [`PinchZoomInfos`] without any pinch-zoom or resizing from a viewport size
    /// for a nested pipeline or newly initialized root pipeline.
    pub fn new_from_viewport_size(size: Size2D<f32, CSSPixel>) -> Self {
        Self {
            zoom_factor: Scale::identity(),
            rect: Rect::from_size(size),
        }
    }
}

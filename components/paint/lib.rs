/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![deny(unsafe_code)]

use std::cell::Cell;
use std::rc::Rc;

use crossbeam_channel::Sender;
use embedder_traits::{EventLoopWaker, ShutdownState};
use paint_api::{PaintMessage, PaintProxy};
use profile_traits::{mem, time};
use servo_base::generic_channel::RoutedReceiver;
use servo_constellation_traits::EmbedderToConstellationMessage;
#[cfg(feature = "webxr")]
use webxr::WebXrRegistry;

pub use crate::paint::{
    Paint, PaintTargetsInFlight, RenderingContextFactory, WebRenderDebugOption,
};

/// ★DWM 합성 격자.★ `(vblank 의 QPC, 한 주기의 QPC 틱)` -- 조회만 한다, 블록하지 않는다.
///
/// 셸의 표출 클럭이 **자기 기준점에 주기를 더해 나가는 대신 이 격자에 스냅**하라고 있는
/// 함수다. 자유 구동 타이머에 상수 오프셋을 더하는 것으로는 안 된다 -- 기준점이 매 실행
/// 임의라서 결과도 임의다. 매 틱 측정된 vblank 로부터 다시 계산해야 기동 위상과 무관하게
/// 같은 자리에 떨어진다.
///
/// 왜 필요한가: 실측(log_ani_debug/47)에서 깨끗한 런은 커밋이 주기의 8% 지점에,
/// 저더 런은 60% 지점에 떨어졌고 둘 다 런 내내 그 자리를 유지했다. DWM 은 vblank 이전에
/// 미리 합성을 시작하므로 60% 지점 커밋은 그 마감에 걸터앉아 프레임마다 되느냐 마느냐가
/// 갈린다. 어느 자리에 앉을지는 지금 순전히 운이다.
///
/// ★대기를 넣지 말 것.★ `DCompositionWaitForCompositorClock` 같은 프로세스 범위 대기는
/// 생산 스레드가 공유 객체에 줄 서는 회귀가 된다(`GstSystemClock` 사건, `-SinkPacing
/// thread` 가 그 대응). 이 함수는 조회이고, 호출자는 **타이머의 시각을 옮기는 데만** 쓴다.
///
/// Windows 가 아니거나 DWM 이 답하지 않으면 `None` -- 그때는 호출자가 자유 구동으로 돈다.
pub fn dwm_composition_grid() -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        crate::dcomp_compositor::composition_grid()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[macro_use]
mod tracing;

/// WR Native Compositor(DirectComposition) 구현. Windows 전용, painter(Task 5)가 결선한다.
#[cfg(windows)]
mod dcomp_compositor;
#[cfg(windows)]
mod output_grid;
/// raw D3D11 YUV→RGBA 변환 패스(VideoConvertPass, 비디오 WR 탈출 사이클 Task 4).
/// dcomp_compositor(Task 5)가 external compositor surface 경로에서 소비한다.
#[cfg(windows)]
mod dcomp_video_convert;
mod largest_contentful_paint_calculator;
mod paint;
mod painter;
mod pinch_zoom;
mod pipeline_details;
mod refresh_driver;
mod render_notifier;
mod screenshot;
mod touch;
mod web_content_animation;
mod webrender_external_images;
mod webview_renderer;

/// Data used to initialize the `Paint` subsystem.
pub struct InitialPaintState {
    /// A channel to `Paint`.
    pub paint_proxy: PaintProxy,
    /// A port on which messages inbound to `Paint` can be received.
    pub receiver: RoutedReceiver<PaintMessage>,
    /// A channel to the constellation.
    pub embedder_to_constellation_sender: Sender<EmbedderToConstellationMessage>,
    /// A channel to the time profiler thread.
    pub time_profiler_chan: time::ProfilerChan,
    /// A channel to the memory profiler thread.
    pub mem_profiler_chan: mem::ProfilerChan,
    /// A shared state which tracks whether Servo has started or has finished
    /// shutting down.
    pub shutdown_state: Rc<Cell<ShutdownState>>,
    /// An [`EventLoopWaker`] used in order to wake up the embedder when it is
    /// time to paint.
    pub event_loop_waker: Box<dyn EventLoopWaker>,
    /// If WebXR is enabled, a [`WebXrRegistry`] to register WebXR threads.
    #[cfg(feature = "webxr")]
    pub webxr_registry: Box<dyn WebXrRegistry>,
}

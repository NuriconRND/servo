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

pub use crate::paint::{Paint, RenderingContextFactory, WebRenderDebugOption};

#[macro_use]
mod tracing;

/// WR Native Compositor(DirectComposition) 구현. Windows 전용, painter(Task 5)가 결선한다.
#[cfg(windows)]
mod dcomp_compositor;
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

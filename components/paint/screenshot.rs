/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;
use std::rc::Rc;

use embedder_traits::ScreenshotCaptureError;
use euclid::{Point2D, Size2D};
use image::RgbaImage;
use log::error;
use rustc_hash::FxHashMap;
use servo_base::Epoch;
use servo_base::id::{PipelineId, WebViewId};
use webrender_api::units::{DeviceIntRect, DeviceRect};

use crate::paint::RepaintReason;
use crate::painter::Painter;

pub(crate) struct ScreenshotRequest {
    webview_id: WebViewId,
    rect: Option<DeviceRect>,
    callback: Box<dyn FnOnce(Result<RgbaImage, ScreenshotCaptureError>) + 'static>,
    phase: ScreenshotRequestPhase,
}

/// Screenshots requests happen in three phases:
#[derive(PartialEq)]
pub(crate) enum ScreenshotRequestPhase {
    /// A request is sent to the Constellation, asking each Pipeline in a WebView,
    /// to report the display list epoch to render for the screenshot. Each Pipeline
    /// will wait to send an epoch that happens after the Pipeline is ready in a
    /// variety of ways:
    ///
    ///  - The `load` event has fired.
    ///  - All render blocking elements are no longer blocking the rendering.
    ///  - All images are loaded and displayed.
    ///  - All web fonts are loaded.
    ///  - The `reftest-wait` and `test-wait` classes have been removed from the root element.
    ///  - The rendering is up-to-date
    ///
    /// When all Pipelines have reported this epoch to the Constellation it sends a
    /// ScreenshotReadinessResponse back to the renderer.
    ConstellationRequest,
    /// The renderer has received the ScreenshotReadinessReponse from the Constellation
    /// and is now waiting for all display lists to be received from the Pipelines and
    /// sent to WebRender.
    WaitingOnPipelineDisplayLists(Rc<FxHashMap<PipelineId, Epoch>>),
    /// Once the renderer has received all of the Pipeline display lists necessary to take
    /// the screenshot and uploaded them to WebRender, it waits for an appropriate frame to
    /// be ready. Currently this just waits for the [`FrameDelayer`] to stop delaying frames
    /// and for there to be no pending WebRender frames (ones sent to WebRender that are not
    /// ready yet). Once that happens, and a potential extra repaint is triggered, the renderer
    /// will take the screenshot and fufill the request.
    WaitingOnFrame,
}

impl ScreenshotRequestPhase {
    /// ★이 상에 있는 요청이 틱의 생산을 쉬게 하는가.★ 2026-10-22 Task 9, 설계 2-8.
    ///
    /// 참인 상은 ★`WaitingOnFrame` 하나뿐★ 이고, 근거는 하나다: ***그 상을 떠나는 데
    /// 생산이 필요 없다.*** `maybe_take_screenshots` 는 `pending_frames` 가 0 이 되기만
    /// 기다리고, 그 숫자를 빼는 것은 WebRender 의 frame-ready 다
    /// (`Painter::note_webrender_frame_ready`) -- 렌더 패스도 새 생산도 요구하지 않는다.
    ///
    /// 앞의 두 상은 ★반드시 거짓이어야 한다 -- 참이면 영구 정지다.★
    /// `WaitingOnPipelineDisplayLists` 를 떠나려면
    /// [`ScreenshotTaker::prepare_screenshot_requests_for_render`] 가 돌아야 하는데, 그
    /// 주 호출자는 `Painter::generate_frame_for_wall_tick` 의 `Generated` 갈래다
    /// (2026-10-21 Task 7 의 2 차 리뷰가 `update_epoch` 에서 그리로 옮겼다 -- 앞 자리에서는
    /// `pending_frames == 0` 이라 ***직전*** 프레임을 찍었다). 즉 전진이 생산을 요구한다.
    /// 여기서 참을 돌려주면 "전진하려면 생산 -> 요청이 있으니 생산 정지 -> 전진 없음" 이
    /// 닫힌 고리가 된다. `ConstellationRequest` 도 같은 이유로 거짓이다 -- 그 상 자체는
    /// IPC 로 벗어나지만, 벗어난 다음 상이 생산을 필요로 한다.
    pub(crate) fn suspends_production(&self) -> bool {
        matches!(self, Self::WaitingOnFrame)
    }
}

/// 벽 틱이 스크린샷 기계에 묻는 두 가지. ★상(phase) 자체는 밖으로 내보내지 않는다.★
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ScreenshotWaitSummary {
    /// 상과 무관하게 요청이 하나라도 남아 있는가. ★판정이 아니라 폴링을 끌 신호다.★
    pub(crate) any_request: bool,
    /// `WaitingOnFrame` 에 닿은 요청이 있는가. ★이것만이 생산을 쉬게 한다.★
    pub(crate) waiting_on_frame: bool,
}

#[derive(Default)]
pub(crate) struct ScreenshotTaker {
    /// A vector of pending screenshots to be taken. These will be resolved once the
    /// pages have finished loading all content and the rendering reflects the finished
    /// state. See [`ScreenshotRequestPhase`] for more information.
    requests: RefCell<Vec<ScreenshotRequest>>,
}

impl ScreenshotTaker {
    /// 벽 틱이 매 주기 읽는 요약(`Paint::produce_wall_frame`).
    ///
    /// ★살아 있는 요청 목록에서 매번 다시 센다 -- 증분 계수기로 두지 않는다.★ 한 번
    /// 어긋난 계수기는 "생산 정지" 쪽으로 어긋났을 때 ***벽을 영구히 멈춘다***. 이쪽은
    /// 틀릴 수가 없고, 비용은 요청 벡터 한 번 훑기다(보통 길이 0).
    pub(crate) fn wait_summary(&self) -> ScreenshotWaitSummary {
        let requests = self.requests.borrow();
        ScreenshotWaitSummary {
            any_request: !requests.is_empty(),
            waiting_on_frame: requests
                .iter()
                .any(|request| request.phase.suspends_production()),
        }
    }

    pub(crate) fn request_screenshot(
        &self,
        webview_id: WebViewId,
        rect: Option<DeviceRect>,
        callback: Box<dyn FnOnce(Result<RgbaImage, ScreenshotCaptureError>) + 'static>,
    ) {
        self.requests.borrow_mut().push(ScreenshotRequest {
            webview_id,
            rect,
            callback,
            phase: ScreenshotRequestPhase::ConstellationRequest,
        });
    }

    pub(crate) fn handle_screenshot_readiness_reply(
        &self,
        webview_id: WebViewId,
        expected_epochs: FxHashMap<PipelineId, Epoch>,
        renderer: &Painter,
    ) {
        let expected_epochs = Rc::new(expected_epochs);

        for screenshot_request in self.requests.borrow_mut().iter_mut() {
            if screenshot_request.webview_id != webview_id ||
                screenshot_request.phase != ScreenshotRequestPhase::ConstellationRequest
            {
                continue;
            }
            screenshot_request.phase =
                ScreenshotRequestPhase::WaitingOnPipelineDisplayLists(expected_epochs.clone());
        }

        // Maybe when the message is received, the renderer already has the all of the necessary
        // display lists from the Pipelines. In that case, the renderer should move immediately
        // to the next phase of the screenshot state machine.
        self.prepare_screenshot_requests_for_render(renderer);
    }

    pub(crate) fn prepare_screenshot_requests_for_render(&self, renderer: &Painter) {
        let mut any_became_ready = false;

        for screenshot_request in self.requests.borrow_mut().iter_mut() {
            let ScreenshotRequestPhase::WaitingOnPipelineDisplayLists(pipelines) =
                &screenshot_request.phase
            else {
                continue;
            };

            let Some(webview) = renderer.webview_renderer(screenshot_request.webview_id) else {
                continue;
            };

            if pipelines.iter().all(|(pipeline_id, expected_epoch)| {
                webview
                    .pipelines
                    .get(pipeline_id)
                    .and_then(|pipeline| pipeline.display_list_epoch)
                    .is_some_and(|epoch| epoch >= *expected_epoch)
            }) {
                screenshot_request.phase = ScreenshotRequestPhase::WaitingOnFrame;
                any_became_ready = true;
            }
        }

        // If there are now screenshots waiting on a frame, and there are no pending frames,
        // immediately trigger a repaint so that screenshots can be taken when the repaint
        // is done.
        if any_became_ready {
            self.maybe_trigger_paint_for_screenshot(renderer);
        }
    }

    pub(crate) fn maybe_trigger_paint_for_screenshot(&self, renderer: &Painter) {
        if renderer.has_pending_frames() {
            return;
        }

        if self.requests.borrow().iter().any(|screenshot_request| {
            matches!(
                screenshot_request.phase,
                ScreenshotRequestPhase::WaitingOnFrame
            )
        }) {
            renderer.set_needs_repaint(RepaintReason::ReadyForScreenshot);
        }
    }

    pub(crate) fn maybe_take_screenshots(&self, renderer: &Painter) {
        if renderer.has_pending_frames() {
            return;
        }

        let mut requests = self.requests.borrow_mut();
        if requests.is_empty() {
            return;
        }

        // TODO: This can eventually just be `extract_if`. We need to have ownership
        // of the ScreenshotRequest in order to call the `FnOnce` callabck.
        let screenshots = requests.drain(..);
        *requests = screenshots
            .filter_map(|screenshot_request| {
                if !matches!(
                    screenshot_request.phase,
                    ScreenshotRequestPhase::WaitingOnFrame
                ) {
                    return Some(screenshot_request);
                }

                let callback = screenshot_request.callback;
                let Some(webview_renderer) =
                    renderer.webview_renderer(screenshot_request.webview_id)
                else {
                    callback(Err(ScreenshotCaptureError::WebViewDoesNotExist));
                    return None;
                };

                let viewport_rect = webview_renderer.rect.to_i32();
                let viewport_size = viewport_rect.size();
                let rect = screenshot_request.rect.map_or(viewport_rect, |rect| {
                    // We need to convert to the bottom-left origin coordinate
                    // system used by OpenGL
                    // If dpi > 1, y can be computed to be -1 due to rounding issue, resulting in panic.
                    // https://github.com/servo/servo/issues/39306#issuecomment-3342204869
                    let x = rect.min.x as i32;
                    let y = 0.max(
                        (viewport_size.height as f32 - rect.min.y - rect.size().height) as i32,
                    );
                    let w = (rect.size().width as i32).min(viewport_size.width - x);
                    let h = (rect.size().height as i32).min(viewport_size.height - y);

                    DeviceIntRect::from_origin_and_size(Point2D::new(x, y), Size2D::new(w, h))
                });
                if let Err(error) = renderer.rendering_context.make_current() {
                    error!("Failed to make the rendering context current: {error:?}");
                }
                let result = renderer
                    .rendering_context
                    .read_to_image(rect)
                    .ok_or(ScreenshotCaptureError::CouldNotReadImage);
                callback(result);
                None
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use rustc_hash::FxHashMap;

    use super::ScreenshotRequestPhase;
    use crate::wall_tick::tick_should_produce;

    /// ★생산을 쉬게 하는 상은 `WaitingOnFrame` 하나뿐이다.★ 거기서 나가는 길
    /// (`maybe_take_screenshots`)은 `pending_frames` 가 0 이 되기만 기다리고, 그 숫자는
    /// WebRender 의 frame-ready 가 뺀다 -- ★새 생산을 요구하지 않는다.★
    #[test]
    fn only_a_request_waiting_on_a_frame_suspends_production() {
        let phase = ScreenshotRequestPhase::WaitingOnFrame;
        let suspends = phase.suspends_production();
        assert!(
            suspends,
            "`WaitingOnFrame` 은 생산을 쉬게 해야 한다 (실제 {suspends})"
        );
        let should_produce = tick_should_produce(suspends);
        assert!(
            !should_produce,
            "그 상에서는 틱이 쉬어야 한다 (실제 {should_produce})"
        );
    }

    /// ★이 시험이 막는 것은 영구 정지다 -- 기능이 아니라 교착이다.★
    ///
    /// `WaitingOnPipelineDisplayLists` 에서 다음 상으로 미는 자리는
    /// `prepare_screenshot_requests_for_render` 이고, 그 주 호출자는
    /// `Painter::generate_frame_for_wall_tick` 의 `Generated` 갈래다(2026-10-21 Task 7 의
    /// 2 차 리뷰가 `update_epoch` 에서 그리로 옮겼다 -- 앞 자리에서는 ***직전*** 프레임을
    /// 찍었다). 즉 ★이 상을 떠나려면 생산이 필요하다.★ 그래서 여기서 참을 돌려주면
    ///
    ///   요청 도착 -> 전진하려면 생산 -> 요청이 있으니 생산 정지 -> 전진 없음 -> 영원히
    ///
    /// 이 된다. ★생산을 쉬는 상은 떠나는 데 생산이 필요 없는 상 하나뿐이어야 한다.★
    #[test]
    fn a_request_still_waiting_on_display_lists_must_not_suspend_production() {
        let phase =
            ScreenshotRequestPhase::WaitingOnPipelineDisplayLists(Rc::new(FxHashMap::default()));
        let suspends = phase.suspends_production();
        assert!(
            !suspends,
            "`WaitingOnPipelineDisplayLists` 는 생산으로만 벗어나므로 쉬게 하면 안 된다 \
             (실제 {suspends})"
        );
        let should_produce = tick_should_produce(suspends);
        assert!(
            should_produce,
            "전진시킬 생산이 돌아야 한다 (실제 {should_produce})"
        );
    }

    /// Constellation 의 답신을 기다리는 상도 마찬가지다. 여기서 벗어나는 길은 IPC 라
    /// 생산과 무관하지만, 그 다음 상이 생산을 필요로 하므로 여기서 멈추면 결국 같은
    /// 자리에서 잠긴다.
    #[test]
    fn a_request_still_at_the_constellation_must_not_suspend_production() {
        let phase = ScreenshotRequestPhase::ConstellationRequest;
        let suspends = phase.suspends_production();
        assert!(
            !suspends,
            "`ConstellationRequest` 는 생산을 쉬게 하면 안 된다 (실제 {suspends})"
        );
    }
}

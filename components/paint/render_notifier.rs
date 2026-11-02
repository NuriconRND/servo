/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use paint_api::{PaintMessage, PaintProxy};
use servo_base::id::PainterId;
use webrender_api::{DocumentId, FramePublishId, FrameReadyParams};

/// 이 페인터가 ***발행된 것을 본*** 가장 큰 `FramePublishId`.
///
/// ★`Painter` 가 `Renderer::update()` 의 드레인을 한 프레임으로 묶으려면 "지금 큐에 무엇이
/// 들어 있는가" 를 알아야 한다★ -- 그 유일한 통로가 이 알림이다. 알림은 백엔드 스레드에서
/// 오고(`render_backend.rs:1748`/`2137`) 읽는 쪽은 페인터 스레드이므로 원자 하나로 건넨다.
///
/// ★`result_tx.send` 가 알림보다 먼저다★(`render_backend.rs:1706` 대 `1748`). 그래서 여기
/// 값이 `k` 면 id `k` 의 메시지는 ***이미 채널에 있다*** -- 페인터가 그것을 목표로 삼으면
/// 그 패스에서 반드시 소비된다.
pub(crate) type PublishedFrameHighWaterMark = Arc<AtomicU64>;

#[derive(Clone)]
pub(crate) struct RenderNotifier {
    painter_id: PainterId,
    paint_proxy: PaintProxy,
    published_hwm: PublishedFrameHighWaterMark,
}

impl RenderNotifier {
    pub(crate) fn new(
        painter_id: PainterId,
        paint_proxy: PaintProxy,
        published_hwm: PublishedFrameHighWaterMark,
    ) -> RenderNotifier {
        RenderNotifier {
            painter_id,
            paint_proxy,
            published_hwm,
        }
    }
}

impl webrender_api::RenderNotifier for RenderNotifier {
    fn clone(&self) -> Box<dyn webrender_api::RenderNotifier> {
        Box::new(RenderNotifier::new(
            self.painter_id,
            self.paint_proxy.clone(),
            self.published_hwm.clone(),
        ))
    }

    fn wake_up(&self, _composite_needed: bool) {}

    fn new_frame_ready(
        &self,
        document_id: DocumentId,
        publish_id: FramePublishId,
        frame_ready_params: &FrameReadyParams,
    ) {
        // ★id 는 빈틈 없이 +1 이고 중복이 없다★ -- 살아 있는 발행 자리는
        // `render_backend.rs:1706`/`2118` 둘뿐이고 둘 다 `advance()` 뒤에 보낸다. 세 번째
        // 자리(`:1031`, `process_offscreen_scene`)는 `advance` 없이 같은 id 로 보내지만
        // `Transaction::render_offscreen` 이 이 리포에서 호출되지 않아 죽어 있다.
        // 그래서 `fetch_max` 로 단조성만 지켜 주면 된다.
        self.published_hwm
            .fetch_max(publish_id.0, Ordering::Relaxed);
        self.paint_proxy.send(PaintMessage::NewWebRenderFrameReady(
            self.painter_id,
            document_id,
            frame_ready_params.render,
        ));
    }
}

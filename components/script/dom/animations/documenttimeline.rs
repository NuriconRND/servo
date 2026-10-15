/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::gc::HandleObject;
use num_traits::ToPrimitive;
use script_bindings::codegen::GenericBindings::DocumentTimelineBinding::DocumentTimelineOptions;
use script_bindings::reflector::{reflect_dom_object, reflect_dom_object_with_proto};
use script_bindings::root::DomRoot;
use script_bindings::script_runtime::CanGc;
use servo_base::cross_process_instant::CrossProcessInstant;
use servo_config::pref;
use time::Duration;

use crate::dom::bindings::codegen::Bindings::DocumentTimelineBinding::DocumentTimelineMethods;
use crate::dom::types::{AnimationTimeline, Window};

/// <https://drafts.csswg.org/web-animations-1/#the-documenttimeline-interface>
#[dom_struct]
pub(crate) struct DocumentTimeline {
    animation_timeline: AnimationTimeline,
    /// An offset from the `Document`'s time origin as a [`Duration`] offset. This is determined by the original
    /// "originTime" specified during construction of the [`AnimationTimeline`] in the options object.
    /// Note that this value might be negative.
    ///
    /// See:
    ///   - <https://drafts.csswg.org/web-animations-1/#dom-documenttimelineoptions-origintime>
    ///   - <https://html.spec.whatwg.org/multipage/#concept-settings-object-time-origin>
    #[no_trace]
    origin_offset: Duration,
}

impl DocumentTimeline {
    pub(crate) fn new_with_duration(
        window: &Window,
        proto: Option<HandleObject>,
        origin_time: Duration,
        can_gc: CanGc,
    ) -> DomRoot<Self> {
        let duration_since_time_origin =
            CrossProcessInstant::now() - window.navigation_start() - origin_time;
        reflect_dom_object_with_proto(
            Box::new(Self {
                animation_timeline: AnimationTimeline::new_inherited(duration_since_time_origin),
                origin_offset: origin_time,
            }),
            window,
            proto,
            can_gc,
        )
    }

    pub(crate) fn new(window: &Window, can_gc: CanGc) -> DomRoot<DocumentTimeline> {
        let duration = if pref!(layout_animations_test_enabled) {
            Duration::ZERO
        } else {
            CrossProcessInstant::now() - window.navigation_start()
        };
        reflect_dom_object(
            Box::new(Self {
                animation_timeline: AnimationTimeline::new_inherited(duration),
                origin_offset: Duration::ZERO,
            }),
            window,
            can_gc,
        )
    }

    /// Updates the value of the `AnimationTimeline` to the current clock time.
    ///
    /// ★`frame_time` 이 있고 pref 가 켜져 있으면 그것을 쓴다.★ 이 값이 애니메이션 전체의
    /// 기준 시각이고, 여기 실린 지터가 그대로 화면의 떨림이 된다 -- 그 인과는 설계 문서
    /// 50 이 측정으로 닫았다. `None` 은 틱이 아닌 이유(scroll, resize, input, …)로 도는
    /// 회차이고, 그때는 지금 시각이 맞다.
    pub(crate) fn update(&self, window: &Window, frame_time: Option<CrossProcessInstant>) {
        let now = match frame_time {
            Some(frame_time) if pref!(dom_animation_timeline_use_frame_time) => frame_time,
            _ => CrossProcessInstant::now(),
        };
        let duration_since_time_origin = now - window.navigation_start() - self.origin_offset;
        self.animation_timeline
            .set_current_time(duration_since_time_origin);
    }

    /// Increments the current value of the timeline by a specific number of seconds.
    /// This is used for testing.
    pub(crate) fn advance_specific(&self, by: Duration) {
        self.animation_timeline.advance_specific(by);
    }
}

impl DocumentTimelineMethods<crate::DomTypeHolder> for DocumentTimeline {
    fn Constructor(
        window: &Window,
        proto: Option<HandleObject>,
        can_gc: CanGc,
        options: &DocumentTimelineOptions,
    ) -> DomRoot<Self> {
        Self::new_with_duration(
            window,
            proto,
            Duration::seconds_f64(options.originTime.to_f64().unwrap_or_default() / 1000.),
            can_gc,
        )
    }
}

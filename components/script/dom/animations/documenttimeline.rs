/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::gc::HandleObject;
use num_traits::ToPrimitive;
use script_bindings::codegen::GenericBindings::DocumentTimelineBinding::DocumentTimelineOptions;
use script_bindings::reflector::{reflect_dom_object, reflect_dom_object_with_proto};
use script_bindings::root::DomRoot;
use script_bindings::script_runtime::CanGc;
use servo_base::animation_timeline_smoothing::{TimelineSmoothingState, smooth_animation_timeline};
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
    /// 평활기의 상태. `dom_animation_timeline_smoothing_hz` 가 꺼져 있으면 쓰이지 않는다.
    /// ★간격 추정이 이 안에 있다★ -- 매번 처음부터 다시 추정하면 평활이 의미가 없다.
    ///
    /// `#[no_trace]` 가 필요하다. 이 타입은 `JSTraceable` 이 아니고 JS 값도 담지 않는다 --
    /// 붙이지 않으면 파생 매크로가 `trace` 를 찾다가 실패한다. 반대로 `f64` 나
    /// `Cell<f64>` 처럼 ***이미*** `JSTraceable` 인 타입에 붙이면 E0283 이 나므로, 이
    /// 속성은 타입을 보고 붙이는 것이지 관례가 아니다.
    #[no_trace]
    smoothing_state: Cell<Option<TimelineSmoothingState>>,
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
                smoothing_state: Cell::new(None),
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
                smoothing_state: Cell::new(None),
            }),
            window,
            can_gc,
        )
    }

    /// Updates the value of the `AnimationTimeline` to the current clock time.
    ///
    /// ★`dom_animation_timeline_snap_hz` 가 켜져 있으면 격자에 맞춘다.★ 이 값이 애니메이션
    /// 전체의 기준 시각이고, 여기 실린 지터가 그대로 화면의 떨림이 된다 -- 그 인과는
    /// 설계 문서 50 이 측정으로 닫았다. 꺼져 있으면(기본) 아래 `snap_animation_timeline` 이
    /// `raw` 를 그대로 돌려주므로 ***지금 동작과 같다.***
    pub(crate) fn update(&self, window: &Window) {
        let raw = CrossProcessInstant::now() - window.navigation_start() - self.origin_offset;
        let Some(nominal_period) = Self::smoothing_period_seconds() else {
            // ★꺼져 있다. 원래 경로 그대로다*** -- f64 왕복조차 하지 않는다. 이 한 줄이
            // 이 기능의 위험을 되돌릴 수 있는 유일한 수단이라 정확히 예전 코드여야 한다.
            self.animation_timeline.set_current_time(raw);
            return;
        };
        let (smoothed, state) = smooth_animation_timeline(
            self.smoothing_state.get(),
            raw.as_seconds_f64(),
            nominal_period,
        );
        self.smoothing_state.set(Some(state));
        self.animation_timeline
            .set_current_time(Duration::seconds_f64(smoothed));
    }

    /// `dom_animation_timeline_smoothing_hz` 를 주기(초)로 바꾼다. 꺼져 있으면 `None`.
    ///
    /// ★범위를 벗어난 값은 조이고 한 번 경고한다.★ 조용히 꺼 버리면 `-TimelineSmoothing
    /// 1200` 을 오타로 넣은 실행이 "켰는데 효과 없음" 으로 기록된다 -- 요청의 정반대다.
    fn smoothing_period_seconds() -> Option<f64> {
        const MAX_HZ: i64 = 1000;
        let hz = pref!(dom_animation_timeline_smoothing_hz);
        if hz <= 0 {
            return None;
        }
        static ENABLED: std::sync::Once = std::sync::Once::new();
        ENABLED.call_once(|| {
            // ★켜졌다는 증거를 로그에 한 줄 남긴다.★ 이게 없으면 오타로 꺼진 실행을
            // "켰는데 효과 없음" 으로 기록하게 된다 -- 벽 A/B 는 한 번에 몇 분씩 든다.
            info!("animation timeline smoothing enabled at {hz}Hz");
        });
        if hz > MAX_HZ {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                warn!("dom_animation_timeline_smoothing_hz={hz} is out of range; using {MAX_HZ}");
            });
            return Some(1.0 / MAX_HZ as f64);
        }
        Some(1.0 / hz as f64)
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

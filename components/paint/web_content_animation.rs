/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use embedder_traits::EventLoopWaker;
use rustc_hash::FxHashMap;
use servo_base::id::WebViewId;
use servo_config::prefs;
use paint_api::display_list::{PaintAnimation, PaintAnimationProperty, PaintAnimationSegment};
use webrender_api::units::LayoutTransform;
use webrender_api::{ColorF, PropertyBindingKey, PropertyValue};

use crate::refresh_driver::TimerRefreshDriver;
use crate::webview_renderer::WebViewRenderer;

/// The amount of the time the caret blinks before ceasing, in order to preserve power. User
/// activity (a new display list) will reset this.
///
/// TODO: This should be controlled by system settings.
pub(crate) const CARET_BLINK_TIMEOUT: Duration = Duration::from_secs(30);

/// A struct responsible for managing paint-side animations. Currently this only handles text caret
/// blinking, but the idea is that in the future this would handle other types of paint-side
/// animations as well.
///
/// Note: This does not control animations requiring layout (all CSS transitions and animations
/// currently) nor animations due to touch events such as fling.
pub(crate) struct WebContentAnimator {
    event_loop_waker: Box<dyn EventLoopWaker>,
    timer_refresh_driver: Rc<TimerRefreshDriver>,
    caret_visible: Cell<bool>,
    timer_scheduled: Cell<bool>,
    need_update: Arc<AtomicBool>,
    /// Set while a wake for the next paint-animation frame is already queued, so a long
    /// animation queues one timer at a time instead of one per turn.
    paint_animation_wake_scheduled: Arc<AtomicBool>,
}

impl WebContentAnimator {
    pub(crate) fn new(
        event_loop_waker: Box<dyn EventLoopWaker>,
        timer_refresh_driver: Rc<TimerRefreshDriver>,
    ) -> Self {
        Self {
            event_loop_waker,
            timer_refresh_driver,
            caret_visible: Cell::new(true),
            timer_scheduled: Default::default(),
            need_update: Default::default(),
            paint_animation_wake_scheduled: Default::default(),
        }
    }

    /// Ask for another turn of the paint loop soon, so an animation nobody else is
    /// driving still advances.
    ///
    /// The wall usually has video pushing frames anyway, but a page whose only moving
    /// thing is one CSS animation has nothing else to wake the painter -- and the whole
    /// point of these animations is that they keep running when script has stopped
    /// feeding us.
    pub(crate) fn wake_for_paint_animation(&self, period: Duration) {
        if self.paint_animation_wake_scheduled.load(Ordering::Relaxed) {
            return;
        }
        let event_loop_waker = self.event_loop_waker.clone();
        let scheduled = self.paint_animation_wake_scheduled.clone();
        self.timer_refresh_driver.queue_timer(
            period,
            Box::new(move || {
                scheduled.store(false, Ordering::Relaxed);
                event_loop_waker.wake();
            }),
        );
        self.paint_animation_wake_scheduled
            .store(true, Ordering::Relaxed);
    }

    pub(crate) fn schedule_timer_if_necessary(&self) {
        if self.timer_scheduled.get() {
            return;
        }

        let Some(caret_blink_time) = prefs::get().editing_caret_blink_time() else {
            return;
        };

        let event_loop_waker = self.event_loop_waker.clone();
        let need_update = self.need_update.clone();
        self.timer_refresh_driver.queue_timer(
            caret_blink_time,
            Box::new(move || {
                need_update.store(true, Ordering::Relaxed);
                event_loop_waker.wake();
            }),
        );
        self.timer_scheduled.set(true);
    }

    pub(crate) fn update(
        &self,
        webview_renderers: &FxHashMap<WebViewId, WebViewRenderer>,
    ) -> Option<Vec<PropertyValue<ColorF>>> {
        if !self.need_update.load(Ordering::Relaxed) {
            return None;
        }

        let mut colors = Vec::new();
        for renderer in webview_renderers.values() {
            renderer.for_each_connected_pipeline(&mut |pipeline_details| {
                if let Some(property_value) =
                    pipeline_details.animations.update(self.caret_visible.get())
                {
                    colors.push(property_value);
                }
            });
        }

        self.timer_scheduled.set(false);
        self.need_update.store(false, Ordering::Relaxed);

        if colors.is_empty() {
            // All animations have stopped. When a new blinking caret is activated we want
            // it to start in the visible state, so we set `caret_visible` to true here.
            self.caret_visible.set(true);
            return None;
        }

        self.caret_visible.set(!self.caret_visible.get());
        self.schedule_timer_if_necessary();
        Some(colors)
    }
}

/// This structure tracks the animations active for a given pipeline. Currently only caret
/// blinking is tracked, but in the future this could perhaps track paint-side animations.
#[derive(Default)]
pub(crate) struct PipelineAnimations {
    caret: RefCell<Option<CaretAnimation>>,
    /// What layout last told us to keep playing, and the clock it is played against.
    /// Replaced wholesale by every display list -- see [`PaintAnimation`].
    paint: RefCell<Vec<ActivePaintAnimation>>,
    /// The last value of every paint animation that has stopped playing, kept so that a
    /// later transaction does not erase it. See [`HeldPaintValues`].
    held: RefCell<HeldPaintValues>,
}

/// ***전송 한 번이 안 실린 키를 전부 지운다.***
///
/// `Transaction::reset_dynamic_properties` 는 WebRender 의 `SceneProperties` 에서
/// transform/float/color 맵 셋을 통째로 비우고 **그 트랜잭션에 실린 것만** 다시 채운다
/// (`webrender/src/scene.rs`, `flush_pending_updates`). 그리고 맵에 없는 키는
/// `resolve_float`/`resolve_layout_transform` 이 바인딩에 구워진 기본값으로 되돌린다 --
/// 그 기본값은 **디스플레이 리스트를 만든 순간의 값**이다.
///
/// 그래서 먼저 끝난 애니메이션의 키는, 아직 도는 다른 애니메이션이 값을 밀 때마다 화면에서
/// 시작값으로 되돌아간다. 페이드인이면 투명 -- 즉 검은 화면이다. 되돌아간 값은 다음 디스플레이
/// 리스트가 올 때까지 그대로 있으므로, 끊긴 길이는 프레임 한 장이 아니라 **초 단위**가 된다.
///
/// ***고칠 자리는 "더 자주 보내는 쪽"이 아니라 "지우는 쪽"이다.*** 끝난 값을 매 주기 다시
/// 밀면 페인터마다 초당 한 주기치 프레임이 더 나가고(`ActivePaintAnimation::running` 주석의
/// 실측), 그건 비디오 표출을 다시 밀어내는 길이다. 대신 마지막 값을 여기 붙들어 두었다가
/// **이미 나가기로 정해진 전송에만 얹는다** -- 추가 전송도, 추가 프레임도 없다.
#[derive(Default)]
struct HeldPaintValues {
    floats: Vec<PropertyValue<f32>>,
    transforms: Vec<PropertyValue<LayoutTransform>>,
}

/// 한 파이프라인이 붙들 수 있는 값의 상한. 설계의 일부가 아니라 누수 방지턱이다: 키는
/// 디스플레이 리스트가 다시 선언해 줄 때 비로소 정리되므로(`prune_held`), 다시는 돌아오지
/// 않는 요소의 키가 쌓일 수 있다. 애니메이션되는 요소가 수십 개인 화면에서는 닿지 않는다.
const MAX_HELD_PAINT_VALUES: usize = 512;

impl HeldPaintValues {
    fn hold_float(&mut self, value: PropertyValue<f32>) {
        match self
            .floats
            .iter_mut()
            .find(|held| held.key.id == value.key.id)
        {
            Some(slot) => *slot = value,
            None => {
                if self.floats.len() >= MAX_HELD_PAINT_VALUES {
                    self.floats.remove(0);
                }
                self.floats.push(value);
            },
        }
    }

    fn hold_transform(&mut self, value: PropertyValue<LayoutTransform>) {
        match self
            .transforms
            .iter_mut()
            .find(|held| held.key.id == value.key.id)
        {
            Some(slot) => *slot = value,
            None => {
                if self.transforms.len() >= MAX_HELD_PAINT_VALUES {
                    self.transforms.remove(0);
                }
                self.transforms.push(value);
            },
        }
    }

    fn is_empty(&self) -> bool {
        self.floats.is_empty() && self.transforms.is_empty()
    }
}

/// A [`PaintAnimation`] anchored to this painter's clock.
struct ActivePaintAnimation {
    property: PaintAnimationProperty,
    /// The instant the animation's own timeline reads zero. Usually in the past.
    zero: Instant,
    /// Whether the segments run to the animation's end, as opposed to stopping at the
    /// sampling horizon. Not used to decide whether to keep playing -- see `running` --
    /// but it is the difference between "finished" and "outran its runway", which is worth
    /// keeping straight when reading this back.
    complete: bool,
}

/// Interpolate one segment. `Copy + Lerp` would be nicer than a trait per type, but two
/// concrete types do not earn a trait.
fn segment_progress<T>(segment: &PaintAnimationSegment<T>, elapsed: f64) -> f64 {
    let span = segment.end - segment.start;
    let raw = if span > 0.0 {
        (elapsed - segment.start) / span
    } else {
        1.0
    };
    segment.easing.ease(raw)
}

/// Pick the segment covering `elapsed`, clamping to the first or last one outside the
/// range. Segments are few (one for a transition, a handful for keyframes), so a scan
/// beats anything cleverer.
fn find_segment<T>(
    segments: &[PaintAnimationSegment<T>],
    elapsed: f64,
) -> Option<&PaintAnimationSegment<T>> {
    if segments.is_empty() {
        return None;
    }
    if elapsed <= segments[0].start {
        return segments.first();
    }
    segments
        .iter()
        .find(|segment| elapsed < segment.end)
        .or_else(|| segments.last())
}

impl ActivePaintAnimation {
    /// Whether this animation still has anything left to say at `now`.
    ///
    /// ***Past the end of what was sampled there is nothing left to say, finished or
    /// not.*** An animation that outran its horizon is not over -- the rest of it lives in
    /// script -- but the value cannot move again until the next display list arrives, and
    /// WebRender keeps the last value it was given. Re-sending it every frame would burn
    /// a frame per painter per tick to redraw an unchanged picture; measured on the 4-GPU
    /// wall, 2026-09-08, that was 30 frames a second per painter of pure waste.
    ///
    /// It also makes `playing=` in the log mean "actually moving", which is the question
    /// being asked of it.
    fn running(&self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.zero).as_secs_f64();
        match &self.property {
            PaintAnimationProperty::Opacity(_, segments) => {
                segments.last().is_some_and(|last| elapsed < last.end)
            },
            PaintAnimationProperty::Transform(_, segments) => {
                segments.last().is_some_and(|last| elapsed < last.end)
            },
        }
    }

    /// The instant this animation's own timeline runs out, if it has segments at all.
    fn ends_at(&self) -> Option<Instant> {
        let end = match &self.property {
            PaintAnimationProperty::Opacity(_, segments) => segments.last()?.end,
            PaintAnimationProperty::Transform(_, segments) => segments.last()?.end,
        };
        if !end.is_finite() || end < 0.0 {
            return None;
        }
        self.zero.checked_add(Duration::from_secs_f64(end))
    }

    /// Sample the value this animation stops at.
    ///
    /// Sampled at exactly `ends_at` rather than at `now`: past the end `segment_progress`
    /// would hand the easing a ratio above one, and an easing curve is only defined on
    /// [0, 1] -- a cubic-bezier extrapolates, so "the value it finished on" would come out
    /// as some value it never had.
    fn sample_final(&self, held: &mut HeldPaintValues) {
        let Some(end) = self.ends_at() else {
            return;
        };
        let mut floats = Vec::new();
        let mut transforms = Vec::new();
        self.sample(end, &mut floats, &mut transforms);
        for value in floats {
            held.hold_float(value);
        }
        for value in transforms {
            held.hold_transform(value);
        }
    }

    fn sample(
        &self,
        now: Instant,
        floats: &mut Vec<PropertyValue<f32>>,
        transforms: &mut Vec<PropertyValue<LayoutTransform>>,
    ) {
        let elapsed = now.saturating_duration_since(self.zero).as_secs_f64();
        match &self.property {
            PaintAnimationProperty::Opacity(key, segments) => {
                let Some(segment) = find_segment(segments, elapsed) else {
                    return;
                };
                let progress = segment_progress(segment, elapsed) as f32;
                floats.push(PropertyValue {
                    key: *key,
                    value: segment.from + (segment.to - segment.from) * progress,
                });
            },
            PaintAnimationProperty::Transform(key, segments) => {
                let Some(segment) = find_segment(segments, elapsed) else {
                    return;
                };
                let progress = segment_progress(segment, elapsed) as f32;
                // Componentwise on the matrix. Layout subdivided anything whose CSS
                // interpolation is not linear in the matrix, so within one segment this
                // agrees with the styled value to well under a pixel.
                let from = segment.from.to_array();
                let to = segment.to.to_array();
                let mut lerped = [0.0f32; 16];
                for (index, slot) in lerped.iter_mut().enumerate() {
                    *slot = from[index] + (to[index] - from[index]) * progress;
                }
                transforms.push(PropertyValue {
                    key: *key,
                    value: LayoutTransform::from_array(lerped),
                });
            },
        }
    }
}

impl PipelineAnimations {
    pub(crate) fn update(&self, caret_visible: bool) -> Option<PropertyValue<ColorF>> {
        let mut maybe_caret = self.caret.borrow_mut();
        let caret = maybe_caret.as_mut()?;

        if let Some(update) = caret.update(caret_visible) {
            return Some(update);
        }
        *maybe_caret = None;
        None
    }

    /// Sample every animation this pipeline is playing, and say whether any is still
    /// going. Values are appended, never replaced -- the caller collects across pipelines
    /// and sends one transaction.
    pub(crate) fn update_paint_animations(
        &self,
        now: Instant,
        floats: &mut Vec<PropertyValue<f32>>,
        transforms: &mut Vec<PropertyValue<LayoutTransform>>,
    ) -> bool {
        let mut animations = self.paint.borrow_mut();
        let mut held = self.held.borrow_mut();
        // A finished animation stops being sampled, but its last value is held rather than
        // forgotten -- see [`HeldPaintValues`] for why forgetting it blanks the element.
        animations.retain(|animation| {
            if animation.running(now) {
                return true;
            }
            animation.sample_final(&mut held);
            false
        });
        for animation in animations.iter() {
            animation.sample(now, floats, transforms);
        }
        !animations.is_empty()
    }

    /// Append the values of animations that have stopped, for keys nothing is currently
    /// animating, and say how many were appended.
    ///
    /// ***이 함수는 전송을 만들지 않는다.*** 이미 나가기로 정해진 트랜잭션에만 얹히도록
    /// 호출되며(`Painter::perform_updates`), 그래서 프레임 생산 판정 -- `has_values`,
    /// `push_due`, `animated_property_frame`, `still_animating` -- 어느 것에도 들어가지
    /// 않는다. 전송이 없으면 reset 도 없고, reset 이 없으면 붙들 이유도 없다.
    ///
    /// 지금 도는 애니메이션이 먼저다: 같은 키를 둘 다 갖고 있으면 살아 있는 쪽이 옳다.
    pub(crate) fn append_held_paint_values(
        &self,
        floats: &mut Vec<PropertyValue<f32>>,
        transforms: &mut Vec<PropertyValue<LayoutTransform>>,
    ) -> usize {
        let held = self.held.borrow();
        if held.is_empty() {
            return 0;
        }
        let mut appended = 0;
        for value in held.floats.iter() {
            if !floats.iter().any(|live| live.key.id == value.key.id) {
                floats.push(*value);
                appended += 1;
            }
        }
        for value in held.transforms.iter() {
            if !transforms.iter().any(|live| live.key.id == value.key.id) {
                transforms.push(*value);
                appended += 1;
            }
        }
        appended
    }

    /// Drop held values for keys the new display list drives again.
    ///
    /// Only those: a key the new list does not mention may still be referenced by the
    /// scene WebRender is rendering right now -- the new one is not built yet -- and
    /// dropping it there is the blank this whole mechanism exists to prevent. Keys that
    /// really are gone age out against [`MAX_HELD_PAINT_VALUES`].
    fn prune_held(&self, incoming: &[PaintAnimation]) {
        let mut held = self.held.borrow_mut();
        if held.is_empty() {
            return;
        }
        for animation in incoming {
            match &animation.property {
                PaintAnimationProperty::Opacity(key, _) => {
                    held.floats.retain(|value| value.key.id != key.id)
                },
                PaintAnimationProperty::Transform(key, _) => {
                    held.transforms.retain(|value| value.key.id != key.id)
                },
            }
        }
    }

    /// Whether this pipeline has anything the paint thread is playing on its own.
    ///
    /// Used to decide how often script still needs to be asked to tick the animation: see
    /// [`AnimationRefreshDriverObserver::frame_started`].
    pub(crate) fn has_paint_animations(&self) -> bool {
        !self.paint.borrow().is_empty()
    }

    /// Replace what this pipeline is playing with what the new display list says.
    ///
    /// ***Wholesale replacement is the correctness argument.*** Layout samples forward
    /// from the moment it built the list, so as long as script keeps up, every frame's
    /// prediction is overwritten by the truth before it can drift. Only when script stops
    /// producing display lists does the prediction actually get used for long, and that is
    /// exactly when it is worth having.
    pub(crate) fn install_paint_animations(&self, paint_animations: Vec<PaintAnimation>) {
        let received_at = Instant::now();
        self.prune_held(&paint_animations);
        *self.paint.borrow_mut() = paint_animations
            .into_iter()
            .map(|animation| {
                let offset = Duration::from_secs_f64(animation.offset_from_display_list.abs());
                let zero = if animation.offset_from_display_list <= 0.0 {
                    received_at.checked_sub(offset).unwrap_or(received_at)
                } else {
                    received_at + offset
                };
                ActivePaintAnimation {
                    property: animation.property,
                    zero,
                    complete: animation.complete,
                }
            })
            .collect();
    }

    pub(crate) fn handle_new_display_list(
        &self,
        caret_property_binding: Option<(PropertyBindingKey<ColorF>, ColorF)>,
        web_content_animator: &WebContentAnimator,
    ) {
        let Some(caret_blink_time) = prefs::get().editing_caret_blink_time() else {
            return;
        };

        *self.caret.borrow_mut() = match caret_property_binding {
            Some((caret_property_key, original_caret_color)) => {
                web_content_animator.schedule_timer_if_necessary();
                Some(CaretAnimation {
                    caret_property_key,
                    original_caret_color,
                    remaining_blink_count: (CARET_BLINK_TIMEOUT.as_millis() /
                        caret_blink_time.as_millis())
                        as usize,
                })
            },
            None => None,
        }
    }
}

/// Tracks the state of an ongoing caret blinking animation.
struct CaretAnimation {
    pub caret_property_key: PropertyBindingKey<ColorF>,
    pub original_caret_color: ColorF,
    pub remaining_blink_count: usize,
}

impl CaretAnimation {
    pub(crate) fn update(&mut self, caret_visible: bool) -> Option<PropertyValue<ColorF>> {
        if self.remaining_blink_count == 0 {
            return None;
        }

        self.remaining_blink_count = self.remaining_blink_count.saturating_sub(1);
        let value = if caret_visible || self.remaining_blink_count == 0 {
            self.original_caret_color
        } else {
            ColorF::TRANSPARENT
        };

        Some(PropertyValue {
            key: self.caret_property_key,
            value,
        })
    }
}

#[cfg(test)]
mod tests {
    use paint_api::display_list::PaintAnimationEasing;

    use super::*;

    fn opacity_key(id: u64) -> PropertyBindingKey<f32> {
        PropertyBindingKey::new(id)
    }

    /// A fade-in that ran `duration` seconds ago and is already over.
    fn finished_fade(id: u64, duration: f64) -> PaintAnimation {
        PaintAnimation {
            property: PaintAnimationProperty::Opacity(
                opacity_key(id),
                vec![PaintAnimationSegment {
                    start: 0.0,
                    end: duration,
                    from: 0.0,
                    to: 1.0,
                    easing: PaintAnimationEasing::Linear,
                }],
            ),
            // Its zero point is far enough in the past that it has already ended.
            offset_from_display_list: -(duration * 2.0),
            complete: true,
        }
    }

    /// A fade-in that is only starting now, so it is still playing.
    fn running_fade(id: u64, duration: f64) -> PaintAnimation {
        PaintAnimation {
            offset_from_display_list: 0.0,
            ..finished_fade(id, duration)
        }
    }

    fn sample(animations: &PipelineAnimations) -> (Vec<PropertyValue<f32>>, bool) {
        let mut floats = Vec::new();
        let mut transforms = Vec::new();
        let playing =
            animations.update_paint_animations(Instant::now(), &mut floats, &mut transforms);
        (floats, playing)
    }

    /// The defect this whole mechanism exists for: after the animation ends, the next
    /// transaction must still carry its final value, or WebRender reverts the binding to
    /// the value baked in at display-list time -- opacity 0 for a fade-in.
    #[test]
    fn finished_animation_keeps_its_final_value() {
        let animations = PipelineAnimations::default();
        animations.install_paint_animations(vec![finished_fade(1, 0.5)]);

        let (floats, playing) = sample(&animations);
        assert!(!playing, "an animation past its end is not playing");
        assert!(floats.is_empty(), "and it is no longer sampled");

        let mut floats = Vec::new();
        let mut transforms = Vec::new();
        let appended = animations.append_held_paint_values(&mut floats, &mut transforms);
        assert_eq!(appended, 1);
        assert_eq!(floats.len(), 1);
        assert_eq!(floats[0].key.id, opacity_key(1).id);
        assert_eq!(
            floats[0].value, 1.0,
            "the value it finished on, not the one it started from"
        );
    }

    /// Holding a value must never let it overwrite one that is still moving.
    #[test]
    fn a_running_animation_wins_over_a_held_value() {
        let animations = PipelineAnimations::default();
        animations.install_paint_animations(vec![finished_fade(1, 0.5), finished_fade(2, 0.5)]);
        sample(&animations);

        // Key 1 is being animated right now; key 2 is only held. Appending key 2 and not
        // key 1 is what proves the skip was a decision and not an empty held set.
        let mut floats = vec![PropertyValue {
            key: opacity_key(1),
            value: 0.25,
        }];
        let mut transforms = Vec::new();
        assert_eq!(
            animations.append_held_paint_values(&mut floats, &mut transforms),
            1
        );
        assert_eq!(floats.len(), 2);
        assert_eq!(floats[0].value, 0.25, "the live value is left alone");
        assert_eq!(floats[1].key.id, opacity_key(2).id);
    }

    /// A new display list that drives the key again takes the value back; one that says
    /// nothing about a key leaves it held, because the scene being rendered right now may
    /// still be the old one.
    #[test]
    fn a_new_display_list_reclaims_only_the_keys_it_drives() {
        let animations = PipelineAnimations::default();
        animations.install_paint_animations(vec![finished_fade(1, 0.5), finished_fade(2, 0.5)]);
        sample(&animations);

        animations.install_paint_animations(vec![running_fade(1, 10.0)]);

        let mut floats = Vec::new();
        let mut transforms = Vec::new();
        animations.append_held_paint_values(&mut floats, &mut transforms);
        assert_eq!(
            floats.len(),
            1,
            "key 1 is driven again; key 2 is still held"
        );
        assert_eq!(floats[0].key.id, opacity_key(2).id);
    }

    /// The held set is a value cache, not a queue: replaying the same key must not grow it.
    #[test]
    fn holding_the_same_key_twice_replaces_it() {
        let animations = PipelineAnimations::default();
        for _ in 0..3 {
            animations.install_paint_animations(vec![finished_fade(1, 0.5)]);
            sample(&animations);
        }

        let mut floats = Vec::new();
        let mut transforms = Vec::new();
        assert_eq!(
            animations.append_held_paint_values(&mut floats, &mut transforms),
            1
        );
    }
}

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
}

/// A [`PaintAnimation`] anchored to this painter's clock.
struct ActivePaintAnimation {
    property: PaintAnimationProperty,
    /// The instant the animation's own timeline reads zero. Usually in the past.
    zero: Instant,
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
    /// An incomplete animation never finishes on its own: it was sampled up to a horizon
    /// and the truth past that lives in script. Holding the last value is the honest
    /// thing to do -- it is what the viewer already sees -- and the next display list
    /// replaces it.
    fn running(&self, now: Instant) -> bool {
        if !self.complete {
            return true;
        }
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
        // A finished animation is dropped rather than kept at its end value: WebRender
        // holds the last value it was given, and the display list that ends the animation
        // carries the final value inline anyway.
        animations.retain(|animation| animation.running(now));
        for animation in animations.iter() {
            animation.sample(now, floats, transforms);
        }
        !animations.is_empty()
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

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Handing running CSS animations to the paint thread so it can keep playing them.
//!
//! ***An animation that only script can advance stops when script stops.*** Every frame of
//! a CSS animation in this engine costs a restyle, a whole-document display list, and a
//! scene build in every painter, because the animated value is baked into the display list
//! (`PropertyBinding::Value`). That is fine while the script thread is free. It is not fine
//! when the page is doing something else: measured on the 4-GPU wall, 2026-09-08
//! (`log_ani_perf/03`), one content-switch task held the script thread for 9.5 seconds and
//! the recurring ones held it for 310-345 ms each, while the painters were idle enough to
//! composite 60 frames a second throughout. The animation was frozen for all of it.
//!
//! So the animated value is bound instead of baked, and the next stretch of the animation
//! travels with the display list as a small set of segments the paint thread can evaluate
//! on its own. While script keeps up, every display list replaces the previous prediction
//! before it can be seen; when script stalls, the prediction is what keeps the animation
//! moving.
//!
//! Only properties that do not affect layout can work this way. `opacity` is one.

use paint_api::display_list::{
    PaintAnimatedProperty, PaintAnimation, PaintAnimationProperty, PaintAnimationSegment,
    paint_animation_binding_key,
};
use style::animation::{AnimationSetKey, DocumentAnimationSet};
use style::dom::OpaqueNode;
use style::properties::animated_properties::AnimationValue;
use style::properties::{LonghandId, OwnedPropertyDeclarationId, PropertyDeclarationId};
use style::values::computed::Transform as ComputedTransform;
use webrender_api::units::LayoutTransform;
use webrender_api::{PipelineId as WrPipelineId, PropertyBinding, PropertyBindingKey};

/// How far ahead of the display list each animation is sampled, in seconds.
///
/// This is a bet on how long script might stop producing display lists. Too short and the
/// animation freezes anyway at the end of the samples; too long and every display list
/// carries samples that are thrown away on the next one. The stalls that motivated this
/// were 0.3-0.4 s in steady state, with rare multi-second ones, and one second of runway
/// covers the first without making the common case expensive.
const HORIZON_SECONDS: f64 = 1.0;

/// Seconds between samples before collinear ones are merged away.
///
/// Sampling and then merging, rather than reading the keyframes, is deliberate: the sample
/// is whatever the style system says the value is, so easing, iteration, direction and fill
/// are all handled by the code that already implements them. A linear fade collapses to a
/// single segment; an eased one to a handful.
const SAMPLE_SECONDS: f64 = 1.0 / 60.0;

/// Two sampled values closer than this count as the same value.
const EPSILON: f32 = PaintAnimationSegment::<f32>::EPSILON;

/// The opacity this element's animations and transitions produce at `time`, if any of them
/// touch opacity at all.
///
/// Animations win over transitions, which is the cascade order for animated values.
fn animated_opacity(animations: &DocumentAnimationSet, node: OpaqueNode, time: f64) -> Option<f32> {
    let sets = animations.sets.read();
    let set = sets.get(&AnimationSetKey::new_for_non_pseudo(node))?;

    if let Some(map) = set.get_value_map_for_active_animations(time)
        && let Some(value) = map.get(&OwnedPropertyDeclarationId::Longhand(LonghandId::Opacity))
        && let AnimationValue::Opacity(opacity) = value
    {
        return Some(*opacity);
    }

    set.transitions
        .iter()
        .filter(|transition| {
            transition.property_animation.property_id()
                == PropertyDeclarationId::Longhand(LonghandId::Opacity)
        })
        .map(|transition| transition.calculate_value(time))
        .find_map(|value| match value {
            AnimationValue::Opacity(opacity) => Some(opacity),
            _ => None,
        })
}

/// Bind this fragment's opacity to a paint-side animation, if it has one.
///
/// Returns the binding to put in the display list and, when there is something to play,
/// the animation to send along with it. A constant sample run means the animation is not
/// actually changing opacity right now, and then nothing is bound: a binding with no
/// values behind it is worse than no binding, because WebRender would keep whatever it
/// last held.
pub(crate) fn opacity_binding(
    animations: &DocumentAnimationSet,
    pipeline_id: WrPipelineId,
    node: Option<OpaqueNode>,
    now: f64,
    opacity: f32,
) -> (PropertyBinding<f32>, Option<PaintAnimation>) {
    let unbound = (PropertyBinding::Value(opacity), None);
    if !servo_config::pref!(gfx_paint_side_animations_enabled) {
        return unbound;
    }
    let Some(node) = node else {
        return unbound;
    };
    if animated_opacity(animations, node, now).is_none() {
        return unbound;
    }

    let count = (HORIZON_SECONDS / SAMPLE_SECONDS).ceil() as usize + 1;
    let mut samples = Vec::with_capacity(count);
    for index in 0..count {
        let time = now + index as f64 * SAMPLE_SECONDS;
        match animated_opacity(animations, node, time) {
            Some(value) => samples.push(value),
            // The animation ended inside the horizon and no longer contributes a value.
            // Hold the last one: that is what the style system will compute too.
            None => {
                samples.push(samples.last().copied().unwrap_or(opacity));
            },
        }
    }

    let first = samples[0];
    if samples.iter().all(|value| (value - first).abs() <= EPSILON) {
        return unbound;
    }

    let segments = PaintAnimationSegment::<f32>::from_samples(&samples, SAMPLE_SECONDS);
    if segments.is_empty() {
        return unbound;
    }

    // Complete when the tail of the horizon is flat: the animation has settled and the
    // paint thread can stop rather than hold a value it would keep re-sending.
    let last = *samples.last().expect("just sampled");
    let settled = samples
        .iter()
        .rev()
        .take(3)
        .all(|value| (value - last).abs() <= EPSILON);

    let key =
        paint_animation_binding_key(pipeline_id, node.0 as u64, PaintAnimatedProperty::Opacity);
    (
        PropertyBinding::Binding(key, first),
        Some(PaintAnimation {
            property: PaintAnimationProperty::Opacity(key, segments),
            offset_from_display_list: 0.0,
            complete: settled,
        }),
    )
}

/// One `PAINTANIM built` line a second, and one whenever the count changes.
///
/// ***The count is the first thing to look at and the easiest to get silently wrong.***
/// Everything downstream -- the binding, the segments, the frames the painter generates --
/// depends on layout having recognised the animation at all, and a zero here separates
/// "not detected" from "detected but not played", which look identical on screen.
pub(crate) fn log_built(count: usize) {
    use std::cell::Cell;
    use std::time::Instant;

    thread_local! {
        static LAST: Cell<Option<(Instant, usize)>> = const { Cell::new(None) };
    }
    LAST.with(|last| {
        let now = Instant::now();
        let due = match last.get() {
            Some((at, previous)) => {
                previous != count || now.duration_since(at) >= std::time::Duration::from_secs(1)
            },
            None => true,
        };
        if !due {
            return;
        }
        last.set(Some((now, count)));
        // Zero is reported too: a page that should be animating and reports zero is
        // exactly what this line exists to show.
        log::warn!("PAINTANIM built animations={count}");
    });
}

/// The transform list this element's animations and transitions produce at `time`, if any
/// of them touch `transform` at all.
///
/// The caller turns it into a matrix, because that conversion needs the fragment's border
/// box and the rest of its style (`rotate`, `scale`, `translate`, `transform-origin`),
/// none of which belong here.
pub(crate) fn animated_transform_list(
    animations: &DocumentAnimationSet,
    node: OpaqueNode,
    time: f64,
) -> Option<ComputedTransform> {
    let sets = animations.sets.read();
    let set = sets.get(&AnimationSetKey::new_for_non_pseudo(node))?;

    if let Some(map) = set.get_value_map_for_active_animations(time)
        && let Some(value) = map.get(&OwnedPropertyDeclarationId::Longhand(LonghandId::Transform))
        && let AnimationValue::Transform(list) = value
    {
        return Some(list.clone());
    }

    set.transitions
        .iter()
        .filter(|transition| {
            transition.property_animation.property_id()
                == PropertyDeclarationId::Longhand(LonghandId::Transform)
        })
        .map(|transition| transition.calculate_value(time))
        .find_map(|value| match value {
            AnimationValue::Transform(list) => Some(list),
            _ => None,
        })
}

/// Bind this fragment's reference-frame transform to a paint-side animation, if it has one.
///
/// `resolve` turns a sampled transform list into the matrix the reference frame would be
/// pushed with; it is the caller's because that conversion needs the fragment.
pub(crate) fn transform_binding(
    animations: &DocumentAnimationSet,
    pipeline_id: WrPipelineId,
    node: Option<OpaqueNode>,
    now: f64,
    resolve: impl Fn(&ComputedTransform) -> Option<LayoutTransform>,
) -> Option<(PropertyBindingKey<LayoutTransform>, PaintAnimation)> {
    if !servo_config::pref!(gfx_paint_side_animations_enabled) {
        return None;
    }
    let node = node?;
    animated_transform_list(animations, node, now)?;

    let count = (HORIZON_SECONDS / SAMPLE_SECONDS).ceil() as usize + 1;
    let mut samples: Vec<LayoutTransform> = Vec::with_capacity(count);
    for index in 0..count {
        let time = now + index as f64 * SAMPLE_SECONDS;
        let sampled = animated_transform_list(animations, node, time)
            .as_ref()
            .and_then(&resolve);
        match sampled {
            Some(matrix) => samples.push(matrix),
            // Past the end of the animation the style system stops contributing a value.
            // Hold the last one, which is what it will compute too.
            None => match samples.last().copied() {
                Some(last) => samples.push(last),
                None => return None,
            },
        }
    }

    let segments = PaintAnimationSegment::<LayoutTransform>::from_samples(&samples, SAMPLE_SECONDS);
    if segments.is_empty() {
        return None;
    }

    let last = *samples.last().expect("just sampled");
    let settled = samples.iter().rev().take(3).all(|matrix| {
        matrix
            .to_array()
            .iter()
            .zip(last.to_array().iter())
            .all(|(a, b)| (a - b).abs() <= PaintAnimationSegment::<LayoutTransform>::EPSILON)
    });

    let key =
        paint_animation_binding_key(pipeline_id, node.0 as u64, PaintAnimatedProperty::Transform);
    Some((
        key,
        PaintAnimation {
            property: PaintAnimationProperty::Transform(key, segments),
            offset_from_display_list: 0.0,
            complete: settled,
        },
    ))
}

/// Report what the style system is actually animating, once a second.
///
/// ***`built=0` has two completely different causes and they need opposite work.*** Either
/// the page is running CSS animations of properties this module does not bind yet -- and
/// then the answer is to bind them -- or the style system has no animations at all,
/// because the page is moving things from script instead, and then nothing on the paint
/// side can help and the answer lies elsewhere entirely. Measured on the 4-GPU wall,
/// 2026-09-08 (`log_ani_perf/04`): 115 display lists, `built=0` on every one, with no way
/// to tell those apart from the log.
///
/// So this names the properties. It walks the whole document's animation set, which is
/// why it is throttled to once a second rather than run per display list.
pub(crate) fn log_document_animations(animations: &DocumentAnimationSet, now: f64) {
    use std::cell::Cell;
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    thread_local! {
        static LAST: Cell<Option<Instant>> = const { Cell::new(None) };
    }
    let due = LAST.with(|last| {
        let now = Instant::now();
        let due = last
            .get()
            .is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1));
        if due {
            last.set(Some(now));
        }
        due
    });
    if !due {
        return;
    }

    let sets = animations.sets.read();
    let mut animation_count = 0usize;
    let mut transition_count = 0usize;
    let mut properties: BTreeSet<&'static str> = BTreeSet::new();
    for set in sets.values() {
        animation_count += set.animations.len();
        transition_count += set.transitions.len();
        // The keyframes themselves are private, so ask for the values the animations
        // produce right now. One still inside its delay contributes nothing and is
        // counted but unnamed, which is the honest report.
        if let Some(map) = set.get_value_map_for_active_animations(now) {
            for id in map.keys() {
                if let OwnedPropertyDeclarationId::Longhand(id) = id {
                    properties.insert(id.name());
                }
            }
        }
        for transition in &set.transitions {
            if let PropertyDeclarationId::Longhand(id) = transition.property_animation.property_id()
            {
                properties.insert(id.name());
            }
        }
    }

    log::warn!(
        "PAINTANIM document elements={} animations={} transitions={} properties=[{}]",
        sets.len(),
        animation_count,
        transition_count,
        properties.into_iter().collect::<Vec<_>>().join(",")
    );
}

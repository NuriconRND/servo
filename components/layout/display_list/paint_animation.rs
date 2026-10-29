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
//!
//! ★런웨이는 반복 경계를 넘어간다 -- 넘기지 않으면 경계마다 멈춘다.★ stylo 의 샘플러는
//! `progress` 를 현재 반복에 클램프하므로(`third_party/stylo/style/servo/animation.rs`,
//! `total_progress >= 1.0` 이면 끝 키프레임을 그대로 적용한다) 미래 시각을 그냥 질의하면
//! ***다음 반복으로 감기지 않고 끝 값에 평탄해진다***. 그러면 런웨이가 지평이 아니라
//! 경계에서 끝나고, paint 는 다음 디스플레이 리스트까지 마지막 값을 붙든다.
//!
//! 2026-10-29 실측(개발 장비, `tests/html/ticker_slice_probe.html?speed=10`, 무한 반복
//! 자막 다섯):
//!   - 사이클당 한 번 멈칫하고 반복해도 완화되지 않는다. 전 띠가 ***같이*** 멈칫하고
//!     fps 는 60 그대로다 -- 프레임이 늦는 게 아니라 값이 안 변한다.
//!   - `gfx_paint_side_animations_enabled=false` 로 끄면 사라진다.
//!   - `gfx_paint_side_animation_tick_divisor` 1 대 15 에서 ***위치 오차***만 비례해
//!     줄어들고(250ms 분량 -> 17ms 분량) ★1 프레임 정지는 그대로 남는다★ -- 틱을 더
//!     자주 받는 것으로는 고칠 수 없다는 뜻이다.
//!   - ★`direction: alternate` 로 경계에서 값이 연속이어도 양 끝 모두에서 같은 증상이
//!     난다★ -- 위치 불연속이 아니라 ***붙듦***이 원인이라는 증거다.
//! `-FrameTime off`(위상 정합, `docs/superpowers/specs/2026-10-15-tick-animation-frame-time-design.md`)
//! 는 무영향이었고, 페이싱 클럭·커밋 모드·프레임 배리어·`overflow` 종류·타일 수도 무영향이다.

use paint_api::display_list::{
    PaintAnimatedProperty, PaintAnimation, PaintAnimationProperty, PaintAnimationSegment,
    fold_animation_sample_time, paint_animation_binding_key,
};
use servo_base::cross_process_instant::CrossProcessInstant;
use style::animation::{
    Animation, AnimationSetKey, DocumentAnimationSet, ElementAnimationSet, KeyframesIterationState,
};
use style::dom::OpaqueNode;
use style::properties::longhands::animation_direction::computed_value::single_value::T as AnimationDirection;

use style::properties::animated_properties::AnimationValue;
use style::properties::{LonghandId, OwnedPropertyDeclarationId, PropertyDeclarationId};
use style::values::computed::Transform as ComputedTransform;
use webrender_api::units::LayoutTransform;
use webrender_api::{PipelineId as WrPipelineId, PropertyBinding, PropertyBindingKey};

/// How many samples one animation may cost, whatever the horizon asks for.
///
/// The horizon is a pref and the sample rate is fixed, so without a cap a large value
/// would multiply the per-display-list style queries without bound. At the rate below this
/// is a little over four seconds of runway.
const MAX_SAMPLES: usize = 256;

/// How far ahead of the display list each animation is sampled, in seconds.
///
/// See `gfx_paint_side_animation_horizon_ms`: this is a bet on how long script might stop
/// producing display lists, and past it the paint thread holds the last value.
fn horizon_seconds() -> f64 {
    (servo_config::pref!(gfx_paint_side_animation_horizon_ms).max(0) as f64) / 1000.0
}

/// Seconds between samples before collinear ones are merged away.
///
/// Sampling and then merging, rather than reading the keyframes, is deliberate: the sample
/// is whatever the style system says the value is, so easing, iteration, direction and fill
/// are all handled by the code that already implements them. A linear fade collapses to a
/// single segment; an eased one to a handful.
const SAMPLE_SECONDS: f64 = 1.0 / 60.0;

/// Two sampled values closer than this count as the same value.
const EPSILON: f32 = PaintAnimationSegment::<f32>::EPSILON;

/// 반복 경계 너머의 질의 시각을 ★현재 반복 안으로 접는다.★
///
/// 모듈 머리 주석이 왜 필요한지 적는다. 요지는 stylo 가 경계 너머를 끝 키프레임으로
/// 클램프하고, `started_at` 을 전진시키는 `iterate()` 는 `&mut` 라 스크립트 틱에서만
/// 불린다는 것이다 -- 샘플링은 `&DocumentAnimationSet` 만 들고 있으므로 iterate 를 일으킬
/// 수 없다. 그래서 ***질의 시각 쪽을*** 접는다.
///
/// ★alternate 를 시간 거울반사로 정확히 다룬다 -- 사본도, 방향 뒤집기도 필요 없다.★
/// 역방향 반복의 값은 `keyframe(ease(1 - τ/주기))` 이고, 0번째 반복을 `주기 - τ` 에 질의한
/// 값이 `keyframe(ease((주기-τ)/주기))` 로 ***easing 까지 같다***. Web Animations 가
/// directed progress 를 뒤집은 뒤 easing 을 적용하기 때문이다. `Alternate` 는 0번째가
/// 정방향이고 `AlternateReverse` 는 0번째가 역방향이라, 둘 다 ★홀수 번째 반복에서만
/// 거울반사★하면 맞는다. `Normal`/`Reverse` 는 모든 반복이 같으므로 거울반사가 없다.
///
/// ★`Infinite` 에만 접는다.★ `Finite` 는 남은 반복 수와 비정수 마지막 반복
/// (`current_iteration_end_progress`)을 함께 따져야 하고, 다 끝난 뒤의 클램프는 `fill` 의
/// 정의된 동작이다. 지금 동작을 그대로 둔다.
fn folded_sample_time(animation: &Animation, time: f64) -> f64 {
    if !matches!(
        animation.iteration_state,
        KeyframesIterationState::Infinite(_)
    ) {
        return time;
    }
    let alternating = match animation.direction {
        AnimationDirection::Alternate | AnimationDirection::AlternateReverse => true,
        AnimationDirection::Normal | AnimationDirection::Reverse => false,
    };
    fold_animation_sample_time(
        animation.started_at,
        animation.current_iteration_duration(),
        alternating,
        time,
    )
}

/// `ElementAnimationSet::get_value_map_for_active_animations` 와 같은 일을 하되,
/// ★애니메이션마다 질의 시각을 따로 접는다.★
///
/// 세트 단위 API 는 모든 애니메이션에 같은 `now` 를 주는데, 한 요소에 주기가 다른
/// 애니메이션이 여럿 걸릴 수 있으므로 접기는 애니메이션 단위여야 한다. 합성 순서는 세트
/// 단위 API 와 같다(`animations` 순서대로 맵에 덮어쓴다).
fn folded_animated_value(
    set: &ElementAnimationSet,
    longhand: LonghandId,
    time: f64,
) -> Option<AnimationValue> {
    if !set.has_active_animation() {
        return None;
    }
    let mut map = Default::default();
    for animation in &set.animations {
        animation.get_property_declaration_at_time(folded_sample_time(animation, time), &mut map);
    }
    map.get(&OwnedPropertyDeclarationId::Longhand(longhand))
        .cloned()
}

/// The opacity this element's animations and transitions produce at `time`, if any of them
/// touch opacity at all.
///
/// Animations win over transitions, which is the cascade order for animated values.
fn animated_opacity(animations: &DocumentAnimationSet, node: OpaqueNode, time: f64) -> Option<f32> {
    let sets = animations.sets.read();
    let set = sets.get(&AnimationSetKey::new_for_non_pseudo(node))?;

    if let Some(AnimationValue::Opacity(opacity)) =
        folded_animated_value(set, LonghandId::Opacity, time)
    {
        return Some(opacity);
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
    built_at: CrossProcessInstant,
    opacity: f32,
) -> (PropertyBinding<f32>, Option<PaintAnimation>) {
    let unbound = (PropertyBinding::Value(opacity), None);
    if !servo_config::pref!(gfx_paint_side_animations_enabled) {
        return unbound;
    }
    let Some(node) = node else {
        return unbound;
    };
    if !animations
        .sets
        .read()
        .contains_key(&AnimationSetKey::new_for_non_pseudo(node))
    {
        return unbound;
    }
    if animated_opacity(animations, node, now).is_none() {
        return unbound;
    }

    let count = ((horizon_seconds() / SAMPLE_SECONDS).ceil() as usize + 1).min(MAX_SAMPLES);
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
            built_at,
            complete: settled,
        }),
    )
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

    if let Some(AnimationValue::Transform(list)) =
        folded_animated_value(set, LonghandId::Transform, time)
    {
        return Some(list);
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
    built_at: CrossProcessInstant,
    resolve: impl Fn(&ComputedTransform) -> Option<LayoutTransform>,
) -> Option<(PropertyBindingKey<LayoutTransform>, PaintAnimation)> {
    if !servo_config::pref!(gfx_paint_side_animations_enabled) {
        return None;
    }
    let node = node?;
    if animated_transform_list(animations, node, now).is_none() {
        return None;
    }

    let count = ((horizon_seconds() / SAMPLE_SECONDS).ceil() as usize + 1).min(MAX_SAMPLES);
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
                None => {
                    return None;
                },
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
            built_at,
            complete: settled,
        },
    ))
}

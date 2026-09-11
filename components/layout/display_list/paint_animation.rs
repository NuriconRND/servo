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
use std::cell::RefCell;

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

/// Why a binding did or did not happen, for one display list.
///
/// ***`built=0` has several causes and they look identical from outside.*** Measured on
/// the 4-GPU wall, 2026-09-08 (log_ani_perf/08): the document reported one to three
/// running animations of exactly `opacity` and `transform`, and not one of them was ever
/// bound, across 115 display lists. Whether that is the element never reaching this code,
/// the node not matching the animation set, or the samples coming back constant is the
/// whole question, and nothing distinguished them.
#[derive(Default)]
struct BindTally {
    /// Stacking contexts that returned before this code could look, on an element that
    /// does have a running animation. The interesting half of the early return.
    skipped_with_animation: u32,
    considered: u32,
    no_tag: u32,
    not_in_set: u32,
    no_value: u32,
    constant: u32,
    bound: u32,
    transform_considered: u32,
    transform_no_rect: u32,
    transform_no_value: u32,
    transform_constant: u32,
    transform_bound: u32,
}

thread_local! {
    static TALLY: RefCell<BindTally> = RefCell::new(BindTally::default());
}

fn tally(update: impl FnOnce(&mut BindTally)) {
    TALLY.with(|cell| update(&mut cell.borrow_mut()));
}

/// Which property a binding edge is about.
const PROP_OPACITY: u8 = 0;
const PROP_TRANSFORM: u8 = 1;

/// At most this many `PAINTANIMEDGE` lines a second, whatever happens.
const MAX_EDGE_LINES_PER_SECOND: u32 = 30;

thread_local! {
    /// How many display lists in a row this (node, property) has been considered and left
    /// unbound.
    ///
    /// ***This is the length of the wrong frame.*** An element drawn unbound carries its
    /// own style, which for one mid-transition is wherever the page left it -- the target
    /// position before the animation starts, the starting position after it ends. One
    /// display list of that is the reported flash; sixty of them is the reported blackout.
    /// Same defect, and only the count tells them apart.
    static UNBOUND_STREAK: RefCell<std::collections::HashMap<(u64, u8), u32>> =
        RefCell::new(std::collections::HashMap::new());
    /// (node, property) pairs that got a binding in the previous display list.
    static BOUND_PREVIOUS: RefCell<std::collections::HashSet<(u64, u8)>> =
        RefCell::new(std::collections::HashSet::new());
    /// The same, accumulating for the display list being built now.
    static BOUND_CURRENT: RefCell<std::collections::HashSet<(u64, u8)>> =
        RefCell::new(std::collections::HashSet::new());
    /// ***Two budgets, not one.*** A transition binds dozens of nodes at once, and a
    /// shared budget would let that burst of `gained` lines crowd out the `lost` line --
    /// the one the whole thing is for -- in the very second it happens.
    static EDGE_BUDGET_LOST: std::cell::Cell<(Option<std::time::Instant>, u32)> =
        const { std::cell::Cell::new((None, 0)) };
    static EDGE_BUDGET_GAINED: std::cell::Cell<(Option<std::time::Instant>, u32)> =
        const { std::cell::Cell::new((None, 0)) };
}

fn property_name(property: u8) -> &'static str {
    if property == PROP_OPACITY {
        "opacity"
    } else {
        "transform"
    }
}

/// Whether another `PAINTANIMEDGE` line of this kind may go out this second.
fn edge_line_allowed(
    budget: &'static std::thread::LocalKey<std::cell::Cell<(Option<std::time::Instant>, u32)>>,
) -> bool {
    budget.with(|cell| {
        let now = std::time::Instant::now();
        let (start, used) = cell.get();
        let fresh =
            start.is_none_or(|at| now.duration_since(at) >= std::time::Duration::from_secs(1));
        let (start, used) = if fresh { (Some(now), 0) } else { (start, used) };
        if used >= MAX_EDGE_LINES_PER_SECOND {
            cell.set((start, used));
            return false;
        }
        cell.set((start, used + 1));
        true
    })
}

/// This element's animations, as state the log can be read against.
///
/// ***`Canceled` and `Finished` land on the same screen and mean opposite things.*** With
/// `fill-mode: none` a finished animation stops contributing a value and the element goes
/// back to its own style, which is correct; a cancel does the same thing but for a reason
/// the page never asked for. From the display list both look like "no value", and that is
/// the distinction this line exists to make.
fn describe_animations(animations: &DocumentAnimationSet, node: OpaqueNode, now: f64) -> String {
    let sets = animations.sets.read();
    let Some(set) = sets.get(&AnimationSetKey::new_for_non_pseudo(node)) else {
        // ***`OpaqueNode` is the node's address, so a repeated id is not a repeated
        // element.*** If the page rebuilt this container the id can come back on a fresh
        // node, and then a missing set is correct and the defect is elsewhere. Naming the
        // keys the document does hold separates the two: ids we have never bound mean new
        // elements; our own ids still present mean the entry went away under us.
        let mut keys: Vec<String> = sets
            .keys()
            .take(4)
            .map(|key| format!("{}", key.node.0))
            .collect();
        if sets.len() > keys.len() {
            keys.push(format!("+{}", sets.len() - keys.len()));
        }
        return format!("set_missing doc=[{}]", keys.join(","));
    };
    if set.animations.is_empty() && set.transitions.is_empty() {
        return "set_empty".to_string();
    }
    let mut parts: Vec<String> = set
        .animations
        .iter()
        .map(|animation| {
            let progress = if animation.duration > 0.0 {
                (now - animation.started_at) / animation.duration
            } else {
                f64::INFINITY
            };
            format!(
                "{}:{:?}/fill={:?}/p={:.3}/delay={:.3}",
                animation.name, animation.state, animation.fill_mode, progress, animation.delay
            )
        })
        .collect();
    if !set.transitions.is_empty() {
        parts.push(format!("transitions={}", set.transitions.len()));
    }
    parts.join(",")
}

/// Remember that this node's property got a binding in the display list being built.
fn note_bound(node: OpaqueNode, property: u8) {
    BOUND_CURRENT.with(|cell| {
        cell.borrow_mut().insert((node.0 as u64, property));
    });
}

/// One display list drew this property bound and the next one does not.
///
/// ***This is the frame the defect is visible on, and a per-second counter cannot hold
/// it.*** When the binding goes away the display list carries the element's own style
/// instead -- for an element mid-transition that is wherever the page left it, which on
/// this wall was off the right edge of an 11520px viewport (log_ani_debug/00, 08:11:01:
/// four painters, 61 frames, zero external images resolved). So the edge is logged where
/// it happens, with the style state that explains which kind of "no value" it was.
fn note_unbound(
    animations: &DocumentAnimationSet,
    node: OpaqueNode,
    property: u8,
    reason: &str,
    now: f64,
) {
    // Bumped for every unbound outcome, not only the edge: the count is read at the far
    // end, by `note_gained`.
    UNBOUND_STREAK.with(|cell| {
        let mut map = cell.borrow_mut();
        // A page that churns nodes would otherwise grow this map without bound. Dropping
        // it whole costs at most one under-reported streak.
        if map.len() > 4096 {
            map.clear();
        }
        *map.entry((node.0 as u64, property)).or_insert(0) += 1;
    });
    let was_bound = BOUND_PREVIOUS.with(|cell| cell.borrow().contains(&(node.0 as u64, property)));
    if !was_bound || !edge_line_allowed(&EDGE_BUDGET_LOST) {
        return;
    }
    log::warn!(
        "PAINTANIMEDGE edge=lost node={} prop={} reason={} anims=[{}]",
        node.0,
        property_name(property),
        reason,
        describe_animations(animations, node, now)
    );
}

/// The other end: the binding comes back. Pairs with `edge=lost` to give the gap a length.
fn note_gained(animations: &DocumentAnimationSet, node: OpaqueNode, property: u8, now: f64) {
    let streak = UNBOUND_STREAK
        .with(|cell| cell.borrow_mut().remove(&(node.0 as u64, property)))
        .unwrap_or(0);
    let was_bound = BOUND_PREVIOUS.with(|cell| cell.borrow().contains(&(node.0 as u64, property)));
    if was_bound || !edge_line_allowed(&EDGE_BUDGET_GAINED) {
        return;
    }
    log::warn!(
        "PAINTANIMEDGE edge=gained node={} prop={} unbound_dls={} anims=[{}]",
        node.0,
        property_name(property),
        streak,
        describe_animations(animations, node, now)
    );
}

/// The transform actually baked into the display list while this node has no binding.
///
/// ***Every number so far has been about whether a binding was there, not about what was
/// drawn instead.*** An unbound reference frame carries the element's own computed
/// transform, and that value is the whole question: off the side of an 11520px viewport
/// and the wall is black by construction; at rest and the blank comes from somewhere else
/// entirely. Logged on the first unbound display list and once a second after that, so a
/// long blank is sampled without flooding.
pub(crate) fn note_unbound_transform_value(
    node: Option<OpaqueNode>,
    transform: &LayoutTransform,
    origin: webrender_api::units::LayoutPoint,
) {
    let Some(node) = node else {
        return;
    };
    let streak = UNBOUND_STREAK.with(|cell| {
        cell.borrow()
            .get(&(node.0 as u64, PROP_TRANSFORM))
            .copied()
            .unwrap_or(0)
    });
    // 0 means this node never reached the binding code at all (the pref is off, or it has
    // no tag); those are not the elements in question.
    if streak == 0 || (streak > 1 && streak % 60 != 0) {
        return;
    }
    let matrix = transform.to_array();
    log::warn!(
        "PAINTANIMSTATIC node={} unbound_dls={} origin={:.1}/{:.1} translate={:.1}/{:.1} scale={:.3}",
        node.0,
        streak,
        origin.x,
        origin.y,
        matrix[12],
        matrix[13],
        matrix[0]
    );
}

/// Close the display list: this build's bindings become the previous ones.
fn roll_binding_edges() {
    let current = BOUND_CURRENT.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
    BOUND_PREVIOUS.with(|cell| *cell.borrow_mut() = current);
}

/// A stacking context that never reached the binding code. Counted only when the element
/// actually has an animation, because that is the only case worth explaining.
pub(crate) fn note_stacking_context_skipped(
    animations: &DocumentAnimationSet,
    node: Option<OpaqueNode>,
) {
    let Some(node) = node else {
        return;
    };
    if animations
        .sets
        .read()
        .contains_key(&AnimationSetKey::new_for_non_pseudo(node))
    {
        tally(|counters| counters.skipped_with_animation += 1);
    }
}

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
    tally(|counters| counters.considered += 1);
    let Some(node) = node else {
        tally(|counters| counters.no_tag += 1);
        return unbound;
    };
    if !animations
        .sets
        .read()
        .contains_key(&AnimationSetKey::new_for_non_pseudo(node))
    {
        tally(|counters| counters.not_in_set += 1);
        note_unbound(animations, node, PROP_OPACITY, "not_in_set", now);
        return unbound;
    }
    if animated_opacity(animations, node, now).is_none() {
        tally(|counters| counters.no_value += 1);
        note_unbound(animations, node, PROP_OPACITY, "no_value", now);
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
        tally(|counters| counters.constant += 1);
        note_unbound(animations, node, PROP_OPACITY, "constant", now);
        return unbound;
    }

    let segments = PaintAnimationSegment::<f32>::from_samples(&samples, SAMPLE_SECONDS);
    if segments.is_empty() {
        tally(|counters| counters.constant += 1);
        note_unbound(animations, node, PROP_OPACITY, "no_segments", now);
        return unbound;
    }
    tally(|counters| counters.bound += 1);
    note_gained(animations, node, PROP_OPACITY, now);
    note_bound(node, PROP_OPACITY);

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

    // ***Unconditional, and before the throttle.*** The summary below is skipped when
    // nothing changed; the edge state is per display list and skipping it would make the
    // next build compare against a list two builds old.
    roll_binding_edges();

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
        let counters = TALLY.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
        // Zero is reported too: a page that should be animating and reports zero is
        // exactly what this line exists to show -- and the tally says which zero it is.
        log::warn!(
            "PAINTANIM built animations={count} skipped_with_animation={} considered={}              no_tag={} not_in_set={} no_value={} constant={} bound={} tx_considered={}              tx_no_rect={} tx_no_value={} tx_constant={} tx_bound={}",
            counters.skipped_with_animation,
            counters.considered,
            counters.no_tag,
            counters.not_in_set,
            counters.no_value,
            counters.constant,
            counters.bound,
            counters.transform_considered,
            counters.transform_no_rect,
            counters.transform_no_value,
            counters.transform_constant,
            counters.transform_bound,
        );
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
    tally(|counters| counters.transform_considered += 1);
    let node = node?;
    if animated_transform_list(animations, node, now).is_none() {
        tally(|counters| counters.transform_no_value += 1);
        note_unbound(animations, node, PROP_TRANSFORM, "no_value", now);
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
                    note_unbound(animations, node, PROP_TRANSFORM, "unresolvable", now);
                    return None;
                },
            },
        }
    }

    let segments =
        PaintAnimationSegment::<LayoutTransform>::from_samples(&samples, SAMPLE_SECONDS);
    if segments.is_empty() {
        tally(|counters| counters.transform_constant += 1);
        note_unbound(animations, node, PROP_TRANSFORM, "constant", now);
        return None;
    }
    tally(|counters| counters.transform_bound += 1);
    note_gained(animations, node, PROP_TRANSFORM, now);
    note_bound(node, PROP_TRANSFORM);

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

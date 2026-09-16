/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The set of animations for a document.

use std::cell::Cell;

use cssparser::ToCss;
use embedder_traits::{AnimationState as AnimationsPresentState, UntrustedNodeAddress};
use libc::c_void;
use rustc_hash::{FxHashMap, FxHashSet};
use script_bindings::cell::DomRefCell;
use serde::{Deserialize, Serialize};
use servo_base::id::PipelineId;
use servo_constellation_traits::ScriptToConstellationMessage;
use style::animation::{
    Animation, AnimationOrigin, AnimationSetKey, AnimationState, DocumentAnimationSet,
    ElementAnimationSet, KeyframesIterationState, ScriptAnimationRequest, Transition,
};
use style::dom::OpaqueNode;
use style::selector_parser::PseudoElement;
use stylo_atoms::Atom;

use crate::dom::animationevent::AnimationEvent;
use crate::dom::bindings::codegen::Bindings::AnimationEventBinding::AnimationEventInit;
use crate::dom::bindings::codegen::Bindings::EventBinding::EventInit;
use crate::dom::bindings::codegen::Bindings::TransitionEventBinding::TransitionEventInit;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::trace::NoTrace;
use crate::dom::event::Event;
use crate::dom::node::{Node, NodeDamage, NodeTraits, from_untrusted_node_address};
use crate::dom::transitionevent::TransitionEvent;
use crate::dom::window::Window;
use crate::script_runtime::CanGc;

/// The set of animations for a document.
#[derive(Default, JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct Animations {
    /// The map of nodes to their animation states.
    #[no_trace]
    pub(crate) sets: DocumentAnimationSet,

    /// Whether or not we have animations that are running.
    has_running_animations: Cell<bool>,

    /// A list of nodes with in-progress CSS transitions or pending events.
    rooted_nodes: DomRefCell<FxHashMap<NoTrace<OpaqueNode>, Dom<Node>>>,

    /// A list of pending animation-related events.
    pending_events: DomRefCell<Vec<TransitionOrAnimationEvent>>,

    /// The timeline value at the last time all animations were marked dirty.
    /// This is used to prevent marking animations dirty when the timeline
    /// has not changed.
    timeline_value_at_last_dirty: Cell<f64>,

    /// `Element.animate()` 로 만든 애니메이션에 붙일 다음 합성 이름의 번호.
    script_animation_counter: Cell<u64>,
}

impl Animations {
    pub(crate) fn new() -> Self {
        Animations {
            sets: Default::default(),
            has_running_animations: Cell::new(false),
            rooted_nodes: Default::default(),
            pending_events: Default::default(),
            timeline_value_at_last_dirty: Cell::new(0.0),
            script_animation_counter: Cell::new(0),
        }
    }

    pub(crate) fn clear(&self) {
        self.sets.sets.write().clear();
        self.rooted_nodes.borrow_mut().clear();
        self.pending_events.borrow_mut().clear();
    }

    /// `Element.animate()` 가 만든 애니메이션에 붙일 이름.
    ///
    /// 페이지의 `@keyframes` 이름과 충돌할 수 없는 모양이어야 한다 -- 충돌하면
    /// `maybe_start_animations` 가 같은 이름의 CSS 애니메이션으로 착각한다.
    pub(crate) fn next_script_animation_name(&self) -> Atom {
        let index = self.script_animation_counter.get() + 1;
        self.script_animation_counter.set(index);
        Atom::from(format!("-servo-script-{index}"))
    }

    /// 스크립트 애니메이션 요청을 세트에 적재한다. 실제 애니메이션은 다음
    /// 리스타일에서 만들어진다(`ElementAnimationSet::start_script_animations`).
    ///
    /// ***스크립트가 적재한 세트는 여기서 곧바로 루팅되며, 주소 부활
    /// (`from_untrusted_node_address`)에 기대서는 안 된다.*** CSS 경로의 키는 전부
    /// 리스타일이 만들어 주소가 살아 있음이 보장되지만, 이 경로의 키는 그렇지 않다.
    pub(crate) fn add_script_animation(
        &self,
        node: &Node,
        key: AnimationSetKey,
        request: ScriptAnimationRequest,
    ) {
        // ***파일링 시점에 루팅한다.*** 키는 리플렉터의 JSObject 주소(`usize`)일 뿐 아무것도
        // 소유하지 않는다. 문서에 붙은 적 없는 요소는 `unbind_from_tree` 를 거치지 않아
        // `cancel_animations_for_node` 도 돌지 않으므로, 루팅하지 않으면 세트가 노드보다
        // 오래 살아남고 `root_newly_animating_dom_nodes` 가 죽은 주소를 역참조한다.
        // (SpiderMonkey 는 JSObject 주소를 재사용하므로, 죽지 않더라도 엉뚱한 새 요소를
        // 가리켜 조용히 다른 노드를 애니메이트할 수 있다.)
        //
        // `rooted_nodes` 빌림은 `sets` 쓰기 잠금보다 먼저 잡고 이 문장에서 끝낸다 --
        // 둘을 동시에 들고 있지 않는다.
        self.rooted_nodes
            .borrow_mut()
            .entry(NoTrace(key.node))
            .or_insert_with(|| Dom::from_ref(node));

        let mut sets = self.sets.sets.write();
        let set = sets.entry(key).or_default();
        set.pending_script.push(request);
        set.dirty = true;
    }

    /// `Animation.cancel()`. 무언가 바뀌었으면 `true`.
    pub(crate) fn cancel_script_animation(&self, key: &AnimationSetKey, name: &Atom) -> bool {
        let mut sets = self.sets.sets.write();
        let Some(set) = sets.get_mut(key) else {
            return false;
        };
        set.cancel_script_animation(name)
    }

    // Mark all animations dirty, if they haven't been marked dirty since the
    // specified `current_timeline_value`. Returns true if animations were marked
    // dirty or false otherwise.
    pub(crate) fn mark_animating_nodes_as_dirty(&self, current_timeline_value: f64) -> bool {
        if current_timeline_value <= self.timeline_value_at_last_dirty.get() {
            return false;
        }
        self.timeline_value_at_last_dirty
            .set(current_timeline_value);

        let sets = self.sets.sets.read();
        let rooted_nodes = self.rooted_nodes.borrow();
        for node in sets
            .keys()
            .filter_map(|key| rooted_nodes.get(&NoTrace(key.node)))
        {
            node.dirty(NodeDamage::Style);
        }

        true
    }

    pub(crate) fn update_for_new_timeline_value(&self, window: &Window, now: f64) {
        let pipeline_id = window.pipeline_id();
        let mut sets = self.sets.sets.write();

        for (key, set) in sets.iter_mut() {
            self.start_pending_animations(key, set, now, pipeline_id);

            // When necessary, iterate our running animations to the next iteration.
            for animation in set.animations.iter_mut() {
                if animation.iterate_if_necessary(now) {
                    self.add_animation_event(
                        key,
                        animation,
                        TransitionOrAnimationEventType::AnimationIteration,
                        now,
                        pipeline_id,
                    );
                }
            }

            self.finish_running_animations(key, set, now, pipeline_id);
        }

        self.unroot_unused_nodes(&sets);
    }

    /// Cancel animations for the given node, if any exist.
    pub(crate) fn cancel_animations_for_node(&self, node: &Node) {
        let mut animations = self.sets.sets.write();
        // ***세 번째 취소 자리.*** 이 함수는 노드가 트리에서 빠질 때(`unbind_from_tree`
        // 정리 경로) 불린다. 여기서 취소된 애니메이션은 `clear_canceled_animations` 에
        // 쓸려 세트가 비고, `sets.retain` 이 세트를 지운다. 그러면 레이아웃은
        // `not_in_set` 으로 떨어져 그 요소를 자기 스타일로 그린다.
        //
        // log_ani_debug/06 에서 `ANIMSETDROP node=9888400807024` 가 17번 찍혔는데
        // `ANIMCANCEL`(스타일 검사) 도 `ANIMDROP`(렌더링 안 됨) 도 그 노드에는 한 번도
        // 안 찍혔다. 남은 취소 자리가 여기뿐이라 무엇이 여기로 들어오는지를 남긴다.
        let mut cancel_animations_for = |key: AnimationSetKey| {
            if let Some(set) = animations.get_mut(&key) {
                if !set.animations.is_empty() {
                    log::warn!(
                        "ANIMUNBIND node={} pseudo={:?} animations=[{}]",
                        key.node.0,
                        key.pseudo_element,
                        set.animations
                            .iter()
                            .map(|animation| format!(
                                "{}:{:?}/fill={:?}",
                                animation.name, animation.state, animation.fill_mode
                            ))
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                }
                set.cancel_all_animations();
            }
        };

        let opaque_node = node.to_opaque();
        cancel_animations_for(AnimationSetKey::new_for_non_pseudo(opaque_node));
        cancel_animations_for(AnimationSetKey::new_for_pseudo(
            opaque_node,
            PseudoElement::Before,
        ));
        cancel_animations_for(AnimationSetKey::new_for_pseudo(
            opaque_node,
            PseudoElement::After,
        ));
    }

    /// Promote `Pending` animations **before** layout builds the display list.
    ///
    /// ***디스플레이 리스트는 승급이 끝난 세트를 보아야 한다.*** 승급은
    /// [`Self::do_post_reflow_update`] 안에 있었는데, 그 호출은 `Window::force_reflow` 에서
    /// `layout.reflow()` **뒤**다. 그래서 요소가 처음 등장하는 리플로에서는 애니메이션이
    /// 아직 `Pending` 인 채로 디스플레이 리스트가 만들어지고,
    /// `get_value_map_for_active_animations` 가 값을 내주지 않으니 레이아웃이
    /// `transform_binding` 에서 바인딩을 포기하고 **요소 자신의 계산된 transform** 을 굽는다.
    /// 슬라이드-인 애니메이션에서 그 값은 곧 **도착 위치**이므로, 새 구성이 제자리에 한 장
    /// 나타났다가 애니메이션이 붙으면서 시작 위치로 튀어 다시 들어온다.
    ///
    /// 실측(2026-09-16, log_ani_debug/13): `edge=gained` 90 건 중 75 건이 `unbound_dls` 1~2 이고
    /// (p50=1), 그 첫 디스플레이 리스트에 구워진 값이 `PAINTANIMSTATIC ... translate=0.0/0.0
    /// names=[sd-anim-5]` -- 스타일은 애니메이션을 지명하는데 도착 위치가 구워져 있다.
    ///
    /// 그래서 같은 승급을 리플로 **앞**에서 한 번 더 돌린다. `start_pending_animations` 는
    /// 이미 `Pending` 이 아닌 것을 건너뛰므로 뒤의 호출은 그대로 두어도 무해하고, 틱이 끊긴
    /// 경우를 위한 안전망으로 남는다(커밋 `ad1412abd60` 이 그 자리를 만든 이유다).
    pub(crate) fn start_pending_animations_before_reflow(&self, window: &Window, now: f64) {
        let mut sets = self.sets.sets.write();
        if sets.is_empty() {
            return;
        }
        let pipeline_id = window.pipeline_id();
        for (key, set) in sets.iter_mut() {
            self.start_pending_animations(key, set, now, pipeline_id);
        }
    }

    /// This does three things:
    ///  - Cancel animations for any nodes that are no longer being rendered or delegating rendering.
    ///  - Process any new animations that were discovered after reflow.
    ///  - Collect pending events for any animations that changed state.
    pub(crate) fn do_post_reflow_update(&self, window: &Window, now: f64) {
        let mut sets = self.sets.sets.write();
        {
            let rooted_nodes = self.rooted_nodes.borrow();
            for (key, set) in sets.iter_mut() {
                let Some(node) = rooted_nodes.get(&NoTrace(key.node)) else {
                    continue;
                };
                if node.is_being_rendered_or_delegates_rendering(key.pseudo_element) {
                    continue;
                }
                // ***이 취소는 스타일을 보지 않는다.*** `Animation::is_cancelled_in_new_style`
                // 을 거치지 않으므로 거기 붙인 진단(ANIMCANCEL)에 잡히지 않고, 취소된
                // 애니메이션은 바로 아래에서 쓸려나가 세트가 비고, 세트가 비면
                // `sets.retain` 이 문서에서 지운다. 레이아웃은 그 뒤로 그 노드를
                // `not_in_set` 으로 보고 요소를 자기 스타일로 그린다 -- `fill-mode:
                // forwards` 로 붙들고 있어야 할 마지막 키프레임까지 같이 버려진 채로.
                //
                // 화면에 보이는 요소가 여기 걸린다면 판정이 틀린 것이므로, 그때 노드가
                // 문서에 붙어 있었는지와 무엇을 버렸는지를 남긴다.
                if !set.animations.is_empty() {
                    log::warn!(
                        "ANIMDROP node={} pseudo={:?} connected={} animations=[{}]",
                        key.node.0,
                        key.pseudo_element,
                        node.is_connected(),
                        set.animations
                            .iter()
                            .map(|animation| format!(
                                "{}:{:?}/fill={:?}",
                                animation.name, animation.state, animation.fill_mode
                            ))
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                }
                set.cancel_all_animations();
            }
        }

        let pipeline_id = window.pipeline_id();
        self.root_newly_animating_dom_nodes(&sets);

        for (key, set) in sets.iter_mut() {
            // ***시작은 틱을 기다리면 안 된다.***
            //
            // `Pending -> Running` 승급이 일어나는 자리는 여기 말고
            // `update_for_new_timeline_value` 하나뿐이고, 그건 페인트 refresh driver 가
            // 보내는 애니메이션 틱을 타고 온다. 그 틱은 두 겹으로 조건부다: 페인트가
            // 재생 중인 WebView 는 `gfx_paint_side_animation_tick_divisor` 프레임마다
            // 한 번만 받고, "애니메이션이 있다" 신호가 꺼지면 refresh driver 가 관찰
            // 자체를 그만둔다(`ANIMTICK stop`). 그 신호는 바로 아래 `sets.retain` 이
            // 세트를 지운 뒤에 계산되므로, 세트가 한 번 비면 틱이 끊기고, 틱이 끊기면
            // 그 뒤에 만들어진 애니메이션은 영영 시작하지 못한다.
            //
            // 리스타일도 구해 주지 못한다 -- `Animation::update_from_other` 의 승급은
            // `old_state != Pending` 일 때만 돌기 때문에 이미 Pending 인 것은 몇 번을
            // 재제출해도 Pending 이다.
            //
            // 실측(log_ani_debug/05): sd-anim-85 가 9초 동안 Pending p=0.000 에 머물렀고,
            // 그동안 그 요소는 첫 키프레임인 translateX(11520) -- 폭 11520 뷰포트의
            // 완전히 바깥 -- 에 주차되어 벽이 검었다.
            //
            // 그래서 승급을 리플로 경로로 옮긴다. 이 함수는 매 리플로 돌고 틱과 무관하다.
            // 이벤트는 같은 함수가 그대로 쏘므로 `animationstart` 는 유지된다. 페인트측
            // 틱 억제는 그대로 둔다 -- 그건 *이미 도는* 애니메이션을 진행시키는 얘기이고,
            // 시작시키는 것과는 별개다.
            self.start_pending_animations(key, set, now, pipeline_id);
            self.handle_canceled_animations(key, set, now, pipeline_id);
            self.handle_new_animations(key, set, now, pipeline_id);
        }

        // Remove empty states from our collection of states in order to free
        // up space as soon as we are no longer tracking any animations for
        // a node.
        //
        // ***지워진 항목은 레이아웃에서 `not_in_set` 이 되고, 그러면 그 요소는 자기
        // 스타일로 그려진다.*** 애니메이션이 붙기 전의 자기 스타일이 화면 밖이면 그
        // 순간부터 벽이 검다. 그래서 무엇이 언제 지워지는지를 남긴다.
        //
        // ***지우기 전에 그 노드를 스타일 더티로 표시한다.***
        //
        // 애니메이션을 (다시) 만드는 곳은 `maybe_start_animations` 하나이고, 그것은 그
        // 요소가 리스타일될 때만 돈다. 그런데 애니메이션이 리스타일을 유발하는 곳은
        // `mark_animating_nodes_as_dirty` 이고, 그 함수는 **세트가 있는 노드만** 더럽힌다.
        // 그래서 여기서 세트를 지우면 그 노드는 그 목록에서 빠지고, 다시는 리스타일되지
        // 않으며, 리스타일되지 않으니 애니메이션도 다시 만들어지지 않는다. 되살릴 유일한
        // 경로가 "이미 애니메이션이 있는 요소" 에만 도는 자기잠금이다.
        //
        // 실측(log_ani_debug/07, node 64280494740912): `sd-anim-4` 가 세트 소멸 뒤
        // 2.5~4.8초가 지나서야 -- 페이지가 그 요소를 다른 이유로 건드릴 때에야 -- 새
        // `started_at` 으로 다시 만들어졌고, 그 사이 93 디스플레이 리스트 동안 요소는
        // 스타일이 `animation-name: sd-anim-4` 를 그대로 지명한 채(`@keyframes` 도 있다)
        // 애니메이션 없이 `translateX(11520)` -- 폭 11520 뷰포트의 바깥 -- 에 그려졌다.
        // 슬롯 둘이 동시에 ±11520 에 주차되어 화면에 아무것도 남지 않는다.
        //
        // 더티로 표시해 두면 다음 리플로에서 `update_animations_for_new_style` 이 그
        // 요소에 대해 돌고, 스타일이 여전히 이름을 지명하면 애니메이션이 되살아나며,
        // 지명하지 않으면 아무 일도 일어나지 않는다. 비용은 삭제 한 번당 리스타일 한 번이다.
        let rooted_nodes = self.rooted_nodes.borrow();
        sets.retain(|key, state| {
            let keep = !state.is_empty();
            if !keep {
                log::warn!(
                    "ANIMSETDROP node={} pseudo={:?}",
                    key.node.0,
                    key.pseudo_element
                );
                if let Some(node) = rooted_nodes.get(&NoTrace(key.node)) {
                    node.dirty(NodeDamage::Style);
                }
            }
            keep
        });
        drop(rooted_nodes);
        let have_running_animations = sets.values().any(|state| state.needs_animation_ticks());

        self.update_running_animations_presence(window, have_running_animations);
    }

    fn update_running_animations_presence(&self, window: &Window, new_value: bool) {
        let had_running_animations = self.has_running_animations.get();
        if new_value == had_running_animations {
            return;
        }

        self.has_running_animations.set(new_value);
        self.handle_animation_presence_or_pending_events_change(window);
    }

    fn handle_animation_presence_or_pending_events_change(&self, window: &Window) {
        let has_running_animations = self.has_running_animations.get();
        let has_pending_events = !self.pending_events.borrow().is_empty();

        // Do not send the NoAnimationCallbacksPresent state until all pending
        // animation events are delivered.
        let state = match has_running_animations || has_pending_events {
            true => AnimationsPresentState::AnimationsPresent,
            false => AnimationsPresentState::NoAnimationsPresent,
        };
        window.send_to_constellation(ScriptToConstellationMessage::ChangeRunningAnimationsState(
            state,
        ));
    }

    pub(crate) fn running_animation_count(&self) -> usize {
        self.sets
            .sets
            .read()
            .values()
            .map(|state| state.running_animation_and_transition_count())
            .sum()
    }

    /// Walk through the list of pending animations and start all of the ones that
    /// have left the delay phase.
    fn start_pending_animations(
        &self,
        key: &AnimationSetKey,
        set: &mut ElementAnimationSet,
        now: f64,
        pipeline_id: PipelineId,
    ) {
        for animation in set.animations.iter_mut() {
            if animation.state != AnimationState::Pending {
                continue;
            }
            // ***이 한 줄만이 Pending 을 Running 으로 올린다.***
            //
            // `Animation::update_from_other` 는 리스타일마다 상태를 Pending 으로
            // 되돌려 놓고, 그 함수의 마지막 승급은 `old_state != Pending` 일 때만 돈다 --
            // 즉 이미 Pending 인 것은 리스타일로는 영영 못 벗어난다. 그래서 여기가
            // 유일한 출구이고, 여기서 못 올리면 애니메이션은 첫 키프레임에 주차된다.
            // log_ani_debug/05 의 sd-anim-85 가 9초 동안 그랬고, 그 첫 키프레임이
            // translateX(11520) -- 뷰포트 폭 한 장 -- 이라 그동안 벽이 검었다.
            if animation.started_at > now {
                log::warn!(
                    "ANIMPENDING name={} stuck_by_ms={:.1} started_at={:.3} now={:.3} delay={:.3}",
                    animation.name,
                    (animation.started_at - now) * 1000.0,
                    animation.started_at,
                    now,
                    animation.delay
                );
                continue;
            }
            animation.state = AnimationState::Running;
            log::warn!(
                "ANIMSTART name={} node={} started_at={:.3} now={:.3}",
                animation.name,
                key.node.0,
                animation.started_at,
                now
            );
            self.add_animation_event(
                key,
                animation,
                TransitionOrAnimationEventType::AnimationStart,
                now,
                pipeline_id,
            );
        }

        for transition in set.transitions.iter_mut() {
            if transition.state == AnimationState::Pending && transition.start_time <= now {
                transition.state = AnimationState::Running;
                self.add_transition_event(
                    key,
                    transition,
                    TransitionOrAnimationEventType::TransitionStart,
                    now,
                    pipeline_id,
                );
            }
        }
    }

    /// Walk through the list of running animations and remove all of the ones that
    /// have ended.
    fn finish_running_animations(
        &self,
        key: &AnimationSetKey,
        set: &mut ElementAnimationSet,
        now: f64,
        pipeline_id: PipelineId,
    ) {
        for animation in set.animations.iter_mut() {
            if animation.state == AnimationState::Running && animation.has_ended(now) {
                animation.state = AnimationState::Finished;
                self.add_animation_event(
                    key,
                    animation,
                    TransitionOrAnimationEventType::AnimationEnd,
                    now,
                    pipeline_id,
                );
            }
        }

        for transition in set.transitions.iter_mut() {
            if transition.state == AnimationState::Running && transition.has_ended(now) {
                transition.state = AnimationState::Finished;
                self.add_transition_event(
                    key,
                    transition,
                    TransitionOrAnimationEventType::TransitionEnd,
                    now,
                    pipeline_id,
                );
            }
        }
    }

    /// Send events for canceled animations. Currently this only handles canceled
    /// transitions, but eventually this should handle canceled CSS animations as
    /// well.
    fn handle_canceled_animations(
        &self,
        key: &AnimationSetKey,
        set: &mut ElementAnimationSet,
        now: f64,
        pipeline_id: PipelineId,
    ) {
        for transition in &set.transitions {
            if transition.state == AnimationState::Canceled {
                self.add_transition_event(
                    key,
                    transition,
                    TransitionOrAnimationEventType::TransitionCancel,
                    now,
                    pipeline_id,
                );
            }
        }

        for animation in &set.animations {
            if animation.state == AnimationState::Canceled {
                self.add_animation_event(
                    key,
                    animation,
                    TransitionOrAnimationEventType::AnimationCancel,
                    now,
                    pipeline_id,
                );
            }
        }

        set.clear_canceled_animations();
    }

    fn handle_new_animations(
        &self,
        key: &AnimationSetKey,
        set: &mut ElementAnimationSet,
        now: f64,
        pipeline_id: PipelineId,
    ) {
        for animation in set.animations.iter_mut() {
            animation.is_new = false;
        }

        for transition in set.transitions.iter_mut() {
            if transition.is_new {
                self.add_transition_event(
                    key,
                    transition,
                    TransitionOrAnimationEventType::TransitionRun,
                    now,
                    pipeline_id,
                );
                transition.is_new = false;
            }
        }
    }

    /// Ensure that all nodes with new animations are rooted. This should be called
    /// immediately after a restyle, to ensure that these addresses are still valid.
    #[expect(unsafe_code)]
    fn root_newly_animating_dom_nodes(
        &self,
        sets: &FxHashMap<AnimationSetKey, ElementAnimationSet>,
    ) {
        let mut rooted_nodes = self.rooted_nodes.borrow_mut();
        for (key, set) in sets.iter() {
            let opaque_node = key.node;
            if rooted_nodes.contains_key(&NoTrace(opaque_node)) {
                continue;
            }

            if set.animations.iter().any(|animation| animation.is_new) ||
                set.transitions.iter().any(|transition| transition.is_new)
            {
                let address = UntrustedNodeAddress(opaque_node.0 as *const c_void);
                unsafe {
                    rooted_nodes.insert(
                        NoTrace(opaque_node),
                        Dom::from_ref(&*from_untrusted_node_address(address)),
                    )
                };
            }
        }
    }

    // Unroot any nodes that we have rooted but are no longer tracking animations for.
    fn unroot_unused_nodes(&self, sets: &FxHashMap<AnimationSetKey, ElementAnimationSet>) {
        let pending_events = self.pending_events.borrow();
        let nodes: FxHashSet<OpaqueNode> = sets.keys().map(|key| key.node).collect();
        self.rooted_nodes.borrow_mut().retain(|node, _| {
            nodes.contains(&node.0) || pending_events.iter().any(|event| event.node == node.0)
        });
    }

    fn add_transition_event(
        &self,
        key: &AnimationSetKey,
        transition: &Transition,
        event_type: TransitionOrAnimationEventType,
        now: f64,
        pipeline_id: PipelineId,
    ) {
        // Calculate the `elapsed-time` property of the event and take the absolute
        // value to prevent -0 values.
        let elapsed_time = match event_type {
            TransitionOrAnimationEventType::TransitionRun |
            TransitionOrAnimationEventType::TransitionStart => transition
                .property_animation
                .duration
                .min((-transition.delay).max(0.)),
            TransitionOrAnimationEventType::TransitionEnd => transition.property_animation.duration,
            TransitionOrAnimationEventType::TransitionCancel => {
                (now - transition.start_time).max(0.)
            },
            _ => unreachable!(),
        }
        .abs();

        self.pending_events
            .borrow_mut()
            .push(TransitionOrAnimationEvent {
                pipeline_id,
                event_type,
                node: key.node,
                pseudo_element: key.pseudo_element,
                property_or_animation_name: transition
                    .property_animation
                    .property_id()
                    .name()
                    .into(),
                elapsed_time,
            });
    }

    fn add_animation_event(
        &self,
        key: &AnimationSetKey,
        animation: &Animation,
        event_type: TransitionOrAnimationEventType,
        now: f64,
        pipeline_id: PipelineId,
    ) {
        // ***스크립트 애니메이션은 CSS 애니메이션 이벤트를 쏘지 않는다.***
        // `animationstart`/`animationend` 의 `animationName` 은 합성 이름이라
        // 페이지에 의미가 없고, 폴백 경로가 그 이벤트를 듣고 있다면 오작동한다.
        // 시작은 `AnimationState::Running` 으로 만들어 승급 경로를 피하고, 끝과
        // 취소는 여기서 끊는다. 네 종류(`AnimationStart`/`Iteration`/`End`/
        // `Cancel`)가 전부 이 한 자리를 지난다.
        if animation.origin == AnimationOrigin::Script {
            return;
        }

        let iteration_index = match animation.iteration_state {
            KeyframesIterationState::Finite(current, _) |
            KeyframesIterationState::Infinite(current) => current,
        };

        let active_duration = match animation.iteration_state {
            KeyframesIterationState::Finite(_, max) => max * animation.duration,
            KeyframesIterationState::Infinite(_) => f64::MAX,
        };

        // Calculate the `elapsed-time` property of the event and take the absolute
        // value to prevent -0 values.
        let elapsed_time = match event_type {
            TransitionOrAnimationEventType::AnimationStart => {
                (-animation.delay).max(0.).min(active_duration)
            },
            TransitionOrAnimationEventType::AnimationIteration => {
                iteration_index * animation.duration
            },
            TransitionOrAnimationEventType::AnimationEnd => {
                (iteration_index * animation.duration) + animation.current_iteration_duration()
            },
            TransitionOrAnimationEventType::AnimationCancel => {
                (iteration_index * animation.duration) + (now - animation.started_at).max(0.)
            },
            _ => unreachable!(),
        }
        .abs();

        self.pending_events
            .borrow_mut()
            .push(TransitionOrAnimationEvent {
                pipeline_id,
                event_type,
                node: key.node,
                pseudo_element: key.pseudo_element,
                property_or_animation_name: animation.name.to_string(),
                elapsed_time,
            });
    }

    /// An implementation of the final steps of
    /// <https://drafts.csswg.org/web-animations-1/#update-animations-and-send-events>.
    pub(crate) fn send_pending_events(&self, window: &Window, cx: &mut js::context::JSContext) {
        // > 4. Let events to dispatch be a copy of doc’s pending animation event queue.
        // > 5. Clear doc’s pending animation event queue.
        //
        // Take all of the events here, in case sending one of these events
        // triggers adding new events by forcing a layout.
        let events = std::mem::take(&mut *self.pending_events.borrow_mut());
        if events.is_empty() {
            return;
        }

        // > 6. Perform a stable sort of the animation events in events to dispatch as follows:
        // >    1. Sort the events by their scheduled event time such that events that were
        // >       scheduled to occur earlier sort before events scheduled to occur later, and
        // >       events whose scheduled event time is unresolved sort before events with a
        // >       resolved scheduled event time.
        // >    2. Within events with equal scheduled event times, sort by their composite
        // >       order.
        //
        // TODO: Sorting of animation events isn't done yet.

        // 7. Dispatch each of the events in events to dispatch at their corresponding
        // target using the order established in the previous step.
        for event in events.into_iter() {
            // We root the node here to ensure that sending this event doesn't
            // unroot it as a side-effect.
            let node = match self.rooted_nodes.borrow().get(&NoTrace(event.node)) {
                Some(node) => DomRoot::from_ref(&**node),
                None => {
                    warn!("Tried to send an event for an unrooted node");
                    continue;
                },
            };

            let event_atom = match event.event_type {
                TransitionOrAnimationEventType::AnimationEnd => atom!("animationend"),
                TransitionOrAnimationEventType::AnimationStart => atom!("animationstart"),
                TransitionOrAnimationEventType::AnimationCancel => atom!("animationcancel"),
                TransitionOrAnimationEventType::AnimationIteration => atom!("animationiteration"),
                TransitionOrAnimationEventType::TransitionCancel => atom!("transitioncancel"),
                TransitionOrAnimationEventType::TransitionEnd => atom!("transitionend"),
                TransitionOrAnimationEventType::TransitionRun => atom!("transitionrun"),
                TransitionOrAnimationEventType::TransitionStart => atom!("transitionstart"),
            };
            let parent = EventInit {
                bubbles: true,
                cancelable: false,
                composed: false,
            };

            let property_or_animation_name =
                DOMString::from(event.property_or_animation_name.clone());
            let pseudo_element = event
                .pseudo_element
                .map_or_else(DOMString::new, |pseudo_element| {
                    DOMString::from(pseudo_element.to_css_string())
                });
            let elapsed_time = Finite::new(event.elapsed_time as f32).unwrap();
            let window = node.owner_window();

            if event.event_type.is_transition_event() {
                let event_init = TransitionEventInit {
                    parent,
                    propertyName: property_or_animation_name,
                    elapsedTime: elapsed_time,
                    pseudoElement: pseudo_element,
                };
                TransitionEvent::new(&window, event_atom, &event_init, CanGc::from_cx(cx))
                    .upcast::<Event>()
                    .fire(cx, node.upcast());
            } else {
                let event_init = AnimationEventInit {
                    parent,
                    animationName: property_or_animation_name,
                    elapsedTime: elapsed_time,
                    pseudoElement: pseudo_element,
                };
                AnimationEvent::new(&window, event_atom, &event_init, CanGc::from_cx(cx))
                    .upcast::<Event>()
                    .fire(cx, node.upcast());
            }
        }

        if self.pending_events.borrow().is_empty() {
            self.handle_animation_presence_or_pending_events_change(window);
        }
    }
}

/// The type of transition event to trigger. These are defined by
/// CSS Transitions § 6.1 and CSS Animations § 4.2
#[derive(Clone, Debug, Deserialize, JSTraceable, MallocSizeOf, Serialize)]
pub(crate) enum TransitionOrAnimationEventType {
    /// "The transitionrun event occurs when a transition is created (i.e., when it
    /// is added to the set of running transitions)."
    TransitionRun,
    /// "The transitionstart event occurs when a transition’s delay phase ends."
    TransitionStart,
    /// "The transitionend event occurs at the completion of the transition. In the
    /// case where a transition is removed before completion, such as if the
    /// transition-property is removed, then the event will not fire."
    TransitionEnd,
    /// "The transitioncancel event occurs when a transition is canceled."
    TransitionCancel,
    /// "The animationstart event occurs at the start of the animation. If there is
    /// an animation-delay then this event will fire once the delay period has expired."
    AnimationStart,
    /// "The animationiteration event occurs at the end of each iteration of an
    /// animation, except when an animationend event would fire at the same time."
    AnimationIteration,
    /// "The animationend event occurs when the animation finishes"
    AnimationEnd,
    /// "The animationcancel event occurs when the animation stops running in a way
    /// that does not fire an animationend event..."
    AnimationCancel,
}

impl TransitionOrAnimationEventType {
    /// Whether or not this event is a transition-related event.
    pub(crate) fn is_transition_event(&self) -> bool {
        match *self {
            Self::TransitionRun |
            Self::TransitionEnd |
            Self::TransitionCancel |
            Self::TransitionStart => true,
            Self::AnimationEnd |
            Self::AnimationIteration |
            Self::AnimationStart |
            Self::AnimationCancel => false,
        }
    }
}

#[derive(Deserialize, JSTraceable, MallocSizeOf, Serialize)]
/// A transition or animation event.
pub(crate) struct TransitionOrAnimationEvent {
    /// The pipeline id of the layout task that sent this message.
    #[no_trace]
    pub(crate) pipeline_id: PipelineId,
    /// The type of transition event this should trigger.
    pub(crate) event_type: TransitionOrAnimationEventType,
    /// The address of the node which owns this transition.
    #[no_trace]
    pub(crate) node: OpaqueNode,
    /// The pseudo element for this transition or animation, if applicable.
    #[no_trace]
    pub(crate) pseudo_element: Option<PseudoElement>,
    /// The name of the property that is transitioning (in the case of a transition)
    /// or the name of the animation (in the case of an animation).
    pub(crate) property_or_animation_name: String,
    /// The elapsed time property to send with this transition event.
    pub(crate) elapsed_time: f64,
}

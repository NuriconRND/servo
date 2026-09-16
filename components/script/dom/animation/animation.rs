/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use style::animation::AnimationSetKey;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::AnimationBinding::AnimationMethods;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::node::{Node, NodeDamage, NodeTraits};
use crate::dom::window::Window;
use crate::script_runtime::CanGc;

/// <https://drafts.csswg.org/web-animations-1/#the-animation-interface>
///
/// 대상 요소와 합성 이름만 들고 있는 얇은 핸들이다. 상태는 전부
/// `ElementAnimationSet` 에 있고, 이 객체는 그것을 찾아가는 열쇠일 뿐이다.
#[dom_struct]
pub(crate) struct Animation {
    reflector_: Reflector,

    /// 애니메이션이 붙은 노드.
    target: Dom<Node>,

    /// `-servo-script-<N>`. 페이지의 `@keyframes` 이름과 충돌할 수 없다.
    #[no_trace]
    name: Atom,
}

impl Animation {
    fn new_inherited(target: &Node, name: Atom) -> Animation {
        Animation {
            reflector_: Reflector::new(),
            target: Dom::from_ref(target),
            name,
        }
    }

    pub(crate) fn new(
        window: &Window,
        target: &Node,
        name: Atom,
        can_gc: CanGc,
    ) -> DomRoot<Animation> {
        reflect_dom_object(
            Box::new(Animation::new_inherited(target, name)),
            window,
            can_gc,
        )
    }
}

impl AnimationMethods<crate::DomTypeHolder> for Animation {
    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-cancel>
    ///
    /// 마퀴가 ResizeObserver 재시작마다 부르므로 반복 호출과 이미 끝난 애니메이션에
    /// 대한 호출이 흔하다. 둘 다 터지지 않아야 한다.
    fn Cancel(&self) {
        let target = DomRoot::from_ref(&*self.target);
        let key = AnimationSetKey::new_for_non_pseudo(target.to_opaque());
        let document = target.owner_document();
        if document
            .animations()
            .cancel_script_animation(&key, &self.name)
        {
            target.dirty(NodeDamage::Style);
        }
    }
}

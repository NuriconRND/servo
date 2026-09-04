/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use script_bindings::reflector::reflect_dom_object_with_cx;
use servo_media::streams::MediaStreamType;
use servo_media::streams::registry::{MediaStreamId, unregister_stream};

use crate::dom::bindings::codegen::Bindings::MediaStreamTrackBinding::MediaStreamTrackMethods;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;

#[dom_struct]
pub(crate) struct MediaStreamTrack {
    eventtarget: EventTarget,
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    id: MediaStreamId,
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    ty: MediaStreamType,
    /// <https://w3c.github.io/mediacapture-main/#track-ended>
    ended: Cell<bool>,
}

impl MediaStreamTrack {
    pub(crate) fn new_inherited(id: MediaStreamId, ty: MediaStreamType) -> MediaStreamTrack {
        MediaStreamTrack {
            eventtarget: EventTarget::new_inherited(),
            id,
            ty,
            ended: Cell::new(false),
        }
    }

    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        global: &GlobalScope,
        id: MediaStreamId,
        ty: MediaStreamType,
    ) -> DomRoot<MediaStreamTrack> {
        reflect_dom_object_with_cx(
            Box::new(MediaStreamTrack::new_inherited(id, ty)),
            global,
            cx,
        )
    }

    pub(crate) fn id(&self) -> MediaStreamId {
        self.id
    }

    pub(crate) fn ty(&self) -> MediaStreamType {
        self.ty
    }
}

impl MediaStreamTrackMethods<crate::DomTypeHolder> for MediaStreamTrack {
    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-kind>
    fn Kind(&self) -> DOMString {
        match self.ty {
            MediaStreamType::Video => "video".into(),
            MediaStreamType::Audio => "audio".into(),
        }
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-id>
    fn Id(&self) -> DOMString {
        self.id.id().to_string().into()
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-clone>
    fn Clone(&self, cx: &mut js::context::JSContext) -> DomRoot<MediaStreamTrack> {
        MediaStreamTrack::new(cx, &self.global(), self.id, self.ty)
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack-stop>
    ///
    /// 주의: 이 엔진의 `clone()` 은 같은 `MediaStreamId` 를 공유하므로(위 `Clone`
    /// 참조) 클론 하나를 stop 하면 원본까지 멈춘다. 스펙 위반이지만 트랙별 독립
    /// 스트림 복제가 필요해 별건으로 둔다.
    fn Stop(&self) {
        if self.ended.get() {
            return;
        }
        self.ended.set(true);
        unregister_stream(&self.id);
    }
}

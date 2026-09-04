/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::rc::Rc;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::reflector::reflect_dom_object;
use servo_config::pref;
use servo_media::ServoMedia;
use servo_media::streams::MediaStreamType;
use servo_media::streams::capture::{
    Constrain, ConstrainRange, ConstrainString, DisplayCaptureSource, MediaTrackConstraintSet,
};

use crate::conversions::Convert;
use crate::dom::bindings::codegen::Bindings::MediaDevicesBinding::{
    DisplayMediaStreamConstraints, MediaDevicesMethods, MediaStreamConstraints,
};
use crate::dom::bindings::codegen::UnionTypes::{
    BooleanOrMediaTrackConstraints, ClampedUnsignedLongOrConstrainULongRange as ConstrainULong,
    DoubleOrConstrainDoubleRange as ConstrainDouble, StringOrStringSequence,
    StringOrStringSequenceOrConstrainDOMStringParameters as ConstrainDOMString,
};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::media::mediadeviceinfo::MediaDeviceInfo;
use crate::dom::media::mediastream::MediaStream;
use crate::dom::media::mediastreamtrack::MediaStreamTrack;
use crate::dom::promise::Promise;
use crate::script_runtime::CanGc;

#[dom_struct]
pub(crate) struct MediaDevices {
    eventtarget: EventTarget,
}

impl MediaDevices {
    pub(crate) fn new_inherited() -> MediaDevices {
        MediaDevices {
            eventtarget: EventTarget::new_inherited(),
        }
    }

    pub(crate) fn new(global: &GlobalScope, can_gc: CanGc) -> DomRoot<MediaDevices> {
        reflect_dom_object(Box::new(MediaDevices::new_inherited()), global, can_gc)
    }
}

impl MediaDevicesMethods<crate::DomTypeHolder> for MediaDevices {
    /// <https://w3c.github.io/mediacapture-main/#dom-mediadevices-getusermedia>
    fn GetUserMedia(
        &self,
        cx: &mut CurrentRealm,
        constraints: &MediaStreamConstraints,
    ) -> Rc<Promise> {
        let p = Promise::new_in_realm(cx);
        let media = ServoMedia::get();
        media.set_capture_mocking(pref!(media_capture_mocking_enabled));
        let stream = MediaStream::new(cx, &self.global());
        if let Some(constraints) = convert_constraints(&constraints.audio)
            && let Some(audio) = media.create_audioinput_stream(constraints)
        {
            self.global().track_capture_stream(audio);
            let track = MediaStreamTrack::new(cx, &self.global(), audio, MediaStreamType::Audio);
            stream.add_track(&track);
        }
        if let Some(constraints) = convert_constraints(&constraints.video)
            && let Some(video) = media.create_videoinput_stream(constraints)
        {
            self.global().track_capture_stream(video);
            let track = MediaStreamTrack::new(cx, &self.global(), video, MediaStreamType::Video);
            stream.add_track(&track);
        }

        p.resolve_native_with_cx(cx, &stream);
        p
    }

    /// <https://w3c.github.io/mediacapture-screen-share/#dom-mediadevices-getdisplaymedia>
    fn GetDisplayMedia(
        &self,
        cx: &mut CurrentRealm,
        constraints: &DisplayMediaStreamConstraints,
    ) -> Rc<Promise> {
        let p = Promise::new_in_realm(cx);
        let media = ServoMedia::get();
        media.set_capture_mocking(pref!(media_capture_mocking_enabled));
        let stream = MediaStream::new(cx, &self.global());
        // v1: no source picker. The captured screen/window is selected from
        // preferences and the same fixed source is returned every call.
        if let Some(video_constraints) = convert_constraints(&constraints.video) {
            let window_title = pref!(media_screen_capture_window_title);
            let source = DisplayCaptureSource {
                monitor_index: pref!(media_screen_capture_monitor_index) as i32,
                window_title: (!window_title.is_empty()).then_some(window_title),
                show_cursor: pref!(media_screen_capture_show_cursor),
            };
            if let Some(video) = media.create_display_stream(source, video_constraints) {
                self.global().track_capture_stream(video);
                let track =
                    MediaStreamTrack::new(cx, &self.global(), video, MediaStreamType::Video);
                stream.add_track(&track);
            }
        }

        p.resolve_native_with_cx(cx, &stream);
        p
    }

    /// <https://w3c.github.io/mediacapture-main/#dom-mediadevices-enumeratedevices>
    fn EnumerateDevices(&self, cx: &mut JSContext) -> Rc<Promise> {
        // Step 1.
        let mut realm = CurrentRealm::assert(cx);
        let p = Promise::new_in_realm(&mut realm);

        // Step 2.
        // XXX These steps should be run in parallel.
        // XXX Steps 2.1 - 2.4

        // Step 2.5
        let media = ServoMedia::get();
        let device_monitor = media.get_device_monitor();
        let result_list = device_monitor
            .enumerate_devices()
            .map(|devices| {
                devices
                    .iter()
                    .map(|device| {
                        // XXX The media backend has no way to group devices yet.
                        MediaDeviceInfo::new(
                            cx,
                            &self.global(),
                            &device.device_id,
                            device.kind.convert(),
                            &device.label,
                            "",
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        p.resolve_native_with_cx(cx, &result_list);

        // Step 3.
        p
    }
}

fn convert_constraints(js: &BooleanOrMediaTrackConstraints) -> Option<MediaTrackConstraintSet> {
    match js {
        BooleanOrMediaTrackConstraints::Boolean(false) => None,
        BooleanOrMediaTrackConstraints::Boolean(true) => Some(Default::default()),
        BooleanOrMediaTrackConstraints::MediaTrackConstraints(c) => Some(MediaTrackConstraintSet {
            height: c.parent.height.as_ref().and_then(convert_culong),
            width: c.parent.width.as_ref().and_then(convert_culong),
            aspect: c.parent.aspectRatio.as_ref().and_then(convert_cdouble),
            frame_rate: c.parent.frameRate.as_ref().and_then(convert_cdouble),
            sample_rate: c.parent.sampleRate.as_ref().and_then(convert_culong),
            device_id: c
                .parent
                .deviceId
                .as_ref()
                .and_then(convert_string_constraint),
        }),
    }
}

fn convert_culong(js: &ConstrainULong) -> Option<Constrain<u32>> {
    match js {
        ConstrainULong::ClampedUnsignedLong(val) => Some(Constrain::Value(*val)),
        ConstrainULong::ConstrainULongRange(range) => {
            if range.parent.min.is_some() || range.parent.max.is_some() {
                Some(Constrain::Range(ConstrainRange {
                    min: range.parent.min,
                    max: range.parent.max,
                    ideal: range.ideal,
                }))
            } else {
                range.exact.map(Constrain::Value)
            }
        },
    }
}

fn convert_cdouble(js: &ConstrainDouble) -> Option<Constrain<f64>> {
    match js {
        ConstrainDouble::Double(val) => Some(Constrain::Value(**val)),
        ConstrainDouble::ConstrainDoubleRange(range) => {
            if range.parent.min.is_some() || range.parent.max.is_some() {
                Some(Constrain::Range(ConstrainRange {
                    min: range.parent.min.map(|x| *x),
                    max: range.parent.max.map(|x| *x),
                    ideal: range.ideal.map(|x| *x),
                }))
            } else {
                range.exact.map(|exact| Constrain::Value(*exact))
            }
        },
    }
}

/// Web pages commonly pass `deviceId: saved || ""` to mean "no preference",
/// so an empty string must behave like an absent deviceId (legacy
/// first-device selection) rather than a literal id that fails to match any
/// device. Every extraction path below filters out empty strings before
/// they become a `Constrain::Value`/`ConstrainString`.
fn convert_string_constraint(js: &ConstrainDOMString) -> Option<ConstrainString> {
    fn first(value: &StringOrStringSequence) -> Option<String> {
        match value {
            StringOrStringSequence::String(s) => {
                let s = s.to_string();
                (!s.is_empty()).then_some(s)
            },
            StringOrStringSequence::StringSequence(seq) => {
                seq.first().map(|s| s.to_string()).filter(|s| !s.is_empty())
            },
        }
    }
    match js {
        ConstrainDOMString::String(s) => {
            let s = s.to_string();
            (!s.is_empty()).then_some(ConstrainString::Ideal(s))
        },
        ConstrainDOMString::StringSequence(seq) => seq
            .first()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .map(ConstrainString::Ideal),
        ConstrainDOMString::ConstrainDOMStringParameters(params) => {
            if let Some(exact) = params.exact.as_ref().and_then(first) {
                Some(ConstrainString::Exact(exact))
            } else {
                params
                    .ideal
                    .as_ref()
                    .and_then(first)
                    .map(ConstrainString::Ideal)
            }
        },
    }
}

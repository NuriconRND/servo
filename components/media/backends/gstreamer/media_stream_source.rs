/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use glib::subclass::prelude::*;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_base::UniqueFlowCombiner;
use servo_media_player::PlayerError;
use servo_media_streams::{MediaStream, MediaStreamType};
use url::Url;

use crate::media_stream::GStreamerMediaStream;

// Implementation sub-module of the GObject
mod imp {

    use super::*;

    static AUDIO_SRC_PAD_TEMPLATE: LazyLock<gstreamer::PadTemplate> = LazyLock::new(|| {
        // raw 오디오를 그대로 흘려 playbin3/decodebin3가 디코더를 끼우지 않게 한다.
        let caps = gstreamer::Caps::builder("audio/x-raw").build();
        gstreamer::PadTemplate::new(
            "audio_src",
            gstreamer::PadDirection::Src,
            gstreamer::PadPresence::Sometimes,
            &caps,
        )
        .expect("Could not create audio src pad template")
    });

    static VIDEO_SRC_PAD_TEMPLATE: LazyLock<gstreamer::PadTemplate> = LazyLock::new(|| {
        // raw 비디오를 그대로 흘려 playbin3/decodebin3가 디코더를 끼우지 않게 한다.
        // 포맷을 I420으로 고정해야 프록시 경계 너머로 caps가 협상된다 — bare
        // `video/x-raw`(포맷 미지정)는 metadata 0x0로 실패한다(검증된 함정).
        let caps = gstreamer::Caps::builder("video/x-raw")
            .field("format", "I420")
            .build();
        gstreamer::PadTemplate::new(
            "video_src",
            gstreamer::PadDirection::Src,
            gstreamer::PadPresence::Sometimes,
            &caps,
        )
        .expect("Could not create video src pad template")
    });

    pub struct ServoMediaStreamSrc {
        cat: gstreamer::DebugCategory,
        audio_proxysrc: gstreamer::Element,
        audio_srcpad: gstreamer::GhostPad,
        video_proxysrc: gstreamer::Element,
        video_srcpad: gstreamer::GhostPad,
        flow_combiner: Arc<Mutex<UniqueFlowCombiner>>,
        has_audio_stream: Arc<AtomicBool>,
        has_video_stream: Arc<AtomicBool>,
        /// `set_stream` 이 **남의 파이프라인**(MediaStream 쪽)에 심어 둔 proxysink 들.
        ///
        /// 이것을 기억해 두지 않으면 끊을 방법이 없다. 그리고 끊지 못하면 플레이어가 죽은
        /// 뒤에도 그 파이프라인은 계속 돌면서 이미 사라진 proxysrc 로 버퍼를 밀어 넣는다
        /// -- 캡처 표출을 끌 때 죽던 이유가 그것이다(`detach_streams` 주석).
        attached_sinks: Mutex<Vec<(gstreamer::Pipeline, gstreamer::Element)>>,
    }

    impl ServoMediaStreamSrc {
        pub fn set_stream(
            &self,
            stream: &mut GStreamerMediaStream,
            src: &gstreamer::Element,
            only_stream: bool,
        ) -> Result<(), PlayerError> {
            // XXXferjm the current design limits the number of streams to one
            // per type. This fulfills the basic use case for WebRTC, but we should
            // implement support for multiple streams per type at some point, which
            // likely involves encoding and muxing all streams of the same type
            // in a single stream.

            gstreamer::log!(self.cat, "Setting stream");

            // Append a proxysink to the media stream pipeline.
            let pipeline = stream.pipeline_or_new();
            // 표시 경로: 재인코딩(vp8enc/opusenc) 없이 raw tail을 그대로 proxysink로 흘린다.
            // (send 경로 webrtc.rs는 계속 encoded()를 사용 — 무관.)
            let last_element = stream.raw();
            let sink = gstreamer::ElementFactory::make("proxysink")
                .build()
                .map_err(|_| PlayerError::SetStreamFailed)?;
            pipeline
                .add(&sink)
                .map_err(|_| PlayerError::SetStreamFailed)?;
            gstreamer::Element::link_many(&[&last_element, &sink][..])
                .map_err(|_| PlayerError::SetStreamFailed)?;

            // Create the appropriate proxysrc depending on the stream type
            // and connect the media stream proxysink to it.
            self.setup_proxy_src(stream.ty(), &sink, src, only_stream);

            // 끊을 때 되찾을 수 있도록 기억해 둔다(`detach_streams`).
            self.attached_sinks
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push((pipeline.clone(), sink.clone()));

            sink.sync_state_with_parent()
                .map_err(|_| PlayerError::SetStreamFailed)?;
            pipeline
                .set_state(gstreamer::State::Playing)
                .map_err(|_| PlayerError::SetStreamFailed)?;

            Ok(())
        }

        /// 플레이어가 물러날 때, MediaStream 파이프라인에 심어 둔 proxysink 를 걷어낸다.
        ///
        /// ★이 함수가 없어서 캡처 표출을 끌 때 죽었다★
        ///
        /// `set_stream` 은 **남의 파이프라인**(MediaStream 이 소유한다)에 proxysink 를 넣고
        /// 그것을 이 플레이어의 proxysrc 에 물린다. 두 파이프라인은 그 지점에서만 이어져
        /// 있고, 수명은 서로 모른다.
        ///
        /// 실측(log_presentation/05, 22:08:43~45):
        ///
        /// ```text
        /// 22:08:43  MEDIATEARDOWN player drop id=1/2/3 stream_type=Stream
        /// 22:08:45  MEDIATEARDOWN stream drop ... has_consumer=true   (2 초 뒤)
        /// -> 0xc0000005 in gstreamer-1.0-0.dll +0x1c79a
        /// ```
        ///
        /// 플레이어가 먼저 죽어 proxysrc 가 사라지는데, MediaStream 파이프라인은 그대로
        /// PLAYING 이라 캡처 허브가 계속 밀어 넣는다. 그 2 초 동안 proxysink 는 이미 없는
        /// 짝에게 버퍼를 넘긴다. `media_release_detached_player` 를 끄면 사라지는 이유도
        /// 이것이다 -- 그 전에는 플레이어가 GC 전까지 살아 있어 이 창이 열리지 않았다.
        ///
        /// 그래서 물러나기 전에 **먼저** 끊는다: sink 를 NULL 로 내리고, 링크를 풀고,
        /// 파이프라인에서 뺀다. MediaStream 자체는 건드리지 않는다 -- 그 스트림은 다시
        /// 표출되거나 WebRTC 로 나갈 수 있고, 그건 이 플레이어가 정할 일이 아니다.
        pub fn detach_streams(&self) {
            let attached = std::mem::take(
                &mut *self
                    .attached_sinks
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()),
            );
            if attached.is_empty() {
                return;
            }
            // 실제로 끊었는지 다음 실행의 로그로 확인한다 -- 이 줄이 player drop 앞에
            // 나와야 순서가 맞다.
            log::warn!(
                "MEDIATEARDOWN detaching {} proxysink(s) from stream pipelines",
                attached.len()
            );
            for (pipeline, sink) in attached {
                // 먼저 세운다 -- 내리기 전에 링크를 풀면 흐르던 버퍼가 갈 곳을 잃는다.
                let _ = sink.set_state(gstreamer::State::Null);
                if let Some(pad) = sink.static_pad("sink")
                    && let Some(peer) = pad.peer()
                {
                    let _ = peer.unlink(&pad);
                }
                if let Err(error) = pipeline.remove(&sink) {
                    gstreamer::warning!(
                        self.cat,
                        "could not remove the proxysink from the stream pipeline: {error}"
                    );
                }
            }
        }

        fn setup_proxy_src(
            &self,
            stream_type: MediaStreamType,
            sink: &gstreamer::Element,
            src: &gstreamer::Element,
            only_stream: bool,
        ) {
            let (proxysrc, src_pad, no_more_pads) = match stream_type {
                MediaStreamType::Audio => {
                    self.has_audio_stream.store(true, Ordering::Relaxed);
                    (
                        &self.audio_proxysrc,
                        &self.audio_srcpad,
                        self.has_video_stream.load(Ordering::Relaxed),
                    )
                },
                MediaStreamType::Video => {
                    self.has_video_stream.store(true, Ordering::Relaxed);
                    (
                        &self.video_proxysrc,
                        &self.video_srcpad,
                        self.has_audio_stream.load(Ordering::Relaxed),
                    )
                },
            };
            proxysrc.set_property("proxysink", sink);

            // Add proxysrc to bin
            let bin = src.downcast_ref::<gstreamer::Bin>().unwrap();
            bin.add(proxysrc)
                .expect("Could not add proxysrc element to bin");

            let target_pad = proxysrc
                .static_pad("src")
                .expect("Could not get proxysrc's static src pad");
            src_pad
                .set_target(Some(&target_pad))
                .expect("Could not set target pad");

            src.add_pad(src_pad)
                .expect("Could not add source pad to media stream src");
            src.set_element_flags(gstreamer::ElementFlags::SOURCE);

            let proxy_pad = src_pad.internal().unwrap();
            src_pad.set_active(true).expect("Could not active pad");
            self.flow_combiner.lock().unwrap().add_pad(&proxy_pad);

            src.sync_state_with_parent().unwrap();

            if no_more_pads || only_stream {
                src.no_more_pads();
            }
        }
    }

    // Basic declaration of our type for the GObject type system.
    #[glib::object_subclass]
    impl ObjectSubclass for ServoMediaStreamSrc {
        const NAME: &'static str = "ServoMediaStreamSrc";
        type Type = super::ServoMediaStreamSrc;
        type ParentType = gstreamer::Bin;
        type Interfaces = (gstreamer::URIHandler,);

        // Called once at the very beginning of instantiation of each instance and
        // creates the data structure that contains all our state
        fn with_class(_klass: &Self::Class) -> Self {
            let flow_combiner = Arc::new(Mutex::new(UniqueFlowCombiner::new()));

            fn create_ghost_pad_with_template(
                name: &str,
                pad_template: &gstreamer::PadTemplate,
                flow_combiner: Arc<Mutex<UniqueFlowCombiner>>,
            ) -> gstreamer::GhostPad {
                gstreamer::GhostPad::builder_from_template(pad_template)
                    .name(name)
                    .chain_function({
                        move |pad, parent, buffer| {
                            let chain_result =
                                gstreamer::ProxyPad::chain_default(pad, parent, buffer);
                            let result = flow_combiner
                                .lock()
                                .unwrap()
                                .update_pad_flow(pad, chain_result);
                            if result == Err(gstreamer::FlowError::Flushing) {
                                return chain_result;
                            }
                            result
                        }
                    })
                    .build()
            }

            let audio_proxysrc = gstreamer::ElementFactory::make("proxysrc")
                .build()
                .expect("Could not create proxysrc element");
            let audio_srcpad = create_ghost_pad_with_template(
                "audio_src",
                &AUDIO_SRC_PAD_TEMPLATE,
                flow_combiner.clone(),
            );

            let video_proxysrc = gstreamer::ElementFactory::make("proxysrc")
                .build()
                .expect("Could not create proxysrc element");
            let video_srcpad = create_ghost_pad_with_template(
                "video_src",
                &VIDEO_SRC_PAD_TEMPLATE,
                flow_combiner.clone(),
            );

            Self {
                cat: gstreamer::DebugCategory::new(
                    "servomediastreamsrc",
                    gstreamer::DebugColorFlags::empty(),
                    Some("Servo media stream source"),
                ),
                audio_proxysrc,
                audio_srcpad,
                video_proxysrc,
                video_srcpad,
                flow_combiner,
                has_video_stream: Arc::new(AtomicBool::new(false)),
                has_audio_stream: Arc::new(AtomicBool::new(false)),
                attached_sinks: Mutex::new(Vec::new()),
            }
        }
    }

    // The ObjectImpl trait provides the setters/getters for GObject properties.
    // Here we need to provide the values that are internally stored back to the
    // caller, or store whatever new value the caller is providing.
    //
    // This maps between the GObject properties and our internal storage of the
    // corresponding values of the properties.
    impl ObjectImpl for ServoMediaStreamSrc {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    // Let playbin3 know we are a live source.
                    glib::ParamSpecBoolean::builder("is-live")
                        .nick("Is Live")
                        .blurb("Let playbin3 know we are a live source")
                        .default_value(true)
                        .readwrite()
                        .build(),
                ]
            });

            &PROPERTIES
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "is-live" => true.to_value(),
                _ => unimplemented!(),
            }
        }
    }

    impl GstObjectImpl for ServoMediaStreamSrc {}

    // Implementation of gstreamer::Element virtual methods
    impl ElementImpl for ServoMediaStreamSrc {
        fn metadata() -> Option<&'static gstreamer::subclass::ElementMetadata> {
            static ELEMENT_METADATA: LazyLock<gstreamer::subclass::ElementMetadata> =
                LazyLock::new(|| {
                    gstreamer::subclass::ElementMetadata::new(
                        "Servo Media Stream Source",
                        "Source/Audio/Video",
                        "Feed player with media stream data",
                        "Servo developers",
                    )
                });

            Some(&*ELEMENT_METADATA)
        }

        fn pad_templates() -> &'static [gstreamer::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gstreamer::PadTemplate>> = LazyLock::new(|| {
                // Add pad templates for our audio and video source pads.
                // These are later used for actually creating the pads and beforehand
                // already provide information to GStreamer about all possible
                // pads that could exist for this type.
                vec![
                    AUDIO_SRC_PAD_TEMPLATE.clone(),
                    VIDEO_SRC_PAD_TEMPLATE.clone(),
                ]
            });

            PAD_TEMPLATES.as_ref()
        }
    }

    // Implementation of gstreamer::Bin virtual methods
    impl BinImpl for ServoMediaStreamSrc {}

    impl URIHandlerImpl for ServoMediaStreamSrc {
        const URI_TYPE: gstreamer::URIType = gstreamer::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["mediastream"]
        }

        fn uri(&self) -> Option<String> {
            Some("mediastream://".to_string())
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            if let Ok(uri) = Url::parse(uri)
                && uri.scheme() == "mediastream"
            {
                return Ok(());
            }
            Err(glib::Error::new(
                gstreamer::URIError::BadUri,
                format!("Invalid URI '{:?}'", uri,).as_str(),
            ))
        }
    }
}

// Public part of the ServoMediaStreamSrc type. This behaves like a normal
// GObject binding
glib::wrapper! {
    pub struct ServoMediaStreamSrc(ObjectSubclass<imp::ServoMediaStreamSrc>)
        @extends gstreamer::Bin, gstreamer::Element, gstreamer::Object, @implements gstreamer::URIHandler;
}

unsafe impl Send for ServoMediaStreamSrc {}
unsafe impl Sync for ServoMediaStreamSrc {}

impl ServoMediaStreamSrc {
    pub fn set_stream(
        &self,
        stream: &mut GStreamerMediaStream,
        only_stream: bool,
    ) -> Result<(), PlayerError> {
        self.imp()
            .set_stream(stream, self.upcast_ref::<gstreamer::Element>(), only_stream)
    }

    /// 이 소스가 남의 파이프라인에 심어 둔 proxysink 를 걷어낸다. 플레이어가 물러날 때
    /// **반드시** 부른다 -- 이유는 `imp::ServoMediaStreamSrc::detach_streams` 주석.
    pub fn detach_streams(&self) {
        self.imp().detach_streams();
    }
}

// Registers the type for our element, and then registers in GStreamer
// under the name "servomediastreamsrc" for being able to instantiate it via e.g.
// gstreamer::ElementFactory::make().
pub fn register_servo_media_stream_src() -> Result<(), glib::BoolError> {
    gstreamer::Element::register(
        None,
        "servomediastreamsrc",
        gstreamer::Rank::NONE,
        ServoMediaStreamSrc::static_type(),
    )
}

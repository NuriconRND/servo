/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::any::Any;
use std::sync::{Arc, LazyLock, Mutex};

use glib::BoolError;
use gstreamer;
use gstreamer::prelude::*;
use gstreamer_app::{AppSrc, AppSrcCallbacks, AppStreamType};
use servo_media_streams::registry::{
    MediaStreamId, get_stream, register_stream, unregister_stream,
};
use servo_media_streams::{MediaOutput, MediaSocket, MediaStream, MediaStreamType};

use super::BACKEND_BASE_TIME;
use crate::capture_hub::CaptureConsumer;

pub static RTP_CAPS_OPUS: LazyLock<gstreamer::Caps> = LazyLock::new(|| {
    gstreamer::Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("encoding-name", "OPUS")
        .build()
});

pub static RTP_CAPS_VP8: LazyLock<gstreamer::Caps> = LazyLock::new(|| {
    gstreamer::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "VP8")
        .build()
});

const MOCK_VIDEO_WIDTH: i32 = 640;
const MOCK_VIDEO_HEIGHT: i32 = 360;
const MOCK_VIDEO_FPS: i32 = 30;
const MOCK_VIDEO_FRAME_BYTES: usize =
    (MOCK_VIDEO_WIDTH as usize) * (MOCK_VIDEO_HEIGHT as usize) * 4;

fn fill_mock_video_frame(buffer: &mut gstreamer::BufferRef, frame_index: u64) {
    let mut map = buffer.map_writable().unwrap();
    let pixels = map.as_mut_slice();
    let marker_x = (frame_index as usize * 7) % MOCK_VIDEO_WIDTH as usize;

    for y in 0..MOCK_VIDEO_HEIGHT as usize {
        for x in 0..MOCK_VIDEO_WIDTH as usize {
            let offset = (y * MOCK_VIDEO_WIDTH as usize + x) * 4;
            let band = ((x / 80) + (y / 60) + frame_index as usize / 8) % 3;
            let marker = x.abs_diff(marker_x) < 8;
            let (r, g, b) = if marker {
                (255, 255, 255)
            } else if band == 0 {
                (32, 180, 255)
            } else if band == 1 {
                (255, 72, 112)
            } else {
                (78, 220, 140)
            };
            pixels[offset] = b;
            pixels[offset + 1] = g;
            pixels[offset + 2] = r;
            pixels[offset + 3] = 255;
        }
    }
}

pub struct GStreamerMediaStream {
    id: Option<MediaStreamId>,
    type_: MediaStreamType,
    elements: Vec<gstreamer::Element>,
    pipeline: Option<gstreamer::Pipeline>,
    /// 공유 캡처 허브에서의 등록. 캡처 장치가 먹이는 스트림에만 있다.
    /// 이걸 떨어뜨리면 이 스트림의 appsrc 만 허브 명단에서 빠진다 —
    /// 장치는 계속 열려 있다.
    capture_consumer: Option<CaptureConsumer>,
    /// `pipeline` 이 이 스트림 자신이 `pipeline_or_new` 에서 만들어 소유하는지(true),
    /// 아니면 `attach_to_pipeline` 으로 넘겨받은 남의 파이프라인의 복제본인지(false)를
    /// 구분한다. WebRTC 의 공유 파이프라인(webrtc.rs)이 대표적인 후자다 — 여러 트랙이
    /// 같은 파이프라인을 공유하므로, 그 경우 `Drop` 에서 이 플래그가 false 이면 하나의
    /// 트랙이 다른 트랙을 딸려 보내는 파이프라인을 NULL 로 끌고 가는 사고를 막는다.
    owns_pipeline: bool,
}

impl MediaStream for GStreamerMediaStream {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_mut_any(&mut self) -> &mut dyn Any {
        self
    }

    fn set_id(&mut self, id: MediaStreamId) {
        self.id = Some(id);
    }

    fn ty(&self) -> MediaStreamType {
        self.type_
    }
}

impl GStreamerMediaStream {
    pub fn new(type_: MediaStreamType, elements: Vec<gstreamer::Element>) -> Self {
        Self {
            id: None,
            type_,
            elements,
            pipeline: None,
            capture_consumer: None,
            owns_pipeline: false,
        }
    }

    pub fn caps(&self) -> &gstreamer::Caps {
        match self.type_ {
            MediaStreamType::Audio => &RTP_CAPS_OPUS,
            MediaStreamType::Video => &RTP_CAPS_VP8,
        }
    }

    pub fn caps_with_payload(&self, payload: i32) -> gstreamer::Caps {
        match self.type_ {
            MediaStreamType::Audio => gstreamer::Caps::builder("application/x-rtp")
                .field("media", "audio")
                .field("encoding-name", "OPUS")
                .field("payload", payload)
                .build(),
            MediaStreamType::Video => gstreamer::Caps::builder("application/x-rtp")
                .field("media", "video")
                .field("encoding-name", "VP8")
                .field("payload", payload)
                .build(),
        }
    }

    pub fn src_element(&self) -> gstreamer::Element {
        self.elements.last().unwrap().clone()
    }

    pub fn attach_to_pipeline(&mut self, pipeline: &gstreamer::Pipeline) {
        assert!(self.pipeline.is_none());
        let elements: Vec<_> = self.elements.iter().collect();
        pipeline.add_many(&elements[..]).unwrap();
        gstreamer::Element::link_many(&elements[..]).unwrap();
        for element in elements {
            element.sync_state_with_parent().unwrap();
        }
        self.pipeline = Some(pipeline.clone());
        // 남의 파이프라인을 넘겨받은 것이므로 이 스트림이 소유하지 않는다 —
        // Drop 에서 NULL 로 끌고 가면 안 된다.
        self.owns_pipeline = false;
    }

    pub fn pipeline_or_new(&mut self) -> gstreamer::Pipeline {
        match self.pipeline {
            Some(ref pipeline) => pipeline.clone(),
            _ => {
                let pipeline =
                    gstreamer::Pipeline::with_name("gstreamermediastream fresh pipeline");
                let clock = gstreamer::SystemClock::obtain();
                pipeline.set_start_time(gstreamer::ClockTime::NONE);
                pipeline.set_base_time(*BACKEND_BASE_TIME);
                pipeline.use_clock(Some(&clock));
                self.attach_to_pipeline(&pipeline);
                // 방금 이 스트림을 위해 새로 만든 파이프라인이므로 이 스트림이 소유한다 —
                // Drop 에서 다른 소유자 없이 NULL 로 끌고 가도 안전하다.
                self.owns_pipeline = true;
                pipeline
            },
        }
    }

    pub fn create_video() -> MediaStreamId {
        let appsrc = gstreamer::ElementFactory::make("appsrc")
            .property("is-live", true)
            .build()
            .map(|element| element.downcast::<AppSrc>().unwrap())
            .unwrap();
        appsrc.set_format(gstreamer::Format::Time);
        appsrc.set_stream_type(AppStreamType::Stream);
        appsrc.set_max_bytes((MOCK_VIDEO_WIDTH * MOCK_VIDEO_HEIGHT * 4 * 4) as u64);

        let caps = gstreamer::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .field("width", MOCK_VIDEO_WIDTH)
            .field("height", MOCK_VIDEO_HEIGHT)
            .field("framerate", gstreamer::Fraction::new(MOCK_VIDEO_FPS, 1))
            .build();
        appsrc.set_caps(Some(&caps));

        let frame_index = Arc::new(Mutex::new(0u64));
        let frame_index_for_callback = frame_index.clone();
        appsrc.set_callbacks(
            AppSrcCallbacks::builder()
                .need_data(move |appsrc, _| {
                    let mut frame_index = frame_index_for_callback.lock().unwrap();
                    let mut buffer = gstreamer::Buffer::with_size(MOCK_VIDEO_FRAME_BYTES).unwrap();
                    let duration_ns =
                        gstreamer::ClockTime::SECOND.nseconds() / MOCK_VIDEO_FPS as u64;
                    let pts = gstreamer::ClockTime::from_nseconds(*frame_index * duration_ns);
                    {
                        let buffer = buffer.get_mut().unwrap();
                        buffer.set_pts(Some(pts));
                        buffer.set_duration(gstreamer::ClockTime::from_nseconds(duration_ns));
                        fill_mock_video_frame(buffer, *frame_index);
                    }
                    *frame_index += 1;
                    let _ = appsrc.push_buffer(buffer);
                })
                .build(),
        );

        Self::create_video_from(appsrc.upcast())
    }

    /// Attaches encoding adapters to the stream, returning the source element when successful.
    pub fn encoded(&mut self) -> Result<gstreamer::Element, BoolError> {
        let pipeline = self
            .pipeline
            .as_ref()
            .expect("GStreamerMediaStream::encoded() should not be called without a pipeline");
        let src = self.src_element();

        let capsfilter = gstreamer::ElementFactory::make("capsfilter")
            .property("caps", self.caps())
            .build()?;
        match self.type_ {
            MediaStreamType::Video => {
                let vp8enc = gstreamer::ElementFactory::make("vp8enc")
                    .property("deadline", 1i64)
                    .property("cpu-used", -16i32)
                    .property("lag-in-frames", 0i32)
                    .build()?;

                let rtpvp8pay = gstreamer::ElementFactory::make("rtpvp8pay")
                    .property("mtu", 1200u32)
                    .build()?;
                let queue2 = gstreamer::ElementFactory::make("queue").build()?;

                pipeline.add_many([&vp8enc, &rtpvp8pay, &queue2, &capsfilter])?;
                gstreamer::Element::link_many([&src, &vp8enc, &rtpvp8pay, &queue2, &capsfilter])?;
                vp8enc.sync_state_with_parent()?;
                rtpvp8pay.sync_state_with_parent()?;
                queue2.sync_state_with_parent()?;
                capsfilter.sync_state_with_parent()?;
                Ok(capsfilter)
            },
            MediaStreamType::Audio => {
                let opusenc = gstreamer::ElementFactory::make("opusenc").build()?;
                let rtpopuspay = gstreamer::ElementFactory::make("rtpopuspay")
                    .property("mtu", 1200u32)
                    .build()?;
                let queue3 = gstreamer::ElementFactory::make("queue").build()?;
                pipeline.add_many([&opusenc, &rtpopuspay, &queue3, &capsfilter])?;
                gstreamer::Element::link_many([&src, &opusenc, &rtpopuspay, &queue3, &capsfilter])?;
                opusenc.sync_state_with_parent()?;
                rtpopuspay.sync_state_with_parent()?;
                queue3.sync_state_with_parent()?;
                Ok(capsfilter)
            },
        }
    }

    /// 표시(display) 경로용 소스. MediaStream은 이미 raw(videoconvert 뒤 queue)이므로
    /// 재인코딩 없이 그 tail 엘리먼트를 그대로 반환한다. `encoded()`(송출용)와 달리
    /// 파이프라인에 새 엘리먼트를 추가하지 않는다. send 경로는 계속 `encoded()`를 쓴다.
    pub fn raw(&mut self) -> gstreamer::Element {
        self.src_element()
    }

    pub fn create_video_from(source: gstreamer::Element) -> MediaStreamId {
        Self::create_video_from_with(source, None)
    }

    /// `create_video_from` 과 같되, 스트림이 살아있는 동안 붙잡고 있어야 하는
    /// 캡처 허브 등록을 함께 소유한다.
    pub fn create_video_from_with(
        source: gstreamer::Element,
        capture_consumer: Option<CaptureConsumer>,
    ) -> MediaStreamId {
        let videoconvert = gstreamer::ElementFactory::make("videoconvert")
            .build()
            .unwrap();
        // Servo의 비디오 appsink는 I420만 받고(render.rs), servomediastreamsrc의
        // I420 src pad 템플릿 요구는 proxysink/proxysrc 경계를 역전파하지 않는다
        // (getDisplayMedia 캡처 빈에서 검증된 함정 — media_capture.rs 참조). 여기서
        // I420을 고정하지 않으면 videoconvert가 passthrough로 협상해 카메라(YUY2 등)
        // 스트림이 proxy 경계에서 "caps not accepted" → not-negotiated 스톨한다.
        let i420_filter = gstreamer::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gstreamer::Caps::builder("video/x-raw")
                    .field("format", "I420")
                    .build(),
            )
            .build()
            .unwrap();
        let queue = gstreamer::ElementFactory::make("queue").build().unwrap();

        let mut stream = GStreamerMediaStream::new(
            MediaStreamType::Video,
            vec![source, videoconvert, i420_filter, queue],
        );
        stream.capture_consumer = capture_consumer;
        register_stream(Arc::new(Mutex::new(stream)))
    }

    pub fn create_audio() -> MediaStreamId {
        let audiotestsrc = gstreamer::ElementFactory::make("audiotestsrc")
            .property_from_str("wave", "sine")
            .property("is-live", true)
            .build()
            .unwrap();

        Self::create_audio_from(audiotestsrc)
    }

    pub fn create_audio_from(source: gstreamer::Element) -> MediaStreamId {
        let queue = gstreamer::ElementFactory::make("queue").build().unwrap();
        let audioconvert = gstreamer::ElementFactory::make("audioconvert")
            .build()
            .unwrap();
        let audioresample = gstreamer::ElementFactory::make("audioresample")
            .build()
            .unwrap();
        let queue2 = gstreamer::ElementFactory::make("queue").build().unwrap();

        register_stream(Arc::new(Mutex::new(GStreamerMediaStream::new(
            MediaStreamType::Audio,
            vec![source, queue, audioconvert, audioresample, queue2],
        ))))
    }

    pub fn create_proxy(ty: MediaStreamType) -> (MediaStreamId, GstreamerMediaSocket) {
        let proxy_sink = gstreamer::ElementFactory::make("proxysink")
            .build()
            .unwrap();
        let proxy_src = gstreamer::ElementFactory::make("proxysrc")
            .property("proxysink", &proxy_sink)
            .build()
            .unwrap();
        let stream = match ty {
            MediaStreamType::Audio => Self::create_audio_from(proxy_src),
            MediaStreamType::Video => Self::create_video_from(proxy_src),
        };

        (stream, GstreamerMediaSocket { proxy_sink })
    }
}

impl Drop for GStreamerMediaStream {
    fn drop(&mut self) {
        // 허브 등록을 먼저 놓는다 — 곧 NULL 로 갈 파이프라인에 프레임이
        // 더 들어오지 않게.
        self.capture_consumer = None;
        if let Some(pipeline) = self.pipeline.take() {
            // 이 파이프라인을 이 스트림이 만들었을 때만 NULL 로 끌고 간다. 남의
            // 파이프라인(예: webrtc.rs 의 공유 파이프라인)이면 그냥 복제본을
            // 떨어뜨릴 뿐 — 그 파이프라인에 딸린 다른 트랙까지 죽이면 안 된다.
            if self.owns_pipeline {
                if let Err(error) = pipeline.set_state(gstreamer::State::Null) {
                    log::warn!("could not stop a media stream pipeline: {error}");
                }
            }
        }
        if let Some(ref id) = self.id {
            unregister_stream(id);
        }
    }
}

#[derive(Default)]
pub struct MediaSink {
    streams: Vec<Arc<Mutex<dyn MediaStream>>>,
}

impl MediaOutput for MediaSink {
    fn add_stream(&mut self, stream: &MediaStreamId) {
        let stream = get_stream(stream).expect("Media streams registry does not contain such ID");
        {
            let mut stream = stream.lock().unwrap();
            let stream = stream
                .as_mut_any()
                .downcast_mut::<GStreamerMediaStream>()
                .unwrap();
            let pipeline = stream.pipeline_or_new();
            let last_element = stream.elements.last();
            let last_element = last_element.as_ref().unwrap();
            let sink = match stream.type_ {
                MediaStreamType::Audio => "autoaudiosink",
                MediaStreamType::Video => "autovideosink",
            };
            let sink = gstreamer::ElementFactory::make(sink).build().unwrap();
            pipeline.add(&sink).unwrap();
            gstreamer::Element::link_many(&[last_element, &sink][..]).unwrap();

            pipeline.set_state(gstreamer::State::Playing).unwrap();
            sink.sync_state_with_parent().unwrap();
        }
        self.streams.push(stream.clone());
    }
}

pub struct GstreamerMediaSocket {
    proxy_sink: gstreamer::Element,
}

impl GstreamerMediaSocket {
    pub fn proxy_sink(&self) -> &gstreamer::Element {
        &self.proxy_sink
    }
}

impl MediaSocket for GstreamerMediaSocket {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_hub::{HubKey, open_consumer_with};

    fn test_source() -> Option<gstreamer::Element> {
        gstreamer::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .ok()
    }

    /// 페이지 전환의 축소판: 스트림을 레지스트리에서 빼면 소비자가 허브에서
    /// 빠져야 하고, 장치는 계속 열려 있어야 한다.
    #[test]
    fn releasing_the_stream_releases_the_hub_consumer() {
        gstreamer::init().expect("gstreamer::init failed");
        let consumer =
            open_consumer_with(HubKey::for_test("stream-release"), test_source).expect("consumer");
        let hub = consumer.hub().clone();
        let source = consumer.source_element();

        let id = GStreamerMediaStream::create_video_from_with(source, Some(consumer));
        assert_eq!(hub.consumer_count(), 1);

        unregister_stream(&id);

        assert_eq!(
            hub.consumer_count(),
            0,
            "the stream did not release its hub consumer"
        );
        assert!(hub.is_playing(), "releasing a stream closed the device");
    }

    /// 허브를 안 쓰는 스트림(모의 소스, getDisplayMedia 등)은 그대로 동작한다.
    #[test]
    fn a_stream_without_a_capture_consumer_still_registers_and_releases() {
        gstreamer::init().expect("gstreamer::init failed");
        let id = GStreamerMediaStream::create_video_from(test_source().expect("source"));
        assert!(get_stream(&id).is_some());
        unregister_stream(&id);
        assert!(get_stream(&id).is_none());
    }

    /// WebRTC 처럼 여러 트랙이 파이프라인 하나를 공유하는 경우를 축소한 것:
    /// `attach_to_pipeline` 으로 남의 파이프라인에 붙은 스트림은 자신이
    /// 그 파이프라인을 만들지 않았으므로, 드롭돼도 그 파이프라인을
    /// NULL 로 끌고 가면 안 된다 — 같이 붙어 있는 다른 트랙까지 죽는다.
    #[test]
    fn dropping_a_stream_on_a_shared_pipeline_leaves_it_running() {
        gstreamer::init().expect("gstreamer::init failed");
        let pipeline = gstreamer::Pipeline::with_name(
            "media_stream tests: dropping_a_stream_on_a_shared_pipeline_leaves_it_running",
        );
        let id = GStreamerMediaStream::create_video_from(test_source().expect("source"));
        {
            let stream = get_stream(&id).expect("stream");
            let mut stream = stream.lock().unwrap();
            let stream = stream
                .as_mut_any()
                .downcast_mut::<GStreamerMediaStream>()
                .expect("GStreamerMediaStream");
            stream.attach_to_pipeline(&pipeline);
        }
        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("shared pipeline should accept PLAYING");
        pipeline
            .state(gstreamer::ClockTime::from_seconds(5))
            .0
            .expect("shared pipeline should reach PLAYING");

        unregister_stream(&id);

        assert_eq!(
            pipeline.current_state(),
            gstreamer::State::Playing,
            "dropping a stream that does not own its pipeline tore that pipeline down"
        );

        let _ = pipeline.set_state(gstreamer::State::Null);
    }
}

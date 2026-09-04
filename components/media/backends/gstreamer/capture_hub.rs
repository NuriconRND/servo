/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 물리 포트당 살아있는 캡처 연결 하나를, 모든 소비자가 공유한다.
//!
//! 이 장비의 캡처카드는 같은 포트를 두 번 열면 불안정하고 세 번째에 무너진다
//! (2026-08-20 실측: K=1 5/5, K=2 4/5, K=3 0/5). 그런데 `getUserMedia` 는
//! 호출마다 새 `ksvideosrc` 를 열고, 스트림 레지스트리가 스트림을 영원히
//! 붙잡고 있어 닫히지도 않는다 — 페이지를 전환할수록 같은 포트의 연결이
//! 쌓인다. 그래서 장치를 여는 주체를 여기 하나로 모은다.
//!
//! 허브는 그 포트를 **처음 요청할 때 열고, 프로세스가 끝날 때까지 유지한다.**
//! 소비자(= `getUserMedia` 호출 하나)는 `appsrc` 를 받아가고, 사라질 때
//! 명단에서 빠지기만 한다. 캡처 파이프라인의 상태 전이는 0 이다 — 이 저장소가
//! 반복해서 데인 "라이브 파이프라인 teardown" 경로를 아예 밟지 않는다.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSinkCallbacks, AppSrc, AppStreamType};

use crate::device_id::{device_path, normalized_port_key};

/// 소비자 하나가 밀릴 수 있는 프레임 수. 넘으면 오래된 것부터 버린다.
const CONSUMER_MAX_BUFFERS: u64 = 4;

/// 허브 파이프라인이 PLAYING 에 도달하기를 기다리는 시간.
const START_TIMEOUT: gstreamer::ClockTime = gstreamer::ClockTime::from_seconds(5);

static NEXT_CONSUMER_ID: AtomicU64 = AtomicU64::new(1);

/// 키별 슬롯. 전역 맵의 잠금은 슬롯을 꺼낼 때만 잡고, 실제 장치 개방은
/// 슬롯 잠금 아래에서 한다 — 느린 장치 하나가 다른 포트를 막지 않으면서도
/// 같은 포트의 동시 개방은 직렬화된다.
type Slot = Arc<Mutex<Option<Arc<DeviceHub>>>>;
static SLOTS: LazyLock<Mutex<HashMap<HubKey, Slot>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// 물리 포트 하나를 가리키는 키.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HubKey(String);

impl HubKey {
    /// 비디오 장치를 물리 포트로 접는다. ks/mf 쌍둥이는 같은 키가 된다
    /// (`normalized_port_key`). 경로를 노출하지 않는 장치는 표시 이름으로
    /// 폴백한다 — `media_capture.rs::select_device_by_id` 의 3티어와 같은 규칙.
    pub fn for_video_device(device: &gstreamer::Device) -> Self {
        match device_path(device) {
            Some(path) => Self(format!("video:{}", normalized_port_key(&path))),
            None => Self(format!("video-name:{}", device.display_name())),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(name: &str) -> Self {
        Self(format!("test:{name}"))
    }
}

struct Consumer {
    id: u64,
    appsrc: AppSrc,
}

/// 배포 콜백과 소비자 등록이 공유하는 상태. `DeviceHub` 를 직접 담지 않는다 —
/// 담으면 hub -> pipeline -> appsink -> callback -> hub 순환 참조가 된다.
struct SharedState {
    key: HubKey,
    consumers: Mutex<Vec<Consumer>>,
    caps: Mutex<Option<gstreamer::Caps>>,
}

/// 포트 하나에 대응하는 살아있는 캡처 파이프라인.
pub struct DeviceHub {
    key: HubKey,
    pipeline: gstreamer::Pipeline,
    shared: Arc<SharedState>,
    healthy: Arc<AtomicBool>,
}

/// 허브에 등록된 소비자 하나. drop 되면 명단에서 빠진다. 장치는 그대로 열려 있다.
pub struct CaptureConsumer {
    hub: Arc<DeviceHub>,
    id: u64,
    appsrc: AppSrc,
}

impl CaptureConsumer {
    /// 이 소비자의 `MediaStream` 을 먹일 소스 엘리먼트.
    pub fn source_element(&self) -> gstreamer::Element {
        self.appsrc.clone().upcast()
    }

    #[cfg(test)]
    pub(crate) fn hub(&self) -> &Arc<DeviceHub> {
        &self.hub
    }
}

impl Drop for CaptureConsumer {
    fn drop(&mut self) {
        self.hub.remove_consumer(self.id);
    }
}

/// 없을 수도 있는 프로퍼티를 안전하게 설정한다. `set_property` 는 없는
/// 프로퍼티에 패닉하므로 반드시 확인하고 넣는다.
fn set_u64_if_present(element: &gstreamer::Element, name: &str, value: u64) {
    if element.find_property(name).is_some() {
        element.set_property(name, value);
    } else {
        log::warn!(
            "capture hub: {} has no {name} property; leaving its default",
            element.name()
        );
    }
}

fn set_enum_if_present(element: &gstreamer::Element, name: &str, value: &str) {
    if element.find_property(name).is_some() {
        element.set_property_from_str(name, value);
    } else {
        log::warn!(
            "capture hub: {} has no {name} property; leaving its default",
            element.name()
        );
    }
}

impl DeviceHub {
    fn open(key: HubKey, source: gstreamer::Element) -> Option<Arc<DeviceHub>> {
        let pipeline = gstreamer::Pipeline::with_name(&format!("capture hub {}", key.0));

        // 캡처 스레드와 변환 스레드를 분리한다. 변환 지터가 장치에 닿지 않게.
        let queue = gstreamer::ElementFactory::make("queue")
            .property("max-size-buffers", 2u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .build()
            .ok()?;
        set_enum_if_present(&queue, "leaky", "downstream");

        let convert = gstreamer::ElementFactory::make("videoconvert")
            .build()
            .ok()?;
        let filter = gstreamer::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gstreamer::Caps::builder("video/x-raw")
                    .field("format", "I420")
                    .build(),
            )
            .build()
            .ok()?;
        // 배포가 밀려도 캡처는 절대 막지 않는다.
        let appsink = gstreamer::ElementFactory::make("appsink")
            .property("sync", false)
            .property("drop", true)
            .property("max-buffers", 1u32)
            .build()
            .ok()?
            .downcast::<AppSink>()
            .ok()?;

        let elements = [
            &source,
            &queue,
            &convert,
            &filter,
            appsink.upcast_ref::<gstreamer::Element>(),
        ];
        pipeline.add_many(elements).ok()?;
        gstreamer::Element::link_many(elements).ok()?;

        let shared = Arc::new(SharedState {
            key: key.clone(),
            consumers: Mutex::new(Vec::new()),
            caps: Mutex::new(None),
        });
        let healthy = Arc::new(AtomicBool::new(true));

        let sink_shared = shared.clone();
        appsink.set_callbacks(
            AppSinkCallbacks::builder()
                .new_sample(move |sink| distribute(&sink_shared, sink))
                .build(),
        );

        // 동기 핸들러를 쓴다 — 이 파이프라인의 버스를 폴링하는 glib 메인루프가 없다.
        if let Some(bus) = pipeline.bus() {
            let bus_healthy = healthy.clone();
            let bus_key = key.clone();
            bus.set_sync_handler(move |_, message| {
                match message.view() {
                    gstreamer::MessageView::Error(error) => {
                        log::warn!(
                            "capture hub: {} failed: {} ({:?})",
                            bus_key.0,
                            error.error(),
                            error.debug()
                        );
                        bus_healthy.store(false, Ordering::Relaxed);
                    },
                    gstreamer::MessageView::Eos(_) => {
                        // 살아있는 캡처 소스는 정상적으로 스트림을 끝내지 않는다 —
                        // 장치가 뽑히면 소스에 따라 ERROR 대신 EOS 로 나타난다.
                        log::warn!(
                            "capture hub: {} reached EOS; a live capture source should never end",
                            bus_key.0
                        );
                        bus_healthy.store(false, Ordering::Relaxed);
                    },
                    _ => {},
                }
                gstreamer::BusSyncReply::Drop
            });
        }

        if pipeline.set_state(gstreamer::State::Playing).is_err() {
            log::warn!("capture hub: {} could not be started", key.0);
            let _ = pipeline.set_state(gstreamer::State::Null);
            return None;
        }
        // 라이브 소스는 NoPreroll 로 즉시 돌아온다. 장치가 사용 중이면 여기서
        // 실패하므로, 죽은 허브를 슬롯에 넣지 않고 바로 None 을 돌려준다.
        if pipeline.state(START_TIMEOUT).0.is_err() {
            log::warn!("capture hub: {} did not reach PLAYING", key.0);
            let _ = pipeline.set_state(gstreamer::State::Null);
            return None;
        }

        log::info!("capture hub: opened {}", key.0);
        Some(Arc::new(DeviceHub {
            key,
            pipeline,
            shared,
            healthy,
        }))
    }

    fn add_consumer(self: &Arc<Self>) -> Option<CaptureConsumer> {
        let appsrc = gstreamer::ElementFactory::make("appsrc")
            .property("is-live", true)
            // 소비자가 한참 뒤에 합류해도 타임스탬프가 튀지 않도록, 자기
            // 파이프라인의 running time 으로 다시 찍게 한다.
            .property("do-timestamp", true)
            .build()
            .ok()?
            .downcast::<AppSrc>()
            .ok()?;
        appsrc.set_format(gstreamer::Format::Time);
        appsrc.set_stream_type(AppStreamType::Stream);
        appsrc.set_max_bytes(0);
        let element: &gstreamer::Element = appsrc.upcast_ref();
        set_u64_if_present(element, "max-buffers", CONSUMER_MAX_BUFFERS);
        // 밀린 소비자는 장치를 막는 대신 자기 프레임을 버린다.
        set_enum_if_present(element, "leaky-type", "downstream");

        // caps 를 읽는 것과 소비자 목록에 넣는 것을 하나의 임계구역으로 묶는다 —
        // 그 사이에 첫 샘플이 도착하면 `distribute` 가 caps 를 캐시하고 아직
        // 목록에 없는 이 소비자를 건너뛴다. "바뀔 때만 갱신" 가드 때문에 그
        // 뒤로는 다시 안 열려서, 이 소비자는 캡스를 영영 못 받는다. 잠금 순서는
        // `distribute` 와 동일하게 caps -> consumers 로 유지한다.
        let caps_guard = self.shared.caps.lock().unwrap();
        if let Some(caps) = caps_guard.as_ref() {
            appsrc.set_caps(Some(caps));
        }

        let id = NEXT_CONSUMER_ID.fetch_add(1, Ordering::Relaxed);
        let count = {
            let mut consumers = self.shared.consumers.lock().unwrap();
            consumers.push(Consumer {
                id,
                appsrc: appsrc.clone(),
            });
            consumers.len()
        };
        drop(caps_guard);
        log::info!(
            "capture hub: {} consumer {id} added (consumers={count})",
            self.key.0
        );
        Some(CaptureConsumer {
            hub: self.clone(),
            id,
            appsrc,
        })
    }

    fn remove_consumer(&self, id: u64) {
        let count = {
            let mut consumers = self.shared.consumers.lock().unwrap();
            consumers.retain(|consumer| consumer.id != id);
            consumers.len()
        };
        log::info!(
            "capture hub: {} consumer {id} removed (consumers={count})",
            self.key.0
        );
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn consumer_count(&self) -> usize {
        self.shared.consumers.lock().unwrap().len()
    }

    #[cfg(test)]
    pub(crate) fn is_playing(&self) -> bool {
        self.pipeline.current_state() == gstreamer::State::Playing
    }

    /// 파이프라인을 내리고 버스 핸들러를 뗀다. **두 번 불려도 안전하다** — 재개방
    /// 경로(교체된 `slot` 이 `Arc` 를 통해 여전히 살아있는 `CaptureConsumer` 에
    /// 붙들려 있는 동안)와 `Drop` 이 각각 부를 수 있어서, `Arc` 참조 카운트에
    /// 기대지 않고 여기서 명시적으로 장치를 놓아준다. 그러지 않으면 오래된
    /// 소스가 OS 장치 핸들을 쥔 채로 새 허브가 같은 포트를 다시 여는
    /// "포트를 두 번 연다" 상태(K=2)가 재발한다.
    fn shutdown(&self) {
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

impl Drop for DeviceHub {
    fn drop(&mut self) {
        log::info!("capture hub: closing {}", self.key.0);
        self.shutdown();
    }
}

/// 배포 스레드. `appsink` 의 스트리밍 스레드에서 불린다.
fn distribute(
    shared: &SharedState,
    sink: &AppSink,
) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
    let sample = sink.pull_sample().map_err(|_| gstreamer::FlowError::Eos)?;

    if let Some(caps) = sample.caps() {
        let caps = caps.to_owned();
        let mut cached = shared.caps.lock().unwrap();
        if cached.as_ref() != Some(&caps) {
            *cached = Some(caps.clone());
            for consumer in shared.consumers.lock().unwrap().iter() {
                consumer.appsrc.set_caps(Some(&caps));
            }
        }
    }

    let Some(buffer) = sample.buffer() else {
        return Ok(gstreamer::FlowSuccess::Ok);
    };

    let mut consumers = shared.consumers.lock().unwrap();
    consumers.retain(|consumer| {
        // shallow copy — 메모리는 공유하고 헤더만 새로 만든다. 픽셀 복사 없음.
        let mut copy = buffer.copy();
        {
            let copy = copy
                .get_mut()
                .expect("a fresh buffer copy is uniquely owned");
            copy.set_pts(None);
            copy.set_dts(None);
        }
        match consumer.appsrc.push_buffer(copy) {
            // Flushing = 소비자 파이프라인이 아직/이미 NULL. 정상이다.
            Ok(_) | Err(gstreamer::FlowError::Flushing) => true,
            Err(error) => {
                log::info!(
                    "capture hub: {} dropping consumer {} ({error:?})",
                    shared.key.0,
                    consumer.id
                );
                false
            },
        }
    });

    Ok(gstreamer::FlowSuccess::Ok)
}

/// `device` 의 물리 포트에 대한 단 하나의 캡처 연결에 합류한다.
/// 그 포트가 아직 안 열려 있으면 여기서 연다.
pub fn open_video_consumer(device: &gstreamer::Device) -> Option<CaptureConsumer> {
    let key = HubKey::for_video_device(device);
    let device = device.clone();
    open_consumer_with(key, move || device.create_element(None).ok())
}

pub(crate) fn open_consumer_with(
    key: HubKey,
    make_source: impl FnOnce() -> Option<gstreamer::Element>,
) -> Option<CaptureConsumer> {
    let slot = {
        let mut slots = SLOTS.lock().unwrap();
        slots
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    };
    // 전역 잠금은 놓았다. 여기부터는 이 키에 대해서만 직렬화된다.
    let mut slot = slot.lock().unwrap();

    if let Some(hub) = slot.as_ref() {
        if hub.is_healthy() {
            log::info!("capture hub: reused {}", key.0);
            return hub.add_consumer();
        }
        log::warn!("capture hub: {} is unhealthy; reopening", key.0);
        // `*slot = None` 만으로는 부족하다 — 살아있는 `CaptureConsumer` 가 여전히
        // `Arc<DeviceHub>` 를 쥐고 있으면 `Drop` 이 안 불려서 장치가 안 닫힌다.
        // 여기서 명시적으로 닫아 슬롯의 참조 카운트와 무관하게 포트를 놓는다.
        hub.shutdown();
        *slot = None;
    }

    let hub = DeviceHub::open(key, make_source()?)?;
    let consumer = hub.add_consumer();
    *slot = Some(hub);
    consumer
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn init() {
        gstreamer::init().expect("gstreamer::init failed");
    }

    /// 카메라처럼 페이싱되는 테스트 소스.
    fn test_source() -> Option<gstreamer::Element> {
        gstreamer::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .ok()
    }

    /// `make_source` 가 몇 번 불렸는지 = 장치를 몇 번 열었는지.
    fn counting_source(opens: &Arc<AtomicUsize>) -> impl FnOnce() -> Option<gstreamer::Element> {
        let opens = opens.clone();
        move || {
            opens.fetch_add(1, Ordering::Relaxed);
            test_source()
        }
    }

    #[test]
    fn the_same_key_opens_the_device_once() {
        init();
        let opens = Arc::new(AtomicUsize::new(0));
        let key = HubKey::for_test("opens-once");

        let first =
            open_consumer_with(key.clone(), counting_source(&opens)).expect("first consumer");
        let second = open_consumer_with(key, counting_source(&opens)).expect("second consumer");

        assert_eq!(
            opens.load(Ordering::Relaxed),
            1,
            "the device was opened twice"
        );
        assert_eq!(
            first.hub().consumer_count(),
            2,
            "the second consumer did not land on the first consumer's hub"
        );
        assert!(first.hub().is_playing(), "the shared hub is not playing");
        drop(first);
        drop(second);
    }

    #[test]
    fn different_keys_open_separate_hubs() {
        init();
        let opens = Arc::new(AtomicUsize::new(0));

        let a = open_consumer_with(HubKey::for_test("separate-a"), counting_source(&opens))
            .expect("consumer a");
        let b = open_consumer_with(HubKey::for_test("separate-b"), counting_source(&opens))
            .expect("consumer b");

        assert_eq!(opens.load(Ordering::Relaxed), 2);
        drop(a);
        drop(b);
    }

    #[test]
    fn concurrent_opens_of_the_same_key_open_once() {
        init();
        let opens = Arc::new(AtomicUsize::new(0));
        let key = HubKey::for_test("opens-once-concurrently");

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let key = key.clone();
                let opens = opens.clone();
                std::thread::spawn(move || {
                    open_consumer_with(key, counting_source(&opens)).expect("consumer")
                })
            })
            .collect();
        let consumers: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert_eq!(
            opens.load(Ordering::Relaxed),
            1,
            "concurrent opens raced the device open"
        );
        drop(consumers);
    }

    #[test]
    fn a_source_that_cannot_be_built_yields_no_consumer() {
        init();
        assert!(open_consumer_with(HubKey::for_test("no-source"), || None).is_none());
    }

    use std::time::{Duration, Instant};

    /// 소비자의 appsrc 를 자기 파이프라인에서 돌리며 도착한 버퍼를 센다.
    struct ConsumerSink {
        pipeline: gstreamer::Pipeline,
        received: Arc<AtomicUsize>,
    }

    impl ConsumerSink {
        /// `playing=false` 면 PAUSED 에 머문다 = 프레임을 안 빼가는 소비자.
        fn start(consumer: &CaptureConsumer, playing: bool) -> Self {
            let pipeline = gstreamer::Pipeline::new();
            let sink = gstreamer::ElementFactory::make("fakesink")
                .property("sync", false)
                .build()
                .expect("fakesink");
            let source = consumer.source_element();
            pipeline.add_many([&source, &sink]).expect("add");
            source.link(&sink).expect("link");

            let received = Arc::new(AtomicUsize::new(0));
            let counter = received.clone();
            sink.static_pad("sink")
                .expect("fakesink sink pad")
                .add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    gstreamer::PadProbeReturn::Ok
                });

            let state = if playing {
                gstreamer::State::Playing
            } else {
                gstreamer::State::Paused
            };
            pipeline.set_state(state).expect("consumer pipeline state");
            Self { pipeline, received }
        }

        fn count(&self) -> usize {
            self.received.load(Ordering::Relaxed)
        }
    }

    impl Drop for ConsumerSink {
        fn drop(&mut self) {
            let _ = self.pipeline.set_state(gstreamer::State::Null);
        }
    }

    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if condition() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn every_consumer_receives_frames() {
        init();
        let key = HubKey::for_test("fan-out");
        let consumers: Vec<_> = (0..3)
            .map(|_| open_consumer_with(key.clone(), test_source).expect("consumer"))
            .collect();
        let sinks: Vec<_> = consumers
            .iter()
            .map(|consumer| ConsumerSink::start(consumer, true))
            .collect();

        wait_until("all three consumers to receive frames", || {
            sinks.iter().all(|sink| sink.count() >= 3)
        });
    }

    #[test]
    fn dropping_one_consumer_leaves_the_others_running() {
        init();
        let key = HubKey::for_test("drop-one");
        let keep = open_consumer_with(key.clone(), test_source).expect("keep");
        let discard = open_consumer_with(key, test_source).expect("discard");
        let hub = keep.hub().clone();
        let keep_sink = ConsumerSink::start(&keep, true);
        let discard_sink = ConsumerSink::start(&discard, true);

        wait_until("both consumers to start", || {
            keep_sink.count() >= 2 && discard_sink.count() >= 2
        });

        drop(discard_sink);
        drop(discard);
        assert_eq!(hub.consumer_count(), 1);

        let before = keep_sink.count();
        wait_until("the surviving consumer to keep receiving", || {
            keep_sink.count() > before + 2
        });
        assert!(hub.is_playing());
    }

    #[test]
    fn the_hub_keeps_running_with_no_consumers() {
        init();
        let key = HubKey::for_test("no-consumers");
        let consumer = open_consumer_with(key.clone(), test_source).expect("consumer");
        let hub = consumer.hub().clone();
        drop(consumer);

        assert_eq!(hub.consumer_count(), 0);
        assert!(
            hub.is_playing(),
            "the device was closed when the last consumer left"
        );

        // 다시 요청해도 장치를 새로 열지 않는다.
        let opens = Arc::new(AtomicUsize::new(0));
        let rejoined = open_consumer_with(key, counting_source(&opens)).expect("rejoined");
        assert_eq!(opens.load(Ordering::Relaxed), 0);
        assert_eq!(rejoined.hub().consumer_count(), 1);
    }

    #[test]
    fn a_stalled_consumer_does_not_stall_the_others() {
        init();
        let key = HubKey::for_test("stalled");
        let healthy = open_consumer_with(key.clone(), test_source).expect("healthy");
        let stalled = open_consumer_with(key, test_source).expect("stalled");
        let hub = healthy.hub().clone();
        let healthy_sink = ConsumerSink::start(&healthy, true);
        // PAUSED 라 프레임을 빼가지 않는다. appsrc 가 leaky 라 밀린 것을 버린다.
        let _stalled_sink = ConsumerSink::start(&stalled, false);

        wait_until("the healthy consumer to keep receiving", || {
            healthy_sink.count() >= 10
        });
        assert_eq!(hub.consumer_count(), 2, "the stalled consumer was evicted");
    }

    /// 몇 프레임 뒤 버스에 ERROR 를 올리는 소스 = 뽑힌 장치.
    fn failing_source() -> Option<gstreamer::Element> {
        let src = gstreamer::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .ok()?;
        let identity = gstreamer::ElementFactory::make("identity")
            .property("error-after", 3i32)
            .build()
            .ok()?;
        let bin = gstreamer::Bin::new();
        bin.add_many([&src, &identity]).ok()?;
        src.link(&identity).ok()?;
        let pad = identity.static_pad("src")?;
        let ghost = gstreamer::GhostPad::with_target(&pad).ok()?;
        bin.add_pad(&ghost).ok()?;
        Some(bin.upcast())
    }

    /// 몇 프레임 뒤 EOS 를 내는 소스 = 장치 뽑힘이 ERROR 대신 EOS 로 나타나는 경우.
    fn eos_source() -> Option<gstreamer::Element> {
        gstreamer::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property("num-buffers", 5i32)
            .build()
            .ok()
    }

    #[test]
    fn a_failed_device_is_reopened_on_the_next_request() {
        init();
        let key = HubKey::for_test("failed-device");
        let broken = open_consumer_with(key.clone(), failing_source).expect("broken consumer");
        let broken_hub = broken.hub().clone();
        let _broken_sink = ConsumerSink::start(&broken, true);

        wait_until("the hub to notice the device error", || {
            !broken_hub.is_healthy()
        });

        let opens = Arc::new(AtomicUsize::new(0));
        let recovered =
            open_consumer_with(key, counting_source(&opens)).expect("recovered consumer");

        assert_eq!(
            opens.load(Ordering::Relaxed),
            1,
            "the failed hub was reused"
        );
        assert!(recovered.hub().is_healthy());
        assert!(!Arc::ptr_eq(recovered.hub(), &broken_hub));
    }

    #[test]
    fn a_healthy_device_is_never_reopened() {
        init();
        let key = HubKey::for_test("never-reopened");
        let first = open_consumer_with(key.clone(), test_source).expect("first");
        let sink = ConsumerSink::start(&first, true);
        wait_until("the first consumer to receive frames", || sink.count() >= 3);

        let opens = Arc::new(AtomicUsize::new(0));
        for _ in 0..5 {
            let consumer =
                open_consumer_with(key.clone(), counting_source(&opens)).expect("repeat consumer");
            drop(consumer);
        }
        assert_eq!(
            opens.load(Ordering::Relaxed),
            0,
            "a page transition reopened the device"
        );
    }

    #[test]
    fn a_device_that_reaches_eos_is_reopened_on_the_next_request() {
        init();
        let key = HubKey::for_test("eos-device");
        let broken = open_consumer_with(key.clone(), eos_source).expect("eos consumer");
        let broken_hub = broken.hub().clone();
        let _broken_sink = ConsumerSink::start(&broken, true);

        wait_until("the hub to notice the device EOS", || {
            !broken_hub.is_healthy()
        });

        let opens = Arc::new(AtomicUsize::new(0));
        let recovered =
            open_consumer_with(key, counting_source(&opens)).expect("recovered consumer");

        assert_eq!(opens.load(Ordering::Relaxed), 1, "the EOS'd hub was reused");
        assert!(recovered.hub().is_healthy());
        assert!(!Arc::ptr_eq(recovered.hub(), &broken_hub));
    }
}

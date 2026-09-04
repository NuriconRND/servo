# 캡처카드 단일 연결 공유 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 캡처카드 포트당 물리 연결을 프로세스 전체에서 정확히 1개로 유지하고, `getUserMedia()` 가 그 연결의 배포 스레드로부터 프레임을 받게 하여 페이지 전환 시 발생하는 간헐적 크래시를 없앤다.

**Architecture:** 새 모듈 `capture_hub.rs` 가 포트별로 파이프라인 하나(`devicesrc ! queue ! videoconvert ! capsfilter(I420) ! appsink`)를 지연 개방해 프로세스 종료까지 유지한다. `appsink` 의 `new_sample` 콜백이 배포 스레드가 되어, 등록된 소비자들의 `appsrc` 에 프레임의 shallow copy 를 push 한다. `getUserMedia` 는 그 `appsrc` 를 소스로 하는 `MediaStream` 을 받는다. 소비자가 사라질 때 캡처 파이프라인의 상태 전이는 0 이다 — `Vec` 에서 빠질 뿐이다.

**Tech Stack:** Rust 1.95 / Cargo workspace, gstreamer-rs 0.25 + gstreamer-app 0.25, 런타임 GStreamer 1.26.8 (Nuricon build 101), Servo script DOM + WebIDL codegen, Windows x64.

**Spec:** `docs/superpowers/specs/2026-09-04-capture-card-shared-connection-design.md`

## Global Constraints

- 브랜치: `capture-card-shared-connection` (베이스 `multigpu-wall-pacing`). 워크트리 `servo_multigpu-tiled-wall`.
- **모든 PowerShell 세션에서 먼저 `. .\scripts\servo_env.ps1` 을 점(.)으로 소싱**한다. 워크스페이스 루트(`F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser`)에서 소싱한 뒤 `servo_multigpu-tiled-wall` 로 이동한다. 이어서 `$ErrorActionPreference='Continue'` 를 설정한다.
- 단위/통합 테스트 루프: `cargo test -p servo-media-gstreamer --lib`, `cargo test -p servo-media-streams --lib`. (첫 빌드 약 2분, 이후 수 초.)
- **최종 검증 빌드는 `cargo build -p servoshell` 이 아니라 `.\mach build -j 8`** 이다 — `cargo -p` 로는 미디어 백엔드가 더미로 빠지는 함정이 기록되어 있다.
- 서식: `rustfmt --edition 2024 --check <touched .rs>` 와 `git diff --check` 를 태스크마다 통과시킨다.
- **커밋 메시지에 Claude 서명·세션 주소를 넣지 않는다.** 명령형 한 문장 제목 + 필요한 만큼의 본문(이 저장소 관례).
- `set_property` 는 **없는 프로퍼티에 패닉**한다. 버전에 따라 없을 수 있는 프로퍼티(`leaky-type`, `max-buffers`)는 반드시 `find_property` 로 확인 후 설정한다.
- 개발기에는 캡처카드가 없다. 하드웨어가 필요한 확인은 Task 8 의 실기 체크리스트로 미루고, 그 외 전부를 `videotestsrc` 기반 테스트로 덮는다.
- 이 개발기는 GPU 가 CLAUDE.md 기재와 달라(AMD 7800M) **성능 수치를 측정하지 않는다.**

## 파일 구조

| 파일 | 책임 | 태스크 |
|---|---|---|
| `components/media/streams/registry.rs` (수정) | 스트림 레지스트리. 해제가 재진입 교착을 일으키지 않게 고친다 | 1 |
| `components/media/backends/gstreamer/capture_hub.rs` (신규) | 포트당 캡처 파이프라인 1개의 개방·유지·프레임 배포·소비자 명단 | 2·3·4 |
| `components/media/backends/gstreamer/lib.rs` (수정) | `mod capture_hub;` 선언 | 2 |
| `components/media/backends/gstreamer/media_capture.rs` (수정) | 장치 선택과 element 생성을 분리하고, 비디오 입력을 허브로 보낸다 | 5 |
| `components/media/backends/gstreamer/media_stream.rs` (수정) | 스트림이 허브 등록을 소유하고, Drop 에서 파이프라인을 NULL 로 내린다 | 5 |
| `components/script_bindings/webidls/MediaStreamTrack.webidl` (수정) | `stop()` 노출 | 6 |
| `components/script/dom/media/mediastreamtrack.rs` (수정) | `stop()` 구현 | 6 |
| `components/script/dom/globalscope.rs` (수정) | 이 global 이 만든 캡처 스트림 목록과 일괄 해제 | 7 |
| `components/script/dom/media/mediadevices.rs` (수정) | 생성한 스트림 id 를 global 에 등록 | 7 |
| `components/script/dom/window.rs` (수정) | 파이프라인 종료 시 해제 호출 | 7 |
| `tests/html/multigpu_capture_card_probe.html` (수정) | 자동 전환 반복 + `stop()` 버튼 | 8 |
| `docs/multigpu/capture_card_shared_connection.md` (신규) | 운용/진단 노트 | 8 |

---

### Task 1: 레지스트리 해제를 재진입 안전하게

**왜 먼저인가:** 지금 `unregister_stream` 은 잠금을 쥔 채로 `Arc` 를 떨어뜨린다. `Drop` 이 실제로 돌기 시작하는 순간(이 계획의 목적) `GStreamerMediaStream::drop` → `unregister_stream` 이 같은 뮤텍스를 재진입해 **교착**한다. 뒤의 모든 태스크가 이 수정에 의존한다.

**Files:**
- Modify: `components/media/streams/registry.rs:43-45`
- Test: `components/media/streams/registry.rs` (같은 파일의 `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: 없음
- Produces: `pub fn unregister_stream(stream: &MediaStreamId)` — 시그니처 불변. 보장이 추가된다: **레지스트리 잠금을 놓은 뒤에 스트림이 drop 된다.**

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`components/media/streams/registry.rs` 끝에 추가:

```rust
#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::{MediaStream, MediaStreamType};

    /// `GStreamerMediaStream` 과 같은 모양: Drop 에서 자기 자신을 다시 unregister 한다.
    struct ReentrantStream {
        id: Option<MediaStreamId>,
    }

    impl MediaStream for ReentrantStream {
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
            MediaStreamType::Video
        }
    }

    impl Drop for ReentrantStream {
        fn drop(&mut self) {
            if let Some(ref id) = self.id {
                unregister_stream(id);
            }
        }
    }

    #[test]
    fn unregistering_a_stream_that_unregisters_itself_does_not_deadlock() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let id = register_stream(Arc::new(Mutex::new(ReentrantStream { id: None })));
            unregister_stream(&id);
            let _ = tx.send(id);
        });
        let id = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("unregister_stream deadlocked on the re-entrant Drop");
        assert!(get_stream(&id).is_none());
    }
}
```

- [ ] **Step 2: 테스트가 실패(행업)하는 것을 확인한다**

```powershell
cargo test -p servo-media-streams --lib unregistering_a_stream
```

Expected: FAIL — `unregister_stream deadlocked on the re-entrant Drop` (5초 타임아웃).

- [ ] **Step 3: 최소 구현**

`components/media/streams/registry.rs` 의 `unregister_stream` 을 교체:

```rust
pub fn unregister_stream(stream: &MediaStreamId) {
    // 잠금을 놓은 뒤에 스트림을 drop 한다. `MediaStream` 구현체의 Drop 은
    // 관례적으로 자기 자신을 다시 unregister 하므로(GStreamerMediaStream),
    // 잠금을 쥔 채 drop 하면 같은 뮤텍스를 재진입해 교착한다. 반환값을
    // 이름 있는 바인딩으로 받아야 MutexGuard 가 먼저 풀린다 — 결과를 버리면
    // 임시값 drop 순서상 Arc 가 guard 보다 먼저 죽어 교착한다.
    let removed = MEDIA_STREAMS_REGISTRY.lock().unwrap().remove(stream);
    drop(removed);
}
```

- [ ] **Step 4: 테스트 통과 확인**

```powershell
cargo test -p servo-media-streams --lib
```

Expected: PASS (신규 1건 포함).

- [ ] **Step 5: 서식 + 커밋**

```powershell
rustfmt --edition 2024 --check components/media/streams/registry.rs
git diff --check
git add components/media/streams/registry.rs
git commit -m "Drop a stream outside the registry lock"
```

---

### Task 2: 허브 골격 — 포트당 한 번만 연다

**Files:**
- Create: `components/media/backends/gstreamer/capture_hub.rs`
- Modify: `components/media/backends/gstreamer/lib.rs:5-19` (모듈 선언 목록)
- Test: `components/media/backends/gstreamer/capture_hub.rs` 의 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `crate::device_id::{device_path, normalized_port_key}`
- Produces:
  - `pub struct HubKey` (`Clone + Debug + Eq + Hash + PartialEq`), `HubKey::for_video_device(&gstreamer::Device) -> HubKey`, `#[cfg(test)] HubKey::for_test(&str) -> HubKey`
  - `pub struct CaptureConsumer`, `CaptureConsumer::source_element(&self) -> gstreamer::Element`
  - `pub fn open_video_consumer(device: &gstreamer::Device) -> Option<CaptureConsumer>`
  - `pub(crate) fn open_consumer_with(key: HubKey, make_source: impl FnOnce() -> Option<gstreamer::Element>) -> Option<CaptureConsumer>`
  - `pub struct DeviceHub` + `#[cfg(test)] DeviceHub::consumer_count(&self) -> usize`, `#[cfg(test)] DeviceHub::is_playing(&self) -> bool`, `DeviceHub::is_healthy(&self) -> bool`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

새 파일 `components/media/backends/gstreamer/capture_hub.rs` 에 테스트만 먼저 넣는다(구현은 Step 3):

```rust
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

        assert_eq!(opens.load(Ordering::Relaxed), 1, "the device was opened twice");
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
}
```

- [ ] **Step 2: 컴파일 실패를 확인한다**

```powershell
cargo test -p servo-media-gstreamer --lib capture_hub
```

Expected: FAIL — `capture_hub.rs` 가 모듈로 선언되지 않았고 `HubKey`/`open_consumer_with` 가 없다.

- [ ] **Step 3: 최소 구현**

`components/media/backends/gstreamer/lib.rs` 의 모듈 목록에 알파벳 순으로 추가:

```rust
pub mod audio_stream_reader;
mod capture_hub;
mod datachannel;
```

`capture_hub.rs` 의 **테스트 모듈 위에** 다음을 넣는다:

```rust
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
static SLOTS: LazyLock<Mutex<HashMap<HubKey, Slot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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

        let convert = gstreamer::ElementFactory::make("videoconvert").build().ok()?;
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
                if let gstreamer::MessageView::Error(error) = message.view() {
                    log::warn!(
                        "capture hub: {} failed: {} ({:?})",
                        bus_key.0,
                        error.error(),
                        error.debug()
                    );
                    bus_healthy.store(false, Ordering::Relaxed);
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

        if let Some(caps) = self.shared.caps.lock().unwrap().clone() {
            appsrc.set_caps(Some(&caps));
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
}

impl Drop for DeviceHub {
    fn drop(&mut self) {
        log::info!("capture hub: closing {}", self.key.0);
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }
        let _ = self.pipeline.set_state(gstreamer::State::Null);
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
        *slot = None;
    }

    let hub = DeviceHub::open(key, make_source()?)?;
    let consumer = hub.add_consumer();
    *slot = Some(hub);
    consumer
}
```

- [ ] **Step 4: 테스트 통과 확인**

```powershell
cargo test -p servo-media-gstreamer --lib capture_hub
```

Expected: PASS — `the_same_key_opens_the_device_once`, `different_keys_open_separate_hubs`, `concurrent_opens_of_the_same_key_open_once`, `a_source_that_cannot_be_built_yields_no_consumer` 4건.

- [ ] **Step 5: 서식 + 커밋**

```powershell
rustfmt --edition 2024 --check components/media/backends/gstreamer/capture_hub.rs components/media/backends/gstreamer/lib.rs
git diff --check
git add components/media/backends/gstreamer/capture_hub.rs components/media/backends/gstreamer/lib.rs
git commit -m "Open a capture port once and keep it"
```

---

### Task 3: 프레임 배포 — 소비자 여럿, 서로 독립

**Files:**
- Modify: `components/media/backends/gstreamer/capture_hub.rs` (테스트 모듈만 확장; 구현은 Task 2 에서 이미 들어갔다)
- Test: 같은 파일

**Interfaces:**
- Consumes: Task 2 의 `open_consumer_with`, `CaptureConsumer::source_element`, `CaptureConsumer::hub`, `DeviceHub::consumer_count`, `DeviceHub::is_playing`
- Produces: 테스트 헬퍼 `ConsumerSink` (`ConsumerSink::start(&CaptureConsumer, playing: bool) -> ConsumerSink`, `ConsumerSink::count(&self) -> usize`) 와 `wait_until(what: &str, condition: impl FnMut() -> bool)` — Task 4 의 테스트가 그대로 쓴다.

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`capture_hub.rs` 의 `mod tests` 안, 기존 헬퍼 아래에 추가:

```rust
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
```

- [ ] **Step 2: 테스트를 돌려 결과를 확인한다**

```powershell
cargo test -p servo-media-gstreamer --lib capture_hub
```

Expected: 배포 구현은 Task 2 에 이미 들어갔으므로 **PASS 가 정상이다.** 통과하면 Step 3 을 건너뛰고 Step 4 로 간다. 실패하면 Step 3 의 진단 순서를 따른다.

- [ ] **Step 3: 실패 시 진단 (이 순서대로)**

1. `wait_until` 타임아웃 + `count()` 가 0 이면 **caps 협상 실패**다. `$env:GST_DEBUG=3` 으로 다시 돌려 `not-negotiated` 를 확인한다. `appsrc` 가 caps 를 받기 전에 push 되면 이렇게 된다 — `add_consumer` 가 `shared.caps` 캐시를 즉시 반영하는지, `distribute` 가 caps 변화 시 **모든** 소비자에 설정하는지 본다.
2. `count()` 가 4 근처에서 멈추면 `leaky-type` 이 안 걸린 것이다. `set_enum_if_present` 의 warn 로그가 찍혔는지 본다.
3. `the stalled consumer was evicted` 로 실패하면 `push_buffer` 가 `Flushing` 이 아닌 에러를 돌려준 것이다. `distribute` 의 `Err(error)` 가지에서 이미 그 값을 로그로 찍으므로 `-- --nocapture` 로 확인하고, PAUSED 소비자에서 정상적으로 나오는 값이면 `retain` 의 허용 목록에 추가한다.

- [ ] **Step 4: 테스트 통과 확인**

```powershell
cargo test -p servo-media-gstreamer --lib capture_hub
```

Expected: PASS — Task 2 의 4건 + 이번 4건 = 8건.

- [ ] **Step 5: 서식 + 커밋**

```powershell
rustfmt --edition 2024 --check components/media/backends/gstreamer/capture_hub.rs
git diff --check
git add components/media/backends/gstreamer/capture_hub.rs
git commit -m "Pin down what the hub owes each consumer"
```

---

### Task 4: 장치가 죽으면(그때만) 다시 연다

**Files:**
- Modify: `components/media/backends/gstreamer/capture_hub.rs` (테스트 모듈)
- Test: 같은 파일

**Interfaces:**
- Consumes: Task 2 의 `DeviceHub::is_healthy` 와 `open_consumer_with` 의 unhealthy 재개방 분기, Task 3 의 `ConsumerSink`·`wait_until`
- Produces: 없음 (기존 동작을 테스트로 고정)

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`mod tests` 안에 추가:

```rust
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

        assert_eq!(opens.load(Ordering::Relaxed), 1, "the failed hub was reused");
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
```

- [ ] **Step 2: 테스트를 돌려 결과를 확인한다**

```powershell
cargo test -p servo-media-gstreamer --lib capture_hub
```

Expected: 재개방 분기는 Task 2 에 이미 있으므로 PASS 가 정상이다. `a_failed_device_is_reopened_on_the_next_request` 가 `timed out waiting for the hub to notice the device error` 로 실패하면 **버스 메시지가 기대한 형태가 아닌 것**이다 — Step 3 으로 간다.

- [ ] **Step 3: 실패 시 수정**

`DeviceHub::open` 의 sync handler 를 잠시 아래로 바꿔 어떤 메시지가 오는지 본다:

```rust
            bus.set_sync_handler(move |_, message| {
                log::warn!("capture hub bus: {:?}", message.view());
                gstreamer::BusSyncReply::Drop
            });
```

```powershell
$env:RUST_LOG = "servo_media_gstreamer=warn"
cargo test -p servo-media-gstreamer --lib a_failed_device -- --nocapture
```

확인한 뒤 핸들러를 원래 형태로 되돌리고, `MessageView` 분기를 실제로 오는 메시지에 맞춘다. `identity error-after` 가 `Error` 를 안 올리면 대신 `videotestsrc` + `num-buffers=3` 으로 EOS 를 내고 `MessageView::Eos` 도 unhealthy 로 취급한다.

- [ ] **Step 4: 테스트 통과 확인**

```powershell
cargo test -p servo-media-gstreamer --lib capture_hub
```

Expected: PASS — 총 10건.

- [ ] **Step 5: 서식 + 커밋**

```powershell
rustfmt --edition 2024 --check components/media/backends/gstreamer/capture_hub.rs
git diff --check
git add components/media/backends/gstreamer/capture_hub.rs
git commit -m "Reopen a capture port only when the device itself died"
```

---

### Task 5: getUserMedia 의 비디오를 허브로 보낸다

**Files:**
- Modify: `components/media/backends/gstreamer/media_capture.rs:106-145` (`GstMediaDevices::get_track`), `:216-231` (`create_input_stream`)
- Modify: `components/media/backends/gstreamer/media_stream.rs:66-72` (구조체), `:88-96` (`new`), `:246-273` (`create_video_from`), `:320-326` (`Drop`)
- Test: `components/media/backends/gstreamer/media_stream.rs` 의 새 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `crate::capture_hub::{CaptureConsumer, open_video_consumer}`, Task 1 의 재진입 안전한 `unregister_stream`
- Produces:
  - `GstMediaDevices::get_device(&self, video: bool, constraints: MediaTrackConstraintSet) -> Option<gstreamer::Device>`
  - `GStreamerMediaStream::create_video_from_with(source: gstreamer::Element, capture_consumer: Option<CaptureConsumer>) -> MediaStreamId`
  - `GStreamerMediaStream::create_video_from(source)` 는 시그니처·동작 불변(내부적으로 `create_video_from_with(source, None)`)

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`components/media/backends/gstreamer/media_stream.rs` 끝에 추가:

```rust
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
}
```

- [ ] **Step 2: 테스트가 실패하는 것을 확인한다**

```powershell
cargo test -p servo-media-gstreamer --lib media_stream
```

Expected: FAIL — `create_video_from_with` 가 없다.

- [ ] **Step 3: 구현**

3-1. `media_stream.rs` 상단 import 에 추가:

```rust
use crate::capture_hub::CaptureConsumer;
```

3-2. 구조체에 필드를 추가한다:

```rust
pub struct GStreamerMediaStream {
    id: Option<MediaStreamId>,
    type_: MediaStreamType,
    elements: Vec<gstreamer::Element>,
    pipeline: Option<gstreamer::Pipeline>,
    /// 공유 캡처 허브에서의 등록. 캡처 장치가 먹이는 스트림에만 있다.
    /// 이걸 떨어뜨리면 이 스트림의 appsrc 만 허브 명단에서 빠진다 —
    /// 장치는 계속 열려 있다.
    capture_consumer: Option<CaptureConsumer>,
}
```

`new()` 의 초기화에 `capture_consumer: None,` 을 추가한다.

3-3. `create_video_from` 을 갈라서 쓴다 (기존 본문과 주석은 그대로 옮긴다):

```rust
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
```

3-4. `Drop` 을 교체한다:

```rust
impl Drop for GStreamerMediaStream {
    fn drop(&mut self) {
        // 허브 등록을 먼저 놓는다 — 곧 NULL 로 갈 파이프라인에 프레임이
        // 더 들어오지 않게.
        self.capture_consumer = None;
        if let Some(pipeline) = self.pipeline.take() {
            if let Err(error) = pipeline.set_state(gstreamer::State::Null) {
                log::warn!("could not stop a media stream pipeline: {error}");
            }
        }
        if let Some(ref id) = self.id {
            unregister_stream(id);
        }
    }
}
```

3-5. `media_capture.rs` 에서 장치 선택과 element 생성을 가른다. `GstMediaDevices::get_track` 을 아래 둘로 바꾼다 (본문은 기존 `get_track` 에서 그대로 옮기고, 마지막 두 줄만 나눈다):

```rust
    /// 제약을 만족하는 `GstDevice` 를 고른다. element 는 만들지 않는다 —
    /// 비디오 입력은 캡처 허브가 대신 열기 때문이다.
    pub fn get_device(
        &self,
        video: bool,
        mut constraints: MediaTrackConstraintSet,
    ) -> Option<gstreamer::Device> {
        let device_id = constraints.device_id.take();
        let (format, filter) = if video {
            ("video/x-raw", "Video/Source")
        } else {
            ("audio/x-raw", "Audio/Source")
        };
        let caps = into_caps(constraints, format)?;
        let f = self.monitor.add_filter(Some(filter), Some(&caps));
        let devices = self.monitor.devices();
        if let Some(f) = f {
            let _ = self.monitor.remove_filter(f);
        }
        match &device_id {
            Some(requested) => {
                let (ConstrainString::Exact(requested_id) | ConstrainString::Ideal(requested_id)) =
                    requested;
                select_device_by_id(devices.iter(), requested_id)
            },
            None => devices.front().cloned(),
        }
    }

    pub fn get_track(
        &self,
        video: bool,
        constraints: MediaTrackConstraintSet,
    ) -> Option<GstMediaTrack> {
        let device = self.get_device(video, constraints)?;
        let element = device.create_element(None).ok()?;
        Some(GstMediaTrack { element })
    }
```

3-6. `media_capture.rs` 의 import 에 추가하고 `create_input_stream` 을 교체한다:

```rust
use crate::capture_hub::open_video_consumer;
```

```rust
fn create_input_stream(
    stream_type: MediaStreamType,
    constraint_set: MediaTrackConstraintSet,
) -> Option<MediaStreamId> {
    let devices = GstMediaDevices::new();
    match stream_type {
        // 비디오 입력만 허브를 거친다. 같은 포트를 두 번 여는 것이 불안정한
        // 것은 캡처카드 쪽 제약이고, 오디오 입력에는 그 제약이 없다.
        MediaStreamType::Video => {
            let device = devices.get_device(true, constraint_set)?;
            let consumer = open_video_consumer(&device)?;
            let source = consumer.source_element();
            Some(GStreamerMediaStream::create_video_from_with(
                source,
                Some(consumer),
            ))
        },
        MediaStreamType::Audio => {
            let track = devices.get_track(false, constraint_set)?;
            Some(GStreamerMediaStream::create_audio_from(track.element))
        },
    }
}
```

- [ ] **Step 4: 테스트 통과 확인**

```powershell
cargo test -p servo-media-gstreamer --lib
```

Expected: PASS — device_id 6건 + capture_hub 10건 + media_stream 2건 = 18건.

- [ ] **Step 5: 서식 + 커밋**

```powershell
rustfmt --edition 2024 --check components/media/backends/gstreamer/media_stream.rs components/media/backends/gstreamer/media_capture.rs
git diff --check
git add components/media/backends/gstreamer/media_stream.rs components/media/backends/gstreamer/media_capture.rs
git commit -m "Feed getUserMedia video from the shared capture hub"
```

---

### Task 6: MediaStreamTrack.stop()

**Files:**
- Modify: `components/script_bindings/webidls/MediaStreamTrack.webidl:18`
- Modify: `components/script/dom/media/mediastreamtrack.rs`

**Interfaces:**
- Consumes: Task 1 의 재진입 안전한 `unregister_stream`
- Produces: `MediaStreamTrackMethods::Stop(&self)` — 페이지가 명시적으로 트랙을 놓을 수 있게 된다. Task 8 의 프로브 페이지가 쓴다.

- [ ] **Step 1: WebIDL 에 노출한다**

`components/script_bindings/webidls/MediaStreamTrack.webidl` 의

```
    // void stop();
```

를

```
    undefined stop();
```

으로 바꾼다.

- [ ] **Step 2: 빌드가 실패하는 것을 확인한다**

```powershell
cargo check -p script
```

Expected: FAIL — `MediaStreamTrackMethods` 의 `Stop` 이 구현되지 않았다는 에러.

> `cargo check -p script` 가 WebIDL codegen 을 돌리지 않아 에러가 안 나면 `.\mach build -j 8` 로 확인한다.

- [ ] **Step 3: 구현**

`components/script/dom/media/mediastreamtrack.rs` 의 import 에 추가:

```rust
use std::cell::Cell;

use servo_media::streams::registry::unregister_stream;
```

구조체에 필드를 추가한다:

```rust
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
```

`new_inherited` 에 `ended: Cell::new(false),` 를 추가한다.

`impl MediaStreamTrackMethods<crate::DomTypeHolder> for MediaStreamTrack` 안에 추가:

```rust
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
```

- [ ] **Step 4: 빌드 통과 확인**

```powershell
cargo check -p script
```

Expected: PASS.

- [ ] **Step 5: 서식 + 커밋**

```powershell
rustfmt --edition 2024 --check components/script/dom/media/mediastreamtrack.rs
git diff --check
git add components/script_bindings/webidls/MediaStreamTrack.webidl components/script/dom/media/mediastreamtrack.rs
git commit -m "Let a page stop a capture track"
```

---

### Task 7: 파이프라인이 끝나면 캡처 스트림을 놓는다

**왜 필요한가:** `stop()` 만으로는 부족하다. 페이지는 대개 stop 을 부르지 않고, 지금은 문서가 사라져도 스트림이 레지스트리에 영원히 남는다. 페이지 전환에서 결정적으로 불리는 훅에 해제를 건다.

**Files:**
- Modify: `components/script/dom/globalscope.rs:211-249` (구조체), `:803-813` (초기화), `impl GlobalScope` 에 메서드 2개 추가
- Modify: `components/script/dom/media/mediadevices.rs:66-80` (`GetUserMedia`), `:95-108` (`GetDisplayMedia`)
- Modify: `components/script/dom/window.rs:2470-2472` (`clear_js_runtime` 진입부)

**Interfaces:**
- Consumes: Task 1 의 재진입 안전한 `unregister_stream`
- Produces:
  - `GlobalScope::track_capture_stream(&self, id: MediaStreamId)`
  - `GlobalScope::release_capture_streams(&self)`

- [ ] **Step 1: GlobalScope 에 목록을 단다**

`components/script/dom/globalscope.rs` 의 import 에 추가:

```rust
use servo_media::streams::registry::{MediaStreamId, unregister_stream};
```

구조체의 `pipeline_id` 필드 바로 아래에 추가:

```rust
    /// 이 global 이 `getUserMedia`/`getDisplayMedia` 로 만든 캡처 스트림들.
    /// 스트림 레지스트리는 스트림을 강참조로 붙잡으므로, 여기서 명시적으로
    /// 놓아주지 않으면 문서가 사라진 뒤에도 캡처 장치가 계속 물려 있다.
    #[no_trace]
    capture_streams: DomRefCell<Vec<MediaStreamId>>,
```

`new_inherited` 초기화 목록의 `pipeline_id,` 아래에 추가:

```rust
            capture_streams: DomRefCell::new(Vec::new()),
```

- [ ] **Step 2: 등록/해제 메서드를 더한다**

`impl GlobalScope` 안(다른 공개 메서드들 곁)에 추가:

```rust
    /// `getUserMedia`/`getDisplayMedia` 가 만든 스트림을 이 global 소유로 기록한다.
    pub(crate) fn track_capture_stream(&self, id: MediaStreamId) {
        self.capture_streams.borrow_mut().push(id);
    }

    /// 이 global 이 만든 캡처 스트림을 전부 놓는다. 파이프라인 종료 시 불린다.
    /// 레지스트리 항목이 유일한 소유자이므로, 이게 캡처 소비자를 실제로 닫는
    /// 유일한 지점이다.
    pub(crate) fn release_capture_streams(&self) {
        // borrow 를 놓고 나서 해제한다 — unregister_stream 은 스트림의 Drop 을
        // 부르고, 그 Drop 이 무엇을 건드릴지 여기서 가정하지 않는다.
        let ids = std::mem::take(&mut *self.capture_streams.borrow_mut());
        for id in ids {
            unregister_stream(&id);
        }
    }
```

- [ ] **Step 3: 생성 지점에서 기록한다**

`components/script/dom/media/mediadevices.rs` 의 `GetUserMedia` 를 아래처럼 만든다:

```rust
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
```

`GetDisplayMedia` 에서도 같게:

```rust
            if let Some(video) = media.create_display_stream(source, video_constraints) {
                self.global().track_capture_stream(video);
                let track =
                    MediaStreamTrack::new(cx, &self.global(), video, MediaStreamType::Video);
                stream.add_track(&track);
            }
```

- [ ] **Step 4: 파이프라인 종료에 건다**

`components/script/dom/window.rs` 의 `clear_js_runtime` 첫 줄로 넣는다:

```rust
    pub(crate) fn clear_js_runtime(&self) {
        // 캡처 스트림을 먼저 놓는다. 문서가 사라진 뒤에도 캡처 장치를 물고
        // 있으면, 다음 페이지가 같은 포트를 열면서 카드가 무너진다.
        self.as_global_scope().release_capture_streams();

        self.as_global_scope()
            .remove_web_messaging_and_dedicated_workers_infra();
```

- [ ] **Step 5: 빌드 통과 확인**

```powershell
cargo check -p script
```

Expected: PASS.

- [ ] **Step 6: 서식 + 커밋**

```powershell
rustfmt --edition 2024 --check components/script/dom/globalscope.rs components/script/dom/media/mediadevices.rs components/script/dom/window.rs
git diff --check
git add components/script/dom/globalscope.rs components/script/dom/media/mediadevices.rs components/script/dom/window.rs
git commit -m "Release a document's capture streams when its pipeline exits"
```

---

### Task 8: 실기 검증 도구와 문서, 그리고 전체 빌드

**Files:**
- Modify: `tests/html/multigpu_capture_card_probe.html`
- Create: `docs/multigpu/capture_card_shared_connection.md`

**Interfaces:**
- Consumes: Task 6 의 `track.stop()`, Task 2·3 의 `capture hub:` 로그
- Produces: 테스트 장비에서 돌릴 수 있는 재현 절차

- [ ] **Step 1: 프로브 페이지에 전환 반복과 stop 을 넣는다**

`tests/html/multigpu_capture_card_probe.html` 의 컨트롤 영역에 아래 블록을 추가한다 (기존 장치 선택 UI 와 `?device=` 처리는 그대로 둔다):

```html
<div class="controls">
  <button id="stop-track">stop() the current track</button>
  <label>reload cycles <input id="cycle-count" type="number" value="20" min="1" max="200"></label>
  <button id="start-cycles">reload N times</button>
  <span id="cycle-status"></span>
</div>
<script>
  // 페이지 전환을 N회 반복한다. 카운터는 sessionStorage 에 두어 리로드를 넘어간다.
  const CYCLE_KEY = 'captureCardCycleRemaining';

  document.getElementById('stop-track').addEventListener('click', () => {
    const stream = window.currentStream;
    if (!stream) { return; }
    for (const track of stream.getTracks()) { track.stop(); }
    document.getElementById('cycle-status').textContent = 'stopped';
  });

  document.getElementById('start-cycles').addEventListener('click', () => {
    const count = parseInt(document.getElementById('cycle-count').value, 10);
    sessionStorage.setItem(CYCLE_KEY, String(count));
    location.reload();
  });

  const remaining = parseInt(sessionStorage.getItem(CYCLE_KEY) || '0', 10);
  if (remaining > 0) {
    document.getElementById('cycle-status').textContent = `cycles left: ${remaining}`;
    sessionStorage.setItem(CYCLE_KEY, String(remaining - 1));
    // 스트림이 실제로 붙을 시간을 준 뒤 다음 전환으로 넘어간다.
    setTimeout(() => location.reload(), 2000);
  }
</script>
```

기존 `getUserMedia` 성공 처리부에서 얻은 스트림을 `window.currentStream` 에 대입하는 한 줄을 추가한다(변수명은 그 파일의 기존 것에 맞춘다).

- [ ] **Step 2: 전체 빌드**

```powershell
.\mach build -j 8
```

Expected: 성공. **`cargo build -p servoshell` 로 대체하지 않는다** — 미디어 백엔드가 더미로 빠진다.

- [ ] **Step 3: 전체 테스트와 서식**

```powershell
cargo test -p servo-media-gstreamer --lib
cargo test -p servo-media-streams --lib
cargo test -p servo-paint-api wall_layout --lib
git diff --check
```

Expected: 전부 PASS (월 레이아웃 13건 회귀 없음 포함).

- [ ] **Step 4: 운용 문서를 쓴다**

`docs/multigpu/capture_card_shared_connection.md` 를 만들고 아래를 담는다:

- 왜 포트당 하나인지 (2026-08-20 실측: 같은 포트 동시 K개 열기 — K=1 5/5, K=2 4/5, K=3 0/5).
- 무엇이 바뀌었는지: 허브가 지연 개방 후 프로세스 종료까지 유지, `getUserMedia` 는 `appsrc` 로 합류, 소비자 소멸 시 캡처 파이프라인 상태 전이 0.
- 확인용 로그와 그 의미:
  - `capture hub: opened <key>` — **포트당 정확히 한 줄.** 두 줄 이상이면 장치가 다시 열린 것이고, 그 앞에 반드시 `is unhealthy; reopening` 이 있어야 한다.
  - `capture hub: reused <key>` / `consumer … added|removed (consumers=N)` — 전환 후 N 이 다시 1 로 돌아와야 한다.
- 로그를 보려면 `RUST_LOG=servo_media_gstreamer=info` 가 필요하다 (`script=info` 만으로는 안 잡힌다).
- 실기 절차(아래 Step 6).
- 알려진 한계: 오디오 입력과 `getDisplayMedia` 는 허브를 쓰지 않는다(수명 해제만 적용). `MediaStreamTrack.clone()` 이 id 를 공유해 클론 하나를 stop 하면 전부 멈춘다.

- [ ] **Step 5: 커밋**

```powershell
git diff --check
git add tests/html/multigpu_capture_card_probe.html docs/multigpu/capture_card_shared_connection.md
git commit -m "Give the capture probe a way to reproduce page transitions"
```

- [ ] **Step 6: 실기 검증 (테스트 장비 — 개발기에서는 못 한다)**

```powershell
$env:RUST_LOG = "servo_media_gstreamer=info,script=info"
target\debug\servoshell.exe tests/html/multigpu_capture_card_probe.html 2> capture_cycles.err.log
```

프로브에서 `reload N times` 를 20으로 돌린 뒤 **창을 닫아서** 종료한다(강제 종료하면 stderr 가 안 비워져 로그가 빈다). 그다음 확인:

```powershell
Select-String -Path capture_cycles.err.log -Pattern "capture hub: opened"       # 포트당 1줄
Select-String -Path capture_cycles.err.log -Pattern "consumers="                # 마지막이 0
Select-String -Path capture_cycles.err.log -Pattern "panicked|not-negotiated"   # 0건
```

월 팬아웃에서도 같은 절차로 한 번 더:

```powershell
target\debug\servoshell.exe --wall-layout ..\config\wall_layout.local_3x1.json --wall-all-tiles tests/html/multigpu_capture_card_probe.html 2> capture_wall.err.log
```

---

## Self-Review

**스펙 커버리지**

| 스펙 절 | 담당 태스크 |
|---|---|
| §1 허브 구조·키·파이프라인·소비자 체인 | 2 |
| §1 적용 범위(비디오만) | 5 (`create_input_stream` 의 match) |
| §2 배포 스레드·shallow copy·타임스탬프·드롭 정책 | 2(구현) · 3(테스트) |
| §2 `leaky-type` 패닉 가드 | 2 (`set_enum_if_present`) |
| §3(a) `stop()` | 6 |
| §3(b) 파이프라인 종료 시 해제 | 7 |
| §3(c) Drop 이 실제로 돌게 | 1 (교착 제거) · 5 (Drop 확장) |
| §4 개방 실패 / 버스 ERROR / 동시 개방 직렬화 | 2(구현·동시 개방 테스트) · 4(실패 재개방) |
| §5 진단 로그 | 2·3 (로그) · 8 (해석 문서) |
| §6 검증 | 2·3·4·5 (자동) · 8 (빌드·서식·실기) |
| 비목표 | 8 문서의 "알려진 한계" |

**스펙과 다르게 간 것**

- 스펙 §5 의 "5초마다 배포 통계 1줄"은 넣지 않았다. 개방/재사용/소비자 증감 로그만으로 이 작업의 성공·실패가 판정되고, 주기 로그는 이 저장소가 이미 겪은 "로그 홍수로 실행 자체가 무효가 되는" 사고(초당 27,000줄, 60초에 150MB)의 재발 위험만 더한다. 배포량이 궁금해지면 그때 별도로 붙인다.

**타입 일관성 확인**

- `HubKey` 는 2 에서 정의하고 3·4·5 테스트에서 `HubKey::for_test` 로만 쓴다.
- `CaptureConsumer::source_element() -> gstreamer::Element` 는 5 의 `create_video_from_with(source, Some(consumer))` 호출과 맞는다 — `source_element` 가 clone 을 돌려주므로 `consumer` 를 이어서 move 할 수 있다.
- `DeviceHub::consumer_count`/`is_playing`/`CaptureConsumer::hub` 는 전부 `#[cfg(test)] pub(crate)` 라 5 의 `media_stream.rs` 테스트에서 보인다. `HubKey::for_test` 도 마찬가지다.
- `open_consumer_with` 의 `make_source: impl FnOnce() -> Option<gstreamer::Element>` 는 3·4·5 의 `test_source`/`failing_source`(함수 포인터)와 2 의 `counting_source`(클로저) 양쪽에 맞는다.
- `unregister_stream(&MediaStreamId)` 시그니처는 1 에서 바뀌지 않으므로 5·6·7 의 호출부가 전부 유효하다.

**주의로 남기는 것**

- 이 워크트리의 `cargo test` 프로세스가 로드하는 GStreamer 는 **1.26.8 (Nuricon build 101)** 이고 빌드 링크는 `PKG_CONFIG_PATH` 상 1.28.4.100 을 가리킨다(실측 확인). `leaky-type`/`max-buffers` 는 양쪽 다 있지만, 유무를 가정하지 말고 `set_*_if_present` 를 유지한다.
- Task 3 과 4 의 테스트는 Task 2 구현으로 이미 통과할 수 있다. 그건 실패가 아니다 — 각 태스크의 Step 2 에 "통과하면 Step 3 을 건너뛴다"를 명시해 두었다.
- Task 2·3·4 의 테스트는 전역 `SLOTS` 를 공유하고 병렬 실행된다. **테스트마다 고유한 `HubKey::for_test` 이름을 쓸 것** — 이름이 겹치면 서로의 허브를 재사용해 개방 횟수 단언이 무너진다.

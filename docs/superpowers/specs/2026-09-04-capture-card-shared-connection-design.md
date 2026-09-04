# 캡처카드 단일 연결 공유 설계 (2026-09-04)

대상 워크트리: `servo_multigpu-tiled-wall`, 브랜치 `capture-card-shared-connection`
(베이스 `multigpu-wall-pacing`). 런타임 GStreamer = 사내 커스텀 1.26.8/1.28.4.100.

## 목표

1. 인식된 캡처카드 포트당 **물리 연결(`ksvideosrc`)을 프로세스 전체에서 정확히 1개**로 유지한다.
2. `getUserMedia()` 는 그 연결을 새로 열지 않고, **프레임을 배포하는 스레드로부터 수신**한다.
3. 캡처카드를 표출하는 페이지를 전환할 때 발생하는 간헐적 크래시를 없앤다.

## 배경 — 크래시의 구조적 원인

`components/media/streams/registry.rs`

```rust
static MEDIA_STREAMS_REGISTRY: LazyLock<Mutex<HashMap<MediaStreamId, Arc<Mutex<dyn MediaStream>>>>>
pub fn register_stream(stream: Arc<Mutex<dyn MediaStream>>) -> MediaStreamId { ... insert ... }
pub fn unregister_stream(stream: &MediaStreamId) { ... remove ... }
```

- 레지스트리가 `Arc` 를 **강참조로** 보관한다.
- 그 `Arc` 를 떨어뜨리는 유일한 경로는 `GStreamerMediaStream::drop` → `unregister_stream`
  (`media_stream.rs:322`) 인데, **레지스트리가 그 `Arc` 를 쥐고 있으므로 `drop` 이 절대 실행되지
  않는다.** 자기참조 사이클이다.
- `MediaStreamTrack.stop()` 은 WebIDL 에서 주석 처리(`MediaStreamTrack.webidl:18`)되어 미구현이고,
  문서 폐기 시 스트림을 해제하는 경로도 없다.

결과: **페이지 전환마다 같은 포트에 `ksvideosrc` 가 하나씩 더 열리고, 기존 것은 `Playing` 인 채로
남는다.** 2026-08-20 `gst-launch` 로 실측한 포트 동시접속 한계
(같은 포트를 K개 동시에 열었을 때 K=1 은 5/5 성공, K=2 는 4/5, **K=3 은 0/5 — 매번 1개만 살아남음**)와
정확히 맞물려, 전환 2~3회째에 간헐적으로 무너지는 관측 증상의 형태와 일치한다.

> 확증 수준: 위의 코드 사실(레지스트리 사이클, `stop()` 미구현, 해제 훅 부재)은 **읽어서 확인한
> 것**이다. "이것이 관측된 크래시의 원인"이라는 인과는 실기 재현으로 확증하지 않은 **강한 정황**이다
> (개발기에 캡처카드가 없다). 설계는 원인 확정 여부와 무관하게 이 구조를 제거한다.

## 사용자 결정 사항

- 범위: 허브 + **수명 해제까지** (브랜치 누적으로 인한 CPU 누수도 함께 제거).
- 연결 수명: **한 번 열면 프로세스 종료까지 유지**. 소비자 0 이어도 닫지 않는다.
- 개방 시점: **지연 개방** — 그 포트를 처음 요청할 때 연다. 기동 시 선개방하지 않는다.
- 배포 방식: **appsink + Rust 배포 + 소비자별 appsrc** (아래 §대안 검토의 B안).

## 대안 검토

| 안 | 요지 | 판정 |
|---|---|---|
| A. `tee` + 소비자마다 `queue ! proxysink` | GStreamer 관용구, 코드 최소 | **기각** — 소비자 소멸 시 **살아있는 tee 에서 pad 를 떼는 수술**(pad probe 블록 → unlink → `release_request_pad`)이 필요하다. 이 저장소가 반복해서 데인 "라이브 파이프라인 teardown" 경로(RTSP teardown, 창닫기 12초+ 행업)를 전환마다 밟게 된다. |
| **B. `appsink` + 소비자별 `appsrc`** | 허브가 샘플을 뽑아 등록된 소비자에게 push | **채택** — 소비자 제거가 `Vec` 에서 빼는 것뿐이라 **라이브 캡처 파이프라인의 상태 전이가 0** 이다. 소비자별 드롭 정책이 명시적이다. |
| C. 같은 장치 요청에 같은 `MediaStreamId` 재사용 | 가장 저렴 | **기각** — 문서 간에 살아있는 파이프라인을 공유하게 되고, `set_stream` 이 호출마다 proxysink 를 덧붙인다. |

## 설계

### 1. 허브 — `components/media/backends/gstreamer/capture_hub.rs` (신규)

```
static CAPTURE_HUBS: LazyLock<Mutex<HashMap<HubKey, HubSlot>>>
HubKey = (kind, normalized_port_key(device_path))
```

- 키 도출은 기존 `device_id.rs::normalized_port_key` 를 재사용한다 — ks/mf 쌍둥이가 같은 키로 접힌다.
  `device.path` 가 없는 장치는 `display_name` 폴백(`media_capture.rs::select_device_by_id` 의 3티어와 동일 규칙).
- 허브 1개 = 물리 포트 1개 = 파이프라인 1개:

```
<device src> ! queue(leaky=downstream, max-buffers=2)
             ! videoconvert ! capsfilter(video/x-raw,format=I420)
             ! appsink(sync=false, drop=true, max-buffers=1)
```

`queue` 가 KS 캡처 스레드와 변환 스레드를 분리해 변환 지터가 장치에 닿지 않게 한다.
**변환이 허브에서 1회만 일어나는 것이 현행 대비 CPU 이득**이다(현행은 getUserMedia 마다 자기
`videoconvert` 를 갖는다).

- 소비자 1개 = `getUserMedia()` 호출 1개:

```
appsrc(is-live=true, format=Time, do-timestamp=true,
       max-buffers=4, leaky-type=downstream)
             ! videoconvert ! capsfilter(I420) ! queue     ← 기존 create_video_from 체인 그대로
```

`create_videoinput_stream` 의 변경점은 한 곳이다: `device.create_element()` 결과 대신 **허브가 발급한
`appsrc`** 를 `GStreamerMediaStream::create_video_from` 에 넘긴다. 그 아래 스택
(`servomediastreamsrc`, proxysink/proxysrc, 플레이어)은 무변경이다.

**적용 범위**: 허브는 **비디오 입력에만** 적용한다. 오디오 입력은 이 장비에서 배타성 문제가 없고
(현 빌드는 `audioinput` 0개) 검증이 불가능하므로 현행 경로를 유지한다. `getDisplayMedia` 도 포트
배타성 문제가 없어 허브 대상이 아니다. **단 §3 수명 해제는 오디오·`getDisplayMedia` 스트림에도 동일
적용된다.** 허브를 오디오로 확장하는 것은 `kind` 파라미터 추가만으로 되도록 구조를 잡는다.

### 2. 배포 스레드와 드롭 정책

`appsink` 의 `new_sample` 콜백이 배포 스레드다(= 허브 스트리밍 스레드). 하는 일:

1. 샘플 caps 가 직전과 다르면 모든 소비자 `appsrc` 에 새 caps 를 설정하고 허브에 캐시한다
   (신규 소비자는 등록 즉시 이 캐시로 caps 를 받는다).
2. 소비자마다 `buffer.copy()` — **shallow copy**(메모리 공유, `GstBuffer` 헤더만 신규). 픽셀 복사 없음.
3. 복사본의 PTS/DTS 를 비우고 push. 소비자 `appsrc` 가 `do-timestamp=true` 로 자기 파이프라인
   running time 에 맞춰 다시 찍는다 — **소비자가 한참 뒤에 합류해도** 타임스탬프가 튀어 sink 가
   전부 버리는 함정을 피한다.
4. push 결과가 `Flushing`(소비자 파이프라인이 아직 NULL)이면 조용히 무시, `Eos` 면 그 소비자를
   명단에서 제거한다.

**어느 단계도 블록하지 않는다**: `appsink drop=true` + `appsrc block=false, leaky-type=downstream`.
느린 타일이나 죽어가는 소비자가 캡처 장치를 막을 수 없다. 부수효과로 현행의 "소비자 처리율이
캡처단을 backpressure 해서 표시 fps 가 깎이던" 동작도 사라진다.

> `leaky-type` 은 GStreamer 1.20+ 프로퍼티다. 런타임(1.26.8/1.28.4)에는 있으나 **`set_property` 는
> 없는 프로퍼티에 패닉**하므로(WGC `window-handle` 로 이미 데인 함정) `has_property` 확인 후 설정하고,
> 없으면 `warn` 만 남기고 진행한다.

### 3. 수명 — 소비자는 사라지고 장치는 남는다

**(a) `MediaStreamTrack.stop()` 구현.** WebIDL 주석 해제 + `ended` 플래그 + `unregister_stream(id)`.
이미 `ended` 면 no-op.

> 선재 결함 기록(이번 범위 밖): 이 엔진의 `MediaStreamTrack::clone()` 은 **같은 `MediaStreamId` 를
> 공유**한다(`mediastreamtrack.rs:78`). 따라서 클론 하나를 `stop()` 하면 전부 멈춘다. 스펙 위반이지만
> 고치려면 트랙별 독립 스트림 복제가 필요해 별건으로 둔다.

**(b) 파이프라인 종료 시 자동 해제.** `GlobalScope` 에 `capture_streams: DomRefCell<Vec<MediaStreamId>>`
를 추가하고 `GetUserMedia`/`GetDisplayMedia` 가 만든 id 를 기록한다. `Window::clear_js_runtime()`
(`window.rs:2470`, `script_thread.rs:3234` 의 `handle_exit_pipeline_msg` 에서 호출)에서 전부
`unregister_stream` 한다. **페이지 전환 시 결정적으로 불리는 훅**이라 GC 를 기다리지 않는다.

**(c) 실제로 `Drop` 이 돌게 하기.** (a)/(b) 로 레지스트리에서 빠지면 refcount 가 0 이 되어
`GStreamerMediaStream::drop` 이 실행된다. 거기서:

- 소비자 파이프라인을 `State::Null` 로 내린다(장치가 없는 파이프라인이라 즉시 끝난다),
- 새로 추가하는 `_consumer: Option<CaptureConsumer>` 핸들이 drop 되면서 **허브 명단에서 자기
  `appsrc` 만 빠진다.**

허브 파이프라인은 이 과정에서 **상태 전이가 전혀 없다.** 장치는 한 번 열리고 프로세스 종료까지 그대로다.

### 4. 에러 처리

- 허브 개방 실패(장치 없음/사용 중) → `create_videoinput_stream` 이 `None`,
  `getUserMedia` 는 트랙 없는 스트림 반환(현행 동작 유지) + 원인 로그.
- 허브 버스 `ERROR`(장치 뽑힘 등) → 허브를 Failed 로 표시하고 레지스트리에서 제거·`Null` 로 내린다.
  **다음 요청이 새로 연다.** 장치를 닫는 유일한 경로가 "장치 자체가 죽었을 때"뿐이며, 페이지 전환은
  여기에 닿지 않는다.
- **동시 개방 경합 차단**: 같은 키의 개방이 겹치면 두 번째 호출자는 첫 번째가 끝날 때까지 기다렸다가
  그 결과를 공유한다(키별 슬롯 점유). 여기서 실수하면 이 작업의 목적 자체가 무너지므로 테스트로 고정한다.
  개방 중에는 전역 레지스트리 뮤텍스를 잡고 있지 않는다(느린 장치 개방이 다른 포트를 막지 않게).

### 5. 진단

`RUST_LOG=servo_media_gstreamer=info` 필요(이 백엔드 로그는 `script=info` 만으로는 안 잡힌다).

- `capture hub: opened port=… caps=…` — **포트당 정확히 한 줄**. 전환을 20회 해도 한 줄이면 성공 증거.
- `capture hub: reused port=… consumers=N` / `capture hub: consumer removed port=… consumers=N`
- 5초마다 배포 통계 1줄: 포트별 distributed/dropped, 소비자별 drop.

### 6. 검증

개발기에 캡처카드가 없고(그리고 이 개발기는 GPU 가 CLAUDE.md 기재와 달라 성능 측정 금지 대상이다)
**하드웨어 없이 도는 통합 테스트**를 축으로 삼는다. 허브의 소스 엘리먼트를 주입 가능하게 만들어
`videotestsrc` 로 열 수 있게 한다.

통합 테스트(TDD 로 먼저 작성):

1. 소비자 3개가 각각 버퍼를 받는다.
2. 소비자 1개를 drop 해도 나머지가 계속 받는다.
3. 소비자가 0이 되어도 허브 파이프라인은 `Playing` 을 유지한다.
4. 같은 키로 두 번 요청하면 **파이프라인이 하나**다(개방 1회).
5. 같은 키로 **동시에** 두 번 요청해도 개방은 1회다(§4 경합).
6. 소비자 하나를 굶겨도(버퍼를 안 빼가도) 다른 소비자의 수신이 멈추지 않는다.

단위 테스트: 허브 키 도출(ks/mf 정규화, path 없는 장치 폴백).

빌드/서식(미디어 변경이므로 `cargo -p` 가 아니라 **full `mach build`** — `cargo -p` 는 더미 백엔드로
빠지는 함정이 기록되어 있다):

```powershell
. .\scripts\servo_env.ps1
cd servo_multigpu-tiled-wall
.\mach build -j 8
rustfmt --edition 2024 --check <touched .rs>
git diff --check
```

실기(테스트 장비): `tests/html/multigpu_capture_card_probe.html` 에 "N회 자동 전환" 과
`track.stop()` 버튼을 추가하고, 20회 전환 후

- 크래시 없음,
- `capture hub: opened port=` 로그가 포트당 1줄,
- 전환 후 소비자 수가 다시 1로 돌아옴

을 확인한다.

## 비목표

- 오디오 입력 / `getDisplayMedia` 의 허브화(수명 해제만 적용).
- `MediaStreamTrack::clone()` 의 id 공유 결함 수정.
- GPU zero-copy 캡처 경로(gstgl EGL/ANGLE 필요 — 별건으로 보류 중).
- 캡처 프레임의 멀티 GPU 분배.

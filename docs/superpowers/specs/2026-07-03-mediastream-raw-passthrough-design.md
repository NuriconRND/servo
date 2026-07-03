# MediaStream 표시 경로 raw passthrough (WebRTC/카메라 재인코딩 제거) 설계

- 날짜: 2026-07-03
- 브랜치: `multigpu-tiled-wall` (기준 f89573bee)
- 범위: servo-media GStreamer 백엔드. `<video>`에 MediaStream(카메라/WebRTC 수신 트랙)을 표시할 때의 **불필요한 VP8/Opus 재인코딩 제거**.
- 관련: `[[presenter-webrtc-wall]]`, `[[servo-wall-webgl2-findings]]`(I420 borrowed 경로)

---

## 1. 문제 / 배경 (코드로 확정)

Servo가 MediaStream을 `<video>`로 표시할 때, 이미 raw인 프레임을 **VP8로 재인코딩했다가 다시 디코딩**한다. 결과: Chrome 대비 (1) 움직임 시 블록 아티팩트(재인코딩 세대손실), (2) ~0.3–1초 고정 지연(encode+decode 사이클 1개 추가).

**확정된 표시 경로 (재인코딩 위치):**
```
[네트워크 RTP VP8] → webrtcbin → decodebin → avdec_vp8            ← 디코드#1 (필요: RTP→raw)
  → proxysink/proxysrc → GStreamerMediaStream{Video} = [proxy_src, videoconvert, queue]  ← 이미 RAW
  → ServoMediaStreamSrc::set_stream → stream.encoded() = vp8enc→rtpvp8pay→queue→capsfilter  ← ★재인코딩(불필요)
  → proxysink → video_proxysrc(ghost pad, RTP VP8 caps)
  → playbin3 내부 decodebin3 → rtpvp8depay → avdec_vp8            ← ★디코드#2(불필요)
  → appsink(video/x-raw I420) → render.rs get_frame_from_sample → VideoFrame → <video>
```
디코드#1만 필요. **재인코딩 + 디코드#2는 100% 불필요한 로컬 오버헤드.**

### 코드 앵커
- `components/media/backends/gstreamer/media_stream_source.rs:55-92` `ServoMediaStreamSrc::set_stream` — line 71 `let last_element = stream.encoded()...` (재인코딩 호출).
- `components/media/backends/gstreamer/media_stream.rs:198-243` `encoded()` — Video: `src→vp8enc→rtpvp8pay→queue→capsfilter(RTP_CAPS_VP8)`; Audio: `src→opusenc→rtpopuspay→…(RTP_CAPS_OPUS)`.
- `media_stream.rs:123-125` `src_element()` = `elements.last()` = **이미 raw tail**(`create_video_from`의 `[source, videoconvert, queue]`의 `queue`).
- `media_stream.rs:245-254` `create_video_from`, `:267-281` `create_audio_from` — MediaStream이 raw 프레임 보유 확정.
- `media_stream_source.rs:23-41` ghost pad 템플릿(`AUDIO/VIDEO_SRC_PAD_TEMPLATE`) = 현재 `RTP_CAPS_OPUS`/`RTP_CAPS_VP8` 고정.
- `media_stream_source.rs:94-146` `setup_proxy_src`(타입-제네릭), `:186,195` proxysrc 생성.
- `components/media/backends/gstreamer/webrtc.rs:846-880` `on_incoming_decodebin_stream` — 수신 트랙은 decodebin(디코드#1) 뒤 **raw**로 `create_video_from`에 들어감(재인코딩 전에는 raw).
- `components/media/backends/gstreamer/webrtc.rs:522-587` `link_stream` — **send 경로**가 `stream.encoded()`(:554,569) 사용(webrtcbin sink로 RTP 송출, 유지 필요).
- `player.rs:791-916` `setup()` — `uri="mediastream://"` set만; `playbin3`/`decodebin3`가 내부 오토플러그(명시적 decodebin 안 만듦). `:798` 의존성 `["playbin3","decodebin3","queue"]`. `:896-911` `StreamType::Stream → mediastream://`. `:1198-1207` source-setup에서 `ServoMediaStreamSrc`로 캐스팅.
- 싱크: `render.rs:274-323 setup_video_sink`, `:301-320` appsink caps = **`video/x-raw, format=I420`**(비-GL, 이 Windows 빌드 활성). `player.rs:1066-1114` new-sample→`get_frame_from_sample`→borrowed I420 `VideoFrame`.
- 플러그인: `components/servo/gstreamer_plugin_lists/common.rs.in:41 "gstproxy"`(proxysink/proxysrc 번들 확인).

## 2. 목표 / 성공 기준

- MediaStream 표시 경로에서 **VP8 재인코딩 + 재디코딩(그리고 오디오 Opus 재인코딩) 제거** → raw 프레임이 그대로 플레이어→싱크로 흐름.
- **화질:** 재인코딩 세대손실 제거로 움직임 아티팩트 소멸(Chrome 수준 근접).
- **지연:** encode+decode 사이클 제거로 고정 지연 감소.
- **효율:** raw I420가 **추가 CPU 복사·CPU 색변환 없이** WebRender에 borrowed로 도달(YUV→RGB는 WebRender 셰이더). = 일반 `<video>` 파일과 동일한 최적 경로.
- **무회귀:** send 경로(webrtc.rs)·일반 `<video src=url>`·오디오 재생 정상.

## 3. 아키텍처 / 변경 (A안: raw passthrough)

핵심은 국소적 — **playbin3·decodebin3·appsink·render.rs·send 경로 전부 무변경**. 흐르는 caps만 RTP→raw로 바꾸면 decodebin3가 caps 기반 오토플러그로 **디코더를 스스로 생략**한다.

### 변경 3곳
1. **`media_stream.rs`: 신규 `raw()` 메서드** — `encoded()`는 손대지 않음(send와 공유).
   ```rust
   /// 표시(display) 경로용: 이미 raw인 tail 엘리먼트를 그대로 반환(재인코딩 없음).
   pub fn raw(&mut self) -> gstreamer::Element {
       self.src_element()
   }
   ```
2. **`media_stream_source.rs:71`: 표시 경로만** `stream.encoded()` → `stream.raw()`.
   - 브링업 A/B용으로 env 게이트 허용: `if raw_passthrough_enabled { stream.raw() } else { stream.encoded()? }` (env 기본 = raw; 검증 후 게이트 제거하고 무조건 raw로 확정).
   - audio/video 공통(호출부 하나, `setup_proxy_src`가 타입 분기).
3. **`media_stream_source.rs:23-41`: ghost pad 템플릿 caps** `RTP_CAPS_VP8`/`RTP_CAPS_OPUS` → `video/x-raw`/`audio/x-raw`(협상 여유 위해 `Caps::new_any()`도 후보). **decodebin3가 raw로 인식하려면 필수.**

### 데이터 흐름 (변경 후)
```
ServoMediaStreamSrc.video_src (video/x-raw I420)
  → playbin3/decodebin3: caps가 raw/terminal → 디코더 미삽입, multiqueue 버퍼링만 하고 노출
  → videoconvert 패스스루(I420==I420) → video-sink=appsink(I420)
  → new-sample → render.rs → borrowed I420 3평면 → WebRender(YUV→RGB 셰이더)
```

## 4. 효율 분석 (추가 복사·CPU 색변환 없음)

- VP8은 항상 **I420** 디코드; appsink는 **I420** 요청 → **I420 end-to-end**.
- proxysink↔proxysrc·queue = **GstBuffer refcount 참조 전달(데이터 복사 X)**. `render.rs`의 `plane_data()`는 gstreamer 버퍼를 **borrow**(Servo측 Vec 복사 X = 메모리의 "I420 borrowed/zero-copy").
- **CPU 색변환 없음**: I420 유지, YUV→RGB는 **WebRender 셰이더(GPU)** — 필수·사실상 무비용.
- 유일한 "복사" = 3평면 CPU→GPU 업로드(모든 비디오가 하는 **필수** 업로드, <1ms). "추가" 복사가 아님.
- **조건: I420 패스스루 유지** — `videoconvert`(create_video_from 1개 + playbin3 자동삽입 1개)가 I420→I420 패스스루로 협상돼야 함. 어긋나면 실제 변환(CPU 복사) 발생 → §6 검증 항목.

## 5. 컴포넌트 / 파일

- `components/media/backends/gstreamer/media_stream.rs` — 신규 `raw()` 추가(±5줄). `encoded()`/`caps*()`/`RTP_CAPS_*` 불변.
- `components/media/backends/gstreamer/media_stream_source.rs` — (a) `set_stream:71` 호출 `encoded()`→`raw()`(+선택적 env 게이트), (b) ghost pad 템플릿 caps(23-41) raw화.
- (무변경, 참고) `player.rs`·`render.rs`·`webrtc.rs`(send)·모든 싱크 — 손대지 않음.

## 6. 리스크 & 검증

### 리스크
1. **[최상위] ghost pad 템플릿 caps** — RTP 템플릿 유지 시 raw 흐름과 불일치로 decodebin3 협상 오작동 → raw(또는 any)로 변경 필수.
2. **[최상위] decodebin3 raw-bypass 정적 확정 불가** — playbin3 내부 동작 → **런타임 dot 덤프로 확인**(go/no-go).
3. **I420 패스스루 유지** — 어긋나면 videoconvert 실변환(추가 복사).
4. **오디오 동일 적용** — opus 재인코딩 제거, wasapisink 협상 확인.
5. **넓은 blast radius** — 모든 MediaStream 표시(getUserMedia 프리뷰/WebRTC 수신/임의 srcObject)에 영향 → 회귀 검증.
6. **send 경로 리스크 0**(encoded() 불변). 단일-소비자 pad 링크 제약은 선재(회귀 아님).
7. **GL 텍스처 경로(RGBA/GLMemory)** 비활성(SW·I420) → 범위 밖(§7).

### 검증
1. **파이프라인 dot 덤프(1차, 객관적):** `GST_DEBUG_DUMP_DOT_DIR` set → 카메라 출력 실행 → playbin3 `.dot`에서 **`vp8enc`/`avdec_vp8` 소멸 + raw I420 source→appsink 직결** 확인(before/after). caps가 I420 패스스루인지 확인.
2. **A/B env 토글:** 같은 세션에서 `raw()` vs `encoded()` 스위치로 화질/지연 직접 비교. 최종엔 무조건 raw + 게이트 제거.
3. **증상 수용(사용자 육안):** 아티팩트 소멸 + delay 감소를 Chrome과 비교(Servo 네이티브 창은 사용자만 볼 수 있음). 선택: 카메라를 스톱워치에 대고 glass-to-glass 지연 정량화.
4. **회귀:** getUserMedia 로컬 프리뷰 / 오디오(wasapisink) / WebRTC 수신 / 일반 `<video src=file>`(servosrc, 무영향) 정상.

## 7. 비목표

- send 경로(webrtc.rs `encoded()`) 변경.
- GL 텍스처(zero-copy GPU) 디코드 경로 활성화 — 현 CPU I420 borrowed 경로만.
- webrtcbin 수신 트랜시버/RTCP 피드백(NACK/PLI/REMB) 개선 — 별개 이슈(`getReceivers` 등도 별개).
- 타입당 다중 스트림(`media_stream_source.rs:61` 선재 제한) 해결.

## 8. 빌드 / 실행 참고

- `mach build --release` (servo_env.ps1 소싱 후 `$ErrorActionPreference='Continue'` + 프로젝트-로컬 CARGO_HOME — `[[servo-build-run-commands]]`).
- 실행: `etc/multigpu/run_presenter_wall.ps1`(카메라 출력, IndexedDB/WebRTC pref + `--ignore-certificate-errors` 포함). 검증 시 `GST_DEBUG_DUMP_DOT_DIR` env 추가.

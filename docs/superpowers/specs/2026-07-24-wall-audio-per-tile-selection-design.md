# IP-wall 스타일 타일별 오디오 선택 표출 설계 (2026-07-24)

## 목표

현재 video-only로 표출되는 4개 미디어 경로(로컬 비디오 파일, RTSP, 캡처카드, WebRTC 수신)에
**오디오를 포함**하되, IP-wall 운용 모델대로 **재생 중인 여러 `<video>` 중 원하는 것을 골라
소리를 내는** 방식으로 구현한다. 선택 단위는 `<video>` 요소별 `muted` 토글이며, **가산
(체크박스)** 선택 — 여러 타일의 오디오를 동시에 낼 수 있다.

대상 워크트리: `servo_multigpu-tiled-wall`, 브랜치 `nonstandard-media-display-port`.

## 배경 — 실측으로 확정한 현재 상태

**파이프라인 오디오 라우팅은 4경로 모두 이미 end-to-end로 존재한다.** 빠진 것은 (1) DOM
`muted`(모든 probe가 autoplay 회피용으로 설정)와 (2) 캡처·WebRTC의 오디오 *소스* 미획득뿐이다.

- **playbin 경로(비디오 파일 / RTSP `NetworkUri`)**: `player.rs`에 오디오 인프라 완비 —
  `DEFAULT_MUTED=false`, 기본 `autoaudiosink`(restore 경로), `set_mute`가 fakesink↔autoaudiosink
  전환(`player.rs:615-629`), `DISABLE_AUDIO_ENV` 게이트, 커스텀 `AudioRenderer` appsink 경로.
  소스에 오디오가 있으면 demux되어 자동 재생됨 — mute만 안 하면 소리가 난다.
- **MediaStream 경로(캡처카드 getUserMedia / WebRTC 수신)**: `media_stream_source.rs`에
  `audio_proxysrc`/`audio_srcpad`/`has_audio_stream` 오디오 plumbing이 있고
  (`MediaStreamType::Audio` 분기), audio track → audio_proxysrc → playbin3 → autoaudiosink로
  흐른다. `set_mute`도 동일 player 래퍼라 MediaStream player에 적용된다(검증 대상).
- **오디오 출력 sink 전부 사용 가능**: autoaudiosink/wasapi2sink/wasapisink/directsoundsink.
- **Servo는 autoplay-with-sound 정책을 강제하지 않는다**: `htmlmediaelement.rs:1778
  is_allowed_to_play()`가 무조건 `true`, `eligible_for_autoplay`(`:2366`)는 muted를 보지 않는다.
  → 제스처 없이 script `v.muted=false` / `autoplay`로 소리 재생 가능(월 키오스크에 적합).
- **오디오 입력 장치 열거 프로바이더 미등록**: 장치 열거는 `device.api=wasapi2`(gstwasapi2
  제공)인데 servoshell 플러그인 목록(`windows.rs.in`)엔 `gstwasapi`만 있고 `gstwasapi2`가
  없다 → 앞선 스모크에서 audioinput=0이었던 원인. **`gstwasapi2.dll`은 1.28.4.100 번들에 존재.**
  캡처카드는 오디오도 노출: `MZ0380 PCI, Analog 0N Audio`(wasapi2).
- **WebRTC recvonly transceiver는 audio 미디어 라인도 처리**(`webrtc.rs`의 `for media in
  sdp.medias()`가 `application`만 skip). `on_incoming_stream`은 encoding-name으로 audio/video를
  추론한다. → 오디오 수신 협상·디코드 인프라 존재.

## 사용자 결정 사항

- **선택 방식**: 페이지에서 `muted` 토글(새 autoplay pref 없음). 가산(체크박스) — 여러 타일 동시.
- **캡처 오디오 획득**: 자동 페어링 없이 오디오 deviceId 수동 선택(기존 deviceId 인프라 재사용).
- **deliverable**: 4소스 혼합 그리드 1개 + 타일별 오디오 선택 UI.
- **비범위**: getDisplayMedia(화면/시스템 오디오), autoplay pref, video↔audio 자동 페어링.

## 설계

### 아키텍처

오디오 선택은 `<video>` 요소별 `muted` 속성 토글로 실현된다. 네 소스가 모두 `<video>`
(파일/RTSP=`src`, 캡처/WebRTC=`srcObject`)이므로 선택 모델이 소스 종류와 무관하게 균일하다.
`<video>` 하나 = 미디어 파이프라인 하나 = 오디오 sink 하나: `muted=true`면 fakesink(무음),
`false`면 autoaudiosink(출력). **월 `--wall-all-tiles`는 paint만 복제하고 미디어 파이프라인은
복제하지 않으므로**, 언뮤트된 요소의 오디오는 타일 수와 무관하게 논리적으로 1회씩 나오고,
여러 개 언뮤트 시 WASAPI 공유모드에서 OS가 믹싱한다(가산).

### 1. 공통 enabler — `components/servo/gstreamer_plugin_lists/windows.rs.in`

- `gstwasapi2` 한 줄 추가 → 오디오 입력 장치 열거 복구(캡처카드 "Analog 0N Audio" 포함).
- 검증: `set_mute`의 fakesink↔autoaudiosink 전환이 MediaStream(Stream) player에도 적용되는지,
  autoaudiosink가 실제 시스템 출력으로 나가는지 확인.

### 2. deliverable probe — `tests/html/multigpu_wall_audio_grid_probe.html` (신규)

- 파일 / RTSP / 캡처 / WebRTC 타일을 한 페이지 그리드에 배치.
- 각 타일에 **체크박스 오디오 토글**: 체크 시 그 타일의 `<video>.muted=false`, 해제 시 `true`.
  가산(독립) — 서로 영향 없음. 초기 상태 전부 muted.
- 쿼리 파라미터로 소스 지정(예: `?file=...&rtsp=...&capvid=<deviceId>&capaud=<deviceId>&
  signaling=ws://...`) — 없는 소스 타일은 표시만 하고 비활성.
- HUD/콘솔에 각 타일 오디오 상태·트랙 유무 로깅(월 stderr 검증용).

### 3. 파일 / RTSP 오디오 (Task 2에 포함)

- 엔진 변경 없음. probe가 오디오 있는 파일·RTSP를 `<video src>`로 열고 토글로 언뮤트.
- 검증: 가청, 월 1회 재생, 다중 언뮤트 믹싱, A/V 동기.

### 4. 캡처카드 오디오

- 그리드 캡처 타일이 `getUserMedia({ video: { deviceId: <vid> }, audio: { deviceId: <aid> } })`.
  오디오 deviceId 해석은 기존 인프라 재사용(`mediadevices.rs`의 `convert_constraints`가 audio도
  처리, `select_device_by_id`의 display_name 폴백 티어가 wasapi 장치 매칭).
- 오디오 트랙이 MediaStream→audio_proxysrc→autoaudiosink로 흘러 가청. 선택 video 포트와 동기 확인.
- 필요 시 오디오 캡처 bin의 caps 보정(비디오 I420 고정과 유사한 함정이 있으면) — Task에서 판단.

### 5. WebRTC 오디오

- 프로듀서가 오디오 송출(예: `gst-launch-1.0 ... audiotestsrc is-live=true ! opusenc ! webrtcsink`,
  또는 videotestsrc와 함께 webrtcsink에 오디오 추가).
- probe: `RTCPeerConnection`이 audio recvonly transceiver를 이미 얻으므로 audio `ontrack`이 발생 —
  video·audio 두 트랙을 하나의 MediaStream에 부착(`e.track`을 누적). autoaudiosink로 가청.
- 엔진 변경 예상 없음(수신 경로 존재); 프로듀서·probe 배선 위주.

## 데이터 흐름

소스(파일/RTSP demux, 캡처 wasapi2, WebRTC opus) → 오디오 트랙 → (playbin 또는 MediaStream
audio_proxysrc) → playbin3 → autoaudiosink(muted면 fakesink) → 시스템 오디오(WASAPI 공유, OS 믹싱).
선택은 DOM `muted` 토글로만 이뤄지고 파이프라인은 이에 반응. 비디오 render/present는 월 타일로
fan-out되지만 오디오 sink는 논리 파이프라인당 1개.

## 오류 처리

- 오디오 트랙 없는 소스(video-only 캡처/WebRTC): 토글이 no-op(무해). HUD에 "no audio track" 표기.
- 오디오 장치 미열거(gstwasapi2 누락 등): getUserMedia audio 트랙 0 → 캡처 타일 무음, 크래시 없음.
- autoaudiosink 생성 실패: 기존 player 오류 경로(로그 + 무음). 크래시 없음.

## 테스트

1. `cargo build`/`mach build`(미디어·플러그인 변경 → mach 필수, gstwasapi2 자동 복사).
2. `rustfmt --edition 2024 --check` + `git diff --check` (touched .rs).
3. 오디오 입력 장치 열거 확인: enumerateDevices가 audioinput(캡처카드 "Analog 0N Audio" 포함) 반환.
4. 실기 스모크(월 `--wall-all-tiles`): 혼합 그리드에서
   - 파일·RTSP 타일 언뮤트 시 가청, 재뮤트 시 무음.
   - 캡처 타일: video 포트 + audio deviceId 지정 시 가청·동기.
   - WebRTC 타일: 오디오 프로듀서 붙이면 가청·동기.
   - 여러 타일 동시 언뮤트 시 믹싱(가산), 월 타일 수와 무관하게 오디오 1회(중복 없음).
   - 크래시/패닉 0.

## 비범위

- getDisplayMedia(화면/시스템 오디오 캡처).
- autoplay-with-audio 정책 pref(Servo가 미강제라 불필요).
- video↔audio 자동 페어링(오디오는 수동 deviceId).
- 배타(라디오) 선택 모드, 오디오 레벨/게인 UI, 채널 라우팅.

## 검증 결과 / 마무리 (2026-07-24)

**최대 성과 — 근본 버그 수정(mute→unmute 미복원)**: 구현 착수 후 `set_mute`가 재생 중
playbin3의 `audio-sink`를 fakesink↔autoaudiosink로 런타임 스왑(커밋 be2148a68cb 도입)하던
결함을 발견. playbin은 `audio-sink`를 preroll에만 링크하므로 unmute의 restore 스왑이
오디오 브랜치를 재링크하지 못해 소리가 복원되지 않았음. 수정(commit 34681fef9b4): 런타임
스왑 제거, GstPlay `set_mute` 속성 + `set_audio_track_enabled`(가역)만 사용. **사용자 release
빌드 청각 검증 PASS**(mute→무음→unmute→소리 복원, 토글 반복 정상). 이 수정이 타일별 오디오
선택 모델 전체의 전제.

**경로별 결과 (4 중 3 동작)**:
- **로컬 파일**: `Wildlife…mp4`(AAC) 가청, mute 토글 가역 — 사용자 검증 PASS.
- **RTSP**: 오디오 인프라 동일(playbin), 파일과 같은 경로 — 오디오 있는 RTSP 소스로 동작 예상.
- **캡처카드**: `getUserMedia({video:{deviceId}, audio:{deviceId}})`로 audio=1 확인
  ("MZ0380 PCI, Analog 04 Audio"→wasapi2sink까지 파이프라인 구성, negotiation stall 없음,
  caps 수정 불요). 청각·동기는 사용자 확인 대기.
- **WebRTC**: ❌ 엔진 갭으로 이월. 프로듀서(`videotestsrc + audiotestsrc ! webrtcsink`)가
  **OPUS 오디오를 실제로 오퍼**함을 SDP로 확정(`a=rtpmap:101 OPUS/48000/2`, BUNDLE
  `video0 audio1`) — 소스 문제 아님. Servo answer가 recvonly audio 트랜시버를 BUNDLE에
  붙이는데도 **오디오 트랙이 JS로 전달되지 않음**(`ontrack video (v=1 a=0)`). 원인 후보:
  answer의 audio m-line rtpmap 완전성 / webrtcbin의 2번째(audio) 수신 pad·decodebin 생성 /
  트랙→MediaStream 배선. **사용자 결정: 별도 엔진 집중 세션으로 이월**, 이번 4-경로 범위에선
  3/4 완료로 마무리.

**enabler**: `gstwasapi2` 1줄 등록으로 오디오 입력 장치 열거 복구(audioinput=9, 캡처카드
Analog 01-04 Audio 전부 구분). commit 76bf45de71b.

**deliverable**: `tests/html/multigpu_wall_audio_grid_probe.html` — 4타일 그리드 + 타일별
가산(체크박스) 오디오 토글. 파일/RTSP/캡처 타일 배선 완료, WebRTC 타일은 트랙 누적 배선
완료(수신측 엔진 갭으로 오디오만 미도달).

**이월 이슈(무관)**: debug 빌드는 월 + 동적 `<video src>`에서 `MakeCurrentFailed`
(surfman/ANGLE)로 크래시 — release 빌드는 정상. 이번 오디오 작업과 무관한 선재 렌더 이슈.

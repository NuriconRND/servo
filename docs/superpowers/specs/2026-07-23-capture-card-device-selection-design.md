# 캡처카드 getUserMedia 디바이스 선택 설계 (2026-07-23)

## 목표

캡처카드 4포트 환경에서:
1. `navigator.mediaDevices.enumerateDevices()`가 각 포트를 **구분 가능한 label + 유니크한 deviceId**로 반환한다.
2. `getUserMedia({ video: { deviceId: ... } })`로 **특정 포트를 지정해 열 수 있다**.

대상 워크트리: `servo_multigpu-tiled-wall` (브랜치 `nonstandard-media-display-port`), GStreamer 사내 커스텀 1.28.4.100 번들.

## 배경 — 실측으로 확정한 원인 사슬

MZ0380 4포트 캡처카드는 GstDeviceMonitor에 **총 8개 장치**로 잡힌다:

| 프로바이더 | class | display name | 고유 식별자 |
|---|---|---|---|
| winks (`ksvideosrc`, deprecated) | `Video/Source` | "MZ0380 PCI" ×4 (전부 동일) | element `device-path` 프로퍼티로만 노출 |
| mediafoundation (`mfvideosrc`) | `Source/Video` | "MZ0380 PCI, Analog 01~04 Capture" (구분됨) | GstDevice properties `device.path` |

- **4개 동일 이름의 원인**: `components/media/backends/gstreamer/device_monitor.rs`가 `device_class() == "Video/Source"` **정확일치**로 매칭 → mf 장치(class `Source/Video`)가 조용히 탈락하고 ks 장치 4개(이름 전부 "MZ0380 PCI")만 남는다. 게다가 `device_id`가 display_name 그대로라 id도 4개 전부 동일.
- **첫 포트만 열리는 원인**: `components/media/backends/gstreamer/media_capture.rs::get_track`이 `devices.front()` 하드코딩. `MediaTrackConstraintSet`에 `device_id` 필드가 없고 WebIDL의 `deviceId` 멤버는 주석 처리 상태(`MediaDevices.webidl`).
- **ks↔mf 경로 대응 가능**: 두 API의 device path는 KS 카테고리 GUID 한 조각만 다르고 PCI 인스턴스 경로 + 끝의 핀 GUID가 동일하다.

  ```
  ks: \\?\pci#...#6&2adcf5b7&0&000800e7#{6994ad05-...}\{6f814be9-...c03401000222}
  mf: \\?\pci#...#6&2adcf5b7&0&000800e7#{65e8773d-...}\{6f814be9-...c03401000222}
  ```

  카테고리 GUID(`#{...}` 세그먼트)를 제거한 **정규화 키**로 1:1 대응. 순서 대응은 불가(실측: ks는 01,03,02,04 / mf는 04,01,03,02 순).

## 사용자 결정 사항

- 열거는 **mediafoundation 장치를 대표로 노출**(구분되는 이름), 실제 캡처 element는 **ksvideosrc를 우선 사용**.
- deviceId 제약은 bare DOMString / `{exact}` / `{ideal}` 모두 수용.
- 요청 deviceId와 일치하는 장치가 없으면 **실패(트랙 0개)** — exact/ideal 동일 적용. (표준은 ideal이면 폴백이지만, 잘못된 포트가 조용히 열리는 것을 막는 의도적 선택.)
- probe 페이지에 쿼리 파라미터 포트 선택 추가.

## 설계

### 1. 열거 — `backends/gstreamer/device_monitor.rs`

- 클래스 매칭을 정확일치에서 **토큰 포함 검사**로 변경: class를 `/` 분리 토큰으로 보고 {Video,Source} → VideoInput, {Audio,Source} → AudioInput, {Audio,Sink} → AudioOutput. (`Source/Video`·`Video/Source` 모두 수용.)
- videoinput 대표 선출: `device.api == "mediafoundation"`인 장치를 대표로 노출하고, **정규화 키가 같은 ks 쌍둥이는 목록에서 숨긴다**. mf 쌍이 없는 ks 장치는 그대로 노출(사라지는 장치 없음).
- `device_id` = 장치 고유 경로(properties `device.path` → element `device-path` 폴백 → 없으면 display_name). label = display name.
- **공유 헬퍼 분리**: id 계산·정규화 키 함수는 열거(`device_monitor.rs`, 캐시된 monitor)와 오픈(`media_capture.rs`, 새 monitor) 양쪽이 동일 로직을 쓰도록 한 모듈에 둔다.

### 2. 오픈 — `backends/gstreamer/media_capture.rs` + `streams/capture.rs`

- `streams/capture.rs`: `ConstrainString { Exact(String), Ideal(String) }` 신설, `MediaTrackConstraintSet`에 `device_id: Option<ConstrainString>` 추가.
- `get_track`:
  - deviceId 요청 시(값은 mf `device.path`): 정규화 키로 장치들을 스캔해 **ks 쌍둥이를 찾으면 그 장치로 `create_element`** (표시=mf, 실사용=ksvideosrc). ks 쌍이 없으면 **mf 장치로 폴백 + warn 로그**.
  - 일치 장치 없음 → `None`(트랙 0개) + warn 로그.
  - deviceId 미지정 → 현행 `devices.front()` 유지(무회귀).
  - ks 장치의 device-path는 GstDevice properties에 없을 수 있으므로 `create_element()` 후 element의 `device-path` string 프로퍼티를 읽어 대응한다(생성만으로는 상태 변화 없음).

### 3. DOM 배선 — `script_bindings/webidls/MediaDevices.webidl` + `script/dom/media/mediadevices.rs`

- 주석 해제/활성화: `ConstrainDOMStringParameters` dictionary, `ConstrainDOMString` typedef, `MediaTrackConstraintSet`의 `deviceId` 멤버.
- `convert_constraints` 확장: bare string → `Ideal`, `{exact: s}` → `Exact`, `{ideal: s}` → `Ideal`, `sequence<DOMString>` → 첫 원소 사용.
- audio 경로도 같은 constraint set을 지나므로 자동으로 배선된다(오디오 장치 id는 기존 display_name 기반 유지 — 이번 범위는 video).

### 4. probe 페이지 — `tests/html/multigpu_capture_card_probe.html`

- `?device=<0-base 인덱스|라벨 부분문자열(대소문자 무시)>`로 열 포트 선택.
- 선택 시 `{ video: { deviceId: { exact: id } } }`로 오픈, HUD에 선택된 장치 라벨/id와 매칭 방식 표시. 미지정 시 현행(`{video:true}`, 첫 장치) 유지.

## 오류 처리

- deviceId 미일치: 트랙 0개 + `warn!` 로그(요청 id와 사용 가능 id 목록 출력) — 월 디버깅 시 stderr에서 바로 원인 확인.
- ks 쌍둥이 부재: mf 장치로 폴백 + `warn!` 로그.
- element 생성 실패: 기존과 동일하게 `None`.

## 검증

1. `cargo check -p servoshell` → full `mach build` (미디어 변경은 mach 필수 — cargo -p는 media-gstreamer 피처 누락).
2. `rustfmt --edition 2024 --check` + `git diff --check`.
3. 런타임: probe 페이지를 `?device=`로 4포트 각각 열어 HUD에서 (a) enumerateDevices 4개 구분된 label/유니크 id, (b) 지정 포트 영상 재생, (c) stderr에서 ks element 사용 로그 확인. 미지정/오타 id 케이스도 확인(트랙 0 + warn).

## 비범위

- 오디오 입력 deviceId 정밀화(wasapi id), groupId, OverconstrainedError 예외 타입, ondevicechange 이벤트.
- getDisplayMedia 경로는 무변경.

## 검증 결과 (2026-07-23, Task 7)

- 정적: rustfmt --edition 2024 --check(5개 파일) 무출력, `git diff --check` 무출력, `cargo test -p servo-media-gstreamer device_id` 6 passed.
- `.\mach build --release -j 8` exit 0 (2m35s, 증분).
- 실기(MZ0380 4포트) 4포트 전부 개별 실행 검증: `?device=analog 01/02/03/04` 모두 `selected device` 라벨·id가 서로 상이하게 매칭되고, 백엔드 로그 `getUserMedia: deviceId "..." -> "MZ0380 PCI" (api "winks/other")`로 ksvideosrc(비-mediafoundation) 경로 사용을 확인. 4포트 전부 `videoSize=1920x1080 ... advancing`(라이브 신호 있음).
- 네거티브: `?device=nonexistent-port` → `NO videoinput matches`, 백엔드 `getUserMedia` 호출 자체가 없음(로그 부재로 확인) — 페이지 단 선차단 정상.
- 실행 조건: 이 브랜치 `config/wall_layout.local_1x1.json`의 `monitor` 스키마가 파싱 실패("monitor must be a non-negative integer") → `wall_layout.local_3x1.branchschema.json` 사용(3타일, 로그 목적상 문제없음). 콘솔 `console.log`는 stdout에 출력됨(`println!` 경로, headed_window.rs) — 백엔드 확인 로그(`getUserMedia: deviceId ...`)를 보려면 `RUST_LOG=warn,script=info,servo_media_gstreamer=info` 필요(그냥 `script=info`만으로는 media_capture.rs의 info! 로그가 안 잡힘).
- 알려진 이슈(이번 변경과 무관): wall-all-tiles 모드에서 캡처카드 스트림이 열린 상태로 창을 닫으면(WM_CLOSE/CloseMainWindow) GStreamer 파이프라인 teardown이 12초+ 안 끝나 프로세스가 종료되지 않음(기존 메모리에 기록된 "teardown" 이슈와 일치). 매 실행마다 필요한 로그 라인을 프로세스가 살아있는 동안 파일에서 먼저 확인한 뒤 `Stop-Process -Force`로 회수함(데이터 손실 없음, 정상 종료 자체는 blocked).

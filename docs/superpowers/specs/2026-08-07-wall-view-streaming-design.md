# wall_view — 월 표출 영역을 WebSocket + WebCodecs로 스트리밍 (2026-08-07)

## 목표

월에 표출되는 화면 전체를 브라우저에서 볼 수 있게 하는 **독립 프로세스** `wall_view`를 만든다. 원격 모니터링(운영자 확인)과 실시간 제어용 프리뷰를 겸한다.

wall_layout JSON을 그대로 입력으로 받아 캡처 대상과 배치를 결정하고, 데스크톱 캡처 → GPU 합성·축소 → H.264 하드웨어 인코딩 → WebSocket 송출까지를 한 프로세스가 담당한다. 클라이언트는 WebCodecs로 디코드해 `<canvas>`에 그린다.

## 배경 — 참고 솔루션과 실측으로 확인한 전제

### 참고 솔루션 (ViewFlex WallEncoder)

`D:\Project\ViewFlex30\Input\WallEncoder`의 구조는 다음과 같다.

```
wallcapturesrc (capture-left/top/right/bottom, output-width/height, framerate)
  → mfh264enc (bitrate, cabac=0, quality-vs-speed=0, rc-mode=0, threads=1, gop-size=fps, ref=1)
  → rtspserversink (use-auth, id/password, server-ip/port, max-connection-count)
```

C++ DLL이 C ABI(`WallEncoderCreate`/`Play`/`WaitForMessage`/`Release`/`GetRemoteEndPoints`/`SetMaxNumOfClients`)를 노출하고 C# WinForms 앱이 REST로 제어한다. 옵션으로 `tee`를 걸어 저해상도 서브스트림을 동시 송출한다.

**이 구조에서 우리가 가져오는 것**: 인코더 저지연 튜닝 파라미터, "설정(rect·출력크기·fps·bitrate)으로 구동되는 독립 인코더 프로세스"라는 운영 형태, 다중 클라이언트 방어(최대 연결 수).

### `wallcapturesrc` 소스 분석 — 채용하지 않는 근거

`D:\Project\ViewFlex30\Common\GstElement\GstWallCaptureSrc`를 분석했다. 구현은 훌륭하다 — `WallCapturerMulti`가 출력별 캡처 스레드를 띄우고, `DuplicationManagerNv12`가 `ID3D11VideoProcessor`로 **스케일·NV12 변환·배치를 고정기능 한 번에** 처리해 하나의 출력 텍스처로 모은다.

그럼에도 채용하지 않는다.

| 항목 | 근거 |
|---|---|
| 라이선스 게이트가 `exit(1)` | `gstwallcapturesrc.cpp:246-250`, `set_caps` 안에서 키 불일치 시 **프로세스 즉시 종료**. 상시 구동 모니터링 프로세스에 부적합하고 GStreamer 콜백 안이라 잡을 수도 없다 |
| 출력이 시스템 메모리 NV12 | `gstwallcapturesrc.cpp:271-274`가 `video/x-raw`(D3D11 메모리 아님). GPU에서 변환까지 해놓고 CPU로 내린 뒤 인코더가 다시 올린다 |
| 빌드 체인 | VS2019, GStreamer 1.22.4.x 헤더가 네트워크 공유(`\\192.168.1.214\share\...`), 라이선스 SDK가 저장소 밖 `ReferenceDLLs\gstreamer\license\`. 사내 1.28.4.100으로 재빌드 필요 |

**CPU 다운로드는 설계 선호가 아니라 당시 하드웨어의 산물로 보인다.** 대상이 AMD Radeon HD 7800M(2012, GCN1, VCE 1.0)이면 하드웨어 H.264 인코더를 사실상 쓸 수 없어 소프트웨어 인코딩 → 시스템 메모리 NV12가 강제된다. 대상이 RX 580 2048SP(Polaris, VCE 3.4)로 바뀌면 그 제약이 사라지고, **CPU 왕복은 필요조건이 아니라 순수한 손실이 된다.** 즉 하드웨어가 좋아질수록 이 엘리먼트를 쓸 이유가 줄어든다.

### 사내 GStreamer 1.28.4.100에서 실측한 가용 자산

`wallcapturesrc`와 `rtspserversink`는 **없다**(ViewFlex 전용 빌드에만 존재). 대신 다음이 있다:

| 용도 | 엘리먼트 | 확인 사항 |
|---|---|---|
| 캡처 | `d3d11screencapturesrc` | `monitor-index`/`monitor-handle`, `crop-x/y/width/height`, `capture-api`(dxgi/wgc), `show-cursor`(기본 false). **모니터 하나당 하나.** SRC caps = `video/x-raw(memory:D3D11Memory), BGRA` |
| 합성 | `d3d11compositor` | 싱크 패드에 `xpos`/`ypos`/`width`/`height`/`sizing-policy`. **SRC caps에 NV12 포함**(D3D11Memory) — BGRA 입력을 받아 합성·축소·NV12 변환을 한 엘리먼트로 끝낼 수 있어 별도 `d3d11convert` 단이 불필요하다 |
| 인코딩 | `amfh264enc` | SINK caps에 `video/x-raw(memory:D3D11Memory), NV12`. **이 머신 열거값 [64, 1920]** |
| 인코딩 | `mfh264enc` | SINK caps에 `video/x-raw(memory:D3D11Memory), NV12`, [64, 8192]. `low-latency` 속성 보유 |
| 파싱 | `h264parse` | `config-interval`(-1 = 매 IDR에 SPS/PPS) |
| 출력 | `appsink` | 인코딩된 AU를 애플리케이션으로 |

**두 인코더 모두 D3D11 메모리를 직접 받는다** — 캡처부터 인코더까지 CPU 왕복 없이 갈 수 있다는 뜻이고, 이것이 스톡 엘리먼트를 택하는 결정적 근거다.

### 배치 위치 — 버전 관리 정본

프로젝트 루트(`F:\...\20260606_multigpu_browser`)는 **git 저장소가 아니다.** 버전 관리되는 정본은 servo 저장소 안의 `etc/multigpu/tools/`이며 `topology_probe`가 거기 있다(루트 `tools/`의 사본과 내용 동일 — 워크스페이스 충돌을 피한 실행용 복사본).

## 사용자 결정 사항

1. **용도** = 원격 모니터링 + 실시간 제어 프리뷰 (혼재)
2. **캡처 지점** = 데스크톱 캡처(월 화면에 보이는 그대로). 겹친 창·오류 대화상자도 보이는 편이 모니터링에 유리
3. **출력** = 가상 뷰포트 전체를 합쳐 **1080p로 축소**, 단일 스트림
4. **구현 접근** = 스톡 d3d11 엘리먼트 (`wallcapturesrc` 미채용)
5. **레이아웃 파싱 중복** = 지금은 감수. 세 번째 소비자가 생기면 공용 크레이트로 묶는다
6. **인증** = v1 없음. 바인드 기본 `127.0.0.1`. 단 **제품 배포 시 id/password를 넣을 수 있는 확장 지점**을 구조에 남긴다

## 설계

### 1. 프로세스 경계와 배치

`etc/multigpu/tools/wall_view/` — servo 워크스페이스에 속하지 않는 **독립 Rust 바이너리**.

`Cargo.toml`에 빈 `[workspace]` 테이블을 선언해 자체 워크스페이스 루트로 만든다. servo 저장소 안에 있으면서 servo 워크스페이스 멤버가 아닌 패키지는 그렇게 하지 않으면 cargo가 워크스페이스 소속으로 오인해 실패한다. `topology_probe`가 루트 `tools/`에 사본을 둔 이유가 이것으로 보이며, `[workspace]` 선언으로 사본 없이 해결한다.

winit_wall과는 프로세스도 코드도 분리된다. 유일한 접점은 **같은 wall_layout JSON을 읽는다**는 것이다. 이 분리가 주는 것:

- winit_wall의 렌더 경로를 건드리지 않아 월 표출 성능에 영향이 없다
- servo 빌드(약 7분) 없이 스트리머만 반복 개발할 수 있다
- 월이 죽어도 뷰어가 죽지 않고, 그 반대도 성립한다

### 2. 파이프라인

레이아웃을 읽어 **동적으로** 구성한다. 타일이 올라간 spatial display마다 소스를 하나 만들고, 각 소스를 1080p 캔버스의 제 자리에 바로 축소 배치한다.

```
d3d11screencapturesrc(monitor-index=M0, show-cursor=…) ─┐
d3d11screencapturesrc(monitor-index=M1)                 ─┼→ d3d11compositor
                     …                                   ─┘   sink_N.xpos/ypos/width/height
                                                               = 1080p 캔버스 좌표
                                                                      │
                                    video/x-raw(memory:D3D11Memory),NV12,1920x1080
                                                                      ↓
                                              amfh264enc | mfh264enc  (D3D11 메모리 그대로)
                                                                      ↓
                                       h264parse ! video/x-h264,stream-format=avc,alignment=au
                                                                      ↓
                                                                  appsink
                                                                      ↓
                                                          WebSocket broadcast (fan-out)
```

**전체 가상 뷰포트를 중간에 만들지 않는다.** 3840×1080이든 7680×2160이든 각 모니터 캡처가 곧바로 1080p 캔버스의 제 슬롯으로 축소돼 들어간다. `wallcapturesrc`가 VideoProcessor로 하던 "스케일·변환·배치 한 번에"와 같은 효과이며, 인코더의 크기 한계도 자연히 피한다.

인코더 파라미터는 참고 솔루션의 저지연 튜닝을 이식한다: `gop-size = fps`(1초), `ref=1`, `cabac=0`, `rc-mode=0`(CBR), 그리고 1.28에 있는 `low-latency=true`.

### 3. 레이아웃 → 캡처 매핑 (순수 함수)

wall_layout의 타일은 `display`(spatial 인덱스)와 가상 뷰포트 내 `rect`를 갖는다. DXGI 토폴로지를 좌상단=0, 행 우선로 정렬해 실제 모니터를 찾고, 스케일 계수 `output_width / virtual_width`를 곱해 합성 패드 좌표를 낸다.

```rust
/// 레이아웃 + 토폴로지 → 각 캡처 소스의 (모니터, 합성 패드 기하)
fn plan_capture(
    layout: &WallLayout,
    displays: &[DisplayTopology],   // spatial 정렬된 것
    output: Size,                   // 예: 1920x1080
) -> Result<Vec<CaptureSlot>, PlanError>;

struct CaptureSlot {
    monitor_index: usize,           // d3d11screencapturesrc monitor-index
    xpos: i32, ypos: i32,           // d3d11compositor 패드 좌표 (출력 캔버스 기준)
    width: i32, height: i32,
}
```

**이 함수가 이 설계에서 자동 검증이 가능한 핵심부다.** 파이프라인·인코더·네트워크는 전부 런타임 검증이 필요하지만 이건 단위 테스트로 고정된다.

### 4. 프로토콜

**패키징은 AVCC + description.** `h264parse ! video/x-h264,stream-format=avc,alignment=au`로 받으면 caps의 `codec_data`가 그대로 avcC 박스다. 접속 시 한 번 보내면 클라이언트는 파라미터 세트를 이미 갖게 되어, **늦은 참여자는 다음 IDR 하나만 기다리면 된다.**

```
접속 직후 (텍스트 JSON):
  {"type":"init","codec":"avc1.42E01F","description":"<base64 avcC>",
   "codedWidth":1920,"codedHeight":1080,"framerate":30}

미디어 (바이너리, 리틀엔디언 16바이트 헤더 + AVCC 페이로드):
  u8  type          1 = video AU
  u8  flags         bit0 = keyframe
  u16 reserved
  i64 timestamp_us
  u32 reserved
```

코덱 문자열은 avcC의 `profile_idc`/`constraint_flags`/`level_idc` 3바이트에서 유도한다(`avc1.` + 6자리 hex). **이것도 순수 함수라 단위 테스트 대상이다.**

클라이언트:

```js
decoder.configure({
  codec, description,               // init 메시지에서
  optimizeForLatency: true,
  hardwareAcceleration: "prefer-hardware",
});
// 바이너리 메시지마다
decoder.decode(new EncodedVideoChunk({ type, timestamp, data }));
// output 콜백에서 canvas에 draw 후 frame.close()
```

인코딩은 **한 번만** 하고 결과 바이트를 팬아웃한다. 클라이언트 수가 늘어도 GPU 부하는 그대로다.

### 5. 늦은 참여자

- GOP = `1 × fps`(1초) → 최악 1초 대기
- 새 클라이언트 접속 시 `GstForceKeyUnit` 업스트림 이벤트로 **즉시 IDR 요청**. 동시 접속은 최소 간격 200ms로 합친다
- **클라이언트별 정렬 게이트**: 첫 keyframe이 나오기 전까지 그 클라이언트에게 아무것도 보내지 않는다. delta부터 받아 디코더 에러를 내는 상황을 서버에서 막는다

### 6. 흐름 제어 — 느린 클라이언트 격리

**불변식: 한 클라이언트의 느림이 다른 클라이언트나 캡처에 전파되지 않는다.**

appsink 콜백은 절대 블록하지 않는다. 거기서 막히면 파이프라인이 밀리고 캡처·인코딩까지 밀린다.

- 클라이언트마다 **바운디드 큐**(기본 30 AU, 바이트 상한 병행)
- 큐가 차면 **delta를 버리고** 그 클라이언트를 "재정렬 대기"로 표시 → 다음 keyframe에서 복구(+ force-key-unit 요청)
- 연속 3회 실패 또는 5초 초과 지연이면 **연결 종료**
- 최대 동시 연결 수 설정(참고 솔루션의 `max-connection-count` 대응)

### 7. 인증 확장 지점

v1은 인증 없이 가되, 접속 경로가 반드시 이 지점을 지나가게 한다.

```rust
trait Authenticator {
    fn authenticate(&self, req: &UpgradeRequest) -> Result<(), AuthError>;
}
struct NoAuth;   // v1 기본 — 항상 통과
```

나중에 `BasicAuth`를 구현체로 추가하고 설정에 `auth` 절을 여는 것으로 끝난다. 지금 만드는 건 통과 구현 하나와 호출 지점뿐이라 비용이 거의 없고, 인증을 넣을 때 접속 처리 전체를 뜯을 필요가 없어진다.

### 8. 설정

```
wall_view --layout <path.json>
          [--bind 127.0.0.1:8787]      # 기본 로컬 전용, 외부 노출은 명시적으로만
          [--output 1920x1080]
          [--fps 30] [--bitrate 8000]  # kbps
          [--encoder auto|amf|mf|software]
          [--capture-api dxgi|wgc] [--show-cursor]
          [--max-clients 8] [--idle-stop true]
```

클라이언트 페이지(`web/index.html`)는 같은 프로세스에서 정적 서빙해 별도 웹서버 없이 브라우저로 바로 접속되게 한다.

### 9. 에러 처리

**인코더 선택과 조기 실패.** `auto`는 `amfh264enc` → `mfh264enc` → `openh264enc`(소프트웨어, 경고) 순으로 시도한다. 소프트웨어 폴백은 D3D11 메모리를 받지 못하므로 **CPU 다운로드가 한 번 들어가고 제로카피가 깨진다** — 최후 수단이며, 그때는 경고 로그로 명시한다(이 스펙이 `wallcapturesrc`를 기각한 바로 그 비용을 스스로 감수하는 경로이므로 조용히 넘어가면 안 된다). 파이프라인 구성 시점에 **인코더 caps의 크기 한계를 읽어 검증**하고, 출력 크기가 한계를 넘으면 "AMF는 최대 W×H까지 지원, `--output`을 줄이거나 `--encoder mf`를 쓰라"는 메시지로 즉시 실패한다. 이 머신에서 AMF가 1920×1920으로 열거된 것을 실측했으므로 실재하는 위험이다.

**레이아웃과 토폴로지 불일치.** 타일이 가리키는 spatial display가 없으면 **명확히 실패**하고 발견된 디스플레이 목록을 덤프한다. 모니터링 도구가 조용히 일부만 캡처하면 화면이 나오니 정상으로 보인다 — 최악의 실패 양식이다.

**런타임 토폴로지 변경·디바이스 로스트.** 모니터 분리나 해상도 변경은 Desktop Duplication에서 access-lost로 나타난다. 파이프라인 에러를 잡아 **teardown → 1초 백오프 → 재구성**하고, 클라이언트 연결은 유지한 채 새 caps로 init을 재전송한다.

**시청자 0명.** 마지막 클라이언트가 나가면 파이프라인을 정지한다(`--idle-stop`, 기본 켬). 아무도 안 볼 때 월 머신의 GPU와 전력을 쓸 이유가 없다.

## 테스트

### 단위 — 자동으로 잡히는 부분

- `plan_capture`: 2×1, 2×2, 비대칭 배치, 스케일 계수 반올림, display 인덱스 범위 초과 시 에러
- avcC 3바이트 → `avc1.XXXXXX` 코덱 문자열
- 바이너리 프레이밍 인코드/디코드 왕복
- 흐름 제어 큐: 가득 참 → delta 폐기 → keyframe 복구 시퀀스

### 스모크 — 클라이언트 없이

파이프라인을 N초 구동해 appsink에서 프레임 수, keyframe 간격, 캡처 해상도를 확인한다. 브라우저 없이 서버측 정상 동작을 가린다.

### 육안 — 사용자 몫 (이 프로젝트 관례)

브라우저에서 월 화면이 제대로 보이는지, 배치가 레이아웃과 맞는지, 체감 지연이 제어용으로 쓸 만한지. **지연 측정은 기존 프로브 페이지를 활용**한다 — 월에 시계가 도는 페이지를 띄우고 브라우저 화면과 같이 촬영하면 차이가 바로 보인다.

### 운영 제약

- 사내 GStreamer 1.28.4.100 런타임 필요(DLL·플러그인 경로)
- `d3d11screencapturesrc`의 `show-cursor` 기본값은 false — 모니터링 용도면 켜는 편이 나을 수 있다
- 실행은 월이 떠 있는 상태에서만 의미가 있다

## 리스크

| 리스크 | 완화 |
|---|---|
| AMF 인코더 크기 한계(이 머신 1920) | 출력이 1080p라 문제없으나, 구성 시점 caps 검증으로 조기 실패시킨다. RX 580 실기에서 재열거 필요 |
| 월이 서로 다른 GPU의 모니터에 걸침 | 캡처는 모니터별 어댑터에 바인딩되고 합성은 한 디바이스에서 일어나므로 교차 어댑터 전송이 생긴다. `wallcapturesrc`도 같은 물리 제약을 받는다. 단일 GPU 구성에서는 무관 |
| 레이아웃 파싱 사본 증가 | 읽기 전용 소비자이고 스키마 불일치는 즉시 드러난다. 세 번째 소비자가 생기면 공용 크레이트로 |
| 사내 GStreamer 런타임 의존 | 이 저장소가 이미 그 GStreamer로 동작한다. 새로운 의존이 아니다 |

## 비목표

- 인증·TLS (v1 없음. 확장 지점만 남긴다)
- 오디오
- 서브스트림(고화질·저화질 동시 송출) — 참고 솔루션엔 있으나 v1 비범위
- 녹화·저장
- 클라이언트에서 월 제어(입력 되돌리기)
- 타일별 개별 스트림 — 단일 합성 스트림만
- winit_wall 내부 통합(인프로세스 캡처) — 데스크톱 캡처로 결정됨

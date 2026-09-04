# 캡처카드 단일 연결 공유 — 운용 문서 (2026-09-04)

브랜치 `capture-card-shared-connection`. 설계: `docs/superpowers/specs/2026-09-04-capture-card-shared-connection-design.md`.
계획: `docs/superpowers/plans/2026-09-04-capture-card-shared-connection.md`. 코드: `components/media/backends/gstreamer/capture_hub.rs`.

이 문서는 **캡처카드가 실제로 물린 장비에서** 이 브랜치를 검증하려는 사람을 위한 것이다.
개발기에는 캡처카드가 없어서, 여기 적힌 것 중 "실기 절차" 절 이전은 전부 하드웨어 없이 도는
자동 테스트(20건)로 이미 증명되어 있다. 실기에서 확인해야 하는 것은 딱 하나 — **페이지를
반복 전환해도 같은 물리 포트가 한 번만 열리는가**이다.

## 1. 왜 포트당 연결이 하나여야 하는가

2026-08-20, `gst-launch` 로 같은 물리 포트를 K개 동시에 여는 실측(이 문서와 무관하게 이미
기록되어 있던 측정):

| K (동시 개방 수) | 결과 |
|---|---|
| 1 | 5/5 성공 |
| 2 | 4/5 성공 |
| 3 | **0/5 — 매번 정확히 1개만 살아남는다** |

포트를 세 번째로 열면 예외 없이 붕괴하고, 그때마다 결과는 "3개 중 하나만 살아남는다"는
동일한 형태였다. 이 한계는 캡처카드/드라이버 쪽 제약이고 엔진 쪽에서 고칠 수 있는 것이
아니다 — 그래서 이 브랜치의 해법은 "장치를 다시 여는 일 자체를 없앤다"이다.

### 이 문제가 왜 발생했었는가 (구조적 원인)

`getUserMedia()` 는 호출마다 새 `ksvideosrc` 를 열었다. 그리고 스트림 레지스트리
(`components/media/streams/registry.rs`)가 `Arc<Mutex<dyn MediaStream>>` 를 **강참조**로
보관하는데, 그 `Arc` 를 놓는 유일한 경로가 `GStreamerMediaStream::drop` → `unregister_stream`
이었다. 즉 레지스트리 자신이 그 `Arc` 를 쥐고 있어서 `Drop` 이 영원히 불릴 수 없는 자기참조
사이클이었다. 여기에 `MediaStreamTrack.stop()` 은 WebIDL에서 주석 처리되어 미구현, 문서가
사라질 때 스트림을 해제하는 훅도 없었다. 결과: **페이지를 전환할 때마다 같은 포트에 연결이
하나씩 더 쌓이고, 기존 연결은 `Playing` 상태로 계속 살아 있었다.** 전환 2~3회째부터
간헐적으로 무너지던 증상의 형태는 위 K=2/K=3 실측과 정확히 맞물린다.

## 2. 무엇이 바뀌었는가

`components/media/backends/gstreamer/capture_hub.rs` 에 물리 포트당 허브(`DeviceHub`)를 뒀다.

- **지연 개방, 프로세스 수명 유지.** 그 포트를 처음 요청할 때 연다. 소비자가 0이 되어도
  닫지 않는다 — 프로세스가 끝날 때까지 그대로 열려 있다.
- **`getUserMedia()` 는 `appsrc` 로 합류한다.** 장치를 새로 여는 대신, 허브의 `appsink` 가
  뽑은 프레임을 배포 스레드가 소비자별 `appsrc` 로 shallow copy(메모리 공유, 헤더만 새로)
  해서 push 한다. 소비자 등록/해제는 `Vec` 에서 넣고 빼는 것뿐이다.
- **소비자 소멸 시 캡처 파이프라인의 상태 전이는 0이다.** 살아있는 캡처 파이프라인을 건드리는
  일이 아예 없으므로, 이 저장소가 반복해서 데인 "라이브 파이프라인 teardown" 경로(RTSP
  teardown, 창닫기 지연 등)를 전환마다 밟지 않는다.
- 허브가 죽는 경로는 딱 하나 — **버스에 ERROR/EOS 가 와서 장치 자체가 죽었다고 판단될 때**뿐이다.
  그때는 `is unhealthy` 로 표시되고, **다음 요청이 새로 연다**(§3 참고). 페이지 전환은 이 경로에
  닿지 않는다.
- 적용 범위는 **비디오 입력에만**이다 (§6 한계 참고).

## 3. 확인용 로그와 그 의미

로그 소스는 `components/media/backends/gstreamer/capture_hub.rs`. 정확한 문자열:

| 로그 | 의미 |
|---|---|
| `capture hub: opened <key>` | 그 포트를 실제로 새로 열었다. **정상적인 전환 반복에서는 포트당 정확히 한 줄이어야 한다.** |
| `capture hub: reused <key>` | 이미 열려 있는 허브에 새 소비자가 합류했다(장치는 다시 안 열림) — 페이지 전환마다 나와야 하는 정상 로그. |
| `capture hub: <key> consumer <id> added (consumers=N)` | 소비자가 늘었다. `N` 은 그 포트에 현재 붙어 있는 소비자 수. |
| `capture hub: <key> consumer <id> removed (consumers=N)` | 소비자가 빠졌다(트랙 stop 또는 페이지 전환). |
| `capture hub: <key> is unhealthy; reopening` | 허브가 죽어서(장치 뽑힘 등) 재개방으로 넘어간다. |

**판정 규칙:**

- `capture hub: opened` 가 **두 줄 이상** 나오면 장치가 다시 열린 것이다. 이것 자체는 실패가
  아닐 수 있다 — 단, 그 직전에 반드시 `is unhealthy; reopening` 이 있어야 정당하다(장치가
  실제로 죽어서 재개방한 경우). `is unhealthy` 없이 `opened` 가 두 번째 나오면 버그다.
- 전환을 반복한 뒤 `consumers=` 의 **마지막 값이 0**이어야 한다(모든 소비자가 정리됨). 도중의
  값이 1보다 큰 순간이 있는 것은 정상이다 — 이전 페이지의 소비자가 아직 안 빠졌는데 새 페이지의
  소비자가 먼저 붙는 순간이 있을 수 있다. 최종적으로 열려 있는 페이지가 하나면 마지막 값은 1,
  창을 닫아 스트림이 전부 해제되면 0이다.
- **`capture hub: opened` 가 한 줄도 없으면 허브 실패가 아니라 페이지가 애초에 `getUserMedia`
  까지 도달하지 못했다는 뜻이다.** 이 로그는 허브가 실제로 열릴 때만 찍히므로, 프로브가
  `navigator.mediaDevices` 를 못 봤거나(§4의 `--pref dom_webrtc_enabled=true` 누락 — 가장 흔한
  원인이다) 장치 선택이 실패했거나(`?device=` 셀렉터가 아무 것도 못 찾음, 또는 `videoinput`
  0개) 둘 중 하나다. 이 경우 **먼저 pref 를, 그다음 HUD의 "videoinput devices:" 줄과 콘솔의
  `getUserMedia FAILED` 여부를 확인**하고, 그 뒤에도 재현되면 그때 코드 쪽을 의심한다 — "로그가
  0줄이니 허브가 깨졌다"로 바로 넘어가지 않는다.

### RUST_LOG 요구사항

**`RUST_LOG=servo_media_gstreamer=info` 가 반드시 있어야 로그가 보인다.** 이 백엔드 로그는
`script=info` 만으로는 안 잡힌다 — 흔히 `script=info` 만 켜고 "허브 로그가 안 나온다"고
오해하기 쉬우니 주의. 두 로거를 같이 켜려면:

```powershell
$env:RUST_LOG = "servo_media_gstreamer=info,script=info"
```

## 4. 실기 절차

테스트 장비에서, 이 브랜치 그대로 빌드된 `servoshell.exe` 로(아래 명령은 이 워크트리 루트 —
`target\debug\servoshell.exe` 와 `tests\html\` 가 보이는 위치 — 에서 실행한다).

**`--pref dom_webrtc_enabled=true` 가 반드시 있어야 한다.** 이 pref 는 기본값이 `false`
(`components/config/prefs.rs`)이고, 없으면 `navigator.mediaDevices` 자체가 `undefined`라
프로브 페이지가 `getUserMedia` 를 호출하지도 못한다 — 그러면 캡처 허브는 아예 손도 안 타서
아래 로그 확인에서 `capture hub: opened` 가 **한 줄도** 안 나온다(§3의 "0줄" 판정 참고).

```powershell
$env:RUST_LOG = "servo_media_gstreamer=info,script=info"
target\debug\servoshell.exe --pref dom_webrtc_enabled=true tests/html/multigpu_capture_card_probe.html 2> capture_cycles.err.log
```

프로브 페이지(`tests/html/multigpu_capture_card_probe.html`)가 뜨면 좌하단 컨트롤 패널에서:

1. `reload cycles` 입력칸에 원하는 반복 횟수(기본 20)를 넣는다.
2. `reload N times` 를 누른다 — 페이지가 자기 자신을 N회 자동 전환한다(전환 사이 2초 간격,
   상태 줄에 `cycle X/N` 이 표시되고 `X == N` 이 되면 `cycles done (N/N)` 으로 멈춘다).
3. 다 돌 때까지 기다린다(상태 줄이 `cycles done` 을 보여줄 때까지).
4. **창을 닫아서** 종료한다. **강제 종료(작업 관리자 등)하면 stderr 버퍼가 안 비워져 로그
   파일이 비거나 잘려서 남는다** — 빈 `.err.log` 는 "아무 문제도 없었다"가 아니라 "강제
   종료했다"는 뜻일 가능성이 높다.

종료 후 로그를 확인한다:

```powershell
Select-String -Path capture_cycles.err.log -Pattern "capture hub: opened"       # 포트당 1줄이어야 한다
Select-String -Path capture_cycles.err.log -Pattern "consumers="                # 마지막 값이 0이어야 한다
Select-String -Path capture_cycles.err.log -Pattern "panicked|not-negotiated"   # 0건이어야 한다
```

추가로 `stop() the current track` 버튼으로 트랙을 수동으로 멈춰본다 — 누른 직후
`capture hub: ... consumer ... removed` 가 찍히고 `consumers=` 가 하나 줄어야 한다(Task 6이
연결한 `MediaStreamTrack.stop()` 경로 확인).

### 월 팬아웃에서도 한 번 더

같은 절차를 `--wall-layout --wall-all-tiles` 로 반복한다(같은 로그 패턴을 확인):

```powershell
target\debug\servoshell.exe --wall-layout ..\config\wall_layout.local_3x1.json --wall-all-tiles --pref dom_webrtc_enabled=true tests/html/multigpu_capture_card_probe.html 2> capture_wall.err.log
```

### 판정

위 세 개의 `Select-String` 결과가 각각 "포트 수만큼 정확히 1줄(또는 그 앞에 `is unhealthy`가
있는 예외적 재개방)", "마지막 `consumers=` 값 0", "0건" 이면 통과다. 어느 하나라도 어긋나면
§3 의 판정 규칙으로 되짚어 어느 지점에서 재개방이 일어났는지 `Select-String -Context 3` 등으로
앞뒤 로그를 넓혀서 본다.

## 5. 환경 함정 (이 작업을 만들며 실제로 걸린 것들)

이 두 가지는 코드 문제가 아니라 **로컬 환경 상태**다. 겪으면 "기능이 고장났다"로 오귀인하기
쉬우므로 여기 기록한다.

### GST_PLUGIN_PATH — 캡처 허브 테스트가 13/20으로 죽는다

`mach build` 는 GStreamer 의 핵심 DLL들을 `target\debug` 에 **평평하게 복사**하고,
`scripts\servo_env.ps1` 가 그 경로를 PATH 맨 앞에 놓는다. 그러면 이후의 `cargo test` 프로세스가
시스템 GStreamer 대신 그 복사본을 로드하는데, **그 복사본이 내장하고 있는 상대경로 플러그인
디렉터리는 존재하지 않는다** — 그래서 플러그인이 하나도 안 잡히고, `servo-media-gstreamer` /
`servo-media-streams` 의 20개 테스트 중 13개가 0.01초만에 실패한다(플러그인 필요 엘리먼트를
못 만들어서). 고치려면 `GST_PLUGIN_PATH` 를 mach가 DLL을 복사해 온 원본 슬롯#1로 명시 지정한다:

```powershell
cd F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser
. .\scripts\servo_env.ps1
$ErrorActionPreference = 'Continue'
cd servo_multigpu-tiled-wall
$env:GST_PLUGIN_PATH = (Resolve-Path "target\dependencies\gstreamer\1.0\msvc_x86_64\lib\gstreamer-1.0").Path
cargo test -p servo-media-gstreamer --lib
cargo test -p servo-media-streams --lib
```

이 리포지토리의 커밋 기준으로 실측: 이 변수 없이 13/20 실패, 있으면 **20/20 통과, 14.29초**.

### NASM 이 PATH에 없다 — `aws-lc-sys` 빌드스크립트가 죽는다

`scripts\servo_env.ps1` 는 `C:\Program Files\NASM` 을 PATH에 넣지 않는다. 그래서 `aws-lc-sys`
의 빌드스크립트를 다시 돌려야 하는 (캐시가 무효화된) 어떤 `cargo` 호출이든
`"NASM command not found!"` 로 죽을 수 있다. 이 브랜치의 두 테스트 크레이트 자체는 보통
캐시된 상태라 이 문제를 만나지 않지만, 클린 빌드/의존성 갱신 이후에는 만날 수 있다. NASM이
설치돼 있다면 PATH에 `C:\Program Files\NASM` 을 추가하고 재시도한다.

## 6. 알려진 한계

- **오디오 입력과 `getDisplayMedia` 는 허브를 쓰지 않는다.** 이 장비에서 오디오 입력은 포트
  배타성 문제가 없고(현재 빌드는 `audioinput` 0개), `getDisplayMedia` 도 마찬가지다. 두 경로
  모두 §2의 "수명 해제"(문서 종료 시 `GlobalScope::release_capture_streams` 가 스트림을
  정리하는 것)는 동일하게 적용받지만, 허브를 거치지 않으므로 반복 열기 자체를 하나로 묶어주지
  않는다. 오디오 캡처카드가 배타성 문제를 보이면 그때 허브를 확장해야 한다.
- **`MediaStreamTrack.clone()` 이 `MediaStreamId` 를 공유한다.** 클론된 트랙은 원본과 같은
  스트림 id 를 가리키므로, 클론 하나를 `stop()` 하면 원본을 포함해 전부 멈춘다. Web 표준
  위반이지만, 고치려면 트랙별로 독립된 스트림 복제가 필요해 별건으로 남겨둔다.
  `mediastreamtrack.rs` 의 `Clone`/`Stop` 구현에 주석으로도 남아 있다.
- **장치가 (에러를 내지 않고) 멈춰버리면 허브는 건강하다고 잘못 판단한다.** 허브가 여는 시점의
  건강 확인은 파이프라인이 `PLAYING` 상태 전이에 성공했는지만 본다. 비동기 상태 전이가
  "성공"으로 잡히는 경우, 장치가 실제로는 응답을 멈췄어도(에러/EOS 를 내지 않고 그냥 hang) 허브는
  계속 건강한 것으로 저장된다 — 이런 장애는 버스 이벤트로 감지되지 않으므로 재개방되지 않는다.
- **스크립트 스레드가 패닉하면 캡처 스트림이 해제되지 않는다.** §2/§3의 수명 해제는
  `Window::clear_js_runtime()`(정상적인 문서 종료 경로)에 걸려 있다. 스크립트 스레드가
  패닉으로 죽으면 이 경로를 거치지 않으므로, 그 문서가 열었던 캡처 스트림은 해제되지 않고
  장치는 **프로세스가 끝날 때까지** 계속 물려 있는다. 정상 종료(창 닫기)에는 영향이 없다.

## 7. 스코프 밖 (이 문서/브랜치가 다루지 않는 것)

- GPU zero-copy 캡처 경로(gstgl EGL/ANGLE 필요) — 별건으로 보류 중.
- 캡처 프레임의 멀티 GPU 분배.
- `MediaStreamTrack::clone()` 의 id 공유 결함 수정(위 §6에 기록만 함).

# 캡처카드 단일 연결 공유 — 운용 문서 (2026-09-04)

브랜치 `capture-card-shared-connection`. 설계: `docs/superpowers/specs/2026-09-04-capture-card-shared-connection-design.md`.
계획: `docs/superpowers/plans/2026-09-04-capture-card-shared-connection.md`. 코드: `components/media/backends/gstreamer/capture_hub.rs`.

이 문서는 **캡처카드가 실제로 물린 장비에서** 이 브랜치를 검증하려는 사람을 위한 것이다.
개발기에는 캡처카드가 없어서, 여기 적힌 것 중 "실기 절차" 절 이전은 전부 하드웨어 없이 도는
자동 테스트(21건)로 이미 증명되어 있다. 실기에서 확인해야 하는 것은 딱 하나 — **페이지를
반복 전환해도 같은 물리 포트가 한 번만 열리는가**이다.

> **2026-09-04: 그 실기 검증은 통과했다** (§4의 "실기 결과"). 이 문서를 절차서로 다시 쓸
> 일이 있다면 그 절의 수치를 기준선으로 삼으면 된다.

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
  `navigator.mediaDevices` 를 못 봤거나(winit_wall 이면 `-PageFeatures` 누락, servoshell 이면
  `--pref dom_webrtc_enabled=true` 누락 — 가장 흔한 원인이다) 장치 선택이 실패했거나(`?device=` 셀렉터가 아무 것도 못 찾음, 또는 `videoinput`
  0개) 둘 중 하나다. 이 경우 **먼저 pref 를, 그다음 HUD의 "videoinput devices:" 줄과 콘솔의
  `getUserMedia FAILED` 여부를 확인**하고, 그 뒤에도 재현되면 그때 코드 쪽을 의심한다 — "로그가
  0줄이니 허브가 깨졌다"로 바로 넘어가지 않는다.

### RUST_LOG 요구사항

**`RUST_LOG=servo_media_gstreamer=info` 가 반드시 있어야 로그가 보인다.** 이 백엔드 로그는
`script=info` 만으로는 안 잡힌다 — 흔히 `script=info` 만 켜고 "허브 로그가 안 나온다"고
오해하기 쉬우니 주의.

**`run_wall_dist.ps1` 로 돌릴 때는 신경 쓸 필요가 없다** — 런처가 `servo_media_gstreamer=info`
를 포함해 `RUST_LOG` 를 무조건 설정한다. 직접 실행할 때만 다음이 필요하다:

```powershell
$env:RUST_LOG = "servo_media_gstreamer=info,script=info"
```

## 4. 실기 절차

**이 절차는 후속 작업이 아니라 머지 게이트다.** `create_input_stream`(`media_capture.rs`)의
비디오 갈래는 자동 테스트로 전혀 덮이지 않는다 — 그 갈래를 예전 `get_track` 경로로 되돌려도
(즉 이 브랜치의 핵심 변경을 통째로 되돌려도) 21개 단위 테스트는 그대로 전부 통과한다. 그
21개는 **허브 자체가 옳다는 것**만 증명한다: 포트 재사용, 소비자 배포, 재개방 판단, 락 오염
내성. Servo가 실제로 그 허브를 쓰고 있는가 — 즉 `getUserMedia()` 가 진짜로 허브를 거쳐서
같은 포트를 두 번 열지 않는가 — 는 오직 이 실기 절차만 증명한다. 이 절이 안 돌았다면 이
브랜치는 검증되지 않은 것이다.

### 실기 결과 (2026-09-04) — 통과

테스트 장비 4-GPU 월, winit_wall 배포본, `?cycles=10,hold3` 및 `?cycles=10,stop,hold3`.

| 항목 | stop 없이 | `,stop` | 기대값 |
|---|---|---|---|
| `capture hub: opened` | **1** | **1** | 포트당 1 |
| `capture hub: reused` | **9** | **9** | 전환 횟수 − 1 |
| consumer added / removed | 10 / 9 | 10 / 9 | added = 전환 횟수 |
| `is unhealthy` / `closing` | 0 | 0 | 0 |
| `panicked` / `not-negotiated` | 0 / 0 | 0 / 0 | 0 |

10회 전환 동안 물리 포트는 **정확히 한 번** 열렸다. `removed` 가 9인 것은 정상이다 — 마지막
문서의 소비자는 아직 살아 있는 채로 `-DurationSec` 이 프로세스를 끊는다. 소비자 수명은
`08:38:04 added → 08:38:07 removed` 처럼 3~4초(= `hold3`), 사이클 주기는 17~18초였고, 매
사이클 `consumers=0` 으로 복귀해 누적이 없었다. 영상은 육안으로도 확인됐다.

**이것으로 §4 의 머지 게이트는 충족됐다.** 남은 관측 하나는 아래 "미해결" 절에 있다.

### 셸은 winit_wall 이고, 클릭은 못 한다

실기는 **winit_wall 배포본**으로 돌린다. 개발기에서 만들고 테스트 장비로 복사한다:

```powershell
# 개발기 (mozangle 경로 길이 때문에 반드시 subst 드라이브 경유 — Os error 206 방지)
subst W: F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser   # 이미 있으면 생략
. W:\scripts\servo_env.ps1
$ErrorActionPreference = 'Continue'
cd W:\servo_multigpu-tiled-wall
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
.\etc\multigpu\make_wall_dist.ps1 -Force        # -> target\wall_dist (444 DLL / 0.44 GB)
```

`make_wall_dist.ps1` 은 패키징 전에 ANGLE LUID 패치와 `webgpu` 피처 컴파일 여부를 **하드페일로**
검증한다(둘 다 없으면 조용히 잘못 도는 종류의 실패라서). `target\wall_dist` 폴더를 통째로 테스트
장비에 복사하면 Rust·GStreamer·ANGLE 설치 없이 실행된다.

**★winit_wall 은 입력을 엔진에 전달하지 않는다★** — 이벤트 루프가 `CloseRequested` 와
`RedrawRequested` 만 처리한다(`winit_wall/main.rs`). 그래서 프로브 페이지 좌하단의
`stop() the current track` / `reload N times` **버튼은 여기서 누를 수 없다.** 대신 쿼리
문자열로 구동한다:

- `?cycles=N` — 클릭 없이 N회 페이지 전환을 자동 수행.
- `?cycles=N,stop` — 거기에 더해 매 전환 직전 `track.stop()` 호출.
- `?cycles=N,hold5` — 영상이 실제로 뜬 뒤 붙잡고 있을 초(기본 3).

**`hold` 가 왜 필요한가.** 처음엔 전환 타이머를 *페이지 로드* 시점부터 쟀는데, 이 장비에서는
`getUserMedia` 가 그 2초를 거의 다 써버린다. 2026-09-04 실측 로그에서 `consumer N added` 와
`consumer N removed` 가 **같은 초**에 찍혔다 — 14초 주기 중 영상이 1초도 안 살아 있었고,
월을 보고 있어도 **아무것도 안 보였다**. 지금은 `videoWidth > 0` 이고 `currentTime` 이
움직이기 시작한 뒤부터 `hold` 초를 세고 넘어간다(최대 20초까지 기다렸다가, 안 뜨면 그 사이클은
그냥 넘어간다 — 포트 하나가 안 열린다고 실행 전체가 멈추면 안 되므로).

**쉼표 하나짜리 파라미터인 것에 이유가 있다.** 런처는 URL 을 `cmd start` 를 통해 엔진에
넘기는데 `cmd` 에서 `&` 는 명령 구분자다. `&stop=1` 형태는 도중에 잘려도 페이지는 멀쩡히
뜨기 때문에 **조용히 무시된 채로 통과한 것처럼 보인다.** 그래서 `&` 를 아예 안 쓴다.

전환 카운터도 **URL 에 실려서** 넘어간다(`?cycles=20,stop,d3` 의 `d3`). `sessionStorage`
를 쓰지 않는 이유는 §5 에 있다 — 이 장비에서 그게 죽어 있고, 그 실패가 "통과처럼 보이는
0회 실행"을 만들어냈다.

### 1회차 — 기본 경로 (stop 없이)

이게 **보고된 크래시를 실제로 일으키던 경로**다. 실제 웹페이지는 `stop()` 을 부르지 않고,
해제는 파이프라인 종료 시 `release_capture_streams` 가 거둔다.

```powershell
# 테스트 장비, 복사해 온 wall_dist 폴더 안에서
.\run_wall_dist.ps1 -PageFeatures -Serve -DurationSec 90 `
  -Url "multigpu_capture_card_probe.html?cycles=10,hold3"
```

- `-PageFeatures` 가 `dom_webrtc_enabled` 를 켠다. **이게 없으면 `navigator.mediaDevices` 가
  `undefined` 라 페이지가 `getUserMedia` 를 호출조차 못 하고, 로그에 `capture hub: opened` 가
  한 줄도 안 나온다**(§3의 "0줄" 판정 참고). 허브 문제로 오독하기 딱 좋은 형태다.
- `-Serve` 는 `pages\html` 을 http 로 띄우고 상대 URL(쿼리 문자열 포함)을 그 위에서 푼다.
- **`-DurationSec` 은 넉넉히 잡는다.** 2026-09-04 실측에서 한 사이클은 약 **14초**였다(대부분이
  새 문서 로드와 `getUserMedia`이고, 영상 표시 시간이 아니었다). 여기에 `hold` 가 더해지므로
  대략 **`N * (12 + hold) + 30`초** 로 잡는다 — 예: `?cycles=10,hold3` 이면 `-DurationSec 200`.
  모자라면 사이클이 다 안 돌고 끝나며, 그건 `capture hub: reused` 개수가 기대보다 적은 것으로
  드러난다(페이지의 `cycle X/N` 콘솔 줄은 런처 로그에 안 남는다 — 아래 판정 절 참고).
- **전환 횟수는 10회면 충분하다.** 이 절차가 증명하려는 건 "반복 전환해도 포트를 다시 안 연다"
  이고, 그건 `reused` 가 9쯤 쌓이면 성립한다. 20회는 실행 시간만 두 배로 만든다.
- `-Layout` 은 주지 않는다 — 기본값 `wall_layout.multigpu.json`(4-GPU 월)이 맞다.
- `RUST_LOG` 도 주지 않는다. 런처가 `servo_media_gstreamer=info` 를 포함해 무조건 설정한다.
- 로그는 런처가 `wall_<날짜시각>.err.log` 로 남기고, 끝난 뒤 스스로 요약을 출력한다.

### 2회차 — stop 경로

```powershell
.\run_wall_dist.ps1 -PageFeatures -Serve -DurationSec 90 `
  -Url "multigpu_capture_card_probe.html?cycles=10,stop,hold3"
```

두 경로를 **일부러 분리해서** 돌린다. 섞으면 실패했을 때 어느 해제 경로가 깨졌는지 구분할 수
없고, `stop()` 을 먼저 부르면 파이프라인 종료 경로는 이미 빈 목록을 보게 돼 사실상 미검증으로
남는다.

### 로그 판정

```powershell
$log = Get-ChildItem wall_*.err.log | Sort-Object LastWriteTime | Select-Object -Last 1
(Select-String -Path $log -Pattern "capture hub: opened" -SimpleMatch | Measure-Object).Count  # 포트당 1
(Select-String -Path $log -Pattern "capture hub: reused" -SimpleMatch | Measure-Object).Count  # ★전환 횟수-1★
(Select-String -Path $log -Pattern "panicked"            -SimpleMatch | Measure-Object).Count  # 0
Select-String -Path $log -Pattern "consumers=" -SimpleMatch | Select-Object -Last 6            # 끝부분 확인
```

**★`reused` 가 이 절차의 결정적 신호다 — 처음 쓴 판정 기준엔 이게 빠져 있었다★**

2026-09-04 첫 실기에서 `opened=1`, `panicked=0` 이 나와 당시 기준으로는 통과처럼 보였다.
그런데 같은 로그의 `reused` 는 **0** 이었고 `consumer 1 added (consumers=1)` 이 딱 한 줄뿐이었다
— `getUserMedia` 가 전체 실행에서 **한 번만** 불렸다는 뜻, 즉 **전환이 0회**였다. 아무것도
검증하지 않은 실행이 통과로 읽혔다.

이유는 단순하다. `opened` 는 **"포트를 두 번 열지 않았다"** 만 말한다. 전환이 아예 없었어도
그 조건은 만족된다. **"전환이 실제로 일어났다"를 말하는 건 `reused` 뿐이다.** 따라서
`reused == 0` 이면 나머지가 아무리 깨끗해도 **판정 불가(무효 실행)** 로 다룬다.

통과 조건은 네 가지다.

1. **`opened` 가 사용한 포트 수만큼만.** 20회 전환에도 포트당 1줄. 2줄 이상이면 바로 앞에
   `is unhealthy; reopening` 이 있어야 정당한 재개방이고, 없으면 실패다.
2. **`reused` 가 대략 (전환 횟수 − 1) 만큼.** 0이면 무효 실행 — 아래 "0회로 끝났을 때"로 간다.
3. **`consumers=` 의 added 와 removed 가 균형**을 이루고 끝에서 1(창이 살아있을 때) 또는
   0(종료 후)으로 돌아온다. 단조 증가하면 소비자가 누적되는 것이다.
4. **`panicked` / `not-negotiated` 0건.**

### 전환이 0회로 끝났을 때 (reused=0)

허브를 의심하기 전에 이 순서로 본다.

- **`capture hub: opened` 도 0줄인가?** 그러면 페이지가 `getUserMedia` 에 도달조차 못 한 것이다
  — `-PageFeatures` 누락이거나 장치 선택 실패(`?device=` 가 아무것도 못 찾음, 또는
  `videoinput` 0개)다.
- **`opened=1` 인데 `reused=0` 인가?** 스트림은 열렸는데 페이지가 전환을 안 한 것이다. URL 에
  `?cycles=N` 이 실제로 들어갔는지 확인한다. 런처가 `cmd start` 를 거치므로 URL 에 `&` 가
  있으면 거기서 잘릴 수 있다 — 그래서 이 페이지는 `?cycles=20,stop` 처럼 `&` 없는 문법을 쓴다.
- 화면 좌하단 상태 줄(`cycle X/N`)이 갱신되는지 본다. 페이지 자신의 `console.log` 는 런처
  로그에 안 남으므로(아래) 화면이 유일한 직접 신호다.

페이지의 `console.log`(`[capture-card-probe] cycle 3/10 ...`)는 **런처 로그에 안 남는다.**
`script=info` 를 켜도 안 남는다 — 2026-09-04 실측: `-KeepRustLog` 로 `script=info` 를 켠
실행에서도 `[capture-card-probe]` 는 **0줄**이었다(`INFO script` 자체는 32줄 나왔다). 즉
페이지 콘솔은 이 경로로는 못 본다. 진행 상황은 **화면 좌하단 상태 줄**과 로그의 허브 라인
(`reused`, `consumer ... added/removed`)으로 판단한다. `script=info` 자체가 필요하면:

```powershell
$env:RUST_LOG = "warn,paint=info,media=info,winit_wall=info,servo_media_gstreamer=info,script=info"
.\run_wall_dist.ps1 -KeepRustLog -PageFeatures -Serve -DurationSec 90 `
  -Url "multigpu_capture_card_probe.html?cycles=10,hold3"
```

판정에 필요한 신호는 전부 허브 로그에 있으므로 보통은 필요 없다.

### ★미해결 — stop 없는 실행이 도중에 멈춘 적이 있다 (2026-09-04)★

첫 유효 실기에서 관측된 것:

| | stop 없이 | `,stop` |
|---|---|---|
| `capture hub: opened` | 1 | 1 |
| `capture hub: reused` | 1 | 5 |
| consumer added / removed | 2 / 2 | 6 / 6 |
| 전환 | **2회에서 멈춤** | `-DurationSec` 끝까지 |

멈춘 뒤 로그에는 **62초 동안 `paint` 만 흐르고 WARN/ERROR 는 0건**이었다. 엔진이 죽은 게 아니라
**스크립트 쪽 활동만 사라진** 모양이다.

두 실행의 유일한 차이는 **해제가 언제 일어나는가**다. `,stop` 은 네비게이션 *전에* 스트림을
놓으므로 파이프라인 종료 시 `release_capture_streams` 가 빈 목록을 본다. stop 없는 쪽은 종료
그 순간에 해제가 일어나고, `GStreamerMediaStream::drop` 이 **스크립트 스레드 위에서**
`pipeline.set_state(Null)` 을 호출한다 — 그 파이프라인의 `proxysink` 가 아직 `PLAYING` 인
`proxysrc` 와 짝지어진 채로. 이건 §6 에 적어둔 보류 항목(teardown 위험)과 정확히 같은 지점이다.

**★그 뒤 재현되지 않았다 — 그리고 그건 고쳤다는 뜻이 아니다★**

`hold` 를 넣어 스트림이 3초 이상 살아 있게 만든 뒤의 실행(같은 날 `log_capturecard/02`)에서는
**stop 유무와 무관하게 10회 전환이 모두 정상 완료**됐다. 하지만 고친 것은 **프로브의 타이밍**
이지 엔진이 아니다. 멈춤이 관측됐을 때 스트림 수명은 **0.x초**였고 지금은 3~4초다 — 만약 저것이
teardown 경쟁 상태였다면, 타이밍이 바뀌어 안 걸리게 된 것일 뿐 사라진 게 아니다.

현실의 페이지는 영상을 몇 초씩 띄우므로 지금 통과한 형태가 실사용에 더 가깝다. 그래도 이건
**설명되지 않은 채 남은 관측**으로 취급한다. 다시 재현되면 **먼저 이걸 돌려서 script 쪽이
실제로 멈추는지** 가른다 — 재빌드 없이 가능한 판별 실험이다:

```powershell
$env:RUST_LOG = "warn,paint=info,media=info,winit_wall=info,servo_media_gstreamer=info,script=info"
.
un_wall_dist.ps1 -KeepRustLog -PageFeatures -Serve -DurationSec 200 `
  -Url "multigpu_capture_card_probe.html?cycles=10,hold3"
```

- 멈춘 뒤에도 새 문서의 script 로그가 이어지면 → 스크립트 스레드는 살아있고 **페이지 로직**
  문제다(전환 URL 이 안 만들어졌는지 등).
- 멈춘 시점 이후 script 로그가 완전히 끊기면 → **스크립트 스레드가 teardown 에서 막힌 것**이고,
  그때는 해제 경로에 소요시간 계측을 넣어 확정한다(엔진 재빌드 필요).

### 시간을 재야 할 두 지점

이 브랜치는 캡처 장치 개방을 스크립트 스레드로 옮겼고, 대칭적으로 해제도 스크립트 스레드
경로에 걸려 있다. 둘 다 이 저장소가 전에 실측으로 걸렸던 종류의 정지이므로, 실기에서
**초 단위로 재서** 기록한다:

- **런치 후 첫 `getUserMedia`.** 그 포트의 첫 개방은 스크립트 스레드가
  `DeviceHub::open`(`capture_hub.rs`) 안에서 최대 `START_TIMEOUT`(5초)까지 블록할 수 있다 —
  전에는 장치가 플레이어 스레드에서 나중에 열렸지만, 지금은 `getUserMedia()` 호출 자체가
  이 블록을 포함한다.
- **런이 끝나는 순간.** `release_capture_streams`(`globalscope.rs`)가
  `Window::clear_js_runtime`(`window.rs`) 맨 앞에서 돌며 캡처 파이프라인을 `Null` 로 내리는데,
  그 파이프라인의 `proxysink` 가 플레이어 쪽 `PLAYING` 상태의 `proxysrc` 와 여전히 peer 로
  연결돼 있을 수 있다. 이 저장소는 라이브 파이프라인 teardown 에서 수 초짜리 정지를 반복해서
  기록한 전례가 있다(RTSP teardown 등).

**문제처럼 보이는 것 = 눈에 보이는 정지**(화면이 얼어붙거나 창이 응답하지 않는 것). 둘 중
하나라도 관찰되면 정확한 지점과 걸린 시간을 함께 보고할 가치가 있다 — 이 문서는 그 재구조화를
지금 하지 않기로 한 결정 위에 있으므로, 실측이 다음 판단의 근거가 된다.

### 로그가 잘려 보이면

`-DurationSec` 은 시간이 다 되면 프로세스를 강제 종료한다(`Stop-Process -Force`). 런처의 사후
분석 전체가 이 방식의 로그에 기대고 있으므로 정상 경로이지만, 만약 로그 끝이 잘린 것처럼
보이면 `-DurationSec` 없이 돌리고 **창을 닫아서** 끝낸 뒤 다시 확인한다.

### 참고 — servoshell 로 확인하고 싶을 때

버튼을 실제로 눌러 보거나 대화형으로 확인하려면 servoshell 을 쓴다. 이쪽은 입력이 동작하므로
좌하단 컨트롤 패널의 두 버튼이 그대로 먹는다(이 경로는 실기 게이트가 아니라 보조 수단이다).

```powershell
target\debug\servoshell.exe --pref dom_webrtc_enabled=true tests/html/multigpu_capture_card_probe.html 2> capture_cycles.err.log
```

## 5. 환경 함정 (이 작업을 만들며 실제로 걸린 것들)

이 두 가지는 코드 문제가 아니라 **로컬 환경 상태**다. 겪으면 "기능이 고장났다"로 오귀인하기
쉬우므로 여기 기록한다.

### GST_PLUGIN_PATH — 캡처 허브 테스트가 13/21로 죽는다

`mach build` 는 GStreamer 의 핵심 DLL들을 `target\debug` 에 **평평하게 복사**하고,
`scripts\servo_env.ps1` 가 그 경로를 PATH 맨 앞에 놓는다. 그러면 이후의 `cargo test` 프로세스가
시스템 GStreamer 대신 그 복사본을 로드하는데, **그 복사본이 내장하고 있는 상대경로 플러그인
디렉터리는 존재하지 않는다** — 그래서 플러그인이 하나도 안 잡히고, `servo-media-gstreamer` 의 21개 테스트 중 13개가 0.01초만에 실패한다(플러그인 필요 엘리먼트를
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

- **winit_wall 은 입력을 엔진에 전달하지 않는다.** 이벤트 루프가 `CloseRequested` 와
  `RedrawRequested` 만 처리하므로(`components/servo/examples/winit_wall/main.rs`), 프로브
  페이지의 버튼은 실기 셸에서 누를 수 없다. 그래서 이 페이지는 `?cycles=N` / `&stop=1` 쿼리
  파라미터로도 구동되게 만들어 뒀다(§4). 앞으로 실기용 페이지를 추가할 때도 **클릭에 의존하는
  진입점만 두면 winit_wall 에서 시작 자체가 불가능하다**는 점을 염두에 둘 것.
- **오디오 입력과 `getDisplayMedia` 는 허브를 쓰지 않는다.** 이 장비에서 오디오 입력은 포트
  배타성 문제가 없고(현재 빌드는 `audioinput` 0개), `getDisplayMedia` 도 마찬가지다. 두 경로
  모두 §2의 "수명 해제"(문서 종료 시 `GlobalScope::release_capture_streams` 가 스트림을
  정리하는 것)는 동일하게 적용받지만, 허브를 거치지 않으므로 반복 열기 자체를 하나로 묶어주지
  않는다. 오디오 캡처카드가 배타성 문제를 보이면 그때 허브를 확장해야 한다.
- **`MediaStreamTrack.clone()` 이 `MediaStreamId` 를 공유한다.** 클론된 트랙은 원본과 같은
  스트림 id 를 가리키므로, 클론 하나를 `stop()` 하면 원본을 포함해 전부 멈춘다. Web 표준
  위반이지만, 고치려면 트랙별로 독립된 스트림 복제가 필요해 별건으로 남겨둔다.
  `mediastreamtrack.rs` 의 `Clone`/`Stop` 구현에 주석으로도 남아 있다.
- **(해결됨, 2026-09-04 리뷰 수정)** ~~장치가 (에러를 내지 않고) 멈춰버리면 허브는 건강하다고
  잘못 판단한다~~ — `DeviceHub::open` 의 건강 확인이 `pipeline.state(START_TIMEOUT).0.is_err()`
  만 봤는데, `Ok(StateChangeSuccess::Async)`(START_TIMEOUT 이 다 되도록 전이가 안 끝난 상태)는
  `is_err()` 가 아니라서 걸러지지 않았다. 이제 `Success`/`NoPreroll` 만 통과시키고 그 외
  (`Async` 포함)는 실패로 처리해 슬롯에 저장하지 않는다.
- **스크립트 스레드가 패닉하면 캡처 스트림이 해제되지 않는다.** §2/§3의 수명 해제는
  `Window::clear_js_runtime()`(정상적인 문서 종료 경로)에 걸려 있다. 스크립트 스레드가
  패닉으로 죽으면 이 경로를 거치지 않으므로, 그 문서가 열었던 캡처 스트림은 해제되지 않고
  장치는 **프로세스가 끝날 때까지** 계속 물려 있는다. 정상 종료(창 닫기)에는 영향이 없다.
- **허브의 caps 는 처음 연 쪽이 고정한다.** 허브는 포트당 하나의 파이프라인이고 그 `capsfilter`
  는 첫 `getUserMedia()` 가 장치를 열 때 결정된 포맷을 쓴다. 이후 다른 `width`/`height`/
  `frameRate` 제약으로 같은 포트를 여는 두 번째 `getUserMedia()` 는 자기 제약이 조용히
  무시되고 첫 호출자의 포맷을 그대로 받는다 — 지금 제약은 **장치 선택**에만 쓰이고 **포맷
  협상**에는 쓰이지 않는다.

## 7. 스코프 밖 (이 문서/브랜치가 다루지 않는 것)

- GPU zero-copy 캡처 경로(gstgl EGL/ANGLE 필요) — 별건으로 보류 중.
- 캡처 프레임의 멀티 GPU 분배.
- `MediaStreamTrack::clone()` 의 id 공유 결함 수정(위 §6에 기록만 함).

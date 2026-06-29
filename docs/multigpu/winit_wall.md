# winit_wall — 경량 멀티-GPU 월 테스트 하니스

`components/servo/examples/winit_wall.rs`. servoshell이 무거워서(egui UI, 멀티 WebView,
WebDriver, AccessKit 등) 렌더/성능 테스트엔 과하므로, `winit_minimal` 예제를 베이스로 **한 로직
WebView를 N개 타일 윈도우로 fan-out**하는 코어만 담은 예제. servoshell의 `--wall-layout` /
`--wall-all-tiles` 와 같은 모델이지만 입력 좌표 리맵은 생략(상호작용/콘솔 테스트는 servoshell 사용).

## 빌드

먼저 환경을 소싱한 뒤(반드시 dot-source), servo/ 안에서 빌드한다.

```powershell
. .\scripts\servo_env.ps1            # vcvars + rustup/cargo + PKG_CONFIG_PATH(forward-slash)
cd servo
cargo build --release -p servo --example winit_wall --features media-gstreamer,no-wgl
```

산출물: `target\release\examples\winit_wall.exe`.

플래그가 **모두 필수**다(빠지면 실패):

| 옵션 | 이유 |
|------|------|
| `-p servo` | 워크스페이스 default 멤버가 servoshell이라 없으면 `no example target named winit_wall` |
| `--features no-wgl` | servo-webgl이 surfman ANGLE의 `create_isolated_device`(no-wgl 전용)를 사용 → 없으면 컴파일 에러. **실제 per-GPU 선택(DXGI)·디스플레이 토폴로지 열거에도 필수** |
| `--features media-gstreamer` | 비디오/캡처 프로브용. 영상 프로브를 안 쓸 거면 생략 가능하나, servoshell과 동일 feature로 두면 아티팩트 재사용에 유리 |

- 예제 빌드는 servo의 dev-dependencies를 그래프에 끌어와 feature 조합이 servoshell 빌드와 달라져
  **첫 빌드는 상당량 재컴파일**(이후 증분은 빠름).
- `no-wgl` 없이도 빌드는 되며(디스플레이 토폴로지는 빈 폴백 → winit 모니터 인덱스 동작), 멀티-GPU
  선택은 동작하지 않는다.

## 실행

예제 exe는 `target\release\examples\`에 생기는데 mach가 servoshell처럼 DLL을 패키징해 주지 않으므로
**런타임 DLL(ANGLE `libEGL`/`libGLESv2` + GStreamer)을 exe 옆에 둬야 한다**(안 하면
`egl function was not loaded` 패닉). 최초 1회:

```powershell
Copy-Item target\release\*.dll target\release\examples\
```

실행(2x1 월 예):

```powershell
target\release\examples\winit_wall.exe `
  --wall-layout ..\config\wall_layout.local_2x1.json --wall-all-tiles `
  --pref dom_webrtc_enabled=true `
  tests\html\multigpu_wall_sync_probe.html
```

CLI 플래그:

| 플래그 | 설명 |
|--------|------|
| `--wall-layout <path>` | 레이아웃 JSON (필수) |
| `--wall-all-tiles` | 타일마다 윈도우 하나, 모두 하나의 WebView 공유(fan-out) |
| `--wall-tile-index <n>` | 단일 타일 미리보기(기본 0) |
| `--pref name[=value]` | Servo pref (servoshell과 동일; bare/`=true`→bool, 그 외 booleanish). 반복 가능 |
| `<URL 또는 경로>` | positional. 상대/절대 파일 경로 가능(자동 `file://` 변환), 기본은 데모 페이지 |

## 레이아웃 JSON — 공간 디스플레이 인덱스 + GPU 자동 할당

타일은 **공간 디스플레이 인덱스 `display`** 하나로 물리 디스플레이를 가리킨다(좌상단=0, 좌→우 우선,
그다음 위→아래). 앱이 런타임에 DXGI 토폴로지를 열거해 (a) 그 디스플레이의 실제 desktop 좌표로 창을
배치하고 (b) 그 디스플레이를 **구동하는 GPU(adapter)를 자동 할당**한다. `gpu` 필드는 없다(없앴음).

```json
{
  "virtualViewport": { "width": 3840, "height": 1080 },
  "tiles": [
    { "display": 0, "rect": [0, 0, 1920, 1080] },
    { "display": 1, "rect": [1920, 0, 1920, 1080] }
  ],
  "overlapPx": 32
}
```

- 레거시 `monitor`는 `display`의 별칭으로 수용(경고), 레거시 `gpu`는 무시(경고).
- 예제 config: `..\config\wall_layout.local_*.json`(개발기), `etc\multigpu\config\wall_layout.*.json`(샘플).
- 구현: 토폴로지 헬퍼는 `components/shared/paint/rendering_context.rs`의
  `enumerate_display_topology()` / `spatial_order()`(EnumAdapters1→EnumOutputs로 desktop 좌표·
  LUID·구동 adapter index 수집; adapter index가 곧 `requested_gpu_index`). 배치/할당은
  `winit_wall.rs` `resumed()`.

## 진단 / 검증

GUI는 화면에만 뜨므로 stderr를 파일로 받는다. 부팅 시 **디스플레이 토폴로지**가 stderr로 찍힌다
(eprintln; `setup_logging` 이전이라 log 크레이트가 아니라 stderr 사용):

```
Wall display topology (2 desktop display(s)):
  display 0: \\.\DISPLAY2 rect[0,0 1920x1080] adapter 0 luid 00000000:00028c1b
  display 1: \\.\DISPLAY1 rect[1920,0 1920x1080] adapter 0 luid 00000000:00028c1b
tile 0: display 0 -> \\.\DISPLAY2 rect[0,0 1920x1080] adapter 0 luid 00000000:00028c1b
tile 1: display 1 -> \\.\DISPLAY1 rect[1920,0 1920x1080] adapter 0 luid 00000000:00028c1b
```

여기서 좌측(x=0) 물리 디스플레이가 spatial 0으로 정렬됨을 확인한다(Windows DISPLAY 번호와 무관).
멀티-GPU 월에선 디스플레이별로 다른 adapter/LUID가 자동 배정된다.

월 배리어·adapter 선택 로그는 `log`(RUST_LOG):

```powershell
$env:RUST_LOG = "warn,paint=info"
target\release\examples\winit_wall.exe --wall-layout ..\config\wall_layout.local_2x1.json `
  --wall-all-tiles tests\html\multigpu_wall_sync_probe.html 2> ww.err.log
```

확인 포인트:
- `Wall frame barrier complete ... ready=N/N` — 전 타일 paint target 등록·동기.
- `Selected DXGI adapter index N for requested target GPU`(`paint_api::rendering_context`) —
  타일별로 다른 N이면 per-GPU 분산 동작(요청 GPU가 `Some`일 때만 출력).
- `warning: tile ...: adapter ... LUID mismatch` 가 없을 것(토폴로지↔rendering-context LUID 일치).
- per-GPU 실제 렌더는 ANGLE LUID 캐시 패치(`etc/multigpu/patches/`)에 의존.

> **창을 닫아서 종료**해야 stderr가 flush된다(강제 종료하면 버퍼 로그 유실 → 빈 .err.log).

단위 테스트(공간 정렬):

```powershell
cargo test -p servo-paint-api --features no-wgl display_topology_tests
```

## 배포본(다른 머신으로 이동)

`etc/multigpu/make_winit_wall_dist.ps1` 이 exe + 전체 런타임 DLL(GStreamer 커스텀 + ANGLE + VC
런타임 + d3dcompiler) + 프로브 페이지 + test_media + config를 자기완결 폴더로 묶는다.

```powershell
pwsh .\etc\multigpu\make_winit_wall_dist.ps1            # 조립만(exe 사전 빌드 필요)
pwsh .\etc\multigpu\make_winit_wall_dist.ps1 -Build     # 위 빌드 명령부터 수행
pwsh .\etc\multigpu\make_winit_wall_dist.ps1 -OutDir D:\winit_wall_kit
```

타깃에 GStreamer/VC 재배포판 설치 불필요(리소스는 exe 내장, 플러그인은 exe 옆에서 로드). 상세는
스크립트 헤더와 생성되는 `README.txt` 참조.

## 한계
- 입력 좌표 리맵 없음 → 클릭 좌표 안 맞음(상호작용은 servoshell).
- 페이지 `console.log`는 터미널로 라우팅되지 않음(검증은 stderr/배리어 로그로).
- 공간 디스플레이/자동 GPU는 현재 winit_wall 예제에만 적용(servoshell은 후속).

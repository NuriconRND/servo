# 6x6 비디오 그리드 성능 회귀 — b0a7ce85 대조군 실행 가이드

작성 2026-08-21. 대상 증상: FHD30 × 36타일(6x6) 단일 GPU 재생 시 `b0a7ce85`
(2026-07-21, `origin/multigpu-tiled-wall` tip) 대비 CPU/GPU 점유율 상승.

---

## 0. 대조군 3점 — 무엇을 왜 고르는가

`b0a7ce85`..`HEAD(4ce9526b801)` 는 **144 커밋**이다. 한 번에 비교하면 원인이 안 갈리므로
세 지점으로 쪼갠다.

| 지점 | 커밋 | 날짜 | 셸 | 설정 표면 | 이 구간에 들어온 것 |
|---|---|---|---|---|---|
| **A (기준)** | `b0a7ce85ed5` | 07-21 | **servoshell 뿐** | 환경변수 | — |
| **B (중간)** | `d4b82574a04` | 08-06 | winit_wall + servoshell | 환경변수 | 비표준 미디어 이식, 캡처카드, display 스키마 wall_layout, 가드밴드 present_inset, winit_wall 표출셸 (93커밋) |
| **C (현재)** | `4ce9526b801` | 08-20 | winit_wall + servoshell | **pref** | config surface 정리, iframe toplevel, devtools 수정, RTSP teardown (39커밋) |

### ★`b0a7ce85` 에는 `winit_wall` 이 없다★

```
b0a7ce85 : components/servo/examples/  →  winit_minimal.rs 뿐
HEAD     : components/servo/examples/  →  winit_minimal.rs + winit_wall/{main,tile,vsync_refresh_driver}.rs
```

- winit_wall 최초 커밋 `a8ed85998ec`(2026-06-25)은 **`winit_minimal.rs` 를 새로 552줄 작성한
  것**이지 이름 변경이 아니다. `git show -M -C --find-copies-harder` 로도 rename/copy 가
  안 잡히고, `winit_minimal.rs` 는 지금도 삭제되지 않고 나란히 있다. 커밋 메시지의
  "winit_minimal(193줄)을 베이스로" 는 참고했다는 뜻이다.
- 게다가 그 커밋은 `capture-card-getusermedia` / `video-perf-investigation` /
  `wall-spatial-display-autogpu` 브랜치에만 있고 **`multigpu-tiled-wall`(=b0a7ce85 계보)에는
  없다.** `video-perf-investigation` 도 b0a7ce85 를 포함하지 않는다(더 이른 지점에서 분기).
- b0a7ce85 시점의 `wall_layout` 은 아직 `ports/servoshell/wall_layout.rs` 의 `pub(crate)` 라
  예제가 쓸 수 없고, 레이아웃 JSON 도 **구스키마**(`monitor`+`gpu`+`rect`)다.

→ **A 지점의 대조군 셸은 servoshell 이 유일하다.** 마침 그 시점 6x6 실행 레시피가
`etc/multigpu/run_video_wall_d3d11.ps1` 로 저장소에 그대로 남아 있고, 페이지도 지금과 같은
`video_grid_6x6_play.html` 이다.

### B 지점을 `fbfb2c41ae9` 가 아니라 `d4b82574a04` 로 잡은 이유

둘 다 "winit_wall 이식 직후 + config surface 정리 이전" 이지만, `fbfb2c41ae9` 는
present_inset 주입이 깨진 상태이고 `d4b82574a04` 가 바로 그것을 고친 커밋이다
(`fix(winit_wall): remove broken present_inset injection, respect gpu_override`).
`d4b82574a04` 는 **환경변수 시대 마지막 winit_wall 커밋**이라 144커밋을 93/39 로 정확히 가른다.

---

## 1. ★배포본 — 테스트 머신에서 바로 실행★

측정은 이 개발기가 아니라 실제 테스트 머신에서 해야 하므로(§5.5 하드웨어 항목),
개발 환경 없이 돌아가는 자기완결 폴더로 묶어 두었다.

```
F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\dist\
  ctrl_A_b0a7ce85\   (0.44 GB, DLL 443)   engine\servoshell.exe   + run_ctrl_A.ps1
  ctrl_B_d4b82574\   (0.57 GB, DLL 444)   engine\winit_wall.exe   + run_ctrl_B.ps1
```

각 폴더 구성: `engine\`(exe + GStreamer 전량 + ANGLE + MSVC 런타임),
`config\`(그 지점 스키마의 레이아웃), `pages\`(6x6 페이지 + FHD30 소스), 런처 1개.

**폴더째 복사해서 런처를 실행하면 된다.** 런처가 pref→env 변환을 전부 담당한다.

```powershell
.\run_ctrl_A.ps1 -DurationSec 60     # b0a7ce85 servoshell
.\run_ctrl_B.ps1 -DurationSec 60     # d4b82574a04 winit_wall
```

`-Url` 로 그쪽의 실제 페이지를 대신 지정할 수 있다.

런처는 **순수 ASCII 로 작성돼 있다** — 한글이 들어 있으면 테스트 머신이 UTF-8 이 아니라
레거시 콘솔 코드페이지로 읽어 바이트가 깨지고 **닫는 큰따옴표를 못 찾는 파스 에러**가 난다
(실제로 발생). 검증: 파일 바이트를 CP949 로 디코딩해 파싱해도 에러 0.
**앞으로 이 스크립트들을 고칠 때 한글을 다시 넣지 말 것.**

### 런처가 끝에 찍는 마커 판정을 반드시 볼 것

```
markers: d3d11=36/36 direct_file=36/36 dcomp_engaged=1 wr_tile_override=1 panics=0
sync   : armed=36/36 released=36
```

### ★`sync` 줄 읽는 법 — 타일이 따로 노는 원인을 여기서 가른다★

b0a7ce85 의 sync group 은 **시작만** 맞춘다(`release_sync_group`): 공유 `SystemClock` +
동일 base time(현재+500ms) + 전원 동시 `play()`. 그리고 **`released` 는 프로세스 전역 1회성
래치**라, 한 번 풀린 뒤에 등록되는 파이프라인은 그냥 즉시 재생된다(비동기).

| `sync` 줄 | 뜻 | 조치 |
|---|---|---|
| `released=-1` | `SERVO_MEDIA_SYNC_GROUP` 이 안 먹었다(값이 2 미만이거나 미설정) | 플래그 전달 문제 |
| `released < armed` 또는 `released < 타일수` | **30초 워치독**이 부분 그룹을 놓아줬다. 늦게 arm 된 타일은 동기화 없이 재생된다 | ★알려진 arm 손실 이슈★(등록 지점은 gapless 워커 단계). 플래그 문제가 **아니다** |
| `released = 타일수` | 시작은 전부 동기됐다 | 그런데도 화면이 어긋나면 **시작이 아니라 그 뒤의 드리프트**다 → 아래 |

`released = 36` 인데도 어긋나 보이면 원인은 둘 중 하나다:

1. **루프 경계 위상차.** 소스가 ~30초 클립이라 그 뒤 wrap 이 돈다. gapless SEGMENT 되감기는
   파이프라인마다 자기 워커 스레드에서 일어나므로 wrap 을 지날수록 위상이 벌어질 수 있다
   (기존 관측: 타일 1개가 ~2초까지 지연). **기동 직후 30초 안에 봤는지, wrap 이후에 봤는지**를
   먼저 구분할 것.
2. **프레임 드롭 누적.** GPU/CPU 여유가 없으면 타일마다 다른 양을 흘려 위상이 벌어진다.

이미 받아 둔 로그가 있으면 다시 돌릴 필요 없이 바로 확인된다:

```powershell
Select-String -Path .\ctrl_A_*.err.log -Pattern "Sync group released" | Select-Object -Last 1
Select-String -Path .\ctrl_A_*.err.log -Pattern "pipelines armed" | Select-Object -Last 1
```

★참고: 페이지(`video_grid_6x6_play.html`)에는 의도적 stagger 가 **없다.** 시작 동기화를
전적으로 백엔드 sync group 에 맡긴다 — 즉 어긋남은 페이지 탓이 아니다.★

★A/B 지점은 **pref 로 주면 경고 한 줄 없이 조용히 무효**가 되는 구간이다★ — 마커로
"설정이 실제로 먹었는지"를 확인하지 않으면 그 측정은 무효다. 기대치와 다르면 런처가
`Write-Warning` 을 낸다.

### A/B 실험 파라미터 (§5 의심 항목을 바로 가름)

```powershell
.\run_ctrl_A.ps1 -DurationSec 60                     # 지금 구성 그대로
.\run_ctrl_A.ps1 -DurationSec 60 -TileSize ""        # WR 기본 1024x512 (30장) ← 1순위 후보
.\run_ctrl_A.ps1 -DurationSec 60 -DComp off          # DComp 끔
.\run_ctrl_A.ps1 -DurationSec 60 -Vsync -RefreshHz 120 -TileSize "" -DComp off
                                                     # ↑ 07-21 정본 레시피 그대로
```

같은 스위치가 `run_ctrl_B.ps1` 에도 있다. **C 지점(현재 빌드)에서 같은 A/B 를 하려면**
pref 쪽에서 `--pref gfx_wr_picture_tile_size=` (빈 값) / `--pref gfx_dcomp_mode=` (빈 값) /
`--pref gfx_vsync_enabled=true --pref gfx_refresh_hz=120` 로 바꾸면 된다.

### 검증 기록 (2026-08-21, clean PATH = System32 만)

시스템 GStreamer 가 PATH 에 있으면 배포본 누락이 안 드러난다(전례 있음). 그래서
`PATH=C:\Windows\System32;C:\Windows` + `GSTREAMER_1_0_ROOT_MSVC_X86_64` 해제 상태로 검증했다.

| 배포본 | 결과 |
|---|---|
| `ctrl_A_b0a7ce85` | `d3d11=36/36 direct_file=36/36 dcomp_engaged=1 wr_tile_override=1 panics=0` / `armed=36/36 released=36` |
| `ctrl_B_d4b82574` | `d3d11=36/36 direct_file=36/36 dcomp_engaged=1 wr_tile_override=1 panics=0` / `armed=36/36 released=36` |

즉 **이 개발기에서는 sync group 이 36/36 으로 완전히 동작한다.** 테스트 머신에서 어긋난다면
위 `sync` 줄 해석표로 arm 손실인지 드리프트인지부터 가를 것.

## 2. 빌드 산출물 경로 (재빌드용)

워크트리는 `git worktree` 로 만들었다(둘 다 detached HEAD, 원 워크트리 무영향).

| 지점 | 워크트리 | 빌드 산출물 |
|---|---|---|
| A | `F:\...\20260606_multigpu_browser\ctrl_base` (= `W:\ctrl_base`) | `target\release\servoshell.exe` |
| B | `F:\...\20260606_multigpu_browser\ctrl_ww` (= `W:\ctrl_ww`) | `target\release\examples\winit_wall.exe` |
| C | `servo_multigpu-tiled-wall` | `target\release\{servoshell.exe, examples\winit_wall.exe}` (08-20 빌드 존재) |

빌드는 **반드시 `W:` 경유** — `F:\...` 원경로(82자)면 mozangle 이 Os error 206 으로 죽는다.

```powershell
subst W: F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser   # 이미 매핑돼 있으면 생략
. W:\scripts\servo_env.ps1        # 앞의 점 필수 (상위 루트의 스크립트다)
$ErrorActionPreference = 'Continue'   # 없으면 cargo 첫 stderr 줄에서 빌드가 죽는다
cd W:\ctrl_base
.\mach build --release -j 8       # 미디어가 걸린 검증이므로 cargo -p 금지(더미 백엔드)
```

winit_wall(B)은 mach 가 안 만드니 mach 빌드 뒤에 따로:

```powershell
cd W:\ctrl_ww
.\mach build --release -j 8
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl --release
```

`-p servo` 와 `no-wgl` 은 둘 다 필수다(없으면 각각 "no example target" / WGL 백엔드 컴파일 에러).

---

## 3. ★플래그 대응표★ — 지금 명령을 대조군에 어떻게 옮기는가

지금 쓰는 명령(C 지점):

```powershell
.\engine\winit_wall.exe --wall-layout config\wall_layout.singlegpu.json --wall-all-tiles `
  --ignore-certificate-errors `
  --pref devtools_server_enabled=true --pref devtools_server_listen_address=0.0.0.0:7000 `
  --pref dom_image_extended_formats_enabled=true --pref dom_video_extended_containers_enabled=true `
  --pref dom_video_network_uri_enabled=true --pref dom_webrtc_enabled=true `
  --pref dom_screen_capture_enabled=true `
  --pref gfx_dcomp_mode=surface --pref gfx_vsync_enabled=false --pref gfx_refresh_hz=60 `
  --pref gfx_wr_picture_tile_size=display `
  --pref media_d3d11_enabled=true --pref media_direct_file_enabled=true `
  --pref media_gapless_loop_enabled=true --pref media_avdec_max_threads=1 `
  --pref media_sync_group_target=36 `
  "file:///C:/20260812_SDWall_WallView/html/video_grid_6x6_play.html?rows=6&cols=6"
```

| C 지점 (현재) | A 지점 `b0a7ce85` (servoshell) | B 지점 `d4b82574a04` (winit_wall) |
|---|---|---|
| `--wall-layout` `--wall-all-tiles` | 있음. **단 구스키마 JSON 필요**(§4b) | 있음. **신스키마 그대로 통함**(`display`, `monitor` 별칭 허용) |
| `--ignore-certificate-errors` | 있음 | ★**없음**★ (`191c0e051dd` 에서 추가) → 빼야 함 |
| `--pref devtools_server_enabled=true` | 동일 | 동일 (단 승인 델리게이트는 `011c7d98466` 이후라 연결 거동이 다를 수 있음) |
| `--pref devtools_server_listen_address=...` | 동일 | 동일 |
| `--pref dom_webrtc_enabled=true` | 동일 | 동일 |
| `--pref dom_image_extended_formats_enabled=true` | ★**pref 자체가 없음 → `Unknown preference` 즉시 패닉**★ | 있음 |
| `--pref dom_video_extended_containers_enabled=true` | ★**없음 → 패닉**★ | 있음 |
| `--pref dom_video_network_uri_enabled=true` | ★**없음 → 패닉**★ | 있음 |
| `--pref dom_screen_capture_enabled=true` | ★**없음 → 패닉**★ | 있음 |
| `--pref gfx_dcomp_mode=surface` | `$env:SERVO_COMPOSITOR_DCOMP="surface"` | 동일 |
| `--pref gfx_vsync_enabled=false` | **env 를 지운다**(`Remove-Item Env:\SERVO_WIN_VSYNC`). 미설정 = off | 동일 |
| `--pref gfx_refresh_hz=60` | `$env:SERVO_REFRESH_TIMER_HZ="60"` | 동일 |
| `--pref gfx_wr_picture_tile_size=display` | ★`display` **토큰이 없다**★ → `$env:SERVO_WR_PICTURE_TILE_SIZE="<타일창 WxH>"` 로 명시 | 동일(없음) |
| `--pref media_d3d11_enabled=true` | `$env:SERVO_MEDIA_D3D11_VIDEO="1"` | 동일 |
| `--pref media_direct_file_enabled=true` | `$env:SERVO_MEDIA_DIRECT_FILE="1"` | 동일 |
| `--pref media_gapless_loop_enabled=true` | `$env:SERVO_MEDIA_GAPLESS_LOOP="1"` | 동일 |
| `--pref media_avdec_max_threads=1` | `$env:SERVO_GSTREAMER_AVDEC_MAX_THREADS="1"` | 동일 |
| `--pref media_sync_group_target=36` | `$env:SERVO_MEDIA_SYNC_GROUP="36"` (그때도 **개수**였다. 이름만 바뀜) | 동일 |

**env 불리언 표기**: A/B 지점의 플래그는 전부 `env_flag_enabled()` 판정이라
`1` / `true` / `yes` / `on` 을 모두 받는다(대소문자 무시). 다만 **"미설정"이 off 이므로
끄려면 값을 `0` 으로 주는 게 아니라 `Remove-Item Env:\NAME` 으로 지워야 한다** —
`SERVO_VIDEO_DECOUPLE` 만 예외로 `"0"` 이 off 다(기본 on 킬스위치).

**`display` 를 무엇으로 치환하나**: HEAD 의 `=display` 는 "이 painter 의 창 크기"로 해석된다
(`painter.rs:resolve_wr_tile_size`). 실제 레이아웃이

```json
{ "virtualViewport": {"width": 5760, "height": 2160},
  "tiles": [ { "display": 0, "rect": [0, 0, 5760, 2160] } ],
  "overlapPx": 0 }
```

이므로 **타일은 1 개, 창 크기는 5760x2160** 이다. 따라서 A/B 에서는

```
SERVO_WR_PICTURE_TILE_SIZE = "5760x2160"
```

### ★그런데 이 값이 의미하는 바를 계산해 보면★

| 설정 | picture cache 타일 배치 | 타일 1장 크기(RGBA) |
|---|---|---|
| WR 기본 1024x512 (= 07-21 레시피) | `ceil(5760/1024) x ceil(2160/512)` = **6 x 5 = 30 장** | 1024·512·4 ≈ **2.0 MB** |
| `=display` (= 지금) | **1 장** | 5760·2160·4 ≈ **49.8 MB** |

즉 **화면 어디가 조금 바뀌든 50MB 짜리 표면 한 장이 통째로 다시 그려지고 present 된다.**
30장으로 쪼개져 있으면 바뀐 타일만 무효화된다. 36개 비디오가 각자 30fps 로 갱신되는 페이지에서
이 차이는 GPU 대역폭에 그대로 나타난다 — **GPU 점유율 상승의 1순위 후보다.**

다만 이 값은 이유가 있어서 들어간 것이다: `f2d52818af8` 이 "이동 잔상의 트리거는 **슬라이스당
픽처 타일 수 > 1**" 로 결론지었고, DComp 투명 구멍 회피책도 "TileSize 를 디스플레이 해상도와
일치" 였다. 그러니 **그냥 되돌리면 잔상/구멍이 돌아온다** — 대조군에서 A/B 를 정확히 재는 것이
먼저다. (A 지점에는 이 잔상 문제 자체가 없었는지도 같이 확인해야 한다.)

★부수 사실★: 타일이 1 개이므로 `--wall-all-tiles` 는 창 **하나**만 연다. 프레임 배리어·팬아웃
경로는 사실상 퇴화(paint target 1개) 상태이고, 이 실행은 "5760x2160 단일 창"에 가깝다.
즉 이번 회귀는 월 팬아웃 자체보다 **단일 거대 표면의 합성 비용** 문제일 공산이 크다.

---

## 4. 대조군 실행 스크립트 (수동)

### A: servoshell @ `b0a7ce85`

```powershell
# 이전 실행의 stale env 가 새면 A/B 가 무효가 된다 — 먼저 전부 지운다.
Get-ChildItem Env: | Where-Object Name -like 'SERVO_*' | ForEach-Object { Remove-Item "Env:\$($_.Name)" }

$env:SERVO_COMPOSITOR_DCOMP            = "surface"
$env:SERVO_REFRESH_TIMER_HZ            = "60"
$env:SERVO_WR_PICTURE_TILE_SIZE        = "5760x2160"   # = HEAD 의 `display` (타일 창 크기)
$env:SERVO_MEDIA_D3D11_VIDEO           = "1"
$env:SERVO_MEDIA_DIRECT_FILE           = "1"
$env:SERVO_MEDIA_GAPLESS_LOOP          = "1"
$env:SERVO_MEDIA_SYNC_GROUP            = "36"
$env:SERVO_GSTREAMER_AVDEC_MAX_THREADS = "1"
# SERVO_WIN_VSYNC 는 설정하지 않는다 (= gfx_vsync_enabled=false 와 동치)

# 번들 GStreamer 와 시스템 GStreamer 가 섞이면 ABI 불일치 — 07-21 레시피가 하던 대로 차단
$env:GST_PLUGIN_PATH            = ""
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = ""
$env:RUST_LOG = "warn,paint=info,servo_media_gstreamer=info,servo_media_gstreamer_render_d3d11=info"

.\target\release\servoshell.exe `
  --wall-layout wall_layout.singlegpu.monitorgpu.json --wall-all-tiles `
  --ignore-certificate-errors `
  --pref devtools_server_enabled=true `
  --pref devtools_server_listen_address=0.0.0.0:7000 `
  --pref dom_webrtc_enabled=true `
  "file:///.../video_grid_6x6_play.html?rows=6&cols=6" 2> ctrl_A.err.log
```

★확장 미디어 pref 4개(`dom_image_extended_formats_enabled`,
`dom_video_extended_containers_enabled`, `dom_video_network_uri_enabled`,
`dom_screen_capture_enabled`)는 **반드시 빼야 한다** — 넣으면 기동 즉시 패닉한다.★

### B: winit_wall @ `d4b82574a04`

A 와 env 는 완전히 동일하고, 실행부만 바뀐다.

```powershell
.\target\release\examples\winit_wall.exe `
  --wall-layout config\wall_layout.singlegpu.json --wall-all-tiles `
  --pref devtools_server_enabled=true `
  --pref devtools_server_listen_address=0.0.0.0:7000 `
  --pref dom_image_extended_formats_enabled=true `
  --pref dom_video_extended_containers_enabled=true `
  --pref dom_video_network_uri_enabled=true `
  --pref dom_webrtc_enabled=true `
  --pref dom_screen_capture_enabled=true `
  "file:///.../video_grid_6x6_play.html?rows=6&cols=6" 2> ctrl_B.err.log
```

★`--ignore-certificate-errors` 는 이 시점에 **없고, 넣으면 하드 에러다**★ — 인자 루프의
마지막 팔이 `other => return Err(format!("unknown argument: {other}"))` 라 파싱 단계에서
바로 죽는다. 반드시 뺀다.

---

## 4b. A 지점용 구스키마 레이아웃 JSON

`b0a7ce85` 파서는 타일마다 `monitor` + `gpu` + `rect` 를 요구하고 `display` 키를 모른다
(`"monitor must be a non-negative integer"` 로 거부). 현재 쓰는
`wall_layout.singlegpu.json` 을 아래 형태로 변환해 A 전용 파일로 따로 둔다
(**공유 config 는 건드리지 말 것** — 다른 브랜치가 신스키마를 쓴다).

**작성 완료** — `W:\ctrl_base\wall_layout.singlegpu.monitorgpu.json`:

```json
{
  "virtualViewport": { "width": 5760, "height": 2160 },
  "tiles": [
    { "monitor": 0, "gpu": 0, "rect": [0, 0, 5760, 2160] }
  ],
  "overlapPx": 0
}
```

`"display": 0` → `"monitor": 0, "gpu": 0` 으로 바꾼 것뿐이고 나머지는 원본과 동일하다.
b0a7ce85 파서 기준 검증 통과(`monitor`/`gpu`/`rect` 존재, rect 가 virtualViewport 내부).

B 지점(`W:\ctrl_ww\wall_layout.singlegpu.json`)은 **원본 그대로** 쓴다 — 그 시점 파서는 이미
`display` 스키마다(`monitor` 는 레거시 별칭으로만 허용).

---

## 5. ★빌드 전에 이미 보이는 의심 지점 7건★

`b0a7ce85` 당시의 정본 6x6 레시피(`etc/multigpu/run_video_wall_d3d11.ps1`)와 지금 명령의 차이다.
**대조군을 돌리기 전에 이 중 몇 개는 지금 빌드에서 그냥 꺼 보는 것만으로 판별될 수 있다.**

| # | 항목 | 07-21 레시피 | 지금 | 비고 |
|---|---|---|---|---|
| 1 | **DComp** | **기본 off** (`-DComp` 옵트인) | `gfx_dcomp_mode=surface` | 당시 6x6 기준선은 DComp 를 **켜지 않은** 상태였다. `surface` 는 가상 서피스 lend/return 경로다 |
| 2 | **WR picture tile** | 미설정 = WR 기본 1024x512 | `=display`(창 크기 1장) | 슬라이스당 타일 1장. 무효화 입도가 완전히 달라진다 |
| 3 | **vsync** | `SERVO_WIN_VSYNC=1` | `gfx_vsync_enabled=false` | **정반대**. 당시엔 DwmFlush 페이싱이 켜져 있었다 |
| 4 | **refresh** | 미설정 = **120Hz** | `gfx_refresh_hz=60` | 프레임 생산 주기 절반 |
| 5 | **devtools 서버** | 없음 | `0.0.0.0:7000` 리슨 | 상시 CPU 비용 + 파이프라인 질의. 07-21 측정엔 없던 부하다 |
| 6 | **창 구성** | 단일 창 `--window-size`, **wall 미사용** | `--wall-layout` + `--wall-all-tiles` | 프레임 배리어·팬아웃 경로가 통째로 추가됨 |
| 7 | **GStreamer 격리** | `GST_PLUGIN_PATH=""` 로 시스템 플러그인 차단 | 미설정 | 시스템 GStreamer 가 PATH 에 있으면 다른 플러그인이 물릴 수 있다 |

### ★8. GStreamer 런타임 자체가 바뀌었다 (별도 축)★

이건 pref/플래그가 아니라 **런타임 교체**라 위 표와 별개로 봐야 한다.

- `b0a7ce85` 당시: 번들 **1.22.8** (`gstreamer.py` 가 `avformat-59`/`avfilter-8`/
  `libcrypto-1_1` 같은 1.22 시대 DLL 이름을 하드코딩)
- 그 뒤 `7b97e09d23c`..`7b7c0a215d2` (#22~27) 에서 **1.28.4.100 으로 마이그레이션** +
  카메라 소스 플러그인 등록 추가. 지금 이 개발기의
  `GSTREAMER_1_0_ROOT_MSVC_X86_64` 는 `F:\gstreamer-inhouse\1.28.4.100\...` 을 가리킨다.

즉 A↔C 를 그냥 비교하면 **Servo 코드 변화 + GStreamer 메이저 업데이트가 한꺼번에** 섞인다.
디코더 기본값·스레딩·d3d11 플러그인 거동이 버전 간에 바뀌므로 36타일 SW 디코드 CPU 에
직접 영향을 줄 수 있는 축이다.

**이번 대조군은 GStreamer 를 현재 사내 1.28.4.100 으로 고정해 빌드한다** — 그래야 A↔C 차이가
"Servo 코드 변화"로 좁혀진다. 단 `b0a7ce85` 의 `gstreamer.py` 는 1.22 시대 DLL 이름을 요구하므로
**mach 의 DLL 복사/패키지 단계가 exit 1 로 실패한다**(컴파일·링크는 성공). 그 뒤 플러그인과
의존 DLL 을 사내 루트에서 `target\release` 로 직접 복사해 채운다.

→ **A ≈ C 로 나오면 그때 A 를 1.22.8 로 한 번 더 돌려 GStreamer 축을 검증한다.**

보조: 07-21 레시피는 `SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF=1` 도 켰다. HEAD 에서도
`debug_env` 로 살아 있으나 지금 명령엔 없다. direct-file 이 켜져 있으면 불활성이라 영향은
없을 가능성이 높지만, 완전 동치를 원하면 A/B/C 모두에 넣거나 모두에서 빼라.

### 설정 표면 변화 요약

`b0a7ce85` → HEAD 사이에 **pref 33개가 새로 생겼고 삭제된 pref 는 없다.** 이 중
지금 명령이 건드리지 않는데 **기본값이 on** 이라 조용히 동작하는 것들:

| pref | 기본 | 성격 |
|---|---|---|
| `gfx_wall_frame_pacing_enabled` | **true** | 이름과 달리 기본 on (옛 판정이 `mode == Latest` 였다) |
| `gfx_video_escape_stable_swapchain` | **true** | 킬스위치 |
| `gfx_video_decouple_enabled` | **true** | 킬스위치 (A 지점도 기본 on 이라 동치) |
| `media_local_direct_file` | **true** | ★신설★ `92a23efbb42`. 로컬 `file://` `<video>` 를 기본 direct-file 로 돌리고 **Servo fetch 자체를 개시하지 않는다**. A 지점엔 이 개념이 없다 |
| `media_audio_enabled` | **true** | 옛 `SERVO_GSTREAMER_DISABLE_AUDIO` 의 의미를 뒤집은 것 |
| `dom_enforce_framing_policy` / `network_enforce_mixed_content` | **true** | 표준 동작 |
| `media_screen_capture_show_cursor` | true | 캡처 미사용 시 무관 |

`gfx_video_escape_mode` 는 기본 `""`(off) 라 A/C 모두 꺼져 있다 — b0a7ce85 커밋 메시지의
`present_external_only` 경로는 **DComp + video escape 를 둘 다 켜야** 발동하므로, 지금 명령
(`escape` 미지정)에서는 그 fast-path 가 애초에 안 돈다.

---

## 5.5 빌드·기동 실측 (2026-08-21)

### A: `b0a7ce85` servoshell — **PASS**

- `mach build --release -j 8` → `Finished release profile in 7m 53s`,
  `W:\ctrl_base\target\release\servoshell.exe` **151,085,568 bytes**
- ★예상대로★ 그 뒤 DLL 복사 단계가 exit 1: `could not find required GStreamer DLL:
  avcodec-59 / avfilter-8 / avformat-59 / avutil-57 / libcrypto-1_1-x64 / libjpeg-8 /
  libogg-0 / libpng16-16 / libssl-1_1-x64 / libvorbis-0 / libvorbisenc-2 / swresample-4`
  (12건, 전부 1.22 시대 이름). **컴파일·링크는 이미 끝난 뒤라 무해하다.**
- 복구: 사내 `F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64` 의
  `bin\*.dll` + `lib\gstreamer-1.0\*.dll` 을 `target\release` 로 직접 복사(총 443 DLL).
  **b0a7ce85 가 `gstreamer_plugin_lists/{common,windows}.rs.in` 에서 요구하는 플러그인 38개가
  1.28.4.100 에 전부 존재 → 누락 0.**
- 35초 스모크(실제 플래그 전량 + 위 레이아웃 + `video_grid_6x6_play.html?rows=6&cols=6`):

| 마커 | 결과 |
|---|---|
| `profile_id=` (D3D11 파이프라인 활성) | **36 / 36** |
| `direct file playback` | **36 / 36** |
| `[dcomp-native] engaged` | ✅ + `window present skipped (content composited via DComp)` |
| `[wr-tile-size] picture tile size override` | ✅ **5760x2160** |
| `panicked` / `ErrorLoadingPlugins` | **0 / 0** |
| 기타 경고 | `SetTrackFailed` ×72(기존부터 있던 것), WR `gpu_cache_update` 셰이더 1건 |

즉 **A 대조군은 지금 명령과 동일한 구성으로 완전히 동작한다.**

### ★이 개발기에서는 성능 수치를 낼 수 없다★

```
GPU        : AMD Radeon HD 7800M Series (4GB, 27.20.20913.2000)   ← A4000 아님
디스플레이 : 1920x1080 x 2 (총 3840x1080)                          ← 5760x2160 안 들어감
```

CLAUDE.md 의 "RTX A4000 x2 / 3 모니터" 서술과 현재 하드웨어가 다르다. 5760x2160 타일은
여기서 잘리고 GPU 세대도 완전히 다르므로, **CPU/GPU 점유율 비교는 반드시 실제 테스트
머신에서** 해야 한다. 이 개발기에서 유효한 것은 "기동·마커 검증"까지다.

참고 수치(이 AMD 기준, 비교용 아님): `render_ms` n=200 → min 0.80 / p50 6.69 /
p90 21.32 / max 33.95 ms.

## 6. 함정

- **pref 이름은 밑줄 표기.** `gfx.dcomp.mode` 같은 점 표기는 조용히 무시되지 않고
  `Unknown preference` 로 즉시 패닉한다.
- **HEAD 는 옛 환경변수가 설정돼 있으면 기동을 막는다**(`removed_env`, 20개). A/B 실행 후
  같은 셸에서 C 를 돌리면 차단 메시지와 함께 죽는다. **셸을 새로 열거나 §3 첫 줄의
  `SERVO_*` 일괄 삭제를 먼저 하라.**
- **A/B 는 반대 방향의 함정**: pref 로만 주면 그 시점 엔진은 env 를 읽으므로 **스위치가
  조용히 무효**가 된다(에러도 경고도 없다). 로그에서 실제 활성 마커를 확인할 것 —
  `profile_id=`(D3D11 파이프라인 수), `direct file playback`(타일 수만큼),
  `[dcomp-native] engaged`.
- 로그는 `RUST_LOG` 없으면 0바이트다. `warn,paint=info` 이상 필요.
- 월 타일 창은 `WM_CLOSE`(`taskkill /IM`, `CloseMainWindow`)에 반응하지 않는다 → `/F`.
  env_logger 는 비버퍼 stderr 라 강제종료해도 기록은 남는다.

---

## 부록: 참조 커밋

| 커밋 | 내용 |
|---|---|
| `a8ed85998ec` (06-25) | winit_wall 신규 552줄 (winit_minimal 이름변경 **아님**), multigpu-tiled-wall 계보 밖 |
| `b0a7ce85ed5` (07-21) | **A 지점.** external 비디오 갱신 분리(`present_external_only`) 반영 |
| `92a23efbb42` | `media_local_direct_file` 신설(기본 on) — 로컬 file:// fetch 스킵 |
| `c8404d48f6d` | `wall_layout` 을 `servo-paint-api` 로 승격 |
| `fbfb2c41ae9` (08-06) | winit_wall 을 이 계보로 이식 (present_inset 주입 깨짐) |
| `d4b82574a04` | **B 지점.** 위 present_inset 제거 + gpu_override 존중 |
| `4af6dfb0ac6` / `be1cbd78db9` / `2030cf68b7a` | env → pref 이관 (gfx 6 / video 4 / media 9) |
| `164f6a982cf` | 옛 env 20개 설정 시 **기동 차단** |
| `12f931c7599` | picture tile size 를 pref 로 + `display` 토큰 신설 |
| `f2d52818af8` | "이동 잔상의 트리거는 슬라이스당 픽처 타일 수 > 1" ← `=display` 를 쓰게 된 근거 |

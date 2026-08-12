# 설정 매뉴얼 — pref 와 조사용 환경변수 사용법

이 문서는 **어떻게 쓰는가**를 다룬다. 이름·타입·기본값의 **정본 표는
[`configuration.md`](configuration.md)** 이고, 그 문서와 코드의 일치는 테스트가 강제한다
(`components/config/tests/config_surface.rs`). 값이 궁금하면 그쪽, 무엇을 언제 만질지가
궁금하면 이쪽이다.

대상은 이 포크로 비디오 월을 띄우는 사람이다. Windows + servoshell 기준으로 쓴다.

---

## 1. 값을 넘기는 세 가지 경로

우선순위는 **아래로 갈수록 이긴다**.

| 순서 | 경로 | 쓰는 상황 |
|---|---|---|
| 1 | 코드 기본값 (`Preferences::const_default()`) | 아무것도 안 했을 때 |
| 2 | 사용자 prefs 파일 `%APPDATA%\Servo\prefs.json` | 이 머신에서 늘 켜 둘 것 |
| 3 | `--prefs-file <path.json>` (여러 번 가능, 준 순서대로) | 프로필을 파일로 관리할 때 |
| 4 | `--pref <이름>=<값>` (여러 번 가능) | 이번 실행에만 |

### `--pref` 문법

```powershell
target\debug\servoshell.exe --pref media_d3d11_enabled=true --pref media_avdec_max_threads=1 <URL>
```

- **이름은 밑줄 표기다.** `gfx_dcomp_mode` 가 맞고 `gfx.dcomp.mode` 는 틀리다.
- **값을 생략하면 `true`** 다. `--pref media_d3d11_enabled` = `--pref media_d3d11_enabled=true`.
- 값 해석: `true`/`false` → bool, 정수 → i64, 소수 → f64, 나머지는 문자열.
- **모르는 이름은 조용히 무시되지 않는다.** `Unknown preference: "..."` 로 **즉시 죽는다.**
  오타가 넘어가지 않는다는 뜻이라 좋은 성질이지만, 스크립트가 틀린 이름을 넘기면 실행 자체가
  안 된다.

`prefs.json` 도 같은 규칙이다 — 키가 밑줄 표기여야 하고, 모르는 키가 하나라도 있으면 기동이
죽는다.

### PowerShell 스크립트에서

★**`$env:SERVO_*` 로 노브를 켜지 마라.**★ 셸이 pref 세트를 무조건 읽으므로 그 env 는
무효다. 배열에 모아 `Start-Process -ArgumentList` 로 스플라이스하는 것이 이 저장소의 관례다.

```powershell
$prefArgs = @(
    "--pref", "gfx_vsync_enabled=true",
    "--pref", "media_d3d11_enabled=true"
)
Start-Process -FilePath $servoExe -ArgumentList (@("--window-size",$WindowSize) + $prefArgs + @($url))
```

---

## 2. 지금 무슨 설정으로 도는지 확인하기

기동 시 **기본값과 다른 것만** stderr 로 찍는다. 조용한 것이 정상이다.

```
servo: config: media_d3d11_enabled=true (default false)
servo: config: media_sync_group_target=45 (default 0)
servo: config: debug env: SERVO_LOG_PRESENT_CADENCE
```

GUI 실행은 화면에 아무것도 안 보이므로 **stderr 를 파일로 돌려야 읽을 수 있다**:

```powershell
target\debug\servoshell.exe --wall-layout <layout.json> --wall-all-tiles <URL> 2> run.err.log
```

★**창을 닫아서 종료해야 stderr 가 flush 된다.**★ 강제 종료하면 버퍼가 날아가 로그가 빈다 —
빈 `.err.log` 는 "아무 일도 없었다" 가 아니라 대개 "죽여서 잃었다" 는 뜻이다.

---

## 3. 옛 환경변수를 쓰고 있었다면

옛 이름 20 개는 **더 이상 읽지 않고, 설정돼 있으면 기동을 막는다.**

```
servo: error: 2 environment variable(s) moved to prefs are still set.
servo: error:   SERVO_COMPOSITOR_DCOMP is no longer read; use --pref gfx_dcomp_mode=on (hybrid) or --pref gfx_dcomp_mode=surface; the value is a three-state mode, not a boolean
servo: error:   SERVO_GSTREAMER_DISABLE_AUDIO is no longer read; the replacement is inverted: use --pref media_audio_enabled=false to get what SERVO_GSTREAMER_DISABLE_AUDIO=1 used to do
servo: error: see docs/multigpu/configuration.md
```

메시지가 바꿔 쓸 명령을 그대로 알려주므로 따라 하면 된다. 설정된 것을 **전부** 보고하므로
하나씩 고치며 재기동할 필요는 없다.

**그냥 무시하지 않고 막는 이유**: 이 저장소는 이미 그것 때문에 당했다. 운용 스크립트가
`$env:SERVO_COMPOSITOR_DCOMP` 로 DComp 를 켜고 있었는데 셸이 pref 를 읽게 되면서 그 env 가
죽었고, **엔진은 아무 경고도 찍지 않았으며 화면은 그냥 돌았다.** 무엇이 꺼졌는지 알 방법이
없었다.

전체 변환표는 [`configuration.md` 의 마이그레이션 표](configuration.md)에 있다. 값 문법이
그대로가 아닌 것만 여기 다시 적는다:

| 옛 env | 새 pref | 주의 |
|---|---|---|
| `SERVO_GSTREAMER_DISABLE_AUDIO=1` | `media_audio_enabled=false` | **의미가 뒤집혔다** |
| `SERVO_MEDIA_SYNC_GROUP=45` | `media_sync_group_target=45` | 불리언이 아니라 **개수** |
| `SERVO_COMPOSITOR_DCOMP=1` | `gfx_dcomp_mode=on` | **3 상태 모드** (`on`/`surface`) |
| `SERVO_VIDEO_ESCAPE=external` | `gfx_video_escape_mode=external` | 모드 토큰 |
| `SERVO_VIDEO_DECOUPLE=0` | `gfx_video_decouple_enabled=false` | 기본 on 킬스위치 |
| `SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN=0` | `gfx_video_escape_stable_swapchain=false` | 기본 on 킬스위치 |
| `SERVO_GSTREAMER_AVDEC_MAX_THREADS` 미설정 | `media_avdec_max_threads=-1` | 미설정은 `0` 이 아니라 **`-1`** |
| `SERVO_WR_PICTURE_TILE_SIZE=1920x1080` | `gfx_wr_picture_tile_size=1920x1080` | 문법 동일. 추가로 **`=display`** 토큰이 생겼다 |

---

## 4. 레시피

### 4.1 비디오 월 최고 성능 (정본 레시피)

`etc/multigpu/package_run_wall.ps1` 이 실제로 넘기는 조합이다.

```powershell
--pref gfx_vsync_enabled=true
--pref media_d3d11_enabled=true
--pref media_direct_file_enabled=true
--pref media_gapless_loop_enabled=true
--pref media_sync_group_target=<총 비디오 수>
--pref media_avdec_max_threads=1
--pref gfx_dcomp_mode=on          # 선택: WR 네이티브 컴포지터
```

각각이 하는 일:

- `media_d3d11_enabled` — 파이프라인마다 GPU 에서 업로드/변환. 렌더러는 GPU 에 이미 올라간
  공유 텍스처만 바인딩한다(제로카피). 이게 꺼지면 CPU I420 경로로 떨어진다.
- `media_direct_file_enabled` — `file://` 미디어를 GStreamer 가 직접 읽는다. 루프 되감기마다
  스크립트 스레드를 왕복하던 것이 사라진다.
- `media_gapless_loop_enabled` — EOS/flush 없이 SEGMENT 되감기. 루프 경계에서 프레임이
  멈춰 보이던 것이 사라진다.
- `media_sync_group_target` — 아래 4.2.
- `media_avdec_max_threads=1` — 아래 4.3.

### 4.2 타일을 동시에 출발시키기

```powershell
--pref media_sync_group_target=<N>
```

`N` 은 **함께 출발하기를 기다릴 파이프라인 수**다. 불리언이 아니다. `2` 미만이면 비활성.
모든 파이프라인이 공유 시스템 클록에 같은 base time 으로 붙어 프레임 단위로 맞춰 출발한다.
30 초 워치독이 있어 `N` 에 못 미쳐도 결국 놓아준다.

★**함정: `N` 은 격자 칸 수가 아니라 페이지의 총 비디오 수다.**★ 4×3 격자 + PiP 하나면
`12` 가 아니라 `13` 이다. 하나가 모자라면 초과분이 동기 그룹에서 밀려 혼자 시작하고, 그
타일만 어긋나 보인다.

### 4.3 디코더 스레드 정하기

```powershell
--pref media_avdec_max_threads=<N>     # -1 = 자동(미설정), 0 이상 = 캡
```

소프트웨어 `avdec_*` 디코더는 그냥 두면 CPU 코어 수만큼 워커를 띄우고 디코드 프레임 풀도
비례해 커진다. 타일이 많으면 스레드와 메모리가 폭발한다.

| 상황 | 값 | 근거 |
|---|---|---|
| 다중 타일(기본) | `1` | 운용 스크립트 기본값 |
| 45 타일 이상 | `2`~`3` | 스크립트 주석의 권고 |
| 4K 단일 비디오 | `6` | 멀티스레드 디코드가 필요하다 |
| 손대고 싶지 않다 | `-1` | 자동 — 옛 env 미설정과 같다 |

★`0` 은 "자동" 이 아니라 **1 스레드 강제**다. 미설정 보초값은 `-1`.★

### 4.4 DirectComposition 네이티브 컴포지터 A/B

```powershell
--pref gfx_dcomp_mode=on        # 하이브리드(전면 갱신 서피스를 스왑체인으로 승격)
--pref gfx_dcomp_mode=surface   # 가상 서피스 전용
                                # 생략 = off (기존 Draw 경로)
```

3 상태다. `on` 과 `surface` 는 배타적이라 bool 두 개로 쪼개지 않았다. 모르는 값을 주면
경고 한 줄 찍고 `off` 로 처리한다.

★**`surface` 는 헤드리스(`-f`)에서 성립하지 않는다** — GL error 500 으로 죽는다. 기존
동작이고 운용 스크립트는 전부 헤디드라 영향이 없지만, 헤드리스로 A/B 하려다 이걸 버그로
오인하기 쉽다.★

### 4.5 비디오를 WebRender 밖으로 빼기

```powershell
--pref gfx_dcomp_mode=on
--pref gfx_video_escape_mode=external
```

유효 토큰은 `external` 하나뿐이고 빈 문자열이 off 다. **DComp 게이트가 켜져 있어야 실제로
동작한다** — 이것만 켜면 아무 일도 안 일어난다.

문제가 생기면 두 킬스위치가 있다(둘 다 **기본 on**이라 끄는 방향으로 쓴다):

```powershell
--pref gfx_video_escape_stable_swapchain=false
--pref gfx_video_decouple_enabled=false
```

### 4.6 오디오 끄기

```powershell
--pref media_audio_enabled=false
```

**긍정형 pref 다.** 옛 `SERVO_GSTREAMER_DISABLE_AUDIO=1` 을 그대로 옮겨 적으면 반대로
동작한다. 기본은 `true`(오디오 켜짐).

### 4.7 저지연 대 부드러움

```powershell
--pref media_video_sink_policy=low-latency   # 버퍼 1, 늦은 프레임 드롭
--pref media_video_sink_policy=smooth        # 버퍼 3 (기본)
```

WebRTC 를 쓰면 지터버퍼도 함께 본다. 기본 `0`(무버퍼, 최저 지연)이고, 네트워크 지터로
프레임이 끊기면 올린다:

```powershell
--pref media_webrtc_jitter_latency_ms=50
```

### 4.8 picture cache 타일 크기를 디스플레이에 맞추기

```powershell
--pref gfx_wr_picture_tile_size=display      # 타일마다 자기 창 크기
--pref gfx_wr_picture_tile_size=1920x1080    # 전부 같은 크기
                                             # 생략 = WR 기본(콘텐츠 1024x512)
```

`display` 는 **painter 마다 자기 창의 실제 크기**로 정한다. 타일 해상도가 섞인 월에서도 각
타일이 자기 해상도에 맞고, 레이아웃을 바꿔도 값을 다시 적을 필요가 없다.

타일 크기가 창 이상이면 **슬라이스당 타일이 1 장**이 된다. 타일 수·무효화 입도·타일당 DComp
bind/unbind 오버헤드가 함께 달라지므로 A/B 축이자, DComp 투명 구멍의 현행 회피책이기도 하다.

★**WebRender 는 이 값을 검사도 클램프도 하지 않는다.**★ (2026-08-12 확인 —
`render_backend.rs` 가 `frame_config.tile_size_override` 에 그대로 넣고 `picture.rs` 가 그대로
`desired_tile_size` 로 쓴다.) 실질 상한은 GPU 텍스처 크기이고, 넘으면 타일 텍스처 할당이
실패한다. 8K 급 값을 넣을 생각이면 이 점을 먼저 확인하라.

런처는 `-TileSize` 스위치로 받는다(`-TileSize display` 도 그대로 통한다).

**실제로 몇으로 정해졌는지 확인하려면** `RUST_LOG` 를 걸어야 한다 — 기동 덤프(§2)는 pref 로
넘긴 **원문**(`display`)만 보여 주고, 해석된 픽셀 크기는 `paint` 크레이트의 info 로그다.

```powershell
$env:RUST_LOG = "warn,paint=info"
servoshell.exe --pref gfx_wr_picture_tile_size=display <URL> 2> tile.err.log
# [wr-tile-size] picture tile size override: 1600x900
```

형식이 틀리면 같은 태그로 경고가 뜨고 오버라이드 없이 진행한다(WR 기본을 그대로 쓴다).

---

## 5. 노브를 언제 만지나 — 그룹별

### `gfx_*` — 컴포지터와 표출

| pref | 언제 만지나 |
|---|---|
| `gfx_dcomp_mode` | 네이티브 컴포지터 A/B (4.4) |
| `gfx_vsync_enabled` | DWM vsync 페이싱이 필요할 때. **기본 off 인 이유는 `DwmFlush` 가 코어 하나를 상시 소모하기 때문**이다 |
| `gfx_refresh_hz` | 리프레시 드라이버 목표 주파수. `[1,1000]` 밖이면 경고 후 기본 120 |
| `gfx_wall_frame_pacing_enabled` | **기본 on.** 페이싱을 빼고 재보고 싶을 때만 `false` |
| `gfx_wall_frame_max_pending` | 배리어가 허용하는 미완료 프레임 수. 기본 1 |
| `gfx_wall_frame_min_interval_ms` | 프레임 사이 최소 간격. 기본 16 |
| `gfx_wr_picture_tile_size` | WR picture cache 타일 크기. 아래 4.8 |

### `media_*` — 미디어

성능 관련은 4.1~4.3 에 다 있다. 나머지:

| pref | 언제 만지나 |
|---|---|
| `media_video_decoder_policy` | `auto` 로 하드웨어 디코더 선택을 허용하거나, `software` 로 `avdec_*` 강제. 빈 문자열 = software |
| `media_video_sink_policy` | 4.7 |
| `media_audio_enabled` | 4.6 |
| `media_webrtc_jitter_latency_ms` | 4.7 |

### wall CLI 플래그 (pref 가 아니다)

```powershell
--wall-layout <path.json>     # 월 모드 켜기 + 레이아웃
--wall-tile-index <n>         # 이 창이 그릴 타일 하나 (미리보기용)
--wall-all-tiles              # 타일마다 창 하나씩 (실제 월 실행)
```

**아래 조합은 기동이 막힌다** — 요청한 것이 일어나지 않는 상태라서다:

- `--wall-tile-index` 를 `--wall-layout` 없이
- `--wall-all-tiles` 를 `--wall-layout` 없이
- `--wall-tile-index` 와 `--wall-all-tiles` 를 함께 (후자가 타일마다 창을 만드니 단일
  인덱스가 쓰일 자리가 없다)

servoshell 은 `--wall-layout` 이 없으면 평범한 브라우저 창으로 뜬다. winit_wall 은 표출 전용
셸이라 **필수**다.

---

## 6. 조사용 환경변수 (15 개)

pref 가 아니다. **실패 주입 · 프로파일링 · 이분탐색용**이고 조사가 끝나면 지운다. 그래서
CLI 가 아니라 환경변수로 남겼다 — 운용 설정과 섞이지 않게 하려는 것이다.

```powershell
$env:SERVO_LOG_PRESENT_CADENCE = "1"
target\debug\servoshell.exe ... 2> diag.err.log
```

기동 덤프가 `servo: config: debug env: ...` 로 **설정된 것만** 찍어 주므로, 로그만 보고
그때 무엇을 켜고 돌렸는지 알 수 있다.

★**`Presence` 종류는 값을 보지 않는다.** `SERVO_DCOMP_DEBUG=0` 도 **켜진다.** 끄려면 변수
자체를 지워야 한다.★ 어느 노브가 `Presence` 인지는
[`configuration.md`](configuration.md) 의 조사용 표에 종류가 적혀 있다.

자주 쓰는 것:

| 노브 | 무엇을 보나 |
|---|---|
| `SERVO_LOG_PRESENT_CADENCE` | 실제 엔진 present 빈도와 프레임 간 최악 간격. 페이지 rAF 카운트나 외부 캡처 도구와 무관한 그라운드트루스 |
| `SERVO_D3D11_PROFILE` (+ `_MS` 임계값) | D3D11 비디오 렌더 경로 단계별 타이밍 |
| `SERVO_VIDEO_ESCAPE_PROF` | external 탈출 파이프라인에서 acquire/convert/present 중 어디가 예산을 먹나 |
| `SERVO_DCOMP_DEBUG` | DComp bind/add_surface 좌표. **타일당 프레임당** 찍히니 오래 켜두지 마라 |
| `SERVO_DCOMP_READBACK` | 서피스를 CPU 로 읽어 실제로 그려졌는지 확인. `glReadPixels` 로 파이프라인을 멈추니 **느리다** |
| `SERVO_WALL_FRAME_DELAY_*` 3종 | 배리어 실패 주입 — 느린 렌더러 시뮬레이션 |

---

## 7. 증상에서 노브 찾기

| 증상 | 먼저 볼 것 |
|---|---|
| 타일 하나만 어긋나게 출발한다 | `media_sync_group_target` 이 **총 비디오 수**인가 (4.2) |
| 루프 경계마다 화면이 붙잡힌다 | `media_gapless_loop_enabled`, `media_direct_file_enabled` |
| 타일이 많아지니 CPU/메모리가 폭발한다 | `media_avdec_max_threads` (4.3) |
| 비디오가 CPU 를 먹는다 / 업로드가 무겁다 | `media_d3d11_enabled` 가 켜졌나 (기동 덤프로 확인) |
| 코어 하나가 계속 100% | `gfx_vsync_enabled` — `DwmFlush` 가 스핀한다 |
| 켰다고 믿는 노브가 안 먹는다 | 기동 덤프에 그 줄이 있나. 없으면 안 넘어간 것이다 |
| 스크립트가 기동조차 안 된다 | `Unknown preference` 인가 (이름 오타/점 표기), 아니면 제거된 env 차단인가 |
| 소리가 안 난다 | `media_audio_enabled` 를 `false` 로 준 건 아닌가 (의미 반전) |
| `.err.log` 가 비어 있다 | 창을 닫아서 종료했나. 강제 종료하면 버퍼가 날아간다 |

---

## 8. 하지 말 것

**조용히 실패하는 것들** — 아무 경고도 없이 의도와 다르게 돈다:

- `$env:SERVO_*` 로 **이 정리 대상이 아닌** 노브를 켜고 pref 와 섞어 쓰기. 옛 19 개는 이제
  차단되지만, 새 노브를 추가할 때 같은 함정이 다시 생긴다.
- `media_sync_group_target` 에 격자 칸 수만 넣기 (PiP 등 격자 밖 비디오 누락).
- `gfx_video_escape_mode=external` 만 켜고 `gfx_dcomp_mode` 를 빼먹기.
- 기본이 `true` 인 노브를 "설정 안 했으니 off" 로 가정하기 —
  `gfx_wall_frame_pacing_enabled`, `gfx_video_escape_stable_swapchain`,
  `gfx_video_decouple_enabled`, `media_audio_enabled` 넷이다.
- `SERVO_DCOMP_DEBUG=0` 으로 껐다고 믿기 (`Presence` 는 값을 안 본다).

**즉시 실패하는 것들** — 시끄럽게 죽으니 오히려 안전하다:

- 점 표기 pref 이름 (`gfx.dcomp.mode`) → `Unknown preference` 패닉.
- pref 이름 오타 → 같은 패닉. `prefs.json` 의 키도 마찬가지다.
- 제거된 옛 환경변수가 설정된 채로 기동 → 안내 후 종료.
- `--wall-tile-index` 를 `--wall-layout` 없이, 또는 `--wall-all-tiles` 와 함께.

**그 밖에**

- winit_wall 로 `overlapPx > 0` 레이아웃을 쓰지 마라. 가드 밴드는 확장 서피스 + 렌더 원점
  이동 + 표출 크롭이 한 세트인데 winit_wall 은 창 서피스에 직접 그린다. servoshell 을 쓰거나
  `overlapPx: 0` 레이아웃을 쓴다.
- 성능을 debug 빌드로 판단하지 마라. `--release` 로 재확인한다.

---

## 관련 문서

- [`configuration.md`](configuration.md) — 이름·타입·기본값 **정본 표** (테스트가 강제)
- [`multigpu_config_surface_consolidation_design.md`](multigpu_config_surface_consolidation_design.md) — 왜 이렇게 나눴는지
- `components/config/prefs.rs` — pref 정의와 `const_default()`
- `components/config/debug_env.rs` — 조사용 env 단일 등록처
- `components/config/removed_env.rs` — 차단 목록과 안내 문구
- `etc/multigpu/*.ps1` — 실제 운용 레시피

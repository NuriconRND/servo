# 설정 표면 정리 설계 (pref / 환경변수 / CLI)

작성 2026-08-11. 대상 브랜치 `nonstandard-media-display-port`.

## 1. 배경

이 포크의 실행 설정이 세 갈래로 흩어져 있고, 어느 갈래에 무엇이 있는지 코드를 읽어야만 안다.

| 갈래 | 개수 | 비고 |
|---|---|---|
| 환경변수 `SERVO_*` | 39 | `paint`, `media`, `script`, `layout`, `third_party/surfman` 에 산재 |
| wall CLI 플래그 | 4 | `--wall-layout`, `--wall-tile-index`, `--wall-all-tiles`, `--wall-gpu-direct-present` |
| pref | (servo 기본 + 포크 추가분) | `--pref name=value` |

### 실제로 확인된 문제 세 가지

**1. 브랜치 사이에서 이름이 이미 갈라졌다.** 같은 노브가 브랜치마다 다른 이름이다.

```
video-perf-investigation            nonstandard-media-display-port
  SERVO_WR_PICTURE_TILE         ↔     SERVO_WR_PICTURE_TILE_SIZE
  SERVO_VIDEO_DEC_MAX_THREADS   ↔     SERVO_GSTREAMER_AVDEC_MAX_THREADS
```

병합하면 같은 일을 하는 변수가 조용히 둘이 된다.

**2. 한 브랜치 안에서도 갈라져 있다.** `SERVO_COMPOSITOR_DCOMP` 하나를 세 곳이 각자 파싱한다.

| 위치 | 판정 | 인정하는 값 |
|---|---|---|
| `third_party/surfman/src/platform/windows/angle/surface.rs:50` | `dcomp_native_compositor_requested() -> bool` | `1`/`true`/`yes`/`on`/`surface` |
| `components/paint/dcomp_compositor.rs:403` | `storage_mode() -> Hybrid \| SurfaceOnly` | `surface` 인지 여부만 |
| `components/shared/paint/examples/dcomp_native_poc.rs:197` | 강제 설정 | `set_var(.., "1")` |

값은 bool 이 아니라 **3상태**(off / on=하이브리드 / surface=가상서피스 전용)인데 두 함수가 서로 다른 문법으로 나눠 읽는다. 새 모드를 추가하면 두 파서를 다 고쳐야 하고, 한쪽을 잊으면 조용히 하이브리드로 떨어진다.

DComp 만 그런 것이 아니다. **서로 다른 크레이트에서 같은 변수를 각자 읽는 노브가 5 개**다.

| 변수 | 읽는 곳 |
|---|---|
| `SERVO_COMPOSITOR_DCOMP` | surfman / paint / shared-paint 예제 (3) |
| `SERVO_MEDIA_D3D11_VIDEO` | media(render-d3d11) / paint(painter) |
| `SERVO_D3D11_PROFILE` | media(render-d3d11) / media-thread |
| `SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF` | media(player) / script(htmlmediaelement) |
| `SERVO_WIN_VSYNC` | winit_wall / servoshell |

각 지점이 독립적으로 파싱하므로, 문법이나 기본값을 한쪽만 바꾸면 두 크레이트가 서로 다르게 동작한다. 화면상 증상 없이 반쪽만 켜지는 형태다.

**3. 잘못 써도 조용히 무시된다.** 대부분 `env::var(..).ok()?.parse().ok()?` 형태라 오타가 경고 없이 기본값으로 돌아간다. 게다가 다수가 `.is_ok()` 라 **`SERVO_DCOMP_READBACK=0` 을 넣어도 켜진다.**

## 2. 목표와 비목표

**목표**

- 표류·중복 제거, 조사가 끝난 죽은 노브 삭제
- 발견성 — 무엇이 있고 지금 무엇이 적용됐는지 한 곳에서 보인다
- 메커니즘 단일화 — 운용 설정은 pref 하나로

**비목표**

- servoshell / winit_wall 의 인자 파서 통합(bpaf vs 수제 루프). 범위를 넘고 얻는 것이 적다
- upstream servo 소유 노브 변경(`SERVO_TRACING`, `SERVO_DIAGNOSTICS`, `SERVO_STYLE_THREAD_STACK_SIZE_KB`)
- 다른 브랜치를 먼저 병합하는 일. 이 작업은 통합 라인에서 하고, 이후 병합 시 충돌로 드러나게 한다

## 3. 분류

39 개를 세 통으로 나눈다.

**판정 기준** — 새 노브가 어디로 갈지 다투지 않도록 명시한다.

1. 다른 장비에 배포할 때 값을 바꿔야 하는가 → **pref**
2. 이름에 `DISABLE_`/`PROF`/`DEBUG`/`READBACK` 이 붙거나, 특정 조사가 끝나면 지울 것인가 → **조사용 env**

### 운용 19 → pref

`COMPOSITOR_DCOMP`, `WIN_VSYNC`, `REFRESH_TIMER_HZ`, `WALL_FRAME_{PACING,MAX_PENDING,MIN_INTERVAL_MS}`, `MEDIA_{D3D11_VIDEO,SYNC_GROUP,GAPLESS_LOOP,DIRECT_FILE}`, `GSTREAMER_{AVDEC_MAX_THREADS,DISABLE_AUDIO,VIDEO_DECODER_POLICY}`, `VIDEO_SINK_POLICY`, `VIDEO_ESCAPE`, `VIDEO_ESCAPE_{STABLE_SWAPCHAIN,PROMOTE_HYSTERESIS}`, `VIDEO_DECOUPLE`, `WEBRTC_JITTER_LATENCY_MS`

### 조사 17 → env 유지(테이블로 등록)

`WALL_FRAME_DELAY_{TARGET_INDEX,AFTER,COUNT}`, `D3D11_PROFILE{,_MS}`, `DCOMP_{DEBUG,READBACK,VALIDPROBE,NO_PARTIAL_PRESENT,DISABLE_RESIZE_REBUILD,DISABLE_RESIZE_VIRTUAL}`, `DISABLE_VIDEO_{IMMEDIATE_COMPOSITE,UPDATE_COALESCE}`, `LOG_PRESENT_CADENCE`, `VIDEO_ESCAPE_PROF`, `MEDIA_DISABLE_ENOUGHDATA_BACKOFF`, `WR_PICTURE_TILE_SIZE`

### upstream 3 → 손대지 않음

`SERVO_TRACING`, `SERVO_DIAGNOSTICS`, `SERVO_STYLE_THREAD_STACK_SIZE_KB`

### 소유권에 대한 확인 (조사 결과)

- **`third_party/surfman` 은 이 포크가 소유한다.** 서브모듈이 아니라 in-tree 이고, DComp/월 작업으로 10회 이상 직접 수정했다. 자유롭게 고칠 수 있다.
- **`third_party/stylo` 는 다르다.** 2026-07-16 에 upstream 체크아웃을 통째로 복사한 것이고 이후 `parallel.rs` 를 건드린 커밋이 없다. `SERVO_STYLE_THREAD_STACK_SIZE_KB` 는 upstream 이 만든 노브다.

## 4. pref 이름 규칙과 매핑

포크가 이미 추가한 pref(`dom_webgpu_multigpu_fanout`, `media_wall_region_upload`, `media_screen_capture_*`)가 **servo 기존 네임스페이스에 그대로 끼어 있다.** 그 선례를 따라 별도 포크 전용 최상위 네임스페이스를 만들지 않는다.

`gfx_*`(컴포지터/표출)와 `media_*`(미디어) 둘을 쓴다.

★**pref 이름은 러스트 필드명 그대로, 밑줄 표기다.**★ 2026-08-11 실측 확인 — `ServoPreferences` 파생 매크로가 `stringify!(#name)` 으로 필드 식별자를 그대로 이름으로 쓴다(`components/config/macro/lib.rs:28-59`). 즉 CLI 는 `--pref gfx_dcomp_mode=on` 이고 **`--pref gfx.dcomp.mode=on` 은 통하지 않는다.** 저장소의 기존 용례도 같다(`--pref dom_webgpu_enabled=true`, `etc/multigpu/run_three_retargeting_wall.ps1:7`).

이 문서는 처음에 **점 표기로 잘못 적혀 있었다.** servo upstream 문서의 `layout.threads` 식 표기를 보고 그렇게 가정했는데 이 코드베이스에서는 성립하지 않는다. 로그·문서·스크립트가 점 표기를 쓰면 운영자가 그대로 따라 하다 실패한다.

**모르는 pref 이름은 조용히 무시되지 않고 즉시 패닉한다**(`Unknown preference: …`, `prefs.rs` 의 `set_value`). 오타가 조용히 넘어가지 않는다는 뜻이라 이 프로젝트가 경계하는 실패 유형은 아니다 — 다만 스크립트가 틀린 이름을 넘기면 **실행 자체가 죽는다.**

| 현재 env | pref | 타입 | 기본값 |
|---|---|---|---|
| `SERVO_COMPOSITOR_DCOMP` | `gfx_dcomp_mode` | String `off\|on\|surface` | `off` |
| `SERVO_WIN_VSYNC` | `gfx_vsync_enabled` | bool | `false` |
| `SERVO_REFRESH_TIMER_HZ` | `gfx_refresh_hz` | i64 | **`120`** (`[1,1000]` 클램프) |
| `SERVO_WALL_FRAME_PACING` | `gfx_wall_frame_pacing_enabled` | bool | 현행 |
| `SERVO_WALL_FRAME_MAX_PENDING` | `gfx_wall_frame_max_pending` | i64 | 현행 |
| `SERVO_WALL_FRAME_MIN_INTERVAL_MS` | `gfx_wall_frame_min_interval_ms` | i64 | 현행 |
| `SERVO_VIDEO_ESCAPE` | `gfx_video_escape_mode` | **String** (모드) | 현행 |
| `SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN` | `gfx_video_escape_stable_swapchain` | bool | **`true`** |
| `SERVO_VIDEO_ESCAPE_PROMOTE_HYSTERESIS` | `gfx_video_escape_promote_hysteresis` | i64 | **`10`** |
| `SERVO_VIDEO_DECOUPLE` | `gfx_video_decouple_enabled` | bool | **`true`** |
| `SERVO_MEDIA_D3D11_VIDEO` | `media_d3d11_enabled` | bool | 현행 |
| `SERVO_MEDIA_SYNC_GROUP` | `media_sync_group_enabled` | bool | 현행 |
| `SERVO_MEDIA_GAPLESS_LOOP` | `media_gapless_loop_enabled` | bool | 현행 |
| `SERVO_MEDIA_DIRECT_FILE` | `media_direct_file_enabled` | bool | 현행 |
| `SERVO_GSTREAMER_AVDEC_MAX_THREADS` | `media_avdec_max_threads` | i64 | 현행 |
| `SERVO_GSTREAMER_DISABLE_AUDIO` | `media_audio_enabled` | bool **(반전)** | `true` |
| `SERVO_GSTREAMER_VIDEO_DECODER_POLICY` | `media_video_decoder_policy` | String | 현행 |
| `SERVO_VIDEO_SINK_POLICY` | `media_video_sink_policy` | String | 현행 |
| `SERVO_WEBRTC_JITTER_LATENCY_MS` | `media_webrtc_jitter_latency_ms` | i64 | 현행 |

"현행"은 지금 코드가 env 미설정 시 쓰는 값이다. **추측하지 말고** 구현 시 각 호출부에서 실제 값을 읽어 `const_default()` 에 명시한다.

★**"env 미설정 = off" 로 추측하면 안 된다.** 실측에서 반례가 나왔다:

| 노브 | 실제 판정 | 기본값 |
|---|---|---|
| `SERVO_VIDEO_DECOUPLE` | `map(\|v\| v != "0").unwrap_or(true)` | **on** (킬스위치) |
| `SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN` | `as_deref() != Ok("0")` | **on** (킬스위치) |
| `SERVO_REFRESH_TIMER_HZ` | `unwrap_or(120)`, `[1,1000]` 필터 | 120 |
| `SERVO_VIDEO_ESCAPE_PROMOTE_HYSTERESIS` | `unwrap_or(10)` | 10 |

앞의 둘은 **기본 on 인 킬스위치**다. off 로 추측했으면 기능 두 개를 꺼뜨렸을 것이다.

★**`SERVO_VIDEO_ESCAPE` 는 bool 이 아니다.** `paint_api::rendering_context::video_escape_mode() -> VideoEscapeMode` 열거형(`External` 등)을 판정한다 — `gfx_dcomp_mode` 와 같은 다상태 노브다. 구현 시 실제 variant 를 전부 열거해 String 문법을 확정한다.

### 옮기면서 달라지는 세 가지

**1. `DISABLE_*` 를 긍정형으로 뒤집는다.** `SERVO_GSTREAMER_DISABLE_AUDIO` → `media_audio_enabled`(기본 `true`). servo pref 관례가 `*_enabled` 긍정형이고, 이중부정은 실수의 단골 자리다. **의미가 뒤집히는 변경**이므로 마이그레이션 표에 별도로 적는다.

**2. "존재하면 켜짐" 이 사라진다.** `.is_ok()` 로 읽던 것들은 진짜 bool 이 되어 `=false` 가 통한다. 직관과 맞는 방향이지만, 기존 스크립트에서 `=0` 으로 껐다고 믿던 것이 있으면 **동작이 달라진다.**

**3. 기본값을 명시하게 된다.** 지금은 "설정 안 하면 off" 가 암묵적이라, 배포 시 무엇이 켜져 있어야 하는지가 코드 어디에도 없고 실행 명령에만 있다.

`gfx_dcomp_mode` 만 String 인 이유는 3상태이기 때문이다. bool 두 개로 쪼개면 `on` 과 `surface` 가 배타적이라는 것을 타입이 막지 못한다.

## 5. DComp 게이트 — 파싱을 한 곳으로

제약은 소유권이 아니라 **의존 방향**이다. surfman 은 저수준 그래픽 크레이트라 `servo_config` 에 의존시키면 의존이 역류한다. 그래서 읽기를 없애는 대신 **값을 주입**한다.

```
pref  gfx_dcomp_mode = "off" | "on" | "surface"      ← 정본, 한 번만 파싱
        ↓ 기동 시 주입 (embedder/paint → surfman 의 OnceLock setter)
surfman: dcomp_native_compositor_requested()          ← 공개 시그니처 유지, 본문만 교체
paint:   storage_mode()                               ← 같은 주입값에서 유도
```

- `dcomp_native_compositor_requested()` 의 **공개 시그니처를 그대로 두므로 기존 호출부를 손대지 않는다.** 본문만 "env 읽기 → 주입값 읽기"로 바뀐다.
- 주입되지 않은 경우(surfman 단독 예제 `dcomp_native_poc`)에만 env 로 폴백한다. 해당 파일에 이미 `OnceLock` 이 import 돼 있어 패턴이 맞는다.
- 결과적으로 3상태를 한 곳에서 타입으로 파싱하므로, 새 모드를 추가할 때 고칠 곳이 하나가 된다.

## 6. 조사용 env 테이블

### 위치

조사 노브 17 개가 `components/paint`, `components/media/*`, `components/script` 에 흩어져 있다. 셋이 모두 의존하는 크레이트는 **`servo_config`** 하나이고 이미 pref 를 소유하므로 "설정은 여기" 라는 경계가 자연스럽다.

### 모양

```rust
// components/config/debug_env.rs
pub enum Kind { Presence, Int, Str }

pub struct DebugFlag {
    pub name: &'static str,
    pub kind: Kind,
    pub doc: &'static str,   // 무엇을 조사하려고 만든 노브인가
}

pub const DCOMP_READBACK: DebugFlag = DebugFlag {
    name: "SERVO_DCOMP_READBACK",
    kind: Kind::Presence,
    doc: "DComp 서피스를 CPU 로 읽어 내려 실제로 그려졌는지 확인한다(진단 전용, 느리다)",
};

/// 전량 목록 — 기동 덤프와 표류 테스트가 이것 하나만 본다.
pub const ALL: &[&DebugFlag] = &[&DCOMP_READBACK, /* … 17 개 */];
```

호출부는 이름 문자열을 갖지 않는다.

```rust
// before
*OFF.get_or_init(|| std::env::var("SERVO_DCOMP_NO_PARTIAL_PRESENT").is_ok())
// after
debug_env::enabled(&debug_env::DCOMP_NO_PARTIAL_PRESENT)
```

`OnceLock` 캐싱은 `debug_env` 안으로 들어간다 — 지금은 호출부마다 각자 만들어 그 패턴이 17 번 복제돼 있다.

### 값이 깨졌을 때

파싱이 한 곳으로 모이므로 **경고 한 줄**을 넣는다. `SERVO_D3D11_PROFILE_MS=abc` 면 경고를 찍고 기본값을 쓴다. 기동을 막지는 않는다 — 조사용 노브라 실행을 멈출 정도는 아니다.

### 표류 방지 테스트 (양방향)

```
방향 1: 소스 트리에서 env::var("SERVO_…") 로 읽는 이름을 모은다
        → 테이블에도 허용목록에도 없으면 실패
방향 2: 테이블의 각 이름이 소스에 최소 1 회 등장하는가
        → 아니면 실패(죽은 항목이 테이블에 쌓이는 것을 막는다)
```

- **허용목록은 upstream 3 개** + surfman 폴백 1 건(사유 주석 필수)
- 스캔에서 **`third_party/stylo` 는 제외, `third_party/surfman` 은 포함** — §3 의 소유권 판정 그대로
- **잡는 것**: 다른 브랜치 병합으로 딸려 들어온 새 env, 테이블 없이 추가된 노브, pref 로 옮기고 지우지 않은 env 읽기
- **못 잡는 것**: 이름을 상수나 `format!` 로 조립해 읽는 경우. 한 방향만 걸면 죽은 노브가 쌓이므로 반드시 양방향으로 건다

## 7. 발견성

### 기동 덤프

**기본값과 다른 것만** 한 줄씩 찍는다. 39 개를 매번 전부 찍으면 아무도 읽지 않는다.

```
servo: config: gfx_dcomp_mode=surface (default off)
servo: config: media_d3d11_enabled=true (default false)
servo: config: debug env: SERVO_DCOMP_READBACK, SERVO_D3D11_PROFILE_MS=250
```

아무것도 바꾸지 않았으면 한 줄도 나오지 않는다. 조용한 것이 기본이어야 무언가 떴을 때 의미가 생긴다. 이 덤프가 있으면 로그만 보고 "그때 어떤 설정으로 돌렸는지" 를 알 수 있다 — 지금은 실행 명령을 따로 보관해야만 안다.

### 문서와 문서 표류 방지

`docs/multigpu/configuration.md` 한 장에 pref 19 + 조사 노브 17 을 표로 둔다. 그리고 **문서도 테스트로 묶는다**: 테이블의 모든 이름이 문서에 등장하지 않으면 실패.

★ wall_view 에서 같은 수법(임베드 페이지 자체완결성 검사)을 쓰다가 **검사 자체가 느슨해져 통과만 하는** 함정을 두 번 겪었다. 그러므로 이 검사에도 **"검사가 무엇을 잡는지" 를 고정하는 테스트**를 함께 둔다 — 일부러 이름 하나를 빠뜨린 합성 문서로 실패를 확인하는 형태.

## 8. 두 셸의 wall 플래그

지금 `--wall-layout` 류가 `components/servo/examples/winit_wall/main.rs` 와 `ports/servoshell/prefs.rs` 두 곳에서 따로 파싱되고 이미 갈라졌다(`--wall-gpu-direct-present` 는 servoshell 만, `--backend`/`--capture` 는 winit_wall 만).

**파서는 통일하지 않는다**(비목표). 대신 **의미를 공유한다.** `components/shared/paint` 에 `wall_layout.rs` 옆으로:

```rust
pub struct WallArgs {
    pub layout: Option<PathBuf>,
    pub tile_index: usize,
    pub all_tiles: bool,
}

impl WallArgs {
    pub fn validate(&self) -> Result<(), WallArgsError>;
    pub fn resolve(self) -> Result<Option<WallLayout>, WallArgsError>;
}
```

두 셸은 각자 방식으로 채우기만 하고 검증·해석은 한 곳이다. 지금 servoshell 에만 있는 "`--wall-layout` 없이 `--wall-tile-index` 를 주면 경고" 같은 규칙이 winit_wall 에도 자동으로 생긴다.

셸별 고유 플래그(`--capture`, `--backend`)는 **의도적 차이**로 문서에 명시한다 — winit_wall 은 캡처 하니스다.

## 9. 마이그레이션 — 제거된 env 는 기동을 막는다

기존 스크립트·문서·노트가 옛 이름을 참조한다.

| 방식 | 결과 |
|---|---|
| 그냥 삭제 | 스크립트가 **조용히** 옛 설정을 잃는다. 화면은 도는데 DComp 가 꺼진 채 |
| 계속 인정 + 경고 | 정본이 둘이 된다 — 없애려던 표류가 남는다 |
| **설정돼 있으면 기동 차단** ★ | 무엇을 무엇으로 바꾸라고 정확히 알려주고 멈춘다 |

**차단을 택한다.** wall_view 의 인증 설정에서 이미 같은 판단을 했다 — *"설정이 틀리면 기동을 막는다(경고 후 통과가 아니다). 켰다고 믿는데 실제로는 안 켜진 상태가 아예 안 켠 것보다 나쁘기 때문"*. 여기에도 같은 논리가 그대로 적용된다.

```
error: SERVO_COMPOSITOR_DCOMP is no longer read.
       use --pref gfx_dcomp_mode=surface  (or gfx_dcomp_mode=on)
       see docs/multigpu/configuration.md
```

이 차단 목록은 **한시적**이다. 옮긴 19 개에 대해서만 두고, 정리가 안정된 뒤 걷어낸다.

## 10. 삭제 판정 기준

죽은 노브는 지금 단정하지 않는다. 기준만 정하고, 실제 삭제는 근거를 붙여 구현 단계에서 한다.

> 조사가 종결됐고(`docs/` 나 설계 노트에 결론이 적혀 있고), 그 결론이 "미적용" 이거나 기본값으로 확정된 노브는 삭제한다. **분기 자체를 코드에서 걷어낸다.**

삭제 목록은 각 항목의 근거(어느 문서 어느 절)를 제시한 뒤 확인받는다.

## 11. 검증

- `cargo test -p servo_config` — 표류 테스트 양방향, 문서 테스트, 그 검사들이 무엇을 잡는지 고정하는 테스트
- `cargo check -p servoshell`, `cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl --release`
- 옮긴 pref 각각에 대해 **기존 env 로 켰을 때와 새 pref 로 켰을 때 동작이 같은지** 스모크 1 회씩(특히 `gfx_dcomp_mode=surface`, `media_d3d11_enabled`)
- 제거된 env 를 설정한 상태에서 기동이 실제로 막히는지
- `rustfmt --edition 2024 --check`, `git diff --check`

## 12. 리스크와 미결

- **upstream servo 를 당길 때 `components/config/prefs.rs` 충돌이 늘어난다.** 이미 두 번 감수한 비용이며, 포크 전용 네임스페이스를 만들지 않기로 한 선례를 따른 결과다.
- **`media_audio_enabled` 반전**과 **`=0` 이 이제 진짜 off 로 동작**하는 것은 기존 스크립트의 동작을 바꾼다. 마이그레이션 표에 별도 표시.
- 다른 브랜치(`video-perf-investigation` 등)에는 여기 없는 노브(`SERVO_WALL_VIDEO_PACE_HZ`, `SERVO_PERF_GLFINISH`)가 있다. 그 브랜치를 병합할 때 표류 테스트가 실패하며 드러나므로, **병합자가 그 시점에 분류를 정해 테이블에 넣는다.** 이것이 의도된 동작이다.
- 기본값을 "현행" 으로 적은 항목은 구현 시 실제 코드에서 읽어 확정해야 한다. 잘못 적으면 조용히 동작이 바뀐다.

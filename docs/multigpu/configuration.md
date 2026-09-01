# 설정 노브 전량 (pref 26 + 조사용 env 17)

이 포크가 추가한 실행 설정의 **정본 목록**이다. 설계 근거는
`multigpu_config_surface_consolidation_design.md`, 이행 기록은
`docs/superpowers/plans/2026-08-11-config-surface-consolidation.md` 에 있다.

구분은 하나다:

- **운용 설정 → pref**. 무엇을 켜고 끌지 정하는 값. `--pref <이름>=<값>` 으로 준다.
- **조사용 → 등록된 환경변수**. 실패 주입·프로파일링·이분탐색용. 조사가 끝나면 지운다.

## ★ 먼저 읽을 세 가지

**1. pref 이름은 밑줄 표기다.** `--pref gfx_dcomp_mode=on` 이 맞고
`--pref gfx.dcomp.mode=on` 은 **틀리다.** `ServoPreferences` 파생 매크로가 러스트 필드
식별자를 `stringify!` 로 그대로 이름에 쓰기 때문이다. 그리고 **모르는 pref 이름은 조용히
무시되지 않고 `Unknown preference` 로 즉시 패닉한다** — 점 표기로 쓴 스크립트는 실행 자체가
죽는다.

**2. 옛 환경변수는 이제 기동을 막는다.** 아래 20 개 중 하나라도 설정된 채로 servoshell 이나
winit_wall 을 띄우면 무엇을 무엇으로 바꾸라는 안내를 찍고 종료한다. 그냥 무시했다면 스크립트가
**조용히** 옛 설정을 잃었을 것이다 — 실제로 이 저장소에서 `-DComp` 스위치가 그렇게 죽어 있던
적이 있다(엔진은 아무 경고도 찍지 않았다).

**3. 기본값이 반직관적인 것이 넷 있다.** `gfx_wall_frame_pacing_enabled`,
`gfx_video_escape_stable_swapchain`, `gfx_video_decouple_enabled`, `media_audio_enabled` 는
**기본이 `true`** 다. 앞의 셋은 킬스위치라 `=false` 로 끄는 용도이고, 마지막은 옛 부정형
(`SERVO_GSTREAMER_DISABLE_AUDIO`)을 뒤집은 것이다. "설정 안 하면 off" 로 추측하면 안 된다.

## 마이그레이션 표 (옛 환경변수 → 새 pref)

옛 이름으로 검색해 들어온 사람을 위한 표다. **값 문법이 그대로가 아닌 것**을 굵게 표시했다.

| 옛 환경변수 | 새 pref | 바꿔 쓰는 법 |
|---|---|---|
| `SERVO_COMPOSITOR_DCOMP` | `gfx_dcomp_mode` | **3 상태 모드다.** `--pref gfx_dcomp_mode=on`(하이브리드) 또는 `=surface` |
| `SERVO_WIN_VSYNC` | `gfx_vsync_enabled` | `--pref gfx_vsync_enabled=true` |
| `SERVO_REFRESH_TIMER_HZ` | `gfx_refresh_hz` | `--pref gfx_refresh_hz=N` |
| `SERVO_WALL_FRAME_PACING` | `gfx_wall_frame_pacing_enabled` | **기본 on.** 끄려면 `--pref gfx_wall_frame_pacing_enabled=false` |
| `SERVO_WALL_FRAME_MAX_PENDING` | `gfx_wall_frame_max_pending` | `--pref gfx_wall_frame_max_pending=N` |
| `SERVO_WALL_FRAME_MIN_INTERVAL_MS` | `gfx_wall_frame_min_interval_ms` | `--pref gfx_wall_frame_min_interval_ms=N` |
| `SERVO_VIDEO_ESCAPE` | `gfx_video_escape_mode` | **모드 토큰이다.** `--pref gfx_video_escape_mode=external` |
| `SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN` | `gfx_video_escape_stable_swapchain` | **기본 on 킬스위치.** 옛 `=0` 이 `=false` 다 |
| `SERVO_VIDEO_ESCAPE_PROMOTE_HYSTERESIS` | `gfx_video_escape_promote_hysteresis` | `--pref gfx_video_escape_promote_hysteresis=N` |
| `SERVO_VIDEO_DECOUPLE` | `gfx_video_decouple_enabled` | **기본 on 킬스위치.** 옛 `=0` 이 `=false` 다 |
| `SERVO_DISABLE_VIDEO_IMMEDIATE_COMPOSITE` | `gfx_video_immediate_composite_enabled` | ★**의미가 반대다**★ — 옛 env 는 "끄기", 새 pref 는 "켜기"(기본 **켜짐**). 비디오 도착이 합성을 구동하되 `gfx_refresh_hz` 주기로 합쳐서 요청한다. 완전히 끄려면 `--pref gfx_video_immediate_composite_enabled=false` |
| `SERVO_MEDIA_D3D11_VIDEO` | `media_d3d11_enabled` | `--pref media_d3d11_enabled=true` |
| `SERVO_MEDIA_SYNC_GROUP` | `media_sync_group_target` | **불리언이 아니라 개수다.** `--pref media_sync_group_target=N` (타일 수). 이름도 바뀌었다 |
| `SERVO_MEDIA_GAPLESS_LOOP` | `media_gapless_loop_enabled` | `--pref media_gapless_loop_enabled=true` |
| `SERVO_MEDIA_DIRECT_FILE` | `media_direct_file_enabled` | `--pref media_direct_file_enabled=true` |
| `SERVO_GSTREAMER_AVDEC_MAX_THREADS` | `media_avdec_max_threads` | `--pref media_avdec_max_threads=N`. **미설정은 `-1`** 이다(`0` 은 1 스레드 강제) |
| `SERVO_GSTREAMER_DISABLE_AUDIO` | `media_audio_enabled` | ★**의미가 뒤집혔다.** 옛 `=1` 은 `--pref media_audio_enabled=false` 다 |
| `SERVO_GSTREAMER_VIDEO_DECODER_POLICY` | `media_video_decoder_policy` | `--pref media_video_decoder_policy=auto` (또는 `=software`) |
| `SERVO_VIDEO_SINK_POLICY` | `media_video_sink_policy` | `--pref media_video_sink_policy=low-latency` (또는 `=smooth`) |
| `SERVO_WEBRTC_JITTER_LATENCY_MS` | `media_webrtc_jitter_latency_ms` | `--pref media_webrtc_jitter_latency_ms=N` |
| `SERVO_WR_PICTURE_TILE_SIZE` | `gfx_wr_picture_tile_size` | `--pref gfx_wr_picture_tile_size=WxH`, 또는 **`=display`** 로 타일 창 크기에 맞춤 |

### 옮기면서 달라진 두 가지

**"존재하면 켜짐" 이 사라졌다.** 옛 노브 중 여럿이 `.is_ok()` 나 truthy 문자열 매칭이라
`FOO=0` 이 **켜짐**이었다. 이제 진짜 bool 이라 `=false` 가 통한다. 직관과 맞는 방향이지만,
`=0` 으로 껐다고 믿던 스크립트가 있으면 동작이 달라진다.

**기본값이 코드에 적혔다.** 지금은 `Preferences::const_default()` 가 정본이고, 기동 시
기본값과 다른 것만 `servo: config: <이름>=<값> (default <기본값>)` 으로 찍힌다. 조용한 것이
정상이다 — 전량을 매번 찍으면 아무도 읽지 않는다.

## pref 26 개

기본값은 전부 `components/config/prefs.rs` 의 `const_default()` 에서 온 것이다.

앞의 20 개는 옛 환경변수를 옮겨 온 것이고(위 마이그레이션 표), 마지막
`### 표출용 웹 보안 완화` 세 개는 옮겨 온 것이 아니라 **새로 추가한 것**이라 옛 이름이
없다.

### `gfx_*` — 컴포지터 / 표출

| pref | 타입 | 기본값 | 설명 |
|---|---|---|---|
| `gfx_dcomp_mode` | String | `""` (= off) | DirectComposition 네이티브 컴포지터 모드. `off`/빈 문자열 = 기존 Draw 경로, `on` = 하이브리드(전면 갱신 서피스를 스왑체인으로 승격), `surface` = 가상 서피스 전용. 3 상태이므로 bool 두 개로 쪼개지 않는다 — `on` 과 `surface` 가 배타적이라는 것을 타입이 막아야 한다. 모르는 값은 경고 후 off. |
| `gfx_vsync_enabled` | bool | `false` | DWM vsync(`DwmFlush`) 페이싱 드라이버. 기본 off 인 이유는 `DwmFlush` 가 코어 하나를 상시 소모하기 때문이다. |
| `gfx_refresh_hz` | i64 | `120` | 리프레시 드라이버 목표 주파수. `[1,1000]` 범위를 벗어나면 경고 후 기본값을 쓴다. |
| `gfx_wall_frame_pacing_enabled` | bool | **`true`** | wall 프레임 페이싱(latest-wins). 이름과 달리 **기본 on** 이다 — 옛 판정이 `mode == Latest` 였다. |
| `gfx_wall_frame_max_pending` | i64 | `1` | wall 프레임 배리어가 허용하는 미완료 프레임 수. |
| `gfx_wall_frame_min_interval_ms` | i64 | **`0`** | wall 프레임 사이 최소 간격(ms). ***`0` = `gfx_refresh_hz` 한 주기로 유도***. ★옛 기본값 `16` 은 62.5Hz 이고, 정수 ms 로는 60Hz(16.667ms)를 표현할 수 없다★ — 16 은 4.2% 빠르고 17 은 2% 느리다. 유도하면 페인트 타이머 및 비디오 합성 합치기와 같은 박자가 된다. 양수를 명시하면 그 값을 그대로 쓴다. |
| `gfx_wr_picture_tile_size` | String | `""` (= 오버라이드 없음) | WebRender picture cache 타일 크기. `""` = WR 기본 분기(콘텐츠 1024x512, 스크롤바는 WR 이 자체 특수 크기), `WxH`(예 `1920x1080`) = 모든 painter 동일, **`display`** = painter 마다 자기 창 크기. 타일이 창 이상이면 슬라이스당 타일 1 장이 된다. ★**WR 은 이 값을 검사도 클램프도 하지 않는다**(2026-08-12 확인) — 실질 상한은 GPU 텍스처 크기다.★ ★**쪼개면 싸질 것이라는 예상은 반증되었다**(2026-08-26, 45xFHD30 5760x2160): `display` render_ms p50 29.9ms / present 28.6/s 인데, `1024x1024` 는 99.6ms / 8.4/s, `512x512` 는 108.4ms / 8.5/s 로 **합성 1 회가 3.5 배 느려진다**. 전면이 매 프레임 바뀌는 비디오 월에서 픽처 캐시는 순수 오버헤드이므로 `display` 를 유지할 것.★ |

### `gfx_video_*` — 비디오 WR 탈출 / 분리

| pref | 타입 | 기본값 | 설명 |
|---|---|---|---|
| `gfx_video_escape_mode` | String | `""` (= off) | 비디오를 WebRender 밖으로 빼는 모드. 유효 토큰은 `external` 하나뿐이고 빈 문자열이 off 다. DComp 게이트가 켜져 있어야 실제로 동작한다. |
| `gfx_video_escape_stable_swapchain` | bool | **`true`** | 킬스위치. 탈출 경로의 스왑체인 안정화를 끄려면 `=false`. |
| `gfx_video_escape_promote_hysteresis` | i64 | `10` | 비디오 레이어를 탈출 경로로 승격하기 전에 기다리는 프레임 수. |
| `gfx_video_escape_buffer_count` | i64 | `2` | **external 비디오 스왑체인 전용** 백버퍼 수(2..=4, 범위 밖은 경고 후 클램프). ★2 면 in-flight 프레임이 1 장이라 `Present` 가 컴포지터의 백버퍼 반납을 기다린다★ — 45xFHD30 실측에서 렌더러 스레드가 1 초 중 **85%(854ms)를 Present 에서** 보내는데 CPU 는 0.3 코어도 안 쓴다(= 일하는 게 아니라 자고 있다). 1 회당 비용도 영상 수에 따라 0.42ms(20 개) → 0.70ms(45 개)로 커진다(고정 드라이버 비용이면 안 커진다). **콘텐츠 스왑체인에는 적용되지 않는다** — 그쪽 부분 Present 의 catch-up 복사가 `GetBuffer(1)`=직전 프레임이라는 정확한 2 버퍼 핑퐁을 전제한다. |
| `gfx_video_decouple_enabled` | bool | **`true`** | 킬스위치. 비디오 프레임 갱신을 씬 재합성에서 분리하는 경로를 끄려면 `=false`. |
| `dom_image_max_decoded_pixels` | i64 | **`8294400`** (3840x2160) | 디코드된 이미지를 GPU 에 올리기 전에 줄이는 상한(픽셀). `0` 이하면 끈다. ★파일 크기가 아니라 디코드 크기가 GPU 비용이다★ — 실측 8.3MB JPEG = 8103x5405 = **175MB** RGBA. WebRender standalone 축출 임계값이 **8MB**(`texture_cache.rs`)이고 그 **4 배(32MB)**를 넘으면 매 프레임 축출 모드라, 그 이미지 하나가 타일 렌더를 1.1ms → 26ms, 월을 48.7 → 18.5 presents/s 로 떨어뜨렸다. ***intrinsic 크기는 안 바뀐다*** — 레이아웃 동일, 텍스처 해상도만 하락(캡처 대조 확인). 단일 프레임 RGBA8/BGRA8 만. ★참고: 상한 자체가 4K 면 이미지 한 장이 33.2MB 라 이미 32MB 선을 넘는다★ — 표출이 FHD 라면 `=2073600` 이 맞다. |
| `gfx_video_immediate_composite_enabled` | bool | **`true`** | 비디오 프레임 도착이 합성을 구동할지. ***도착마다가 아니라 `gfx_refresh_hz` 주기로 합쳐서*** 요청한다(`painter.rs` `video_composite_due`). ★2026-08-31 실측: 양극단이 둘 다 틀렸다★ — **도착마다**면 요청률이 전 영상 프레임레이트의 합이 되어(36xFHD30 = 1080/s 를 60Hz 월에) 4-GPU 월에서 **39.19 코어** 대 **22.72 코어**, 이질 배치 primary 타일 렌더 **17.42ms 대 6.02ms**. **아예 끄면** 영상만 움직이는 페이지에서 합성을 구동할 것이 없어 **초당 28 회**로 주저앉는다 — 콘텐츠 자신의 30fps 보다 낮다(CSS 애니메이션 도는 동안만 86/s). 합치면 **하한**(페인트 주기)과 **상한**을 동시에 얻는다. `=false` 면 비디오가 합성을 전혀 구동하지 않는다. |

### `media_*` — 미디어

| pref | 타입 | 기본값 | 설명 |
|---|---|---|---|
| `media_d3d11_enabled` | bool | `false` | GStreamer D3D11 per-pipeline GPU 업로드 / YUV 직접 샘플 경로. 꺼지면 기존 CPU(I420 borrowed) 경로를 쓴다. |
| `media_sync_group_target` | i64 | `0` | ★**온오프가 아니라 함께 출발하기를 기다릴 파이프라인 수**★. 타일 수를 그대로 준다. `2` 미만(기본 `0` 포함)이면 비활성. 30 초 워치독이 목표 수에 못 미쳐도 그룹을 놓아준다. |
| `media_gapless_loop_enabled` | bool | `false` | `<video loop>` 무결절 SEGMENT 재탐색 루프. 꺼지면 스펙대로 EOS → flushing seek(0) 경로를 쓴다. |
| `media_direct_file_enabled` | bool | `false` | `file://` 미디어를 GStreamer 가 직접(filesrc) 읽는다. 스크립트 스레드 바이트 왕복을 없앤다. 스크립트 단 힌트(`is_direct_local`)와 OR 로 합쳐진다. |
| `media_avdec_max_threads` | i64 | `-1` | 소프트웨어 `avdec_*` 디코더의 워커 스레드 상한. **`-1` = 미설정(자동)** 이고 `0` 이상이면 그 값으로 캡한다 — `0` 은 "자동" 이 아니라 1 스레드 강제라 보초값으로 쓸 수 없다. `-1` 보다 작은 값은 경고 후 무시된다. |
| `media_numa_pin_streaming_threads` | bool | **`true`** | 비디오 스트리밍 스레드를 NUMA 노드에 라운드로빈으로 고정한다. ★기본 true★. ★두 소켓을 쓰면서 메모리도 로컬로 두려는 것이다.★ 지금은 스레드가 두 소켓을 오가는데(고정 없음) 힙은 first-touch 로 한 노드에 잡히므로, 프레임 버퍼 3.1MB 를 초당 30 회 x 영상 수만큼 소켓 간 링크로 나른다. 반대로 프로세스를 한 노드에 하드 고정하면 지역성은 얻지만 물리 코어가 절반(20)으로 줄어 SMT 가 포화한다 — 실측(54xFHD30, 코어/영상): 고정 없음 1.005, 프로세스 고정 0.735, 54 개 별도 프로세스 0.418. 스레드를 노드에 붙박이로 두면 둘 다 얻는다: 그 스레드가 할당한 버퍼가 그 노드에 잡히고 같은 스레드가 읽으며(Windows 힙의 스레드별 캐시가 돕는다), 절반씩 나뉘어 물리 40 코어를 다 쓴다. 경합 없는 구간의 0.45~0.48 이 유지되면 54 영상이 25 코어로, 40 물리 코어 안에 들어간다. 실측(2026-08-27, 54xFHD30): 디코드 0.99 -> **0.53** 코어/영상, 머신 71% -> **35%**, `pts_rate` 0.70 -> **1.00**. 대조군(비 Servo 월)이 같은 장비에서 54 영상을 50~70% 로 도는데 이쪽이 더 낮다. **기본값이 true 인 이유**: 노드가 하나인 장비에서는 아무 일도 하지 않고 즉시 돌아간다(no-op). 노드가 둘 이상인 장비에서는 켜지 않을 이유가 측정되지 않았다 — 끄면 스레드가 소켓을 오가며 원격 메모리를 물거나(1.005), 프로세스를 한 노드에 가두면 물리 코어가 절반이 된다(0.735). 되돌리려면 `=false`. **비범위**: GStreamer 가 만드는 다른 스레드(`multiqueue:src`, `qtdemux:sink`)는 잡지 않는다 — 실측상 전부 합쳐 1 코어 미만이라 뜨거운 스레드 하나로 충분했다. 그리고 영상의 절반은 GPU 가 붙지 않은 노드에서 업로드하게 되는데, 업로드는 영상당 0.047 코어라 디코드 전체가 원격인 것보다 싸다 — 실측으로 확인됐다. |
| `media_audio_enabled` | bool | **`true`** | ★**의미가 뒤집힌 pref**★. 옛 `SERVO_GSTREAMER_DISABLE_AUDIO` 는 끄는 스위치였고 이것은 켜는 스위치다. |
| `media_video_decoder_policy` | String | `""` (= software) | 비디오 디코더 선택 정책. 인정 토큰(대소문자 무시): `auto`/`default` = 자동 선택 유지, `software`/`avdec` = 소프트웨어 디코더 강제. 그 외 값은 경고 후 software. |
| `media_video_sink_policy` | String | `""` (= smooth) | appsink 버퍼링/지연 정책. 인정 토큰(대소문자 무시): `low-latency`/`low_latency`/`latency`, `smooth`/`complete`. 그 외 값은 경고 후 smooth. |
| `media_video_sink_qos` | String | `""` (= 정책값) | appsink 의 `qos` 만 정책과 **독립적으로** 덮어쓴다. 토큰: `on`/`true`/`1`, `off`/`false`/`0`. 그 외는 경고 후 정책값. ★`media_video_sink_policy` 는 qos/drop/max-lateness/max-buffers 를 **한 묶음**으로 바꾸므로 qos 만 재려면 이쪽을 쓴다.★ 기본(Smooth)은 `qos=false` 이고 이는 GStreamer 기본이 아니라 **이 포크가 명시적으로 끄는 값**이다 — 꺼져 있으면 QoS 이벤트가 상류로 가지 않아 **avdec 이 부하 시 프레임을 건너뛰지 못한다**(과부하에서 완만한 열화 대신 절벽). |
| `media_video_sink_pacing` | String | `""` (= clock) | 비디오 싱크의 **페이싱 방식**. `clock` = 현행(appsink `sync=true`, 파이프라인 클럭 대기). `thread` = appsink `sync=false` + 스트리밍 스레드가 PTS 앵커에 맞춰 직접 잠. `none` = **진단 전용**(`sync=false` + sleep 도 안 함) — 클럭 대기 비용과 페이싱 자체의 효과를 분리하려고 넣었다. 실제 재생에 쓰면 디코더가 최대 속도로 앞지른다. ★`GstSystemClock::obtain()` 은 프로세스당 싱글턴이라 파이프라인 45 개의 싱크가 프레임마다 **같은 객체**에서 대기한다★ — 실측(80 논리코어, 45xFHD30, Servo 없이 디코드만): 45 프로세스 0.399 코어/영상, 1 프로세스 0.795, 1 프로세스 + 클럭 없이 sleep 0.284. 공유 클럭 대기가 디코딩만큼을 더 쓴다. ★단 **월에서는 그 해석이 그대로 옮겨오지 않는다**(2026-08-26) — `none` 을 넣어 분리해 보니 개발기 20 영상에서 clock 12.7ms/프레임, none 12.5ms/프레임으로 같고 thread 만 7.25ms 다. 이득의 원인은 클럭 제거가 아니라 **재우는 행위가 동시성을 낮춰 경합을 줄이는 것**이다.★ 그래서 테스트 장비 20 영상에서는 CPU 43% 감소(16.5 -> 9.2 코어)로 유효하지만, 45 영상에서는 이미 GPU 가 병목이라 차이가 없다(45.7 vs 45.3 코어). **비범위**: `thread` 는 파이프라인마다 독립 앵커라 영상 간 동기를 보장하지 않는다(`media_sync_group_target` 과 양립 불가) — 별도 과제 Video Sync Group. |
| `media_pipeline_mode` | String | `""` (= playbin3) | 미디어 파이프라인 구성. `playbin3` = 현행(`gstreamer_play::Play` 가 소유). `uridecodebin3` = playsink 없이 직접 구성하고 appsink 를 손으로 링크. ★playbin3 는 playsink 를 포함하고 playsink 가 디코더와 appsink 사이에 `vqueue` 를 끼운다 — 그 큐가 **원본 3.1MB 프레임을 프레임마다 스레드 경계 너머로 나른다**★ (실측 45xFHD30, 코어/영상: 큐 없음 0.284 / 디먹서 직후 큐+오디오 0.294 / multiqueue+뒤큐 0.729). `decodebin3` 의 코덱 자동선택은 유지된다(H264/H265/VP9 등) — 없애는 것은 playsink 뿐이다. **제약**: 렌더러가 appsink 자체가 아닌 것을 붙이면(unix 의 `glsinkbin`) 쓸 수 없고 경고 후 폴백한다. 트랙 선택 API 미구현. |
| `media_webrtc_jitter_latency_ms` | i64 | `0` | `webrtcbin` 지터버퍼 latency(ms). `webrtcbin` 자체 기본은 200ms 인데 로컬/LAN 캡처에서는 그대로 고정 지연이 되므로 0(무버퍼)으로 둔다. 네트워크 지터로 프레임이 끊기면 올린다. |

### 표출용 웹 보안 완화 — `dom_enforce_framing_policy` / `network_enforce_mixed_content` / `dom_iframe_toplevel_embed_enabled`

**앞의 둘은 기본이 `true`(= 표준 동작)** 이다. 끄는 것은 월 표출 전용 탈출구이고, 일반
브라우징용으로 끄면 안 된다. 세 번째(`dom_iframe_toplevel_embed_enabled`)는 기본이
`false` 다.

| pref | 타입 | 기본값 | 설명 |
|---|---|---|---|
| `dom_enforce_framing_policy` | bool | **`true`** | 사이트가 선언한 프레이밍 정책(`X-Frame-Options`, CSP `frame-ancestors`)을 **자식 navigable(iframe)** 내비게이션에서 강제한다. `=false` 면 두 검사를 건너뛴다. 최상위 내비게이션은 원래 이 경로로 막히지 않으므로(`xframeoptions.rs` Step 1) 영향이 없다. |
| `network_enforce_mixed_content` | bool | **`true`** | secure context 에서 비-secure 하위 리소스(mixed content)를 차단한다. `=false` 면 `file://` 부모 페이지의 iframe 이 `http://` 대상을 열 수 있다. |
| `dom_iframe_toplevel_embed_enabled` | bool | `false` | `<iframe toplevel>` 을 인정한다. 그 속성이 붙은 iframe 은 부모의 박스 안에서 그대로 렌더되면서(모든 CSS 적용) 내용만 top-level browsing context 가 되어, `X-Frame-Options`/`frame-ancestors`/frame-busting 이 **성립하지 않는다**. 꺼져 있으면 속성은 완전히 무시된다. |

**★위험★**

- 설계상 **스푸핑을 허용**한다. 사설망 표출 전용 키오스크 전제에서만 켠다.
- v1 은 입력 라우팅이 없어 클릭재킹은 성립하지 않지만, **입력을 얹으면 되살아난다.**
- **게이트가 문서 단위가 아니라 프로세스 전역**이다. `toplevel` 로 띄운 외부 사이트가 자기
  안에 또 `<iframe toplevel>` 을 쓸 수 있다. "운용자가 작성한 월 레이아웃 HTML만 이 속성을
  쓴다"는 전제는 코드에 없다.
- **임베드된 사이트가 월 창의 제목과 로드 상태를 가져간다.** 임베드 문서는 `is_top_level()`
  이 true 가 되어 `ChangePageTitle` / `NotifyLoadStatusChanged` 를 **월 WebView 의 id 로**
  보낸다(`components/script/dom/document/document.rs` 의 세 지점). servoshell 에서는 창
  제목이 외부 사이트 제목으로 바뀐다. 즉 스푸핑 표면이 *우리 UI 안의 남의 사이트* 를 넘어
  **브라우저 크롬 자체** 까지 닿는다.

**앞의 두 pref 와의 관계.** `<iframe toplevel>` 을 쓰면 `dom_enforce_framing_policy` 와
`network_enforce_mixed_content` 는 **필요 없다.** 앞의 둘은 검사를 *끄는* 것이고,
`toplevel` 은 애초에 그 검사에 *도달하지 않는* 것이다. 평범한 iframe 을 그대로 쓰면서
정책만 무르게 하고 싶을 때 앞의 둘을 쓴다.

**★`toplevel` 이 풀어주지 않는 것★** — 부모 페이지와 임베드된 사이트는 **서로 통신할 수
없다**(부모가 없으므로 `postMessage` 상대가 없다). 입력·포커스·history 는 v1 비범위다.

**왜 필요한가.** 월에 "우리 레이아웃 HTML + 그 안의 iframe 으로 외부 사이트" 를 띄우려는
시도는 상용 사이트 대부분이 `XFO: DENY` 라 성립하지 않는다(naver.com·iana.org 실측; Chrome
도 동일하게 막힌다). 월은 사설망 표출 전용 키오스크라 이 헤더가 지키는 클릭재킹 방어가 사
줄 것이 없으므로, 셸 수준에서 명시적으로 끌 수 있게 했다 — `--ignore-certificate-errors`
와 같은 성격의 노브다.

**앞의 두 pref 로 끄고도 안 되는 것 세 가지**:

1. **JS frame-busting** — `if (top !== self) top.location = self.location` 은 헤더가 아니라
   스크립트다. iframe 에 `sandbox="allow-scripts allow-same-origin"`(top-navigation 토큰
   제외)을 주어 봉쇄한다.
2. **로그인 세션** — 쿠키가 `SameSite=Lax` 면 서드파티 프레임에 안 붙는다. 로그인이 필요한
   대시보드는 이 경로로 못 띄운다.
3. **반응형 붕괴 / Servo 웹호환성** — 정책이 아니라 별개 문제로 그대로 남는다.

**`toplevel` 은 이 셋을 어떻게 바꾸는가.** frame-busting(항목 1)은 `toplevel` 을 쓰면
임베드된 문서에서 `top === self` 라 그 스크립트 자체가 발동하지 않는다 — 항목 1 은
`toplevel` 에는 해당하지 않는다. 로그인 세션(항목 2)은 임베드된 문서가 스스로 top-level
이므로 서드파티 프레임이라는 전제가 사라진다. ★다만 이 포크에서 실측하지 않았다★ —
쿠키가 실제로 붙는지는 확인이 필요하다. 게다가 **이 엔진은 SameSite 쿠키 속성을 애초에
구현하지 않는다**(쿠키 저장소 `components/net/cookie.rs` / `cookie_storage.rs` 어디에도
`same_site`/`SameSite` 가 없다 — `components/net` 전체의 다른 매치는 무관한 Fetch
`Sec-Fetch-Site` 헤더 판정이다) — 즉 `toplevel` 이 서드파티 전제를 없앤 것이 아니라, 그
전제를 지키던 검사가 이 포크에는 처음부터 없었다. 실측 헤지는 그대로 유효하지만, 운용자가
"SameSite 가 막아 줄 것" 이라는 헛검증을 하지 않도록 적어 둔다. 반응형 붕괴 / Servo
웹호환성(항목 3)은 `toplevel` 로도 그대로 남는다.

**차단 사유가 안 보이는 함정.** Servo 내장 에러 페이지(`neterror.html`)는 CSS 가 한 줄도
없어 캔버스가 transparent 로 남고(`layout/display_list/mod.rs` 가 투명 루트 배경에서 조기
리턴), 흰 배경의 출처인 `shell_background_color_rgba` 는 painter 의 **창** clear color 라
nested document 에 적용되지 않는다. **진단용 iframe 의 배경은 반드시 밝게 둘 것** — 어둡게
두면 차단된 로드가 검정 위 검정으로 그려져 "아무것도 안 그려짐" 으로 오진한다(실제로 3 회
연속 오진했다).

## 조사용 환경변수 15 개

**이것들은 pref 가 아니다.** 실패 주입·프로파일링·이분탐색용이고, 조사가 끝나면 지운다.
등록처는 `components/config/debug_env.rs` 한 곳이며 호출부는 이름 문자열을 갖지 않는다.

`Kind` 는 셋이다. **`Presence`** 는 설정 여부만 보고 값은 무시한다(`FOO=0` 도 켜짐).
**`Int`** 는 정수. **`Str`** 는 원본 문자열을 호출부가 직접 판정하는데, 판정식이 노브마다
`"1"/"true"`, `"1"/"true"/"on"`, `"1"/"true"/"yes"/"on"` 세 변종으로 갈려 있어 통일하지
않았다 — 통일 자체가 조용한 동작 변경이 되기 때문이다.

아래 설명은 `debug_env.rs` 의 `doc` 필드 원문이다(테스트가 일치를 강제한다).

### `SERVO_WALL_FRAME_DELAY_TARGET_INDEX` — `Int`

wall frame barrier 실패 주입: 프레임 준비 신호를 보류할 0-기반 paint target 인덱스. 미설정이면 주입 기능 전체가 꺼진다(느린 렌더러 시뮬레이션, Phase 5).

### `SERVO_WALL_FRAME_DELAY_AFTER` — `Int`

위 주입이 적용되기 시작하는 첫 logical_frame_id. 미설정/파싱 실패 시 기본 1.

### `SERVO_WALL_FRAME_DELAY_COUNT` — `Int`

지연을 적용할 logical frame 개수. 미설정/파싱 실패 시 기본 1, 0이면 주입 무효화.

### `SERVO_D3D11_PROFILE` — `Str`

D3D11 비디오 렌더 경로의 단계별 타이밍 계측을 켠다. truthy: "1"/"true"/"on" (대소문자 무시). render-d3d11 크레이트와 media-thread 크레이트 두 곳에서 각자 이 진단을 게이트한다.

### `SERVO_D3D11_PROFILE_MS` — `Str`

위 계측의 로그 임계값(ms, f64). build_frame 총시간이 이 값 이상일 때만 로깅. 미설정/파싱 실패 시 기본 8.0. 정수 전용 파서로는 표현 못 하는 f64 값이라 Str로 등록했다(예 "8.5").

### `SERVO_DCOMP_DEBUG` — `Presence`

DComp bind/add_surface 좌표 진단 로그(Task 5 스모크 디버깅). 타일당 프레임당 불리는 핫패스라 OnceLock 캐시가 필수.

### `SERVO_DCOMP_READBACK` — `Presence`

DComp 서피스를 CPU 로 읽어 내려 실제로 그려졌는지 확인한다(Task 6 결함 진단 전용, glReadPixels로 파이프라인을 멈추므로 느리다, 호출부가 <=120프레임으로 제한).

### `SERVO_DCOMP_VALIDPROBE` — `Presence`

Task 9 결함 진단: non-opaque 서피스의 per-Virtual-bind valid_rect/dirty_rect 커버리지 로그(<=300프레임으로 제한).

### `SERVO_DCOMP_NO_PARTIAL_PRESENT` — `Presence`

진단: 부분 Present만 끄는 스위치(스펙 §3) — 강등 폴백 경로 검증용.

### `SERVO_LOG_PRESENT_CADENCE` — `Str`

진단: 초당 1회, 실제 엔진 present 빈도(frame-ready rate)와 프레임 간 최악 간격을 로깅한다 — 페이지의 requestAnimationFrame 카운트나 외부 캡처 도구와 무관한 그라운드트루스.

### `SERVO_DCOMP_DISABLE_RESIZE_REBUILD` — `Str`

킬 스위치(기본 = 활성). 런타임 리사이즈(드래그/최대화) 정착 후 picture-cache 재구축을 끈다 — Task 12/12b 마스터 스위치, A/B 검증 및 회귀 시 안전 밸브. Windows 전용.

### `SERVO_DCOMP_DISABLE_RESIZE_VIRTUAL` — `Str`

task-12b 전용 킬 스위치(기본 = 활성). "드래그 중 가상 서피스 모드"만 끄고 Task 12의 정착 재구축은 유지한다. Windows 전용.

### `SERVO_VIDEO_ESCAPE_PROF` — `Presence`

external 비디오 present 파이프라인 프로파일러 게이트. 켜지면 렌더러 스레드가 초당 1회 [vesc-prof] 집계 라인(info)을 낸다 — acquire/convert/present 중 어느 단계가 프레임 예산을 먹는지 판독용.

### `SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF` — `Str`

킬 스위치: PlayerError::EnoughData 백오프(요청 취소/재탐색)를 끈다. truthy: "1"/"true"/"yes"/"on"(대소문자 무시). gstreamer player 백엔드와 HTMLMediaElement 두 곳에서 각자 같은 판정으로 게이트한다.

### `SERVO_MEDIA_VIDEO_RATE` — `Str`

파이프라인별 VIDEORATE 요약을 1초에 한 줄씩 info로 찍는다. truthy: "1"/"true"/"on"(대소문자 무시). fps는 appsink가 실제로 받은 프레임 수, pts_rate는 pts가 벽시계 대비 몇 배로 진행하는지다 — 1.0이면 정상 재생, 2.7이면 디코더가 그만큼 앞질러 돌고 있다는 뜻. 기본 off인 이유는 45타일 장시간 운용에서 초당 45줄이 쌓이기 때문이다(같은 이유로 기존 sample summary는 debug에 있다).

```
VIDEORATE id=12 fps=82.3 pts_rate=2.74x frames=247 window_ms=1002
```

★CPU 수치만으로는 "디코더가 2.7배 빠르게 앞질러 돈다"와 "1배로 도는데 프레임당
비용이 2.7배다"가 구분되지 않는다★ — 이 줄이 그 둘을 가른다. gapless 루프가 pts 를
되감은 창은 비율 대신 `wrapped` 로 표시하고 집계에서 뺀다. 런처는 `-VideoRate` 로
켜고 종료 후 min/median/max 를 요약한다.

### `SERVO_FRAME_REASON_PROF` — `Str`

generate_frame 을 부른 호출 지점과 RenderReasons 를 1초에 한 줄로 집계한다. truthy: "1"/"true"/"on"(대소문자 무시). 합성 요청 경로가 9 곳이라 어느 것이 초당 200 회를 만드는지 로그만 보고는 가를 수 없다. 호출 지점은 #[track_caller] 로 얻으므로 호출부는 손대지 않는다. 프레임마다 불리는 핫패스라 기본 off 다.

```
FRAMEREASON total=217 window_ms=1001 painter.rs:2030/SCENE=198 painter.rs:2136/SCENE=1
```

실측(2026-08-25, 20 영상 단일 타일): 합성의 99% 가 `generate_frame_for_script`
한 지점에서 나왔고 영상 도착 경로는 무시할 수준이었다. 런처는 `-FrameReason` 으로
켜고 종료 후 호출 지점별 초당 횟수를 요약한다.

### `SERVO_MEDIA_SINK_PROF` — `Str`

비디오 appsink 콜백을 단계별로 재서 1초에 한 줄 찍는다(pace/diag/build/render/          notify). truthy: "1"/"true"/"on"(대소문자 무시). ★D3D11PROF 는 build_frame           '안' 만 본다★ - 45 영상 실측에서 그 안은 프레임당 1.56ms(0.047 코어/영상)로           스레드가 쓰는 0.76 코어의 6% 에 불과했다. 나머지 0.35 코어(디코드 천장 0.36 을           빼고도 남는 몫)는 콜백의 '바깥' 어딘가이고, 후보가 넷뿐이라 재면 바로 갈린다           - 특히 notify 는 프레임마다 IpcSender 전송이라 45 영상이면 초당 1350 회다.           프레임마다 불리는 핫패스라 기본 off.

```
SINKPROF id=3 fr=30 busy_ms=812.4 (pace=740.1 diag=1.2 build=47.0 render=9.8 notify=14.3)
```

★`pace` 는 CPU 가 아니다★ — `media_video_sink_pacing=thread` 에서 자는 시간이라 코어를
쓰지 않는다. 단계 합이 벽시계와 맞는지 보려고 함께 찍을 뿐이고, 코어 환산은 나머지
넷으로 한다. 런처는 `-SinkProf` 로 켠다.

### `SERVO_WEBGL_FANOUT_PROF` — `Str`

WebGL 멀티-GPU 팬아웃의 비용을 1 초에 한 줄씩 집계한다 — 생산자 측 WEBGLFANOUT(논리 명령 수 / 디바이스별 적용 수 / 컨텍스트 전환 수 / ANGLE 락 대기 / 전환·적용·직렬화 시간)과 소비자 측 WEBGLEXTIMG(painter 별 lock·unlock 횟수와 시간, take_surface / create_texture / destroy_texture 시간, 프런트 버퍼가 없던 횟수). truthy: "1" 또는 "true"(대소문자 무시). ★재실행 단가 가설은 반증됐다★ - 명령마다 컨텍스트를 갈아타는 것은 사실이지만(적용의 96.7%) 그 비용이 창의 0.4% 라, 재실행 루프를 뒤집는 작업(전환 4N→4)은 아무것도 사주지 않는다. 남은 물음은 소비자 측이다: WebGL 캔버스가 생기는 순간 타일 하나의 렌더가 0.23ms 에서 15ms 로 뛰는데, 그 시간이 전부 renderer.render() 안이면서 그리는 일도(draw_calls=2) 올리는 일도(upload_mb=0.0) 아니다. create_ms 가 크면 서피스를 기다리는 것이고, no_front_buffer 가 크면 기다리는 게 아니라 받을 게 없는 것이다. 프레임마다 불리는 핫패스라 기본 off.

```
WEBGLFANOUT window_ms=1001 swaps=13 ctx=1 dev=4 cmds=767 applies=2630 switches=2540   lock_wait_ms=0.4 switch_ms=21.9 apply_ms=27.4 serialize_ms=0.6
WEBGLEXTIMG painter=PainterId(1) window_ms=1000 locks=17 lock_ms=254.3 take_ms=0.1   create_ms=253.8 no_front_buffer=0 unlocks=17 unlock_ms=1.2 destroy_ms=1.1
```

읽는 법 — 생산자 측(`WEBGLFANOUT`): `switches` 가 `applies` 와 같으면 명령마다 컨텍스트를
갈아탄다는 뜻이다. ★그 자체는 사실로 확인됐지만 비용이 0.4% 라 뒤집을 값어치가 없다★
(2026-09-01 실측). `lock_wait_ms` 는 ANGLE 락이 디바이스당 하나가 된 뒤 46.7% 에서 0.0% 로
떨어졌다 — 다시 커진다면 같은 GPU 에 타일이 여럿인 배치를 의심할 것.

읽는 법 — 소비자 측(`WEBGLEXTIMG`): `create_ms` 가 `lock_ms` 의 대부분이면 painter 가
크로스-디바이스 서피스를 **기다리는** 것이고(keyed mutex), 그렇다면 생산자·소비자가 서로를
기다리는 결합을 끊는 것이 답이다. 반대로 `no_front_buffer` 가 크면 기다리는 게 아니라
**받을 게 없는** 것이므로 원인은 생산 쪽이다. 둘 다 작은데 타일 렌더가 여전히 느리면 이
경로가 아니다.

painter 쪽 `ANGLE_GL_LOCK` 대기는 `SERVO_LOG_PRESENT_CADENCE` 의 `angle_lock_ms` 로 본다.

### `SERVO_DCOMP_BIND_PROF` — `Str`

DComp Native 컴포지터가 프레임마다 쓰는 시간을 1 초에 한 줄로 쪼갠다 — end_frame 전체와 그 안의 gl().flush() / Commit(), 그리고 타일 왕복(BeginDraw / pbuffer 래핑 / EndDraw / teardown)과 begin_frame·end_frame 호출 수. truthy: "1" 또는 "true"(대소문자 무시). ★1 차 계측이 타일 왕복을 무죄로 밝혔다★(2026-09-01): 왕복이 bind 당 0.541ms, painter 당 초당 3.2ms 였고, 무엇보다 painter 가 초당 18 번 렌더하는데 bind 는 6 번뿐인데도 모든 렌더가 느렸다 — 비용이 타일별이 아니라는 뜻이다. 그래서 end_frame 을 잰다: bind 와 무관 하게 매 프레임 불리고, 이 컴포지터에서 무거운 일(GL flush, 스왑체인 Present, DWM Commit) 이 전부 그 안에 있다. end_frame_ms 가 프레임 예산을 채우면 그 안의 flush/commit 분해가 어느 쪽인지 가르고, 채우지 않으면 비용은 이 컴포지터가 아니라 WebRender 자신의 네이티브 컴포지터 경로에 있다. 프레임마다 불리는 핫패스라 기본 off.

```
DCOMPBIND window_ms=1001 frames=18/18 binds=6 full_tile=2 end_frame_ms=270.4   flush_ms=3.1 commit_ms=1.8 begin_ms=0.1 pbuffer_ms=3.0 enddraw_ms=0.1 teardown_ms=0.0
```

읽는 법: `frames` 는 `begin_frame/end_frame` 호출 수다 — 이 값이 painter 의 렌더 횟수와 맞는지
부터 본다(1 차 계측에서 bind 수와 렌더 수가 3 배 어긋난 것이 결정적 단서였다).
`end_frame_ms / frames` 가 타일당 렌더 비용에 근접하면 원인은 이 컴포지터 안이고, 그러면
`flush_ms` 와 `commit_ms` 가 어느 쪽인지 가른다. 반대로 `end_frame_ms` 가 작은데 렌더가 여전히
느리면 **이 컴포지터가 아니라 WebRender 자신의 네이티브 경로**가 비용이고, 그때는 DComp 를
켠 채로 고칠 여지가 없다는 뜻이라 판단이 달라진다.

타일 왕복 넷(`begin_ms`/`pbuffer_ms`/`enddraw_ms`/`teardown_ms`)은 이미 무죄로 밝혀졌지만
남겨 둔다 — 배치가 바뀌면(타일 크기, 페이지 구성) 다시 커질 수 있고, 빠졌다는 이유로 다시
의심하는 일을 막는다.

## wall CLI 플래그

pref 가 아니라 CLI 플래그다. 두 셸이 각자 파싱하지만 **검증과 해석은
`components/shared/paint/wall_args.rs` 한 곳**에서 한다.

| 플래그 | 뜻 |
|---|---|
| `--wall-layout <path>` | wall 모드를 켜고 레이아웃 JSON 을 읽는다. servoshell 에서는 선택(없으면 평범한 창), winit_wall 에서는 필수(표출 전용 셸이다) |
| `--wall-tile-index <n>` | 이 창이 그릴 타일. 준 경우에만 적용되고 기본은 0 |
| `--wall-all-tiles` | 타일마다 창을 하나씩 연다. 하나의 논리 WebView 를 공유한다 |

**아래 조합은 기동을 막는다**(예전에는 servoshell 이 `warn!` 로 넘기고 winit_wall 은 아예
검사하지 않았다):

- `--wall-tile-index` 를 `--wall-layout` 없이 준 경우 — 타일이라는 것이 없다
- `--wall-all-tiles` 를 `--wall-layout` 없이 준 경우 — 같은 이유
- `--wall-tile-index` 와 `--wall-all-tiles` 를 함께 준 경우 — 후자가 타일마다 창을 만들므로
  단일 인덱스가 쓰일 자리가 없다

경고가 아니라 오류인 이유는 셋 다 **사용자가 요청한 것이 일어나지 않는** 상태이기 때문이다.
GUI 실행에서 `warn!` 은 stderr 를 파일로 돌려야만 보이므로 사실상 아무도 못 본다.

### 셸별 고유 플래그 (의도적 차이)

| 플래그 | 셸 | 뜻 |
|---|---|---|
| `--capture <path.png>` / `--capture-sec <n>` | winit_wall | 주 타일 프레임버퍼를 PNG 로 덤프하고 종료(기본 3 초 후). winit_wall 은 캡처 하니스다 |

> **설계 문서의 오기 정정(2026-08-12).** 설계 문서 §8·§12 는 `--wall-gpu-direct-present`(servoshell)
> 와 `--backend`(winit_wall)를 셸별 고유 플래그로 적었지만, **둘 다 실제로는 존재하지 않는다**
> (`ports/` 와 `winit_wall/main.rs` 전량 검색으로 확인). WebGPU 직접 present 는 CLI 플래그가
> 아니라 **pref `dom_webgpu_gpu_direct`**(기본 false, `dom_webgpu_multigpu_fanout` 필요)이며
> 이 정리 작업의 19 개에는 포함되지 않는 기존 pref 다.

## 알려진 함정

**`gfx_dcomp_mode=surface` 는 헤드리스(`-f`)에서 성립하지 않는다.** GL error 500 으로 죽는다.
기존 동작이고 운용 스크립트는 전부 헤디드라 영향이 없지만, 헤드리스로 A/B 하려다 이것을
버그로 오인하기 쉽다.

**`winit_wall` 은 `overlapPx` 가드 밴드를 지원하지 않는다.** 확장 서피스 + 렌더 원점 이동 +
표출 시 크롭이 한 세트인데 winit_wall 은 창 서피스에 직접 그린다. `overlapPx: 0` 레이아웃만
쓰거나 servoshell 을 쓴다.

**`etc/multigpu/*.ps1` 에서 `$env:SERVO_*` 로 노브를 켜지 마라.** 셸이 pref 세트를 무조건
읽으므로 그 env 는 무효다. 이제는 조용히 무시되는 대신 기동이 막히지만, 새 노브를 추가할
때도 같은 함정이 있다 — 스크립트는 `--pref` 인자로 넘긴다.

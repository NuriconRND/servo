# 월 타일 병렬 렌더 — 설계안

작성 2026-09-01. 브랜치 `multigpu-wall-pacing`. 근거 로그 `log_webgpu/24~27`.

> ## ★결론(2026-09-03 갱신): 되살아났다. A 안으로 간다. C 는 건너뛴다.★
>
> 아래 보류 사유는 **더 이상 성립하지 않는다.** 그때는 병목이 DComp 의 `BeginDraw`/`EndDraw`
> 였고 `-DComp off` 로 피할 수 있었다. 그 뒤 두 번 더 파고들어 진짜 병목이 나왔다
> (`log_webgpu/57`~`61`):
>
> | | 값 |
> |---|---|
> | `create_surface_texture` 가 `paint` 에서 차지하는 비율 | **93%** (DComp on/off **양쪽**) |
> | 그 중 `AcquireSync(0, INFINITE)` | **74%**, 회당 **7.41ms** ← ★이 74% 의 정체를 오진했다, §5.6★ |
> | painter 4 개 합 | 초당 **900~935ms** |
> | `OpenSharedResource` + EGL pbuffer | 26% (회당 2.57ms) |
>
> ★즉 패스의 대부분이 **네 번의 독립적인 GPU 대기를 직렬로 쌓은 것**이다.★ §1 이 "스톨이
> 타일별이고 GPU별이라 본질적으로 겹칠 수 있다"고 가정했던 바로 그 모양이 실측으로 확인됐다 —
> 각 painter 는 **자기 타일 GPU** 가 그 캔버스 프레임을 끝내기를 기다린다. 대기는 병렬화가
> 가장 잘 듣는 종류다. `WALLPASS` 의 `parallel_ceiling` 도 지금 **3.0~3.8배**를 보고한다.
>
> **C 는 하지 않는다 — 실패가 이미 증명됐다.** C 는 스톨이 미룰 수 있는 백프레셔일 때만 듣는데,
> 그 대기는 WebRender 의 external-image `lock` 콜백 안, 즉 `renderer.render()` **안**에서
> 일어난다(`webrender_external_images.rs` → `create_surface_texture`). 제출/프레젠트로 쪼개도
> 제출 패스에서 그대로 막힌다. §5 는 "C 가 안 되면 그 실패가 A 를 확정한다"고 했고, 그 정보를
> 하루를 쓰지 않고 계측으로 얻었다.
>
> ★유혹적이지만 틀린 지름길: "준비 안 됐으면 직전 캔버스 프레임을 쓴다".★ 대기는 사라지지만
> **타일마다 다른 프레임을 표출하게 되어 이음매가 어긋난다** — 이 프로젝트의 존재 이유를
> 깨뜨린다. 배리어가 논리 프레임을 맞추는 이유와 같다.
>
> ★백버퍼 상한도 답이 아니다(사용자 3회 확인).★ 어떤 페이지가 뜰지는 서비스 제공자가 고를 수
> 있는 것이 아니므로 **최대 크기에서 성능이 나와야 한다.**
>
> 아래 원래 보류 사유는 기록으로 남긴다.
>
> ## (2026-09-01) 보류 사유 — 지금은 무효
>
> `log_webgpu/27` 의 짝 A/B(같은 장비·같은 페이지·연속 실행)가 원인을 다른 데서 찾았다.
> **DComp Native 컴포지터를 끄면 월이 24.9fps 에서 61.9fps 가 된다.**
>
> | | `-DComp off` | `-DComp surface` |
> |---|---|---|
> | 타일 fps | **61.9** | 24.9 |
> | passes/s | **60.0** | 16.8~21.4 |
> | pass_ms | **16.14** | 42~56 |
> | 평균 `render_ms` | **0.83ms** (14,847회) | 7.75ms (5,979회) |
> | 느린 프레임(>16ms) | **53** | 1,471 |
> | WebGL commands/s | **1,215** | 495 |
>
> 타일당 15ms 는 WebGL 도, 업로드도, 락도, external-image 콜백도 아니었다. WR 컴포지터의
> `bind()`/`unbind()` 가 부르는 **`IDCompositionSurface::BeginDraw`/`EndDraw`** 였다
> (`dcomp_compositor.rs:877,904,992,1006,1129`). 캔버스가 없을 때 빨랐던 이유도 이걸로 설명된다
> — picture cache 가 전부 유효하면 `bind()` 자체가 불리지 않는다.
>
> **직렬 타일 루프는 병목이 아니다.** `pass_ms=16.14` 가 16.67ms 예산 안에 들어가고 월이
> 클럭을 채운다. §4~§6 의 A안은 **하지 않는다.** 아래 내용은 (a) 나중에 진짜로 병렬화가
> 필요해질 때의 근거와 (b) 확인해 둔 제약 인벤토리로 남긴다 — 특히 §7 의 해소된 항목들은
> 다시 조사하지 말 것.
>
> **남은 것**: `-DComp off` 가 이 구성(`escape=off`)에서 옳다는 것이지 보편적이지 않다.
> DComp Native 는 비디오 escape 경로에서 실측 이득이 있었다(escape pref 를 준 실행에서
> draw_calls 10→1). 기본값을 바꿀지는 두 경로를 함께 놓고 판단해야 한다.

## 1. 무엇을 사는가

현재 한 월 패스는 **타일 비용의 합**이다. `render_all_tiles`(winit_wall `main.rs:386`)가
타일마다 `make_current → paint_target → present` 를 차례로 돈다.

```
per-tile=[18.21,15.21,15.29,15.26]  pass_ms=63.98  slowest=18.21  ceiling=3.51x
```

목표는 한 패스를 **최댓값**으로 만드는 것: 64ms → ~18ms → **~55 passes/s**.

★이 작업은 타일당 15ms 스톨을 없애지 않는다. 겹칠 뿐이다.★ 스톨 자체의 원인(ANGLE/드라이버가
공유 서피스가 걸린 프레임마다 고정 지연을 내는 것)은 별개의 축으로 남는다. 그래도 하는 이유는
그 스톨이 **타일별이고 GPU별이라 본질적으로 겹칠 수 있기** 때문이다.

## 2. 왜 지금인가 — 그리고 왜 전에 기각했었나

2026-08 말에 같은 작업을 **duty 57%, 천장 1.63배**로 기각했다. ★그 측정은 무효다★ — 당시엔
전역 `ANGLE_GL_LOCK` 이 `Painter::render()` 전체를 감싸고 있어서, 스레드를 넷으로 늘려도
그 락에서 다시 줄을 섰다. 락이 디바이스당으로 바뀐 뒤(`5cc95bd09ee`) 같은 계측이
**3.5배**를 보고한다.

그리고 스톨의 성격이 밝혀졌다(`log_webgpu/26`):

| 후보 | 실측 | 판정 |
|---|---|---|
| 업로드 | `upload_mb=0.00 upload_ms=0.00` | 아님 |
| ANGLE 락 | `angle_lock_ms=0.06` | 아님 |
| WebRender update | `wr_update_ms=0.01` | 아님 |
| external-image 콜백 | 타일당 7.8 ms/s | 아님 |
| **`wr_render_ms`** | **30.66 (드로우 3번)** | **여기** |

픽셀에 비례하지 않는다(`?scale=0.5` 가 0% 변화). 즉 fill 도 대역폭도 아닌 **고정 스톨**이고,
CPU 작업이 아니므로 겹칠 수 있다.

## 3. 제약 인벤토리 (코드 실측)

병렬화를 막는 것은 성능이 아니라 **소유권**이다.

1. **surfman `Device` 는 스레드 로컬이다** — `connection.rs:102` 의 계약이 "Device handles are
   local to a single thread". 생성 스레드일 필요는 없지만 **한 번에 한 스레드**이므로, 타일
   컨텍스트는 한 스레드가 **생성부터 소멸까지** 소유해야 한다.
2. `TileWindow.rendering_context: Rc<dyn RenderingContext>` (`tile.rs:26`) — `!Send`.
3. `Painter`(`painter.rs:191`)가 붙들고 있는 것: `Rc<dyn RenderingContext>`,
   `Rc<BaseRefreshDriver>`, `Rc<AnimationRefreshDriverObserver>`, `Rc<dyn gleam::gl::Gl>`,
   `Option<Rc<RefCell<DCompNativeCompositor>>>`, `webrender::Renderer`, 그리고 `Cell`/`RefCell`
   필드 20여 개.
4. `Paint` 가 `painters: Vec<Rc<RefCell<Painter>>>` 와 **월 프레임 배리어 상태**를 `Cell` 로 들고
   있다. 배리어는 지금 단일 스레드 가정 위에 서 있다.
5. `WebView::paint_target()` → `Paint::render_paint_target()` → `Painter::render()` 는
   **호출 스레드에서 동기 실행**된다. 별도 페인트 스레드는 없다(이름과 달리).
6. winit `Window` 의 생성과 이벤트는 메인 스레드. `present()` 는 DXGI 라 다른 스레드에서도
   가능하지만 리사이즈·DComp 재구축과 조율이 필요하다.

즉 A안은 "루프에 `par_iter` 를 붙이는" 종류가 아니라 **소유 모델을 바꾸는** 작업이다.

## 4. 후보

### A. 타일당 전용 스레드 (전체 소유권 이동)

타일 스레드가 자기 `RenderingContext` + `Painter` + `webrender::Renderer` 를 **만들고 죽을 때까지
소유**한다. 메인 스레드는 채널로 "논리 프레임 N 그려라" 를 보내고 완료를 기다린다.

- 장점: 진짜 겹침. surfman 의 스레드 로컬 계약과 정확히 맞는다. 직렬 루프가 사라진다.
- 단점: 가장 큰 변경. §3 의 `Rc` 들을 스레드별 인스턴스로 나누거나 `Arc` 로 올려야 하고,
  refresh driver·배리어·`Paint` 레지스트리가 전부 단일 스레드 가정 위에 있다.

### B. 렌더만 떼어내기

`renderer.update()+render()` 만 워커로 보내고 `Painter` 는 메인에 남긴다.

- **성립하지 않는다.** GL 컨텍스트가 렌더하는 스레드에서 current 여야 하므로 컨텍스트가 그쪽으로
  가야 하고, 그러면 `Painter` 의 절반이 따라간다. 결국 A로 수렴한다. 기록만 남긴다.

### C. 스레드 없이 겹치기

루프를 **제출 패스 / 프레젠트 패스**로 쪼개고, 타일 스왑체인 버퍼 수를 늘린다.

```
for tile { make_current; paint_target }   // 4개 제출
for tile { make_current; present }        // 4개 프레젠트
```

- 장점: 소유권을 건드리지 않는다. 하루면 된다.
- 단점: **스톨이 백프레셔성일 때만 듣는다.** 드라이버가 `render()` 안에서 동기 대기를 하면
  제출 패스에서 그대로 막히고 아무것도 나아지지 않는다.

## 5. 권장 순서

> ★2026-09-03 갱신: 이 절은 무효다. **C 는 건너뛰고 A 로 간다.**★ 판정 기준을 미리 고정해
> 둔 것은 옳았고, 그 기준에 답하는 데 하루를 쓰는 대신 계측이 답했다 — 대기가
> `renderer.render()` 안에 있으므로 제출/프레젠트 분리는 원리상 듣지 않는다(위 머리말).

**C 를 먼저 하루, 안 되면 A.**

A 는 주 단위 작업이고 C 는 하루다. C 가 되면 A 는 필요 없다. C 가 안 되면 그 실패 자체가
"스톨은 미룰 수 있는 백프레셔가 아니라 동기 대기"라는 정보이고, 그때 A 가 유일한 답으로 확정된다.

**C 의 판정 기준(미리 고정한다)**: `WALLPASS` 의 `pass_ms` 가 `serial_sum` 아래로 내려가고
`passes/s` 가 오르면 성공. 두 패스로 쪼갠 뒤에도 `pass_ms ≈ serial_sum` 이면 실패이고 즉시 A 로
간다. ★"조금 나아졌다"는 성공이 아니다★ — 상금이 3.5배인데 1.2배가 나오면 그건 노이즈다.

## ★5.5 결과 (2026-09-03, `log_webgpu/68`~`69`)★

팬아웃까지 완료. pref `gfx_wall_parallel_tiles`(런처 `-ParallelTiles`, 기본 off).

| | 직렬 | 팬아웃 |
|---|---|---|
| passes/s | 21.8 | **45.0** |
| pass_ms | 45.30 | **21.34** |
| 논리 프레임(60초) | 1,323 | **2,790** |

이음매 sync 유지, `missing swap chain` 0. **순 이득 2.1 배.**

### ★천장은 이미 받았다 — 남은 손실은 겹침이 아니다★

`serial_sum 73.4 / pass_ms 21.3` = **실제 병렬도 3.5 배**. §1 이 말한 천장 그대로다.
그런데 타일 하나가 **직렬 11.3ms → 병렬 18ms** 로 1.6 배 비싸졌고, 3.5 ÷ 1.6 = 2.1 이 그대로
관측된 이득이다. 즉 **더 겹칠 것은 없다.**

타일별 값이 `18.82, 18.24, 18.37, 18.30` 으로 **고르다** — primary 만 느린 것이 아니다
(처음엔 그렇게 보였는데, 그건 스레드 타일이 `tile_ms` 에 0 으로 기록되던 계측 구멍 때문이었다.
★팬아웃을 넣으면 셸 루프가 그 타일을 건너뛰므로 타일별 계측이 죽는다★ — 스레드가 자기 시간을
재서 조인이 돌려주도록 고쳤다, `90f834fff4c`).

WebGL 스레드는 `swaps/s 45.5` 로 월을 따라가고 **busy 5.2%** — 여전히 생산자는 무죄다.

### 다음 축은 병렬화가 아니다

남은 것은 [[webgl-tile-create-texture-bottleneck]] 의 그 비용이다 — `create_surface_texture`
가 `paint` 의 93%, 그 중 `AcquireSync` 가 74%(회당 7.41ms). 넷이 **동시에** 그 대기에 들어가면
한 개뿐인 WebGL 스레드가 백엔드 넷의 생산을 순차로 내주므로 줄이 생기고, 그것이 11.3→18ms 다.
그러므로 다음은 타일을 더 겹치는 것이 아니라 **타일 하나를 싸게 만드는 것**이다.

> ★위 문단의 진단 절반은 틀렸다 — §5.6 을 보라.★ "타일 하나를 싸게" 라는 방향은 맞았지만,
> 그 7.41ms 가 **WebGL 스레드를 기다리는 시간**이라는 설명은 반증됐다. 실제로는 매 프레임
> 공유 텍스처를 다시 여는 것이 원인이었고, 생산자는 여기서도 무죄였다.

## ★5.6 import 캐시 — 여기서 끝났다 (2026-09-03, `log_webgpu/70`, 커밋 `6f0f1f32be3`)★

`create_surface_texture` 가 들여온 것(로컬 D3D11 텍스처 · EGL pbuffer · 키드 뮤텍스 ·
GL 텍스처)을 `share_handle` 로 캐시한다. 매 프레임 만들고 부수던 이유는 무효화가 아니라
**소유권 반납**이다 — `SurfaceTexture` 가 `Surface` 를 삼키고, 생산자가 그 버퍼에 다시
그리려면 텍스처를 부수는 것 말고 `Surface` 를 돌려받을 길이 없다. 스왑체인은 서피스 몇 장을
돌려 쓰므로 같은 핸들이 곧 다시 온다.

| | 직렬 | 팬아웃 | **+ 캐시** |
|---|---|---|---|
| passes/s | 21.8 | 45.0 | **60.0 (표시 상한)** |
| pass_ms | 45.30 | 21.34 | **1.47** |
| 타일당 | 11.3 | 18.0 | **1.21** |
| 타일 fps | — | — | **P1~P4 전부 63.4** |
| import 회당 | 10.01ms | — | **0.08ms** |

적중률 **15,065 / 15,077 (100%)**. 팬아웃 대비 **14.5 배**, 직렬 대비 **30.8 배**.
★이제 월은 페인트에 묶여 있지 않다 — 1.47ms 는 초당 680 패스 능력인데 60 만 돈다.★

### ★예측이 틀렸다 — 26% 라고 했는데 실제는 자릿수가 달랐다★

캐시가 걷어낼 것은 `OpenSharedResource` + pbuffer + GL 텍스처 = **26%** 이고, 나머지 74% 인
`AcquireSync` 는 **대기라서 캐시로 줄지 않는다**고 적었다. 그 74% 가 회당
**7.41ms → 0.04ms (185 배)** 로 사라졌다.

★그 대기는 생산자 경합이 아니라 **재-import 가 스스로 만든 것**이었다.★ 매 프레임
`OpenSharedResource` 로 공유 텍스처를 새로 열면 소비자 디바이스에는 매번 새 리소스가 생기고,
갓 연 리소스의 첫 acquire 는 정상 상태의 펜스 확인 경로를 타지 못한다. 열기를 캐시하니
acquire 가 그 버퍼의 펜스 하나를 보는 값으로 떨어졌다.

★일반화할 교훈: "이건 대기다 → 못 줄인다" 는 추론은 **무엇을 기다리는지 확인하기 전까지
성립하지 않는다.**★ 여기서는 우리 코드가 만든 대기였다. 런처의 판정 문구도 그렇게 고쳤다
(적중률을 퍼센트보다 먼저 읽는다).

### 남은 것 — 이 축에서는 없다

`SURFIMPORT` 총합이 회당 0.08ms 다. 그 안에서 `AcquireSync` 가 49.6%, `BindTexImage` 가
44.5% 라는 비율은 **이 규모에서는 노이즈**다. `WEBGLEXTIMG` 의 unlock/destroy 가 타일당
14~18ms/s 로 lock 보다 크지만 콜백 전체가 타일당 21.1ms/s, 즉 **2%** 다.
이 블록은 이제 사냥이 아니라 **회귀 감시**다 — 잡아야 할 것은 `reused%` 가 떨어지는 것
(리사이즈 폭주, 또는 생산자가 매 프레임 새 share handle 을 주는 경우)이고, 그러면 옛 비용이
통째로 돌아온다.

## ★5.7 비디오 그리드 — 45 영상 61.9fps (2026-09-03, `log_webgpu/71`~`73`)★

병렬 타일은 비디오에도 듣는다. ★단 import 캐시(§5.6)는 비디오에 닿지 않는다★ —
`create_surface_texture` 의 실사용 호출자는 `webrender_external_images.rs:140` 하나뿐이고
그건 WebGL/캔버스 경로다. 비디오는 `media-thread/lib.rs` 의 `d3d11_wrap_cache` 로 애초에
"정상 상태에서 프레임당 재래핑 0" 이다.

45×FHD30(9열×5행), `-DComp on`, 같은 빌드:

| | fps | pass_ms | paint | outside |
|---|---|---|---|---|
| 직렬 | 34.6 | 24.21 | 23.72 | 0.38 |
| 병렬 | 42.1 | 10.37 | 4.84 | 5.53 |
| **병렬 + CPU 플래그 2 개** | **61.9** | **4.63** | 1.79 | 2.84 |

### ★이 결과의 전제: `-PipelineMode uridecodebin3` + `-SinkPacing thread`★

둘 다 기본 off 이고 런처 주석에 cores/video 실측이 붙어 있다 — playbin3 의 `vqueue` 가
원본 3.1MB 프레임을 매 장 스레드 경계 너머로 나르고(0.729 → 0.284),
`GstSystemClock::obtain()` 은 프로세스당 싱글턴이라 45 개 sink 가 매 프레임 같은 객체를
기다린다(0.795 → 0.284).

★굶어도 증상이 벽 쪽에 나온다★ — 패스는 멀쩡해 보이고 타일만 느리다. 그래서 하루치
타일-측 A/B(`log_webgpu/71`~`72`)를 통째로 굶은 구성 위에서 쟀고 **그 수치는 전부 무효다.**
런처가 이제 실행 전에 경고한다(타일 16 개 이상 + 둘 중 하나라도 없음).

### ★`-DcompParallelCommit` 는 병렬 타일과 같이 쓰면 손해★

`outside` 5.53 → **8.84ms**, fps 42.1 → **30.8**.

`flush_deferred_dcomp_commits`(`paint.rs:2780`)가 `with_painter()` 를 쓰는데, 병렬 경로의
painter 는 `PainterHost::Threaded` 라 ★그 호출 하나가 그 스레드로 보내고 답을 기다리는
왕복★이다. PC 경로는 포인터를 **가져오려고만** 4 번 왕복한 뒤 `std::thread::scope` 로
**패스마다 OS 스레드 4 개를 새로 만든다.** PC 없으면 같은 4 번 왕복인데 Commit 을 **이미
존재하는 그 페인터 스레드 안에서** 한다.

★플래그의 전제가 병렬화로 무효가 됐다★ — 근거였던 "Commit 4×2.44 = 9.8ms, 메인 스레드
직렬" 이 병렬 실행에서는 `Commit 5.7 ms/s ÷ 39.7 frames/s` = **회당 0.14ms** 다.
겹칠 것이 없고 스레드 생성비만 남는다.

### ★`outside` 는 병렬 경로에서 Commit 이 아니다★

라벨("deferred DComp Commit flush + the loop itself")을 읽고 Commit 이라고 단정했는데,
**같은 화면의 DCOMPBIND 가 0.14ms 라고 말하고 있었다** — §5.6 의 AcquireSync 와 같은 실수다.

이유는 코드에 있다(`winit_wall/main.rs:579`): ★스레드 타일의 시간은 `tile_ms` 에만 넣고
`split.paint_ms` 에는 더하지 않는다★(더하면 `outside` 가 음수가 된다). 따라서 병렬 경로의
`outside` = **느린 타일을 기다리는 join 시간**이다. 산수가 닫힌다:

| 실행 | paint | 느린 타일 | 차 | outside |
|---|---|---|---|---|
| 병렬 | 4.84 | 10.21 | 5.37 | 5.53 |
| +PC | 5.35 | 12.99 | 7.64 | 8.84 |
| +rotate | 5.14 | 11.35 | 6.21 | 5.88 |

즉 ★`pass_ms ≈ 가장 느린 타일`, 나머지는 전부 장부★다.

### 느린 타일은 painter 가 아니라 display 를 따라간다

| | P1 | P2 | P3 | P4 |
|---|---|---|---|---|
| 정상 배치 | 5.24 | 5.31 | 5.39 | **9.78** |
| `reversed.json` | **12.74** | 6.22 | 5.97 | 5.67 |

★`-RotateTileOrder` 로는 이걸 구분할 수 없다★ — 회전은 **한 패스 안의 순회 순서**만 바꿀
뿐, 어떤 painter 가 어떤 디스플레이를 모는지는 안 바꾼다. `reversed.json` 도 완전하지
않다(그 파일 주석: *"every display keeps its own rect, so it cannot tell content apart from
hardware"*). 하드웨어와 콘텐츠를 가르는 도구는 **`wall_layout.multigpu.rectswap.json`** 이다.

지금은 쫓지 않는다 — CPU 플래그를 넣으면 느린 타일이 4.16ms 이고 예산은 16.67ms 라
비대칭이 fps 를 잡지 않는다.

### 부수 관찰: `-VideoEscape external` 의 판정이 타일 수에 따라 뒤집힌다

36 영상은 escape 가 맞지만(61.1fps), 45 영상은 escape 30.5 대 escape 없음 42.1 이다.
DCOMPBIND 가 직접 말한다: `external video 266.3 ms/s`, 101,445 adds,
`visuals/frame 15.1`(escape 없으면 1.2) — ★영상마다 visual 을 만들므로 45 개면 관리 비용이
아끼는 것을 이긴다.★ 단 이 비교도 CPU 플래그 없이 잰 것이라, 재비교가 필요하다.

## 6. A 설계 (C 실패 시)

### 6.1 소유 모델

```
메인 스레드                          타일 스레드 N (타일당 1개)
  winit 이벤트 루프                    RenderingContext  (생성~소멸 전 구간 소유)
  Servo / WebView / 스크립트           Painter
  프레젠테이션 클럭                    webrender::Renderer
  타일 창 핸들(HWND)                   ANGLE Device/Context
        |                                     ^
        |  RenderTile { logical_frame_id }     |
        +------------------------------------->+
        |  TileDone { local_frame_id, ms }     |
        +<-------------------------------------+
```

창은 메인 스레드가 만들고 **HWND 만** 타일 스레드로 넘긴다(winit 창 객체 자체를 옮기지 않는다).
컨텍스트는 타일 스레드가 그 HWND 로 직접 만든다 — surfman 의 스레드 로컬 계약을 지키는 유일한 길.

### 6.2 단계 (각 단계가 되돌릴 수 있는 지점이다)

1. ~~**`RenderingContext` + WebRender 인스턴스를 타일 스레드에서 생성**~~ **★통과
   (2026-09-03, `log_webgpu/62`).★** WebRender 쪽은 §7-3 으로 정적 확인이 끝나 있었고, 이
   단계가 실제로 걸던 것은 **surfman/ANGLE 이 다른 스레드에서 컨텍스트를 만들고 current 로
   삼는가**였다. 네 타일 전부 자기 워커 스레드에서 만들고 current 로 삼았으며, 각자 자기
   GPU 에 붙었다(gpu 0/1/3/2 = display 0/1/2/3):

   ```
   TILESPIKE tile 0: OK on worker ThreadId(2) -- display=Some(0) gpu=Some(0) create_ms=272.6
   TILESPIKE tile 1: OK on worker ThreadId(3) -- display=Some(1) gpu=Some(1) create_ms=218.4
   TILESPIKE tile 2: OK on worker ThreadId(4) -- display=Some(2) gpu=Some(3) create_ms=218.3
   TILESPIKE tile 3: OK on worker ThreadId(5) -- display=Some(3) gpu=Some(2) create_ms=305.2
   TILESPIKE VERDICT: PASS
   ```

   ★생성만 된 것과 구분하려고 GL 에 직접 물었다★ — 각 스레드가
   `ANGLE (AMD Radeon RX 580 ... D3D11)` 을 응답했다. 재현: `-TileThreadSpike`
   (`tile::plan_and_run_tile_thread_spike`). 렌더하지 않고 즉시 종료하며 판정을 종료 코드로도
   낸다.

   부수 산물: **창 생성과 컨텍스트 생성이 갈렸다**(`plan_tile_windows` → `TilePlan` →
   `bind_context`). A 안이 요구하는 바로 그 분리다 — 창은 메인 스레드 전용, 컨텍스트는 타일
   스레드 전용, 그 사이를 건너는 것은 HWND 정수 하나뿐. 스파이크를 신뢰할 수 있게 만든 것도
   이 분리다: 컨텍스트가 이미 붙은 HWND 에 두 번째를 만들면 `-DComp on` 에서 HWND 당 DComp
   타깃이 하나뿐이라 실패하고, 그 실패가 "스레드 탓" 으로 오독된다.
2. **`Painter` 를 타일 스레드로.** 진행 중.
   - **완료(2026-09-03, `8ecab58122a`)**: `Painter::new` 가 `&Paint` 대신 `PainterInputs`
     를 받는다 — `Paint` 에서 읽던 여덟 가지를 `Send` 값 묶음으로 떼어냈다. `Paint` 는 `Rc`
     투성이라 참조를 스레드로 건넬 수 없고, `Renderer`/`RenderingContext` 는 `!Send` 라
     만들어 놓고 옮길 수도 없다. 그래서 **타일 스레드가 직접 만들 수 있는 재료**만 남긴 것이
     이 단계다. `Paint::assert_painter_inputs_are_send` 가 컴파일 타임에 강제한다(★가드가
     실제로 잡는지 `Rc<()>` 를 끼워 넣어 확인했다★).
   - `refresh_driver` / `animation_refresh_driver_observer` 는 §7-1 대로 **이미 타일별**이라
     할 일이 없다.
   - **남은 것**: `Painter` 를 실제로 스레드에 올리고 채널로 구동하기(3 단계와 맞물린다).
3. **채널 프로토콜.** ★배리어는 옮기지 않는다 — 메인이 계속 소유한다.★ 원래 이 단계에
   "배리어를 스레드 안전 구조로" 라고 적었으나, 실제로 `WallFrameCoordinator` 를 만지는 곳은
   `paint.rs` 세 곳뿐이고 전부 메인이다(`webview_has_active_frame`,
   `expire_keep_previous_before`, `register_frame`). §6.1 대로 **타일은 `TileDone` 만 돌려주고
   메인이 판정**하면 `Mutex` 로 바꿀 이유가 없다. 실패 주입 env 3종도 그대로 동작한다.
   `paint_target_keep_previous_logical_frame`(셸이 "이 타일 건너뛸까" 를 묻는 것)도 메인이
   **디스패치 전에** 판정해 건너뛸 타일은 아예 보내지 않는 쪽이 맞다.

   ### 3.1 실제 규모 (2026-09-03 실측)

   `Paint` 가 `Painter` 를 **변형**하는 지점 48 곳, **읽는** 지점 13 곳. 그런데 48 중 22 는
   이미 헬퍼 두 개를 거친다(`for_each_webview_painter_mut` 17,
   `for_each_source_painter_target_mut` 5) — 그 22 는 **2 곳만 고치면 된다.**
   흩어진 것은 23 곳이고, 성격이 둘로 갈린다:

   | 성격 | 곳 | 어떻게 |
   |---|---|---|
   | **매 프레임** — `render_paint_target`, `render`, `handle_new_display_list`, `issue_wall_frame_request`, 위 헬퍼 2 | ~6 | 팬아웃/조인. **여기가 이득이 나오는 자리다** |
   | **제어·설정** — `add_webview`/`remove_webview`, `show`/`hide`, `set_page_zoom`, `set_viewport_details`, `set_hidpi_scale_factor`, `resize_rendering_context`, `add`/`update_webview_paint_target`, 입력 이벤트 3, `device_pixels_per_page_pixel`, `handle_browser_message` 안의 13 | ~19 | **블로킹 왕복**으로 충분하다. 프레임당 도는 것이 아니라 성능에 무관하고, 동기 의미론이 그대로 보존된다 |

   ★이 분류가 위험도를 결정한다★ — 제어 경로를 블로킹으로 두면 동작이 바뀌지 않으므로,
   틀릴 수 있는 곳이 매 프레임 경로 여섯 곳으로 줄어든다.

   ### 3.2 순서

   1. ~~`Painter` 접근을 단일 API 뒤로 모은다~~ **완료(2026-09-03, `f6f1a534ab9`).**
      `Paint::with_painter` / `with_painter_mut`(+ primary 변형) 둘뿐이고, `Ref`/`RefMut` 를
      돌려주던 접근자 넷은 **삭제했다.** ★painter 빌림을 호출 너머로 들고 갈 방법이 없어진
      것이 요점★ — 그런 코드는 오늘은 잘 돌지만 painter 가 스레드로 가는 순간 조용히
      깨진다. 지금은 전부 인라인이라 동작 불변이다.

      손이 더 간 곳 둘: `remove_webview` 는 `remove_painter` 전에 `drop(painter)` 로 빌림을
      끊고 있었는데 클로저가 끝나며 같은 일을 한다. 이미지/폰트 키 배치는 빌림 하나를 쥔 채
      루프를 돌았는데, **키 하나마다 왕복하는 형태를 남겨 두지 않으려고** 배치 전체를 한 번의
      `with_painter` 안에서 만들도록 바꿨다.
   2. 그 API 의 구현만 채널 왕복으로 바꾼다 — 제어 경로는 블로킹, 매 프레임 경로는 팬아웃/조인.

      ### ★2026-09-03: 컴파일러에게 미리 물어본 결과★

      `with_painter*` 의 클로저에 `Send + 'static` 을 걸어 보면(4 줄 변경) **무엇이 스레드를
      못 건너는지 전부 나온다.** 실측: 서로 다른 호출 지점 **33 곳, `E0277` 31 건.**
      추측하지 말고 이 실험을 다시 돌려 목록을 뽑을 것 — 되돌리기도 4 줄이다.

      갈래는 둘이고, **일의 성격이 완전히 다르다**:

      | 갈래 | 정체 | 성격 |
      |---|---|---|
      | **A. 클로저가 `&self` 를 캡처** | `RefCell<HashMap<..>>`, `Cell<u64>`, `Rc<RefCell<Painter>>`, `Rc<Cell<ShutdownState>>`, `RefCell<WallFrameCoordinator>`, `OnceCell<..>`, `RefCell<MessageReader>` … 전부 `Paint` 필드다. 클로저가 `self.time_profiler_chan` 같은 것 **하나**를 쓰면 `Paint` 전체가 딸려 들어온다 | 기계적. **메인에서 계산해 소유 데이터만 넘기는** 형태로 재구성. 스레드에서는 클로저가 `Paint` 를 만질 수 **없으므로** 어차피 해야 하는 일이다 |
      | **B. 페이로드 자체가 비-Send** | `dyn WebViewTrait`(→ `add_webview`), 스크린샷 콜백 `dyn FnOnce(Result<ImageBuffer, ..>)`, 브로드캐스트 헬퍼의 `impl FnMut(&mut Painter)` | ★설계 결정★. 앞의 둘은 **libservo 공개 트레이트 경계에 `Send` 를 요구**하므로 servoshell·예제까지 파급된다. 셋째는 `Fn + Send + Clone` 으로 바꿔 painter 마다 클론하면 된다 |

      ★B 를 먼저 정해야 A 를 끝낼 수 있다★ — A 를 다 고쳐도 B 가 남으면 경계를 켤 수 없고,
      경계를 못 켜면 A 가 실제로 스레드를 건널 수 있는지 검증할 방법이 없다.

      ### ★2026-09-03 저녁: 둘 다 완료. 경계는 이제 상시 켜져 있다★

      `9a2f94591d3`(B) → `1d687661d39`(A). **더 이상 실험이 아니라 영구 보증이다** — `Paint` 가
      painter 에 닿는 모든 길이 스레드를 건널 수 있음을 컴파일러가 지킨다.

      **B (구조적)**: `dyn WebViewTrait` 는 트레이트에 `Send` 를 요구할 필요가 없었다. 실제로
      쓰던 메서드가 `set_animating` 하나였고(§3.2 위 표) 반환값도 없어서, `PaintProxy` 로 통보만
      보내고 트레이트 객체는 `Paint` 가 메인에서 계속 소유한다. **libservo 공개 API 무변경.**

      **A (기계적, 52 곳)**: 세 모양이었다.
      - 대부분은 `move` 만 붙이면 됐다(빌리던 것이 대개 `Copy` 인 id·상태).
      - **루프 안 4 곳**은 `move` 가 첫 회에 값을 가져가 버려서, 반복마다 미리 클론한다.
        ★클론 횟수는 그대로다★ — 클로저 안에 있던 것을 밖으로 끌어냈을 뿐이다.
      - **2 곳은 `self.time_profiler_chan` 하나 때문에 `&self` 를 통째로** 캡처하고 있었다.
        그러면 `Paint` 의 모든 `Cell`/`RefCell` 이 딸려 온다. 채널만 클론해서 넘긴다.
      - `show_webview`/`hide_webview` 는 브로드캐스트를 그만뒀다. 바깥 지역변수를 갱신해
        결과를 모으던 형태인데, 클로저가 건너가면 지역변수는 남는다. **각 호출의 반환값을
        받는** 모양으로 바꿨다(팬아웃/조인이 요구할 모양이기도 하다).

      ★예외 하나는 이름으로 남겼다★: `with_primary_painter_not_thread_ready`, 호출자는
      `request_screenshot` 하나. 스크린샷 기계가 임베더 콜백과
      `ScreenshotRequestPhase::WaitingOnPipelineDisplayLists(Rc<..>)` 를 들고 있어 별도
      하위 과제다. **경계를 통째로 내리는 대신 예외에 이름을 붙였다** — 이 함수를 쓰는 곳이
      늘면 그만큼 A 안에서 멀어진다는 뜻이고, 이름이 그것을 말한다.
   3. ~~`painters: Vec<Rc<RefCell<Painter>>>` 를 스레드 핸들 목록으로 교체.~~
      **완료(2026-09-03).** `PainterHost` 가 `Inline` / `Threaded` 로 갈리고, 스레드 경로는
      pref `gfx_wall_parallel_tiles`(런처 `-ParallelTiles`, 기본 off) 뒤에 있다.

      ### ★실기 확인 (`log_webgpu/67`)★

      네 타일 모두 캔버스 출력, 격리 디바이스 8 개(painter 4 + WebGL 백엔드 4),
      `missing swap chain` 0, 논리 프레임 **21.4/s** — 직렬 기준선 22.1/s 와 같다.
      ★아직 팬아웃이 아니라 타일당 블로킹 왕복이므로 이득이 없는 것이 정상이다.★
      스레드화 자체가 안전하고 비용이 없다는 것이 이것으로 확인됐다.

      ### 여기까지 오며 걸린 함정 (같은 자리에 다시 빠지지 말 것)

      **★`create_webgl_context` 는 surfman 상세가 없는 painter 를 조용히 걸러낸다★**
      (`webgl_thread.rs`). 그 상세는 `Paint::register_rendering_context` 만 채우는데 스레드
      타일은 그 경로를 지나지 않는다. 넷이 들어가 하나만 남아 세 타일이 캔버스를 못 받았고,
      화면에는 "그 타일만 캔버스 없음" 으로만 보였다. ★그 침묵 때문에 원인을 두 번 잘못
      짚었다★(`log_webgpu/64`~`66`: 처음엔 등록 순서, 다음엔 로드 시점). 지금은 타일 스레드가
      자기 상세를 `Painter` 생성 전에 등록하고, 필터도 걸러낼 때 말한다.

      교훈: **팬아웃 대상 목록은 양쪽 끝에서 찍어라.** `PAINTTARGETS`(Paint 가 답한 것)와
      `WEBGLTARGETS`(WebGL 스레드가 받은 것)를 나란히 보고 나서야 "목록은 처음부터 옳았다"가
      드러났고, 그제야 아래를 볼 수 있었다.
4. **`present()` 를 타일 스레드로** — 리사이즈/DComp 재구축과의 조율이 여기 있다.
5. **직렬 경로 제거** — `render_all_tiles` 를 fan-out/join 으로 대체.

### 6.3 유지해야 하는 것

- 월 프레임 배리어의 의미론(16ms 데드라인, keep-previous 정책, `logical_frame_id`)
- 프레젠테이션 클럭(`about_to_wait` + `ControlFlow::WaitUntil`) — ★틱은 메인 스레드에 남는다★
- `--capture`, `--wall-tile-index` 단일 타일 모드
- servoshell 은 건드리지 않는다(별도 셸, 가드밴드 담당)

## 7. 미해결 (설계 확정 전에 답해야 함)

1. ~~**`BaseRefreshDriver` 를 타일별로 나눌 수 있나?**~~ **해소(2026-09-03). ★이미 나뉘어
   있다.★** 전제가 틀렸다 — `Paint` 는 하나도 들고 있지 않고, `Painter::new`
   (`painter.rs:509`)가 painter 마다 자기 `BaseRefreshDriver` 와 자기
   `AnimationRefreshDriverObserver` 를 만든다. 같은 날 스톨 조사에서 `ANIMTICK start` 가
   4 줄 나오고 `stop` 이 없던 것이 그 증거였다(각 observer 가 자기 `animating` Cell 을 가진다).
   메인에 남기고 메시지로 통신할 필요가 없다.
2. ~~**`dcomp_shared` 의 "shared" 가 타일 간 공유인가.**~~ **해소(2026-09-01). 공유가 아니다.**
   "shared" 는 **WebRender 와 Painter 사이**를 뜻한다 — WR 이 `Box<SharedDComp>`
   (`CompositorConfig::Native`)로 소유하고 painter 가 같은 인스턴스의 `Rc` 클론으로
   `present_external_only()` 를 직접 부른다(WR 프레임 빌드를 우회하는 fast-path). 둘 다 A 에서
   같은 타일 스레드에 산다.
   - `maybe_create`(`dcomp_compositor.rs:1299`)는 **호출마다 새로 만든다.** 캐시도 전역도 없고
     호출 지점이 `Painter::new`(`painter.rs:524`) **하나뿐**이라 painter 당 정확히 하나다.
   - 그 인스턴스는 **그 painter 자신의 HWND 와 자신의 ANGLE D3D11 디바이스**에 묶인다. 마침
     그 디바이스 포인터가 지금 ANGLE 락의 키이므로(`5cc95bd09ee`), DComp 경로도 이미
     타일별로 갈려 있다.
   - `dcomp_compositor.rs` 의 전역은 전부 `OnceLock` **설정 캐시**(버퍼 수, stable swapchain,
     unbind 동작, 프로파일 게이트, storage mode)다. 최초 1회 읽고 불변이며 `OnceLock` 은
     `Sync` 라 여러 스레드에서 안전하다. 타일별 상태를 든 전역은 없다.

   A 에 대한 함의: 전제가 유지될 뿐 아니라 유리하다. 타일 스레드가 HWND 로 컨텍스트를 만들면
   DComp 컴포지터도 그 스레드에서 그 컨텍스트로부터 자연히 만들어진다.
3. ~~**`webrender::Renderer` 가 다른 스레드에서 생성·구동 가능한가.**~~ **해소(2026-09-01).**
   ★생성은 된다. 옮기는 것은 안 된다.★ A안은 옮기지 않고 타일 스레드가 직접 만드므로 성립한다.
   근거:
   - `Renderer` 는 `!Send` 다 — `shaders: Rc<RefCell<Shaders>>`(`renderer/mod.rs:833`)와
     `device: Device` 안의 `Rc<dyn gl::Gl>`. 그래서 **이미 만들어진 것을 스레드로 넘길 수는
     없다.**
   - ★webrender 에 `thread_local!` 이 하나도 없다★ — 스레드별 초기화 요구가 없다. 가장 큰
     위험이었는데 0 이다.
   - 프로세스 전역 가변 상태는 `AtomicUsize` 카운터 셋뿐이다(`NEXT_TILE_ID` picture.rs:297,
     `NEXT_NAMESPACE_ID` render_backend.rs:761, `NEXT_NATIVE_SURFACE_ID` resource_cache.rs:61).
     비원자적인 것은 `PROFILER_HOOKS` 하나인데 **Servo 가 참조하지 않는다.**
   - painter 마다 **완전히 독립적인 인스턴스**를 만든다(`painter.rs:559`): 자기 Renderer,
     자기 RenderBackend + SceneBuilder 스레드(`create_webrender_instance` 가 직접 spawn 한다 —
     `renderer/init.rs:643,667,707`), 자기 RenderApi, 그리고 `namespace_alloc_by_client: true`
     + `shared_font_namespace: painter_id`. 네임스페이스가 이미 painter 별로 갈려 있다.

   `Renderer` doc 의 *"all instances share the same thread"* 는 **Gecko/Servo 가 지금 그렇게
   몰아서 구동한다는 서술**이지 제약이 아니다. 진짜 제약은 처음부터 있던 것 하나뿐이다 —
   **Renderer 는 자기 GL 컨텍스트를 소유한 스레드에 머물러야 한다.** A 가 요구하는 바로 그것이다.

   부수: 스레드 총수는 늘지 않는다. 지금도 인스턴스가 4 개라 WebRender 스레드는 이미 그만큼
   떠 있고, 바뀌는 것은 **누가 Renderer 를 구동하느냐**뿐이다.
4. **타일이 같은 GPU 를 공유하는 배치**(개발기 3-in-1). 그때는 두 타일 스레드가 하나의
   per-LUID ANGLE 디바이스를 동시에 쓰게 되고, 디바이스당 락이 그 둘을 직렬화한다 —
   4-GPU 월과 개발기의 동작이 갈린다. **개발기에서 성능을 재면 안 된다.**

## 8. 하지 않을 것

- 재실행 루프 뒤집기(전환 4N→4). ★반증됨★ — 전환은 적용의 96.7%에서 일어나지만 비용이 0.4%다.
- 크로스-GPU 텍스처 복사. RX580 은 피어 복사가 없어 시스템 메모리를 경유한다.
- 15ms 스톨의 원인 규명. 별개 축이고, 병렬화는 그것과 무관하게 이득을 낸다.

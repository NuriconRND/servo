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
> | 그 중 `AcquireSync(0, INFINITE)` | **74%**, 회당 **7.41ms** |
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
2. **`Painter` 를 타일 스레드로** — `Rc` 필드를 정리한다. `refresh_driver` 와
   `animation_refresh_driver_observer` 는 **타일별 인스턴스**로 나눌 수 있는지, 아니면
   메인에 남기고 메시지로 통신할지 결정해야 한다(미해결, §7).
3. **채널 프로토콜 + 배리어 이동** — 월 프레임 배리어를 `Cell` 기반에서 스레드 안전 구조로.
   실패 주입 env 3종(`SERVO_WALL_FRAME_DELAY_*`)이 계속 동작해야 한다.
4. **`present()` 를 타일 스레드로** — 리사이즈/DComp 재구축과의 조율이 여기 있다.
5. **직렬 경로 제거** — `render_all_tiles` 를 fan-out/join 으로 대체.

### 6.3 유지해야 하는 것

- 월 프레임 배리어의 의미론(16ms 데드라인, keep-previous 정책, `logical_frame_id`)
- 프레젠테이션 클럭(`about_to_wait` + `ControlFlow::WaitUntil`) — ★틱은 메인 스레드에 남는다★
- `--capture`, `--wall-tile-index` 단일 타일 모드
- servoshell 은 건드리지 않는다(별도 셸, 가드밴드 담당)

## 7. 미해결 (설계 확정 전에 답해야 함)

1. **`BaseRefreshDriver` 를 타일별로 나눌 수 있나?** 지금은 `Paint` 와 painter 들이 하나를
   공유하고 observer 가 `Painter` 를 콜백한다. 나눌 수 없으면 메인에 남기고 메시지로 통신해야
   하는데, 그러면 프레임 시작 신호가 스레드를 한 번 더 건넌다.
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

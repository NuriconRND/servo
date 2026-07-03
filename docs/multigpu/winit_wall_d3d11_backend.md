# winit_wall — wr-d3d11 네이티브 D3D11 백엔드 통합 & 검증 가이드

> WebRender를 surfman/ANGLE(GL 에뮬레이션) 대신 **네이티브 D3D11**(`wr-d3d11`)로 구동해,
> 멀티-GPU 비디오 월의 성능 저하 원인 중 "ANGLE의 GL→D3D 에뮬레이션 오버헤드" 기여도를
> 분리 측정하기 위한 실험 경로. 이 문서는 (1) 지금까지 검증된 것, (2) 직접 재현/검증하는 법,
> (3) 1920×1080을 넘어서(대화면 단일 서피스) 및 멀티-GPU로 확장할 때 필요한 설정·작업을 정리한다.

## 1. 현재 상태 (2026-07-03 기준, 1920×1080 단일 타일)

- ✅ `winit_wall --backend d3d11` 이 **빌드·실행되고 WebRender가 네이티브 D3D11로 렌더**한다.
  ANGLE/surfman 경유 없이 `Gld3d11`(gleam `gl::Gl`의 D3D11 구현)이 WebRender의 GL 호출을 받는다.
- ✅ 렌더 정확도: 색상(4색 사분면), 텍스트/폰트, 안티앨리어싱, 드롭섀도우, 반투명 원까지
  GL 백엔드와 동일하게 렌더된다. (`tests/html/dx_render_check.html` 기준 백버퍼 리드백 비교)
- ✅ 상하 방향(Y 규약) 버그 수정 완료 — 아래 2-D 참조.
- ⚠️ **알려진 충실도 갭**: `border-radius:50%`(원형/둥근 모서리 클립)가 적용되지 않아 중앙 흰 원이
  **사각형**으로 렌더된다(색/텍스트/섀도우/AA/반투명은 정상). 원인은 `wr-d3d11`의 클립/라운드
  브러시 셰이더 번역(Servo 통합이 아님). 비디오 월 워크로드(사각 타일·비디오)에는 무영향이나,
  둥근 UI 요소를 쓰는 페이지에선 추후 조사 필요. → `webrender_d3d11_native/wr-d3d11` 셰이더 경로.
- 단일 타일 / 단일 GPU만 배선됨(멀티-GPU 어댑터 선택은 미배선 — 5절).

빌드 산출물: `servo/target/release/examples/winit_wall.exe`
빌드 커맨드:
```powershell
. F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\scripts\servo_env.ps1
cd F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\servo
cargo build --release -p servo --example winit_wall --features media-gstreamer,no-wgl -j 8
```

## 2. 실행 전제 조건 (다른 장비로 옮길 때 특히 중요)

### 2-A. 셰이더 변환 툴 (실행 시 필수)
`wr-d3d11`은 WebRender의 GLSL(ES300) 셰이더를 **런타임에** 변환한다:
GLSL → SPIR-V(`glslangValidator`) → HLSL(`spirv-cross`) → DXBC(`D3DCompile`, Windows SDK 내장).

- 툴 위치 기본값: `webrender_d3d11_native/tools/bin/{glslangValidator.exe, spirv-cross.exe}`
- 환경변수 `WR_D3D11_TOOLS_DIR` 로 다른 경로 지정 가능(`shaders.rs::tools_dir`).
- 툴이 없으면: 최초 셰이더 컴파일에서 `... 실행 실패 — tools/fetch-shader-tools.ps1 실행 여부 확인` 패닉.
- 새 장비 세팅: `webrender_d3d11_native/tools/fetch-shader-tools.ps1` 실행해 두 exe를 받는다.

### 2-B. 셰이더 디스크 캐시 (최초 실행이 느린 이유)
- 변환·컴파일된 DXBC는 `webrender_d3d11_native/shader-cache/<hash>.{dxbc,hlsl,json}` 에 캐시된다.
- **최초 실행은 모든 셰이더를 변환/컴파일**하므로 첫 프레임까지 수백 ms~수 초 걸릴 수 있다.
  두 번째 실행부터는 캐시 히트로 즉시 뜬다.
- 변환 실패 시 GLSL/HLSL 덤프가 `shader-cache/failed/<hash>.{glsl,hlsl}` 에 남는다(디버깅용).
- 캐시 무효화가 필요하면(툴 인자/버전 변경 등) `shader-cache/` 를 지우고 재실행.

### 2-C. GStreamer / ANGLE DLL
- GL 백엔드(`--backend gl`)는 실행 파일 옆에 ANGLE(`libEGL.dll`/`libGLESv2.dll`) DLL이 필요하다.
- 미디어(비디오)를 쓰면 GStreamer DLL도 필요. 검증 스크립트는 `target\release\*.dll` 을 exe 옆으로
  복사해 둔다(아래 3절 스크립트 참고). D3D11 백엔드는 ANGLE이 필요 없지만, 같은 폴더에서 돌리면 무해.

## 3. 직접 재현·검증하는 법 (1920×1080)

### 3-A. 렌더 정확도 검증 (백버퍼 리드백 비교)
화면 캡처는 창이 오프스크린 디스플레이(0,0)에 뜨면 검게 찍혀 신뢰할 수 없다. 그래서
`--capture <png>` 로 **백버퍼를 present 직전에 직접 리드백**해 PNG로 저장한다(GL·D3D11 공통 경로).

```powershell
# gl 과 d3d11 을 각각 캡처해 사분면 색을 비교
pwsh -NoProfile -File scratchpad\capture_compare.ps1 -CaptureSec 5
```
스크립트가 하는 일: `example_1x1.json`(1920×1080 단일 타일) + `dx_render_check.html` 로 두 백엔드를
띄우고, 5초 뒤 primary 타일 프레임버퍼를 PNG로 저장 후 종료, 사분면/중앙 색을 출력한다.
기대: 두 PNG가 시각적으로 동일(RED 좌상 / GREEN 우상 / BLUE 좌하 / YELLOW 우하 / 중앙 흰 원).

`winit_wall` 직접 실행 시 캡처 플래그:
```
winit_wall.exe --backend d3d11 --wall-layout <1x1.json> --wall-all-tiles \
               --capture out.png --capture-sec 4 tests\html\dx_render_check.html
```
- `--capture <path>`: primary 타일 프레임버퍼를 PNG로 1회 저장 후 종료.
- `--capture-sec <n>`: 시작 후 몇 초 시점에 캡처할지(기본 3). 정적 페이지도 캡처 시점까지 계속 redraw.

### 3-B. 성능(present_fps) 비교
```
winit_wall.exe --backend d3d11 --wall-layout <1x1.json> --wall-all-tiles <page> 2> d3d11.err.log
winit_wall.exe --backend gl    --wall-layout <1x1.json> --wall-all-tiles <page> 2> gl.err.log
```
stderr에 초당 `Present perf: presents_per_s=.. avg_present_ms=..` 이 찍힌다. 창을 닫아 종료해야
버퍼가 flush된다(강제 kill 시 로그 유실). 미디어 페이지(비디오 그리드)로 두 백엔드를 비교하면
ANGLE 경로 대비 네이티브 D3D11의 present/합성 비용 차이를 볼 수 있다.

### 2-D. Y 규약(상하 반전) 처리 — 참고
초기 통합에서 D3D11 출력이 **상하 반전**됐다. 원인: `wr-d3d11`은 렌더 타깃을 내부적으로 GL
물리 레이아웃(row 0 = 하단)으로 유지하는데, DXGI present는 백버퍼 row 0을 화면 상단에 놓는다.
WebRender의 `WebRenderOptions::surface_origin_is_top_left` 가 기본 `false`(GL bottom-left)라 최종
패스를 GL 기준으로 뒤집어, DXGI 스왑체인에선 뒤집힌 채로 나온다.
- 수정: `RenderingContext::surface_origin_is_top_left()` 트레잇 메서드를 추가하고 D3D11 컨텍스트는
  `true`를 반환, `painter.rs`가 이를 `WebRenderOptions` 에 전달(무비용 — 투영 방향 플래그일 뿐).
  `wr-d3d11-sample`의 D3D11 백엔드와 동일한 처리다.

## 4. 1920×1080을 넘어서 (대화면 단일 서피스)

1 tile 1 GPU 원칙 하에 한 GPU가 구동하는 단일 대화면(예: 3840×2160) 검증 시:
- **해상도만 바꾸면 된다** — layout JSON의 `virtualViewport`/`tile.rect`를 대상 해상도로.
  Y 규약·백버퍼·DSV는 `resize`/`configure_target`에서 크기만 따라간다.
- D3D11 텍스처 한계 16384px 이내면 문제없음(4K/8K 폭 모두 OK).
- **최초 실행 셰이더 캐시 워밍**을 감안(2-B). 캐시가 채워진 뒤 present_fps를 측정할 것.
- 대화면에서 fps가 급락하면, 이는 백엔드(GL vs D3D11)와 **독립적인** WebRender picture caching /
  전체-씬 재합성 이슈일 수 있다(별도 조사 항목). 백엔드 비교는 동일 해상도·동일 페이지에서
  gl/d3d11만 바꿔 present_ms 차이를 볼 것.
- 검증은 사용자(대화면 장비)가 진행. 개발 장비에서는 1920×1080으로만 정합성 확인.

## 5. 멀티-GPU / 멀티 타일로 확장 (현재 미배선 — 필요한 작업)

현재 D3D11 백엔드는 **스왑체인 기본 어댑터**에 바인딩되고 `--wall-all-tiles`에서 타일 0만 렌더한다
(`winit_wall.rs`의 D3d11 분기가 `gpu_index`를 무시). 타일별 지정 GPU로 확장하려면:

1. **어댑터 지정 D3D11 디바이스 생성** — `webrender_d3d11_native/wr-d3d11/src/context.rs`:
   현재 `D3d11Context::create()`는 `D3D11CreateDevice(None, HARDWARE, ...)`로 기본 어댑터를 쓴다.
   지정 어댑터용 생성자 추가:
   ```rust
   pub fn new_hardware_on_adapter(adapter: &IDXGIAdapter) -> windows::core::Result<Self> {
       // 어댑터를 명시하면 driver_type 은 반드시 UNKNOWN 이어야 함
       Self::create_on(Some(adapter), D3D_DRIVER_TYPE_UNKNOWN)
   }
   ```
   (`create`를 `create_on(adapter: Option<&IDXGIAdapter>, driver_type)`로 일반화하고 첫 인자에 전달.)

2. **LUID/인덱스로 어댑터 열거** — `winit_wall.rs`는 이미 타일→디스플레이→어댑터 인덱스/ LUID를
   `spatial_order(enumerate_display_topology())` 로 구한다(`disp.adapter_index`, `disp.luid`).
   `IDXGIFactory1::EnumAdapters1(adapter_index)` 또는 LUID 매칭으로 `IDXGIAdapter`를 얻어
   위 생성자에 넘긴다.

3. **Dx11RenderingContext::new 확장** — `dx11_rendering_context.rs`:
   현재 `new(hwnd, size)`는 내부에서 `D3d11Context::new_hardware()`(기본 어댑터)를 호출한다.
   `new_on_adapter(hwnd, size, adapter_index)` 를 추가해 2)의 어댑터로 디바이스를 만들고,
   그 디바이스에서 `IDXGIFactory2`를 얻어(현재 코드가 이미 device→adapter→factory 캐스팅을 함)
   스왑체인을 생성한다. **스왑체인은 백버퍼를 구동하는 그 어댑터의 팩토리로 만들어야** 해당
   모니터로 direct present된다.

4. **winit_wall D3d11 분기 배선** — `winit_wall.rs` 412-444 근처:
   `let _ = gpu_index;` 를 지우고 `gpu_index`(= `disp.adapter_index`)를 `new_on_adapter`에 전달.
   멀티 타일 경고(292 근처)와 tile 0 한정 로직을 제거해 모든 타일 창을 각자 어댑터로 생성.
   각 타일은 자기 스레드/컨텍스트에서 make_current→paint→present (D3D11 immediate context는
   디바이스별로 독립이므로 GL처럼 전역 current 충돌 없음).

5. **RenderingContext::requested_gpu_index()** 는 이미 트레잇에 있으니, Dx11 컨텍스트가 이를
   `Some(adapter_index)`로 반환하도록 오버라이드하면 진단 로그/일관성에 도움(선택).

주의:
- 서로 다른 GPU의 D3D11 디바이스 간에는 텍스처를 직접 공유하지 않는다(v1 비목표: cross-GPU 복사).
  각 타일은 자기 GPU에서 전체 WebRender 인스턴스를 독립 구동한다(현 팬아웃 모델과 동일 철학).
- 미디어(비디오)를 여러 GPU 타일에 표출하려면 CPU 프레임을 각 GPU에 업로드해야 한다
  (멀티-GPU 프레임 분산 메모 참조). 이는 백엔드(GL/D3D11) 무관한 별도 경로.

## 6. 0.68 호환 주의 — `GL_EXT_color_buffer_float`

`wr-d3d11`은 원래 WebRender 0.69 대상이라 `EXTENSIONS`에 `GL_EXT_color_buffer_float`를 광고했다.
Servo의 WebRender 0.68은 이 확장이 있으면 GPU 캐시 갱신에 **point-scatter 경로**를 택하는데,
그 `gpu_cache_update` 정점 셰이더가 `gl_PointSize`를 쓰고 SPIRV-Cross HLSL 백엔드가 이를 지원하지
않아(`Unsupported builtin in HLSL: 1`) 초기화가 크래시했다.
- 수정: `webrender_d3d11_native/wr-d3d11/src/lib.rs` 의 `EXTENSIONS` 에서 그 확장을 제거 →
  WebRender가 **PixelBuffer(텍스처 업로드) GPU 캐시 경로**로 폴백(포인트 셰이더 불필요).
- 성능 함의: GPU 캐시 갱신이 point-scatter 대신 텍스처 업로드로 이뤄진다. GPU 캐시 갱신량이
  많은 씬에서 미세한 차이가 있을 수 있으나, 비디오 월 워크로드에선 지배적 요인이 아니다.
- WebRender를 0.69로 올리면 이 우회는 재검토 필요.

## 7. 관련 코드 좌표

- `servo/components/shared/paint/dx11_rendering_context.rs` — `Dx11RenderingContext`(신규).
- `servo/components/shared/paint/rendering_context.rs` — `RenderingContext` 트레잇
  (`surface_origin_is_top_left` 기본 메서드 추가).
- `servo/components/paint/painter.rs:344` — `WebRenderOptions` 에 `surface_origin_is_top_left` 전달.
- `servo/components/paint/paint.rs:989` — 비-surfman 컨텍스트는 surfman 상세 등록을 건너뜀
  (D3D11은 `connection()`이 None; surfman 상세는 WebGL 전용).
- `servo/components/servo/examples/winit_wall.rs` — `--backend`, `--capture[-sec]`, D3d11 분기.
- `webrender_d3d11_native/wr-d3d11/src/{context.rs,lib.rs,shaders.rs,gl_impl.rs}` — 네이티브 백엔드.

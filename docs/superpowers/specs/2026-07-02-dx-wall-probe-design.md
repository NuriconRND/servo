# dx_wall_probe 설계 — WebRender 없는 DirectX11 비디오 월 대조군

- 날짜: 2026-07-02
- 코드 위치: 리포 루트 `tools/dx_wall_probe/` (servo / winit_wall과 완전 별개, 기존 `tools/topology_probe`·`tools/wall_perf_analyzer`와 동일 계층)
- 관련: `servo/etc/multigpu/BUILD_DEPLOY_and_PARAMS.txt`, 메모리 `video-grid-composite-bottleneck.md`

## 1. 목적 (Why)

멀티-GPU 대화면 비디오 월에서 성능 저하가 **WebRender 합성 경로** 때문임을 격리·입증하기 위한
**대조군**. Servo/winit_wall과 동일한 테스트 동작(같은 wall_layout, 같은 GPU 배치, 같은 GStreamer
per-video 디코드, 같은 grid)을 하되, **WebRender를 완전히 배제**하고 각 비디오를 표출 영역을 덮는
Windows 스왑체인 백버퍼에 **직접 렌더**한다. 이것이 vsync 60fps를 유지하면, 병목이 디코드/레이아웃이
아니라 WebRender 합성임을 실증한다.

## 2. 확정 결정 (Decisions)

- 언어/플랫폼: **C++ / Win32 / Direct3D 11 / GStreamer C API** (Windows 전용).
- 디코드→GPU 경로: **appsink → CPU YUV(NV12/I420) 프레임 → DX11 텍스처 업로드 → 셰이더 YUV→RGB →
  쿼드 드로우.** Servo의 decode/프레임 경로와 동일 → **합성기만 다름 = WebRender 합성 비용 순수 격리.**
- 범위: **전 타일 멀티-GPU 단일 프로세스** (winit_wall `--wall-all-tiles` 대응). 타일마다 창+스왑체인을
  그 타일을 구동하는 GPU에.
- 렌더 백엔드: **DX11만** (DX12는 후속 spec). 단 렌더러를 최소 seam으로 분리해 후속 DX12 용이하게.
- 프레젠트 모델: **타일당 렌더 스레드**(기본, per-tile 독립 vsync present) + 메인 스레드는 Win32 메시지
  펌프. `--present-mode single`로 winit_wall식 단일-스레드 순차 present도 선택 가능(엄밀 대조용).
- present는 vsync에 맞춤(`Present(1,0)`), **vsync 전에 준비 안 된 프레임은 무시**(최신-프레임-슬롯이
  오래된 것을 덮어써 자연 드롭).
- 빌드: **CMake** (벤더드 VS2022 toolchain). GStreamer devel 링크는 `GSTREAMER_1_0_ROOT_MSVC_X86_64`
  (기본; 정확한 Servo 일치가 필요하면 번들 `servo/target/dependencies/.../msvc_x86_64` 1.26.8로 전환).

## 3. 입력 (CLI)

- `--wall-layout <json>` : winit_wall과 동일 포맷(virtualViewport{w,h}, tiles[]{display,rect[x,y,w,h]}, overlapPx).
- `--cols N` `--rows N` : 각 타일 내 grid (총 비디오 = tiles × cols × rows).
- `--video <path>` : 전 셀 공통 소스(file 경로 → file:// URI).
- `--present-mode per-tile|single` (기본 per-tile).
- `--vsync on|off` (기본 on).
- (선택) `--wall-tile-index N` : 특정 타일만 (디버그용 단일 타일).

## 4. 컴포넌트 (각 파일 = 단일 책임)

- `wall_layout.{h,cpp}` : JSON 파싱 → `WallLayout{ virtual_viewport, tiles[], overlap_px }`,
  `WallTile{ display_index, rect }`. (작은 JSON 파서 또는 헤더-온리 lib; 의존 최소화.)
- `dxgi_topology.{h,cpp}` : `CreateDXGIFactory1`→`EnumAdapters1`→`EnumOutputs`→`DXGI_OUTPUT_DESC.
  DesktopCoordinates` + adapter `DXGI_ADAPTER_DESC1.AdapterLuid` 로 **desktop-rect → adapter** 매핑.
  공간 순서(top-left=0, 좌→우 그다음 위→아래) 정렬 = winit_wall `spatial_order` 재현. 타일.display →
  구동 adapter 반환.
- `dx11_tile_renderer.{h,cpp}` : 한 타일의 GPU/창 소유. 그 adapter로 `D3D11CreateDevice` +
  `CreateSwapChainForHwnd`(FLIP_DISCARD). 셀별 YUV 텍스처(Y=R8, UV=RG8(NV12) 또는 U/V=R8(I420)),
  YUV→RGB 픽셀 셰이더, 셀 sub-rect 뷰포트 쿼드 드로우, `Present`. 프레임 슬롯에서 최신 프레임을 읽어
  업로드; 새 프레임 없으면 이전 텍스처 유지.
- `video_cell.{h,cpp}` : 한 셀의 GStreamer 파이프라인(`playbin` + `appsink`) + **최신-프레임 슬롯**
  (뮤텍스 보호 이중버퍼: `new-sample` 콜백이 write, 렌더 스레드가 take-latest). loop 재생.
- `win32_window.{h,cpp}` : 무테두리 창 생성(타일 rect 위치·크기), 메시지 처리, 닫기 이벤트.
- `main.cpp` : CLI 파싱 → 레이아웃/토폴로지 → 타일별 (창+renderer+cells) 구성 → present-mode에 따라
  타일 스레드 시작 or 단일 루프 → Win32 메시지 펌프 → 종료 정리.
- `shaders/yuv.hlsl` : NV12/I420 → RGB 변환 픽셀 셰이더(+ 풀스크린/쿼드 버텍스). Rec709/601 인지.

## 5. 데이터 흐름

1. main: 레이아웃 파싱 → 토폴로지로 각 타일.display→adapter 결정.
2. 타일마다: 창 생성(display 좌표) → 그 adapter로 DX11 device+swapchain → cols×rows개 `video_cell`
   생성(각자 GStreamer 파이프라인 시작).
3. GStreamer 스레드: 디코드된 CPU YUV 프레임을 셀의 최신-프레임 슬롯에 write(오래된 것 덮어씀).
4. 렌더(타일 스레드, vsync 루프): 각 셀 슬롯에서 최신 프레임 take → 그 device의 텍스처로 업로드 →
   셀 sub-rect에 YUV→RGB 쿼드 드로우 → 전 셀 후 `Present(1,0)`.
5. 계측: 타일별 초당 `present_fps`/`avg_present_ms`, 셀 업로드 프레임수 stderr.

## 6. 에러 처리 / 정리

- GStreamer: 파이프라인 에러 로그, EOS→loop(seek 0), 시작 실패 셀은 스킵+경고.
- DX11: `D3D11CreateDevice`/스왑체인 실패 시 그 타일 스킵+명확한 에러. device-lost(`DXGI_ERROR_DEVICE_REMOVED`)
  감지 시 로그(v1은 해당 타일 정지; 재생성은 후속).
- 종료: 창 닫기 → 타일 스레드 정지 → 파이프라인 NULL 전환 → 리소스 해제.

## 7. 검증 (수동, winit_wall과 대조)

- 단일 타일 1080p에서 cols×rows(예 3x3, 4x4, 6x6) 늘려가며 `present_fps` 확인.
- 대화면 멀티-GPU에서 winit_wall과 **동일 wall_layout·동일 영상수**로 present_fps 비교.
- 기대: 직접 DX11이 vsync 60 유지(또는 winit_wall 대비 큰 폭 상회) → WebRender 합성이 병목임을 입증.
- `--present-mode single` 로 단일-스레드 present의 영향(멀티-GPU 직렬화)도 분리 측정.

## 8. 범위 밖 (Out of Scope)

- DX12 백엔드(후속 spec). WebGL/WebGPU/DOM/CSS(비디오 그리드만). 입력 이벤트/상호작용.
- cross-GPU 텍스처 공유(각 타일은 자기 GPU에 CPU 프레임 업로드; 공유 없음).
- 하드웨어 zero-copy 디코드 경로(이번은 appsink CPU 프레임으로 Servo 일치가 목적).
- HTML 파싱(그리드는 CLI cols/rows로 재현; multigpu_wall_video_grid_36.html는 참조용).

## 9. 빌드/배포 개요 (상세는 구현 계획에서)

- `tools/dx_wall_probe/CMakeLists.txt` : Win32 + d3d11.lib/dxgi.lib/d3dcompiler.lib + GStreamer
  (gstreamer-1.0, gstapp-1.0, gobject-2.0, glib-2.0) include/lib(pkgconfig 또는 GSTREAMER_1_0_ROOT).
- 실행: exe 옆(또는 PATH)에 GStreamer bin DLL 필요(플러그인 로드). 레이아웃/영상 경로는 CLI 인자.

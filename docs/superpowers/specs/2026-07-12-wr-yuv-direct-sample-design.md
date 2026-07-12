# WR YUV 직접 샘플: 변환기 제거 + ANGLE 디바이스 DYNAMIC plane 링 (A-dyn)

날짜: 2026-07-12
상태: 설계 승인됨 (구현 전)
대상: components/media/backends/gstreamer/render-d3d11, components/media/player/video.rs,
components/media/media-thread, components/script/dom/html/htmlmediaelement.rs,
third_party/surfman (EGLImage 래핑 헬퍼 소형 추가)
선행 설계: 2026-07-09-d3d11-per-pipeline-upload-design.md (§3-k 구현·검증 완료),
2026-07-11-d3d11-dynamic-upload-design.md (§3-n 구현·검증 완료)
관련 조사: §3-o/§3-p ANGLE present-path-fast (present 복사 제거, 커밋 bde39ef8f)

## 1. 배경과 문제

### 1.1 잔여 2패스 구조

present-path-fast(§3-p) 적용 후에도 Servo의 비디오 표시는 네이티브 probe
(D:\2_TechReview\20260703_dx_wall_probe) 대비 2패스다:

- **probe (1패스)**: Y/U/V plane 텍스처를 최종 draw에서 직접 샘플, 셰이더에서
  BT.709 변환 → 백버퍼. 타일·프레임당 draw 1회.
- **Servo (2패스)**: GstD3D11Converter가 YUV→RGBA 링 텍스처로 draw 1회
  (YUV 3.1MB 읽기 + RGBA 8.3MB 쓰기, 1080p I420 기준) + WR 합성이 그 RGBA를
  다시 샘플해 draw 1회. RGBA 중간 텍스처 왕복 = 타일·프레임당 +16MB RW.

### 1.2 구형 AMD 제약 (설계 지배 조건)

구형 AMD GPU는 CopySubresourceRegion 등 복사류 명령에 특히 취약하다
(현장 관찰 — §3-n의 동기와 동일 계열; 창 확대 시 GPU 사용률이 먼저 포화함을
사용자 확인). 따라서 본 설계의 최우선 제약은 **GPU 복사 명령을 줄이는 것이
아니라 0으로 만드는 것**이다. 이 제약이 "공유 DEFAULT 링 + 복사" 계열 설계를
전부 기각시킨다 (§3 대안 표).

### 1.3 사용자 결정 (2026-07-12)

- WR YUV 직접 샘플 **단일 경로**. GstD3D11Converter/RGBA 경로는 **완전 삭제**
  (env 폴백도 두지 않음).
- 복사 명령(CopySubresourceRegion/UpdateSubresource) 회피 최우선.
- P010(10-bit)은 범위 제외 (8-bit I420/YV12/NV12만).

## 2. 목표와 성공 기준

비디오 표시 경로에서 GPU가 하는 일을 **WR 합성 draw에서 YUV plane을 샘플하는
것 하나로** 줄인다. GPU 복사 명령 0회, 변환 draw 0회, fence 0회. probe의
`decode-copy-dyn` 모드("복사 0 + 시스템 메모리(WC) 샘플")와 동형 구조.

성공 기준:
- 기존 45타일 검증 항목 전부 유지: FAIL 0, 마커 45/45, lockstep ±1,
  gapless/동기그룹 무회귀, 메모리 플래토, 시작 안착 기존 수준(t≈6s)
- 색 정확(BT.601/709 limited/full 픽셀 검증), 상하 방향 정확
- WebGPU 월(gpu-direct) 무회귀 — 공유 핸들 import 경로는 WebGPU가 계속 사용
- 구형 AMD 실기: GPU 점유율 감소 또는 동일 점유율에서 타일 수 증가 (실기 확보
  시 측정, §3-n·§3-p와 동일하게 추후)

## 3. 검토한 대안

| 안 | 내용 | 판정 |
|---|---|---|
| 공유 DEFAULT plane 링 + CopySubresourceRegion | DYNAMIC 스테이징 유지, GPU 복사로 공유 텍스처 채움 | **기각** — 복사 명령 재도입, §1.2 위배 |
| 공유 DEFAULT plane 링 + UpdateSubresource | 드라이버 관리 업로드 | **기각** — 드라이버 내부도 스테이징+GPU 전송(복사)이며 구형 AMD 경로 예측 불가 |
| **ANGLE 디바이스에 DYNAMIC plane 링, gst 스레드는 memcpy만 (A-dyn)** | 복사 0, 단일 디바이스로 회귀해 공유 자체를 제거 | **채택** |
| 네이티브 NV12/P010 텍스처 + EGL_D3D11_TEXTURE_PLANE_ANGLE | 슬롯당 텍스처 1개 | 기각 — 네이티브 NV12는 드라이버 의존(§3-n 회피 결정), I420은 DXGI 포맷 부재로 videoconvert 강제 |
| 렌더러 스레드 업로드 (기존 raw Buffer 경로) | WR 텍스처 캐시 업로드 | 기각 — §3-b 실측 45타일 27ms/frame 절벽. 단 이 경로는 `SERVO_MEDIA_D3D11_VIDEO` off의 기존 폴백으로 존치(삭제 아님) |

핵심 논리: 디바이스가 갈라져 있는 한 공유(=DEFAULT=복사)가 강제된다. 복사를
0으로 만드는 유일한 길은 plane 텍스처를 **소비자(ANGLE) 디바이스에 두는 것**
이고, 이때 D3D11 스레딩 규칙(디바이스 메서드는 free-threaded, immediate
context는 단일 스레드)에 따라 Map/Unmap만 렌더러 스레드로 보내고 무거운
memcpy는 gst 스레드에 남기면 §3-k가 달성한 "렌더러 스레드 업로드 0"도
유지된다. 이 producer 계약은 probe `decode-copy`/`decode-copy-dyn` 모드가
이미 검증했다 (사전 Map된 슬롯 링에 스트리밍 스레드가 쓰고, 렌더 스레드는
Unmap/re-Map만).

## 4. 설계

### 4.1 데이터 흐름

```
[플레이어 생성 시]
  ANGLE ID3D11Device 포인터(free-threaded)를 플레이어에 전달
  플레이어가 plane DYNAMIC 텍스처 링 생성 (슬롯 4 × plane 2~3,
    R8=Y/U/V, RG8=NV12 UV, USAGE_DYNAMIC + BIND_SHADER_RESOURCE,
    CPU_ACCESS_WRITE, 공유 플래그 없음) → 슬롯 레지스트리에 등록

[매 비디오 프레임, gst 스트리밍 스레드 — D3D 호출 0회]
  appsink(sysmem I420/YV12/NV12) → build_frame:
    레지스트리에서 FREE(사전 Map됨) 슬롯 claim → 행 단위 memcpy
    (RowPitch 준수, §3-n 로직 재사용) → FILLED 마킹
    → VideoFrame{메타데이터만: ring_epoch, 포맷, colorimetry, plane 크기}
    → 기존 경로로 htmlmediaelement에 전달

[htmlmediaelement — render_yuv_frame 패턴 재사용]
  plane별 ImageKey 2~3개 + plane별 ExternalImageId,
  ExternalImageType::TextureHandle(Texture2D), UpdateImage 트랜잭션 1회
  → layout이 기존 push_yuv_image로 WR YUV 프리미티브 생성

[합성 시, 렌더러 스레드 — media-thread lock() 콜백]
  최신 FILLED 슬롯 존재 시: Unmap(context 호출, 렌더러 스레드 ✓)
    → PRESENTING 전환, 이전 PRESENTING 슬롯 re-Map(WRITE_DISCARD) → FREE
  해당 슬롯의 EGLImage 래핑 GL 텍스처 반환 (plane별)
  → WR brush_yuv_image가 YUV→RGB를 합성 draw 안에서 수행 (1패스)
```

두 프레임이 합성 사이에 도착하면 lock이 최신 FILLED만 소비하고 이전 것은
재활용한다 (latest-wins — painter 코얼레싱과 동일 철학). 합성이 비디오보다
빠르면(60 vs 30fps) 같은 PRESENTING 슬롯을 재반환한다.

### 4.2 슬롯 상태기계와 스레딩 규칙

상태: `FREE(mapped, 포인터 공개) → WRITING(gst claim) → FILLED(쓰기 완료)
→ PRESENTING(Unmap됨, WR 샘플 중) → (re-Map) → FREE`

- plane 2~3개는 슬롯 인덱스를 공유하며 한 몸으로 전이한다 (프레임 원자성).
- **D3D 컨텍스트 호출(Map/Unmap)은 전부 렌더러 스레드**: lock() 콜백이
  렌더러 스레드에서 실행되므로 ANGLE의 GL 호출과 자연 직렬화된다. gst
  스레드는 공개된 포인터에 memcpy만 한다 (CPU 포인터, D3D 아님).
- 텍스처 **생성**은 ID3D11Device 메서드라 free-threaded — 플레이어 스레드에서
  수행 가능.
- WRITE_DISCARD rename이 이전 합성의 draw in-flight와의 충돌을 제거한다
  (probe 동일). 단일 디바이스이므로 Unmap→draw 하자드는 런타임이 추적 —
  fence 불필요.
- 레지스트리는 기존 `RawVideoFrameExternalImages`/`D3d11VideoFrameExternalImages`
  패턴(전역 static + 뮤텍스)의 후속으로 구현. 잠금 구간은 상태 전이만
  (memcpy는 잠금 밖).

### 4.3 프로비저닝 핸드셰이크 (첫 프레임)

초기 Map은 렌더러 스레드에서만 가능하므로 닭-달걀이 생긴다 (첫 lock은 첫
이미지 갱신 후에야 옴). 해법 — **첫 프레임 스테이징**:

1. 링 생성 직후 플레이어는 첫 프레임을 CPU 스테이징 버퍼(malloc)에 복사하고
   FILLED-STAGED로 마킹, 이미지 갱신은 정상 발행.
2. 렌더러의 첫 lock: 링 전 슬롯 Map → 스테이징 프레임을 슬롯 0에 memcpy
   (렌더러 스레드 일회성 복사, 플레이어당 1프레임) → 포인터 공개 → 이후
   정상 사이클.
3. 크기 변경 시 새 에폭 링으로 동일 핸드셰이크 반복.

45타일 동시 시작 시 렌더러 일회성 복사 45×3.1MB가 첫 합성들에 분산된다 —
기존 공유 핸들 import 램프(§3-l 계측)와 동급의 일회성 비용.

### 4.4 EGLImage 래핑 (pbuffer 경로 불가)

- 기존 surfman pbuffer 경로(`CreatePbufferFromClientBuffer`)는 **사용 불가**:
  SwapChain11이 client-buffer 텍스처에 무조건 RTV를 생성하는데
  (SwapChain11.cpp:319) DYNAMIC은 RENDER_TARGET 바인드가 불가.
- 대신 **`EGL_ANGLE_image_d3d11_texture`** 경로 사용:
  `eglCreateImageKHR(display, EGL_NO_CONTEXT, EGL_D3D11_TEXTURE_ANGLE, tex)`
  → `glEGLImageTargetTexture2DOES(GL_TEXTURE_2D, image)`.
  ExternalImageSiblingImpl11은 바인드 플래그로 능력을 판정하며
  (mIsTexturable=SHADER_RESOURCE만 요구, :49-54) RTV는 renderable일 때만
  생성한다 (:125) — SRV-only 텍스처 합법.
- surfman에 소형 헬퍼 추가 (EGLImage 생성 + GL 텍스처 바인딩 + 파기).
  래핑은 슬롯당 1회, 캐시 (45타일 × 슬롯 4 × plane 3 ≈ 최대 540개, 일회성).
- **주의**: 기존 `create_surface_texture_from_shared_handle`(pbuffer import)은
  WebGPU gpu-direct가 계속 사용하므로 삭제하지 않는다 (canvas_context.rs:370).

### 4.5 포맷·컬러·방향

- 지원: I420/YV12/NV12 (8-bit). appsink caps 목록에서 P010_10LE 제거 —
  10-bit 소스는 playbin이 videoconvert로 8-bit 강등 (현 콘텐츠 전부 8-bit).
  ANGLE은 R16/RG16도 검증 목록에 있어 후속 확장 길은 열려 있음
  (Renderer11.cpp:1578-1579).
- YV12: WR `YuvData::PlanarYCbCr`는 Y,Cb,Cr 순서 — external ID 매핑에서
  U/V plane 스왑 (§3-n 변환기가 하던 스왑의 이전).
- Colorimetry: gst caps → `VideoFrameYuvColorSpace/Range` 매핑은 기존 raw
  경로(render.rs)와 htmlmediaelement의 `wr_yuv_color_space/range`
  (htmlmediaelement.rs:228-240) 재사용. YUV→RGB 수식이 변환기에서 WR
  brush_yuv_image 셰이더로 이동하므로 픽셀 검증 필수.
- 수직 플립: D3D11 래핑 텍스처는 플립 제외(§3-k needs_vertical_flip 이력).
  plane 경로도 동일 예외를 타는지 구현에서 확인 (검증 계획 §8에 방향 체크).

### 4.6 htmlmediaelement / media-thread 변경

- `VideoFrameData::D3D11`의 페이로드를 교체: `shared_handle/ring_epoch` →
  `{ring_epoch, yuv 포맷, colorimetry, plane 크기}` (슬롯 인덱스는 프레임에
  싣지 않는다 — lock이 레지스트리에서 최신 FILLED를 소비). gst 버퍼는
  build_frame에서 memcpy 후 즉시 해제 가능 (frame-holder는 메타데이터만 유지).
- `render_d3d11_frame`을 `render_yuv_frame`(htmlmediaelement.rs:574) 패턴으로
  재작성: plane별 2~3 ImageKey + `MediaFrameYuvImage` + TextureHandle external.
  add/update/delete·포맷 변경 수명주기는 기존 코드 구조 이식.
- media-thread: `D3d11VideoFrameExternalImages`를 plane별 ID + 슬롯 레지스트리
  로 확장. `lock_d3d11`은 "최신 FILLED 소비 + EGLImage 캐시 반환" 으로 재작성
  (OpenSharedResource 래핑 캐시는 제거).
- 해체: 플레이어 제거 → 레지스트리 removed 마킹 → 렌더러가 다음 lock/Drop에서
  Unmap + EGLImage/GL 텍스처 파기 + 텍스처 해제 (기존
  `purge_removed_d3d11_entries` + Drop 패턴 유지).

### 4.7 삭제 목록

| 삭제 | 근거 |
|---|---|
| GstD3D11Converter FFI 일체 + convert_buffer 호출 (lib.rs:363) | 변환 draw 제거 |
| RGBA MISC_SHARED 링 + GetSharedHandle + QUERY_EVENT fence | 공유·fence 불필요 (단일 디바이스) |
| 플레이어별 GstD3D11Device 생성 + gstd3d11.dll 라이브러리 로딩 | 플레이어가 D3D 호출 안 함 |
| DynamicUploadSet의 플레이어 디바이스 텍스처 | ANGLE 디바이스 링으로 대체 (행 복사 로직은 재사용) |
| legacy d3d11upload 경로 + `SERVO_MEDIA_D3D11_UPLOAD` env | 변환기 의존이므로 함께 소멸 |
| lock_d3d11의 OpenSharedResource 래핑 캐시 | EGLImage 캐시로 대체 |

유지: `SERVO_MEDIA_D3D11_VIDEO=1` 게이트(off=기존 raw Buffer 경로), d3d11 bin의
리드 큐(`SERVO_MEDIA_D3D11_LEAD_FRAMES`, §3-l — appsink 앞 큐로 존치),
`SERVO_D3D11_PROFILE` 계측 게이트(대상 재조준), gapless/동기그룹/direct-file 전부
무관 존치. 패키징의 gstd3d11.dll 번들 의무(§3-n 함정 2)는 소멸 — 계획에서
번들 목록 재검토.

부수 이득: 멀티GPU 월(§4.5 스펙)에서 링이 창별 ANGLE 디바이스에 생기므로
어댑터 정합이 자동이 된다 (프로듀서 디바이스 정합 문제 자체가 소멸).

## 5. 검증된 근거 (2026-07-12 소스 확인)

| 사실 | 근거 |
|---|---|
| layout의 WR YUV 프리미티브 경로 존재 | display_list/mod.rs:765-784 `push_yuv_image` (NV12/PlanarYCbCr + depth/space/range) |
| plane별 키·external ID 수명주기 기존재 | htmlmediaelement.rs:574-715 `render_yuv_frame`, :198-226 `MediaYuvExternalIds` |
| WR 0.68: TextureHandle external은 텍스처 캐시 미경유 | resource_cache.rs:162-163, :1451 |
| WR YUV 배치가 plane별 텍스처 해석 (전 plane 동일 종류 요구) | batch.rs:2338-2374 |
| ANGLE client buffer 포맷: R8/RG8/R16/RG16 허용 | Renderer11.cpp getD3DTextureInfo :1576-1579 |
| ANGLE 요구: 텍스처가 ANGLE 자신의 디바이스 소속 | Renderer11.cpp:1500-1505 |
| getD3DTextureInfo에 usage/bind 검사 없음 | :1483-1666 전문 확인 |
| pbuffer 경로는 무조건 RTV 생성 → DYNAMIC 불가 | SwapChain11.cpp:313-320 |
| EGLImage 경로는 SRV-only 합법 (RTV는 renderable일 때만) | ExternalImageSiblingImpl11.cpp:49-54, :125 |
| producer 계약(사전 Map 링 + 스트리밍 스레드 쓰기) 검증됨 | probe app.h:35-53 `DecodeCopy`/`DecodeCopyDyn` |
| 직접 DYNAMIC 샘플이 단일 디바이스 최적해 | probe README "프로브 결론" (par-memcpy-static 기본값) |
| 렌더러 스레드 업로드는 45타일 절벽 | §3-b WR 프로파일러 실측 (27ms/frame) |
| MISC_SHARED=DEFAULT 강제, context 단일 스레드 | D3D11 API 규칙 (probe README:162도 명시) |

## 6. PoC 게이트 (구현 계획의 첫 태스크)

1. **DYNAMIC 텍스처의 texturable 판정**: `IDXGIResource::GetUsage`가
   DYNAMIC+SHADER_RESOURCE 텍스처에서 `DXGI_USAGE_SHADER_INPUT`을 보고해
   `mIsTexturable=true`가 되는지 (ExternalImageSiblingImpl11.cpp:53-54).
2. **WRITE_DISCARD rename 투명성**: re-Map 후에도 EGLImage의 SRV가 새 내용을
   샘플하는지 (D3D11 rename 규약상 뷰는 리소스 객체를 따라가야 함 — 실증).
3. 1타일 E2E: ANGLE 디바이스에 R8×3 생성 → 렌더러 스레드 Map/memcpy/Unmap →
   EGLImage 래핑 → WR YUV external로 표시 → 색·방향 픽셀 확인.

셋 다 1타일 PoC 하나로 판정된다. **게이트 1·2 중 하나라도 실패하면 구현을
진행하지 않고 중단한 뒤 사용자에게 이후 진행 방향을 문의한다** (사용자 지시
2026-07-12). 이때 제시할 후보는 공유 DEFAULT plane 링 + 복사 설계(본 문서 §3
첫 행)이나, AMD 복사 회피 제약과 상충하므로 자동 채택하지 않는다.

## 7. 리스크와 대응

| 리스크 | 평가·대응 |
|---|---|
| WC 시스템 메모리 샘플링 비용 (매 합성 PCIe 경유) | 월(축소 표출): 샘플 footprint ≈ 출력 픽셀 — 복사 대비 대역폭 대폭 감소. 창 확대 1:1: plane 전체 × 합성률 ≈ 복사 방식의 ~2배 바이트지만 복사 엔진 미사용(draw 샘플러 경로). **probe `decode-copy` vs `decode-copy-dyn` A/B(README:193, avg_present_ms 비교)로 AMD 실기 판정 가능 — 구현 비차단** |
| 렌더러 스레드 Map/Unmap 호출량 (~45×3×2÷2 ≈ 135회/합성 @30fps 비디오) | probe 실측: Map/Unmap은 memcpy 대비 소액, par-atlas로 "호출 수는 병목 아님" 확인. SERVO_D3D11_PROFILE로 계측 |
| 첫 프레임 스테이징 복사 (렌더러 스레드, 45×3.1MB 일회성) | 시작 램프에 분산, 기존 import 램프와 동급. 시작 안착 시간 회귀 검증 |
| WR external YUV TextureHandle 조합의 Servo 첫 사용 | PoC 게이트 3에서 조기 검증 |
| 색 수식 이동 (변환기→WR 셰이더) | BT.601/709 픽셀 검증 (스모크에 포함) |
| 상하 방향 | needs_vertical_flip 예외 경로 확인 + 스크린샷 검증 |
| 슬롯 고갈 (합성 지연 시 FREE 부족) | 슬롯 4개 + latest-wins 소비로 완화; 고갈 시 프레임 드롭 + 계측 카운터 (기존 관례) |
| GL 텍스처/EGLImage 540개 | 일회성·캐시, 메모리 미미 (텍스처 자체는 어차피 존재) |

## 8. 검증 계획

1. PoC (§6) — 게이트 실패 시 여기서 중단·재설계.
2. 크레이트 테스트: 슬롯 상태기계 단위 테스트 (D3D 없이 상태 전이·claim 경합·
   에폭 교체). §3-n의 변환기 의존 E2E 테스트는 삭제 대상과 함께 제거.
3. 2×2 스모크: 픽셀 색(BT.709 limited 기준값)·상하 방향·재생 안정.
4. 45타일 회귀: FAIL 0, 마커 45/45, lockstep ±1, 시작 안착 t≤기존, 메모리
   플래토, 루프 경계 무결 (gapless).
5. WebGPU 월 무회귀 (공유 핸들 import 경로 보존 확인).
6. WR 프로파일러(Ctrl+F12)·PresentMon으로 Renderer/present 지표 전후 비교.
7. AMD 실기: ServoWallPackage 재패키징 후 측정 (추후, §3-n·§3-p와 동일 패턴).
   선택: probe decode-copy vs decode-copy-dyn A/B로 샘플링-비용 가설 독립 검증.

## 9. 비범위

- P010/10-bit 표시 (videoconvert 강등으로 정확성은 유지)
- HW 디코드 (사용자 보류 선언)
- probe A/B 실행 (사용자 판단, 구현 비차단)
- raw Buffer 경로(`SERVO_MEDIA_D3D11_VIDEO` off) 개선

# 파이프라인별 D3D11 직접 업로드 설계

날짜: 2026-07-09
상태: 설계 승인됨 (구현 전)
대상: components/media (gstreamer 백엔드, media-thread), components/paint, ports/servoshell

## 1. 배경과 문제

다중 비디오 월(단일 창 N타일 `<video>` 그리드)의 모든 성능 증상 — 45타일 상한,
시작 슬로우모드(42타일부터 30fps대 고착), 간헐 멈칫→추격, 루프 경계 부하 — 의
단일 뿌리는 업로드 구조다:

```
[CPU 디코드 avdec, ~12코어] → [시스템 RAM] → [WR 렌더러 단일 스레드가 glTexSubImage2D
                                              (ANGLE 내부 동기 memcpy) ~4.2GB/s] → [VRAM]
```

- 업로드가 렌더러 스레드의 임계 경로에 있어 프레임 시간을 직접 소모한다
  (45타일 실측: 합성당 123~140MB, 27~45ms).
- 동기 그룹(lockstep) 도입으로 45타일의 프레임 도착이 한 점에 뭉치며, 시작 직후
  N×3.1MB 통짜 업로드가 30fps 예산(33.3ms)을 넘는 42타일부터 슬로우모드에
  고착된다 (실측: 40타일 4초 탈출, 42타일 12초+).
- 완화책(디코더 스레드 증가, 부분 flush 캡, 시작 마진, 커버 은폐)은 전부
  이 구조를 우회하는 것으로, 사용자 원칙(휴리스틱 금지)에 따라 근본 해결을 택한다.

HW 디코드(NVDEC) 전면 전환 대신 이 설계를 먼저 하는 이유: NVDEC은 세션 수 제한
(소비자 GPU) 또는 엔진 처리량 한계(45×30=1350fps는 엔진 1~2개를 포화 가능)가
있고, SW 디코드는 이미 45타일에서 검증되어 있다. 업로드만 분산하면 SW 디코드
체제로 목표를 달성하며, HW 디코드는 같은 구조 위에서 후속 교체가 가능하다.

## 2. 목표와 성공 기준

렌더러 스레드의 비디오 업로드를 0으로 만든다. 업로드는 각 gst 파이프라인의
자기 스레드에서 수행하고, WR에는 GPU 상주 텍스처 핸들만 전달한다.

성공 기준:
- 45타일: 시작 슬로우모드 소멸 (동기 그룹 릴리즈 후 5초 내 60fps 안착)
- 렌더러 프레임당 Texture cache update ≈ 0ms (WR 프로파일러)
- 기존 검증 항목 유지: 타일 간 ±1프레임 lockstep, gapless 루프, 스톨 0,
  블랙 타일 0 (blacktile_check.ps1)
- 경계 확장 증명: 50~54타일에서도 시작 매끄러움 (기존 42 경계 소멸)
- env 게이트 off 시 기존 경로와 동작 동일 (A/B 공존)

## 3. 확인된 성립 조건 (2026-07-09 검증)

- 번들 ANGLE(libGLESv2.dll)에 `EGL_ANGLE_d3d_texture_client_buffer` 및
  `EGL_ANGLE_image_d3d11_texture` 확장 문자열 존재 — D3D11 텍스처의 무복사
  GL 래핑 경로 확보.
- 시스템 GStreamer(msvc_x86_64)에 gstd3d11.dll 존재, servo-media 레지스트리
  스캐너 화이트리스트에 d3d11 포함. 단 servo 배포 디렉터리 번들에는 미포함 —
  번들 목록 추가 필요.
- WR/Servo에 `ExternalImageType::TextureHandle` + `NativeTexture` 소비 분기가
  이미 존재 (htmlmediaelement의 TextureHandle 분기, MediaExternalImages의
  GLPlayer NativeTexture 경로).

## 4. 아키텍처

### 4.1 파이프라인

```
현재:  servosrc → qtdemux → avdec(SW) → appsink(I420, 시스템 RAM)
설계:  servosrc → qtdemux → avdec(SW) → d3d11upload → d3d11convert(RGBA)
                                         → appsink(caps: video/x-raw(memory:D3D11Memory), RGBA)
```

- d3d11upload: 파이프라인 스트리밍 스레드에서 시스템 RAM → D3D11 텍스처 업로드.
- d3d11convert: YUV→RGBA 변환을 GPU 셰이더로 수행. WR에는 프레임당 RGBA 텍스처
  1장만 전달되어 현행 plane 3개(Y/U/V) 관리와 WR YUV 셰이더 의존이 제거된다.
- 프레임 텍스처는 gst d3d11 버퍼 풀이 공유 가능 플래그(shared handle + keyed
  mutex)로 할당하도록 allocation 파라미터를 지정한다.
- 파이프라인의 GstD3D11Device는 명시적으로 생성해 지정한다 (4.4 멀티GPU 참조).

### 4.2 Servo 핸드오프 (현 Raw 경로의 대칭 교체)

| 현재 (Raw) | 설계 (D3D11) |
|---|---|
| RawVideoFrameExternalImages: external ID → gst 프레임(시스템 RAM) | D3D11 프레임 레지스트리: external ID → (gst 프레임 참조, ID3D11Texture2D/공유핸들, keyed mutex) |
| ImageUpdate: plane 3개 × ImageKey, `ExternalImageType::Buffer` | 프레임당 1개 ImageKey, `ExternalImageType::TextureHandle` |
| lock() → `ExternalImageSource::RawData(&[u8])` → WR이 업로드 | lock() → ANGLE 래핑 → `ExternalImageSource::NativeTexture(gl_id)` → WR은 바인딩만 |
| 렌더러 unlock에서 프레임 drop | 동일 + keyed mutex release |

- htmlmediaelement: D3D11 백엔드 프레임이면 TextureHandle 분기 사용 (기존 GL
  분기와 동형). VideoFrame 추상화에 D3D11 텍스처 변형 추가.
- 코얼레싱(latest-wins)·in-flight 게이트·gapless·동기 그룹은 전부 직교 — 변경
  없이 그대로 동작하며 업로드 비용 항만 사라진다.

### 4.3 ANGLE 래핑 (렌더러 스레드, lock() 내부)

- `EGL_ANGLE_image_d3d11_texture`: D3D11 텍스처 → EGLImage →
  `glEGLImageTargetTexture2DOES` → GL 텍스처 id. 픽셀 복사 없음(µs 단위 API 포장).
- gst 풀이 텍스처를 재활용하므로 래핑 캐시(ID3D11Texture2D 포인터 → GL id/EGLImage)
  를 두어 재래핑을 방지한다. 캐시 무효화: 레지스트리에서 해당 텍스처가 풀에서
  해제될 때(프레임 참조 소멸) 함께 정리.
- 대안 경로(폴백): `EGL_ANGLE_d3d_texture_client_buffer`
  (eglCreatePbufferFromClientBuffer + eglBindTexImage).

### 4.4 동기화와 수명

- keyed mutex 프로토콜: 파이프라인(쓰기) release → 렌더러 lock()에서 acquire(0),
  unlock()에서 release. acquire 실패(타임아웃) 시 이전 프레임 유지(스킵)하고
  경고 로그.
- 프레임 수명: 레지스트리가 최신 프레임의 gst 참조를 보유(현행과 동일한
  latest-wins), 교체 시 이전 프레임 unref → 풀 반환. 렌더러가 lock 중인
  프레임은 locked 맵이 참조를 유지(현행과 동일).

### 4.5 멀티GPU (월 확장)

- 타일의 렌더링 GPU(`requested_gpu_index` → 어댑터 LUID)로 GstD3D11Device를
  명시 생성해 파이프라인에 주입한다. ANGLE 래핑은 동일 어댑터의 디바이스끼리만
  성립하므로 이 선택이 곧 정합성 보장이다.
- 전달 경로: servoshell(wall 레이아웃, GPU 인덱스) → Servo 미디어 초기화 →
  플레이어 생성 파라미터. 단일 모니터 검증 단계에서는 기본 어댑터 사용.
- HW 디코드 후속 교체: 같은 파이프라인 모양에서 avdec → d3d11h264dec(NVDEC).
  구조 불변, caps 협상만 재확인.

## 5. 게이트와 공존

- env `SERVO_MEDIA_D3D11_VIDEO=1` (기본 off). off면 기존 Raw 경로 그대로.
- 실패 폴백: 파이프라인 협상 실패/래핑 실패 시 해당 플레이어는 Raw 경로로
  폴백하고 경고 로그 (전면 실패 대신 타일 단위 강등).

## 6. 검증 계획 (단계별)

1. PoC: 1파이프라인 — d3d11upload→appsink(D3D11 caps)에서 GstD3D11Memory 획득,
   공유핸들 추출, ANGLE 래핑, 화면 표시까지. 상호운용이 최대 리스크이므로
   최우선.
2. 4타일 → 36타일 → 45타일: 기존 도구 재사용 (gridperf_sweep, PresentMon,
   프레임카운터 캡처, blacktile_check).
3. 회귀: env off에서 기존 지표(36타일 61fps/45타일 시작 곡선) 재현.
4. 경계 재측정: 42/45/50/54타일 시작 곡선 — 42 경계 소멸 확인.
5. 멀티GPU: 월 gpu-direct 구성에서 타일별 어댑터 정합 확인 (후속 단계).

## 7. 리스크

| 리스크 | 심각도 | 대응 |
|---|---|---|
| gst d3d11 공유 텍스처 ↔ ANGLE 래핑 상호운용 (디바이스 불일치, mutex 프로토콜, 포맷) | 최고 | PoC 최우선 검증; 실패 시 디바이스 간 GPU-GPU 복사 1회 폴백 설계 |
| gstd3d11.dll 번들 누락/의존성(D3DCompiler) | 중 | 번들 목록 추가 + 로드 확인 |
| appsink caps 변경이 VideoFrame/포지션 보고에 미치는 영향 | 중 | VideoFrame D3D11 변형 추가, 기존 GL 변형 패턴 준수 |
| 45개 파이프라인의 D3D11 디바이스/컨텍스트 자원 (디바이스 공유 vs 파이프라인별) | 중 | 어댑터당 GstD3D11Device 1개 공유(gst 권장) 로 시작 |
| keyed mutex 경합으로 인한 렌더러 대기 | 저 | acquire 타임아웃 0 + 이전 프레임 유지 |

## 8. 비범위 (이번 단계에서 하지 않음)

- HW 디코드(d3d11h264dec) 전환 — 후속 단계
- DirectComposition 오버레이
- 오디오 경로 변경
- Raw 경로 제거 (영구 공존, 기본값 유지)

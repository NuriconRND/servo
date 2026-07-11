# d3d11upload 대체: DYNAMIC 텍스처 직접 업로드 설계

날짜: 2026-07-11
상태: 설계 승인됨 (구현 전)
대상: components/media/backends/gstreamer/render-d3d11 (단일 크레이트 내 완결)
선행 설계: 2026-07-09-d3d11-per-pipeline-upload-design.md (구현·검증 완료 상태에서 출발)

## 1. 배경과 문제

### 1.1 현재 구조와 d3d11upload의 실제 동작 (2026-07-11 소스 확인)

현재 D3D11 비디오 경로는 `avdec(I420 sysmem) → d3d11upload → appsink(D3D11Memory)
→ GstD3D11Converter → RGBA 공유 링`이다. 설치 GStreamer 1.28.4(msvc) 기준
1.28 브랜치 소스로 d3d11upload의 업로드 방식을 확인한 결과, 프레임당 복사가
2단계로 일어난다:

1. **CPU memcpy → staging 텍스처**: d3d11upload의 transform은
   `gst_d3d11_buffer_copy_into`를 호출하고, 입력이 시스템 메모리라 fallback
   경로(`gst_video_frame_copy`)를 탄다 (sys/d3d11/gstd3d11pluginutils.cpp).
   GstD3D11Memory의 CPU map은 메모리마다 별도의 **USAGE_STAGING 텍스처**를
   만들어 `Map(D3D11_MAP_WRITE)` 후 memcpy한다 (gst-libs/gst/d3d11/
   gstd3d11memory.cpp: `gst_d3d11_allocate_staging_texture`,
   `gst_d3d11_memory_map_cpu_access`). unmap은 `NEED_UPLOAD` 플래그만 세운다.
2. **GPU 복사 staging → default**: 변환기가 버퍼를 GPU-map하는 순간
   `gst_d3d11_memory_upload()`가 **`CopySubresourceRegion(default ← staging)`**
   을 디바이스 락 안에서 실행한다 (gstd3d11memory.cpp, 1.28 브랜치 481행).

부가 문제: staging의 `Map(D3D11_MAP_WRITE)`는 DISCARD가 불가능해 직전 프레임의
GPU 복사가 안 끝났으면 Map 자체가 블록될 수 있다.

### 1.2 문제

구형 AMD GPU에서 이 CopySubresourceRegion이 GPU 점유율을 크게 올려 다수 타일
표출에 한계가 발생한다 (현장 관찰; 해당 실기는 다른 장비에 있어 측정은 추후).
복사는 타일 수 × fps에 비례하는 GPU 엔진 시간을 소모한다 (45타일 1080p I420
30fps 기준 약 4.2GB/s의 GPU 내부 복사 트래픽).

### 1.3 왜 d3d11upload를 유지한 채로는 불가능한가

GstD3D11Memory의 CPU map 경로는 DYNAMIC usage를 지원하지 않고 항상 staging을
경유한다 (1.28 소스 확인). 또한 CopySubresourceRegion의 dest는 DYNAMIC이 될 수
없어 풀 텍스처만 DYNAMIC으로 바꾸는 방식도 성립하지 않는다.

## 2. 목표와 성공 기준

업로드를 플레이어별 **DYNAMIC 텍스처 + Map(WRITE_DISCARD) + 행 단위 memcpy**로
대체해 GPU 복사와 staging Map 스톨을 제거한다. CPU 복사 1회는 동일 스레드
(파이프라인별 스트리밍 스레드)에서 그대로 유지한다 — 렌더러 스레드 업로드 0
(선행 설계 달성 사항)은 불변.

성공 기준:
- A4000 월: 기존 검증 항목 전부 유지 (45타일 lockstep ±1, 시작 곡선 5초 내
  60fps, gapless, 스톨 0, blacktile_check 0) + 회귀 없음
- `SERVO_MEDIA_D3D11_UPLOAD=legacy`에서 기존 경로와 동작 동일 (A/B 공존)
- 구형 AMD GPU: GPU 점유율 유의미 감소 또는 동일 점유율에서 타일 수 증가
  (실기 확보 시 측정)

## 3. 검토한 대안

| 안 | 내용 | 판정 |
|---|---|---|
| **A (채택)** | render-d3d11 자체 dynamic 업로드 — d3d11upload 제거, appsink sysmem caps, alloc_wrapped 입력 버퍼 + 기존 변환기 | GStreamer 무수정, 신규 FFI 0, 링 출력 쪽과 대칭 구조 |
| B | GStreamer 소스 패치 + gstd3d11 재빌드 (DYNAMIC 지원 추가) | meson+MSVC 빌드 인프라 신설, 공용 라이브러리 dll 교체·재패치 부담, 회귀 반경 큼 |
| C | 커스텀 GStreamer 엘리먼트(Rust)로 d3d11upload 대체 | A와 효과 동일한데 subclassing 보일러플레이트만 추가 |

## 4. 아키텍처

### 4.1 파이프라인

```
현재:  avdec(I420 sysmem) → d3d11upload ─[CPU memcpy→staging]─[CopySubresourceRegion→default]→
       appsink(D3D11Memory) → GstD3D11Converter → RGBA 링 슬롯

설계:  avdec(I420 sysmem) → appsink(sysmem, 지원 포맷 목록 caps)
       → build_frame(스트리밍 스레드): DYNAMIC 텍스처 Map(WRITE_DISCARD) → 행 단위 memcpy → Unmap
       → GstD3D11Converter → RGBA 링 슬롯 (이하 기존과 동일)
```

- appsink caps를 시스템 메모리 + 지원 포맷 목록으로 협상시키면 디코더가 목록
  밖 포맷을 낼 때 playbin이 videoconvert를 자동 삽입해 목록 내 포맷으로
  맞춘다 → 런타임 포맷 폴백 불필요.
- 업로드·변환 수행 스레드는 현재와 동일한 파이프라인별 스트리밍 스레드.
  파이프라인별 전용 디바이스 구조(락 콘보이 해결)도 불변.

### 4.2 DYNAMIC 텍스처 세트 (플레이어별 1세트, 링 불필요)

`Map(WRITE_DISCARD)`는 드라이버가 내부적으로 메모리를 갈아끼우는(renaming)
방식이라 (1) 이전 프레임을 변환기 draw가 아직 읽는 중이어도 블록되지 않고,
(2) 큐에 남은 draw는 이전 버전을 계속 읽으므로 안전하다. 따라서 입력 텍스처는
caps당 1세트만 만들어 매 프레임 재사용한다.

### 4.3 포맷 → plane 텍스처 매핑 (GStreamer 자체 매핑과 동일)

| 포맷 | plane 텍스처 | DXGI 포맷 |
|---|---|---|
| I420 / YV12 | 3장 (Y 전체, U·V 절반 크기) | R8_UNORM ×3 |
| NV12 | 2장 (Y 전체 + UV 절반) | R8_UNORM, R8G8_UNORM |
| P010_10LE | 2장 | R16_UNORM, R16G16_UNORM |

- NV12/P010을 단일 네이티브 텍스처(DXGI NV12/P010) 대신 plane별 2장으로 만드는
  이유: DYNAMIC usage의 NV12/P010 생성은 드라이버 의존이 크다 (대상이 구형
  AMD인 만큼 확실한 조합만 사용).
- plane 치수는 `GstVideoInfo`의 plane 치수를 사용한다 (홀수 해상도 반올림 포함).
- 텍스처 desc: `D3D11_USAGE_DYNAMIC` + `D3D11_CPU_ACCESS_WRITE` +
  `D3D11_BIND_SHADER_RESOURCE`, MiscFlags 0.

## 5. 컴포넌트 변경

### 5.1 `lib.rs::build_video_sink` — env 분기

- **dynamic (기본)**: bin 없이 appsink를 그대로 video-sink로 설정. caps =
  `video/x-raw`(sysmem) + `format={I420, YV12, NV12, P010_10LE}` + PAR 1/1.
- **legacy** (`SERVO_MEDIA_D3D11_UPLOAD=legacy`): 현재의 d3d11upload bin 그대로.
  `ElementFactory::find("d3d11upload")` 존재 체크는 이 모드에서만 수행 —
  dynamic 모드는 플러그인(gstd3d11.dll) 없이 라이브러리(gstd3d11-1.0-0.dll)만
  필요해 번들 요구사항이 줄어든다.

### 5.2 `interop.rs` 신규 — `DynamicUploadSet` (링과 대칭 구조)

- 보유: plane별 `ComPtr<ID3D11Texture2D>` + 이를 `alloc_wrapped`로 감싼
  GstBuffer 1개 (캐시·재사용 — 변환기가 메모리별 SRV를 내부 캐시하므로 SRV
  생성도 1회).
- `upload(&sysmem_frame)`: plane마다 `디바이스 락 → Map(WRITE_DISCARD) → 언락`,
  락 밖에서 행 단위 memcpy (드라이버 RowPitch ≠ src stride 대응),
  `락 → Unmap → 언락`. GStreamer 자신의 staging 경로와 동일한 락 프로토콜.
- 래핑 버퍼에 VideoMeta(plane stride/offset) 부착 — 변환기가 요구하는지
  PoC에서 확정하고 불필요하면 뺀다.

### 5.3 `lib.rs::build_frame` — 샘플 메모리 타입 분기

- D3D11 메모리 (legacy 협상 결과) → 기존 코드 그대로.
- sysmem → `PlayerState.upload`(caps 변경 시 converter와 함께 무효화·재생성)의
  `upload()` 후, 기존과 동일하게 `converter_convert_buffer(업로드 버퍼, 링 슬롯)`.
  이후 링 finish/공유 핸들 전달 로직 무변경.

### 5.4 FFI (`ffi.rs`)

신규 심볼 0개. CreateTexture2D/Map/Unmap은 링에서 이미 쓰는 직접 COM 호출,
alloc_wrapped·converter는 기존 바인딩 재사용.

### 5.5 계측

기존 D3D11PROF 하트비트에 `upload=` 필드 추가 (업로드 시간 분포 확인용).

## 6. 게이트와 공존

- 상위 게이트 `SERVO_MEDIA_D3D11_VIDEO=1` (기본 off) 불변.
- `SERVO_MEDIA_D3D11_UPLOAD` = 미설정/`dynamic`(기본) | `legacy`(기존
  d3d11upload 경로 복귀 스위치).
- Raw(비 D3D11) 경로는 무변경.

## 7. 에러 처리

- 텍스처 생성·Map 실패: warn 로그 + 프레임 드롭 (기존 converter 실패와 동일
  정책). 세트 생성 실패 시 1회 warn 후 이후 프레임도 드롭.
- 목록 밖 포맷: 협상 단계에서 videoconvert 자동 삽입으로 흡수 (런타임 분기 없음).

## 8. 검증 계획

1. **PoC — 변환기 상호운용 (최우선 리스크)**: `examples/d3d11_upload_poc.rs`에
   dynamic 입력 변형 추가 — 래핑된 다중 plane DYNAMIC 입력 버퍼를
   GstD3D11Converter가 수용하는지, 1타일 화면 표시·색 정확성까지. I420 먼저,
   NV12는 caps 강제 협상으로 확인.
2. **정확성 (단일 창)**: 색상·줄맞춤(RowPitch ≠ stride), 루프 경계, caps 변경
   (해상도 다른 클립 교체) 시 세트 재생성.
3. **회귀 (A4000 월, 기존 도구 재사용)**: 45타일 lockstep ±1, 시작 곡선,
   blacktile_check, gapless, 스톨 0. legacy env로 기존 경로 동작 동일성 확인.
4. **성능 계측 (개발 장비)**: D3D11PROF `upload=` 분포 (기대치: 현행 0.01ms
   수준 유지), `convert=` 변화, GPU 점유율 (PresentMon).
5. **AMD 실기 (추후, 다른 장비)**: 동일 콘텐츠·타일 수 GPU 점유율 전후 비교,
   최대 타일 수 재측정. `D:\ServoWallPackage` 재패키징에 반영.

## 9. 리스크

| 리스크 | 심각도 | 대응 |
|---|---|---|
| 변환기가 래핑 다중 plane DYNAMIC 입력을 수용하지 않음 | 최고 | PoC 최우선 검증 (출력 쪽 래핑은 기검증이라 리스크 낮음); 실패 시 VideoMeta 부착/desc 조정으로 대응 |
| WRITE_DISCARD renaming 품질이 구형 AMD 드라이버에서 낮음 (renaming 대신 동기화) | 중 | 실기 측정으로 확인; legacy 복귀 env 상시 유지 |
| 목록 밖 포맷에서 videoconvert 자동 삽입으로 CPU 증가 | 저 | 현 표출 레시피는 I420 고정; 관찰 항목으로만 유지 |
| Map(WRITE_DISCARD) 매 프레임 드라이버 할당 오버헤드 | 저 | renaming은 드라이버 내부 링 재사용이라 통상 무시 가능; upload= 계측으로 확인 |

## 10. 비범위

- HW 디코드(d3d11h264dec) 전환 — 보류 상태 유지
- GStreamer 소스/바이너리 수정
- Raw(비 D3D11) 경로 변경
- DirectComposition 오버레이
- 멀티GPU 어댑터 정합 (스펙 §4.5 후속 과제와 직교)

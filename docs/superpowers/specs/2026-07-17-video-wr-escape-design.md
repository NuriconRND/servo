# 비디오 WR 콘텐츠 패스 탈출 — WR External Compositor Surface 설계

- 날짜: 2026-07-17
- 상태: 승인됨 (사용자 브레인스토밍 완료)
- 선행: §3-t WR Native Compositor(DComp), §3-u 스왑체인 하이브리드, §3-w 복합 콘텐츠, §3-q A-dyn 제로카피(YUV plane 직접 샘플), §3-z AMD 3중 A/B 판독
- 참조 구현: Gecko의 영상 compositor surface 구조, D:\2_TechReview\20260703_dx_wall_probe\src\dx11_tile_renderer.cpp (변환 셰이더 동형 근거)

## 1. 배경과 목적

§3-z AMD 실측 판독으로 병목이 확정됐다: 대역폭이 아니라 **타일 수/드로우 수에 비례하는 프레임당 제출 오버헤드**(타일별 패스 셋업 + 비디오×타일 교차 draw + ANGLE GL→D3D11 번역세). `-TileSize 3840x3240`으로 타일 배수는 제거했으나 브라우저 고정비 3층(①WR 프레임빌드/배칭 ②ANGLE 번역세 ③외부 이미지 lock 45회/프레임)이 남아 probe와 격차가 유지된다.

본 설계는 **비디오를 WR 콘텐츠 패스에서 탈출**시킨다: WR의 external compositor surface 기구(Gecko가 영상에 쓰는 구조)를 구현해, 영상 프레임을 디코더 → plane 링 → **raw D3D11 변환 draw 1회 → 비디오별 DComp 비주얼 직결**로 표시한다. 비디오 프레임당 ANGLE 0, WR draw 0, 콘텐츠 패스 재실행 0 = probe 실질 동형.

## 2. 완료 기준 (사용자 확정)

**월 + 복합 둘 다.** 즉:
- 45타일 월 전면 프로모션 정상 (lockstep·fps 무회귀, 개발기 A5000)
- mixed_media_demo + complex_media_stress 전 요소 정상 (프로모션·폴백 혼재 포함)
- 게이트 off 완전 무회귀
- ServoWallPackage 재생성 + AMD 판독 가이드 (AMD 실측은 사용자 몫)

## 3. 범위 / 비범위

**범위**
- Servo 레이아웃: 비디오 YuvImage 프리미티브에 프로모션 플래그 설정 (게이트 하)
- dcomp_compositor.rs: external surface stub 3종 실구현
- paint 크레이트: raw D3D11 YUV→RGBA 변환 패스 (probe 동형)
- shared/paint: plane 접근 interop trait + media-thread 구현
- 런처/패키지: `-VideoEscape` 스위치, AMD 3중 A/B 가이드

**비범위 (명시적 이월)**
- 페이싱 우회(media 스레드 직접 Present): WR 컴포짓 페이싱은 유지. AMD 실측에서 WR 프레임빌드 잔존 고정비가 병목으로 확인될 때 후속 증분으로. 본 설계의 비주얼/스왑체인 인프라가 그 착지점이 된다.
- canvas/WebGL/RGBA 이미지 프로모션 (YUV 비디오만)
- HW 디코드 (사용자 보류 유지)
- Draw 모드(-DComp 없음)에서의 프로모션 (플래그 자체를 설정하지 않음)

## 4. 게이트 설계

- 신규 env `SERVO_VIDEO_ESCAPE` (기본 **unset=off**):
  - `native` = C 단계 모드: `PREFER_COMPOSITOR_SURFACE`만 설정. WR이 비디오별 전용 native surface에 자기가 그림(ANGLE 경유, 갱신 시만). 컴포지터 변경 없이 프로모션 거동 검증 + AMD 진단 레버.
  - `external` = 최종 모드: `PREFER | SUPPORTS_EXTERNAL_COMPOSITOR_SURFACE` 설정. 비디오는 WR이 그리지 않고 ExternalImageId가 컴포지터로 직결.
  - 그 외 값/미설정 = off (플래그 미설정).
- `SERVO_COMPOSITOR_DCOMP=1` 또는 `=surface`일 때만 발효. **Draw 모드에서는 어떤 플래그도 설정하지 않음** → 디스플레이 리스트가 현재와 동일 (무회귀 보장 1층).
- 런처(run_video_wall_d3d11.ps1, run_wall.ps1): `-VideoEscape native|external` 스위치 (set-or-clear 관례 — 미지정 시 env 제거).
- AMD 3중 A/B: `-DComp` / `-DComp -VideoEscape native` / `-DComp -VideoEscape external` — 콘텐츠 패스 소멸 이득과 ANGLE 제출세 소멸 이득을 분리 측정.

## 5. 아키텍처

### 5.1 데이터 흐름 (external 모드 최종 상태)

```
[무변경]   디코드(avdec) → plane 링 memcpy (gst 스레드, A-dyn 그대로)
[WR]       YuvImage 프리미티브 + 플래그 → 프로모션 판정
            · 불투명·무마스크 비디오 = underlay (개수 무제한)
            · 콘텐츠 타일엔 투명 컷아웃(z-write)만 기록
            · 비디오 image key는 타일 의존성에서 제외 → 갱신에도 타일 무효화 없음
[컴포지터]  attach된 ExternalImageId → interop으로 plane 링 조회 →
            raw D3D11 셰이더 1-draw (스케일+색변환) →
            비디오별 flip 스왑체인(표시 크기) → Present
[DWM]      콘텐츠 비주얼 + 비디오 비주얼 N개 합성
```

- 비디오 프레임당 비용: **D3D11 draw 1 + Present 1** (ANGLE 0, WR draw 0, 콘텐츠 재실행 0).
- 콘텐츠 패스는 월에서 사실상 정적, 복합 페이지에선 시계/티커 등 비비디오 갱신만.
- 스왑체인은 **표시 크기**(월 213px급)로 생성 — 1080p RGBA 링 부활 없음, 쓰기 대역폭 ~1/25.

### 5.2 페이싱 (무변경)

비디오 갱신 → WR 트랜잭션 → 컴포짓-온리 프레임(래스터 없음, attach+add_surface+Present) 구조 유지. in-flight 합성 게이트(§3-d)·동기그룹·gapless·리드 큐 전부 무변경.

### 5.3 단계 구성

1. **C 단계 (`native`)**: 레이아웃 플래그만. 컴포지터 변경 0 — WR이 만드는 전용 서피스는 기존 create_surface/타일 경로가 수용. 목적 = 프로모션 조건/컷아웃/z-순서를 Gecko 검증 경로 위에서 실기 확인 (문제 발생 시 원인 층이 프로모션으로 격리됨).
2. **A 단계 (`external`)**: SUPPORTS 플래그 + stub 3종 구현 + 변환 패스 + interop. 본선.

## 6. WR 메커니즘 근거 (2026-07-17 소스 실측, webrender-0.68.0)

- 플래그 정의: webrender_api display_item.rs:51 `PREFER_COMPOSITOR_SURFACE`, :55 `SUPPORTS_EXTERNAL_COMPOSITOR_SURFACE`.
- 프로모션 판정: picture.rs:3426-3540 (YuvImage), :2612 `can_promote_to_surface`.
  - Windows는 **underlay 우선** (:3445 `prefer_underlay = clip_on_top || !macos`).
  - **불투명·무마스크 underlay는 개수 제한 없음** — 45타일 전면 프로모션 가능. 마스크 underlay는 1개(:2659), overlay는 `MAX_COMPOSITOR_SURFACES = 4`(:265, 서브슬라이스 상한 :1936).
  - 실패 조건 → Blit 폴백: 비루트 타일 캐시(:2691), 2D scale/translation 아닌 transform(:2701), atomic 슬라이스(:2709), 8192px 초과(:2929 `MAX_COMPOSITOR_SURFACES_SIZE`), overlay+마스크(:2637).
  - 10bit 초과 색심도는 강제 프로모션(:3442 `force`).
- external 경로 조건: picture.rs:2898 — `SUPPORTS_EXTERNAL_COMPOSITOR_SURFACE` + `image_rendering == Auto` + resource cache에 external image 존재(A-dyn plane 발행이 충족) → Y plane(api_keys[0])의 ExternalImageId 사용.
- external surface 수명: :2958 `create_compositor_external_surface(is_opaque)` (크기 무관 키), 매 프레임 :2997 `attach_compositor_external_image`. update_params=None — WR은 그리지 않음.
- 프로모션된 비디오의 image key는 타일 의존성에서 제외(:3484 주석 — 갱신에도 타일 비무효화). 컷아웃 = "transparent z-write"(:3522 주석).
- 컴포지터 인터페이스: dcomp_compositor.rs:2374 `create_external_surface` / :2378 `attach_external_image` — 현재 warn-once stub. `add_surface`(:1731)는 z 순서(underlay→콘텐츠→overlay)로 호출됨 — WR renderer mod.rs:6663 "z-order is implicit based on order added" (external surface도 동일 add_surface 경로, descriptor.surfaces 순회).
- CompositorCapabilities(composite.rs:1373): 기존 값 유지 (`supports_external_compositor_surface_negative_scaling` 기본 true — 음수 스케일 미사용이므로 무관).

## 7. Servo 측 변경 (레이아웃)

- 지점: components/layout/display_list/mod.rs:765-795 — `push_yuv_image` 호출부의 `CommonItemProperties.flags`.
- 게이트 판정(env 2종)은 프로세스당 1회 lazy 읽기. `native`→PREFER만, `external`→PREFER|SUPPORTS.
- `push_image` 폴백 분기(RGBA, :795)는 플래그 미설정 유지 (YUV 비디오만 대상).

## 8. 컴포지터 상세 (A 단계 본체)

### 8.1 storage 종류 추가

기존 가상 서피스/승격 스왑체인/fallback에 더해 `External { swapchain: Option<...>, attached: Option<ExternalImageId>, last_presented_generation, srv_cache }` 추가.

- `create_external_surface(id, is_opaque)`: 비주얼 생성 + External 등록. 스왑체인은 크기 미상이므로 **지연 생성**.
- `attach_external_image(id, external_image)`: 이번 프레임 표시 대상 기록 (WR이 매 컴포짓 호출).
- `add_surface` external 분기: transform(ScaleOffset)에서 표시 크기·위치 산출 → 스왑체인 (재)생성(크기 변경 시) → attach 프레임 세대 ≠ 지난 Present 세대면 변환 draw + Present → 비주얼 위치/클립 설정. z-순서는 기존 `frame_surfaces` AddVisual(FALSE) 흐름 그대로.
- `destroy_surface`: 비주얼+스왑체인+SRV 캐시 해제 (기존 ComOwned Drop 패턴).

**원자성**: 변환·Present는 add_surface 시점, DComp Commit은 end_frame 1회 → 컷아웃 등장과 비디오 첫 표시가 같은 Commit에 묶여 승격 순간 플래시 구조적 불가 (§3-w 원자 교체 원칙).

### 8.2 변환 패스 (paint 크레이트 신규 모듈)

- HLSL 풀스크린 트라이앵글 VS + YUV 샘플 PS. 4포맷: I420(R8×3) / NV12(R8+RG8) / I420_10LE(R16×3, ×64 스케일=Color10) / P010(R16+RG16, Color16). BT.601/709 행렬, limited/full range. 의미론은 WR yuv.glsl·A-dyn 동일 (§3-q ColorDepth 페어링 근거 재사용).
- 디바이스 = DComp 컴포지터가 보유한 ANGLE 하부 D3D11 디바이스. plane 텍스처(DYNAMIC+SHADER_RESOURCE, 동일 디바이스 소속)에 SRV 직접 생성, (텍스처,포맷)별 캐시.
- 스왑체인: flip discard 2버퍼, 표시 크기, 불투명 알파, sync interval 0 (렌더러 스레드 비블로킹 — DWM이 최신 프레임 합성).

### 8.3 plane 접근 interop

`components/shared/paint`에 신규 trait `VideoExternalSurfaceProvider`:

```
acquire(ExternalImageId) → Option<VideoFrameLease {
    plane 텍스처 핸들들, 포맷, 소스 크기,
    색공간/레인지/비트심도, frame_generation }>
release(lease)
```

- 구현: media-thread (A-dyn 링 레지스트리 위). **기존 GL lock()과 동일한 슬롯 잠금 규율** (카운트 0→1, WRITE_DISCARD rename 투명성 PoC 기검증) 재사용.
- 호출은 렌더러 스레드 한정 — 신규 동시성 없음. gst 스레드 무변경.
- 주입: 시작 시 DComp 컴포지터에 등록 (기존 external image handler 배관 패턴).

### 8.4 수명·엣지 케이스

- 리사이즈/드래그: 표시 크기 변경 시 스왑체인 재생성은 크기 안정화 후, 드래그 중엔 비주얼 transform 스케일 임시 표시 (§3-y resize_active와 정합).
- 소스 교체(해상도/8↔10bit): image key·external id 변경 → WR이 external surface 파괴/재생성 → 기존 수명주기 콜백으로 자동 처리.
- 진단: `SERVO_DCOMP_DEBUG=1`에 external 수명주기·Present 카운트 로그 추가. 프로모션 성공/실패는 WR 프로파일러 카운터(COMPOSITOR_SURFACE_UNDERLAYS/BLITS).

## 9. 에러 처리 / 폴백

| 상황 | 처리 |
|---|---|
| 게이트 off | 플래그 미설정 → 디스플레이 리스트 현재와 동일 (즉시 복귀 스위치) |
| 프로모션 실패 (WR 내장) | 해당 비디오만 Blit = 현 A-dyn 콘텐츠 경로. 비디오 단위 혼재 정상 동작 |
| 4K 회전 페이지 (X/Y 플립) | ComplexTransform → 전부 폴백 — **폴백 경로 상시 검증 페이지** |
| acquire() None (플레이어 해체 경합) | 프레임 스킵, 마지막 프레임 유지, destroy_surface가 정리 |
| 디바이스 로스트 | 기존 DComp 디바이스 로스트 경로 편승 |
| 스왑체인 생성 실패 (OOM급) | warn + 다음 프레임 재시도. 지속 실패 시 해당 비디오 구멍 노출 — 기존 DComp 서피스 생성 실패와 동급의 수용 리스크 |

## 10. 검증 계획

**C 단계 게이트**
1. 월 45타일(`native`): 45개 전부 underlay 승격(WR 카운터), 콘텐츠 타일 무효화 정지, 육안+lockstep ±1, 잔상 0.
2. mixed_media_demo + complex_media_stress(`native`): 전 요소 정상 (자막/티커 over 비디오 블렌딩, PiP 승격 또는 폴백, 시계 동작).
3. 게이트 off 무회귀.

**A 단계 게이트**
1. 월 45타일(`external`): lockstep ±1, fps·메모리 플랫, gapless 루프 경계 무결, 30분+ 소크.
2. 복합 2종 전 요소 정상 + 폴백 혼재(4K 회전 페이지) 정상.
3. 10-bit(I420_10LE, jellyfish) 색 정상.
4. 리사이즈/드래그 §3-y 시나리오 재실행 — 잔상/블랙 0.
5. WebGPU 월·게이트 off 무회귀, `=surface` 진단 모드 호환.
6. PresentMon 비디오별 스왑체인 Present 카데이던스 관측.
7. `-TileSize` 상호작용 재측정 (콘텐츠 정적화로 운영 레시피에서 TileSize 확대 불필요 여부).

**패키지/인계**: ServoWallPackage 재생성 + run_wall.ps1 `-VideoEscape` + AMD 3중 A/B 판독 가이드. AMD 실측은 사용자 몫.

## 11. 알려진 리스크 (실측 전 미지수)

- **v1 표시 계약 한계 (계획 수립 중 확정)**: WR은 external surface의 원본(비클립) 표시 크기를 컴포지터에 전달하지 않음(add_surface는 transform+clip_rect만). v1은 dest=clip_rect·UV 0..1 계약 — 비디오가 **부분 클립**(overflow 크롭/스크롤 반노출)되는 페이지에선 크롭 대신 압착으로 표시될 수 있음. 월·mixed·stress 검증 페이지는 전부 무클립이라 무영향. 발현 시 후속(프리미티브 rect 전달 채널) 이월.

- DWM 46개 비주얼 합성 비용 (AMD GCN1 구형) — 스위치 3종(off/native/external)이 판별 레버.
- 렌더러 스레드 45 draw+Present/프레임 비용 — 개발기 선측정.
- C 단계 자체는 일시적 fps 하락 가능 (비디오별 서피스 패스 45개) — 검증 정거장이며 게이트 뒤라 표출 레시피 무영향.
- 페이싱 잔존 고정비(WR 프레임빌드)가 AMD에서 여전히 지배적이면 → 후속 증분(media 직접 Present, §3 비범위)의 판단 근거가 된다.

## 12. 구현 결과 (2026-07-18)

전 태스크 완료 — 월+복합 완료 기준(§2) 전부 충족. 개발기 RTX A5000 실기 검증.

### 12.1 커밋 체인

| 커밋 | 내용 |
|---|---|
| `b8515b129` | Task 1 — 게이트 `SERVO_VIDEO_ESCAPE`(OnceLock 파서) + 레이아웃 YuvImage 프로모션 플래그 + 런처 `-VideoEscape` |
| `91813794c` | Task 3 — `VideoExternalSurfaceProvider` interop trait(paint_api) + 전역 슬롯 + d3d11_ring 접근자 + media-thread provider 구현/등록 |
| `9784eaf37` | Task 4 — `VideoConvertPass`(raw D3D11 YUV→RGBA 1-draw, HLSL, `SwapDeviceContextState` 격리) + WARP E2E 테스트 5종 |
| `206067629` | Task 5 — DComp external compositor surface 실구현: 비디오별 스왑체인 비주얼 직결, 변환 1-draw 연결, attach 세대 dedup, 드래그 중 재생성 억제 |
| `061c7f5d0` | 크래시 근본수정 — plane SRV 캐시 제거(아래 12.2 ①) |

검증 전용 기록 커밋(코드 변경 0): `4e2784432`(Task 2, C단계 native 실기 검증), `1a87b4eb0`(Task 6 최초 실행, A단계 크래시로 BLOCKED), `ab6a784d7`(Task 6 재개, 크래시 수정 후 A단계 전 게이트 PASS).

### 12.2 스펙 대비 이탈

① **SRV 캐시 절대 금지 — 매 변환 신선 생성** (§8.2에 캐시 방침 명시 없었음, 구현 중 발견된 근본 제약). `VideoConvertPass`가 plane 텍스처(`D3D11_USAGE_DYNAMIC`)의 `ID3D11ShaderResourceView`를 프레임 간 캐시·재사용하면, 그 슬롯이 다음 present 전 `Map(WRITE_DISCARD)`로 rename(백킹 버퍼 교체)될 때 캐시 SRV가 이미 해제된 rename 버퍼를 가리키는 dangling 상태가 되어 NVIDIA 드라이버(`nvwgf2umx.dll`)가 커맨드 제출 시 역참조하다 `0xc0000005` AV로 죽는다(동시 비디오 수에 비례해 발현 가속 — 5x5 ≈2초, 4x4 ≈32초, 3x3 이하는 관측창 내 생존). 수정 = SRV를 `convert()`마다 새로 생성하고 draw 직후 unbind+즉시 Release(D3D11 지연 파기로 안전). **재발방지 원칙: `D3D11_USAGE_DYNAMIC`(WRITE_DISCARD) 텍스처에 대한 뷰(SRV/RTV/UAV) 캐싱은 절대 금지 — rename마다 뷰가 낡는다.** native(C단계) 경로가 무사한 이유는 ANGLE이 매 draw 자신의 상태관리자로 뷰를 다시 유도하기 때문(=fresh view와 동치, 실험으로 확인). 상세: `.superpowers/sdd/escape-crash-debug-report.md`(11모드 실험 사다리).

② `frames_logged`(External 서피스별 진단 로그 상한 카운터)·`convert_pass_init_failed`(변환 패스 최초 생성 실패 래치, 매 프레임 재시도 방지) 필드 추가 — §8.1의 `External { swapchain, attached, last_presented_generation, srv_cache }` 스케치에 없던 구현 세부(SRV 캐시 필드 자체는 ①로 인해 제거됨). `dcomp_compositor.rs`.

③ **v1 표시 계약(§11 리스크) 실기 검증 완료, 한정 그대로 확인됨**: `SERVO_DCOMP_DEBUG=1` 브링업 로그에서 `scale=(1,1)`, `clip`=정확히 타일 rect(예: 2x2 그리드 960x540씩 4모서리), `src=1920x1080`(소스 원본 해상도) 전부 계약과 일치 확인 — v1의 dest=clip_rect·UV 0..1 전제가 무클립 페이지(월·mixed·stress)에서는 위배되지 않는다. §11이 지적한 **부분 클립 페이지에서의 크롭-대신-압착 리스크는 검증 페이지 전부가 무클립이라 이번 사이클에서 미발현·미검증 상태로 남는다** — 이월(12.4).

④ PiP류(`complex_media_stress.html`의 `#pipv`) 둥근 모서리(`border-radius`)가 external 경로에서 **사각 클립으로 대체**됨(`[dcomp-native] rounded clip radii unsupported; applying rectangular clip only` warn). v1은 rect 클립만 표현 — 기능 결함 아님, 시각적 사소 저하로 판정.

④-b (최종 리뷰에서 추가 식별된 이탈 2건) §8.4의 "드래그 중 비주얼 transform 스케일 임시 표시"는 **미구현** — 구현은 스왑체인 재생성 억제만 수행하므로, 표시 크기가 창 크기에 연동되는 페이지에선 드래그 중 구크기 콘텐츠가 새 클립 아래 크롭/미충전으로 보일 수 있음(안정화 후 자가 치유; 검증 페이지 전부 고정 픽셀 레이아웃이라 미발현 — Task 6 리사이즈 40스텝에서 재생성 0회). 또한 §8.2의 "flip discard"는 기존 `create_composition_swapchain` 재사용으로 **FLIP_SEQUENTIAL**로 구현됨(매 프레임 전면 재드로우라 기능 등가).

⑤ **native 모드는 진단 전용 — 복합 페이지 PiP 캐비앗**: `-VideoEscape native`에서 알파 패널 위 오버레이 비디오(PiP)가 시작 직후(프레임 약 2) 내장 카운터가 멈추는 프레임 동결 결함 발견(Task 2, `escape-task-2-pip-triage.md`) — WR 자체의 콘텐츠 갱신 계층 결함(§3-w/x/y 비불투명 슬라이스 비디오 계열과 동일 취약점 재발로 추정, 우리 층 버그 아님)으로 트리아지. 순수 비디오 월은 이 결함의 영향을 받지 않는다(PiP류 알파 오버레이가 없음). **`-VideoEscape external`은 동일 시나리오(62초 관측, 장면 전환+카운터 진행 직접 확인)에서 이 동결이 재현되지 않는다** — external이 WR draw 자체를 우회하므로 이 결함 층을 타지 않는 것으로 해석. AMD 가이드(패키지 `run_wall.ps1` 헤더)에 "native는 A/B 진단 전용, PiP류 복합 페이지 프로덕션에는 external 사용" 캐비앗을 명문화했다(12.5 참조).

### 12.3 검증 수치

- **C 단계(native, Task 2)**: 45타일 create_surface +45(비디오 전용 서피스, 213x216 opaque=true). **콘텐츠 타일 무효화 정지**: 하트비트 없는 콘텐츠 서피스 bind 총량 off 23,519회 → native **2회**(약 11,760배 감소).
- **A 단계(external, Task 6 재개) 30분 소크**: 45타일(9x5), Working Set **4651~4720MB로 플랫**(변동 ~1.5%), 32.3분간 크래시 0, 신규 WARN/ERROR 0.
- **복합 승격**: mixed_media_demo 6/6, complex_media_stress **13/13**(PiP 포함) 전부 `create_external_surface opaque=true` 승격. PiP 라이브니스 결정 게이트 **PASS**(위 ⑤).
- **PresentMon**: 46개 스왑체인(비디오45+콘텐츠1) **100% Composed: Flip**.
- **★TileSize A/B 무차이(운영 레시피 변경)★**: `-VideoEscape external` 상태에서 `-TileSize 3840x3240` 유/무 = **29.09fps ↔ 30.39fps**(잡음 수준 차이). 비디오가 이미 WR 콘텐츠 패스(픽처캐시)를 완전히 벗어나 있어 WR 타일 크기가 비디오 프레젠트 비용에 관여하지 않기 때문 — **external 채택 시 `-TileSize` 튜닝은 불요**. `-TileSize` 확대 레시피는 `-DComp` 단독(hybrid) 또는 `-VideoEscape native`에서만 유의미.
- 그 외 전 항목 PASS(10bit 색 정상, 리사이즈/드래그 40스텝 잔상 0, WebGPU 월 무회귀, `=surface` 호환, 게이트 off 무회귀). 상세: `.superpowers/sdd/escape-task-6-report.md`.

### 12.4 이월 사항

- v1 부분 클립 표시 리스크(§11, 12.2③) — 검증 페이지가 전부 무클립이라 미발현. 부분 클립 비디오를 갖는 페이지가 실제 배포에 등장하면 재검증 필요(발현 시 프리미티브 rect 전달 채널 후속).
- PiP `border-radius` 사각 클립 대체(12.2④) — 시각적 사소 저하, 기능 무관. 필요 시 후속 스펙 항목화.
- native 모드 PiP 동결 결함(12.2⑤) 자체의 근본수정 — external로 이미 우회되므로 비긴급, WR 콘텐츠 갱신 계층 문제로 범위 밖 유지.
- 스케일 애니메이션 비디오의 스왑체인 재생성 처닝(2026-07-18 변형 혼재 데모 `complex_media_transforms.html`에서 실측) — v1 계약(스왑체인=클립 크기)상 크기가 매 프레임 변하는 scale 애니메이션 비디오는 스왑체인을 프레임마다 재생성(~35회/s, 표시 무결·비용만). translate 전용은 무처닝(위치만 갱신). 후속 후보: 애니메이션 감지 시 스왑체인 크기 고정+비주얼 스케일 표시.
- 페이싱 우회(media 스레드 직접 Present, §3 비범위) — AMD 실측에서 WR 프레임빌드 잔존 고정비가 여전히 지배적으로 확인되면 후속 증분 착수 근거.
- DWM 46개 비주얼 합성 비용의 실제 AMD 영향 — 개발기(A5000)에서는 무회귀 확인, 구형/저대역폭 GPU에서의 실측은 사용자 몫(AMD 3중 A/B).

### 12.5 인계물

- `D:\ServoWallPackage\run_wall.ps1`: `-VideoEscape native|external` 스위치(런처 관례와 동일한 set-or-clear) + 헤더에 AMD 3중 A/B 판독 절차(영어) — `(1) -DComp` 기준 / `(2) -DComp -VideoEscape native`(콘텐츠 패스 소멸 이득) / `(3) -DComp -VideoEscape external`(ANGLE 제출세까지 소멸), 판독 기준(3>2>1 = 제출 오버헤드 가설 확증, 1↔3 GPU% 비교 필수), TileSize 무요 결론, native 진단 전용 캐비앗 포함.
- `D:\ServoWallPackage.zip` 재생성(servoshell.exe 교체, 나머지 리소스/테스트 페이지 무변경 — `complex_media_stress.html` 기존 포함 확인됨).
- **방법론 노트**: external 스왑체인은 BitBlt/CopyFromScreen으로 실제 캡처 가능(native/hybrid 서피스는 검정 캡처됨, 기존 세션 결론이 native/hybrid 한정으로 정정됨) — 향후 검증의 1차 캡처 수단으로 재채택 가능.

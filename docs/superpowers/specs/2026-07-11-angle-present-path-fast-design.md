# ANGLE present-path-fast로 매 프레임 present 복사 제거 설계

날짜: 2026-07-11
상태: 설계 승인됨 (구현 전)
대상: third_party/surfman/src/platform/windows/angle/device.rs
관련 조사: video-grid-play-heartbeat.md §3-o (RenderDoc 규명)

## 1. 배경과 문제

다중 비디오 월에서 **창을 키우면 구형 AMD GPU의 사용량이 급등하며 성능이 급락**한다
(사용자 현장 관찰). 같은 창 크기에서 네이티브 D3D11 프로브(20260703_dx_wall_probe)는
부드럽게 재생된다.

RenderDoc 캡처(servoshell 2×2, 1920×1080, scratchpad/servo_wall_frame4626.rdc)로
프레임 구조를 규명한 결과:

1. WebRender가 웹 콘텐츠(비디오 타일)를 오프스크린 텍스처(GL FBO 0)에 렌더한다.
2. `eglSwapBuffers`마다 ANGLE이 그 오프스크린을 스왑체인 백버퍼로 **전창
   `CopyResource`(복사)** 한다. 픽셀 히스토리로 백버퍼 중앙(비디오 영역)이
   clear + 이 복사로만 갱신됨을 확인.

이 복사는 ANGLE `SwapChain11.cpp`의 `copyOffscreenToBackbuffer`이며,
`NeedsOffscreenTexture()`가 참일 때만 실행된다:

```cpp
bool NeedsOffscreenTexture(Renderer11 *renderer, NativeWindow11 *nativeWindow, EGLint orientation) {
    return orientation != EGL_SURFACE_ORIENTATION_INVERT_Y_ANGLE &&
           !(renderer->presentPathFastEnabled() && nativeWindow->getNativeWindow());
}
```

`presentPathFastEnabled`는 EGL **디스플레이 생성 속성**
`EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE`로 정해지며 **기본값이 COPY**다
(`Renderer11.cpp:535`). Servo/surfman은 이 속성을 넘기지 않아 매 프레임 전창
복사(창 면적 비례 대역폭: 1080p ≈ 15.7MB RW/frame ≈ 2.6GB/s@60, 4K 2.25배)를
지불한다. 프로브는 `FLIP_DISCARD` 백버퍼에 직접 렌더해 이 복사가 없다.

## 2. 목표와 성공 기준

ANGLE present-path-fast를 활성화해 `NeedsOffscreenTexture`를 거짓으로 만들고, GL이
백버퍼에 직접 렌더하게 해 매 프레임 present 복사를 제거한다.

성공 기준 (개발장비 A4000):
- RenderDoc 재캡처에서 present-time `CopyResource`/`copyOffscreenToBackbuffer` 소멸
  (핵심 지표).
- 화면 상하·좌우 반전 없음, 색 정상 (2×2 및 45타일 육안).
- 회귀 없음: 45타일 lockstep ±1, 블랙 타일 0, 시작 곡선 유지, import 경고 0.
- 격리 WebGL 월(cross-GPU 공유) 블랙 타일 0.
- 창 리사이즈 시 깨짐 없음.

성능 이득(구형 AMD 창확대 fps)은 다른 장비에서 추후 실측 (dynamic 업로드와 동일
패턴). 이 개발장비 검증 통과 시 커밋.

## 3. 확인된 성립 조건 (2026-07-11 검증)

- Servo/WebRender는 서피스 방위(orientation/INVERT_Y/present_path)를 `components/`
  어디서도 참조하지 않음 → present-path-fast의 Y 처리(ANGLE 내부 viewScale 보정)가
  WR에 투명할 것으로 예상.
- present-path-fast가 적용되는 윈도우 서피스는 `SurfaceTexture`로 감싸지지 않음
  (공유 핸들/pbuffer만) → "윈도우 서피스 샘플 불가" 제약 무관.
- 상수 `EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE`(0x33a4),
  `EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE`(0x33a9)는 surfman
  `platform/generic/egl/ffi.rs:34,37`에 이미 정의됨(미사용) → import만 추가.

## 4. surfman 디스플레이 경로 3종 (2026-07-11 확인)

- `device.rs:224` (`new`): LUID 키 **메인 공유 디스플레이**. 컴포지터·월 윈도우가
  사용 — 월의 present 복사가 여기서 발생. **핵심 타깃.**
- `device.rs:294` (`new_isolated` 프로브): 같은 LUID 디스플레이를 어댑터 조회용으로
  생성. ANGLE이 LUID로 디스플레이를 캐시하므로(코드 주석 256행) 속성이 224와
  불일치하면 캐시 키가 갈릴 위험.
- `device.rs:372` (device-ext): 격리 WebGL 컨텍스트용. `EGL_PLATFORM_DEVICE_EXT`
  타입 + 윈도우 서피스 없음(pbuffer만) → present-path-fast 무효과.

## 5. 설계

### 5.1 공용 헬퍼

`fn luid_display_attribs(luid_high, luid_low, driver_type) -> Vec<EGLAttrib>` 신설.
현재 `new`(206–222)와 `new_isolated` 프로브(285–292)의 중복 속성 리스트를 이
헬퍼로 통합하고, 다음 두 항목을 포함한다:

```rust
EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE as EGLAttrib,
EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE as EGLAttrib,
```

WARP(`D3D_DRIVER_TYPE_WARP`)와 LUID 분기는 기존 로직을 그대로 헬퍼 내부로 옮긴다.

### 5.2 호출 경로 통일

- `device.rs:224`(`new`)와 `device.rs:294`(`new_isolated` 프로브) 모두 이 헬퍼를
  호출 → 같은 LUID에 대해 항상 동일 속성으로 `GetPlatformDisplay`가 불려 ANGLE
  디스플레이 캐시 키가 일치한다.
- `device.rs:372`(device-ext)는 무변경.

### 5.3 동작 결과 (ANGLE 내부, 추가 코드 없이)

```
현재(COPY):  WR → 오프스크린 텍스처 → eglSwapBuffers마다 CopyResource(오프→백버퍼) → Present
설계(FAST):  WR → 백버퍼 직접 렌더 (viewScale로 Y 보정) → Present
```

`NeedsOffscreenTexture()`가 거짓이 되어 `copyOffscreenToBackbuffer` 미실행. Y축은
ANGLE `StateManager11` viewScale(+1)로 내부 보정 → WR/surfman 코드 변경 없음.

### 5.4 게이트

없음. 무조건 적용(기본 on) — 사용자 결정. present-path-fast 활성 여부는 런타임에서
신뢰성 있게 재조회 불가하므로 런타임 자동 폴백도 두지 않는다.

## 6. 리스크와 대응

| 리스크 | 심각도 | 대응 |
|---|---|---|
| Y축 상하 반전 (WR/ANGLE 조합이 방위에 암묵 의존) | 최고 | RenderDoc + 육안 검증; 반전 시 present-path-fast 철회(헬퍼 되돌림) 후 대안 재검토 — Y-flip 보정 코드 억지 삽입 금지(휴리스틱 금지 원칙) |
| ANGLE LUID 디스플레이 캐시 불일치 → 격리 WebGL 블랙 타일 | 중 | 헬퍼로 224/294 속성 일치; WebGL 월 스모크로 블랙 타일 0 확인 |
| 리사이즈 시 present-path-fast 깨짐 (ANGLE 이력) | 저 | 창 리사이즈 1회 스모크 |
| MSAA/윈도우 서피스 샘플 제약 | 저 | 해당 없음 확인됨(§3); 회귀로 재확인 |

## 7. 검증 계획 (개발장비 A4000)

1. `mach build --release` 후 RenderDoc 재캡처 → present-time 복사 소멸 확인
   (renderdoccmd 주입 + F12; scratchpad/analyze_rdc*.py로 CopyResource 부재 확인).
2. 육안: 2×2, 45타일 — 상하·좌우 정상, 색 정상.
3. 회귀: 45타일 lockstep ±1, blacktile 0, 시작 곡선, import 경고 0
   (run_video_wall_d3d11.ps1 재사용).
4. 격리 WebGL 월 스모크 — 블랙 타일 0.
5. 창 리사이즈 1회 — 깨짐 없음.

## 8. 비범위

- DirectComposition 코드 변경 (이미 존재, 직교)
- 렌더 스케일 캡 / WR 합성 구조 변경
- env 게이트
- AMD 실기 측정 (추후, 다른 장비)
- dynamic 업로드 경로 (§3-n, 별개 완료 항목)

## 9. 부기 (2026-07-11 구현·검증 후) — §7의 RenderDoc 기준은 무효

§7-1의 "RenderDoc 재캡처로 복사 소멸 확인"은 **달성 불가능한 잘못된 기준**이었다.
**RenderDoc은 캡처를 위해 ANGLE을 offscreen+CopyResource 경로로 강제**한다 —
baseline / present-path-fast / present-path-fast+DComp-off 세 구성 캡처 모두 동일한
매 프레임 CopyResource(Texture83→Backbuffer)를 보였다. 즉 RenderDoc(주입형 그래픽
디버거)으로는 이 변경의 효과를 관측할 수 없다.

**대체 검증법(실제 사용, 더 견고함):** present-path-fast가 켜지면 ANGLE이
`EGL_ANGLE_surface_orientation` 확장을 **광고하지 않는다**
(Renderer11.cpp:1337 `surfaceOrientation = !mPresentPathFastEnabled`).
surfman `Device::new`에 임시 진단 `eprintln!`을 넣어
`eglQueryString(display, EGL_EXTENSIONS)`에 해당 확장이 없음을 확인 →
`present_path_fast_engaged=true` (진단은 검증 후 되돌림). 이는
`NeedsOffscreenTexture`가 소비하는 바로 그 플래그의 직접 판독이며, HWND 윈도우
서피스에서는 이 플래그가 참이면 복사 분기가 ANGLE 코드상 도달 불가다.

**실제 수행한 검증(전부 PASS):** present-path-fast 활성(위 진단, 비디오월+WebGPU월);
Y반전 없음/색 정상(2×2·45타일 비디오·WebGPU retargeting 월 스크린샷); 45타일 회귀
(FAIL=0, 45/45 마커, import 0, lockstep ±1); 격리 디바이스/WebGPU 월(60fps, 블랙
타일 0 = 캐시 일관성); 리사이즈(재배치 정상). AMD 창확대 fps 이득만 추후 실기.

**후속 독립 확인 도구(원할 시):** RenderDoc 말고 ETW/GPUView 또는 D3D11 debug-layer
트레이스 — 주입 없이 관측하는 도구를 쓸 것.

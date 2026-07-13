# WR Native Compositor (DirectComposition) 설계

날짜: 2026-07-13
상태: 승인 대기
선행 문서: 2026-07-12-wr-yuv-direct-sample(A-dyn 제로카피), 2026-07-11 present-path-fast 부기

## 1. 배경과 문제

구형 AMD GPU(HD 7800M, GCN1, 메모리 대역폭 ~70GB/s) 실기에서 servoshell 창을
확대하면 probe(decode-copy-dyn) 대비 GPU 점유율이 과다하고 성능이 급락한다.

근본원인은 확정되어 있다(2026-07-13, WR 내장 프로파일러 실측):

- WR의 picture caching은 화면을 타일(기본 1024×512)로 나눠 캐싱하지만,
  비디오 월 워크로드는 매 프레임 모든 타일이 무효화된다.
- 그 결과 매 프레임 2단 draw가 재실행된다:
  ①콘텐츠→타일(창면적 쓰기) + ②타일→백버퍼(창면적 읽기+쓰기).
- 합계 창면적 ~3× 트래픽 vs probe 1×(디코드→백버퍼 직행 draw 1회).
- 판정 근거: Rendered picture tiles 6/frame(1936×1119 창, 창면적 비례),
  Alpha passes 0, Texture cache 0.01ms(제로카피 건재). 경로 가설(ppf 미발동)과
  프레젠트 가설(PresentMode 차이)은 실기 로그·PresentMon으로 기각됨.
- WR 0.68에는 picture caching을 끄는 스위치가 없다.

②단을 없애는 정공법이 WR이 이미 제공하는 Native Compositor 인터페이스다:
`CompositorConfig::Native`(webrender-0.68 composite.rs:314)에 `Compositor` trait
(composite.rs:1446) 구현체를 넘기면, WR은 타일을 embedder가 만든 OS 서피스에
직접 그리고 ②단 draw를 OS 컴포지터(DWM) 합성으로 대체한다. Gecko가 Windows에서
프로덕션으로 쓰는 경로이며, `NativeSurfaceInfo` 문서가 ANGLE+DirectComposition
조합을 1급 시나리오로 명시한다(pbuffer가 기본 프레임버퍼가 되므로 fbo_id=0).

## 2. 목표와 성공 기준

목표: 게이트 on 시 매 프레임 GPU 트래픽을 창면적 ~3×에서 ~1×로 — probe와
동형 구조. AMD 실기에서 창 확대 시 급락 해소.

이번 사이클 완료 기준(사용자 확정):

1. 개발기(NVIDIA A5000) 기능 무결 — 색/방향/45타일 lockstep/리사이즈/메모리
2. 구조 계측 — WR 프로파일러에서 ②단(composite draw) 비용 소멸 확인
3. 게이트 off 무회귀 — 현행과 동일 경로
4. ServoWallPackage 재패키징 + AMD 판독 가이드 동봉

AMD 실기 실측(창 확대 시 GPU%/fps probe 동급)은 사용자가 패키지로 진행한다
(§3-n dynamic upload, §3-p present-path-fast와 동일 패턴).

## 3. 범위 / 비범위

범위(사용자 확정: "월 표출 전용 최소 범위"):

- env 게이트 `SERVO_COMPOSITOR_DCOMP=1`, 기본 off. off면 현행과 바이트 동일.
- 단일 창. 단, 설계는 창(painter) 단위 인스턴스로 하고 전역 상태를 만들지
  않는다 — 다중 창 확장 시 이 코드는 무수정이 원칙(§4.5 해결 후 검증만 추가).
- 리사이즈/창 이동 동작.
- 게이트 on 시 egui 크롬(툴바)은 topmost 웹콘텐츠 비주얼에 가려진다 — 허용.
  월 표출은 런처가 URL을 인자로 넘기므로 운용 지장 없음.

비범위(명시적 제외):

- egui 크롬 표시(별도 DComp 레이어化) — 후속 과제
- 다중 창 검증 — §4.5 per-device 레지스트리 선결
- 비디오의 DComp 오버레이 승격(`attach_external_image` 활용) — §10 참조
- HW 디코드, 스크린샷 API(WebDriver류) 호환, 기본 on 전환(AMD 실측 후 판단)

## 4. 대안 검토와 결정

- A안(채택): `CompositorConfig::Native` + DirectComposition 구현.
  ②단 소멸을 정공법으로 달성, Gecko 프로덕션 검증 경로, WR 무수정
  (crates.io 0.68 그대로, vendoring 불요). 구현량 A-dyn급.
- B안(기각): vendored WR 핵 — 전면 dirty 시 타일 우회 직행 draw.
  WR 콘텐츠 패스가 타일 타깃 단위 설계라 "직행"은 draw 경로 재구성이 필요해
  실규모 불확실 + vendoring 유지보수 부담(과거 의도적으로 제거) + 업그레이드
  취약. 실험용 분류.
- C안(기각): 운용 제한(창 크기/해상도 제한). 문제를 풀지 않음.

## 5. 아키텍처

### 5.1 초기화 (painter.rs 렌더러 생성 시, 게이트 on일 때)

1. RenderingContext 인터롭으로 ANGLE의 D3D11 디바이스와 창 HWND 획득
2. 그 디바이스로 `DCompositionCreateDevice` → `CreateTargetForHwnd(topmost=TRUE)`
   → 루트 비주얼 생성
3. `WebRenderOptions.compositor_config = CompositorConfig::Native { compositor }`

게이트 off면 compositor_config를 건드리지 않음(기본 Draw) = 현행 그대로.

### 5.2 프레임 흐름 (게이트 on)

```
[콘텐츠 패스 — 기존 ①단, draw 대상만 타일 텍스처 → DComp 서피스로 변경]
변경된 타일마다:
  WR → bind(NativeTileId, dirty_rect)
    impl: 가상 서피스 BeginDraw(rect) → ID3D11Texture2D + 오프셋
          → EGL pbuffer 래핑(EGL_ANGLE_d3d_texture_client_buffer)
          → eglMakeCurrent → NativeSurfaceInfo{origin=오프셋, fbo_id: 0}
  WR이 GL로 타일 콘텐츠 draw   ← 제로카피 비디오 텍스처 샘플링 무변경
  WR → unbind → impl: EndDraw (+pbuffer 해제)

[합성 — 기존 ②단 draw가 이것으로 대체]
  WR → begin_frame
  WR → add_surface(서피스 순서대로) → impl: 비주얼 오프셋/클립 갱신
  WR → end_frame → impl: DComp Commit
  → DWM이 타일 서피스를 화면에 직접 합성 (앱 GPU draw 0)
```

45타일 월: 매 프레임 전 타일 dirty → 콘텐츠 draw 창면적 1×, ②단 0.
DWM 합성은 현재도(Composed: Flip) 일어나므로 순증 비용이 아니다.

egui/창 백버퍼: servoshell 무변경. egui는 기존대로 창 백버퍼에 그려지고
present되지만 topmost DComp 트리가 웹콘텐츠로 덮는다.

### 5.3 서피스 전략

Gecko DCLayerTree와 동일: WR 서피스(picture cache 슬라이스)당
`IDCompositionVirtualSurface` 1개 + `IDCompositionVisual` 1개. 타일은 가상
서피스 내 영역으로 관리(BeginDraw(rect)). 슬라이스는 보통 1~3개라 비주얼
트리가 작다. `CompositorCapabilities.virtual_surface_size`로 WR에 통지
(값은 Gecko 참조, 계획 단계 앵커).

pbuffer는 BeginDraw마다 생성/EndDraw 시 해제부터 시작(Gecko 방식). 캐시는
측정 후 최적화 후보.

## 6. 컴포넌트와 통합 지점 (코드 앵커)

1. `DCompNativeCompositor` (신규, components/paint/ 내 cfg(windows) 모듈)
   - 상태: DComp 디바이스·타깃·루트 비주얼,
     `NativeSurfaceId → {가상 서피스, 비주얼, 타일 크기, 불투명 여부}` 맵
   - `Compositor` trait 전체 구현. 단 외부 서피스 3종(create_external_surface /
     attach_external_image / create_backdrop_surface)은 미사용 stub(warn) —
     embedder가 `prefer_compositor_surface`를 세울 때만 발동하며 Servo는 안
     세움(계획 단계 grep으로 최종 확정).
   - DEBUG_OVERLAY 서피스(NativeSurfaceId::DEBUG_OVERLAY)는 일반 서피스와
     동일 취급 → Ctrl+F12 프로파일러 유지.

2. RenderingContext 인터롭 확장 (2026-07-12 A-dyn에서 확립한 패턴 그대로:
   components/shared/paint/rendering_context.rs trait 기본 메서드 + vendored
   surfman(third_party/surfman) 경유 구현 + Window/Offscreen 위임 5지점)
   - 추가 접근자: ANGLE D3D11 디바이스(DComp 디바이스 생성용), D3D 텍스처→
     EGL pbuffer 래핑/해제/make-current 헬퍼, 창 HWND.
   - 근거 확보됨: pbuffer 래핑(SwapChain11.cpp:319)은 무조건 RTV — BeginDraw
     텍스처는 RENDER_TARGET이므로 요구사항과 정확히 일치(A-dyn 조사에서 확인).

3. painter.rs (통합 지점 1곳, create_webrender_instance 호출부 :361 부근)
   - 게이트 판정 → 인터롭 재료 획득 → 컴포지터 생성 → compositor_config 전달.
   - 생성 중 임의 단계 실패 시 warn + Draw 폴백.

4. servoshell 무변경. run_wall.ps1 / run_video_wall_d3d11.ps1에 `-DComp`
   A/B 스위치 추가(검증·패키징 단계).

5. DComp API 바인딩: winapi 0.3 `um::dcomp` 사용 예정. IDCompositionVirtualSurface
   커버리지는 계획 단계 앵커 — 부족하면 `windows` 크레이트 보충.

의존 관계: 이 기능은 D3D11 제로카피 비디오 경로와 독립(콘텐츠 패스 무변경).
표출 레시피는 게이트 조합: `SERVO_MEDIA_D3D11_VIDEO=1 + SERVO_COMPOSITOR_DCOMP=1
+ GAPLESS + SYNC_GROUP + WIN_VSYNC + ...`(기존 레시피에 DComp만 추가).

## 7. 에러 처리·폴백·해체

- 초기화 실패(어느 단계든) → warn + Draw 폴백. 게이트 on이어도 화면이 안
  나오는 상황은 만들지 않는다.
- 런타임 실패(BeginDraw/eglMakeCurrent)는 실질적으로 디바이스 로스트(TDR)뿐
  — 현행 경로도 동일하게 취약. 로그 + 패닉 회피(해당 프레임 포기) 수준 방어.
- make-current 규약: bind가 pbuffer를 current로 변경. 프레임 종료 후 painter의
  present 경로에서 make_current로 창 서피스를 복원한다(정확한 복원 지점은 계획
  단계 앵커 §11-7에 포함 — WR render() 이후 present 흐름에서 확정).
- 해체 순서(A-dyn surfman UAF 교훈): 컴포지터 deinit은 WR renderer deinit
  내부 → egl.Terminate 이전. 이 시점에 DComp 객체(ANGLE D3D11 디바이스 참조
  보유) 전부 Release. 정상 종료가 회귀 가드를 겸한다.

## 8. 리스크와 완화

1. WR Native 경로 × Servo(ANGLE+surfman) 조합은 최초 — Gecko 검증 경로지만
   우리 환경 특이점 가능. 완화: PoC 게이트를 계획 초반 배치(DComp 가상 서피스
   1장에 pbuffer로 삼각형 렌더 → 화면 표시 확인). PoC 실패 시 HALT + 사용자
   문의(A-dyn §6과 동일 규칙).
2. RenderDoc 검증 불가 가능성(present-path-fast 함정과 동일 계열 — RenderDoc이
   ANGLE 동작을 바꿈). 판정 도구를 처음부터 WR 내장 프로파일러 + PresentMon +
   GPU% A/B + 육안으로 설계.
3. 입력: DComp 비주얼은 히트테스트 투명 — HWND가 기존대로 입력 수신. 무영향.
4. 페이싱: Commit은 비동기(다음 vblank DWM 반영). WIN_VSYNC=1 레시피 유지,
   in-flight 합성 게이트는 render() 완료 기반이라 무영향.
5. 기존 검증 도구: CopyFromScreen 계열(blacktile_check 등)은 DWM 합성 결과
   캡처라 유효. PrintWindow류만 무효.
6. 메모리: 타일 저장소가 GL 텍스처(picture cache)에서 DComp 서피스로 이동 —
   순증 근소. 45타일 소크에서 플랫 확인.

## 9. 검증 계획 (개발기 NVIDIA A5000)

1. PoC 게이트(§8-1). 실패 시 HALT.
2. 기능 무결(게이트 on): 2×2 비디오 색/방향/재생 → 45타일 lockstep ±1·루프
   무결·메모리 플랫·FAIL 0 → 리사이즈/창 이동 → WebGPU 월 페이지 →
   Ctrl+F12 오버레이 표시.
3. 구조 계측: WR 프로파일러 ②단 비용 소멸 확인, 게이트 on/off × 창 크기
   스윕으로 GPU%·renderer 시간 A/B(NVIDIA에서도 창면적 계수 감소 방향성 관측
   가능), PresentMon 저더 무회귀.
4. 게이트 off 회귀: 현행 동일 경로 확인(스팟 fps + 45타일 마커).
5. 패키징: ServoWallPackage 재패키징, run_wall.ps1 `-DComp` 스위치, AMD 판독
   가이드(창 확대 GPU% A/B 절차) 동봉.

## 10. 미래 확장 (기록만, 이번 비범위)

- 비디오 DComp 오버레이 승격: `create_external_surface` + `attach_external_image`
  로 비디오 프레임을 콘텐츠 draw 없이 DComp가 직접 표시 — ①단마저 비디오
  영역만큼 줄이는 다음 단계. NV12/RGBA 표시 가능 형식 변환이 필요해 별도
  프로젝트(현 제로카피는 R8/R16 plane 텍스처라 직접 표시 불가).
- egui 크롬의 DComp 레이어化(툴바 표시).
- 다중 창: §4.5 해결 후 검증 추가(코드는 창 단위 인스턴스라 무수정 원칙).

## 11. 계획 단계 앵커 항목 (writing-plans에서 확정할 것)

1. winapi 0.3 `um::dcomp` 커버리지(특히 IDCompositionVirtualSurface,
   CreateTargetForHwnd) — 부족 시 windows 크레이트 보충 결정.
2. Servo가 `prefer_compositor_surface`를 세우지 않음을 grep으로 확정
   (외부 서피스 stub의 안전 근거).
3. `CompositorCapabilities.virtual_surface_size` 값(Gecko DCLayerTree 참조)과
   나머지 capabilities 필드 값.
4. HWND 접근자 — surfman NativeWidget(angle 백엔드)에서 노출 경로.
5. eglCreatePbufferFromClientBuffer(EGL_D3D_TEXTURE_ANGLE) 시그니처와 surfman
   내 기존 로드 여부.
6. WR 프로파일러에서 ②단 소멸을 판독할 지표 이름(Composite time 등) 실측.
7. WR renderer가 Native 모드에서 창 서피스에 접근하는 잔여 경로(스크린샷/
   readback/clear) 유무 확인 — present/이벤트 루프와의 상호작용 포함.
8. `enable_native_compositor(false)` 런타임 토글이 호출되는 조건과 대응.

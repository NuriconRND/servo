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

## 12. 구현 결과와 이탈 (2026-07-13)

Task 1-6로 게이트(`SERVO_COMPOSITOR_DCOMP=1`) 구현·검증 완료(HEAD `2f0f449d4`).
아래는 완료 기준(§2) 대비 결과와, 계획에 없던 설계 이탈 3건이다.

### 결과 요약

- **개발기(NVIDIA A5000) 기능 무결** — 전부 PASS: 2×2 비디오(색/방향/lockstep
  ±0), 45타일(45/45, lockstep ±0~1, 블랙타일 0, 루프 무결, 5분 메모리 플랫
  ±2.3%), WebGPU 월(3D 캐릭터 2체, 70-116fps, 에러 0), 저더(64초, fps avg
  61.5/min 57.5, maxGap avg 25ms, drop 0), 리사이즈/모니터 이동/급resize 6단
  (무크래시), 정상 종료 3+회(좀비 0).
- **②단(타일→백버퍼 draw) 소멸 — 존재 증명 완료.** `SERVO_DCOMP_DEBUG=1` 로그로
  매 프레임 전 타일이 `create_surface`→`bind`(BeginDraw)→`add_surface`(AddVisual)
  경로로 DComp에 합성되고(경고/실패 0), 동시에 `[dcomp-native] window present
  skipped` — WR이 더 이상 타일→창 백버퍼 최종 합성 draw를 하지 않음이 구조적으로
  확인됨. WR 프로파일러 수치 채집은 게이트 on에서 표시되지 않아(§9-2 이탈 3
  참조) 불가했고, 대신 이 로그 기반 존재 증명으로 대체.
- **게이트 off 무회귀 — PASS.** 마커 부재, d3d11 45/45, fps avg 62.4(on의
  61.5와 동급). off는 현행과 바이트 동일 경로.
- **AMD 실기 GPU% 실이득은 사용자 몫.** 개발기(A5000)의 GPU% A/B는 45타일
  decode가 GPU 점유를 지배해(off avg 24-30% / on avg 24-32%, 표본 분산
  17-71%) 창면적 계수의 깨끗한 신호를 얻지 못했다(다만 on/off 평균 동급 = 게이트로
  인한 GPU 회귀는 없음). §1의 원 문제(구형 AMD 대역폭 병목)에 대한 실측은 패키지로
  사용자가 진행(런처 AMD 판독 가이드 참조).
- **패키징 완료.** `run_video_wall_d3d11.ps1` / `D:\ServoWallPackage\run_wall.ps1`
  양쪽에 `-DComp` 스위치 + 마커 자동 검증(2종) 추가, `ServoWallPackage.zip`
  재생성(exe만 교체 — DLL/리소스 무변경, surfman/paint는 정적 링크).

### 설계 이탈 / 발견 3건

1. **ANGLE present-path-fast(ppf)가 pbuffer에도 발동해 top-left 규약을 깨뜨림
   (계획에 없던 상호작용, §5.2 갱신).** ANGLE의 `UsePresentPathFast()`는
   `attachment->type() == GL_FRAMEBUFFER_DEFAULT`로 발동 여부를 판정하는데,
   render-pbuffer를 current로 만들면 그것 자체가 `GL_FRAMEBUFFER_DEFAULT`가 되어
   ppf가 타일 pbuffer 렌더링에도 발동했다. 발동 시 viewScale 수직 무반전 +
   시저 y 자동 반전이 걸려 WR NativeSurface의 `ortho(bottom=0,top=h)` 투영
   전제(stock ANGLE, viewScale −1)가 깨지며 타일이 수직으로 흩어졌다. 원래
   §3-p present-path-fast는 "창 present의 offscreen→backbuffer 복사 제거"가
   목적이었는데, 게이트 on에서는 창 present 자체를 스킵하므로 그 목적이
   무의미해진다는 점에 착안해 해소: **게이트 on이면 surfman이 ppf 관련 EGL
   디스플레이 속성(`EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE`/`FAST`)을 애초에
   요청하지 않고, `WindowRenderingContext::present()`는 실제 컴포지터 발동
   여부(env가 아님)를 기준으로 스킵한다.** 커밋 `d0486e4a3`(surfman ppf 게이트)
   + `e87765943`(present 스킵) + `2f0f449d4`(스킵 판정을 env→실발동 기준으로
   교정, 리뷰 픽스). PoC(Task 3)는 clear가 방향 무관 + 시저가 PoC 자체 flip과
   ANGLE 재반전이 상쇄돼 이 버그를 못 잡았다 — Task 5 애니메이션 4분면 재검증에서
   발견·수정.
2. **ANGLE 창 서피스가 HWND의 DComp 타깃을 선점 — Task 1 opt-out이 있어야만
   성립(계획에 이미 있었으나 PoC에서 필수 전제로 실증됨).** surfman의 기본
   ANGLE 창 서피스 경로는 `EGL_DIRECT_COMPOSITION_ANGLE=TRUE`로 만들어져
   HWND에 ANGLE 자신의 DComp 타깃을 붙인다. `IDCompositionDevice::
   CreateTargetForHwnd`는 (hwnd, topmost)당 1개만 허용되므로, 그 위에 네이티브
   컴포지터가 또 타깃을 만들면 `hr=0x88980800`
   (`DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED`)로 거부된다(Task 3 PoC 최초
   실행에서 실제로 재현). 해법은 §6-4에 이미 설계돼 있던 opt-out
   (`SERVO_COMPOSITOR_DCOMP=1`이면 창 서피스를 DirectComposition 속성 없이
   "평범한 HWND 서피스"로 생성 — Task 1)이며, 이 게이트가 없으면 네이티브
   컴포지터 자체가 초기화 단계에서 성립하지 않는다는 것이 PoC로 실증됐다(설계
   변경이 아니라 "이미 있던 전제 조건의 필수성 확인" — 기록 목적으로 이탈에
   포함).
3. **게이트 on에서 Ctrl+F12 WR 프로파일러 오버레이가 표시되지 않음 —
   미해결·이월(§9-2 검증 항목 결함, 사용자 결정 대기).** 근본원인은 규명됨:
   콘텐츠 타일은 `is_opaque=true`(DXGI_ALPHA_MODE_IGNORE)라 정상 표시되지만,
   `DEBUG_OVERLAY` 서피스(`NativeSurfaceId::u64::MAX`)만 `is_opaque=false`
   (DXGI_ALPHA_MODE_PREMULTIPLIED)로 생성되어, WR 디버그 렌더러가 그 비불투명
   서피스에 그린 결과가 premultiplied 합성에서 사실상 투명하게 합성된다
   (`SERVO_DCOMP_DEBUG=1`으로 서피스 자체는 매 프레임 정상 처리됨을 확인 —
   create_surface/bind/add_surface 경고 0). 정확한 GL 레벨 메커니즘(straight
   vs premultiplied 알파 규약 차이)은 RenderDoc급 조사가 필요하나, RenderDoc은
   §8-2/§3-p와 동일 계열 함정(ANGLE 동작을 바꿔 판독 불가)으로 이 프로젝트
   범위에서 배제된 도구다. **진단 전용 결함이며 월 기능(비디오/WebGPU/저더/
   리사이즈 전부 PASS)에는 무영향.** 수정은 통과 중인 합성/pbuffer 공유 경로를
   건드려야 해 회귀 리스크가 있고 이득은 진단 전용이라, 이번 사이클에서는
   수정하지 않고 원인 규명과 함께 후속(§10 "egui 크롬의 DComp 레이어化"와 동류
   작업)으로 이월한다. **사용자 결정 대기.**

### 잔여 Minor (이번 사이클 스코프 밖, 수정하지 않음)

- 게이트 on 시 창 하단에 egui 툴바 높이만큼 흰 밴드(webview 뷰포트가 툴바
  높이를 제외한 크기인데 DComp 트리는 창 원점부터 그림) — §3 "egui 크롬은
  topmost 웹콘텐츠에 가려짐" 허용 범위의 코롤러리, 월 운용 무영향.
- `set_dcomp_native_active(true)` 호출 지점이 코드베이스 전체에서 painter.rs
  단 한 곳(성공 분기)이라는 불변조건은 주석으로만 남아 있고 타입 레벨 보장은
  없음(그럴 만큼 코드가 아직 작아 과설계로 보류).
- `no-wgl`(paint-api 피처) ↔ `sm-angle`(surfman 피처) 커플링 — 게이트 관련
  코드 대부분이 `#[cfg(all(target_os = "windows", feature = "no-wgl"))]`로
  게이팅되는데, servoshell은 항상 두 피처를 함께 켜므로(Cargo.toml) 실질
  문제는 없으나 두 피처가 별도 크레이트에 선언돼 커플링이 암묵적이다.

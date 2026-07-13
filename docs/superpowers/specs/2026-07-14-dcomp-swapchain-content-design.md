# DComp 스왑체인 콘텐츠 + 레이어 컬링 설계

날짜: 2026-07-14
상태: 승인 대기
선행 문서: 2026-07-13-wr-native-compositor-design (DComp 네이티브 컴포지터 — 본 설계의 기반)

## 1. 배경과 문제

WR Native Compositor(DComp, 2026-07-13 완료) AMD 실기 A/B 결과:
**GPU 사용량은 개선됐으나, 창이 크고 동영상이 많으면 프레임 드랍이 잔존.**

판별 체인(전부 실측):

1. **동일 조건(같은 창 크기·동영상 수)에서 probe(decode-copy-dyn)는 유지, Servo만
   드랍** → 병목은 비디오 읽기/디코드가 아니라 Servo 합성 구조에 잔존. 따라잡을
   여유가 실재.
2. **`-TileSize 1920x1080`(타일 1장) 실험 — 차이 없음** → per-tile 횟수(바인딩
   왕복 수)는 무관.
3. **프레임당 전면(1920×1080) DComp 레이어 2장**(슬라이스 0·2, 둘 다 불투명)이
   add_surface됨을 개발기 로그로 실측 — probe는 1레이어.

남은 용의자 2개로 수렴:

- **①DComp 가상 서피스 갱신 기구**: 창면적 콘텐츠를 매 프레임 BeginDraw/EndDraw로
  DWM 소유 메모리에 대여/반납하는 프로토콜의 숨은 비용(내부 동기화·아틀라스 관리 —
  타일 수 무관인 점과 정합).
- **②DWM 다중 레이어 합성**: probe 1장 vs Servo 2장 = DWM 단계 창면적 읽기 2배.

사용자 방향 제시: "중간 캐시를 제거하고 display list를 백버퍼에 직행 합성" —
본 설계는 그 목표(콘텐츠 1× draw → flip present → DWM 1레이어 = probe 동형)를
WR 무수정으로 달성하는 경로다.

## 2. 목표와 완료 기준

목표: 게이트 on 월 표출에서 프레임 draw·present 구조를 probe와 동형으로 —
용의자 ①(대여/반납 기구)과 ②(2레이어)를 제거.

완료 기준(2026-07-13 사이클과 동일 관례):

1. 개발기 기능 무결 — 4분면/2×2/45타일(lockstep·메모리·fps 기존 기준선) +
   텍스트 오버레이 페이지
2. 구조 계측 — 콘텐츠 슬라이스 스왑체인 승격, 프레임당 Present 1, 컬링 후
   visual 1개, PresentMon에서 servoshell Present 이벤트 복귀
3. 3모드 무회귀 — off(Draw) / `=surface`(구 경로) / `=1`(신규 하이브리드)
4. 런처 `-DCompSurface` + ServoWallPackage 재패키징 + AMD 3중 A/B 판독 가이드

AMD 실측(프레임 드랍 해소 판정)은 사용자가 패키지로 진행. 이번에는 스왑체인
Present 덕에 PresentMon PresentMode(MPO/DirectFlip 승격 여부) 판독까지 가능.

## 3. 범위 / 비범위

범위:

- `SERVO_COMPOSITOR_DCOMP=1`(기존 값) = 신규 하이브리드 동작.
  **`=surface`** = 현행 가상 서피스 전용(구 경로 보존, AMD A/B 판정용 —
  §3-n `-LegacyUpload` 패턴). off = Draw 모드, 완전 무변경.
- 변경은 컴포지터 모듈(dcomp_compositor.rs) 내부 중심. surfman/paint_api/
  painter/servoshell/WR 무수정.

비범위:

- MPO/DirectFlip 승격 강제·튜닝 (관찰만 — 승격은 보너스)
- 비디오의 DComp 오버레이 승격 (전 스펙 §10 그대로 이월)
- Ctrl+F12 오버레이 게이트 on 미표시 수정 (별도 이월 유지)
- 다중 창, B/C안 (AMD 실측 후 필요 시 재평가)

## 4. 대안 검토와 결정

- **A안(채택): 스왑체인 하이브리드 + 레이어 컬링.** 용의자 ①②를 정확히 제거,
  Chromium 프로덕션 구조(스왑체인 visual) 참조 가능, 좌표계·인프라(2026-07-13
  사이클) 전부 재사용.
- **B안(기각): 창 백버퍼 직행** — 모든 슬라이스가 창 스왑체인 하나를 공유.
  probe와 문자적 동일이나 함정 4종: (1)WR draw 순서=z순서 보장 부재 + 서피스
  transform이 add_surface 시점(draw 이후)에야 확정 (2)슬라이스 간 clear 충돌
  (3)flip 버퍼 회전으로 비갱신 슬라이스 잔상 → 전면 강제 재그리기 필요(WR에
  훅 없음) (4)전면 슬라이스 2장 구조에서 슬라이스 간 overdraw로 창면적 2× 쓰기
  — 목적 자체를 훼손. A안 성공 시 성능 종착점이 동일하므로 우선순위 없음.
- **C안(기각): vendored WR 직행** — B의 함정을 내부 정공법으로 풀 수 있는 유일한
  안이나, vendoring 유지보수 + 참조 구현 부재(Gecko도 안 함) + 추가 이득(타일
  CPU 오버헤드)이 실측상 병목 아님(-TileSize 무관 실증). A안 배포 후에도 격차가
  WR 콘텐츠 패스 내부로 계측될 때만 정당화.

## 5. 아키텍처

### 5.1 하이브리드 저장소 (서피스별 적응)

스왑체인은 "매 프레임 전면 갱신"에 완벽하지만 flip 버퍼 회전(백버퍼=2~3프레임
전 내용) 탓에 부분/희소 갱신에 잔상 문제가 있다. 서피스별로 갱신 패턴에 맞는
저장소를 쓴다(Chromium이 비디오=스왑체인, UI=DComp 서피스로 나누는 원리):

- **스왑체인**(flip, 대여/반납 없음): 전면 dirty가 반복되는 서피스 — 월의 비디오
  콘텐츠 슬라이스. **승격 = dirty_rect가 서피스 extent 전체를 덮는 프레임**에
  수행(그 프레임 draw가 새 버퍼를 완전히 채워 잔상 원천 불가).
- **가상 서피스**(기존): 그 외 전부 — BeginDraw의 내용 보존이 부분/희소 갱신에
  정확하고, 갱신이 드물어 기구 비용 무시 가능. 정적 슬라이스·텍스트 오버레이가
  여기 남는다.
- **강등**: 스왑체인 서피스에 부분 dirty가 반복되면 가상 서피스로 복귀(전환은
  전면 dirty 프레임에).

### 5.2 프레임 흐름 (게이트 on)

```
bind(타일):
  [스왑체인] GetBuffer(0) → pbuffer 래핑(프레임당 1회 캐시, Present 후 파기)
             → NativeSurfaceInfo{origin=서피스-로컬 타일 위치, fbo 0}
  [가상]     기존 BeginDraw 경로 그대로
  ── WR 콘텐츠 draw (비디오/텍스트/도형 합성 — 기존과 완전 동일) ──
unbind: [스왑체인] no-op / [가상] EndDraw

end_frame:
  ① 레이어 컬링: add_surface는 아래→위 순서이므로 목록을 역순(최상위부터)으로
     훑어, 불투명·단일 rect 클립이 하위 서피스의 클립을 완전히 포함하면 그 하위
     visual을 트리에서 제외 (알파 서피스는 절대 컬링하지 않음; 진단용 컬링-오프
     env 제공)
  ② 이번 프레임에 그려진 스왑체인만 Present(0)   ← GPU 복사 0
  ③ Commit 1회
```

월(45타일) 결과: 콘텐츠 슬라이스 = 스왑체인 1개(전면 draw 1× + Present) +
컬링으로 하위 슬라이스 제거 → **DWM 1레이어 + flip = probe 동형**.
MPO/DirectFlip 승격 자격 발생(보너스).

### 5.3 혼합 콘텐츠 (동영상+텍스트+도형)

- 같은 슬라이스 내: WR 콘텐츠 패스가 z-순서 배칭으로 합성(불투명 front-to-back
  + z-버퍼, 알파 back-to-front) — 본 설계와 무관하게 기존 동일.
- 별도 알파 슬라이스(fixed 오버레이 등): 가상 서피스로 남아 DWM이 겹침 —
  내용 불변 시 draw 0. 컬링 비대상이라 소실 불가.

### 5.4 스왑체인 크기와 리사이즈

서피스의 타일 extent를 추적해 스왑체인 크기로 사용. extent 변화(창 리사이즈 등)
시 `ResizeBuffers`는 전면 dirty 프레임에서만 수행(잔상 방지). 월은 extent 고정.

## 6. 컴포넌트와 통합 지점

1. **dcomp_compositor.rs 개정(중심)**: `SurfaceEntry` 저장소를
   `VirtualSurface`(기존) | `SwapChain{swapchain, 앵커·extent, 프레임 pbuffer
   캐시, dirty 이력}` enum으로. 승격/강등 판정, 스왑체인 생성(ANGLE D3D11
   디바이스 → IDXGIDevice → 어댑터 → IDXGIFactory2 →
   `CreateSwapChainForComposition` → `visual.SetContent(swapchain)`),
   컬링(end_frame), Present+Commit. visual 트리·클립·오프셋 로직 재사용.
2. **surfman/paint_api/painter/servoshell/WR: 무수정.** pbuffer 래핑은 기존
   `create_render_pbuffer_from_d3d_texture`(RENDER_TARGET 범용)를 스왑체인
   백버퍼에 그대로 사용 — 좌표계 재검증 불필요(ppf off + 동일 래핑 = 2026-07-13
   Task 5 해결 조건 그대로).
3. **게이트**: on/off 정본은 surfman 기존 함수 유지. `=surface` 세부 모드는
   컴포지터 내부에서 판정.
4. **Cargo**: winapi 스왑체인 계열(dxgi1_2 등) 피처 확인·추가(계획 앵커 —
   PoC의 44종에 포함일 가능성).
5. **런처 2종**(dev+패키지): `-DCompSurface` 스위치(env `=surface`) + 마커에
   모드 표기(`engaged (swapchain hybrid)` / `(surface)`).

## 7. 에러 처리·폴백·해체

- 스왑체인 생성/리사이즈/GetBuffer/래핑 실패 → **그 서피스만 가상 서피스 폴백**
  (warn 1회) = 검증된 현행 동작. 화면이 깨지는 단계 없음.
- 게이트 초기화 실패 → Draw 폴백(기존).
- 스왑체인 서피스의 예외적 부분 dirty(월 무발동): 1차 = 이전 버퍼에서 비갱신
  영역 복사 후 draw·프레젠트. 1차 불가 시(이전 버퍼 읽기 불가 판명 — §9 앵커)
  = 그 프레임은 **프레젠트 보류**(마지막 완전한 버퍼 유지 — 부분 갱신이 화면에
  늦게 반영되는 지연은 허용, 잔상·깨짐은 불허) + 강등 예약, 다음 전면 dirty
  프레임에 가상 서피스로 강등 실행. 전면 dirty가 오래 안 오는 경우의 갱신 지연은
  `redraw_on_invalidation` capability로 해소 가능한지 §9 앵커에서 확정.
- Present 전 GL 제출 보장: ANGLE 플러시 지점 확정(§9 앵커) — 기존 EndDraw의
  암묵적 동기화를 대체.
- 해체: `visual.SetContent(null)` → 스왑체인 Release를 기존 deinit 계단
  (surfman UAF 교훈 순서)에 편입.

## 8. 리스크와 완화

1. pbuffer 캐시 수명: flip에선 Present마다 백버퍼가 바뀜 → 래핑은 프레임 단위
   캐시(Present 후 파기). 래핑 비용은 기존 실측상 저렴.
2. 컬링 = 요소 소실 리스크 → 보수 규칙(불투명+단일 rect 완전 포함) + 진단용
   컬링-오프 env로 즉시 판별 가능.
3. MPO/DirectFlip 승격은 보장 아님 — 미승격이어도 ①② 제거 이득은 확보.
4. 알파 스왑체인(premultiplied) 정확성 → 텍스트 오버레이 페이지로 검증.
   (기본 설계상 알파 서피스는 가상 서피스에 남으므로 발동 자체가 드묾.)
5. 페이싱: Present(0) 비블로킹 — WIN_VSYNC 페이싱·in-flight 게이트 무영향.

## 9. 계획 단계 앵커 항목 (writing-plans에서 확정)

1. winapi `CreateSwapChainForComposition`(IDXGIFactory2, dxgi1_2) 커버리지와
   필요 피처; 스왑체인 파라미터(FLIP_SEQUENTIAL vs DISCARD, 버퍼 수,
   DXGI_ALPHA_MODE_PREMULTIPLIED/IGNORE 선택).
2. flip 모델에서 이전 버퍼 읽기(GetBuffer(i) 규칙) 가능 여부 — §7 부분 dirty
   1차 안전망의 성립 조건.
3. `CompositorCapabilities.redraw_on_invalidation`의 정확한 의미(WR 소스)와
   부분 dirty 안전망으로의 활용 가능성.
4. Present 전 ANGLE 플러시 방법(glFlush/glFinish/eglWaitClient 중 필요 최소).
5. WR의 add_surface 호출 순서가 z-순서(아래→위)라는 가정 확인(composite.rs
   문서/코드 — 컬링 규칙의 전제).
6. 승격 판정식 상세: dirty_rect==extent 판정과 다중 타일 서피스에서의 프레임 내
   집계 방법(bind가 타일별로 오므로 프레임 단위 union 필요).
7. GetBuffer(0)→pbuffer 래핑의 프레임 내 재사용 규칙(같은 프레임에 같은
   스왑체인의 bind가 여러 번 올 때 1회만 래핑).
8. 진단 로깅: 승격/강등/컬링/Present를 SERVO_DCOMP_DEBUG(기존 OnceLock 게이트)에
   편입.

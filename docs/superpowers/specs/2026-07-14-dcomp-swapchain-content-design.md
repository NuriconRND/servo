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

## 10. 구현 결과와 이탈 (2026-07-14)

Task 1–6 구현·검증 완료(HEAD `e8b0561ce`). §9 앵커 8건은 구현 중 전부 해소(강등
미구현 1건만 설계 자체를 정제 — 아래 참조). 이 절은 승인된 설계 대비 실제 구현의
확정치·이탈·실측 발견을 기록한다.

### 5-1~5-6 검증 수치 (Task 5, `.superpowers/sdd/task-5-report.md` 상세)

- **45타일 lockstep**: 2×2·45타일 모두 전 타일 동일 프레임 카운터로 **±0**(스크린샷
  단면 비교). 5분 연속 구동 메모리 ~4790±30MB로 **플랫**(누수 없음).
- **fps**: 3모드(off/surface/on) 전부 **avg 60.97~61.77, dropped_total 0**, 45/45 재생
  — 기준선(off) 대비 하이브리드(on) 무회귀.
- **PresentMon (개발기 RTX A5000)**: `=1`(하이브리드) — **1545 Present, 전부
  servoshell.exe, PresentMode="Composed: Flip"**(승격된 스왑체인 Present가 DWM 컴포지션
  계층에서 실측 관찰됨 — 스왑체인 실재의 OS 레벨 증거). `=surface`(가상서피스 전용,
  대조군) — **Present 0**(IDXGISwapChain::Present 자체가 없음, DComp Commit로만 갱신되어
  PresentMon 미관측). 이 개발기에서 MPO/DirectFlip 승격은 미발동(Composed 유지) —
  §8-3에서 예견한 대로 기록만, 이득은 ①②단 소멸 자체(§1 판별 체인)에서 이미 확보.
- **3모드 무회귀**: off(engaged0)/surface(engaged1·promote0)/on(engaged1·promote>0)
  전부 정상 렌더·블랙 0·크래시 0(§5-4). 해체·재시작(25타일×3 + 45타일 재기동)
  실패 0(§5-6).

### §7 정제 — 강등은 구현하지 않음(보류-누적으로 충분, 근거)

승인된 설계 §7은 "예외적 부분 dirty → 1차 이전 버퍼 복사, 불가 시 프레젠트 보류 +
**강등 예약**, 다음 전면 dirty에 가상 서피스로 강등"을 명시했다. 구현은 강등을
**구현하지 않고 보류-누적만으로 정제**했다:

- **이전 버퍼 복사(1차 안전망)가 §9 앵커 2에서 불필요로 확정**: FLIP_DISCARD
  채택(아래)으로 이전 버퍼를 애초에 읽지 않는 설계가 되어, "1차 실패 시 강등"의
  전제(1차가 실패할 수 있다)가 사라졌다.
- 부분 dirty 프레임은 **coverage 누적 규칙**(§5.2, bind별 dirty가 타일 valid_rect
  전체를 덮을 때만 그 타일을 covered로 집계 — 부분 타일은 미피복 취급)으로 Present를
  보류하고, 다음 프레임에 이어서 커버리지를 채운다. 실측(45타일 프로덕션 월, 30fps
  콘텐츠)상 이 보류는 **정상 케이스**(아래 참조)이지 예외가 아니다 — 강등을 발동할
  '이례적 상황'이 실무에서 관측되지 않았다.
- 강등을 생략해도 §7의 안전 불변식(잔상 불허, 지연만 허용)은 **withhold 카운터 +
  WITHHOLD_WARN_FRAMES 경고**로 그대로 유지된다: 부분 dirty가 오래 누적되면
  화면 갱신이 지연되지만(마지막 완전 프레임 유지), 깨지거나 잔상이 남는 경로는
  없다. 강등의 실효는 "지연을 없애고 부분 갱신을 즉시 반영"인데, 프로덕션 월(비디오
  슬라이스는 전면 갱신이 기본 패턴)에서 이 이득은 발동 빈도가 낮아 우선순위가
  낮다고 판단 — **의도적 범위 축소**(이탈이지 결함이 아님).
- 이월: 강등이 실제로 필요한 갱신 패턴(부분 dirty가 만성적인 슬라이스)이 실기에서
  나타나면 §7 원안대로 구현.

### FLIP_DISCARD 채택 (이전 버퍼 미참조)

스왑체인은 `DXGI_SWAP_EFFECT_FLIP_DISCARD` + BufferCount 2로 생성한다
(`dcomp_compositor.rs:499`). 매 프레임 GetBuffer(0)는 그 프레임에 온전히 새로
그려지는 버퍼이고, 이전 프레젠트 내용을 읽거나 보존할 필요가 없다 — 정확성은
"부분 dirty 프레임은 covered==full일 때만 Present"(coverage 누적, §5.2)로 보장되며
DISCARD의 "이전 버퍼 내용 불특정"과 충돌하지 않는다. §9 앵커 1(FLIP_SEQUENTIAL vs
DISCARD)의 확정 답이며, 앵커 2(이전 버퍼 읽기 가능 여부)를 "불필요"로 해소한 근거이기도
하다.

### 승격/보류 상수

- `PROMOTE_STREAK = 3`: 연속 3회 전면 갱신(coverage 풀) 프레임이면 가상 서피스 →
  스왑체인 승격을 요청한다(`dcomp_compositor.rs:51`). 단발 전면 갱신에 의한 오승격을
  거르는 디바운스.
- `WITHHOLD_WARN_FRAMES = 60`: 스왑체인 서피스가 부분 dirty로 60프레임 연속 Present
  보류 상태면 warn 1회(`dcomp_compositor.rs:53`) — 화면 갱신 지연이 눈에 띄는
  수준(60fps 기준 1초)임을 운영자에게 알리는 임계.

### is_opaque 승격 게이트 (§5.3 정합, `e8b0561ce`)

Task 5-3에서 발견된 결함 픽스: 승격 판정식(`dcomp_compositor.rs` end_frame, 원래
`~996`행)에 **`entry.is_opaque` 조건이 없어** 비불투명(알파) 슬라이스도 전면 갱신
스트릭만 충족하면 스왑체인으로 오승격되고 있었다. 오승격된 알파 슬라이스는
coverage가 항상 부분(알파 콘텐츠는 보통 부분/희소 갱신)이라 Present가 영구
보류(withhold 167~187회 관측)되는 병리를 낳았다. 수정은 승격 조건에
`entry.is_opaque &&`를 추가 — 이제 설계 §5.3("별도 알파 슬라이스는 가상 서피스로
남는다")대로 **불투명 슬라이스만** 승격 대상이 된다. 검증: 45타일 프로덕션 월에서
promote=2(불투명 2슬라이스, 회귀 없음) + overlay_wall 회귀 페이지에서 promote=0·
withhold=0(병리 소멸). 순수 조건 추가, unwrap/panic 없음.

### displayed_anchor 3상태 (승격~첫 Present 창구간 근본 수정)

승격 직후(스왑체인 생성 성공~첫 완전 Present 사이)와 리사이즈 regen 직후, visual에
실제로 붙어 있는 콘텐츠의 좌표 anchor는 스토리지의 렌더 대상 anchor(`sc.anchor`)와
다를 수 있다 — 오프셋 산식이 잘못 `sc.anchor` 기준으로 계산하면 아직 표시 중인
구 콘텐츠(가상좌표 fallback 또는 옛 anchor의 스왑체인)가 화면 밖(~16384px)으로
밀려나 순간적으로 소실된다. 수정은 `displayed_anchor: Option<DeviceIntPoint>` 필드로
**"지금 화면에 붙어 있는 콘텐츠의 anchor"를 별도 부기**하고 오프셋 산식이 이를
따르게 했다(`dcomp_compositor.rs:268,943`). 3상태:

1. **`None`**: 승격 직후, 아직 fallback 가상 서피스 콘텐츠가 visual에 붙어 있다 —
   오프셋은 가상좌표(0,0) 기준.
2. **`Some(옛 anchor)`**: regen(리사이즈) 이후, 아직 새 스왑체인으로 content-swap되지
   않아 옛 anchor 콘텐츠가 계속 표시 중 — 오프셋은 옛 anchor 기준으로 유지.
3. **`Some(새 anchor)`**: 첫 완전 Present에서 content-swap이 일어나 스왑체인 콘텐츠로
   전환된 프레임 — 콘텐츠 전환과 오프셋 전환을 **같은 Commit에서 원자적으로**
   재적용(무글리치).

### 리사이즈 regen — ★WR은 리사이즈에 서피스를 재생성하지 않는다(실측 발견)★

설계 §5.4는 "서피스의 타일 extent를 추적해 스왑체인 크기로 사용, extent 변화 시
ResizeBuffers"를 명시했으나, 구현·검증(5-1) 과정에서 **WR 자체는 창 리사이즈에 기존
NativeSurfaceId를 재사용하며 서피스를 파기·재생성하지 않는다**는 것이 실측으로
확인됐다(같은 id의 `tiles`/`virtual_offset`이 갱신될 뿐). 따라서 구현은 §5.4를
"ResizeBuffers 호출"이 아니라 **스왑체인 자체를 새 크기로 재생성**(regen)하는
경로로 구현했다 — `geometry_changed`(extent.min/size 변화) 감지 시 `regen_requests`에
쌓아 루프 밖에서 `create_composition_swapchain`으로 새 스왑체인을 만들고
`visual.SetContent`는 다음 완전 Present까지 옛 스왑체인을 유지(무글리치, displayed_anchor
3상태와 결합). 5-1 검증에서 축소(904×461)·확대(1664×941) 각각 regen 마커 발생(누적
regen=3) + 4분면 픽셀 정확 3회로 확인. 이는 API 표면(ResizeBuffers vs 재생성) 수준의
이탈이며 설계 의도(리사이즈에도 잔상 없는 무결 표시)는 그대로 달성됐다.

### 알려진 제한(이월): 비불투명 슬라이스의 비디오는 DComp 미표시

**5-3에서 발견, 조사 완료(근본원인 규명), 본 프로젝트 범위 밖으로 이월** — 상세는
`.superpowers/sdd/task-5-report.md` §5-3 및 "5-3 픽스 웨이브 Part B" 참조. 요약:

- 증상: float 레이아웃(다중 비디오 + position:fixed 반투명 텍스트 오버레이) 페이지에서
  WR이 라이브 비디오를 **비불투명(non-opaque) 픽처캐시 슬라이스**에 배치하면, 그
  비디오는 DComp 경로(=1과 =surface 둘 다)에서 **전혀 표시되지 않는다**(검정).
  Draw(off) 모드는 정상.
- 실험으로 확정된 원인 경계: pbuffer EGL config는 알파를 가짐(config 문제 아님) /
  alpha_mode 강제(IGNORE)로도 불변(알파값 문제 아님) / 비불투명 슬라이스의 **솔리드**
  콘텐츠는 정상 표시(비불투명 가상서피스 표시 자체는 정상) → **비불투명 슬라이스에
  놓인 "비디오" 콘텐츠 합성 경로만** 실패. WR의 비불투명 슬라이스 vs 비디오 이미지
  프리미티브 합성(또는 ANGLE 외부텍스처 샘플) 수준이며, **`=surface` 베이스 경로(직전
  WR Native Compositor 프로젝트에서 이미 머지된 가상서피스 기반)와 공유** —
  `dcomp_compositor.rs` 모듈 밖.
- **프로덕션 영향 없음**: 실제 월(flex-wrap 레이아웃)은 비디오 슬라이스가 항상
  불투명으로 나와 3모드 전부 정상(45타일 검증, §5-2/5-4 PASS). 이 결함이 발동하는
  것은 float 레이아웃 + 다중 비디오 + 알파 오버레이가 겹치는 특정 엣지케이스뿐이다.
- 부수적으로 발견·수정 완료: 이 엣지케이스가 유발한 승격 오동작(위 is_opaque 게이트)은
  본 프로젝트 범위 내에서 해결됨 — 남은 것은 검정 증상(WR/ANGLE 레이어) 자체뿐이다.

### 30fps 콘텐츠의 부분 dirty 프레임 Present 보류 = 정상 케이던스

45타일 프로덕션 월(§5-2 PASS)에서 하이브리드 모드 실행 중 **withhold 카운터가 305까지
관측**됐으나 이는 결함이 아니라 **정상 동작**이다: 콘텐츠는 30fps 비디오이고 컴포지터는
그보다 높은 프레임레이트로 합성 기회를 얻으므로, 새 비디오 프레임이 아직 도착하지 않은
합성 사이클에서는 그 슬라이스의 타일 dirty가 부분(또는 0)이 되어 coverage가 풀을 채우지
못하고 Present가 그 프레임만 보류된다 — 다음 완전 갱신 프레임(새 비디오 프레임 도착
시점)에 정상 Present된다. 월은 이 상태에서도 45/45 lockstep·블랙 0으로 정상 표시됐다
(§5-2) — withhold는 "마지막 완전 프레임 유지, 부분 프레임만 건너뜀"이 설계 의도대로
작동하고 있다는 증거이지, 화면 이상의 신호가 아니다. §7 정제(위)에서 강등을 생략한
근거이기도 하다(이 정상 케이던스에 강등이 발동하면 오히려 불필요한 가상서피스 왕복이
늘어난다).

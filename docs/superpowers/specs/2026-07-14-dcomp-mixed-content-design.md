# DComp 복합 콘텐츠 지원 설계 (부분 Present + 강등 폴백 + 알파 슬라이스 진단)

날짜: 2026-07-14 (오후)
상태: 사용자 승인 (§1–§4 섹션별 승인 완료)
선행: `2026-07-14-dcomp-swapchain-content-design.md` (스왑체인 하이브리드 + 레이어 컬링, 로컬 10커밋 03946de91..21f6dab41)
브랜치: multigpu-tiled-wall (기존 10커밋 위에 로컬 유지 — 푸시는 AMD 실측 후 결정)

## 1. 배경 — 실측으로 확정된 결함 2건

복합 미디어 페이지 `tests/html/mixed_media_demo.html`(3x2 비디오 그리드 100vh + fixed
상단바(회전 로고/LIVE 펄스/JS 시계) + 반투명 자막 슬라이드 + 하단 뉴스 티커)에서
2026-07-14 실측:

### 결함 ① — 하이브리드(=1) 승격 슬라이스 동결 (이번 사이클 신규)

- 시작 초기 페인트 3프레임이 전면 더티 → `PROMOTE_STREAK=3`을 자명하게 충족 →
  상단바 슬라이스까지 스왑체인 승격.
- 이후 시계(1초 텍스트)·로고 애니 등 **부분 갱신만** 발생 → full-coverage Present
  규칙(부분 더티는 같은 백버퍼 누적 + 보류)을 영원히 못 채움.
- 실측: 상단바 슬라이스(id6) withhold **20,655회, covered=0/2 — 단 1회도 Present
  안 됨** = 시계 정지·화면 동결. 비디오 슬라이스(id0)도 341회 covered=3/6.
- 선행 스펙의 "withhold≈절반은 30fps 정상 케이던스" 판정은 순수 비디오 월
  한정이었음이 판명. **full-coverage 규칙은 '매 프레임 전면 갱신' 콘텐츠 전제의
  설계 결함.**
- 표시가 어는 메커니즘: 승격 후 draw는 스왑체인으로 가는데 visual은 여전히
  `fallback_virtual`(승격 전 마지막 화면)을 표시 → fallback은 더 이상 갱신되지
  않으므로 그 시점에서 동결.

### 결함 ② — 비불투명(알파) 슬라이스의 비디오 검정 + 티커 텍스트 미표시 (이월 확장)

- `=surface`(가상 서피스 전용)에서 비디오 영역 검정 + 티커 텍스트 미표시. 상·하단
  바(불투명 배경)는 정색 표출 — 슬라이스 단위 결함.
- 선행 사이클의 overlay_wall.html(float 레이아웃)이 재현 1호, mixed_media_demo가
  재현 2호. 하이브리드(=1)에서도 알파 슬라이스는 승격되지 않으므로(is_opaque
  게이트) 동일하게 발현 — **베이스 경로 공통 결함**.
- 레이아웃 함정(재현 조건): 바를 일반 플로우 블록으로 두고 그리드를 82vh로 줄이면
  WR이 전면 슬라이스를 비불투명으로 분류해 비디오가 알파 슬라이스에 들어감.
  그리드 100vh + fixed 오버레이 구조로도 오버레이 쪽 알파 슬라이스에서 재현.
- 기각된 가설(2026-07-14 실측): "WR이 비디오를 외부 컴포지터 서피스로 승격 →
  stub 무시" — 오늘 로그 5개 전부에서 외부서피스 warn-once **0건**. 비디오는
  타일 콘텐츠 draw로 들어가고 있음.
- 남은 용의자: 알파 타일의 실제 draw 단계(안 그려졌거나 0을 씀) 또는 DComp
  합성 단계. 알파 "솔리드"는 정상 표시 실증(선행 사이클 quad 검증) — 텍스트·
  비디오(텍스처 샘플링 계열)만 실패하는 패턴.

## 2. 목표 / 비목표

**목표**
1. `=1`(하이브리드)에서 복합 콘텐츠가 동결 없이 정확히 표시 — 어떤 갱신
   패턴(전면/부분/리듬 변화)이든 표시 결과는 항상 최신.
2. 대면적 부분 갱신 콘텐츠도 스왑체인 성능 유지(부분 Present) — 성능 처리는
   1차=부분 Present, 폴백=강등(사용자 승인: A안).
3. 결함 ②의 원인 계층 확정(진단) — 우리 코드 층이면 이번 사이클에서 수정,
   WR 내부급(vendoring 필요)이면 규모·선택지 보고 후 재결정(결정 게이트).
4. 순수 비디오 월 경로 무회귀 (45타일 lockstep/fps/메모리/PresentMon flip).

**비목표**
- `=surface` 모드 동작 변경 (AMD 3중 A/B 비교군 보존; 결함②는 베이스 공통
  수정이라 예외적으로 양쪽에 적용됨).
- 게이트 off(Draw) 경로 변경.
- 다중 창(§4.5 per-device 레지스트리), Ctrl+F12 오버레이 — 기존 이월 유지.
- AMD 실측(사용자 몫). 단 패키지에 판독 가이드 갱신 포함.

## 3. 모드·게이트

| 게이트 | 값 | 의미 |
|---|---|---|
| `SERVO_COMPOSITOR_DCOMP` | `1` / `surface` / off | 기존 그대로 (hybrid / 가상서피스 전용 / Draw) |
| `SERVO_DCOMP_NO_PARTIAL_PRESENT` | `1` | 부분 Present 비활성(진단용, `NO_CULL` 관례) — 강등 폴백만으로 동작 |
| `SERVO_DCOMP_READBACK` | `1` | 결함② 진단: 타일 unbind 시점 픽셀 readback 로그 (기본 off, 진단 전용) |

부분 Present는 PoC 통과 + 런타임 자격(GetBuffer(1) 읽기 성공) 시 **기본 on**.

## 4. 승격 위생 — 최소 나이 조건

- `PROMOTE_MIN_AGE_FRAMES = 30`: 서피스가 **그려진 프레임 수 30회** 경과 후부터
  promote_streak 인정(그 전에는 streak 누적 안 함).
- 효과: 시작 초기 페인트에 의한 자명 승격 제거. 순수 월은 시작 +0.5초에 승격
  (무해 지연). 상단바류는 30프레임 내 부분 갱신이 streak을 리셋 → 승격 자체가
  일어나지 않아 불필요한 스왑체인 churn 감소.
- `PROMOTE_STREAK=3`, is_opaque 게이트, 첫 전면 커버리지 Present에서 SetContent
  원자 전환, `displayed_anchor` 3상태 — 기존 유지.

## 5. 부분 Present — 1차 경로

### 5.1 PoC 선행 게이트 (HALT)

독립 예제 `components/shared/paint/examples/dcomp_partial_present_poc.rs`
(선행 사이클 dcomp_native_poc 관례, `--features no-wgl`)로 4항목 확정:

1. `CreateSwapChainForComposition` + **`DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL`** 생성.
   (근거: FLIP_DISCARD는 Present 후 버퍼 내용 미보존 — catch-up 복사가 직전
   프레임 버퍼 내용을 전제하므로 SEQUENTIAL 필수. 부분 Present 채택 시 승격
   스왑체인의 스왑 이펙트를 SEQUENTIAL로 전환.)
2. **`GetBuffer(1)` 읽기** — 직전 Present 버퍼를 `CopySubresourceRegion` 소스로
   사용 가능한지. **핵심 미지수 — 실패 시 부분 Present 전체 폐기(HALT), §6
   강등만으로 사이클 완결.**
3. `IDXGISwapChain1::Present1` + `DXGI_PRESENT_PARAMETERS` DirtyRects 동작
   (DComp visual + Commit 하에서 화면 정확성).
4. 버퍼 로테이션 의미론 실측: Present 후 GetBuffer(0)/GetBuffer(1)의 내용을
   readback으로 검증(교호 갱신 패턴 → 합성 결과 픽셀 판정).

### 5.2 본구현 (dcomp_compositor.rs)

- 승격 흐름 유지: streak 충족 → 스왑체인 생성 → **첫 전면 커버리지 Present**에서
  SetContent 전환(이 전까지는 fallback_virtual 표시 — 기존과 동일).
- **첫 콘텐츠 Present 이후: 그려진 모든 프레임을 즉시 Present. withhold 분기 소멸.**
- 부분 프레임 절차(프레임 내 순서가 정확성의 핵심 — catch-up은 **end_frame에서
  draw 완료·GL flush 후, Present1 직전** 수행. 첫 bind 시점에는 이번 프레임의
  전체 더티를 알 수 없어 "전면 더티면 생략" 판정이 불가하기 때문):
  1. GL flush(기존 flush-before-Present) → 이번 프레임 draw가 buffer 0에 확정.
  2. **catch-up 영역 = stale[현재 버퍼] − 이번 프레임 더티** (정확한 렉트 영역
     차집합 — 유닛테스트 필수. 이번 더티와 겹치는 부분을 복사하면 신규 콘텐츠를
     구본으로 덮어써 손상되므로 차집합은 근사 불가·정확해야 함). 결과 렉트들을
     GetBuffer(1)→GetBuffer(0) CopySubresourceRegion.
  3. `Present1`(이번 프레임 더티 렉트 힌트, `MAX_PRESENT_DIRTY_RECTS=16` 초과
     시 렉트 0개=전체 프레젠트).
- **버퍼별 stale 추적**: `stale: [Vec<DeviceIntRect>; 2]`. Present(dirty=D) 시
  반대 버퍼의 stale에 D 추가; catch-up 복사 후 해당 버퍼 stale 비움. 렉트 수가
  16 초과 시 서피스 extent 전체 1렉트로 붕괴(보수적 — 차집합 후 복사라 손상
  없음).
- 순수 월(항상 전면 더티): 이번 더티가 extent 전체 → 차집합 공집합 →
  **복사 0바이트**, 기존 경로와 동일.
- FrameCoverage의 per-tile dirty 기록을 더티 렉트 유니온 소스로 재사용.

### 5.3 부분 Present 실패 시 런타임 폴백

GetBuffer(1)/CopySubresourceRegion/Present1 이 DXGI 오류를 반환하면 해당
서피스를 즉시 강등(§6)하고 per-surface warn-once(디바이스 로스트 로그 폭주
함정 회피). 이후 재승격은 쿨다운 적용.

## 6. 강등(demote) — 폴백 안전망

### 6.1 발동 조건

| 상황 | 조건 |
|---|---|
| 첫 콘텐츠 Present 전 (승격 창구간) | withhold `DEMOTE_AFTER_WITHHOLD=30`프레임 연속 — **부분 Present 가능 여부와 무관** (버퍼에 미기록 영역=쓰레기라 부분 Present 불가; §4 위생으로 이 케이스 자체가 희귀해짐) |
| 첫 콘텐츠 Present 후 | 부분 Present 불가(PoC 실패 / `NO_PARTIAL_PRESENT=1` / 런타임 오류) 상태에서 withhold 30프레임 연속 |
| 즉시 | catch-up 복사·Present1 DXGI 오류 |

### 6.2 시딩 (강등 후 표시가 최신이 되도록 — 케이스 2종)

1. **첫 Present 전** (오늘 동결 케이스): `fallback_virtual` 생존 + 미갱신 영역은
   변한 적 없어 여전히 유효 → **누적 더티 영역만** fallback_virtual에
   BeginDraw(렉트별, 16 초과 시 바운딩 유니온 1회)로 스왑체인 buffer 0에서
   복사해 최신화. visual은 계속 fallback 표시 → 끊김 없음.
   (buffer 0은 승격 후 Present가 없어 로테이션 0 = 모든 부분 draw 누적 상태.)
2. **Present 이력 있음**: fallback은 첫 전면 Present에서 해제된 상태 → 가상
   서피스 신규 생성 + buffer 0 전체 복사 + SetContent(virtual) 전환.
   (withhold 중에는 Present가 없어 buffer 0 = 마지막 전면 + 이후 누적 더티 =
   완전·최신. 부분 Present 경로에서 catch-up 복사 자체가 실패해 강등하는 경우
   직전 프레임 더티 영역이 1프레임 구본으로 시딩될 수 있음 — 해당 영역은 직전에
   갱신되던 콘텐츠라 재갱신 시 자가 치유되는 알려진 미세 한계로 수용.)

### 6.3 재승격 쿨다운

- 서피스별 강등 횟수 n에 대해 쿨다운 = `300 × 2^(n−1)` 프레임, 상한 3600(약 1분).
- 쿨다운 중에는 streak 누적 자체를 중단. destroy_surface 시 상태 소멸.

### 6.4 상태 정리

- 강등 후 storage는 Virtual로 복귀 — bind/unbind는 기존 가상 서피스 경로 그대로.
- `displayed_anchor` 3상태 규약 유지(강등 = fallback/virtual 표시 상태로 복귀).
- 스왑체인·frame_pbuffer는 강등 시 해제. 리사이즈 재생성 로직은 기존 유지.

## 7. 결함 ② 진단 — readback 계측 + 최소 재현 + 결정 게이트

### 7.1 계측 (`SERVO_DCOMP_READBACK=1`)

- unbind() 직전(pbuffer가 current인 시점) 타일 더티 영역을 8×8 그리드
  glReadPixels 샘플 → 로그: surface id / tile / opaque / 알파 min·max /
  RGB 비영(非零) 카운트.
- 로그 폭주 함정(진단 로깅이 스톨 7-10× 증폭 — 기존 함정 기록) 회피:
  **프레임 120까지만** 기록 후 자동 중단.

### 7.2 최소 재현 페이지 `tests/html/dcomp_alpha_probe.html`

- 불투명 베이스(전면 비디오 1개 = 대조군, 불투명 슬라이스) + fixed 오버레이
  (will-change:transform으로 알파 슬라이스 강제) 내용물을 `?case=`로 선택:
  `solid`(rgba 반투명 사각형 — 정상 표시 예상 대조군) / `text`(티커형 텍스트) /
  `video`(소형 비디오). rAF 하트비트 필수(기존 관례).
- 화면 판정: CopyFromScreen 픽셀 샘플(기존 도구), =surface와 =1 양쪽.

### 7.3 판정 트리 + 결정 게이트

```
readback(알파 타일 draw 결과)
├─ 픽셀 정상 (비디오 RGB 존재)  → 소실은 DComp 합성/서피스 측 = 우리 층
│    → 알파 서피스 포맷/premultiply/visual 설정 조사 → 이번 사이클에서 수정
└─ 픽셀부터 0/검정            → WR draw 층 (배칭/블렌드/시저/텍스처 바인딩)
     → GL 에러 체크 + 원인 좁히기
     ├─ 우리 층에서 우회 가능(bind 상태/설정)  → 이번 사이클에서 수정
     └─ WR 내부 수정 필요(vendoring급)        → HALT: 규모·선택지 보고 후 재결정
```

- 진단·수정 결과와 무관하게 결함 ①(동결) 수정은 독립적으로 완결된다.

## 8. 에러 처리

- DXGI/DComp 호출 실패: per-surface warn-once + 즉시 강등(§6). 프로세스 지속.
- 디바이스 로스트: 기존 이월 항목(로그 폭주) 악화 금지 — 신규 로그는 전부
  warn-once 또는 프레임 상한.
- PoC HALT 2곳: §5.1-2(GetBuffer(1) 불가 → 부분 Present 폐기), §7.3(WR급 →
  보고 후 재결정). HALT 시에도 사이클은 강등+진단 결과로 완결 가능.

## 9. 검증 계획 / 수용 기준

1. **mixed_media_demo (=1)**: 45초 관찰 — 영상 6개 재생(픽셀 시변), 시계 1초
   갱신, 티커 스크롤, 자막 애니, 동결 0. 로그: withhold 무한 부재(모든 승격
   서피스가 Present 또는 강등으로 수렴), panic 0. (티커·자막 등 알파 슬라이스
   내용물의 표시는 결함② 게이트 결과에 종속 — ② 수정 성공 시 전 요소 표시,
   HALT 시 알파 슬라이스 항목만 제외하고 판정하며 스펙 이월 기록.)
2. **quad_promote_partial.html 회귀** + 강등/재승격 시나리오(?full=N으로 리듬
   전환) 픽셀 정확, 쿨다운 동작 로그 확인.
3. **45타일 순수 월 무회귀 (=1)**: lockstep ±1, fps 60, 5분 메모리 플랫,
   PresentMon flip 유지, catch-up 복사 0 확인(로그).
4. **=surface / off 무회귀**: 45타일 표시 정상(=surface), off 바이트 동일 경로.
5. **부분 Present 단위검증**: PoC 4항목 + 본구현 후 30fps 콘텐츠(covered=3/6
   케이스)가 withhold 없이 매 그려진 프레임 Present되는지 로그 확인. 비디오
   슬라이스가 부분 판정되던 원인(covered=3/6, 반올림 의심)은 검증 중 로그로
   관찰·기록(블로커 아님 — 부분 Present가 정확성을 보장).
6. **패키징**: ServoWallPackage.zip 재생성 — mixed_media_demo.html +
   dcomp_alpha_probe.html 포함, AMD 판독 가이드에 "성능 A/B는 순수 비디오 월
   페이지로" 주의 명기.

## 10. 이월(이번 사이클 범위 밖)

- 결함② WR급 판정 시의 본수정(게이트 결과에 따름).
- Ctrl+F12 오버레이, 디바이스 로스트 로그 폭주 본수정, §4.5 다중 창.
- AMD 3중 A/B 실측·해석, 10+α 커밋 푸시 결정(사용자).

## 11. 구현 결과 (사후 기록, 2026-07-15)

### 11.1 PoC 게이트 (Task 1)

G1~G4 전부 PASS. `CreateSwapChainForComposition` + `FLIP_SEQUENTIAL` 생성 확인(G1),
**`GetBuffer(1)` 읽기가 `CopySubresourceRegion` 소스로 성립**(G2 — §5.1의 HALT
미지수 해소, 부분 Present 채택 확정), `Present1` DirtyRects 화면 정확(G3), 버퍼
로테이션 실측 = **2버퍼 핑퐁**(G4 — Present 후 `GetBuffer(0)`은 그 Present 직전
내용, 스펙 §5.1-4 예상 그대로).

### 11.2 부분 Present 채택 (기본 on)

승격 스왑체인을 FLIP_SEQUENTIAL로 전환, catch-up = `stale[버퍼] − 이번 프레임
더티`(정확한 렉트 차집합, 겹치는 부분은 복사 제외) 복사 후 `Present1` 더티렉트
힌트 적용. 두 상한은 서로 다르게 동작한다: `MAX_PRESENT_DIRTY_RECTS=16` 초과 시
그 프레임의 Present1 더티렉트 **힌트를 생략**(`DirtyRectsCount=0`, 즉 전체
프레젠트 — 붕괴가 아니다); `MAX_STALE_RECTS=32` 초과 시 stale 목록을 바운딩
유니온 **1렉트로 붕괴**하지만(`collapse_dirty_if_oversized`/`StaleTracker::
on_present`), 그 유니온은 stale 렉트들의 경계일 뿐 반드시 서피스 전체는 아니다.
45타일 월 실측: 초기 검증 present-partial **478회**(withhold 0, 실패 0,
2.5분 재생) + Task 7 5분 소크 재확인 **596건** 중 **catchup=0(완전 더티 프레임,
복사 0바이트) 41건(6.9%)**, 나머지 555건(93%)은 catchup 1~16 렉트(30fps 콘텐츠의
정상적 부분 갱신 케이던스) — "전면 더티 프레임은 복사 0" 요구를 실측으로 확인.

### 11.3 강등 폴백 (실측)

승격 위생(`PROMOTE_MIN_AGE_FRAMES=30`, 쿨다운 중 streak 누적 중단)과 강등 3조건
(첫 Present 전 withhold 30 연속 / 첫 Present 후 부분 Present 불가 상태 withhold
30 연속 / catch-up·Present1 즉시 DXGI 오류) 전부 실기 확인. **타입1**(부분
Present 강제 비활성, `full=33`)·**타입2**(withhold 장기화, `full=120`) 둘 다
재현, **지수 쿨다운 300→600→1200 프레임을 벽시계로 실측 일치**(재승격 간격은
근사 일치 — 원 5s/9s 대비 실측 4s/10s, 에스컬레이션 공식·카운트·픽셀 정확성은
정확히 일치). 쿨다운 중 streak 누적 중단은 §6.3 정합대로 리뷰 픽스에서 반영·
재검증됨.

### 11.4 결함② 판정 — 스펙의 가설과 실제 원인이 달랐음

스펙 §1/§7이 세운 가설("비불투명 슬라이스의 비디오·텍스트 합성 경로 결함", WR
draw 층 용의)은 **기각됨**. `SERVO_DCOMP_READBACK` 계측 + `dcomp_alpha_probe.html`
재현으로 확정한 실제 원인은 우리 층의 결함 2건:

- **(a)** 선행 사이클(`2026-07-14-dcomp-swapchain-content-design.md` §5.3 레이어
  컬링)이 WR `is_opaque`를 "클립 완전 피복 보장" 힌트로 오용 — 실제로는 backdrop
  힌트일 뿐이라, 클립은 완전 피복이지만 내부적으로 작게만 그리는 슬라이스(하트비트
  점 2×2, 상단 스트립만 페인트)가 하위의 실콘텐츠(비디오·티커)를 컬링(surf0
  1012회 등).
- **(b)** `AddVisual(TRUE, NULL)` 호출이 MS 문서상 NULL 레퍼런스 특례로 형제
  visual들 "아래"에 삽입되어 **z-순서 전체가 역전**(:2055) — 월/probe는 유효
  visual이 1개뿐이라 무증상이었고, 그래서 선행 사이클 검증을 통과했었음.

수정: (b) `AddVisual` `insertAbove` TRUE→FALSE(1플래그, NULL 레퍼런스 의미론
주석 보존) + (a) **컬링 전면 제거(A-1)**. 중간 시도 A-2(실측 타일 extent 기준
dense-tile 술어로 컬링을 건전화)는 리뷰에서 Critical로 적발 — 가상공간 extent
(~16384px) 대 디바이스공간 clip의 좌표계 이중성으로 영구 미발동(단일 좌표계
테스트가 이를 못 잡는 맹점) — 폐기하고 최종적으로 A-1(제거)만 채택.

**이는 선행 스펙 `2026-07-14-dcomp-swapchain-content-design.md` §5.3(레이어
컬링)의 기능 반전이다.** 근거: `is_opaque`가 클립 피복을 보장하지 않음(실측
확정), 월 정상상태에서 컬링이 실제로 발동한 사례가 0(시작 과도기 42건뿐 — 편익
자체가 실측상 없음), A-2 시도가 드러낸 좌표계 이중성. `cull_covered`/
`cull_disabled`/`SERVO_DCOMP_NO_CULL` 게이트는 코드와 함께 완전 소멸(+23/-126).

### 11.5 수용 기준 결과 (§9 대조)

1. **mixed_media_demo(=1) 45초**: 영상 6개·시계 1초 갱신·티커 스크롤·자막 애니
   전부 정상, 동결 0, withhold 무한 부재, panic 0 — **PASS**.
2. **quad_promote_partial 회귀 + 강등/재승격**: 기본(부분 Present, 강등 0)·
   `NO_PARTIAL_PRESENT`(강등 3회+쿨다운 300/600/1200) 양팔 픽셀 정확 — **PASS**.
3. **45타일 순수 월 무회귀(=1)**: lockstep 45/45(5분 후 완전 일치, 시작 시
   ±1 이내), 메모리 플랫(~4.7GB, ±150MB 진동, 누수 없음), PresentMon
   `Composed: Flip` 2575/2575(100%), catch-up 복사 0(완전 더티 프레임) 41건 실측
   — **PASS**. (부기: 초기 관측된 fps 절대치 "0.33/0.26" 소동은 측정
   앨리어싱으로 판명·정정됨 — 샘플 간격 30.37초가 소스 루프 주기와 우연히
   일치해 프레임 카운터가 정확히 1랩 돌아 보임. 랩 보정 및 클린 재측정 결과
   on≈29.96~30.0fps / off≈29.9~30.05fps로 **콘텐츠 정격 30fps 그대로 정상** —
   무회귀 판정에 영향 없음, 후속 조사 불요로 종결.)
4. **=surface/off 무회귀**: 전 요소 표시(=surface는 기지 backdrop 아티팩트만
   재확인 — 범위가 티커바까지 확장 관찰됐으나 hybrid·프로덕션 무영향; off는
   기준선과 바이트 등가 경로) — **PASS**.
5. **부분 Present 단위검증**: `cargo test -p servo-paint --lib --features
   paint_api/no-wgl dcomp` **7/7 PASS**(region_subtract/StaleTracker/승격 위생/
   강등 쿨다운/붕괴 포함, Task 6b에서 컬링 전용 테스트 2개 삭제로 9→7).
6. **패키징**: ServoWallPackage.zip 재생성 완료 — servoshell.exe 최신 빌드
   교체, `mixed_media_demo.html` + `dcomp_alpha_probe.html` 추가, `run_wall.ps1`
   AMD 판독 가이드에 "성능 A/B는 순수 비디오 월 페이지로, mixed_media_demo는
   정확성 확인 전용" 주의 명기.

### 11.6 이월

- `=surface` 전용 캡션/티커 backdrop 검정(반투명 자막·티커 배경이 순수 검정 —
  hybrid(=1)·프로덕션 경로 무영향, 진단 모드 한정 기지 결함 재확인만, 본수정
  범위 밖).
- ~~regen 영구 차단 시 `frame_dirty` 누적~~ → **최종 리뷰 픽스로 해소**(§11.8-2,
  geometry_changed 분기 클램프). 잔여 미세 한계 1건(최종 리뷰 판정 Minor·작위적):
  regen 차단 중 32렉트 초과 붕괴가 발생한 뒤 지오메트리가 regen 없이 옛 extent로
  정확히 복귀하는 경우, 과대 붕괴된 frame_dirty가 catch-up 차집합의 감수로 쓰여
  유니온 틈에서 1프레임급 일시 stale 가능(자가 치유). 향후 강화 옵션: 이 분기
  붕괴 발동 시 `partial_present=false` 2줄.
- Ctrl+F12 오버레이 미표시, 디바이스 로스트 로그 폭주 본수정, §4.5 다중 창
  레지스트리 — 기존 이월 그대로 유지.
- AMD 3중 A/B 실측·해석은 사용자 몫(패키지 판독 가이드 갱신 완료, §11.5-6 참조).

### 11.7 커밋 범위

`d8b1c57cb..c09817cc8`(15커밋 — Task 1 PoC부터 Task 6b A-1 컬링 제거까지, 스펙·
계획 문서 커밋 `3a8d6b9bf`/`b31327a45` 별도), 선행 스왑체인 콘텐츠 사이클
10커밋(`03946de91..21f6dab41`) 위 로컬. HEAD `c09817cc8`, 전부 미푸시 — 푸시는
AMD 실측 후 사용자 결정 대기(선행 사이클과 동일 정책).

### 11.8 최종 리뷰 픽스 (2026-07-15)

- Important #1(리젠 후 강등 데드엔드) 해결: regen 후 pre-attach(`content_attached
  =false`)인데 `fallback_virtual`이 `None`인 상태(regen이 fallback을 복원하지
  않음 — 첫 content-swap에서 이미 소모됨)를 별도 제3상태로 명시 식별 — 시딩
  불가로 판정해 단발 로그(영어) 후 해당 서피스의 강등 처리 자체를 보류
  (`demote_blocked`), content-swap 성공 또는 regen에서 재판정.
- Important #2(geometry_changed 지속 시 `frame_dirty` 무한 성장) 해결: 해당
  분기에서 `collapse_dirty_if_oversized(take, MAX_STALE_RECTS)` 클램프 —
  성장 상한 = 32렉트 + 1프레임 분. 잔여 미세 한계는 §11.6 참조.
- Minor 반영: 부분 Present 실패 시 dirty 드롭의 안전 근거 주석화,
  SetContent(swapchain) 실패 warn을 스왑체인당 1회로 게이트(regen 재장전).
- 확인 재리뷰 판정: **Ready to merge YES** (2026-07-15, fable 최종 리뷰어 —
  healthy 서피스 wedge 불가·상태 수명주기 정합 재추적 검증).

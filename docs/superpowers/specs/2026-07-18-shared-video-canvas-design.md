# 공유 비디오 캔버스 — underlay external 비디오 단일 스왑체인 설계

- 날짜: 2026-07-18
- 상태: 승인됨 (사용자 브레인스토밍 완료)
- 선행: §3-aa 비디오 WR 탈출(external), §3-ac AMD Present×N 직렬화 판독, §3-w 부분 Present 기계, §3-y 리사이즈/드래그 정합
- 참조: docs/superpowers/specs/2026-07-17-video-wr-escape-design.md (external 모드 정본), D:\2_TechReview\20260703_dx_wall_probe (probe 토폴로지 근거)

## 1. 배경과 목적

§3-ac AMD 실측(vesc-prof)으로 external 모드의 절벽 원인이 확정됐다: **Present×N 직렬화**. 비디오별 flip 스왑체인 46개에 프레임마다 Present를 호출하면 AMD 드라이버에서 Present당 0.67ms(30타일)→1.0ms(36타일)로 초선형 증가하고, 렌더러 스레드의 최대 75%가 Present 드라이버 코드에 갇혀 GPU 기아가 발생한다(컴포짓 18-26fps 붕괴). convert/acquire는 부차. 1차 패치(41d462b83 상태스왑 배치화+RTV 캐시)는 유지 가치는 있으나 절벽 해소에 불충분.

본 설계는 **underlay external 비디오 전체를 단일 컴포지션 스왑체인(공유 캔버스)에 draw**하고 프레임당 **Present 1회**로 마감한다. 월 45타일 기준 draw 45(현행 동일) + Present 46→2(캔버스1+콘텐츠1), DWM 비주얼 46→2 = probe(dx11_tile_renderer: 매 프레임 전 타일 draw + Present 1) 토폴로지와 문자 그대로 동형이 된다. 부수 이득: 캔버스 크기가 창 크기로 고정되므로 스케일 애니메이션 비디오의 스왑체인 재생성 처닝(전 스펙 §12.4 이월)이 underlay에서 소멸한다.

## 2. 완료 기준 (사용자 확정)

관례대로 **A5000 검증 + AMD는 사용자 A/B**:
- 개발기(A5000)에서 월 45타일 lockstep·fps·소크 무회귀 + 복합 3종 정상 + off/native/external/`=surface` 무회귀
- ServoWallPackage에 `-VideoEscape canvas` 스위치 + AMD 판독 가이드 갱신 — AMD 실측(36타일 절벽 해소 판독)은 사용자 몫

## 3. 범위 / 비범위

**범위**
- dcomp_compositor.rs: 캔버스 storage·2단계 프레임 알고리즘·underlay/overlay 판정
- dcomp_video_convert.rs: dest-rect(부분 뷰포트) draw 변형
- 게이트 `SERVO_VIDEO_ESCAPE=canvas` + 런처/패키지 스위치
- vesc-prof·SERVO_DCOMP_DEBUG 진단 확장

**비범위 (명시적 유지/이월)**
- overlay external 비디오(PiP류, WR 상한 4개): 기존 per-video 스왑체인 경로 무변경
- 레이아웃/WR 변경 0 (canvas의 레이아웃 플래그는 external과 동일)
- 페이싱 우회(media 직접 Present): 이월 유지 — canvas가 Present×N 자체를 없애므로 필요성 재평가는 AMD 실측 후
- v1 부분 클립 압착·PiP 둥근 모서리 사각 클립: 전 스펙 이월 그대로 (계약 무변경)

## 4. 게이트 설계 (사용자 확정: 신규 모드값)

- `SERVO_VIDEO_ESCAPE=canvas` 추가 (기존 off/native/external 무변경 보존 — AMD에서 external↔canvas 직접 A/B 가능).
- 레이아웃 플래그는 external과 동일(PREFER|SUPPORTS) — WR 프로모션/컷아웃/attach 거동 전부 동일, 차이는 컴포지터의 external surface 처리뿐.
- 런처 `-VideoEscape canvas` (set-or-clear 관례). AMD A/B는 4자: ①-DComp ②native ③external ④canvas, 핵심 판독 = ③↔④(present_ms 소멸·GPU%·fps).
- 판독 후 운영 레시피만 canvas로 전환 (external은 진단/폴백 레버로 존속).

## 5. 아키텍처

### 5.1 underlay/overlay 판정

WR은 add_surface를 z 순서(underlay들 → 콘텐츠 → overlay들)로 호출한다(전 스펙 §6, renderer mod.rs "z-order is implicit based on order added"). 프레임 시작 시 `content_seen=false`, 비-external add_surface(콘텐츠 서피스)에서 true로 승격. external surface가:
- `content_seen=false`에 오면 **underlay → 캔버스 대상**
- true면 **overlay → 기존 per-video 경로 무변경**

### 5.2 캔버스

- 스왑체인 1개: 창(백버퍼) 크기, FLIP 2버퍼, 기존 `create_composition_swapchain` 재사용, **premultiplied alpha**(비디오 없는 영역 투명 — 복합 페이지 반투명 패널의 배경 규약이 현행과 동일 유지). sync interval 0. **(정정 2026-07-19: §12 애드온으로 기본이 opaque(IGNORE)로 전환됨 — premultiplied는 `SERVO_VIDEO_CANVAS_PREMUL=1` 진단 레버로만 유지. 시각 등가 논증은 §12.1.)**
- 비주얼 1개: 최초 underlay external의 add_surface 시점에 트리 맨 아래로 삽입(이후 콘텐츠/overlay 비주얼이 기존 AddVisual(FALSE) 흐름으로 위에 쌓임). 클립 없음(창 전체).
- 생성: 최초 underlay external 등장 시 지연 생성. 포맷은 현행 external 스왑체인과 동일(8bit — 10bit 소스는 셰이더 변환 후 8bit 출력, external과 동일 계약).

### 5.3 프레임 알고리즘 (2단계, 전량 재드로우)

```
1단계 (add_external_surface, underlay마다):
    이번 프레임 목록에 (id, clip_rect, external_id) 기록만 — draw 없음
2단계 (end_frame, DComp Commit 직전):
    전 비디오 lease acquire → frame_seq로 더티 판정
    더티 합집합 공집합 → 전부 release, Present 스킵 (캔버스 무접촉)
    비공집합 → begin_batch → 전체 투명 클리어 → 전 비디오를 add 순서(z 보존)로
        dest=clip_rect에 VideoConvertPass 1-draw → Present1(더티 힌트) → release
```

- **더티 판정**: ① frame_seq 전진 ② rect가 지난 프레임과 상이(스케일/이동 애니) ③ 등장/소멸(소멸은 빈 자리 rect를 더티에 포함) ④ draw 성공↔실패 전이. 힌트 16개 초과 또는 (재)생성 직후 첫 Present는 전체(기존 상수/규약 재사용).
- **catch-up 불요 근거**: 매 프레임 전체 클리어+전량 재드로우이므로 flip 2버퍼 간 신선도 부기가 구조적으로 불필요 — 무갱신 비디오는 같은 소스 프레임을 재드로우해 픽셀 동일이 보장되고 더티 힌트 정합이 자동 성립한다.
- **Present 스킵**: 복합 페이지에서 시계/티커만 갱신된 컴포짓엔 캔버스 무접촉. 월은 매 컴포짓이 비디오 갱신이라 스킵 없음.
- **lease 보유**: 2단계 draw 루프 동안만(서브 ms). 링은 latest-wins라 writer(gst)는 잠긴 슬롯을 피해 계속 쓴다 — 기존 GL lock 규율과 동일.
- **뷰 규율**: plane SRV는 매 draw 신선 생성(★DYNAMIC(WRITE_DISCARD) 텍스처 뷰 캐싱 절대 금지 — §3-aa 재발방지 원칙★). 캔버스 백버퍼 RTV만 포인터 키 캐시(스왑체인 버퍼는 rename 없음 — 기존 rtv_cache 패턴). 배치는 41d462b83 begin_batch 재사용(프레임당 상태스왑 2회).

### 5.4 변환 패스 확장

VideoConvertPass에 dest-rect 변형 추가: RSSetViewports(+scissor)를 dest rect로 설정해 셰이더 무변경으로 부분 영역 draw. 색변환 의미론(4포맷·BT.601/709·limited/full·10bit 스케일)은 무변경.

## 6. 에러 처리 / 폴백

| 상황 | 처리 |
|---|---|
| 게이트 off/native/external | 각 모드 거동 무변경 (canvas는 순수 추가 경로) |
| 캔버스 스왑체인 생성 실패 (OOM급) | warn + 다음 프레임 재시도. 지속 실패 시 underlay 전체 미표시 — 기존 external 실패와 동급 수용 리스크 |
| lease acquire 실패 (해체 경합) | 해당 비디오만 이번 프레임 투명 구멍(1프레임, 컷아웃도 동시 소멸이라 실질 비가시). 해당 rect는 더티 포함(힌트 정합) |
| WR 프로모션 실패 | 해당 비디오 Blit=콘텐츠 경로 — external과 동일, 혼재 정상 |
| overlay 비디오 | per-video 경로 그대로 (≤4개) |
| 디바이스 로스트 | 기존 DComp 로스트 경로 편승 |

## 7. 수명 / 엣지 케이스

- **리사이즈**: 창 크기 변경 → 캔버스 재생성(전체 클리어+전량 재드로우+전체 Present). 드래그 중(`resize_active`) 재생성 억제·기존 캔버스 유지, draw는 기존 버퍼 크기로 클램프 — 현행 external 드래그 정책과 동일(§3-y 정합).
- **destroy_surface(underlay)**: 등록 해제만(per-video 자원 없음). 빈 자리는 다음 프레임 클리어+더티로 자연 처리.
- **`=surface` 콘텐츠 모드**: 캔버스는 콘텐츠 storage 방식과 독립 — 호환.
- **진단**: `SERVO_DCOMP_DEBUG=1`에 캔버스 생명주기(생성/재생성/Present/스킵 카운트)+최초 5프레임 계약 로그. vesc-prof에 canvas draw/present 시간 계측(AMD 판독용).
- **캡처**: 캔버스도 flip 스왑체인 — BitBlt/winshot 캡처 가능 유지(external과 동일).

## 8. 검증 계획

1. **유닛(WARP E2E)**: VideoConvertPass dest-rect — 한 타깃에 복수 비디오를 viewport 오프셋 draw, 픽셀 위치·색 검증 (`cargo test -p servo-paint --lib --features paint_api/no-wgl dcomp` 체계 확장).
2. **월 45타일(canvas)**: lockstep ±1, fps external 동등 이상, **PresentMon 스왑체인 46→2** + Composed:Flip, 30분 소크 WS 플랫.
3. **복합 3종**(mixed_media_demo / complex_media_stress / complex_media_transforms): 전 요소 정상 — overlay(PiP) per-video 혼재, WR 폴백(Z-회전/3D플립) 혼재, **스케일 애니 스왑체인 재생성 0**.
4. **리사이즈/드래그**: §3-y 시나리오 재실행 — 잔상/블랙 0.
5. **무회귀**: off/native/external 각 모드 + WebGPU 월 + `=surface` 조합.
6. **패키지**: run_wall.ps1 `-VideoEscape canvas` + AMD 4자 판독 가이드(핵심 ③external↔④canvas, present_ms/GPU%/fps 기준 명시), ServoWallPackage 재생성.

## 9. 접근 대안 검토 (기각 사유 기록)

- **B. 갱신분만 draw + §3-w catch-up 재사용**: 절감 대상(작은 쿼드 draw)이 저렴해 복잡도(콘텐츠 storage 결합 기계의 분리 리팩토링+영역별 stale 부기) 대비 이득 미미. 월은 lockstep 동시 갱신이라 A와 동비용. 기각.
- **C. DComp 가상 서피스 캔버스(BeginDraw/EndDraw)**: per-call 오버헤드 잔존(Present×N→BeginDraw×N), AMD 가상 서피스 경로 이득 없음 실측(§3-z ①≈②), 캡처 검정. 기각.
- **media 스레드 직접 Present**: 이월 유지(§3 비범위).

## 10. 알려진 리스크 (실측 전 미지수)

- DWM의 premultiplied 전창 캔버스 합성 비용(AMD GCN1) — 현행도 비주얼 46개 합성 중이므로 2개로 줄면 악화 방향은 아님. 판독 레버 = external↔canvas GPU%.
- Present1 더티 힌트는 월에서 사실상 항상 전체(45>16) — 월의 이득은 힌트가 아니라 Present 횟수 소멸에서 나온다(설계 의도와 일치). 힌트 이득은 복합 페이지 한정.
- 전체 클리어 1회/프레임(4K RGBA) — GPU 클리어 고속 경로라 사소 예상. AMD 실측에서 유의미하면 "불투명 월 감지 시 클리어 생략" 후속 레버.
- 렌더러 스레드의 2단계 acquire 45회/프레임 — 현행과 동수(위치만 add_surface→end_frame 이동), 신규 비용 아님.

## 11. 구현 결과 (2026-07-18)

전 태스크 완료 — 완료 기준(§2) 개발기(A5000) 몫 전부 충족. AMD 실측(4자 A/B)은 사용자 몫으로 이월(설계 의도).

### 11.1 커밋 체인

| 커밋 | 내용 |
|---|---|
| `0cd50e631` | 계획 정리 — `canvas_flush` 도달 불가 방어 분기 제거(드래그 중 무스왑체인이면 생성 허용으로 단순화) |
| `a87059126` | Task 1 — 게이트 `VideoEscapeMode::Canvas`(`SERVO_VIDEO_ESCAPE=canvas`, 레이아웃 프로모션 플래그는 external과 동일) |
| `82ed73cb4` | Task 2 — `VideoConvertPass::convert_to_rect`(dest-rect 부분 뷰포트 draw, RSSetViewports+scissor) + WARP E2E, 기존 `convert()`를 신규 경로로 위임하는 리팩토링 포함 |
| `9e0588b97` | Task 3 — `canvas_dirty_rects` 순수 함수(더티 판정 ①~④) + `present1_with_dirty` Present1 헬퍼 일반화 |
| `a38b6a44a` | Task 4 — `canvas_flush` 본체: underlay external 일괄 draw + Present1 1회, 캔버스 비주얼 트리 최하단 z 치환, 드래그 중(`resize_active`) 재생성 억제 |
| `58822215f` | Task 5 — A5000 런타임 검증 배터리 기록(6 스텝 전부 PASS) |

선행(이번 사이클 착수 전, 참고): `f7423143c`(AMD vesc-prof 판독 — Present×N 직렬화 확정, 본 설계의 착수 근거) → `9b96a8cab`(스펙 승인) → `542db02b0`(계획 승인).

### 11.2 스펙 대비 이탈

① **Task 5 결과 기록물의 리포 편입 방식 이탈**: 계획 문면은 "리포트 커밋"을 전제했으나, 이번 사이클은 리포 관례(전 사이클 이후 확립)에 따라 `.superpowers/sdd/canvas-task-5-report.md`를 비추적 스크래치로 남기고, 기록 자체는 원장 커밋(`58822215f`, progress.md 갱신)으로 갈음했다. 검증 내용·판정에는 영향 없음 — 커밋 대상 산출물의 형태만 계획 문면과 다르다.

② **검증 커맨드 결함 2건 발견(계획/브리프 문서 결함, 코드 무관, Task 5에서 실측 확인)**:
  - `complex_media_stress.html`/`complex_media_transforms.html`을 `-Cols 3 -Rows 3`으로 축소 실행하면 실제 비디오 수는 9그리드+PiP=10개다(브리프의 `-Sync 13`은 미지정 시 기본 `COLS=4` 그리드 12+PiP=13 기준). `-Sync 13`으로 그대로 실행하면 t+31s경 `Sync group timeout: releasing 10 of 13 pipelines` WARN이 1회 발생하지만 자기 복구형이며 이후 재생·lockstep에 부작용은 없다. 축소 그리드 재현 시 `-Sync 10` 권장.
  - `=surface` 진단 조합의 정확한 커맨드는 `-DComp -DCompSurface -VideoEscape canvas`이다(브리프 원문은 `-DCompSurface -VideoEscape canvas`로 `-DComp`가 누락). 누락된 채 실행하면 런처 내장 경고(`-DCompSurface requires -DComp; ignoring -DCompSurface (DComp stays off)`)와 함께 DComp 자체가 꺼진 채 실행되어 의도한 조합을 전혀 검증하지 못한다(Task 5에서 1차 시도로 실측 확인). `etc/multigpu/package_run_wall.ps1`/`run_video_wall_d3d11.ps1` 자체의 AMD A/B 헤더·`-VideoEscape` 주석에는 이 두 결함이 있는 커맨드가 애초에 실려 있지 않음을 확인했다(§11.5의 헤더 갱신은 처음부터 정확한 순수 월 커맨드만 사용).

③ **Task 4 리뷰 Minor 인지 항목(코드 수정 없음, 실측으로 무영향 확인)**:
  - 페이지 이탈→복귀 시 더티 힌트 규칙 ⑤(§5.3 ③ "등장/소멸")이 문면상 명시적으로 발동하지 않는 엣지가 있으나, 캔버스 비주얼이 트리에서 빠졌다 재추가되는 DComp Commit 자체가 전체 재합성을 유발해 실질적으로 커버됨 — Task 5 실측(45타일 lockstep, 복합 3종, 리사이즈/드래그)에서 이상 없음.
  - `vesc-prof`의 `converts`/`srv_creates` 카운터는 스킵·실패한 항목도 계상해 실제 유효 변환보다 과대 표시될 수 있음(진단 전용 카운터, 계획 정본 코드 그대로 — 판정 로직에는 영향 없음, `converts = presents × N` 정합성 확인에는 지장 없음).

④ 이번 사이클 자체에서 추가로 식별된 코드 결함·설계 이탈은 없음(①~③ 전부 문서/계획 결함이며 컴포지터 구현 코드는 스펙 §5 그대로). 아직 예정된 "최종 whole-branch 리뷰"(브랜치 전체 대상, Task 6 범위 밖)에서 추가 발견 시 후속 기록.

문면 이탈 3건(최종 whole-branch 리뷰에서 식별, 전부 타당한 방향 — 구현이 정본):
⑤ 캔버스 플러시 지점은 §5.3 문면(end_frame)이 아닌 **start_compositing**(close_external_batch 직전) — external 배치가 타일 GL 전에 닫혀야 하는 ANGLE 상태 규율(close_external_batch 앵커)과 동일 근거로 이쪽이 정확한 자리다.
⑥ lease 보유는 "draw 루프 동안만"이 아니라 **Present1 완료까지**(per-video external 경로 선례와 동일, 링은 latest-wins라 writer 무간섭).
⑦ §5.4의 "(+scissor)"와 달리 구현은 **viewport 전용**(래스터라이저 ScissorEnable FALSE — 풀스크린 삼각형은 뷰포트로 사상·클립되어 dest rect 밖 픽셀 무접촉, WARP E2E가 격리를 검증).

### 11.3 검증 수치 (A5000, 전거: `.superpowers/sdd/canvas-task-5-report.md`)

- **45타일 월**: `converts = presents × 45` 전 표본(6개) 정확 일치 — 프레임당 45비디오 전량 변환 후 캔버스 Present 1회라는 설계와 정합. `presents/frames` 비율 0.62~0.87(38~53 / 59~61, 더티 스킵으로 external의 프레임당 N Present 대비 극적 감소), `frames` 59~61(≈60fps 유지). 스크린샷 2매(5초 간격)에서 45/45 타일 카운터 완전 동일(±0) + 캡처 사이 재생 진행 확인.
- **PresentMon**: 20초 캡처(1951행), 고유 `SwapChainAddress` **2개**(캔버스+콘텐츠, 설계 목표 46→2 실측 확인), `PresentMode` = `Composed: Flip` **1951/1951(100%)**.
- **복합 3종 전부 PASS**: mixed_media_demo(6/6, `presents < frames` 재현) / complex_media_stress(9그리드+PiP, `canvas swapchain (re)create`=1, `external (re)create`=1 — PiP 오버레이 1개는 overlay 경로 그대로, sync-timeout WARN 1건은 §11.2②의 문서 결함) / complex_media_transforms(`canvas (re)create`=1 — **스케일 애니 구동 전 구간 포함 추가 재생성 0회**, 캔버스 스왑체인 처닝 소멸 확인; `external (re)create`=98은 overlay/이동·스케일 비디오의 기존 이월 결함으로 캔버스와 무관, 아래 §11.4 참조).
- **리사이즈/드래그**: 편측 성장 40스텝(1100×620→1900×1420)에서 `canvas swapchain (re)create` 정확히 2회(①최초 생성 ②정착 후 1회), **드래그 구간(40스텝) 중 추가 재생성 0회**(억제 확인). winshot 클린 캡처로 정착 후 잔상/검정 밴드 0, lockstep 정확 확인.
- **무회귀 4종 + WebGPU 월**: off / native(로그 기반 PASS, winshot 검정은 기지 사양) / external / `=surface`+canvas(§11.2②의 정정된 커맨드로 재실행 후 `dcomp=surface` 정상 진입) 전부 dcomp engaged=1, WARN 베이스라인 3종만, panic 0. WebGPU 다중 GPU 월(fanout+gpu-direct) 생존 확인, panic 0.
- **30분 소크**: 45타일 유지, WS 4,980MB(0min)→5,158MB(20min, +3.6%, 단조 증가 아님)→3,264MB(25min, 로그 이벤트 0건의 OS working-set 트림으로 판정)→3,247MB(30min, 트림 후 안정). 소크 전 구간 **신규 WARN/ERROR 0건**, panic 0. 종료 시 실화면 캡처: 45타일 30fps lockstep **±0** 유지.

### 11.4 이월 사항

- **AMD 실기 4자 A/B(핵심 이월, 설계 의도대로 사용자 몫)**: `-DComp` / `-VideoEscape native`(진단 전용) / `-VideoEscape external` / `-VideoEscape canvas` — §11.5 인계 패키지의 헤더 가이드가 판독 절차 정본. 핵심 판독은 ③↔④(present_ms 소멸·GPU%·36+타일 fps 회복 여부)이며, 이것이 본 사이클의 존재 이유(§1 Present×N 직렬화 가설의 AMD 확증)다.
- **overlay(PiP류) 비디오의 스왑체인 재생성 처닝** — 전 스펙(§12.4)에서 이미 이월된 결함이 이번 사이클에서도 재현 확인됨(complex_media_transforms, `external (re)create`=98/~30s). 캔버스는 underlay 전용이라 이 결함의 대상(overlay per-video 경로)을 건드리지 않음 — 범위 밖 유지, 근본수정은 별도 후속 과제.
- **v1 부분 클립 압착 리스크**(전 스펙 §11/12.2③) — canvas도 external과 동일한 WR 프로모션·컷아웃 계약을 쓰므로 리스크 성격 무변경, 검증 페이지 전부 무클립이라 이번 사이클에서도 미발현.
- **PiP `border-radius` 사각 클립 대체**(전 스펙 §12.2④) — overlay 경로 무변경이라 그대로 존속, 시각적 사소 저하.
- **native 모드 PiP 동결 결함**(전 스펙 §12.2⑤) — 근본수정 미착수, "진단 전용" 캐비앗으로 계속 우회. canvas는 external과 마찬가지로 이 결함 층(WR draw)을 타지 않음(설계상 자연 우회, 별도 검증 없음 — native만의 결함이므로).
- **페이싱 우회(media 스레드 직접 Present)**(§3 비범위) — 이번 사이클로 Present×N 자체가 소멸했으므로, AMD 실측에서 canvas 채택 후에도 페이싱 잔존 고정비가 지배적이면 그때 재평가(§9 C안 기각 사유와 별개로, WR 프레임빌드 고정비 자체는 canvas로 해소되지 않음).
- **불투명 월 감지 시 전체 클리어 생략 레버**(§10 리스크 3번째 항목) — A5000에서 클리어 비용이 관측 가능한 병목으로 드러나지 않아 착수 보류, AMD 실측에서 유의미하면 후속.

### 11.5 인계물

- `D:\ServoWallPackage\run_wall.ps1`(`etc/multigpu/package_run_wall.ps1`에서 재복사): `-VideoEscape canvas` 스위치 지원 + 헤더가 AMD **4자** A/B 절차(영어)로 갱신 — `(1) -DComp` 기준 / `(2) native`(진단 전용 캐비앗 명문화) / `(3) external`(N Presents/frame) / `(4) canvas`(1 Present/frame, RECOMMENDED), 핵심 판독 ③↔④(present_ms·GPU%·36+타일 fps), PresentMon 2 스왑체인 기대치, TileSize 불요 결론 포함.
- `etc/multigpu/run_video_wall_d3d11.ps1`의 `-VideoEscape` 파라미터 주석에 `canvas` 값 설명 추가(개발환경 런처도 패키지와 동일 스위치 사용 가능).
- `D:\ServoWallPackage.zip` 재생성(servoshell.exe + run_wall.ps1 교체, 나머지 리소스/테스트 페이지 무변경) — 크기/스모크 결과는 `.superpowers/sdd/task-6-report.md` 참조.
- **방법론 노트**: 캔버스 스왑체인도 flip 스왑체인이라 BitBlt/winshot 캡처 가능(external과 동일 특성 유지 — native/hybrid 서피스만 검정 캡처).

## 12. 애드온: 캔버스 알파 모드 opaque 전환 (2026-07-19, 승인됨)

**동기**: §5.2는 캔버스를 premultiplied로 확정했으나, §10 첫 리스크(구형 AMD GCN1에서 DWM의 premultiplied 전창 레이어 블렌딩 비용)를 사용자가 선제 회피하기로 결정. premultiplied→opaque는 DWM 합성을 블렌딩→불투명 합성으로 바꿔 어느 GPU에서든 같거나 싸진다.

### 12.1 시각 등가 논증 (설계 근거)

캔버스는 비주얼 트리 최하단이므로 알파 모드가 화면에 관여하는 영역은 "캔버스가 비쳐 보이는 곳"뿐이다:
- **비디오 rect**: 변환 셰이더가 알파=1 불투명 픽셀을 쓰므로 두 모드 결과 동일.
- **비디오 밖**: 위층 콘텐츠 레이어가 불투명(페이지 배경)이라 캔버스 비가시 — 검정이든 투명이든 무관.
- **반투명 패널/티커(알파 슬라이스)**: 자기 아래의 불투명 콘텐츠 슬라이스와 블렌딩되며 캔버스와 직접 만나지 않음.
- **과도기 구멍(lease 실패·시작 직후)**: premultiplied면 창 배경 브러시(§3-t '하단 흰 밴드'의 흰색)가 노출될 수 있으나 opaque면 검정 — 오히려 개선.

**유일한 이론적 예외 = 루트 배경이 반투명인 페이지**(html/body 알파<1). 브라우저 기본·표출 페이지 전부 불투명 배경이라 실존하지 않음 — 수용 제약으로 명시(그런 페이지가 등장하면 §12.3 복귀 레버 사용).

### 12.2 설계 (접근 A: 항상 opaque — B안 전면 피복 자동 감지는 실익 없는 복잡도로 기각)

- `ensure_canvas`의 스왑체인 생성: `create_composition_swapchain(size, opaque)`의 opaque를 **기본 true**(`DXGI_ALPHA_MODE_IGNORE`)로.
- 결정은 순수 함수(진단 env 판독 포함, OnceLock 캐시)로 분리 — 유닛테스트 대상.
- 전체 클리어는 (0,0,0,0) 그대로 — IGNORE 모드에서 알파가 무시되어 불투명 검정으로 표시됨(주석으로 의도 명시).
- `SERVO_DCOMP_DEBUG=1` 캔버스 생성 로그에 `alpha=opaque|premul` 표기.
- 그 외 로직(재생성/드래그/더티/Present/z) 전부 무변경. canvas 모드 밖 경로 접촉 0.

### 12.3 복귀 레버

`SERVO_VIDEO_CANVAS_PREMUL=1` = premultiplied 복귀(진단·AMD A/B 전용, 기본 미설정=opaque). 런처 스위치는 두지 않음(env 직접 설정) — AMD 가이드에 1줄 기재.

### 12.4 검증 계획

1. 유닛: 알파 모드 결정 함수(기본 opaque / env "1"이면 premul / 기타 값 opaque).
2. 실기(A5000): ①45타일 무회귀(lockstep·fps) ②**mixed/stress에서 premul↔opaque winshot 픽셀 비교로 §12.1 시각 등가 실증**(반투명 패널·티커 영역 포함) ③transforms 스케일 애니 정상 ④PresentMon 스왑체인 여전히 2개.
3. AMD 이득 실측은 관례대로 사용자 몫(§12.3 레버로 A/B).

### 12.5 완료 기준

A5000 검증 + AMD 가이드에 레버 기재 + 패키지 재생성. 푸시는 기존 보류 유지.

### 12.6 구현 결과 (2026-07-19)

Task 1 커밋 `53dd20d8b`(`components/paint/dcomp_compositor.rs`, +38/-4) — `canvas_alpha_opaque(Option<&str>) -> bool`(기본 opaque, `Some("1")`만 premul) + `canvas_swapchain_opaque()`(OnceLock env 바인딩) + `ensure_canvas`가 `create_composition_swapchain(size, opaque)`로 전환 + 로그 `alpha=opaque|premul` 표기. 전 태스크 완료, A5000 검증 전부 PASS.

**유닛**: `cargo test -p servo-paint --lib --features paint_api/no-wgl dcomp` → **23 passed, 0 failed**(`canvas_alpha_opaque_defaults_and_premul_lever` 포함, 기본 opaque/레버 "1"만 premul/기타 토큰 전부 opaque 확인).

**45타일 무회귀**(`-Cols 9 -Rows 5 -DComp -VideoEscape canvas -Sync 45`): `alpha=opaque` 마커 정확히 1회, `d3d11_active_markers=45/45`, `direct_file=45/45`, `dcomp_engaged_markers=1`(mode=hybrid). winshot 2매(5초 간격): 1차 44/45타일 000344·1타일 000343(±1 lockstep), 2차 45/45타일 000788 균일 — 정상 진행 확인.

**픽셀 프로브 표**(캡처 1936×1119, OS 창 크롬 포함 좌표): 정적 비-동적 지점(topbar 배경 간극, tickerbar 배경) 각 페이지 3곳, 전 프로브 채널 차 **0** (기준 ≤1 대비 여유 통과).

| 페이지 | 프로브 | 좌표 | opaque RGB | premul RGB | diff |
|---|---|---|---|---|---|
| mixed_media_demo (`-Cols 3 -Rows 2 -Sync 6`) | topbar 간극 | (1000,75) | (16,24,32) | (16,24,32) | (0,0,0) |
| mixed_media_demo | tickerbar 상단 | (1000,1005) | (11,18,32) | (11,18,32) | (0,0,0) |
| mixed_media_demo | tickerbar 우측 | (1850,1050) | (11,18,32) | (11,18,32) | (0,0,0) |
| complex_media_stress (`-Cols 3 -Rows 3 -Sync 10`) | topbar 간극 | (1150,75) | (16,24,32) | (16,24,32) | (0,0,0) |
| complex_media_stress | tickerbar 배경 | (1000,1095) | (11,18,32) | (11,18,32) | (0,0,0) |
| complex_media_stress | tickerbar 배경 우측 | (1900,1095) | (11,18,32) | (11,18,32) | (0,0,0) |

두 페이지 모두 반투명 패널(통계/스크롤러/자막)은 그리드 비디오 위에 직접 얹혀 있어(레이아웃상 그리드가 뷰포트 전체를 덮고 그 위에 fixed 오버레이) 프로브로 삼으면 캔버스 알파 모드가 아니라 촬영 시점 비디오 프레임 차이가 섞여 판독을 오염시킨다 — 브리프의 "패널 내부" 예시 대신 topbar/tickerbar의 완전 불투명 솔리드 배경(CSS 헥스와 정확히 일치, `#101820`/`#0b1220`)을 정적 프로브로 채택했다(브리프 "예: 배경 여백/패널 내부/티커 배경" 중 배경·티커 배경 옵션). premul 런 로그에서 `alpha=premul` 확인: mixed(`video_wall_d3d11_20260719_095003_stderr.log:200`), stress(`video_wall_d3d11_20260719_095358_stderr.log:331`). 두 opaque 런 로그도 `alpha=opaque` 1회씩 확인. stress 런(오프/온 양쪽)에서 sync-timeout WARN 없음(§11.2②의 이월 결함 이번 사이클엔 비재현).

**transforms 스케일 애니 무회귀**(`-Cols 3 -Rows 3 -DComp -VideoEscape canvas -Page tests\html\complex_media_transforms.html -Sync 10`): `canvas swapchain (re)create` 로그 **정확히 1회**(`alpha=opaque`), 관측 52초(스케일 애니 구동 구간 포함) 동안 추가 재생성 **0회**, panic 0건.

**패키지 재생성**: `D:\ServoWallPackage\servoshell.exe`(Task 1 빌드, 150,895,104B) + `D:\ServoWallPackage\run_wall.ps1`(`etc/multigpu/package_run_wall.ps1`에서 재복사, AMD 4자 가이드 끝에 `SERVO_VIDEO_CANVAS_PREMUL=1` 진단 레버 2행 추가) 반영 → `D:\ServoWallPackage.zip` **1,216,856,231B**(2026-07-19 09:57:55, 이전 1,216,854,445B/07-18 20:04 대비 헤더 2행분만 증가). 스모크(`-Cols 2 -Rows 2 -DComp -VideoEscape canvas`): `d3d11_active_markers=4/4`, `direct_file=4/4`, `dcomp_engaged_markers=1`(mode=hybrid) — 기동 마커 PASS.

**스펙 대비 이탈**: 문서 결함 1건 — 브리프(Step 2/3) 커맨드의 `-Page mixed_media_demo.html` / `-Page complex_media_stress.html` / `-Page complex_media_transforms.html`(파일명만)은 런처의 `$Page` 파라미터가 리포 루트 기준 상대 경로(`Join-Path $servoRoot $Page`)를 요구해 그대로 실행하면 파일을 찾지 못한다 — `tests\html\<name>.html`로 실행해 해소(§11.2②에 이미 기록된 동일 범주의 문서 결함 재발, 코드 무관). 그 외 이탈 없음 — 6/6 프로브 diff 0(기준 ≤1), 45타일·transforms 판정 전부 브리프 기준 그대로 충족.

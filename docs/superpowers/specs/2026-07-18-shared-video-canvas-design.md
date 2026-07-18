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

- 스왑체인 1개: 창(백버퍼) 크기, FLIP 2버퍼, 기존 `create_composition_swapchain` 재사용, **premultiplied alpha**(비디오 없는 영역 투명 — 복합 페이지 반투명 패널의 배경 규약이 현행과 동일 유지). sync interval 0.
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

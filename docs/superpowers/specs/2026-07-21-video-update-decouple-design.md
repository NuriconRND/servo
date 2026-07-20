# 설계: external 비디오 갱신을 WR 프레임 빌드에서 분리 (video-update-decouple)

- 날짜: 2026-07-21
- 브랜치: `video-update-decouple` (base: `multigpu-tiled-wall`)
- 상태: 설계 승인 대기
- 운영 모드 전제: `-DComp -VideoEscape external`

## 1. 문제 (근본원인 — 소스 확정)

증상: **작은 창에서는 36개 비디오 타일이 정상, 창을 키우면 프레임 하락.** AMD에서 재현(A5000 무증상). 변수는 타일 수(고정 36)가 아니라 "타일 수 × 타일 크기".

블랙박스 A/B로 다음이 실측 배제됨:
- Present 개수 아님 — 공유 캔버스(36→1)가 무효.
- WR picture-cache 래스터 아님 — external에서 `-TileSize` 무효.
- GPU 아님 — GPU 여유, **CPU 한 스레드 포화**.

소스 정독으로 확정한 근본원인:

**포화되는 스레드는 WebRender RenderBackend 스레드이고, 하는 일은 "비디오 프레임 도착마다 전체 씬 프레임 빌드"다.**

호출 경로:
```
비디오 1장 디코드 → htmlmediaelement → paint_api.update_images (epoch=None)
  → painter.rs:1954  pending_video_frame_updates stash + immediate_image_update=true
  → painter.rs:2027-2036  즉시-합성 게이트 (pending_frames==0)
  → painter.rs:1132  transaction.generate_frame(0, present=true)
       → RenderBackend가 기존 씬 전체로부터 프레임 재빌드
         (스페이셜 트리 갱신 + 전 프리미티브/클립 순회 + picture-cache
          월드공간 dirty/의존성 재계산) — O(씬 프리미티브 + 창 면적), 단일 스레드, CPU
```

36개 비디오 = 프레임당 36개 독립 트리거. `pending_frames==0` 가드가 "한 번에 하나"로 제한하나, 하나 끝나면 즉시 다음 발동 → **RenderBackend가 전체 씬 빌드를 back-to-back으로 돌며 포화.** 표시 fps = 빌드 처리율 = 1/(빌드 1회 시간). 창이 커지면 빌드 1회 비용↑ → fps↓.

증상 일치(6/6):

| 관찰 | 설명 |
|---|---|
| GPU 여유 / CPU 한 스레드 포화 | 프레임 빌드는 순수 CPU(래스터=GPU는 뒷단) |
| 큰 창서 하락 | 빌드 비용 ∝ 창 면적 |
| 여러 개일 때만(1개 OK) | 1개=60/s·가벼운 빌드 / 36개=back-to-back·무거운 빌드 |
| canvas(present 36→1) 무효 | present는 빌드 뒤 단계 |
| external -TileSize 무효 | 빌드의 트리/프리미티브 순회는 타일 크기 불변(타일=래스터=GPU만) |
| 하이브리드·external 둘 다 하락 | 트리거가 컴포지터보다 상류(painter.rs) — 모드 무관 |

### 아이러니 (기록)

이 메커니즘은 과거 Task 3 수정 그 자체다 — "4K 단일 비디오가 48fps에 갇힘"을 "비디오 갱신이 전체 씬을 60fps로 재합성"하게 만들어 해결했다. 비디오 1~4개엔 약, 36개×큰 창엔 독. **그때의 해결책이 지금의 병목.**

### external에서 이 빌드는 순수 낭비

external 모드에서 비디오는 DComp가 합성한다(WR 밖). 새 프레임이 필요로 하는 건 그 DComp 스왑체인 present + Commit뿐 — **WR 씬은 안 바뀐다.** 그런데 비디오 프레임마다 전체 씬 빌드를 강제한다.

## 2. 목표 / 비목표

목표:
- external 비디오 프레임 갱신이 **전체 씬 빌드를 트리거하지 않게** 한다. 비디오 표출 비용을 "씬 크기·창 면적·비디오 개수와 무관한 present"로 붕괴.
- WR·ANGLE·WebGL·일반 웹 렌더링 전부 유지. 렌더러 재작성 없음.
- 시계·티커·자막 등 실제 콘텐츠 변화는 기존대로 정확히 갱신.

비목표:
- WebRender/ANGLE 제거(별개·대규모, 이번 범위 아님).
- 하이브리드(`-VideoEscape` 미사용) 모드의 근본 개선(하이브리드는 비디오를 WR 안에서 그리므로 구조적으로 빌드가 필요 — 운영 모드가 아님).
- 일반 웹 임의 CSS 최적화.

## 3. 확정된 구조적 사실 (설계 근거)

- 컴포지터 소유권: `maybe_create`(dcomp_compositor.rs:1186) → WR `CompositorConfig::Native`(painter.rs:445-478). "렌더러 스레드 단일 인스턴스"(:116).
- 스레드 분리:
  - **빌드**(병목) = RenderBackend 스레드 (`transaction.generate_frame`).
  - **합성**(add_external_surface → provider.acquire → present_external → Commit) = painter/Renderer 스레드 (renderer.render 내부, ANGLE D3D 컨텍스트 소유).
- 비디오 픽셀은 이미 사이드채널: `paint_api::video_external_surface_provider().acquire(external_id)`(dcomp_compositor.rs:1499). WR 이미지 시스템 무관. WR이 주는 건 placement(transform/clip)뿐이고 컴포지터가 `last_placement`/`visual`/External storage로 캐시.

→ 비디오 present에 필요한 모든 것(lease=provider, placement=캐시, swapchain/visual=캐시)이 **painter 스레드에서 이미 손에 있다.** RenderBackend 빌드는 우회 가능.

## 4. 설계

### 4.1 컴포지터 핸들 공유 (무-WR-패치)

DComp 컴포지터 상태를 `Rc<RefCell<DCompNativeCompositor>>`로 보유. WR에는 이 셀에 위임하는 얇은 `Box<dyn Compositor>` 래퍼를 넘긴다. painter는 `Rc` 클론을 보유. 전부 단일 렌더러 스레드에서 도므로 `Rc<RefCell>` 안전.

- 재진입 불변조건: WR의 `render()`가 트레이트로 셀을 빌리는 구간과 painter의 fast-path 빌림이 **중첩되지 않는다**(동일 스레드, painter가 프레임당 두 경로 중 하나만 선택). RefCell 이중 대여 패닉을 코드 구조로 방지.

대안(비채택): WR Renderer에 API 신설 — 패치가 깊고 upstream 병합 부담↑.

### 4.2 `present_external_only(&mut self)` — 컴포지터 경량 경로 신설

동작:
1. `self.surfaces`의 External 엔트리만 순회.
2. 각 surface: `provider.acquire(external_id)` → `present_external(캐시된 last_placement의 clip/transform, 캐시 ref_size, is_opaque, resize_active)` → `provider.release`. 기존 `external_needs_present` dedup 유지(frame_seq 안 바뀐 건 스킵).
3. 마지막에 `Commit()` **1회**.

**스킵**: `begin_frame`/`RemoveAllVisuals`/`AddVisual` 재구성(비주얼 트리는 직전 실합성 배치 그대로), 콘텐츠 타일 bind, promote/regen/demote 상태머신, 콘텐츠 스왑체인 present. 기존 `present_external`을 그대로 재사용.

**배치 수명(정확성 필수)**: `present_external`은 첫 convert에서 `begin_batch`로 우리 `ID3DDeviceContextState`를 활성화한다. 이 배치가 열린 채 ANGLE/GL이 돌면 상태가 어긋난다(dcomp_compositor.rs:1776 주석). 정상 경로는 `start_compositing`이 타일 GL 직전에 닫는다. fast-path에는 그 후속 GL이 없지만, **`present_external_only`은 Commit 직전(또는 반환 직전)에 반드시 `close_external_batch`를 호출**해 배치를 닫고 다음 프레임/경로의 GL 안전성을 보장한다.

### 4.3 painter 게이트 라우팅 (painter.rs:2027-2036)

프레임의 pending 갱신이 **external 비디오 픽셀 전용**일 때 fast-path 선택:
- 조건: `immediate_image_update`(epoch=None 비디오 프레임)이며, **pending 디스플레이리스트/씬 변화 없음**, 그리고 해당 이미지들이 **현재 승격된 external 서피스**에 대응.
- 액션: `transaction.generate_frame`(전체 빌드) 대신 **공유 핸들로 `present_external_only`** 호출.
- 그 외(씬 변화 = 시계/티커 reflow → set_display_list, 또는 비-external/미승격 비디오): **기존 `generate_frame` 경로** 유지(전체 빌드+합성이 placement도 갱신).

경계 판정 보수적으로: external 승격 여부가 불확실하면 안전하게 `generate_frame`로 폴백(정확성 우선, 성능은 fast-path 적용 시에만 이득).

### 4.4 vsync 페이싱

도착마다 present하지 않는다. 도착 시 `external_dirty=true` 마킹 → refresh 틱(BaseRefreshDriver, painter.rs:403)마다 **1회** `present_external_only`(모든 승격 비디오의 최신 프레임 → Commit 1회). 36 Commit/프레임 방지, provider 링이 프레임당 최신만 제공.

### 4.5 빌드 경로와의 협조

- 씬 변화 시 `generate_frame`이 비주얼 트리+placement 재구성 → fast-path는 항상 최신 placement 사용(캐시가 실합성 직후 갱신됨).
- **리사이즈 중**: 기존 `dcomp_resize_active`(task-12b) 재사용 — fast-path는 리사이즈 동안 발동 억제, 빌드 경로가 가상 서피스로 운반(기존 드래그 로직 무변경).
- 승격/강등 전환기: fast-path는 **승격 완료된 External storage만** 대상. 전환 중(Virtual/전환 대기)이면 그 surface는 fast-path 스킵 → 다음 실합성에서 정상 처리.

### 4.6 Present×N 대비 (2단계, 조건부)

빌드 병목 제거 후 **혹시** 36 present/vsync가 새 병목으로 드러나면(과거 §3-ac 우려 — 이제 빌드 노이즈 없이 격리 측정 가능), `present_external_only` 내부에서 공유 캔버스로 붕괴(전 비디오를 창 크기 서피스 1개에 draw, present 1회). 설계상 pluggable 2단계로 분리하되, **1단계(빌드 분리)를 먼저 측정**하고 필요 시에만 착수.

### 4.7 킬스위치

`SERVO_VIDEO_DECOUPLE=0` → 현재 동작(비디오당 `generate_frame`) 복귀. 기본 on.

## 5. 데이터 흐름 요약

```
[비디오 프레임 도착]
  → painter: external-비디오-전용 판정?
      ├─ 예 → external_dirty=true (present 안 함)
      └─ 아니오(씬 변화) → transaction.generate_frame (기존 전체 빌드+합성)
[refresh 틱]
  → external_dirty && !resize_active ?
      → 공유핸들.present_external_only():
           for 각 승격 External surface:
               lease = provider.acquire(id); present_external(캐시 placement); provider.release
           Commit()   // 1회
      → external_dirty=false
```

## 6. 정확성 / 엣지 케이스

- placement 스테일: fast-path는 캐시 placement 사용. 레이아웃 변화는 set_display_list → generate_frame 경로가 placement 갱신하므로 fast-path는 항상 직전 실합성 기준으로 정확. 레이아웃이 바뀌는 프레임은 정의상 fast-path 대상이 아님(씬 변화).
- 리사이즈/드래그: `dcomp_resize_active`로 fast-path 억제 → 기존 task-12b 경로 유지(블랙 방지).
- 승격 전 첫 프레임/attach 전: External storage 없거나 `attached_external_id`=None이면 present_external 조기 반환(기존). fast-path도 동일하게 안전.
- dedup: `external_needs_present`(ring_id, frame_seq)로 갱신 없는 비디오는 재present 스킵 — 정지 영상에서 불필요 present 없음.
- 콘텐츠 정합: 시계/티커가 바뀌는 프레임엔 generate_frame이 돌아 콘텐츠+비디오 함께 최신. 비디오만 바뀌는 프레임엔 콘텐츠 불변이므로 fast-path만으로 정확.
- 스레드 안전: fast-path는 painter/Renderer 스레드(ANGLE D3D 컨텍스트 소유)에서만 실행. RenderBackend와 무관. RefCell 중첩 대여 금지 불변 유지.

## 7. 검증 계획 (게이트)

AMD 실기(필수 — A5000 무증상):
1. **주 게이트**: 큰 창·36타일에서 (a) RenderBackend 스레드 포화 해소(작업관리자/프로파일러), (b) fps 유지/개선.
2. 콘텐츠 정합: 시계/티커/자막이 계속 정상 갱신(정지·지연 없음).
3. 리사이즈/드래그: 블랙·잔상 없음(task-12b 무회귀).
4. 킬스위치 `=0`: 즉시 기존 동작 복귀(A/B 대조).

A5000(무회귀):
5. 45타일 lockstep·fps 무회귀, 소크 클린.

보조 계측(선택): painter.rs 빌드 시간 계측으로 "빌드 분리 전/후 RenderBackend 부하" 정량화. `present_external_only` present 수/Commit 수 프로파일(기존 `[vesc-prof]` 확장).

## 8. 리스크

- 게이트 판정 오류로 씬 변화를 fast-path로 오분류 → 콘텐츠 스테일. 완화: 불확실 시 generate_frame 폴백(정확성 우선).
- RefCell 재진입 패닉. 완화: 단일 스레드 + 프레임당 단일 경로 구조로 원천 차단, 디버그 assert.
- Present×N 재부상(2단계 필요). 완화: 1단계 후 격리 측정 → 필요 시 캔버스 붕괴.
- 승격 히스테리시스/전환과의 상호작용. 완화: fast-path는 승격 완료 External만 대상, 전환기는 실합성에 위임.

## 9. 범위 밖 (후속 후보)

- 하이브리드 모드 개선.
- Present×N 2단계(조건부 착수).
- 콘텐츠 자체가 매 프레임 바뀌는 페이지(고빈도 티커)에서 generate_frame O(면적) 비용 — 별도 사안.

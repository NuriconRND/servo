# 비디오 승격 히스테리시스 — 회전 타일 플래핑 제거 설계 (Servo-side)

- 날짜: 2026-07-20
- 상태: 설계 승인(사용자), 계획/구현 대기
- 관련: `docs/superpowers/specs/2026-07-17-video-wr-escape-design.md`(external 승격 경로), `docs/superpowers/specs/2026-07-20-video-escape-stable-swapchain-design.md`(별개 — 스케일 스왑체인)
- 대상: `components/layout/display_list/mod.rs`(주), 소량 shared

## 1. 배경과 문제

`SERVO_VIDEO_ESCAPE=external` 모드에서 Servo는 각 비디오 프리미티브에
`PREFER_COMPOSITOR_SURFACE | SUPPORTS_EXTERNAL_COMPOSITOR_SURFACE` 플래그를 부여하고
(`mod.rs:768-773`), WR이 매 프레임 승격 가능성을 판정한다. 승격 조건 핵심은
**변환이 2D scale/translation**일 것(`webrender/picture.rs:2701` `is_2d_scale_translation`).

**문제(플래핑):** Z-회전/3D-플립 애니메이션이 걸린 비디오 타일은 회전이 0°/180° 부근을
지날 때만 변환이 2D scale/translation으로 퇴화한다. 그래서 WR이 **승격(0°/180° 부근) ↔
강등(90°/270° 부근)** 을 반복하고, 그 경로 전환 프레임이 **깜빡임**으로 보인다
(`complex_media_transforms.html`의 spinZ 타일, 사용자 실측 확인).

### 1.1 실측 범위 (2026-07-20)

- external 모드에서만 발생(플래그가 external일 때만 부여 — `mod.rs:773` `Off => {}`).
- **회전/3D-애니 비디오에서만** 발생. 정적 그리드(scale=1, 프로덕션 월)·순수 2D 애니(scale/translate)는 무발생(항상 2D → 1회 승격 후 유지).
- 로그 증거: 시작 후 `create_external_surface`가 11개 배치가 아니라 **1개씩 불규칙** 발생(플래핑 서피스 clip이 spinZ 위치와 일치). WR 강등 시 비2D 변환은 승격 절대 불가(`picture.rs:2703-2706` — 승격 시 `get_relative_scale_offset` 크래시)라 **강등측 히스테리시스(회전 타일 승격 유지)는 불가능**.

## 2. 목표 / 완료 기준

- **목표**: 회전/3D-애니 비디오의 승격 플래핑을 제거해 깜빡임을 없앤다.
- **완료 기준**
  1. spinZ 타일이 콘텐츠 패스에 안정적으로 고정(플래핑 소멸) — 로그상 시작 후 신규 create 없음, 육안 깜빡임 소멸.
  2. bob(2D 스케일 애니) 및 정적 타일은 정상 승격(external 최적화 보존).
  3. 프로덕션 순수 월 45타일 무회귀.
  4. 순수 함수(2D 판정/히스테리시스 전이) 유닛 테스트 통과.

## 3. 범위 / 비범위

- **범위**: external 모드에서 `PREFER_COMPOSITOR_SURFACE` 플래그 부여를 **승격측 시간 히스테리시스**로 게이트.
- **비범위**:
  - WR 소스 수정/벤더링(불필요 — 플래그는 Servo가 부여).
  - `native` 모드(제거됨), overlay/마스크 승격 세부.
  - 스케일 스왑체인 처닝(별도 스펙, 이미 처리).

## 4. 근본원인 (코드 레벨)

승격 후보 플래그를 **매 프레임 무조건**(external이면) 부여하므로, WR이 순간적 2D 상태를
보고 즉시 승격한다. 회전 애니는 순간만 2D이므로 승격/강등이 반복(플래핑). **플래그 부여에
"안정적 2D"라는 시간 조건이 없는 것**이 근본원인.

## 5. 설계 (Servo-side 승격 히스테리시스)

핵심: 비디오의 변환이 **N프레임 연속 2D-scale-translation일 때만** `PREFER_COMPOSITOR_SURFACE`
플래그를 부여한다. WR은 승격 후보로 인식조차 안 하므로 콘텐츠 패스에 고정된다.

### 5.1 2D-scale-translation 판정 (WR 로직 복제)

WR의 `is_2d_scale_translation`(`util.rs:539`)은 webrender **내부** 트레이트라 Servo에서
import 불가 → 동일 로직을 Servo 헬퍼로 복제(euclid `LayoutTransform` = `Transform3D<f32,…>`):

```rust
// WR webrender/src/util.rs:539 와 동일. NEARLY_ZERO는 WR과 동일 값(1.0/4096.0)로 맞춘다.
const NEARLY_ZERO: f32 = 1.0 / 4096.0;
fn is_2d_scale_translation(t: &LayoutTransform) -> bool {
    (t.m33 - 1.0).abs() < NEARLY_ZERO && (t.m44 - 1.0).abs() < NEARLY_ZERO &&
    t.m12.abs() < NEARLY_ZERO && t.m13.abs() < NEARLY_ZERO && t.m14.abs() < NEARLY_ZERO &&
    t.m21.abs() < NEARLY_ZERO && t.m23.abs() < NEARLY_ZERO && t.m24.abs() < NEARLY_ZERO &&
    t.m31.abs() < NEARLY_ZERO && t.m32.abs() < NEARLY_ZERO && t.m34.abs() < NEARLY_ZERO &&
    t.m43.abs() < NEARLY_ZERO
}
```
`rotate(θ)`(rotateZ)는 `is_2d()`엔 true지만 `is_2d_scale_translation`엔 θ≠0/180에서 false —
회전을 정확히 배제한다(핵심). Servo는 애니메이션 변환을 매 프레임 **값으로** 스페이셜 트리에
push(`mod.rs:303 PropertyBinding::Value`)하므로 현재 회전값이 판정에 반영된다.

### 5.2 누적 2D 여부 (per-node taint)

스페이셜 트리 구축 루프(`mod.rs:284`, `scroll_tree.nodes` 순회, 부모가 자식보다 먼저)에서
per-node **taint**(비2D-scale-translation 조상 존재)를 1패스로 계산:
```
tainted[node] = tainted[parent] || (node is ReferenceFrame && !is_2d_scale_translation(node.info.transform))
```
`Vec<bool>`(노드 인덱스)로 빌더 필드에 저장. 루트 2개(`skip(2)`)는 false. 이는 누적 변환의
2D-scale-translation 여부를 보수적으로 근사(비2D 조상이 하나라도 있으면 taint) — 오탐(회전이
상쇄되는 퇴화 케이스에서 과보수)은 "승격 덜 함"뿐이라 안전(플래핑 방지에 부합).

### 5.3 히스테리시스 상태 (프레임 간 지속)

빌더는 프레임마다 새로 생성되므로 카운터는 **정적 맵**에 둔다(프로젝트의 static/OnceLock 패턴):
- `static HYSTERESIS: Mutex<HashMap<u64, HystEntry>>`, `HystEntry { streak: u32, last_frame: u64 }`.
- 키 = `fragment.base.tag.to_display_list_fragment_id()`(안정 u64, `base_fragment.rs:326`).
- 프레임 스탬프 = 정적 `AtomicU64`(디스플레이 리스트 빌드마다 +1).
- **정리(pruning)**: `last_frame`가 현재보다 M프레임(예: 300) 이상 뒤처진 엔트리는 제거(페이지 전환/스크롤로 사라진 비디오의 무한 증가 방지). 빌드 시작 시 1회 스윕.

### 5.4 플래그 게이트 (visit_image)

`mod.rs:768` external 분기에서, 비디오마다:
```
is_stable = !tainted[self.current_reference_frame_scroll_node_id.index]
streak = is_stable ? streak+1 : 0    // 정적 맵 갱신
if streak >= N:  common.flags |= PREFER_COMPOSITOR_SURFACE | SUPPORTS_EXTERNAL_COMPOSITOR_SURFACE
```
- spinZ: 순간만 2D → streak 리셋 반복 → N 못 채움 → 플래그 무 → 콘텐츠 패스 고정 → 깜빡임 소멸.
- bob/정적: 항상 2D → N 채움 → 플래그 부여 → 승격 유지.
- 정적 타일도 최초 N프레임만 콘텐츠 패스 후 승격(1회 지연, 무해).

`current_reference_frame_scroll_node_id`(`mod.rs:96`, 순회 중 `:673/692` 갱신)는 visit_image
시점에 비디오의 enclosing reference frame 노드를 가리킨다.

### 5.5 임계값 / 게이트

- 기본 `N = 10`(≈0.16초 @60fps). 회전 감지에 충분, 정적 승격 지연 최소.
- 킬스위치 env **`SERVO_VIDEO_ESCAPE_PROMOTE_HYSTERESIS`**: 미설정/양수=N(기본 10), `0`=히스테리시스 끔(구 동작=즉시 승격, A/B·롤백). 프로세스당 1회 lazy 읽기.

## 6. 변경 지점 요약

| 지점 | 변경 |
|---|---|
| `mod.rs` 헬퍼 | `is_2d_scale_translation(LayoutTransform)`, 히스테리시스 순수 전이 함수, 게이트 env 리더 |
| `mod.rs:284` 스페이셜 트리 루프 | per-node taint `Vec<bool>` 계산 → 빌더 필드 |
| `DisplayListBuilder` 구조체(`:88`) | `reference_frame_non_2d: Vec<bool>` 필드 |
| `mod.rs:768` visit_image external 분기 | taint 조회 + 정적 맵 streak 갱신 + N 게이트 |
| 정적 맵 + 프레임 카운터 | 파일 상단 static |

## 7. 검증 계획

1. **순수 함수 유닛 테스트**: `is_2d_scale_translation`(identity/scale/translate=true, rotateZ 45°/90°=false, rotateY=false, 0°=true), 히스테리시스 전이(연속 2D → N에서 발화, 리셋 → 0).
2. **객관(로그)**: transforms 페이지 `SERVO_DCOMP_DEBUG=1` → 시작 후 신규 `create_external_surface` **0**(플래핑 소멸). bob/정적은 시작 시 승격 유지.
3. **육안**: spinZ 깜빡임 소멸, bob 정상 승격 표시, 나머지 정상.
4. **무회귀**: 순수 월 45타일(정적) 정상 승격·lockstep, mixed/stress 정상.
5. **A/B**: 킬스위치 `=0`로 플래핑 재현(레버 유효성).

## 8. 리스크 / 완화

- **누적 근사 보수성**(§5.2): 회전 상쇄 퇴화 케이스에서 과보수 → 안전(승격 덜 함), 실용 무해.
- **정적 맵 수명**: 프레임 스탬프 pruning으로 누수 방지. 멀티 파이프라인(iframe) 키 충돌 가능성 → 필요 시 키에 pipeline_id 결합(설계 여지, 월은 단일 파이프라인).
- **NEARLY_ZERO 불일치**: WR과 정확히 같은 값(1/4096) 사용 — 계획에서 WR 소스 대조.
- 롤백: 킬스위치 `=0` 또는 커밋 revert.

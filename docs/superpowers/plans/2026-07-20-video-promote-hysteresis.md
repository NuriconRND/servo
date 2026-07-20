# 비디오 승격 히스테리시스 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** external 모드에서 비디오의 변환이 N프레임 연속 2D-scale-translation일 때만 `PREFER_COMPOSITOR_SURFACE` 플래그를 부여해, 회전/3D-애니 타일의 승격 플래핑(깜빡임)을 제거한다.

**Architecture:** WR의 `is_2d_scale_translation` 판정을 Servo에 복제하고, 스페이셜 트리에서 per-node 누적 2D taint를 계산한다. 비디오별 "연속 2D 프레임" 카운터를 정적 맵(키=DOM 태그 u64)에 유지하고, visit_image에서 임계 N 이상일 때만 승격 플래그를 부여한다. WR 무수정(벤더링 불요).

**Tech Stack:** Rust, `servo-layout` 크레이트, `webrender_api`(LayoutTransform=euclid Transform3D), `paint_api`(SpatialTreeNodeInfo).

## Global Constraints

- 변경은 `components/layout/display_list/mod.rs` 중심(shared 무변경).
- `LayoutTransform = Transform3D<f32, LayoutPixel, LayoutPixel>`(webrender_api::units), 필드 `m11..m44` pub. `is_2d_scale_translation`은 WR `webrender/src/util.rs:539`와 동일(`NEARLY_ZERO = 1.0/4096.0`).
- 안정 키 = `fragment.base.tag`(`Option<Tag>`) → `.to_display_list_fragment_id() -> u64`(`base_fragment.rs:326`).
- `info.transform: FastLayoutTransform` → `.to_transform()`로 `&LayoutTransform`(`mod.rs:303` 용례).
- `current_reference_frame_scroll_node_id: ScrollTreeNodeId`(`.index: usize`), 순회 중 `:673/692` 갱신.
- 임계값 env `SERVO_VIDEO_ESCAPE_PROMOTE_HYSTERESIS`: 미설정=기본 10, `0`=즉시 승격(구 동작). 프로세스당 1회.
- 유닛 테스트: `cargo test -p servo-layout --lib <filter>` (gstreamer bin PATH 선행/`PKG_CONFIG_PATH`; 대형 크레이트라 컴파일 다소 소요).
- 릴리즈 빌드: `.\mach build --release`(servoshell.exe kill 후). 커밋 메시지 한국어, Claude 서명 금지.

---

### Task 1: `is_2d_scale_translation` 순수 헬퍼 + 유닛 테스트

WR의 2D-scale-translation 판정을 복제(회전 배제). 유닛 테스트로 회전이 배제됨을 고정.

**Files:**
- Modify: `components/layout/display_list/mod.rs` (자유 함수 + 상수; 테스트는 신규 `#[cfg(test)] mod promote_tests`)

**Interfaces:**
- Produces: `const PROMOTE_NEARLY_ZERO: f32`, `fn is_2d_scale_translation(t: &LayoutTransform) -> bool`

- [ ] **Step 1: 실패 테스트 작성** — 파일 끝에 테스트 모듈 추가:

```rust
#[cfg(test)]
mod promote_tests {
    use super::*;
    use webrender_api::units::LayoutTransform;
    use euclid::Angle;

    #[test]
    fn identity_and_scale_translate_are_2d() {
        assert!(is_2d_scale_translation(&LayoutTransform::identity()));
        assert!(is_2d_scale_translation(&LayoutTransform::scale(0.78, 0.78, 1.0)));
        assert!(is_2d_scale_translation(&LayoutTransform::translation(30.0, -12.0, 0.0)));
        // scale + translate 조합도 2D
        let m = LayoutTransform::scale(0.9, 0.9, 1.0).then_translate(euclid::vec3(5.0, 5.0, 0.0));
        assert!(is_2d_scale_translation(&m));
    }

    #[test]
    fn rotatez_nonzero_is_not_2d_scale_translation() {
        // rotateZ(45deg): is_2d()엔 true지만 scale/translation은 아님 -> false
        let m = LayoutTransform::rotation(0.0, 0.0, 1.0, Angle::degrees(45.0));
        assert!(!is_2d_scale_translation(&m));
        // 90deg도 false
        let m90 = LayoutTransform::rotation(0.0, 0.0, 1.0, Angle::degrees(90.0));
        assert!(!is_2d_scale_translation(&m90));
    }

    #[test]
    fn rotatez_0_and_180_degenerate_to_2d() {
        // 0deg = identity -> true
        let m0 = LayoutTransform::rotation(0.0, 0.0, 1.0, Angle::degrees(0.0));
        assert!(is_2d_scale_translation(&m0));
        // 180deg = scale(-1,-1) -> 2D scale (회전 항 0) -> true (플래핑의 승격 순간)
        let m180 = LayoutTransform::rotation(0.0, 0.0, 1.0, Angle::degrees(180.0));
        assert!(is_2d_scale_translation(&m180));
    }

    #[test]
    fn rotatey_is_not_2d_scale_translation() {
        // 3D Y-플립: z 결합 -> false
        let m = LayoutTransform::rotation(0.0, 1.0, 0.0, Angle::degrees(45.0));
        assert!(!is_2d_scale_translation(&m));
    }
}
```

- [ ] **Step 2: 실패 확인**

Run: `cargo test -p servo-layout --lib is_2d`
Expected: 컴파일 실패("cannot find function `is_2d_scale_translation`").

- [ ] **Step 3: 헬퍼 구현** — 파일의 자유 함수 영역(예: 기존 `fn` 들 근처, impl 밖)에 추가:

```rust
/// WR `webrender/src/util.rs:539` `is_2d_scale_translation` 복제(WR 내부 트레이트라 import
/// 불가). 행렬이 순수 2D 스케일+평행이동인가(회전/스큐/z 결합 없음). rotateZ(θ)는 euclid
/// `is_2d()`엔 true지만 θ≠0/180에서 여기선 false — 회전을 배제한다. NEARLY_ZERO는 WR과 동일.
const PROMOTE_NEARLY_ZERO: f32 = 1.0 / 4096.0;
fn is_2d_scale_translation(t: &LayoutTransform) -> bool {
    let z = PROMOTE_NEARLY_ZERO;
    (t.m33 - 1.0).abs() < z && (t.m44 - 1.0).abs() < z &&
    t.m12.abs() < z && t.m13.abs() < z && t.m14.abs() < z &&
    t.m21.abs() < z && t.m23.abs() < z && t.m24.abs() < z &&
    t.m31.abs() < z && t.m32.abs() < z && t.m34.abs() < z &&
    t.m43.abs() < z
}
```

그리고 `LayoutTransform` import를 `mod.rs`의 webrender_api units use(`:47` 인근)에 추가:
`LayoutTransform` 토큰을 기존 `{ ... LayoutPixel, LayoutPoint, ... }` 목록에 삽입.

- [ ] **Step 4: 통과 확인**

Run: `cargo test -p servo-layout --lib promote_tests`
Expected: 4개 테스트 PASS.

- [ ] **Step 5: 커밋**

```bash
git add components/layout/display_list/mod.rs
git commit -m "승격 히스테리시스: is_2d_scale_translation 헬퍼(WR 복제, 회전 배제) + 유닛 테스트"
```

---

### Task 2: 게이트 + taint 계산 + 정적 히스테리시스 맵 + visit_image 배선

env 게이트, 정적 카운터 맵, 빌더 taint 필드/계산, visit_image 플래그 게이트를 배선한다.

**Files:**
- Modify: `components/layout/display_list/mod.rs`

**Interfaces:**
- Consumes: Task 1의 `is_2d_scale_translation`.
- Produces: `fn promote_hysteresis_frames() -> u32`, 정적 `PROMOTE_HYSTERESIS`/`PROMOTE_FRAME`, 빌더 필드 `reference_frame_non_2d: Vec<bool>`, `promote_frame: u64`.

- [ ] **Step 1: 게이트 리더 + 정적 맵 + import** — 파일 상단 자유 함수/정적 영역에 추가:

```rust
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// 승격 히스테리시스 임계값(env `SERVO_VIDEO_ESCAPE_PROMOTE_HYSTERESIS`). 미설정=10,
/// 0=즉시 승격(구 동작). 프로세스당 1회.
fn promote_hysteresis_frames() -> u32 {
    static N: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("SERVO_VIDEO_ESCAPE_PROMOTE_HYSTERESIS")
            .ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(10)
    })
}

struct PromoteHystEntry { streak: u32, last_frame: u64 }
/// 비디오별 연속 2D-scale-translation 프레임 수(키 = tag.to_display_list_fragment_id()).
static PROMOTE_HYSTERESIS: LazyLock<std::sync::Mutex<std::collections::HashMap<u64, PromoteHystEntry>>> =
    LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
/// 디스플레이 리스트 빌드마다 +1 되는 프레임 스탬프(pruning 기준).
static PROMOTE_FRAME: AtomicU64 = AtomicU64::new(0);
/// last_frame가 이만큼 뒤처진 엔트리는 제거(사라진 비디오 누수 방지).
const PROMOTE_PRUNE_AGE: u64 = 300;
```

- [ ] **Step 2: 빌더 필드 추가** — `DisplayListBuilder`(`:88`) 구조체에:

```rust
    /// 스페이셜 트리 노드별 "비2D-scale-translation 조상 존재" taint(add_all_spatial_nodes에서 채움).
    reference_frame_non_2d: Vec<bool>,
    /// 이번 빌드의 프레임 스탬프(승격 히스테리시스 맵 갱신/pruning 기준).
    promote_frame: u64,
```

`build()`의 빌더 생성(`:198` `DisplayListBuilder { ... }`)에 초기화 추가:

```rust
            reference_frame_non_2d: Vec::new(),
            promote_frame: {
                let f = PROMOTE_FRAME.fetch_add(1, Ordering::Relaxed) + 1;
                // pruning 스윕(빌드당 1회).
                if let Ok(mut map) = PROMOTE_HYSTERESIS.lock() {
                    map.retain(|_, e| e.last_frame + PROMOTE_PRUNE_AGE >= f);
                }
                f
            },
```

- [ ] **Step 3: taint 계산** — `add_all_spatial_nodes`(`:271`)의 노드 루프(`:284` `for node in scroll_tree.nodes.iter().skip(2)`)를 인덱스 포함으로 바꾸고 taint를 채운다. 루프 진입 전:

```rust
        let mut non_2d = vec![false; scroll_tree.nodes.len()];
```

루프 헤더를 `for (node_index, node) in scroll_tree.nodes.iter().enumerate().skip(2) {`로 변경하고, 루프 본문 안(mapping.push 전후 아무 곳, parent 계산 이후)에 taint 전파 추가:

```rust
            // 누적 taint: 부모가 tainted거나, 이 노드가 비2D-scale-translation reference frame.
            let parent_tainted = non_2d.get(parent_scroll_node_id.index).copied().unwrap_or(false);
            let self_non_2d = matches!(&node.info,
                SpatialTreeNodeInfo::ReferenceFrame(info)
                    if !is_2d_scale_translation(info.transform.to_transform()));
            non_2d[node_index] = parent_tainted || self_non_2d;
```

루프 뒤(`scroll_tree.update_mapping(mapping)` 인근)에 저장:

```rust
        self.reference_frame_non_2d = non_2d;
```

- [ ] **Step 4: visit_image 플래그 게이트** — `mod.rs:768` external 분기를 히스테리시스 게이트로 교체:

```rust
            match paint_api::rendering_context::video_escape_mode() {
                paint_api::rendering_context::VideoEscapeMode::External => {
                    // 누적 변환이 2D-scale-translation인가(회전 타일은 순간만 true).
                    let is_2d = self.reference_frame_non_2d
                        .get(self.current_reference_frame_scroll_node_id.index)
                        .map_or(true, |tainted| !*tainted);
                    // 비디오별 연속 2D 프레임 카운터 갱신 후, N 이상일 때만 승격 후보.
                    let promote = match fragment.base.tag {
                        Some(tag) => {
                            let key = tag.to_display_list_fragment_id();
                            let frame = self.promote_frame;
                            let mut map = PROMOTE_HYSTERESIS.lock().unwrap();
                            let e = map.entry(key)
                                .or_insert(PromoteHystEntry { streak: 0, last_frame: frame });
                            e.streak = if is_2d { e.streak.saturating_add(1) } else { 0 };
                            e.last_frame = frame;
                            e.streak >= promote_hysteresis_frames()
                        },
                        // 태그 없는(익명) 비디오는 히스테리시스 키가 없음 — 현재 프레임 2D면 승격(즉시).
                        None => is_2d,
                    };
                    if promote {
                        common.flags |= PrimitiveFlags::PREFER_COMPOSITOR_SURFACE |
                            PrimitiveFlags::SUPPORTS_EXTERNAL_COMPOSITOR_SURFACE;
                    }
                },
                paint_api::rendering_context::VideoEscapeMode::Off => {},
            }
```

- [ ] **Step 5: 빌드**

```bash
powershell -Command "Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force"
.\mach build --release
```
Expected: 컴파일 성공(에러 0).

- [ ] **Step 6: 커밋**

```bash
git add components/layout/display_list/mod.rs
git commit -m "승격 히스테리시스 배선: 스페이셜 트리 taint + 비디오별 연속 2D 카운터 게이트(기본 10프레임, SERVO_VIDEO_ESCAPE_PROMOTE_HYSTERESIS=0 즉시 승격)"
```

---

### Task 3: 라이브 검증 + 무회귀

**Files:** (검증만; 수정 필요 시 Task 2로 회귀)

- [ ] **Step 1: 플래핑 소멸 확인 (기본 N=10)**

```powershell
$env:SERVO_DCOMP_DEBUG = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Page "tests\html\complex_media_transforms.html" -Cols 4 -Rows 3 -Sync 12 -DComp -VideoEscape external -MoveX 1920 -MoveY 0 -LogPrefix "hyst_on" -Detach
# ~60s 재생 후 최신 hyst_on_*_stderr.log:
#   시작 후(11개 초기 승격 이후) 신규 'create_external_surface' 발생 0 = 플래핑 소멸
```
Expected: 초기 create(정적/bob 승격분) 이후 **신규 create 0**. spinZ가 external surface 안 받음(콘텐츠 패스 고정).

- [ ] **Step 2: 육안 확인**

```powershell
.\scratchpad\winshot.ps1 -OutPath "$env:CLAUDE_JOB_DIR\tmp\hyst_bob.png"
```
확인: spinZ(1행2열) 깜빡임 소멸(콘텐츠 패스로 계속 회전 렌더), bob(하단2열) 정상 승격·스케일 표시, 나머지 정상. lockstep 유지.

- [ ] **Step 3: 무회귀 — 순수 월 45타일**

```powershell
.\etc\multigpu\run_video_wall_d3d11.ps1 -Page "tests\html\video_grid_6x6_play.html" -Cols 9 -Rows 5 -DComp -VideoEscape external -MoveX 1920 -MoveY 0 -LogPrefix "hyst_wall" -Detach
```
확인: 45타일(정적) **N프레임 후 정상 승격**(create 45), lockstep·표시 무변화.

- [ ] **Step 4: 킬스위치 A/B (플래핑 재현)**

```powershell
$env:SERVO_DCOMP_DEBUG = "1"; $env:SERVO_VIDEO_ESCAPE_PROMOTE_HYSTERESIS = "0"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Page "tests\html\complex_media_transforms.html" -Cols 4 -Rows 3 -Sync 12 -DComp -VideoEscape external -MoveX 1920 -MoveY 0 -LogPrefix "hyst_off" -Detach
```
Expected: 시작 후 신규 create가 다시 산발 발생(spinZ 플래핑 재현) → 레버 유효.

- [ ] **Step 5: 검증 원장 커밋**

```bash
git commit --allow-empty -m "승격 히스테리시스 검증: transforms 플래핑 소멸(신규 create 0), 45타일 정상 승격 무회귀, 킬스위치 A/B 확인"
```

---

## Self-Review

**1. Spec coverage:**
- §5.1 is_2d_scale_translation 복제 → Task 1. ✓
- §5.2 per-node taint → Task 2 Step 3. ✓
- §5.3 정적 맵 + pruning → Task 2 Step 1·2. ✓
- §5.4 visit_image 게이트 → Task 2 Step 4. ✓
- §5.5 임계값/킬스위치 → Task 2 Step 1(`promote_hysteresis_frames`). ✓
- §7 검증 → Task 3. ✓

**2. Placeholder scan:** 모든 스텝에 실제 코드/명령/기대값. TBD 없음. ✓

**3. Type consistency:** `is_2d_scale_translation`/`promote_hysteresis_frames`/`PROMOTE_HYSTERESIS`/`PROMOTE_FRAME`/`reference_frame_non_2d`/`promote_frame` 이름이 Task 1·2에서 일관. `info.transform.to_transform()`(FastLayoutTransform→&LayoutTransform), `tag.to_display_list_fragment_id()`(u64), `ScrollTreeNodeId.index`(usize) 확인. `PrimitiveFlags::PREFER_COMPOSITOR_SURFACE` 기존 import 사용(`:770` 기존 용례). ✓

**참고:** `fragment.base.tag`는 `Option<Tag>`(`:700` 용례). None(익명 비디오)은 현재-프레임 2D면 즉시 승격(fallback) — 프로덕션 비디오는 DOM 노드가 있어 항상 Some.

# canvas-only 진단 레버 + 무하트비트 월 페이지 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 진단 게이트 `SERVO_DCOMP_CANVAS_ONLY=1`(캔버스 외 비주얼 생략 = DWM 1레이어, probe 동형)과 무하트비트 월 페이지로, 콘텐츠층 잔여 비용 3종(래스터/Present/DWM 레이어)을 AMD에서 분리 측정 가능하게 한다 (스펙 §13, 승인됨).

**Architecture:** 스펙 `docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md` §13. end_frame AddVisual 루프에서 "이번 프레임 캔버스 비주얼이 실제 추가될 때만" 그 외 비주얼을 생략 — WR/Present/상태머신 무접촉(표시 트리만 축소). 무하트비트 페이지는 perf 페이지 사본에서 `#stats` HUD+rAF tick 스크립트만 제거(비디오 그리드/재생 로직 유지).

**Tech Stack:** Rust (servo-paint), HTML 페이지 사본, PowerShell 검증.

## Global Constraints

- 리포 `D:\2_TechReview\20260606_multigpu_browser\servo`, 브랜치 `multigpu-tiled-wall`. **푸시 금지 — 로컬 커밋만.** 커밋 한국어, Claude 서명 금지.
- 게이트 기본 off(진단 전용). off일 때 코드 경로 무변경. 상태머신(승격/강등/부분 Present/coverage)·WR·Present 절대 무접촉 — **AddVisual 생략만**.
- 셸 포그라운드만(백그라운드 금지), 빌드/테스트 타임아웃 600000ms. 실행 후 servoshell kill + env 정리.
- **빌드 순서**: `Stop-Process servoshell` → `. .\etc\multigpu\servo_env.ps1` → `$ErrorActionPreference='Continue'` → `. .\.venv\Scripts\Activate.ps1` → `python mach build --release`.
- 유닛테스트: `cargo test -p servo-paint --lib --features paint_api/no-wgl <필터>`.
- 런처 `-Page`는 리포 루트 상대경로(`tests\html\...`) 필수. winshot은 `scratchpad\shot.png` 고정 출력 → 복사 필요.

---

### Task 1: 게이트 `SERVO_DCOMP_CANVAS_ONLY`

**Files:**
- Modify: `components/paint/dcomp_compositor.rs` — env 게이트 함수 2개(기존 `canvas_swapchain_opaque` 옆), `DCompNativeCompositor`에 `warned_canvas_only: bool` 필드+생성자 init, end_frame AddVisual 루프 분기, tests 모듈 유닛테스트

**Interfaces:**
- Consumes: end_frame의 기존 AddVisual 루프(캔버스 치환 분기 포함), `frame_canvas_items`, `canvas.content_attached`, OnceLock env 게이트 관례.
- Produces: `fn canvas_only_requested(token: Option<&str>) -> bool`(순수) + `fn dcomp_canvas_only() -> bool`(env `SERVO_DCOMP_CANVAS_ONLY`, OnceLock). Task 2 실험이 env 발동에 의존.

- [ ] **Step 1: 실패하는 테스트 작성**

tests 모듈(`canvas_alpha_opaque_defaults_and_premul_lever` 옆)에 추가:

```rust
    #[test]
    fn canvas_only_requested_only_on_literal_one() {
        assert!(!canvas_only_requested(None));
        assert!(canvas_only_requested(Some("1")));
        assert!(!canvas_only_requested(Some("0")));
        assert!(!canvas_only_requested(Some("true")));
        assert!(!canvas_only_requested(Some("")));
    }
```

- [ ] **Step 2: 실패 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl canvas_only`
Expected: 컴파일 실패 — `cannot find function canvas_only_requested`

- [ ] **Step 3: 구현**

`canvas_swapchain_opaque()` 아래에 추가:

```rust
/// canvas-only 진단 게이트 판정(스펙 §13). "1"일 때만 발동 — 캔버스 외 비주얼을
/// 트리에서 생략해 DWM 1레이어(probe 동형)로 만든다. 진단 전용(복합 페이지 UI 미표시).
fn canvas_only_requested(token: Option<&str>) -> bool {
    matches!(token, Some("1"))
}

/// canvas_only_requested의 env 바인딩(프로세스당 1회 캐시).
fn dcomp_canvas_only() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        canvas_only_requested(std::env::var("SERVO_DCOMP_CANVAS_ONLY").ok().as_deref())
    })
}
```

`DCompNativeCompositor` 필드(`warned_canvas_fail` 옆)에 `warned_canvas_only: bool` 추가 + 생성자에 `warned_canvas_only: false,`.

end_frame AddVisual 루프 — 루프 시작 전에 판정을 계산하고, 캔버스 치환 분기 **뒤**(비-캔버스 fallthrough 진입점)에 생략을 넣는다:

```rust
        if let Some(root) = self.root_visual_ptr() {
            // §13 canvas-only 진단: 이번 프레임 캔버스 비주얼이 실제로 추가될 때만
            // 그 외 비주얼 전부 생략(DWM 1레이어 — 콘텐츠층 잔여 비용 분리 측정).
            // 캔버스 미존재/미부착 프레임은 정상 합성(블랙아웃 방지). 상태머신·Present
            // 무접촉 — 표시 트리만 줄인다.
            let canvas_only = self.canvas_mode
                && dcomp_canvas_only()
                && !self.frame_canvas_items.is_empty()
                && self.canvas.as_ref().is_some_and(|c| c.content_attached);
            if canvas_only && !self.warned_canvas_only {
                self.warned_canvas_only = true;
                log::info!(
                    "[dcomp-native] canvas-only diagnostic active: suppressing non-canvas visuals"
                );
            }
            let mut canvas_added = false;
            for id in self.frame_surfaces.iter() {
                if self.canvas_mode && self.frame_canvas_items.iter().any(|(cid, _)| cid == id) {
                    // (기존 캔버스 치환 분기 무변경)
                    ...
                    continue;
                }
                if canvas_only {
                    continue; // §13: 콘텐츠/overlay 비주얼 생략
                }
                let Some(entry) = self.surfaces.get(id) else { continue; };
                // (기존 AddVisual 무변경)
                ...
            }
        }
```

(위 `...`는 기존 코드 그대로 유지한다는 뜻 — 신규 줄은 `canvas_only` 계산+warn-once와 `if canvas_only { continue; }` 뿐.)

- [ ] **Step 4: 테스트/스위트 확인 + 릴리스 빌드**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl canvas_only` → PASS
Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl dcomp` → 전부 PASS
릴리스 빌드(Global Constraints 순서) → 성공.

- [ ] **Step 5: 2×2 스모크 (게이트 on/off)**

```powershell
$env:SERVO_DCOMP_DEBUG = "1"; $env:SERVO_DCOMP_CANVAS_ONLY = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 2 -Rows 2 -DComp -VideoEscape canvas *> scratchpad\canvas_only_smoke.log
# 40초 → winshot 캡처 복사 → Stop-Process
```
판독: stderr 로그에 `canvas-only diagnostic active` 1회, 캡처에서 4비디오 정상 표시 + **#stats HUD 미표시**(perf 페이지 하트비트가 콘텐츠층에 있으므로 게이트 on이면 사라짐 — 발동의 시각 증거). 게이트 env 제거 후 재실행 → HUD 다시 표시(off 무변경). 종료 후 env 정리.

- [ ] **Step 6: 커밋**

```powershell
git add components/paint/dcomp_compositor.rs
git commit -m "canvas-only 진단 게이트: SERVO_DCOMP_CANVAS_ONLY=1이면 캔버스 외 비주얼 생략(DWM 1레이어, 스펙 13)"
```

---

### Task 2: 무하트비트 페이지 + 실험 매트릭스 + 가이드/패키지 + 스펙 기록

**Files:**
- Create: `tests/html/video_grid_wall_clean.html` (perf 페이지 사본 — `#stats` div(:59)+`#stats` CSS(:41-)+rAF tick 스크립트 블록(:79 stats 참조~:249) 제거, 그리드 생성/재생 로직 유지)
- Modify: `etc/multigpu/package_run_wall.ps1` (실험 매트릭스 가이드), `docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md` (§13.4 결과)
- 패키지: `D:\ServoWallPackage\`에 페이지+런처 복사, zip 재생성

**Interfaces:**
- Consumes: Task 1 게이트·로그 마커, `scratchpad\winshot.ps1`(shot.png 고정→복사), PresentMon(`D:\PresentMon-2.3.1-x64.exe`, 관리자·전경 필수).
- Produces: 실험 매트릭스 실측(A5000 기준선), AMD용 가이드. 검증 코드 변경 0.

- [ ] **Step 1: 페이지 사본 작성 + 단독 확인**

사본에서 제거: `#stats` CSS 블록, `<div id="stats">` 요소, `const stats = ...`부터 rAF tick 정의·호출까지의 진단 스크립트(그리드 생성·`play()` 로직은 유지 — 파일을 열어 의존 관계 확인 후 tick 전용 함수/변수만 제거). 저장 인코딩 UTF-8(BOM 무), 한글 주석 깨짐 없는지 확인.
확인 실행: `-Cols 3 -Rows 3 -DComp -VideoEscape canvas -Page tests\html\video_grid_wall_clean.html -Sync 9` — 9타일 재생 정상 + HUD 없음(winshot).

- [ ] **Step 2: A5000 매트릭스 3단 실측**

각 단 60초, `SERVO_DCOMP_DEBUG=1`+`SERVO_VIDEO_ESCAPE_PROF=1`, 9×5=45타일:
1. 기준: perf 페이지, 게이트 off
2. 무하트비트: `-Page tests\html\video_grid_wall_clean.html -Sync 45`
3. 무하트비트 + `$env:SERVO_DCOMP_CANVAS_ONLY="1"`

판독·기록: 각 단 vesc-prof(frames/presents/present_ms), ②에서 콘텐츠 승격 로그 소멸(promote 0 — 하트비트 슬라이스 부재) 확인, ③에서 `canvas-only diagnostic active` + winshot으로 45타일 표시 동일(갭 검정) 확인. 가능하면 ②/③에서 PresentMon 10초 — 프레젠팅 스왑체인 수 ①2 → ②1(캔버스만) 확인. A5000에서 fps 차이는 없을 것으로 예상(대역폭 여유) — 그 자체를 기준선으로 기록.

- [ ] **Step 3: AMD 가이드 갱신**

`etc/multigpu/package_run_wall.ps1` 헤더의 canvas readout 블록에 추가(영어):

```powershell
#   - Content-layer cost isolation (3-step matrix, wall only):
#       (a) canvas baseline    : .\run_wall.ps1 -Cols 9 -Rows 5 -Sync -1 -DComp -VideoEscape canvas
#       (b) + clean page       : add -Page tests\html\video_grid_wall_clean.html  (no HUD -> content raster/present drop to zero)
#       (c) + canvas-only      : also set SERVO_DCOMP_CANVAS_ONLY=1  (drops ALL non-video layers -> DWM composes 1 layer, probe-identical)
#     Read fps/GPU%% deltas: (a)->(b) = content raster+present cost, (b)->(c) = DWM extra-layer cost.
#     CAUTION: SERVO_DCOMP_CANVAS_ONLY is DIAGNOSTIC ONLY - page UI (tickers, clocks, PiP) disappears while set.
```

- [ ] **Step 4: 패키지 재생성 + 스모크**

```powershell
Copy-Item target\release\servoshell.exe D:\ServoWallPackage\ -Force
Copy-Item etc\multigpu\package_run_wall.ps1 D:\ServoWallPackage\run_wall.ps1 -Force
Copy-Item tests\html\video_grid_wall_clean.html D:\ServoWallPackage\tests\html\ -Force
Compress-Archive -Path D:\ServoWallPackage\* -DestinationPath D:\ServoWallPackage.zip -Force
```
(패키지 내 tests\html 경로가 다르면 실제 구조 확인 후 동일 위치에 배치.) 스모크: 패키지에서 (b) 커맨드 기동 마커 확인.

- [ ] **Step 5: 스펙 §13.4 결과 기록 + 커밋**

`### 13.4 구현 결과 (2026-07-19)` — 커밋 SHA, 매트릭스 3단 수치 표(frames/presents/present_ms, 스왑체인 수), 페이지 사본 상세, 이탈(없으면 "이탈 없음").

```powershell
git add tests/html/video_grid_wall_clean.html etc/multigpu/package_run_wall.ps1 docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md
git commit -m "무하트비트 월 페이지+콘텐츠층 비용 3단 매트릭스: A5000 기준선 실측, AMD 가이드 기재, 패키지 반영(스펙 13.4)"
```

---

## Self-Review 결과

1. **스펙 커버리지**: §13.1 게이트/무변경 보장/판정 순수함수/로그→Task 1, 무하트비트 페이지→Task 2 Step 1, §13.2 매트릭스→Task 2 Step 2-3, §13.3 완료 기준→Task 2 Step 4-5. 갭 없음.
2. **플레이스홀더**: 없음(페이지 제거 대상은 라인 근거 포함, `...`는 "기존 코드 무변경" 표기로 한정적 사용 — 신규 줄은 전부 명시).
3. **타입 일관성**: `canvas_only_requested(Option<&str>) -> bool`/`dcomp_canvas_only() -> bool` 정의=사용처 일치, env 이름 전 태스크 동일.

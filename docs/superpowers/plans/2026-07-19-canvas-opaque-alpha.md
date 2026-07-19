# 캔버스 알파 모드 opaque 전환 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 공유 비디오 캔버스 스왑체인의 기본 알파 모드를 premultiplied → opaque(IGNORE)로 전환해 구형 AMD의 DWM 전창 블렌딩 비용을 제거한다 (스펙 §12 애드온, 승인됨).

**Architecture:** `ensure_canvas`의 `create_composition_swapchain(size, false)` 한 곳의 opaque 플래그를 순수 함수 판정으로 바꾼다. 기본 opaque, 진단 env `SERVO_VIDEO_CANVAS_PREMUL=1`이면 premultiplied 복귀(AMD A/B 레버). 클리어 (0,0,0,0)은 IGNORE 모드에서 불투명 검정으로 표시되므로 무변경. 그 외 로직(재생성/드래그/더티/Present/z) 전부 무접촉.

**Tech Stack:** Rust (servo-paint), DXGI 알파 모드, PowerShell 검증 스크립트.

## Global Constraints

- 리포 `D:\2_TechReview\20260606_multigpu_browser\servo`, 브랜치 `multigpu-tiled-wall`. **푸시 금지(사용자 보류 유지) — 로컬 커밋만.** 커밋 메시지 한국어, Claude 서명 금지.
- 스펙 정본: `docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md` **§12**.
- 셸 명령은 전부 포그라운드(백그라운드 금지). 빌드/테스트 타임아웃 600000ms.
- **빌드 순서(매번 그대로):**
  ```powershell
  Stop-Process -Name servoshell -Force -ErrorAction SilentlyContinue
  cd D:\2_TechReview\20260606_multigpu_browser\servo
  . .\etc\multigpu\servo_env.ps1
  $ErrorActionPreference = 'Continue'
  . .\.venv\Scripts\Activate.ps1
  python mach build --release
  ```
- 유닛테스트: `cargo test -p servo-paint --lib --features paint_api/no-wgl <필터>`.
- canvas 모드 밖(off/native/external) 거동 무변경 — 변경 지점이 `ensure_canvas`(canvas 전용)뿐임을 유지.

---

### Task 1: 알파 모드 결정 함수 + ensure_canvas 전환

**Files:**
- Modify: `components/paint/dcomp_compositor.rs` (`video_escape_prof` :102 부근에 결정 함수 추가, `ensure_canvas` :1798-1845 수정, tests 모듈에 유닛테스트)

**Interfaces:**
- Consumes: 기존 `create_composition_swapchain(size, is_opaque)` (true=IGNORE, false=PREMULTIPLIED), `dcomp_debug()`, 기존 OnceLock env 게이트 패턴.
- Produces: `fn canvas_alpha_opaque(token: Option<&str>) -> bool` (순수, 테스트 대상) + `fn canvas_swapchain_opaque() -> bool` (env `SERVO_VIDEO_CANVAS_PREMUL` OnceLock 캐시) — Task 2 검증이 env 레버 거동에 의존.

- [ ] **Step 1: 실패하는 테스트 작성**

tests 모듈(기존 `parse_video_escape_token_accepts_canvas` 옆)에 추가:

```rust
    #[test]
    fn canvas_alpha_opaque_defaults_and_premul_lever() {
        // 기본(미설정) = opaque
        assert!(canvas_alpha_opaque(None));
        // 레버 "1"만 premultiplied 복귀 (스펙 §12.3)
        assert!(!canvas_alpha_opaque(Some("1")));
        // 그 외 값은 전부 opaque (오타/미지 토큰 안전 방향)
        assert!(canvas_alpha_opaque(Some("0")));
        assert!(canvas_alpha_opaque(Some("true")));
        assert!(canvas_alpha_opaque(Some("")));
    }
```

- [ ] **Step 2: 테스트 실패 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl canvas_alpha`
Expected: **컴파일 실패** — `cannot find function canvas_alpha_opaque`

- [ ] **Step 3: 최소 구현**

`video_escape_prof()`(:102 부근) 아래에 추가:

```rust
/// 공유 캔버스 스왑체인 알파 모드 판정(스펙 §12). 기본 opaque(IGNORE) — 구형 AMD의
/// DWM premultiplied 전창 블렌딩 비용 회피. `SERVO_VIDEO_CANVAS_PREMUL=1`일 때만
/// premultiplied 복귀(진단·A/B 레버, §12.3). 시각 등가 논증은 스펙 §12.1.
fn canvas_alpha_opaque(token: Option<&str>) -> bool {
    !matches!(token, Some("1"))
}

/// canvas_alpha_opaque의 env 바인딩(프로세스당 1회 캐시 — 기존 진단 env 게이트 관례).
fn canvas_swapchain_opaque() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        canvas_alpha_opaque(std::env::var("SERVO_VIDEO_CANVAS_PREMUL").ok().as_deref())
    })
}
```

`ensure_canvas`의 스왑체인 생성부(:1830-1831)를 교체:

```rust
        // 스왑체인 (재)생성. 기본 opaque(IGNORE) — 클리어 (0,0,0,0)은 알파가 무시되어
        // 불투명 검정으로 표시된다(스펙 §12.2; 빈 영역이 비가시임은 §12.1 등가 논증).
        // SERVO_VIDEO_CANVAS_PREMUL=1이면 premultiplied 복귀(§12.3 진단 레버).
        let opaque = canvas_swapchain_opaque();
        let created = self.create_composition_swapchain(size, opaque);
```

같은 함수의 dcomp_debug 로그(:1842-1844)를 alpha 표기 포함으로 교체:

```rust
                if dcomp_debug() {
                    log::info!(
                        "[dcomp-dbg] canvas swapchain (re)create {}x{} alpha={}",
                        size.width, size.height,
                        if opaque { "opaque" } else { "premul" }
                    );
                }
```

함수 doc 주석(:1798)의 "(창 크기, premultiplied)"도 "(창 크기, 기본 opaque — §12)"로 갱신.

- [ ] **Step 4: 테스트 통과 + 회귀 스위트 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl canvas_alpha`
Expected: PASS (1 passed)
Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl dcomp`
Expected: 전부 PASS

- [ ] **Step 5: 릴리스 빌드**

Global Constraints의 빌드 순서 그대로. Expected: 성공, `target\release\servoshell.exe` 갱신.

- [ ] **Step 6: 커밋**

```powershell
git add components/paint/dcomp_compositor.rs
git commit -m "캔버스 알파 모드 opaque 기본 전환: DWM 전창 블렌딩 회피, SERVO_VIDEO_CANVAS_PREMUL=1 복귀 레버(스펙 12 애드온)"
```

---

### Task 2: 실기 검증(픽셀 등가 실증) + 가이드/패키지 + 스펙 결과 기록

**Files:**
- Modify: `etc/multigpu/package_run_wall.ps1` (헤더 env 레버 1줄), `docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md` (§12.6 결과)
- 패키지(리포 밖): `D:\ServoWallPackage\run_wall.ps1`, `D:\ServoWallPackage.zip`, `D:\ServoWallPackage\servoshell.exe`
- 증거: `scratchpad\canvas_alpha\` (winshot 캡처, 픽셀 비교 결과)

**Interfaces:**
- Consumes: Task 1의 `SERVO_VIDEO_CANVAS_PREMUL` 레버와 `alpha=opaque|premul` 로그. `scratchpad\winshot.ps1`(캡처는 `scratchpad\shot.png` 고정 출력 — 매 캡처 후 복사 필요).
- Produces: 스펙 §12.6 판정 기록. 검증 코드 변경 0.

- [ ] **Step 1: 45타일 무회귀 + alpha=opaque 로그 확인**

```powershell
$env:SERVO_DCOMP_DEBUG = "1"
cd D:\2_TechReview\20260606_multigpu_browser\servo
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 9 -Rows 5 -DComp -VideoEscape canvas -Sync 45 *> scratchpad\canvas_alpha\wall45.log
```
(90초 내 타임아웃 허용 — servoshell은 계속 생존.) 판독: stderr 로그(`target\multigpu_logs\..._stderr.log` 최신)에서 `canvas swapchain (re)create 1920x1080 alpha=opaque` 1회. winshot 2매(5초 간격, `scratchpad\canvas_alpha\wall_a/b.png`로 복사) — 45/45 lockstep ±1·카운터 진행. 종료: `Stop-Process -Name servoshell -Force`.

- [ ] **Step 2: 픽셀 등가 실증 (opaque ↔ premul, 핵심)**

mixed_media_demo를 두 모드로 각각 기동·캡처:

```powershell
# 런1: opaque(기본)
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 2 -DComp -VideoEscape canvas -Page mixed_media_demo.html -Sync 6 *> scratchpad\canvas_alpha\mixed_opaque.log
# 30초 대기 후 winshot → scratchpad\canvas_alpha\mixed_opaque.png 복사, Stop-Process
# 런2: premul 레버
$env:SERVO_VIDEO_CANVAS_PREMUL = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 2 -DComp -VideoEscape canvas -Page mixed_media_demo.html -Sync 6 *> scratchpad\canvas_alpha\mixed_premul.log
# 30초 대기 후 winshot → scratchpad\canvas_alpha\mixed_premul.png 복사, Stop-Process
$env:SERVO_VIDEO_CANVAS_PREMUL = ""
```
premul 런의 stderr 로그에서 `alpha=premul` 확인(레버 발효 증거).

**비교 방법** — 두 캡처는 시점이 달라 비디오/시계/티커는 픽셀이 다른 게 정상. **비-동적 프로브 지점**(페이지 배경 여백, 반투명 패널 내부의 텍스트 없는 부분, 티커 밴드의 텍스트 없는 배경 — 캡처를 열어 3곳 이상 좌표 선정)의 RGB를 비교한다:

```powershell
Add-Type -AssemblyName System.Drawing
$a = [System.Drawing.Bitmap]::FromFile("D:\2_TechReview\20260606_multigpu_browser\servo\scratchpad\canvas_alpha\mixed_opaque.png")
$b = [System.Drawing.Bitmap]::FromFile("D:\2_TechReview\20260606_multigpu_browser\servo\scratchpad\canvas_alpha\mixed_premul.png")
# 프로브 좌표는 캡처 확인 후 확정 (예: 배경 여백/패널 내부/티커 배경)
$probes = @(@(30,30), @(960,540), @(200,1000))
foreach ($p in $probes) {
    $pa = $a.GetPixel($p[0], $p[1]); $pb = $b.GetPixel($p[0], $p[1])
    "probe ($($p[0]),$($p[1])): opaque=($($pa.R),$($pa.G),$($pa.B)) premul=($($pb.R),$($pb.G),$($pb.B))"
}
$a.Dispose(); $b.Dispose()
```
판독 기준: 전 프로브 채널별 차 ≤1 (스펙 §12.1 시각 등가). complex_media_stress(`-Cols 3 -Rows 3 -Sync 10`)로 동일 절차 1회 더(반투명 패널·PiP 페이지).

- [ ] **Step 3: transforms 스케일 애니 무회귀**

```powershell
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 3 -DComp -VideoEscape canvas -Page complex_media_transforms.html -Sync 10 *> scratchpad\canvas_alpha\transforms.log
```
~40초 관측 후 종료. 판독: `canvas swapchain (re)create` 정확히 1회(`alpha=opaque`), 스케일 애니 중 추가 재생성 0.

- [ ] **Step 4: AMD 가이드 레버 1줄 + 패키지 재생성**

`etc/multigpu/package_run_wall.ps1` 헤더의 canvas readout 블록 끝에 추가:

```powershell
#   - Diagnostic: set SERVO_VIDEO_CANVAS_PREMUL=1 to revert the canvas to premultiplied
#     alpha (A/B lever for DWM blend cost on old GPUs; default is opaque = cheaper).
```

패키지 재생성:
```powershell
Copy-Item D:\2_TechReview\20260606_multigpu_browser\servo\target\release\servoshell.exe D:\ServoWallPackage\ -Force
Copy-Item D:\2_TechReview\20260606_multigpu_browser\servo\etc\multigpu\package_run_wall.ps1 D:\ServoWallPackage\run_wall.ps1 -Force
Compress-Archive -Path D:\ServoWallPackage\* -DestinationPath D:\ServoWallPackage.zip -Force
```
스모크: 패키지에서 `.\run_wall.ps1 -Cols 2 -Rows 2 -DComp -VideoEscape canvas` 기동 마커 확인 후 종료.

- [ ] **Step 5: 스펙 §12.6 결과 기록**

`docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md` §12 말미에 `### 12.6 구현 결과 (2026-07-19)` 추가 — 커밋 SHA, 유닛/45타일/픽셀 프로브 수치(프로브 좌표·RGB 값 표), transforms 재생성 카운트, 패키지 zip 크기/시각. 이탈 있으면 명기, 없으면 "이탈 없음".

- [ ] **Step 6: 커밋**

```powershell
git add etc/multigpu/package_run_wall.ps1 docs/superpowers/specs/2026-07-18-shared-video-canvas-design.md
git commit -m "캔버스 opaque 전환 마감: 픽셀 등가 실증 기록(스펙 12.6), AMD 가이드에 premul 복귀 레버 기재, 패키지 재생성"
```

---

## Self-Review 결과

1. **스펙 커버리지**: §12.2 설계(결정 함수/ensure_canvas/클리어 유지/로그)→Task 1, §12.3 레버→Task 1(+가이드 Task 2), §12.4 검증 ①유닛→T1 ②실기 4항→T2 Step 1-3(PresentMon 2체인은 §12.4의 ④ — 스왑체인 수는 알파 모드와 무관하고 이전 사이클 실측 유효, T2 Step 1 로그의 단일 캔버스 생성으로 갈음: 이탈 아님·기록만), §12.5 완료 기준→Task 2 Step 4-5. 갭 없음.
2. **플레이스홀더**: 없음 (픽셀 프로브 좌표만 "캡처 확인 후 확정"으로 명시 — 실행 시 결정 사항이며 판독 기준(채널 차 ≤1)은 고정).
3. **타입 일관성**: `canvas_alpha_opaque(Option<&str>) -> bool` / `canvas_swapchain_opaque() -> bool` T1 정의 = T1 사용처 일치. env 이름 `SERVO_VIDEO_CANVAS_PREMUL` 전 태스크 동일.

# 비디오 external 스왑체인 안정화 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** external 비디오 서피스의 스왑체인 크기를 언스케일 풋프린트로 고정하고 표시 스케일을 DComp 비주얼 변환(SetTransform)으로 적용해, 스케일 애니메이션 타일의 매 프레임 스왑체인 재생성(실측 5653회)을 제거한다.

**Architecture:** 순수 헬퍼 3종(참조 크기 산정 / 재생성 판정 / 스케일 행렬)을 먼저 TDD로 만들고, external 서피스 경로(`add_external_surface`/`present_external`/`place_external_visual`)에 킬스위치 게이트 뒤로 배선한다. 정적 타일(scale=1)은 identity 행렬이라 무회귀. 검증은 라이브 월 재계측(로그 재생성 카운트) + 육안.

**Tech Stack:** Rust, `servo-paint` 크레이트, winapi(DirectComposition/DXGI), WebRender API units(`DeviceIntSize`/`DeviceIntRect`/`DeviceIntPoint`).

## Global Constraints

- 모든 변경은 `components/paint/dcomp_compositor.rs` external 서피스 경로 한정.
- 바인딩은 **winapi**(windows 크레이트 아님). `IDCompositionVisual::SetTransform_1(matrix: *const D2D_MATRIX_3X2_F)` (winapi dcomp.rs:179). `D2D_MATRIX_3X2_F`는 `winapi::um::dcommon`, 필드 `matrix: [[FLOAT; 2]; 3]`, 레이아웃 `[[m11,m12],[m21,m22],[dx,dy]]`(2D affine).
- `CompositorSurfaceTransform = ScaleOffset` — `transform.scale.x/.y`(f32), `transform.offset.x/.y`.
- 기본 **on**; 킬스위치 env **`SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN`** = `"0"`이면 구 동작(clip 크기 스왑체인, 매 프레임 재생성). 프로세스당 1회 lazy 읽기.
- 정적 타일(scale=1)은 identity 스케일 → 배치 결과 현재와 동일(무회귀 필수).
- 재생성 판정 허용오차 `SWAPCHAIN_REF_TOLERANCE_PX = 4`(각 축).
- 유닛 테스트: `cargo test -p servo-paint --lib --features paint_api/no-wgl <filter>` (gstreamer bin이 PATH 선행 또는 `PKG_CONFIG_PATH=C:/gstreamer/1.0/msvc_x86_64/lib/pkgconfig` 필요 — pkg-config 크레이트 경로 함정). 필요 시 `. .\etc\multigpu\servo_env.ps1` 선행.
- 릴리즈 빌드: `.\mach build --release` (`.venv` activate + `servo_env.ps1`). 빌드 전 `servoshell.exe` kill(링크 잠금 os error 5 방지).
- 커밋 메시지: 한국어, **Claude 서명(Co-Authored-By) 금지**.
- 표출 검증 런처: `.\etc\multigpu\run_video_wall_d3d11.ps1 -DComp -VideoEscape external` (창 우측 모니터 `-MoveX 1920 -MoveY 0`).

---

### Task 1: 순수 헬퍼 3종 + 유닛 테스트

external 서피스 스왑체인 크기·재생성·스케일 행렬의 순수 로직을 자유 함수로 분리하고 TDD로 검증한다. 배선(Task 2)에서 이 함수들을 호출한다.

**Files:**
- Modify: `components/paint/dcomp_compositor.rs` (자유 함수 3개 추가 — 기존 자유 함수 `refine_opaque_clip`(:206) 인근; 상수 1개; 테스트는 기존 `#[cfg(test)] mod tests`(:3057) 내부)

**Interfaces:**
- Produces:
  - `fn external_swapchain_ref_size(clip_size: DeviceIntSize, scale_x: f32, scale_y: f32) -> DeviceIntSize`
  - `fn external_swapchain_needs_recreate(has_swapchain: bool, current_ref: DeviceIntSize, new_ref: DeviceIntSize, tol: i32) -> bool`
  - `fn external_visual_scale_matrix(ref_size: DeviceIntSize, display_size: DeviceIntSize) -> [[f32; 2]; 3]`
  - `const SWAPCHAIN_REF_TOLERANCE_PX: i32 = 4;`

- [ ] **Step 1: 실패 테스트 작성** — 기존 `mod tests`(:3057) 안에 추가:

```rust
    #[test]
    fn ref_size_divides_out_scale() {
        // clip 473x345 at scale 0.993 -> unscaled footprint ~476x347
        let r = external_swapchain_ref_size(DeviceIntSize::new(473, 345), 0.993, 0.993);
        assert_eq!(r, DeviceIntSize::new(476, 347));
    }

    #[test]
    fn ref_size_scale_one_is_identity() {
        let r = external_swapchain_ref_size(DeviceIntSize::new(476, 347), 1.0, 1.0);
        assert_eq!(r, DeviceIntSize::new(476, 347));
    }

    #[test]
    fn ref_size_degenerate_scale_clamped() {
        // scale ~0 must not divide-by-zero; ref clamped to >=1
        let r = external_swapchain_ref_size(DeviceIntSize::new(10, 10), 0.0, 0.0);
        assert_eq!(r, DeviceIntSize::new(10, 10)); // scale treated as 1.0
        let r2 = external_swapchain_ref_size(DeviceIntSize::new(0, 0), 1.0, 1.0);
        assert_eq!(r2, DeviceIntSize::new(1, 1));
    }

    #[test]
    fn recreate_only_on_absence_or_large_change() {
        let a = DeviceIntSize::new(476, 347);
        // no swapchain -> must create
        assert!(external_swapchain_needs_recreate(false, a, a, 4));
        // jitter within tolerance -> no recreate
        assert!(!external_swapchain_needs_recreate(true, a, DeviceIntSize::new(478, 349), 4));
        // change beyond tolerance -> recreate
        assert!(external_swapchain_needs_recreate(true, a, DeviceIntSize::new(600, 347), 4));
    }

    #[test]
    fn scale_matrix_maps_ref_to_display() {
        // ref 476x347 shown at 390x285 -> sx=390/476, sy=285/347, no translate
        let m = external_visual_scale_matrix(DeviceIntSize::new(476, 347), DeviceIntSize::new(390, 285));
        assert!((m[0][0] - 390.0 / 476.0).abs() < 1e-6);
        assert!((m[1][1] - 285.0 / 347.0).abs() < 1e-6);
        assert_eq!(m[0][1], 0.0);
        assert_eq!(m[1][0], 0.0);
        assert_eq!(m[2], [0.0, 0.0]);
    }

    #[test]
    fn scale_matrix_identity_for_equal_sizes() {
        let m = external_visual_scale_matrix(DeviceIntSize::new(476, 347), DeviceIntSize::new(476, 347));
        assert_eq!(m, [[1.0, 0.0], [0.0, 1.0], [0.0, 0.0]]);
    }
```

- [ ] **Step 2: 실패 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl external_swapchain external_visual scale_matrix ref_size recreate`
Expected: 컴파일 실패("cannot find function `external_swapchain_ref_size`" 등).

- [ ] **Step 3: 헬퍼 구현** — `refine_opaque_clip`(:206) 인근에 추가:

```rust
/// External 비디오 서피스 스왑체인 재생성 허용오차(px, 각 축). 스케일 애니메이션의
/// ±1px 지터는 재생성하지 않고, 실 레이아웃 리사이즈만 잡는다.
const SWAPCHAIN_REF_TOLERANCE_PX: i32 = 4;

/// External 비디오 서피스의 언스케일 레이아웃 풋프린트: 화면 clip 크기를 컴포지터
/// 서피스 스케일로 나눈 값. 순수 CSS scale 애니메이션 동안 안정(scale transform은
/// 레이아웃 박스를 바꾸지 않는다)이라, 이 크기로 만든 스왑체인은 스케일 변화로
/// 재생성되지 않는다. scale이 퇴화(≈0)면 1.0으로 취급, 결과는 최소 1x1로 클램프.
fn external_swapchain_ref_size(clip_size: DeviceIntSize, scale_x: f32, scale_y: f32) -> DeviceIntSize {
    let sx = if scale_x.abs() > 1e-4 { scale_x.abs() } else { 1.0 };
    let sy = if scale_y.abs() > 1e-4 { scale_y.abs() } else { 1.0 };
    let w = (clip_size.width as f32 / sx).round() as i32;
    let h = (clip_size.height as f32 / sy).round() as i32;
    DeviceIntSize::new(w.max(1), h.max(1))
}

/// 스왑체인을 (재)생성해야 하는가. 참조 크기 고정 후에는 스케일 애니메이션이 재생성을
/// 트리거하지 않고, 스왑체인 부재 또는 참조 크기가 `tol`px 초과로 바뀐 경우(파괴 경로를
/// 우회한 실 레이아웃 리사이즈)만 재생성한다.
fn external_swapchain_needs_recreate(
    has_swapchain: bool,
    current_ref: DeviceIntSize,
    new_ref: DeviceIntSize,
    tol: i32,
) -> bool {
    !has_swapchain
        || (current_ref.width - new_ref.width).abs() > tol
        || (current_ref.height - new_ref.height).abs() > tol
}

/// 참조 크기 백버퍼를 화면 표시 크기로 스케일하는 D2D_MATRIX_3X2_F 내용
/// (`[[m11,m12],[m21,m22],[dx,dy]]`). 원점 기준 스케일만 담고 평행이동은 SetOffset이
/// 담당하므로 dx=dy=0. 정적(display==ref) 서피스는 identity.
fn external_visual_scale_matrix(ref_size: DeviceIntSize, display_size: DeviceIntSize) -> [[f32; 2]; 3] {
    let sx = display_size.width as f32 / ref_size.width.max(1) as f32;
    let sy = display_size.height as f32 / ref_size.height.max(1) as f32;
    [[sx, 0.0], [0.0, sy], [0.0, 0.0]]
}
```

- [ ] **Step 4: 통과 확인**

Run: `cargo test -p servo-paint --lib --features paint_api/no-wgl external_swapchain external_visual scale_matrix ref_size recreate`
Expected: 6개 테스트 PASS.

- [ ] **Step 5: 커밋**

```bash
git add components/paint/dcomp_compositor.rs
git commit -m "external 스왑체인 안정화: 참조 크기/재생성 판정/스케일 행렬 순수 헬퍼 + 유닛 테스트"
```

---

### Task 2: 게이트 + external 경로 배선

킬스위치 게이트를 추가하고 헬퍼를 `add_external_surface`/`present_external`/`place_external_visual`에 배선한다. 게이트 on이면 스왑체인을 참조 크기로 고정+SetTransform 스케일, off면 기존 동작.

**Files:**
- Modify: `components/paint/dcomp_compositor.rs`
  - 게이트 헬퍼(파일 상단 `dcomp_debug`(:74) 인근)
  - `use winapi::um::dcommon::D2D_MATRIX_3X2_F;` import 추가(:40 `d2dbasetypes::D2D_RECT_F` 인근)
  - `place_external_visual`(:1358)
  - `add_external_surface`(:1393)
  - `present_external`(:1463) 재생성 판정(:1491-1493)

**Interfaces:**
- Consumes: Task 1의 `external_swapchain_ref_size`, `external_swapchain_needs_recreate`, `external_visual_scale_matrix`, `SWAPCHAIN_REF_TOLERANCE_PX`.
- Produces: `fn stable_swapchain() -> bool` (게이트, 기본 true).

- [ ] **Step 1: 게이트 헬퍼 추가** — `dcomp_debug`(:74) 인근:

```rust
/// External 스왑체인 안정화 게이트(env `SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN`).
/// 기본 on; "0"이면 구 동작(clip 크기 스왑체인, 매 프레임 재생성)으로 복귀(AMD A/B/롤백).
fn stable_swapchain() -> bool {
    static STABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *STABLE.get_or_init(|| std::env::var("SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN").as_deref() != Ok("0"))
}
```

- [ ] **Step 2: import 추가** — `:40` 인근:

```rust
use winapi::um::dcommon::D2D_MATRIX_3X2_F;
```

- [ ] **Step 3: `place_external_visual`에 ref_size 파라미터 + SetTransform 추가**

`place_external_visual`의 시그니처를 `fn place_external_visual(&self, id: NativeSurfaceId, clip_rect: DeviceIntRect, ref_size: DeviceIntSize)`로 바꾸고, 로컬 클립을 `ref_size` 기준으로, 스케일 행렬을 적용한다. 게이트 off면 기존 동작(clip 크기 로컬 클립, SetTransform 미적용):

```rust
    fn place_external_visual(&self, id: NativeSurfaceId, clip_rect: DeviceIntRect, ref_size: DeviceIntSize) {
        let Some(entry) = self.surfaces.get(&id) else {
            return;
        };
        let offset_x = clip_rect.min.x as f32;
        let offset_y = clip_rect.min.y as f32;
        let stable = stable_swapchain();
        // 게이트 on: 백버퍼=ref_size, 표시 스케일은 SetTransform. off: 백버퍼=clip 크기(구 동작).
        let (clip_w, clip_h) = if stable {
            (ref_size.width as f32, ref_size.height as f32)
        } else {
            ((clip_rect.max.x - clip_rect.min.x) as f32, (clip_rect.max.y - clip_rect.min.y) as f32)
        };
        let local_clip = D2D_RECT_F { left: 0.0, top: 0.0, right: clip_w, bottom: clip_h };
        // Safety: visual은 ComOwned가 수명을 보장하는 살아있는 IDCompositionVisual.
        unsafe {
            let hr = (*entry.visual.as_ptr()).SetOffsetX_1(offset_x);
            if hr < 0 {
                warn!("[dcomp-native] external SetOffsetX failed (hr=0x{:08x})", hr as u32);
            }
            let hr = (*entry.visual.as_ptr()).SetOffsetY_1(offset_y);
            if hr < 0 {
                warn!("[dcomp-native] external SetOffsetY failed (hr=0x{:08x})", hr as u32);
            }
            if stable {
                let display = clip_rect.size();
                let m = external_visual_scale_matrix(ref_size, display);
                let matrix = D2D_MATRIX_3X2_F { matrix: m };
                let hr = (*entry.visual.as_ptr()).SetTransform_1(&matrix);
                if hr < 0 {
                    warn!("[dcomp-native] external SetTransform failed (hr=0x{:08x})", hr as u32);
                }
            }
            let hr = (*entry.visual.as_ptr()).SetClip_1(&local_clip);
            if hr < 0 {
                warn!("[dcomp-native] external SetClip failed (hr=0x{:08x})", hr as u32);
            }
        }
    }
```

- [ ] **Step 4: `add_external_surface`에서 ref_size 산정 + 전달**

`:1408` `let size = clip_rect.size();` 아래에 ref_size 산정을 추가하고, `place_external_visual` 호출(:1411)과 `present_external` 호출(:1454)에 전달:

```rust
        let size = clip_rect.size();
        // 게이트 on: 언스케일 풋프린트로 스왑체인 고정. off: 기존 clip 크기.
        let ref_size = if stable_swapchain() {
            external_swapchain_ref_size(size, transform.scale.x, transform.scale.y)
        } else {
            size
        };

        // Step 4-7/8(앞당김): 비주얼 배치 + z-order 기록은 provider 유무와 무관하게 항상.
        self.place_external_visual(id, clip_rect, ref_size);
```

그리고 `present_external` 호출을 ref_size 전달로:

```rust
        self.present_external(id, &lease, ref_size, is_opaque, resize_active, transform, clip_rect);
```

- [ ] **Step 5: `present_external` 재생성 판정을 참조 크기 기준으로**

`present_external`의 `size` 파라미터는 이제 참조 크기(ref_size)를 받는다. 시그니처 주석을 갱신하고 재생성 판정(:1491-1496)을 헬퍼로 교체:

```rust
        // Step 4-4: 스왑체인 필요 판정. 게이트 on이면 참조 크기 고정(스케일 애니는 재생성
        // 안 함, 허용오차 초과 변화만). off면 구 동작(size != 이전). resize_active 중 억제 유지.
        let need_new = match self.surfaces.get(&id).map(|e| &e.storage) {
            Some(SurfaceStorage::External(ext)) => {
                if stable_swapchain() {
                    external_swapchain_needs_recreate(
                        ext.swapchain.is_some(), ext.swapchain_size, size, SWAPCHAIN_REF_TOLERANCE_PX,
                    )
                } else {
                    ext.swapchain.is_none() || ext.swapchain_size != size
                }
            },
            _ => return,
        };
```

(이후 `create_composition_swapchain(size, is_opaque)`·`ext.swapchain_size = size`는 그대로 — `size`가 이제 참조 크기라 스왑체인이 참조 크기로 생성되고 `swapchain_size`에 저장된다. convert 패스는 이 백버퍼 전체에 그린다 — 무변경.)

- [ ] **Step 6: 빌드**

```bash
# servoshell 종료 후 빌드
powershell -Command "Get-Process servoshell -ErrorAction SilentlyContinue | Stop-Process -Force"
.\mach build --release
```
Expected: 컴파일 성공(경고 무방, 에러 0).

- [ ] **Step 7: 커밋**

```bash
git add components/paint/dcomp_compositor.rs
git commit -m "external 스왑체인 안정화 배선: 참조 크기 고정 + SetTransform 스케일(기본 on, SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN=0 복귀)"
```

---

### Task 3: 라이브 검증 + 무회귀

빌드한 바이너리로 재계측해 재생성 처닝 소멸을 객관 확인하고, 육안·무회귀·킬스위치 A/B를 확인한다.

**Files:** (변경 없음 — 검증만; 필요 시 후속 수정은 Task 2로 회귀)

- [ ] **Step 1: 처닝 소멸 재계측 (게이트 on)**

```powershell
$env:SERVO_DCOMP_DEBUG = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Page "tests\html\complex_media_transforms.html" -Cols 4 -Rows 3 -Sync 12 -DComp -VideoEscape external -MoveX 1920 -MoveY 0 -LogPrefix "stable_on" -Detach
# ~60s 재생 후(다른 작업으로 시간 경과), 최신 stable_on_*_stderr.log에서:
#   grep -c 'external swapchain (re)create'  → 승격분(~11)만, surface 15 반복 소멸
```
Expected: `external swapchain (re)create` 총 ~11(시작 승격분), `NativeSurfaceId(15)` 반복 **0**. (게이트 on 전 5653 대비.)

- [ ] **Step 2: 육안 확인 (PrintWindow 캡처)**

```powershell
.\scratchpad\winshot.ps1 -OutPath "$env:CLAUDE_JOB_DIR\tmp\stable_bob.png"
```
확인: bob 타일(하단 2번째, "이동·스케일")이 스케일 애니메이션 중 **크롭/스트레치/블랙 없이** 정상 위치·크기. 정적 타일 11개 정상. lockstep(모든 프레임 카운터 동일) 유지.

- [ ] **Step 3: 무회귀 — 순수 월 45타일**

```powershell
.\etc\multigpu\run_video_wall_d3d11.ps1 -Page "tests\html\video_grid_6x6_play.html" -Cols 9 -Rows 5 -DComp -VideoEscape external -MoveX 1920 -MoveY 0 -LogPrefix "stable_wall" -Detach
```
확인: 45타일(전부 scale=1) 표시·lockstep 무변화. `external swapchain (re)create`는 시작 승격분만.

- [ ] **Step 4: 킬스위치 A/B (게이트 off로 구 동작 재현)**

```powershell
$env:SERVO_DCOMP_DEBUG = "1"; $env:SERVO_VIDEO_ESCAPE_STABLE_SWAPCHAIN = "0"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Page "tests\html\complex_media_transforms.html" -Cols 4 -Rows 3 -Sync 12 -DComp -VideoEscape external -MoveX 1920 -MoveY 0 -LogPrefix "stable_off" -Detach
```
Expected: `external swapchain (re)create`가 다시 수천 회(surface 15 처닝 재현) → 레버 유효.

- [ ] **Step 5: 결과 기록 + 커밋(검증 원장)**

검증 수치를 스펙/플랜 인근 또는 커밋 메시지에 기록:

```bash
git commit --allow-empty -m "external 스왑체인 안정화 검증: transforms 재생성 5653→~11(게이트 on), 45타일 무회귀, 킬스위치 A/B 확인"
```

---

## Self-Review

**1. Spec coverage:**
- §5.1 참조 크기 산정 → Task 1 `external_swapchain_ref_size` + Task 2 Step 4. ✓
- §5.2 재생성 판정 변경(허용오차) → Task 1 `external_swapchain_needs_recreate` + Task 2 Step 5. ✓
- §5.3 SetTransform 배치 → Task 1 `external_visual_scale_matrix` + Task 2 Step 3. ✓ (클립/변환 좌표계 상호작용은 Task 3 Step 2 육안 검증 — scale≤1이라 로컬 클립 (0,0)-(ref)가 표시 풋프린트를 크롭하지 않음.)
- §5.4 convert 패스 무변경 → Task 2 Step 5 주석 명시. ✓
- §5.5 데이터 구조 → `swapchain_size` 의미를 "참조 크기"로 재정의(신규 필드 불요, Task 2 Step 5). ✓
- §6 게이트 → Task 2 Step 1 `stable_swapchain()`. ✓
- §8 검증 → Task 3 전부. ✓

**2. Placeholder scan:** 모든 스텝에 실제 코드/명령/기대값 포함. TBD/TODO 없음. ✓

**3. Type consistency:** `external_swapchain_ref_size`/`_needs_recreate`/`external_visual_scale_matrix`/`SWAPCHAIN_REF_TOLERANCE_PX`/`stable_swapchain` 이름이 Task 1 정의와 Task 2 호출에서 일치. `D2D_MATRIX_3X2_F { matrix: [[f32;2];3] }` winapi 레이아웃 일치. `present_external`의 `size` 파라미터 의미를 참조 크기로 재정의(Task 2 Step 4·5 일관). ✓

**참고:** §5.5의 "swapchain_size 재정의"를 택함 — `present_external`이 받는 `size`가 이제 clip 크기가 아니라 참조 크기이므로, 스왑체인은 참조 크기로 생성되고 convert 패스는 그 백버퍼 전체에 그린다. 표시 스케일은 오직 SetTransform이 담당한다.

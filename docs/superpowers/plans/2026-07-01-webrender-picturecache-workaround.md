# WebRender picture-caching 우회 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 큰 단일 per-GPU surface에서 all-dynamic 비디오 월의 프레임레이트를, WebRender 소형 포크의 env 게이트로 picture caching을 꺼서 회복한다 (Servo/Paint 소스 무수정).

**Architecture:** 레지스트리의 `webrender` 0.68.0을 로컬(`../webrender/webrender`)로 복사해 `tile_cache.rs` 한 곳만 패치 — env `WR_DISABLE_PICTURE_CACHING`가 켜지면 루트 slice를 `PictureCompositeMode::TileCache` 대신 `None`(passthrough) picture로 방출해, 매 프레임 타일 dirty/의존성 추적(`picture.rs:5415-6009`, 측정된 ~400-600ms)을 통째로 스킵. 기존 `[patch.crates-io]` 슬롯으로 연결.

**Tech Stack:** Rust, WebRender 0.68.0, Servo(포크), winit_wall 예제, PowerShell(빌드/측정), 기존 `paint::painter` 계측(`Render perf`).

## Global Constraints

- 플랫폼: Windows. 모든 빌드/실행 전 `. ..\scripts\servo_env.ps1` (servo/ 에서 dot-source).
- 빌드는 **release만** (perf 측정 유효성). 빌드 명령: `cargo build --release -p servo --example winit_wall --features media-gstreamer,no-wgl -j 8`.
- 브랜치: `video-perf-investigation`.
- **Servo/Paint 소스 무수정.** 리포 변경은 `Cargo.toml`의 `[patch.crates-io]` 활성화 + 재현용 `.patch` 파일 + (선택) 프로브 페이지뿐.
- 포크 대상은 `webrender` 크레이트 1개만 (`webrender_api`="0.68", `wr_malloc_size_of`="0.2.2"는 crates.io 그대로).
- 로그 타깃은 `paint` (lib name). 측정 시 `RUST_LOG=warn,paint=info`.
- 경로:
  - 레지스트리 소스: `C:\Users\nuricon\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\webrender-0.68.0`
  - 포크 위치: `F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\webrender\webrender` (= servo/ 기준 `../webrender/webrender`)
  - servo 리포: `F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\servo`
- 측정 헬퍼(기존, DLL 복사 포함): `C:\Users\nuricon\AppData\Local\Temp\claude\F--20260609-SDWall-BrowserTest-20260606-multigpu-browser\1265d766-4357-4d98-a011-7f1393985e51\scratchpad\run_perf_measure.ps1`
  - 사용법: `pwsh -NoProfile -File <runner> -Label <name> -DurationSec <n> -Layout <json> -Page <html>`
  - env는 러너 호출 전에 `$env:...`로 설정하면 자식 프로세스에 상속됨.

---

### Task 1: 문제 재현(베이스라인) 측정

**Files:**
- (없음 — 측정만)

**Interfaces:**
- Produces: 베이스라인 수치 — 16영상@3840×3240에서 `avg_update_ms` ~수백 ms, `composite_fps` ~1-2. 이후 Task 4 비교 기준.

- [ ] **Step 1: 현재 release 빌드 최신화**

servo/ 에서:
```powershell
. ..\scripts\servo_env.ps1
cargo build --release -p servo --example winit_wall --features media-gstreamer,no-wgl -j 8
```
Expected: `Finished ... release`.

- [ ] **Step 2: 베이스라인 측정 (picture caching 켜진 현재 상태)**

```powershell
$runner = "C:\Users\nuricon\AppData\Local\Temp\claude\F--20260609-SDWall-BrowserTest-20260606-multigpu-browser\1265d766-4357-4d98-a011-7f1393985e51\scratchpad\run_perf_measure.ps1"
Remove-Item Env:\WR_DISABLE_PICTURE_CACHING -ErrorAction SilentlyContinue
pwsh -NoProfile -File $runner -Label pc_baseline -DurationSec 12 -Layout "etc\multigpu\config\wall_layout.single_3840x3240.json" -Page "tests\html\perf_video_grid_4x4.html" 2>&1 | Select-String "Render perf" | Select-Object -Last 3
```
Expected(문제 존재 확인): `avg_update_ms`가 수백 ms(예: 400~670), `composite_fps` ~1-2.

---

### Task 2: 로컬 webrender 포크 설정 (패치 없이 빌드 무변화 확인)

**Files:**
- Create: `F:\...\webrender\webrender\` (레지스트리 복사본)
- Modify: `servo/Cargo.toml` (`[patch.crates-io]` webrender 슬롯 활성화, 라인 441 부근)

**Interfaces:**
- Produces: `[patch.crates-io] webrender = { path = "../webrender/webrender" }` 활성 상태에서 빌드 성공(코드 변경 0이므로 동작 동일).

- [ ] **Step 1: 레지스트리 webrender 소스를 로컬로 복사**

```powershell
$src = "C:\Users\nuricon\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\webrender-0.68.0"
$dst = "F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\webrender\webrender"
New-Item -ItemType Directory -Force -Path (Split-Path $dst) | Out-Null
Copy-Item -Recurse -Force $src $dst
Test-Path (Join-Path $dst "src\tile_cache.rs")
```
Expected: `True`.

- [ ] **Step 2: `.cargo` 읽기전용 속성 제거(복사본 편집 가능하게)**

```powershell
Get-ChildItem -Recurse "F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\webrender\webrender" -File | ForEach-Object { $_.IsReadOnly = $false }
```
Expected: 오류 없음.

- [ ] **Step 3: servo/Cargo.toml에서 webrender path 패치 활성화**

`servo/Cargo.toml`의 `[patch.crates-io]` 섹션에서 아래 주석 한 줄만 해제(라인 441 부근). `webrender_api`/`wr_malloc_size_of`는 **주석 유지**(포크 안 함):
```toml
webrender = { path = "../webrender/webrender" }
```

- [ ] **Step 4: 패치 적용 상태로 빌드 (동작 무변화 확인)**

```powershell
. ..\scripts\servo_env.ps1
cargo build --release -p servo --example winit_wall --features media-gstreamer,no-wgl -j 8
```
Expected: `Finished ... release`. (webrender가 path 소스로 재컴파일됨. 코드 변경이 없으므로 런타임 동작은 이전과 동일.)

- [ ] **Step 5: Cargo.toml 변경 커밋**

```powershell
git add Cargo.toml Cargo.lock
git commit -m "build: activate local webrender path patch slot (no code change yet)"
```

---

### Task 3: tile_cache.rs env 게이트 + None-composite 패치

**Files:**
- Modify: `../webrender/webrender/src/tile_cache.rs` (헬퍼 추가 + `create_tile_cache` 게이트, 636-660 부근)
- Create: `servo/etc/multigpu/patches/webrender-0.68-disable-picture-caching.patch` (재현용 diff)

**Interfaces:**
- Consumes: env `WR_DISABLE_PICTURE_CACHING`.
- Produces: env 설정 시 루트 slice가 `None` composite picture로 방출됨(tile cache 미생성).

- [ ] **Step 1: 헬퍼 함수 추가**

`tile_cache.rs` 상단 import 근처(예: `use std::mem;` 다음 줄, 17번 라인 부근)에 추가:
```rust
/// Multi-GPU video-wall workaround: disable WebRender picture caching when
/// `WR_DISABLE_PICTURE_CACHING` is set in the environment. Read once (scene
/// build time), so toggling requires a restart.
fn picture_caching_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var("WR_DISABLE_PICTURE_CACHING").is_ok())
}
```

- [ ] **Step 2: `create_tile_cache`의 게이트 지점 수정 (636-660 부근)**

아래 원본 블록:
```rust
    let slice_id = SliceId::new(slice);

    // Store some information about the picture cache slice. This is used when we swap the
    // new scene into the frame builder to either reuse existing slices, or create new ones.
    tile_caches.insert(slice_id, TileCacheParams {
        debug_flags,
        slice,
        slice_flags,
        spatial_node_index: scroll_root,
        visibility_node_index: visibility_node,
        background_color,
        shared_clip_node_id,
        shared_clip_leaf_id,
        virtual_surface_size: frame_builder_config.compositor_kind.get_virtual_surface_size(),
        image_surface_count: prim_list.image_surface_count,
        yuv_image_surface_count: prim_list.yuv_image_surface_count,
    });

    let pic_index = prim_store.pictures.alloc().init(PicturePrimitive::new_image(
        Some(PictureCompositeMode::TileCache { slice_id }),
        Picture3DContext::Out,
        PrimitiveFlags::IS_BACKFACE_VISIBLE,
        prim_list,
        scroll_root,
        RasterSpace::Screen,
        PictureFlags::empty(),
        None,
    ));

    tile_cache_pictures.push(PictureIndex(pic_index));
```

를 아래로 교체:
```rust
    let slice_id = SliceId::new(slice);

    // Multi-GPU video-wall workaround: when picture caching is disabled, emit
    // this slice as a normal (None composite) picture instead of a tile cache,
    // so all-dynamic content (every primitive changing every frame) skips the
    // per-frame tile dirty/dependency tracking, which provides no benefit here.
    let composite_mode = if picture_caching_disabled() {
        None
    } else {
        // Store some information about the picture cache slice. This is used when we swap the
        // new scene into the frame builder to either reuse existing slices, or create new ones.
        tile_caches.insert(slice_id, TileCacheParams {
            debug_flags,
            slice,
            slice_flags,
            spatial_node_index: scroll_root,
            visibility_node_index: visibility_node,
            background_color,
            shared_clip_node_id,
            shared_clip_leaf_id,
            virtual_surface_size: frame_builder_config.compositor_kind.get_virtual_surface_size(),
            image_surface_count: prim_list.image_surface_count,
            yuv_image_surface_count: prim_list.yuv_image_surface_count,
        });
        Some(PictureCompositeMode::TileCache { slice_id })
    };

    let pic_index = prim_store.pictures.alloc().init(PicturePrimitive::new_image(
        composite_mode,
        Picture3DContext::Out,
        PrimitiveFlags::IS_BACKFACE_VISIBLE,
        prim_list,
        scroll_root,
        RasterSpace::Screen,
        PictureFlags::empty(),
        None,
    ));

    tile_cache_pictures.push(PictureIndex(pic_index));
```

- [ ] **Step 3: 포크 빌드 (컴파일 확인)**

```powershell
. ..\scripts\servo_env.ps1
cargo build --release -p servo --example winit_wall --features media-gstreamer,no-wgl -j 8
```
Expected: `Finished ... release`. 컴파일 에러(특히 unused var / 타입) 없어야 함. (게이트가 켜지지 않은 상태에선 `else` 분기가 원본과 동일하므로 동작 동일.)

- [ ] **Step 4: 재현용 patch 파일 저장 + 커밋**

```powershell
$fork = "F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser\webrender"
git -C $fork init -q 2>$null   # (포크가 git이 아니면 diff용으로만; 실패해도 무방)
# 원본 대비 diff 생성(수동): 아래는 servo 리포에 patch 텍스트를 보관하는 것이 목적
New-Item -ItemType Directory -Force -Path "etc\multigpu\patches" | Out-Null
```
`etc/multigpu/patches/webrender-0.68-disable-picture-caching.patch`에 Step 1·2의 diff(unified) 저장 후:
```powershell
git add etc/multigpu/patches/webrender-0.68-disable-picture-caching.patch
git commit -m "patches: webrender 0.68 env-gated picture-caching disable (WR_DISABLE_PICTURE_CACHING)"
```
Expected: 커밋 생성. (이유: 포크 트리는 리포 밖 로컬이므로, 변경을 리포에 재현 가능하게 남김.)

---

### Task 4: 회복(env on) + 무회귀(env off) 검증

**Files:**
- (없음 — 측정만)

**Interfaces:**
- Consumes: Task 3의 포크 빌드, Task 1의 베이스라인.

- [ ] **Step 1: env ON 측정 (picture caching 비활성)**

```powershell
$runner = "C:\Users\nuricon\AppData\Local\Temp\claude\F--20260609-SDWall-BrowserTest-20260606-multigpu-browser\1265d766-4357-4d98-a011-7f1393985e51\scratchpad\run_perf_measure.ps1"
$env:WR_DISABLE_PICTURE_CACHING = "1"
pwsh -NoProfile -File $runner -Label pc_off -DurationSec 12 -Layout "etc\multigpu\config\wall_layout.single_3840x3240.json" -Page "tests\html\perf_video_grid_4x4.html" 2>&1 | Select-String "Render perf|Present perf" | Select-Object -Last 4
```
Expected(성공 기준): `avg_update_ms`가 수십 ms 수준으로 급감, `composite_fps` ≥ 24(이상적 ~30+). (Task 1의 수백 ms/1-2fps 대비 극적 개선.)

- [ ] **Step 2: 화면 렌더 정상성 육안 확인**

Step 1 실행 중 창에 16개 영상이 정상 재생·배치되는지 눈으로 확인(누락/검은칸/깨짐 없음).
Expected: 4×4 그리드 정상 표시. (문제 시 → 설계 spec 6절 폴백으로.)

- [ ] **Step 3: env OFF 재측정 (무회귀 확인)**

```powershell
Remove-Item Env:\WR_DISABLE_PICTURE_CACHING -ErrorAction SilentlyContinue
pwsh -NoProfile -File $runner -Label pc_on_recheck -DurationSec 12 -Layout "etc\multigpu\config\wall_layout.single_3840x3240.json" -Page "tests\html\perf_video_grid_4x4.html" 2>&1 | Select-String "Render perf" | Select-Object -Last 3
```
Expected: Task 1 베이스라인과 유사(수백 ms/1-2fps) — env off일 때 기존 동작 보존.

---

### Task 5: 정적/비디오-아님 페이지 무회귀 스모크 (env off 기본값)

**Files:**
- (없음 — 측정만)

- [ ] **Step 1: env off로 비-비디오 프로브 페이지 실행**

```powershell
Remove-Item Env:\WR_DISABLE_PICTURE_CACHING -ErrorAction SilentlyContinue
$runner = "C:\Users\nuricon\AppData\Local\Temp\claude\F--20260609-SDWall-BrowserTest-20260606-multigpu-browser\1265d766-4357-4d98-a011-7f1393985e51\scratchpad\run_perf_measure.ps1"
pwsh -NoProfile -File $runner -Label static_smoke -DurationSec 8 -Layout "etc\multigpu\config\wall_layout.example_1x1.json" -Page "tests\html\multigpu_wall_stress_cases.html" 2>&1 | Select-String "Render perf" | Select-Object -Last 2
```
Expected: 정상 실행, 패닉 없음. (env off라 picture caching 정상 동작 = 기존과 동일.)

---

### Task 6: 마무리 — 문서화 + 브랜치 상태 정리

**Files:**
- Modify: `servo/video_grid_perf_summary.txt` (env 노브 항목에 `WR_DISABLE_PICTURE_CACHING` + 결과 추가) 또는 메모리 갱신
- Modify: `servo/etc/multigpu/patches/README`(있으면) 또는 patch 파일 헤더 주석

- [ ] **Step 1: 결과 수치 기록**

Task 4의 pc_baseline vs pc_off 수치(avg_update_ms, composite_fps)를 `video_grid_perf_summary.txt`의 노브 섹션에 한 줄 추가.

- [ ] **Step 2: 사용법 문서화**

`WR_DISABLE_PICTURE_CACHING=1` + `[patch.crates-io]` 활성 + `../webrender/webrender` 포크 필요를 patch 파일 상단 주석 또는 짧은 README에 명시.

- [ ] **Step 3: 커밋**

```powershell
git add -A
git commit -m "docs: record WR_DISABLE_PICTURE_CACHING workaround result and usage"
```

- [ ] **Step 4: 최종 상태 확인**

```powershell
git status --short
git log --oneline -5
```
Expected: 작업 트리 clean, 커밋 이력에 Task 2/3/6 커밋 존재.

---

## 리스크 메모 (실행자용)

- **핵심 리스크:** `None`-composite 루트 picture가 렌더 안 되거나 깨지면(Task 4 Step 2 실패), 설계 spec 6절 폴백:
  TileCache 구조는 유지하되 `picture.rs`의 per-frame 타일 업데이트 진입부에서 dirty-tracking만 우회하는 대안 지점을 재탐색. (그 경우 이 계획을 중단하고 재설계.)
- **빌드 시간:** webrender path 재컴파일 포함 시 초기 빌드가 길 수 있음. 이후 tile_cache.rs만 바꾸면 증분.
- **포크 트리는 리포 밖**(`../webrender`)이므로 servo 리포에는 patch 파일로만 재현됨. 다른 머신 재현 시 Task 2를 다시 수행해야 함.

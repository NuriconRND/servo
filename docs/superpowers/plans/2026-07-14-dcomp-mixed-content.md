# DComp 복합 콘텐츠 지원 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** DComp 하이브리드(=1)에서 부분 갱신 콘텐츠가 동결 없이 표시되도록 부분 Present(1차) + 가상 서피스 강등(폴백)을 구현하고, 알파 슬라이스 비디오 검정 결함의 원인 계층을 진단한다.

**Architecture:** `components/paint/dcomp_compositor.rs`의 승격 상태머신을 확장한다 — 승격 스왑체인을 FLIP_SEQUENTIAL로 전환하고, 첫 콘텐츠 Present 이후에는 stale 영역 catch-up 복사(GetBuffer(1)→GetBuffer(0)) 후 매 그려진 프레임을 Present1한다. 부분 Present 불가 시 지속 withhold는 가상 서피스로 자동 강등한다(백버퍼 시딩 복사 + 재승격 지수 쿨다운). 결함②는 unbind 시점 readback 계측 + 최소 재현 페이지로 원인 계층만 확정한다(수정은 결정 게이트).

**Tech Stack:** Rust, winapi 0.3(dcomp/dxgi1_2/d3d11), WebRender 0.68 Compositor trait, ANGLE EGL pbuffer interop(기존), PowerShell 검증 스크립트.

**스펙:** `docs/superpowers/specs/2026-07-14-dcomp-mixed-content-design.md` (사용자 승인). 이 계획의 모든 요구는 스펙이 정본.

## Global Constraints

- 상수(스펙 §4·§5·§6 값 그대로): `PROMOTE_MIN_AGE_FRAMES=30`, `PROMOTE_STREAK=3`(기존 유지), `DEMOTE_AFTER_WITHHOLD=30`, 재승격 쿨다운 `300 × 2^(n−1)` 프레임(상한 `3600`), `MAX_PRESENT_DIRTY_RECTS=16`, stale 렉트 32개 초과 시 바운딩 유니온으로 붕괴(과대 방향만 안전), **frame_dirty(차집합의 감수)는 절대 근사·붕괴 금지**.
- 승격 스왑체인 SwapEffect = `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL`로 전환(FLIP_DISCARD는 Present 후 버퍼 미보존 — 스펙 §5.1-1).
- 순수 월(전면 더티) 프레임의 catch-up 복사 = 0바이트(차집합 공집합) — 기존 경로와 성능 동일해야 함.
- `=surface` 모드 동작 무변경, 게이트 off(Draw) 무변경. 새 env: `SERVO_DCOMP_NO_PARTIAL_PRESENT=1`(부분 Present 비활성, 진단용), `SERVO_DCOMP_READBACK=1`(결함② 계측, 프레임 120 상한). env 조회는 기존 `OnceLock` 관례 — per-frame `std::env::var` 금지.
- 서드파티(third_party/surfman, WR) 수정 금지 — 이번 사이클은 `components/paint`(+ shared/paint 예제)와 테스트 페이지만.
- 빌드: PowerShell에서 `. .\etc\multigpu\servo_env.ps1; $ErrorActionPreference='Continue'; python mach build --release` (env 소싱 후 ErrorActionPreference 재설정 필수 — cargo stderr가 빌드를 중단시킴). **`cargo check --workspace` 금지(mozjs 행)**. 유닛테스트: `cargo test -p servo-paint --lib dcomp` (같은 env 소싱 후).
- 커밋 메시지는 한국어, Claude 관련 문구/Co-Authored-By 금지. Rust 주석은 파일 기존 관례(한국어) 유지, 로그 문자열은 영어.
- 실기 검증 관례: 런처 `etc/multigpu/run_video_wall_d3d11.ps1`(-DComp/-DCompSurface/-Page/-MoveX/-Detach), 로그 검색은 PowerShell `Select-String`(bash grep은 UTF-16 못 읽음), 픽셀 판정은 CopyFromScreen(DComp 출력에 유효), 새 테스트 페이지에는 rAF 하트비트 필수(없으면 winit 이벤트 루프 기아). 실행 전 기존 servoshell 프로세스 kill.
- HALT 게이트 2곳: Task 1 G2 실패(부분 Present 폐기 — Task 4를 건너뛰고 Task 5의 강등을 유일 경로로 검증), Task 6 판정=WR급(수정 착수 없이 보고).

---

## 현재 코드 앵커 (2026-07-14 HEAD 3a8d6b9bf 기준, `components/paint/dcomp_compositor.rs`)

구현자가 길을 잃지 않도록 이 계획이 참조하는 현재 구조(줄번호는 근사 — 심볼로 찾을 것):

- `PROMOTE_STREAK`/`WITHHOLD_WARN_FRAMES` 상수: :51-53
- `FrameCoverage { covered_tiles }` + `note_tile`(dirty.contains_box(&valid)만 집계)/`is_full`: :107-124
- `SwapChainStorage { swapchain, anchor, size, coverage, frame_pbuffer, drawn_this_frame, content_attached, withheld_frames, fallback_virtual, displayed_anchor }`: :259-279
- `SurfaceEntry { storage, visual, virtual_offset, tile_size, is_opaque, tiles, frame_coverage, promote_streak, last_placement }`: :301-316
- `DCompNativeCompositor` 필드(d3d11_device, dxgi_factory, warned_* 등): :331-357
- `create_composition_swapchain`(FLIP_DISCARD, BufferCount 2): :491-533
- `bind()` SwapChain arm(프레임 첫 bind에서 GetBuffer(0)→pbuffer 캐시, `sc.coverage.note_tile`, origin = tile_rect.min − anchor): :698-760
- `bind()` Virtual arm(BeginDraw): :761-830대, Virtual `entry.frame_coverage.note_tile`: :828
- `end_frame()`: flush(:989) → per-surface 루프(Virtual 승격 판정 :1002-1034 / SwapChain regen·Present·withhold :1035-1112) → promote_requests(:1118-1162) → regen_requests(:1169-1202) → 컬링/AddVisual → `Commit()`(:1237)
- `apply_visual_placement`(content-swap 시 오프셋 재적용): :226-256
- 유닛테스트 모듈(FrameCoverage 등): 파일 말미 `#[cfg(test)] mod tests`
- PoC 예제 선례: `components/shared/paint/examples/dcomp_native_poc.rs` (창 생성/DComp 부트스트랩/게이트 PASS-FAIL 출력 관례), 실행 `cargo run --release -p servo-paint-api --example <이름> --features no-wgl`

---

### Task 1: 부분 Present PoC (HALT 게이트)

**Files:**
- Create: `components/shared/paint/examples/dcomp_partial_present_poc.rs`
- Modify: (없음 — 제품 코드 무변경)

**Interfaces:**
- Consumes: `dcomp_native_poc.rs`의 창 생성/D3D11+DComp 부트스트랩 패턴(복사해 시작).
- Produces: 콘솔 G1~G4 PASS/FAIL 판정 → Task 4 진행 여부 결정(G2 FAIL = HALT).

- [ ] **Step 1: PoC 작성**

`dcomp_native_poc.rs`를 열어 창 생성 + D3D11 디바이스(D3D11_CREATE_DEVICE_BGRA_SUPPORT) + DComp 디바이스/타깃/비주얼 부트스트랩 부분을 복사한 뒤, 다음 4게이트를 순서대로 검증하는 예제를 작성한다. 렌더는 GL 불필요 — D3D11 `ClearRenderTargetView` + `CopySubresourceRegion`만 사용(방향 문제 없음).

```rust
// 게이트 개요 (각 게이트는 println!("G{n} {PASS|FAIL} ...") 출력, G2 FAIL 시 즉시 종료 코드 2)
// G1: CreateSwapChainForComposition, 640x480, BGRA8, BufferCount=2,
//     SwapEffect=DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, Scaling=DXGI_SCALING_STRETCH,
//     AlphaMode=DXGI_ALPHA_MODE_IGNORE → hr==S_OK && 포인터 non-null.
//     visual.SetContent(swapchain) + Commit.
// G2 (핵심 미지수): 프레임 A(빨강 전체 클리어) Present 후,
//     GetBuffer(1)이 성공하고 그 텍스처를 CopySubresourceRegion의 **소스**로 사용해
//     GetBuffer(0)에 복사가 성공하는가. 판정: 복사 후 GetBuffer(0)을
//     D3D11_USAGE_STAGING 텍스처로 CopyResource → Map → 픽셀이 빨강(0xFF0000FF BGRA
//     주의: B8G8R8A8 메모리 바이트 순 B,G,R,A)인지 확인. hr 실패 또는 픽셀 불일치 = FAIL.
// G3: 부분 Present — 프레임 B: 좌반 파랑으로 draw(좌반만 RTV 클리어는 불가하므로
//     스테이징→서브리소스 복사 또는 ClearView 대신 '좌반 크기 보조 텍스처를 만들어
//     CopySubresourceRegion으로 좌반에 복사'), IDXGISwapChain1::Present1(0, 0,
//     &DXGI_PRESENT_PARAMETERS { DirtyRectsCount:1, pDirtyRects:&RECT{0,0,320,480},
//     pScrollRect:null, pScrollOffset:null }) → hr==S_OK.
//     화면 판정: 창을 (100,100)에 두고 CopyFromScreen으로 좌반=파랑, 우반=빨강 확인
//     (우반은 G2에서 catch-up 복사로 채워 둔 빨강이 유지되어야 함).
// G4: 로테이션 의미론 — 프레임 C(초록 전체) Present 후 GetBuffer(0) 스테이징 readback:
//     내용이 '프레임 B 시점의 버퍼'(즉 파랑+빨강 조합)인지 확인 = BufferCount 2 교대 확인.
//     (여기 결과가 예상과 다르면 stale 부기 설계의 '반대 버퍼' 가정을 로그로 기록만 하고
//     G4 FAIL로 표시 — Task 4에서 부기 방향을 이 실측에 맞춘다.)
```

구현 세부: 스테이징 readback 헬퍼 `fn read_pixel(ctx, tex, x, y) -> [u8;4]`(D3D11_TEXTURE2D_DESC Usage=STAGING, CPUAccessFlags=READ, CopyResource, Map(0, D3D11_MAP_READ)) 하나 만들어 G2~G4 공용. 창은 `-MoveX` 개념 없이 주모니터 (100,100) 고정. 각 프레임 사이 `std::thread::sleep(200ms)` + Commit.

- [ ] **Step 2: 실행·판정**

```powershell
. .\etc\multigpu\servo_env.ps1; $ErrorActionPreference='Continue'
cargo run --release -p servo-paint-api --example dcomp_partial_present_poc --features no-wgl
```
Expected: `G1 PASS`, `G2 PASS`, `G3 PASS`, `G4 PASS`(또는 G4의 실측 로테이션 기록). **G2 FAIL이면 HALT — 컨트롤러에 보고**(부분 Present 폐기, Task 4 스킵, Task 5가 유일 경로).

- [ ] **Step 3: 커밋**

```powershell
git add components/shared/paint/examples/dcomp_partial_present_poc.rs
git commit -m "dcomp: 부분 Present PoC - FLIP_SEQUENTIAL GetBuffer(1) 읽기/Present1/로테이션 검증"
```

---

### Task 2: 렉트 영역 차집합 + stale 부기 (순수 로직, TDD)

**Files:**
- Modify: `components/paint/dcomp_compositor.rs` (새 헬퍼 + `#[cfg(test)] mod tests`에 테스트 추가)

**Interfaces:**
- Produces: `fn subtract_rect(minuend: DeviceIntRect, sub: DeviceIntRect) -> heapless 없는 Vec<DeviceIntRect>` 스타일의
  `fn region_subtract(minuend: &[DeviceIntRect], subtrahend: &[DeviceIntRect]) -> Vec<DeviceIntRect>`,
  `struct StaleTracker { stale: [Vec<DeviceIntRect>; 2], cur: usize }` +
  `fn on_present(&mut self, frame_dirty: &[DeviceIntRect], full: DeviceIntRect)` /
  `fn catchup_rects(&self, frame_dirty: &[DeviceIntRect]) -> Vec<DeviceIntRect>` /
  `fn reset(&mut self)`. Task 4가 그대로 소비.

- [ ] **Step 1: 실패하는 테스트 작성** (`mod tests`에 추가)

```rust
fn r(x0: i32, y0: i32, x1: i32, y1: i32) -> DeviceIntRect {
    DeviceIntRect::new(DeviceIntPoint::new(x0, y0), DeviceIntPoint::new(x1, y1))
}

#[test]
fn region_subtract_cases() {
    // 비겹침: 그대로
    assert_eq!(region_subtract(&[r(0,0,10,10)], &[r(20,20,30,30)]), vec![r(0,0,10,10)]);
    // 완전 포함: 공집합
    assert!(region_subtract(&[r(0,0,10,10)], &[r(0,0,10,10)]).is_empty());
    assert!(region_subtract(&[r(2,2,8,8)], &[r(0,0,10,10)]).is_empty());
    // 부분 겹침(우하단 조각): 면적 보존 검증 — 결과 면적 = 10*10 - 5*5
    let out = region_subtract(&[r(0,0,10,10)], &[r(5,5,15,15)]);
    let area: i32 = out.iter().map(|q| (q.max.x-q.min.x)*(q.max.y-q.min.y)).sum();
    assert_eq!(area, 100 - 25);
    // 결과 조각이 서로 겹치지 않고 subtrahend와도 겹치지 않는다
    for (i, a) in out.iter().enumerate() {
        assert!(a.intersection(&r(5,5,15,15)).is_none());
        for b in out.iter().skip(i + 1) {
            assert!(a.intersection(b).is_none());
        }
    }
    // 여러 감수 렉트 순차 차감
    let out = region_subtract(&[r(0,0,100,10)], &[r(10,0,20,10), r(30,0,40,10)]);
    let area: i32 = out.iter().map(|q| (q.max.x-q.min.x)*(q.max.y-q.min.y)).sum();
    assert_eq!(area, 1000 - 100 - 100);
}

#[test]
fn stale_tracker_bookkeeping() {
    let full = r(0, 0, 100, 100);
    let mut st = StaleTracker::default();
    // 첫 전면 Present: 반대 버퍼가 전면 stale
    st.on_present(&[full], full);
    // 이번 프레임 좌반만 갱신 → catch-up = 전면 − 좌반 = 우반
    let catchup = st.catchup_rects(&[r(0,0,50,100)]);
    let area: i32 = catchup.iter().map(|q| (q.max.x-q.min.x)*(q.max.y-q.min.y)).sum();
    assert_eq!(area, 100*100 - 50*100);
    // 그 부분 프레임 Present 후: 반대 버퍼(방금 전면이었던 쪽)의 stale = 좌반
    st.on_present(&[r(0,0,50,100)], full);
    let catchup = st.catchup_rects(&[]);
    let area: i32 = catchup.iter().map(|q| (q.max.x-q.min.x)*(q.max.y-q.min.y)).sum();
    assert_eq!(area, 50*100);
    // 전면 더티 프레임의 catch-up은 공집합 (Global Constraints: 순수 월 0바이트)
    assert!(st.catchup_rects(&[full]).is_empty());
    // stale 32개 초과 → 바운딩 유니온 붕괴 (과대 안전)
    let mut st = StaleTracker::default();
    for i in 0..40 {
        st.on_present(&[r(i*2, 0, i*2+1, 1)], full);
    }
    assert!(st.stale[st.cur].len() <= 32 + 1);
}
```

- [ ] **Step 2: 실패 확인**

```powershell
. .\etc\multigpu\servo_env.ps1; $ErrorActionPreference='Continue'
cargo test -p servo-paint --lib dcomp
```
Expected: FAIL — `region_subtract`/`StaleTracker` 미정의 컴파일 에러.

- [ ] **Step 3: 구현** (상수·FrameCoverage 근처, `tile_virtual_rect` 아래에 배치)

```rust
/// 부분 Present catch-up용 상수(스펙 §5.2). 힌트 렉트 상한 / stale 목록 붕괴 상한.
const MAX_PRESENT_DIRTY_RECTS: usize = 16;
const MAX_STALE_RECTS: usize = 32;

/// minuend − sub: 겹치면 최대 4조각(상/하/좌/우 밴드)으로 분해. 정확 연산(근사 금지 —
/// 차집합이 넓으면 stale 픽셀 잔존, 좁으면 신규 콘텐츠를 구본으로 덮어씀. 스펙 §5.2-2).
fn subtract_rect(minuend: DeviceIntRect, sub: DeviceIntRect) -> Vec<DeviceIntRect> {
    let Some(ix) = minuend.intersection(&sub) else {
        return vec![minuend];
    };
    let mut out = Vec::with_capacity(4);
    // 상단 밴드
    if minuend.min.y < ix.min.y {
        out.push(DeviceIntRect::new(
            DeviceIntPoint::new(minuend.min.x, minuend.min.y),
            DeviceIntPoint::new(minuend.max.x, ix.min.y),
        ));
    }
    // 하단 밴드
    if ix.max.y < minuend.max.y {
        out.push(DeviceIntRect::new(
            DeviceIntPoint::new(minuend.min.x, ix.max.y),
            DeviceIntPoint::new(minuend.max.x, minuend.max.y),
        ));
    }
    // 좌측 밴드(교차 세로 구간만)
    if minuend.min.x < ix.min.x {
        out.push(DeviceIntRect::new(
            DeviceIntPoint::new(minuend.min.x, ix.min.y),
            DeviceIntPoint::new(ix.min.x, ix.max.y),
        ));
    }
    // 우측 밴드(교차 세로 구간만)
    if ix.max.x < minuend.max.x {
        out.push(DeviceIntRect::new(
            DeviceIntPoint::new(ix.max.x, ix.min.y),
            DeviceIntPoint::new(minuend.max.x, ix.max.y),
        ));
    }
    out
}

/// 감수 목록 전체를 순차 차감. 빈/역전 렉트는 자연 소거(밴드 조건이 걸러냄).
fn region_subtract(
    minuend: &[DeviceIntRect],
    subtrahend: &[DeviceIntRect],
) -> Vec<DeviceIntRect> {
    let mut acc: Vec<DeviceIntRect> = minuend.to_vec();
    for sub in subtrahend {
        let mut next = Vec::with_capacity(acc.len());
        for m in acc {
            next.extend(subtract_rect(m, *sub));
        }
        acc = next;
    }
    acc
}

/// 버퍼 2개(FLIP_SEQUENTIAL) 기준 stale 영역 부기(버퍼-로컬 좌표, 스펙 §5.2).
/// stale[i] = 버퍼 i가 놓친 갱신 영역. Present(D) 시 현재 버퍼는 완성(∅), 반대
/// 버퍼에 D 누적, 쓰기 대상 교대. 과대(바운딩 붕괴)는 안전 — 이미 최신인 영역을
/// 한 번 더 복사할 뿐. Task 1 G4 실측 로테이션이 다르면 이 모델을 그에 맞춘다.
#[derive(Default)]
struct StaleTracker {
    stale: [Vec<DeviceIntRect>; 2],
    cur: usize,
}

impl StaleTracker {
    /// 이번 프레임(더티 frame_dirty)을 Present한 직후 호출.
    fn on_present(&mut self, frame_dirty: &[DeviceIntRect], full: DeviceIntRect) {
        self.stale[self.cur].clear();
        let other = 1 - self.cur;
        self.stale[other].extend_from_slice(frame_dirty);
        if self.stale[other].len() > MAX_STALE_RECTS {
            let union = self.stale[other]
                .iter()
                .fold(None::<DeviceIntRect>, |acc, r| {
                    Some(acc.map_or(*r, |a| a.union(r)))
                })
                .unwrap_or(full);
            self.stale[other] = vec![union];
        }
        self.cur = other;
    }

    /// Present 직전 catch-up 복사 대상(= 현재 버퍼 stale − 이번 프레임 더티).
    fn catchup_rects(&self, frame_dirty: &[DeviceIntRect]) -> Vec<DeviceIntRect> {
        region_subtract(&self.stale[self.cur], frame_dirty)
    }

    fn reset(&mut self) {
        self.stale[0].clear();
        self.stale[1].clear();
        self.cur = 0;
    }
}
```

- [ ] **Step 4: 통과 확인**

```powershell
cargo test -p servo-paint --lib dcomp
```
Expected: PASS (기존 FrameCoverage 테스트 포함 전부).

- [ ] **Step 5: 커밋**

```powershell
git add components/paint/dcomp_compositor.rs
git commit -m "dcomp: 렉트 영역 차집합 + 버퍼별 stale 부기 (부분 Present 기반, 유닛테스트)"
```

---

### Task 3: 승격 위생 — 최소 나이 + 재승격 쿨다운 골격

**Files:**
- Modify: `components/paint/dcomp_compositor.rs`

**Interfaces:**
- Consumes: 기존 `SurfaceEntry.promote_streak`, end_frame Virtual arm(:1002-1034).
- Produces: `SurfaceEntry`에 `drawn_frames: u32`, `demote_count: u32`, `promote_blocked_until: u64`; `DCompNativeCompositor.frame_counter: u64`; 상수 `PROMOTE_MIN_AGE_FRAMES`, `DEMOTE_AFTER_WITHHOLD`, `DEMOTE_COOLDOWN_BASE`, `DEMOTE_COOLDOWN_CAP`. Task 4·5가 소비.

- [ ] **Step 1: 상수·필드 추가**

상수(:51 근처):
```rust
/// 스펙 §4: 그려진 지 이만큼의 프레임이 지나야 승격 streak을 인정(시작 과도기 배제).
const PROMOTE_MIN_AGE_FRAMES: u32 = 30;
/// 스펙 §6.1: withhold가 이만큼 연속되면 가상 서피스로 강등.
const DEMOTE_AFTER_WITHHOLD: u32 = 30;
/// 스펙 §6.3: 강등 n회째 재승격 쿨다운 = BASE × 2^(n−1), 상한 CAP (프레임).
const DEMOTE_COOLDOWN_BASE: u64 = 300;
const DEMOTE_COOLDOWN_CAP: u64 = 3600;
```

`SurfaceEntry`(:301)에 필드 3개(주석 포함), `create_surface`의 초기화(:639-649)에 0 추가:
```rust
    /// 이 서피스가 그려진(bind된) 프레임의 누적 수 — PROMOTE_MIN_AGE_FRAMES 게이트.
    drawn_frames: u32,
    /// 강등 누적 횟수(쿨다운 지수의 n).
    demote_count: u32,
    /// 이 프레임 번호 전까지 승격 금지(재승격 쿨다운). 0 = 제한 없음.
    promote_blocked_until: u64,
```

`DCompNativeCompositor`(:331)에 `frame_counter: u64` 추가(maybe_create에서 0), `begin_frame`(:901)에서 `self.frame_counter += 1;`.

- [ ] **Step 2: 승격 조건 확장**

end_frame Virtual arm(:1006-1033)을 다음으로 교체(변경점: drawn 판정 → drawn_frames 증가, MIN_AGE·쿨다운 게이트):

```rust
                    let frame_drawn = !entry.frame_coverage.covered_tiles.is_empty()
                        || entry.frame_drawn_partial;
                    let frame_full = entry.frame_coverage.is_full(&entry.tiles);
                    entry.frame_coverage.reset();
                    entry.frame_drawn_partial = false;
                    if frame_drawn {
                        entry.drawn_frames = entry.drawn_frames.saturating_add(1);
                    }
                    // 승격 상태머신(스펙 §4): streak은 MIN_AGE 경과 후부터만 누적.
                    entry.promote_streak = if frame_full
                        && entry.drawn_frames > PROMOTE_MIN_AGE_FRAMES
                    {
                        entry.promote_streak + 1
                    } else {
                        0
                    };
                    if mode == StorageMode::Hybrid
                        && entry.is_opaque
                        && !self.warned_promote_fail
                        && entry.promote_streak >= PROMOTE_STREAK
                        && self.frame_counter >= entry.promote_blocked_until
                        && self.dxgi_factory.is_some()
                    {
                        // (이하 기존 surface_extent/tiles_are_dense/promote_requests 그대로)
```

`frame_drawn_partial`: Virtual bind에서 note_tile이 부분 더티라 집계 안 되는 프레임도 "그려짐"으로 세기 위한 bool 필드 — `SurfaceEntry`에 추가하고 Virtual bind arm(:828 근처)에서 `entry.frame_drawn_partial = true;`를 note_tile 호출 직후에 넣는다(주석: 부분 더티 프레임도 나이에 포함).

기존 is_opaque 승격 주석 블록(:1010-1017)은 유지하되 MIN_AGE 한 줄을 덧붙인다.

- [ ] **Step 3: 빌드 + 유닛테스트**

```powershell
. .\etc\multigpu\servo_env.ps1; $ErrorActionPreference='Continue'
cargo test -p servo-paint --lib dcomp
```
Expected: PASS(컴파일 포함). (승격 지연의 실기 확인은 Task 4/5 검증에 포함 — promote 로그 프레임 번호가 33 이상.)

- [ ] **Step 4: 커밋**

```powershell
git add components/paint/dcomp_compositor.rs
git commit -m "dcomp: 승격 위생 - 최소 나이 30프레임 + 재승격 쿨다운 골격"
```

---

### Task 4: 부분 Present 본구현 (PoC G2 PASS 전제)

**Files:**
- Modify: `components/paint/dcomp_compositor.rs`

**Interfaces:**
- Consumes: Task 2 `StaleTracker`/`region_subtract`/상수, Task 3 필드, PoC G4 실측 로테이션.
- Produces: SwapChainStorage에 `frame_dirty: Vec<DeviceIntRect>`, `stale: StaleTracker`, `partial_present: bool`; `DCompNativeCompositor.d3d11_context: Option<ComOwned<ID3D11DeviceContext>>`; env 게이트 `partial_present_disabled()`. Task 5·7이 소비.

- [ ] **Step 1: 스왑 이펙트 전환 + 컨텍스트 확보**

- `create_composition_swapchain`(:513): `SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD` → `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL` (import도 교체: :29의 `DXGI_SWAP_EFFECT_FLIP_DISCARD` → `winapi::shared::dxgi::DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL`). 주석: FLIP_DISCARD는 Present 후 버퍼 미보존 — catch-up 복사 전제 위반(스펙 §5.1-1). regen 주석(:1049)의 FLIP_DISCARD 문구도 SEQUENTIAL로 정정.
- `maybe_create`에서 d3d 확보 직후 `GetImmediateContext`로 `d3d11_context` 확보(ComOwned, AddRef됨), 필드 추가. import `winapi::um::d3d11::ID3D11DeviceContext`.
- env 게이트(OnceLock 관례, `cull_disabled` :98 아래):

```rust
/// 진단: 부분 Present만 끄는 스위치(스펙 §3) — 강등 폴백 경로 검증용.
fn partial_present_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var("SERVO_DCOMP_NO_PARTIAL_PRESENT").is_ok())
}
```

- [ ] **Step 2: bind에서 프레임 더티 렉트 수집**

SwapChain arm의 `sc.coverage.note_tile(...)`(:741) 직후에 버퍼-로컬 더티 렉트 누적(주석: dirty_rect는 타일-로컬 → 타일 가상 rect + 더티 − anchor = 버퍼-로컬):

```rust
                let dirty_buf = DeviceIntRect::new(
                    DeviceIntPoint::new(
                        tile_rect.min.x + dirty_rect.min.x - sc.anchor.x,
                        tile_rect.min.y + dirty_rect.min.y - sc.anchor.y,
                    ),
                    DeviceIntPoint::new(
                        tile_rect.min.x + dirty_rect.max.x - sc.anchor.x,
                        tile_rect.min.y + dirty_rect.max.y - sc.anchor.y,
                    ),
                );
                sc.frame_dirty.push(dirty_buf);
```

`SwapChainStorage`에 필드 추가(승격·regen 초기화 지점 :1137-1148/:1185-1193에도):
```rust
    /// 이번 프레임 bind가 쌓은 버퍼-로컬 더티 렉트(Present1 힌트 + stale 부기 재료).
    frame_dirty: Vec<DeviceIntRect>,
    /// 버퍼별 stale 부기(스펙 §5.2). content_attached 후 부분 Present에 사용.
    stale: StaleTracker,
    /// 이 스왑체인에서 부분 Present 사용 가능(GetBuffer(1) 프로브 성공 + env 미차단).
    partial_present: bool,
```
승격 시 초기값: `frame_dirty: Vec::new(), stale: StaleTracker::default(), partial_present: false`. regen 시 `frame_dirty.clear(); stale.reset(); partial_present = false;`(재프로브).

- [ ] **Step 3: end_frame — 부분 Present 경로**

SwapChain arm(:1056-1108)의 Present/withhold 분기를 다음 구조로 교체한다. 기존 "full Present + content-swap" 블록(:1056-1093)은 **그대로 유지**하되 Present 성공 시 부기 추가, "부분 갱신" 분기(:1094-1108)를 부분 Present/withhold로 나눈다:

```rust
                    } else if sc.drawn_this_frame && sc.coverage.is_full(&entry.tiles) {
                        // ... (기존 Present(0,0) + content-swap 블록 그대로) ...
                        // Present 성공 분기(sc.coverage.reset(); sc.withheld_frames = 0;) 직후에 추가:
                        //   let full = DeviceIntRect::new(DeviceIntPoint::zero(),
                        //       DeviceIntPoint::new(sc.size.width, sc.size.height));
                        //   let dirty = std::mem::take(&mut sc.frame_dirty);
                        //   sc.stale.on_present(&dirty, full);
                        //   그리고 content-swap 완료(sc.content_attached=true) 직후 1회:
                        //   sc.partial_present = !partial_present_disabled()
                        //       && probe_partial_present(&sc.swapchain);
                    } else if sc.drawn_this_frame && sc.content_attached && sc.partial_present {
                        // 부분 Present(스펙 §5.2): catch-up 복사(정확 차집합) 후 매 프레임 Present1.
                        let full = DeviceIntRect::new(
                            DeviceIntPoint::zero(),
                            DeviceIntPoint::new(sc.size.width, sc.size.height),
                        );
                        let dirty = std::mem::take(&mut sc.frame_dirty);
                        let catchup = sc.stale.catchup_rects(&dirty);
                        let ok = self_copy_catchup(&self.d3d11_context, sc, &catchup)
                            && present1_partial(sc, &dirty);
                        if ok {
                            sc.coverage.reset();
                            sc.withheld_frames = 0;
                            sc.stale.on_present(&dirty, full);
                        } else {
                            // 스펙 §5.3: 런타임 실패 → 즉시 강등(warn-once는 강등부에서).
                            sc.partial_present = false;
                            demote_requests.push(*id);
                        }
                    } else if sc.drawn_this_frame {
                        // 부분 갱신 + 부분 Present 불가 → 종전 withhold(강등 카운터, Task 5).
                        sc.frame_dirty.clear();
                        sc.withheld_frames += 1;
                        // ... (기존 WITHHOLD_WARN_FRAMES warn + dcomp_debug 로그 유지) ...
                    }
```

주의: 전면 Present 분기에서도 `frame_dirty`는 반드시 `take`(비우기)한다 — 다음 프레임 부기 오염 방지. withhold 분기에서도 clear하되 **coverage는 유지**(기존 누적 의미론 — 스펙 §6.2-1 시딩 근거). 컴파일 사정상 `demote_requests: Vec<NativeSurfaceId>`를 promote/regen_requests 옆(:997)에 선언하고 처리부는 Task 5가 붙인다(이 태스크에서는 선언 + push만 — 처리 루프는 `for _ in demote_requests {}` 자리표시가 아니라 **Task 5 전까지 빈 Vec 소비 없이 두면 unused 경고**가 나므로, 이 태스크에서 warn 로그만 남기는 최소 처리(`for id in demote_requests { warn!(...) }`)를 넣고 Task 5가 본 처리로 교체).

헬퍼 3개(impl DCompNativeCompositor 밖 자유 함수 또는 impl 내 — borrow 사정상 자유 함수 권장, `sc`와 `Option<ComOwned<ID3D11DeviceContext>>`만 받게):

```rust
/// content-swap 시 1회: GetBuffer(1)이 이 환경에서 열리는지 프로브(스펙 §3 '런타임 자격').
fn probe_partial_present(swapchain: &ComOwned<IDXGISwapChain1>) -> bool {
    // Safety: 살아있는 스왑체인. 성공 시 AddRef된 텍스처 즉시 Release.
    unsafe {
        let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
        let hr = (*swapchain.as_ptr()).GetBuffer(
            1,
            &ID3D11Texture2D::uuidof(),
            &mut tex as *mut _ as *mut _,
        );
        if hr < 0 || tex.is_null() {
            warn!("[dcomp-native] GetBuffer(1) probe failed (hr=0x{:08x}); partial present off", hr as u32);
            return false;
        }
        (*(tex as *mut IUnknown)).Release();
        true
    }
}

/// catch-up 복사: GetBuffer(1)→frame_pbuffer.texture(=GetBuffer(0)) 렉트들.
/// 전면 더티 프레임이면 rects가 공집합 → 복사 0(Global Constraints).
fn self_copy_catchup(
    ctx: &Option<ComOwned<ID3D11DeviceContext>>,
    sc: &SwapChainStorage,
    rects: &[DeviceIntRect],
) -> bool {
    if rects.is_empty() {
        return true;
    }
    let Some(ctx) = ctx.as_ref() else { return false; };
    let Some(fp) = sc.frame_pbuffer.as_ref() else { return false; };
    // Safety: 살아있는 스왑체인/컨텍스트. src는 AddRef → 사용 후 Release.
    unsafe {
        let mut src: *mut ID3D11Texture2D = ptr::null_mut();
        let hr = (*sc.swapchain.as_ptr()).GetBuffer(
            1,
            &ID3D11Texture2D::uuidof(),
            &mut src as *mut _ as *mut _,
        );
        if hr < 0 || src.is_null() {
            warn!("[dcomp-native] GetBuffer(1) failed at copy (hr=0x{:08x})", hr as u32);
            return false;
        }
        for rc in rects {
            // 버퍼 경계로 클램프(stale 바운딩 붕괴가 경계 밖을 물 수 있음).
            let x0 = rc.min.x.max(0);
            let y0 = rc.min.y.max(0);
            let x1 = rc.max.x.min(sc.size.width);
            let y1 = rc.max.y.min(sc.size.height);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let src_box = D3D11_BOX {
                left: x0 as u32, top: y0 as u32, front: 0,
                right: x1 as u32, bottom: y1 as u32, back: 1,
            };
            (*ctx.as_ptr()).CopySubresourceRegion(
                fp.texture as *mut _, 0, x0 as u32, y0 as u32, 0,
                src as *mut _, 0, &src_box,
            );
        }
        (*(src as *mut IUnknown)).Release();
        true
    }
}

/// Present1 + DirtyRects 힌트(스펙 §5.2-3). 16 초과·공집합이면 힌트 없이 전체.
fn present1_partial(sc: &SwapChainStorage, dirty: &[DeviceIntRect]) -> bool {
    let rects: Vec<RECT> = dirty
        .iter()
        .filter(|r| r.max.x > r.min.x && r.max.y > r.min.y)
        .map(|r| RECT {
            left: r.min.x.max(0),
            top: r.min.y.max(0),
            right: r.max.x.min(sc.size.width),
            bottom: r.max.y.min(sc.size.height),
        })
        .collect();
    let use_hint = !rects.is_empty() && rects.len() <= MAX_PRESENT_DIRTY_RECTS;
    let params = DXGI_PRESENT_PARAMETERS {
        DirtyRectsCount: if use_hint { rects.len() as u32 } else { 0 },
        pDirtyRects: if use_hint { rects.as_ptr() as *mut RECT } else { ptr::null_mut() },
        pScrollRect: ptr::null_mut(),
        pScrollOffset: ptr::null_mut(),
    };
    // Safety: 살아있는 스왑체인. SyncInterval 0 = 기존 Present와 동일 페이싱.
    let hr = unsafe { (*sc.swapchain.as_ptr()).Present1(0, 0, &params) };
    if hr < 0 {
        warn!("[dcomp-native] Present1 failed (hr=0x{:08x})", hr as u32);
        return false;
    }
    true
}
```

import 추가: `winapi::um::d3d11::{D3D11_BOX, ID3D11DeviceContext}`, `winapi::shared::dxgi1_2::DXGI_PRESENT_PARAMETERS`. borrow 참고: end_frame 루프는 `self.surfaces.iter_mut()` 중이므로 `self.d3d11_context` 접근이 필요하면 루프 앞에서 `let ctx = self.d3d11_context.clone();`(ComOwned가 Clone 미구현이면 `as_ptr` 원시 포인터 캡처 — 기존 `let rc = self.rendering_context.clone();` :992 관례를 따라 해결).

- [ ] **Step 4: 빌드 + 유닛테스트 + 실기 검증**

```powershell
. .\etc\multigpu\servo_env.ps1; $ErrorActionPreference='Continue'
cargo test -p servo-paint --lib dcomp
python mach build --release
```
Expected: 테스트 PASS, 빌드 성공.

실기(기존 servoshell kill 후):
```powershell
$env:SERVO_DCOMP_DEBUG = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 2 -DComp -Page tests\html\mixed_media_demo.html -MoveX 1920 -MoveY 0 -Detach
# 20초 후 최신 stderr 로그에서:
# (1) promote 로그 존재(비디오 슬라이스) + 그 프레임 번호가 시작 직후가 아님
# (2) 'withhold' 라인이 content-swap 이후 0건 (부분 Present가 대체)
# (3) 비디오 영역 CopyFromScreen 픽셀이 2초 간격 두 샘플에서 상이(재생 중)
# (4) 45타일 순수 월(-Page 없이 -Cols 9 -Rows 5)에서 promote 후 정상 재생 + fps 저하 없음
```
Expected: (1)-(4) 충족. 시계·티커의 완전 표시는 결함②(알파 슬라이스)에 종속이므로 이 태스크의 판정 기준이 아니다 — 판정은 "동결 없음 + withhold 무한 부재".

- [ ] **Step 5: 커밋**

```powershell
git add components/paint/dcomp_compositor.rs
git commit -m "dcomp: 부분 Present 본구현 - FLIP_SEQUENTIAL + catch-up 복사 + Present1 더티 힌트"
```

---

### Task 5: 강등(demote) 폴백 + 쿨다운 본처리

**Files:**
- Modify: `components/paint/dcomp_compositor.rs`

**Interfaces:**
- Consumes: Task 3 필드(demote_count/promote_blocked_until/frame_counter), Task 4 `demote_requests` push 지점·d3d11_context.
- Produces: end_frame 강등 처리 루프(스펙 §6). Task 7 검증이 소비.

- [ ] **Step 1: withhold 분기에 강등 트리거 추가**

Task 4 Step 3의 마지막 else-if(withhold) 분기에서 `sc.withheld_frames += 1;` 직후:

```rust
                        if sc.withheld_frames >= DEMOTE_AFTER_WITHHOLD {
                            // 스펙 §6.1: 첫 Present 전이면 무조건, 후면 부분 Present 불가
                            // 상태에서만 이 분기에 도달한다(가능 상태는 위 분기가 소비).
                            demote_requests.push(*id);
                        }
```

- [ ] **Step 2: 강등 처리 루프 (regen_requests 처리 :1202 뒤, 컬링 앞)**

Task 4의 임시 warn 루프를 본 처리로 교체:

```rust
        // 강등(스펙 §6): 스왑체인 → 가상 서피스 복귀. 시딩으로 표시 최신성 보장.
        for surface_id in demote_requests {
            let Some(dcomp_device) = self.dcomp_device_ptr() else { break };
            let ctx = /* Task 4와 동일한 방식으로 확보한 d3d11_context 접근 */;
            let Some(entry) = self.surfaces.get_mut(&surface_id) else { continue };
            let SurfaceStorage::SwapChain(sc) = &mut entry.storage else { continue };
            release_frame_pbuffer(&rc, sc);

            let seeded = if !sc.content_attached {
                // §6.2-1: fallback_virtual 생존 — 누적 더티 영역만 buffer 0에서 복사해 최신화.
                // 누적 더티 = coverage.covered_tiles의 타일 rect들(+frame_dirty 잔여).
                // 타일 단위 과대는 안전(그 타일 안 미갱신 픽셀도 buffer 0에 승격 이후
                // 값이 있으면 함께 복사되지만, 승격 후 그린 적 없는 타일은 목록에 없다).
                demote_seed_into_fallback(dcomp_device, ctx, sc, &entry.tiles, entry.virtual_offset, entry.tile_size)
            } else {
                // §6.2-2: 새 가상 서피스 생성 + buffer 0 전체 복사 + SetContent 전환.
                demote_seed_new_virtual(dcomp_device, ctx, sc, entry.is_opaque, entry.virtual_offset)
            };
            if !seeded {
                warn!("[dcomp-native] demote seeding failed for {:?}; keeping swapchain (frozen)", surface_id);
                continue;
            }
            // storage 교체 + 쿨다운. fallback 또는 새 virtual이 seeded 내부에서 확보됨.
            // (seeded 헬퍼가 Option<ComOwned<IDCompositionVirtualSurface>>를 돌려주는
            //  시그니처로 하고, 여기서 storage = Virtual{..}로 교체하는 형태로 구현.)
            entry.demote_count = entry.demote_count.saturating_add(1);
            let cooldown = (DEMOTE_COOLDOWN_BASE << (entry.demote_count.saturating_sub(1)).min(4))
                .min(DEMOTE_COOLDOWN_CAP);
            entry.promote_blocked_until = self.frame_counter + cooldown;
            entry.promote_streak = 0;
            entry.frame_coverage.reset();
            if dcomp_debug() {
                log::info!("[dcomp-dbg] demote id={:?} count={} cooldown={}",
                    surface_id, entry.demote_count, cooldown);
            }
        }
```

시딩 헬퍼 2개(자유 함수). 공통 재료: buffer 0 텍스처는 `GetBuffer(0)`으로 새로 얻는다(frame_pbuffer는 위에서 해제). 좌표: BeginDraw는 **가상공간 절대좌표** RECT(기존 bind Virtual arm :764-770과 동일 규약), 복사 소스 박스는 **버퍼-로컬**(가상 − anchor).

```rust
/// §6.2-1: 승격 후 그려진 타일들의 영역을 buffer 0 → fallback_virtual로 복사해 최신화.
/// 반환: 시딩된 가상 서피스(성공 시 fallback take), 실패 시 None(스왑체인 유지).
fn demote_seed_into_fallback(
    dcomp_device: *mut IDCompositionDevice,
    ctx: *mut ID3D11DeviceContext,
    sc: &mut SwapChainStorage,
    tiles: &std::collections::HashSet<(i32, i32)>,
    virtual_offset: DeviceIntPoint,
    tile_size: DeviceIntSize,
) -> Option<ComOwned<IDCompositionVirtualSurface>> {
    let fallback = sc.fallback_virtual.take()?;
    // 승격 이후 스왑체인에 그려진 적 있는 타일 = sc.coverage.covered_tiles ∪ 이번 프레임
    // note_tile 대상. 보수적으로 covered_tiles를 쓰되, 비어 있으면(한 번도 전면 타일이
    // 없었음) frame_dirty 누적이 없으므로 fallback이 이미 최신 — 복사 없이 성공 처리.
    // 각 타일: BeginDraw(tile_virtual_rect) → CopySubresourceRegion(buffer0, 버퍼-로컬
    // 박스, dst=BeginDraw 텍스처+update_offset) → EndDraw. HRESULT 실패 시 EndDraw
    // 정리 후 fallback을 sc에 되돌리고 None.
    // ... (bind Virtual arm의 BeginDraw/EndDraw 시퀀스를 GL 없이 D3D 복사로 재사용)
    Some(fallback)
}

/// §6.2-2: 새 가상 서피스 + buffer 0 전체 복사 + visual.SetContent(virtual) +
/// last_placement 재적용(content-swap :1077-1085와 동일 원자 전환, anchor=zero 가상 산식).
fn demote_seed_new_virtual(...) -> Option<ComOwned<IDCompositionVirtualSurface>> {
    // CreateVirtualSurface(VIRTUAL_SURFACE_SIZE², B8G8R8A8, is_opaque에 따른 alpha_mode)
    // → BeginDraw(가상좌표 anchor..anchor+size 렉트) → buffer 0 전체 박스 복사 →
    // EndDraw → SetContent(virtual) → apply_visual_placement(visual, last_placement,
    //   virtual_offset, DeviceIntPoint::zero())  // 가상 산식 복귀 — 같은 Commit에서 원자
    // 실패 시 각 단계 정리 후 None.
}
```

구현 주의(스펙 §6.4): storage를 `SurfaceStorage::Virtual { virtual_surface }`로 교체하면 ComOwned Drop이 옛 스왑체인을 Release — visual에는 이미 새 virtual이 SetContent됐으므로(케이스 2) 또는 fallback이 계속 붙어 있으므로(케이스 1) 안전. `displayed_anchor`는 storage와 함께 소멸(Virtual arm의 content_anchor=zero 산식 :956-959가 자동 적용).

- [ ] **Step 3: 빌드 + 강등 경로 실기 검증**

```powershell
. .\etc\multigpu\servo_env.ps1; $ErrorActionPreference='Continue'
cargo test -p servo-paint --lib dcomp
python mach build --release
# 강등 경로 강제: 부분 Present 차단
$env:SERVO_DCOMP_NO_PARTIAL_PRESENT = "1"
$env:SERVO_DCOMP_DEBUG = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 2 -DComp -Page tests\html\mixed_media_demo.html -MoveX 1920 -MoveY 0 -Detach
```
Expected(로그 + 화면):
1. 비디오 슬라이스 promote 후, 부분 더티 프레임에서 withhold가 30에 도달하면 `demote` 로그(count=1, cooldown=300) — 이후 비디오 계속 재생(가상 서피스 경로, 픽셀 시변).
2. 화면 동결 없음(45초 관찰, 2초 간격 CopyFromScreen 시변).
3. 재승격 발생 시(전면 더티 복귀) 쿨다운 프레임 이후에만 promote 로그.
4. env 제거 후 재실행 → demote 로그 없이 부분 Present 경로(Task 4 판정 재확인).
5. `quad_promote_partial.html?full=120` (기존 evidence 페이지): 승격→부분 전환 시나리오 픽셀 정확(기존 검증 방법 재사용).

- [ ] **Step 4: 커밋**

```powershell
git add components/paint/dcomp_compositor.rs
git commit -m "dcomp: 가상 서피스 자동 강등 - 백버퍼 시딩 2케이스 + 재승격 지수 쿨다운"
```

---

### Task 6: 결함② 진단 — readback 계측 + 재현 페이지 + 판정 (결정 게이트)

**Files:**
- Modify: `components/paint/dcomp_compositor.rs` (readback 계측)
- Create: `tests/html/dcomp_alpha_probe.html`
- Commit(기존 미추적): `tests/html/mixed_media_demo.html`
- Create: `.superpowers/sdd/evidence/alpha-slice-diagnosis.md` (진단 보고서)

**Interfaces:**
- Consumes: unbind()(:862, pbuffer가 아직 current인 시점), `SERVO_DCOMP_READBACK` env.
- Produces: 진단 보고서(판정: 우리 층 vs WR급) — 컨트롤러 결정 게이트 입력.

- [ ] **Step 1: readback 계측 추가**

env 게이트(OnceLock)와 프레임 상한(스펙 §7.1 — 진단 로깅 스톨 증폭 함정):

```rust
/// 결함② 진단: unbind 직전 타일 픽셀 readback(스펙 §7.1). 프레임 120까지만.
fn readback_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SERVO_DCOMP_READBACK").is_ok())
}
```

unbind()에서 EndDraw 전에(bound가 Some인 Virtual 경로 — pbuffer가 아직 current):

```rust
        if readback_enabled() && self.frame_counter <= 120 {
            if let Some(bound) = &self.bound {
                // GL이 pbuffer에 current인 시점 — 8x8 그리드 glReadPixels 샘플.
                // rendering_context에 간단 헬퍼가 없으므로 gleam 직접 사용이 불가한
                // 경우 rc.read_pixels_rgba(x,y,1,1) 류가 있는지 먼저 확인하고, 없으면
                // paint_api RenderingContext의 기존 GL 접근 경로(Device::gl()은 여기
                // 없음)를 쓰지 말고 **surfman GL 함수 포인터**(rc가 이미 pbuffer를
                // current로 만든 EGL 컨텍스트)를 통해 gl::ReadPixels를 호출한다 —
                // bind에서 make_render_pbuffer_current가 성공한 컨텍스트 그대로다.
                // 로그 형식(영어): "[dcomp-readback] surface=? tile=(x,y) opaque=?
                //   alpha_min=? alpha_max=? rgb_nonzero=N/64"
            }
        }
```

(정확한 GL 접근 수단은 구현 시 `rendering_context.rs`의 기존 public 표면에서 선택 — 이 태스크의 산출물은 계측 로그이므로 수단은 자유. 스왑체인 arm은 bound=None이라 자연 제외 — 알파 슬라이스는 항상 Virtual이므로 진단 대상 커버에 충분.)

- [ ] **Step 2: 재현 페이지 작성**

`tests/html/dcomp_alpha_probe.html` — 구조(스펙 §7.2): 불투명 body(#000) + 전면 비디오 1개(대조군: 불투명 슬라이스, `../Wildlife_FHD30fps_counter_10Mbitrate.mp4`, muted/loop/playsInline/autoplay) + `position:fixed; will-change:transform` 오버레이 박스(중앙 40vw×30vh, 알파 슬라이스 강제) + rAF 하트비트(2px 점 색 토글 — video_grid_6x6_play.html 관례 복사). `?case=` 분기:
- `solid`: 오버레이에 `background: rgba(200,40,60,0.55)` (대조군 — 표시돼야 정상)
- `text`: 오버레이에 흰색 3vh 텍스트 여러 줄
- `video`: 오버레이 안에 소형 video 요소(같은 소스, 40vw×30vh)

```html
<!-- 골격 (전체는 구현 시 관례대로; 하트비트·자동재생 필수) -->
<div id="base"><video id="bg" ...></video></div>
<div id="overlay" style="position:fixed; left:30vw; top:35vh; width:40vw; height:30vh;
     will-change:transform; z-index:5;"></div>
<div id="hb"></div>
<script>
  const c = new URLSearchParams(location.search).get("case") || "solid";
  const ov = document.getElementById("overlay");
  if (c === "solid") { ov.style.background = "rgba(200,40,60,0.55)"; }
  if (c === "text")  { ov.innerHTML = "<div style='color:#fff;font-size:3vh'>알파 슬라이스 텍스트 진단<br>ALPHA SLICE TEXT</div>"; }
  if (c === "video") { ov.innerHTML = "<video src='../Wildlife_FHD30fps_counter_10Mbitrate.mp4' muted loop autoplay playsinline style='width:100%;height:100%;object-fit:fill'></video>"; }
  // rAF 하트비트 (video_grid_6x6_play.html과 동일)
</script>
```

- [ ] **Step 3: 진단 실행 (3케이스 × =surface, 필요 시 =1 교차)**

```powershell
python mach build --release   # (계측 포함 빌드)
$env:SERVO_DCOMP_READBACK = "1"; $env:SERVO_DCOMP_DEBUG = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -DComp -DCompSurface -Page "tests\html\dcomp_alpha_probe.html?case=video" -MoveX 1920 -Detach
# 각 케이스: (a) [dcomp-readback] 알파 서피스 타일의 alpha/rgb 통계
#            (b) CopyFromScreen 오버레이 중심 픽셀
# 케이스별 기록: solid(대조군: readback 유색+화면 유색 예상) / text / video
```

판정 트리(스펙 §7.3)를 적용해 `.superpowers/sdd/evidence/alpha-slice-diagnosis.md`에 기록:
- readback 정상(비디오 RGB 존재) + 화면 검정 → **우리 층(DComp 합성/서피스)** — 알파 모드/premultiply/BeginDraw 파라미터 후보를 증거와 함께 명시.
- readback부터 0/검정 → **WR draw 층** — GL 에러 유무, 대조군(solid)과의 차이를 명시하고, 우리 층 우회 가능성(bind 상태) vs WR 내부(vendoring) 판정.

- [ ] **Step 4: 결정 게이트 (HALT 분기)**

- 판정 = **우리 층**: 보고서에 구체 수정안(1-2문단)을 적고 커밋 후, **컨트롤러가 수정 태스크를 이 보고서 기반으로 추가 디스패치**한다(수정 내용은 진단 결과에 종속이라 본 계획에 사전 기술 불가 — 스펙 §7.3).
- 판정 = **WR급**: 수정 착수 없이 보고서만 커밋하고 HALT — 컨트롤러가 사용자에게 규모·선택지 보고(스펙 §8).

- [ ] **Step 5: 커밋**

```powershell
git add components/paint/dcomp_compositor.rs tests/html/dcomp_alpha_probe.html tests/html/mixed_media_demo.html .superpowers/sdd/evidence/alpha-slice-diagnosis.md
git commit -m "dcomp: 알파 슬라이스 결함 진단 - readback 계측 + 재현 페이지 + 판정 보고"
```

---

### Task 7: 통합 검증 (수용 기준, 스펙 §9)

**Files:**
- Modify: (코드 무변경 — 검증 결과를 `.superpowers/sdd/evidence/`에 기록)

**Interfaces:**
- Consumes: Task 4·5(·6 수정 시) 빌드.

- [ ] **Step 1: mixed_media_demo (=1) 45초**

```powershell
$env:SERVO_DCOMP_DEBUG = "1"
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 2 -DComp -Page tests\html\mixed_media_demo.html -MoveX 1920 -MoveY 0 -Detach
# 45초 동안 15초 간격 3회 CopyFromScreen 샘플(비디오 4점 + 상단바 + 티커바):
```
Expected: 비디오 픽셀 3회 모두 상이(재생), 동결 0, 로그에 withhold 무한 없음(모든 승격 서피스가 present-partial 또는 demote로 수렴), panic 0. 시계·티커·자막 표시는 결함② 게이트 결과에 따라 판정(수정됐으면 표시 필수, HALT면 제외 기록 — 스펙 §9-1).

- [ ] **Step 2: 45타일 순수 월 무회귀 (=1)**

```powershell
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 9 -Rows 5 -DComp -MoveX 1920 -Detach
```
Expected: 45타일 lockstep(±1 프레임, 내장 카운터 육안/캡처), 5분 메모리 플랫(작업 관리자 or Get-Process WS 3회 샘플), fps 60(페이지 카운터), 로그에서 catch-up 복사 0 확인(`[dcomp-dbg]` present-partial의 catchup=0 또는 전면 Present 경로만), PresentMon으로 Composed: Flip 유지(선행 사이클 도구 D:\PresentMon-2.3.1-x64.exe, 관리자, servoshell 전면).

- [ ] **Step 3: quad_promote_partial 회귀 + 강등/재승격**

Task 5 Step 3의 5번 항목 + `SERVO_DCOMP_NO_PARTIAL_PRESENT=1` 조합 재확인(픽셀 정확·이탈 없음).

- [ ] **Step 4: =surface / off 무회귀**

```powershell
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 2 -DComp -DCompSurface -Page tests\html\video_grid_6x6_play.html -MoveX 1920 -Detach   # 표시 정상
.\etc\multigpu\run_video_wall_d3d11.ps1 -Cols 3 -Rows 2 -Page tests\html\mixed_media_demo.html -MoveX 1920 -Detach                            # off: 전 요소 정상(오늘 기확인 기준선)
```

- [ ] **Step 5: 결과 기록 + 커밋**

`.superpowers/sdd/evidence/mixed-content-verification.md`에 항목별 PASS/FAIL 기록.

```powershell
git add .superpowers/sdd/evidence/mixed-content-verification.md
git commit -m "dcomp: 복합 콘텐츠 통합 검증 결과 기록"
```

---

### Task 8: 패키징 + 스펙 구현 결과 기록

**Files:**
- Modify: `docs/superpowers/specs/2026-07-14-dcomp-mixed-content-design.md` (§11 구현 결과 추가)
- Modify: `D:\ServoWallPackage\` (exe/페이지/가이드 갱신) → `D:\ServoWallPackage.zip` 재생성

**Interfaces:**
- Consumes: Task 7 PASS.

- [ ] **Step 1: 패키지 갱신**

- `D:\ServoWallPackage\servoshell.exe`(및 관련 바이너리)를 최신 빌드(`target\release\`)로 교체.
- `tests\html\mixed_media_demo.html`, `tests\html\dcomp_alpha_probe.html`을 패키지 페이지 폴더(기존 video_grid_6x6_play.html 위치와 동일 상대 구조)에 추가.
- `run_wall.ps1`의 판독 가이드에 추가: "성능 A/B(3중: 없이/-DComp -DCompSurface/-DComp)는 **반드시 순수 비디오 월 페이지**로 측정. mixed_media_demo는 복합 콘텐츠 **정확성** 확인용."
- 재압축: `Compress-Archive -Path D:\ServoWallPackage\* -DestinationPath D:\ServoWallPackage.zip -Force` (주의: Remove-Item으로 기존 zip 삭제 불가 — -Force 덮어쓰기 관례).

- [ ] **Step 2: 스펙에 §11 구현 결과 추가**

스펙 파일 끝에 `## 11. 구현 결과 (사후 기록)` 섹션: PoC G1~G4 실측, 부분 Present 채택 여부, 강등 동작 실측, 결함② 판정 결과와 게이트 결정, 검증 결과 요약, 이탈 사항(있으면).

- [ ] **Step 3: 커밋**

```powershell
git add docs/superpowers/specs/2026-07-14-dcomp-mixed-content-design.md
git commit -m "docs: DComp 복합 콘텐츠 스펙 구현 결과 기록 + 패키지 재생성"
```

---

## Self-Review 결과 (계획 작성 후 점검)

1. **스펙 커버리지**: §3(게이트 3종)=T4/T6, §4(승격 위생)=T3, §5(PoC+부분 Present)=T1/T2/T4, §6(강등 3조건+시딩 2케이스+쿨다운)=T5, §7(진단)=T6, §8(에러 처리: warn-once/즉시 강등)=T4 Step3·T5, §9(수용 기준 6항)=T7(1-5)+T8(6), §10(이월)=해당 없음. 갭 없음.
2. **자리표시자**: T5 Step 2의 시딩 헬퍼 2개와 T6 Step 1의 GL readback 수단은 의도적 구현 재량(정확한 실행 시퀀스·규약·실패 처리 방향을 본문에 명시, BeginDraw/EndDraw 선례 참조 지정) — 진단 태스크 특성상 판정 절차가 산출물이며, 수정 코드는 §7.3 결정 게이트 뒤에만 존재 가능(스펙 정합).
3. **타입 일관성**: `StaleTracker`(T2 정의 → T4 소비), `demote_requests: Vec<NativeSurfaceId>`(T4 선언 → T5 처리), 상수명(T3 정의 → T4/T5 사용) 일치 확인.

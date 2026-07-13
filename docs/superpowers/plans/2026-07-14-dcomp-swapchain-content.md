# DComp 스왑체인 콘텐츠 + 레이어 컬링 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 게이트 on 월 표출에서 콘텐츠 슬라이스를 DComp 가상 서피스 대신 flip 스왑체인에 그리고(대여/반납 기구 제거) 가려진 불투명 레이어를 컬링해, probe 동형(콘텐츠 1× draw + flip present + DWM 1레이어)을 달성한다.

**Architecture:** `DCompNativeCompositor` 내부에서 서피스 저장소를 하이브리드화 — 전면 dirty가 반복되는 서피스만 스왑체인으로 승격, 나머지는 기존 가상 서피스. Present는 "백버퍼 full coverage" 시에만(잔상 원천 차단, 부분 dirty는 같은 백버퍼에 누적). visual 트리 조립을 end_frame으로 이연해 가려진 불투명 레이어를 컬링. surfman/paint_api/painter/servoshell/WR 무수정(게이트 파서 1곳 제외).

**Tech Stack:** Rust, winapi 0.3.9 (dxgi1_2 `CreateSwapChainForComposition`, 기존 dcomp), webrender 0.68 (crates.io 무수정), 기존 pbuffer 래핑 인터롭.

**Spec:** `docs/superpowers/specs/2026-07-14-dcomp-swapchain-content-design.md`

## Global Constraints

- 게이트 값: `SERVO_COMPOSITOR_DCOMP=1`(기존 truthy) = 하이브리드, **`=surface`** = 가상 서피스 전용(구 경로 — 동작 바이트 동일 보존, AMD A/B), off = Draw 무변경.
- **알파(비불투명) 서피스는 절대 컬링하지 않는다.**
- **스왑체인 Present는 백버퍼 full coverage(아래 규칙)에서만** — 잔상·깨짐 불허, 갱신 지연은 허용. (스펙 §7의 "보류+강등"을 "보류-누적"으로 정제: Present를 안 하면 GetBuffer(0)가 같은 버퍼이므로 부분 draw가 누적되어 full 도달 시 Present — 강등 경로 불필요, 스펙의 규범 목표를 더 단순하게 충족.)
- **full coverage 판정은 타일 단위 보수 규칙**: bind의 `dirty_rect ⊇ valid_rect`인 타일만 "완전 갱신"으로 집계, 서피스의 전 타일이 집계되면 full. (union-rect 근사는 대각 부분 dirty에서 거짓 full 가능 — 금지.)
- components/paint는 clippy unwrap/panic deny. 실패는 로그 + 서피스 단위 Virtual 폴백(화면 유지).
- 커밋 한국어, Claude 부기 금지. 한글 파일 수정은 Edit 도구.
- ★빌드: 워크스페이스 게이트는 `.\mach build --release`(etc/multigpu/servo_env.ps1 소싱 + `$ErrorActionPreference='Continue'`, 출력 파일 리다이렉트, 동기 폴링). `cargo check --workspace` 금지. surfman은 워크스페이스 비멤버 — 테스트는 크레이트 디렉터리/스탠드얼론 사본에서. servo-paint 단독 check/test는 선재 feature-unification으로 불가할 수 있음(시도 후 문서화, 실게이트는 mach+스모크).
- 스모크에서 띄운 servoshell은 Stop-Process 후 실사망 확인. 성능 판정 런은 기본 로그(진단 env 끔).

## 확정된 코드 앵커 (2026-07-14 실측)

| 항목 | 사실 |
|---|---|
| winapi | `CreateSwapChainForComposition`(shared/dxgi1_2.rs:215), `DXGI_SWAP_CHAIN_DESC1`(:121), `IDXGIFactory2`(:170), `IDXGISwapChain1`, `DXGI_SCALING_STRETCH`(:53), `DXGI_ALPHA_MODE_*`(:28), `DXGI_SWAP_EFFECT_FLIP_DISCARD`(dxgi.rs:71) 전부 존재. paint는 이미 dxgi1_2 임포트 사용 중(피처 활성) |
| GL 제출 보장 | `webrender::Device::gl()` pub(device/gl.rs:1964) → end_frame에서 `device.gl().flush()` 후 Present (bind 간에는 eglMakeCurrent가 EGL 규약상 이전 컨텍스트 작업을 flush) |
| add_surface 순서 | z-순서 아래→위 (composite.rs:1553-1560 trait 문서 + 현 구현 AddVisual 주석) — 컬링은 역순 순회 |
| redraw_on_invalidation | 콘텐츠 무변경 프레임 무효화 시 ForceRedraw 용(render_backend.rs:1601) — 부분 dirty 안전망으로 부적합 확정. 미사용 |
| FORCE_PICTURE_INVALIDATION | DebugFlags로 런타임 존재(render_backend.rs:1274) — 이번 미사용, 보류 장기화 관측 시 후속 레버로 기록만 |
| 현 구현 | dcomp_compositor.rs 675줄: `SurfaceEntry{virtual_surface,visual,virtual_offset,tile_size,is_opaque}`(:95), `BoundTile`(:106), bind(:354)/unbind(:467)/begin_frame(:494)/add_surface(:506, AddVisual 즉시)/end_frame(:595, Commit만)/create_tile·destroy_tile no-op(:346-352), `dcomp_debug()` OnceLock(:50), `tile_virtual_rect`(:57) |
| surfman 게이트 파서 | `dcomp_native_compositor_requested()`(third_party/surfman/.../angle/surface.rs, truthy="1/true/yes/on") — **"surface" 값이 현재 false 판정 → truthy 집합에 추가 필수**(창 서피스 opt-out·ppf 제외가 =surface에서도 켜져야 함) |
| 스왑체인 파라미터 | FLIP_DISCARD(이전 버퍼 미참조 — coverage 규칙이 대체), BufferCount=2, `DXGI_SCALING_STRETCH`(composition 필수), Format B8G8R8A8, AlphaMode는 is_opaque 따라 IGNORE/PREMULTIPLIED, BufferUsage=RENDER_TARGET_OUTPUT |
| bind origin (스왑체인) | `origin = tile_rect.min − anchor` (anchor=extent.min, 가상좌표). virtual 경로의 `update_offset − dirty.min`과 등가 규약 |
| Present 미실행 시 | GetBuffer(0)는 동일 버퍼 유지 → 부분 draw 누적이 성립하는 근거 |

## File Structure

- Modify: `components/paint/dcomp_compositor.rs` — 전 태스크의 중심 (저장소 enum, 승격/coverage, 스왑체인 헬퍼, 컬링)
- Modify: `third_party/surfman/src/platform/windows/angle/surface.rs` — truthy에 "surface" 추가 (+ 테스트)
- Modify: `etc/multigpu/run_video_wall_d3d11.ps1`, `D:\ServoWallPackage\run_wall.ps1` — `-DCompSurface` (Task 6)
- Modify: `docs/superpowers/specs/2026-07-14-dcomp-swapchain-content-design.md` — §10 구현결과 (Task 6)
- Create: `.superpowers/sdd/evidence/overlay_wall.html` — 텍스트 오버레이 검증 페이지 (Task 5)

---

### Task 1: 게이트 확장 + 순수 로직 3종 (모드/coverage/컬링)

**Files:**
- Modify: `third_party/surfman/src/platform/windows/angle/surface.rs` (truthy 집합)
- Modify: `components/paint/dcomp_compositor.rs` (모드 파서 + 순수 로직 + 테스트; COM 미접촉)

**Interfaces:**
- Produces (Task 2-4가 사용):
  - `enum StorageMode { Hybrid, SurfaceOnly }` + `fn storage_mode() -> StorageMode` (OnceLock 캐시)
  - `struct FrameCoverage { covered_tiles: HashSet<(i32,i32)> }` — `fn reset(&mut self)`, `fn note_tile(&mut self, tile:(i32,i32), dirty:DeviceIntRect, valid:DeviceIntRect)`(dirty ⊇ valid일 때만 집계), `fn is_full(&self, tiles:&HashSet<(i32,i32)>) -> bool`(비어있지 않고 tiles ⊆ covered)
  - `fn cull_covered(entries: &[(DeviceIntRect, bool)]) -> Vec<bool>` — (device 클립, opaque) 아래→위 순 입력, 표시 여부 반환
  - `fn surface_extent(tiles:&HashSet<(i32,i32)>, virtual_offset:DeviceIntPoint, tile_size:DeviceIntSize) -> Option<DeviceIntRect>`

- [ ] **Step 1: surfman truthy 확장 + 테스트**

`dcomp_native_compositor_requested()`의 truthy 판정에 `"surface"` 추가 (=surface에서도 창 서피스 opt-out·ppf 제외가 켜져야 전체 구성이 정합 — 세부 모드만 컴포지터가 나눔):

```rust
pub fn dcomp_native_compositor_requested() -> bool {
    std::env::var("SERVO_COMPOSITOR_DCOMP").is_ok_and(|v| {
        v == "1"
            || v.eq_ignore_ascii_case("true")
            || v.eq_ignore_ascii_case("yes")
            || v.eq_ignore_ascii_case("on")
            // "surface" = 네이티브 컴포지터 on + 가상 서피스 전용 모드(구 경로 A/B).
            // 모드 세부는 paint::dcomp_compositor::storage_mode()가 판정한다.
            || v.eq_ignore_ascii_case("surface")
    })
}
```

기존 파라미터화 테스트 옆에 truthy 케이스 테스트 추가(env 무의존이 불가한 함수이므로 — 기존 이 함수 테스트 관례를 따르되 env 조작 테스트가 없다면 생략하고 luid_display_attribs 테스트만 유지, 판정은 Task 2 스모크 마커로).

- [ ] **Step 2: 순수 로직 + 단위 테스트 작성** (dcomp_compositor.rs, `tile_virtual_rect` 아래)

```rust
/// SERVO_COMPOSITOR_DCOMP 값의 세부 모드. "surface"=가상 서피스 전용(구 경로 A/B),
/// 그 외 truthy=하이브리드(전면 갱신 서피스를 스왑체인으로 승격).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StorageMode {
    Hybrid,
    SurfaceOnly,
}

fn storage_mode() -> StorageMode {
    static MODE: std::sync::OnceLock<StorageMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        match std::env::var("SERVO_COMPOSITOR_DCOMP") {
            Ok(v) if v.eq_ignore_ascii_case("surface") => StorageMode::SurfaceOnly,
            _ => StorageMode::Hybrid,
        }
    })
}

/// 진단: 컬링만 끄는 스위치(요소 소실 의심 시 즉시 판별용).
fn cull_disabled() -> bool {
    static NO_CULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NO_CULL.get_or_init(|| std::env::var("SERVO_DCOMP_NO_CULL").is_ok())
}

/// 스왑체인 백버퍼의 유효 피복(타일 단위, Present까지 누적).
/// Present를 안 하면 flip이라도 GetBuffer(0)가 같은 버퍼이므로 누적이 성립한다.
/// 판정은 보수적: bind의 dirty가 그 타일의 valid_rect 전체를 덮을 때만 집계
/// (부분 dirty 타일은 미피복 취급 — 잔상 불허, 지연 허용. 스펙 §7 정제).
#[derive(Default)]
struct FrameCoverage {
    covered_tiles: std::collections::HashSet<(i32, i32)>,
}

impl FrameCoverage {
    fn reset(&mut self) {
        self.covered_tiles.clear();
    }
    fn note_tile(&mut self, tile: (i32, i32), dirty: DeviceIntRect, valid: DeviceIntRect) {
        if dirty.contains_box(&valid) {
            self.covered_tiles.insert(tile);
        }
    }
    fn is_full(&self, tiles: &std::collections::HashSet<(i32, i32)>) -> bool {
        !tiles.is_empty() && tiles.iter().all(|t| self.covered_tiles.contains(t))
    }
}

/// 서피스의 타일 집합 → 가상공간 extent(스왑체인 크기·anchor의 근거).
fn surface_extent(
    tiles: &std::collections::HashSet<(i32, i32)>,
    virtual_offset: DeviceIntPoint,
    tile_size: DeviceIntSize,
) -> Option<DeviceIntRect> {
    let mut it = tiles.iter();
    let first = *it.next()?;
    let mut rect = tile_virtual_rect(virtual_offset, tile_size, first.0, first.1);
    for &(x, y) in it {
        rect = rect.union(&tile_virtual_rect(virtual_offset, tile_size, x, y));
    }
    Some(rect)
}

/// 레이어 컬링: entries는 add_surface 순서(z 아래→위)의 (device 클립, 불투명 여부).
/// 최상위부터 훑어, 불투명 클립이 완전히 포함하는 하위 항목을 숨긴다.
/// 알파(불투명 아님) 항목은 절대 숨겨지지 않는 것이 아니라 — 알파 항목도 "위의
/// 불투명"에 완전히 덮이면 안 보이므로 숨김 대상이 될 수 있다. 숨기는 주체가
/// 불투명이어야 한다는 것이 안전 조건이다.
fn cull_covered(entries: &[(DeviceIntRect, bool)]) -> Vec<bool> {
    let mut visible = vec![true; entries.len()];
    for top in (0..entries.len()).rev() {
        if !entries[top].1 || !visible[top] {
            continue;
        }
        for below in 0..top {
            if visible[below] && entries[top].0.contains_box(&entries[below].0) {
                visible[below] = false;
            }
        }
    }
    visible
}
```

(`DeviceIntRect`는 euclid Box2D — `contains_box`/`union` 메서드명이 다르면 이 파일의 기존 사용례·euclid 문서에 맞춰 통일하고 테스트로 고정할 것.)

- [ ] **Step 3: 단위 테스트** (기존 `mod tests`에 추가)

```rust
    fn r(x0: i32, y0: i32, x1: i32, y1: i32) -> DeviceIntRect {
        DeviceIntRect::new(DeviceIntPoint::new(x0, y0), DeviceIntPoint::new(x1, y1))
    }

    #[test]
    fn coverage_full_only_when_every_tile_fully_drawn() {
        let tiles: std::collections::HashSet<_> = [(0, 0), (1, 0)].into_iter().collect();
        let mut cov = FrameCoverage::default();
        let valid = r(0, 0, 100, 100);
        cov.note_tile((0, 0), r(0, 0, 100, 100), valid);
        assert!(!cov.is_full(&tiles)); // 타일 하나 남음
        cov.note_tile((1, 0), r(0, 0, 50, 100), valid); // 부분 dirty → 미집계
        assert!(!cov.is_full(&tiles));
        cov.note_tile((1, 0), r(0, 0, 100, 100), valid); // 누적 프레임에서 완전 갱신
        assert!(cov.is_full(&tiles));
        cov.reset();
        assert!(!cov.is_full(&tiles));
    }

    #[test]
    fn cull_hides_fully_covered_below_opaque_top() {
        // 월 실측 구조: 전면 불투명 2장 → 하위 숨김
        let v = cull_covered(&[(r(0, 0, 1920, 1080), true), (r(0, 0, 1920, 1080), true)]);
        assert_eq!(v, vec![false, true]);
        // 최상위가 알파면 아무도 못 숨김
        let v = cull_covered(&[(r(0, 0, 1920, 1080), true), (r(0, 0, 1920, 1080), false)]);
        assert_eq!(v, vec![true, true]);
        // 부분 겹침은 유지
        let v = cull_covered(&[(r(0, 0, 1920, 1080), true), (r(0, 0, 900, 1080), true)]);
        assert_eq!(v, vec![true, true]);
        // 3겹: 최상 불투명이 아래 둘 다 덮음
        let v = cull_covered(&[
            (r(0, 0, 100, 100), true),
            (r(0, 0, 100, 100), false),
            (r(0, 0, 100, 100), true),
        ]);
        assert_eq!(v, vec![false, false, true]);
    }

    #[test]
    fn surface_extent_unions_tiles() {
        let vo = DeviceIntPoint::new(16384, 16384);
        let ts = DeviceIntSize::new(1024, 512);
        let tiles: std::collections::HashSet<_> = [(0, 0), (1, 0), (0, 1)].into_iter().collect();
        let e = surface_extent(&tiles, vo, ts).unwrap();
        assert_eq!(e, r(16384, 16384, 16384 + 2048, 16384 + 1024));
        assert!(surface_extent(&Default::default(), vo, ts).is_none());
    }
```

- [ ] **Step 4: 테스트 실행 시도**

Run: surfman — `Set-Location third_party\surfman; cargo test --features "chains sm-angle-default" --lib present_path` (지난 사이클 스탠드얼론 방식; 워크스페이스 밖이라 env 소싱 후). paint — `cargo test -p servo-paint --lib dcomp` 시도, 선재 feature-unification 실패 시 그 사실을 리포트에 기록(테스트는 유지, 실게이트는 Task 2의 mach+스모크).
Expected: surfman 테스트 PASS, paint는 PASS 또는 선재 이슈 문서화.

- [ ] **Step 5: Commit**

```
git add third_party/surfman/src/platform/windows/angle/surface.rs components/paint/dcomp_compositor.rs
git commit -m "dcomp: =surface 게이트 값 + 하이브리드 순수 로직(모드/coverage/컬링)

SERVO_COMPOSITOR_DCOMP=surface(구 가상서피스 전용, AMD A/B)를 truthy로
수용. FrameCoverage(타일 단위 보수 피복 - full일 때만 Present 규칙의
근거), cull_covered(불투명 완전 피복 하위 숨김), surface_extent.
COM 미접촉 - 동작 무변경."
```

---

### Task 2: 스왑체인 인프라 (동작 불변 리팩터 + 헬퍼)

**Files:**
- Modify: `components/paint/dcomp_compositor.rs`

**Interfaces:**
- Consumes: Task 1의 `storage_mode()` (아직 미사용 — Task 3에서 결선).
- Produces (Task 3이 사용):
  - `enum SurfaceStorage { Virtual { virtual_surface: ComOwned<IDCompositionVirtualSurface> }, SwapChain(SwapChainStorage) }`
  - `struct SwapChainStorage { swapchain: ComOwned<IDXGISwapChain1>, anchor: DeviceIntPoint, size: DeviceIntSize, coverage: FrameCoverage, frame_pbuffer: Option<FramePbuffer>, drawn_this_frame: bool, content_attached: bool, withheld_frames: u32, fallback_virtual: Option<ComOwned<IDCompositionVirtualSurface>> }`
  - `struct FramePbuffer { pbuffer: usize, texture: *mut ID3D11Texture2D }`
  - `DCompNativeCompositor::create_composition_swapchain(&self, size: DeviceIntSize, is_opaque: bool) -> Option<ComOwned<IDXGISwapChain1>>`
  - `SurfaceEntry`에 추가 필드: `storage: SurfaceStorage`(virtual_surface 필드 대체), `tiles: HashSet<(i32,i32)>`
  - `DCompNativeCompositor`에 추가 필드: `d3d11_device: *mut ID3D11Device`(비소유), `dxgi_factory: Option<ComOwned<IDXGIFactory2>>`

- [ ] **Step 1: imports + 팩토리 확보**

imports에 추가: `winapi::shared::dxgi::{IDXGIAdapter, DXGI_SWAP_EFFECT_FLIP_DISCARD}`, `winapi::shared::dxgi1_2::{IDXGIFactory2, IDXGISwapChain1, DXGI_SWAP_CHAIN_DESC1, DXGI_SCALING_STRETCH}`, `winapi::shared::dxgitype::DXGI_SAMPLE_DESC`, `winapi::shared::dxgiformat` 기존, `winapi::um::d3d11::D3D11_BIND_RENDER_TARGET`은 불필요(BufferUsage는 DXGI_USAGE_RENDER_TARGET_OUTPUT — `winapi::shared::dxgitype::DXGI_USAGE_RENDER_TARGET_OUTPUT`).

`maybe_create`의 성공 경로에서 (dxgi ComOwned를 이미 갖고 있음):

```rust
        // 스왑체인 생성용 팩토리: dxgi 디바이스 → 어댑터 → 부모 팩토리(IDXGIFactory2).
        // 실패해도 컴포지터는 성립(하이브리드 승격만 불가 → Virtual 유지) — None 허용.
        let dxgi_factory = {
            let mut adapter_raw: *mut IDXGIAdapter = ptr::null_mut();
            let hr = (*dxgi.as_ptr()).GetAdapter(&mut adapter_raw);
            if hr < 0 || adapter_raw.is_null() {
                warn!("[dcomp-native] GetAdapter failed (hr=0x{:08x}); swapchain promotion disabled", hr as u32);
                None
            } else {
                let adapter = ComOwned::from_raw(adapter_raw);
                adapter.and_then(|adapter| {
                    let mut factory_raw: *mut IDXGIFactory2 = ptr::null_mut();
                    let hr = (*adapter.as_ptr()).GetParent(
                        &IDXGIFactory2::uuidof(),
                        &mut factory_raw as *mut _ as *mut _,
                    );
                    if hr < 0 || factory_raw.is_null() {
                        warn!("[dcomp-native] GetParent(IDXGIFactory2) failed (hr=0x{:08x}); swapchain promotion disabled", hr as u32);
                        None
                    } else {
                        ComOwned::from_raw(factory_raw)
                    }
                })
            }
        };
```

구조체에 `d3d11_device: d3d, dxgi_factory,` 저장(필드 추가). `d3d11_device`는 비소유(rendering_context가 수명 보유 — 기존 계약 주석 유지).

- [ ] **Step 2: SurfaceStorage enum 도입 (동작 불변)**

```rust
struct FramePbuffer {
    pbuffer: usize,
    /// GetBuffer가 AddRef해 돌려준 백버퍼 텍스처. 파기 시 Release.
    texture: *mut ID3D11Texture2D,
}

struct SwapChainStorage {
    swapchain: ComOwned<IDXGISwapChain1>,
    /// 가상공간에서 백버퍼 (0,0)이 대응하는 지점(= 승격 시점 extent.min).
    anchor: DeviceIntPoint,
    size: DeviceIntSize,
    coverage: FrameCoverage,
    frame_pbuffer: Option<FramePbuffer>,
    drawn_this_frame: bool,
    /// 첫 Present 후 visual.SetContent(swapchain)를 완료했는가.
    /// false인 동안 visual은 fallback_virtual(마지막 완전 화면)을 계속 표시한다.
    content_attached: bool,
    withheld_frames: u32,
    /// content_attached 전까지 유지되는 구 가상 서피스(글리치 없는 전환용).
    fallback_virtual: Option<ComOwned<IDCompositionVirtualSurface>>,
}

enum SurfaceStorage {
    Virtual {
        virtual_surface: ComOwned<IDCompositionVirtualSurface>,
    },
    SwapChain(SwapChainStorage),
}
```

`SurfaceEntry`를 `{ storage: SurfaceStorage, visual, virtual_offset, tile_size, is_opaque, tiles: HashSet<(i32,i32)>, promote_streak: u32 }`로 개편. 기존 코드의 `entry.virtual_surface` 사용처(bind/unbind/create_surface)는 `match &entry.storage { SurfaceStorage::Virtual { virtual_surface } => ..., SurfaceStorage::SwapChain(..) => (Task 3까지 unreachable — `debug_assert!(false)` 없이 warn 후 fail 반환) }` 패턴으로 치환. `create_tile`/`destroy_tile`을 부기로 변경:

```rust
    fn create_tile(&mut self, _device: &mut Device, id: NativeTileId) {
        if let Some(entry) = self.surfaces.get_mut(&id.surface_id) {
            entry.tiles.insert((id.x, id.y));
        }
    }

    fn destroy_tile(&mut self, _device: &mut Device, id: NativeTileId) {
        if let Some(entry) = self.surfaces.get_mut(&id.surface_id) {
            entry.tiles.remove(&(id.x, id.y));
        }
    }
```

- [ ] **Step 3: create_composition_swapchain 헬퍼** (impl DCompNativeCompositor)

```rust
    /// 컴포지션용 flip 스왑체인 생성. 실패 시 None(호출자는 Virtual 유지 폴백).
    /// FLIP_DISCARD + BufferCount 2: 이전 버퍼를 읽지 않는다 — 정확성은
    /// FrameCoverage의 full-coverage Present 규칙이 보장(계획 Global Constraints).
    fn create_composition_swapchain(
        &self,
        size: DeviceIntSize,
        is_opaque: bool,
    ) -> Option<ComOwned<IDXGISwapChain1>> {
        let factory = self.dxgi_factory.as_ref()?.as_ptr();
        if size.width <= 0 || size.height <= 0 {
            return None;
        }
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.width as u32,
            Height: size.height as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: 0,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH, // CreateSwapChainForComposition 필수값
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: if is_opaque { DXGI_ALPHA_MODE_IGNORE } else { DXGI_ALPHA_MODE_PREMULTIPLIED },
            Flags: 0,
        };
        // Safety: factory/디바이스는 살아있는 COM 포인터(생성 시 확보). out-param은 ComOwned로.
        unsafe {
            let mut sc_raw: *mut IDXGISwapChain1 = ptr::null_mut();
            let hr = (*factory).CreateSwapChainForComposition(
                self.d3d11_device as *mut IUnknown,
                &desc,
                ptr::null_mut(),
                &mut sc_raw,
            );
            if hr < 0 || sc_raw.is_null() {
                warn!("[dcomp-native] CreateSwapChainForComposition {}x{} failed (hr=0x{:08x})",
                    size.width, size.height, hr as u32);
                return None;
            }
            ComOwned::from_raw(sc_raw)
        }
    }
```

(이 시점에는 호출자가 없어 dead_code — `#[allow(dead_code)]`를 달고 Task 3에서 제거한다고 주석에 명시.)

- [ ] **Step 4: 빌드 + 3모드 무회귀 스모크**

Run: mach build --release (동기 폴링, EXIT 0) → 게이트 `=1`로 4분면 스모크(quad_anim.html — engaged 마커+TL red 픽셀+클린 종료; 리팩터가 기존 virtual 동작을 깨지 않았는지), `=surface`로 동일(동작 동일해야 함 — 아직 분기 없음), off로 마커 부재.
Expected: 3모드 전부 기존과 동일.

- [ ] **Step 5: Commit**

```
git add components/paint/dcomp_compositor.rs
git commit -m "dcomp: SurfaceStorage enum + 스왑체인 헬퍼 (동작 불변 리팩터)

가상 서피스를 storage enum으로 감싸고 타일 집합 부기, DXGI 팩토리
확보, CreateSwapChainForComposition 헬퍼(FLIP_DISCARD/BufferCount 2/
STRETCH/알파모드) 추가. 승격 결선은 다음 커밋 - 3모드 스모크 무회귀."
```

---

### Task 3: 하이브리드 배선 (승격 + coverage-Present + SetContent 전환)

**Files:**
- Modify: `components/paint/dcomp_compositor.rs`

**Interfaces:**
- Consumes: Task 1 `storage_mode`/`FrameCoverage`/`surface_extent`, Task 2 storage enum/헬퍼.
- Produces: 게이트 `=1`에서 전면 갱신 서피스의 스왑체인 렌더·Present 동작(Task 4·5의 전제). 진단 로그 `[dcomp-dbg] promote/present/withhold/content-swap`.

- [ ] **Step 1: bind의 스왑체인 분기**

`bind`에서 storage로 분기. Virtual 경로는 기존 그대로. SwapChain 경로:

```rust
            SurfaceStorage::SwapChain(sc) => {
                // 프레임 첫 bind에서 백버퍼 래핑(프레임 캐시). Present를 안 하면
                // GetBuffer(0)가 같은 버퍼이므로 미프레젠트 프레임 간 누적도 정확.
                if sc.frame_pbuffer.is_none() {
                    // Safety: swapchain은 살아있는 IDXGISwapChain1. GetBuffer는 AddRef된
                    // RENDER_TARGET 백버퍼를 돌려준다.
                    let texture = unsafe {
                        let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
                        let hr = (*sc.swapchain.as_ptr()).GetBuffer(
                            0,
                            &ID3D11Texture2D::uuidof(),
                            &mut tex as *mut _ as *mut _,
                        );
                        if hr < 0 || tex.is_null() {
                            warn!("[dcomp-native] GetBuffer(0) failed (hr=0x{:08x}); giving up tile", hr as u32);
                            return fail;
                        }
                        tex
                    };
                    let pbuffer = match self.rendering_context.create_render_pbuffer_from_d3d_texture(
                        texture as usize,
                        UntypedSize2D::new(sc.size.width, sc.size.height),
                    ) {
                        Some(p) => p,
                        None => {
                            warn!("[dcomp-native] swapchain pbuffer wrap failed; giving up tile");
                            // Safety: GetBuffer가 AddRef한 텍스처 반납.
                            unsafe { (*(texture as *mut IUnknown)).Release(); }
                            return fail;
                        },
                    };
                    sc.frame_pbuffer = Some(FramePbuffer { pbuffer, texture });
                }
                let fp = sc.frame_pbuffer.as_ref().expect("set above");
                if !self.rendering_context.make_render_pbuffer_current(fp.pbuffer) {
                    warn!("[dcomp-native] make current (swapchain) failed; giving up tile");
                    return fail;
                }
                sc.coverage.note_tile((id.x, id.y), dirty_rect, valid_rect);
                sc.drawn_this_frame = true;
                // 스왑체인 bind는 BoundTile을 만들지 않는다(EndDraw/파기 대상 없음).
                self.bound = None;
                let origin = DeviceIntPoint::new(
                    tile_rect.min.x - sc.anchor.x,
                    tile_rect.min.y - sc.anchor.y,
                );
                return NativeSurfaceInfo { origin, fbo_id: 0 };
            }
```

주의: 현재 `bind`는 서피스 borrow를 즉시 끝내는 구조 — 스왑체인 분기는 `get_mut`가 필요하므로 함수 서두를 `let Some(entry) = self.surfaces.get_mut(&id.surface_id)`로 바꾸고, rendering_context 호출은 `&self.rendering_context` clone된 Rc로(이미 필드) borrow 충돌 없이 가능(엔트리 borrow 지속 중 self의 다른 필드 접근이 필요하면 `let rc = self.rendering_context.clone();`를 서두에). unbind는 `self.bound == None`이면 no-op(기존 그대로 — 스왑체인 타일에 정확).

clippy `expect` 금지 주의 — `expect("set above")` 대신:

```rust
                let Some(fp) = sc.frame_pbuffer.as_ref() else { return fail; };
```

- [ ] **Step 2: valid_rect 사용** — `bind` 시그니처의 `_valid_rect`를 `valid_rect`로 활성화(coverage 인자).

- [ ] **Step 3: end_frame — 승격 판정 + Present + SetContent 전환**

기존 end_frame(Commit만)을 다음 구조로 확장 (컬링은 Task 4에서 이 함수에 합류):

```rust
    fn end_frame(&mut self, device: &mut Device) {
        // GL 커맨드를 D3D 큐에 확실히 제출한 뒤 Present(순서 보장).
        device.gl().flush();

        let mode = storage_mode();
        for (id, entry) in self.surfaces.iter_mut() {
            // --- 승격 판정 (Virtual → SwapChain): 이번 프레임이 전면 갱신이었는가 ---
            // 전면 갱신 = 이 프레임에 그려진 dirty가 전 타일의 valid를 덮음.
            // Virtual 경로에는 FrameCoverage가 없으므로 bind에서 기록한
            // frame_full_tiles(아래 Step 4의 per-entry 프레임 집계)를 쓴다.
            ...
        }
        ...
    }
```

전체 코드(승격 집계 필드 포함) — `SurfaceEntry`에 `frame_coverage: FrameCoverage`(Virtual에서도 bind가 note_tile — 승격 판정용)를 추가하고:

```rust
    fn end_frame(&mut self, device: &mut Device) {
        device.gl().flush();
        let mode = storage_mode();

        for (id, entry) in self.surfaces.iter_mut() {
            let frame_full = entry.frame_coverage.is_full(&entry.tiles);

            match &mut entry.storage {
                SurfaceStorage::Virtual { .. } => {
                    // 승격 상태머신: 연속 PROMOTE_STREAK회 전면 갱신이면 스왑체인 생성.
                    entry.promote_streak = if frame_full { entry.promote_streak + 1 } else { 0 };
                    if mode == StorageMode::Hybrid
                        && entry.promote_streak >= PROMOTE_STREAK
                        && self.dxgi_factory.is_some()
                    {
                        if let Some(extent) =
                            surface_extent(&entry.tiles, entry.virtual_offset, entry.tile_size)
                        {
                            // (borrow 사정상 실제 구현은 승격 대상 id를 모아 루프 밖에서 수행)
                            promote_requests.push((*id, extent));
                        }
                    }
                }
                SurfaceStorage::SwapChain(sc) => {
                    if sc.drawn_this_frame && sc.coverage.is_full(&entry.tiles) {
                        // Safety: 살아있는 스왑체인. SyncInterval 0 = 비블로킹(페이싱은 기존 유지).
                        let hr = unsafe { (*sc.swapchain.as_ptr()).Present(0, 0) };
                        if hr < 0 {
                            warn!("[dcomp-native] Present failed (hr=0x{:08x})", hr as u32);
                        } else {
                            sc.coverage.reset();
                            sc.withheld_frames = 0;
                            if !sc.content_attached {
                                // 첫 완전 프레젠트 → visual 콘텐츠를 스왑체인으로 전환(무글리치).
                                // Safety: visual/swapchain 살아있음.
                                let hr = unsafe {
                                    (*entry.visual.as_ptr())
                                        .SetContent(sc.swapchain.as_ptr() as *const IUnknown)
                                };
                                if hr >= 0 {
                                    sc.content_attached = true;
                                    sc.fallback_virtual = None; // 구 가상 서피스 해제
                                    if dcomp_debug() {
                                        log::info!("[dcomp-dbg] content-swap id={:?} -> swapchain", id);
                                    }
                                } else {
                                    warn!("[dcomp-native] SetContent(swapchain) failed (hr=0x{:08x})", hr as u32);
                                }
                            }
                        }
                    } else if sc.drawn_this_frame {
                        // 부분 갱신 → Present 보류(마지막 완전 화면 유지, 다음 프레임 누적).
                        sc.withheld_frames += 1;
                        if sc.withheld_frames == WITHHOLD_WARN_FRAMES {
                            warn!(
                                "[dcomp-native] surface {:?}: swapchain present withheld for {} frames \
                                 (partial dirty accumulating; display update delayed)",
                                id, sc.withheld_frames
                            );
                        }
                        if dcomp_debug() {
                            log::info!("[dcomp-dbg] withhold id={:?} covered={}/{}",
                                id, sc.coverage.covered_tiles.len(), entry.tiles.len());
                        }
                    }
                    // 프레임 pbuffer 파기(미프레젠트여도 다음 프레임 GetBuffer(0)=같은 버퍼).
                    if let Some(fp) = sc.frame_pbuffer.take() {
                        self.rendering_context.destroy_render_pbuffer(fp.pbuffer);
                        if !fp.texture.is_null() {
                            // Safety: GetBuffer가 AddRef한 백버퍼 텍스처 반납.
                            unsafe { (*(fp.texture as *mut IUnknown)).Release(); }
                        }
                    }
                    sc.drawn_this_frame = false;
                }
            }
            entry.frame_coverage.reset();
        }

        // 승격 실행(luty 밖): 스왑체인 생성 성공 시 storage 교체, visual 콘텐츠는
        // fallback_virtual(구 가상 서피스)로 유지 — 첫 완전 Present에서 전환.
        for (id, extent) in promote_requests { ... create_composition_swapchain(extent.size(), is_opaque)
            → SurfaceStorage::SwapChain(SwapChainStorage { swapchain, anchor: extent.min, size: extent.size(),
                coverage: FrameCoverage::default(), frame_pbuffer: None, drawn_this_frame: false,
                content_attached: false, withheld_frames: 0,
                fallback_virtual: Some(구 virtual_surface 이동) }) ;
            dcomp_debug → "[dcomp-dbg] promote id=.. extent=..x.." ... }

        // (Task 4가 여기에 컬링+AddVisual을 삽입)

        let Some(dcomp_device) = self.dcomp_device_ptr() else { return; };
        // Safety: 살아있는 IDCompositionDevice.
        let hr = unsafe { (*dcomp_device).Commit() };
        if hr < 0 {
            warn!("[dcomp-native] Commit failed (hr=0x{:08x})", hr as u32);
        }
    }
```

상수: `const PROMOTE_STREAK: u32 = 3;`, `const WITHHOLD_WARN_FRAMES: u32 = 60;`. borrow 관계(HashMap iter_mut 중 self 메서드 호출 불가)는 승격 요청을 `Vec<(NativeSurfaceId, DeviceIntRect)>`로 모아 루프 밖 처리로 푼다(코드 골격 위와 같음 — 구현자가 borrow에 맞게 완성하되 의미는 이 블록이 정본).

- [ ] **Step 4: bind(Virtual 경로)에도 승격 집계 추가** — 기존 Virtual bind 성공 경로 말미에 `entry.frame_coverage.note_tile((id.x, id.y), dirty_rect, valid_rect);` (get→get_mut 전환 포함). release_all/destroy_surface에서 SwapChain의 frame_pbuffer도 정리(파기 코드 재사용 — 공용 fn `release_frame_pbuffer(&rc, &mut SwapChainStorage)`로 추출).

- [ ] **Step 5: 빌드 + 승격 스모크**

Run: mach build --release → `SERVO_DCOMP_DEBUG=1` + `=1` 게이트로 quad_anim.html 스모크.
Expected: 로그에 `promote id=NativeSurfaceId(..)` (전면 갱신 슬라이스 승격), `content-swap`, 이후 프레임 Present 지속(withhold 없음); TL red 픽셀 정확; 리사이즈 후에도 정확(서피스 재생성 → 재승격); 클린 종료. `=surface` 게이트: promote 로그 0(구 경로). 45타일 -DComp 스모크: 콘텐츠 슬라이스 승격 + 픽셀.

- [ ] **Step 6: Commit**

```
git add components/paint/dcomp_compositor.rs
git commit -m "dcomp: 하이브리드 승격 결선 - 전면 갱신 서피스를 flip 스왑체인으로

연속 3프레임 전면 갱신(타일 단위 valid 피복 판정) 서피스를 스왑체인으로
승격. Present는 full coverage에서만(부분 dirty는 같은 백버퍼 누적+보류 -
잔상 불허/지연 허용, 스펙 §7 정제). visual 콘텐츠는 첫 완전 Present에서
SetContent 전환(무글리치, 그때까지 구 가상 서피스 표시). BeginDraw/
EndDraw 기구가 승격 서피스에서 소멸."
```

---

### Task 4: 레이어 컬링 (AddVisual 이연)

**Files:**
- Modify: `components/paint/dcomp_compositor.rs`

**Interfaces:**
- Consumes: Task 1 `cull_covered`/`cull_disabled`, Task 3의 end_frame 구조.
- Produces: 월에서 DWM 1레이어. 진단 로그 `[dcomp-dbg] cull`.

- [ ] **Step 1: add_surface에서 AddVisual 제거 → 기록**

`DCompNativeCompositor`에 `frame_surfaces: Vec<(NativeSurfaceId, DeviceIntRect /*device clip*/, bool /*opaque*/)>` 필드 추가. `begin_frame`에서 `self.frame_surfaces.clear();` (RemoveAllVisuals 유지). `add_surface`는 SetOffset/SetClip까지 기존대로 수행하되 `AddVisual` 블록을 삭제하고:

```rust
        self.frame_surfaces.push((id, clip_rect, entry.is_opaque));
```

- [ ] **Step 2: end_frame에 컬링+AddVisual 삽입** (Task 3 골격의 "(Task 4 삽입)" 지점, Commit 직전)

```rust
        // 레이어 컬링: 최상위 불투명 클립이 완전히 덮는 하위 visual을 트리에서 제외.
        // 진단: SERVO_DCOMP_NO_CULL로 끌 수 있다(요소 소실 의심 시 즉시 판별).
        let entries: Vec<(DeviceIntRect, bool)> = self
            .frame_surfaces
            .iter()
            .map(|(_, clip, opaque)| (*clip, *opaque))
            .collect();
        let visible = if cull_disabled() {
            vec![true; entries.len()]
        } else {
            cull_covered(&entries)
        };
        if let Some(root) = self.root_visual_ptr() {
            for (i, (id, _, _)) in self.frame_surfaces.iter().enumerate() {
                if !visible[i] {
                    if dcomp_debug() {
                        log::info!("[dcomp-dbg] cull id={:?} (covered by opaque above)", id);
                    }
                    continue;
                }
                let Some(entry) = self.surfaces.get(id) else { continue; };
                // Safety: visual/root 살아있음. 순서 = add_surface 순서(z 아래→위) 유지.
                let hr = unsafe { (*root).AddVisual(entry.visual.as_ptr(), TRUE, ptr::null()) };
                if hr < 0 {
                    warn!("[dcomp-native] AddVisual failed (hr=0x{:08x})", hr as u32);
                }
            }
        }
```

(borrow 사정으로 Task 3 루프와의 순서는: 승격/Present 루프 → 컬링+AddVisual → Commit. frame_surfaces는 Commit 후 유지해도 무방 — begin_frame이 clear.)

- [ ] **Step 3: 빌드 + 컬링 스모크**

Run: mach build → 45타일 `-DComp` + `SERVO_DCOMP_DEBUG=1` 단시간 런.
Expected: 로그에 `cull id=NativeSurfaceId(0)` (하위 전면 불투명 숨김), add된 visual = 1; 화면 픽셀 정상(45타일 표시); `SERVO_DCOMP_NO_CULL=1`로 cull 로그 0(스위치 동작); quad 페이지 픽셀 회귀 없음.

- [ ] **Step 4: Commit**

```
git add components/paint/dcomp_compositor.rs
git commit -m "dcomp: 가려진 불투명 레이어 컬링 (AddVisual을 end_frame으로 이연)

add_surface는 기록만 하고 end_frame에서 최상위 불투명 클립이 완전히
덮는 하위 visual을 제외 후 조립 - 월에서 DWM 1레이어(probe 동형).
SERVO_DCOMP_NO_CULL 진단 스위치."
```

---

### Task 5: 통합 검증 (기능 계단 + 3모드 + PresentMon + 오버레이)

**Files:**
- Create: `.superpowers/sdd/evidence/overlay_wall.html` (검증 페이지, 커밋 대상 아님)
- 필요 시 수정: Task 2-4 파일 (결함 근본수정 + 커밋)

**Interfaces:**
- Consumes: Task 4까지 전체. 기존 도구(run_video_wall_d3d11.ps1, PresentMon D:\PresentMon-2.3.1-x64.exe, CopyFromScreen 픽셀 기법, GRIDPERF perf 페이지).
- Produces: 스펙 §2 완료 기준 1~3의 판정 결과(Task 6 문서화 입력).

검증 항목 (각각 PASS/FAIL + 증거를 리포트에):

- [ ] **5-1 4분면 애니메이션** (게이트 `=1`): 승격 로그 + 4분면 픽셀 + 리사이즈(축소/확대 각 1회 — 서피스 재생성·재승격) + 클린 종료.
- [ ] **5-2 비디오 2×2 → 45타일** (`-DComp`): 45/45, lockstep ±1(프레임카운터), 블랙타일 0, 5분 메모리 플랫(WS 샘플링), perf 페이지 ?log=1 40초 fps avg/min/maxGap이 직전 기준선(§Task6: avg 61.5/drop 0)과 동급.
- [ ] **5-3 텍스트 오버레이**: overlay_wall.html 신설 —

```html
<!DOCTYPE html><html><head><meta charset="utf-8"><style>
html,body{margin:0;width:100%;height:100%;overflow:hidden;background:#000}
video{width:25vw;height:25vh;object-fit:fill;display:block;float:left}
#cap{position:fixed;left:0;bottom:8vh;width:100vw;text-align:center;
 font:bold 6vh sans-serif;color:rgba(255,255,255,.85);
 text-shadow:0 0 8px rgba(0,0,0,.9);pointer-events:none}
</style></head><body>
<script>for(let i=0;i<16;i++){const v=document.createElement('video');
v.src='../../tests/html/../BigBuckBunny_1080p30s.mp4';/* 구현자: 리포의 실제 테스트 영상 경로로 교체 */
v.muted=true;v.loop=true;v.autoplay=true;document.body.appendChild(v);}
</script>
<div id="cap">투명 자막 오버레이 검증 — TRANSPARENT CAPTION</div>
<script>function f(){requestAnimationFrame(f)}requestAnimationFrame(f)</script>
</body></html>
```

  (영상 경로는 tests/html의 기존 테스트 영상으로 구현 시 확정.) 검증: 자막이 비디오 위에 정상 표시(픽셀: 자막 영역 흰색 성분), 진단 로그에서 알파 슬라이스가 생기면 Virtual 유지(promote 없음)·컬링 미발동 확인 — 단, WR이 자막을 같은 슬라이스에 넣으면(§스펙 5.3 경우 1) 그 사실을 기록(둘 다 정상 케이스).
- [ ] **5-4 3모드 매트릭스**: off / `=surface` / `=1` — 각 45타일 스팟(마커·표시·fps), `=surface`는 promote 로그 0 + 기존 동작.
- [ ] **5-5 PresentMon**: 45타일 `=1`에서 PresentMon 캡처(관리자·포그라운드 함정) — servoshell의 Present 이벤트 존재 확인 + PresentMode 기록(개발기 기준선; MPO/DirectFlip 여부는 기록만). `=surface`에선 Present 이벤트 부재(대조).
- [ ] **5-6 해체/재시작**: `=1` 정상 종료 3회 + 45타일 재기동 — 크래시/AV/좀비 0.
- [ ] **5-7** 발견 결함은 근본수정 + 커밋 + 해당 항목 재검증. 결과를 리포트로.

---

### Task 6: 런처 -DCompSurface + 패키징 + 스펙 부기

**Files:**
- Modify: `etc/multigpu/run_video_wall_d3d11.ps1`
- Modify: `D:\ServoWallPackage\run_wall.ps1` (리포 밖 — 커밋 비대상)
- Modify: `docs/superpowers/specs/2026-07-14-dcomp-swapchain-content-design.md` (§10 구현결과 — Edit 도구)

- [ ] **Step 1: 런처 2종에 `-DCompSurface` 스위치** — `-DComp`와 조합: `-DComp -DCompSurface` → `SERVO_COMPOSITOR_DCOMP=surface`, `-DComp`만 → `=1`, 없음 → env 제거(기존 관례). `-DCompSurface` 단독은 경고 후 무시. 마커 검증: `-DComp` 시 기존 engaged 확인 유지 + `-DCompSurface`면 promote 로그가 없어야 함을 주석으로 문서화(자동 검증은 engaged만 — promote는 SERVO_DCOMP_DEBUG 전용이라 기본 로그에 없음).
- [ ] **Step 2: 스모크** — dev 런처 `-Cols 2 -Rows 2 -DComp` / `-DComp -DCompSurface` 각 1회: 마커 PASS + 표시 + 실사망.
- [ ] **Step 3: 스펙 §10 "구현 결과와 이탈" 추가** — 검증 수치(5-1~5-6), §7 정제(보류-누적, 강등 미구현 사유), FLIP_DISCARD 채택(이전 버퍼 미참조), 승격 임계(3연속)·보류 warn(60프레임) 상수, PresentMode 개발기 기준선.
- [ ] **Step 4: ServoWallPackage 재패키징** — exe 교체(타임스탬프 확인) + run_wall.ps1 + zip 재생성(Compress-Archive -Force — Remove-Item은 보호 경로라 금지) + 패키지 단독 스모크(`-DComp`, `-DComp -DCompSurface`). AMD 판독 가이드를 3중 A/B로 갱신: ①`-DComp` 없이 ②`-DComp -DCompSurface`(가상 서피스) ③`-DComp`(스왑체인) × 창 확대 + PresentMon PresentMode — ③이 ②보다 개선되면 대여/반납 기구가 진범, ③=②면 다른 원인(로그 첨부).
- [ ] **Step 5: Commit** (리포 파일: 런처 + 스펙)

```
git commit -m "dcomp: 런처 -DCompSurface(3중 A/B) + 스펙 구현결과 부기"
```

- [ ] **Step 6: 최종 whole-branch 리뷰(컨트롤러) → 사용자 푸시 게이트**

---

## Self-Review 결과

1. **스펙 커버리지**: §5.1 하이브리드/승격/강등→보류-누적 정제=Task 1·3(정제는 Global Constraints에 명시), §5.2 프레임 흐름=Task 3, 컬링=Task 4, §5.3 혼합 콘텐츠=Task 5-3, §5.4 리사이즈=서피스 재생성 경로(Task 3 스모크 리사이즈 + 5-1), §6 통합 지점=Task 1(surfman 1곳)·2·6, §7 폴백=Task 2(팩토리 실패)·3(생성 실패 Virtual 유지·보류), §8 리스크=프레임 pbuffer 캐시(Task 3)/컬링 스위치(Task 1·4)/flush(Task 3)/알파(5-3), §9 앵커 8건=본 계획 앵커 표에서 전부 해소(2번 이전 버퍼 읽기는 FLIP_DISCARD+누적 규칙으로 불필요화, 3번 redraw_on_invalidation 부적합 확정, 4번 Device::gl().flush 채택). 갭 없음.
2. **플레이스홀더**: Task 3 Step 3의 `...`/승격 실행 골격은 의미 규범을 코드 블록으로 제시하고 borrow 재배열만 구현자 재량으로 명시 — 미정의 참조 없음(모든 타입·상수 본 계획에 정의). Task 5-3 영상 경로는 구현 시 확정으로 명시.
3. **타입 일관성**: `FrameCoverage`(T1 정의↔T3 사용), `SurfaceStorage`/`SwapChainStorage`/`FramePbuffer`(T2↔T3), `cull_covered`/`cull_disabled`(T1↔T4), `storage_mode`(T1↔T3), `create_composition_swapchain`(T2↔T3), 상수 `PROMOTE_STREAK`/`WITHHOLD_WARN_FRAMES`(T3) 일치 확인.

# 타일별 출력 위상 정렬 (B1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 타일마다 DComp Commit 을 **자기 출력의 vblank 격자**에 맞춰 내보내, 각 타일의 갱신 박자를 규칙적으로 만든다.

**Architecture:** 이미 있는 출력 vblank 프로브 스레드를 상시로 승격해 `HMONITOR → 격자` 를 공유 상태에 저장한다. 타일은 기동 시 `HWND → HMONITOR → IDXGIOutput` 으로 자기 출력을 찾는다. 지금 즉시 실행되는 지연 커밋을, 그 타일 출력의 다음 목표 시각에 맞춰 **커밋 스케줄러 스레드**가 대신 건다. 렌더 패스는 지금처럼 하나로 두고 **커밋 시각만** 옮긴다.

**Tech Stack:** Rust, winapi 0.3(`dxgi`/`dwmapi`/`winuser`/`profileapi`), DirectComposition, Windows QPC. 빌드는 `cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release`.

**Spec:** `docs/superpowers/specs/2026-09-21-per-output-commit-phase-design.md`

## Global Constraints

- 저장소 `W:\servo_multigpu-tiled-wall`, 브랜치 `wall-animation-perf`. 다른 브랜치로 옮기지 않는다.
- **범위는 B1 뿐이다.** B2(타일별 샘플 시각)는 실기 판정 후 별건이다. `pump_paint_animation` 을 건드리지 않는다.
- **렌더 패스를 분해하지 않는다.** 프레임 배리어와 `-ParallelTiles` 구조는 그대로다. 커밋 시각만 옮긴다.
- **생산 스레드(비디오·스트림·WebGL)를 새로 기다리게 만들지 않는다.** `WaitForVBlank` 는 블록하므로 전용 프로브 스레드에만 둔다. 근거: `GstSystemClock::obtain()` 이 프로세스 싱글턴이라 45 개 파이프라인 sink 가 같은 객체를 기다렸고 그 비용이 디코딩과 맞먹었다(`-SinkPacing thread` 가 그 대응).
- **모든 폴백의 종착지는 "지금 동작"(즉시 커밋)이다.** 이 기능이 실패해도 벽은 오늘과 같이 돈다.
- 새 pref 이름은 `gfx_present_align_per_output_pct`, 기본 `-1`(끔), 유효 범위 `0..=99`. 런처 스위치는 `-PerOutputAlign`.
- 새 로그 줄 이름은 `OUTCOMMIT`. 게이트는 기존 `SERVO_DCOMP_BIND_PROF`(`-DcompBindProf`).
- **PowerShell 에서 native exe(`cargo`/`git`) 뒤에 `2>&1`·`2>$null` 을 붙이지 않는다.** PS 5.1 이 stderr 를 `NativeCommandError` 로 감싸 성공한 빌드를 실패로 보고한다.
- **`#[repr(packed)]` 구조체(`DWM_TIMING_INFO`, `DXGI_OUTPUT_DESC`)의 필드를 빌리지 않는다.** 값으로 복사해 쓴다(`let x = info.field;`).
- 커밋 메시지 끝에 `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>` 를 붙인다.
- 절대 `git add -A` / `git add .` 를 쓰지 않는다. 다음 파일은 절대 스테이지하지 않는다: `Cargo.lock`, `etc/multigpu/config/wall_layout.example_1x1.json`, `tests/html/multigpu_standard_video_extended_probe.html`, `tests/html/multigpu_standard_video_rtsp_probe.html`.

## File Structure

| 파일 | 책임 | 변경 |
|---|---|---|
| `components/paint/output_grid.rs` | **신규.** 출력 열거, `HMONITOR → 격자` 저장, 프로브 스레드, `HWND → 출력` 해석 | Create |
| `components/paint/commit_scheduler.rs` | **신규.** 마감 큐 하나를 가진 스레드. `(device_ptr, deadline)` 을 받아 때가 되면 `commit_device_ptr` 호출. 디바이스별 뮤텍스 보유 | Create |
| `components/paint/lib.rs` | 두 모듈 선언 | Modify |
| `components/paint/dcomp_compositor.rs` | 기존 `output_vblank_probe_loop` 를 `output_grid` 로 이사. `end_frame` 진입 시 디바이스 뮤텍스 획득. `OUTCOMMIT` 로그 | Modify |
| `components/paint/paint.rs` | `flush_deferred_dcomp_commits` 가 즉시 커밋 대신 스케줄러에 넘김 | Modify |
| `components/config/prefs.rs` | `gfx_present_align_per_output_pct` | Modify |
| `etc/multigpu/run_wall_dist.ps1` | `-PerOutputAlign` | Modify |

새 파일 둘로 나눈 이유: 격자 수집(윈도우 API·스레드·열거)과 커밋 시각 결정(큐·뮤텍스)은 바뀌는 이유가 다르다. `dcomp_compositor.rs` 는 이미 4,500 행이라 더 키우지 않는다.

---

### Task 1: 출력 격자 모듈 — 열거·프로브·조회

**Files:**
- Create: `components/paint/output_grid.rs`
- Modify: `components/paint/lib.rs`
- Modify: `components/paint/dcomp_compositor.rs` (기존 프로브 제거)

**Interfaces:**
- Produces:
  - `pub(crate) fn grid_for_monitor(monitor: usize) -> Option<OutputGrid>`
  - `pub(crate) struct OutputGrid { pub vblank_qpc: u64, pub period_qpc: u64, pub sampled_qpc: u64 }`
  - `pub(crate) fn monitor_for_hwnd(hwnd: usize) -> Option<usize>`
  - `pub(crate) fn start_probe()`
  - `pub(crate) fn qpc_now() -> Option<u64>` / `pub(crate) fn qpc_frequency() -> Option<u64>`

- [ ] **Step 1: 새 모듈 파일을 만든다**

`components/paint/output_grid.rs`:

```rust
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 출력(모니터)마다의 vblank 격자.
//!
//! ★왜 이 모듈이 있나★ — `DwmGetCompositionTimingInfo` 는 `hWnd` 에 NULL 만 받는다. 즉
//! 데스크톱(주 모니터) 격자 하나만 준다. 4-GPU 벽에서 네 모니터의 vblank 는 실측으로
//! 주기의 0.67 에 흩어져 있고(log_ani_debug_02/02: 기준 대비 +1.17 / +6.57 / −4.63ms),
//! 좋은 자리를 5ms 로 넉넉히 잡아도 네 창의 교집합이 공집합이다. 하나의 클럭으로는 넷을
//! 만족시킬 수 없으므로 출력마다 따로 안다.
//!
//! ★`WaitForVBlank` 는 블록한다.★ 생산 스레드나 메인에서 부르면 공유 객체에 줄 서는
//! 회귀가 된다 — `GstSystemClock::obtain()` 이 프로세스 싱글턴이라 45 개 파이프라인의
//! sink 가 매 프레임 같은 객체를 기다렸고 그 비용이 디코딩과 맞먹었다(`-SinkPacing
//! thread` 가 그 대응). 그래서 **전용 스레드 하나**가 돌며 격자를 갱신하고, 나머지는
//! 전부 `grid_for_monitor` 로 **읽기만** 한다.

use std::collections::HashMap;
use std::ptr;
use std::sync::{Mutex, OnceLock};

use log::warn;
use winapi::Interface;
use winapi::shared::dxgi::{
    CreateDXGIFactory1, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
};
use winapi::um::profileapi::{QueryPerformanceCounter, QueryPerformanceFrequency};
use winapi::um::winuser::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};

/// 한 출력의 vblank 격자.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputGrid {
    /// 마지막으로 관측한 vblank 의 QPC.
    pub vblank_qpc: u64,
    /// 한 주기의 QPC 틱.
    pub period_qpc: u64,
    /// 그 관측을 뜬 QPC. ★격자가 얼마나 묵었는지 호출자가 판단해야 한다★ — 프로브가
    /// 정체하면 옛 격자로 스케줄하는 것보다 폴백이 낫다.
    pub sampled_qpc: u64,
}

static GRID: Mutex<Option<HashMap<usize, OutputGrid>>> = Mutex::new(None);

/// QPC 주파수는 부팅 중 고정이므로 한 번만 읽는다.
pub(crate) fn qpc_frequency() -> Option<u64> {
    static FREQ: OnceLock<Option<u64>> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut freq: i64 = 0;
        // Safety: 순수 out-param.
        if unsafe { QueryPerformanceFrequency(&mut freq as *mut i64 as *mut _) } != 0 && freq > 0 {
            Some(freq as u64)
        } else {
            None
        }
    })
}

pub(crate) fn qpc_now() -> Option<u64> {
    let mut now: i64 = 0;
    // Safety: 순수 out-param.
    if unsafe { QueryPerformanceCounter(&mut now as *mut i64 as *mut _) } != 0 && now >= 0 {
        Some(now as u64)
    } else {
        None
    }
}

/// 이 창이 올라가 있는 모니터. 매핑은 디스플레이 구성이 바뀌면 썩으므로 호출자가
/// 주기적으로 다시 묻는다(비용은 API 한 번이다).
pub(crate) fn monitor_for_hwnd(hwnd: usize) -> Option<usize> {
    if hwnd == 0 {
        return None;
    }
    // Safety: HWND 는 셸이 만든 살아 있는 창. 실패 시 NULL 을 돌려준다.
    let monitor = unsafe { MonitorFromWindow(hwnd as *mut _, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        None
    } else {
        Some(monitor as usize)
    }
}

pub(crate) fn grid_for_monitor(monitor: usize) -> Option<OutputGrid> {
    GRID.lock().ok()?.as_ref()?.get(&monitor).copied()
}

pub(crate) fn start_probe() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Err(error) = std::thread::Builder::new()
            .name(String::from("OutputVBlankProbe"))
            .spawn(probe_loop)
        {
            warn!("[outgrid] 프로브 스레드를 띄우지 못했다: {error}; 전 타일 폴백");
        }
    });
}

/// 열거된 출력 하나.
struct Output {
    name: String,
    monitor: usize,
    output: *mut IDXGIOutput,
}

// Safety: 이 포인터는 프로브 스레드에서 만들어 그 스레드에서만 쓰이고 그 스레드에서 해제된다.
// `Vec<Output>` 이 스레드 경계를 넘지 않으므로 `Send` 가 필요 없다 — 이 구조체는 프로브
// 루프의 지역 값으로만 존재한다.

fn probe_loop() {
    // Safety: 전부 COM 생성/열거. 포인터는 이 스레드 안에서만 살고, 루프가 끝나면 해제한다.
    let outputs = unsafe { enumerate_outputs() };
    if outputs.len() < 1 {
        warn!("[outgrid] 출력을 하나도 찾지 못했다; 전 타일 폴백");
        return;
    }
    warn!(
        "[outgrid] 출력 {} 개를 잰다: {}",
        outputs.len(),
        outputs
            .iter()
            .map(|o| o.name.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let Some(freq) = qpc_frequency() else {
        warn!("[outgrid] QPC 주파수를 읽지 못했다; 전 타일 폴백");
        return;
    };
    // 주기는 DXGI 가 직접 주지 않으므로 vblank 간격에서 추정한다. 첫 바퀴에는 직전 관측이
    // 없으므로 60Hz 를 가정하고, 두 번째 바퀴부터 실측으로 대체된다.
    let mut previous: HashMap<usize, u64> = HashMap::new();
    let assumed_period = freq / 60;

    loop {
        for out in &outputs {
            // Safety: 살아 있는 IDXGIOutput. 이 호출은 블록한다 -- 그래서 전용 스레드다.
            let hr = unsafe { (*out.output).WaitForVBlank() };
            if hr < 0 {
                continue;
            }
            let Some(now) = qpc_now() else { continue };
            let period = match previous.insert(out.monitor, now) {
                // 연속 두 관측 사이에는 **출력 수만큼의 vblank** 가 들어간다(한 바퀴 도는
                // 동안 다른 출력들을 기다렸기 때문). 관측 간격을 그 수로 나눠 주기를 얻는다.
                Some(before) if now > before => {
                    let span = now - before;
                    let ticks = outputs.len() as u64;
                    let estimate = span / ticks.max(1);
                    // 말도 안 되는 값은 버린다(모드 전환·정체). 30~240Hz 밖이면 가정값.
                    if estimate > freq / 240 && estimate < freq / 30 {
                        estimate
                    } else {
                        assumed_period
                    }
                },
                _ => assumed_period,
            };
            if let Ok(mut guard) = GRID.lock() {
                guard
                    .get_or_insert_with(HashMap::new)
                    .insert(out.monitor, OutputGrid {
                        vblank_qpc: now,
                        period_qpc: period,
                        sampled_qpc: now,
                    });
            }
        }
    }
}

/// Safety: 호출자는 프로브 스레드여야 한다. 돌려준 포인터는 그 스레드에서만 쓰인다.
unsafe fn enumerate_outputs() -> Vec<Output> {
    let mut found = Vec::new();
    let mut factory: *mut IDXGIFactory1 = ptr::null_mut();
    if CreateDXGIFactory1(&IDXGIFactory1::uuidof(), &mut factory as *mut _ as *mut _) < 0
        || factory.is_null()
    {
        warn!("[outgrid] CreateDXGIFactory1 실패");
        return found;
    }
    for ai in 0..16u32 {
        let mut adapter: *mut IDXGIAdapter1 = ptr::null_mut();
        if (*factory).EnumAdapters1(ai, &mut adapter) < 0 || adapter.is_null() {
            break;
        }
        for oi in 0..8u32 {
            let mut output: *mut IDXGIOutput = ptr::null_mut();
            if (*adapter).EnumOutputs(oi, &mut output) < 0 || output.is_null() {
                break;
            }
            let mut desc: DXGI_OUTPUT_DESC = std::mem::zeroed();
            if (*output).GetDesc(&mut desc) < 0 {
                (*output).Release();
                continue;
            }
            // `#[repr(packed)]` 이라 필드를 빌릴 수 없다. 값으로 복사한다.
            let monitor = desc.Monitor as usize;
            let name_end = desc
                .DeviceName
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.DeviceName.len());
            let name = String::from_utf16_lossy(&desc.DeviceName[..name_end]);
            found.push(Output {
                name,
                monitor,
                output,
            });
        }
        (*adapter).Release();
    }
    (*factory).Release();
    found
}
```

- [ ] **Step 2: 모듈을 선언한다**

`components/paint/lib.rs` 의 `mod dcomp_compositor;` 줄 **바로 아래**에 추가:

```rust
#[cfg(windows)]
mod output_grid;
```

- [ ] **Step 3: 기존 진단 프로브를 제거한다**

`components/paint/dcomp_compositor.rs` 에서 다음 셋을 지운다. 새 모듈이 같은 일을 하며,
둘이 같이 돌면 출력마다 두 스레드가 `WaitForVBlank` 를 기다린다.

- `fn start_output_vblank_probe()` 전체 (`#[cfg(windows)]` 속성 포함)
- `fn output_vblank_probe_loop()` 전체
- `note_dwm_phase()` 안의 `start_output_vblank_probe();` 호출 두 줄(주석 포함)

그 자리에 `note_dwm_phase()` 안에서 새 프로브를 띄운다:

```rust
    // 출력별 격자 프로브를 여기서 한 번만 띄운다 -- 같은 게이트, 같은 목적이다.
    #[cfg(windows)]
    crate::output_grid::start_probe();
```

`OUTPHASE` 로그는 여기서 일단 사라졌다가 **최종 리뷰에서 되살렸다(Ruling 22).** 이 단계의
판단("출력 간 어긋남은 `OUTCOMMIT` 이 대신 보여 준다")은 틀렸다 — pref 가 꺼진 기준선 런
에서는 스케줄러가 돌지 않아 `OUTCOMMIT` 이 한 줄도 나오지 않고, 그러면 이 설계 전체가 딛고
선 출력 간 vblank 확산을 볼 수 없다. 게다가 `OUTCOMMIT` 의 위상은 마감을 정한 바로 그
격자로 접은 값이라 격자가 틀려도 목표치를 가리킨다(순환). `OUTPHASE` 는 그 격자에 대한
**독립적인 검산**이고, Task 6 의 기준 0 이 읽는 줄이다.

- [ ] **Step 4: 빌드해 컴파일을 확인한다**

```
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release --message-format short
```

기대: 성공. `dcomp_compositor.rs` 에 `start_output_vblank_probe`/`output_vblank_probe_loop`
미사용 경고가 남아 있으면 Step 3 의 제거가 덜 된 것이다.

**★이 파일 트리에는 이 코드를 검증하는 단위 테스트를 둘 수 없다.★** 전부 Windows 디스플레이
하드웨어에 의존하는 COM 호출이고, 이 저장소에 그 층의 테스트 하니스가 없다. 검증은
Task 6 의 실기 로그(`[outgrid] 출력 N 개를 잰다`)가 한다. 그 사실을 여기 적어 두는 이유는,
"테스트가 없다" 를 누락이 아니라 **판단**으로 남기기 위해서다.

- [ ] **Step 5: 커밋**

```bash
git add components/paint/output_grid.rs components/paint/lib.rs components/paint/dcomp_compositor.rs
git commit -m "$(cat <<'EOF'
feat(paint): 출력별 vblank 격자 모듈

DwmGetCompositionTimingInfo 는 hWnd 에 NULL 만 받아 데스크톱 격자 하나만 준다. 네 모니터의
vblank 는 실측으로 주기의 0.67 에 흩어져 있고(log_ani_debug_02/02) 좋은 자리를 5ms 로 잡아도
네 창의 교집합이 공집합이라, 하나의 클럭으로는 넷을 만족시킬 수 없다. 출력마다 따로 안다.

전용 스레드 하나가 WaitForVBlank 로 출력을 돌며 격자를 갱신하고 나머지는 읽기만 한다.
그 API 는 블록하므로 생산 스레드나 메인에 두면 공유 객체에 줄 서는 회귀가 된다 --
GstSystemClock::obtain() 이 프로세스 싱글턴이라 45 개 파이프라인 sink 가 같은 객체를
기다렸고 그 비용이 디코딩과 맞먹었다(-SinkPacing thread 가 그 대응).

주기는 DXGI 가 주지 않으므로 vblank 관측 간격을 출력 수로 나눠 추정하고, 30~240Hz 밖이면
가정값(60Hz)으로 되돌린다. 격자에 표본 시각을 함께 담아 호출자가 묵은 격자를 버릴 수 있게 한다.

dcomp_compositor 의 진단용 OUTPHASE 프로브는 제거한다 -- 같은 일을 하며, 둘이 같이 돌면
출력마다 두 스레드가 vblank 를 기다린다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: 커밋 스케줄러 — 마감 큐와 디바이스 뮤텍스

**Files:**
- Create: `components/paint/commit_scheduler.rs`
- Modify: `components/paint/lib.rs`

**Interfaces:**
- Consumes: `crate::output_grid::{qpc_now, qpc_frequency}` (Task 1)
- Produces:
  - `pub(crate) fn schedule(device: usize, deadline_qpc: u64)`
  - `pub(crate) fn device_guard(device: usize) -> DeviceGuard`
  - `pub(crate) struct DeviceGuard` (Drop 시 해제)
  - `pub(crate) fn take_stats() -> SchedulerStats`
  - `pub(crate) struct SchedulerStats { pub scheduled: u64, pub slip_us_max: u64, pub slip_us_sum: u64, pub lock_wait_us_max: u64, pub lock_wait_us_sum: u64 }`

- [ ] **Step 1: 스케줄러 파일을 만든다**

`components/paint/commit_scheduler.rs`:

```rust
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 지연된 DComp Commit 을 **타일 출력의 목표 시각**에 내보내는 스케줄러.
//!
//! ★왜 스레드가 필요한가★ — 타일마다 목표 시각이 다르고(네 모니터가 주기의 0.67 에
//! 흩어져 있다) 그 차이가 최대 11.2ms 다. 메인에서 기다릴 수 없고 기다려서도 안 된다.
//!
//! ★이 설계가 새로 만드는 위험★ — 지금의 `gfx_dcomp_parallel_commit` 은
//! `std::thread::scope` 로 join 을 보장해 "돌아올 때 그 디바이스를 만지는 워커가 없다" 가
//! 성립한다. 스케줄러는 **나중에** 커밋하므로 그 보장이 사라진다: 스케줄러가 디바이스 D 를
//! 커밋하는 동안 그 painter 가 다음 프레임의 `end_frame` 에 들어갈 수 있다. 정상 부하에서는
//! 겹치지 않지만(최대 지연 11.2ms < 주기 16.67ms) **"정상 부하에서는" 은 보장이 아니다.**
//! 그래서 디바이스마다 뮤텍스를 두고, 양쪽이 그것을 잡는다. 커밋이 0.02ms 라 경합은 드물고
//! 짧아야 하며, 그 가정은 `lock_wait_us` 계수가 지켜본다.
//!
//! 이 뮤텍스는 생산 스레드와 무관하다 -- DComp 디바이스를 만지는 둘(스케줄러, 그 타일의
//! painter) 사이에서만 걸린다. 비디오·스트림 생산자는 이 경로에 없다.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use log::warn;

use crate::output_grid::{qpc_frequency, qpc_now};

#[derive(Default, Clone, Copy)]
pub(crate) struct SchedulerStats {
    /// 스케줄에 올린 커밋 수.
    pub scheduled: u64,
    /// 마감보다 늦게 커밋한 시간. 크면 스케줄러가 병목이다.
    pub slip_us_max: u64,
    pub slip_us_sum: u64,
    /// 디바이스 뮤텍스 대기. ★0 에 가까워야 한다는 위 가정의 검산이다.★
    pub lock_wait_us_max: u64,
    pub lock_wait_us_sum: u64,
}

struct Shared {
    /// (마감 QPC, 디바이스 포인터). 작은 큐라 정렬 없이 최소값을 훑는다 -- 타일 수만큼이다.
    queue: Mutex<Vec<(u64, usize)>>,
    condvar: Condvar,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
static DEVICE_LOCKS: Mutex<Option<HashMap<usize, Arc<Mutex<()>>>>> = Mutex::new(None);

static SCHEDULED: AtomicU64 = AtomicU64::new(0);
static SLIP_MAX: AtomicU64 = AtomicU64::new(0);
static SLIP_SUM: AtomicU64 = AtomicU64::new(0);
static LOCK_MAX: AtomicU64 = AtomicU64::new(0);
static LOCK_SUM: AtomicU64 = AtomicU64::new(0);

fn device_lock(device: usize) -> Arc<Mutex<()>> {
    let mut guard = DEVICE_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashMap::new)
        .entry(device)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// 디바이스를 만지는 동안 들고 있어야 하는 것. `end_frame` 과 스케줄러가 같이 쓴다.
pub(crate) struct DeviceGuard {
    _inner: Arc<Mutex<()>>,
    _held: Option<std::sync::MutexGuard<'static, ()>>,
}

/// ★`end_frame` 진입 시 부른다.★ 반환값을 프레임이 끝날 때까지 들고 있는다.
pub(crate) fn device_guard(device: usize) -> DeviceGuard {
    let lock = device_lock(device);
    let start = qpc_now();
    // Safety: `Arc` 를 함께 들고 있으므로 뮤텍스는 가드보다 오래 산다. `transmute` 로
    // 수명을 늘리는 대신 `Arc` 를 구조체에 담아 그 사실을 타입으로 보장한다.
    let held = {
        let cloned = lock.clone();
        let guard = cloned.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: `_inner` 가 같은 `Arc` 를 들고 있어 가드가 살아 있는 동안 뮤텍스가 해제되지
        // 않는다. 수명만 늘리고 대상은 그대로다.
        unsafe {
            std::mem::transmute::<std::sync::MutexGuard<'_, ()>, std::sync::MutexGuard<'static, ()>>(
                guard,
            )
        }
    };
    if let (Some(start), Some(now)) = (start, qpc_now()) {
        record_lock_wait(now.saturating_sub(start));
    }
    DeviceGuard {
        _inner: lock,
        _held: Some(held),
    }
}

fn record_lock_wait(ticks: u64) {
    let Some(freq) = qpc_frequency() else { return };
    let us = ticks.saturating_mul(1_000_000) / freq.max(1);
    LOCK_SUM.fetch_add(us, Ordering::Relaxed);
    LOCK_MAX.fetch_max(us, Ordering::Relaxed);
}

/// 이 디바이스의 커밋을 `deadline_qpc` 에 건다. 마감이 이미 지났으면 즉시 커밋된다.
pub(crate) fn schedule(device: usize, deadline_qpc: u64) {
    let shared = SHARED.get_or_init(|| {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Vec::new()),
            condvar: Condvar::new(),
        });
        let worker = shared.clone();
        if let Err(error) = std::thread::Builder::new()
            .name(String::from("DcompCommitScheduler"))
            .spawn(move || scheduler_loop(&worker))
        {
            warn!("[commitsched] 스레드를 띄우지 못했다: {error}");
        }
        shared
    });
    {
        let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        // 같은 디바이스가 이미 걸려 있으면 **덮어쓴다.** 밀린 커밋을 쌓으면 한 주기에 여러
        // 개가 나가고, 그것이 정확히 없애려는 현상이다.
        if let Some(slot) = queue.iter_mut().find(|(_, d)| *d == device) {
            slot.0 = deadline_qpc;
        } else {
            queue.push((deadline_qpc, device));
        }
    }
    SCHEDULED.fetch_add(1, Ordering::Relaxed);
    shared.condvar.notify_one();
}

pub(crate) fn take_stats() -> SchedulerStats {
    SchedulerStats {
        scheduled: SCHEDULED.swap(0, Ordering::Relaxed),
        slip_us_max: SLIP_MAX.swap(0, Ordering::Relaxed),
        slip_us_sum: SLIP_SUM.swap(0, Ordering::Relaxed),
        lock_wait_us_max: LOCK_MAX.swap(0, Ordering::Relaxed),
        lock_wait_us_sum: LOCK_SUM.swap(0, Ordering::Relaxed),
    }
}

fn scheduler_loop(shared: &Arc<Shared>) {
    let Some(freq) = qpc_frequency() else {
        warn!("[commitsched] QPC 주파수를 읽지 못했다; 스케줄러를 멈춘다");
        return;
    };
    loop {
        // 때가 된 것을 전부 꺼낸다.
        let due: Vec<(u64, usize)> = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            let Some(now) = qpc_now() else {
                // 시계를 못 읽으면 큐를 비워 폴백한다 -- 붙들고 있으면 화면이 멈춘다.
                queue.drain(..).collect()
            };
            let mut ready = Vec::new();
            queue.retain(|&(deadline, device)| {
                if deadline <= now {
                    ready.push((deadline, device));
                    false
                } else {
                    true
                }
            });
            if ready.is_empty() {
                // 가장 이른 마감까지 잔다. 큐가 비면 알림을 기다린다.
                let wait = queue.iter().map(|&(d, _)| d).min().map(|d| {
                    let ticks = d.saturating_sub(now);
                    std::time::Duration::from_secs_f64(ticks as f64 / freq as f64)
                });
                match wait {
                    Some(duration) => {
                        let _ = shared.condvar.wait_timeout(queue, duration);
                    },
                    None => {
                        let _ = shared.condvar.wait(queue);
                    },
                }
                continue;
            }
            ready
        };

        for (deadline, device) in due {
            if let Some(now) = qpc_now() {
                let slip = now.saturating_sub(deadline);
                let us = slip.saturating_mul(1_000_000) / freq.max(1);
                SLIP_SUM.fetch_add(us, Ordering::Relaxed);
                SLIP_MAX.fetch_max(us, Ordering::Relaxed);
            }
            let _guard = device_guard(device);
            crate::dcomp_compositor::commit_device_ptr(device);
        }
    }
}
```

- [ ] **Step 2: 모듈을 선언한다**

`components/paint/lib.rs` 의 `mod output_grid;` 아래에 추가:

```rust
#[cfg(windows)]
mod commit_scheduler;
```

- [ ] **Step 3: 큐 덮어쓰기 규칙을 단위 테스트로 고정한다**

★이 파일에서 **테스트할 수 있는 유일한 논리**가 큐 규칙이다.★ 나머지는 스레드와 COM 이다.
큐 규칙은 순수 로직이므로 갈라내어 테스트한다. `commit_scheduler.rs` 맨 아래에 추가:

```rust
/// 큐에 마감을 넣거나 갱신한다. ★같은 디바이스는 덮어쓴다★ -- 밀린 커밋을 쌓으면 한
/// 주기에 여러 개가 나가고, 그것이 정확히 없애려는 현상이다. 스레드·COM 없이 테스트할 수
/// 있도록 `schedule` 에서 이 규칙만 갈라냈다.
fn upsert(queue: &mut Vec<(u64, usize)>, device: usize, deadline: u64) {
    if let Some(slot) = queue.iter_mut().find(|(_, d)| *d == device) {
        slot.0 = deadline;
    } else {
        queue.push((deadline, device));
    }
}

#[cfg(test)]
mod tests {
    use super::upsert;

    #[test]
    fn a_second_schedule_for_the_same_device_replaces_the_first() {
        let mut queue = Vec::new();
        upsert(&mut queue, 0xAA, 100);
        upsert(&mut queue, 0xAA, 250);
        assert_eq!(queue, vec![(250, 0xAA)], "같은 디바이스는 쌓이지 않고 덮어써야 한다");
    }

    #[test]
    fn different_devices_each_keep_their_own_deadline() {
        let mut queue = Vec::new();
        upsert(&mut queue, 0xAA, 100);
        upsert(&mut queue, 0xBB, 250);
        upsert(&mut queue, 0xAA, 300);
        queue.sort();
        assert_eq!(queue, vec![(250, 0xBB), (300, 0xAA)]);
    }
}
```

그리고 `schedule` 안의 덮어쓰기 블록을 이 함수 호출로 바꾼다:

```rust
    {
        let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        upsert(&mut queue, device, deadline_qpc);
    }
```

- [ ] **Step 4: 테스트가 실패하는 것을 확인한다**

Step 3 을 넣기 **전에** 이 단계를 먼저 밟았다면 `upsert` 미정의로 실패했을 것이다. 순서를
지키지 못했다면 지금 `upsert` 를 잠시 주석 처리해 실패를 확인하고 되돌린다.

```
cargo test -p servo-paint --lib commit_scheduler --release
```

기대(주석 처리 상태): 컴파일 실패 `cannot find function upsert`.

- [ ] **Step 5: 테스트가 통과하는 것을 확인한다**

```
cargo test -p servo-paint --lib commit_scheduler --release
```

기대: `a_second_schedule_for_the_same_device_replaces_the_first` 와
`different_devices_each_keep_their_own_deadline` 둘 다 PASS.

★`servo-paint` 의 다른 테스트가 이미 실패하고 있을 수 있다.★ 그 경우 **이 둘만** 통과하면
된다. 기존 실패를 고치는 것은 이 계획의 범위가 아니다.

- [ ] **Step 6: 빌드**

```
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release --message-format short
```

기대: 성공. `schedule`/`device_guard`/`take_stats` 는 아직 호출처가 없어 `dead_code` 경고가
난다 -- Task 3·4·5 에서 없어진다. 경고를 없애려고 `#[allow(dead_code)]` 를 붙이지 말 것:
다음 태스크가 그것을 지우는 것을 잊으면 진짜 미사용이 숨는다.

- [ ] **Step 7: 커밋**

```bash
git add components/paint/commit_scheduler.rs components/paint/lib.rs
git commit -m "$(cat <<'EOF'
feat(paint): 지연 DComp Commit 을 목표 시각에 내보내는 스케줄러

타일마다 목표 시각이 다르고(네 모니터가 주기의 0.67 에 흩어져 있다) 그 차이가 최대
11.2ms 다. 메인에서 기다릴 수 없고 기다려서도 안 되므로 마감 큐 하나를 가진 스레드가 건다.

★이 설계가 새로 만드는 위험을 뮤텍스로 막는다.★ 지금의 gfx_dcomp_parallel_commit 은
thread::scope 의 join 으로 '돌아올 때 그 디바이스를 만지는 워커가 없다' 를 보장하는데,
스케줄러는 나중에 커밋하므로 그 보장이 사라진다. 디바이스마다 뮤텍스를 두고 스케줄러와
end_frame 이 함께 잡는다. 경합이 드물고 짧아야 한다는 가정은 lock_wait_us 계수가 지켜본다.
이 뮤텍스는 생산 스레드와 무관하다 -- DComp 디바이스를 만지는 둘 사이에서만 걸린다.

같은 디바이스가 이미 걸려 있으면 덮어쓴다. 밀린 커밋을 쌓으면 한 주기에 여러 개가 나가고
그것이 정확히 없애려는 현상이다. 그 규칙만 upsert 로 갈라내 단위 테스트를 붙였다 --
나머지는 스레드와 COM 이라 이 트리에서 테스트할 수 없다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: pref 와 런처 스위치

**Files:**
- Modify: `components/config/prefs.rs`
- Modify: `etc/multigpu/run_wall_dist.ps1`

**Interfaces:**
- Produces: `gfx_present_align_per_output_pct: i64` (기본 `-1`), 런처 `-PerOutputAlign`

- [ ] **Step 1: pref 를 더한다**

`components/config/prefs.rs` 의 `pub gfx_present_align_dwm_pct: i64,` **바로 아래**에 추가:

```rust
    /// ★타일마다 자기 출력의 vblank 격자에 맞춰 Commit 한다.★ `-1`(기본) = 끔,
    /// `0..99` = 그 출력 주기의 백분율 지점을 겨냥한다.
    ///
    /// `gfx_present_align_dwm_pct` 는 **데스크톱(주 모니터) 격자 하나**에만 맞춘다. 실측에서
    /// 네 모니터의 vblank 가 주기의 0.67 에 흩어져 있어(log_ani_debug_02/02: 기준 대비
    /// +1.17 / +6.57 / −4.63ms), 좋은 자리를 5ms 로 넉넉히 잡아도 네 창의 교집합이
    /// 공집합이다 -- 어떤 커밋 시각을 골라도 최소 한 대는 나쁜 자리에 앉는다. 그래서
    /// 타일마다 따로 맞춘다.
    ///
    /// 켜면 `gfx_dcomp_parallel_commit` 은 무시된다(목적이 겹친다). 기동 로그에 남는다.
    ///
    /// 이 값이 고치는 것은 **타일 내 저더**다. 이음매를 넘는 물체의 완전한 연속성은 genlock
    /// 없이 성립하지 않는다.
    pub gfx_present_align_per_output_pct: i64,
```

기본값 리터럴의 `gfx_present_align_dwm_pct: -1,` **바로 아래**에 추가:

```rust
            gfx_present_align_per_output_pct: -1,
```

- [ ] **Step 2: 런처 스위치를 더한다**

`etc/multigpu/run_wall_dist.ps1` 의 `[int]    $DwmAlign = -1,` **바로 아래**에 추가:

```powershell
    # gfx_present_align_per_output_pct: 타일마다 자기 출력의 vblank 격자에 맞춰 Commit.
    # -1(기본) = 끔, 0..99 = 그 출력 주기의 백분율 지점.
    #
    # ★-DwmAlign 과의 차이★ -DwmAlign 은 데스크톱(주 모니터) 격자 하나에만 맞춘다. 실측에서
    # 네 모니터의 vblank 가 주기의 0.67 에 흩어져 있어(기준 대비 +1.17 / +6.57 / -4.63ms),
    # 좋은 자리를 5ms 로 잡아도 네 창의 교집합이 공집합이다 -- 어떤 시각을 골라도 최소 한
    # 대는 나쁜 자리에 앉는다. 그래서 타일마다 따로 맞춘다.
    #
    # 켜면 -DcompParallelCommit 은 무시된다(목적이 겹친다). 기동 로그에 남는다.
    # 판정은 -DcompBindProf 의 OUTCOMMIT 줄 -- 네 출력의 phase p50 이 전부 목표 근처여야 한다.
    [ValidateRange(-1, 99)]
    [int]    $PerOutputAlign = -1,
```

`$argList` 의 `"--pref", "gfx_present_align_dwm_pct=$DwmAlign",` **바로 아래**에 추가:

```powershell
    "--pref", "gfx_present_align_per_output_pct=$PerOutputAlign",
```

설정 요약 `Write-Host` 줄에서 `dwm_align=$DwmAlign` 을 다음으로 바꾼다:

```
dwm_align=$DwmAlign per_output_align=$PerOutputAlign
```

- [ ] **Step 3: 스크립트가 파싱되는지 확인한다**

```powershell
$e=$null; [void][System.Management.Automation.Language.Parser]::ParseFile((Resolve-Path .\etc\multigpu\run_wall_dist.ps1),[ref]$null,[ref]$e); if($e.Count){"PARSE ERROR: $($e[0].Message)"}else{"ok"}
```

기대: `ok`.

- [ ] **Step 4: config 표면 테스트를 돌린다**

```
cargo test -p servo-config --test config_surface --release
```

기대: 20 개 중 19 개 PASS, `every_env_name_read_in_sources_is_registered` 만 FAIL.
★그 하나는 이 작업 이전부터 실패하던 기존 결함이다★ — 지목하는 env 넷
(`SERVO_MEDIA_LOCK_SLOW_MS`, `SERVO_SCRIPT_FORCE_GC_SEC`, `SERVO_SCRIPT_SLOW_TASK_MS`,
`SERVO_WR_SLOW_MS`)은 이 계획이 손대는 파일에 없다. 다른 테스트가 새로 실패하면 그것은
이 변경 탓이다.

- [ ] **Step 5: 커밋**

```bash
git add components/config/prefs.rs etc/multigpu/run_wall_dist.ps1
git commit -m "$(cat <<'EOF'
feat: gfx_present_align_per_output_pct pref 와 -PerOutputAlign 스위치

gfx_present_align_dwm_pct 는 데스크톱 격자 하나에만 맞춘다. 실측에서 네 모니터의 vblank 가
주기의 0.67 에 흩어져 있어 좋은 자리를 5ms 로 잡아도 네 창의 교집합이 공집합이다 -- 어떤
커밋 시각을 골라도 최소 한 대는 나쁜 자리에 앉는다. 타일마다 따로 맞추기 위한 손잡이다.

기본 -1(끔)이라 기본 동작은 그대로다. 켜면 gfx_dcomp_parallel_commit 은 무시된다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: 타일 → 출력 해석과 목표 시각 계산, 그리고 커밋을 스케줄러로

**Files:**
- Modify: `components/paint/painter.rs` (`take_pending_dcomp_commit` 옆에 모니터 조회 추가)
- Modify: `components/paint/paint.rs:2878` (`flush_deferred_dcomp_commits`)
- Modify: `components/paint/dcomp_compositor.rs` (`end_frame` 진입 시 디바이스 가드)

**Interfaces:**
- Consumes: `output_grid::{grid_for_monitor, monitor_for_hwnd, qpc_now, OutputGrid}` (Task 1), `commit_scheduler::{schedule, device_guard}` (Task 2), `gfx_present_align_per_output_pct` (Task 3)
- Produces: `Painter::pending_dcomp_commit_monitor(&self) -> Option<usize>`

- [ ] **Step 1: painter 가 자기 모니터를 알려주게 한다**

`components/paint/painter.rs` 의 `pub(crate) fn take_pending_dcomp_commit(&self) -> Option<usize>`
**바로 위**에 추가:

```rust
    /// 이 painter 의 창이 올라가 있는 모니터(`HMONITOR`).
    ///
    /// ★매번 다시 묻는다.★ 디스플레이 구성이 바뀌면 `HMONITOR` 가 달라지므로 기동 시 한 번
    /// 캐시하면 핫플러그 뒤에 조용히 틀린 격자로 스케줄한다. `MonitorFromWindow` 는 API 한
    /// 번이라 프레임당 호출해도 비용이 없다.
    #[cfg(windows)]
    pub(crate) fn pending_dcomp_commit_monitor(&self) -> Option<usize> {
        let hwnd = self.rendering_context.window_hwnd()?;
        crate::output_grid::monitor_for_hwnd(hwnd)
    }
```

- [ ] **Step 2: `end_frame` 이 디바이스 가드를 잡게 한다**

`components/paint/dcomp_compositor.rs:3633` 의 `fn end_frame(&mut self, device: &mut Device) {`
**바로 다음 줄**에 추가:

```rust
        // ★스케줄러와 같은 디바이스를 동시에 만지지 않는다.★ 지금의 parallel_commit 은
        // `thread::scope` 의 join 으로 "돌아올 때 워커가 없다" 를 보장하는데, 커밋 스케줄러는
        // **나중에** 커밋하므로 그 보장이 사라진다. 정상 부하에서는 겹치지 않지만
        // (최대 지연 11.2ms < 주기 16.67ms) "정상 부하에서는" 은 보장이 아니다.
        // 경합은 드물고 짧아야 하며(커밋 0.02ms), 그 가정은 OUTCOMMIT 의 lock_wait 이 잰다.
        #[cfg(windows)]
        let _device_guard = self
            .dcomp_device_ptr()
            .map(|device| crate::commit_scheduler::device_guard(device as usize));
```

- [ ] **Step 3: flush 가 스케줄러에 넘기게 한다**

`components/paint/paint.rs:2878` 의 `pub fn flush_deferred_dcomp_commits(&self)` 본문
**맨 앞**(기존 `#[cfg(windows)] if servo_config::pref!(gfx_dcomp_parallel_commit)` 블록보다
위)에 추가:

```rust
        // ★타일마다 자기 출력의 격자에 맞춘다.★ 데스크톱 격자 하나로는 넷을 만족시킬 수
        // 없다 -- 실측에서 네 모니터의 vblank 가 주기의 0.67 에 흩어져 있고, 좋은 자리를
        // 5ms 로 잡아도 네 창의 교집합이 공집합이다(log_ani_debug_02/02).
        //
        // 여기서는 **거는 것만** 한다. 실제 Commit 은 스케줄러 스레드가 그 시각에 낸다.
        // 이 함수는 메인(또는 스레드된 painter 로의 왕복)에서 도므로 여기서 기다리면
        // 그대로 패스가 길어진다.
        #[cfg(windows)]
        {
            let pct = servo_config::pref!(gfx_present_align_per_output_pct);
            if (0..=99).contains(&pct) {
                if servo_config::pref!(gfx_dcomp_parallel_commit) {
                    static WARNED: std::sync::Once = std::sync::Once::new();
                    WARNED.call_once(|| {
                        warn!(
                            "[commitsched] gfx_present_align_per_output_pct 가 켜져 있어 \
                             gfx_dcomp_parallel_commit 을 무시한다 -- 목적이 겹친다"
                        );
                    });
                }
                let mut all_scheduled = true;
                for painter_id in self.painter_ids() {
                    let pending = self
                        .with_painter(painter_id, |painter| {
                            painter
                                .take_pending_dcomp_commit()
                                .map(|device| (device, painter.pending_dcomp_commit_monitor()))
                        })
                        .flatten();
                    let Some((device, monitor)) = pending else {
                        continue;
                    };
                    match monitor.and_then(deadline_for_monitor) {
                        Some(deadline) => crate::commit_scheduler::schedule(device, deadline),
                        None => {
                            // 격자를 못 구하면 지금 낸다 = 오늘 동작. 나빠지지 않는다.
                            all_scheduled = false;
                            crate::dcomp_compositor::commit_device_ptr(device);
                        },
                    }
                }
                let _ = all_scheduled;
                return;
            }
        }
```

같은 파일의 `flush_deferred_dcomp_commits` **바로 위**에 목표 시각 계산을 둔다:

```rust
/// 이 모니터의 다음 목표 커밋 시각(QPC). 격자가 없거나 너무 묵었으면 `None` -- 호출자는
/// 즉시 커밋으로 폴백한다.
///
/// `deadline = vblank + k*period + target` 에서 `k` 는 마감이 미래가 되는 최소 정수다.
/// ★매번 측정된 vblank 로부터 다시 센다.★ 자유 구동 기준점에 상수를 더하면 기준점이 매
/// 실행 임의라 결과도 임의다(선행 설계 §5-10 의 교훈).
#[cfg(windows)]
fn deadline_for_monitor(monitor: usize) -> Option<u64> {
    let pct = servo_config::pref!(gfx_present_align_per_output_pct).clamp(0, 99) as u64;
    let grid = crate::output_grid::grid_for_monitor(monitor)?;
    let now = crate::output_grid::qpc_now()?;
    if grid.period_qpc == 0 {
        return None;
    }
    // 격자가 60 주기(60Hz 면 1 초)보다 묵었으면 프로브가 정체한 것이다. 옛 격자로
    // 스케줄하는 것보다 즉시 커밋이 낫다.
    if now.saturating_sub(grid.sampled_qpc) > grid.period_qpc.saturating_mul(60) {
        return None;
    }
    let target = grid.period_qpc * pct / 100;
    let base = grid.vblank_qpc.wrapping_add(target);
    // vblank 는 드라이버에 따라 직전일 수도 다음일 수도 있다. 나머지 연산을 두 번 걸어
    // 어느 쪽이든 격자 위의 같은 점으로 접는다.
    let period = grid.period_qpc as i128;
    let delta = now as i128 - base as i128;
    let mut ahead = period - (((delta % period) + period) % period);
    // 지금과 너무 가까우면 한 칸 뒤로 -- 렌더가 끝난 직후라 커밋할 틈은 있지만, 스케줄러가
    // 깨어나기도 전에 지나간 마감은 즉시 커밋이 되어 정렬이 무의미해진다.
    if ahead < period / 8 {
        ahead += period;
    }
    Some(now.wrapping_add(ahead as u64))
}
```

`components/paint/paint.rs` 상단에 `warn` 이 이미 import 돼 있지 않으면 추가한다.

- [ ] **Step 4: 빌드**

```
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release --message-format short
```

기대: 성공. Task 2 에서 났던 `schedule`/`device_guard` 미사용 경고가 사라진다.
`take_stats` 만 미사용으로 남는다(Task 5 에서 없어진다).

- [ ] **Step 5: 끄면 동작이 그대로인지 확인한다**

pref 기본이 `-1` 이므로 새 코드는 돌지 않는다. 배포본을 만들어 **`-PerOutputAlign` 없이**
한 번 돌려, `WALLCLOCK`/`WALLPASS`/`DWMPHASE` 가 기준선과 같은지 본다.

```powershell
.\etc\multigpu\make_wall_dist.ps1 -Force
```

★기준선★(log_ani_debug/48, `-DwmAlign 0 -RefreshHz 60`): `WALLCLOCK p95` 17.00~17.23,
`off` 합 5~9, `WALLPASS pass_ms avg` 1.8~2.5, `DWMPHASE phase p50` 0.119~0.123.

이 값들이 유의하게 달라졌으면 **끈 상태에서 무언가가 바뀐 것**이므로 다음 태스크로 가기
전에 원인을 찾는다. `end_frame` 의 디바이스 가드는 pref 와 무관하게 항상 걸리므로 그것이
비용을 내는지가 여기서 갈린다.

- [ ] **Step 6: 커밋**

```bash
git add components/paint/painter.rs components/paint/paint.rs components/paint/dcomp_compositor.rs
git commit -m "$(cat <<'EOF'
feat(paint): 지연 커밋을 타일 출력의 목표 시각에 건다

flush_deferred_dcomp_commits 가 즉시 커밋하는 대신, 타일마다 자기 출력의 격자에서 다음
목표 시각을 구해 스케줄러에 넘긴다. 데스크톱 격자 하나로는 넷을 만족시킬 수 없다 --
네 모니터의 vblank 가 주기의 0.67 에 흩어져 있어 좋은 자리를 5ms 로 잡아도 네 창의
교집합이 공집합이다(log_ani_debug_02/02).

목표 시각은 매번 측정된 vblank 로부터 다시 센다. 자유 구동 기준점에 상수를 더하면
기준점이 매 실행 임의라 결과도 임의다(선행 설계 §5-10 의 교훈).

모니터는 프레임마다 MonitorFromWindow 로 다시 묻는다 -- 기동 시 캐시하면 핫플러그 뒤에
조용히 틀린 격자로 스케줄한다. 격자가 없거나 60 주기보다 묵었으면 즉시 커밋으로
폴백한다. 모든 폴백의 종착지는 오늘 동작이다.

end_frame 이 디바이스 가드를 잡는다 -- 스케줄러가 나중에 커밋하므로 parallel_commit 의
join 보장이 사라지기 때문이다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: OUTCOMMIT — 타일별 커밋이 **자기 출력** 격자 어디에 떨어졌나

**Files:**
- Modify: `components/paint/commit_scheduler.rs` (커밋 시점의 위상 기록)
- Modify: `components/paint/dcomp_compositor.rs` (초당 한 줄 출력)

**Interfaces:**
- Consumes: `output_grid::{grid_for_monitor, qpc_now}`, `commit_scheduler::take_stats`
- Produces: 로그 줄 `OUTCOMMIT`

- [ ] **Step 1: 스케줄러가 커밋 시점 위상을 모은다**

`components/paint/commit_scheduler.rs` 의 `schedule` 이 모니터도 받도록 시그니처를 바꾼다:

```rust
pub(crate) fn schedule(device: usize, monitor: usize, deadline_qpc: u64) {
```

큐 항목을 `(u64, usize, usize)` = `(마감, 디바이스, 모니터)` 로 바꾸고 `upsert` 도 같이
바꾼다(테스트의 기대값도 3-튜플로 갱신한다):

```rust
fn upsert(queue: &mut Vec<(u64, usize, usize)>, device: usize, monitor: usize, deadline: u64) {
    if let Some(slot) = queue.iter_mut().find(|(_, d, _)| *d == device) {
        slot.0 = deadline;
        slot.2 = monitor;
    } else {
        queue.push((deadline, device, monitor));
    }
}
```

커밋 직후 위상을 기록한다. `scheduler_loop` 의 `crate::dcomp_compositor::commit_device_ptr(device);`
**바로 다음**에:

```rust
            // ★이것이 판정이다.★ 이 커밋이 **자기 출력** 격자의 어디에 떨어졌나.
            // 지금까지는 데스크톱 격자 하나만 보였으므로 나머지 셋이 어디 있는지 알 수 없었다.
            if let (Some(grid), Some(after)) =
                (crate::output_grid::grid_for_monitor(monitor), qpc_now())
            {
                if grid.period_qpc > 0 {
                    let period = grid.period_qpc as i128;
                    let delta = after as i128 - grid.vblank_qpc as i128;
                    let folded = ((delta % period) + period) % period;
                    record_phase(monitor, folded as f64 / period as f64);
                }
            }
```

위상 저장소를 파일 상단 static 옆에 추가:

```rust
/// 모니터 -> 이번 창의 위상 표본. `OUTCOMMIT` 이 초당 비운다.
static PHASES: Mutex<Option<HashMap<usize, Vec<f64>>>> = Mutex::new(None);

fn record_phase(monitor: usize, phase: f64) {
    if let Ok(mut guard) = PHASES.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .entry(monitor)
            .or_default()
            .push(phase);
    }
}

/// 모니터별 위상 표본을 꺼내 비운다.
pub(crate) fn take_phases() -> Vec<(usize, Vec<f64>)> {
    let Ok(mut guard) = PHASES.lock() else {
        return Vec::new();
    };
    match guard.as_mut() {
        Some(map) => map.drain().collect(),
        None => Vec::new(),
    }
}
```

- [ ] **Step 2: 호출처를 새 시그니처에 맞춘다**

`components/paint/paint.rs` 의 `crate::commit_scheduler::schedule(device, deadline)` 을
`crate::commit_scheduler::schedule(device, monitor_value, deadline)` 로 바꾼다. `monitor` 는
`Option<usize>` 이므로 `match` 안에서 이미 확정된 값을 쓴다:

```rust
                    match monitor.and_then(deadline_for_monitor) {
                        Some(deadline) => crate::commit_scheduler::schedule(
                            device,
                            monitor.unwrap_or_default(),
                            deadline,
                        ),
```

- [ ] **Step 3: 초당 한 줄을 찍는다**

`components/paint/dcomp_compositor.rs` 의 `maybe_emit_bind_profile` 안, `DCOMPBIND` 를
찍는 `log::info!`/`warn!` **바로 다음**에 추가:

```rust
        // ★OUTCOMMIT -- 타일별 커밋이 자기 출력 격자 어디에 떨어졌나.★
        //
        // 이 추적 내내 맹점이었던 양이다. DWMPHASE 는 데스크톱(주 모니터) 격자 하나만 보므로
        // "주 모니터 기준 0.135 로 안전한데 화면은 저더" 가 성립했다. 출력마다 따로 봐야
        // 넷이 전부 좋은 자리에 앉았는지 알 수 있다.
        //
        // slip = 스케줄러가 마감보다 늦은 시간(크면 스케줄러가 병목).
        // lock_wait = 디바이스 뮤텍스 대기(0 에 가까워야 한다는 가정의 검산).
        #[cfg(windows)]
        if *DCOMP_BIND_PROF {
            let stats = crate::commit_scheduler::take_stats();
            for (monitor, mut phase) in crate::commit_scheduler::take_phases() {
                if phase.is_empty() {
                    continue;
                }
                phase.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let at = |p: f64| phase[(((phase.len() - 1) as f64) * p).round() as usize];
                warn!(
                    "OUTCOMMIT monitor={monitor:#x} n={} phase p05={:.3} p50={:.3} p95={:.3} \
                     scheduled={} slip_us_max={} lock_wait_us_max={}",
                    phase.len(),
                    at(0.05),
                    at(0.50),
                    at(0.95),
                    stats.scheduled,
                    stats.slip_us_max,
                    stats.lock_wait_us_max,
                );
            }
        }
```

- [ ] **Step 4: 빌드하고 미사용 경고가 사라졌는지 본다**

```
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release --message-format short
```

기대: 성공. Task 2 에서 남겨 둔 `take_stats` 미사용 경고가 사라진다. 남아 있으면 Step 3 이
빠진 것이다.

- [ ] **Step 5: 큐 테스트를 다시 돌린다**

Step 1 에서 `upsert` 시그니처가 바뀌었으므로 테스트도 3-튜플로 갱신돼 있어야 한다.

```
cargo test -p servo-paint --lib commit_scheduler --release
```

기대: 두 테스트 PASS.

- [ ] **Step 6: 커밋**

```bash
git add components/paint/commit_scheduler.rs components/paint/paint.rs components/paint/dcomp_compositor.rs
git commit -m "$(cat <<'EOF'
diag(paint): OUTCOMMIT -- 타일별 커밋이 자기 출력 격자 어디에 떨어졌나

이 추적 내내 맹점이었던 양이다. DWMPHASE 는 DwmGetCompositionTimingInfo 가 hWnd 에 NULL 만
받아 데스크톱 격자 하나만 본다. 그래서 '주 모니터 기준 위상 0.135 로 안전한데 화면은 저더'
가 성립했고, 원인이 보이기까지 여러 라운드가 걸렸다.

스케줄러가 커밋 직후 그 출력 격자에서의 위상을 모으고, 초당 한 줄로 모니터마다 찍는다.
slip(마감 대비 지연)과 lock_wait(디바이스 뮤텍스 대기)을 같이 낸다 -- 후자는 '경합이 드물고
짧다' 는 설계 가정의 검산이다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: 실기 검증 (운영자)

**Files:** 없음. 측정과 판정만.

**Interfaces:**
- Consumes: Task 1~5 전부

★이 태스크는 사람과 물리적 벽을 요구한다.★ 구현자는 배포본 생성까지 하고 기기 결과를
지어내지 않는다.

- [ ] **Step 1: 배포본을 만든다**

```powershell
.\etc\multigpu\make_wall_dist.ps1 -Force
```

`BUILD.txt` 의 `commit` 이 Task 5 의 커밋인지 확인한다. 아니면 빌드가 옛것이다.

- [ ] **Step 2: 끈 상태 기준선 (운영자)**

```
.\run_wall_dist.ps1 -RefreshHz 60 -DwmAlign 0 -DcompBindProf -DurationSec 120
```

`[outgrid] 출력 N 개를 잰다` 가 찍혀야 한다. 안 찍히면 프로브가 못 떴다.

- [ ] **Step 3: 켠 상태 (운영자, 최소 5 회)**

```
.\run_wall_dist.ps1 -RefreshHz 60 -PerOutputAlign 8 -DcompBindProf -DurationSec 120
```

★한 런의 성공은 판정이 아니다.★ 이 추적에서 5 회 중 4 회 성공을 개선으로 읽었다가 두 번
틀렸다(같은 쌍안정 과정의 두 표본이었다). **여러 런에서 일관된가**가 판정이다.

프로브 페이지는 `wall_anim_jitter_probe.html` 을 쓴다 — A 행이 가로 이음매에 걸쳐 있어
네 GPU 영역을 모두 지난다.

- [ ] **Step 4: 성공 기준을 대조한다**

★기준 0 을 먼저 본다. 통과하지 못하면 나머지는 읽을 가치가 없다.★ 마감도 격자로 계산하고
위상도 **같은 격자**로 접으므로, 격자가 틀려도 `phase p50` 은 목표치를 가리킨다(순환).
격자가 옳다는 것을 독립적으로 확인한 뒤에야 기준 1 이 의미를 갖는다.

| # | 기준 | 어디서 |
|---|---|---|
| 0 | `OUTPHASE` 가 **네 줄** 나오고, 네 줄 모두 `period_ms≈16.67` 이며 `measured=1` | `OUTPHASE` |
| 1 | 네 출력의 `OUTCOMMIT phase p50` 이 전부 목표(0.08)의 ±0.05 안 | `OUTCOMMIT` |
| 2 | `WALLPASS pass_ms avg` 가 기준선(1.8~2.5)에서 유의하게 나빠지지 않음 | `WALLPASS` |
| 3 | `WALLCLOCK off` 합이 기준선(5~9)에서 유의하게 나빠지지 않음 | `WALLCLOCK` |
| 4 | `lock_wait_painter_us_max` < 100 | `OUTCOMMIT total` |
| 5 | `slip_us_max` < 2000 | `OUTCOMMIT total` |
| 6 | 육안: A 행(네 영역 통과)에서 저더가 **여러 런에 걸쳐 일관되게** 관찰되지 않음 | 사람 |

기준 0 의 실패 모양 셋:
- `period_ms` 가 8.33 이나 12.50 → 주기 추정이 틀렸다(옛 `span/N` 산술의 재발).
- `measured=0` → 실측이 검사에 걸려 가정값 60Hz 로 떨어졌다. 맞는 값이지만 재고 있지 않다.
- `grid=none` 줄이 있거나 줄이 넷보다 적다 → 그 출력은 격자가 없어 **폴백 커밋**이다.
  그 타일에는 이 기능이 걸리지 않은 것이므로 기준 1 에서 제외하지 말고 실패로 읽는다.

★기준 4 의 필드 이름에 주의한다.★ `lock_wait_us_max` 는 더 이상 없다. 두 방향을 나눠 내며,
기준 4 가 읽어야 하는 것은 **`lock_wait_painter_us_max`**(콘텐츠 쪽이 스케줄러의 커밋 하나를
기다린 시간, 상한이 `Commit()` 하나라는 주장의 검산)다. `lock_wait_sched_us_max` 는 반대
방향(스케줄러가 페인터의 `end_frame` 임계구역을 기다린 시간)이라 구조상 훨씬 크고, 그것을
읽으면 정상인 벽을 탈락시킨다.

- [ ] **Step 5: 결과를 설계 문서에 적고 커밋한다**

`docs/superpowers/specs/2026-09-21-per-output-commit-phase-design.md` 끝에 `## 실기 결과`
절을 더해 여섯 기준의 실측값과 판정을 적는다. ★판정 근거를 남기지 않으면 다음 사람이 같은
측정을 다시 한다.★

기준 0 이 틀리면 격자 자체가 틀린 것이므로 `OUTPHASE` 와 `[outgrid]` 부터 본다 — 이 경우
기준 1 의 통과는 아무것도 뜻하지 않는다. 기준 0·1 이 맞는데 6 이 틀리면 **B1 로는
부족하다**는 뜻이고, B2(타일별 샘플 시각)가 다음 차례다. 기준 0 은 맞는데 1 이 틀리면
스케줄링이 안 걸린 것이므로 `slip`/폴백 경로를 먼저 본다.

---

## Self-Review

**1. 스펙 커버리지**

| 스펙 절 | 태스크 |
|---|---|
| §1 격자 원천(프로브 승격, 공유 상태, 폴백) | Task 1 |
| §2 타일↔출력 매핑(HWND→HMONITOR→출력, 주기적 재해석) | Task 4 Step 1 (프레임마다 재질의) |
| §3 B1 커밋 스케줄링(마감 큐, 목표 시각 식) | Task 2, Task 4 Step 3 |
| §3 동시 접근 위험(디바이스 뮤텍스, 경합 계수) | Task 2 Step 1, Task 4 Step 2, Task 5 |
| §3 `parallel_commit` 과의 관계(무시하고 로그) | Task 4 Step 3 |
| §4 primary 비대칭 | Task 4 — 커밋만 스케줄하므로 primary 도 대칭으로 다뤄진다. **셸 클럭 격자를 primary 출력으로 바꾸는 것은 이 계획에서 뺐다**(아래 참고) |
| §5 B2 | 범위 밖(Global Constraints) |
| §계측과 판정(OUTCOMMIT, 성공 기준 4) | Task 5, Task 6 |
| §실패 모드와 폴백(넷) | Task 1(프로브 실패), Task 4(매핑 실패·격자 묵음), Task 2(큐 적체=즉시 커밋) |

★스펙 §4 의 "셸 클럭 격자를 primary 출력으로" 는 이 계획에서 제외했다.★ 이유: B1 은 커밋
시각만 옮기므로 그것 없이도 성공 기준 1~6 을 판정할 수 있고, 셸 클럭을 건드리면
`gfx_present_align_dwm_pct` 와 상호작용해 실패 시 원인이 둘로 갈린다. Task 6 에서 기준 1 이
맞는데 6 이 틀리면 그때 별도로 다룬다. 이 제외를 스펙에 반영하는 것은 Task 6 Step 5 의
결과 기록에서 함께 한다.

**2. 플레이스홀더 스캔** — "TBD/TODO/적절히" 없음. 모든 코드 단계에 실제 코드가 있다.
테스트를 붙일 수 없는 곳(Task 1)은 그 사실과 이유를 본문에 적었다.

**3. 타입 일관성** — `schedule` 은 Task 2 에서 2-인자로 만들고 Task 5 에서 3-인자로 바꾼다.
그 변경과 호출처 갱신, 테스트 갱신을 Task 5 Step 1·2·5 에 명시했다. `OutputGrid` 필드명
(`vblank_qpc`/`period_qpc`/`sampled_qpc`)은 Task 1 정의와 Task 4·5 사용이 일치한다.
`monitor` 는 전 구간 `usize`(HMONITOR 를 usize 로) 로 통일했다.

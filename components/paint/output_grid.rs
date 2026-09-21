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
#![allow(unsafe_code)]
// `enumerate_outputs` 는 사실상 전체가 raw DXGI FFI다 -- `unsafe fn` 본문 안의 개별
// 연산마다 중첩 `unsafe {}`를 요구하는 edition 2024 린트는 신호 대비 잡음이 크므로 끈다
// (dcomp_compositor.rs/dcomp_video_convert.rs의 `#![allow(unsafe_code)]`와 같은 취지).
#![allow(unsafe_op_in_unsafe_fn)]

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
    if outputs.is_empty() {
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
    // 이 반환은 이미 살아 있는 outputs 의 IDXGIOutput 을 해제하지 않는다 -- 프로세스 생애
    // 동안 많아야 한 번, 넷짜리 객체라 감수한다.
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

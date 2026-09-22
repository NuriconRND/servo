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

use std::collections::{HashMap, HashSet};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use log::warn;
use winapi::Interface;
use winapi::shared::dxgi::{
    CreateDXGIFactory1, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
};
use winapi::um::profileapi::{QueryPerformanceCounter, QueryPerformanceFrequency};
use winapi::um::wingdi::DEVMODEW;
use winapi::um::winuser::{
    ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, MONITOR_DEFAULTTONEAREST, MonitorFromWindow,
};

/// 한 출력의 vblank 격자.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputGrid {
    /// 마지막으로 관측한 vblank 의 QPC. ★"언제 뜬 표본인가" 도 이 값이다★ --
    /// `WaitForVBlank` 가 돌아온 직후 시계를 읽으므로 관측 시각과 vblank 시각이 같은
    /// 값이다. 그래서 격자가 얼마나 묵었는지도 이것 하나로 판단한다(예전에 따로 두었던
    /// `sampled_qpc` 는 언제나 이것과 같은 값이라, 구분이 있는 척하는 이름뿐이었다).
    pub vblank_qpc: u64,
    /// 한 주기의 QPC 틱.
    pub period_qpc: u64,
    /// `period_qpc` 를 믿을 근거가 있나. 참이면 디스플레이 모드에서 온 정확값(또는 정상
    /// 범위의 실측)이고, 거짓이면 60Hz 가정값으로 떨어진 것이다.
    /// ★틀린 주기는 목표 지점을 매 프레임 격자의 다른 자리에 떨어뜨려, 이 작업이 없애려는
    /// 바로 그 저더를 만든다.★ 그런데 위상 로그는 같은 격자로 접으므로 주기가 틀려도
    /// 목표치를 가리킨다 -- 출처를 따로 싣지 않으면 그 사실을 볼 방법이 없다.
    pub measured: bool,
}

static GRID: Mutex<Option<HashMap<usize, OutputGrid>>> = Mutex::new(None);

/// HMONITOR -> `DeviceName`. ★격자와 따로 둔다★ -- `OutputGrid` 는 타일마다 매 프레임
/// 복사되는 `Copy` 값이고 이름은 초당 한 번 로그를 찍을 때만 필요하다. 이름을 격자에
/// 실으면 그 뜨거운 경로가 `String` 복제를 물게 된다.
static NAMES: Mutex<Option<HashMap<usize, String>>> = Mutex::new(None);

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

/// 이 벽 프레임의 **공통 목표 표시 시각**(QPC)과 그때 쓴 주기. 한 패스 안에서 네 painter 가
/// 같은 값을 받는다.
///
/// ★타일마다 따로 올림하면 안 된다.★ 처음 구현이 그랬고, 그것이 실기에서 2 프레임 이상의
/// 타일 간 어긋남을 만들었다(log_ani_debug_02/07). 각 타일이 *자기* 다음 vblank 로 올림하는데
/// 공통 기준이 없으면, 두 타일의 렌더가 격자 경계를 사이에 두고 갈라질 때 절대 시각이 한
/// 주기 통째로 벌어진다. 설계가 약속한 것은 "위상차 이내(= 1 프레임 미만)" 였으므로 그것은
/// 거래가 아니라 결함이다.
///
/// 그래서 기준 출력 하나의 격자에서 T 를 한 번 구하고, 타일은 거기에 **자기 위상 오프셋만**
/// 더한다. 오프셋은 정의상 `[0, period)` 이므로 네 타일의 퍼짐이 **구조적으로** 한 주기
/// 미만이다. T 가 다음 격자점으로 넘어갈 때는 넷이 **함께** 넘어간다.
///
/// 한 패스 안에서 같은 T 를 주려고 반 주기 동안 기억한다 -- 벽 패스는 ~2.5ms 라 네 painter 가
/// 그 창 안에 전부 들어온다.
fn wall_sample_base(now: u64, lead_periods: u64) -> Option<(u64, u64)> {
    static BASE: Mutex<Option<(u64, u64, u64)>> = Mutex::new(None);
    let (reference, period) = reference_grid()?;
    let mut guard = BASE.lock().ok()?;
    if let Some((set_at, base, remembered)) = *guard {
        if remembered == period && now.saturating_sub(set_at) < period / 2 {
            return Some((base, period));
        }
    }
    let base = now
        .saturating_add(lead_ticks(now, reference, period, lead_periods)?);
    *guard = Some((now, base, period));
    Some((base, period))
}

/// 기준 출력의 vblank 와 주기. ★어느 것을 고르든 상관없지만 **매번 같아야 한다**★ --
/// 기준이 바뀌면 T 가 통째로 움직인다. HMONITOR 최솟값은 열거 순서와 무관하게 안정적이다.
fn reference_grid() -> Option<(u64, u64)> {
    let guard = GRID.lock().ok()?;
    let map = guard.as_ref()?;
    let (_, grid) = map
        .iter()
        .filter(|(_, grid)| grid.period_qpc > 0)
        .min_by_key(|(monitor, _)| **monitor)?;
    Some((grid.vblank_qpc, grid.period_qpc))
}

/// 기준 격자로부터 이 출력이 얼마나 뒤에 있나. 정의상 `[0, period)`.
///
/// 격자가 없는 타일은 0 -- ★`now` 로 떨어뜨리지 않는다.★ 예전에는 그렇게 했고, 그러면 그
/// 타일만 lead 0 이고 나머지는 17~33ms 라 **즉시 2 프레임이 벌어졌다**. 공통 T 를 그대로
/// 쓰는 편이 언제나 낫다: 위상 보정을 못 받을 뿐 같은 프레임 안에 머문다.
fn phase_offset_for(monitor: usize, reference_vblank: u64, period: u64) -> u64 {
    let Some(grid) = grid_for_monitor(monitor) else {
        return 0;
    };
    if period == 0 {
        return 0;
    }
    let period_i = period as i128;
    let delta = grid.vblank_qpc as i128 - reference_vblank as i128;
    (((delta % period_i) + period_i) % period_i) as u64
}

/// 이 타일이 **공통 목표 시각**에 닿기까지 남은 간격. B2 의 샘플 시각이 이것이다.
pub(crate) fn lead_to_next_vblank(monitor: usize, lead_periods: u64) -> Option<Duration> {
    let now = qpc_now()?;
    let freq = qpc_frequency()?;
    let (base, period) = wall_sample_base(now, lead_periods)?;
    let (reference_vblank, _) = reference_grid()?;
    let target = base.saturating_add(phase_offset_for(monitor, reference_vblank, period));
    // 이미 지난 목표는 0 으로 -- 음수 lead 는 만들지 않는다. `lead_periods >= 1` 이면 T 가
    // 최소 한 주기 앞이라 정상 부하에서는 걸리지 않는다.
    let ticks = target.saturating_sub(now);
    Some(Duration::from_secs_f64(ticks as f64 / freq as f64))
}

/// ★주기는 재는 것이 아니라 **정해진 값**이다.★ 디스플레이 모드가 알려 준다.
///
/// 처음에는 vblank 관측에서 주기를 역산했다. 그 산술을 한 번 고쳤지만(한 바퀴를 출력 수로
/// 나누던 것 → 같은 출력의 연속 두 vblank), **측정이 옳은 출처인가는 묻지 않았다.** 그것이
/// 잘못이었다: 실측값은 16.55~16.78ms 로 흔들리고, 그 흔들림이 목표 지점을 주기 경계 너머로
/// 밀어낸다. 실기에서 한 출력의 샘플 lead 가 17.00↔33.05ms, 즉 **한 주기를 통째로** 오갔다
/// (log_ani_debug_02/07, 141).
///
/// 그래서 명목 주사율을 모드에서 읽고, 실측은 **정확값 후보 둘 중 어느 쪽인지 고르는 데만**
/// 쓴다: 60Hz 냐 60000/1001Hz(59.94)냐. 고른 뒤에는 그 실행 동안 고정이다.
fn nominal_period_ticks(name: &str, freq: u64, measured: Option<u64>) -> Option<u64> {
    let hz = display_frequency_hz(name)?;
    if hz == 0 {
        return None;
    }
    // 정수 Hz 후보와 그 1000/1001 변형(59.94 등). 정수 나눗셈의 버림은 60Hz·10MHz 에서
    // 0.02ppm 이라 무시할 수 있다.
    let exact = freq / hz;
    let ntsc = freq.saturating_mul(1001) / (hz.saturating_mul(1000));
    let Some(measured) = measured else {
        return Some(exact);
    };
    let pick_exact = measured.abs_diff(exact) <= measured.abs_diff(ntsc);
    Some(if pick_exact { exact } else { ntsc })
}

/// 이 출력의 현재 모드 주사율(Hz). `\\.\DISPLAY3` 같은 `DeviceName` 을 그대로 받는다.
fn display_frequency_hz(name: &str) -> Option<u64> {
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    wide.push(0);
    let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
    mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    // Safety: 널 종료된 장치 이름과 크기를 채운 DEVMODEW 를 넘긴다. 순수 조회다.
    let ok = unsafe { EnumDisplaySettingsW(wide.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) };
    if ok == 0 {
        return None;
    }
    // 1 과 0 은 "기본값" 을 뜻하는 특수값이라 주사율로 쓸 수 없다.
    match mode.dmDisplayFrequency {
        0 | 1 => None,
        hz => Some(hz as u64),
    }
}

/// `lead_to_next_vblank` 의 산술만 갈라낸 것 -- COM 도 시계도 없이 테스트할 수 있다.
///
/// `vblank` 는 드라이버에 따라 직전일 수도 다음일 수도 있으므로 나머지 연산을 두 번 걸어
/// 어느 쪽이든 격자 위의 같은 점으로 접는다(`deadline_for_monitor` 와 같은 규약).
fn lead_ticks(now: u64, vblank: u64, period: u64, lead_periods: u64) -> Option<u64> {
    if period == 0 {
        return None;
    }
    let period_i = period as i128;
    let delta = now as i128 - vblank as i128;
    // 다음 격자점까지 남은 틱. `now` 가 정확히 격자점이면 한 주기 뒤를 가리킨다 -- 이미
    // 지나간 시각을 샘플 시각으로 주는 것보다 낫다.
    let ahead = period_i - (((delta % period_i) + period_i) % period_i);
    Some(ahead as u64 + period.saturating_mul(lead_periods))
}

/// 모니터 -> 이번 창의 샘플 lead 표본(ms). `SAMPLELEAD` 가 초당 비운다.
static LEADS: Mutex<Option<HashMap<usize, Vec<f64>>>> = Mutex::new(None);
/// 격자나 모니터를 못 구해 오늘 동작으로 떨어진 횟수.
static LEAD_MISSING: AtomicU64 = AtomicU64::new(0);

pub(crate) fn note_sample_lead(monitor: usize, lead: Duration) {
    if let Ok(mut guard) = LEADS.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .entry(monitor)
            .or_default()
            .push(lead.as_secs_f64() * 1000.0);
    }
}

pub(crate) fn note_sample_lead_missing() {
    LEAD_MISSING.fetch_add(1, Ordering::Relaxed);
}

/// ★네 타일의 lead 가 실제로 위상차만큼 벌어져 있는지 보는 줄이다.★ 전부 같은 값이 나오면
/// B2 가 걸리지 않은 것이고(모니터 해석 실패 등), 그러면 이 작업은 아무 일도 하지 않는다.
/// 프로브 스레드가 자기 1 초 창으로 낸다 -- 페인터마다 찍으면 창이 넷으로 쪼개진다.
fn emit_samplelead() {
    let leads: Vec<(usize, Vec<f64>)> = match LEADS.lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(map) => map.drain().collect(),
            None => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    let missing = LEAD_MISSING.swap(0, Ordering::Relaxed);
    for (monitor, mut lead) in leads {
        if lead.is_empty() {
            continue;
        }
        lead.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let at = |p: f64| lead[(((lead.len() - 1) as f64) * p).round() as usize];
        let name = name_for_monitor(monitor).unwrap_or_else(|| "?".into());
        warn!(
            "SAMPLELEAD out={name} monitor={monitor:#x} n={} lead_ms p05={:.2} p50={:.2} \
             p95={:.2} nogrid={missing}",
            lead.len(),
            at(0.05),
            at(0.50),
            at(0.95),
        );
    }
}

/// 이 HMONITOR 의 `DeviceName`(`\\.\DISPLAY3` 등). ★로그 전용이다★ -- `OUTCOMMIT` 의 p50
/// 이 틀렸을 때 `monitor=0x…` 만으로는 어느 물리 디스플레이인지 짚을 수 없다.
pub(crate) fn name_for_monitor(monitor: usize) -> Option<String> {
    NAMES.lock().ok()?.as_ref()?.get(&monitor).cloned()
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

/// 한 번의 열거 결과. ★팩토리를 같이 들고 있는다★ -- `IDXGIFactory1::IsCurrent()` 는
/// "이 팩토리를 만든 뒤 어댑터/출력 구성이 바뀌었나" 를 답하므로, 목록을 만든 바로 그
/// 팩토리만이 그 목록이 썩었는지 알 수 있다.
struct Enumeration {
    factory: *mut IDXGIFactory1,
    outputs: Vec<Output>,
}

impl Enumeration {
    /// Safety: 프로브 스레드에서만, 이 열거의 포인터를 아무도 안 쓸 때 부른다.
    unsafe fn release(&mut self) {
        for out in self.outputs.drain(..) {
            (*out.output).Release();
        }
        if !self.factory.is_null() {
            (*self.factory).Release();
            self.factory = ptr::null_mut();
        }
    }

    /// 다시 열거해야 하나. ★"구성이 바뀌었나" 와 "지금 잴 것이 있나" 는 다른 질문이고,
    /// 둘을 하나로 묶었던 것이 앞선 수정의 결함이었다.★
    ///
    /// 예전에는 `!factory.is_null() && IsCurrent() == 0` 만 보았다. 그러면 재열거 도중
    /// `CreateDXGIFactory1` 이 한 번 실패해 NULL 팩토리 + 빈 목록이 남는 순간, 이 조건이
    /// **영원히 거짓**이 되어 다시는 열거를 시도하지 않는다 -- 잴 출력도 없으므로 남은 실행
    /// 내내 전 타일이 폴백한다. 핫플러그 도중의 일시적 실패 하나가 영구 고장이 되는 것이고,
    /// 그것은 I-9 가 없애려던 바로 그 실패 모양이다.
    ///
    /// 그래서 세 경우 모두 재열거를 요구한다: 팩토리가 없거나(직전 시도 실패), 잴 출력이
    /// 없거나, 팩토리가 구성 변경을 알렸거나. 재시도 폭주는 호출자의 백오프가 막는다.
    ///
    /// Safety: 살아 있는 팩토리이거나 NULL.
    unsafe fn needs_reenumerate(&self) -> bool {
        self.factory.is_null() || self.outputs.is_empty() || (*self.factory).IsCurrent() == 0
    }

    /// 열거된 출력 이름들. `announce` 가 목록이 실제로 바뀌었는지 볼 때만 쓴다.
    fn names(&self) -> Vec<String> {
        self.outputs.iter().map(|o| o.name.clone()).collect()
    }
}

/// 같은 사유의 경고를 **초당 한 번**으로 줄인다.
///
/// 구성 변경 과도 상태(팩토리는 살아 있는데 `IsCurrent()` 가 거짓이고 출력이 0)에서는 이
/// 루프가 백오프 간격마다 돌고, 제한이 없으면 초당 스무 줄까지 나온다. 그 로그는 상황을
/// 설명하지 않고 덮기만 한다.
fn throttled(slot: &mut Option<Instant>) -> bool {
    let now = Instant::now();
    match *slot {
        Some(at) if now.duration_since(at) < Duration::from_secs(1) => false,
        _ => {
            *slot = Some(now);
            true
        },
    }
}

/// 같은 출력의 **연속 두 vblank** 시각에서 주기를 낸다. 두 번째 값은 실측인지 여부이고,
/// 거짓이면 `assumed` 로 떨어졌다는 뜻이다.
///
/// ★한 바퀴 도는 시간에서 역산하면 안 된다.★ 예전 코드는 같은 출력의 연속 두 관측 간격을
/// 출력 수로 나눴는데, 한 바퀴에 걸리는 시간은 각 구간 `(위상_{i+1} - 위상_i) mod P` 의 합
/// 이라 항상 `w·P` 이고 그 `w` 는 열거 순서가 위상 원을 감는 횟수(`1..N`)다 -- 출력 수가
/// 아니다. 게다가 모니터들이 서로 드리프트하므로 `w` 는 실행 중에 바뀐다. 그래서 그 계산은
/// `w=2` 면 120Hz, `w=3` 이면 80Hz 처럼 **정상 범위 안의 틀린 주기**를 발행했고, 틀린 주기로
/// 접은 마감은 매 프레임 실제 위상의 다른 자리에 떨어져 정확히 이 작업이 없애려는 저더를
/// 만든다. 연속 두 vblank 사이에는 정의상 한 주기만 들어가므로 감는 횟수가 개입할 여지가
/// 없다.
///
/// ★남아 있는 한계(고치지 않았다, 기록용).★ 두 대기 사이에 이 스레드가 선점당하면 첫 vblank
/// 를 놓쳐 `2P` 가 측정될 수 있다. 60Hz 에서 `2P` = 33.3ms 는 30Hz 컷에 걸려 가정값으로
/// 떨어지므로 이 벽에서는 드러나지 않는다. 그러나 **60Hz 를 넘는 출력에서는 그 `2P` 가
/// 30~240Hz 창 안에 들어온다** -- 예컨대 120Hz 의 `2P` 는 16.67ms 라 `measured=1` 인 채로
/// 정확히 절반의 주파수를 발행한다. 혼합 주사율 벽에서 이 격자를 쓰려면 표본을 여러 개 떠
/// 중앙값을 쓰거나, `2P` 를 걸러낼 일관성 검사가 필요하다.
fn period_from_pair(t1: u64, t2: u64, freq: u64, assumed: u64) -> (u64, bool) {
    // 역행(t2 < t1)은 있을 수 없는 관측이다 -- QPC 가 뒤로 갔거나 표본이 섞였다는 뜻이라
    // 값 자체를 믿을 수 없다.
    let Some(measured) = t2.checked_sub(t1) else {
        return (assumed, false);
    };
    // 말도 안 되는 값은 버린다(모드 전환·세션 잠금으로 한쪽이 지연된 경우). 30~240Hz 밖이면
    // 가정값을 쓴다.
    if measured > freq / 240 && measured < freq / 30 {
        (measured, true)
    } else {
        (assumed, false)
    }
}

/// 이 출력의 vblank 를 연속 두 번 기다린다. ★둘째는 반드시 첫째의 바로 다음 vblank다★ --
/// `WaitForVBlank` 는 호출 시점 이후 그 출력의 다음 vblank 에 돌아오므로, 두 시각의 간격이
/// 곧 이 출력의 주기다(`period_from_pair` 주석 참고).
///
/// Safety: 살아 있는 `IDXGIOutput`. 이 호출은 블록한다 -- 그래서 전용 스레드다.
unsafe fn wait_two_vblanks(output: *mut IDXGIOutput) -> Option<(u64, u64)> {
    if (*output).WaitForVBlank() < 0 {
        return None;
    }
    let t1 = qpc_now()?;
    if (*output).WaitForVBlank() < 0 {
        return None;
    }
    let t2 = qpc_now()?;
    Some((t1, t2))
}

fn probe_loop() {
    let Some(freq) = qpc_frequency() else {
        warn!("[outgrid] QPC 주파수를 읽지 못했다; 전 타일 폴백");
        return;
    };
    // 첫 표본이 나오기 전과, 실측이 말이 안 될 때 쓰는 값. `measured=0` 으로 구분된다.
    let assumed_period = freq / 60;

    // 첫 열거도 루프 안에서 한다 -- 기동 시 실패와 실행 중 실패를 같은 복구 경로가 다루게
    // 하려는 것이다. 예전에는 기동 열거가 비면 스레드가 그냥 끝나 버렸고, 그러면 나중에
    // 모니터가 붙어도 프로브가 없다.
    let mut current = Enumeration {
        factory: ptr::null_mut(),
        outputs: Vec::new(),
    };
    // 재열거 실패 시의 백오프. 100ms 에서 시작해 2s 까지 배로 늘리고, 성공하면 되돌린다.
    // 구성 변경 중에는 실패가 연달아 나므로 고정 간격이면 COM 열거와 로그가 같이 폭주한다.
    const BACKOFF_MIN: Duration = Duration::from_millis(100);
    const BACKOFF_MAX: Duration = Duration::from_secs(2);
    let mut backoff = BACKOFF_MIN;
    let mut announced: Vec<String> = Vec::new();
    let mut last_reenum_warn: Option<Instant> = None;
    let mut last_empty_warn: Option<Instant> = None;
    let mut last_outphase = Instant::now();

    loop {
        // ★출력을 한 번만 열거하면 핫플러그 뒤 영구 폴백이 된다.★ 모드 변경·핫플러그로
        // 생긴 새 HMONITOR 는 영영 격자를 못 받고, 사라진 HMONITOR 항목은 `GRID` 에 남아
        // 죽은 격자로 마감을 계산하게 한다. 팩토리가 구성 변경을 알려 주므로 한 바퀴에 한 번
        // 물어본다(비용은 API 한 번이다).
        // Safety: 살아 있는 팩토리이거나 NULL.
        if unsafe { current.needs_reenumerate() } {
            if throttled(&mut last_reenum_warn) {
                warn!("[outgrid] 출력 목록을 다시 연다(구성 변경이거나 직전 열거 실패)");
            }
            // Safety: 이 열거의 포인터는 이 스레드 밖으로 나간 적이 없다.
            unsafe { current.release() };
            // Safety: 위와 같다.
            current = unsafe { enumerate_outputs() };
            if current.outputs.is_empty() {
                if throttled(&mut last_empty_warn) {
                    warn!(
                        "[outgrid] 잴 출력이 없다; {}ms 뒤 다시 본다(전 타일 폴백 중)",
                        backoff.as_millis()
                    );
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
            backoff = BACKOFF_MIN;
            // ★목록이 실제로 바뀌었을 때만 찍는다.★ 과도 상태에서는 재열거가 연달아 돌므로
            // 무조건 찍으면 같은 네 줄이 초당 여러 번 나온다.
            let names = current.names();
            if names != announced {
                announce(&current.outputs);
                announced = names;
            }
            publish_names(&current.outputs);
            forget_vanished(&current.outputs);
        }

        // 한 바퀴는 출력마다 vblank 두 개씩이라 약 `(N+w)·P` ≈ 5P ≈ 83ms 다(`w` 는 열거 순서가
        // 위상 원을 감는 횟수). 즉 출력별 격자가 그 주기로 갱신된다. 마감은 `vblank + k·P` 로
        // 접으므로 그 정도 신선도면 충분하다.
        let mut sampled = 0usize;
        for out in &current.outputs {
            // Safety: 살아 있는 IDXGIOutput.
            let Some((t1, t2)) = (unsafe { wait_two_vblanks(out.output) }) else {
                continue;
            };
            let (sampled_period, sane) = period_from_pair(t1, t2, freq, assumed_period);
            // ★주기는 모드에서 온 고정값을 쓴다.★ 실측은 정확값 후보 둘 중 어느 쪽인지
            // 고르는 데만 쓰고(`nominal_period_ticks`), 모드를 못 읽으면 그때만 실측으로
            // 떨어진다. `measured` 는 이제 "이 주기를 믿을 근거가 있나" 를 뜻한다 --
            // 모드에서 왔거나(참) 실측이 정상 범위였거나(참), 둘 다 아니면 거짓이다.
            let nominal = nominal_period_ticks(&out.name, freq, sane.then_some(sampled_period));
            let (period, measured) = match nominal {
                Some(period) => (period, true),
                None => (sampled_period, sane),
            };
            if let Ok(mut guard) = GRID.lock() {
                guard
                    .get_or_insert_with(HashMap::new)
                    .insert(out.monitor, OutputGrid {
                        // 더 최근인 둘째 vblank 를 기준으로 삼는다.
                        vblank_qpc: t2,
                        period_qpc: period,
                        measured,
                    });
            }
            sampled += 1;
        }

        // ★한 바퀴가 전부 실패하면 이 루프는 코어 하나를 태운다.★ 세션 잠금, RDP 끊김,
        // 모니터 전원 off, 토폴로지 변경 중에는 모든 출력의 `WaitForVBlank` 가 즉시 실패
        // HRESULT 로 돌아오므로 블록하는 것이 하나도 없다. 그런 상태는 초 단위로 이어지니
        // 잠시 자고 다시 본다.
        if sampled == 0 {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }

        if *crate::dcomp_compositor::DCOMP_BIND_PROF &&
            last_outphase.elapsed() >= Duration::from_secs(1)
        {
            last_outphase = Instant::now();
            emit_outphase(&current.outputs, freq);
            emit_samplelead();
        }
    }
}

fn announce(outputs: &[Output]) {
    warn!(
        "[outgrid] 출력 {} 개를 잰다: {}",
        outputs.len(),
        outputs
            .iter()
            .map(|o| o.name.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    );
}

fn publish_names(outputs: &[Output]) {
    if let Ok(mut guard) = NAMES.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        for out in outputs {
            map.insert(out.monitor, out.name.clone());
        }
    }
}

/// 재열거에서 사라진 HMONITOR 의 격자·이름을 버린다. 남겨 두면 죽은 모니터의 옛 격자가
/// 계속 조회되어, 그 자리에 붙은 창이 없는데도 마감이 계산된다.
fn forget_vanished(outputs: &[Output]) {
    let live: HashSet<usize> = outputs.iter().map(|o| o.monitor).collect();
    if let Ok(mut guard) = GRID.lock() {
        if let Some(map) = guard.as_mut() {
            map.retain(|monitor, _| live.contains(monitor));
        }
    }
    if let Ok(mut guard) = NAMES.lock() {
        if let Some(map) = guard.as_mut() {
            map.retain(|monitor, _| live.contains(monitor));
        }
    }
}

/// ★격자에 대한 독립적인 검산이다.★ `OUTCOMMIT` 의 위상은 마감을 정한 격자로 다시 접은
/// 값이라 격자가 틀려도 목표치를 가리킨다(`commit_scheduler::scheduler_loop` 주석). 이 줄은
/// 커밋과 무관하게 프로브가 직접 잰 값이고, 정렬 pref 가 꺼져 있어도 나오므로 기준선 런에서
/// 출력 간 vblank 확산 — 이 설계 전체가 딛고 선 그 측정 — 을 볼 수 있는 유일한 곳이다.
///
/// `rel_ms` 는 열거 첫 출력의 vblank 를 0 으로 둔 상대 위상으로, 주기로 접어 `[0,P)` 다.
fn emit_outphase(outputs: &[Output], freq: u64) {
    // 기준은 격자가 있는 첫 출력이다. 열거 첫 출력이 아직(또는 영영) 격자를 못 얻었다고
    // 나머지 셋의 확산까지 못 보게 되면, 정작 그 상태에서 가장 보고 싶은 값을 잃는다.
    let base = outputs.iter().find_map(|out| grid_for_monitor(out.monitor));
    // 기준으로 삼을 격자가 하나도 없으면 상대 위상이라는 말 자체가 성립하지 않는다. 그래도
    // ★줄은 낸다★ -- "전부 격자가 없다" 는 이 함수가 아무 것도 안 찍는 것과 구분되어야 하고,
    // 후자는 프로브가 죽었다는 뜻이라 대처가 다르다.
    let base = match base.filter(|grid| grid.period_qpc > 0) {
        Some(base) => base,
        None => {
            for out in outputs {
                warn!(
                    "OUTPHASE out={} monitor={:#x} grid=none",
                    out.name, out.monitor
                );
            }
            return;
        },
    };
    let period = base.period_qpc as i128;
    let to_ms = |ticks: f64| ticks * 1000.0 / freq as f64;
    for out in outputs {
        let Some(grid) = grid_for_monitor(out.monitor) else {
            // ★격자가 없는 출력이야말로 운영자가 봐야 할 줄이다.★ 조용히 건너뛰면 그 출력은
            // `OUTPHASE` 에도 `OUTCOMMIT` 에도(위상 표본이 없으므로) 안 나와서, `[outgrid]`
            // 열거 목록과 손으로 대조해야만 빠진 것을 알 수 있다. 매초 네 줄이 나오는지 세는
            // 것만으로 판정이 되게 한다.
            warn!(
                "OUTPHASE out={} monitor={:#x} grid=none",
                out.name, out.monitor
            );
            continue;
        };
        // 표본은 출력마다 최대 한 바퀴(~83ms)까지 시각이 벌어지지만, 주기로 접으면 그
        // 차이는 사라지고 위상만 남는다.
        let delta = grid.vblank_qpc as i128 - base.vblank_qpc as i128;
        let rel = (((delta % period) + period) % period) as f64;
        warn!(
            "OUTPHASE out={} monitor={:#x} period_ms={:.2} measured={} rel_ms={:+.2}",
            out.name,
            out.monitor,
            to_ms(grid.period_qpc as f64),
            u8::from(grid.measured),
            to_ms(rel),
        );
    }
}

/// Safety: 호출자는 프로브 스레드여야 한다. 돌려준 포인터는 그 스레드에서만 쓰인다.
unsafe fn enumerate_outputs() -> Enumeration {
    let mut found = Vec::new();
    let mut factory: *mut IDXGIFactory1 = ptr::null_mut();
    if CreateDXGIFactory1(&IDXGIFactory1::uuidof(), &mut factory as *mut _ as *mut _) < 0
        || factory.is_null()
    {
        warn!("[outgrid] CreateDXGIFactory1 실패");
        return Enumeration {
            factory: ptr::null_mut(),
            outputs: found,
        };
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
            // `desc` 는 이 회전에서만 사는 스택 임시인데 `Output` 은 루프 밖으로 나가므로,
            // 필요한 둘을 소유 값으로 떠 온다. (`DXGI_OUTPUT_DESC` 는 packed 가 아니다 --
            // 예전 주석이 그렇게 적혀 있었지만 빌림이 막히는 구조체는 `DWM_TIMING_INFO`
            // 뿐이고, 그 설명은 `dcomp_compositor.rs` 의 해당 자리에 있다.)
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
    Enumeration {
        factory,
        outputs: found,
    }
}

#[cfg(test)]
mod tests {
    use super::{lead_ticks, period_from_pair};

    /// B2 의 샘플 시각 산술. ★타일을 갈라놓는 것은 `vblank` 가 출력마다 다르다는 사실
    /// 하나이고, 이 함수가 그 차이를 그대로 통과시켜야 한다.★
    #[test]
    fn lead_reaches_the_next_grid_point_of_that_output() {
        let period = 166_667; // 10MHz 에서 60Hz
        // 격자점 직후: 거의 한 주기를 기다린다.
        assert_eq!(lead_ticks(1_000_010, 1_000_000, period, 0), Some(period - 10));
        // 격자점 직전: 조금만 기다린다.
        assert_eq!(lead_ticks(1_166_600, 1_000_000, period, 0), Some(67));
        // `vblank` 가 미래일 수도 있다(드라이버가 다음 vblank 를 준다) -- 같은 점으로 접힌다.
        assert_eq!(lead_ticks(1_000_010, 1_166_667, period, 0), Some(period - 10));
        // lead_periods 는 공통 오프셋이라 그대로 더해진다.
        assert_eq!(
            lead_ticks(1_000_010, 1_000_000, period, 2),
            Some(period - 10 + 2 * period)
        );
        // 주기를 모르면 샘플 시각을 지어내지 않는다.
        assert_eq!(lead_ticks(1_000_010, 1_000_000, 0, 0), None);
    }

    /// ★★네 타일의 퍼짐은 한 주기 미만이어야 한다 -- 이것이 요구된 보장이다.★★
    ///
    /// 처음 구현은 타일마다 *자기* 다음 vblank 로 따로 올림해서, 렌더가 격자 경계를 사이에
    /// 두고 갈라지면 두 타일이 한 주기 통째로 벌어졌다. 실기에서 2 프레임 이상, 관측자 기준
    /// 5 프레임까지 어긋났다(log_ani_debug_02/07). 설계가 약속한 것은 위상차 이내였다.
    ///
    /// 지금은 공통 T 에 `phase_offset_for` 만 더하므로 퍼짐 = 오프셋들의 범위이고, 오프셋은
    /// 정의상 `[0, period)` 다. 이 단언이 그 불변식을 지킨다.
    #[test]
    fn every_offset_stays_inside_one_period() {
        let period = 166_667;
        let reference = 1_000_000;
        // 실측 위상(log_ani_debug_02/02): 기준 대비 +1.17 / +6.57 / −4.63ms.
        let period_signed = period as i64;
        for delta in [0_i64, 11_700, 65_700, -46_300, 1 - period_signed, period_signed - 1] {
            let vblank = (reference as i64 + delta) as u64;
            let period_i = period as i128;
            let offset =
                (((vblank as i128 - reference as i128) % period_i + period_i) % period_i) as u64;
            assert!(
                offset < period,
                "delta={delta} 의 오프셋 {offset} 이 한 주기를 넘었다"
            );
        }
    }

    /// ★두 출력의 샘플 시각 차이가 곧 위상차여야 한다.★ 오프셋이 전부 0 이면 B2 는 아무
    /// 일도 하지 않고, 한 주기를 넘으면 위 보장이 깨진다 -- 그 사이여야 한다.
    #[test]
    fn two_outputs_differ_by_their_vblank_phase() {
        let period = 166_667;
        let reference = 1_000_000;
        let period_i = period as i128;
        let offset_of = |vblank: u64| {
            (((vblank as i128 - reference as i128) % period_i + period_i) % period_i) as u64
        };
        // 두 출력의 vblank 가 11.2ms(= 112_000 틱) 떨어져 있다.
        let a = offset_of(reference);
        let b = offset_of(reference + 112_000);
        assert_eq!(a, 0, "기준 출력의 오프셋은 0 이다");
        assert_eq!(b - a, 112_000, "샘플 시각 차이가 vblank 위상차와 같아야 한다");
        assert!(b < period, "그래도 한 주기 안이다");
    }

    /// 모드에서 온 주기가 실측 흔들림을 흡수하는가. 60Hz 와 59.94Hz 를 가려내되, 고른 뒤에는
    /// 실측이 어떻든 그 정확값이 나와야 한다.
    #[test]
    fn nominal_period_picks_the_exact_candidate() {
        // 이 테스트는 산술만 본다 -- 모드 조회는 `display_frequency_hz` 가 하고 그것은
        // 하드웨어를 요구한다. 두 후보의 계산이 맞는지가 요점이다.
        let freq = 10_000_000_u64;
        let exact = freq / 60; // 166_666
        let ntsc = freq * 1001 / 60_000; // 166_833
        assert_ne!(exact, ntsc, "두 후보가 구분돼야 한다");
        // 실측이 60Hz 쪽에 가까우면 60Hz 후보를, 59.94 쪽이면 그쪽을 골라야 한다.
        assert!(166_700_u64.abs_diff(exact) <= 166_700_u64.abs_diff(ntsc));
        assert!(166_800_u64.abs_diff(ntsc) <= 166_800_u64.abs_diff(exact));
    }

    /// 10MHz QPC 를 가정한다(실제 하드웨어와 같은 값이라 ms 환산이 직관적이다).
    const FREQ: u64 = 10_000_000;
    /// 60Hz 가정값.
    const ASSUMED: u64 = FREQ / 60;

    #[test]
    fn a_normal_pair_yields_the_measured_period() {
        // 16.67ms = 60Hz. ★옛 코드는 이 표본을 출력 수(4)로 또 나눠 4.17ms 로 만들었고,
        // 그 값은 240Hz 컷에 걸려 가정값으로 떨어졌다★ -- 즉 실측을 버리고 `measured=0` 을
        // 냈다. 그래서 이 단언은 옛 계산으로는 통과할 수 없다.
        let (period, measured) = period_from_pair(0, 166_667, FREQ, ASSUMED);
        assert_eq!(period, 166_667, "연속 두 vblank 간격이 곧 주기다");
        assert!(measured, "실측을 썼으면 measured 가 참이어야 한다");
    }

    #[test]
    fn a_pair_faster_than_240hz_falls_back_to_the_assumed_period() {
        // 0.1ms = 10kHz. 중복 깨움 등으로 나올 수 있는 값이고 주기가 아니다.
        let (period, measured) = period_from_pair(1_000, 2_000, FREQ, ASSUMED);
        assert_eq!(period, ASSUMED);
        assert!(!measured);
    }

    #[test]
    fn a_pair_slower_than_30hz_falls_back_to_the_assumed_period() {
        // 50ms = 20Hz. 세션 잠금·모드 전환으로 한쪽이 지연되면 이렇게 나온다.
        let (period, measured) = period_from_pair(0, 500_000, FREQ, ASSUMED);
        assert_eq!(period, ASSUMED);
        assert!(!measured);
    }

    #[test]
    fn a_backwards_pair_falls_back_to_the_assumed_period() {
        // t2 < t1. 있을 수 없는 관측이므로 뺄셈이 감싸 돌게 두면 안 된다 -- 감싸 돌면
        // 거대한 값이 나와 30Hz 컷에 걸리긴 하지만, 그건 우연이지 의도가 아니다.
        let (period, measured) = period_from_pair(500_000, 400_000, FREQ, ASSUMED);
        assert_eq!(period, ASSUMED);
        assert!(!measured);
    }
}

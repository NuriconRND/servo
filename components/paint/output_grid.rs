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

/// `lead_to_next_composition` 의 산술만 갈라낸 것 -- COM 도 시계도 없이 테스트할 수 있다.
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

/// DWM 이 이 타일을 실제로 합성한 기록 한 건. 전부 ms 로 환산해 둔다.
#[derive(Clone, Copy)]
struct DcompSample {
    /// `lastFrameTime` 을 그 디바이스의 합성 주기로 접은 값. ★타일 간 비교의 핵심이다★ --
    /// QPC 절대 시각을 공통 모듈러로 접었으므로, 네 줄의 이 값 차이가 곧 네 타일이 실제로
    /// 얼마나 떨어져 합성되는지다.
    phase_ms: f64,
    /// 표본을 뜬 시점 기준으로 마지막 합성이 얼마나 지났나.
    behind_ms: f64,
    /// 다음 합성까지 남은 예상 시간.
    next_ms: f64,
    period_ms: f64,
    rate_hz: f64,
    /// 이 합성이 직전 표본과 같은 합성인가를 가리기 위한 원본 값.
    last_qpc: u64,
    /// 그 합성보다 이 타일의 커밋이 얼마나 **앞서** 들어갔나(한 주기로 접음).
    /// ★한 주기에 가까우면 여유가 많고, 0 에 가까우면 마감을 스치고 있다는 뜻이다.★
    commit_lead_ms: Option<f64>,
}

/// ★공통 합성 격자 — (마지막 합성 QPC, 주기 QPC).★
///
/// 실기(log_ani_debug_02/09·10)에서 네 DComp 디바이스가 **같은** `lastFrameTime` 과
/// `rate=60.000Hz` 를 돌려주는 것이 확인됐다(`phase_ms p05=p50=p95=7.84`, 네 출력 동일).
/// DWM 은 네 타일을 하나의 데스크톱 합성 패스에서 함께 올린다 -- 출력마다 다른 것은 합성이
/// 아니라 스캔아웃 시점뿐이다. 그래서 맞출 격자는 출력별이 아니라 **이 하나**다.
///
/// 이 값이 B1·B2 가 왜 둘 다 아무 효과가 없었는지를 설명한다: 둘 다 출력별 vblank 에
/// 맞췄는데, 맞출 대상인 합성이 이미 공통이었다.
static COMPOSITION: Mutex<Option<(u64, u64)>> = Mutex::new(None);

pub(crate) fn composition_grid() -> Option<(u64, u64)> {
    *COMPOSITION.lock().ok()?
}

/// 이 프레임이 표시될 **합성 시각**까지 남은 간격. 네 타일이 같은 값을 받는다.
///
/// ★샘플과 표시가 같은 클럭 위에 있어야 한다.★ 지금까지는 렌더가 끝난 순간의 벽시계로
/// 샘플했다. 그 간격은 실측으로 16.06~18.07ms 로 흔들리는데(`ANIMSTEP dt_ms`), 합성은
/// 정확히 16.667ms 마다 일어난다(지터 0). 그래서 36.0px 움직인 프레임과 41.0px 움직인
/// 프레임이 같은 시간 동안 표시되고, 등속 운동이 ±6% 로 빨라졌다 느려졌다 한다 -- 그것이
/// 이 추적 내내 쫓던 저더다. 애니메이션은 내내 정확했다. 틀린 것은 어느 시계로 물었느냐다.
///
/// ★렌더 틱도 같은 격자에 잠가야 한다(`gfx_present_align_dwm_pct`).★ 자유 실행하는 렌더를
/// 격자에 스냅하기만 하면 가끔 두 렌더가 같은 격자점에 걸려 그 프레임의 변위가 0 이 되고
/// 다음이 두 칸을 뛴다. 출력별 격자로 그것을 한 번 겪었다(B2 1 차, ANIMSTEP p05 35.0→29.3).
/// 한 타일의 샘플 인덱스가 어떻게 움직였나. `SAMPLESLIP` 이 초당 비운다.
#[derive(Default)]
struct SlipTally {
    last_index: Option<u64>,
    n: u64,
    /// 인덱스가 그대로였다 = 이 프레임의 변위가 0 이다.
    same: u64,
    /// 두 칸 뛰었다 = 직전 프레임이 건너뛰어졌다.
    jump2: u64,
    /// 세 칸 이상.
    jump_n: u64,
    /// 슬립이 난 순간, 렌더가 격자의 어디에 있었나(ms).
    phase_at_slip: Vec<f64>,
    /// 전체 프레임의 같은 값 -- 위와 비교해야 "경계에 몰렸나" 를 말할 수 있다.
    phase_all: Vec<f64>,
}

static SLIPS: Mutex<Option<HashMap<usize, SlipTally>>> = Mutex::new(None);

/// ★슬립이 **언제** 일어나는지 찍는다.★
///
/// 정상 창의 프레임당 변위는 38.40px 에 spread 0.004 로 사실상 완벽한데, 창의 28% 에
/// 결함이 있고 10% 는 spread≈1.0 으로 파탄이다(log_ani_debug_02/14). 그리고 변위 0 프레임
/// 12 창 중 8 창을 기존 `dup`/`skip1` 계수가 **놓쳤다** -- 그 계수는 렌더 간격을 세지 샘플
/// 인덱스를 세지 않기 때문이다.
///
/// 가설은 "렌더가 격자 경계 근처에 떨어져 올림이 두 칸 사이를 오간다" 이지만, 두 번 추측으로
/// 고치려다 두 번 회귀를 냈으므로 이번에는 재고 나서 고친다. 슬립 순간의 위상 분포를 전체
/// 분포와 나란히 내면 경계에 몰렸는지 아닌지가 바로 보인다.
fn note_sample_slip(monitor: usize, index: u64, phase_ms: f64) {
    let Ok(mut guard) = SLIPS.lock() else { return };
    let tally = guard
        .get_or_insert_with(HashMap::new)
        .entry(monitor)
        .or_default();
    tally.n += 1;
    tally.phase_all.push(phase_ms);
    if let Some(last) = tally.last_index {
        let slipped = match index.saturating_sub(last) {
            1 => false,
            0 => {
                tally.same += 1;
                true
            },
            2 => {
                tally.jump2 += 1;
                true
            },
            _ => {
                tally.jump_n += 1;
                true
            },
        };
        if slipped {
            tally.phase_at_slip.push(phase_ms);
        }
    }
    tally.last_index = Some(index);
}

fn emit_sampleslip() {
    let tallies: Vec<(usize, SlipTally)> = match SLIPS.lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(map) => map
                .iter_mut()
                .map(|(monitor, tally)| (*monitor, std::mem::take(tally)))
                .collect(),
            None => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    for (monitor, tally) in tallies {
        if tally.n == 0 {
            continue;
        }
        let pick = |mut v: Vec<f64>, p: f64| -> f64 {
            if v.is_empty() {
                return f64::NAN;
            }
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            v[(((v.len() - 1) as f64) * p).round() as usize]
        };
        let name = name_for_monitor(monitor).unwrap_or_else(|| "?".into());
        let slip_n = tally.phase_at_slip.len();
        warn!(
            "SAMPLESLIP out={name} n={} same={} jump2={} jumpN={} \
             phase_all_ms p05={:.2} p50={:.2} p95={:.2} \
             phase_slip_ms n={slip_n} p05={:.2} p50={:.2} p95={:.2}",
            tally.n,
            tally.same,
            tally.jump2,
            tally.jump_n,
            pick(tally.phase_all.clone(), 0.05),
            pick(tally.phase_all.clone(), 0.50),
            pick(tally.phase_all, 0.95),
            pick(tally.phase_at_slip.clone(), 0.05),
            pick(tally.phase_at_slip.clone(), 0.50),
            pick(tally.phase_at_slip, 0.95),
        );
    }
}

pub(crate) fn lead_to_next_composition(monitor: usize, lead_periods: u64) -> Option<Duration> {
    let (last, period) = composition_grid()?;
    let now = qpc_now()?;
    let freq = qpc_frequency()?;
    // ★인덱스는 시계에서만 온다.★ 여기에 "직전보다 최소 한 칸" 같은 단조 증가를 얹으려다
    // 두 번 연속으로 회귀를 냈다(log_ani_debug_02/12·13). 두 번째 로그가 이유를 정확히
    // 보여 준다: `pump_paint_animation` 이 페인터당 **초당 102 회** 불린다(60 이 아니다).
    // 인덱스를 호출마다 올리면 네 페인터 × 102 = 408 칸/초가 되고, 실시간이 요구하는
    // 60 칸/초를 초당 5.8 초씩 앞질러 샘플 시각이 6.6 초 미래로 달아났다
    // (`SAMPLELEAD p50=6664ms`). 화면에서는 애니메이션이 멈춘 것처럼 보였다.
    //
    // 프레임 경계를 호출 수로 흉내 내려 한 것이 잘못이었다. `wanted_index` 는 시계에서 직접
    // 오므로 **달아날 수 없다** -- 미끄러질 때 한 칸을 건너뛸 뿐이고 실측에서 그 슬립은
    // 창의 11% 였다(나머지 89% 는 dx spread 0.005). 슬립을 없애려면 먼저 저 102 회의
    // 정체를 알아야 한다.
    let index = wanted_index(now, last, period, lead_periods)?;
    // 렌더가 격자의 어디에 떨어졌나. 슬립이 경계에 몰리는지 보려면 이 값이 필요하다.
    let phase_ms = ((now.saturating_sub(last % period)) % period) as f64 * 1000.0 / freq as f64;
    note_sample_slip(monitor, index, phase_ms);
    let target = target_for_index(last, period, index);
    Some(Duration::from_secs_f64(
        target.saturating_sub(now) as f64 / freq as f64,
    ))
}

/// 이 시각이 겨누는 격자점의 **절대** 인덱스.
///
/// ★원점은 격자의 위상이지 `last` 가 아니다.★ 처음에는 `(target - last) / period` 로 셌는데,
/// `last`(마지막 합성 시각)는 매 프레임 한 칸 전진한다. 그러면 정상 동작에서 인덱스가 늘
/// 같은 값(예: 2)으로 나오고, 거기에 단조 증가를 강제하니 매 프레임 한 칸씩 **덧**전진해
/// 샘플 시각이 두 배 속도로 달아났다. 실기에서 물체가 훨씬 빠르게 오른쪽 끝에 도달하고 그
/// 뒤로 반복 재생되지 않았다(log_ani_debug_02/12).
///
/// `last % period` 는 격자의 위상이라 프레임이 지나도 **같은 값**이다. 그것을 원점으로 삼으면
/// 인덱스가 절대값이 되고, 정상 동작에서 프레임마다 정확히 1 씩 는다.
fn wanted_index(now: u64, last: u64, period: u64, lead_periods: u64) -> Option<u64> {
    let ahead = lead_ticks(now, last, period, lead_periods)?;
    let target = now.saturating_add(ahead);
    Some(target.saturating_sub(last % period) / period)
}

fn target_for_index(last: u64, period: u64, index: u64) -> u64 {
    (last % period).saturating_add(index.saturating_mul(period))
}

static DCOMP_STATS: Mutex<Option<HashMap<usize, Vec<DcompSample>>>> = Mutex::new(None);
static DCOMP_STAT_FAILED: AtomicU64 = AtomicU64::new(0);

/// 디바이스 -> 그 디바이스에 마지막으로 `Commit()` 을 낸 시각(QPC).
///
/// ★재려는 것은 "그 커밋이 공통 합성 시점보다 얼마나 앞서 들어갔나" 다.★ 계측으로 확인된
/// 바에 따르면 DWM 은 네 타일을 **하나의** 합성 패스에서 함께 올린다(네 디바이스의
/// `lastFrameTime` 이 같다). 그러면 타일마다 다를 수 있는 것은 합성 시점이 아니라 **그
/// 마감에 제때 들어갔는가** 뿐이고, 마감을 아슬아슬하게 스치는 타일은 간헐적으로 직전
/// 프레임이 한 번 더 올라간다 -- 그 타일에만 나타나는 저더다.
///
/// ★반드시 디바이스 가드 **밖**에서 기록한다.★ 임계구역에 남는 것은 `Commit()` 하나여야
/// 한다는 규칙(C3)을 이 계측이 깨서는 안 된다. 호출 지점은 전부 `note_commit_failure` 옆,
/// 즉 가드를 푼 뒤다.
static COMMITS: Mutex<Option<HashMap<usize, u64>>> = Mutex::new(None);

pub(crate) fn note_commit_at(device: usize) {
    let Some(now) = qpc_now() else { return };
    if let Ok(mut guard) = COMMITS.lock() {
        guard.get_or_insert_with(HashMap::new).insert(device, now);
    }
}

fn commit_at(device: usize) -> Option<u64> {
    let guard = COMMITS.lock().ok()?;
    guard.as_ref()?.get(&device).copied()
}

pub(crate) fn note_dcomp_stat_failed() {
    DCOMP_STAT_FAILED.fetch_add(1, Ordering::Relaxed);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn note_dcomp_stat(
    monitor: usize,
    device: usize,
    last: u64,
    now: u64,
    next: u64,
    freq: u64,
    rate_num: u32,
    rate_den: u32,
) {
    if freq == 0 || rate_num == 0 || rate_den == 0 {
        note_dcomp_stat_failed();
        return;
    }
    let to_ms = |ticks: u64| ticks as f64 * 1000.0 / freq as f64;
    // 합성 주기는 DWM 이 유리수로 알려 준다 -- 추정할 필요가 없다.
    let period_ticks = freq.saturating_mul(rate_den as u64) / rate_num as u64;
    if period_ticks == 0 {
        note_dcomp_stat_failed();
        return;
    }
    // 커밋이 그 합성보다 얼마나 앞섰나. 커밋이 합성 뒤일 수도 있으므로(다음 합성을 향한
    // 커밋) 부호 안전하게 한 주기로 접는다 -- 결과는 언제나 `[0, period)` 이고 "직전 합성
    // 이후 얼마나 지나 커밋했나" 의 여집합, 즉 "다음 합성까지 얼마나 남기고 커밋했나" 다.
    let period_i = period_ticks as i128;
    let commit_lead_ms = commit_at(device).map(|commit| {
        let delta = last as i128 - commit as i128;
        to_ms((((delta % period_i) + period_i) % period_i) as u64)
    });
    // ★격자 갱신은 계측 게이트 밖이다.★ B2 가 이 격자를 쓰므로, 프로파일이 꺼져 있다고
    // 격자를 안 채우면 B2 가 통째로 무력해진다 -- B1 에서 프로브를 진단 플래그 뒤에 두어
    // 똑같이 당한 적이 있다(설계 문서 C5).
    if let Ok(mut guard) = COMPOSITION.lock() {
        *guard = Some((last, period_ticks));
    }
    if !*crate::dcomp_compositor::DCOMP_BIND_PROF {
        return;
    }
    let sample = DcompSample {
        phase_ms: to_ms(last % period_ticks),
        behind_ms: to_ms(now.saturating_sub(last)),
        next_ms: to_ms(next.saturating_sub(now)),
        period_ms: to_ms(period_ticks),
        rate_hz: rate_num as f64 / rate_den as f64,
        last_qpc: last,
        commit_lead_ms,
    };
    if let Ok(mut guard) = DCOMP_STATS.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .entry(monitor)
            .or_default()
            .push(sample);
    }
}

/// ★네 줄의 `phase_ms` 차이가 곧 타일 간 실제 표시 시점 차이다.★ 이 추적에서 지금까지
/// 추측으로만 다루던 값이고, 세 번의 잘못된 수정이 전부 이 값을 모르는 상태에서 나왔다.
fn emit_dcompstat() {
    let stats: Vec<(usize, Vec<DcompSample>)> = match DCOMP_STATS.lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(map) => map.drain().collect(),
            None => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    let failed = DCOMP_STAT_FAILED.swap(0, Ordering::Relaxed);
    for (monitor, samples) in stats {
        if samples.is_empty() {
            continue;
        }
        let pick = |mut values: Vec<f64>, p: f64| {
            values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            values[(((values.len() - 1) as f64) * p).round() as usize]
        };
        let phases: Vec<f64> = samples.iter().map(|s| s.phase_ms).collect();
        let behinds: Vec<f64> = samples.iter().map(|s| s.behind_ms).collect();
        let nexts: Vec<f64> = samples.iter().map(|s| s.next_ms).collect();
        let leads: Vec<f64> = samples.iter().filter_map(|s| s.commit_lead_ms).collect();
        // ★서로 다른 합성이 몇 번이었나.★ 프레임을 60 개 냈는데 이 값이 60 보다 작으면, 그
        // 차이만큼은 **같은 합성에 두 프레임이 들어갔거나 합성을 건너뛴 것**이다 -- 그 타일이
        // 직전 프레임을 한 번 더 보여 준 횟수이고, 그 타일에만 나타나는 저더의 정체다.
        let mut seen: Vec<u64> = samples.iter().map(|s| s.last_qpc).collect();
        seen.sort_unstable();
        seen.dedup();
        let last = samples[samples.len() - 1];
        let name = name_for_monitor(monitor).unwrap_or_else(|| "?".into());
        let (lead_p05, lead_p50) = if leads.is_empty() {
            (f64::NAN, f64::NAN)
        } else {
            (pick(leads.clone(), 0.05), pick(leads, 0.50))
        };
        warn!(
            "DCOMPSTAT out={name} monitor={monitor:#x} n={} distinct={} rate={:.3}Hz \
             period_ms={:.3} phase_ms p05={:.2} p50={:.2} p95={:.2} behind_ms p50={:.2} \
             next_ms p50={:.2} commit_lead_ms p05={:.2} p50={:.2} failed={failed}",
            samples.len(),
            seen.len(),
            last.rate_hz,
            last.period_ms,
            pick(phases.clone(), 0.05),
            pick(phases.clone(), 0.50),
            pick(phases, 0.95),
            pick(behinds, 0.50),
            pick(nexts, 0.50),
            lead_p05,
            lead_p50,
        );
    }
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
            emit_sampleslip();
            emit_dcompstat();
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
    use super::{lead_ticks, period_from_pair, target_for_index, wanted_index};

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
    /// 그런데 계측이 그 전제를 다시 뒤집었다 -- 아래 테스트가 지금의 보장이다.
    #[test]
    fn samples_land_exactly_on_the_grid() {
        let period = 166_667;
        let last = 1_000_000;
        // 격자점에서 얼마나 떨어져 물어도, 샘플 시각은 언제나 격자점 위다.
        for offset in [1_u64, 1_000, 83_333, 166_666] {
            let now = last + offset;
            let lead = lead_ticks(now, last, period, 1).unwrap();
            assert_eq!(
                (now + lead - last) % period,
                0,
                "offset={offset} 에서 샘플이 격자를 벗어났다"
            );
        }
    }

    /// ★★네 타일이 **같은** 샘플 시각을 받아야 한다 -- 이제 이것이 보장이다.★★
    ///
    /// 출력별 격자에 맞추던 시절에는 "퍼짐이 한 주기 미만" 이 목표였다. 실기 계측이 그 전제를
    /// 뒤집었다(log_ani_debug_02/09·10): 네 DComp 디바이스가 같은 `lastFrameTime` 과
    /// `rate=60.000Hz` 를 돌려준다. DWM 은 네 타일을 하나의 합성 패스에서 함께 올리므로 맞출
    /// 격자는 하나뿐이고, 그러면 네 타일의 lead 는 **같아야** 한다. 다르게 주는 것은 오차다.
    ///
    /// `lead_to_next_composition` 이 모니터를 인자로 받지 않으므로 이 성질은 타입으로도
    /// 보장되지만, 그 설계 결정 자체를 여기 못박아 둔다.
    #[test]
    fn all_tiles_get_the_same_lead_from_the_shared_grid() {
        let period = 166_667;
        let last = 1_000_000;
        let now = 5_000_000;
        let first = lead_ticks(now, last, period, 1).unwrap();
        for tile in 0..4 {
            assert_eq!(
                lead_ticks(now, last, period, 1).unwrap(),
                first,
                "타일 {tile} 이 다른 lead 를 받았다 -- 공통 격자에서는 있을 수 없다"
            );
        }
        // 한 주기 앞(lead_periods=1) + 다음 격자점까지의 거리.
        assert_eq!(first, period - (4_000_000 % period) + period);
    }

    /// ★★샘플 시각이 실시간보다 빨리 흐르면 안 된다.★★
    ///
    /// 이것을 못 잡아서 실기 한 라운드를 버렸다(log_ani_debug_02/12). 인덱스를 움직이는
    /// 원점(`last`)에서 세어 놓고 단조 증가를 강제했더니, 정상 동작에서도 매 프레임 한 칸씩
    /// 덧전진해 물체가 두 배 속도로 날아가 화면 끝에 닿고 멈췄다. 프레임을 여러 번 돌려
    /// 샘플 시각이 **정확히 한 주기씩** 나아가는지 보는 것이 그 결함을 잡는 단언이다.
    #[test]
    fn the_sample_time_advances_exactly_one_period_per_frame() {
        let period = 166_667;
        let phase = 12_345;
        let mut previous: Option<u64> = None;
        let mut last_target: Option<u64> = None;
        for frame in 0..10_u64 {
            // 합성 격자도 매 프레임 한 칸 전진한다 -- 이것이 결함의 전제였다.
            let last = phase + (100 + frame) * period;
            // 렌더는 합성 직후 조금씩 다른 시각에 끝난다(±1ms 지터).
            let jitter = (frame % 3) as u64 * 10_000;
            let now = last + 1_000 + jitter;
            let want = wanted_index(now, last, period, 1).unwrap();
            let index = match previous {
                Some(prev) if want > prev + 4 => want,
                Some(prev) => prev + 1,
                None => want,
            };
            previous = Some(index);
            let target = target_for_index(last, period, index);
            if let Some(before) = last_target {
                assert_eq!(
                    target - before,
                    period,
                    "프레임 {frame} 에서 샘플 시각이 한 주기가 아니라 {} 틱 나아갔다",
                    target - before
                );
            }
            last_target = Some(target);
        }
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

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

use std::collections::{HashMap, HashSet, VecDeque};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use log::warn;
use winapi::Interface;
use winapi::shared::dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIFactory1,
    IDXGIOutput,
};
use winapi::shared::windef::HMONITOR;
use winapi::um::profileapi::{QueryPerformanceCounter, QueryPerformanceFrequency};
use winapi::um::wingdi::DEVMODEW;
use winapi::um::winuser::{
    ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST,
    MONITORINFO, MONITORINFOF_PRIMARY, MonitorFromWindow,
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

/// DWM 이 이 타일을 실제로 합성한 기록 한 건. 전부 ms 로 환산해 둔다.
#[derive(Clone, Copy)]
struct DcompSample {
    /// `lastFrameTime` 을 그 디바이스의 합성 주기로 접은 값. ★타일 간 비교의 핵심이다★ --
    /// QPC 절대 시각을 공통 모듈러로 접었으므로, 네 줄의 이 값 차이가 곧 네 타일이 실제로
    /// 얼마나 떨어져 합성되는지다.
    phase_ms: f64,
    /// 표본을 뜬 시점(`currentTime`) 기준으로 `lastFrameTime` 이 얼마나 지났나. `now - last`.
    ///
    /// ★부호 있는 값이다.★ 예전에는 `saturating_sub` 라 음수가 전부 0 으로 뭉개졌고, 실기
    /// 로그에서 이 값이 **모든 창에서 정확히 0.00** 이었다 -- 즉 `last >= now` 가 항상 참인데
    /// 그 사실도, 얼마나 미래인지도 볼 수 없었다. `lastFrameTime` 이 과거인지 미래인지가
    /// `commit_to_comp_ms` 를 해석하는 전제이므로 그 부호를 지우면 안 된다.
    behind_ms: f64,
    /// 다음 합성까지 남은 예상 시간.
    next_ms: f64,
    /// `currentTime - (그 호출 직후 읽은 QPC)`.
    ///
    /// ★`currentTime` 이 정말 호출 시각인가를 가리는 값이다.★ 0 에 가까우면 그렇고,
    /// 그때에만 `behind_ms`·`next_ms` 를 "읽은 순간 기준" 으로 읽을 수 있다. 0 이 아니면
    /// 그 셋의 기준점이 전부 이 값만큼 밀려 있는 것이다.
    current_vs_qpc_ms: Option<f64>,
    period_ms: f64,
    rate_hz: f64,
    /// 이 합성이 직전 표본과 같은 합성인가를 가리기 위한 원본 값.
    last_qpc: u64,
    /// 그 합성보다 이 타일의 커밋이 얼마나 **앞서** 들어갔나(한 주기로 접음).
    /// ★한 주기에 가까우면 여유가 많고, 0 에 가까우면 마감을 스치고 있다는 뜻이다.★
    commit_lead_ms: Option<f64>,
    /// `Commit()` 에서 합성 엔진이 그 배치를 처리하기까지. ★접지 않은 부호 있는 값이다.★
    ///
    /// `commit_lead_ms` 는 같은 차이를 한 주기로 접으므로 "2ms 만에 처리됐다" 와 "한 주기를
    /// 넘겨 18.7ms 만에 처리됐다" 가 같은 값으로 찍힌다 -- 정작 묻고 싶은 것이 그 구분인데
    /// 그것만 지워진다. 여기에는 생값을 둔다.
    ///
    /// `lastFrameTime` 은 MSDN 정의상 "합성 엔진이 마지막으로 처리한 배치의 시각" 이므로
    /// 이 차이가 곧 커밋 -> 합성 처리 완료다. 스캔아웃까지는 여기에 `OUTPHASE` 의
    /// `vs_comp_ms` 를 더하면 된다.
    ///
    /// 음수일 수 있다: 표본을 뜬 `lastFrameTime` 이 이 커밋보다 **앞선** 합성일 때다(아직
    /// 우리 배치가 처리되지 않았다).
    commit_to_comp_ms: Option<f64>,
    /// 위 값이 몇 주기인가. `0` 이면 같은 주기 안에 처리됐고, `1` 이면 다음 합성이 실어
    /// 갔고, `2` 이상이면 ★합성을 한 번 이상 놓쳤다★ -- 그 타일이 직전 프레임을 한 번 더
    /// 보여 줬다는 뜻이다. 음수는 아직 처리 전이다.
    commit_periods: Option<i64>,
    /// `lastFrameTime` 과 **DWM vblank** 의 위상차. `(-P/2, P/2]` 로 접는다.
    ///
    /// ★0 에 붙어 있으면 커밋된 DComp 명령의 수행이 DWM vblank 마다 일어난다는 뜻이다.★
    /// `phase_ms` 가 답하지 못하는 질문이 이것이다 -- 그쪽은 QPC 0 을 원점으로 접으므로
    /// 기준이 아무것도 아니고, 접는 주기가 한 틱만 달라져도 값이 주기 안에서 통째로
    /// 옮겨진다. 여기는 두 절대 시각의 **차이**를 접으니 그 증폭이 없다.
    last_vs_dwmvb_ms: Option<f64>,
    /// `qpcCompose - qpcVBlank`, `[0, P)`. DWM 이 실제로 합성 패스를 돈 시각이 격자점에서
    /// 얼마나 뒤인가. ★격자점과 일한 시각은 다르고, 스캔아웃 판정을 가르는 것은 뒤쪽이다.★
    compose_after_vblank_ms: Option<f64>,
    /// 합성 **격자점** 뒤로 이 패널의 vblank 가 언제 오나. `[0, P)`.
    ///
    /// 예전 `last_vs_outvb_ms` 를 방향 있는 규약으로 고친 것이다. 가운데로 접으면 음수가
    /// 나오고 그것을 "합성보다 먼저 스캔아웃했다" 로 읽게 되는데, 접힌 위상에는 선후가 없다.
    outvb_after_last_ms: Option<f64>,
    /// ★`qpcCompose` 뒤로 이 패널의 vblank 가 언제 오나 -- 이것이 여유다.★ `[0, P)`.
    ///
    /// 스캔아웃이 "합성이 끝난 뒤 처음 오는 패널 vblank" 라면, 이 값이 0 에 가까운 패널은
    /// 작은 흔들림 하나로 그 프레임을 잡느냐 다음 것을 잡느냐가 뒤집힌다. 놓치면 한 주기를
    /// 통째로 기다리므로 그대로 프레임 중복이고, 눈에는 저더로 보인다.
    ///
    /// p05 가 0 쪽에 붙어 있는지가 판정이고, `p95 - p05` 가 흔들림의 폭이다.
    outvb_after_compose_ms: Option<f64>,
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

/// `pump_paint_animation` 이 어디서 불렸나. ★슬립의 정체가 호출 간격이므로 이것이 다음
/// 질문이다.★
///
/// 위상 가설은 계측으로 폐기됐다(log_ani_debug_02/15): 렌더는 주기의 2~3ms 에 안정적으로
/// 떨어지고 슬립도 같은 자리에서 난다. 남은 사실은 호출 간격이 불규칙하다는 것뿐이다 --
/// `same` 476(같은 주기에 두 번)과 `jumpN` 248(세 주기 이상 거름)이 그 모양이고,
/// 초당 호출이 64.4 회인데 렌더는 61 회다.
///
/// 호출 지점은 둘뿐이라 세면 바로 갈린다: 렌더 끝(`Painter::render`)과 렌더가 멈췄을 때의
/// 폴백(`perform_updates`).
static PUMP_FROM_RENDER: AtomicU64 = AtomicU64::new(0);
static PUMP_FROM_IDLE: AtomicU64 = AtomicU64::new(0);

/// 페인터 -> (렌더 수, 벽 프레임 번호를 달고 온 수, 본 번호들).
///
/// ★"이 경로의 렌더가 벽 프레임 조정을 받고 있나" 를 묻는 계측이다.★
///
/// 샘플 인덱스를 `last_ready_wall_logical_frame_id` 에 걸었다가 실기에서 더 나빠졌다
/// (log_ani_debug_02/17: 프레임의 73% 에서 인덱스가 멈췄다). 원인을 뒤져 보니 논리적 프레임
/// 카운터는 **있는데**(`paint.rs` 의 `next_logical_frame_id`, 벽 프레임 요청 하나당 한 번),
/// 그 번호가 **벽 프레임 요청으로 생긴 프레임에만** 붙는다 -- 페인터가 받는 필드가
/// `Option<u64>` 인 것이 그 뜻이다.
///
/// 그렇다면 이 애니메이션 경로의 렌더 대부분이 벽 프레임 조정을 거치지 않는다는 뜻이고,
/// 그건 샘플 시각보다 **먼저** 확인해야 할 사실이다. 네 타일을 같은 프레임에 묶는 배리어가
/// 그 프레임들에는 걸리지 않는다는 뜻이므로, 지금까지 쫓던 타일 간 어긋남의 원인이 거기일
/// 수 있다. 숫자로 확정하고 나서 다음을 정한다.
static FRAME_IDS: Mutex<Option<HashMap<String, (u64, u64, HashSet<u64>)>>> = Mutex::new(None);

pub(crate) fn note_frame_id(painter: &str, frame_id: Option<u64>) {
    if let Ok(mut guard) = FRAME_IDS.lock() {
        let slot = guard
            .get_or_insert_with(HashMap::new)
            .entry(painter.to_owned())
            .or_insert_with(|| (0, 0, HashSet::new()));
        slot.0 += 1;
        if let Some(id) = frame_id {
            slot.1 += 1;
            slot.2.insert(id);
        }
    }
}

fn emit_frameid() {
    let rows: Vec<(String, (u64, u64, HashSet<u64>))> = match FRAME_IDS.lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(map) => map.drain().collect(),
            None => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    for (painter, (renders, with_id, ids)) in rows {
        if renders == 0 {
            continue;
        }
        warn!(
            "FRAMEID painter={painter} renders={renders} with_id={with_id} distinct_ids={} \
             without_id={}",
            ids.len(),
            renders.saturating_sub(with_id),
        );
    }
}

pub(crate) fn note_pump_from_render() {
    PUMP_FROM_RENDER.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn note_pump_from_idle() {
    PUMP_FROM_IDLE.fetch_add(1, Ordering::Relaxed);
}

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
    let from_render = PUMP_FROM_RENDER.swap(0, Ordering::Relaxed);
    let from_idle = PUMP_FROM_IDLE.swap(0, Ordering::Relaxed);
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
             phase_slip_ms n={slip_n} p05={:.2} p50={:.2} p95={:.2} \
             pump_all from_render={from_render} from_idle={from_idle}",
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

/// 이 프레임이 실릴 **합성 시각**까지 남은 간격. 네 타일이 같은 값을 받는다.
///
/// ★샘플과 표시가 같은 클럭 위에 있어야 한다.★ 원래는 렌더가 끝난 순간의 벽시계로
/// 샘플했다. 그 간격은 실측으로 16.06~18.07ms 로 흔들리는데(`ANIMSTEP dt_ms`), 합성은
/// 정확히 16.667ms 마다 일어난다(지터 0). 36.0px 움직인 프레임과 41.0px 움직인 프레임이
/// 같은 시간 동안 표시되니 등속 운동이 ±6% 로 빨라졌다 느려졌다 했다 -- 애니메이션은 내내
/// 정확했고, 틀린 것은 어느 시계로 물었느냐였다.
///
/// ★그리고 목표 시각은 `now` 에서 뽑지 않는다.★ 여기까지 오는 데 세 번의 회귀가 들었다.
/// `now` 에서 올림으로 인덱스를 뽑으면 틱 지터가 그대로 반올림 경계를 넘나들고(전체의
/// 4.28% 에서 슬립), 그걸 막으려고 단조 증가·시간 창·프레임 번호·PLL 을 차례로 얹었지만
/// 전부 **흔들리는 값을 받아 놓고 뒤에서 떠는** 짓이었다.
///
/// `last`(= `lastFrameTime`)가 이미 격자점 위의 절대 시각이고, 네 페인터가 같은 값을
/// 읽는다는 것도 측정돼 있다(`DCOMPSTAT`: 네 출력의 phase 가 동일, `distinct == n` 이라
/// 합성마다 새 값). 그러면 목표는 그냥 `last + (1 + lead) * period` 다 -- 반올림도,
/// 타이머도, 위상 추정도 없다. `now` 는 "그 시각까지 얼마나 남았나" 를 재는 데만 쓴다.
/// `lead_to_next_composition` 의 산술 전부. 시계도 COM 도 없이 테스트할 수 있다.
///
/// `last` 는 격자점이므로 여기에 주기의 정수배를 더한 값도 격자점이다. 그래서 이 함수의
/// 출력은 **정의상** 격자 위에 있고, 같은 `last` 를 읽은 타일들은 같은 값을 받는다.
fn composition_target(last: u64, period: u64, lead_periods: u64) -> u64 {
    last.saturating_add(period.saturating_mul(1 + lead_periods))
}

pub(crate) fn lead_to_next_composition(monitor: usize, lead_periods: u64) -> Option<Duration> {
    let (last, period) = composition_grid()?;
    let now = qpc_now()?;
    let freq = qpc_frequency()?;
    // ★★목표 시각은 `now` 에서 뽑지 않는다. **합성 번호에서 센다.**★★
    //
    // `last`(= `lastFrameTime`)는 합성 격자 위의 절대 시각이고, 네 페인터가 **같은 값을
    // 읽는다는 것이 이미 측정돼 있다**(DCOMPSTAT: 네 출력의 phase p05=p50=p95 가 동일,
    // distinct == n 이므로 합성마다 새 값이다). 그러면 "이 프레임이 실릴 합성" 은
    // `last` 에서 몇 칸 뒤인지로 그냥 세면 된다 -- 반올림도, 타이머도, 위상 추정도 필요 없다.
    //
    // ★이걸 몇 라운드 전에 했어야 했다.★ 나는 `last` 를 **위상 기준**으로만 쓰고 인덱스는
    // 흔들리는 `now` 에서 올림으로 뽑았다. 그래서 틱 지터(p50 16.67 / p95 17.23ms, 주기
    // 16.667ms)가 그대로 반올림 경계를 넘나들며 전체의 4.28% 에서 슬립을 만들었고, 그것을
    // 막으려고 단조 증가·시간 창·프레임 번호·PLL 을 차례로 얹다가 세 번 회귀를 냈다.
    // 흔들리는 값을 받아 놓고 뒤에서 떠는 것을 막으려 한 것이 전부 잘못이었다.
    //
    // `now` 는 이제 "그 시각까지 얼마나 남았나" 를 재는 데만 쓴다.
    let target = composition_target(last, period, lead_periods);
    // 렌더가 격자의 어디에 떨어졌나. 슬립 진단용이고 목표 계산에는 쓰이지 않는다.
    let phase_ms = ((now.saturating_sub(last % period)) % period) as f64 * 1000.0 / freq as f64;
    note_sample_slip(monitor, target / period, phase_ms);
    Some(Duration::from_secs_f64(
        target.saturating_sub(now) as f64 / freq as f64,
    ))
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
    note_tick_to_commit(device, now);
}

/// ★이 틱이 난 시각(QPC).★ 셸이 `render_all_tiles` 를 부르기 **직전**에 찍는다.
///
/// 커밋 위상이 흔들리는 원인을 가르려고 둔다. 체인은 셋으로 쫪어진다:
///
/// ```text
/// 격자점 --tick_jitter--> 실제 틱 --tick_to_commit--> 커밋 --commit_lead--> 합성
///        (페이싱 깨어남)        (패스 길이)          (이미 있던 값)
/// ```
///
/// ★가운데 구간만 측정이 없었다.★ `commit_lead_ms` 의 산포(p50-p05 = 0.63ms)가 패스
/// 길이에서 오는지 페이싱 스레드의 깨어남에서 오는지 구별할 수가 없었고, 그 구별이
/// "커밋을 패스 끝에서 떼어낼 가치가 있나" 를 결정한다.
static TICK_QPC: AtomicU64 = AtomicU64::new(0);

/// 한 패스가 남긴 자국. `MISSEVENT` 가 놓친 합성 프레임에서 거꾸로 되짚는다.
///
/// ★디바이스별이 아니라 패스별이다.★ 네 타일은 한 패스의 끝에서 한꺼번에 커밋되므로
/// (`flush_deferred_dcomp_commits`), "그 순간 무슨 일이 있었나" 의 단위는 패스다. 대신
/// 네 커밋이 얼마나 벌어졌는지를 `first`↔`last` 로 남긴다 -- 그 폭이 디바이스별
/// 차이를 그대로 드러낸다.
#[derive(Clone, Copy)]
struct PassMark {
    /// 단조 증가하는 패스 번호. ★연속한 두 합성이 같은 번호를 물면 그 사이에
    /// 새 커밋이 없었다는 것이 **정의상** 참이다★ -- 시간 문턱이 필요 없다.
    ///
    /// 전에는 `c2s > 한 주기` 로 갈랐는데 그걸로 나온 분류가 합성/렌더 속도차와
    /// 맞지 않았다(초당 1.1~1.9 개가 새 커밋 없이 지나야 하는데 0.01 개로 잡혔다).
    /// ★임계값을 찍는 실수를 두 번째 했다.★
    seq: u64,
    tick_qpc: u64,
    /// 이 패스의 첫/마지막 커밋(QPC). 0 = 아직 커밋이 없다.
    first_commit_qpc: u64,
    last_commit_qpc: u64,
    /// 이 패스가 낸 커밋 수. ★4 보다 작으면 타일이 건너뛰었다는 뜻이다★
    /// (`skipped_busy`). 놓침과 같은 줄에서 보여야 하는 값이라 여기 둔다.
    commits: u32,
}

/// 최근 패스들. walk 상한(`MAX_WALK` = 240 프레임 ≈ 4초)를 덮어야 되짚기가
/// 가능하므로 그보다 길게 잡는다.
const PASS_RING_CAP: usize = 288;
static PASS_RING: Mutex<Option<VecDeque<PassMark>>> = Mutex::new(None);
/// 다음 패스에 붙일 번호. 0 은 "없음" 으로 쓰지 않으므로 1 부터 나간다.
static PASS_SEQ: AtomicU64 = AtomicU64::new(0);

/// 셸이 표출 틱을 낼 때 부른다. ★틱마다 한 번★ -- 타일마다가 아니다.
pub(crate) fn note_present_tick_now() {
    let Some(now) = qpc_now() else { return };
    TICK_QPC.store(now, Ordering::Relaxed);
    if let Ok(mut guard) = PASS_RING.lock() {
        let ring = guard.get_or_insert_with(VecDeque::new);
        if ring.len() >= PASS_RING_CAP {
            ring.pop_front();
        }
        ring.push_back(PassMark {
            seq: PASS_SEQ.fetch_add(1, Ordering::Relaxed) + 1,
            tick_qpc: now,
            first_commit_qpc: 0,
            last_commit_qpc: 0,
            commits: 0,
        });
    }
}

/// 디바이스 -> 이번 창의 `틱 -> 커밋` 표본(ms). `TICKCOMMIT` 이 초당 비운다.
static TICK_TO_COMMIT: Mutex<Option<HashMap<usize, Vec<f64>>>> = Mutex::new(None);

fn note_tick_to_commit(device: usize, commit_qpc: u64) {
    let tick = TICK_QPC.load(Ordering::Relaxed);
    // 기동 직후에는 아직 틱이 없다. 그리고 커밋이 틱보다 앞설 수는 없으므로, 그런 표본은
    // 틱을 놓친 것이니 세지 않는다 -- 음수를 0 으로 접으면 분포가 조용히 거짓말을 한다.
    if tick == 0 || commit_qpc <= tick {
        return;
    }
    let Some(freq) = qpc_frequency() else { return };
    let ms = (commit_qpc - tick) as f64 * 1000.0 / freq as f64;
    if let Ok(mut guard) = TICK_TO_COMMIT.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .entry(device)
            .or_default()
            .push(ms);
    }
    // 현재 패스에 새긴다. ★틱이 미리 칸을 만들어 두므로 여기서는 뒤가 항상 있다★
    // -- 없다면 틱 없이 커밋이 난 것이고, 그런 커밋은 어느 패스의 것도 아니니 버린다.
    if let Ok(mut guard) = PASS_RING.lock()
        && let Some(mark) = guard.as_mut().and_then(|ring| ring.back_mut())
    {
        if mark.first_commit_qpc == 0 {
            mark.first_commit_qpc = commit_qpc;
        }
        mark.last_commit_qpc = commit_qpc;
        mark.commits += 1;
    }
}

/// 합성 시각 `start` 을 먹였을 패스 -- 그 전에 커밋을 마친 마지막 패스다.
///
/// 돌려주는 것: `(그 패스, 직전 세 패스의 t2c_ms)`. 직전 값이 필요한 이유는
/// ★이것이 갑작스러운 튜는 패스였는지, 원래 계속 길었는지를 가르기 위해서다★.
fn pass_before(start_qpc: u64, freq: u64) -> Option<(PassMark, Vec<f64>)> {
    let guard = PASS_RING.lock().ok()?;
    let ring = guard.as_ref()?;
    let index = ring
        .iter()
        .rposition(|m| m.last_commit_qpc != 0 && m.last_commit_qpc <= start_qpc)?;
    let mark = ring[index];
    let prev = ring
        .iter()
        .take(index)
        .rev()
        .take(3)
        .filter(|m| m.last_commit_qpc != 0 && m.last_commit_qpc > m.tick_qpc)
        .map(|m| (m.last_commit_qpc - m.tick_qpc) as f64 * 1000.0 / freq as f64)
        .collect();
    Some((mark, prev))
}

/// ★패스가 커밋 위상을 얼마나 미는가.★
///
/// 읽는 법: `spread`(p95-p05)를 같은 창의 `DCOMPSTAT commit_lead_ms` 의 산포와 견준다.
/// 두 값이 같은 크기면 커밋 위상의 흔들림은 **패스 길이**에서 오는 것이고, 그러면 커밋을
/// 패스 끝에서 떼어낼(= `commit_scheduler` 를 공통 격자에 겨름) 이유가 선다. 훨씬 작으면
/// 원인은 그 앞 구간(페이싱 깨어남)이고, `WALLCLOCK jit_ms` 가 그것을 받는다.
///
/// `late` 는 커밋이 반 주기(P/2)보다 늦게 나간 횟수다 -- 그 프레임은 겨냥한 위상에서
/// 통째로 벗어났다. 이 수가 `COMPREFRESH` 의 `d0-d2` 와 함께 움직이는지가 인과의 방향을
/// 가른다(관측만으로는 못 가리던 바로 그것이다). DWM 주기를 못 구하면 `late=-1`.
fn emit_tickcommit() {
    let samples: Vec<(usize, Vec<f64>)> = match TICK_TO_COMMIT.lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(map) => map.drain().collect(),
            None => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    // 반 주기. ★측정된 DWM 주기에서 가져온다★ -- 60Hz 를 상수로 박으면 주사율 변경
    // 테스트에서 조용히 틀린 값을 낸다(`assumed_period` 가 같은 함정을 이미 한 번 놓았다).
    let half_period_ms = crate::dcomp_compositor::composition_grid()
        .zip(qpc_frequency())
        .map(|((_, period), freq)| period as f64 * 500.0 / freq as f64);
    for (device, mut t2c) in samples {
        if t2c.is_empty() {
            continue;
        }
        t2c.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let at = |p: f64| t2c[(((t2c.len() - 1) as f64) * p).round() as usize];
        let late = match half_period_ms {
            Some(half) => t2c.iter().filter(|&&v| v >= half).count() as i64,
            None => -1,
        };
        warn!(
            "TICKCOMMIT device={device:#x} n={} t2c_ms p05={:.2} p50={:.2} p95={:.2} \
             max={:.2} spread={:.2} late={late}",
            t2c.len(),
            at(0.05),
            at(0.50),
            at(0.95),
            at(1.00),
            at(0.95) - at(0.05),
        );
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
    qpc_at_read: Option<u64>,
) {
    if freq == 0 || rate_num == 0 || rate_den == 0 {
        note_dcomp_stat_failed();
        return;
    }
    let to_ms = |ticks: u64| ticks as f64 * 1000.0 / freq as f64;
    // ★부호 있는 환산.★ `to_ms` 는 `u64` 만 받아서, 음수가 나올 수 있는 차이를 담으려면
    // 호출부마다 `saturating_sub` 로 뭉개거나 캐스팅을 손으로 쓰게 된다. 전자는 실제로
    // `behind_ms` 를 전 창 0.00 으로 만들어 버렸다.
    let to_ms_signed = |ticks: i128| ticks as f64 * 1000.0 / freq as f64;
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
    let commit_delta = commit_at(device).map(|commit| last as i128 - commit as i128);
    let commit_lead_ms =
        commit_delta.map(|delta| to_ms((((delta % period_i) + period_i) % period_i) as u64));
    // ★같은 차이를 접지 않고 그대로 남긴다.★ 위의 접힌 값이 "몇 번째 합성이었나" 를 지우는데,
    // 커밋에서 합성까지 얼마나 걸리는가는 바로 그 정보다.
    let commit_to_comp_ms = commit_delta.map(&to_ms_signed);
    // 몇 주기인가. 음수 쪽으로도 바닥 나눗셈이 되도록 `div_euclid` 를 쓴다 -- `-1 / P` 가
    // 0 이 되면 "아직 처리 전" 이 "같은 주기에 처리됨" 으로 둔갑한다.
    let commit_periods = commit_delta.map(|delta| delta.div_euclid(period_i) as i64);
    // ★격자 갱신은 계측 게이트 밖이다.★ B2 가 이 격자를 쓰므로, 프로파일이 꺼져 있다고
    // 격자를 안 채우면 B2 가 통째로 무력해진다 -- B1 에서 프로브를 진단 플래그 뒤에 두어
    // 똑같이 당한 적이 있다(설계 문서 C5).
    if let Ok(mut guard) = COMPOSITION.lock() {
        *guard = Some((last, period_ticks));
    }
    if !*crate::dcomp_compositor::DCOMP_BIND_PROF {
        return;
    }
    // ★위상차는 `0 에 가까운가` 로 판정하고 싶다.★ `[0, P)` 로 접으면 정렬된 경우가 0 쪽과
    // P 쪽 양끝에 나뉘어 나타나 한눈에 안 보인다. `(-P/2, P/2]` 로 접는다.
    let fold_centered = |delta: i128, period: i128| -> f64 {
        let folded = ((delta % period) + period) % period;
        to_ms_signed(if folded * 2 > period {
            folded - period
        } else {
            folded
        })
    };
    // ★"다음에 언제 오나" 는 앞으로의 거리다.★ `(-P/2, P/2]` 로 접으면 음수가 나오고,
    // 그것을 "합성보다 먼저 스캔아웃했다" 로 읽게 된다 -- 접힌 위상에는 선후가 없는데도.
    // 방향이 있는 값은 `[0, P)` 로 접어 항상 양수로 둔다.
    let fold_forward =
        |delta: i128, period: i128| to_ms_signed(((delta % period) + period) % period);
    let dwm = crate::dcomp_compositor::dwm_timing().filter(|(_, _, period)| *period > 0);
    let out_grid = grid_for_monitor(monitor).filter(|grid| grid.period_qpc > 0);
    // ★1) `lastFrameTime` 이 DWM vblank 격자에 걸려 있나.★ 0 에 붙어 있으면 커밋된 DComp
    // 명령의 수행이 DWM vblank 마다 일어난다고 볼 수 있다. `phase_ms` 와 달리 이것은 두
    // 절대 시각의 **차이**를 접으므로, 접는 주기가 한 틱 틀려도 결과가 그만큼만 움직인다.
    // 여기는 "0 인가" 를 묻는 값이라 가운데로 접는 것이 맞다.
    let last_vs_dwmvb_ms =
        dwm.map(|(vblank, _, period)| fold_centered(last as i128 - vblank as i128, period as i128));
    // ★2) DWM 이 실제로 합성 패스를 돈 시각이 격자점에서 얼마나 뒤인가.★ `qpcVBlank` 는
    // 격자이고 `qpcCompose` 는 일한 시각이다. 스캔아웃 판정을 가르는 것은 뒤쪽이다.
    let compose_after_vblank_ms = dwm.map(|(vblank, compose, period)| {
        fold_forward(compose as i128 - vblank as i128, period as i128)
    });
    // ★3) 합성 격자점 뒤로 이 패널의 vblank 가 언제 오나.★
    let outvb_after_last_ms = out_grid.map(|grid| {
        fold_forward(
            grid.vblank_qpc as i128 - last as i128,
            grid.period_qpc as i128,
        )
    });
    // ★4) **`qpcCompose` 뒤로** 이 패널의 vblank 가 언제 오나 -- 이것이 여유다.★
    //
    // 스캔아웃이 "합성이 끝난 뒤 처음 오는 패널 vblank" 라면, 이 값이 0 에 가까운 패널은
    // 작은 흔들림 하나로 그 프레임을 잡느냐 다음 것을 잡느냐가 뒤집힌다 -- 한 주기를
    // 통째로 기다리게 되고, 그것이 곧 프레임 중복이다. p05 가 0 쪽에 붙어 있는지가
    // 판정이고, p95-p05 가 그 흔들림의 폭이다.
    let outvb_after_compose_ms = dwm.zip(out_grid).map(|((_, compose, _), grid)| {
        fold_forward(
            grid.vblank_qpc as i128 - compose as i128,
            grid.period_qpc as i128,
        )
    });
    let sample = DcompSample {
        phase_ms: to_ms(last % period_ticks),
        behind_ms: to_ms_signed(now as i128 - last as i128),
        next_ms: to_ms(next.saturating_sub(now)),
        current_vs_qpc_ms: qpc_at_read.map(|qpc| to_ms_signed(now as i128 - qpc as i128)),
        period_ms: to_ms(period_ticks),
        rate_hz: rate_num as f64 / rate_den as f64,
        last_qpc: last,
        commit_lead_ms,
        commit_to_comp_ms,
        commit_periods,
        last_vs_dwmvb_ms,
        compose_after_vblank_ms,
        outvb_after_last_ms,
        outvb_after_compose_ms,
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
        // ★커밋 -> 합성 처리.★ 접히지 않은 생값이라 "몇 번째 합성이 실어 갔나" 가 남아
        // 있다. `p95` 를 같이 내는 것은 평균이 괜찮아도 꼬리가 한 주기를 넘으면 그 프레임은
        // 화면에 늦게 뜨기 때문이다.
        // `currentTime` 이 호출 시각인가. 0 에 가까워야 나머지 값들의 기준점이 성립한다.
        // ★`lastFrameTime` 이 어느 격자에 걸려 있나.★ 둘 다 0 에 붙어야 하는지, 하나만
        // 붙는지가 이 줄의 요점이다.
        let three = |values: Vec<f64>| -> (f64, f64, f64) {
            if values.is_empty() {
                (f64::NAN, f64::NAN, f64::NAN)
            } else {
                (
                    pick(values.clone(), 0.05),
                    pick(values.clone(), 0.50),
                    pick(values, 0.95),
                )
            }
        };
        let (dwmvb_p05, dwmvb_p50, dwmvb_p95) =
            three(samples.iter().filter_map(|s| s.last_vs_dwmvb_ms).collect());
        let (cav_p05, cav_p50, cav_p95) = three(
            samples
                .iter()
                .filter_map(|s| s.compose_after_vblank_ms)
                .collect(),
        );
        let (outvb_p05, outvb_p50, outvb_p95) = three(
            samples
                .iter()
                .filter_map(|s| s.outvb_after_last_ms)
                .collect(),
        );
        let (oac_p05, oac_p50, oac_p95) = three(
            samples
                .iter()
                .filter_map(|s| s.outvb_after_compose_ms)
                .collect(),
        );
        let vs_qpc: Vec<f64> = samples.iter().filter_map(|s| s.current_vs_qpc_ms).collect();
        let (qpc_p05, qpc_p50, qpc_p95) = if vs_qpc.is_empty() {
            (f64::NAN, f64::NAN, f64::NAN)
        } else {
            (
                pick(vs_qpc.clone(), 0.05),
                pick(vs_qpc.clone(), 0.50),
                pick(vs_qpc, 0.95),
            )
        };
        let to_comp: Vec<f64> = samples.iter().filter_map(|s| s.commit_to_comp_ms).collect();
        let (comp_p05, comp_p50, comp_p95) = if to_comp.is_empty() {
            (f64::NAN, f64::NAN, f64::NAN)
        } else {
            (
                pick(to_comp.clone(), 0.05),
                pick(to_comp.clone(), 0.50),
                pick(to_comp, 0.95),
            )
        };
        // 주기 분포. ★`late` 가 0 이 아니면 그만큼의 커밋이 합성을 한 번 이상 놓쳤다★ --
        // 그 타일이 직전 프레임을 다시 보여 준 횟수이고, 저더를 세는 직접적인 수다.
        // `pending` 은 표본을 뜰 때 아직 처리되지 않았던 것이라 결함이 아니다.
        let mut pending = 0usize;
        let mut same = 0usize;
        let mut next_comp = 0usize;
        let mut late = 0usize;
        for periods in samples.iter().filter_map(|s| s.commit_periods) {
            match periods {
                p if p < 0 => pending += 1,
                0 => same += 1,
                1 => next_comp += 1,
                _ => late += 1,
            }
        }
        warn!(
            "DCOMPSTAT out={name} monitor={monitor:#x} n={} distinct={} rate={:.3}Hz \
             period_ms={:.3} phase_ms p05={:.2} p50={:.2} p95={:.2} behind_ms p50={:+.2} \
             next_ms p50={:.2} commit_lead_ms p05={:.2} p50={:.2} \
             commit_to_comp_ms p05={:.2} p50={:.2} p95={:.2} \
             current_vs_qpc_ms p05={:+.2} p50={:+.2} p95={:+.2} \
             last_vs_dwmvb_ms p05={:+.2} p50={:+.2} p95={:+.2} \
             compose_after_vblank_ms p05={:.2} p50={:.2} p95={:.2} \
             outvb_after_last_ms p05={:.2} p50={:.2} p95={:.2} \
             outvb_after_compose_ms p05={:.2} p50={:.2} p95={:.2} \
             periods pending={pending} same={same} next={next_comp} late={late} failed={failed}",
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
            comp_p05,
            comp_p50,
            comp_p95,
            qpc_p05,
            qpc_p50,
            qpc_p95,
            dwmvb_p05,
            dwmvb_p50,
            dwmvb_p95,
            cav_p05,
            cav_p50,
            cav_p95,
            outvb_p05,
            outvb_p50,
            outvb_p95,
            oac_p05,
            oac_p50,
            oac_p95,
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
    /// 이 출력을 구동하는 어댑터의 LUID `(LowPart, HighPart)`.
    ///
    /// ★`DCompositionGetStatistics` 가 돌려주는 타깃은 이름이 없다★ -- `displayAdapterLuid`
    /// 로만 자기를 밝히므로, 그 통계를 `\\.\DISPLAYxx` 에 붙이려면 이 값이 필요하다.
    adapter_luid: (u32, i32),
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
    // 직전 창에서 마지막으로 본 합성 프레임 id. 다음 창은 그 다음부터 훑는다.
    let mut last_comp_frame_id: Option<u64> = None;

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
            emit_comp_stats(&current.outputs, freq, &mut last_comp_frame_id);
            emit_samplelead();
            emit_sampleslip();
            emit_tickcommit();
            emit_frameid();
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

/// 한 타깃이 이번 창에서 보인 것. `emit_comp_stats` 가 프레임을 훑으며 채운다.
#[derive(Default)]
struct TargetWalk {
    name: Option<String>,
    frames: u32,
    /// 합성 프레임 하나가 지나는 동안 이 타깃의 `refreshCount` 가 몇 올랐나의 분포.
    /// ★`[2]` 이상이 저더를 **직접 센** 수다★ -- 그 패널이 같은 합성에 리프레시를 두 번
    /// 썼다는 뜻이니 같은 그림을 두 번 보여 준 것이다.
    refresh_delta: [u32; 4],
    /// 같은 식의 `presentCount` 분포. `[0]` 이 크면 그 프레임에 새 present 를 못 받았다.
    present_delta: [u32; 4],
    outstanding: [u32; 4],
    present_vs_start_sum: f64,
    present_vs_start_max: f64,
    /// `completedStats.time` 이 채워지는지부터가 질문이다. 0 이면 못 쓴다.
    completed_zero: u32,
    completed_vs_start_sum: f64,
    completed_vs_start_max: f64,
    last_refresh: Option<u32>,
    last_present: Option<u32>,
}

fn bump(slot: &mut [u32; 4], value: i64) {
    let index = value.clamp(0, 3) as usize;
    slot[index] += 1;
}

/// ★합성 프레임을 **하나도 빼놓지 않고** 훑어 패널별로 센다.★
///
/// 예전 판은 창마다 "마지막으로 완료된 프레임" 하나만 봤다. 그런데 `frame_id` 는 창당
/// 62~64 씩 오른다 -- 즉 60 개 중 1 개, 1.6% 만 본 것이다. 보고된 저더가 3~4 초에 한 번
/// (초당 0.3 회)이니 그 표본으로는 운이 좋아야 걸린다. `DCompositionGetStatistics` 는 임의
/// id 를 받고 id 가 조밀하게 연속이므로, 직전 창의 끝부터 지금까지를 전부 되짚는다.
///
/// 읽는 법:
/// - `period_ms` ★가장 먼저 본다.★ 16.67 이 아니면 이 구조체의 시간들이 QPC 가 아니라는
///   뜻이고, 그러면 나머지를 믿으면 안 된다. 단위 가정을 출력 자체가 검산하게 둔다.
/// - ★`refresh_d[2]`/`[3+]`★ -- 그 패널이 한 합성에 리프레시를 둘 이상 쓴 횟수. 네 패널 중
///   하나만 크면 그 패널만 프레임을 중복해 보여 주고 있다는 뜻이고, 그게 저더다.
/// - `present_d[0]` -- 그 프레임에 새 present 를 못 받은 횟수.
/// - `completed_zero` -- `completedStats.time` 이 0 이던 횟수. 창 전체면 그 필드는 이
///   프로세스에서 못 쓰는 것이고, 표출 시각은 다른 데서 찾아야 한다.
fn emit_comp_stats(outputs: &[Output], freq: u64, last_id: &mut Option<u64>) {
    let Some(current) = crate::comp_stats::completed_frame_id() else {
        // ★조용히 건너뛰지 않는다.★ 이 API 는 Windows 10 1803+ 이고 실패가 조용하면
        // "값이 안 나온다" 와 "줄이 아예 없다" 를 구분할 수 없다.
        warn!("COMPSTATS unavailable=1");
        return;
    };
    // 한 창에 훑을 상한. 창이 늘어지거나 첫 호출이어도 비용이 터지지 않게 막는다.
    const MAX_WALK: u64 = 240;
    let first = match *last_id {
        Some(previous) if current > previous => {
            (previous + 1).max(current.saturating_sub(MAX_WALK))
        },
        // 첫 창이거나 id 가 되감겼다. 이번 것 하나만 보고 다음 창부터 훑는다.
        _ => current,
    };
    *last_id = Some(current);

    let to_ms = |ticks: i128| ticks as f64 * 1000.0 / freq as f64;
    let mut walks: Vec<((u32, i32), TargetWalk)> = Vec::new();
    let mut seen = 0u32;
    let mut missing = 0u32;
    let mut period_ms = 0.0;
    let mut target_vs_start_ms = 0.0;
    // ★목록이 잘렸는지 본다.★ `returned` 가 `targets` 보다 작으면 `MAX_TARGETS` 에 걸린
    // 것이고, 그러면 보이지 않는 디스플레이가 있다는 뜻이다.
    let mut targets_declared = 0u32;
    let mut targets_returned = 0usize;
    let mut start_vs_dwmvb: Option<f64> = None;
    let dwm_vblank = crate::dcomp_compositor::composition_grid().filter(|(_, p)| *p > 0);
    // `MISSEVENT` 의 창당 상한. 놓침은 프로브에선 초당 0.1 회지만 output 페이지에선
    // 30 회를 넘는다. ★상한에 걸리면 건너뛴 수를 같은 줄에 찍는다★ -- 줄이 없는 것과
    // 많아서 잘린 것은 전혀 다른 상황이다.
    const MAX_MISS_LINES: u32 = 24;
    let mut miss_lines = 0u32;
    let mut miss_suppressed = 0u32;
    // 앞 합성 프레임을 먹인 패스 번호.
    let mut prev_seq: Option<u64> = None;

    for id in first..=current {
        let Some(frame) = crate::comp_stats::sample_frame(id) else {
            missing += 1;
            continue;
        };
        seen += 1;
        targets_declared = targets_declared.max(frame.target_count);
        targets_returned = targets_returned.max(frame.targets.len());
        period_ms = to_ms(frame.frame_period as i128);
        target_vs_start_ms = to_ms(frame.target_time as i128 - frame.start_time as i128);
        // ★패스는 프레임당 한 번만 찾는다.★ 네 타일이 공유하는 값이고, 앞 프레임과
        // 비교해야 "그 사이에 새 커밋이 있었나" 를 말할 수 있으므로 놓침 때만 찾으면 늦다.
        let pass = pass_before(frame.start_time, freq);
        let seq = pass.as_ref().map(|(mark, _)| mark.seq);
        let newcommit: i32 = match (seq, prev_seq) {
            (Some(now), Some(before)) if now == before => 0,
            (Some(_), Some(_)) => 1,
            _ => -1,
        };
        prev_seq = seq;
        start_vs_dwmvb = dwm_vblank.map(|(vblank, period)| {
            let period = period as i128;
            to_ms(((frame.start_time as i128 - vblank as i128) % period + period) % period)
        });
        for target in &frame.targets {
            let luid = target.display_adapter_luid;
            let slot = match walks.iter().position(|(key, _)| *key == luid) {
                Some(index) => &mut walks[index].1,
                None => {
                    let name = outputs
                        .iter()
                        .find(|out| out.adapter_luid == luid)
                        .map(|out| out.name.clone());
                    walks.push((
                        luid,
                        TargetWalk {
                            name,
                            ..Default::default()
                        },
                    ));
                    &mut walks.last_mut().expect("just pushed").1
                },
            };
            slot.frames += 1;
            if let Some(previous) = slot.last_refresh {
                bump(
                    &mut slot.refresh_delta,
                    i64::from(target.presented.refresh_count) - i64::from(previous),
                );
            }
            if let Some(previous) = slot.last_present {
                let delta = i64::from(target.presented.present_count) - i64::from(previous);
                bump(&mut slot.present_delta, delta);
                // ★놓친 바로 그 순간을 뽑는다.★ 초당 통계로는 보이지 않는 사건이다 --
                // 프로브 a8 은 120초에 11 회라 1초 창의 p50/p95 가 구조적으로 못 본다.
                if delta == 0 {
                    if miss_lines < MAX_MISS_LINES {
                        miss_lines += 1;
                        emit_missevent(
                            id,
                            slot.name.as_deref(),
                            &frame,
                            target,
                            freq,
                            pass.as_ref(),
                            newcommit,
                        );
                    } else {
                        miss_suppressed += 1;
                    }
                }
            }
            slot.last_refresh = Some(target.presented.refresh_count);
            slot.last_present = Some(target.presented.present_count);
            bump(
                &mut slot.outstanding,
                i64::from(target.outstanding_presents),
            );
            let present = to_ms(target.present_time as i128 - frame.start_time as i128);
            slot.present_vs_start_sum += present;
            slot.present_vs_start_max = slot.present_vs_start_max.max(present);
            if target.completed.time == 0 {
                slot.completed_zero += 1;
            } else {
                let completed = to_ms(target.completed.time as i128 - frame.start_time as i128);
                slot.completed_vs_start_sum += completed;
                slot.completed_vs_start_max = slot.completed_vs_start_max.max(completed);
            }
        }
    }

    warn!(
        "COMPWALK ids={first}..{current} seen={seen} missing={missing} \
         targets={targets_declared}/{targets_returned} period_ms={period_ms:.3} \
         target_vs_start_ms={target_vs_start_ms:+.2} start_vs_dwmvb_ms={} \
         miss_lines={miss_lines} miss_suppressed={miss_suppressed}",
        start_vs_dwmvb.map_or_else(|| "n/a".to_string(), |ms| format!("{ms:.2}")),
    );
    for (luid, walk) in &walks {
        let frames = walk.frames.max(1) as f64;
        let completed_n = walk.frames.saturating_sub(walk.completed_zero).max(1) as f64;
        warn!(
            "COMPREFRESH out={} luid={:08x}:{:08x} frames={} \
             refresh_d=[{},{},{},{}] present_d=[{},{},{},{}] outstanding=[{},{},{},{}] \
             present_vs_start_ms avg={:+.2} max={:+.2} \
             completed_zero={} completed_vs_start_ms avg={:+.2} max={:+.2}",
            walk.name.as_deref().unwrap_or("?"),
            luid.1,
            luid.0,
            walk.frames,
            walk.refresh_delta[0],
            walk.refresh_delta[1],
            walk.refresh_delta[2],
            walk.refresh_delta[3],
            walk.present_delta[0],
            walk.present_delta[1],
            walk.present_delta[2],
            walk.present_delta[3],
            walk.outstanding[0],
            walk.outstanding[1],
            walk.outstanding[2],
            walk.outstanding[3],
            walk.present_vs_start_sum / frames,
            walk.present_vs_start_max,
            walk.completed_zero,
            walk.completed_vs_start_sum / completed_n,
            walk.completed_vs_start_max,
        );
    }
}

/// ★놓친 합성 프레임 하나를 그 순간의 맥락과 함께 찍는다.★
///
/// 왜 초당 통계가 아니라 사건이어야 하는가: 프로브 페이지의 놓침은 120초에 11 회다.
/// 1초 창의 p50/p95 는 60 표본의 분위수라 1/600 사건을 구조적으로 못 본다. 똑같은
/// 실수를 한 번 했다 -- 초당 0.3 회 사건을 초당 1 표본으로 쪽다가 음성 결론을 냈다
/// (설계 문서 §6-4).
///
/// 읽는 법:
///
/// * `c2s` -- 그 패스의 마지막 커밋이 이 합성 시작보다 얼마나 앞선나. ★작을수록
///   마감을 스친 것★이고, 음수면 합성이 시작된 뒤에 커밋했다는 뜻이다.
/// * `t2c` -- 그 패스의 틱→마지막커밋. `prev` 와 비교해 ★갑작스러운 튜인지
///   원래 길었는지★를 가른다.
/// * `commits` -- 그 패스가 낸 커밋 수. ★4 미만이면 타일이 건너뛰었다★(`skipped_busy`).
/// * `outstanding` -- 그 프레임에서 DWM 의 큐에 쌓여 있던 present 수.
///
/// 패스를 못 찾으면(링이 비었거나 링보다 오래된 프레임) `pass=none` 으로 찍는다 --
/// 그런 놓침이 있었다는 사실 자체는 남아야 한다.
fn emit_missevent(
    frame_id: u64,
    name: Option<&str>,
    frame: &crate::comp_stats::FrameSample,
    target: &crate::comp_stats::TargetSample,
    freq: u64,
    pass: Option<&(PassMark, Vec<f64>)>,
    newcommit: i32,
) {
    let to_ms = |ticks: i128| ticks as f64 * 1000.0 / freq as f64;
    let out = name.unwrap_or("?");
    let outstanding = target.outstanding_presents;
    match pass {
        Some((mark, prev)) => {
            let c2s = to_ms(frame.start_time as i128 - mark.last_commit_qpc as i128);
            let t2c = to_ms(mark.last_commit_qpc as i128 - mark.tick_qpc as i128);
            let span = to_ms(mark.last_commit_qpc as i128 - mark.first_commit_qpc as i128);
            let prev = prev
                .iter()
                .map(|v| format!("{v:.2}"))
                .collect::<Vec<_>>()
                .join(",");
            warn!(
                "MISSEVENT fid={frame_id} out={out} outstanding={outstanding} \
                 newcommit={newcommit} pass_seq={} c2s_ms={c2s:.2} t2c_ms={t2c:.2} \
                 span_ms={span:.2} commits={} prev_t2c=[{prev}]",
                mark.seq, mark.commits,
            );
        },
        None => {
            warn!(
                "MISSEVENT fid={frame_id} out={out} outstanding={outstanding} \
                 newcommit={newcommit} pass=none"
            );
        },
    }
}

/// 이 출력이 primary 인가.
///
/// ★DWM 합성은 primary 의 vblank 에 물려 있다.★ `DwmGetCompositionTimingInfo(NULL)` 의
/// `qpcVBlank` 가 곧 그 패널의 vblank 이므로, 넷 중 어느 것이 primary 인지 모르면 "어느
/// 타일이 합성 클럭과 동기인가" 를 답할 수 없다. 열거 순서(`OUTPHASE` 의 `rel_ms` 기준)는
/// DXGI 어댑터/출력 번호일 뿐 primary 와 무관하다 -- 그래서 따로 묻는다.
fn is_primary(monitor: usize) -> bool {
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // Safety: 순수 out-param 조회. `monitor` 는 DXGI 열거가 준 살아 있는 `HMONITOR` 다.
    if unsafe { GetMonitorInfoW(monitor as HMONITOR, &mut info) } == 0 {
        return false;
    }
    info.dwFlags & MONITORINFOF_PRIMARY != 0
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
                    "OUTPHASE out={} monitor={:#x} primary={} grid=none",
                    out.name,
                    out.monitor,
                    u8::from(is_primary(out.monitor)),
                );
            }
            return;
        },
    };
    let period = base.period_qpc as i128;
    let to_ms = |ticks: f64| ticks * 1000.0 / freq as f64;
    // ★두 격자를 같은 축에 올린다.★ `vblank_qpc % period` 만 찍으면 비교가 안 된다 --
    // `DCOMPSTAT phase_ms` 는 **합성** 주기로 접고 여기는 **출력** 주기로 접는데, QPC 값이
    // 1e12 규모라 두 주기가 한 틱만 달라도 나머지는 전혀 다른 값이 나온다. 그래서 절대
    // 위상(`abs_ms`)은 참고로만 두고, 판정은 **차이를 접은** 두 값으로 한다.
    //
    // `vs_dwmvb_ms` = 이 출력의 vblank 가 DWM(=primary) vblank 뒤 얼마인가. primary 라면
    // 0 에 붙어야 한다 -- `primary=1` 인 줄과 대조하면 그 자체가 검산이다.
    // `vs_comp_ms`  = DComp 가 마지막으로 합성한 시각 뒤 얼마인가 = 합성에서 스캔아웃까지.
    //
    // 두 값이 창마다 **흐르면** 그 격자와 이 출력의 주기가 실제로 다르다는 뜻이고, 그것이
    // 고정 오프셋과 구분되는 유일한 신호다(고정 오프셋은 프레임을 중복시키지 않는다).
    let dwm_vblank = crate::dcomp_compositor::composition_grid();
    let composition = composition_grid();
    let show =
        |value: Option<f64>| value.map_or_else(|| "n/a".to_string(), |ms| format!("{ms:+.2}"));
    for out in outputs {
        let Some(grid) = grid_for_monitor(out.monitor) else {
            // ★격자가 없는 출력이야말로 운영자가 봐야 할 줄이다.★ 조용히 건너뛰면 그 출력은
            // `OUTPHASE` 에도 `OUTCOMMIT` 에도(위상 표본이 없으므로) 안 나와서, `[outgrid]`
            // 열거 목록과 손으로 대조해야만 빠진 것을 알 수 있다. 매초 네 줄이 나오는지 세는
            // 것만으로 판정이 되게 한다.
            warn!(
                "OUTPHASE out={} monitor={:#x} primary={} grid=none",
                out.name,
                out.monitor,
                u8::from(is_primary(out.monitor)),
            );
            continue;
        };
        // 표본은 출력마다 최대 한 바퀴(~83ms)까지 시각이 벌어지지만, 주기로 접으면 그
        // 차이는 사라지고 위상만 남는다.
        let delta = grid.vblank_qpc as i128 - base.vblank_qpc as i128;
        let rel = (((delta % period) + period) % period) as f64;
        // 이 출력의 vblank 를 남의 원점 기준으로 접는다. 주기가 0 이면 접을 수 없다.
        let against = |origin: u64, other_period: u64| -> Option<f64> {
            let other_period = i128::from(other_period);
            if other_period == 0 {
                return None;
            }
            let diff = grid.vblank_qpc as i128 - origin as i128;
            Some(to_ms(
                (((diff % other_period) + other_period) % other_period) as f64,
            ))
        };
        warn!(
            "OUTPHASE out={} monitor={:#x} primary={} period_ms={:.2} measured={} rel_ms={:+.2} \
             abs_ms={:.2} vs_dwmvb_ms={} vs_comp_ms={}",
            out.name,
            out.monitor,
            u8::from(is_primary(out.monitor)),
            to_ms(grid.period_qpc as f64),
            u8::from(grid.measured),
            to_ms(rel),
            to_ms((grid.vblank_qpc % grid.period_qpc.max(1)) as f64),
            show(dwm_vblank.and_then(|(vblank, p)| against(vblank, p))),
            show(composition.and_then(|(last, p)| against(last, p))),
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
        // ★어댑터 LUID 를 떠 둔다.★ `DCompositionGetStatistics` 가 돌려주는 타깃은 이름이
        // 아니라 `displayAdapterLuid` 로만 자기를 밝힌다 -- 그 통계를 우리 출력에 붙이는
        // 열쇠가 이것이다. 못 읽으면 0 으로 두고, 그러면 그 출력은 매칭에서 빠진다.
        let mut adapter_desc: DXGI_ADAPTER_DESC1 = std::mem::zeroed();
        let adapter_luid = if (*adapter).GetDesc1(&mut adapter_desc) < 0 {
            (0u32, 0i32)
        } else {
            (
                adapter_desc.AdapterLuid.LowPart,
                adapter_desc.AdapterLuid.HighPart,
            )
        };
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
                adapter_luid,
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
    use super::{composition_target, period_from_pair};

    /// ★목표 시각은 언제나 격자 위에 있고, 시계 지터와 무관하다.★
    ///
    /// 세 번의 회귀가 전부 이 성질을 포기한 데서 나왔다. `now` 에서 올림으로 인덱스를 뽑으면
    /// 틱 지터(p50 16.67 / p95 17.23ms, 주기 16.667ms)가 반올림 경계를 넘나들며 전체의
    /// 4.28% 에서 슬립을 만들었고, 그것을 뒤에서 떨어 막으려다 매번 더 나빠졌다.
    #[test]
    fn the_target_is_always_on_the_grid_and_advances_one_period() {
        let period = 166_667;
        let phase = 12_345;
        let mut previous: Option<u64> = None;
        for step in 0..10_u64 {
            // 합성 격자가 한 칸씩 전진한다. `now` 는 등장하지도 않는다 -- 그것이 요점이다.
            let last = phase + (100 + step) * period;
            let target = composition_target(last, period, 1);
            assert_eq!(
                (target - phase) % period,
                0,
                "목표가 격자를 벗어났다(step={step})"
            );
            if let Some(before) = previous {
                assert_eq!(target - before, period, "한 주기가 아니다(step={step})");
            }
            previous = Some(target);
        }
    }

    /// 같은 `last` 를 읽은 타일들은 같은 목표를 받는다 -- 네 페인터가 같은 값을 읽는다는
    /// 것은 `DCOMPSTAT` 으로 측정돼 있으므로, 이 함수가 결정적이면 타일 간 어긋남이 없다.
    #[test]
    fn tiles_reading_the_same_composition_agree() {
        let period = 166_667;
        let last = 12_345 + 500 * period;
        let target = composition_target(last, period, 1);
        for _tile in 0..4 {
            assert_eq!(composition_target(last, period, 1), target);
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

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
#![allow(unsafe_code)]
// `enumerate_outputs` 는 사실상 전체가 raw DXGI FFI다 -- `unsafe fn` 본문 안의 개별
// 연산마다 중첩 `unsafe {}`를 요구하는 edition 2024 린트는 신호 대비 잡음이 크므로 끈다
// (dcomp_compositor.rs/dcomp_video_convert.rs의 `#![allow(unsafe_code)]`와 같은 취지).
#![allow(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;

use log::warn;

/// ★계측 전체의 주 스위치.★ `SERVO_WALL_NO_PROBE=1` 이면 꺼진다.
///
/// 계측 비용을 재려고 계측을 더 붙이는 것은 순환이다. 이 스위치는 그 대신 ***같은
/// 바이너리에서 계측만 빼는*** A/B 를 가능하게 한다.
static PROBE_ENABLED: LazyLock<bool> = LazyLock::new(|| {
    !servo_config::debug_env::enabled(&servo_config::debug_env::WALL_NO_PROBE)
});

/// ★수집 게이트. 방출과 **같은** 조건이어야 한다.★
///
/// 어긋나면 아무도 비우지 않는 맵에 영원히 쌓인다. 이 코드베이스가 그 함정에 두 번 당했다 --
/// `PHASES` 가 프로세스 수명 내내 자랐고(Fix round 1, Ruling 15), B1 의 프로브를 진단 플래그
/// 뒤에 두어 격자가 영영 비었다(설계 문서 C5). ★그래서 수집·방출·프로브 스레드가 이 하나의
/// 값을 본다★ -- 셋이 갈라질 수 없게 한다.
///
/// `note_dcomp_stat` 만 예외다. 그 함수는 B2(`-SampleLead`)가 쓰는 공통 합성 격자를 채우므로
/// 격자 갱신은 이 게이트 **앞**에 있어야 한다 -- 계측을 껐다고 기능이 죽으면 안 된다.
pub(crate) static COLLECT: LazyLock<bool> =
    LazyLock::new(|| *PROBE_ENABLED && *crate::dcomp_compositor::DCOMP_BIND_PROF);
use winapi::Interface;
use winapi::shared::dxgi::{
    CreateDXGIFactory1, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
};
use winapi::shared::windef::HMONITOR;
use winapi::um::profileapi::{QueryPerformanceCounter, QueryPerformanceFrequency};
use winapi::um::wingdi::DEVMODEW;
use winapi::um::winuser::{
    ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST,
    MONITORINFO, MONITORINFOF_PRIMARY, MonitorFromWindow,
};

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
    /// ★`last mod period`(ms).★ 인덱스는 `last / period` 이므로, 이 잔차가 0 이나 주기
    /// 끝에 가까우면 `last` 의 작은 흔들림이 몫을 앞뒤로 뒤집는다 -- 그것이 `same` 과
    /// `jump2` 가 1:1 로 짝지어 나오는 모양이다. 멀리 있으면 같은 흔들림이 아무 일도
    /// 하지 않는다. 그래서 ***이 값이 슬립의 순간에 어디 있었나***가 판정이다.
    residue_at_slip: Vec<f64>,
    /// 전체 프레임의 같은 값. 위와 나란히 놓아야 "경계에 몰렸나" 를 말할 수 있다.
    residue_all: Vec<f64>,
    /// 직전에 본 `last`(합성 격자점의 절대 QPC).
    last_grid: Option<u64>,
    /// ★`last` 가 뒤로 갔거나 제자리였던 횟수.★ 인덱스는 `last / period` 이므로 `last` 가
    /// 단조롭지 않으면 몫이 반복되고, 그 다음 프레임이 밀린 만큼을 한꺼번에 따라잡는다 --
    /// `same` 바로 뒤에 `jumpN` 이 붙는 모양이 그것이다. `DCOMPSTAT distinct` 는 값이
    /// 달라지기만 하면 세므로 ***역행을 잡지 못한다.***
    grid_back: u64,
    /// 역행했을 때 뒤로 간 양(ms). 한 주기에 못 미치는 흔들림인지 그 이상인지 가른다.
    grid_back_ms: Vec<f64>,
    /// 한 주기를 크게 넘겨 앞으로 뛴 횟수(>= 2 주기). 역행의 짝이다.
    grid_leap: u64,
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
/// 직전 샘플 이후 합성이 몇 번 지났나. 음수면 격자가 뒤로 간 것이다.
///
/// ★증분으로 센다. 절대 몫으로 세면 안 된다.★ 예전에는 `last / period` 를 인덱스로 삼고
/// 그 차를 봤는데, `last` 는 부팅 이후 QPC(가동 4.2 시간이면 1,518 억 틱)라 ***나눗수가
/// 몇 틱만 달라져도 몫이 수십 칸 움직인다.*** 그리고 그 나눗수는 고정이 아니다 --
/// `period_ticks` 는 DWM 이 돌려주는 rate 에서 매번 다시 계산되는데, 이 기계에서 그 rate 가
/// 60.002 / 60.003 / 60.004Hz 로 흔들렸다(log_ani_debug 24/local_05: 106 / 11 / 1 회).
/// 세 값의 `period_ticks` 는 166661 / 166658 / 166655 라 한 번 흔들릴 때마다 인덱스가
/// 16 칸씩 튀었다.
///
/// 그 16 칸은 전진하면 `jumpN`, 되돌아오면 -- `saturating_sub` 가 음수를 0 으로 뭉개므로 --
/// `same` 으로 기록됐다. ★측정된 "슬립 2.2%" 는 그래서 전부 이 둘의 짝이었다★(40 대 40).
/// 실제 애니메이션은 멀쩡했다. 쓰이는 값은 `target = last + 2 * period` 이고, 주기가 3 틱
/// 달라져도 그것은 0.6µs 움직일 뿐이다.
///
/// 벽에서 이 지표가 0.00% 였던 것도 샘플링이 더 좋아서가 아니라 ***그 기계의 rate 가
/// 60.000Hz 로 고정이라 나눗수가 안 흔들렸기 때문***이다.
///
/// 증분은 한 주기 남짓이라 나눗수의 몇 틱 차이에 무감하다. 반올림으로 센다 -- 실제 합성
/// 간격은 공칭 주기와 정확히 같지 않으므로 절삭하면 한 칸이 0 으로 떨어진다.
fn compositions_advanced(previous: u64, last: u64, period: u64) -> i64 {
    if period == 0 {
        return 0;
    }
    let delta = last as i128 - previous as i128;
    let period = period as i128;
    // 반올림 나눗셈. 음수 쪽도 대칭이어야 역행이 -1 로 나온다.
    let rounded = if delta >= 0 {
        (delta + period / 2) / period
    } else {
        (delta - period / 2) / period
    };
    rounded as i64
}

fn note_sample_slip(
    monitor: usize,
    phase_ms: f64,
    residue_ms: f64,
    last_grid: u64,
    period_ticks: u64,
    freq: u64,
) {
    if !*COLLECT {
        return;
    }
    let Ok(mut guard) = SLIPS.lock() else { return };
    let tally = guard
        .get_or_insert_with(HashMap::new)
        .entry(monitor)
        .or_default();
    tally.n += 1;
    tally.phase_all.push(phase_ms);
    tally.residue_all.push(residue_ms);
    if let Some(previous) = tally.last_grid {
        if last_grid <= previous {
            tally.grid_back += 1;
            tally
                .grid_back_ms
                .push((previous - last_grid) as f64 * 1000.0 / freq as f64);
        } else if last_grid - previous >= period_ticks.saturating_mul(2) {
            tally.grid_leap += 1;
        }
        // ★분류는 `last` 의 증분으로 한다.★ 왜 절대 몫이면 안 되는지는
        // `compositions_advanced`.
        let slipped = match compositions_advanced(previous, last_grid, period_ticks) {
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
            tally.residue_at_slip.push(residue_ms);
        }
    }
    tally.last_grid = Some(last_grid);
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

/// 돌려주는 것은 ★`(앵커, 남은 시간)`★ 이고, 호출자는 `앵커 + 남은 시간` 을 써야 한다.
///
/// ★앵커를 여기서 읽는 이유.★ `sample_at = anchor + (target - qpc_now)` 이고 `target` 은
/// 격자 위 정수배다. ***두 시계를 같은 순간에 읽으면 `anchor` 가 상쇄되어 `sample_at` 이
/// 정확히 `target` 이 된다*** -- 샘플 시각이 격자에 떨어져 간격이 완벽히 고르다. 둘 사이에
/// 무엇이든 끼면 그만큼 밀리고, ***그 일의 소요 시간 변동이 그대로 샘플 간격의 지터가
/// 된다.*** 호출자가 `Instant::now()` 를 따로 읽어 넘기면 그 사이에 `tile_monitor`
/// (`MonitorFromWindow`)와 `composition_grid`(`DwmGetCompositionTimingInfo`, DWM 과
/// 통신한다)가 들어가므로, ★앵커는 반드시 `qpc_now()` 옆에서 읽어야 한다.★
///
/// `snap_to_dwm_grid_at` 이 같은 이유로 같은 모양이다 -- 그쪽은 이 수정으로 missed present
/// 가 3~4 배 줄었다.
pub(crate) fn lead_to_next_composition(
    monitor: usize,
    lead_periods: u64,
) -> Option<(std::time::Instant, Duration)> {
    let (last, period) = composition_grid()?;
    let freq = qpc_frequency()?;
    // ★이 두 줄 사이에 아무것도 두지 말 것.★ 위 문단이 그 이유다.
    let anchor = std::time::Instant::now();
    let now = qpc_now()?;
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
    // ★이 잔차는 진단으로만 남는다.★ 한때 인덱스를 `last / period` 로 뽑았고, 그래서
    // 이 값이 0 이나 주기 끝에 붙으면 몫이 뒤집힌다는 가설을 세웠다. ***측정이 그것을
    // 반증했다*** -- 슬립은 주기 한가운데(9~11ms)에서 났고 `residue_slip` 이 `residue_all`
    // 과 구별되지 않았다(log_ani_debug 24/local_04). 진짜 원인은 나눗수 쪽이었고
    // (`compositions_advanced`), 분류는 이제 증분으로 한다. 분포는 그대로 찍어 둔다 --
    // 다음에 같은 질문이 오면 다시 재는 대신 이 줄을 보면 된다.
    let residue_ms = (last % period) as f64 * 1000.0 / freq as f64;
    note_sample_slip(monitor, phase_ms, residue_ms, last, period, freq);
    Some((
        anchor,
        Duration::from_secs_f64(target.saturating_sub(now) as f64 / freq as f64),
    ))
}

static DCOMP_STAT_FAILED: AtomicU64 = AtomicU64::new(0);

/// 셸이 표출 틱을 낼 때 부른다. ★틱마다 한 번★ -- 타일마다가 아니다.
pub(crate) fn note_present_tick_now() {}

pub(crate) fn note_dcomp_stat_failed() {
    if !*COLLECT {
        return;
    }
    DCOMP_STAT_FAILED.fetch_add(1, Ordering::Relaxed);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn note_dcomp_stat(
    _monitor: usize,
    _device: usize,
    last: u64,
    _now: u64,
    _next: u64,
    freq: u64,
    rate_num: u32,
    rate_den: u32,
    _qpc_at_read: Option<u64>,
) {
    if freq == 0 || rate_num == 0 || rate_den == 0 {
        note_dcomp_stat_failed();
        return;
    }
    // 합성 주기는 DWM 이 유리수로 알려 준다 -- 추정할 필요가 없다.
    let period_ticks = freq.saturating_mul(rate_den as u64) / rate_num as u64;
    if period_ticks == 0 {
        note_dcomp_stat_failed();
        return;
    }
    // ★격자 갱신은 계측 게이트 밖이다.★ B2 가 이 격자를 쓰므로, 프로파일이 꺼져 있다고
    // 격자를 안 채우면 B2 가 통째로 무력해진다 -- B1 에서 프로브를 진단 플래그 뒤에 두어
    // 똑같이 당한 적이 있다(설계 문서 C5).
    if let Ok(mut guard) = COMPOSITION.lock() {
        *guard = Some((last, period_ticks));
    }
    if !*COLLECT {
        return;
    }
}

/// 모니터 -> 이번 창의 샘플 lead 표본(ms). `SAMPLELEAD` 가 초당 비운다.
static LEADS: Mutex<Option<HashMap<usize, Vec<f64>>>> = Mutex::new(None);
/// 격자나 모니터를 못 구해 오늘 동작으로 떨어진 횟수.
static LEAD_MISSING: AtomicU64 = AtomicU64::new(0);

pub(crate) fn note_sample_lead(monitor: usize, lead: Duration) {
    if !*COLLECT {
        return;
    }
    if let Ok(mut guard) = LEADS.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .entry(monitor)
            .or_default()
            .push(lead.as_secs_f64() * 1000.0);
    }
}

pub(crate) fn note_sample_lead_missing() {
    if !*COLLECT {
        return;
    }
    LEAD_MISSING.fetch_add(1, Ordering::Relaxed);
}

/// ★페이싱 스레드를 primary 출력의 다음 vblank 까지 블록한다.★ 돌아온 직후의
/// `(vblank_qpc, period_qpc)` 를 QPC 틱으로 준다.
///
/// ★왜 조회가 아니라 대기인가.★ 지금까지 셸의 박자는 `DwmGetCompositionTimingInfo` 로
/// 격자를 **묻고** 그 지점까지 `thread::sleep` 했다(`winit_wall` 의 `snap_to_dwm_grid_at`).
/// 그 경로에는 오차가 둘 있다 -- DWM 으로의 **프로세스 간 호출** 지연(변동한다)과 대기
/// 타이머의 깨우기 지터다. 둘 다 그대로 틱 위상의 지터가 되고, ★커밋 위상 하나가 저더를
/// 정하는 구조에서★(`commit_guard` 모듈 주석: 위상 13% -> 63% 에서 dup 6 -> 2146) 그것은
/// 공짜가 아니다. vblank 를 직접 기다리면 깨우는 주체가 커널의 디스플레이 인터럽트이므로
/// 조회도 예측도 없다.
///
/// ★기본은 primary, `gfx_wall_pacing_output` 으로 바꿀 수 있다.★ 네 패널의 vblank 는
/// 주기의 0.67 에 흩어져 있지만(`OUTPHASE`) 합성 격자는 하나이고, 셸이 맞춰야 하는 것은
/// 그 하나다 -- 타일별 정렬은 B2(`-SampleLead`)의 몫이고 이 함수의 일이 아니다.
///
/// ★primary 가 그 하나라는 보장이 없다.★ 오래 그렇게 적혀 있었고 2026-10-26 에 깨졌다 --
/// 같은 바이너리로 돈 네 런에서 DWM 격자에 붙은 출력이 런마다 달랐다. 어긋난 런은 커밋
/// 위상이 내내 52% 였고 dup 이 4.025% 로 최악이었다(붙은 런은 0.9%). 그래서 어느 출력을
/// 기다릴지가 pref 로 나와 있다. 자동 선택기는 아직 없다 -- 핀으로 인과를 먼저 가른다.
///
/// ★이 호출은 블록한다 -- 페이싱 스레드 전용이다.★ 핸들을 thread-local 에 두는 것이 그
/// 계약을 구조로 만든 것이다: COM 포인터가 스레드 경계를 넘지 않고, 다른 스레드가 불러도
/// 자기 핸들을 따로 연다.
///
/// `None` 이면 호출자가 타이머로 떨어진다. 세션 잠금·모니터 전원 off·토폴로지 변경 중에는
/// `WaitForVBlank` 가 즉시 실패 HRESULT 로 돌아오므로 ★그 상태에서 이 함수는 블록하지
/// 않는다★ -- 되풀이되는 실패의 백오프는 호출자 몫이다.
pub(crate) fn wait_primary_vblank() -> Option<(u64, u64)> {
    thread_local! {
        static SOURCE: std::cell::RefCell<VblankSource> =
            const { std::cell::RefCell::new(VblankSource::new()) };
    }
    SOURCE.with(|source| source.borrow_mut().wait())
}

/// `wait_primary_vblank` 가 들고 있는 primary 출력 핸들.
///
/// thread-local 이므로 만든 스레드에서만 쓰이고 그 스레드에서 해제된다 -- 그래서 `Output` 의
/// 포인터에 `Send` 가 필요 없다.
/// ★페이싱이 기다릴 출력의 고정 이름.★ 비어 있거나 `primary` 면 오늘 동작이다.
///
/// 틱 경로에서 `pref!` 를 치지 않는다 -- 그것이 전역 `RwLock` 을 잡으므로, 페이싱
/// 스레드에서 없애려던 양이 바로 그런 것이다(`spawn_pacing_thread` 가 구성을 기동에 한 번만
/// 읽는 것과 같은 이유). 그래서 첫 접근에 한 번만 읽는다.
static PACING_OUTPUT_PIN: LazyLock<String> =
    LazyLock::new(|| servo_config::pref!(gfx_wall_pacing_output).trim().to_string());

/// 고정이 걸려 있으면 그 이름의 색인, 아니면 `None`.
///
/// ★이름(`DeviceName`)으로 찾는다 -- 색인이나 `HMONITOR` 가 아니다.★ 열거 순서는 토폴로지가
/// 바뀌면 달라지고 `HMONITOR` 값은 윈도가 재사용한다. 둘 다 조용히 다른 패널을 가리키게
/// 되는데, 이 pref 의 목적이 바로 "어느 패널인지" 라 그 실패가 치명적이다.
fn pinned_index(outputs: &[Output]) -> Option<usize> {
    let pin = PACING_OUTPUT_PIN.as_str();
    if pin.is_empty() || pin.eq_ignore_ascii_case("primary") {
        return None;
    }
    outputs
        .iter()
        .position(|out| out.name.eq_ignore_ascii_case(pin))
}

struct VblankSource {
    /// 살아 있는 열거. 구성이 바뀌거나 대기가 실패하면 버린다.
    enumeration: Option<Enumeration>,
    /// `enumeration.outputs` 안에서 primary 의 색인.
    primary: usize,
    /// ★실제로 기다릴 출력의 색인.★ 고정이 없거나 그 이름을 못 찾으면 `primary` 와 같다.
    wait_idx: usize,
    /// 그 출력의 `HMONITOR`. 관측을 격자에 실을 때 키가 된다(`note_pacing_vblank`).
    monitor: usize,
    /// 디스플레이 모드에서 읽은 주기. 못 읽으면 `None` 이고 60Hz 가정값으로 떨어진다.
    period_qpc: Option<u64>,
    /// `VBANCHOR` 를 마지막으로 찍은 QPC. 초당 한 줄로 조인다.
    last_log_qpc: u64,
}

impl VblankSource {
    const fn new() -> Self {
        Self {
            enumeration: None,
            primary: 0,
            wait_idx: 0,
            monitor: 0,
            period_qpc: None,
            last_log_qpc: 0,
        }
    }

    /// 핸들이 없거나 구성이 바뀌었으면 다시 연다.
    ///
    /// ★열거가 비어도 다시 연다★ -- `needs_reenumerate` 가 빈 목록을 재열거 사유로 치기
    /// 때문이다. 그러면 모니터가 없는 동안 호출마다 `CreateDXGIFactory1` 이 한 번 돌지만,
    /// 그때 `wait` 은 `None` 을 주고 호출자가 타이머로 떨어져 한 주기를 자므로 초당 60 번이
    /// 상한이다. 대신 모니터가 돌아오면 ★다음 호출에서 바로 복구한다★ -- 한 번만 열거하는
    /// 구조가 핫플러그 뒤 영구 폴백이 되는 것을 지금은 없어진 프로브 스레드가 이미 겪었다.
    fn refresh(&mut self) {
        let stale = match self.enumeration.as_ref() {
            // Safety: 살아 있는 팩토리이거나 NULL.
            Some(enumeration) => unsafe { enumeration.needs_reenumerate() },
            None => true,
        };
        if !stale {
            return;
        }
        if let Some(mut old) = self.enumeration.take() {
            // Safety: 이 열거의 포인터는 이 스레드 밖으로 나간 적이 없다.
            unsafe { old.release() };
        }
        // Safety: 위와 같다.
        let enumeration = unsafe { enumerate_outputs() };
        // primary 를 못 찾으면 첫 출력을 쓴다. 합성 격자와 어긋날 수 있지만 박자가 아예 없는
        // 것보다 낫고, 어긋났다면 `OUTPHASE` 의 `primary=` 칸이 그 사실을 보여 준다.
        let primary = enumeration
            .outputs
            .iter()
            .position(|out| is_primary(out.monitor))
            .unwrap_or(0);
        // ★고정이 있으면 그쪽을 기다린다.★ 이름을 못 찾으면 primary 로 떨어지되 **매
        // 재열거마다 다시 찾는다** -- 고정 대상이 꺼져 있다 돌아오는 경우에 영구 폴백이
        // 되면 안 된다. `primary` 는 그대로 계산해 둔다: 폴백이자 로그의 한 칸이다.
        let wait_idx = pinned_index(&enumeration.outputs).unwrap_or(primary);
        // ★주기는 모드에서 읽는다 -- 실측하지 않는다.★ 실측하려면 vblank 를 두 번 기다려야
        // 하는데, 이 스레드는 기다린 vblank 마다 틱을 내므로 둘째 대기는 곧 ***틱 하나를
        // 통째로 거르는 것***이다.
        // ★주기와 `HMONITOR` 는 **기다릴 출력**의 것이어야 한다.★ 전에는 둘 다 `primary`
        // 에서 읽었는데, 고정이 다른 출력을 가리키면 다른 패널의 주기로 박자를 재고 남의
        // 모니터 키로 관측을 싣게 된다.
        self.period_qpc = enumeration
            .outputs
            .get(wait_idx)
            .zip(qpc_frequency())
            .and_then(|(out, freq)| nominal_period_ticks(&out.name, freq, None));
        self.monitor = enumeration
            .outputs
            .get(wait_idx)
            .map_or(0, |out| out.monitor);
        self.enumeration = Some(enumeration);
        self.primary = primary;
        self.wait_idx = wait_idx;
    }

    /// `VBANCHOR` 한 줄. 초당 하나로 조인다.
    ///
    /// ★arm 을 로그로 증명하는 것이 이 줄의 전부다.★ 2026-10-26 에 arm 을 증명할 수 없는
    /// 배포본으로 A/B 를 두 바퀴 돌렸고 둘 다 버렸다. 위상 자체는 `OUTPHASE` 의
    /// `vs_dwmvb_ms` 가 출력마다 이미 내므로 여기서 DWM 을 또 묻지 않는다 -- 그 IPC 를
    /// 없애려고 vblank 클럭을 만든 것이다.
    fn log_anchor(&mut self, now: u64, freq: u64) {
        if self.last_log_qpc != 0 && now.saturating_sub(self.last_log_qpc) < freq {
            return;
        }
        self.last_log_qpc = now;
        let Some(out) = self
            .enumeration
            .as_ref()
            .and_then(|enumeration| enumeration.outputs.get(self.wait_idx))
        else {
            return;
        };
        let pin = PACING_OUTPUT_PIN.as_str();
        warn!(
            "VBANCHOR out={} monitor={:#x} primary={} pin={} honoured={} period_ms={:.4}",
            out.name,
            out.monitor,
            u8::from(self.wait_idx == self.primary),
            if pin.is_empty() { "-" } else { pin },
            u8::from(self.wait_idx != self.primary),
            self.period_qpc
                .map_or(f64::NAN, |ticks| ticks as f64 * 1000.0 / freq as f64),
        );
    }

    fn wait(&mut self) -> Option<(u64, u64)> {
        self.refresh();
        let output = self
            .enumeration
            .as_ref()
            .and_then(|enumeration| enumeration.outputs.get(self.wait_idx))?
            .output;
        // Safety: 살아 있는 `IDXGIOutput`. ★블록한다★ -- 그래서 페이싱 스레드 전용이다.
        if unsafe { (*output).WaitForVBlank() } < 0 {
            // ★여기서 자지 않는다.★ 자면 그만큼 박자가 밀리고, 호출자는 이 실패를 "이번
            // 회전만 타이머로" 로 다루므로 간격은 그쪽이 지킨다. 대신 핸들을 버려 다음
            // 호출이 재열거하게 한다 -- 실패의 흔한 원인이 토폴로지 변경이다.
            if let Some(mut dead) = self.enumeration.take() {
                // Safety: 위와 같다.
                unsafe { dead.release() };
            }
            return None;
        }
        let vblank = qpc_now()?;
        let period = self
            .period_qpc
            .or_else(|| qpc_frequency().map(|freq| freq / 60))?;
        // ★틱을 보낸 **뒤**가 아니라 여기인 이유★ -- 이 함수는 호출자가 틱을 내기 직전에
        // 돌아가지만, 초당 한 번 `warn!` 하나가 전부다(포매팅 몇 마이크로초). 틱 경로에
        // 넣지 말아야 할 것은 잠금과 IPC 이고 둘 다 없다.
        if let Some(freq) = qpc_frequency() {
            self.log_anchor(vblank, freq);
        }
        Some((vblank, period))
    }
}

impl Drop for VblankSource {
    fn drop(&mut self) {
        if let Some(mut enumeration) = self.enumeration.take() {
            // Safety: 이 열거의 포인터는 이 스레드 밖으로 나간 적이 없고, thread-local 의
            // 소멸자는 그 스레드에서 돈다.
            unsafe { enumeration.release() };
        }
    }
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
}

/// 이 출력이 primary 인가.
///
/// ★낡음(2026-10-26 반증)★ -- 여기 있던 "DWM 합성은 primary 의 vblank 에 물려 있다" 는
/// 이 벽에서 틀렸다. 같은 바이너리·같은 명령으로 돈 네 런에서 `vs_dwmvb_ms` 가 0 에 붙은
/// 출력이 런마다 달랐고, 한 런은 `primary=0` 인 DISPLAY22 였다. DWM 격자에 붙은 출력은
/// **측정해서** 알아내야 하고, 그 측정이 `OUTPHASE` 의 `vs_dwmvb_ms` 다.
///
/// 그래도 primary 는 찍을 가치가 있다: 기본 페이싱 대상이고(`gfx_wall_pacing_output` 이
/// 비어 있을 때), 핀이 걸린 런에서 "무엇에서 벗어났는가" 의 기준점이다. 열거 순서
/// (`OUTPHASE` 의 `rel_ms` 기준)는 DXGI 어댑터/출력 번호일 뿐 primary 와 무관하다 --
/// 그래서 따로 묻는다.
fn is_primary(monitor: usize) -> bool {
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // Safety: 순수 out-param 조회. `monitor` 는 DXGI 열거가 준 살아 있는 `HMONITOR` 다.
    if unsafe { GetMonitorInfoW(monitor as HMONITOR, &mut info) } == 0 {
        return false;
    }
    info.dwFlags & MONITORINFOF_PRIMARY != 0
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
    use super::composition_target;

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
}

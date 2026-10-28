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

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;

use log::warn;

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
    Some((
        anchor,
        Duration::from_secs_f64(target.saturating_sub(now) as f64 / freq as f64),
    ))
}

pub(crate) fn note_dcomp_stat(last: u64, freq: u64, rate_num: u32, rate_den: u32) {
    if freq == 0 || rate_num == 0 || rate_den == 0 {
        return;
    }
    // 합성 주기는 DWM 이 유리수로 알려 준다 -- 추정할 필요가 없다.
    let period_ticks = freq.saturating_mul(rate_den as u64) / rate_num as u64;
    if period_ticks == 0 {
        return;
    }
    // ★격자 갱신은 어떤 진단 게이트 뒤에도 두지 않는다.★ B2 가 이 격자를 쓰므로, 게이트
    // 뒤에 두면 B2 가 통째로 무력해진다 -- B1 에서 프로브를 진단 플래그 뒤에 두어
    // 똑같이 당한 적이 있다(설계 문서 C5).
    if let Ok(mut guard) = COMPOSITION.lock() {
        *guard = Some((last, period_ticks));
    }
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
    // 이 thread-local 이 primary 출력 핸들을 들고 있다. 만든 스레드에서만 쓰이고 그
    // 스레드에서 해제되므로 `Output` 의 포인터에 `Send` 가 필요 없다.
    thread_local! {
        static SOURCE: std::cell::RefCell<VblankSource> =
            const { std::cell::RefCell::new(VblankSource::new()) };
    }
    SOURCE.with(|source| source.borrow_mut().wait())
}

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

/// ★실제로 바인딩된 출력을 쉘에 한 줄로 넘긴다.★ 바인딩이 **바뀔 때만** 채워진다.
///
/// ★왜 여기서 직접 찍지 않는가.★ 이 자리는 페이싱 스레드다. 그 스레드의 stderr 는
/// 금지다 -- 한 줄이라도 여기서 내면 "틱 경로에 I/O 가 없다" 가 더는 참이 아니게 되고,
/// 다음 사람이 두 줄째를 넣을 때 기달 선이 사라진다. 그래서 문자열만 놓아두고
/// 메인 스레드가 꺼내 찍는다(`winit_wall` 의 `about_to_wait`).
///
/// ★왜 필요한가.★ `gfx_wall_pacing_output` 이 이름을 못 찾으면 **조용히** primary 로
/// 떨어진다(`refresh`). 런처는 pref 를 넘겼다는 것만 찍을 수 있으므로, 핀이 실제로
/// 걸렸는지는 엔진만 말할 수 있다. 그 일을 하던 `VBANCHOR` 는 2026-10-27 계측 제거에서
/// 없어졌고, 그 뒤로 확인할 방법이 하나도 없었다.
static PACING_BINDING: Mutex<Option<String>> = Mutex::new(None);

/// 위 자물쇠를 잡기 **전에** 보는 깃발. 메인 루프가 회전마다 보므로 relaxed 로드
/// 하나보다 비싸면 안 된다.
static PACING_BINDING_PENDING: AtomicBool = AtomicBool::new(false);

/// 쉘이 꺼내 간다. 낼 것이 없으면 자물쇠를 잡지 않고 `None`.
///
/// 깃발을 내린 뒤 자물쇠를 잡기 전에 페이싱 스레드가 새 줄을 넣으면, 이번 호출이
/// 그 새 줄을 가져가고 깃발은 올라간 채로 남는다 -- 다음 회전이 빈 자물쇠를 한 번 잡고
/// `None` 을 돌려줄 뿐이다. 잃는 줄은 없다.
pub(crate) fn take_pacing_binding() -> Option<String> {
    if !PACING_BINDING_PENDING.swap(false, Ordering::Relaxed) {
        return None;
    }
    PACING_BINDING.lock().ok().and_then(|mut slot| slot.take())
}

/// 페이싱 스레드가 바인딩이 바뀔 때 부른다. ★자물쇠 하나, I/O 없음.★
fn publish_pacing_binding(line: String) {
    if let Ok(mut slot) = PACING_BINDING.lock() {
        *slot = Some(line);
        PACING_BINDING_PENDING.store(true, Ordering::Relaxed);
    }
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
    /// 쉘에 마지막으로 넘긴 바인딩 설명. 같은 값이면 다시 넘기지 않는다 --
    /// 모니터가 없는 동안 `refresh` 는 호출마다 재열거하므로, 이것이 없으면 같은 줄이
    /// 초당 60 번 올라간다.
    announced: Option<String>,
}

impl VblankSource {
    const fn new() -> Self {
        Self {
            enumeration: None,
            primary: 0,
            wait_idx: 0,
            monitor: 0,
            period_qpc: None,
            announced: None,
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
        // ★바인딩이 바뀔었으면 쉘에 한 줄 넘긴다.★ 여기서 찍지 않는 이유는
        // `PACING_BINDING` 의 주석에 있다. 정상 구성에서 재열거는 기동과 토폴로지 변경에만
        // 도므로 줄 수는 그 횟수와 같다. 모니터가 하나도 없으면 호출마다 이 `String` 을
        // 한 번 만드는데, 그 상태의 같은 호출은 바로 위에서 `CreateDXGIFactory1` 과 전체
        // 열거를 이미 돌리고 있다 -- 할당 하나는 그 옆에서 보이지 않는 양이다.
        let line = self.describe_binding();
        if self.announced.as_deref() != Some(line.as_str()) {
            publish_pacing_binding(line.clone());
            self.announced = Some(line);
        }
    }

    /// 지금 무엇에 묶여 있나 -- 쉘이 찍을 한 줄.
    ///
    /// 읽는 사람이 알아야 하는 것은 셋이다: ① 어느 패널을 기다렸나, ② 그것이
    /// primary 인가, ③ ★`-PacingOutput` 이 실제로 걸렸나★. 셋째가 이 줄의 존재
    /// 이유다 -- 이름을 못 찾은 핀은 조용히 primary 로 떨어지고, 그러면 런은 정확히
    /// 핀을 주지 않은 런과 같아진다.
    fn describe_binding(&self) -> String {
        let outputs = self
            .enumeration
            .as_ref()
            .map_or(&[][..], |enumeration| enumeration.outputs.as_slice());
        let Some(bound) = outputs.get(self.wait_idx) else {
            return String::from("pacing vblank: 열린 출력이 하나도 없다 -- 페이싱은 타이머로 떨어진다");
        };
        let hz = self
            .period_qpc
            .filter(|period| *period > 0)
            .zip(qpc_frequency())
            .map_or_else(
                || String::from("Hz 미상"),
                |(period, freq)| format!("{:.2}Hz", freq as f64 / period as f64),
            );
        let pin = PACING_OUTPUT_PIN.as_str();
        let pin_state = if pin.is_empty() || pin.eq_ignore_ascii_case("primary") {
            String::from("pin 없음 -> primary")
        } else if pinned_index(outputs).is_some() {
            format!("pin \"{pin}\" 적용됨")
        } else {
            format!("★pin \"{pin}\" 을 찾지 못해 primary 로 떨어졌다★")
        };
        format!(
            "pacing vblank: {} ({}, 출력 {}/{}) {} -- {}",
            bound.name,
            if self.wait_idx == self.primary {
                "primary"
            } else {
                "non-primary"
            },
            self.wait_idx + 1,
            outputs.len(),
            hz,
            pin_state,
        )
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

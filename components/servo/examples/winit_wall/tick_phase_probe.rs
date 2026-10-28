/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! ★틱 위상 프로브★ — 페이싱 틱이 DWM 합성 격자의 어느 위상에 떨어지는지 센다.
//!
//! ★왜 필요한가.★ 저더를 정하는 변수는 하나다 — 커밋이 DWM 합성 격자의 어느 위상에
//! 떨어지나(`commit_guard` 모듈 doc: 위상 13% → 63% 에서 dup 6 → 2146). 그 위상은 틱 위상에
//! 파이프라인 지연을 더한 것이고, 틱 위상은 ***페이싱 클럭을 무엇으로 고르느냐***가 정한다.
//! 그런데 2026-10-27 계측 제거 이후 그 위상을 수치로 말할 수단이 하나도 없다. 클럭 원천을
//! 바꾸는 작업(`docs/superpowers/plans/2026-10-28-compositor-clock-pacing.md`)은 이것 없이는
//! "바꿨는데 좋아진 것 같다" 밖에 말하지 못한다. 그래서 이것이 그 계획의 Task 0 이다.
//!
//! # 스레드 셋이 하는 일이 다르다
//!
//! | 스레드 | 하는 일 | 치르는 값 |
//! |---|---|---|
//! | 페이싱 | 틱마다 QPC 하나를 링에 넣는다 | QPC 1 회 + 원자 연산 3 회 |
//! | ★리포터(전용)★ | 접고, 주기적으로 찍는다 | 이 스레드가 존재하는 이유 |
//! | 메인(렌더) | ★아무것도 하지 않는다★ | 0 |
//!
//! ★페이싱 스레드에 들어가면 안 되는 것이 둘이다.★
//!
//!  * stderr·파일 I/O — `lib.rs` 의 `wait_primary_vblank` doc 이 정한 규칙이다.
//!  * ★`DwmGetCompositionTimingInfo` 질의★ — 그 질의가 **없다는 것**이 vblank 경로의 존재
//!    이유다(`spawn_pacing_thread` doc: "타이머 경로의 오차는 둘이고 … DWM 으로의 프로세스
//!    간 호출 지연"). 재려고 그것을 틱 경로에 넣으면 ***재려던 것을 부순다.***
//!
//! ★메인에도 넣지 않는다.★ 접으려면 DWM 격자를 물어야 하는데 그것은 캐시가 아니라 매번
//! 프로세스 간 호출이고(`dcomp_compositor.rs:1264`), 메인은 곧 렌더 스레드다. 거기에 초당
//! 열 번을 더하면 프로브가 ***재려는 대상을 흔든다.*** 그래서 전용 스레드가 모아서 쓴다 —
//! 계측 규칙이 정한 기본 형태 그대로다.
//!
//! # ★왜 주기적으로 찍나 — 종료 훅은 없다★
//!
//! 처음에는 `main` 의 이벤트 루프가 끝난 뒤 한 번 찍게 했다. ***그 줄은 실제 측정 런에서
//! 한 번도 나오지 않는다.*** 런처가 `-DurationSec` 을 `Stop-Process -Force`
//! (`TerminateProcess`)로 끝내고(`run_wall_dist.ps1:1015-1019`), 무제한 런도 같은 방법으로
//! 죽이며(`:352-354` "Wall tile windows ignore WM_CLOSE"), 엔진에는 자체 종료 타이머가
//! 없다. `run_app` 이 반환하지 않으므로 그 뒤의 코드는 ***도달하지 않는다.***
//!
//! 그래서 리포터가 [`REPORT_EVERY`] 마다 ***런 시작부터의 누적***을 찍는다. 강제 종료로
//! 잃는 것은 마지막 구간뿐이고, ★로그의 마지막 줄이 곧 그 런의 답이다.★ `env_logger` 는
//! stderr 를 버퍼 없이 쓰므로(런처 주석 `:1017`) 강제 종료가 줄을 삼키지 않는다.

use std::f64::consts::PI;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use super::{QueryPerformanceCounter, qpc_frequency};

/// 링 크기. 2 의 거듭제곱이라 나머지 연산이 `&` 로 접힌다.
const RING: usize = 1024;

/// 리포터가 접으러 깨는 간격.
///
/// ★이 값이 앵커 오차의 한계를 정한다.★ 접는 연산은 주기에 대한 나머지라, 앵커가 표본보다
/// 나중이어서 생기는 오차는 "DWM 이 보고한 주기와 실제 주기의 차이" × (앵커와 표본의 시간
/// 거리)다. 그 거리의 상한이 바로 이 값이다. `qpcRefreshPeriod` 는 QPC 틱 단위 정수라
/// 양자화 오차가 0.5 틱(상대 3e-6)이고, 250ms 면 0.75µs = 주기의 0.005% — 버킷 폭 0.1% 의
/// 1/20 이다. ★이 근사는 해상도 한참 아래에 있다.★
///
/// 링이 1024 이므로 이 간격에 쌓이는 것은 60Hz 에서 15 개다. 여유가 68 배다.
const FOLD_EVERY: Duration = Duration::from_millis(250);

/// 누적 요약을 찍는 간격.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// 위상 버킷. 주기의 1/1000 단위라 60Hz 에서 16.7µs 해상도다.
///
/// 이 해상도를 고른 이유는 안전대의 폭이다 — 위상 항등식의 안전대가 `≳ +0.9ms`,
/// 함정이 `−1.5 ~ +0.5ms` 이므로(`2026-10-27-log-arm-gate.md:44-72`) 1% 버킷(0.17ms)은
/// 그 경계를 6 칸으로밖에 못 나눈다. 1‰ 면 60 칸이다.
const PHASE_BUCKETS: usize = 1000;

/// 간격 버킷. 주기의 1/100 단위이고 5.11 주기에서 포화한다.
const GAP_BUCKETS: usize = 512;

/// 건너뜀으로 셀 간격의 하한(주기의 1.5 배). 버킷 단위로 150.
const GAP_SKIP_BUCKET: usize = 150;

/// 앵커를 다시 ***물어 볼*** 최소 간격(QPC 주파수의 1/10 = 100ms).
///
/// ★성공·실패와 무관하게 시도 시각을 기준으로 센다.★ 앵커의 *나이*로 재면, DWM 이 답하지
/// 않거나 같은 낡은 `qpcVBlank` 를 계속 돌려줄 때 영영 stale 로 남아 접을 때마다 다시
/// 묻게 된다 — 막으려던 프로세스 간 호출 폭주가 바로 그 모양이다.
const ANCHOR_QUERY_DIVISOR: i64 = 10;

/// 주기가 이만큼(1/1000 = 버킷 한 칸) 넘게 달라졌을 때만 "바뀌었다" 로 센다.
///
/// ★엄격 비교는 거짓 경보였다.★ 첫 실측(log_DwmClock/01)에서 20 초 런에 23 번과 11 번이
/// 찍혔는데, 두 보고 줄의 주기가 모두 `16.666ms` 로 같았다 — 즉 변동폭이 0.017ms(0.1%)
/// 아래였다. `qpcRefreshPeriod` 는 DWM 의 ***추정치***라 LSB 가 흔들린다. 그 흔들림을
/// 모드 변경으로 신고하면, 이 수가 0 이 아닐 때 런을 의심하라는 doc 이 ★늑대 소년★이 된다.
/// 진짜로 잡아야 할 것(60.00 → 59.94 = 0.1%)은 이 문턱을 넘는다.
const PERIOD_CHANGE_PERMILLE: u64 = 1;

/// 앵커에서 이만큼(1 초) 넘게 떨어진 표본은 접지 않고 센다.
///
/// 리포터가 굶어 백로그가 쌓이면 위 `FOLD_EVERY` 의 오차 논증이 깨진다. 그때는 ***조용히
/// 번진 히스토그램을 내놓느니 세고 버린다.***
const SAMPLE_MAX_AGE_DIVISOR: i64 = 1;

/// 거절된 틱이 얼마나 일렀나 -- µs 단위, 1ms 에서 포화.
const SHORTFALL_BUCKETS: usize = 1000;

/// `ClockSite` 의 자리 수. ★`ClockSite::index` 와 순서가 맞아야 하고,
/// `clock_site_indices_match_the_probe_labels` 테스트가 그것을 못 박는다.★
const SITES: usize = 4;

/// 자리 이름. 위와 같은 순서다.
const SITE_LABELS: [&str; SITES] = ["about_to_wait", "user_event", "window_event", "pacing"];

/// 자리별 발화 수. ★페이싱이 박자를 쥐고 있는데 다른 자리가 발화했다면 그것이 "가로챔"
/// 이다★ -- `main.rs` 의 `user_event` 주석이 그 구조를 적는다.
static GATE_FIRED: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];

/// ★페이싱 틱이 게이트에서 거절된 수.★ 다른 자리의 거절은 세지 않는다 -- 엔진 wake 는
/// 초당 수백 번 오고 그 대부분이 격자점 사이에 떨어지므로, 세면 신호가 묻힌다.
static GATE_DEFERRED: AtomicU64 = AtomicU64::new(0);

/// 거절된 페이싱 틱이 격자점보다 얼마나 일렀나(µs).
static GATE_SHORTFALL: [AtomicU32; SHORTFALL_BUCKETS] =
    [const { AtomicU32::new(0) }; SHORTFALL_BUCKETS];

static ENABLED: AtomicBool = AtomicBool::new(false);

/// 페이싱 스레드가 넣은 틱 QPC. 생산자 하나(페이싱), 소비자 하나(리포터).
static SLOTS: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];

/// 지금까지 넣은 개수. ★생산자만 쓴다.★
static PUSHED: AtomicU64 = AtomicU64::new(0);

/// 타이머 폴백 경로로 나간 틱의 수. vblank 경로가 `Unavailable` 로 떨어진 회전과
/// `-PacingDwmQuery` 런이 여기 센다 — ★두 모집단을 구분하지 않으면 폴백이 섞인 런의
/// 위상 p50 을 클럭의 성질로 오해한다.★
static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);

static FOLD: Mutex<Fold> = Mutex::new(Fold::new());

/// 접힌 상태. ★리포터 스레드만 쓰고★, `main` 이 끝에 한 번 읽기만 한다.
struct Fold {
    phase: [u32; PHASE_BUCKETS],
    gap: [u32; GAP_BUCKETS],
    /// 접은 개수 = 링의 읽기 커서. ★리포터만 만진다.★
    folded: u64,
    samples: u64,
    gaps: u64,
    skips: u64,
    dropped: u64,
    /// 앵커에서 너무 멀어 버린 표본.
    stale: u64,
    /// `(vblank_qpc, period_qpc)` — 마지막으로 읽은 DWM 격자.
    anchor: Option<(u64, u64)>,
    /// 마지막으로 DWM 에 **물어본** 시각(QPC). 성공 여부와 무관하다.
    last_query: Option<u64>,
    query_failures: u64,
    /// ★문턱을 넘은★ 주기 변경만 센다(`PERIOD_CHANGE_PERMILLE`).
    period_changes: u32,
    /// 본 주기의 최소·최대. 잡음의 크기를 ***수치로*** 보여 준다 -- 카운터 하나보다
    /// 이쪽이 "믿을 만한 런인가" 를 직접 답한다.
    period_seen: Option<(u64, u64)>,
    /// 직전에 접은 틱. 간격을 재는 데만 쓰고, 끊기면 `None` 으로 버린다.
    last_tick: Option<u64>,
}

impl Fold {
    const fn new() -> Self {
        Self {
            phase: [0; PHASE_BUCKETS],
            gap: [0; GAP_BUCKETS],
            folded: 0,
            samples: 0,
            gaps: 0,
            skips: 0,
            dropped: 0,
            stale: 0,
            anchor: None,
            last_query: None,
            query_failures: 0,
            period_changes: 0,
            period_seen: None,
            last_tick: None,
        }
    }
}

/// 중독된 자물쇠도 그대로 쓴다.
///
/// 안에 있는 것은 순수한 카운터뿐이라 패닉이 망가뜨릴 불변식이 없다. 반대로 중독을 이유로
/// 물러나면 ★측정이 통째로 사라진 런과 프로브를 끈 런이 로그에서 구분되지 않는다.★
fn fold_lock() -> std::sync::MutexGuard<'static, Fold> {
    FOLD.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 프로브를 켜고 리포터 스레드를 띄운다. ★페이싱 스레드를 띄우기 전에 부른다.★
pub(crate) fn start() {
    ENABLED.store(true, Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name(String::from("WallTickPhase"))
        .spawn(|| {
            let started = std::time::Instant::now();
            let mut next_report = started + REPORT_EVERY;
            loop {
                std::thread::sleep(FOLD_EVERY);
                fold_pending();
                if std::time::Instant::now() >= next_report {
                    report(started.elapsed());
                    next_report += REPORT_EVERY;
                }
            }
        });
    if let Err(error) = spawned {
        log::warn!("wall: 틱 위상 리포터 스레드를 띄우지 못했다: {error}; 요약이 나오지 않는다");
    }
}

/// ★페이싱 스레드가 틱을 내기 직전에 부른다.★
///
/// ★호출자가 걸러 준다 — 여기서 `ENABLED` 를 보지 않는다.★ 페이싱 스레드는 구성을 기동에
/// 한 번 읽어 지역 변수로 들고 있으므로(`spawn_pacing_thread`), 꺼져 있으면 이 함수가 아예
/// 불리지 않는다. 즉 프로브가 꺼진 런의 틱 경로에 남는 것은 ***지역 bool 분기 하나***다.
///
/// 재는 순간은 "클럭이 깨워서 틱을 내기로 정한 때"다. `send_event` 뒤가 아니라 앞에서 재는
/// 이유는 프록시·이벤트 루프 지연을 클럭의 위상으로 셈하지 않기 위해서다 — 그 지연은 클럭
/// 선택과 무관하고, 섞으면 세 경로의 비교가 흐려진다.
///
/// `from_timer` 는 이 틱이 ***타이머 경로***로 나갔다는 뜻이다(`-PacingDwmQuery`, 또는
/// vblank 가 `Unavailable` 이라 떨어진 회전).
pub(crate) fn note_tick(from_timer: bool) {
    let mut qpc: i64 = 0;
    // Safety: 순수 out-param.
    if unsafe { QueryPerformanceCounter(&mut qpc) } == 0 || qpc < 0 {
        return;
    }
    if from_timer {
        TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
    }
    push(qpc as u64);
}

/// 자리 이름. 색인은 `ClockSite::index` 와 같은 순서다.
pub(crate) fn site_label(index: usize) -> &'static str {
    SITE_LABELS.get(index).copied().unwrap_or("?")
}

/// ★메인 스레드가 게이트를 통과해 발화했다.★ 자리별로 센다.
///
/// 이 함수와 아래 [`note_gate_deferred`] 는 ***메인(렌더) 스레드***에서 불린다. 치르는 값은
/// relaxed 원자 연산 둘이고, 같은 자리에서 이미 `Instant::now()` 를 읽으므로 그에 비하면
/// 없는 값이다.
pub(crate) fn note_gate_fired(site: usize) {
    if !is_enabled() {
        return;
    }
    if let Some(slot) = GATE_FIRED.get(site) {
        slot.fetch_add(1, Ordering::Relaxed);
    }
}

/// ★페이싱 틱이 왔는데 게이트가 거절했다.★ `shortfall` 은 격자점까지 남은 거리다.
///
/// ★이 수가 왜 중요한가.★ `drive_present_clock_timer` 는 `now >= next` 일 때만 발화하고
/// `next` 는 DWM 격자점이다. 그래서 페이싱 틱이 격자점보다 이르면 그 발화가 ***다음
/// 깨우기로 밀린다.*** 2026-10-28 실측에서 컴포지터 클럭 틱은 격자점 +0.1%(17µs)에
/// 떨어지고 퍼짐이 ±0.3 점(±50µs)이었으므로, `-DwmAlign 0` 에서는 여유가 17µs 뿐이다.
/// 그 산술이 맞는지를 이 카운터가 판정한다 -- ★틱 위상 프로브는 페이싱 스레드를 세고
/// 메인의 발화는 세지 않아서 이 질문에 답할 수 없었다.★
pub(crate) fn note_gate_deferred(shortfall: std::time::Duration) {
    if !is_enabled() {
        return;
    }
    GATE_DEFERRED.fetch_add(1, Ordering::Relaxed);
    let bucket = (shortfall.as_micros() as usize).min(SHORTFALL_BUCKETS - 1);
    GATE_SHORTFALL[bucket].fetch_add(1, Ordering::Relaxed);
}

/// 링에 하나 넣는다. 생산자는 페이싱 스레드 하나뿐이라 `PUSHED` 를 relaxed 로 읽어도 된다.
fn push(qpc: u64) {
    let pushed = PUSHED.load(Ordering::Relaxed);
    SLOTS[(pushed as usize) & (RING - 1)].store(qpc, Ordering::Relaxed);
    // ★Release 로 쓴다.★ 소비자는 `PUSHED` 를 Acquire 로 읽고 그 뒤에 슬롯을 읽으므로,
    // 이 순서가 "개수가 보이면 값도 보인다" 를 보장한다.
    PUSHED.store(pushed.wrapping_add(1), Ordering::Release);
}

/// 쌓인 틱을 접는다. ★리포터 스레드만 부른다.★
fn fold_pending() {
    let pushed = PUSHED.load(Ordering::Acquire);
    let mut fold = fold_lock();
    if pushed == fold.folded {
        return;
    }

    // ★여유를 코드로 보장한다.★ 접는 동안에도 생산자는 계속 넣으므로, 아직 안 읽은 슬롯을
    // 덮을 수 있다. 임계를 링의 절반으로 두면 접기 시작 시점에 ***최소 512 틱(8.5 초)***의
    // 여유가 남는다 — 접는 데 드는 시간(수십 µs)의 10 만 배다. 넘친 몫은 세고 버린다.
    let backlog = pushed - fold.folded;
    if backlog > (RING / 2) as u64 {
        fold.dropped += backlog - (RING / 2) as u64;
        fold.folded = pushed - (RING / 2) as u64;
        // 끊긴 구간을 건너뛴 간격으로 세면 거짓 건너뜀이 된다.
        fold.last_tick = None;
    }

    let newest = SLOTS[((pushed - 1) as usize) & (RING - 1)].load(Ordering::Relaxed);
    refresh_anchor(&mut fold, newest);
    let Some((vblank_qpc, period_qpc)) = fold.anchor else {
        // DWM 이 답하지 않는다. 접지 않고 커서만 밀어 이 구간을 버린다 — 앵커 없이 접으면
        // 위상이 아니라 잡음이 쌓인다.
        fold.dropped += pushed - fold.folded;
        fold.folded = pushed;
        fold.last_tick = None;
        return;
    };
    let max_age = qpc_frequency().map_or(u64::MAX, |freq| (freq / SAMPLE_MAX_AGE_DIVISOR) as u64);

    let mut folded = fold.folded;
    while folded < pushed {
        let tick = SLOTS[(folded as usize) & (RING - 1)].load(Ordering::Relaxed);
        fold_sample(&mut fold, tick, vblank_qpc, period_qpc, max_age);
        folded += 1;
    }
    fold.folded = folded;
}

/// 앵커를 다시 묻는다 — ★마지막 **시도**로부터 100ms 가 지났을 때만.★
fn refresh_anchor(fold: &mut Fold, newest: u64) {
    if let (Some(last), Some(freq)) = (fold.last_query, qpc_frequency())
        && newest.saturating_sub(last) <= (freq / ANCHOR_QUERY_DIVISOR) as u64
    {
        return;
    }
    fold.last_query = Some(newest);
    let Some((vblank_qpc, period_qpc)) = servo::dwm_composition_grid().filter(|(_, p)| *p != 0)
    else {
        fold.query_failures += 1;
        return;
    };
    if let Some((_, previous)) = fold.anchor
        && previous.abs_diff(period_qpc) * 1000 > previous * PERIOD_CHANGE_PERMILLE
    {
        // 주사율이 런 도중에 ***실제로*** 바뀌었다(DRR, 모드 변경). 접은 값들이 서로 다른
        // 주기를 기준으로 하므로, 이 수가 0 이 아니면 그 런은 믿을 것이 못 된다.
        fold.period_changes += 1;
    }
    fold.period_seen = Some(match fold.period_seen {
        Some((low, high)) => (low.min(period_qpc), high.max(period_qpc)),
        None => (period_qpc, period_qpc),
    });
    fold.anchor = Some((vblank_qpc, period_qpc));
}

/// 표본 하나를 접는다. ★순수 함수라 테스트가 이것을 직접 부른다.★
///
/// `vblank_qpc` 는 DWM 이 돌려준 vblank 인데 드라이버에 따라 직전일 수도 다음일 수도 있다
/// (`snap_to_dwm_grid_at` 이 같은 이유로 나머지를 두 번 건다). 위상은 주기에 대한 나머지로
/// 정의되므로 어느 쪽이든 같은 점으로 접힌다.
fn fold_sample(fold: &mut Fold, tick: u64, vblank_qpc: u64, period_qpc: u64, max_age: u64) {
    if period_qpc == 0 {
        return;
    }
    if tick.abs_diff(vblank_qpc) > max_age {
        fold.stale += 1;
        fold.last_tick = None;
        return;
    }
    let period = period_qpc as i128;
    let delta = tick as i128 - vblank_qpc as i128;
    let phase = ((delta % period) + period) % period;
    let bucket = ((phase * PHASE_BUCKETS as i128 / period) as usize).min(PHASE_BUCKETS - 1);
    fold.phase[bucket] += 1;
    fold.samples += 1;

    if let Some(previous) = fold.last_tick {
        let gap = tick.saturating_sub(previous) as i128;
        let gap_bucket = ((gap * 100 / period) as usize).min(GAP_BUCKETS - 1);
        fold.gap[gap_bucket] += 1;
        fold.gaps += 1;
        if gap_bucket >= GAP_SKIP_BUCKET {
            fold.skips += 1;
        }
    }
    fold.last_tick = Some(tick);
}

/// 히스토그램에서 백분위에 해당하는 버킷 색인. ★선형 변수 전용★(간격).
fn percentile(hist: &[u32], total: u64, pct: u64) -> usize {
    if total == 0 {
        return 0;
    }
    let target = (total * pct / 100).max(1);
    let mut cumulative = 0u64;
    for (index, count) in hist.iter().enumerate() {
        cumulative += u64::from(*count);
        if cumulative >= target {
            return index;
        }
    }
    hist.len() - 1
}

/// 원형 변수의 요약. 버킷 색인은 원래(회전하지 않은) 좌표다.
#[derive(Debug, PartialEq)]
struct Circular {
    p05: usize,
    p50: usize,
    p95: usize,
    /// `p95 - p05` 를 회전한 좌표에서 잰 값. ★감싸지 않으므로 항상 폭이다.★
    width: usize,
    /// 집중도 `R` (0~1). 1 이면 한 점에 모였고 0 이면 고르게 흩어졌다.
    concentration: f64,
}

/// ★위상은 원형 변수다 — 선형 백분위를 그대로 쓰면 안 된다.★
///
/// 틱이 격자점 근처(위상 ≈ 0)에 모이면 표본이 0‰ 쪽과 999‰ 쪽으로 갈라진다. 그 상태로
/// 선형 백분위를 뽑으면 실제 퍼짐이 0.6 점인데 `p05=0.1, p95=99.9` 로 찍혀 "99.8 점 폭"
/// 으로 읽힌다. ★그리고 그 경우가 바로 우리가 가장 보고 싶은 경우다★ — 컴포지터 클럭도
/// `-DwmAlign 0` 의 vblank 경로도 거기에 떨어질 것으로 기대된다.
///
/// 그래서 원형 평균 방향을 구해 그것이 한가운데 오도록 좌표를 돌린 뒤 백분위를 뽑고, 결과만
/// 원래 좌표로 되돌린다. 단봉 분포에서는 이것으로 충분하고, 얼마나 단봉인지는 `R` 이 말해
/// 준다(`R` 이 작으면 애초에 백분위를 믿을 것이 못 된다).
fn circular_summary(hist: &[u32; PHASE_BUCKETS], total: u64) -> Option<Circular> {
    if total == 0 {
        return None;
    }
    let n = PHASE_BUCKETS as f64;
    let (mut sx, mut sy) = (0.0f64, 0.0f64);
    for (index, count) in hist.iter().enumerate() {
        if *count == 0 {
            continue;
        }
        let angle = 2.0 * PI * (index as f64 + 0.5) / n;
        sx += f64::from(*count) * angle.cos();
        sy += f64::from(*count) * angle.sin();
    }
    let concentration = sx.hypot(sy) / total as f64;
    // 평균 방향을 한가운데(500‰)에 놓는 원점.
    let mean = (sy.atan2(sx) / (2.0 * PI) * n).rem_euclid(n) as usize % PHASE_BUCKETS;
    let origin = (mean + PHASE_BUCKETS / 2) % PHASE_BUCKETS;

    let rotated = |j: usize| hist[(origin + j) % PHASE_BUCKETS];
    let at = |pct: u64| {
        let target = (total * pct / 100).max(1);
        let mut cumulative = 0u64;
        for j in 0..PHASE_BUCKETS {
            cumulative += u64::from(rotated(j));
            if cumulative >= target {
                return j;
            }
        }
        PHASE_BUCKETS - 1
    };
    let (j05, j50, j95) = (at(5), at(50), at(95));
    Some(Circular {
        p05: (origin + j05) % PHASE_BUCKETS,
        p50: (origin + j50) % PHASE_BUCKETS,
        p95: (origin + j95) % PHASE_BUCKETS,
        width: j95.saturating_sub(j05),
        concentration,
    })
}

/// 누적 요약 두 줄. ★리포터가 10 초마다 부른다.★
///
/// `warn!` 인 이유는 방막 런이 `RUST_LOG=warn` 이기 때문이다 — `info` 로 두면 정작 볼
/// 사람에게 안 보인다(`about_to_wait` 의 바인딩 줄과 같은 이유).
fn report(elapsed: Duration) {
    let fold = fold_lock();
    let secs = elapsed.as_secs();
    if fold.samples == 0 {
        log::warn!(
            "wall: tick phase [{secs}s] 표본 없음 -- 페이싱 스레드가 틱을 내지 않았거나 DWM 이 \
             격자를 주지 않았다(유실 {}, 질의실패 {})",
            fold.dropped,
            fold.query_failures,
        );
        return;
    }
    let Some(circular) = circular_summary(&fold.phase, fold.samples) else {
        return;
    };
    let period_ms = fold
        .anchor
        .zip(qpc_frequency())
        .map_or(f64::NAN, |((_, period), freq)| {
            period as f64 * 1000.0 / freq as f64
        });
    let timer = TIMER_TICKS.load(Ordering::Relaxed);
    log::warn!(
        "wall: tick phase [{secs}s] p05/p50/p95 = {:.1}/{:.1}/{:.1} % 폭 {:.1}점 R={:.3} \
         (n={}, 타이머경로 {timer}, 주기 {period_ms:.3}ms)",
        circular.p05 as f64 / 10.0,
        circular.p50 as f64 / 10.0,
        circular.p95 as f64 / 10.0,
        circular.width as f64 / 10.0,
        circular.concentration,
        fold.samples,
    );
    report_gate(secs);
    let gap = |p: u64| percentile(&fold.gap, fold.gaps, p) as f64 / 100.0;
    log::warn!(
        "wall: tick gap   [{secs}s] p05/p50/p95 = {:.2}/{:.2}/{:.2} 주기 (n={}, 1.5주기초과 {}, \
         유실 {}, 낡음 {}, 질의실패 {}, 주기변경 {}, 주기폭 {:.1}µs)",
        gap(5),
        gap(50),
        gap(95),
        fold.gaps,
        fold.skips,
        fold.dropped,
        fold.stale,
        fold.query_failures,
        fold.period_changes,
        period_span_us(&fold),
    );
}

/// DWM 이 보고한 주기의 최대-최소를 µs 로. 0 에 가까우면 격자가 안정적이라는 뜻이다.
fn period_span_us(fold: &Fold) -> f64 {
    fold.period_seen
        .zip(qpc_frequency())
        .map_or(f64::NAN, |((low, high), freq)| {
            (high - low) as f64 * 1_000_000.0 / freq as f64
        })
}

/// ★게이트 한 줄.★ 페이싱 틱이 메인에서 몇이 발화하고 몇이 거절됐나.
///
/// 자물쇠를 잡지 않는다 -- 전부 원자 카운터라 `Fold` 와 무관하다.
fn report_gate(secs: u64) {
    let deferred = GATE_DEFERRED.load(Ordering::Relaxed);
    let fired: Vec<u64> = GATE_FIRED
        .iter()
        .map(|slot| slot.load(Ordering::Relaxed))
        .collect();
    let pacing_fired = fired.last().copied().unwrap_or(0);
    let offered = pacing_fired + deferred;
    if offered == 0 {
        return;
    }
    // 페이싱이 아닌 자리의 발화 -- ★페이싱이 박자를 쥔 런에서 이것이 0 이 아니면
    // 가로챈 것이다.★
    let others: Vec<String> = fired
        .iter()
        .enumerate()
        .take(SITES - 1)
        .filter(|(_, count)| **count > 0)
        .map(|(index, count)| format!("{} {count}", site_label(index)))
        .collect();
    let shortfall = |p: u64| percentile_u32(&GATE_SHORTFALL, deferred, p);
    log::warn!(
        "wall: tick gate  [{secs}s] pacing 발화 {pacing_fired} / 거절 {deferred} ({:.2}%), \
         거절 모자란 양 p50/p95 = {}/{}µs, 다른 자리 발화 [{}]",
        deferred as f64 * 100.0 / offered as f64,
        shortfall(50),
        shortfall(95),
        if others.is_empty() {
            String::from("없음")
        } else {
            others.join(", ")
        },
    );
}

/// 원자 히스토그램의 백분위. 선형 변수(µs)라 원형 보정이 필요 없다.
fn percentile_u32(hist: &[AtomicU32], total: u64, pct: u64) -> usize {
    if total == 0 {
        return 0;
    }
    let target = (total * pct / 100).max(1);
    let mut cumulative = 0u64;
    for (index, slot) in hist.iter().enumerate() {
        cumulative += u64::from(slot.load(Ordering::Relaxed));
        if cumulative >= target {
            return index;
        }
    }
    hist.len() - 1
}

/// 이벤트 루프가 정상 종료했을 때 `main` 이 마지막으로 한 번 부른다.
///
/// ★이것은 덤이지 장치가 아니다.★ 측정 런은 강제 종료로 끝나므로 여기 닿지 않는다(모듈
/// doc 참조). 접지 않고 ***읽기만*** 하는 이유는 접는 쪽이 리포터 스레드 하나라는 SPSC
/// 전제를 깨지 않기 위해서다 — 마지막 `FOLD_EVERY` 구간을 잃지만 그 값은 요약을 바꾸지
/// 않는다.
pub(crate) fn log_summary() {
    if !is_enabled() {
        return;
    }
    report(Duration::ZERO);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★버킷 산수가 눈으로 보이도록 둥근 값을 쓴다.★ 1000 버킷이므로 `phase / 100` 이
    /// 곧 버킷이다. 현실적인 주기(60Hz·10MHz QPC = 166_667)는 홀수라 절단이 섞여 기대값을
    /// 읽기 어렵다 -- 그 절단 자체는 `truncation_rounds_a_bucket_down` 이 따로 못 박는다.
    const PERIOD: u64 = 100_000;
    /// 나이 검사를 끄고 싶을 때.
    const ANY_AGE: u64 = u64::MAX;

    fn fresh() -> Fold {
        Fold::new()
    }

    #[test]
    fn phase_is_taken_modulo_the_period() {
        let mut fold = fresh();
        fold_sample(&mut fold, 1_000_000 + PERIOD / 2, 1_000_000, PERIOD, ANY_AGE);
        // 세 주기 뒤의 같은 위상 -- 같은 버킷에 떨어져야 한다.
        fold_sample(
            &mut fold,
            1_000_000 + PERIOD * 3 + PERIOD / 2,
            1_000_000,
            PERIOD,
            ANY_AGE,
        );
        assert_eq!(fold.samples, 2);
        assert_eq!(fold.phase[500], 2, "반 주기는 500‰ 버킷이다");
    }

    #[test]
    fn phase_folds_a_vblank_reported_in_the_future() {
        // DWM 이 **다음** vblank 를 돌려주는 드라이버가 있다. 틱이 앵커보다 이르면 delta 가
        // 음수인데, 그때도 같은 격자 위의 점으로 접혀야 한다.
        let mut fold = fresh();
        let anchor = 10_000_000;
        fold_sample(&mut fold, anchor - PERIOD / 4, anchor, PERIOD, ANY_AGE);
        assert_eq!(fold.phase[750], 1, "−1/4 주기는 +3/4 주기와 같은 자리다");
    }

    #[test]
    fn a_sample_far_from_the_anchor_is_counted_not_folded() {
        let mut fold = fresh();
        fold_sample(&mut fold, 0, 10_000_000, PERIOD, 1_000);
        assert_eq!(fold.samples, 0, "앵커에서 멀면 접지 않는다");
        assert_eq!(fold.stale, 1);
        assert!(fold.last_tick.is_none(), "끊어야 거짓 간격이 생기지 않는다");
    }

    #[test]
    fn gap_is_not_counted_across_a_break() {
        let mut fold = fresh();
        fold_sample(&mut fold, 1_000_000, 1_000_000, PERIOD, ANY_AGE);
        assert_eq!(fold.gaps, 0, "첫 표본에는 직전이 없다");
        fold_sample(&mut fold, 1_000_000 + PERIOD, 1_000_000, PERIOD, ANY_AGE);
        assert_eq!(fold.gaps, 1);
        assert_eq!(fold.gap[100], 1, "한 주기는 100 버킷이다");

        fold.last_tick = None; // 유실이 끊은 상태를 흉내낸다.
        fold_sample(&mut fold, 1_000_000 + PERIOD * 50, 1_000_000, PERIOD, ANY_AGE);
        assert_eq!(fold.gaps, 1, "끊긴 구간은 간격으로 세지 않는다");
        assert_eq!(fold.skips, 0);
    }

    #[test]
    fn a_doubled_interval_counts_as_a_skip() {
        let mut fold = fresh();
        fold_sample(&mut fold, 0, 0, PERIOD, ANY_AGE);
        fold_sample(&mut fold, PERIOD * 2, 0, PERIOD, ANY_AGE);
        assert_eq!(fold.skips, 1);
        assert_eq!(fold.gap[200], 1);
    }

    #[test]
    fn a_huge_gap_saturates_instead_of_panicking() {
        let mut fold = fresh();
        fold_sample(&mut fold, 0, 0, PERIOD, ANY_AGE);
        fold_sample(&mut fold, PERIOD * 10_000, 0, PERIOD, ANY_AGE);
        assert_eq!(fold.gap[GAP_BUCKETS - 1], 1);
    }

    #[test]
    fn a_zero_period_is_ignored_rather_than_dividing_by_zero() {
        let mut fold = fresh();
        fold_sample(&mut fold, 1_000, 0, 0, ANY_AGE);
        assert_eq!(fold.samples, 0);
    }

    #[test]
    fn truncation_rounds_a_bucket_down() {
        // 현실의 주기는 홀수다(60Hz, QPC 10MHz). 정확히 반 주기인 표본은 499‰ 로 떨어진다 --
        // 나누기가 버림이기 때문이다. ★치우침은 한 버킷(0.1 점 = 60Hz 에서 17µs)을 넘지
        // 않으므로 그대로 둔다★; 보정하면 산수만 복잡해지고 해상도는 그대로다.
        const ODD: u64 = 166_667;
        let mut fold = fresh();
        fold_sample(&mut fold, ODD / 2, 0, ODD, ANY_AGE);
        assert_eq!(fold.phase[499], 1);
        assert_eq!(fold.phase[500], 0);
    }

    #[test]
    fn percentiles_walk_the_histogram() {
        let mut hist = [0u32; 10];
        hist[2] = 50;
        hist[7] = 50;
        assert_eq!(percentile(&hist, 100, 5), 2);
        assert_eq!(percentile(&hist, 100, 50), 2);
        assert_eq!(percentile(&hist, 100, 95), 7);
        assert_eq!(percentile(&hist, 0, 50), 0, "표본이 없어도 안 터진다");
    }

    // ---------------------------------------------------------------- 원형 통계

    fn hist_at(points: &[(usize, u32)]) -> [u32; PHASE_BUCKETS] {
        let mut hist = [0u32; PHASE_BUCKETS];
        for (index, count) in points {
            hist[*index] = *count;
        }
        hist
    }

    #[test]
    fn a_cluster_in_the_middle_behaves_like_the_linear_case() {
        let hist = hist_at(&[(400, 10), (500, 80), (600, 10)]);
        let summary = circular_summary(&hist, 100).expect("표본이 있다");
        assert_eq!(summary.p50, 500);
        assert_eq!(summary.p05, 400);
        assert_eq!(summary.p95, 600);
        assert_eq!(summary.width, 200);
        assert!(summary.concentration > 0.8, "한곳에 모였다");
    }

    #[test]
    fn a_cluster_straddling_zero_is_not_reported_as_full_width() {
        // ★이것이 선형 백분위가 틀리는 경우다.★ 절반은 999‰, 절반은 0‰ 에 있다.
        let hist = hist_at(&[(995, 50), (5, 50)]);
        let summary = circular_summary(&hist, 100).expect("표본이 있다");
        assert_eq!(summary.width, 10, "실제 폭은 1 점이지 99 점이 아니다");
        assert!(
            summary.p05 == 995 || summary.p05 == 5,
            "양 끝 중 하나가 p05 다: {}",
            summary.p05
        );
        assert!(summary.concentration > 0.99, "0 을 걸쳐도 모여 있다");

        // 선형 백분위였다면 폭이 990 으로 읽혔을 것이다 -- 회귀를 못 박아 둔다.
        let linear = percentile(&hist, 100, 95) - percentile(&hist, 100, 5);
        assert_eq!(linear, 990, "선형 계산은 이렇게 틀린다");
    }

    #[test]
    fn a_uniform_spread_reports_a_low_concentration() {
        let mut hist = [0u32; PHASE_BUCKETS];
        for slot in hist.iter_mut() {
            *slot = 1;
        }
        let summary = circular_summary(&hist, PHASE_BUCKETS as u64).expect("표본이 있다");
        assert!(
            summary.concentration < 0.01,
            "고르게 흩어지면 R 이 0 에 가깝다: {}",
            summary.concentration
        );
    }

    #[test]
    fn an_empty_histogram_has_no_summary() {
        assert!(circular_summary(&[0u32; PHASE_BUCKETS], 0).is_none());
    }

    /// ★쌓는 비용을 수치로 적는다.★ 메모리 규칙(`no-io-on-timing-critical-threads`)이
    /// "그 쌓기 비용도 수치로 평가 필수" 라고 요구한다.
    ///
    /// 상한을 100ns 로 느슨하게 잡은 것은 이 테스트가 ***회귀 탐지기***이지 벤치마크가
    /// 아니기 때문이다. 자물쇠나 할당이 섞여 들어오면 여기서 걸린다. 실측값은 커밋 메시지에
    /// 적는다.
    #[test]
    fn push_cost_is_sane() {
        const N: u32 = 200_000;
        let start = std::time::Instant::now();
        for i in 0..N {
            push(u64::from(i));
        }
        let per = start.elapsed().as_nanos() as f64 / f64::from(N);
        println!("push: {per:.1} ns/call ({N} 회)");
        assert!(per < 100.0, "링 push 가 {per:.1}ns 다 -- 자물쇠나 할당이 섞였나?");
    }
}

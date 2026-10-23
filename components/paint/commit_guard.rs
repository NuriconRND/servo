/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! DComp 디바이스 가드 + 출력 매핑. ★여기에 스케줄러는 없다.★
//!
//! ★이 파일은 `commit_scheduler.rs` 였다(2026-10-23 에 그 절반을 들어냈다).★ 들어낸 것은
//! "커밋을 렌더와 다른 시각에 내는" 기계 전부다 -- 스케줄러 스레드, 마감 큐, 상주 워커 풀,
//! `flush_before_render`, 재시도, `CommitAlign`. 왜 들어냈는지를 여기 남긴다. 그 기계를
//! 다시 세우려는 사람은 먼저 이 세 문단을 반증해야 한다.
//!
//! ★1. 그 기계는 저더를 **악화**시켰다.★ 실기 A/B(log_ani_debug_06/15·16, 6 구성 x 120s)에서
//! 저더(`COMPREFRESH refresh_d[2]`)는 ***커밋이 합성 주기의 어느 위상에 떨어지는가*** 하나로
//! 단조하게 설명됐다 -- 위상 13% 에서 dup 6, 63% 에서 2146, 340 배. 그리고 스케줄러가 커밋을
//! 더 많이 낼수록(24% -> 48% -> 70%) 저더가 컸다.
//!
//! ★2. 그 기계는 겨냥한 위상을 맞추지 못했다.★ 스케줄러는 `condvar.wait_timeout` 으로 깨는데
//! 실측 지연이 1.0~3.8ms 이고 산포가 그만큼 컸다(`wake_us_avg`). 반면 틱은 페이싱 스레드가
//! 고해상도 타이머로 찍는다. ***정확한 시계를 두고 부정확한 시계로 같은 일을 하던 구조다.***
//! 그래서 커밋의 52~76% 는 애초에 스케줄러가 아니라 `flush_before_render`(= 다음 틱)에서
//! 나갔고, 실측 위상 법칙이 `커밋 위상 ~= DwmAlign + 13%` 로 다섯 구성 전부 1.1 포인트 안에
//! 맞았다 -- 위상을 정한 것은 `CommitAlign` 이 아니라 틱이었다.
//!
//! ★3. 그 기계가 막는다던 위험은 실재하지 않았다.★ `flush_before_render` 는
//! `DCOMPOSITION_ERROR_SURFACE_BEING_RENDERED` 를 막으려고 있었는데, `BINDOPEN` 이 그
//! 잠금 구간을 재 보니 주기당 0.40ms(주기의 2.4%)였고 세 런 모두 `retried=0` 이었다
//! (log_ani_debug_06/14). 디바이스는 주기의 97.6% 동안 비어 있다.
//!
//! 그래서 지금 구조는 하나다 -- ★패스가 미룬 커밋은 **다음 틱 머리**에서 네 디바이스가
//! 한꺼번에 나간다★(`paint::Paint::commit_deferred_at_tick`). 위상은 틱의 위상이고, 그것은
//! `-DwmAlign` 이 직접 정한다.
//!
//! ---
//!
//! ★위상 법칙과 저더 곡선 -- 6 구성 실측★(log_ani_debug_06/15·16, 각 120s x 3).
//! `CommitAlign` 이 있던 시절의 표이고, `Dwm/Com` 이 같은 행이 지금 구조와 같은 동작이다
//! (마감 = 다음 틱 = 커밋이 틱에서 나감).
//!
//! ```text
//!   Dwm/Com   커밋 위상 p50(p95)   dup(런별)            런평균
//!     0 / 0      13.2 (17.3)       2, 18, 4                 8
//!     8 / 8      20.8 (26.6)       2, 15, 2                 6
//!    14 / 8      27   (34.1)       49, 51, 29              43
//!    22 / 8      33.9 (40.3)       265, 281, 407          318
//!    22 / 22     34.9 (40.8)       452, 417, 462          444
//!    50 / 50     62.7 (68.4)       1596, 2695            2146
//! ```
//!
//! ★위상 13% -> 63% 에서 dup 6 -> 2146. 단조, 예외 없음.★ 그 위상을 정한 것은 틱이었고,
//! 실측 법칙 `커밋 위상 ~= DwmAlign + 13%`(13% ~= `begin_frame` 오버헤드)가 다섯 구성 전부
//! 1.1 포인트 안에 맞았다. 문턱은 30% 대 중반이고 ***분포의 꼬리가 중앙값보다 먼저 닿는다***
//! -- 중앙값 27% 로 평평해 보이는 자리에서 8 포인트만 뒤로 가면 dup 이 7 배가 된다.
//!
//! 그래서 이 구조에서 지켜야 할 것은 하나다 -- ★`-DwmAlign` 을 주기의 20% 아래에 두라.★
//! `TICKCOMMIT` 의 `t2c_ms` 가 틱에서 커밋까지의 실제 지연이고, `COMMITCYCLE` 의 `phase_pct`
//! 가 그 결과 위상이다. 둘 중 뒤가 30% 대에 올라오면 저더가 따라 온다.
//!
//! ★아직 모르는 것★ -- 회귀 이전 기준선(`cba6e697388`)은 `14/8` 에서 dup 3~11 이었는데 같은
//! 자리의 지금 코드는 43 이다. 위상만으로는 그 6 배가 설명되지 않는다. 지금 구조는 동작점을
//! 곡선의 평평한 바닥으로 옮긴 것이고, 무엇이 분포를 나쁘게 만들었는지는 열려 있다.
//!
//! ---
//!
//! ★남은 것: 디바이스 가드.★ 한 디바이스를 두 스레드가 동시에 만지지 못하게 한다. 스케줄러가
//! 사라졌어도 경쟁자는 남는다 -- 그 타일의 painter 가 `end_frame` 에서(ANGLE GL 락을 쥔 채),
//! 비디오 fast-path `present_external_only` 가, 셸 스레드가 틱에서 내는 커밋이, 그리고
//! `begin_frame` 의 자기복구 커밋이 전부 같은 디바이스를 만진다.
//!
//! ★가드를 잡는 책임은 커밋의 정문(`dcomp_compositor::commit_device_at_tick`)에 있고, `Commit()`
//! 을 실제로 부르는 자리는 크레이트 전체에서 `dcomp_compositor::raw_commit` 하나뿐이다.★
//! 새 커밋 지점은 그 함수를 지나갈 수밖에 없다 -- 가드를 다섯 자리에서 하나씩 챙기던 방식은
//! 이미 세 번 실패했다(2·3·5 의 누락이 따로따로 발견됐다).
//!
//! ★가드 안에서 하는 일은 `Commit()` 하나뿐이어야 한다.★ 이 뮤텍스를 기다리는 쪽은 ANGLE GL
//! 락을 쥔 채로 기다릴 수 있고, 같은 디바이스의 WebGL 스레드가 그 뒤에 줄을 선다 -- 생산
//! 스레드가 커밋 대기에 동기화되어 처리량을 잃은 회귀 부류다. `note_dwm_phase`(전역 뮤텍스 +
//! DWM 조회)와 실패 로그와 위상 기록은 전부 가드 밖으로 뺀다. 그 상한이 지켜지는지는
//! `LOCKWAIT` 의 `painter_us_max` 가 지켜본다.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use log::warn;

use crate::output_grid::{qpc_frequency, qpc_now};

/// B2 pref(`gfx_sample_lead_periods`)를 **한 번만** 읽어 캐시한다. `Some(n)` = 켜짐,
/// `None` = 꺼짐(`-1`).
///
/// ★`pref!` 는 `PREFERENCES.read().unwrap()` 으로 펼쳐지는 RwLock 획득이다.★ 이 값을 묻는
/// 자리가 painter 마다 프레임당 하나라, 꺼져 있을 때도 프레임당 넷의 락이 벽의 가장 뜨거운
/// 경로에 새로 생긴다. `LazyLock` 으로 굳혀도 안전한 이유: 이 pref 는 기동 시 커맨드라인에서
/// 한 번 정해지고 실행 중에 바뀌지 않는다(Ruling 9 에서 리뷰어가 `Preferences` 의 모든 쓰기
/// 지점을 트리에서 감사해 확인했다).
///
/// 상한을 4 로 둔다 -- 그보다 앞서 보는 것은 네 주기(67ms) 뒤의 위치를 그리는 것이라
/// 애니메이션이 눈에 띄게 앞서 나가고, 실수로 큰 값을 준 것과 구분되지 않는다.
pub(crate) static SAMPLE_LEAD_PERIODS: LazyLock<Option<u64>> = LazyLock::new(|| {
    let raw = servo_config::pref!(gfx_sample_lead_periods);
    (0..=4).contains(&raw).then_some(raw as u64)
});

/// 디바이스 -> 그 디바이스가 그리는 모니터.
///
/// ★커밋 실패 로그가 "어느 타일이 죽었나" 를 말할 수 있게 하는 유일한 연결이다.★ 커밋
/// 경로는 디바이스 포인터만 들고 다니므로, 이 맵이 없으면 TDR 로 한 타일이 죽은 것과 넷이
/// 다 죽은 것이 같은 로그로 보인다.
static DEVICE_MONITOR: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);

pub(crate) fn remember_device_monitor(device: usize, monitor: usize) {
    if let Ok(mut guard) = DEVICE_MONITOR.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .insert(device, monitor);
    }
}

/// 실패 로그가 디바이스를 출력 이름으로 옮길 때 쓴다.
pub(crate) fn monitor_for_device(device: usize) -> Option<usize> {
    let guard = DEVICE_MONITOR.lock().ok()?;
    guard.as_ref()?.get(&device).copied()
}

// ★디바이스 뮤텍스는 `Arc` 로 나눠 갖지 않고 프로세스 수명으로 누수시킨다.★ 과거에 제안된
// `Arc<Mutex<()>> + transmute 로 수명 늘리기` 는 구조체 필드 드롭 순서(선언 순) 때문에
// unsound 하다: `_inner: Arc<...>` 가 `_held: MutexGuard<'static, ...>` 보다 먼저 드롭되므로,
// 그 `Arc` 가 마지막 참조였다면 뮤텍스가 먼저 해제되고 그다음에야 이미 해제된 뮤텍스를
// 가리키는 가드가 드롭된다 -- use-after-free 다. `transmute` 는 이걸 컴파일러로부터 숨길
// 뿐이다. 디바이스는 넷이고 프로세스 내내 살므로, `&'static Mutex<()>` 로 한 번 누수시키는
// 것은 실질적으로 정적 할당이다. 이러면 가드의 수명이 그 뮤텍스의 실제 수명(= 프로세스 전체)
// 과 타입 그대로 일치해서, 드롭 순서에 정확성이 매달리지 않는다.
static DEVICE_LOCKS: Mutex<Option<HashMap<usize, &'static Mutex<()>>>> = Mutex::new(None);

/// 디바이스마다 하나. 위 주석의 이유로 프로세스 수명으로 누수시킨다.
fn device_lock(device: usize) -> &'static Mutex<()> {
    let mut guard = DEVICE_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashMap::new)
        .entry(device)
        .or_insert_with(|| &*Box::leak(Box::new(Mutex::new(()))))
}

/// 이 가드를 잡는 쪽이 누구인가. ★두 역할의 대기 시간은 섞으면 안 된다.★
///
/// `Painter` 가 기다린 시간의 상한은 `Commit()` 하나여야 한다 -- 그것이 모듈 주석의 주장이고
/// `LOCKWAIT painter_us_max` 가 그 검산이다. 반대 방향(셸이 painter 의 `end_frame` 을 기다린
/// 시간)은 페인터 임계구역이 서피스 루프 전체를 포함하므로(Ruling 18) 설계상 훨씬 길다.
/// 둘을 한 통에 넣으면 그 검산이 정상인 벽에서도 실패하고, 그 실패가 무엇을 뜻하는지 아무도
/// 말할 수 없다.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GuardRole {
    /// 콘텐츠 쪽(페인터 스레드의 `end_frame`·비디오 fast-path, `begin_frame` 의 자기복구).
    Painter,
    /// 셸 스레드가 틱에서 내는 커밋.
    Tick,
}

/// ★디바이스를 만지기 직전에 부른다.★ 반환값을 그 작업이 끝날 때까지 들고 있는다.
/// `role` 은 대기 시간을 어느 통에 넣을지만 정한다.
///
/// ★재진입 불가다.★ 이미 이 디바이스의 가드를 쥔 채로 다시 부르면 자기 자신과 데드락한다 --
/// 그래서 커밋 경로는 가드를 잡는 정문(`commit_device_at_tick`)과 이미 쥐고 있을 때만 쓰는
/// `_locked` 변형을 이름으로 갈라 둔다.
pub(crate) fn device_guard(device: usize, role: GuardRole) -> std::sync::MutexGuard<'static, ()> {
    let lock = device_lock(device);
    let start = qpc_now();
    let held = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let (Some(start), Some(now)) = (start, qpc_now()) {
        record_lock_wait(role, now.saturating_sub(start));
    }
    held
}

static LOCK_PAINTER_N: AtomicU64 = AtomicU64::new(0);
static LOCK_PAINTER_MAX: AtomicU64 = AtomicU64::new(0);
static LOCK_PAINTER_SUM: AtomicU64 = AtomicU64::new(0);
static LOCK_TICK_N: AtomicU64 = AtomicU64::new(0);
static LOCK_TICK_MAX: AtomicU64 = AtomicU64::new(0);
static LOCK_TICK_SUM: AtomicU64 = AtomicU64::new(0);

fn record_lock_wait(role: GuardRole, ticks: u64) {
    let Some(freq) = qpc_frequency() else { return };
    let us = ticks.saturating_mul(1_000_000) / freq.max(1);
    let (n, sum, max) = match role {
        GuardRole::Painter => (&LOCK_PAINTER_N, &LOCK_PAINTER_SUM, &LOCK_PAINTER_MAX),
        GuardRole::Tick => (&LOCK_TICK_N, &LOCK_TICK_SUM, &LOCK_TICK_MAX),
    };
    n.fetch_add(1, Ordering::Relaxed);
    sum.fetch_add(us, Ordering::Relaxed);
    max.fetch_max(us, Ordering::Relaxed);
}

/// 디바이스 가드 대기 한 줄. `output_grid` 의 프로브 스레드가 초당 한 번 부른다.
///
/// ★예전에는 스케줄러 스레드가 `OUTCOMMIT` 의 꼬리에 실어 냈다.★ 그 스레드가 사라졌으므로
/// 표본을 비울 사람이 없어지고, 비우지 않으면 `*_SUM` 이 창을 넘어 새서 평균이 의미를 잃는다.
/// 방출자는 하나, 창도 하나여야 한다.
///
/// 읽는 법: `painter_us_max` 가 이 모듈의 주장("가드 안에서 하는 일은 `Commit()` 하나")을
/// 검산한다. 커지면 가드 안에 `Commit()` 아닌 것이 들어온 것이다.
pub(crate) fn emit_lockwait() {
    let pn = LOCK_PAINTER_N.swap(0, Ordering::Relaxed);
    let ps = LOCK_PAINTER_SUM.swap(0, Ordering::Relaxed);
    let pm = LOCK_PAINTER_MAX.swap(0, Ordering::Relaxed);
    let tn = LOCK_TICK_N.swap(0, Ordering::Relaxed);
    let ts = LOCK_TICK_SUM.swap(0, Ordering::Relaxed);
    let tm = LOCK_TICK_MAX.swap(0, Ordering::Relaxed);
    if pn == 0 && tn == 0 {
        return;
    }
    warn!(
        "LOCKWAIT painter_n={pn} painter_us_max={pm} painter_us_avg={} \
         tick_n={tn} tick_us_max={tm} tick_us_avg={}",
        ps / pn.max(1),
        ts / tn.max(1),
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::{
        GuardRole, LOCK_PAINTER_MAX, LOCK_PAINTER_N, LOCK_PAINTER_SUM, LOCK_TICK_MAX, LOCK_TICK_N,
        LOCK_TICK_SUM, record_lock_wait,
    };

    fn drain() -> (u64, u64, u64, u64) {
        let pn = LOCK_PAINTER_N.swap(0, Ordering::Relaxed);
        let pm = LOCK_PAINTER_MAX.swap(0, Ordering::Relaxed);
        LOCK_PAINTER_SUM.swap(0, Ordering::Relaxed);
        let tn = LOCK_TICK_N.swap(0, Ordering::Relaxed);
        let tm = LOCK_TICK_MAX.swap(0, Ordering::Relaxed);
        LOCK_TICK_SUM.swap(0, Ordering::Relaxed);
        (pn, pm, tn, tm)
    }

    /// ★두 역할이 같은 통에 들어가면 `painter_us_max` 의 검산이 무의미해진다.★ 그 값이 묻는
    /// 것은 "콘텐츠 쪽이 커밋 하나를 기다린 시간" 인데, 셸이 페인터의 `end_frame` 서피스
    /// 루프 전체를 기다린 시간은 설계상 훨씬 길다(Ruling 18). 둘을 섞으면 정상인 벽도
    /// 탈락하고, 그 탈락이 무엇을 뜻하는지 아무도 말할 수 없다.
    ///
    /// 이 테스트는 그 분리를 지킨다 -- 두 역할을 같은 카운터로 되돌리면 실패한다.
    #[test]
    fn lock_wait_is_counted_per_acquirer_role() {
        // 이 스위트에서 이 전역들을 만지는 테스트는 이것뿐이다. 앞선 값이 남아 있을 수
        // 있으니 먼저 비운다.
        let _ = drain();

        // 틱 단위로 넣는다. 실제 값은 QPC 주파수에 따라 달라지므로 크기 비교만 단언한다.
        record_lock_wait(GuardRole::Painter, 1_000);
        record_lock_wait(GuardRole::Tick, 1_000_000);
        record_lock_wait(GuardRole::Tick, 2_000_000);

        let (pn, pm, tn, tm) = drain();
        assert_eq!(pn, 1, "페인터 표본은 하나다");
        assert_eq!(tn, 2, "틱 표본은 둘이다");
        assert!(
            pm < tm,
            "셸의 긴 대기가 페인터 쪽 최댓값을 오염시키면 안 된다: painter_max={pm} tick_max={tm}"
        );

        // 꺼낸 뒤에는 둘 다 0 이어야 한다 -- 창이 1 초라 다음 창으로 새면 안 된다.
        assert_eq!(drain(), (0, 0, 0, 0));
    }
}

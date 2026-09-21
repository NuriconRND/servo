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
//! ★이 뮤텍스는 생산 스레드와 무관하지 않다.★ 설계 문서에 그렇게 적혀 있었지만 틀렸다 --
//! 이것을 잡는 곳은 다섯이고, 스케줄러를 뺀 넷은 전부 생산·렌더·셸 스레드 위에 있다:
//! 1. 스케줄러 스레드(여기),
//! 2. 그 타일의 painter 가 `end_frame` 에서(그 painter 는 그 순간 **ANGLE GL 락을 쥐고
//!    있다** -- `painter.rs` 의 타일 렌더 전체가 그 락 안이다),
//! 3. 비디오 fast-path `present_external_only`(역시 ANGLE GL 락 안, 타일당 최대 ~60/s),
//! 4. `begin_frame` → `flush_deferred_commit` 의 자기복구 커밋(역시 WR 렌더 경로 안),
//! 5. 셸 스레드의 즉시 커밋(격자 미확보 폴백, 스케줄러 사망 폴백).
//!
//! 즉 이 뮤텍스를 기다리는 쪽은 ANGLE GL 락을 쥔 채로 기다릴 수 있고, 같은 디바이스의
//! WebGL 스레드가 그 뒤에 줄을 선다. 과거에 한 번 터졌던 회귀 부류다(생산 스레드가 커밋
//! 대기에 동기화되어 처리량을 잃은 건). 그래서 **기다림의 상한을 `Commit()` 하나(~0.02ms)로
//! 못 박는다** -- 어느 쪽이든 가드 안에서 하는 일은 `Commit()` 뿐이고, `note_dwm_phase`
//! (전역 뮤텍스 + DWM 조회)와 실패 로그와 위상 기록은 전부 가드 밖으로 뺀다. 그 상한이
//! 지켜지는지는 `lock_wait_painter_us_max` 가 지켜본다.
//!
//! ★다섯을 하나씩 챙기는 방식은 이미 세 번 실패했다.★ 2·3·5 의 가드 누락이 따로따로
//! 발견됐다. 그래서 가드를 잡는 책임을 커밋의 정문
//! (`dcomp_compositor::commit_device_ptr`)으로 옮겼고, `Commit()` 을 실제로 부르는 자리는
//! 크레이트 전체에서 `dcomp_compositor::raw_commit` 하나뿐이다. 새 커밋 지점은 그 함수를
//! 지나갈 수밖에 없다.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock};

use log::warn;

use crate::output_grid::{qpc_frequency, qpc_now};

/// 정렬 pref(`gfx_present_align_per_output_pct`)를 **한 번만** 읽어 캐시한다.
/// `Some(pct)` = 켜짐(`0..=99`), `None` = 꺼짐.
///
/// ★`pref!` 는 `PREFERENCES.read().unwrap()` 으로 펼쳐지는 RwLock 획득이다.★ 이 pref 를
/// 묻는 자리는 painter 마다 `end_frame` 당 하나 + flush 당 하나라, 꺼져 있을 때도 프레임당
/// 다섯 번의 락이 벽의 가장 뜨거운 경로에 새로 생긴다. "꺼져 있으면 오늘과 같다" 가 이
/// 작업 전체의 전제이므로 그 비용은 0 이어야 한다.
///
/// `LazyLock` 으로 굳혀도 안전한 이유: 이 pref 는 기동 시 커맨드라인에서 한 번 정해지고
/// 실행 중에 바뀌지 않는다(Ruling 9 에서 리뷰어가 `Preferences` 의 모든 쓰기 지점을
/// 트리에서 감사해 확인했다). 같은 파일의 `PRESENT_SYNC_INTERVAL` 과 같은 이유·같은 방식.
pub(crate) static ALIGN_PCT: LazyLock<Option<u64>> = LazyLock::new(|| {
    let raw = servo_config::pref!(gfx_present_align_per_output_pct);
    (0..=99).contains(&raw).then_some(raw as u64)
});

#[derive(Default, Clone, Copy)]
pub(crate) struct SchedulerStats {
    /// 실제로 큐에 올린 커밋 수.
    pub scheduled: u64,
    /// 스케줄러가 없어 호출 스레드에서 즉시 커밋한 수. ★`scheduled` 와 섞지 않는다★ --
    /// 이것들은 스케줄된 적이 없으므로 slip 표본도 lock_wait 표본도 만들지 않는다.
    pub immediate: u64,
    /// 마감보다 늦게 커밋한 시간. 크면 스케줄러가 병목이다.
    pub slip_n: u64,
    pub slip_us_max: u64,
    pub slip_us_sum: u64,
    /// 콘텐츠 쪽이 디바이스 뮤텍스를 기다린 시간. ★Task 6 기준 4 가 읽어야 하는 것이
    /// 이것이다★ -- 상한이 `Commit()` 하나라는 C3 의 주장을 검산한다.
    pub lock_painter_n: u64,
    pub lock_painter_us_max: u64,
    pub lock_painter_us_sum: u64,
    /// 스케줄러가 기다린 시간. Ruling 18 때문에 페인터 임계구역이 서피스 루프 전체를
    /// 포함하므로 설계상 길다 -- 기준 4 와 섞으면 안 된다.
    pub lock_sched_n: u64,
    pub lock_sched_us_max: u64,
    pub lock_sched_us_sum: u64,
}

struct Shared {
    /// (마감 QPC, 디바이스 포인터, 모니터). 작은 큐라 정렬 없이 최소값을 훑는다 -- 타일
    /// 수만큼이다.
    queue: Mutex<Vec<(u64, usize, usize)>>,
    condvar: Condvar,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

static SCHEDULED: AtomicU64 = AtomicU64::new(0);
static IMMEDIATE: AtomicU64 = AtomicU64::new(0);
static SLIP_N: AtomicU64 = AtomicU64::new(0);
static SLIP_MAX: AtomicU64 = AtomicU64::new(0);
static SLIP_SUM: AtomicU64 = AtomicU64::new(0);
static LOCK_PAINTER_N: AtomicU64 = AtomicU64::new(0);
static LOCK_PAINTER_MAX: AtomicU64 = AtomicU64::new(0);
static LOCK_PAINTER_SUM: AtomicU64 = AtomicU64::new(0);
static LOCK_SCHED_N: AtomicU64 = AtomicU64::new(0);
static LOCK_SCHED_MAX: AtomicU64 = AtomicU64::new(0);
static LOCK_SCHED_SUM: AtomicU64 = AtomicU64::new(0);

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
fn take_phases() -> Vec<(usize, Vec<f64>)> {
    let Ok(mut guard) = PHASES.lock() else {
        return Vec::new();
    };
    match guard.as_mut() {
        Some(map) => map.drain().collect(),
        None => Vec::new(),
    }
}

/// 스케줄러 스레드가 살아서 큐를 비우고 있는지. ★거짓이면 즉시 커밋으로 대체한다★ --
/// 아무도 비우지 않는 큐에 쌓기만 하면 그 타일은 다시는 커밋되지 않고 벽이 멈춘다. 늦게
/// 커밋하는 것은 아예 안 하는 것보다 항상 낫다는 것이 이 폴백의 근거다.
static SCHEDULER_ALIVE: AtomicBool = AtomicBool::new(false);

// ★디바이스 뮤텍스는 `Arc` 로 나눠 갖지 않고 프로세스 수명으로 누수시킨다.★ 브리프가 제안한
// `Arc<Mutex<()>> + transmute 로 수명 늘리기` 는 구조체 필드 드롭 순서(선언 순) 때문에
// unsound 하다: `_inner: Arc<...>` 가 `_held: MutexGuard<'static, ...>` 보다 먼저 드롭되므로,
// 그 `Arc` 가 마지막 참조였다면 뮤텍스가 먼저 해제되고 그다음에야 이미 해제된 뮤텍스를
// 가리키는 가드가 드롭된다 -- use-after-free 다. `transmute` 는 이걸 컴파일러로부터 숨길
// 뿐이다. 디바이스는 넷이고 프로세스 내내 살므로, `&'static Mutex<()>` 로 한 번 누수시키는
// 것은 실질적으로 정적 할당이다. 이러면 가드의 수명이 그 뮤텍스의 실제 수명(=프로세스 전체)
// 과 타입 그대로 일치해서, 드롭 순서에 정확성이 매달리지 않는다.
static DEVICE_LOCKS: Mutex<Option<HashMap<usize, &'static Mutex<()>>>> = Mutex::new(None);

/// 디바이스마다 하나. ★프로세스 수명으로 누수시킨다★ -- 디바이스는 넷이고 프로세스 내내
/// 살므로 실질적으로 정적 할당이다. `Arc` + 수명 늘리기를 쓰면 가드와 소유권의 드롭 순서에
/// 정확성이 매달리는데, 여기서는 타입이 스스로 옳다.
fn device_lock(device: usize) -> &'static Mutex<()> {
    let mut guard = DEVICE_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashMap::new)
        .entry(device)
        .or_insert_with(|| &*Box::leak(Box::new(Mutex::new(()))))
}

/// 이 가드를 잡는 쪽이 누구인가. ★두 역할의 대기 시간은 섞으면 안 된다.★
///
/// Task 6 의 기준 4(`lock_wait_us_max < 100µs`)가 뜻하는 것은 **콘텐츠 쪽이 스케줄러의 커밋
/// 하나를 기다린 시간**이다 -- 그 상한이 `Commit()` 하나라는 것이 C3 의 주장이고, 기준 4 는
/// 그 주장을 검산한다. 그런데 반대 방향, 즉 스케줄러가 페인터의 `end_frame` 을 기다린 시간은
/// Ruling 18 때문에 서피스 루프 전체를 포함해 **설계상 훨씬 길다**. 둘을 한 통에 넣으면
/// 기준 4 는 정상인 벽에서도 실패하고, 그 실패가 무엇을 뜻하는지 아무도 말할 수 없다.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GuardRole {
    /// 콘텐츠 쪽(페인터 스레드의 `end_frame`·비디오 fast-path, 셸 스레드의 즉시 커밋).
    /// 기준 4 가 읽어야 하는 것은 이 값이다.
    Painter,
    /// 커밋 스케줄러 스레드. 이쪽이 긴 것은 페인터가 가드를 오래 쥐었다는 뜻이고,
    /// `slip` 과 같은 이야기를 한다.
    Scheduler,
}

/// ★디바이스를 만지기 직전에 부른다.★ 반환값을 그 작업이 끝날 때까지 들고 있는다 --
/// 스케줄러와 그 타일의 painter 가 같은 디바이스를 동시에 만지지 못하게 막는 것이 이 가드의
/// 전부다. `role` 은 대기 시간을 어느 통에 넣을지만 정한다(위 `GuardRole` 참고).
///
/// ★재진입 불가다.★ 이미 이 디바이스의 가드를 쥔 채로 다시 부르면 자기 자신과 데드락한다 --
/// 그래서 커밋 경로는 가드를 잡는 정문(`commit_device_ptr`)과 이미 쥐고 있을 때만 쓰는
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

fn record_lock_wait(role: GuardRole, ticks: u64) {
    let Some(freq) = qpc_frequency() else { return };
    let us = ticks.saturating_mul(1_000_000) / freq.max(1);
    let (n, sum, max) = match role {
        GuardRole::Painter => (&LOCK_PAINTER_N, &LOCK_PAINTER_SUM, &LOCK_PAINTER_MAX),
        GuardRole::Scheduler => (&LOCK_SCHED_N, &LOCK_SCHED_SUM, &LOCK_SCHED_MAX),
    };
    n.fetch_add(1, Ordering::Relaxed);
    sum.fetch_add(us, Ordering::Relaxed);
    max.fetch_max(us, Ordering::Relaxed);
}

/// 이 디바이스의 커밋을 `deadline_qpc` 에 건다. 마감이 이미 지났으면 즉시 커밋된다.
/// `monitor` 는 커밋 시점에 위상을 잴 출력을 가리킨다 -- 스케줄만으로는 어느 격자에
/// 맞춰야 하는지 알 수 없다.
pub(crate) fn schedule(device: usize, monitor: usize, deadline_qpc: u64) {
    let shared = SHARED.get_or_init(|| {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Vec::new()),
            condvar: Condvar::new(),
        });
        let worker = shared.clone();
        match std::thread::Builder::new()
            .name(String::from("DcompCommitScheduler"))
            .spawn(move || scheduler_loop(&worker))
        {
            // 스레드가 실제로 떴을 때만 켠다 -- 이 플래그가 "큐에 넣어도 누군가 비운다" 를
            // 보장하는 유일한 근거다.
            Ok(_) => SCHEDULER_ALIVE.store(true, Ordering::Relaxed),
            Err(error) => warn!("[commitsched] 스레드를 띄우지 못했다: {error}"),
        }
        shared
    });
    if !SCHEDULER_ALIVE.load(Ordering::Relaxed) {
        // 스케줄러가 없다(뜨지 못했거나 이미 죽었다). 그래도 큐에 넣으면 아무도 비우지 않아
        // 이 타일은 영영 커밋되지 않고 벽이 멈춘다 -- 오늘 이전의 동작인 즉시 커밋으로
        // 떨어진다. 통계에는 잡아 둔다: 스케줄이 아니라 즉시 커밋이었다는 사실은 로그가 아닌
        // 별도 계수가 필요하지만, 최소한 "커밋은 일어났다" 는 여기서 놓치지 않는다.
        //
        // ★위상은 여기서 기록하지 않는다.★ 이 경로는 정렬 기능 자체가 죽었을 때만 타므로
        // "격자 어디에 떨어졌나" 라는 질문이 성립하지 않는다 -- 기록하면 정상적으로
        // 정렬된 표본들의 p50 을 의미 없는 값으로 오염시킨다.
        //
        // ★가드는 `commit_device_ptr` 안에서 걸린다.★ 예전엔 이 줄이 가드 없이 커밋했는데,
        // 이것은 셸 스레드에서 돌고 그 순간 스레드형 페인터가 같은 디바이스의 가드를 쥔
        // `end_frame` 안에 있을 수 있다 -- 기능이 이미 퇴화한 상태에서 하필 동시 접근이 난다.
        // 커밋의 정문이 스스로 가드를 잡게 바꾸면서 이 자리도 손대지 않고 안전해졌다.
        crate::dcomp_compositor::commit_device_ptr(device);
        IMMEDIATE.fetch_add(1, Ordering::Relaxed);
        return;
    }
    {
        let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        upsert(&mut queue, device, monitor, deadline_qpc);
    }
    SCHEDULED.fetch_add(1, Ordering::Relaxed);
    shared.condvar.notify_one();
}

fn take_stats() -> SchedulerStats {
    SchedulerStats {
        scheduled: SCHEDULED.swap(0, Ordering::Relaxed),
        immediate: IMMEDIATE.swap(0, Ordering::Relaxed),
        slip_n: SLIP_N.swap(0, Ordering::Relaxed),
        slip_us_max: SLIP_MAX.swap(0, Ordering::Relaxed),
        slip_us_sum: SLIP_SUM.swap(0, Ordering::Relaxed),
        lock_painter_n: LOCK_PAINTER_N.swap(0, Ordering::Relaxed),
        lock_painter_us_max: LOCK_PAINTER_MAX.swap(0, Ordering::Relaxed),
        lock_painter_us_sum: LOCK_PAINTER_SUM.swap(0, Ordering::Relaxed),
        lock_sched_n: LOCK_SCHED_N.swap(0, Ordering::Relaxed),
        lock_sched_us_max: LOCK_SCHED_MAX.swap(0, Ordering::Relaxed),
        lock_sched_us_sum: LOCK_SCHED_SUM.swap(0, Ordering::Relaxed),
    }
}

fn scheduler_loop(shared: &Arc<Shared>) {
    let Some(freq) = qpc_frequency() else {
        warn!("[commitsched] QPC 주파수를 읽지 못했다; 스케줄러를 멈춘다");
        // 이 스레드는 여기서 끝난다 -- 이후로는 큐에 넣어도 아무도 비우지 않는다. 플래그를
        // 내려 그 순간부터 `schedule` 이 즉시 커밋으로 폴백하게 한다.
        SCHEDULER_ALIVE.store(false, Ordering::Relaxed);
        return;
    };
    // ★OUTCOMMIT 은 여기서 낸다.★ 예전엔 타일 4 개 각자의 `maybe_emit_bind_profile` 이
    // 독립된 ~1 초 타이머로 이 표본들을 비웠다 -- 즉 한 줄이 실제로는 임의의 ~0.25 초
    // 조각이었고, 그 창이 짧아진 만큼 `slip_us_max`/`lock_wait_us_max` 도 작게 나와
    // Task 6 판정 기준 4/5 를 엉뚱한 이유로 통과시킬 뻔했다(Fix round 1, Ruling 16). 표본을
    // 만드는 스레드가 유일한 창을 재는 것이 맞다 -- 방출자는 하나, 창도 하나.
    let mut last_emit = std::time::Instant::now();
    loop {
        // 때가 된 것을 전부 꺼낸다. ★기다림은 락을 쥔 채 이 안쪽 루프에서 한다★ --
        // `condvar.wait*` 는 가드를 돌려주므로 그것을 그대로 다시 쓰면 되고, 깨자마자 놓았다가
        // 바깥 루프 맨 위에서 다시 잡을 이유가 없다(그 한 사이클이 예전 구조의 잉여였다).
        let due: Vec<(u64, usize, usize)> = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                let Some(now) = qpc_now() else {
                    // 시계를 못 읽으면 큐를 비워 폴백한다 -- 붙들고 있으면 화면이 멈춘다.
                    break queue.drain(..).collect();
                };
                let mut ready = Vec::new();
                queue.retain(|&(deadline, device, monitor)| {
                    if deadline <= now {
                        ready.push((deadline, device, monitor));
                        false
                    } else {
                        true
                    }
                });
                if !ready.is_empty() {
                    break ready;
                }
                // 가장 이른 마감까지 잔다. 큐가 비면 알림을 기다린다.
                let wait = queue.iter().map(|&(d, _, _)| d).min().map(|d| {
                    let ticks = d.saturating_sub(now);
                    std::time::Duration::from_secs_f64(ticks as f64 / freq as f64)
                });
                queue = match wait {
                    Some(duration) => {
                        shared
                            .condvar
                            .wait_timeout(queue, duration)
                            .unwrap_or_else(|e| e.into_inner())
                            .0
                    },
                    None => shared
                        .condvar
                        .wait(queue)
                        .unwrap_or_else(|e| e.into_inner()),
                };
            }
        };

        for (deadline, device, monitor) in due {
            if let Some(now) = qpc_now() {
                let slip = now.saturating_sub(deadline);
                let us = slip.saturating_mul(1_000_000) / freq.max(1);
                SLIP_N.fetch_add(1, Ordering::Relaxed);
                SLIP_SUM.fetch_add(us, Ordering::Relaxed);
                SLIP_MAX.fetch_max(us, Ordering::Relaxed);
            }
            // ★가드는 `Commit()` 한 줄만 감싼다.★ 이 뮤텍스를 기다리는 상대(그 타일의
            // painter, 비디오 fast-path)는 ANGLE GL 락을 쥔 채로 기다린다. 가드 안에서 하는
            // 일이 길어지면 그만큼 그 락도 길게 잡히고, 같은 디바이스의 WebGL 스레드가 그
            // 뒤에 줄을 선다 -- 생산 스레드가 커밋 대기에 동기화되는 그 회귀다. 그래서
            // `note_dwm_phase`(전역 뮤텍스 + DWM 조회 + 초당 한 번 로그 I/O)를 품은
            // `commit_device_ptr` 대신 순수 커밋만 부르고, 나머지는 전부 가드 밖으로 뺀다.
            //
            // ★실패 로그도 가드 밖이다.★ TDR·디바이스 제거 뒤에는 모든 커밋이 계속 실패하므로,
            // 실패 경로에 로그가 들어 있으면 임계구역이 `Commit()` 이 아니라 파일 쓰기 길이가
            // 된다 -- 그러면 위 상한이 무너진다. `hr` 만 들고 나와 밖에서 찍는다.
            let hr = {
                let _guard = device_guard(device, GuardRole::Scheduler);
                crate::dcomp_compositor::commit_device_ptr_locked(device)
            };
            // 가드를 푼 뒤에 한다. 아래 세 줄은 DComp 디바이스를 만지지 않으므로 painter 와
            // 겹쳐도 안전하다.
            crate::dcomp_compositor::note_commit_failure(hr, "commitsched");
            crate::dcomp_compositor::note_dwm_phase();

            // ★이것이 판정이다.★ 이 커밋이 **자기 출력** 격자의 어디에 떨어졌나.
            // 지금까지는 데스크톱 격자 하나만 보였으므로 나머지 셋이 어디 있는지 알 수 없었다.
            //
            // ★단, 이 위상은 순환이다.★ `Commit()` 이 돌아온 시각을 이 출력 격자에 접은
            // 값인데, 마감을 정한 것도 같은 격자다 -- DWM 이 실제로 언제 가져갔는지는 재지
            // 않으므로, 격자가 틀려도 이 값은 여전히 목표치를 가리킨다(재는 것은 사실상
            // 스케줄러가 제 마감을 얼마나 잘 맞췄나이고, 그건 `slip` 이 이미 잰다). 격자 자체의
            // 검산은 아래 `OUTCOMMIT` 줄의 `period_ms`/`measured` 와, 프로브가 커밋과 무관하게
            // 내는 `OUTPHASE` 다.
            //
            // ★게이트는 여기, 수집 자체에 건다.★ (Fix round 1, Ruling 15) 예전엔 `record_phase`
            // 호출만 `DCOMP_BIND_PROF` 뒤에 있고 `grid_for_monitor` 는 무조건 불렸다 --
            // `grid_for_monitor` 도 자기 락을 하나 잡으므로, 정렬 pref 는 켜져 있고
            // `SERVO_DCOMP_BIND_PROF` 는 꺼져 있는 실제 운영 구성에서 `PHASES` 가 프로세스
            // 수명 내내 무한히 자랐다(그 계측이 꺼져 있다는 사실 자체를 아무도 비우지 않았으므로).
            if *crate::dcomp_compositor::DCOMP_BIND_PROF {
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
            }
        }

        // 큐가 비어 마감이 하나도 없으면 위 `due` 가 비고, 이 `for` 도 돌지 않은 채 다음
        // 루프 회전 맨 위 `condvar.wait[_timeout]` 에서 잔다 -- 그동안은 여기 도달하지 않으므로
        // 아무 것도 찍지 않는다. 맞다: 비운 큐는 보고할 것도 없다는 뜻이다.
        if *crate::dcomp_compositor::DCOMP_BIND_PROF &&
            last_emit.elapsed() >= std::time::Duration::from_secs(1)
        {
            last_emit = std::time::Instant::now();
            emit_outcommit();
        }
    }
}

/// 모니터별 위상 한 줄씩 + 전역 스케줄러 통계 한 줄. 스케줄러 스레드가 표본을 만드는
/// 유일한 곳이라 그 스레드가 유일한 1 초 창으로 낸다(Fix round 1, Ruling 16).
///
/// ★모니터 줄에는 모니터 값만 싣는다★(Ruling 17) -- `take_stats()` 는 네 디바이스 전체의
/// 누적이라, 그걸 모니터별 줄마다 되풀이해 찍으면 그 줄이 마치 그 모니터만의 값인 것처럼
/// 거짓말한다. 전역 통계는 별도의 `total` 줄로 딱 한 번만 낸다.
fn emit_outcommit() {
    let stats = take_stats();
    let freq = qpc_frequency().unwrap_or(0);
    for (monitor, mut phase) in take_phases() {
        if phase.is_empty() {
            continue;
        }
        phase.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let at = |p: f64| phase[(((phase.len() - 1) as f64) * p).round() as usize];
        // ★주기와 그 출처를 같이 찍는다.★ 위 위상은 마감을 정한 격자로 다시 접은 값이라
        // 격자가 틀려도 목표치를 가리킨다 -- 즉 p50 만으로는 격자를 검산할 수 없다.
        // 60Hz 벽에서 `period_ms` 가 16.67 이 아니면(8.33, 12.50 …) 주기 추정이 틀린 것이고,
        // `measured=0` 이면 실측을 못 얻어 60Hz 가정값으로 돌고 있다는 뜻이다.
        let grid = crate::output_grid::grid_for_monitor(monitor);
        let period_ms = match (grid, freq) {
            (Some(grid), freq) if freq > 0 => grid.period_qpc as f64 * 1000.0 / freq as f64,
            _ => 0.0,
        };
        let measured = grid.is_some_and(|grid| grid.measured);
        // 이름이 없으면(프로브가 아직 못 열거했거나 핫플러그 직후) 물음표를 남긴다 -- 줄을
        // 통째로 거르면 정작 문제 있는 타일이 로그에서 사라진다.
        let name = crate::output_grid::name_for_monitor(monitor).unwrap_or_else(|| "?".into());
        warn!(
            "OUTCOMMIT out={name} monitor={monitor:#x} period_ms={period_ms:.2} measured={} \
             n={} phase p05={:.3} p50={:.3} p95={:.3}",
            u8::from(measured),
            phase.len(),
            at(0.05),
            at(0.50),
            at(0.95),
        );
    }
    // ★평균마다 제 분모를 쓴다.★ 예전에는 셋 다 `scheduled` 로 나눴는데, `scheduled` 에는
    // 스케줄된 적 없는 즉시 커밋까지 들어 있었고 `lock_wait` 표본은 스케줄당 둘이었다 --
    // 모집단이 서로 다른 값을 같은 수로 나누고 있었다는 뜻이다.
    // ★`lock_wait` 을 획득자 역할로 나눠 낸다.★ Task 6 기준 4 가 읽어야 하는 것은
    // `lock_wait_painter_us_max` 다 -- 콘텐츠 쪽이 스케줄러의 커밋 하나를 기다린 시간이고,
    // 그 상한이 `Commit()` 하나라는 주장의 검산이다. `lock_wait_sched_*` 는 반대 방향이라
    // Ruling 18 의 서피스 루프만큼 길 수 있고, 기준 4 와 섞으면 정상인 벽도 탈락한다.
    warn!(
        "OUTCOMMIT total scheduled={} immediate={} slip_n={} slip_us_max={} slip_us_avg={} \
         lock_wait_painter_n={} lock_wait_painter_us_max={} lock_wait_painter_us_avg={} \
         lock_wait_sched_n={} lock_wait_sched_us_max={} lock_wait_sched_us_avg={}",
        stats.scheduled,
        stats.immediate,
        stats.slip_n,
        stats.slip_us_max,
        stats.slip_us_sum / stats.slip_n.max(1),
        stats.lock_painter_n,
        stats.lock_painter_us_max,
        stats.lock_painter_us_sum / stats.lock_painter_n.max(1),
        stats.lock_sched_n,
        stats.lock_sched_us_max,
        stats.lock_sched_us_sum / stats.lock_sched_n.max(1),
    );
}

/// 큐에 마감을 넣거나 갱신한다. ★같은 디바이스는 덮어쓴다★ -- 밀린 커밋을 쌓으면 한
/// 주기에 여러 개가 나가고, 그것이 정확히 없애려는 현상이다. 스레드·COM 없이 테스트할 수
/// 있도록 `schedule` 에서 이 규칙만 갈라냈다.
fn upsert(queue: &mut Vec<(u64, usize, usize)>, device: usize, monitor: usize, deadline: u64) {
    if let Some(slot) = queue.iter_mut().find(|(_, d, _)| *d == device) {
        slot.0 = deadline;
        slot.2 = monitor;
    } else {
        queue.push((deadline, device, monitor));
    }
}

#[cfg(test)]
mod tests {
    use super::{GuardRole, record_lock_wait, take_stats, upsert};

    /// ★두 역할이 같은 통에 들어가면 Task 6 의 기준 4 가 무의미해진다.★ 기준 4 는 "콘텐츠
    /// 쪽이 스케줄러의 커밋 하나를 기다린 시간 < 100µs" 를 묻는데, 스케줄러가 페인터의
    /// `end_frame` 서피스 루프 전체를 기다린 시간은 Ruling 18 때문에 설계상 훨씬 길다. 둘을
    /// 섞으면 정상인 벽도 탈락하고, 그 탈락이 무엇을 뜻하는지 아무도 말할 수 없다.
    ///
    /// 이 테스트는 그 분리를 지킨다 -- 두 역할을 같은 카운터로 되돌리면 실패한다.
    #[test]
    fn lock_wait_is_counted_per_acquirer_role() {
        // 이 스위트에서 이 전역들을 만지는 테스트는 이것뿐이다. 앞선 값이 남아 있을 수 있으니
        // 먼저 비운다.
        let _ = take_stats();

        // 틱 단위로 넣는다. 실제 값은 QPC 주파수에 따라 달라지므로 크기 비교만 단언한다.
        record_lock_wait(GuardRole::Painter, 1_000);
        record_lock_wait(GuardRole::Scheduler, 1_000_000);
        record_lock_wait(GuardRole::Scheduler, 2_000_000);

        let stats = take_stats();
        assert_eq!(stats.lock_painter_n, 1, "페인터 표본은 하나다");
        assert_eq!(stats.lock_sched_n, 2, "스케줄러 표본은 둘이다");
        assert!(
            stats.lock_painter_us_max < stats.lock_sched_us_max,
            "스케줄러의 긴 대기가 페인터 쪽 최댓값을 오염시키면 안 된다: \
             painter_max={} sched_max={}",
            stats.lock_painter_us_max,
            stats.lock_sched_us_max
        );

        // 꺼낸 뒤에는 둘 다 0 이어야 한다 -- 창이 1 초라 다음 창으로 새면 안 된다.
        let drained = take_stats();
        assert_eq!(drained.lock_painter_n, 0);
        assert_eq!(drained.lock_sched_n, 0);
    }

    #[test]
    fn a_second_schedule_for_the_same_device_replaces_the_first() {
        let mut queue = Vec::new();
        upsert(&mut queue, 0xAA, 0x11, 100);
        upsert(&mut queue, 0xAA, 0x11, 250);
        assert_eq!(
            queue,
            vec![(250, 0xAA, 0x11)],
            "같은 디바이스는 쌓이지 않고 덮어써야 한다"
        );

        // 핫플러그: 같은 디바이스가 다른 모니터로 옮겨가면 마감뿐 아니라 모니터도 갱신돼야
        // 한다. 이 어서션이 없으면 `upsert` 안의 `slot.2 = monitor;` 를 지워도 위 어서션까지는
        // 전부 통과한다(Fix round 1, Ruling 19) -- 모니터가 안 바뀌는 회귀를 이 스위트가
        // 놓치고 있었다는 뜻이다. 여전히 쌓이지 않는다(길이 1)는 것도 같이 확인한다.
        upsert(&mut queue, 0xAA, 0x22, 400);
        assert_eq!(
            queue,
            vec![(400, 0xAA, 0x22)],
            "모니터가 바뀌어도 쌓이지 않고 마감·모니터 모두 덮어써야 한다"
        );
    }

    #[test]
    fn different_devices_each_keep_their_own_deadline() {
        let mut queue = Vec::new();
        upsert(&mut queue, 0xAA, 0x11, 100);
        upsert(&mut queue, 0xBB, 0x22, 250);
        upsert(&mut queue, 0xAA, 0x11, 300);
        queue.sort();
        assert_eq!(queue, vec![(250, 0xBB, 0x22), (300, 0xAA, 0x11)]);
    }
}

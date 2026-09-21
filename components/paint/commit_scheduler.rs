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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// (마감 QPC, 디바이스 포인터, 모니터). 작은 큐라 정렬 없이 최소값을 훑는다 -- 타일
    /// 수만큼이다.
    queue: Mutex<Vec<(u64, usize, usize)>>,
    condvar: Condvar,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

static SCHEDULED: AtomicU64 = AtomicU64::new(0);
static SLIP_MAX: AtomicU64 = AtomicU64::new(0);
static SLIP_SUM: AtomicU64 = AtomicU64::new(0);
static LOCK_MAX: AtomicU64 = AtomicU64::new(0);
static LOCK_SUM: AtomicU64 = AtomicU64::new(0);

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

/// ★`end_frame` 진입 시 부른다.★ 반환값을 프레임이 끝날 때까지 들고 있는다 -- 스케줄러와
/// 그 타일의 painter 가 같은 디바이스를 동시에 만지지 못하게 막는 것이 이 가드의 전부다.
pub(crate) fn device_guard(device: usize) -> std::sync::MutexGuard<'static, ()> {
    let lock = device_lock(device);
    let start = qpc_now();
    let held = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let (Some(start), Some(now)) = (start, qpc_now()) {
        record_lock_wait(now.saturating_sub(start));
    }
    held
}

fn record_lock_wait(ticks: u64) {
    let Some(freq) = qpc_frequency() else { return };
    let us = ticks.saturating_mul(1_000_000) / freq.max(1);
    LOCK_SUM.fetch_add(us, Ordering::Relaxed);
    LOCK_MAX.fetch_max(us, Ordering::Relaxed);
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
        crate::dcomp_compositor::commit_device_ptr(device);
        SCHEDULED.fetch_add(1, Ordering::Relaxed);
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
        slip_us_max: SLIP_MAX.swap(0, Ordering::Relaxed),
        slip_us_sum: SLIP_SUM.swap(0, Ordering::Relaxed),
        lock_wait_us_max: LOCK_MAX.swap(0, Ordering::Relaxed),
        lock_wait_us_sum: LOCK_SUM.swap(0, Ordering::Relaxed),
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
    // 아래 `'grab` 라벨 블록 안에 있는 `continue` 를 그 블록이 아니라 이 루프로 보내려면
    // 라벨을 밝혀야 한다(라벨 블록 안의 무라벨 `continue` 는 어느 쪽을 뜻하는지 모호해
    // rustc 가 거부한다).
    //
    // ★OUTCOMMIT 은 여기서 낸다.★ 예전엔 타일 4 개 각자의 `maybe_emit_bind_profile` 이
    // 독립된 ~1 초 타이머로 이 표본들을 비웠다 -- 즉 한 줄이 실제로는 임의의 ~0.25 초
    // 조각이었고, 그 창이 짧아진 만큼 `slip_us_max`/`lock_wait_us_max` 도 작게 나와
    // Task 6 판정 기준 4/5 를 엉뚱한 이유로 통과시킬 뻔했다(Fix round 1, Ruling 16). 표본을
    // 만드는 스레드가 유일한 창을 재는 것이 맞다 -- 방출자는 하나, 창도 하나.
    let mut last_emit = std::time::Instant::now();
    'sched: loop {
        // 때가 된 것을 전부 꺼낸다.
        // `let-else` 의 `else` 갈래는 반드시 발산해야 한다(값을 만들어 블록을 그 값으로
        // 끝내는 용도가 아니다) -- 그래서 바깥 블록에 이름을 붙이고 `break` 로 값을 낸다.
        let due: Vec<(u64, usize, usize)> = 'grab: {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            let Some(now) = qpc_now() else {
                // 시계를 못 읽으면 큐를 비워 폴백한다 -- 붙들고 있으면 화면이 멈춘다.
                break 'grab queue.drain(..).collect();
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
            if ready.is_empty() {
                // 가장 이른 마감까지 잔다. 큐가 비면 알림을 기다린다.
                let wait = queue.iter().map(|&(d, _, _)| d).min().map(|d| {
                    let ticks = d.saturating_sub(now);
                    std::time::Duration::from_secs_f64(ticks as f64 / freq as f64)
                });
                // `let _ = ...` 로 락 가드를 받으면 `let_underscore_lock` 이 deny 라 빌드가
                // 깨진다 -- 깨우자마자 버릴 가드라 바인딩 자체가 무의미하므로 `drop` 으로
                // 명시한다(재잠금은 다음 루프 회전 맨 위에서 다시 한다).
                match wait {
                    Some(duration) => {
                        drop(shared.condvar.wait_timeout(queue, duration));
                    },
                    None => {
                        drop(shared.condvar.wait(queue));
                    },
                }
                continue 'sched;
            }
            ready
        };

        for (deadline, device, monitor) in due {
            if let Some(now) = qpc_now() {
                let slip = now.saturating_sub(deadline);
                let us = slip.saturating_mul(1_000_000) / freq.max(1);
                SLIP_SUM.fetch_add(us, Ordering::Relaxed);
                SLIP_MAX.fetch_max(us, Ordering::Relaxed);
            }
            let _guard = device_guard(device);
            crate::dcomp_compositor::commit_device_ptr(device);

            // ★이것이 판정이다.★ 이 커밋이 **자기 출력** 격자의 어디에 떨어졌나.
            // 지금까지는 데스크톱 격자 하나만 보였으므로 나머지 셋이 어디 있는지 알 수 없었다.
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
    for (monitor, mut phase) in take_phases() {
        if phase.is_empty() {
            continue;
        }
        phase.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let at = |p: f64| phase[(((phase.len() - 1) as f64) * p).round() as usize];
        warn!(
            "OUTCOMMIT monitor={monitor:#x} n={} phase p05={:.3} p50={:.3} p95={:.3}",
            phase.len(),
            at(0.05),
            at(0.50),
            at(0.95),
        );
    }
    warn!(
        "OUTCOMMIT total scheduled={} slip_us_max={} slip_us_avg={} \
         lock_wait_us_max={} lock_wait_us_avg={}",
        stats.scheduled,
        stats.slip_us_max,
        stats.slip_us_sum / stats.scheduled.max(1),
        stats.lock_wait_us_max,
        stats.lock_wait_us_sum / stats.scheduled.max(1),
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
    use super::upsert;

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

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
    /// (마감 QPC, 디바이스 포인터). 작은 큐라 정렬 없이 최소값을 훑는다 -- 타일 수만큼이다.
    queue: Mutex<Vec<(u64, usize)>>,
    condvar: Condvar,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

static SCHEDULED: AtomicU64 = AtomicU64::new(0);
static SLIP_MAX: AtomicU64 = AtomicU64::new(0);
static SLIP_SUM: AtomicU64 = AtomicU64::new(0);
static LOCK_MAX: AtomicU64 = AtomicU64::new(0);
static LOCK_SUM: AtomicU64 = AtomicU64::new(0);

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
pub(crate) fn schedule(device: usize, deadline_qpc: u64) {
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
        crate::dcomp_compositor::commit_device_ptr(device);
        SCHEDULED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    {
        let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        upsert(&mut queue, device, deadline_qpc);
    }
    SCHEDULED.fetch_add(1, Ordering::Relaxed);
    shared.condvar.notify_one();
}

pub(crate) fn take_stats() -> SchedulerStats {
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
    'sched: loop {
        // 때가 된 것을 전부 꺼낸다.
        // `let-else` 의 `else` 갈래는 반드시 발산해야 한다(값을 만들어 블록을 그 값으로
        // 끝내는 용도가 아니다) -- 그래서 바깥 블록에 이름을 붙이고 `break` 로 값을 낸다.
        let due: Vec<(u64, usize)> = 'grab: {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            let Some(now) = qpc_now() else {
                // 시계를 못 읽으면 큐를 비워 폴백한다 -- 붙들고 있으면 화면이 멈춘다.
                break 'grab queue.drain(..).collect();
            };
            let mut ready = Vec::new();
            queue.retain(|&(deadline, device)| {
                if deadline <= now {
                    ready.push((deadline, device));
                    false
                } else {
                    true
                }
            });
            if ready.is_empty() {
                // 가장 이른 마감까지 잔다. 큐가 비면 알림을 기다린다.
                let wait = queue.iter().map(|&(d, _)| d).min().map(|d| {
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

        for (deadline, device) in due {
            if let Some(now) = qpc_now() {
                let slip = now.saturating_sub(deadline);
                let us = slip.saturating_mul(1_000_000) / freq.max(1);
                SLIP_SUM.fetch_add(us, Ordering::Relaxed);
                SLIP_MAX.fetch_max(us, Ordering::Relaxed);
            }
            let _guard = device_guard(device);
            crate::dcomp_compositor::commit_device_ptr(device);
        }
    }
}

/// 큐에 마감을 넣거나 갱신한다. ★같은 디바이스는 덮어쓴다★ -- 밀린 커밋을 쌓으면 한
/// 주기에 여러 개가 나가고, 그것이 정확히 없애려는 현상이다. 스레드·COM 없이 테스트할 수
/// 있도록 `schedule` 에서 이 규칙만 갈라냈다.
fn upsert(queue: &mut Vec<(u64, usize)>, device: usize, deadline: u64) {
    if let Some(slot) = queue.iter_mut().find(|(_, d)| *d == device) {
        slot.0 = deadline;
    } else {
        queue.push((deadline, device));
    }
}

#[cfg(test)]
mod tests {
    use super::upsert;

    #[test]
    fn a_second_schedule_for_the_same_device_replaces_the_first() {
        let mut queue = Vec::new();
        upsert(&mut queue, 0xAA, 100);
        upsert(&mut queue, 0xAA, 250);
        assert_eq!(queue, vec![(250, 0xAA)], "같은 디바이스는 쌓이지 않고 덮어써야 한다");
    }

    #[test]
    fn different_devices_each_keep_their_own_deadline() {
        let mut queue = Vec::new();
        upsert(&mut queue, 0xAA, 100);
        upsert(&mut queue, 0xBB, 250);
        upsert(&mut queue, 0xAA, 300);
        queue.sort();
        assert_eq!(queue, vec![(250, 0xBB), (300, 0xAA)]);
    }
}

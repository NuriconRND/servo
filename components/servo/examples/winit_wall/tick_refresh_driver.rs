/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 표출 틱이 발사하는 [`RefreshDriver`].

use std::sync::{Arc, Mutex};

use servo::RefreshDriver;

type StartFrameCallback = Box<dyn Fn() + Send + 'static>;

/// 다음 발사에서 부를 콜백들.
///
/// ★`Mutex` 하나면 된다 -- 조건변수가 없다.★ `DwmVsyncRefreshDriver` 는 자기 스레드가
/// 할 일이 생길 때까지 자야 해서 `Condvar` 가 필요했지만, 이 드라이버는 **스레드를 띄우지
/// 않는다.** 발사는 이미 존재하는 표출 틱이 하므로 기다릴 것이 없다.
#[derive(Default)]
struct Pending {
    callbacks: Mutex<Vec<StartFrameCallback>>,
}

impl Pending {
    fn fire(&self) {
        // ★락을 놓고 부른다.★ 콜백 안에서 `observe_next_frame` 이 다시 불리는 것이 정상
        // 흐름이고(`notify_will_paint` 가 매 렌더마다 재등록한다), 잠근 채 부르면 거기서
        // 교착한다. 그래서 큐를 통째로 꺼내 놓고 락을 놓은 뒤에 돈다.
        let callbacks: Vec<StartFrameCallback> = {
            let Ok(mut guard) = self.callbacks.lock() else {
                // 락이 중독됐다면 이미 다른 스레드가 콜백 안에서 패닉한 것이다. 표출을
                // 멈추는 것보다 이 틱을 건너뛰는 편이 낫다.
                return;
            };
            std::mem::take(&mut *guard)
        };
        for callback in callbacks {
            callback();
        }
    }
}

/// 표출 틱이 발사하는 [`RefreshDriver`].
///
/// ★기본 `TimerRefreshDriver` 는 전용 스레드에서 `gfx_refresh_hz` 주기로 도는 **두 번째**
/// 시계다.★ 이 셸에는 이미 표출 틱이 같은 주기로 돌고 있으므로, 둘은 같은 공칭 주기를
/// 임의의 위상으로 각자 센다. 실측에서 그 위상에 따라 `WALLCLOCK` 의 `user/s` 가
/// 0.02 와 6.65 사이를 오갔다(log_ani_debug_03/19, 04/08_temp).
///
/// 그래서 시계를 하나로 만든다. 이 드라이버는 **스레드를 띄우지 않고**, 콜백을 모아
/// 두었다가 표출 틱이 [`TickFireHandle::fire`] 를 부를 때 전부 발사한다.
///
/// ★세 프레임 클럭(Pacing / Timer / Vsync) 모두 이것을 쓴다★(2026-10-21). 예전에는
/// `FrameClock::Pacing` 에만 달려 있었고 나머지 둘은 `PaintTimerThread` 와 DwmFlush
/// 스레드가 자기 클럭으로 쐈는데, 그러면 "콜백 → 생산 → 렌더" 가 한 틱 안의 순서가
/// 아니라 두 시계의 경합이 된다. 달라지는 것은 ***누가 쏘나*** 뿐이고, 틱 에지를 누가
/// 내는지는 구성별로 그대로다 -- [`TickRefreshDriver::driven_by_paint_timer`] 참고.
pub(crate) struct TickRefreshDriver {
    pending: Arc<Pending>,
    /// paint 의 기본 타이머가 이 드라이버 대신 ★깨우기★ 를 내 줘야 하는가.
    /// [`RefreshDriver::needs_paint_timer_heartbeat`] 로 전달된다.
    paint_timer_edge: bool,
}

impl TickRefreshDriver {
    /// 틱 에지를 다른 것이 내는 구성용(페이싱 스레드 / DwmFlush 스레드).
    pub(crate) fn new() -> Self {
        Self {
            pending: Arc::new(Pending::default()),
            paint_timer_edge: false,
        }
    }

    /// ★틱 에지를 paint 의 기본 타이머가 내는 구성용(`FrameClock::Timer`).★
    ///
    /// 그 구성에는 주기적으로 이벤트 루프를 깨울 것이 `PaintTimerThread` 말고 없다 --
    /// 페이싱 스레드도 DwmFlush 스레드도 없다. 그 타이머가 콜백을 쏘던 것을 틱으로
    /// 옮겼으므로, ***깨우기만*** 계속 내 달라고 paint 에 알린다. 그렇게 해야 틱 에지가
    /// 생기는 시각이 종전 그대로이면서 발사만 틱으로 넘어간다.
    pub(crate) fn driven_by_paint_timer() -> Self {
        Self {
            pending: Arc::new(Pending::default()),
            paint_timer_edge: true,
        }
    }

    /// 발사용 핸들. ★Servo 생성자가 드라이버를 가져가 버리므로 그 전에 떠 둬야 한다.★
    pub(crate) fn handle(&self) -> TickFireHandle {
        TickFireHandle {
            pending: self.pending.clone(),
        }
    }
}

impl RefreshDriver for TickRefreshDriver {
    fn observe_next_frame(&self, start_frame_callback: StartFrameCallback) {
        if let Ok(mut guard) = self.pending.callbacks.lock() {
            guard.push(start_frame_callback);
        }
    }

    fn needs_paint_timer_heartbeat(&self) -> bool {
        self.paint_timer_edge
    }
}

/// 표출 틱이 드라이버를 깨우는 손잡이. 발사만 할 수 있다.
#[derive(Clone)]
pub(crate) struct TickFireHandle {
    pending: Arc<Pending>,
}

impl TickFireHandle {
    /// 이 틱까지 등록된 콜백을 전부 부른다. 없으면 아무 일도 하지 않는다.
    pub(crate) fn fire(&self) {
        self.pending.fire();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use servo::RefreshDriver;

    use super::*;

    /// ★콜백이 없는 틱은 아무 일도 하지 않는다.★ 애니메이션이 없는 정적 페이지의 매
    /// 프레임이 이 경우다 -- 여기서 패닉하면 화면이 멈춘다.
    #[test]
    fn fire_with_no_callbacks_is_a_noop() {
        let driver = TickRefreshDriver::new();
        let handle = driver.handle();
        handle.fire();
        handle.fire();
    }

    /// ★한 틱이 그때까지 쌓인 것을 **전부** 부른다.★ trait 주석이 그렇게 요구한다 --
    /// 하나만 부르고 나머지를 버리면 그 관찰자는 영영 굶는다.
    #[test]
    fn fire_calls_every_pending_callback_once() {
        let driver = TickRefreshDriver::new();
        let handle = driver.handle();
        let count = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let count = count.clone();
            driver.observe_next_frame(Box::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
            }));
        }

        handle.fire();
        assert_eq!(count.load(Ordering::SeqCst), 3, "셋 다 불려야 한다");

        // 두 번째 틱에는 남은 것이 없다 -- 발사는 큐를 비운다.
        handle.fire();
        assert_eq!(count.load(Ordering::SeqCst), 3, "다시 부르면 안 된다");
    }

    /// ★콜백 안에서 다시 등록하는 것이 정상 흐름이다.★ `notify_will_paint` 가 매 렌더마다
    /// `observe_next_frame` 을 다시 건다. 발사가 큐를 잠근 채 콜백을 부르면 여기서 교착한다.
    #[test]
    fn callback_may_register_again_without_deadlock() {
        let driver = Arc::new(TickRefreshDriver::new());
        let handle = driver.handle();
        let count = Arc::new(AtomicUsize::new(0));

        let again = driver.clone();
        let hits = count.clone();
        driver.observe_next_frame(Box::new(move || {
            hits.fetch_add(1, Ordering::SeqCst);
            // 다음 프레임을 다시 관찰한다 -- 실제 Servo 가 하는 일.
            again.observe_next_frame(Box::new(|| {}));
        }));

        handle.fire();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        // 재등록된 것이 다음 틱에 불릴 수 있어야 한다(= 큐에 들어갔다).
        handle.fire();
    }

    /// ★빈 틱이 몇 번 지나가도 나중에 몰아 발사하지 않는다.★ 틱은 돌지만 Servo 가 아직
    /// 관찰자를 안 붙인 기동 구간이 이 경우다.
    #[test]
    fn idle_ticks_do_not_accumulate() {
        let driver = TickRefreshDriver::new();
        let handle = driver.handle();
        for _ in 0..10 {
            handle.fire();
        }

        let count = Arc::new(AtomicUsize::new(0));
        let hits = count.clone();
        driver.observe_next_frame(Box::new(move || {
            hits.fetch_add(1, Ordering::SeqCst);
        }));
        handle.fire();
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "앞선 빈 틱이 쌓여 있으면 안 된다"
        );
    }

    /// ★박동을 요구하는 것은 `FrameClock::Timer` 구성뿐이다.★ 다른 둘에도 걸면 그
    /// 구성의 깨우기가 두 배가 되고, 그것이 이 작업이 없애려는 두 번째 시계다.
    #[test]
    fn only_the_paint_timer_edge_asks_for_a_heartbeat() {
        assert!(!TickRefreshDriver::new().needs_paint_timer_heartbeat());
        assert!(
            TickRefreshDriver::driven_by_paint_timer().needs_paint_timer_heartbeat(),
            "Timer 구성에는 깨울 사람이 PaintTimerThread 뿐이다"
        );
    }
}

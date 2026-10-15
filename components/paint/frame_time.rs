/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 셸이 알려준 프레임 시각.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use servo_base::cross_process_instant::CrossProcessInstant;

/// 셸이 마지막으로 알려준 프레임 시각, epoch 로부터의 나노초. `i64::MIN` 은 "없음".
///
/// ★전역인 이유.★ 이 값을 쓰는 `AnimationRefreshDriverObserver` 는 `Painter::new` 안에서
/// 만들어져(`painter.rs:843`) 셸이 닿을 수 없다. `Servo -> Paint -> Painter -> observer`
/// 로 배선하는 것보다 `note_present_tick`(`lib.rs:62`)의 선례를 따르는 편이 작다.
static FRAME_TIME_NANOS: AtomicI64 = AtomicI64::new(i64::MIN);

/// 셸이 표출 틱 시각을 알린다. ***틱을 낸 맨 앞에서*** 부르는 것이 요점이다 -- 렌더 준비
/// 비용이 섞이면 그만큼이 애니메이션 값의 지터가 된다.
///
/// 부르지 않는 임베더는 `stored_frame_time()` 이 `None` 이라 지금 동작 그대로다.
pub fn note_frame_time(frame_time: CrossProcessInstant) {
    let nanos = (frame_time - CrossProcessInstant::epoch()).whole_nanoseconds();
    // `i128 -> i64`. 범위를 넘으면(292 년) 저장하지 않는다 -- `i64::MIN` 과 충돌해
    // "없음" 으로 읽히는 것보다 조용히 무시하는 편이 낫다.
    let Ok(nanos) = i64::try_from(nanos) else {
        return;
    };
    if nanos == i64::MIN {
        return;
    }
    FRAME_TIME_NANOS.store(nanos, Ordering::Relaxed);
}

/// 저장된 값. 셸이 한 번도 부르지 않았으면 `None`.
pub(crate) fn stored_frame_time() -> Option<CrossProcessInstant> {
    let nanos = FRAME_TIME_NANOS.load(Ordering::Relaxed);
    if nanos == i64::MIN {
        return None;
    }
    Some(CrossProcessInstant::epoch() + time::Duration::nanoseconds(nanos))
}

/// 저장값이 아직 쓸 만한가.
///
/// ★셸이 부르다 멈추면 버려야 한다.★ 페이싱 스레드가 죽었을 때 굳은 값을 계속 쓰면
/// ***애니메이션 시각이 정지한다*** -- 저더보다 훨씬 나쁘다.
///
/// ★미래의 값은 버리지 않는다.★ 셸은 틱 시각을 재고 그 다음에 렌더를 부르므로 보통
/// 과거지만, 두 시계 읽기 사이에 역전이 보일 수 있다. 그것까지 폴백시키면 그 프레임만
/// 값이 튄다.
pub(crate) fn fresh_frame_time(
    stored: Option<CrossProcessInstant>,
    now: CrossProcessInstant,
    max_age: Duration,
) -> Option<CrossProcessInstant> {
    let stored = stored?;
    let age = now - stored;
    if age.is_negative() {
        return Some(stored);
    }
    (age <= time::Duration::try_from(max_age).ok()?).then_some(stored)
}

/// 틱에 실을 프레임 시각. 없거나 오래됐으면 `None` -- 호출자가 `now()` 로 떨어진다.
pub(crate) fn frame_time_for_tick(
    now: CrossProcessInstant,
    max_age: Duration,
) -> Option<CrossProcessInstant> {
    fresh_frame_time(stored_frame_time(), now, max_age)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use servo_base::cross_process_instant::CrossProcessInstant;

    use super::*;

    fn at(seconds: f64) -> CrossProcessInstant {
        CrossProcessInstant::epoch() + time::Duration::seconds_f64(seconds)
    }

    const MAX_AGE: Duration = Duration::from_millis(67); // 4 x 16.7ms

    /// ★셸이 아예 부르지 않으면 `None` 이다.★ `servoshell` 과 다른 임베더가 그렇다.
    /// 호출자는 이 경우 `now()` 를 쓰므로 ***지금 동작 그대로***여야 한다.
    #[test]
    fn nothing_stored_is_none() {
        assert_eq!(fresh_frame_time(None, at(100.0), MAX_AGE), None);
    }

    /// 방금 받은 값은 그대로 쓴다.
    #[test]
    fn a_recent_value_is_used() {
        let stored = at(100.0);
        assert_eq!(
            fresh_frame_time(Some(stored), at(100.005), MAX_AGE),
            Some(stored)
        );
    }

    /// ★셸이 부르다 멈추면 버린다.★ 페이싱 스레드가 죽었을 때 굳은 값을 계속 쓰면
    /// ***애니메이션 시각이 정지한다*** -- 저더보다 훨씬 나쁘다. 네 주기가 지나면
    /// `None` 을 돌려주어 호출자가 `now()` 로 떨어지게 한다.
    #[test]
    fn a_stale_value_is_dropped() {
        let stored = at(100.0);
        assert_eq!(fresh_frame_time(Some(stored), at(100.100), MAX_AGE), None);
    }

    /// 경계: 정확히 `max_age` 만큼 지난 값은 아직 쓴다. 한 쪽으로 정해 두지 않으면
    /// 주기와 정확히 맞아떨어지는 실행에서 매 프레임 폴백이 일어날 수 있다.
    #[test]
    fn exactly_max_age_is_still_fresh() {
        let stored = at(100.0);
        let now = stored + time::Duration::try_from(MAX_AGE).unwrap();
        assert_eq!(fresh_frame_time(Some(stored), now, MAX_AGE), Some(stored));
    }

    /// ★미래의 값도 쓴다.★ 셸은 틱 시각을 재고 ***그 다음에*** 렌더를 부르므로 보통
    /// 과거지만, 두 시계 읽기 사이에 역전이 보일 수 있다. 음수 나이를 "오래됐다" 로
    /// 잘못 판정하면 그 프레임만 폴백해 거기서 값이 튄다.
    #[test]
    fn a_value_slightly_in_the_future_is_used() {
        let stored = at(100.010);
        assert_eq!(
            fresh_frame_time(Some(stored), at(100.0), MAX_AGE),
            Some(stored)
        );
    }

    /// 저장과 조회가 왕복한다. 전역 상태라 다른 테스트와 간섭하지 않도록 이 테스트만
    /// 실제 저장소를 건드린다.
    #[test]
    fn store_and_read_round_trip() {
        let t = CrossProcessInstant::now();
        note_frame_time(t);
        assert_eq!(stored_frame_time(), Some(t));
    }
}

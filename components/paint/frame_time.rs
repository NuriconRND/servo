/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 셸이 알려준 프레임 시각.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use servo_base::cross_process_instant::CrossProcessInstant;
use servo_base::id::PainterId;
use servo_base::interval_summary::{percentiles, summarize_intervals};

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

/// 틱에 실을 프레임 시각. 셸 값이 없거나 오래됐으면 `now` 로 떨어진다.
///
/// ★폴백까지 여기서 한다.★ 두 호출 지점이 각자 `unwrap_or_else(now)` 하던 것을 모았다.
/// 계측이 빠진 틱 경로가 하나라도 있으면 `FRAMETIME` 이 "실제로 나간 수열" 을 재지 못한다
/// (`CommitSite` 를 인자로 받는 `raw_commit` 과 같은 이유).
pub(crate) fn frame_time_for_tick(
    painter_id: PainterId,
    now: CrossProcessInstant,
    max_age: Duration,
) -> CrossProcessInstant {
    let fresh = fresh_frame_time(stored_frame_time(), now, max_age);
    let delivered = fresh.unwrap_or(now);
    if *FRAME_TIME_PROF {
        record_delivered(painter_id, delivered, now, fresh.is_some());
    }
    delivered
}

// -------------------------------------------------------------------------------------
// FRAMETIME -- 틱에 실려 나간 프레임 시각의 수열.
//
// ★무엇을 재는가.★ 다른 지표는 전부 프레임을 세지만 이것은 그 프레임이 실은 값을 센다.
// 이 값은 스크립트로 건너가 페이지의 `requestAnimationFrame` 타임스탬프가 되고, 페이지는
// 그것으로 자기 위치를 계산한다. ***프레임이 초당 60 개 나가도 이 수열이 고르지 않으면
// 화면의 변위가 고르지 않다.***
//
// 왜 필요한가(2026-10-21): 같은 출력 페이지를 Chrome 은 single tile 에서 60fps 로 돌리는데
// winit_wall 은 떨린다. 그런데 우리 쪽 지표는 전부 깨끗하다 -- 초당 60~61 프레임이 전부
// 화면에 나가고(`WRRATE superseded=0`), 스크립트 스레드는 1%, 메인은 13% 다. 남은 변수가
// "그 프레임들이 어떤 시각값을 실었는가" 인데 그것을 재는 수단이 없었다.
//
// ★반드시 `RAFCLOCK` 과 함께 읽는다★(`components/script/raf_clock.rs`). 이쪽은 보낸 값이고
// 저쪽은 페이지가 받은 값이다. 둘이 다르면 그 사이 -- 스크립트 스레드의 틱 소비와
// `Document::resolve_frame_time` 의 단조 증가 클램프 -- 가 범인이다.
// -------------------------------------------------------------------------------------

/// `SERVO_FRAME_TIME_PROF` 게이트. 틱마다 묻는 자리라 env 읽기를 캐시한다.
static FRAME_TIME_PROF: LazyLock<bool> = LazyLock::new(|| {
    servo_config::debug_env::string(&servo_config::debug_env::FRAME_TIME_PROF)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
});

/// painter 하나의 한 창.
#[derive(Default)]
struct DeliveryWindow {
    start: Option<Instant>,
    previous: Option<CrossProcessInstant>,
    dt_ms: Vec<f64>,
    lag_ms: Vec<f64>,
    fallback: u32,
}

/// painter 별 창. 틱은 painter 스레드에서 오고 여럿이 같은 전역을 쓰므로 뮤텍스다 --
/// 초당 수백 번이라 경합이 문제되지 않는다. 게이트가 꺼져 있으면 아예 잠그지 않는다.
static DELIVERIES: LazyLock<Mutex<HashMap<PainterId, DeliveryWindow>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn millis(duration: time::Duration) -> f64 {
    duration.as_seconds_f64() * 1000.0
}

fn record_delivered(
    painter_id: PainterId,
    delivered: CrossProcessInstant,
    now: CrossProcessInstant,
    from_shell: bool,
) {
    let Ok(mut deliveries) = DELIVERIES.lock() else {
        return;
    };
    let window = deliveries.entry(painter_id).or_default();
    let at = Instant::now();
    let start = *window.start.get_or_insert(at);
    if !from_shell {
        window.fallback += 1;
    }
    if let Some(previous) = window.previous {
        window.dt_ms.push(millis(delivered - previous));
    }
    window.previous = Some(delivered);
    // ★음수를 자르지 않는다.★ `fresh_frame_time` 이 미래의 값도 쓰므로 `now - delivered`
    // 는 음수일 수 있다. 자르면 위상이 앞선 것을 못 본다.
    window.lag_ms.push(millis(now - delivered));

    if at.duration_since(start) < Duration::from_secs(1) {
        return;
    }
    window.start = Some(at);
    let mut dt = std::mem::take(&mut window.dt_ms);
    let mut lag = std::mem::take(&mut window.lag_ms);
    let fallback = std::mem::replace(&mut window.fallback, 0);
    // 로그는 락 밖에서 찍는다 -- 다른 painter 의 틱을 막지 않는다.
    drop(deliveries);

    let period_ms = crate::refresh_driver::paint_timer_period().as_secs_f64() * 1000.0;
    let (Some(intervals), Some(lag)) = (
        summarize_intervals(&mut dt, period_ms),
        percentiles(&mut lag),
    ) else {
        return;
    };
    if *crate::painter::LOG_ANIM_STEP_SEQ {
        log::warn!(
            "FRAMETIMESEQ painter={painter_id:?} n={} dt_ms=[{}]",
            intervals.n,
            intervals.sequence,
        );
    }
    log::warn!(
        "FRAMETIME painter={painter_id:?} n={} per_interval dup={} one={} skip1={} skipN={} \
         dt_ms p05={:.2} p50={:.2} p95={:.2} max={:.2} \
         lag_ms p50={:.2} p95={:.2} max={:.2} fallback={fallback}",
        intervals.n,
        intervals.dup,
        intervals.one,
        intervals.skip1,
        intervals.skip_n,
        intervals.p05,
        intervals.p50,
        intervals.p95,
        intervals.max,
        lag.p50,
        lag.p95,
        lag.max,
    );
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

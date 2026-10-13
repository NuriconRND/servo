/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 애니메이션 타임라인 값을 리프레시 격자에 맞춘다.

/// 타임라인 값을 리프레시 격자에 맞춘다.
///
/// ★왜 필요한가.★ 애니메이션 값은 `DocumentTimeline::update` 가 `CrossProcessInstant::now()`
/// 로 잡는데, 그것은 ***"스크립트가 그 함수에 도달한 순간"*** 이고 표출 격자와 무관하다.
/// 도달은 `TickAnimation` -> constellation -> script thread 큐를 거치므로 경로 지연이 매번
/// 다르고, 그 지터가 그대로 애니메이션 값이 된다. 실측(설계 문서 50): 틱이 60Hz 일 때 값의
/// 출처가 3.81% 이탈하고 화면값이 3.46% 이탈한다 -- 거의 1:1 이다. 15Hz 일 때는 여유가
/// 지연을 흡수해 이탈이 0.00% 다.
///
/// ★`while next + period <= raw { next += period }` 패턴.★ 셸의 표출 클럭
/// (`drive_present_clock_timer`)과 `TimerRefreshDriver::observe_next_frame` 이 같은 모양이고
/// 같은 이유다 -- "지금부터 한 주기" 로 재면 매 사이클에 처리 지연이 누적되어 ***구조적으로
/// 느려진다.*** 기준점에 주기를 더해 나가면 표류하지 않는다.
///
/// ★밀린 칸은 버린다.★ 긴 정지 뒤에 하나씩 밟으면 이미 지나간 순간들을 위한 프레임이
/// 쏟아져 애니메이션이 빨리 감기로 돈다. 디스플레이가 하는 일이 그것이다.
///
/// `period` 가 0 이하면 스냅하지 않고 `raw` 를 그대로 준다 -- pref 가 꺼진 경로다.
pub fn snap_animation_timeline(previous: Option<f64>, raw: f64, period: f64) -> f64 {
    if period <= 0.0 {
        return raw;
    }
    let Some(previous) = previous else {
        // 첫 호출은 기준점이다. 격자를 여기서 시작한다.
        return raw;
    };
    if raw <= previous {
        // ★단조.★ 시각이 뒤로 갔거나 같은 순간에 두 번 불렸다. 되감지 않는다.
        return previous;
    }
    let mut next = previous;
    while next + period <= raw {
        next += period;
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: f64 = 1.0 / 60.0; // 16.667ms

    /// 첫 호출은 기준점이다. ★여기서 0 이나 epoch 를 쓰면 애니메이션이 과거에서 시작해
    /// 순간이동한다.★
    #[test]
    fn first_call_uses_the_raw_time() {
        assert_eq!(snap_animation_timeline(None, 123.456, P), 123.456);
    }

    /// 꺼져 있으면(period <= 0) 손대지 않는다. ★이 경로가 지금 동작이고, 되돌릴 수 있는
    /// 유일한 수단이다.★
    #[test]
    fn period_zero_passes_the_raw_time_through() {
        assert_eq!(snap_animation_timeline(Some(1.0), 1.019, 0.0), 1.019);
        assert_eq!(snap_animation_timeline(Some(1.0), 1.019, -1.0), 1.019);
    }

    /// ★지터를 흡수한다. 이것이 이 함수의 전부다.★ 한 격자 칸 안에서는 raw 가 어디에
    /// 떨어지든 같은 값이 나온다 -- 17.0ms 뒤에 도착했든 18.5ms 뒤에 도착했든 둘 다
    /// 한 주기(16.67ms) 격자점을 받는다.
    #[test]
    fn absorbs_arrival_jitter_within_one_period() {
        let base = 1.0;
        let early = snap_animation_timeline(Some(base), base + 0.0170, P);
        let late = snap_animation_timeline(Some(base), base + 0.0185, P);
        assert_eq!(early, late, "같은 칸이면 같은 격자점이어야 한다");
        assert!(
            (early - (base + P)).abs() < 1e-9,
            "한 주기 뒤여야 한다: {early}"
        );
    }

    /// ★한 주기에 못 미치면 전진하지 않는다.★ 격자가 주기보다 촘촘해지면 스냅의 의미가
    /// 없다 -- 16.0ms 는 16.67ms 칸을 아직 못 넘었으므로 직전 값 그대로다. 이 경계를 잘못
    /// 잡으면(`next <= raw`) raw 를 넘어서는 격자점을 주어 ***애니메이션이 미래로 앞선다.***
    #[test]
    fn does_not_advance_before_a_full_period_has_passed() {
        let base = 1.0;
        assert_eq!(snap_animation_timeline(Some(base), base + 0.0160, P), base);
    }

    /// ★단조.★ 시각이 뒤로 가도 타임라인은 되감기지 않는다 -- 되감기면 애니메이션이
    /// 역행하고 WAAPI 의 `currentTime` 이 음수 델타를 본다.
    #[test]
    fn never_goes_backwards() {
        let previous = 5.0;
        assert_eq!(snap_animation_timeline(Some(previous), 4.9, P), previous);
        assert_eq!(snap_animation_timeline(Some(previous), 5.0, P), previous);
    }

    /// ★긴 정지 뒤에는 밀린 칸을 버린다.★ 하나씩 밟으면 애니메이션이 빨리 감기로 돈다 --
    /// 디스플레이가 하는 일은 지나간 칸을 버리는 것이다.
    #[test]
    fn a_long_stall_skips_the_missed_grid_points() {
        let previous = 1.0;
        let raw = previous + 2.0; // 2 초 정지 = 120 칸
        let got = snap_animation_timeline(Some(previous), raw, P);
        assert!(got > raw - P, "raw 바로 앞 격자점이어야 한다: {got}");
        assert!(got <= raw, "raw 를 넘으면 안 된다: {got}");
    }

    /// ★장기 드리프트가 없다.★ 뒤처진 채 고정되면 모든 애니메이션이 영구히 느려진다 --
    /// 저더보다 훨씬 나쁘다. 지터를 섞어 1,000 스텝을 돌려도 raw 를 따라가야 한다.
    #[test]
    fn does_not_drift_over_a_thousand_steps() {
        let mut previous = 0.0;
        let mut raw = 0.0;
        for index in 0..1000 {
            // 실측 범위의 지터: 주기 ±1ms 를 번갈아.
            raw += P + if index % 2 == 0 { 0.001 } else { -0.001 };
            previous = snap_animation_timeline(Some(previous), raw, P);
        }
        let lag = raw - previous;
        assert!(lag >= 0.0, "앞서면 안 된다: {lag}");
        assert!(lag < P, "한 주기 이상 뒤처지면 안 된다: {lag}");
    }
}

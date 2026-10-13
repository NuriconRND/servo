/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 애니메이션 타임라인 값의 간격을 고르게 만든다.

use malloc_size_of_derive::MallocSizeOf;

/// 평활기가 들고 다니는 상태. 두 값뿐이고 `Copy` 다.
///
/// `MallocSizeOf` 는 `#[dom_struct]` 안에 들어가기 위해 필요하다 -- `CrossProcessInstant`
/// 와 같은 이유다.
#[derive(Clone, Copy, Debug, MallocSizeOf, PartialEq)]
pub struct TimelineSmoothingState {
    /// 직전에 내보낸 값(초).
    previous: f64,
    /// 관측으로 추정한 틱 간격(초). ★공칭 주기가 아니라 실제로 들어오는 간격이다.★
    interval: f64,
}

/// 추정 간격을 관측값 쪽으로 끌어오는 비율. 시간상수는 1/ADAPT 틱 = 20 틱 ≒ 0.33 초다.
/// 0.02 에서 0.15 까지 결과가 같아서(비균일 0.00%) 민감한 값이 아니다.
const ADAPT: f64 = 0.05;

/// 이 배수를 넘는 간격은 틱이 아니라 ***정지***로 본다. 추정을 더럽히지 않고 다시 건다.
const STALL_INTERVALS: f64 = 4.0;

/// 출력이 실제 시각에서 이만큼(공칭 주기의 배수) 벗어나면 다시 건다. 추정이 틀어졌거나
/// 소스 주파수가 바뀐 경우다.
const REANCHOR_PERIODS: f64 = 4.0;

/// 애니메이션 타임라인 값의 ***간격***을 고르게 만든다.
///
/// ★왜 필요한가.★ 애니메이션 값은 `DocumentTimeline::update` 가 `CrossProcessInstant::now()`
/// 로 잡는데, 그것은 ***"스크립트가 그 함수에 도달한 순간"*** 이고 표출 격자와 무관하다.
/// 도달은 `TickAnimation` -> constellation -> script thread 큐를 거치므로 경로 지연이 매번
/// 다르고, 그 지터가 그대로 애니메이션 값이 된다. 실측(설계 문서 50): 틱이 60Hz 일 때 값의
/// 출처가 3.81% 이탈하고 화면값이 3.46% 이탈한다 -- 거의 1:1 이다.
///
/// ★왜 고정 격자가 아닌가.★ 처음에는 값을 `gfx_refresh_hz` 격자에 스냅하려 했다. 측정해
/// 보니 ***그쪽이 더 나쁘다.*** 격자가 첫 샘플에 고정된 채 자유 진동하므로 칸 경계를
/// 넘을 때마다 0배/2배 스텝이 나오고(비균일 2.6~8.9%), 틱 평균이 격자 주기와 다르면
/// (실측 16.80 대 16.67) 따라잡기 위해 2배 스텝이 ***주기적으로 반드시*** 나온다.
/// 6% 흔들림이 한 프레임 통째로 건너뛰는 것으로 바뀌는 셈이라 눈에 더 잘 띈다.
///
/// 그래서 격자에 맞추지 않고 ***간격을 추정해 그만큼씩 내보낸다.*** 추정은 관측된 간격의
/// 지수이동평균이라 소스 주파수를 따라간다 -- `tick_divisor` 가 4 라서 틱이 15Hz 로 와도
/// (간격 66.7ms) 그대로 따라간다. 고정 격자로는 불가능한 경우다.
///
/// `nominal_period` 는 ***씨앗과 허용오차***로만 쓴다(보통 `1/gfx_refresh_hz`). 추정이
/// 자리잡기 전의 초기값이고, 재앵커 판정의 기준 길이다. 0 이하거나 비유한이면 평활하지
/// 않고 `raw` 를 그대로 준다.
///
/// 반환값은 (쓸 타임라인 값, 다음 호출에 넘길 상태) 다.
pub fn smooth_animation_timeline(
    state: Option<TimelineSmoothingState>,
    raw: f64,
    nominal_period: f64,
) -> (f64, TimelineSmoothingState) {
    let seed = TimelineSmoothingState {
        previous: raw,
        interval: nominal_period,
    };
    if !raw.is_finite() || !nominal_period.is_finite() || nominal_period <= 0.0 {
        return (raw, seed);
    }
    let Some(state) = state else {
        // 첫 호출은 기준점이다. 격자를 여기서 시작한다.
        return (raw, seed);
    };
    let delta = raw - state.previous;
    if !(delta > 0.0) {
        // ★단조.★ 시각이 뒤로 갔거나 같은 순간에 두 번 불렸다. 되감지 않는다. `delta` 가
        // NaN 이어도 이 가지로 들어온다(비교가 거짓이므로).
        return (state.previous, state);
    }
    if delta > state.interval * STALL_INTERVALS {
        // ★긴 정지.★ 밀린 칸을 하나씩 밟으면 지나간 순간들을 위한 프레임이 쏟아져
        // 애니메이션이 빨리 감기로 돈다. 지금에 다시 걸되 추정은 건드리지 않는다 --
        // 정지 한 번으로 추정이 오염되면 복귀 뒤 수십 프레임이 흔들린다.
        return (
            raw,
            TimelineSmoothingState {
                previous: raw,
                interval: state.interval,
            },
        );
    }
    let interval = state.interval + (delta - state.interval) * ADAPT;
    let next = state.previous + interval;
    let tolerance = nominal_period * REANCHOR_PERIODS;
    if next > raw + tolerance || raw - next > tolerance {
        // 추정이 실제에서 너무 멀어졌다. 드리프트를 쌓지 않고 다시 건다.
        return (
            raw,
            TimelineSmoothingState {
                previous: raw,
                interval,
            },
        );
    }
    (
        next,
        TimelineSmoothingState {
            previous: next,
            interval,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: f64 = 1.0 / 60.0; // 16.667ms

    /// 상태를 물려가며 한 수열을 흘린다.
    fn feed(raws: &[f64], period: f64) -> Vec<f64> {
        let mut state = None;
        raws.iter()
            .map(|raw| {
                let (out, next) = smooth_animation_timeline(state, *raw, period);
                state = Some(next);
                out
            })
            .collect()
    }

    /// 첫 호출은 기준점이다. ★여기서 0 이나 epoch 를 쓰면 애니메이션이 과거에서 시작해
    /// 순간이동한다.★
    #[test]
    fn first_call_uses_the_raw_time() {
        let (out, state) = smooth_animation_timeline(None, 123.456, P);
        assert_eq!(out, 123.456);
        assert_eq!(state.previous, 123.456);
        assert_eq!(state.interval, P, "추정은 공칭 주기에서 시작한다");
    }

    /// 꺼져 있으면(period <= 0) 손대지 않는다. ★이 경로가 지금 동작이고, 되돌릴 수 있는
    /// 유일한 수단이다.★
    #[test]
    fn period_zero_passes_the_raw_time_through() {
        let state = TimelineSmoothingState {
            previous: 1.0,
            interval: P,
        };
        assert_eq!(smooth_animation_timeline(Some(state), 1.019, 0.0).0, 1.019);
        assert_eq!(smooth_animation_timeline(Some(state), 1.019, -1.0).0, 1.019);
    }

    /// ★화면에 보이는 것은 스텝의 균일함이다.★ 지터 섞인 입력 18,000 틱을 흘려 한 스텝이
    /// 평균에서 25% 넘게 벗어나는 비율을 센다. 등속 애니메이션에서 그런 프레임은 ***한
    /// 프레임을 통째로 건너뛰거나 두 배 뛴 것***으로 보인다 -- 6% 흔들림보다 훨씬 잘
    /// 보인다. 즉 이 비율이 저더 그 자체다.
    ///
    /// ★마지막 칸이 고정 격자로는 불가능한 경우다.★ 66.7ms 는 `tick_divisor=4` 의 15Hz
    /// 틱이고, 60Hz 격자는 그것을 따라갈 수 없다. 추정이 간격을 따라가므로 여기서도 고르다.
    #[test]
    fn keeps_steps_uniform_under_arrival_jitter() {
        for (mean_ms, jitter_ms) in [(16.67, 1.0), (16.80, 1.0), (16.80, 3.0), (66.70, 2.0)] {
            let ratio = non_unit_step_ratio(mean_ms, jitter_ms, P);
            assert!(
                ratio < 0.005,
                "평균 {mean_ms}ms +-{jitter_ms}ms 에서 비균일 스텝이 {:.2}% 다",
                ratio * 100.0
            );
        }
    }

    /// ★고정 격자였다면 이 테스트가 실패한다.★ 이 수치가 설계를 바꾼 근거다 -- 격자에
    /// 스냅하면 같은 입력에서 비균일이 2.6% 나왔고, 그 2.6% 는 각각 한 프레임을 통째로
    /// 건너뛰거나 두 배 뛴 것이다.
    #[test]
    fn no_single_step_is_a_dropped_or_doubled_frame() {
        let steps = jittered_steps(16.67, 1.0, P, 4_000);
        let expected = 0.01667;
        let worst = steps
            .iter()
            .map(|step| (step / expected - 1.0).abs())
            .fold(0.0_f64, f64::max);
        assert!(
            worst < 0.25,
            "어떤 스텝도 평균에서 25% 넘게 벗어나면 안 된다. 최악 {:.1}%",
            worst * 100.0
        );
    }

    /// ★단조.★ 시각이 뒤로 가도 타임라인은 되감기지 않는다 -- 되감기면 애니메이션이
    /// 역행하고 WAAPI 의 `currentTime` 이 음수 델타를 본다.
    #[test]
    fn never_goes_backwards() {
        let state = TimelineSmoothingState {
            previous: 5.0,
            interval: P,
        };
        assert_eq!(smooth_animation_timeline(Some(state), 4.9, P).0, 5.0);
        assert_eq!(smooth_animation_timeline(Some(state), 5.0, P).0, 5.0);
    }

    /// ★긴 정지 뒤에는 밀린 칸을 버린다.★ 하나씩 밟으면 애니메이션이 빨리 감기로 돈다 --
    /// 디스플레이가 하는 일은 지나간 칸을 버리는 것이다. 그리고 ★추정을 더럽히지 않아야★
    /// 한다. 정지 한 번으로 간격 추정이 2 초가 되면 복귀 뒤 애니메이션이 멈춘 듯 보인다.
    #[test]
    fn a_long_stall_reanchors_without_poisoning_the_estimate() {
        let state = TimelineSmoothingState {
            previous: 1.0,
            interval: P,
        };
        let raw = 1.0 + 2.0; // 2 초 정지
        let (out, next) = smooth_animation_timeline(Some(state), raw, P);
        assert_eq!(out, raw, "지금에 다시 걸어야 한다");
        assert_eq!(next.interval, P, "추정은 그대로여야 한다");
    }

    /// ★정지에서 복귀한 직후도 고와야 한다.★ 재앵커가 한 번 튀는 것은 어쩔 수 없지만,
    /// 그 다음 프레임부터는 평소 간격이어야 한다.
    #[test]
    fn recovers_smoothly_after_a_stall() {
        let mut raws: Vec<f64> = (1..=300).map(|i| i as f64 * 0.0168).collect();
        let last = *raws.last().unwrap();
        raws.push(last + 2.0); // 정지
        for i in 1..=20 {
            raws.push(last + 2.0 + i as f64 * 0.0168);
        }
        let out = feed(&raws, P);
        let worst = out[out.len() - 20..]
            .windows(2)
            .map(|w| ((w[1] - w[0]) / 0.0168 - 1.0).abs())
            .fold(0.0_f64, f64::max);
        assert!(worst < 0.25, "복귀 뒤 최악 편차 {:.1}%", worst * 100.0);
    }

    /// ★장기 드리프트가 없다.★ 뒤처진 채 고정되면 모든 애니메이션이 영구히 느려진다 --
    /// 저더보다 훨씬 나쁘다. 지터를 섞어 1,000 스텝을 돌려도 raw 를 따라가야 한다.
    #[test]
    fn does_not_drift_over_a_thousand_steps() {
        let mut raw = 0.0;
        let raws: Vec<f64> = (0..1000)
            .map(|index| {
                raw += P + if index % 2 == 0 { 0.001 } else { -0.001 };
                raw
            })
            .collect();
        let out = feed(&raws, P);
        let lag = raw - out.last().unwrap();
        assert!(
            lag.abs() < P * 4.0,
            "실제 시각에서 네 주기 넘게 벌어지면 안 된다: {lag}"
        );
    }

    /// ★소스 주파수가 바뀌어도 따라간다.★ `tick_divisor` 를 실행 중에 바꾸거나 탭이
    /// 백그라운드로 내려가면 틱 간격이 달라진다. 추정이 못 따라가면 재앵커가 매 프레임
    /// 일어나 평활이 꺼진 것과 같아진다.
    #[test]
    fn follows_a_change_in_tick_rate() {
        let mut raw = 0.0;
        let mut raws = Vec::new();
        for _ in 0..200 {
            raw += 0.0167;
            raws.push(raw);
        }
        for _ in 0..200 {
            raw += 0.0334; // 절반으로 느려졌다
            raws.push(raw);
        }
        let out = feed(&raws, P);
        let worst = out[out.len() - 50..]
            .windows(2)
            .map(|w| ((w[1] - w[0]) / 0.0334 - 1.0).abs())
            .fold(0.0_f64, f64::max);
        assert!(
            worst < 0.25,
            "새 주파수에 안착해야 한다. 최악 {:.1}%",
            worst * 100.0
        );
    }

    /// ★비유한 입력에 멈추거나 패닉하지 않는다.★ 지금 호출자는 유한한 값만 주지만, 이
    /// 함수는 `pub` 이고 공유 크레이트에 있다. 다음 호출자가 그러리라는 보장은 없다.
    #[test]
    fn non_finite_inputs_are_inert() {
        let state = TimelineSmoothingState {
            previous: 1.0,
            interval: P,
        };
        for raw in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let (out, _) = smooth_animation_timeline(Some(state), raw, P);
            let _ = out; // 멈추지 않고 돌아온 것 자체가 이 테스트의 내용이다.
        }
        let (out, _) = smooth_animation_timeline(Some(state), 2.0, f64::NAN);
        assert_eq!(out, 2.0, "비유한 주기는 평활하지 않는다");
    }

    /// 재현 가능한 의사난수. 입력 수열이 매번 같아야 회귀를 잡는다.
    fn jittered_steps(mean_ms: f64, jitter_ms: f64, period: f64, count: usize) -> Vec<f64> {
        let mut seed: u64 = 0x5DEE_CE66_D1EB_1A5F;
        let mut raw = 0.0;
        let raws: Vec<f64> = (0..count)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let unit = ((seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0;
                raw += (mean_ms + unit * jitter_ms) / 1000.0;
                raw
            })
            .collect();
        let out = feed(&raws, period);
        // 추정이 자리잡는 앞 100 틱은 뺀다 -- 과도구간이지 정상 동작이 아니다.
        out[100..].windows(2).map(|w| w[1] - w[0]).collect()
    }

    fn non_unit_step_ratio(mean_ms: f64, jitter_ms: f64, period: f64) -> f64 {
        let steps = jittered_steps(mean_ms, jitter_ms, period, 18_000);
        let expected = mean_ms / 1000.0;
        let bad = steps
            .iter()
            .filter(|step| (*step / expected - 1.0).abs() > 0.25)
            .count();
        bad as f64 / steps.len() as f64
    }
}

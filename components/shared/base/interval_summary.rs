/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 연속한 표본 사이 간격의 요약.
//!
//! ★왜 공유 모듈인가.★ 같은 수열이 두 곳에서 측정된다 -- 페인트가 틱에 실어 보낸 프레임
//! 시각(`FRAMETIME`, `components/paint/frame_time.rs`)과, 스크립트가 그것을 해석해 페이지의
//! `requestAnimationFrame` 콜백에 넘긴 값(`RAFCLOCK`,
//! `components/script/raf_clock.rs`). 두 줄을 나란히 놓고 "어디서 고르지 않게 되었는가" 를
//! 읽는 것이 이 계측의 전부인데, 밴드 경계나 백분위 정의가 둘 사이에서 어긋나면 그 비교가
//! 조용히 거짓말을 한다. 그래서 한 함수로 묶는다.

/// 한 줄에 한 창(60 개)을 담는다. 프레임마다 한 줄이면 300 초에 72,000 줄이 되고, 이
/// 로그는 이미 200MB 를 넘긴 적이 있다. 소수 한 자리면 충분하다.
pub fn format_intervals(samples: &[f64]) -> String {
    let mut out = String::with_capacity(samples.len() * 6);
    for (index, value) in samples.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!("{value:.1}"));
    }
    out
}

/// 정렬 뒤 백분위. 비어 있으면 `None`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Percentiles {
    pub p05: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

/// ★제자리 정렬한다.★ 원시 순서가 필요하면 부르기 전에 찍어 두어야 한다.
pub fn percentiles(samples: &mut [f64]) -> Option<Percentiles> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = |p: f64| -> f64 { samples[(((samples.len() - 1) as f64) * p).round() as usize] };
    Some(Percentiles {
        p05: at(0.05),
        p50: at(0.50),
        p95: at(0.95),
        max: at(1.00),
    })
}

/// 간격 수열 하나의 요약.
#[derive(Clone, Debug, PartialEq)]
pub struct Intervals {
    pub n: usize,
    /// 한 주기의 절반에 못 미치는 간격. ★0 이 아니면 그 자체가 결함이다★ -- 연속한 두
    /// 표본이 사실상 같은 시각이라는 뜻이고, 그 시각으로 위치를 계산하는 페이지는 같은
    /// 위치를 두 번 그린다(화면은 그 프레임만큼 멈춘다).
    pub dup: usize,
    /// 한 주기(0.5~1.5). 깨끗한 실행은 전부 여기다.
    pub one: usize,
    /// 두 주기(1.5~2.5). 한 프레임을 건너뛰었다.
    pub skip1: usize,
    /// 2.5 주기 이상.
    pub skip_n: usize,
    pub p05: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
    /// 정렬 전 원시 수열.
    pub sequence: String,
}

/// 간격을 주기로 나누어 네 칸에 넣고 백분위를 뽑는다.
///
/// ★`ANIMREADY` 와 같은 경계를 쓴다★(0.5 / 1.5 / 2.5 주기). 세 지표를 나란히 읽는 것이
/// 목적이므로 경계가 같아야 한다.
///
/// `samples` 는 제자리 정렬되지만 `sequence` 는 정렬 전에 만들어 둔다 -- `ANIMSTEPSEQ` 가
/// 이 함정을 한 번 밟았고, 그 원시 수열이 "아홉 스텝 가다가 124ms 멈춤" 을 보여준 유일한
/// 줄이었다.
pub fn summarize_intervals(samples: &mut [f64], period_ms: f64) -> Option<Intervals> {
    if samples.is_empty() || !(period_ms > 0.0) {
        return None;
    }
    let n = samples.len();
    let sequence = format_intervals(samples);
    let band = |low: f64, high: f64| -> usize {
        samples
            .iter()
            .filter(|&&v| v >= low * period_ms && v < high * period_ms)
            .count()
    };
    let dup = band(0.0, 0.5);
    let one = band(0.5, 1.5);
    let skip1 = band(1.5, 2.5);
    let skip_n = samples.iter().filter(|&&v| v >= 2.5 * period_ms).count();
    let p = percentiles(samples)?;
    Some(Intervals {
        n,
        dup,
        one,
        skip1,
        skip_n,
        p05: p.p05,
        p50: p.p50,
        p95: p.p95,
        max: p.max,
        sequence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD_MS: f64 = 1000.0 / 60.0;

    /// ★이 계측이 존재하는 이유.★ 60 프레임이 나갔는데 그중 둘이 같은 시각을 실었으면
    /// 페이지는 같은 위치를 두 번 계산한다 -- 프레임 수만 보는 지표는 이것을 놓친다.
    #[test]
    fn a_repeated_value_is_counted_as_dup() {
        let mut dt = vec![PERIOD_MS, 0.0, 2.0 * PERIOD_MS];
        let summary = summarize_intervals(&mut dt, PERIOD_MS).unwrap();
        assert_eq!(
            (summary.dup, summary.one, summary.skip1, summary.skip_n),
            (1, 1, 1, 0)
        );
    }

    /// 깨끗한 실행의 모양을 고정해 둔다 -- 그래야 이상한 실행을 알아볼 수 있다.
    #[test]
    fn an_even_sequence_is_all_one() {
        let mut dt = vec![PERIOD_MS; 60];
        let summary = summarize_intervals(&mut dt, PERIOD_MS).unwrap();
        assert_eq!(
            (summary.dup, summary.one, summary.skip1, summary.skip_n),
            (0, 60, 0, 0)
        );
        assert!((summary.p50 - PERIOD_MS).abs() < 1e-9);
    }

    /// 경계 2.5 주기는 `skipN` 쪽이다.
    #[test]
    fn the_boundary_at_two_and_a_half_periods_is_skip_n() {
        let mut dt = vec![2.49 * PERIOD_MS, 2.5 * PERIOD_MS];
        let summary = summarize_intervals(&mut dt, PERIOD_MS).unwrap();
        assert_eq!((summary.skip1, summary.skip_n), (1, 1));
    }

    /// 백분위는 정렬 뒤 값에서 뽑는다. 입력을 일부러 뒤섞어 둔다.
    #[test]
    fn percentiles_come_from_the_sorted_values() {
        let mut dt = vec![50.0, 10.0, 30.0, 20.0, 40.0];
        let summary = summarize_intervals(&mut dt, PERIOD_MS).unwrap();
        assert_eq!((summary.p50, summary.max), (30.0, 50.0));
    }

    /// ★원시 수열은 정렬 전의 것이어야 한다.★ 나중에 만들면 조용히 정렬된 값이 찍힌다.
    #[test]
    fn the_raw_sequence_keeps_its_order() {
        let mut dt = vec![30.0, 10.0, 20.0];
        let summary = summarize_intervals(&mut dt, PERIOD_MS).unwrap();
        assert_eq!(summary.sequence, "30.0,10.0,20.0");
    }

    #[test]
    fn an_empty_window_is_not_summarized() {
        assert_eq!(summarize_intervals(&mut [], PERIOD_MS), None);
        assert_eq!(percentiles(&mut []), None);
    }

    /// 주기가 0 이면 밴드가 전부 무의미해진다 -- 나누지 말고 포기한다.
    #[test]
    fn a_zero_period_is_not_summarized() {
        assert_eq!(summarize_intervals(&mut [16.7], 0.0), None);
    }

    /// NaN 이 섞여도 정렬이 패닉하지 않는다. 시각 차이에서 NaN 이 나올 일은 없지만,
    /// 조사용 계측이 본 코드를 죽이는 일은 더더욱 없어야 한다.
    #[test]
    fn a_nan_does_not_panic_the_sort() {
        let mut dt = vec![16.7, f64::NAN, 33.4];
        assert!(summarize_intervals(&mut dt, PERIOD_MS).is_some());
    }
}

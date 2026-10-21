/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! RAFCLOCK -- 페이지가 실제로 받은 `requestAnimationFrame` 타임스탬프의 수열.
//!
//! ★`FRAMETIME` 의 반대편이다.★ 페인트는 틱마다 프레임 시각을 실어 보내고
//! (`components/paint/frame_time.rs`), 스크립트는 그것을 받아
//! [`Document::resolve_frame_time`] 에서 해석한 뒤 rAF 콜백에 넘긴다. 두 수열이 같다는
//! 보장이 없다 -- 사이에 둘이 끼어 있다.
//!
//! 1. ***틱 소비***. `pending_frame_time` 은 `Cell` 하나라, 스크립트가 한 회차를 돌기 전에
//!    틱이 둘 들어오면 앞의 것은 덮여 사라진다.
//! 2. ***단조 증가 클램프***. `resolve_frame_time` 은 후보가 직전 값보다 과거면 직전 값을
//!    그대로 돌려준다. 그러면 페이지는 ***지난 프레임과 똑같은 타임스탬프***를 받고 같은
//!    위치를 두 번 계산한다. 틱을 못 쓴 회차가 `CrossProcessInstant::now()` 로 떨어지면
//!    시계가 한 번 앞으로 튀고, 뒤이어 오는 (페이싱된, 따라서 더 이른) 진짜 틱들이
//!    줄줄이 클램프된다 -- ★프레임은 60 개가 나가는데 화면이 멈추는 모양★ 이 그것이다.
//!
//! 그래서 `clamped` 와 `fallback` 을 따로 센다. 둘 중 하나라도 0 이 아니면 범인은 여기다.
//!
//! 같은 밴드·백분위를 쓰도록 요약은 `servo_base::interval_summary` 가 맡는다 -- 두 줄을
//! 나란히 읽는 것이 이 계측의 전부이므로 정의가 어긋나면 비교가 거짓말을 한다.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use servo_base::cross_process_instant::CrossProcessInstant;
use servo_base::id::PipelineId;
use servo_base::interval_summary::summarize_intervals;

/// `SERVO_FRAME_TIME_PROF` 게이트. `FRAMETIME` 과 ***같은 스위치***다 -- 한쪽만 켜고 보면
/// 비교할 짝이 없어서 아무것도 판정하지 못한다.
static FRAME_TIME_PROF: LazyLock<bool> = LazyLock::new(|| {
    servo_config::debug_env::string(&servo_config::debug_env::FRAME_TIME_PROF)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
});

/// `gfx_refresh_hz` 의 주기. 밴드를 나누는 데만 쓴다.
///
/// ★`crate::refresh_driver::paint_timer_period` 와 같은 값이어야 한다★ -- 그쪽은 paint
/// 크레이트의 `pub(crate)` 라 여기서 부를 수 없어 같은 pref 를 같은 규칙으로 읽는다.
/// 기본값과 유효 범위를 바꾸려면 두 곳을 함께 고쳐야 한다.
static PERIOD_MS: LazyLock<f64> = LazyLock::new(|| {
    let raw_hz = servo_config::pref!(gfx_refresh_hz);
    let hz = if (1..=1000).contains(&raw_hz) {
        raw_hz
    } else {
        120
    };
    1000.0 / hz as f64
});

#[derive(Default)]
struct ClockWindow {
    start: Option<Instant>,
    previous: Option<CrossProcessInstant>,
    dt_ms: Vec<f64>,
    /// 셸이 보낸 틱 시각 대신 `now()` 로 떨어진 회차.
    fallback: u32,
    /// 단조 증가 클램프가 걸린 회차 -- 페이지가 직전과 같은 시각을 받았다.
    clamped: u32,
}

static CLOCKS: LazyLock<Mutex<HashMap<PipelineId, ClockWindow>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 한 회차가 페이지에 넘길 시각을 정한 직후에 부른다.
///
/// `resolved` 는 rAF 콜백이 실제로 받는 값이고, `from_frame_time` 은 그것이 셸의 틱에서
/// 왔는지(아니면 `now()` 로 떨어졌는지), `clamped` 는 단조 증가 하한에 걸려 직전 값으로
/// 되돌려졌는지다.
pub(crate) fn note_resolved(
    pipeline_id: PipelineId,
    resolved: CrossProcessInstant,
    from_frame_time: bool,
    clamped: bool,
) {
    if !*FRAME_TIME_PROF {
        return;
    }
    let Ok(mut clocks) = CLOCKS.lock() else {
        return;
    };
    let window = clocks.entry(pipeline_id).or_default();
    let at = Instant::now();
    let start = *window.start.get_or_insert(at);
    if !from_frame_time {
        window.fallback += 1;
    }
    if clamped {
        window.clamped += 1;
    }
    if let Some(previous) = window.previous {
        window
            .dt_ms
            .push((resolved - previous).as_seconds_f64() * 1000.0);
    }
    window.previous = Some(resolved);

    if at.duration_since(start) < Duration::from_secs(1) {
        return;
    }
    window.start = Some(at);
    let mut dt = std::mem::take(&mut window.dt_ms);
    let fallback = std::mem::replace(&mut window.fallback, 0);
    let clamped = std::mem::replace(&mut window.clamped, 0);
    drop(clocks);

    let Some(intervals) = summarize_intervals(&mut dt, *PERIOD_MS) else {
        return;
    };
    log::warn!(
        "RAFCLOCK pipeline={pipeline_id:?} n={} per_interval dup={} one={} skip1={} skipN={} \
         dt_ms p05={:.2} p50={:.2} p95={:.2} max={:.2} fallback={fallback} clamped={clamped}",
        intervals.n,
        intervals.dup,
        intervals.one,
        intervals.skip1,
        intervals.skip_n,
        intervals.p05,
        intervals.p50,
        intervals.p95,
        intervals.max,
    );
    if std::env::var("SERVO_LOG_ANIM_STEP_SEQ").is_ok_and(|value| value != "0") {
        log::warn!(
            "RAFCLOCKSEQ pipeline={pipeline_id:?} n={} dt_ms=[{}]",
            intervals.n,
            intervals.sequence,
        );
    }
}

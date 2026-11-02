/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! WR 프레임이 ★누가 발행했는지★ 세는 프로브.
//!
//! ## 무엇을 재는가
//!
//! 규칙 1 은 "프레임 생산자는 단일하다" 다. 이 프로브는 그것을 발행 지점에서 직접 센다 --
//! [`Painter::generate_frame_with_diagnostic_id`] 가 트랜잭션 `generate_frame` 의 ***유일한
//! 깔때기***이고, 벽 틱 경로만 `wall_logical_frame_id` 를 들고 온다(다른 자리는 전부 `None`).
//! 그래서 그 하나로 ★틱이 낸 것★과 ★그 밖이 낸 것★이 갈린다.
//!
//! `other` 쪽은 이유 비트를 OR 로 모아 둔다 -- 어느 자리가 냈는지 이름이 남는다.
//!
//! ## 왜 이것을 재는가 (2026-11-02)
//!
//! 최소 WebGL2 삼각형 한 장에서 HUD 가 ★65fps★ 를 찍었다(저더 동반). 다른 콘텐츠는 같은
//! 조건에서 60~61 이었다. 그 5 의 출처를 셸에서 찾으려 했고 ***틀렸다***:
//!
//! - `PASSSOURCE`(2026-11-02, log_ani_shift_01/14) 가 ★틱 60.0/s, 패스 60/s, OS redraw 는
//!   런 전체에 4 건★ 임을 보였다. WebGL 런과 대조군이 똑같았다 -- 셸의 두 번째 입구
//!   (`WindowEvent::RedrawRequested`)는 범인이 아니다.
//! - 그래서 "HUD fps = 패스 횟수" 라는 전제가 틀렸다. HUD 는 `render_impl` 진입을 세는데,
//!   그 함수의 호출처는 `pub fn render` ***하나가 아니라 다섯***이고 넷이
//!   `Renderer::update()` 안에 있다. 그리고 `painter.rs` 는 매 패스마다 `renderer.render()`
//!   직전에 `renderer.update()` 를 부른다.
//! - `update()` 는 ***그려지지 않은 채 남은*** 프레임이 텍스처 캐시 작업을 품고 있으면
//!   (`must_be_drawn() = has_texture_cache_tasks && !has_been_rendered`) 그것을 그 자리에서
//!   렌더해 작업을 잃지 않게 한다.
//!
//! 즉 HUD 가 60 을 넘으면 ★패스당 WR 프레임이 둘 이상 발행된 것★이다. 이 프로브가 그
//! 발행을 센다.
//!
//! ## 성능 규칙
//!
//! ★타이밍 경로에 I/O 금지.★ 쌓는 쪽은 원자 둘(계수기 하나와 이유 비트 OR)이고, 출력은
//! 전용 리포터 스레드가 [`REPORT_EVERY`] 마다 한다. 꺼져 있으면 `AtomicBool` 로드 하나다.
//! 쌓기 비용의 상한은 `tests::noting_a_frame_is_cheap_enough` 가 고정한다.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// 리포터가 누적을 찍는 간격.
const REPORT_EVERY: Duration = Duration::from_secs(10);

static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

/// 벽 틱이 발행한 프레임. ★이것만 있어야 한다.★
static TICK_FRAMES: AtomicU64 = AtomicU64::new(0);
/// 그 밖이 발행한 프레임.
static OTHER_FRAMES: AtomicU64 = AtomicU64::new(0);
/// `other` 들의 `RenderReasons` 비트를 OR 로 모은 것 -- 어느 자리가 냈는지 남는다.
static OTHER_REASON_BITS: AtomicU64 = AtomicU64::new(0);

/// 프로브가 켜져 있는가.
pub(crate) fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 프로브를 켜고 리포터 스레드를 띄운다. ★여러 페인터가 불러도 한 번만 띄운다.★
pub(crate) fn start() {
    if STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    ENABLED.store(true, Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name(String::from("FrameSource"))
        .spawn(|| {
            let started = std::time::Instant::now();
            loop {
                std::thread::sleep(REPORT_EVERY);
                report(started.elapsed());
            }
        });
    if let Err(error) = spawned {
        log::warn!("frame-source: 리포터 스레드를 띄우지 못했다: {error}; 요약이 나오지 않는다");
    }
}

/// 프레임 발행 하나를 센다. ★원자 둘이 전부다.★
pub(crate) fn note_frame(from_wall_tick: bool, reason_bits: u64) {
    if !is_enabled() {
        return;
    }
    if from_wall_tick {
        TICK_FRAMES.fetch_add(1, Ordering::Relaxed);
    } else {
        OTHER_FRAMES.fetch_add(1, Ordering::Relaxed);
        OTHER_REASON_BITS.fetch_or(reason_bits, Ordering::Relaxed);
    }
}

/// ★`warn!` 로 찍는 이유: 벽 런이 `RUST_LOG=warn` 이다.★ `info!` 로 두면 정작 볼 수 없다 --
/// `paint_animation_mesh_probe` 가 그 함정에 걸려 첫 실기 런에서 한 줄도 남지 않았다.
fn report(elapsed: Duration) {
    let tick = TICK_FRAMES.load(Ordering::Relaxed);
    let other = OTHER_FRAMES.load(Ordering::Relaxed);
    let bits = OTHER_REASON_BITS.load(Ordering::Relaxed);
    let secs = elapsed.as_secs_f64().max(1e-9);
    log::warn!(
        "FRAMESRC t={secs:.0}s tick={tick} ({:.1}/s) other={other} ({:.1}/s) \
         other_reasons=0x{bits:x}",
        tick as f64 / secs,
        other as f64 / secs,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★두 시험이 같은 전역 `ENABLED` 를 건드린다★ -- cargo test 는 병렬이므로 직렬화한다.
    static SERIALIZE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// ★쌓기 비용을 수치로 고정한다★ -- 이 프로젝트의 계측 규칙이 요구하는 것이다.
    /// 원자 둘이므로 한 자리 ns 여야 한다. 상한은 느린 CI 를 감안해 200ns 로 둔다.
    #[test]
    fn noting_a_frame_is_cheap_enough() {
        let _guard = SERIALIZE.lock().unwrap();
        ENABLED.store(true, Ordering::Relaxed);
        const CALLS: u32 = 200_000;
        let started = std::time::Instant::now();
        for index in 0..CALLS {
            note_frame(index % 2 == 0, u64::from(index));
        }
        let per_call = started.elapsed().as_secs_f64() / f64::from(CALLS) * 1e9;
        ENABLED.store(false, Ordering::Relaxed);
        assert!(
            per_call < 200.0,
            "쌓기 비용 {per_call:.1}ns/call -- 상한 200ns 를 넘었다"
        );
        println!("note_frame: {per_call:.1} ns/call");
    }

    /// 꺼져 있으면 아무것도 쌓이지 않는다. ★기본 런이 전과 같다는 것이 이 시험이다.★
    #[test]
    fn a_disabled_probe_counts_nothing() {
        let _guard = SERIALIZE.lock().unwrap();
        ENABLED.store(false, Ordering::Relaxed);
        let before = TICK_FRAMES.load(Ordering::Relaxed);
        note_frame(true, 1);
        assert_eq!(TICK_FRAMES.load(Ordering::Relaxed), before);
    }
}

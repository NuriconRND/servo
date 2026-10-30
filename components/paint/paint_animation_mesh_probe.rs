/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 연달아 오는 paint-side 예측이 ★서로 맞물리는지★ 직접 재는 프로브.
//!
//! ## 무엇을 재는가
//!
//! 설계(`docs/superpowers/specs/2026-10-15-tick-animation-frame-time-design.md`)와
//! `animation_zero` 의 doc 이 불변식 하나를 주장한다 — ***"연달아 오는 예측들이 서로 정확히
//! 맞물린다"***. 즉 디스플레이 리스트가 예측을 통째로 교체할 때
//! (`PipelineAnimations::install_paint_animations`), ★교체 직전의 예측이 그 순간 내던 값과
//! 새 세그먼트가 그 순간 내는 값이 같아야 한다.★ 다르면 그 차이가 ***그대로 화면의 튐***이고,
//! 튐의 빈도는 디스플레이 리스트 속도다.
//!
//! 이 프로브는 그 차이를 설치마다 재서 분포로 쌓는다. ★가설을 세우는 대신 불변식을
//! 측정한다★ -- 2026-10-30 까지 이 증상에 대해 세운 기구 다섯이 모두 틀렸고, 그중 하나는
//! ***이미 있던 시험***(`even_build_times_give_even_zeros_despite_jittery_transit`)이
//! 반증했다.
//!
//! ## 성능 규칙
//!
//! ★타이밍 경로에 I/O 금지.★ 쌓는 쪽이 하는 일은 원자 연산 둘(히스토그램 버킷 하나와 최대값
//! 갱신)뿐이고, 출력은 전용 리포터 스레드가 [`REPORT_EVERY`] 마다 한다. 강제 종료로 끝나는
//! 런에서도 요약이 남도록 ***런 시작부터의 누적***을 찍는다(종료 훅이 없다).
//!
//! ★꺼져 있으면 호출 경로에 남는 것은 `AtomicBool::load(Relaxed)` 하나다.★ 설치 경로의
//! 측정 자체(이전/새 예측을 한 번씩 더 샘플링하는 것)도 그 분기 안에 있으므로, 꺼진 런은
//! 측정 비용이 0 이다. 쌓기 비용의 수치는 `tests::noting_an_error_is_cheap_enough` 가
//! 상한으로 고정한다.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// 히스토그램 칸 수. 칸 0 은 "1/1024 이하"(= 사실상 맞물림), 그 위는 2 배씩이다.
///
/// ★해상도를 이 자리에 맞춘 이유★: 설계는 오차가 0 이라고 주장하고, 눈에 보이는 튐은 1px
/// 급이다. 1/1024 에서 시작해 2 배씩 올리면 그 둘 사이가 10 칸으로 갈린다 -- 주장과 증상
/// 사이의 어디에 있는지가 한 눈에 보인다.
const BUCKETS: usize = 12;

/// 칸 0 의 상한. 이 아래는 맞물린 것으로 센다.
const MESHED_BELOW: f32 = 1.0 / 1024.0;

/// 리포터가 누적을 찍는 간격.
const REPORT_EVERY: Duration = Duration::from_secs(10);

static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

/// 예측 교체가 일어난 횟수.
static INSTALLS: AtomicU64 = AtomicU64::new(0);
/// 교체 때 ***직전 예측에도 있던*** 키의 수 -- 맞물림을 따질 수 있는 모집단이다.
static CARRIED: AtomicU64 = AtomicU64::new(0);
/// 교체 때 새로 나타난 키의 수. 이들은 비교 대상이 없다(맞물림을 따질 수 없다).
static FRESH: AtomicU64 = AtomicU64::new(0);
/// 맞물림 오차의 분포.
static HIST: [AtomicU64; BUCKETS] = [const { AtomicU64::new(0) }; BUCKETS];
/// 관측된 최대 오차, 1e-9 단위의 정수. ★f32 를 원자적으로 최대 갱신할 수 없어 정수로 둔다.★
static WORST_NANOS: AtomicU64 = AtomicU64::new(0);

/// 프로브가 켜져 있는가. ★설치 경로가 측정 전체를 이 값으로 건너뛴다.★
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
        .name(String::from("PaintAnimMesh"))
        .spawn(|| {
            let started = std::time::Instant::now();
            loop {
                std::thread::sleep(REPORT_EVERY);
                report(started.elapsed());
            }
        });
    if let Err(error) = spawned {
        log::warn!("paint-anim-mesh: 리포터 스레드를 띄우지 못했다: {error}; 요약이 나오지 않는다");
    }
}

/// 한 번의 예측 교체를 센다. `carried` 는 비교할 수 있었던 키, `fresh` 는 새로 나타난 키.
pub(crate) fn note_install(carried: u64, fresh: u64) {
    INSTALLS.fetch_add(1, Ordering::Relaxed);
    if carried > 0 {
        CARRIED.fetch_add(carried, Ordering::Relaxed);
    }
    if fresh > 0 {
        FRESH.fetch_add(fresh, Ordering::Relaxed);
    }
}

/// 맞물림 오차 하나를 쌓는다. ★이 함수가 쌓기 비용의 전부다 -- 원자 연산 둘.★
pub(crate) fn note_mesh_error(error: f32) {
    HIST[bucket_of(error)].fetch_add(1, Ordering::Relaxed);
    let nanos = (error.max(0.0) as f64 * 1e9) as u64;
    WORST_NANOS.fetch_max(nanos, Ordering::Relaxed);
}

/// 오차를 히스토그램 칸으로. 칸 0 은 `MESHED_BELOW` 이하, 그 위는 2 배씩이다.
fn bucket_of(error: f32) -> usize {
    if !(error > MESHED_BELOW) {
        // NaN 도 여기로 온다 -- 비교가 거짓이므로. NaN 을 "맞물림" 으로 세는 것은 틀리지만,
        // 아래 `report` 가 최대값을 함께 찍으므로 조용히 묻히지는 않는다.
        return 0;
    }
    let steps = (error / MESHED_BELOW).log2().floor();
    ((steps as usize) + 1).min(BUCKETS - 1)
}

fn report(elapsed: Duration) {
    let installs = INSTALLS.load(Ordering::Relaxed);
    let carried = CARRIED.load(Ordering::Relaxed);
    let fresh = FRESH.load(Ordering::Relaxed);
    let worst = WORST_NANOS.load(Ordering::Relaxed) as f64 / 1e9;
    let counts: Vec<u64> = HIST.iter().map(|b| b.load(Ordering::Relaxed)).collect();
    let total: u64 = counts.iter().sum();
    if total == 0 {
        log::info!(
            "MESHPROBE t={:.0}s installs={installs} carried={carried} fresh={fresh} \
             samples=0 (맞물림을 따질 키가 없었다)",
            elapsed.as_secs_f64(),
        );
        return;
    }
    let meshed_pct = counts[0] as f64 * 100.0 / total as f64;
    // 칸 경계를 값으로 같이 찍는다 -- 숫자 열만 보고 해석할 수 있어야 한다.
    let mut spread = String::new();
    for (index, count) in counts.iter().enumerate() {
        if *count == 0 {
            continue;
        }
        let edge = if index == 0 {
            MESHED_BELOW
        } else {
            MESHED_BELOW * (1u32 << (index - 1).min(31)) as f32 * 2.0
        };
        spread.push_str(&format!(
            " <={:.4}:{:.1}%",
            edge,
            *count as f64 * 100.0 / total as f64
        ));
    }
    log::info!(
        "MESHPROBE t={:.0}s installs={installs} carried={carried} fresh={fresh} \
         samples={total} meshed(<={:.4})={meshed_pct:.2}% worst={worst:.6}{spread}",
        elapsed.as_secs_f64(),
        MESHED_BELOW,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 칸 경계가 의도대로다. ★칸 0 이 "맞물림" 이고 그 위가 2 배씩★이라는 것이 보고를 읽는
    /// 전제이므로 고정한다.
    #[test]
    fn the_buckets_start_at_the_meshed_threshold_and_double() {
        assert_eq!(bucket_of(0.0), 0);
        assert_eq!(bucket_of(MESHED_BELOW), 0);
        assert_eq!(bucket_of(MESHED_BELOW * 1.5), 1);
        assert_eq!(bucket_of(MESHED_BELOW * 2.5), 2);
        assert_eq!(bucket_of(MESHED_BELOW * 4.5), 3);
        // 1px 급 -- 눈에 보이는 튐은 이 근처다.
        assert!(bucket_of(1.0) > 8, "{}", bucket_of(1.0));
    }

    /// 아무리 큰 값도, 음수도, NaN 도 칸 안에 떨어진다(색인 패닉 금지).
    #[test]
    fn no_input_can_fall_outside_the_histogram() {
        for error in [
            0.0,
            -1.0,
            f32::MIN,
            f32::MAX,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            1e30,
        ] {
            let bucket = bucket_of(error);
            assert!(bucket < BUCKETS, "{error} -> {bucket}");
        }
    }

    /// ★쌓기 비용을 수치로 고정한다★ -- 이 프로젝트의 계측 규칙이 요구하는 것이다.
    /// 원자 연산 둘이므로 한 자리 ns 여야 한다. 상한은 느린 CI 를 감안해 200ns 로 둔다.
    #[test]
    fn noting_an_error_is_cheap_enough() {
        const CALLS: u32 = 200_000;
        let started = std::time::Instant::now();
        for index in 0..CALLS {
            note_mesh_error(index as f32 * 1e-6);
        }
        let per_call = started.elapsed().as_secs_f64() / f64::from(CALLS) * 1e9;
        assert!(
            per_call < 200.0,
            "쌓기 비용 {per_call:.1}ns/call -- 상한 200ns 를 넘었다"
        );
        println!("note_mesh_error: {per_call:.1} ns/call");
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! ★전용 렌더 타깃을 요청보다 큰 풀 텍스처로 재사용한다★ -- 규칙과 그 스위치.
//!
//! ## 왜 (2026-11-12 추가)
//!
//! upstream 의 `ResourceCache::get_or_create_render_target_from_pool` 은 크기가 ***정확히
//! 같은*** 타깃만 재사용한다. 2048 을 넘는 태스크(전용 타깃)는 256 올림 크기를 받으므로,
//! 크기가 프레임마다 조금씩 바뀌는 surface 는 256 경계를 넘을 때마다 새 텍스처를 만들고,
//! 풀이 64 MB 를 넘으면 60 프레임 안 쓴 것을 버린다. 그것이 반복된다.
//!
//! 그런 surface 를 만드는 것이 ***회전 + opacity*** 다. opacity 필터 스태킹 컨텍스트가
//! 회전한 좌표계 안에 있으면 surface 를 로컬 공간에서 그리고, 크기는 "prim ∩ 타일을 회전
//! 공간으로 역사상한 bbox" 라 각도마다 다르다. 영역이 타일보다 크면 그 교집합이 실제로
//! 변한다. 회전만이면 중간 surface 가 없고, opacity 만이면 크기가 일정하다.
//!
//! 실측(2026-11-12, 개발 장비 3840x1080 타일 하나, `wall_rotate_opacity_probe.html`
//! `w=1.6&h=3.2` = 6144x3456 상자, divisor 15, `c697adb4985` RTPROBE, 10~20 초 창):
//!
//! | 조건 | 새 할당 | GC 해제 | 렌더 > 1 주기 | 렌더 worst |
//! |---|---|---|---|---|
//! | 회전+opacity (두 번) | 54 / 56 (약 2.2 GB) | 55 / 57 | 2 / 3 | 21.6 / 22.5 ms |
//! | 회전만 (두 번) | 0 / 0 | 0 / 0 | 0 / 0 | 2.1 / 1.7 ms |
//! | opacity 만 | 0 | 0 | 0 | 2.2 ms |
//!
//! opacity 만인 대조군도 매 프레임 같은 크기의 전용 surface 를 그리지만 스파이크가 없다 --
//! 비용은 그리는 것이 아니라 ***새로 만드는 것***이다. 렌더러의 텍스처 생성 CPU 시간 자체는
//! 작아서(최대 0.06 ms) 비용은 새 텍스처를 처음 쓰는 GPU 쪽으로 보인다(추정, 미측정).
//!
//! ## 무엇을 바꾸는가
//!
//! 전용 요청(공유 타깃에 못 넣는 태스크)에만, 정확 일치가 없을 때만 적용한다.
//!
//! 1. ★쉬고 있는 같은 포맷 전용 타깃 중 요청을 담는 가장 작은 것★을 빌린다.
//! 2. 없으면 요청과 그 후보들의 ★가로·세로 각각의 최대값(포락선)★ 으로 새로 만들고, 그
//!    포락선에 들어가는 쉬는 후보는 바로 해제한다([`supersedes`]).
//! 3. (2026-11-12 추가, 벽 런 뒤) GC 는 포맷마다 가장 큰 전용 타깃을 `keep_frames` 동안
//!    64 MB 임계로는 버리지 않는다([`gc_keeps`]) -- 아래 "포락선을 GC 에서 지킨다".
//!
//! 공유 크기(정사각 `max_shared_surface_size`) 타깃은 어디에도 끼지 않는다. 회전 한 바퀴
//! 안에서 포락선이 최대 크기로 수렴하고, 그 뒤로는 그 하나가 쓰이는 동안 새 할당이 없다.
//! 3 이 없으면 영역이 타일을 1 초 넘게 떠날 때 포락선을 잃는다(벽 런에서 실제로 그랬다).
//!
//! 큰 텍스처에 작은 태스크를 그리는 것은 공유 타깃이 매 프레임 하는 일과 같은 경로다 --
//! 프레임 빌더는 `texture_id` 와 `used_rect` 만 넘기고, 렌더러는 `used_rect` 만 지우고
//! (scissor), 투영과 샘플링은 실제 텍스처 크기를 쓴다.
//!
//! 끄면 upstream 동작 그대로다. pref 는 `gfx_wr_render_target_reuse_larger`(기본 켜짐),
//! 런처의 되돌리기 스위치는 `-NoRenderTargetReuse`. 3 만 끄려면
//! `gfx_wr_render_target_keep_frames=0`(런처 `-RenderTargetKeepFrames 0`).
//! 벽 검증(2026-11-12, log_rotation_opacity_judder_01/02, `a1f848f3d39`, 실제 IPWall 의
//! 회전+이동+opacity 이미지, 120 초씩, 30~110 초의 10 초 창):
//!
//! | 런 | 조건 | 새 할당 | GC 해제 | 운영자 육안 |
//! |---|---|---|---|---|
//! | 04 | upstream (`-NoRenderTargetReuse`) | 254~290 (10 초에 약 5 GB) | 262~293 | 반복 애니메이션 셋 중 마지막 구간에서 거의 항상 judder |
//! | 03 | 1·2 만 (`-RenderTargetKeepFrames 0`) | 19~20 (cold 7) | 4~6 | 등장 뒤 거의 없음 |
//! | 02 | 1·2·3 (기본) | 9~10 (cold 2) | 3~5 | 03 과 같음 |
//!
//! 1·2 가 이 judder 를 없앤다. 3 은 남은 할당을 절반으로 줄이지만(spared 10 초에 189~282)
//! 눈에 보이는 차이는 없었다. ★등장 순간의 judder 는 세 조건 모두에 있다★(등장 창의 주기 초과
//! 62/67/79) -- 페이지 기동·셰이더 컴파일·장면 등장 비용이 몰린 창이고 이 기구와 별건이다.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(true);

/// painter 가 pref 를 읽어 세운다(`webrender` 는 `servo-config` 에 의존하지 않는다).
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

#[inline]
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 풀에 든 렌더 타깃 하나를 [`pick_dedicated`] 와 [`supersedes`] 가 보는 모습.
///
/// ★크기를 `i32` 쌍으로 받는 이유★: 이 파일은 의존성이 없어서 `rustc --test` 로 혼자
/// 시험할 수 있다. webrender 는 Servo 워크스페이스 밖의 `[patch]` 크레이트라
/// `cargo test -p webrender` 가 안 되고, 단독 매니페스트는 상위 워크스페이스로 잡히며
/// dev-dependency(mozangle, rand 0.4)도 있다(2026-11-12 확인).
#[derive(Clone, Copy, Debug)]
pub struct PoolEntry {
    pub width: i32,
    pub height: i32,
    pub same_format: bool,
    pub active: bool,
    /// 이번 프레임에 이미 쓰였는가. ★이번 프레임의 앞 패스가 그린 텍스처일 수 있다★ --
    /// 그런 것은 해제하지 않는다(`gc_render_targets` 의 `used_this_frame` 과 같은 규칙).
    pub used_this_frame: bool,
}

/// [`pick_dedicated`] 의 답.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// 풀의 이 순번 타깃을 그대로 쓴다.
    Reuse(usize),
    /// 이 크기로 새로 만든다. 만들기 전에 [`supersedes`] 가 참인 것을 풀에서 해제한다.
    ///
    /// `cold` 는 ★후보가 하나도 없었다★ 는 뜻이다(2026-11-12 추가). 포락선이 GC 로 사라진
    /// 뒤의 첫 할당이 이것이고, 벽 런(log_rotation_opacity_judder_01/01)의 남은 할당이 "GC 뒤
    /// 다시 키우기" 인지를 RTPROBE 가 이 값으로 가른다.
    Allocate { width: i32, height: i32, cold: bool },
}

/// 공유 쪽 몫이 아니고, 같은 포맷이며, 지금 쓰이지 않는 타깃인가.
fn is_candidate(entry: &PoolEntry, shared: i32) -> bool {
    entry.same_format && !entry.active && !(entry.width == shared && entry.height == shared)
}

/// 정확 일치가 없는 ★전용★ 요청에 무엇을 줄지 고른다.
///
/// 1. 후보 중 요청을 담는 ★가장 작은★ 것을 빌린다 -- 작은 전용 태스크가 큰 텍스처를
///    차지해 큰 요청이 또 만들게 되는 일을 막는다.
/// 2. 없으면 요청과 후보들의 ★가로·세로 각각의 최대값(포락선)★ 으로 새로 만든다.
///
/// `shared` 는 공유 타깃의 한 변(`max_shared_surface_size`)이다.
pub fn pick_dedicated<I>(pool: I, width: i32, height: i32, shared: i32) -> Pick
where
    I: IntoIterator<Item = PoolEntry>,
{
    let mut best: Option<(usize, i64)> = None;
    let (mut env_w, mut env_h) = (width, height);
    let mut cold = true;
    for (index, entry) in pool.into_iter().enumerate() {
        if !is_candidate(&entry, shared) {
            continue;
        }
        cold = false;
        env_w = env_w.max(entry.width);
        env_h = env_h.max(entry.height);
        if entry.width >= width && entry.height >= height {
            let area = entry.width as i64 * entry.height as i64;
            if best.map_or(true, |(_, best_area)| area < best_area) {
                best = Some((index, area));
            }
        }
    }
    match best {
        Some((index, _)) => Pick::Reuse(index),
        None => Pick::Allocate {
            width: env_w,
            height: env_h,
            cold,
        },
    }
}

/// 포락선 `width` x `height` 를 새로 만들 때 이 타깃을 해제하는가.
///
/// ★해제하는 이유★: 남겨 두면 1 번(가장 작은 것)이 요청을 옛 타깃들에 나눠 주고, 그러면
/// 가장 큰 포락선이 60 프레임 동안 안 쓰이는 구간이 생겨 GC 가 버린다 -- 회전 한 바퀴마다
/// 다시 만든다. 해제하면 그 surface 의 몫이 포락선 하나로 모이고, 매 프레임 쓰이므로 GC 에
/// 걸리지 않는다. 포락선에 들어가는 쉬는 후보만, 그리고 이번 프레임에 쓰이지 않은 것만 푼다.
pub fn supersedes(entry: PoolEntry, width: i32, height: i32, shared: i32) -> bool {
    is_candidate(&entry, shared)
        && !entry.used_this_frame
        && entry.width <= width
        && entry.height <= height
}

// ## 포락선을 GC 에서 지킨다 (2026-11-12 추가)
//
// 벽 런(log_rotation_opacity_judder_01/01, `60c1bfd9a5a`, 실제 IPWall 의 회전+이동+opacity
// 이미지, 9 초 장면 주기)에서 위 두 규칙은 재할당 대부분을 흡수했지만(큰 것 재사용 10 초에 약
// 2200) 0 으로 수렴하지 않았다. 30~110 초의 모든 10 초 창이
//
//     새 할당 19~20 ≈ GC 해제 4~5 + 대체 해제 15
//
// 이고, GC 해제 4~5/10 초 ≈ painter 넷 x 9 초 주기마다 한 번이다. 즉 painter 마다 주기에 한 번
// ★포락선을 GC 로 잃고, 다시 나타날 때 처음부터 키운다★(GC 1 + 성장 약 4). 이미지가 이동하므로
// 어떤 타일에서는 주기 일부에만 보이고, 60 프레임 넘게 안 보이면 풀이 64 MB 를 넘은 상태라
// (포락선 하나가 최대 4096x4096 = 64 MB) GC 가 버린다. 개발 장비 프로브는 이동이 없어 포락선이
// 매 프레임 쓰였으므로 이 경우가 안 보였다. 위 산술은 집계에서 맞춘 추정이고, 이 블록이 더하는
// RTPROBE `cold` 와 `spared` 가 그것을 직접 가른다.
//
// 그래서 포맷마다 ★가장 큰 전용 타깃 하나★(그 포맷의 포락선)는 `keep_frames` 동안 안 쓰여도
// 64 MB 임계로는 버리지 않는다. 320 MB 레드라인에서는 그대로 버린다. 대가는 painter(GPU)당
// 포맷마다 최대 64 MB 를 그 시간만큼 더 쥐는 것이다.

/// 포락선을 GC 에서 지키는 프레임 수. 0 이면 upstream 그대로(60 프레임 규칙만).
static KEEP_FRAMES: AtomicU64 = AtomicU64::new(1800);

/// 이 포크의 공유 타깃 한 변. 풀 요청이 처음 올 때 적힌다(0 = 아직 모름 -> 지키지 않는다).
/// GC(`ResourceCache::end_frame`) 는 그 값을 받지 않으므로 여기 둔다.
static SHARED_SIZE: AtomicI32 = AtomicI32::new(0);

/// painter 가 pref(`gfx_wr_render_target_keep_frames`)를 읽어 세운다.
pub fn set_keep_frames(frames: u64) {
    KEEP_FRAMES.store(frames, Ordering::Relaxed);
}

#[inline]
pub fn keep_frames() -> u64 {
    KEEP_FRAMES.load(Ordering::Relaxed)
}

/// 렌더 태스크 그래프가 전용 요청을 낼 때 공유 타깃 한 변을 적는다.
#[inline]
pub fn note_shared_size(shared: i32) {
    SHARED_SIZE.store(shared, Ordering::Relaxed);
}

#[inline]
pub fn shared_size() -> i32 {
    SHARED_SIZE.load(Ordering::Relaxed)
}

/// 포맷이 넷을 넘을 일은 없다(렌더 타깃은 RGBA8 과 R8 둘뿐이다).
const MAX_FORMATS: usize = 4;

/// 포맷마다 가장 큰 전용 타깃의 순번 -- 그 포맷의 포락선. [`envelopes`] 가 만든다.
#[derive(Clone, Copy, Debug, Default)]
pub struct Envelopes {
    slots: [Option<(u32, usize, i64)>; MAX_FORMATS],
}

impl Envelopes {
    pub fn contains(&self, index: usize) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|&(_, slot, _)| slot == index)
    }
}

/// `(가로, 세로, 포맷 키)` 목록에서 포맷마다 넓이가 가장 큰 전용 타깃을 고른다. 공유 크기
/// 정사각은 빼고, 넓이가 같으면 앞의 것이다. 포맷이 [`MAX_FORMATS`] 를 넘으면 넘친 포맷은
/// 지키지 않는다(그 경우 upstream 동작).
pub fn envelopes<I>(pool: I, shared: i32) -> Envelopes
where
    I: IntoIterator<Item = (i32, i32, u32)>,
{
    let mut result = Envelopes::default();
    for (index, (width, height, format)) in pool.into_iter().enumerate() {
        if width == shared && height == shared {
            continue;
        }
        let area = width as i64 * height as i64;
        match result
            .slots
            .iter_mut()
            .find(|slot| matches!(slot, Some((f, _, _)) if *f == format))
        {
            Some(slot) => {
                let larger = matches!(slot, Some((_, _, best)) if area > *best);
                if larger {
                    *slot = Some((format, index, area));
                }
            },
            None => {
                if let Some(empty) = result.slots.iter_mut().find(|slot| slot.is_none()) {
                    *empty = Some((format, index, area));
                }
            },
        }
    }
    result
}

/// GC 가 이 타깃을 ★포락선이라서★ 남기는가. 레드라인 위에서는 남기지 않는다.
///
/// `used_within_keep` 는 `RenderTarget::used_recently(현재, keep_frames)` 그대로다.
pub fn gc_keeps(is_envelope: bool, used_within_keep: bool, above_red_line: bool) -> bool {
    keep_frames() > 0 && is_envelope && used_within_keep && !above_red_line
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHARED: i32 = 2048;

    fn idle(width: i32, height: i32) -> PoolEntry {
        PoolEntry {
            width,
            height,
            same_format: true,
            active: false,
            used_this_frame: false,
        }
    }

    fn alloc(width: i32, height: i32, cold: bool) -> Pick {
        Pick::Allocate {
            width,
            height,
            cold,
        }
    }

    #[test]
    fn an_empty_pool_allocates_the_request_cold() {
        assert_eq!(
            pick_dedicated([], 3840, 1280, SHARED),
            alloc(3840, 1280, true)
        );
    }

    #[test]
    fn a_smaller_request_borrows_an_idle_larger_target() {
        assert_eq!(
            pick_dedicated([idle(3840, 1280)], 3584, 1024, SHARED),
            Pick::Reuse(0)
        );
    }

    #[test]
    fn the_smallest_target_that_holds_the_request_wins() {
        let pool = [idle(4096, 4096), idle(3840, 1792), idle(4096, 1792)];
        assert_eq!(pick_dedicated(pool, 3584, 1536, SHARED), Pick::Reuse(1));
    }

    #[test]
    fn a_miss_grows_to_the_envelope_of_the_idle_dedicated_targets() {
        // (4096, 1536) is wider than both idle targets, so neither holds it: grow to the
        // widest and the tallest seen. There were candidates, so it is not cold.
        let pool = [idle(3840, 1280), idle(3584, 1792)];
        assert_eq!(
            pick_dedicated(pool, 4096, 1536, SHARED),
            alloc(4096, 1792, false)
        );
    }

    #[test]
    fn shared_size_targets_are_neither_borrowed_nor_part_of_the_envelope_nor_freed() {
        let pool = [idle(SHARED, SHARED)];
        assert_eq!(
            pick_dedicated(pool, 1024, 512, SHARED),
            alloc(1024, 512, true)
        );
        let pool = [idle(SHARED, SHARED), idle(1024, 512)];
        assert_eq!(
            pick_dedicated(pool, 2304, 256, SHARED),
            alloc(2304, 512, false)
        );
        assert!(!supersedes(idle(SHARED, SHARED), 4096, 4096, SHARED));
    }

    #[test]
    fn an_active_target_is_neither_borrowed_nor_part_of_the_envelope_nor_freed() {
        let busy = PoolEntry {
            active: true,
            ..idle(4096, 4096)
        };
        assert_eq!(
            pick_dedicated([busy], 3584, 1024, SHARED),
            alloc(3584, 1024, true)
        );
        let busy = PoolEntry {
            active: true,
            ..idle(1024, 1024)
        };
        assert!(!supersedes(busy, 4096, 4096, SHARED));
    }

    #[test]
    fn another_format_is_neither_borrowed_nor_part_of_the_envelope_nor_freed() {
        let alpha = PoolEntry {
            same_format: false,
            ..idle(4096, 4096)
        };
        assert_eq!(
            pick_dedicated([alpha], 3584, 1024, SHARED),
            alloc(3584, 1024, true)
        );
        let alpha = PoolEntry {
            same_format: false,
            ..idle(1024, 1024)
        };
        assert!(!supersedes(alpha, 4096, 4096, SHARED));
    }

    #[test]
    fn a_target_used_earlier_in_this_frame_is_not_freed() {
        let used = PoolEntry {
            used_this_frame: true,
            ..idle(3840, 1280)
        };
        assert!(!supersedes(used, 4096, 2048, SHARED));
        assert!(supersedes(idle(3840, 1280), 4096, 2048, SHARED));
        assert!(
            !supersedes(idle(4096, 4096), 4096, 2048, SHARED),
            "not inside the envelope"
        );
    }

    #[test]
    fn the_envelope_is_the_largest_non_shared_target_of_each_format() {
        const RGBA: u32 = 3;
        const R8: u32 = 1;
        let pool = [
            (SHARED, SHARED, RGBA),
            (3840, 1792, RGBA),
            (4096, 2816, RGBA),
            (2304, 512, R8),
            (3072, 1024, RGBA),
        ];
        let envelopes = envelopes(pool, SHARED);
        assert!(
            !envelopes.contains(0),
            "the shared surface is never an envelope"
        );
        assert!(!envelopes.contains(1));
        assert!(envelopes.contains(2), "largest RGBA");
        assert!(envelopes.contains(3), "the only R8");
        assert!(!envelopes.contains(4));
    }

    #[test]
    fn the_gc_keeps_only_an_envelope_used_within_the_window_and_below_the_red_line() {
        assert!(gc_keeps(true, true, false));
        assert!(!gc_keeps(false, true, false), "not the envelope");
        assert!(!gc_keeps(true, false, false), "idle past the keep window");
        assert!(!gc_keeps(true, true, true), "above the red line");
    }

    /// One pool entry in the simulation below.
    struct Sim {
        width: i32,
        height: i32,
        last: usize,
    }

    /// Which pool rule the simulation runs.
    #[derive(Clone, Copy)]
    enum Rule {
        /// Upstream: exact match or allocate the request.
        ExactOnly,
        /// Borrow larger, grow to the envelope, free what it supersedes.
        ReuseLarger,
        /// The same, and the GC keeps the envelope for this many frames.
        ReuseLargerKeep(usize),
    }

    /// Allocations per lap for a surface that asks for `cycle` once per frame (`None` = the
    /// surface is not on this tile that frame), lap after lap, with the pool run the way
    /// `ResourceCache` runs it: exact match first, then the rule, then (above the 64 MB
    /// threshold, which is where the churn was measured) a GC that drops whatever was not used
    /// in the last `window` frames. `window` is shorter than a lap, as 60 frames is shorter than
    /// a 9-12 s cycle.
    fn allocations_per_lap(
        rule: Rule,
        cycle: &[Option<(i32, i32)>],
        laps: usize,
        window: usize,
    ) -> Vec<usize> {
        let mut pool: Vec<Sim> = Vec::new();
        let mut per_lap = vec![0; laps];
        let mut frame = 0;
        for lap in 0..laps {
            for request in cycle {
                frame += 1;
                if let Some((w, h)) = *request {
                    if let Some(entry) = pool.iter_mut().find(|e| e.width == w && e.height == h) {
                        entry.last = frame;
                    } else {
                        let view = |e: &Sim| PoolEntry {
                            width: e.width,
                            height: e.height,
                            same_format: true,
                            active: false,
                            used_this_frame: e.last == frame,
                        };
                        let pick = match rule {
                            Rule::ExactOnly => alloc(w, h, true),
                            Rule::ReuseLarger | Rule::ReuseLargerKeep(_) => {
                                pick_dedicated(pool.iter().map(view), w, h, SHARED)
                            },
                        };
                        match pick {
                            Pick::Reuse(index) => pool[index].last = frame,
                            Pick::Allocate { width, height, .. } => {
                                per_lap[lap] += 1;
                                if !matches!(rule, Rule::ExactOnly) {
                                    pool.retain(|e| !supersedes(view(e), width, height, SHARED));
                                }
                                pool.push(Sim {
                                    width,
                                    height,
                                    last: frame,
                                });
                            },
                        }
                    }
                }
                let keep = match rule {
                    Rule::ReuseLargerKeep(keep) => keep,
                    _ => 0,
                };
                let envelope = envelopes(pool.iter().map(|e| (e.width, e.height, 0)), SHARED);
                let mut index = 0;
                pool.retain(|e| {
                    let recent = frame - e.last < window;
                    let kept = keep > 0 && envelope.contains(index) && frame - e.last < keep;
                    index += 1;
                    recent || kept
                });
            }
        }
        per_lap
    }

    /// 256-rounded sizes a rotating surface asked for, in order, over one lap.
    const ROTATION: [Option<(i32, i32)>; 6] = [
        Some((3840, 1280)),
        Some((3584, 1792)),
        Some((4096, 1536)),
        Some((3328, 2304)),
        Some((2560, 2816)),
        Some((3584, 1792)),
    ];

    /// The same rotation, but the surface leaves this tile for longer than the GC window
    /// (four frames against a window of three), as a moving image does on the wall.
    const ROTATION_WITH_ABSENCE: [Option<(i32, i32)>; 10] = [
        Some((3840, 1280)),
        Some((3584, 1792)),
        Some((4096, 1536)),
        Some((3328, 2304)),
        Some((2560, 2816)),
        Some((3584, 1792)),
        None,
        None,
        None,
        None,
    ];

    #[test]
    fn upstream_reallocates_every_lap_when_the_gc_window_is_shorter_than_a_lap() {
        // The simulation must reproduce the measured churn, or the next test proves nothing.
        let per_lap = allocations_per_lap(Rule::ExactOnly, &ROTATION, 4, 3);
        assert!(per_lap[1..].iter().all(|&n| n > 0), "per lap = {per_lap:?}");
    }

    #[test]
    fn a_rotating_surface_stops_allocating_after_the_first_lap() {
        let per_lap = allocations_per_lap(Rule::ReuseLarger, &ROTATION, 4, 3);
        assert!(per_lap[0] > 0);
        assert_eq!(&per_lap[1..], &[0, 0, 0], "per lap = {per_lap:?}");
    }

    #[test]
    fn an_absence_longer_than_the_gc_window_regrows_every_lap_without_keep() {
        // The wall run of 60c1bfd9a5a: it must reproduce, or the keep test proves nothing.
        let per_lap = allocations_per_lap(Rule::ReuseLarger, &ROTATION_WITH_ABSENCE, 4, 3);
        assert!(per_lap[1..].iter().all(|&n| n > 0), "per lap = {per_lap:?}");
    }

    #[test]
    fn keeping_the_envelope_survives_the_absence() {
        let per_lap = allocations_per_lap(Rule::ReuseLargerKeep(20), &ROTATION_WITH_ABSENCE, 4, 3);
        assert!(per_lap[0] > 0);
        assert_eq!(&per_lap[1..], &[0, 0, 0], "per lap = {per_lap:?}");
    }

    #[test]
    fn an_absence_longer_than_the_keep_window_regrows() {
        let per_lap = allocations_per_lap(Rule::ReuseLargerKeep(4), &ROTATION_WITH_ABSENCE, 4, 3);
        assert!(per_lap[1..].iter().all(|&n| n > 0), "per lap = {per_lap:?}");
    }

    #[test]
    fn the_switches_default_to_on() {
        assert!(is_enabled());
        assert_eq!(keep_frames(), 1800);
    }
}

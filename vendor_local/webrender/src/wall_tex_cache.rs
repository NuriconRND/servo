/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! 텍스처 캐시 내보내기 예산의 ★하한★ 과, `wall_tex_probe` 가 쓰는 순수 도우미.
//!
//! ## 왜 (2026-11-13 추가)
//!
//! 회전 + opacity judder 의 렌더 타깃 쪽 원인을 고친 뒤(`wall_rt_reuse.rs`) 운영자가 남은 차이를
//! 보고했다: 같은 회전 + opacity 애니메이션을 이미지와 비디오에 걸고 교대로 전환하면 ★이미지
//! 쪽이 전환 애니메이션에서 judder 가 더 많다★. 이미지는 3072x4608(디코드 56.6 MB) 였다. 이미지를
//! 4096 이하로 줄였더니 "줄긴 했지만 확 체감될 정도는 아니었고 등장/퇴장 애니메이션에서는 여전히
//! 발생" 했다.
//!
//! 코드로 확인한 차이(측정 전):
//!
//! 1. 이미지는 WR 텍스처 캐시에 올려 둔 RGBA 를 그리고, 비디오는 디코더의 D3D11 텍스처를 그대로
//!    가져온다 -- 비디오는 텍스처 캐시를 거치지 않는다.
//! 2. 출력 페이지는 다음 장면을 5 초 전에 `opacity: 0` 으로 미리 마운트하는데, opacity 가 0.001
//!    이하인 picture 는 `is_visible()` 이 거짓이라 안을 방문하지 않는다 -- ★이미지는 미리 올라가지
//!    않고★ 등장 첫 프레임에 painter 마다 전체를 올린다.
//! 3. 내보내기 예산이 작다. standalone 8 MB, 공유 RGBA 아틀라스 약 5.3 MB(2048x2048 의 1/3)이고,
//!    예산의 몇 배인지에 따라 "안 쓰인 지 몇 프레임이면 내보낼지" 가 400 -> 1 로 준다
//!    (`TextureCache::should_continue_evicting`). 4096x2731 이미지(44.7 MB)는 5 배라 10 프레임,
//!    3072x4608(56.6 MB)은 7 배라 1 프레임이다. 화면에서 빠지면 곧 내보내고 돌아오면 다시 올린다.
//! 4. 4096 을 넘는 이미지는 512 타일로 쪼개져(`image_tiling_threshold`) 공유 아틀라스로 가고, 보이는
//!    타일만 요청되므로 회전·이동 중에도 타일 업로드와 내보내기가 이어진다.
//!    ★정정(2026-11-13 같은 날)★: 위 3072x4608 이미지는 쪼개지지 않는다. Servo 가 디코드 뒤
//!    GPU 에 올리기 전에 `dom_image_max_decoded_pixels`(기본 3840x2160 픽셀, `53ff7fd9a84`,
//!    2026-08-31)로 줄이므로 2352x3527(33.2 MB)한 장으로 올라간다. 4 는 그 상한 안에서 한 변이
//!    4096 을 넘는 이미지(예: 1000x8000 파노라마)나 상한을 끈 경우에만 해당한다.
//!
//! 개발 장비 실측(2026-11-13, 이 이미지, 회전+opacity, 상자가 12 초 주기마다 약 3.5 초 화면 밖,
//! 20~40 초): 기본 예산에서는 창마다 31.6 MB 를 내보내고 다시 올리며 그 프레임이 업로드+생성
//! 17.5~27 ms(주기 초과 1~2, 최악 19.2~40.7 ms). 하한 1024 MB 에서는 첫 1 회 뒤 업로드·내보내기
//! 0, 주기 초과 0, 최악 1.7~1.8 ms. 타일 문턱(4096/16384)은 이 이미지에 차이가 없었다.
//!
//! 이 파일의 레버는 3 을 막는다: `set_eviction_floor_mb` 가 0 이 아니면 모든 예산의 내보내기
//! 기준을 그 값 아래로 내리지 않는다. 그러면 그 양까지는 화면에서 빠진 이미지도 GPU 에 남는다.
//! 대가는 painter(GPU)당 그만큼의 VRAM 이다. ★공유 아틀라스는 여러 장이 되면 기준 아래에서도
//! 400 프레임 넘게 안 쓰인 것을 계속 내보낸다★(조각 모음 목적, upstream 동작) -- 쪼개진 큰
//! 이미지는 그래서 이 레버만으로는 다 남지 않는다. 4 는 painter 의 `image_tiling_threshold`
//! (pref `gfx_wr_image_tiling_threshold`)로 따로 가른다.

use std::sync::atomic::{AtomicU64, Ordering};

/// 내보내기 기준의 하한(바이트). 0 이면 upstream 그대로.
static EVICTION_FLOOR: AtomicU64 = AtomicU64::new(0);

/// painter 가 pref(`gfx_wr_texture_cache_evict_budget_mb`)를 읽어 세운다.
pub fn set_eviction_floor_mb(mb: u64) {
    EVICTION_FLOOR.store(mb.saturating_mul(1024 * 1024), Ordering::Relaxed);
}

/// upstream 이 고른 내보내기 기준에 하한을 건다.
#[inline]
pub fn floor_eviction_threshold(upstream: usize) -> usize {
    upstream.max(EVICTION_FLOOR.load(Ordering::Relaxed) as usize)
}

/// 무거운 업로드 프레임 하나를 원자 두 칸에 담는다: (유닉스 ms, KiB << 24 | µs).
///
/// µs 는 24 비트(약 16.7 초)에서, KiB 는 40 비트에서 잘린다 -- 한 프레임에 그보다 큰 값은 없다.
pub fn pack_heavy(unix_ms: u64, bytes: u64, nanos: u64) -> (u64, u64) {
    let kib = (bytes / 1024).min((1u64 << 40) - 1);
    let micros = (nanos / 1000).min((1u64 << 24) - 1);
    (unix_ms, (kib << 24) | micros)
}

/// [`pack_heavy`] 의 반대: (유닉스 ms, KiB, µs).
pub fn unpack_heavy(a: u64, b: u64) -> (u64, u64, u64) {
    (a, b >> 24, b & ((1u64 << 24) - 1))
}

/// 유닉스 ms 를 UTC 의 (시, 분, 초, ms) 로. 로그 줄의 `...Z` 시각과 맞춰 읽기 위한 것이다.
pub fn utc_hms(unix_ms: u64) -> (u64, u64, u64, u64) {
    let ms = unix_ms % 1000;
    let secs = (unix_ms / 1000) % 86_400;
    (secs / 3600, (secs / 60) % 60, secs % 60, ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_floor_is_off_by_default_and_only_raises() {
        assert_eq!(floor_eviction_threshold(8 * 1024 * 1024), 8 * 1024 * 1024);
        set_eviction_floor_mb(512);
        assert_eq!(floor_eviction_threshold(8 * 1024 * 1024), 512 * 1024 * 1024);
        assert_eq!(
            floor_eviction_threshold(600 * 1024 * 1024),
            600 * 1024 * 1024,
            "never lowers an upstream threshold"
        );
        set_eviction_floor_mb(0);
        assert_eq!(floor_eviction_threshold(5_592_405), 5_592_405);
    }

    #[test]
    fn a_heavy_frame_round_trips() {
        let (a, b) = pack_heavy(1_794_450_144_985, 56_623_104, 18_345_678);
        assert_eq!(unpack_heavy(a, b), (1_794_450_144_985, 55_296, 18_345));
    }

    #[test]
    fn oversized_values_saturate_instead_of_spilling() {
        let (a, b) = pack_heavy(1, u64::MAX, u64::MAX);
        let (_, kib, micros) = unpack_heavy(a, b);
        assert_eq!(kib, (1u64 << 40) - 1);
        assert_eq!(micros, (1u64 << 24) - 1);
    }

    #[test]
    fn utc_clock_matches_the_log_timestamps() {
        // 2026-11-12T04:38:06.250Z
        assert_eq!(utc_hms(1_794_458_286_250), (4, 38, 6, 250));
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! 렌더 타깃 풀이 ★전용 타깃을 얼마나 새로 만들고 버리는지★ 와, 중간 surface 가
//! `MAX_SURFACE_SIZE` 로 ★축소 래스터화★ 되는 횟수를 센다 (`RTPROBE`).
//!
//! ## 왜 (2026-11-12 추가)
//!
//! 운영자 관찰(2026-11-09, log_webgl_perf_01/16): 복합채널 이미지에 회전 애니메이션만
//! 걸면 judder 가 없는데, 같은 애니메이션에 opacity 변화를 더하면 judder 가 난다.
//! 출력 페이지(`RegionBox.tsx`)는 두 경우 모두 같은 요소에 WAAPI 하나를 걸고, 차이는
//! 키프레임에 `opacity` 가 실리느냐뿐이다. 엔진 쪽에서 그 차이가 만드는 것은 하나다 --
//! ★opacity 필터 스태킹 컨텍스트, 즉 중간 surface.★
//!
//! 그 surface 는 회전한 좌표계 안에 있으므로 `PicturePrimitive::assign_surface` 가
//! 래스터 공간을 ***자기 로컬 공간***으로 잡는다(`raster_spatial_node_index =
//! surface_spatial_node_index`). 크기는 `get_surface_rects` 가 "prim 사각형 ∩ 부모
//! 클립을 로컬로 역사상한 사각형" 으로 정하는데, 부모 클립(= 이 포크에서는 painter 의 타일)을
//! 회전 공간으로 역사상한 사각형은 ***각도마다 다르다.*** 그래서 surface 크기가 매 프레임
//! 바뀐다. 회전만 걸린 경우에는 중간 surface 가 없고 이미지는 타일에 바로 그려진다.
//!
//! 크기가 매 프레임 바뀌는 surface 가 부딪히는 자리가 둘이다. 이 프로브는 그 둘을 센다.
//!
//! 1. ★전용 렌더 타깃의 정확 일치 재사용★ (`render_task_graph.rs`,
//!    `ResourceCache::get_or_create_render_target_from_pool`). 2048 을 넘는 태스크는
//!    공유 타깃에 못 들어가고 256 단위로 올림한 전용 크기를 받는데, 풀은 ***크기가 정확히
//!    같을 때만*** 재사용한다. 각도가 256 경계를 넘을 때마다 새 텍스처(3840x2560 이면
//!    39 MB, 4096x4096 이면 64 MB)를 만들고, 풀이 64 MB 를 넘으면 60 프레임 안 쓴 것을
//!    버린다(`ResourceCache::end_frame`). 회전이 그 각도로 돌아오면 다시 만든다.
//! 2. ★`MAX_SURFACE_SIZE`(4096) 축소★ (`get_surface_rects`). 넘으면
//!    `device_pixel_scale = 4096 / 최대 변` 으로 줄여 그리는데, 그 최대 변이 위 이유로
//!    각도마다 다르다 -- 배율과 스냅 위치가 매 프레임 바뀐다. 벽 타일(5760x2160)의 대각선은
//!    6152 라 일어날 수 있고, 개발 장비 타일(3840x1080)의 대각선은 3989 라 ★일어날 수 없다.★
//!
//! 둘 다 아직 ***가설***이다. 이 프로브는 "그 구성에서 실제로 일어나는가, 얼마나 자주,
//! 얼마나 비싼가" 를 수치로 낸다.
//!
//! ## 성능 규칙 (타이밍 경로에 I/O 금지)
//!
//! 쌓는 쪽은 원자 연산만 한다. 출력은 전용 리포터 스레드가 [`REPORT_EVERY`] 마다 한다.
//! ★꺼져 있으면 각 호출 자리에 남는 것은 `AtomicBool::load(Relaxed)` 하나다.★ 렌더러의
//! 생성 시각은 그 자리가 원래 재던 `zeitstempel::now()` 를 그대로 쓰고, 삭제 시각만
//! 켜졌을 때 따로 잰다. 줄은 ***누적과 직전 창의 차분을 함께*** 찍는다 -- 누적값을 창
//! 값으로 읽는 실수를 여러 번 했다.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 리포터가 줄을 찍는 간격.
const REPORT_EVERY: Duration = Duration::from_secs(10);

static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

/// 공유(2048 정사각) 타깃 요청 수.
static SHARED_REQUESTS: AtomicU64 = AtomicU64::new(0);
/// 전용 타깃 요청 수 -- 2048 을 넘거나 공유할 수 없는 태스크.
static DEDICATED_REQUESTS: AtomicU64 = AtomicU64::new(0);
/// 요청된 전용 타깃의 가장 큰 변(256 올림 뒤).
static DEDICATED_MAX_DIM: AtomicU64 = AtomicU64::new(0);
/// 풀에 맞는 것이 없어 새로 할당한 수와 바이트.
static POOL_MISSES: AtomicU64 = AtomicU64::new(0);
static POOL_MISS_BYTES: AtomicU64 = AtomicU64::new(0);
/// ★2026-11-12★ 정확 일치는 없었지만 더 큰 쉬는 타깃을 빌린 수(`wall_rt_reuse.rs` 1 번).
static REUSE_LARGER: AtomicU64 = AtomicU64::new(0);
/// ★2026-11-12★ 포락선을 새로 만들며 대체해 푼 수와 바이트(`wall_rt_reuse.rs` 2 번).
static SUPERSEDED: AtomicU64 = AtomicU64::new(0);
static SUPERSEDED_BYTES: AtomicU64 = AtomicU64::new(0);
/// ★2026-11-12★ 새 할당 중 후보가 하나도 없던 것(`wall_rt_reuse::Pick::Allocate::cold`).
/// 포락선이 GC 로 사라진 뒤 다시 키우는 사슬의 첫 할당이 이것이다.
static COLD_MISSES: AtomicU64 = AtomicU64::new(0);
/// ★2026-11-12★ GC 가 버렸을 것을 포락선이라서 남긴 횟수(GC 가 도는 프레임마다 센다).
static GC_SPARED: AtomicU64 = AtomicU64::new(0);
/// 풀 GC 가 버린 수와 바이트.
static GC_FREES: AtomicU64 = AtomicU64::new(0);
static GC_FREE_BYTES: AtomicU64 = AtomicU64::new(0);
/// 렌더러가 렌더 타깃 텍스처를 ***실제로*** 만든 수(같은 배치의 삭제분 재사용은 빼고),
/// 그 시간의 합과 최대(ns).
static RT_CREATES: AtomicU64 = AtomicU64::new(0);
static RT_CREATE_NS: AtomicU64 = AtomicU64::new(0);
static RT_CREATE_MAX_NS: AtomicU64 = AtomicU64::new(0);
/// 렌더러가 렌더 타깃 텍스처를 지운 수, 그 시간의 합과 최대(ns).
static RT_DELETES: AtomicU64 = AtomicU64::new(0);
static RT_DELETE_NS: AtomicU64 = AtomicU64::new(0);
static RT_DELETE_MAX_NS: AtomicU64 = AtomicU64::new(0);
/// `MAX_SURFACE_SIZE` 로 축소된 surface 수와, 그때의 가장 작은 배율(1e-3 단위).
static SURFACE_LIMITED: AtomicU64 = AtomicU64::new(0);
static SURFACE_LIMITED_MIN_MILLI: AtomicU64 = AtomicU64::new(u64::MAX);

/// 프로브가 켜져 있는가. ★모든 호출 자리가 이 값으로 먼저 건너뛴다.★
#[inline]
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 프로브를 켜고 리포터 스레드를 띄운다. ★여러 painter 가 불러도 한 번만 띄운다.★
/// 이 포크에서는 painter(GPU)마다 WebRender 가 하나씩 있고, 수치는 그 전부의 합이다.
pub fn start() {
    if STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    ENABLED.store(true, Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name(String::from("WrRtProbe"))
        .spawn(|| {
            let started = std::time::Instant::now();
            let mut previous = Snapshot::default();
            let mut tiles = TileReport::default();
            loop {
                std::thread::sleep(REPORT_EVERY);
                let now = Snapshot::take();
                report(started.elapsed(), &now, &previous);
                report_tiles(started.elapsed(), &mut tiles);
                previous = now;
            }
        });
    if let Err(error) = spawned {
        warn!("RTPROBE: 리포터 스레드를 띄우지 못했다: {error}; 요약이 나오지 않는다");
    }
}

/// 렌더 태스크 그래프가 풀에 타깃을 요청했다. `dedicated` 는 공유 타깃에 못 넣는 경우.
#[inline]
pub fn note_pool_request(dedicated: bool, width: i32, height: i32) {
    if !is_enabled() {
        return;
    }
    if dedicated {
        DEDICATED_REQUESTS.fetch_add(1, Ordering::Relaxed);
        DEDICATED_MAX_DIM.fetch_max(width.max(height).max(0) as u64, Ordering::Relaxed);
    } else {
        SHARED_REQUESTS.fetch_add(1, Ordering::Relaxed);
    }
}

/// 풀에 맞는 것이 없어 새 렌더 타깃을 할당했다.
#[inline]
pub fn note_pool_miss(bytes: usize) {
    if !is_enabled() {
        return;
    }
    POOL_MISSES.fetch_add(1, Ordering::Relaxed);
    POOL_MISS_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// 정확 일치 대신 더 큰 쉬는 타깃을 빌렸다.
#[inline]
pub fn note_reuse_larger() {
    if !is_enabled() {
        return;
    }
    REUSE_LARGER.fetch_add(1, Ordering::Relaxed);
}

/// 포락선을 만들며 쉬는 타깃 하나를 대체해 풀었다.
#[inline]
pub fn note_superseded(bytes: usize) {
    if !is_enabled() {
        return;
    }
    SUPERSEDED.fetch_add(1, Ordering::Relaxed);
    SUPERSEDED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// 후보가 하나도 없는 상태에서 새로 만들었다.
#[inline]
pub fn note_cold_miss() {
    if !is_enabled() {
        return;
    }
    COLD_MISSES.fetch_add(1, Ordering::Relaxed);
}

/// GC 가 포락선이라서 남겼다.
#[inline]
pub fn note_gc_spared() {
    if !is_enabled() {
        return;
    }
    GC_SPARED.fetch_add(1, Ordering::Relaxed);
}

/// 풀 GC 가 렌더 타깃 하나를 버렸다.
#[inline]
pub fn note_gc_free(bytes: usize) {
    if !is_enabled() {
        return;
    }
    GC_FREES.fetch_add(1, Ordering::Relaxed);
    GC_FREE_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// 렌더러가 렌더 타깃 텍스처 하나를 만들었다(`nanos` 는 그 생성에 든 시간).
#[inline]
pub fn note_rt_create(nanos: u64) {
    if !is_enabled() {
        return;
    }
    RT_CREATES.fetch_add(1, Ordering::Relaxed);
    RT_CREATE_NS.fetch_add(nanos, Ordering::Relaxed);
    RT_CREATE_MAX_NS.fetch_max(nanos, Ordering::Relaxed);
}

/// 렌더러가 렌더 타깃 텍스처 하나를 지웠다.
#[inline]
pub fn note_rt_delete(nanos: u64) {
    if !is_enabled() {
        return;
    }
    RT_DELETES.fetch_add(1, Ordering::Relaxed);
    RT_DELETE_NS.fetch_add(nanos, Ordering::Relaxed);
    RT_DELETE_MAX_NS.fetch_max(nanos, Ordering::Relaxed);
}

/// surface 하나가 `MAX_SURFACE_SIZE` 로 축소됐다. `scale` 은 그 결과 배율.
#[inline]
pub fn note_surface_limited(scale: f32) {
    if !is_enabled() {
        return;
    }
    SURFACE_LIMITED.fetch_add(1, Ordering::Relaxed);
    let milli = (scale.max(0.0) as f64 * 1000.0).round() as u64;
    SURFACE_LIMITED_MIN_MILLI.fetch_min(milli, Ordering::Relaxed);
}

// ## picture cache 타일을 얼마나 다시 그리는가 -- `TILEPROBE`, `TILESEC` (2026-11-16 추가)
//
// 벽 런 log_rotation_opacity_judder_01/04·05(2026-11-16): 보드 교체(두 보드가 2.5 초 동안 통째로
// 미끄러진다) 중 16.7 ms 를 넘긴 렌더가 전환 구간에 몰리고(07: 246 중 194, 08: 317 중 210), 그
// 시간은 거의 전부 `device.end_frame` 대기(중앙 18 ms)다 -- GPU 역압. opacity 를 빼도(08) 그대로였다.
// 가설: 보드가 움직이면 그 위의 모든 프리미티브가 picture cache 타일 기준으로 움직이므로 ★painter 의
// 모든 타일을 매 프레임 다시 래스터한다★. 정상 구간에서는 움직이는 영역의 타일만 다시 그린다.
// 이 계측이 그 양을 직접 센다.
//
// `draw_picture_cache_target` 이 다시 그리는 타일마다 한 번 불린다(네이티브 compositor 경로에서는
// 그 타일의 compositor surface 에 그린다). 그 `dirty_rect` 면적을 렌더러 스레드 지역 값에 더하고,
// 프레임 그리기가 끝나면(`flush_tile_frame`) 원자 계수기로 넘긴다. painter 마다 렌더러 스레드가
// 따로라 스레드 지역 누적이 곧 painter 별 프레임 합이다.
//
// 초 단위 줄(`TILESEC`)은 각 렌더러 스레드가 벽시계 초가 바뀔 때 직전 초의 합을 원자 링에 넣고,
// 리포터가 초별로 painter 넷을 합쳐 찍는다 -- 프레임마다 줄을 찍지 않으면서 2.5 초 전환과 맞춰 볼
// 해상도를 얻는다. 꺼져 있으면 두 자리 모두 `AtomicBool` 로드 하나다.

/// 한 프레임에 이만큼(px) 이상 다시 그리면 큰 래스터 프레임이다. 벽 타일 하나가 5760x2160 =
/// 12.4 Mpx 이므로 그 절반쯤이다.
const FULL_RASTER_PX: u64 = 6_000_000;

thread_local! {
    /// (이 프레임에 다시 그린 타일 수, 면적 px)
    static TILE_FRAME: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    /// (현재 초, 그 초의 다시 그린 면적 kpx, 프레임 수, 큰 래스터 프레임 수)
    static TILE_SECOND: Cell<(u64, u64, u64, u64)> = const { Cell::new((0, 0, 0, 0)) };
}

static TILE_FRAMES: AtomicU64 = AtomicU64::new(0);
static TILES: AtomicU64 = AtomicU64::new(0);
static TILE_PX: AtomicU64 = AtomicU64::new(0);
static TILE_FULL_FRAMES: AtomicU64 = AtomicU64::new(0);
/// 직전 보고 이후의 프레임당 최대값. 리포터가 `swap(0)` 으로 창을 끊는다.
static TILE_FRAME_MAX_TILES: AtomicU64 = AtomicU64::new(0);
static TILE_FRAME_MAX_PX: AtomicU64 = AtomicU64::new(0);

const SECOND_RING: usize = 256;
static SECOND_A: [AtomicU64; SECOND_RING] = [const { AtomicU64::new(0) }; SECOND_RING];
static SECOND_B: [AtomicU64; SECOND_RING] = [const { AtomicU64::new(0) }; SECOND_RING];
static SECOND_NEXT: AtomicU64 = AtomicU64::new(0);

/// 렌더러가 picture cache 타일 하나를 다시 그린다.
#[inline]
pub fn note_tile_drawn(width: i32, height: i32) {
    if !is_enabled() {
        return;
    }
    let area = width.max(0) as u64 * height.max(0) as u64;
    TILE_FRAME.with(|cell| {
        let (n, px) = cell.get();
        cell.set((n + 1, px + area));
    });
}

/// 렌더러가 한 프레임을 다 그렸다. 이 스레드가 이 프레임에 다시 그린 양을 넘긴다.
pub fn flush_tile_frame() {
    if !is_enabled() {
        return;
    }
    let (n, px) = TILE_FRAME.with(|cell| cell.replace((0, 0)));
    let full = px >= FULL_RASTER_PX;
    if n > 0 {
        TILE_FRAMES.fetch_add(1, Ordering::Relaxed);
        TILES.fetch_add(n, Ordering::Relaxed);
        TILE_PX.fetch_add(px, Ordering::Relaxed);
        TILE_FRAME_MAX_TILES.fetch_max(n, Ordering::Relaxed);
        TILE_FRAME_MAX_PX.fetch_max(px, Ordering::Relaxed);
        if full {
            TILE_FULL_FRAMES.fetch_add(1, Ordering::Relaxed);
        }
    }
    let now_sec = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    TILE_SECOND.with(|cell| {
        let (sec, kpx, frames, fulls) = cell.get();
        if sec != now_sec {
            if sec != 0 && frames > 0 {
                push_second(sec, kpx, frames, fulls);
            }
            cell.set((now_sec, px / 1000, 1, full as u64));
        } else {
            cell.set((sec, kpx + px / 1000, frames + 1, fulls + full as u64));
        }
    });
}

/// 한 렌더러 스레드의 한 초 합을 링에 넣는다: (초, kpx << 32 | 프레임 << 16 | 큰 래스터 프레임).
fn push_second(sec: u64, kpx: u64, frames: u64, fulls: u64) {
    let b = (kpx.min(u32::MAX as u64) << 32) | (frames.min(0xffff) << 16) | fulls.min(0xffff);
    let slot = (SECOND_NEXT.fetch_add(1, Ordering::AcqRel) as usize) % SECOND_RING;
    SECOND_B[slot].store(b, Ordering::Relaxed);
    SECOND_A[slot].store(sec, Ordering::Release);
}

#[derive(Default)]
struct TileReport {
    frames: u64,
    tiles: u64,
    px: u64,
    fulls: u64,
    seen: u64,
}

/// `TILEPROBE`(창 요약)과 `TILESEC`(초별, painter 넷 합)를 찍는다.
fn report_tiles(elapsed: Duration, state: &mut TileReport) {
    let frames = TILE_FRAMES.load(Ordering::Relaxed);
    let tiles = TILES.load(Ordering::Relaxed);
    let px = TILE_PX.load(Ordering::Relaxed);
    let fulls = TILE_FULL_FRAMES.load(Ordering::Relaxed);
    let max_tiles = TILE_FRAME_MAX_TILES.swap(0, Ordering::Relaxed);
    let max_px = TILE_FRAME_MAX_PX.swap(0, Ordering::Relaxed);
    warn!(
        "TILEPROBE t={:.0}s w:frames_with_tiles={} w:tiles={} w:raster={:.0}Mpx w:full_frames(>={}Mpx)={} \
         w:frame_max_tiles={} w:frame_max_raster={:.1}Mpx | total raster={:.0}Mpx full_frames={}",
        elapsed.as_secs_f64(),
        frames - state.frames,
        tiles - state.tiles,
        (px - state.px) as f64 / 1e6,
        FULL_RASTER_PX / 1_000_000,
        fulls - state.fulls,
        max_tiles,
        max_px as f64 / 1e6,
        px as f64 / 1e6,
        fulls,
    );
    let next = SECOND_NEXT.load(Ordering::Acquire);
    let first = state.seen.max(next.saturating_sub(SECOND_RING as u64));
    let mut by_second: Vec<(u64, u64, u64, u64, u64)> = Vec::new();
    for index in first..next {
        let slot = (index as usize) % SECOND_RING;
        let sec = SECOND_A[slot].load(Ordering::Acquire);
        let b = SECOND_B[slot].load(Ordering::Relaxed);
        let (kpx, frames, fulls) = (b >> 32, (b >> 16) & 0xffff, b & 0xffff);
        match by_second.iter_mut().find(|row| row.0 == sec) {
            Some(row) => {
                row.1 += kpx;
                row.2 += frames;
                row.3 += fulls;
                row.4 += 1;
            },
            None => by_second.push((sec, kpx, frames, fulls, 1)),
        }
    }
    by_second.sort_by_key(|row| row.0);
    for (sec, kpx, frames, fulls, threads) in by_second {
        let (h, m, s, _) = crate::wall_tex_cache::utc_hms(sec * 1000);
        warn!(
            "TILESEC at={h:02}:{m:02}:{s:02}Z raster={:.0}Mpx frames={frames} full={fulls} painters={threads}",
            kpx as f64 / 1000.0
        );
    }
    state.frames = frames;
    state.tiles = tiles;
    state.px = px;
    state.fulls = fulls;
    state.seen = next;
}

#[derive(Clone, Copy, Default)]
struct Snapshot {
    shared: u64,
    dedicated: u64,
    misses: u64,
    miss_bytes: u64,
    reuse_larger: u64,
    superseded: u64,
    superseded_bytes: u64,
    cold: u64,
    spared: u64,
    gc_frees: u64,
    gc_bytes: u64,
    creates: u64,
    create_ns: u64,
    deletes: u64,
    delete_ns: u64,
    limited: u64,
}

impl Snapshot {
    fn take() -> Self {
        Snapshot {
            shared: SHARED_REQUESTS.load(Ordering::Relaxed),
            dedicated: DEDICATED_REQUESTS.load(Ordering::Relaxed),
            misses: POOL_MISSES.load(Ordering::Relaxed),
            miss_bytes: POOL_MISS_BYTES.load(Ordering::Relaxed),
            reuse_larger: REUSE_LARGER.load(Ordering::Relaxed),
            superseded: SUPERSEDED.load(Ordering::Relaxed),
            superseded_bytes: SUPERSEDED_BYTES.load(Ordering::Relaxed),
            cold: COLD_MISSES.load(Ordering::Relaxed),
            spared: GC_SPARED.load(Ordering::Relaxed),
            gc_frees: GC_FREES.load(Ordering::Relaxed),
            gc_bytes: GC_FREE_BYTES.load(Ordering::Relaxed),
            creates: RT_CREATES.load(Ordering::Relaxed),
            create_ns: RT_CREATE_NS.load(Ordering::Relaxed),
            deletes: RT_DELETES.load(Ordering::Relaxed),
            delete_ns: RT_DELETE_NS.load(Ordering::Relaxed),
            limited: SURFACE_LIMITED.load(Ordering::Relaxed),
        }
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn ms(nanos: u64) -> f64 {
    nanos as f64 / 1e6
}

/// ★`warn!` 로 찍는 이유: 벽 런이 `RUST_LOG=warn` 이다★ -- `paint_animation_mesh_probe`
/// 가 `info!` 로 냈다가 첫 실기 런에서 한 줄도 남지 않았다(2026-10-30).
///
/// `w:` 는 직전 창(10 초)의 차분이고, 나머지는 런 시작부터의 누적이다. 최대값과 최소 배율은
/// 누적만 있다.
fn report(elapsed: Duration, now: &Snapshot, previous: &Snapshot) {
    let min_milli = SURFACE_LIMITED_MIN_MILLI.load(Ordering::Relaxed);
    let min_scale = if min_milli == u64::MAX {
        String::from("-")
    } else {
        format!("{:.3}", min_milli as f64 / 1000.0)
    };
    warn!(
        "RTPROBE t={:.0}s w:dedicated={} w:miss={} ({:.0}MB) w:cold={} w:reuse_larger={} \
         w:superseded={} ({:.0}MB) w:spared={} w:gc_free={} ({:.0}MB) \
         w:create={} ({:.1}ms) w:delete={} ({:.1}ms) w:limited={} | \
         total shared={} dedicated={} max_dim={} miss={} ({:.0}MB) gc_free={} ({:.0}MB) \
         create={} ({:.1}ms, max {:.2}ms) delete={} ({:.1}ms, max {:.2}ms) \
         limited={} min_scale={}",
        elapsed.as_secs_f64(),
        now.dedicated - previous.dedicated,
        now.misses - previous.misses,
        mib(now.miss_bytes - previous.miss_bytes),
        now.cold - previous.cold,
        now.reuse_larger - previous.reuse_larger,
        now.superseded - previous.superseded,
        mib(now.superseded_bytes - previous.superseded_bytes),
        now.spared - previous.spared,
        now.gc_frees - previous.gc_frees,
        mib(now.gc_bytes - previous.gc_bytes),
        now.creates - previous.creates,
        ms(now.create_ns - previous.create_ns),
        now.deletes - previous.deletes,
        ms(now.delete_ns - previous.delete_ns),
        now.limited - previous.limited,
        now.shared,
        now.dedicated,
        DEDICATED_MAX_DIM.load(Ordering::Relaxed),
        now.misses,
        mib(now.miss_bytes),
        now.gc_frees,
        mib(now.gc_bytes),
        now.creates,
        ms(now.create_ns),
        ms(RT_CREATE_MAX_NS.load(Ordering::Relaxed)),
        now.deletes,
        ms(now.delete_ns),
        ms(RT_DELETE_MAX_NS.load(Ordering::Relaxed)),
        now.limited,
        min_scale,
    );
}

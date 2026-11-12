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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

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
            loop {
                std::thread::sleep(REPORT_EVERY);
                let now = Snapshot::take();
                report(started.elapsed(), &now, &previous);
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

#[derive(Clone, Copy, Default)]
struct Snapshot {
    shared: u64,
    dedicated: u64,
    misses: u64,
    miss_bytes: u64,
    reuse_larger: u64,
    superseded: u64,
    superseded_bytes: u64,
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
        "RTPROBE t={:.0}s w:dedicated={} w:miss={} ({:.0}MB) w:reuse_larger={} \
         w:superseded={} ({:.0}MB) w:gc_free={} ({:.0}MB) \
         w:create={} ({:.1}ms) w:delete={} ({:.1}ms) w:limited={} | \
         total shared={} dedicated={} max_dim={} miss={} ({:.0}MB) gc_free={} ({:.0}MB) \
         create={} ({:.1}ms, max {:.2}ms) delete={} ({:.1}ms, max {:.2}ms) \
         limited={} min_scale={}",
        elapsed.as_secs_f64(),
        now.dedicated - previous.dedicated,
        now.misses - previous.misses,
        mib(now.miss_bytes - previous.miss_bytes),
        now.reuse_larger - previous.reuse_larger,
        now.superseded - previous.superseded,
        mib(now.superseded_bytes - previous.superseded_bytes),
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

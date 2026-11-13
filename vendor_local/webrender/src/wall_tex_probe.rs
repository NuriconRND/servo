/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! WR 텍스처 캐시의 ★업로드·할당·내보내기★ 를 센다 (`TEXPROBE`, `TEXHEAVY`).
//!
//! ## 왜 (2026-11-13 추가)
//!
//! 이미지와 비디오에 같은 회전 + opacity 애니메이션을 걸어 교대로 전환하면 이미지 쪽 전환에서
//! judder 가 더 많고, 이미지를 4096 이하로 줄여도 등장/퇴장 애니메이션에서는 남는다(운영자,
//! 2026-11-13). 코드로 본 가설은 `wall_tex_cache.rs` 머리 주석에 있다 -- 요지는 "이미지는 숨겨
//! 미리 마운트돼도 GPU 에 안 올라가고, 화면에서 빠지면 작은 예산 때문에 곧 내보내져서, 등장
//! 첫 프레임마다 painter 넷이 다시 올린다" 이다. ***지금 로그에는 텍스처 업로드를 세는 줄이
//! 없다*** -- RTPROBE 는 렌더 타깃만 센다. 이 프로브가 그것을 잰다.
//!
//! ## 무엇을 찍는가
//!
//! - `TEXPROBE` (10 초마다): 업로드가 있던 프레임 수, 업로드 바이트와 시간(렌더러가 원래 재던
//!   `upload_total`), 그 프레임에서 새로 만든 텍스처의 생성 시간, 무거운 프레임 수, 텍스처 캐시
//!   할당과 내보내기(standalone / 공유 아틀라스). `w:` 는 직전 창의 차분, 나머지는 누적이다.
//! - `TEXHEAVY` (무거운 프레임마다 한 줄, 다음 보고 때 몰아서): UTC 시각(로그 줄의 `...Z` 와 같은
//!   시계), 업로드 크기와 시간. ★등장/퇴장이 9 초 장면 주기에 묶여 있으므로★ 이 시각을
//!   `ANIMSCRIPTSTART` 줄과 맞추면 어느 전환에서 무엇이 올라갔는지 보인다. 무거운 프레임의 기준은
//!   업로드 [`HEAVY_BYTES`] 이상 또는 업로드+생성 [`HEAVY_NANOS`] 이상이다.
//!
//! ## 성능 규칙 (타이밍 경로에 I/O 금지)
//!
//! 쌓는 쪽은 원자 연산만 한다. 시간은 렌더러가 원래 재던 값을 받기만 하고 새로 재지 않는다.
//! 무거운 프레임만 벽시계를 한 번 읽고 링에 넣는다(드물다). 출력은 전용 리포터 스레드가 한다.
//! ★꺼져 있으면 호출 자리마다 `AtomicBool` 로드 하나다.★ 링이 한 창에 [`RING`] 개를 넘게
//! 받으면 오래된 것부터 덮어쓰고, 리포터는 덮인 수를 `lost=` 로 찍는다.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::wall_tex_cache::{pack_heavy, unpack_heavy, utc_hms};

const REPORT_EVERY: Duration = Duration::from_secs(10);

/// 한 프레임 업로드가 이 이상이면 무거운 프레임이다(4 MiB = 1024x1024 RGBA 한 장).
pub const HEAVY_BYTES: u64 = 4 * 1024 * 1024;
/// 업로드 + 생성 시간이 이 이상이어도 무거운 프레임이다(60 Hz 한 주기의 1/4).
pub const HEAVY_NANOS: u64 = 4_000_000;

/// 무거운 프레임 링의 칸 수.
const RING: usize = 128;

static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

static UPLOAD_FRAMES: AtomicU64 = AtomicU64::new(0);
static UPLOAD_BYTES: AtomicU64 = AtomicU64::new(0);
static UPLOAD_NS: AtomicU64 = AtomicU64::new(0);
static CREATE_NS: AtomicU64 = AtomicU64::new(0);
static FRAME_MAX_BYTES: AtomicU64 = AtomicU64::new(0);
static FRAME_MAX_NS: AtomicU64 = AtomicU64::new(0);
static HEAVY_FRAMES: AtomicU64 = AtomicU64::new(0);

static ALLOC_STANDALONE: AtomicU64 = AtomicU64::new(0);
static ALLOC_STANDALONE_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOC_SHARED: AtomicU64 = AtomicU64::new(0);
static ALLOC_SHARED_BYTES: AtomicU64 = AtomicU64::new(0);
static EVICT_STANDALONE: AtomicU64 = AtomicU64::new(0);
static EVICT_STANDALONE_BYTES: AtomicU64 = AtomicU64::new(0);
static EVICT_SHARED: AtomicU64 = AtomicU64::new(0);
static EVICT_SHARED_BYTES: AtomicU64 = AtomicU64::new(0);

static RING_A: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];
static RING_B: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];
static RING_NEXT: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 프로브를 켜고 리포터 스레드를 띄운다. ★여러 painter 가 불러도 한 번만 띄운다.★
/// 수치는 painter(GPU)마다 하나씩인 WebRender 전부의 합이다.
pub fn start() {
    if STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    ENABLED.store(true, Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name(String::from("WrTexProbe"))
        .spawn(|| {
            let started = std::time::Instant::now();
            let mut previous = Snapshot::default();
            let mut ring_seen = 0u64;
            loop {
                std::thread::sleep(REPORT_EVERY);
                let now = Snapshot::take();
                report(started.elapsed(), &now, &previous);
                ring_seen = report_heavy(ring_seen);
                previous = now;
            }
        });
    if let Err(error) = spawned {
        warn!("TEXPROBE: 리포터 스레드를 띄우지 못했다: {error}; 요약이 나오지 않는다");
    }
}

/// 렌더러가 한 프레임의 텍스처 캐시 갱신을 마쳤다. 바이트와 시간은 렌더러가 이미 잰 값이다.
#[inline]
pub fn note_frame(upload_bytes: u64, upload_ns: u64, create_ns: u64) {
    if !is_enabled() || (upload_bytes == 0 && create_ns == 0) {
        return;
    }
    UPLOAD_FRAMES.fetch_add(1, Ordering::Relaxed);
    UPLOAD_BYTES.fetch_add(upload_bytes, Ordering::Relaxed);
    UPLOAD_NS.fetch_add(upload_ns, Ordering::Relaxed);
    CREATE_NS.fetch_add(create_ns, Ordering::Relaxed);
    FRAME_MAX_BYTES.fetch_max(upload_bytes, Ordering::Relaxed);
    let nanos = upload_ns + create_ns;
    FRAME_MAX_NS.fetch_max(nanos, Ordering::Relaxed);
    if upload_bytes >= HEAVY_BYTES || nanos >= HEAVY_NANOS {
        HEAVY_FRAMES.fetch_add(1, Ordering::Relaxed);
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let (a, b) = pack_heavy(unix_ms, upload_bytes, nanos);
        let slot = (RING_NEXT.fetch_add(1, Ordering::AcqRel) as usize) % RING;
        RING_B[slot].store(b, Ordering::Relaxed);
        RING_A[slot].store(a, Ordering::Release);
    }
}

/// 텍스처 캐시가 항목 하나를 할당했다.
#[inline]
pub fn note_alloc(shared: bool, bytes: u64) {
    if !is_enabled() {
        return;
    }
    if shared {
        ALLOC_SHARED.fetch_add(1, Ordering::Relaxed);
        ALLOC_SHARED_BYTES.fetch_add(bytes, Ordering::Relaxed);
    } else {
        ALLOC_STANDALONE.fetch_add(1, Ordering::Relaxed);
        ALLOC_STANDALONE_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// 텍스처 캐시가 항목 하나를 내보냈다(자동 내보내기만 -- 명시적 삭제는 세지 않는다).
#[inline]
pub fn note_evict(shared: bool, bytes: u64) {
    if !is_enabled() {
        return;
    }
    if shared {
        EVICT_SHARED.fetch_add(1, Ordering::Relaxed);
        EVICT_SHARED_BYTES.fetch_add(bytes, Ordering::Relaxed);
    } else {
        EVICT_STANDALONE.fetch_add(1, Ordering::Relaxed);
        EVICT_STANDALONE_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Default)]
struct Snapshot {
    upload_frames: u64,
    upload_bytes: u64,
    upload_ns: u64,
    create_ns: u64,
    heavy: u64,
    alloc_standalone: u64,
    alloc_standalone_bytes: u64,
    alloc_shared: u64,
    alloc_shared_bytes: u64,
    evict_standalone: u64,
    evict_standalone_bytes: u64,
    evict_shared: u64,
    evict_shared_bytes: u64,
}

impl Snapshot {
    fn take() -> Self {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Snapshot {
            upload_frames: l(&UPLOAD_FRAMES),
            upload_bytes: l(&UPLOAD_BYTES),
            upload_ns: l(&UPLOAD_NS),
            create_ns: l(&CREATE_NS),
            heavy: l(&HEAVY_FRAMES),
            alloc_standalone: l(&ALLOC_STANDALONE),
            alloc_standalone_bytes: l(&ALLOC_STANDALONE_BYTES),
            alloc_shared: l(&ALLOC_SHARED),
            alloc_shared_bytes: l(&ALLOC_SHARED_BYTES),
            evict_standalone: l(&EVICT_STANDALONE),
            evict_standalone_bytes: l(&EVICT_STANDALONE_BYTES),
            evict_shared: l(&EVICT_SHARED),
            evict_shared_bytes: l(&EVICT_SHARED_BYTES),
        }
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn ms(nanos: u64) -> f64 {
    nanos as f64 / 1e6
}

/// ★`warn!` 로 찍는 이유: 벽 런이 `RUST_LOG=warn` 이다★ (RTPROBE 와 같다).
fn report(elapsed: Duration, now: &Snapshot, previous: &Snapshot) {
    let d = |f: fn(&Snapshot) -> u64| f(now) - f(previous);
    warn!(
        "TEXPROBE t={:.0}s w:upload_frames={} w:upload={:.1}MB ({:.1}ms) w:create={:.1}ms \
         w:heavy={} w:alloc_standalone={} ({:.1}MB) w:alloc_shared={} ({:.1}MB) \
         w:evict_standalone={} ({:.1}MB) w:evict_shared={} ({:.1}MB) | \
         total upload={:.0}MB heavy={} frame_max={:.1}MB {:.2}ms",
        elapsed.as_secs_f64(),
        d(|s| s.upload_frames),
        mib(d(|s| s.upload_bytes)),
        ms(d(|s| s.upload_ns)),
        ms(d(|s| s.create_ns)),
        d(|s| s.heavy),
        d(|s| s.alloc_standalone),
        mib(d(|s| s.alloc_standalone_bytes)),
        d(|s| s.alloc_shared),
        mib(d(|s| s.alloc_shared_bytes)),
        d(|s| s.evict_standalone),
        mib(d(|s| s.evict_standalone_bytes)),
        d(|s| s.evict_shared),
        mib(d(|s| s.evict_shared_bytes)),
        mib(now.upload_bytes),
        now.heavy,
        mib(FRAME_MAX_BYTES.load(Ordering::Relaxed)),
        ms(FRAME_MAX_NS.load(Ordering::Relaxed)),
    );
}

/// 지난 보고 뒤로 링에 들어온 무거운 프레임을 시각 순서대로 찍는다. 다음에 읽을 순번을 돌려준다.
fn report_heavy(seen: u64) -> u64 {
    let next = RING_NEXT.load(Ordering::Acquire);
    let first = seen.max(next.saturating_sub(RING as u64));
    let lost = first - seen;
    for index in first..next {
        let slot = (index as usize) % RING;
        let a = RING_A[slot].load(Ordering::Acquire);
        let b = RING_B[slot].load(Ordering::Relaxed);
        let (unix_ms, kib, micros) = unpack_heavy(a, b);
        let (h, m, s, millis) = utc_hms(unix_ms);
        warn!(
            "TEXHEAVY at={:02}:{:02}:{:02}.{:03}Z upload={:.1}MB upload+create={:.2}ms",
            h,
            m,
            s,
            millis,
            kib as f64 / 1024.0,
            micros as f64 / 1000.0,
        );
    }
    if lost > 0 {
        warn!("TEXHEAVY lost={lost} (링 {RING} 칸을 넘어 덮였다)");
    }
    next
}

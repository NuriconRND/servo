/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 드문 무거운 일에 ★UTC ms 시각과 소요 시간★ 을 찍는다 (`WALLEVENT`, `WALLEVENTS`).
//!
//! ## 왜 (2026-11-16 추가)
//!
//! 벽 런 log_rotation_opacity_judder_01/07(2026-11-16, `db223a1e1f4`, 보드 교체
//! `bounce/left/2500ms` 로 이미지 <-> 비디오, 영역 애니메이션 없음): 전환 2.5 초 동안 painter
//! 마다 화면 전체(12.4 Mpx)를 거의 매 프레임 다시 그리는데(`TILESEC` 1800~2900 Mpx/s), 16.7 ms
//! 를 넘긴 렌더는 ★다시 그리는 양이 오히려 줄어드는 바운스 후반(2~3 초)에 가장 많다★(전환당
//! 14.0 대 1~2 초 7.3). 운영자도 "바운스 후반에서 더 도드라진다" 고 봤다. 그 구간에 그리기 말고
//! 다른 일이 겹친다는 뜻이고, 후보는 나가는 보드를 치우는 일이다 -- 출력 페이지는 그 시각을
//! 남기지 않는다(`useBoardHandover.ts`).
//!
//! 그래서 후보가 되는 일들에 시각을 찍는다. 같은 시계(`SystemTime`, UTC)로 찍히므로 `TILESEC`,
//! `MESHJUMP`, `[page] [+N.Ns]` 와 맞춰 볼 수 있다.
//!
//! - `media.create` / `media.release` -- 스크립트 스레드에서 미디어 플레이어를 만들고 놓는 일
//!   (문서에서 빠진 미디어는 그 자리에서 놓는다, `release_player_while_detached`).
//! - `media.dispose` -- ★실제 GStreamer 해체★. 위의 release 는 `PlayerInner` 를 전용 스레드
//!   (`GstPlayerDisposal{N}`)로 넘기기만 하고, 파이프라인을 내리는 일은 거기서 한다(파일 비디오
//!   평균 28 ms -- `GStreamerPlayer` 의 `Drop` 주석). extra = 0 Stream, 1 Seekable, 2 NetworkUri.
//! - `dcomp.create_surface` / `dcomp.destroy_surface` -- WebRender 의 picture cache 슬라이스마다
//!   하나인 합성 surface. 보드 교체 중 painter 당 타일이 두 장까지 늘었다(`TILEPROBE`
//!   frame_max 24.9 Mpx) -- 슬라이스가 생기고 사라지면 이것이 찍힌다.
//! - `render.over` (2026-11-16 추가) -- 한 주기(16.667 ms)를 넘긴 painter 렌더. `RENDERSPLIT` 과
//!   같은 범위(present 포함)를 재지만 ms 시각과 ★타일 원점★(`tile=x,y`, 곧 어느 GPU 인지)을 남긴다.
//!   08 런에서 바운스 후반 과부하가 +2.15~2.45 초로 좁혀졌는데(나가는 보드 정리 +2.55 초보다 앞),
//!   `RENDERSPLIT` 은 로그 초 해상도이고 painter 를 남기지 않아 그 이상 가를 수 없었다.
//! - `dcomp.promote` / `dcomp.demote` / `dcomp.regen` -- 그 surface 를 swap chain 으로 올리고
//!   내리는 일, 그리고 swap chain 크기가 바뀌어 다시 만드는 일. extra 는 kpx.
//!
//! ## 왜 servo_config 에 있나
//!
//! 찍는 자리가 script(미디어 요소), paint(DComp), 미디어 백엔드(해체 스레드) 세 crate 에 걸친다.
//! 셋이 모두 의존하는 가장 가벼운 crate 가 이것이다 -- paint_api 에 두면 미디어 백엔드가
//! WebRender API 까지 끌어와야 한다.
//!
//! ## 성능 규칙
//!
//! 모두 드문 일이라 앞뒤로 `Instant` 를 읽고 원자 링([`RING`] 칸)에 넣는다. `at=` 은 ★끝난 시각★이다. 출력은 painter 가 띄운
//! 리포터 스레드가 10 초마다 한다(창마다 최대 [`PRINT_MAX`] 줄, 넘친 수는 `more=`).
//! ★꺼져 있으면 각 자리에 `AtomicBool` 로드 하나다★ -- 시계도 읽지 않는다([`Timer`]).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const REPORT_EVERY: Duration = Duration::from_secs(10);
/// 2026-11-16: `render.over` 가 들어오며 256 -> 1024, 120 -> 400 (전환 하나에 과부하 렌더가 20~30,
/// 나쁜 창이면 그 몇 배). 출력은 리포터 스레드가 하므로 렌더 경로 비용과 무관하다.
const RING: usize = 1024;
const PRINT_MAX: u64 = 400;

static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static RING_A: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];
static RING_B: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];
static NEXT: AtomicU64 = AtomicU64::new(0);

/// 찍는 일의 종류. 값은 링에 담기는 코드다 -- 바꾸면 [`Kind::name`] 도 바꾼다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    MediaCreate = 1,
    MediaRelease = 2,
    DcompCreateSurface = 3,
    DcompDestroySurface = 4,
    DcompPromote = 5,
    DcompDemote = 6,
    /// 이미 swap chain 인 surface 의 크기가 바뀌어 다시 만든다.
    DcompRegen = 7,
    MediaDispose = 8,
    /// extra = 타일 원점 `(x << 16) | y` ([`tile_code`]).
    RenderOver = 9,
}

/// 코드 0 은 비워 둔다(빈 칸과 구별). 코드 최댓값 + 1.
const KINDS: usize = 10;

/// `WALLEVENTS` 에 찍는 순서: 미디어(생성 -> 놓기 -> 해체), 그다음 DComp.
const ORDER: [Kind; KINDS - 1] = [
    Kind::MediaCreate,
    Kind::MediaRelease,
    Kind::MediaDispose,
    Kind::DcompCreateSurface,
    Kind::DcompDestroySurface,
    Kind::DcompPromote,
    Kind::DcompDemote,
    Kind::DcompRegen,
    Kind::RenderOver,
];

impl Kind {
    fn from_code(code: u64) -> Option<Kind> {
        Some(match code {
            1 => Kind::MediaCreate,
            2 => Kind::MediaRelease,
            3 => Kind::DcompCreateSurface,
            4 => Kind::DcompDestroySurface,
            5 => Kind::DcompPromote,
            6 => Kind::DcompDemote,
            7 => Kind::DcompRegen,
            8 => Kind::MediaDispose,
            9 => Kind::RenderOver,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::MediaCreate => "media.create",
            Kind::MediaRelease => "media.release",
            Kind::DcompCreateSurface => "dcomp.create_surface",
            Kind::DcompDestroySurface => "dcomp.destroy_surface",
            Kind::DcompPromote => "dcomp.promote",
            Kind::DcompDemote => "dcomp.demote",
            Kind::DcompRegen => "dcomp.regen",
            Kind::MediaDispose => "media.dispose",
            Kind::RenderOver => "render.over",
        }
    }
}

#[inline]
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 일 하나의 시작을 잡는다. 꺼져 있으면 시계를 읽지 않는다.
pub struct Timer(Option<Instant>);

impl Timer {
    #[inline]
    pub fn start() -> Timer {
        Timer(is_enabled().then(Instant::now))
    }

    /// 끝났다. `extra` 는 종류마다 뜻이 다르다(surface 면적 kpx 등, 없으면 0).
    #[inline]
    pub fn finish(self, kind: Kind, extra: u32) {
        if let Some(started) = self.0 {
            record(kind, started.elapsed(), extra);
        }
    }

    /// `min` 이상 걸렸을 때만 찍는다. `extra` 는 그때만 계산한다(자물쇠를 잡는 값이어도 된다).
    #[inline]
    pub fn finish_if_longer(self, kind: Kind, min: Duration, extra: impl FnOnce() -> u32) {
        if let Some(started) = self.0 {
            let took = started.elapsed();
            if took >= min {
                record(kind, took, extra());
            }
        }
    }
}

/// 타일 원점을 extra 하나에 담는다(2026-11-16). 벽 가상 뷰포트는 11520x4320 이라 16 비트씩이면 된다.
pub fn tile_code(x: i32, y: i32) -> u32 {
    ((x.clamp(0, 0xffff) as u32) << 16) | y.clamp(0, 0xffff) as u32
}

/// `WALLEVENT` 줄의 꼬리. 대부분 `extra=N`, `render.over` 는 `tile=x,y`.
fn describe_extra(kind: Kind, extra: u32) -> String {
    match kind {
        Kind::RenderOver => format!("tile={},{}", extra >> 16, extra & 0xffff),
        _ => format!("extra={extra}"),
    }
}

/// 링에 넣는다: (UTC ms, 종류 << 56 | µs(24 비트) << 32 | extra).
pub fn record(kind: Kind, took: Duration, extra: u32) {
    if !is_enabled() {
        return;
    }
    let (a, b) = pack(unix_ms(), kind, took, extra);
    let slot = (NEXT.fetch_add(1, Ordering::AcqRel) as usize) % RING;
    RING_B[slot].store(b, Ordering::Relaxed);
    RING_A[slot].store(a, Ordering::Release);
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn pack(unix_ms: u64, kind: Kind, took: Duration, extra: u32) -> (u64, u64) {
    let micros = (took.as_micros() as u64).min((1 << 24) - 1);
    (
        unix_ms,
        ((kind as u64) << 56) | (micros << 32) | extra as u64,
    )
}

fn unpack(b: u64) -> (Option<Kind>, u64, u32) {
    (
        Kind::from_code(b >> 56),
        (b >> 32) & ((1 << 24) - 1),
        (b & 0xffff_ffff) as u32,
    )
}

/// UTC (시, 분, 초, ms). 로그 줄의 `...Z` 와 같은 시계다.
fn utc_hms(unix_ms: u64) -> (u64, u64, u64, u64) {
    let secs = (unix_ms / 1000) % 86_400;
    (secs / 3600, (secs / 60) % 60, secs % 60, unix_ms % 1000)
}

/// 켜고 리포터를 띄운다. ★여러 painter 가 불러도 한 번만.★
pub fn start() {
    if STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    ENABLED.store(true, Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name(String::from("WallEvent"))
        .spawn(|| {
            let started = Instant::now();
            let mut seen = 0u64;
            loop {
                std::thread::sleep(REPORT_EVERY);
                seen = report(started.elapsed(), seen);
            }
        });
    if let Err(error) = spawned {
        log::warn!("WALLEVENT: 리포터 스레드를 띄우지 못했다: {error}");
    }
}

/// ★`warn!` 로 찍는 이유: 벽 런이 `RUST_LOG=warn` 이다.★
fn report(elapsed: Duration, seen: u64) -> u64 {
    let next = NEXT.load(Ordering::Acquire);
    let first = seen.max(next.saturating_sub(RING as u64));
    let lost = first - seen;
    let mut window = [0u64; KINDS];
    let mut printed = 0u64;
    for index in first..next {
        let slot = (index as usize) % RING;
        let a = RING_A[slot].load(Ordering::Acquire);
        let b = RING_B[slot].load(Ordering::Relaxed);
        let (kind, micros, extra) = unpack(b);
        let Some(kind) = kind else { continue };
        window[kind as usize] += 1;
        if printed < PRINT_MAX {
            let (h, m, s, ms) = utc_hms(a);
            log::warn!(
                "WALLEVENT at={h:02}:{m:02}:{s:02}.{ms:03}Z {} took={:.2}ms {}",
                kind.name(),
                micros as f64 / 1000.0,
                describe_extra(kind, extra),
            );
            printed += 1;
        }
    }
    let shown = next - first;
    if shown > printed || lost > 0 {
        log::warn!(
            "WALLEVENT more={} (창마다 {PRINT_MAX} 줄까지, 링 {RING} 칸에서 덮인 {lost} 포함)",
            shown - printed + lost
        );
    }
    if shown > 0 {
        log::warn!("{}", summary_line(elapsed.as_secs_f64(), &window));
    }
    next
}

/// `WALLEVENTS` 한 줄. ★한 줄이어야 한다★ -- 분석 스크립트가 줄 단위로 읽는다(2026-11-16, 형식
/// 문자열의 줄 잇기를 잘못 써서 두 줄로 찍힌 적이 있어 시험으로 묶었다).
fn summary_line(elapsed_secs: f64, window: &[u64; KINDS]) -> String {
    let mut line = format!("WALLEVENTS t={elapsed_secs:.0}s");
    for kind in ORDER {
        line.push_str(&format!(" w:{}={}", kind.name(), window[kind as usize]));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_round_trips_through_the_ring_encoding() {
        let (a, b) = pack(
            1_794_458_286_250,
            Kind::DcompPromote,
            Duration::from_micros(18_345),
            12_441,
        );
        assert_eq!(a, 1_794_458_286_250);
        assert_eq!(unpack(b), (Some(Kind::DcompPromote), 18_345, 12_441));
    }

    #[test]
    fn a_huge_duration_saturates_instead_of_corrupting_the_kind() {
        let (_, b) = pack(0, Kind::MediaRelease, Duration::from_secs(3600), u32::MAX);
        assert_eq!(
            unpack(b),
            (Some(Kind::MediaRelease), (1 << 24) - 1, u32::MAX)
        );
    }

    #[test]
    fn every_kind_has_a_code_and_a_name() {
        for kind in ORDER {
            assert_eq!(Kind::from_code(kind as u64), Some(kind));
            assert!((kind as usize) < KINDS);
            assert!(kind.name().contains('.'));
        }
        assert_eq!(Kind::from_code(0), None);
        assert_eq!(Kind::from_code(KINDS as u64), None);
    }

    #[test]
    fn the_summary_is_one_line_naming_every_kind() {
        let mut window = [0u64; KINDS];
        for kind in ORDER {
            window[kind as usize] = kind as u64 * 10;
        }
        let line = summary_line(12.4, &window);
        assert!(!line.contains('\n'), "{line}");
        assert!(line.starts_with("WALLEVENTS t=12s "), "{line}");
        for kind in ORDER {
            let field = format!(" w:{}={}", kind.name(), kind as u64 * 10);
            assert!(line.contains(&field), "{line}");
        }
    }

    #[test]
    fn utc_clock_matches_the_log_timestamps() {
        // 2026-11-12T04:38:06.250Z (python 으로 확인한 값)
        assert_eq!(utc_hms(1_794_458_286_250), (4, 38, 6, 250));
    }

    #[test]
    fn a_disabled_timer_reads_no_clock() {
        // 시험 프로세스에서는 start() 를 부르지 않으므로 꺼져 있다.
        assert!(Timer::start().0.is_none());
    }

    #[test]
    fn a_disabled_conditional_timer_never_computes_extra() {
        // 꺼져 있으면 extra 클로저(타일 자물쇠)를 부르지 않아야 한다.
        Timer::start().finish_if_longer(Kind::RenderOver, Duration::ZERO, || {
            panic!("extra computed while disabled")
        });
    }

    #[test]
    fn tile_origins_round_trip_for_the_whole_wall() {
        // 2x2 벽(11520x4320)의 네 원점과, 범위를 벗어난 값의 자르기.
        for (x, y) in [(0, 0), (5760, 0), (0, 2160), (5760, 2160)] {
            assert_eq!(
                describe_extra(Kind::RenderOver, tile_code(x, y)),
                format!("tile={x},{y}")
            );
        }
        assert_eq!(
            describe_extra(Kind::RenderOver, tile_code(-5, 70_000)),
            "tile=0,65535"
        );
        assert_eq!(describe_extra(Kind::MediaDispose, 1), "extra=1");
    }
}

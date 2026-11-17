/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 벽 패스의 구간을 스레드마다 링에 쌓고, 리포터 스레드가 접어서 찍는다 (`WALLSPAN` 계열).
//!
//! ## 왜 (2026-11-17)
//!
//! `-DetachTileRender` 와 `-DcompParallelCommit` 는 추가된 뒤로 런타임에서 의도대로 동작하는지
//! 확인된 적이 없다. 로그 log_rotation_opacity_judder_01/11(2026-11-16, a8d124b9147)에서 둘 다
//! painter 넷의 결합을 풀지 못했다:
//! - 분리 모드에서도 패스 끝 flush 가 painter 마다 블로킹 왕복을 해서, 다른 painter 의 과부하가
//!   끝나기 전에 시작한 과부하가 0/351 이었다.
//! - 병렬 Commit 은 메인이 Commit 넷을 다 기다려서, 결합 지점이 Commit join 으로 옮겨졌을 뿐이다.
//!
//! 그리고 Commit 은 어디서도 시간을 잰 적이 없다. 이 모듈은 두 플래그를 의도대로 완성하면서
//! "정말 분리됐나", "정말 겹쳐 나갔나", "Commit 이 무엇을 기다리나" 를 수치로 판정하려는 것이다.
//!
//! ## 기록 규칙 (no-io-on-timing-critical-threads)
//!
//! - 기록하는 스레드(메인, painter, 커밋 워커)는 ★자기 링에만★ 쓴다. 링은 스레드가 처음 기록할 때
//!   한 번 차지하고(원자 증가 하나) 그 뒤로는 그 스레드 혼자 쓴다. 기록 한 번은 QPC 읽기와 원자
//!   저장 넷이고 락·할당·syscall 이 없다(QPC 는 사용자 모드 `rdtsc` 계열이다).
//! - 링이 차면 ★새 기록을 버리고★ `lost` 를 올린다. 덮어쓰지 않는다. `lost > 0` 인 창은 판정에
//!   쓰지 않는다.
//! - 리포터(`WallSpan` 스레드)가 250 ms 마다 링을 비우고 10 초마다 창 값을 `warn!` 한 줄씩 찍는다.
//!   DWM 합성 격자(`DwmGetCompositionTimingInfo`)도 리포터만 묻는다.
//! - 기록 비용 자체를 잰다: 링마다 1024 번째 기록을 QPC 로 감싸 그 시간을 쌓고, 리포터가 창마다
//!   평균과 최댓값을 찍는다(`rec_ns`).
//! - 꺼져 있으면 각 자리에서 `AtomicBool` 로드 하나다(`now()` 가 `None`).
//!
//! ## 줄 읽는 법
//!
//! - `WALLDETACH xpass=a/b/..` -- painter 별로, ★다른 painter 의 더 이른 틱 렌더가 아직 도는 동안★
//!   시작한 렌더 수. 결합 구조에서는 구조상 0 이다. `over_xpass` 는 둘 다 과부하인 짝만 센 것이다.
//! - `WALLCOMMIT conc=a/b/c/d` -- 그 Commit 이 시작할 때 이미 돌고 있던 다른 Commit 이 0/1/2/3 개
//!   이상이던 횟수. 넷이 정말 겹쳐 나가면 뒤쪽 칸이 찬다.
//! - `issue_us` -- 메인이 낸 시각부터 가드를 잡은 시각까지(워커 기동과 가드 대기).
//! - `start_phase`/`end_phase` -- DWM 합성 격자에서의 위상(주기 대비 %).

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

const RINGS: usize = 16;
const CAP: usize = 4096;
const COST_EVERY: u64 = 1024;
const DRAIN_EVERY: Duration = Duration::from_millis(250);
const DRAINS_PER_REPORT: u32 = 40;
/// RENDERSPLIT/render.over 와 같은 문턱(한 주기, 60 Hz).
const OVER_US: u64 = 16_667;
/// 이 이상 걸린 Commit 을 길다고 센다.
const LONG_COMMIT_US: u64 = 4_000;
pub(crate) const MAX_SLOTS: usize = 8;

/// 기록의 종류. 값은 링에 담기는 코드다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    /// painter 렌더 전체(present 포함). id = painter 슬롯, seq = 그 렌더를 보낸 틱, a/b = 시작/끝.
    /// flag 1 = 메인 스레드에서 그린 타일(인라인).
    Render = 1,
    /// 렌더 전에 자기 디바이스의 Commit 을 기다린 구간. id = painter 슬롯. flag 1 = 시한 초과.
    Gate = 2,
    /// 분리 모드에서 아직 렌더 중이라 그 틱을 건너뛴 painter. id = painter 슬롯, seq = 틱.
    BusySkip = 3,
    /// 렌더를 보내지 못했다(스레드가 없거나 채널이 닫혔다). id = painter 슬롯.
    RenderRefused = 4,
    /// 틱 머리 Commit. id = 디바이스 슬롯, seq = 틱, a = 메인이 낸 시각, b = 가드를 잡은 시각,
    /// c = Commit 이 돌아온 시각. flag 1 = hr < 0.
    TickCommit = 5,
    /// `end_frame` 안의 Commit(`-DcompCommitInFrame`). id = 디바이스 슬롯, a/b = Commit 앞뒤
    /// (가드 안). flag 1 = hr < 0.
    InframeCommit = 6,
    /// `begin_frame` 의 자기복구 Commit. a = 가드 전, b = 가드 후, c = Commit 뒤.
    RecoverCommit = 7,
    /// 메인 스레드가 painter 를 기다린 구간. id = [`MainSpan`].
    Main = 8,
    /// painter 슬롯과 디바이스·타일의 대응. id = painter 슬롯, a = 디바이스 슬롯,
    /// b = 타일 x, c = 타일 y. 처음 렌더할 때 한 번.
    SlotMap = 9,
}

impl Kind {
    fn from_code(code: u64) -> Option<Kind> {
        Some(match code {
            1 => Kind::Render,
            2 => Kind::Gate,
            3 => Kind::BusySkip,
            4 => Kind::RenderRefused,
            5 => Kind::TickCommit,
            6 => Kind::InframeCommit,
            7 => Kind::RecoverCommit,
            8 => Kind::Main,
            9 => Kind::SlotMap,
            _ => return None,
        })
    }
}

/// 메인 스레드가 painter 를 기다리는 자리.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum MainSpan {
    TickCommit = 0,
    Gather = 1,
    Produce = 2,
    Ack = 3,
    Join = 4,
    Flush = 5,
    Updates = 6,
    Messages = 7,
}

const MAIN_SPANS: [(&str, MainSpan); 8] = [
    ("tick_commit", MainSpan::TickCommit),
    ("gather", MainSpan::Gather),
    ("produce", MainSpan::Produce),
    ("ack", MainSpan::Ack),
    ("join", MainSpan::Join),
    ("flush", MainSpan::Flush),
    ("updates", MainSpan::Updates),
    ("messages", MainSpan::Messages),
];

struct Ring {
    head: AtomicU64,
    tail: AtomicU64,
    lost: AtomicU64,
    cost_n: AtomicU64,
    cost_sum: AtomicU64,
    cost_max: AtomicU64,
    slots: [[AtomicU64; 4]; CAP],
}

impl Ring {
    const fn new() -> Ring {
        Ring {
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            cost_n: AtomicU64::new(0),
            cost_sum: AtomicU64::new(0),
            cost_max: AtomicU64::new(0),
            slots: [const { [const { AtomicU64::new(0) }; 4] }; CAP],
        }
    }
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static RING: [Ring; RINGS] = [const { Ring::new() }; RINGS];
static NEXT_RING: AtomicUsize = AtomicUsize::new(0);
/// 링을 못 받은 스레드의 기록 수.
static NO_RING: AtomicU64 = AtomicU64::new(0);
/// 틱 번호. `on_wall_tick` 이 올린다.
static TICK: AtomicU64 = AtomicU64::new(0);
static NEXT_PAINTER_SLOT: AtomicUsize = AtomicUsize::new(0);
/// 디바이스 포인터 -> 슬롯. 처음 보는 디바이스를 CAS 로 넣는다(디바이스는 넷, 프로세스 수명).
static DEVICES: [AtomicUsize; MAX_SLOTS] = [const { AtomicUsize::new(0) }; MAX_SLOTS];

const UNCLAIMED: u8 = u8::MAX;
const NONE: u8 = u8::MAX - 1;

thread_local! {
    static MY_RING: Cell<u8> = const { Cell::new(UNCLAIMED) };
}

#[inline]
pub(crate) fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 켜져 있으면 지금 QPC 값, 꺼져 있으면 `None`. 기록 자리는 이것으로 시작 시각을 잡는다.
#[inline]
pub(crate) fn now() -> Option<u64> {
    if !is_enabled() {
        return None;
    }
    qpc()
}

#[cfg(windows)]
#[inline]
fn qpc() -> Option<u64> {
    crate::output_grid::qpc_now()
}

#[cfg(not(windows))]
#[inline]
fn qpc() -> Option<u64> {
    None
}

/// 틱을 하나 올리고 그 번호를 돌려준다. `on_wall_tick` 에서만 부른다.
pub(crate) fn begin_tick() -> u64 {
    TICK.fetch_add(1, Ordering::Relaxed) + 1
}

pub(crate) fn current_tick() -> u64 {
    TICK.load(Ordering::Relaxed)
}

/// painter 마다 한 번(`Painter::new`). 슬롯은 만든 순서이고 [`MAX_SLOTS`] 를 넘으면 마지막
/// 슬롯을 같이 쓴다.
pub(crate) fn next_painter_slot() -> u8 {
    NEXT_PAINTER_SLOT
        .fetch_add(1, Ordering::Relaxed)
        .min(MAX_SLOTS - 1) as u8
}

/// 디바이스 포인터의 슬롯. 처음 보는 포인터는 빈 칸에 넣는다. 넘치면 마지막 슬롯이다.
pub(crate) fn device_slot(device: usize) -> u8 {
    for (index, slot) in DEVICES.iter().enumerate() {
        let current = slot.load(Ordering::Relaxed);
        if current == device {
            return index as u8;
        }
        if current == 0
            && slot
                .compare_exchange(0, device, Ordering::AcqRel, Ordering::Relaxed)
                .map_or_else(|existing| existing == device, |_| true)
        {
            return index as u8;
        }
    }
    (MAX_SLOTS - 1) as u8
}

fn my_ring() -> Option<&'static Ring> {
    MY_RING.with(|cell| {
        let mut index = cell.get();
        if index == UNCLAIMED {
            let claimed = NEXT_RING.fetch_add(1, Ordering::Relaxed);
            index = if claimed < RINGS { claimed as u8 } else { NONE };
            cell.set(index);
        }
        (index != NONE).then(|| &RING[index as usize])
    })
}

fn pack_meta(kind: Kind, id: u8, flag: u8, seq: u64) -> u64 {
    ((kind as u64) << 56)
        | ((id as u64) << 48)
        | (((flag & 0xf) as u64) << 44)
        | (seq & ((1 << 44) - 1))
}

fn unpack_meta(meta: u64) -> (Option<Kind>, u8, u8, u64) {
    (
        Kind::from_code(meta >> 56),
        ((meta >> 48) & 0xff) as u8,
        ((meta >> 44) & 0xf) as u8,
        meta & ((1 << 44) - 1),
    )
}

/// 한 건 기록한다. 꺼져 있으면 아무것도 하지 않는다.
#[inline]
pub(crate) fn record(kind: Kind, id: u8, flag: u8, seq: u64, a: u64, b: u64, c: u64) {
    if !is_enabled() {
        return;
    }
    let Some(ring) = my_ring() else {
        NO_RING.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let head = ring.head.load(Ordering::Relaxed);
    let measure = head % COST_EVERY == COST_EVERY - 1;
    let cost_start = if measure { qpc() } else { None };
    if head.wrapping_sub(ring.tail.load(Ordering::Acquire)) >= CAP as u64 {
        ring.lost.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let slot = &ring.slots[(head % CAP as u64) as usize];
    slot[0].store(pack_meta(kind, id, flag, seq), Ordering::Relaxed);
    slot[1].store(a, Ordering::Relaxed);
    slot[2].store(b, Ordering::Relaxed);
    slot[3].store(c, Ordering::Relaxed);
    ring.head.store(head + 1, Ordering::Release);
    if let (Some(start), Some(end)) = (cost_start, qpc()) {
        let ns = qpc_to_ns(end.saturating_sub(start));
        ring.cost_n.fetch_add(1, Ordering::Relaxed);
        ring.cost_sum.fetch_add(ns, Ordering::Relaxed);
        ring.cost_max.fetch_max(ns, Ordering::Relaxed);
    }
}

/// 구간 하나. `start` 는 [`now`] 로 잡은 값이다.
#[inline]
pub(crate) fn record_span(kind: Kind, id: u8, flag: u8, seq: u64, start: Option<u64>) {
    if let (Some(start), Some(end)) = (start, now()) {
        record(kind, id, flag, seq, start, end, 0);
    }
}

#[inline]
pub(crate) fn record_main(span: MainSpan, start: Option<u64>) {
    record_span(Kind::Main, span as u8, 0, current_tick(), start);
}

#[cfg(windows)]
fn qpc_to_ns(ticks: u64) -> u64 {
    match crate::output_grid::qpc_frequency() {
        Some(freq) => (ticks as u128 * 1_000_000_000 / freq as u128) as u64,
        None => 0,
    }
}

#[cfg(not(windows))]
fn qpc_to_ns(_ticks: u64) -> u64 {
    0
}

fn qpc_to_us(ticks: u64) -> u64 {
    qpc_to_ns(ticks) / 1000
}

/// 리포터가 링에서 꺼낸 한 건.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Event {
    pub kind: Kind,
    pub id: u8,
    pub flag: u8,
    pub seq: u64,
    pub a: u64,
    pub b: u64,
    pub c: u64,
}

/// 켜고 리포터를 띄운다. `mode` 는 배너처럼 줄마다 붙일 구성 설명이다. 여러 painter 가 불러도
/// 한 번만.
pub(crate) fn start(mode: String) {
    if STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    ENABLED.store(true, Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name(String::from("WallSpan"))
        .spawn(move || reporter(mode));
    if let Err(error) = spawned {
        log::warn!("WALLSPAN: 리포터 스레드를 띄우지 못했다: {error}");
    }
}

fn drain(into: &mut Vec<Event>) {
    for ring in RING.iter() {
        let head = ring.head.load(Ordering::Acquire);
        let tail = ring.tail.load(Ordering::Relaxed);
        for index in tail..head {
            let slot = &ring.slots[(index % CAP as u64) as usize];
            let (kind, id, flag, seq) = unpack_meta(slot[0].load(Ordering::Relaxed));
            if let Some(kind) = kind {
                into.push(Event {
                    kind,
                    id,
                    flag,
                    seq,
                    a: slot[1].load(Ordering::Relaxed),
                    b: slot[2].load(Ordering::Relaxed),
                    c: slot[3].load(Ordering::Relaxed),
                });
            }
        }
        ring.tail.store(head, Ordering::Release);
    }
}

/// 링마다 누적값의 합(`lost`, 비용 표본).
fn totals() -> (u64, u64, u64, u64) {
    let mut lost = NO_RING.load(Ordering::Relaxed);
    let (mut n, mut sum, mut max) = (0, 0, 0);
    for ring in RING.iter() {
        lost += ring.lost.load(Ordering::Relaxed);
        n += ring.cost_n.load(Ordering::Relaxed);
        sum += ring.cost_sum.load(Ordering::Relaxed);
        max = max.max(ring.cost_max.swap(0, Ordering::Relaxed));
    }
    (lost, n, sum, max)
}

fn reporter(mode: String) {
    let started = std::time::Instant::now();
    let mut window: Vec<Event> = Vec::with_capacity(16 * 1024);
    let mut grid: Option<(u64, u64)> = None;
    let (mut last_lost, mut last_n, mut last_sum) = (0, 0, 0);
    let mut drains = 0;
    // 슬롯 대응은 처음 렌더할 때 한 번만 찍히므로, 창마다 내려면 리포터가 기억해 둔다.
    let mut known_map: std::collections::BTreeMap<u8, Event> = Default::default();
    loop {
        std::thread::sleep(DRAIN_EVERY);
        drain(&mut window);
        for event in window.iter().filter(|e| e.kind == Kind::SlotMap) {
            known_map.insert(event.id, *event);
        }
        #[cfg(windows)]
        if let Some(fresh) = crate::dcomp_compositor::composition_grid() {
            grid = Some(fresh);
        }
        drains += 1;
        if drains < DRAINS_PER_REPORT {
            continue;
        }
        drains = 0;
        let (lost, n, sum, max) = totals();
        let cost = Cost {
            lost: lost - last_lost,
            mean_ns: (sum - last_sum).checked_div(n - last_n).unwrap_or(0),
            max_ns: max,
        };
        (last_lost, last_n, last_sum) = (lost, n, sum);
        let map: Vec<Event> = known_map.values().copied().collect();
        for line in report_lines(
            started.elapsed().as_secs(),
            &mode,
            &window,
            &map,
            cost,
            grid,
            qpc_to_us,
        ) {
            log::warn!("{line}");
        }
        window.clear();
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Cost {
    pub lost: u64,
    pub mean_ns: u64,
    pub max_ns: u64,
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn p50_p95_max(mut values: Vec<u64>) -> String {
    values.sort_unstable();
    format!(
        "{}/{}/{}",
        percentile(&values, 0.5),
        percentile(&values, 0.95),
        values.last().copied().unwrap_or(0)
    )
}

/// 합성 격자에서의 위상(주기 대비 %).
pub(crate) fn phase_pct(t: u64, grid: (u64, u64)) -> u64 {
    let (vblank, period) = grid;
    if period == 0 {
        return 0;
    }
    let offset = (t as i128 - vblank as i128).rem_euclid(period as i128) as u64;
    offset * 100 / period
}

fn per_slot(values: &[u64; MAX_SLOTS], slots: usize) -> String {
    values[..slots.max(1)]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// 다른 painter 의 더 이른 틱 렌더가 아직 도는 동안 시작한 렌더의 수(painter 별). 둘 다 과부하인
/// 짝만 센 것이 두 번째 값이다. ★결합된 구조에서는 구조상 0 이다★ -- 메인이 패스마다 모든
/// 렌더를 기다리므로 다음 틱의 렌더가 앞 틱의 렌더와 겹칠 수 없다.
pub(crate) fn cross_pass_overlaps(
    renders: &[Event],
    to_us: impl Fn(u64) -> u64,
) -> ([u64; MAX_SLOTS], [u64; MAX_SLOTS]) {
    let mut xpass = [0u64; MAX_SLOTS];
    let mut over_xpass = [0u64; MAX_SLOTS];
    for r in renders {
        let r_over = to_us(r.b.saturating_sub(r.a)) > OVER_US;
        let mut hit = false;
        let mut over_hit = false;
        for other in renders {
            if other.id == r.id || other.seq >= r.seq {
                continue;
            }
            if other.a < r.a && other.b > r.a {
                hit = true;
                if r_over && to_us(other.b.saturating_sub(other.a)) > OVER_US {
                    over_hit = true;
                }
            }
        }
        let slot = (r.id as usize).min(MAX_SLOTS - 1);
        xpass[slot] += hit as u64;
        over_xpass[slot] += over_hit as u64;
    }
    (xpass, over_xpass)
}

/// 10 초 창 하나를 줄 여러 개로 접는다. 순수 함수라 시험할 수 있다.
pub(crate) fn report_lines(
    t_secs: u64,
    mode: &str,
    events: &[Event],
    slot_map: &[Event],
    cost: Cost,
    grid: Option<(u64, u64)>,
    to_us: impl Fn(u64) -> u64 + Copy,
) -> Vec<String> {
    let mut lines = Vec::new();
    let slots = events
        .iter()
        .chain(slot_map)
        .filter(|e| matches!(e.kind, Kind::Render | Kind::SlotMap))
        .map(|e| e.id as usize + 1)
        .max()
        .unwrap_or(1)
        .min(MAX_SLOTS);
    let mut map = Vec::new();
    for e in slot_map.iter().filter(|e| e.kind == Kind::SlotMap) {
        map.push(format!("p{}=d{}@{},{}", e.id, e.a, e.b, e.c));
    }
    lines.push(format!(
        "WALLSPAN t={t_secs}s w=10s {mode} records={} lost={} rec_ns={}/{} map={}",
        events.len(),
        cost.lost,
        cost.mean_ns,
        cost.max_ns,
        if map.is_empty() {
            String::from("-")
        } else {
            map.join(",")
        }
    ));

    // painter 별 렌더, 과부하, 겹침, 건너뜀.
    let renders: Vec<Event> = events
        .iter()
        .copied()
        .filter(|e| e.kind == Kind::Render)
        .collect();
    let mut count = [0u64; MAX_SLOTS];
    let mut over = [0u64; MAX_SLOTS];
    let mut skip = [0u64; MAX_SLOTS];
    let mut refused = [0u64; MAX_SLOTS];
    let mut inline = Vec::new();
    for r in &renders {
        let slot = (r.id as usize).min(MAX_SLOTS - 1);
        count[slot] += 1;
        over[slot] += (to_us(r.b.saturating_sub(r.a)) > OVER_US) as u64;
        if r.flag & 1 != 0 && !inline.contains(&r.id) {
            inline.push(r.id);
        }
    }
    for e in events {
        let slot = (e.id as usize).min(MAX_SLOTS - 1);
        match e.kind {
            Kind::BusySkip => skip[slot] += 1,
            Kind::RenderRefused => refused[slot] += 1,
            _ => {},
        }
    }
    let (xpass, over_xpass) = cross_pass_overlaps(&renders, to_us);
    // 같은 틱 안에서 가장 늦은 painter 와 가장 이른 painter 의 틱 차(렌더 시작 순서로).
    let lag_max = {
        let mut latest = [0u64; MAX_SLOTS];
        let mut lag = 0;
        let mut sorted = renders.clone();
        sorted.sort_by_key(|r| r.a);
        for r in &sorted {
            let slot = (r.id as usize).min(MAX_SLOTS - 1);
            latest[slot] = r.seq;
            let seen: Vec<u64> = latest[..slots].iter().copied().filter(|&s| s > 0).collect();
            if let (Some(min), Some(max)) = (seen.iter().min(), seen.iter().max()) {
                lag = lag.max(max - min);
            }
        }
        lag
    };
    lines.push(format!(
        "WALLDETACH t={t_secs}s w=10s renders={} over={} inline={} xpass={} over_xpass={} \
         busy_skip={} refused={} lag_max={lag_max}",
        per_slot(&count, slots),
        per_slot(&over, slots),
        if inline.is_empty() {
            String::from("-")
        } else {
            inline
                .iter()
                .map(|i| format!("p{i}"))
                .collect::<Vec<_>>()
                .join(",")
        },
        per_slot(&xpass, slots),
        per_slot(&over_xpass, slots),
        per_slot(&skip, slots),
        per_slot(&refused, slots),
    ));

    // 메인 스레드의 기다림.
    let mut main = String::from("WALLMAINWAIT t=");
    main.push_str(&format!("{t_secs}s w=10s"));
    for (name, span) in MAIN_SPANS {
        let values: Vec<u64> = events
            .iter()
            .filter(|e| e.kind == Kind::Main && e.id == span as u8)
            .map(|e| to_us(e.b.saturating_sub(e.a)))
            .collect();
        if values.is_empty() {
            continue;
        }
        let n = values.len();
        main.push_str(&format!(" {name}_us={n}:{}", p50_p95_max(values)));
    }
    lines.push(main);

    // Commit: 자리와 디바이스마다.
    let commits: Vec<&Event> = events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                Kind::TickCommit | Kind::InframeCommit | Kind::RecoverCommit
            )
        })
        .collect();
    // (시작, 끝) -- 동시성 판정용. 틱 Commit 은 가드를 잡은 뒤부터다.
    let span_of = |e: &Event| match e.kind {
        Kind::TickCommit | Kind::RecoverCommit => (e.b, e.c),
        _ => (e.a, e.b),
    };
    for (site, kind) in [
        ("tick", Kind::TickCommit),
        ("inframe", Kind::InframeCommit),
        ("recover", Kind::RecoverCommit),
    ] {
        for device in 0..MAX_SLOTS as u8 {
            let mine: Vec<&&Event> = commits
                .iter()
                .filter(|e| e.kind == kind && e.id == device)
                .collect();
            if mine.is_empty() {
                continue;
            }
            let mut durations = Vec::new();
            let mut issue = Vec::new();
            let mut conc = [0u64; 4];
            let mut long = 0;
            let mut rejected = 0;
            let mut start_phase = Vec::new();
            let mut end_phase = Vec::new();
            for e in &mine {
                let (start, end) = span_of(e);
                let us = to_us(end.saturating_sub(start));
                durations.push(us);
                long += (us >= LONG_COMMIT_US) as u64;
                rejected += (e.flag & 1) as u64;
                if kind != Kind::InframeCommit {
                    issue.push(to_us(e.b.saturating_sub(e.a)));
                }
                let in_flight = commits
                    .iter()
                    .filter(|o| {
                        let (os, oe) = span_of(o);
                        !std::ptr::eq(**o, **e) && os <= start && oe > start
                    })
                    .count();
                conc[in_flight.min(3)] += 1;
                if let Some(grid) = grid {
                    start_phase.push(phase_pct(start, grid));
                    end_phase.push(phase_pct(end, grid));
                }
            }
            let n = mine.len();
            let mut issue_sorted = issue.clone();
            issue_sorted.sort_unstable();
            start_phase.sort_unstable();
            end_phase.sort_unstable();
            lines.push(format!(
                "WALLCOMMIT t={t_secs}s w=10s site={site} dev={device} n={n} dur_us={} \
                 issue_us={}/{} conc={}/{}/{}/{} long={long} start_phase={}/{} \
                 end_phase={}/{} rej={rejected}",
                p50_p95_max(durations),
                percentile(&issue_sorted, 0.5),
                issue_sorted.last().copied().unwrap_or(0),
                conc[0],
                conc[1],
                conc[2],
                conc[3],
                percentile(&start_phase, 0.5),
                percentile(&start_phase, 0.95),
                percentile(&end_phase, 0.5),
                percentile(&end_phase, 0.95),
            ));
        }
    }

    // 틱마다 Commit 넷의 시작/끝 흩어짐, 그리고 관문.
    let mut by_tick: std::collections::BTreeMap<u64, Vec<&Event>> = Default::default();
    for e in commits.iter().filter(|e| e.kind == Kind::TickCommit) {
        by_tick.entry(e.seq).or_default().push(e);
    }
    let mut spread_start = Vec::new();
    let mut spread_end = Vec::new();
    for group in by_tick.values().filter(|g| g.len() > 1) {
        let starts: Vec<u64> = group.iter().map(|e| e.b).collect();
        let ends: Vec<u64> = group.iter().map(|e| e.c).collect();
        spread_start.push(to_us(
            starts.iter().max().unwrap() - starts.iter().min().unwrap(),
        ));
        spread_end.push(to_us(
            ends.iter().max().unwrap() - ends.iter().min().unwrap(),
        ));
    }
    let mut gate = String::new();
    let mut gate_timeout = 0;
    for slot in 0..slots as u8 {
        let waits: Vec<u64> = events
            .iter()
            .filter(|e| e.kind == Kind::Gate && e.id == slot)
            .map(|e| {
                gate_timeout += (e.flag & 1) as u64;
                to_us(e.b.saturating_sub(e.a))
            })
            .collect();
        if !waits.is_empty() {
            gate.push_str(&format!(
                " gate_p{slot}_us={}:{}",
                waits.len(),
                p50_p95_max(waits)
            ));
        }
    }
    lines.push(format!(
        "WALLCOMMITTICK t={t_secs}s w=10s ticks={} spread_start_us={} spread_end_us={}{gate} \
         gate_timeout={gate_timeout}",
        by_tick.len(),
        p50_p95_max(spread_start),
        p50_p95_max(spread_end),
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: Kind, id: u8, seq: u64, a: u64, b: u64) -> Event {
        Event {
            kind,
            id,
            flag: 0,
            seq,
            a,
            b,
            c: 0,
        }
    }

    #[test]
    fn meta_round_trips() {
        let meta = pack_meta(Kind::TickCommit, 3, 1, 123_456);
        assert_eq!(unpack_meta(meta), (Some(Kind::TickCommit), 3, 1, 123_456));
        assert_eq!(unpack_meta(0).0, None);
    }

    #[test]
    fn joined_renders_never_cross_a_pass() {
        // 틱 1 의 렌더 넷이 다 끝난 뒤 틱 2 가 시작한다(결합 구조).
        let renders = vec![
            ev(Kind::Render, 0, 1, 0, 20_000),
            ev(Kind::Render, 1, 1, 0, 5_000),
            ev(Kind::Render, 0, 2, 20_100, 25_000),
            ev(Kind::Render, 1, 2, 20_100, 24_000),
        ];
        let (xpass, over_xpass) = cross_pass_overlaps(&renders, |t| t);
        assert_eq!(xpass, [0; MAX_SLOTS]);
        assert_eq!(over_xpass, [0; MAX_SLOTS]);
    }

    #[test]
    fn a_detached_painter_starts_while_a_slow_one_is_still_rendering() {
        // painter 0 이 틱 1 을 20 ms 동안 그리는 사이 painter 1 이 틱 2 를 시작한다.
        let renders = vec![
            ev(Kind::Render, 0, 1, 0, 20_000),
            ev(Kind::Render, 1, 1, 0, 5_000),
            ev(Kind::Render, 1, 2, 16_700, 21_000),
        ];
        let (xpass, over_xpass) = cross_pass_overlaps(&renders, |t| t);
        assert_eq!(xpass[1], 1);
        assert_eq!(xpass[0], 0);
        assert_eq!(over_xpass[1], 0, "painter 1's render was short");
    }

    #[test]
    fn phase_wraps_around_the_grid() {
        let grid = (1_000, 100);
        assert_eq!(phase_pct(1_000, grid), 0);
        assert_eq!(phase_pct(1_050, grid), 50);
        assert_eq!(phase_pct(1_199, grid), 99);
        assert_eq!(phase_pct(990, grid), 90, "times before the anchor wrap too");
    }

    #[test]
    fn every_report_line_is_one_line() {
        let events = vec![
            ev(Kind::Render, 0, 1, 0, 20_000),
            ev(Kind::Render, 1, 1, 0, 5_000),
            Event {
                kind: Kind::TickCommit,
                id: 0,
                flag: 0,
                seq: 1,
                a: 0,
                b: 10,
                c: 5_000,
            },
            Event {
                kind: Kind::TickCommit,
                id: 1,
                flag: 1,
                seq: 1,
                a: 0,
                b: 20,
                c: 900,
            },
            ev(Kind::Main, MainSpan::Join as u8, 1, 0, 20_000),
            Event {
                kind: Kind::Gate,
                id: 1,
                flag: 1,
                seq: 2,
                a: 0,
                b: 3_000,
                c: 0,
            },
        ];
        let lines = report_lines(
            10,
            "detach=on",
            &events,
            &[],
            Cost::default(),
            Some((0, 16_667)),
            |t| t,
        );
        for line in &lines {
            assert!(!line.contains('\n'), "{line}");
        }
        let commit0 = lines
            .iter()
            .find(|l| l.contains("site=tick dev=0"))
            .unwrap();
        assert!(commit0.contains("long=1"), "{commit0}");
        // dev 0 이 먼저(10) 시작했으므로 그때 다른 Commit 은 없었고, dev 1 은 dev 0 이 도는 중(20)에
        // 시작했다.
        assert!(commit0.contains("conc=1/0/0/0"), "{commit0}");
        let commit1 = lines
            .iter()
            .find(|l| l.contains("site=tick dev=1"))
            .unwrap();
        assert!(commit1.contains("conc=0/1/0/0"), "{commit1}");
        assert!(commit1.contains("rej=1"), "{commit1}");
        let tick = lines
            .iter()
            .find(|l| l.starts_with("WALLCOMMITTICK"))
            .unwrap();
        assert!(tick.contains("gate_timeout=1"), "{tick}");
        let main = lines
            .iter()
            .find(|l| l.starts_with("WALLMAINWAIT"))
            .unwrap();
        assert!(main.contains("join_us=1:20000/20000/20000"), "{main}");
    }

    #[test]
    fn device_slots_are_stable() {
        let a = device_slot(0xdead_0000);
        let b = device_slot(0xbeef_0000);
        assert_ne!(a, b);
        assert_eq!(device_slot(0xdead_0000), a);
    }

    #[test]
    fn a_disabled_probe_reads_no_clock() {
        assert!(now().is_none());
    }
}

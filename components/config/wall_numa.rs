/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 벽 NUMA 배치 (2026-11-18 추가) -- 타일 GPU 의 노드 표, 스레드 핀, 비디오 배치.
//!
//! ## 왜
//!
//! 테스트 장비는 소켓 둘(Xeon Gold 6248 x2, 노드 둘, processor group 둘)이고, 2026-10-19 보드
//! 교체 뒤로 RX 580 넷이 두 노드에 2:2 로 붙어 있다(런처 `-NumaNode` 주석). 그래서 프로세스 전체를
//! 한 노드에 두는 `start /NODE` 로는 어느 값을 줘도 GPU 둘이 먼 노드에 남는다. 맞추려면 스레드
//! 단위로 해야 한다:
//! - 타일의 렌더 스레드(와 그 painter 의 WebRender 스레드)는 그 타일 GPU 의 노드에 둔다.
//! - 영상의 스트리밍 스레드는 그 영상을 그리는 타일의 노드에 둔다.
//!
//! 옛 보드(GPU 넷이 모두 노드 1)에서 프로세스 전체를 먼 노드에 두면 45 영상이 23~29.5 -> 6 fps,
//! Present 0.65 -> 2.9 ms 였다(`73d3aeddff0`). 스트리밍 스레드를 두 노드에 나눠 핀하면 디코드가
//! 0.99 -> 0.53 코어/영상이었다(`733085450e3`). 타일 단위 배치는 이 모듈이 처음이고 벽에서
//! 측정된 적이 없다.
//!
//! ## 누가 무엇을 하나
//!
//! - winit_wall: 기동 때 CPU 노드([`enumerate_cpu`])와 GPU 노드(winit_wall `numa.rs`, D3DKMT 와
//!   CfgMgr)를 알아내 [`publish`] 한다. 렌더 스레드를 [`pin_current_thread`] 로 핀하고, painter 를
//!   만들 스레드에 [`set_tile_hint`] 로 그 타일의 노드를 알린다.
//! - paint(`Painter::new`): [`current_thread_pin`] 이 있으면 WebRender 스레드도 같은 노드에 핀한다.
//! - media-thread: painter 의 D3D11 디바이스 -> 노드를 [`register_device_node`] 한다.
//! - GStreamer 백엔드: 영상마다 [`VideoPlacement`] 를 두고 스트리밍 스레드를 옮긴다.
//!
//! ## 왜 servo_config 에 있나
//!
//! `wall_event` 와 같은 이유다. 쓰는 자리가 winit_wall, paint, media-thread, GStreamer 백엔드에
//! 걸치고, 넷이 모두 의존하는 가장 가벼운 crate 가 이것이다.
//!
//! ## 성능 규칙
//!
//! 핀은 기동 때와, 영상을 그리는 타일 집합이 바뀔 때만 한다. 매 프레임 경로에서 하는 일은
//! 정렬된 디바이스 목록 비교와 스레드 로컬 읽기 하나다. pref 는 영상을 만들 때 한 번 읽는다.

#![allow(unsafe_code)]

use std::cell::Cell;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// 표가 다룰 수 있는 노드 수. 노드별 영상 수 배열의 크기이기도 하다.
pub const MAX_NODES: usize = 64;

/// CPU 노드 하나(가짜 분할이면 가짜 노드 하나).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuNode {
    /// 표 안의 노드 번호. 가짜 분할이 없으면 OS 노드 번호와 같다.
    pub node: u16,
    /// OS 의 NUMA 노드 번호.
    pub os_node: u16,
    /// 이 노드가 있는 processor group. 노드가 group 여럿에 걸치면 논리 프로세서가 가장 많은 group 이다.
    pub group: u16,
    /// 그 group 안의 논리 프로세서 마스크.
    pub mask: u64,
    /// 논리 프로세서 수(`mask` 의 비트 수).
    pub logical: u32,
    /// 물리 코어 수.
    pub cores: u32,
    /// 이 노드가 걸친 group 수(1 이 아니면 `mask` 는 그중 하나만 덮는다).
    pub groups_spanned: u16,
}

/// 물리 코어 하나(SMT 형제를 함께 담은 마스크).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Core {
    pub group: u16,
    pub mask: u64,
}

/// GPU(DXGI 어댑터) 하나의 위치.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuNode {
    pub gpu_index: usize,
    /// (HighPart, LowPart) -- `DisplayTopology::luid` 와 같은 순서.
    pub luid: Option<(i32, u32)>,
    /// PCI (bus, device, function).
    pub pci: Option<(u32, u32, u32)>,
    /// OS 가 알려 준 NUMA 노드. 모르면 `None`.
    pub os_node: Option<u16>,
    /// 어떻게 알아냈는지(또는 왜 모르는지).
    pub how: String,
}

/// 타일 하나의 노드.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileNode {
    /// 레이아웃의 타일 번호.
    pub tile_index: usize,
    pub gpu_index: Option<usize>,
    /// 표 노드 번호. `None` = 모른다(핀하지 않는다).
    pub node: Option<u16>,
    /// `gfx_wall_numa_tile_nodes` 로 덮어쓴 값인가.
    pub overridden: bool,
}

/// 기동 때 한 번 만들어지는 표.
#[derive(Clone, Debug, Default)]
pub struct Topology {
    pub nodes: Vec<CpuNode>,
    pub gpus: Vec<GpuNode>,
    pub tiles: Vec<TileNode>,
    /// `gfx_wall_numa_fake_split`(0 또는 1 = 분할 없음).
    pub fake_split: u32,
}

impl Topology {
    /// 노드가 둘 이상인가. 하나면 배치할 것이 없다.
    pub fn multi_node(&self) -> bool {
        self.nodes.len() > 1
    }

    pub fn node(&self, node: u16) -> Option<&CpuNode> {
        self.nodes.iter().find(|entry| entry.node == node)
    }

    /// OS 노드 -> 표 노드. 가짜 분할이면 그 OS 노드의 첫 가짜 노드다.
    pub fn table_node_for_os(&self, os_node: u16) -> Option<u16> {
        self.nodes
            .iter()
            .find(|entry| entry.os_node == os_node)
            .map(|entry| entry.node)
    }

    pub fn tile(&self, tile_index: usize) -> Option<&TileNode> {
        self.tiles.iter().find(|tile| tile.tile_index == tile_index)
    }

    /// 노드별 물리 코어 수(인덱스 = 표 노드 번호). 표에 없는 번호는 0 이다.
    fn cores_by_node(&self) -> Vec<u32> {
        let len = self
            .nodes
            .iter()
            .map(|entry| entry.node as usize + 1)
            .max()
            .unwrap_or(0)
            .min(MAX_NODES);
        let mut cores = vec![0; len];
        for entry in &self.nodes {
            if let Some(slot) = cores.get_mut(entry.node as usize) {
                *slot = entry.cores.max(1);
            }
        }
        cores
    }
}

static TOPOLOGY: OnceLock<Topology> = OnceLock::new();

/// 표를 한 번 정한다. 이미 정해져 있으면 `false`.
pub fn publish(topology: Topology) -> bool {
    TOPOLOGY.set(topology).is_ok()
}

pub fn topology() -> Option<&'static Topology> {
    TOPOLOGY.get()
}

// ----------------------------------------------------------------------------------------------
// 스레드 로컬: 이 스레드가 핀된 노드, 이 스레드가 만들 painter 의 타일.
// ----------------------------------------------------------------------------------------------

/// painter 를 만들 스레드에 winit_wall 이 알려 주는 "이 painter 의 타일".
///
/// painter 는 GPU 인덱스만 알고 타일 번호를 모른다. 그리고 개발 장비처럼 GPU 하나에 타일이 여럿이면
/// GPU 인덱스로는 타일 단위 덮어쓰기를 가를 수 없다. 그래서 painter 를 만들기 직전에 그 스레드에
/// 적어 둔다(인라인 타일은 메인 스레드, 병렬 타일은 WallTilePainter 스레드).
///
/// 한계(2026-11-18 리뷰): 렌더 스레드 핀은 이것으로 타일마다 갈리지만, 영상 배치는 갈리지 않는다.
/// 같은 GPU 의 타일들은 D3D11 디바이스 하나를 공유하고, 영상의 요구 신호는 디바이스 단위라서다.
/// 공유 디바이스에 노드가 둘 이상 등록되면 그 디바이스는 배치에 쓰지 않는다([`register_device_node`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileHint {
    pub tile_index: usize,
    pub node: Option<u16>,
}

thread_local! {
    static PINNED: Cell<Option<u16>> = const { Cell::new(None) };
    /// 처음 핀하기 전의 (group, mask). [`suspend_pin`] 이 되돌릴 때 쓴다.
    static PINNED_PREV: Cell<Option<(u16, u64)>> = const { Cell::new(None) };
    static TILE_HINT: Cell<Option<TileHint>> = const { Cell::new(None) };
}

pub fn set_tile_hint(hint: Option<TileHint>) {
    TILE_HINT.with(|cell| cell.set(hint));
}

pub fn tile_hint() -> Option<TileHint> {
    TILE_HINT.with(Cell::get)
}

/// 이 스레드를 [`pin_current_thread`] 로 핀한 노드. 핀하지 않았거나 실패했으면 `None`.
pub fn current_thread_pin() -> Option<u16> {
    PINNED.with(Cell::get)
}

// ----------------------------------------------------------------------------------------------
// 순수 함수(시험 대상).
// ----------------------------------------------------------------------------------------------

/// 2026-11-18: 실제 노드마다 물리 코어를 `parts` 묶음으로 나눠 가짜 노드로 삼는다
/// (`gfx_wall_numa_fake_split`, 진단 전용). 노드 `r` 의 `k` 번째 묶음은 가짜 노드 `r*parts+k` 다.
/// 코어는 마스크의 가장 낮은 비트 순으로 연속해 나누므로 SMT 형제는 같은 묶음에 남는다.
pub fn split_nodes(nodes: &[CpuNode], cores: &[Core], parts: u32) -> Vec<CpuNode> {
    if parts <= 1 {
        return nodes.to_vec();
    }
    let mut out = Vec::new();
    for real in nodes {
        let mut mine: Vec<u64> = cores
            .iter()
            .filter(|core| core.group == real.group && core.mask & real.mask != 0)
            .map(|core| core.mask & real.mask)
            .collect();
        mine.sort_by_key(|mask| mask.trailing_zeros());
        if mine.is_empty() {
            out.push(real.clone());
            continue;
        }
        let count = (parts as usize).min(mine.len());
        for part in 0..count {
            let lo = mine.len() * part / count;
            let hi = mine.len() * (part + 1) / count;
            let mask = mine[lo..hi].iter().fold(0u64, |acc, mask| acc | mask);
            let node = real.os_node as u32 * parts + part as u32;
            if node as usize >= MAX_NODES {
                break;
            }
            out.push(CpuNode {
                node: node as u16,
                os_node: real.os_node,
                group: real.group,
                mask,
                logical: mask.count_ones(),
                cores: (hi - lo) as u32,
                groups_spanned: real.groups_spanned,
            });
        }
    }
    out
}

/// 2026-11-18: `gfx_wall_numa_tile_nodes` 를 읽는다. `"0,1,-,1"` -> `[Some(0), Some(1), None, Some(1)]`.
/// 빈 문자열은 빈 목록이다. 빈 칸과 `-` 는 "감지값". 숫자가 아닌 칸은 오류다.
pub fn parse_tile_nodes(spec: &str) -> Result<Vec<Option<u16>>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(Vec::new());
    }
    spec.split(',')
        .map(|field| {
            let field = field.trim();
            if field.is_empty() || field == "-" {
                Ok(None)
            } else {
                field
                    .parse::<u16>()
                    .map(Some)
                    .map_err(|_| format!("'{field}' is not a node number"))
            }
        })
        .collect()
}

/// 2026-11-18: `KMTQAITYPE_PHYSICALADAPTERPNPKEY` 가 돌려주는 것은 인스턴스 ID 가 아니라 레지스트리
/// 경로다(`\REGISTRY\MACHINE\SYSTEM\ControlSet001\Enum\<instance id>\Device Parameters`). 그대로
/// `CM_Locate_DevNodeW` 에 주면 `CR_INVALID_DEVICE_ID` 가 난다(2026-11-18 개발 장비 실측). `\Enum\`
/// 뒤부터, 끝의 `\Device Parameters` 앞까지가 인스턴스 ID 다.
pub fn instance_id_from_pnp_key(key: &str) -> Option<String> {
    let lower = key.to_ascii_lowercase();
    let start = lower.find("\\enum\\")? + "\\enum\\".len();
    let mut end = key.len();
    if lower.ends_with("\\device parameters") {
        end -= "\\device parameters".len();
    }
    (start < end).then(|| key[start..end].to_owned())
}

/// 2026-11-18: 영상 하나의 노드를 고른다.
///
/// - `current`: 지금 노드.
/// - `demanded`: 이 영상을 요구한 디바이스들의 노드(디바이스 하나에 하나, 중복 허용).
/// - `loads`: 노드별 영상 수. ★이 영상은 뺀 값이다★.
/// - `cores`: 노드별 물리 코어 수(같은 길이).
///
/// 규칙:
/// 1. 요구가 없으면 지금 노드를 유지한다. 지금 노드가 없으면 영상이 가장 적은 노드로 간다.
/// 2. 지금 노드가 요구 노드 안에 있으면 유지한다. 요구는 2 초 TTL 이라 영상이 옮겨 가는 동안 옛 타일과
///    새 타일이 함께 보인다. 여기서 바꾸면 그 2 초 동안 왔다 갔다 한다.
/// 3. 아니면 요구가 가장 많은 노드(같으면 영상이 적은 쪽)를 고른다.
/// 4. 그 노드의 영상 수가 가장 적은 노드보다 `max(2, 물리 코어/4)` 이상 많으면 가장 적은 노드로
///    넘긴다. GPU 가 한 노드에 몰린 장비에서 디코드가 한 노드에 몰리지 않게 하려는 것이다. 한 노드에
///    가두면 0.74 코어/영상, 두 노드에 나누면 0.53 이었다(2026-08-27, `733085450e3`). SMT 는 물리
///    코어 20 에서 포화했다(`b097591a87b`).
pub fn choose_video_node(
    current: Option<u16>,
    demanded: &[u16],
    loads: &[usize],
    cores: &[u32],
) -> Option<u16> {
    let count = loads.len().min(cores.len());
    let present = |node: u16| (node as usize) < count && cores[node as usize] > 0;
    let least = (0..count)
        .filter(|&node| cores[node] > 0)
        .min_by_key(|&node| (loads[node], std::cmp::Reverse(cores[node]), node))
        .map(|node| node as u16)?;
    let current = current.filter(|&node| present(node));
    if demanded.iter().all(|&node| !present(node)) {
        return Some(current.unwrap_or(least));
    }
    if let Some(node) = current
        && demanded.contains(&node)
    {
        return Some(node);
    }
    let preferred = (0..count as u16)
        .filter(|&node| present(node))
        .map(|node| (node, demanded.iter().filter(|&&d| d == node).count()))
        .filter(|&(_, hits)| hits > 0)
        .max_by_key(|&(node, hits)| {
            (
                hits,
                std::cmp::Reverse(loads[node as usize]),
                std::cmp::Reverse(node),
            )
        })
        .map(|(node, _)| node)?;
    let min_load = (0..count)
        .filter(|&node| cores[node] > 0)
        .map(|node| loads[node])
        .min()
        .unwrap_or(0);
    let slack = (cores[preferred as usize] as usize / 4).max(2);
    if loads[preferred as usize] >= min_load + slack {
        Some(least)
    } else {
        Some(preferred)
    }
}

// ----------------------------------------------------------------------------------------------
// 핀.
// ----------------------------------------------------------------------------------------------

/// 핀 한 번의 결과. 되읽기까지 담는다 -- 되읽지 않은 핀을 핀했다고 적으면 안 된다
/// (2026-08 `start /AFFINITY` 가 조용히 거절된 일, `f43a69557b9`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinReport {
    pub node: u16,
    pub group: u16,
    pub mask: u64,
    /// 핀 전의 group 과 마스크. 핀하지 않은 스레드면 Windows 11 에서는 primary group 이다.
    pub prev_group: u16,
    pub prev_mask: u64,
    /// `GetThreadGroupAffinity` 가 요청한 값을 돌려줬는가.
    pub readback_ok: bool,
    /// 핀 직후 이 스레드가 돈 (group, processor).
    pub ran_on: (u16, u8),
    /// 그 프로세서가 마스크 안인가.
    pub ran_inside: bool,
}

impl std::fmt::Display for PinReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "node={} group={} mask=0x{:x} prev=g{}/0x{:x} readback={} ran_on=g{}/#{} inside={}",
            self.node,
            self.group,
            self.mask,
            self.prev_group,
            self.prev_mask,
            if self.readback_ok { "ok" } else { "MISMATCH" },
            self.ran_on.0,
            self.ran_on.1,
            if self.ran_inside { "yes" } else { "NO" },
        )
    }
}

/// 이 스레드를 표 노드 `node` 에 하드 핀한다(`SetThreadGroupAffinity`). 성공하면
/// [`current_thread_pin`] 이 그 노드를 돌려준다.
pub fn pin_current_thread(node: u16) -> Result<PinReport, String> {
    let entry = topology()
        .ok_or("no NUMA table")?
        .node(node)
        .ok_or_else(|| format!("node {node} is not in the NUMA table"))?
        .clone();
    let report = imp::pin_current(&entry)?;
    PINNED.with(|cell| cell.set(Some(node)));
    PINNED_PREV.with(|cell| {
        if cell.get().is_none() {
            cell.set(Some((report.prev_group, report.prev_mask)));
        }
    });
    Ok(report)
}

/// 2026-11-18 (리뷰): 이 스레드의 핀을 `f` 동안 핀하기 전의 affinity 로 되돌렸다가 다시 핀한다.
///
/// 메인 스레드는 타일 0 의 디바이스를 만들기 전에 핀하는데, 그 뒤 `ServoBuilder::build` 가 메인에서
/// 엔진 스레드(constellation, script, layout, net, ...)를 띄운다. Windows 문서는 "새 스레드는 그것을
/// 만든 스레드의 group 에 배정된다" 고 하므로, 그대로 두면 그 스레드들이 모두 타일 0 의 group 을
/// 물려받는다 -- `-NumaNode` 가 정한(또는 Windows 가 고른) primary group 이 아니라. 그 사이만 풀어
/// 둔다. 핀되어 있지 않으면 `f` 만 부른다.
pub fn suspend_pin<R>(f: impl FnOnce() -> R) -> R {
    let (Some(node), Some((group, mask))) = (current_thread_pin(), PINNED_PREV.with(Cell::get))
    else {
        return f();
    };
    if !imp::set_current_affinity(group, mask) {
        return f();
    }
    PINNED.with(|cell| cell.set(None));
    let result = f();
    let _ = pin_current_thread(node);
    result
}

/// [`pin_current_thread`] 를 하고 결과를 `warn!` 한 줄로 남긴다(엔진 스레드용, 로거가 선 뒤).
/// 런처는 `RUST_LOG=warn` 이라 info 는 보이지 않는다.
pub fn pin_current_thread_logged(node: u16, who: &str) {
    match pin_current_thread(node) {
        Ok(report) => log::warn!("WALLNUMA pin {who}: {report}"),
        Err(error) => log::warn!("WALLNUMA pin {who}: node={node} FAILED: {error}"),
    }
}

/// CPU 노드와 물리 코어를 OS 에서 읽는다. 노드가 group 여럿에 걸치면 논리 프로세서가 가장 많은
/// group 을 쓴다(`SetThreadGroupAffinity` 는 group 하나만 받는다).
pub fn enumerate_cpu() -> (Vec<CpuNode>, Vec<Core>) {
    imp::enumerate_cpu()
}

// ----------------------------------------------------------------------------------------------
// 디바이스 -> 노드(미디어).
// ----------------------------------------------------------------------------------------------

/// D3D11 디바이스 포인터(media-thread 와 D3D11 링이 디바이스를 가리키는 값) -> 표 노드.
/// painter 마다 한 번 쓰고, 영상의 요구 디바이스 집합이 바뀔 때만 읽는다.
/// `None` = 노드가 서로 다른 타일이 이 디바이스를 함께 쓴다(배치에 쓰지 않는다).
static DEVICE_NODES: RwLock<Vec<(usize, Option<u16>)>> = RwLock::new(Vec::new());

/// 2026-11-18 (리뷰): 같은 GPU 의 타일들은 D3D11 디바이스 하나를 공유한다(media-thread
/// `same_device_for_the_same_gpu_is_normal`). 그 타일들의 노드가 다르면(덮어쓰기, 가짜 분할) 나중
/// 등록이 앞의 것을 덮어 영상이 엉뚱한 타일의 노드를 따라갔다. 이제 그 디바이스를 모호로 표시하고
/// 한 번 경고한다. 같은 노드로 다시 등록하는 것은 정상이라 조용히 지나간다.
pub fn register_device_node(device: usize, node: u16) {
    let mut table = DEVICE_NODES
        .write()
        .unwrap_or_else(|poison| poison.into_inner());
    match table.iter_mut().find(|(seen, _)| *seen == device) {
        None => table.push((device, Some(node))),
        Some((_, entry)) => {
            if let Some(existing) = *entry
                && existing != node
            {
                log::warn!(
                    "WALLNUMA warning: media device 0x{device:x} is shared by tiles on nodes \
                     {existing} and {node} (tiles on one GPU share a D3D11 device); video \
                     placement ignores this device"
                );
                *entry = None;
            }
        },
    }
}

pub fn device_node(device: usize) -> Option<u16> {
    DEVICE_NODES
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .iter()
        .find(|&&(seen, _)| seen == device)
        .and_then(|&(_, node)| node)
}

// ----------------------------------------------------------------------------------------------
// 영상 배치.
// ----------------------------------------------------------------------------------------------

/// 노드별 영상 수(노드가 정해진 [`VideoPlacement`] 수).
static VIDEO_LOAD: [AtomicUsize; MAX_NODES] = [const { AtomicUsize::new(0) }; MAX_NODES];
static NEXT_VIDEO: AtomicUsize = AtomicUsize::new(0);
/// 스레드 ID -> 지금 그 스레드를 가진 영상. GLib 의 스레드 풀이 스트리밍 스레드를 다른 파이프라인에
/// 다시 줄 수 있어서, 옛 영상이 새 주인의 스레드를 옮기지 않게 한다.
static THREAD_OWNER: Mutex<Vec<(u32, usize)>> = Mutex::new(Vec::new());

/// 영상 하나의 스트리밍 스레드 배치(`media_numa_pin_policy=tile`).
///
/// - 버스의 `STREAM_STATUS` ENTER/LEAVE 가 그 파이프라인의 스트리밍 스레드를 등록하고 뺀다
///   ([`Self::enter_current_thread`]). ENTER 는 그 스레드 자신이 보내므로 그 자리에서 핀한다.
/// - D3D11 렌더가 요구 디바이스 집합이 바뀔 때 [`Self::on_devices_changed`] 를 부르고, 노드가
///   바뀌면 등록된 스레드를 모두 옮긴다.
pub struct VideoPlacement {
    id: usize,
    /// 표 노드 번호, -1 = 아직 없음.
    node: AtomicI32,
    threads: Mutex<Vec<(u32, isize)>>,
}

impl VideoPlacement {
    /// 이 영상에 `tile` 배치를 쓸지 정한다. 안 쓰면 `None` 이고 호출자는 예전 round-robin 을 쓴다.
    /// pref 는 여기서 한 번만 읽는다.
    pub fn for_new_video() -> Option<Arc<Self>> {
        if !crate::pref!(media_numa_pin_streaming_threads) {
            return None;
        }
        let policy = crate::pref!(media_numa_pin_policy);
        match policy.trim().to_ascii_lowercase().as_str() {
            "" | "tile" => {},
            "roundrobin" | "round-robin" => return None,
            other => {
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    log::warn!(
                        "Ignoring invalid media_numa_pin_policy={other:?}; expected tile or roundrobin (using tile)"
                    )
                });
            },
        }
        let topology = topology()?;
        if !topology.multi_node() {
            return None;
        }
        Some(Arc::new(Self {
            id: NEXT_VIDEO.fetch_add(1, Ordering::Relaxed),
            node: AtomicI32::new(-1),
            threads: Mutex::new(Vec::new()),
        }))
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn node(&self) -> Option<u16> {
        let node = self.node.load(Ordering::Acquire);
        (node >= 0).then_some(node as u16)
    }

    /// ENTER: 이 스트리밍 스레드를 등록하고 영상의 노드에 핀한다. 아직 노드가 없으면 영상이 가장
    /// 적은 노드를 고른다(어느 타일도 아직 그리지 않았다).
    ///
    /// 2026-11-18 (리뷰): 등록, 노드 읽기(또는 첫 결정), 자기 핀을 `threads` 자물쇠 안에서 한다.
    /// [`Self::on_devices_changed`] 도 같은 자물쇠 안에서 노드를 바꾸고 등록된 스레드를 옮긴다. 그래서
    /// 노드 변경은 이 ENTER 의 앞이나 뒤에 통째로 일어난다 -- 앞이면 새 노드를 읽고, 뒤면 핸들로
    /// 옮겨진다. 따로 하던 때는 옛 노드를 읽은 스레드가 새 노드로 옮겨진 직후 옛 노드로 돌아가 그대로
    /// 남을 수 있었고, 첫 ENTER 둘이 서로 다른 노드를 골라 노드별 영상 수가 잠깐 음수로 돌 수 있었다.
    pub fn enter_current_thread(&self) {
        let Some((tid, handle)) = imp::open_current_thread() else {
            return;
        };
        {
            let mut owners = THREAD_OWNER
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            owners.retain(|&(seen, _)| seen != tid);
            owners.push((tid, self.id));
        }
        let mut line = None;
        {
            let mut threads = self
                .threads
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(slot) = threads.iter_mut().find(|(seen, _)| *seen == tid) {
                imp::close(slot.1);
                slot.1 = handle;
            } else {
                threads.push((tid, handle));
            }
            let node = match self.node() {
                Some(node) => Some(node),
                None => {
                    let chosen = self.choose(&[]);
                    if let Some(node) = chosen {
                        line = self.assign_locked(threads.as_slice(), node, "enter", &[]);
                    }
                    chosen
                },
            };
            if let Some(node) = node {
                let _ = pin_current_thread(node);
            }
        }
        if let Some(line) = line {
            log::warn!("{line}");
        }
    }

    /// LEAVE: 등록을 뺀다. 스레드는 그 노드에 남는다(되돌릴 API 가 없다 -- 다음 ENTER 가 다시 핀한다).
    pub fn leave_current_thread(&self) {
        let tid = imp::current_thread_id();
        {
            let mut threads = self
                .threads
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            threads.retain(|&(seen, handle)| {
                if seen == tid {
                    imp::close(handle);
                }
                seen != tid
            });
        }
        let mut owners = THREAD_OWNER
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        owners.retain(|&(seen, owner)| !(seen == tid && owner == self.id));
    }

    /// 요구 디바이스 집합이 바뀌었을 때(호출자가 정렬해 비교한다) 노드를 다시 고른다.
    pub fn on_devices_changed(&self, devices: &[usize]) {
        let demanded: Vec<u16> = devices
            .iter()
            .filter_map(|&device| device_node(device))
            .collect();
        let line = {
            let threads = self
                .threads
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match self.choose(&demanded) {
                Some(node) if Some(node) != self.node() => {
                    self.assign_locked(threads.as_slice(), node, "demand", &demanded)
                },
                _ => None,
            }
        };
        if let Some(line) = line {
            log::warn!("{line}");
        }
    }

    /// 지금 스레드가 영상의 노드에 있지 않으면 옮긴다. 매 프레임 불러도 된다 -- 스레드 로컬 읽기와
    /// 원자 읽기 하나이고, 시스템 호출은 노드가 바뀐 프레임에만 한다.
    pub fn follow_current_thread(&self) {
        if let Some(node) = self.node()
            && current_thread_pin() != Some(node)
        {
            let _ = pin_current_thread(node);
        }
    }

    fn choose(&self, demanded: &[u16]) -> Option<u16> {
        let topology = topology()?;
        let cores = topology.cores_by_node();
        let mut loads: Vec<usize> = (0..cores.len())
            .map(|node| VIDEO_LOAD[node].load(Ordering::Relaxed))
            .collect();
        if let Some(node) = self.node()
            && let Some(load) = loads.get_mut(node as usize)
        {
            *load = load.saturating_sub(1);
        }
        choose_video_node(self.node(), demanded, &loads, &cores)
    }

    /// 노드를 `node` 로 바꾸고 등록된 스레드를 옮긴다. ★`threads` 자물쇠를 쥔 채로 부른다★
    /// ([`Self::enter_current_thread`] 주석). 남길 줄을 돌려준다 -- 쓰기는 자물쇠를 놓은 뒤에 한다.
    fn assign_locked(
        &self,
        threads: &[(u32, isize)],
        node: u16,
        reason: &str,
        demanded: &[u16],
    ) -> Option<String> {
        let topology = topology()?;
        let entry = topology.node(node)?.clone();
        let old = self.node.swap(node as i32, Ordering::AcqRel);
        if old == node as i32 {
            return None;
        }
        if old >= 0 {
            VIDEO_LOAD[old as usize].fetch_sub(1, Ordering::Relaxed);
        }
        VIDEO_LOAD[node as usize].fetch_add(1, Ordering::Relaxed);
        let owned: Vec<u32> = {
            let owners = THREAD_OWNER
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            owners
                .iter()
                .filter(|&&(_, owner)| owner == self.id)
                .map(|&(tid, _)| tid)
                .collect()
        };
        let (mut moved, mut failed) = (0, 0);
        for &(tid, handle) in threads {
            if !owned.contains(&tid) {
                continue;
            }
            if imp::pin_handle(handle, &entry) {
                moved += 1;
            } else {
                failed += 1;
            }
        }
        let total = threads.len();
        let loads: Vec<usize> = (0..topology.cores_by_node().len())
            .map(|index| VIDEO_LOAD[index].load(Ordering::Relaxed))
            .collect();
        Some(format!(
            "WALLNUMA video v{} node {} -> {} reason={reason} demanded={demanded:?} threads={total} \
             moved={moved} failed={failed} loads={loads:?}",
            self.id,
            if old >= 0 {
                old.to_string()
            } else {
                "-".to_owned()
            },
            node,
        ))
    }
}

impl Drop for VideoPlacement {
    fn drop(&mut self) {
        let node = self.node.load(Ordering::Acquire);
        if node >= 0 {
            VIDEO_LOAD[node as usize].fetch_sub(1, Ordering::Relaxed);
        }
        let threads = std::mem::take(
            &mut *self
                .threads
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
        for (_, handle) in threads {
            imp::close(handle);
        }
        let mut owners = THREAD_OWNER
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        owners.retain(|&(_, owner)| owner != self.id);
    }
}

// ----------------------------------------------------------------------------------------------
// Win32.
// ----------------------------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::System::Kernel::PROCESSOR_NUMBER;
    use windows_sys::Win32::System::SystemInformation::{
        GROUP_AFFINITY, GetLogicalProcessorInformationEx, LOGICAL_PROCESSOR_RELATIONSHIP,
        RelationNumaNode, RelationNumaNodeEx, RelationProcessorCore,
        SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessorNumberEx, GetCurrentThread, GetCurrentThreadId, GetThreadGroupAffinity,
        OpenThread, SetThreadGroupAffinity, THREAD_QUERY_INFORMATION, THREAD_SET_INFORMATION,
    };

    use super::{Core, CpuNode, PinReport};

    fn affinity_for(entry: &CpuNode) -> GROUP_AFFINITY {
        // SAFETY: GROUP_AFFINITY 는 정수만 담는다. Reserved 는 0 이어야 한다(아니면 ERROR_INVALID_PARAMETER).
        let mut affinity: GROUP_AFFINITY = unsafe { std::mem::zeroed() };
        affinity.Mask = entry.mask as usize;
        affinity.Group = entry.group;
        affinity
    }

    pub(super) fn pin_current(entry: &CpuNode) -> Result<PinReport, String> {
        let wanted = affinity_for(entry);
        // SAFETY: 아래 호출은 모두 현재 스레드의 의사 핸들(닫으면 안 된다)과 크기가 맞는 출력 인자만 쓴다.
        unsafe {
            let mut previous: GROUP_AFFINITY = std::mem::zeroed();
            if SetThreadGroupAffinity(GetCurrentThread(), &wanted, &mut previous) == 0 {
                return Err(format!(
                    "SetThreadGroupAffinity failed: error {}",
                    GetLastError()
                ));
            }
            let mut readback: GROUP_AFFINITY = std::mem::zeroed();
            let readback_ok = GetThreadGroupAffinity(GetCurrentThread(), &mut readback) != 0
                && readback.Group == wanted.Group
                && readback.Mask == wanted.Mask;
            let mut processor: PROCESSOR_NUMBER = std::mem::zeroed();
            GetCurrentProcessorNumberEx(&mut processor);
            let ran_inside = processor.Group == wanted.Group
                && processor.Number < 64
                && (wanted.Mask >> processor.Number) & 1 == 1;
            Ok(PinReport {
                node: entry.node,
                group: wanted.Group,
                mask: wanted.Mask as u64,
                prev_group: previous.Group,
                prev_mask: previous.Mask as u64,
                readback_ok,
                ran_on: (processor.Group, processor.Number),
                ran_inside,
            })
        }
    }

    pub(super) fn set_current_affinity(group: u16, mask: u64) -> bool {
        // SAFETY: GROUP_AFFINITY 는 정수만 담고 Reserved 는 0 이다. 현재 스레드의 의사 핸들을 쓴다.
        unsafe {
            let mut affinity: GROUP_AFFINITY = std::mem::zeroed();
            affinity.Group = group;
            affinity.Mask = mask as usize;
            SetThreadGroupAffinity(GetCurrentThread(), &affinity, std::ptr::null_mut()) != 0
        }
    }

    pub(super) fn pin_handle(handle: isize, entry: &CpuNode) -> bool {
        let wanted = affinity_for(entry);
        // SAFETY: `handle` 은 `open_current_thread` 가 THREAD_SET_INFORMATION 으로 연 실제 핸들이고,
        // 등록이 빠질 때까지 닫지 않는다.
        unsafe { SetThreadGroupAffinity(handle as HANDLE, &wanted, std::ptr::null_mut()) != 0 }
    }

    pub(super) fn current_thread_id() -> u32 {
        // SAFETY: 인자 없는 조회.
        unsafe { GetCurrentThreadId() }
    }

    pub(super) fn open_current_thread() -> Option<(u32, isize)> {
        // SAFETY: 자기 스레드 ID 로 실제 핸들을 연다. 실패하면 null 이다.
        unsafe {
            let tid = GetCurrentThreadId();
            let handle = OpenThread(THREAD_SET_INFORMATION | THREAD_QUERY_INFORMATION, 0, tid);
            (!handle.is_null()).then_some((tid, handle as isize))
        }
    }

    pub(super) fn close(handle: isize) {
        // SAFETY: `open_current_thread` 가 연 핸들을 한 번만 닫는다.
        unsafe {
            CloseHandle(handle as HANDLE);
        }
    }

    /// `GetLogicalProcessorInformationEx` 의 항목을 하나씩 넘긴다. 버퍼는 `u64` 로 잡는다 --
    /// GROUP_AFFINITY 가 usize 를 담아 8 바이트 정렬이 필요하다.
    fn for_each_entry(
        relationship: LOGICAL_PROCESSOR_RELATIONSHIP,
        mut visit: impl FnMut(*const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
    ) {
        let mut length: u32 = 0;
        // SAFETY: 첫 호출은 길이만 얻는다(버퍼 null).
        unsafe {
            GetLogicalProcessorInformationEx(relationship, std::ptr::null_mut(), &mut length);
        }
        if length == 0 {
            return;
        }
        let mut buffer = vec![0u64; (length as usize).div_ceil(8)];
        let base = buffer.as_mut_ptr() as *mut u8;
        // SAFETY: 버퍼는 `length` 바이트 이상이고 8 바이트 정렬이다.
        if unsafe {
            GetLogicalProcessorInformationEx(
                relationship,
                base as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
                &mut length,
            )
        } == 0
        {
            return;
        }
        let mut offset = 0usize;
        while offset + 8 <= length as usize {
            // SAFETY: `offset` 은 OS 가 적은 항목 경계이고, 각 항목은 `Size` 바이트다.
            let entry =
                unsafe { base.add(offset) } as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX;
            let size = unsafe { (*entry).Size } as usize;
            if size == 0 {
                break;
            }
            visit(entry);
            offset += size;
        }
    }

    pub(super) fn enumerate_cpu() -> (Vec<CpuNode>, Vec<Core>) {
        let mut cores = Vec::new();
        for_each_entry(RelationProcessorCore, |entry| {
            // SAFETY: RelationProcessorCore 항목의 공용체는 Processor 이고, 코어의 GroupCount 는 1 이다.
            let processor = unsafe { &(*entry).Anonymous.Processor };
            let mask = processor.GroupMask[0];
            cores.push(Core {
                group: mask.Group,
                mask: mask.Mask as u64,
            });
        });
        // 2026-11-18 (리뷰): RelationNumaNodeEx 는 빌드 20348 이전(Windows 10, Server 2019)에는 실패한다.
        // 그때는 RelationNumaNode 로 다시 읽는다(GroupCount 0, GroupMask 하나).
        let mut nodes = Vec::new();
        for relationship in [RelationNumaNodeEx, RelationNumaNode] {
            for_each_entry(relationship, |entry| {
                if let Some(node) = numa_entry(entry, &cores) {
                    nodes.push(node);
                }
            });
            if !nodes.is_empty() {
                break;
            }
        }
        nodes.sort_by_key(|entry| entry.node);
        nodes.dedup_by_key(|entry| entry.node);
        (nodes, cores)
    }

    fn numa_entry(
        entry: *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
        cores: &[Core],
    ) -> Option<CpuNode> {
        {
            // SAFETY: NUMA 항목의 공용체는 NumaNode 이다. GroupMasks 는 가변 길이 배열이라 원시 포인터로
            // 읽는다. GroupCount 는 빌드 20348 이전에는 0 이다(그때는 1 개).
            let numa = unsafe { &(*entry).Anonymous.NumaNode };
            let count = numa.GroupCount.max(1) as usize;
            let first = unsafe { numa.Anonymous.GroupMasks.as_ptr() };
            let masks: Vec<GROUP_AFFINITY> = (0..count)
                .map(|index| unsafe { *first.add(index) })
                .collect();
            let best = masks.iter().max_by_key(|mask| mask.Mask.count_ones())?;
            if best.Mask == 0 {
                return None; // 프로세서 없는(메모리만 있는) 노드
            }
            let node = numa.NodeNumber as u16;
            let node_cores = cores
                .iter()
                .filter(|core| core.group == best.Group && core.mask & best.Mask as u64 != 0)
                .count() as u32;
            Some(CpuNode {
                node,
                os_node: node,
                group: best.Group,
                mask: best.Mask as u64,
                logical: best.Mask.count_ones(),
                cores: node_cores,
                groups_spanned: count as u16,
            })
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{Core, CpuNode, PinReport};

    pub(super) fn pin_current(_: &CpuNode) -> Result<PinReport, String> {
        Err("thread placement is implemented for Windows only".to_owned())
    }
    pub(super) fn set_current_affinity(_: u16, _: u64) -> bool {
        false
    }
    pub(super) fn pin_handle(_: isize, _: &CpuNode) -> bool {
        false
    }
    pub(super) fn current_thread_id() -> u32 {
        0
    }
    pub(super) fn open_current_thread() -> Option<(u32, isize)> {
        None
    }
    pub(super) fn close(_: isize) {}
    pub(super) fn enumerate_cpu() -> (Vec<CpuNode>, Vec<Core>) {
        (Vec::new(), Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(node: u16, group: u16, mask: u64, cores: u32) -> CpuNode {
        CpuNode {
            node,
            os_node: node,
            group,
            mask,
            logical: mask.count_ones(),
            cores,
            groups_spanned: 1,
        }
    }

    #[test]
    fn fake_split_keeps_smt_siblings_together() {
        // 개발 장비 모양: 노드 하나, 8 코어 16 LP, SMT 형제가 이웃 비트(2k, 2k+1).
        let real = [node(0, 0, 0xffff, 8)];
        let cores: Vec<Core> = (0..8)
            .map(|k| Core {
                group: 0,
                mask: 0b11 << (2 * k),
            })
            .collect();
        let split = split_nodes(&real, &cores, 2);
        assert_eq!(split.len(), 2);
        assert_eq!(
            (split[0].node, split[0].mask, split[0].cores),
            (0, 0x00ff, 4)
        );
        assert_eq!(
            (split[1].node, split[1].mask, split[1].cores),
            (1, 0xff00, 4)
        );
        assert!(
            split
                .iter()
                .all(|entry| entry.os_node == 0 && entry.logical == 8)
        );
        // 분할 없음
        assert_eq!(split_nodes(&real, &cores, 1), real.to_vec());
    }

    #[test]
    fn tile_nodes_parse_blanks_as_detected() {
        assert_eq!(parse_tile_nodes(""), Ok(vec![]));
        assert_eq!(
            parse_tile_nodes("0, 1,-,,1"),
            Ok(vec![Some(0), Some(1), None, None, Some(1)])
        );
        assert!(parse_tile_nodes("0,x").is_err());
    }

    #[test]
    fn pnp_key_is_stripped_to_the_instance_id() {
        let key = r"\REGISTRY\MACHINE\SYSTEM\ControlSet001\Enum\PCI\VEN_1002&DEV_6827&SUBSYS_1\4&385106d8&0&0000\Device Parameters";
        assert_eq!(
            instance_id_from_pnp_key(key).as_deref(),
            Some(r"PCI\VEN_1002&DEV_6827&SUBSYS_1\4&385106d8&0&0000")
        );
        assert_eq!(instance_id_from_pnp_key(r"\REGISTRY\MACHINE\nothing"), None);
    }

    #[test]
    fn a_video_follows_the_node_of_its_tile() {
        let cores = [20, 20];
        assert_eq!(choose_video_node(None, &[1], &[0, 0], &cores), Some(1));
        assert_eq!(choose_video_node(Some(0), &[1], &[3, 3], &cores), Some(1));
        // 두 타일(두 노드)에 걸치면 지금 노드를 유지한다(2 초 TTL 동안 왕복 방지).
        assert_eq!(
            choose_video_node(Some(0), &[0, 1], &[3, 3], &cores),
            Some(0)
        );
        // 둘 다 요구하고 지금 노드가 없으면 요구가 많은 쪽, 같으면 영상이 적은 쪽.
        assert_eq!(
            choose_video_node(None, &[0, 1, 1], &[0, 0], &cores),
            Some(1)
        );
        assert_eq!(choose_video_node(None, &[0, 1], &[4, 1], &cores), Some(1));
        // 요구가 없으면 유지, 처음이면 영상이 가장 적은 노드.
        assert_eq!(choose_video_node(Some(1), &[], &[0, 9], &cores), Some(1));
        assert_eq!(choose_video_node(None, &[], &[5, 2], &cores), Some(1));
    }

    #[test]
    fn a_crowded_node_spills_to_the_least_loaded_one() {
        // GPU 가 한 노드(1)에 몰린 장비: 모든 영상이 노드 1 을 원한다. slack = max(2, 20/4) = 5.
        let cores = [20, 20];
        assert_eq!(choose_video_node(None, &[1], &[0, 4], &cores), Some(1));
        assert_eq!(choose_video_node(None, &[1], &[0, 5], &cores), Some(0));
        // 54 영상을 차례로 놓으면 두 노드에 거의 반씩 갈린다.
        let mut loads = [0usize, 0];
        for _ in 0..54 {
            let node = choose_video_node(None, &[1], &loads, &cores).unwrap();
            loads[node as usize] += 1;
        }
        assert_eq!(loads[0] + loads[1], 54);
        assert!(
            loads[1] >= loads[0] && loads[1] - loads[0] <= 5,
            "{loads:?}"
        );
    }

    #[test]
    fn unknown_device_nodes_do_not_move_a_video() {
        let cores = [20, 20];
        assert_eq!(choose_video_node(Some(0), &[7], &[1, 1], &cores), Some(0));
        assert_eq!(choose_video_node(None, &[], &[], &[]), None);
    }
}

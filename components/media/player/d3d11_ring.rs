/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! D3D11 plane 링 레지스트리 — WR YUV 직접 샘플 경로의 상태기계.
//!
//! gst 스트리밍 스레드(프로듀서)와 렌더러 스레드(media-thread 경유 소비자)가
//! ring_id로 공유하는 전역 레지스트리다. 이 크레이트는 D3D를 전혀 호출하지
//! 않는다 — 텍스처/포인터는 전부 불투명 `usize`이며, 실제 D3D11 Map/Unmap은
//! Task 6(렌더러)이 이 모듈이 반환한 [`ConsumePlan`]을 보고 수행한 뒤
//! [`ConsumeCommit`]으로 결과를 되돌려준다.
//!
//! ## 슬롯 상태기계 (슬롯 하나 = 2~3개 plane이 한 단위로 전이)
//! ```text
//! Unmapped ──(렌더러: 초기 Map)──────────────> Free(mapped)
//! Free ──(프로듀서: claim)────> Writing ──(memcpy 후 publish)──> Filled(여전히 mapped)
//! Filled(최신) ──(렌더러: Unmap)──────────────> Presenting
//! Filled(구형, 최신에 밀림) ──(D3D 호출 없이)──> Free   [여전히 mapped]
//! Presenting(다음 소비 시) ──(렌더러: Map WRITE_DISCARD)──> Free
//! ```
//! - 소비(consume)는 합성(composite)당 한 번: WR이 2~3개 plane을 lock한 뒤
//!   모두 unlock한다. [`D3d11PlaneRings::note_plane_lock_and_plan`]은 링별
//!   lock 카운터가 0→1로 전이할 때만 계획을 반환한다(plane tearing 방지).
//! - FREE 슬롯이 없는 프로듀서는 memcpy 전에 프레임을 드롭한다(카운터만 증가).
//! - 최초 구간(전 슬롯 Unmapped)에는 프로듀서가 [`stage_first_frame`]으로
//!   첫 프레임 바이트를 CPU에 스테이징해 두고, 첫 소비 시
//!   [`ConsumePlan::InitialMapAll`]로 전 슬롯을 한 번에 Map한다.
//!
//! [`stage_first_frame`]: D3d11PlaneRings::stage_first_frame

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// 링당 슬롯 개수(더블/트리플/쿼드 버퍼링 여유분).
pub const SLOT_COUNT: usize = 4;
/// 슬롯당 최대 plane 개수(I420=3, NV12=2).
pub const MAX_PLANES: usize = 3;

/// plane 텍스처의 DXGI 포맷. `R8` = `DXGI_FORMAT_R8_UNORM`(Y/단일 채널),
/// `Rg8` = `DXGI_FORMAT_R8G8_UNORM`(교차 배치 UV, NV12).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RingPlaneFormat {
    R8,
    Rg8,
}

/// 링 슬롯 하나의 plane 하나를 기술하는 정적 정보. `texture`를 제외한
/// 필드들은 링 생성 시 정해지며 링 수명 동안 불변이다.
#[derive(Clone, Copy, Debug)]
pub struct PlaneDesc {
    /// AddRef된 `ID3D11Texture2D`(DYNAMIC). 프로듀서가 생성, 렌더러가
    /// (링 제거 시) 해제한다.
    pub texture: usize,
    pub width: i32,
    pub height: i32,
    pub format: RingPlaneFormat,
    /// 복사할 유효 바이트/행 (width × bytes-per-pixel).
    pub row_bytes: usize,
}

/// [`D3d11PlaneRings::claim_free_slot`]이 반환하는, 프로듀서가 memcpy할
/// 대상 슬롯. `planes[i]`는 `PlaneDesc`가 `Some`인 plane에 한해 매핑된
/// 포인터 정보를 담는다.
pub struct ClaimedSlot {
    pub slot: usize,
    pub planes: [Option<MappedPlane>; MAX_PLANES],
}

/// 매핑된(mapped) plane 메모리 하나. `rows`/`row_bytes`는 링 생성 시
/// [`PlaneDesc`]에서 그대로 가져온 값이고, `data_ptr`/`row_pitch`는 가장
/// 최근 D3D11 Map 결과다.
#[derive(Clone, Copy, Debug)]
pub struct MappedPlane {
    pub data_ptr: usize,
    pub row_pitch: u32,
    pub rows: usize,
    pub row_bytes: usize,
}

/// 소비자(렌더러)가 D3D11 작업을 마친 뒤 [`D3d11PlaneRings::commit_consume`]로
/// 돌려주는 매핑 결과 하나(텍스처 핸들로 어느 slot/plane인지 역참조한다).
#[derive(Clone, Copy, Debug)]
pub struct RemappedPlane {
    pub texture: usize,
    pub data_ptr: usize,
    pub row_pitch: u32,
}

/// [`D3d11PlaneRings::note_plane_lock_and_plan`]이 반환하는 소비 계획.
/// 소비자는 이 계획대로 D3D11 호출(Unmap/Map[+DISCARD]/스테이징 복사)을
/// 수행한 뒤 [`ConsumeCommit`]으로 결과를 커밋해야 한다.
#[derive(Clone, Debug)]
pub enum ConsumePlan {
    /// 최초 소비: 전 슬롯이 아직 Unmapped다. 소비자는 전 슬롯의 모든
    /// plane을 Map하고, `staged`가 있으면 그 바이트를 슬롯 0에 복사한
    /// 뒤 슬롯 0을 Presenting으로 커밋해야 한다.
    InitialMapAll {
        slots: [[Option<PlaneDesc>; MAX_PLANES]; SLOT_COUNT],
        staged: Option<Vec<Vec<u8>>>,
    },
    /// 정상 진행: 최신 Filled 슬롯(`filled_slot`)을 Unmap해 Presenting으로
    /// 삼고, 이전 Presenting 슬롯을 Map(WRITE_DISCARD)해 Free로 되돌린다.
    /// 그 외 더 오래된 Filled 슬롯이 있었다면 레지스트리가 D3D 작업 없이
    /// 내부적으로 Free 처리했다(이 계획에는 나타나지 않는다).
    Advance {
        /// `filled_slot`의 plane 텍스처들 — Unmap 대상.
        unmap: Vec<usize>,
        /// 이전 Presenting 슬롯의 plane 텍스처들 — Map(DISCARD) 대상.
        map: Vec<usize>,
        filled_slot: usize,
    },
}

/// [`ConsumePlan`]에 대응하는 커밋. 각 변형은 소비자가 실제로 수행한 D3D11
/// Map 호출들의 결과(텍스처→(ptr,pitch))를 담는다.
#[derive(Clone, Debug)]
pub enum ConsumeCommit {
    /// `mapped`는 전 슬롯(SLOT_COUNT×MAX_PLANES 이하)의 Map 결과. 커밋 후
    /// 슬롯 0 = Presenting, 나머지 = Free로 확정된다.
    InitialMapAll { mapped: Vec<RemappedPlane> },
    /// `remapped`는 이전 Presenting 슬롯 plane들의 Map(DISCARD) 결과.
    /// `filled_slot`은 계획에서 받은 값을 그대로 되돌려주는 검증용 필드.
    Advance {
        filled_slot: usize,
        remapped: Vec<RemappedPlane>,
    },
}

/// [`D3d11PlaneRings::remove_ring`]으로 제거된 링 하나. 렌더러가 정리(cleanup)에
/// 쓴다: `mapped`는 Unmap이 먼저 필요한 텍스처, `textures`는 전부 Release
/// 대상이다.
pub struct RemovedRing {
    pub textures: Vec<usize>,
    pub mapped: Vec<usize>,
}

/// 슬롯 하나의 내부 상태. 공개 API가 아니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotState {
    /// 아직 한 번도 Map된 적 없음(링 생성 직후 초기값).
    Unmapped,
    /// Map되어 있고 비어 있어 프로듀서가 claim 가능.
    Free,
    /// 프로듀서가 claim해 memcpy 중(publish/abandon 대기).
    Writing,
    /// publish 완료, 아직 소비되지 않음.
    Filled,
    /// 현재 렌더러가 GPU 샘플링에 쓰는 슬롯(Unmapped 상태의 텍스처).
    Presenting,
    /// 직전까지 Presenting이었고, 다음 Advance의 Map(DISCARD) 결과를
    /// 기다리는 중(아직 Free 아님 — claim 불가).
    Remapping,
}

#[derive(Clone, Copy)]
struct SlotInfo {
    state: SlotState,
    /// 정적 plane 기술자(링 생성 시 고정, texture 핸들 불변).
    planes: [Option<PlaneDesc>; MAX_PLANES],
    /// 동적 매핑 정보 (data_ptr, row_pitch). state == Unmapped일 때만 None.
    mapped: [Option<(usize, u32)>; MAX_PLANES],
    /// publish_slot 시점에 부여되는 단조 증가 시퀀스 — "최신 Filled" 판정용.
    filled_seq: u64,
}

struct RingState {
    planes_per_slot: usize,
    slots: [SlotInfo; SLOT_COUNT],
    /// 현재 Presenting 슬롯. 최초 소비(InitialMapAll) 전에는 None.
    presenting_slot: Option<usize>,
    /// 이번 합성에서 lock된 plane 개수(0→1 전이에서만 계획 발급).
    lock_count: u64,
    dropped_frames: u64,
    next_filled_seq: u64,
    /// claim_free_slot이 전부 실패하는 최초 구간에 프로듀서가 스테이징한
    /// 첫 프레임 바이트. InitialMapAll 계획 발급 시 소비(take)된다.
    staged_first_frame: Option<Vec<Vec<u8>>>,
}

/// 링별 상태를 담는 전역 레지스트리. 포이즌된 뮤텍스는 복구해서 계속
/// 쓴다(단일 스레드 패닉이 나머지 스레드의 비디오 파이프라인을 죽이지
/// 않도록 — "poisoned-free").
struct Registry {
    rings: HashMap<u64, RingState>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            rings: HashMap::new(),
        })
    })
}

fn removed_rings() -> &'static Mutex<Vec<RemovedRing>> {
    static REMOVED_RINGS: OnceLock<Mutex<Vec<RemovedRing>>> = OnceLock::new();
    REMOVED_RINGS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 소비자(ANGLE) 디바이스 핸들. media-thread가 시작 시 한 번 publish한다.
static CONSUMER_DEVICE: Mutex<Option<usize>> = Mutex::new(None);

static NEXT_RING_ID: AtomicU64 = AtomicU64::new(1);

/// 포이즌된 뮤텍스를 복구해서 잠근다(한 스레드의 패닉이 다른 스레드의
/// 비디오 처리를 영구히 막지 않도록).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn plane_textures(slot: &SlotInfo) -> Vec<usize> {
    slot.planes.iter().filter_map(|p| p.map(|d| d.texture)).collect()
}

/// 주어진 텍스처 핸들을 가진 (slot, plane) 매핑 정보를 갱신한다. 링 안의
/// 모든 텍스처 핸들은 유일하므로 선형 탐색으로 충분하다(최대 12개).
fn apply_mapped(ring: &mut RingState, texture: usize, data_ptr: usize, row_pitch: u32) {
    for slot in ring.slots.iter_mut() {
        for i in 0..MAX_PLANES {
            if let Some(desc) = slot.planes[i] {
                if desc.texture == texture {
                    slot.mapped[i] = Some((data_ptr, row_pitch));
                }
            }
        }
    }
}

/// 전역 D3D11 plane 링 레지스트리. 모든 메서드는 연관 함수(연관 타입에
/// 인스턴스가 없다) — 내부적으로 [`registry()`] 뮤텍스를 짧게 잠근다.
pub struct D3d11PlaneRings;

impl D3d11PlaneRings {
    /// 소비자(ANGLE) 디바이스를 publish한다. media-thread가 시작 시 호출.
    pub fn set_consumer_device(device: usize) {
        *lock(&CONSUMER_DEVICE) = Some(device);
    }

    /// 현재 publish된 소비자 디바이스(없으면 None — 아직 시작 전).
    pub fn consumer_device() -> Option<usize> {
        *lock(&CONSUMER_DEVICE)
    }

    /// 새 링을 등록한다. `slots`는 프로듀서가 미리 생성한 텍스처들의 정적
    /// 기술자 — 전 슬롯이 Unmapped로 시작한다. 반환된 ring_id는 최초
    /// 생성(epoch=1)을 뜻하며, 프로듀서가 크기 변경 등으로 링을 다시 만들
    /// 때는 새 ring_id를 받게 되므로 세대 추적(ring_epoch)은 프로듀서
    /// 쪽(`VideoFrameD3D11YuvData::ring_epoch`)의 책임이다.
    pub fn create_ring(
        planes_per_slot: usize,
        slots: [[Option<PlaneDesc>; MAX_PLANES]; SLOT_COUNT],
    ) -> u64 {
        let ring_id = NEXT_RING_ID.fetch_add(1, Ordering::Relaxed);
        let ring_slots = std::array::from_fn(|i| SlotInfo {
            state: SlotState::Unmapped,
            planes: slots[i],
            mapped: [None; MAX_PLANES],
            filled_seq: 0,
        });
        let ring = RingState {
            planes_per_slot,
            slots: ring_slots,
            presenting_slot: None,
            lock_count: 0,
            dropped_frames: 0,
            next_filled_seq: 0,
            staged_first_frame: None,
        };
        lock(registry()).rings.insert(ring_id, ring);
        ring_id
    }

    /// 링을 제거한다. 정리에 필요한 텍스처 목록은
    /// [`take_removed_rings`](Self::take_removed_rings)로 나중에 가져간다.
    pub fn remove_ring(ring_id: u64) {
        let removed = {
            let mut reg = lock(registry());
            reg.rings.remove(&ring_id)
        };
        let Some(ring) = removed else {
            return;
        };
        let mut textures = Vec::new();
        let mut mapped = Vec::new();
        for slot in ring.slots.iter() {
            // Presenting 슬롯은 소비자가 이미 Unmap해 GPU 샘플링에 쓰던
            // 것이므로 "mapped"(Unmap 필요) 목록에서 제외한다. Unmapped는
            // 한 번도 Map된 적이 없다. 나머지(Free/Writing/Filled/Remapping)는
            // 매핑돼 있(었)다 — Remapping은 커밋 전이라 불확실하지만
            // 보수적으로 포함한다(초과 Unmap은 D3D 경고 수준).
            let is_mapped = !matches!(slot.state, SlotState::Unmapped | SlotState::Presenting);
            for p in slot.planes.iter().flatten() {
                textures.push(p.texture);
                if is_mapped {
                    mapped.push(p.texture);
                }
            }
        }
        lock(removed_rings()).push(RemovedRing { textures, mapped });
    }

    /// mapped FREE 슬롯을 claim한다. 반환된 포인터에 plane별로 행 복사를
    /// 한 뒤 [`publish_slot`](Self::publish_slot)을 호출해야 한다. FREE가
    /// 없으면 None(드롭 카운터만 증가 — memcpy 전 드롭이라 비용이 없다).
    pub fn claim_free_slot(ring_id: u64) -> Option<ClaimedSlot> {
        let mut reg = lock(registry());
        let ring = reg.rings.get_mut(&ring_id)?;
        let Some(idx) = ring
            .slots
            .iter()
            .position(|s| s.state == SlotState::Free)
        else {
            ring.dropped_frames += 1;
            return None;
        };
        ring.slots[idx].state = SlotState::Writing;
        let planes_per_slot = ring.planes_per_slot;
        let slot = &ring.slots[idx];
        let mut planes: [Option<MappedPlane>; MAX_PLANES] = [None; MAX_PLANES];
        for p in 0..planes_per_slot.min(MAX_PLANES) {
            if let (Some(desc), Some((ptr, pitch))) = (slot.planes[p], slot.mapped[p]) {
                planes[p] = Some(MappedPlane {
                    data_ptr: ptr,
                    row_pitch: pitch,
                    rows: desc.height.max(0) as usize,
                    row_bytes: desc.row_bytes,
                });
            }
        }
        Some(ClaimedSlot { slot: idx, planes })
    }

    /// claim했던 슬롯을 Filled로 표시한다(memcpy 완료 후 호출).
    pub fn publish_slot(ring_id: u64, slot: usize) {
        let mut reg = lock(registry());
        let Some(ring) = reg.rings.get_mut(&ring_id) else {
            return;
        };
        if slot >= SLOT_COUNT || ring.slots[slot].state != SlotState::Writing {
            return;
        }
        ring.next_filled_seq += 1;
        ring.slots[slot].filled_seq = ring.next_filled_seq;
        ring.slots[slot].state = SlotState::Filled;
    }

    /// claim 후 실패(디코드 실패 등) 시 슬롯을 FREE로 되돌린다.
    pub fn abandon_slot(ring_id: u64, slot: usize) {
        let mut reg = lock(registry());
        let Some(ring) = reg.rings.get_mut(&ring_id) else {
            return;
        };
        if slot < SLOT_COUNT && ring.slots[slot].state == SlotState::Writing {
            ring.slots[slot].state = SlotState::Free;
        }
    }

    /// 전 슬롯이 아직 Unmapped인 초기 구간용 — 첫 프레임을 CPU에
    /// 스테이징해 둔다(plane별 연속 바이트). 최초 소비(InitialMapAll) 시
    /// 슬롯 0으로 복사된다. 여러 번 호출하면 마지막 호출 값으로 덮어쓴다.
    pub fn stage_first_frame(ring_id: u64, planes: Vec<Vec<u8>>) {
        let mut reg = lock(registry());
        if let Some(ring) = reg.rings.get_mut(&ring_id) {
            ring.staged_first_frame = Some(planes);
        }
    }

    /// FREE 슬롯이 없어 memcpy 전에 드롭된 프레임 누적 개수.
    pub fn dropped_frames(ring_id: u64) -> u64 {
        let reg = lock(registry());
        reg.rings.get(&ring_id).map(|r| r.dropped_frames).unwrap_or(0)
    }

    /// plane lock 카운트가 0→1로 전이할 때만 소비 계획을 반환한다(합성당
    /// 한 번 — 나머지 plane lock들은 None을 받는다). 새 Filled 프레임이
    /// 없으면(첫 소비 이후) None.
    pub fn note_plane_lock_and_plan(ring_id: u64) -> Option<ConsumePlan> {
        let mut reg = lock(registry());
        let ring = reg.rings.get_mut(&ring_id)?;
        ring.lock_count += 1;
        if ring.lock_count != 1 {
            return None;
        }

        if ring.presenting_slot.is_none() {
            let slots: [[Option<PlaneDesc>; MAX_PLANES]; SLOT_COUNT] =
                std::array::from_fn(|i| ring.slots[i].planes);
            let staged = ring.staged_first_frame.take();
            return Some(ConsumePlan::InitialMapAll { slots, staged });
        }

        let newest = ring
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.state == SlotState::Filled)
            .max_by_key(|(_, s)| s.filled_seq)
            .map(|(i, _)| i);
        let filled_slot = newest?;

        // 최신이 아닌 구형 Filled 슬롯들은 D3D 작업 없이 바로 Free로.
        for i in 0..SLOT_COUNT {
            if i != filled_slot && ring.slots[i].state == SlotState::Filled {
                ring.slots[i].state = SlotState::Free;
            }
        }

        let unmap = plane_textures(&ring.slots[filled_slot]);
        let old_presenting = ring.presenting_slot.expect("checked Some above");
        let map = plane_textures(&ring.slots[old_presenting]);

        ring.slots[filled_slot].state = SlotState::Presenting;
        ring.slots[old_presenting].state = SlotState::Remapping;
        ring.presenting_slot = Some(filled_slot);

        Some(ConsumePlan::Advance {
            unmap,
            map,
            filled_slot,
        })
    }

    /// plane unlock을 알린다(lock_count 감소). WR이 lock한 plane 개수만큼
    /// 짝을 맞춰 호출해야 다음 합성에서 게이트가 정상 리셋된다.
    pub fn note_plane_unlock(ring_id: u64) {
        let mut reg = lock(registry());
        if let Some(ring) = reg.rings.get_mut(&ring_id) {
            ring.lock_count = ring.lock_count.saturating_sub(1);
        }
    }

    /// [`ConsumePlan`]에 따른 D3D11 작업(Unmap/Map/스테이징 복사)을 마친
    /// 뒤 상태를 커밋한다.
    pub fn commit_consume(ring_id: u64, commit: ConsumeCommit) {
        let mut reg = lock(registry());
        let Some(ring) = reg.rings.get_mut(&ring_id) else {
            return;
        };
        match commit {
            ConsumeCommit::InitialMapAll { mapped } => {
                for RemappedPlane {
                    texture,
                    data_ptr,
                    row_pitch,
                } in mapped
                {
                    apply_mapped(ring, texture, data_ptr, row_pitch);
                }
                for i in 0..SLOT_COUNT {
                    ring.slots[i].state = if i == 0 {
                        SlotState::Presenting
                    } else {
                        SlotState::Free
                    };
                }
                ring.presenting_slot = Some(0);
            },
            ConsumeCommit::Advance {
                filled_slot: _,
                remapped,
            } => {
                for RemappedPlane {
                    texture,
                    data_ptr,
                    row_pitch,
                } in remapped
                {
                    apply_mapped(ring, texture, data_ptr, row_pitch);
                }
                for slot in ring.slots.iter_mut() {
                    if slot.state == SlotState::Remapping {
                        slot.state = SlotState::Free;
                    }
                }
            },
        }
    }

    /// 현재 Presenting 슬롯의 plane 텍스처 기술자(정적 정보). lock 반환용.
    pub fn presenting_plane(ring_id: u64, plane: usize) -> Option<PlaneDesc> {
        let reg = lock(registry());
        let ring = reg.rings.get(&ring_id)?;
        let idx = ring.presenting_slot?;
        ring.slots[idx].planes.get(plane).copied().flatten()
    }

    /// [`remove_ring`](Self::remove_ring)으로 제거된 링들을 모아 반환한다
    /// (호출 시점에 비운다). 렌더러가 주기적으로 폴링해 정리한다.
    pub fn take_removed_rings() -> Vec<RemovedRing> {
        std::mem::take(&mut *lock(removed_rings()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(texture: usize) -> PlaneDesc {
        PlaneDesc {
            texture,
            width: 64,
            height: 64,
            format: RingPlaneFormat::R8,
            row_bytes: 64,
        }
    }

    /// PlaneDesc는 브리프 고정 derive 목록(PartialEq 없음)을 그대로 두므로,
    /// 테스트에서는 texture 핸들만으로 슬롯 배열의 동일성을 비교한다.
    fn slots_texture_ids(
        slots: &[[Option<PlaneDesc>; MAX_PLANES]; SLOT_COUNT],
    ) -> [[Option<usize>; MAX_PLANES]; SLOT_COUNT] {
        std::array::from_fn(|s| std::array::from_fn(|p| slots[s][p].map(|d| d.texture)))
    }

    /// planes_per_slot=2인 링용 슬롯 배열. 텍스처 핸들은 가짜 usize(1000+i).
    fn make_slots(base: usize) -> [[Option<PlaneDesc>; MAX_PLANES]; SLOT_COUNT] {
        std::array::from_fn(|slot| {
            std::array::from_fn(|plane| {
                if plane < 2 {
                    Some(desc(base + slot * 10 + plane))
                } else {
                    None
                }
            })
        })
    }

    /// 초기 InitialMapAll 컨슘을 흉내내 링을 "워밍업"한다: 전 슬롯 Map,
    /// 슬롯0을 Presenting으로 커밋한다. 반환값은 각 (slot,plane) 텍스처에
    /// 부여된 가짜 data_ptr 맵(검증용).
    fn warm_up(
        ring_id: u64,
        slots: &[[Option<PlaneDesc>; MAX_PLANES]; SLOT_COUNT],
    ) -> std::collections::HashMap<usize, usize> {
        let plan = D3d11PlaneRings::note_plane_lock_and_plan(ring_id).expect("initial plan");
        let ConsumePlan::InitialMapAll {
            slots: plan_slots,
            staged: _,
        } = plan
        else {
            panic!("expected InitialMapAll plan");
        };
        assert_eq!(slots_texture_ids(&plan_slots), slots_texture_ids(slots));

        let mut ptrs = std::collections::HashMap::new();
        let mut mapped = Vec::new();
        for slot in plan_slots.iter() {
            for p in slot.iter().flatten() {
                let ptr = p.texture * 100;
                ptrs.insert(p.texture, ptr);
                mapped.push(RemappedPlane {
                    texture: p.texture,
                    data_ptr: ptr,
                    row_pitch: 256,
                });
            }
        }
        D3d11PlaneRings::commit_consume(ring_id, ConsumeCommit::InitialMapAll { mapped });
        D3d11PlaneRings::note_plane_unlock(ring_id);
        ptrs
    }

    #[test]
    fn claim_when_all_unmapped_returns_none_and_stage_accepts() {
        let slots = make_slots(1000);
        let ring_id = D3d11PlaneRings::create_ring(2, slots);

        // 전 슬롯 Unmapped 상태이므로 claim은 항상 None + 드롭 카운트 증가.
        assert!(D3d11PlaneRings::claim_free_slot(ring_id).is_none());
        assert_eq!(D3d11PlaneRings::dropped_frames(ring_id), 1);

        // stage_first_frame은 패닉 없이 받아들여져야 하며, claim 동작에는
        // 영향을 주지 않는다(여전히 Unmapped).
        D3d11PlaneRings::stage_first_frame(ring_id, vec![vec![1, 2, 3], vec![4, 5]]);
        assert!(D3d11PlaneRings::claim_free_slot(ring_id).is_none());
        assert_eq!(D3d11PlaneRings::dropped_frames(ring_id), 2);
    }

    #[test]
    fn initial_map_all_then_presenting_slot0() {
        let slots = make_slots(2000);
        let ring_id = D3d11PlaneRings::create_ring(2, slots);

        let staged_bytes = vec![vec![9u8, 9, 9], vec![8u8, 8]];
        D3d11PlaneRings::stage_first_frame(ring_id, staged_bytes.clone());

        let plan = D3d11PlaneRings::note_plane_lock_and_plan(ring_id).expect("initial plan");
        let ConsumePlan::InitialMapAll {
            slots: plan_slots,
            staged,
        } = plan
        else {
            panic!("expected InitialMapAll plan");
        };
        assert_eq!(staged, Some(staged_bytes));
        assert_eq!(slots_texture_ids(&plan_slots), slots_texture_ids(&slots));

        let mut mapped = Vec::new();
        for slot in plan_slots.iter() {
            for p in slot.iter().flatten() {
                mapped.push(RemappedPlane {
                    texture: p.texture,
                    data_ptr: p.texture * 100,
                    row_pitch: 256,
                });
            }
        }
        D3d11PlaneRings::commit_consume(ring_id, ConsumeCommit::InitialMapAll { mapped });
        D3d11PlaneRings::note_plane_unlock(ring_id);

        let presenting = D3d11PlaneRings::presenting_plane(ring_id, 0).expect("slot0 plane0");
        assert_eq!(presenting.texture, slots[0][0].unwrap().texture);

        // 나머지 슬롯들은 Free이므로 claim이 성공해야 하고, 슬롯0은 절대
        // 반환되지 않는다(Presenting).
        let claimed = D3d11PlaneRings::claim_free_slot(ring_id).expect("free slot available");
        assert_ne!(claimed.slot, 0);
    }

    #[test]
    fn claim_fill_publish_then_first_lock_advances() {
        let slots = make_slots(3000);
        let ring_id = D3d11PlaneRings::create_ring(2, slots);
        warm_up(ring_id, &slots);

        let claimed = D3d11PlaneRings::claim_free_slot(ring_id).expect("free slot");
        let slot_idx = claimed.slot;
        assert_ne!(slot_idx, 0);
        D3d11PlaneRings::publish_slot(ring_id, slot_idx);

        let plan = D3d11PlaneRings::note_plane_lock_and_plan(ring_id).expect("advance plan");
        let ConsumePlan::Advance {
            unmap,
            map,
            filled_slot,
        } = plan
        else {
            panic!("expected Advance plan");
        };
        assert_eq!(filled_slot, slot_idx);

        let expected_unmap: Vec<usize> = slots[slot_idx]
            .iter()
            .flatten()
            .map(|d| d.texture)
            .collect();
        let expected_map: Vec<usize> = slots[0].iter().flatten().map(|d| d.texture).collect();
        assert_eq!(unmap, expected_unmap);
        assert_eq!(map, expected_map);

        let remapped = map
            .iter()
            .map(|&texture| RemappedPlane {
                texture,
                data_ptr: texture * 200,
                row_pitch: 512,
            })
            .collect();
        D3d11PlaneRings::commit_consume(
            ring_id,
            ConsumeCommit::Advance {
                filled_slot,
                remapped,
            },
        );
        D3d11PlaneRings::note_plane_unlock(ring_id);

        let presenting = D3d11PlaneRings::presenting_plane(ring_id, 0).expect("new presenting");
        assert_eq!(presenting.texture, slots[slot_idx][0].unwrap().texture);
    }

    #[test]
    fn two_filled_latest_wins_older_returns_free_without_d3d() {
        let slots = make_slots(4000);
        let ring_id = D3d11PlaneRings::create_ring(2, slots);
        let ptrs = warm_up(ring_id, &slots);

        // 슬롯0=Presenting. 나머지 1,2,3을 모두 claim해서 Free 슬롯을
        // 없앤 뒤, 1은 publish(구형/older), 2는 publish(신형/newer),
        // 3은 Writing 상태로 남겨(둘 다 아님) 스윕 대상에서 제외한다.
        let c1 = D3d11PlaneRings::claim_free_slot(ring_id).expect("slot1");
        assert_eq!(c1.slot, 1);
        D3d11PlaneRings::publish_slot(ring_id, 1);

        let c2 = D3d11PlaneRings::claim_free_slot(ring_id).expect("slot2");
        assert_eq!(c2.slot, 2);
        D3d11PlaneRings::publish_slot(ring_id, 2);

        let c3 = D3d11PlaneRings::claim_free_slot(ring_id).expect("slot3");
        assert_eq!(c3.slot, 3);
        // slot3은 publish하지 않음 (Writing 유지)

        // 지금 Free 슬롯이 없어야 한다.
        assert!(D3d11PlaneRings::claim_free_slot(ring_id).is_none());

        let plan = D3d11PlaneRings::note_plane_lock_and_plan(ring_id).expect("advance plan");
        let ConsumePlan::Advance {
            unmap,
            map,
            filled_slot,
        } = plan
        else {
            panic!("expected Advance plan");
        };
        // 최신(slot2)이 승리 - unmap 목록에는 slot2 텍스처만, slot1 텍스처는 없다.
        assert_eq!(filled_slot, 2);
        let slot1_textures: Vec<usize> = slots[1].iter().flatten().map(|d| d.texture).collect();
        let slot2_textures: Vec<usize> = slots[2].iter().flatten().map(|d| d.texture).collect();
        for t in &slot1_textures {
            assert!(!unmap.contains(t));
            assert!(!map.contains(t));
        }
        for t in &slot2_textures {
            assert!(unmap.contains(t));
        }

        // 구형 Filled(slot1)은 D3D 작업 없이 즉시 Free로 회귀했어야 하며,
        // 지금 유일한 Free 슬롯이므로 claim이 결정적으로 slot1을 반환한다.
        let reclaimed = D3d11PlaneRings::claim_free_slot(ring_id).expect("slot1 freed");
        assert_eq!(reclaimed.slot, 1);
        // 포인터는 최초 워밍업 때 매핑된 값 그대로(재매핑 없음).
        let plane0 = reclaimed.planes[0].expect("plane0 mapped");
        assert_eq!(plane0.data_ptr, ptrs[&slots[1][0].unwrap().texture]);
    }

    #[test]
    fn second_plane_lock_same_composite_no_advance() {
        let slots = make_slots(5000);
        let ring_id = D3d11PlaneRings::create_ring(2, slots);
        warm_up(ring_id, &slots);

        let claimed = D3d11PlaneRings::claim_free_slot(ring_id).expect("free slot");
        D3d11PlaneRings::publish_slot(ring_id, claimed.slot);

        // 첫 lock(plane0) - 0->1 게이트 통과, 계획 발급.
        let plan = D3d11PlaneRings::note_plane_lock_and_plan(ring_id).expect("first lock plans");
        let ConsumePlan::Advance {
            map, filled_slot, ..
        } = plan
        else {
            panic!("expected Advance plan");
        };
        let remapped = map
            .iter()
            .map(|&texture| RemappedPlane {
                texture,
                data_ptr: texture * 300,
                row_pitch: 512,
            })
            .collect();
        D3d11PlaneRings::commit_consume(
            ring_id,
            ConsumeCommit::Advance {
                filled_slot,
                remapped,
            },
        );

        // 같은 합성 내 두번째 plane lock(plane1) - lock_count 1->2, 계획 없음.
        let second = D3d11PlaneRings::note_plane_lock_and_plan(ring_id);
        assert!(second.is_none());

        // 두 lock 모두 unlock (합성 종료).
        D3d11PlaneRings::note_plane_unlock(ring_id);
        D3d11PlaneRings::note_plane_unlock(ring_id);
    }

    #[test]
    fn remove_ring_reports_textures_and_mapped_for_cleanup() {
        // 케이스 1: 전 슬롯 Unmapped인 링 제거 — mapped는 비어야 한다.
        let slots_a = make_slots(7000);
        let ring_a = D3d11PlaneRings::create_ring(2, slots_a);
        D3d11PlaneRings::remove_ring(ring_a);

        // 케이스 2: 워밍업된 링(슬롯0 Presenting=이미 Unmap됨, 나머지 Free).
        let slots_b = make_slots(8000);
        let ring_b = D3d11PlaneRings::create_ring(2, slots_b);
        warm_up(ring_b, &slots_b);
        D3d11PlaneRings::remove_ring(ring_b);

        // 제거 후 프로듀서/소비자 API는 전부 no-op이어야 한다.
        assert!(D3d11PlaneRings::claim_free_slot(ring_b).is_none());
        assert!(D3d11PlaneRings::note_plane_lock_and_plan(ring_b).is_none());

        let removed = D3d11PlaneRings::take_removed_rings();
        // 다른 테스트와 병렬 실행될 수 있으므로 개수 대신 내용으로 찾는다.
        let all_b: Vec<usize> = slots_b
            .iter()
            .flat_map(|s| s.iter().flatten().map(|d| d.texture))
            .collect();
        let entry_a = removed
            .iter()
            .find(|r| r.textures.contains(&slots_a[0][0].unwrap().texture))
            .expect("ring_a cleanup entry");
        assert_eq!(entry_a.textures.len(), 8); // 4슬롯×2plane 전부 Release 대상
        assert!(entry_a.mapped.is_empty()); // 한 번도 Map 안 됨

        let entry_b = removed
            .iter()
            .find(|r| r.textures.contains(&all_b[0]))
            .expect("ring_b cleanup entry");
        assert_eq!(entry_b.textures.len(), 8);
        // Presenting(슬롯0)은 이미 Unmap됐으므로 mapped 목록에서 제외,
        // Free인 슬롯 1~3의 6개 텍스처만 Unmap 대상이다.
        assert_eq!(entry_b.mapped.len(), 6);
        for d in slots_b[0].iter().flatten() {
            assert!(!entry_b.mapped.contains(&d.texture));
        }

        // take는 소진형 — 두 번째 호출엔 방금 항목들이 없어야 한다.
        let again = D3d11PlaneRings::take_removed_rings();
        assert!(
            !again
                .iter()
                .any(|r| r.textures.contains(&all_b[0])
                    || r.textures.contains(&slots_a[0][0].unwrap().texture))
        );
    }

    #[test]
    fn unlock_resets_gate_next_lock_advances() {
        let slots = make_slots(6000);
        let ring_id = D3d11PlaneRings::create_ring(2, slots);
        warm_up(ring_id, &slots);

        let c1 = D3d11PlaneRings::claim_free_slot(ring_id).expect("slot for gen1");
        D3d11PlaneRings::publish_slot(ring_id, c1.slot);

        // 합성 1: lock(plane0)->계획, commit, lock(plane1)->None, 두 unlock.
        let plan1 = D3d11PlaneRings::note_plane_lock_and_plan(ring_id).expect("gen1 plan");
        let ConsumePlan::Advance {
            map, filled_slot, ..
        } = plan1
        else {
            panic!("expected Advance plan");
        };
        assert_eq!(filled_slot, c1.slot);
        let remapped = map
            .iter()
            .map(|&texture| RemappedPlane {
                texture,
                data_ptr: texture * 400,
                row_pitch: 512,
            })
            .collect();
        D3d11PlaneRings::commit_consume(
            ring_id,
            ConsumeCommit::Advance {
                filled_slot,
                remapped,
            },
        );
        assert!(D3d11PlaneRings::note_plane_lock_and_plan(ring_id).is_none());
        D3d11PlaneRings::note_plane_unlock(ring_id);
        D3d11PlaneRings::note_plane_unlock(ring_id);

        // 합성 2: 게이트가 리셋되어 새 Filled 슬롯에 대해 다시 계획이 나와야 한다.
        let c2 = D3d11PlaneRings::claim_free_slot(ring_id).expect("slot for gen2");
        assert_ne!(c2.slot, c1.slot);
        D3d11PlaneRings::publish_slot(ring_id, c2.slot);

        let plan2 = D3d11PlaneRings::note_plane_lock_and_plan(ring_id).expect("gen2 plan");
        let ConsumePlan::Advance {
            filled_slot: filled_slot2,
            map: map2,
            ..
        } = plan2
        else {
            panic!("expected Advance plan");
        };
        assert_eq!(filled_slot2, c2.slot);
        // 이번엔 이전 presenting(=c1.slot)이 re-map 대상이어야 한다.
        let expected_map2: Vec<usize> = slots[c1.slot].iter().flatten().map(|d| d.texture).collect();
        assert_eq!(map2, expected_map2);
        D3d11PlaneRings::note_plane_unlock(ring_id);
    }
}

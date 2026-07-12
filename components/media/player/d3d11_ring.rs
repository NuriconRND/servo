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
//! - FREE 슬롯이 없으면 프로듀서는 프레임 바이트를 버리되(memcpy 전 — 카운터만
//!   증가), 드롭 대신 직전 Presenting 슬롯을 그대로 재제시해 스트림을 유지한다
//!   (bf70293c4 이후 — build_frame의 None 리턴이 파이프라인을 죽이던 문제 수정).
//! - 최초 구간(전 슬롯 Unmapped)에는 프로듀서가 [`stage_first_frame`]으로
//!   첫 프레임 바이트를 CPU에 스테이징해 두고, 첫 소비 시
//!   [`ConsumePlan::InitialMapAll`]로 전 슬롯을 한 번에 Map한다(슬롯 0은
//!   스테이징 복사 후 다시 Unmap해 Presenting=unmapped 불변식을 지킨다).
//!
//! [`stage_first_frame`]: D3d11PlaneRings::stage_first_frame

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use log::warn;

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
///
/// # 커밋 계약(반드시 지켜야 함)
/// `note_plane_lock_and_plan`이 `Some(plan)`을 반환하는 순간, 관련 슬롯은
/// 이미 상태기계상 전이돼 있다(`Advance`라면 D3D 호출 전에 old-presenting
/// 슬롯을 내부적으로 `Remapping` 상태로 옮겨둔다 — Task 6이 아직 아무
/// D3D11 작업도 하지 않은 시점에!). 그러므로 **`Some`으로 받은 plan
/// 하나당 정확히 한 번** [`D3d11PlaneRings::commit_consume`]을 호출할
/// 의무가 있다. D3D 작업이 부분적으로(또는 전부) 실패해도 이 의무는
/// 없어지지 않는다 — 커밋을 건너뛰면 전이된 슬롯이 영원히
/// `Remapping`(또는 초기 구간에서는 미확정 상태)에 갇혀 claim 불가능해
/// 지고, 그만큼 링이 실질적으로 줄어든다(더블/트리플 버퍼링 여유가
/// 영구히 사라짐).
///
/// 실패 시 무엇을 커밋해야 하는지(`ConsumeCommit` 기준):
/// - `InitialMapAll`: 슬롯 0의 plane은 스테이징 복사 후 다시 Unmap되므로
///   설계상 `mapped`에서 제외한다(Presenting=unmapped 불변식). 슬롯 1..N에서
///   Map에 실패한 plane도 `mapped: Vec<RemappedPlane>`에서 빼고, 성공한
///   plane만 담아 커밋한다. 심지어 전부 실패했다면 `mapped: Vec::new()`로
///   커밋한다 — 그래도 슬롯 상태 전이(슬롯0→Presenting, 나머지→Free)는
///   일어나야 한다(그러지 않으면 링 전체가 영원히 Unmapped로 wedge된다).
/// - `Advance`: 재-Map(WRITE_DISCARD)에 실패한 텍스처는 `remapped`에서
///   빼고 커밋한다(`remapped: Vec::new()`도 유효한 커밋이다). `filled_slot`
///   은 plan에서 받은 값을 그대로 되돌려주면 된다(성공/실패와 무관한
///   검증용 필드). 재-Map 실패 plane은 슬롯이 Free로 전이되기 전에
///   레지스트리가 그 `mapped` 포인터를 None으로 무효화하므로(낡은 포인터가
///   이미 Unmap된 메모리를 가리키는 것을 원천 차단), 다음 claim은 그 plane을
///   건너뛰고 copy도 스킵한다 — 스테일-내용이지만 안전한 프레임이 된다.
#[derive(Clone, Debug)]
pub enum ConsumePlan {
    /// 최초 소비: 전 슬롯이 아직 Unmapped다. 소비자는 전 슬롯의 모든
    /// plane을 Map하고, `staged`가 있으면 그 바이트를 슬롯 0에 복사한 뒤
    /// 슬롯 0을 다시 Unmap하고(Presenting=unmapped 불변식) 슬롯 0의 plane을
    /// 커밋 `mapped`에서 제외해야 한다 — 커밋 후 슬롯 0 = Presenting(unmapped),
    /// 슬롯 1..N = Free(mapped)로 확정된다.
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
    /// `mapped`는 슬롯 1..N의 Map 결과(슬롯 0은 스테이징 복사 후 다시 Unmap
    /// 되므로 제외 — Presenting=unmapped 불변식). 커밋 후 슬롯 0 = Presenting,
    /// 나머지 = Free로 확정된다.
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
    /// 없으면 None(카운터만 증가 — memcpy 전이라 비용 없음). 이때 프로듀서는
    /// 프레임을 드롭하지 않고 직전 Presenting 슬롯을 재제시한다(bf70293c4 이후).
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
    ///
    /// # 커밋 의무
    /// `Some(plan)`을 반환하는 이 호출 자체가 이미 관련 슬롯 상태를
    /// 전이시킨다 — `Advance`의 경우 old-presenting 슬롯을 D3D 작업 전에
    /// `Remapping`으로 옮겨둔다(eager 전이; [`ConsumePlan`] 문서 참고).
    /// 따라서 `Some`을 받은 호출자는 그 뒤의 D3D 작업 성공 여부와
    /// 무관하게 [`commit_consume`](Self::commit_consume)을 정확히 한 번
    /// 호출해야 한다 — 건너뛰면 그 슬롯이 영구히 claim 불가능해진다
    /// (링이 실질적으로 축소됨, "wedge").
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
    ///
    /// # 반드시 커밋해야 함(never-skip 계약)
    /// [`note_plane_lock_and_plan`](Self::note_plane_lock_and_plan)이
    /// `Some`을 반환하면 그 시점에 이미 슬롯 상태가 전이돼 있다 — 이
    /// 함수를 호출하지 않으면 그 슬롯은 영원히 claim 불가능한 상태로
    /// 링에 남는다(자세한 내용/실패 시 커밋 방법은 [`ConsumePlan`] 문서
    /// 참고). D3D 작업이 실패해도 반드시 호출하되, 실패한 plane은 해당
    /// `mapped`/`remapped` 벡터에서 제외해 부분 커밋하면 된다.
    ///
    /// # 상태 가드(오용/중복 커밋 방지)
    /// - `InitialMapAll` 커밋은 링이 아직 초기 구간일 때만
    ///   (`presenting_slot.is_none()`) 적용된다. 이미 최초 Advance를 거친
    ///   뒤 뒤늦게 도착한 stale/중복 `InitialMapAll` 커밋은 경고 로그만
    ///   남기고 상태를 전혀 바꾸지 않는다(no-op).
    /// - `InitialMapAll` 커밋은 `Writing` 중인 슬롯을 절대 Free/Presenting
    ///   으로 전이시키지 않는다(현재 구현상 초기 구간에는 Free 슬롯이
    ///   없어 Writing 슬롯이 있을 수 없지만, 상태기계 변경에 대비한
    ///   방어적 가드다). 그런 슬롯이 있으면 경고 후 그 슬롯만 제외한다.
    /// - `Advance` 커밋은 실제로 `Remapping` 상태인 슬롯에 한해서만
    ///   mapped 포인터를 반영하고 Free로 전이시킨다(텍스처 핸들이 다른
    ///   상태의 슬롯과 우연히 일치해도 영향받지 않는다). `Remapping`
    ///   슬롯이 하나도 없으면(이미 처리된 중복 Advance 커밋) 경고만
    ///   남기고 no-op이다. 정상 경로에서는 `Remapping` 슬롯이 최대
    ///   하나여야 한다는 불변조건을 `debug_assert`로 검증한다.
    pub fn commit_consume(ring_id: u64, commit: ConsumeCommit) {
        let mut reg = lock(registry());
        let Some(ring) = reg.rings.get_mut(&ring_id) else {
            return;
        };
        match commit {
            ConsumeCommit::InitialMapAll { mapped } => {
                // stale/중복 가드: 이미 최초 Advance를 거쳐 초기 구간을
                // 벗어난 링에는 적용하지 않는다 — 슬롯 0을 Presenting으로
                // 강제하면 이미 진행 중인 링 상태를 덮어써 버린다.
                if ring.presenting_slot.is_some() {
                    warn!(
                        "d3d11_ring: stale InitialMapAll commit for ring {ring_id} ignored \
                         (presenting_slot={:?}, already past initial phase)",
                        ring.presenting_slot
                    );
                    return;
                }
                // 방어적 가드: 초기 구간에는 이론상 Writing 슬롯이 있을 수
                // 없다(Free 슬롯이 없으면 claim_free_slot이 항상 실패하기
                // 때문). 그래도 프로듀서가 memcpy 중인 슬롯을 Free로
                // 되돌려 재-claim 가능하게 만드는 사고를 막기 위해 명시
                // 적으로 확인한다.
                let writing: Vec<usize> = ring
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.state == SlotState::Writing)
                    .map(|(i, _)| i)
                    .collect();
                if !writing.is_empty() {
                    warn!(
                        "d3d11_ring: InitialMapAll commit for ring {ring_id} found slot(s) \
                         {writing:?} already Writing — leaving them untouched"
                    );
                }

                for RemappedPlane {
                    texture,
                    data_ptr,
                    row_pitch,
                } in mapped
                {
                    apply_mapped(ring, texture, data_ptr, row_pitch);
                }
                for i in 0..SLOT_COUNT {
                    if writing.contains(&i) {
                        continue;
                    }
                    ring.slots[i].state = if i == 0 {
                        SlotState::Presenting
                    } else {
                        SlotState::Free
                    };
                }
                // 슬롯 0이 Writing이라 위에서 건너뛰었다면 아직 Presenting이
                // 아니므로 presenting_slot을 확정하지 않는다(다음 정상
                // InitialMapAll 커밋을 기다린다).
                if !writing.contains(&0) {
                    ring.presenting_slot = Some(0);
                }
            },
            ConsumeCommit::Advance {
                filled_slot: _,
                remapped,
            } => {
                let remapping_slots: Vec<usize> = ring
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.state == SlotState::Remapping)
                    .map(|(i, _)| i)
                    .collect();
                debug_assert!(
                    remapping_slots.len() <= 1,
                    "d3d11_ring: at most one slot should be Remapping at a time, found {:?}",
                    remapping_slots
                );
                if remapping_slots.is_empty() {
                    // stale/중복 가드: Remapping 슬롯이 없다는 것은 이미
                    // 처리된(또는 잘못된 시점에 도착한) Advance 커밋이란
                    // 뜻이다. mapped 포인터를 엉뚱한 슬롯(텍스처 핸들이
                    // 우연히 일치하는 Free/Filled/Writing 슬롯 등)에 잘못
                    // 반영하지 않도록 아무 것도 건드리지 않는다.
                    warn!(
                        "d3d11_ring: stale/duplicate Advance commit for ring {ring_id} ignored \
                         (no slot in Remapping state)"
                    );
                    return;
                }

                // 재-Map(WRITE_DISCARD)에 성공한 텍스처 집합. 여기 없는 plane은
                // 재-Map 실패라 낡은 mapped 포인터가 이미 Unmap된 메모리를 가리킨다.
                let remapped_textures: Vec<usize> = remapped.iter().map(|r| r.texture).collect();

                for RemappedPlane {
                    texture,
                    data_ptr,
                    row_pitch,
                } in remapped
                {
                    // Remapping 슬롯의 plane에 한해서만 반영한다 — 텍스처
                    // 핸들이 우연히 다른 상태의 슬롯과 겹치는 경우를
                    // 원천 차단한다.
                    for &idx in &remapping_slots {
                        if let Some(plane_idx) = ring.slots[idx]
                            .planes
                            .iter()
                            .position(|p| p.map(|d| d.texture) == Some(texture))
                        {
                            ring.slots[idx].mapped[plane_idx] = Some((data_ptr, row_pitch));
                        }
                    }
                }
                // 재-Map 실패 plane의 스테일 포인터 무효화: remapped에 없는 Remapping
                // 슬롯 plane의 mapped를 Free 전이 전에 None으로 지운다. 그러지 않으면
                // claim_free_slot이 이미 Unmap된 VA를 가리키는 낡은 포인터를 프로듀서
                // 에게 건네 그 위로 memcpy(UB)한다. claim_free_slot은 mapped=None인
                // plane을 건너뛰고 copy_planes도 그 plane을 스킵하므로, 스테일-내용
                // 이지만 안전한 프레임이 나간다.
                for &idx in &remapping_slots {
                    for plane_idx in 0..MAX_PLANES {
                        if let Some(desc) = ring.slots[idx].planes[plane_idx] {
                            if !remapped_textures.contains(&desc.texture) {
                                ring.slots[idx].mapped[plane_idx] = None;
                            }
                        }
                    }
                }
                for &idx in &remapping_slots {
                    ring.slots[idx].state = SlotState::Free;
                }
            },
        }
    }

    /// 현재 Presenting 슬롯의 plane 텍스처 기술자(정적 정보). lock 반환용.
    ///
    /// 주의(eager 상태 전이): `note_plane_lock_and_plan`의 `Advance` 분기는
    /// D3D 작업 전에 이미 `presenting_slot`을 새 슬롯으로 옮겨둔다(plan
    /// 발급 시점에 상태가 전이됨, [`ConsumePlan`] 문서 참고). 즉
    /// `commit_consume`이 아직 호출되기 전(D3D11 Unmap이 실제로는 아직
    /// 실행되지 않았을 수도 있는 시점)이라도 이 함수는 이미 새
    /// Presenting 슬롯의 plane 기술자를 반환할 수 있다.
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

    /// 초기 InitialMapAll 컨슘을 흉내내 링을 "워밍업"한다: 슬롯 1..N을 Map,
    /// 슬롯0을 Presenting으로 커밋한다. 실제 consume_plan과 마찬가지로 슬롯0의
    /// plane은 스테이징 복사 후 다시 Unmap되므로 `mapped`에서 제외한다
    /// (Presenting=unmapped 불변식). 반환값은 매핑된 (slot,plane) 텍스처의 가짜
    /// data_ptr 맵(검증용, 슬롯0 제외).
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
        for (slot_idx, slot) in plan_slots.iter().enumerate() {
            // 슬롯0은 unmapped로 커밋(Presenting=unmapped) — mapped에서 제외.
            if slot_idx == 0 {
                continue;
            }
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

        // 소비자(consume_plan)는 슬롯0을 스테이징 복사 후 다시 Unmap하고 커밋
        // `mapped`에서 제외한다 — 슬롯0 = Presenting(unmapped), 슬롯1..N만 mapped.
        let mut mapped = Vec::new();
        for (slot_idx, slot) in plan_slots.iter().enumerate() {
            if slot_idx == 0 {
                continue;
            }
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

        // 슬롯0 = Presenting(unmapped): presenting_plane은 정적 텍스처 기술자를
        // 반환하고(방향/크기용), 슬롯0은 절대 claim되지 않는다.
        let presenting = D3d11PlaneRings::presenting_plane(ring_id, 0).expect("slot0 plane0");
        assert_eq!(presenting.texture, slots[0][0].unwrap().texture);

        // 슬롯1..N = Free(mapped): 정확히 3개가 claim되고, 슬롯0은 절대 나오지
        // 않으며, 각 슬롯의 plane은 커밋한 매핑 포인터(texture*100)를 싣는다.
        let mut claimed_slots = Vec::new();
        while let Some(c) = D3d11PlaneRings::claim_free_slot(ring_id) {
            assert_ne!(c.slot, 0, "슬롯0(Presenting)은 claim되면 안 된다");
            let p0 = c.planes[0].expect("claimed slot plane0 mapped");
            assert_eq!(p0.data_ptr, slots[c.slot][0].unwrap().texture * 100);
            claimed_slots.push(c.slot);
        }
        claimed_slots.sort();
        assert_eq!(claimed_slots, vec![1, 2, 3]);
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

    /// Finding 2(문서화된 커밋 계약) 회귀 테스트: `note_plane_lock_and_plan`이
    /// `Some(Advance)`를 반환하면 그 호출 자체가 이미 old-presenting 슬롯을
    /// `Remapping`으로 옮겨 버린다(문서 참고) — 만약 소비자가
    /// `commit_consume`을 호출하지 않으면 그 슬롯은 영원히 wedge된다.
    /// 여기서는 "commit을 건너뛰면 실제로 어떻게 되는가"를 명시적으로
    /// 검증해서, 앞으로 이 계약이 깨지면(예: guard가 실수로 wedge된
    /// 슬롯을 재사용 가능하게 만들면) 테스트가 즉시 실패하게 한다.
    #[test]
    fn uncommitted_plan_wedges_remapping_slot_documented() {
        let slots = make_slots(9000);
        let ring_id = D3d11PlaneRings::create_ring(2, slots);
        warm_up(ring_id, &slots); // 슬롯0 = Presenting, 1~3 = Free.

        let claimed = D3d11PlaneRings::claim_free_slot(ring_id).expect("free slot");
        let claimed_slot = claimed.slot;
        assert_ne!(claimed_slot, 0);
        D3d11PlaneRings::publish_slot(ring_id, claimed_slot);

        // Advance plan을 발급만 하고 절대 commit_consume을 호출하지
        // 않는다. 이 호출 자체가 이미 슬롯0을 Remapping으로,
        // claimed_slot을 Presenting으로 옮겨 버린다(eager 전이) —
        // 아직 D3D11 작업(Unmap/Map)은 전혀 일어나지 않았는데도.
        let plan = D3d11PlaneRings::note_plane_lock_and_plan(ring_id).expect("advance plan issued");
        let ConsumePlan::Advance {
            filled_slot: planned_slot,
            ..
        } = plan
        else {
            panic!("expected Advance plan");
        };
        assert_eq!(planned_slot, claimed_slot);

        // commit 없이 unlock만 호출(합성 종료를 흉내) — 실사용에서는
        // 여기서 반드시 commit_consume을 호출해야 하지만, 이 테스트는
        // "안 부르면 어떻게 되는가"를 검증하는 것이 목적이다.
        D3d11PlaneRings::note_plane_unlock(ring_id);

        // 다음 lock/plan 사이클: 게이트는 리셋됐지만 소비할 새 Filled
        // 슬롯이 없다(claimed_slot은 이미 Presenting, 슬롯0은 Remapping
        // 이라 Filled가 아니다) — None이어야 한다.
        let second = D3d11PlaneRings::note_plane_lock_and_plan(ring_id);
        assert!(
            second.is_none(),
            "no Filled slot exists after the uncommitted plan, so re-locking must yield None"
        );

        // 슬롯0(Remapping으로 wedge됨)은 claim_free_slot이 절대 반환하지
        // 않는다 — 남은 Free 슬롯(2,3)을 모두 소진해도 마찬가지다.
        let mut claimed_slots = Vec::new();
        while let Some(c) = D3d11PlaneRings::claim_free_slot(ring_id) {
            assert_ne!(c.slot, 0, "wedged Remapping slot must never be claimable");
            claimed_slots.push(c.slot);
        }
        assert!(!claimed_slots.contains(&0));

        // remove_ring은 wedge된 Remapping 슬롯을 "mapped"(Unmap 필요)에
        // 보수적으로 포함해야 한다(:293-296 주석에 기술된 동작).
        D3d11PlaneRings::remove_ring(ring_id);
        let removed = D3d11PlaneRings::take_removed_rings();
        let slot0_textures: Vec<usize> = slots[0].iter().flatten().map(|d| d.texture).collect();
        let entry = removed
            .iter()
            .find(|r| r.textures.contains(&slot0_textures[0]))
            .expect("ring cleanup entry");
        for t in &slot0_textures {
            assert!(
                entry.mapped.contains(t),
                "wedged Remapping slot must be conservatively reported as mapped"
            );
        }
    }

    /// Finding 1 회귀 테스트: 링이 이미 최초 Advance를 거친(초기 구간을
    /// 벗어난) 뒤에 도착한 stale/오용 `InitialMapAll` 커밋은 아무 상태도
    /// 바꾸지 않아야 한다 — 특히 마침 memcpy 중(Writing)인 슬롯을
    /// Free로 스탬프해 재-claim 가능하게 만들면 안 된다.
    #[test]
    fn stale_initial_commit_after_advance_is_rejected() {
        let slots = make_slots(10000);
        let ring_id = D3d11PlaneRings::create_ring(2, slots);
        warm_up(ring_id, &slots);

        // 링을 초기 구간 너머로 정상적으로 한 번 Advance시킨다(커밋 포함).
        let c1 = D3d11PlaneRings::claim_free_slot(ring_id).expect("slot for advance");
        D3d11PlaneRings::publish_slot(ring_id, c1.slot);
        let plan = D3d11PlaneRings::note_plane_lock_and_plan(ring_id).expect("advance plan");
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
                data_ptr: texture * 700,
                row_pitch: 256,
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

        let presenting_before =
            D3d11PlaneRings::presenting_plane(ring_id, 0).expect("presenting after advance");
        assert_eq!(presenting_before.texture, slots[c1.slot][0].unwrap().texture);

        // 프로듀서가 memcpy 중인(Writing) 슬롯을 하나 만들어 둔다.
        let writing = D3d11PlaneRings::claim_free_slot(ring_id).expect("writing slot");
        let writing_slot = writing.slot;
        assert_ne!(writing_slot, c1.slot); // Presenting 슬롯은 claim 대상이 아님.

        // stale InitialMapAll 커밋: 전 슬롯의 텍스처에 말도 안 되는
        // data_ptr(0xdead)을 실어 보낸다. 링은 이미 초기 구간을 벗어났으
        // 므로(presenting_slot.is_some()) 이 커밋은 경고만 남기고
        // 완전히 no-op이어야 한다.
        let bogus_mapped: Vec<RemappedPlane> = slots
            .iter()
            .flat_map(|s| {
                s.iter().flatten().map(|d| RemappedPlane {
                    texture: d.texture,
                    data_ptr: 0xdead,
                    row_pitch: 0,
                })
            })
            .collect();
        D3d11PlaneRings::commit_consume(ring_id, ConsumeCommit::InitialMapAll { mapped: bogus_mapped });

        // Presenting 슬롯이 그대로다(슬롯0으로 강제되지 않았다).
        let presenting_after =
            D3d11PlaneRings::presenting_plane(ring_id, 0).expect("presenting unaffected");
        assert_eq!(presenting_after.texture, presenting_before.texture);

        // Writing 슬롯은 여전히 claim 불가하다 — 만약 상태가 corrupt돼
        // Free로 바뀌었다면 아래 루프가 그 슬롯을 반환했을 것이다. 아울러
        // 반환되는 어떤 슬롯의 매핑 포인터도 0xdead가 아니어야 한다(=
        // bogus 커밋이 적용되지 않았다는 증거).
        let mut reclaimed = Vec::new();
        while let Some(c) = D3d11PlaneRings::claim_free_slot(ring_id) {
            if let Some(p0) = c.planes[0] {
                assert_ne!(
                    p0.data_ptr, 0xdead,
                    "stale InitialMapAll commit must not have been applied"
                );
            }
            reclaimed.push(c.slot);
        }
        assert!(
            !reclaimed.contains(&writing_slot),
            "stale InitialMapAll commit must not have freed the Writing slot"
        );
    }

    /// Finding 1 회귀 테스트: 정상적으로 한 번 커밋된 `Advance`를 동일한
    /// (또는 변조된) 내용으로 다시 커밋해도 두 번째 호출은 no-op이어야
    /// 한다 — Remapping 슬롯이 이미 하나도 없으므로 아무 슬롯의 매핑
    /// 포인터도 잘못 덮어써서는 안 되고 패닉도 없어야 한다.
    #[test]
    fn duplicate_advance_commit_is_noop() {
        let slots = make_slots(11000);
        let ring_id = D3d11PlaneRings::create_ring(2, slots);
        warm_up(ring_id, &slots);

        let c1 = D3d11PlaneRings::claim_free_slot(ring_id).expect("slot for advance");
        D3d11PlaneRings::publish_slot(ring_id, c1.slot);

        let plan = D3d11PlaneRings::note_plane_lock_and_plan(ring_id).expect("advance plan");
        let ConsumePlan::Advance {
            map, filled_slot, ..
        } = plan
        else {
            panic!("expected Advance plan");
        };
        let remapped: Vec<RemappedPlane> = map
            .iter()
            .map(|&texture| RemappedPlane {
                texture,
                data_ptr: texture * 900,
                row_pitch: 512,
            })
            .collect();
        // 첫 커밋(정상 경로).
        D3d11PlaneRings::commit_consume(
            ring_id,
            ConsumeCommit::Advance {
                filled_slot,
                remapped,
            },
        );
        D3d11PlaneRings::note_plane_unlock(ring_id);

        let presenting_before =
            D3d11PlaneRings::presenting_plane(ring_id, 0).expect("presenting after first commit");
        assert_eq!(presenting_before.texture, slots[c1.slot][0].unwrap().texture);

        // 같은 filled_slot으로 다시 커밋(변조된 data_ptr로 재현) — 이제
        // Remapping 슬롯이 하나도 없으므로 완전히 no-op이어야 한다.
        let bogus_remapped: Vec<RemappedPlane> = map
            .iter()
            .map(|&texture| RemappedPlane {
                texture,
                data_ptr: texture * 12345,
                row_pitch: 1,
            })
            .collect();
        D3d11PlaneRings::commit_consume(
            ring_id,
            ConsumeCommit::Advance {
                filled_slot,
                remapped: bogus_remapped,
            },
        );

        // presenting_slot이 그대로다.
        let presenting_after =
            D3d11PlaneRings::presenting_plane(ring_id, 0).expect("presenting unaffected");
        assert_eq!(presenting_after.texture, presenting_before.texture);

        // 첫 커밋으로 Free가 된 old-presenting 슬롯(=슬롯0)을 다시
        // claim해서, data_ptr이 중복 커밋의 bogus 값(texture*12345)이
        // 아니라 첫 커밋 값(texture*900) 그대로인지 확인한다.
        let reclaimed = D3d11PlaneRings::claim_free_slot(ring_id).expect("old presenting slot free");
        assert_eq!(reclaimed.slot, 0);
        let p0 = reclaimed.planes[0].expect("plane0 mapped");
        let expected_texture = slots[0][0].unwrap().texture;
        assert_eq!(p0.data_ptr, expected_texture * 900);
        assert_ne!(p0.data_ptr, expected_texture * 12345);
    }

    /// Fix 2 회귀 테스트: 이미 Map된 적 있어 레지스트리에 낡은 mapped 포인터가
    /// 남아 있는 Presenting 슬롯이 다음 Advance에서 Remapping이 됐다가 재-Map에
    /// 실패하면(=해당 plane이 `remapped`에서 빠지면), Free로 전이하기 전에 그
    /// plane의 mapped가 None으로 무효화돼야 한다. 그러지 않으면 claim이 이미
    /// Unmap된 메모리를 가리키는 낡은 포인터를 프로듀서에게 건네 UB memcpy를
    /// 유발한다. 재-Map에 성공한 plane은 새 포인터를 그대로 싣고, 실패한 plane은
    /// claim에서 스킵(None)된다.
    #[test]
    fn advance_commit_with_missing_remapped_plane_clears_mapped_and_claim_skips_it() {
        let slots = make_slots(12000);
        let ring_id = D3d11PlaneRings::create_ring(2, slots);
        let ptrs = warm_up(ring_id, &slots); // 슬롯0=Presenting(unmapped), 1~3=Free(mapped).

        // --- Advance 1: 슬롯1을 소비해 Presenting으로 올린다. 소비자는 이때
        // 슬롯1을 Unmap하지만 레지스트리는 워밍업 때의 낡은 mapped 포인터를 그대로
        // 보유한다(다음 재-Map 실패 시 스테일이 될 값). ---
        let c1 = D3d11PlaneRings::claim_free_slot(ring_id).expect("slot1 free");
        assert_eq!(c1.slot, 1);
        D3d11PlaneRings::publish_slot(ring_id, 1);
        let plan1 = D3d11PlaneRings::note_plane_lock_and_plan(ring_id).expect("advance1");
        let ConsumePlan::Advance {
            map: map1,
            filled_slot: fs1,
            ..
        } = plan1
        else {
            panic!("expected Advance plan");
        };
        assert_eq!(fs1, 1);
        let remapped1 = map1
            .iter()
            .map(|&t| RemappedPlane {
                texture: t,
                data_ptr: t * 200,
                row_pitch: 256,
            })
            .collect();
        D3d11PlaneRings::commit_consume(
            ring_id,
            ConsumeCommit::Advance {
                filled_slot: fs1,
                remapped: remapped1,
            },
        );
        D3d11PlaneRings::note_plane_unlock(ring_id);
        // 슬롯1은 지금 Presenting이고, 레지스트리 mapped는 워밍업 값(곧 스테일).
        let stale_plane1_ptr = ptrs[&slots[1][1].unwrap().texture];

        // --- Advance 2: 슬롯0을 소비해 Presenting으로 올리고 슬롯1을 Remapping
        // 시킨다. 슬롯1 plane1의 재-Map은 실패했다고 가정해 remapped에서 뺀다. ---
        let c2 = D3d11PlaneRings::claim_free_slot(ring_id).expect("free slot");
        assert_eq!(c2.slot, 0); // Advance1로 Free가 된 old-presenting 슬롯0.
        D3d11PlaneRings::publish_slot(ring_id, 0);
        let plan2 = D3d11PlaneRings::note_plane_lock_and_plan(ring_id).expect("advance2");
        let ConsumePlan::Advance {
            map: map2,
            filled_slot: fs2,
            ..
        } = plan2
        else {
            panic!("expected Advance plan");
        };
        assert_eq!(fs2, 0);
        // map2 = 슬롯1의 두 plane 텍스처(이전 Presenting).
        let good_texture = slots[1][0].unwrap().texture; // plane0: 재-Map 성공
        let missing_texture = slots[1][1].unwrap().texture; // plane1: 재-Map 실패(누락)
        assert!(map2.contains(&good_texture));
        assert!(map2.contains(&missing_texture));
        let remapped2 = vec![RemappedPlane {
            texture: good_texture,
            data_ptr: good_texture * 300,
            row_pitch: 128,
        }];
        D3d11PlaneRings::commit_consume(
            ring_id,
            ConsumeCommit::Advance {
                filled_slot: fs2,
                remapped: remapped2,
            },
        );
        D3d11PlaneRings::note_plane_unlock(ring_id);

        // 슬롯1이 Free로 회귀 — claim이 슬롯1을 반환한다(슬롯0은 Presenting).
        let reclaimed = D3d11PlaneRings::claim_free_slot(ring_id).expect("slot1 freed");
        assert_eq!(reclaimed.slot, 1);
        // plane0: 재-Map 성공 → 새 포인터.
        let p0 = reclaimed.planes[0].expect("plane0 remapped");
        assert_eq!(p0.data_ptr, good_texture * 300);
        // plane1: 재-Map 실패 → mapped=None으로 무효화(스테일 포인터 차단).
        assert!(
            reclaimed.planes[1].is_none(),
            "재-Map 실패 plane은 None으로 무효화돼야 한다 (스테일 포인터 {stale_plane1_ptr}가 남으면 UB)"
        );
    }
}

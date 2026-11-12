/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! 벽 타일이 캔버스 이미지의 ★어느 부분을 실제로 표출하는지★ 를 프레임마다 적어 둔다.
//!
//! ## 무엇을 푸는가
//!
//! 이 포크는 WebGL 캔버스 하나를 GPU 마다 하나씩, 네 백엔드에 복제해 그린다. 백엔드 k 는
//! 자기 타일 몫만 셰이딩하면 되므로 scissor 를 거는데(`gfx_wall_webgl_tile_scissor`), 그
//! 사각형을 ***타일 기하에서*** 유도한다 -- "캔버스가 가상 뷰포트를 원점에서 꽉 채운다" 를
//! 가정하고, 캔버스 백킹 크기가 뷰포트와 같은지로만 검사한다.
//!
//! 그 가정이 깨지는 구간이 있다. 구성 전환과 자유 모드의 실시간 위치/크기 편집은 캔버스의
//! ***백킹 크기를 바꾸지 않고*** CSS transform 으로 화면상 위치만 옮긴다. 그러면 검사는
//! 통과하고 scissor 는 ***옮기기 전 사각형***으로 걸린다. 타일이 새로 보게 된 영역은
//! 그려지지도 지워지지도 않아 ★비거나 깜박인다★ -- 운영자 보고(2026-11-09,
//! log_webgl_perf_01/13·14): "구성간 전환 애니메이션시 최종적으로 WebGL 표출 영역이
//! 아니게 되는 부분이 비어 보임", "시작시 전환 애니메이션 재생할 때 좌하단 WebGL 컨텐츠가
//! 깜박이면서 등장함. 전환 애니메이션 종료 후에는 깜박임 관찰되지 않음".
//!
//! ## 왜 여기인가
//!
//! 필요한 값이 ***이 자리에만*** 모여 있다. `TileCacheInstance::update_prim_dependencies`
//! 는 (1) 감싼 surface 들의 inflation 까지 반영해 타일 캐시 픽처 공간으로 올린
//! `pic_coverage_rect`, (2) 그 공간으로 unmap 해 둔 화면 사각형
//! (`screen_rect_in_pic_space`) -- 이 포크에서는 painter 가 문서를 타일 크기로 잡으므로
//! ★그것이 곧 타일★ 이다, (3) prim 의 공간 노드와 `local_prim_rect`, (4) 그리고 Image
//! 분기에서 ***이미 꺼내 쓰고 있는*** `image_data.key` 를 전부 손에 들고 있다.
//!
//! 그리고 전환 중에도 신선하다. 이 포크의 페인트측 애니메이션이 `append_dynamic_properties`
//! 로 보간값을 밀어 넣고, 그것이 `doc.frame_is_valid = false` 로 프레임을 무효화하므로
//! ***디스플레이 리스트가 새로 오지 않아도*** transform 이 움직이는 프레임마다 여기가 다시
//! 돈다.
//!
//! `ExternalImageHandler::lock` 으로는 할 수 없다 -- 그 트레잇은 목적지 기하를 전혀 받지
//! 않는다. lock 은 "프레임 빌드가 적어 둔 값을 읽는 자리" 로만 쓸 수 있다.
//!
//! ## 왜 전역 맵인가
//!
//! `FrameBuilderConfig` 가 `Copy` 라 핸들(`Arc`)을 실을 수 없고, `webrender_api` 는
//! crates.io 0.68 그대로여서 거기에 타입을 더할 수도 없다. 반면 `u32` 하나는 `Copy` 로
//! 흘려보낼 수 있으므로 ★painter 구분은 slot 번호로★ 하고 값은 전역 맵에 둔다. 이 포크는
//! surfman 쪽에서도 같은 모양을 쓴다(`IMPORT_*`, `RETIRED_SHARE_HANDLES`).
//!
//! slot 이 필요한 이유: 캔버스의 `ImageKey` 는 ***WebGL 컨텍스트당 하나***이고 네 painter 의
//! WR 에 같은 키로 들어간다(`webgl_thread::handle_set_image_key`). 키만으로는 네 타일이
//! 서로 덮어쓴다.
//!
//! ## 낡은 값을 쓰지 않는다
//!
//! 읽는 쪽이 나이를 본다. 두 가지가 "이번 프레임 보고 없음" 을 만든다 --
//! `PrimitiveVisibility::reset` 이 `clip_chain` 을 지우지 않아 컬링된 prim 이 지난 프레임
//! 사각형을 들고 있을 수 있고, 타일 밖이면 `update_prim_dependencies` 가
//! `VisibilityState::Culled` 로 조기 반환해 아예 적지 않는다. 그래서 나이가 지난 값은
//! ★없는 것으로 취급★ 하고, 소비자는 기존 경로(타일 기하에서 유도)로 돌아간다 -- 안전한
//! 쪽이다.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use api::ImageKey;

/// 한 (slot, 캔버스) 쌍이 이번 프레임에 표출한 영역.
#[derive(Clone, Copy, Debug)]
pub struct CanvasTileRect {
    /// 이 타일이 표출하는 영역, 캔버스 백킹 픽셀, ★위-왼쪽 원점★(GL 이 아니다 --
    /// 뒤집기는 소비자가 한다).
    pub rect: [i32; 4],
    /// 그 값을 유도한 캔버스 목적지 박스의 크기, 같은 단위. 소비자가 자기가 아는 백킹
    /// 크기와 비교해 ★배율이 1:1 인지★ 확인할 수 있다.
    pub dest_size: [i32; 2],
    /// 적은 시각. 나이 판정과 ★표의 offset 기준★ 이다.
    pub at: Instant,
    /// ★시각별 사각형 표.★ `(at 로부터의 offset(ms), 그 시각의 사각형)`, 앞
    /// `sample_count` 개만 유효하고 offset 오름차순이다.
    ///
    /// `rect` 는 이 표의 `offset == 0` 항목과 같다(표가 비면 `rect` 만 있다). 소비자는
    /// `union_over` 로 ★자기 비트맵이 표출되는 구간★ 만 합집합한다 -- 그래서 표를 촘촘히
    /// 해도 걸리는 scissor 가 넓어지지 않는다.
    pub samples: [(u16, [i32; 4]); MAX_LEAD_SAMPLES],
    /// `samples` 중 유효한 개수.
    pub sample_count: u8,
}

/// 표의 칸 수. 창이 100ms 안쪽이고 간격이 한 주기(16.67ms)이므로 여유를 두고 열여섯이면
/// 넘치지 않는다. 고정 배열이라 ★할당이 없고★ `Copy` 가 유지된다(구조체는 320 바이트쯤
/// 이고 조회마다 한 번 복사된다 -- 캔버스 프레임당 한 번이다).
pub const MAX_LEAD_SAMPLES: usize = 16;

impl CanvasTileRect {
    /// `[from_ms, to_ms]` 구간에 걸리는 샘플들의 합집합.
    ///
    /// 소비자는 ★자기 비트맵이 표출되는 전 기간★ 을 넘긴다 -- 즉 `from` 은 이 기록의
    /// 나이, `to` 는 그 나이에 자기 프레임 간격을 더한 값이다. 그 구간 밖의 샘플은 이
    /// 비트맵이 화면에 있는 동안 해당되지 않으므로 ★쓰지 않는다★.
    ///
    /// 구간에 걸리는 것이 없으면(표가 비었거나 구간이 표를 벗어났으면) `rect` 를 준다 --
    /// 그러면 동작이 표를 들이기 전과 같다.
    pub fn union_over(&self, from_ms: u64, to_ms: u64) -> ([i32; 4], u8) {
        let mut acc: Option<[i32; 4]> = None;
        let mut used = 0;
        for &(offset, rect) in &self.samples[..self.sample_count as usize] {
            let offset = offset as u64;
            if offset < from_ms || offset > to_ms {
                continue;
            }
            used += 1;
            acc = Some(match acc {
                None => rect,
                Some(have) => union_boxes(have, rect),
            });
        }
        // ★몇 개를 썼는지 함께 준다.★ 0 이면 폴백이고, 그것은 `lead 0` 과 같은 상태라
        // 구조적으로 한 프레임 부족하다 -- 소비자가 그 횟수를 센다. `rect` 와 같은지로
        // 판정하면 ***움직이지 않을 때 우연히 같은 경우***를 폴백으로 오판한다.
        (acc.unwrap_or(self.rect), used)
    }
}

/// `[x, y, w, h]` 둘의 합집합.
fn union_boxes(a: [i32; 4], b: [i32; 4]) -> [i32; 4] {
    let x0 = a[0].min(b[0]);
    let y0 = a[1].min(b[1]);
    let x1 = (a[0] + a[2]).max(b[0] + b[2]);
    let y1 = (a[1] + a[3]).max(b[1] + b[3]);
    [x0, y0, x1 - x0, y1 - y0]
}

/// 꺼져 있으면 비용이 ★원자 로드 하나★ 다. `paint` 가 pref 를 보고 세운다 -- `webrender` 는
/// `servo-config` 에 의존하지 않는다(설정 표면 규약).
static ENABLED: AtomicBool = AtomicBool::new(false);

/// swap 뒤 clear 를 타일 영역만 할지(`gfx_wall_webgl_tile_scissor_clear`).
///
/// ★원자인 이유★: 이 값을 읽는 자리가 `handle_swap_buffers` 안이고 프레임당 painter 수만큼
/// 돈다(240/s). 거기서 `pref!` 를 치면 전역 `RwLock` 을 그만큼 잡는다 -- 계측을 전부 걷어낸
/// 2026-10-27 규칙이 금지하는 형태다. `ENABLED`/`MAX_AGE_MS` 와 같은 축이므로 같은 자리에
/// 둔다. 2026-11-12.
static TILE_CLEAR: AtomicBool = AtomicBool::new(false);

/// `paint` 가 pref 를 보고 세운다. 근거는 `TILE_CLEAR` 의 doc 주석.
pub fn set_tile_clear(enabled: bool) {
    TILE_CLEAR.store(enabled, Ordering::Relaxed);
}

/// swap 뒤 clear 를 타일 영역만 할지. 비용은 ★원자 로드 하나★ 다.
pub fn tile_clear() -> bool {
    TILE_CLEAR.load(Ordering::Relaxed)
}

type Map = HashMap<(u32, ImageKey), CanvasTileRect>;

fn map() -> &'static Mutex<Map> {
    static MAP: OnceLock<Mutex<Map>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// ★기록 대상인 캔버스의 `ImageKey` 집합.★ WebGL 쪽이 등록한다.
///
/// ★왜 필요한가★(2026-11-09, log_webgl_perf_01/15): 처음에는 `note` 를 ***모든 Image
/// prim***에 대해 불렀다. 캔버스만 필요한데 일반 이미지까지 들어오므로 (1) 맵이 페이지의
/// 이미지 수만큼 커지고, (2) 이미지가 생기고 사라지는 구성(전환 애니메이션)에서 `forget`
/// 을 부르는 사람이 없어 계속 자랐고, (3) 애초에 ***일반 이미지 경로를 건드릴 이유가
/// 없다***. 그 구성에서 `winit_wall` 이 `0xc0000374`(힙 손상)로 죽었고, 이 기능을 끄면
/// 죽지 않았다. 손상의 기구를 코드로 설명하지는 못했지만, ***건드리지 않아야 할 경로를
/// 건드리고 있던 것***은 그 자체로 고칠 이유다.
fn canvas_keys() -> &'static Mutex<HashSet<ImageKey>> {
    static KEYS: OnceLock<Mutex<HashSet<ImageKey>>> = OnceLock::new();
    KEYS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 이 `ImageKey` 가 등록된 캔버스인가.
///
/// ★계산을 시작하기 전에 묻는다.★ 2026-11-09 에 기록만 걸러서 ***계산은 모든 Image prim
/// 에서 그대로 돌았다***. 캔버스만 필요한데 일반 이미지까지 좌표 변환을 돌릴 이유가 없다.
///
/// 그때 `winit_wall` 이 회전 애니메이션에서 죽었는데, ★원인은 이 게이트가 아니라
/// ref/target 순서였다★ -- `picture.rs::note_canvas_tile_rect` 의 doc 주석에 적어 뒀다.
/// 이 게이트는 불필요한 작업을 없애는 것이고, 크래시 수정은 그쪽이다.
pub fn is_canvas(key: ImageKey) -> bool {
    match canvas_keys().lock() {
        Ok(keys) => keys.contains(&key),
        Err(_) => false,
    }
}

/// 이 `ImageKey` 를 캔버스로 등록한다. WebGL 쪽이 키를 받을 때 부른다.
pub fn register_canvas(key: ImageKey) {
    if let Ok(mut keys) = canvas_keys().lock() {
        keys.insert(key);
    }
}

/// 기록을 켜거나 끈다. `paint` 가 부른다.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// 애니메이션이 ★이 프레임이 표출될 창 안에서 지나가는 변환들★. painter 가 틱마다 적고
/// `note_canvas_tile_rect` 가 프레임 빌드에서 읽는다.
///
/// ## ★무엇을 고치는 값인가★ (2026-11-11)
///
/// WR 이 적는 사각형은 그 프레임의 애니메이션 값으로는 정확하지만, 캔버스는 비동기다 --
/// WebGL 스레드가 그 값을 읽어 scissor 를 걸고 그린 비트맵은 ***더 뒤의 프레임***에서
/// 표출된다. 그 사이에 캔버스가 지나간 영역이 타일 경계에 ***그려진 적 없는 띠***가 된다.
///
/// 그래서 `note_canvas_tile_rect` 가 ***창 안의 여러 시각에 대해 각각 사각형을 계산하고
/// 합집합***을 적는다. 그 계산에 필요한 것이 "그 시각의 변환" 이고, WR 은 애니메이션
/// 정의를 모르므로(`DynamicProperties` 로 그 프레임의 값만 받는다) painter 가 여기에
/// 실어 준다.
///
/// ## ★왜 4x4 전체인가★
///
/// 1 차 구현은 평행이동 성분(`m41`/`m42`) 둘만 실었다. 그 근사가 "방향" 이라는 개념을
/// 만들고, 부호를 틀리기 쉬워 양방향으로 덮었고, 그 비용을 줄이려 단방향으로 바꿨다가
/// ***회귀했다***(log_webgl_perf_01/32). 운영자 지적이 정확했다 -- "애초에 행렬 변환으로
/// 계산하면 방향이 무슨 상관인데?" 행렬을 그대로 쓰면 이동·스케일·회전·전환점이 전부
/// 따라오고 방향 개념이 사라진다.
///
/// ## ★왜 Mutex 인가★
///
/// `Transform3D` 는 16 float 이라 원자에 들어가지 않는다. painter 는 틱당 ***한 번***
/// 쓰고(정상 상태에서 경합이 없다) 프레임 빌드가 읽는다. 이 모듈은 이미 같은 모양의
/// `Mutex` 둘을 그 경로에 갖고 있다. 근거와 한계는 설계 문서 5.1 에 있다.
fn lead_map() -> &'static Mutex<HashMap<u32, Vec<LeadSample>>> {
    static LEAD_SAMPLES: OnceLock<Mutex<HashMap<u32, Vec<LeadSample>>>> = OnceLock::new();
    LEAD_SAMPLES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 한 애니메이션 바인딩의 한 시각 샘플.
#[derive(Clone, Copy, Debug)]
pub struct LeadSample {
    /// 이 샘플의 시각, 창 기준 시각(`is_now` 인 샘플)으로부터의 offset(ms). WR 이 표의
    /// 키로 그대로 쓴다. 과거 샘플(나이 흡수용)은 0 이다.
    pub offset_ms: u16,
    /// 이 애니메이션의 바인딩 id. 소비자가 `SpatialTree::find_spatial_node_by_anim_id`
    /// 에 그대로 넘기므로 `u64` 가 아니라 그 타입이어야 한다.
    pub binding_id: api::PropertyBindingId,
    /// 그 시각의 변환, 행 우선 16 성분(`LayoutTransform::to_array` 와 같은 순서).
    pub transform: [f32; 16],
    /// 이 샘플이 ★현재 시각★ 의 것인가. 켤레 합성의 기준(`A(now)^-1`)이 된다.
    pub is_now: bool,
}

/// 이 painter 슬롯의 창 샘플을 바꿔 넣는다. ★틱당 한 번, Mutex 한 번.★
pub fn set_lead_samples(slot: u32, samples: Vec<LeadSample>) {
    if let Ok(mut map) = lead_map().lock() {
        if samples.is_empty() {
            map.remove(&slot);
        } else {
            map.insert(slot, samples);
        }
    }
}

/// 그 슬롯의 창 샘플. 없으면 빈 벡터 -- 그러면 호출자는 오늘 경로만 쓴다.
pub fn lead_samples(slot: u32) -> Vec<LeadSample> {
    lead_map()
        .lock()
        .ok()
        .and_then(|map| map.get(&slot).cloned())
        .unwrap_or_default()
}

/// 쓸 수 있는 기록의 최대 나이, ms. `painter` 가 틱마다 pref 에서 갱신한다.
///
/// ★`pref!` 를 소비자 쪽에서 치지 않기 위한 것이다.★ `pref!` 는 전역 `RwLock` 획득이고,
/// 이 값을 읽는 자리는 ***캔버스 프레임마다 painter 수만큼*** 도는 타이밍 경로다. 계측을
/// 전부 걷어낸 2026-10-27 의 규칙이 그것을 금지한다. 원자 하나로 바꾼다.
static MAX_AGE_MS: AtomicU64 = AtomicU64::new(34);

/// 유효기간을 세운다(ms). `paint` 가 부른다.
pub fn set_max_age_ms(ms: u64) {
    MAX_AGE_MS.store(ms, Ordering::Relaxed);
}

/// 지금 유효기간. ★원자 로드 하나다.★
pub fn max_age_ms() -> u64 {
    MAX_AGE_MS.load(Ordering::Relaxed)
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 이번 프레임의 사각형을 적는다. `picture.rs` 의 Image 분기가 부른다.
pub fn note(
    slot: u32,
    key: ImageKey,
    rect: [i32; 4],
    dest_size: [i32; 2],
    samples: &[(u16, [i32; 4])],
) {
    if !is_enabled() {
        return;
    }
    // 폭이나 높이가 0 이면 이 타일에 안 보인다는 뜻이고, 그것은 "보고 없음" 과 같다 --
    // 적지 않아야 소비자가 기존 경로로 돌아간다.
    if rect[2] <= 0 || rect[3] <= 0 {
        return;
    }
    // ★등록된 캔버스만 기록한다.★ 근거는 `canvas_keys` 의 doc 주석에 있다.
    match canvas_keys().lock() {
        Ok(keys) if keys.contains(&key) => {},
        _ => return,
    }
    if let Ok(mut map) = map().lock() {
        map.insert(
            (slot, key),
            CanvasTileRect {
                rect,
                dest_size,
                at: Instant::now(),
                samples: {
                    let mut table = [(0u16, [0i32; 4]); MAX_LEAD_SAMPLES];
                    for (slot, sample) in table.iter_mut().zip(samples) {
                        *slot = *sample;
                    }
                    table
                },
                sample_count: samples.len().min(MAX_LEAD_SAMPLES) as u8,
            },
        );
    }
}

/// 이 (slot, 캔버스)의 최신 사각형. `max_age_ms` 보다 오래됐으면 `None` 이다.
pub fn get(slot: u32, key: ImageKey, max_age_ms: u64) -> Option<CanvasTileRect> {
    match lookup(slot, key, max_age_ms) {
        Lookup::Hit(value, _) => Some(value),
        _ => None,
    }
}

/// `lookup` 의 결과. ★`None` 하나로 뭉치지 않는다★ -- "WR 이 아직 적지 않았다" 와 "적었지만
/// 낡았다" 는 원인이 다르고 고치는 자리도 다르다. 소비자가 이것을 사유별로 센다.
pub enum Lookup {
    /// 기능이 꺼져 있다.
    Disabled,
    /// 이 (slot, 캔버스)에 대해 WR 이 적은 것이 없다.
    Missing,
    /// 적힌 것이 있지만 `max_age_ms` 보다 낡았다. 값과 나이를 함께 준다.
    Stale(CanvasTileRect, u64),
    /// 쓸 수 있는 값. ★나이를 함께 준다★ -- 소비자가 `union_over` 의 구간 시작으로
    /// 쓴다. 앞 판은 이 나이를 계산해서 버렸고, 그래서 `lead` 를 고정 상수로 추측해야
    /// 했다(근거는 `CanvasTileRect::samples` 의 doc 주석).
    Hit(CanvasTileRect, u64),
}

/// `get` 과 같은 조회지만 ★왜 못 썼는지를 함께 돌려준다.★
pub fn lookup(slot: u32, key: ImageKey, max_age_ms: u64) -> Lookup {
    if !is_enabled() {
        return Lookup::Disabled;
    }
    let Ok(guard) = map().lock() else {
        return Lookup::Missing;
    };
    let Some(value) = guard.get(&(slot, key)).copied() else {
        return Lookup::Missing;
    };
    drop(guard);
    let age = value.at.elapsed().as_millis() as u64;
    if age > max_age_ms {
        return Lookup::Stale(value, age);
    }
    Lookup::Hit(value, age)
}

/// 이 캔버스의 기록과 등록을 모든 slot 에서 지운다. ★컨텍스트가 사라질 때 반드시 부른다★
/// -- 안 부르면 맵이 자란다.
pub fn forget(key: ImageKey) {
    if let Ok(mut keys) = canvas_keys().lock() {
        keys.remove(&key);
    }
    if let Ok(mut map) = map().lock() {
        map.retain(|(_, k), _| *k != key);
    }
}

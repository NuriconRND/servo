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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use api::ImageKey;

/// 한 (slot, 캔버스) 쌍이 이번 프레임에 표출한 영역.
#[derive(Clone, Copy, Debug)]
pub struct CanvasTileRect {
    /// 캔버스 백킹 픽셀에서의 사각형, ★위-왼쪽 원점★(GL 이 아니다 -- 뒤집기는 소비자가
    /// 한다, 기존 경로와 같은 규약을 쓰기 위해서다).
    pub rect: [i32; 4],
    /// 그 값을 유도한 캔버스 목적지 박스의 크기, 같은 단위. 소비자가 자기가 아는 백킹
    /// 크기와 비교해 ★배율이 1:1 인지★ 확인할 수 있다.
    pub dest_size: [i32; 2],
    /// 적은 시각. 나이 판정에만 쓴다.
    pub at: Instant,
}

/// 꺼져 있으면 비용이 ★원자 로드 하나★ 다. `paint` 가 pref 를 보고 세운다 -- `webrender` 는
/// `servo-config` 에 의존하지 않는다(설정 표면 규약).
static ENABLED: AtomicBool = AtomicBool::new(false);

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

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 이번 프레임의 사각형을 적는다. `picture.rs` 의 Image 분기가 부른다.
pub fn note(slot: u32, key: ImageKey, rect: [i32; 4], dest_size: [i32; 2]) {
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
    let value = CanvasTileRect {
        rect,
        dest_size,
        at: Instant::now(),
    };
    if let Ok(mut map) = map().lock() {
        map.insert((slot, key), value);
    }
}

/// 이 (slot, 캔버스)의 최신 사각형. `max_age_ms` 보다 오래됐으면 `None` 이다.
pub fn get(slot: u32, key: ImageKey, max_age_ms: u64) -> Option<CanvasTileRect> {
    if !is_enabled() {
        return None;
    }
    let value = *map().lock().ok()?.get(&(slot, key))?;
    if value.at.elapsed().as_millis() as u64 > max_age_ms {
        return None;
    }
    Some(value)
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

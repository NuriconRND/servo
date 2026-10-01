//! ★합성 프레임이 **각 디스플레이에 언제 표출됐는지**를 묻는 유일한 창구.★
//!
//! 지금까지 쓰던 `IDCompositionDevice::GetFrameStatistics` 는 데스크톱 합성 하나를
//! 돌려줄 뿐이고(네 디바이스가 같은 `lastFrameTime` 을 준다), `DWM_TIMING_INFO` 의
//! `qpcCompose` 는 실측에서 `qpcVBlank` 와 **완전히 같았다** -- 전 표본 차이 0.00. 즉 둘
//! 다 "합성이 실제로 끝난 시각" 도, "그 프레임이 어느 패널에 언제 떴는지" 도 말해 주지
//! 않는다.
//!
//! `dcomp.dll` 은 그 둘을 주는 함수를 따로 내보낸다(Windows 10 1803+). COM 이 아니라 평범한
//! export 이고 조회만 한다 -- 블록하지 않는다.
//!
//! - [`DCompositionGetFrameId`] : 합성 프레임의 id. `CREATED`/`CONFIRMED`/`COMPLETED` 세
//!   단계가 있어 "완료된 마지막 프레임" 을 특정할 수 있다.
//! - [`DCompositionGetStatistics`] : 그 프레임이 **시작한 시각**, **겨냥한 vblank**, 주기,
//!   그리고 그때 존재하던 타깃(디스플레이) 목록.
//! - [`DCompositionGetTargetStatistics`] : ★타깃별 `presentTime`★ -- 그 패널에 실제로 표출된
//!   시각. 밀려 있는 present 수도 같이 준다.
//!
//! ★`windows-sys 0.61` 에는 DirectComposition 모듈이 아예 없고, `winapi 0.3.9` 에는 이름만
//! 비슷한 구식 `DCompositionGetFrameStatistics`(인자가 다른 별개 함수)뿐이라 직접 선언한다.★
//! 아래 선언은 이 장비의 SDK 10.0.26100.0 원문에서 그대로 옮긴 것이다 --
//! `um/dcomp.h:175,189,205` 와 `shared/dcomptypes.h:100-159`.

#![cfg(windows)]
// 크레이트가 `#![deny(unsafe_code)]` 다. 이 모듈은 전부 FFI 조회라 예외를 둔다
// (`dcomp_compositor.rs`/`output_grid.rs` 와 같은 취지).
#![allow(unsafe_code)]

use winapi::shared::minwindef::UINT;
use winapi::shared::ntdef::HRESULT;

/// `COMPOSITION_FRAME_ID_TYPE`. 쓰는 것은 `COMPLETED` 뿐이지만, 셋을 같이 적어 두어야
/// 나중에 "확정 단계와 완료 단계가 얼마나 벌어지나" 를 묻고 싶을 때 손댈 곳이 없다.
#[allow(dead_code)]
#[repr(u32)]
#[derive(Clone, Copy)]
pub(crate) enum FrameIdType {
    Created = 0,
    Confirmed = 1,
    Completed = 2,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CompositionFrameStats {
    start_time: u64,
    target_time: u64,
    frame_period: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct CompositionTargetId {
    display_adapter_luid: Luid,
    render_adapter_luid: Luid,
    vidpn_source_id: UINT,
    vidpn_target_id: UINT,
    unique_id: UINT,
}

/// `winapi` 의 `LUID` 는 `Default`/`PartialEq` 를 구현하지 않아 배열 초기화와 비교에 쓸 수
/// 없다. 레이아웃이 같은 사본을 둔다.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Luid {
    low_part: u32,
    high_part: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CompositionTargetStats {
    outstanding_presents: UINT,
    present_time: u64,
    vblank_duration: u64,
    presented_stats: CompositionStats,
    completed_stats: CompositionStats,
}

#[link(name = "dcomp")]
unsafe extern "system" {
    fn DCompositionGetFrameId(frame_id_type: u32, frame_id: *mut u64) -> HRESULT;
    fn DCompositionGetStatistics(
        frame_id: u64,
        frame_stats: *mut CompositionFrameStats,
        target_id_count: UINT,
        target_ids: *mut CompositionTargetId,
        actual_target_id_count: *mut UINT,
    ) -> HRESULT;
    fn DCompositionGetTargetStatistics(
        frame_id: u64,
        target_id: *const CompositionTargetId,
        target_stats: *mut CompositionTargetStats,
    ) -> HRESULT;
}

/// `COMPOSITION_STATS` 를 그대로 옮긴 것. `presentedStats`/`completedStats` 둘 다 이 모양이다.
///
/// ★`refresh_count` 가 이 모듈에서 가장 중요한 수다.★ 합성 프레임 하나가 지나는 동안 이
/// 값이 어떤 타깃에서만 2 올랐다면, 그 패널은 그 합성에 **리프레시를 두 번 썼다** -- 같은
/// 그림을 두 번 보여 줬다는 뜻이고, 그것이 곧 저더다. 시각을 추론할 필요 없이 세어진다.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct CompositionStats {
    pub present_count: UINT,
    pub refresh_count: UINT,
    pub virtual_refresh_count: UINT,
    pub time: u64,
}

/// 한 합성 프레임에 대해 한 타깃(디스플레이)이 돌려준 것.
#[derive(Clone, Copy)]
pub(crate) struct TargetSample {
    /// 이 타깃을 구동하는 어댑터의 LUID. 우리 타일에 붙이는 열쇠다 -- 기동 로그의
    /// `tile N: display N -> \\.\DISPLAYxx adapter N luid ...` 와 같은 값이다.
    pub display_adapter_luid: (u32, i32),
    /// DWM 이 이 타깃에 그 프레임을 **제출한** 시각(QPC).
    ///
    /// ★스캔아웃 시각이 아니다.★ 네 패널의 vblank 가 12.7ms 에 걸쳐 흩어져 있는데 이 값은
    /// 넷이 0.6ms 안에 모여 있다 -- 같은 순간에 네 패널이 표출하는 것은 물리적으로 불가능
    /// 하므로, 이것은 제출/큐잉 시각이다. 실제 표출은 `completed` 쪽에서 찾아야 한다.
    pub present_time: u64,
    /// 그 타깃에 아직 밀려 있는 present 수.
    pub outstanding_presents: u32,
    pub presented: CompositionStats,
    pub completed: CompositionStats,
}

/// 한 합성 프레임의 전부.
pub(crate) struct FrameSample {
    /// 합성이 시작한 시각(QPC).
    pub start_time: u64,
    /// 그 합성이 겨냥한 vblank(QPC).
    pub target_time: u64,
    /// 한 주기의 QPC 틱. ★단위 검산용이기도 하다★ -- ms 로 환산해 16.67 이 안 나오면
    /// 이 구조체의 시간들이 QPC 가 아니라는 뜻이고, 그러면 아래 값들을 믿으면 안 된다.
    pub frame_period: u64,
    /// 그 시점에 존재하던 타깃 수(`targets` 가 잘렸는지 보려면 이것과 비교한다).
    pub target_count: u32,
    pub targets: Vec<TargetSample>,
}

/// 한 번에 받아 올 타깃 수의 상한. SDK 의 `COMPOSITION_STATS_MAX_TARGETS` 는 256 이지만
/// 이 벽은 넷이고, 상한을 크게 잡아 봐야 스택만 먹는다. 잘리면 `target_count` 로 보인다.
const MAX_TARGETS: usize = 16;

/// 마지막으로 **완료된** 합성 프레임의 id.
///
/// ★id 는 조밀하게 연속이다★ -- 실측에서 창(약 1 초)마다 62~64 씩 오른다. 즉 합성마다
/// 하나이고, 그래서 `sample_frame` 으로 지난 창의 프레임을 **빠짐없이** 되짚을 수 있다.
pub(crate) fn completed_frame_id() -> Option<u64> {
    let mut frame_id: u64 = 0;
    // Safety: 순수 out-param 조회.
    if unsafe { DCompositionGetFrameId(FrameIdType::Completed as u32, &mut frame_id) } < 0 {
        return None;
    }
    Some(frame_id)
}

/// 주어진 합성 프레임 하나의 통계. 실패하면 `None` -- DWM 이 보관하는 이력은 유한하므로
/// 너무 오래된 id 는 실패한다. 호출자는 그 프레임을 건너뛰고 센다.
///
/// ★조회만 한다.★ 어떤 대기도 넣지 말 것 -- 이 저장소에는 합성 경로에 프로세스 범위 대기를
/// 넣어 생긴 회귀가 기록돼 있다(`dcomp_compositor::composition_grid` 주석).
pub(crate) fn sample_frame(frame_id: u64) -> Option<FrameSample> {
    let mut stats = CompositionFrameStats::default();
    let mut ids = [CompositionTargetId::default(); MAX_TARGETS];
    let mut actual: UINT = 0;
    // Safety: `ids` 는 `MAX_TARGETS` 개를 담을 수 있고 그 수를 그대로 넘긴다.
    if unsafe {
        DCompositionGetStatistics(
            frame_id,
            &mut stats,
            MAX_TARGETS as UINT,
            ids.as_mut_ptr(),
            &mut actual,
        )
    } < 0
    {
        return None;
    }
    let returned = (actual as usize).min(MAX_TARGETS);
    let mut targets = Vec::with_capacity(returned);
    for id in ids.iter().take(returned) {
        let mut target_stats = CompositionTargetStats::default();
        // Safety: `id` 는 바로 위 호출이 채운 값이고, 호출 동안 살아 있다.
        if unsafe { DCompositionGetTargetStatistics(frame_id, id, &mut target_stats) } < 0 {
            continue;
        }
        targets.push(TargetSample {
            display_adapter_luid: (
                id.display_adapter_luid.low_part,
                id.display_adapter_luid.high_part,
            ),
            present_time: target_stats.present_time,
            outstanding_presents: target_stats.outstanding_presents,
            presented: target_stats.presented_stats,
            completed: target_stats.completed_stats,
        });
    }
    Some(FrameSample {
        start_time: stats.start_time,
        target_time: stats.target_time,
        frame_period: stats.frame_period,
        target_count: actual,
        targets,
    })
}

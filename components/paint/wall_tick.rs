/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 벽 프레임 생산 틱의 순수 판정.
//!
//! ★프레임 생산자는 벽 전체에 하나다.★ 한 주기에 논리 프레임 번호를 하나 떠서 전 타일에
//! 뿌리고 배리어로 묶는다. 프레임의 **구성 요소**(비디오 도착, 애니메이션 값, 스크립트의 새
//! 디스플레이 리스트)는 그 프레임을 낼 **방아쇠가 될 수 없다**. 구성 요소 생산자는 약속된
//! 자리에 자기 값만 갱신하고, 생산은 그 값들을 모아 그리기만 한다 -- ★생산자는 프레임에
//! 무엇이 들었는지 묻지 않는다.★
//!
//! 판정이 `painter.rs` 가 아니라 여기 있는 이유: 그 파일은 5,000 줄이라 다음 사람이 거기서
//! 이것을 못 찾는다. 위 규칙을 떠받치는 타입끼리 한자리에 모아 두어야 규칙을 깨는 변경이
//! 리뷰에서 눈에 띈다.

/// 한 타일이 이번 틱에 한 일.
///
/// ★`KeepPrevious` 는 "아무 일도 안 했다" 가 아니다.★ 그 타일은 **직전 프레임을 그대로
/// 유지하는 방식으로** 이 논리 프레임에 참여한다 -- 배리어에서 조용히 빠지면 화면에 프레임
/// N 과 N-1 이 섞인다. 그 유지를 실제로 지시하는 배선은 Task 8 이 넣는다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TileOutcome {
    /// 이 타일이 이번 논리 프레임의 합성을 냈다.
    Generated,
    /// 이 타일은 이번 논리 프레임에 직전 프레임을 유지한다.
    KeepPrevious,
}

/// 이 타일이 이번 틱에 합성을 낼 수 있는가.
///
/// ★두 인자는 호출자가 **고정된 순서로** 평가해서 넘긴다.★ `renderer_behind` 가 먼저다.
/// 그것은 조회가 아니라 liveness 프로브이고, 2 초 강제 해제가 **거절 경로에서만** 돌기
/// 때문이다(`Painter::renderer_behind`). 답이 "이번엔 건너뛴다" 인 타일에서도 매 틱 불러야
/// 한다 -- 실측(2026-08-31): 그 호출을 잃자 네 페인터 중 둘이 렌더를 멈추고 남은 44 초
/// 동안 돌아오지 않았다.
///
/// `waiting_on_canvas` 는 ★캔버스 이미지 대기만★ 본다. `FrameDelayer::needs_new_frame` 은
/// `pending_frame` 도 같이 보는데 그것은 스크립트의 빚이고, 틱 생산은 그 빚과 무관하게
/// 돈다(규칙 2).
pub(crate) fn tile_may_generate(renderer_behind: bool, waiting_on_canvas: bool) -> TileOutcome {
    if renderer_behind || waiting_on_canvas {
        return TileOutcome::KeepPrevious;
    }
    TileOutcome::Generated
}

#[cfg(test)]
mod tests {
    use super::{TileOutcome, tile_may_generate};

    /// 앞 합성이 아직 안 끝난 타일은 이번 프레임을 건너뛴다. 그래도 호출자는
    /// `renderer_behind()` 를 이미 불렀고, 그 호출이 2 초 강제 해제를 돌린다.
    #[test]
    fn a_tile_that_is_behind_keeps_its_previous_frame() {
        assert_eq!(tile_may_generate(true, false), TileOutcome::KeepPrevious);
    }

    /// ★캔버스를 기다려도 liveness 는 물어본 뒤다.★ 순서가 뒤집히면 2 초 강제 해제가
    /// 영영 안 돈다 -- 그것이 "4 개 중 2 개 타일 44 초 사망" 조건이다.
    #[test]
    fn a_tile_waiting_on_canvas_keeps_its_previous_frame() {
        assert_eq!(tile_may_generate(false, true), TileOutcome::KeepPrevious);
    }

    #[test]
    fn a_free_tile_generates() {
        assert_eq!(tile_may_generate(false, false), TileOutcome::Generated);
    }
}

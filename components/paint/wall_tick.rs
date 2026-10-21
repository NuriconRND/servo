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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// 한 타일이 이번 틱에 한 일.
///
/// ★`KeepPrevious` 는 "아무 일도 안 했다" 가 아니다.★ 그 타일은 **직전 프레임을 그대로
/// 유지하는 방식으로** 이 논리 프레임에 참여한다 -- 배리어에서 조용히 빠지면 화면에 프레임
/// N 과 N-1 이 섞인다. 그 유지를 실제로 지시하는 자리는
/// `WallFrameCoordinator::arm_keep_previous` 다(2026-10-22 Task 8).
/// ★분류하는 자리는 하나다 -- `issue_wall_frame_request` 의 `match`.★ 2026-10-22 Task 8
/// 전에는 `generated()` 라는 술어가 그 일을 했는데, 거절한 타일도 갈 곳이 생기면서
/// 참/거짓 하나로는 모자라게 됐다. 술어 대신 ★빠짐없는 `match`★ 를 쓰는 편이 그 메서드의
/// 취지("변형이 늘면 한자리에서 정한다")를 더 강하게 지킨다 -- 변형을 더하면 분류가
/// 조용히 한쪽으로 묶이지 않고 ***컴파일 오류***가 난다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TileOutcome {
    /// 이 타일이 이번 논리 프레임의 합성을 냈다. ★배리어가 기다리는 대상이다★ -- 이쪽만
    /// frame-ready 를 낸다.
    Generated,
    /// 이 타일은 이번 논리 프레임에 직전 프레임을 유지한다. ★배리어에 넣으면 안 된다★ --
    /// 트랜잭션을 안 보냈으므로 ***오지 않을 frame-ready 를*** 기다리게 되고, 그 논리
    /// 프레임이 마감까지 안 닫힌다.
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
/// `waiting_on_canvas` 는 ★캔버스 이미지 대기만★ 본다. 2026-10-21 Task 7 전에는
/// `FrameDelayer::needs_new_frame` 이 스크립트의 빚(`pending_frame`)도 같이 봤고, 틱 생산은
/// 그 빚과 무관하게 돌아야 하므로(규칙 2) 그쪽을 쓰면 안 됐다. 그 플래그는 세울 사람과 함께
/// 사라졌다.
pub(crate) fn tile_may_generate(renderer_behind: bool, waiting_on_canvas: bool) -> TileOutcome {
    if renderer_behind || waiting_on_canvas {
        return TileOutcome::KeepPrevious;
    }
    TileOutcome::Generated
}

/// 이 타일이 쥔 캔버스 ack 를 지금 풀어도 되는가. ★해제는 생산과 무관하다.★
///
/// 인자가 둘뿐인 것이 이 술어의 전부다 -- ★`renderer_behind` 를 보지 않는다★(그리고
/// 2026-10-21 에 사라진 `pending_frame` 도 보지 않았다). 그 둘은 "이 타일이 지금 합성을 낼
/// 수 있는가" 를 묻는 값이고, 해제는 그 질문과 상관이 없다. 상관시키면 ***거절하는
/// 타일에서 빚이 영영 안 풀린다***: 실측
/// (2026-10-16, `log_ani_debug 36`) 에서 divisor=1 실행은 스크립트 요청이 ***매번***
/// 거절 경로로 빠졌고, 그 경로가 ack 를 안 풀어 117 초에서 멎었다. 멈춘 것은 페인터가
/// 아니라 스크립트였다 -- 그때까지 초당 60 프레임, dx 공칭값, overwritten=0 이었다.
///
/// `pending_canvas_images` 조건은 ★남는다 -- FrameDelayer 계약이다.★ ack 는 "네가
/// 기다리던 캔버스가 화면에 올라갔다" 는 뜻이라, 이미지가 WR 에 닿기 전에 풀면 스크립트가
/// 합성되지 않은 캔버스 위에서 다음 rAF 를 돈다. ★진입점 무조건 해제는 검토했고 택하지
/// 않았다.★
pub(crate) fn ack_is_releasable(holds_debt: bool, pending_canvas_images: usize) -> bool {
    holds_debt && pending_canvas_images == 0
}

/// ★다음 틱 에지에 벽 생산 1 회가 예약됐는가.★ 자유 실행 표출 클럭이 없는 임베더
/// (servoshell / WPT / Android)용이다 -- `frame_time.rs` 의 전역 통지와 같은 모양이다.
///
/// ★깨우기는 프레임을 내지 않는다.★ 깨우기의 일은 이 비트를 세우는 것뿐이고, 틱이 그것을
/// 집어 가면 그때 생산한다(`Paint::on_demand_wall_tick`). 그래야 생산자가 어디서나 틱
/// 하나이고(규칙 1), 깨우기라는 구성 요소가 프레임의 방아쇠가 되지 않으며(규칙 2),
/// 깨우기가 없는 조용한 페이지는 생산도 없다(불변식 16).
///
/// ★비트는 하나다 -- 개수가 아니다.★ 한 에지 전에 깨우기가 열 번 와도 생산은 한 번이다.
/// 자유 실행 클럭이 있는 구성은 이 비트를 보지 않고 매 주기 생산한다.
static NEXT_EDGE_SCHEDULED: AtomicBool = AtomicBool::new(false);

/// 다음 틱 에지에 생산 1 회를 예약한다. 이미 예약돼 있으면 아무 일도 없다.
pub(crate) fn schedule_production_at_next_edge() {
    NEXT_EDGE_SCHEDULED.store(true, Ordering::Release);
}

/// 예약을 집어 간다. ★`swap(false)` 라서 한 예약은 한 번만 생산이 된다.★
pub(crate) fn take_scheduled_edge() -> bool {
    NEXT_EDGE_SCHEDULED.swap(false, Ordering::AcqRel)
}

/// ★지금까지 센 틱 에지의 번호.★
///
/// 벽 전체에 하나인 것이 요점이다 -- 틱도 하나이기 때문이다(규칙 1). 타일마다 자기
/// 예산(`ExternalPresentBudget`)을 들고 이 번호를 보면 "틱마다, 타일마다 한 번" 이 번호
/// 비교 하나로 정해진다.
///
/// ★writer 가 둘이다★ -- 셸의 구성에 따라 틱 에지가 다른 자리에서 나기 때문이다:
///
/// - `paint::note_present_tick()` — 자유 실행 표출 클럭이 있는 셸(`winit_wall`, 세 클럭
///   구성 전부가 이 함수를 지난다).
/// - `Paint::on_demand_wall_tick()` — 자유 실행 클럭이 없는 임베더(servoshell / WPT /
///   Android). 그쪽은 `note_present_tick()` 을 부르지 않으므로 여기서 올리지 않으면
///   번호가 0 에 멈추고 `ExternalPresentBudget` 이 ***최초 1 회 뒤 영구 정지***한다.
///
/// ★강제하지 않는다 -- 증상만 적어 둔다.★ 둘 다 부르는 셸은 아직 없다. 생기면 한 에지에
/// 번호가 ***두 번*** 올라 `ExternalPresentBudget` 의 천장이 깨지고(한 틱에 외부 서피스
/// 표출이 두 번 나간다), `FRAMEREASON` 의 `external_surface` 계수가 틱당 2 로 보인다 --
/// 설계 8 절의 "틱당 1 회 이하" 판정이 그것을 잡는다. 잡힌 쪽이 writer 를 하나로 줄이면 된다.
static TICK_SEQ: AtomicU64 = AtomicU64::new(0);

/// 틱 에지가 났다. ★틱마다 한 번★ -- 타일마다가 아니다. 부르는 자리는 둘이고 어느
/// 구성에서 어느 쪽이 도는지는 `TICK_SEQ` 의 주석에 있다.
pub(crate) fn note_tick_edge() {
    TICK_SEQ.fetch_add(1, Ordering::Release);
}

/// 지금 틱의 번호.
pub(crate) fn current_tick_seq() -> u64 {
    TICK_SEQ.load(Ordering::Acquire)
}

/// ★타일 하나가 틱 하나에 쓸 수 있는 외부 서피스 표출 예산.★
///
/// `present_external_only` 는 WR 프레임을 안 거치고 DComp 에 바로 commit 하는 경로라
/// `generate_frame` 의 회계 밖에 있다. 그래서 자기 스로틀을 들고 있었는데 -- 직전 표출에서
/// 14ms -- 그 숫자는 표출 주기와 아무 관계가 없는 상수다. 120Hz 로 도는 벽의 주기는
/// 8.33ms 라 ★한 틱 걸러 한 번만★ 통과했다.
///
/// 박자는 틱이 준다. 천장("틱당 한 번")과 바닥("매 틱 한 번")이 같은 값이므로 상수 하나가
/// 사라지고, 원래 요구 -- "fast-path 는 WR 빌드와 무관하므로 WR 이 바쁠 때에도 발동해야
/// 한다"(`dcomp_compositor::should_fast_present`) -- 는 그대로 선다. 틱은 WR 의 바쁨과
/// 무관하게 매 주기 돌기 때문이다.
///
/// ★시각이 아니라 틱 번호를 쥔다.★ 그래야 "틱마다 한 번" 이 상수 없이 정의된다.
#[derive(Default)]
pub(crate) struct ExternalPresentBudget {
    last_tick: Option<u64>,
}

impl ExternalPresentBudget {
    /// 이번 틱(`tick`)의 몫이 남아 있으면 참을 돌려주고 소진한다.
    pub(crate) fn take(&mut self, tick: u64) -> bool {
        if self.last_tick == Some(tick) {
            return false;
        }
        self.last_tick = Some(tick);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExternalPresentBudget, TileOutcome, ack_is_releasable, schedule_production_at_next_edge,
        take_scheduled_edge, tile_may_generate,
    };

    /// ★틱마다 한 번은 반드시 받는다 -- 그것이 비디오의 바닥이다(불변식 4).★
    ///
    /// 전에는 직전 표출에서 14ms 라는 자기 상수로 걸렀다. 그 상수는 표출 주기와 무관해서
    /// 120Hz 벽(주기 8.33ms)에서는 한 틱 걸러 한 번만 통과했고, 빠진 틱의 비디오는 다음
    /// 틱까지 화면에 안 올라갔다. 박자는 틱이 준다.
    #[test]
    fn each_tick_gets_its_own_external_present() {
        let mut budget = ExternalPresentBudget::default();
        assert!(budget.take(1), "첫 틱은 자기 몫을 받는다");
        let second = budget.take(2);
        assert!(
            second,
            "새 틱은 자기 몫을 받아야 한다 (실제 {second}) -- 틱 번호가 1 에서 2 로 넘어갔다"
        );
    }

    /// ★천장도 같은 값이다 -- 틱당 한 번을 넘지 않는다.★ 도착마다 present 하면 36 타일에서
    /// 초당 1,080 Commit 이 된다.
    #[test]
    fn a_second_external_present_in_the_same_tick_is_refused() {
        let mut budget = ExternalPresentBudget::default();
        assert!(budget.take(7), "그 틱의 첫 표출은 나간다");
        let second = budget.take(7);
        assert!(
            !second,
            "같은 틱의 두 번째 표출은 거절해야 한다 (실제 {second}) -- 틱 번호가 둘 다 7 이다"
        );
    }

    /// ★예약은 한 번만 생산이 된다 -- 두 공개 함수가 같은 비트를 만지는지까지 본다.★
    /// 집어 가면 비트가 내려가야 한다: 안 내려가면 깨우기 한 번이 매 틱 생산이 되어 수요
    /// 구동 임베더가 자유 실행으로 변한다. `NEXT_EDGE_SCHEDULED` 를 만지는 시험은 이것 하나뿐
    /// 이라 전역이어도 병렬 실행에서 흔들리지 않는다.
    #[test]
    fn a_scheduled_edge_is_taken_exactly_once() {
        assert!(!take_scheduled_edge(), "예약이 없으면 생산하지 않는다");
        schedule_production_at_next_edge();
        schedule_production_at_next_edge();
        assert!(take_scheduled_edge(), "예약이 있으면 한 번 생산한다");
        assert!(!take_scheduled_edge(), "집어 간 뒤에는 비어 있어야 한다");
    }

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

    /// ★해제는 생산과 무관하다.★ 이 타일이 이번 틱에 합성했든 안 했든 같다 --
    /// 그러지 않으면 "모든 요청이 거절" 인 새 구조에서 빚이 영영 안 풀린다.
    #[test]
    fn a_debt_with_no_outstanding_images_is_releasable() {
        let releasable = ack_is_releasable(true, 0);
        assert!(
            releasable,
            "holds_debt=true pending_canvas_images=0 이면 풀어야 한다 (실제 {releasable})"
        );
    }

    /// 캔버스가 아직 안 올라갔으면 풀지 않는다 -- FrameDelayer 계약이다.
    #[test]
    fn a_debt_with_outstanding_images_waits() {
        let releasable = ack_is_releasable(true, 1);
        assert!(
            !releasable,
            "holds_debt=true pending_canvas_images=1 이면 기다려야 한다 (실제 {releasable})"
        );
    }

    #[test]
    fn no_debt_releases_nothing() {
        let releasable = ack_is_releasable(false, 0);
        assert!(
            !releasable,
            "빚이 없으면 보낼 것도 없다 (실제 {releasable})"
        );
    }

    // ★`only_a_generated_tile_counts_as_produced` 는 2026-10-22 Task 8 에서 삭제됐다.★
    // 그 시험이 보던 `TileOutcome::generated()` 와 함께 사라졌다 -- 거절한 타일이
    // `arm_keep_previous` 로 가게 되면서 분류가 참/거짓이 아니게 됐고, 분류는
    // `issue_wall_frame_request` 의 빠짐없는 `match` 하나가 한다. 같은 규칙("배리어는
    // 실제로 낸 타일만 기다린다")은 이제 시험이 아니라 ★컴파일러★ 가 지킨다.
}

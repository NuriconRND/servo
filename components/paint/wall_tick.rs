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
/// N 과 N-1 이 섞인다.
///
/// ★유지를 셸에 ***지시***할지는 사유에 달렸다★(2026-10-22 Task 9.5) --
/// `KeepPreviousReason` 과 `decline_may_arm_keep_previous` 를 볼 것. 지시 자체는
/// `WallFrameCoordinator::arm_keep_previous` 가 건다(2026-10-22 Task 8).
///
/// ★분류하는 자리는 하나다 -- `issue_wall_frame_request` 의 `match`.★ 2026-10-22 Task 8
/// 전에는 `generated()` 라는 술어가 그 일을 했는데, 거절한 타일도 갈 곳이 생기면서
/// 참/거짓 하나로는 모자라게 됐다.
///
/// ★컴파일러가 지키는 것은 빠짐없음(exhaustiveness)뿐이다.★ 변형을 더하면 그 `match` 가
/// 컴파일 오류가 나므로 ***분류되지 않는 변형***은 생길 수 없다. 그러나 아래 두 변종이
/// **어느 집합으로** 가야 하는지는 ★문서만 지킨다★ -- `KeepPrevious` 를
/// `generated_painter_ids` 에 밀어 넣어도 컴파일은 된다. 그 규칙을 깨는 변경은 리뷰에서
/// 잡는 수밖에 없다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TileOutcome {
    /// 이 타일이 이번 논리 프레임의 합성을 냈다. ★배리어가 기다리는 대상이다★ -- 이쪽만
    /// frame-ready 를 낸다.
    Generated,
    /// 이 타일은 이번 논리 프레임에 직전 프레임을 유지한다. ★배리어에 넣으면 안 된다★ --
    /// 트랜잭션을 안 보냈으므로 ***오지 않을 frame-ready 를*** 기다리게 되고, 그 논리
    /// 프레임이 마감까지 안 닫힌다.
    ///
    /// 사유를 싣는 이유는 하나다: ★사유에 따라 셸에 "렌더를 건너뛰라" 고 지시해도 되는지가
    /// 갈린다★(`decline_may_arm_keep_previous`).
    KeepPrevious(KeepPreviousReason),
}

/// 타일이 이번 논리 프레임을 거절한 ★사유★. 2026-10-22 Task 9.5.
///
/// 둘을 가르는 이유는 진단이 아니라 ★판정★ 이다 -- `decline_may_arm_keep_previous` 가
/// 이것만 보고 무장 여부를 정한다. 그 전에는 둘이 한 변형이었고, 그래서 자기잠금인 쪽까지
/// 무장했다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeepPreviousReason {
    /// ★임베더가 직전에 낸 프레임을 아직 렌더하지 않았다★(`Painter::renderer_behind`).
    /// 일시적인 상태이고, 푸는 자리는 `Painter::render` 의 끝 하나다.
    RendererBehind,
    /// 캔버스 이미지가 아직 WR 에 안 닿았다(`FrameDelayer::waiting_on_canvas_images`).
    /// ★이쪽은 낸 프레임이 없다★ -- `renderer_behind` 를 먼저 보고 거짓일 때만 여기 온다.
    WaitingOnCanvasImages,
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
/// ★`renderer_behind` 를 먼저 본다 -- 순서가 사유를 정한다.★ 둘 다 참이면
/// `RendererBehind` 다. 그쪽이 더 센 조건이기 때문이다: 낸 프레임이 아직 안 그려졌다는 뜻이고,
/// 그 상태에서는 무장이 자기잠금이 된다(`decline_may_arm_keep_previous`).
///
/// ## ★여기가 이 브랜치에서 유일하게 바닥을 ***좁힌*** 자리다★ (2026-10-22 전수 리뷰)
///
/// `waiting_on_canvas` 가 참이면 이 타일은 ***이번 틱 전체를*** 거절한다. 그런데 틱 생산은
/// 비디오의 바닥이기도 하다 -- 생산이 `flush_pending_video_frame_updates` 를 무조건 거치는
/// 것이 불변식 4 의 "아예 없음이 아닌 페인트 박자" 다. 그래서 ★캔버스를 기다리는 타일에
/// 승격되지 않은(= WR 텍스처로 올라가는) 비디오가 같이 있으면 그 비디오도 같이 선다.★
///
/// ★삭제된 `video_composite_owed` 블록에는 그 조건이 없었다★ -- 그쪽은 캔버스 대기와
/// 무관하게 비디오 빚을 갚았다. 이 브랜치의 다른 변경은 전부 바닥을 ***넓혔고***(생산자를
/// 하나로 모으면서 거절 사유가 줄었다), 여기 하나만 좁아졌으므로 적어 둔다.
///
/// ★크지 않다고 보는 근거★: 대기는 캔버스 이미지가 WR 에 닿을 때까지이고, 닿는 즉시
/// `PaintMessage::UpdateImages` 가 다음 에지를 예약해 그 틱이 비디오까지 같이 싣는다. 즉
/// 지연이지 정지가 아니다. 승격된(DComp 직행) 비디오는 애초에 이 경로 밖이라 영향이 없다.
/// 좁히려면 "캔버스 대기 타일도 비디오 flush 만은 한다" 가 필요한데, 그러면 거절 타일이
/// 트랜잭션을 내게 되어 `TileOutcome` 의 두 갈래 계약이 깨진다 -- 실기 벽에서 재기 전에는
/// 손대지 않는다.
pub(crate) fn tile_may_generate(renderer_behind: bool, waiting_on_canvas: bool) -> TileOutcome {
    if renderer_behind {
        return TileOutcome::KeepPrevious(KeepPreviousReason::RendererBehind);
    }
    if waiting_on_canvas {
        return TileOutcome::KeepPrevious(KeepPreviousReason::WaitingOnCanvasImages);
    }
    TileOutcome::Generated
}

/// 이 거절을 셸에 ★"렌더를 건너뛰라" 로 지시해도 되는가★. 2026-10-22 Task 9.5.
///
/// ★무장의 효과는 하나뿐이다 -- 임베더가 그 타일의 `paint_target` 과 `present` 를 둘 다
/// 건너뛴다★(`WallFrameCoordinator::keep_previous_logical_frame` 의 호출자:
/// `winit_wall/main.rs` 의 `render_all_tiles`, servoshell `window.rs` 의
/// `repaint_webviews`). 배리어와는 무관하다 -- `register_frame` 에는 ***생성한 타일만***
/// 넘어가고(`Paint::issue_wall_frame_request`), keep-previous 타일을 넣으면 오지 않을
/// frame-ready 를 기다려 그 논리 프레임이 안 닫힌다(`TileOutcome::KeepPrevious` 주석).
///
/// ## `RendererBehind` 에 무장하면 ★자기잠금★ 이다
///
/// 그 사유의 조건은 `display_composite_in_flight` 다. 그 깃발을 내리는 자리는 셋인데
/// ★무장이 걸린 타일에서는 셋 다 안 돈다★:
///
/// 1. `Painter::render` 의 끝 -- ★무장이 막는 것이 바로 이 렌더다.★
/// 2. `Painter::renderer_behind` 의 2 초 강제 해제 -- 거절 경로에서만 돌므로 ***다음
///    틱***이 와야 한다.
/// 3. `Painter::note_webrender_frame_ready` 의 `!need_repaint` 갈래 -- 이름대로 "뒤따를
///    렌더가 없다" 일 때만이고, 무장 중에는 `need_repaint` 가 참인 프레임이 남아 있다.
///
/// 그래서 무장은 ***자기 전제를 영구화한다***:
///
/// ```text
/// 거절(renderer_behind) -> 무장 -> 셸이 렌더를 건너뜀 -> in_flight 가 안 내려감
///   -> 다음 틱도 같은 사유로 거절 -> 다시 무장 -> ...
/// ```
///
/// 탈출구는 `renderer_behind()` 안의 ★2 초★ 강제 해제뿐이다. 즉 자유 실행 벽에서는 타일
/// 하나가 ***최대 2 초 멈춘다*** -- 2026-10-22 Task 8 이전에는 거절한 타일도 셸이 그대로
/// 렌더해 깃발이 그 패스에 풀렸고, 지연은 한 틱이었다. ★무장이 한 틱 딸꾹질을 2 초 정지로
/// 바꿨다.★
///
/// 수요 구동 임베더에서는 2 초 탈출구조차 안 돈다 -- 그 렌더가 곧 다음 깨우기의 출처라서
/// (렌더 -> `notify_will_paint` -> rAF -> `GenerateFrame` -> 예약) 틱 자체가 영영 오지
/// 않는다. 실측(2026-10-22, servoshell `-z` + rAF 페이지 8 초): 논리 프레임이 3 에서 멎고
/// `Wall repaint target skipped ... logical_frame_id=3` 이 초당 63 회, 새 프레임 0.
///
/// ## `WaitingOnCanvasImages` 는 자기잠금이 아니다
///
/// 그 타일은 ***낸 프레임이 없다***(`renderer_behind` 가 거짓일 때만 이 사유가 된다).
/// 기다림을 푸는 것은 렌더가 아니라 캔버스 쪽의 `update_images` 도착이라, 렌더를 건너뛰어도
/// 조건이 바뀌지 않는다. 무장이 아끼는 것은 ★같은 내용의 present 한 번★ 이다
/// (`WallFrameCoordinator::expire_keep_previous_before` 주석: "the skip only ever saved a
/// redundant present").
///
/// ★다만 공짜는 아니다 -- 적어 둔다.★ 무장은 `Painter::render` 를 막고, 그 함수의 맨 앞이
/// 캔버스 ack ***복구***(`Painter::flush_owed_canvas_ack`)다. 이미지가 도착해
/// `pending_canvas_images` 가 0 이 된 뒤에도 무장이 남아 있으면 그 복구가 그 타일에서 안
/// 돈다. 자유 실행 벽에서는 다음 틱(8.3ms)이 무장을 걷으므로 창이 한 틱이다. 수요 구동
/// 임베더에서는 더 길 수 있고, 그쪽은 `wall_frame_arms_keep_previous` 가 타일 하나짜리 벽을
/// 통째로 막아 준다.
pub(crate) fn decline_may_arm_keep_previous(reason: KeepPreviousReason) -> bool {
    match reason {
        KeepPreviousReason::RendererBehind => false,
        KeepPreviousReason::WaitingOnCanvasImages => true,
    }
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

/// 이 벽 프레임이 keep-previous 무장을 ★걸 수 있는 모양인가★. 2026-10-22 Task 9.5.
///
/// 게이트는 둘이고 ★서로 다른 것을 막는다★ -- 어느 쪽도 다른 쪽을 포함하지 않는다:
///
/// - 사유 게이트(`decline_may_arm_keep_previous`) -- ***어느 타일을*** 무장할 수 있나.
///   자기잠금인 `RendererBehind` 를 걸러 낸다. ★타일 수와 무관하다.★
/// - 이 함수 -- ***이 벽에*** 무장이라는 개념이 성립하나. 타일이 하나면 성립하지 않는다.
///
/// ★keep-previous 는 벽 ***단위*** 장치다.★ 설계 2-5 가 무장을 요구하는 이유는 한 가지다:
/// 거절한 타일을 조용히 빼면 "화면에 프레임 N 인 타일과 N-1 인 타일이 섞인다". 타일이
/// 하나뿐이면 섞일 상대가 없고, 남는 효과는 ★임베더가 그 하나뿐인 렌더를 건너뛴다★ 는
/// 위험뿐이다 -- 그 렌더는 캔버스 ack 복구(`Painter::flush_owed_canvas_ack`)가 사는 자리
/// 이기도 하다.
///
/// ★새 판정이 아니라 복원이다.★ 다른 무장 지점(`WallFrameCoordinator::miss_barrier`)은
/// 배리어가 등록된 프레임에서만 돌고, 배리어는 ***생성한 타일이 둘 이상***일 때만 등록된다
/// (`Paint::issue_wall_frame_request`). 즉 2026-10-22 Task 8 이전에 타일 하나짜리 벽의
/// painter 는 무장될 방법이 없었다.
pub(crate) fn wall_frame_arms_keep_previous(
    target_count: usize,
    any_armable_decline: bool,
) -> bool {
    any_armable_decline && target_count > 1
}

/// 이번 틱이 벽 프레임을 낼 차례인가. ★"쉰다" 사유는 스크린샷 하나뿐이다.★ 설계 2-8 /
/// 불변식 16, 2026-10-22 Task 9.
///
/// 막으려는 것: `maybe_take_screenshots` 와 `maybe_trigger_paint_for_screenshot` 는
/// `has_pending_frames()` 가 참이면 그 자리에서 돌아선다(`screenshot.rs`). 틱이 매 주기
/// 생산하면 그 값이 거의 항상 참이라 ★`ScreenshotTaker` 를 지나는 스크린샷이 영영 안
/// 찍힌다.★
///
/// ★`winit_wall --capture` 는 그 실행자가 ***아니다*** -- 2026-10-22 에 바로잡았다.★ 그
/// 플래그는 `ScreenshotTaker` 를 아예 안 지나고, 셸의 `maybe_capture` 가 주 타일의
/// `rendering_context.read_to_image()` 를 직접 부른다(`winit_wall/main.rs`; 그 파일에
/// `screenshot` 이라는 낱말이 한 번도 안 나온다). 설계 2-8 과 8 절이 이 규칙의 검증으로
/// `--capture` 를 지목한 것은 틀렸다.
///
/// ★살아 있는 실행자가 생겼다 -- 2026-10-22 Task 9.5.★ `ScreenshotTaker` 로 들어오는 문은
/// `Paint::request_screenshot` 하나이고 그것을 부르는 것은 servoshell / WPT 인데, 그 셸의
/// 틱이 그날 배선됐다(`RunningAppState::spin_event_loop` 의 `on_demand_wall_tick`).
/// 그래서 이 규칙은 ***지금 매 스크린샷마다 발화한다***. (그 셸의 틱은 끌 수 없었고 --
/// 대안 생산자가 없어 끄면 순백 화면이다 -- 2026-10-22 부터는 `winit_wall` 도 같다:
/// 생산을 끄던 `gfx_single_frame_producer` 가 그날 사라졌다.)
///
/// 실측(2026-10-22, 배선 당일): 헤드리스 `servoshell -z -x -o` 가 0.3 초에 정상 이미지를
/// 저장했다 -- 이 규칙이 생산을 쉬게 하고, WR frame-ready 가 `pending_frames` 를 비우고,
/// 렌더 끝의 `maybe_take_screenshots` 가 찍는 순서가 교착 없이 돌았다.
///
/// 벽(`winit_wall`)에서 이 규칙이 도는지는 `WALLSHOTPAUSE` 로그 한 줄이 말해 주고, 그
/// 셸은 `ScreenshotTaker` 를 안 지나므로 그 줄이 ***안 나오는 것***이 정상이다.
///
/// ★규칙 3 을 어기지 않는다.★ 이 질문은 프레임의 *내용* 이 아니라 ***외부 모드*** 를
/// 묻는다 -- 생산자는 여전히 프레임에 무엇이 들었는지 모른다.
///
/// ★인자는 "요청이 떠 있는가" 가 아니라 "`WaitingOnFrame` 에 닿은 요청이 있는가" 다.★
/// 그 차이가 동작과 교착을 가른다. 상태 기계를 미는 자리
/// (`Painter::advance_screenshot_requests`)의 호출자는 `generate_frame_for_wall_tick` 의
/// `Generated` 갈래 하나이므로 ★전진하려면 생산해야 한다★ -- "요청이 있으면 쉰다" 로 두면
/// 요청이 `WaitingOnPipelineDisplayLists` 에서 영영 못 나가고 생산이 영구 정지한다.
/// 상별 판정은 `ScreenshotRequestPhase::suspends_production` 한 자리에 있다.
pub(crate) fn tick_should_produce(screenshot_waiting_on_frame: bool) -> bool {
    !screenshot_waiting_on_frame
}

/// 부트스트랩 한 장을 허용할지. ★설계 7 절 예외의 종료 조건.★ 2026-10-22 Task 9.
///
/// `Painter::send_initial_pipeline_transaction` 은 틱 말고 WR 프레임을 내는 유일한
/// 자리다. 틱 루프를 믿을 수 있기 전의 한 장이라 남기지만, 조건이 없으면 그것은 예외가
/// 아니라 ★두 번째 생산자★ 다(규칙 1).
///
/// 둘 다 만족해야 낸다:
///
/// 1. `already_bootstrapped` 가 거짓 -- ★파이프라인당 정확히 1 회.★ 같은 파이프라인에
///    `SendInitialTransaction` 이 또 오면 디스플레이 리스트만 싣고 프레임은 다음 틱에
///    맡긴다(값 적재는 생산이 아니다).
/// 2. `tick_frames_produced == 0` -- ★그 타일의 첫 틱 생산 이전에만.★ 틱이 이미 한 장이라도
///    냈다면 틱 루프는 믿을 수 있고 예외가 할 일이 없다. ★이것이 ***종료*** 조건이다★ --
///    계수기가 단조 증가하므로 한 번 0 을 벗어나면 이 타일에서 다시는 통과하지 않는다.
///
/// ★생산이 아예 없는 구성에서는 2 번이 영원히 참이고, 그래도 기동 화면은 검지 않다★ --
/// 1 번이 여전히 파이프라인당 1 회로 묶는다. (그런 구성은 2026-10-22 에 사라졌다. 틱 생산을
/// 끄던 `gfx_single_frame_producer` 가 그날 삭제되어, 지금은 모든 셸이 틱마다 생산한다.
/// 그 뒤 이 조건은 ***종료*** 조건으로만 돈다.)
pub(crate) fn bootstrap_frame_allowed(
    already_bootstrapped: bool,
    tick_frames_produced: u64,
) -> bool {
    !already_bootstrapped && tick_frames_produced == 0
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
/// 박자는 틱이 준다: 상수 하나가 사라지고, 원래 요구 -- "fast-path 는 WR 빌드와 무관하므로
/// WR 이 바쁠 때에도 발동해야 한다"(`dcomp_compositor::should_fast_present`) -- 는 그대로
/// 선다. 틱은 WR 의 바쁨과 무관하게 매 주기 돌기 때문이다.
///
/// ★시각이 아니라 틱 번호를 쥔다.★ 그래야 "틱당 한 번 이하" 가 상수 없이 정의된다.
///
/// ## ★이것은 천장이다. 바닥이 아니다.★ 2026-10-22 전수 리뷰에서 바로잡았다
///
/// 이 구조체의 초안 주석은 천장("틱당 한 번")과 바닥("매 틱 한 번")이 같은 값이라고 적었다.
/// ★거짓이다.★ `take` 는 ***불려야*** 무언가를 하고, 부르는 자리는
/// `Painter::update_images` 안의 즉시-합성 게이트 하나뿐이다 -- 즉 ***외부 서피스가 도착한
/// 틱***에만 불린다. 도착이 없는 틱에는 아무도 `take` 를 부르지 않으므로 그 틱의
/// `present_external_only` 도 없다. 이 예산이 주는 것은 "틱당 한 번을 넘지 않는다" 하나다.
///
/// ★설계 7 절과의 차이를 여기 적어 둔다.★ 7 절은 fast-path 를 ***틱 소유로 이관***하라고
/// 했고(그러면 바닥이 생긴다), 실제로 착지한 것은 ***도착 구동 호출의 틱 단위
/// 속도 제한***이다. 그 차이는 좁힐 수 있지만 DComp commit 경로의 거동 변경이고, 이
/// 저장소에는 ★commit 이 렌더 스레드로 새면 눈에 보이는 저더가 난다★ 는 실측 기록이 있어
/// 실기 벽 없이는 재지 못한다. 그래서 2026-10-22 에는 ***주장만 바로잡고 거동은 두었다***.
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
        ExternalPresentBudget, KeepPreviousReason, TileOutcome, ack_is_releasable,
        bootstrap_frame_allowed, decline_may_arm_keep_previous, schedule_production_at_next_edge,
        take_scheduled_edge, tick_should_produce, tile_may_generate, wall_frame_arms_keep_previous,
    };

    /// ★새 틱은 몫을 ***거절당하지 않는다***.★ 예산이 주는 보장은 이것 하나다.
    ///
    /// 전에는 직전 표출에서 14ms 라는 자기 상수로 걸렀다. 그 상수는 표출 주기와 무관해서
    /// 120Hz 벽(주기 8.33ms)에서는 한 틱 걸러 한 번만 통과했고, 빠진 틱의 비디오는 다음
    /// 틱까지 화면에 안 올라갔다. 박자는 틱이 준다.
    ///
    /// ★이 시험을 불변식 4 의 바닥으로 읽지 말 것 -- 바닥은 여기 없다.★ 초안 이름과 주석
    /// ("틱마다 한 번은 반드시 받는다")은 틀렸다: `take` 를 부르는 자리는
    /// `Painter::update_images` 하나이고 외부 서피스가 ***도착한*** 틱에만 불린다. 도착이
    /// 없으면 이 예산은 아예 조회되지 않는다. 근거와 설계 7 절과의 차이는
    /// [`ExternalPresentBudget`] 주석에 있다. 비디오의 바닥을 주는 것은 틱 생산이 무조건
    /// 거치는 `flush_pending_video_frame_updates` 쪽이지 이 경로가 아니다.
    #[test]
    fn a_new_tick_is_not_refused_its_external_present() {
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
        assert_eq!(
            tile_may_generate(true, false),
            TileOutcome::KeepPrevious(KeepPreviousReason::RendererBehind)
        );
    }

    /// 둘 다 참이면 ★`RendererBehind` 가 이긴다★ -- 무장 금지 쪽으로 기울어야 안전하다.
    /// 뒤집히면 낸 프레임이 있는 타일이 무장되어 자기잠금이 돌아온다.
    #[test]
    fn a_tile_that_is_both_behind_and_waiting_reports_being_behind() {
        assert_eq!(
            tile_may_generate(true, true),
            TileOutcome::KeepPrevious(KeepPreviousReason::RendererBehind)
        );
    }

    /// ★캔버스를 기다려도 liveness 는 물어본 뒤다.★ 순서가 뒤집히면 2 초 강제 해제가
    /// 영영 안 돈다 -- 그것이 "4 개 중 2 개 타일 44 초 사망" 조건이다.
    #[test]
    fn a_tile_waiting_on_canvas_keeps_its_previous_frame() {
        assert_eq!(
            tile_may_generate(false, true),
            TileOutcome::KeepPrevious(KeepPreviousReason::WaitingOnCanvasImages)
        );
    }

    #[test]
    fn a_free_tile_generates() {
        assert_eq!(tile_may_generate(false, false), TileOutcome::Generated);
    }

    /// ★역압 거절은 절대 무장하지 않는다 -- 무장이 자기 전제를 영구화한다.★
    ///
    /// `display_composite_in_flight` 를 내리는 자리 셋 중 무장한 타일에서 도는 것은
    /// ***하나도 없다***(`decline_may_arm_keep_previous` 주석의 1·2·3). 참으로
    /// 뒤집히면 자유 실행 벽에서 타일 하나가 2 초(강제 해제 주기)
    /// 멈추고, 수요 구동 임베더에서는 그 렌더가 다음 깨우기의 출처라 ***영구 정지***다
    /// (실측 2026-10-22: 논리 프레임이 3 에서 멈추고 skip 로그가 초당 63 회).
    #[test]
    fn a_back_pressure_decline_is_never_armed() {
        let armable = decline_may_arm_keep_previous(KeepPreviousReason::RendererBehind);
        assert!(
            !armable,
            "역압 거절에 무장하면 무장이 막는 렌더가 곧 그 역압을 푸는 자리다 \
             (실제 {armable})"
        );
    }

    /// 캔버스 대기 거절은 낸 프레임이 없으므로 자기잠금이 아니다 -- 무장해도 된다.
    /// 기다림을 푸는 것은 렌더가 아니라 `update_images` 도착이다.
    #[test]
    fn a_canvas_wait_decline_may_be_armed() {
        let armable = decline_may_arm_keep_previous(KeepPreviousReason::WaitingOnCanvasImages);
        assert!(
            armable,
            "캔버스 대기는 렌더와 무관하므로 같은 내용의 present 한 번을 아껴도 된다 \
             (실제 {armable})"
        );
    }

    /// ★타일이 여럿일 때만 무장한다 -- 그때만 섞일 상대가 있다.★ 설계 2-5.
    #[test]
    fn a_declining_tile_on_a_multi_tile_wall_is_armed() {
        let armed = wall_frame_arms_keep_previous(4, true);
        assert!(
            armed,
            "타일 4 개 중 하나가 무장 가능한 사유로 거절하면 세워야 한다 (실제 {armed})"
        );
    }

    /// ★타일이 하나면 무장에 뜻이 없고 위험만 남는다.★ 섞일 상대가 없고, 건너뛰는 그
    /// 렌더가 캔버스 ack 복구(`Painter::flush_owed_canvas_ack`)가 사는 자리다.
    #[test]
    fn a_declining_tile_on_a_single_tile_wall_is_not_armed() {
        let armed = wall_frame_arms_keep_previous(1, true);
        assert!(
            !armed,
            "타일이 하나면 섞일 상대가 없고 무장은 하나뿐인 렌더를 막는다 (실제 {armed})"
        );
    }

    /// 무장 가능한 거절이 없으면 타일 수와 무관하게 무장하지 않는다.
    #[test]
    fn a_wall_frame_with_no_armable_decline_arms_nothing() {
        assert!(!wall_frame_arms_keep_previous(4, false));
        assert!(!wall_frame_arms_keep_previous(1, false));
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

    /// 스크린샷이 프레임을 기다리는 동안은 생산을 쉰다 -- 매 틱 생산하면
    /// `has_pending_frames()` 가 영영 참이라 `--capture` 가 안 찍힌다.
    /// ★이것은 프레임의 *내용* 을 묻는 것이 아니라 외부 모드이므로 규칙 3 을 어기지 않는다.★
    #[test]
    fn a_screenshot_waiting_on_a_frame_suspends_production() {
        let should_produce = tick_should_produce(true);
        assert!(
            !should_produce,
            "`WaitingOnFrame` 요청이 있으면 쉬어야 한다 (실제 {should_produce})"
        );
    }

    #[test]
    fn production_resumes_once_the_screenshot_is_taken() {
        let should_produce = tick_should_produce(false);
        assert!(
            should_produce,
            "기다리는 요청이 없으면 매 틱 생산한다 (실제 {should_produce})"
        );
    }

    /// ★예외는 그 타일의 첫 틱 생산 이전에, 파이프라인당 한 번만 통과한다.★
    #[test]
    fn the_bootstrap_frame_is_allowed_before_the_first_tick_production() {
        let allowed = bootstrap_frame_allowed(false, 0);
        assert!(
            allowed,
            "처음 보는 파이프라인이고 틱이 아직 아무것도 안 냈으면 통과한다 (실제 {allowed})"
        );
    }

    /// 같은 파이프라인의 두 번째 `SendInitialTransaction` 은 프레임을 내지 않는다 --
    /// 디스플레이 리스트만 싣고 다음 틱에 맡긴다.
    #[test]
    fn a_second_bootstrap_for_the_same_pipeline_is_refused() {
        let allowed = bootstrap_frame_allowed(true, 0);
        assert!(
            !allowed,
            "이미 부트스트랩한 파이프라인은 거절한다 (실제 {allowed}, tick_frames_produced=0)"
        );
    }

    /// ★이것이 ***종료*** 조건이다.★ 계수기는 단조 증가하므로 한 번 0 을 벗어나면 이
    /// 타일에서 부트스트랩은 다시는 통과하지 않는다 -- 틱 루프를 믿을 수 있게 된 뒤의
    /// 예외는 두 번째 생산자일 뿐이다(규칙 1).
    #[test]
    fn the_bootstrap_is_refused_once_the_tick_has_produced() {
        let allowed = bootstrap_frame_allowed(false, 1);
        assert!(
            !allowed,
            "틱이 한 장이라도 냈으면 거절한다 (실제 {allowed}, already_bootstrapped=false)"
        );
    }
}

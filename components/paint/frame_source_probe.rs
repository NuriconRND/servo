/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! WR 프레임이 ★누가 발행했는지★ 세는 프로브.
//!
//! ## 무엇을 재는가
//!
//! 규칙 1 은 "프레임 생산자는 단일하다" 다. 이 프로브는 그것을 발행 지점에서 직접 센다 --
//! [`Painter::generate_frame_with_diagnostic_id`] 가 트랜잭션 `generate_frame` 의 ***유일한
//! 깔때기***이고, 벽 틱 경로만 `wall_logical_frame_id` 를 들고 온다(다른 자리는 전부 `None`).
//! 그래서 그 하나로 ★틱이 낸 것★과 ★그 밖이 낸 것★이 갈린다.
//!
//! `other` 쪽은 이유 비트를 OR 로 모아 둔다 -- 어느 자리가 냈는지 이름이 남는다.
//!
//! ## 왜 이것을 재는가 (2026-11-02)
//!
//! 최소 WebGL2 삼각형 한 장에서 HUD 가 ★65fps★ 를 찍었다(저더 동반). 다른 콘텐츠는 같은
//! 조건에서 60~61 이었다. 그 5 의 출처를 셸에서 찾으려 했고 ***틀렸다***:
//!
//! - `PASSSOURCE`(2026-11-02, log_ani_shift_01/14) 가 ★틱 60.0/s, 패스 60/s, OS redraw 는
//!   런 전체에 4 건★ 임을 보였다. WebGL 런과 대조군이 똑같았다 -- 셸의 두 번째 입구
//!   (`WindowEvent::RedrawRequested`)는 범인이 아니다.
//! - 그래서 "HUD fps = 패스 횟수" 라는 전제가 틀렸다. HUD 는 `render_impl` 진입을 세는데,
//!   그 함수의 호출처는 `pub fn render` ***하나가 아니라 다섯***이고 넷이
//!   `Renderer::update()` 안에 있다. 그리고 `painter.rs` 는 매 패스마다 `renderer.render()`
//!   직전에 `renderer.update()` 를 부른다.
//! - `update()` 는 ***그려지지 않은 채 남은*** 프레임이 텍스처 캐시 작업을 품고 있으면
//!   (`must_be_drawn() = has_texture_cache_tasks && !has_been_rendered`) 그것을 그 자리에서
//!   렌더해 작업을 잃지 않게 한다.
//!
//! 즉 HUD 가 60 을 넘으면 ★패스당 WR 프레임이 둘 이상 발행된 것★이다. 이 프로브가 그
//! 발행을 센다.
//!
//! ## 성능 규칙
//!
//! ★타이밍 경로에 I/O 금지.★ 쌓는 쪽은 원자 둘(계수기 하나와 이유 비트 OR)이고, 출력은
//! 전용 리포터 스레드가 [`REPORT_EVERY`] 마다 한다. 꺼져 있으면 `AtomicBool` 로드 하나다.
//! 쌓기 비용의 상한은 `tests::noting_a_frame_is_cheap_enough` 가 고정한다.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 리포터가 누적을 찍는 간격.
const REPORT_EVERY: Duration = Duration::from_secs(10);

static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

/// 벽 틱이 발행한 프레임. ★이것만 있어야 한다.★
static TICK_FRAMES: AtomicU64 = AtomicU64::new(0);
/// 그 밖이 발행한 프레임.
static OTHER_FRAMES: AtomicU64 = AtomicU64::new(0);
/// `other` 들의 `RenderReasons` 비트를 OR 로 모은 것 -- 어느 자리가 냈는지 남는다.
static OTHER_REASON_BITS: AtomicU64 = AtomicU64::new(0);

/// `Renderer::update()` 시간 히스토그램의 칸 수. 칸 0 은 "16µs 이하", 그 위는 2 배씩이다.
///
/// ★이 자리를 재는 이유★(2026-11-02): `update()` 는 결과 메시지를 비우면서, 그려지지 않은
/// 채 남은 프레임이 텍스처 캐시 작업을 품고 있으면 그것을 ***그 자리에서 렌더***한다
/// (`must_be_drawn()`, frame_builder.rs:1379). 그 렌더는 `device_size = None` 으로 들어가므로
/// ***표출하지 않는다*** -- renderer/mod.rs:1674 의 주석이 "Setting device_size to None is
/// what ensures compositing/presenting the frame is skipped" 라고 명시한다.
///
/// 그래서 그것은 ★프레임을 더 내보내는 것이 아니라 패스 안에서 시간을 더 먹는 것★이다.
/// `painter.rs` 가 `renderer.render()` ***직전에*** `update()` 를 동기로 부르므로, 흘림이
/// 일어난 패스는 그만큼 길어지고 16.7ms 예산을 넘으면 틱을 놓친다.
///
/// 실측 근거: 최소 WebGL2 삼각형에서 HUD 가 페인터당 61~65, ★타일 0 은 간헐적으로 70~80★
/// 이었고 ***그 순간에 저더가 심하게 보였다***(log_ani_shift_01/15). HUD 는 `render_impl`
/// 진입을 세므로 그 초과분이 곧 흘림 횟수다. 대조군(스크롤 텍스트)은 59~63 이고 그런
/// 치솟음이 없었다.
const UPDATE_BUCKETS: usize = 12;

/// 칸 0 의 상한, 나노초. 이 아래는 "비용 없음" 으로 센다.
const UPDATE_FLOOR_NANOS: u64 = 16_000;

/// `Renderer::update()` 소요 시간의 분포.
static UPDATE_HIST: [AtomicU64; UPDATE_BUCKETS] = [const { AtomicU64::new(0) }; UPDATE_BUCKETS];
/// 관측된 최대 `update()` 소요 시간, 나노초.
static UPDATE_WORST_NANOS: AtomicU64 = AtomicU64::new(0);

/// 틱 생산이 `KeepPrevious(RendererBehind)` 로 거절된 횟수.
static KEEP_BEHIND: AtomicU64 = AtomicU64::new(0);
/// 틱 생산이 `KeepPrevious(WaitingOnCanvasImages)` 로 거절된 횟수.
///
/// ★이 둘을 가르는 계수기가 트리에 없었다.★ 이유를 찍는 유일한 자리
/// (`paint.rs:2215`)는 `info!` 이고 ***다중 타일 벽이 한 장 이하를 냈을 때만*** 돌아서
/// 정상 운영에서는 나오지 않는다(벽 런은 `RUST_LOG=warn` 이라 더 그렇다).
///
/// ★왜 갈라야 하는가★(2026-11-02): 발행이 틱의 약 83% 였고 나머지 17% 가 거절이다.
/// 그 17% 가 캔버스 대기인지 역압인지에 따라 볼 곳이 전혀 다르다. 그리고 운영자 관찰이
/// 캔버스 쪽을 가리킨다 -- ***ticker 단독·ticker+영상은 멀쩡한데 ticker+WebGL 이면
/// ticker 가 저더한다***. 코드상 `DelayNewFrameForCanvas` 는 타일 범위 없이 WebView 의
/// 네 페인터 전부에 뿌려지므로(`paint.rs:2609`), 캔버스 없는 타일도 같이 무장된다.
///
/// ★`RendererBehind` 가 먼저 검사되고 더 센 조건이다★(`wall_tick.rs:102-105`). 둘 다
/// 걸린 틱은 `behind` 로 센다 -- 그래서 `canvas` 는 ***하한***이다.
static KEEP_CANVAS: AtomicU64 = AtomicU64::new(0);

/// `animating_webviews` 가 WebView 를 ★paint 측으로 판정★ 한 횟수(깎임 후보).
static PAINT_SIDE_JUDGED: AtomicU64 = AtomicU64::new(0);
/// 그중 ★`canvas_is_live()` 가 판정을 되돌린★ 횟수.
///
/// ★이 둘이 `gfx_wall_canvas_defeats_paint_side_throttle` 가 바꾸는 바로 그 양이다.★
/// 2026-11-02 에 그러지 못한 계측을 한 번 설계했다 -- `KEEP_CANVAS` 는 ***거절***을 세는데
/// `-NoArmOnCanvasWait` 는 ***무장***만 끄므로, 수치가 안 움직인 것이 당연했고 그것으로는
/// 그 수정의 성패를 말할 수 없었다. ★계측은 수정이 바꾸는 양과 같은 양을 세야 한다.★
///
/// 왜 필요한가(log_webgl_perf_01/02): 같은 스위치로 ***프로브 페이지는 캔버스 거절이
/// 2.0% -> 0.1% 로 떨어지고 저더가 사라졌는데***(run 08), ***실제 output 페이지는 1.5% 로
/// 남고 저더가 그대로다***(run 11). 즉 그 페이지에서는 판정이 되돌려지지 않는다. 이
/// 둘이 그 이유를 가른다 -- `judged` 가 양수인데 `defeated` 가 0 이면 ★신호가 안 잡히는
/// 것★이고, 둘 다 양수인데 거절이 남으면 ★깎임 외의 원인★이다.
static PAINT_SIDE_DEFEATED: AtomicU64 = AtomicU64::new(0);
/// 애니메이션 틱이 ★어느 박자로 몇 번 실제로 나갔는지★. 박자별로 센다.
///
/// ★이것이 없어서 가장 유력한 가설을 로그로 확인할 수 없었다.★ `painter.rs` 의 cadence
/// 결정 주석이 2026-11-02 실측을 이렇게 적어 뒀다 -- "ticker 와 WebGL 을 함께 표출하면
/// ***둘 다*** 저더한다. ticker 가 `has_paint_animations` 로 이 WebView 를 paint 측으로
/// 만들고, 런처 기본값 divisor=15 가 ***WebGL 의 rAF 를 초당 네 번으로 깎는다***." 60Hz
/// 에서 15 로 나누면 4/s 이고, 운영자는 1/4/15/30/60Hz 를 눈으로 구별한다.
///
/// 그런데 ***그 4/s 를 찍는 계수기가 없었다***. 28 번 실기(60fps WebGL + ticker, 육안
/// judder)에서 `PRODUCE` 는 60.0/s, `KEEPPREV` 는 100%, scissor 는 100% 였고 -- 전부
/// 표출 틱의 수치다. 애니메이션 틱은 그와 ***다른 박자***이고, 그것을 아무도 세지 않았다.
///
/// 칸: 0 = Full, 1 = PaintSide, 2 = LiveCanvas, 3 = 틱할 WebView 가 없어 건너뜀.
static TICK_CADENCE: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// 관측한 프레임 수(= 표출 틱). 위 칸들과의 비율이 실제 약수다.
static CADENCE_FRAMES: AtomicU64 = AtomicU64::new(0);

/// 박자 칸 이름. 로그 순서와 같다.
pub(crate) const CADENCE_FULL: usize = 0;
pub(crate) const CADENCE_PAINT_SIDE: usize = 1;
pub(crate) const CADENCE_LIVE_CANVAS: usize = 2;
pub(crate) const CADENCE_NONE: usize = 3;

/// 이 프레임에 어느 박자의 WebView 가 틱을 받았는지 적는다. ★원자 증가뿐이다.★
///
/// 한 프레임에 여러 박자가 함께 나갈 수 있으므로 칸마다 따로 센다 -- 합이 프레임 수를
/// 넘는 것이 정상이다.
pub(crate) fn note_tick_cadence(frames: u64, buckets: [bool; 3], skipped: bool) {
    if !is_enabled() {
        return;
    }
    CADENCE_FRAMES.store(frames, Ordering::Relaxed);
    for (i, hit) in buckets.iter().enumerate() {
        if *hit {
            TICK_CADENCE[i].fetch_add(1, Ordering::Relaxed);
        }
    }
    if skipped {
        TICK_CADENCE[CADENCE_NONE].fetch_add(1, Ordering::Relaxed);
    }
}

/// `Painter::render` 한 번의 소요 시간 분포. 칸은 [`UPDATE_FLOOR_NANOS`] 를 공유한다.
///
/// ★이것이 "관계없는 요소까지 저더하는" 이유를 재는 자리다.★ 벽은 ***하나의 논리
/// 프레임***이고 셸이 네 타일을 조인하므로(`winit_wall/main.rs` 의 `render_all_tiles`),
/// 어느 타일이든 렌더가 길어지면 ***그 프레임에 실린 모든 것***이 늦는다. WebGL 캔버스는
/// 외부 이미지라서 WR 이 합성할 때 비트맵을 가져오고(`lock_swap_chain`), 그 호출은
/// ***렌더 패스 안에서 동기로*** 일어난다. 그래서 그 비용은 캔버스만의 비용이 아니다.
///
/// ★`PASSSOURCE` 가 60.0/s 라는 것은 패스가 제시간에 끝난다는 뜻이 아니다.★ 페이싱
/// 스레드는 패스가 길어도 그대로 틱하므로 ***횟수는 유지되면서 프레임만 늦는다***. 그
/// 구분을 못 해서 2026-11-03 에 "패스는 제시간에 돈다" 고 잘못 읽었다.
static RENDER_HIST: [AtomicU64; UPDATE_BUCKETS] = [const { AtomicU64::new(0) }; UPDATE_BUCKETS];
/// 관측된 최대 `Painter::render` 소요 시간, 나노초(런 전체).
static RENDER_WORST_NANOS: AtomicU64 = AtomicU64::new(0);
/// 직전 보고 구간의 최대값. ★리포터가 읽고 0 으로 돌린다★ -- 누적 최대값은 한 번 튀면
/// 영원히 남아서 "전반이 심하고 후반에 가라앉는다" 를 가린다. 운영자가 본 그 모양을
/// 수치로 보려면 구간값이 필요하다.
static RENDER_WORST_INTERVAL_NANOS: AtomicU64 = AtomicU64::new(0);
/// 렌더가 한 주기([`FRAME_PERIOD_NANOS`])를 넘은 횟수.
static RENDER_OVER_PERIOD: AtomicU64 = AtomicU64::new(0);
/// 직전 보고 구간의 넘김 횟수. 리포터가 읽고 0 으로 돌린다.
static RENDER_OVER_INTERVAL: AtomicU64 = AtomicU64::new(0);

/// 60Hz 한 주기, 나노초. 넘김 판정의 기준이다.
const FRAME_PERIOD_NANOS: u64 = 16_667_000;

/// `Paint::produce_wall_frame` 이 불린 횟수. ★틱당 1 이어야 한다.★
/// 앞 리포트의 틱 에지 수와 생산 호출 수. 구간 차분용이다.
static PREV_EDGES: AtomicU64 = AtomicU64::new(0);
static PREV_CALLS: AtomicU64 = AtomicU64::new(0);
static PRODUCE_CALLS: AtomicU64 = AtomicU64::new(0);
/// 그중 ★대상 타일이 없어 아무것도 하지 않은★ 횟수(`target_painter_ids.is_empty()`).
static PRODUCE_NO_TARGETS: AtomicU64 = AtomicU64::new(0);
/// 그중 ★스크린샷이 생산을 쉬게 한★ 횟수(`tick_should_produce` 가 거짓).
static PRODUCE_HELD: AtomicU64 = AtomicU64::new(0);
/// 요청이 물은 (페인터, 틱) 쌍의 수 -- `target_painter_ids.len()` 의 합.
///
/// ★이것이 KEEPPREV 의 `attempts` 가 되어야 했던 값이다.★ 그쪽은 발행+거절로 만든
/// 유도값이라, 실측에서 199/s 가 나왔을 때 ***240/s 와의 간격을 설명할 수단이 없었다***
/// (2026-11-03, log_webgl_perf_01/05). 네 페인터가 매 틱 답한다는 것은 코드로 확인했다 --
/// `with_each_painter_mut` 의 `None` 갈래는 페인터 스레드가 죽었을 때만 나오고 그때
/// `warn!` 을 찍는데, 로그에 그 경고가 없다. 그러면 240/s 가 나와야 한다.
static PRODUCE_TARGETS: AtomicU64 = AtomicU64::new(0);
/// `outcome` 이 `None` 이었던 (페인터, 틱) 쌍. ★0 이어야 한다★ -- 양수면 위 추론이 틀렸다.
static OUTCOME_NONE: AtomicU64 = AtomicU64::new(0);
/// `canvas_is_live()` 가 거짓이었을 때, 마지막 캔버스 활동으로부터 지난 최대 시간(ms).
///
/// ★창(500ms)이 모자란 것인지 아예 캔버스가 안 오는 것인지 가른다.★ 활동이 한 번도 없으면
/// 갱신되지 않으므로 0 으로 남는다.
static CANVAS_STALE_WORST_MS: AtomicU64 = AtomicU64::new(0);

/// 프로브가 켜져 있는가.
pub(crate) fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 프로브를 켜고 리포터 스레드를 띄운다. ★여러 페인터가 불러도 한 번만 띄운다.★
pub(crate) fn start() {
    if STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    ENABLED.store(true, Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name(String::from("FrameSource"))
        .spawn(|| {
            let started = std::time::Instant::now();
            loop {
                std::thread::sleep(REPORT_EVERY);
                report(started.elapsed());
            }
        });
    if let Err(error) = spawned {
        log::warn!("frame-source: 리포터 스레드를 띄우지 못했다: {error}; 요약이 나오지 않는다");
    }
}

/// paint 측 판정과, 캔버스 신호가 그것을 되돌렸는지를 센다. ★원자 둘 이하다.★
///
/// `stale_for` 는 신호가 거짓일 때 ***마지막 캔버스 활동으로부터 지난 시간***이고,
/// 활동이 한 번도 없었으면 `None` 이다.
pub(crate) fn note_paint_side_judgement(defeated: bool, stale_for: Option<Duration>) {
    if !is_enabled() {
        return;
    }
    PAINT_SIDE_JUDGED.fetch_add(1, Ordering::Relaxed);
    if defeated {
        PAINT_SIDE_DEFEATED.fetch_add(1, Ordering::Relaxed);
    } else if let Some(stale_for) = stale_for {
        let millis = stale_for.as_millis().min(u128::from(u64::MAX)) as u64;
        CANVAS_STALE_WORST_MS.fetch_max(millis, Ordering::Relaxed);
    }
}

/// `Painter::render` 한 번의 소요 시간을 쌓는다. ★원자 다섯 이하다.★
/// `render()` 안을 구간으로 쪼개 잰다. `RENDERCOST` 와 ★같은 범위★ 이므로 합이 그 값과 맞는다.
///
/// ## 왜 (2026-11-13)
///
/// 측정된 "느림" 이 하나뿐인데 분해가 없었다.
///
///     RENDERCOST  58: over_period=258 (1.18%)  ★worst=60.3ms★  <2048us:78.2%
///                 56: over_period=175 (0.80%)  ★worst=62.1ms★  <2048us:82.1%
///
/// 82% 가 2ms 안인데 최악이 60ms -- 60Hz 주기의 ★3.6 배★ 이고 초당 2.3 회 난다. 그런데
/// `RENDERCOST` 는 전체 시간만 재므로 그 60ms 안에서 무엇이 시간을 쓰는지 아무 데이터도 없다.
///
/// 전수 열거 12 자리가 전부 기각된 뒤(마지막 `busy map` 은 60 번에서 앞 판 동작으로도 대기
/// max 54us) 남은 것은 ***이미 측정된 꼬리를 쪼개는 것*** 이다. ticker 단독과 비교하지 않아도
/// 60ms 렌더는 그 자체로 결함이다.
///
/// ## 구간
///
///     lock     angle_gl_lock + make_current + prepare_for_rendering
///     update   Renderer::update()          -- `UPDATECOST` 가 이미 잰다(free=99.9%)
///     draw     clear_background + Renderer::render()   ★한 번도 측정 안 됨★
///     post     screenshots + paint metrics + composite_in_flight 해제
///     pump     pump_paint_animation(Render)  -- ticker 보간이 여기다
///     present  RenderingContext::present()  ★한 번도 측정 안 됨★
///
/// ## 비용
///
/// 경계 여섯에 `Instant::now()` 이고 240/s 이면 약 36us/s 다. 꼬리 줄은 주기(16.67ms)를 넘긴
/// 프레임에만 나오므로 실측 1.18% = 초당 2.8 줄이다. 꺼져 있으면 `None` 하나다.
/// 구간 수. 순서는 lock/update/clear/draw/post/pump/present 다.
const PARTS: usize = 7;

pub(crate) struct RenderSplit {
    at: Option<Instant>,
    lock_us: u64,
    update_us: u64,
    clear_us: u64,
    draw_us: u64,
    post_us: u64,
    pump_us: u64,
    /// `Renderer::render` 가 돌려준 통계 중 꼬리 분석에 쓰는 것만. 근거는 `note_stats`.
    stats: Option<DrawStats>,
}

/// `RendererStats` 에서 뽑아 둔 값. 전부 그 구조체가 이미 채워 돌려주는 것이다.
#[derive(Clone, Copy)]
struct DrawStats {
    draw_calls: usize,
    color_targets: usize,
    alpha_targets: usize,
    upload_mb: f64,
    resource_upload_ms: f64,
    cache_upload_ms: f64,
    frame_build_ms: f64,
    full_paint: bool,
}

impl RenderSplit {
    pub(crate) fn start() -> Self {
        Self {
            at: is_enabled().then(Instant::now),
            lock_us: 0,
            update_us: 0,
            clear_us: 0,
            draw_us: 0,
            post_us: 0,
            pump_us: 0,
            stats: None,
        }
    }

    /// 앞 경계로부터의 경과를 us 로 돌려주고 경계를 지금으로 옮긴다.
    fn lap(&mut self) -> u64 {
        let Some(at) = self.at else {
            return 0;
        };
        let now = Instant::now();
        let us = now.duration_since(at).as_micros() as u64;
        self.at = Some(now);
        us
    }

    pub(crate) fn mark_lock(&mut self) {
        self.lock_us = self.lap();
    }

    pub(crate) fn mark_update(&mut self) {
        self.update_us = self.lap();
    }

    pub(crate) fn mark_clear(&mut self) {
        self.clear_us = self.lap();
    }

    pub(crate) fn mark_draw(&mut self) {
        self.draw_us = self.lap();
    }

    /// `Renderer::render` 의 통계를 보관한다. 꼬리 프레임의 줄에 함께 찍는다.
    ///
    /// ## 왜 (2026-11-13)
    ///
    /// 61 번이 60ms 꼬리를 `draw` 로 좁혔고(꼬리 257 건에서 total 의 95.7%, 최악 프레임은
    /// 60.8ms 중 60785us), `AcquireSync` 는 아니라고 가렸다(LOCKTAIL 10 건, max 12.8ms).
    /// 그러면 `Renderer::render` 안이고, 그 안을 가르는 값이 ***이미 반환되고 있었다*** --
    /// `RenderResults::stats` 를 앞 판이 `let _ =` 로 버렸다.
    ///
    /// `draw_calls` 와 `upload_mb` 가 크면 작업량이 늘어난 것이고, 둘이 평시와 같은데 시간만
    /// 60ms 면 ★GPU 가 밀린 것★ 이다 -- 그때는 WebGL 스레드가 같은 GPU 에 넣은 작업과의
    /// 경합이고 GPU 타이머 쿼리로 넘어간다.
    pub(crate) fn note_stats(&mut self, stats: &webrender::RendererStats) {
        if self.at.is_none() {
            return;
        }
        self.stats = Some(DrawStats {
            draw_calls: stats.total_draw_calls,
            color_targets: stats.color_target_count,
            alpha_targets: stats.alpha_target_count,
            upload_mb: stats.texture_upload_mb,
            resource_upload_ms: stats.resource_upload_time,
            cache_upload_ms: stats.gpu_cache_upload_time,
            frame_build_ms: stats.frame_build_time,
            full_paint: stats.full_paint,
        });
    }

    pub(crate) fn mark_post(&mut self) {
        self.post_us = self.lap();
    }

    pub(crate) fn mark_pump(&mut self) {
        self.pump_us = self.lap();
    }

    /// 마지막 구간(`present`)을 닫고 쌓는다.
    pub(crate) fn finish(&mut self) {
        if self.at.is_none() {
            return;
        }
        let present_us = self.lap();
        // WR 안의 네 구간을 가져온다. 근거는 `webrender::wall_render_split` 의 모듈 주석 --
        // 62 번에서 `draws=0 upload=0.00MB` 인 프레임이 54ms 걸렸으므로 그 기다림이
        // `render_impl` 의 어느 자리인지 가른다.
        let wr = webrender::wall_render_split::take();
        note_render_split(
            [
                self.lock_us,
                self.update_us,
                self.clear_us,
                self.draw_us,
                self.post_us,
                self.pump_us,
                present_us,
            ],
            self.stats,
            wr,
        );
    }
}

/// 구간 일곱의 합계·최댓값과, 주기를 넘긴 프레임의 내역. 근거는 `RenderSplit` 의 doc 주석.
fn note_render_split(
    parts: [u64; PARTS],
    stats: Option<DrawStats>,
    wr: webrender::wall_render_split::Split,
) {
    static SUM: [AtomicU64; PARTS] = [const { AtomicU64::new(0) }; PARTS];
    static MAX: [AtomicU64; PARTS] = [const { AtomicU64::new(0) }; PARTS];
    static CALLS: AtomicU64 = AtomicU64::new(0);
    static OVER: AtomicU64 = AtomicU64::new(0);

    let total: u64 = parts.iter().sum();
    let nth = CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    for (slot, value) in SUM.iter().zip(parts) {
        slot.fetch_add(value, Ordering::Relaxed);
    }
    for (slot, value) in MAX.iter().zip(parts) {
        slot.fetch_max(value, Ordering::Relaxed);
    }

    // 주기를 넘긴 프레임만 그때그때 찍는다 -- 실측 1.07% 이므로 초당 2.6 줄이다.
    const PERIOD_US: u64 = 16_667;
    if total > PERIOD_US {
        let over = OVER.fetch_add(1, Ordering::Relaxed) + 1;
        let [lock, update, clear, draw, post, pump, present] = parts;
        // ★통계를 함께 찍는다★ -- `draw` 가 60ms 일 때 작업량이 늘었는지 GPU 가 밀렸는지
        // 가르는 값이고, `Renderer::render` 가 이미 돌려주던 것이다.
        let detail = match stats {
            Some(st) => format!(
                " draws={} targets={}/{} upload={:.2}MB res_upload={:.2}ms \
                 cache_upload={:.2}ms build={:.2}ms full_paint={}",
                st.draw_calls,
                st.color_targets,
                st.alpha_targets,
                st.upload_mb,
                st.resource_upload_ms,
                st.cache_upload_ms,
                st.frame_build_ms,
                st.full_paint,
            ),
            None => String::from(" stats=n/a"),
        };
        // ★WR 안의 네 구간★ -- 작업량이 0 인데 시간이 간 자리를 이것이 가른다.
        let inside = format!(
            " wr(enter={}us gpu={}us draw={}us exit={}us)",
            wr.enter_us, wr.gpu_us, wr.draw_us, wr.exit_us
        );
        log::warn!(
            "RENDERSPLIT #{over} total={:.1}ms lock={lock}us update={update}us clear={clear}us \
             draw={draw}us post={post}us pump={pump}us present={present}us{inside}{detail}",
            total as f64 / 1000.0,
        );
    }

    if nth % 1200 != 0 {
        return;
    }
    const NAMES: [&str; PARTS] = ["lock", "update", "clear", "draw", "post", "pump", "present"];
    let mut line = String::with_capacity(260);
    line.push_str(&format!(
        "RENDERSPLITS n={nth} over={}",
        OVER.load(Ordering::Relaxed)
    ));
    for ((name, sum), max) in NAMES.iter().zip(&SUM).zip(&MAX) {
        line.push_str(&format!(
            " {name}(avg={}us max={}us)",
            sum.swap(0, Ordering::Relaxed) / 1200,
            max.swap(0, Ordering::Relaxed),
        ));
    }
    log::warn!("{line}");
}

pub(crate) fn note_render_duration(elapsed: Duration) {
    if !is_enabled() {
        return;
    }
    let nanos = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
    RENDER_HIST[update_bucket_of(nanos)].fetch_add(1, Ordering::Relaxed);
    RENDER_WORST_NANOS.fetch_max(nanos, Ordering::Relaxed);
    RENDER_WORST_INTERVAL_NANOS.fetch_max(nanos, Ordering::Relaxed);
    if nanos > FRAME_PERIOD_NANOS {
        RENDER_OVER_PERIOD.fetch_add(1, Ordering::Relaxed);
        RENDER_OVER_INTERVAL.fetch_add(1, Ordering::Relaxed);
    }
}

/// `produce_wall_frame` 한 번을 센다. `targets` 는 그 틱이 물은 타일 수이고,
/// `held` 는 스크린샷으로 생산을 쉬었는지다. ★원자 셋 이하다.★
pub(crate) fn note_produce_wall_frame(targets: usize, held: bool) {
    if !is_enabled() {
        return;
    }
    PRODUCE_CALLS.fetch_add(1, Ordering::Relaxed);
    if held {
        PRODUCE_HELD.fetch_add(1, Ordering::Relaxed);
    } else if targets == 0 {
        PRODUCE_NO_TARGETS.fetch_add(1, Ordering::Relaxed);
    } else {
        PRODUCE_TARGETS.fetch_add(targets as u64, Ordering::Relaxed);
    }
}

/// `outcome` 이 `None` 이었던 (페인터, 틱) 쌍을 센다. ★원자 하나다.★
pub(crate) fn note_outcome_none() {
    if !is_enabled() {
        return;
    }
    OUTCOME_NONE.fetch_add(1, Ordering::Relaxed);
}

/// 틱 생산이 거절된 것을 이유별로 센다. ★원자 하나다.★
pub(crate) fn note_keep_previous(renderer_behind: bool) {
    if !is_enabled() {
        return;
    }
    if renderer_behind {
        KEEP_BEHIND.fetch_add(1, Ordering::Relaxed);
    } else {
        KEEP_CANVAS.fetch_add(1, Ordering::Relaxed);
    }
}

/// `Renderer::update()` 한 번의 소요 시간을 쌓는다. ★원자 둘이 전부다.★
pub(crate) fn note_update_duration(elapsed: Duration) {
    let nanos = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
    UPDATE_HIST[update_bucket_of(nanos)].fetch_add(1, Ordering::Relaxed);
    UPDATE_WORST_NANOS.fetch_max(nanos, Ordering::Relaxed);
}

/// 소요 시간을 칸으로. 칸 0 은 [`UPDATE_FLOOR_NANOS`] 이하, 그 위는 2 배씩이다.
///
/// ★경계 관례★: 칸 `k`(k>=1)는 `[FLOOR*2^(k-1), FLOOR*2^k)` 를 담는다 -- ***정확한 2 의
/// 거듭제곱은 윗칸으로 간다.*** `paint_animation_mesh_probe::bucket_of` 와 같은 관례이고,
/// 보고의 라벨도 그래서 `<` 다(`<=` 가 아니다).
fn update_bucket_of(nanos: u64) -> usize {
    if nanos <= UPDATE_FLOOR_NANOS {
        return 0;
    }
    let steps = (nanos / UPDATE_FLOOR_NANOS).ilog2() as usize;
    (steps + 1).min(UPDATE_BUCKETS - 1)
}

/// 프레임 발행 하나를 센다. ★원자 둘이 전부다.★
pub(crate) fn note_frame(from_wall_tick: bool, reason_bits: u64) {
    if !is_enabled() {
        return;
    }
    if from_wall_tick {
        TICK_FRAMES.fetch_add(1, Ordering::Relaxed);
    } else {
        OTHER_FRAMES.fetch_add(1, Ordering::Relaxed);
        OTHER_REASON_BITS.fetch_or(reason_bits, Ordering::Relaxed);
    }
}

/// ★`warn!` 로 찍는 이유: 벽 런이 `RUST_LOG=warn` 이다.★ `info!` 로 두면 정작 볼 수 없다 --
/// `paint_animation_mesh_probe` 가 그 함정에 걸려 첫 실기 런에서 한 줄도 남지 않았다.
/// 박자별 애니메이션 틱을 ★구간 차분으로★ 찍는다.
///
/// ★누적이 아니라 구간이다.★ 이 프로젝트는 괄호 안의 `/s` 를 구간값으로 오독해 틀린
/// 결론을 세 번 냈다. 읽는 쪽이 나눌 일이 없게 여기서 직접 뺀다.
///
/// 읽는 법: `frames` 는 그 구간의 표출 틱 수(60Hz 면 ~600/10s)이고, 각 칸은 그 박자의
/// WebView 가 틱을 ***받은*** 프레임 수다. `paint_side` 가 `frames/15` 쯤이면
/// `gfx_paint_side_animation_tick_divisor=15` 가 그대로 걸린 것이고, 그 WebView 에 WebGL
/// 캔버스가 같이 있으면 ***캔버스의 rAF 도 그 박자로 깎인다*** -- 60Hz 에서 4/s 다.
/// 운영자는 1/4/15/30/60Hz 를 눈으로 구별한다.
fn report_tick_cadence(elapsed: Duration) {
    static LAST: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
    /// 직전 보고 시각(프로브 시작 이후 ms). ★구간 길이를 여기서 뺀다.★
    static LAST_AT_MS: AtomicU64 = AtomicU64::new(0);

    let frames = CADENCE_FRAMES.load(Ordering::Relaxed);
    let now = [
        frames,
        TICK_CADENCE[CADENCE_FULL].load(Ordering::Relaxed),
        TICK_CADENCE[CADENCE_PAINT_SIDE].load(Ordering::Relaxed),
        TICK_CADENCE[CADENCE_LIVE_CANVAS].load(Ordering::Relaxed),
        TICK_CADENCE[CADENCE_NONE].load(Ordering::Relaxed),
    ];
    let mut d = [0u64; 5];
    for i in 0..5 {
        d[i] = now[i].saturating_sub(LAST[i].swap(now[i], Ordering::Relaxed));
    }
    // 애니메이션이 하나도 없는 런에서는 조용히 넘어간다.
    if d[0] == 0 && d[1] == 0 && d[2] == 0 && d[3] == 0 && d[4] == 0 {
        return;
    }
    // ★2026-11-11 수정: 분모가 틀려 있었다.★ 칸은 ***구간 차분***인데 `elapsed` 는
    // ***프로브 시작 이후 총 경과***라, `frames=311 (3.1/s)` 이 실제로는 31.1/s 였다.
    // 그 10 배 오차로 "`tick` 이 초당 3 번만 돈다" 는 가설을 세웠고, 조사에서 실제
    // `pump_paint_animation` 은 120/s 로 돈다는 것이 확인되어 가설이 죽었다. 구간 길이를
    // 여기서 직접 뺀다 -- 누적으로 나누지 않는다.
    let now_ms = elapsed.as_millis() as u64;
    let prev_at_ms = LAST_AT_MS.swap(now_ms, Ordering::Relaxed);
    let secs = ((now_ms.saturating_sub(prev_at_ms)) as f64 / 1000.0).max(1e-9);
    let rate = |n: u64| n as f64 / secs;
    // 실효 약수 -- 기대값과 맞는지 바로 보려는 것이다.
    let div = |n: u64| {
        if n == 0 {
            f64::INFINITY
        } else {
            d[0] as f64 / n as f64
        }
    };
    log::warn!(
        "TICKCADENCE {secs:.0}s frames={} ({:.1}/s) full={} ({:.1}/s) \
         paint_side={} ({:.1}/s, div={:.1}) live_canvas={} ({:.1}/s, div={:.1}) none={}",
        d[0],
        rate(d[0]),
        d[1],
        rate(d[1]),
        d[2],
        rate(d[2]),
        div(d[2]),
        d[3],
        rate(d[3]),
        div(d[3]),
        d[4],
    );
}

fn report(elapsed: Duration) {
    report_tick_cadence(elapsed);
    let tick = TICK_FRAMES.load(Ordering::Relaxed);
    let other = OTHER_FRAMES.load(Ordering::Relaxed);
    let bits = OTHER_REASON_BITS.load(Ordering::Relaxed);
    let secs = elapsed.as_secs_f64().max(1e-9);
    log::warn!(
        "FRAMESRC t={secs:.0}s tick={tick} ({:.1}/s) other={other} ({:.1}/s) \
         other_reasons=0x{bits:x}",
        tick as f64 / secs,
        other as f64 / secs,
    );

    // ★`update()` 시간★ -- 흘림이 패스를 얼마나 늘리는지. 칸 경계를 값으로 같이 찍는다.
    let counts: Vec<u64> = UPDATE_HIST
        .iter()
        .map(|b| b.load(Ordering::Relaxed))
        .collect();
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return;
    }
    let worst_us = UPDATE_WORST_NANOS.load(Ordering::Relaxed) as f64 / 1000.0;
    let free_pct = counts[0] as f64 * 100.0 / total as f64;
    let mut spread = String::new();
    for (index, count) in counts.iter().enumerate().skip(1) {
        if *count == 0 {
            continue;
        }
        let edge_us =
            UPDATE_FLOOR_NANOS as f64 * (1u64 << (index - 1).min(31)) as f64 * 2.0 / 1000.0;
        spread.push_str(&format!(
            " <{edge_us:.0}us:{:.1}%",
            *count as f64 * 100.0 / total as f64
        ));
    }
    log::warn!(
        "UPDATECOST t={secs:.0}s calls={total} ({:.1}/s) free={free_pct:.1}% worst={worst_us:.0}us{spread}",
        total as f64 / secs,
    );

    // ★거절 이유★ -- 17% 의 정체. `behind` 가 먼저 검사되므로 `canvas` 는 하한이다.
    let behind = KEEP_BEHIND.load(Ordering::Relaxed);
    let canvas = KEEP_CANVAS.load(Ordering::Relaxed);
    let declined = behind + canvas;
    let attempts = tick + declined;
    let share = |value: u64| {
        if attempts == 0 {
            0.0
        } else {
            value as f64 * 100.0 / attempts as f64
        }
    };
    log::warn!(
        "KEEPPREV t={secs:.0}s attempts={attempts} generated={tick} ({:.1}%) behind={behind} ({:.1}%) canvas={canvas} ({:.1}%)",
        share(tick),
        share(behind),
        share(canvas),
    );

    // ★paint 측 판정★ -- 깎임 후보와, 캔버스 신호가 그것을 되돌린 비율.
    let judged = PAINT_SIDE_JUDGED.load(Ordering::Relaxed);
    if judged > 0 {
        let defeated = PAINT_SIDE_DEFEATED.load(Ordering::Relaxed);
        let stale_worst = CANVAS_STALE_WORST_MS.load(Ordering::Relaxed);
        log::warn!(
            "PAINTSIDE t={secs:.0}s judged={judged} defeated={defeated} ({:.1}%) stale_worst={stale_worst}ms",
            defeated as f64 * 100.0 / judged as f64,
        );
    }
    // ★렌더 길이★ -- 이것이 길면 그 프레임에 실린 ***모든 것***이 늦는다. 구간값을 같이
    // 찍는다: 누적 최대값만으로는 "전반이 심하고 후반에 가라앉는다" 가 보이지 않는다.
    let render_counts: Vec<u64> = RENDER_HIST
        .iter()
        .map(|b| b.load(Ordering::Relaxed))
        .collect();
    let render_total: u64 = render_counts.iter().sum();
    if render_total > 0 {
        let worst_ms = RENDER_WORST_NANOS.load(Ordering::Relaxed) as f64 / 1e6;
        let interval_ms = RENDER_WORST_INTERVAL_NANOS.swap(0, Ordering::Relaxed) as f64 / 1e6;
        let over = RENDER_OVER_PERIOD.load(Ordering::Relaxed);
        let over_interval = RENDER_OVER_INTERVAL.swap(0, Ordering::Relaxed);
        let mut spread = String::new();
        for (index, count) in render_counts.iter().enumerate() {
            if *count == 0 {
                continue;
            }
            let edge_us = if index == 0 {
                UPDATE_FLOOR_NANOS as f64 / 1000.0
            } else {
                UPDATE_FLOOR_NANOS as f64 * (1u64 << (index - 1).min(31)) as f64 * 2.0 / 1000.0
            };
            spread.push_str(&format!(
                " <{edge_us:.0}us:{:.1}%",
                *count as f64 * 100.0 / render_total as f64
            ));
        }
        log::warn!(
            "RENDERCOST t={secs:.0}s renders={render_total} ({:.1}/s) over_period={over} ({:.2}%) worst={worst_ms:.1}ms last10s_worst={interval_ms:.1}ms last10s_over={over_interval}{spread}",
            render_total as f64 / secs,
            over as f64 * 100.0 / render_total as f64,
        );
    }
    // 애니메이션 기준 시각의 흔들림. 근거는 `web_content_animation::note_animation_zero`.
    // ★`PRODUCE` 와 독립이다★ -- 생산이 없는 구간에도 DL 은 올 수 있다.
    if let Some(line) = crate::web_content_animation::animation_zero_report(secs) {
        log::warn!("{line}");
    }
    // ★호출 수를 직접 센다★ -- KEEPPREV 의 attempts 는 유도값이라 199 대 240 을 설명하지
    // 못했다. 틱당 호출 1, 호출당 타일 4 가 기대값이다.
    let calls = PRODUCE_CALLS.load(Ordering::Relaxed);
    if calls > 0 {
        let targets = PRODUCE_TARGETS.load(Ordering::Relaxed);
        let no_targets = PRODUCE_NO_TARGETS.load(Ordering::Relaxed);
        let held = PRODUCE_HELD.load(Ordering::Relaxed);
        let none = OUTCOME_NONE.load(Ordering::Relaxed);
        // ★틱 에지 수를 함께 찍는다.★ 근거는 `wall_tick::tick_edge_count` 의 doc 주석 --
        // 요점은 `pacing 발화`(`tick_phase_probe`)와 이 줄은 기준점이 달라 직접 뺄 수 없고,
        // 같은 리포트 안에서만 손실 구간을 가를 수 있다는 것이다.
        //
        // 구간 차분도 함께 낸다 -- 시작 과도구간이 누적 평균을 가리지 않게 한다.
        let edges = crate::wall_tick::tick_edge_count();
        let prev_edges = PREV_EDGES.swap(edges, Ordering::Relaxed);
        let prev_calls = PREV_CALLS.swap(calls, Ordering::Relaxed);
        let d_edges = edges.saturating_sub(prev_edges);
        let d_calls = calls.saturating_sub(prev_calls);
        let window = REPORT_EVERY.as_secs_f64().max(1e-9);
        log::warn!(
            "PRODUCE t={secs:.0}s calls={calls} ({:.1}/s) targets={targets} ({:.1}/s) \
             per_call={:.2} no_targets={no_targets} held={held} outcome_none={none} \
             edges={edges} ({:.1}/s) window(edges={:.1}/s calls={:.1}/s miss={})",
            calls as f64 / secs,
            targets as f64 / secs,
            targets as f64 / calls as f64,
            edges as f64 / secs,
            d_edges as f64 / window,
            d_calls as f64 / window,
            d_edges.saturating_sub(d_calls),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★두 시험이 같은 전역 `ENABLED` 를 건드린다★ -- cargo test 는 병렬이므로 직렬화한다.
    static SERIALIZE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// ★쌓기 비용을 수치로 고정한다★ -- 이 프로젝트의 계측 규칙이 요구하는 것이다.
    /// 원자 둘이므로 한 자리 ns 여야 한다. 상한은 느린 CI 를 감안해 200ns 로 둔다.
    #[test]
    fn noting_a_frame_is_cheap_enough() {
        let _guard = SERIALIZE.lock().unwrap();
        ENABLED.store(true, Ordering::Relaxed);
        const CALLS: u32 = 200_000;
        let started = std::time::Instant::now();
        for index in 0..CALLS {
            note_frame(index % 2 == 0, u64::from(index));
        }
        let per_call = started.elapsed().as_secs_f64() / f64::from(CALLS) * 1e9;
        ENABLED.store(false, Ordering::Relaxed);
        assert!(
            per_call < 200.0,
            "쌓기 비용 {per_call:.1}ns/call -- 상한 200ns 를 넘었다"
        );
        println!("note_frame: {per_call:.1} ns/call");
    }

    /// `update()` 칸 경계가 의도대로다. ★보고를 읽는 전제이므로 고정한다.★
    #[test]
    fn the_update_buckets_start_at_the_floor_and_double() {
        assert_eq!(update_bucket_of(0), 0);
        assert_eq!(update_bucket_of(UPDATE_FLOOR_NANOS), 0);
        // 칸 1 = [FLOOR, FLOOR*2) -- 정확히 두 배인 값은 윗칸이다.
        assert_eq!(update_bucket_of(UPDATE_FLOOR_NANOS + 1), 1);
        assert_eq!(update_bucket_of(UPDATE_FLOOR_NANOS * 2 - 1), 1);
        assert_eq!(update_bucket_of(UPDATE_FLOOR_NANOS * 2), 2);
        assert_eq!(update_bucket_of(UPDATE_FLOOR_NANOS * 3), 2);
        assert_eq!(update_bucket_of(UPDATE_FLOOR_NANOS * 4), 3);
        // 한 프레임 예산(16.7ms)은 끝쪽 칸이어야 한다 -- 그 근처가 저더의 경계다.
        assert!(
            update_bucket_of(16_700_000) > 8,
            "{}",
            update_bucket_of(16_700_000)
        );
    }

    /// 어떤 입력도 칸 밖으로 떨어지지 않는다(색인 패닉 금지).
    #[test]
    fn no_duration_can_fall_outside_the_update_histogram() {
        for nanos in [0, 1, UPDATE_FLOOR_NANOS, u64::MAX, u64::MAX / 2] {
            let bucket = update_bucket_of(nanos);
            assert!(bucket < UPDATE_BUCKETS, "{nanos} -> {bucket}");
        }
    }

    /// 꺼져 있으면 아무것도 쌓이지 않는다. ★기본 런이 전과 같다는 것이 이 시험이다.★
    #[test]
    fn a_disabled_probe_counts_nothing() {
        let _guard = SERIALIZE.lock().unwrap();
        ENABLED.store(false, Ordering::Relaxed);
        let before = TICK_FRAMES.load(Ordering::Relaxed);
        note_frame(true, 1);
        assert_eq!(TICK_FRAMES.load(Ordering::Relaxed), before);
    }
}

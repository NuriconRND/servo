//! `render_impl` 의 네 구간을 재서 `paint` 쪽에 넘긴다.
//!
//! ## 왜 (2026-11-13)
//!
//! 62 번이 결정적인 것을 보여줬다.
//!
//! ```text
//! total=54.6ms  draw=54539us
//!   draws=0  targets=1/0  upload=0.00MB
//!   res_upload=0.00ms  cache_upload=0.00ms  build=0.00ms
//! ```
//!
//! 꼬리 231 건 전체에서 `draws` 가 p50=3, max=3 이고 `upload` 는 p50=0.00MB 다. ***작업량이
//! 전혀 없는데 54ms 가 걸린다.*** `clear_background` 는 avg 10us, max 40us 로 무죄이고,
//! `AcquireSync`(`LOCKTAIL`)는 10 건 max 12.8ms 로 그 꼬리의 4% 뿐이다.
//!
//! 작업량이 평시와 같은데 시간만 길면 ★GPU 또는 드라이버에서 기다린 것★ 이고, 그 기다림이
//! `render_impl` 의 어느 자리인지를 이 모듈이 가른다.
//!
//! ## 구간
//!
//! ```text
//! enter  staging_texture_pool.begin_frame + texture_resolver.begin_frame + begin_frame
//!        -- 드라이버가 앞 프레임을 기다릴 수 있는 자리
//! gpu    prepare_gpu_cache  -- external image lock(= AcquireSync)이 이 안이다
//! draw   draw_frame + draw_frame_debug_items
//! exit   unlock_external_images + gpu_profiler.end_frame + 프로파일 집계
//! ```
//!
//! ## 왜 전역 원자인가
//!
//! WR 은 `paint` 를 참조하지 않으므로(의존 방향이 반대다) 값을 돌려줄 통로가 없다. 그래서
//! 전역 원자에 쌓고 `painter` 가 `RenderSplit::finish` 에서 읽어 간다. 한 프로세스에 painter
//! 가 넷이지만 ★각자 자기 스레드에서 자기 Renderer 를 돌리므로★ 스레드 로컬이면 섞이지 않는다.
//!
//! ## 비용
//!
//! 경계 넷에 `Instant::now()` 이고 240/s 이면 약 24us/s 다. 값은 스레드 로컬에 쌓으므로 원자
//! 경합이 없고, 꼬리 줄은 `RENDERSPLIT` 이 이미 내는 것에 묻어 가므로 I/O 가 늘지 않는다.

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

/// 이 스레드의 마지막 `render_impl` 구간들. 순서는 enter/gpu/draw/exit 다.
#[derive(Clone, Copy, Default)]
pub struct Split {
    pub enter_us: u64,
    pub gpu_us: u64,
    pub draw_us: u64,
    pub unlock_us: u64,
    pub endf_us: u64,
    /// `mark_unlock` 뒤부터 `render_impl` 끝까지. ★`update_gpu_profile` 의 블로킹 GPU
    /// 쿼리(`GL_QUERY_RESULT`)가 이 안이다★ -- 63 번에서 wr 합이 `draw` 의 20.1% 밖에
    /// 설명하지 못한 것이 이 구간이었다.
    /// `bind_debug_overlay` + `draw_*_debug`. ★DComp `BeginDraw` 가 여기★ -- HUD 가 꺼지면
    /// `bind_debug_overlay` 가 `None` 을 주므로 0 에 가깝다.
    pub overlay_us: u64,
    /// Telemetry/메모리/프로파일 집계 (CPU).
    pub prof_us: u64,
    /// `profiler.set_counters`/`update` + `PROFILER_DBG` 블록 + `debug_renderer` 렌더.
    /// ★HUD 를 실제로 그리는 구간★.
    pub dbg_us: u64,
    /// `staging/pbo/device end_frame` 셋.
    pub endf3_us: u64,
    /// ★`check_gl_errors` = `glGetError`★. 드라이버의 미처리 명령을 flush 하고 동기화하므로,
    /// HUD 가 추가한 GL 작업을 여기서 전부 기다린다(65 번: HUD 켜짐 p50 17500us 대 꺼짐
    /// 245us). 주기 게이트가 들어간 뒤로는 검사하는 프레임에만 값이 찍힌다.
    pub tail_us: u64,
    /// GPU 타이머 쿼리가 켜져 있나. `DebugFlags::GPU_TIME_QUERIES` 의 현재 값이다.
    pub gpu_timers: bool,
}

thread_local! {
    /// 구간 경계의 시각. `start` 가 세우고 각 `mark_*` 가 옮긴다.
    static AT: Cell<Option<Instant>> = const { Cell::new(None) };
    /// 쌓인 구간. `paint` 가 `take` 로 가져간다.
    static SPLIT: Cell<Split> = const {
        Cell::new(Split {
            enter_us: 0,
            gpu_us: 0,
            draw_us: 0,
            unlock_us: 0,
            endf_us: 0,
            overlay_us: 0,
            prof_us: 0,
            dbg_us: 0,
            endf3_us: 0,
            tail_us: 0,
            gpu_timers: false,
        })
    };
}

/// 재기 시작한다. 반환값은 `mark_*` 에 그대로 넘긴다(쓰이지는 않고, 호출 순서를 코드에 박기
/// 위한 표시다).
pub fn start() -> Instant {
    let now = Instant::now();
    AT.set(Some(now));
    SPLIT.set(Split::default());
    now
}

/// 앞 경계로부터의 경과를 us 로 돌려주고 경계를 지금으로 옮긴다.
fn lap() -> u64 {
    let Some(at) = AT.get() else {
        return 0;
    };
    let now = Instant::now();
    let us = now.duration_since(at).as_micros() as u64;
    AT.set(Some(now));
    us
}

pub fn mark_enter(_started: Instant) {
    let us = lap();
    SPLIT.set(Split {
        enter_us: us,
        ..SPLIT.get()
    });
}

pub fn mark_gpu(_started: Instant) {
    let us = lap();
    SPLIT.set(Split {
        gpu_us: us,
        ..SPLIT.get()
    });
}

pub fn mark_draw(_started: Instant) {
    let us = lap();
    SPLIT.set(Split {
        draw_us: us,
        ..SPLIT.get()
    });
}

pub fn mark_unlock(_started: Instant) {
    let us = lap();
    SPLIT.set(Split {
        unlock_us: us,
        ..SPLIT.get()
    });
}

pub fn mark_endf(_started: Instant) {
    let us = lap();
    SPLIT.set(Split {
        endf_us: us,
        ..SPLIT.get()
    });
}

/// `debug_overlay` 블록이 끝났다. 그 안에 DComp `BeginDraw`(`compositor.bind`)가 있다.
pub fn mark_overlay(_started: Instant) {
    let us = lap();
    SPLIT.set(Split {
        overlay_us: us,
        ..SPLIT.get()
    });
}

/// CPU 집계가 끝났다. 아래부터 HUD 를 실제로 그린다.
pub fn mark_prof(_started: Instant) {
    let us = lap();
    SPLIT.set(Split {
        prof_us: us,
        ..SPLIT.get()
    });
}

/// HUD 그리기가 끝났다.
pub fn mark_dbg(_started: Instant) {
    let us = lap();
    SPLIT.set(Split {
        dbg_us: us,
        ..SPLIT.get()
    });
}

/// `end_frame` 셋이 끝났다. 다음이 `check_gl_errors` 다.
pub fn mark_endf3(_started: Instant) {
    let us = lap();
    SPLIT.set(Split {
        endf3_us: us,
        ..SPLIT.get()
    });
}

/// `check_gl_errors` 를 이 프레임에 할지. 주기는 `set_gl_error_check_every` 가 싣는다.
///
/// ## 왜 주기인가 (2026-11-13)
///
/// `glGetError` 는 드라이버의 미처리 명령을 flush 하고 ***동기화***한다. 65 번에서 그 자리의
/// p50 이 HUD 켜짐 17500us, 꺼짐 245us 였다 -- HUD 가 추가한 GL 명령을 이 호출이 전부
/// 기다렸고, 작업량이 같은 프레임(`draws=3`)이 41ms 가 됐다.
///
/// 매 프레임 전체 파이프라인을 동기화하면서 `OUT_OF_MEMORY` 하나를 보는 것은 대가가 너무
/// 크다. 기본 60(1 초마다)이면 감지는 유지되고 비용은 1/60 이다. 1 은 앞 판 동작(비교용),
/// 0 은 완전히 끈다.
pub fn should_check_gl_errors() -> bool {
    let every = GL_ERROR_EVERY.load(Ordering::Relaxed);
    if every == 0 {
        return false;
    }
    let nth = GL_ERROR_SEQ.fetch_add(1, Ordering::Relaxed);
    nth % u64::from(every) == 0
}

/// `gfx_wall_gl_error_check_every` 를 싣는다. `paint` 가 틱마다 부른다.
pub fn set_gl_error_check_every(every: u32) {
    GL_ERROR_EVERY.store(every, Ordering::Relaxed);
}

/// 지금 주기. 로그에 함께 찍어 어느 설정으로 측정했는지 남긴다.
pub fn gl_error_check_every() -> u32 {
    GL_ERROR_EVERY.load(Ordering::Relaxed)
}

static GL_ERROR_EVERY: AtomicU32 = AtomicU32::new(60);
static GL_ERROR_SEQ: AtomicU64 = AtomicU64::new(0);

/// `render_impl` 의 끝.
pub fn mark_tail(_started: Instant) {
    let us = lap();
    SPLIT.set(Split {
        tail_us: us,
        ..SPLIT.get()
    });
}

/// GPU 타이머 쿼리가 켜져 있는지 남긴다. `Renderer::set_debug_flags` 가 부른다.
///
/// `tail` 이 범인이면 `DebugFlags::GPU_TIME_QUERIES` 를 끄는 것만으로 해결되므로, 그 값이
/// 로그에 함께 있어야 판정이 된다.
pub fn note_gpu_timers(enabled: bool) {
    GPU_TIMERS.set(enabled);
}

thread_local! {
    static GPU_TIMERS: Cell<bool> = const { Cell::new(false) };
}

/// 쌓인 구간을 가져가고 비운다. `paint` 의 `RenderSplit::finish` 가 부른다.
pub fn take() -> Split {
    let mut split = SPLIT.get();
    split.gpu_timers = GPU_TIMERS.get();
    SPLIT.set(Split::default());
    AT.set(None);
    split
}

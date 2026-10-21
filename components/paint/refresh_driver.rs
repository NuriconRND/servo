/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, LazyCell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{RecvTimeoutError, Sender};
use embedder_traits::{EventLoopWaker, RefreshDriver};
use log::warn;
use servo_base::cross_process_instant::CrossProcessInstant;
use servo_base::id::{PainterId, WebViewId};
use servo_constellation_traits::EmbedderToConstellationMessage;
use timers::{BoxedTimerCallback, TimerEventRequest, TimerScheduler};

use crate::painter::Painter;
use crate::webview_renderer::WebViewRenderer;

/// The [`BaseRefreshDriver`] is a "base class" for [`RefreshDriver`] trait
/// implementations. It encapsulates shared behavior so that it does not have to be
/// implemented by all trait implementations. It is responsible for providing
/// [`RefreshDriver`] implementations with a callback that is used to wake up the event
/// loop and trigger frame readiness.
pub(crate) struct BaseRefreshDriver {
    /// Whether or not the [`BaseRefreshDriver`] is waiting for a frame. Once the [`RefreshDriver`]
    /// informs the base that a frame start happened, this becomes false.
    waiting_for_frame: Arc<AtomicBool>,
    /// An [`EventLooperWaker`] which alerts the main UI event loop when a frame start occurs.
    event_loop_waker: Box<dyn EventLoopWaker>,
    /// A list of internal observers that watch for frame starts.
    observers: RefCell<Vec<Rc<dyn RefreshDriverObserver>>>,
    /// The implementation of the [`RefreshDriver`]. By default this is a simple timer, but the
    /// embedder can install a custom driver, such as one that is run via the hardware vsync signal.
    refresh_driver: Rc<dyn RefreshDriver>,
    /// 설치된 드라이버가 자기 클럭을 갖고 있지 않을 때, 타이머 스레드가 대신 내 주는
    /// ★깨우기★. [`Heartbeat`] 와 [`RefreshDriver::needs_paint_timer_heartbeat`] 참고.
    heartbeat: Option<Arc<Heartbeat>>,
}

impl BaseRefreshDriver {
    pub(crate) fn new(
        event_loop_waker: Box<dyn EventLoopWaker>,
        refresh_driver: Option<Rc<dyn RefreshDriver>>,
        timer_refresh_driver: &LazyCell<Rc<TimerRefreshDriver>>,
    ) -> Self {
        let refresh_driver = refresh_driver.unwrap_or_else(|| (**timer_refresh_driver).clone());
        let waiting_for_frame = Arc::new(AtomicBool::new(false));
        // ★콜백을 모아 두기만 하는 드라이버는 이벤트 루프를 깨우지 않는다.★ 그러면 임베더의
        // 틱이 올 일이 없고, 틱이 콜백을 쏘는 구조에서 그것은 화면이 영구히 멈춘다는 뜻이다.
        // 그 구성에서만 타이머 스레드가 ***깨우기만*** 계속 내 준다 -- 발사는 여전히 틱이다.
        let heartbeat = refresh_driver.needs_paint_timer_heartbeat().then(|| {
            Heartbeat::new(
                (**timer_refresh_driver).clone(),
                event_loop_waker.clone_box(),
                waiting_for_frame.clone(),
            )
        });
        Self {
            waiting_for_frame,
            event_loop_waker,
            observers: Default::default(),
            refresh_driver,
            heartbeat,
        }
    }

    pub(crate) fn add_observer(&self, observer: Rc<dyn RefreshDriverObserver>) {
        let mut observers = self.observers.borrow_mut();
        observers.push(observer);

        // If this is the first observer, make sure to observe the next frame.
        if observers.len() == 1 {
            self.observe_next_frame();
        }
    }

    pub(crate) fn notify_will_paint(&self, painter: &mut Painter) {
        // Limit the borrow of `self.observers` to the minimum here.
        let still_has_observers = {
            let mut observers = self.observers.borrow_mut();
            observers.retain(|observer| observer.frame_started(painter));
            !observers.is_empty()
        };

        if still_has_observers {
            self.observe_next_frame();
        }
    }

    fn observe_next_frame(&self) {
        // ★`Release` 다 -- [`Heartbeat::beat`] 의 `Acquire` 적재와 짝이다.★ 그쪽이 이
        // 저장을 놓치면 아무도 박동을 다시 걸지 않아 `FrameClock::Timer` 가 얼어붙는다.
        self.waiting_for_frame.store(true, Ordering::Release);

        let waiting_for_frame = self.waiting_for_frame.clone();
        let event_loop_waker = self.event_loop_waker.clone_box();
        self.refresh_driver.observe_next_frame(Box::new(move || {
            waiting_for_frame.store(false, Ordering::Relaxed);
            event_loop_waker.wake();
        }));

        // ★자기 클럭이 없는 드라이버면 타이머 스레드가 틱 에지를 내 준다.★ 바로 위에서
        // `waiting_for_frame` 을 세웠으므로, 박동은 틱이 그 콜백을 쏠 때까지 이어진다.
        self.arm_paint_timer_heartbeat();
    }

    /// ★프레임 관찰자 없이도 틱이 필요할 때 박동을 건다.★
    ///
    /// 페인트 쪽 애니메이션과 비디오 도착은 `RefreshDriverObserver` 를 달지 않고도 다음
    /// 프레임에 깨워 달라고 한다(`WebContentAnimator::wake_for_paint_animation`). 그 요청은
    /// 설치된 드라이버에 쌓이고 임베더의 틱이 쏘는데, 자기 클럭이 없는 드라이버에서는 그
    /// 틱을 낼 깨우기도 여기서 나와야 한다 -- 걸지 않으면 그 애니메이션이 선다.
    ///
    /// 한 번만 건다. 박동이 틱을 내고, 틱이 그 콜백을 쏘고, 쏘인 쪽이 계속 돌 생각이면
    /// 다음 바퀴에서 다시 여기를 부른다.
    pub(crate) fn arm_paint_timer_heartbeat(&self) {
        if let Some(heartbeat) = self.heartbeat.as_ref() {
            heartbeat.arm();
        }
    }

    /// Whether or not the renderer should trigger a message to the embedder to request a
    /// repaint. This might be true if we are animating and we are still waiting for a new
    /// frame from the `RefreshDriver`.
    pub(crate) fn wait_to_paint(&self) -> bool {
        !self.observers.borrow().is_empty() && self.waiting_for_frame.load(Ordering::Relaxed)
    }
}

/// A [`RefreshDriverObserver`] is an internal subscriber to frame start signals from the
/// [`RefreshDriver`]. Examples of these kind of observers would be one that triggers new
/// animation frames right after vsync signals or one that handles touch interactions once
/// per frame.
pub(crate) trait RefreshDriverObserver {
    /// Informs the observer that a new frame has started. The observer should return
    /// `true` to keep observing or `false` if wants to stop observing and should be
    /// removed by the [`BaseRefreshDriver`].
    fn frame_started(&self, painter: &mut Painter) -> bool;
}

/// The [`AnimationRefreshDriverObserver`] is the default implementation of a
/// [`RefreshDriver`] on systems without vsync hardware integration. It has a very basic
/// way of triggering frames using a timer. It prevents new animation frames until the
/// timer has fired.
pub(crate) struct AnimationRefreshDriverObserver {
    /// The channel on which messages can be sent to the Constellation.
    pub(crate) constellation_sender: Sender<EmbedderToConstellationMessage>,

    /// Whether or not we are currently animating via a timer.
    pub(crate) animating: Cell<bool>,

    /// Which painter this observer belongs to, so only the one that owns a `WebView`
    /// speaks for it. See [`Painter::animating_webviews`].
    painter_id: PainterId,

    /// Frames this observer has seen, used to thin out ticks for animations the paint
    /// thread is playing itself.
    frames_observed: Cell<u64>,
}

impl AnimationRefreshDriverObserver {
    pub(crate) fn new(
        constellation_sender: Sender<EmbedderToConstellationMessage>,
        painter_id: PainterId,
    ) -> Self {
        Self {
            constellation_sender,
            animating: Default::default(),
            painter_id,
            frames_observed: Default::default(),
        }
    }

    pub(crate) fn notify_animation_state_changed(
        &self,
        webview_renderer: &WebViewRenderer,
    ) -> bool {
        // A fan-out paints one `WebView` into several painters. Only the one that owns it
        // asks for its ticks; the rest would each add one more tick per frame for the same
        // animation. See [`Painter::animating_webviews`].
        if PainterId::from(webview_renderer.id) != self.painter_id {
            return false;
        }
        if !webview_renderer.animating() {
            // If no other WebView is animating we will officially stop animating once the
            // next frame has been painted.
            if self.animating.get() {
                // ★전이만 찍는다★ — 매 프레임이 아니다. 스톨 조사용(2026-09-02): `-DComp on`
                // 에서 월이 얼어붙는데 컴포지터와 painter 는 멀쩡했고(48 passes/s, 렌더 12,114
                // 회, external-image lock 균형), 멈춘 것은 콘텐츠 생산 쪽이었다. 여기서 관찰을
                // 그만두면 rAF 가 안 돌고 캔버스가 영영 멈추므로, 그 순간이 로그에 있어야 한다.
                warn!(
                    "ANIMTICK stop-requested: {:?} reports not animating",
                    webview_renderer.id
                );
            }
            return false;
        }

        let frame_time = crate::frame_time::frame_time_for_tick(
            self.painter_id,
            CrossProcessInstant::now(),
            frame_time_max_age(),
        );
        if let Err(error) =
            self.constellation_sender
                .send(EmbedderToConstellationMessage::TickAnimation(
                    vec![webview_renderer.id],
                    frame_time,
                ))
        {
            warn!("Sending tick to constellation failed ({error:?}).");
        }

        if self.animating.get() {
            return false;
        }

        // ★양성 대조★: 실제 전이가 일어나는 자리는 여기다. 처음엔 `frame_started` 에 걸었는데,
        // 이 함수가 먼저 `animating` 을 세워서 그쪽 가드가 영영 참이 되지 않았다 — 그래서
        // 로그가 0 줄이었고, 그것이 "stop 이 안 났다" 인지 "로깅이 고장났다" 인지 구분되지
        // 않았다. 음성 결과를 증명하려면 양성 대조가 먼저 보여야 한다.
        warn!(
            "ANIMTICK start: {:?} began animating; refresh driver now observing",
            webview_renderer.id
        );
        self.animating.set(true);
        true
    }
}

impl RefreshDriverObserver for AnimationRefreshDriverObserver {
    fn frame_started(&self, painter: &mut Painter) -> bool {
        // If any WebViews are animating ask them to paint again for another animation tick.
        let animating_webviews = painter.animating_webviews();

        // If nothing is animating any longer, update our state and exit early without requesting
        // any new frames.
        if animating_webviews.is_empty() {
            if self.animating.get() {
                // ★여기가 틱이 끊기는 자리다.★ `false` 를 돌려주면 `BaseRefreshDriver` 가 이
                // 관찰자를 목록에서 빼고 다음 프레임을 관찰하지 않는다 — 누군가
                // `notify_animation_state_changed` 로 다시 깨우지 않는 한 rAF 는 영영 안 돈다.
                // 스톨 실행에서 이 줄이 찍혔는지가 "아무도 그리라고 시키지 않는 상태" 인지를
                // 가른다(2026-09-02 조사).
                warn!("ANIMTICK stop: no animating webviews; refresh driver stops observing");
            }
            self.animating.set(false);
            return false;
        }
        // ***A `WebView` whose animation the paint thread is already playing does not need
        // script to recompute the page every frame.*** The value on screen comes from the
        // paint-side prediction between ticks, so a tick is only needed often enough to
        // rebase that prediction and to fire the animation's own events. Every tick that
        // is not needed costs a rendering update, and a rendering update reflows the whole
        // document: measured on the 4-GPU wall, 2026-09-08 (log_ani_perf/13), 122 reflows
        // a second with an animation running against 54 without, and during a content
        // switch a single reflow there costs 14.5 ms.
        //
        // Ticks for anything the paint thread is *not* playing keep the frame rate, since
        // slowing those would genuinely slow the animation.
        //
        // ★이것이 나누는 것은 표출 틱이 아니라 `RefreshDriver` 틱이다.★ 전에는 그 둘이
        // 같은 주기인 구성이 `FrameClock::Pacing` 뿐이었다 -- `Timer` 는 `PaintTimerThread`
        // 가, `Vsync` 는 DwmFlush 스레드가 자기 클럭으로 콜백을 쐈기 때문이다. 2026-10-21
        // 부터 세 구성 모두 셸의 표출 틱이 콜백을 쏘므로 ★여기서 세는 프레임이 곧 표출
        // 틱★ 이 됐다. ***의도한 결과다***: `epoch/s` 의 기대값은 세 구성 모두에서
        // `gfx_refresh_hz`/divisor 다. 예전 측정값과 비교하는 사람이 회귀로 읽지 않도록
        // 여기 적어 둔다(`Timer`/`Vsync` 의 옛 기대값은 자유 구동 페인트 타이머 기준이었다).
        let divisor = servo_config::pref!(gfx_paint_side_animation_tick_divisor).max(1) as u64;
        let frame = self.frames_observed.get().wrapping_add(1);
        self.frames_observed.set(frame);
        let due_for_paint_driven = divisor == 1 || frame % divisor == 0;
        let webviews_to_tick: Vec<WebViewId> = animating_webviews
            .into_iter()
            .filter(|(_, paint_driven)| !*paint_driven || due_for_paint_driven)
            .map(|(webview_id, _)| webview_id)
            .collect();
        if webviews_to_tick.is_empty() {
            // Still observing: the next frame may be the one that ticks.
            self.animating.set(true);
            return true;
        }

        // Request new animation frames from all animating WebViews.
        let frame_time = crate::frame_time::frame_time_for_tick(
            self.painter_id,
            CrossProcessInstant::now(),
            frame_time_max_age(),
        );
        if let Err(error) =
            self.constellation_sender
                .send(EmbedderToConstellationMessage::TickAnimation(
                    webviews_to_tick,
                    frame_time,
                ))
        {
            warn!("Sending tick to constellation failed ({error:?}).");
            return false;
        }

        // 여기서는 전이 로그를 걸지 않는다 — `notify_animation_state_changed` 가 이미
        // `animating` 을 세운 뒤라 이 가드는 도달하지 않는다(그래서 처음에 0 줄이 나왔다).
        self.animating.set(true);
        true
    }
}

enum TimerThreadMessage {
    Request(TimerEventRequest),
    Quit,
}

/// A thread that manages a [`TimerScheduler`] running in the background of the
/// [`RefreshDriver`]. This is necessary because we need a reliable way of waking up the
/// embedder's main thread, which may just be sleeping until the `EventLoopWaker` asks it
/// to wake up.
///
/// It would be nice to integrate this somehow into the embedder thread, but it would
/// require both some communication with the embedder and for all embedders to be well
/// behave respecting wakeup timeouts -- a bit too much to ask at the moment.
pub(crate) struct TimerRefreshDriver {
    sender: Sender<TimerThreadMessage>,
    /// ★다음 틱의 **절대 시각**.★ 자세한 것은 `observe_next_frame`.
    next_frame_at: Cell<Option<Instant>>,
    join_handle: Option<JoinHandle<()>>,
}

impl Default for TimerRefreshDriver {
    fn default() -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded::<TimerThreadMessage>();
        let join_handle = thread::Builder::new()
            .name(String::from("PaintTimerThread"))
            .spawn(move || {
                let mut scheduler = TimerScheduler::default();

                loop {
                    let recv_result = match scheduler.next_deadline() {
                        Some(deadline) => receiver.recv_deadline(deadline),
                        None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    };
                    match recv_result {
                        Ok(TimerThreadMessage::Request(request)) => {
                            scheduler.schedule_timer(request);
                        },
                        Err(RecvTimeoutError::Timeout) => scheduler.dispatch_completed_timers(),
                        Ok(TimerThreadMessage::Quit) | Err(RecvTimeoutError::Disconnected) => {
                            return;
                        },
                    }
                }
            })
            .expect("Could not create RefreshDriver timer thread.");

        Self {
            sender,
            next_frame_at: Cell::new(None),
            join_handle: Some(join_handle),
        }
    }
}

impl TimerRefreshDriver {
    pub(crate) fn queue_timer(&self, duration: Duration, callback: BoxedTimerCallback) {
        let _ = self
            .sender
            .send(TimerThreadMessage::Request(TimerEventRequest {
                callback,
                duration,
            }));
    }
}

/// 임베더 드라이버가 자기 클럭을 갖고 있지 않을 때, 타이머 스레드가 대신 내 주는
/// ★틱 에지★(= 이벤트 루프 깨우기).
///
/// ★깨우기만 한다.★ 프레임 콜백은 임베더 드라이버에 쌓인 채로 임베더의 틱을 기다린다 --
/// 여기서 쏘면 발사 주체가 다시 둘이 되고, 그것이 없애려는 "두 시계 맥놀이" 다. 바뀌는
/// 것은 ***누가 콜백을 쏘나*** 뿐이고, 에지가 언제 생기나는 종전 그대로다(같은 스레드,
/// 같은 `paint_timer_period`, 같은 절대 격자).
///
/// ★스스로 다시 건다.★ `observe_next_frame` 에서 한 번만 걸면, 그 깨우기가 임베더의 틱
/// 게이트를 통과하지 못한 경우(아직 기한이 안 참) 다시 걸어 줄 사람이 없어 그대로 멎는다 --
/// 임베더의 재등록은 렌더를 거쳐야 하고, 렌더는 틱을 거쳐야 하기 때문이다. 그래서 프레임을
/// 기다리는 동안은(`waiting_for_frame`) 타이머 스레드에서 자기 자신을 다시 건다. 틱이
/// 콜백을 쏘면 그 플래그가 내려가고 다음 박동에서 저절로 멎는다 -- ★조용한 페이지가
/// 영원히 깨어 있지는 않는다.★
struct Heartbeat {
    sender: Sender<TimerThreadMessage>,
    period: Duration,
    /// 다음 에지의 **절대 시각**. `TimerRefreshDriver::observe_next_frame` 과 같은 격자
    /// 규칙이다 -- ★늦었으면 지나간 에지는 버린다★(그쪽 주석이 왜인지 적는다).
    next_at: Mutex<Instant>,
    event_loop_waker: Box<dyn EventLoopWaker>,
    /// [`BaseRefreshDriver::waiting_for_frame`] 과 같은 플래그. 참인 동안만 박동한다.
    waiting_for_frame: Arc<AtomicBool>,
    /// 타이머에 하나가 걸려 있는가. 같은 에지에 둘이 걸리면 깨우기가 두 배가 된다.
    armed: AtomicBool,
}

impl Heartbeat {
    fn new(
        timer_refresh_driver: Rc<TimerRefreshDriver>,
        event_loop_waker: Box<dyn EventLoopWaker>,
        waiting_for_frame: Arc<AtomicBool>,
    ) -> Arc<Self> {
        let period = paint_timer_period();
        Arc::new(Self {
            sender: timer_refresh_driver.sender.clone(),
            period,
            next_at: Mutex::new(Instant::now() + period),
            event_loop_waker,
            waiting_for_frame,
            armed: AtomicBool::new(false),
        })
    }

    /// 다음 격자점에 하나 건다. 이미 걸려 있으면 아무 일도 하지 않는다.
    fn arm(self: &Arc<Self>) {
        if self.armed.swap(true, Ordering::SeqCst) {
            return;
        }
        let now = Instant::now();
        let delay = {
            // 중독된 락에서도 격자를 들고 간다 -- 박동을 포기하면 화면이 멈춘다.
            let mut next = self
                .next_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while *next <= now {
                *next += self.period;
            }
            *next - now
        };
        let this = self.clone();
        let sent = self
            .sender
            .send(TimerThreadMessage::Request(TimerEventRequest {
                callback: Box::new(move || this.beat()),
                duration: delay,
            }));
        if sent.is_err() {
            // 타이머 스레드가 이미 내려갔다. 다음 `observe_next_frame` 이 다시 시도할 수
            // 있도록 표시만 되돌린다.
            self.armed.store(false, Ordering::SeqCst);
        }
    }

    /// 타이머 스레드에서 돈다.
    fn beat(self: &Arc<Self>) {
        // ★표시를 먼저 내린다.★ 깨어난 메인이 `observe_next_frame` 으로 다음 것을 걸려고
        // 할 때 "이미 걸려 있다" 로 보이면 그 에지가 통째로 사라진다. 먼저 내려 두면 최악이
        // 메인과 여기가 하나씩 걸려 하나가 `armed` 에 막히는 경우이고, 그때도 하나는 남는다.
        //
        // ★그 "최악" 이 성립하는 근거는 두 플래그의 순서다.★ 메인이 `armed` 에 막히는
        // 경우는 메인의 `swap` 이 이 `store` 보다 먼저인 경우뿐이고(둘 다 `SeqCst` 라 한
        // 전순서 위에 있다), 그러면 메인이 그보다 앞서 한 `waiting_for_frame` 저장
        // (`Release`)이 아래 적재(`Acquire`)에 보인다 -- 그래서 여기서 다시 건다.
        // ★`Relaxed` 로 두면 그 문장이 성립하지 않고, 둘 다 물러나 아무도 안 거는 끼어듦이
        // 남는다★(x86 에서는 나지 않지만, 주석이 갖지 않은 안전을 주장하게 된다).
        self.armed.store(false, Ordering::SeqCst);
        self.event_loop_waker.wake();
        if self.waiting_for_frame.load(Ordering::Acquire) {
            self.arm();
        }
    }
}

/// 셸이 알려준 프레임 시각을 버리는 나이. 네 주기다 -- 그보다 오래됐으면 셸이 틱을
/// 내지 않고 있다는 뜻이므로 지금 시각으로 떨어진다(설계 문서 3.3).
fn frame_time_max_age() -> Duration {
    paint_timer_period() * 4
}

/// The free-running paint-timer period, from `gfx_refresh_hz`.
///
/// ***Exact, not truncated.*** This used to be `Duration::from_millis(1000 / hz)`: integer
/// division into integer milliseconds, which cannot express a 60Hz period at all. 1000/60 is
/// 16ms = 62.5Hz, so frame production ran 4.2% ahead of the display that the pref exists to
/// match -- against the pref's own stated purpose of not beating with vsync. Every hz in
/// `501..=999` was worse still: it collapsed to 1ms, so asking for 600Hz produced 1000Hz.
///
/// ★낡음(2026-08-31 기록, 2026-09-08 `a18686dbd77` 이후 무효)★ — three 가 아니라 호출 지점이 11 곳이고, `script/raf_clock.rs:42` 의 바깥 제약이 하나 더 있다.
/// One function so the three places that need this period cannot drift apart again: this
/// timer, the wall pacer's default minimum interval, and the video composite coalescing.
///
/// Read once, like the value it replaces: `gfx_refresh_hz` is a startup pref, and the video
/// path asks for this per arriving frame (over a thousand times a second on a full wall).
pub(crate) fn paint_timer_period() -> Duration {
    static PERIOD: LazyLock<Duration> = LazyLock::new(|| {
        let raw_hz = servo_config::pref!(gfx_refresh_hz);
        let hz = if (1..=1000).contains(&raw_hz) {
            raw_hz
        } else {
            warn!("Ignoring gfx_refresh_hz={raw_hz} (must be in [1, 1000]); using default 120");
            120
        };
        Duration::from_secs_f64(1.0 / hz as f64)
    });
    *PERIOD
}

impl RefreshDriver for TimerRefreshDriver {
    fn observe_next_frame(&self, new_start_frame_callback: Box<dyn Fn() + Send + 'static>) {
        // Free-running paint-timer period. Default 120Hz. Override with the `gfx_refresh_hz`
        // pref (formerly the `SERVO_REFRESH_TIMER_HZ` env var) to match a specific display
        // refresh (e.g. 60) so frame production does not run faster than the display and beat
        // against its vsync (which shows up as periodic judder / non-uniform 60fps even with a
        // single video). Read once; clamped to [1, 1000] Hz.
        //
        // ★절대 격자 위에서 잰다 -- "지금부터 한 주기" 가 아니다.★ 예전에는
        // `queue_timer(paint_timer_period(), ..)` 였는데, 그것은 **호출된 순간부터** 한
        // 주기다. 이 드라이버는 일회성이라 콜백이 끝난 뒤 다시 걸리므로, 한 사이클이
        // `주기 + 디스패치·깨우기·처리 지연` 이 되고 그 지연이 **누적된다.** 구조적으로
        // 공칭 주사율보다 느려진다.
        //
        // 셸의 표출 클럭은 반대로 `while next <= now { next += period }` 라 표류하지 않는다.
        // 같은 공칭 주기의 두 시계 중 하나만 표류하면 둘은 반드시 서로를 지나가고, 그
        // 어긋남이 화면에서는 제자리걸음 뒤 두 배 점프로 보인다.
        //
        // 그래서 여기도 기준점에 주기를 더해 나간다. ★늦었으면 지나간 틱은 버린다★ --
        // 밀린 것을 몰아 내면 한 표시 간격에 여러 개가 들어가고, 그것이 정확히 없애려는
        // 현상이다. 디스플레이가 하는 일도 그것이다.
        let period = paint_timer_period();
        let now = Instant::now();
        let mut next = self.next_frame_at.get().unwrap_or(now + period);
        while next <= now {
            next += period;
        }
        self.next_frame_at.set(Some(next));
        self.queue_timer(next - now, new_start_frame_callback);
    }
}

impl Drop for TimerRefreshDriver {
    fn drop(&mut self) {
        let _ = self.sender.send(TimerThreadMessage::Quit);
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// 콜백을 모아 두기만 하는 드라이버. ★셸의 틱 드라이버와 같은 모양이다★ -- 스레드가
    /// 없고, 남이 부를 때까지 아무것도 쏘지 않는다.
    struct HoardingRefreshDriver {
        needs_heartbeat: bool,
        held: Mutex<Vec<Box<dyn Fn() + Send + 'static>>>,
    }

    impl HoardingRefreshDriver {
        fn new(needs_heartbeat: bool) -> Rc<Self> {
            Rc::new(Self {
                needs_heartbeat,
                held: Mutex::new(Vec::new()),
            })
        }
    }

    impl RefreshDriver for HoardingRefreshDriver {
        fn observe_next_frame(&self, start_frame_callback: Box<dyn Fn() + Send + 'static>) {
            self.held.lock().unwrap().push(start_frame_callback);
        }

        fn needs_paint_timer_heartbeat(&self) -> bool {
            self.needs_heartbeat
        }
    }

    #[derive(Clone)]
    struct CountingWaker(Arc<AtomicUsize>);

    impl EventLoopWaker for CountingWaker {
        fn clone_box(&self) -> Box<dyn EventLoopWaker> {
            Box::new(self.clone())
        }

        fn wake(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// ★자기 클럭이 없는 드라이버에는 타이머 스레드가 깨우기를 내 줘야 한다.★
    ///
    /// 그 드라이버는 콜백을 모아 두기만 하므로 이벤트 루프를 깨우지 않는다. 깨우기가 없으면
    /// 임베더의 틱이 올 일이 없고, 틱이 콜백을 쏘는 구조에서 그것은 ***화면이 영구히
    /// 멈춘다*** 는 뜻이다.
    ///
    /// ★깨우기만이다.★ 타이머가 콜백까지 쏘면 발사 주체가 다시 둘이 된다 -- 그것을
    /// `waiting_for_frame` 이 아직 서 있는 것으로 확인한다(콜백이 쏘였다면 내려갔을 값이다).
    ///
    /// ★두 번째 박동까지 본다.★ 하나만 보면 [`Heartbeat::beat`] 의 자기 재장전을 통째로
    /// 지워도 통과하는데, 재장전이 이 메커니즘의 전부다 -- 한 번 깨우고 마는 박동은 그
    /// 깨우기가 셸의 틱 게이트를 통과하지 못한 바로 그 경우에 사슬을 끊는다. 경과 시간까지
    /// 보는 것은 ★주기에도 눈을 달기 위해서다★: 두 박동이 걸린 시간을 보지 않으면 1Hz 로
    /// 걸어도 통과한다.
    #[test]
    fn a_driver_without_a_clock_of_its_own_gets_woken_by_the_paint_timer() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let timer_refresh_driver: LazyCell<Rc<TimerRefreshDriver>> = LazyCell::default();
        let driver = HoardingRefreshDriver::new(true);
        let base = BaseRefreshDriver::new(
            Box::new(CountingWaker(wakes.clone())),
            Some(driver.clone() as Rc<dyn RefreshDriver>),
            &timer_refresh_driver,
        );

        let started = Instant::now();
        base.observe_next_frame();

        // 주기는 `paint_timer_period()`. 느린 기계에서도 흔들리지 않도록 마감은 넉넉히 둔다 --
        // 아래 단언이 실제 간격을 본다.
        let deadline = started + Duration::from_secs(5);
        while wakes.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let elapsed = started.elapsed();

        assert!(
            wakes.load(Ordering::SeqCst) >= 2,
            "타이머 스레드가 두 번 깨우지 않았다 -- 박동이 자기를 다시 걸지 않으면 \
             틱 게이트가 아직 안 찬 그 한 번에 사슬이 끊긴다 (wakes={}, elapsed={elapsed:?})",
            wakes.load(Ordering::SeqCst)
        );
        assert!(
            elapsed >= paint_timer_period(),
            "두 박동이 한 주기보다 빨리 왔다 -- 절대 격자를 안 쓰고 있다 ({elapsed:?})"
        );
        // ★상한은 `paint_timer_period()` 를 쓰고 있는지만 가린다.★ 두 박동이면 두 주기
        // 남짓인데, Windows 의 기본 타이머 해상도(15.6ms)가 120Hz 주기(8.3ms)보다 굵어
        // 실측은 서너 주기까지 늘어난다. 여덟 주기면 그 흔들림의 두 배라 안정적이고,
        // 주기를 잘못 잡은 경우(예: 1Hz)는 그래도 잡힌다.
        assert!(
            elapsed < paint_timer_period() * 8,
            "박동 간격이 `paint_timer_period()` 가 아니다 ({elapsed:?}, \
             기대 ~2 x {:?})",
            paint_timer_period()
        );
        assert!(
            base.waiting_for_frame.load(Ordering::Acquire),
            "깨우기만 와야 한다 -- 콜백이 쏘였다면 waiting_for_frame 이 내려갔을 것이다"
        );
        assert_eq!(
            driver.held.lock().unwrap().len(),
            1,
            "콜백은 드라이버에 쌓인 채로 틱을 기다려야 한다"
        );
    }

    /// ★자기 스레드로 발사하는 드라이버에는 박동을 걸지 않는다.★ 걸면 그 구성의 깨우기가
    /// 두 배가 되고, 그것은 이 작업이 없애려는 두 번째 시계를 다시 들이는 것이다.
    #[test]
    fn a_driver_with_its_own_clock_gets_no_heartbeat() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let timer_refresh_driver: LazyCell<Rc<TimerRefreshDriver>> = LazyCell::default();
        let driver = HoardingRefreshDriver::new(false);
        let base = BaseRefreshDriver::new(
            Box::new(CountingWaker(wakes.clone())),
            Some(driver.clone() as Rc<dyn RefreshDriver>),
            &timer_refresh_driver,
        );

        base.observe_next_frame();
        thread::sleep(paint_timer_period() * 8);

        assert_eq!(
            wakes.load(Ordering::SeqCst),
            0,
            "자기 클럭이 있는 드라이버에 박동을 걸면 시계가 둘이 된다"
        );
    }
}

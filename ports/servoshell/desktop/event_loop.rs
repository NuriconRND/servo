/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! An event loop implementation that works in headless mode.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time;

use log::warn;
use servo::EventLoopWaker;
use winit::event_loop::{EventLoop, EventLoop as WinitEventLoop, EventLoopProxy};
use winit::window::WindowId;

use super::app::App;

#[derive(Debug)]
pub enum AppEvent {
    /// Another process or thread has kicked the OS event loop with EventLoopWaker.
    Waker,
    Accessibility(egui_winit::accesskit_winit::Event),
}

impl From<egui_winit::accesskit_winit::Event> for AppEvent {
    fn from(event: egui_winit::accesskit_winit::Event) -> AppEvent {
        AppEvent::Accessibility(event)
    }
}

impl AppEvent {
    pub(crate) fn window_id(&self) -> Option<WindowId> {
        match self {
            AppEvent::Waker => None,
            AppEvent::Accessibility(event) => Some(event.window_id),
        }
    }
}

/// A headed or headless event loop. Headless event loops are necessary for environments without a
/// display server. Ideally, we could use the headed winit event loop in both modes, but on Linux,
/// the event loop requires a display server, which prevents running servoshell in a console.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ServoShellEventLoop {
    /// A real Winit windowing event loop.
    Winit(EventLoop<AppEvent>),
    /// A fake event loop which contains a signalling flag used to ensure
    /// that pending events get processed in a timely fashion, and a condition
    /// variable to allow waiting on that flag changing state.
    Headless(Arc<HeadlessEventLoop>),
}

impl ServoShellEventLoop {
    pub(crate) fn headless() -> ServoShellEventLoop {
        ServoShellEventLoop::Headless(Default::default())
    }

    pub(crate) fn headed() -> ServoShellEventLoop {
        ServoShellEventLoop::Winit(
            WinitEventLoop::with_user_event()
                .build()
                .expect("Could not start winit event loop"),
        )
    }
}

impl ServoShellEventLoop {
    pub(crate) fn event_loop_proxy(&self) -> Option<EventLoopProxy<AppEvent>> {
        match self {
            ServoShellEventLoop::Winit(event_loop) => Some(event_loop.create_proxy()),
            ServoShellEventLoop::Headless(..) => None,
        }
    }

    /// Returns the embedder waker plus, for the headed (winit) backend, the
    /// [`WakeCoalescer`] shared with that waker. The app holds the coalescer and drives it
    /// from its control-flow handling (`set_animating` / `take_requested`) so compositor
    /// wake-ups don't starve hardware input on Windows. See [`WakeCoalescer`].
    pub fn create_event_loop_waker(&self) -> (Box<dyn EventLoopWaker>, Option<WakeCoalescer>) {
        match self {
            ServoShellEventLoop::Winit(event_loop) => {
                let coalescer = WakeCoalescer::default();
                (
                    Box::new(HeadedEventLoopWaker::new(event_loop, coalescer.clone())),
                    Some(coalescer),
                )
            },
            ServoShellEventLoop::Headless(data) => {
                (Box::new(HeadlessEventLoopWaker(data.clone())), None)
            },
        }
    }

    pub fn run_app(self, app: &mut App) {
        match self {
            ServoShellEventLoop::Winit(event_loop) => {
                event_loop
                    .run_app(app)
                    .expect("Failed while running events loop");
            },
            ServoShellEventLoop::Headless(event_loop) => event_loop.run_app(app),
        }
    }
}

/// Suppresses redundant compositor wake-ups while the app is animating.
///
/// `EventLoopWaker::wake()` wakes the winit loop via `EventLoopProxy::send_event`, which
/// on Windows is a `PostMessage` to the thread-event-target. In the Win32
/// `PeekMessage`/`GetMessage` priority order, POSTED messages outrank hardware INPUT
/// (`WM_MOUSEMOVE`/clicks/keys). While an animating page (e.g. a 60fps WebGPU demo, and
/// especially the multi-GPU wall) keeps the compositor waking the loop, there is always a
/// posted `Waker` ahead of queued input, so input is starved and the page never sees the
/// mouse. (On the wall this was the flock not following the cursor.)
///
/// Key insight: while animating, `ControlFlow::Poll` already makes `about_to_wait` pump
/// servo every iteration, so the `Waker` posts are *redundant*. So `wake()` does NOT post
/// while animating — it just records that a pump is owed (`requested`); `about_to_wait`
/// performs that pump. When idle, `wake()` posts normally (sparse, no starvation) to wake
/// the blocked loop with low latency.
///
/// No wake is lost across the animating→idle transition: `wake()` does
/// `requested = true` *then* reads `animating`, while `set_control_flow_for_state` does
/// `set_animating(false)` *then* reads `requested`. Under `SeqCst` this ordering guarantees
/// that a wake which saw `animating == true` (and was therefore suppressed) is always
/// observed by the subsequent `requested` check, which forces one more `Poll` iteration to
/// pump it.
#[derive(Clone, Default)]
pub(crate) struct WakeCoalescer {
    animating: Arc<AtomicBool>,
    requested: Arc<AtomicBool>,
}

impl WakeCoalescer {
    /// Called from `wake()`. Returns `true` if a real `Waker` should be posted now.
    fn begin_wake(&self) -> bool {
        // Order matters (see the struct doc): record the request before reading `animating`.
        self.requested.store(true, Ordering::SeqCst);
        // Post only when NOT animating; while animating, `about_to_wait` pumps every Poll
        // iteration and a posted Waker would only starve hardware input.
        !self.animating.load(Ordering::SeqCst)
    }

    pub(crate) fn set_animating(&self, animating: bool) {
        self.animating.store(animating, Ordering::SeqCst);
    }

    /// Take (clear) the "pump owed" flag, returning whether a wake had occurred.
    pub(crate) fn take_requested(&self) -> bool {
        self.requested.swap(false, Ordering::SeqCst)
    }

    /// Whether a wake is currently owed a pump (read without clearing).
    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}

/// Runtime override that restores the pre-`WakeEdge` behavior of posting a `Waker` on every
/// call, in case edge-triggering ever needs to be ruled out as a cause. Only the literal
/// value `"1"` disables the gate; anything else (unset, `"0"`, `"true"`, ...) leaves it
/// enabled. See [`WakeEdge`].
pub(crate) const WAKE_EDGE_KILLSWITCH_ENV: &str = "SERVO_DISABLE_WAKE_EDGE";

/// Pure killswitch predicate for [`WAKE_EDGE_KILLSWITCH_ENV`]. `token` is typically
/// `std::env::var(WAKE_EDGE_KILLSWITCH_ENV).ok()`. Factored out from the env read so the
/// "only literal 1" rule is unit-testable without touching process environment.
pub(crate) fn wake_edge_suppressed(token: Option<&str>) -> bool {
    token == Some("1")
}

/// Cached (read-once) killswitch state. `notify_new_frame_ready` is on the hottest path in
/// the bug this exists to fix (up to ~20k calls/s during the storm described in
/// pacing-investigation-report.md), so re-reading the environment on every call would be
/// wasteful; the env var is a startup-time debug toggle and is not expected to change while
/// the process is running.
static WAKE_EDGE_KILLSWITCH: LazyLock<bool> =
    LazyLock::new(|| wake_edge_suppressed(std::env::var(WAKE_EDGE_KILLSWITCH_ENV).ok().as_deref()));

/// Edge-triggers the `event_loop_waker.wake()` posted from `notify_new_frame_ready` on a new
/// frame-ready notification, instead of posting unconditionally on every notification.
///
/// This targets a different failure mode than [`WakeCoalescer`]: `WakeCoalescer` suppresses
/// posts only while `is_animating()`; without an active `requestAnimationFrame` loop (e.g. a
/// plain `<video>` grid) the webview never becomes "animating", so `WakeCoalescer` is a
/// no-op and every `notify_new_frame_ready` posts a `Waker`. On Windows, posted messages
/// outrank queued `WM_PAINT` in `PeekMessage` priority order, so a frame-ready notification
/// arriving faster than the paint that would clear it (every `spin_event_loop` iteration, in
/// the observed case) creates a self-sustaining posted-message flood that starves
/// `RedrawRequested` indefinitely and freezes the page. See
/// `.superpowers/sdd/pacing-investigation-report.md` chain stages F (this gate) and H (where
/// [`Self::clear`] must be called).
///
/// `outstanding` latches "a wake has been posted for a repaint that has not yet happened":
/// [`Self::should_post`] returns `true` only on the false→true transition and `false` while
/// already latched, so repeated notifications while a repaint is pending post at most once.
/// [`Self::clear`] must be called only when the embedder has *actually painted* (report's H
/// point, e.g. after `window.repaint_webviews()` runs) -- clearing when a `Waker` event is
/// merely dequeued from the event loop would re-open the same self-sustaining flood, since
/// nothing would then stop the next spin from posting again before the paint happens.
///
/// `AtomicBool`/`SeqCst`: `should_post()` is called from the embedder-delegate thread and
/// `clear()` from the winit event-loop thread (same cross-thread shape as [`WakeCoalescer`]
/// above; see its doc comment for the ordering argument). `SeqCst` keeps the same
/// conservative guarantee here rather than reasoning about a weaker ordering for a latch that
/// is not on a tight hot loop once the fix is working (post-fix, `should_post`/`clear` each
/// fire at most once per repaint, not once per spin).
#[derive(Default)]
pub(crate) struct WakeEdge {
    outstanding: AtomicBool,
}

impl WakeEdge {
    /// Returns `true` on the false→true transition of the latch (post a wake now); `false`
    /// while a wake is already outstanding (suppress -- a paint is already pending that will
    /// eventually call [`Self::clear`]). The killswitch bypasses the latch entirely and
    /// always returns `true`, restoring the unconditional-post behavior.
    pub(crate) fn should_post(&self) -> bool {
        *WAKE_EDGE_KILLSWITCH || !self.outstanding.swap(true, Ordering::SeqCst)
    }

    /// Re-arms the latch for the next transition. Must only be called once the embedder has
    /// actually painted (report's H point), not when a `Waker` event is dequeued.
    pub(crate) fn clear(&self) {
        self.outstanding.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::{WakeEdge, wake_edge_suppressed};

    #[test]
    fn wake_edge_posts_only_on_transition() {
        let edge = WakeEdge::default();
        assert!(edge.should_post()); // first latch → post
        assert!(!edge.should_post()); // outstanding held → suppress
        assert!(!edge.should_post());
        edge.clear(); // paint completed
        assert!(edge.should_post()); // new transition → post again (no lost wake)
    }

    #[test]
    fn wake_edge_killswitch_literal_one_only() {
        assert!(!wake_edge_suppressed(None));
        assert!(wake_edge_suppressed(Some("1")));
        assert!(!wake_edge_suppressed(Some("0")));
        assert!(!wake_edge_suppressed(Some("true")));
    }
}

#[derive(Clone)]
struct HeadedEventLoopWaker {
    proxy: Arc<Mutex<EventLoopProxy<AppEvent>>>,
    coalescer: WakeCoalescer,
}

impl HeadedEventLoopWaker {
    fn new(event_loop: &EventLoop<AppEvent>, coalescer: WakeCoalescer) -> HeadedEventLoopWaker {
        let proxy = Arc::new(Mutex::new(event_loop.create_proxy()));
        HeadedEventLoopWaker { proxy, coalescer }
    }
}

impl EventLoopWaker for HeadedEventLoopWaker {
    fn wake(&self) {
        // While animating, suppress the post (the Poll loop pumps anyway) so posted
        // messages don't starve hardware input on Windows. See `WakeCoalescer`.
        if !self.coalescer.begin_wake() {
            return;
        }
        if let Err(err) = self.proxy.lock().unwrap().send_event(AppEvent::Waker) {
            warn!("Failed to wake up event loop ({}).", err);
        }
    }

    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }
}

/// The [`HeadlessEventLoop`] is used when running in headless mode. The event
/// loop just loops over a condvar to simulate a real windowing system event loop.
#[derive(Default)]
pub(crate) struct HeadlessEventLoop {
    guard: Arc<Mutex<bool>>,
    condvar: Condvar,
}

impl HeadlessEventLoop {
    fn run_app(&self, app: &mut App) {
        app.init(None);

        loop {
            self.sleep();
            if !app.pump_servo_event_loop(None) {
                break;
            }
            *self.guard.lock().unwrap() = false;
        }
    }

    fn sleep(&self) {
        // To avoid sleeping when we should be processing events, do two things:
        // * before sleeping, check whether our signalling flag has been set
        // * wait on a condition variable with a maximum timeout, to allow
        //   being woken up by any signals that occur while sleeping.
        let guard = self.guard.lock().unwrap();
        if *guard {
            return;
        }
        let _ = self
            .condvar
            .wait_timeout(guard, time::Duration::from_millis(5))
            .unwrap();
    }
}

#[derive(Clone)]
struct HeadlessEventLoopWaker(Arc<HeadlessEventLoop>);

impl EventLoopWaker for HeadlessEventLoopWaker {
    fn wake(&self) {
        // Set the signalling flag and notify the condition variable.
        // This ensures that any sleep operation is interrupted,
        // and any non-sleeping operation will have a change to check
        // the flag before going to sleep.
        let mut flag = self.0.guard.lock().unwrap();
        *flag = true;
        self.0.condvar.notify_all();
    }

    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }
}

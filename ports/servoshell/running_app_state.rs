/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Shared state and methods for desktop and EGL implementations.

use std::cell::{Cell, Ref, RefCell};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::rc::Rc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use euclid::Rect;
use image::{DynamicImage, ImageFormat, RgbaImage};
#[cfg(all(
    any(coverage, llvm_pgo),
    any(target_os = "android", target_env = "ohos")
))]
use libc::c_char;
use log::{error, info, warn};
use servo::{
    AllowOrDenyRequest, AuthenticationRequest, BluetoothDeviceSelectionRequest, CSSPixel,
    ConsoleLogLevel, CreateNewWebViewRequest, DeviceIntPoint, DeviceIntSize, EmbedderControl,
    EmbedderControlId, EventLoopWaker, GenericSender, InputEvent, InputEventId, InputEventResult,
    JSValue, LoadStatus, MediaSessionEvent, PermissionRequest, PrefValue, Preferences,
    ScreenshotCaptureError, Servo, ServoDelegate, ServoError, TraversalId, UserContentManager,
    WebDriverCommandMsg, WebDriverJSResult, WebDriverLoadStatus, WebDriverScriptCommand,
    WebDriverSenders, WebView, WebViewDelegate, WebViewId,
};
use url::Url;

#[cfg(all(
    feature = "gamepad",
    not(any(target_os = "android", target_env = "ohos"))
))]
pub(crate) use crate::desktop::gamepad::ServoshellGamepadDelegate;
use crate::prefs::{EXPERIMENTAL_PREFS, ServoShellPreferences};
use crate::webdriver::WebDriverEmbedderControls;
use crate::window::{PlatformWindow, ServoShellWindow, ServoShellWindowId};

#[cfg(all(
    any(coverage, llvm_pgo),
    any(target_os = "android", target_env = "ohos")
))]
unsafe extern "C" {
    fn __llvm_profile_set_filename(file: *const c_char);
    fn __llvm_profile_write_file();
}

#[derive(Default)]
pub struct WebViewCollection {
    /// List of top-level browsing contexts.
    /// Modified by EmbedderMsg::WebViewOpened and EmbedderMsg::WebViewClosed,
    /// and we exit if it ever becomes empty.
    webviews: HashMap<WebViewId, WebView>,

    /// The order in which the webviews were created.
    pub(crate) creation_order: Vec<WebViewId>,

    /// The [`WebView`] that is currently active. This is the [`WebView`] that is shown and has
    /// input focus.
    active_webview_id: Option<WebViewId>,
}

impl WebViewCollection {
    pub fn add(&mut self, webview: WebView) {
        let id = webview.id();
        self.creation_order.push(id);
        self.webviews.insert(id, webview);
    }

    /// Removes a webview from the collection by [`WebViewId`]. If the removed [`WebView`] was the active
    /// [`WebView`] then the next newest [`WebView`] will be activated.
    pub fn remove(&mut self, id: WebViewId) -> Option<WebView> {
        self.creation_order.retain(|&webview_id| webview_id != id);
        let removed_webview = self.webviews.remove(&id);

        if self.active_webview_id == Some(id) {
            self.active_webview_id = None;
            if let Some(newest) = self.creation_order.last() {
                self.activate_webview(*newest);
            }
        }

        removed_webview
    }

    pub fn get(&self, id: WebViewId) -> Option<&WebView> {
        self.webviews.get(&id)
    }

    pub fn contains(&self, id: WebViewId) -> bool {
        self.webviews.contains_key(&id)
    }

    pub fn active(&self) -> Option<&WebView> {
        self.active_webview_id.and_then(|id| self.webviews.get(&id))
    }

    pub fn active_id(&self) -> Option<WebViewId> {
        self.active_webview_id
    }

    /// Gets a reference to the most recently created webview, if any.
    pub fn newest(&self) -> Option<&WebView> {
        self.creation_order
            .last()
            .and_then(|id| self.webviews.get(id))
    }

    pub fn all_in_creation_order(&self) -> impl Iterator<Item = (WebViewId, &WebView)> {
        self.creation_order
            .iter()
            .filter_map(move |id| self.webviews.get(id).map(|webview| (*id, webview)))
    }

    /// Returns an iterator over all webview references (in arbitrary order).
    pub fn values(&self) -> impl Iterator<Item = &WebView> {
        self.webviews.values()
    }

    /// Returns true if the collection contains no webviews.
    pub fn is_empty(&self) -> bool {
        self.webviews.is_empty()
    }

    pub(crate) fn activate_webview(&mut self, id_to_activate: WebViewId) {
        assert!(self.creation_order.contains(&id_to_activate));

        self.active_webview_id = Some(id_to_activate);
        for (webview_id, webview) in self.all_in_creation_order() {
            if id_to_activate == webview_id {
                webview.show();
                webview.focus();
            } else {
                webview.hide();
                webview.blur();
            }
        }
    }

    pub(crate) fn activate_webview_by_index(&mut self, index: usize) {
        let Some(webview_id) = self.creation_order.get(index) else {
            // Just ignore requests to activate uknown WebViews. This can happen by pressing
            // keyboard shortcuts in the interface.
            return;
        };
        self.activate_webview(*webview_id);
    }
}

/// A command received via the user interacting with the user interface.
#[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
pub(crate) enum UserInterfaceCommand {
    Go(String),
    Back,
    Forward,
    Reload,
    ReloadAll,
    NewWebView,
    CloseWebView(WebViewId),
    NewWindow,
}

pub(crate) struct RunningAppState {
    /// The gamepad provider, used for handling gamepad events and set on each WebView.
    /// May be `None` if gamepad support is disabled or failed to initialize.
    #[cfg(all(
        feature = "gamepad",
        not(any(target_os = "android", target_env = "ohos"))
    ))]
    gamepad_delegate: Option<Rc<ServoshellGamepadDelegate>>,

    /// The [`WebDriverSenders`] used to reply to pending WebDriver requests.
    pub(crate) webdriver_senders: RefCell<WebDriverSenders>,

    /// When running in WebDriver mode, [`WebDriverEmbedderControls`] is a virtual container
    /// for all embedder controls. This overrides the normal behavior where these controls
    /// are shown in the GUI or not processed at all in headless mode.
    pub(crate) webdriver_embedder_controls: WebDriverEmbedderControls,

    /// A [`HashMap`] of pending WebDriver events. It is the WebDriver embedder's responsibility
    /// to inform the WebDriver server when the event has been fully handled. This map is used
    /// to report back to WebDriver when that happens.
    pub(crate) pending_webdriver_events: RefCell<HashMap<InputEventId, Sender<()>>>,

    /// A [`Receiver`] for receiving commands from a running WebDriver server, if WebDriver
    /// was enabled.
    pub(crate) webdriver_receiver: Option<Receiver<WebDriverCommandMsg>>,

    /// servoshell specific preferences created during startup of the application.
    pub(crate) servoshell_preferences: ServoShellPreferences,

    /// A handle to the Servo instance.
    pub(crate) servo: Servo,

    /// Wakes the platform event loop when Servo reports that a new frame can be presented.
    event_loop_waker: Box<dyn EventLoopWaker>,

    /// WebViews that currently need periodic embedder event-loop spins for animation.
    animating_webviews: RefCell<HashMap<WebViewId, bool>>,

    /// Whether or not the application has achieved stable image output. This is used
    /// for the `exit_after_stable_image` option.
    pub(crate) achieved_stable_image: Rc<Cell<bool>>,

    /// The [`UserContentManager`] for all `WebView`s created.
    pub(crate) user_content_manager: Rc<UserContentManager>,

    /// Whether or not program exit has been triggered. This means that all windows
    /// will be destroyed and shutdown will start at the end of the current event loop.
    exit_scheduled: Cell<bool>,

    /// Whether the user has enabled experimental preferences.
    experimental_preferences_enabled: Cell<bool>,

    /// The set of [`ServoShellWindow`]s that currently exist for this instance of servoshell.
    // This is the last field of the struct to ensure that windows are dropped *after* all
    // other references to the relevant rendering contexts have been destroyed.
    // See https://github.com/servo/servo/issues/36711.
    windows: RefCell<HashMap<ServoShellWindowId, Rc<ServoShellWindow>>>,

    /// The currently focused [`ServoShellWindow`], if one is focused.
    focused_window: RefCell<Option<Rc<ServoShellWindow>>>,

    /// Whether accessibility is active in servoshell.
    ///
    /// Set by the platform via AccessKit, and forwarded to existing and new WebViews via
    /// [`WebView::set_accessibility_active()`], in [`Self::set_accessibility_active()`] and
    /// and [`ServoShellWindow::create_toplevel_webview()`].
    accessibility_active: Cell<bool>,
}

impl RunningAppState {
    pub(crate) fn new(
        servo: Servo,
        servoshell_preferences: ServoShellPreferences,
        event_loop_waker: Box<dyn EventLoopWaker>,
        user_content_manager: Rc<UserContentManager>,
        default_preferences: Preferences,
        #[cfg(all(
            feature = "gamepad",
            not(any(target_os = "android", target_env = "ohos"))
        ))]
        gamepad_delegate: Option<Rc<ServoshellGamepadDelegate>>,
    ) -> Self {
        servo.set_delegate(Rc::new(ServoShellServoDelegate));

        let webdriver_receiver = servoshell_preferences.webdriver_port.get().map(|port| {
            let (embedder_sender, embedder_receiver) = unbounded();
            webdriver_server::start_server(
                port,
                embedder_sender,
                event_loop_waker.clone(),
                default_preferences,
            );
            embedder_receiver
        });

        let experimental_preferences_enabled =
            Cell::new(servoshell_preferences.experimental_preferences_enabled);

        Self {
            windows: Default::default(),
            focused_window: Default::default(),
            #[cfg(all(
                feature = "gamepad",
                not(any(target_os = "android", target_env = "ohos"))
            ))]
            gamepad_delegate,
            webdriver_senders: RefCell::default(),
            webdriver_embedder_controls: Default::default(),
            pending_webdriver_events: Default::default(),
            webdriver_receiver,
            servoshell_preferences,
            servo,
            event_loop_waker,
            animating_webviews: Default::default(),
            achieved_stable_image: Default::default(),
            exit_scheduled: Default::default(),
            user_content_manager,
            experimental_preferences_enabled,
            accessibility_active: Cell::new(false),
        }
    }

    pub(crate) fn open_window(
        self: &Rc<Self>,
        platform_window: Rc<dyn PlatformWindow>,
        initial_url: Url,
    ) -> Rc<ServoShellWindow> {
        let window = Rc::new(ServoShellWindow::new(platform_window.clone()));
        self.windows
            .borrow_mut()
            .insert(window.id(), window.clone());
        window.create_and_activate_toplevel_webview(self.clone(), initial_url);

        // If the window already has platform focus, mark it as focused in our application state.
        if platform_window.has_platform_focus() {
            self.focus_window(window.clone());
        }

        window
    }

    pub(crate) fn open_window_with_shared_webview(
        self: &Rc<Self>,
        platform_window: Rc<dyn PlatformWindow>,
        webview: WebView,
    ) -> Rc<ServoShellWindow> {
        let window = Rc::new(ServoShellWindow::new(platform_window.clone()));
        self.windows
            .borrow_mut()
            .insert(window.id(), window.clone());
        window.add_shared_toplevel_webview(self.clone(), webview);

        // If the window already has platform focus, mark it as focused in our application state.
        if platform_window.has_platform_focus() {
            self.focus_window(window.clone());
        }

        window
    }

    pub(crate) fn windows<'a>(
        &'a self,
    ) -> Ref<'a, HashMap<ServoShellWindowId, Rc<ServoShellWindow>>> {
        self.windows.borrow()
    }

    pub(crate) fn focused_window(&self) -> Option<Rc<ServoShellWindow>> {
        self.focused_window.borrow().clone()
    }

    pub(crate) fn is_animating(&self) -> bool {
        self.animating_webviews
            .borrow()
            .values()
            .any(|animating| *animating)
    }

    pub(crate) fn focus_window(&self, window: Rc<ServoShellWindow>) {
        window.focus();
        *self.focused_window.borrow_mut() = Some(window);
    }

    #[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
    pub(crate) fn window(&self, id: ServoShellWindowId) -> Option<Rc<ServoShellWindow>> {
        self.windows.borrow().get(&id).cloned()
    }

    pub(crate) fn webview_by_id(&self, webview_id: WebViewId) -> Option<WebView> {
        self.maybe_window_for_webview_id(webview_id)?
            .webview_by_id(webview_id)
    }

    pub(crate) fn webdriver_receiver(&self) -> Option<&Receiver<WebDriverCommandMsg>> {
        self.webdriver_receiver.as_ref()
    }

    pub(crate) fn servo(&self) -> &Servo {
        &self.servo
    }

    #[cfg(all(
        feature = "gamepad",
        not(any(target_os = "android", target_env = "ohos"))
    ))]
    pub(crate) fn gamepad_delegate(&self) -> Option<Rc<ServoshellGamepadDelegate>> {
        self.gamepad_delegate.clone()
    }

    pub(crate) fn schedule_exit(&self) {
        // When explicitly required to shutdown, unset webdriver port
        // which allows normal shutdown.
        // Note that when not explicitly required to shutdown, we still keep Servo alive
        // when all tabs are closed when `webdriver_port` enabled, which is necessary
        // to run wpt test using servodriver.
        self.servoshell_preferences.webdriver_port.set(None);
        self.exit_scheduled.set(true);

        #[cfg(all(
            any(coverage, llvm_pgo),
            any(target_os = "android", target_env = "ohos")
        ))]
        {
            use std::ffi::CString;

            use crate::prefs::default_config_dir;

            let mut profile_path = default_config_dir().expect("Need a config dir");
            profile_path.push("profiles/");
            let filename = format!(
                "{}/profile-%h-%p.profraw",
                profile_path.to_str().expect("Should be unicode")
            );
            let c_filename = CString::new(filename).expect("Need a valid cstring");
            unsafe {
                __llvm_profile_set_filename(c_filename.as_ptr() as *const c_char);
                __llvm_profile_write_file()
            }
        }
    }

    #[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
    pub(crate) fn experimental_preferences_enabled(&self) -> bool {
        self.experimental_preferences_enabled.get()
    }

    #[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
    pub(crate) fn set_experimental_preferences_enabled(&self, new_value: bool) {
        let old_value = self.experimental_preferences_enabled.replace(new_value);
        if old_value == new_value {
            return;
        }
        for pref in EXPERIMENTAL_PREFS {
            self.servo.set_preference(pref, PrefValue::Bool(new_value));
        }
    }

    /// Close any [`ServoShellWindow`] that doesn't have an open [`WebView`].
    fn close_empty_windows(&self) {
        self.windows.borrow_mut().retain(|_, window| {
            if !self.exit_scheduled.get() && !window.should_close() {
                return true;
            }

            if let Some(focused_window) = self.focused_window()
                && Rc::ptr_eq(window, &focused_window)
            {
                *self.focused_window.borrow_mut() = None;
            }
            false
        });
    }

    /// Spins the internal application event loop.
    ///
    /// - Notifies Servo about incoming gamepad events
    /// - Spin the Servo event loop, which will update Servo's embedding layer and trigger
    ///   delegate methods.
    ///
    /// Returns true if the event loop should continue spinning and false if it should exit.
    pub(crate) fn spin_event_loop(
        self: &Rc<Self>,
        create_platform_window: Option<&dyn Fn(Url) -> Rc<dyn PlatformWindow>>,
    ) -> bool {
        // We clone here to avoid a double borrow. User interface commands can update the list of windows.
        let windows: Vec<_> = self.windows.borrow().values().cloned().collect();
        for window in windows {
            window.handle_interface_commands(self, create_platform_window);
        }

        self.handle_webdriver_messages(create_platform_window);

        #[cfg(all(
            feature = "gamepad",
            not(any(target_os = "android", target_env = "ohos"))
        ))]
        if servo::pref!(dom_gamepad_enabled) {
            self.handle_gamepad_events();
        }

        self.servo.spin_event_loop();

        // ★이 셸의 틱 지점 -- 이벤트 처리 뒤, 렌더 앞.★ 2026-10-22 Task 9.5, 설계 2-2/2-4.
        self.on_demand_wall_tick();

        let windows_to_update: Vec<_> = self.windows.borrow().values().cloned().collect();
        let mut windows_to_repaint = Vec::new();
        for window in windows_to_update {
            if window.update_and_take_repaint_request(self) {
                windows_to_repaint.push(window);
            }
        }
        self.extend_wall_tile_repaint_requests(&mut windows_to_repaint);
        if self.servoshell_preferences.wall_all_tiles {
            let mut repainted_webviews = Vec::new();
            for window in windows_to_repaint {
                let Some(webview) = window.active_webview() else {
                    window.repaint_now(self);
                    continue;
                };
                if repainted_webviews.contains(&webview.id()) {
                    continue;
                }
                repainted_webviews.push(webview.id());
                window.repaint_now(self);
            }
        } else {
            for window in windows_to_repaint {
                window.request_repaint();
            }
        }

        if self.servoshell_preferences.exit_after_stable_image && self.achieved_stable_image.get() {
            self.schedule_exit();
        }

        self.close_empty_windows();

        // When no more windows are open, exit the application. Do not do this when
        // running WebDriver, which expects to keep running with no WebView open.
        if self.servoshell_preferences.webdriver_port.get().is_none()
            && self.windows.borrow().is_empty()
        {
            self.schedule_exit()
        }

        !self.exit_scheduled.get()
    }

    /// ★이 셸의 틱 에지 -- 벽 프레임을 내는 유일한 자리.★ 2026-10-22 Task 9.5.
    ///
    /// 자유 실행 표출 클럭이 없는 임베더가 전부 여기를 지난다: 데스크톱 servoshell
    /// (`desktop/app.rs` 의 `pump_servo_event_loop`), WPT(= `--headless` servoshell,
    /// `desktop/event_loop.rs` 의 `HeadlessEventLoop`), Android·OHOS(`egl/app.rs` 의
    /// "Servo heartbeat"). 셋이 `RunningAppState::spin_event_loop` 하나를 공유하므로
    /// ★배선도 한 자리면 된다★.
    ///
    /// ★깨우기가 예약한 에지에서만 생산한다.★ `WebView::on_demand_wall_tick` 이 예약
    /// 비트를 `swap(false)` 로 가져가고, 참이었을 때만 `Paint::on_wall_tick`(구성 요소
    /// 수집 -> 생산)으로 내려간다. 예약이 없으면 아무 일도 없다 -- 깨우기가 없는 조용한
    /// 페이지는 생산도 없다(설계 불변식 16). 예약을 거는 자리는 `paint` 안에 셋이다:
    /// 스크립트의 `PaintMessage::GenerateFrame`(= 새 디스플레이 리스트),
    /// `PaintMessage::UpdateImages`(= 캔버스·비디오 이미지가 WR 에 닿았다),
    /// `Painter::pump_paint_animation`(= 페인트측 애니메이션·비디오). ★셋째는 렌더 끝과
    /// 유휴 폴백에서 부른 것만이다★ -- 틱 자신의 구성 요소 수집(`PumpSource::Tick`)은
    /// 예약하지 않는다. 걸면 틱 N 이 틱 N+1 을 되먹여 생산이 이 루프의 폴링 속도로 돈다
    /// (2026-10-22 실측: 초당 13,000 논리 프레임 대 WR 합성 60).
    ///
    /// ★`WebView` 는 문을 여는 손잡이일 뿐이다.★ 이 틱은 벽 전체에 하나이고
    /// (`Paint::on_demand_wall_tick` 은 `Paint` 를 통째로 돈다), 고른 `WebView` 가 생산
    /// 범위를 정하지 않는다. 그래서 ★한 번만 부른다★ -- 여럿을 돌면 첫 호출이 비트를
    /// 가져가 나머지는 전부 `None` 이고, 그것은 틱이 여럿인 것처럼 보이는 거짓 그림이다.
    ///
    /// ★생산을 끄는 손잡이 뒤에 두지 않는다 -- 그런 손잡이는 이제 없다.★ 한때
    /// `gfx_single_frame_producer` 가 있었지만 이 셸은 처음부터 그 뒤에 두지 않았고
    /// (대안 생산자가 없어 끄는 것이 폴백이 아니라 ***순백 화면*** 이다 -- 2026-10-22
    /// 배선 전 실측: `WALLBOOTSTRAP ... tick_frames_produced=0` 뒤에 헤드리스 스크린샷이
    /// 1024x740 전면 `#FFFFFF`), 2026-10-22 에 `winit_wall` 쪽 게이트까지 삭제되면서 그
    /// pref 자체가 사라졌다. 두 셸이 같은 규칙으로 돈다.
    ///
    /// ★`servo::note_present_tick()` 은 부르지 않는다.★ 이 셸의 `wall_tick::TICK_SEQ`
    /// writer 는 `Paint::on_demand_wall_tick` 하나다. 둘 다 부르면 한 에지에 번호가 두 번
    /// 올라 `ExternalPresentBudget` 의 "틱당 1 회" 천장이 깨진다.
    ///
    /// ★표출률 상한이 없다 -- 에지는 ***이벤트 루프의 스핀***이지 표출 클럭이 아니다.★
    /// 헤드리스는 5ms condvar 폴링이고(`desktop/event_loop.rs`), 헤드는 애니메이션 중
    /// `ControlFlow::Poll` 이라 둘 다 패널 주사율과 무관하다. 실측(2026-10-22, 60Hz 패널
    /// + rAF 페이지): 초당 63~64 장. `winit_wall` 쪽 상한
    /// (`gfx_refresh_hz` / vsync / 페이싱 스레드)에 해당하는 것이 이 셸에는 없다.
    ///
    /// ★그래도 폭주하지 않는다 -- 다만 이 주장은 한 번 거짓이었다.★ 상한을 주는 것은
    /// 예약 비트가 에지당 한 번만 소비되는 것(`swap(false)`)과, ★그 비트를 다시 세우는
    /// 것이 전부 렌더에 묶여 있다★ 는 사실이다: rAF 는 렌더 -> `notify_will_paint` ->
    /// rAF -> `GenerateFrame` -> 예약이고, 페인트측 애니메이션은 렌더 끝의
    /// `pump_paint_animation`(`PumpSource::Render`)이다. 그리고 `renderer_behind` 가 앞
    /// 프레임이 안 그려진 타일의 생산을 거절하므로 in-flight 는 한 장을 안 넘는다.
    ///
    /// ★2026-10-22 전수 리뷰 전까지는 그 "전부" 가 참이 아니었다.★ 틱 자신의 구성 요소
    /// 수집(`Paint::gather_components` -> `pump_paint_animation`)이 다음 에지를 다시
    /// 예약해서, 페인트측 CSS 애니메이션이 도는 페이지에서는 ***틱 N 이 틱 N+1 을
    /// 되먹였다***. 실측(headed servoshell, `transform` 애니메이션 한 개, 20 초):
    /// 논리 프레임 초당 13,000(최대 21,568) 대 WR 합성 60. WR 프레임은 `renderer_behind`
    /// 가 묶어 주어 fps 로는 안 보였고, 남는 증상이 메인 스레드 CPU 와
    /// `next_logical_frame_id` 소각이었다. `PumpSource::Tick` 이 그 되먹임을 끊었고, 같은
    /// 측정이 초당 60 으로 돌아왔다. 진단 줄은 `WALLTICKRATE`(`Paint::note_demand_tick`).
    ///
    /// 상한(최소 간격 게이트)을 두고 싶어지면 그것은 ★이 자리★ 이지 `Paint` 안이 아니다.
    /// 다만 2026-10-22 에는 두지 않았다 -- 되먹임을 끊는 쪽이 상한 상수를 새로 고르는
    /// 것보다 낫고, 끊고 나면 박자는 이미 있는 렌더 클럭이 준다.
    fn on_demand_wall_tick(&self) {
        let Some(webview) = self.any_webview() else {
            return;
        };
        webview.on_demand_wall_tick();
    }

    /// 틱 문을 열 `WebView` 하나. ★어느 것이든 같다★ -- [`Self::on_demand_wall_tick`] 참고.
    ///
    /// 활성 `WebView` 를 먼저 보는 것은 ★스핀마다 Vec 을 만들지 않기 위해서다★ --
    /// `ServoShellWindow::webviews()` 는 그 창의 `WebView` 를 전부 복제해 담는다. 활성이
    /// 없는 창(띄운 직후, WebDriver)이 있을 수 있어 그 경우에만 그쪽으로 내려간다.
    ///
    /// `windows` 의 빌림은 이 함수가 돌려줄 때 끝난다 -- 틱은 painter 를 건드리므로 빌린
    /// 채로 넘어가지 않는다.
    fn any_webview(&self) -> Option<WebView> {
        let windows = self.windows.borrow();
        if let Some(active) = windows.values().find_map(|window| window.active_webview()) {
            return Some(active);
        }
        windows.values().find_map(|window| {
            window
                .webviews()
                .into_iter()
                .next()
                .map(|(_id, webview)| webview)
        })
    }

    pub(crate) fn maybe_window_for_webview_id(
        &self,
        webview_id: WebViewId,
    ) -> Option<Rc<ServoShellWindow>> {
        if let Some(focused_window) = self.focused_window()
            && focused_window.contains_webview(webview_id)
        {
            return Some(focused_window);
        }

        for window in self.windows.borrow().values() {
            if window.contains_webview(webview_id) {
                return Some(window.clone());
            }
        }
        None
    }

    pub(crate) fn windows_for_webview_id(
        &self,
        webview_id: WebViewId,
    ) -> Vec<Rc<ServoShellWindow>> {
        self.windows
            .borrow()
            .values()
            .filter(|window| window.contains_webview(webview_id))
            .cloned()
            .collect()
    }

    fn extend_wall_tile_repaint_requests(
        &self,
        windows_to_repaint: &mut Vec<Rc<ServoShellWindow>>,
    ) {
        if !self.servoshell_preferences.wall_all_tiles {
            return;
        }

        let original_repaint_requests = windows_to_repaint.clone();
        for window in original_repaint_requests {
            let Some(webview) = window.active_webview() else {
                continue;
            };
            for tile_window in self.windows_for_webview_id(webview.id()) {
                if windows_to_repaint
                    .iter()
                    .any(|window| window.id() == tile_window.id())
                {
                    continue;
                }
                windows_to_repaint.push(tile_window);
            }
        }
    }

    pub(crate) fn window_for_webview_id(&self, webview_id: WebViewId) -> Rc<ServoShellWindow> {
        self.maybe_window_for_webview_id(webview_id)
            .unwrap_or_else(|| panic!("Looking for unexpected WebView: {webview_id:?}"))
    }

    pub(crate) fn platform_window_for_webview_id(
        &self,
        webview_id: WebViewId,
    ) -> Rc<dyn PlatformWindow> {
        self.window_for_webview_id(webview_id).platform_window()
    }

    /// If we are exiting after achieving a stable image or we want to save the display of the
    /// [`WebView`] to an image file, request a screenshot of the [`WebView`].
    fn maybe_request_screenshot(&self, webview: WebView) {
        let output_path = self.servoshell_preferences.output_image_path.clone();
        if !self.servoshell_preferences.exit_after_stable_image && output_path.is_none() {
            return;
        }

        // Never request more than a single screenshot for now.
        let achieved_stable_image = self.achieved_stable_image.clone();
        if achieved_stable_image.get() {
            return;
        }

        webview.take_screenshot(None, move |image| {
            achieved_stable_image.set(true);

            let Some(output_path) = output_path else {
                return;
            };

            let image = match image {
                Ok(image) => image,
                Err(error) => {
                    error!("Could not take screenshot: {error:?}");
                    return;
                },
            };

            let image_format = ImageFormat::from_path(&output_path).unwrap_or(ImageFormat::Png);
            if let Err(error) =
                DynamicImage::ImageRgba8(image).save_with_format(output_path, image_format)
            {
                error!("Failed to save screenshot: {error}.");
            }
        });
    }

    pub(crate) fn set_pending_traversal(
        &self,
        traversal_id: TraversalId,
        sender: GenericSender<WebDriverLoadStatus>,
    ) {
        self.webdriver_senders
            .borrow_mut()
            .pending_traversals
            .insert(traversal_id, sender);
    }

    pub(crate) fn set_load_status_sender(
        &self,
        webview_id: WebViewId,
        sender: GenericSender<WebDriverLoadStatus>,
    ) {
        self.webdriver_senders
            .borrow_mut()
            .load_status_senders
            .insert(webview_id, sender);
    }

    fn remove_load_status_sender(&self, webview_id: WebViewId) {
        self.webdriver_senders
            .borrow_mut()
            .load_status_senders
            .remove(&webview_id);
    }

    fn set_script_command_interrupt_sender(
        &self,
        sender: Option<GenericSender<WebDriverJSResult>>,
    ) {
        self.webdriver_senders
            .borrow_mut()
            .script_evaluation_interrupt_sender = sender;
    }

    pub(crate) fn handle_webdriver_input_event(
        &self,
        webview_id: WebViewId,
        input_event: InputEvent,
        response_sender: Option<Sender<()>>,
    ) {
        if let Some(webview) = self.webview_by_id(webview_id) {
            let event_id = webview.notify_input_event(input_event);
            if let Some(response_sender) = response_sender {
                self.pending_webdriver_events
                    .borrow_mut()
                    .insert(event_id, response_sender);
            }
        } else {
            error!("Could not find WebView ({webview_id:?}) for WebDriver event: {input_event:?}");
        };
    }

    pub(crate) fn handle_webdriver_screenshot(
        &self,
        webview_id: WebViewId,
        rect: Option<Rect<f32, CSSPixel>>,
        result_sender: Sender<Result<RgbaImage, ScreenshotCaptureError>>,
    ) {
        if let Some(webview) = self.webview_by_id(webview_id) {
            let rect = rect.map(|rect| rect.to_box2d().into());
            webview.take_screenshot(rect, move |result| {
                if let Err(error) = result_sender.send(result) {
                    warn!("Failed to send response to TakeScreenshot: {error}");
                }
            });
        } else if let Err(error) =
            result_sender.send(Err(ScreenshotCaptureError::WebViewDoesNotExist))
        {
            error!("Failed to send response to TakeScreenshot: {error}");
        }
    }

    pub(crate) fn handle_webdriver_script_command(&self, script_command: &WebDriverScriptCommand) {
        match script_command {
            WebDriverScriptCommand::ExecuteScriptWithCallback(_webview_id, response_sender) => {
                // Give embedder a chance to interrupt the script command.
                // Webdriver only handles 1 script command at a time, so we can
                // safely set a new interrupt sender and remove the previous one here.
                self.set_script_command_interrupt_sender(Some(response_sender.clone()));
            },
            WebDriverScriptCommand::AddLoadStatusSender(webview_id, load_status_sender) => {
                self.set_load_status_sender(*webview_id, load_status_sender.clone());
            },
            WebDriverScriptCommand::RemoveLoadStatusSender(webview_id) => {
                self.remove_load_status_sender(*webview_id);
            },
            _ => {
                self.set_script_command_interrupt_sender(None);
            },
        }
    }

    pub(crate) fn handle_webdriver_load_url(
        &self,
        webview_id: WebViewId,
        url: Url,
        load_status_sender: GenericSender<WebDriverLoadStatus>,
    ) {
        let Some(webview) = self.webview_by_id(webview_id) else {
            return;
        };

        self.platform_window_for_webview_id(webview_id)
            .dismiss_embedder_controls_for_webview(webview_id);

        info!("Loading URL in webview {}: {}", webview_id, url);
        self.set_load_status_sender(webview_id, load_status_sender);
        webview.load(url);
    }

    #[cfg(all(
        feature = "gamepad",
        not(any(target_os = "android", target_env = "ohos"))
    ))]
    pub(crate) fn handle_gamepad_events(&self) {
        let Some(gamepad_delegate) = self.gamepad_delegate.as_ref() else {
            return;
        };
        let Some(active_webview) = self
            .focused_window()
            .and_then(|window| window.active_webview())
        else {
            return;
        };
        gamepad_delegate.handle_gamepad_events(active_webview);
    }

    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    pub(crate) fn handle_focused(&self, window: Rc<ServoShellWindow>) {
        *self.focused_window.borrow_mut() = Some(window);
    }

    /// Interrupt any ongoing WebDriver-based script evaluation.
    ///
    /// From <https://w3c.github.io/webdriver/#dfn-execute-a-function-body>:
    /// > The rules to execute a function body are as follows. The algorithm returns
    /// > an ECMAScript completion record.
    /// >
    /// > If at any point during the algorithm a user prompt appears, immediately return
    /// > Completion { Type: normal, Value: null, Target: empty }, but continue to run the
    /// >  other steps of this algorithm in parallel.
    fn interrupt_webdriver_script_evaluation(&self) {
        if let Some(sender) = &self
            .webdriver_senders
            .borrow()
            .script_evaluation_interrupt_sender
        {
            sender.send(Ok(JSValue::Null)).unwrap_or_else(|err| {
                info!(
                    "Notify dialog appear failed. Maybe the channel to webdriver is closed: {err}"
                );
            });
        }
    }

    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    pub(crate) fn set_accessibility_active(&self, active: bool) {
        let was_active = self.accessibility_active.replace(active);
        if active == was_active {
            return;
        }

        for window in self.windows().values() {
            for (_, webview) in window.webviews() {
                // Activate accessibility in the WebView.
                // There are two sites like this; this is the a11y activation site.
                webview.set_accessibility_active(active);
            }
        }
    }

    pub(crate) fn accessibility_active(&self) -> bool {
        self.accessibility_active.get()
    }
}

impl WebViewDelegate for RunningAppState {
    fn screen_geometry(&self, webview: WebView) -> Option<servo::ScreenGeometry> {
        Some(
            self.platform_window_for_webview_id(webview.id())
                .screen_geometry(),
        )
    }

    fn notify_status_text_changed(&self, webview: WebView, _status: Option<String>) {
        for window in self.windows_for_webview_id(webview.id()) {
            window.set_needs_update();
        }
    }

    fn notify_history_changed(&self, webview: WebView, _entries: Vec<Url>, _current: usize) {
        for window in self.windows_for_webview_id(webview.id()) {
            window.set_needs_update();
        }
    }

    fn notify_page_title_changed(&self, webview: WebView, _: Option<String>) {
        for window in self.windows_for_webview_id(webview.id()) {
            window.set_needs_update();
        }
    }

    fn notify_traversal_complete(&self, _webview: WebView, traversal_id: TraversalId) {
        let mut webdriver_state = self.webdriver_senders.borrow_mut();
        if let Entry::Occupied(entry) = webdriver_state.pending_traversals.entry(traversal_id) {
            let sender = entry.remove();
            let _ = sender.send(WebDriverLoadStatus::Complete);
        }
    }

    fn request_move_to(&self, webview: WebView, new_position: DeviceIntPoint) {
        self.platform_window_for_webview_id(webview.id())
            .set_position(new_position);
    }

    fn request_resize_to(&self, webview: WebView, requested_outer_size: DeviceIntSize) {
        self.platform_window_for_webview_id(webview.id())
            .request_resize(&webview, requested_outer_size);
    }

    fn request_authentication(
        &self,
        webview: WebView,
        authentication_request: AuthenticationRequest,
    ) {
        self.platform_window_for_webview_id(webview.id())
            .show_http_authentication_dialog(webview.id(), authentication_request);
    }

    fn request_create_new(&self, parent_webview: WebView, request: CreateNewWebViewRequest) {
        let window = self.window_for_webview_id(parent_webview.id());
        let platform_window = window.platform_window();
        let webview = request
            .builder(platform_window.rendering_context())
            .hidpi_scale_factor(platform_window.hidpi_scale_factor())
            .delegate(parent_webview.delegate())
            .build();

        webview.notify_theme_change(platform_window.theme());
        window.add_webview(webview.clone());

        // When WebDriver is enabled, do not focus and raise the WebView to the top,
        // as that is what the specification expects. Otherwise, we would like `window.open()`
        // to create a new foreground tab
        if self.servoshell_preferences.webdriver_port.get().is_none() {
            window.activate_webview(webview.id());
        } else {
            webview.hide();
        }
    }

    fn notify_closed(&self, webview: WebView) {
        self.animating_webviews.borrow_mut().remove(&webview.id());
        for window in self.windows_for_webview_id(webview.id()) {
            window.close_webview(webview.id());
        }
    }

    fn notify_input_event_handled(
        &self,
        webview: WebView,
        id: InputEventId,
        result: InputEventResult,
    ) {
        self.platform_window_for_webview_id(webview.id())
            .notify_input_event_handled(&webview, id, result);
        if let Some(response_sender) = self.pending_webdriver_events.borrow_mut().remove(&id) {
            let _ = response_sender.send(());
        }
    }

    fn notify_cursor_changed(&self, webview: WebView, cursor: servo::Cursor) {
        self.platform_window_for_webview_id(webview.id())
            .set_cursor(cursor);
    }

    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        for window in self.windows_for_webview_id(webview.id()) {
            window.set_needs_update();
        }

        if status == LoadStatus::Complete {
            if let Some(sender) = self
                .webdriver_senders
                .borrow_mut()
                .load_status_senders
                .remove(&webview.id())
            {
                let _ = sender.send(WebDriverLoadStatus::Complete);
            }
            self.maybe_request_screenshot(webview);
        }
    }

    fn notify_fullscreen_state_changed(&self, webview: WebView, fullscreen_state: bool) {
        self.platform_window_for_webview_id(webview.id())
            .set_fullscreen(fullscreen_state);
    }

    fn show_bluetooth_device_dialog(
        &self,
        webview: WebView,
        request: BluetoothDeviceSelectionRequest,
    ) {
        self.platform_window_for_webview_id(webview.id())
            .show_bluetooth_device_dialog(webview.id(), request);
    }

    fn request_permission(&self, webview: WebView, permission_request: PermissionRequest) {
        self.platform_window_for_webview_id(webview.id())
            .show_permission_dialog(webview.id(), permission_request);
    }

    fn notify_new_frame_ready(&self, webview: WebView) {
        for window in self.windows_for_webview_id(webview.id()) {
            window.set_needs_repaint();
        }
        self.event_loop_waker.wake();
    }

    fn notify_animating_changed(&self, webview: WebView, animating: bool) {
        self.animating_webviews
            .borrow_mut()
            .insert(webview.id(), animating);

        if !animating {
            return;
        }

        for window in self.windows_for_webview_id(webview.id()) {
            window.set_needs_repaint();
        }
        self.event_loop_waker.wake();
    }

    fn show_embedder_control(&self, webview: WebView, embedder_control: EmbedderControl) {
        if self.servoshell_preferences.webdriver_port.get().is_some() {
            if matches!(&embedder_control, EmbedderControl::SimpleDialog(..)) {
                self.interrupt_webdriver_script_evaluation();

                // Dialogs block the page load, so need need to notify WebDriver
                if let Some(sender) = self
                    .webdriver_senders
                    .borrow_mut()
                    .load_status_senders
                    .get(&webview.id())
                {
                    let _ = sender.send(WebDriverLoadStatus::Blocked);
                };
            }

            self.webdriver_embedder_controls
                .show_embedder_control(webview.id(), embedder_control);
            return;
        }

        self.window_for_webview_id(webview.id())
            .show_embedder_control(webview, embedder_control);
    }

    fn hide_embedder_control(&self, webview: WebView, embedder_control_id: EmbedderControlId) {
        if self.servoshell_preferences.webdriver_port.get().is_some() {
            self.webdriver_embedder_controls
                .hide_embedder_control(webview.id(), embedder_control_id);
            return;
        }

        self.window_for_webview_id(webview.id())
            .hide_embedder_control(webview, embedder_control_id);
    }

    fn notify_favicon_changed(&self, webview: WebView) {
        for window in self.windows_for_webview_id(webview.id()) {
            window.notify_favicon_changed(webview.clone());
        }
    }

    fn notify_media_session_event(&self, webview: WebView, event: MediaSessionEvent) {
        self.platform_window_for_webview_id(webview.id())
            .notify_media_session_event(event);
    }

    fn notify_crashed(&self, webview: WebView, reason: String, backtrace: Option<String>) {
        self.platform_window_for_webview_id(webview.id())
            .notify_crashed(webview, reason, backtrace);
    }

    fn show_console_message(&self, webview: WebView, level: ConsoleLogLevel, message: String) {
        self.platform_window_for_webview_id(webview.id())
            .show_console_message(level, &message);
    }

    fn notify_accessibility_tree_update(
        &self,
        webview: WebView,
        tree_update: accesskit::TreeUpdate,
    ) {
        self.platform_window_for_webview_id(webview.id())
            .notify_accessibility_tree_update(webview, tree_update);
    }
}

struct ServoShellServoDelegate;
impl ServoDelegate for ServoShellServoDelegate {
    fn notify_devtools_server_started(&self, port: u16, _token: String) {
        info!("Devtools Server running on port {port}");
    }

    fn request_devtools_connection(&self, request: AllowOrDenyRequest) {
        request.allow();
    }

    fn notify_error(&self, error: ServoError) {
        error!("Saw Servo error: {error:?}!");
    }

    fn show_console_message(&self, level: ConsoleLogLevel, message: String) {
        // For messages without a WebView context, apply platform-specific behavior
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        println!("{message}");
        log::log!(level.into(), "{message}");
    }
}

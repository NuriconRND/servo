/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Application entry point, runs the event loop.

use std::path::Path;
use std::rc::Rc;
use std::time::Instant;
use std::{env, fs};

use servo::protocol_handler::ProtocolRegistry;
use servo::{
    EventLoopWaker, Opts, Preferences, ServoBuilder, ServoUrl, UserContentManager, UserScript,
};
use url::Url;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::window::WindowId;

use log::info;

use super::event_loop::AppEvent;
use crate::desktop::event_loop::{ServoShellEventLoop, WakeCoalescer};
use crate::desktop::headed_window::HeadedWindow;
use crate::desktop::headless_window::HeadlessWindow;
use crate::desktop::protocols;
use crate::desktop::tracing::trace_winit_event;
use crate::parser::get_default_url;
use crate::prefs::ServoShellPreferences;
use crate::running_app_state::RunningAppState;
#[cfg(feature = "gamepad")]
use crate::running_app_state::ServoshellGamepadDelegate;
use crate::window::{PlatformWindow, ServoShellWindowId};

pub(crate) enum AppState {
    Initializing,
    Running(Rc<RunningAppState>),
    ShuttingDown,
}

pub struct App {
    opts: Opts,
    preferences: Preferences,
    servoshell_preferences: ServoShellPreferences,
    waker: Box<dyn EventLoopWaker>,
    /// Shared with the headed waker so we can re-arm wake-ups after pumping a
    /// [`AppEvent::Waker`]. `None` in headless mode. See [`WakeCoalescer`].
    wake_coalescer: Option<WakeCoalescer>,
    event_loop_proxy: Option<EventLoopProxy<AppEvent>>,
    initial_url: ServoUrl,
    t_start: Instant,
    t: Instant,
    state: AppState,
}

impl App {
    pub fn new(
        opts: Opts,
        preferences: Preferences,
        servo_shell_preferences: ServoShellPreferences,
        event_loop: &ServoShellEventLoop,
    ) -> Self {
        let initial_url = get_default_url(
            servo_shell_preferences.url.as_deref(),
            env::current_dir().unwrap(),
            |path| fs::metadata(path).is_ok(),
            &servo_shell_preferences,
        );

        let t = Instant::now();
        let (waker, wake_coalescer) = event_loop.create_event_loop_waker();
        App {
            opts,
            preferences,
            servoshell_preferences: servo_shell_preferences,
            waker,
            wake_coalescer,
            event_loop_proxy: event_loop.event_loop_proxy(),
            initial_url,
            t_start: t,
            t,
            state: AppState::Initializing,
        }
    }

    /// Initialize Application once event loop start running.
    pub fn init(&mut self, active_event_loop: Option<&ActiveEventLoop>) {
        // pref 를 여기서 전역으로 확정해 둔다. 원래는 `ServoBuilder::build()`(Servo::new())
        // 안에서만 `prefs::set()`이 불렸는데, 그건 이 함수 안의
        // `create_platform_window_with_preferences()`(HeadedWindow::new, 아래)보다 늦다 —
        // 그 창 생성 코드가 `gfx_vsync_enabled` pref 를 읽어야 해서 닭이 먼저냐 문제가
        // 생긴다. `self.preferences`는 이미 CLI/설정 파일 파싱으로 확정된 값이라 그대로
        // 재사용한다. 아래 `servo_builder.build()`가 같은 값으로 다시 `prefs::set()`을
        // 불러도 diff 가 비어 있어 조용히 반환된다(멱등).
        servo::prefs::set(self.preferences.clone());
        servo::config_dump::log_effective_config();

        // DComp 게이트(`gfx_dcomp_mode`) 주입 — 위 prefs::set()과 같은 이유로 여기서
        // 해야 한다: 아래 `create_platform_window_with_preferences()`(HeadedWindow::new)가
        // 이미 이 값을 보는 `RenderingContext`(창 서피스)를 만든다. env 시절엔 프로세스
        // 시작부터 늘 정해져 있었으므로 암묵적으로 이 시점보다 앞이었지만, pref로 옮긴
        // 지금은 명시적으로 순서를 맞춰야 한다. 파싱 정본은 `paint_api::rendering_context::
        // DcompMode::parse`(components/shared/paint) 한 곳뿐 — surfman 재수출이 아니다
        // (surfman은 이 정규화가 끝난 불리언만 받는다). 여기서 다시 문자열을 판정하지 않는다.
        paint_api::rendering_context::set_dcomp_mode(
            paint_api::rendering_context::DcompMode::parse(&self.preferences.gfx_dcomp_mode),
        );

        let mut protocol_registry = ProtocolRegistry::default();
        let _ = protocol_registry.register(
            "urlinfo",
            protocols::urlinfo::UrlInfoProtocolHander::default(),
        );
        let _ =
            protocol_registry.register("servo", protocols::servo::ServoProtocolHandler::default());
        let _ = protocol_registry.register(
            "resource",
            protocols::resource::ResourceProtocolHandler::default(),
        );

        let servo_builder = ServoBuilder::default()
            .opts(self.opts.clone())
            .preferences(self.preferences.clone())
            .protocol_registry(protocol_registry)
            .event_loop_waker(self.waker.clone());

        let initial_tile_indices = self.initial_wall_tile_indices();
        let url = self.initial_url.as_url().clone();
        let first_tile_index = initial_tile_indices
            .first()
            .copied()
            .unwrap_or(self.servoshell_preferences.wall_tile_index);
        let first_window_preferences = self.preferences_for_wall_tile(first_tile_index);
        let platform_window = self.create_platform_window_with_preferences(
            &first_window_preferences,
            url,
            active_event_loop,
        );

        #[cfg(feature = "webxr")]
        let servo_builder =
            servo_builder.webxr_registry(super::webxr::XrDiscoveryWebXrRegistry::new_boxed(
                platform_window.clone(),
                active_event_loop,
                &self.preferences,
            ));

        let servo = servo_builder.build();
        servo.setup_logging();
        self.log_initial_wall_tile_plan(&initial_tile_indices);

        let user_content_manager = Rc::new(UserContentManager::new(&servo));
        for script in load_userscripts(self.servoshell_preferences.userscripts_directory.as_deref())
            .expect("Loading userscripts failed")
        {
            user_content_manager.add_script(Rc::new(script));
        }

        for user_stylesheet in &self.servoshell_preferences.user_stylesheets {
            user_content_manager.add_stylesheet(user_stylesheet.clone());
        }

        let running_state = Rc::new(RunningAppState::new(
            servo,
            self.servoshell_preferences.clone(),
            self.waker.clone(),
            user_content_manager,
            self.preferences.clone(),
            #[cfg(feature = "gamepad")]
            ServoshellGamepadDelegate::maybe_new().map(Rc::new),
        ));
        let primary_window =
            running_state.open_window(platform_window, self.initial_url.as_url().clone());
        let shared_wall_webview = self
            .servoshell_preferences
            .wall_all_tiles
            .then(|| primary_window.active_webview())
            .flatten();
        for tile_index in initial_tile_indices.into_iter().skip(1) {
            info!("Opening wall tile window {tile_index}.");
            let tile_preferences = self.preferences_for_wall_tile(tile_index);
            let platform_window = self.create_platform_window_with_preferences(
                &tile_preferences,
                self.initial_url.as_url().clone(),
                active_event_loop,
            );
            if let Some(shared_wall_webview) = &shared_wall_webview {
                running_state
                    .open_window_with_shared_webview(platform_window, shared_wall_webview.clone());
            } else {
                running_state.open_window(platform_window, self.initial_url.as_url().clone());
            }
        }

        self.state = AppState::Running(running_state);
    }

    fn initial_wall_tile_indices(&self) -> Vec<usize> {
        if self.servoshell_preferences.headless || !self.servoshell_preferences.wall_all_tiles {
            return vec![self.servoshell_preferences.wall_tile_index];
        }

        self.servoshell_preferences
            .wall_layout
            .as_ref()
            .map(|layout| (0..layout.tiles.len()).collect())
            .unwrap_or_else(|| vec![self.servoshell_preferences.wall_tile_index])
    }

    fn log_initial_wall_tile_plan(&self, tile_indices: &[usize]) {
        if !self.servoshell_preferences.wall_all_tiles {
            return;
        }

        info!(
            "Wall-all-tiles startup requested: opening {} tile window(s).",
            tile_indices.len()
        );
        let Some(layout) = &self.servoshell_preferences.wall_layout else {
            return;
        };
        for tile_index in tile_indices {
            let Some(tile) = layout.tiles.get(*tile_index) else {
                continue;
            };
            info!(
                "Wall tile {} plan: display {} (auto-GPU), visible rect {:?}, render rect {:?}.",
                tile_index,
                tile.display,
                tile.rect,
                layout.tile_render_rect(*tile_index)
            );
        }
    }

    fn preferences_for_wall_tile(&self, tile_index: usize) -> ServoShellPreferences {
        let mut preferences = self.servoshell_preferences.clone();
        preferences.wall_tile_index = tile_index;
        if preferences.wall_all_tiles
            && let Some(layout) = &preferences.wall_layout
            && let Some(tile) = layout.tiles.get(tile_index)
        {
            preferences.initial_window_size = tile.rect.size.to_u32();
        }
        preferences
    }

    #[servo::servo_tracing::instrument(level = "debug", skip_all)]
    fn create_platform_window(
        &self,
        url: Url,
        active_event_loop: Option<&ActiveEventLoop>,
    ) -> Rc<dyn PlatformWindow> {
        self.create_platform_window_with_preferences(
            &self.servoshell_preferences,
            url,
            active_event_loop,
        )
    }

    #[servo::servo_tracing::instrument(level = "debug", skip_all)]
    fn create_platform_window_with_preferences(
        &self,
        servoshell_preferences: &ServoShellPreferences,
        url: Url,
        active_event_loop: Option<&ActiveEventLoop>,
    ) -> Rc<dyn PlatformWindow> {
        assert_eq!(servoshell_preferences.headless, active_event_loop.is_none());

        let Some(active_event_loop) = active_event_loop else {
            return HeadlessWindow::new(servoshell_preferences);
        };

        HeadedWindow::new(
            servoshell_preferences,
            active_event_loop,
            self.event_loop_proxy
                .clone()
                .expect("Should always have event loop proxy in headed mode."),
            url,
        )
    }

    pub fn pump_servo_event_loop(&mut self, active_event_loop: Option<&ActiveEventLoop>) -> bool {
        let AppState::Running(state) = &self.state else {
            return false;
        };

        let create_platform_window = |url: Url| self.create_platform_window(url, active_event_loop);
        if !state.spin_event_loop(Some(&create_platform_window)) {
            self.state = AppState::ShuttingDown;
            return false;
        }
        true
    }

    fn set_control_flow_for_state(&self, event_loop: &ActiveEventLoop) {
        let is_animating = match &self.state {
            AppState::Running(state) => state.is_animating(),
            _ => false,
        };
        // Tell the waker our animating state FIRST, then read `requested`. This ordering is
        // what makes `WakeCoalescer` lose no wake across the animating->idle transition.
        let control_flow = if let Some(coalescer) = &self.wake_coalescer {
            coalescer.set_animating(is_animating);
            if is_animating || coalescer.requested() {
                // When idle but a wake is owed (possibly one suppressed during animation),
                // take one more Poll iteration so `about_to_wait` pumps it.
                ControlFlow::Poll
            } else {
                ControlFlow::Wait
            }
        } else if is_animating {
            ControlFlow::Poll
        } else {
            ControlFlow::Wait
        };
        event_loop.set_control_flow(control_flow);
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.init(Some(event_loop));
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        window_event: WindowEvent,
    ) {
        let now = Instant::now();
        trace_winit_event!(
            window_event,
            "@{:?} (+{:?}) {window_event:?}",
            now - self.t_start,
            now - self.t
        );
        self.t = now;

        let AppState::Running(state) = &self.state else {
            return;
        };

        if let Some(window) = state.window(ServoShellWindowId::from(u64::from(window_id)))
            && let Some(headed_window) = window.platform_window().as_headed_window()
        {
            headed_window.handle_winit_window_event(state.clone(), window, window_event);
        }

        if !self.pump_servo_event_loop(event_loop.into()) {
            event_loop.exit();
        }
        self.set_control_flow_for_state(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, app_event: AppEvent) {
        let AppState::Running(state) = &self.state else {
            return;
        };

        if let Some(window) = app_event
            .window_id()
            .and_then(|window_id| state.window(ServoShellWindowId::from(u64::from(window_id))))
            && let Some(headed_window) = window.platform_window().as_headed_window()
        {
            headed_window.handle_winit_app_event(state.clone(), app_event);
        }

        if !self.pump_servo_event_loop(event_loop.into()) {
            event_loop.exit();
        }

        self.set_control_flow_for_state(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let is_animating = match &self.state {
            AppState::Running(state) => state.is_animating(),
            _ => false,
        };
        // Pump when animating (Poll) OR when a wake is owed. While animating the waker
        // suppresses its posts (to avoid starving input), so the owed pump is performed
        // here instead; this also pumps a wake that was suppressed right as animation ended.
        let owed = self
            .wake_coalescer
            .as_ref()
            .is_some_and(|coalescer| coalescer.take_requested());
        if (is_animating || owed) && !self.pump_servo_event_loop(event_loop.into()) {
            event_loop.exit();
            return;
        }

        self.set_control_flow_for_state(event_loop);
    }
}

fn load_userscripts(userscripts_directory: Option<&Path>) -> std::io::Result<Vec<UserScript>> {
    let mut userscripts = Vec::new();
    if let Some(userscripts_directory) = &userscripts_directory {
        let mut files = std::fs::read_dir(userscripts_directory)?
            .map(|e| e.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?;
        files.sort_unstable();
        for file in files {
            let script = std::fs::read_to_string(&file)?;
            userscripts.push(UserScript::new(script, Some(file)));
        }
    }
    Ok(userscripts)
}

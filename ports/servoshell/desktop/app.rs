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
use crate::desktop::event_loop::ServoShellEventLoop;
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
        App {
            opts,
            preferences,
            servoshell_preferences: servo_shell_preferences,
            waker: event_loop.create_event_loop_waker(),
            event_loop_proxy: event_loop.event_loop_proxy(),
            initial_url,
            t_start: t,
            t,
            state: AppState::Initializing,
        }
    }

    /// Initialize Application once event loop start running.
    pub fn init(&mut self, active_event_loop: Option<&ActiveEventLoop>) {
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
        running_state.open_window(platform_window, self.initial_url.as_url().clone());
        for tile_index in initial_tile_indices.into_iter().skip(1) {
            info!("Opening wall tile window {tile_index}.");
            let tile_preferences = self.preferences_for_wall_tile(tile_index);
            let platform_window = self.create_platform_window_with_preferences(
                &tile_preferences,
                self.initial_url.as_url().clone(),
                active_event_loop,
            );
            running_state.open_window(platform_window, self.initial_url.as_url().clone());
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
                "Wall tile {} plan: monitor {}, gpu {}, visible rect {:?}, render rect {:?}.",
                tile_index,
                tile.monitor,
                tile.gpu,
                tile.rect,
                layout.tile_render_rect(*tile_index)
            );
        }
    }

    fn preferences_for_wall_tile(&self, tile_index: usize) -> ServoShellPreferences {
        let mut preferences = self.servoshell_preferences.clone();
        preferences.wall_tile_index = tile_index;
        if preferences.wall_all_tiles &&
            let Some(layout) = &preferences.wall_layout &&
            let Some(tile) = layout.tiles.get(tile_index)
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
        assert_eq!(
            servoshell_preferences.headless,
            active_event_loop.is_none()
        );

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

        if let Some(window) = state.window(ServoShellWindowId::from(u64::from(window_id))) &&
            let Some(headed_window) = window.platform_window().as_headed_window()
        {
            headed_window.handle_winit_window_event(state.clone(), window, window_event);
        }

        if !self.pump_servo_event_loop(event_loop.into()) {
            event_loop.exit();
        }
        // Block until the window gets an event
        event_loop.set_control_flow(ControlFlow::Wait);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, app_event: AppEvent) {
        let AppState::Running(state) = &self.state else {
            return;
        };

        if let Some(window) = app_event
            .window_id()
            .and_then(|window_id| state.window(ServoShellWindowId::from(u64::from(window_id)))) &&
            let Some(headed_window) = window.platform_window().as_headed_window()
        {
            headed_window.handle_winit_app_event(state.clone(), app_event);
        }

        if !self.pump_servo_event_loop(event_loop.into()) {
            event_loop.exit();
        }

        // Block until the window gets an event
        event_loop.set_control_flow(ControlFlow::Wait);
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

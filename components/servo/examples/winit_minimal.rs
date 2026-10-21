/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;

use euclid::{Scale, Size2D};
use servo::{
    InputEvent, RenderingContext, Servo, ServoBuilder, WebView, WebViewBuilder, WheelDelta,
    WheelEvent, WheelMode, WindowRenderingContext,
};
use tracing::warn;
use url::Url;
use webrender_api::units::{DevicePixel, DevicePoint};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{MouseScrollDelta, WindowEvent};
use winit::event_loop::EventLoop;
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::Window;

fn main() -> Result<(), Box<dyn Error>> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install crypto provider");

    let event_loop = EventLoop::with_user_event()
        .build()
        .expect("Failed to create EventLoop");
    let mut app = App::new(&event_loop);
    Ok(event_loop.run_app(&mut app)?)
}

struct AppState {
    window: Window,
    servo: Servo,
    rendering_context: Rc<WindowRenderingContext>,
    webviews: RefCell<Vec<WebView>>,
}

impl ::servo::WebViewDelegate for AppState {
    fn notify_new_frame_ready(&self, _: WebView) {
        self.window.request_redraw();
    }
}

impl AppState {
    /// ★엔진 메시지를 처리하고, 그 뒤에 벽 틱을 한 번 돈다 -- 이 임베더의 유일한 생산
    /// 지점이다.★ 2026-10-22, 설계 2-4 / 규칙 1.
    ///
    /// `Servo::spin_event_loop` 은 메시지만 처리한다. 생산은 틱이 하고, 자유 실행 표출
    /// 클럭이 없는 임베더의 틱은 `WebView::on_demand_wall_tick` 이다 -- 깨우기가 세워 둔
    /// 예약 비트를 집어 가 그때 한 번 생산한다. 이 예제가 그것을 안 부르던 동안
    /// ★부트스트랩 한 장 뒤로 창이 영영 안 바뀌었다★(실측 2026-10-22: 15 초 동안
    /// `WALLBOOTSTRAP` 1 줄, 합성 0 회, 프로세스 CPU 0.01 코어 -- 그동안 스크립트는
    /// 초당 32 번 돌고 있었다).
    ///
    /// ★`Servo::spin_event_loop` 안으로 올리지 않았다.★ 그 함수는 자유 실행 클럭이 있는
    /// 셸(`winit_wall`)도 지나는데, 그쪽은 이미 매 주기 `on_wall_tick` 으로 생산한다.
    /// 거기에 이것이 얹히면 생산자가 둘이 되고(규칙 1), `wall_tick::TICK_SEQ` 의 writer 도
    /// 둘이 되어 한 에지에 번호가 두 번 올라 `ExternalPresentBudget` 의 "틱당 한 번"
    /// 천장이 깨진다(그 상수의 주석이 바로 이 증상을 적어 두고 있다). 올리려면 "이 셸에
    /// 자유 실행 클럭이 있는가" 라는 손잡이가 필요한데, 이 계획은 그런 손잡이를 없애는
    /// 중이다.
    ///
    /// ★한 번만 부른다.★ 틱은 벽 전체에 하나이고(`Paint::on_demand_wall_tick` 은 `Paint`
    /// 를 통째로 돈다) 고른 `WebView` 는 문을 여는 손잡이일 뿐이다. 여럿을 돌면 첫 호출이
    /// 비트를 가져가 나머지는 전부 `None` 이다.
    fn spin_event_loop_and_wall_tick(&self) {
        self.servo.spin_event_loop();
        // 빌림을 틱보다 먼저 끝낸다 -- 틱은 painter 를 건드리므로 빌린 채로 넘기지 않는다.
        let webview = self.webviews.borrow().last().cloned();
        if let Some(webview) = webview {
            webview.on_demand_wall_tick();
        }
    }
}

enum App {
    Initial(Waker),
    Running(Rc<AppState>),
}

impl App {
    fn new(event_loop: &EventLoop<WakerEvent>) -> Self {
        Self::Initial(Waker::new(event_loop))
    }
}

impl ApplicationHandler<WakerEvent> for App {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        if let Self::Initial(waker) = self {
            let display_handle = event_loop
                .display_handle()
                .expect("Failed to get display handle");
            let window = event_loop
                .create_window(Window::default_attributes())
                .expect("Failed to create winit Window");
            let window_handle = window.window_handle().expect("Failed to get window handle");

            let rendering_context = Rc::new(
                WindowRenderingContext::new(display_handle, window_handle, window.inner_size())
                    .expect("Could not create RenderingContext for window."),
            );

            let _ = rendering_context.make_current();

            let servo = ServoBuilder::default()
                .event_loop_waker(Box::new(waker.clone()))
                .build();
            servo.setup_logging();

            let app_state = Rc::new(AppState {
                window,
                servo,
                rendering_context,
                webviews: Default::default(),
            });

            // Make a new WebView and assign the `AppState` as the delegate.
            let url = Url::parse("https://demo.servo.org/experiments/twgl-tunnel/")
                .expect("Guaranteed by argument");

            let webview =
                WebViewBuilder::new(&app_state.servo, app_state.rendering_context.clone())
                    .url(url)
                    .hidpi_scale_factor(Scale::new(app_state.window.scale_factor() as f32))
                    .delegate(app_state.clone())
                    .build();

            app_state.webviews.borrow_mut().push(webview);
            *self = Self::Running(app_state);
        }
    }

    fn user_event(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop, _event: WakerEvent) {
        if let Self::Running(state) = self {
            state.spin_event_loop_and_wall_tick();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        if let Self::Running(state) = self {
            state.spin_event_loop_and_wall_tick();
        }

        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            },
            WindowEvent::RedrawRequested => {
                if let Self::Running(state) = self {
                    state.webviews.borrow().last().unwrap().paint();
                    state.rendering_context.present();
                }
            },
            WindowEvent::MouseWheel { delta, .. } => {
                if let Self::Running(state) = self {
                    if let Some(webview) = state.webviews.borrow().last() {
                        let (delta_x, delta_y, mode) = match delta {
                            MouseScrollDelta::LineDelta(dx, dy) => {
                                ((dx * 76.0) as f64, (dy * 76.0) as f64, WheelMode::DeltaLine)
                            },
                            MouseScrollDelta::PixelDelta(delta) => {
                                (delta.x, delta.y, WheelMode::DeltaPixel)
                            },
                        };

                        webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(
                            WheelDelta {
                                x: delta_x,
                                y: delta_y,
                                z: 0.0,
                                mode,
                            },
                            DevicePoint::default().into(),
                        )));
                    }
                }
            },
            WindowEvent::Resized(new_size) => {
                if let Self::Running(state) = self {
                    if let Some(webview) = state.webviews.borrow().last() {
                        webview.resize(new_size);
                    }
                }
            },
            _ => (),
        }
    }
}

#[derive(Clone)]
struct Waker(winit::event_loop::EventLoopProxy<WakerEvent>);
#[derive(Debug)]
struct WakerEvent;

impl Waker {
    fn new(event_loop: &EventLoop<WakerEvent>) -> Self {
        Self(event_loop.create_proxy())
    }
}

impl embedder_traits::EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn embedder_traits::EventLoopWaker> {
        Box::new(Self(self.0.clone()))
    }

    fn wake(&self) {
        if let Err(error) = self.0.send_event(WakerEvent) {
            warn!(?error, "Failed to wake event loop");
        }
    }
}

pub fn winit_size_to_euclid_size<T>(size: PhysicalSize<T>) -> Size2D<T, DevicePixel> {
    Size2D::new(size.width, size.height)
}

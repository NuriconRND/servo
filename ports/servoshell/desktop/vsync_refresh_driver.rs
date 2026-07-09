/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A vsync-locked [`RefreshDriver`] for Windows desktop windows.
//!
//! The default paint `RefreshDriver` is a free-running timer whose period is applied *after* each
//! frame finishes, so its effective rate depends on per-frame work and cannot cleanly match a
//! fixed display refresh: it overshoots (~65fps on a 60Hz display) and beats against vsync,
//! producing periodic judder even for a single video. This driver instead blocks on `DwmFlush`,
//! which returns at the next DWM composition (the display's vsync), so frame production is
//! phase-locked to the display and runs at exactly the refresh rate.
//!
//! Opt-in on Windows desktop via `SERVO_WIN_VSYNC=1`. It is not the default because under heavy
//! compositor load (many simultaneous videos) it degrades worse than the free-running timer.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use servo::RefreshDriver;

// `dwmapi!DwmFlush` blocks until the next DWM composition (i.e. the display vsync). Linked
// directly so we do not have to pull an extra `windows-sys` feature into the build.
#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmFlush() -> i32;
}

type StartFrameCallback = Box<dyn Fn() + Send + 'static>;

struct Shared {
    /// Callbacks registered for the next frame, plus a quit flag for shutdown.
    state: Mutex<(Vec<StartFrameCallback>, bool)>,
    condvar: Condvar,
}

/// A [`RefreshDriver`] that paces frame production to the Windows DWM composition clock.
pub(crate) struct DwmVsyncRefreshDriver {
    shared: Arc<Shared>,
    join_handle: Option<JoinHandle<()>>,
}

impl DwmVsyncRefreshDriver {
    pub(crate) fn new() -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new((Vec::new(), false)),
            condvar: Condvar::new(),
        });
        let thread_shared = shared.clone();
        let join_handle = thread::Builder::new()
            .name(String::from("DwmVsyncRefresh"))
            .spawn(move || vsync_loop(&thread_shared))
            .expect("Could not create DwmVsyncRefresh thread.");
        Self {
            shared,
            join_handle: Some(join_handle),
        }
    }
}

fn vsync_loop(shared: &Arc<Shared>) {
    loop {
        // Wait until at least one frame has been requested (or we are asked to quit), so we do
        // not spin on DwmFlush when Servo is idle and not animating.
        {
            let mut state = shared.state.lock().unwrap();
            while state.0.is_empty() && !state.1 {
                state = shared.condvar.wait(state).unwrap();
            }
            if state.1 {
                return;
            }
        }

        // Block until the next DWM composition (the display vsync).
        unsafe {
            DwmFlush();
        }

        // Fire every callback registered up to this vsync.
        let callbacks: Vec<StartFrameCallback> = {
            let mut state = shared.state.lock().unwrap();
            state.0.drain(..).collect()
        };
        for callback in callbacks {
            callback();
        }
    }
}

impl RefreshDriver for DwmVsyncRefreshDriver {
    fn observe_next_frame(&self, start_frame_callback: StartFrameCallback) {
        {
            let mut state = self.shared.state.lock().unwrap();
            state.0.push(start_frame_callback);
        }
        self.shared.condvar.notify_one();
    }
}

impl Drop for DwmVsyncRefreshDriver {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap();
            state.1 = true;
        }
        self.shared.condvar.notify_all();
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

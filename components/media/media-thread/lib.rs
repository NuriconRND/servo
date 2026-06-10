/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![deny(unsafe_code)]
#![allow(clippy::type_complexity)]

mod media_thread;

use std::sync::{Mutex, OnceLock};

use euclid::default::Size2D;
use ipc_channel::ipc::{IpcReceiver, IpcSender, channel};
use log::warn;
use malloc_size_of_derive::MallocSizeOf;
use paint_api::{
    ExternalImageSource, WebRenderExternalImageApi, WebRenderExternalImageHandlers,
    WebRenderExternalImageIdManager, WebRenderImageHandlerType,
};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use servo_config::{opts, pref};
pub use servo_media::player::context::{GlApi, GlContext, NativeDisplay, PlayerGLContext};
use servo_media::player::video::VideoFrame;
use webrender_api::ExternalImageId;

use crate::media_thread::GLPlayerThread;

static RAW_VIDEO_EXTERNAL_IMAGE_ID_MANAGER: OnceLock<WebRenderExternalImageIdManager> =
    OnceLock::new();

#[derive(Clone)]
struct RawVideoPlane {
    frame: VideoFrame,
    plane_index: usize,
}

fn raw_video_planes() -> &'static Mutex<FxHashMap<u64, RawVideoPlane>> {
    static RAW_VIDEO_PLANES: OnceLock<Mutex<FxHashMap<u64, RawVideoPlane>>> = OnceLock::new();
    RAW_VIDEO_PLANES.get_or_init(|| Mutex::new(FxHashMap::default()))
}

fn raw_video_external_image_id_manager() -> Option<WebRenderExternalImageIdManager> {
    RAW_VIDEO_EXTERNAL_IMAGE_ID_MANAGER.get().cloned()
}

/// Registry used by single-process WebRender external images to borrow decoded
/// YUV plane data without first copying it into Servo-owned BGRA memory.
pub struct RawVideoFrameExternalImages;

impl RawVideoFrameExternalImages {
    pub fn initialize(id_manager: WebRenderExternalImageIdManager) {
        let _ = RAW_VIDEO_EXTERNAL_IMAGE_ID_MANAGER.set(id_manager);
    }

    pub fn allocate_plane_ids(plane_count: usize) -> Option<Vec<ExternalImageId>> {
        if opts::get().multiprocess || opts::get().force_ipc {
            return None;
        }

        let mut id_manager = raw_video_external_image_id_manager()?;
        Some(
            (0..plane_count)
                .map(|_| id_manager.next_id(WebRenderImageHandlerType::Media))
                .collect(),
        )
    }

    pub fn update_plane(id: ExternalImageId, frame: VideoFrame, plane_index: usize) {
        raw_video_planes()
            .lock()
            .unwrap()
            .insert(id.0, RawVideoPlane { frame, plane_index });
    }

    pub fn remove_plane(id: ExternalImageId) {
        raw_video_planes().lock().unwrap().remove(&id.0);
        if let Some(mut id_manager) = raw_video_external_image_id_manager() {
            id_manager.remove(&id);
        }
    }

    fn frame_for_plane(id: u64) -> Option<RawVideoPlane> {
        raw_video_planes().lock().unwrap().get(&id).cloned()
    }
}

/// A global version of the [`WindowGLContext`] to be shared between the embedder and the
/// constellation. This is only okay to do because OpenGL contexts cannot be used across processes
/// anyway.
///
/// This avoid having to establish a depenency on `media` in `*_traits` crates.
static WINDOW_GL_CONTEXT: Mutex<WindowGLContext> = Mutex::new(WindowGLContext::inactive());

/// These are the messages that the GLPlayer thread will forward to
/// the video player which lives in htmlmediaelement
#[derive(Debug, Deserialize, Serialize)]
pub enum GLPlayerMsgForward {
    PlayerId(u64),
    Lock(IpcSender<(u32, Size2D<i32>, usize)>),
    Unlock(),
}

/// GLPlayer thread Message API
///
/// These are the messages that the thread will receive from the
/// constellation, the webrender::ExternalImageHandle demultiplexor
/// implementation, or a htmlmediaelement
#[derive(Debug, Deserialize, Serialize)]
pub enum GLPlayerMsg {
    /// Registers an instantiated player in DOM
    RegisterPlayer(IpcSender<GLPlayerMsgForward>),
    /// Unregisters a player's ID
    UnregisterPlayer(u64),
    /// Locks a specific texture from a player. Lock messages are used
    /// for a correct synchronization with WebRender external image
    /// API.
    ///
    /// WR locks a external texture when it wants to use the shared
    /// texture contents.
    ///
    /// The WR client should not change the shared texture content
    /// until the Unlock call.
    ///
    /// Currently OpenGL Sync Objects are used to implement the
    /// synchronization mechanism.
    Lock(u64, IpcSender<(u32, Size2D<i32>, usize)>),
    /// Unlocks a specific texture from a player. Unlock messages are
    /// used for a correct synchronization with WebRender external
    /// image API.
    ///
    /// The WR unlocks a context when it finished reading the shared
    /// texture contents.
    ///
    /// Unlock messages are always sent after a Lock message.
    Unlock(u64),
    /// Frees all resources and closes the thread.
    Exit,
}

/// A [`PlayerGLContext`] that renders to a window. Note that if the background
/// thread is not started for this context, then it is inactive (returning
/// `Unknown` values in the trait implementation).
#[derive(Clone, Debug, Deserialize, Serialize, MallocSizeOf)]
pub struct WindowGLContext {
    /// Application's GL Context
    pub context: GlContext,
    /// Application's GL Api
    pub api: GlApi,
    /// Application's native display
    pub display: NativeDisplay,
    /// A channel to the GLPlayer thread.
    pub glplayer_thread_sender: Option<IpcSender<GLPlayerMsg>>,
}

impl WindowGLContext {
    /// Create an inactive [`WindowGLContext`].
    pub const fn inactive() -> Self {
        WindowGLContext {
            context: GlContext::Unknown,
            api: GlApi::None,
            display: NativeDisplay::Unknown,
            glplayer_thread_sender: None,
        }
    }

    pub fn register(context: Self) {
        *WINDOW_GL_CONTEXT.lock().unwrap() = context;
    }

    pub fn get() -> Self {
        WINDOW_GL_CONTEXT.lock().unwrap().clone()
    }

    /// Sends an exit message to close the GLPlayerThread.
    pub fn exit(&self) {
        self.send(GLPlayerMsg::Exit);
    }

    #[inline]
    pub fn send(&self, message: GLPlayerMsg) {
        // Don't do anything if GL accelerated playback is disabled.
        let Some(sender) = self.glplayer_thread_sender.as_ref() else {
            return;
        };

        if let Err(error) = sender.send(message) {
            warn!("Could no longer communicate with GL accelerated media threads: {error}")
        }
    }

    pub fn initialize(display: NativeDisplay, api: GlApi, context: GlContext) {
        if matches!(display, NativeDisplay::Unknown) || matches!(context, GlContext::Unknown) {
            return;
        }

        let mut window_gl_context = WINDOW_GL_CONTEXT.lock().unwrap();
        if window_gl_context.glplayer_thread_sender.is_some() {
            warn!("Not going to initialize GL accelerated media playback more than once.");
            return;
        }

        window_gl_context.context = context;
        window_gl_context.display = display;
        window_gl_context.api = api;
    }

    pub fn initialize_image_handler(external_image_handlers: &mut WebRenderExternalImageHandlers) {
        RawVideoFrameExternalImages::initialize(external_image_handlers.id_manager());

        let mut window_gl_context = WINDOW_GL_CONTEXT.lock().unwrap();

        let thread_sender = if pref!(media_glvideo_enabled) {
            if matches!(window_gl_context.display, NativeDisplay::Unknown)
                || matches!(window_gl_context.context, GlContext::Unknown)
            {
                None
            } else if let Some(thread_sender) = window_gl_context.glplayer_thread_sender.clone() {
                Some(thread_sender)
            } else {
                let thread_sender = GLPlayerThread::start(external_image_handlers.id_manager());
                window_gl_context.glplayer_thread_sender = Some(thread_sender.clone());
                Some(thread_sender)
            }
        } else {
            None
        };

        let image_handler = Box::new(MediaExternalImages::new(thread_sender));
        external_image_handlers.set_handler(image_handler, WebRenderImageHandlerType::Media);
    }
}

impl PlayerGLContext for WindowGLContext {
    fn get_gl_context(&self) -> GlContext {
        match self.glplayer_thread_sender {
            Some(..) => self.context.clone(),
            None => GlContext::Unknown,
        }
    }

    fn get_native_display(&self) -> NativeDisplay {
        match self.glplayer_thread_sender {
            Some(..) => self.display.clone(),
            None => NativeDisplay::Unknown,
        }
    }

    fn get_gl_api(&self) -> GlApi {
        self.api.clone()
    }
}

/// Bridge between the webrender::ExternalImage callbacks and the
/// GLPlayerThreads.
struct GLPlayerExternalImages {
    // @FIXME(victor): this should be added when GstGLSyncMeta is
    // added
    // webrender_gl: Rc<dyn gl::Gl>,
    glplayer_channel: IpcSender<GLPlayerMsg>,
    // Used to avoid creating a new channel on each received WebRender
    // request.
    lock_channel: (
        IpcSender<(u32, Size2D<i32>, usize)>,
        IpcReceiver<(u32, Size2D<i32>, usize)>,
    ),
}

impl GLPlayerExternalImages {
    fn new(sender: IpcSender<GLPlayerMsg>) -> Self {
        Self {
            glplayer_channel: sender,
            lock_channel: channel().unwrap(),
        }
    }
}

impl WebRenderExternalImageApi for GLPlayerExternalImages {
    fn lock(&mut self, id: u64) -> (ExternalImageSource<'_>, Size2D<i32>) {
        // The GLPlayerMsgForward::Lock message inserts a fence in the
        // GLPlayer command queue.
        self.glplayer_channel
            .send(GLPlayerMsg::Lock(id, self.lock_channel.0.clone()))
            .unwrap();
        let (image_id, size, _gl_sync) = self.lock_channel.1.recv().unwrap();
        // The next glWaitSync call is run on the WR thread and it's
        // used to synchronize the two flows of OpenGL commands in
        // order to avoid WR using a semi-ready GLPlayer texture.
        // glWaitSync doesn't block WR thread, it affects only
        // internal OpenGL subsystem.
        // self.webrender_gl
        //    .wait_sync(gl_sync as gl::GLsync, 0, gl::TIMEOUT_IGNORED);
        (ExternalImageSource::NativeTexture(image_id), size)
    }

    fn unlock(&mut self, id: u64) {
        self.glplayer_channel.send(GLPlayerMsg::Unlock(id)).unwrap();
    }
}

/// Bridge between WebRender external image callbacks and media-backed images.
///
/// Raw YUV planes are handled in-process through [`RawVideoFrameExternalImages`].
/// GL texture-backed media keeps using [`GLPlayerExternalImages`].
struct MediaExternalImages {
    glplayer_images: Option<GLPlayerExternalImages>,
    locked_raw_planes: FxHashMap<u64, RawVideoPlane>,
}

impl MediaExternalImages {
    fn new(glplayer_sender: Option<IpcSender<GLPlayerMsg>>) -> Self {
        Self {
            glplayer_images: glplayer_sender.map(GLPlayerExternalImages::new),
            locked_raw_planes: Default::default(),
        }
    }
}

impl WebRenderExternalImageApi for MediaExternalImages {
    fn lock(&mut self, id: u64) -> (ExternalImageSource<'_>, Size2D<i32>) {
        if let Some(raw_plane) = RawVideoFrameExternalImages::frame_for_plane(id) {
            self.locked_raw_planes.insert(id, raw_plane);
            let raw_plane = self
                .locked_raw_planes
                .get(&id)
                .expect("Raw media plane should be locked");
            let Some(yuv_data) = raw_plane.frame.get_yuv_data() else {
                return (ExternalImageSource::Invalid, Size2D::zero());
            };
            let Some(plane) = yuv_data.plane(raw_plane.plane_index) else {
                return (ExternalImageSource::Invalid, Size2D::zero());
            };
            let Some(data) = raw_plane.frame.get_plane_data(raw_plane.plane_index) else {
                return (ExternalImageSource::Invalid, Size2D::zero());
            };
            return (
                ExternalImageSource::RawData(data),
                Size2D::new(plane.width, plane.height),
            );
        }

        self.glplayer_images
            .as_mut()
            .map(|glplayer_images| glplayer_images.lock(id))
            .unwrap_or((ExternalImageSource::Invalid, Size2D::zero()))
    }

    fn unlock(&mut self, id: u64) {
        if self.locked_raw_planes.remove(&id).is_some() {
            return;
        }

        if let Some(glplayer_images) = self.glplayer_images.as_mut() {
            glplayer_images.unlock(id);
        }
    }
}

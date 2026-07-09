/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![deny(unsafe_code)]
#![allow(clippy::type_complexity)]

mod media_thread;

use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use euclid::default::Size2D;
use ipc_channel::ipc::{IpcReceiver, IpcSender, channel};
use log::{debug, info, warn};
use malloc_size_of_derive::MallocSizeOf;
use paint_api::rendering_context::{RenderingContext, SurfaceTexture};
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
static RAW_VIDEO_PLANE_LOCK_COUNT: AtomicU64 = AtomicU64::new(0);
static RAW_VIDEO_PLANE_UNLOCK_COUNT: AtomicU64 = AtomicU64::new(0);

const RAW_VIDEO_PLANE_INFO_INTERVAL: u64 = 360;

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
        // Take the old plane out of the map but DROP IT OUTSIDE the mutex. Dropping the
        // previous VideoFrame unrefs a gst buffer, which can block on the decoder's buffer
        // pool lock (notably during a flushing loop-restart seek). Doing that while holding
        // this global map mutex stalls the renderer thread, which takes the same mutex in
        // `frame_for_plane` for every plane upload (one flushing video would freeze the
        // uploads of ALL videos for the duration of the flush).
        let lock_start = std::time::Instant::now();
        let mut planes = raw_video_planes().lock().unwrap();
        let lock_wait_ms = lock_start.elapsed().as_secs_f64() * 1000.0;
        let old_plane = planes.insert(id.0, RawVideoPlane { frame, plane_index });
        drop(planes);
        let drop_start = std::time::Instant::now();
        drop(old_plane);
        let drop_ms = drop_start.elapsed().as_secs_f64() * 1000.0;
        if lock_wait_ms > 10.0 || drop_ms > 10.0 {
            warn!(
                "Slow raw plane update: id={} lock_wait_ms={:.1} old_frame_drop_ms={:.1}",
                id.0, lock_wait_ms, drop_ms,
            );
        }
    }

    pub fn remove_plane(id: ExternalImageId) {
        // As in `update_plane`, drop the removed plane outside the mutex.
        let removed_plane = raw_video_planes().lock().unwrap().remove(&id.0);
        drop(removed_plane);
        if let Some(mut id_manager) = raw_video_external_image_id_manager() {
            id_manager.remove(&id);
        }
    }

    fn frame_for_plane(id: u64) -> Option<RawVideoPlane> {
        let lock_start = std::time::Instant::now();
        let planes = raw_video_planes().lock().unwrap();
        let lock_wait_ms = lock_start.elapsed().as_secs_f64() * 1000.0;
        if lock_wait_ms > 10.0 {
            warn!("Slow raw plane read lock: id={} lock_wait_ms={:.1}", id, lock_wait_ms);
        }
        planes.get(&id).cloned()
    }
}

/// D3D11 GPU 상주 비디오 프레임 레지스트리 (raw YUV 레지스트리의 대칭물).
/// external image ID → 최신 프레임 (latest-wins). 값은 스칼라뿐이라 gst 참조 보유 없음
/// — 프레임 수명은 render-d3d11의 공유 링이 소유한다.
#[derive(Clone, Copy, Debug)]
pub struct D3d11VideoFrameInfo {
    pub shared_handle: u64,
    pub ring_epoch: u32,
    pub width: i32,
    pub height: i32,
}

fn d3d11_video_frames() -> &'static Mutex<FxHashMap<u64, D3d11VideoFrameInfo>> {
    static D3D11_VIDEO_FRAMES: OnceLock<Mutex<FxHashMap<u64, D3d11VideoFrameInfo>>> =
        OnceLock::new();
    D3D11_VIDEO_FRAMES.get_or_init(|| Mutex::new(FxHashMap::default()))
}

fn d3d11_removed_ids() -> &'static Mutex<Vec<u64>> {
    static D3D11_REMOVED_IDS: OnceLock<Mutex<Vec<u64>>> = OnceLock::new();
    D3D11_REMOVED_IDS.get_or_init(|| Mutex::new(Vec::new()))
}

pub struct D3d11VideoFrameExternalImages;

impl D3d11VideoFrameExternalImages {
    pub fn allocate_id() -> Option<ExternalImageId> {
        if opts::get().multiprocess || opts::get().force_ipc {
            return None;
        }
        let mut id_manager = raw_video_external_image_id_manager()?;
        Some(id_manager.next_id(WebRenderImageHandlerType::Media))
    }

    pub fn update(id: ExternalImageId, info: D3d11VideoFrameInfo) {
        d3d11_video_frames().lock().unwrap().insert(id.0, info);
    }

    pub fn remove(id: ExternalImageId) {
        d3d11_video_frames().lock().unwrap().remove(&id.0);
        // 렌더러 스레드의 래핑 캐시는 다음 lock 때 이 목록을 보고 정리한다.
        d3d11_removed_ids().lock().unwrap().push(id.0);
        if let Some(mut id_manager) = raw_video_external_image_id_manager() {
            id_manager.remove(&id);
        }
    }

    fn info_for(id: u64) -> Option<D3d11VideoFrameInfo> {
        d3d11_video_frames().lock().unwrap().get(&id).copied()
    }

    fn take_removed_ids() -> Vec<u64> {
        std::mem::take(&mut *d3d11_removed_ids().lock().unwrap())
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

    pub fn initialize_image_handler(
        external_image_handlers: &mut WebRenderExternalImageHandlers,
        rendering_context: Rc<dyn RenderingContext>,
    ) {
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

        let image_handler = Box::new(MediaExternalImages::new(
            thread_sender,
            Some(rendering_context),
        ));
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

#[derive(Default)]
struct D3d11TextureCacheEntry {
    ring_epoch: u32,
    /// shared_handle → (SurfaceTexture 유지용, GL 텍스처 id). 링 슬롯이 안정적이라
    /// 플레이어당 최대 4개 — 정상 상태에서 프레임당 재래핑 0.
    textures: FxHashMap<u64, (SurfaceTexture, u32)>,
}

/// Bridge between WebRender external image callbacks and media-backed images.
///
/// Raw YUV planes are handled in-process through [`RawVideoFrameExternalImages`].
/// GL texture-backed media keeps using [`GLPlayerExternalImages`].
struct MediaExternalImages {
    glplayer_images: Option<GLPlayerExternalImages>,
    locked_raw_planes: FxHashMap<u64, RawVideoPlane>,
    rendering_context: Option<Rc<dyn RenderingContext>>,
    d3d11_texture_cache: FxHashMap<u64, D3d11TextureCacheEntry>,
}

impl MediaExternalImages {
    fn new(
        glplayer_sender: Option<IpcSender<GLPlayerMsg>>,
        rendering_context: Option<Rc<dyn RenderingContext>>,
    ) -> Self {
        Self {
            glplayer_images: glplayer_sender.map(GLPlayerExternalImages::new),
            locked_raw_planes: Default::default(),
            rendering_context,
            d3d11_texture_cache: Default::default(),
        }
    }

    fn purge_removed_d3d11_entries(&mut self) {
        for removed in D3d11VideoFrameExternalImages::take_removed_ids() {
            if let Some(entry) = self.d3d11_texture_cache.remove(&removed) {
                if let Some(rendering_context) = self.rendering_context.as_ref() {
                    for (_, (surface_texture, _)) in entry.textures {
                        rendering_context.destroy_texture(surface_texture);
                    }
                }
            }
        }
    }

    fn lock_d3d11(
        &mut self,
        id: u64,
        info: D3d11VideoFrameInfo,
    ) -> (ExternalImageSource<'_>, Size2D<i32>) {
        let Some(rendering_context) = self.rendering_context.as_ref() else {
            return (ExternalImageSource::Invalid, Size2D::zero());
        };
        let entry = self.d3d11_texture_cache.entry(id).or_default();
        if entry.ring_epoch != info.ring_epoch {
            // 링 재생성(크기 변경) — 이전 세대 래핑 전부 폐기
            for (_, (surface_texture, _)) in std::mem::take(&mut entry.textures) {
                rendering_context.destroy_texture(surface_texture);
            }
            entry.ring_epoch = info.ring_epoch;
        }
        if !entry.textures.contains_key(&info.shared_handle) {
            let size = Size2D::new(info.width, info.height);
            match rendering_context.create_texture_from_shared_handle(info.shared_handle, size) {
                Some((surface_texture, gl_texture, _)) => {
                    entry.textures.insert(info.shared_handle, (surface_texture, gl_texture));
                },
                None => {
                    warn!("D3D11 video: 공유 핸들 import 실패 (id={id})");
                    return (ExternalImageSource::Invalid, Size2D::zero());
                },
            }
        }
        let (_, gl_texture) = entry.textures[&info.shared_handle];
        (
            ExternalImageSource::NativeTexture(gl_texture),
            Size2D::new(info.width, info.height),
        )
    }
}

impl Drop for MediaExternalImages {
    fn drop(&mut self) {
        // 캐시된 SurfaceTexture는 destroy_surface_texture 없이 drop되면 surfman이
        // 패닉하므로(teardown 안전장치), 핸들러 해체 시 전부 명시 파기한다.
        // rendering_context가 None이면 lock_d3d11이 항상 Invalid만 반환해 캐시가
        // 늘 비어 있으므로 그냥 반환해도 안전하다.
        let Some(rendering_context) = self.rendering_context.as_ref() else {
            return;
        };
        for (_, entry) in self.d3d11_texture_cache.drain() {
            for (_, (surface_texture, _)) in entry.textures {
                rendering_context.destroy_texture(surface_texture);
            }
        }
    }
}

impl WebRenderExternalImageApi for MediaExternalImages {
    fn lock(&mut self, id: u64) -> (ExternalImageSource<'_>, Size2D<i32>) {
        // GPU 상주 D3D11 프레임: 렌더러는 캐시된 GL 텍스처를 돌려줄 뿐 업로드하지 않는다.
        if let Some(info) = D3d11VideoFrameExternalImages::info_for(id) {
            self.purge_removed_d3d11_entries();
            return self.lock_d3d11(id, info);
        }

        // Diagnostic: this runs on the renderer thread once per plane per upload. Time the
        // full body (map lookup + gst buffer map via get_yuv_data/get_plane_data) to catch
        // stalls caused by gst-side locks (e.g. around a flushing loop-restart seek).
        let lock_body_start = std::time::Instant::now();
        if let Some(raw_plane) = RawVideoFrameExternalImages::frame_for_plane(id) {
            // Replacing a previously-locked plane drops its VideoFrame here on the renderer
            // thread; time it together with the rest of the body.
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
            let lock_count = RAW_VIDEO_PLANE_LOCK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            debug!(
                "Wall raw video plane lock: external_id={} plane_index={} size={}x{} \
                 bytes={} total_locks={}",
                id,
                raw_plane.plane_index,
                plane.width,
                plane.height,
                data.len(),
                lock_count,
            );
            if lock_count <= 6 || lock_count % RAW_VIDEO_PLANE_INFO_INTERVAL == 0 {
                info!(
                    "Wall raw video plane lock summary: total_locks={} external_id={} \
                     plane_index={} size={}x{} bytes={}",
                    lock_count,
                    id,
                    raw_plane.plane_index,
                    plane.width,
                    plane.height,
                    data.len(),
                );
            }
            let lock_body_ms = lock_body_start.elapsed().as_secs_f64() * 1000.0;
            if lock_body_ms > 10.0 {
                warn!(
                    "Slow raw plane lock body: external_id={} plane_index={} ms={:.1}",
                    id, raw_plane.plane_index, lock_body_ms,
                );
            }
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
        // D3D11 캐시는 unlock에서 유지한다 (링 슬롯 재사용 — 폐기는 epoch 변경/제거 시).
        if self.d3d11_texture_cache.contains_key(&id) {
            return;
        }

        // Diagnostic: dropping the locked plane here unrefs a gst buffer ON THE RENDERER
        // THREAD; if the frame was replaced meanwhile this may be the last ref and the
        // buffer returns to the decoder's pool, which can block during a flushing seek.
        let unlock_start = std::time::Instant::now();
        if let Some(raw_plane) = self.locked_raw_planes.remove(&id) {
            let plane_index = raw_plane.plane_index;
            drop(raw_plane);
            let unlock_ms = unlock_start.elapsed().as_secs_f64() * 1000.0;
            if unlock_ms > 10.0 {
                warn!(
                    "Slow raw plane unlock: external_id={} plane_index={} ms={:.1}",
                    id, plane_index, unlock_ms,
                );
            }
            let unlock_count = RAW_VIDEO_PLANE_UNLOCK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            debug!(
                "Wall raw video plane unlock: external_id={} plane_index={} total_unlocks={}",
                id, plane_index, unlock_count,
            );
            if unlock_count <= 6 || unlock_count % RAW_VIDEO_PLANE_INFO_INTERVAL == 0 {
                info!(
                    "Wall raw video plane unlock summary: total_unlocks={} external_id={} \
                     plane_index={}",
                    unlock_count, id, plane_index,
                );
            }
            return;
        }

        if let Some(glplayer_images) = self.glplayer_images.as_mut() {
            glplayer_images.unlock(id);
        }
    }
}

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
use paint_api::rendering_context::{D3d11GlWrappedTexture, RenderingContext};
use paint_api::{
    ExternalImageSource, WebRenderExternalImageApi, WebRenderExternalImageHandlers,
    WebRenderExternalImageIdManager, WebRenderImageHandlerType,
};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use servo_config::debug_env;
use servo_config::{opts, pref};
pub use servo_media::player::context::{GlApi, GlContext, NativeDisplay, PlayerGLContext};
use servo_media::player::d3d11_ring::{
    ConsumeCommit, ConsumePlan, D3d11PlaneRings, MAX_PLANES, PlaneDesc, RemappedPlane,
};
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

/// D3D11 plane 바인딩 — external image ID 하나를 공유 plane 링의 한 plane에
/// 연결한다(WR YUV 직접 샘플 경로). 값은 스칼라뿐이라 gst 참조를 보유하지
/// 않는다 — plane 텍스처의 수명은 [`D3d11PlaneRings`] 레지스트리가 소유하고
/// 렌더러 스레드가 링 제거 시 해제한다. 필드 이름/타입은 프로듀서(Task 7)가
/// 그대로 작성하므로 고정이다.
#[derive(Clone, Copy, Debug)]
pub struct D3d11PlaneBinding {
    pub ring_id: u64,
    pub ring_epoch: u32,
    pub plane_index: usize,
    pub width: i32,
    pub height: i32,
    /// DComp external surface interop(Task 5)이 lease를 통해 소비하는
    /// 색정보 — WR YUV 직접 샘플 경로가 쓰는 것과 동일한 원천에서 채워진다
    /// (htmlmediaelement.rs `render_d3d11_yuv_frame`의 `VideoFrameD3D11YuvData`).
    pub yuv_format: paint_api::VideoLeaseFormat,
    pub color_space: paint_api::VideoLeaseColorSpace,
    pub color_range: paint_api::VideoLeaseColorRange,
}

/// external image ID(u64) → 최신 plane 바인딩(latest-wins). 프로듀서가
/// `update_plane`으로 쓰고, 렌더러 스레드의 `lock`/`unlock`이 읽는다.
fn d3d11_plane_bindings() -> &'static Mutex<FxHashMap<u64, D3d11PlaneBinding>> {
    static D3D11_PLANE_BINDINGS: OnceLock<Mutex<FxHashMap<u64, D3d11PlaneBinding>>> =
        OnceLock::new();
    D3D11_PLANE_BINDINGS.get_or_init(|| Mutex::new(FxHashMap::default()))
}

pub struct D3d11VideoFrameExternalImages;

impl D3d11VideoFrameExternalImages {
    /// plane 개수만큼 external image ID를 할당한다(raw YUV 경로의
    /// `allocate_plane_ids` 미러). 멀티프로세스/강제 IPC에서는 in-process
    /// 외부 이미지 경로가 없으므로 None.
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

    /// plane 바인딩을 갱신한다(프레임마다 프로듀서가 호출).
    pub fn update_plane(id: ExternalImageId, binding: D3d11PlaneBinding) {
        d3d11_plane_bindings().lock().unwrap().insert(id.0, binding);
    }

    /// plane 바인딩을 제거한다(플레이어 리셋/삭제 시). 텍스처/링 정리는
    /// 레지스트리의 `take_removed_rings`가 담당하므로 여기서는 바인딩 맵과
    /// id_manager 슬롯만 정리한다.
    pub fn remove_plane(id: ExternalImageId) {
        d3d11_plane_bindings().lock().unwrap().remove(&id.0);
        if let Some(mut id_manager) = raw_video_external_image_id_manager() {
            id_manager.remove(&id);
        }
    }

    fn binding_for(id: u64) -> Option<D3d11PlaneBinding> {
        d3d11_plane_bindings().lock().unwrap().get(&id).copied()
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

        paint_api::set_video_external_surface_provider(std::sync::Arc::new(
            MediaVideoExternalSurfaceProvider,
        ));
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

/// Execute a [`ConsumePlan`] on the renderer thread and commit the result.
///
/// Runs on the renderer (ANGLE GL) thread, keyed off the 0->1 plane-lock
/// transition (so exactly once per composite per ring). Every path through
/// this function ends in exactly one [`D3d11PlaneRings::commit_consume`],
/// even when some or all D3D11 Map calls fail — an empty `mapped`/`remapped`
/// vector is a valid commit and is what keeps the ring from wedging (see the
/// never-skip contract on [`ConsumePlan`]).
fn consume_plan(rc: &dyn RenderingContext, ring_id: u64, plan: ConsumePlan) {
    match plan {
        ConsumePlan::InitialMapAll { slots, staged } => {
            // Map ALL slots' planes. Slot 0 is special: after copying the staged
            // first-frame bytes into it we Unmap it again and EXCLUDE it from the
            // commit's `mapped` vec, so slot 0 is committed as Presenting=unmapped
            // (D3D11 leaves sampling of a still-mapped resource undefined). The
            // first Advance then re-Maps slot 0 legitimately. Slots 1..N stay
            // mapped and are committed Free.
            let mut mapped: Vec<RemappedPlane> = Vec::new();
            let mut slot0_mapped: [Option<(usize, u32, PlaneDesc)>; MAX_PLANES] = [None; MAX_PLANES];
            for (slot_idx, slot) in slots.iter().enumerate() {
                for (plane_idx, plane) in slot.iter().enumerate() {
                    let Some(desc) = plane else {
                        continue;
                    };
                    match rc.map_d3d11_dynamic_texture(desc.texture) {
                        Some((data_ptr, row_pitch)) => {
                            if slot_idx == 0 {
                                // Remember for the staged copy + Unmap below; do NOT
                                // publish slot 0's pointers to the registry.
                                slot0_mapped[plane_idx] = Some((data_ptr, row_pitch, *desc));
                            } else {
                                mapped.push(RemappedPlane {
                                    texture: desc.texture,
                                    data_ptr,
                                    row_pitch,
                                });
                            }
                        },
                        None => {
                            warn!(
                                "D3D11 media: InitialMapAll map failed (ring={ring_id} \
                                 texture={})",
                                desc.texture
                            );
                        },
                    }
                }
            }
            // Copy staged first-frame bytes into slot 0. Staged vecs are tightly
            // packed, so the source row stride equals `row_bytes`.
            if let Some(staged) = staged {
                for (plane_idx, src) in staged.iter().enumerate() {
                    if plane_idx >= MAX_PLANES {
                        break;
                    }
                    if let Some((data_ptr, row_pitch, desc)) = slot0_mapped[plane_idx] {
                        let rows = desc.height.max(0) as usize;
                        rc.copy_rows_to_mapped(data_ptr, row_pitch, src, desc.row_bytes, rows);
                    }
                }
            }
            // Unmap slot 0 so it genuinely matches Presenting=unmapped. The commit
            // below transitions slot 0 -> Presenting; sampling it while mapped is
            // undefined, and the first Advance's re-Map of a still-mapped slot 0
            // would fail (and ride the failed-remap path). Slot 0 is excluded from
            // `mapped` above, so the registry records it unmapped too.
            for slot0_plane in slot0_mapped.iter().flatten() {
                let (_, _, desc) = slot0_plane;
                rc.unmap_d3d11_texture(desc.texture);
            }
            D3d11PlaneRings::commit_consume(ring_id, ConsumeCommit::InitialMapAll { mapped });
        },
        ConsumePlan::Advance {
            unmap,
            map,
            filled_slot,
        } => {
            for texture in unmap {
                rc.unmap_d3d11_texture(texture);
            }
            let mut remapped: Vec<RemappedPlane> = Vec::new();
            for texture in map {
                match rc.map_d3d11_dynamic_texture(texture) {
                    Some((data_ptr, row_pitch)) => remapped.push(RemappedPlane {
                        texture,
                        data_ptr,
                        row_pitch,
                    }),
                    None => warn!("D3D11 media: Advance re-map failed (ring={ring_id} texture={texture})"),
                }
            }
            D3d11PlaneRings::commit_consume(
                ring_id,
                ConsumeCommit::Advance {
                    filled_slot,
                    remapped,
                },
            );
        },
    }
}

/// DComp external surface 경로가 렌더러 스레드에서 plane 링을 직접 소비하기
/// 위한 provider. `lock_d3d11`(위 `MediaExternalImages::lock_d3d11`)과 동일한
/// 링 잠금 규율(0→1 소비 계획, 짝맞춤 unlock)을 그대로 쓴다 — `consume_plan`을
/// 직접 재사용하므로 두 소비 경로가 상태기계 규약을 벗어나지 않는다.
pub struct MediaVideoExternalSurfaceProvider;

impl paint_api::VideoExternalSurfaceProvider for MediaVideoExternalSurfaceProvider {
    fn acquire(
        &self,
        rc: &dyn RenderingContext,
        external_id: u64,
    ) -> Option<paint_api::VideoFrameLease> {
        let binding = D3d11VideoFrameExternalImages::binding_for(external_id)?;

        // 합성당 1회 소비: lock_d3d11과 동일하게 링별 lock_count 0→1 전이에서만
        // plan이 나오고, Some(plan)은 반드시 consume_plan이 commit까지 끝낸다.
        if let Some(plan) = D3d11PlaneRings::note_plane_lock_and_plan(binding.ring_id) {
            consume_plan(rc, binding.ring_id, plan);
        }

        let plane_count = match D3d11PlaneRings::plane_count(binding.ring_id) {
            Some(n) => n,
            None => {
                D3d11PlaneRings::note_plane_unlock(binding.ring_id);
                return None;
            },
        };

        let mut planes: [Option<paint_api::VideoLeasePlane>; 3] = [None; 3];
        for i in 0..plane_count {
            match D3d11PlaneRings::presenting_plane(binding.ring_id, i) {
                Some(p) => {
                    planes[i] = Some(paint_api::VideoLeasePlane {
                        texture: p.texture,
                        width: p.width,
                        height: p.height,
                    })
                },
                None => {
                    D3d11PlaneRings::note_plane_unlock(binding.ring_id);
                    return None;
                },
            }
        }

        let frame_seq = match D3d11PlaneRings::presenting_filled_seq(binding.ring_id) {
            Some(s) => s,
            None => {
                D3d11PlaneRings::note_plane_unlock(binding.ring_id);
                return None;
            },
        };

        Some(paint_api::VideoFrameLease {
            ring_id: binding.ring_id,
            planes,
            plane_count,
            format: binding.yuv_format,
            color_space: binding.color_space,
            color_range: binding.color_range,
            frame_seq,
        })
    }

    fn release(&self, _rc: &dyn RenderingContext, ring_id: u64) {
        D3d11PlaneRings::note_plane_unlock(ring_id);
    }
}

// D3D11PROF: 임시 진단 계측 (시작 램프 조사 §12, env SERVO_D3D11_PROFILE=1 게이트,
// 기본 off, 조사 종료 후 제거 예정). 렌더러 스레드의 plane 텍스처 첫-lock 래핑
// (EGLImage 바인딩)이 램프 구간 프레임 예산을 얼마나 먹는지 정량화한다. 래핑은
// 타일당 링 슬롯 4개 × plane 2~3개의 일회성 이벤트라 매 건 로깅해도 로그 폭주 없음.
fn d3d11_profile_enabled() -> bool {
    // servo_config::debug_env가 프로세스 1회 판정을 캐시한다. 이 판정식은
    // render-d3d11/lib.rs의 profile_enabled()에도 동일하게 존재하며, 2026-08-11
    // 조사로 두 판정식이 문자 그대로 같음을 확인했다("1"/"true"/"on", 대소문자 무시).
    debug_env::string(&debug_env::D3D11_PROFILE)
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on"))
}
// D3D11PROF: import(래핑) 누적 횟수/누적 소요(µs) — 로그에 running total로 포함.
static D3D11_IMPORT_COUNT: AtomicU64 = AtomicU64::new(0);
static D3D11_IMPORT_TOTAL_US: AtomicU64 = AtomicU64::new(0);

/// Bridge between WebRender external image callbacks and media-backed images.
///
/// Raw YUV planes are handled in-process through [`RawVideoFrameExternalImages`].
/// GL texture-backed media keeps using [`GLPlayerExternalImages`].
struct MediaExternalImages {
    glplayer_images: Option<GLPlayerExternalImages>,
    locked_raw_planes: FxHashMap<u64, RawVideoPlane>,
    rendering_context: Option<Rc<dyn RenderingContext>>,
    /// EGLImage 래핑 캐시: plane 텍스처(usize) → GL 래핑. 링 슬롯 텍스처가
    /// 안정적이라 링당 최대 SLOT_COUNT×MAX_PLANES개 — 정상 상태에서 프레임당
    /// 재래핑 0. 링 제거 시 `take_removed_rings` 기반으로 파기한다.
    d3d11_wrap_cache: FxHashMap<usize, D3d11GlWrappedTexture>,
    /// 이번 합성에서 lock한 D3D11 plane external id → ring_id. `unlock`이
    /// `note_plane_unlock`을 정확히 짝맞춰 호출하기 위한 로컬 추적(글로벌
    /// 바인딩 맵이 lock~unlock 사이에 변경돼도 안전하도록 self에 보관).
    locked_d3d11_planes: FxHashMap<u64, u64>,
}

impl MediaExternalImages {
    fn new(
        glplayer_sender: Option<IpcSender<GLPlayerMsg>>,
        rendering_context: Option<Rc<dyn RenderingContext>>,
    ) -> Self {
        // 소비자(ANGLE) 디바이스를 레지스트리에 publish한다. 이 값이 있어야
        // 프로듀서가 plane 링을 만들어 프레임을 채운다 — publish 전에는
        // 프로듀서가 모든 프레임을 드롭한다(WR YUV 직접 샘플 경로의 on-switch).
        if let Some(rc) = rendering_context.as_ref() {
            if let Some(device) = rc.media_d3d11_device_handle() {
                D3d11PlaneRings::set_consumer_device(device);
            }
        }
        Self {
            glplayer_images: glplayer_sender.map(GLPlayerExternalImages::new),
            locked_raw_planes: Default::default(),
            rendering_context,
            d3d11_wrap_cache: Default::default(),
            locked_d3d11_planes: Default::default(),
        }
    }

    /// 제거된 링들을 정리한다: `mapped` 텍스처 Unmap(Presenting은 레지스트리가
    /// 이미 제외) → 캐시된 GL 래핑 파기 → 텍스처 Release.
    fn purge_removed_d3d11_entries(&mut self) {
        let removed = D3d11PlaneRings::take_removed_rings();
        if removed.is_empty() {
            return;
        }
        let Some(rc) = self.rendering_context.clone() else {
            return;
        };
        for ring in removed {
            for texture in ring.mapped {
                rc.unmap_d3d11_texture(texture);
            }
            for texture in ring.textures {
                if let Some(wrap) = self.d3d11_wrap_cache.remove(&texture) {
                    rc.destroy_d3d11_gl_wrap(wrap);
                }
                rc.release_d3d11_texture(texture);
            }
        }
    }

    fn lock_d3d11(
        &mut self,
        id: u64,
        binding: D3d11PlaneBinding,
    ) -> (ExternalImageSource<'_>, Size2D<i32>) {
        // rendering_context가 없으면 이 경로 전체가 꺼진 것 — 링을 전혀
        // 건드리지 않고(lock_count 증가 없음) 즉시 Invalid를 반환한다. 로컬
        // 추적에도 넣지 않으므로 unlock은 짝맞춰 no-op이 된다.
        let Some(rc) = self.rendering_context.clone() else {
            return (ExternalImageSource::Invalid, Size2D::zero());
        };

        // 여기서부터는 note_plane_lock_and_plan을 호출해 lock_count를 올릴 수
        // 있으므로, unlock이 반드시 note_plane_unlock으로 짝을 맞추도록 로컬에
        // 기록해 둔다(글로벌 바인딩 맵이 lock~unlock 사이 변경돼도 안전).
        self.locked_d3d11_planes.insert(id, binding.ring_id);

        // 합성당 1회 소비: 링별 lock_count 0→1 전이에서만 plan이 나온다.
        // Some(plan)은 반드시 정확히 한 번 commit_consume으로 끝나야 한다
        // (consume_plan이 모든 실패 분기 포함 이를 보장한다).
        if let Some(plan) = D3d11PlaneRings::note_plane_lock_and_plan(binding.ring_id) {
            consume_plan(&*rc, binding.ring_id, plan);
        }

        // lock 반환용 텍스처: 현재 Presenting 슬롯의 이 plane 기술자.
        let Some(plane) = D3d11PlaneRings::presenting_plane(binding.ring_id, binding.plane_index)
        else {
            return (ExternalImageSource::Invalid, Size2D::zero());
        };

        // EGLImage 래핑 캐시(텍스처 usize 키). 정상 상태에서 프레임당 재래핑 0.
        let wrap = if let Some(cached) = self.d3d11_wrap_cache.get(&plane.texture) {
            *cached
        } else {
            let import_start = std::time::Instant::now(); // D3D11PROF
            match rc.wrap_d3d11_texture_as_gl_texture(plane.texture) {
                Some(wrap) => {
                    // D3D11PROF: 렌더러 스레드 첫-lock 래핑 소요 — 매 건 로깅
                    // (일회성 이벤트). n/sum_ms는 프로세스 누계.
                    if d3d11_profile_enabled() {
                        let us = import_start.elapsed().as_micros() as u64;
                        let n = D3D11_IMPORT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                        let total = D3D11_IMPORT_TOTAL_US.fetch_add(us, Ordering::Relaxed) + us;
                        warn!(
                            "D3D11PROF wrap texture={} took_ms={:.2} n={n} sum_ms={:.1}",
                            plane.texture,
                            us as f64 / 1000.0,
                            total as f64 / 1000.0,
                        );
                    }
                    self.d3d11_wrap_cache.insert(plane.texture, wrap);
                    wrap
                },
                None => {
                    warn!("D3D11 media: EGLImage 래핑 실패 (texture={})", plane.texture);
                    return (ExternalImageSource::Invalid, Size2D::zero());
                },
            }
        };
        (
            ExternalImageSource::NativeTexture(wrap.gl_texture),
            Size2D::new(plane.width, plane.height),
        )
    }
}

impl Drop for MediaExternalImages {
    fn drop(&mut self) {
        // rendering_context가 None이면 lock_d3d11이 항상 Invalid만 반환해
        // 래핑 캐시가 늘 비어 있으므로 그냥 반환해도 안전하다.
        let Some(rc) = self.rendering_context.clone() else {
            return;
        };
        // 1) 이미 제거된 링: 전체 정리(Unmap + 래핑 파기 + Release).
        for ring in D3d11PlaneRings::take_removed_rings() {
            for texture in ring.mapped {
                rc.unmap_d3d11_texture(texture);
            }
            for texture in ring.textures {
                if let Some(wrap) = self.d3d11_wrap_cache.remove(&texture) {
                    rc.destroy_d3d11_gl_wrap(wrap);
                }
                rc.release_d3d11_texture(texture);
            }
        }
        // 2) 아직 살아있는 링의 텍스처: 우리가 만든 GL 래핑(EGLImage)만
        //    파기한다. 텍스처 자체의 Unmap/Release는 링(프로듀서)이 소유하므로
        //    건드리지 않는다 — 살아있는 링의 텍스처를 Release하면 안 된다.
        for (_texture, wrap) in self.d3d11_wrap_cache.drain() {
            rc.destroy_d3d11_gl_wrap(wrap);
        }
    }
}

impl WebRenderExternalImageApi for MediaExternalImages {
    fn lock(&mut self, id: u64) -> (ExternalImageSource<'_>, Size2D<i32>) {
        // 제거된 링의 GPU 텍스처를 공통 prologue에서 배출한다: 마지막 D3D11
        // 비디오가 멈춰 더 이상 D3D11 plane을 lock하지 않게 된 뒤에도 다음
        // 임의 미디어 lock(raw 포함)에서 배수되도록. take_removed_rings가 비면
        // 즉시 반환하므로 비용은 uncontended mutex poll 1회.
        self.purge_removed_d3d11_entries();

        // GPU 상주 D3D11 plane: 렌더러는 링 슬롯을 소비(Map/Unmap)하고 EGLImage로
        // 래핑한 GL 텍스처를 돌려줄 뿐 CPU 업로드하지 않는다.
        if let Some(binding) = D3d11VideoFrameExternalImages::binding_for(id) {
            return self.lock_d3d11(id, binding);
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
        // D3D11 plane: lock에서 로컬 추적한 것만 note_plane_unlock으로 짝을 맞춘다
        // (lock_count 감소). 래핑 캐시는 unlock에서 유지한다 — 폐기는 링 제거 시.
        if let Some(ring_id) = self.locked_d3d11_planes.remove(&id) {
            D3d11PlaneRings::note_plane_unlock(ring_id);
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

    fn needs_vertical_flip(&mut self, id: u64) -> bool {
        // GPU 상주 D3D11 plane(EGLImage 래핑, 상단-하단 텍스처)만 플립 제외.
        // 바인딩 맵을 보면 첫 lock 전에도 판정할 수 있다.
        D3d11VideoFrameExternalImages::binding_for(id).is_none()
    }
}

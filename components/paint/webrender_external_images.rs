/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::rc::Rc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use euclid::default::Size2D;
use log::{debug, info, warn};
use paint_api::rendering_context::RenderingContext;
use paint_api::{ExternalImageSource, WebRenderExternalImageApi};
use rustc_hash::{FxHashMap, FxHashSet};
use servo_base::id::PainterId;
use servo_canvas_traits::webgl::{WebGLContextId, WebGLSurfaceId, WebGLThreads};
use surfman::chains::{SwapChainAPI, SwapChains, SwapChainsAPI};
use surfman::{Device, SurfaceTexture};
use webgl::webgl_thread::WebGLContextBusyMap;

/// `gfx_webgl_stage_to_painter_device` 게이트. 프레임마다 물어보는 자리라 pref 읽기를 캐시한다.
static STAGE_TO_PAINTER_DEVICE: LazyLock<bool> =
    LazyLock::new(|| servo_config::pref!(gfx_webgl_stage_to_painter_device));

/// 스테이징 복사용으로 painter 디바이스에 사는 텍스처와 blit 용 FBO 둘.
///
/// 크기가 바뀌면 통째로 다시 만든다. 서피스당 하나다.
/// painter 가 들고 있는 프론트 버퍼. `unlock` 이 무엇을 돌려줘야 하는지 구분한다.
enum LockedFrontBuffer {
    /// 이번 프레임에 새로 집어왔다 -- `unlock` 이 체인에 돌려준다(또는 `last_good` 으로
    /// 옮기고 그 전 것을 돌려준다).
    Fresh(SurfaceTexture),
    /// ★직전 프레임을 다시 보여줬다★ -- 돌려줄 것이 없다. 그 서피스는 `last_good` 이
    /// 계속 들고 있다.
    Stale,
}

/// 마지막으로 성공한 프레임. 생산자가 뮤텍스를 들고 있을 때 이것을 다시 보여준다.
///
/// `gl_texture` 는 ★`lock` 이 WR 에게 돌려준 바로 그 값★ 이다(`stage_to_painter_device` 가
/// 켜져 있으면 그 사본). 그래서 낡은 프레임도 같은 경로로 샘플된다.
struct LastGoodFrame {
    texture: SurfaceTexture,
    gl_texture: gleam::gl::GLuint,
    size: Size2D<i32>,
}

struct StagingTarget {
    texture: gleam::gl::GLuint,
    read_fbo: gleam::gl::GLuint,
    draw_fbo: gleam::gl::GLuint,
    size: Size2D<i32>,
}

/// Bridge between the webrender::ExternalImage callbacks and the WebGLThreads.
/// `STALECANVAS` 를 마지막으로 찍은 때(단조 ms). 0 = 아직 안 찍음.
static STALE_REPORT_AT_MS: AtomicU64 = AtomicU64::new(0);

/// 생산자가 뮤텍스를 들고 있어서 직전 프레임을 다시 보여준 횟수.
static STALE_REUSED: AtomicU64 = AtomicU64::new(0);
/// 같은 상황인데 보여줄 직전 프레임이 없어서 빈 칸이 된 횟수(첫 프레임 등).
static STALE_UNAVAILABLE: AtomicU64 = AtomicU64::new(0);

pub struct WebGLExternalImages {
    painter_id: PainterId,
    webgl_threads: WebGLThreads,
    rendering_context: Rc<dyn RenderingContext>,
    swap_chains: SwapChains<WebGLSurfaceId, Device>,
    busy_webgl_context_map: WebGLContextBusyMap,
    locked_front_buffers: FxHashMap<WebGLSurfaceId, Vec<LockedFrontBuffer>>,
    /// 서피스마다 마지막으로 성공한 프레임. `gfx_webgl_nonblocking_front_buffer_lock` 이
    /// 켜져 있을 때만 채운다. 한 서피스에 ★한 장★ 이다 -- 새 프레임이 들어오면 그 전 것을
    /// 체인에 돌려준다.
    last_good: FxHashMap<WebGLSurfaceId, LastGoodFrame>,
    /// 이번 프레임에 WR 에게 돌려준 `(gl_texture, size)`. `unlock` 이 서피스를 `last_good`
    /// 으로 옮길 때 같이 싣는다.
    pending_last_good: FxHashMap<WebGLSurfaceId, (gleam::gl::GLuint, Size2D<i32>)>,
    logged_locked_surfaces: FxHashSet<WebGLSurfaceId>,
    /// 서피스별 스테이징 대상(`gfx_webgl_stage_to_painter_device`).
    staging: FxHashMap<WebGLSurfaceId, StagingTarget>,
}

/// 스왑체인이 사라진 뒤에 돌아온 프론트 버퍼 수(진단용). 0 이 아니면 해체와 잠금이
/// 겹치는 길이 실제로 밟혔다는 뜻이다 -- GPU 메모리가 새는 것은 아니지만(그 자원은
/// `destroy_context` 가 이미 반환했다) 그 겹침 자체가 다음에 볼 것이다.
static ORPHANED_FRONT_BUFFERS: AtomicU64 = AtomicU64::new(0);

impl WebGLExternalImages {
    pub fn new(
        painter_id: PainterId,
        webgl_threads: WebGLThreads,
        rendering_context: Rc<dyn RenderingContext>,
        swap_chains: SwapChains<WebGLSurfaceId, Device>,
        busy_webgl_context_map: WebGLContextBusyMap,
    ) -> Self {
        Self {
            painter_id,
            webgl_threads,
            rendering_context,
            swap_chains,
            busy_webgl_context_map,
            locked_front_buffers: FxHashMap::default(),
            last_good: FxHashMap::default(),
            pending_last_good: FxHashMap::default(),
            logged_locked_surfaces: FxHashSet::default(),
            staging: FxHashMap::default(),
        }
    }

    /// 프론트 버퍼를 체인에 돌려준다. 체인이 이미 사라졌으면 잊는다.
    ///
    /// `unlock_swap_chain` 의 꼬리를 그대로 뽑은 것이다 -- 낡은 프레임 유지 경로가 같은
    /// 처리를 해야 해서 두 곳에서 부른다. 패닉하면 안 되는 이유는 아래 주석에 있다.
    fn recycle_or_forget(
        &mut self,
        surface_id: WebGLSurfaceId,
        surface: surfman::Surface,
    ) -> Option<()> {
        match self.swap_chains.get(surface_id) {
            Some(swap_chain) => swap_chain.recycle_surface(surface),
            None => {
                // ★여기서 패닉하면 안 된다★
                //
                // 예전에는 `expect("Should always have a SwapChain for a busy WebGLContext")`
                // 였다. 그 "always" 가 깨지는 경우가 실제로 있다 -- WebGL 표출을 중단하면
                // 컨텍스트가 그 자리에서 해체되는데(`release_contexts_of_destroyed_document`),
                // 그때 이 페인터가 아직 프론트 버퍼를 쥐고 있으면 돌려줄 스왑체인이 없다.
                //
                // 대가가 컸다(log_webgl_memleak_repraise/06): 이 패닉이 `main` 에서 나면
                // 프로세스가 통째로 죽고(크래시 로그도 안 남는다 -- 패닉이지 AV 가 아니다),
                // `WallTilePainter` 에서 나면 그 타일만 죽어 **초기화 상태의 흰 화면**이
                // 그대로 남는다(사용자가 본 "4번 타일에 작업표시줄과 흰 화면").
                //
                // 이 서피스의 GL 자원은 이미 사라졌다 -- 스왑체인을 부순 쪽이
                // `destroy_context` 까지 했으므로 GPU 쪽은 그때 반환됐다. 남은 것은 죽은
                // 핸들을 담은 러스트 구조체뿐인데, `Surface::drop` 은 파괴되지 않은 것을
                // 보면 패닉하도록 되어 있다. 그래서 잊는다(GPU 메모리를 새게 하는 것이
                // 아니다). 다만 이 길이 얼마나 밟히는지는 보여야 한다.
                let count = ORPHANED_FRONT_BUFFERS.fetch_add(1, Ordering::Relaxed) + 1;
                warn!(
                    "WebGL external image: no swap chain for {surface_id:?} at unlock; \
                     the context was torn down while this painter held a front buffer \
                     (orphaned_total={count})"
                );
                std::mem::forget(surface);
            },
        }

        let _ = self.webgl_threads.finished_rendering_to_context(surface_id);

        Some(())
    }

    /// 직전 프레임을 다시 보여준다. 생산자가 뮤텍스를 들고 있을 때 쓴다.
    ///
    /// ★`Stale` 표를 꼭 밀어 넣는다★ -- `unlock` 이 그것을 꺼내 busy 계수를 내린다. 안
    /// 넣으면 `lock` 이 올린 계수가 영원히 남는다(기존 실패 경로들이 그 자리에서 직접
    /// 내리는 것과 같은 이유다).
    fn reuse_last_good(&mut self, surface_id: WebGLSurfaceId) -> Option<(u32, Size2D<i32>)> {
        if let Some(last) = self.last_good.get(&surface_id) {
            STALE_REUSED.fetch_add(1, Ordering::Relaxed);
            self.locked_front_buffers
                .entry(surface_id)
                .or_default()
                .push(LockedFrontBuffer::Stale);
            return Some((last.gl_texture, last.size));
        }
        // 보여줄 것이 아직 없다(첫 프레임). 빈 칸이 된다 -- 기존 동작과 같다.
        STALE_UNAVAILABLE.fetch_add(1, Ordering::Relaxed);
        self.mark_surface_not_busy(surface_id);
        let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
        None
    }

    fn lock_swap_chain(&mut self, id: WebGLContextId) -> Option<(u32, Size2D<i32>)> {
        let surface_id = WebGLSurfaceId::new(id, self.painter_id);
        debug!("... locking chain {:?} for surface {:?}", id, surface_id);

        {
            let mut busy_webgl_context_map = self.busy_webgl_context_map.write();
            *busy_webgl_context_map.entry(surface_id).or_default() += 1;
        }

        let Some(swap_chain) = self.swap_chains.get(surface_id) else {
            // ★스왑체인이 없다 = 이 컨텍스트의 백엔드를 놓아 둔 상태★ — 그런데 방금 WR 이
            // 이것을 합성하려 했으므로 다시 보이기 시작한 것이다. 되살려 달라고 알리고 이
            // 프레임만 비운다(비디오 링이 처음 뜰 때와 같은 모양이다).
            let _ = self.webgl_threads.restore_context(id);
            self.mark_surface_not_busy(surface_id);
            let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
            return None;
        };
        // ★생산자를 기다리지 않는다★(`gfx_webgl_nonblocking_front_buffer_lock`). 근거는
        // 그 pref 와 `SwapChain::try_take_surface` 의 doc 주석에 있다 -- 요지는 이 뮤텍스가
        // 무제한 GPU 대기 위에 걸쳐 잡혀 있어서, 캔버스가 느리면 ***관계없는 요소까지***
        // 이 스레드에서 같이 늦는다는 것이다.
        let nonblocking = servo_config::pref!(gfx_webgl_nonblocking_front_buffer_lock);
        let front_buffer = if nonblocking {
            match swap_chain.try_take_surface() {
                Ok(surface) => surface,
                Err(()) => {
                    // 생산자가 스왑/clear 중이다. 이 프레임은 포기하고 직전 것을 보여준다.
                    return self.reuse_last_good(surface_id);
                },
            }
        } else {
            swap_chain.take_surface()
        };
        let Some(front_buffer) = front_buffer else {
            // 뮤텍스는 잡았는데 서피스가 없다. 여기서도 낡은 프레임이 빈 칸보다 낫다.
            if nonblocking {
                return self.reuse_last_good(surface_id);
            }
            warn!("WebGL external image lock failed: no front buffer for {surface_id:?}");
            self.mark_surface_not_busy(surface_id);
            let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
            return None;
        };
        let created = self.rendering_context.create_texture(front_buffer);
        let (surface_texture, gl_texture, size) = match created {
            Ok(texture) => texture,
            Err(front_buffer) => {
                // 잠금 경로에도 같은 함정이 있다 — 서피스를 집어온 사이에 컨텍스트가
                // 해체되면 돌려줄 체인이 없다. 이유와 대처는 `unlock_swap_chain` 의
                // 같은 자리 주석과 같다.
                match self.swap_chains.get(surface_id) {
                    Some(swap_chain) => swap_chain.recycle_surface(front_buffer),
                    None => {
                        let count = ORPHANED_FRONT_BUFFERS.fetch_add(1, Ordering::Relaxed) + 1;
                        warn!(
                            "WebGL external image: no swap chain for {surface_id:?} after taking \
                             a surface; the context was torn down mid-lock \
                             (orphaned_total={count})"
                        );
                        std::mem::forget(front_buffer);
                    },
                }
                self.mark_surface_not_busy(surface_id);
                let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
                return None;
            },
        };
        let locked_buffers = self.locked_front_buffers.entry(surface_id).or_default();
        if !locked_buffers.is_empty() {
            warn!(
                "WebGL external image nested lock: surface={surface_id:?} depth_before={}",
                locked_buffers.len(),
            );
        }
        locked_buffers.push(LockedFrontBuffer::Fresh(surface_texture));
        // 크로스-디바이스 텍스처 대신 painter 디바이스 사본을 넘긴다(진단용, 기본 off).
        let gl_texture = if *STAGE_TO_PAINTER_DEVICE {
            let staged = self.stage_to_painter_device(surface_id, gl_texture, size);
            staged.unwrap_or(gl_texture)
        } else {
            gl_texture
        };
        if nonblocking {
            report_stale_counters();
            // ★WR 에게 돌려주는 바로 그 값★ 을 기억한다. 서피스 자체는 `unlock` 에서
            // `last_good` 으로 옮긴다(여기서 옮기면 이 프레임 중에 소유권이 빠진다).
            self.pending_last_good
                .insert(surface_id, (gl_texture, size));
        }
        if self.logged_locked_surfaces.insert(surface_id) {
            info!(
                "WebGL external image lock routed: surface={surface_id:?} painter={:?} texture={gl_texture} size={size:?}",
                self.painter_id,
            );
        }

        Some((gl_texture, size))
    }

    /// 크로스-디바이스 텍스처를 painter 디바이스의 텍스처로 한 번 복사해서 그것을 WR 에게
    /// 넘긴다(`gfx_webgl_stage_to_painter_device`).
    ///
    /// ★무엇을 가르려는 것인가★: `Commit` 이 캔버스가 있을 때만 비싸다. 캔버스의 GPU 작업을
    /// 미리 끝내는 것(`gfx_webgl_swap_sync=finish`)은 **효과가 없었다** — WebGL 스레드가
    /// 17 초를 기다렸는데 Commit 은 9.87→9.86ms 로 요지부동이었다. 그러니 "소스가 준비 안 됨"
    /// 은 아니다. 남은 후보가 **크로스-디바이스 텍스처를 DComp 서피스로 읽어 들이는 비용
    /// 자체**이고, 이 복사가 그것을 타일 드로우 밖으로 빼낸다. 떨어지면 원인 확정이고,
    /// 그대로면 크로스-디바이스도 아니다.
    ///
    /// ★WR `render()` 안에서 도는 코드다★ — FBO 바인딩을 반드시 저장·복원한다. 안 그러면
    /// WR 이 그리던 대상이 바뀌어 화면이 깨진다.
    // `gleam` 의 `get_integer_v` 가 unsafe 다. WR 이 걸어 둔 FBO 바인딩을 되돌리려면 그것을
    // 읽어야 하고, 읽지 않고 0 으로 되돌리면 WR 이 그리던 대상이 바뀐다.
    #[expect(unsafe_code)]
    fn stage_to_painter_device(
        &mut self,
        surface_id: WebGLSurfaceId,
        source: gleam::gl::GLuint,
        size: Size2D<i32>,
    ) -> Option<gleam::gl::GLuint> {
        use gleam::gl;

        if size.width <= 0 || size.height <= 0 {
            return None;
        }
        let api = self.rendering_context.gleam_gl_api();

        let needs_new = self
            .staging
            .get(&surface_id)
            .is_none_or(|target| target.size != size);
        if needs_new {
            if let Some(old) = self.staging.remove(&surface_id) {
                api.delete_framebuffers(&[old.read_fbo, old.draw_fbo]);
                api.delete_textures(&[old.texture]);
            }
            let texture = api.gen_textures(1)[0];
            api.bind_texture(gl::TEXTURE_2D, texture);
            api.tex_image_2d(
                gl::TEXTURE_2D,
                0,
                gl::RGBA8 as gl::GLint,
                size.width,
                size.height,
                0,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                None,
            );
            api.tex_parameter_i(
                gl::TEXTURE_2D,
                gl::TEXTURE_MIN_FILTER,
                gl::LINEAR as gl::GLint,
            );
            api.tex_parameter_i(
                gl::TEXTURE_2D,
                gl::TEXTURE_MAG_FILTER,
                gl::LINEAR as gl::GLint,
            );
            api.tex_parameter_i(
                gl::TEXTURE_2D,
                gl::TEXTURE_WRAP_S,
                gl::CLAMP_TO_EDGE as gl::GLint,
            );
            api.tex_parameter_i(
                gl::TEXTURE_2D,
                gl::TEXTURE_WRAP_T,
                gl::CLAMP_TO_EDGE as gl::GLint,
            );
            api.bind_texture(gl::TEXTURE_2D, 0);
            let fbos = api.gen_framebuffers(2);
            self.staging.insert(
                surface_id,
                StagingTarget {
                    texture,
                    read_fbo: fbos[0],
                    draw_fbo: fbos[1],
                    size,
                },
            );
        }
        let target = self.staging.get(&surface_id)?;

        // WR 의 현재 바인딩을 기억해 두고 끝나면 되돌린다.
        let mut prev_read = [0 as gl::GLint];
        let mut prev_draw = [0 as gl::GLint];
        unsafe {
            api.get_integer_v(gl::READ_FRAMEBUFFER_BINDING, &mut prev_read);
            api.get_integer_v(gl::DRAW_FRAMEBUFFER_BINDING, &mut prev_draw);
        }

        api.bind_framebuffer(gl::READ_FRAMEBUFFER, target.read_fbo);
        api.framebuffer_texture_2d(
            gl::READ_FRAMEBUFFER,
            gl::COLOR_ATTACHMENT0,
            gl::TEXTURE_2D,
            source,
            0,
        );
        api.bind_framebuffer(gl::DRAW_FRAMEBUFFER, target.draw_fbo);
        api.framebuffer_texture_2d(
            gl::DRAW_FRAMEBUFFER,
            gl::COLOR_ATTACHMENT0,
            gl::TEXTURE_2D,
            target.texture,
            0,
        );
        api.blit_framebuffer(
            0,
            0,
            size.width,
            size.height,
            0,
            0,
            size.width,
            size.height,
            gl::COLOR_BUFFER_BIT,
            gl::NEAREST,
        );

        api.bind_framebuffer(gl::READ_FRAMEBUFFER, prev_read[0] as gl::GLuint);
        api.bind_framebuffer(gl::DRAW_FRAMEBUFFER, prev_draw[0] as gl::GLuint);

        Some(target.texture)
    }

    fn mark_surface_not_busy(&self, surface_id: WebGLSurfaceId) {
        let mut busy_webgl_context_map = self.busy_webgl_context_map.write();
        let Some(count) = busy_webgl_context_map.get_mut(&surface_id) else {
            warn!("WebGL external image busy counter underflow: missing entry for {surface_id:?}");
            return;
        };

        if *count == 0 {
            warn!("WebGL external image busy counter underflow: zero count for {surface_id:?}");
        } else {
            *count -= 1;
        }
    }

    fn unlock_swap_chain(&mut self, id: WebGLContextId) -> Option<()> {
        let surface_id = WebGLSurfaceId::new(id, self.painter_id);
        debug!("... unlocked chain {:?} for surface {:?}", id, surface_id);

        let locked_front_buffer = match self.locked_front_buffers.get_mut(&surface_id) {
            Some(locked_buffers) => {
                let locked_front_buffer = locked_buffers.pop();
                if locked_buffers.is_empty() {
                    self.locked_front_buffers.remove(&surface_id);
                }
                locked_front_buffer
            },
            None => None,
        };
        // ★낡은 프레임을 보여준 턴★ -- 돌려줄 서피스가 없다. busy 계수만 내린다.
        if let Some(LockedFrontBuffer::Stale) = locked_front_buffer {
            self.mark_surface_not_busy(surface_id);
            let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
            return Some(());
        }
        let locked_front_buffer = match locked_front_buffer {
            Some(LockedFrontBuffer::Fresh(texture)) => Some(texture),
            // `Stale` 은 위에서 처리했다.
            Some(LockedFrontBuffer::Stale) | None => None,
        };
        let Some(locked_front_buffer) = locked_front_buffer else {
            // The matching lock did not acquire a front buffer (e.g. none was ready),
            // and it already released the busy count on that failure path. There is
            // nothing locked to release here, so return without decrementing the busy
            // counter again (which would underflow).
            return None;
        };

        // Only release the busy count once an actual locked buffer is being unlocked,
        // keeping lock/unlock balanced.
        self.mark_surface_not_busy(surface_id);

        // ★최신 프레임을 `last_good` 으로 옮기고, 그 ***전*** 것을 체인에 돌려준다.★
        // 그래서 painter 가 들고 있는 것은 언제나 한 장이고, 다음 프레임에 생산자가 뮤텍스를
        // 들고 있어도 보여줄 그림이 있다. 들고 있는 서피스는 `recycled_surfaces` 에 없으니
        // 생산자가 그것을 바인드하지 않는다 -- 교착은 생기지 않는다.
        if let Some((gl_texture, size)) = self.pending_last_good.remove(&surface_id) {
            let previous = self.last_good.insert(
                surface_id,
                LastGoodFrame {
                    texture: locked_front_buffer,
                    gl_texture,
                    size,
                },
            );
            match previous {
                // 그 전 프레임을 돌려준다 -- 아래 기존 경로를 그대로 탄다.
                Some(previous) => {
                    let destroyed = self.rendering_context.destroy_texture(previous.texture);
                    let Some(previous_surface) = destroyed else {
                        let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
                        return Some(());
                    };
                    return self.recycle_or_forget(surface_id, previous_surface);
                },
                // 첫 프레임이라 돌려줄 것이 없다.
                None => {
                    let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
                    return Some(());
                },
            }
        }

        let destroyed = self.rendering_context.destroy_texture(locked_front_buffer);
        let locked_front_buffer = destroyed?;

        self.recycle_or_forget(surface_id, locked_front_buffer)?;

        let _ = self.webgl_threads.finished_rendering_to_context(surface_id);

        Some(())
    }
}

/// `STALECANVAS` 를 10 초마다 한 줄 찍는다.
///
/// ★이 수치가 이 수정의 판정이다.★ 0 이면 뮤텍스 경합이 없다는 뜻이고, 그러면 저더의
/// 원인에 대한 내 분석이 틀린 것이다. 크면 painter 가 그만큼 캔버스를 기다리고 있었다는
/// 뜻이다. 둘 다 알아야 한다.
///
/// 타이밍 경로에서 불리므로 ★꺼져 있을 때 비용은 원자 로드 하나★ 이고, 켜져 있어도
/// 간격 안에서는 로드 하나와 비교 하나다. I/O 는 간격이 지났을 때만 한다.
fn report_stale_counters() {
    let reused = STALE_REUSED.load(Ordering::Relaxed);
    let unavailable = STALE_UNAVAILABLE.load(Ordering::Relaxed);
    if reused == 0 && unavailable == 0 {
        return;
    }
    let now_ms = std::time::Instant::now()
        .duration_since(*PROCESS_START)
        .as_millis() as u64;
    let last = STALE_REPORT_AT_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) < 10_000 {
        return;
    }
    if STALE_REPORT_AT_MS
        .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        // 다른 painter 가 방금 찍었다.
        return;
    }
    warn!(
        "STALECANVAS t={}s reused={reused} unavailable={unavailable}",
        now_ms / 1000,
    );
}

/// `report_stale_counters` 의 기준점. 단조 시계만 쓴다.
static PROCESS_START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

impl WebRenderExternalImageApi for WebGLExternalImages {
    fn lock(&mut self, id: u64) -> (ExternalImageSource<'_>, Size2D<i32>) {
        let locked = self.lock_swap_chain(WebGLContextId(id));
        match locked {
            Some((texture_id, size)) => (ExternalImageSource::NativeTexture(texture_id), size),
            None => (ExternalImageSource::Invalid, Size2D::zero()),
        }
    }

    fn unlock(&mut self, id: u64) {
        self.unlock_swap_chain(WebGLContextId(id));
    }
}

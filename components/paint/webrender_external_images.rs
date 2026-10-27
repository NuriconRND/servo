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
struct StagingTarget {
    texture: gleam::gl::GLuint,
    read_fbo: gleam::gl::GLuint,
    draw_fbo: gleam::gl::GLuint,
    size: Size2D<i32>,
}

/// Bridge between the webrender::ExternalImage callbacks and the WebGLThreads.
pub struct WebGLExternalImages {
    painter_id: PainterId,
    webgl_threads: WebGLThreads,
    rendering_context: Rc<dyn RenderingContext>,
    swap_chains: SwapChains<WebGLSurfaceId, Device>,
    busy_webgl_context_map: WebGLContextBusyMap,
    locked_front_buffers: FxHashMap<WebGLSurfaceId, Vec<SurfaceTexture>>,
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
            logged_locked_surfaces: FxHashSet::default(),
            staging: FxHashMap::default(),
        }
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
        let front_buffer = swap_chain.take_surface();
        let Some(front_buffer) = front_buffer else {
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
        locked_buffers.push(surface_texture);
        // 크로스-디바이스 텍스처 대신 painter 디바이스 사본을 넘긴다(진단용, 기본 off).
        let gl_texture = if *STAGE_TO_PAINTER_DEVICE {
            let staged = self.stage_to_painter_device(surface_id, gl_texture, size);
            staged.unwrap_or(gl_texture)
        } else {
            gl_texture
        };
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
        let destroyed = self.rendering_context.destroy_texture(locked_front_buffer);
        let locked_front_buffer = destroyed?;

        match self.swap_chains.get(surface_id) {
            Some(swap_chain) => swap_chain.recycle_surface(locked_front_buffer),
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
                std::mem::forget(locked_front_buffer);
            },
        }

        let _ = self.webgl_threads.finished_rendering_to_context(surface_id);

        Some(())
    }
}

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

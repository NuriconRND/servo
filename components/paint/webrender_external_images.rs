/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use euclid::default::Size2D;
use log::{debug, info, warn};
use paint_api::rendering_context::RenderingContext;
use paint_api::{ExternalImageSource, WebRenderExternalImageApi};
use rustc_hash::{FxHashMap, FxHashSet};
use servo_base::id::PainterId;
use servo_canvas_traits::webgl::{WebGLContextId, WebGLSurfaceId, WebGLThreads};
use servo_config::debug_env;
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

/// `SERVO_WEBGL_FANOUT_PROF` 게이트. 프레임마다 물어보는 자리라 env 읽기를 캐시한다.
static WEBGL_FANOUT_PROF: LazyLock<bool> = LazyLock::new(|| {
    debug_env::string(&debug_env::WEBGL_FANOUT_PROF)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
});

/// WebGL 팬아웃의 **소비자 측** 비용(`SERVO_WEBGL_FANOUT_PROF`). 창은 1 초.
///
/// 재는 이유: WebGL 캔버스가 생기는 순간 타일 하나의 렌더가 0.23ms 에서 15ms 로 뛰는데
/// (2026-09-01 실측, 캔버스 생성과 1 초 안에 상관), 그 시간이 전부 `renderer.render()`
/// 안이고 `wr_update_ms=0.01`, `draw_calls=2`, `upload_mb=0.0` 이다. 그리는 일도 올리는
/// 일도 아니라면 남는 것은 **기다리는 일**이고, 이 콜백이 그 유일한 후보다.
///
/// 두 갈래를 갈라야 한다:
/// * `create_ns` 가 크다 = 서피스를 **기다린다**(크로스-디바이스 keyed mutex 획득).
///   생산자와 소비자가 서로를 기다리는 모양이고, 캔버스가 13.2fps 인데 WebGL 스레드가
///   95% 유휴인 것과 맞아떨어진다.
/// * `no_front_buffer` 가 크다 = 기다리는 게 아니라 **받을 게 없다**. 그러면 원인은
///   생산 쪽이지 이 경로가 아니다.
#[derive(Default)]
struct ExternalImageProfile {
    window_start: Option<Instant>,
    locks: u64,
    lock_ns: u64,
    /// `swap_chain.take_surface()` — 생산자가 내놓은 프런트 버퍼를 집어오는 시간.
    take_ns: u64,
    /// `create_texture()` — 공유 핸들 임포트와 keyed mutex 획득이 여기서 일어난다.
    create_ns: u64,
    /// 프런트 버퍼가 없어 그냥 돌아온 횟수(기다린 게 아니라 받을 게 없던 경우).
    no_front_buffer: u64,
    unlocks: u64,
    unlock_ns: u64,
    /// `destroy_texture()` — keyed mutex 해제와 서피스 반납.
    destroy_ns: u64,
    /// 스테이징 복사(`gfx_webgl_stage_to_painter_device`)에 쓴 시간과 횟수.
    stage_ns: u64,
    stages: u64,
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
    /// 소비자 측 비용 누적기. 꺼져 있으면 갱신되지 않는다.
    profile: ExternalImageProfile,
    /// 서피스별 스테이징 대상(`gfx_webgl_stage_to_painter_device`).
    staging: FxHashMap<WebGLSurfaceId, StagingTarget>,
}

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
            profile: ExternalImageProfile::default(),
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
            warn!("WebGL external image lock failed: missing swap chain for {surface_id:?}");
            self.mark_surface_not_busy(surface_id);
            let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
            return None;
        };
        let take_start = WEBGL_FANOUT_PROF.then(Instant::now);
        let front_buffer = swap_chain.take_surface();
        if let Some(start) = take_start {
            self.profile.take_ns += start.elapsed().as_nanos() as u64;
        }
        let Some(front_buffer) = front_buffer else {
            if *WEBGL_FANOUT_PROF {
                self.profile.no_front_buffer += 1;
            }
            warn!("WebGL external image lock failed: no front buffer for {surface_id:?}");
            self.mark_surface_not_busy(surface_id);
            let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
            return None;
        };
        let create_start = WEBGL_FANOUT_PROF.then(Instant::now);
        let created = self.rendering_context.create_texture(front_buffer);
        if let Some(start) = create_start {
            self.profile.create_ns += start.elapsed().as_nanos() as u64;
        }
        let (surface_texture, gl_texture, size) = match created {
            Ok(texture) => texture,
            Err(front_buffer) => {
                self.swap_chains
                    .get(surface_id)
                    .expect("Should always have a SwapChain after taking a surface")
                    .recycle_surface(front_buffer);
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
            let start = WEBGL_FANOUT_PROF.then(Instant::now);
            let staged = self.stage_to_painter_device(surface_id, gl_texture, size);
            if let Some(start) = start {
                self.profile.stage_ns += start.elapsed().as_nanos() as u64;
                self.profile.stages += 1;
            }
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
                gl::TEXTURE_2D, 0, gl::RGBA8 as gl::GLint, size.width, size.height, 0,
                gl::RGBA, gl::UNSIGNED_BYTE, None,
            );
            api.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as gl::GLint);
            api.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as gl::GLint);
            api.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as gl::GLint);
            api.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as gl::GLint);
            api.bind_texture(gl::TEXTURE_2D, 0);
            let fbos = api.gen_framebuffers(2);
            self.staging.insert(surface_id, StagingTarget {
                texture,
                read_fbo: fbos[0],
                draw_fbo: fbos[1],
                size,
            });
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
            gl::READ_FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, source, 0,
        );
        api.bind_framebuffer(gl::DRAW_FRAMEBUFFER, target.draw_fbo);
        api.framebuffer_texture_2d(
            gl::DRAW_FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, target.texture, 0,
        );
        api.blit_framebuffer(
            0, 0, size.width, size.height,
            0, 0, size.width, size.height,
            gl::COLOR_BUFFER_BIT, gl::NEAREST,
        );

        api.bind_framebuffer(gl::READ_FRAMEBUFFER, prev_read[0] as gl::GLuint);
        api.bind_framebuffer(gl::DRAW_FRAMEBUFFER, prev_draw[0] as gl::GLuint);

        Some(target.texture)
    }

    /// 계측 창(1 초)을 닫고 한 줄 찍는다(`SERVO_WEBGL_FANOUT_PROF`).
    fn maybe_emit_profile(&mut self) {
        if !*WEBGL_FANOUT_PROF {
            return;
        }
        let now = Instant::now();
        let window = now - *self.profile.window_start.get_or_insert(now);
        if window < Duration::from_secs(1) {
            return;
        }
        let ms = |ns: u64| ns as f64 / 1_000_000.0;
        let profile = &self.profile;
        warn!(
            "WEBGLEXTIMG painter={:?} window_ms={:.0} locks={} lock_ms={:.1} \
             take_ms={:.1} create_ms={:.1} no_front_buffer={} unlocks={} \
             unlock_ms={:.1} destroy_ms={:.1} stage_ms={:.1} stages={}",
            self.painter_id,
            window.as_secs_f64() * 1000.0,
            profile.locks,
            ms(profile.lock_ns),
            ms(profile.take_ns),
            ms(profile.create_ns),
            profile.no_front_buffer,
            profile.unlocks,
            ms(profile.unlock_ns),
            ms(profile.destroy_ns),
            ms(profile.stage_ns),
            profile.stages,
        );
        self.profile = ExternalImageProfile {
            window_start: Some(now),
            ..Default::default()
        };
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
        let destroy_start = WEBGL_FANOUT_PROF.then(Instant::now);
        let destroyed = self.rendering_context.destroy_texture(locked_front_buffer);
        if let Some(start) = destroy_start {
            self.profile.destroy_ns += start.elapsed().as_nanos() as u64;
        }
        let locked_front_buffer = destroyed?;

        self.swap_chains
            .get(surface_id)
            .expect("Should always have a SwapChain for a busy WebGLContext")
            .recycle_surface(locked_front_buffer);

        let _ = self.webgl_threads.finished_rendering_to_context(surface_id);

        Some(())
    }
}

impl WebRenderExternalImageApi for WebGLExternalImages {
    fn lock(&mut self, id: u64) -> (ExternalImageSource<'_>, Size2D<i32>) {
        let start = WEBGL_FANOUT_PROF.then(Instant::now);
        let locked = self.lock_swap_chain(WebGLContextId(id));
        if let Some(start) = start {
            self.profile.locks += 1;
            self.profile.lock_ns += start.elapsed().as_nanos() as u64;
        }
        match locked {
            Some((texture_id, size)) => (ExternalImageSource::NativeTexture(texture_id), size),
            None => (ExternalImageSource::Invalid, Size2D::zero()),
        }
    }

    fn unlock(&mut self, id: u64) {
        let start = WEBGL_FANOUT_PROF.then(Instant::now);
        self.unlock_swap_chain(WebGLContextId(id));
        if let Some(start) = start {
            self.profile.unlocks += 1;
            self.profile.unlock_ns += start.elapsed().as_nanos() as u64;
        }
        self.maybe_emit_profile();
    }
}

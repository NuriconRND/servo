/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::rc::Rc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
    locked_front_buffers: FxHashMap<WebGLSurfaceId, Vec<Option<SurfaceTexture>>>,
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

    fn lock_swap_chain(&mut self, id: WebGLContextId) -> Option<(u32, Size2D<i32>)> {
        let surface_id = WebGLSurfaceId::new(id, self.painter_id);
        debug!("... locking chain {:?} for surface {:?}", id, surface_id);

        // ★painter 별 샤드로 간다.★ 근거는 `WebGLContextBusyMap` 의 doc 주석 -- 앞 판은
        // painter 4 개가 하나의 `RwLock` 을 공유해 프레임마다 초당 480 회 write 로 잡았다.
        self.busy_webgl_context_map.mark_busy(surface_id);

        let Some(swap_chain) = self.swap_chains.get(surface_id) else {
            // ★스왑체인이 없다 = 이 컨텍스트의 백엔드를 놓아 둔 상태★ — 그런데 방금 WR 이
            // 이것을 합성하려 했으므로 다시 보이기 시작한 것이다. 되살려 달라고 알리고 이
            // 프레임만 비운다(비디오 링이 처음 뜰 때와 같은 모양이다).
            let _ = self.webgl_threads.restore_context(id);
            self.mark_surface_not_busy(surface_id);
            let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
            return None;
        };
        // ★여기서 생산자를 기다린다 -- 그것이 올바른 동작이다.★
        //
        // 2026-11-09 에 기다리지 않는 경로를 넣었다가 2026-11-11 에 제거했다
        // (`gfx_webgl_nonblocking_front_buffer_lock`). 실기에서 ***훨씬 나빠졌다***
        // (log_webgl_perf_01/29): 캔버스 전달이 120/s 에서 30/s 로 떨어지고(painter
        // 넷으로 나누면 7.5fps -- 운영자 육안 "10fps 에도 못 미치는 것처럼" 과 맞는다),
        // `surfaces_created` 44 -> 196, `surfaces_live` 16 -> 52,
        // `pbuffer_ms` 50 -> 1678 이었다.
        //
        // ★기구: 직전 프레임을 다시 보여주려면 그 서피스를 들고 있어야 한다.★ 제거한
        // 코드의 주석이 그것을 그대로 적어 뒀다 -- "painter 가 들고 있는 것은 언제나
        // 한 장이고". 그러면 두 장짜리 체인이 ***한 장으로*** 돌고, `swap_buffers` 가
        // `recycled_surfaces` 에서 재활용할 것을 찾지 못해 매번 새 서피스를 만든다.
        // 즉 이 뮤텍스 대기는 버그가 아니라 ***서피스 순환을 지키는 장치***다.
        //
        // 대기를 없애려면 장이 하나 더 필요하다(트리플 버퍼링, 백엔드마다 +190MB).
        // 그 전에 이 대기가 실제로 얼마나 긴지를 재야 한다 -- ★`LOCKWAIT` 가 그것을
        // 잰다★(2026-11-11, `note_lock_wait` 의 doc 주석).
        let wait_began = Instant::now();
        // ★나이를 함께 받는다★ -- 꼬리의 원인을 가르는 값이다. 근거는 `note_lock_wait` 의
        // doc 주석.
        let taken = swap_chain.take_surface_with_age();
        let took_surface_at = Instant::now();
        let (front_buffer, pending_age) = match taken {
            Some((surface, age)) => (Some(surface), age),
            None => (None, None),
        };
        let Some(front_buffer) = front_buffer else {
            warn!("WebGL external image lock failed: no front buffer for {surface_id:?}");
            self.mark_surface_not_busy(surface_id);
            let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
            return None;
        };
        let created = self.rendering_context.create_texture(front_buffer);
        note_lock_wait(
            took_surface_at.saturating_duration_since(wait_began),
            took_surface_at.elapsed(),
            pending_age,
        );
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
        locked_buffers.push(Some(surface_texture));
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
        self.busy_webgl_context_map.mark_not_busy(surface_id);
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
        let locked_front_buffer = locked_front_buffer.flatten();
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

        // ★세 단계를 가른다.★ 근거는 `note_unlock_cost` 의 doc 주석 -- 65 번에서 HUD 를 끈 뒤
        // 남은 꼬리를 `unlock` 이 지배했고(p50 6637us, max 55285us) 그 안에 `ReleaseSync` 가
        // 있다. 어느 단계인지 알아야 고칠 수 있다.
        let at = Instant::now();
        let destroyed = self.rendering_context.destroy_texture(locked_front_buffer);
        let destroy_us = at.elapsed().as_micros() as u64;
        let locked_front_buffer = destroyed?;

        let at = Instant::now();
        self.recycle_or_forget(surface_id, locked_front_buffer)?;
        let recycle_us = at.elapsed().as_micros() as u64;

        let at = Instant::now();
        let _ = self.webgl_threads.finished_rendering_to_context(surface_id);
        note_unlock_steps(destroy_us, recycle_us, at.elapsed().as_micros() as u64);

        Some(())
    }
}

/// `lock_swap_chain` 의 대기 둘을 잰다. 1200 lock 마다 한 줄, ★꼬리는 그때그때 한 줄★.
///
/// ## 재는 것
///
/// * ★`chain`★ -- `take_surface` 가 스왑체인의 블로킹 Mutex 를 기다린 시간.
/// * ★`tex`★ -- `create_texture` 가 걸린 시간. 그 안에서 DXGI keyed mutex 를
///   `AcquireSync(0, INFINITE)` 로 잡으므로, GPU 가 그 서피스를 쓰고 있으면 기다린다.
/// * ★`age`★ -- 그 장이 생산된 뒤 지난 시간(`None` = `recycled` 에서 꺼냄).
///
/// ## 왜 `age` 가 필요한가 (2026-11-11, log_webgl_perf_01/42)
///
/// 첫 실측에서 `tex` 가 평균 73~136us 인데 5 초 구간 최대가 2735~11172us 였다 --
/// ***꼬리가 평균의 100 배***다. 간헐적 judder 가 바로 이 꼬리일 수 있다.
///
/// 내가 "`SURFIMPORT acquire_ms` 의 구간 평균이 79us 이니 트리플 버퍼링은 효용이 없다" 고
/// 적었는데 ***운영자가 그 논리를 반증했다*** -- 1 초 window 의 누적 합으로 나눈 평균은
/// 꼬리에 대해 아무것도 말하지 않고, 잡으려는 것이 바로 꼬리다. 그리고 트리플 버퍼링이
/// 고치는 것은 "평균 대기" 가 아니라 ***대기가 발생하는 조건***이다.
///
/// ```text
///   소비자가 방금 생산된 장을 가져감  ->  GPU 가 아직 그 장에 쓰고 있음  ->  대기
///   장이 셋이면 한 프레임 묵은 장을 가져갈 수 있음  ->  GPU 가 끝낼 시간이 있음
/// ```
///
/// 그래서 트리플 버퍼링의 효용을 가르는 수치는 하나다.
///
/// * `age` 가 짧은데(1~3ms) 꼬리가 난다  ->  GPU 가 아직 쓰고 있다  ->  ★장을 늘리면 고친다★
/// * `age` 가 긴데도(30ms+) 꼬리가 난다  ->  GPU 가 다른 일로 바쁘다  ->  ★장을 늘려도 무효★
///
/// ## 비용
///
/// lock 당 시계 셋과 원자 여덟이다(히스토그램 버킷 하나 포함). `Instant::now()` 가 약
/// 25ns 이므로 240 lock/s 에서 18us/s 다. 꼬리 줄은 8ms 이상일 때만 나오고 실측 빈도가
/// ***초당 0.2 회***라 I/O 가 문제되지 않는다. `swap_buffers` 쪽에 시계 하나가 더 늘었다
/// (240/s = 6us/s).
/// `ExternalImageHandler::unlock` 한 번의 비용. 꼬리(>= 2ms)는 그때그때, 나머지는 1200 회마다.
///
/// ## 왜 (2026-11-13)
///
/// `lock` 쪽은 `LOCKWAIT`(요약)와 `LOCKTAIL`(꼬리)이 `chain`/`tex` 를 재는데 ★`unlock` 쪽은
/// 비어 있었다.★ 64 번에서 `-ProfilerHud "FPS"` 를 빼니 judder 가 사라지고(운영자 육안) 꼬리가
/// 305 건 -> 19 건으로 줄었는데, ***그 19 건을 `unlock` 이 지배한다***
/// (p50 12.5ms, p90 56.7ms, max 76.3ms). 그리고 그 프레임들은 `draws=3 upload=0.63MB
/// full_paint=true` 로 실제 작업이 있는 프레임이다.
///
/// `unlock_swap_chain` 안에 `ReleaseSync` 가 있고, 그것은 생산자가 그 서피스를 다시 잡을 수
/// 있게 keyed mutex 를 놓는 일이다. 거기서 수십 ms 가 가면 ★소비자가 생산자를 기다리는 것★
/// 이므로, 과제 #6 이 `AcquireSync` 로 잘못 겨냥했던 것의 반대편이다.
///
/// ## 비용
///
/// 이 경로는 이미 `ReleaseSync` 와 GL 호출을 하므로 `Instant::now()` 둘이 무의미하다. 프레임당
/// 캔버스 수만큼 불린다(실측 240/s 이하).
/// `unlock_swap_chain` 의 세 단계. 합이 2ms 를 넘으면 그때그때, 나머지는 1200 회마다.
///
/// ## 단계 (2026-11-13)
///
///     destroy   rendering_context.destroy_texture(locked_front_buffer)
///     recycle   recycle_or_forget  -- ★여기에 `ReleaseSync` 가 있다★
///     finish    webgl_threads.finished_rendering_to_context
///
/// 65 번에서 HUD 를 끈 뒤 남은 꼬리 31 건을 `unlock` 이 지배했다(p50 6637us, p90 33086us,
/// max 55285us). 과제 #6 을 11 일 동안 `AcquireSync` 로 겨냥했는데 실제 꼬리는 ***반대편인
/// `ReleaseSync`*** 다. 어느 단계인지 이 계측이 가른다.
fn note_unlock_steps(destroy_us: u64, recycle_us: u64, finish_us: u64) {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    static SUM: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
    static MAX: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
    static TAILS: AtomicU64 = AtomicU64::new(0);

    let parts = [destroy_us, recycle_us, finish_us];
    let total: u64 = parts.iter().sum();
    let nth = CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    for (slot, value) in SUM.iter().zip(parts) {
        slot.fetch_add(value, Ordering::Relaxed);
    }
    for (slot, value) in MAX.iter().zip(parts) {
        slot.fetch_max(value, Ordering::Relaxed);
    }
    if total >= 2_000 {
        let tail = TAILS.fetch_add(1, Ordering::Relaxed) + 1;
        warn!(
            "UNLOCKSTEP #{tail} total={total}us destroy={destroy_us}us \
             recycle={recycle_us}us finish={finish_us}us"
        );
    }
    if nth % 1200 != 0 {
        return;
    }
    warn!(
        "UNLOCKSTEPS n={nth} destroy(avg={}us max={}us) recycle(avg={}us max={}us) \
         finish(avg={}us max={}us) tails={}",
        SUM[0].swap(0, Ordering::Relaxed) / 1200,
        MAX[0].swap(0, Ordering::Relaxed),
        SUM[1].swap(0, Ordering::Relaxed) / 1200,
        MAX[1].swap(0, Ordering::Relaxed),
        SUM[2].swap(0, Ordering::Relaxed) / 1200,
        MAX[2].swap(0, Ordering::Relaxed),
        TAILS.load(Ordering::Relaxed),
    );
}

fn note_unlock_cost(took: Duration) {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    static SUM_US: AtomicU64 = AtomicU64::new(0);
    static MAX_US: AtomicU64 = AtomicU64::new(0);
    static TAILS: AtomicU64 = AtomicU64::new(0);

    let us = took.as_micros() as u64;
    let nth = CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    SUM_US.fetch_add(us, Ordering::Relaxed);
    MAX_US.fetch_max(us, Ordering::Relaxed);
    if took >= Duration::from_millis(2) {
        let tail = TAILS.fetch_add(1, Ordering::Relaxed) + 1;
        warn!("UNLOCKTAIL #{tail} took={us}us");
    }
    if nth % 1200 != 0 {
        return;
    }
    warn!(
        "UNLOCKCOST n={nth} took(avg={}us max={}us) tails={}",
        SUM_US.swap(0, Ordering::Relaxed) / 1200,
        MAX_US.swap(0, Ordering::Relaxed),
        TAILS.load(Ordering::Relaxed),
    );
}

fn note_lock_wait(chain: Duration, tex: Duration, age: Option<Duration>) {
    /// 꼬리로 보고 한 줄 찍는 기준. 한 주기(16.7ms)의 절반이다.
    const TAIL_US: u64 = 8_000;

    static CHAIN_SUM_US: AtomicU64 = AtomicU64::new(0);
    static CHAIN_MAX_US: AtomicU64 = AtomicU64::new(0);
    static TEX_SUM_US: AtomicU64 = AtomicU64::new(0);
    static TEX_MAX_US: AtomicU64 = AtomicU64::new(0);
    static LOCKS: AtomicU64 = AtomicU64::new(0);
    /// `tex` 의 버킷: <0.5ms, <2ms, <8ms, >=8ms. 꼬리가 하나인지 여러 개인지 보인다.
    static TEX_BUCKETS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
    /// `recycled` 에서 꺼낸 횟수(= 직전 프레임 재표출). `age` 가 `None` 인 경우다.
    static RECYCLED: AtomicU64 = AtomicU64::new(0);
    static TAILS: AtomicU64 = AtomicU64::new(0);

    let chain_us = chain.as_micros() as u64;
    let tex_us = tex.as_micros() as u64;
    CHAIN_SUM_US.fetch_add(chain_us, Ordering::Relaxed);
    CHAIN_MAX_US.fetch_max(chain_us, Ordering::Relaxed);
    TEX_SUM_US.fetch_add(tex_us, Ordering::Relaxed);
    TEX_MAX_US.fetch_max(tex_us, Ordering::Relaxed);
    let bucket = match tex_us {
        ..500 => 0,
        500..2_000 => 1,
        2_000..TAIL_US => 2,
        _ => 3,
    };
    TEX_BUCKETS[bucket].fetch_add(1, Ordering::Relaxed);
    match age {
        Some(_) => {},
        None => {
            RECYCLED.fetch_add(1, Ordering::Relaxed);
        },
    }

    // ★꼬리는 그때그때 한 줄.★ 이 줄이 트리플 버퍼링의 효용을 가른다 -- 근거는 위 doc.
    if tex_us >= TAIL_US {
        let nth = TAILS.fetch_add(1, Ordering::Relaxed) + 1;
        // ★위상을 함께 찍는다★ -- 이 사건이 전환의 어디에서 났는지를 ***자기가 들고
        // 다닌다***. 1 초 타임스탬프로는 "애니메이션 마지막 0.75 초" 와 "끝난 직후" 를
        // 가를 수 없었다(근거는 `paint_api::animation_phase` 의 doc).
        //
        // 판정: 0.7~1.0 에 몰리면 오버슈트 복귀, 1.0 초과면 전환 종료 처리, 고르게
        // 퍼지면 전환 전체다.
        // `phase` 는 전환 진행도가 아니다 -- 2026-11-12 실측에서 130 건이 모두
        // 0.014~0.050 이었다. `paint_animation_phase` 가 DL 빌드 시각을 0 으로, 3 초
        // horizon 을 분모로 쓰기 때문이고(DL 은 초당 36~60 회 갱신된다), 그래서 아래의
        // "0.7~1.0 이면 오버슈트 복귀" 는 ***그 범위에 값이 갈 수 없으므로 성립하지
        // 않는다.*** 잘못 만든 계측이다. 남겨 두는 것은 DL 수명의 대리값으로는 읽히기
        // 때문이고, 전환 진행도는 `t=` 가 준다.
        let phase = match paint_api::animation_phase() {
            Some(phase) => format!("{phase:.3}"),
            None => "idle".to_owned(),
        };
        // 전환 기준 시각으로부터의 경과. 근거는 `surfman` 의 `mark_transition_epoch` doc.
        // 원자 load 하나다.
        let at = match surfman::platform::windows::angle::surface::transition_elapsed_ms() {
            Some(ms) => format!("+{ms}ms"),
            None => "n/a".to_owned(),
        };
        match age {
            Some(age) => warn!(
                "LOCKTAIL #{nth} tex={tex_us}us chain={chain_us}us t={at} phase={phase} \
                 surface_age={}us (fresh from producer)",
                age.as_micros()
            ),
            None => warn!(
                "LOCKTAIL #{nth} tex={tex_us}us chain={chain_us}us t={at} phase={phase} \
                 surface_age=n/a (recycled, re-present)"
            ),
        }
    }

    // ★호출 수로 리포트 시점을 정한다★ -- 시계를 또 읽지 않는다. 1200 은 네 painter 가
    // 60Hz 로 돌 때 약 5 초다.
    let locks = LOCKS.fetch_add(1, Ordering::Relaxed) + 1;
    if locks % 1200 != 0 {
        return;
    }
    let n = LOCKS.swap(0, Ordering::Relaxed).max(1);
    warn!(
        "LOCKWAIT n={n} chain(avg={}us max={}us) tex(avg={}us max={}us) \
         tex_hist(<0.5ms={} <2ms={} <8ms={} >=8ms={}) recycled={}",
        CHAIN_SUM_US.swap(0, Ordering::Relaxed) / n,
        CHAIN_MAX_US.swap(0, Ordering::Relaxed),
        TEX_SUM_US.swap(0, Ordering::Relaxed) / n,
        TEX_MAX_US.swap(0, Ordering::Relaxed),
        TEX_BUCKETS[0].swap(0, Ordering::Relaxed),
        TEX_BUCKETS[1].swap(0, Ordering::Relaxed),
        TEX_BUCKETS[2].swap(0, Ordering::Relaxed),
        TEX_BUCKETS[3].swap(0, Ordering::Relaxed),
        RECYCLED.swap(0, Ordering::Relaxed),
    );
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
        // ★`unlock` 쪽은 한 번도 재지 않았다.★ 근거는 `note_unlock_cost` 의 doc 주석 --
        // `lock` 쪽은 `LOCKWAIT`/`LOCKTAIL` 이 재는데 이쪽은 비어 있었고, 64 번에서 HUD 를
        // 끈 뒤 남은 꼬리 19 건을 `unlock` 이 지배했다(p50 12.5ms, max 76.3ms).
        let started = Instant::now();
        self.unlock_swap_chain(WebGLContextId(id));
        note_unlock_cost(started.elapsed());
    }
}

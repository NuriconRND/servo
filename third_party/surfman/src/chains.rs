/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! An implementation of thread-safe swap chains for the `surfman` surface manager.
//!
//! The role of a swap chain is to allow surfaces to be communicated between contexts,
//! often in different threads. Each swap chain has a *producer* context,
//! responsible for creating and destroying surfaces, and a number of *consumer* contexts,
//! (usually just one) which take surfaces from the swap chain, and return them for recycling.
//!
//! Each swap chain has a *back buffer*, that is the current surface that the producer context may draw to.
//! Each swap chain has a *front buffer*, that is the most recent surface the producer context finished drawing to.
//!
//! The producer may *swap* these buffers when it has finished drawing and has a surface ready to display.
//!
//! The consumer may *take* the front buffer, display it, then *recycle* it.
//!
//! Each producer context has one *attached* swap chain, whose back buffer is the current surface of the context.
//! The producer may change the attached swap chain, attaching a currently unattached swap chain,
//! and detaching the currently attached one.

#![allow(missing_docs)]

use crate::device::Device as DeviceAPI;
use crate::{ContextID, Error, SurfaceAccess, SurfaceInfo, SurfaceType};
use euclid::default::Size2D;
use fnv::{FnvHashMap, FnvHashSet};
use glow as gl;
use glow::Context as Gl;
use glow::HasContext;
use log::debug;
use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::fmt::Debug;
use std::hash::Hash;
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::time::{Duration, Instant};

// The data stored for each swap chain.
struct SwapChainData<Device: DeviceAPI> {
    // The size of the back buffer
    size: Size2D<i32>,
    // The id of the producer context
    context_id: ContextID,
    // The surface access mode for the context.
    surface_access: SurfaceAccess,
    // The back buffer of the swap chain.
    back_buffer: BackBuffer<Device>,
    // Some if the producing context has finished drawing a new front buffer, ready to be displayed.
    //
    // ★`deferred_front_buffer` 가 켜지면 두 칸까지 들고, 소비자가 앞에서 꺼낸다★
    // (2026-11-12). 그러면 소비자가 받는 장은 ***한 프레임 전***에 제출된 것이라 그 GPU
    // 작업이 끝났을 가능성이 높다 -- `AcquireSync` 꼬리를 줄이려는 시도다. 근거는
    // `set_deferred_front_buffer` 의 doc 주석.
    //
    // 꺼져 있으면 한 칸만 쓰므로 ***예전 `Option` 과 동작이 같다.***
    pending_surfaces: VecDeque<(Device::Surface, Instant)>,
    /// 두 칸을 쓸지. `paint` 가 pref 를 보고 세운다.
    deferred_front_buffer: bool,
    // (`pending_since` 는 `pending_surfaces` 가 시각을 함께 들게 되어 2026-11-12 에 사라졌다.
    //  그 값이 무엇을 가르는지는 `webrender_external_images.rs` 의 `note_lock_wait` doc 에 있다.)
    // All of the surfaces that have already been displayed, ready to be recycled.
    recycled_surfaces: Vec<Device::Surface>,
}

pub enum PreserveBuffer<'a> {
    Yes(&'a Gl),
    No,
}

enum BackBuffer<Device: DeviceAPI> {
    Attached,
    Detached(Device::Surface),
    TakenAttached,
    TakenDetached,
}

impl<Device: DeviceAPI> BackBuffer<Device> {
    fn take_surface(
        &mut self,
        device: &Device,
        context: &mut Device::Context,
    ) -> Result<Device::Surface, Error> {
        let new_back_buffer = match self {
            BackBuffer::Attached => BackBuffer::TakenAttached,
            BackBuffer::Detached(_) => BackBuffer::TakenDetached,
            _ => return Err(Error::Failed),
        };
        let surface = match mem::replace(self, new_back_buffer) {
            BackBuffer::Attached => device.unbind_surface_from_context(context)?.unwrap(),
            BackBuffer::Detached(surface) => surface,
            _ => unreachable!(),
        };
        Ok(surface)
    }
    fn take_surface_texture(
        &mut self,
        device: &Device,
        context: &mut Device::Context,
    ) -> Result<Device::SurfaceTexture, Error> {
        let surface = self.take_surface(device, context)?;
        device
            .create_surface_texture(context, surface)
            .map_err(|(err, surface)| {
                let _ = self.replace_surface(device, context, surface);
                err
            })
    }
    fn replace_surface(
        &mut self,
        device: &Device,
        context: &mut Device::Context,
        surface: Device::Surface,
    ) -> Result<(), Error> {
        let new_back_buffer = match self {
            BackBuffer::TakenAttached => {
                if let Err((err, mut surface)) = device.bind_surface_to_context(context, surface) {
                    debug!("Oh no, destroying surface");
                    let _ = device.destroy_surface(context, &mut surface);
                    return Err(err);
                }
                BackBuffer::Attached
            },
            BackBuffer::TakenDetached => BackBuffer::Detached(surface),
            _ => return Err(Error::Failed),
        };
        *self = new_back_buffer;
        Ok(())
    }
    fn replace_surface_texture(
        &mut self,
        device: &Device,
        context: &mut Device::Context,
        surface_texture: Device::SurfaceTexture,
    ) -> Result<(), Error> {
        let surface = device
            .destroy_surface_texture(context, surface_texture)
            .map_err(|(err, _)| err)?;
        self.replace_surface(device, context, surface)
    }
}

impl<Device: DeviceAPI> SwapChainData<Device> {
    // Returns `Ok` if `context` is the producer context for this swap chain.
    fn validate_context(&self, device: &Device, context: &Device::Context) -> Result<(), Error> {
        if self.context_id == device.context_id(context) {
            Ok(())
        } else {
            Err(Error::IncompatibleContext)
        }
    }

    // Swap the back and front buffers.
    // Called by the producer.
    // Returns an error if `context` is not the producer context for this swap chain.
    fn swap_buffers(
        &mut self,
        device: &Device,
        context: &mut Device::Context,
        preserve_buffer: PreserveBuffer<'_>,
    ) -> Result<(), Error> {
        debug!("Swap buffers on context {:?}", self.context_id);
        self.validate_context(device, context)?;

        // Recycle the old front buffer
        //
        // ★두 칸을 쓸 때는 앞의 것만 밀어낸다★ -- 뒤의 것은 아직 소비자가 가져가지 않은
        // 최신 프레임이다. 한 칸이면 예전과 같이 그 하나를 재활용한다.
        let keep = if self.deferred_front_buffer { 1 } else { 0 };
        while self.pending_surfaces.len() > keep {
            let Some((old_front_buffer, _)) = self.pending_surfaces.pop_front() else {
                break;
            };
            let SurfaceInfo { id, size, .. } = device.surface_info(&old_front_buffer);
            debug!(
                "Recycling surface {:?} ({:?}) for context {:?}",
                id, size, self.context_id
            );
            self.recycle_surface(old_front_buffer);
        }

        // Fetch a new back buffer, recycling presented buffers if possible.
        let new_back_buffer = self
            .recycled_surfaces
            .iter()
            .position(|surface| device.surface_info(surface).size == self.size)
            .map(|index| {
                debug!("Recycling surface for context {:?}", self.context_id);
                Ok(self.recycled_surfaces.swap_remove(index))
            })
            .unwrap_or_else(|| {
                debug!(
                    "Creating a new surface ({:?}) for context {:?}",
                    self.size, self.context_id
                );
                let surface_type = SurfaceType::Generic { size: self.size };
                crate::platform::windows::angle::surface::note_next_surface_origin(
                    crate::platform::windows::angle::surface::SurfaceOrigin::Swap,
                );
                device.create_surface(context, self.surface_access, surface_type)
            })?;

        let back_info = device.surface_info(&new_back_buffer);

        // Swap the buffers
        debug!(
            "Surface {:?} is the new back buffer for context {:?}",
            device.surface_info(&new_back_buffer).id,
            self.context_id
        );
        let new_front_buffer = self.back_buffer.take_surface(device, context)?;
        self.back_buffer
            .replace_surface(device, context, new_back_buffer)?;

        if let PreserveBuffer::Yes(gl) = preserve_buffer {
            let front_info = device.surface_info(&new_front_buffer);
            unsafe {
                gl.bind_framebuffer(gl::READ_FRAMEBUFFER, front_info.framebuffer_object);
                debug_assert_eq!(gl.get_error(), gl::NO_ERROR);
                gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, back_info.framebuffer_object);
                debug_assert_eq!(gl.get_error(), gl::NO_ERROR);
                gl.blit_framebuffer(
                    0,
                    0,
                    front_info.size.width,
                    front_info.size.height,
                    0,
                    0,
                    back_info.size.width,
                    back_info.size.height,
                    gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT | gl::STENCIL_BUFFER_BIT,
                    gl::NEAREST,
                );
                debug_assert_eq!(gl.get_error(), gl::NO_ERROR);
            }
        }

        // Update the state
        debug!(
            "Surface {:?} is the new front buffer for context {:?}",
            device.surface_info(&new_front_buffer).id,
            self.context_id
        );
        self.pending_surfaces
            .push_back((new_front_buffer, Instant::now()));
        Ok(())
    }

    // Swap the attached swap chain.
    // Called by the producer.
    // Returns an error if `context` is not the producer context for both swap chains.
    // Returns an error if this swap chain is attached, or the other swap chain is detached.
    fn take_attachment_from(
        &mut self,
        device: &Device,
        context: &mut Device::Context,
        other: &mut SwapChainData<Device>,
    ) -> Result<(), Error> {
        self.validate_context(device, context)?;
        other.validate_context(device, context)?;
        let our_surface = self.back_buffer.take_surface(device, context)?;
        let their_surface = other.back_buffer.take_surface(device, context)?;
        mem::swap(&mut self.back_buffer, &mut other.back_buffer);
        self.back_buffer
            .replace_surface(device, context, our_surface)?;
        other
            .back_buffer
            .replace_surface(device, context, their_surface)?;
        Ok(())
    }

    // Resize the swap chain.
    // This creates a new back buffer of the appropriate size,
    // and destroys the old one.
    // Called by the producer.
    // Returns an error if `context` is not the producer context for this swap chain.
    // Returns an error if `size` is smaller than (1, 1).
    fn resize(
        &mut self,
        device: &Device,
        context: &mut Device::Context,
        size: Size2D<i32>,
    ) -> Result<(), Error> {
        debug!(
            "Resizing context {:?} to {:?}",
            device.context_id(context),
            size
        );
        self.validate_context(device, context)?;
        if (size.width < 1) || (size.height < 1) {
            return Err(Error::Failed);
        }
        let surface_type = SurfaceType::Generic { size };
        crate::platform::windows::angle::surface::note_next_surface_origin(
            crate::platform::windows::angle::surface::SurfaceOrigin::Resize,
        );
        let new_back_buffer = device.create_surface(context, self.surface_access, surface_type)?;
        let mut old_back_buffer = self.back_buffer.take_surface(device, context)?;
        self.back_buffer
            .replace_surface(device, context, new_back_buffer)?;
        device.destroy_surface(context, &mut old_back_buffer)?;
        for mut surface in self.recycled_surfaces.drain(..) {
            device.destroy_surface(context, &mut surface)?;
        }
        // ★옛 크기 front buffer 도 버린다.★ 2026-11-12.
        //
        // 앞 판은 `pending_surface` 를 건드리지 않았다. 그러면 다음 `swap_buffers` 가 그
        // ***옛 크기*** 장을 `recycled_surfaces` 로 넣고(:166-173), 그 장은 크기 술어
        // (:176-183)를 영구히 실패시킨다 -- 즉 소비자에게 옛 크기 비트맵이 한 번 가고,
        // 재활용 풀은 쓸 수 없는 장을 안고 있게 된다.
        while let Some((mut stale_front, _)) = self.pending_surfaces.pop_front() {
            device.destroy_surface(context, &mut stale_front)?;
        }
        // ★다음 swap 이 쓸 장을 미리 넣어 둔다.★ 2026-11-12.
        //
        // 위에서 풀을 비웠으므로 그대로 두면 다음 `swap_buffers` 가 재활용할 것을 찾지
        // 못해 ***또*** `create_surface` 를 한다(:184-191). 벽 크기면 리사이즈 한 번에
        // 190MB 를 두 장 만드는 셈이고, 그 두 번째는 페이지의 첫 프레임 -- 즉 전환
        // 애니메이션이 ***이미 돌고 있을 때*** -- 일어난다.
        //
        // 여기서 한 장을 미리 만들면 그 할당이 이 호출 안으로 들어온다. 총 장 수는 같고,
        // 타이밍만 애니메이션 밖으로 옮긴다.
        crate::platform::windows::angle::surface::note_next_surface_origin(
            crate::platform::windows::angle::surface::SurfaceOrigin::Resize,
        );
        let spare =
            device.create_surface(context, self.surface_access, SurfaceType::Generic { size })?;
        self.recycled_surfaces.push(spare);
        self.size = size;
        Ok(())
    }

    // Get the current size.
    // Called by a consumer.
    fn size(&self) -> Size2D<i32> {
        self.size
    }

    // Take the current back buffer.
    // Called by a producer.
    fn take_surface_texture(
        &mut self,
        device: &Device,
        context: &mut Device::Context,
    ) -> Result<Device::SurfaceTexture, Error> {
        self.validate_context(device, context)?;
        self.back_buffer.take_surface_texture(device, context)
    }

    // Recycle the current back buffer.
    // Called by a producer.
    fn recycle_surface_texture(
        &mut self,
        device: &Device,
        context: &mut Device::Context,
        surface_texture: Device::SurfaceTexture,
    ) -> Result<(), Error> {
        self.validate_context(device, context)?;
        self.back_buffer
            .replace_surface_texture(device, context, surface_texture)
    }

    // Take the current front buffer.
    // Returns the most recent recycled surface if there is no current front buffer.
    // Called by a consumer.
    fn take_surface(&mut self) -> Option<Device::Surface> {
        self.take_surface_with_age().map(|(surface, _)| surface)
    }

    // `take_surface` 와 같지만 ★그 장이 생산된 뒤 얼마나 지났는지★ 를 함께 준다.
    //
    // `None` 은 `recycled` 에서 꺼냈다는 뜻이다 -- 직전 프레임을 다시 보여주는 경우이고,
    // GPU 가 이미 끝낸 장이라 `AcquireSync` 꼬리의 후보가 아니다. 근거는 `pending_since`
    // 의 주석.
    fn take_surface_with_age(&mut self) -> Option<(Device::Surface, Option<Duration>)> {
        // ★앞에서 꺼낸다★ -- 두 칸을 쓸 때 그것이 "한 프레임 전" 이다. 한 칸이면 그 하나이고
        // 예전과 같다.
        match self.pending_surfaces.pop_front() {
            Some((surface, at)) => Some((surface, Some(at.elapsed()))),
            None => self.recycled_surfaces.pop().map(|surface| (surface, None)),
        }
    }

    // Take the current front buffer.
    // Returns `None` if there is no current front buffer.
    // Called by a consumer.
    /// 두 칸을 쓸지 세운다. 근거는 `SwapChain::set_deferred_front_buffer` 의 doc.
    fn set_deferred_front_buffer(&mut self, enabled: bool) {
        self.deferred_front_buffer = enabled;
    }

    fn take_pending_surface(&mut self) -> Option<Device::Surface> {
        self.pending_surfaces
            .pop_front()
            .map(|(surface, _)| surface)
    }

    // Recycle the current front buffer.
    // Called by a consumer.
    fn recycle_surface(&mut self, surface: Device::Surface) {
        self.recycled_surfaces.push(surface)
    }

    // Clear the current back buffer.
    // Called by the producer.
    // Returns an error if `context` is not the producer context for this swap chain.
    fn clear_surface(
        &mut self,
        device: &Device,
        context: &mut Device::Context,
        gl: &Gl,
        color: [f32; 4],
        scissor: Option<[i32; 4]>,
    ) -> Result<(), Error> {
        self.validate_context(device, context)?;

        // Save the current GL state
        let draw_fbo;
        let read_fbo;
        let mut clear_color = [0., 0., 0., 0.];
        let mut clear_depth = [0.];
        let mut clear_stencil = [0];
        let color_mask;
        let depth_mask;
        let mut stencil_mask = [0];
        let scissor_enabled = unsafe { gl.is_enabled(gl::SCISSOR_TEST) };
        // ★박스도 저장한다★ -- `scissor` 를 받으면 아래에서 그것을 덮어쓰기 때문이다.
        // 활성 여부만 저장하던 원래 코드는 박스를 바꾸지 않았으므로 그걸로 충분했다.
        let mut saved_scissor_box = [0i32; 4];
        unsafe { gl.get_parameter_i32_slice(gl::SCISSOR_BOX, &mut saved_scissor_box) };
        let rasterizer_enabled = unsafe { gl.is_enabled(gl::RASTERIZER_DISCARD) };
        unsafe {
            draw_fbo = gl.get_parameter_framebuffer(gl::DRAW_FRAMEBUFFER_BINDING);
            read_fbo = gl.get_parameter_framebuffer(gl::READ_FRAMEBUFFER_BINDING);
            gl.get_parameter_f32_slice(gl::COLOR_CLEAR_VALUE, &mut clear_color[..]);
            gl.get_parameter_f32_slice(gl::DEPTH_CLEAR_VALUE, &mut clear_depth[..]);
            gl.get_parameter_i32_slice(gl::STENCIL_CLEAR_VALUE, &mut clear_stencil[..]);
            depth_mask = gl.get_parameter_bool(gl::DEPTH_WRITEMASK);
            gl.get_parameter_i32_slice(gl::STENCIL_WRITEMASK, &mut stencil_mask[..]);
            color_mask = gl.get_parameter_bool_array::<4>(gl::COLOR_WRITEMASK);
        }

        // Make the back buffer the current surface
        let reattach = if self.is_attached() {
            None
        } else {
            let surface = self.back_buffer.take_surface(device, context)?;
            let mut reattach = device.unbind_surface_from_context(context)?;
            if let Err((err, mut surface)) = device.bind_surface_to_context(context, surface) {
                debug!("Oh no, destroying surfaces");
                let _ = device.destroy_surface(context, &mut surface);
                if let Some(ref mut reattach) = reattach {
                    let _ = device.destroy_surface(context, reattach);
                }
                return Err(err);
            }
            reattach
        };

        // Clear it
        let fbo = device
            .context_surface_info(context)
            .unwrap()
            .unwrap()
            .framebuffer_object;
        unsafe {
            gl.bind_framebuffer(gl::FRAMEBUFFER, fbo);
            gl.clear_color(color[0], color[1], color[2], color[3]);
            gl.clear_depth(1.);
            gl.clear_stencil(0);
            // ★`scissor` 가 있으면 그 영역만 지운다.★ scissor 는 색·깊이·스텐실 clear 에
            // 모두 걸린다. 셰이딩에 쓰는 것과 같은 사각형이므로 지우는 영역과 그리는
            // 영역이 어긋날 수 없다 -- 근거는 호출 자리 주석.
            match scissor {
                Some([x, y, w, h]) => {
                    gl.scissor(x, y, w, h);
                    gl.enable(gl::SCISSOR_TEST);
                },
                None => gl.disable(gl::SCISSOR_TEST),
            }
            gl.disable(gl::RASTERIZER_DISCARD);
            gl.depth_mask(true);
            gl.stencil_mask(0xFFFFFFFF);
            gl.color_mask(true, true, true, true);
            gl.clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT | gl::STENCIL_BUFFER_BIT);
        }
        // Reattach the old surface
        if let Some(surface) = reattach {
            let mut old_surface = device.unbind_surface_from_context(context)?.unwrap();
            if let Err((err, mut surface)) = device.bind_surface_to_context(context, surface) {
                debug!("Oh no, destroying surface");
                let _ = device.destroy_surface(context, &mut surface);
                let _ = device.destroy_surface(context, &mut old_surface);
                return Err(err);
            }
            self.back_buffer
                .replace_surface(device, context, old_surface)?;
        }

        // Restore the GL state
        unsafe {
            gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, draw_fbo);
            gl.bind_framebuffer(gl::READ_FRAMEBUFFER, read_fbo);
            gl.clear_color(
                clear_color[0],
                clear_color[1],
                clear_color[2],
                clear_color[3],
            );
            gl.color_mask(color_mask[0], color_mask[1], color_mask[2], color_mask[3]);
            gl.clear_depth(clear_depth[0] as f64);
            gl.clear_stencil(clear_stencil[0]);
            gl.depth_mask(depth_mask);
            gl.stencil_mask(stencil_mask[0] as _);
            gl.scissor(
                saved_scissor_box[0],
                saved_scissor_box[1],
                saved_scissor_box[2],
                saved_scissor_box[3],
            );
            if scissor_enabled {
                gl.enable(gl::SCISSOR_TEST);
            } else {
                gl.disable(gl::SCISSOR_TEST);
            }
            if rasterizer_enabled {
                gl.enable(gl::RASTERIZER_DISCARD);
            }
        }

        Ok(())
    }

    /// Is this the attached swap chain?
    fn is_attached(&self) -> bool {
        match self.back_buffer {
            BackBuffer::Attached | BackBuffer::TakenAttached => true,
            BackBuffer::Detached(_) | BackBuffer::TakenDetached => false,
        }
    }

    // Destroy the swap chain.
    // Called by the producer.
    // Returns an error if `context` is not the producer context for this swap chain.
    fn destroy(&mut self, device: &Device, context: &mut Device::Context) -> Result<(), Error> {
        self.validate_context(device, context)?;
        let surfaces = self
            .pending_surfaces
            .drain(..)
            .map(|(surface, _)| surface)
            .chain(self.back_buffer.take_surface(device, context).into_iter())
            .chain(self.recycled_surfaces.drain(..));
        for mut surface in surfaces {
            device.destroy_surface(context, &mut surface)?;
        }
        Ok(())
    }
}

/// A thread-safe swap chain.
pub struct SwapChain<Device: DeviceAPI>(Arc<Mutex<SwapChainData<Device>>>);

// We can't derive Clone unfortunately
impl<Device: DeviceAPI> Clone for SwapChain<Device> {
    fn clone(&self) -> Self {
        SwapChain(self.0.clone())
    }
}

impl<Device: DeviceAPI> SwapChain<Device> {
    // Guarantee unique access to the swap chain data
    fn lock(&self) -> MutexGuard<SwapChainData<Device>> {
        self.0.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// Swap the back and front buffers.
    /// Called by the producer.
    /// Returns an error if `context` is not the producer context for this swap chain.
    pub fn swap_buffers(
        &self,
        device: &Device,
        context: &mut Device::Context,
        preserve_buffer: PreserveBuffer<'_>,
    ) -> Result<(), Error> {
        self.lock().swap_buffers(device, context, preserve_buffer)
    }

    /// Swap the attached swap chain.
    /// Called by the producer.
    /// Returns an error if `context` is not the producer context for both swap chains.
    /// Returns an error if this swap chain is attached, or the other swap chain is detached.
    pub fn take_attachment_from(
        &self,
        device: &Device,
        context: &mut Device::Context,
        other: &SwapChain<Device>,
    ) -> Result<(), Error> {
        self.lock()
            .take_attachment_from(device, context, &mut *other.lock())
    }

    /// Resize the swap chain.
    /// This creates a new back buffer of the appropriate size,
    /// and destroys the old one.
    /// Called by the producer.
    /// Returns an error if `context` is not the producer context for this swap chain.
    pub fn resize(
        &self,
        device: &Device,
        context: &mut Device::Context,
        size: Size2D<i32>,
    ) -> Result<(), Error> {
        self.lock().resize(device, context, size)
    }

    /// Get the current size.
    /// Called by a consumer.
    pub fn size(&self) -> Size2D<i32> {
        self.lock().size()
    }

    /// Take the current back buffer.
    /// Called by a producer.
    /// `take_surface` 와 같지만 ★그 장이 생산된 뒤 얼마나 지났는지★ 를 함께 준다.
    ///
    /// `None` 은 `recycled` 에서 꺼냈다는 뜻이다. 근거는 `SwapChainData::pending_since`
    /// 의 주석 -- 요점은 이 값이 `AcquireSync` 꼬리의 원인을 가른다는 것이다.
    pub fn take_surface_with_age(&self) -> Option<(Device::Surface, Option<Duration>)> {
        self.lock().take_surface_with_age()
    }

    pub fn take_surface_texture(
        &self,
        device: &Device,
        context: &mut Device::Context,
    ) -> Result<Device::SurfaceTexture, Error> {
        self.lock().take_surface_texture(device, context)
    }

    /// Recycle the current back buffer.
    /// Called by a producer.
    pub fn recycle_surface_texture(
        &self,
        device: &Device,
        context: &mut Device::Context,
        surface_texture: Device::SurfaceTexture,
    ) -> Result<(), Error> {
        self.lock()
            .recycle_surface_texture(device, context, surface_texture)
    }

    /// Take the current front buffer.
    /// Returns `None` if there is no current front buffer.
    /// Called by a consumer.
    /// 소비자가 ★한 프레임 묵은★ front buffer 를 가져가게 할지.
    ///
    /// ## 왜 (2026-11-12)
    ///
    /// 생산자는 그 장이 back buffer 인 ***전체 캔버스 프레임*** 동안 keyed mutex 를 잡는다
    /// (`bind_surface_to_context` 의 `AcquireSync` -> 다음 swap 의 `unbind` 의
    /// `ReleaseSync`). 소비자의 `AcquireSync` 는 그 서피스에 쌓인 GPU 작업이 은퇴할 때까지
    /// 기다리므로, ***가장 최신*** 장을 가져가면 그 작업이 아직 남아 있을 수 있다.
    ///
    /// 그리고 `recycled` 장이 기다리는 기구도 보인다 -- `swap_buffers` 가 옛 front 를 풀에
    /// 넣고(:166-180) 바로 다음 줄에서 크기 맞는 것을 집어 back buffer 로 쓰므로(:181-197),
    /// ★방금 넣은 그 장을 다시 집을 수 있다★. 소비자가 그것을 `pop` 으로 가져가면 직전
    /// 프레임의 작업이 남아 있다. 43 번 실측에서 꼬리 412 개 중 180 개가 그 경우였다.
    ///
    /// 켜면 큐를 두 칸으로 쓰고 소비자가 앞(= 한 프레임 전)에서 꺼낸다. 그 장의 작업은 한
    /// 프레임 전에 제출됐으므로 끝났을 가능성이 높다.
    ///
    /// ★대가★: 표출 지연 +1 프레임(60Hz 에서 16.7ms), 장 수 2 -> 3 (백엔드당 +199MB).
    /// 큐가 가득 차면 앞을 버린다(latest-wins) -- 소비자가 느릴 때 오래된 프레임을 쌓지
    /// 않는다.
    ///
    /// 꺼져 있으면(기본) 한 칸만 쓰므로 ***예전 동작과 완전히 같다.***
    pub fn set_deferred_front_buffer(&self, enabled: bool) {
        self.lock().set_deferred_front_buffer(enabled);
    }

    pub fn take_pending_surface(&self) -> Option<Device::Surface> {
        self.lock().take_pending_surface()
    }

    /// Clear the current back buffer.
    /// Called by the producer.
    /// Returns an error if `context` is not the producer context for this swap chain.
    /// Clear the current back buffer.
    /// Called by the producer.
    /// Returns an error if `context` is not the producer context for this swap chain.
    /// 캔버스를 지운다. `scissor` 가 `Some([x, y, w, h])` 면 ★그 영역만★ 지운다(GL 좌표,
    /// 아래-왼쪽 원점).
    ///
    /// ★왜★(2026-11-09 도입, 2026-11-10 제거, 2026-11-12 pref 게이트로 복원): 벽에서 이
    /// clear 가 ***프레임마다 서피스 전체***를 지운다 -- 11520x4320 RGBA8 = 한 장 199MB,
    /// 백엔드 네 장. 그런데 백엔드 k 는 자기 타일만 쓴다.
    ///
    /// 처음 제거한 근거는 운영자 평가였다 -- "scissor 가 제대로 이루어지지 않는 영역이
    /// 검은색 혹은 단색이 아니면 시각적으로 혼동만 일으킬 뿐이다." 그때는 scissor 자체가
    /// 틀렸고, 지금은 `lead 8` 에서 띠가 사라진 것이 실기로 확인됐다(39 번). 그래서
    /// ★지우는 영역과 그리는 영역이 같다는 전제가 이제 성립한다.★
    pub fn clear_surface(
        &self,
        device: &Device,
        context: &mut Device::Context,
        gl: &Gl,
        color: [f32; 4],
        scissor: Option<[i32; 4]>,
    ) -> Result<(), Error> {
        self.lock()
            .clear_surface(device, context, gl, color, scissor)
    }

    /// Is this the attached swap chain?
    pub fn is_attached(&self) -> bool {
        self.lock().is_attached()
    }

    /// Destroy the swap chain.
    /// Called by the producer.
    /// Returns an error if `context` is not the producer context for this swap chain.
    pub fn destroy(&self, device: &Device, context: &mut Device::Context) -> Result<(), Error> {
        self.lock().destroy(device, context)
    }

    /// Create a new attached swap chain
    pub fn create_attached(
        device: &Device,
        context: &mut Device::Context,
        surface_access: SurfaceAccess,
    ) -> Result<SwapChain<Device>, Error> {
        let size = device.context_surface_info(context).unwrap().unwrap().size;
        Ok(SwapChain(Arc::new(Mutex::new(SwapChainData {
            size,
            context_id: device.context_id(context),
            surface_access,
            back_buffer: BackBuffer::Attached,
            pending_surfaces: VecDeque::new(),
            deferred_front_buffer: false,
            recycled_surfaces: Vec::new(),
        }))))
    }

    /// Create a new detached swap chain
    pub fn create_detached(
        device: &Device,
        context: &mut Device::Context,
        surface_access: SurfaceAccess,
        size: Size2D<i32>,
    ) -> Result<SwapChain<Device>, Error> {
        let surface_type = SurfaceType::Generic { size };
        crate::platform::windows::angle::surface::note_next_surface_origin(
            crate::platform::windows::angle::surface::SurfaceOrigin::Detached,
        );
        let surface = device.create_surface(context, surface_access, surface_type)?;
        Ok(SwapChain(Arc::new(Mutex::new(SwapChainData {
            size,
            context_id: device.context_id(context),
            surface_access,
            back_buffer: BackBuffer::Detached(surface),
            pending_surfaces: VecDeque::new(),
            deferred_front_buffer: false,
            recycled_surfaces: Vec::new(),
        }))))
    }
}

impl<Device> SwapChainAPI for SwapChain<Device>
where
    Device: 'static + DeviceAPI,
    Device::Surface: Send,
{
    type Surface = Device::Surface;

    /// Take the current front buffer.
    /// Returns the most recent recycled surface if there is no current front buffer.
    /// Called by a consumer.
    fn take_surface(&self) -> Option<Device::Surface> {
        self.lock().take_surface()
    }

    /// Recycle the current front buffer.
    /// Called by a consumer.
    fn recycle_surface(&self, surface: Device::Surface) {
        self.lock().recycle_surface(surface)
    }
}

/// A thread-safe collection of swap chains.
#[derive(Default)]
pub struct SwapChains<SwapChainID: Eq + Hash, Device: DeviceAPI> {
    // The swap chain ids, indexed by context id
    ids: Arc<Mutex<FnvHashMap<ContextID, FnvHashSet<SwapChainID>>>>,
    // The swap chains, indexed by swap chain id
    table: Arc<RwLock<FnvHashMap<SwapChainID, SwapChain<Device>>>>,
}

// We can't derive Clone unfortunately
impl<SwapChainID: Eq + Hash, Device: DeviceAPI> Clone for SwapChains<SwapChainID, Device> {
    fn clone(&self) -> Self {
        SwapChains {
            ids: self.ids.clone(),
            table: self.table.clone(),
        }
    }
}

impl<SwapChainID, Device> SwapChains<SwapChainID, Device>
where
    SwapChainID: Clone + Eq + Hash + Debug,
    Device: DeviceAPI,
{
    /// Create a new collection.
    pub fn new() -> SwapChains<SwapChainID, Device> {
        SwapChains {
            ids: Arc::new(Mutex::new(FnvHashMap::default())),
            table: Arc::new(RwLock::new(FnvHashMap::default())),
        }
    }

    // Lock the ids
    fn ids(&self) -> MutexGuard<FnvHashMap<ContextID, FnvHashSet<SwapChainID>>> {
        self.ids.lock().unwrap_or_else(|err| err.into_inner())
    }

    // Lock the lookup table
    fn table(&self) -> RwLockReadGuard<FnvHashMap<SwapChainID, SwapChain<Device>>> {
        self.table.read().unwrap_or_else(|err| err.into_inner())
    }

    // Lock the lookup table for writing
    fn table_mut(&self) -> RwLockWriteGuard<FnvHashMap<SwapChainID, SwapChain<Device>>> {
        self.table.write().unwrap_or_else(|err| err.into_inner())
    }

    /// Create a new attached swap chain and insert it in the table.
    /// Returns an error if the `id` is already in the table.
    pub fn create_attached_swap_chain(
        &self,
        id: SwapChainID,
        device: &Device,
        context: &mut Device::Context,
        surface_access: SurfaceAccess,
    ) -> Result<(), Error> {
        match self.table_mut().entry(id.clone()) {
            Entry::Occupied(_) => Err(Error::Failed)?,
            Entry::Vacant(entry) => {
                entry.insert(SwapChain::create_attached(device, context, surface_access)?)
            },
        };
        self.ids()
            .entry(device.context_id(context))
            .or_insert_with(Default::default)
            .insert(id);
        Ok(())
    }

    /// Create a new dettached swap chain and insert it in the table.
    /// Returns an error if the `id` is already in the table.
    pub fn create_detached_swap_chain(
        &self,
        id: SwapChainID,
        size: Size2D<i32>,
        device: &Device,
        context: &mut Device::Context,
        surface_access: SurfaceAccess,
    ) -> Result<(), Error> {
        match self.table_mut().entry(id.clone()) {
            Entry::Occupied(_) => Err(Error::Failed)?,
            Entry::Vacant(entry) => entry.insert(SwapChain::create_detached(
                device,
                context,
                surface_access,
                size,
            )?),
        };
        self.ids()
            .entry(device.context_id(context))
            .or_insert_with(Default::default)
            .insert(id);
        Ok(())
    }

    /// Destroy a swap chain.
    /// Called by the producer.
    /// Returns an error if `context` is not the producer context for the swap chain.
    pub fn destroy(
        &self,
        id: SwapChainID,
        device: &Device,
        context: &mut Device::Context,
    ) -> Result<(), Error> {
        // ★테이블 락을 놓고 파괴한다.★ 2026-11-12.
        //
        // `if let` 의 scrutinee 가 만든 임시 `RwLockWriteGuard` 는 Rust 2021 의 임시값 수명
        // 규칙에서 ***블록 끝까지 산다***(이 크레이트는 edition 2021, Cargo.toml:13; 2024 가
        // 그것을 바꿨다). 그래서 앞 판은 서피스 파괴 전체를 write 락 안에서 했다.
        //
        // 소비자는 매 프레임 `lock_swap_chain` 에서 `table()`(read 락)을 잡고, writer 가
        // 대기 중이면 새 reader 가 막히므로 ★그 동안 네 painter 의 render() 가 전부 멈춘다★.
        // 벽에서 파괴할 장이 장당 199MB 로 둘셋이다.
        //
        // 운영자 관찰과 맞는다 -- "WebGL 구성끼리 전환할 때, 전환 애니메이션 종료 직후
        // 멈칫하는 현상이 거의 항상 나타나는데, WebGL이 포함되지 않는 구성에서 WebGL
        // 구성으로 넘어가면 그 현상이 발생하지 않는다." ★나가는 쪽에 WebGL 이 없으면
        // 파괴할 서피스가 없다.★
        //
        // `let` 으로 빼면 임시 guard 가 ***그 문장 끝에서*** 떨어진다. 테이블에서 빼낸 뒤에는
        // 다른 스레드가 그 체인을 찾을 수 없으므로 락 밖에서 파괴해도 안전하다.
        let removed = self.table_mut().remove(&id);
        if let Some(swap_chain) = removed {
            swap_chain.destroy(device, context)?;
        }
        if let Some(ids) = self.ids().get_mut(&device.context_id(context)) {
            ids.remove(&id);
        }
        Ok(())
    }

    /// Iterate over all the swap chains for a particular producer context.
    /// Called by the producer.
    pub fn iter(
        &self,
        device: &Device,
        context: &mut Device::Context,
    ) -> impl Iterator<Item = (SwapChainID, SwapChain<Device>)> {
        self.ids()
            .get(&device.context_id(context))
            .iter()
            .flat_map(|ids| ids.iter())
            .filter_map(|id| Some((id.clone(), self.table().get(id)?.clone())))
            .collect::<Vec<_>>()
            .into_iter()
    }
}

impl<SwapChainID, Device> SwapChainsAPI<SwapChainID> for SwapChains<SwapChainID, Device>
where
    SwapChainID: 'static + Clone + Eq + Hash + Debug + Sync + Send,
    Device: 'static + DeviceAPI,
    Device::Surface: Send,
{
    type Surface = Device::Surface;
    type SwapChain = SwapChain<Device>;

    /// Get a swap chain
    fn get(&self, id: SwapChainID) -> Option<SwapChain<Device>> {
        debug!("Getting swap chain {:?}", id);
        self.table().get(&id).cloned()
    }
}

/// The consumer's view of a swap chain
pub trait SwapChainAPI: 'static + Clone + Send {
    type Surface;

    /// Take the current front buffer.
    fn take_surface(&self) -> Option<Self::Surface>;

    /// Recycle the current front buffer.
    fn recycle_surface(&self, surface: Self::Surface);
}

/// The consumer's view of a collection of swap chains
pub trait SwapChainsAPI<SwapChainID>: 'static + Clone + Send {
    type Surface;
    type SwapChain: SwapChainAPI<Surface = Self::Surface>;

    /// Get a swap chain
    fn get(&self, id: SwapChainID) -> Option<Self::SwapChain>;
}

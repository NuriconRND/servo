/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Main process implementation of [GPUCanvasContext](https://www.w3.org/TR/webgpu/#canvas-context)

use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use arrayvec::ArrayVec;
use euclid::default::Size2D;
use log::warn;
use paint_api::rendering_context::RenderingContext;
#[cfg(windows)]
use paint_api::rendering_context::SurfaceTexture;
use paint_api::{
    CrossProcessPaintApi, ExternalImageSource, SerializableImageData, WebRenderExternalImageApi,
};
use pixels::{SharedSnapshot, Snapshot, SnapshotAlphaMode, SnapshotPixelFormat};
use rustc_hash::FxHashMap;
use servo_base::Epoch;
use servo_base::generic_channel::GenericSender;
use webgpu_traits::{
    ContextConfiguration, PRESENTATION_BUFFER_COUNT, PendingTexture, WebGPUContextId, WebGPUMsg,
};
use webrender_api::units::DeviceIntSize;
use webrender_api::{
    ExternalImageData, ExternalImageId, ExternalImageType, ImageDescriptor, ImageDescriptorFlags,
    ImageFormat, ImageKey,
};
use wgpu_core::device::HostMap;
use wgpu_core::global::Global;
use wgpu_core::id::{
    self, BufferId, CommandBufferId, CommandEncoderId, DeviceId, QueueId, TextureId,
};
use wgpu_core::resource::{
    BufferAccessError, BufferDescriptor, BufferMapOperation, CreateBufferError,
};
use wgpu_types::{
    BufferUsages, COPY_BYTES_PER_ROW_ALIGNMENT, CommandBufferDescriptor, CommandEncoderDescriptor,
    Extent3d, Origin3d, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo,
    TextureAspect,
};

pub type WebGpuExternalImageMap = Arc<Mutex<FxHashMap<WebGPUContextId, ContextData>>>;

const fn image_data(context_id: WebGPUContextId) -> ExternalImageData {
    ExternalImageData {
        id: ExternalImageId(context_id.0),
        channel_index: 0,
        image_type: ExternalImageType::Buffer,
        normalized_uvs: false,
    }
}

/// Allocated buffer on GPU device
#[derive(Clone, Copy, Debug)]
struct Buffer {
    device_id: DeviceId,
    queue_id: QueueId,
    size: u64,
}

impl Buffer {
    /// Returns true if buffer is compatible with provided configuration
    fn has_compatible_config(&self, config: &ContextConfiguration) -> bool {
        config.device_id == self.device_id && self.size == config.buffer_size()
    }
}

/// Mapped GPUBuffer
#[derive(Debug)]
struct MappedBuffer {
    buffer: Buffer,
    data: NonNull<u8>,
    len: u64,
    image_size: Size2D<u32>,
    image_format: ImageFormat,
    is_opaque: bool,
}

// Mapped buffer can be shared between safely (it's read-only)
unsafe impl Send for MappedBuffer {}

impl MappedBuffer {
    const fn slice(&'_ self) -> &'_ [u8] {
        // Safety: Pointer is from wgpu, and we only use it here
        unsafe { std::slice::from_raw_parts(self.data.as_ptr(), self.len as usize) }
    }

    fn stride(&self) -> u32 {
        (self.image_size.width * self.image_format.bytes_per_pixel() as u32)
            .next_multiple_of(COPY_BYTES_PER_ROW_ALIGNMENT)
    }
}

#[derive(Debug)]
enum StagingBufferState {
    /// The Initial state: the buffer has yet to be created with only an
    /// id reserved for it.
    Unassigned,
    /// The buffer is allocated in the WGPU Device and is ready to be used.
    Available(Buffer),
    /// `mapAsync` is currently running on the buffer.
    Mapping(Buffer),
    /// The buffer is currently mapped.
    Mapped(MappedBuffer),
}

/// A staging buffer used for texture to buffer to CPU copy operations.
#[derive(Debug)]
struct StagingBuffer {
    global: Arc<Global>,
    buffer_id: BufferId,
    state: StagingBufferState,
}

// [`StagingBuffer`] only used for reading (never for writing)
// so it is safe to share between threads.
unsafe impl Sync for StagingBuffer {}

impl StagingBuffer {
    fn new(global: Arc<Global>, buffer_id: BufferId) -> Self {
        Self {
            global,
            buffer_id,
            state: StagingBufferState::Unassigned,
        }
    }

    const fn is_mapped(&self) -> bool {
        matches!(self.state, StagingBufferState::Mapped(..))
    }

    /// Return true if buffer can be used directly with provided config
    /// without any additional work
    fn is_available_and_has_compatible_config(&self, config: &ContextConfiguration) -> bool {
        let StagingBufferState::Available(buffer) = &self.state else {
            return false;
        };
        buffer.has_compatible_config(config)
    }

    /// Return true if buffer is not mapping or being mapped
    const fn needs_assignment(&self) -> bool {
        matches!(
            self.state,
            StagingBufferState::Unassigned | StagingBufferState::Available(_)
        )
    }

    /// Make buffer available by unmapping / destroying it and then recreating it if needed.
    fn ensure_available(&mut self, config: &ContextConfiguration) -> Result<(), CreateBufferError> {
        let recreate = match &self.state {
            StagingBufferState::Unassigned => true,
            StagingBufferState::Available(buffer) |
            StagingBufferState::Mapping(buffer) |
            StagingBufferState::Mapped(MappedBuffer { buffer, .. }) => {
                if buffer.has_compatible_config(config) {
                    let _ = self.global.buffer_unmap(self.buffer_id);
                    false
                } else {
                    self.global.buffer_drop(self.buffer_id);
                    true
                }
            },
        };
        if recreate {
            let buffer_size = config.buffer_size();
            let (_, error) = self.global.device_create_buffer(
                config.device_id,
                &BufferDescriptor {
                    label: None,
                    size: buffer_size,
                    usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                },
                Some(self.buffer_id),
            );
            if let Some(error) = error {
                return Err(error);
            };
            self.state = StagingBufferState::Available(Buffer {
                device_id: config.device_id,
                queue_id: config.queue_id,
                size: buffer_size,
            });
        }
        Ok(())
    }

    /// Makes buffer available and prepares command encoder
    /// that will copy texture to this staging buffer.
    ///
    /// Caller must submit command buffer to queue.
    fn prepare_load_texture_command_buffer(
        &mut self,
        texture_id: TextureId,
        encoder_id: CommandEncoderId,
        command_buffer_id: CommandBufferId,
        config: &ContextConfiguration,
    ) -> Result<CommandBufferId, Box<dyn std::error::Error>> {
        self.ensure_available(config)?;
        let StagingBufferState::Available(buffer) = &self.state else {
            unreachable!("Should be made available by `ensure_available`")
        };
        let device_id = buffer.device_id;
        let command_descriptor = CommandEncoderDescriptor { label: None };
        let (encoder_id, error) = self.global.device_create_command_encoder(
            device_id,
            &command_descriptor,
            Some(encoder_id),
        );
        if let Some(error) = error {
            return Err(error.into());
        };
        let buffer_info = TexelCopyBufferInfo {
            buffer: self.buffer_id,
            layout: TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(config.stride()),
                rows_per_image: None,
            },
        };
        let texture_info = TexelCopyTextureInfo {
            texture: texture_id,
            mip_level: 0,
            origin: Origin3d::ZERO,
            aspect: TextureAspect::All,
        };
        let copy_size = Extent3d {
            width: config.size.width,
            height: config.size.height,
            depth_or_array_layers: 1,
        };
        self.global.command_encoder_copy_texture_to_buffer(
            encoder_id,
            &texture_info,
            &buffer_info,
            &copy_size,
        )?;
        let (command_buffer_id, error) = self.global.command_encoder_finish(
            encoder_id,
            &CommandBufferDescriptor::default(),
            Some(command_buffer_id),
        );
        if let Some((_, error)) = error {
            return Err(error.into());
        };
        Ok(command_buffer_id)
    }

    /// Unmaps the buffer or cancels a mapping operation if one is in progress.
    fn unmap(&mut self) {
        match self.state {
            StagingBufferState::Unassigned | StagingBufferState::Available(_) => {},
            StagingBufferState::Mapping(buffer) |
            StagingBufferState::Mapped(MappedBuffer { buffer, .. }) => {
                let _ = self.global.buffer_unmap(self.buffer_id);
                self.state = StagingBufferState::Available(buffer)
            },
        }
    }

    /// Obtain a snapshot from this buffer if is mapped or return `None` if it is not mapped.
    fn snapshot(&self) -> Option<Snapshot> {
        let StagingBufferState::Mapped(mapped) = &self.state else {
            return None;
        };
        let format = match mapped.image_format {
            ImageFormat::RGBA8 => SnapshotPixelFormat::RGBA,
            ImageFormat::BGRA8 => SnapshotPixelFormat::BGRA,
            _ => unreachable!("GPUCanvasContext does not support other formats per spec"),
        };
        let alpha_mode = if mapped.is_opaque {
            SnapshotAlphaMode::AsOpaque {
                premultiplied: false,
            }
        } else {
            SnapshotAlphaMode::Transparent {
                premultiplied: true,
            }
        };
        let padded_byte_width = mapped.stride();
        let data = mapped.slice();
        let bytes_per_pixel = mapped.image_format.bytes_per_pixel() as usize;
        let mut result_unpadded =
            Vec::<u8>::with_capacity(mapped.image_size.area() as usize * bytes_per_pixel);
        for row in 0..mapped.image_size.height {
            let start = (row * padded_byte_width).try_into().ok()?;
            result_unpadded
                .extend(&data[start..start + mapped.image_size.width as usize * bytes_per_pixel]);
        }
        let mut snapshot =
            Snapshot::from_vec(mapped.image_size, format, alpha_mode, result_unpadded);
        if mapped.is_opaque {
            snapshot.transform(SnapshotAlphaMode::Opaque, snapshot.format())
        }
        Some(snapshot)
    }
}

impl Drop for StagingBuffer {
    fn drop(&mut self) {
        match self.state {
            StagingBufferState::Unassigned => {},
            StagingBufferState::Available(_) |
            StagingBufferState::Mapping(_) |
            StagingBufferState::Mapped(_) => {
                self.global.buffer_drop(self.buffer_id);
            },
        }
    }
}

pub struct WebGpuExternalImages {
    pub image_map: WebGpuExternalImageMap,
    /// The compositor rendering context for this painter, used to import a GPU-direct shared
    /// texture as a GL texture (the per-painter device, like `WebGLExternalImages`).
    rendering_context: Rc<dyn RenderingContext>,
    /// This painter's GPU adapter LUID (from its wall-layout GPU index), used to pick the
    /// shared present texture from ITS GPU so the tile samples its own GPU's copy. `None` for
    /// non-wall painters → GPU-direct disabled, CPU readback used.
    #[allow(dead_code)] // read only on Windows.
    painter_luid: Option<(i32, u32)>,
    pub locked_ids: FxHashMap<WebGPUContextId, PresentationStagingBuffer>,
    /// GPU-direct: the imported GL surface textures currently locked, kept alive while the
    /// compositor samples them and destroyed on unlock.
    #[cfg(windows)]
    locked_textures: FxHashMap<WebGPUContextId, SurfaceTexture>,
}

impl WebGpuExternalImages {
    pub fn new(
        image_map: WebGpuExternalImageMap,
        rendering_context: Rc<dyn RenderingContext>,
        painter_luid: Option<(i32, u32)>,
    ) -> Self {
        Self {
            image_map,
            rendering_context,
            painter_luid,
            locked_ids: Default::default(),
            #[cfg(windows)]
            locked_textures: Default::default(),
        }
    }
}

impl WebRenderExternalImageApi for WebGpuExternalImages {
    fn lock(&mut self, id: u64) -> (ExternalImageSource<'_>, Size2D<i32>) {
        let id = WebGPUContextId(id);

        // GPU-direct: if this context has a shared present texture, import it on this
        // painter's device and sample it directly (no CPU readback). Falls back to the
        // CPU `RawData` path below if unavailable or if the import fails.
        #[cfg(windows)]
        {
            let handle_and_size = {
                let webgpu_contexts = self.image_map.lock().unwrap();
                webgpu_contexts
                    .get(&id)
                    .and_then(|context_data| {
                        context_data.gpu_direct_handle_and_size(self.painter_luid)
                    })
            };
            if let Some((handle, size)) = handle_and_size &&
                let Some((surface_texture, gl_texture, texture_size)) =
                    self.rendering_context.create_texture_from_shared_handle(handle, size)
            {
                self.locked_textures.insert(id, surface_texture);
                return (ExternalImageSource::NativeTexture(gl_texture), texture_size);
            }
        }

        let presentation = {
            let mut webgpu_contexts = self.image_map.lock().unwrap();
            webgpu_contexts
                .get_mut(&id)
                .and_then(|context_data| context_data.presentation.clone())
        };
        let Some(presentation) = presentation else {
            return (ExternalImageSource::Invalid, Size2D::zero());
        };
        self.locked_ids.insert(id, presentation);
        let presentation = self.locked_ids.get(&id).unwrap();
        let StagingBufferState::Mapped(mapped_buffer) = &presentation.staging_buffer.state else {
            unreachable!("Presentation staging buffer should be mapped")
        };
        let size = mapped_buffer.image_size;
        (
            ExternalImageSource::RawData(mapped_buffer.slice()),
            size.cast().cast_unit(),
        )
    }

    fn unlock(&mut self, id: u64) {
        let id = WebGPUContextId(id);

        // GPU-direct: release the imported GL texture for this context, if any.
        #[cfg(windows)]
        if let Some(surface_texture) = self.locked_textures.remove(&id) {
            self.rendering_context.destroy_texture(surface_texture);
            return;
        }

        let Some(presentation) = self.locked_ids.remove(&id) else {
            return;
        };
        let mut webgpu_contexts = self.image_map.lock().unwrap();
        if let Some(context_data) = webgpu_contexts.get_mut(&id) {
            // We use this to return staging buffer if a newer one exists.
            presentation.maybe_destroy(context_data);
        } else {
            // This will not free this buffer id in script,
            // but that's okay because we still have many free ids.
            drop(presentation);
        }
    }
}

/// Staging buffer currently used for presenting the epoch.
///
/// Users should [`ContextData::replace_presentation`] when done.
#[derive(Clone)]
pub struct PresentationStagingBuffer {
    epoch: Epoch,
    staging_buffer: Arc<StagingBuffer>,
}

impl PresentationStagingBuffer {
    fn new(epoch: Epoch, staging_buffer: StagingBuffer) -> Self {
        Self {
            epoch,
            staging_buffer: Arc::new(staging_buffer),
        }
    }

    /// If the internal staging buffer is not shared,
    /// unmap it and call [`ContextData::return_staging_buffer`] with it.
    fn maybe_destroy(self, context_data: &mut ContextData) {
        if let Some(mut staging_buffer) = Arc::into_inner(self.staging_buffer) {
            staging_buffer.unmap();
            context_data.return_staging_buffer(staging_buffer);
        }
    }
}

/// The embedder process-side representation of what is the `GPUCanvasContext` in script.
pub struct ContextData {
    /// The [`ImageKey`] of the WebRender image associated with this context.
    image_key: Option<ImageKey>,
    /// The current size of this context.
    size: DeviceIntSize,
    /// Staging buffers that are not actively used.
    ///
    /// Staging buffer here are either [`StagingBufferState::Unassigned`] or [`StagingBufferState::Available`].
    /// They are removed from here when they are in process of being mapped or are already mapped.
    inactive_staging_buffers: ArrayVec<StagingBuffer, PRESENTATION_BUFFER_COUNT>,
    /// The [`PresentationStagingBuffer`] of the most recent presentation. This will
    /// be `None` directly after initialization, as clearing is handled completely in
    /// the `ScriptThread`.
    presentation: Option<PresentationStagingBuffer>,
    /// Next epoch to be used
    next_epoch: Epoch,
    /// GPU-direct present (Phase 3): one shared D3D12 texture PER physical GPU (keyed by
    /// adapter LUID) that the canvas is GPU-copied into each frame, exported as a handle for
    /// each tile's compositor to sample its own GPU's copy directly (no CPU readback). Empty
    /// until first present with GPU-direct enabled.
    #[cfg(windows)]
    gpu_direct: FxHashMap<(i32, u32), crate::shared_present::SharedPresentTexture>,
    /// Monotonic frame counter used to mint fresh copy command ids each frame.
    #[cfg(windows)]
    gpu_direct_frame: u32,
}

impl ContextData {
    fn new(
        global: &Arc<Global>,
        buffer_ids: ArrayVec<id::BufferId, PRESENTATION_BUFFER_COUNT>,
        size: DeviceIntSize,
    ) -> Self {
        Self {
            image_key: None,
            size,
            inactive_staging_buffers: buffer_ids
                .iter()
                .map(|buffer_id| StagingBuffer::new(global.clone(), *buffer_id))
                .collect(),
            presentation: None,
            next_epoch: Epoch(1),
            #[cfg(windows)]
            gpu_direct: FxHashMap::default(),
            #[cfg(windows)]
            gpu_direct_frame: 0,
        }
    }

    /// GPU-direct: the shared present texture's NT handle (as `u64`) and size for the GPU with
    /// the given adapter LUID, if one exists for this context. Each tile's compositor looks up
    /// ITS GPU's handle so it samples its own GPU's copy (no cross-GPU transfer).
    #[cfg(windows)]
    fn gpu_direct_handle_and_size(
        &self,
        painter_luid: Option<(i32, u32)>,
    ) -> Option<(u64, Size2D<i32>)> {
        let luid = painter_luid?;
        self.gpu_direct.get(&luid).map(|shared| {
            (
                shared.shared_handle.0 as u64,
                Size2D::new(shared.width as i32, shared.height as i32),
            )
        })
    }

    /// Returns `None` if no staging buffer is unused or failure when making it available
    fn get_or_make_available_buffer(
        &'_ mut self,
        config: &ContextConfiguration,
    ) -> Option<StagingBuffer> {
        self.inactive_staging_buffers
            .iter()
            // Try to get first preallocated GPUBuffer.
            .position(|staging_buffer| {
                staging_buffer.is_available_and_has_compatible_config(config)
            })
            // Fall back to the first inactive staging buffer.
            .or_else(|| {
                self.inactive_staging_buffers
                    .iter()
                    .position(|staging_buffer| staging_buffer.needs_assignment())
            })
            // Or just the use first one.
            .or_else(|| {
                if self.inactive_staging_buffers.is_empty() {
                    None
                } else {
                    Some(0)
                }
            })
            .and_then(|index| {
                let mut staging_buffer = self.inactive_staging_buffers.remove(index);
                if staging_buffer.ensure_available(config).is_ok() {
                    Some(staging_buffer)
                } else {
                    // If we fail to make it available, return it to the list of inactive staging buffers.
                    self.inactive_staging_buffers.push(staging_buffer);
                    None
                }
            })
    }

    /// Destroy the context that this [`ContextData`] represents,
    /// freeing all of its buffers, and deleting the associated WebRender image.
    fn destroy(
        mut self,
        script_sender: &GenericSender<WebGPUMsg>,
        paint_api: &CrossProcessPaintApi,
    ) {
        // This frees the id in the `ScriptThread`.
        for staging_buffer in self.inactive_staging_buffers {
            if let Err(error) = script_sender.send(WebGPUMsg::FreeBuffer(staging_buffer.buffer_id))
            {
                warn!(
                    "Unable to send FreeBuffer({:?}) ({error})",
                    staging_buffer.buffer_id
                );
            };
        }
        if let Some(image_key) = self.image_key.take() {
            paint_api.delete_image(image_key);
        }
    }

    /// Advance the [`Epoch`] and return the new one.
    fn next_epoch(&mut self) -> Epoch {
        let epoch = self.next_epoch;
        self.next_epoch.next();
        epoch
    }

    /// If the given [`PresentationStagingBuffer`] is for a newer presentation, replace the existing
    /// one. Deallocate the older one by calling [`Self::return_staging_buffer`] on it.
    fn replace_presentation(&mut self, presentation: PresentationStagingBuffer) {
        let stale_presentation = if presentation.epoch >=
            self.presentation
                .as_ref()
                .map(|p| p.epoch)
                .unwrap_or_default()
        {
            self.presentation.replace(presentation)
        } else {
            Some(presentation)
        };
        if let Some(stale_presentation) = stale_presentation {
            stale_presentation.maybe_destroy(self);
        }
    }

    fn clear_presentation(&mut self) {
        if let Some(stale_presentation) = self.presentation.take() {
            stale_presentation.maybe_destroy(self);
        }
    }

    fn return_staging_buffer(&mut self, staging_buffer: StagingBuffer) {
        self.inactive_staging_buffers.push(staging_buffer)
    }
}

impl crate::WGPU {
    pub(crate) fn create_context(
        &self,
        context_id: WebGPUContextId,
        size: DeviceIntSize,
        buffer_ids: ArrayVec<id::BufferId, PRESENTATION_BUFFER_COUNT>,
    ) {
        let context_data = ContextData::new(&self.global, buffer_ids, size);
        assert!(
            self.wgpu_image_map
                .lock()
                .unwrap()
                .insert(context_id, context_data)
                .is_none(),
            "Context should be created only once!"
        );
    }

    pub(crate) fn set_image_key(&self, context_id: WebGPUContextId, image_key: ImageKey) {
        let mut webgpu_contexts = self.wgpu_image_map.lock().unwrap();
        let context_data = webgpu_contexts.get_mut(&context_id).unwrap();

        if let Some(old_image_key) = context_data.image_key.replace(image_key) {
            self.paint_api.delete_image(old_image_key);
        }

        self.paint_api.add_image(
            image_key,
            ImageDescriptor {
                format: ImageFormat::BGRA8,
                size: context_data.size,
                stride: None,
                offset: 0,
                flags: ImageDescriptorFlags::empty(),
            },
            SerializableImageData::External(image_data(context_id)),
            false,
        );
    }

    pub(crate) fn get_image(
        &self,
        context_id: WebGPUContextId,
        pending_texture: Option<PendingTexture>,
        sender: GenericSender<SharedSnapshot>,
    ) {
        let mut webgpu_contexts = self.wgpu_image_map.lock().unwrap();
        let context_data = webgpu_contexts.get_mut(&context_id).unwrap();
        if let Some(PendingTexture {
            texture_id,
            encoder_id,
            command_buffer_id,
            configuration,
        }) = pending_texture
        {
            let Some(staging_buffer) = context_data.get_or_make_available_buffer(&configuration)
            else {
                warn!("Failure obtaining available staging buffer");
                sender
                    .send(SharedSnapshot::cleared(configuration.size))
                    .unwrap();
                return;
            };

            let epoch = context_data.next_epoch();
            let wgpu_image_map = self.wgpu_image_map.clone();
            let sender = sender;
            drop(webgpu_contexts);
            self.texture_download(
                texture_id,
                encoder_id,
                command_buffer_id,
                staging_buffer,
                configuration,
                move |staging_buffer| {
                    let mut webgpu_contexts = wgpu_image_map.lock().unwrap();
                    let context_data = webgpu_contexts.get_mut(&context_id).unwrap();
                    sender
                        .send(
                            staging_buffer
                                .snapshot()
                                .as_ref()
                                .map(Snapshot::to_shared)
                                .unwrap_or_else(|| SharedSnapshot::cleared(configuration.size)),
                        )
                        .unwrap();
                    if staging_buffer.is_mapped() {
                        context_data.replace_presentation(PresentationStagingBuffer::new(
                            epoch,
                            staging_buffer,
                        ));
                    } else {
                        // failure
                        context_data.return_staging_buffer(staging_buffer);
                    }
                },
            );
        } else {
            sender
                .send(
                    context_data
                        .presentation
                        .as_ref()
                        .and_then(|presentation_staging_buffer| {
                            presentation_staging_buffer.staging_buffer.snapshot()
                        })
                        .unwrap_or_else(Snapshot::empty)
                        .to_shared(),
                )
                .unwrap();
        }
    }

    /// GPU-direct present (Phase 3): for every physical GPU (the primary plus each secondary
    /// fan-out global), ensure a shared present texture exists and GPU-copy the canvas into it.
    /// Each tile's compositor then samples ITS GPU's copy directly (no cross-GPU transfer, no
    /// CPU readback). Best-effort: a GPU that fails simply has no entry and falls back to the
    /// CPU readback path for its tile.
    #[cfg(windows)]
    fn gpu_direct_copy(
        &self,
        context_data: &mut ContextData,
        context_id: WebGPUContextId,
        canvas_texture_id: id::TextureId,
        config: &ContextConfiguration,
    ) {
        let size = Extent3d {
            width: config.size.width,
            height: config.size.height,
            depth_or_array_layers: 1,
        };
        let frame = context_data.gpu_direct_frame;
        if let Some(primary_luid) = self.primary_luid {
            self.gpu_direct_copy_one(
                context_data,
                context_id,
                primary_luid,
                &self.global,
                canvas_texture_id,
                config,
                size,
                frame,
            );
        }
        for secondary in &self.secondary_gpus {
            self.gpu_direct_copy_one(
                context_data,
                context_id,
                secondary.target_luid,
                &secondary.global,
                canvas_texture_id,
                config,
                size,
                frame,
            );
        }
        context_data.gpu_direct_frame = frame.wrapping_add(1);
    }

    /// Ensure + copy the canvas into the shared present texture for ONE GPU global (keyed by
    /// `luid`). The canvas texture exists on every global (mirrored by the command fan-out),
    /// so the copy source id is the same on each; the destination is that GPU's own shared
    /// texture.
    #[cfg(windows)]
    #[allow(clippy::too_many_arguments)]
    fn gpu_direct_copy_one(
        &self,
        context_data: &mut ContextData,
        context_id: WebGPUContextId,
        luid: (i32, u32),
        global: &Arc<Global>,
        canvas_texture_id: id::TextureId,
        config: &ContextConfiguration,
        size: Extent3d,
        frame: u32,
    ) {
        let needs_create = context_data
            .gpu_direct
            .get(&luid)
            .is_none_or(|shared| shared.width != size.width || shared.height != size.height);
        if needs_create {
            // Drop the previous shared texture (on resize) to free its GPU memory.
            if let Some(old) = context_data.gpu_direct.remove(&luid) {
                global.texture_drop(old.texture_id);
            }
            let texture_id = crate::shared_present::next_shared_texture_id();
            if let Some(shared) = crate::shared_present::create_shared_present_texture(
                global,
                config.device_id,
                texture_id,
                size.width,
                size.height,
            ) {
                log::info!(
                    "GPU-direct: created shared present texture {}x{} for {context_id:?} on \
                     GPU LUID {luid:?}",
                    size.width,
                    size.height
                );
                context_data.gpu_direct.insert(luid, shared);
            }
        }
        if let Some(shared) = context_data.gpu_direct.get(&luid) {
            let shared_texture_id = shared.texture_id;
            crate::shared_present::copy_canvas_to_shared(
                global,
                config.device_id,
                config.queue_id,
                canvas_texture_id,
                shared_texture_id,
                size,
                frame,
            );
        }
    }

    /// Read the texture to the staging buffer, map it to CPU memory, and update the
    /// image in WebRender when complete.
    pub(crate) fn present(
        &self,
        context_id: WebGPUContextId,
        pending_texture: Option<PendingTexture>,
        size: Size2D<u32>,
        canvas_epoch: Epoch,
    ) {
        let mut webgpu_contexts = self.wgpu_image_map.lock().unwrap();
        let context_data = webgpu_contexts.get_mut(&context_id).unwrap();

        let Some(image_key) = context_data.image_key else {
            return;
        };

        let Some(PendingTexture {
            texture_id,
            encoder_id,
            command_buffer_id,
            configuration,
        }) = pending_texture
        else {
            context_data.clear_presentation();
            self.paint_api.update_image(
                image_key,
                ImageDescriptor {
                    format: ImageFormat::BGRA8,
                    size: size.cast_unit().cast(),
                    stride: None,
                    offset: 0,
                    flags: ImageDescriptorFlags::empty(),
                },
                SerializableImageData::External(image_data(context_id)),
                Some(canvas_epoch),
            );
            return;
        };
        let Some(staging_buffer) = context_data.get_or_make_available_buffer(&configuration) else {
            warn!("Failure obtaining available staging buffer");
            context_data.clear_presentation();
            self.paint_api.update_image(
                image_key,
                configuration.into(),
                SerializableImageData::External(image_data(context_id)),
                Some(canvas_epoch),
            );
            return;
        };
        let epoch = context_data.next_epoch();
        // GPU-direct present (Phase 3): also GPU-copy the canvas into a shared texture on the
        // primary GPU so the compositor can later sample it directly. Additive to the CPU
        // readback below, which still drives the display until the compositor side is wired.
        #[cfg(windows)]
        if self.multigpu_fanout && self.gpu_direct_present {
            self.gpu_direct_copy(context_data, context_id, texture_id, &configuration);
        }
        let wgpu_image_map = self.wgpu_image_map.clone();
        let paint_api = self.paint_api.clone();
        drop(webgpu_contexts);
        self.texture_download(
            texture_id,
            encoder_id,
            command_buffer_id,
            staging_buffer,
            configuration,
            move |staging_buffer| {
                let mut webgpu_contexts = wgpu_image_map.lock().unwrap();
                let context_data = webgpu_contexts.get_mut(&context_id).unwrap();
                if staging_buffer.is_mapped() {
                    context_data.replace_presentation(PresentationStagingBuffer::new(
                        epoch,
                        staging_buffer,
                    ));
                } else {
                    context_data.return_staging_buffer(staging_buffer);
                    context_data.clear_presentation();
                }
                // update image in WR
                paint_api.update_image(
                    image_key,
                    configuration.into(),
                    SerializableImageData::External(image_data(context_id)),
                    Some(canvas_epoch),
                );
            },
        );
    }

    /// Copies data from provided texture using `encoder_id` to the provided [`StagingBuffer`].
    ///
    /// `callback` is guaranteed to be called.
    ///
    /// Returns a [`StagingBuffer`] with the [`StagingBufferState::Mapped`] state
    /// on success or [`StagingBufferState::Available`] on failure.
    fn texture_download(
        &self,
        texture_id: TextureId,
        encoder_id: CommandEncoderId,
        command_buffer_id: CommandBufferId,
        mut staging_buffer: StagingBuffer,
        config: ContextConfiguration,
        callback: impl FnOnce(StagingBuffer) + Send + 'static,
    ) {
        let Ok(command_buffer_id) = staging_buffer.prepare_load_texture_command_buffer(
            texture_id,
            encoder_id,
            command_buffer_id,
            &config,
        ) else {
            return callback(staging_buffer);
        };
        let StagingBufferState::Available(buffer) = &staging_buffer.state else {
            unreachable!("`prepare_load_texture_command_buffer` should make buffer available")
        };
        let buffer_id = staging_buffer.buffer_id;
        let buffer_size = buffer.size;
        {
            let _guard = self.poller.lock();
            let result = self
                .global
                .queue_submit(buffer.queue_id, &[command_buffer_id]);
            if result.is_err() {
                return callback(staging_buffer);
            }
        }
        staging_buffer.state = match staging_buffer.state {
            StagingBufferState::Available(buffer) => StagingBufferState::Mapping(buffer),
            _ => unreachable!("`prepare_load_texture_command_buffer` should make buffer available"),
        };
        let map_callback = {
            let token = self.poller.token();
            Box::new(move |result: Result<(), BufferAccessError>| {
                drop(token);
                staging_buffer.state = match staging_buffer.state {
                    StagingBufferState::Mapping(buffer) => {
                        if let Ok((data, len)) = result.and_then(|_| {
                            staging_buffer.global.buffer_get_mapped_range(
                                staging_buffer.buffer_id,
                                0,
                                Some(buffer.size),
                            )
                        }) {
                            StagingBufferState::Mapped(MappedBuffer {
                                buffer,
                                data,
                                len,
                                image_size: config.size,
                                image_format: config.format,
                                is_opaque: config.is_opaque,
                            })
                        } else {
                            StagingBufferState::Available(buffer)
                        }
                    },
                    _ => {
                        unreachable!("Mapping buffer should have StagingBufferState::Mapping state")
                    },
                };
                callback(staging_buffer);
            })
        };
        let map_op = BufferMapOperation {
            host: HostMap::Read,
            callback: Some(map_callback),
        };
        // error is handled by map_callback
        let _ = self
            .global
            .buffer_map_async(buffer_id, 0, Some(buffer_size), map_op);
        self.poller.wake();
    }

    pub(crate) fn destroy_context(&mut self, context_id: WebGPUContextId) {
        self.wgpu_image_map
            .lock()
            .unwrap()
            .remove(&context_id)
            .unwrap()
            .destroy(&self.script_sender, &self.paint_api);
    }
}

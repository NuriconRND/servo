/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Data and main loop of WebGPU thread.

use std::borrow::Cow;
use std::slice;
use std::sync::{Arc, Mutex};

use log::{info, warn};
use paint_api::{CrossProcessPaintApi, WebRenderExternalImageIdManager, WebRenderImageHandlerType};
use rustc_hash::FxHashMap;
use servo_base::generic_channel::{GenericReceiver, GenericSender, GenericSharedMemory};
use servo_base::id::PipelineId;
use servo_config::pref;
use webgpu_traits::{
    Adapter, ComputePassId, DeviceLostReason, Error, ErrorScope, Mapping, Pipeline, PopError,
    RenderPassId, ShaderCompilationInfo, WebGPU, WebGPUAdapter, WebGPUContextId, WebGPUDevice,
    WebGPUMsg, WebGPUQueue, WebGPURequest, apply_render_command,
};
use webrender_api::ExternalImageId;
use wgc::command::{ComputePass, ComputePassDescriptor, RenderPass};
use wgc::device::DeviceDescriptor;
use wgc::id;
use wgc::id::DeviceId;
use wgc::pipeline::ShaderModuleDescriptor;
use wgc::resource::BufferMapOperation;
pub use wgpu_core as wgc;
use wgpu_core::command::RenderPassDescriptor;
use wgpu_core::resource::BufferAccessResult;
pub use wgpu_types as wgt;
use wgpu_types::error::WebGpuError;
use wgpu_types::{ExperimentalFeatures, MemoryHints};
use wgt::InstanceDescriptor;

use crate::canvas_context::WebGpuExternalImageMap;
use crate::poll_thread::Poller;

#[derive(Eq, Hash, PartialEq)]
pub(crate) struct DeviceScope {
    pub device_id: DeviceId,
    pub pipeline_id: PipelineId,
    /// <https://www.w3.org/TR/webgpu/#dom-gpudevice-errorscopestack-slot>
    ///
    /// Is `None` if device is lost
    pub error_scope_stack: Option<Vec<ErrorScope>>,
    // TODO:
    // Queue for this device (to remove transmutes)
    // queue_id: QueueId,
    // Poller for this device
    // poller: Poller,
}

impl DeviceScope {
    pub fn new(device_id: DeviceId, pipeline_id: PipelineId) -> Self {
        Self {
            device_id,
            pipeline_id,
            error_scope_stack: Some(Vec::new()),
        }
    }
}

/// A dedicated wgpu [`wgc::global::Global`] bound to one additional physical GPU, used to
/// mirror ("fan out") the page's WebGPU work so each tile of a multi-GPU wall can run its
/// content on its own GPU. Each secondary global has independent id registries, so the
/// script-provided resource ids are reused verbatim on it (no id translation needed).
///
/// Phase 1 only creates the per-GPU mirror device. Command replay (Phase 2) and per-tile
/// present (Phase 3) build on this foundation.
pub(crate) struct SecondaryGpu {
    /// The dedicated wgpu instance/registries bound to one additional GPU.
    pub(crate) global: Arc<wgc::global::Global>,
    /// DXGI adapter LUID (HighPart, LowPart) of the physical GPU this global targets.
    pub(crate) target_luid: (i32, u32),
    /// Adapter id (internally allocated within `global`) for this GPU.
    adapter_id: id::AdapterId,
    /// Poller for this global's async work (buffer maps, submitted-work-done).
    #[allow(dead_code)]
    poller: Poller,
}

#[expect(clippy::upper_case_acronyms)] // Name of the library
pub(crate) struct WGPU {
    receiver: GenericReceiver<WebGPURequest>,
    sender: GenericSender<WebGPURequest>,
    pub(crate) script_sender: GenericSender<WebGPUMsg>,
    pub(crate) global: Arc<wgc::global::Global>,
    devices: Arc<Mutex<FxHashMap<DeviceId, DeviceScope>>>,
    /// Whether multi-GPU wall fan-out is enabled (pref `dom_webgpu_multigpu_fanout`).
    pub(crate) multigpu_fanout: bool,
    /// Whether GPU-direct present is enabled (pref `dom_webgpu_gpu_direct`); copies the canvas
    /// into a shared texture each frame for the compositor to sample without CPU readback.
    pub(crate) gpu_direct_present: bool,
    /// Whether [`WGPU::ensure_secondary_gpus`] has already run.
    fanout_initialized: bool,
    /// DXGI LUID of the page's primary adapter (set during fan-out init), used to key the
    /// primary GPU's GPU-direct shared texture.
    pub(crate) primary_luid: Option<(i32, u32)>,
    /// One [`SecondaryGpu`] per additional physical GPU (beyond the page's primary adapter).
    pub(crate) secondary_gpus: Vec<SecondaryGpu>,
    pub(crate) paint_api: CrossProcessPaintApi,
    pub(crate) webrender_external_image_id_manager: WebRenderExternalImageIdManager,
    pub(crate) wgpu_image_map: WebGpuExternalImageMap,
    /// Provides access to poller thread
    pub(crate) poller: Poller,
    /// Store compute passes
    compute_passes: FxHashMap<ComputePassId, ComputePass>,
    /// Store render passes
    render_passes: FxHashMap<RenderPassId, RenderPass>,
    /// Per-secondary-GPU mirror of each compute pass, aligned to [`Self::secondary_gpus`]
    /// order. Used only when multi-GPU fan-out is active.
    secondary_compute_passes: FxHashMap<ComputePassId, Vec<ComputePass>>,
    /// Per-secondary-GPU mirror of each render pass, aligned to [`Self::secondary_gpus`] order.
    secondary_render_passes: FxHashMap<RenderPassId, Vec<RenderPass>>,
}

impl WGPU {
    pub(crate) fn new(
        receiver: GenericReceiver<WebGPURequest>,
        sender: GenericSender<WebGPURequest>,
        script_sender: GenericSender<WebGPUMsg>,
        paint_api: CrossProcessPaintApi,
        webrender_external_image_id_manager: WebRenderExternalImageIdManager,
        wgpu_image_map: WebGpuExternalImageMap,
    ) -> Self {
        let global = Arc::new(wgc::global::Global::new(
            "wgpu-core",
            Self::build_instance_descriptor(),
            None,
        ));
        WGPU {
            poller: Poller::new(Arc::clone(&global)),
            receiver,
            sender,
            script_sender,
            global,
            devices: Arc::new(Mutex::new(FxHashMap::default())),
            multigpu_fanout: pref!(dom_webgpu_multigpu_fanout),
            gpu_direct_present: pref!(dom_webgpu_gpu_direct),
            fanout_initialized: false,
            primary_luid: None,
            secondary_gpus: Vec::new(),
            paint_api,
            webrender_external_image_id_manager,
            wgpu_image_map,
            compute_passes: FxHashMap::default(),
            render_passes: FxHashMap::default(),
            secondary_compute_passes: FxHashMap::default(),
            secondary_render_passes: FxHashMap::default(),
        }
    }

    /// Build the wgpu [`InstanceDescriptor`] used for both the primary global and every
    /// per-GPU secondary global. Kept identical so secondary globals enumerate the same
    /// backends/adapters as the page's primary instance.
    fn build_instance_descriptor() -> InstanceDescriptor {
        let backend_pref = pref!(dom_webgpu_wgpu_backend);
        let backends = if !backend_pref.is_empty() {
            info!(
                "Selecting backends based on dom.webgpu.wgpu_backend pref: {:?}",
                backend_pref
            );
            wgt::Backends::from_comma_list(&backend_pref)
        } else if pref!(dom_webgpu_multigpu_fanout) {
            // Fan-out matches adapters to physical GPUs by DXGI LUID, which is DX12-only.
            // Force the DX12 backend so the page's primary device is also LUID-matchable
            // (otherwise wgpu may pick Vulkan first and its LUID can't be read).
            info!("WebGPU multi-GPU fan-out: forcing DX12 backend for LUID-based GPU matching");
            wgt::Backends::DX12
        } else {
            wgt::Backends::PRIMARY
        };
        InstanceDescriptor {
            backends,
            backend_options: wgt::BackendOptions {
                gl: wgt::GlBackendOptions {
                    gles_minor_version: wgt::Gles3MinorVersion::Automatic,
                    fence_behavior: wgt::GlFenceBehavior::Normal,
                    debug_fns: wgt::GlDebugFns::Auto,
                },
                dx12: wgt::Dx12BackendOptions {
                    ..Default::default()
                },
                noop: wgt::NoopBackendOptions::default(),
            },

            flags: wgt::InstanceFlags::from_build_config() |
                wgt::InstanceFlags::AUTOMATIC_TIMESTAMP_NORMALIZATION,
            // TODO(sagudev): firefox actually sets this, but it can cause OOM for us
            // meaning that we are likely leaking something
            memory_budget_thresholds: wgt::MemoryBudgetThresholds {
                for_resource_creation: Some(95),
                for_device_loss: Some(99),
            },
            display: None,
        }
    }

    /// Read the DXGI adapter LUID `(HighPart, LowPart)` for an adapter in `global`.
    ///
    /// Returns `None` for non-DX12 adapters or non-Windows builds. The LUID is the
    /// stable per-physical-GPU key (same scheme surfman/ANGLE uses for the WebGL
    /// fan-out), so it distinguishes even two identical GPUs.
    #[cfg(windows)]
    fn adapter_luid(global: &wgc::global::Global, adapter_id: id::AdapterId) -> Option<(i32, u32)> {
        // SAFETY: we only read the adapter's DXGI desc; the handle is not retained or mutated.
        let hal_adapter = unsafe { global.adapter_as_hal::<wgc::api::Dx12>(adapter_id) }?;
        let desc = unsafe { hal_adapter.raw_adapter().GetDesc1() }.ok()?;
        let luid = desc.AdapterLuid;
        Some((luid.HighPart, luid.LowPart))
    }

    #[cfg(not(windows))]
    fn adapter_luid(
        _global: &wgc::global::Global,
        _adapter_id: id::AdapterId,
    ) -> Option<(i32, u32)> {
        None
    }

    /// Lazily create one dedicated [`SecondaryGpu`] per additional physical GPU.
    ///
    /// Triggered on the first device request, once we know which physical GPU the page's
    /// primary adapter sits on. Each secondary GPU gets its own wgpu global whose adapter
    /// registry uses only internal allocation (via `enumerate_adapters`), so it never
    /// clashes with the script-driven external ids on the primary global.
    fn ensure_secondary_gpus(&mut self, primary_adapter_id: id::AdapterId) {
        if !self.multigpu_fanout || self.fanout_initialized {
            return;
        }
        self.fanout_initialized = true;

        #[cfg(windows)]
        {
            let Some(primary_luid) = Self::adapter_luid(&self.global, primary_adapter_id) else {
                warn!(
                    "WebGPU multi-GPU fan-out: could not read primary adapter LUID (not a DX12 \
                     adapter?); skipping fan-out to avoid duplicating work onto the primary GPU"
                );
                return;
            };
            info!("WebGPU multi-GPU fan-out: primary adapter LUID = {primary_luid:?}");
            self.primary_luid = Some(primary_luid);

            // Discover the distinct non-primary GPU LUIDs using a throwaway global. Its
            // adapter registry is internal-allocation only, so probing it never mixes ids
            // with the script-driven primary global. Only fan out to *discrete* GPUs:
            // skip the software (WARP), integrated, and virtual adapters DXGI also enumerates,
            // and skip the primary GPU itself.
            let probe = wgc::global::Global::new(
                "wgpu-core-probe",
                Self::build_instance_descriptor(),
                None,
            );
            let mut wanted_luids: Vec<(i32, u32)> = Vec::new();
            for adapter_id in probe.enumerate_adapters(wgt::Backends::DX12) {
                if probe.adapter_get_info(adapter_id).device_type !=
                    wgt::DeviceType::DiscreteGpu
                {
                    continue;
                }
                if let Some(luid) = Self::adapter_luid(&probe, adapter_id) {
                    if luid == primary_luid || wanted_luids.contains(&luid) {
                        continue;
                    }
                    wanted_luids.push(luid);
                }
            }

            for luid in wanted_luids {
                let global = Arc::new(wgc::global::Global::new(
                    "wgpu-core-secondary",
                    Self::build_instance_descriptor(),
                    None,
                ));
                let mut chosen_adapter = None;
                for adapter_id in global.enumerate_adapters(wgt::Backends::DX12) {
                    if Self::adapter_luid(&global, adapter_id) == Some(luid) {
                        chosen_adapter = Some(adapter_id);
                        break;
                    }
                }
                match chosen_adapter {
                    Some(adapter_id) => {
                        let poller = Poller::new(Arc::clone(&global));
                        self.secondary_gpus.push(SecondaryGpu {
                            global,
                            target_luid: luid,
                            adapter_id,
                            poller,
                        });
                        info!(
                            "WebGPU multi-GPU fan-out: initialized secondary GPU (LUID {luid:?})"
                        );
                    },
                    None => {
                        warn!(
                            "WebGPU multi-GPU fan-out: no DX12 adapter found for LUID {luid:?}"
                        );
                    },
                }
            }
            info!(
                "WebGPU multi-GPU fan-out: {} secondary GPU(s) ready",
                self.secondary_gpus.len()
            );
        }
        #[cfg(not(windows))]
        {
            let _ = primary_adapter_id;
            warn!("WebGPU multi-GPU fan-out is only supported on Windows/DX12");
        }
    }

    /// Apply `f` to the mirror of compute pass `pass_id` on every secondary GPU global.
    /// No-op when fan-out is inactive. Errors on secondary globals are intentionally
    /// ignored (their output is not presented yet; only the primary drives the page).
    fn replay_secondary_compute<F>(&mut self, pass_id: ComputePassId, mut f: F)
    where
        F: FnMut(&wgc::global::Global, &mut ComputePass),
    {
        if let Some(passes) = self.secondary_compute_passes.get_mut(&pass_id) {
            for (secondary, pass) in self.secondary_gpus.iter().zip(passes.iter_mut()) {
                f(&secondary.global, pass);
            }
        }
    }

    /// Apply `f` to the mirror of render pass `pass_id` on every secondary GPU global.
    fn replay_secondary_render<F>(&mut self, pass_id: RenderPassId, mut f: F)
    where
        F: FnMut(&wgc::global::Global, &mut RenderPass),
    {
        if let Some(passes) = self.secondary_render_passes.get_mut(&pass_id) {
            for (secondary, pass) in self.secondary_gpus.iter().zip(passes.iter_mut()) {
                f(&secondary.global, pass);
            }
        }
    }

    pub(crate) fn run(&mut self) {
        loop {
            if let Ok(msg) = self.receiver.recv() {
                log::trace!("recv: {msg:?}");
                match msg {
                    WebGPURequest::SetImageKey {
                        context_id,
                        image_key,
                    } => self.set_image_key(context_id, image_key),
                    WebGPURequest::BufferMapAsync {
                        callback: sender,
                        buffer_id,
                        device_id,
                        host_map,
                        offset,
                        size,
                    } => {
                        let glob = Arc::clone(&self.global);
                        let resp_sender = sender.clone();
                        let token = self.poller.token();
                        let callback = Box::from(move |result: BufferAccessResult| {
                            drop(token);
                            let response = result.and_then(|_| {
                                let global = &glob;
                                let (slice_pointer, range_size) =
                                    global.buffer_get_mapped_range(buffer_id, offset, size)?;
                                // SAFETY: guarantee to be safe from wgpu
                                let data = unsafe {
                                    slice::from_raw_parts(
                                        slice_pointer.as_ptr(),
                                        range_size as usize,
                                    )
                                };

                                Ok(Mapping {
                                    data: GenericSharedMemory::from_bytes(data),
                                    range: offset..offset + range_size,
                                    mode: host_map,
                                })
                            });
                            if let Err(e) = resp_sender.send(response) {
                                warn!("Could not send BufferMapAsync Response ({})", e);
                            }
                        });

                        let operation = BufferMapOperation {
                            host: host_map,
                            callback: Some(callback),
                        };
                        let global = &self.global;
                        let result = global.buffer_map_async(buffer_id, offset, size, operation);
                        self.poller.wake();
                        // Per spec we also need to raise validation error here
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::CommandEncoderFinish {
                        command_encoder_id,
                        device_id,
                        desc,
                        command_buffer_id,
                    } => {
                        let global = &self.global;
                        let (_, error) = global.command_encoder_finish(
                            command_encoder_id,
                            &desc,
                            Some(command_buffer_id),
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.command_encoder_finish(
                                command_encoder_id,
                                &desc,
                                Some(command_buffer_id),
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, error.map(|(_, e)| e));
                    },
                    WebGPURequest::CopyBufferToBuffer {
                        device_id,
                        command_encoder_id,
                        source_id,
                        source_offset,
                        destination_id,
                        destination_offset,
                        size,
                    } => {
                        let global = &self.global;
                        let result = global.command_encoder_copy_buffer_to_buffer(
                            command_encoder_id,
                            source_id,
                            source_offset,
                            destination_id,
                            destination_offset,
                            Some(size),
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.command_encoder_copy_buffer_to_buffer(
                                command_encoder_id,
                                source_id,
                                source_offset,
                                destination_id,
                                destination_offset,
                                Some(size),
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::ResolveQuerySet {
                        device_id,
                        command_encoder_id,
                        query_set_id,
                        first_query,
                        query_count,
                        destination_id,
                        destination_offset,
                    } => {
                        let global = &self.global;
                        let result = global.command_encoder_resolve_query_set(
                            command_encoder_id,
                            query_set_id,
                            first_query,
                            query_count,
                            destination_id,
                            destination_offset,
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.command_encoder_resolve_query_set(
                                command_encoder_id,
                                query_set_id,
                                first_query,
                                query_count,
                                destination_id,
                                destination_offset,
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::CopyBufferToTexture {
                        device_id,
                        command_encoder_id,
                        source,
                        destination,
                        copy_size,
                    } => {
                        let global = &self.global;
                        let result = global.command_encoder_copy_buffer_to_texture(
                            command_encoder_id,
                            &source,
                            &destination,
                            &copy_size,
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.command_encoder_copy_buffer_to_texture(
                                command_encoder_id,
                                &source,
                                &destination,
                                &copy_size,
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::CopyTextureToBuffer {
                        device_id,
                        command_encoder_id,
                        source,
                        destination,
                        copy_size,
                    } => {
                        let global = &self.global;
                        let result = global.command_encoder_copy_texture_to_buffer(
                            command_encoder_id,
                            &source,
                            &destination,
                            &copy_size,
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.command_encoder_copy_texture_to_buffer(
                                command_encoder_id,
                                &source,
                                &destination,
                                &copy_size,
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::CopyTextureToTexture {
                        device_id,
                        command_encoder_id,
                        source,
                        destination,
                        copy_size,
                    } => {
                        let global = &self.global;
                        let result = global.command_encoder_copy_texture_to_texture(
                            command_encoder_id,
                            &source,
                            &destination,
                            &copy_size,
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.command_encoder_copy_texture_to_texture(
                                command_encoder_id,
                                &source,
                                &destination,
                                &copy_size,
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::CreateBindGroup {
                        device_id,
                        bind_group_id,
                        descriptor,
                    } => {
                        let global = &self.global;
                        let (_, error) = global.device_create_bind_group(
                            device_id,
                            &descriptor,
                            Some(bind_group_id),
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.device_create_bind_group(
                                device_id,
                                &descriptor,
                                Some(bind_group_id),
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                    WebGPURequest::CreateBindGroupLayout {
                        device_id,
                        bind_group_layout_id,
                        descriptor,
                    } => {
                        let global = &self.global;
                        if let Some(desc) = descriptor {
                            let (_, error) = global.device_create_bind_group_layout(
                                device_id,
                                &desc,
                                Some(bind_group_layout_id),
                            );
                            for secondary in &self.secondary_gpus {
                                let _ = secondary.global.device_create_bind_group_layout(
                                    device_id,
                                    &desc,
                                    Some(bind_group_layout_id),
                                );
                            }
                            self.maybe_dispatch_wgpu_error(device_id, error);
                        }
                    },
                    WebGPURequest::CreateBuffer {
                        device_id,
                        buffer_id,
                        descriptor,
                    } => {
                        let global = &self.global;
                        let (_, error) =
                            global.device_create_buffer(device_id, &descriptor, Some(buffer_id));
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.device_create_buffer(
                                device_id,
                                &descriptor,
                                Some(buffer_id),
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                    WebGPURequest::CreateCommandEncoder {
                        device_id,
                        command_encoder_id,
                        desc,
                    } => {
                        let global = &self.global;
                        let (_, error) = global.device_create_command_encoder(
                            device_id,
                            &desc,
                            Some(command_encoder_id),
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.device_create_command_encoder(
                                device_id,
                                &desc,
                                Some(command_encoder_id),
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                    WebGPURequest::CreateComputePipeline {
                        device_id,
                        compute_pipeline_id,
                        descriptor,
                        async_sender: sender,
                    } => {
                        let global = &self.global;
                        let (_, error) = global.device_create_compute_pipeline(
                            device_id,
                            &descriptor,
                            Some(compute_pipeline_id),
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.device_create_compute_pipeline(
                                device_id,
                                &descriptor,
                                Some(compute_pipeline_id),
                            );
                        }
                        if let Some(sender) = sender {
                            let res = match error.and_then(Error::from_wgpu_error) {
                                // if device is lost we must return pipeline and not raise any error
                                None => Ok(Pipeline {
                                    id: compute_pipeline_id,
                                    label: descriptor.label.unwrap_or_default().to_string(),
                                }),
                                Some(e) => Err(e),
                            };
                            if let Err(e) = sender.send(res) {
                                warn!("Failed sending WebGPUComputePipelineResponse {e:?}");
                            }
                        } else {
                            self.maybe_dispatch_wgpu_error(device_id, error);
                        }
                    },
                    WebGPURequest::CreatePipelineLayout {
                        device_id,
                        pipeline_layout_id,
                        descriptor,
                    } => {
                        let global = &self.global;
                        let (_, error) = global.device_create_pipeline_layout(
                            device_id,
                            &descriptor,
                            Some(pipeline_layout_id),
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.device_create_pipeline_layout(
                                device_id,
                                &descriptor,
                                Some(pipeline_layout_id),
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                    WebGPURequest::CreateRenderPipeline {
                        device_id,
                        render_pipeline_id,
                        descriptor,
                        async_sender: sender,
                    } => {
                        let global = &self.global;
                        let (_, error) = global.device_create_render_pipeline(
                            device_id,
                            &descriptor,
                            Some(render_pipeline_id),
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.device_create_render_pipeline(
                                device_id,
                                &descriptor,
                                Some(render_pipeline_id),
                            );
                        }

                        if let Some(sender) = sender {
                            let res = match error.and_then(Error::from_wgpu_error) {
                                // if device is lost we must return pipeline and not raise any error
                                None => Ok(Pipeline {
                                    id: render_pipeline_id,
                                    label: descriptor.label.unwrap_or_default().to_string(),
                                }),
                                Some(e) => Err(e),
                            };
                            if let Err(e) = sender.send(res) {
                                warn!("Failed sending WebGPURenderPipelineResponse {e:?}");
                            }
                        } else {
                            self.maybe_dispatch_wgpu_error(device_id, error);
                        }
                    },
                    WebGPURequest::CreateSampler {
                        device_id,
                        sampler_id,
                        descriptor,
                    } => {
                        let global = &self.global;
                        let (_, error) =
                            global.device_create_sampler(device_id, &descriptor, Some(sampler_id));
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.device_create_sampler(
                                device_id,
                                &descriptor,
                                Some(sampler_id),
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                    WebGPURequest::CreateQuerySet {
                        device_id,
                        query_set_id,
                        descriptor,
                    } => {
                        let global = &self.global;
                        let (_, error) = global.device_create_query_set(
                            device_id,
                            &descriptor,
                            Some(query_set_id),
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.device_create_query_set(
                                device_id,
                                &descriptor,
                                Some(query_set_id),
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                    WebGPURequest::CreateShaderModule {
                        device_id,
                        program_id,
                        program,
                        label,
                        callback: sender,
                    } => {
                        let global = &self.global;
                        let source =
                            wgpu_core::pipeline::ShaderModuleSource::Wgsl(Cow::Borrowed(&program));
                        let desc = ShaderModuleDescriptor {
                            label: label.map(|s| s.into()),
                            runtime_checks: wgt::ShaderRuntimeChecks::checked(),
                        };
                        let (_, error) = global.device_create_shader_module(
                            device_id,
                            &desc,
                            source,
                            Some(program_id),
                        );
                        for secondary in &self.secondary_gpus {
                            let secondary_source = wgpu_core::pipeline::ShaderModuleSource::Wgsl(
                                Cow::Borrowed(&program),
                            );
                            let secondary_desc = ShaderModuleDescriptor {
                                label: desc.label.clone(),
                                runtime_checks: wgt::ShaderRuntimeChecks::checked(),
                            };
                            let _ = secondary.global.device_create_shader_module(
                                device_id,
                                &secondary_desc,
                                secondary_source,
                                Some(program_id),
                            );
                        }
                        if let Err(e) = sender.send(
                            error
                                .as_ref()
                                .map(|e| ShaderCompilationInfo::from(e, &program)),
                        ) {
                            warn!("Failed to send CompilationInfo {e:?}");
                        }
                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                    WebGPURequest::CreateContext {
                        buffer_ids,
                        size,
                        sender,
                    } => {
                        let id = self
                            .webrender_external_image_id_manager
                            .next_id(WebRenderImageHandlerType::WebGpu);
                        let context_id = WebGPUContextId(id.0);

                        if let Err(error) = sender.send(context_id) {
                            warn!("Failed to send ContextId to new context ({error})");
                        };

                        self.create_context(context_id, size, buffer_ids);
                    },
                    WebGPURequest::Present {
                        context_id,
                        pending_texture,
                        size,
                        canvas_epoch,
                    } => {
                        self.present(context_id, pending_texture, size, canvas_epoch);
                    },
                    WebGPURequest::GetImage {
                        context_id,
                        pending_texture,
                        sender,
                    } => self.get_image(context_id, pending_texture, sender),
                    WebGPURequest::ValidateTextureDescriptor {
                        device_id,
                        texture_id,
                        descriptor,
                    } => {
                        // https://gpuweb.github.io/gpuweb/#dom-gpucanvascontext-configure
                        // validating TextureDescriptor by creating dummy texture
                        let global = &self.global;
                        let (_, error) =
                            global.device_create_texture(device_id, &descriptor, Some(texture_id));
                        global.texture_drop(texture_id);
                        self.poller.wake();
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeTexture(texture_id))
                        {
                            warn!("Unable to send FreeTexture({:?}) ({:?})", texture_id, e);
                        };
                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                    WebGPURequest::DestroyContext { context_id } => {
                        self.destroy_context(context_id);
                        self.webrender_external_image_id_manager
                            .remove(&ExternalImageId(context_id.0));
                    },
                    WebGPURequest::CreateTexture {
                        device_id,
                        texture_id,
                        descriptor,
                    } => {
                        let global = &self.global;
                        let (_, error) =
                            global.device_create_texture(device_id, &descriptor, Some(texture_id));
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.device_create_texture(
                                device_id,
                                &descriptor,
                                Some(texture_id),
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                    WebGPURequest::CreateTextureView {
                        texture_id,
                        texture_view_id,
                        device_id,
                        descriptor,
                    } => {
                        let global = &self.global;
                        if let Some(desc) = descriptor {
                            let (_, error) = global.texture_create_view(
                                texture_id,
                                &desc,
                                Some(texture_view_id),
                            );
                            for secondary in &self.secondary_gpus {
                                let _ = secondary.global.texture_create_view(
                                    texture_id,
                                    &desc,
                                    Some(texture_view_id),
                                );
                            }
                            self.maybe_dispatch_wgpu_error(device_id, error);
                        }
                    },
                    WebGPURequest::DestroyBuffer(buffer) => {
                        let global = &self.global;
                        global.buffer_destroy(buffer);
                        // Mirror destroy (actual GPU memory free) onto each secondary GPU and
                        // wake its poller so the freed memory is reclaimed on maintain.
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.buffer_destroy(buffer);
                            secondary.poller.wake();
                        }
                    },
                    WebGPURequest::DestroyDevice(device) => {
                        let global = &self.global;
                        global.device_destroy(device);
                        // Wake poller thread to trigger DeviceLostClosure
                        self.poller.wake();
                        // Mirror onto secondary GPU globals (same device id on each).
                        for secondary in &self.secondary_gpus {
                            secondary.global.device_destroy(device);
                            secondary.poller.wake();
                        }
                    },
                    WebGPURequest::DestroyTexture(texture_id) => {
                        let global = &self.global;
                        global.texture_destroy(texture_id);
                        // Mirror destroy (actual GPU memory free) onto each secondary GPU and
                        // wake its poller so the freed memory is reclaimed on maintain. Without
                        // this the per-frame canvas texture leaks on secondary GPUs.
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.texture_destroy(texture_id);
                            secondary.poller.wake();
                        }
                    },
                    WebGPURequest::Exit(sender) => {
                        if let Err(e) = sender.send(()) {
                            warn!("Failed to send response to WebGPURequest::Exit ({})", e)
                        }
                        break;
                    },
                    WebGPURequest::DropCommandEncoder(id) => {
                        let global = &self.global;
                        global.command_encoder_drop(id);
                        for secondary in &self.secondary_gpus {
                            secondary.global.command_encoder_drop(id);
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeCommandEncoder(id)) {
                            warn!("Unable to send FreeCommandEncoder({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropCommandBuffer(id) => {
                        let global = &self.global;
                        global.command_buffer_drop(id);
                        for secondary in &self.secondary_gpus {
                            secondary.global.command_buffer_drop(id);
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeCommandBuffer(id)) {
                            warn!("Unable to send FreeCommandBuffer({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropDevice(device_id) => {
                        let global = &self.global;
                        global.device_drop(device_id);
                        // Mirror onto secondary GPU globals (same device id on each).
                        for secondary in &self.secondary_gpus {
                            secondary.global.device_drop(device_id);
                        }
                        let device_scope = self
                            .devices
                            .lock()
                            .unwrap()
                            .remove(&device_id)
                            .expect("Device should not be dropped by this point");
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeDevice {
                            device_id,
                            pipeline_id: device_scope.pipeline_id,
                        }) {
                            warn!("Unable to send FreeDevice({:?}) ({:?})", device_id, e);
                        };
                    },
                    WebGPURequest::RenderBundleEncoderFinish {
                        render_bundle_encoder,
                        descriptor,
                        render_bundle_id,
                        device_id,
                    } => {
                        let global = &self.global;
                        let (_, error) = global.render_bundle_encoder_finish(
                            render_bundle_encoder,
                            &descriptor,
                            Some(render_bundle_id),
                        );

                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                    WebGPURequest::RequestAdapter {
                        sender,
                        options,
                        adapter_id,
                    } => {
                        let global = &self.global;
                        let response = self
                            .global
                            .request_adapter(&options, wgt::Backends::all(), Some(adapter_id))
                            .map(|adapter_id| {
                                // TODO: can we do this lazily
                                let adapter_info = global.adapter_get_info(adapter_id);
                                let limits = global.adapter_limits(adapter_id);
                                let features = global.adapter_features(adapter_id);
                                Adapter {
                                    adapter_info,
                                    adapter_id: WebGPUAdapter(adapter_id),
                                    features,
                                    limits,
                                    channel: WebGPU(self.sender.clone()),
                                }
                            })
                            .map_err(|err| err.to_string());

                        if let Err(e) = sender.send(Some(response)) {
                            warn!(
                                "Failed to send response to WebGPURequest::RequestAdapter ({})",
                                e
                            )
                        }
                    },
                    WebGPURequest::RequestDevice {
                        sender,
                        adapter_id,
                        descriptor,
                        device_id,
                        queue_id,
                        pipeline_id,
                    } => {
                        let desc = DeviceDescriptor {
                            label: descriptor.label.as_ref().map(crate::Cow::from),
                            required_features: descriptor.required_features,
                            required_limits: descriptor.required_limits.clone(),
                            memory_hints: MemoryHints::MemoryUsage,
                            trace: wgpu_types::Trace::Off,
                            experimental_features: ExperimentalFeatures::disabled(),
                        };
                        // Multi-GPU wall fan-out: mirror this device onto every additional
                        // physical GPU (reusing the same device/queue ids on each secondary
                        // global). Phase 1 only creates the mirror devices; command replay
                        // onto them follows in Phase 2.
                        self.ensure_secondary_gpus(adapter_id.0);
                        for secondary in &self.secondary_gpus {
                            match secondary.global.adapter_request_device(
                                secondary.adapter_id,
                                &desc,
                                Some(device_id),
                                Some(queue_id),
                            ) {
                                Ok(_) => info!(
                                    "WebGPU fan-out: mirrored device {device_id:?} onto \
                                     secondary GPU (LUID {:?})",
                                    secondary.target_luid
                                ),
                                Err(e) => warn!(
                                    "WebGPU fan-out: failed to mirror device onto secondary \
                                     GPU (LUID {:?}): {e}",
                                    secondary.target_luid
                                ),
                            }
                        }
                        let global = &self.global;
                        let device = WebGPUDevice(device_id);
                        let queue = WebGPUQueue(queue_id);
                        let result = global
                            .adapter_request_device(
                                adapter_id.0,
                                &desc,
                                Some(device_id),
                                Some(queue_id),
                            )
                            .map(|_| {
                                {
                                    self.devices.lock().unwrap().insert(
                                        device_id,
                                        DeviceScope::new(device_id, pipeline_id),
                                    );
                                }
                                let script_sender = self.script_sender.clone();
                                let devices = Arc::clone(&self.devices);
                                let callback = Box::from(move |reason, msg| {
                                    let reason = match reason {
                                        wgt::DeviceLostReason::Unknown => DeviceLostReason::Unknown,
                                        wgt::DeviceLostReason::Destroyed => {
                                            DeviceLostReason::Destroyed
                                        },
                                    };
                                    // make device lost by removing error scopes stack
                                    let _ = devices
                                        .lock()
                                        .unwrap()
                                        .get_mut(&device_id)
                                        .expect("Device should not be dropped by this point")
                                        .error_scope_stack
                                        .take();
                                    if let Err(e) = script_sender.send(WebGPUMsg::DeviceLost {
                                        device,
                                        pipeline_id,
                                        reason,
                                        msg,
                                    }) {
                                        warn!("Failed to send WebGPUMsg::DeviceLost: {e}");
                                    }
                                });
                                global.device_set_device_lost_closure(device_id, callback);
                                descriptor
                            })
                            .map_err(Into::into);
                        if let Err(e) = sender.send((device, queue, result)) {
                            warn!(
                                "Failed to send response to WebGPURequest::RequestDevice ({})",
                                e
                            )
                        }
                    },
                    WebGPURequest::BeginComputePass {
                        command_encoder_id,
                        compute_pass_id,
                        label,
                        device_id,
                    } => {
                        if !self.secondary_gpus.is_empty() {
                            let mut passes = Vec::with_capacity(self.secondary_gpus.len());
                            for secondary in &self.secondary_gpus {
                                let (spass, _e) =
                                    secondary.global.command_encoder_begin_compute_pass(
                                        command_encoder_id,
                                        &ComputePassDescriptor {
                                            label: label.clone(),
                                            timestamp_writes: None,
                                        },
                                    );
                                passes.push(spass);
                            }
                            self.secondary_compute_passes.insert(compute_pass_id, passes);
                        }
                        let global = &self.global;
                        let (pass, error) = global.command_encoder_begin_compute_pass(
                            command_encoder_id,
                            &ComputePassDescriptor {
                                label,
                                timestamp_writes: None,
                            },
                        );
                        assert!(
                            self.compute_passes.insert(compute_pass_id, pass).is_none(),
                            "ComputePass should not exist yet."
                        );
                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                    WebGPURequest::ComputePassSetPipeline {
                        compute_pass_id,
                        pipeline_id,
                        device_id,
                    } => {
                        let pass = self
                            .compute_passes
                            .get_mut(&compute_pass_id)
                            .expect("ComputePass should exists");
                        let result = self.global.compute_pass_set_pipeline(pass, pipeline_id);
                        self.replay_secondary_compute(compute_pass_id, |g, p| {
                            let _ = g.compute_pass_set_pipeline(p, pipeline_id);
                        });
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::ComputePassSetBindGroup {
                        compute_pass_id,
                        index,
                        bind_group_id,
                        offsets,
                        device_id,
                    } => {
                        let pass = self
                            .compute_passes
                            .get_mut(&compute_pass_id)
                            .expect("ComputePass should exists");
                        let result = self.global.compute_pass_set_bind_group(
                            pass,
                            index,
                            Some(bind_group_id),
                            &offsets,
                        );
                        self.replay_secondary_compute(compute_pass_id, |g, p| {
                            let _ = g.compute_pass_set_bind_group(
                                p,
                                index,
                                Some(bind_group_id),
                                &offsets,
                            );
                        });
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::ComputePassDispatchWorkgroups {
                        compute_pass_id,
                        x,
                        y,
                        z,
                        device_id,
                    } => {
                        let pass = self
                            .compute_passes
                            .get_mut(&compute_pass_id)
                            .expect("ComputePass should exists");
                        let result = self.global.compute_pass_dispatch_workgroups(pass, x, y, z);
                        self.replay_secondary_compute(compute_pass_id, |g, p| {
                            let _ = g.compute_pass_dispatch_workgroups(p, x, y, z);
                        });
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::ComputePassDispatchWorkgroupsIndirect {
                        compute_pass_id,
                        buffer_id,
                        offset,
                        device_id,
                    } => {
                        let pass = self
                            .compute_passes
                            .get_mut(&compute_pass_id)
                            .expect("ComputePass should exists");
                        let result = self
                            .global
                            .compute_pass_dispatch_workgroups_indirect(pass, buffer_id, offset);
                        self.replay_secondary_compute(compute_pass_id, |g, p| {
                            let _ = g.compute_pass_dispatch_workgroups_indirect(p, buffer_id, offset);
                        });
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::EndComputePass {
                        compute_pass_id,
                        device_id,
                    } => {
                        // https://www.w3.org/TR/2024/WD-webgpu-20240703/#dom-gpucomputepassencoder-end
                        let pass = self
                            .compute_passes
                            .get_mut(&compute_pass_id)
                            .expect("ComputePass should exists");
                        let result = self.global.compute_pass_end(pass);
                        self.replay_secondary_compute(compute_pass_id, |g, p| {
                            let _ = g.compute_pass_end(p);
                        });
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::BeginRenderPass {
                        command_encoder_id,
                        render_pass_id,
                        label,
                        color_attachments,
                        depth_stencil_attachment,
                        device_id,
                    } => {
                        // Mirror onto each secondary GPU first (cloning the descriptor inputs),
                        // then run the primary path unchanged (consuming the originals).
                        if !self.secondary_gpus.is_empty() {
                            let mut passes = Vec::with_capacity(self.secondary_gpus.len());
                            for secondary in &self.secondary_gpus {
                                let secondary_desc = RenderPassDescriptor {
                                    label: label.clone(),
                                    color_attachments: Cow::Owned(color_attachments.clone()),
                                    depth_stencil_attachment: depth_stencil_attachment.as_ref(),
                                    timestamp_writes: None,
                                    occlusion_query_set: None,
                                    multiview_mask: None,
                                };
                                let (spass, _e) = secondary
                                    .global
                                    .command_encoder_begin_render_pass(
                                        command_encoder_id,
                                        &secondary_desc,
                                    );
                                passes.push(spass);
                            }
                            self.secondary_render_passes.insert(render_pass_id, passes);
                        }
                        let global = &self.global;
                        let desc = &RenderPassDescriptor {
                            label,
                            color_attachments: color_attachments.into(),
                            depth_stencil_attachment: depth_stencil_attachment.as_ref(),
                            timestamp_writes: None,
                            occlusion_query_set: None,
                            multiview_mask: None,
                        };
                        let (pass, error) =
                            global.command_encoder_begin_render_pass(command_encoder_id, desc);
                        assert!(
                            self.render_passes.insert(render_pass_id, pass).is_none(),
                            "RenderPass should not exist yet."
                        );
                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                    WebGPURequest::RenderPassCommand {
                        render_pass_id,
                        render_command,
                        device_id,
                    } => {
                        // Mirror onto each secondary GPU (clone the command), then run primary.
                        self.replay_secondary_render(render_pass_id, |g, p| {
                            let _ = apply_render_command(g, p, render_command.clone());
                        });
                        let pass = self
                            .render_passes
                            .get_mut(&render_pass_id)
                            .expect("RenderPass should exists");
                        let result = apply_render_command(&self.global, pass, render_command);
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::EndRenderPass {
                        render_pass_id,
                        device_id,
                    } => {
                        // https://www.w3.org/TR/2024/WD-webgpu-20240703/#dom-gpurenderpassencoder-end
                        let pass = self
                            .render_passes
                            .get_mut(&render_pass_id)
                            .expect("RenderPass should exists");
                        let result = self.global.render_pass_end(pass);
                        self.replay_secondary_render(render_pass_id, |g, p| {
                            let _ = g.render_pass_end(p);
                        });
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::Submit {
                        device_id,
                        queue_id,
                        command_buffers,
                    } => {
                        let global = &self.global;
                        let result = {
                            let _guard = self.poller.lock();
                            global.queue_submit(queue_id, &command_buffers)
                        };
                        // Submit the mirrored command buffers on each secondary GPU and wake
                        // its poller so completed work (and deferred resource frees) is reaped.
                        for secondary in &self.secondary_gpus {
                            {
                                let _guard = secondary.poller.lock();
                                let _ = secondary.global.queue_submit(queue_id, &command_buffers);
                            }
                            secondary.poller.wake();
                        }
                        self.maybe_dispatch_wgpu_error(device_id, result.err().map(|(_, x)| x));
                    },
                    WebGPURequest::UnmapBuffer { buffer_id, mapping } => {
                        let global = &self.global;
                        if let Some(mapping) = &mapping &&
                            let Ok((slice_pointer, range_size)) = global.buffer_get_mapped_range(
                                buffer_id,
                                mapping.range.start,
                                Some(mapping.range.end - mapping.range.start),
                            )
                        {
                            unsafe {
                                slice::from_raw_parts_mut(
                                    slice_pointer.as_ptr(),
                                    range_size as usize,
                                )
                            }
                            .copy_from_slice(&mapping.data);
                        }
                        // Ignore result because this operation always succeed from user perspective
                        let _result = global.buffer_unmap(buffer_id);
                        // Mirror onto each secondary GPU so `mapped_at_creation` buffers receive
                        // their initial data and become usable (otherwise their command buffers
                        // would fail validation with a still-mapped buffer).
                        for secondary in &self.secondary_gpus {
                            if let Some(mapping) = &mapping &&
                                let Ok((slice_pointer, range_size)) =
                                    secondary.global.buffer_get_mapped_range(
                                        buffer_id,
                                        mapping.range.start,
                                        Some(mapping.range.end - mapping.range.start),
                                    )
                            {
                                unsafe {
                                    slice::from_raw_parts_mut(
                                        slice_pointer.as_ptr(),
                                        range_size as usize,
                                    )
                                }
                                .copy_from_slice(&mapping.data);
                            }
                            let _ = secondary.global.buffer_unmap(buffer_id);
                        }
                    },
                    WebGPURequest::WriteBuffer {
                        device_id,
                        queue_id,
                        buffer_id,
                        buffer_offset,
                        data,
                    } => {
                        let global = &self.global;
                        let result = global.queue_write_buffer(
                            queue_id,
                            buffer_id,
                            buffer_offset as wgt::BufferAddress,
                            &data,
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.queue_write_buffer(
                                queue_id,
                                buffer_id,
                                buffer_offset as wgt::BufferAddress,
                                &data,
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::WriteTexture {
                        device_id,
                        queue_id,
                        texture_cv,
                        data_layout,
                        size,
                        data,
                    } => {
                        let global = &self.global;
                        let _guard = self.poller.lock();
                        // TODO: Report result to content process
                        let result = global.queue_write_texture(
                            queue_id,
                            &texture_cv,
                            &data,
                            &data_layout,
                            &size,
                        );
                        drop(_guard);
                        for secondary in &self.secondary_gpus {
                            let _g = secondary.poller.lock();
                            let _ = secondary.global.queue_write_texture(
                                queue_id,
                                &texture_cv,
                                &data,
                                &data_layout,
                                &size,
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, result.err());
                    },
                    WebGPURequest::QueueOnSubmittedWorkDone { sender, queue_id } => {
                        let global = &self.global;
                        let token = self.poller.token();
                        let callback = Box::from(move || {
                            drop(token);
                            if let Err(e) = sender.send(()) {
                                warn!("Could not send SubmittedWorkDone Response ({})", e);
                            }
                        });
                        global.queue_on_submitted_work_done(queue_id, callback);
                        self.poller.wake();
                    },
                    WebGPURequest::DropTexture(id) => {
                        let global = &self.global;
                        global.texture_drop(id);
                        self.poller.wake();
                        for secondary in &self.secondary_gpus {
                            secondary.global.texture_drop(id);
                            secondary.poller.wake();
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeTexture(id)) {
                            warn!("Unable to send FreeTexture({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropAdapter(id) => {
                        let global = &self.global;
                        global.adapter_drop(id);
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeAdapter(id)) {
                            warn!("Unable to send FreeAdapter({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropBuffer(id) => {
                        let global = &self.global;
                        global.buffer_drop(id);
                        self.poller.wake();
                        for secondary in &self.secondary_gpus {
                            secondary.global.buffer_drop(id);
                            secondary.poller.wake();
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeBuffer(id)) {
                            warn!("Unable to send FreeBuffer({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropPipelineLayout(id) => {
                        let global = &self.global;
                        global.pipeline_layout_drop(id);
                        for secondary in &self.secondary_gpus {
                            secondary.global.pipeline_layout_drop(id);
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreePipelineLayout(id)) {
                            warn!("Unable to send FreePipelineLayout({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropComputePipeline(id) => {
                        let global = &self.global;
                        global.compute_pipeline_drop(id);
                        for secondary in &self.secondary_gpus {
                            secondary.global.compute_pipeline_drop(id);
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeComputePipeline(id))
                        {
                            warn!("Unable to send FreeComputePipeline({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropComputePass(id) => {
                        // Pass might have already ended.
                        self.compute_passes.remove(&id);
                        self.secondary_compute_passes.remove(&id);
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeComputePass(id)) {
                            warn!("Unable to send FreeComputePass({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropRenderPass(id) => {
                        self.render_passes
                            .remove(&id)
                            .expect("RenderPass should exists");
                        self.secondary_render_passes.remove(&id);
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeRenderPass(id)) {
                            warn!("Unable to send FreeRenderPass({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropRenderPipeline(id) => {
                        let global = &self.global;
                        global.render_pipeline_drop(id);
                        for secondary in &self.secondary_gpus {
                            secondary.global.render_pipeline_drop(id);
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeRenderPipeline(id)) {
                            warn!("Unable to send FreeRenderPipeline({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropBindGroup(id) => {
                        let global = &self.global;
                        global.bind_group_drop(id);
                        for secondary in &self.secondary_gpus {
                            secondary.global.bind_group_drop(id);
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeBindGroup(id)) {
                            warn!("Unable to send FreeBindGroup({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropBindGroupLayout(id) => {
                        let global = &self.global;
                        global.bind_group_layout_drop(id);
                        for secondary in &self.secondary_gpus {
                            secondary.global.bind_group_layout_drop(id);
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeBindGroupLayout(id))
                        {
                            warn!("Unable to send FreeBindGroupLayout({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropTextureView(id) => {
                        let global = &self.global;
                        global.texture_view_drop(id);
                        self.poller.wake();
                        for secondary in &self.secondary_gpus {
                            secondary.global.texture_view_drop(id);
                            secondary.poller.wake();
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeTextureView(id)) {
                            warn!("Unable to send FreeTextureView({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropSampler(id) => {
                        let global = &self.global;
                        global.sampler_drop(id);
                        for secondary in &self.secondary_gpus {
                            secondary.global.sampler_drop(id);
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeSampler(id)) {
                            warn!("Unable to send FreeSampler({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropShaderModule(id) => {
                        let global = &self.global;
                        global.shader_module_drop(id);
                        for secondary in &self.secondary_gpus {
                            secondary.global.shader_module_drop(id);
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeShaderModule(id)) {
                            warn!("Unable to send FreeShaderModule({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropRenderBundle(id) => {
                        let global = &self.global;
                        global.render_bundle_drop(id);
                        for secondary in &self.secondary_gpus {
                            secondary.global.render_bundle_drop(id);
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeRenderBundle(id)) {
                            warn!("Unable to send FreeRenderBundle({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::DropQuerySet(id) => {
                        let global = &self.global;
                        global.query_set_drop(id);
                        for secondary in &self.secondary_gpus {
                            secondary.global.query_set_drop(id);
                        }
                        if let Err(e) = self.script_sender.send(WebGPUMsg::FreeQuerySet(id)) {
                            warn!("Unable to send FreeQuerySet({:?}) ({:?})", id, e);
                        };
                    },
                    WebGPURequest::PushErrorScope { device_id, filter } => {
                        // <https://www.w3.org/TR/webgpu/#dom-gpudevice-pusherrorscope>
                        let mut devices = self.devices.lock().unwrap();
                        let device_scope = devices
                            .get_mut(&device_id)
                            .expect("Device should not be dropped by this point");
                        if let Some(error_scope_stack) = &mut device_scope.error_scope_stack {
                            error_scope_stack.push(ErrorScope::new(filter));
                        } // else device is lost
                    },
                    WebGPURequest::DispatchError { device_id, error } => {
                        self.dispatch_error(device_id, error);
                    },
                    WebGPURequest::PopErrorScope {
                        device_id,
                        callback: sender,
                    } => {
                        // <https://www.w3.org/TR/webgpu/#dom-gpudevice-poperrorscope>
                        let mut devices = self.devices.lock().unwrap();
                        let device_scope = devices
                            .get_mut(&device_id)
                            .expect("Device should not be dropped by this point");
                        let result =
                            if let Some(error_scope_stack) = &mut device_scope.error_scope_stack {
                                if let Some(error_scope) = error_scope_stack.pop() {
                                    Ok(
                                        // TODO: Do actual selection instead of selecting first error
                                        error_scope.errors.first().cloned(),
                                    )
                                } else {
                                    Err(PopError::Empty)
                                }
                            } else {
                                // This means the device has been lost.
                                Err(PopError::Lost)
                            };
                        if let Err(error) = sender.send(result) {
                            warn!("Error while sending PopErrorScope result: {error}");
                        }
                    },
                    WebGPURequest::ComputeGetBindGroupLayout {
                        device_id,
                        pipeline_id,
                        index,
                        id,
                    } => {
                        let global = &self.global;
                        let (_, error) = global.compute_pipeline_get_bind_group_layout(
                            pipeline_id,
                            index,
                            Some(id),
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.compute_pipeline_get_bind_group_layout(
                                pipeline_id,
                                index,
                                Some(id),
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                    WebGPURequest::RenderGetBindGroupLayout {
                        device_id,
                        pipeline_id,
                        index,
                        id,
                    } => {
                        let global = &self.global;
                        let (_, error) = global.render_pipeline_get_bind_group_layout(
                            pipeline_id,
                            index,
                            Some(id),
                        );
                        for secondary in &self.secondary_gpus {
                            let _ = secondary.global.render_pipeline_get_bind_group_layout(
                                pipeline_id,
                                index,
                                Some(id),
                            );
                        }
                        self.maybe_dispatch_wgpu_error(device_id, error);
                    },
                }
            }
        }
        if let Err(e) = self.script_sender.send(WebGPUMsg::Exit) {
            warn!("Failed to send WebGPUMsg::Exit to script ({})", e);
        }
    }

    #[inline]
    fn maybe_dispatch_wgpu_error<E: WebGpuError>(
        &mut self,
        device_id: id::DeviceId,
        error: Option<E>,
    ) {
        self.maybe_dispatch_error(device_id, error.and_then(Error::from_wgpu_error))
    }

    /// Dispatches error (if there is any)
    fn maybe_dispatch_error(&mut self, device_id: id::DeviceId, error: Option<Error>) {
        if let Some(error) = error {
            self.dispatch_error(device_id, error);
        }
    }

    /// <https://www.w3.org/TR/webgpu/#abstract-opdef-dispatch-error>
    fn dispatch_error(&mut self, device_id: id::DeviceId, error: Error) {
        let mut devices = self.devices.lock().unwrap();
        let device_scope = devices
            .get_mut(&device_id)
            .expect("Device should not be dropped by this point");
        if let Some(error_scope_stack) = &mut device_scope.error_scope_stack {
            if let Some(error_scope) = error_scope_stack
                .iter_mut()
                .rev()
                .find(|error_scope| error_scope.filter == error.filter())
            {
                error_scope.errors.push(error);
            } else if self
                .script_sender
                .send(WebGPUMsg::UncapturedError {
                    device: WebGPUDevice(device_id),
                    pipeline_id: device_scope.pipeline_id,
                    error: error.clone(),
                })
                .is_err()
            {
                warn!("Failed to send WebGPUMsg::UncapturedError: {error:?}");
            }
        } // else device is lost
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use webgpu_traits::{WebGPU, WebGPUDevice, WebGPUQuerySet, WebGPURequest};
use wgpu_core::resource::QuerySetDescriptor;

use crate::conversions::Convert;
use crate::dom::bindings::codegen::Bindings::WebGPUBinding::{
    GPUQuerySetDescriptor, GPUQuerySetMethods, GPUQueryType,
};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::USVString;
use crate::dom::globalscope::GlobalScope;
use crate::dom::webgpu::gpudevice::GPUDevice;
use crate::script_runtime::CanGc;

#[derive(JSTraceable, MallocSizeOf)]
struct DroppableGPUQuerySet {
    #[no_trace]
    channel: WebGPU,
    #[no_trace]
    query_set: WebGPUQuerySet,
}

impl Drop for DroppableGPUQuerySet {
    fn drop(&mut self) {
        if let Err(e) = self
            .channel
            .0
            .send(WebGPURequest::DropQuerySet(self.query_set.0))
        {
            warn!(
                "Failed to send DropQuerySet ({:?}) ({})",
                self.query_set.0, e
            );
        }
    }
}

#[dom_struct]
pub(crate) struct GPUQuerySet {
    reflector_: Reflector,
    label: DomRefCell<USVString>,
    #[no_trace]
    device: WebGPUDevice,
    droppable: DroppableGPUQuerySet,
}

impl GPUQuerySet {
    fn new_inherited(
        channel: WebGPU,
        device: WebGPUDevice,
        query_set: WebGPUQuerySet,
        label: USVString,
    ) -> Self {
        Self {
            reflector_: Reflector::new(),
            label: DomRefCell::new(label),
            device,
            droppable: DroppableGPUQuerySet { channel, query_set },
        }
    }

    pub(crate) fn new(
        global: &GlobalScope,
        channel: WebGPU,
        device: WebGPUDevice,
        query_set: WebGPUQuerySet,
        label: USVString,
        can_gc: CanGc,
    ) -> DomRoot<Self> {
        reflect_dom_object(
            Box::new(GPUQuerySet::new_inherited(channel, device, query_set, label)),
            global,
            can_gc,
        )
    }

    pub(crate) fn id(&self) -> WebGPUQuerySet {
        self.droppable.query_set
    }

    /// <https://gpuweb.github.io/gpuweb/#dom-gpudevice-createqueryset>
    pub(crate) fn create(
        device: &GPUDevice,
        descriptor: &GPUQuerySetDescriptor,
        can_gc: CanGc,
    ) -> DomRoot<GPUQuerySet> {
        let query_set_id = device.global().wgpu_id_hub().create_query_set_id();
        let desc = QuerySetDescriptor {
            label: (&descriptor.parent).convert(),
            ty: match descriptor.type_ {
                GPUQueryType::Occlusion => wgpu_types::QueryType::Occlusion,
                GPUQueryType::Timestamp => wgpu_types::QueryType::Timestamp,
            },
            count: descriptor.count,
        };

        device
            .channel()
            .0
            .send(WebGPURequest::CreateQuerySet {
                device_id: device.id().0,
                query_set_id,
                descriptor: desc,
            })
            .expect("Failed to create WebGPU query set");

        let query_set = WebGPUQuerySet(query_set_id);

        GPUQuerySet::new(
            &device.global(),
            device.channel(),
            device.id(),
            query_set,
            descriptor.parent.label.clone(),
            can_gc,
        )
    }
}

impl GPUQuerySetMethods<crate::DomTypeHolder> for GPUQuerySet {
    /// <https://gpuweb.github.io/gpuweb/#dom-gpuqueryset-destroy>
    fn Destroy(&self) {
        // The underlying wgpu query set is released when this DOM object is
        // dropped (see DroppableGPUQuerySet). An explicit destroy() is a no-op
        // here to avoid double-freeing the same query set id.
    }

    /// <https://gpuweb.github.io/gpuweb/#dom-gpuobjectbase-label>
    fn Label(&self) -> USVString {
        self.label.borrow().clone()
    }

    /// <https://gpuweb.github.io/gpuweb/#dom-gpuobjectbase-label>
    fn SetLabel(&self, value: USVString) {
        *self.label.borrow_mut() = value;
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use script_bindings::script_runtime::CanGc;
use webgpu_traits::{WebGPU, WebGPUDevice, WebGPUExternalTexture, WebGPURequest};

use crate::dom::bindings::codegen::Bindings::WebGPUBinding::GPUExternalTextureMethods;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::USVString;
use crate::dom::globalscope::GlobalScope;

#[derive(JSTraceable, MallocSizeOf)]
struct DroppableGPUExternalTexture {
    #[no_trace]
    channel: WebGPU,
    #[no_trace]
    external_texture: WebGPUExternalTexture,
}

impl Drop for DroppableGPUExternalTexture {
    fn drop(&mut self) {
        if let Err(e) = self
            .channel
            .0
            .send(WebGPURequest::DropExternalTexture(self.external_texture.0))
        {
            warn!(
                "Failed to send DropExternalTexture ({:?}) ({})",
                self.external_texture.0, e
            );
        }
    }
}

/// <https://gpuweb.github.io/gpuweb/#gpuexternaltexture>
#[dom_struct]
pub(crate) struct GPUExternalTexture {
    reflector_: Reflector,
    label: DomRefCell<USVString>,
    #[no_trace]
    device: WebGPUDevice,
    droppable: DroppableGPUExternalTexture,
}

impl GPUExternalTexture {
    fn new_inherited(
        channel: WebGPU,
        device: WebGPUDevice,
        external_texture: WebGPUExternalTexture,
        label: USVString,
    ) -> Self {
        Self {
            reflector_: Reflector::new(),
            label: DomRefCell::new(label),
            device,
            droppable: DroppableGPUExternalTexture {
                channel,
                external_texture,
            },
        }
    }

    pub(crate) fn new(
        global: &GlobalScope,
        channel: WebGPU,
        device: WebGPUDevice,
        external_texture: WebGPUExternalTexture,
        label: USVString,
        can_gc: CanGc,
    ) -> DomRoot<Self> {
        reflect_dom_object(
            Box::new(GPUExternalTexture::new_inherited(
                channel,
                device,
                external_texture,
                label,
            )),
            global,
            can_gc,
        )
    }

    pub(crate) fn id(&self) -> WebGPUExternalTexture {
        self.droppable.external_texture
    }
}

impl GPUExternalTextureMethods<crate::DomTypeHolder> for GPUExternalTexture {
    /// <https://gpuweb.github.io/gpuweb/#dom-gpuobjectbase-label>
    fn Label(&self) -> USVString {
        self.label.borrow().clone()
    }

    /// <https://gpuweb.github.io/gpuweb/#dom-gpuobjectbase-label>
    fn SetLabel(&self, value: USVString) {
        *self.label.borrow_mut() = value;
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use servo_canvas_traits::webgl::WebGLVersion;

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;
use crate::script_runtime::CanGc;

#[dom_struct]
pub(crate) struct EXTColorBufferFloat {
    reflector_: Reflector,
}

impl EXTColorBufferFloat {
    fn new_inherited() -> EXTColorBufferFloat {
        Self {
            reflector_: Reflector::new(),
        }
    }
}

impl WebGLExtension for EXTColorBufferFloat {
    type Extension = EXTColorBufferFloat;
    fn new(ctx: &WebGLRenderingContext, can_gc: CanGc) -> DomRoot<EXTColorBufferFloat> {
        reflect_dom_object(
            Box::new(EXTColorBufferFloat::new_inherited()),
            &*ctx.global(),
            can_gc,
        )
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::Specific(WebGLVersion::WebGL2)
    }

    fn is_supported(ext: &WebGLExtensions) -> bool {
        ext.supports_any_gl_extension(&[
            "GL_EXT_color_buffer_float",
            "GL_CHROMIUM_color_buffer_float_rgba",
        ])
    }

    fn enable(_ext: &WebGLExtensions) {}

    fn name() -> &'static str {
        "EXT_color_buffer_float"
    }
}

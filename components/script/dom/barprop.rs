/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::BarPropBinding::BarPropMethods;
use crate::dom::bindings::root::DomRoot;
use crate::dom::window::Window;

/// <https://html.spec.whatwg.org/multipage/#barprop>
#[dom_struct]
pub(crate) struct BarProp {
    reflector_: Reflector,
}

impl BarProp {
    pub(crate) fn new(cx: &mut JSContext, window: &Window) -> DomRoot<BarProp> {
        reflect_dom_object(
            cx,
            Box::new(BarProp {
                reflector_: Reflector::new(),
            }),
            window,
        )
    }
}

impl BarPropMethods<crate::DomTypeHolder> for BarProp {
    /// <https://html.spec.whatwg.org/multipage/#dom-barprop-visible>
    fn Visible(&self) -> bool {
        // Only popup windows report hidden bars, and every webview here is a full browser tab.
        true
    }
}

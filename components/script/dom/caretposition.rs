/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use app_units::Au;
use dom_struct::dom_struct;
use euclid::Rect;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use style_traits::CSSPixel;

use crate::dom::bindings::codegen::Bindings::CaretPositionBinding::CaretPositionMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::domrect::DOMRect;
use crate::dom::globalscope::GlobalScope;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::window::Window;

/// <https://drafts.csswg.org/cssom-view/#caretposition>
#[dom_struct]
pub(crate) struct CaretPosition {
    reflector_: Reflector,
    offset_node: Dom<Node>,
    offset: u32,
    /// The caret in the viewport, when it is in laid out text.
    #[no_trace]
    client_rect: Option<Rect<Au, CSSPixel>>,
}

impl CaretPosition {
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        offset_node: &Node,
        offset: u32,
        client_rect: Option<Rect<Au, CSSPixel>>,
    ) -> DomRoot<CaretPosition> {
        reflect_dom_object_with_cx(
            Box::new(CaretPosition {
                reflector_: Reflector::new(),
                offset_node: Dom::from_ref(offset_node),
                offset,
                client_rect,
            }),
            window.upcast::<GlobalScope>(),
            cx,
        )
    }
}

impl CaretPositionMethods<crate::DomTypeHolder> for CaretPosition {
    /// <https://drafts.csswg.org/cssom-view/#dom-caretposition-offsetnode>
    fn OffsetNode(&self) -> DomRoot<Node> {
        self.offset_node.as_rooted()
    }

    /// <https://drafts.csswg.org/cssom-view/#dom-caretposition-offset>
    fn Offset(&self) -> u32 {
        self.offset
    }

    /// <https://drafts.csswg.org/cssom-view/#dom-caretposition-getclientrect>
    fn GetClientRect(&self, cx: &mut JSContext) -> Option<DomRoot<DOMRect>> {
        let rect = self.client_rect?;
        let window = self.offset_node.owner_window();
        Some(DOMRect::new(
            cx,
            window.upcast(),
            rect.origin.x.to_f64_px(),
            rect.origin.y.to_f64_px(),
            rect.size.width.to_f64_px(),
            rect.size.height.to_f64_px(),
        ))
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, local_name, ns};
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};

use crate::dom::bindings::codegen::Bindings::SVGAnimatedStringBinding::SVGAnimatedStringMethods;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::element::Element;
use crate::dom::node::NodeTraits;

/// A string attribute reflected through `baseVal`/`animVal`; without SMIL animation support
/// both read the attribute.
/// <https://svgwg.org/svg2-draft/types.html#InterfaceSVGAnimatedString>
#[dom_struct]
pub(crate) struct SVGAnimatedString {
    reflector_: Reflector,
    element: Dom<Element>,
    #[no_trace]
    local_name: LocalName,
}

impl SVGAnimatedString {
    fn new_inherited(element: &Element, local_name: LocalName) -> SVGAnimatedString {
        SVGAnimatedString {
            reflector_: Reflector::new(),
            element: Dom::from_ref(element),
            local_name,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        element: &Element,
        local_name: LocalName,
    ) -> DomRoot<SVGAnimatedString> {
        reflect_dom_object_with_cx(
            Box::new(SVGAnimatedString::new_inherited(element, local_name)),
            &*element.owner_window(),
            cx,
        )
    }
}

impl SVGAnimatedStringMethods<crate::DomTypeHolder> for SVGAnimatedString {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedString__baseVal>
    fn BaseVal(&self) -> DOMString {
        // `href` falls back to the deprecated `xlink:href` when absent.
        // <https://svgwg.org/svg2-draft/types.html#__svg__SVGURIReference__href>
        if self.local_name == local_name!("href") && !self.element.has_attribute(&self.local_name) {
            return self
                .element
                .get_attribute_string_value_with_namespace(&ns!(xlink), &local_name!("href"))
                .map(DOMString::from)
                .unwrap_or_default();
        }
        self.element.get_string_attribute(&self.local_name)
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedString__baseVal>
    fn SetBaseVal(&self, cx: &mut JSContext, value: DOMString) {
        self.element
            .set_string_attribute(cx, &self.local_name, value);
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedString__animVal>
    fn AnimVal(&self) -> DOMString {
        self.BaseVal()
    }
}

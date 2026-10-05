/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;

use crate::dom::bindings::codegen::Bindings::SVGAElementBinding::SVGAElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::document::Document;
use crate::dom::element::Element;
use crate::dom::node::Node;
use crate::dom::svg::svganimatedstring::SVGAnimatedString;
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;

#[dom_struct]
pub(crate) struct SVGAElement {
    svggraphicselement: SVGGraphicsElement,
    href: MutNullableDom<SVGAnimatedString>,
    target: MutNullableDom<SVGAnimatedString>,
}

impl SVGAElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGAElement {
        SVGAElement {
            svggraphicselement: SVGGraphicsElement::new_inherited(local_name, prefix, document),
            href: Default::default(),
            target: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGAElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGAElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }
}

impl SVGAElementMethods<crate::DomTypeHolder> for SVGAElement {
    /// <https://svgwg.org/svg2-draft/linking.html#__svg__SVGAElement__target>
    fn Target(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.target.or_init(|| {
            SVGAnimatedString::new(cx, self.upcast::<Element>(), local_name!("target"))
        })
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGURIReference__href>
    fn Href(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.href
            .or_init(|| SVGAnimatedString::new(cx, self.upcast::<Element>(), local_name!("href")))
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, local_name, ns};
use js::context::{JSContext, NoGC};
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::HTMLAllCollectionBinding::HTMLAllCollectionMethods;
use crate::dom::bindings::codegen::Bindings::HTMLCollectionBinding::HTMLCollectionMethods;
use crate::dom::bindings::codegen::UnionTypes::HTMLCollectionOrElement;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::element::Element;
use crate::dom::html::htmlcollection::{CollectionFilter, HTMLCollection};
use crate::dom::node::Node;
use crate::dom::window::Window;

/// <https://html.spec.whatwg.org/multipage/#htmlallcollection>
///
/// Its [[IsHTMLDDA]] slot and [[Call]] live in the bindings layer (see
/// `script_bindings::proxyhandler::HTML_ALL_COLLECTION_PROXY_CLASS`).
#[dom_struct]
pub(crate) struct HTMLAllCollection {
    reflector_: Reflector,
    /// Every element in the document, in tree order.
    elements: Dom<HTMLCollection>,
}

/// <https://html.spec.whatwg.org/multipage/#all-named-elements>
fn is_all_named_element(element: &Element) -> bool {
    *element.namespace() == ns!(html) &&
        matches!(
            *element.local_name(),
            local_name!("a") |
                local_name!("button") |
                local_name!("embed") |
                local_name!("form") |
                local_name!("frame") |
                local_name!("frameset") |
                local_name!("iframe") |
                local_name!("img") |
                local_name!("input") |
                local_name!("map") |
                local_name!("meta") |
                local_name!("object") |
                local_name!("select") |
                local_name!("textarea")
        )
}

/// Matches the elements step 2 of
/// <https://html.spec.whatwg.org/multipage/#concept-get-all-named> collects.
#[derive(JSTraceable, MallocSizeOf)]
struct AllNamedFilter {
    #[no_trace]
    name: Atom,
}

impl CollectionFilter for AllNamedFilter {
    fn filter(&self, element: &Element, _root: &Node) -> bool {
        element.get_id().is_some_and(|id| id == self.name) ||
            (is_all_named_element(element) &&
                element.get_name().is_some_and(|name| name == self.name))
    }
}

/// <https://tc39.es/ecma262/#array-index>: the canonical decimal form of an integer in
/// 0..2^32-1. Any other spelling ("01", "+1", "1.0") is a name, not an index.
fn as_array_index(string: &str) -> Option<u32> {
    let index: u32 = string.parse().ok()?;
    (index != u32::MAX && index.to_string() == string).then_some(index)
}

impl HTMLAllCollection {
    fn new_inherited(elements: &HTMLCollection) -> HTMLAllCollection {
        HTMLAllCollection {
            reflector_: Reflector::new(),
            elements: Dom::from_ref(elements),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        document: &Document,
    ) -> DomRoot<HTMLAllCollection> {
        let elements =
            HTMLCollection::by_qualified_name(cx, window, document.upcast(), LocalName::from("*"));
        reflect_dom_object_with_cx(
            Box::new(HTMLAllCollection::new_inherited(&elements)),
            window,
            cx,
        )
    }

    /// <https://html.spec.whatwg.org/multipage/#concept-get-all-named>
    fn get_all_named(
        &self,
        cx: &mut JSContext,
        name: DOMString,
    ) -> Option<HTMLCollectionOrElement> {
        // Step 1.
        if name.is_empty() {
            return None;
        }
        let filter = AllNamedFilter {
            name: Atom::from(name),
        };
        let root = self.elements.root_node();

        // Steps 3-4, without materializing a collection for the common zero/one-match cases.
        {
            let mut matches = self
                .elements
                .elements_iter(cx.no_gc())
                .filter(|element| filter.filter(element, &root));
            let first = matches.next()?.as_rooted();
            if matches.next().is_none() {
                return Some(HTMLCollectionOrElement::Element(first));
            }
        }

        // Steps 2 and 5.
        Some(HTMLCollectionOrElement::HTMLCollection(HTMLCollection::new(
            cx,
            self.global().as_window(),
            &root,
            Box::new(filter),
        )))
    }
}

impl HTMLAllCollectionMethods<crate::DomTypeHolder> for HTMLAllCollection {
    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-length>
    fn Length(&self, cx: &JSContext) -> u32 {
        self.elements.Length(cx)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-item>
    fn IndexedGetter(&self, cx: &JSContext, index: u32) -> Option<DomRoot<Element>> {
        self.elements.Item(cx, index)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-nameditem>
    fn NamedItem(&self, cx: &mut JSContext, name: DOMString) -> Option<HTMLCollectionOrElement> {
        self.get_all_named(cx, name)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-nameditem>
    fn NamedGetter(&self, cx: &mut JSContext, name: DOMString) -> Option<HTMLCollectionOrElement> {
        self.get_all_named(cx, name)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-item>
    fn Item(
        &self,
        cx: &mut JSContext,
        name_or_index: Option<DOMString>,
    ) -> Option<HTMLCollectionOrElement> {
        // Step 1.
        let name_or_index = name_or_index?;

        // Step 2, via <https://html.spec.whatwg.org/multipage/#concept-get-all-indexed-or-named>.
        if let Some(index) = as_array_index(&name_or_index.str()) {
            return self
                .elements
                .Item(cx, index)
                .map(HTMLCollectionOrElement::Element);
        }
        self.get_all_named(cx, name_or_index)
    }

    /// <https://html.spec.whatwg.org/multipage/#the-htmlallcollection-interface:supported-property-names>
    fn SupportedPropertyNames(&self, no_gc: &NoGC) -> Vec<DOMString> {
        let mut result: Vec<DOMString> = vec![];
        for element in self.elements.elements_iter(no_gc) {
            let id = element.get_id();
            let name = element
                .get_name()
                .filter(|_| is_all_named_element(&element));
            for atom in id.into_iter().chain(name) {
                let name = DOMString::from(&*atom);
                if !atom.is_empty() && !result.contains(&name) {
                    result.push(name);
                }
            }
        }
        result
    }
}

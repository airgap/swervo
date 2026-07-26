/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashSet;

use base64::Engine as _;
use cssparser::{Parser, ParserInput};
use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, QualName, local_name, ns};
use js::context::JSContext;
use js::rust::HandleObject;
use layout_api::SVGElementData;
use pixels::EncodedImageType;
use script_bindings::cell::DomRefCell;
use servo_url::ServoUrl;
use style::attr::AttrValue;
use style::parser::ParserContext;
use style::stylesheets::Origin;
use style::values::specified::LengthPercentage;
use style_traits::ParsingMode;
use uuid::Uuid;
use xml5ever::serialize::TraversalScope;

use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, LayoutDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::element::{AttributeMutation, CustomElementCreationMode, Element, ElementCreator};
use crate::dom::html::htmlimageelement::HTMLImageElement;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::text::Text;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{
    ChildrenMutation, CloneChildrenFlag, Node, NodeDamage, NodeTraits, UnbindContext,
};
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;

#[dom_struct]
pub(crate) struct SVGSVGElement {
    svggraphicselement: SVGGraphicsElement,
    uuid: String,
    // The XML source of subtree rooted at this SVG element, serialized into
    // a base64 encoded `data:` url. This is cached to avoid recomputation
    // on each layout and must be invalidated when the subtree changes.
    #[no_trace]
    cached_serialized_data_url: DomRefCell<Option<Result<ServoUrl, ()>>>,
}

impl SVGSVGElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGSVGElement {
        SVGSVGElement {
            svggraphicselement: SVGGraphicsElement::new_inherited(local_name, prefix, document),
            uuid: Uuid::new_v4().to_string(),
            cached_serialized_data_url: Default::default(),
        }
    }

    #[cfg_attr(crown, allow(crown::unrooted_must_root))]
    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGSVGElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGSVGElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }

    pub(crate) fn serialize_and_cache_subtree(&self, cx: &mut js::context::JSContext) {
        let mut cloned_nodes = self.process_use_elements(cx);
        // Order matters: lowering `<foreignObject>` and `<image href>` first means the
        // `<image>` elements those passes insert (which carry `mask`/`clip-path`/`filter`
        // attributes) are seen by the external-reference pass below.
        cloned_nodes.extend(self.process_foreign_objects(cx));
        cloned_nodes.extend(self.process_image_elements(cx));
        cloned_nodes.extend(self.process_external_references(cx));

        let serialize_result = self
            .upcast::<Node>()
            .xml_serialize(TraversalScope::IncludeNode);

        self.cleanup_cloned_nodes(cx, &cloned_nodes);

        let Ok(xml_source) = serialize_result else {
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        };

        let xml_source: String = xml_source.into();
        let base64_encoded_source = base64::engine::general_purpose::STANDARD.encode(xml_source);
        let data_url = format!("data:image/svg+xml;base64,{}", base64_encoded_source);
        match ServoUrl::parse(&data_url) {
            Ok(url) => *self.cached_serialized_data_url.borrow_mut() = Some(Ok(url)),
            Err(error) => error!("Unable to parse serialized SVG data url: {error}"),
        };
    }

    fn process_use_elements(&self, cx: &mut JSContext) -> Vec<DomRoot<Node>> {
        let mut cloned_nodes = Vec::new();
        let root_node = self.upcast::<Node>();

        for node in root_node.traverse_preorder(ShadowIncluding::No) {
            if let Some(element) = node.downcast::<Element>() &&
                element.local_name() == &local_name!("use") &&
                let Some(cloned) = self.process_single_use_element(cx, element)
            {
                cloned_nodes.push(cloned);
            }
        }

        cloned_nodes
    }

    fn process_single_use_element(
        &self,
        cx: &mut JSContext,
        use_element: &Element,
    ) -> Option<DomRoot<Node>> {
        let href = use_element.get_string_attribute(&local_name!("href"));
        let href_view = href.str();
        let id_str = href_view.strip_prefix("#")?;
        let id = DOMString::from(id_str);
        let document = self.upcast::<Node>().owner_doc();
        let referenced_element = document.GetElementById(cx, id)?;
        let referenced_node = referenced_element.upcast::<Node>();
        let has_svg_ancestor = referenced_node
            .inclusive_ancestors(ShadowIncluding::No)
            .any(|ancestor| ancestor.is::<SVGSVGElement>());
        if !has_svg_ancestor {
            return None;
        }
        let cloned_node = Node::clone(
            cx,
            referenced_node,
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        let root_node = self.upcast::<Node>();
        let _ = root_node.AppendChild(cx, &cloned_node);

        Some(cloned_node)
    }

    /// Inline elements referenced from this subtree via `url(#id)` in `mask`, `clip-path`,
    /// `filter`, paint, and marker attributes but defined OUTSIDE it (the common pattern is a
    /// single hidden `<svg><defs>` at document root holding every mask — Discord, GitHub, …).
    /// The subtree is serialized as a standalone SVG document, so without this the rasterizer
    /// can't resolve those ids and drops the reference entirely (an unmasked rect renders as a
    /// square where the page expects a circle). Referenced elements are cloned in, recursively
    /// (a cloned mask may itself reference a gradient), and removed after serialization.
    fn process_external_references(&self, cx: &mut JSContext) -> Vec<DomRoot<Node>> {
        let reference_attributes: Vec<LocalName> = [
            "mask",
            "clip-path",
            "filter",
            "fill",
            "stroke",
            "marker-start",
            "marker-mid",
            "marker-end",
        ]
        .iter()
        .map(|name| LocalName::from(*name))
        .collect();

        let root_node = self.upcast::<Node>();
        let document = root_node.owner_doc();
        let mut cloned_nodes = Vec::new();
        let mut processed_ids: HashSet<String> = HashSet::new();
        // Worklist of subtrees still to scan: starts at this svg element, grows with each
        // clone (whose content can reference further external definitions).
        let mut pending: Vec<DomRoot<Node>> = vec![DomRoot::from_ref(root_node)];

        while let Some(scan_root) = pending.pop() {
            let mut referenced_ids: Vec<String> = Vec::new();
            for node in scan_root.traverse_preorder(ShadowIncluding::No) {
                let Some(element) = node.downcast::<Element>() else {
                    continue;
                };
                for attr_name in &reference_attributes {
                    if !element.has_attribute(attr_name) {
                        continue;
                    }
                    let value = element.get_string_attribute(attr_name);
                    if let Some(id) = parse_url_fragment_reference(&value.str()) {
                        referenced_ids.push(id);
                    }
                }
            }

            for id in referenced_ids {
                if !processed_ids.insert(id.clone()) {
                    continue;
                }
                let Some(referenced_element) = document.GetElementById(cx, DOMString::from(id))
                else {
                    continue;
                };
                let referenced_node = referenced_element.upcast::<Node>();
                // Already inside this svg element: it serializes with the subtree as-is.
                if root_node.is_inclusive_ancestor_of(referenced_node) {
                    continue;
                }
                // Same guard as `<use>`: only inline definitions that live in some svg.
                let has_svg_ancestor = referenced_node
                    .inclusive_ancestors(ShadowIncluding::No)
                    .any(|ancestor| ancestor.is::<SVGSVGElement>());
                if !has_svg_ancestor {
                    continue;
                }
                let cloned_node = Node::clone(
                    cx,
                    referenced_node,
                    None,
                    CloneChildrenFlag::CloneChildren,
                    None,
                );
                let _ = root_node.AppendChild(cx, &cloned_node);
                pending.push(cloned_node.clone());
                cloned_nodes.push(cloned_node);
            }
        }

        cloned_nodes
    }

    /// Lower each `<foreignObject>` whose content is effectively a single `<img>` (optionally
    /// inside wrapper elements — the universal avatar pattern) into an SVG `<image>` carrying
    /// the foreignObject's geometry and effect attributes, with the img's already-decoded
    /// raster embedded as a `data:image/png` href. The rasterizer skips `<foreignObject>`
    /// entirely (it cannot lay out HTML), so without this every masked avatar simply
    /// disappears. The `<image>` is inserted in the foreignObject's sibling position to
    /// preserve SVG paint order, and removed after serialization.
    fn process_foreign_objects(&self, cx: &mut JSContext) -> Vec<DomRoot<Node>> {
        let root_node = self.upcast::<Node>();
        let foreign_object_name = LocalName::from("foreignObject");
        // Collect first: lowering mutates the tree mid-traversal otherwise.
        let foreign_objects: Vec<DomRoot<Element>> = root_node
            .traverse_preorder(ShadowIncluding::No)
            .filter_map(DomRoot::downcast::<Element>)
            .filter(|element| {
                element.local_name() == &foreign_object_name && *element.namespace() == ns!(svg)
            })
            .collect();

        let mut inserted_nodes = Vec::new();
        for foreign_object in foreign_objects {
            if let Some(image_node) = self.lower_foreign_object_to_image(cx, &foreign_object) {
                inserted_nodes.push(image_node);
            }
        }
        inserted_nodes
    }

    /// Re-embed each `<image>` element whose href points at a fetched external resource as a
    /// sibling `<image>` with the decoded raster as a `data:` href. The rasterized document
    /// cannot fetch (no network, no base URL), so external hrefs render nothing without this;
    /// `SVGImageElement` fetches its href through the image cache and invalidates this svg's
    /// cached serialization when pixels arrive. data: hrefs pass through untouched.
    fn process_image_elements(&self, cx: &mut JSContext) -> Vec<DomRoot<Node>> {
        use crate::dom::svg::svgimageelement::SVGImageElement;

        let root_node = self.upcast::<Node>();
        let image_elements: Vec<DomRoot<SVGImageElement>> = root_node
            .traverse_preorder(ShadowIncluding::No)
            .filter_map(DomRoot::downcast::<SVGImageElement>)
            .collect();

        let mut inserted_nodes = Vec::new();
        for image_element in image_elements {
            let element = image_element.upcast::<Element>();
            let href = element.get_string_attribute(&local_name!("href"));
            let effective_href = if href.str().is_empty() {
                element
                    .get_attribute_string_value_with_namespace(&ns!(xlink), &local_name!("href"))
                    .unwrap_or_default()
            } else {
                href.to_string()
            };
            // Nothing to do: no href, or one the rasterizer can already consume.
            if effective_href.is_empty() || effective_href.starts_with("data:") {
                continue;
            }
            let Some(snapshot) = image_element.get_raster_image_data() else {
                continue;
            };
            let Some(data_url) = png_data_url(snapshot) else {
                continue;
            };

            let element_node = image_element.upcast::<Node>();
            let document = element_node.owner_doc();
            let replacement = Element::create(
                cx,
                QualName::new(None, ns!(svg), LocalName::from("image")),
                None,
                &document,
                ElementCreator::ScriptCreated,
                CustomElementCreationMode::Synchronous,
                None,
            );
            for name in [
                "x",
                "y",
                "width",
                "height",
                "mask",
                "clip-path",
                "filter",
                "transform",
                "opacity",
                // Case-sensitive SVG name: must go through the namespace-explicit accessor —
                // the namespace-less getters debug-assert lowercase ASCII.
                "preserveAspectRatio",
            ] {
                let attr_name = LocalName::from(name);
                let Some(value) =
                    element.get_attribute_string_value_with_namespace(&ns!(), &attr_name)
                else {
                    continue;
                };
                replacement.set_attribute_from_parser(
                    cx,
                    QualName::new(None, ns!(), attr_name),
                    DOMString::from(value),
                    None,
                );
            }
            replacement.set_attribute_from_parser(
                cx,
                QualName::new(None, ns!(), LocalName::from("href")),
                DOMString::from(data_url),
                None,
            );

            // Same paint slot; the original's unresolvable href renders nothing there.
            let Some(parent) = element_node.GetParentNode() else {
                continue;
            };
            let replacement_node = DomRoot::from_ref(replacement.upcast::<Node>());
            if parent
                .InsertBefore(cx, &replacement_node, Some(element_node))
                .is_ok()
            {
                inserted_nodes.push(replacement_node);
            }
        }
        inserted_nodes
    }

    fn lower_foreign_object_to_image(
        &self,
        cx: &mut JSContext,
        foreign_object: &Element,
    ) -> Option<DomRoot<Node>> {
        let foreign_object_node = foreign_object.upcast::<Node>();

        // The content must be effectively a single image: exactly one <img> among the
        // descendants and no non-whitespace text. Wrapper elements (divs) are tolerated;
        // any styling they carry is beyond what this lowering can represent.
        let mut image_element: Option<DomRoot<HTMLImageElement>> = None;
        for node in foreign_object_node
            .traverse_preorder(ShadowIncluding::No)
            .skip(1)
        {
            if let Some(image) = node.downcast::<HTMLImageElement>() {
                if image_element.is_some() {
                    return None;
                }
                image_element = Some(DomRoot::from_ref(image));
            } else if node.is::<Text>() &&
                node.GetTextContent()
                    .is_some_and(|text| !text.str().trim().is_empty())
            {
                return None;
            }
        }
        let image_element = image_element?;

        // Encode the img's decoded raster as a data: URI the standalone rasterized document
        // can consume. Not loaded yet -> skip; the load-completion hook on HTMLImageElement
        // invalidates this svg's cached serialization, so we re-run once pixels exist.
        let data_url = png_data_url(image_element.get_raster_image_data()?)?;

        let document = foreign_object_node.owner_doc();
        let image_svg_element = Element::create(
            cx,
            QualName::new(None, ns!(svg), LocalName::from("image")),
            None,
            &document,
            ElementCreator::ScriptCreated,
            CustomElementCreationMode::Synchronous,
            None,
        );
        // Carry the foreignObject's geometry and effects over to the replacement <image>.
        // set_attribute_from_parser: the element is freshly created (no collisions) and SVG
        // attribute names are case-sensitive (`preserveAspectRatio`), which the lowercase-only
        // `set_attribute` path asserts against.
        for name in [
            "x",
            "y",
            "width",
            "height",
            "mask",
            "clip-path",
            "filter",
            "transform",
            "opacity",
        ] {
            let attr_name = LocalName::from(name);
            if foreign_object.has_attribute(&attr_name) {
                let value = foreign_object.get_string_attribute(&attr_name);
                image_svg_element.set_attribute_from_parser(
                    cx,
                    QualName::new(None, ns!(), attr_name),
                    value,
                    None,
                );
            }
        }
        // An HTML <img> with explicit dimensions fills its box; SVG <image> letterboxes by
        // default. "none" matches the HTML behavior the foreignObject content actually had.
        image_svg_element.set_attribute_from_parser(
            cx,
            QualName::new(None, ns!(), LocalName::from("preserveAspectRatio")),
            DOMString::from("none"),
            None,
        );
        image_svg_element.set_attribute_from_parser(
            cx,
            QualName::new(None, ns!(), LocalName::from("href")),
            DOMString::from(data_url),
            None,
        );

        // Insert in the foreignObject's paint-order slot (the foreignObject itself renders
        // nothing in the rasterizer, so no double paint).
        let parent = foreign_object_node.GetParentNode()?;
        let image_node = DomRoot::from_ref(image_svg_element.upcast::<Node>());
        parent
            .InsertBefore(cx, &image_node, Some(foreign_object_node))
            .ok()?;
        Some(image_node)
    }

    fn cleanup_cloned_nodes(&self, cx: &mut JSContext, cloned_nodes: &[DomRoot<Node>]) {
        if cloned_nodes.is_empty() {
            return;
        }

        // Nodes from the reference pass hang off this svg root; lowered foreignObject
        // images sit at arbitrary depths — remove each from its actual parent.
        for cloned_node in cloned_nodes {
            if let Some(parent) = cloned_node.GetParentNode() {
                let _ = parent.RemoveChild(cx, cloned_node);
            }
        }
    }

    pub(crate) fn invalidate_cached_serialized_subtree_and_rasterization_result(&self) {
        let owner_window = self.owner_window();
        owner_window
            .image_cache()
            .evict_rasterized_image(&self.uuid);
        if let Some(Ok(url)) = &*self.cached_serialized_data_url.borrow() {
            owner_window.layout_mut().remove_cached_image(url);
            owner_window.image_cache().evict_completed_image(
                url,
                owner_window.origin().immutable(),
                &None,
            );
        }

        *self.cached_serialized_data_url.borrow_mut() = None;
        self.upcast::<Node>().dirty(NodeDamage::Other);
    }
}

/// Encode a decoded raster as a `data:image/png;base64,…` URI (the canvas-toDataURL
/// encoding path). The standalone rasterized SVG document can consume data: URIs but
/// cannot fetch anything else.
fn png_data_url(mut snapshot: pixels::Snapshot) -> Option<String> {
    let mut data_url = String::from("data:image/png;base64,");
    let mut encoder = base64::write::EncoderStringWriter::from_consumer(
        &mut data_url,
        &base64::engine::general_purpose::STANDARD,
    );
    snapshot
        .encode_for_mime_type(&EncodedImageType::Png, None, &mut encoder)
        .ok()?;
    encoder.into_inner();
    Some(data_url)
}

/// Extract `id` from a `url(#id)` attribute value (quotes and whitespace tolerated).
/// Returns `None` for non-fragment urls and the `none` keyword.
fn parse_url_fragment_reference(value: &str) -> Option<String> {
    let inner = value
        .trim()
        .strip_prefix("url(")?
        .strip_suffix(")")?
        .trim()
        .trim_matches(|c| c == '"' || c == '\'');
    inner
        .strip_prefix('#')
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

impl<'dom> LayoutDom<'dom, SVGSVGElement> {
    #[expect(unsafe_code)]
    pub(crate) fn data(self) -> SVGElementData<'dom> {
        let svg_id = self.unsafe_get().uuid.clone();
        let element = self.upcast::<Element>();
        let width = element.get_attr_for_layout(&ns!(), &local_name!("width"));
        let height = element.get_attr_for_layout(&ns!(), &local_name!("height"));
        let view_box = element.get_attr_for_layout(&ns!(), &local_name!("viewBox"));
        SVGElementData {
            source: unsafe {
                self.unsafe_get()
                    .cached_serialized_data_url
                    .borrow_for_layout()
                    .clone()
            },
            width,
            height,
            view_box,
            svg_id,
        }
    }
}

impl VirtualMethods for SVGSVGElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
    }

    fn attribute_mutated(
        &self,
        cx: &mut js::context::JSContext,
        attr: AttrRef<'_>,
        mutation: AttributeMutation,
    ) {
        self.super_type()
            .unwrap()
            .attribute_mutated(cx, attr, mutation);

        self.invalidate_cached_serialized_subtree_and_rasterization_result();
    }

    fn attribute_affects_presentational_hints(&self, attr: AttrRef<'_>) -> bool {
        match attr.local_name() {
            &local_name!("width") | &local_name!("height") => true,
            _ => self
                .super_type()
                .unwrap()
                .attribute_affects_presentational_hints(attr),
        }
    }

    fn parse_plain_attribute(&self, name: &LocalName, value: DOMString) -> AttrValue {
        match *name {
            local_name!("width") | local_name!("height") => {
                let value = &value.str();
                let parser_input = &mut ParserInput::new(value);
                let parser = &mut Parser::new(parser_input);
                let doc = self.owner_document();
                let url = doc.url().into_url().into();
                let context = ParserContext::new(
                    Origin::Author,
                    &url,
                    None,
                    ParsingMode::ALLOW_UNITLESS_LENGTH,
                    doc.quirks_mode(),
                    /* namespaces = */ Default::default(),
                    None,
                    None,
                    /* attr_taint = */ Default::default(),
                );
                let val = LengthPercentage::parse_quirky(
                    &context,
                    parser,
                    style::values::specified::AllowQuirks::Always,
                );
                AttrValue::LengthPercentage(value.to_string(), val.ok())
            },
            _ => self
                .super_type()
                .unwrap()
                .parse_plain_attribute(name, value),
        }
    }

    fn children_changed(&self, cx: &mut JSContext, mutation: &ChildrenMutation) {
        if let Some(super_type) = self.super_type() {
            super_type.children_changed(cx, mutation);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result();
    }

    fn unbind_from_tree(&self, cx: &mut js::context::JSContext, context: &UnbindContext<'_>) {
        if let Some(s) = self.super_type() {
            s.unbind_from_tree(cx, context);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result();
    }
}

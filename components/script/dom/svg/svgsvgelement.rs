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
use layout_api::{SVG_PAINT_PROPERTIES, SVGElementData, parse_view_box, svg_paint_signature};
use net_traits::image_cache::Image;
use percent_encoding::percent_decode_str;
use pixels::EncodedImageType;
use script_bindings::cell::DomRefCell;
use servo_url::ServoUrl;
use style::Atom;
use style::attr::AttrValue;
use style::color::AbsoluteColor;
use style::parser::{Parse, ParserContext};
use style::properties::{ComputedValues, LonghandId, PropertyDeclarationId};
use style::stylesheets::Origin;
use style::values::computed::{Color as ComputedColor, SVGPaint, Size as StyleSize};
use style::values::generics::svg::{SVGPaintFallback, SVGPaintKind};
use style::values::specified::{Color as SpecifiedColor, LengthPercentage};
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
use crate::dom::svg::svgimageelement::SVGImageElement;

#[dom_struct]
pub(crate) struct SVGSVGElement {
    svggraphicselement: SVGGraphicsElement,
    uuid: String,
    // The XML source of subtree rooted at this SVG element, serialized into
    // a base64 encoded `data:` url. This is cached to avoid recomputation
    // on each layout and must be invalidated when the subtree changes.
    #[no_trace]
    cached_serialized_data_url: DomRefCell<Option<Result<ServoUrl, ()>>>,
    /// The `svg_paint_signature` of this element's style that the cached serialization was
    /// built with; layout re-requests serialization when the current style no longer matches.
    cached_paint_signature: DomRefCell<Option<String>>,
    /// The IDs the cached serialization looked up in the document to inline targets defined
    /// outside this subtree, found or not; registered with the document so that the
    /// serialization is invalidated when any of them appears, disappears or changes.
    #[no_trace]
    referenced_ids: DomRefCell<Vec<Atom>>,
}

/// `<use>` elements whose href is rewritten in the serialization, with the new value.
type HrefRewrites = Vec<(DomRoot<Element>, String)>;

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
            cached_paint_signature: Default::default(),
            referenced_ids: Default::default(),
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
        // A re-serialization (layout saw the paint properties change) must not leave the
        // previous document's raster behind: rasterizations are cached per svg element.
        if self.cached_serialized_data_url.borrow().is_some() {
            self.evict_cached_images();
        }
        // Recorded only once the passes below are done: their temporary clones mutate this
        // subtree, which invalidates (clears) the cache, signature included.
        let paint_signature = self
            .upcast::<Element>()
            .style_from_last_restyle()
            .map(|style| svg_paint_signature(&style));

        let document = self.owner_document();
        self.unregister_referenced_ids();
        document.set_svg_serialization_in_progress(true);
        let mut referenced_ids = HashSet::new();
        let mut href_rewrites = HrefRewrites::new();

        let mut cloned_nodes =
            self.process_use_elements(cx, &mut referenced_ids, &mut href_rewrites);
        // Order matters: lowering `<foreignObject>` first means the `<image>` elements that
        // pass inserts (which carry `mask`/`clip-path`/`filter` attributes) are seen by the
        // external-reference pass below.
        cloned_nodes.extend(self.process_foreign_objects(cx));
        cloned_nodes.extend(self.process_external_references(cx, &mut referenced_ids));

        let rewrite_attributes = |element: &Element, attributes: &mut Vec<(QualName, AttrValue)>| {
            self.rewrite_serialized_attributes(element, attributes, &href_rewrites)
        };
        let serialize_result = self
            .upcast::<Node>()
            .xml_serialize_with_attribute_rewrite(TraversalScope::IncludeNode, &rewrite_attributes);

        self.cleanup_cloned_nodes(cx, &cloned_nodes);
        document.set_svg_serialization_in_progress(false);
        for id in &referenced_ids {
            document.register_svg_id_reference_listener(id.clone(), self);
        }
        *self.referenced_ids.borrow_mut() = referenced_ids.into_iter().collect();
        *self.cached_paint_signature.borrow_mut() = paint_signature;

        let Ok(xml_source) = serialize_result else {
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        };

        // Tagged with this element's id so that identical subtrees in different `<svg>`s don't
        // share an image-cache entry: re-serializing one evicts its entry and rasterizations
        // (see `evict_cached_images`), which left every other `<svg>` with the same markup
        // painting nothing until something happened to damage it.
        let xml_source = format!("<!--{}-->{}", self.uuid, String::from(xml_source));
        let base64_encoded_source = base64::engine::general_purpose::STANDARD.encode(xml_source);
        let data_url = format!("data:image/svg+xml;base64,{}", base64_encoded_source);
        match ServoUrl::parse(&data_url) {
            Ok(url) => *self.cached_serialized_data_url.borrow_mut() = Some(Ok(url)),
            Err(error) => error!("Unable to parse serialized SVG data url: {error}"),
        };
    }

    fn unregister_referenced_ids(&self) {
        let document = self.owner_document();
        for id in self.referenced_ids.take() {
            document.unregister_svg_id_reference_listener(&id, self);
        }
    }

    fn process_use_elements(
        &self,
        cx: &mut JSContext,
        referenced_ids: &mut HashSet<Atom>,
        href_rewrites: &mut HrefRewrites,
    ) -> Vec<DomRoot<Node>> {
        let mut cloned_nodes = Vec::new();
        let root_node = self.upcast::<Node>();

        for node in root_node.traverse_preorder(ShadowIncluding::No) {
            if let Some(element) = node.downcast::<Element>() &&
                element.local_name() == &local_name!("use") &&
                let Some(cloned) =
                    self.process_single_use_element(cx, element, referenced_ids, href_rewrites)
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
        referenced_ids: &mut HashSet<Atom>,
        href_rewrites: &mut HrefRewrites,
    ) -> Option<DomRoot<Node>> {
        let href = use_element.get_string_attribute(&local_name!("href"));
        let effective_href = if href.str().is_empty() {
            use_element
                .get_attribute_string_value_with_namespace(&ns!(xlink), &local_name!("href"))
                .unwrap_or_default()
        } else {
            href.to_string()
        };
        if effective_href.is_empty() {
            return None;
        }
        let Some(id_str) = effective_href.strip_prefix('#') else {
            return self.process_external_use_element(
                cx,
                use_element,
                &effective_href,
                href_rewrites,
            );
        };
        if id_str.is_empty() {
            return None;
        }
        referenced_ids.insert(Atom::from(id_str));
        let id = DOMString::from(id_str);
        let document = self.upcast::<Node>().owner_doc();
        let referenced_element = document.GetElementById(cx, id)?;
        let referenced_node = referenced_element.upcast::<Node>();
        let root_node = self.upcast::<Node>();
        // Already inside this svg: it serializes with the subtree and the rasterizer resolves
        // the reference itself. Cloning it in anyway PAINTED it — a bare <path> appended at
        // the root is normal content, not a definition (dark blob behind every guild icon).
        if root_node.is_inclusive_ancestor_of(referenced_node) {
            return None;
        }
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
        Some(self.append_in_defs(cx, &cloned_node))
    }

    /// Inline the target of a `<use href="file.svg#id">` from the fetched external document
    /// under an id unique to this serialization, and point the use's serialized href at it:
    /// the rasterizer cannot fetch, and the page's own ids may clash with the external one's.
    /// While the document loads nothing is inlined; its arrival invalidates this svg.
    fn process_external_use_element(
        &self,
        cx: &mut JSContext,
        use_element: &Element,
        href: &str,
        href_rewrites: &mut HrefRewrites,
    ) -> Option<DomRoot<Node>> {
        let document = self.owner_document();
        let mut url = document.encoding_parse_a_url(href).ok()?;
        let id = percent_decode_str(url.fragment().filter(|fragment| !fragment.is_empty())?)
            .decode_utf8_lossy()
            .into_owned();
        url.set_fragment(None);
        let external_document = document.external_svg_document(url, self)?;
        let referenced_element = external_document.GetElementById(cx, DOMString::from(&*id))?;
        let cloned_node = Node::clone(
            cx,
            referenced_element.upcast::<Node>(),
            Some(&document),
            CloneChildrenFlag::CloneChildren,
            None,
        );
        let inlined_id = format!("external-use-{}-{id}", href_rewrites.len());
        cloned_node.downcast::<Element>().unwrap().set_atomic_attribute(
            cx,
            &local_name!("id"),
            DOMString::from(&*inlined_id),
        );
        href_rewrites.push((DomRoot::from_ref(use_element), format!("#{inlined_id}")));
        Some(self.append_in_defs(cx, &cloned_node))
    }

    /// Append `node` to this svg inside a `<defs>` wrapper, so that it is resolvable by id but
    /// never paints, and return the wrapper.
    fn append_in_defs(&self, cx: &mut JSContext, node: &Node) -> DomRoot<Node> {
        let document = self.owner_document();
        let root_node = self.upcast::<Node>();
        let defs = Element::create(
            cx,
            QualName::new(None, ns!(svg), LocalName::from("defs")),
            None,
            &document,
            ElementCreator::ScriptCreated,
            CustomElementCreationMode::Synchronous,
            None,
        );
        let defs_node = DomRoot::from_ref(defs.upcast::<Node>());
        let _ = defs_node.AppendChild(cx, node);
        let _ = root_node.AppendChild(cx, &defs_node);
        defs_node
    }

    /// Inline elements referenced from this subtree via `url(#id)` in `mask`, `clip-path`,
    /// `filter`, paint, and marker attributes but defined OUTSIDE it (the common pattern is a
    /// single hidden `<svg><defs>` at document root holding every mask — Discord, GitHub, …).
    /// The subtree is serialized as a standalone SVG document, so without this the rasterizer
    /// can't resolve those ids and drops the reference entirely (an unmasked rect renders as a
    /// square where the page expects a circle). Referenced elements are cloned in, recursively
    /// (a cloned mask may itself reference a gradient), and removed after serialization.
    fn process_external_references(
        &self,
        cx: &mut JSContext,
        resolved_ids: &mut HashSet<Atom>,
    ) -> Vec<DomRoot<Node>> {
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
                // Paint servers named by stylesheet rules or inline `style`, which the attribute
                // scan can't see; the serialization carries them as computed `url(#id)`s.
                if let Some(style) = element.style_from_last_restyle() {
                    let inherited_svg = style.get_inherited_svg();
                    referenced_ids.extend(
                        [inherited_svg.clone_fill(), inherited_svg.clone_stroke()]
                            .iter()
                            .filter_map(paint_server_id),
                    );
                }
                // A gradient or pattern takes its unspecified attributes and its stops or
                // content from the one its href names:
                // <https://svgwg.org/svg2-draft/pservers.html#LinearGradientElementHrefAttribute>
                if matches!(
                    *element.local_name(),
                    local_name!("linearGradient") |
                        local_name!("radialGradient") |
                        local_name!("pattern")
                ) {
                    let href = element.get_string_attribute(&local_name!("href"));
                    let effective_href = if href.str().is_empty() {
                        element
                            .get_attribute_string_value_with_namespace(
                                &ns!(xlink),
                                &local_name!("href"),
                            )
                            .unwrap_or_default()
                    } else {
                        href.to_string()
                    };
                    if let Some(id) = effective_href.strip_prefix('#').filter(|id| !id.is_empty())
                    {
                        referenced_ids.push(id.to_owned());
                    }
                }
            }

            for id in referenced_ids {
                if !processed_ids.insert(id.clone()) {
                    continue;
                }
                resolved_ids.insert(Atom::from(&*id));
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
        // Native foreignObject layout (LYK-136 stage 3) builds real boxes for the HTML
        // content on top of the raster; lowering it into the raster too would double-paint.
        // Instead, synthesize each foreignObject's standalone mask document (phase 2): the
        // mask-image presentation hint composites the native content through the svg mask.
        if servo_config::pref!(dom_svg_foreignobject_native) {
            self.synthesize_native_foreign_object_masks(cx);
            return Vec::new();
        }
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

    /// The `href` and `preserveAspectRatio` that `image` is serialized with when its href is an
    /// external resource that has been fetched: the rasterized document cannot fetch (no
    /// network, no base URL), so the image is embedded as a `data:` URL instead, a raster as
    /// PNG and an SVG document as its own source. `SVGImageElement` fetches through the image
    /// cache and invalidates this svg's cached serialization when the image arrives. `None`
    /// keeps the element's own attributes: no href, a `data:` href the rasterizer reads itself,
    /// or an image not loaded (yet).
    fn embedded_image_attributes(
        &self,
        image: &SVGImageElement,
    ) -> Option<(String, Option<String>)> {
        let element = image.upcast::<Element>();
        let href = element.get_string_attribute(&local_name!("href"));
        let effective_href = if href.str().is_empty() {
            element
                .get_attribute_string_value_with_namespace(&ns!(xlink), &local_name!("href"))
                .unwrap_or_default()
        } else {
            href.to_string()
        };
        if effective_href.is_empty() || effective_href.starts_with("data:") {
            return None;
        }
        let preserve_aspect_ratio = element.get_attribute_string_value_with_namespace(
            &ns!(),
            &local_name!("preserveAspectRatio"),
        );
        match image.image_data()? {
            Image::Raster(raster) => {
                Some((png_data_url(raster.as_snapshot())?, preserve_aspect_ratio))
            },
            // Embedded as SVG rather than pre-rasterized, so that the rasterizer renders the
            // nested document at the final resolution.
            Image::Vector(vector) => {
                let source = self
                    .owner_window()
                    .image_cache()
                    .vector_image_source(vector.id)?;
                let data_url = format!(
                    "data:image/svg+xml;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(source.text.as_bytes())
                );
                // Chrome draws an SVG image into the `<image>` viewport as that document's own
                // viewport, fitting its viewBox with its root's `preserveAspectRatio`, and
                // stretches a document without a viewBox; the `<image>`'s attribute only counts
                // when it is `none`, which stretches as well. The rasterizer fits the nested
                // document's size with the `<image>`'s attribute instead, so that is replaced by
                // the value giving Chrome's result.
                let stretched = preserve_aspect_ratio
                    .as_deref()
                    .is_some_and(|value| value.split_whitespace().any(|token| token == "none"));
                let preserve_aspect_ratio = match source.root_preserve_aspect_ratio {
                    Some(root_value) if !stretched => root_value,
                    _ => "none".to_owned(),
                };
                Some((data_url, Some(preserve_aspect_ratio)))
            },
        }
    }

    /// Phase 2 of native foreignObject rendering (LYK-136): for each foreignObject child
    /// carrying `mask="url(#id)"`, build a standalone SVG document — the referenced mask
    /// plus a white rect covering the foreignObject's rect with that mask applied — and
    /// store it on the element. The mask-image presentation hint feeds it to the CSS
    /// mask-image pipeline (rasterize for_mask -> WR image-mask on the box), so the native
    /// HTML content clips exactly like the rasterized path. Nested references inside the
    /// mask (gradients) are not chased yet; objectBoundingBox masks are exact, and
    /// userSpaceOnUse is approximated by a viewBox anchored at the foreignObject rect.
    fn synthesize_native_foreign_object_masks(&self, cx: &mut JSContext) {
        use crate::dom::svg::svgelement::SVGElement;

        let root_node = self.upcast::<Node>();
        let document = root_node.owner_doc();
        let foreign_object_name = LocalName::from("foreignObject");
        let foreign_objects: Vec<DomRoot<Element>> = root_node
            .traverse_preorder(ShadowIncluding::No)
            .filter_map(DomRoot::downcast::<Element>)
            .filter(|element| {
                element.local_name() == &foreign_object_name && *element.namespace() == ns!(svg)
            })
            .collect();

        for foreign_object in foreign_objects {
            let Some(svg_element) = foreign_object.downcast::<SVGElement>() else {
                continue;
            };
            let mask_document_url = (|| -> Option<ServoUrl> {
                let mask_value = foreign_object
                    .get_attribute_string_value_with_namespace(&ns!(), &local_name!("mask"))?;
                let id = parse_url_fragment_reference(&mask_value)?;
                let referenced = document.GetElementById(cx, DOMString::from(id.clone()))?;
                let referenced_node = referenced.upcast::<Node>();
                referenced_node
                    .inclusive_ancestors(ShadowIncluding::No)
                    .any(|ancestor| ancestor.is::<SVGSVGElement>())
                    .then_some(())?;
                let mask_xml: String = referenced_node
                    .xml_serialize(TraversalScope::IncludeNode)
                    .ok()?
                    .into();
                // Inline the mask's own reference closure (its <use href="#id"> targets,
                // gradients, nested masks, …) — they live outside the mask element (the
                // guild-icon pattern keeps the squircle path in a sibling <defs>) and a
                // dangling reference rasterizes the mask all-black: content vanishes.
                let closure_xml = serialize_reference_closure(cx, referenced_node);
                let defs_xml = if closure_xml.is_empty() {
                    String::new()
                } else {
                    format!("<defs>{}</defs>", closure_xml)
                };
                let attr = |name: &LocalName| {
                    foreign_object
                        .get_attribute_string_value_with_namespace(&ns!(), name)
                        .unwrap_or_default()
                };
                let x = attr(&local_name!("x"));
                let y = attr(&local_name!("y"));
                let width = attr(&local_name!("width"));
                let height = attr(&local_name!("height"));
                if width.is_empty() || height.is_empty() {
                    return None;
                }
                let x = if x.is_empty() { "0".to_owned() } else { x };
                let y = if y.is_empty() { "0".to_owned() } else { y };
                let doc = format!(
                    concat!(
                        "<svg xmlns=\"http://www.w3.org/2000/svg\" ",
                        "xmlns:xlink=\"http://www.w3.org/1999/xlink\" ",
                        "width=\"{w}\" height=\"{h}\" viewBox=\"{x} {y} {w} {h}\">{mask}{defs}",
                        "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" ",
                        "fill=\"white\" mask=\"url(#{id})\"/></svg>"
                    ),
                    w = width,
                    h = height,
                    x = x,
                    y = y,
                    mask = mask_xml,
                    defs = defs_xml,
                    id = id,
                );
                let data_url = format!(
                    "data:image/svg+xml;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(doc)
                );
                ServoUrl::parse(&data_url).ok()
            })();
            svg_element.set_native_mask_document(mask_document_url);
        }
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

    /// Adjust `element`'s attributes for the standalone serialization the rasterizer reads.
    fn rewrite_serialized_attributes(
        &self,
        element: &Element,
        attributes: &mut Vec<(QualName, AttrValue)>,
        href_rewrites: &HrefRewrites,
    ) {
        if let Some((_, href)) = href_rewrites
            .iter()
            .find(|(use_element, _)| std::ptr::eq(&**use_element, element))
        {
            attributes.retain(|(name, _)| name.local != local_name!("href"));
            attributes.push((
                QualName::new(None, ns!(), local_name!("href")),
                AttrValue::String(href.clone()),
            ));
        }

        // `xlink:href` is serialized without its prefix, so next to a plain `href` it duplicates
        // that attribute: an XML well-formedness error that leaves the whole svg blank. The
        // plain attribute takes precedence anyway:
        // <https://svgwg.org/svg2-draft/linking.html#XLinkRefAttrs>
        if attributes
            .iter()
            .any(|(name, _)| name.ns == ns!() && name.local == local_name!("href"))
        {
            attributes
                .retain(|(name, _)| name.ns != ns!(xlink) || name.local != local_name!("href"));
        }
        if let Some(image) = element.downcast::<SVGImageElement>() &&
            let Some((href, preserve_aspect_ratio)) = self.embedded_image_attributes(image)
        {
            attributes.retain(|(name, _)| {
                let is_href =
                    name.local == local_name!("href") && matches!(name.ns, ns!() | ns!(xlink));
                let is_preserve_aspect_ratio =
                    name.ns == ns!() && name.local == local_name!("preserveAspectRatio");
                !is_href && !is_preserve_aspect_ratio
            });
            attributes.push((
                QualName::new(None, ns!(), local_name!("href")),
                AttrValue::String(href),
            ));
            if let Some(preserve_aspect_ratio) = preserve_aspect_ratio {
                attributes.push((
                    QualName::new(None, ns!(), local_name!("preserveAspectRatio")),
                    AttrValue::String(preserve_aspect_ratio),
                ));
            }
        }

        // With a viewBox, layout sizes the root's box and fits the viewBox into it; the
        // rasterizer only needs the viewBox's aspect ratio as the image's natural size. Left in,
        // `width`/`height` made that size wrong whenever CSS overrode one of them: `width="50"`
        // without `height` reads as 50x100 to the rasterizer (the missing height defaults to
        // 100%), so the icon was fitted into its box at half size.
        if std::ptr::eq(element, self.upcast::<Element>()) &&
            element
                .get_attribute_string_value_with_namespace(&ns!(), &local_name!("viewBox"))
                .is_some_and(|view_box| parse_view_box(&view_box).is_some())
        {
            attributes.retain(|(name, _)| {
                name.ns != ns!() ||
                    (name.local != local_name!("width") && name.local != local_name!("height"))
            });
        } else if std::ptr::eq(element, self.upcast::<Element>()) &&
            let Some(style) = element.style_from_last_restyle()
        {
            // Without a viewBox the rasterizer resolves percentages in the content (a `<use>`'s
            // default `100%` size, GitLab's CSS-sized sprite icons) against its 100x100
            // fallback viewport; browsers resolve them against the CSS box. An absolute CSS
            // size therefore becomes the serialized viewport size.
            let position = style.get_position();
            for (name, size) in [
                (local_name!("width"), &position.width),
                (local_name!("height"), &position.height),
            ] {
                let StyleSize::LengthPercentage(length_percentage) = size else {
                    continue;
                };
                let Some(length) = length_percentage.0.to_length() else {
                    continue;
                };
                attributes.retain(|(attribute, _)| attribute.ns != ns!() || attribute.local != name);
                attributes.push((
                    QualName::new(None, ns!(), name),
                    AttrValue::String(length.px().to_string()),
                ));
            }
        }

        let Some(declarations) = self.paint_declarations(element) else {
            return;
        };
        let style_name = QualName::new(None, ns!(), local_name!("style"));
        match attributes.iter_mut().find(|(name, _)| *name == style_name) {
            // Appended, so they win over the inline text they were computed from: the computed
            // values already include it, in a form the consumer understands (inline text can hold
            // `var()` or `currentcolor` that a standalone SVG renderer can't resolve).
            // Custom properties and `var()` uses are dropped from the inline text: resvg's CSS
            // parser stops at a `--name` and discards every later declaration, the appended
            // ones included, and their computed results are what gets appended.
            Some((_, value)) => {
                // Joined without empty entries: resvg also gives up on a leading `;`.
                let kept: Vec<&str> = value
                    .split(';')
                    .map(str::trim)
                    .filter(|declaration| {
                        !declaration.is_empty() &&
                            !declaration.starts_with("--") &&
                            !declaration.contains("var(")
                    })
                    .chain(std::iter::once(declarations.as_str()))
                    .collect();
                *value = AttrValue::String(kept.join(";"));
            },
            None => attributes.push((style_name, AttrValue::String(declarations))),
        }
    }

    /// CSS declarations that carry `element`'s cascaded paint properties (see
    /// `SVG_PAINT_PROPERTIES`) into the standalone serialization, which otherwise sees none of
    /// the page's stylesheets, inherited values or custom properties. A property is copied where
    /// a rule set it, i.e. where it differs from the flat-tree parent's value, and wherever a
    /// presentation attribute names it: those are in the cascade (see `SVGElement`'s
    /// `parse_plain_attribute`), so the computed value says whether a rule overrode the
    /// attribute, even with a value equal to the inherited one. The root `<svg>` also gets the
    /// inherited properties it merely inherits from the page, since the standalone document has
    /// no ancestors. Unstyled elements (inside `display: none` subtrees, clones made for
    /// serialization) inherit from the serialized parent like the cascade would, with the
    /// custom properties in their `var()` presentation attributes substituted.
    fn paint_declarations(&self, element: &Element) -> Option<String> {
        let style = element.style_from_last_restyle();
        let parent_style = element
            .upcast::<Node>()
            .parent_in_flat_tree()
            .and_then(DomRoot::downcast::<Element>)
            .and_then(|parent| parent.style_from_last_restyle());
        let custom_property_style = style.clone().or_else(|| {
            element
                .upcast::<Node>()
                .inclusive_ancestors_in_flat_tree()
                .filter_map(DomRoot::downcast::<Element>)
                .find_map(|ancestor| ancestor.style_from_last_restyle())
        });
        let is_root = std::ptr::eq(element, self.upcast::<Element>());
        // Inline declarations are serialized too, and resvg can't read all of them (`var()`,
        // lowercase `currentcolor`), so every paint property they name is re-emitted computed.
        let inline_style = element.get_attribute_string_value_with_namespace(
            &ns!(),
            &local_name!("style"),
        );

        let mut declarations = String::new();
        for property in SVG_PAINT_PROPERTIES {
            let attribute = element.get_attribute_string_value_with_namespace(
                &ns!(),
                &LocalName::from(property.name()),
            );
            let from_cascade = style.as_ref().and_then(|style| {
                let value = rasterizer_value(style, property);
                let set_by_a_rule = parent_style
                    .as_ref()
                    .is_none_or(|parent| rasterizer_value(parent, property) != value);
                let inherited_into_root = is_root && property.inherited();
                let named_inline = inline_style
                    .as_deref()
                    .is_some_and(|style| style.contains(property.name()));
                (set_by_a_rule || inherited_into_root || attribute.is_some() || named_inline)
                    .then_some(value)
            });
            let value = from_cascade.or_else(|| {
                let attribute = attribute.filter(|attribute| attribute.contains("var("))?;
                let style = custom_property_style.as_ref()?;
                let substituted = substitute_custom_properties(&attribute, style)?;
                Some(self.rasterizer_text(property, &substituted, style))
            });
            if let Some(value) = value {
                declarations.push_str(property.name());
                declarations.push(':');
                declarations.push_str(&value);
                declarations.push(';');
            }
        }
        (!declarations.is_empty()).then_some(declarations)
    }

    /// `text` (a property value after `var()` substitution) with any colour in it rewritten as
    /// the rasterizer needs, see [`rasterizer_value`]; `none`, `url(#…)` pass through.
    fn rasterizer_text(&self, property: LonghandId, text: &str, style: &ComputedValues) -> String {
        if !matches!(
            property,
            LonghandId::Color | LonghandId::Fill | LonghandId::Stroke | LonghandId::StopColor
        ) {
            return text.to_owned();
        }
        let document = self.owner_document();
        let url = document.url().into_url().into();
        let context = ParserContext::new(
            Origin::Author,
            &url,
            None,
            ParsingMode::DEFAULT,
            document.quirks_mode(),
            /* namespaces = */ Default::default(),
            None,
            None,
            /* attr_taint = */ Default::default(),
        );
        let mut input = ParserInput::new(text);
        let mut parser = Parser::new(&mut input);
        let color = parser.parse_entirely(|parser| SpecifiedColor::parse(&context, parser));
        match color
            .ok()
            .and_then(|color| color.to_computed_color(None).ok())
        {
            Some(color) => {
                rasterizer_paint_color(&color, &style.get_inherited_text().clone_color())
            },
            None => text.to_owned(),
        }
    }

    fn evict_cached_images(&self) {
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
    }

    pub(crate) fn invalidate_cached_serialized_subtree_and_rasterization_result(&self) {
        self.evict_cached_images();
        *self.cached_serialized_data_url.borrow_mut() = None;
        *self.cached_paint_signature.borrow_mut() = None;
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

/// Serialize the transitive same-document reference closure of `subtree` — every element
/// reached through `url(#id)` paint/effect attributes or `href="#id"`/`xlink:href="#id"`
/// (`<use>`) from inside `subtree`, excluding elements already within it. The synthesized
/// standalone mask document embeds the result inside `<defs>` (referenced-only definitions
/// must not paint on their own). Cycles and duplicates are cut by the id set; targets are
/// required to live inside some `<svg>`, mirroring `process_external_references`.
fn serialize_reference_closure(cx: &mut JSContext, subtree: &Node) -> String {
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

    let document = subtree.owner_doc();
    let mut processed_ids: HashSet<String> = HashSet::new();
    let mut serialized = String::new();
    let mut pending: Vec<DomRoot<Node>> = vec![DomRoot::from_ref(subtree)];

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
            let href = element.get_string_attribute(&local_name!("href"));
            let effective_href = if href.str().is_empty() {
                element
                    .get_attribute_string_value_with_namespace(&ns!(xlink), &local_name!("href"))
                    .unwrap_or_default()
            } else {
                href.to_string()
            };
            if let Some(id) = effective_href.strip_prefix('#') {
                if !id.is_empty() {
                    referenced_ids.push(id.to_owned());
                }
            }
        }

        for id in referenced_ids {
            if !processed_ids.insert(id.clone()) {
                continue;
            }
            let Some(referenced_element) = document.GetElementById(cx, DOMString::from(id)) else {
                continue;
            };
            let referenced_node = referenced_element.upcast::<Node>();
            if subtree.is_inclusive_ancestor_of(referenced_node) {
                continue;
            }
            let has_svg_ancestor = referenced_node
                .inclusive_ancestors(ShadowIncluding::No)
                .any(|ancestor| ancestor.is::<SVGSVGElement>());
            if !has_svg_ancestor {
                continue;
            }
            if let Ok(xml) = referenced_node.xml_serialize(TraversalScope::IncludeNode) {
                serialized.push_str(&String::from(xml));
            }
            pending.push(DomRoot::from_ref(referenced_node));
        }
    }
    serialized
}

/// `property`'s computed value written for the inline-SVG rasterizer (resvg) rather than as
/// CSS: colours become sRGB `rgba()` (resvg has no `oklab()`, `color-mix()`, `light-dark()`, …),
/// `currentcolor` keeps the `currentColor` spelling resvg requires, so it keeps tracking `color`,
/// and paint-server urls become in-document `url(#id)` references.
fn rasterizer_value(style: &ComputedValues, property: LonghandId) -> String {
    let current_color = style.get_inherited_text().clone_color();
    match property {
        LonghandId::Color => rasterizer_color(&current_color),
        LonghandId::StopColor => rasterizer_color(
            &style
                .get_svg()
                .clone_stop_color()
                .resolve_to_absolute(&current_color),
        ),
        LonghandId::Fill => {
            rasterizer_paint(&style.get_inherited_svg().clone_fill(), &current_color)
        },
        LonghandId::Stroke => {
            rasterizer_paint(&style.get_inherited_svg().clone_stroke(), &current_color)
        },
        _ => {
            let mut value = String::new();
            style
                .computed_or_resolved_value(property, None, &mut value)
                .expect("Writing CSS to a String cannot fail");
            value
        },
    }
}

fn rasterizer_color(color: &AbsoluteColor) -> String {
    let [red, green, blue, alpha] = *color.clone().into_srgb_legacy().raw_components();
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "rgba({}, {}, {}, {})",
        channel(red),
        channel(green),
        channel(blue),
        alpha.clamp(0.0, 1.0)
    )
}

fn rasterizer_paint_color(color: &ComputedColor, current_color: &AbsoluteColor) -> String {
    if color.is_currentcolor() {
        "currentColor".to_owned()
    } else {
        rasterizer_color(&color.resolve_to_absolute(current_color))
    }
}

fn rasterizer_paint(paint: &SVGPaint, current_color: &AbsoluteColor) -> String {
    let kind = match &paint.kind {
        SVGPaintKind::None => "none".to_owned(),
        SVGPaintKind::Color(color) => rasterizer_paint_color(color, current_color),
        // Paint servers live in the page; a reference that resolves to no fragment can't.
        SVGPaintKind::PaintServer(_) => match paint_server_id(paint) {
            Some(id) => format!("url(#{id})"),
            None => "none".to_owned(),
        },
        SVGPaintKind::ContextFill => "context-fill".to_owned(),
        SVGPaintKind::ContextStroke => "context-stroke".to_owned(),
    };
    match &paint.fallback {
        SVGPaintFallback::Unset => kind,
        SVGPaintFallback::None => format!("{kind} none"),
        SVGPaintFallback::Color(color) => {
            format!("{kind} {}", rasterizer_paint_color(color, current_color))
        },
    }
}

/// The element id a paint-server `paint` references, if any.
fn paint_server_id(paint: &SVGPaint) -> Option<String> {
    let SVGPaintKind::PaintServer(url) = &paint.kind else {
        return None;
    };
    let url = url.url()?;
    Some(percent_decode_str(url.fragment()?).decode_utf8_lossy().into_owned())
}

/// `value` with each `var(--name[, fallback])` replaced by the custom property's computed value
/// in `style`, or else its fallback. `None` when a reference has neither, which makes the whole
/// value invalid.
fn substitute_custom_properties(value: &str, style: &ComputedValues) -> Option<String> {
    let mut substituted = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("var(") {
        substituted.push_str(&rest[..start]);
        let arguments = &rest[start + "var(".len()..];
        let mut depth = 1;
        let end = arguments.char_indices().find_map(|(index, character)| {
            match character {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {},
            }
            (depth == 0).then_some(index)
        })?;
        let (name, fallback) = match arguments[..end].split_once(',') {
            Some((name, fallback)) => (name.trim(), Some(fallback.trim())),
            None => (arguments[..end].trim(), None),
        };
        let name = Atom::from(name.strip_prefix("--")?);
        let custom_value = style.computed_value_to_string(PropertyDeclarationId::Custom(&name));
        if custom_value.trim().is_empty() {
            substituted.push_str(&substitute_custom_properties(fallback?, style)?);
        } else {
            substituted.push_str(custom_value.trim());
        }
        rest = &arguments[end + 1..];
    }
    substituted.push_str(rest);
    Some(substituted)
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
            source_paint_signature: unsafe {
                self.unsafe_get()
                    .cached_paint_signature
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

        self.unregister_referenced_ids();

        self.invalidate_cached_serialized_subtree_and_rasterization_result();
    }
}

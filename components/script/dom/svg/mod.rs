/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

pub(crate) mod svgaelement;
pub(crate) mod svganimatedstring;
pub(crate) mod svgelement;
pub(crate) mod svggraphicselement;
pub(crate) mod svgimageelement;
pub(crate) mod svgsvgelement;
pub(crate) mod svguseelement;

/// Defines a module per SVG element interface that has no members implemented beyond its
/// inheritance chain. Creating the element still has to produce the right interface: pages
/// branch on `instanceof SVGAElement` and friends (crates.io's router, NYT), and a
/// `ReferenceError` on an unknown interface name aborts the whole script.
macro_rules! svg_element_interfaces {
    ($($kind:ident $module:ident::$name:ident : $parent_module:ident::$parent:ident;)*) => {
        $(svg_element_interfaces!(@define $kind $module $name $parent_module $parent);)*
    };
    (@define $kind:ident $module:ident $name:ident $parent_module:ident $parent:ident) => {
        pub(crate) mod $module {
            use dom_struct::dom_struct;
            use html5ever::{LocalName, Prefix};

            use crate::dom::document::Document;
            use crate::dom::svg::$parent_module::$parent;

            #[dom_struct]
            pub(crate) struct $name {
                parent: $parent,
            }

            impl $name {
                pub(crate) fn new_inherited(
                    local_name: LocalName,
                    prefix: Option<Prefix>,
                    document: &Document,
                ) -> $name {
                    $name {
                        parent: $parent::new_inherited(local_name, prefix, document),
                    }
                }
            }

            svg_element_interfaces!(@constructor $kind $name);
        }
    };
    (@constructor abstract_interface $name:ident) => {};
    (@constructor interface $name:ident) => {
        impl $name {
            pub(crate) fn new(
                cx: &mut js::context::JSContext,
                local_name: LocalName,
                prefix: Option<Prefix>,
                document: &Document,
                proto: Option<js::rust::HandleObject>,
            ) -> crate::dom::bindings::root::DomRoot<$name> {
                crate::dom::node::Node::reflect_node_with_proto(
                    cx,
                    Box::new($name::new_inherited(local_name, prefix, document)),
                    document,
                    proto,
                )
            }
        }
    };
}

svg_element_interfaces! {
    abstract_interface svggeometryelement::SVGGeometryElement : svggraphicselement::SVGGraphicsElement;
    interface svggelement::SVGGElement : svggraphicselement::SVGGraphicsElement;
    interface svgdefselement::SVGDefsElement : svggraphicselement::SVGGraphicsElement;
    interface svgsymbolelement::SVGSymbolElement : svggraphicselement::SVGGraphicsElement;
    interface svgswitchelement::SVGSwitchElement : svggraphicselement::SVGGraphicsElement;
    interface svgforeignobjectelement::SVGForeignObjectElement : svggraphicselement::SVGGraphicsElement;
    interface svgdescelement::SVGDescElement : svgelement::SVGElement;
    interface svgtitleelement::SVGTitleElement : svgelement::SVGElement;
    interface svgmetadataelement::SVGMetadataElement : svgelement::SVGElement;
    interface svgpathelement::SVGPathElement : svggeometryelement::SVGGeometryElement;
    interface svgrectelement::SVGRectElement : svggeometryelement::SVGGeometryElement;
    interface svgcircleelement::SVGCircleElement : svggeometryelement::SVGGeometryElement;
    interface svgellipseelement::SVGEllipseElement : svggeometryelement::SVGGeometryElement;
    interface svglineelement::SVGLineElement : svggeometryelement::SVGGeometryElement;
    interface svgpolylineelement::SVGPolylineElement : svggeometryelement::SVGGeometryElement;
    interface svgpolygonelement::SVGPolygonElement : svggeometryelement::SVGGeometryElement;
    abstract_interface svgtextcontentelement::SVGTextContentElement : svggraphicselement::SVGGraphicsElement;
    abstract_interface svgtextpositioningelement::SVGTextPositioningElement : svgtextcontentelement::SVGTextContentElement;
    interface svgtextelement::SVGTextElement : svgtextpositioningelement::SVGTextPositioningElement;
    interface svgtspanelement::SVGTSpanElement : svgtextpositioningelement::SVGTextPositioningElement;
    interface svgtextpathelement::SVGTextPathElement : svgtextcontentelement::SVGTextContentElement;
    abstract_interface svggradientelement::SVGGradientElement : svgelement::SVGElement;
    interface svglineargradientelement::SVGLinearGradientElement : svggradientelement::SVGGradientElement;
    interface svgradialgradientelement::SVGRadialGradientElement : svggradientelement::SVGGradientElement;
    interface svgstopelement::SVGStopElement : svgelement::SVGElement;
    interface svgpatternelement::SVGPatternElement : svgelement::SVGElement;
    interface svgmarkerelement::SVGMarkerElement : svgelement::SVGElement;
    interface svgclippathelement::SVGClipPathElement : svgelement::SVGElement;
    interface svgmaskelement::SVGMaskElement : svgelement::SVGElement;
    interface svgfilterelement::SVGFilterElement : svgelement::SVGElement;
}

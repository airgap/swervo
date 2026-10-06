/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::borrow::Cow;

use layout_api::{
    LayoutElement, LayoutElementType, LayoutNode, LayoutNodeType, PseudoElementChain,
};
use script::layout_dom::ServoLayoutNode;
use servo_arc::Arc as ServoArc;
use style::dom::NodeInfo;
use style::properties::ComputedValues;
use style::selector_parser::PseudoElement;
use style::str::char_is_whitespace;
use style::values::generics::counters::{Content, ContentItem};
use style::values::specified::Quotes;
use style::values::specified::box_::Display as StyloDisplay;
use web_atoms::LocalName;

use crate::context::LayoutContext;
use crate::dom::{BoxSlot, LayoutBox, NodeExt};
use crate::flow::inline::SharedInlineStyles;
use crate::lists::{
    apply_quote_item, counter_values, generate_counter_representation, list_item_ordinal,
    quote_depth,
};
use crate::quotes::quotes_for_lang;
use crate::replaced::ReplacedContents;
use crate::style_ext::{Display, DisplayGeneratingBox, DisplayInside, DisplayOutside};

/// A data structure used to pass and store related layout information together to
/// avoid having to repeat the same arguments in argument lists.
#[derive(Clone)]
pub(crate) struct NodeAndStyleInfo<'dom> {
    pub node: ServoLayoutNode<'dom>,
    pub style: ServoArc<ComputedValues>,
}

impl<'dom> NodeAndStyleInfo<'dom> {
    pub(crate) fn new(node: ServoLayoutNode<'dom>, style: ServoArc<ComputedValues>) -> Self {
        Self { node, style }
    }

    pub(crate) fn pseudo_element_chain(&self) -> PseudoElementChain {
        self.node.pseudo_element_chain()
    }

    pub(crate) fn with_pseudo_element(
        &self,
        context: &LayoutContext,
        pseudo_element_type: PseudoElement,
    ) -> Option<Self> {
        let element = self.node.as_element()?.with_pseudo(pseudo_element_type)?;
        let style = element.style(&context.style_context);
        Some(NodeAndStyleInfo {
            node: element.as_node(),
            style,
        })
    }
}

#[derive(Debug)]
pub(super) enum Contents {
    /// Any kind of content that is not replaced nor a widget, including the contents of pseudo-elements.
    NonReplaced(NonReplacedContents),
    /// A widget with native appearance. This has several behavior in common with replaced elements,
    /// but isn't fully replaced (see discussion in <https://github.com/w3c/csswg-drafts/issues/12876>).
    /// Examples: `<input>`, `<textarea>`, `<select>`...
    /// <https://drafts.csswg.org/css-ui/#widget>
    Widget(NonReplacedContents),
    /// Example: an `<img src=…>` element.
    /// <https://drafts.csswg.org/css2/conform.html#replaced-element>
    Replaced(ReplacedContents),
}

#[derive(Debug)]
pub(super) enum NonReplacedContents {
    /// Refers to a DOM subtree, plus `::before` and `::after` pseudo-elements.
    OfElement,
    /// Content of a `::before` or `::after` pseudo-element that is being generated.
    /// <https://drafts.csswg.org/css2/generate.html#content>
    OfPseudoElement(Vec<PseudoElementContentItem>),
}

#[derive(Debug)]
pub(super) enum PseudoElementContentItem {
    Text(String),
    Replaced(ReplacedContents),
}

pub(super) trait TraversalHandler<'dom> {
    fn handle_text(&mut self, info: &NodeAndStyleInfo<'dom>, text: Cow<'dom, str>);

    /// Or pseudo-element
    fn handle_element(
        &mut self,
        info: &NodeAndStyleInfo<'dom>,
        display: DisplayGeneratingBox,
        contents: Contents,
        box_slot: BoxSlot<'dom>,
    );

    /// Notify the handler that we are about to recurse into a `display: contents` element.
    fn enter_display_contents(&mut self, _: SharedInlineStyles);

    /// Notify the handler that we have finished a `display: contents` element.
    fn leave_display_contents(&mut self);
}

fn traverse_children_of<'dom>(
    parent_element_info: &NodeAndStyleInfo<'dom>,
    context: &LayoutContext,
    handler: &mut impl TraversalHandler<'dom>,
) {
    parent_element_info
        .node
        .set_uses_content_attribute_with_attr(false);

    let is_element = parent_element_info.pseudo_element_chain().is_empty();
    if is_element {
        traverse_eager_pseudo_element(PseudoElement::Before, parent_element_info, context, handler);
    }

    // Inside a native-foreignObject `<svg>` host only `<foreignObject>`s become boxes (in the
    // host's widget); every other svg element paints through the host's rasterized image.
    if parent_element_info
        .node
        .as_element()
        .is_some_and(|element| element.is_svg_element() && !is_foreign_object(&element))
    {
        traverse_svg_foreign_objects(parent_element_info.node, context, handler);
    } else {
        for child in parent_element_info.node.flat_tree_children() {
            if child.is_text_node() {
                let info = NodeAndStyleInfo::new(child, child.style(&context.style_context));
                handler.handle_text(&info, child.text_content());
            } else if child.is_element() {
                traverse_element(child, context, handler);
            }
        }
    }

    if is_element {
        traverse_eager_pseudo_element(PseudoElement::After, parent_element_info, context, handler);
    }
}

pub(crate) fn is_foreign_object<'dom>(element: &impl LayoutElement<'dom>) -> bool {
    element.is_svg_element() && element.local_name() == "foreignObject"
}

/// Traverse the `<foreignObject>` descendants of an svg element, looking through svg containers
/// (`<g>`, `<a>`, `<switch>`, …) that are themselves painted as part of the rasterized svg. Text,
/// `<title>`, `<desc>` and the like never lay out as boxes; a container hidden with
/// `display: none` hides the foreignObjects inside it.
fn traverse_svg_foreign_objects<'dom>(
    svg_parent: ServoLayoutNode<'dom>,
    context: &LayoutContext,
    handler: &mut impl TraversalHandler<'dom>,
) {
    for child in svg_parent.flat_tree_children() {
        let Some(element) = child.as_element() else {
            continue;
        };
        if !element.is_svg_element() {
            continue;
        }
        if is_foreign_object(&element) {
            traverse_element(child, context, handler);
        } else if !matches!(
            Display::from(child.style(&context.style_context).get_box().display),
            Display::None
        ) {
            traverse_svg_foreign_objects(child, context, handler);
        }
    }
}

fn traverse_element<'dom>(
    element: ServoLayoutNode<'dom>,
    context: &LayoutContext,
    handler: &mut impl TraversalHandler<'dom>,
) {
    let style = element.style(&context.style_context);
    let info = NodeAndStyleInfo::new(element, style);

    match Display::from(info.style.get_box().display) {
        Display::None => {},
        Display::Contents => {
            if ReplacedContents::for_element(element, context).is_some() {
                // `display: content` on a replaced element computes to `display: none`
                // <https://drafts.csswg.org/css-display-3/#valdef-display-contents>
                element.unset_all_boxes()
            } else {
                let shared_inline_styles =
                    SharedInlineStyles::from_info_and_context(&info, context);
                element
                    .box_slot()
                    .set(LayoutBox::DisplayContents(shared_inline_styles.clone()));

                handler.enter_display_contents(shared_inline_styles);
                traverse_children_of(&info, context, handler);
                handler.leave_display_contents();
            }
        },
        Display::GeneratingBox(display) => {
            if info.style.in_top_layer() {
                traverse_backdrop_pseudo_element(&info, context, handler);
            }
            let contents = Contents::for_element(element, context);
            let display = display.used_value_for_contents(&contents);
            let box_slot = element.box_slot();
            handler.handle_element(&info, display, contents, box_slot);
        },
    }
}

/// Every element in the top layer has a `::backdrop` box, which is in the top layer directly
/// below it. It is built just before the element's box, so it is hoisted and painted first.
/// <https://drafts.csswg.org/css-position-4/#backdrop>
fn traverse_backdrop_pseudo_element<'dom>(
    node_info: &NodeAndStyleInfo<'dom>,
    context: &LayoutContext,
    handler: &mut impl TraversalHandler<'dom>,
) {
    let Some(backdrop_info) = node_info.with_pseudo_element(context, PseudoElement::Backdrop)
    else {
        return;
    };
    let Display::GeneratingBox(display) = Display::from(backdrop_info.style.get_box().display)
    else {
        return;
    };
    let box_slot = backdrop_info.node.box_slot();
    handler.handle_element(
        &backdrop_info,
        display,
        Contents::for_pseudo_element(Vec::new()),
        box_slot,
    );
}

/// A ruby base and the annotation paired with it, the unit that ruby layout stacks.
/// <https://drafts.csswg.org/css-ruby/#ruby-pairing>
pub(crate) struct RubyColumn<'dom> {
    base: Vec<ServoLayoutNode<'dom>>,
    annotation: Option<ServoLayoutNode<'dom>>,
}

/// The contents of a `display: ruby` element, in order.
pub(crate) enum RubyItem<'dom> {
    Column(RubyColumn<'dom>),
    /// White space between two columns, or after the last one, which separates them like a
    /// space in the surrounding text, as in Chrome.
    WhiteSpace,
}

/// Pairs the children of a `display: ruby` element into columns: each `display: ruby-text`
/// child annotates the content since the previous annotation. This covers the common
/// `<ruby>base<rt>text</rt>base<rt>text</rt></ruby>` markup but neither ruby text containers
/// nor nested rubies. `display: none` children such as `<rp>` produce no boxes.
pub(crate) fn ruby_items<'dom>(
    ruby_info: &NodeAndStyleInfo<'dom>,
    context: &LayoutContext,
) -> Vec<RubyItem<'dom>> {
    let mut items = Vec::new();
    let mut base: Vec<ServoLayoutNode<'dom>> = Vec::new();
    let push_column = |items: &mut Vec<RubyItem<'dom>>,
                       base: Vec<ServoLayoutNode<'dom>>,
                       annotation: Option<ServoLayoutNode<'dom>>| {
        let starts_with_white_space = base.first().is_some_and(|node| {
            node.is_text_node() && node.text_content().starts_with(char_is_whitespace)
        });
        if starts_with_white_space && !items.is_empty() {
            items.push(RubyItem::WhiteSpace);
        }
        items.push(RubyItem::Column(RubyColumn { base, annotation }));
    };
    for child in ruby_info.node.flat_tree_children() {
        if child.is_text_node() {
            base.push(child);
            continue;
        }
        if !child.is_element() {
            continue;
        }
        let display = child.style(&context.style_context).get_box().display;
        if display == StyloDisplay::RubyText {
            push_column(&mut items, std::mem::take(&mut base), Some(child));
        } else if Display::from(display) != Display::None {
            base.push(child);
        }
    }
    let base_is_white_space = base
        .iter()
        .all(|node| node.is_text_node() && node.text_content().chars().all(char_is_whitespace));
    if !base_is_white_space {
        push_column(&mut items, base, None);
    } else if !base.is_empty() && !items.is_empty() {
        items.push(RubyItem::WhiteSpace);
    }
    items
}

/// Feeds a ruby column to `handler`: the annotation first, as a block so that it stacks
/// above the base, and then the base content.
pub(crate) fn traverse_ruby_column<'dom>(
    column: &RubyColumn<'dom>,
    context: &LayoutContext,
    handler: &mut impl TraversalHandler<'dom>,
) {
    if let Some(annotation) = column.annotation {
        let info = NodeAndStyleInfo::new(annotation, annotation.style(&context.style_context));
        let contents = Contents::for_element(annotation, context);
        let display = DisplayGeneratingBox::OutsideInside {
            outside: DisplayOutside::Block,
            inside: DisplayInside::Flow {
                is_list_item: false,
            },
        };
        handler.handle_element(&info, display, contents, annotation.box_slot());
    }
    for &node in &column.base {
        if node.is_text_node() {
            let info = NodeAndStyleInfo::new(node, node.style(&context.style_context));
            handler.handle_text(&info, node.text_content());
        } else {
            traverse_element(node, context, handler);
        }
    }
}

fn traverse_eager_pseudo_element<'dom>(
    pseudo_element_type: PseudoElement,
    node_info: &NodeAndStyleInfo<'dom>,
    context: &LayoutContext,
    handler: &mut impl TraversalHandler<'dom>,
) {
    assert!(pseudo_element_type.is_eager());

    // If this node doesn't have this eager pseudo-element, exit early. This depends on
    // the style applied to the element.
    let Some(pseudo_element_info) = node_info.with_pseudo_element(context, pseudo_element_type)
    else {
        return;
    };
    if pseudo_element_info.style.ineffective_content_property() {
        return;
    }

    match Display::from(pseudo_element_info.style.get_box().display) {
        Display::None => {},
        Display::Contents => {
            let items = generate_pseudo_element_content(&pseudo_element_info, context);
            let box_slot = pseudo_element_info.node.box_slot();
            let shared_inline_styles =
                SharedInlineStyles::from_info_and_context(&pseudo_element_info, context);
            box_slot.set(LayoutBox::DisplayContents(shared_inline_styles.clone()));

            handler.enter_display_contents(shared_inline_styles);
            traverse_pseudo_element_contents(&pseudo_element_info, context, handler, items);
            handler.leave_display_contents();
        },
        Display::GeneratingBox(display) => {
            let items = generate_pseudo_element_content(&pseudo_element_info, context);
            let box_slot = pseudo_element_info.node.box_slot();
            let contents = Contents::for_pseudo_element(items);
            handler.handle_element(&pseudo_element_info, display, contents, box_slot);
        },
    }
}

fn traverse_pseudo_element_contents<'dom>(
    info: &NodeAndStyleInfo<'dom>,
    context: &LayoutContext,
    handler: &mut impl TraversalHandler<'dom>,
    items: Vec<PseudoElementContentItem>,
) {
    let mut anonymous_info = None;
    for item in items {
        match item {
            PseudoElementContentItem::Text(text) => handler.handle_text(info, text.into()),
            PseudoElementContentItem::Replaced(contents) => {
                let anonymous_info = anonymous_info.get_or_insert_with(|| {
                    info.with_pseudo_element(context, PseudoElement::ServoAnonymousBox)
                        .unwrap_or_else(|| info.clone())
                });
                let display_inline = DisplayGeneratingBox::OutsideInside {
                    outside: DisplayOutside::Inline,
                    inside: DisplayInside::Flow {
                        is_list_item: false,
                    },
                };
                // `display` is not inherited, so we get the initial value
                debug_assert!(
                    Display::from(anonymous_info.style.get_box().display) ==
                        Display::GeneratingBox(display_inline)
                );
                handler.handle_element(
                    anonymous_info,
                    display_inline,
                    Contents::Replaced(contents),
                    anonymous_info.node.box_slot(),
                )
            },
        }
    }
}

impl Contents {
    /// Returns true iff the `try_from` impl below would return `Err(_)`
    pub fn is_replaced(&self) -> bool {
        matches!(self, Contents::Replaced(_))
    }

    pub(crate) fn for_element(node: ServoLayoutNode<'_>, context: &LayoutContext) -> Self {
        let is_widget = matches!(
            node.type_id(),
            Some(LayoutNodeType::Element(
                LayoutElementType::HTMLInputElement |
                    LayoutElementType::HTMLSelectElement |
                    LayoutElementType::HTMLTextAreaElement
            ))
        );
        if is_widget {
            Self::Widget(NonReplacedContents::OfElement)
        } else if let Some(replaced) = ReplacedContents::for_element(node, context) {
            Self::Replaced(replaced)
        } else {
            Self::NonReplaced(NonReplacedContents::OfElement)
        }
    }

    pub(crate) fn for_pseudo_element(contents: Vec<PseudoElementContentItem>) -> Self {
        Self::NonReplaced(NonReplacedContents::OfPseudoElement(contents))
    }

    pub(crate) fn non_replaced_contents(self) -> Option<NonReplacedContents> {
        match self {
            Self::NonReplaced(contents) | Self::Widget(contents) => Some(contents),
            Self::Replaced(_) => None,
        }
    }
}

impl NonReplacedContents {
    pub(crate) fn traverse<'dom>(
        self,
        context: &LayoutContext,
        info: &NodeAndStyleInfo<'dom>,
        handler: &mut impl TraversalHandler<'dom>,
    ) {
        match self {
            NonReplacedContents::OfElement => traverse_children_of(info, context, handler),
            NonReplacedContents::OfPseudoElement(items) => {
                traverse_pseudo_element_contents(info, context, handler, items)
            },
        }
    }
}

fn get_quote_from_pair<I, S>(item: &ContentItem<I>, opening: &S, closing: &S) -> String
where
    S: ToString + ?Sized,
{
    match item {
        ContentItem::OpenQuote => opening.to_string(),
        ContentItem::CloseQuote => closing.to_string(),
        _ => unreachable!("Got an unexpected ContentItem type when processing quotes."),
    }
}

fn is_quote_item<I>(item: &ContentItem<I>) -> bool {
    matches!(
        item,
        ContentItem::OpenQuote |
            ContentItem::CloseQuote |
            ContentItem::NoOpenQuote |
            ContentItem::NoCloseQuote
    )
}

/// <https://www.w3.org/TR/CSS2/generate.html#propdef-content>
pub(crate) fn generate_pseudo_element_content(
    pseudo_element_info: &NodeAndStyleInfo,
    context: &LayoutContext,
) -> Vec<PseudoElementContentItem> {
    match &pseudo_element_info.style.get_counters().content {
        Content::Items(items) => {
            let mut vec = vec![];
            let mut current_quote_depth = if items.items.iter().any(is_quote_item) {
                quote_depth(context, pseudo_element_info.node)
            } else {
                0
            };
            for item in items.items.iter() {
                match item {
                    ContentItem::String(s) => {
                        vec.push(PseudoElementContentItem::Text(s.to_string()));
                    },
                    ContentItem::Attr(attr) => {
                        let element = pseudo_element_info
                            .node
                            .as_element()
                            .expect("Expected an element");

                        // From
                        // <https://html.spec.whatwg.org/multipage/#case-sensitivity-of-the-css-%27attr%28%29%27-function>
                        //
                        // > CSS Values and Units leaves the case-sensitivity of attribute names for
                        // > the purpose of the `attr()` function to be defined by the host language.
                        // > [[CSSVALUES]].
                        // >
                        // > When comparing the attribute name part of a CSS `attr()`function to the
                        // > names of namespace-less attributes on HTML elements in HTML documents,
                        // > the name part of the CSS `attr()` function must first be converted to
                        // > ASCII lowercase. The same function when compared to other attributes must
                        // > be compared according to its original case. In both cases, to match the
                        // > values must be identical to each other (and therefore the comparison is
                        // > case sensitive).
                        let attr_name = match element.is_html_element_in_html_document() {
                            true => &*attr.attribute.to_ascii_lowercase(),
                            false => &*attr.attribute,
                        };

                        pseudo_element_info
                            .node
                            .set_uses_content_attribute_with_attr(true);
                        let attr_val =
                            element.attribute(&attr.namespace_url, &LocalName::from(attr_name));
                        vec.push(PseudoElementContentItem::Text(
                            attr_val.map_or("".to_string(), |s| s.to_string()),
                        ));
                    },
                    ContentItem::Image(image) => {
                        if let Some(replaced_content) =
                            ReplacedContents::from_image(pseudo_element_info.node, context, image)
                        {
                            vec.push(PseudoElementContentItem::Replaced(replaced_content));
                        }
                    },
                    ContentItem::OpenQuote |
                    ContentItem::CloseQuote |
                    ContentItem::NoOpenQuote |
                    ContentItem::NoCloseQuote => {
                        let Some(depth) = apply_quote_item(item, &mut current_quote_depth) else {
                            continue;
                        };
                        // Levels deeper than the list of pairs reuse its last pair.
                        let maybe_quote = match &pseudo_element_info.style.get_list().quotes {
                            Quotes::QuoteList(quote_list) => {
                                quote_list.0.get(depth).or(quote_list.0.last()).map(
                                    |quote_pair| {
                                        get_quote_from_pair(
                                            item,
                                            &*quote_pair.opening,
                                            &*quote_pair.closing,
                                        )
                                    },
                                )
                            },
                            Quotes::Auto => {
                                let lang = &pseudo_element_info.style.get_font()._x_lang;
                                let quotes = quotes_for_lang(lang.0.as_ref(), depth);
                                Some(get_quote_from_pair(item, &quotes.opening, &quotes.closing))
                            },
                        };
                        if let Some(quote) = maybe_quote {
                            vec.push(PseudoElementContentItem::Text(quote));
                        }
                    },
                    ContentItem::Counter(name, style) => {
                        let value = if &*name.0 == "list-item" {
                            list_item_ordinal(context, pseudo_element_info.node)
                        } else {
                            *counter_values(context, pseudo_element_info.node, name)
                                .last()
                                .expect("A counter always has a value")
                        };
                        vec.push(PseudoElementContentItem::Text(
                            generate_counter_representation(style, value),
                        ));
                    },
                    ContentItem::Counters(name, separator, style) => {
                        // Nested `list-item` numbering (`counters(list-item, ".")`) only
                        // shows the innermost list's ordinal.
                        let values = if &*name.0 == "list-item" {
                            vec![list_item_ordinal(context, pseudo_element_info.node)]
                        } else {
                            counter_values(context, pseudo_element_info.node, name)
                        };
                        let text = values
                            .into_iter()
                            .map(|value| generate_counter_representation(style, value))
                            .collect::<Vec<_>>()
                            .join(separator);
                        vec.push(PseudoElementContentItem::Text(text));
                    },
                }
            }
            vec
        },
        Content::Normal | Content::None => unreachable!(),
    }
}

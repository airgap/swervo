/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use app_units::Au;
use euclid::{Point2D, Size2D};
use layout_api::{DangerousStyleElement, LayoutElement, LayoutNode};
use script::layout_dom::{ServoDangerousStyleElement, ServoLayoutNode};
use selectors::Element as _;
use style::counter_style::{CounterStyle, Symbol, SymbolsType};
use style::dom::{OpaqueNode, TElement};
use style::properties::ComputedValues;
use style::properties::longhands::list_style_position::computed_value::T as ListStylePosition;
use style::properties::longhands::list_style_type::computed_value::T as ListStyleType;
use style::selector_parser::PseudoElement;
use style::values::CustomIdent;
use style::values::computed::Image;
use style::values::generics::counters::{Content, ContentItem};
use stylo_atoms::atom;
use web_atoms::{local_name, ns};

use crate::context::LayoutContext;
use crate::dom_traversal::{
    NodeAndStyleInfo, PseudoElementContentItem, generate_pseudo_element_content,
};
use crate::geom::PhysicalRect;
use crate::replaced::ReplacedContents;

/// <https://drafts.csswg.org/css-lists/#content-property>
pub(crate) fn make_marker<'dom>(
    context: &LayoutContext,
    info: &NodeAndStyleInfo<'dom>,
) -> Option<(NodeAndStyleInfo<'dom>, Vec<PseudoElementContentItem>)> {
    let marker_info =
        info.with_pseudo_element(context, style::selector_parser::PseudoElement::Marker)?;
    let style = &marker_info.style;
    let list_style = style.get_list();

    // https://drafts.csswg.org/css-lists/#marker-image
    let marker_image = || match &list_style.list_style_image {
        Image::Url(url) => Some(vec![
            PseudoElementContentItem::Replaced(ReplacedContents::from_image_url(
                marker_info.node,
                context,
                url,
            )?),
            PseudoElementContentItem::Text(" ".into()),
        ]),
        // XXX: Non-None image types unimplemented.
        Image::ImageSet(..) |
        Image::Gradient(..) |
        Image::Image(..) |
        Image::CrossFade(..) |
        Image::PaintWorklet(..) |
        Image::None => None,
        Image::LightDark(..) => unreachable!("light-dark() should be disabled"),
    };

    let content = match &marker_info.style.get_counters().content {
        Content::Items(_) => generate_pseudo_element_content(&marker_info, context),
        Content::None => return None,
        Content::Normal => marker_image().or_else(|| {
            Some(vec![PseudoElementContentItem::Text(marker_string(
                &list_style.list_style_type,
                list_item_ordinal(context, info.node),
            )?)])
        })?,
    };

    Some((marker_info, content))
}

/// A disc, circle or square marking an outside list item. Chrome paints these as shapes sized
/// from the marker font's ascent rather than as the `•`/`◦`/`▪` glyphs their counter styles
/// name, and spaces them from the item by that ascent too. Fonts draw the glyphs at widely varying
/// sizes (Liberation Sans' bullet is about two thirds of Chrome's disc), so following Blink's
/// geometry (`ListMarker::InlineMarginsForOutside`, `ListMarker::RelativeSymbolMarkerRect`,
/// `ListMarkerPainter::PaintSymbol`) is what keeps list layouts from shifting.
#[derive(Clone, Copy)]
pub(crate) enum SymbolMarker {
    Disc,
    Circle,
    Square,
}

impl SymbolMarker {
    pub(crate) fn for_outside_marker(marker_style: &ComputedValues) -> Option<Self> {
        let list_style = marker_style.get_list();
        if marker_style.pseudo() != Some(PseudoElement::Marker) ||
            list_style.list_style_position != ListStylePosition::Outside ||
            !matches!(marker_style.get_counters().content, Content::Normal) ||
            !matches!(list_style.list_style_image, Image::None)
        {
            return None;
        }
        let CounterStyle::Name(name) = &list_style.list_style_type.0 else {
            return None;
        };
        match name.0 {
            atom!("disc") => Some(Self::Disc),
            atom!("circle") => Some(Self::Circle),
            atom!("square") => Some(Self::Square),
            _ => None,
        }
    }

    /// Blink works in whole pixels of the font's rounded ascent.
    fn ascent_px(ascent: Au) -> i32 {
        ascent.to_f32_px().round() as i32
    }

    /// How far the start of the marker box sits before the list item's content edge.
    pub(crate) fn inline_offset(ascent: Au) -> Au {
        // `kCMarkerPaddingPx` (7) plus one.
        Au::from_px(Self::ascent_px(ascent) * 2 / 3 + 8)
    }

    /// The shape's rect relative to the top-left corner of the marker text, whose top is the
    /// baseline minus `ascent`.
    pub(crate) fn rect(ascent: Au) -> PhysicalRect<Au> {
        let ascent = Self::ascent_px(ascent);
        let size = Au::from_px(ascent / 3);
        PhysicalRect::new(
            Point2D::new(
                Au::from_px(1),
                Au::from_px(3 * (ascent - ascent * 2 / 3) / 2),
            ),
            Size2D::new(size, size),
        )
    }
}

fn symbol_to_string(symbol: &Symbol) -> &str {
    match symbol {
        Symbol::String(string) => string,
        Symbol::Ident(ident) => &ident.0,
    }
}

/// The value of the `list-item` counter for the list item `node`, following HTML's ordinal-value
/// rules (<https://html.spec.whatwg.org/multipage/#ordinal-value>): the list owner's `start`
/// and `reversed`, each item's `value`. General CSS counters (`counter-reset`,
/// `counter-increment`) aren't implemented; this covers the list numbering pages rely on.
#[expect(unsafe_code)]
pub(crate) fn list_item_ordinal(context: &LayoutContext, node: ServoLayoutNode<'_>) -> i32 {
    let Some(element) = node.as_element() else {
        return 1;
    };
    let element = unsafe { element.dangerous_style_element() };
    let is_list_item = |element: &ServoDangerousStyleElement<'_>| {
        element
            .layout_element()
            .style(&context.style_context)
            .get_box()
            .display
            .is_list_item()
    };
    let integer_attribute = |element: &ServoDangerousStyleElement<'_>, name| {
        element
            .layout_element()
            .attribute_as_str(&ns!(), &name)
            .and_then(|value| value.trim().parse::<i32>().ok())
    };

    let mut items = vec![element];
    let mut sibling = element.prev_sibling_element();
    while let Some(previous) = sibling {
        sibling = previous.prev_sibling_element();
        if is_list_item(&previous) {
            items.push(previous);
        }
    }
    items.reverse();

    let owner = element.traversal_parent();
    let owner_is_ol = owner.is_some_and(|owner| {
        owner.layout_element().is_html_element_in_html_document() &&
            *owner.layout_element().local_name() == local_name!("ol")
    });
    let reversed = owner_is_ol &&
        owner.is_some_and(|owner| {
            owner
                .layout_element()
                .attribute(&ns!(), &local_name!("reversed"))
                .is_some()
        });
    let step = if reversed { -1 } else { 1 };
    let start = owner
        .filter(|_| owner_is_ol)
        .and_then(|owner| integer_attribute(&owner, local_name!("start")))
        .unwrap_or_else(|| {
            if !reversed {
                return 1;
            }
            // A reversed list counts down from its number of items.
            let mut count = 0;
            let mut child = owner.and_then(|owner| owner.first_element_child());
            while let Some(item) = child {
                if is_list_item(&item) {
                    count += 1;
                }
                child = item.next_sibling_element();
            }
            count
        });

    let mut ordinal = start - step;
    for item in items {
        ordinal = match integer_attribute(&item, local_name!("value")) {
            Some(value) if *item.layout_element().local_name() == local_name!("li") => value,
            _ => ordinal + step,
        };
    }
    ordinal
}

/// <https://drafts.csswg.org/css-counter-styles-3/#generate-a-counter>
pub(crate) fn generate_counter_representation(counter_style: &CounterStyle, value: i32) -> String {
    if let Some(representation) = numeric_counter_representation(counter_style, value) {
        return representation;
    }
    // Other counter styles are drawn for a value of 0.
    zero_counter_representation(counter_style).to_owned()
}

/// The predefined counter styles whose representation depends on the value: decimal and the
/// other decimal-digit scripts, alphabetic, roman and greek.
fn numeric_counter_representation(counter_style: &CounterStyle, value: i32) -> Option<String> {
    let CounterStyle::Name(name) = counter_style else {
        return None;
    };
    let digits = |zero: char| -> String {
        let mut representation: String = value
            .unsigned_abs()
            .to_string()
            .chars()
            .map(|digit| char::from_u32(zero as u32 + digit.to_digit(10).unwrap()).unwrap())
            .collect();
        if value < 0 {
            representation.insert(0, '-');
        }
        representation
    };
    let alphabetic = |symbols: &[char]| -> Option<String> {
        if value < 1 {
            return None;
        }
        let mut remaining = value as usize;
        let mut representation = vec![];
        while remaining > 0 {
            remaining -= 1;
            representation.push(symbols[remaining % symbols.len()]);
            remaining /= symbols.len();
        }
        Some(representation.into_iter().rev().collect())
    };
    let roman = |upper: bool| -> Option<String> {
        if !(1..=3999).contains(&value) {
            return None;
        }
        const NUMERALS: [(i32, &str); 13] = [
            (1000, "m"),
            (900, "cm"),
            (500, "d"),
            (400, "cd"),
            (100, "c"),
            (90, "xc"),
            (50, "l"),
            (40, "xl"),
            (10, "x"),
            (9, "ix"),
            (5, "v"),
            (4, "iv"),
            (1, "i"),
        ];
        let mut remaining = value;
        let mut representation = String::new();
        for (amount, numeral) in NUMERALS {
            while remaining >= amount {
                representation.push_str(numeral);
                remaining -= amount;
            }
        }
        Some(if upper {
            representation.to_uppercase()
        } else {
            representation
        })
    };
    let lower_latin: Vec<char> = ('a'..='z').collect();
    let upper_latin: Vec<char> = ('A'..='Z').collect();
    let lower_greek: Vec<char> = "αβγδεζηθικλμνξοπρστυφχψω".chars().collect();
    let representation = match name.0 {
        atom!("decimal") => digits('0'),
        atom!("decimal-leading-zero") => {
            if (0..10).contains(&value) {
                format!("0{value}")
            } else {
                digits('0')
            }
        },
        atom!("lower-alpha") | atom!("lower-latin") => alphabetic(&lower_latin)?,
        atom!("upper-alpha") | atom!("upper-latin") => alphabetic(&upper_latin)?,
        atom!("lower-greek") => alphabetic(&lower_greek)?,
        atom!("lower-roman") => roman(false)?,
        atom!("upper-roman") => roman(true)?,
        atom!("arabic-indic") => digits('\u{660}'),
        atom!("bengali") => digits('\u{9E6}'),
        atom!("cambodian") | atom!("khmer") => digits('\u{17E0}'),
        atom!("devanagari") => digits('\u{966}'),
        atom!("gujarati") => digits('\u{AE6}'),
        atom!("gurmukhi") => digits('\u{A66}'),
        atom!("kannada") => digits('\u{CE6}'),
        atom!("lao") => digits('\u{ED0}'),
        atom!("malayalam") => digits('\u{D66}'),
        atom!("mongolian") => digits('\u{1810}'),
        atom!("myanmar") => digits('\u{1040}'),
        atom!("oriya") => digits('\u{B66}'),
        atom!("persian") => digits('\u{6F0}'),
        atom!("tamil") => digits('\u{BE6}'),
        atom!("telugu") => digits('\u{C66}'),
        atom!("thai") => digits('\u{E50}'),
        atom!("tibetan") => digits('\u{F20}'),
        _ => return None,
    };
    Some(representation)
}

fn zero_counter_representation(counter_style: &CounterStyle) -> &str {
    match counter_style {
        CounterStyle::None | CounterStyle::String(_) => unreachable!("Invalid counter style"),
        CounterStyle::Name(name) => match name.0 {
            atom!("disc") => "\u{2022}",            /* "•" */
            atom!("circle") => "\u{25E6}",          /* "◦" */
            atom!("square") => "\u{25AA}",          /* "▪" */
            atom!("disclosure-open") => "\u{25BE}", /* "▾" */
            // TODO: Use U+25C2 "◂" depending on the direction.
            atom!("disclosure-closed") => "\u{25B8}", /* "▸" */
            atom!("decimal-leading-zero") => "00",
            atom!("arabic-indic") => "\u{660}", /* "٠" */
            atom!("bengali") => "\u{9E6}",      /* "০" */
            atom!("cambodian") | atom!("khmer") => "\u{17E0}", /* "០" */
            atom!("devanagari") => "\u{966}",   /* "०" */
            atom!("gujarati") => "\u{AE6}",     /* "૦" */
            atom!("gurmukhi") => "\u{A66}",     /* "੦" */
            atom!("kannada") => "\u{CE6}",      /* "೦" */
            atom!("lao") => "\u{ED0}",          /* "໐" */
            atom!("malayalam") => "\u{D66}",    /* "൦" */
            atom!("mongolian") => "\u{1810}",   /* "᠐" */
            atom!("myanmar") => "\u{1040}",     /* "၀" */
            atom!("oriya") => "\u{B66}",        /* "୦" */
            atom!("persian") => "\u{6F0}",      /* "۰" */
            atom!("tamil") => "\u{BE6}",        /* "௦" */
            atom!("telugu") => "\u{C66}",       /* "౦" */
            atom!("thai") => "\u{E50}",         /* "๐" */
            atom!("tibetan") => "\u{F20}",      /* "༠" */
            atom!("cjk-decimal") |
            atom!("cjk-earthly-branch") |
            atom!("cjk-heavenly-stem") |
            atom!("japanese-informal") => "\u{3007}", /* "〇" */
            atom!("korean-hangul-formal") => "\u{C601}", /* "영" */
            atom!("korean-hanja-informal") |
            atom!("korean-hanja-formal") |
            atom!("japanese-formal") |
            atom!("simp-chinese-informal") |
            atom!("simp-chinese-formal") |
            atom!("trad-chinese-informal") |
            atom!("trad-chinese-formal") |
            atom!("cjk-ideographic") => "\u{96F6}", /* "零" */
            // Fall back to decimal.
            _ => "0",
        },
        CounterStyle::Symbols { ty, symbols } => match ty {
            // For numeric, use the first symbol, which represents the value 0.
            SymbolsType::Numeric => {
                symbol_to_string(symbols.0.first().expect("symbols() should have symbols"))
            },
            // For cyclic, the first symbol represents the value 1. However, it loops back,
            // so the last symbol represents the value 0.
            SymbolsType::Cyclic => {
                symbol_to_string(symbols.0.last().expect("symbols() should have symbols"))
            },
            // For the others, the first symbol represents the value 1, and 0 is out of range.
            // Therefore, fall back to `decimal`.
            SymbolsType::Alphabetic | SymbolsType::Symbolic | SymbolsType::Fixed => "0",
        },
    }
}

/// <https://drafts.csswg.org/css-lists/#marker-string>
pub(crate) fn marker_string(list_style_type: &ListStyleType, ordinal: i32) -> Option<String> {
    let suffix = match &list_style_type.0 {
        CounterStyle::None => return None,
        CounterStyle::String(string) => return Some(string.to_string()),
        CounterStyle::Name(name) => match name.0 {
            atom!("disc") |
            atom!("circle") |
            atom!("square") |
            atom!("disclosure-open") |
            atom!("disclosure-closed") => " ",
            atom!("hiragana") |
            atom!("hiragana-iroha") |
            atom!("katakana") |
            atom!("katakana-iroha") |
            atom!("cjk-decimal") |
            atom!("cjk-earthly-branch") |
            atom!("cjk-heavenly-stem") |
            atom!("japanese-informal") |
            atom!("japanese-formal") |
            atom!("simp-chinese-informal") |
            atom!("simp-chinese-formal") |
            atom!("trad-chinese-informal") |
            atom!("trad-chinese-formal") |
            atom!("cjk-ideographic") => "\u{3001}", /* "、" */
            atom!("korean-hangul-formal") |
            atom!("korean-hanja-informal") |
            atom!("korean-hanja-formal") => ", ",
            atom!("ethiopic-numeric") => "/ ",
            _ => ". ",
        },
        CounterStyle::Symbols { .. } => " ",
    };
    Some(generate_counter_representation(&list_style_type.0, ordinal) + suffix)
}

/// The values of the instances of counter `name` in scope at the pseudo-element `node`
/// (outermost first), as `counters()` lists them; `counter()` takes the last.
/// <https://drafts.csswg.org/css-lists/#auto-numbering>: walks the document in tree order up to
/// `node`, applying each rendered element's and `::before`/`::after`'s `counter-reset` and
/// `counter-increment`. An instance lives on its creator's following siblings and their
/// descendants; a reset on a sibling of its creator replaces it. Without an instance in scope
/// the value is 0, as `counter()` instantiates one.
#[expect(unsafe_code)]
pub(crate) fn counter_values(
    context: &LayoutContext,
    node: ServoLayoutNode<'_>,
    name: &CustomIdent,
) -> Vec<i32> {
    let mut root = node;
    while let Some(parent) = unsafe { root.dangerous_flat_tree_parent() } {
        root = parent;
    }
    let mut walk = CounterWalk {
        context,
        name,
        target: node.opaque(),
        target_pseudo: node.pseudo_element_chain().primary,
        instances: Vec::new(),
    };
    for child in root.flat_tree_children() {
        if let Some(values) = walk.visit(child, 0) {
            return values;
        }
    }
    vec![0]
}

struct CounterWalk<'a> {
    context: &'a LayoutContext<'a>,
    name: &'a CustomIdent,
    target: OpaqueNode,
    target_pseudo: Option<PseudoElement>,
    /// The instances in scope, innermost last, with the tree depth of their creator.
    instances: Vec<(usize, i32)>,
}

impl CounterWalk<'_> {
    fn apply(&mut self, style: &ComputedValues, depth: usize) {
        let counters = style.get_counters();
        for pair in counters
            .counter_reset
            .iter()
            .filter(|pair| pair.name == *self.name)
        {
            match self.instances.last_mut() {
                Some((creator_depth, value)) if *creator_depth == depth => *value = pair.value,
                _ => self.instances.push((depth, pair.value)),
            }
        }
        for pair in counters
            .counter_increment
            .iter()
            .filter(|pair| pair.name == *self.name)
        {
            if self.instances.is_empty() {
                self.instances.push((depth, 0));
            }
            let (_, value) = self.instances.last_mut().expect("Pushed above");
            *value = value.wrapping_add(pair.value);
        }
    }

    fn values(&self) -> Vec<i32> {
        if self.instances.is_empty() {
            return vec![0];
        }
        self.instances.iter().map(|(_, value)| *value).collect()
    }

    fn pseudo_style(
        &self,
        node: ServoLayoutNode<'_>,
        pseudo: PseudoElement,
    ) -> Option<servo_arc::Arc<ComputedValues>> {
        let style = node
            .as_element()?
            .with_pseudo(pseudo)?
            .style(&self.context.style_context);
        (!style.ineffective_content_property()).then_some(style)
    }

    /// Returns the values at the target once reached.
    fn visit(&mut self, node: ServoLayoutNode<'_>, depth: usize) -> Option<Vec<i32>> {
        node.as_element()?;
        let style = node.style(&self.context.style_context);
        if style.get_box().display.is_none() {
            return None;
        }
        self.apply(&style, depth);
        let is_target = node.opaque() == self.target;
        if is_target && self.target_pseudo == Some(PseudoElement::Marker) {
            return Some(self.values());
        }
        if let Some(before) = self.pseudo_style(node, PseudoElement::Before) {
            self.apply(&before, depth + 1);
        }
        if is_target && self.target_pseudo == Some(PseudoElement::Before) {
            return Some(self.values());
        }
        for child in node.flat_tree_children() {
            if let Some(values) = self.visit(child, depth + 1) {
                return Some(values);
            }
        }
        if let Some(after) = self.pseudo_style(node, PseudoElement::After) {
            self.apply(&after, depth + 1);
        }
        if is_target {
            return Some(self.values());
        }
        // The instances its children and pseudo-elements created end with this element.
        self.instances
            .retain(|(creator_depth, _)| *creator_depth <= depth);
        None
    }
}

/// The quote depth at the start of the generated content of the pseudo-element `node`.
/// <https://drafts.csswg.org/css-content/#quote-values>: walks the document in tree order up to
/// `node`; each `open-quote` and `no-open-quote` of a rendered `::before`/`::after` increments the
/// depth, each `close-quote` and `no-close-quote` decrements it, never below zero.
#[expect(unsafe_code)]
pub(crate) fn quote_depth(context: &LayoutContext, node: ServoLayoutNode<'_>) -> usize {
    let mut root = node;
    while let Some(parent) = unsafe { root.dangerous_flat_tree_parent() } {
        root = parent;
    }
    let mut walk = QuoteWalk {
        context,
        target: node.opaque(),
        target_pseudo: node.pseudo_element_chain().primary,
        depth: 0,
    };
    for child in root.flat_tree_children() {
        if walk.visit(child) {
            break;
        }
    }
    walk.depth
}

/// Applies one quote item of `content` to the quote `depth`, returning the depth to use for the
/// quote mark it renders, if it renders one.
pub(crate) fn apply_quote_item<I>(item: &ContentItem<I>, depth: &mut usize) -> Option<usize> {
    match item {
        ContentItem::OpenQuote => {
            *depth += 1;
            Some(*depth - 1)
        },
        ContentItem::NoOpenQuote => {
            *depth += 1;
            None
        },
        // A close-quote that would make the depth negative renders nothing.
        ContentItem::CloseQuote => depth
            .checked_sub(1)
            .inspect(|new_depth| *depth = *new_depth),
        ContentItem::NoCloseQuote => {
            *depth = depth.saturating_sub(1);
            None
        },
        _ => None,
    }
}

struct QuoteWalk<'a> {
    context: &'a LayoutContext<'a>,
    target: OpaqueNode,
    target_pseudo: Option<PseudoElement>,
    depth: usize,
}

impl QuoteWalk<'_> {
    fn apply(&mut self, node: ServoLayoutNode<'_>, pseudo: PseudoElement) {
        let Some(style) = node
            .as_element()
            .and_then(|element| element.with_pseudo(pseudo))
            .map(|element| element.style(&self.context.style_context))
            .filter(|style| !style.ineffective_content_property())
        else {
            return;
        };
        if let Content::Items(items) = &style.get_counters().content {
            for item in items.items.iter() {
                apply_quote_item(item, &mut self.depth);
            }
        }
    }

    /// Returns true once the target is reached.
    fn visit(&mut self, node: ServoLayoutNode<'_>) -> bool {
        if node.as_element().is_none() {
            return false;
        }
        let style = node.style(&self.context.style_context);
        if style.get_box().display.is_none() {
            return false;
        }
        let is_target = node.opaque() == self.target;
        // ::marker and ::before precede the element's children. Quotes in ::marker content are
        // not counted, as computing the (lazy) ::marker style of every element is costly.
        if is_target && self.target_pseudo != Some(PseudoElement::After) {
            return true;
        }
        self.apply(node, PseudoElement::Before);
        for child in node.flat_tree_children() {
            if self.visit(child) {
                return true;
            }
        }
        if is_target {
            return true;
        }
        self.apply(node, PseudoElement::After);
        false
    }
}

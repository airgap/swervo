/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::borrow::Cow;
use std::char::{ToLowercase, ToUppercase};
use std::ops::Range;

use icu_properties::BidiClass;
use icu_segmenter::WordSegmenter;
use layout_api::{LayoutElement, LayoutNode, SharedSelection};
use style::computed_values::_webkit_text_security::T as WebKitTextSecurity;
use style::computed_values::direction::T as Direction;
use style::computed_values::white_space_collapse::T as WhiteSpaceCollapse;
use style::dom::{NodeInfo, OpaqueNode};
use style::properties::ComputedValues;
use style::selector_parser::PseudoElement;
use style::values::specified::text::TextTransformCase;
use unicode_bidi::Level;
use unicode_categories::UnicodeCategories;
use web_atoms::local_name;

use super::text_run::TextRun;
use super::{
    InlineBox, InlineBoxIdentifier, InlineBoxes, InlineFormattingContext, InlineItem,
    SharedInlineStyles, TextOrigin, TextOriginKind, TextOrigins,
};
use crate::cell::ArcRefCell;
use crate::context::LayoutContext;
use crate::dom::{LayoutBox, NodeExt};
use crate::dom_traversal::NodeAndStyleInfo;
use crate::flow::BlockLevelBox;
use crate::flow::float::FloatBox;
use crate::formatting_contexts::IndependentFormattingContext;
use crate::positioned::AbsolutelyPositionedBox;
use crate::style_ext::ComputedValuesExt;

#[derive(Default)]
pub(crate) struct InlineFormattingContextBuilder {
    /// A stack of [`SharedInlineStyles`] including one for the root, one for each inline box on the
    /// inline box stack, and importantly, one for every `display: contents` element that we are
    /// currently processing. Normally `display: contents` elements don't affect the structure of
    /// the [`InlineFormattingContext`], but the styles they provide do style their children.
    pub shared_inline_styles_stack: Vec<SharedInlineStyles>,

    /// The collection of text strings that make up this [`InlineFormattingContext`] under
    /// construction.
    pub text_segments: Vec<String>,

    /// The current offset in the final text string of this [`InlineFormattingContext`],
    /// used to properly set the text range of new [`InlineItem::TextRun`]s.
    current_text_offset: usize,

    /// The current character offset in the final text string of this [`InlineFormattingContext`],
    /// used to properly set the text range of new [`InlineItem::TextRun`]s. Note that this is
    /// different from the UTF-8 code point offset.
    current_character_offset: usize,

    /// If the [`InlineFormattingContext`] that we are building has a selection shared with its
    /// originating node in the DOM, this will not be `None`.
    pub shared_selection: Option<SharedSelection>,

    /// The DOM origin of the text pushed so far, which maps between character offsets in the
    /// laid out text and DOM positions for selections and carets.
    pub text_origins: TextOrigins,

    /// The text node whose text is being pushed in pieces (around a `::first-letter`) and the
    /// UTF-16 length of the pieces already pushed.
    text_node_progress: Option<(OpaqueNode, u32)>,

    /// Whether the last processed node ended with whitespace. This is used to
    /// implement rule 4 of <https://www.w3.org/TR/css-text-3/#collapse>:
    ///
    /// > Any collapsible space immediately following another collapsible space—even one
    /// > outside the boundary of the inline containing that space, provided both spaces are
    /// > within the same inline formatting context—is collapsed to have zero advance width.
    /// > (It is invisible, but retains its soft wrap opportunity, if any.)
    last_inline_box_ended_with_collapsible_white_space: bool,

    /// Whether or not the current state of the inline formatting context is on a word boundary
    /// for the purposes of `text-transform: capitalize`.
    on_word_boundary: bool,

    /// Whether or not this inline formatting context will contain floats.
    pub contains_floats: bool,

    /// The current list of [`InlineItem`]s in this [`InlineFormattingContext`] under
    /// construction. This is stored in a flat list to make it easy to access the last
    /// item.
    pub inline_items: Vec<InlineItem>,

    /// The current [`InlineBox`] tree of this [`InlineFormattingContext`] under construction.
    pub inline_boxes: InlineBoxes,

    /// The ongoing stack of inline boxes stack of the builder.
    ///
    /// Contains all the currently ongoing inline boxes we entered so far.
    /// The traversal is at all times as deep in the tree as this stack is,
    /// which is why the code doesn't need to keep track of the actual
    /// container root (see `handle_inline_level_element`).
    ///
    /// When an inline box ends, it's removed from this stack.
    inline_box_stack: Vec<InlineBoxIdentifier>,

    /// Whether this [`InlineFormattingContextBuilder`] is empty for the purposes of ignoring
    /// during box tree construction. An IFC is empty if it only contains TextRuns with
    /// completely collapsible whitespace. When that happens it can be ignored completely.
    pub is_empty: bool,

    /// Whether or not the `::first-letter` pseudo-element of this inline formatting context
    /// has been processed yet.
    has_processed_first_letter: bool,

    /// Whether or not the inline formatting context under construction has any kind of
    /// right-to-left content such as a character with an RTL character class or a `dir`
    /// attribute specifying right-to-left content.
    pub(crate) has_right_to_left_content: bool,
}

impl InlineFormattingContextBuilder {
    /// <https://drafts.csswg.org/css-text/#white-space>:
    /// > Except where specified otherwise, white space processing in CSS affects only the document
    /// > white space characters: spaces (U+0020), tabs (U+0009), and segment breaks.
    ///
    /// From <https://github.com/w3c/csswg-drafts/issues/5147#issuecomment-637816669>:
    /// > HTML clearly treats CR, LF, and CRLF as segment breaks.
    ///
    /// Other browsers also consider the form feed character (0x0c) to be document white space, it
    /// seems.
    ///
    /// Taken all together, this is equivalent to the WhatWG Infra Standard's definition of ASCII
    /// white space.
    pub(crate) fn is_document_white_space(character: char) -> bool {
        character.is_ascii_whitespace()
    }

    pub(crate) fn new(info: &NodeAndStyleInfo, context: &LayoutContext) -> Self {
        let has_right_to_left_content = info.style.get_inherited_box().direction == Direction::Rtl;
        Self {
            // For the purposes of `text-transform: capitalize` the start of the IFC is a word boundary.
            on_word_boundary: true,
            is_empty: true,
            shared_inline_styles_stack: vec![SharedInlineStyles::from_info_and_context(
                info, context,
            )],
            shared_selection: info.node.selection(),
            has_right_to_left_content,
            ..Default::default()
        }
    }

    pub(crate) fn currently_processing_inline_box(&self) -> bool {
        !self.inline_box_stack.is_empty()
    }

    fn push_control_character_string(&mut self, string_to_push: &str) {
        self.text_segments.push(string_to_push.to_owned());
        self.current_text_offset += string_to_push.len();
        self.current_character_offset += string_to_push.chars().count();
    }

    fn shared_inline_styles(&self) -> SharedInlineStyles {
        self.shared_inline_styles_stack
            .last()
            .expect("Should always have at least one SharedInlineStyles")
            .clone()
    }

    pub(crate) fn push_atomic(
        &mut self,
        independent_formatting_context_creator: impl FnOnce()
            -> ArcRefCell<IndependentFormattingContext>,
        old_layout_box: Option<LayoutBox>,
    ) -> InlineItem {
        // If there is an existing undamaged layout box that's compatible, use that.
        let independent_formatting_context = old_layout_box
            .and_then(|layout_box| match layout_box {
                LayoutBox::InlineLevel(InlineItem::Atomic(atomic, ..)) => Some(atomic),
                _ => None,
            })
            .unwrap_or_else(independent_formatting_context_creator);

        let inline_level_box = InlineItem::Atomic(
            independent_formatting_context,
            self.current_text_offset,
            Level::ltr(), /* This will be assigned later if necessary. */
        );
        self.inline_items.push(inline_level_box.clone());
        self.is_empty = false;

        // Push an object replacement character for this atomic, which will ensure that the line breaker
        // inserts a line breaking opportunity here.
        self.push_control_character_string("\u{fffc}");

        self.last_inline_box_ended_with_collapsible_white_space = false;
        self.on_word_boundary = true;

        // Atomics such as images should prevent any following text as being interpreted as the first letter.
        self.has_processed_first_letter = true;

        inline_level_box
    }

    pub(crate) fn push_absolutely_positioned_box(
        &mut self,
        absolutely_positioned_box_creator: impl FnOnce() -> ArcRefCell<AbsolutelyPositionedBox>,
        old_layout_box: Option<LayoutBox>,
    ) -> InlineItem {
        let absolutely_positioned_box = old_layout_box
            .and_then(|layout_box| match layout_box {
                LayoutBox::InlineLevel(InlineItem::OutOfFlowAbsolutelyPositionedBox(
                    positioned_box,
                    ..,
                )) => Some(positioned_box),
                _ => None,
            })
            .unwrap_or_else(absolutely_positioned_box_creator);

        // We cannot just reuse the old inline item, because the `current_text_offset` may have changed.
        let inline_level_box = InlineItem::OutOfFlowAbsolutelyPositionedBox(
            absolutely_positioned_box,
            self.current_text_offset,
        );

        self.inline_items.push(inline_level_box.clone());
        self.is_empty = false;
        inline_level_box
    }

    pub(crate) fn push_float_box(
        &mut self,
        float_box_creator: impl FnOnce() -> ArcRefCell<FloatBox>,
        old_layout_box: Option<LayoutBox>,
    ) -> InlineItem {
        let inline_level_box = old_layout_box
            .and_then(|layout_box| match layout_box {
                LayoutBox::InlineLevel(inline_item) => Some(inline_item),
                _ => None,
            })
            .unwrap_or_else(|| InlineItem::OutOfFlowFloatBox(float_box_creator()));

        debug_assert!(
            matches!(inline_level_box, InlineItem::OutOfFlowFloatBox(..),),
            "Created float box with incompatible `old_layout_box`"
        );

        self.inline_items.push(inline_level_box.clone());
        self.is_empty = false;
        self.contains_floats = true;
        inline_level_box
    }

    pub(crate) fn push_block_level_box(&mut self, block_level: ArcRefCell<BlockLevelBox>) {
        assert!(self.currently_processing_inline_box());
        self.contains_floats = self.contains_floats || block_level.borrow().contains_floats();
        self.inline_items.push(InlineItem::BlockLevel(block_level));
    }

    pub(crate) fn start_inline_box(
        &mut self,
        inline_box_creator: impl FnOnce() -> ArcRefCell<InlineBox>,
        old_layout_box: Option<LayoutBox>,
    ) -> InlineItem {
        // If there is an existing undamaged layout box that's compatible, use the `InlineBox` within it.
        let inline_box = old_layout_box
            .and_then(|layout_box| match layout_box {
                LayoutBox::InlineLevel(InlineItem::StartInlineBox(inline_box)) => Some(inline_box),
                _ => None,
            })
            .unwrap_or_else(inline_box_creator);

        let borrowed_inline_box = inline_box.borrow();

        let style = &borrowed_inline_box.base.style;
        self.push_control_character_string(style.bidi_control_chars().0);
        self.has_right_to_left_content =
            self.has_right_to_left_content || style.get_inherited_box().direction == Direction::Rtl;

        self.shared_inline_styles_stack
            .push(borrowed_inline_box.shared_inline_styles.clone());
        std::mem::drop(borrowed_inline_box);

        let identifier = self.inline_boxes.start_inline_box(inline_box.clone());
        let inline_item = InlineItem::StartInlineBox(inline_box);
        self.inline_items.push(inline_item.clone());
        self.inline_box_stack.push(identifier);
        self.is_empty = false;
        inline_item
    }

    /// End the ongoing inline box in this [`InlineFormattingContextBuilder`], returning
    /// shared references to all of the box tree items that were created for it. More than
    /// a single box tree items may be produced for a single inline box when that inline
    /// box is split around a block-level element.
    pub(crate) fn end_inline_box(&mut self) {
        self.shared_inline_styles_stack.pop();
        self.inline_items.push(InlineItem::EndInlineBox);
        let identifier = self
            .inline_box_stack
            .pop()
            .expect("Ended non-existent inline box");
        self.inline_boxes.end_inline_box(identifier);
        let inline_level_box = self.inline_boxes.get(&identifier);
        let bidi_control_chars = inline_level_box.borrow().base.style.bidi_control_chars();
        self.push_control_character_string(bidi_control_chars.1);
    }

    /// This is like [`Self::push_text`], except that it might possibly add an anonymous box if
    ///
    ///  - This inline formatting context has a `::first-letter` style.
    ///  - No anonymous box for `::first-letter` has been added yet.
    ///  - First letter content is detected in this text.
    ///
    /// Note that this should only be used when processing text in block containers.
    pub(crate) fn push_text_with_possible_first_letter<'dom>(
        &mut self,
        text: Cow<'dom, str>,
        info: &NodeAndStyleInfo<'dom>,
        container_info: &NodeAndStyleInfo<'dom>,
        layout_context: &LayoutContext,
    ) -> bool {
        if self.has_processed_first_letter || !container_info.pseudo_element_chain().is_empty() {
            self.push_text(text, info);
            return false;
        }

        let Some(first_letter_info) =
            container_info.with_pseudo_element(layout_context, PseudoElement::FirstLetter)
        else {
            self.push_text(text, info);
            return false;
        };

        let first_letter_range = first_letter_range(&text[..]);
        if first_letter_range.is_empty() {
            return false;
        }

        // Push any leading white space first.
        if first_letter_range.start != 0 {
            self.push_text(Cow::Borrowed(&text[0..first_letter_range.start]), info);
        }

        // Push the first-letter text into an anonymous box with the `::first-letter` style.
        let box_slot = first_letter_info.node.box_slot();
        let inline_item = self.start_inline_box(
            || ArcRefCell::new(InlineBox::new(&first_letter_info, layout_context)),
            None,
        );
        box_slot.set(LayoutBox::InlineLevel(inline_item));

        let first_letter_text = Cow::Borrowed(&text[first_letter_range.clone()]);
        // The first letter is pushed for its pseudo-element, but the text after it still
        // continues at a later DOM offset of the text node.
        let node = info.node.opaque();
        let pushed_length = match self.text_node_progress {
            Some((progress_node, length)) if progress_node == node => length,
            _ => 0,
        };
        self.text_node_progress = Some((
            node,
            pushed_length + first_letter_text.encode_utf16().count() as u32,
        ));
        self.push_text(first_letter_text, &first_letter_info);
        self.end_inline_box();
        self.has_processed_first_letter = true;

        // Now push the non-first-letter text.
        self.push_text(Cow::Borrowed(&text[first_letter_range.end..]), info);

        true
    }

    pub(crate) fn push_text<'dom>(&mut self, text: Cow<'dom, str>, info: &NodeAndStyleInfo<'dom>) {
        let trim_beginning_white_space = self.last_inline_box_ended_with_collapsible_white_space;
        let starts_on_word_boundary = self.on_word_boundary;
        let char_iterator = rendered_characters(
            &text,
            &info.style,
            trim_beginning_white_space,
            starts_on_word_boundary,
        );

        let bidi_class_map = icu_properties::maps::bidi_class();
        let white_space_collapse = info.style.clone_white_space_collapse();
        let mut character_count = 0;
        let new_text: String = char_iterator
            .inspect(|&character| {
                character_count += 1;

                // If this character has a strong right-to-left class the new inline formatting context will
                // need to be BiDi-aware. This match is derived from the list of strong right-to-left classes
                // at https://www.unicode.org/reports/tr44/#Bidi_Class_Values.
                self.has_right_to_left_content = self.has_right_to_left_content ||
                    matches!(
                        bidi_class_map.get(character),
                        BidiClass::RightToLeft |
                            BidiClass::ArabicLetter |
                            BidiClass::RightToLeftEmbedding |
                            BidiClass::RightToLeftIsolate |
                            BidiClass::RightToLeftOverride
                    );

                self.is_empty = self.is_empty &&
                    match white_space_collapse {
                        WhiteSpaceCollapse::Collapse => Self::is_document_white_space(character),
                        WhiteSpaceCollapse::PreserveBreaks => {
                            Self::is_document_white_space(character) && character != '\n'
                        },
                        WhiteSpaceCollapse::Preserve | WhiteSpaceCollapse::BreakSpaces => false,
                    };
            })
            .collect();

        if new_text.is_empty() {
            return;
        }

        if let Some(last_character) = new_text.chars().next_back() {
            self.on_word_boundary = last_character.is_whitespace();
            self.last_inline_box_ended_with_collapsible_white_space =
                self.on_word_boundary && white_space_collapse != WhiteSpaceCollapse::Preserve;
        }

        let new_range = self.current_text_offset..self.current_text_offset + new_text.len();
        self.current_text_offset = new_range.end;

        let new_character_range =
            self.current_character_offset..self.current_character_offset + character_count;
        self.current_character_offset = new_character_range.end;

        if let Some((node, dom_offsets, kind)) =
            self.origin_of_text(&text, &new_text, info, trim_beginning_white_space)
        {
            debug_assert_eq!(dom_offsets.len(), character_count + 1);
            self.text_origins.sources.push(TextOrigin {
                node,
                kind,
                character_start: new_character_range.start,
                dom_offsets,
            });
        }

        self.text_segments.push(new_text);

        let current_inline_styles = self.shared_inline_styles();

        if let Some(InlineItem::TextRun(text_run)) = self.inline_items.last() &&
            text_run
                .borrow()
                .inline_styles
                .ptr_eq(&current_inline_styles)
        {
            let box_slot = info.node.box_slot();
            let old_text_run = box_slot.take_layout_box_as_text_run();

            {
                let mut text_run = text_run.borrow_mut();
                text_run.text_range.end = new_range.end;
                text_run.character_range.end = new_character_range.end;

                // If this text node does not have a `TextRun` in the box slot, this means that
                // it is either new or dirty, which means that the entire `TextRun` just extended
                // is dirty as well. In this case, never reuse existing shaping results. Clear
                // all old items to ensure this.
                if old_text_run.is_none() {
                    text_run.items.clear();
                }
            }

            box_slot.set(LayoutBox::Text(text_run.clone()));
            return;
        }

        let box_slot = info.node.is_text_node().then(|| info.node.box_slot());
        let text_run = ArcRefCell::new(TextRun::new(
            info.into(),
            current_inline_styles,
            new_range,
            new_character_range,
            box_slot
                .as_ref()
                .and_then(|box_slot| box_slot.take_layout_box_as_text_run()),
        ));
        self.inline_items
            .push(InlineItem::TextRun(text_run.clone()));

        if let Some(box_slot) = box_slot {
            box_slot.set(LayoutBox::Text(text_run));
        }
    }

    /// For text of a DOM text node, or the line feed generated for a `<br>`, returns the node,
    /// the UTF-16 offset in it of each of the `rendered` characters laid out for `text` followed
    /// by the offset just past the last one, and which kind of node it is.
    fn origin_of_text(
        &mut self,
        text: &str,
        rendered: &str,
        info: &NodeAndStyleInfo,
        trim_beginning_white_space: bool,
    ) -> Option<(OpaqueNode, Vec<u32>, TextOriginKind)> {
        let node = info.node.opaque();
        if !info.node.is_text_node() {
            let is_line_break = info
                .node
                .as_element()
                .is_some_and(|element| element.local_name() == &local_name!("br"));
            if !is_line_break {
                return None;
            }
            self.note_editability(info);
            // Before the `<br>` is offset 0 in it, after it is offset 1.
            let mut dom_offsets = vec![0; rendered.chars().count()];
            dom_offsets.push(1);
            return Some((node, dom_offsets, TextOriginKind::LineBreak));
        }

        let piece_start = match self.text_node_progress {
            Some((progress_node, length)) if progress_node == node => length,
            _ => 0,
        };
        let piece_length = text.encode_utf16().count() as u32;
        self.text_node_progress = Some((node, piece_start + piece_length));
        self.note_editability(info);

        let collapsed: Vec<char> = WhitespaceCollapse::new(
            text.chars(),
            info.style.clone_white_space_collapse(),
            trim_beginning_white_space,
        )
        .collect();
        // Collapsing only drops characters and turns white space into spaces, so each remaining
        // character is the first DOM character from where the previous one was taken that it
        // can have come from.
        let mut collapsed_offsets = Vec::with_capacity(collapsed.len());
        let mut end = piece_start;
        let mut offset = piece_start;
        for character in text.chars() {
            if let Some(&wanted) = collapsed.get(collapsed_offsets.len()) &&
                (wanted == character ||
                    (wanted == ' ' && Self::is_document_white_space(character)))
            {
                collapsed_offsets.push(offset);
                end = offset + character.len_utf16() as u32;
            }
            offset += character.len_utf16() as u32;
        }
        if collapsed_offsets.is_empty() {
            return None;
        }

        // `text-transform` can turn one character into several (ß into SS), which all get the
        // offset of the one they came from. Other transformations replace characters one by one.
        let rendered: Vec<char> = rendered.chars().collect();
        let mut dom_offsets = Vec::with_capacity(rendered.len() + 1);
        for (&character, &offset) in collapsed.iter().zip(&collapsed_offsets) {
            let rest = &rendered[dom_offsets.len()..];
            let produced = |expansion: &mut dyn Iterator<Item = char>| {
                let mut count = 0;
                for expected in expansion {
                    if rest.get(count) != Some(&expected) {
                        return None;
                    }
                    count += 1;
                }
                Some(count)
            };
            let count = if rest.first() == Some(&character) {
                1
            } else {
                produced(&mut character.to_uppercase())
                    .or_else(|| produced(&mut character.to_lowercase()))
                    .unwrap_or(1)
            };
            let count = count.min(rest.len());
            dom_offsets.extend(std::iter::repeat_n(offset, count));
        }
        dom_offsets.push(end);
        Some((node, dom_offsets, TextOriginKind::Text))
    }

    fn note_editability(&mut self, info: &NodeAndStyleInfo) {
        if !self.text_origins.is_editable && info.node.is_editable() {
            self.text_origins.is_editable = true;
        }
    }

    /// Makes this empty inline formatting context hold a line for the caret of the editing host
    /// it belongs to, as other browsers lay out an empty editing host with one line.
    pub(crate) fn hold_line_for_empty_editing_host(&mut self, info: &NodeAndStyleInfo) {
        debug_assert!(self.is_empty);
        self.is_empty = false;
        self.text_origins.is_editable = true;
        self.text_origins.sources.push(TextOrigin {
            node: info.node.opaque(),
            kind: TextOriginKind::EmptyEditingHost,
            character_start: self.current_character_offset,
            dom_offsets: vec![0],
        });
    }

    pub(crate) fn enter_display_contents(&mut self, shared_inline_styles: SharedInlineStyles) {
        self.shared_inline_styles_stack.push(shared_inline_styles);
    }

    pub(crate) fn leave_display_contents(&mut self) {
        self.shared_inline_styles_stack.pop();
    }

    /// Finish the current inline formatting context, returning [`None`] if the context was empty.
    pub(crate) fn finish(
        self,
        layout_context: &LayoutContext,
        has_first_formatted_line: bool,
        is_single_line_text_input: bool,
        default_bidi_level: Level,
    ) -> Option<InlineFormattingContext> {
        if self.is_empty {
            return None;
        }

        assert!(self.inline_box_stack.is_empty());
        Some(InlineFormattingContext::new_with_builder(
            self,
            layout_context,
            has_first_formatted_line,
            is_single_line_text_input,
            default_bidi_level,
        ))
    }
}

fn preserve_segment_break() -> bool {
    true
}

/// The characters that layout shapes and renders for `text`, after white space collapsing,
/// `text-transform` and `-webkit-text-security`. `trim_beginning_white_space` and
/// `on_word_boundary` describe the inline formatting context just before `text`.
fn rendered_characters<'text>(
    text: &'text str,
    style: &ComputedValues,
    trim_beginning_white_space: bool,
    on_word_boundary: bool,
) -> Box<dyn Iterator<Item = char> + 'text> {
    let collapsed = WhitespaceCollapse::new(
        text.chars(),
        style.clone_white_space_collapse(),
        trim_beginning_white_space,
    );

    // TODO: Not all text transforms are about case, this logic should stop ignoring
    // TextTransform::FULL_WIDTH and TextTransform::FULL_SIZE_KANA.
    let text_transform = style.clone_text_transform().case();
    let char_iterator: Box<dyn Iterator<Item = char> + 'text> = match text_transform {
        TextTransformCase::None => Box::new(collapsed),
        TextTransformCase::Capitalize => {
            // `TextTransformation` doesn't support capitalization, so we must capitalize the whole
            // string at once and make a copy. Here `on_word_boundary` indicates whether or not the
            // inline formatting context as a whole is on a word boundary. This is different from
            // `trim_beginning_white_space` because the word boundaries are between atomic inlines
            // and at the start of the IFC, and because preserved spaces are a word boundary.
            let collapsed_string: String = collapsed.collect();
            Box::new(
                capitalize_string(&collapsed_string, on_word_boundary)
                    .chars()
                    .collect::<Vec<_>>()
                    .into_iter(),
            )
        },
        _ => {
            // If `text-transform` is active, wrap the `WhitespaceCollapse` iterator in
            // a `TextTransformation` iterator.
            Box::new(TextTransformation::new(collapsed, text_transform))
        },
    };

    if style.clone__webkit_text_security() != WebKitTextSecurity::None {
        Box::new(TextSecurityTransform::new(
            char_iterator,
            style.clone__webkit_text_security(),
        ))
    } else {
        char_iterator
    }
}

pub struct WhitespaceCollapse<InputIterator> {
    char_iterator: InputIterator,
    white_space_collapse: WhiteSpaceCollapse,

    /// Whether or not we should collapse white space completely at the start of the string.
    /// This is true when the last character handled in our owning [`super::InlineFormattingContext`]
    /// was collapsible white space.
    remove_collapsible_white_space_at_start: bool,

    /// Whether or not the last character produced was newline. There is special behavior
    /// we do after each newline.
    following_newline: bool,

    /// Whether or not we have seen any non-white space characters, indicating that we are not
    /// in a collapsible white space section at the beginning of the string.
    have_seen_non_white_space_characters: bool,

    /// Whether the last character that we processed was a non-newline white space character. When
    /// collapsing white space we need to wait until the next non-white space character or the end
    /// of the string to push a single white space.
    inside_white_space: bool,

    /// When we enter a collapsible white space region, we may need to wait to produce a single
    /// white space character as soon as we encounter a non-white space character. When that
    /// happens we queue up the non-white space character for the next iterator call.
    character_pending_to_return: Option<char>,
}

impl<InputIterator> WhitespaceCollapse<InputIterator> {
    pub fn new(
        char_iterator: InputIterator,
        white_space_collapse: WhiteSpaceCollapse,
        trim_beginning_white_space: bool,
    ) -> Self {
        Self {
            char_iterator,
            white_space_collapse,
            remove_collapsible_white_space_at_start: trim_beginning_white_space,
            inside_white_space: false,
            following_newline: false,
            have_seen_non_white_space_characters: false,
            character_pending_to_return: None,
        }
    }

    fn is_leading_trimmed_white_space(&self) -> bool {
        !self.have_seen_non_white_space_characters && self.remove_collapsible_white_space_at_start
    }

    /// Whether or not we need to produce a space character if the next character is not a newline
    /// and not white space. This happens when we are exiting a section of white space and we
    /// waited to produce a single space character for the entire section of white space (but
    /// not following or preceding a newline).
    fn need_to_produce_space_character_after_white_space(&self) -> bool {
        self.inside_white_space && !self.following_newline && !self.is_leading_trimmed_white_space()
    }
}

impl<InputIterator> Iterator for WhitespaceCollapse<InputIterator>
where
    InputIterator: Iterator<Item = char>,
{
    type Item = char;

    fn next(&mut self) -> Option<Self::Item> {
        // Point 4.1.1 first bullet:
        // > If white-space is set to normal, nowrap, or pre-line, whitespace
        // > characters are considered collapsible
        // If whitespace is not considered collapsible, it is preserved entirely, which
        // means that we can simply return the input string exactly.
        if self.white_space_collapse == WhiteSpaceCollapse::Preserve ||
            self.white_space_collapse == WhiteSpaceCollapse::BreakSpaces
        {
            // From <https://drafts.csswg.org/css-text-3/#white-space-processing>:
            // > Carriage returns (U+000D) are treated identically to spaces (U+0020) in all respects.
            //
            // In the non-preserved case these are converted to space below.
            return match self.char_iterator.next() {
                Some('\r') => Some(' '),
                next => next,
            };
        }

        if let Some(character) = self.character_pending_to_return.take() {
            self.inside_white_space = false;
            self.have_seen_non_white_space_characters = true;
            self.following_newline = false;
            return Some(character);
        }

        while let Some(character) = self.char_iterator.next() {
            // Don't push non-newline whitespace immediately. Instead wait to push it until we
            // know that it isn't followed by a newline. See `push_pending_whitespace_if_needed`
            // above.
            if InlineFormattingContextBuilder::is_document_white_space(character) &&
                character != '\n'
            {
                self.inside_white_space = true;
                continue;
            }

            // Point 4.1.1:
            // > 2. Collapsible segment breaks are transformed for rendering according to the
            // >    segment break transformation rules.
            if character == '\n' {
                // From <https://drafts.csswg.org/css-text-3/#line-break-transform>
                // (4.1.3 -- the segment break transformation rules):
                //
                // > When white-space is pre, pre-wrap, or pre-line, segment breaks are not
                // > collapsible and are instead transformed into a preserved line feed"
                if self.white_space_collapse != WhiteSpaceCollapse::Collapse {
                    self.inside_white_space = false;
                    self.following_newline = true;
                    return Some(character);

                // Point 4.1.3:
                // > 1. First, any collapsible segment break immediately following another
                // >    collapsible segment break is removed.
                // > 2. Then any remaining segment break is either transformed into a space (U+0020)
                // >    or removed depending on the context before and after the break.
                } else if !self.following_newline &&
                    preserve_segment_break() &&
                    !self.is_leading_trimmed_white_space()
                {
                    self.inside_white_space = false;
                    self.following_newline = true;
                    return Some(' ');
                } else {
                    self.following_newline = true;
                    continue;
                }
            }

            // Point 4.1.1:
            // > 2. Any sequence of collapsible spaces and tabs immediately preceding or
            // >    following a segment break is removed.
            // > 3. Every collapsible tab is converted to a collapsible space (U+0020).
            // > 4. Any collapsible space immediately following another collapsible space—even
            // >    one outside the boundary of the inline containing that space, provided both
            // >    spaces are within the same inline formatting context—is collapsed to have zero
            // >    advance width.
            if self.need_to_produce_space_character_after_white_space() {
                self.inside_white_space = false;
                self.character_pending_to_return = Some(character);
                return Some(' ');
            }

            self.inside_white_space = false;
            self.have_seen_non_white_space_characters = true;
            self.following_newline = false;
            return Some(character);
        }

        if self.need_to_produce_space_character_after_white_space() {
            self.inside_white_space = false;
            return Some(' ');
        }

        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.char_iterator.size_hint()
    }

    fn count(self) -> usize
    where
        Self: Sized,
    {
        self.char_iterator.count()
    }
}

enum PendingCaseConversionResult {
    Uppercase(ToUppercase),
    Lowercase(ToLowercase),
}

impl PendingCaseConversionResult {
    fn next(&mut self) -> Option<char> {
        match self {
            PendingCaseConversionResult::Uppercase(to_uppercase) => to_uppercase.next(),
            PendingCaseConversionResult::Lowercase(to_lowercase) => to_lowercase.next(),
        }
    }
}

/// This is an iterator that consumes a char iterator and produces character transformed
/// by the given CSS `text-transform` value. It currently does not support
/// `text-transform: capitalize` because Unicode segmentation libraries do not support
/// streaming input one character at a time.
pub struct TextTransformation<InputIterator> {
    /// The input character iterator.
    char_iterator: InputIterator,
    /// The `text-transform` value to use.
    text_transform: TextTransformCase,
    /// If an uppercasing or lowercasing produces more than one character, this
    /// caches them so that they can be returned in subsequent iterator calls.
    pending_case_conversion_result: Option<PendingCaseConversionResult>,
}

impl<InputIterator> TextTransformation<InputIterator> {
    pub fn new(char_iterator: InputIterator, text_transform: TextTransformCase) -> Self {
        Self {
            char_iterator,
            text_transform,
            pending_case_conversion_result: None,
        }
    }
}

impl<InputIterator> Iterator for TextTransformation<InputIterator>
where
    InputIterator: Iterator<Item = char>,
{
    type Item = char;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(character) = self
            .pending_case_conversion_result
            .as_mut()
            .and_then(|result| result.next())
        {
            return Some(character);
        }
        self.pending_case_conversion_result = None;

        for character in self.char_iterator.by_ref() {
            match self.text_transform {
                TextTransformCase::None => return Some(character),
                TextTransformCase::Uppercase => {
                    let mut pending_result =
                        PendingCaseConversionResult::Uppercase(character.to_uppercase());
                    if let Some(character) = pending_result.next() {
                        self.pending_case_conversion_result = Some(pending_result);
                        return Some(character);
                    }
                },
                TextTransformCase::Lowercase => {
                    let mut pending_result =
                        PendingCaseConversionResult::Lowercase(character.to_lowercase());
                    if let Some(character) = pending_result.next() {
                        self.pending_case_conversion_result = Some(pending_result);
                        return Some(character);
                    }
                },
                // `text-transform: capitalize` currently cannot work on a per-character basis,
                // so must be handled outside of this iterator.
                TextTransformCase::Capitalize => return Some(character),
            }
        }
        None
    }
}

pub struct TextSecurityTransform<InputIterator> {
    /// The input character iterator.
    char_iterator: InputIterator,
    /// The `-webkit-text-security` value to use.
    text_security: WebKitTextSecurity,
}

impl<InputIterator> TextSecurityTransform<InputIterator> {
    pub fn new(char_iterator: InputIterator, text_security: WebKitTextSecurity) -> Self {
        Self {
            char_iterator,
            text_security,
        }
    }
}

impl<InputIterator> Iterator for TextSecurityTransform<InputIterator>
where
    InputIterator: Iterator<Item = char>,
{
    type Item = char;

    fn next(&mut self) -> Option<Self::Item> {
        // The behavior of `-webkit-text-security` isn't specified, so we have some
        // flexibility in the implementation. We just need to maintain a rough
        // compatability with other browsers.
        Some(match self.char_iterator.next()? {
            // This is not ideal, but zero width space is used for some special reasons in
            // `<input>` fields, so these remain untransformed, otherwise they would show up
            // in empty text fields.
            '\u{200B}' => '\u{200B}',
            // Newlines are preserved, so that `<br>` keeps working as expected.
            '\n' => '\n',
            character => match self.text_security {
                WebKitTextSecurity::None => character,
                WebKitTextSecurity::Circle => '○',
                WebKitTextSecurity::Disc => '●',
                WebKitTextSecurity::Square => '■',
            },
        })
    }
}

/// Given a string and whether the start of the string represents a word boundary, create a copy of
/// the string with letters after word boundaries capitalized.
pub(crate) fn capitalize_string(string: &str, allow_word_at_start: bool) -> String {
    let mut output_string = String::new();
    output_string.reserve(string.len());

    let word_segmenter = WordSegmenter::new_auto();
    let mut bounds = word_segmenter.segment_str(string).peekable();
    let mut byte_index = 0;
    for character in string.chars() {
        let current_byte_index = byte_index;
        byte_index += character.len_utf8();

        if let Some(next_index) = bounds.peek() &&
            *next_index == current_byte_index
        {
            bounds.next();

            if current_byte_index != 0 || allow_word_at_start {
                output_string.extend(character.to_uppercase());
                continue;
            }
        }

        output_string.push(character);
    }

    output_string
}

/// Computes the range of the first letter.
///
/// The range includes any preceding punctuation and white space, and any trailing punctuation. Any
/// non-punctuation following the letter/number/symbol of first-letter ends the range. Intervening
/// spaces within trailing punctuation are not supported yet.
///
/// If the resulting range is empty, no compatible first-letter text was found.
///
/// <https://drafts.csswg.org/css-pseudo/#first-letter-pattern>
fn first_letter_range(text: &str) -> Range<usize> {
    enum State {
        /// All characters that precede the `PrecedingWhitespaceAndPunctuation` state.
        Start,
        /// All preceding punctuation and intervening whitepace that precedes the `Lns` state.
        PrecedingPunctuation,
        /// Unicode general category L: letter, N: number and S: symbol
        Lns,
        /// All punctuation (but no whitespace or other characters), that
        /// come after the `Lns` state.
        TrailingPunctuation,
    }

    let mut start = 0;
    let mut state = State::Start;
    for (index, character) in text.char_indices() {
        match &mut state {
            State::Start => {
                if character.is_letter() || character.is_number() || character.is_symbol() {
                    start = index;
                    state = State::Lns;
                } else if character.is_punctuation() {
                    start = index;
                    state = State::PrecedingPunctuation
                }
            },
            State::PrecedingPunctuation => {
                if character.is_letter() || character.is_number() || character.is_symbol() {
                    state = State::Lns;
                } else if !character.is_separator_space() && !character.is_punctuation() {
                    return 0..0;
                }
            },
            State::Lns => {
                // TODO: Implement support for intervening spaces
                // <https://drafts.csswg.org/css-pseudo/#first-letter-pattern>
                if character.is_punctuation() &&
                    !character.is_punctuation_open() &&
                    !character.is_punctuation_dash()
                {
                    state = State::TrailingPunctuation;
                } else {
                    return start..index;
                }
            },
            State::TrailingPunctuation => {
                // TODO: Implement support for intervening spaces
                // <https://drafts.csswg.org/css-pseudo/#first-letter-pattern>
                if character.is_punctuation() &&
                    !character.is_punctuation_open() &&
                    !character.is_punctuation_dash()
                {
                    continue;
                } else {
                    return start..index;
                }
            },
        }
    }

    match state {
        State::Start | State::PrecedingPunctuation => 0..0,
        State::Lns | State::TrailingPunctuation => start..text.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_first_letter_eq(text: &str, expected: &str) {
        let range = first_letter_range(text);
        assert_eq!(&text[range], expected);
    }

    #[test]
    fn test_first_letter_range() {
        // All spaces
        assert_first_letter_eq("", "");
        assert_first_letter_eq("  ", "");

        // Spaces and punctuation only
        assert_first_letter_eq("(", "");
        assert_first_letter_eq(" (", "");
        assert_first_letter_eq("( ", "");
        assert_first_letter_eq("()", "");

        // Invalid chars
        assert_first_letter_eq("\u{0903}", "");

        // First letter only
        assert_first_letter_eq("A", "A");
        assert_first_letter_eq(" A", "A");
        assert_first_letter_eq("A ", "A");
        assert_first_letter_eq(" A ", "A");

        // Word
        assert_first_letter_eq("App", "A");
        assert_first_letter_eq(" App", "A");
        assert_first_letter_eq("App ", "A");

        // Preceding punctuation(s), intervening spaces and first letter
        assert_first_letter_eq(r#""A"#, r#""A"#);
        assert_first_letter_eq(r#" "A"#, r#""A"#);
        assert_first_letter_eq(r#""A "#, r#""A"#);
        assert_first_letter_eq(r#"" A"#, r#"" A"#);
        assert_first_letter_eq(r#" "A "#, r#""A"#);
        assert_first_letter_eq(r#"("A"#, r#"("A"#);
        assert_first_letter_eq(r#" ("A"#, r#"("A"#);
        assert_first_letter_eq(r#"( "A"#, r#"( "A"#);
        assert_first_letter_eq(r#"[ ( "A"#, r#"[ ( "A"#);

        // First letter and succeeding punctuation(s)
        // TODO: modify test cases when intervening spaces in succeeding puntuations is supported
        assert_first_letter_eq(r#"A""#, r#"A""#);
        assert_first_letter_eq(r#"A" "#, r#"A""#);
        assert_first_letter_eq(r#"A)]"#, r#"A)]"#);
        assert_first_letter_eq(r#"A" )]"#, r#"A""#);
        assert_first_letter_eq(r#"A)] >"#, r#"A)]"#);

        // All
        assert_first_letter_eq(r#" ("A" )]"#, r#"("A""#);
        assert_first_letter_eq(r#" ("A")] >"#, r#"("A")]"#);

        // Non ASCII chars
        assert_first_letter_eq("一", "一");
        assert_first_letter_eq(" 一 ", "一");
        assert_first_letter_eq("一二三", "一");
        assert_first_letter_eq(" 一二三 ", "一");
        assert_first_letter_eq("（一二三）", "（一");
        assert_first_letter_eq(" （一二三） ", "（一");
        assert_first_letter_eq("（（一", "（（一");
        assert_first_letter_eq(" （ （一", "（ （一");
        assert_first_letter_eq("一）", "一）");
        assert_first_letter_eq("一））", "一））");
        assert_first_letter_eq("一） ）", "一）");
    }
}

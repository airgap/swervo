/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::cmp::Ordering;

use app_units::Au;
use dom_struct::dom_struct;
use euclid::Point2D;
use js::context::{JSContext, NoGC};
use layout_api::RestyleReason;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use style::Zero;
use style_traits::CSSPixel;

use crate::dom::abstractrange::bp_position;
use crate::dom::bindings::codegen::Bindings::NodeBinding::{GetRootNodeOptions, NodeMethods};
use crate::dom::bindings::codegen::Bindings::RangeBinding::RangeMethods;
use crate::dom::bindings::codegen::Bindings::SelectionBinding::SelectionMethods;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::eventtarget::EventTarget;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::range::Range;
use crate::dom::element::Element;
use crate::dom::html::htmlbrelement::HTMLBRElement;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::text::Text;
use crate::dom::window::{LaidOutCaretLine, LaidOutCaretStop, Window};

/// What a press of the primary button selects, by its click count.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PointerSelection {
    Character,
    Word,
    Paragraph,
}

/// The unit a caret moves by, as in `Selection.modify()`.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum CaretMovement {
    Character,
    Word,
    Line,
    LineBoundary,
    DocumentBoundary,
}

#[derive(Clone, Copy, JSTraceable, MallocSizeOf)]
enum Direction {
    Forwards,
    Backwards,
    Directionless,
}

#[dom_struct]
pub(crate) struct Selection {
    reflector_: Reflector,
    document: Dom<Document>,
    range: MutNullableDom<Range>,
    direction: Cell<Direction>,
    /// <https://w3c.github.io/selection-api/#dfn-has-scheduled-selectionchange-event>
    has_scheduled_selectionchange_event: Cell<bool>,
    /// The horizontal position in the viewport that consecutive caret movements by lines aim
    /// for, which is where the first of them started, and the offset in
    /// [`Self::line_movement_goal_node`] where the last of them left the focus.
    #[no_trace]
    line_movement_goal: Cell<Option<(Au, u32)>>,
    line_movement_goal_node: MutNullableDom<Node>,
}

impl Selection {
    fn new_inherited(document: &Document) -> Selection {
        Selection {
            reflector_: Reflector::new(),
            document: Dom::from_ref(document),
            range: MutNullableDom::new(None),
            direction: Cell::new(Direction::Directionless),
            has_scheduled_selectionchange_event: Cell::new(false),
            line_movement_goal: Cell::new(None),
            line_movement_goal_node: MutNullableDom::new(None),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, document: &Document) -> DomRoot<Selection> {
        reflect_dom_object_with_cx(
            Box::new(Selection::new_inherited(document)),
            &*document.global(),
            cx,
        )
    }

    fn set_range(&self, range: &Range) {
        // If we are setting to literally the same Range object
        // (not just the same positions), then there's nothing changing
        // and no task to queue.
        if let Some(existing) = self.range.get() &&
            &*existing == range
        {
            return;
        }
        self.range.set(Some(range));
        range.associate_selection(self);
        self.queue_selectionchange_task();
    }

    fn clear_range(&self) {
        // If we already don't have a a Range object, then there's
        // nothing changing and no task to queue.
        if let Some(range) = self.range.get() {
            range.disassociate_selection(self);
            self.range.set(None);
            self.queue_selectionchange_task();
        }
    }

    /// <https://w3c.github.io/selection-api/#dfn-schedule-a-selectionchange-event>
    pub(crate) fn queue_selectionchange_task(&self) {
        // https://w3c.github.io/editing/docs/execCommand/#state-override
        // https://w3c.github.io/editing/docs/execCommand/#value-override
        // > Whenever the number of ranges in the selection changes to something
        // > different, and whenever a boundary point of the range at a given index in the
        // > selection changes to something different, the state override and value
        // > override must be unset for every command.
        self.document.clear_command_overrides();

        // Layout paints the caret and selected text of editing hosts.
        self.document.add_restyle_reason(RestyleReason::SelectionChanged);

        // Step 1. If target's has scheduled selectionchange event is true, abort these steps.
        if self.has_scheduled_selectionchange_event.get() {
            return;
        }
        // Step 2. Set target's has scheduled selectionchange event to true.
        self.has_scheduled_selectionchange_event.set(true);
        // Step 3. Queue a task on the user interaction task source to fire a
        // selectionchange event on target.
        let this = Trusted::new(self);
        self.document
            .owner_global()
            .task_manager()
            .user_interaction_task_source() // w3c/selection-api#117
            .queue(
                // https://w3c.github.io/selection-api/#firing-selectionchange-event
                task!(selectionchange_task_steps: move |cx| {
                    let this = this.root();
                    // Step 1. Set target's has scheduled selectionchange event to false.
                    this.has_scheduled_selectionchange_event.set(false);
                    // Step 2. If target is an element, fire an event named
                    // selectionchange, which bubbles and not cancelable, at target.
                    //
                    // n/a

                    // Step 3. Otherwise, if target is a document, fire an event named
                    // selectionchange, which does not bubble and not cancelable, at
                    // target.
                    this.document.upcast::<EventTarget>().fire_event(cx, atom!("selectionchange"));
                }),
            );
    }

    fn is_same_root(&self, node: &Node) -> bool {
        &*node.GetRootNode(&GetRootNodeOptions::empty()) == self.document.upcast::<Node>()
    }

    /// <https://w3c.github.io/editing/docs/execCommand/#active-range>
    pub(crate) fn active_range(&self) -> Option<DomRoot<Range>> {
        // > The active range is the range of the selection given by calling
        // > getSelection() on the context object. (Thus the active range may be null.)
        self.range.get()
    }

    pub(crate) fn collapse_current_range(&self, node: &Node, offset: u32) {
        let range = self.range.get().expect("Must always have a range");
        range.set_start(node, offset);
        range.set_end(node, offset);
    }

    pub(crate) fn extend_current_range(&self, node: &Node, offset: u32) {
        let range = self.range.get().expect("Must always have a range");
        assert!(range.collapsed(), "Must only extend after collapsing");

        let anchor_node = range.start_container();
        if (*anchor_node == *node && range.start_offset() < offset) || anchor_node.is_before(node) {
            range.set_end(node, offset);
            self.direction.set(Direction::Forwards);
        } else {
            range.set_start(node, offset);
            self.direction.set(Direction::Backwards);
        }
    }

    /// <https://w3c.github.io/selection-api/#dfn-anchor>
    pub(crate) fn anchor_node(&self) -> Option<DomRoot<Node>> {
        self.range.get().map(|range| match self.direction.get() {
            Direction::Forwards => range.start_container(),
            _ => range.end_container(),
        })
    }

    /// <https://w3c.github.io/selection-api/#dfn-anchor>
    pub(crate) fn anchor_offset(&self) -> u32 {
        self.range
            .get()
            .map(|range| match self.direction.get() {
                Direction::Forwards => range.start_offset(),
                _ => range.end_offset(),
            })
            .unwrap_or(0)
    }

    /// <https://w3c.github.io/selection-api/#dfn-focus>
    pub(crate) fn focus_node(&self) -> Option<DomRoot<Node>> {
        self.range.get().map(|range| match self.direction.get() {
            Direction::Forwards => range.end_container(),
            _ => range.start_container(),
        })
    }

    /// <https://w3c.github.io/selection-api/#dfn-focus>
    pub(crate) fn focus_offset(&self) -> u32 {
        self.range
            .get()
            .map(|range| match self.direction.get() {
                Direction::Forwards => range.end_offset(),
                _ => range.start_offset(),
            })
            .unwrap_or(0)
    }

    /// Selects for a press of the primary button at `point` (in the viewport) over `hit_node`:
    /// puts the caret at the closest caret position, or selects the word or the paragraph there.
    /// With `extend` only the focus moves to the closest caret position, as when dragging or
    /// pressing with Shift.
    pub(crate) fn select_at_point(
        &self,
        cx: &mut JSContext,
        hit_node: &Node,
        point: Point2D<Au, CSSPixel>,
        granularity: PointerSelection,
        extend: bool,
    ) {
        let extend = extend && self.range.get().is_some();
        // Like in other browsers, a selection stays in the editing host it starts in, and one
        // that starts outside editing hosts does not enter them.
        let editing_host = if extend {
            self.anchor_node().and_then(|anchor| anchor.editing_host_of())
        } else {
            hit_node.editing_host_of()
        };
        let window = self.document.window();
        let lines = match &editing_host {
            Some(editing_host) => window.caret_stops_query(editing_host),
            None => caret_stops_around(window, hit_node),
        };
        let Some((_, mut stop)) = closest_caret_stop(&lines, point) else {
            return;
        };
        // Dragged above or below the editing host it stays in, the focus goes to its start or end.
        if let Some(editing_host) = &editing_host &&
            !editing_host.is_inclusive_ancestor_of(hit_node)
        {
            let (first, last) = (&lines[0], &lines[lines.len() - 1]);
            if point.y < first.top {
                stop = &first.stops[0];
            } else if point.y >= last.bottom {
                stop = &last.stops[last.stops.len() - 1];
            }
        }

        let result = match granularity {
            PointerSelection::Character => {
                let (node, offset) = if editing_host.is_none() {
                    self.position_outside_editing_hosts(&stop.node, stop.offset, extend)
                } else {
                    (stop.node.clone(), stop.offset)
                };
                if extend {
                    self.Extend(cx, &node, offset)
                } else {
                    self.Collapse(cx, Some(&node), offset)
                }
            },
            // Like in other browsers, this selects what follows the closest caret position.
            PointerSelection::Word => {
                match stop
                    .node
                    .downcast::<Text>()
                    .and_then(|text| word_around(&text.data(), stop.offset))
                {
                    Some(word) => self.SetBaseAndExtent(
                        cx,
                        &stop.node,
                        word.start,
                        &stop.node,
                        word.end,
                    ),
                    None => self.Collapse(cx, Some(&stop.node), stop.offset),
                }
            },
            PointerSelection::Paragraph => {
                let ((start, start_offset), (end, end_offset)) =
                    paragraph_around(&stop.node, stop.offset, editing_host.as_deref());
                self.SetBaseAndExtent(cx, &start, start_offset, &end, end_offset)
            },
        };
        result.expect("Laid out caret positions are valid boundary points");
    }

    /// A selection made outside editing hosts ends before or after an editing host instead of
    /// at a position in it: before it when the focus moves forward from the anchor.
    fn position_outside_editing_hosts(
        &self,
        node: &Node,
        offset: u32,
        extend: bool,
    ) -> (DomRoot<Node>, u32) {
        let Some(editing_host) = node.editing_host_of() else {
            return (DomRoot::from_ref(node), offset);
        };
        let parent = editing_host
            .GetParentNode()
            .expect("Editing hosts have a parent");
        let index = editing_host.index();
        let backward = extend &&
            self.anchor_node().is_some_and(|anchor| {
                bp_position(&parent, index, &anchor, self.anchor_offset()) ==
                    Some(Ordering::Less)
            });
        (parent, if backward { index + 1 } else { index })
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-modify>, for selections in
    /// editable content, using the caret positions of the laid out text.
    pub(crate) fn modify_in_editable_content(
        &self,
        cx: &mut JSContext,
        extend: bool,
        forward: bool,
        movement: CaretMovement,
    ) {
        let (Some(range), Some(focus_node)) = (self.range.get(), self.focus_node()) else {
            return;
        };
        let Some(editing_host) = focus_node.editing_host_of() else {
            return;
        };

        // Moving a non-collapsed selection by a character collapses it to the side it moves to.
        let (start_node, start_offset) = if !extend && !range.collapsed() {
            if movement == CaretMovement::Character {
                let result = if forward {
                    self.CollapseToEnd(cx)
                } else {
                    self.CollapseToStart(cx)
                };
                result.expect("The selection has a range");
                return;
            }
            if forward {
                (range.end_container(), range.end_offset())
            } else {
                (range.start_container(), range.start_offset())
            }
        } else {
            (focus_node, self.focus_offset())
        };

        let lines = self.document.window().caret_stops_query(&editing_host);
        let positions: Vec<(usize, usize)> = lines
            .iter()
            .enumerate()
            .flat_map(|(line_index, line)| {
                (0..line.stops.len()).map(move |stop_index| (line_index, stop_index))
            })
            .collect();
        let stop_at =
            |(line_index, stop_index): (usize, usize)| &lines[line_index].stops[stop_index];
        let Some(current) = positions
            .iter()
            .position(|&position| {
                let stop = stop_at(position);
                *stop.node == *start_node && stop.offset == start_offset
            })
            .or_else(|| {
                positions.iter().position(|&position| {
                    let stop = stop_at(position);
                    bp_position(&stop.node, stop.offset, &start_node, start_offset) !=
                        Some(Ordering::Less)
                })
            })
            .or(positions.len().checked_sub(1))
        else {
            return;
        };
        let (current_line, _) = positions[current];

        // Whether the character between two consecutive positions is part of a word; a line
        // break is not.
        let is_word_character_between = |first: usize, second: usize| {
            let (first, second) = (positions[first], positions[second]);
            first.0 == second.0 &&
                character_before(stop_at(second))
                    .is_some_and(is_word_character)
        };

        let mut line_goal_x = None;
        let target = match movement {
            CaretMovement::Character if forward => (current + 1).min(positions.len() - 1),
            CaretMovement::Character => current.saturating_sub(1),
            // Like other browsers on Linux, words are left at their end going forward and at their
            // start going backward.
            CaretMovement::Word if forward => {
                let mut target = current;
                while target + 1 < positions.len() && !is_word_character_between(target, target + 1)
                {
                    target += 1;
                }
                while target + 1 < positions.len() &&
                    is_word_character_between(target, target + 1)
                {
                    target += 1;
                }
                target
            },
            CaretMovement::Word => {
                let mut target = current;
                while target > 0 && !is_word_character_between(target - 1, target) {
                    target -= 1;
                }
                while target > 0 && is_word_character_between(target - 1, target) {
                    target -= 1;
                }
                target
            },
            CaretMovement::Line => {
                // Like in other browsers, consecutive movements by lines keep aiming for the
                // horizontal position where the first one started.
                let x = match self.line_movement_goal.get() {
                    Some((x, offset))
                        if offset == start_offset &&
                            self.line_movement_goal_node
                                .get()
                                .is_some_and(|node| node == start_node) =>
                    {
                        x
                    },
                    _ => stop_at(positions[current]).x,
                };
                line_goal_x = Some(x);
                let target_line = if forward {
                    Some(current_line + 1).filter(|line| *line < lines.len())
                } else {
                    current_line.checked_sub(1)
                };
                match target_line {
                    Some(target_line) => {
                        let line_start = positions
                            .iter()
                            .position(|position| position.0 == target_line)
                            .expect("Every line has a position");
                        let (closest, _) = lines[target_line]
                            .stops
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, stop)| (stop.x - x).abs())
                            .expect("Every line has a position");
                        line_start + closest
                    },
                    // There is no line to move to, so go to the start or end of this one.
                    None if forward => positions.len() - 1,
                    None => 0,
                }
            },
            CaretMovement::LineBoundary => {
                let mut on_line = positions
                    .iter()
                    .enumerate()
                    .filter(|(_, position)| position.0 == current_line)
                    .map(|(index, _)| index);
                if forward {
                    on_line.last()
                } else {
                    on_line.next()
                }
                .expect("The current line has a position")
            },
            CaretMovement::DocumentBoundary if forward => positions.len() - 1,
            CaretMovement::DocumentBoundary => 0,
        };
        let target = stop_at(positions[target]);
        self.move_to_stop(cx, target, extend);
        self.line_movement_goal
            .set(line_goal_x.map(|x| (x, target.offset)));
        self.line_movement_goal_node
            .set(line_goal_x.map(|_| &*target.node));
    }

    fn move_to_stop(&self, cx: &mut JSContext, stop: &LaidOutCaretStop, extend: bool) {
        let result = if extend && self.range.get().is_some() {
            self.Extend(cx, &stop.node, stop.offset)
        } else {
            self.Collapse(cx, Some(&stop.node), stop.offset)
        };
        result.expect("Caret stops are valid boundary points");
    }
}

/// The character just before a caret position in a text node, if any.
fn character_before(stop: &LaidOutCaretStop) -> Option<char> {
    let text = stop.node.downcast::<Text>()?;
    let offset = stop.offset as usize;
    let units: Vec<u16> = text.data().encode_utf16().take(offset).collect();
    char::decode_utf16(units.iter().rev().take(2).rev().copied())
        .last()?
        .ok()
}

/// The caret positions, by line, of the nearest block around `node` that has laid out text.
pub(crate) fn caret_stops_around(window: &Window, node: &Node) -> Vec<LaidOutCaretLine> {
    let mut block = node.block_node_of();
    while let Some(current) = block.filter(|block| block.is::<Element>()) {
        let lines = window.caret_stops_query(&current);
        if !lines.is_empty() {
            return lines;
        }
        block = current
            .GetParentNode()
            .and_then(|parent| parent.block_node_of());
    }
    Vec::new()
}

/// The caret position closest to `point` (in the viewport) and its line: on the closest line,
/// or at the end of the line past either end of it, like in other browsers.
pub(crate) fn closest_caret_stop(
    lines: &[LaidOutCaretLine],
    point: Point2D<Au, CSSPixel>,
) -> Option<(&LaidOutCaretLine, &LaidOutCaretStop)> {
    let distance_to_line =
        |line: &LaidOutCaretLine| (line.top - point.y).max(point.y - line.bottom).max(Au::zero());
    let line = lines.iter().min_by_key(|line| distance_to_line(line))?;
    let stop = line
        .stops
        .iter()
        .min_by_key(|stop| (stop.x - point.x).abs())
        .expect("Laid out lines have caret positions");
    Some((line, stop))
}

/// Whether a character is part of a word, for moving and selecting by words.
fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// The UTF-16 range of what a double click at `offset` in `text` selects, as in other browsers
/// on Linux: the word there, the run of white space there, or the other character there. At the
/// end of the text it is what ends there.
fn word_around(text: &str, offset: u32) -> Option<std::ops::Range<u32>> {
    let mut characters = Vec::new();
    let mut start = 0;
    for character in text.chars() {
        characters.push((start, character));
        start += character.len_utf16() as u32;
    }
    let index = match characters.iter().position(|(start, _)| *start >= offset) {
        Some(index) if characters[index].0 == offset => index,
        Some(index) => index.checked_sub(1)?,
        None => characters.len().checked_sub(1)?,
    };
    let class = |character: char| {
        if is_word_character(character) {
            0
        } else if character.is_whitespace() {
            1
        } else {
            2
        }
    };
    let (word_start, character) = characters[index];
    let range_end = |index: usize| {
        characters
            .get(index + 1)
            .map_or(start, |(start, _)| *start)
    };
    if class(character) == 2 {
        return Some(word_start..range_end(index));
    }
    let first = characters[..index]
        .iter()
        .rposition(|(_, other)| class(*other) != class(character))
        .map_or(0, |other| other + 1);
    let last = characters[index..]
        .iter()
        .position(|(_, other)| class(*other) != class(character))
        .map_or(characters.len() - 1, |other| index + other - 1);
    Some(characters[first].0..range_end(last))
}

/// A piece of the content of a block, in tree order, for finding its paragraphs.
enum BlockContent {
    Text(DomRoot<Node>),
    LineBreak(DomRoot<Node>),
    /// A nested block, which is not part of the paragraphs around it.
    Block(DomRoot<Node>),
}

fn collect_block_content(node: &Node, content: &mut Vec<BlockContent>) {
    for child in node.children() {
        if child.is::<Text>() {
            content.push(BlockContent::Text(child));
        } else if child.is::<HTMLBRElement>() {
            content.push(BlockContent::LineBreak(child));
        } else if child.is::<Element>() {
            if child.is_block_node() {
                content.push(BlockContent::Block(child));
            } else {
                collect_block_content(&child, content);
            }
        }
    }
}

/// The start and end of what a triple click at (`node`, `offset`) selects, as in other
/// browsers: the paragraph there with the line break that ends it, or up to the start of the
/// next block. A paragraph in an editing host does not extend out of it.
fn paragraph_around(
    node: &Node,
    offset: u32,
    editing_host: Option<&Node>,
) -> ((DomRoot<Node>, u32), (DomRoot<Node>, u32)) {
    let block = node
        .block_node_of()
        .expect("Laid out caret positions are in a block");
    let mut content = Vec::new();
    collect_block_content(&block, &mut content);

    // The node at the position, or the last one when the position is at the end of its parent.
    let position_node = if node.is::<Text>() {
        DomRoot::from_ref(node)
    } else {
        node.children()
            .nth(offset as usize)
            .or_else(|| node.children().last())
            .unwrap_or_else(|| DomRoot::from_ref(node))
    };
    let content_node = |piece: &BlockContent| match piece {
        BlockContent::Text(node) | BlockContent::LineBreak(node) | BlockContent::Block(node) => {
            node.clone()
        },
    };
    let Some(index) = content
        .iter()
        .position(|piece| position_node.is_inclusive_ancestor_of(&content_node(piece)))
    else {
        return (
            (DomRoot::from_ref(node), offset),
            (DomRoot::from_ref(node), offset),
        );
    };

    let is_text = |piece: &BlockContent| matches!(piece, BlockContent::Text(_));
    let first = content[..index]
        .iter()
        .rposition(|piece| !is_text(piece))
        .map_or(0, |other| other + 1);
    let end_index = content[index..]
        .iter()
        .position(|piece| !is_text(piece))
        .map(|other| index + other);

    let start = match &content[first] {
        BlockContent::Text(text) => (text.clone(), 0),
        BlockContent::LineBreak(node) | BlockContent::Block(node) => (
            node.GetParentNode().expect("Content of a block has a parent"),
            node.index(),
        ),
    };
    // The end of the last text of the paragraph, if it has text.
    let end_of_text = |end_index: usize| {
        (end_index > first).then(|| {
            let last = content_node(&content[end_index - 1]);
            let length = last.len();
            (last, length)
        })
    };
    let end = match end_index {
        Some(end_index) => match &content[end_index] {
            BlockContent::LineBreak(line_break) => (
                line_break
                    .GetParentNode()
                    .expect("Content of a block has a parent"),
                line_break.index() + 1,
            ),
            _ => end_of_text(end_index).unwrap_or_else(|| start.clone()),
        },
        None => start_of_next_block(&block, editing_host)
            .or_else(|| end_of_text(content.len()))
            .unwrap_or_else(|| start.clone()),
    };
    (start, end)
}

/// The start of the first block or text after `block` in tree order, without leaving
/// `editing_host`, or entering editing hosts when it is `None`.
fn start_of_next_block(block: &Node, editing_host: Option<&Node>) -> Option<(DomRoot<Node>, u32)> {
    let document = block.owner_document();
    let root = editing_host.unwrap_or(document.upcast::<Node>());
    block
        .following_nodes(root, ShadowIncluding::No)
        .filter(|node| !block.is_ancestor_of(node))
        .find(|node| {
            let is_start = match node.downcast::<Text>() {
                Some(text) => !text.is_whitespace_node(),
                None => node.is::<Element>() && node.is_block_node(),
            };
            is_start && node.editing_host_of().as_deref() == editing_host
        })
        .map(|node| (node, 0))
}

impl SelectionMethods<crate::DomTypeHolder> for Selection {
    /// <https://w3c.github.io/selection-api/#dom-selection-anchornode>
    fn GetAnchorNode(&self) -> Option<DomRoot<Node>> {
        // > The attribute must return the anchor node of this, or null if the anchor is
        // > null or anchor is not in the document tree.
        let anchor_node = self.anchor_node()?;
        if !anchor_node.is_in_a_document_tree() {
            return None;
        }
        Some(anchor_node)
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-anchoroffset>
    fn AnchorOffset(&self) -> u32 {
        // > The attribute must return the anchor offset of this, or 0 if the anchor is null
        // > or anchor is not in the document tree.
        if self
            .anchor_node()
            .is_none_or(|anchor_node| !anchor_node.is_in_a_document_tree())
        {
            return 0;
        }
        self.anchor_offset()
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-focusnode>
    fn GetFocusNode(&self) -> Option<DomRoot<Node>> {
        // > The attribute must return the focus node of this, or null if the focus is
        // > null or focus is not in the document tree.
        let focus_node = self.focus_node()?;
        if !focus_node.is_in_a_document_tree() {
            return None;
        }
        Some(focus_node)
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-focusoffset>
    fn FocusOffset(&self) -> u32 {
        // > The attribute must return the focus offset of this, or 0 if the focus is null
        // > or focus is not in the document tree.
        if self
            .focus_node()
            .is_none_or(|focus_node| !focus_node.is_in_a_document_tree())
        {
            return 0;
        }
        self.focus_offset()
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-iscollapsed>
    fn IsCollapsed(&self) -> bool {
        // > The attribute must return true if and only if the anchor and focus are the
        // > same (including if both are null). Otherwise it must return false.
        self.range.get().is_none_or(|range| range.collapsed())
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-rangecount>
    fn RangeCount(&self) -> u32 {
        // > The attribute must return 0 if this is empty or either focus or anchor is not
        // > in the document tree, and must return 1 otherwise.
        let Some(range) = self.range.get() else {
            return 0;
        };
        if !range.start_and_end_are_in_document_tree() {
            return 0;
        }
        1
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-type>
    fn Type(&self) -> DOMString {
        // > The attribute must return "None" if this is empty or either focus or anchor
        // > is not in the document tree, "Caret" if this's range is collapsed, and "Range"
        // > otherwise.
        let Some(range) = self.range.get() else {
            return DOMString::from("None");
        };
        if !range.start_and_end_are_in_document_tree() {
            return DOMString::from("None");
        }

        if range.collapsed() {
            DOMString::from("Caret")
        } else {
            DOMString::from("Range")
        }
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-getrangeat>
    fn GetRangeAt(&self, index: u32) -> Fallible<DomRoot<Range>> {
        // > The method must throw an IndexSizeError exception if index is not 0, or if this
        // > is empty or either focus or anchor is not in the document tree. Otherwise, it
        // > must return a reference to (not a copy of) this's range.
        if index != 0 {
            return Err(Error::IndexSize(None));
        }

        let Some(range) = self.range.get() else {
            return Err(Error::IndexSize(None));
        };

        if !range.start_and_end_are_in_document_tree() {
            return Err(Error::IndexSize(None));
        }

        Ok(DomRoot::from_ref(&range))
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-addrange>
    fn AddRange(&self, range: &Range) {
        // Step 1. If the root of the range's boundary points are not the document
        // associated with this, abort these steps.
        if !self.is_same_root(&range.start_container()) {
            return;
        }

        // Step 2. If rangeCount is not 0, abort these steps.
        if self.RangeCount() != 0 {
            return;
        }

        // Step 3. Set this's range to range by a strong reference (not by making a copy).
        self.set_range(range);

        // Are we supposed to set Direction here? w3c/selection-api#116
        self.direction.set(Direction::Forwards);
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-removerange>
    fn RemoveRange(&self, range: &Range) -> ErrorResult {
        // > The method must make this empty by disassociating its range if this's range
        // > is range. Otherwise, it must throw a NotFoundError.
        if let Some(own_range) = self.range.get() &&
            &*own_range == range
        {
            self.clear_range();
            return Ok(());
        }
        Err(Error::NotFound(None))
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-removeallranges>
    fn RemoveAllRanges(&self) {
        // > The method must make this empty by disassociating its range if this has an
        // > associated range.
        self.clear_range();
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-empty>
    fn Empty(&self) {
        // > The method must be an alias, and behave identically, to removeAllRanges().
        self.clear_range();
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-collapse>
    fn Collapse(&self, cx: &mut JSContext, node: Option<&Node>, offset: u32) -> ErrorResult {
        // Step 1. If node is null, this method must behave identically as
        // removeAllRanges() and abort these steps.
        let Some(node) = node else {
            self.clear_range();
            return Ok(());
        };

        // Step 2. If node is a DocumentType, throw an InvalidNodeTypeError exception and
        // abort these steps.
        if node.is_doctype() {
            return Err(Error::InvalidNodeType(None));
        }

        // Step 3. The method must throw an IndexSizeError exception if offset is longer
        // than node's length and abort these steps.
        if offset > node.len() {
            return Err(Error::IndexSize(None));
        }

        // Step 4. If document associated with this is not a shadow-including inclusive
        // ancestor of node, abort these steps.
        //
        // TODO: `is_same_root` does reach beyond shadow root boundaries, so this check is
        // wrong.
        if !self.is_same_root(node) {
            return Ok(());
        }

        // Step 5. Otherwise, let newRange be a new range.
        // Step 6. Set the start the start and the end of newRange to (node, offset).
        let new_range = Range::new(cx, &self.document, node, offset, node, offset);

        // Step 7. Set this's range to newRange.
        self.set_range(&new_range);

        // Are we supposed to set Direction here? w3c/selection-api#116
        self.direction.set(Direction::Forwards);

        Ok(())
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-setposition>
    fn SetPosition(&self, cx: &mut JSContext, node: Option<&Node>, offset: u32) -> ErrorResult {
        // > The method must be an alias, and behave identically, to collapse().
        self.Collapse(cx, node, offset)
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-collapsetostart>
    fn CollapseToStart(&self, cx: &mut JSContext) -> ErrorResult {
        // > The method must throw InvalidStateError exception if the this is empty.
        // > Otherwise, it must create a new range, set the start both its start and end to
        // > the start of this's range, and then set this's range to the newly-created
        // > range.
        if let Some(range) = self.range.get() {
            self.Collapse(cx, Some(&*range.start_container()), range.start_offset())
        } else {
            Err(Error::InvalidState(None))
        }
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-collapsetoend>
    fn CollapseToEnd(&self, cx: &mut JSContext) -> ErrorResult {
        // > The method must throw InvalidStateError exception if the this is empty.
        // > Otherwise, it must create a new range, set the start both its start and end to
        // > the end of this's range, and then set this's range to the newly-created range.
        if let Some(range) = self.range.get() {
            self.Collapse(cx, Some(&*range.end_container()), range.end_offset())
        } else {
            Err(Error::InvalidState(None))
        }
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-extend>
    fn Extend(&self, cx: &mut JSContext, node: &Node, offset: u32) -> ErrorResult {
        // Step 1. If the document associated with this is not a shadow-including
        // inclusive ancestor of node, abort these steps.
        //
        // TODO: `is_same_root` does reach beyond shadow root boundaries, so this check is
        // wrong.
        if !self.is_same_root(node) {
            return Ok(());
        }

        // Step 2. If this is empty, throw an InvalidStateError exception and abort these steps.
        let Some(range) = self.range.get() else {
            return Err(Error::InvalidState(None));
        };

        // This isn't specified, but it appears to be implementation behavior of other
        // browsers. See w3c/selection-api#118.
        if node.is_doctype() {
            return Err(Error::InvalidNodeType(None));
        }

        // As with is_doctype, this is not explicit in the selection specification steps
        // here but implied by which exceptions are thrown in WPT tests.
        if offset > node.len() {
            return Err(Error::IndexSize(None));
        }

        // Step 3. Let oldAnchor and oldFocus be the this's anchor and focus, and let
        // newFocus be the boundary point (node, offset).
        //
        // Note: oldFocus is unused, so we do not set it here.
        let old_anchor_node = &*self
            .anchor_node()
            .expect("has range, therefore has anchor node");
        let old_anchor_offset = self.anchor_offset();

        // Step 4. Let newRange be a new range.
        let new_range;
        let direction;

        // Step 5. If node's root is not the same as the this's range's root, set the
        // start newRange's start and end to newFocus.
        if !self.is_same_root(&range.start_container()) {
            new_range = Range::new(cx, &self.document, node, offset, node, offset);
            direction = Direction::Forwards;
        } else {
            let is_old_anchor_before_or_equal = matches!(
                bp_position(old_anchor_node, old_anchor_offset, node, offset),
                Some(Ordering::Less) | Some(Ordering::Equal)
            );
            if is_old_anchor_before_or_equal {
                // Step 6. Otherwise, if oldAnchor is before or equal to newFocus, set the start
                // newRange's start to oldAnchor, then set its end to newFocus.
                new_range = Range::new(
                    cx,
                    &self.document,
                    old_anchor_node,
                    old_anchor_offset,
                    node,
                    offset,
                );
                direction = Direction::Forwards;
            } else {
                // Step 7. Otherwise, set the start newRange's start to newFocus, then set
                // its end to oldAnchor.
                new_range = Range::new(
                    cx,
                    &self.document,
                    node,
                    offset,
                    old_anchor_node,
                    old_anchor_offset,
                );
                direction = Direction::Backwards;
            }
        }

        // Step 8. Set this's range to newRange.
        self.set_range(&new_range);

        // Step 9. If newFocus is before oldAnchor, set this's direction to backwards.
        // Otherwise, set it to forwards.
        self.direction.set(direction);

        Ok(())
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-setbaseandextent>
    fn SetBaseAndExtent(
        &self,
        cx: &mut JSContext,
        anchor_node: &Node,
        anchor_offset: u32,
        focus_node: &Node,
        focus_offset: u32,
    ) -> ErrorResult {
        // This isn't specified, but it appears to be implementation behavior of other
        // browsers. See w3c/selection-api#118.
        if anchor_node.is_doctype() || focus_node.is_doctype() {
            return Err(Error::InvalidNodeType(None));
        }

        // Step 1. If anchorOffset is longer than anchorNode's length or if focusOffset is
        // longer than focusNode's length, throw an IndexSizeError exception and abort
        // these steps.
        if anchor_offset > anchor_node.len() || focus_offset > focus_node.len() {
            return Err(Error::IndexSize(None));
        }

        // Step 2. If document associated with this is not a shadow-including inclusive
        // ancestor of anchorNode or focusNode, abort these steps.
        //
        // TODO: `is_same_root` does reach beyond shadow root boundaries, so this check is
        // wrong.
        if !self.is_same_root(anchor_node) || !self.is_same_root(focus_node) {
            return Ok(());
        }

        // Step 3. Let anchor be the boundary point (anchorNode, anchorOffset) and let
        // focus be the boundary point (focusNode, focusOffset).
        //
        // Note: We do not model the boundary point in this way.

        // Step 4. Let newRange be a new range.
        let new_range;
        let direction;

        // Step 5. If anchor is before focus, set the start the newRange's start to anchor
        // and its end to focus. Otherwise, set the start them to focus and anchor
        // respectively.
        let is_anchor_before_focus =
            bp_position(anchor_node, anchor_offset, focus_node, focus_offset) ==
                Some(Ordering::Less);
        if is_anchor_before_focus {
            new_range = Range::new(
                cx,
                &self.document,
                anchor_node,
                anchor_offset,
                focus_node,
                focus_offset,
            );
            direction = Direction::Forwards;
        } else {
            new_range = Range::new(
                cx,
                &self.document,
                focus_node,
                focus_offset,
                anchor_node,
                anchor_offset,
            );
            direction = Direction::Backwards;
        }

        // Step 6. Set this's range to newRange.
        self.set_range(&new_range);

        // Step 7. If focus is before anchor, set this's direction to backwards.
        // Otherwise, set it to forwards
        self.direction.set(direction);

        Ok(())
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-modify>
    fn Modify(
        &self,
        cx: &mut JSContext,
        alter: DOMString,
        direction: DOMString,
        granularity: DOMString,
    ) {
        // Step 1. If alter is not ASCII case-insensitive match with "extend" or "move", abort
        // these steps.
        let extend = match &*alter.str().to_ascii_lowercase() {
            "extend" => true,
            "move" => false,
            _ => return,
        };
        // Step 2. If direction is not ASCII case-insensitive match with "forward", "backward",
        // "left", or "right", abort these steps.
        // TODO: "left" and "right" depend on the direction of the text; only left-to-right
        // text is handled.
        let forward = match &*direction.str().to_ascii_lowercase() {
            "forward" | "right" => true,
            "backward" | "left" => false,
            _ => return,
        };
        // Step 3. If granularity is not ASCII case-insensitive match with "character", "word",
        // "sentence", "line", "paragraph", "lineboundary", "sentenceboundary",
        // "paragraphboundary", "documentboundary", abort these steps.
        // TODO: Sentences and paragraphs are not supported.
        let movement = match &*granularity.str().to_ascii_lowercase() {
            "character" => CaretMovement::Character,
            "word" => CaretMovement::Word,
            "line" => CaretMovement::Line,
            "lineboundary" => CaretMovement::LineBoundary,
            "documentboundary" => CaretMovement::DocumentBoundary,
            _ => return,
        };
        // Step 4. If this selection is empty, abort these steps.
        // Step 5. Let effectiveDirection be backwards.
        // Step 6. If direction is ASCII case-insensitive match with "forward", set
        // effectiveDirection to forwards.
        // Step 7. If direction is ASCII case-insensitive match with "right" and inline base
        // direction of this selection's focus is ltr, set effectiveDirection to forwards.
        // Step 8. If direction is ASCII case-insensitive match with "left" and inline base
        // direction of this selection's focus is rtl, set effectiveDirection to forwards.
        // Step 9. Set this selection's direction to effectiveDirection.
        // Step 10. If alter is ASCII case-insensitive match with "extend", set this
        // selection's focus to the location as if the user had requested to extend selection
        // by granularity.
        // Step 11. Otherwise, set this selection's focus and anchor to the location as if the
        // user had requested to move selection by granularity.
        //
        // TODO: Only caret positions in editable content are known.
        self.modify_in_editable_content(cx, extend, forward, movement);
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-selectallchildren>
    fn SelectAllChildren(&self, cx: &mut JSContext, node: &Node) -> ErrorResult {
        // Step 1. If node is a DocumentType, throw an InvalidNodeTypeError exception and
        // abort these steps.
        if node.is_doctype() {
            return Err(Error::InvalidNodeType(None));
        }

        // Step 2. If node's root is not the document associated with this, abort these
        // steps.
        if !self.is_same_root(node) {
            return Ok(());
        }

        // Let newRange be a new range and childCount be the number of children of node.
        let child_count = node.children_count();

        // Step 4. Set newRange's start to (node, 0).
        // Step 5. Set newRange's end to (node, childCount).
        let new_range = Range::new(cx, &self.document, node, 0, node, child_count);

        // Step 6. Set this's range to newRange.
        self.set_range(&new_range);

        // Step 7. Set this's direction to forwards.
        self.direction.set(Direction::Forwards);

        Ok(())
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-deletecontents>
    fn DeleteFromDocument(&self, cx: &mut JSContext) -> ErrorResult {
        // > The method must invoke deleteContents() on this's range if this is not empty
        // > and both focus and anchor are in the document tree. Otherwise the method must
        // > do nothing.
        let Some(range) = self.range.get() else {
            return Ok(());
        };
        if !range.start_and_end_are_in_document_tree() {
            return Ok(());
        }

        range.DeleteContents(cx)
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-containsnode>
    fn ContainsNode(&self, node: &Node, allow_partial_containment: bool) -> bool {
        // > The method must return false if this is empty or if node's root is not the document
        // > associated with this.
        // >
        // > Otherwise, if allowPartialContainment is false, the method must return true if and only
        // > if start of its range is before or visually equivalent to the first boundary point in
        // > the node *and* end of its range is after or visually equivalent to the last boundary
        // > point in the node.
        // >
        // > If allowPartialContainment is true, the method must return true if and only if start of
        // > its range is before or visually equivalent to the last boundary point in the node *and*
        // > end of its range is after or visually equivalent to the first boundary point in the
        // > node.

        if !self.is_same_root(node) {
            return false;
        }
        let Some(range) = self.range.get() else {
            return false;
        };
        let start_node = &*range.start_container();
        if !self.is_same_root(start_node) {
            // node can't be contained in a range with a different root
            return false;
        }
        let end_node = &*range.end_container();

        let first_offset = 0;
        let last_offset = node.len();
        let (compare_start_to, compare_end_to) = if allow_partial_containment {
            (last_offset, first_offset)
        } else {
            (first_offset, last_offset)
        };

        // TODO: find out what "visually equivalent" means for boundary points and implement it.
        // https://github.com/w3c/selection-api/issues/6
        // For now it is simplified to "position is equal".
        matches!(
            bp_position(start_node, range.start_offset(), node, compare_start_to),
            Some(Ordering::Less) | Some(Ordering::Equal)
        ) && matches!(
            bp_position(end_node, range.end_offset(), node, compare_end_to),
            Some(Ordering::Greater) | Some(Ordering::Equal)
        )
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-stringifier>
    fn Stringifier(&self, no_gc: &NoGC) -> DOMString {
        // > The stringification must return the string, which is the concatenation of the
        // > rendered text if there is a range associated with this.
        // >
        // > If the selection is within a textarea or input element, it must return the
        // > selected substring in its value.
        //
        // TODO: This implementation should be examined in depth. Does rendered text take
        // into account `display: none`. The case for textarea and input elements is
        // completely unhandled here.
        if let Some(range) = self.range.get() {
            range.Stringifier(no_gc)
        } else {
            DOMString::from("")
        }
    }
}

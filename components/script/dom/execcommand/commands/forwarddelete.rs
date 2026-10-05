/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use js::context::JSContext;
use script_bindings::inheritance::Castable;

use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::characterdata::CharacterData;
use crate::dom::document::Document;
use crate::dom::html::htmlbrelement::HTMLBRElement;
use crate::dom::html::htmlhrelement::HTMLHRElement;
use crate::dom::html::htmlimageelement::HTMLImageElement;
use crate::dom::html::htmltableelement::HTMLTableElement;
use crate::dom::selection::Selection;
use crate::dom::text::Text;

/// <https://w3c.github.io/editing/docs/execCommand/#the-forwarddelete-command>
pub(crate) fn execute_forward_delete_command(
    cx: &mut JSContext,
    document: &Document,
    selection: &Selection,
) -> bool {
    let active_range = selection
        .active_range()
        .expect("Must always have an active range");
    // Step 1. If the active range is not collapsed, delete the selection and return true.
    if !active_range.collapsed() {
        selection.delete_the_selection(
            cx,
            document,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        return true;
    }

    // Step 2. Canonicalize whitespace at the active range's start.
    active_range
        .start_container()
        .canonicalize_whitespace(cx, active_range.start_offset(), true);

    // Step 3. Let node and offset be the active range's start node and offset.
    let mut node = active_range.start_container();
    let mut offset = active_range.start_offset();

    // Step 4. Repeat the following steps:
    loop {
        // Step 4.1. If offset is the length of node and node's nextSibling is an editable
        // invisible node, remove node's nextSibling from its parent.
        if offset == node.len() &&
            let Some(sibling) = node.GetNextSibling() &&
            sibling.is_editable() &&
            sibling.is_invisible(cx.no_gc())
        {
            sibling.remove_self(cx);
            continue;
        }
        // Step 4.2. Otherwise, if node has a child with index offset and that child is an
        // editable invisible node, remove that child from node.
        let child = node.children().nth(offset as usize);
        if let Some(child) = &child &&
            child.is_editable() &&
            child.is_invisible(cx.no_gc())
        {
            child.remove_self(cx);
            continue;
        }
        // Step 4.3. Otherwise, if offset is the length of node and node is an inline node, or if
        // node is invisible, set offset to one plus the index of node, then set node to its
        // parent.
        if (offset == node.len() && node.is_inline_node()) || node.is_invisible(cx.no_gc()) {
            offset = 1 + node.index();
            node = node.GetParentNode().expect("Must always have a parent");
            continue;
        }
        // Step 4.4. Otherwise, if node has a child with index offset and that child is neither a
        // block node nor a br nor an img nor a collapsed block prop, set node to that child,
        // then set offset to zero.
        if let Some(child) = child &&
            !(child.is_block_node() ||
                child.is::<HTMLBRElement>() ||
                child.is::<HTMLImageElement>() ||
                child.is_collapsed_block_prop(cx.no_gc()))
        {
            node = child;
            offset = 0;
            continue;
        }
        // Step 4.5. Otherwise, break from this loop.
        break;
    }

    // Step 5. If node is a Text node and offset is not node's length:
    if let Some(text) = node.downcast::<Text>() &&
        offset != node.len()
    {
        // Step 5.1. Let end offset be offset plus one.
        // Step 5.2. While end offset is not node's length and the end offsetth code unit of
        // node's data has general category M when interpreted as a Unicode code point, add one
        // to end offset.
        let data: Vec<u16> = text
            .upcast::<CharacterData>()
            .data()
            .encode_utf16()
            .collect();
        let mut end_offset = offset + 1;
        // A surrogate pair is one character.
        if (0xD800..0xDC00).contains(&data[offset as usize]) && end_offset < node.len() {
            end_offset += 1;
        }
        while end_offset != node.len() && is_combining_mark(data[end_offset as usize]) {
            end_offset += 1;
        }
        // Step 5.3. Call collapse(node, offset) on the context object's selection.
        selection.collapse_current_range(&node, offset);
        // Step 5.4. Call extend(node, end offset) on the context object's selection.
        selection.extend_current_range(&node, end_offset);
        // Step 5.5. Delete the selection.
        selection.delete_the_selection(
            cx,
            document,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        // Step 5.6. Return true.
        return true;
    }

    // Step 6. If node is an inline node, return true.
    if node.is_inline_node() {
        return true;
    }

    // Step 7. If node has a child with index offset and that child is a br or hr or img, but is
    // not a collapsed br:
    if node.children().nth(offset as usize).is_some_and(|child| {
        child.is::<HTMLHRElement>() ||
            child.is::<HTMLImageElement>() ||
            (child.is::<HTMLBRElement>() && !child.precedes_a_line_break(cx.no_gc()))
    }) {
        // Step 7.1. Call collapse(node, offset) on the context object's selection.
        selection.collapse_current_range(&node, offset);
        // Step 7.2. Call extend(node, offset + 1) on the context object's selection.
        selection.extend_current_range(&node, offset + 1);
        // Step 7.3. Delete the selection.
        selection.delete_the_selection(
            cx,
            document,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        // Step 7.4. Return true.
        return true;
    }

    // Step 8. Let end node equal node and let end offset equal offset.
    let mut end_node = node.clone();
    let mut end_offset = offset;

    // Step 9. If end node has a child with index end offset, and that child is a collapsed
    // block prop, add one to end offset.
    if end_node
        .children()
        .nth(end_offset as usize)
        .is_some_and(|child| child.is_collapsed_block_prop(cx.no_gc()))
    {
        end_offset += 1;
    }

    // Step 10. Repeat the following steps:
    loop {
        // Step 10.1. If end offset is the length of end node, set end offset to one plus the
        // index of end node and then set end node to its parent.
        if end_offset == end_node.len() {
            // NOTE: Not in the spec, but at the end of the editing host there is nothing to
            // delete; walking out of it would extend the selection past it, while other
            // browsers do nothing.
            if end_node.is_editing_host() {
                return true;
            }
            end_offset = 1 + end_node.index();
            end_node = end_node.GetParentNode().expect("Must always have a parent");
            continue;
        }
        // Step 10.2. Otherwise, if end node has an editable invisible child with index end
        // offset, remove it from end node.
        if let Some(child) = end_node.children().nth(end_offset as usize) &&
            child.is_editable() &&
            child.is_invisible(cx.no_gc())
        {
            child.remove_self(cx);
            continue;
        }
        // Step 10.3. Otherwise, break from this loop.
        break;
    }

    // Step 11. If the child of end node with index end offset minus one is a table, return true.
    if end_offset > 0 &&
        end_node
            .children()
            .nth(end_offset as usize - 1)
            .is_some_and(|child| child.is::<HTMLTableElement>())
    {
        return true;
    }

    // Step 12. If the child of end node with index end offset is a table:
    if end_node
        .children()
        .nth(end_offset as usize)
        .is_some_and(|child| child.is::<HTMLTableElement>())
    {
        // Step 12.1. Call collapse(end node, end offset) on the context object's selection.
        selection.collapse_current_range(&end_node, end_offset);
        // Step 12.2. Call extend(end node, end offset + 1) on the context object's selection.
        selection.extend_current_range(&end_node, end_offset + 1);
        // Step 12.3. Return true.
        return true;
    }

    // Step 13. If offset is the length of node, and the child of end node with index end offset
    // is an hr or br:
    if offset == node.len() &&
        end_node
            .children()
            .nth(end_offset as usize)
            .is_some_and(|child| child.is::<HTMLHRElement>() || child.is::<HTMLBRElement>())
    {
        // Step 13.1. Call collapse(end node, end offset) on the context object's selection.
        selection.collapse_current_range(&end_node, end_offset);
        // Step 13.2. Call extend(end node, end offset + 1) on the context object's selection.
        selection.extend_current_range(&end_node, end_offset + 1);
        // Step 13.3. Delete the selection.
        selection.delete_the_selection(
            cx,
            document,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        // Step 13.4. Call collapse(node, offset) on the context object's selection.
        selection.collapse_current_range(&node, offset);
        // Step 13.5. Return true.
        return true;
    }

    // Step 14. While end node has a child with index end offset:
    while let Some(child) = end_node.children().nth(end_offset as usize) {
        // Step 14.1. If end node's child with index end offset is editable and invisible,
        // remove it from end node.
        if child.is_editable() && child.is_invisible(cx.no_gc()) {
            child.remove_self(cx);
        } else {
            // Step 14.2. Otherwise, set end node to its child with index end offset and set end
            // offset to zero.
            end_node = child;
            end_offset = 0;
        }
    }

    // Step 15. Call collapse(node, offset) on the context object's selection.
    selection.collapse_current_range(&node, offset);

    // Step 16. Call extend(end node, end offset) on the context object's selection.
    selection.extend_current_range(&end_node, end_offset);

    // Step 17. Delete the selection.
    selection.delete_the_selection(
        cx,
        document,
        Default::default(),
        Default::default(),
        Default::default(),
    );

    // Step 18. Return true.
    true
}

/// Whether a UTF-16 code unit is a combining mark (general category M), approximated by the
/// combining mark blocks of the Basic Multilingual Plane.
fn is_combining_mark(code_unit: u16) -> bool {
    matches!(
        code_unit,
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
    )
}

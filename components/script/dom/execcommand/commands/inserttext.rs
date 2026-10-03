/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use js::context::JSContext;
use script_bindings::inheritance::Castable;

use crate::dom::bindings::codegen::Bindings::CharacterDataBinding::CharacterDataMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::characterdata::CharacterData;
use crate::dom::document::Document;
use crate::dom::node::Node;
use crate::dom::selection::Selection;
use crate::dom::text::Text;

/// <https://w3c.github.io/editing/docs/execCommand/#the-inserttext-command>, inserting the whole
/// value at once and without the autolinking and override steps: typed text goes into the text
/// node at the caret (or a new one), and the caret moves after it.
pub(crate) fn execute_insert_text_command(
    cx: &mut JSContext,
    document: &Document,
    selection: &Selection,
    value: DOMString,
) -> bool {
    // Step 1. If the active range is not collapsed, delete the selection.
    let active_range = selection
        .active_range()
        .expect("Must always have an active range");
    if !active_range.collapsed() {
        selection.delete_the_selection(
            cx,
            document,
            Default::default(),
            Default::default(),
            Default::default(),
        );
    }

    // Step 3. If value is the empty string, return false.
    if value.is_empty() {
        return false;
    }
    let inserted_length = value.str().encode_utf16().count() as u32;

    let active_range = selection
        .active_range()
        .expect("Must always have an active range");
    let node = active_range.start_container();
    let offset = active_range.start_offset();

    // Step 8. If node is a Text node, insert the value into it at offset.
    let (text_node, end_offset) = if node.is::<Text>() {
        let character_data = node.downcast::<CharacterData>().unwrap();
        if character_data.InsertData(cx, offset, value).is_err() {
            return false;
        }
        (node, offset + inserted_length)
    } else {
        // Step 9. Otherwise a new Text node holding the value goes at (node, offset), or the
        // value is appended to a Text node right before it.
        let previous = offset
            .checked_sub(1)
            .and_then(|index| node.children().nth(index as usize))
            .filter(|child| child.is::<Text>());
        match previous {
            Some(previous) => {
                let character_data = previous.downcast::<CharacterData>().unwrap();
                let previous_length = character_data.Length();
                if character_data
                    .InsertData(cx, previous_length, value)
                    .is_err()
                {
                    return false;
                }
                (previous, previous_length + inserted_length)
            },
            None => {
                let text = Text::new(cx, value, document);
                let text_node = DomRoot::upcast::<Node>(text);
                let reference = node.children().nth(offset as usize);
                if node
                    .InsertBefore(cx, &text_node, reference.as_deref())
                    .is_err()
                {
                    return false;
                }
                (text_node, inserted_length)
            },
        }
    };

    // Step 10. Canonicalize whitespace around the insertion, so a typed space at the end of a
    // line stays visible (it becomes a no-break space).
    text_node.canonicalize_whitespace(cx, end_offset, true);
    selection.collapse_current_range(&text_node, end_offset);
    true
}

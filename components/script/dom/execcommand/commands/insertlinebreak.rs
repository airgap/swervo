/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use js::context::JSContext;
use script_bindings::inheritance::Castable;

use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::codegen::Bindings::RangeBinding::RangeMethods;
use crate::dom::document::Document;
use crate::dom::element::Element;
use crate::dom::execcommand::contenteditable::node::{NodeOrString, is_allowed_child};
use crate::dom::node::Node;
use crate::dom::selection::Selection;
use crate::dom::text::Text;

/// <https://w3c.github.io/editing/docs/execCommand/#the-insertlinebreak-command>
pub(crate) fn execute_insert_line_break_command(
    cx: &mut JSContext,
    document: &Document,
    selection: &Selection,
) -> bool {
    // Step 1. Delete the selection, with strip wrappers false.
    selection.delete_the_selection(
        cx,
        document,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    let active_range = selection
        .active_range()
        .expect("Must always have an active range");
    let start_node = active_range.start_container();
    // Step 2. If the active range's start node is neither editable nor an editing host,
    // return true.
    if !start_node.is_editable_or_editing_host() {
        return true;
    }
    // Step 3. If the active range's start node is an Element, and "br" is not an allowed child
    // of it, return true.
    if start_node.is::<Element>() &&
        !is_allowed_child(
            NodeOrString::String("br".to_owned()),
            NodeOrString::Node(start_node.clone()),
        )
    {
        return true;
    }
    // Step 4. If the active range's start node is not an Element, and "br" is not an allowed
    // child of the active range's start node's parent, return true.
    if !start_node.is::<Element>() &&
        !is_allowed_child(
            NodeOrString::String("br".to_owned()),
            NodeOrString::Node(start_node.GetParentNode().expect("Must always have a parent")),
        )
    {
        return true;
    }
    if start_node.is::<Text>() {
        // Step 5. If the active range's start node is a Text node and its start offset is zero,
        // call collapse() on the context object's selection, with first argument equal to the
        // active range's start node's parent and second argument equal to the active range's
        // start node's index.
        if active_range.start_offset() == 0 {
            let parent = start_node.GetParentNode().expect("Must always have a parent");
            selection.collapse_current_range(&parent, start_node.index());
        } else if active_range.start_offset() == start_node.len() {
            // Step 6. If the active range's start node is a Text node and its start offset is the
            // length of its start node, call collapse() on the context object's selection, with
            // first argument equal to the active range's start node's parent and second argument
            // equal to one plus the active range's start node's index.
            let parent = start_node.GetParentNode().expect("Must always have a parent");
            selection.collapse_current_range(&parent, 1 + start_node.index());
        }
    }
    // Step 7. Let br be the result of calling createElement("br") on the context object.
    let br = document.create_element(cx, "br");
    // Step 8. Call insertNode(br) on the active range.
    if active_range.InsertNode(cx, br.upcast()).is_err() {
        unreachable!("Must always be able to insert");
    }
    // Step 9. Call collapse() on the context object's selection, with br's parent as the first
    // argument and one plus br's index as the second argument.
    let br = br.upcast::<Node>();
    let parent = br.GetParentNode().expect("Was just inserted");
    selection.collapse_current_range(&parent, 1 + br.index());
    // Step 10. If br is a collapsed line break, call createElement("br") on the context object
    // and let extra br be the result, then call insertNode(extra br) on the active range.
    if br.precedes_a_line_break(cx.no_gc()) {
        let extra_br = document.create_element(cx, "br");
        if active_range.InsertNode(cx, extra_br.upcast()).is_err() {
            unreachable!("Must always be able to insert");
        }
        // Inserting into the collapsed range made it contain the extra br; the caret stays
        // between the two, as in other browsers.
        selection.collapse_current_range(&parent, 1 + br.index());
    }
    // Step 11. Return true.
    true
}

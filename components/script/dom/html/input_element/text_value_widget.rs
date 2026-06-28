/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Ref;

use html5ever::{local_name, ns};
use js::context::JSContext;
use markup5ever::QualName;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::CharacterDataBinding::CharacterDataMethods;
use script_bindings::codegen::GenericBindings::NodeBinding::NodeMethods;
use script_bindings::root::Dom;
use style::selector_parser::PseudoElement;

use crate::dom::bindings::inheritance::Castable;
use crate::dom::characterdata::CharacterData;
use crate::dom::element::{CustomElementCreationMode, Element, ElementCreator};
use crate::dom::htmlinputelement::HTMLInputElement;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::text::Text;

#[derive(Default, JSTraceable, MallocSizeOf, PartialEq)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct TextValueWidget {
    shadow_tree: DomRefCell<Option<TextValueShadowTree>>,
}

impl TextValueWidget {
    /// Get the shadow tree for this [`HTMLInputElement`], if it is created and valid, otherwise
    /// recreate the shadow tree and return it.
    fn get_or_create_shadow_tree(
        &self,
        cx: &mut JSContext,
        input: &HTMLInputElement,
    ) -> Ref<'_, TextValueShadowTree> {
        {
            if let Ok(shadow_tree) = Ref::filter_map(self.shadow_tree.borrow(), |shadow_tree| {
                shadow_tree.as_ref()
            }) {
                return shadow_tree;
            }
        }

        let element = input.upcast::<Element>();
        let shadow_root = element
            .shadow_root()
            .unwrap_or_else(|| element.attach_ua_shadow_root(cx, true));
        let shadow_root = shadow_root.upcast();
        *self.shadow_tree.borrow_mut() = Some(TextValueShadowTree::new(cx, shadow_root));
        self.get_or_create_shadow_tree(cx, input)
    }

    pub(crate) fn update_shadow_tree(&self, cx: &mut JSContext, input: &HTMLInputElement) {
        self.get_or_create_shadow_tree(cx, input).update(cx, input)
    }
}

#[derive(Clone, JSTraceable, MallocSizeOf, PartialEq)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct TextValueShadowTree {
    value: Dom<Text>,
}

impl TextValueShadowTree {
    fn new(cx: &mut JSContext, shadow_root: &Node) -> Self {
        let document = shadow_root.owner_document();
        // Mirror the text-control shadow structure (see text_input_widget.rs): an inner container
        // (UA CSS: display:flex; height:stretch) holding an inner editor (margin-block:auto;
        // block-size:fit-content) that wraps the value. This vertically centers the label exactly
        // like text inputs — height:stretch fills the control and margin:auto centers the editor,
        // internal to the shadow. A bare text node (or plain wrapper) is NOT centered, because the
        // host's `inline-flex; align-items:center` does not center UA-shadow content (LYK-1299).
        let inner_container = Element::create(
            cx,
            QualName::new(None, ns!(html), local_name!("div")),
            None,
            &document,
            ElementCreator::ScriptCreated,
            CustomElementCreationMode::Asynchronous,
            None,
        );
        Node::replace_all(cx, Some(inner_container.upcast()), shadow_root);
        inner_container
            .upcast::<Node>()
            .set_implemented_pseudo_element(PseudoElement::ServoTextControlInnerContainer);

        let inner_editor = Element::create(
            cx,
            QualName::new(None, ns!(html), local_name!("div")),
            None,
            &document,
            ElementCreator::ScriptCreated,
            CustomElementCreationMode::Asynchronous,
            None,
        );
        inner_container
            .upcast::<Node>()
            .AppendChild(cx, inner_editor.upcast())
            .unwrap();
        inner_editor
            .upcast::<Node>()
            .set_implemented_pseudo_element(PseudoElement::ServoTextControlInnerEditor);

        let value = Text::new(cx, Default::default(), &document);
        inner_editor
            .upcast::<Node>()
            .AppendChild(cx, value.upcast())
            .unwrap();
        Self {
            value: value.as_traced(),
        }
    }

    fn update(&self, cx: &mut JSContext, input_element: &HTMLInputElement) {
        let character_data = self.value.upcast::<CharacterData>();
        let value = input_element.value_for_shadow_dom();
        if character_data.Data() != value {
            character_data.SetData(cx, value);
        }
    }
}

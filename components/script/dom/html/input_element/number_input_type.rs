/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use html5ever::{local_name, ns};
use js::context::JSContext;
use markup5ever::QualName;
use script_bindings::cell::DomRefCell;
use script_bindings::domstring::parse_floating_point_number;
use script_bindings::root::{Dom, DomRoot};
use style::selector_parser::PseudoElement;

use crate::dom::bindings::codegen::Bindings::DOMRectBinding::DOMRect_Binding::DOMRectMethods;
use crate::dom::bindings::codegen::Bindings::ElementBinding::ElementMethods;
use crate::dom::bindings::codegen::Bindings::MouseEventBinding::MouseEventMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::str::DOMString;
use crate::dom::element::{CustomElementCreationMode, Element, ElementCreator};
use crate::dom::htmlinputelement::text_input_widget::TextInputWidget;
use crate::dom::input_element::input_type::SpecificInputType;
use crate::dom::input_element::{HTMLInputElement, StepDirection};
use crate::dom::node::{Node, NodeTraits};
use crate::dom::types::MouseEvent;

#[derive(Default, JSTraceable, MallocSizeOf, PartialEq)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct NumberInputType {
    text_input_widget: DomRefCell<TextInputWidget>,
    /// The `::-webkit-inner-spin-button` in the UA shadow tree, created with it.
    spin_button: DomRefCell<Option<Dom<Element>>>,
}

impl NumberInputType {
    fn spin_button(&self, cx: &mut JSContext, input: &HTMLInputElement) -> DomRoot<Element> {
        if let Some(spin_button) = &*self.spin_button.borrow() {
            return spin_button.as_rooted();
        }

        let inner_container = self
            .text_input_widget
            .borrow()
            .inner_container(cx, input);
        let spin_button = Element::create(
            cx,
            QualName::new(None, ns!(html), local_name!("div")),
            None,
            &input.owner_document(),
            ElementCreator::ScriptCreated,
            CustomElementCreationMode::Asynchronous,
            None,
        );
        inner_container
            .upcast::<Node>()
            .AppendChild(cx, spin_button.upcast::<Node>())
            .unwrap();
        spin_button
            .upcast::<Node>()
            .set_implemented_pseudo_element(PseudoElement::WebkitInnerSpinButton);
        *self.spin_button.borrow_mut() = Some(spin_button.as_traced());
        spin_button
    }

    /// Which half of the spin button, if any, a mouse event is over: the upper half steps up
    /// and the lower half steps down.
    pub(crate) fn spin_button_direction(
        &self,
        cx: &mut JSContext,
        input: &HTMLInputElement,
        mouse_event: &MouseEvent,
    ) -> Option<StepDirection> {
        let rect = self.spin_button(cx, input).GetBoundingClientRect(cx);
        let (x, y) = (f64::from(mouse_event.ClientX()), f64::from(mouse_event.ClientY()));
        if rect.Width() <= 0.0 ||
            x < rect.X() ||
            x >= rect.X() + rect.Width() ||
            y < rect.Y() ||
            y >= rect.Y() + rect.Height()
        {
            return None;
        }
        if y < rect.Y() + rect.Height() / 2.0 {
            Some(StepDirection::Up)
        } else {
            Some(StepDirection::Down)
        }
    }
}

impl SpecificInputType for NumberInputType {
    fn sanitize_value(&self, _input: &HTMLInputElement, value: &mut DOMString) {
        if !value.is_valid_floating_point_number_string() {
            value.clear();
        }
        // Spec says that user agent "may" round the value
        // when it's suffering a step mismatch, but WPT tests
        // want it unrounded, and this matches other browser
        // behavior (typing an unrounded number into an
        // integer field box and pressing enter generally keeps
        // the number intact but makes the input box :invalid)
    }

    /// <https://html.spec.whatwg.org/multipage/#number-state-(type=number):concept-input-value-string-number>
    fn convert_string_to_number(&self, input: &str) -> Option<f64> {
        parse_floating_point_number(input)
    }

    /// <https://html.spec.whatwg.org/multipage/#number-state-(type=number):concept-input-value-string-number>
    fn convert_number_to_string(&self, input: f64) -> Option<DOMString> {
        let mut value = DOMString::from(input.to_string());
        value.set_best_representation_of_the_floating_point_number();
        Some(value)
    }

    /// <https://html.spec.whatwg.org/multipage/#number-state-(type=number):suffering-from-bad-input>
    fn suffers_from_bad_input(&self, value: &DOMString) -> bool {
        !value.is_valid_floating_point_number_string()
    }

    fn update_shadow_tree(&self, cx: &mut JSContext, input: &HTMLInputElement) {
        self.text_input_widget
            .borrow()
            .update_shadow_tree(cx, input);
        self.spin_button(cx, input);
    }

    fn update_placeholder_contents(&self, cx: &mut JSContext, input: &HTMLInputElement) {
        self.text_input_widget
            .borrow()
            .update_placeholder_contents(cx, input)
    }
}

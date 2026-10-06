/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::fmt;

use bitflags::bitflags;
use dom_struct::dom_struct;
use itertools::Itertools;
use js::context::JSContext;
use script_bindings::cell::{DomRefCell, Ref};
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::ElementInternalsBinding::ValidityStateFlags;
use crate::dom::bindings::codegen::Bindings::ValidityStateBinding::ValidityStateMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::element::Element;
use crate::dom::html::htmlfieldsetelement::HTMLFieldSetElement;
use crate::dom::html::htmlformelement::FormControlElementHelpers;
use crate::dom::html::input_element::HTMLInputElement;
use crate::dom::node::Node;
use crate::dom::window::Window;

/// <https://html.spec.whatwg.org/multipage/#validity-states>
#[derive(Clone, Copy, JSTraceable, MallocSizeOf)]
pub(crate) struct ValidationFlags(u32);

bitflags! {
    impl ValidationFlags: u32 {
        const VALUE_MISSING    = 0b0000000001;
        const TYPE_MISMATCH    = 0b0000000010;
        const PATTERN_MISMATCH = 0b0000000100;
        const TOO_LONG         = 0b0000001000;
        const TOO_SHORT        = 0b0000010000;
        const RANGE_UNDERFLOW  = 0b0000100000;
        const RANGE_OVERFLOW   = 0b0001000000;
        const STEP_MISMATCH    = 0b0010000000;
        const BAD_INPUT        = 0b0100000000;
        const CUSTOM_ERROR     = 0b1000000000;
    }
}

impl fmt::Display for ValidationFlags {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        let flag_to_message = [
            (ValidationFlags::VALUE_MISSING, "Value missing"),
            (ValidationFlags::TYPE_MISMATCH, "Type mismatch"),
            (ValidationFlags::PATTERN_MISMATCH, "Pattern mismatch"),
            (ValidationFlags::TOO_LONG, "Too long"),
            (ValidationFlags::TOO_SHORT, "Too short"),
            (ValidationFlags::RANGE_UNDERFLOW, "Range underflow"),
            (ValidationFlags::RANGE_OVERFLOW, "Range overflow"),
            (ValidationFlags::STEP_MISMATCH, "Step mismatch"),
            (ValidationFlags::BAD_INPUT, "Bad input"),
            (ValidationFlags::CUSTOM_ERROR, "Custom error"),
        ];

        flag_to_message
            .iter()
            .filter_map(|&(flag, flag_str)| {
                if self.contains(flag) {
                    Some(flag_str)
                } else {
                    None
                }
            })
            .format(", ")
            .fmt(formatter)
    }
}

// https://html.spec.whatwg.org/multipage/#validitystate
#[dom_struct]
pub(crate) struct ValidityState {
    reflector_: Reflector,
    element: Dom<Element>,
    custom_error_message: DomRefCell<DOMString>,
    invalid_flags: Cell<ValidationFlags>,
    /// <https://html.spec.whatwg.org/multipage/#user-validity>
    user_validity: Cell<bool>,
}

impl ValidityState {
    fn new_inherited(element: &Element) -> ValidityState {
        ValidityState {
            reflector_: Reflector::new(),
            element: Dom::from_ref(element),
            custom_error_message: DomRefCell::new(DOMString::new()),
            invalid_flags: Cell::new(ValidationFlags::empty()),
            user_validity: Cell::new(false),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        element: &Element,
    ) -> DomRoot<ValidityState> {
        reflect_dom_object_with_cx(Box::new(ValidityState::new_inherited(element)), window, cx)
    }

    // https://html.spec.whatwg.org/multipage/#custom-validity-error-message
    pub(crate) fn custom_error_message(&self) -> Ref<'_, DOMString> {
        self.custom_error_message.borrow()
    }

    // https://html.spec.whatwg.org/multipage/#custom-validity-error-message
    pub(crate) fn set_custom_error_message(&self, cx: &mut JSContext, error: DOMString) {
        *self.custom_error_message.borrow_mut() = error;
        self.perform_validation_and_update(cx, ValidationFlags::CUSTOM_ERROR);
    }

    /// Given a set of [ValidationFlags], recalculate their value by performing
    /// validation on this [ValidityState]'s associated element. Additionally,
    /// if [ValidationFlags::CUSTOM_ERROR] is in `update_flags` and a custom
    /// error has been set on this [ValidityState], the state will be updated
    /// to reflect the existance of a custom error.
    pub(crate) fn perform_validation_and_update(
        &self,
        cx: &mut JSContext,
        update_flags: ValidationFlags,
    ) {
        let mut invalid_flags = self.invalid_flags.get();
        invalid_flags.remove(update_flags);

        if let Some(validatable) = self.element.as_maybe_validatable() {
            let new_flags = validatable.perform_validation(cx, update_flags);
            invalid_flags.insert(new_flags);
        }

        // https://html.spec.whatwg.org/multipage/#suffering-from-a-custom-error
        if update_flags.contains(ValidationFlags::CUSTOM_ERROR) &&
            !self.custom_error_message().is_empty()
        {
            invalid_flags.insert(ValidationFlags::CUSTOM_ERROR);
        }

        self.invalid_flags.set(invalid_flags);
        self.update_pseudo_classes(cx);
    }

    pub(crate) fn update_invalid_flags(&self, update_flags: ValidationFlags) {
        self.invalid_flags.set(update_flags);
    }

    pub(crate) fn invalid_flags(&self) -> ValidationFlags {
        self.invalid_flags.get()
    }

    /// <https://html.spec.whatwg.org/multipage/#user-validity>
    pub(crate) fn set_user_validity(&self, cx: &mut JSContext, user_validity: bool) {
        if self.user_validity.replace(user_validity) != user_validity {
            self.update_pseudo_classes(cx);
        }
    }

    pub(crate) fn update_pseudo_classes(&self, cx: &mut JSContext) {
        let invalid_flags = self.invalid_flags.get();
        let is_candidate = self.element.is_instance_validatable();
        let is_valid = invalid_flags.is_empty();
        self.element
            .set_state(ElementState::VALID, is_candidate && is_valid);
        self.element
            .set_state(ElementState::INVALID, is_candidate && !is_valid);

        // <https://html.spec.whatwg.org/multipage/#selector-user-valid>
        // <https://html.spec.whatwg.org/multipage/#selector-user-invalid>
        let user_validity = is_candidate && self.user_validity.get();
        self.element
            .set_state(ElementState::USER_VALID, user_validity && is_valid);
        self.element
            .set_state(ElementState::USER_INVALID, user_validity && !is_valid);

        // <https://html.spec.whatwg.org/multipage/#selector-in-range>
        // <https://html.spec.whatwg.org/multipage/#selector-out-of-range>
        if let Some(input) = self.element.downcast::<HTMLInputElement>() {
            let has_range_limitations = is_candidate && input.has_range_limitations();
            let out_of_range = invalid_flags
                .intersects(ValidationFlags::RANGE_UNDERFLOW | ValidationFlags::RANGE_OVERFLOW);
            self.element.set_state(
                ElementState::INRANGE,
                has_range_limitations && !out_of_range,
            );
            self.element.set_state(
                ElementState::OUTOFRANGE,
                has_range_limitations && out_of_range,
            );
        }

        if let Some(form_control) = self.element.as_maybe_form_control() &&
            let Some(form_owner) = form_control.form_owner()
        {
            form_owner.update_validity(cx);
        }

        if let Some(fieldset) = self
            .element
            .upcast::<Node>()
            .ancestors()
            .find_map(DomRoot::downcast::<HTMLFieldSetElement>)
        {
            fieldset.update_validity(cx);
        }
    }
}

impl ValidityStateMethods<crate::DomTypeHolder> for ValidityState {
    /// <https://html.spec.whatwg.org/multipage/#dom-validitystate-valuemissing>
    fn ValueMissing(&self) -> bool {
        self.invalid_flags()
            .contains(ValidationFlags::VALUE_MISSING)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-validitystate-typemismatch>
    fn TypeMismatch(&self) -> bool {
        self.invalid_flags()
            .contains(ValidationFlags::TYPE_MISMATCH)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-validitystate-patternmismatch>
    fn PatternMismatch(&self) -> bool {
        self.invalid_flags()
            .contains(ValidationFlags::PATTERN_MISMATCH)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-validitystate-toolong>
    fn TooLong(&self) -> bool {
        self.invalid_flags().contains(ValidationFlags::TOO_LONG)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-validitystate-tooshort>
    fn TooShort(&self) -> bool {
        self.invalid_flags().contains(ValidationFlags::TOO_SHORT)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-validitystate-rangeunderflow>
    fn RangeUnderflow(&self) -> bool {
        self.invalid_flags()
            .contains(ValidationFlags::RANGE_UNDERFLOW)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-validitystate-rangeoverflow>
    fn RangeOverflow(&self) -> bool {
        self.invalid_flags()
            .contains(ValidationFlags::RANGE_OVERFLOW)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-validitystate-stepmismatch>
    fn StepMismatch(&self) -> bool {
        self.invalid_flags()
            .contains(ValidationFlags::STEP_MISMATCH)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-validitystate-badinput>
    fn BadInput(&self) -> bool {
        self.invalid_flags().contains(ValidationFlags::BAD_INPUT)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-validitystate-customerror>
    fn CustomError(&self) -> bool {
        self.invalid_flags().contains(ValidationFlags::CUSTOM_ERROR)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-validitystate-valid>
    fn Valid(&self) -> bool {
        self.invalid_flags().is_empty()
    }
}

impl From<&ValidityStateFlags> for ValidationFlags {
    fn from(flags: &ValidityStateFlags) -> Self {
        let mut bits = ValidationFlags::empty();
        if flags.valueMissing {
            bits |= ValidationFlags::VALUE_MISSING;
        }
        if flags.typeMismatch {
            bits |= ValidationFlags::TYPE_MISMATCH;
        }
        if flags.patternMismatch {
            bits |= ValidationFlags::PATTERN_MISMATCH;
        }
        if flags.tooLong {
            bits |= ValidationFlags::TOO_LONG;
        }
        if flags.tooShort {
            bits |= ValidationFlags::TOO_SHORT;
        }
        if flags.rangeUnderflow {
            bits |= ValidationFlags::RANGE_UNDERFLOW;
        }
        if flags.rangeOverflow {
            bits |= ValidationFlags::RANGE_OVERFLOW;
        }
        if flags.stepMismatch {
            bits |= ValidationFlags::STEP_MISMATCH;
        }
        if flags.badInput {
            bits |= ValidationFlags::BAD_INPUT;
        }
        if flags.customError {
            bits |= ValidationFlags::CUSTOM_ERROR;
        }
        bits
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use std::borrow::Borrow;
use std::cell::Cell;

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name, ns};
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::HTMLElementBinding::HTMLElementMethods;
use script_bindings::error::{Error, ErrorResult};
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::HTMLDialogElementBinding::HTMLDialogElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::element::{AttributeMutation, Element};
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::html::htmlelement::HTMLElement;
use crate::dom::htmlbuttonelement::{CommandState, HTMLButtonElement};
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{BindContext, Node, NodeTraits, UnbindContext};
use crate::dom::toggleevent::ToggleEvent;

/// <https://html.spec.whatwg.org/multipage/#attr-dialog-closedby>
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ClosedByState {
    Any,
    CloseRequest,
    None,
}

#[dom_struct]
pub(crate) struct HTMLDialogElement {
    htmlelement: HTMLElement,
    return_value: DomRefCell<DOMString>,
    /// <https://html.spec.whatwg.org/multipage/#enable-close-watcher-for-requestclose()>
    enable_close_watcher_for_request_close: Cell<bool>,
    /// <https://html.spec.whatwg.org/multipage/#dialog-request-close-return-value>
    request_close_return_value: DomRefCell<Option<DOMString>>,
    /// <https://html.spec.whatwg.org/multipage/#dialog-request-close-source-element>
    request_close_source_element: MutNullableDom<Element>,
    /// <https://html.spec.whatwg.org/multipage/#close-watcher-is-running-cancel-action>
    is_running_cancel_action: Cell<bool>,
}

impl HTMLDialogElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> HTMLDialogElement {
        HTMLDialogElement {
            htmlelement: HTMLElement::new_inherited(local_name, prefix, document),
            return_value: DomRefCell::new(DOMString::new()),
            enable_close_watcher_for_request_close: Cell::new(false),
            request_close_return_value: DomRefCell::new(None),
            request_close_source_element: Default::default(),
            is_running_cancel_action: Cell::new(false),
        }
    }

    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<HTMLDialogElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(HTMLDialogElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }

    /// <https://html.spec.whatwg.org/multipage/#show-a-modal-dialog>
    pub fn show_a_modal(
        &self,
        cx: &mut js::context::JSContext,
        source: Option<DomRoot<Element>>,
    ) -> ErrorResult {
        let subject = self.upcast::<Element>();
        // Step 1. If subject has an open attribute and is modal of subject is true, then return.
        if subject.has_attribute(&local_name!("open")) &&
            subject.state().contains(ElementState::MODAL)
        {
            return Ok(());
        }

        // Step 2. If subject has an open attribute, then throw an "InvalidStateError" DOMException.
        if subject.has_attribute(&local_name!("open")) {
            return Err(Error::InvalidState(Some(
                "Cannot call showModal() on an already open dialog.".into(),
            )));
        }

        // Step 3. If subject's node document is not fully active, then throw an "InvalidStateError" DOMException.
        if !subject.owner_document().is_fully_active() {
            return Err(Error::InvalidState(Some(
                "Cannot call showModal() on a dialog whose document is not fully active.".into(),
            )));
        }

        // Step 4. If subject is not connected, then throw an "InvalidStateError" DOMException.
        if !subject.is_connected() {
            return Err(Error::InvalidState(Some(
                "Cannot call showModal() on a dialog that is not connected.".into(),
            )));
        }

        // Step 5. If subject is in the popover showing state, then throw an "InvalidStateError" DOMException.
        if self.upcast::<HTMLElement>().is_popover_showing() {
            return Err(Error::InvalidState(Some(
                "Cannot call showModal() on a dialog that is showing as a popover.".into(),
            )));
        }

        // Step 6. If the result of firing an event named beforetoggle, using ToggleEvent, with the cancelable attribute initialized to true, the oldState attribute initialized to "closed", the newState attribute initialized to "open", and the source attribute initialized to source at subject is false, then return.
        let event = ToggleEvent::new(
            cx,
            &self.owner_window(),
            atom!("beforetoggle"),
            EventBubbles::DoesNotBubble,
            EventCancelable::Cancelable,
            DOMString::from("closed"),
            DOMString::from("open"),
            source.borrow().clone(),
        );
        let event = event.upcast::<Event>();
        if !event.fire(cx, self.upcast::<EventTarget>()) {
            return Ok(());
        }

        // Step 7. If subject has an open attribute, then return.
        if subject.has_attribute(&local_name!("open")) {
            return Ok(());
        }

        // Step 8. If subject is not connected, then return.
        if !subject.is_connected() {
            return Ok(());
        }

        // Step 9. If subject is in the popover showing state, then return.
        if self.upcast::<HTMLElement>().is_popover_showing() {
            return Ok(());
        }

        // Step 10. Queue a dialog toggle event task given subject, "closed", "open", and source.
        self.queue_dialog_toggle_event_task("closed", "open", source);

        // Step 11. Add an open attribute to subject, whose value is the empty string.
        subject.set_bool_attribute(cx, &local_name!("open"), true);
        subject.set_open_state(true);

        // Step 12. Assert: subject's close watcher is not null.
        let document = self.owner_document();
        debug_assert!(
            document
                .top_layer()
                .is_close_watcher_active(self.upcast::<HTMLElement>())
        );

        // Step 13. Set is modal of subject to true.
        self.upcast::<Element>().set_modal_state(true);

        // Step 14. Set subject's node document to be blocked by the modal dialog subject.
        // Being blocked is derived from the topmost modal dialog in the top layer.

        // Step 15. If subject's node document's top layer does not already contain subject, then add an element to the top layer given subject.
        if !document.top_layer().contains(subject) {
            document.top_layer().add(subject);
        }

        // Step 16. Set subject's previously focused element to the focused element.
        self.upcast::<HTMLElement>().set_previously_focused_element(
            self.owner_document()
                .focus_handler()
                .focused_area()
                .element(),
        );

        // Step 17. Let document be subject's node document.
        // Step 18. Let hideUntil be the result of running topmost popover ancestor given subject, document's showing hint popover list, null, and false.
        // Step 19. If hideUntil is null, then set hideUntil to the result of running topmost popover ancestor given subject, document's showing auto popover list, null, and false.
        // Step 20. If hideUntil is null, then set hideUntil to document.
        // Step 21. Run hide all popovers until given hideUntil, false, and true.
        self.hide_popovers_not_containing_self(cx);

        // Step 22. Run the dialog focusing steps given subject.
        self.run_dialog_focusing_steps(cx);
        Ok(())
    }

    /// <https://html.spec.whatwg.org/multipage/#close-the-dialog>
    pub fn close_the_dialog(
        &self,
        cx: &mut js::context::JSContext,
        result: Option<DOMString>,
        source: Option<DomRoot<Element>>,
    ) {
        let subject = self.upcast::<Element>();
        // Step 1. If subject does not have an open attribute, then return.
        if !subject.has_attribute(&local_name!("open")) {
            return;
        }

        // Step 2. Fire an event named beforetoggle, using ToggleEvent, with the oldState attribute initialized to "open", the newState attribute initialized to "closed", and the source attribute initialized to source at subject.
        let event = ToggleEvent::new(
            cx,
            &self.owner_window(),
            atom!("beforetoggle"),
            EventBubbles::DoesNotBubble,
            EventCancelable::NotCancelable,
            DOMString::from("open"),
            DOMString::from("closed"),
            source.borrow().clone(),
        );
        let event = event.upcast::<Event>();
        event.fire(cx, self.upcast::<EventTarget>());

        // Step 3. If subject does not have an open attribute, then return.
        if !subject.has_attribute(&local_name!("open")) {
            return;
        }

        // Step 4. Queue a dialog toggle event task given subject, "open", "closed", and source.
        self.queue_dialog_toggle_event_task("open", "closed", source);

        // Step 5. Remove subject's open attribute.
        subject.remove_attribute(cx, &ns!(), &local_name!("open"));
        subject.set_open_state(false);

        // Step 6. If is modal of subject is true, then request an element to be removed from the top layer given subject.
        // Step 7. Let wasModal be the value of subject's is modal flag.
        let was_modal = subject.state().contains(ElementState::MODAL);
        if was_modal {
            subject.owner_document().top_layer().remove(subject);
        }

        // Step 8. Set is modal of subject to false.
        self.upcast::<Element>().set_modal_state(false);

        // Step 9. If result is not null, then set subject's returnValue attribute to result.
        if let Some(new_value) = result {
            *self.return_value.borrow_mut() = new_value;
        }

        // Step 10. Set subject's request close return value to null.
        *self.request_close_return_value.borrow_mut() = None;

        // Step 11. Set subject's request close source element to null.
        self.request_close_source_element.set(None);

        // Step 12. If subject's previously focused element is not null, then:
        if let Some(element) = self.upcast::<HTMLElement>().previously_focused_element() {
            // Step 12.1. Let element be subject's previously focused element.
            // Step 12.2. Set subject's previously focused element to null.
            self.upcast::<HTMLElement>()
                .set_previously_focused_element(None);

            // Step 12.3. If subject's node document's focused area of the document's DOM anchor is
            // a shadow-including inclusive descendant of subject, or wasModal is true, then run the
            // focusing steps for element; the viewport should not be scrolled by doing this step.
            let subject_node = subject.upcast::<Node>();
            let document = subject.owner_document();
            if document
                .focus_handler()
                .focused_area()
                .dom_anchor(&document)
                .traverse_preorder(ShadowIncluding::Yes)
                .any(|node| &*node == subject_node) ||
                was_modal
            {
                element.upcast::<Node>().run_the_focusing_steps(cx, None);
            }
        }

        // Step 13. Queue an element task on the user interaction task source given the subject element to fire an event named close at subject.
        let target = self.upcast::<EventTarget>();
        self.owner_global()
            .task_manager()
            .user_interaction_task_source()
            .queue_simple_event(target, atom!("close"));
    }

    /// Steps 17 to 21 of showing a modal dialog and steps 8 to 12 of show(): hide every popover
    /// that is not an ancestor of this dialog.
    fn hide_popovers_not_containing_self(&self, cx: &mut JSContext) {
        let document = self.owner_document();
        let hide_until = HTMLElement::topmost_popover_ancestor(
            self.upcast(),
            &document.top_layer().showing_auto_popover_list(),
            None,
            false,
        );
        HTMLElement::hide_all_popovers_until(cx, &document, hide_until.as_deref(), false, true);
    }

    /// <https://html.spec.whatwg.org/multipage/#computed-closed-by-state>
    pub(crate) fn computed_closed_by_state(&self) -> ClosedByState {
        let element = self.upcast::<Element>();
        // > 1. If dialog's closedby attribute is in the Auto state, then: if dialog's is modal is
        // >    true, return Close Request; otherwise return None.
        // > 2. Return the state of dialog's closedby attribute.
        match element
            .get_attribute_string_value(&local_name!("closedby"))
            .map(|value| value.to_ascii_lowercase())
            .as_deref()
        {
            Some("any") => ClosedByState::Any,
            Some("closerequest") => ClosedByState::CloseRequest,
            Some("none") => ClosedByState::None,
            _ if element.state().contains(ElementState::MODAL) => ClosedByState::CloseRequest,
            _ => ClosedByState::None,
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#dialog-setup-steps>
    fn run_dialog_setup_steps(&self) {
        // > 1. Assert: subject has an open attribute.
        // > 2. Assert: subject is connected.
        // > 3. Assert: subject's node document's open dialogs list does not contain subject.
        // > 4. Add subject to subject's node document's open dialogs list.
        let document = self.owner_document();
        document.top_layer().add_open_dialog(self);
        // > 5. Set subject's close watcher to the result of establishing a close watcher given
        // >    subject's relevant global object, with cancelAction, closeAction and
        // >    getEnabledState as defined by the close watcher methods on this element.
        document
            .top_layer()
            .establish_close_watcher(self.upcast::<HTMLElement>());
    }

    /// <https://html.spec.whatwg.org/multipage/#dialog-cleanup-steps>
    fn run_dialog_cleanup_steps(&self) {
        let document = self.owner_document();
        // > 1. Remove subject from subject's node document's open dialogs list.
        document.top_layer().remove_open_dialog(self);
        // > 2. If subject's close watcher is not null, then destroy subject's close watcher and
        // >    set subject's close watcher to null.
        document
            .top_layer()
            .destroy_close_watcher(self.upcast::<HTMLElement>());
    }

    /// The getEnabledState of the dialog's close watcher, set up in the
    /// <https://html.spec.whatwg.org/multipage/#dialog-setup-steps>.
    pub(crate) fn close_watcher_enabled_state(&self) -> bool {
        // > 1. If dialog's enable close watcher for request close is true, then return true.
        // > 2. If dialog's computed closed-by state is not None, then return true.
        // > 3. Return false.
        self.enable_close_watcher_for_request_close.get() ||
            self.computed_closed_by_state() != ClosedByState::None
    }

    pub(crate) fn is_running_cancel_action(&self) -> bool {
        self.is_running_cancel_action.get()
    }

    /// The cancelAction of the dialog's close watcher, run as steps 7 to 9 of
    /// <https://html.spec.whatwg.org/multipage/#close-watcher-request-close>.
    pub(crate) fn run_close_watcher_cancel_action(
        &self,
        cx: &mut JSContext,
        can_prevent_close: bool,
    ) -> bool {
        self.is_running_cancel_action.set(true);
        // > Return the result of firing an event named cancel at dialog, with the cancelable
        // > attribute initialized to canPreventClose.
        let event = Event::new(
            cx,
            self.owner_window().upcast(),
            atom!("cancel"),
            EventBubbles::DoesNotBubble,
            if can_prevent_close {
                EventCancelable::Cancelable
            } else {
                EventCancelable::NotCancelable
            },
        );
        let should_continue = event.fire(cx, self.upcast::<EventTarget>());
        self.is_running_cancel_action.set(false);
        should_continue
    }

    /// The closeAction of the dialog's close watcher.
    pub(crate) fn run_close_watcher_close_action(&self, cx: &mut JSContext) {
        // > Close the dialog given dialog, dialog's request close return value, and dialog's
        // > request close source element.
        let result = self.request_close_return_value.borrow().clone();
        let source = self.request_close_source_element.get();
        self.close_the_dialog(cx, result, source);
    }

    /// <https://html.spec.whatwg.org/multipage/#dialog-request-close>
    fn request_close(
        &self,
        cx: &mut JSContext,
        return_value: Option<DOMString>,
        source: Option<DomRoot<Element>>,
    ) {
        let subject = self.upcast::<Element>();
        // > 1. If subject does not have an open attribute, then return.
        if !subject.has_attribute(&local_name!("open")) {
            return;
        }
        // > 2. If subject is not connected or subject's node document is not fully active, then
        // >    return.
        let document = self.owner_document();
        if !subject.is_connected() || !document.is_fully_active() {
            return;
        }
        // > 3. Assert: subject's close watcher is not null.
        debug_assert!(
            document
                .top_layer()
                .is_close_watcher_active(self.upcast::<HTMLElement>())
        );
        // > 4. Set subject's enable close watcher for request close to true.
        self.enable_close_watcher_for_request_close.set(true);
        // > 5. Set subject's request close return value to returnValue.
        *self.request_close_return_value.borrow_mut() = return_value;
        // > 6. Set subject's request close source element to source.
        self.request_close_source_element.set(source.as_deref());
        // > 7. Request to close subject's close watcher with false.
        document
            .top_layer()
            .request_to_close(cx, self.upcast::<HTMLElement>(), false);
        // > 8. Set subject's enable close watcher for request close to false.
        self.enable_close_watcher_for_request_close.set(false);
    }

    /// <https://html.spec.whatwg.org/multipage/#queue-a-dialog-toggle-event-task>
    pub fn queue_dialog_toggle_event_task(
        &self,
        old_state: &str,
        new_state: &str,
        source: Option<DomRoot<Element>>,
    ) {
        // TODO: Step 1. If element's dialog toggle task tracker is not null, then:
        // TODO: Step 1.1. Set oldState to element's dialog toggle task tracker's old state.
        // TODO: Step 1.2. Remove element's dialog toggle task tracker's task from its task queue.
        // TODO: Step 1.3. Set element's dialog toggle task tracker to null.
        // Step 2. Queue an element task given the DOM manipulation task source and element to run the following steps:
        let this = Trusted::new(self);
        let old_state = old_state.to_string();
        let new_state = new_state.to_string();

        let trusted_source = source
            .as_ref()
            .map(|el| Trusted::new(el.upcast::<EventTarget>()));

        self.owner_global()
            .task_manager()
            .dom_manipulation_task_source()
            .queue(task!(fire_toggle_event: move |cx| {
                let this = this.root();

                let source = trusted_source.as_ref().map(|s| {
                    DomRoot::from_ref(s.root().downcast::<Element>().unwrap())
                });

                // Step 2.1. Fire an event named toggle at element, using ToggleEvent, with the oldState attribute initialized to oldState, the newState attribute initialized to newState, and the source attribute initialized to source.
                let event = ToggleEvent::new(
                    cx,
                    &this.owner_window(),
                    atom!("toggle"),
                    EventBubbles::DoesNotBubble,
                    EventCancelable::NotCancelable,
                    DOMString::from(old_state),
                    DOMString::from(new_state),
                    source,
                );
                let event = event.upcast::<Event>();
                event.fire(cx, this.upcast::<EventTarget>());

                // TODO: Step 2.2. Set element's dialog toggle task tracker to null.
            }));
        // TODO: Step 3. Set element's dialog toggle task tracker to a struct with task set to the just-queued task and old state set to oldState.
    }

    /// <https://html.spec.whatwg.org/multipage/#dialog-focusing-steps>
    pub(crate) fn run_dialog_focusing_steps(&self, cx: &mut JSContext) {
        // TODO: Step 1. If the allow focus steps given subject's node document return false, then return.

        // Step 2. Let control be null.
        let mut control = None;

        // Step 3. If subject has the autofocus attribute, then set control to subject.
        if self.upcast::<HTMLElement>().Autofocus() {
            control = self.upcast::<Node>().get_the_focusable_area();
        }

        // Step 4. If control is null, then set control to the focus delegate of subject.
        if control.is_none() {
            control = self.upcast::<Node>().focus_delegate();
        }

        // Step 5. If control is null, then set control to subject.
        if control.is_none() {
            control = self.upcast::<Node>().get_the_focusable_area();
        }

        // Step 6. Run the focusing steps for control.
        // FIXME: Use the focusing step once they support a focusable area as an argument
        if let Some(control) = control {
            let document = self.owner_document();
            document.focus_handler().focus(cx, control);
        }

        // TODO: Step 7. Let topDocument be control's node navigable's top-level traversable's active document.
        // TODO: Step 8. If control's node document's origin is not the same as the origin of topDocument, then return.
        // TODO: Step 9. Empty topDocument's autofocus candidates.
        // TODO: Step 10. Set topDocument's autofocus processed flag to true.
    }
}

impl HTMLDialogElementMethods<crate::DomTypeHolder> for HTMLDialogElement {
    // https://html.spec.whatwg.org/multipage/#dom-dialog-open
    make_bool_getter!(Open, "open");

    // https://html.spec.whatwg.org/multipage/#dom-dialog-open
    make_bool_setter!(SetOpen, "open");

    /// <https://html.spec.whatwg.org/multipage/#dom-dialog-returnvalue>
    fn ReturnValue(&self) -> DOMString {
        let return_value = self.return_value.borrow();
        return_value.clone()
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-dialog-returnvalue>
    fn SetReturnValue(&self, _cx: &mut JSContext, return_value: DOMString) {
        *self.return_value.borrow_mut() = return_value;
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-dialog-show>
    fn Show(&self, cx: &mut js::context::JSContext) -> ErrorResult {
        let element = self.upcast::<Element>();
        // Step 1. If this has an open attribute and is modal of this is false, then return.
        if element.has_attribute(&local_name!("open")) &&
            !element.state().contains(ElementState::MODAL)
        {
            return Ok(());
        }

        // Step 2. If this has an open attribute, then throw an "InvalidStateError" DOMException.
        if element.has_attribute(&local_name!("open")) {
            return Err(Error::InvalidState(Some(
                "Cannot call show() on an already open dialog.".into(),
            )));
        }

        // Chrome throws for a dialog showing as a popover too, like showModal() does in step 5.
        if self.upcast::<HTMLElement>().is_popover_showing() {
            return Err(Error::InvalidState(Some(
                "Cannot call show() on a dialog that is showing as a popover.".into(),
            )));
        }

        // Step 3. If the result of firing an event named beforetoggle, using ToggleEvent, with the cancelable attribute initialized to true, the oldState attribute initialized to "closed", and the newState attribute initialized to "open" at this is false, then return.
        let event = ToggleEvent::new(
            cx,
            &self.owner_window(),
            atom!("beforetoggle"),
            EventBubbles::DoesNotBubble,
            EventCancelable::Cancelable,
            DOMString::from("closed"),
            DOMString::from("open"),
            None,
        );
        let event = event.upcast::<Event>();
        if !event.fire(cx, self.upcast::<EventTarget>()) {
            return Ok(());
        }

        // Step 4. If this has an open attribute, then return.
        if element.has_attribute(&local_name!("open")) {
            return Ok(());
        }

        // Step 5. Queue a dialog toggle event task given this, "closed", "open", and null.
        self.queue_dialog_toggle_event_task("closed", "open", None);

        // Step 6. Add an open attribute to this, whose value is the empty string.
        element.set_bool_attribute(cx, &local_name!("open"), true);
        element.set_open_state(true);

        // Step 7. Set this's previously focused element to the focused element.
        self.upcast::<HTMLElement>().set_previously_focused_element(
            self.owner_document()
                .focus_handler()
                .focused_area()
                .element(),
        );

        // Step 8. Let document be this's node document.
        // Step 9. Let hideUntil be the result of running topmost popover ancestor given this, document's showing hint popover list, null, and false.
        // Step 10. If hideUntil is null, then set hideUntil to the result of running topmost popover ancestor given this, document's showing auto popover list, null, and false.
        // Step 11. If hideUntil is null, then set hideUntil to document.
        // Step 12. Run hide all popovers until given hideUntil, false, and true.
        self.hide_popovers_not_containing_self(cx);

        // Step 13. Run the dialog focusing steps given this.
        self.run_dialog_focusing_steps(cx);
        Ok(())
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-dialog-showmodal>
    fn ShowModal(&self, cx: &mut js::context::JSContext) -> ErrorResult {
        // The showModal() method steps are to show a modal dialog given this and null.
        self.show_a_modal(cx, None)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-dialog-close>
    fn Close(&self, cx: &mut js::context::JSContext, return_value: Option<DOMString>) {
        // Step 1. If returnValue is not given, then set it to null.
        // Step 2. Close the dialog this with returnValue and null.
        self.close_the_dialog(cx, return_value, None);
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-dialog-requestclose>
    fn RequestClose(&self, cx: &mut JSContext, return_value: Option<DOMString>) {
        // Step 1. If returnValue is not given, then set it to null.
        // Step 2. Request to close the dialog this with returnValue and null.
        self.request_close(cx, return_value, None);
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-dialog-closedby>
    fn ClosedBy(&self) -> DOMString {
        // > The closedBy getter steps are to return the keyword corresponding to the computed
        // > closed-by state given this.
        DOMString::from(match self.computed_closed_by_state() {
            ClosedByState::Any => "any",
            ClosedByState::CloseRequest => "closerequest",
            ClosedByState::None => "none",
        })
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-dialog-closedby>
    fn SetClosedBy(&self, cx: &mut JSContext, value: DOMString) {
        self.upcast::<Element>()
            .set_string_attribute(cx, &local_name!("closedby"), value);
    }
}

impl VirtualMethods for HTMLDialogElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<HTMLElement>() as &dyn VirtualMethods)
    }

    /// <https://html.spec.whatwg.org/multipage/#the-dialog-element:concept-element-attributes-change-ext>
    fn attribute_mutated(
        &self,
        cx: &mut JSContext,
        attr: AttrRef<'_>,
        mutation: AttributeMutation,
    ) {
        self.super_type()
            .unwrap()
            .attribute_mutated(cx, attr, mutation);
        // > 2. If localName is not open, then return.
        if attr.local_name() != &local_name!("open") || !self.upcast::<Node>().is_connected() {
            return;
        }
        match mutation {
            // > 4. If value is null and oldValue is not null, then run the dialog cleanup steps
            // >    given element.
            AttributeMutation::Removed => self.run_dialog_cleanup_steps(),
            // > 5. If value is not null and oldValue is null, then run the dialog setup steps
            // >    given element.
            AttributeMutation::Set(None, _) => self.run_dialog_setup_steps(),
            AttributeMutation::Set(Some(_), _) => {},
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#the-dialog-element:html-element-insertion-steps>
    fn bind_to_tree(&self, cx: &mut JSContext, context: &BindContext) {
        if let Some(super_type) = self.super_type() {
            super_type.bind_to_tree(cx, context);
        }
        // > 1. If insertedNode's node document is not fully active, then return.
        // > 2. If insertedNode has an open attribute and is connected, then run the dialog setup
        // >    steps given insertedNode.
        if context.tree_connected &&
            self.owner_document().is_fully_active() &&
            self.upcast::<Element>().has_attribute(&local_name!("open"))
        {
            self.run_dialog_setup_steps();
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#the-dialog-element:html-element-removing-steps>
    fn unbind_from_tree(&self, cx: &mut JSContext, context: &UnbindContext) {
        if let Some(super_type) = self.super_type() {
            super_type.unbind_from_tree(cx, context);
        }
        let element = self.upcast::<Element>();
        // > 1. If removedNode has an open attribute, then run the dialog cleanup steps given
        // >    removedNode.
        if element.has_attribute(&local_name!("open")) {
            self.run_dialog_cleanup_steps();
        }
        // > 2. If removedNode's node document's top layer contains removedNode, then remove an
        // >    element from the top layer immediately given removedNode.
        self.owner_document().top_layer().remove(element);
        // > 3. Set is modal of removedNode to false.
        element.set_modal_state(false);
    }

    /// <https://html.spec.whatwg.org/multipage/#the-dialog-element:is-valid-command-steps>
    fn is_valid_command_steps(&self, command: CommandState) -> bool {
        // Step 1. If command is in the Close state, the Request Close state, or the
        // ShowModal state, then return true.
        if matches!(
            command,
            CommandState::Close | CommandState::RequestClose | CommandState::ShowModal
        ) {
            return true;
        }
        // Step 2. Return false.
        false
    }

    /// <https://html.spec.whatwg.org/multipage/#the-dialog-element:command-steps>
    fn command_steps(
        &self,
        cx: &mut js::context::JSContext,
        source: DomRoot<HTMLButtonElement>,
        command: CommandState,
    ) -> bool {
        if self
            .super_type()
            .unwrap()
            .command_steps(cx, source.clone(), command)
        {
            return true;
        }

        // Step 1. If element is in the popover showing state, then return.
        if self.upcast::<HTMLElement>().is_popover_showing() {
            return false;
        }
        let element = self.upcast::<Element>();

        // Step 2. If command is in the Close state and element has an open attribute, then
        // close the dialog element with source's optional value and source.
        if command == CommandState::Close && element.has_attribute(&local_name!("open")) {
            let button_element = DomRoot::from_ref(source.upcast::<Element>());
            self.close_the_dialog(cx, source.optional_value(), Some(button_element));
            return true;
        }

        // Step 3. If command is in the Request Close state and element has an open attribute,
        // then request to close the dialog element with source's optional value and source.
        if command == CommandState::RequestClose && element.has_attribute(&local_name!("open")) {
            let button_element = DomRoot::from_ref(source.upcast::<Element>());
            self.request_close(cx, source.optional_value(), Some(button_element));
            return true;
        }

        // Step 4. If command is the Show Modal state and element does not have an open attribute,
        // then show a modal dialog given element and source.
        if command == CommandState::ShowModal && !element.has_attribute(&local_name!("open")) {
            let button_element = DomRoot::from_ref(source.upcast::<Element>());
            let _ = self.show_a_modal(cx, Some(button_element));
            return true;
        }

        false
    }
}

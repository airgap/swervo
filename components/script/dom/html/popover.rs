/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The popover API: <https://html.spec.whatwg.org/multipage/#the-popover-attribute>

use html5ever::{local_name, ns};
use js::context::JSContext;
use malloc_size_of_derive::MallocSizeOf;
use script_bindings::codegen::GenericBindings::DocumentBinding::DocumentMethods;
use script_bindings::codegen::GenericBindings::DocumentFragmentBinding::DocumentFragmentMethods;
use script_bindings::codegen::GenericBindings::NodeBinding::{GetRootNodeOptions, NodeMethods};
use stylo_dom::ElementState;

use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::document::top_layer::PopoverStack;
use crate::dom::documentfragment::DocumentFragment;
use crate::dom::element::Element;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::html::htmlbuttonelement::HTMLButtonElement;
use crate::dom::html::htmlelement::HTMLElement;
use crate::dom::html::htmlformelement::FormControlElementHelpers;
use crate::dom::html::input_element::HTMLInputElement;
use crate::dom::input_element::input_type::InputType;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::toggleevent::ToggleEvent;
use crate::dom::types::HTMLDialogElement;

/// <https://html.spec.whatwg.org/multipage/#attr-popover>
///
/// The No Popover state is represented by the absence of a state.
#[derive(Clone, Copy, Debug, MallocSizeOf, PartialEq)]
pub(crate) enum PopoverState {
    Auto,
    Manual,
    Hint,
}

impl PopoverState {
    /// The state for a present popover attribute with the given value.
    pub(crate) fn from_attribute_value(value: &str) -> Self {
        // > The attribute's missing value default is the No Popover state, its invalid value
        // > default is the Manual state, and its empty value default is the Auto state.
        match value.to_ascii_lowercase().as_str() {
            "" | "auto" => PopoverState::Auto,
            "hint" => PopoverState::Hint,
            _ => PopoverState::Manual,
        }
    }

    fn uses_auto_stack(self) -> bool {
        matches!(self, PopoverState::Auto | PopoverState::Hint)
    }
}

/// The value of a `popovertargetaction` attribute:
/// <https://html.spec.whatwg.org/multipage/#attr-popovertargetaction>
#[derive(Clone, Copy, PartialEq)]
enum PopoverTargetAction {
    Toggle,
    Show,
    Hide,
}

impl HTMLElement {
    /// The state of the element's popover attribute, or `None` in the No Popover state.
    pub(crate) fn popover_state(&self) -> Option<PopoverState> {
        let value = self
            .upcast::<Element>()
            .get_attribute_string_value(&local_name!("popover"))?;
        Some(PopoverState::from_attribute_value(&value))
    }

    /// Whether the element's popover visibility state is showing:
    /// <https://html.spec.whatwg.org/multipage/#popover-visibility-state>
    pub(crate) fn is_popover_showing(&self) -> bool {
        self.upcast::<Element>()
            .state()
            .contains(ElementState::POPOVER_OPEN)
    }

    fn opened_in_popover_mode(&self) -> Option<PopoverState> {
        self.upcast::<Element>()
            .ensure_rare_data()
            .opened_in_popover_mode
    }

    /// <https://html.spec.whatwg.org/multipage/#check-popover-validity>
    fn check_popover_validity(
        &self,
        expected_to_be_showing: bool,
        throw_exceptions: bool,
        expected_document: Option<&Document>,
        ignore_dom_state: bool,
    ) -> Fallible<bool> {
        // > 1. If ignoreDomState is false and element's popover attribute is in the No Popover
        // >    state, then: if throwExceptions is true, then throw a "NotSupportedError"
        // >    DOMException; return false.
        if !ignore_dom_state && self.popover_state().is_none() {
            if throw_exceptions {
                return Err(Error::NotSupported(Some(
                    "The element does not have a valid popover attribute.".into(),
                )));
            }
            return Ok(false);
        }

        // > 2. If any of the following are true: expectedToBeShowing is true and element's
        // >    popover visibility state is not showing; or expectedToBeShowing is false and
        // >    element's popover visibility state is not hidden, then return false.
        if expected_to_be_showing != self.is_popover_showing() {
            return Ok(false);
        }

        // > 3. If any of the following are true: ignoreDomState is false and element is not
        // >    connected; element's node document is not fully active; ignoreDomState is false and
        // >    expectedDocument is not null and element's node document is not expectedDocument;
        // >    element is a dialog element and its is modal is set to true; or element's
        // >    fullscreen flag is set, then: if throwExceptions is true, then throw an
        // >    "InvalidStateError" DOMException; return false.
        let element = self.upcast::<Element>();
        let document = self.owner_document();
        if (!ignore_dom_state && !element.is_connected()) ||
            !document.is_fully_active() ||
            (!ignore_dom_state &&
                expected_document.is_some_and(|expected| *expected != *document)) ||
            (self.is::<HTMLDialogElement>() && element.state().contains(ElementState::MODAL)) ||
            element.state().contains(ElementState::FULLSCREEN)
        {
            if throw_exceptions {
                return Err(Error::InvalidState(Some(
                    "The popover is disconnected, a modal dialog or fullscreen.".into(),
                )));
            }
            return Ok(false);
        }

        // > 4. Return true.
        Ok(true)
    }

    /// <https://html.spec.whatwg.org/multipage/#show-popover>
    pub(crate) fn show_popover(
        &self,
        cx: &mut JSContext,
        throw_exceptions: bool,
        invoker: Option<&HTMLElement>,
    ) -> ErrorResult {
        // > 1. If the result of running check popover validity given element, false,
        // >    throwExceptions, null, and false is false, then return.
        if !self.check_popover_validity(false, throw_exceptions, None, false)? {
            return Ok(());
        }

        // > 2. Let document be element's node document.
        let document = self.owner_document();

        // > 3. Assert: element's popover invoker is null.
        // > 4. Assert: element is not in document's top layer.
        debug_assert!(!document.top_layer().contains(self.upcast()));

        // > 5. Let nestedShow be element's popover showing or hiding.
        let nested_show = self
            .upcast::<Element>()
            .ensure_rare_data()
            .popover_showing_or_hiding;

        // > 6. Let fireEvents be the boolean negation of nestedShow.
        let fire_events = !nested_show;

        // > 7. Set element's popover showing or hiding to true.
        self.set_popover_showing_or_hiding(true);

        // > 8. Let cleanupShowingFlag be the following steps: if nestedShow is false, then set
        // >    element's popover showing or hiding to false.
        let cleanup_showing_flag = || {
            if !nested_show {
                self.set_popover_showing_or_hiding(false);
            }
        };

        // > 9. If the result of firing an event named beforetoggle, using ToggleEvent, with the
        // >    cancelable attribute initialized to true, the oldState attribute initialized to
        // >    "closed", the newState attribute initialized to "open", and the source attribute
        // >    initialized to invoker at element is false, then run cleanupShowingFlag and return.
        if !self.fire_beforetoggle(cx, true, "closed", "open", invoker) {
            cleanup_showing_flag();
            return Ok(());
        }

        // > 10. If the result of running check popover validity given element, false,
        // >     throwExceptions, document, and false is false, then run cleanupShowingFlag and
        // >     return.
        match self.check_popover_validity(false, throw_exceptions, Some(&document), false) {
            Ok(true) => {},
            result => {
                cleanup_showing_flag();
                return result.map(|_| ());
            },
        }

        // > 11. Let shouldRestoreFocus be false.
        let mut should_restore_focus = false;

        // > 12. Let originalType be the current state of element's popover attribute.
        let original_type = self
            .popover_state()
            .expect("check popover validity ensured there is a popover state");

        // > 13. Let effectiveType be originalType.
        let mut effective_type = original_type;
        let mut ancestor = None;

        // > 14. If originalType is Auto or Hint:
        if original_type.uses_auto_stack() {
            // > 14.1. Let ancestor be the result of running the topmost popover ancestor
            // >       algorithm given element, source, and true.
            ancestor = HTMLElement::topmost_popover_ancestor(self.upcast(), invoker, true);

            // > 14.2. If all of the following are true: ancestor is not null; ancestor's opened
            // >       in popover mode is "hint"; and effectiveType is the Auto state, then set
            // >       effectiveType to the Hint state.
            if effective_type == PopoverState::Auto &&
                ancestor.as_ref().is_some_and(|ancestor| {
                    ancestor.opened_in_popover_mode() == Some(PopoverState::Hint)
                })
            {
                effective_type = PopoverState::Hint;
            }

            // > 14.3. Run hide popover stack until given document, ancestor, Hint,
            // >       shouldRestoreFocus, and true.
            HTMLElement::hide_popover_stack_until(
                cx,
                &document,
                ancestor.as_deref(),
                PopoverStack::Hint,
                should_restore_focus,
                fire_events,
            );
            // > 14.4. If effectiveType is the Auto state, then run hide popover stack until given
            // >       document, ancestor, Auto, shouldRestoreFocus, and true.
            if effective_type == PopoverState::Auto {
                HTMLElement::hide_popover_stack_until(
                    cx,
                    &document,
                    ancestor.as_deref(),
                    PopoverStack::Auto,
                    should_restore_focus,
                    fire_events,
                );
            }

            // > 14.5. If originalType is not equal to the value of element's popover attribute,
            // >       then run cleanupShowingSteps; if throwExceptions is true, then throw an
            // >       "InvalidStateError" DOMException; return.
            if self.popover_state() != Some(original_type) {
                cleanup_showing_flag();
                if throw_exceptions {
                    return Err(Error::InvalidState(Some(
                        "The popover attribute changed while showing the popover.".into(),
                    )));
                }
                return Ok(());
            }

            // > 14.6. Set validityResult to the result of running check popover validity given
            // >       element, false, and document.
            // > 14.7. If validityResult is not true: run cleanupShowingSteps; if throwExceptions
            // >       is true and validityResult is a DOMException, then throw validityResult;
            // >       return.
            match self.check_popover_validity(false, throw_exceptions, Some(&document), false) {
                Ok(true) => {},
                result => {
                    cleanup_showing_flag();
                    return result.map(|_| ());
                },
            }

            // > 14.8. If the result of running topmost auto or hint popover on document is null,
            // >       then set shouldRestoreFocus to true.
            if document
                .top_layer()
                .topmost_auto_or_hint_popover()
                .is_none()
            {
                should_restore_focus = true;
            }

            // > 14.9. If effectiveType is Auto: Assert: document's showing auto popover list
            // >       does not contain element. Set element's opened in popover mode to "auto".
            // > 14.10. Otherwise: Assert: effectiveType is Hint. Assert: document's showing hint
            // >        popover list does not contain element. Set element's opened in popover mode
            // >        to "hint".
            let stack = if effective_type == PopoverState::Auto {
                PopoverStack::Auto
            } else {
                PopoverStack::Hint
            };
            document.top_layer().push_showing_popover(stack, self);

            // > 14.11. Set element's popover close watcher to the result of establishing a close
            // >        watcher given element's relevant global object, with cancelAction: return
            // >        true; closeAction: hide a popover given element, true, true, false, and
            // >        null; getEnabledState: return true.
            document.top_layer().establish_close_watcher(self);
        }

        // > 22. Set element's previously focused element to null.
        self.set_previously_focused_element(None);

        // > 23. Let originallyFocusedElement be document's focused area of the document's DOM
        // >     anchor.
        let originally_focused_element = document
            .focus_handler()
            .focused_area()
            .element()
            .map(DomRoot::from_ref);

        // > 24. Add an element to the top layer given element.
        document.top_layer().add(self.upcast());

        // > If effectiveType is Hint and ancestor's opened in popover mode is "auto", then set
        // > document's hint stack parent to ancestor.
        if effective_type == PopoverState::Hint &&
            let Some(ancestor) = ancestor &&
            ancestor.opened_in_popover_mode() == Some(PopoverState::Auto)
        {
            document.top_layer().set_hint_stack_parent(Some(&ancestor));
        }

        // > 25. Set element's popover visibility state to showing.
        self.upcast::<Element>()
            .set_state(ElementState::POPOVER_OPEN, true);

        // > 26. Set element's popover invoker to invoker.
        // > 27. Set element's opened in popover mode to effectiveType.
        {
            let mut rare_data = self.upcast::<Element>().ensure_rare_data();
            rare_data.popover_invoker.set(invoker.map(Castable::upcast));
            rare_data.opened_in_popover_mode = Some(effective_type);
        }

        // > 28. Run the popover focusing steps given element.
        self.run_popover_focusing_steps(cx);

        // > 29. If shouldRestoreFocus is true and element's popover attribute is not in the No
        // >     Popover state, then set element's previously focused element to
        // >     originallyFocusedElement.
        if should_restore_focus && self.popover_state().is_some() {
            self.set_previously_focused_element(originally_focused_element.as_deref());
        }

        // > 30. Queue a popover toggle event task given element, "closed", "open", and invoker.
        self.queue_popover_toggle_event_task("closed", "open", invoker);

        // > 31. Run cleanupShowingFlag.
        cleanup_showing_flag();
        Ok(())
    }

    /// <https://html.spec.whatwg.org/multipage/#hide-popover-algorithm>
    pub(crate) fn hide_popover(
        &self,
        cx: &mut JSContext,
        focus_previous_element: bool,
        mut fire_events: bool,
        throw_exceptions: bool,
        ignore_dom_state: bool,
        source: Option<&HTMLElement>,
    ) -> ErrorResult {
        // > 1. If the result of running check popover validity given element, true,
        // >    throwExceptions, null, and ignoreDomState is false, then return.
        if !self.check_popover_validity(true, throw_exceptions, None, ignore_dom_state)? {
            return Ok(());
        }

        // > 2. Let document be element's node document.
        let document = self.owner_document();

        // > 3. Let nestedHide be element's popover showing or hiding.
        let nested_hide = self
            .upcast::<Element>()
            .ensure_rare_data()
            .popover_showing_or_hiding;

        // > 4. Set element's popover showing or hiding to true.
        self.set_popover_showing_or_hiding(true);

        // > 5. If nestedHide is true, then set fireEvents to false.
        if nested_hide {
            fire_events = false;
        }

        // > 6. Let cleanupSteps be the following steps: if nestedHide is false, then set
        // >    element's popover showing or hiding to false.
        let cleanup_steps = || {
            if !nested_hide {
                self.set_popover_showing_or_hiding(false);
            }
        };

        // > Let autoPopoverListContainsElement be true if document's showing auto popover list
        // > contains element; otherwise false.
        // > Let hintPopoverListContainsElement be true if document's showing hint popover list
        // > contains element; otherwise false.
        let auto_popover_list_contains_element = document
            .top_layer()
            .showing_popover_list_contains(PopoverStack::Auto, self);
        let hint_popover_list_contains_element = document
            .top_layer()
            .showing_popover_list_contains(PopoverStack::Hint, self);

        // > 7. If element's opened in popover mode is "auto" or "hint":
        if self
            .opened_in_popover_mode()
            .is_some_and(PopoverState::uses_auto_stack)
        {
            // > 7.1. If hintPopoverListContainsElement is true, then run hide popover stack until
            // >      given document, element, Hint, focusPreviousElement, and fireEvents.
            if hint_popover_list_contains_element {
                HTMLElement::hide_popover_stack_until(
                    cx,
                    &document,
                    Some(self),
                    PopoverStack::Hint,
                    focus_previous_element,
                    fire_events,
                );
            }
            // > 7.2. If element is document's hint stack parent, then run hide popover stack
            // >      until given document, null, Hint, focusPreviousElement, and fireEvents.
            if document.top_layer().hint_stack_parent().as_deref() == Some(self) {
                HTMLElement::hide_popover_stack_until(
                    cx,
                    &document,
                    None,
                    PopoverStack::Hint,
                    focus_previous_element,
                    fire_events,
                );
            }
            // > 7.3. If autoPopoverListContainsElement is true, then run hide popover stack until
            // >      given document, element, Auto, focusPreviousElement, and fireEvents.
            if auto_popover_list_contains_element {
                HTMLElement::hide_popover_stack_until(
                    cx,
                    &document,
                    Some(self),
                    PopoverStack::Auto,
                    focus_previous_element,
                    fire_events,
                );
            }
            // > 7.4. Set validityResult to the result of running check popover validity given
            // >      element, true, and null.
            // > 7.5. If validityResult is not true: run cleanupSteps; if throwExceptions is true
            // >      and validityResult is a DOMException, then throw validityResult; return.
            match self.check_popover_validity(true, throw_exceptions, None, ignore_dom_state) {
                Ok(true) => {},
                result => {
                    cleanup_steps();
                    return result.map(|_| ());
                },
            }
        }

        // > 9. Set element's popover invoker to null.
        self.upcast::<Element>()
            .ensure_rare_data()
            .popover_invoker
            .set(None);

        // > 10. If fireEvents is true:
        if fire_events {
            // > 10.1. Fire an event named beforetoggle, using ToggleEvent, with the oldState
            // >       attribute initialized to "open", the newState attribute initialized to
            // >       "closed", and the source attribute set to source at element.
            self.fire_beforetoggle(cx, false, "open", "closed", source);

            // > 10.3. If the result of running check popover validity given element, true,
            // >       throwExceptions, null, and ignoreDomState is false, then run cleanupSteps
            // >       and return.
            match self.check_popover_validity(true, throw_exceptions, None, ignore_dom_state) {
                Ok(true) => {},
                result => {
                    cleanup_steps();
                    return result.map(|_| ());
                },
            }
        }

        // > 10.4. Request an element to be removed from the top layer given element.
        // > 11. Otherwise, remove an element from the top layer immediately given element.
        document.top_layer().remove(self.upcast());

        // > 12. Set element's popover close watcher to null after destroying it, and remove
        // >     element from document's showing auto or hint popover list.
        document.top_layer().destroy_close_watcher(self);
        document.top_layer().remove_showing_popover(self);

        // > 13. Set element's opened in popover mode to null.
        self.upcast::<Element>()
            .ensure_rare_data()
            .opened_in_popover_mode = None;

        // > 14. Set element's popover visibility state to hidden.
        self.upcast::<Element>()
            .set_state(ElementState::POPOVER_OPEN, false);

        // > If element is document's hint stack parent, or document's showing hint popover list is
        // > empty, then set document's hint stack parent to null.
        if document.top_layer().hint_stack_parent().as_deref() == Some(self) ||
            document
                .top_layer()
                .showing_popover_list(PopoverStack::Hint)
                .is_empty()
        {
            document.top_layer().set_hint_stack_parent(None);
        }

        // > 15. If fireEvents is true, then queue a popover toggle event task given element,
        // >     "open", "closed", and source.
        if fire_events {
            self.queue_popover_toggle_event_task("open", "closed", source);
        }

        // > 16. Let previouslyFocusedElement be element's previously focused element.
        // > 17. If previouslyFocusedElement is not null, then:
        if let Some(previously_focused_element) = self.previously_focused_element() {
            // > 17.1. Set element's previously focused element to null.
            self.set_previously_focused_element(None);
            // > 17.2. If focusPreviousElement is true and document's focused area of the
            // >       document's DOM anchor is a shadow-including inclusive descendant of
            // >       element, then run the focusing steps for previouslyFocusedElement; the
            // >       viewport should not be scrolled by doing this step.
            let anchor = document
                .focus_handler()
                .focused_area()
                .dom_anchor(&document);
            let focus_is_inside = self
                .upcast::<Node>()
                .is_shadow_including_inclusive_ancestor_of(&anchor);
            if focus_previous_element && focus_is_inside {
                previously_focused_element
                    .upcast::<Node>()
                    .run_the_focusing_steps(cx, None);
            }
        }

        // > 18. Run cleanupSteps.
        cleanup_steps();
        Ok(())
    }

    fn set_popover_showing_or_hiding(&self, value: bool) {
        self.upcast::<Element>()
            .ensure_rare_data()
            .popover_showing_or_hiding = value;
    }

    /// Fire a `beforetoggle` [`ToggleEvent`] at this element and return false if it was
    /// canceled.
    fn fire_beforetoggle(
        &self,
        cx: &mut JSContext,
        cancelable: bool,
        old_state: &str,
        new_state: &str,
        source: Option<&HTMLElement>,
    ) -> bool {
        let event = ToggleEvent::new(
            cx,
            &self.owner_window(),
            atom!("beforetoggle"),
            EventBubbles::DoesNotBubble,
            if cancelable {
                EventCancelable::Cancelable
            } else {
                EventCancelable::NotCancelable
            },
            DOMString::from(old_state),
            DOMString::from(new_state),
            source.map(|source| DomRoot::from_ref(source.upcast::<Element>())),
        );
        event
            .upcast::<Event>()
            .fire(cx, self.upcast::<EventTarget>())
    }

    /// <https://html.spec.whatwg.org/multipage/#queue-a-popover-toggle-event-task>
    fn queue_popover_toggle_event_task(
        &self,
        old_state: &str,
        new_state: &str,
        source: Option<&HTMLElement>,
    ) {
        // > 1. If element's popover toggle task tracker is not null, then: set oldState to
        // >    element's popover toggle task tracker's old state; remove element's popover toggle
        // >    task tracker's task from its task queue; set element's popover toggle task tracker
        // >    to null.
        //
        // Queued tasks cannot be removed, so a superseded task notices that the generation moved
        // on and does nothing.
        let (old_state, generation) = {
            let mut rare_data = self.upcast::<Element>().ensure_rare_data();
            let old_state = rare_data
                .popover_toggle_task_old_state
                .take()
                .unwrap_or_else(|| DOMString::from(old_state));
            rare_data.popover_toggle_task_generation += 1;
            // > 3. Set element's popover toggle task tracker to a struct with task set to the
            // >    just-queued task and old state set to oldState.
            rare_data.popover_toggle_task_old_state = Some(old_state.clone());
            (
                old_state.to_string(),
                rare_data.popover_toggle_task_generation,
            )
        };

        let this = Trusted::new(self);
        let new_state = new_state.to_owned();
        let source = source.map(|source| Trusted::new(source.upcast::<Element>()));

        // > 2. Queue an element task given the DOM manipulation task source and element to run the
        // >    following steps:
        self.owner_global()
            .task_manager()
            .dom_manipulation_task_source()
            .queue(task!(fire_popover_toggle_event: move |cx| {
                let this = this.root();
                {
                    let mut rare_data = this.upcast::<Element>().ensure_rare_data();
                    if rare_data.popover_toggle_task_generation != generation {
                        return;
                    }
                    // > 2.2. Set element's popover toggle task tracker to null.
                    rare_data.popover_toggle_task_old_state = None;
                }
                // > 2.1. Fire an event named toggle at element, using ToggleEvent, with the
                // >      oldState attribute initialized to oldState, the newState attribute
                // >      initialized to newState, and the source attribute initialized to source.
                let event = ToggleEvent::new(
                    cx,
                    &this.owner_window(),
                    atom!("toggle"),
                    EventBubbles::DoesNotBubble,
                    EventCancelable::NotCancelable,
                    DOMString::from(old_state),
                    DOMString::from(new_state),
                    source.map(|source| source.root()),
                );
                event.upcast::<Event>().fire(cx, this.upcast::<EventTarget>());
            }));
    }

    /// <https://html.spec.whatwg.org/multipage/#popover-focusing-steps>
    fn run_popover_focusing_steps(&self, cx: &mut JSContext) {
        // > 1. If subject is a dialog element, then run the dialog focusing steps given subject
        // >    and return.
        if let Some(dialog) = self.downcast::<HTMLDialogElement>() {
            dialog.run_dialog_focusing_steps(cx);
            return;
        }

        // > 2. If subject has the autofocus attribute, then let control be subject.
        // > 3. Otherwise, let control be the autofocus delegate for subject given "other".
        let node = self.upcast::<Node>();
        let control = if self
            .upcast::<Element>()
            .has_attribute(&local_name!("autofocus"))
        {
            Some(DomRoot::from_ref(node))
        } else {
            node.traverse_preorder(ShadowIncluding::No)
                .skip(1)
                .find(|descendant| {
                    descendant
                        .downcast::<Element>()
                        .is_some_and(|element| element.has_attribute(&local_name!("autofocus"))) &&
                        descendant.get_the_focusable_area().is_some()
                })
        };

        // > 4. If control is null, then return.
        // > 5. Run the focusing steps given control.
        if let Some(control) = control {
            control.run_the_focusing_steps(cx, None);
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#hide-popovers-until>
    ///
    /// An `endpoint` of `None` stands for null, which hides every auto and hint popover.
    pub(crate) fn hide_popovers_until(
        cx: &mut JSContext,
        document: &Document,
        endpoint: Option<&HTMLElement>,
        focus_previous_element: bool,
        fire_events: bool,
    ) {
        // > 1. Let endpointIsHint be true if document's showing hint popover list contains
        // >    endpoint; otherwise false.
        let endpoint_is_hint = endpoint.is_some_and(|endpoint| {
            document
                .top_layer()
                .showing_popover_list_contains(PopoverStack::Hint, endpoint)
        });

        // > 2. Run hide popover stack until given document, endpoint, Hint, focusPreviousElement,
        // >    and fireEvents.
        Self::hide_popover_stack_until(
            cx,
            document,
            endpoint,
            PopoverStack::Hint,
            focus_previous_element,
            fire_events,
        );

        // > 3. Let autoEndpoint be endpoint.
        // > 4. If endpointIsHint is true, then set autoEndpoint to document's hint stack parent.
        let auto_endpoint = if endpoint_is_hint {
            document.top_layer().hint_stack_parent()
        } else {
            endpoint.map(DomRoot::from_ref)
        };

        // > 5. Run hide popover stack until given document, autoEndpoint, Auto,
        // >    focusPreviousElement, and fireEvents.
        Self::hide_popover_stack_until(
            cx,
            document,
            auto_endpoint.as_deref(),
            PopoverStack::Auto,
            focus_previous_element,
            fire_events,
        );
    }

    /// <https://html.spec.whatwg.org/multipage/#hide-popover-stack-until>
    pub(crate) fn hide_popover_stack_until(
        cx: &mut JSContext,
        document: &Document,
        endpoint: Option<&HTMLElement>,
        stack: PopoverStack,
        focus_previous_element: bool,
        fire_events: bool,
    ) {
        // > 1. Let popoverList be document's showing auto popover list if stackType is Auto;
        // >    otherwise document's showing hint popover list.
        let popover_list = document.top_layer().showing_popover_list(stack);

        // > 2. Let lastHideIndex be 0 if popoverList does not contain endpoint; otherwise the
        // >    index of endpoint in popoverList plus 1.
        let last_hide_index = endpoint
            .and_then(|endpoint| {
                popover_list
                    .iter()
                    .position(|popover| &**popover == endpoint)
            })
            .map_or(0, |index| index + 1);

        // > 3. Let toHide be a slice of popoverList from lastHideIndex, in reverse order.
        // > 4. Let toRemain be a slice of popoverList from 0 to lastHideIndex.
        let (to_remain, to_hide) = popover_list.split_at(last_hide_index);

        // > 5. For each popover of toHide: run the hide popover algorithm given popover,
        // >    focusPreviousElement, fireEvents, false, and null.
        for popover in to_hide.iter().rev() {
            popover
                .hide_popover(cx, focus_previous_element, fire_events, false, false, None)
                .unwrap();
        }

        // > 6. Let newPopoverList be document's showing auto popover list if stackType is Auto;
        // >    otherwise document's showing hint popover list.
        // > 7. Let toCheck be newPopoverList in reverse order.
        // > 8. For each popover of toCheck: if toRemain contains popover, then continue; run the
        // >    hide popover algorithm given popover, focusPreviousElement, false, false, and null.
        let new_popover_list = document.top_layer().showing_popover_list(stack);
        for popover in new_popover_list.iter().rev() {
            if to_remain.contains(popover) {
                continue;
            }
            popover
                .hide_popover(cx, focus_previous_element, false, false, false, None)
                .unwrap();
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#topmost-popover-ancestor>
    pub(crate) fn topmost_popover_ancestor(
        new_popover_or_top_layer_element: &Element,
        source: Option<&HTMLElement>,
        is_popover: bool,
    ) -> Option<DomRoot<HTMLElement>> {
        // > 2. Otherwise (isPopover is false): Assert: source is null.
        debug_assert!(is_popover || source.is_none());

        // > 3. Let document be newPopoverOrTopLayerElement's node document.
        // > 4. Let combinedPopovers be document's showing auto popover list extended with
        // >    document's showing hint popover list.
        let document = new_popover_or_top_layer_element.owner_document();
        let mut combined_popovers = document
            .top_layer()
            .showing_popover_list(PopoverStack::Auto);
        combined_popovers.extend(
            document
                .top_layer()
                .showing_popover_list(PopoverStack::Hint),
        );

        // The index of the last item in combinedPopovers of which node is a flat tree descendant.
        let ancestor_index = |node: &Node| {
            combined_popovers.iter().rposition(|popover| {
                node.inclusive_ancestors_in_flat_tree()
                    .skip(1)
                    .any(|ancestor| &*ancestor == popover.upcast::<Node>())
            })
        };

        // > 5. Let popoverAncestorIndex be the index of the last item in combinedPopovers of which
        // >    newPopoverOrTopLayerElement is a flat tree descendant, otherwise -1.
        let popover_ancestor_index = ancestor_index(new_popover_or_top_layer_element.upcast());
        // > 6. Let sourceAncestorIndex be -1.
        // > 7. If source is not null, then set sourceAncestorIndex to the index of the last item
        // >    in combinedPopovers of which source is a flat tree descendant, otherwise -1.
        let source_ancestor_index = source.and_then(|source| ancestor_index(source.upcast()));

        // > 8. Let ancestorIndex be the maximum of popoverAncestorIndex and sourceAncestorIndex.
        // > 9. If ancestorIndex is -1, then return null.
        // > 10. Return combinedPopovers[ancestorIndex].
        let ancestor_index = popover_ancestor_index.max(source_ancestor_index)?;
        Some(combined_popovers[ancestor_index].clone())
    }

    /// <https://html.spec.whatwg.org/multipage/#nearest-inclusive-open-popover>
    pub(crate) fn nearest_inclusive_open_popover(node: &Node) -> Option<DomRoot<HTMLElement>> {
        // > 1. Let currentNode be node.
        // > 2. While currentNode is not null: if currentNode's popover attribute is in the Auto
        // >    state or the Hint state, and currentNode's popover visibility state is showing,
        // >    then return currentNode; set currentNode to currentNode's parent in the flat tree.
        // > 3. Return null.
        node.inclusive_ancestors_in_flat_tree()
            .filter_map(DomRoot::downcast::<HTMLElement>)
            .find(|element| {
                element.is_popover_showing() &&
                    element
                        .popover_state()
                        .is_some_and(PopoverState::uses_auto_stack)
            })
    }

    /// <https://html.spec.whatwg.org/multipage/#nearest-inclusive-target-popover-for-invoker>
    pub(crate) fn nearest_inclusive_target_popover_for_invoker(
        cx: &JSContext,
        node: &Node,
    ) -> Option<DomRoot<HTMLElement>> {
        // > 1. Let currentNode be node.
        // > 2. While currentNode is not null: let targetPopover be currentNode's popover target
        // >    element; if targetPopover is not null and targetPopover's popover attribute is in
        // >    the Auto state or the Hint state, and targetPopover's popover visibility state is
        // >    showing, then return targetPopover; set currentNode to currentNode's ancestor in the
        // >    flat tree.
        // > 3. Return null.
        node.inclusive_ancestors_in_flat_tree().find_map(|current| {
            let target_popover = popover_target_element(cx, &current)?;
            (target_popover.is_popover_showing() &&
                target_popover
                    .popover_state()
                    .is_some_and(PopoverState::uses_auto_stack))
            .then_some(target_popover)
        })
    }

    /// The popover attribute change steps:
    /// <https://html.spec.whatwg.org/multipage/#the-popover-attribute:concept-element-attributes-change-ext>
    pub(crate) fn popover_attribute_changed(
        &self,
        cx: &mut JSContext,
        old_state: Option<PopoverState>,
    ) {
        // > If element's popover visibility state is in the showing state and oldValue and value
        // > are in different states, then run the hide popover algorithm given element, true,
        // > true, false, and false.
        if self.is_popover_showing() && old_state != self.popover_state() {
            self.hide_popover(cx, true, true, false, false, None)
                .unwrap();
        }
    }

    /// The popover part of the HTML element removing steps:
    /// <https://html.spec.whatwg.org/multipage/#dom-trees:concept-node-remove-ext>
    pub(crate) fn popover_removing_steps(&self, cx: &mut JSContext) {
        // > If removedNode's popover attribute is not in the No Popover state, then run the hide
        // > popover algorithm given removedNode, false, false, false, and true.
        if self.popover_state().is_some() {
            self.hide_popover(cx, false, false, false, true, None)
                .unwrap();
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#popover-target-attribute-activation-behavior>
    pub(crate) fn popover_target_attribute_activation_behavior(
        &self,
        cx: &mut JSContext,
        event_target: &EventTarget,
    ) {
        // > 1. Let popover be node's popover target element.
        // > 2. If popover is null, then return.
        let Some(popover) = popover_target_element(cx, self.upcast()) else {
            return;
        };

        // > 3. If eventTarget is a shadow-including inclusive descendant of popover and popover is
        // >    a shadow-including descendant of node, then return.
        if let Some(event_target) = event_target.downcast::<Node>() &&
            popover
                .upcast::<Node>()
                .is_shadow_including_inclusive_ancestor_of(event_target) &&
            self.upcast::<Node>()
                .is_shadow_including_inclusive_ancestor_of(popover.upcast()) &&
            popover.upcast::<Node>() != self.upcast::<Node>()
        {
            return;
        }

        let action = popover_target_action(self.upcast());
        let showing = popover.is_popover_showing();

        // > 4. If node's popovertargetaction attribute is in the show state and popover's popover
        // >    visibility state is showing, then return.
        // > 5. If node's popovertargetaction attribute is in the hide state and popover's popover
        // >    visibility state is hidden, then return.
        if (action == PopoverTargetAction::Show && showing) ||
            (action == PopoverTargetAction::Hide && !showing)
        {
            return;
        }

        // > 6. If popover's popover visibility state is showing, then run the hide popover
        // >    algorithm given popover, true, true, false, false, and node.
        if showing {
            popover
                .hide_popover(cx, true, true, false, false, Some(self))
                .unwrap();
            return;
        }

        // > 7. Otherwise, if popover's popover visibility state is hidden and the result of
        // >    running check popover validity given popover, false, false, null, and false is
        // >    true, then run show popover given popover, false, and node.
        if popover
            .check_popover_validity(false, false, None, false)
            .unwrap()
        {
            popover.show_popover(cx, false, Some(self)).unwrap();
        }
    }

    /// The Toggle Popover, Show Popover and Hide Popover command steps (steps 5.7 to 5.9) of
    /// <https://html.spec.whatwg.org/multipage/#the-button-element:activation-behaviour>.
    pub(crate) fn run_popover_command(
        &self,
        cx: &mut JSContext,
        source: &HTMLElement,
        show: bool,
        hide: bool,
    ) {
        // > If command is in the Hide Popover state or the Toggle Popover state, and the result of
        // > running check popover validity given target, true, false, null, and false is true,
        // > then run the hide popover algorithm given target, true, true, false, false, and
        // > element.
        if hide &&
            self.check_popover_validity(true, false, None, false)
                .unwrap()
        {
            self.hide_popover(cx, true, true, false, false, Some(source))
                .unwrap();
            return;
        }
        // > Otherwise, if command is in the Show Popover state or the Toggle Popover state, and
        // > the result of running check popover validity given target, false, false, null, and
        // > false is true, then run show popover given target, false, and element.
        if show &&
            self.check_popover_validity(false, false, None, false)
                .unwrap()
        {
            self.show_popover(cx, false, Some(source)).unwrap();
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-showpopover>
    pub(crate) fn show_popover_method(
        &self,
        cx: &mut JSContext,
        source: Option<&HTMLElement>,
    ) -> ErrorResult {
        // > 1. Let invoker be options["source"] if it exists; otherwise, null.
        // > 2. Run show popover given this, true, and invoker.
        self.show_popover(cx, true, source)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-togglepopover>
    pub(crate) fn toggle_popover_method(
        &self,
        cx: &mut JSContext,
        force: Option<bool>,
        source: Option<&HTMLElement>,
    ) -> Fallible<bool> {
        // > 4. If this's popover visibility state is showing, and force is null or false, then run
        // >    the hide popover algorithm given this, true, true, true, and false.
        if self.is_popover_showing() && force != Some(true) {
            self.hide_popover(cx, true, true, true, false, None)?;
        }
        // > 5. Otherwise, if force is null or true, then run show popover given this, true, and
        // >    invoker.
        else if force != Some(false) {
            self.show_popover(cx, true, source)?;
        }
        // > 6. Otherwise: let expectedToBeShowing be true if this's popover visibility state is
        // >    showing; otherwise false; run check popover validity given this,
        // >    expectedToBeShowing, true, null, and false.
        else {
            self.check_popover_validity(self.is_popover_showing(), true, None, false)?;
        }
        // > 7. Return true if this's popover visibility state is showing; otherwise false.
        Ok(self.is_popover_showing())
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-popover>
    pub(crate) fn popover_attribute_getter(&self) -> Option<DOMString> {
        // > The popover IDL attribute must reflect the popover attribute, limited to only known
        // > values.
        self.popover_state().map(|state| {
            DOMString::from(match state {
                PopoverState::Auto => "auto",
                PopoverState::Manual => "manual",
                PopoverState::Hint => "hint",
            })
        })
    }
}

/// <https://html.spec.whatwg.org/multipage/#popover-target-element>
fn popover_target_element(cx: &JSContext, node: &Node) -> Option<DomRoot<HTMLElement>> {
    // > 1. If node is not a button, then return null.
    let element = node.downcast::<Element>()?;
    let is_submit_button = if let Some(button) = node.downcast::<HTMLButtonElement>() {
        button.is_submit_button()
    } else if let Some(input) = node.downcast::<HTMLInputElement>() {
        if !matches!(
            *input.input_type(),
            InputType::Submit(_) | InputType::Reset(_) | InputType::Button(_) | InputType::Image(_)
        ) {
            return None;
        }
        input.is_submit_button()
    } else {
        return None;
    };

    // > 2. If node is disabled, then return null.
    if element.is_actually_disabled() {
        return None;
    }

    // > 3. If node has a form owner and node is a submit button, then return null.
    let has_form_owner = element
        .as_maybe_form_control()
        .is_some_and(|control| control.form_owner().is_some());
    if has_form_owner && is_submit_button {
        return None;
    }

    // > 4. Let popoverElement be the result of running node's get the popovertarget-associated
    // >    element.
    // > 5. If popoverElement is null, then return null.
    let popover_element = popovertarget_associated_element(cx, element)?;
    let popover_element = DomRoot::downcast::<HTMLElement>(popover_element)?;

    // > 6. If popoverElement's popover attribute is in the No Popover state, then return null.
    popover_element.popover_state()?;

    // > 7. Return popoverElement.
    Some(popover_element)
}

/// <https://html.spec.whatwg.org/multipage/#attr-associated-element> for `popovertarget`.
pub(crate) fn popovertarget_associated_element(
    cx: &JSContext,
    element: &Element,
) -> Option<DomRoot<Element>> {
    // > 1. If element's explicitly set attr-element is not null: if it is a descendant of any of
    // >    element's shadow-including ancestors, return it; otherwise return null.
    let explicitly_set = element
        .ensure_rare_data()
        .explicitly_set_popover_target_element
        .get();
    if let Some(explicitly_set) = explicitly_set {
        let explicitly_set_node = explicitly_set.upcast::<Node>();
        let in_scope = element
            .upcast::<Node>()
            .inclusive_ancestors(ShadowIncluding::Yes)
            .skip(1)
            .any(|ancestor| ancestor.is_ancestor_of(explicitly_set_node));
        return in_scope.then_some(explicitly_set);
    }

    // > 2. Otherwise, if the content attribute is present in element, return the first element
    // >    in tree order within element's root whose ID is the attribute's value.
    let id = element.get_attribute_string_value(&local_name!("popovertarget"))?;
    let root_node = element
        .upcast::<Node>()
        .GetRootNode(&GetRootNodeOptions::empty());
    if let Some(document) = root_node.downcast::<Document>() {
        return document.GetElementById(cx, DOMString::from(id));
    }
    root_node
        .downcast::<DocumentFragment>()?
        .GetElementById(cx, DOMString::from(id))
}

/// <https://html.spec.whatwg.org/multipage/#attr-popovertargetaction>
fn popover_target_action(element: &Element) -> PopoverTargetAction {
    // > The attribute's missing value default and invalid value default are both the toggle
    // > state.
    match element
        .get_attribute_string_value(&local_name!("popovertargetaction"))
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("show") => PopoverTargetAction::Show,
        Some("hide") => PopoverTargetAction::Hide,
        _ => PopoverTargetAction::Toggle,
    }
}

/// The getter of the `popoverTargetAction` IDL attribute, which reflects `popovertargetaction`
/// limited to only known values.
pub(crate) fn popover_target_action_getter(element: &Element) -> DOMString {
    DOMString::from(match popover_target_action(element) {
        PopoverTargetAction::Toggle => "toggle",
        PopoverTargetAction::Show => "show",
        PopoverTargetAction::Hide => "hide",
    })
}

/// The setter of a reflected `Element?` attribute for `popovertarget`:
/// <https://html.spec.whatwg.org/multipage/#reflecting-content-attributes-in-idl-attributes>
pub(crate) fn set_popover_target_element(
    cx: &mut JSContext,
    element: &Element,
    value: Option<&Element>,
) {
    match value {
        // > 1. If the given value is null, then: set this's explicitly set attr-element to null;
        // >    run this's delete the content attribute; return.
        None => {
            element
                .ensure_rare_data()
                .explicitly_set_popover_target_element
                .set(None);
            element.remove_attribute(cx, &ns!(), &local_name!("popovertarget"));
        },
        // > 2. Run this's set the content attribute with the empty string.
        // > 3. Set this's explicitly set attr-element to a weak reference to the given value.
        Some(value) => {
            element.set_string_attribute(cx, &local_name!("popovertarget"), DOMString::new());
            element
                .ensure_rare_data()
                .explicitly_set_popover_target_element
                .set(Some(value));
        },
    }
}

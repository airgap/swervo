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
use crate::dom::documentfragment::DocumentFragment;
use crate::dom::element::Element;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::html::htmlbuttonelement::HTMLButtonElement;
use crate::dom::html::htmlelement::HTMLElement;
use crate::dom::html::htmlformelement::{FormControl, FormControlElementHelpers};
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
    /// Hint popovers share the auto popover stack: the separate showing hint popover list is not
    /// implemented.
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

        // > 13. Let stackToAppendTo be null.
        // > 14. If originalType is the Auto state, then: run close entire popover list given
        // >     document's showing hint popover list, shouldRestoreFocus, and fireEvents; let
        // >     ancestor be the result of running the topmost popover ancestor algorithm given
        // >     element, document's showing auto popover list, invoker, and true; if ancestor is
        // >     null, then set ancestor to document; run hide all popovers until given ancestor,
        // >     shouldRestoreFocus, and fireEvents; set stackToAppendTo to "auto".
        if original_type.uses_auto_stack() {
            let ancestor = HTMLElement::topmost_popover_ancestor(
                self.upcast(),
                &document.top_layer().showing_auto_popover_list(),
                invoker,
                true,
            );
            HTMLElement::hide_all_popovers_until(
                cx,
                &document,
                ancestor.as_deref(),
                should_restore_focus,
                fire_events,
            );

            // > 16. If originalType is not equal to the value of element's popover attribute,
            // >     then: if throwExceptions is true, then throw an "InvalidStateError"
            // >     DOMException; return.
            if self.popover_state() != Some(original_type) {
                cleanup_showing_flag();
                if throw_exceptions {
                    return Err(Error::InvalidState(Some(
                        "The popover attribute changed while showing the popover.".into(),
                    )));
                }
                return Ok(());
            }

            // > 17. If the result of running check popover validity given element, false,
            // >     throwExceptions, document, and false is false, then run cleanupShowingFlag
            // >     and return.
            match self.check_popover_validity(false, throw_exceptions, Some(&document), false) {
                Ok(true) => {},
                result => {
                    cleanup_showing_flag();
                    return result.map(|_| ());
                },
            }

            // > 18. If the result of running topmost auto or hint popover on document is null,
            // >     then set shouldRestoreFocus to true.
            if document.top_layer().topmost_auto_popover().is_none() {
                should_restore_focus = true;
            }

            // > 19. If stackToAppendTo is "auto": Assert: document's showing auto popover list
            // >     does not contain element. Set element's opened in popover mode to "auto".
            // >     Append element to document's showing auto popover list.
            document.top_layer().push_showing_auto_popover(self);

            // > 21. Set element's popover close watcher to the result of establishing a close
            // >     watcher given element's relevant global object, with cancelAction: return
            // >     true; closeAction: hide a popover given element, true, true, false, and null;
            // >     getEnabledState: return true.
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

        // > 25. Set element's popover visibility state to showing.
        self.upcast::<Element>()
            .set_state(ElementState::POPOVER_OPEN, true);

        // > 26. Set element's popover invoker to invoker.
        // > 27. Set element's opened in popover mode to originalType.
        {
            let mut rare_data = self.upcast::<Element>().ensure_rare_data();
            rare_data.popover_invoker.set(invoker.map(Castable::upcast));
            rare_data.opened_in_popover_mode = Some(original_type);
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

        // > 7. If element's opened in popover mode is "auto" or "hint", then: run hide all
        // >    popovers until given element, focusPreviousElement, and fireEvents; if the result
        // >    of running check popover validity given element, true, throwExceptions, and
        // >    ignoreDomState is false, then run cleanupSteps and return.
        if self
            .opened_in_popover_mode()
            .is_some_and(PopoverState::uses_auto_stack)
        {
            HTMLElement::hide_all_popovers_until(
                cx,
                &document,
                Some(self),
                focus_previous_element,
                fire_events,
            );
            match self.check_popover_validity(true, throw_exceptions, None, ignore_dom_state) {
                Ok(true) => {},
                result => {
                    cleanup_steps();
                    return result.map(|_| ());
                },
            }
        }

        // > 8. Let autoPopoverListContainsElement be true if document's showing auto popover
        // >    list's last item is element, otherwise false.
        let auto_popover_list_contains_element =
            document.top_layer().topmost_auto_popover().as_deref() == Some(self);

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

            // > 10.2. If autoPopoverListContainsElement is true and document's showing auto
            // >       popover list's last item is not element, then run hide all popovers until
            // >       given element, focusPreviousElement, and false.
            if auto_popover_list_contains_element &&
                document.top_layer().topmost_auto_popover().as_deref() != Some(self)
            {
                HTMLElement::hide_all_popovers_until(
                    cx,
                    &document,
                    Some(self),
                    focus_previous_element,
                    false,
                );
            }

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
        // >     element from document's showing auto popover list.
        document.top_layer().destroy_close_watcher(self);
        document.top_layer().remove_showing_auto_popover(self);

        // > 13. Set element's opened in popover mode to null.
        self.upcast::<Element>()
            .ensure_rare_data()
            .opened_in_popover_mode = None;

        // > 14. Set element's popover visibility state to hidden.
        self.upcast::<Element>()
            .set_state(ElementState::POPOVER_OPEN, false);

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

    /// <https://html.spec.whatwg.org/multipage/#hide-all-popovers-until>
    ///
    /// An `endpoint` of `None` stands for the document.
    pub(crate) fn hide_all_popovers_until(
        cx: &mut JSContext,
        document: &Document,
        endpoint: Option<&HTMLElement>,
        focus_previous_element: bool,
        fire_events: bool,
    ) {
        // > 1. If endpoint is an HTML element and endpoint is not in the popover showing state,
        // >    then return.
        if endpoint.is_some_and(|endpoint| !endpoint.is_popover_showing()) {
            return;
        }

        // > 5. If endpoint is a Document: run close entire popover list given document's showing
        // >    auto popover list, focusPreviousElement, and fireEvents; return.
        let Some(endpoint) = endpoint else {
            while let Some(popover) = document.top_layer().topmost_auto_popover() {
                popover
                    .hide_popover(cx, focus_previous_element, fire_events, false, false, None)
                    .unwrap();
            }
            return;
        };

        // > 7. Run hide popover stack until given endpoint, document's showing auto popover list,
        // >    focusPreviousElement, and fireEvents.
        Self::hide_popover_stack_until(cx, document, endpoint, focus_previous_element, fire_events);
    }

    /// <https://html.spec.whatwg.org/multipage/#hide-popover-stack-until>
    fn hide_popover_stack_until(
        cx: &mut JSContext,
        document: &Document,
        endpoint: &HTMLElement,
        focus_previous_element: bool,
        mut fire_events: bool,
    ) {
        // > 1. Let repeatingHide be false.
        // > 2. Perform the following steps at least once:
        loop {
            // > 2.1. Let lastToHide be null.
            // > 2.2. For each popover in popoverList: if popover is endpoint, then break; set
            // >      lastToHide to popover... in reverse, so that lastToHide is the popover
            // >      directly above endpoint.
            let popover_list = document.top_layer().showing_auto_popover_list();
            let last_to_hide = popover_list
                .iter()
                .position(|popover| &**popover == endpoint)
                .and_then(|index| popover_list.get(index + 1).cloned());

            // > 2.3. If lastToHide is null, then return.
            let Some(last_to_hide) = last_to_hide else {
                return;
            };

            // > 2.4. While lastToHide's popover visibility state is showing: Assert: popoverList
            // >      is not empty; run the hide popover algorithm given the last item in
            // >      popoverList, focusPreviousElement, fireEvents, false, and false.
            while last_to_hide.is_popover_showing() {
                let topmost = document
                    .top_layer()
                    .topmost_auto_popover()
                    .expect("A showing auto popover is in the showing auto popover list");
                topmost
                    .hide_popover(cx, focus_previous_element, fire_events, false, false, None)
                    .unwrap();
            }

            // > 2.6. Set repeatingHide to true if popoverList contains endpoint and popoverList's
            // >      last item is not endpoint, otherwise false.
            let repeating_hide = document
                .top_layer()
                .showing_auto_popover_list_contains(endpoint) &&
                document.top_layer().topmost_auto_popover().as_deref() != Some(endpoint);

            // > 2.7. If repeatingHide is true, then set fireEvents to false.
            if !repeating_hide {
                return;
            }
            fire_events = false;
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#topmost-popover-ancestor>
    pub(crate) fn topmost_popover_ancestor(
        new_popover_or_top_layer_element: &Element,
        popover_list: &[DomRoot<HTMLElement>],
        invoker: Option<&HTMLElement>,
        is_popover: bool,
    ) -> Option<DomRoot<HTMLElement>> {
        // > 2. Let popoverPositions be an empty ordered map.
        // > 3. Let index be 0.
        // > 4. For each popover of popoverList: set popoverPositions[popover] to index; increment
        // >    index by 1.
        // > 5. If isPopover is true, then set popoverPositions[newPopoverOrTopLayerElement] to
        // >    index, and increment index by 1.
        let position = |popover: &HTMLElement| -> Option<usize> {
            if let Some(index) = popover_list.iter().position(|item| &**item == popover) {
                return Some(index);
            }
            if is_popover && popover.upcast::<Element>() == new_popover_or_top_layer_element {
                return Some(popover_list.len());
            }
            None
        };

        // > 6. Let topmostPopoverAncestor be null.
        let mut topmost_popover_ancestor: Option<(DomRoot<HTMLElement>, usize)> = None;

        // > 7. Let checkAncestor be an algorithm which performs the following steps given
        // >    candidate:
        let mut check_ancestor = |candidate: Option<DomRoot<Node>>| {
            // > 7.1. If candidate is null, then return.
            let Some(candidate) = candidate else {
                return;
            };
            // > 7.2. Let okNesting be false. 7.3. Let candidateAncestor be null.
            // > 7.4. While okNesting is false: set candidateAncestor to the result of running
            // >      nearest inclusive open popover given candidate; if candidateAncestor is null
            // >      or popoverPositions does not contain candidateAncestor, then return.
            //
            // Hint popovers share the auto stack, so nesting is always ok.
            let Some(candidate_ancestor) = Self::nearest_inclusive_open_popover(&candidate) else {
                return;
            };
            let Some(candidate_position) = position(&candidate_ancestor) else {
                return;
            };
            // > 7.6. If topmostPopoverAncestor is null or
            // >      popoverPositions[topmostPopoverAncestor] is less than candidatePosition,
            // >      then set topmostPopoverAncestor to candidateAncestor.
            if topmost_popover_ancestor
                .as_ref()
                .is_none_or(|(_, topmost_position)| *topmost_position < candidate_position)
            {
                topmost_popover_ancestor = Some((candidate_ancestor, candidate_position));
            }
        };

        // > 8. Run checkAncestor given newPopoverOrTopLayerElement's parent node within the flat
        // >    tree.
        check_ancestor(
            new_popover_or_top_layer_element
                .upcast::<Node>()
                .parent_in_flat_tree(),
        );

        // > 9. Run checkAncestor given invoker.
        check_ancestor(invoker.map(|invoker| DomRoot::from_ref(invoker.upcast::<Node>())));

        // > 10. Return topmostPopoverAncestor.
        topmost_popover_ancestor.map(|(popover, _)| popover)
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

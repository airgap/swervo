/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::MouseEventBinding::MouseEventMethods;
use script_bindings::inheritance::Castable;
use script_bindings::root::{Dom, DomRoot};
use stylo_dom::ElementState;

use crate::dom::bindings::root::{LayoutDom, MutNullableDom};
use crate::dom::html::htmldialogelement::ClosedByState;
use crate::dom::node::Node;
use crate::dom::types::{Element, HTMLDialogElement, HTMLElement, MouseEvent};

/// The kind of pointer event that runs the light dismiss activities.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum LightDismissEventType {
    PointerDown,
    PointerUp,
}

/// The [`DocumentTopLayer`] holds a `Document`'s top layer together with the bookkeeping that
/// decides what enters and leaves it: the showing auto popover list, the open dialogs list, the
/// light dismiss pointerdown targets and the window's close watcher manager.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct DocumentTopLayer {
    /// <https://drafts.csswg.org/css-position-4/#document-top-layer>
    top_layer: DomRefCell<Vec<Dom<Element>>>,
    /// <https://html.spec.whatwg.org/multipage/#showing-auto-popover-list>
    showing_auto_popover_list: DomRefCell<Vec<Dom<HTMLElement>>>,
    /// <https://html.spec.whatwg.org/multipage/#open-dialogs-list>
    open_dialogs_list: DomRefCell<Vec<Dom<HTMLDialogElement>>>,
    /// <https://html.spec.whatwg.org/multipage/#popover-pointerdown-target>
    popover_pointerdown_target: MutNullableDom<HTMLElement>,
    /// <https://html.spec.whatwg.org/multipage/#dialog-pointerdown-target>
    dialog_pointerdown_target: MutNullableDom<HTMLDialogElement>,
    /// <https://html.spec.whatwg.org/multipage/#close-watcher-manager-groups>
    ///
    /// A close watcher is represented by the dialog or popover element that established it.
    close_watcher_groups: DomRefCell<Vec<Vec<Dom<HTMLElement>>>>,
    /// <https://html.spec.whatwg.org/multipage/#allowed-number-of-groups>
    allowed_number_of_groups: Cell<usize>,
    /// <https://html.spec.whatwg.org/multipage/#next-user-interaction-allows-a-new-group>
    next_user_interaction_allows_a_new_group: Cell<bool>,
}

impl Default for DocumentTopLayer {
    fn default() -> Self {
        Self {
            top_layer: Default::default(),
            showing_auto_popover_list: Default::default(),
            open_dialogs_list: Default::default(),
            popover_pointerdown_target: Default::default(),
            dialog_pointerdown_target: Default::default(),
            close_watcher_groups: Default::default(),
            allowed_number_of_groups: Cell::new(1),
            next_user_interaction_allows_a_new_group: Cell::new(true),
        }
    }
}

impl DocumentTopLayer {
    pub(crate) fn contains(&self, element: &Element) -> bool {
        self.top_layer
            .borrow()
            .iter()
            .any(|item| &**item == element)
    }

    /// The top layer in the order its elements were added, which is the order layout paints and
    /// hit tests them in.
    #[expect(unsafe_code)]
    pub(crate) fn elements_for_layout(&self) -> &[LayoutDom<'_, Element>] {
        // # Safety: `Dom<Element>` has the same memory layout as `LayoutDom<'_, Element>`.
        unsafe { LayoutDom::to_layout_slice(self.top_layer.borrow_for_layout()) }
    }

    /// <https://drafts.csswg.org/css-position-4/#add-an-element-to-the-top-layer>
    pub(crate) fn add(&self, element: &Element) {
        // > If el is already contained in top layer, remove it first, then append it.
        self.remove(element);
        self.top_layer.borrow_mut().push(Dom::from_ref(element));
    }

    /// <https://drafts.csswg.org/css-position-4/#remove-an-element-from-the-top-layer-immediately>
    ///
    /// Overlay transitions are not supported, so a removal request takes effect immediately.
    pub(crate) fn remove(&self, element: &Element) {
        self.top_layer
            .borrow_mut()
            .retain(|item| &**item != element);
    }

    /// <https://html.spec.whatwg.org/multipage/#blocked-by-a-modal-dialog>: the topmost modal
    /// dialog in the top layer, which makes everything outside of it inert.
    pub(crate) fn blocking_modal_dialog(&self) -> Option<DomRoot<HTMLDialogElement>> {
        self.top_layer.borrow().iter().rev().find_map(|element| {
            if !element.state().contains(ElementState::MODAL) {
                return None;
            }
            element
                .downcast::<HTMLDialogElement>()
                .map(DomRoot::from_ref)
        })
    }

    pub(crate) fn showing_auto_popover_list(&self) -> Vec<DomRoot<HTMLElement>> {
        self.showing_auto_popover_list
            .borrow()
            .iter()
            .map(|popover| DomRoot::from_ref(&**popover))
            .collect()
    }

    pub(crate) fn push_showing_auto_popover(&self, popover: &HTMLElement) {
        debug_assert!(!self.showing_auto_popover_list_contains(popover));
        self.showing_auto_popover_list
            .borrow_mut()
            .push(Dom::from_ref(popover));
    }

    pub(crate) fn remove_showing_auto_popover(&self, popover: &HTMLElement) {
        self.showing_auto_popover_list
            .borrow_mut()
            .retain(|item| &**item != popover);
    }

    pub(crate) fn showing_auto_popover_list_contains(&self, popover: &HTMLElement) -> bool {
        self.showing_auto_popover_list
            .borrow()
            .iter()
            .any(|item| &**item == popover)
    }

    /// <https://html.spec.whatwg.org/multipage/#topmost-auto-popover>
    pub(crate) fn topmost_auto_popover(&self) -> Option<DomRoot<HTMLElement>> {
        self.showing_auto_popover_list
            .borrow()
            .last()
            .map(|popover| DomRoot::from_ref(&**popover))
    }

    /// <https://html.spec.whatwg.org/multipage/#popover-stack-position>: one more than the index
    /// of `popover` in the showing auto popover list, or zero when it is not in that list.
    pub(crate) fn popover_stack_position(&self, popover: Option<&HTMLElement>) -> usize {
        let Some(popover) = popover else {
            return 0;
        };
        self.showing_auto_popover_list
            .borrow()
            .iter()
            .position(|item| &**item == popover)
            .map_or(0, |index| index + 1)
    }

    pub(crate) fn add_open_dialog(&self, dialog: &HTMLDialogElement) {
        if self
            .open_dialogs_list
            .borrow()
            .iter()
            .any(|item| &**item == dialog)
        {
            return;
        }
        self.open_dialogs_list
            .borrow_mut()
            .push(Dom::from_ref(dialog));
    }

    pub(crate) fn remove_open_dialog(&self, dialog: &HTMLDialogElement) {
        self.open_dialogs_list
            .borrow_mut()
            .retain(|item| &**item != dialog);
    }

    /// <https://html.spec.whatwg.org/multipage/#establish-a-close-watcher>
    pub(crate) fn establish_close_watcher(&self, close_watcher: &HTMLElement) {
        self.destroy_close_watcher(close_watcher);
        let mut groups = self.close_watcher_groups.borrow_mut();
        // > 4. If manager's groups's size is less than manager's allowed number of groups, then
        // >    append « closeWatcher » to manager's groups.
        // > 5. Otherwise: Assert: manager's groups's size is at least 1 in this branch, since
        // >    manager's allowed number of groups is always at least 1. Append closeWatcher to
        // >    manager's groups's last item.
        if groups.len() < self.allowed_number_of_groups.get() {
            groups.push(vec![Dom::from_ref(close_watcher)]);
        } else {
            groups
                .last_mut()
                .expect("There is always at least one allowed group")
                .push(Dom::from_ref(close_watcher));
        }
        // > 6. Set manager's next user interaction allows a new group to true.
        self.next_user_interaction_allows_a_new_group.set(true);
    }

    /// <https://html.spec.whatwg.org/multipage/#close-watcher-destroy>
    pub(crate) fn destroy_close_watcher(&self, close_watcher: &HTMLElement) {
        let mut groups = self.close_watcher_groups.borrow_mut();
        for group in groups.iter_mut() {
            group.retain(|item| &**item != close_watcher);
        }
        groups.retain(|group| !group.is_empty());
    }

    /// <https://html.spec.whatwg.org/multipage/#close-watcher-active>
    pub(crate) fn is_close_watcher_active(&self, close_watcher: &HTMLElement) -> bool {
        self.close_watcher_groups
            .borrow()
            .iter()
            .flatten()
            .any(|item| &**item == close_watcher)
    }

    /// <https://html.spec.whatwg.org/multipage/#notify-the-close-watcher-manager-about-user-activation>
    pub(crate) fn notify_about_user_activation(&self) {
        // > 2. If manager's next user interaction allows a new group is true, then increment
        // >    manager's allowed number of groups.
        // > 3. Set manager's next user interaction allows a new group to false.
        if self.next_user_interaction_allows_a_new_group.get() {
            self.allowed_number_of_groups
                .set(self.allowed_number_of_groups.get() + 1);
        }
        self.next_user_interaction_allows_a_new_group.set(false);
    }

    /// Part of <https://html.spec.whatwg.org/multipage/#close-watcher-request-close>:
    /// whether a close request may be cancelled without consuming more than the available groups.
    fn groups_below_allowed_number(&self) -> bool {
        self.close_watcher_groups.borrow().len() < self.allowed_number_of_groups.get()
    }

    /// <https://html.spec.whatwg.org/multipage/#process-close-watchers>
    pub(crate) fn process_close_watchers(&self, cx: &mut JSContext) -> bool {
        // > 1. Let processedACloseWatcher be false.
        let mut processed_a_close_watcher = false;

        // > 2. If window's close watcher manager's groups is not empty:
        // > 2.1. Let group be the last item in window's close watcher manager's groups.
        let group: Vec<DomRoot<HTMLElement>> = self
            .close_watcher_groups
            .borrow()
            .last()
            .map(|group| {
                group
                    .iter()
                    .map(|item| DomRoot::from_ref(&**item))
                    .collect()
            })
            .unwrap_or_default();

        // > 2.2. For each closeWatcher of group, in reverse order:
        for close_watcher in group.iter().rev() {
            // > 2.2.1. If the result of running closeWatcher's get enabled state is true, set
            // >        processedACloseWatcher to true.
            if close_watcher_enabled_state(close_watcher) {
                processed_a_close_watcher = true;
            }
            // > 2.2.2. Let shouldProceed be the result of requesting to close closeWatcher with
            // >        true.
            // > 2.2.3. If shouldProceed is false, then break.
            if !self.request_to_close(cx, close_watcher, true) {
                break;
            }
        }

        // > 3. If window's close watcher manager's allowed number of groups is greater than 1,
        // >    decrement it by 1.
        if self.allowed_number_of_groups.get() > 1 {
            self.allowed_number_of_groups
                .set(self.allowed_number_of_groups.get() - 1);
        }

        // > 4. Return processedACloseWatcher.
        processed_a_close_watcher
    }

    /// <https://html.spec.whatwg.org/multipage/#close-watcher-request-close>
    pub(crate) fn request_to_close(
        &self,
        cx: &mut JSContext,
        close_watcher: &HTMLElement,
        require_history_action_activation: bool,
    ) -> bool {
        // > 1. If closeWatcher is not active, then return true.
        if !self.is_close_watcher_active(close_watcher) {
            return true;
        }
        // > 2. If the result of running closeWatcher's get enabled state is false, then return
        // >    true.
        if !close_watcher_enabled_state(close_watcher) {
            return true;
        }
        // > 3. If closeWatcher's is running cancel action is true, then return true.
        let dialog = close_watcher.downcast::<HTMLDialogElement>();
        if dialog.is_some_and(HTMLDialogElement::is_running_cancel_action) {
            return true;
        }
        // > 4. Let window be closeWatcher's window.
        // > 5. If window's associated Document is not fully active, then return true.
        let document = close_watcher.upcast::<Node>().owner_doc();
        if !document.is_fully_active() {
            return true;
        }
        let window = document.window();

        // > 6. Let canPreventClose be true if requireHistoryActionActivation is false, or if
        // >    window's close watcher manager's groups's size is less than window's close watcher
        // >    manager's allowed number of groups, and window has history-action activation;
        // >    otherwise false.
        let can_prevent_close = !require_history_action_activation ||
            (self.groups_below_allowed_number() && window.has_history_action_activation());

        // > 7. Set closeWatcher's is running cancel action to true.
        // > 8. Let shouldContinue be the result of running closeWatcher's cancel action given
        // >    canPreventClose.
        // > 9. Set closeWatcher's is running cancel action to false.
        //
        // Popovers have no cancel action, so they always continue.
        let should_continue = match dialog {
            Some(dialog) => dialog.run_close_watcher_cancel_action(cx, can_prevent_close),
            None => true,
        };

        // > 10. If shouldContinue is false, then:
        if !should_continue {
            // > 10.1. Assert: canPreventClose is true.
            debug_assert!(can_prevent_close);
            // > 10.2. Consume history-action user activation given window.
            window.consume_history_action_user_activation();
            // > 10.3. Return false.
            return false;
        }

        // > 11. If closeWatcher is not active, then return true.
        // > 12. If window's associated Document is not fully active, then return true.
        // > 13. Close closeWatcher.
        self.close(cx, close_watcher);

        // > 14. Return true.
        true
    }

    /// <https://html.spec.whatwg.org/multipage/#close-watcher-close>
    fn close(&self, cx: &mut JSContext, close_watcher: &HTMLElement) {
        // > 1. If closeWatcher is not active, then return.
        if !self.is_close_watcher_active(close_watcher) {
            return;
        }
        // > 2. If closeWatcher's window's associated Document is not fully active, then return.
        if !close_watcher.upcast::<Node>().owner_doc().is_fully_active() {
            return;
        }
        // > 3. Destroy closeWatcher.
        self.destroy_close_watcher(close_watcher);
        // > 4. Run closeWatcher's close action.
        match close_watcher.downcast::<HTMLDialogElement>() {
            // Close the dialog given dialog, dialog's request close return value, and dialog's
            // request close source element.
            Some(dialog) => dialog.run_close_watcher_close_action(cx),
            // Hide popover given element, true, true, false, and null.
            None => close_watcher
                .hide_popover(cx, true, true, false, false, None)
                .unwrap(),
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#run-light-dismiss-activities>
    pub(crate) fn run_light_dismiss_activities(
        &self,
        cx: &mut JSContext,
        event: &MouseEvent,
        event_type: LightDismissEventType,
        target: &Node,
    ) {
        // > 1. Run light dismiss open popovers with event.
        self.light_dismiss_open_popovers(cx, event_type, target);
        // > 2. Run light dismiss open dialogs with event.
        self.light_dismiss_open_dialogs(cx, event, event_type, target);
    }

    /// <https://html.spec.whatwg.org/multipage/#light-dismiss-open-popovers>
    fn light_dismiss_open_popovers(
        &self,
        cx: &mut JSContext,
        event_type: LightDismissEventType,
        target: &Node,
    ) {
        // > 4. Let topmostPopover be the result of running topmost auto popover given document.
        // > 5. If topmostPopover is null, then return.
        if self.topmost_auto_popover().is_none() {
            return;
        }

        match event_type {
            // > 6. If event is a PointerEvent and event's type is "pointerdown", then: set
            // >    document's popover pointerdown target to the result of running topmost clicked
            // >    popover given target.
            LightDismissEventType::PointerDown => {
                self.popover_pointerdown_target
                    .set(self.topmost_clicked_popover(cx, target).as_deref());
            },
            // > 7. If event is a PointerEvent and event's type is "pointerup", then:
            LightDismissEventType::PointerUp => {
                // > 7.1. Let ancestor be the result of running topmost clicked popover given
                // >      target.
                let ancestor = self.topmost_clicked_popover(cx, target);
                // > 7.2. Let sameTarget be true if ancestor is document's popover pointerdown
                // >      target.
                let same_target =
                    ancestor.as_deref() == self.popover_pointerdown_target.get().as_deref();
                // > 7.3. Set document's popover pointerdown target to null.
                self.popover_pointerdown_target.set(None);
                // > 7.4. If ancestor is null, then set ancestor to document.
                // > 7.5. If sameTarget is true, then run hide all popovers until given ancestor,
                // >      false, and true.
                if same_target {
                    HTMLElement::hide_all_popovers_until(
                        cx,
                        &target.owner_doc(),
                        ancestor.as_deref(),
                        false,
                        true,
                    );
                }
            },
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#topmost-clicked-popover>
    fn topmost_clicked_popover(&self, cx: &JSContext, node: &Node) -> Option<DomRoot<HTMLElement>> {
        // > 1. Let clickedPopover be the result of running nearest inclusive open popover given
        // >    node.
        let clicked_popover = HTMLElement::nearest_inclusive_open_popover(node);
        // > 2. Let invokerPopover be the result of running nearest inclusive target popover for
        // >    invoker given node.
        let invoker_popover = HTMLElement::nearest_inclusive_target_popover_for_invoker(cx, node);
        // > 3. If the result of getting the popover stack position given clickedPopover is
        // >    greater than the result of getting the popover stack position given
        // >    invokerPopover, then return clickedPopover.
        // > 4. Return invokerPopover.
        if self.popover_stack_position(clicked_popover.as_deref()) >
            self.popover_stack_position(invoker_popover.as_deref())
        {
            return clicked_popover;
        }
        invoker_popover
    }

    /// <https://html.spec.whatwg.org/multipage/#light-dismiss-open-dialogs>
    fn light_dismiss_open_dialogs(
        &self,
        cx: &mut JSContext,
        event: &MouseEvent,
        event_type: LightDismissEventType,
        target: &Node,
    ) {
        // > 3. If document's open dialogs list is empty, then return.
        if self.open_dialogs_list.borrow().is_empty() {
            return;
        }

        // > 4. Let ancestor be the result of running nearest clicked dialog given event.
        let ancestor = nearest_clicked_dialog(event, target);

        match event_type {
            // > 5. If event's type is "pointerdown", then set document's dialog pointerdown target
            // >    to ancestor.
            LightDismissEventType::PointerDown => {
                self.dialog_pointerdown_target.set(ancestor.as_deref());
            },
            // > 6. If event's type is "pointerup", then:
            LightDismissEventType::PointerUp => {
                // > 6.1. Let sameTarget be true if ancestor is document's dialog pointerdown
                // >      target.
                let same_target =
                    ancestor.as_deref() == self.dialog_pointerdown_target.get().as_deref();
                // > 6.2. Set document's dialog pointerdown target to null.
                self.dialog_pointerdown_target.set(None);
                // > 6.3. If sameTarget is false, then return.
                if !same_target {
                    return;
                }
                // > 6.4. Let topmostDialog be the last element of document's open dialogs list.
                let Some(topmost_dialog) = self
                    .open_dialogs_list
                    .borrow()
                    .last()
                    .map(|dialog| DomRoot::from_ref(&**dialog))
                else {
                    return;
                };
                // > 6.5. If ancestor is topmostDialog, then return.
                if ancestor.as_deref() == Some(&*topmost_dialog) {
                    return;
                }
                // > 6.6. If topmostDialog's computed closed-by state is not Any, then return.
                if topmost_dialog.computed_closed_by_state() != ClosedByState::Any {
                    return;
                }
                // > 6.7. Assert: topmostDialog's close watcher is not null.
                // > 6.8. Request to close topmostDialog's close watcher with false.
                self.request_to_close(cx, topmost_dialog.upcast(), false);
            },
        }
    }
}

/// <https://html.spec.whatwg.org/multipage/#close-watcher-get-enabled-state>
fn close_watcher_enabled_state(close_watcher: &HTMLElement) -> bool {
    match close_watcher.downcast::<HTMLDialogElement>() {
        Some(dialog) => dialog.close_watcher_enabled_state(),
        // Popover close watchers are always enabled.
        None => true,
    }
}

/// <https://html.spec.whatwg.org/multipage/#nearest-clicked-dialog>
fn nearest_clicked_dialog(event: &MouseEvent, target: &Node) -> Option<DomRoot<HTMLDialogElement>> {
    // > 2. If target is a dialog element, target has an open attribute, target's is modal is true,
    // >    and event's clientX and clientY are outside the bounds of target, then return null.
    if let Some(dialog) = target.downcast::<HTMLDialogElement>() {
        let element = dialog.upcast::<Element>();
        if element.has_attribute(&html5ever::local_name!("open")) &&
            element.state().contains(ElementState::MODAL)
        {
            // A dialog without a box has empty bounds, so every point is outside of it.
            let rect = element.upcast::<Node>().border_box().unwrap_or_default();
            let x = event.ClientX() as f64;
            let y = event.ClientY() as f64;
            if x < rect.min_x().to_f64_px() ||
                x > rect.max_x().to_f64_px() ||
                y < rect.min_y().to_f64_px() ||
                y > rect.max_y().to_f64_px()
            {
                return None;
            }
        }
    }

    // > 3. Let currentNode be target.
    // > 4. While currentNode is not null:
    // > 4.1. If currentNode is a dialog element and currentNode has an open attribute, then return
    // >      currentNode.
    // > 4.2. Set currentNode to currentNode's parent in the flat tree.
    // > 5. Return null.
    target
        .inclusive_ancestors_in_flat_tree()
        .filter_map(DomRoot::downcast::<HTMLDialogElement>)
        .find(|dialog| {
            dialog
                .upcast::<Element>()
                .has_attribute(&html5ever::local_name!("open"))
        })
}

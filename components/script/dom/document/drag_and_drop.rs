/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The HTML drag-and-drop processing model for drags that start and end inside a document:
//! <https://html.spec.whatwg.org/multipage/#drag-and-drop-processing-model>

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use euclid::Point2D;
use js::context::JSContext;
use keyboard_types::Modifiers;
use script_bindings::codegen::GenericBindings::DataTransferBinding::DataTransferMethods;
use script_bindings::codegen::GenericBindings::HTMLElementBinding::HTMLElementMethods;
use script_bindings::codegen::GenericBindings::HTMLImageElementBinding::HTMLImageElementMethods;
use script_bindings::inheritance::Castable;
use script_bindings::root::DomRoot;
use script_bindings::str::DOMString;
use script_traits::ConstellationInputEvent;
use style::Atom;
use style_traits::CSSPixel;

use crate::dom::bindings::root::MutNullableDom;
use crate::dom::datatransfer::DataTransfer;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::inputevent::HitTestResult;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::Node;
use crate::dom::types::{
    DragEvent, Element, HTMLAnchorElement, HTMLElement, HTMLImageElement, Window,
};
use crate::drag_data_store::{DragDataStore, Mode};

/// How far, in CSS pixels along either axis, the pointer has to travel with the primary button
/// held before a press on a draggable element becomes a drag. This is Chrome's threshold on
/// Linux and Windows.
const DRAG_THRESHOLD: f32 = 4.0;

/// <https://html.spec.whatwg.org/multipage/#current-drag-operation>
#[derive(Clone, Copy, MallocSizeOf, PartialEq)]
enum DragOperation {
    None,
    Copy,
    Link,
    Move,
}

impl DragOperation {
    fn as_str(self) -> &'static str {
        match self {
            DragOperation::None => "none",
            DragOperation::Copy => "copy",
            DragOperation::Link => "link",
            DragOperation::Move => "move",
        }
    }

    fn from_drop_effect(drop_effect: &str) -> Self {
        match drop_effect {
            "copy" => DragOperation::Copy,
            "link" => DragOperation::Link,
            "move" => DragOperation::Move,
            _ => DragOperation::None,
        }
    }

    /// The `dropEffect` a `dragenter` or `dragover` event starts with, from the table in
    /// <https://html.spec.whatwg.org/multipage/#dndevents>. Where the table leaves the choice
    /// to platform conventions, this picks what Chrome picks: "move" whenever it is allowed,
    /// and "copy" for "uninitialized" even when dragging a link.
    fn initial_for(effect_allowed: &str) -> Self {
        match effect_allowed {
            "none" => DragOperation::None,
            "link" => DragOperation::Link,
            "move" | "copyMove" | "linkMove" => DragOperation::Move,
            _ => DragOperation::Copy,
        }
    }

    /// The operation a canceled `dragover` selects: its `dropEffect`, when `effectAllowed`
    /// permits it.
    /// <https://html.spec.whatwg.org/multipage/#drag-and-drop-processing-model>
    fn allowed_by(self, effect_allowed: &str) -> Self {
        let allowed = match self {
            DragOperation::None => false,
            DragOperation::Copy => matches!(
                effect_allowed,
                "uninitialized" | "copy" | "copyLink" | "copyMove" | "all"
            ),
            DragOperation::Link => matches!(
                effect_allowed,
                "uninitialized" | "link" | "copyLink" | "linkMove" | "all"
            ),
            DragOperation::Move => matches!(
                effect_allowed,
                "uninitialized" | "move" | "copyMove" | "linkMove" | "all"
            ),
        };
        if allowed { self } else { DragOperation::None }
    }
}

/// The pointer and keyboard state that DND events report.
#[derive(Clone, Copy, MallocSizeOf)]
pub(crate) struct PointerState {
    client_point: Point2D<i32, CSSPixel>,
    page_point: Point2D<i32, CSSPixel>,
    modifiers: Modifiers,
    buttons: u16,
}

impl PointerState {
    pub(crate) fn new(
        hit_test_result: &HitTestResult,
        input_event: &ConstellationInputEvent,
    ) -> Self {
        Self {
            client_point: hit_test_result.point_in_frame.to_i32(),
            page_point: hit_test_result
                .point_relative_to_initial_containing_block
                .to_i32(),
            modifiers: input_event.active_keyboard_modifiers,
            buttons: input_event.pressed_mouse_buttons,
        }
    }
}

#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct DragAndDrop {
    /// The draggable element under a primary button press that has not yet moved far enough
    /// to start a drag.
    drag_candidate: MutNullableDom<Element>,
    /// Where the press on `drag_candidate` happened, in the frame.
    #[no_trace]
    drag_candidate_point: Cell<Option<Point2D<f32, CSSPixel>>>,
    /// The state at the press on `drag_candidate`, which `dragstart` reports.
    #[no_trace]
    drag_candidate_state: Cell<Option<PointerState>>,
    /// <https://html.spec.whatwg.org/multipage/#source-node>, set while a drag is in progress.
    source_node: MutNullableDom<Element>,
    /// <https://html.spec.whatwg.org/multipage/#current-target-element>
    current_target_element: MutNullableDom<Element>,
    /// The drag data store, which each DND event's `DataTransfer` borrows while it is dispatched.
    #[no_trace]
    drag_data_store: RefCell<Option<DragDataStore>>,
    #[no_trace]
    current_drag_operation: Cell<DragOperation>,
    /// Like Chrome, the pointer move that changes the target fires `drag`, `dragenter` and
    /// `dragleave`, and the next one fires `dragover` alone.
    target_just_changed: Cell<bool>,
    /// The most recent pointer state, for ending the drag on a release or Escape.
    #[no_trace]
    last_pointer_state: Cell<Option<PointerState>>,
}

impl DragAndDrop {
    pub(crate) fn new() -> Self {
        Self {
            drag_candidate: Default::default(),
            drag_candidate_point: Default::default(),
            drag_candidate_state: Default::default(),
            source_node: Default::default(),
            current_target_element: Default::default(),
            drag_data_store: Default::default(),
            current_drag_operation: Cell::new(DragOperation::None),
            target_just_changed: Cell::new(false),
            last_pointer_state: Default::default(),
        }
    }

    pub(crate) fn is_dragging(&self) -> bool {
        self.source_node.get().is_some()
    }

    /// Remember the draggable inclusive ancestor of the target of an uncanceled primary button
    /// press, if there is one, so that moving the pointer far enough starts dragging it.
    /// <https://html.spec.whatwg.org/multipage/#drag-and-drop-processing-model>
    pub(crate) fn note_primary_button_down(
        &self,
        target: &Node,
        hit_test_result: &HitTestResult,
        input_event: &ConstellationInputEvent,
    ) {
        // > If the user is attempting to drag an element, the user agent must look for the
        // > nearest ancestor element (including itself) for which the draggable IDL attribute
        // > is true.
        let draggable = target
            .inclusive_ancestors(ShadowIncluding::Yes)
            .filter_map(DomRoot::downcast::<HTMLElement>)
            .find(|element| element.Draggable());
        self.drag_candidate
            .set(draggable.as_ref().map(|element| element.upcast()));
        self.drag_candidate_point
            .set(Some(hit_test_result.point_in_frame));
        self.drag_candidate_state
            .set(Some(PointerState::new(hit_test_result, input_event)));
    }

    pub(crate) fn clear_drag_candidate(&self) {
        self.drag_candidate.set(None);
    }

    /// Start dragging the drag candidate if the pointer has moved past the drag threshold with
    /// the primary button still held. Returns whether a drag started.
    pub(crate) fn maybe_start_drag(
        &self,
        cx: &mut JSContext,
        window: &Window,
        hit_test_result: &HitTestResult,
        input_event: &ConstellationInputEvent,
    ) -> bool {
        let Some(source_node) = self.drag_candidate.get() else {
            return false;
        };
        if input_event.pressed_mouse_buttons & 1 == 0 {
            self.drag_candidate.set(None);
            return false;
        }
        let press_point = self
            .drag_candidate_point
            .get()
            .expect("a drag candidate always has a press point");
        let distance = hit_test_result.point_in_frame - press_point;
        if distance.x.abs() <= DRAG_THRESHOLD && distance.y.abs() <= DRAG_THRESHOLD {
            return false;
        }
        self.drag_candidate.set(None);

        // > Create a drag data store. All the DND events fired subsequently by the steps in
        // > this section must use this drag data store.
        let mut drag_data_store = DragDataStore::new();
        // > If the list of dragged nodes contains an img element, or an a element with an href
        // > attribute, add to the drag data store item list an item whose kind is text, type
        // > string is "text/uri-list", and data is the absolute URL of that element.
        if let Some(anchor) = source_node.downcast::<HTMLAnchorElement>() &&
            let Some(url) = anchor.full_href_url_for_user_interface()
        {
            let url = DOMString::from(url.as_str());
            drag_data_store.set_data(DOMString::from("text/uri-list"), url.clone());
            // Chrome also offers a dragged link's URL as text/plain.
            drag_data_store.set_data(DOMString::from("text/plain"), url);
        } else if let Some(image) = source_node.downcast::<HTMLImageElement>() {
            let current_src = image.CurrentSrc();
            if !current_src.0.is_empty() {
                drag_data_store.set_data(
                    DOMString::from("text/uri-list"),
                    DOMString::from(current_src.0),
                );
            }
        }
        *self.drag_data_store.borrow_mut() = Some(drag_data_store);
        self.source_node.set(Some(&source_node));
        self.current_target_element.set(None);
        self.current_drag_operation.set(DragOperation::None);
        self.target_just_changed.set(false);
        self.last_pointer_state
            .set(Some(PointerState::new(hit_test_result, input_event)));

        // > Fire a DND event named dragstart at the source node. If the event is canceled,
        // > then the drag-and-drop operation should not occur; return.
        let press_state = self
            .drag_candidate_state
            .get()
            .expect("a drag candidate always has a press state");
        let (canceled, _) = self.fire_dnd_event(
            cx,
            window,
            "dragstart",
            &source_node,
            None,
            DragOperation::None,
            press_state,
        );
        if canceled {
            self.reset();
            return false;
        }
        true
    }

    /// Run one iteration of the drag-and-drop processing model for a pointer move to `target`.
    /// The event order follows Chrome's `EventHandler::UpdateDragAndDrop`, which keeps
    /// the target element under the pointer even when `dragenter` is not canceled.
    pub(crate) fn pointer_moved(
        &self,
        cx: &mut JSContext,
        window: &Window,
        target: &Element,
        state: PointerState,
    ) {
        self.last_pointer_state.set(Some(state));
        let source_node = self.source_node.get().expect("a drag is in progress");
        let effect_allowed = self.allowed_effects_state();
        let initial_drop_effect = DragOperation::initial_for(&effect_allowed);

        let previous_target = self.current_target_element.get();
        if previous_target.as_deref() != Some(target) {
            if !self.fire_drag_or_cancel(cx, window, &source_node, state) {
                return;
            }
            self.fire_dnd_event(
                cx,
                window,
                "dragenter",
                target,
                previous_target.as_deref(),
                initial_drop_effect,
                state,
            );
            if let Some(previous_target) = previous_target {
                self.fire_dnd_event(
                    cx,
                    window,
                    "dragleave",
                    &previous_target,
                    Some(target),
                    DragOperation::None,
                    state,
                );
            }
            self.current_target_element.set(Some(target));
            self.current_drag_operation.set(DragOperation::None);
            self.target_just_changed.set(true);
            return;
        }

        if !self.target_just_changed.replace(false) &&
            !self.fire_drag_or_cancel(cx, window, &source_node, state)
        {
            return;
        }

        // > If the dragover event is not canceled, ... reset the current drag operation to
        // > "none". Otherwise, set the current drag operation based on the values of the
        // > effectAllowed and dropEffect attributes of the DragEvent object's dataTransfer.
        let (canceled, drop_effect) = self.fire_dnd_event(
            cx,
            window,
            "dragover",
            target,
            None,
            initial_drop_effect,
            state,
        );
        let operation = if canceled {
            DragOperation::from_drop_effect(&drop_effect).allowed_by(&effect_allowed)
        } else {
            DragOperation::None
        };
        self.current_drag_operation.set(operation);
    }

    /// The primary button was released: drop on the current target element if the last
    /// `dragover` accepted the drag, then end the drag.
    pub(crate) fn pointer_released(&self, cx: &mut JSContext, window: &Window) {
        let state = self
            .last_pointer_state
            .get()
            .expect("a drag in progress has a pointer state");
        if let Some(target) = self.current_target_element.get() {
            if self.current_drag_operation.get() == DragOperation::None {
                // > If the current drag operation is "none" ... fire a DND event named
                // > dragleave at it.
                self.fire_dnd_event(
                    cx,
                    window,
                    "dragleave",
                    &target,
                    None,
                    DragOperation::None,
                    state,
                );
            } else {
                // > Otherwise, ... fire a DND event named drop at it. If the event is
                // > canceled, set the current drag operation to the value of the dropEffect
                // > attribute of the DragEvent object's dataTransfer object as it stood after
                // > the event dispatch finished. Otherwise, ... reset the current drag operation
                // > to "none".
                let (canceled, drop_effect) = self.fire_dnd_event(
                    cx,
                    window,
                    "drop",
                    &target,
                    None,
                    self.current_drag_operation.get(),
                    state,
                );
                self.current_drag_operation.set(if canceled {
                    DragOperation::from_drop_effect(&drop_effect)
                } else {
                    DragOperation::None
                });
            }
        }
        self.end_drag(cx, window, state);
    }

    /// The pointer left the viewport: the drag no longer has a target in this document.
    pub(crate) fn pointer_left_viewport(&self, cx: &mut JSContext, window: &Window) {
        let state = self
            .last_pointer_state
            .get()
            .expect("a drag in progress has a pointer state");
        if let Some(target) = self.current_target_element.take() {
            self.fire_dnd_event(
                cx,
                window,
                "dragleave",
                &target,
                None,
                DragOperation::None,
                state,
            );
        }
        self.current_drag_operation.set(DragOperation::None);
    }

    /// <https://html.spec.whatwg.org/multipage/#drag-and-drop-processing-model>
    /// > If the user ends the drag-and-drop operation (e.g. by pressing the Escape key) ...
    /// > the drag operation was a failure.
    pub(crate) fn cancel(&self, cx: &mut JSContext, window: &Window) {
        let state = self
            .last_pointer_state
            .get()
            .expect("a drag in progress has a pointer state");
        self.current_drag_operation.set(DragOperation::None);
        if let Some(target) = self.current_target_element.get() {
            self.fire_dnd_event(
                cx,
                window,
                "dragleave",
                &target,
                None,
                DragOperation::None,
                state,
            );
        }
        self.end_drag(cx, window, state);
    }

    /// Fire `drag` at the source node, canceling the whole drag if the event is canceled.
    /// Returns whether the drag continues.
    fn fire_drag_or_cancel(
        &self,
        cx: &mut JSContext,
        window: &Window,
        source_node: &Element,
        state: PointerState,
    ) -> bool {
        let (canceled, _) = self.fire_dnd_event(
            cx,
            window,
            "drag",
            source_node,
            None,
            DragOperation::None,
            state,
        );
        if canceled {
            self.cancel(cx, window);
        }
        !canceled
    }

    /// > Fire a DND event named dragend at the source node.
    fn end_drag(&self, cx: &mut JSContext, window: &Window, state: PointerState) {
        let source_node = self.source_node.get().expect("a drag is in progress");
        self.fire_dnd_event(
            cx,
            window,
            "dragend",
            &source_node,
            None,
            self.current_drag_operation.get(),
            state,
        );
        self.reset();
    }

    fn reset(&self) {
        self.source_node.set(None);
        self.current_target_element.set(None);
        self.drag_data_store.borrow_mut().take();
        self.current_drag_operation.set(DragOperation::None);
        self.target_just_changed.set(false);
        self.last_pointer_state.set(None);
    }

    fn allowed_effects_state(&self) -> String {
        self.drag_data_store
            .borrow()
            .as_ref()
            .expect("a drag in progress has a drag data store")
            .allowed_effects_state()
            .to_owned()
    }

    /// <https://html.spec.whatwg.org/multipage/#fire-a-dnd-event>
    ///
    /// Returns whether the event was canceled and the `dropEffect` after dispatch.
    #[expect(clippy::too_many_arguments)]
    fn fire_dnd_event(
        &self,
        cx: &mut JSContext,
        window: &Window,
        event_type: &str,
        target: &Element,
        related_target: Option<&Element>,
        drop_effect: DragOperation,
        state: PointerState,
    ) -> (bool, String) {
        let mut drag_data_store = self
            .drag_data_store
            .borrow_mut()
            .take()
            .expect("a drag in progress has a drag data store");
        // > If e is dragstart, then set the drag data store mode to the read/write mode ...
        // > If e is drop, set the drag data store mode to the read-only mode.
        drag_data_store.set_mode(match event_type {
            "dragstart" => Mode::ReadWrite,
            "drop" => Mode::ReadOnly,
            _ => Mode::Protected,
        });
        let effect_allowed = DOMString::from(drag_data_store.allowed_effects_state());

        // > Let dataTransfer be a newly created DataTransfer object associated with the given
        // > drag data store.
        let shared_store = Rc::new(RefCell::new(Some(drag_data_store)));
        let data_transfer = DataTransfer::new(cx, window, Rc::clone(&shared_store));
        // > Set the effectAllowed attribute to the drag data store's drag data store allowed
        // > effects state. Set the dropEffect attribute to "none" if e is dragstart, drag, or
        // > dragleave; to the value corresponding to the current drag operation if e is drop or
        // > dragend; and to a value based on the effectAllowed attribute's value and the
        // > drag-and-drop source, as given by the table in #dndevents, otherwise.
        data_transfer.initialize_effects(DOMString::from(drop_effect.as_str()), effect_allowed);

        // > Create a DragEvent object and initialize it to have the given name e, to bubble,
        // > to be cancelable unless e is dragleave or dragend, and to have the detail
        // > attribute initialized to zero ...
        let cancelable = if matches!(event_type, "dragleave" | "dragend") {
            EventCancelable::NotCancelable
        } else {
            EventCancelable::Cancelable
        };
        let event = DragEvent::new(
            cx,
            window,
            None,
            Atom::from(event_type),
            EventBubbles::Bubbles,
            cancelable,
            Some(window),
            0,
            state.client_point,
            state.client_point,
            state.page_point,
            state.modifiers,
            0,
            state.buttons,
            related_target.map(|element| element.upcast()),
            Some(&data_transfer),
        );
        let event = event.upcast::<Event>();
        event.set_composed(true);
        let not_canceled = event.fire(cx, target.upcast());

        let mut drag_data_store = shared_store
            .borrow_mut()
            .take()
            .expect("only the drag-and-drop processing model breaks the association");
        // > If e is dragstart, ... the drag data store allowed effects state must be set to
        // > the value of the effectAllowed attribute ...
        if event_type == "dragstart" {
            drag_data_store.set_allowed_effects_state(data_transfer.EffectAllowed().to_string());
        }
        // > Set the drag data store mode back to the protected mode if it was changed in the
        // > first step. Break the association between dataTransfer and the drag data store.
        drag_data_store.set_mode(Mode::Protected);
        *self.drag_data_store.borrow_mut() = Some(drag_data_store);

        (!not_canceled, data_transfer.DropEffect().to_string())
    }
}

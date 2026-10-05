/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use euclid::Point2D;
use js::context::JSContext;
use js::rust::HandleObject;
use keyboard_types::Modifiers;
use script_bindings::reflector::reflect_dom_object_with_proto_and_cx;
use style::Atom;
use style_traits::CSSPixel;

use crate::dom::bindings::codegen::Bindings::DragEventBinding;
use crate::dom::bindings::codegen::Bindings::DragEventBinding::DragEventMethods;
use crate::dom::bindings::codegen::Bindings::MouseEventBinding::MouseEventMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::datatransfer::DataTransfer;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::mouseevent::MouseEvent;
use crate::dom::window::Window;

/// <https://html.spec.whatwg.org/multipage/#the-dragevent-interface>
#[dom_struct]
pub(crate) struct DragEvent {
    mouseevent: MouseEvent,
    data_transfer: MutNullableDom<DataTransfer>,
}

impl DragEvent {
    fn new_inherited() -> DragEvent {
        DragEvent {
            mouseevent: MouseEvent::new_inherited(),
            data_transfer: MutNullableDom::new(None),
        }
    }

    pub(crate) fn new_uninitialized(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<DragEvent> {
        reflect_dom_object_with_proto_and_cx(
            Box::new(DragEvent::new_inherited()),
            window,
            proto,
            cx,
        )
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        event_type: Atom,
        can_bubble: EventBubbles,
        cancelable: EventCancelable,
        view: Option<&Window>,
        detail: i32,
        screen_point: Point2D<i32, CSSPixel>,
        client_point: Point2D<i32, CSSPixel>,
        page_point: Point2D<i32, CSSPixel>,
        modifiers: Modifiers,
        button: i16,
        buttons: u16,
        related_target: Option<&EventTarget>,
        data_transfer: Option<&DataTransfer>,
    ) -> DomRoot<DragEvent> {
        let event = DragEvent::new_uninitialized(cx, window, proto);
        event.mouseevent.initialize_mouse_event(
            event_type,
            can_bubble,
            cancelable,
            view,
            detail,
            screen_point,
            client_point,
            page_point,
            modifiers,
            button,
            buttons,
            related_target,
            None,
        );
        event.data_transfer.set(data_transfer);
        event
    }
}

impl DragEventMethods<crate::DomTypeHolder> for DragEvent {
    /// <https://html.spec.whatwg.org/multipage/#dom-dragevent-datatransfer>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        event_type: DOMString,
        init: &DragEventBinding::DragEventInit,
    ) -> DomRoot<DragEvent> {
        let mouse_init = &init.parent;
        let event_init = &mouse_init.parent.parent.parent;
        let scroll_offset = window.scroll_offset();
        let page_point = Point2D::new(
            scroll_offset.x as i32 + mouse_init.clientX,
            scroll_offset.y as i32 + mouse_init.clientY,
        );
        let event = DragEvent::new(
            cx,
            window,
            proto,
            event_type.into(),
            EventBubbles::from(event_init.bubbles),
            EventCancelable::from(event_init.cancelable),
            mouse_init.parent.parent.view.as_deref(),
            mouse_init.parent.parent.detail,
            Point2D::new(mouse_init.screenX, mouse_init.screenY),
            Point2D::new(mouse_init.clientX, mouse_init.clientY),
            page_point,
            mouse_init.parent.modifiers(),
            mouse_init.button,
            mouse_init.buttons,
            mouse_init.relatedTarget.as_deref(),
            init.dataTransfer.as_deref(),
        );
        event.upcast::<Event>().set_composed(event_init.composed);
        event
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-dragevent-datatransfer>
    fn GetDataTransfer(&self) -> Option<DomRoot<DataTransfer>> {
        self.data_transfer.get()
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    fn IsTrusted(&self) -> bool {
        self.mouseevent.IsTrusted()
    }
}

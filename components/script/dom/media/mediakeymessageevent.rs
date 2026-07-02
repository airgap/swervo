/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::ptr;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsapi::{Heap, JSObject};
use js::typedarray::{ArrayBufferU8, HeapArrayBuffer};
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_bindings::trace::RootedTraceableBox;
use stylo_atoms::Atom;

use crate::dom::bindings::buffer_source::{BufferSource, HeapBufferSource, create_buffer_source};
use crate::dom::bindings::codegen::Bindings::EventBinding::EventMethods;
use crate::dom::bindings::codegen::Bindings::MediaKeyMessageEventBinding::MediaKeyMessageEventMethods;
use crate::dom::bindings::codegen::Bindings::MediaKeySystemAccessBinding::MediaKeyMessageType;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::event::Event;
use crate::dom::globalscope::GlobalScope;
use crate::script_runtime::{CanGc, JSContext as SafeJSContext};

/// <https://w3c.github.io/encrypted-media/#mediakeymessageevent>
#[dom_struct]
pub(crate) struct MediaKeyMessageEvent {
    event: Event,
    message_type: MediaKeyMessageType,
    #[ignore_malloc_size_of = "mozjs"]
    message: DomRefCell<HeapBufferSource<ArrayBufferU8>>,
}

impl MediaKeyMessageEvent {
    /// Build a `MediaKeyMessageEvent`, copying `message` into a fresh `ArrayBuffer`.
    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        type_: Atom,
        message_type: MediaKeyMessageType,
        message: &[u8],
        can_gc: CanGc,
    ) -> DomRoot<MediaKeyMessageEvent> {
        rooted!(&in(cx) let mut array = ptr::null_mut::<JSObject>());
        let buffer_source = if message.is_empty() {
            HeapBufferSource::<ArrayBufferU8>::default()
        } else {
            create_buffer_source::<ArrayBufferU8>(cx.into(), message, array.handle_mut(), can_gc)
                .expect("Creating an ArrayBuffer from the license message should never fail");
            HeapBufferSource::<ArrayBufferU8>::new(BufferSource::ArrayBuffer(Heap::boxed(
                *array.handle(),
            )))
        };

        let ev = reflect_dom_object_with_proto(
            Box::new(MediaKeyMessageEvent {
                event: Event::new_inherited(),
                message_type,
                message: DomRefCell::new(buffer_source),
            }),
            global,
            None,
            can_gc,
        );
        ev.upcast::<Event>().init_event(type_, false, false);
        ev
    }
}

impl MediaKeyMessageEventMethods<crate::DomTypeHolder> for MediaKeyMessageEvent {
    /// <https://w3c.github.io/encrypted-media/#dom-mediakeymessageevent-messagetype>
    fn MessageType(&self) -> MediaKeyMessageType {
        self.message_type
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeymessageevent-message>
    fn GetMessage(&self, _cx: SafeJSContext) -> Option<RootedTraceableBox<HeapArrayBuffer>> {
        self.message.borrow().typed_array_to_option()
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    fn IsTrusted(&self) -> bool {
        self.upcast::<Event>().IsTrusted()
    }
}

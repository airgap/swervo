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
use crate::dom::bindings::codegen::Bindings::MediaEncryptedEventBinding::MediaEncryptedEventMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::event::Event;
use crate::dom::globalscope::GlobalScope;
use crate::script_runtime::{CanGc, JSContext as SafeJSContext};

/// <https://w3c.github.io/encrypted-media/#mediaencryptedevent>
#[dom_struct]
pub(crate) struct MediaEncryptedEvent {
    event: Event,
    init_data_type: DOMString,
    #[ignore_malloc_size_of = "mozjs"]
    init_data: DomRefCell<HeapBufferSource<ArrayBufferU8>>,
}

impl MediaEncryptedEvent {
    /// Build a `MediaEncryptedEvent`, copying `init_data` into a fresh `ArrayBuffer`.
    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        type_: Atom,
        init_data_type: DOMString,
        init_data: &[u8],
        can_gc: CanGc,
    ) -> DomRoot<MediaEncryptedEvent> {
        rooted!(&in(cx) let mut array = ptr::null_mut::<JSObject>());
        let buffer_source = if init_data.is_empty() {
            HeapBufferSource::<ArrayBufferU8>::default()
        } else {
            create_buffer_source::<ArrayBufferU8>(cx.into(), init_data, array.handle_mut(), can_gc)
                .expect("Creating an ArrayBuffer from init data should never fail");
            HeapBufferSource::<ArrayBufferU8>::new(BufferSource::ArrayBuffer(Heap::boxed(
                *array.handle(),
            )))
        };

        let ev = reflect_dom_object_with_proto(
            Box::new(MediaEncryptedEvent {
                event: Event::new_inherited(),
                init_data_type,
                init_data: DomRefCell::new(buffer_source),
            }),
            global,
            None,
            can_gc,
        );
        ev.upcast::<Event>().init_event(type_, false, false);
        ev
    }
}

impl MediaEncryptedEventMethods<crate::DomTypeHolder> for MediaEncryptedEvent {
    /// <https://w3c.github.io/encrypted-media/#dom-mediaencryptedevent-initdatatype>
    fn InitDataType(&self) -> DOMString {
        self.init_data_type.clone()
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediaencryptedevent-initdata>
    fn GetInitData(&self, _cx: SafeJSContext) -> Option<RootedTraceableBox<HeapArrayBuffer>> {
        self.init_data.borrow().typed_array_to_option()
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    fn IsTrusted(&self) -> bool {
        self.upcast::<Event>().IsTrusted()
    }
}

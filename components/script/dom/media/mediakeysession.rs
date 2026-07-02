/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Encrypted Media Extensions `MediaKeySession` — brick 1 (framework skeleton). The Clear Key
//! session logic (generateRequest→message, update→keys, keyStatuses) lands in brick 2; for now
//! the operations return resolved promises so the API surface is exercisable.

use std::rc::Rc;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object;

use crate::dom::bindings::codegen::Bindings::MediaKeySessionBinding::MediaKeySessionMethods;
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::Promise;
use crate::script_runtime::CanGc;

#[dom_struct]
pub(crate) struct MediaKeySession {
    eventtarget: EventTarget,
    session_id: DomRefCell<DOMString>,
}

impl MediaKeySession {
    fn new_inherited() -> MediaKeySession {
        MediaKeySession {
            eventtarget: EventTarget::new_inherited(),
            session_id: DomRefCell::new(DOMString::new()),
        }
    }

    pub(crate) fn new(global: &GlobalScope, can_gc: CanGc) -> DomRoot<MediaKeySession> {
        reflect_dom_object(Box::new(MediaKeySession::new_inherited()), global, can_gc)
    }
}

impl MediaKeySessionMethods<crate::DomTypeHolder> for MediaKeySession {
    fn SessionId(&self) -> DOMString {
        self.session_id.borrow().clone()
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-generaterequest>
    fn GenerateRequest(
        &self,
        cx: &mut JSContext,
        _init_data_type: DOMString,
        _init_data: ArrayBufferViewOrArrayBuffer,
    ) -> Rc<Promise> {
        // brick 2: parse init data, fire a `message` event carrying the Clear Key request.
        let mut realm = CurrentRealm::assert(cx);
        let promise = Promise::new_in_realm(&mut realm);
        promise.resolve_native_with_cx(cx, &());
        promise
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-update>
    fn Update(&self, cx: &mut JSContext, _response: ArrayBufferViewOrArrayBuffer) -> Rc<Promise> {
        // brick 2: parse the JWK key set, store keys, fire `keystatuseschange`.
        let mut realm = CurrentRealm::assert(cx);
        let promise = Promise::new_in_realm(&mut realm);
        promise.resolve_native_with_cx(cx, &());
        promise
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-close>
    fn Close(&self, cx: &mut JSContext) -> Rc<Promise> {
        let mut realm = CurrentRealm::assert(cx);
        let promise = Promise::new_in_realm(&mut realm);
        promise.resolve_native_with_cx(cx, &());
        promise
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Encrypted Media Extensions `MediaKeySystemAccess` — brick 1 (framework skeleton), gated behind
//! `dom_eme_enabled`. Clear Key only for now.

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::MediaKeySystemAccessBinding::MediaKeySystemAccessMethods;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;
use crate::dom::media::mediakeys::MediaKeys;
use crate::dom::promise::{Promise, RootedPromise};

#[dom_struct]
pub(crate) struct MediaKeySystemAccess {
    reflector_: Reflector,
    key_system: DOMString,
}

impl MediaKeySystemAccess {
    fn new_inherited(key_system: DOMString) -> MediaKeySystemAccess {
        MediaKeySystemAccess {
            reflector_: Reflector::new(),
            key_system,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        key_system: DOMString,
    ) -> DomRoot<MediaKeySystemAccess> {
        reflect_dom_object(
            cx,
            Box::new(MediaKeySystemAccess::new_inherited(key_system)),
            global,
        )
    }
}

impl MediaKeySystemAccessMethods<crate::DomTypeHolder> for MediaKeySystemAccess {
    fn KeySystem(&self) -> DOMString {
        self.key_system.clone()
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysystemaccess-createmediakeys>
    fn CreateMediaKeys(&self, cx: &mut JSContext) -> RootedPromise {
        let mut realm = CurrentRealm::assert(cx);
        let promise = Promise::new_in_realm(&mut realm);
        let media_keys = MediaKeys::new(&mut realm, &self.global());
        promise.resolve_native(&mut realm, &media_keys);
        promise
    }
}

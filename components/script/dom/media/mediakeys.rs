/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Encrypted Media Extensions `MediaKeys` — brick 1 (framework skeleton).

use dom_struct::dom_struct;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::MediaKeySystemAccessBinding::MediaKeySessionType;
use crate::dom::bindings::codegen::Bindings::MediaKeysBinding::MediaKeysMethods;
use crate::dom::bindings::error::Fallible;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::globalscope::GlobalScope;
use crate::dom::media::mediakeysession::MediaKeySession;
use crate::script_runtime::CanGc;

#[dom_struct]
pub(crate) struct MediaKeys {
    reflector_: Reflector,
}

impl MediaKeys {
    fn new_inherited() -> MediaKeys {
        MediaKeys {
            reflector_: Reflector::new(),
        }
    }

    pub(crate) fn new(global: &GlobalScope, can_gc: CanGc) -> DomRoot<MediaKeys> {
        reflect_dom_object(Box::new(MediaKeys::new_inherited()), global, can_gc)
    }
}

impl MediaKeysMethods<crate::DomTypeHolder> for MediaKeys {
    /// <https://w3c.github.io/encrypted-media/#dom-mediakeys-createsession>
    fn CreateSession(
        &self,
        _session_type: MediaKeySessionType,
    ) -> Fallible<DomRoot<MediaKeySession>> {
        // brick 2 records the session type + tracks the session on the MediaKeys.
        Ok(MediaKeySession::new(&self.global(), CanGc::deprecated_note()))
    }
}

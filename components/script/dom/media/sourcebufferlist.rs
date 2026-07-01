/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions `SourceBufferList` — Phase 1 scaffold.

use dom_struct::dom_struct;
use script_bindings::cell::DomRefCell;

use script_bindings::reflector::reflect_dom_object;

use crate::dom::bindings::codegen::Bindings::SourceBufferListBinding::SourceBufferListMethods;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::media::sourcebuffer::SourceBuffer;
use crate::script_runtime::CanGc;

#[dom_struct]
pub(crate) struct SourceBufferList {
    eventtarget: EventTarget,
    buffers: DomRefCell<Vec<Dom<SourceBuffer>>>,
}

impl SourceBufferList {
    fn new_inherited() -> SourceBufferList {
        SourceBufferList {
            eventtarget: EventTarget::new_inherited(),
            buffers: DomRefCell::new(vec![]),
        }
    }

    pub(crate) fn new(global: &GlobalScope, can_gc: CanGc) -> DomRoot<SourceBufferList> {
        reflect_dom_object(Box::new(SourceBufferList::new_inherited()), global, can_gc)
    }
}

impl SourceBufferListMethods<crate::DomTypeHolder> for SourceBufferList {
    fn Length(&self) -> u32 {
        self.buffers.borrow().len() as u32
    }

    fn IndexedGetter(&self, index: u32) -> Option<DomRoot<SourceBuffer>> {
        self.buffers
            .borrow()
            .get(index as usize)
            .map(|b| DomRoot::from_ref(&**b))
    }
}

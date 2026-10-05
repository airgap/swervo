/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions `SourceBufferList` — Phase 1 scaffold.

use dom_struct::dom_struct;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::SourceBufferListBinding::SourceBufferListMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
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

    /// Append a SourceBuffer to the list (used by `MediaSource.addSourceBuffer`) and queue the
    /// `addsourcebuffer` event at the list.
    pub(crate) fn add(&self, source_buffer: &SourceBuffer) {
        self.buffers.borrow_mut().push(Dom::from_ref(source_buffer));
        self.queue_event("addsourcebuffer");
    }

    /// The presentation times every SourceBuffer in the list holds: the intersection of their
    /// `buffered` ranges, which is what the media element can play
    /// (<https://w3c.github.io/media-source/#htmlmediaelement-extensions-buffered>).
    pub(crate) fn buffered_intersection(&self) -> Vec<(f64, f64)> {
        let buffers = self.buffers.borrow();
        let mut buffers = buffers.iter();
        let Some(first) = buffers.next() else {
            return vec![];
        };
        buffers.fold(first.buffered_ranges(), |intersection, buffer| {
            intersect_ranges(&intersection, &buffer.buffered_ranges())
        })
    }

    /// Queue a task that fires a named event at this SourceBufferList.
    fn queue_event(&self, name: &'static str) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(sbl_event: move |cx| {
                this.root()
                    .upcast::<EventTarget>()
                    .fire_event(cx, Atom::from(name));
            }));
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

/// Intersect two sorted lists of disjoint `[start, end)` ranges.
fn intersect_ranges(a: &[(f64, f64)], b: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let (mut i, mut j) = (0, 0);
    let mut intersection = vec![];
    while i < a.len() && j < b.len() {
        let start = a[i].0.max(b[j].0);
        let end = a[i].1.min(b[j].1);
        if start < end {
            intersection.push((start, end));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    intersection
}

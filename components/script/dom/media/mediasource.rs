/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions `MediaSource` — Phase 1 scaffold, gated behind
//! `dom_mediasource_enabled`. The media operations (`addSourceBuffer`/`endOfStream`) throw
//! `NotSupported` until Phase 2 wires the GStreamer append pipeline, so the interface exists and
//! feature-detection stays honest.

use std::cell::Cell;

use dom_struct::dom_struct;
use js::rust::HandleObject;

use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{
    EndOfStreamError, MediaSourceMethods, ReadyState,
};
use script_bindings::reflector::reflect_dom_object_with_proto;

use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::media::sourcebuffer::SourceBuffer;
use crate::dom::media::sourcebufferlist::SourceBufferList;
use crate::dom::window::Window;
use crate::script_runtime::CanGc;

#[dom_struct]
pub(crate) struct MediaSource {
    eventtarget: EventTarget,
    source_buffers: Dom<SourceBufferList>,
    active_source_buffers: Dom<SourceBufferList>,
    ready_state: Cell<ReadyState>,
    duration: Cell<f64>,
}

impl MediaSource {
    fn new_inherited(
        source_buffers: &SourceBufferList,
        active_source_buffers: &SourceBufferList,
    ) -> MediaSource {
        MediaSource {
            eventtarget: EventTarget::new_inherited(),
            source_buffers: Dom::from_ref(source_buffers),
            active_source_buffers: Dom::from_ref(active_source_buffers),
            ready_state: Cell::new(ReadyState::Closed),
            duration: Cell::new(f64::NAN),
        }
    }

    fn new_with_proto(
        global: &GlobalScope,
        proto: Option<HandleObject>,
        can_gc: CanGc,
    ) -> DomRoot<MediaSource> {
        let source_buffers = SourceBufferList::new(global, can_gc);
        let active_source_buffers = SourceBufferList::new(global, can_gc);
        reflect_dom_object_with_proto(
            Box::new(MediaSource::new_inherited(
                &source_buffers,
                &active_source_buffers,
            )),
            global,
            proto,
            can_gc,
        )
    }
}

impl MediaSourceMethods<crate::DomTypeHolder> for MediaSource {
    /// <https://w3c.github.io/media-source/#dom-mediasource-mediasource>
    fn Constructor(
        window: &Window,
        proto: Option<HandleObject>,
        can_gc: CanGc,
    ) -> Fallible<DomRoot<MediaSource>> {
        Ok(MediaSource::new_with_proto(&window.global(), proto, can_gc))
    }

    fn SourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&self.source_buffers)
    }
    fn ActiveSourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&self.active_source_buffers)
    }
    fn ReadyState(&self) -> ReadyState {
        self.ready_state.get()
    }
    fn GetDuration(&self) -> Fallible<f64> {
        Ok(self.duration.get())
    }
    fn SetDuration(&self, value: f64) -> ErrorResult {
        self.duration.set(value);
        Ok(())
    }

    /// Phase 2 wires codec validation + the SourceBuffer append pipeline.
    fn AddSourceBuffer(&self, _type_: DOMString) -> Fallible<DomRoot<SourceBuffer>> {
        Err(Error::NotSupported(None))
    }
    fn RemoveSourceBuffer(&self, _buffer: &SourceBuffer) -> ErrorResult {
        Err(Error::NotSupported(None))
    }
    fn EndOfStream(&self, _error: Option<EndOfStreamError>) -> ErrorResult {
        Err(Error::NotSupported(None))
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-istypesupported>
    /// Phase 1 heuristic; Phase 2 will consult the GStreamer registry scanner.
    fn IsTypeSupported(_window: &Window, type_: DOMString) -> bool {
        let t = type_.to_ascii_lowercase();
        !t.is_empty() && (t.starts_with("video/") || t.starts_with("audio/"))
    }
}

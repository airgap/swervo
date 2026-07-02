/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions `MediaSource` — Phase 1 scaffold, gated behind
//! `dom_mediasource_enabled`. The media operations (`addSourceBuffer`/`endOfStream`) throw
//! `NotSupported` until Phase 2 wires the GStreamer append pipeline, so the interface exists and
//! feature-detection stays honest.

use std::cell::Cell;
use std::ffi::CString;

use dom_struct::dom_struct;
use js::rust::HandleObject;

use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{
    EndOfStreamError, MediaSourceMethods, ReadyState,
};
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_media::{ServoMedia, SupportsMediaType};
use stylo_atoms::Atom;

use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::html::htmlmediaelement::HTMLMediaElement;
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
    /// The `HTMLMediaElement` this MediaSource is attached to, once `URL.createObjectURL`'d and
    /// loaded. `None` while `readyState` is `"closed"`.
    media_element: MutNullableDom<HTMLMediaElement>,
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
            media_element: Default::default(),
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

    /// Record the `HTMLMediaElement` this MediaSource has been attached to.
    pub(crate) fn set_media_element(&self, element: &HTMLMediaElement) {
        self.media_element.set(Some(element));
    }

    /// The `HTMLMediaElement` this MediaSource is attached to, if any.
    pub(crate) fn media_element(&self) -> Option<DomRoot<HTMLMediaElement>> {
        self.media_element.get()
    }

    /// Whether `readyState` is `"open"`.
    pub(crate) fn is_open(&self) -> bool {
        self.ready_state.get() == ReadyState::Open
    }

    /// Transition to `"open"` and fire `sourceopen`, per the MSE attach steps. Called from a
    /// media-element task once the element has resolved this MediaSource's blob URL.
    pub(crate) fn open_and_fire_sourceopen(&self, cx: &mut js::context::JSContext) {
        self.ready_state.set(ReadyState::Open);
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from("sourceopen"));
    }

    /// Queue a task that fires a named event at this MediaSource.
    fn queue_event(&self, name: &'static str) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_event: move |cx| {
                this.root()
                    .upcast::<EventTarget>()
                    .fire_event(cx, Atom::from(name));
            }));
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

    /// <https://w3c.github.io/media-source/#dom-mediasource-addsourcebuffer>
    fn AddSourceBuffer(&self, type_: DOMString) -> Fallible<DomRoot<SourceBuffer>> {
        // Step 1. If type is an empty string, throw a TypeError.
        if type_.is_empty() {
            return Err(Error::Type(
                CString::new("The type provided is empty").unwrap(),
            ));
        }
        // Step 2. If type is not supported, throw a NotSupportedError.
        if ServoMedia::get().can_play_type(&type_.str()) == SupportsMediaType::No {
            return Err(Error::NotSupported(None));
        }
        // Step 4. If readyState is not "open", throw an InvalidStateError.
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        // Steps 5-7. Create the SourceBuffer, add it to sourceBuffers (which queues the
        // `addsourcebuffer` event at the list).
        let source_buffer = SourceBuffer::new(&self.global(), self, CanGc::deprecated_note());
        self.source_buffers.add(&source_buffer);
        Ok(source_buffer)
    }
    fn RemoveSourceBuffer(&self, _buffer: &SourceBuffer) -> ErrorResult {
        Err(Error::NotSupported(None))
    }
    /// <https://w3c.github.io/media-source/#dom-mediasource-endofstream>
    fn EndOfStream(&self, _error: Option<EndOfStreamError>) -> ErrorResult {
        // Step 1. If readyState is not "open", throw an InvalidStateError.
        if !self.is_open() {
            return Err(Error::InvalidState(None));
        }
        // Steps 2-4. Transition to "ended" and signal end-of-stream to the player.
        self.ready_state.set(ReadyState::Ended);
        if let Some(element) = self.media_element.get() &&
            let Some(player) = element.get_player()
        {
            let _ = player.lock().unwrap().end_of_stream();
        }
        self.queue_event("sourceended");
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-istypesupported>
    /// Consults the media backend (GStreamer registry) for real codec/container support.
    fn IsTypeSupported(_window: &Window, type_: DOMString) -> bool {
        if type_.is_empty() {
            return false;
        }
        ServoMedia::get().can_play_type(&type_.str()) != SupportsMediaType::No
    }
}

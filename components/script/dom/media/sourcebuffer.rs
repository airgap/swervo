/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions `SourceBuffer` — Phase 1 scaffold (state only). `appendBuffer` /
//! `remove` and the GStreamer append pipeline land in Phase 2.

use std::cell::Cell;
use std::ffi::CString;

use dom_struct::dom_struct;
use script_bindings::reflector::reflect_dom_object;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::SourceBufferBinding::{AppendMode, SourceBufferMethods};
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::media::mediasource::MediaSource;
use crate::dom::timeranges::{TimeRanges, TimeRangesContainer};
use js::context::JSContext;
use crate::script_runtime::CanGc;

#[dom_struct]
pub(crate) struct SourceBuffer {
    eventtarget: EventTarget,
    mode: Cell<AppendMode>,
    updating: Cell<bool>,
    /// Running total of bytes appended, used to keep the player's input size in sync so its
    /// (percent-based) buffering query and playback progress work.
    total_bytes: Cell<u64>,
    timestamp_offset: Cell<f64>,
    append_window_start: Cell<f64>,
    append_window_end: Cell<f64>,
    media_source: Dom<MediaSource>,
}

impl SourceBuffer {
    fn new_inherited(media_source: &MediaSource) -> SourceBuffer {
        SourceBuffer {
            eventtarget: EventTarget::new_inherited(),
            mode: Cell::new(AppendMode::Segments),
            updating: Cell::new(false),
            total_bytes: Cell::new(0),
            timestamp_offset: Cell::new(0.0),
            append_window_start: Cell::new(0.0),
            append_window_end: Cell::new(f64::INFINITY),
            media_source: Dom::from_ref(media_source),
        }
    }

    pub(crate) fn new(
        global: &GlobalScope,
        media_source: &MediaSource,
        can_gc: CanGc,
    ) -> DomRoot<SourceBuffer> {
        reflect_dom_object(
            Box::new(SourceBuffer::new_inherited(media_source)),
            global,
            can_gc,
        )
    }

    /// Queue a task that fires a named event at this SourceBuffer.
    fn queue_event(&self, name: &'static str) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_sb_event: move |cx| {
                this.root()
                    .upcast::<EventTarget>()
                    .fire_event(cx, Atom::from(name));
            }));
    }

    /// The async segment of `appendBuffer`: push the bytes into the attached element's player,
    /// then clear `updating` and fire `update` + `updateend`.
    fn finish_append(&self, cx: &mut js::context::JSContext, bytes: Vec<u8>) {
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from("updatestart"));

        let total = self.total_bytes.get().saturating_add(bytes.len() as u64);
        self.total_bytes.set(total);
        if let Some(element) = self.media_source.media_element() &&
            let Some(player) = element.get_player()
        {
            let player = player.lock().unwrap();
            let _ = player.set_input_size(total);
            if let Err(error) = player.push_data(bytes) {
                warn!("MSE appendBuffer push_data failed: {error:?}");
            }
        }

        self.updating.set(false);
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from("update"));
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from("updateend"));
    }
}

impl SourceBufferMethods<crate::DomTypeHolder> for SourceBuffer {
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
    fn AppendBuffer(&self, data: ArrayBufferViewOrArrayBuffer) -> ErrorResult {
        // Steps 1-2. Throw InvalidStateError if updating, or the parent MediaSource is not "open".
        if self.updating.get() || !self.media_source.is_open() {
            return Err(Error::InvalidState(None));
        }
        // Copy out the bytes to append.
        let bytes = match data {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => view.to_vec(),
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => buffer.to_vec(),
        };
        // Step 5. Set updating to true and run the append asynchronously.
        self.updating.set(true);
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_append_buffer: move |cx| {
                this.root().finish_append(cx, bytes);
            }));
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn Abort(&self) -> ErrorResult {
        // If the parent MediaSource is not "open", throw InvalidStateError.
        if !self.media_source.is_open() {
            return Err(Error::InvalidState(None));
        }
        // Abort any in-progress append: reset updating and fire abort + updateend.
        if self.updating.get() {
            self.updating.set(false);
            self.queue_event("abort");
            self.queue_event("updateend");
        }
        // Reset the append window to its defaults.
        self.append_window_start.set(0.0);
        self.append_window_end.set(f64::INFINITY);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-remove>
    fn Remove(&self, start: f64, end: f64) -> ErrorResult {
        // If not "open" or currently updating, throw InvalidStateError.
        if !self.media_source.is_open() || self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        // The range must be valid: 0 <= start < end.
        if !(start >= 0.0) || !(start < end) {
            return Err(Error::Type(
                CString::new("Invalid remove range").unwrap(),
            ));
        }
        // Run the removal asynchronously. NB: the GStreamer appsrc cannot drop already-pushed
        // data, so `buffered` does not shrink (best-effort); the update events still fire so
        // players that call remove() for buffer management proceed normally.
        self.updating.set(true);
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_remove: move |cx| {
                let sb = this.root();
                sb.upcast::<EventTarget>()
                    .fire_event(cx, Atom::from("updatestart"));
                sb.updating.set(false);
                sb.upcast::<EventTarget>()
                    .fire_event(cx, Atom::from("update"));
                sb.upcast::<EventTarget>()
                    .fire_event(cx, Atom::from("updateend"));
            }));
        Ok(())
    }

    fn GetMode(&self) -> Fallible<AppendMode> {
        Ok(self.mode.get())
    }
    fn SetMode(&self, value: AppendMode) -> ErrorResult {
        self.mode.set(value);
        Ok(())
    }
    fn Updating(&self) -> bool {
        self.updating.get()
    }
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-buffered>
    /// Reports the ranges the attached element's player has actually buffered.
    fn GetBuffered(&self, cx: &mut JSContext) -> Fallible<DomRoot<TimeRanges>> {
        let mut buffered = TimeRangesContainer::default();
        if let Some(element) = self.media_source.media_element() &&
            let Some(player) = element.get_player()
        {
            for range in player.lock().unwrap().buffered() {
                let _ = buffered.add(range.start, range.end);
            }
        }
        Ok(TimeRanges::new(cx, self.global().as_window(), buffered))
    }
    fn GetTimestampOffset(&self) -> Fallible<f64> {
        Ok(self.timestamp_offset.get())
    }
    fn SetTimestampOffset(&self, value: f64) -> ErrorResult {
        self.timestamp_offset.set(value);
        Ok(())
    }
    fn GetAppendWindowStart(&self) -> Fallible<f64> {
        Ok(self.append_window_start.get())
    }
    fn SetAppendWindowStart(&self, value: f64) -> ErrorResult {
        self.append_window_start.set(value);
        Ok(())
    }
    fn GetAppendWindowEnd(&self) -> Fallible<f64> {
        Ok(self.append_window_end.get())
    }
    fn SetAppendWindowEnd(&self, value: f64) -> ErrorResult {
        self.append_window_end.set(value);
        Ok(())
    }
}

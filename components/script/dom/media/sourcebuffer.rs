/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions `SourceBuffer` — Phase 1 scaffold (state only). `appendBuffer` /
//! `remove` and the GStreamer append pipeline land in Phase 2.

use std::cell::Cell;

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
use crate::script_runtime::CanGc;

#[dom_struct]
pub(crate) struct SourceBuffer {
    eventtarget: EventTarget,
    mode: Cell<AppendMode>,
    updating: Cell<bool>,
    buffered: Dom<TimeRanges>,
    timestamp_offset: Cell<f64>,
    append_window_start: Cell<f64>,
    append_window_end: Cell<f64>,
    media_source: Dom<MediaSource>,
}

impl SourceBuffer {
    fn new_inherited(media_source: &MediaSource, buffered: &TimeRanges) -> SourceBuffer {
        SourceBuffer {
            eventtarget: EventTarget::new_inherited(),
            mode: Cell::new(AppendMode::Segments),
            updating: Cell::new(false),
            buffered: Dom::from_ref(buffered),
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
        let buffered =
            TimeRanges::new(global.as_window(), TimeRangesContainer::default(), can_gc);
        reflect_dom_object(
            Box::new(SourceBuffer::new_inherited(media_source, &buffered)),
            global,
            can_gc,
        )
    }

    /// The async segment of `appendBuffer`: push the bytes into the attached element's player,
    /// then clear `updating` and fire `update` + `updateend`.
    fn finish_append(&self, cx: &mut js::context::JSContext, bytes: Vec<u8>) {
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from("updatestart"));

        if let Some(element) = self.media_source.media_element() &&
            let Some(player) = element.get_player()
        {
            if let Err(error) = player.lock().unwrap().push_data(bytes) {
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
    fn GetBuffered(&self) -> Fallible<DomRoot<TimeRanges>> {
        Ok(DomRoot::from_ref(&self.buffered))
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

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions `SourceBuffer`. Appended bytes go to this SourceBuffer's own stream
//! in the player; `buffered` comes from the segments' container timestamps.

use std::cell::Cell;
use std::ffi::CString;

use dom_struct::dom_struct;
use script_bindings::cell::DomRefCell;
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
use crate::dom::media::mediasegmentparser::MediaSegmentParser;
use crate::dom::media::mediasource::MediaSource;
use crate::dom::timeranges::{TimeRanges, TimeRangesContainer};
use js::context::JSContext;
use crate::script_runtime::CanGc;

#[dom_struct]
pub(crate) struct SourceBuffer {
    eventtarget: EventTarget,
    mode: Cell<AppendMode>,
    updating: Cell<bool>,
    /// This SourceBuffer's input stream in the player.
    stream: usize,
    #[no_trace]
    #[ignore_malloc_size_of = "Holds only partial headers and time ranges"]
    segments: DomRefCell<MediaSegmentParser>,
    timestamp_offset: Cell<f64>,
    append_window_start: Cell<f64>,
    append_window_end: Cell<f64>,
    media_source: Dom<MediaSource>,
}

impl SourceBuffer {
    fn new_inherited(media_source: &MediaSource, stream: usize) -> SourceBuffer {
        SourceBuffer {
            eventtarget: EventTarget::new_inherited(),
            mode: Cell::new(AppendMode::Segments),
            updating: Cell::new(false),
            stream,
            segments: Default::default(),
            timestamp_offset: Cell::new(0.0),
            append_window_start: Cell::new(0.0),
            append_window_end: Cell::new(f64::INFINITY),
            media_source: Dom::from_ref(media_source),
        }
    }

    pub(crate) fn new(
        global: &GlobalScope,
        media_source: &MediaSource,
        stream: usize,
        can_gc: CanGc,
    ) -> DomRoot<SourceBuffer> {
        reflect_dom_object(
            Box::new(SourceBuffer::new_inherited(media_source, stream)),
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

    pub(crate) fn buffered_ranges(&self) -> Vec<(f64, f64)> {
        self.segments.borrow().ranges().to_vec()
    }

    /// The async segment of `appendBuffer`: record the segments' time ranges, push the bytes
    /// into this SourceBuffer's stream in the player, then clear `updating` and fire `update` +
    /// `updateend`.
    fn finish_append(&self, cx: &mut js::context::JSContext, bytes: Vec<u8>) {
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from("updatestart"));

        self.segments
            .borrow_mut()
            .append(&bytes, self.timestamp_offset.get());
        if let Some(element) = self.media_source.media_element() &&
            let Some(player) = element.get_player() &&
            let Err(error) = player
                .lock()
                .unwrap()
                .push_source_buffer_data(self.stream, bytes)
        {
            warn!("MSE appendBuffer push failed: {error:?}");
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
        self.segments.borrow_mut().reset_partial();
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
        // The range must be ordered and valid: 0 <= start < end. Reject NaN explicitly.
        if start.is_nan() || end.is_nan() || start < 0.0 || start >= end {
            return Err(Error::Type(
                CString::new("Invalid remove range").unwrap(),
            ));
        }
        // Run the removal asynchronously. The player's stream can't drop data it already took,
        // but `buffered` drops the range, which is what players managing their buffer check.
        self.updating.set(true);
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_remove: move |cx| {
                let sb = this.root();
                sb.upcast::<EventTarget>()
                    .fire_event(cx, Atom::from("updatestart"));
                sb.segments.borrow_mut().remove(start, end);
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
    fn GetBuffered(&self, cx: &mut JSContext) -> Fallible<DomRoot<TimeRanges>> {
        let mut buffered = TimeRangesContainer::default();
        for (start, end) in self.buffered_ranges() {
            buffered
                .add(start, end)
                .expect("MediaSegmentParser ranges are valid and disjoint");
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

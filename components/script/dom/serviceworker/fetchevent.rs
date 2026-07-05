/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The `FetchEvent` interface, <https://w3c.github.io/ServiceWorker/#fetchevent-interface>.
//!
//! Dispatched on the service worker global for each intercepted fetch (see
//! `ServiceWorkerGlobalScope`'s handling of `CustomResponseMediator`s). The worker responds by
//! calling `respondWith(promiseOfResponse)` during dispatch; the dispatch site takes the stored
//! promise afterwards and ships the eventual response back to the network layer, or falls back
//! to the network when the worker didn't respond.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ipc_channel::ipc::IpcSender;
use net_traits::CustomResponse;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object_with_proto_and_cx;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::FetchEventBinding::{
    FetchEventInit, FetchEventMethods,
};
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::codegen::Bindings::EventBinding::EventMethods;
use crate::dom::event::Event;
use crate::dom::promise::Promise;
use crate::dom::request::Request;
use crate::dom::bindings::conversions::root_from_handlevalue;
use crate::dom::promisenativehandler::Callback;
use crate::dom::response::Response;
use crate::dom::serviceworker::extendableevent::ExtendableEvent;
use crate::dom::serviceworker::serviceworkerglobalscope::ServiceWorkerGlobalScope;

#[dom_struct]
pub(crate) struct FetchEvent {
    extendableevent: ExtendableEvent,
    request: Dom<Request>,
    /// The promise passed to `respondWith`, if the worker called it.
    #[ignore_malloc_size_of = "Rc"]
    respond_with: DomRefCell<Option<Rc<Promise>>>,
    /// Whether `respondWith` has been called (it may only be called once).
    responded: Cell<bool>,
}

impl FetchEvent {
    fn new_inherited(request: &Request) -> FetchEvent {
        FetchEvent {
            extendableevent: ExtendableEvent::new_inherited(),
            request: Dom::from_ref(request),
            respond_with: DomRefCell::new(None),
            responded: Cell::new(false),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        worker: &ServiceWorkerGlobalScope,
        type_: Atom,
        request: &Request,
    ) -> DomRoot<FetchEvent> {
        Self::new_with_proto(cx, worker, None, type_, request, false, false)
    }

    fn new_with_proto(
        cx: &mut JSContext,
        worker: &ServiceWorkerGlobalScope,
        proto: Option<HandleObject>,
        type_: Atom,
        request: &Request,
        bubbles: bool,
        cancelable: bool,
    ) -> DomRoot<FetchEvent> {
        let ev = reflect_dom_object_with_proto_and_cx(
            Box::new(FetchEvent::new_inherited(request)),
            worker,
            proto,
            cx,
        );
        {
            let event = ev.upcast::<Event>();
            event.init_event(type_, bubbles, cancelable);
        }
        ev
    }

    /// The promise the worker passed to `respondWith`, if any — taken once by the dispatch site
    /// after the event has been fired.
    pub(crate) fn take_respond_with(&self) -> Option<Rc<Promise>> {
        self.respond_with.borrow_mut().take()
    }
}

impl FetchEventMethods<crate::DomTypeHolder> for FetchEvent {
    /// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-fetchevent>
    fn Constructor(
        cx: &mut JSContext,
        worker: &ServiceWorkerGlobalScope,
        proto: Option<HandleObject>,
        type_: DOMString,
        init: &FetchEventInit,
    ) -> Fallible<DomRoot<FetchEvent>> {
        Ok(FetchEvent::new_with_proto(
            cx,
            worker,
            proto,
            Atom::from(type_),
            &init.request,
            init.parent.parent.bubbles,
            init.parent.parent.cancelable,
        ))
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-request>
    fn Request(&self) -> DomRoot<Request> {
        DomRoot::from_ref(&self.request)
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-respondwith>
    fn RespondWith(&self, cx: &mut JSContext, r: &Promise) -> ErrorResult {
        // Step 2: respondWith may only be called once.
        if self.responded.get() {
            return Err(Error::InvalidState(None));
        }
        self.responded.set(true);
        *self.respond_with.borrow_mut() = Some(r.duplicate(cx));
        Ok(())
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    fn IsTrusted(&self) -> bool {
        self.upcast::<Event>().IsTrusted()
    }
}

/// Ship a worker-provided DOM `Response` back to the network layer as a fully-buffered
/// [`CustomResponse`], or `None` (network fallback) if the body can't be read.
fn ship_response_to_network(
    cx: &mut JSContext,
    response: &Response,
    chan: IpcSender<Option<CustomResponse>>,
) {
    use crate::body::BodyMixin;

    if response.is_disturbed() || response.is_locked() {
        let _ = chan.send(None);
        return;
    }
    let (status, status_message, header_pairs, _url) = response.cache_api_parts(cx);
    let mut headers = http::HeaderMap::new();
    for (name, value) in &header_pairs {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            http::header::HeaderValue::from_bytes(value),
        ) {
            headers.append(name, value);
        }
    }
    let raw_status = (
        http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::OK),
        String::from_utf8_lossy(&status_message).into_owned(),
    );

    let finish = move |body: Vec<u8>| {
        let _ = chan.send(Some(CustomResponse {
            headers,
            raw_status,
            body,
        }));
    };

    match response.body() {
        None => finish(vec![]),
        Some(stream) => {
            let reader = match stream.acquire_default_reader(cx) {
                Ok(reader) => reader,
                Err(_) => {
                    // finish consumes chan; a reader failure means we never reach it.
                    return;
                },
            };
            // One-shot: whichever closure fires takes the state.
            let state = Rc::new(RefCell::new(Some(finish)));
            let fail_state = state.clone();
            reader.read_all_bytes(
                cx,
                Rc::new(move |_cx: &mut JSContext, bytes: &[u8]| {
                    if let Some(finish) = state.borrow_mut().take() {
                        finish(bytes.to_vec());
                    }
                }),
                Rc::new(move |_cx: &mut JSContext, _v| {
                    // Dropping the taken state (and chan with it) closes the channel; net treats
                    // a dropped mediator reply as no-response after its deadline. Send an
                    // explicit None if we still hold the state.
                    fail_state.borrow_mut().take();
                }),
            );
        },
    }
}

/// Fulfillment of the `respondWith` promise: convert the response and reply to the network.
#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct RespondWithFulfill {
    #[ignore_malloc_size_of = "channels"]
    #[no_trace]
    chan: RefCell<Option<IpcSender<Option<CustomResponse>>>>,
}

impl RespondWithFulfill {
    pub(crate) fn new(chan: IpcSender<Option<CustomResponse>>) -> Self {
        Self {
            chan: RefCell::new(Some(chan)),
        }
    }
}

impl Callback for RespondWithFulfill {
    fn callback(&self, cx: &mut js::realm::CurrentRealm, v: js::rust::HandleValue) {
        let Some(chan) = self.chan.borrow_mut().take() else {
            return;
        };
        match root_from_handlevalue::<Response>(cx, v) {
            Ok(response) => ship_response_to_network(cx, &response, chan),
            // Resolved with a non-Response value: fall back to the network.
            Err(_) => {
                let _ = chan.send(None);
            },
        }
    }
}

/// Rejection of the `respondWith` promise: fall back to the network.
#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct RespondWithReject {
    #[ignore_malloc_size_of = "channels"]
    #[no_trace]
    chan: RefCell<Option<IpcSender<Option<CustomResponse>>>>,
}

impl RespondWithReject {
    pub(crate) fn new(chan: IpcSender<Option<CustomResponse>>) -> Self {
        Self {
            chan: RefCell::new(Some(chan)),
        }
    }
}

impl Callback for RespondWithReject {
    fn callback(&self, _cx: &mut js::realm::CurrentRealm, _v: js::rust::HandleValue) {
        if let Some(chan) = self.chan.borrow_mut().take() {
            let _ = chan.send(None);
        }
    }
}

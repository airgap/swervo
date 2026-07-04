/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The `Cache` interface, <https://w3c.github.io/ServiceWorker/#cache-interface>.
//!
//! A `Cache` DOM object is a handle (id) onto one named request→response store in the cache
//! storage thread. `put` fully reads the response body on the script thread (spec: `put`
//! consumes the response), then ships the buffered entry to the storage thread; `add`/`addAll`
//! fetch through the normal fetch stack and `put` each arrival, settling the returned promise
//! when every entry has been stored.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use dom_struct::dom_struct;
use log::error;
use servo_base::generic_channel::GenericCallback;
use servo_url::ServoUrl;
use storage_traits::cache_storage::{
    CacheApiQueryOptions, CacheApiRequest, CacheApiResponse, CacheStorageThreadMsg,
};

use js::context::JSContext;
use js::realm::CurrentRealm;
use js::rust::HandleValue as SafeHandleValue;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_base::generic_channel::GenericSend;

use crate::body::BodyMixin;
use crate::dom::bindings::codegen::Bindings::CacheBinding::{CacheMethods, CacheQueryOptions};
use crate::dom::bindings::codegen::Bindings::RequestBinding::RequestInit;
use crate::dom::bindings::codegen::Bindings::ResponseBinding::ResponseMethods;
use crate::dom::bindings::codegen::UnionTypes::RequestOrUSVString;
use crate::dom::bindings::conversions::root_from_handlevalue;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::refcounted::{Trusted, TrustedPromise};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::ByteString;
use crate::dom::cachestorage::{CacheReplyHandler, native_query_options};
use crate::dom::request::Request;
use crate::dom::response::Response;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::Promise;
use crate::dom::promisenativehandler::{Callback, PromiseNativeHandler};
use crate::fetch::Fetch;
use crate::realms::enter_auto_realm;

/// Convert a `RequestInfo` argument into the transferable request form the storage thread
/// matches against.
pub(crate) fn request_info_to_cache_request(
    global: &GlobalScope,
    info: &RequestOrUSVString,
) -> Result<CacheApiRequest, Error> {
    match info {
        RequestOrUSVString::Request(request) => {
            let net_request = request.get_request();
            Ok(CacheApiRequest {
                url: net_request.current_url().to_string(),
                method: net_request.method.to_string(),
                headers: net_request
                    .headers
                    .iter()
                    .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
                    .collect(),
            })
        },
        RequestOrUSVString::USVString(url) => {
            let url = global
                .api_base_url()
                .join(url)
                .map_err(|_| Error::Type(c"Invalid request URL".to_owned()))?;
            Ok(CacheApiRequest {
                url: url.to_string(),
                method: "GET".to_owned(),
                headers: vec![],
            })
        },
    }
}

/// Spec: `Cache.put` (and the request-side of `add`/`addAll`) only accepts http(s) GETs.
fn validate_put_request(cache_request: &CacheApiRequest) -> Result<(), Error> {
    let scheme_ok = ServoUrl::parse(&cache_request.url)
        .map(|url| url.scheme() == "http" || url.scheme() == "https")
        .unwrap_or(false);
    if !scheme_ok {
        return Err(Error::Type(
            c"Request scheme must be http or https".to_owned(),
        ));
    }
    if !cache_request.method.eq_ignore_ascii_case("GET") {
        return Err(Error::Type(c"Request method must be GET".to_owned()));
    }
    Ok(())
}

/// One in-flight `addAll` batch: its promise settles once every request has been fetched and
/// stored, or rejects on the first failure.
#[derive(JSTraceable, MallocSizeOf)]
struct AddAllRun {
    #[ignore_malloc_size_of = "Rc"]
    promise: Rc<Promise>,
    remaining: Cell<usize>,
    failed: Cell<bool>,
}

/// Where a completed `put` reports to: directly settling a `Cache.put` promise, or ticking off
/// one item of an `addAll` batch.
enum PutTarget {
    Direct(TrustedPromise),
    AddAllItem { cache: Trusted<Cache>, run_id: u64 },
}

#[dom_struct]
pub(crate) struct Cache {
    reflector_: Reflector,
    /// The storage-thread id of the named cache this object fronts.
    cache_id: i64,
    /// In-flight `addAll` batches, by run id.
    pending_add_all: DomRefCell<HashMap<u64, AddAllRun>>,
    next_add_all_id: Cell<u64>,
}

impl Cache {
    fn new_inherited(cache_id: i64) -> Cache {
        Cache {
            reflector_: Reflector::new(),
            cache_id,
            pending_add_all: DomRefCell::new(HashMap::new()),
            next_add_all_id: Cell::new(0),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope, cache_id: i64) -> DomRoot<Cache> {
        reflect_dom_object_with_cx(Box::new(Cache::new_inherited(cache_id)), global, cx)
    }

    /// Read `response`'s body to completion (consuming it, per spec), then ship the entry to the
    /// storage thread and report to `target`.
    fn put_response(
        &self,
        cx: &mut JSContext,
        cache_request: CacheApiRequest,
        response: &Response,
        target: PutTarget,
    ) {
        let global = self.global();
        let (status, status_message, headers, response_url) = response.cache_api_parts(cx);

        // Everything the eventual send needs, shared (script-thread only) between the
        // read-success and read-failure paths; whichever fires first takes it.
        type Pending = (
            PutTarget,
            CacheApiRequest,
            u16,
            Vec<u8>,
            Vec<(String, Vec<u8>)>,
            Option<String>,
        );
        let pending: Rc<RefCell<Option<Pending>>> = Rc::new(RefCell::new(Some((
            target,
            cache_request,
            status,
            status_message,
            headers,
            response_url,
        ))));

        let task_source = global
            .task_manager()
            .dom_manipulation_task_source()
            .to_sendable();
        let storage = global.storage_threads().clone();
        let cache_id = self.cache_id;

        // Ship the entry to the storage thread; the reply settles `target` on the script thread.
        let do_send: Rc<dyn Fn(Vec<u8>)> = {
            let pending = pending.clone();
            Rc::new(move |body: Vec<u8>| {
                let Some((target, cache_request, status, status_message, headers, response_url)) =
                    pending.borrow_mut().take()
                else {
                    return;
                };
                let stored = CacheApiResponse {
                    status,
                    status_message,
                    headers,
                    url: response_url,
                    body,
                };
                // The reply closure is FnMut but fires once; `target` moves out through a cell.
                let target_cell = std::sync::Mutex::new(Some(target));
                let task_source = task_source.clone();
                let callback =
                    GenericCallback::new(move |message: Result<Result<(), String>, _>| {
                        let ok = matches!(message, Ok(Ok(())));
                        let Some(target) = target_cell.lock().unwrap().take() else {
                            return;
                        };
                        task_source.queue(task!(cache_api_put_reply: move |cx| {
                            match target {
                                PutTarget::Direct(trusted_promise) => {
                                    let promise = trusted_promise.root();
                                    if ok {
                                        promise.resolve_native(cx, &());
                                    } else {
                                        promise.reject_error(cx, Error::Operation(None));
                                    }
                                },
                                PutTarget::AddAllItem { cache, run_id } => {
                                    cache.root().finish_add_all_item(cx, run_id, ok);
                                },
                            }
                        }));
                    })
                    .expect("Could not create Cache put callback");
                let _ = storage.send(CacheStorageThreadMsg::Put {
                    sender: callback,
                    cache_id,
                    request: cache_request,
                    response: stored,
                });
            })
        };

        match response.body() {
            None => do_send(vec![]),
            Some(stream) => {
                let reader = match stream.acquire_default_reader(cx) {
                    Ok(reader) => reader,
                    Err(error) => {
                        if let Some((target, ..)) = pending.borrow_mut().take() {
                            Cache::settle_target_failure(cx, target, error);
                        }
                        return;
                    },
                };
                let fail_pending = pending.clone();
                reader.read_all_bytes(
                    cx,
                    Rc::new(move |_cx: &mut JSContext, bytes: &[u8]| {
                        do_send(bytes.to_vec());
                    }),
                    Rc::new(move |cx: &mut JSContext, _v| {
                        if let Some((target, ..)) = fail_pending.borrow_mut().take() {
                            Cache::settle_target_failure(
                                cx,
                                target,
                                Error::Type(c"Failed to read response body".to_owned()),
                            );
                        }
                    }),
                );
            },
        }
    }

    fn settle_target_failure(cx: &mut JSContext, target: PutTarget, error: Error) {
        match target {
            PutTarget::Direct(trusted_promise) => {
                trusted_promise.root().reject_error(cx, error);
            },
            PutTarget::AddAllItem { cache, run_id } => {
                cache.root().finish_add_all_item(cx, run_id, false);
            },
        }
    }

    /// One `addAll` item finished (stored or failed); settle the batch promise when done.
    fn finish_add_all_item(&self, cx: &mut JSContext, run_id: u64, ok: bool) {
        let mut runs = self.pending_add_all.borrow_mut();
        let Some(run) = runs.get(&run_id) else {
            return;
        };
        if !ok && !run.failed.get() {
            run.failed.set(true);
            run.promise.reject_error(
                cx,
                Error::Type(c"A request in addAll failed to fetch or store".to_owned()),
            );
        }
        let remaining = run.remaining.get().saturating_sub(1);
        run.remaining.set(remaining);
        if remaining == 0 {
            let run = runs.remove(&run_id).expect("run just looked up");
            if !run.failed.get() {
                run.promise.resolve_native(cx, &());
            }
        }
    }

    /// The fetch step of `add`/`addAll` for one request.
    fn fetch_and_put(
        &self,
        cx: &mut JSContext,
        info: RequestOrUSVString,
        cache_request: CacheApiRequest,
        run_id: u64,
    ) {
        let global = self.global();
        let mut realm = enter_auto_realm(cx, &*global);
        let realm_cx = &mut realm.current_realm();

        let init = RequestInit::empty();
        let fetch_promise = Fetch(&global, info, init, realm_cx);

        let fulfill = AddAllFetchFulfill {
            cache: Dom::from_ref(self),
            run_id,
            cache_request: RefCell::new(Some(cache_request)),
        };
        let reject = AddAllFetchReject {
            cache: Dom::from_ref(self),
            run_id,
        };
        let handler = PromiseNativeHandler::new(
            realm_cx,
            &global,
            Some(Box::new(fulfill)),
            Some(Box::new(reject)),
        );
        fetch_promise.append_native_handler(realm_cx, &handler);
    }
}

/// Fulfillment of one `addAll` fetch: validate and store the response.
#[derive(JSTraceable, MallocSizeOf)]
struct AddAllFetchFulfill {
    cache: Dom<Cache>,
    run_id: u64,
    #[no_trace]
    cache_request: RefCell<Option<CacheApiRequest>>,
}

impl Callback for AddAllFetchFulfill {
    fn callback(&self, cx: &mut CurrentRealm, v: SafeHandleValue) {
        let cache = DomRoot::from_ref(&*self.cache);
        let Some(cache_request) = self.cache_request.borrow_mut().take() else {
            return;
        };
        let Ok(response) = root_from_handlevalue::<Response>(cx, v) else {
            cache.finish_add_all_item(cx, self.run_id, false);
            return;
        };
        // Spec: reject if the response status is not ok (add/addAll refuse error responses).
        if !response.Ok() {
            cache.finish_add_all_item(cx, self.run_id, false);
            return;
        }
        cache.put_response(
            cx,
            cache_request,
            &response,
            PutTarget::AddAllItem {
                cache: Trusted::new(&*cache),
                run_id: self.run_id,
            },
        );
    }
}

/// Rejection of one `addAll` fetch: the batch fails.
#[derive(JSTraceable, MallocSizeOf)]
struct AddAllFetchReject {
    cache: Dom<Cache>,
    run_id: u64,
}

impl Callback for AddAllFetchReject {
    fn callback(&self, cx: &mut CurrentRealm, _v: SafeHandleValue) {
        DomRoot::from_ref(&*self.cache).finish_add_all_item(cx, self.run_id, false);
    }
}

impl CacheMethods<crate::DomTypeHolder> for Cache {
    /// <https://w3c.github.io/ServiceWorker/#cache-match>
    fn Match(
        &self,
        cx: &mut JSContext,
        request: RequestOrUSVString,
        options: &CacheQueryOptions,
    ) -> Rc<Promise> {
        let global = self.global();
        let promise = Promise::new(cx, &global);
        let cache_request = match request_info_to_cache_request(&global, &request) {
            Ok(cache_request) => cache_request,
            Err(error) => {
                promise.reject_error(cx, error);
                return promise;
            },
        };

        let mut handler = CacheReplyHandler::new(
            &promise,
            global
                .task_manager()
                .dom_manipulation_task_source()
                .to_sendable(),
        );
        let callback = GenericCallback::new(move |message: Result<Vec<CacheApiResponse>, _>| {
            let responses = message.unwrap_or_default();
            handler.settle(responses, |cx, promise, responses| {
                match responses.into_iter().next() {
                    Some(stored) => {
                        let global = promise.global();
                        let response = Response::new_from_cache_api(cx, &global, &stored);
                        promise.resolve_native(cx, &response);
                    },
                    None => promise.resolve_native(cx, &()),
                }
            });
        })
        .expect("Could not create Cache match callback");

        let _ = global.storage_threads().send(CacheStorageThreadMsg::Match {
            sender: callback,
            origin: global.origin().immutable().clone(),
            cache_id: Some(self.cache_id),
            cache_name: None,
            request: Some(cache_request),
            options: native_query_options(options),
            max_results: 1,
        });

        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-matchall>
    fn MatchAll(
        &self,
        cx: &mut JSContext,
        request: Option<RequestOrUSVString>,
        options: &CacheQueryOptions,
    ) -> Rc<Promise> {
        let global = self.global();
        let promise = Promise::new(cx, &global);
        let cache_request = match request
            .as_ref()
            .map(|info| request_info_to_cache_request(&global, info))
            .transpose()
        {
            Ok(cache_request) => cache_request,
            Err(error) => {
                promise.reject_error(cx, error);
                return promise;
            },
        };

        let mut handler = CacheReplyHandler::new(
            &promise,
            global
                .task_manager()
                .dom_manipulation_task_source()
                .to_sendable(),
        );
        let callback = GenericCallback::new(move |message: Result<Vec<CacheApiResponse>, _>| {
            let responses = message.unwrap_or_default();
            handler.settle(responses, |cx, promise, responses| {
                let global = promise.global();
                let responses: Vec<DomRoot<Response>> = responses
                    .iter()
                    .map(|stored| Response::new_from_cache_api(cx, &global, stored))
                    .collect();
                promise.resolve_native(cx, &responses);
            });
        })
        .expect("Could not create Cache matchAll callback");

        let _ = global.storage_threads().send(CacheStorageThreadMsg::Match {
            sender: callback,
            origin: global.origin().immutable().clone(),
            cache_id: Some(self.cache_id),
            cache_name: None,
            request: cache_request,
            options: native_query_options(options),
            max_results: u32::MAX,
        });

        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-add>
    fn Add(&self, cx: &mut JSContext, request: RequestOrUSVString) -> Rc<Promise> {
        self.AddAll(cx, vec![request])
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-addall>
    fn AddAll(&self, cx: &mut JSContext, requests: Vec<RequestOrUSVString>) -> Rc<Promise> {
        let global = self.global();
        let promise = Promise::new(cx, &global);

        // Validate every request up front; one bad request rejects the whole batch untouched.
        let mut converted = Vec::with_capacity(requests.len());
        for info in &requests {
            match request_info_to_cache_request(&global, info)
                .and_then(|cache_request| {
                    validate_put_request(&cache_request)?;
                    Ok(cache_request)
                }) {
                Ok(cache_request) => converted.push(cache_request),
                Err(error) => {
                    promise.reject_error(cx, error);
                    return promise;
                },
            }
        }

        if requests.is_empty() {
            promise.resolve_native(cx, &());
            return promise;
        }

        let run_id = self.next_add_all_id.get();
        self.next_add_all_id.set(run_id + 1);
        self.pending_add_all.borrow_mut().insert(
            run_id,
            AddAllRun {
                promise: promise.clone(),
                remaining: Cell::new(requests.len()),
                failed: Cell::new(false),
            },
        );

        for (info, cache_request) in requests.into_iter().zip(converted) {
            self.fetch_and_put(cx, info, cache_request, run_id);
        }

        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-put>
    fn Put(&self, cx: &mut JSContext, request: RequestOrUSVString, response: &Response) -> Rc<Promise> {
        let global = self.global();
        let promise = Promise::new(cx, &global);
        let cache_request = match request_info_to_cache_request(&global, &request)
            .and_then(|cache_request| {
                validate_put_request(&cache_request)?;
                Ok(cache_request)
            }) {
            Ok(cache_request) => cache_request,
            Err(error) => {
                promise.reject_error(cx, error);
                return promise;
            },
        };

        // Spec: a disturbed or locked body rejects with a TypeError.
        if response.is_disturbed() || response.is_locked() {
            promise.reject_error(
                cx,
                Error::Type(c"Response body is disturbed or locked".to_owned()),
            );
            return promise;
        }

        self.put_response(
            cx,
            cache_request,
            response,
            PutTarget::Direct(TrustedPromise::new(promise.clone())),
        );

        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-delete>
    fn Delete(
        &self,
        cx: &mut JSContext,
        request: RequestOrUSVString,
        options: &CacheQueryOptions,
    ) -> Rc<Promise> {
        let global = self.global();
        let promise = Promise::new(cx, &global);
        let cache_request = match request_info_to_cache_request(&global, &request) {
            Ok(cache_request) => cache_request,
            Err(error) => {
                promise.reject_error(cx, error);
                return promise;
            },
        };

        let mut handler = CacheReplyHandler::new(
            &promise,
            global
                .task_manager()
                .dom_manipulation_task_source()
                .to_sendable(),
        );
        let callback = GenericCallback::new(move |message: Result<bool, _>| {
            let deleted = message.unwrap_or(false);
            handler.settle(deleted, |cx, promise, deleted| {
                promise.resolve_native(cx, &deleted)
            });
        })
        .expect("Could not create Cache delete callback");

        let _ = global
            .storage_threads()
            .send(CacheStorageThreadMsg::DeleteEntry {
                sender: callback,
                cache_id: self.cache_id,
                request: cache_request,
                options: native_query_options(options),
            });

        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-keys>
    fn Keys(
        &self,
        cx: &mut JSContext,
        request: Option<RequestOrUSVString>,
        options: &CacheQueryOptions,
    ) -> Rc<Promise> {
        let global = self.global();
        let promise = Promise::new(cx, &global);
        let cache_request = match request
            .as_ref()
            .map(|info| request_info_to_cache_request(&global, info))
            .transpose()
        {
            Ok(cache_request) => cache_request,
            Err(error) => {
                promise.reject_error(cx, error);
                return promise;
            },
        };

        let mut handler = CacheReplyHandler::new(
            &promise,
            global
                .task_manager()
                .dom_manipulation_task_source()
                .to_sendable(),
        );
        let callback = GenericCallback::new(move |message: Result<Vec<CacheApiRequest>, _>| {
            let stored = message.unwrap_or_default();
            handler.settle(stored, |cx, promise, stored| {
                let global = promise.global();
                let requests: Vec<DomRoot<Request>> = stored
                    .iter()
                    .filter_map(|entry| {
                        let mut init = RequestInit::empty();
                        init.method = Some(ByteString::new(entry.method.as_bytes().to_vec()));
                        Request::constructor(
                            cx,
                            &global,
                            None,
                            RequestOrUSVString::USVString(entry.url.clone().into()),
                            &init,
                        )
                        .ok()
                    })
                    .collect();
                promise.resolve_native(cx, &requests);
            });
        })
        .expect("Could not create Cache keys callback");

        let _ = global.storage_threads().send(CacheStorageThreadMsg::Keys {
            sender: callback,
            cache_id: self.cache_id,
            request: cache_request,
            options: native_query_options(options),
        });

        promise
    }
}

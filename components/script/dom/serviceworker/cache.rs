/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use js::rust::HandleValue as SafeHandleValue;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use script_bindings::root::DomRoot;
use servo_base::generic_channel::{GenericCallback, GenericSend};
use storage_traits::cache_storage::{
    CacheApiQueryOptions, CacheApiRequest, CacheApiResponse, CacheBatchError, CacheMatchScope,
    CacheStorageThreadMessage, CacheStorageThreadResponse,
};

use crate::dom::bindings::codegen::Bindings::CacheBinding::CacheMethods;
use crate::dom::bindings::codegen::Bindings::RequestBinding::RequestInit;
use crate::dom::bindings::codegen::GenericBindings::CacheBinding::CacheQueryOptions;
use crate::dom::bindings::codegen::UnionTypes::RequestOrUSVString;
use crate::dom::bindings::conversions::root_from_handlevalue;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::Dom;
use crate::dom::bindings::str::DOMString;
use crate::dom::fetch::request::Request;
use crate::dom::fetch::response::Response;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promisenativehandler::{Callback, PromiseNativeHandler};
use crate::dom::{Promise, RootedPromise, TracedPromise};
use crate::fetch::body::BodyMixin;
use crate::fetch::fetch::Fetch;
use crate::realms::enter_auto_realm;

/// Convert the bindings' `CacheQueryOptions` dictionary to the form the storage thread uses.
pub(crate) fn native_query_options(options: &CacheQueryOptions) -> CacheApiQueryOptions {
    CacheApiQueryOptions {
        ignore_search: options.ignoreSearch,
        ignore_method: options.ignoreMethod,
        ignore_vary: options.ignoreVary,
    }
}

/// Get the request a `RequestInfo` argument designates, in the form the storage thread matches
/// against: the associated request of a `Request` object, or the request of the result of
/// invoking the initial value of `Request` as constructor with a string.
pub(crate) fn request_info_to_cache_request(
    global: &GlobalScope,
    info: &RequestOrUSVString,
) -> Result<CacheApiRequest, Error> {
    match info {
        RequestOrUSVString::Request(request) => {
            let net_request = request.request();
            Ok(CacheApiRequest {
                url: net_request.url().to_string(),
                method: net_request.method.to_string(),
                headers: net_request
                    .headers
                    .iter()
                    .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
                    .collect(),
            })
        },
        RequestOrUSVString::USVString(url) => {
            // Note: parsing the URL is the only step of the constructor that can throw for
            // a string argument.
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

/// The request checks shared by `put` and `addAll`: the request's URL’s scheme must be an HTTP(S)
/// scheme and its method must be `GET`.
fn validate_request_for_storage(request: &CacheApiRequest) -> Result<(), Error> {
    let is_http = request.url.starts_with("http:") || request.url.starts_with("https:");
    if !is_http {
        return Err(Error::Type(
            c"Request scheme must be http or https".to_owned(),
        ));
    }
    if request.method != "GET" {
        return Err(Error::Type(c"Request method must be GET".to_owned()));
    }
    Ok(())
}

/// Whether a header list's `Vary` header has a field-value matching `*`.
fn vary_contains_star(headers: &[(String, Vec<u8>)]) -> bool {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("vary"))
        .any(|(_, value)| {
            String::from_utf8_lossy(value)
                .split(',')
                .any(|field| field.trim() == "*")
        })
}

/// Snapshot a response for storage; the body is filled in once it has been read.
fn cache_api_response(cx: &mut JSContext, response: &Response) -> CacheApiResponse {
    let (status, status_message, headers, url) = response.cache_api_parts(cx);
    CacheApiResponse {
        status,
        status_message,
        headers,
        url,
        response_type: response.cache_api_response_type(),
        body: vec![],
    }
}

/// Read all bytes of `response`'s body (consuming it), then call `done` with the bytes, or with
/// the error if the body could not be read. A null body reads as no bytes.
fn read_response_body<F>(cx: &mut JSContext, response: &Response, done: F)
where
    F: FnOnce(&mut JSContext, Result<Vec<u8>, Error>) + 'static,
{
    let Some(stream) = response.body() else {
        done(cx, Ok(vec![]));
        return;
    };
    let reader = match stream.acquire_default_reader(cx) {
        Ok(reader) => reader,
        Err(error) => {
            done(cx, Err(error));
            return;
        },
    };
    // Only one of the two steps runs; whichever does takes `done`.
    let success_done = Rc::new(RefCell::new(Some(done)));
    let failure_done = success_done.clone();
    reader.read_all_bytes(
        cx,
        Rc::new(move |cx: &mut JSContext, bytes: &[u8]| {
            if let Some(done) = success_done.borrow_mut().take() {
                done(cx, Ok(bytes.to_vec()));
            }
        }),
        Rc::new(move |cx: &mut JSContext, _error: SafeHandleValue| {
            if let Some(done) = failure_done.borrow_mut().take() {
                done(
                    cx,
                    Err(Error::Type(c"Failed to read the response body".to_owned())),
                );
            }
        }),
    );
}

/// What a pending promise waits for, so a reply from the storage thread settles it the right way.
#[derive(Clone, Copy, JSTraceable, MallocSizeOf, PartialEq)]
enum PendingOperation {
    Match,
    MatchAll,
    Put,
    Delete,
    Keys,
}

#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct PendingPromise {
    operation: PendingOperation,
    promise: TracedPromise,
}

/// One in-flight `addAll`: the fetched entries, in request order, waiting until every fetch has
/// completed so that they can be stored as one batch.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct AddAllRun {
    promise: TracedPromise,
    #[no_trace]
    entries: Vec<Option<(CacheApiRequest, CacheApiResponse)>>,
    remaining: usize,
}

/// <https://w3c.github.io/ServiceWorker/#cache>
#[dom_struct]
pub(crate) struct Cache {
    reflector_: Reflector,

    /// The name used to identify
    /// <https://w3c.github.io/ServiceWorker/#dfn-relevant-request-response-list>
    name: DOMString,

    #[no_trace]
    #[ignore_malloc_size_of = "GenericCallback"]
    callback: RefCell<Option<GenericCallback<CacheStorageThreadResponse>>>,

    /// Promises waiting for a reply from the storage thread, in the order the requests were sent.
    pending_promises: RefCell<VecDeque<PendingPromise>>,

    /// In-flight `addAll` calls, by id.
    pending_add_all: DomRefCell<HashMap<u64, AddAllRun>>,
    next_add_all_id: Cell<u64>,
}

impl Cache {
    fn new_inherited(name: DOMString) -> Cache {
        Cache {
            reflector_: Reflector::new(),
            name,
            callback: Default::default(),
            pending_promises: Default::default(),
            pending_add_all: Default::default(),
            next_add_all_id: Cell::new(0),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope, name: DOMString) -> DomRoot<Cache> {
        reflect_dom_object(cx, Box::new(Cache::new_inherited(name)), global)
    }

    /// Setup the callback to the backend service, if this hasn't been done already.
    pub(crate) fn get_or_setup_callback(&self) -> GenericCallback<CacheStorageThreadResponse> {
        if let Some(cb) = self.callback.borrow().as_ref() {
            return cb.clone();
        }

        let global = self.global();
        let response_listener = Trusted::new(self);

        let task_source = global
            .task_manager()
            .dom_manipulation_task_source()
            .to_sendable();
        let callback = GenericCallback::new(move |message| {
            let response_listener = response_listener.clone();
            let response = match message {
                Ok(inner) => Some(inner),
                Err(err) => {
                    error!("Error in Cache callback {:?}.", err);
                    None
                },
            };
            task_source.queue(task!(set_request_result_to_database: move |cx| {
                let cache = response_listener.root();
                cache.handle_response(cx, response)
            }));
        })
        .expect("Could not create Cache callback");

        *self.callback.borrow_mut() = Some(callback.clone());

        callback
    }

    /// Send `message` (built around this object's callback) to the storage thread; its reply
    /// settles `promise` according to `operation`.
    fn send_to_backend(
        &self,
        cx: &mut JSContext,
        operation: PendingOperation,
        promise: &RootedPromise,
        message: impl FnOnce(GenericCallback<CacheStorageThreadResponse>) -> CacheStorageThreadMessage,
    ) {
        let callback = self.get_or_setup_callback();
        if self
            .global()
            .storage_threads()
            .send(message(callback))
            .is_err()
        {
            promise.reject_error(
                cx,
                Error::Operation(Some("Could not run the parallel steps.".to_string())),
            );
            return;
        }
        self.pending_promises
            .borrow_mut()
            .push_back(PendingPromise {
                operation,
                promise: promise.to_traced(),
            });
    }

    fn take_pending_promise(
        &self,
        cx: &mut JSContext,
        operation: PendingOperation,
    ) -> Option<RootedPromise> {
        let pending = self.pending_promises.borrow_mut().pop_front()?;
        debug_assert!(
            pending.operation == operation,
            "Cache backend replies arrived out of order."
        );
        Some(pending.promise.root(cx))
    }

    fn handle_response(&self, cx: &mut JSContext, response: Option<CacheStorageThreadResponse>) {
        let Some(response) = response else {
            let Some(pending) = self.pending_promises.borrow_mut().pop_front() else {
                error!("No pending promise for Cache response.");
                return;
            };
            pending.promise.root(cx).reject_error(
                cx,
                Error::Operation(Some("No response from Cache backend.".to_string())),
            );
            return;
        };

        let operation = match &response {
            CacheStorageThreadResponse::MatchResult(_) => {
                match self.pending_promises.borrow().front() {
                    Some(PendingPromise {
                        operation: PendingOperation::MatchAll,
                        ..
                    }) => PendingOperation::MatchAll,
                    _ => PendingOperation::Match,
                }
            },
            CacheStorageThreadResponse::PutResult(_) => PendingOperation::Put,
            CacheStorageThreadResponse::DeleteEntriesResult(_) => PendingOperation::Delete,
            CacheStorageThreadResponse::KeysResult(_) => PendingOperation::Keys,
            CacheStorageThreadResponse::HasCacheResult(_) |
            CacheStorageThreadResponse::OpenCacheResult { .. } |
            CacheStorageThreadResponse::CacheNamesResult(_) |
            CacheStorageThreadResponse::DeleteCacheResult(_) => {
                debug_assert!(false, "Unexpected CacheStorage response in Cache.");
                return;
            },
        };
        let Some(promise) = self.take_pending_promise(cx, operation) else {
            debug_assert!(false, "No pending promise for Cache response.");
            return;
        };

        match response {
            // <https://w3c.github.io/ServiceWorker/#cache-match>
            // <https://w3c.github.io/ServiceWorker/#cache-matchall>
            CacheStorageThreadResponse::MatchResult(result) => {
                let responses = match result {
                    Ok(responses) => responses,
                    Err(error) => {
                        promise.reject_error(cx, Error::Operation(Some(error)));
                        return;
                    },
                };
                // matchAll step 2.4: Queue a task, on promise’s relevant settings object’s
                // responsible event loop using the DOM manipulation task source, to perform the
                // following steps:
                // Note: we are inside the task.
                let global = self.global();
                if operation == PendingOperation::Match {
                    // match step 2.1: If p rejects with an exception, then reject.
                    // match step 2.2: Else if p resolves with an array, response, then:
                    // Step 2.2.1: If response is an empty array, then resolve with undefined.
                    // Step 2.2.2: Else, resolve with the first element of response.
                    match responses.first() {
                        Some(stored) => {
                            let response = Response::new_from_cache_api(cx, &global, stored);
                            promise.resolve_native(cx, &response);
                        },
                        None => promise.resolve_native(cx, &()),
                    }
                } else {
                    // Step 2.4.1: Let responseList be a list.
                    // Step 2.4.2: For each response in responses: add a new Response object
                    // associated with response and a new associated Headers object whose guard
                    // is "immutable" to responseList.
                    let response_list: Vec<DomRoot<Response>> = responses
                        .iter()
                        .map(|stored| Response::new_from_cache_api(cx, &global, stored))
                        .collect();
                    // Step 2.4.3: Resolve promise with a frozen array created from
                    // responseList, in realm.
                    promise.resolve_native(cx, &response_list);
                }
            },
            // <https://w3c.github.io/ServiceWorker/#cache-put>, and the batch steps of
            // <https://w3c.github.io/ServiceWorker/#cache-addAll>.
            CacheStorageThreadResponse::PutResult(result) => match result {
                // Step 3.2: Queue a task ... to resolve cacheJobPromise with undefined.
                Ok(()) => promise.resolve_native(cx, &()),
                // Step 3.1: If an exception was thrown, queue a task ... to reject
                // cacheJobPromise with the exception.
                Err(CacheBatchError::DuplicateRequest) => promise.reject_error(
                    cx,
                    Error::InvalidState(Some(
                        "Two requests in one batch match each other.".to_string(),
                    )),
                ),
                Err(CacheBatchError::NoSuchCache) => promise.reject_error(
                    cx,
                    Error::InvalidState(Some("The cache has been deleted.".to_string())),
                ),
                Err(CacheBatchError::Internal(error)) => {
                    promise.reject_error(cx, Error::Operation(Some(error)))
                },
            },
            // <https://w3c.github.io/ServiceWorker/#cache-delete>
            CacheStorageThreadResponse::DeleteEntriesResult(result) => match result {
                // Step 5.2: Queue a task ... to resolve cacheJobPromise with requestResponses’s
                // size > 0.
                Ok(deleted) => promise.resolve_native(cx, &deleted),
                Err(error) => promise.reject_error(cx, Error::Operation(Some(error))),
            },
            // <https://w3c.github.io/ServiceWorker/#cache-keys>
            CacheStorageThreadResponse::KeysResult(result) => {
                let requests = match result {
                    Ok(requests) => requests,
                    Err(error) => {
                        promise.reject_error(cx, Error::Operation(Some(error)));
                        return;
                    },
                };
                // Step 5.4: Queue a task,
                // on promise’s relevant settings object’s responsible event loop using the DOM manipulation task source,
                // to perform the following steps:
                // Note: we are inside the task.

                // Step 5.4.1: Let requestList be a list.
                // Step 5.4.2: For each request of requests:
                // Step 5.4.2.1: Add a new Request object associated with request
                // and a new associated Headers object whose guard is "immutable" to requestList.
                let global = self.global();
                let request_list: Vec<DomRoot<Request>> = requests
                    .iter()
                    .filter_map(|stored| Request::new_from_cache_api(cx, &global, stored))
                    .collect();

                // Step 5.4.3: Resolve promise with a frozen array created from requestList, in realm.
                promise.resolve_native(cx, &request_list);
            },
            _ => unreachable!("Filtered out above."),
        }
    }

    /// The "match" steps shared by `match` and `matchAll`.
    fn match_requests(
        &self,
        cx: &mut JSContext,
        operation: PendingOperation,
        request: Option<&RequestOrUSVString>,
        options: &CacheQueryOptions,
    ) -> RootedPromise {
        let global = self.global();
        // Step 3: Let promise be a new promise.
        let promise = Promise::new(cx, &global);

        // Step 1: Let r be null.
        // Step 2: If the optional argument request is not omitted, then set r to the request
        // it designates; if this throws an exception, return a promise rejected with it.
        // Note: the check that r’s method is `GET` (unless ignoreMethod) is done in parallel,
        // as part of the request matching.
        let r = match request
            .map(|request| request_info_to_cache_request(&global, request))
            .transpose()
        {
            Ok(r) => r,
            Err(error) => {
                promise.reject_error(cx, error);
                return promise;
            },
        };

        // Step 4: Run these substeps in parallel:
        let max_results = if operation == PendingOperation::Match {
            1
        } else {
            usize::MAX
        };
        self.send_to_backend(cx, operation, &promise, |callback| {
            CacheStorageThreadMessage::Match {
                scope: CacheMatchScope::Cache(self.name.to_string()),
                request: r,
                options: native_query_options(options),
                max_results,
                callback,
                origin: global.origin().immutable().clone(),
            }
        });

        // Step 5: Return promise.
        promise
    }

    /// Send the batch of "put" operations of a `put` or `addAll` call.
    fn store_entries(
        &self,
        cx: &mut JSContext,
        promise: &RootedPromise,
        entries: Vec<(CacheApiRequest, CacheApiResponse)>,
    ) {
        let global = self.global();
        self.send_to_backend(cx, PendingOperation::Put, promise, |callback| {
            CacheStorageThreadMessage::Put {
                cache_name: self.name.to_string(),
                entries,
                callback,
                origin: global.origin().immutable().clone(),
            }
        });
    }

    /// Fetch one request of an `addAll` call; the result is reported to the run `run_id`.
    fn fetch_for_add_all(
        &self,
        cx: &mut JSContext,
        info: RequestOrUSVString,
        cache_request: CacheApiRequest,
        run_id: u64,
        index: usize,
    ) {
        let global = self.global();
        let mut realm = enter_auto_realm(cx, &*global);
        let realm_cx = &mut realm.current_realm();

        let fetch_promise = Fetch(&global, info, &RequestInit::empty(), realm_cx);

        rooted!(&in(realm_cx) let mut fulfill = Some(AddAllFetchFulfill {
            cache: Dom::from_ref(self),
            run_id,
            index,
            cache_request: RefCell::new(Some(cache_request)),
        }));
        rooted!(&in(realm_cx) let mut reject = Some(AddAllFetchReject {
            cache: Dom::from_ref(self),
            run_id,
        }));
        let handler = PromiseNativeHandler::new(
            realm_cx,
            &global,
            fulfill
                .take()
                .map(|callback| Box::new(callback) as Box<dyn Callback>),
            reject
                .take()
                .map(|callback| Box::new(callback) as Box<dyn Callback>),
        );
        fetch_promise.append_native_handler(realm_cx, &handler);
    }

    /// One fetch of an `addAll` run failed: reject the run's promise and forget the run, so the
    /// remaining fetches have nothing left to report to.
    fn fail_add_all(&self, cx: &mut JSContext, run_id: u64, error: Error) {
        let Some(run) = self.pending_add_all.borrow_mut().remove(&run_id) else {
            return;
        };
        run.promise.root(cx).reject_error(cx, error);
    }

    /// One fetch of an `addAll` run completed with its body read. Once every fetch has, store
    /// the entries as one batch.
    fn complete_add_all_entry(
        &self,
        cx: &mut JSContext,
        run_id: u64,
        index: usize,
        entry: (CacheApiRequest, CacheApiResponse),
    ) {
        let finished = {
            let mut runs = self.pending_add_all.borrow_mut();
            let Some(run) = runs.get_mut(&run_id) else {
                return;
            };
            run.entries[index] = Some(entry);
            run.remaining -= 1;
            if run.remaining > 0 {
                return;
            }
            runs.remove(&run_id).expect("The run was just found.")
        };
        // Step 6: Return the result of reacting to p with a fulfillment handler that, when
        // called with argument responses, performs the following substeps:
        // Step 6.1: Let operations be an empty list.
        // Step 6.2: For each response in responses: append a cache batch operation of type
        // "put" with the matching request and response.
        let entries = finished.entries.into_iter().flatten().collect();
        let promise = finished.promise.root(cx);
        self.store_entries(cx, &promise, entries);
    }
}

/// The fetch handling of one `addAll` request: validate the response, then read its body.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct AddAllFetchFulfill {
    cache: Dom<Cache>,
    run_id: u64,
    index: usize,
    #[no_trace]
    cache_request: RefCell<Option<CacheApiRequest>>,
}

impl js::gc::Rootable for AddAllFetchFulfill {}

impl Callback for AddAllFetchFulfill {
    /// Steps 4.2.6 and 4.2.7 of <https://w3c.github.io/ServiceWorker/#cache-addAll>.
    fn callback(&self, cx: &mut CurrentRealm, v: SafeHandleValue) {
        let cache = DomRoot::from_ref(&*self.cache);
        let Some(cache_request) = self.cache_request.borrow_mut().take() else {
            return;
        };
        let Ok(response) = root_from_handlevalue::<Response>(cx, v) else {
            cache.fail_add_all(cx, self.run_id, Error::Type(c"Fetch failed".to_owned()));
            return;
        };

        // Step 4.2.6.1: If response’s type is "error", or response’s status is not an ok status
        // or is 206, reject responsePromise with a TypeError.
        // Note: a network error rejects the fetch promise instead.
        let status = response.status_code();
        if !(200..300).contains(&status) || status == 206 {
            cache.fail_add_all(
                cx,
                self.run_id,
                Error::Type(c"Response status is not ok".to_owned()),
            );
            return;
        }

        // Step 4.2.6.2: Else if response’s header list contains a header named `Vary`, then:
        // if any of its field-values matches "*", reject responsePromise with a TypeError.
        let stored_response = cache_api_response(cx, &response);
        if vary_contains_star(&stored_response.headers) {
            cache.fail_add_all(
                cx,
                self.run_id,
                Error::Type(c"Response has a Vary header of *".to_owned()),
            );
            return;
        }

        // Step 4.2.7: processResponseEndOfBody: resolve responsePromise with response.
        let trusted_cache = Trusted::new(&*cache);
        let run_id = self.run_id;
        let index = self.index;
        read_response_body(cx, &response, move |cx, body| {
            let cache = trusted_cache.root();
            match body {
                Ok(body) => {
                    let entry = (
                        cache_request,
                        CacheApiResponse {
                            body,
                            ..stored_response
                        },
                    );
                    cache.complete_add_all_entry(cx, run_id, index, entry);
                },
                Err(error) => cache.fail_add_all(cx, run_id, error),
            }
        });
    }
}

/// A rejected fetch of one `addAll` request: the whole `addAll` rejects.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct AddAllFetchReject {
    cache: Dom<Cache>,
    run_id: u64,
}

impl js::gc::Rootable for AddAllFetchReject {}

impl Callback for AddAllFetchReject {
    fn callback(&self, cx: &mut CurrentRealm, _v: SafeHandleValue) {
        DomRoot::from_ref(&*self.cache).fail_add_all(
            cx,
            self.run_id,
            Error::Type(c"A request in addAll failed to fetch".to_owned()),
        );
    }
}

impl CacheMethods<crate::DomTypeHolder> for Cache {
    /// <https://w3c.github.io/ServiceWorker/#dom-cache-match>
    fn Match(
        &self,
        cx: &mut JSContext,
        request: RequestOrUSVString,
        options: &CacheQueryOptions,
    ) -> RootedPromise {
        // Step 1: Let promise be a new promise.
        // Step 2: Run these substeps in parallel:
        // Step 2.1: Let p be the result of running the algorithm specified in matchAll(request,
        // options) method with request and options.
        // Note: the first matching response is requested directly, and steps 2.2 (resolving
        // with the first element, or undefined) run in handle_response.
        self.match_requests(cx, PendingOperation::Match, Some(&request), options)
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-cache-matchall>
    fn MatchAll(
        &self,
        cx: &mut JSContext,
        request: Option<RequestOrUSVString>,
        options: &CacheQueryOptions,
    ) -> RootedPromise {
        self.match_requests(cx, PendingOperation::MatchAll, request.as_ref(), options)
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-cache-add>
    fn Add(&self, cx: &mut JSContext, request: RequestOrUSVString) -> RootedPromise {
        // Step 1: Let requests be an array containing only request.
        // Step 2: Let responseArrayPromise be the result of running the algorithm specified in
        // addAll(requests) passing requests as the argument.
        // Step 3: Return the result of reacting to responseArrayPromise with a fulfillment
        // handler that returns undefined.
        // Note: addAll already resolves with undefined.
        self.AddAll(cx, vec![request])
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-cache-addall>
    fn AddAll(&self, cx: &mut JSContext, requests: Vec<RequestOrUSVString>) -> RootedPromise {
        let global = self.global();
        let promise = Promise::new(cx, &global);

        // Step 1: Let responsePromises be an empty list.
        // Step 2: Let requestList be an empty list.
        // Step 3: For each request whose type is Request in requests:
        // Step 3.1: Let r be request’s request.
        // Step 3.2: If r’s url’s scheme is not one of "http" and "https", or r’s method is not
        // `GET`, return a promise rejected with a TypeError.
        // Step 4: For each request in requests: let r be the associated request of the result of
        // invoking the initial value of Request as constructor with request as its argument (if
        // this throws an exception, return a promise rejected with that exception), and append
        // r to requestList.
        let mut request_list = Vec::with_capacity(requests.len());
        for request in &requests {
            match request_info_to_cache_request(&global, request).and_then(|r| {
                validate_request_for_storage(&r)?;
                Ok(r)
            }) {
                Ok(r) => request_list.push(r),
                Err(error) => {
                    promise.reject_error(cx, error);
                    return promise;
                },
            }
        }

        // Step 7: Let p be the result of getting a promise to wait for all of responsePromises.
        // Note: with no requests p is already fulfilled, and the batch has no operations.
        if requests.is_empty() {
            promise.resolve_native(cx, &());
            return promise;
        }

        let run_id = self.next_add_all_id.get();
        self.next_add_all_id.set(run_id + 1);
        self.pending_add_all.borrow_mut().insert(
            run_id,
            AddAllRun {
                promise: promise.to_traced(),
                entries: vec![None; requests.len()],
                remaining: requests.len(),
            },
        );

        // Step 6: For each request in requestList: fetch request, and resolve or reject the
        // corresponding responsePromise.
        for (index, (request, cache_request)) in requests.into_iter().zip(request_list).enumerate()
        {
            self.fetch_for_add_all(cx, request, cache_request, run_id, index);
        }

        // Step 8: Return the result of reacting to p (see complete_add_all_entry).
        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-cache-put>
    fn Put(
        &self,
        cx: &mut JSContext,
        request: RequestOrUSVString,
        response: &Response,
    ) -> RootedPromise {
        let global = self.global();
        let promise = Promise::new(cx, &global);

        // Step 1: Let innerRequest be null.
        // Step 2: If request is a Request object, then set innerRequest to request’s request.
        // Step 3: Else: let requestObj be the result of invoking Request’s constructor with
        // request as its argument (if this throws, return a promise rejected with the
        // exception), and set innerRequest to requestObj’s request.
        // Step 4: If innerRequest’s url’s scheme is not one of "http" and "https", or
        // innerRequest’s method is not `GET`, return a promise rejected with a TypeError.
        let inner_request = match request_info_to_cache_request(&global, &request).and_then(|r| {
            validate_request_for_storage(&r)?;
            Ok(r)
        }) {
            Ok(inner_request) => inner_request,
            Err(error) => {
                promise.reject_error(cx, error);
                return promise;
            },
        };

        // Step 5: Let innerResponse be response’s response.
        let inner_response = cache_api_response(cx, response);

        // Step 6: If innerResponse’s status is 206, return a promise rejected with a TypeError.
        if response.status_code() == 206 {
            promise.reject_error(cx, Error::Type(c"Response status is 206".to_owned()));
            return promise;
        }

        // Step 7: If innerResponse’s header list contains a header named `Vary`, then: if any
        // of its field-values matches "*", return a promise rejected with a TypeError.
        if vary_contains_star(&inner_response.headers) {
            promise.reject_error(
                cx,
                Error::Type(c"Response has a Vary header of *".to_owned()),
            );
            return promise;
        }

        // Step 8: If innerResponse’s body is disturbed or locked, return a promise rejected
        // with a TypeError.
        if response.is_disturbed() || response.is_locked() {
            promise.reject_error(
                cx,
                Error::Type(c"Response body is disturbed or locked".to_owned()),
            );
            return promise;
        }

        // Step 9: Let clonedResponse be the result of cloning innerResponse.
        // Step 10: Let bodyReadPromise be a promise resolved with undefined.
        // Step 11: If innerResponse’s body is non-null, run these substeps: let reader be the
        // result of getting a reader for innerResponse’s body, and set bodyReadPromise to the
        // result of reading all bytes from reader.
        // Note: this ensures that the response body is locked until the read is complete.
        let trusted_cache = Trusted::new(self);
        let cache_job_promise = promise.clone();
        read_response_body(cx, response, move |cx, body| {
            let cache = trusted_cache.root();
            let promise = cache_job_promise;
            match body {
                // Step 12: Let operations be a list containing a cache batch operation of type
                // "put" with innerRequest and clonedResponse.
                // Step 14: Return the result of the fulfillment of bodyReadPromise: run the
                // batch in parallel, resolving or rejecting cacheJobPromise.
                Ok(body) => cache.store_entries(
                    cx,
                    &promise,
                    vec![(
                        inner_request,
                        CacheApiResponse {
                            body,
                            ..inner_response
                        },
                    )],
                ),
                Err(error) => promise.reject_error(cx, error),
            }
        });

        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-cache-delete>
    fn Delete(
        &self,
        cx: &mut JSContext,
        request: RequestOrUSVString,
        options: &CacheQueryOptions,
    ) -> RootedPromise {
        let global = self.global();
        let promise = Promise::new(cx, &global);

        // Step 1: Let r be null.
        // Step 2: If request is a Request object, set r to request’s request (and if r’s
        // method is not `GET` and options["ignoreMethod"] is false, return a promise resolved
        // with false). Else, set r to the associated request of the result of invoking the
        // initial value of Request as constructor with request as its argument; if this throws
        // an exception, return a promise rejected with that exception.
        // Note: the method check is part of the request matching done in parallel.
        let r = match request_info_to_cache_request(&global, &request) {
            Ok(r) => r,
            Err(error) => {
                promise.reject_error(cx, error);
                return promise;
            },
        };

        // Step 3: Let operations be a list containing a cache batch operation of type "delete"
        // with r and options.
        // Step 5: Run the batch in parallel, resolving cacheJobPromise with whether any entry
        // was removed.
        self.send_to_backend(cx, PendingOperation::Delete, &promise, |callback| {
            CacheStorageThreadMessage::DeleteEntries {
                cache_name: self.name.to_string(),
                request: r,
                options: native_query_options(options),
                callback,
                origin: global.origin().immutable().clone(),
            }
        });

        // Step 6: Return cacheJobPromise.
        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-cache-keys>
    fn Keys(
        &self,
        cx: &mut JSContext,
        request: Option<RequestOrUSVString>,
        options: &CacheQueryOptions,
    ) -> RootedPromise {
        // Step 3: Let realm be this’s relevant realm.
        // Note: the global is used as the realm.
        let global = self.global();

        // Step 4: Let promise be a new promise.
        // Note: step re-ordered to make it available in Step 2.
        let promise = Promise::new(cx, &global);

        // Step 1: Let r be null.
        // Step 2: If the optional argument request is not omitted, then:
        // Step 2.1: If request is a Request object, then set r to request’s request (and if r’s
        // method is not `GET` and options["ignoreMethod"] is false, return a promise resolved
        // with an empty array).
        // Step 2.2: Else if request is a string, then set r to the associated request of the
        // result of invoking the initial value of Request as constructor with request as its
        // argument. If this throws an exception, return a promise rejected with that exception.
        // Note: the method check is part of the request matching done in parallel.
        let r = match request
            .as_ref()
            .map(|request| request_info_to_cache_request(&global, request))
            .transpose()
        {
            Ok(r) => r,
            Err(error) => {
                promise.reject_error(cx, error);
                return promise;
            },
        };

        // Step 5: Run these substeps in parallel:
        self.send_to_backend(cx, PendingOperation::Keys, &promise, |callback| {
            CacheStorageThreadMessage::Keys {
                cache_name: self.name.to_string(),
                request: r,
                options: native_query_options(options),
                callback,
                origin: global.origin().immutable().clone(),
            }
        });

        // Step 6: Return promise.
        promise
    }
}

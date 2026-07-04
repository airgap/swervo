/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The `CacheStorage` interface (`window.caches` / `self.caches`),
//! <https://w3c.github.io/ServiceWorker/#cachestorage-interface>.
//!
//! Thin promise-based front end over the cache storage thread (see
//! `storage_traits::cache_storage`); all matching and persistence happens there. Replies arrive on
//! a [`GenericCallback`] and are queued back to the script thread as tasks that settle the
//! returned promises — the script thread never blocks.

use std::rc::Rc;

use dom_struct::dom_struct;
use js::context::JSContext;
use log::error;
use servo_base::generic_channel::GenericCallback;
use storage_traits::cache_storage::{
    CacheApiQueryOptions, CacheApiResponse, CacheStorageThreadMsg,
};

use crate::dom::bindings::codegen::Bindings::CacheBinding::CacheQueryOptions;
use crate::dom::bindings::codegen::Bindings::CacheStorageBinding::{
    CacheStorageMethods, MultiCacheQueryOptions,
};
use crate::dom::bindings::codegen::UnionTypes::RequestOrUSVString;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::refcounted::TrustedPromise;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_base::generic_channel::GenericSend;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::cache::{Cache, request_info_to_cache_request};
use crate::dom::response::Response;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::Promise;
use crate::task_source::SendableTaskSource;

/// Convert the bindings' `CacheQueryOptions` dictionary to the transferable form.
pub(crate) fn native_query_options(options: &CacheQueryOptions) -> CacheApiQueryOptions {
    CacheApiQueryOptions {
        ignore_search: options.ignoreSearch,
        ignore_method: options.ignoreMethod,
        ignore_vary: options.ignoreVary,
    }
}

/// A one-shot bridge from a storage-thread reply to a promise settlement task. The storage
/// thread's reply arrives on an arbitrary thread; `settle` queues the actual promise resolution
/// back onto the script thread.
pub(crate) struct CacheReplyHandler {
    trusted_promise: Option<TrustedPromise>,
    task_source: SendableTaskSource,
}

impl CacheReplyHandler {
    pub(crate) fn new(promise: &Rc<Promise>, task_source: SendableTaskSource) -> Self {
        Self {
            trusted_promise: Some(TrustedPromise::new(promise.clone())),
            task_source,
        }
    }

    /// Queue `settle(promise, value)` on the script thread. Logs if fired twice.
    pub(crate) fn settle<T, F>(&mut self, value: T, settle: F)
    where
        T: Send + 'static,
        F: FnOnce(&mut JSContext, Rc<Promise>, T) + Send + 'static,
    {
        let Some(trusted_promise) = self.trusted_promise.take() else {
            error!("Cache API reply handler fired twice.");
            return;
        };
        self.task_source
            .queue(task!(cache_api_reply: move |cx| {
                settle(cx, trusted_promise.root(), value);
            }));
    }
}

#[dom_struct]
pub(crate) struct CacheStorage {
    reflector_: Reflector,
}

impl CacheStorage {
    fn new_inherited() -> CacheStorage {
        CacheStorage {
            reflector_: Reflector::new(),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<CacheStorage> {
        reflect_dom_object_with_cx(Box::new(CacheStorage::new_inherited()), global, cx)
    }
}

impl CacheStorageMethods<crate::DomTypeHolder> for CacheStorage {
    /// <https://w3c.github.io/ServiceWorker/#cache-storage-match>
    fn Match(
        &self,
        cx: &mut JSContext,
        request: RequestOrUSVString,
        options: &MultiCacheQueryOptions,
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
            global.task_manager().dom_manipulation_task_source().to_sendable(),
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
        .expect("Could not create CacheStorage match callback");

        let _ = global.storage_threads().send(CacheStorageThreadMsg::Match {
            sender: callback,
            origin: global.origin().immutable().clone(),
            cache_id: None,
            cache_name: options.cacheName.as_ref().map(|name| name.to_string()),
            request: Some(cache_request),
            options: native_query_options(&options.parent),
            max_results: 1,
        });

        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-has>
    fn Has(&self, cx: &mut JSContext, cache_name: DOMString) -> Rc<Promise> {
        let global = self.global();
        let promise = Promise::new(cx, &global);

        let mut handler = CacheReplyHandler::new(
            &promise,
            global.task_manager().dom_manipulation_task_source().to_sendable(),
        );
        let callback = GenericCallback::new(move |message: Result<bool, _>| {
            let has = message.unwrap_or(false);
            handler.settle(has, |cx, promise, has| promise.resolve_native(cx, &has));
        })
        .expect("Could not create CacheStorage has callback");

        let _ = global.storage_threads().send(CacheStorageThreadMsg::Has {
            sender: callback,
            origin: global.origin().immutable().clone(),
            name: cache_name.to_string(),
        });

        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-open>
    fn Open(&self, cx: &mut JSContext, cache_name: DOMString) -> Rc<Promise> {
        let global = self.global();
        let promise = Promise::new(cx, &global);

        let mut handler = CacheReplyHandler::new(
            &promise,
            global.task_manager().dom_manipulation_task_source().to_sendable(),
        );
        let callback = GenericCallback::new(move |message: Result<Result<i64, String>, _>| {
            let result = match message {
                Ok(inner) => inner,
                Err(error) => Err(error.to_string()),
            };
            handler.settle(result, |cx, promise, result| match result {
                Ok(cache_id) => {
                    let global = promise.global();
                    let cache = Cache::new(cx, &global, cache_id);
                    promise.resolve_native(cx, &cache);
                },
                Err(message) => {
                    error!("Cache API open failed: {message}");
                    promise.reject_error(cx, Error::Operation(None));
                },
            });
        })
        .expect("Could not create CacheStorage open callback");

        let _ = global.storage_threads().send(CacheStorageThreadMsg::Open {
            sender: callback,
            origin: global.origin().immutable().clone(),
            name: cache_name.to_string(),
        });

        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-delete>
    fn Delete(&self, cx: &mut JSContext, cache_name: DOMString) -> Rc<Promise> {
        let global = self.global();
        let promise = Promise::new(cx, &global);

        let mut handler = CacheReplyHandler::new(
            &promise,
            global.task_manager().dom_manipulation_task_source().to_sendable(),
        );
        let callback = GenericCallback::new(move |message: Result<bool, _>| {
            let deleted = message.unwrap_or(false);
            handler.settle(deleted, |cx, promise, deleted| {
                promise.resolve_native(cx, &deleted)
            });
        })
        .expect("Could not create CacheStorage delete callback");

        let _ = global.storage_threads().send(CacheStorageThreadMsg::Delete {
            sender: callback,
            origin: global.origin().immutable().clone(),
            name: cache_name.to_string(),
        });

        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-keys>
    fn Keys(&self, cx: &mut JSContext) -> Rc<Promise> {
        let global = self.global();
        let promise = Promise::new(cx, &global);

        let mut handler = CacheReplyHandler::new(
            &promise,
            global.task_manager().dom_manipulation_task_source().to_sendable(),
        );
        let callback = GenericCallback::new(move |message: Result<Vec<String>, _>| {
            let names = message.unwrap_or_default();
            handler.settle(names, |cx, promise, names| {
                let names: Vec<DOMString> = names.into_iter().map(DOMString::from).collect();
                promise.resolve_native(cx, &names);
            });
        })
        .expect("Could not create CacheStorage keys callback");

        let _ = global.storage_threads().send(CacheStorageThreadMsg::Names {
            sender: callback,
            origin: global.origin().immutable().clone(),
        });

        promise
    }
}

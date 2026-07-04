/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Messages and data types for the Cache API storage thread
//! (<https://w3c.github.io/ServiceWorker/#cache-objects>).
//!
//! The Cache API is origin-keyed named storage of request → response pairs, exposed to script as
//! `caches` on both windows and workers, and is what a ServiceWorker serves offline content from.
//! The backing store lives in the storage component (a sibling of IndexedDB / WebStorage) so it is
//! reachable from every script thread and persists via the profile's config dir.

use malloc_size_of_derive::MallocSizeOf;
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::GenericCallback;
use servo_url::ImmutableOrigin;

/// A request as stored in (and matched against) a cache: the fields of a fetch request that the
/// Cache API's request-matching algorithm consults. Header values are raw bytes.
#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct CacheApiRequest {
    /// Fragment-less serialization is applied at match time; the URL is stored as given.
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, Vec<u8>)>,
}

/// A response as stored in a cache. The body is fully buffered (`Cache.put` reads the response
/// body to completion before storing, per spec).
///
/// The DOM `Response.type` is not round-tripped yet: cached responses are reconstructed as
/// `default`. That's invisible to the dominant `cache.match(...).then(r => respondWith(r))` use.
#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct CacheApiResponse {
    pub status: u16,
    pub status_message: Vec<u8>,
    pub headers: Vec<(String, Vec<u8>)>,
    /// The response URL (last entry of the url list), if any.
    pub url: Option<String>,
    pub body: Vec<u8>,
}

/// The `CacheQueryOptions` dictionary, in transferable form.
#[derive(Clone, Copy, Debug, Default, Deserialize, MallocSizeOf, Serialize)]
pub struct CacheApiQueryOptions {
    pub ignore_search: bool,
    pub ignore_method: bool,
    pub ignore_vary: bool,
}

/// Operations on the per-origin cache storage. All replies go through [`GenericCallback`]s so the
/// requesting script thread never blocks (every Cache API entry point is promise-returning).
#[derive(Debug, Deserialize, Serialize)]
pub enum CacheStorageThreadMsg {
    /// `CacheStorage.open`: get-or-create the named cache, returning its id.
    Open {
        sender: GenericCallback<Result<i64, String>>,
        origin: ImmutableOrigin,
        name: String,
    },

    /// `CacheStorage.has`: does the named cache exist?
    Has {
        sender: GenericCallback<bool>,
        origin: ImmutableOrigin,
        name: String,
    },

    /// `CacheStorage.delete`: remove the named cache (and its entries); false if absent.
    Delete {
        sender: GenericCallback<bool>,
        origin: ImmutableOrigin,
        name: String,
    },

    /// `CacheStorage.keys`: cache names for the origin, in creation order.
    Names {
        sender: GenericCallback<Vec<String>>,
        origin: ImmutableOrigin,
    },

    /// Query matching responses. Serves `Cache.match` / `Cache.matchAll` (with `cache_id`) and
    /// `CacheStorage.match` (with `cache_id: None`, searching the origin's caches in order,
    /// optionally restricted to `cache_name`).
    Match {
        sender: GenericCallback<Vec<CacheApiResponse>>,
        origin: ImmutableOrigin,
        /// Search this cache only (a `Cache` object's operation)...
        cache_id: Option<i64>,
        /// ...or, for `CacheStorage.match`, optionally only the cache with this name.
        cache_name: Option<String>,
        /// `None` matches everything (only legal for `matchAll`/`keys` style queries).
        request: Option<CacheApiRequest>,
        options: CacheApiQueryOptions,
        /// Cap on returned responses (1 for `match`, unbounded for `matchAll`).
        max_results: u32,
    },

    /// `Cache.put` (and the store step of `add`/`addAll`): store a request/response pair,
    /// replacing any entry the request matches.
    Put {
        sender: GenericCallback<Result<(), String>>,
        cache_id: i64,
        request: CacheApiRequest,
        response: CacheApiResponse,
    },

    /// `Cache.delete`: remove matching entries; true if any were removed.
    DeleteEntry {
        sender: GenericCallback<bool>,
        cache_id: i64,
        request: CacheApiRequest,
        options: CacheApiQueryOptions,
    },

    /// `Cache.keys`: the stored requests, optionally filtered by a query request.
    Keys {
        sender: GenericCallback<Vec<CacheApiRequest>>,
        cache_id: i64,
        request: Option<CacheApiRequest>,
        options: CacheApiQueryOptions,
    },

    /// Send a reply once pending work is flushed, then shut the thread down.
    Exit(GenericCallback<()>),
}

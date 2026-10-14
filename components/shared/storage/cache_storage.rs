/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::ops::{Deref, DerefMut};

use malloc_size_of_derive::MallocSizeOf;
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::{GenericCallback, GenericSender};
use servo_url::ImmutableOrigin;

use crate::client_storage::StorageProxyMap;

#[derive(Debug, Deserialize, Serialize)]
pub enum CacheStorageError<T> {
    Internal(T),
}

#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct CacheStorageThreadHandle {
    sender: GenericSender<CacheStorageThreadMessage>,
}

impl CacheStorageThreadHandle {
    pub fn new(sender: GenericSender<CacheStorageThreadMessage>) -> Self {
        CacheStorageThreadHandle { sender }
    }
}

impl From<CacheStorageThreadHandle> for GenericSender<CacheStorageThreadMessage> {
    fn from(handle: CacheStorageThreadHandle) -> Self {
        handle.sender
    }
}

impl From<GenericSender<CacheStorageThreadMessage>> for CacheStorageThreadHandle {
    fn from(sender: GenericSender<CacheStorageThreadMessage>) -> Self {
        CacheStorageThreadHandle::new(sender)
    }
}

impl Deref for CacheStorageThreadHandle {
    type Target = GenericSender<CacheStorageThreadMessage>;

    fn deref(&self) -> &Self::Target {
        &self.sender
    }
}

impl DerefMut for CacheStorageThreadHandle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sender
    }
}

/// A request as stored in (and matched against) a cache: the parts of a fetch request that the
/// [request matches cached item](https://w3c.github.io/ServiceWorker/#request-matches-cached-item)
/// algorithm consults. Header values are raw bytes.
#[derive(Clone, Debug, Deserialize, MallocSizeOf, PartialEq, Serialize)]
pub struct CacheApiRequest {
    /// The request's current URL, serialized. Fragments are excluded at match time.
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, Vec<u8>)>,
}

/// The `type` of a stored response, <https://fetch.spec.whatwg.org/#concept-response-type>.
#[derive(Clone, Copy, Debug, Default, Deserialize, MallocSizeOf, PartialEq, Serialize)]
pub enum CacheApiResponseType {
    Basic,
    Cors,
    #[default]
    Default,
    Error,
    Opaque,
    OpaqueRedirect,
}

/// A response as stored in a cache. The body is fully buffered: `Cache.put` reads the response
/// body to completion before the entry is stored.
#[derive(Clone, Debug, Deserialize, MallocSizeOf, PartialEq, Serialize)]
pub struct CacheApiResponse {
    pub status: u16,
    pub status_message: Vec<u8>,
    pub headers: Vec<(String, Vec<u8>)>,
    /// The response URL (last entry of the URL list), if any.
    pub url: Option<String>,
    pub response_type: CacheApiResponseType,
    pub body: Vec<u8>,
}

/// <https://w3c.github.io/ServiceWorker/#dictdef-cachequeryoptions>, in transferable form.
#[derive(Clone, Copy, Debug, Default, Deserialize, MallocSizeOf, PartialEq, Serialize)]
pub struct CacheApiQueryOptions {
    pub ignore_search: bool,
    pub ignore_method: bool,
    pub ignore_vary: bool,
}

/// Which caches a [`CacheStorageThreadMessage::Match`] searches.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum CacheMatchScope {
    /// The named cache only: a `Cache` object's `match`/`matchAll`, or `CacheStorage.match`
    /// with `cacheName` set.
    Cache(String),
    /// Every cache of the origin, in creation order: `CacheStorage.match` without `cacheName`.
    AllCaches,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum CacheStorageThreadMessage {
    /// <https://w3c.github.io/ServiceWorker/#cache-storage-has>
    HasCache {
        cache_name: String,
        callback: GenericCallback<CacheStorageThreadResponse>,
        proxy: StorageProxyMap,
        origin: ImmutableOrigin,
    },
    /// <https://w3c.github.io/ServiceWorker/#cache-storage-open>
    OpenCache {
        cache_name: String,
        callback: GenericCallback<CacheStorageThreadResponse>,
        proxy: StorageProxyMap,
        origin: ImmutableOrigin,
    },
    /// <https://w3c.github.io/ServiceWorker/#cache-storage-keys>
    CacheNames {
        callback: GenericCallback<CacheStorageThreadResponse>,
        origin: ImmutableOrigin,
    },
    /// <https://w3c.github.io/ServiceWorker/#cache-keys>
    Keys {
        cache_name: String,
        /// `None` lists every stored request.
        request: Option<CacheApiRequest>,
        options: CacheApiQueryOptions,
        callback: GenericCallback<CacheStorageThreadResponse>,
        origin: ImmutableOrigin,
    },
    /// <https://w3c.github.io/ServiceWorker/#cache-matchall>, and through it
    /// <https://w3c.github.io/ServiceWorker/#cache-match> and
    /// <https://w3c.github.io/ServiceWorker/#cache-storage-match>.
    Match {
        scope: CacheMatchScope,
        /// `None` matches every entry (only `matchAll` may omit the request).
        request: Option<CacheApiRequest>,
        options: CacheApiQueryOptions,
        /// Cap on returned responses: 1 for `match`, unbounded for `matchAll`.
        max_results: usize,
        callback: GenericCallback<CacheStorageThreadResponse>,
        origin: ImmutableOrigin,
    },
    /// The "put" operations of <https://w3c.github.io/ServiceWorker/#batch-cache-operations>,
    /// as issued by `Cache.put` (one entry) and `Cache.add`/`Cache.addAll` (one per request).
    /// Applied atomically: either every entry is stored or none is.
    Put {
        cache_name: String,
        entries: Vec<(CacheApiRequest, CacheApiResponse)>,
        callback: GenericCallback<CacheStorageThreadResponse>,
        origin: ImmutableOrigin,
    },
    /// The "delete" operation of <https://w3c.github.io/ServiceWorker/#batch-cache-operations>,
    /// as issued by <https://w3c.github.io/ServiceWorker/#cache-delete>.
    DeleteEntries {
        cache_name: String,
        request: CacheApiRequest,
        options: CacheApiQueryOptions,
        callback: GenericCallback<CacheStorageThreadResponse>,
        origin: ImmutableOrigin,
    },
    DeleteCache {
        cache_name: String,
        callback: GenericCallback<CacheStorageThreadResponse>,
        proxy: StorageProxyMap,
        origin: ImmutableOrigin,
    },
    Exit(GenericSender<()>),
}

/// Why a batch of cache operations failed.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum CacheBatchError {
    /// Two operations in one batch target the same request: script throws `InvalidStateError`.
    DuplicateRequest,
    /// The cache does not exist (it was deleted through `CacheStorage.delete`).
    NoSuchCache,
    Internal(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum CacheStorageThreadResponse {
    HasCacheResult(Result<bool, String>),
    OpenCacheResult {
        result: Result<(), String>,
        cache_name: String,
    },
    CacheNamesResult(Result<Vec<String>, String>),
    KeysResult(Result<Vec<CacheApiRequest>, String>),
    MatchResult(Result<Vec<CacheApiResponse>, String>),
    PutResult(Result<(), CacheBatchError>),
    DeleteEntriesResult(Result<bool, String>),
    DeleteCacheResult(Result<bool, String>),
}

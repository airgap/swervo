/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashMap;
use std::fmt::Debug;
use std::path::PathBuf;
use std::thread;

use log::{error, warn};
use servo_base::generic_channel::{self, GenericReceiver, GenericSender};
use servo_url::ImmutableOrigin;
use storage_traits::cache_storage::{
    CacheApiQueryOptions, CacheApiRequest, CacheApiResponse, CacheBatchError, CacheMatchScope,
    CacheStorageError, CacheStorageThreadHandle, CacheStorageThreadMessage,
    CacheStorageThreadResponse,
};
use storage_traits::client_storage::StorageProxyMap;

use self::sqlite::SqliteCacheStorageEngine;

mod sqlite;

trait CacheStorageEngine {
    type Error: Debug;

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-has>
    fn has_cache(
        &mut self,
        origin: ImmutableOrigin,
        cache_name: &str,
    ) -> Result<bool, CacheStorageError<Self::Error>>;

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-open>
    fn open_cache(
        &mut self,
        origin: ImmutableOrigin,
        cache_name: String,
        proxy_map: &StorageProxyMap,
    ) -> Result<(), CacheStorageError<Self::Error>>;

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-keys>
    fn cache_names(
        &mut self,
        origin: &ImmutableOrigin,
    ) -> Result<Vec<String>, CacheStorageError<Self::Error>>;

    /// <https://w3c.github.io/ServiceWorker/#cache-keys>
    fn keys(
        &mut self,
        origin: &ImmutableOrigin,
        cache_name: &str,
        request: Option<&CacheApiRequest>,
        options: &CacheApiQueryOptions,
    ) -> Result<Vec<CacheApiRequest>, CacheStorageError<Self::Error>>;

    /// <https://w3c.github.io/ServiceWorker/#cache-matchall>
    fn match_all(
        &mut self,
        origin: &ImmutableOrigin,
        scope: &CacheMatchScope,
        request: Option<&CacheApiRequest>,
        options: &CacheApiQueryOptions,
        max_results: usize,
    ) -> Result<Vec<CacheApiResponse>, CacheStorageError<Self::Error>>;

    /// The "put" operations of <https://w3c.github.io/ServiceWorker/#batch-cache-operations>.
    fn put(
        &mut self,
        origin: &ImmutableOrigin,
        cache_name: &str,
        entries: Vec<(CacheApiRequest, CacheApiResponse)>,
    ) -> Result<(), CacheBatchError>;

    /// The "delete" operation of <https://w3c.github.io/ServiceWorker/#batch-cache-operations>.
    fn delete_entries(
        &mut self,
        origin: &ImmutableOrigin,
        cache_name: &str,
        request: &CacheApiRequest,
        options: &CacheApiQueryOptions,
    ) -> Result<bool, CacheStorageError<Self::Error>>;

    /// <https://w3c.github.io/ServiceWorker/#dom-cachestorage-delete>
    fn delete_cache(
        &mut self,
        origin: ImmutableOrigin,
        cache_name: &str,
        proxy_map: &StorageProxyMap,
    ) -> Result<bool, CacheStorageError<Self::Error>>;
}

/// <https://w3c.github.io/ServiceWorker/#dfn-request-response-list>
#[derive(Clone, Default)]
pub struct RequestResponseList {
    /// A list of tuples consisting of a request (a request) and a response (a response)
    pub list: Vec<(CacheApiRequest, CacheApiResponse)>,
}

/// The URL used for comparison by
/// <https://w3c.github.io/ServiceWorker/#request-matches-cached-item>: the URL with its fragment
/// excluded and, with `ignoreSearch`, its query set to the empty string. Stored URLs are
/// serialized by the URL parser, so the first `#` starts the fragment and the first `?` the query.
fn comparison_url(url: &str, ignore_search: bool) -> &str {
    let without_fragment = &url[..url.find('#').unwrap_or(url.len())];
    if ignore_search {
        &without_fragment[..without_fragment.find('?').unwrap_or(without_fragment.len())]
    } else {
        without_fragment
    }
}

/// <https://fetch.spec.whatwg.org/#concept-header-list-get>: the combined value of `name`.
fn combined_header_value(headers: &[(String, Vec<u8>)], name: &str) -> Option<Vec<u8>> {
    let mut values = headers
        .iter()
        .filter(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_slice());
    let first = values.next()?;
    let mut combined = first.to_vec();
    for value in values {
        combined.extend_from_slice(b", ");
        combined.extend_from_slice(value);
    }
    Some(combined)
}

/// The field values of a response's `Vary` header, or `None` if it has none.
fn vary_field_values(headers: &[(String, Vec<u8>)]) -> Option<Vec<String>> {
    let vary = combined_header_value(headers, "vary")?;
    Some(
        String::from_utf8_lossy(&vary)
            .split(',')
            .map(|field| field.trim().to_owned())
            .filter(|field| !field.is_empty())
            .collect(),
    )
}

/// <https://w3c.github.io/ServiceWorker/#request-matches-cached-item>
pub fn request_matches_cached_item(
    request_query: &CacheApiRequest,
    request: &CacheApiRequest,
    response: Option<&CacheApiResponse>,
    options: &CacheApiQueryOptions,
) -> bool {
    // Step 1. If options["ignoreMethod"] is false and request’s method is not `GET`, return false.
    // Note: the methods that take a request query return early when its method is not `GET`
    // (e.g. step 2.3 of matchAll); that check is folded in here as well.
    if !options.ignore_method &&
        (!request.method.eq_ignore_ascii_case("GET") ||
            !request_query.method.eq_ignore_ascii_case("GET"))
    {
        return false;
    }

    // Step 2. Let queryURL be requestQuery’s url.
    // Step 3. Let cachedURL be request’s url.
    // Step 4. If options["ignoreSearch"] is true, then set cachedURL’s query and queryURL’s query
    // to the empty string.
    // Step 5. If queryURL does not equal cachedURL with exclude fragments set to true, then return
    // false.
    if comparison_url(&request_query.url, options.ignore_search) !=
        comparison_url(&request.url, options.ignore_search)
    {
        return false;
    }

    // Step 6. If response is null, options["ignoreVary"] is true, or response’s header list does
    // not contain `Vary`, then return true.
    let Some(response) = response else {
        return true;
    };
    if options.ignore_vary {
        return true;
    }
    let Some(field_values) = vary_field_values(&response.headers) else {
        return true;
    };

    // Step 7. Let fieldValues be the list containing the elements corresponding to the
    // field-values of the Vary header for the value of the header with name `Vary`.
    // Step 8. For each fieldValue in fieldValues:
    for field_value in field_values {
        // Step 8.1. If fieldValue matches "*", or the combined value given fieldValue and
        // request’s header list does not match the combined value given fieldValue and
        // requestQuery’s header list, then return false.
        if field_value == "*" ||
            combined_header_value(&request.headers, &field_value) !=
                combined_header_value(&request_query.headers, &field_value)
        {
            return false;
        }
    }

    // Step 9. Return true.
    true
}

/// <https://w3c.github.io/ServiceWorker/#query-cache>, returning the indices of the matching
/// entries in `storage`.
pub fn query_cache(
    request_query: &CacheApiRequest,
    options: &CacheApiQueryOptions,
    storage: &[(CacheApiRequest, CacheApiResponse)],
) -> Vec<usize> {
    // Step 1. Let resultList be an empty list.
    // Step 3. For each requestResponse of storage:
    // Step 3.1. Let cachedRequest be requestResponse’s request.
    // Step 3.2. Let cachedResponse be requestResponse’s response.
    // Step 3.3. If the result of running request matches cached item with requestQuery,
    // cachedRequest, cachedResponse, and options is true, then append a copy to resultList.
    storage
        .iter()
        .enumerate()
        .filter(|(_, (cached_request, cached_response))| {
            request_matches_cached_item(
                request_query,
                cached_request,
                Some(cached_response),
                options,
            )
        })
        .map(|(index, _)| index)
        .collect()
}

#[derive(Default)]
pub struct MemCacheStorageEngine {
    /// <https://w3c.github.io/ServiceWorker/#dfn-name-to-cache-map>, per origin. The map is
    /// ordered: caches are kept in creation order, as `CacheStorage.keys` and
    /// `CacheStorage.match` observe.
    name_to_cache_map: HashMap<ImmutableOrigin, Vec<(String, RequestResponseList)>>,
}

impl MemCacheStorageEngine {
    fn cache(&self, origin: &ImmutableOrigin, cache_name: &str) -> Option<&RequestResponseList> {
        self.name_to_cache_map
            .get(origin)?
            .iter()
            .find(|(name, _)| name == cache_name)
            .map(|(_, cache)| cache)
    }

    fn cache_mut(
        &mut self,
        origin: &ImmutableOrigin,
        cache_name: &str,
    ) -> Option<&mut RequestResponseList> {
        self.name_to_cache_map
            .get_mut(origin)?
            .iter_mut()
            .find(|(name, _)| name == cache_name)
            .map(|(_, cache)| cache)
    }
}

impl CacheStorageEngine for MemCacheStorageEngine {
    type Error = ();

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-has>
    /// The parallel steps.
    fn has_cache(
        &mut self,
        origin: ImmutableOrigin,
        cache_name: &str,
    ) -> Result<bool, CacheStorageError<Self::Error>> {
        // Step 2.1:For each key → value of the relevant name to cache map:
        // Step 2.1.1: If cacheName matches key, resolve promise with true and abort these steps.
        // Step 2.2: Resolve promise with false.
        // Note: promise resolved in the callback in CacheStorage.
        Ok(self.cache(&origin, cache_name).is_some())
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-open>
    /// The parallel steps.
    fn open_cache(
        &mut self,
        origin: ImmutableOrigin,
        cache_name: String,
        proxy_map: &StorageProxyMap,
    ) -> Result<(), CacheStorageError<Self::Error>> {
        // Step 2.1: For each key → value of the relevant name to cache map:
        // Step 2.1.1: If cacheName matches key, then:
        // Resolve promise with a new Cache object that represents value.
        // Note: promise resolved in script.
        if self.cache(&origin, &cache_name).is_some() {
            // Step 2.1.2: Abort these steps.
            return Ok(());
        }

        // Step 2.2: Let cache be a new request response list.
        let cache = RequestResponseList::default();

        // Step 2.3: Set the relevant name to cache map[cacheName] to cache.
        // If this cache write operation failed due to exceeding the granted quota limit,
        // reject promise with a QuotaExceededError and abort these steps.
        // Note: there are no quota checks storage side at this point.
        let Ok(response) = proxy_map
            .handle
            .create_database(proxy_map.bottle_id, cache_name.clone())
            .recv()
        else {
            return Err(CacheStorageError::Internal(()));
        };
        if response.is_err() {
            return Err(CacheStorageError::Internal(()));
        }
        self.name_to_cache_map
            .entry(origin)
            .or_default()
            .push((cache_name, cache));

        // Step 2.4: Resolve promise with a new Cache object that represents cache.
        // Note: promise resolved in script.
        Ok(())
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-keys>
    /// The parallel steps.
    fn cache_names(
        &mut self,
        origin: &ImmutableOrigin,
    ) -> Result<Vec<String>, CacheStorageError<Self::Error>> {
        // Step 2.1: Let cacheKeys be the result of getting the keys of the relevant name to
        // cache map.
        // Note: the keys are in creation order.
        Ok(self
            .name_to_cache_map
            .get(origin)
            .map(|caches| caches.iter().map(|(name, _)| name.clone()).collect())
            .unwrap_or_default())
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-keys>
    /// The parallel steps.
    fn keys(
        &mut self,
        origin: &ImmutableOrigin,
        cache_name: &str,
        request: Option<&CacheApiRequest>,
        options: &CacheApiQueryOptions,
    ) -> Result<Vec<CacheApiRequest>, CacheStorageError<Self::Error>> {
        // Note: a cache deleted from the name to cache map has no entries left to report.
        let Some(relevant_cache) = self.cache(origin, cache_name) else {
            return Ok(Vec::new());
        };

        // Step 5.1: Let requests be an empty list.
        // Step 5.2: If the optional argument request is omitted, then:
        // Step 5.2.1: For each requestResponse of the relevant request response list:
        // Step 5.2.1.1: Add requestResponse’s request to requests.
        // Step 5.3: Else:
        // Step 5.3.1: Let requestResponses be the result of running Query Cache with r and
        // options.
        // Step 5.3.2: For each requestResponse of requestResponses:
        // Step 5.3.2.1: Add requestResponse’s request to requests.
        let requests = match request {
            None => relevant_cache
                .list
                .iter()
                .map(|(request, _)| request.clone())
                .collect(),
            Some(request) => query_cache(request, options, &relevant_cache.list)
                .into_iter()
                .map(|index| relevant_cache.list[index].0.clone())
                .collect(),
        };
        Ok(requests)
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-matchall>
    /// The parallel steps, also serving `Cache.match` and `CacheStorage.match`.
    fn match_all(
        &mut self,
        origin: &ImmutableOrigin,
        scope: &CacheMatchScope,
        request: Option<&CacheApiRequest>,
        options: &CacheApiQueryOptions,
        max_results: usize,
    ) -> Result<Vec<CacheApiResponse>, CacheStorageError<Self::Error>> {
        // <https://w3c.github.io/ServiceWorker/#cache-storage-match>
        // Step 2.1: If options["cacheName"] exists, then: for each key → value of the relevant
        // name to cache map, if options["cacheName"] matches key, resolve promise with the
        // result of running match on a new Cache object representing value.
        // Step 2.2: Else: for each cacheName → cache of the relevant name to cache map, run
        // match on a new Cache object representing cache, and resolve with the first
        // non-undefined result.
        let caches: Vec<&RequestResponseList> = match scope {
            CacheMatchScope::Cache(cache_name) => {
                self.cache(origin, cache_name).into_iter().collect()
            },
            CacheMatchScope::AllCaches => self
                .name_to_cache_map
                .get(origin)
                .map(|caches| caches.iter().map(|(_, cache)| cache).collect())
                .unwrap_or_default(),
        };

        // Step 2.1: Let responses be an empty list.
        let mut responses = Vec::new();
        for cache in caches {
            match request {
                // Step 2.2: If the optional argument request is omitted, then:
                // Step 2.2.1: For each requestResponse of the relevant request response list:
                // add a copy of requestResponse’s response to responses.
                None => responses.extend(cache.list.iter().map(|(_, response)| response.clone())),
                // Step 2.3: Else:
                // Step 2.3.1: Let requestResponses be the result of running Query Cache with r
                // and options.
                // Step 2.3.2: For each requestResponse of requestResponses: add a copy of
                // requestResponse’s response to responses.
                Some(request) => responses.extend(
                    query_cache(request, options, &cache.list)
                        .into_iter()
                        .map(|index| cache.list[index].1.clone()),
                ),
            }
            if responses.len() >= max_results {
                responses.truncate(max_results);
                break;
            }
        }
        Ok(responses)
    }

    /// <https://w3c.github.io/ServiceWorker/#batch-cache-operations>, for "put" operations.
    fn put(
        &mut self,
        origin: &ImmutableOrigin,
        cache_name: &str,
        entries: Vec<(CacheApiRequest, CacheApiResponse)>,
    ) -> Result<(), CacheBatchError> {
        // Step 1: Let cache be the relevant request response list.
        let Some(cache) = self.cache_mut(origin, cache_name) else {
            return Err(CacheBatchError::NoSuchCache);
        };

        // Step 2: Let backupCache be a new request response list that is a copy of cache.
        // Note: the operations are applied to a copy that replaces cache on success, which is
        // equivalent to restoring backupCache on failure (step 4.1).
        let mut working = cache.list.clone();

        // Step 3: Let addedItems be an empty list.
        let mut added_items: Vec<(CacheApiRequest, CacheApiResponse)> = Vec::new();
        let options = CacheApiQueryOptions::default();

        // Step 4.2: For each operation in operations:
        for (request, response) in entries {
            // Step 4.2.3: If the result of running Query Cache with operation’s request,
            // operation’s options, and addedItems is not empty, throw an InvalidStateError.
            if !query_cache(&request, &options, &added_items).is_empty() {
                return Err(CacheBatchError::DuplicateRequest);
            }

            // Step 4.2.4: Let requestResponses be the result of running Query Cache with
            // operation’s request and operation’s options.
            // Step 4.2.6.4 / 4.2.6.5: For each requestResponse of requestResponses, replace the
            // item from cache whose value matches requestResponse with operation’s request and
            // response, else append the new item.
            // Note: the matching entries are removed and the new entry appended.
            let mut matched = query_cache(&request, &options, &working);
            matched.sort_unstable();
            for index in matched.into_iter().rev() {
                working.remove(index);
            }
            working.push((request.clone(), response.clone()));

            // Step 4.2.6.7: Append operation’s request and response to addedItems.
            added_items.push((request, response));
        }

        // Step 4.3: Return resultList.
        cache.list = working;
        Ok(())
    }

    /// <https://w3c.github.io/ServiceWorker/#batch-cache-operations>, for a "delete" operation.
    fn delete_entries(
        &mut self,
        origin: &ImmutableOrigin,
        cache_name: &str,
        request: &CacheApiRequest,
        options: &CacheApiQueryOptions,
    ) -> Result<bool, CacheStorageError<Self::Error>> {
        let Some(cache) = self.cache_mut(origin, cache_name) else {
            return Ok(false);
        };
        // Step 4.2.4: Let requestResponses be the result of running Query Cache with
        // operation’s request and operation’s options.
        let mut matched = query_cache(request, options, &cache.list);
        // Step 4.2.5.1: For each requestResponse of requestResponses: remove the item whose
        // value matches requestResponse from cache.
        matched.sort_unstable();
        let deleted = !matched.is_empty();
        for index in matched.into_iter().rev() {
            cache.list.remove(index);
        }
        Ok(deleted)
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-cachestorage-delete>
    /// The "running the algorithm specified in has" part, and the parallel steps.
    fn delete_cache(
        &mut self,
        origin: ImmutableOrigin,
        cache_name: &str,
        proxy: &StorageProxyMap,
    ) -> Result<bool, CacheStorageError<Self::Error>> {
        // Step 1: Let promise be the result of running the algorithm specified in has(cacheName) method with cacheName.
        // Step 2: Return the result of reacting to promise with a fulfillment handler that,
        // when called with argument cacheExists,
        // performs the following substeps:
        // Note: skipping th promise here.

        // Step 2.1: If cacheExists is false, then:
        // Step 2.1.1: Resolve promise with false.
        // Note: done below using the result of the removal.

        // Step 2.3: Run the following substeps in parallel:
        // Step 2.3.1: Remove the relevant name to cache map[cacheName].
        let has = match self.name_to_cache_map.get_mut(&origin) {
            Some(caches) => {
                let before = caches.len();
                caches.retain(|(name, _)| name != cache_name);
                caches.len() != before
            },
            None => false,
        };

        if has {
            let Ok(response) = proxy
                .handle
                .delete_database(proxy.bottle_id, cache_name.to_string())
                .recv()
            else {
                return Err(CacheStorageError::Internal(()));
            };
            if response.is_err() {
                return Err(CacheStorageError::Internal(()));
            }
        }

        // Step 2.3.2: Resolve promise with true.
        // Note: promise resolved in script.
        Ok(has)
    }
}

pub trait CacheStorageThreadFactory {
    fn new(config_dir: Option<PathBuf>, temporary_storage: bool) -> Self;
}

impl CacheStorageThreadFactory for CacheStorageThreadHandle {
    /// Spawn the cache storage thread. With a config dir and without temporary storage, caches
    /// persist in a SQLite database in the config dir; otherwise they live only in memory.
    fn new(config_dir: Option<PathBuf>, temporary_storage: bool) -> CacheStorageThreadHandle {
        let (generic_sender, generic_receiver) = generic_channel::channel().unwrap();
        let database_path = config_dir
            .filter(|_| !temporary_storage)
            .map(|dir| dir.join(sqlite::DB_FILE_NAME));
        let sender_clone = generic_sender.clone();
        thread::Builder::new()
            .name("CacheStorageThread".to_owned())
            .spawn(move || {
                let engine = database_path.and_then(|path| {
                    SqliteCacheStorageEngine::open(&path)
                        .inspect_err(|error| {
                            warn!(
                                "Failed to open Cache API storage at {path:?}, \
                                 falling back to memory: {error:?}"
                            )
                        })
                        .ok()
                });
                match engine {
                    Some(engine) => {
                        CacheStorageThread::new(sender_clone, generic_receiver, engine).start()
                    },
                    None => CacheStorageThread::new(
                        sender_clone,
                        generic_receiver,
                        MemCacheStorageEngine::default(),
                    )
                    .start(),
                }
            })
            .expect("Thread spawning failed");

        CacheStorageThreadHandle::new(generic_sender)
    }
}

struct CacheStorageThread<E: CacheStorageEngine> {
    receiver: GenericReceiver<CacheStorageThreadMessage>,
    // Note: a sender to self might be required later for the storage engine.
    _sender: GenericSender<CacheStorageThreadMessage>,
    engine: E,
}

impl<E> CacheStorageThread<E>
where
    E: CacheStorageEngine,
{
    pub fn new(
        _sender: GenericSender<CacheStorageThreadMessage>,
        receiver: GenericReceiver<CacheStorageThreadMessage>,
        engine: E,
    ) -> CacheStorageThread<E> {
        CacheStorageThread {
            _sender,
            receiver,
            engine,
        }
    }

    pub fn start(&mut self) {
        while let Ok(message) = self.receiver.recv() {
            match message {
                CacheStorageThreadMessage::HasCache {
                    cache_name,
                    callback,
                    proxy: _,
                    origin,
                } => {
                    let result = self.engine.has_cache(origin.clone(), &cache_name);
                    if callback
                        .send(CacheStorageThreadResponse::HasCacheResult(
                            result.map_err(|e| format!("{:?}", e)),
                        ))
                        .is_err()
                    {
                        error!("Failed to send response to script for HasCache message.");
                    }
                },
                CacheStorageThreadMessage::OpenCache {
                    cache_name,
                    callback,
                    proxy,
                    origin,
                } => {
                    let result = self
                        .engine
                        .open_cache(origin.clone(), cache_name.clone(), &proxy);
                    if callback
                        .send(CacheStorageThreadResponse::OpenCacheResult {
                            result: result.map_err(|e| format!("{:?}", e)),
                            cache_name,
                        })
                        .is_err()
                    {
                        error!("Failed to send response to script for OpenCache message.");
                    }
                },
                CacheStorageThreadMessage::CacheNames { callback, origin } => {
                    let result = self.engine.cache_names(&origin);
                    if callback
                        .send(CacheStorageThreadResponse::CacheNamesResult(
                            result.map_err(|e| format!("{:?}", e)),
                        ))
                        .is_err()
                    {
                        error!("Failed to send response to script for CacheNames message.");
                    }
                },
                CacheStorageThreadMessage::Keys {
                    cache_name,
                    request,
                    options,
                    callback,
                    origin,
                } => {
                    let result = self
                        .engine
                        .keys(&origin, &cache_name, request.as_ref(), &options);
                    if callback
                        .send(CacheStorageThreadResponse::KeysResult(
                            result.map_err(|e| format!("{:?}", e)),
                        ))
                        .is_err()
                    {
                        error!("Failed to send response to script for Keys message.");
                    }
                },
                CacheStorageThreadMessage::Match {
                    scope,
                    request,
                    options,
                    max_results,
                    callback,
                    origin,
                } => {
                    let result = self.engine.match_all(
                        &origin,
                        &scope,
                        request.as_ref(),
                        &options,
                        max_results,
                    );
                    if callback
                        .send(CacheStorageThreadResponse::MatchResult(
                            result.map_err(|e| format!("{:?}", e)),
                        ))
                        .is_err()
                    {
                        error!("Failed to send response to script for Match message.");
                    }
                },
                CacheStorageThreadMessage::Put {
                    cache_name,
                    entries,
                    callback,
                    origin,
                } => {
                    let result = self.engine.put(&origin, &cache_name, entries);
                    if callback
                        .send(CacheStorageThreadResponse::PutResult(result))
                        .is_err()
                    {
                        error!("Failed to send response to script for Put message.");
                    }
                },
                CacheStorageThreadMessage::DeleteEntries {
                    cache_name,
                    request,
                    options,
                    callback,
                    origin,
                } => {
                    let result =
                        self.engine
                            .delete_entries(&origin, &cache_name, &request, &options);
                    if callback
                        .send(CacheStorageThreadResponse::DeleteEntriesResult(
                            result.map_err(|e| format!("{:?}", e)),
                        ))
                        .is_err()
                    {
                        error!("Failed to send response to script for DeleteEntries message.");
                    }
                },
                CacheStorageThreadMessage::DeleteCache {
                    cache_name,
                    callback,
                    proxy,
                    origin,
                } => {
                    let result = self
                        .engine
                        .delete_cache(origin.clone(), &cache_name, &proxy);
                    if callback
                        .send(CacheStorageThreadResponse::DeleteCacheResult(
                            result.map_err(|e| format!("{:?}", e)),
                        ))
                        .is_err()
                    {
                        error!("Failed to send response to script for DeleteCache message.");
                    }
                },
                CacheStorageThreadMessage::Exit(sender) => {
                    let _ = sender.send(());
                    break;
                },
            }
        }
    }
}

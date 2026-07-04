/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/ServiceWorker/#cachestorage-interface

[SecureContext, Exposed=(Window,Worker), Pref="dom_cache_api_enabled"]
interface CacheStorage {
  [NewObject] Promise<(Response or undefined)> match(RequestInfo request, optional MultiCacheQueryOptions options = {});
  [NewObject] Promise<boolean> has(DOMString cacheName);
  [NewObject] Promise<Cache> open(DOMString cacheName);
  [NewObject] Promise<boolean> delete(DOMString cacheName);
  [NewObject] Promise<sequence<DOMString>> keys();
};

dictionary MultiCacheQueryOptions : CacheQueryOptions {
  DOMString cacheName;
};

// https://w3c.github.io/ServiceWorker/#self-caches
partial interface mixin WindowOrWorkerGlobalScope {
  [SecureContext, SameObject, Pref="dom_cache_api_enabled"] readonly attribute CacheStorage caches;
};

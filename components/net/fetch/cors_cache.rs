/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A partitioned, profile-local CORS preflight cache shared by concurrent fetches.
//! <https://fetch.spec.whatwg.org/#cors-preflight-cache>

use std::sync::Arc;
use std::time::{Duration, Instant};

use http::Method;
use http::header::HeaderName;
use net_traits::partition_key::NetworkPartitionKey;
use net_traits::request::{CredentialsMode, Origin, Request};
use parking_lot::Mutex;
use servo_url::ServoUrl;

const MAX_ENTRIES: usize = 1024;
const MAX_AGE: Duration = Duration::from_secs(600);

/// The method or header authorized by a successful preflight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeaderOrMethod {
    HeaderData(HeaderName),
    MethodData(Method),
}

/// An entry in the CORS cache.
#[derive(Clone, Debug)]
pub struct CorsCacheEntry {
    pub partition_key: NetworkPartitionKey,
    pub origin: Origin,
    pub url: ServoUrl,
    pub max_age: Duration,
    pub credentials: bool,
    pub header_or_method: HeaderOrMethod,
    created: Instant,
}

fn matches_request(
    entry: &CorsCacheEntry,
    request: &Request,
    key: &NetworkPartitionKey,
    url: &ServoUrl,
) -> bool {
    entry.partition_key == *key &&
        entry.origin == request.origin &&
        entry.url == *url &&
        (entry.credentials || request.credentials_mode != CredentialsMode::Include)
}

fn cleanup(entries: &mut Vec<CorsCacheEntry>) {
    let now = Instant::now();
    entries.retain(|entry| now.duration_since(entry.created) < entry.max_age);
}

fn insert(entries: &mut Vec<CorsCacheEntry>, mut entry: CorsCacheEntry) {
    entry.max_age = entry.max_age.min(MAX_AGE);
    if entry.max_age.is_zero() {
        return;
    }
    if entries.len() >= MAX_ENTRIES {
        entries.remove(0);
    }
    entries.push(entry);
}

/// Clones share state within one profile. Locks are held only for cache operations,
/// never while a preflight or response is awaited.
#[derive(Clone, Default)]
pub struct CorsCache(Arc<Mutex<Vec<CorsCacheEntry>>>);

impl CorsCache {
    fn lookup(
        &mut self,
        request: &Request,
        query: HeaderOrMethod,
        new_max_age: Option<Duration>,
    ) -> bool {
        let Some(partition_key) = request.network_partition_key() else {
            return false;
        };
        let url = request.current_url();
        let mut entries = self.0.lock();
        cleanup(&mut entries);
        if let Some(entry) = entries.iter_mut().find(|entry| {
            matches_request(entry, request, &partition_key, &url) && entry.header_or_method == query
        }) {
            if let Some(age) = new_max_age {
                // An anonymous refresh must not restart an older credentialed grant's lifetime.
                entry.max_age = age.min(MAX_AGE);
            }
            return true;
        }
        if let Some(age) = new_max_age {
            insert(
                &mut entries,
                CorsCacheEntry {
                    partition_key,
                    origin: request.origin.clone(),
                    url,
                    max_age: age,
                    credentials: request.credentials_mode == CredentialsMode::Include,
                    header_or_method: query,
                    created: Instant::now(),
                },
            );
        }
        false
    }

    /// Remove expired authorizations.
    pub fn cleanup(&mut self) {
        cleanup(&mut self.0.lock());
    }

    /// Clear authorizations in all fetches sharing this profile's cache.
    pub fn clear(&self) {
        self.0.lock().clear();
    }

    /// A failed preflight invalidates this request's grants, including credentialed
    /// entries, while preserving authorizations in other top-level partitions.
    pub fn clear_request(&self, request: &Request) {
        let Some(key) = request.network_partition_key() else {
            return;
        };
        let url = request.current_url();
        self.0.lock().retain(|entry| {
            entry.partition_key != key || entry.origin != request.origin || entry.url != url
        });
    }

    pub fn match_header(&mut self, request: &Request, header_name: &HeaderName) -> bool {
        self.lookup(
            request,
            HeaderOrMethod::HeaderData(header_name.clone()),
            None,
        )
    }

    pub fn match_header_and_update(
        &mut self,
        request: &Request,
        header_name: &HeaderName,
        new_max_age: Duration,
    ) -> bool {
        self.lookup(
            request,
            HeaderOrMethod::HeaderData(header_name.clone()),
            Some(new_max_age),
        )
    }

    pub fn match_method(&mut self, request: &Request, method: Method) -> bool {
        self.lookup(request, HeaderOrMethod::MethodData(method), None)
    }

    pub fn match_method_and_update(
        &mut self,
        request: &Request,
        method: Method,
        new_max_age: Duration,
    ) -> bool {
        self.lookup(
            request,
            HeaderOrMethod::MethodData(method),
            Some(new_max_age),
        )
    }

    pub fn insert(&mut self, entry: CorsCacheEntry) {
        let mut entries = self.0.lock();
        cleanup(&mut entries);
        insert(&mut entries, entry);
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Schemeful top-level-site keys for network caches.

use malloc_size_of_derive::MallocSizeOf;
use serde::{Deserialize, Serialize};
use servo_url::{Host, ImmutableOrigin};

use crate::pub_domains::reg_suffix;

/// A schemeful site, without a port or a subdomain below the registrable domain.
#[derive(Clone, Debug, Deserialize, Eq, Hash, MallocSizeOf, PartialEq, Serialize)]
pub struct NetworkPartitionKey {
    scheme: String,
    host: Host,
}

impl NetworkPartitionKey {
    /// Opaque or unavailable top-level origins deliberately disable cache reuse.
    /// They must never fall back to the embedded resource's origin.
    pub fn from_origin(origin: &ImmutableOrigin) -> Option<Self> {
        let ImmutableOrigin::Tuple(scheme, host, _) = origin else {
            return None;
        };
        let host = match host {
            Host::Domain(domain) => Host::Domain(reg_suffix(domain).to_owned()),
            host => host.clone(),
        };
        Some(Self {
            scheme: scheme.clone(),
            host,
        })
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use content_security_policy::CspList;
use http::HeaderMap;
use malloc_size_of_derive::MallocSizeOf;
use serde::{Deserialize, Serialize};

use crate::ReferrerPolicy;

/// When a policy container is associated with a request, it has an additional state of "Client". As
/// per the spec:
///
/// `"client" is changed to a policy container during fetching. It provides a convenient way for
/// standards to not have to set request’s policy container.`
///
/// This can be achieved with an `Option` however this struct is used with the intent to reduce
/// ambiguity when mapping our implementation to the spec.
///
/// <https://fetch.spec.whatwg.org/#concept-request-policy-container>
#[derive(Clone, Debug, Default, Deserialize, MallocSizeOf, Serialize)]
pub enum RequestPolicyContainer {
    #[default]
    Client,
    PolicyContainer(PolicyContainer),
}

/// <https://html.spec.whatwg.org/multipage/#policy-containers>
#[derive(Clone, Debug, Default, Deserialize, MallocSizeOf, Serialize)]
pub struct PolicyContainer {
    #[ignore_malloc_size_of = "Defined in rust-content-security-policy"]
    /// <https://html.spec.whatwg.org/multipage/#policy-container-csp-list>
    pub csp_list: Option<CspList>,
    /// <https://html.spec.whatwg.org/multipage/#policy-container-embedder-policy>
    pub embedder_policy: EmbedderPolicy,
    /// <https://html.spec.whatwg.org/multipage/#policy-container-referrer-policy>
    pub referrer_policy: ReferrerPolicy,
}

impl PolicyContainer {
    pub fn set_csp_list(&mut self, csp_list: Option<CspList>) {
        self.csp_list = csp_list;
    }

    pub fn set_referrer_policy(&mut self, referrer_policy: ReferrerPolicy) {
        self.referrer_policy = referrer_policy;
    }

    pub fn get_referrer_policy(&self) -> ReferrerPolicy {
        // https://w3c.github.io/webappsec-referrer-policy/#referrer-policy-empty-string
        if self.referrer_policy == ReferrerPolicy::EmptyString {
            return ReferrerPolicy::default();
        }

        self.referrer_policy
    }
}

/// <https://html.spec.whatwg.org/multipage/#embedder-policy>
#[derive(Clone, Debug, Default, Deserialize, MallocSizeOf, Serialize)]
pub struct EmbedderPolicy {
    /// <https://html.spec.whatwg.org/multipage/#embedder-policy-value-2>
    pub value: EmbedderPolicyValue,
}

impl EmbedderPolicy {
    /// Obtain the supported enforcing embedder policy from a response. COEP is a
    /// structured-field *item*, not a comma-separated list or a quoted string.
    ///
    /// <https://html.spec.whatwg.org/multipage/#obtain-an-embedder-policy>
    ///
    /// Reporting and credentialless are not implemented yet. This does not grant
    /// cross-origin isolation: COOP, navigation checks and agent-cluster isolation
    /// are separate prerequisites before shared memory can be enabled (GATO-42).
    pub fn from_headers(headers: Option<&HeaderMap>, is_secure_context: bool) -> Self {
        if !is_secure_context {
            return Self::default();
        }
        let Some(headers) = headers else {
            return Self::default();
        };
        let mut values = headers.get_all("cross-origin-embedder-policy").iter();
        let Some(value) = values.next() else {
            return Self::default();
        };
        // Repeated fields combine with a comma and cannot form a valid sf-item.
        if values.next().is_some() {
            return Self::default();
        }
        let Ok(item) = sfv::Parser::new(value.as_bytes()).parse::<sfv::Item>() else {
            return Self::default();
        };
        let value = match item.bare_item {
            sfv::BareItem::Token(token) if token.as_str() == "require-corp" => {
                EmbedderPolicyValue::RequireCorp
            },
            _ => EmbedderPolicyValue::UnsafeNone,
        };
        Self { value }
    }
}

/// <https://html.spec.whatwg.org/multipage/#embedder-policy-value>
#[derive(Clone, Copy, Debug, Default, Deserialize, MallocSizeOf, Serialize)]
pub enum EmbedderPolicyValue {
    /// "unsafe-none"
    #[default]
    UnsafeNone,
    /// "require-corp"
    RequireCorp,
    // TODO "credentialless"
}

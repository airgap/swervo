/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use http::{HeaderMap, HeaderValue};
use net_traits::policy_container::{EmbedderPolicy, EmbedderPolicyValue};

fn requires_corp(values: &[&str], secure: bool) -> bool {
    let mut headers = HeaderMap::new();
    for value in values {
        headers.append(
            "cross-origin-embedder-policy",
            HeaderValue::from_str(value).unwrap(),
        );
    }
    matches!(
        EmbedderPolicy::from_headers(Some(&headers), secure).value,
        EmbedderPolicyValue::RequireCorp
    )
}

#[test]
fn require_corp_is_a_case_sensitive_structured_field_token() {
    assert!(requires_corp(&["require-corp"], true));
    assert!(requires_corp(&["  require-corp  "], true));
    assert!(requires_corp(
        &[r#"require-corp;report-to="endpoint""#],
        true
    ));
    assert!(requires_corp(
        &[r#"require-corp;unknown=?1;token=abc"#],
        true
    ));
    for value in [
        "",
        "unsafe-none",
        "Require-Corp",
        r#""require-corp""#,
        "require-corp, require-corp",
        "require-corp;",
        "require-corp;bad=",
        r#"require-corp;report-to="unterminated"#,
        "?1",
        "(require-corp)",
    ] {
        assert!(!requires_corp(&[value], true), "{value:?}");
    }
}

#[test]
fn repeated_fields_cannot_silently_select_one_policy() {
    assert!(!requires_corp(&["require-corp", "require-corp"], true));
    assert!(!requires_corp(&["require-corp", "unsafe-none"], true));
    assert!(!requires_corp(&["unsafe-none", "require-corp"], true));
}

#[test]
fn insecure_and_missing_headers_do_not_enforce_coep() {
    assert!(!requires_corp(&["require-corp"], false));
    assert!(!requires_corp(&[], true));
    assert!(matches!(
        EmbedderPolicy::from_headers(None, true).value,
        EmbedderPolicyValue::UnsafeNone
    ));
}

#[test]
fn report_only_does_not_enforce_and_unsupported_credentialless_stays_disabled() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "cross-origin-embedder-policy-report-only",
        HeaderValue::from_static("require-corp"),
    );
    assert!(matches!(
        EmbedderPolicy::from_headers(Some(&headers), true).value,
        EmbedderPolicyValue::UnsafeNone
    ));
    assert!(!requires_corp(&["credentialless"], true));
}

/// Exercise the existing CORP enforcement with an actual fetched response and
/// the policy produced by the header parser, rather than just testing enum values.
#[test]
fn response_policy_controls_cross_origin_no_cors_fetches() {
    use http_body_util::combinators::BoxBody;
    use hyper::body::{Bytes, Incoming};
    use hyper::{Request as HyperRequest, Response as HyperResponse};
    use net_traits::policy_container::PolicyContainer;
    use net_traits::request::{Origin, Referrer, RequestBuilder, RequestClient, RequestMode};
    use servo_base::id::TEST_WEBVIEW_ID;
    use servo_url::ServoUrl;

    for (coep, corp, allowed) in [
        ("unsafe-none", None, true),
        ("require-corp", None, false),
        ("require-corp", Some("cross-origin"), true),
        ("require-corp", Some("same-origin"), false),
    ] {
        let handler =
            move |_: HyperRequest<Incoming>,
                  response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
                *response.body_mut() = crate::make_body(b"resource".to_vec());
                if let Some(corp) = corp {
                    response.headers_mut().insert(
                        "cross-origin-resource-policy",
                        HeaderValue::from_static(corp),
                    );
                }
            };
        let (server, url) = crate::make_server(handler);
        let origin = ServoUrl::parse("http://other.example/").unwrap().origin();
        let mut headers = HeaderMap::new();
        headers.insert(
            "cross-origin-embedder-policy",
            HeaderValue::from_static(coep),
        );
        let policy = PolicyContainer {
            embedder_policy: EmbedderPolicy::from_headers(Some(&headers), true),
            ..Default::default()
        };
        let request = RequestBuilder::new(Some(TEST_WEBVIEW_ID), url, Referrer::NoReferrer)
            .origin(origin.clone())
            .mode(RequestMode::NoCors)
            .policy_container(policy.clone())
            .client(RequestClient {
                source: embedder_traits::RequestSource::Unknown,
                top_level_origin: Some(origin.clone()),
                preloaded_resources: Default::default(),
                policy_container: policy,
                origin: Origin::Origin(origin),
                is_nested_browsing_context: false,
                insecure_requests_policy: net_traits::request::InsecureRequestsPolicy::DoNotUpgrade,
                has_trustworthy_ancestor_origin: false,
            })
            .build();
        let response = crate::fetch(request, None);
        server.close();
        assert_eq!(
            !response.is_network_error(),
            allowed,
            "COEP={coep}, CORP={corp:?}"
        );
    }
}

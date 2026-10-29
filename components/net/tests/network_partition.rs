/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use headers::{CacheControl, HeaderMapExt};
use http::header::{self, HeaderName, HeaderValue};
use http::{Method, Request as HyperRequest, Response as HyperResponse};
use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Incoming};
use net::fetch::cors_cache::CorsCache;
use net::http_cache::HttpCache;
use net_traits::blob_url_store::UrlWithBlobClaim;
use net_traits::partition_key::NetworkPartitionKey;
use net_traits::request::{
    InsecureRequestsPolicy, Origin, Referrer, Request, RequestBuilder, RequestClient, RequestMode,
};
use net_traits::response::{CacheState, Response, ResponseBody};
use net_traits::{ResourceFetchTiming, ResourceTimingType};
use servo_url::{ImmutableOrigin, ServoUrl};

use crate::{fetch_with_context, make_body, make_server, new_fetch_context};

pub(crate) fn client(origin: ImmutableOrigin) -> RequestClient {
    RequestClient {
        top_level_origin: Some(origin.clone()),
        origin: Origin::Origin(origin),
        preloaded_resources: Default::default(),
        policy_container: Default::default(),
        is_nested_browsing_context: false,
        insecure_requests_policy: InsecureRequestsPolicy::DoNotUpgrade,
        has_trustworthy_ancestor_origin: true,
    }
}

fn request(top: &str, url: &ServoUrl) -> Request {
    let mut client = client(url.origin());
    client.top_level_origin = Some(ServoUrl::parse(top).unwrap().origin());
    RequestBuilder::new(
        None,
        UrlWithBlobClaim::new(url.clone(), None),
        Referrer::NoReferrer,
    )
    .client(client)
    .origin(url.origin())
    .policy_container(Default::default())
    .build()
}

fn response(url: &ServoUrl) -> Response {
    let response = Response::new(
        url.clone(),
        ResourceFetchTiming::new(ResourceTimingType::Resource),
    );
    *response.body.lock() = ResponseBody::Done(b"site A only".to_vec());
    let mut response = response;
    response
        .headers
        .typed_insert(CacheControl::new().with_max_age(Duration::from_secs(3600)));
    response
}

#[test]
fn schemeful_sites_share_only_the_registrable_domain_and_scheme() {
    let key = |url: &str| NetworkPartitionKey::from_origin(&ServoUrl::parse(url).unwrap().origin());
    assert_eq!(
        key("https://one.example.com:444/"),
        key("https://two.example.com:555/")
    );
    assert_ne!(key("https://a.example/"), key("https://b.example/"));
    assert_ne!(
        key("https://a.example.com./"),
        key("https://b.attacker.com./")
    );
    assert_ne!(
        key("https://a.example.co.uk./"),
        key("https://b.attacker.co.uk./")
    );
    assert_eq!(
        key("https://a.example.com./"),
        key("https://b.example.com./")
    );
    assert_ne!(key("https://example.com/"), key("https://example.com./"));
    assert_ne!(
        key("https://a.example.com../"),
        key("https://b.attacker.com../")
    );
    assert_ne!(key("http://a.example/"), key("https://a.example/"));
    assert_eq!(key("http://127.0.0.1:444/"), key("http://127.0.0.1:555/"));
    assert_ne!(key("http://127.0.0.1/"), key("http://127.0.0.2/"));
    assert_eq!(
        NetworkPartitionKey::from_origin(&ImmutableOrigin::new_opaque()),
        None
    );
}

#[test]
fn navigations_use_destination_but_iframe_redirects_keep_the_top_level_site() {
    let url = ServoUrl::parse("https://third.example/one").unwrap();
    let mut navigation = request("https://a.example/", &url);
    navigation.mode = RequestMode::Navigate;
    let original = NetworkPartitionKey::from_origin(&url.origin());
    assert_eq!(navigation.network_partition_key(), original);
    let redirect = ServoUrl::parse("https://fourth.example/two").unwrap();
    navigation
        .url_list
        .push(UrlWithBlobClaim::new(redirect.clone(), None));
    assert_eq!(
        navigation.network_partition_key(),
        NetworkPartitionKey::from_origin(&redirect.origin())
    );
    navigation
        .client
        .as_mut()
        .unwrap()
        .is_nested_browsing_context = true;
    assert_eq!(
        navigation.network_partition_key(),
        request("https://a.example/", &url).network_partition_key()
    );
}

#[tokio::test]
async fn http_cache_separates_sites_and_reuses_the_same_site() {
    let url = ServoUrl::parse("https://third.example/resource").unwrap();
    let cache = HttpCache::default();
    cache
        .store(&request("https://a.example/", &url), &response(&url))
        .await;
    assert!(
        cache
            .construct_response(&request("https://sub.a.example:8443/", &url), &mut None)
            .await
            .is_some()
    );
    assert!(
        cache
            .construct_response(&request("https://b.example/", &url), &mut None)
            .await
            .is_none()
    );
    assert!(
        cache
            .construct_response(&request("http://a.example/", &url), &mut None)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn missing_and_opaque_context_cannot_read_or_populate_a_shared_cache() {
    let url = ServoUrl::parse("https://third.example/resource").unwrap();
    let cache = HttpCache::default();
    let known = request("https://a.example/", &url);
    let mut missing = known.clone();
    missing.client = None;
    cache.store(&missing, &response(&url)).await;
    assert!(
        cache
            .construct_response(&missing, &mut None)
            .await
            .is_none()
    );
    assert!(cache.construct_response(&known, &mut None).await.is_none());
    cache.store(&known, &response(&url)).await;
    assert!(
        cache
            .construct_response(&missing, &mut None)
            .await
            .is_none()
    );
    let mut opaque = known;
    opaque.client.as_mut().unwrap().top_level_origin = Some(ImmutableOrigin::new_opaque());
    cache.store(&opaque, &response(&url)).await;
    assert!(cache.construct_response(&opaque, &mut None).await.is_none());
    opaque.client.as_mut().unwrap().top_level_origin = None;
    assert!(cache.construct_response(&opaque, &mut None).await.is_none());
}

#[tokio::test]
async fn persisted_cache_keeps_partition_and_ignores_legacy_unpartitioned_rows() {
    let directory = tempfile::tempdir().unwrap();
    let url = ServoUrl::parse("https://third.example/resource").unwrap();
    let cache = HttpCache::default();
    let a = request("https://a.example/", &url);
    let b = request("https://b.example/", &url);
    cache.store(&a, &response(&url)).await;
    cache.persist(directory.path());
    let loaded = HttpCache::load(directory.path());
    assert!(loaded.construct_response(&a, &mut None).await.is_some());
    assert!(loaded.construct_response(&b, &mut None).await.is_none());
    let path = directory.path().join("http_cache.json");
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    legacy["entries"][0]
        .as_object_mut()
        .unwrap()
        .remove("partition_key");
    std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let loaded = HttpCache::load(directory.path());
    assert!(loaded.construct_response(&a, &mut None).await.is_none());
    assert!(loaded.construct_response(&b, &mut None).await.is_none());
}

#[test]
fn preflight_cache_separates_top_level_sites_for_the_same_request_origin() {
    let url = ServoUrl::parse("https://fourth.example/resource").unwrap();
    let mut a = request("https://a.example/", &url);
    a.origin = Origin::Origin(ServoUrl::parse("https://third.example/").unwrap().origin());
    a.client.as_mut().unwrap().origin = a.origin.clone();
    let mut b = a.clone();
    b.client.as_mut().unwrap().top_level_origin =
        Some(ServoUrl::parse("https://b.example/").unwrap().origin());
    let header = HeaderName::from_static("x-partition-test");
    let mut cache = CorsCache::default();
    assert!(!cache.match_method_and_update(&a, Method::PUT, Duration::from_secs(600)));
    assert!(!cache.match_header_and_update(&a, &header, Duration::from_secs(600)));
    assert!(cache.match_method(&a, Method::PUT));
    assert!(cache.match_header(&a, &header));
    assert!(!cache.match_method(&b, Method::PUT));
    assert!(!cache.match_header(&b, &header));
    b.client = None;
    assert!(!cache.match_method_and_update(&b, Method::PUT, Duration::from_secs(600)));
    assert!(!cache.match_method(&b, Method::PUT));
}

#[test]
fn related_url_invalidation_is_confined_to_the_request_partition() {
    let counter = Arc::new(AtomicUsize::new(0));
    let requests = counter.clone();
    let handler =
        move |request: HyperRequest<Incoming>,
              response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
            if request.method() == Method::POST {
                response
                    .headers_mut()
                    .insert(header::LOCATION, HeaderValue::from_static("/resource"));
            } else {
                response
                    .headers_mut()
                    .typed_insert(CacheControl::new().with_max_age(Duration::from_secs(3600)));
                *response.body_mut() = make_body(
                    requests
                        .fetch_add(1, Ordering::SeqCst)
                        .to_string()
                        .into_bytes(),
                );
            }
        };
    let (server, url) = make_server(handler);
    let url = url.join("/resource").unwrap();
    let a = request("https://a.example/", &url);
    let b = request("https://b.example/", &url);
    let mut context = new_fetch_context(None, None);
    assert!(!fetch_with_context(a.clone(), &mut context).is_network_error());
    assert!(!fetch_with_context(b.clone(), &mut context).is_network_error());
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    assert!(matches!(
        fetch_with_context(a.clone(), &mut context).cache_state,
        CacheState::Local
    ));
    let mut post = request("https://a.example/", &url.join("/invalidate").unwrap());
    post.method = Method::POST;
    assert!(!fetch_with_context(post, &mut context).is_network_error());
    assert!(matches!(
        fetch_with_context(b, &mut context).cache_state,
        CacheState::Local
    ));
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    assert!(!fetch_with_context(a, &mut context).is_network_error());
    assert_eq!(counter.load(Ordering::SeqCst), 3);
    let _ = server.close();
}

#[test]
fn preflight_cache_shares_profile_lifetime_and_clear_without_sharing_sites() {
    let url = ServoUrl::parse("https://third.example/resource").unwrap();
    let a = request("https://a.example/", &url);
    let b = request("https://b.example/", &url);
    let mut first = CorsCache::default();
    let mut next_fetch = first.clone();
    first.match_method_and_update(&a, Method::PUT, Duration::MAX);
    assert!(next_fetch.match_method(&a, Method::PUT));
    assert!(!next_fetch.match_method(&b, Method::PUT));
    next_fetch.clear();
    assert!(!first.match_method(&a, Method::PUT));
    first.match_method_and_update(&a, Method::PUT, Duration::ZERO);
    assert!(!next_fetch.match_method(&a, Method::PUT));
}

#[test]
fn preflight_cache_evicts_old_authorizations_under_many_distinct_headers() {
    let url = ServoUrl::parse("https://third.example/resource").unwrap();
    let a = request("https://a.example/", &url);
    let mut cache = CorsCache::default();
    for index in 0..4096 {
        cache.match_header_and_update(
            &a,
            &HeaderName::from_bytes(format!("x-probe-{index}").as_bytes()).unwrap(),
            Duration::from_secs(600),
        );
    }
    assert!(!cache.match_header(&a, &HeaderName::from_static("x-probe-0")));
    assert!(cache.match_header(&a, &HeaderName::from_static("x-probe-4095")));
}

#[test]
fn updating_an_old_credentialed_grant_does_not_restart_its_lifetime() {
    let url = ServoUrl::parse("https://third.example/resource").unwrap();
    let mut credentialed = request("https://a.example/", &url);
    credentialed.credentials_mode = net_traits::request::CredentialsMode::Include;
    let mut anonymous = credentialed.clone();
    anonymous.credentials_mode = net_traits::request::CredentialsMode::Omit;
    let mut cache = CorsCache::default();
    cache.match_method_and_update(&credentialed, Method::PUT, Duration::from_secs(600));
    std::thread::sleep(Duration::from_millis(100));
    assert!(cache.match_method_and_update(&anonymous, Method::PUT, Duration::from_millis(50)));
    assert!(!cache.match_method(&credentialed, Method::PUT));
    assert!(!cache.match_method_and_update(&anonymous, Method::PUT, Duration::from_secs(600)));
    assert!(!cache.match_method(&credentialed, Method::PUT));
}

#[test]
fn failed_preflight_revokes_only_the_matching_site_origin_and_url() {
    let requests = Arc::new(AtomicUsize::new(0));
    let received = requests.clone();
    let handler =
        move |request: HyperRequest<Incoming>,
              response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
            assert_eq!(request.method(), Method::OPTIONS);
            received.fetch_add(1, Ordering::SeqCst);
            *response.status_mut() = http::StatusCode::FORBIDDEN;
        };
    let (server, url) = make_server(handler);
    let mut a = request("http://a.example/", &url);
    a.origin = Origin::Origin(crate::mock_origin());
    a.client.as_mut().unwrap().origin = a.origin.clone();
    a.mode = RequestMode::CorsMode;
    a.method = Method::PUT;
    a.credentials_mode = net_traits::request::CredentialsMode::Include;
    let mut b = a.clone();
    b.client.as_mut().unwrap().top_level_origin =
        Some(ServoUrl::parse("http://b.example/").unwrap().origin());
    let mut other_url = a.clone();
    other_url
        .url_list
        .push(UrlWithBlobClaim::new(url.join("/different").unwrap(), None));
    let mut other_origin = a.clone();
    other_origin.origin = Origin::Origin(
        ServoUrl::parse("http://other-origin.example/")
            .unwrap()
            .origin(),
    );
    other_origin.client.as_mut().unwrap().origin = other_origin.origin.clone();
    let mut context = new_fetch_context(None, None);
    let mut cache = context.state.cors_cache.clone();
    let allowed = HeaderName::from_static("x-allowed");
    for request in [&a, &b, &other_url, &other_origin] {
        cache.match_method_and_update(request, Method::PUT, Duration::from_secs(600));
        cache.match_header_and_update(request, &allowed, Duration::from_secs(600));
    }
    let mut failed = a.clone();
    failed.credentials_mode = net_traits::request::CredentialsMode::Omit;
    failed.unsafe_request = true;
    failed
        .headers
        .insert("x-reject", HeaderValue::from_static("force-new-preflight"));
    assert!(fetch_with_context(failed, &mut context).is_network_error());
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(!cache.match_method(&a, Method::PUT));
    assert!(!cache.match_header(&a, &allowed));
    assert!(cache.match_method(&b, Method::PUT));
    assert!(cache.match_method(&other_url, Method::PUT));
    assert!(cache.match_method(&other_origin, Method::PUT));
    let _ = server.close();
}

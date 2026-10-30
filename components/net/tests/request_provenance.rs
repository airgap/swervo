/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use embedder_traits::{RequestSource, WebResourceRequest, WebResourceResponseMsg};
use net::embedder::NetToEmbedderMsg;
use net_traits::blob_url_store::UrlWithBlobClaim;
use net_traits::request::{Destination, Referrer, RequestBuilder, RequestMode};
use servo_base::id::TEST_WEBVIEW_ID;
use servo_url::ServoUrl;

fn intercept(source: Option<RequestSource>, nested: bool, redirect: bool) -> WebResourceRequest {
    let initial = ServoUrl::parse("https://example.com/start").unwrap();
    let current = ServoUrl::parse("gator://settings?block_ads=off").unwrap();
    let mut builder = RequestBuilder::new(
        Some(TEST_WEBVIEW_ID),
        UrlWithBlobClaim::new(initial.clone(), None),
        Referrer::NoReferrer,
    )
    .destination(Destination::Document)
    .mode(RequestMode::Navigate);
    if let Some(source) = source {
        let mut client = crate::network_partition::client(initial.origin());
        client.source = source;
        client.is_nested_browsing_context = nested;
        builder = builder.client(client);
    }
    if redirect {
        builder = builder.url_list(vec![initial, current.clone()]);
    }
    let mut request = builder.build();
    let (proxy, receiver) = crate::create_generic_embedder_proxy_and_receiver();
    let context = crate::new_fetch_context(None, Some(proxy));
    let observed = std::thread::spawn(move || match receiver.recv().unwrap() {
        NetToEmbedderMsg::WebResourceRequested(_, request, sender) => {
            sender.send(WebResourceResponseMsg::DoNotIntercept).unwrap();
            *request
        },
        _ => unreachable!(),
    });
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            context
                .request_interceptor
                .lock()
                .await
                .intercept_request(&mut request, &mut None, &context)
                .await;
        });
    let observed = observed.join().unwrap();
    assert_eq!(observed.referrer_url, None);
    if redirect {
        assert_eq!(observed.url, current.into_url());
        assert!(observed.is_redirect);
    }
    observed
}

#[test]
fn interception_preserves_document_identity_despite_no_referrer_and_redirect() {
    let url = ServoUrl::parse("https://attacker.example/path").unwrap();
    let source = RequestSource::Document {
        origin: url.origin(),
        url,
        is_top_level: true,
        has_user_activation: true,
    };
    let observed = intercept(Some(source.clone()), false, true);
    assert_eq!(observed.source, source);
    assert!(observed.is_for_main_frame);
}

#[test]
fn nested_document_navigation_is_not_a_main_frame_request() {
    let observed = intercept(Some(RequestSource::Browser), true, false);
    assert!(!observed.is_for_main_frame);
}

#[test]
fn missing_context_is_unknown_and_worker_context_never_becomes_browser() {
    assert_eq!(intercept(None, false, false).source, RequestSource::Unknown);
    assert_eq!(
        intercept(Some(RequestSource::Worker), false, false).source,
        RequestSource::Worker
    );
}

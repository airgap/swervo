/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashSet;

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use net_traits::request::{CredentialsMode, Destination, RequestBuilder, RequestId, RequestMode};
use net_traits::{FetchMetadata, NetworkError, ResourceFetchTiming};
use script_traits::DocumentActivity;
use servo_url::ServoUrl;

use crate::document_loader::DocumentLoader;
use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentReadyState;
use crate::dom::bindings::codegen::Bindings::SVGUseElementBinding::SVGUseElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::csp::{GlobalCspReporting, Violation};
use crate::dom::document::{Document, DocumentSource, HasBrowsingContext, IsHTMLDocument};
use crate::dom::element::Element;
use crate::dom::globalscope::GlobalScope;
use crate::dom::node::Node;
use crate::dom::performance::performanceresourcetiming::InitiatorType;
use crate::dom::servoparser::ServoParser;
use crate::dom::svg::svganimatedstring::SVGAnimatedString;
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;
use crate::dom::svg::svgsvgelement::SVGSVGElement;
use crate::fetch::RequestWithGlobalScope;
use crate::network_listener::{self, FetchResponseListener, ResourceTimingListener};
use crate::url::ensure_blob_referenced_by_url_is_kept_alive;

#[dom_struct]
pub(crate) struct SVGUseElement {
    svggraphicselement: SVGGraphicsElement,
    href: MutNullableDom<SVGAnimatedString>,
}

impl SVGUseElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGUseElement {
        SVGUseElement {
            svggraphicselement: SVGGraphicsElement::new_inherited(local_name, prefix, document),
            href: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGUseElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGUseElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }
}

impl SVGUseElementMethods<crate::DomTypeHolder> for SVGUseElement {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGURIReference__href>
    fn Href(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.href
            .or_init(|| SVGAnimatedString::new(cx, self.upcast::<Element>(), local_name!("href")))
    }
}

/// The state of an external resource document referenced by `<use href="file.svg#id">`.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) enum ExternalSvgDocument {
    /// Fetching; the svgs whose serialization asked for it, to invalidate on completion.
    Pending(HashSet<Dom<SVGSVGElement>>),
    Loaded(Dom<Document>),
    Failed,
}

/// Fetch `url` as an external resource document for `<use>`
/// (<https://svgwg.org/svg2-draft/linking.html#processingURL>). Same-origin only, as in
/// Chrome and Firefox: a cross-origin sprite renders nothing there.
pub(crate) fn fetch_external_svg_document(document: &Document, url: ServoUrl) {
    let global = document.global();
    let request = RequestBuilder::new(
        Some(document.webview_id()),
        ensure_blob_referenced_by_url_is_kept_alive(&global, url.clone()),
        global.get_referrer(),
    )
    .destination(Destination::None)
    .mode(RequestMode::SameOrigin)
    .credentials_mode(CredentialsMode::CredentialsSameOrigin)
    .with_global_scope(&global);
    let context = ExternalSvgDocumentFetchContext {
        document: Trusted::new(document),
        url,
        body: Vec::new(),
        ok: false,
    };
    document.fetch_background(request, context);
}

struct ExternalSvgDocumentFetchContext {
    /// The document whose `<use>` elements reference the resource.
    document: Trusted<Document>,
    url: ServoUrl,
    body: Vec<u8>,
    /// Whether the response was a success status.
    ok: bool,
}

impl ExternalSvgDocumentFetchContext {
    /// Parse the fetched bytes as an XML document, the way `DOMParser` parses `image/svg+xml`.
    fn parse(&self, cx: &mut JSContext, owner: &Document) -> DomRoot<Document> {
        let window = owner.window();
        let resource = Document::new(
            cx,
            window,
            HasBrowsingContext::No,
            Some(self.url.clone()),
            None,
            owner.origin().clone(),
            IsHTMLDocument::NonHTMLDocument,
            Some(
                "image/svg+xml"
                    .parse()
                    .expect("image/svg+xml is a MIME type"),
            ),
            None,
            DocumentActivity::Inactive,
            DocumentSource::FromParser,
            DocumentLoader::new(&owner.loader()),
            None,
            None,
            Default::default(),
            false,
            false,
            Some(owner.insecure_requests_policy()),
            owner.has_trustworthy_ancestor_or_current_origin(),
            owner.custom_element_reaction_stack(),
            owner.creation_sandboxing_flag_set(),
            owner.pipeline_id(),
            owner.image_cache(),
        );
        let source = DOMString::from(String::from_utf8_lossy(&self.body).into_owned());
        ServoParser::parse_xml_document(cx, &resource, Some(source), self.url.clone(), None);
        resource.set_ready_state(cx, DocumentReadyState::Complete);
        resource
    }
}

impl FetchResponseListener for ExternalSvgDocumentFetchContext {
    fn process_request_body(&mut self, _: RequestId) {}

    fn process_response(
        &mut self,
        _: &mut JSContext,
        _: RequestId,
        metadata: Result<FetchMetadata, NetworkError>,
    ) {
        self.ok = metadata.is_ok_and(|metadata| {
            let metadata = match metadata {
                FetchMetadata::Unfiltered(metadata) => metadata,
                FetchMetadata::Filtered { unsafe_, .. } => unsafe_,
            };
            metadata.status.in_range(200..300)
        });
    }

    fn process_response_chunk(&mut self, _: &mut JSContext, _: RequestId, chunk: Vec<u8>) {
        if self.ok {
            self.body.extend_from_slice(&chunk);
        }
    }

    fn process_response_eof(
        self,
        cx: &mut JSContext,
        _: RequestId,
        response: Result<(), NetworkError>,
        timing: ResourceFetchTiming,
    ) {
        network_listener::submit_timing(cx, &self, &response, &timing);
        let owner = self.document.root();
        let resource = (self.ok && response.is_ok()).then(|| self.parse(cx, &owner));
        owner.finish_external_svg_document(self.url, resource);
    }

    fn process_csp_violations(
        &mut self,
        cx: &mut JSContext,
        _: RequestId,
        violations: Vec<Violation>,
    ) {
        self.resource_timing_global()
            .report_csp_violations(cx, violations, None, None);
    }
}

impl ResourceTimingListener for ExternalSvgDocumentFetchContext {
    fn resource_timing_information(&self) -> (InitiatorType, ServoUrl) {
        (InitiatorType::LocalName("use".to_owned()), self.url.clone())
    }

    fn resource_timing_global(&self) -> DomRoot<GlobalScope> {
        self.document.root().global()
    }
}

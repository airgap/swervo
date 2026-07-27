/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::sync::Arc;

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name, ns};
use js::context::JSContext;
use js::rust::HandleObject;
use net_traits::image_cache::{
    Image, ImageCache, ImageCacheResult, ImageLoadListener, ImageOrMetadataAvailable,
    ImageResponse, PendingImageId,
};
use net_traits::blob_url_store::UrlWithBlobClaim;
use net_traits::request::{CredentialsMode, Destination, RequestBuilder, RequestId};
use net_traits::{
    CoreResourceThread, FetchMetadata, FetchResponseMsg, NetworkError, ResourceFetchTiming,
};
use pixels::Snapshot;
use script_bindings::cell::DomRefCell;
use servo_url::ServoUrl;
use style::attr::AttrValue;

use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::csp::{GlobalCspReporting, Violation};
use crate::dom::document::Document;
use crate::dom::element::AttributeMutation;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::element::Element;
use crate::dom::globalscope::GlobalScope;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{Node, NodeDamage, NodeTraits};
use crate::dom::performance::performanceresourcetiming::InitiatorType;
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;
use crate::dom::svg::svgsvgelement::SVGSVGElement;
use crate::fetch::{FetchCanceller, RequestWithGlobalScope};
use crate::network_listener::{self, FetchResponseListener, ResourceTimingListener};
use crate::url::ensure_blob_referenced_by_url_is_kept_alive;

/// <https://svgwg.org/svg2-draft/embedded.html#Placement>
const DEFAULT_WIDTH: u32 = 300;
const DEFAULT_HEIGHT: u32 = 150;

#[dom_struct]
pub(crate) struct SVGImageElement {
    svggraphicselement: SVGGraphicsElement,
    /// The decoded image fetched for the current `href`, if any. Consumed by the
    /// svg serializer (LYK-136): the subtree is rasterized as a standalone document,
    /// so the raster is re-embedded there as a `data:` URI at serialization time.
    #[no_trace]
    image: DomRefCell<Option<Image>>,
    /// Incremented on every (re)fetch; stale image-cache responses are dropped.
    generation_id: Cell<u32>,
}

impl SVGImageElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGImageElement {
        SVGImageElement {
            svggraphicselement: SVGGraphicsElement::new_inherited(local_name, prefix, document),
            image: DomRefCell::new(None),
            generation_id: Cell::new(0),
        }
    }

    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGImageElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGImageElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }

    /// The `href` in effect: the plain attribute wins over the deprecated
    /// `xlink:href`, per <https://svgwg.org/svg2-draft/linking.html#XLinkRefAttrs>.
    fn image_href(&self) -> Option<DOMString> {
        let element = self.upcast::<Element>();
        let href = element.get_string_attribute(&local_name!("href"));
        if !href.str().is_empty() {
            return Some(href);
        }
        element
            .get_attribute_string_value_with_namespace(&ns!(xlink), &local_name!("href"))
            .map(DOMString::from)
    }

    pub(crate) fn image_data(&self) -> Option<Image> {
        self.image.borrow().clone()
    }

    /// The decoded raster for the current href, if fetched (copy).
    pub(crate) fn get_raster_image_data(&self) -> Option<Snapshot> {
        Some(self.image_data()?.as_raster_image()?.as_snapshot())
    }

    fn generation_id(&self) -> u32 {
        self.generation_id.get()
    }

    /// <https://svgwg.org/svg2-draft/linking.html#processingURL>
    fn fetch_image_resource(&self, cx: &mut JSContext) {
        // Abort any in-flight instance of this algorithm (stale responses are
        // dropped by the generation check in the cache listener).
        self.generation_id.set(self.generation_id.get() + 1);
        *self.image.borrow_mut() = None;

        let Some(href) = self.image_href() else {
            self.upcast::<Node>().dirty(NodeDamage::Other);
            return;
        };
        let global = self.owner_global();
        let Ok(url) = self
            .owner_document()
            .encoding_parse_a_url(&href.str())
            .map(|url| ensure_blob_referenced_by_url_is_kept_alive(&global, url))
        else {
            self.queue_simple_event("error");
            return;
        };

        let window = self.owner_window();
        let image_cache = window.image_cache();
        let cache_result = image_cache.get_cached_image_status(
            url.url(),
            window.origin().immutable().clone(),
            None,
        );

        let id = match cache_result {
            ImageCacheResult::Available(ImageOrMetadataAvailable::ImageAvailable {
                image,
                url,
                ..
            }) => {
                self.process_image_response(ImageResponse::Loaded(image, url), cx);
                return;
            },
            ImageCacheResult::Available(ImageOrMetadataAvailable::MetadataAvailable(_, id)) => id,
            ImageCacheResult::ReadyForRequest(id) => {
                self.fetch_request(url, id);
                id
            },
            ImageCacheResult::FailedToLoadOrDecode => {
                self.process_image_response(ImageResponse::FailedToLoadOrDecode, cx);
                return;
            },
            ImageCacheResult::Pending(id) => id,
        };

        let trusted_node = Trusted::new(self);
        let generation = self.generation_id();
        let callback = window.register_image_cache_listener(id, move |response, cx| {
            let element = trusted_node.root();
            // Ignore any image response for a previous request that has been discarded.
            if generation != element.generation_id() {
                return;
            }
            element.process_image_response(response.response, cx);
        });
        image_cache.add_listener(ImageLoadListener::new(callback, window.pipeline_id(), id));
    }

    fn fetch_request(&self, url: UrlWithBlobClaim, id: PendingImageId) {
        let document = self.owner_document();
        let global = self.owner_global();
        let request = RequestBuilder::new(
            Some(document.webview_id()),
            url.clone(),
            global.get_referrer(),
        )
        .destination(Destination::Image)
        .credentials_mode(CredentialsMode::Include)
        .use_url_credentials(true)
        .with_global_scope(&global);

        let context = SVGImageFetchContext::new(
            self,
            url.url(),
            id,
            request.id,
            self.global().core_resource_thread(),
        );
        document.fetch_background(request, context);
    }

    fn process_image_response(&self, response: ImageResponse, _cx: &mut JSContext) {
        match response {
            ImageResponse::Loaded(image, _url) => {
                *self.image.borrow_mut() = Some(image);
                self.upcast::<Node>().dirty(NodeDamage::Other);
                // The raster is baked into the enclosing svg's serialized
                // rasterization; that snapshot predates these pixels.
                self.invalidate_enclosing_svg_serializations();
                self.queue_simple_event("load");
            },
            ImageResponse::MetadataLoaded(..) => {},
            ImageResponse::FailedToLoadOrDecode => {
                *self.image.borrow_mut() = None;
                self.queue_simple_event("error");
            },
        }
    }

    fn invalidate_enclosing_svg_serializations(&self) {
        for svg_root in self
            .upcast::<Node>()
            .inclusive_ancestors(ShadowIncluding::No)
            .filter_map(DomRoot::downcast::<SVGSVGElement>)
        {
            svg_root.invalidate_cached_serialized_subtree_and_rasterization_result();
        }
    }

    fn queue_simple_event(&self, name: &str) {
        let atom = match name {
            "load" => atom!("load"),
            _ => atom!("error"),
        };
        self.owner_global()
            .task_manager()
            .dom_manipulation_task_source()
            .queue_simple_event(self.upcast(), atom);
    }
}

impl VirtualMethods for SVGImageElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
    }

    fn attribute_mutated(
        &self,
        cx: &mut js::context::JSContext,
        attr: AttrRef<'_>,
        mutation: AttributeMutation,
    ) {
        self.super_type()
            .unwrap()
            .attribute_mutated(cx, attr, mutation);
        if attr.local_name() == &local_name!("href") &&
            matches!(attr.namespace(), &ns!() | &ns!(xlink))
        {
            match mutation {
                AttributeMutation::Set(..) => self.fetch_image_resource(cx),
                AttributeMutation::Removed => {
                    self.generation_id.set(self.generation_id.get() + 1);
                    *self.image.borrow_mut() = None;
                    self.upcast::<Node>().dirty(NodeDamage::Other);
                    self.invalidate_enclosing_svg_serializations();
                },
            }
        }
    }

    fn attribute_affects_presentational_hints(&self, attr: AttrRef<'_>) -> bool {
        match attr.local_name() {
            &local_name!("width") | &local_name!("height") => true,
            _ => self
                .super_type()
                .unwrap()
                .attribute_affects_presentational_hints(attr),
        }
    }

    fn parse_plain_attribute(&self, name: &LocalName, value: DOMString) -> AttrValue {
        match *name {
            local_name!("width") => AttrValue::from_u32(value.into(), DEFAULT_WIDTH),
            local_name!("height") => AttrValue::from_u32(value.into(), DEFAULT_HEIGHT),
            _ => self
                .super_type()
                .unwrap()
                .parse_plain_attribute(name, value),
        }
    }
}

/// Feeds fetch responses for an `<svg:image>` href into the image cache
/// (mirrors `PosterFrameFetchContext` on HTMLVideoElement).
struct SVGImageFetchContext {
    /// Reference to the script thread image cache.
    image_cache: Arc<dyn ImageCache>,
    /// The element that initiated the request.
    elem: Trusted<SVGImageElement>,
    /// The cache ID for this request.
    id: PendingImageId,
    /// True if this response is invalid and should be ignored.
    cancelled: bool,
    /// Url for the resource.
    url: ServoUrl,
    /// A [`FetchCanceller`] for this request.
    fetch_canceller: FetchCanceller,
}

impl FetchResponseListener for SVGImageFetchContext {
    fn process_request_body(&mut self, _: RequestId) {
        self.fetch_canceller.ignore()
    }

    fn process_response(
        &mut self,
        _: &mut JSContext,
        request_id: RequestId,
        metadata: Result<FetchMetadata, NetworkError>,
    ) {
        self.image_cache.notify_pending_response(
            self.id,
            FetchResponseMsg::ProcessResponse(request_id, metadata.clone()),
        );

        let metadata = metadata.ok().map(|meta| match meta {
            FetchMetadata::Unfiltered(m) => m,
            FetchMetadata::Filtered { unsafe_, .. } => unsafe_,
        });

        let status_is_ok = metadata
            .as_ref()
            .is_none_or(|m| m.status.in_range(200..300));

        if !status_is_ok {
            self.cancelled = true;
            self.fetch_canceller.abort();
        }
    }

    fn process_response_chunk(
        &mut self,
        _: &mut JSContext,
        request_id: RequestId,
        payload: Vec<u8>,
    ) {
        if self.cancelled {
            // An error was received previously, skip processing the payload.
            return;
        }

        self.image_cache.notify_pending_response(
            self.id,
            FetchResponseMsg::ProcessResponseChunk(request_id, payload.into()),
        );
    }

    fn process_response_eof(
        self,
        cx: &mut JSContext,
        request_id: RequestId,
        response: Result<(), NetworkError>,
        timing: ResourceFetchTiming,
    ) {
        self.image_cache.notify_pending_response(
            self.id,
            FetchResponseMsg::ProcessResponseEOF(request_id, response.clone(), timing.clone()),
        );
        network_listener::submit_timing(cx, &self, &response, &timing);
    }

    fn process_csp_violations(
        &mut self,
        cx: &mut js::context::JSContext,
        _request_id: RequestId,
        violations: Vec<Violation>,
    ) {
        let global = &self.resource_timing_global();
        global.report_csp_violations(cx, violations, None, None);
    }
}

impl ResourceTimingListener for SVGImageFetchContext {
    fn resource_timing_information(&self) -> (InitiatorType, ServoUrl) {
        let initiator_type = InitiatorType::LocalName(
            self.elem
                .root()
                .upcast::<Element>()
                .local_name()
                .to_string(),
        );
        (initiator_type, self.url.clone())
    }

    fn resource_timing_global(&self) -> DomRoot<GlobalScope> {
        self.elem.root().owner_document().global()
    }
}

impl SVGImageFetchContext {
    fn new(
        elem: &SVGImageElement,
        url: ServoUrl,
        id: PendingImageId,
        request_id: RequestId,
        core_resource_thread: CoreResourceThread,
    ) -> SVGImageFetchContext {
        let window = elem.owner_window();
        SVGImageFetchContext {
            image_cache: window.image_cache(),
            elem: Trusted::new(elem),
            id,
            cancelled: false,
            url,
            fetch_canceller: FetchCanceller::new(request_id, false, core_resource_thread),
        }
    }
}

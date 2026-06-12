/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Experimental, non-standard `<x-image>` element for image formats beyond the
//! browser-standard set (TIFF, OpenEXR, HDR, TGA, DDS, QOI, PNM, ...). It fetches
//! `src`, decodes the bytes via `pixels::load_extended_from_memory` (the `image`
//! crate's extended decoders), uploads the raster to WebRender, and displays it
//! as a replaced element through the same `MediaFrame` path as `<video>`/
//! `<rtsp-stream>`. `<img>` is untouched. Gated behind `dom_x_image_enabled`.

use std::cell::Cell;

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name, ns};
use js::context::JSContext;
use js::rust::HandleObject;
use layout_api::{HTMLMediaData, MediaFrame, MediaMetadata};
use net_traits::request::{Destination, RequestBuilder, RequestId};
use net_traits::{FetchMetadata, NetworkError, ResourceFetchTiming};
use paint_api::SerializableImageData;
use pixels::{CorsStatus, load_extended_from_memory};
use script_bindings::cell::DomRefCell;
use servo_url::ServoUrl;
use style::attr::{AttrValue, LengthOrPercentageOrAuto};

use crate::dom::bindings::codegen::Bindings::XImageElementBinding::XImageElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, LayoutDom};
use crate::dom::bindings::str::{DOMString, USVString};
use crate::dom::csp::{GlobalCspReporting, Violation};
use crate::dom::document::Document;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::element::{AttributeMutation, Element};
use crate::dom::globalscope::GlobalScope;
use crate::dom::html::htmlelement::HTMLElement;
use crate::dom::node::{Node, NodeDamage, NodeTraits};
use crate::dom::performance::performanceresourcetiming::InitiatorType;
use crate::dom::virtualmethods::VirtualMethods;
use crate::fetch::{FetchCanceller, RequestWithGlobalScope};
use crate::network_listener::{self, FetchResponseListener, ResourceTimingListener};
use crate::url::ensure_blob_referenced_by_url_is_kept_alive;

#[dom_struct]
pub(crate) struct XImageElement {
    htmlelement: HTMLElement,
    /// The decoded frame to present (raw raster uploaded to WebRender), if any.
    #[no_trace]
    current_frame: DomRefCell<Option<MediaFrame>>,
    /// Natural dimensions of the decoded image.
    natural_width: Cell<Option<u32>>,
    natural_height: Cell<Option<u32>>,
    /// Bumped on each new fetch so stale responses are ignored.
    generation: Cell<u32>,
}

impl XImageElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> XImageElement {
        XImageElement {
            htmlelement: HTMLElement::new_inherited(local_name, prefix, document),
            current_frame: Default::default(),
            natural_width: Cell::new(None),
            natural_height: Cell::new(None),
            generation: Cell::new(0),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<XImageElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(XImageElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }

    /// Start fetching the current `src`. Previous results are invalidated.
    fn fetch_image(&self) {
        self.generation.set(self.generation.get() + 1);
        let generation = self.generation.get();

        let src = self.Src().0;
        if src.is_empty() {
            return;
        }
        let document = self.owner_document();
        let global = self.owner_global();
        let parsed = match document.encoding_parse_a_url(&src) {
            Ok(url) => url,
            Err(_) => {
                warn!("x-image: could not parse src {src}");
                return;
            },
        };
        let url = ensure_blob_referenced_by_url_is_kept_alive(&global, parsed);

        let request = RequestBuilder::new(
            Some(document.webview_id()),
            url.clone(),
            global.get_referrer(),
        )
        .destination(Destination::Image)
        .with_global_scope(&global);

        let context = XImageFetchContext {
            elem: Trusted::new(self),
            url: url.url(),
            generation,
            data: vec![],
            cancelled: false,
            fetch_canceller: FetchCanceller::new(request.id, false, global.core_resource_thread()),
        };
        document.fetch_background(request, context);
    }

    /// Decode the fetched bytes and upload the raster to WebRender.
    fn finish_decode(&self, generation: u32, bytes: &[u8]) {
        if generation != self.generation.get() {
            return; // stale response
        }
        // Extension hint for formats without magic bytes (e.g. TGA).
        let src = self.Src().0;
        let extension = std::path::Path::new(src.as_str())
            .extension()
            .and_then(|ext| ext.to_str());
        let Some(raster) = load_extended_from_memory(bytes, extension, CorsStatus::Unsafe) else {
            warn!("x-image: could not decode image ({} bytes)", bytes.len());
            return;
        };

        let window = self.owner_window();
        let paint_api = window.paint_api();
        let Some(image_key) = paint_api.generate_image_key_blocking(window.webview_id()) else {
            warn!("x-image: could not allocate image key");
            return;
        };
        let (descriptor, shared_memory, should_cache) =
            raster.webrender_image_descriptor_and_data_for_frame(0);
        paint_api.add_image(
            image_key,
            descriptor,
            SerializableImageData::Raw(shared_memory),
            should_cache,
        );

        let width = raster.metadata.width;
        let height = raster.metadata.height;
        *self.current_frame.borrow_mut() = Some(MediaFrame {
            image_key,
            yuv: None,
            width: width as i32,
            height: height as i32,
        });
        self.natural_width.set(Some(width));
        self.natural_height.set(Some(height));
        self.upcast::<Node>().dirty(NodeDamage::Other);
    }
}

impl XImageElementMethods<crate::DomTypeHolder> for XImageElement {
    make_url_getter!(Src, "src");
    make_url_setter!(SetSrc, "src");
    make_dimension_uint_getter!(Width, "width");
    make_dimension_uint_setter!(SetWidth, "width");
    make_dimension_uint_getter!(Height, "height");
    make_dimension_uint_setter!(SetHeight, "height");

    fn NaturalWidth(&self) -> u32 {
        self.natural_width.get().unwrap_or(0)
    }
    fn NaturalHeight(&self) -> u32 {
        self.natural_height.get().unwrap_or(0)
    }
    fn Complete(&self) -> bool {
        self.current_frame.borrow().is_some()
    }
}

impl VirtualMethods for XImageElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<HTMLElement>() as &dyn VirtualMethods)
    }

    fn attribute_mutated(
        &self,
        cx: &mut JSContext,
        attr: AttrRef<'_>,
        mutation: AttributeMutation,
    ) {
        self.super_type()
            .unwrap()
            .attribute_mutated(cx, attr, mutation);
        if attr.local_name() == &local_name!("src") {
            self.fetch_image();
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
        match name {
            &local_name!("width") | &local_name!("height") => {
                AttrValue::from_dimension(value.into())
            },
            _ => self.super_type().unwrap().parse_plain_attribute(name, value),
        }
    }
}

impl LayoutDom<'_, XImageElement> {
    pub(crate) fn data(self) -> HTMLMediaData {
        let element = self.unsafe_get();
        let current_frame = element.current_frame.borrow().clone();
        let metadata = element
            .natural_width
            .get()
            .zip(element.natural_height.get())
            .map(|(width, height)| MediaMetadata { width, height });
        HTMLMediaData {
            current_frame,
            metadata,
        }
    }

    pub(crate) fn get_width(self) -> LengthOrPercentageOrAuto {
        self.upcast::<Element>()
            .get_attr_for_layout(&ns!(), &local_name!("width"))
            .map(AttrValue::as_dimension)
            .cloned()
            .unwrap_or(LengthOrPercentageOrAuto::Auto)
    }

    pub(crate) fn get_height(self) -> LengthOrPercentageOrAuto {
        self.upcast::<Element>()
            .get_attr_for_layout(&ns!(), &local_name!("height"))
            .map(AttrValue::as_dimension)
            .cloned()
            .unwrap_or(LengthOrPercentageOrAuto::Auto)
    }
}

/// Accumulates the fetched bytes and decodes them on EOF.
struct XImageFetchContext {
    elem: Trusted<XImageElement>,
    url: ServoUrl,
    generation: u32,
    data: Vec<u8>,
    cancelled: bool,
    fetch_canceller: FetchCanceller,
}

impl FetchResponseListener for XImageFetchContext {
    fn process_request_body(&mut self, _: RequestId) {}

    fn process_response(
        &mut self,
        _: &mut JSContext,
        _: RequestId,
        metadata: Result<FetchMetadata, NetworkError>,
    ) {
        let metadata = metadata.ok().map(|meta| match meta {
            FetchMetadata::Unfiltered(m) => m,
            FetchMetadata::Filtered { unsafe_, .. } => unsafe_,
        });
        let status_ok = metadata
            .as_ref()
            .is_none_or(|m| m.status.in_range(200..300));
        if !status_ok {
            self.cancelled = true;
            self.fetch_canceller.abort();
        }
    }

    fn process_response_chunk(&mut self, _: &mut JSContext, _: RequestId, chunk: Vec<u8>) {
        if !self.cancelled {
            self.data.extend_from_slice(&chunk);
        }
    }

    fn process_response_eof(
        self,
        cx: &mut JSContext,
        _: RequestId,
        response: Result<(), NetworkError>,
        timing: ResourceFetchTiming,
    ) {
        if !self.cancelled && response.is_ok() {
            self.elem.root().finish_decode(self.generation, &self.data);
        }
        network_listener::submit_timing(cx, &self, &response, &timing);
    }

    fn process_csp_violations(&mut self, _: RequestId, violations: Vec<Violation>) {
        let global = self.resource_timing_global();
        global.report_csp_violations(violations, None, None);
    }
}

impl ResourceTimingListener for XImageFetchContext {
    fn resource_timing_information(&self) -> (InitiatorType, ServoUrl) {
        (
            InitiatorType::LocalName("x-image".to_string()),
            self.url.clone(),
        )
    }

    fn resource_timing_global(&self) -> DomRoot<GlobalScope> {
        self.elem.root().owner_document().global()
    }
}

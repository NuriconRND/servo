/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Experimental, non-standard `<rtsp-stream>` element for playing web-non-standard
//! live video sources (e.g. RTSP) through GStreamer's `rtspsrc`, without touching
//! the standard `<video>`/`HTMLMediaElement` code path.
//!
//! Unlike `<video>`, this element does not implement the `HTMLMediaElement`
//! timeline semantics (duration/seek/currentTime). A live RTSP stream has no
//! finite duration and is not seekable, so it is exposed through a deliberately
//! minimal surface (`src`, `play()`, `stop()`, `videoWidth`/`videoHeight`,
//! `playing`). It is gated behind the `dom_rtsp_stream_enabled` pref.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use ipc_channel::ipc;
use ipc_channel::router::ROUTER;
use js::context::JSContext;
use js::rust::HandleObject;
use layout_api::{HTMLMediaData, MediaMetadata};
use script_bindings::cell::DomRefCell;
use servo_media::player::video::VideoFrameRenderer;
use servo_media::player::{Player, PlayerEvent, StreamType};
use servo_media::{ClientContextId, ServoMedia};
use style::attr::{AttrValue, LengthOrPercentageOrAuto};

use crate::dom::bindings::codegen::Bindings::RtspStreamElementBinding::RtspStreamElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::root::{DomRoot, LayoutDom};
use crate::dom::bindings::str::{DOMString, USVString};
use crate::dom::document::Document;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::element::{AttributeMutation, Element};
use crate::dom::html::htmlelement::HTMLElement;
use crate::dom::html::htmlmediaelement::MediaFrameRenderer;
use crate::dom::node::{Node, NodeDamage, NodeTraits};
use crate::dom::virtualmethods::VirtualMethods;

#[dom_struct]
pub(crate) struct RtspStreamElement {
    htmlelement: HTMLElement,
    /// The servo-media player driving the GStreamer `rtspsrc` pipeline, lazily
    /// created on the first `play()`.
    #[ignore_malloc_size_of = "servo_media"]
    #[no_trace]
    player: DomRefCell<Option<Arc<Mutex<dyn Player>>>>,
    /// Renderer that uploads decoded frames to WebRender. Reuses the same
    /// implementation as `<video>` but as an independent instance, so the
    /// standard media path is unaffected.
    #[conditional_malloc_size_of]
    #[no_trace]
    video_renderer: Arc<Mutex<MediaFrameRenderer>>,
    /// Natural width reported by the backend once metadata is known.
    video_width: Cell<Option<u32>>,
    /// Natural height reported by the backend once metadata is known.
    video_height: Cell<Option<u32>>,
    /// True between a successful `play()` and a `stop()`/error.
    playing: Cell<bool>,
}

impl RtspStreamElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> RtspStreamElement {
        let window = document.window();
        RtspStreamElement {
            htmlelement: HTMLElement::new_inherited(local_name, prefix, document),
            player: Default::default(),
            video_renderer: Arc::new(Mutex::new(MediaFrameRenderer::new(
                document.webview_id(),
                window.paint_api().clone(),
                window.get_player_context(),
            ))),
            video_width: Cell::new(None),
            video_height: Cell::new(None),
            playing: Cell::new(false),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<RtspStreamElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(RtspStreamElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }

    /// Create the servo-media player for the current `src` and start it.
    fn start_player(&self) {
        let uri = self.Src().0;
        if uri.is_empty() {
            warn!("rtsp-stream: play() with empty src ignored");
            return;
        }

        let window = self.owner_window();
        let (action_sender, action_receiver) = ipc::channel::<PlayerEvent>().unwrap();

        let pipeline_id = window.pipeline_id();
        let client_context_id =
            ClientContextId::build(pipeline_id.namespace_id.0, pipeline_id.index.0.get());

        let video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>> =
            Some(self.video_renderer.clone());

        let player = ServoMedia::get().create_player(
            &client_context_id,
            StreamType::NetworkUri,
            action_sender,
            video_renderer,
            None,
            Box::new(window.get_player_context()),
            Some(uri),
        );

        let player_id = player.lock().unwrap().get_id();
        *self.player.borrow_mut() = Some(player.clone());

        // Route player events back onto the script thread.
        let trusted = Trusted::new(self);
        let task_source = self
            .owner_global()
            .task_manager()
            .media_element_task_source()
            .to_sendable();
        ROUTER.add_typed_route(
            action_receiver,
            Box::new(move |message| {
                let event = message.unwrap();
                let trusted = trusted.clone();
                task_source.queue(task!(handle_rtsp_player_event: move || {
                    trusted.root().handle_player_event(event);
                }));
            }),
        );

        // Wire up the renderer (registers the GLPlayer when present and records
        // the player id, without which produced frames are dropped).
        let weak_video_renderer = Arc::downgrade(&self.video_renderer);
        let setup_task_source = self
            .owner_global()
            .task_manager()
            .media_element_task_source()
            .to_sendable();
        self.video_renderer.lock().unwrap().setup(
            player_id,
            setup_task_source,
            weak_video_renderer,
        );

        // play() lazily runs the GStreamer setup (which connects to the RTSP
        // server and checks for the rtspsrc plugin); surface failures softly.
        eprintln!("[RTSP-DIAG] element: calling player.play() for {}", self.Src().0);
        if let Err(error) = player.lock().unwrap().play() {
            eprintln!("[RTSP-DIAG] element: player.play() FAILED: {error:?}");
            warn!("rtsp-stream: could not start playback: {error:?}");
            self.stop_player();
            return;
        }
        eprintln!("[RTSP-DIAG] element: player.play() returned Ok");

        self.playing.set(true);
    }

    /// Stop and tear down the player, if any.
    fn stop_player(&self) {
        if let Some(ref player) = *self.player.borrow()
            && let Err(error) = player.lock().unwrap().stop()
        {
            warn!("rtsp-stream: could not stop player: {error:?}");
        }
        *self.player.borrow_mut() = None;
        self.video_renderer.lock().unwrap().reset();
        self.playing.set(false);
        self.video_width.set(None);
        self.video_height.set(None);
        self.upcast::<Node>().dirty(NodeDamage::Other);
    }

    fn handle_player_event(&self, event: PlayerEvent) {
        // Diagnostic (experimental element): trace every backend event so the
        // RTSP feasibility smoke can see how far the pipeline progresses.
        eprintln!("[RTSP-DIAG] element received event: {event:?}");
        match event {
            PlayerEvent::MetadataUpdated(ref metadata) => {
                let width = (metadata.width != 0).then_some(metadata.width);
                let height = (metadata.height != 0).then_some(metadata.height);
                if self.video_width.get() != width || self.video_height.get() != height {
                    self.video_width.set(width);
                    self.video_height.set(height);
                    self.upcast::<Node>().dirty(NodeDamage::Other);
                }
            },
            PlayerEvent::VideoFrameUpdated => {
                self.upcast::<Node>().dirty(NodeDamage::Other);
            },
            PlayerEvent::Error(ref error) => {
                warn!("rtsp-stream: player error: {error}");
                self.playing.set(false);
            },
            PlayerEvent::EndOfStream => {
                self.playing.set(false);
            },
            // Live network playback has no meaningful seek/position/buffering
            // semantics for this element; ignore the rest.
            _ => {},
        }
    }
}

impl RtspStreamElementMethods<crate::DomTypeHolder> for RtspStreamElement {
    // src (rtsp:// URL)
    make_url_getter!(Src, "src");
    make_url_setter!(SetSrc, "src");

    // Presentation box size.
    make_dimension_uint_getter!(Width, "width");
    make_dimension_uint_setter!(SetWidth, "width");
    make_dimension_uint_getter!(Height, "height");
    make_dimension_uint_setter!(SetHeight, "height");

    fn VideoWidth(&self) -> u32 {
        self.video_width.get().unwrap_or(0)
    }

    fn VideoHeight(&self) -> u32 {
        self.video_height.get().unwrap_or(0)
    }

    fn Playing(&self) -> bool {
        self.playing.get()
    }

    fn Play(&self) {
        if self.playing.get() {
            return;
        }
        self.start_player();
    }

    fn Stop(&self) {
        self.stop_player();
    }
}

impl VirtualMethods for RtspStreamElement {
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

        // A new source invalidates the current pipeline; tear it down so the
        // next play() picks up the new URL.
        if attr.local_name() == &local_name!("src") && self.player.borrow().is_some() {
            self.stop_player();
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
            _ => self
                .super_type()
                .unwrap()
                .parse_plain_attribute(name, value),
        }
    }
}

impl LayoutDom<'_, RtspStreamElement> {
    pub(crate) fn data(self) -> HTMLMediaData {
        let element = self.unsafe_get();

        let current_frame = element.video_renderer.lock().unwrap().current_frame();

        let metadata = element
            .video_width
            .get()
            .zip(element.video_height.get())
            .map(|(width, height)| MediaMetadata { width, height });

        HTMLMediaData {
            current_frame,
            metadata,
        }
    }

    pub(crate) fn get_width(self) -> LengthOrPercentageOrAuto {
        self.upcast::<Element>()
            .get_attr_for_layout(&html5ever::ns!(), &local_name!("width"))
            .map(AttrValue::as_dimension)
            .cloned()
            .unwrap_or(LengthOrPercentageOrAuto::Auto)
    }

    pub(crate) fn get_height(self) -> LengthOrPercentageOrAuto {
        self.upcast::<Element>()
            .get_attr_for_layout(&html5ever::ns!(), &local_name!("height"))
            .map(AttrValue::as_dimension)
            .cloned()
            .unwrap_or(LengthOrPercentageOrAuto::Auto)
    }
}

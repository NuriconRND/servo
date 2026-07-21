/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

pub mod audio_decoder;
pub mod audio_sink;
pub mod audio_stream_reader;
mod datachannel;
mod device_monitor;
pub mod media_capture;
pub mod media_stream;
mod media_stream_source;
pub mod player;
mod registry_scanner;
mod render;
mod source;
pub mod webrtc;

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};
use std::thread;
use std::vec::Vec;

use device_monitor::GStreamerDeviceMonitor;
use gstreamer::prelude::*;
use ipc_channel::ipc::IpcSender;
use log::warn;
use media_stream::GStreamerMediaStream;
use mime::Mime;
use registry_scanner::GSTREAMER_REGISTRY_SCANNER;
use servo_media::{Backend, BackendDeInit, BackendInit, MediaInstanceError, SupportsMediaType};
use servo_media_audio::context::{AudioContext, AudioContextOptions};
use servo_media_audio::decoder::AudioDecoder;
use servo_media_audio::sink::AudioSinkError;
use servo_media_audio::{AudioBackend, AudioStreamReader};
use servo_media_player::audio::AudioRenderer;
use servo_media_player::context::PlayerGLContext;
use servo_media_player::video::VideoFrameRenderer;
use servo_media_player::{Player, PlayerEvent, StreamType};
use servo_media_streams::capture::MediaTrackConstraintSet;
use servo_media_streams::device_monitor::MediaDeviceMonitor;
use servo_media_streams::registry::MediaStreamId;
use servo_media_streams::{MediaOutput, MediaSocket, MediaStreamType};
use servo_media_traits::{BackendMsg, ClientContextId, MediaInstance};
use servo_media_webrtc::{WebRtcBackend, WebRtcController, WebRtcSignaller};

static BACKEND_BASE_TIME: LazyLock<gstreamer::ClockTime> =
    LazyLock::new(|| gstreamer::SystemClock::obtain().time());

static BACKEND_THREAD: OnceLock<bool> = OnceLock::new();
const VIDEO_DECODER_POLICY_ENV: &str = "SERVO_GSTREAMER_VIDEO_DECODER_POLICY";
const SOFTWARE_VIDEO_DECODERS: &[&str] = &[
    "avdec_h264",
    "avdec_h265",
    "avdec_mpeg2video",
    "avdec_vp8",
    "avdec_vp9",
    "dav1ddec",
    "vp8dec",
    "vp9dec",
];

pub type WeakMediaInstance = Weak<Mutex<dyn MediaInstance>>;
pub type WeakMediaInstanceHashMap = HashMap<ClientContextId, Vec<(usize, WeakMediaInstance)>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VideoDecoderPolicy {
    Software,
    Auto,
}

impl VideoDecoderPolicy {
    fn from_environment() -> Self {
        match env::var(VIDEO_DECODER_POLICY_ENV) {
            Ok(value)
                if value.eq_ignore_ascii_case("auto") || value.eq_ignore_ascii_case("default") =>
            {
                Self::Auto
            },
            Ok(value)
                if value.eq_ignore_ascii_case("software")
                    || value.eq_ignore_ascii_case("avdec") =>
            {
                Self::Software
            },
            Ok(value) => {
                warn!(
                    "Ignoring invalid {VIDEO_DECODER_POLICY_ENV}={value:?}; \
                     expected software or auto"
                );
                Self::Software
            },
            Err(_) => Self::Software,
        }
    }
}

fn configure_video_decoder_policy() {
    match VideoDecoderPolicy::from_environment() {
        VideoDecoderPolicy::Auto => {
            log::info!("GStreamer video decoder policy: auto");
        },
        VideoDecoderPolicy::Software => configure_software_video_decoders(),
    }
}

fn configure_software_video_decoders() {
    let decoder_factories = gstreamer::ElementFactory::factories_with_type(
        gstreamer::ElementFactoryType::DECODER | gstreamer::ElementFactoryType::MEDIA_VIDEO,
        gstreamer::Rank::NONE,
    );

    let mut demoted = Vec::new();
    for factory in decoder_factories {
        let name = factory.name();
        let klass = factory.metadata("klass").unwrap_or_default();
        let is_hardware = klass.contains("Hardware")
            || name.starts_with("d3d11")
            || name.starts_with("nv")
            || name.starts_with("qsv")
            || name.starts_with("mf");
        if is_hardware && factory.rank() != gstreamer::Rank::NONE {
            factory.set_rank(gstreamer::Rank::NONE);
            demoted.push(name.to_string());
        }
    }

    let mut promoted = Vec::new();
    for decoder in SOFTWARE_VIDEO_DECODERS {
        let Some(factory) = gstreamer::ElementFactory::find(decoder) else {
            continue;
        };
        factory.set_rank(gstreamer::Rank::PRIMARY + 32);
        promoted.push((*decoder).to_owned());
    }

    log::info!(
        "GStreamer video decoder policy: software promoted={:?} demoted_hardware={:?}",
        promoted,
        demoted,
    );
}

pub struct GStreamerBackend {
    capture_mocking: AtomicBool,
    instances: Arc<Mutex<WeakMediaInstanceHashMap>>,
    next_instance_id: AtomicUsize,
    /// Channel to communicate media instances with its owner Backend.
    backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct ErrorLoadingPlugins<'a>(Vec<&'a str>);

impl GStreamerBackend {
    pub fn init_with_plugins<'a, T: AsRef<str>>(
        plugin_dir: PathBuf,
        plugins: &'a [T],
    ) -> Result<Box<dyn Backend>, ErrorLoadingPlugins<'a>> {
        gstreamer::init().unwrap();

        // GStreamer between 1.19.1 and 1.22.7 will not send messages like "end of stream"
        // to GstPlayer unless there is a GLib main loop running somewhere. We should remove
        // this workaround when we raise of required version of GStreamer.
        // See https://github.com/servo/media/pull/393.
        let needs_background_glib_main_loop = {
            let (major, minor, micro, _) = gstreamer::version();
            (major, minor, micro) >= (1, 19, 1) && (major, minor, micro) <= (1, 22, 7)
        };

        if needs_background_glib_main_loop {
            BACKEND_THREAD.get_or_init(|| {
                thread::spawn(|| glib::MainLoop::new(None, false).run());
                true
            });
        }

        let mut errors = vec![];
        for plugin in plugins {
            let mut path = plugin_dir.clone();
            path.push(plugin.as_ref());
            let registry = gstreamer::Registry::get();
            if gstreamer::Plugin::load_file(&path)
                .is_ok_and(|plugin| registry.add_plugin(&plugin).is_ok())
            {
                continue;
            }
            errors.push(plugin.as_ref());
        }

        if !errors.is_empty() {
            return Err(ErrorLoadingPlugins(errors));
        }
        configure_video_decoder_policy();

        type MediaInstancesVec = Vec<(usize, Weak<Mutex<dyn MediaInstance>>)>;
        let instances: HashMap<ClientContextId, MediaInstancesVec> = Default::default();
        let instances = Arc::new(Mutex::new(instances));

        let instances_ = instances.clone();
        let (backend_chan, recvr) = mpsc::channel();
        thread::Builder::new()
            .name("GStreamerBackend ShutdownThread".to_owned())
            .spawn(move || {
                match recvr.recv().unwrap() {
                    BackendMsg::Shutdown {
                        context,
                        id,
                        tx_ack,
                    } => {
                        let mut instances_ = instances_.lock().unwrap();
                        if let Some(vec) = instances_.get_mut(&context) {
                            vec.retain(|m| m.0 != id);
                            if vec.is_empty() {
                                instances_.remove(&context);
                            }
                        }
                        // tell caller we are done removing this instance
                        let _ = tx_ack.send(());
                    },
                };
            })
            .unwrap();

        Ok(Box::new(GStreamerBackend {
            capture_mocking: AtomicBool::new(false),
            instances,
            next_instance_id: AtomicUsize::new(0),
            backend_chan: Arc::new(Mutex::new(backend_chan)),
        }))
    }

    fn media_instance_action(
        &self,
        id: &ClientContextId,
        cb: &dyn Fn(&dyn MediaInstance) -> Result<(), MediaInstanceError>,
    ) {
        let mut instances = self.instances.lock().unwrap();
        match instances.get_mut(id) {
            Some(vec) => vec.retain(|(_, weak)| match weak.upgrade() {
                Some(instance) => {
                    if cb(&*(instance.lock().unwrap())).is_err() {
                        warn!("Error executing media instance action");
                    }
                    true
                },
                _ => false,
            }),
            None => {
                warn!("Trying to exec media action on an unknown client context");
            },
        }
    }
}

impl Backend for GStreamerBackend {
    #[expect(clippy::redundant_clone, reason = "False positive")]
    fn create_player(
        &self,
        context_id: &ClientContextId,
        stream_type: StreamType,
        sender: IpcSender<PlayerEvent>,
        renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
        audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
        gl_context: Box<dyn PlayerGLContext>,
        network_uri: Option<String>,
    ) -> Arc<Mutex<dyn Player>> {
        let id = self.next_instance_id.fetch_add(1, Ordering::Relaxed);
        let player = Arc::new(Mutex::new(player::GStreamerPlayer::new(
            id,
            context_id,
            self.backend_chan.clone(),
            stream_type,
            sender,
            renderer,
            audio_renderer,
            gl_context,
            network_uri,
        )));
        let mut instances = self.instances.lock().unwrap();
        let entry = instances.entry(*context_id).or_default();
        entry.push((id, Arc::downgrade(&player).clone()));
        player
    }

    #[expect(clippy::redundant_clone, reason = "False positive")]
    fn create_audio_context(
        &self,
        client_context_id: &ClientContextId,
        options: AudioContextOptions,
    ) -> Result<Arc<Mutex<AudioContext>>, AudioSinkError> {
        let id = self.next_instance_id.fetch_add(1, Ordering::Relaxed);
        let audio_context =
            AudioContext::new::<Self>(id, client_context_id, self.backend_chan.clone(), options)?;

        let audio_context = Arc::new(Mutex::new(audio_context));

        let mut instances = self.instances.lock().unwrap();
        let entry = instances.entry(*client_context_id).or_default();
        entry.push((id, Arc::downgrade(&audio_context).clone()));

        Ok(audio_context)
    }

    fn create_webrtc(&self, signaller: Box<dyn WebRtcSignaller>) -> WebRtcController {
        WebRtcController::new::<Self>(signaller)
    }

    fn create_audiostream(&self) -> MediaStreamId {
        GStreamerMediaStream::create_audio()
    }

    fn create_videostream(&self) -> MediaStreamId {
        GStreamerMediaStream::create_video()
    }

    fn create_stream_output(&self) -> Box<dyn MediaOutput> {
        Box::new(media_stream::MediaSink::default())
    }

    fn create_stream_and_socket(
        &self,
        ty: MediaStreamType,
    ) -> (Box<dyn MediaSocket>, MediaStreamId) {
        let (id, socket) = GStreamerMediaStream::create_proxy(ty);
        (Box::new(socket), id)
    }

    fn create_audioinput_stream(&self, set: MediaTrackConstraintSet) -> Option<MediaStreamId> {
        if self.capture_mocking.load(Ordering::Acquire) {
            // XXXManishearth we should caps filter this
            return Some(self.create_audiostream());
        }
        media_capture::create_audioinput_stream(set)
    }

    fn create_videoinput_stream(&self, set: MediaTrackConstraintSet) -> Option<MediaStreamId> {
        if self.capture_mocking.load(Ordering::Acquire) {
            // XXXManishearth we should caps filter this
            return Some(self.create_videostream());
        }
        media_capture::create_videoinput_stream(set)
    }

    fn can_play_type(&self, media_type: &str) -> SupportsMediaType {
        if let Ok(mime) = media_type.parse::<Mime>() {
            let mime_type = mime.type_().as_str().to_owned() + "/" + mime.subtype().as_str();
            let codecs = match mime.get_param("codecs") {
                Some(codecs) => codecs
                    .as_str()
                    .split(',')
                    .map(|codec| codec.trim())
                    .collect(),
                None => vec![],
            };

            if GSTREAMER_REGISTRY_SCANNER.is_container_type_supported(&mime_type) {
                if codecs.is_empty() {
                    return SupportsMediaType::Maybe;
                } else if GSTREAMER_REGISTRY_SCANNER.are_all_codecs_supported(&codecs) {
                    return SupportsMediaType::Probably;
                } else {
                    return SupportsMediaType::No;
                }
            }
        }
        SupportsMediaType::No
    }

    fn set_capture_mocking(&self, mock: bool) {
        self.capture_mocking.store(mock, Ordering::Release)
    }

    fn mute(&self, id: &ClientContextId, val: bool) {
        self.media_instance_action(
            id,
            &(move |instance: &dyn MediaInstance| instance.mute(val)),
        );
    }

    fn suspend(&self, id: &ClientContextId) {
        self.media_instance_action(id, &|instance: &dyn MediaInstance| instance.suspend());
    }

    fn resume(&self, id: &ClientContextId) {
        self.media_instance_action(id, &|instance: &dyn MediaInstance| instance.resume());
    }

    fn get_device_monitor(&self) -> Box<dyn MediaDeviceMonitor> {
        Box::new(GStreamerDeviceMonitor::new())
    }
}

impl AudioBackend for GStreamerBackend {
    type Sink = audio_sink::GStreamerAudioSink;
    fn make_decoder() -> Box<dyn AudioDecoder> {
        Box::new(audio_decoder::GStreamerAudioDecoder::new())
    }
    fn make_sink() -> Result<Self::Sink, AudioSinkError> {
        audio_sink::GStreamerAudioSink::new()
    }

    fn make_streamreader(
        id: MediaStreamId,
        sample_rate: f32,
    ) -> Result<Box<dyn AudioStreamReader + Send>, AudioSinkError> {
        let reader = audio_stream_reader::GStreamerAudioStreamReader::new(id, sample_rate)
            .map_err(AudioSinkError::Backend)?;
        Ok(Box::new(reader))
    }
}

impl WebRtcBackend for GStreamerBackend {
    type Controller = webrtc::GStreamerWebRtcController;

    fn construct_webrtc_controller(
        signaller: Box<dyn WebRtcSignaller>,
        thread: WebRtcController,
    ) -> Self::Controller {
        webrtc::construct(signaller, thread).expect("WebRTC creation failed")
    }
}

impl BackendInit for GStreamerBackend {
    fn init() -> Box<dyn Backend> {
        Self::init_with_plugins::<&str>(PathBuf::new(), &[]).unwrap()
    }
}

impl BackendDeInit for GStreamerBackend {
    fn deinit(&self) {
        let to_shutdown: Vec<(ClientContextId, usize)> = {
            let map = self.instances.lock().unwrap();
            map.iter()
                .flat_map(|(ctx, v)| v.iter().map(move |(id, _)| (*ctx, *id)))
                .collect()
        };

        for (ctx, id) in to_shutdown {
            let (tx_ack, rx_ack) = mpsc::channel();
            let _ = self
                .backend_chan
                .lock()
                .unwrap()
                .send(BackendMsg::Shutdown {
                    context: ctx,
                    id,
                    tx_ack,
                });
            let _ = rx_ack.recv();
        }
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::env;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, Once};
use std::time::{self, Instant};

use byte_slice_cast::AsSliceOf;
use glib;
use glib::prelude::*;
use gstreamer;
use gstreamer_app;
use gstreamer_play;
use gstreamer_play::prelude::*;
use ipc_channel::ipc::{IpcReceiver, IpcSender, channel};
use servo_media::MediaInstanceError;
use servo_media_player::audio::AudioRenderer;
use servo_media_player::context::PlayerGLContext;
use servo_media_player::metadata::Metadata;
use servo_media_player::video::VideoFrameRenderer;
use servo_media_player::{
    PlaybackState, Player, PlayerError, PlayerEvent, SeekLock, SeekLockMsg, StreamType,
};
use servo_media_streams::registry::{MediaStreamId, get_stream};
use servo_media_traits::{BackendMsg, ClientContextId, MediaInstance};

use super::BACKEND_BASE_TIME;
use crate::media_stream::GStreamerMediaStream;
use crate::media_stream_source::{ServoMediaStreamSrc, register_servo_media_stream_src};
use crate::render::GStreamerRender;
use crate::source::{ServoSrc, register_servo_src};

const DEFAULT_MUTED: bool = false;
const DEFAULT_PAUSED: bool = true;
const DEFAULT_CAN_RESUME: bool = false;
const DEFAULT_PLAYBACK_RATE: f64 = 1.0;
const DEFAULT_VOLUME: f64 = 1.0;
const DEFAULT_TIME_RANGES: Vec<Range<f64>> = vec![];

const MAX_BUFFER_SIZE: i32 = 500 * 1024 * 1024;
const DISABLE_AUDIO_ENV: &str = "SERVO_GSTREAMER_DISABLE_AUDIO";
const DISABLE_ENOUGHDATA_BACKOFF_ENV: &str = "SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF";
const VIDEO_SAMPLE_INFO_INTERVAL: u64 = 120;
const VIDEO_SAMPLE_LATE_GAP_MS: f64 = 20.0;

#[derive(Default)]
struct VideoSampleDiagnostics {
    sample_count: u64,
    summary_frame_count: u64,
    summary_started_at: Option<Instant>,
    last_sample_at: Option<Instant>,
    last_pts: Option<gstreamer::ClockTime>,
    late_pts_gaps_since_summary: u64,
    late_wall_gaps_since_summary: u64,
}

fn clock_time_ms(clock_time: Option<gstreamer::ClockTime>) -> Option<f64> {
    clock_time.map(|clock_time| clock_time.nseconds() as f64 / 1_000_000.0)
}

fn clock_delta_ms(
    previous: Option<gstreamer::ClockTime>,
    current: Option<gstreamer::ClockTime>,
) -> Option<f64> {
    match (previous, current) {
        (Some(previous), Some(current)) => {
            Some((current.nseconds() as i128 - previous.nseconds() as i128) as f64 / 1_000_000.0)
        },
        _ => None,
    }
}

fn format_optional_ms(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:.3}"),
        None => "None".to_owned(),
    }
}

fn should_log_pipeline_element(factory_name: &str, klass: &str) -> bool {
    let factory_name = factory_name.to_ascii_lowercase();
    klass.contains("Decoder")
        || klass.contains("Converter")
        || klass.contains("Sink")
        || factory_name.contains("dec")
        || factory_name.contains("convert")
        || factory_name.contains("scale")
        || factory_name.contains("balance")
        || factory_name.contains("deinterlace")
        || factory_name.contains("appsink")
}

fn log_pipeline_element_added(element: &gstreamer::Element) {
    let Some(factory) = element.factory() else {
        return;
    };
    let factory_name = factory.name();
    let klass = factory.metadata("klass").unwrap_or_default();

    if !should_log_pipeline_element(&factory_name, &klass) {
        return;
    }

    log::info!(
        "GStreamer pipeline element added: element={} factory={} klass={} rank={}",
        element.name(),
        factory_name,
        klass,
        factory.rank(),
    );
}

/// (video-wall) When `SERVO_VIDEO_DEC_MAX_THREADS=N` is set, force `max-threads=N`
/// on any auto-plugged video decoder that exposes the property (libav `avdec_*`).
/// With many parallel software decoders (e.g. a 36-video wall), libav's default
/// auto-threading spawns many threads per decoder and oversubscribes the cores;
/// N=1 gives one decode thread per stream. Unset/invalid = GStreamer default.
/// Hardware decoders (nvh264dec/d3d11*) lack the property and are left untouched.
fn video_decoder_max_threads() -> Option<i32> {
    static N: std::sync::OnceLock<Option<i32>> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        env::var("SERVO_VIDEO_DEC_MAX_THREADS")
            .ok()?
            .trim()
            .parse()
            .ok()
    })
}

fn maybe_set_decoder_max_threads(element: &gstreamer::Element) {
    let Some(n) = video_decoder_max_threads() else {
        return;
    };
    if element.find_property("max-threads").is_none() {
        return;
    }
    element.set_property("max-threads", n);
    log::info!(
        "GStreamer decoder max-threads set: element={} max_threads={}",
        element.name(),
        n,
    );
}

fn configure_playbin_flags(
    pipeline: &gstreamer::Element,
    prefer_native_video: bool,
) -> Result<(), PlayerError> {
    let flags = pipeline.property_value("flags");
    let flags_class = glib::FlagsClass::with_type(flags.type_())
        .ok_or_else(|| PlayerError::Backend("FlagsClass creation failed".to_owned()))?;
    let mut flags_builder = flags_class
        .builder_with_value(flags)
        .ok_or_else(|| PlayerError::Backend("FlagsClass creation failed".to_owned()))?;

    if !cfg!(any(target_os = "windows", target_os = "android")) {
        flags_builder = flags_builder.set_by_nick("download");
    }

    if prefer_native_video {
        flags_builder = flags_builder
            .set_by_nick("native-video")
            .unset_by_nick("deinterlace")
            .unset_by_nick("soft-colorbalance")
            .unset_by_nick("text");
    }

    let disable_audio = env_flag_enabled(DISABLE_AUDIO_ENV);
    if disable_audio {
        flags_builder = flags_builder
            .unset_by_nick("audio")
            .unset_by_nick("soft-volume");
    }

    let flags = flags_builder
        .build()
        .ok_or_else(|| PlayerError::Backend("FlagsClass creation failed".to_owned()))?;
    pipeline.set_property_from_value("flags", &flags);

    log::info!(
        "GStreamer playbin flags configured: prefer_native_video={} \
         download={} disabled_video_filters={} disable_audio={}",
        prefer_native_video,
        !cfg!(any(target_os = "windows", target_os = "android")),
        prefer_native_video,
        disable_audio,
    );
    Ok(())
}

fn env_flag_enabled(name: &str) -> bool {
    env::var(name).is_ok_and(|value| {
        value == "1"
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
            || value.eq_ignore_ascii_case("on")
    })
}

fn create_disabled_audio_sink() -> Result<gstreamer::Element, PlayerError> {
    let audio_sink = gstreamer::ElementFactory::make("fakesink")
        .build()
        .map_err(|error| PlayerError::Backend(format!("fakesink creation failed: {error:?}")))?;
    audio_sink.set_property("sync", false);
    audio_sink.set_property("async", false);
    Ok(audio_sink)
}

fn disable_pipeline_audio_sink(
    pipeline: &gstreamer::Element,
    reason: &str,
) -> Result<(), PlayerError> {
    let audio_sink = create_disabled_audio_sink()?;
    pipeline.set_property("audio-sink", &audio_sink);
    log::info!(
        "GStreamer audio sink disabled: reason={} sink=fakesink sync=false async=false",
        reason,
    );
    Ok(())
}

fn restore_pipeline_audio_sink(
    pipeline: &gstreamer::Element,
    reason: &str,
) -> Result<(), PlayerError> {
    let audio_sink = gstreamer::ElementFactory::make("autoaudiosink")
        .build()
        .map_err(|error| {
            PlayerError::Backend(format!("autoaudiosink creation failed: {error:?}"))
        })?;
    pipeline.set_property("audio-sink", &audio_sink);
    log::info!(
        "GStreamer audio sink restored: reason={} sink=autoaudiosink",
        reason,
    );
    Ok(())
}

impl VideoSampleDiagnostics {
    fn note_sample(&mut self, sample: &gstreamer::Sample) {
        let now = Instant::now();
        self.sample_count = self.sample_count.saturating_add(1);
        self.summary_frame_count = self.summary_frame_count.saturating_add(1);
        if self.summary_started_at.is_none() {
            self.summary_started_at = Some(now);
        }

        let buffer = sample.buffer();
        let pts = buffer.and_then(|buffer| buffer.pts());
        let duration = buffer.and_then(|buffer| buffer.duration());
        let pts_ms = clock_time_ms(pts);
        let duration_ms = clock_time_ms(duration);
        let pts_delta_ms = clock_delta_ms(self.last_pts, pts);
        let wall_delta_ms = self.last_sample_at.map(|last_sample_at| {
            now.saturating_duration_since(last_sample_at).as_secs_f64() * 1000.0
        });
        let pts_gap_ms = match (pts_delta_ms, duration_ms) {
            (Some(pts_delta_ms), Some(duration_ms)) => Some(pts_delta_ms - duration_ms),
            _ => None,
        };
        let late_pts_gap = pts_delta_ms.is_some_and(|delta| delta > VIDEO_SAMPLE_LATE_GAP_MS);
        let late_wall_gap = wall_delta_ms.is_some_and(|delta| delta > VIDEO_SAMPLE_LATE_GAP_MS);

        if late_pts_gap {
            self.late_pts_gaps_since_summary = self.late_pts_gaps_since_summary.saturating_add(1);
        }
        if late_wall_gap {
            self.late_wall_gaps_since_summary = self.late_wall_gaps_since_summary.saturating_add(1);
        }

        log::debug!(
            "GStreamer video sample: sample_id={} pts_ms={} duration_ms={} \
             pts_delta_ms={} wall_delta_ms={} pts_gap_ms={} late_pts_gap={} \
             late_wall_gap={}",
            self.sample_count,
            format_optional_ms(pts_ms),
            format_optional_ms(duration_ms),
            format_optional_ms(pts_delta_ms),
            format_optional_ms(wall_delta_ms),
            format_optional_ms(pts_gap_ms),
            late_pts_gap,
            late_wall_gap,
        );

        if self.summary_frame_count >= VIDEO_SAMPLE_INFO_INTERVAL || late_pts_gap || late_wall_gap {
            let summary_wall_elapsed_ms = self
                .summary_started_at
                .map(|summary_started_at| {
                    now.saturating_duration_since(summary_started_at)
                        .as_secs_f64()
                        * 1000.0
                })
                .unwrap_or_default();
            log::info!(
                "GStreamer video sample summary: sample_id={} frames_since_summary={} \
                 summary_wall_elapsed_ms={:.3} pts_ms={} duration_ms={} \
                 pts_delta_ms={} wall_delta_ms={} pts_gap_ms={} late_pts_gaps={} \
                 late_wall_gaps={}",
                self.sample_count,
                self.summary_frame_count,
                summary_wall_elapsed_ms,
                format_optional_ms(pts_ms),
                format_optional_ms(duration_ms),
                format_optional_ms(pts_delta_ms),
                format_optional_ms(wall_delta_ms),
                format_optional_ms(pts_gap_ms),
                self.late_pts_gaps_since_summary,
                self.late_wall_gaps_since_summary,
            );
            self.summary_frame_count = 0;
            self.summary_started_at = Some(now);
            self.late_pts_gaps_since_summary = 0;
            self.late_wall_gaps_since_summary = 0;
        }

        self.last_sample_at = Some(now);
        self.last_pts = pts;
    }
}

fn metadata_from_media_info(media_info: &gstreamer_play::PlayMediaInfo) -> Result<Metadata, ()> {
    let dur = media_info.duration();
    let duration = if let Some(dur) = dur {
        let mut nanos = dur.nseconds();
        nanos %= 1_000_000_000;
        let seconds = dur.seconds();
        Some(time::Duration::new(seconds, nanos as u32))
    } else {
        None
    };

    let mut audio_tracks = Vec::new();
    let mut video_tracks = Vec::new();

    let format = media_info
        .container_format()
        .unwrap_or_else(|| glib::GString::from(""))
        .to_string();

    for stream_info in media_info.stream_list() {
        let stream_type = stream_info.stream_type();
        match stream_type.as_str() {
            "audio" => {
                let codec = stream_info
                    .codec()
                    .unwrap_or_else(|| glib::GString::from(""))
                    .to_string();
                audio_tracks.push(codec);
            },
            "video" => {
                let codec = stream_info
                    .codec()
                    .unwrap_or_else(|| glib::GString::from(""))
                    .to_string();
                video_tracks.push(codec);
            },
            _ => {},
        }
    }

    let mut width: u32 = 0;
    let height: u32 = if media_info.number_of_video_streams() > 0 {
        let first_video_stream = &media_info.video_streams()[0];
        width = first_video_stream.width() as u32;
        first_video_stream.height() as u32
    } else {
        0
    };

    let is_seekable = media_info.is_seekable();
    let is_live = media_info.is_live();
    let title = media_info.title().map(|s| s.as_str().to_string());

    Ok(Metadata {
        duration,
        width,
        height,
        format,
        is_seekable,
        audio_tracks,
        video_tracks,
        is_live,
        title,
    })
}

pub struct GStreamerAudioChunk(gstreamer::buffer::MappedBuffer<gstreamer::buffer::Readable>);
impl AsRef<[f32]> for GStreamerAudioChunk {
    fn as_ref(&self) -> &[f32] {
        self.0.as_ref().as_slice_of::<f32>().unwrap_or_default()
    }
}

#[derive(PartialEq)]
enum PlayerSource {
    Seekable(ServoSrc),
    Stream(ServoMediaStreamSrc),
}

struct PlayerInner {
    player: gstreamer_play::Play,
    _signal_adapter: gstreamer_play::PlaySignalAdapter,
    source: Option<PlayerSource>,
    video_sink: gstreamer_app::AppSink,
    input_size: u64,
    seekable: bool,
    play_state: gstreamer_play::PlayState,
    paused: Cell<bool>,
    can_resume: Cell<bool>,
    playback_rate: Cell<f64>,
    muted: Cell<bool>,
    custom_audio_renderer: bool,
    volume: Cell<f64>,
    stream_type: StreamType,
    last_metadata: Option<Metadata>,
    cat: gstreamer::DebugCategory,
    enough_data: Arc<AtomicBool>,
}

impl PlayerInner {
    pub fn set_input_size(&mut self, size: u64) -> Result<(), PlayerError> {
        // Set input_size to proxy its value, since it
        // could be set by the user before calling .setup().
        self.input_size = size;
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            source.set_size(if size > 0 {
                size as i64
            } else {
                -1 // live source
            });
        }
        Ok(())
    }

    pub fn set_seekable(&mut self, seekable: bool) -> Result<(), PlayerError> {
        self.seekable = seekable;
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            source.set_seekable(seekable);
        }
        Ok(())
    }

    pub fn set_mute(&mut self, muted: bool) -> Result<(), PlayerError> {
        if self.muted.get() == muted {
            return Ok(());
        }

        self.muted.set(muted);
        self.player.set_mute(muted);
        let env_audio_disabled = env_flag_enabled(DISABLE_AUDIO_ENV);
        let audio_track_enabled = !muted && !env_audio_disabled;
        self.player.set_audio_track_enabled(audio_track_enabled);
        if !self.custom_audio_renderer {
            if muted || env_audio_disabled {
                disable_pipeline_audio_sink(&self.player.pipeline(), "muted")?;
            } else {
                restore_pipeline_audio_sink(&self.player.pipeline(), "unmuted")?;
            }
        }
        log::info!(
            "GStreamer mute state updated: muted={} audio_track_enabled={}",
            muted,
            audio_track_enabled,
        );
        Ok(())
    }

    pub fn muted(&self) -> bool {
        self.muted.get()
    }

    pub fn set_playback_rate(&mut self, playback_rate: f64) -> Result<(), PlayerError> {
        if self.stream_type != StreamType::Seekable {
            return Err(PlayerError::NonSeekableStream);
        }

        if self.playback_rate.get() == playback_rate {
            return Ok(());
        }

        self.playback_rate.set(playback_rate);

        // The new playback rate will not be passed to the pipeline if the
        // current GstPlay state is less than GST_STATE_PAUSED, which will be
        // set immediately before the initial gstreamer_play_MESSAGE_MEDIA_INFO_UPDATED
        // message is posted to bus.
        if self.last_metadata.is_some() {
            self.player.set_rate(playback_rate);
        }
        Ok(())
    }

    pub fn playback_rate(&self) -> f64 {
        self.playback_rate.get()
    }

    pub fn play(&mut self) -> Result<(), PlayerError> {
        if !self.paused.get() {
            return Ok(());
        }

        self.paused.set(false);
        self.can_resume.set(false);
        self.player.play();
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), PlayerError> {
        self.player.stop();
        self.paused.set(true);
        self.can_resume.set(false);
        self.last_metadata = None;
        self.source = None;
        Ok(())
    }

    pub fn pause(&mut self) -> Result<(), PlayerError> {
        if self.paused.get() {
            return Ok(());
        }

        self.paused.set(true);
        self.can_resume.set(true);
        self.player.pause();
        Ok(())
    }

    pub fn paused(&self) -> bool {
        self.paused.get()
    }

    pub fn can_resume(&self) -> bool {
        self.can_resume.get()
    }

    pub fn end_of_stream(&mut self) -> Result<(), PlayerError> {
        match self.source {
            Some(ref mut source) => {
                if let PlayerSource::Seekable(source) = source {
                    source
                        .push_end_of_stream()
                        .map(|_| ())
                        .map_err(|_| PlayerError::EOSFailed)
                } else {
                    Ok(())
                }
            },
            _ => Ok(()),
        }
    }

    pub fn seek(&mut self, time: f64) -> Result<(), PlayerError> {
        if self.stream_type != StreamType::Seekable {
            return Err(PlayerError::NonSeekableStream);
        }
        if let Some(ref metadata) = self.last_metadata
            && let Some(ref duration) = metadata.duration
            && duration < &time::Duration::new(time as u64, 0)
        {
            gstreamer::warning!(self.cat, obj = &self.player, "Trying to seek out of range");
            return Err(PlayerError::SeekOutOfRange);
        }

        let time = time * 1_000_000_000.;
        self.player
            .seek(gstreamer::ClockTime::from_nseconds(time as u64));
        Ok(())
    }

    pub fn set_volume(&mut self, volume: f64) -> Result<(), PlayerError> {
        if self.volume.get() == volume {
            return Ok(());
        }

        self.volume.set(volume);
        self.player.set_volume(volume);
        Ok(())
    }

    pub fn volume(&self) -> f64 {
        self.volume.get()
    }

    pub fn push_data(&mut self, data: Vec<u8>) -> Result<(), PlayerError> {
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            if self.enough_data.load(Ordering::Relaxed)
                && !env_flag_enabled(DISABLE_ENOUGHDATA_BACKOFF_ENV)
            {
                return Err(PlayerError::EnoughData);
            }
            return source
                .push_buffer(data)
                .map(|_| ())
                .map_err(|_| PlayerError::BufferPushFailed);
        }
        Err(PlayerError::BufferPushFailed)
    }

    pub fn set_src(&mut self, source: PlayerSource) {
        self.source = Some(source);
    }

    pub fn buffered(&self) -> Vec<Range<f64>> {
        let mut buffered_ranges = vec![];

        let Some(duration) = self
            .last_metadata
            .as_ref()
            .and_then(|metadata| metadata.duration)
        else {
            return buffered_ranges;
        };

        let pipeline = self.player.pipeline();
        let mut buffering = gstreamer::query::Buffering::new(gstreamer::Format::Percent);
        if pipeline.query(&mut buffering) {
            let ranges = buffering.ranges();
            for (start, end) in ranges {
                let start = (if let gstreamer::GenericFormattedValue::Percent(start) = start {
                    start.unwrap()
                } else {
                    gstreamer::format::Percent::from_percent(0)
                } / gstreamer::format::Percent::MAX) as f64
                    * duration.as_secs_f64();
                let end = (if let gstreamer::GenericFormattedValue::Percent(end) = end {
                    end.unwrap()
                } else {
                    gstreamer::format::Percent::from_percent(0)
                } / gstreamer::format::Percent::MAX) as f64
                    * duration.as_secs_f64();
                buffered_ranges.push(Range { start, end });
            }
        }

        buffered_ranges
    }

    pub fn seekable(&self) -> Vec<Range<f64>> {
        // if the servosrc is seekable, we should return the duration of the media
        if let Some(metadata) = self.last_metadata.as_ref()
            && metadata.is_seekable
            && let Some(duration) = metadata.duration
        {
            return vec![Range {
                start: 0.0,
                end: duration.as_secs_f64(),
            }];
        }

        // if the servosrc is not seekable, we should return the buffered range
        self.buffered()
    }

    fn set_stream(&mut self, stream: &MediaStreamId, only_stream: bool) -> Result<(), PlayerError> {
        debug_assert!(self.stream_type == StreamType::Stream);
        let Some(PlayerSource::Stream(ref source)) = self.source else {
            return Err(PlayerError::SetStreamFailed);
        };

        let stream = get_stream(stream).expect("Media streams registry does not contain such ID");
        let mut stream = stream.lock().unwrap();
        let Some(stream) = stream.as_mut_any().downcast_mut::<GStreamerMediaStream>() else {
            return Err(PlayerError::SetStreamFailed);
        };

        let playbin = self
            .player
            .pipeline()
            .dynamic_cast::<gstreamer::Pipeline>()
            .unwrap();
        let clock = gstreamer::SystemClock::obtain();
        playbin.set_base_time(*BACKEND_BASE_TIME);
        playbin.set_start_time(gstreamer::ClockTime::NONE);
        playbin.use_clock(Some(&clock));
        source
            .set_stream(stream, only_stream)
            .map_err(|_| PlayerError::SetStreamFailed)
    }

    fn set_audio_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        self.player
            .set_audio_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        self.player.set_audio_track_enabled(enabled);
        Ok(())
    }

    fn set_video_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        self.player
            .set_video_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        self.player.set_video_track_enabled(enabled);
        Ok(())
    }
}

macro_rules! notify(
    ($observer:expr_2021, $event:expr_2021) => {
        $observer.lock().unwrap().send($event)
    };
);

struct SeekChannel {
    sender: SeekLock,
    recv: IpcReceiver<SeekLockMsg>,
}

impl SeekChannel {
    fn new() -> Self {
        let (sender, recv) = channel::<SeekLockMsg>().expect("Couldn't create IPC channel");
        Self {
            sender: SeekLock {
                lock_channel: sender,
            },
            recv,
        }
    }

    fn sender(&self) -> SeekLock {
        self.sender.clone()
    }

    fn _await(&self) -> SeekLockMsg {
        self.recv.recv().unwrap()
    }
}

pub struct GStreamerPlayer {
    /// The player unique ID.
    id: usize,
    /// The ID of the client context this player belongs to.
    context_id: ClientContextId,
    /// Channel to communicate with the owner GStreamerBackend instance.
    backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
    inner: RefCell<Option<Arc<Mutex<PlayerInner>>>>,
    observer: Arc<Mutex<IpcSender<PlayerEvent>>>,
    audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
    video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
    /// Indicates whether the setup was succesfully performed and
    /// we are ready to consume a/v data.
    is_ready: Arc<Once>,
    /// Indicates whether the type of media stream to be played is a live stream.
    stream_type: StreamType,
    /// Network URI to play directly when `stream_type` is
    /// [`StreamType::NetworkUri`] (e.g. an `rtsp://` URL). The backend lets
    /// playbin3 auto-plug the source element (`rtspsrc`) for this URI instead of
    /// registering an AppSrc.
    network_uri: Option<String>,
    /// Decorator used to setup the video sink and process the produced frames.
    render: Arc<Mutex<GStreamerRender>>,
}

impl GStreamerPlayer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: usize,
        context_id: &ClientContextId,
        backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
        stream_type: StreamType,
        observer: IpcSender<PlayerEvent>,
        video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
        audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
        gl_context: Box<dyn PlayerGLContext>,
        network_uri: Option<String>,
    ) -> GStreamerPlayer {
        let _ = gstreamer::DebugCategory::new(
            "servoplayer",
            gstreamer::DebugColorFlags::empty(),
            Some("Servo player"),
        );

        Self {
            id,
            context_id: *context_id,
            backend_chan,
            inner: RefCell::new(None),
            observer: Arc::new(Mutex::new(observer)),
            audio_renderer,
            video_renderer,
            is_ready: Arc::new(Once::new()),
            stream_type,
            network_uri,
            render: Arc::new(Mutex::new(GStreamerRender::new(gl_context))),
        }
    }

    fn setup(&self) -> Result<(), PlayerError> {
        if self.inner.borrow().is_some() {
            return Ok(());
        }

        // Check that we actually have the elements that we
        // need to make this work.
        for element in ["playbin3", "decodebin3", "queue"] {
            if gstreamer::ElementFactory::find(element).is_none() {
                return Err(PlayerError::Backend(format!(
                    "Missing dependency: {}",
                    element
                )));
            }
        }

        // NetworkUri playback relies on playbin3 auto-plugging a network source
        // element. For rtsp:// URIs this is `rtspsrc` (which itself depends on the
        // `rtpmanager` plugin). Fail fast with a clear error if the RTSP plugins
        // are not available in this GStreamer install.
        if self.stream_type == StreamType::NetworkUri
            && self
                .network_uri
                .as_deref()
                .is_some_and(|uri| uri.starts_with("rtsp"))
        {
            for element in ["rtspsrc", "rtpbin"] {
                if gstreamer::ElementFactory::find(element).is_none() {
                    return Err(PlayerError::Backend(format!(
                        "Missing dependency: {}",
                        element
                    )));
                }
            }
        }

        let player = gstreamer_play::Play::default();
        let signal_adapter = gstreamer_play::PlaySignalAdapter::new_sync_emit(&player);
        let pipeline = player.pipeline();
        pipeline.connect("deep-element-added", false, move |args| {
            if let Ok(element) = args[2].get::<gstreamer::Element>() {
                log_pipeline_element_added(&element);
                maybe_set_decoder_max_threads(&element);
            }
            None
        });

        let prefer_native_video = !self.render.lock().unwrap().is_gl()
            && !servo_config::opts::get().multiprocess
            && !servo_config::opts::get().force_ipc;
        configure_playbin_flags(&pipeline, prefer_native_video)?;

        // Set max size for the player buffer.
        pipeline.set_property("buffer-size", MAX_BUFFER_SIZE);

        // Set player position interval update to 0.5 seconds.
        let mut config = player.config();
        config.set_position_update_interval(500u32);
        player
            .set_config(config)
            .map_err(|e| PlayerError::Backend(e.to_string()))?;

        if let Some(ref audio_renderer) = self.audio_renderer {
            let audio_sink =
                gstreamer::ElementFactory::make("appsink")
                    .build()
                    .map_err(|error| {
                        PlayerError::Backend(format!("appsink creation failed: {error:?}"))
                    })?;

            pipeline.set_property("audio-sink", &audio_sink);

            let audio_sink = audio_sink.dynamic_cast::<gstreamer_app::AppSink>().unwrap();

            let weak_audio_renderer = Arc::downgrade(audio_renderer);

            audio_sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_preroll(|_| Ok(gstreamer::FlowSuccess::Ok))
                    .new_sample(move |audio_sink| {
                        let sample = audio_sink
                            .pull_sample()
                            .map_err(|_| gstreamer::FlowError::Eos)?;
                        let buffer = sample.buffer_owned().ok_or(gstreamer::FlowError::Error)?;
                        let audio_info = sample
                            .caps()
                            .and_then(|caps| gstreamer_audio::AudioInfo::from_caps(caps).ok())
                            .ok_or(gstreamer::FlowError::Error)?;
                        let positions =
                            audio_info.positions().ok_or(gstreamer::FlowError::Error)?;

                        let Some(audio_renderer) = weak_audio_renderer.upgrade() else {
                            return Err(gstreamer::FlowError::Flushing);
                        };

                        for position in positions.iter() {
                            let buffer = buffer.clone();
                            let map = match buffer.into_mapped_buffer_readable() {
                                Ok(map) => map,
                                _ => {
                                    return Err(gstreamer::FlowError::Error);
                                },
                            };
                            let chunk = Box::new(GStreamerAudioChunk(map));
                            let channel = position.to_mask() as u32;

                            audio_renderer.lock().unwrap().render(chunk, channel);
                        }
                        Ok(gstreamer::FlowSuccess::Ok)
                    })
                    .build(),
            );
        } else if env_flag_enabled(DISABLE_AUDIO_ENV) {
            disable_pipeline_audio_sink(&pipeline, DISABLE_AUDIO_ENV)?;
        }

        let video_sink = self.render.lock().unwrap().setup_video_sink(&pipeline)?;

        // There's a known bug in gstreamer that may cause a wrong transition
        // to the ready state while setting the uri property:
        // https://cgit.freedesktop.org/gstreamer/gst-plugins-bad/commit/?id=afbbc3a97ec391c6a582f3c746965fdc3eb3e1f3
        // This may affect things like setting the config, so until the bug is
        // fixed, make sure that state dependent code happens before this line.
        // The estimated version for the fix is 1.14.5 / 1.15.1.
        // https://github.com/servo/servo/issues/22010#issuecomment-432599657
        let uri = match self.stream_type {
            StreamType::Stream => {
                register_servo_media_stream_src().map_err(|error| {
                    PlayerError::Backend(format!(
                        "servomediastreamsrc registration error: {error:?}"
                    ))
                })?;
                "mediastream://".to_value()
            },
            StreamType::Seekable => {
                register_servo_src().map_err(|error| {
                    PlayerError::Backend(format!("servosrc registration error: {error:?}"))
                })?;
                "servosrc://".to_value()
            },
            StreamType::NetworkUri => {
                // Let playbin3 own the source: it auto-plugs the right element
                // (e.g. rtspsrc) for the scheme. No servo AppSrc is registered.
                let uri = self.network_uri.clone().ok_or_else(|| {
                    PlayerError::Backend("NetworkUri stream type requires a network_uri".to_owned())
                })?;
                uri.to_value()
            },
        };
        player.set_property("uri", &uri);

        // No video_renderers no video
        if self.video_renderer.is_none() {
            player.set_video_track_enabled(false);
        }

        *self.inner.borrow_mut() = Some(Arc::new(Mutex::new(PlayerInner {
            player,
            _signal_adapter: signal_adapter.clone(),
            source: None,
            video_sink,
            input_size: 0,
            seekable: true,
            play_state: gstreamer_play::PlayState::Stopped,
            paused: Cell::new(DEFAULT_PAUSED),
            can_resume: Cell::new(DEFAULT_CAN_RESUME),
            playback_rate: Cell::new(DEFAULT_PLAYBACK_RATE),
            muted: Cell::new(DEFAULT_MUTED),
            custom_audio_renderer: self.audio_renderer.is_some(),
            volume: Cell::new(DEFAULT_VOLUME),
            stream_type: self.stream_type,
            last_metadata: None,
            cat: gstreamer::DebugCategory::get("servoplayer").unwrap(),
            enough_data: Arc::new(AtomicBool::new(false)),
        })));

        let inner = self.inner.borrow();
        let inner = inner.as_ref().unwrap();
        let observer = self.observer.clone();
        // Handle `end-of-stream` signal.
        signal_adapter.connect_end_of_stream(move |_| {
            let _ = notify!(observer, PlayerEvent::EndOfStream);
        });

        let observer = self.observer.clone();
        // Handle `error` signal
        signal_adapter.connect_error(move |_self, error, _details| {
            let _ = notify!(observer, PlayerEvent::Error(error.to_string()));
        });

        let inner_clone = inner.clone();
        let observer = self.observer.clone();
        // Handle `state-changed` signal.
        signal_adapter.connect_state_changed(move |_, play_state| {
            inner_clone.lock().unwrap().play_state = play_state;

            let state = match play_state {
                gstreamer_play::PlayState::Buffering => Some(PlaybackState::Buffering),
                gstreamer_play::PlayState::Stopped => Some(PlaybackState::Stopped),
                gstreamer_play::PlayState::Paused => Some(PlaybackState::Paused),
                gstreamer_play::PlayState::Playing => Some(PlaybackState::Playing),
                _ => None,
            };
            if let Some(v) = state {
                let _ = notify!(observer, PlayerEvent::StateChanged(v));
            }
        });

        let observer = self.observer.clone();
        // Handle `position-update` signal.
        signal_adapter.connect_position_updated(move |_, position| {
            if let Some(seconds) = position.map(|p| p.seconds_f64()) {
                let _ = notify!(observer, PlayerEvent::PositionChanged(seconds));
            }
        });

        let observer = self.observer.clone();
        // Handle `seek-done` signal.
        signal_adapter.connect_seek_done(move |_, position| {
            let _ = notify!(observer, PlayerEvent::SeekDone(position.seconds_f64()));
        });

        // Handle `media-info-updated` signal.
        let inner_clone = inner.clone();
        let observer = self.observer.clone();
        signal_adapter.connect_media_info_updated(move |_, info| {
            let Ok(metadata) = metadata_from_media_info(info) else {
                return;
            };

            let mut inner = inner_clone.lock().unwrap();

            if inner.last_metadata.as_ref() == Some(&metadata) {
                return;
            }

            // TODO: Workaround to generate expected `paused` state change event.
            // <https://github.com/servo/servo/issues/40740>
            let mut send_pause_event = false;

            if inner.last_metadata.is_none() && metadata.is_seekable {
                if inner.playback_rate.get() != DEFAULT_PLAYBACK_RATE {
                    // The `paused` state change event will be fired after the
                    // seek initiated by the playback rate change has
                    // completed.
                    inner.player.set_rate(inner.playback_rate.get());
                } else if inner.play_state == gstreamer_play::PlayState::Paused {
                    send_pause_event = true;
                }
            }

            inner.last_metadata = Some(metadata.clone());
            let env_audio_disabled = env_flag_enabled(DISABLE_AUDIO_ENV);
            let audio_track_enabled = !inner.muted.get() && !env_audio_disabled;
            inner.player.set_audio_track_enabled(audio_track_enabled);
            if !inner.custom_audio_renderer {
                if inner.muted.get() || env_audio_disabled {
                    let _ = disable_pipeline_audio_sink(&inner.player.pipeline(), "muted");
                }
            }
            gstreamer::info!(
                inner.cat,
                obj = &inner.player,
                "Metadata updated: {:?}",
                metadata
            );
            let _ = notify!(observer, PlayerEvent::MetadataUpdated(metadata));

            if send_pause_event {
                let _ = notify!(observer, PlayerEvent::StateChanged(PlaybackState::Paused));
            }
        });

        // Handle `duration-changed` signal.
        let inner_clone = inner.clone();
        let observer = self.observer.clone();
        signal_adapter.connect_duration_changed(move |_, duration| {
            let duration = duration.map(|duration| {
                time::Duration::new(
                    duration.seconds(),
                    (duration.nseconds() % 1_000_000_000) as u32,
                )
            });

            let mut inner = inner_clone.lock().unwrap();
            if let Some(ref mut metadata) = inner.last_metadata
                && metadata.duration != duration
            {
                metadata.duration = duration;
                gstreamer::info!(
                    inner.cat,
                    obj = &inner.player,
                    "Duration changed: {:?}",
                    duration
                );
                let _ = notify!(observer, PlayerEvent::DurationChanged(duration));
            }
        });

        if let Some(video_renderer) = self.video_renderer.clone() {
            let sample_diagnostics = Arc::new(Mutex::new(VideoSampleDiagnostics::default()));
            // Creates a closure that renders a frame using the video_renderer
            // Used in the preroll and sample callbacks
            let render_sample = {
                let render = self.render.clone();
                let observer = self.observer.clone();
                let sample_diagnostics = sample_diagnostics.clone();
                let weak_video_renderer = Arc::downgrade(&video_renderer);

                move |sample: gstreamer::Sample| {
                    sample_diagnostics.lock().unwrap().note_sample(&sample);

                    let Some(frame) = render.lock().unwrap().get_frame_from_sample(sample) else {
                        return Err(gstreamer::FlowError::Error);
                    };

                    match weak_video_renderer.upgrade() {
                        Some(video_renderer) => {
                            video_renderer.lock().unwrap().render(frame);
                        },
                        _ => {
                            return Err(gstreamer::FlowError::Flushing);
                        },
                    };

                    let _ = notify!(observer, PlayerEvent::VideoFrameUpdated);
                    Ok(gstreamer::FlowSuccess::Ok)
                }
            };

            // Set video_sink callbacks.
            inner.lock().unwrap().video_sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_preroll({
                        let render_sample = render_sample.clone();
                        move |video_sink| {
                            render_sample(
                                video_sink
                                    .pull_preroll()
                                    .map_err(|_| gstreamer::FlowError::Eos)?,
                            )
                        }
                    })
                    .new_sample(move |video_sink| {
                        render_sample(
                            video_sink
                                .pull_sample()
                                .map_err(|_| gstreamer::FlowError::Eos)?,
                        )
                    })
                    .build(),
            );
        };

        let (receiver, error_handler_id) = {
            let inner_clone = inner.clone();
            let inner = inner.lock().unwrap();
            let pipeline = inner.player.pipeline();

            let (sender, receiver) = mpsc::channel();

            let sender = Arc::new(Mutex::new(sender));
            let sender_clone = sender.clone();
            let is_ready_clone = self.is_ready.clone();
            let observer = self.observer.clone();
            pipeline.connect("source-setup", false, move |args| {
                let source = args[1].get::<gstreamer::Element>().unwrap();

                let mut inner = inner_clone.lock().unwrap();
                let source = match inner.stream_type {
                    StreamType::Seekable => {
                        let servosrc = source
                            .dynamic_cast::<ServoSrc>()
                            .expect("Source element is expected to be a ServoSrc!");

                        if inner.input_size > 0 {
                            servosrc.set_size(inner.input_size as i64);
                        }
                        servosrc.set_seekable(inner.seekable);

                        let sender_clone = sender.clone();
                        let is_ready = is_ready_clone.clone();
                        let observer_ = observer.clone();
                        let observer__ = observer.clone();
                        let observer___ = observer.clone();
                        let servosrc_ = servosrc.clone();
                        let enough_data_ = inner.enough_data.clone();
                        let enough_data__ = inner.enough_data.clone();
                        let seek_channel = Arc::new(Mutex::new(SeekChannel::new()));
                        servosrc.set_callbacks(
                            gstreamer_app::AppSrcCallbacks::builder()
                                .need_data(move |_, _| {
                                    // We block the caller of the setup method until we get
                                    // the first need-data signal, so we ensure that we
                                    // don't miss any data between the moment the client
                                    // calls setup and the player is actually ready to
                                    // get any data.
                                    is_ready.call_once(|| {
                                        let _ = sender_clone.lock().unwrap().send(Ok(()));
                                    });

                                    enough_data_.store(false, Ordering::Relaxed);
                                    let _ = notify!(observer_, PlayerEvent::NeedData);
                                })
                                .enough_data(move |_| {
                                    enough_data__.store(true, Ordering::Relaxed);
                                    let _ = notify!(observer__, PlayerEvent::EnoughData);
                                })
                                .seek_data(move |_, offset| {
                                    let (ret, ack_channel) = if servosrc_.set_seek_offset(offset) {
                                        let _ = notify!(
                                            observer___,
                                            PlayerEvent::SeekData(
                                                offset,
                                                seek_channel.lock().unwrap().sender()
                                            )
                                        );
                                        let (ret, ack_channel) =
                                            seek_channel.lock().unwrap()._await();
                                        (ret, Some(ack_channel))
                                    } else {
                                        (true, None)
                                    };

                                    servosrc_.set_seek_done();
                                    if let Some(ack_channel) = ack_channel {
                                        ack_channel.send(()).unwrap();
                                    }
                                    ret
                                })
                                .build(),
                        );

                        PlayerSource::Seekable(servosrc)
                    },
                    StreamType::Stream => {
                        let media_stream_src = source
                            .dynamic_cast::<ServoMediaStreamSrc>()
                            .expect("Source element is expected to be a ServoMediaStreamSrc!");
                        let sender_clone = sender.clone();
                        is_ready_clone.call_once(|| {
                            let _ = notify!(sender_clone, Ok(()));
                        });
                        PlayerSource::Stream(media_stream_src)
                    },
                    StreamType::NetworkUri => {
                        // The auto-plugged source (e.g. rtspsrc) is owned by
                        // playbin3, so we must NOT dynamic_cast it to a servo
                        // AppSrc here. There is also no `need-data` signal to wait
                        // on, so unblock `setup()` immediately. No PlayerSource is
                        // stored: data is pulled by the backend, never pushed.
                        let sender_clone = sender.clone();
                        is_ready_clone.call_once(|| {
                            let _ = notify!(sender_clone, Ok(()));
                        });
                        return None;
                    },
                };

                inner.set_src(source);

                None
            });

            let error_handler_id =
                signal_adapter.connect_error(move |signal_adapter, error, _details| {
                    let _ = notify!(sender_clone, Err(PlayerError::Backend(error.to_string())));
                    signal_adapter.play().stop();
                });

            inner.player.pause();

            (receiver, error_handler_id)
        };

        let result = receiver.recv().unwrap();
        glib::signal::signal_handler_disconnect(&inner.lock().unwrap().player, error_handler_id);
        result
    }
}

macro_rules! inner_player_proxy_getter {
    ($fn_name:ident, $return_type:ty, $default_value:expr_2021) => {
        fn $fn_name(&self) -> $return_type {
            if self.setup().is_err() {
                return $default_value;
            }

            let inner = self.inner.borrow();
            let inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name()
        }
    };
}

macro_rules! inner_player_proxy {
    ($fn_name:ident, $return_type:ty) => {
        fn $fn_name(&self) -> Result<$return_type, PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name()
        }
    };

    ($fn_name:ident, $arg1:ident, $arg1_type:ty) => {
        fn $fn_name(&self, $arg1: $arg1_type) -> Result<(), PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name($arg1)
        }
    };

    ($fn_name:ident, $arg1:ident, $arg1_type:ty, $arg2:ident, $arg2_type:ty) => {
        fn $fn_name(&self, $arg1: $arg1_type, $arg2: $arg2_type) -> Result<(), PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name($arg1, $arg2)
        }
    };
}

impl Player for GStreamerPlayer {
    inner_player_proxy!(play, ());
    inner_player_proxy!(pause, ());
    inner_player_proxy_getter!(paused, bool, DEFAULT_PAUSED);
    inner_player_proxy_getter!(can_resume, bool, DEFAULT_CAN_RESUME);
    inner_player_proxy!(stop, ());
    inner_player_proxy!(end_of_stream, ());
    inner_player_proxy!(set_input_size, size, u64);
    inner_player_proxy!(set_seekable, seekable, bool);
    inner_player_proxy!(set_mute, muted, bool);
    inner_player_proxy_getter!(muted, bool, DEFAULT_MUTED);
    inner_player_proxy!(set_playback_rate, playback_rate, f64);
    inner_player_proxy_getter!(playback_rate, f64, DEFAULT_PLAYBACK_RATE);
    inner_player_proxy!(push_data, data, Vec<u8>);
    inner_player_proxy!(seek, time, f64);
    inner_player_proxy!(set_volume, volume, f64);
    inner_player_proxy_getter!(volume, f64, DEFAULT_VOLUME);
    inner_player_proxy_getter!(buffered, Vec<Range<f64>>, DEFAULT_TIME_RANGES);
    inner_player_proxy_getter!(seekable, Vec<Range<f64>>, DEFAULT_TIME_RANGES);
    inner_player_proxy!(set_stream, stream, &MediaStreamId, only_stream, bool);
    inner_player_proxy!(set_audio_track, stream_index, i32, enabled, bool);
    inner_player_proxy!(set_video_track, stream_index, i32, enabled, bool);

    fn render_use_gl(&self) -> bool {
        self.render.lock().unwrap().is_gl()
    }
}

impl MediaInstance for GStreamerPlayer {
    fn get_id(&self) -> usize {
        self.id
    }

    fn mute(&self, val: bool) -> Result<(), MediaInstanceError> {
        self.set_mute(val).map_err(|_| MediaInstanceError)
    }

    fn suspend(&self) -> Result<(), MediaInstanceError> {
        self.pause().map_err(|_| MediaInstanceError)
    }

    fn resume(&self) -> Result<(), MediaInstanceError> {
        if !self.can_resume() {
            return Ok(());
        }

        self.play().map_err(|_| MediaInstanceError)
    }
}

impl Drop for GStreamerPlayer {
    fn drop(&mut self) {
        let _ = self.stop();
        let (tx_ack, rx_ack) = mpsc::channel();
        let _ = self
            .backend_chan
            .lock()
            .unwrap()
            .send(BackendMsg::Shutdown {
                context: self.context_id,
                id: self.id,
                tx_ack,
            });
        let _ = rx_ack.recv();
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, Once};
use std::time::{self, Duration, Instant};

use byte_slice_cast::AsSliceOf;
use glib;
use glib::prelude::*;
use gstreamer;
use gstreamer_app;
use gstreamer_play;
use gstreamer_play::prelude::*;
use ipc_channel::ipc::{IpcReceiver, IpcSender, channel};
use servo_config::{debug_env, pref};
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
// 오디오 켜짐/꺼짐(config-surface-consolidation Task 5: `media_audio_enabled` pref, 구 env
// `SERVO_GSTREAMER_DISABLE_AUDIO`). pref 는 긍정형(켜는 스위치)이라 옛 이름과 의미가
// 뒤집혔다 — 아래 호출부는 `!pref!(media_audio_enabled)` 로 옛 `disable_audio` truthy 판정을
// 재현한다.
//
// Caps the worker-thread count of software libav video decoders (avdec_*, `media_
// avdec_max_threads` pref, 구 env `SERVO_GSTREAMER_AVDEC_MAX_THREADS`). Each avdec otherwise
// auto-spawns ~CPU-count threads plus a proportional decoded-frame pool; with many
// simultaneous <video> elements this explodes thread count and memory. `-1` = leave automatic
// (no change, e.g. for the single 4K wall video that needs multithreaded decode).
//
// Opt-in seamless (gapless) looping for <video loop> (`media_gapless_loop_enabled` pref, 구 env
// `SERVO_MEDIA_GAPLESS_LOOP`). The spec path loops via EOS -> script "ended" handling ->
// flushing seek(0), which stalls the decoder pipeline at every loop boundary; with many
// simultaneous videos each boundary shows up as a visible display hold (frames held 3+
// refreshes). When enabled and the element has the loop attribute, the pipeline instead runs
// in SEGMENT-seek mode and is rewound with a NON-flushing SEGMENT seek on SEGMENT_DONE:
// decoders never flush and no EOS reaches the script layer while looping.
//
// Opt-in direct local-file playback (`media_direct_file_enabled` pref, 구 env
// `SERVO_MEDIA_DIRECT_FILE`). When enabled and the media resource is a `file://` URL pointing
// at an existing file, playbin is pointed straight at that file (its own filesrc) instead of
// the servosrc byte-push path. GStreamer then reads the file itself, so loop rewinds and
// seeks never round-trip through the script layer (the confirmed cause of the intermittent
// per-tile stall at gapless loop-wrap boundaries — see §14 of the investigation-loop-stall
// report). The OS page cache absorbs the reads, so this achieves the servosrc byte-cache
// effect at effectively zero extra process RAM. Off, or a non-file URL, or a missing file →
// the servosrc path is used unchanged (byte-identical).
const VIDEO_SAMPLE_INFO_INTERVAL: u64 = 120;
const VIDEO_SAMPLE_LATE_GAP_MS: f64 = 20.0;

/// 재앵커 문턱. 이만큼 넘게 밀렸으면 따라잡기를 포기하고 현재 시점을 새 기준으로 삼는다.
///
/// 따라잡지 않는 쪽을 택한 이유: 밀린 만큼을 전속력으로 디코드하면 부하가 더 늘어 더
/// 밀리는 양의 되먹임이 된다. 늦은 프레임을 버리지 않고 그 지점부터 정상 속도로 잇는 편이
/// 벽에서는 낫다(영상 간 위상은 어차피 `thread` 페이싱의 비범위다).
const SINK_PACER_RESYNC_AFTER: Duration = Duration::from_secs(1);

/// `media_video_sink_pacing=thread` 일 때 비디오 스트리밍 스레드를 PTS 에 맞춰 재운다.
///
/// GstBaseSink 의 `sync=true` 와 같은 일을 하되 **파이프라인마다 자기 앵커**를 쓴다.
/// `GstSystemClock::obtain()` 이 프로세스당 싱글턴이라, 45 개 싱크가 프레임마다 같은
/// 객체에서 대기하는 것이 디코딩만큼의 CPU 를 더 쓰는 것이 실측됐다(0.795 vs 0.284
/// 코어/영상). 공유 객체를 없애면 그 몫이 사라진다.
///
/// ★비범위★ — 앵커가 파이프라인마다 독립이므로 영상 간 동기는 보장되지 않는다.
/// 그건 별도 과제(Video Sync Group)다.
#[derive(Default)]
struct SinkPacer {
    /// (기준 시각, 그 시각에 해당하는 PTS).
    anchor: Option<(Instant, gstreamer::ClockTime)>,
}

impl SinkPacer {
    /// 이 버퍼를 내보내기 전에 얼마나 자야 하는지. 잘 필요가 없으면 `None`.
    ///
    /// 잠은 호출자가 잔다 — 뮤텍스를 쥔 채로 자지 않기 위함이다.
    fn sleep_before(&mut self, pts: Option<gstreamer::ClockTime>) -> Option<Duration> {
        let pts = pts?;
        let now = Instant::now();
        // 첫 버퍼이거나 PTS 가 뒤로 감겼으면(gapless 루프/seek) 여기서 다시 기준을 잡는다.
        let (anchor_at, anchor_pts) = match self.anchor {
            Some(anchor) if pts >= anchor.1 => anchor,
            _ => {
                self.anchor = Some((now, pts));
                return None;
            },
        };
        let target = anchor_at + Duration::from_nanos((pts - anchor_pts).nseconds());
        if target > now {
            return Some(target - now);
        }
        if now.duration_since(target) > SINK_PACER_RESYNC_AFTER {
            self.anchor = Some((now, pts));
        }
        None
    }
}

/// VIDEORATE 요약 주기. 초당 한 줄이면 45타일에서도 읽을 만하고, 재생 속도가 1.0배인지
/// 아닌지는 1초 창이면 충분히 갈린다.
const VIDEO_RATE_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// 파이프라인마다 안정적인 번호를 붙인다. 45개가 동시에 찍으므로 어느 줄이 어느
/// 파이프라인 것인지 구분되지 않으면 요약을 읽을 수 없다.
static NEXT_VIDEO_DIAGNOSTICS_ID: AtomicU64 = AtomicU64::new(0);

struct VideoSampleDiagnostics {
    id: u64,
    sample_count: u64,
    summary_frame_count: u64,
    summary_started_at: Option<Instant>,
    last_sample_at: Option<Instant>,
    last_pts: Option<gstreamer::ClockTime>,
    late_pts_gaps_since_summary: u64,
    late_wall_gaps_since_summary: u64,
    /// VIDEORATE 창. 위의 summary 카운터와 분리해 둔다 — 저쪽은 프레임 수 기준이고
    /// late 갭에도 리셋되므로 벽시계 기준 비율을 낼 수 없다.
    rate_started_at: Option<Instant>,
    rate_frames: u64,
    rate_pts_start: Option<gstreamer::ClockTime>,
}

impl Default for VideoSampleDiagnostics {
    fn default() -> Self {
        Self {
            id: NEXT_VIDEO_DIAGNOSTICS_ID.fetch_add(1, Ordering::Relaxed),
            sample_count: 0,
            summary_frame_count: 0,
            summary_started_at: None,
            last_sample_at: None,
            last_pts: None,
            late_pts_gaps_since_summary: 0,
            late_wall_gaps_since_summary: 0,
            rate_started_at: None,
            rate_frames: 0,
            rate_pts_start: None,
        }
    }
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

/// 파이프라인에 실제로 들어간 요소를 전부 찍는다(파이프라인당 1 회, 핫패스 아님).
///
/// 예전에는 코덱/변환 계열만 골라 찍었는데, `queue` 와 `multiqueue` 는 klass 가 Generic
/// 이라 그 필터를 통과하지 못했다. 그 결과 월의 로그에는 h264parse/avdec/appsink 세 종만
/// 남았고, **디코더 뒤에 큐가 있는지조차 로그로 알 수 없었다.**
///
/// 그게 지금 문제가 되는 이유: 45 영상에서 영상당 디코드 비용이 0.39 코어여야 하는데
/// 0.98 이고, 그 차이의 상당 부분이 **원본 3.1MB 프레임이 스레드 경계를 몇 번 건너는가**
/// 에서 온다(측정: 앞 큐만 0.294, 앞+뒤 0.729). 스레드 토폴로지가 곧 성능이므로
/// 토폴로지를 만드는 요소를 로그에서 빼면 안 된다.
fn should_log_pipeline_element(_factory_name: &str, _klass: &str) -> bool {
    true
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

// Apply the `media_avdec_max_threads` pref (구 env SERVO_GSTREAMER_AVDEC_MAX_THREADS) to
// software libav video decoders (avdec_*) as they are auto-plugged into the pipeline. Capping
// to a small value (e.g. 1) collapses the per-decoder worker-thread count and decoded-frame
// pool, which is what lets many <video> tiles decode at once without saturating CPU
// scheduling (FPS jitter) or memory. `-1`(기본값) leaves the decoder at its automatic thread
// count so single-video/4K playback is unchanged.
/// Tune the RTSP elements that playbin3 auto-plugs for an `rtsp://` URI.
///
/// We hand playbin3 nothing but the URI, so every RTSP element arrives with GStreamer's
/// defaults. Two of those defaults matter enough to expose:
///
///   - `rtspsrc latency` defaults to **2000 ms**, and that jitter buffer is what dominates
///     time-to-first-frame. Measured against the in-house camera with `gst-launch`: 2.02s at
///     the default, 0.76s at `latency=0`, 0.73s at `latency=200`. Hand-assembling the whole
///     pipeline instead of using playbin3 was worth only 0.24s by comparison — the lever is
///     this property, not the pipeline's shape.
///   - `rtph264depay wait-for-keyframe` defaults to **false**, so the depayloader will emit
///     slices from mid-GOP. When those reach `h264parse` before the parameter sets do, it
///     fails with `Could not decode stream. No caps set` and the element stalls for good
///     (there is no retry path). That is the intermittent RTSP failure.
///
/// ★These two pull in opposite directions★ — shrinking the jitter buffer makes it *more*
/// likely that slices outrun the parameter sets. Both are therefore off by default (`-1` /
/// `false` = leave GStreamer's value alone), so this function is a no-op unless an operator
/// opts in, and the two are meant to be evaluated together rather than one at a time.
///
/// `eprintln!` rather than `log::debug!` on purpose: the in-house GStreamer build caps its
/// own debug output at WARNING, so `GST_DEBUG=rtspsrc:5` yields nothing and our own stderr is
/// the only way to see what was applied.
fn configure_rtsp_elements(element: &gstreamer::Element) {
    let Some(factory) = element.factory() else {
        return;
    };
    match factory.name().as_str() {
        "rtspsrc" => {
            let latency_ms = pref!(media_rtsp_latency_ms);
            if latency_ms < 0 {
                return;
            }
            let latency_ms = u32::try_from(latency_ms).unwrap_or(u32::MAX);
            element.set_property("latency", latency_ms);
            eprintln!("[RTSP-DIAG] rtspsrc: latency set to {latency_ms}ms");
        },
        "rtph264depay" => {
            if !pref!(media_rtsp_wait_for_keyframe) {
                return;
            }
            element.set_property("wait-for-keyframe", true);
            eprintln!("[RTSP-DIAG] rtph264depay: wait-for-keyframe set to true");
        },
        _ => {},
    }
}

fn configure_software_decoder_threads(element: &gstreamer::Element) {
    let Some(factory) = element.factory() else {
        return;
    };
    let factory_name = factory.name();
    let klass = factory.metadata("klass").unwrap_or_default();
    // Most avdec video decoders share the GstFFMpegVidDec base which exposes
    // "max-threads", but single-threaded codecs (e.g. avdec_wmv2, avdec_flv) do NOT —
    // this holds on both GStreamer 1.22.8 and 1.28.4.100 (verified via gst-inspect).
    if !factory_name.starts_with("avdec_") || !klass.contains("Video") {
        return;
    }
    let max_threads = pref!(media_avdec_max_threads);
    // `-1` = 미설정 보초값 — 구 env 부재와 동일하게 자동 스레드 수를 그대로 둔다. 그보다
    // 작은 음수는 오타다. 구 env 경로가 그런 값에 warn 을 찍었으므로 그대로 유지한다 —
    // 조용히 무시하면 캡을 걸었다고 믿는데 안 걸린 상태가 된다.
    if max_threads < 0 {
        if max_threads < -1 {
            log::warn!(
                "Ignoring invalid media_avdec_max_threads={max_threads}; \
                 expected -1 (automatic) or a non-negative thread cap"
            );
        }
        return;
    }
    // Setting an absent GObject property panics (and, occurring inside a non-unwinding
    // GStreamer callback, aborts the process). Gate on presence.
    if element.find_property("max-threads").is_none() {
        log::debug!("{factory_name} has no max-threads property; skipping");
        return;
    }
    // GObject 프로퍼티는 gint(i32) — pref 는 i64 라 그대로 캐스팅하면 랩어라운드한다.
    // 스레드 캡이라 상한을 넘는 값은 i32::MAX 로 포화시키는 편이 뒤집히는 것보다 낫다.
    let max_threads = max_threads.min(i32::MAX as i64) as i32;
    element.set_property("max-threads", max_threads);
    log::info!("Set {factory_name} max-threads={max_threads}");
}

fn configure_playbin_flags(
    pipeline: &gstreamer::Element,
    prefer_native_video: bool,
) -> Result<(), PlayerError> {
    let flags = pipeline.property_value("flags");
    let flags_class = glib::FlagsClass::with_type(flags.type_()).ok_or_else(|| {
        PlayerError::Backend("FlagsClass creation failed".to_owned())
    })?;
    let mut flags_builder = flags_class.builder_with_value(flags).ok_or_else(|| {
        PlayerError::Backend("FlagsClass creation failed".to_owned())
    })?;

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

    // media_audio_enabled 는 긍정형 pref 다(구 env DISABLE_AUDIO_ENV 는 부정형이었다) —
    // 단항 부정 한 번으로 옛 disable_audio truthy 판정을 재현한다.
    let disable_audio = !pref!(media_audio_enabled);
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

/// `SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF`(servo_config::debug_env 등록)의 truthy 판정.
/// `htmlmediaelement.rs`의 `disable_enough_data_backoff()`와 동일한 판정식이다(2026-08-11
/// 조사로 확인함) — Task 5 로 pref 가 된 media_* 노브들과 문자 그대로 같은 truthy
/// 집합(1/true/yes/on, 대소문자 무시)이지만, 이 노브는 조사용이라 이관 대상이 아니고
/// 이름 문자열 없이 debug_env 상수로 읽어야 해서 별도 함수로 둔다.
fn enoughdata_backoff_disabled() -> bool {
    debug_env::string(&debug_env::MEDIA_DISABLE_ENOUGHDATA_BACKOFF).is_some_and(|value| {
        value == "1"
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
            || value.eq_ignore_ascii_case("on")
    })
}

/// `SERVO_MEDIA_VIDEO_RATE`(servo_config::debug_env 등록)의 truthy 판정.
/// 위 `enoughdata_backoff_disabled()` 와 같은 집합을 쓴다.
///
/// 프레임마다 불리는 자리라 debug_env 의 OnceLock 캐시에 의존한다 — 매번 환경변수를
/// 읽으면 이 진단이 측정하려는 비용에 자기 자신이 섞인다.
fn video_rate_logging_enabled() -> bool {
    debug_env::string(&debug_env::MEDIA_VIDEO_RATE).is_some_and(|value| {
        value == "1"
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
            || value.eq_ignore_ascii_case("on")
    })
}

/// `media_pipeline_mode` 해석. 인정 토큰 밖은 경고 후 playbin3.
fn use_uridecodebin3_pipeline() -> bool {
    let value = pref!(media_pipeline_mode);
    if value.eq_ignore_ascii_case("uridecodebin3") {
        return true;
    }
    if !value.is_empty() && !value.eq_ignore_ascii_case("playbin3") {
        log::warn!(
            "Ignoring invalid media_pipeline_mode={value:?}; expected playbin3 or uridecodebin3"
        );
    }
    false
}

/// `uridecodebin3 ! appsink` 파이프라인을 만든다. playsink 가 없으므로 디코더 뒤 큐도 없다.
///
/// 반환: (pipeline, uridecodebin3). 링크는 `pad-added` 에서 이뤄진다 — decodebin3 는
/// 스트림을 발견한 뒤에야 소스 패드를 낸다.
fn build_uridecodebin3_pipeline(
    uri: &str,
    appsink: &gstreamer_app::AppSink,
) -> Result<(gstreamer::Pipeline, gstreamer::Element), PlayerError> {
    let pipeline = gstreamer::Pipeline::new();
    let decodebin = gstreamer::ElementFactory::make("uridecodebin3")
        .property("uri", uri)
        .build()
        .map_err(|error| {
            PlayerError::Backend(format!("uridecodebin3 creation failed: {error:?}"))
        })?;
    let sink = appsink.upcast_ref::<gstreamer::Element>().clone();
    pipeline
        .add_many([&decodebin, &sink])
        .map_err(|error| PlayerError::Backend(format!("pipeline add failed: {error:?}")))?;

    // 비디오 패드 하나만 연결한다. 오디오 패드는 링크하지 않고 두면 decodebin3 가 그
    // 스트림을 계속 흘려보내며 multiqueue 패드를 유지하므로, 나중에 select-streams 로
    // 아예 받지 않도록 하는 것이 다음 단계다.
    let sink_for_pad = sink.clone();
    decodebin.connect_pad_added(move |_, pad| {
        let is_video = pad
            .current_caps()
            .and_then(|caps| caps.structure(0).map(|s| s.name().starts_with("video/")))
            .unwrap_or(false);
        if !is_video {
            return;
        }
        let Some(target) = sink_for_pad.static_pad("sink") else {
            return;
        };
        if target.is_linked() {
            // 비디오 스트림이 둘 이상인 파일: 첫 번째만 쓴다.
            return;
        }
        if let Err(error) = pad.link(&target) {
            log::error!("uridecodebin3: linking video pad to appsink failed: {error:?}");
        }
    });

    Ok((pipeline, decodebin))
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

impl VideoSampleDiagnostics {
    /// 이 파이프라인이 실제로 초당 몇 프레임을 내보내고 있는지, 그리고 pts 가 벽시계
    /// 대비 몇 배로 진행하는지를 1초에 한 줄로 남긴다.
    ///
    /// ★왜 필요한가★ — 45영상 월에서 디코드 스레드가 영상당 0.98코어에 붙는데, 이 장비의
    /// 단일 스레드 디코드 상한이 재생속도의 2.8배다. 그러면 원인이 둘로 갈린다: 디코더가
    /// 1x보다 빠르게 앞질러 도는 것인가(=싱크가 throttle 을 못 함), 아니면 1x로 도는데
    /// 프레임당 비용이 경합으로 부풀어 오른 것인가. CPU 수치만으로는 두 경우가 구분되지
    /// 않는다 — 초당 전달 프레임 수를 직접 세는 수밖에 없다.
    ///
    /// `pts_rate` 가 1.0 이면 정상 재생, 2.7 이면 그만큼 앞질러 도는 것이다.
    ///
    /// gapless 루프가 돌면 pts 가 뒤로 감기므로 그 창은 비율을 내지 않고 버린다(`wrapped`).
    /// 기본은 off — `SERVO_MEDIA_VIDEO_RATE` 로 켠다. 45타일 장시간 운용에서 초당 45줄이
    /// 쌓이기 때문이고, 이는 기존 sample summary 가 debug 에 머무는 이유와 같다.
    fn note_rate(&mut self, now: Instant, pts: Option<gstreamer::ClockTime>) {
        if !video_rate_logging_enabled() {
            return;
        }
        self.rate_frames = self.rate_frames.saturating_add(1);
        let Some(started_at) = self.rate_started_at else {
            self.rate_started_at = Some(now);
            self.rate_pts_start = pts;
            return;
        };
        let elapsed = now.saturating_duration_since(started_at);
        if elapsed < VIDEO_RATE_REPORT_INTERVAL {
            return;
        }
        let seconds = elapsed.as_secs_f64();
        let pts_rate = match (self.rate_pts_start, pts) {
            (Some(start), Some(end)) if end >= start => {
                Some((end - start).nseconds() as f64 / 1_000_000_000.0 / seconds)
            },
            _ => None,
        };
        log::info!(
            target: "media",
            "VIDEORATE id={} fps={:.1} pts_rate={} frames={} window_ms={:.0}",
            self.id,
            self.rate_frames as f64 / seconds,
            match pts_rate {
                Some(rate) => format!("{rate:.2}x"),
                None => "wrapped".to_string(),
            },
            self.rate_frames,
            seconds * 1000.0,
        );
        self.rate_started_at = Some(now);
        self.rate_pts_start = pts;
        self.rate_frames = 0;
    }

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
        self.note_rate(now, pts);

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
            // debug 레벨: late 갭 조건이 다중 타일 lockstep에서 사실상 매 프레임 참이 되어
            // info로는 장시간 표출 시 로그가 GB급으로 폭증한다 (45타일 2.6h에 3.6GB 실측).
            log::debug!(
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
    /// `None` when the pipeline was not built by `gstreamer_play::Play`.
    ///
    /// `Play` owns a playbin3, and playbin3 owns a playsink, and playsink is what inserts
    /// the `vqueue` between the decoder and our appsink -- the hop that carries every raw
    /// 3.1MB frame across a thread boundary. `media_pipeline_mode=uridecodebin3` builds the
    /// pipeline directly instead, so there is no `Play` to hold.
    player: Option<gstreamer_play::Play>,
    _signal_adapter: Option<gstreamer_play::PlaySignalAdapter>,
    /// The top-level pipeline, whoever built it. Previously this was always reached through
    /// `player.pipeline()`; it is a field now because `player` can be `None`.
    pipeline: gstreamer::Element,
    source: Option<PlayerSource>,
    video_sink: gstreamer_app::AppSink,
    input_size: u64,
    seekable: bool,
    play_state: gstreamer_play::PlayState,
    paused: Cell<bool>,
    can_resume: Cell<bool>,
    playback_rate: Cell<f64>,
    muted: Cell<bool>,
    volume: Cell<f64>,
    stream_type: StreamType,
    last_metadata: Option<Metadata>,
    cat: gstreamer::DebugCategory,
    enough_data: Arc<AtomicBool>,
    /// Whether the element wants looping playback (see `media_gapless_loop_enabled`; always
    /// false when the pref is off, in which case looping stays on the spec's EOS path).
    looping: Cell<bool>,
    /// Whether the pipeline is currently in SEGMENT-seek mode for gapless looping.
    segment_loop_active: Cell<bool>,
    /// Channel to the gapless-loop worker thread (`None` when `media_gapless_loop_enabled`
    /// is off).
    gapless_loop_sender: Option<Sender<GaplessLoopMsg>>,
    /// Sync-group start (see `media_sync_group_target`): play() was requested but the
    /// pipeline is held paused until the group releases.
    sync_hold: Cell<bool>,
    /// Whether this pipeline has been armed and registered with the sync group.
    sync_armed: Cell<bool>,
    /// Direct local-file playback (see `media_direct_file_enabled`): playbin reads the file
    /// itself via its own filesrc, so there is no servosrc. The element still fetches and
    /// pushes bytes, which are dropped as harmless no-ops (`push_data`/`set_input_size`).
    direct_file: Cell<bool>,
}

/// Messages for the gapless-loop worker thread (see `media_gapless_loop_enabled`).
enum GaplessLoopMsg {
    /// (Re-)evaluate whether the pipeline should enter SEGMENT-seek mode.
    MaybeEnter,
    /// The current segment finished; rewind with a non-flushing SEGMENT seek.
    SegmentDone,
    /// Arm this pipeline for a synchronized group start (see `media_sync_group_target`).
    ArmSyncGroup,
}

// Opt-in synchronized start for many simultaneous <video> pipelines (video wall).
// `media_sync_group_target=N`(config-surface-consolidation Task 5, 구 env
// SERVO_MEDIA_SYNC_GROUP=N — 온오프가 아니라 목표 파이프라인 수 정수다, prefs.rs 필드 doc
// 참고)
// holds each seekable pipeline paused-prerolled (armed at position 0, in SEGMENT mode when
// gapless looping is also enabled) until N pipelines are ready, then starts them all on a
// shared system clock with an identical base time so they render in frame-level lockstep
// (the standard GStreamer multi-pipeline sync recipe). Combined with gapless looping the
// lockstep persists across loop boundaries, because the non-flushing SEGMENT rewinds
// preserve running-time continuity. A watchdog releases the group after 30s even if fewer
// than N pipelines arrived.
fn sync_group_target() -> Option<usize> {
    // 옛 env 는 문자열 파싱 후 LazyLock 으로 프로세스 수명 캐시했다 — pref 읽기는 이미
    // 가벼운 RwLock 클론이고 이 함수는 프레임 핫패스가 아니라(play()/arm 시점에만 호출)
    // 캐시를 유지할 이유가 없다.
    let target = pref!(media_sync_group_target);
    (target >= 2).then_some(target as usize)
}

struct SyncGroupMember {
    /// `None` for a pipeline that was not built by `Play` -- released by state change.
    play: Option<gstreamer_play::Play>,
    pipeline: gstreamer::Element,
}

struct SyncGroupState {
    members: Vec<SyncGroupMember>,
    released: bool,
    watchdog_started: bool,
}

static SYNC_GROUP: std::sync::LazyLock<Mutex<SyncGroupState>> =
    std::sync::LazyLock::new(|| {
        Mutex::new(SyncGroupState {
            members: Vec::new(),
            released: false,
            watchdog_started: false,
        })
    });

fn sync_group_released() -> bool {
    SYNC_GROUP.lock().unwrap().released
}

/// Start every member pipeline in lockstep: shared clock, disabled automatic base-time
/// adjustment, identical base time slightly in the future, then PLAYING for all.
fn release_sync_group(members: &[SyncGroupMember]) {
    let clock = gstreamer::SystemClock::obtain();
    let base = clock.time() + gstreamer::ClockTime::from_mseconds(500);
    for member in members {
        if let Ok(pipeline) = member.pipeline.clone().downcast::<gstreamer::Pipeline>() {
            pipeline.use_clock(Some(&clock));
        }
        member.pipeline.set_start_time(gstreamer::ClockTime::NONE);
        member.pipeline.set_base_time(base);
    }
    for member in members {
        match member.play.as_ref() {
            Some(play) => play.play(),
            None => {
                let _ = member.pipeline.set_state(gstreamer::State::Playing);
            },
        }
    }
    log::info!(
        "Sync group released: {} pipelines starting at shared base time",
        members.len()
    );
}

/// Register an armed pipeline; releases the whole group when the target count is reached.
fn register_sync_member(play: Option<gstreamer_play::Play>, pipeline: gstreamer::Element) {
    let Some(target) = sync_group_target() else {
        match play.as_ref() {
            Some(play) => play.play(),
            None => {
                let _ = pipeline.set_state(gstreamer::State::Playing);
            },
        }
        return;
    };
    let mut state = SYNC_GROUP.lock().unwrap();
    if state.released {
        drop(state);
        match play.as_ref() {
            Some(play) => play.play(),
            None => {
                let _ = pipeline.set_state(gstreamer::State::Playing);
            },
        }
        return;
    }
    state.members.push(SyncGroupMember { play, pipeline });
    log::info!("Sync group: {}/{} pipelines armed", state.members.len(), target);
    if !state.watchdog_started {
        state.watchdog_started = true;
        let _ = std::thread::Builder::new()
            .name(String::from("GstSyncGroupWatchdog"))
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(30));
                let mut state = SYNC_GROUP.lock().unwrap();
                if !state.released {
                    state.released = true;
                    log::warn!(
                        "Sync group timeout: releasing {} of {} pipelines",
                        state.members.len(),
                        target
                    );
                    release_sync_group(&state.members);
                }
            });
    }
    if state.members.len() >= target {
        state.released = true;
        release_sync_group(&state.members);
    }
}

impl PlayerInner {
    pub fn set_input_size(&mut self, size: u64) -> Result<(), PlayerError> {
        // Direct file mode: GStreamer reads the file itself, so the servosrc input size is
        // irrelevant. Accept and drop it so the element's flow is not disturbed.
        if self.direct_file.get() {
            return Ok(());
        }
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
        if let Some(player) = self.player.as_ref() {
            player.set_mute(muted);
        }
        let audio_disabled = !pref!(media_audio_enabled);
        let audio_track_enabled = !muted && !audio_disabled;
        // Mute via GstPlay's reversible controls only: the `mute` property plus audio-track
        // (de)selection. Do NOT swap the `audio-sink` element at runtime — playbin3 links
        // `audio-sink` at preroll, so a live restore->autoaudiosink swap fails to re-link the
        // audio branch, leaving audio dead after an unmute (mute becomes irreversible). The
        // construction-time media_audio_enabled=false fakesink (set before PLAYING) is
        // unaffected.
        if let Some(player) = self.player.as_ref() {
            player.set_audio_track_enabled(audio_track_enabled);
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
            if let Some(player) = self.player.as_ref() {
                player.set_rate(playback_rate);
            }
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
        // Synchronized group start (see `media_sync_group_target`): hold the pipeline
        // paused and prerolled; the sync group releases every member simultaneously on a
        // shared clock. Arming happens once metadata is known (`request_sync_group_arm`).
        if sync_group_target().is_some() &&
            self.stream_type == StreamType::Seekable &&
            !sync_group_released()
        {
            self.sync_hold.set(true);
            self.set_pipeline_paused();
            self.request_sync_group_arm();
            return Ok(());
        }
        self.set_pipeline_playing();
        Ok(())
    }

    /// Ask the worker thread to arm this pipeline for the synchronized group start.
    /// No-op until metadata is known (the pipeline must have prerolled real media).
    fn request_sync_group_arm(&self) {
        if !self.sync_hold.get() || self.sync_armed.get() || self.last_metadata.is_none() {
            return;
        }
        if let Some(ref sender) = self.gapless_loop_sender {
            let _ = sender.send(GaplessLoopMsg::ArmSyncGroup);
        }
    }

    pub fn stop(&mut self) -> Result<(), PlayerError> {
        match self.player.as_ref() {
            Some(player) => player.stop(),
            None => {
                let _ = self.pipeline.set_state(gstreamer::State::Null);
            },
        }
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
        // A real pause request cancels a pending synchronized start hold.
        self.sync_hold.set(false);
        self.set_pipeline_paused();
        Ok(())
    }

    /// Start playback, whoever owns the pipeline.
    fn set_pipeline_playing(&self) {
        match self.player.as_ref() {
            Some(player) => player.play(),
            None => {
                let _ = self.pipeline.set_state(gstreamer::State::Playing);
            },
        }
    }

    fn set_pipeline_paused(&self) {
        match self.player.as_ref() {
            Some(player) => player.pause(),
            None => {
                let _ = self.pipeline.set_state(gstreamer::State::Paused);
            },
        }
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
            gstreamer::warning!(self.cat, obj = &self.pipeline, "Trying to seek out of range");
            return Err(PlayerError::SeekOutOfRange);
        }

        let time = time * 1_000_000_000.;
        // A regular (flushing, non-SEGMENT) seek takes the pipeline out of segment-loop
        // mode; `connect_seek_done` re-enters it once the seek settles.
        self.segment_loop_active.set(false);
        let position = gstreamer::ClockTime::from_nseconds(time as u64);
        match self.player.as_ref() {
            Some(player) => player.seek(position),
            // Same flushing seek `Play::seek` performs, issued straight on the pipeline.
            None => {
                let _ = self.pipeline.seek_simple(
                    gstreamer::SeekFlags::FLUSH | gstreamer::SeekFlags::KEY_UNIT,
                    position,
                );
            },
        }
        Ok(())
    }

    pub fn set_looping(&mut self, looping: bool) -> Result<(), PlayerError> {
        if !pref!(media_gapless_loop_enabled) {
            return Ok(());
        }
        self.looping.set(looping);
        self.request_segment_loop_entry();
        Ok(())
    }

    /// Ask the gapless-loop worker thread to (re-)evaluate entering SEGMENT-seek mode.
    /// This only posts a message: the actual pipeline seek MUST NOT run on GstPlay signal
    /// dispatch threads or while the `PlayerInner` mutex is held (both can deadlock or
    /// stall the pipeline), so all seeking happens on the dedicated worker (see `setup`).
    fn request_segment_loop_entry(&self) {
        // Cheap pre-filter: this is also called from the frequent position-updated signal.
        if !self.looping.get() || self.segment_loop_active.get() {
            return;
        }
        if let Some(ref sender) = self.gapless_loop_sender {
            let _ = sender.send(GaplessLoopMsg::MaybeEnter);
        }
    }

    pub fn set_volume(&mut self, volume: f64) -> Result<(), PlayerError> {
        if self.volume.get() == volume {
            return Ok(());
        }

        self.volume.set(volume);
        if let Some(player) = self.player.as_ref() {
            player.set_volume(volume);
        }
        Ok(())
    }

    pub fn volume(&self) -> f64 {
        self.volume.get()
    }

    pub fn push_data(&mut self, data: Vec<u8>) -> Result<(), PlayerError> {
        // Direct file mode: there is no servosrc to feed; GStreamer reads the file itself.
        // The element still fetches and pushes bytes, so drop them and report success — an
        // error here would make the fetch listener treat the push as a failure and could
        // stall or abort the element's load flow.
        if self.direct_file.get() {
            drop(data);
            return Ok(());
        }
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            if self.enough_data.load(Ordering::Relaxed) && !enoughdata_backoff_disabled() {
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

        let pipeline = self.pipeline.clone();
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
            .pipeline
            .clone()
            .dynamic_cast::<gstreamer::Pipeline>()
            .map_err(|_| PlayerError::SetStreamFailed)?;
        let clock = gstreamer::SystemClock::obtain();
        playbin.set_base_time(*BACKEND_BASE_TIME);
        playbin.set_start_time(gstreamer::ClockTime::NONE);
        playbin.use_clock(Some(&clock));
        source
            .set_stream(stream, only_stream)
            .map_err(|_| PlayerError::SetStreamFailed)
    }

    fn set_audio_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        // Track selection is a `Play` feature; the direct pipeline has no equivalent yet.
        let Some(player) = self.player.as_ref() else {
            return Err(PlayerError::SetTrackFailed);
        };
        player
            .set_audio_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        player.set_audio_track_enabled(enabled);
        Ok(())
    }

    fn set_video_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        let Some(player) = self.player.as_ref() else {
            return Err(PlayerError::SetTrackFailed);
        };
        player
            .set_video_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        player.set_video_track_enabled(enabled);
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
    /// Media resource URL hint (see `Player::set_resource_url`), captured before `setup()`
    /// so the direct local-file path (see `media_direct_file_enabled`) can be chosen. `None`
    /// unless the element hinted a URL.
    resource_url: RefCell<Option<String>>,
    /// Script-set hint (see `Player::set_direct_file`) that direct local-file playback should
    /// be used regardless of `media_direct_file_enabled`. `resolve_direct_file_url` honors
    /// either signal.
    force_direct_file: Cell<bool>,
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
            resource_url: RefCell::new(None),
            force_direct_file: Cell::new(false),
        }
    }

    /// If direct local-file playback applies, return the `file://` URI to hand to playbin.
    /// Requires a `Seekable` stream, a `file` scheme, an existing target file, and EITHER the
    /// `media_direct_file_enabled` pref OR the script-set `force_direct_file` hint (see
    /// `set_direct_file`, used for non-standard containers). Otherwise `None` (the servosrc
    /// path is used). Logs the direct-mode entry, and a warning when a file:// resource is
    /// missing.
    fn resolve_direct_file_url(&self) -> Option<String> {
        if !(pref!(media_direct_file_enabled) || self.force_direct_file.get())
            || self.stream_type != StreamType::Seekable
        {
            return None;
        }
        let raw = self.resource_url.borrow().clone()?;
        let parsed = url::Url::parse(&raw).ok()?;
        if parsed.scheme() != "file" {
            return None;
        }
        match parsed.to_file_path() {
            Ok(path) if path.is_file() => {
                log::info!("direct file playback: {}", path.display());
                Some(raw)
            },
            _ => {
                log::warn!(
                    "media_direct_file_enabled: file not found for {raw}; falling back to servosrc"
                );
                None
            },
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
        if self.stream_type == StreamType::NetworkUri &&
            self.network_uri
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
                configure_software_decoder_threads(&element);
                configure_rtsp_elements(&element);
            }
            None
        });

        let prefer_native_video = !self.render.lock().unwrap().is_gl() &&
            !servo_config::opts::get().multiprocess &&
            !servo_config::opts::get().force_ipc;
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
                        crate::thread_name::tag_audio_streaming_thread();
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
        } else if !pref!(media_audio_enabled) {
            // reason 은 로그에 그대로 찍힌다 — pref 이름만 넘기면 `reason=media_audio_enabled`
            // 가 되어 오디오가 켜져서 sink 를 껐다는 뜻으로 읽힌다. 값까지 적는다.
            disable_pipeline_audio_sink(&pipeline, "media_audio_enabled=false")?;
        }

        let video_sink = self.render.lock().unwrap().setup_video_sink(&pipeline)?;

        // There's a known bug in gstreamer that may cause a wrong transition
        // to the ready state while setting the uri property:
        // https://cgit.freedesktop.org/gstreamer/gst-plugins-bad/commit/?id=afbbc3a97ec391c6a582f3c746965fdc3eb3e1f3
        // This may affect things like setting the config, so until the bug is
        // fixed, make sure that state dependent code happens before this line.
        // The estimated version for the fix is 1.14.5 / 1.15.1.
        // https://github.com/servo/servo/issues/22010#issuecomment-432599657
        // Direct local-file playback (see `media_direct_file_enabled`): when applicable,
        // point playbin at the file:// URL directly (its own filesrc) instead of servosrc.
        // `None` keeps the existing servosrc byte-push path.
        let direct_file_url = self.resolve_direct_file_url();
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
                if let Some(ref file_url) = direct_file_url {
                    // GStreamer reads the local file itself; no servosrc registration needed.
                    file_url.to_value()
                } else {
                    register_servo_src().map_err(|error| {
                        PlayerError::Backend(format!("servosrc registration error: {error:?}"))
                    })?;
                    "servosrc://".to_value()
                }
            },
            StreamType::NetworkUri => {
                // Let playbin3 own the source: it auto-plugs the right element
                // (e.g. rtspsrc) for the scheme. No servo AppSrc is registered.
                let uri = self.network_uri.clone().ok_or_else(|| {
                    PlayerError::Backend(
                        "NetworkUri stream type requires a network_uri".to_owned(),
                    )
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
            player: Some(player),
            _signal_adapter: Some(signal_adapter.clone()),
            pipeline: pipeline.clone(),
            source: None,
            video_sink,
            input_size: 0,
            seekable: true,
            play_state: gstreamer_play::PlayState::Stopped,
            paused: Cell::new(DEFAULT_PAUSED),
            can_resume: Cell::new(DEFAULT_CAN_RESUME),
            playback_rate: Cell::new(DEFAULT_PLAYBACK_RATE),
            muted: Cell::new(DEFAULT_MUTED),
            volume: Cell::new(DEFAULT_VOLUME),
            stream_type: self.stream_type,
            last_metadata: None,
            cat: gstreamer::DebugCategory::get("servoplayer").unwrap(),
            enough_data: Arc::new(AtomicBool::new(false)),
            looping: Cell::new(false),
            segment_loop_active: Cell::new(false),
            gapless_loop_sender: None,
            sync_hold: Cell::new(false),
            sync_armed: Cell::new(false),
            direct_file: Cell::new(direct_file_url.is_some()),
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
            {
                let mut inner = inner_clone.lock().unwrap();
                inner.play_state = play_state;
                if play_state == gstreamer_play::PlayState::Playing {
                    // Gapless looping arms itself once the pipeline is actually playing
                    // (no-op unless `set_looping(true)` was requested).
                    inner.request_segment_loop_entry();
                }
            }

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
        let inner_clone = inner.clone();
        signal_adapter.connect_position_updated(move |_, position| {
            // Gapless looping delays segment-mode entry until playback is well underway
            // (see the worker's position gate); this periodic signal retries the entry.
            inner_clone.lock().unwrap().request_segment_loop_entry();
            if let Some(seconds) = position.map(|p| p.seconds_f64()) {
                let _ = notify!(observer, PlayerEvent::PositionChanged(seconds));
            }
        });

        let observer = self.observer.clone();
        let inner_clone = inner.clone();
        // Handle `seek-done` signal.
        signal_adapter.connect_seek_done(move |_, position| {
            // A regular seek (e.g. the user dragging the scrubber) cancels segment-loop
            // mode; re-enter it once the seek has settled.
            inner_clone.lock().unwrap().request_segment_loop_entry();
            let _ = notify!(observer, PlayerEvent::SeekDone(position.seconds_f64()));
        });

        // Gapless looping (`media_gapless_loop_enabled`) and synchronized group start
        // (`media_sync_group_target`): all pipeline seeking happens on this dedicated
        // worker thread. Entering segment mode or rewinding from GstPlay signal threads /
        // bus callbacks (or while holding the `PlayerInner` mutex) deadlocks or stalls the
        // pipeline, so signal handlers only post messages here.
        if pref!(media_gapless_loop_enabled) || sync_group_target().is_some() {
            let pipeline = inner.lock().unwrap().pipeline.clone();
            if let Some(bus) = pipeline.bus() {
                let (loop_sender, loop_receiver) = mpsc::channel::<GaplessLoopMsg>();
                inner.lock().unwrap().gapless_loop_sender = Some(loop_sender.clone());
                let pipeline_weak = pipeline.downgrade();
                let inner_for_loop = inner.clone();
                std::thread::Builder::new()
                    .name(String::from("GstGaplessLoop"))
                    .spawn(move || {
                        let mut last_rewind: Option<std::time::Instant> = None;
                        while let Ok(message) = loop_receiver.recv() {
                            let Some(pipeline) = pipeline_weak.upgrade() else {
                                return;
                            };
                            match message {
                                GaplessLoopMsg::MaybeEnter => {
                                    {
                                        let inner = inner_for_loop.lock().unwrap();
                                        if !inner.looping.get() ||
                                            inner.segment_loop_active.get() ||
                                            inner.stream_type != StreamType::Seekable ||
                                            inner.play_state !=
                                                gstreamer_play::PlayState::Playing ||
                                            inner.last_metadata.is_none()
                                        {
                                            continue;
                                        }
                                    }
                                    // Enter only once playback is well underway. A segment
                                    // seek during startup (preroll still settling, e.g.
                                    // dozens of pipelines starting at once) can wedge the
                                    // pipeline before its first frame, leaving a dead
                                    // tile. The position-updated signal retries this
                                    // entry periodically, so skipping here is safe.
                                    let Some(position) =
                                        pipeline.query_position::<gstreamer::ClockTime>()
                                    else {
                                        continue;
                                    };
                                    if position < gstreamer::ClockTime::from_mseconds(500) {
                                        continue;
                                    }
                                    {
                                        // Re-check and claim the mode before seeking
                                        // (reverted on failure) so racing MaybeEnter
                                        // messages do not double-seek.
                                        let inner = inner_for_loop.lock().unwrap();
                                        if !inner.looping.get() || inner.segment_loop_active.get()
                                        {
                                            continue;
                                        }
                                        inner.segment_loop_active.set(true);
                                    }
                                    match pipeline.seek(
                                        1.0,
                                        gstreamer::SeekFlags::FLUSH |
                                            gstreamer::SeekFlags::SEGMENT |
                                            gstreamer::SeekFlags::ACCURATE,
                                        gstreamer::SeekType::Set,
                                        position,
                                        gstreamer::SeekType::None,
                                        gstreamer::ClockTime::NONE,
                                    ) {
                                        Ok(()) => {
                                            log::info!(
                                                "Gapless loop: entered segment mode at {position}"
                                            );
                                        },
                                        Err(error) => {
                                            log::warn!(
                                                "Gapless loop: segment mode entry failed: {error:?}"
                                            );
                                            inner_for_loop
                                                .lock()
                                                .unwrap()
                                                .segment_loop_active
                                                .set(false);
                                        },
                                    }
                                },
                                GaplessLoopMsg::ArmSyncGroup => {
                                    let play_handle = {
                                        let inner = inner_for_loop.lock().unwrap();
                                        if !inner.sync_hold.get() || inner.sync_armed.get() {
                                            continue;
                                        }
                                        inner.sync_armed.set(true);
                                        // When gapless looping is on, arm SEGMENT mode at
                                        // position 0 while still paused: the pipeline
                                        // re-prerolls at 0 in segment mode, so the
                                        // synchronized start needs no later flushing seek
                                        // (which would break lockstep).
                                        if pref!(media_gapless_loop_enabled) {
                                            inner.segment_loop_active.set(true);
                                        }
                                        inner.player.clone()
                                        // `None` for the direct pipeline; the group
                                        // then releases it by state change instead.
                                    };
                                    if pref!(media_gapless_loop_enabled) {
                                        if let Err(error) = pipeline.seek(
                                            1.0,
                                            gstreamer::SeekFlags::FLUSH |
                                                gstreamer::SeekFlags::SEGMENT |
                                                gstreamer::SeekFlags::ACCURATE,
                                            gstreamer::SeekType::Set,
                                            gstreamer::ClockTime::ZERO,
                                            gstreamer::SeekType::None,
                                            gstreamer::ClockTime::NONE,
                                        ) {
                                            log::warn!(
                                                "Sync group: segment arm seek failed: {error:?}"
                                            );
                                            inner_for_loop
                                                .lock()
                                                .unwrap()
                                                .segment_loop_active
                                                .set(false);
                                        }
                                    }
                                    register_sync_member(play_handle, pipeline.clone());
                                },
                                GaplessLoopMsg::SegmentDone => {
                                    let looping = {
                                        let inner = inner_for_loop.lock().unwrap();
                                        inner.looping.get() && inner.segment_loop_active.get()
                                    };
                                    if !looping {
                                        continue;
                                    }
                                    // Storm guard: a SEGMENT_DONE right after the previous
                                    // rewind means the segment finished without playing
                                    // real data; leave segment mode instead of rewinding
                                    // in a tight loop.
                                    if let Some(previous) = last_rewind &&
                                        previous.elapsed() <
                                            std::time::Duration::from_millis(1000)
                                    {
                                        log::warn!(
                                            "Gapless loop: rewind storm detected; leaving segment mode"
                                        );
                                        inner_for_loop
                                            .lock()
                                            .unwrap()
                                            .segment_loop_active
                                            .set(false);
                                        continue;
                                    }
                                    last_rewind = Some(std::time::Instant::now());
                                    // No FLUSH flag: decoders keep their state and the
                                    // pipeline wraps to the start without a stall or EOS.
                                    if let Err(error) = pipeline.seek(
                                        1.0,
                                        gstreamer::SeekFlags::SEGMENT,
                                        gstreamer::SeekType::Set,
                                        gstreamer::ClockTime::ZERO,
                                        gstreamer::SeekType::None,
                                        gstreamer::ClockTime::NONE,
                                    ) {
                                        log::warn!(
                                            "Gapless loop: rewind seek failed: {error:?}"
                                        );
                                    }
                                },
                            }
                        }
                    })
                    .expect("Could not create GstGaplessLoop thread.");
                bus.enable_sync_message_emission();
                // The callback must be Sync; mpsc senders are not, so guard with a mutex.
                let loop_sender = Mutex::new(loop_sender);
                bus.connect_sync_message(Some("segment-done"), move |_, _| {
                    if let Ok(sender) = loop_sender.lock() {
                        let _ = sender.send(GaplessLoopMsg::SegmentDone);
                    }
                });
            }
        }

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
                    if let Some(player) = inner.player.as_ref() {
                        player.set_rate(inner.playback_rate.get());
                    }
                } else if inner.play_state == gstreamer_play::PlayState::Paused {
                    send_pause_event = true;
                }
            }

            inner.last_metadata = Some(metadata.clone());
            let audio_disabled = !pref!(media_audio_enabled);
            let audio_track_enabled = !inner.muted.get() && !audio_disabled;
            // Apply the initial mute state via audio-track selection only — no runtime
            // audio-sink swap (see set_mute: a live sink swap is irreversible on playbin3).
            if let Some(player) = inner.player.as_ref() {
                player.set_audio_track_enabled(audio_track_enabled);
            }
            gstreamer::info!(
                inner.cat,
                obj = &inner.pipeline,
                "Metadata updated: {:?}",
                metadata
            );
            // Gapless looping waits for prerolled media; metadata arrival may be the
            // last missing condition. Same for arming a synchronized group start.
            inner.request_segment_loop_entry();
            inner.request_sync_group_arm();
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
                    obj = &inner.pipeline,
                    "Duration changed: {:?}",
                    duration
                );
                let _ = notify!(observer, PlayerEvent::DurationChanged(duration));
            }
        });

        if let Some(video_renderer) = self.video_renderer.clone() {
            let sample_diagnostics = Arc::new(Mutex::new(VideoSampleDiagnostics::default()));
            // `media_video_sink_pacing=thread` 에서만 쓴다. 모드는 한 번만 읽는다 —
            // 프레임마다 pref 를 읽으면 이 경로가 줄이려는 비용에 자기 자신이 섞인다.
            let sink_pacing = crate::render::VideoSinkPacing::from_pref();
            let sink_pacer = Arc::new(Mutex::new(SinkPacer::default()));
            // Creates a closure that renders a frame using the video_renderer
            // Used in the preroll and sample callbacks
            let render_sample = {
                let render = self.render.clone();
                let observer = self.observer.clone();
                let sample_diagnostics = sample_diagnostics.clone();
                let sink_pacer = sink_pacer.clone();
                let weak_video_renderer = Arc::downgrade(&video_renderer);

                move |sample: gstreamer::Sample| {
                    // This closure runs on the video sink's streaming thread --
                    // a GLib thread with no OS name of its own. Latched inside.
                    crate::thread_name::tag_video_streaming_thread();
                    // 싱크가 클럭을 기다리지 않으므로 재생 속도를 여기서 지킨다.
                    // 잠은 락 밖에서 잔다.
                    if sink_pacing == crate::render::VideoSinkPacing::Thread {
                        let sleep = sink_pacer
                            .lock()
                            .unwrap()
                            .sleep_before(sample.buffer().and_then(|buffer| buffer.pts()));
                        if let Some(sleep) = sleep {
                            std::thread::sleep(sleep);
                        }
                    }
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
            let pipeline = inner.pipeline.clone();

            let (sender, receiver) = mpsc::channel();

            let sender = Arc::new(Mutex::new(sender));
            let sender_clone = sender.clone();
            let is_ready_clone = self.is_ready.clone();
            let observer = self.observer.clone();
            pipeline.connect("source-setup", false, move |args| {
                let source = args[1].get::<gstreamer::Element>().unwrap();

                let mut inner = inner_clone.lock().unwrap();

                // Direct file mode: playbin instantiated its own filesrc, not a ServoSrc.
                // There is nothing to wire up; just release setup()'s readiness gate (as the
                // Stream branch does) and leave `inner.source` as None so push_data and
                // set_input_size become harmless no-ops.
                if inner.direct_file.get() {
                    let sender_clone = sender.clone();
                    is_ready_clone.call_once(|| {
                        let _ = notify!(sender_clone, Ok(()));
                    });
                    return None;
                }

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

            inner.set_pipeline_paused();

            (receiver, error_handler_id)
        };

        let result = receiver.recv().unwrap();
        // ★Disconnect from the object the handler was connected to★ — `error_handler_id`
        // came from `signal_adapter.connect_error(...)`, and a `PlaySignalAdapter` is a
        // different GObject from the `Play` it adapts. Disconnecting it from `player` did
        // nothing but emit
        //     GLib-GObject-CRITICAL: instance '0x...' has no handler with id 'N'
        // once per player created, and left this setup-time handler connected for the life
        // of the pipeline. That matters: its closure calls `signal_adapter.play().stop()`,
        // so every later error permanently stopped playback instead of letting the element
        // recover — turning any transient error (an RTSP stream is full of them) into a
        // dead video.
        if let Some(adapter) = inner.lock().unwrap()._signal_adapter.as_ref() {
            glib::signal::signal_handler_disconnect(adapter, error_handler_id);
        }
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
    inner_player_proxy!(set_looping, looping, bool);
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

    fn set_resource_url(&self, url: &str) {
        // Store the hint for `setup()` to consider (see `resolve_direct_file_url`). Must be
        // called before the first proxy call triggers `setup()`.
        *self.resource_url.borrow_mut() = Some(url.to_owned());
    }

    fn set_direct_file(&self, direct: bool) {
        // Script-set preference for direct local-file playback (see `resolve_direct_file_url`).
        // Must be set before the first proxy call triggers `setup()`.
        self.force_direct_file.set(direct);
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

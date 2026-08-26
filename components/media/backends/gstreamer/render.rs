/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;

use glib::prelude::*;
use gstreamer_video::prelude::*;
use servo_config::pref;
use servo_media_gstreamer_render::Render;
use servo_media_player::PlayerError;
use servo_media_player::context::PlayerGLContext;
use servo_media_player::video::{
    Buffer, VideoFrame, VideoFrameData, VideoFramePlane, VideoFrameYuvColorRange,
    VideoFrameYuvColorSpace, VideoFrameYuvData, VideoFrameYuvFormat,
};

const LOW_LATENCY_VIDEO_MAX_BUFFERS: u32 = 1;
const LOW_LATENCY_VIDEO_MAX_LATENESS_NS: i64 = 16_000_000;
const SMOOTH_VIDEO_MAX_BUFFERS: u32 = 3;
const DISABLED_VIDEO_MAX_LATENESS_NS: i64 = -1;
const VIDEO_SINK_PROCESSING_DEADLINE_NS: u64 = 0;

/// 비디오 싱크의 페이싱 방식(`media_video_sink_pacing`).
///
/// `Clock` 은 GstBaseSink 기본 동작(`sync=true`)이고, `Thread` 는 싱크의 클럭 대기를
/// 끄고 스트리밍 스레드가 PTS 앵커로 직접 잔다. 후자가 존재하는 이유는
/// `GstSystemClock` 이 프로세스당 싱글턴이라 파이프라인 45 개가 프레임마다 같은 객체에서
/// 대기하기 때문이다 — 실측치는 `media_video_sink_pacing` 문서 참조.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoSinkPacing {
    Clock,
    Thread,
    /// 진단 전용: 싱크의 클럭 대기를 끄고 **아무 페이싱도 하지 않는다**.
    ///
    /// 재생 속도는 무의미해지지만(디코더가 전속력으로 돈다) ★프레임당 CPU 는 유효하다★.
    /// `Clock` 과 비교하면 **공유 GstSystemClock 대기만의 비용**이, `Thread` 와 비교하면
    /// **재우는 방식 자체의 비용**이 나온다. 둘이 섞여 있어 `Thread` 가 왜 손해인지 가릴
    /// 수 없었기 때문에 필요하다 — 실측에서 `Thread` 는 CPU 가 줄지 않고 처리량이 절반이
    /// 되어 프레임당 비용이 두 배가 됐다(34ms -> 65ms).
    None,
}

impl VideoSinkPacing {
    pub fn from_pref() -> Self {
        let value = pref!(media_video_sink_pacing);
        if value.eq_ignore_ascii_case("thread") {
            Self::Thread
        } else if value.eq_ignore_ascii_case("none") {
            Self::None
        } else {
            if !value.is_empty() && !value.eq_ignore_ascii_case("clock") {
                log::warn!(
                    "Ignoring invalid media_video_sink_pacing={value:?};                      expected clock, thread or none"
                );
            }
            Self::Clock
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Clock => "clock",
            Self::Thread => "thread",
            Self::None => "none",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VideoSinkPolicy {
    Smooth,
    LowLatency,
}

impl VideoSinkPolicy {
    /// `media_video_sink_policy` pref(구 env `SERVO_VIDEO_SINK_POLICY`)를 판정한다. **빈
    /// 문자열 = Smooth** — 이 파일의 다른 String pref 와 같은 "빈 문자열 = 관례상 기본값"
    /// 규약이고, 구 env 의 `Err(_) => Smooth` 와 동일한 동작이다(경고 없음). 비어있지
    /// 않은데 인정 토큰이 아니면 옛 동작처럼 경고 후 Smooth 로 폴백한다.
    fn from_pref() -> Self {
        let value = pref!(media_video_sink_policy);
        if value.is_empty() {
            return Self::Smooth;
        }
        if value.eq_ignore_ascii_case("low-latency")
            || value.eq_ignore_ascii_case("low_latency")
            || value.eq_ignore_ascii_case("latency")
        {
            Self::LowLatency
        } else if value.eq_ignore_ascii_case("smooth") || value.eq_ignore_ascii_case("complete") {
            Self::Smooth
        } else {
            log::warn!(
                "Ignoring invalid media_video_sink_policy={value:?}; \
                 expected smooth or low-latency"
            );
            Self::Smooth
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Smooth => "smooth",
            Self::LowLatency => "low-latency",
        }
    }

    fn max_buffers(self) -> u32 {
        match self {
            Self::Smooth => SMOOTH_VIDEO_MAX_BUFFERS,
            Self::LowLatency => LOW_LATENCY_VIDEO_MAX_BUFFERS,
        }
    }

    fn drop_late(self) -> bool {
        self == Self::LowLatency
    }

    fn qos(self) -> bool {
        self == Self::LowLatency
    }

    /// 정책이 정한 qos 에 `media_video_sink_qos` 오버라이드를 적용한다.
    ///
    /// 정책 하나가 qos/drop/max-lateness/max-buffers 를 함께 바꾸므로, qos 만 재려면
    /// 나머지 셋을 고정한 채 이것만 뒤집을 수 있어야 한다.
    fn effective_qos(self) -> bool {
        let value = pref!(media_video_sink_qos);
        if value.is_empty() {
            return self.qos();
        }
        if value.eq_ignore_ascii_case("on") || value.eq_ignore_ascii_case("true") || value == "1" {
            true
        } else if value.eq_ignore_ascii_case("off")
            || value.eq_ignore_ascii_case("false")
            || value == "0"
        {
            false
        } else {
            log::warn!("Ignoring invalid media_video_sink_qos={value:?}; expected on or off");
            self.qos()
        }
    }

    fn max_lateness_ns(self) -> i64 {
        match self {
            Self::Smooth => DISABLED_VIDEO_MAX_LATENESS_NS,
            Self::LowLatency => LOW_LATENCY_VIDEO_MAX_LATENESS_NS,
        }
    }
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
mod platform {
    extern crate servo_media_gstreamer_render_unix;
    pub use self::servo_media_gstreamer_render_unix::RenderUnix as Render;
    use super::*;

    pub fn create_render(gl_context: Box<dyn PlayerGLContext>) -> Option<Render> {
        Render::new(gl_context)
    }
}

#[cfg(target_os = "android")]
mod platform {
    extern crate servo_media_gstreamer_render_android;
    pub use self::servo_media_gstreamer_render_android::RenderAndroid as Render;
    use super::*;

    pub fn create_render(gl_context: Box<dyn PlayerGLContext>) -> Option<Render> {
        Render::new(gl_context)
    }
}

#[cfg(target_os = "windows")]
mod platform {
    extern crate servo_media_gstreamer_render_d3d11;
    pub use self::servo_media_gstreamer_render_d3d11::RenderD3D11 as Render;
    use super::*;

    // media_d3d11_enabled pref 게이트 + 사전 점검은 RenderD3D11::new 내부.
    // None이면 기존 CPU(I420 borrowed) 경로가 그대로 쓰인다.
    pub fn create_render(_gl_context: Box<dyn PlayerGLContext>) -> Option<Render> {
        Render::new()
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "android",
    target_os = "windows",
)))]
mod platform {
    use servo_media_gstreamer_render::Render as RenderTrait;
    use servo_media_player::PlayerError;
    use servo_media_player::context::PlayerGLContext;
    use servo_media_player::video::VideoFrame;

    pub struct RenderDummy();
    pub type Render = RenderDummy;

    pub fn create_render(_: Box<dyn PlayerGLContext>) -> Option<RenderDummy> {
        None
    }

    impl RenderTrait for RenderDummy {
        fn is_gl(&self) -> bool {
            false
        }

        fn build_frame(&self, _: gstreamer::Sample) -> Option<VideoFrame> {
            None
        }

        fn build_video_sink(
            &self,
            _: &gstreamer::Element,
            _: &gstreamer::Element,
        ) -> Result<(), PlayerError> {
            Err(PlayerError::Backend(
                "Not available videosink decorator".to_owned(),
            ))
        }
    }
}

struct GStreamerBuffer {
    frame: gstreamer_video::VideoFrame<gstreamer_video::video_frame::Readable>,
}

impl Buffer for GStreamerBuffer {
    fn frame_data(&self) -> Option<VideoFrameData> {
        match self.frame.format() {
            gstreamer_video::VideoFormat::I420 => Some(VideoFrameData::Yuv(
                self.yuv_data(VideoFrameYuvFormat::I420)?,
            )),
            gstreamer_video::VideoFormat::Nv12 => Some(VideoFrameData::Yuv(
                self.yuv_data(VideoFrameYuvFormat::NV12)?,
            )),
            _ => Some(VideoFrameData::Raw(self.raw_bgra_copy()?)),
        }
    }

    fn plane_data(&self, plane_index: usize) -> Option<&[u8]> {
        self.frame.plane_data(plane_index as u32).ok()
    }
}

impl GStreamerBuffer {
    fn raw_bgra_copy(&self) -> Option<Arc<Vec<u8>>> {
        let data = self.frame.plane_data(0).ok()?;
        Some(Arc::new(data.to_vec()))
    }

    fn yuv_data(&self, format: VideoFrameYuvFormat) -> Option<VideoFrameYuvData> {
        let info = self.frame.info();
        let mut planes = [None; 3];

        match format {
            VideoFrameYuvFormat::I420 => {
                planes[0] = Some(VideoFramePlane {
                    width: info.comp_width(0) as i32,
                    height: info.comp_height(0) as i32,
                    stride: info.comp_stride(0),
                });
                planes[1] = Some(VideoFramePlane {
                    width: info.comp_width(1) as i32,
                    height: info.comp_height(1) as i32,
                    stride: info.comp_stride(1),
                });
                planes[2] = Some(VideoFramePlane {
                    width: info.comp_width(2) as i32,
                    height: info.comp_height(2) as i32,
                    stride: info.comp_stride(2),
                });
            },
            VideoFrameYuvFormat::NV12 => {
                planes[0] = Some(VideoFramePlane {
                    width: info.comp_width(0) as i32,
                    height: info.comp_height(0) as i32,
                    stride: info.comp_stride(0),
                });
                planes[1] = Some(VideoFramePlane {
                    width: info.comp_width(1) as i32,
                    height: info.comp_height(1) as i32,
                    stride: info.comp_stride(1),
                });
            },
            // The raw CPU (Yuv) path only ever produces 8-bit I420/NV12 (see
            // `frame_data` above); 10-bit formats flow exclusively through the
            // D3D11 plane-ring path. Handle them gracefully rather than panic.
            VideoFrameYuvFormat::I420_10 | VideoFrameYuvFormat::P010 => return None,
        }

        Some(VideoFrameYuvData {
            format,
            planes,
            color_space: match info.colorimetry().matrix() {
                gstreamer_video::VideoColorMatrix::Bt709 => VideoFrameYuvColorSpace::Rec709,
                gstreamer_video::VideoColorMatrix::Bt2020 => VideoFrameYuvColorSpace::Rec2020,
                _ => VideoFrameYuvColorSpace::Rec601,
            },
            color_range: match info.colorimetry().range() {
                gstreamer_video::VideoColorRange::Range0_255 => VideoFrameYuvColorRange::Full,
                _ => VideoFrameYuvColorRange::Limited,
            },
        })
    }
}

pub struct GStreamerRender {
    render: Option<platform::Render>,
}

impl GStreamerRender {
    pub fn new(gl_context: Box<dyn PlayerGLContext>) -> Self {
        GStreamerRender {
            render: platform::create_render(gl_context),
        }
    }

    pub fn is_gl(&self) -> bool {
        if let Some(render) = self.render.as_ref() {
            render.is_gl()
        } else {
            false
        }
    }

    pub fn get_frame_from_sample(&self, sample: gstreamer::Sample) -> Option<VideoFrame> {
        if let Some(render) = self.render.as_ref() {
            render.build_frame(sample)
        } else {
            let buffer = sample.buffer_owned()?;
            let caps = sample.caps()?;
            let info = gstreamer_video::VideoInfo::from_caps(caps).ok()?;
            let frame = gstreamer_video::VideoFrame::from_buffer_readable(buffer, &info).ok()?;

            VideoFrame::new(
                info.width() as i32,
                info.height() as i32,
                Arc::new(GStreamerBuffer { frame }),
            )
        }
    }

    /// appsink 을 만들고 정책만 적용한 뒤, **파이프라인에 붙이지 않고** 돌려준다.
    ///
    /// `media_pipeline_mode=uridecodebin3` 는 playbin 의 `video-sink` 속성이 아니라 손으로
    /// 패드를 링크하므로 부착 단계가 다르다. 정책 설정(qos/drop/max-lateness/페이싱)은 두
    /// 경로가 반드시 같아야 해서 여기 한 곳에서만 한다.
    ///
    /// 렌더러가 appsink 자체가 아닌 다른 것을 붙이는 경우(unix 의 `glsinkbin`)에는
    /// `detached_video_sink_caps()` 가 `None` 이고, 이 함수도 그 사실을 그대로 돌려준다.
    pub fn create_detached_video_sink(
        &self,
    ) -> Result<Option<gstreamer_app::AppSink>, PlayerError> {
        let Some(caps) = self
            .render
            .as_ref()
            .and_then(|render| render.detached_video_sink_caps())
        else {
            return Ok(None);
        };
        let appsink = self.new_configured_appsink()?;
        appsink.set_property("caps", &caps);
        Ok(Some(appsink))
    }

    pub fn setup_video_sink(
        &self,
        pipeline: &gstreamer::Element,
    ) -> Result<gstreamer_app::AppSink, PlayerError> {
        let appsink = self.new_configured_appsink()?;

        if let Some(render) = self.render.as_ref() {
            render.build_video_sink(appsink.upcast_ref::<gstreamer::Element>(), pipeline)?
        } else {
            let use_borrowed_yuv =
                !servo_config::opts::get().multiprocess && !servo_config::opts::get().force_ipc;
            let caps = if use_borrowed_yuv {
                gstreamer_video::VideoCapsBuilder::new()
                    .format(gstreamer_video::VideoFormat::I420)
                    .pixel_aspect_ratio(gstreamer::Fraction::from((1, 1)))
                    .build()
            } else {
                gstreamer_video::VideoCapsBuilder::new()
                    .format(gstreamer_video::VideoFormat::Bgra)
                    .pixel_aspect_ratio(gstreamer::Fraction::from((1, 1)))
                    .build()
            };

            appsink.set_caps(Some(&caps));
            pipeline.set_property("video-sink", &appsink);
        };

        Ok(appsink)
    }

    fn new_configured_appsink(&self) -> Result<gstreamer_app::AppSink, PlayerError> {
        let appsink = gstreamer::ElementFactory::make("appsink")
            .build()
            .map_err(|error| PlayerError::Backend(format!("appsink creation failed: {error:?}")))?
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        let policy = VideoSinkPolicy::from_pref();
        appsink.set_max_buffers(policy.max_buffers());
        appsink.set_drop(policy.drop_late());
        appsink.set_property("qos", policy.effective_qos());
        appsink.set_property("max-lateness", policy.max_lateness_ns());
        appsink.set_property("processing-deadline", VIDEO_SINK_PROCESSING_DEADLINE_NS);
        appsink.set_property("enable-last-sample", false);
        // sync/async 는 이 포크가 건드리지 않으므로 GstBaseSink 기본값(둘 다 true)이어야
        // 한다. 굳이 읽어서 찍는 이유: sync 가 꺼지면 싱크가 클럭을 기다리지 않고,
        // 디코더가 재생 속도와 무관하게 전속력으로 돌아 영상당 CPU 가 몇 배로 뛴다.
        // 그게 실제로 일어나고 있는지 로그만으로 판정할 수 있어야 한다 — 세팅한 값이
        // 아니라 읽어 온 값을 찍는 것이 요점이다.
        // `thread` 페이싱에서는 싱크가 클럭을 기다리지 않는다. 대신 스트리밍
        // 스레드가 PTS 앵커에 맞춰 직접 자므로(player.rs 의 SinkPacer) 재생 속도는
        // 그대로이고, 프로세스당 싱글턴인 GstSystemClock 에서의 경합만 사라진다.
        let pacing = VideoSinkPacing::from_pref();
        // Clock 이 아닌 모드는 싱크가 클럭을 기다리지 않는다.
        if pacing != VideoSinkPacing::Clock {
            appsink.set_property("sync", false);
        }
        let sink_sync = appsink.property::<bool>("sync");
        let sink_async = appsink.property::<bool>("async");
        log::info!(
            "GStreamer video sink policy: policy={} max_buffers={} drop={} qos={} \
             max_lateness_ns={} processing_deadline_ns={} enable_last_sample=false \
             sync={sink_sync} async={sink_async} pacing={}",
            policy.as_str(),
            policy.max_buffers(),
            policy.drop_late(),
            policy.effective_qos(),
            policy.max_lateness_ns(),
            VIDEO_SINK_PROCESSING_DEADLINE_NS,
            pacing.as_str(),
        );

        Ok(appsink)
    }
}

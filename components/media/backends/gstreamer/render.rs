/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::env;
use std::sync::Arc;

use glib::prelude::*;
use gstreamer_video::prelude::*;
use servo_media_gstreamer_render::Render;
use servo_media_player::PlayerError;
use servo_media_player::context::PlayerGLContext;
use servo_media_player::video::{
    Buffer, VideoFrame, VideoFrameData, VideoFramePlane, VideoFrameYuvColorRange,
    VideoFrameYuvColorSpace, VideoFrameYuvData, VideoFrameYuvFormat,
};

const VIDEO_SINK_POLICY_ENV: &str = "SERVO_VIDEO_SINK_POLICY";
const LOW_LATENCY_VIDEO_MAX_BUFFERS: u32 = 1;
const LOW_LATENCY_VIDEO_MAX_LATENESS_NS: i64 = 16_000_000;
const SMOOTH_VIDEO_MAX_BUFFERS: u32 = 3;
const DISABLED_VIDEO_MAX_LATENESS_NS: i64 = -1;
const VIDEO_SINK_PROCESSING_DEADLINE_NS: u64 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VideoSinkPolicy {
    Smooth,
    LowLatency,
}

impl VideoSinkPolicy {
    fn from_environment() -> Self {
        match env::var(VIDEO_SINK_POLICY_ENV) {
            Ok(value)
                if value.eq_ignore_ascii_case("low-latency")
                    || value.eq_ignore_ascii_case("low_latency")
                    || value.eq_ignore_ascii_case("latency") =>
            {
                Self::LowLatency
            },
            Ok(value)
                if value.eq_ignore_ascii_case("smooth")
                    || value.eq_ignore_ascii_case("complete") =>
            {
                Self::Smooth
            },
            Ok(value) => {
                log::warn!(
                    "Ignoring invalid {VIDEO_SINK_POLICY_ENV}={value:?}; \
                     expected smooth or low-latency"
                );
                Self::Smooth
            },
            Err(_) => Self::Smooth,
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

    // env SERVO_MEDIA_D3D11_VIDEO 게이트 + 사전 점검은 RenderD3D11::new 내부.
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

    pub fn setup_video_sink(
        &self,
        pipeline: &gstreamer::Element,
    ) -> Result<gstreamer_app::AppSink, PlayerError> {
        let appsink = gstreamer::ElementFactory::make("appsink")
            .build()
            .map_err(|error| PlayerError::Backend(format!("appsink creation failed: {error:?}")))?
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        let policy = VideoSinkPolicy::from_environment();
        appsink.set_max_buffers(policy.max_buffers());
        appsink.set_drop(policy.drop_late());
        appsink.set_property("qos", policy.qos());
        appsink.set_property("max-lateness", policy.max_lateness_ns());
        appsink.set_property("processing-deadline", VIDEO_SINK_PROCESSING_DEADLINE_NS);
        appsink.set_property("enable-last-sample", false);
        log::info!(
            "GStreamer video sink policy: policy={} max_buffers={} drop={} qos={} \
             max_lateness_ns={} processing_deadline_ns={} enable_last_sample=false",
            policy.as_str(),
            policy.max_buffers(),
            policy.drop_late(),
            policy.qos(),
            policy.max_lateness_ns(),
            VIDEO_SINK_PROCESSING_DEADLINE_NS,
        );

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
}

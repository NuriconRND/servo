/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

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

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "android",
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
    fn to_vec(&self) -> Option<VideoFrameData> {
        match self.frame.format() {
            gstreamer_video::VideoFormat::I420 => Some(VideoFrameData::Yuv(
                self.yuv_data(VideoFrameYuvFormat::I420)?,
            )),
            gstreamer_video::VideoFormat::Nv12 => Some(VideoFrameData::Yuv(
                self.yuv_data(VideoFrameYuvFormat::NV12)?,
            )),
            _ => {
                let data = self.frame.plane_data(0).ok()?;
                Some(VideoFrameData::Raw(Arc::new(data.to_vec())))
            },
        }
    }

    fn plane_data(&self, plane_index: usize) -> Option<&[u8]> {
        self.frame.plane_data(plane_index as u32).ok()
    }
}

impl GStreamerBuffer {
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

        if let Some(render) = self.render.as_ref() {
            render.build_video_sink(appsink.upcast_ref::<gstreamer::Element>(), pipeline)?
        } else {
            let use_borrowed_yuv =
                !servo_config::opts::get().multiprocess && !servo_config::opts::get().force_ipc;
            let caps = if use_borrowed_yuv {
                gstreamer_video::VideoCapsBuilder::new()
                    .format_list([
                        gstreamer_video::VideoFormat::I420,
                        gstreamer_video::VideoFormat::Nv12,
                        gstreamer_video::VideoFormat::Bgra,
                    ])
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

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use gstreamer;
use gstreamer::caps::NoFeature;
use gstreamer::prelude::*;
use servo_media_streams::MediaStreamType;
use servo_media_streams::capture::*;
use servo_media_streams::registry::MediaStreamId;

use crate::media_stream::GStreamerMediaStream;

trait AddToCaps {
    type Bound;
    fn add_to_caps(
        &self,
        name: &str,
        min: Self::Bound,
        max: Self::Bound,
        builder: gstreamer::caps::Builder<NoFeature>,
    ) -> Option<gstreamer::caps::Builder<NoFeature>>;
}

impl AddToCaps for Constrain<u32> {
    type Bound = u32;
    fn add_to_caps(
        &self,
        name: &str,
        min: u32,
        max: u32,
        builder: gstreamer::caps::Builder<NoFeature>,
    ) -> Option<gstreamer::caps::Builder<NoFeature>> {
        match self {
            Constrain::Value(v) => Some(builder.field(name, v)),
            Constrain::Range(r) => {
                let min = into_i32(r.min.unwrap_or(min));
                let max = into_i32(r.max.unwrap_or(max));
                let range = gstreamer::IntRange::<i32>::new(min, max);

                // TODO: Include the ideal caps value in the caps, needs a refactor
                //       of the AddToCaps trait
                Some(builder.field(name, range))
            },
        }
    }
}

fn into_i32(x: u32) -> i32 {
    if x > i32::MAX as u32 {
        i32::MAX
    } else {
        x as i32
    }
}

impl AddToCaps for Constrain<f64> {
    type Bound = i32;
    fn add_to_caps<'a>(
        &self,
        name: &str,
        min: i32,
        max: i32,
        builder: gstreamer::caps::Builder<NoFeature>,
    ) -> Option<gstreamer::caps::Builder<NoFeature>> {
        match self {
            Constrain::Value(v) => {
                Some(builder.field("name", gstreamer::Fraction::approximate_f64(*v)?))
            },
            Constrain::Range(r) => {
                let min = r
                    .min
                    .and_then(gstreamer::Fraction::approximate_f64)
                    .unwrap_or(gstreamer::Fraction::new(min, 1));
                let max = r
                    .max
                    .and_then(gstreamer::Fraction::approximate_f64)
                    .unwrap_or(gstreamer::Fraction::new(max, 1));
                let range = gstreamer::FractionRange::new(min, max);
                // TODO: Include the ideal caps value in the caps, needs a refactor
                //       of the AddToCaps trait
                Some(builder.field(name, range))
            },
        }
    }
}

// TODO(Manishearth): Should support a set of constraints
fn into_caps(set: MediaTrackConstraintSet, format: &str) -> Option<gstreamer::Caps> {
    let mut builder = gstreamer::Caps::builder(format);
    if let Some(w) = set.width {
        builder = w.add_to_caps("width", 0, 1000000, builder)?;
    }
    if let Some(h) = set.height {
        builder = h.add_to_caps("height", 0, 1000000, builder)?;
    }
    if let Some(aspect) = set.aspect {
        builder = aspect.add_to_caps("pixel-aspect-ratio", 0, 1000000, builder)?;
    }
    if let Some(fr) = set.frame_rate {
        builder = fr.add_to_caps("framerate", 0, 1000000, builder)?;
    }
    if let Some(sr) = set.sample_rate {
        builder = sr.add_to_caps("rate", 0, 1000000, builder)?;
    }
    Some(builder.build())
}

struct GstMediaDevices {
    monitor: gstreamer::DeviceMonitor,
}

impl GstMediaDevices {
    pub fn new() -> Self {
        Self {
            monitor: gstreamer::DeviceMonitor::new(),
        }
    }

    pub fn get_track(
        &self,
        video: bool,
        constraints: MediaTrackConstraintSet,
    ) -> Option<GstMediaTrack> {
        let (format, filter) = if video {
            ("video/x-raw", "Video/Source")
        } else {
            ("audio/x-raw", "Audio/Source")
        };
        let caps = into_caps(constraints, format)?;
        let f = self.monitor.add_filter(Some(filter), Some(&caps));
        let devices = self.monitor.devices();
        if let Some(f) = f {
            let _ = self.monitor.remove_filter(f);
        }
        match devices.front() {
            Some(d) => {
                let element = d.create_element(None).ok()?;
                Some(GstMediaTrack { element })
            },
            _ => None,
        }
    }
}

pub struct GstMediaTrack {
    element: gstreamer::Element,
}

fn create_input_stream(
    stream_type: MediaStreamType,
    constraint_set: MediaTrackConstraintSet,
) -> Option<MediaStreamId> {
    let devices = GstMediaDevices::new();
    devices
        .get_track(stream_type == MediaStreamType::Video, constraint_set)
        .map(|track| {
            let f = match stream_type {
                MediaStreamType::Audio => GStreamerMediaStream::create_audio_from,
                MediaStreamType::Video => GStreamerMediaStream::create_video_from,
            };
            f(track.element)
        })
}

pub fn create_audioinput_stream(constraint_set: MediaTrackConstraintSet) -> Option<MediaStreamId> {
    create_input_stream(MediaStreamType::Audio, constraint_set)
}

pub fn create_videoinput_stream(constraint_set: MediaTrackConstraintSet) -> Option<MediaStreamId> {
    create_input_stream(MediaStreamType::Video, constraint_set)
}

/// Create a screen/window capture video stream for `getDisplayMedia()`.
///
/// Uses the GStreamer `d3d11screencapturesrc` element (Windows Graphics Capture /
/// DXGI desktop duplication). A non-empty `window_title` captures the matching
/// top-level window by `HWND`; otherwise `monitor_index` captures a whole monitor.
#[cfg(target_os = "windows")]
pub fn create_display_stream(
    source: DisplayCaptureSource,
    _constraint_set: MediaTrackConstraintSet,
) -> Option<MediaStreamId> {
    let src = match gstreamer::ElementFactory::make("d3d11screencapturesrc").build() {
        Ok(src) => src,
        Err(e) => {
            log::warn!("getDisplayMedia: failed to create d3d11screencapturesrc: {e}");
            return None;
        },
    };
    src.set_property("show-cursor", source.show_cursor);
    match source.window_title.as_deref().filter(|t| !t.is_empty()) {
        Some(title) => match find_window_by_title(title) {
            Some(hwnd) => {
                // The default DXGI Desktop Duplication backend can only capture
                // whole monitors; per-window capture requires Windows Graphics
                // Capture (WGC), which also gates the `window-handle` property
                // (`conditionally available`). Select WGC *before* setting the
                // handle, otherwise the property is absent and `set_property`
                // panics. WGC draws a yellow capture border around the window.
                src.set_property_from_str("capture-api", "wgc");
                src.set_property("window-handle", hwnd);
            },
            None => {
                log::warn!("getDisplayMedia: no visible window title contains {title:?}");
                return None;
            },
        },
        None => src.set_property("monitor-index", source.monitor_index),
    }

    // On GStreamer 1.26.x, `d3d11screencapturesrc` negotiates
    // `video/x-raw(memory:D3D11Memory),format=BGRA` — GPU-resident buffers. The
    // downstream CPU `videoconvert` (added by `create_video_from`) cannot map
    // D3D11 textures, so without an explicit readback the caps negotiation loops
    // forever and no frame ever reaches the <video> element (videoSize 0x0). Route
    // through `d3d11download` to read the frames back into system memory. (Older
    // 1.22.8 offered a system-memory caps variant directly and did not need this;
    // the runtime here is the system 1.26.x install.)
    let Ok(d3d11download) = gstreamer::ElementFactory::make("d3d11download").build() else {
        log::warn!("getDisplayMedia: failed to create d3d11download");
        return None;
    };
    // Servo's video appsink demands `video/x-raw,format=I420` (render.rs), and the
    // display playbin runs with video filters disabled, so it will NOT auto-insert
    // a converter. The capture frames are BGRA, so convert to I420 *inside this bin*
    // and pin it with a capsfilter — this makes the MediaStream deliver I420
    // deterministically (the `format=I420` src pad template alone does not
    // back-propagate the demand across the proxy boundary on GStreamer 1.26.x).
    let Ok(convert) = gstreamer::ElementFactory::make("videoconvert").build() else {
        log::warn!("getDisplayMedia: failed to create videoconvert");
        return None;
    };
    let i420_filter = match gstreamer::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gstreamer::Caps::builder("video/x-raw")
                .field("format", "I420")
                .build(),
        )
        .build()
    {
        Ok(f) => f,
        Err(e) => {
            log::warn!("getDisplayMedia: failed to create I420 capsfilter: {e}");
            return None;
        },
    };
    let bin = gstreamer::Bin::new();
    if bin
        .add_many([&src, &d3d11download, &convert, &i420_filter])
        .is_err()
        || src.link(&d3d11download).is_err()
        || d3d11download.link(&convert).is_err()
        || convert.link(&i420_filter).is_err()
    {
        log::warn!("getDisplayMedia: failed to assemble capture bin");
        return None;
    }
    let Some(src_pad) = i420_filter.static_pad("src") else {
        log::warn!("getDisplayMedia: capsfilter has no src pad");
        return None;
    };
    let Ok(ghost_pad) = gstreamer::GhostPad::with_target(&src_pad) else {
        log::warn!("getDisplayMedia: failed to create ghost pad for capture bin");
        return None;
    };
    if bin.add_pad(&ghost_pad).is_err() {
        log::warn!("getDisplayMedia: failed to add ghost pad to capture bin");
        return None;
    }

    Some(GStreamerMediaStream::create_video_from(bin.upcast()))
}

#[cfg(not(target_os = "windows"))]
pub fn create_display_stream(
    _source: DisplayCaptureSource,
    _constraint_set: MediaTrackConstraintSet,
) -> Option<MediaStreamId> {
    // Screen capture is currently only wired for the Windows (d3d11screencapturesrc)
    // path. Other platforms (ximagesrc/pipewiresrc) are future work.
    log::warn!("getDisplayMedia: screen capture is not implemented on this platform");
    None
}

/// Find the first visible top-level window whose title contains `needle`
/// (case-insensitive), returning its `HWND` as a `u64` for `window-handle`.
#[cfg(target_os = "windows")]
fn find_window_by_title(needle: &str) -> Option<u64> {
    use windows_sys::Win32::Foundation::{FALSE, HWND, LPARAM, TRUE};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
    };

    struct Ctx {
        needle_lower: String,
        found: u64,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
        let ctx = unsafe { &mut *(lparam as *mut Ctx) };
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return TRUE;
        }
        let len = unsafe { GetWindowTextLengthW(hwnd) };
        if len <= 0 {
            return TRUE;
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        if n <= 0 {
            return TRUE;
        }
        let title = String::from_utf16_lossy(&buf[..n as usize]).to_lowercase();
        if title.contains(&ctx.needle_lower) {
            ctx.found = hwnd as u64;
            return FALSE; // stop enumeration
        }
        TRUE
    }

    let mut ctx = Ctx {
        needle_lower: needle.to_lowercase(),
        found: 0,
    };
    unsafe {
        EnumWindows(Some(enum_proc), &mut ctx as *mut Ctx as LPARAM);
    }
    (ctx.found != 0).then_some(ctx.found)
}

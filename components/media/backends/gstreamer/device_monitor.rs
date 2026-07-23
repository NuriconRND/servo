/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;
use std::collections::HashSet;

use gstreamer::DeviceMonitor as GstDeviceMonitor;
use gstreamer::prelude::*;
use servo_media_streams::device_monitor::{MediaDeviceInfo, MediaDeviceKind, MediaDeviceMonitor};

use crate::device_id::{classify_device_class, device_api, device_path, normalized_port_key};

pub struct GStreamerDeviceMonitor {
    devices: RefCell<Option<Vec<MediaDeviceInfo>>>,
}

impl GStreamerDeviceMonitor {
    pub fn new() -> Self {
        Self {
            devices: RefCell::new(None),
        }
    }

    fn get_devices(&self) -> Result<Vec<MediaDeviceInfo>, ()> {
        const AUDIO_SOURCE: &str = "Audio/Source";
        const AUDIO_SINK: &str = "Audio/Sink";
        const VIDEO_SOURCE: &str = "Video/Source";
        let device_monitor = GstDeviceMonitor::new();
        let audio_caps = gstreamer_audio::AudioCapsBuilder::new().build();
        device_monitor.add_filter(Some(AUDIO_SOURCE), Some(&audio_caps));
        device_monitor.add_filter(Some(AUDIO_SINK), Some(&audio_caps));
        let video_caps = gstreamer_video::VideoCapsBuilder::new().build();
        device_monitor.add_filter(Some(VIDEO_SOURCE), Some(&video_caps));

        struct Candidate {
            info: MediaDeviceInfo,
            port_key: Option<String>,
            is_mediafoundation: bool,
        }
        let candidates: Vec<Candidate> = device_monitor
            .devices()
            .iter()
            .filter_map(|device| {
                let kind = classify_device_class(device.device_class().as_str())?;
                let label = device.display_name().as_str().to_owned();
                let path = device_path(device);
                // The id must be unique per physical port: capture cards expose
                // several ports under one display name, so prefer the device
                // path over the (possibly duplicated) display name.
                let device_id = path.clone().unwrap_or_else(|| label.clone());
                Some(Candidate {
                    info: MediaDeviceInfo {
                        device_id,
                        kind,
                        label,
                    },
                    port_key: path.as_deref().map(normalized_port_key),
                    is_mediafoundation: device_api(device).as_deref() == Some("mediafoundation"),
                })
            })
            .collect();

        // The same physical capture port shows up once per provider API
        // (winks + mediafoundation). Expose the mediafoundation entry — it has
        // a distinguishable display name and a `device.path` — and hide
        // provider twins of ports it already covers. Opening still prefers the
        // ksvideosrc twin (see media_capture.rs::select_device_by_id).
        let mf_video_keys: HashSet<String> = candidates
            .iter()
            .filter(|c| matches!(c.info.kind, MediaDeviceKind::VideoInput) && c.is_mediafoundation)
            .filter_map(|c| c.port_key.clone())
            .collect();
        Ok(candidates
            .into_iter()
            .filter(|c| {
                c.is_mediafoundation
                    || !matches!(c.info.kind, MediaDeviceKind::VideoInput)
                    || c.port_key
                        .as_deref()
                        .is_none_or(|key| !mf_video_keys.contains(key))
            })
            .map(|c| c.info)
            .collect())
    }
}

impl MediaDeviceMonitor for GStreamerDeviceMonitor {
    fn enumerate_devices(&self) -> Option<Vec<MediaDeviceInfo>> {
        {
            if let Some(ref devices) = *self.devices.borrow() {
                return Some(devices.clone());
            }
        }
        let devices = self.get_devices().ok()?;
        *self.devices.borrow_mut() = Some(devices.clone());
        Some(devices)
    }
}

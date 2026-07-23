/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Shared device-identity helpers for the GStreamer capture backend.
//!
//! `enumerate_devices()` (device_monitor.rs) and `get_track()`
//! (media_capture.rs) each probe a *separate* `GstDeviceMonitor`, so the
//! deviceId a page receives from enumeration must be re-derivable identically
//! when opening a device. Keep all id/key derivation here.

use gstreamer::prelude::*;
use servo_media_streams::device_monitor::MediaDeviceKind;

/// Map a `GstDevice` class string to a `MediaDeviceKind`.
///
/// Windows providers disagree on token order: winks reports `Video/Source`
/// while mediafoundation reports `Source/Video`. Match on the set of
/// `/`-separated tokens instead of the exact string (the old exact match
/// silently dropped every mediafoundation device, which is why capture cards
/// showed N identical ksvideosrc names).
pub fn classify_device_class(class: &str) -> Option<MediaDeviceKind> {
    let has = |t: &str| class.split('/').any(|token| token.eq_ignore_ascii_case(t));
    if has("Video") && has("Source") {
        Some(MediaDeviceKind::VideoInput)
    } else if has("Audio") && has("Source") {
        Some(MediaDeviceKind::AudioInput)
    } else if has("Audio") && has("Sink") {
        Some(MediaDeviceKind::AudioOutput)
    } else {
        None
    }
}

/// Normalize a Windows device interface path so the winks and mediafoundation
/// views of the *same physical port* compare equal.
///
/// The two APIs differ only in the KS category GUID segment:
///   ks: \\?\pci#...#6&2adcf5b7&0&000800e7#{6994ad05-...}\{6f814be9-...}
///   mf: \\?\pci#...#6&2adcf5b7&0&000800e7#{65e8773d-...}\{6f814be9-...}
/// Dropping the `#{...}` segment (and lowercasing) yields a stable per-port key.
pub fn normalized_port_key(path: &str) -> String {
    let mut key = path.to_ascii_lowercase();
    if let Some(start) = key.find("#{") {
        if let Some(len) = key[start..].find('}') {
            key.replace_range(start..start + len + 1, "");
        }
    }
    key
}

/// The unique device path, if the provider exposes one.
///
/// mediafoundation fills the `device.path` property on the `GstDevice`; winks
/// does not populate device properties at all, but its source element carries
/// the path in the `device-path` property, so fall back to instantiating the
/// element (stays in NULL state; cheap) and reading it back.
pub fn device_path(device: &gstreamer::Device) -> Option<String> {
    if let Some(props) = device.properties() {
        if let Ok(path) = props.get::<String>("device.path") {
            if !path.is_empty() {
                return Some(path);
            }
        }
    }
    let element = device.create_element(None).ok()?;
    element.find_property("device-path")?;
    element
        .property::<Option<String>>("device-path")
        .filter(|path| !path.is_empty())
}

/// The provider API name (`device.api` property), e.g. "mediafoundation".
pub fn device_api(device: &gstreamer::Device) -> Option<String> {
    device
        .properties()
        .and_then(|props| props.get::<String>("device.api").ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 실측 경로 (gst-device-monitor-1.0, MZ0380 4포트 카드)
    const KS_PORT1: &str = r"\\?\pci#ven_12ab&dev_0381&subsys_12abe5cf&rev_00#6&2adcf5b7&0&000800e7#{6994ad05-93ef-11d0-a3cc-00a0c9223196}\{6f814be9-9af6-43cf-9249-c03401000222}";
    const MF_PORT1: &str = r"\\?\pci#ven_12ab&dev_0381&subsys_12abe5cf&rev_00#6&2adcf5b7&0&000800e7#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\{6f814be9-9af6-43cf-9249-c03401000222}";
    const MF_PORT2: &str = r"\\?\pci#ven_12ab&dev_0381&subsys_12abe5cf&rev_00#6&74d9658&0&001000e7#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\{6f814be9-9af6-43cf-9249-c03402000222}";

    #[test]
    fn same_port_across_apis_shares_key() {
        assert_eq!(normalized_port_key(KS_PORT1), normalized_port_key(MF_PORT1));
    }

    #[test]
    fn different_ports_have_different_keys() {
        assert_ne!(normalized_port_key(MF_PORT1), normalized_port_key(MF_PORT2));
    }

    #[test]
    fn key_is_case_insensitive() {
        assert_eq!(
            normalized_port_key(&MF_PORT1.to_ascii_uppercase()),
            normalized_port_key(MF_PORT1)
        );
    }

    #[test]
    fn path_without_category_guid_is_only_lowercased() {
        assert_eq!(normalized_port_key("No-Guid-Here"), "no-guid-here");
    }

    #[test]
    fn classifies_both_video_source_token_orders() {
        assert!(matches!(
            classify_device_class("Video/Source"),
            Some(MediaDeviceKind::VideoInput)
        ));
        assert!(matches!(
            classify_device_class("Source/Video"),
            Some(MediaDeviceKind::VideoInput)
        ));
    }

    #[test]
    fn classifies_audio_and_rejects_unknown() {
        assert!(matches!(
            classify_device_class("Audio/Source"),
            Some(MediaDeviceKind::AudioInput)
        ));
        assert!(matches!(
            classify_device_class("Audio/Sink"),
            Some(MediaDeviceKind::AudioOutput)
        ));
        assert!(classify_device_class("Video/Sink").is_none());
        assert!(classify_device_class("Source/Monitor").is_none());
    }
}

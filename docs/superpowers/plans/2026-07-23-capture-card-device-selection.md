# 캡처카드 getUserMedia 디바이스 선택 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `enumerateDevices()`가 캡처카드 4포트를 구분되는 label/유니크 deviceId로 반환하고, `getUserMedia({video:{deviceId}})`로 특정 포트를 열 수 있게 한다 (표시=mediafoundation, 실사용 element=ksvideosrc).

**Architecture:** GStreamer 백엔드에 장치 식별 공유 헬퍼 모듈(`device_id.rs`)을 신설하고, 열거(`device_monitor.rs`)와 오픈(`media_capture.rs`)이 같은 id 파생 로직을 쓰게 한다. DOM 쪽은 WebIDL의 주석 처리된 `ConstrainDOMString`/`deviceId`를 활성화해 `MediaTrackConstraintSet::device_id`로 흘려보낸다. 스펙: `docs/superpowers/specs/2026-07-23-capture-card-device-selection-design.md`.

**Tech Stack:** Rust (gstreamer-rs, glib), Servo WebIDL codegen, GStreamer 사내 커스텀 1.28.4.100 번들.

## Global Constraints

- 작업 디렉터리: **`W:\servo_multigpu-tiled-wall`** (subst 필수 — 긴 경로에서 mozangle build.rs가 Os error 206으로 죽음). W:가 없으면: `subst W: F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser`
- 모든 빌드/검사 전 PowerShell에서: `. W:\scripts\servo_env.ps1` 소싱 후 `$ErrorActionPreference='Continue'` (mach가 cargo stderr로 중단되는 것 방지)
- 미디어 코드 변경의 **런타임 검증은 full `mach build` 산출물로만** 유효 (`cargo build -p servoshell`은 media-gstreamer 피처 누락 → 더미 백엔드)
- release 최종 링크에서 lld-link가 0xc0000005로 죽으면: `$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = "<MSVC link.exe 풀경로>"` (vswhere로 위치 확인; bare `link`는 GnuWin32 함정)
- 렌더가 필요한 실행은 반드시 `--wall-all-tiles` 포함 (이 브랜치는 없으면 아무것도 렌더 안 됨)
- 완료 조건: `cargo check -p servoshell` + `rustfmt --edition 2024 --check <touched .rs>` + `git diff --check` 통과
- 커밋 메시지는 저장소 관례대로 한국어, 말미에 Claude Co-Authored-By/세션 링크 트레일러

---

### Task 1: `ConstrainString` + `MediaTrackConstraintSet.device_id` (servo-media-streams)

**Files:**
- Modify: `components/media/streams/capture.rs:16-28`
- Modify: `components/script/dom/media/mediadevices.rs:155` (구성 사이트 컴파일 유지용 `device_id: None` — Task 5에서 실제 변환으로 교체)

**Interfaces:**
- Produces: `servo_media_streams::capture::ConstrainString { Exact(String), Ideal(String) }` (Clone, Debug), `MediaTrackConstraintSet.device_id: Option<ConstrainString>` — Task 4·5가 사용

- [ ] **Step 1: capture.rs에 타입 추가**

`components/media/streams/capture.rs`의 `MediaTrackConstraintSet` 정의를 다음으로 교체:

```rust
#[derive(Default)]
pub struct MediaTrackConstraintSet {
    pub width: Option<Constrain<u32>>,
    pub height: Option<Constrain<u32>>,
    pub aspect: Option<Constrain<f64>>,
    pub frame_rate: Option<Constrain<f64>>,
    pub sample_rate: Option<Constrain<u32>>,
    pub device_id: Option<ConstrainString>,
}

/// A DOMString-valued constraint (`deviceId` 등).
///
/// 스펙과 달리 이 프로젝트에선 Exact/Ideal 모두 "일치 없으면 실패"로 다룬다
/// (월 디버깅 시 잘못된 포트가 조용히 열리는 것 방지 — 설계 스펙 참조).
#[derive(Clone, Debug)]
pub enum ConstrainString {
    Exact(String),
    Ideal(String),
}
```

- [ ] **Step 2: mediadevices.rs 구성 사이트에 임시 필드 추가**

`components/script/dom/media/mediadevices.rs:155`의 `Some(MediaTrackConstraintSet {` 블록에 `sample_rate:` 줄 다음 한 줄 추가:

```rust
            device_id: None,
```

- [ ] **Step 3: 컴파일 확인**

Run: `cargo check -p servo-media-streams` → Expected: 성공
Run: `cargo check -p servoshell` → Expected: 성공 (최초 1회는 오래 걸림; 이후 증분)

- [ ] **Step 4: Commit**

```bash
git add components/media/streams/capture.rs components/script/dom/media/mediadevices.rs
git commit -m "feat(media): MediaTrackConstraintSet에 device_id(ConstrainString) 추가"
```

---

### Task 2: 장치 식별 공유 헬퍼 `device_id.rs` (servo-media-gstreamer) — TDD

**Files:**
- Create: `components/media/backends/gstreamer/device_id.rs`
- Modify: `components/media/backends/gstreamer/lib.rs:9` 부근 (`mod device_id;` 추가)
- Test: 같은 파일 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces (crate 내부 공용): `classify_device_class(&str) -> Option<MediaDeviceKind>`, `normalized_port_key(&str) -> String`, `device_path(&gstreamer::Device) -> Option<String>`, `device_api(&gstreamer::Device) -> Option<String>` — Task 3·4가 사용

- [ ] **Step 1: 실패하는 테스트와 함께 모듈 뼈대 작성**

`components/media/backends/gstreamer/device_id.rs` 신규 (함수 본문은 일단 `todo!()`; 테스트가 컴파일되도록 시그니처만):

```rust
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
    todo!()
}

/// Normalize a Windows device interface path so the winks and mediafoundation
/// views of the *same physical port* compare equal.
///
/// The two APIs differ only in the KS category GUID segment:
///   ks: \\?\pci#...#6&2adcf5b7&0&000800e7#{6994ad05-...}\{6f814be9-...}
///   mf: \\?\pci#...#6&2adcf5b7&0&000800e7#{65e8773d-...}\{6f814be9-...}
/// Dropping the `#{...}` segment (and lowercasing) yields a stable per-port key.
pub fn normalized_port_key(path: &str) -> String {
    todo!()
}

/// The unique device path, if the provider exposes one.
///
/// mediafoundation fills the `device.path` property on the `GstDevice`; winks
/// does not populate device properties at all, but its source element carries
/// the path in the `device-path` property, so fall back to instantiating the
/// element (stays in NULL state; cheap) and reading it back.
pub fn device_path(device: &gstreamer::Device) -> Option<String> {
    todo!()
}

/// The provider API name (`device.api` property), e.g. "mediafoundation".
pub fn device_api(device: &gstreamer::Device) -> Option<String> {
    todo!()
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
```

`components/media/backends/gstreamer/lib.rs`의 `mod datachannel;` 다음 줄에 추가:

```rust
mod device_id;
```

- [ ] **Step 2: 테스트가 실패(패닉)하는지 확인**

Run: `cargo test -p servo-media-gstreamer device_id`
Expected: 컴파일 성공, `normalized_port_key`/`classify` 테스트들이 `todo!()` 패닉으로 FAIL.
(링크/DLL 문제로 테스트 exe 실행이 불가한 경우: 이 사실을 기록하고 Step 4의 컴파일 확인 + Task 7 런타임 스모크로 대체 검증.)

- [ ] **Step 3: 구현**

`todo!()` 4개를 다음 본문으로 교체:

```rust
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

pub fn normalized_port_key(path: &str) -> String {
    let mut key = path.to_ascii_lowercase();
    if let Some(start) = key.find("#{") {
        if let Some(len) = key[start..].find('}') {
            key.replace_range(start..start + len + 1, "");
        }
    }
    key
}

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

pub fn device_api(device: &gstreamer::Device) -> Option<String> {
    device
        .properties()
        .and_then(|props| props.get::<String>("device.api").ok())
}
```

- [ ] **Step 4: 테스트 통과 확인**

Run: `cargo test -p servo-media-gstreamer device_id`
Expected: 6 passed. (실행 불가 시: `cargo check -p servo-media-gstreamer` 성공으로 대체하고 Task 7에서 보완.)

- [ ] **Step 5: Commit**

```bash
git add components/media/backends/gstreamer/device_id.rs components/media/backends/gstreamer/lib.rs
git commit -m "feat(media): 캡처 장치 식별 공유 헬퍼(device_id) — 포트 정규화 키/클래스 토큰 매칭"
```

---

### Task 3: 열거 수정 — mf 대표 노출 + 유니크 deviceId (device_monitor.rs)

**Files:**
- Modify: `components/media/backends/gstreamer/device_monitor.rs:22-50` (`get_devices`)

**Interfaces:**
- Consumes: Task 2의 `classify_device_class`/`normalized_port_key`/`device_path`/`device_api`
- Produces: `enumerate_devices()`가 videoinput에 대해 mf 장치(구분 label + `device.path` id)를 노출, 같은 포트의 ks 쌍둥이는 숨김

- [ ] **Step 1: get_devices 교체**

`device_monitor.rs`의 `get_devices` 본문을 다음으로 교체 (`use` 추가: `use std::collections::HashSet;` 파일 상단, `use crate::device_id::{classify_device_class, device_api, device_path, normalized_port_key};`):

```rust
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
```

주의: 기존 `use servo_media_streams::device_monitor::{MediaDeviceInfo, MediaDeviceKind, MediaDeviceMonitor};`는 유지 (MediaDeviceKind를 matches!에 사용). `is_none_or`가 이 Rust 버전(1.95.0)에 없다는 에러가 나면 `.map_or(true, |key| !mf_video_keys.contains(key))`로 대체.

- [ ] **Step 2: 컴파일 확인**

Run: `cargo check -p servo-media-gstreamer`
Expected: 성공 (경고 없이)

- [ ] **Step 3: Commit**

```bash
git add components/media/backends/gstreamer/device_monitor.rs
git commit -m "fix(media): enumerateDevices 캡처카드 포트 구분 — mf 클래스(Source/Video) 탈락 수정 + device.path 기반 유니크 id + ks 쌍둥이 숨김"
```

---

### Task 4: 오픈 배선 — deviceId 매칭 + ks 우선 (media_capture.rs)

**Files:**
- Modify: `components/media/backends/gstreamer/media_capture.rs:120-144` (`get_track`) + 파일 상단 use

**Interfaces:**
- Consumes: Task 1 `ConstrainString`, Task 2 헬퍼
- Produces: `get_track`이 `constraints.device_id`를 해석: 정규화 키 일치 장치 중 ks 우선 선택, 미일치 시 `None`(트랙 0개), 미지정 시 현행 `front()`

- [ ] **Step 1: get_track 교체 + 선택 헬퍼 추가**

파일 상단 use에 추가:

```rust
use crate::device_id::{device_api, device_path, normalized_port_key};
```

(`servo_media_streams::capture::*`는 이미 glob import — `ConstrainString` 포함됨.)

`GstMediaDevices::get_track`을 다음으로 교체:

```rust
    pub fn get_track(
        &self,
        video: bool,
        mut constraints: MediaTrackConstraintSet,
    ) -> Option<GstMediaTrack> {
        let device_id = constraints.device_id.take();
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
        let device = match &device_id {
            Some(requested) => {
                let (ConstrainString::Exact(requested_id) | ConstrainString::Ideal(requested_id)) =
                    requested;
                select_device_by_id(devices.iter(), requested_id)?
            },
            None => devices.front()?.clone(),
        };
        let element = device.create_element(None).ok()?;
        Some(GstMediaTrack { element })
    }
```

`GstMediaDevices` impl 블록 뒤에 자유 함수 추가:

```rust
/// Resolve a requested deviceId (as returned by `enumerateDevices` — a
/// mediafoundation `device.path`) to the `GstDevice` to open.
///
/// The same physical port is matched across provider APIs via
/// `normalized_port_key`. When both providers expose it, prefer the winks
/// (`ksvideosrc`) twin for the actual capture element (project decision:
/// display = mediafoundation, capture = ksvideosrc), falling back to the
/// mediafoundation device itself. No match at all fails the track — both
/// Exact and Ideal — so a wrong port never opens silently (see design spec).
fn select_device_by_id<'a>(
    devices: impl Iterator<Item = &'a gstreamer::Device>,
    requested_id: &str,
) -> Option<gstreamer::Device> {
    let requested_key = normalized_port_key(requested_id);
    let mut mediafoundation_match = None;
    let mut other_match = None;
    let mut available = Vec::new();
    for device in devices {
        let Some(path) = device_path(device) else {
            continue;
        };
        if normalized_port_key(&path) != requested_key {
            available.push(path);
            continue;
        }
        if device_api(device).as_deref() == Some("mediafoundation") {
            mediafoundation_match = Some(device.clone());
        } else {
            other_match = Some(device.clone());
        }
    }
    if other_match.is_none() && mediafoundation_match.is_some() {
        log::warn!(
            "getUserMedia: no ksvideosrc twin for deviceId {requested_id:?}; \
             falling back to the mediafoundation device"
        );
    }
    let selected = other_match.or(mediafoundation_match);
    match &selected {
        Some(device) => log::info!(
            "getUserMedia: deviceId {:?} -> {:?} (api {:?})",
            requested_id,
            device.display_name().as_str(),
            device_api(device).as_deref().unwrap_or("winks/other"),
        ),
        None => log::warn!(
            "getUserMedia: no device matches deviceId {requested_id:?}; \
             available device paths: {available:?}"
        ),
    }
    selected
}
```

- [ ] **Step 2: 컴파일 확인**

Run: `cargo check -p servo-media-gstreamer`
Expected: 성공

- [ ] **Step 3: Commit**

```bash
git add components/media/backends/gstreamer/media_capture.rs
git commit -m "feat(media): getUserMedia deviceId로 캡처 포트 선택 — 정규화 키 매칭, 캡처 element는 ksvideosrc 우선"
```

---

### Task 5: DOM 배선 — WebIDL deviceId 활성화 + 변환 (script)

**Files:**
- Modify: `components/script_bindings/webidls/MediaDevices.webidl:61-97`
- Modify: `components/script/dom/media/mediadevices.rs` (import, `convert_constraints`, Task 1의 `device_id: None` 교체, 변환 함수 추가)

**Interfaces:**
- Consumes: Task 1 `ConstrainString`
- Produces: 페이지의 `{deviceId:"id"}` / `{deviceId:{exact:"id"}}` / `{deviceId:{ideal:"id"}}` / `{deviceId:["id",...]}`가 `MediaTrackConstraintSet.device_id`로 전달됨

- [ ] **Step 1: WebIDL 주석 해제**

`MediaDevices.webidl`에서 다음 3곳 활성화:

66-69행의 주석 블록을 실제 dictionary로:

```webidl
dictionary ConstrainDOMStringParameters {
             (DOMString or sequence<DOMString>) exact;
             (DOMString or sequence<DOMString>) ideal;
};
```

78행 typedef 주석 해제:

```webidl
typedef (DOMString or sequence<DOMString> or ConstrainDOMStringParameters) ConstrainDOMString;
```

95행 `MediaTrackConstraintSet` 내 멤버 주석 해제:

```webidl
             ConstrainDOMString deviceId;
```

(`groupId`는 주석 유지 — 비범위.)

- [ ] **Step 2: mediadevices.rs 변환 구현**

import 블록 수정 — `UnionTypes` import에 추가:

```rust
use crate::dom::bindings::codegen::UnionTypes::{
    BooleanOrMediaTrackConstraints, ClampedUnsignedLongOrConstrainULongRange as ConstrainULong,
    DoubleOrConstrainDoubleRange as ConstrainDouble, StringOrStringSequence,
    StringOrStringSequenceOrConstrainDOMStringParameters as ConstrainDOMString,
};
```

`servo_media::streams::capture` import에 `ConstrainString` 추가:

```rust
use servo_media::streams::capture::{
    Constrain, ConstrainRange, ConstrainString, DisplayCaptureSource, MediaTrackConstraintSet,
};
```

`convert_constraints`의 `device_id: None,`(Task 1)을 다음으로 교체:

```rust
            device_id: c.parent.deviceId.as_ref().and_then(convert_string_constraint),
```

파일 하단(`convert_cdouble` 다음)에 변환 함수 추가:

```rust
fn convert_string_constraint(js: &ConstrainDOMString) -> Option<ConstrainString> {
    fn first(value: &StringOrStringSequence) -> Option<String> {
        match value {
            StringOrStringSequence::String(s) => Some(s.to_string()),
            StringOrStringSequence::StringSequence(seq) => seq.first().map(|s| s.to_string()),
        }
    }
    match js {
        ConstrainDOMString::String(s) => Some(ConstrainString::Ideal(s.to_string())),
        ConstrainDOMString::StringSequence(seq) => {
            seq.first().map(|s| ConstrainString::Ideal(s.to_string()))
        },
        ConstrainDOMString::ConstrainDOMStringParameters(params) => {
            if let Some(exact) = params.exact.as_ref().and_then(first) {
                Some(ConstrainString::Exact(exact))
            } else {
                params.ideal.as_ref().and_then(first).map(ConstrainString::Ideal)
            }
        },
    }
}
```

주의: codegen 유니온 배리언트/타입 이름이 다르면(예: `StringSequence`가 아닌 다른 이름) 빌드 에러 메시지의 실제 생성 이름에 맞춰 조정. `params.exact`/`params.ideal`은 `Option<StringOrStringSequence>`.

- [ ] **Step 3: 컴파일 확인 (codegen 포함)**

Run: `cargo check -p servoshell`
Expected: 성공 (WebIDL 변경으로 script 재codegen — 수 분 소요)

- [ ] **Step 4: Commit**

```bash
git add components/script_bindings/webidls/MediaDevices.webidl components/script/dom/media/mediadevices.rs
git commit -m "feat(script): MediaTrackConstraints deviceId(ConstrainDOMString) 활성화 및 미디어 백엔드 배선"
```

---

### Task 6: probe 페이지 — `?device=` 포트 선택

**Files:**
- Modify: `tests/html/multigpu_capture_card_probe.html`

**Interfaces:**
- Consumes: Task 3의 구분 label/유니크 id, Task 5의 `{deviceId:{exact}}` 경로

- [ ] **Step 1: 선택 로직 추가**

헤더 주석의 `NOTE: capture cards often expose...(future work)` 문단을 다음으로 교체:

```
  Select a specific port with ?device=<N|substring>: a decimal N opens the
  N-th (0-based) videoinput from enumerateDevices(); anything else matches
  case-insensitively against device labels. Selection opens with
  { video: { deviceId: { exact: id } } }; no ?device keeps the legacy
  { video: true } first-device behavior. A selector that matches nothing
  fails loudly (no track) instead of silently opening another port.
```

`start()`의 enumerate/open 로직 수정 — `const devs = ...` 블록은 유지하되 스코프를 밖으로 빼고, open 단계를 다음으로 교체:

```js
      // 1) enumerate
      let devs = [];
      try {
        devs = (await navigator.mediaDevices.enumerateDevices())
          .filter(d => d.kind === "videoinput");
        clog("enumerateDevices: " + devs.length + " videoinput");
        deviceLines = "videoinput devices: " + devs.length;
        devs.forEach((d, i) => {
          const line = `  [${i}] label="${d.label}" id=${d.deviceId || "(empty)"}`;
          deviceLines += "\n" + line;
          clog(line.trim());
        });
      } catch (e) { deviceLines = "enumerateDevices FAILED: " + e; }
      render();

      // 2) pick the port: ?device=<N|substring> (default: first device)
      const sel = new URLSearchParams(location.search).get("device");
      let constraints = { video: true };
      if (sel !== null) {
        const chosen = /^\d+$/.test(sel)
          ? devs[Number(sel)]
          : devs.find(d => (d.label || "").toLowerCase().includes(sel.toLowerCase()));
        if (!chosen) {
          liveLines = `NO videoinput matches ?device=${JSON.stringify(sel)}`;
          clog(liveLines); render(); return;
        }
        constraints = { video: { deviceId: { exact: chosen.deviceId } } };
        deviceLines += `\nselected: "${chosen.label}"\n  id=${chosen.deviceId}`;
        clog(`selected device: label="${chosen.label}" id=${chosen.deviceId}`);
        render();
      }

      // 3) open
      try {
        const stream = await navigator.mediaDevices.getUserMedia(constraints);
```

(이후 기존 open-try 블록 본문은 그대로; `{ video: true }` 리터럴만 `constraints`로 대체된 형태. catch 블록·통계 로직 무변경.)

- [ ] **Step 2: Commit**

```bash
git add tests/html/multigpu_capture_card_probe.html
git commit -m "test(html): 캡처카드 probe에 ?device= 포트 선택 추가"
```

---

### Task 7: 정적 검증 + full 빌드 + 4포트 런타임 스모크

**Files:**
- 산출물: `servoshell_capture_device_*.err.log` (검증 후 삭제 가능)

- [ ] **Step 1: 정적 검증**

```powershell
rustfmt --edition 2024 --check components\media\streams\capture.rs components\media\backends\gstreamer\device_id.rs components\media\backends\gstreamer\device_monitor.rs components\media\backends\gstreamer\media_capture.rs components\script\dom\media\mediadevices.rs
git diff --check
cargo test -p servo-media-gstreamer device_id
```

Expected: rustfmt 무출력, diff 무출력, 테스트 6 passed.

- [ ] **Step 2: full mach build (release — 사용자가 검증한 프로파일)**

```powershell
. W:\scripts\servo_env.ps1
$ErrorActionPreference='Continue'
cd W:\servo_multigpu-tiled-wall
.\mach build --release -j 8
```

Expected: exit 0. lld-link 0xc0000005 시 Global Constraints의 링커 오버라이드 적용 후 재시도.

- [ ] **Step 3: 스모크 — 열거 + 포트 지정 오픈 (예: 2번 포트)**

```powershell
$page = "file:///W:/servo_multigpu-tiled-wall/tests/html/multigpu_capture_card_probe.html?device=analog 03"
target\release\servoshell.exe --wall-layout ..\config\wall_layout.local_1x1.json --wall-all-tiles --pref dom_webrtc_enabled=true $page 2> ..\servoshell_capture_device_smoke.err.log
```

(레이아웃 파일명이 다르면 `config/` 안의 1타일 로컬 레이아웃 사용. 창을 **닫아서** 종료 — 강제 kill은 로그 유실.)

로그 확인:

```powershell
Select-String -Path ..\servoshell_capture_device_smoke.err.log -Pattern "capture-card-probe|getUserMedia:"
```

Expected:
- `enumerateDevices: 4 videoinput` (+ 다른 카메라 있으면 그 수만큼 가산)
- 4개 label이 `MZ0380 PCI, Analog 01..04 Capture`로 **전부 상이**, id도 전부 상이
- `selected device: label="MZ0380 PCI, Analog 03 Capture"`
- `getUserMedia: deviceId ... (api "winks/other")` — ks element 사용 확인
- `live videoSize=1920x1080 ... advancing` (해당 포트에 신호가 있을 때)

- [ ] **Step 4: 네거티브 스모크 — 미일치 id는 트랙 0**

```powershell
$page = "file:///W:/servo_multigpu-tiled-wall/tests/html/multigpu_capture_card_probe.html?device=nonexistent-port"
target\release\servoshell.exe --wall-layout ..\config\wall_layout.local_1x1.json --wall-all-tiles --pref dom_webrtc_enabled=true $page 2> ..\servoshell_capture_device_neg.err.log
```

Expected: `NO videoinput matches` (라벨 미일치는 페이지 단에서 차단). 추가로 백엔드 단 확인이 필요하면 probe를 잠깐 수정하지 말고 devtools 없이 다음으로 확인: `?device=0`으로 정상 오픈되는 로그와 대비해, 존재하지 않는 raw id를 exact로 주는 케이스는 단위 로그(`no device matches deviceId`)가 남는지 — 4포트 중 케이블 안 꽂힌 포트 id로 열어 트랙은 생기되 신호 없음과 구분할 것.

- [ ] **Step 5: 나머지 포트 순회 (01, 02, 04)**

Step 3을 `?device=analog 01` / `analog 02` / `analog 04`로 반복, 각각 selected 라벨과 videoSize/advancing 확인.

- [ ] **Step 6: 최종 커밋 (로그 정리 + 문서)**

스펙의 검증 절 결과를 스펙 파일 말미에 1-2줄 추기하고:

```bash
git add docs/superpowers/specs/2026-07-23-capture-card-device-selection-design.md
git commit -m "docs: 캡처카드 deviceId 선택 검증 결과 기록"
```

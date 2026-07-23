/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

pub struct ConstrainRange<T> {
    pub min: Option<T>,
    pub max: Option<T>,
    pub ideal: Option<T>,
}

pub enum ConstrainBool {
    Ideal(bool),
    Exact(bool),
}

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

pub enum Constrain<T> {
    Value(T),
    Range(ConstrainRange<T>),
}

/// Describes the fixed source captured by `MediaDevices.getDisplayMedia()`.
///
/// This v1 source is selected from preferences rather than an interactive picker:
/// a non-empty `window_title` captures the first matching top-level window,
/// otherwise `monitor_index` selects a whole monitor (-1 = primary).
#[derive(Clone, Debug)]
pub struct DisplayCaptureSource {
    pub monitor_index: i32,
    pub window_title: Option<String>,
    pub show_cursor: bool,
}

impl Default for DisplayCaptureSource {
    fn default() -> Self {
        Self {
            monitor_index: -1,
            window_title: None,
            show_cursor: true,
        }
    }
}

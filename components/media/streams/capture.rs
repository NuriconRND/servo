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

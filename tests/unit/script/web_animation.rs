/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use script::test::web_animation::{
    IterationSpec, KeyframeError, css_property_name, resolve_duration_seconds, resolve_iterations,
    resolve_offsets,
};

#[test]
fn offsets_empty() {
    assert_eq!(resolve_offsets(&[]), Ok(vec![]));
}

#[test]
fn offsets_single_frame_is_the_last_frame() {
    // 첫 프레임의 null 은 0, 마지막 프레임의 null 은 1. 프레임이 하나면 둘 다
    // 같은 프레임에 적용되어 1 로 끝난다.
    assert_eq!(resolve_offsets(&[None]), Ok(vec![1.0]));
}

#[test]
fn offsets_all_missing_are_evenly_distributed() {
    // 마퀴(TickerContent.tsx:116)가 offset 없이 2 프레임을 넘긴다.
    assert_eq!(resolve_offsets(&[None, None]), Ok(vec![0.0, 1.0]));
    assert_eq!(
        resolve_offsets(&[None, None, None]),
        Ok(vec![0.0, 0.5, 1.0])
    );
    assert_eq!(
        resolve_offsets(&[None, None, None, None, None]),
        Ok(vec![0.0, 0.25, 0.5, 0.75, 1.0])
    );
}

#[test]
fn offsets_all_declared_pass_through() {
    // 등장/퇴장(useEntranceAnimation.ts toCssFrames)은 offset 을 전부 명시한다.
    assert_eq!(
        resolve_offsets(&[Some(0.0), Some(0.3), Some(1.0)]),
        Ok(vec![0.0, 0.3, 1.0])
    );
}

#[test]
fn offsets_interpolate_between_declared_anchors() {
    assert_eq!(
        resolve_offsets(&[Some(0.0), None, Some(0.8), None, Some(1.0)]),
        Ok(vec![0.0, 0.4, 0.8, 0.9, 1.0])
    );
}

#[test]
fn offsets_repeated_value_is_allowed() {
    // 비감소면 된다. 같은 값이 연달아 오는 것은 합법이다.
    assert_eq!(
        resolve_offsets(&[Some(0.0), Some(0.5), Some(0.5), Some(1.0)]),
        Ok(vec![0.0, 0.5, 0.5, 1.0])
    );
}

#[test]
fn offsets_out_of_range_is_rejected() {
    assert_eq!(
        resolve_offsets(&[Some(0.0), Some(1.5)]),
        Err(KeyframeError::OffsetOutOfRange(1.5))
    );
    assert_eq!(
        resolve_offsets(&[Some(-0.1), Some(1.0)]),
        Err(KeyframeError::OffsetOutOfRange(-0.1))
    );
    assert!(matches!(
        resolve_offsets(&[Some(f64::NAN)]),
        Err(KeyframeError::OffsetOutOfRange(value)) if value.is_nan()
    ));
}

#[test]
fn offsets_decreasing_is_rejected() {
    assert_eq!(
        resolve_offsets(&[Some(0.6), Some(0.2)]),
        Err(KeyframeError::OffsetOutOfOrder)
    );
}

#[test]
fn duration_is_milliseconds_to_seconds() {
    assert_eq!(resolve_duration_seconds(1000.0), Some(1.0));
    assert_eq!(resolve_duration_seconds(450.0), Some(0.45));
}

#[test]
fn non_positive_or_non_finite_duration_creates_nothing() {
    // ***하드 가드.*** stylo 의 Animation 은 진행도를
    // `(now - started_at) / duration` 으로 계산하므로 0 이면 나눗셈이 깨진다.
    // CSS 경로도 `maybe_start_animations` 에서 `if duration == 0. { continue; }`
    // 로 같은 것을 막고 있다.
    assert_eq!(resolve_duration_seconds(0.0), None);
    assert_eq!(resolve_duration_seconds(-1.0), None);
    assert_eq!(resolve_duration_seconds(f64::NAN), None);
    assert_eq!(resolve_duration_seconds(f64::INFINITY), None);
}

#[test]
fn iterations_finite_and_infinite() {
    assert_eq!(resolve_iterations(1.0), Ok(IterationSpec::Finite(1.0)));
    assert_eq!(resolve_iterations(2.5), Ok(IterationSpec::Finite(2.5)));
    assert_eq!(resolve_iterations(0.0), Ok(IterationSpec::Finite(0.0)));
    // 마퀴가 iterations: Infinity 를 넘긴다.
    assert_eq!(
        resolve_iterations(f64::INFINITY),
        Ok(IterationSpec::Infinite)
    );
}

#[test]
fn iterations_negative_or_nan_is_rejected() {
    assert_eq!(
        resolve_iterations(-1.0),
        Err(KeyframeError::InvalidIterations)
    );
    assert_eq!(
        resolve_iterations(f64::NAN),
        Err(KeyframeError::InvalidIterations)
    );
}

#[test]
fn property_names_map_from_idl_to_css() {
    assert_eq!(css_property_name("opacity"), "opacity");
    assert_eq!(css_property_name("transform"), "transform");
    // 페이지(useEntranceAnimation.ts toCssFrames)가 camelCase 로 넘긴다.
    assert_eq!(css_property_name("transformOrigin"), "transform-origin");
    // 이미 CSS 이름이면 그대로.
    assert_eq!(css_property_name("transform-origin"), "transform-origin");
    // 명세가 정한 두 예외.
    assert_eq!(css_property_name("cssFloat"), "float");
    assert_eq!(css_property_name("cssOffset"), "offset");
    // 벤더 접두사는 앞에 하이픈이 붙는다.
    assert_eq!(css_property_name("webkitTransform"), "-webkit-transform");
}

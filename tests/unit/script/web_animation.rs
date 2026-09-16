/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use script::test::web_animation::{KeyframeError, resolve_offsets};

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

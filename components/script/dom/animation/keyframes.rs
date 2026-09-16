/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 스크립트가 넘긴 키프레임을 스타일이 쓸 수 있는 형태로 옮기는, JS 컨텍스트가
//! 필요 없는 부분.

/// `Element.animate()` 가 `TypeError` 로 거절해야 하는 입력.
#[derive(Debug, PartialEq)]
pub enum KeyframeError {
    /// `offset` 이 `[0, 1]` 밖이거나 숫자가 아니다.
    OffsetOutOfRange(f64),
    /// 명시된 `offset` 들이 비감소가 아니다.
    OffsetOutOfOrder,
    /// `iterations` 가 음수이거나 NaN 이다.
    InvalidIterations,
}

/// <https://drafts.csswg.org/web-animations-1/#compute-missing-keyframe-offsets>
///
/// 명시된 오프셋은 검사만 하고 그대로 두고, 비어 있는 자리는 양쪽의 알려진 이웃
/// 사이에 균등 분배한다. 첫 프레임의 빈 오프셋은 0, 마지막 프레임의 빈 오프셋은 1 이다.
pub fn resolve_offsets(declared: &[Option<f64>]) -> Result<Vec<f64>, KeyframeError> {
    if declared.is_empty() {
        return Ok(vec![]);
    }

    // 1. 명시된 값 검사: 범위와 순서.
    let mut previous: Option<f64> = None;
    for offset in declared.iter().flatten() {
        if !(0.0..=1.0).contains(offset) {
            // NaN 은 어떤 비교에도 false 이므로 여기서 함께 걸린다.
            return Err(KeyframeError::OffsetOutOfRange(*offset));
        }
        if previous.is_some_and(|previous| *offset < previous) {
            return Err(KeyframeError::OffsetOutOfOrder);
        }
        previous = Some(*offset);
    }

    // 2. 양 끝을 고정한다. 프레임이 하나뿐이면 두 규칙이 같은 자리에 적용되어 1 이 된다.
    // (마지막 규칙을 먼저 적용해야 인덱스가 겹치는 단일 프레임 경우 1 이 남는다.)
    let mut resolved: Vec<Option<f64>> = declared.to_vec();
    let last_index = resolved.len() - 1;
    if resolved[last_index].is_none() {
        resolved[last_index] = Some(1.0);
    }
    if resolved[0].is_none() {
        resolved[0] = Some(0.0);
    }

    // 3. 알려진 이웃 사이를 균등 분배한다.
    let mut anchor_index = 0;
    let mut anchor_value = resolved[0].expect("첫 오프셋은 위에서 고정했다");
    let mut output = vec![0.0; resolved.len()];
    output[0] = anchor_value;

    for index in 1..resolved.len() {
        let Some(value) = resolved[index] else {
            continue;
        };
        let span = index - anchor_index;
        for step in 1..span {
            output[anchor_index + step] =
                anchor_value + (value - anchor_value) * (step as f64) / (span as f64);
        }
        output[index] = value;
        anchor_index = index;
        anchor_value = value;
    }

    Ok(output)
}

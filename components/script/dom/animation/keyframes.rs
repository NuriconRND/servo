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

/// 애니메이션 반복 횟수. stylo 의 `KeyframesIterationState` 로 옮기기 전 형태다.
#[derive(Debug, PartialEq)]
pub enum IterationSpec {
    /// 유한 반복.
    Finite(f64),
    /// 무한 반복.
    Infinite,
}

/// 밀리초 duration 을 stylo 가 쓰는 초 단위로 옮긴다.
///
/// ***`None` 이면 애니메이션을 만들지 않는다.*** `stylo::Animation` 은 진행도를
/// `(now - started_at) / duration` 으로 계산하므로 0 이면 나눗셈이 깨진다. CSS 경로도
/// `maybe_start_animations` 에서 `if duration == 0. { continue; }` 로 같은 것을 막는다.
pub fn resolve_duration_seconds(duration_ms: f64) -> Option<f64> {
    if !duration_ms.is_finite() || duration_ms <= 0.0 {
        return None;
    }
    Some(duration_ms / 1000.0)
}

/// `iterations` 옵션을 해석한다. 음수와 NaN 은 `TypeError` 다.
pub fn resolve_iterations(iterations: f64) -> Result<IterationSpec, KeyframeError> {
    if iterations.is_nan() || iterations < 0.0 {
        return Err(KeyframeError::InvalidIterations);
    }
    if iterations.is_infinite() {
        return Ok(IterationSpec::Infinite);
    }
    Ok(IterationSpec::Finite(iterations))
}

/// <https://drafts.csswg.org/web-animations-1/#animation-property-name-to-idl-attribute-name>
/// 의 역방향. 키프레임의 키는 IDL 속성 이름(camelCase)일 수도, CSS 속성 이름일 수도 있다.
pub fn css_property_name(idl_name: &str) -> String {
    match idl_name {
        "cssFloat" => return "float".to_owned(),
        "cssOffset" => return "offset".to_owned(),
        _ => {},
    }

    // 이미 CSS 이름(하이픈이 있거나 대문자가 없다)이면 그대로 둔다.
    if !idl_name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return idl_name.to_owned();
    }

    let mut output = String::with_capacity(idl_name.len() + 2);
    if let Some(rest) = idl_name.strip_prefix("webkit") {
        output.push_str("-webkit");
        push_kebab(&mut output, rest);
        return output;
    }
    push_kebab(&mut output, idl_name);
    output
}

fn push_kebab(output: &mut String, input: &str) {
    for character in input.chars() {
        if character.is_ascii_uppercase() {
            output.push('-');
            output.push(character.to_ascii_lowercase());
        } else {
            output.push(character);
        }
    }
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `Element.animate()` 가 넘긴 키프레임을 stylo 의 자료구조로 옮기는 층.
//!
//! ***이 파일은 형제 모듈 `keyframes` 와 일부러 갈라져 있다.*** `keyframes.rs` 는
//! std 밖의 것을 하나도 쓰지 않아야 한다 -- 그래야 그 순수 함수들을 `#[path]` 로
//! 직접 끌어오는 독립 `rustc --test` 하네스에서 돌릴 수 있다. `use style::…` 가
//! 한 줄이라도 들어가면 그 테스트가 영영 돌지 않는다. stylo 를 필요로 하는
//! 변환은 전부 여기 둔다.

use cssparser::SourceLocation;
use script_bindings::record::Record;
use servo_arc::Arc;
use servo_url::ServoUrl;
use style::context::QuirksMode;
use style::properties::{
    Importance, PropertyDeclaration, PropertyDeclarationBlock, PropertyId,
    SourcePropertyDeclaration, parse_one_declaration_into,
};
use style::shared_lock::SharedRwLock;
use style::stylesheets::keyframes_rule::{
    Keyframe, KeyframePercentage, KeyframeSelectors, KeyframesAnimation,
};
use style::stylesheets::{CssRuleType, Origin, UrlExtraData};
use style::values::computed::TimingFunction;
use style::values::specified::easing::TimingFunction as SpecifiedTimingFunction;
use style_traits::ParsingMode;

use super::keyframes::{KeyframeError, css_property_name, resolve_offsets};
use crate::dom::bindings::str::DOMString;

/// `Element.animate()` 가 넘긴 키프레임 배열을 stylo 의 `KeyframesAnimation` 으로 옮긴다.
///
/// 반환하는 `TimingFunction` 은 전체 기본 타이밍 함수(옵션의 `easing`)다. 프레임이
/// 자기 `easing` 을 선언했으면 그 프레임의 선언 블록에
/// `animation-timing-function` 으로 들어가고, `KeyframesStep` 이 그것을 감지해
/// 기본값을 덮는다 -- CSS `@keyframes` 안에 같은 속성을 쓴 것과 같은 경로다.
///
/// `url` / `quirks_mode` 는 CSS 값 파싱에 쓰이고, `lock` 은 선언 블록을 감싸는 데
/// 쓰인다(문서의 저자 스타일 잠금).
pub(crate) fn build_keyframes_animation(
    frames: &[Record<DOMString, DOMString>],
    global_easing: &str,
    url: &ServoUrl,
    quirks_mode: QuirksMode,
    lock: &SharedRwLock,
) -> Result<(KeyframesAnimation, TimingFunction), KeyframeError> {
    let url_data = UrlExtraData(url.get_arc());

    // 1. 오프셋을 먼저 확정한다. 여기서 거절되면 아무것도 만들지 않는다.
    let declared: Vec<Option<f64>> = frames
        .iter()
        .map(|frame| {
            frame
                .get(&DOMString::from("offset"))
                .and_then(|value| value.str().trim().parse::<f64>().ok())
        })
        .collect();
    let offsets = resolve_offsets(&declared)?;

    // 2. 전체 기본 타이밍 함수.
    let timing_function = parse_timing_function(global_easing, &url_data, quirks_mode)
        .unwrap_or_else(|| {
            // 파싱 실패한 easing 은 조용히 기본값으로 떨어진다. 이 범위에서는
            // TypeError 를 쏘는 것보다 안전하다 -- 페이지가 넘기는 값은
            // 'linear' / 'ease-out' 넷뿐이고, 실패하면 애니메이션 자체가 없어지는
            // 것보다 곡선만 기본값인 편이 낫다.
            SpecifiedTimingFunction::ease().to_computed_value_without_context()
        });

    // 3. 프레임마다 선언 블록을 만든다.
    let mut keyframes = Vec::with_capacity(frames.len());
    for (frame, offset) in frames.iter().zip(offsets.iter()) {
        let mut block = PropertyDeclarationBlock::new();

        for (key, value) in frame.iter() {
            // `StringView` 는 `Deref<Target = str>` 이므로 명시적으로 `&str` 로 받는다.
            let key_view = key.str();
            let key: &str = &key_view;
            if key == "offset" || key == "composite" {
                continue;
            }
            let property_name = if key == "easing" {
                "animation-timing-function".to_owned()
            } else {
                css_property_name(key)
            };
            let Ok(id) = PropertyId::parse_enabled_for_all_content(&property_name) else {
                // 알 수 없는 속성은 조용히 버린다(명세 동작).
                continue;
            };

            let mut declarations = SourcePropertyDeclaration::default();
            if parse_one_declaration_into(
                &mut declarations,
                id,
                &value.str(),
                Origin::Author,
                &url_data,
                None,
                ParsingMode::DEFAULT,
                quirks_mode,
                CssRuleType::Keyframe,
            )
            .is_err()
            {
                // 파싱 실패한 값도 조용히 버린다(명세 동작).
                continue;
            }
            block.extend(declarations.drain(), Importance::Normal);
        }

        keyframes.push(Arc::new(lock.wrap(Keyframe {
            selector: KeyframeSelectors::from_percentage(KeyframePercentage(*offset as f32)),
            block: Arc::new(lock.wrap(block)),
            source_location: SourceLocation { line: 0, column: 0 },
        })));
    }

    // 4. stylo 가 정렬과 0%/100% 합성 스텝을 맡는다.
    //
    // ***`KeyframesStep` 을 직접 만들지 않는 이유가 여기 있다.*** 이 함수가
    // 부르는 `get_animated_properties` 가 `display` 와 애니메이션할 수 없는
    // 속성을 전부 걸러준다. 그것이 `start_script_animations` 의
    // `debug_assert!(property.is_animatable())` 를 스크립트 입력으로 터뜨릴 수
    // 없게 만드는 유일한 장치다.
    let guard = lock.read();
    let animation = KeyframesAnimation::from_keyframes(&keyframes, None, &guard);
    drop(guard);

    Ok((animation, timing_function))
}

/// `easing` 문자열을 계산된 `TimingFunction` 으로 옮긴다.
fn parse_timing_function(
    easing: &str,
    url_data: &UrlExtraData,
    quirks_mode: QuirksMode,
) -> Option<TimingFunction> {
    let Ok(id) = PropertyId::parse_enabled_for_all_content("animation-timing-function") else {
        return None;
    };
    let mut declarations = SourcePropertyDeclaration::default();
    parse_one_declaration_into(
        &mut declarations,
        id,
        easing,
        Origin::Author,
        url_data,
        None,
        ParsingMode::DEFAULT,
        quirks_mode,
        CssRuleType::Style,
    )
    .ok()?;

    let mut block = PropertyDeclarationBlock::new();
    block.extend(declarations.drain(), Importance::Normal);
    block.declarations().iter().find_map(|declaration| {
        match declaration {
            // 단일 값만 쓴다. `animation-timing-function` 은 리스트 속성이다.
            PropertyDeclaration::AnimationTimingFunction(value) => {
                Some(value.0[0].to_computed_value_without_context())
            },
            _ => None,
        }
    })
}

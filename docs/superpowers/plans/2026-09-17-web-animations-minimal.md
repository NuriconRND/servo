# Web Animations 최소 구현 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `Element.animate()` 와 `Animation.cancel()` 을 pref 뒤에 최소 범위로 구현해, `/output` 페이지가 Servo 전용 CSS `@keyframes` 폴백 대신 Chrome과 같은 네이티브 경로를 타게 한다.

**Architecture:** 스크립트가 키프레임을 파싱해 `KeyframesAnimation` 을 만들고 `ElementAnimationSet.pending_script` 에 적재한다. 리스타일이 그것을 드레인해 기존 `ComputedKeyframe::generate_for_keyframes` 로 계산하고 보통의 `stylo::Animation` 으로 만든다. 그 아래(캐스케이드·페인트측 바인딩·타임라인)는 CSS 애니메이션과 **완전히 같은 경로**다. 스타일이 애니메이션 수명을 모는 세 자리만 `AnimationOrigin::Script` 를 건너뛴다.

**Tech Stack:** Rust 2024, Servo(이 포크), 벤더된 stylo(`third_party/stylo`), WebIDL 코드젠(`components/script_bindings/codegen`), SpiderMonkey 바인딩.

**Spec:** `docs/superpowers/specs/2026-09-17-web-animations-minimal-design.md`

## Global Constraints

- **저장소 범위**: 수정은 엔진에 한한다. `F:\20260609_SDWall_BrowserTest\frontend` 와 `backend` 는 **참고용이며 수정 금지**(열람은 자유).
- **브랜치**: `wall-transition-flicker`. 커밋 메시지 끝에 `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>` 를 붙인다.
- **스테이징 금지 파일**: `Cargo.lock`, `etc/multigpu/config/wall_layout.example_1x1.json`, `tests/html/multigpu_standard_video_extended_probe.html`, `tests/html/multigpu_standard_video_rtsp_probe.html` — 이미 더티이고 이 작업과 무관하다. `git add` 는 **항상 파일을 명시**하고 `git add -A` 를 쓰지 않는다.
- **pref 기본값**: `dom_web_animations_enabled = false`. 이 값을 뒤집는 것은 이 계획의 범위 밖이며 별도 커밋이다.
- **합성 애니메이션 이름**: `-servo-script-<N>` (N은 문서별 1부터 증가).
- **빌드 명령**: `cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu`
- **실기 바이너리**: `winit_wall` (`servoshell` 아님).
- **커밋 전**: 손댄 파일에 `rustfmt`, 그리고 `git diff --check`.
- **A/B 측정 시**: 반드시 `-NumaNode 1`. `-NumaNode auto` 는 이 장비에서 무효다.

---

### Task 1: 순수 키프레임 오프셋 해석 + 테스트 하네스

이 태스크는 **테스트 하네스가 실제로 도는지**를 먼저 증명한다. `components/script` 에는 in-crate 테스트가 하나도 없으므로, 저장소의 기존 방식(`script::test` 재노출 + `tests/unit/script` 크레이트)을 따른다.

**Files:**
- Create: `components/script/dom/animation/mod.rs`
- Create: `components/script/dom/animation/keyframes.rs`
- Modify: `components/script/dom/mod.rs` (모듈 등록)
- Modify: `components/script/test.rs` (테스트 재노출)
- Create: `tests/unit/script/web_animation.rs`
- Modify: `tests/unit/script/lib.rs` (테스트 모듈 등록)

**Interfaces:**
- Consumes: 없음 (첫 태스크)
- Produces:
  - `pub enum KeyframeError { OffsetOutOfRange(f64), OffsetOutOfOrder, InvalidIterations }`
  - `pub fn resolve_offsets(declared: &[Option<f64>]) -> Result<Vec<f64>, KeyframeError>`

---

- [ ] **Step 1: 모듈 디렉터리와 빈 파일 만들기**

`components/script/dom/animation/mod.rs`:

```rust
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Web Animations 최소 구현.
//!
//! 설계: `docs/superpowers/specs/2026-09-17-web-animations-minimal-design.md`

pub(crate) mod keyframes;
```

- [ ] **Step 2: 실패하는 테스트를 쓴다**

`tests/unit/script/web_animation.rs`:

```rust
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
    assert_eq!(resolve_offsets(&[None, None, None]), Ok(vec![0.0, 0.5, 1.0]));
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
    assert_eq!(
        resolve_offsets(&[Some(f64::NAN)]),
        Err(KeyframeError::OffsetOutOfRange(f64::NAN))
    );
}

#[test]
fn offsets_decreasing_is_rejected() {
    assert_eq!(
        resolve_offsets(&[Some(0.6), Some(0.2)]),
        Err(KeyframeError::OffsetOutOfOrder)
    );
}
```

`tests/unit/script/lib.rs` 의 모듈 목록(알파벳 순, `textinput` 과 `timeranges` 뒤)에 추가:

```rust
#[cfg(test)]
mod web_animation;
```

- [ ] **Step 3: 테스트가 실패하는지 확인**

Run: `cargo test -p script_tests web_animation`
Expected: 컴파일 실패 — `unresolved import script::test::web_animation`

- [ ] **Step 4: 최소 구현**

`components/script/dom/animation/keyframes.rs`:

```rust
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
    let mut resolved: Vec<Option<f64>> = declared.to_vec();
    let last_index = resolved.len() - 1;
    if resolved[0].is_none() {
        resolved[0] = Some(0.0);
    }
    if resolved[last_index].is_none() {
        resolved[last_index] = Some(1.0);
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
```

`components/script/dom/mod.rs` 의 모듈 목록(알파벳 순 — `analysernode` 와 `animationevent` 사이)에 추가:

```rust
pub(crate) mod animation;
```

`components/script/test.rs` 끝에 추가:

```rust
pub mod web_animation {
    pub use crate::dom::animation::keyframes::{KeyframeError, resolve_offsets};
}
```

- [ ] **Step 5: 테스트가 통과하는지 확인**

Run: `cargo test -p script_tests web_animation`
Expected: 8개 테스트 PASS

- [ ] **Step 6: 커밋**

```bash
rustfmt --edition 2024 components/script/dom/animation/mod.rs \
        components/script/dom/animation/keyframes.rs \
        components/script/dom/mod.rs \
        components/script/test.rs \
        tests/unit/script/web_animation.rs \
        tests/unit/script/lib.rs
git diff --check
git add components/script/dom/animation/mod.rs \
        components/script/dom/animation/keyframes.rs \
        components/script/dom/mod.rs \
        components/script/test.rs \
        tests/unit/script/web_animation.rs \
        tests/unit/script/lib.rs
git commit -F - <<'EOF'
script: 키프레임 오프셋 해석 (Web Animations 1/6)

Element.animate 의 키프레임 offset 규칙을 순수 함수로 떼어 테스트한다.
마퀴(TickerContent.tsx)가 offset 없이 2 프레임을 넘기므로 균등 분배가
실제로 쓰인다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 2: 순수 옵션 해석 — duration / iterations / 속성 이름

**Files:**
- Modify: `components/script/dom/animation/keyframes.rs`
- Modify: `components/script/test.rs`
- Modify: `tests/unit/script/web_animation.rs`

**Interfaces:**
- Consumes: Task 1 의 `KeyframeError`
- Produces:
  - `pub enum IterationSpec { Finite(f64), Infinite }`
  - `pub fn resolve_duration_seconds(duration_ms: f64) -> Option<f64>` — `None` 이면 애니메이션을 만들지 않는다
  - `pub fn resolve_iterations(iterations: f64) -> Result<IterationSpec, KeyframeError>`
  - `pub fn css_property_name(idl_name: &str) -> String`

---

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`tests/unit/script/web_animation.rs` 의 `use` 를 아래로 바꾸고 테스트를 덧붙인다:

```rust
use script::test::web_animation::{
    IterationSpec, KeyframeError, css_property_name, resolve_duration_seconds, resolve_iterations,
    resolve_offsets,
};
```

```rust
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
    assert_eq!(resolve_iterations(f64::INFINITY), Ok(IterationSpec::Infinite));
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
```

- [ ] **Step 2: 테스트가 실패하는지 확인**

Run: `cargo test -p script_tests web_animation`
Expected: 컴파일 실패 — `unresolved imports ... IterationSpec, css_property_name, ...`

- [ ] **Step 3: 최소 구현**

`components/script/dom/animation/keyframes.rs` 에 덧붙인다:

```rust
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
```

`components/script/test.rs` 의 재노출을 넓힌다:

```rust
pub mod web_animation {
    pub use crate::dom::animation::keyframes::{
        IterationSpec, KeyframeError, css_property_name, resolve_duration_seconds,
        resolve_iterations, resolve_offsets,
    };
}
```

- [ ] **Step 4: 테스트가 통과하는지 확인**

Run: `cargo test -p script_tests web_animation`
Expected: 13개 테스트 PASS

- [ ] **Step 5: 커밋**

```bash
rustfmt --edition 2024 components/script/dom/animation/keyframes.rs \
        components/script/test.rs \
        tests/unit/script/web_animation.rs
git diff --check
git add components/script/dom/animation/keyframes.rs \
        components/script/test.rs \
        tests/unit/script/web_animation.rs
git commit -F - <<'EOF'
script: 애니메이션 옵션·속성 이름 해석 (Web Animations 2/6)

duration<=0 는 애니메이션을 만들지 않는다 -- stylo 의 진행도 계산이
duration 으로 나누므로 0 이면 깨진다. CSS 경로의 같은 가드와 짝이다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 3: stylo — 출처 플래그와 스크립트 애니메이션 보관함

이 태스크는 **추가만 한다.** 아무도 새 코드를 부르지 않으므로 기존 동작은 바이트 단위로 같다.

**Files:**
- Modify: `third_party/stylo/style/stylesheets/keyframes_rule.rs` (`KeyframeSelectors::from_percentage`)
- Modify: `third_party/stylo/style/servo/animation.rs` (나머지 전부)

**Interfaces:**
- Consumes: 없음
- Produces (전부 `style::animation::` 아래):
  - `pub enum AnimationOrigin { Css, Script }` — `Clone, Copy, Debug, MallocSizeOf, PartialEq`
  - `Animation` 의 새 공개 필드 `pub origin: AnimationOrigin`
  - `pub struct ScriptAnimationRequest { pub name: Atom, pub keyframes: KeyframesAnimation, pub duration: f64, pub iteration_state: KeyframesIterationState, pub fill_mode: AnimationFillMode, pub timing_function: TimingFunction }`
  - `ElementAnimationSet` 의 새 공개 필드 `pub pending_script: Vec<ScriptAnimationRequest>`
  - `pub fn ElementAnimationSet::start_script_animations<E: TElement>(&mut self, element: E, context: &SharedStyleContext, new_style: &Arc<ComputedValues>, resolver: &mut StyleResolverForElement<E>)`
  - `pub fn ElementAnimationSet::cancel_script_animation(&mut self, name: &Atom) -> bool`
  - `style::stylesheets::keyframes_rule::KeyframeSelectors::from_percentage(percentage: KeyframePercentage) -> Self`

---

- [ ] **Step 1: `KeyframeSelectors` 에 공개 생성자를 단다**

`third_party/stylo/style/stylesheets/keyframes_rule.rs`, `impl KeyframeSelectors` 안 `new_for_unit_testing` 바로 뒤:

```rust
    /// Create selectors for a single percentage. Used for keyframes that come from
    /// script (`Element.animate`) rather than from a parsed `@keyframes` rule.
    pub fn from_percentage(percentage: KeyframePercentage) -> Self {
        KeyframeSelectors(vec![KeyframeSelector {
            range_name: TimelineRangeName::None,
            percentage,
        }])
    }
```

- [ ] **Step 2: `AnimationOrigin` 을 정의하고 `Animation` 에 필드를 단다**

`third_party/stylo/style/servo/animation.rs`, `AnimationState` 의 `impl` 블록 뒤(약 113행):

```rust
/// Where an animation came from.
///
/// ***스타일이 애니메이션의 수명을 모는 자리에서 이 둘은 다르게 취급된다.*** CSS
/// 애니메이션은 `animation-name` 이 지명하는 동안만 살지만, 스크립트 애니메이션은
/// 스타일에 이름이 없으므로 같은 규칙을 적용하면 만들어지자마자 취소된다. 세 자리에서
/// `Script` 를 건너뛴다 -- `is_cancelled_in_new_style` 순회, `maybe_start_animations`
/// 의 기존 애니메이션 순회, 그리고 `matching.rs` 의 `Finished` retain.
#[derive(Clone, Copy, Debug, MallocSizeOf, PartialEq)]
pub enum AnimationOrigin {
    /// `animation-name` 이 지명해서 만들어졌다. 스타일이 수명을 쥔다.
    Css,
    /// `Element.animate()` 로 만들어졌다. 수명은 `cancel()` 과
    /// `cancel_animations_for_node` 만 끝낸다.
    Script,
}
```

`pub struct Animation` 의 `is_new` 필드 바로 앞에 추가:

```rust
    /// Where this animation came from. See [`AnimationOrigin`].
    pub origin: AnimationOrigin,
```

`maybe_start_animations` 안의 `let mut new_animation = Animation {` 리터럴에 `origin: AnimationOrigin::Css,` 를 `is_new: true,` 앞에 넣는다.

- [ ] **Step 3: 빌드**

`Animation` 구조체 리터럴은 저장소 전체에 하나뿐이다(`servo/animation.rs:1916`,
`maybe_start_animations` 안). 그 한 자리만 고치면 된다.

Run: `cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu`
Expected: 성공.

- [ ] **Step 4: 요청 구조체와 보관함을 단다**

`servo/animation.rs`, `pub struct ElementAnimationSet` 바로 앞:

```rust
/// A request from script (`Element.animate`) to create an animation.
///
/// ***2단계인 이유:*** `ComputedKeyframe::generate_for_keyframes` 는
/// `SharedStyleContext` 와 요소의 `ComputedValues` 를 요구하는데 스크립트에는 둘 다
/// 없다. 그것들은 리스타일 중에만 존재하므로, 파싱만 스크립트가 하고 계산은 CSS
/// 애니메이션이 만들어지는 바로 그 자리로 미룬다.
#[derive(Debug, MallocSizeOf)]
pub struct ScriptAnimationRequest {
    /// 합성 이름(`-servo-script-<N>`). `cancel()` 이 이것으로 자기 것을 찾는다.
    pub name: Atom,
    /// 스크립트가 넘긴 키프레임.
    pub keyframes: KeyframesAnimation,
    /// 초 단위 지속 시간. 항상 0 보다 크다(스크립트 쪽에서 걸렀다).
    pub duration: f64,
    /// 반복 상태.
    pub iteration_state: KeyframesIterationState,
    /// `fill` 옵션.
    pub fill_mode: AnimationFillMode,
    /// 프레임이 자기 것을 선언하지 않았을 때 쓰는 기본 타이밍 함수.
    pub timing_function: TimingFunction,
}
```

`ElementAnimationSet` 의 `dirty` 앞에 필드 추가:

```rust
    /// Animations requested by script that have not been computed yet.
    /// See [`ScriptAnimationRequest`].
    pub pending_script: Vec<ScriptAnimationRequest>,
```

- [ ] **Step 5: 보관함이 세트의 수명에 참여하게 한다**

`ElementAnimationSet::cancel_all_animations` 의 첫 줄을 바꾼다:

```rust
    pub fn cancel_all_animations(&mut self) {
        self.dirty = !self.animations.is_empty() || !self.pending_script.is_empty();
        self.pending_script.clear();
        for animation in self.animations.iter_mut() {
            animation.state = AnimationState::Canceled;
        }
        self.cancel_active_transitions();
    }
```

`ElementAnimationSet::is_empty` 를 바꾼다:

```rust
    /// Whether this `ElementAnimationSet` is empty, which means it doesn't
    /// hold any animations in any state.
    ///
    /// ***대기 중인 스크립트 요청도 센다.*** 세지 않으면 `do_post_reflow_update` 의
    /// `sets.retain` 이 드레인되기 전에 요청째로 세트를 지운다.
    pub fn is_empty(&self) -> bool {
        self.animations.is_empty() && self.transitions.is_empty() && self.pending_script.is_empty()
    }
```

- [ ] **Step 6: 드레인과 취소를 구현한다**

`ElementAnimationSet` 의 `update_animations_for_new_style` 바로 앞에 추가:

```rust
    /// Turn every pending script animation request into a real `Animation`.
    ///
    /// 이 함수는 `maybe_start_animations` 가 CSS 애니메이션에 하는 것과 같은 일을
    /// 하되, 값을 스타일이 아니라 요청에서 읽는다. 아래 경로(캐스케이드, 페인트측
    /// 바인딩, 타임라인)는 전부 같다.
    pub fn start_script_animations<E>(
        &mut self,
        element: E,
        context: &SharedStyleContext,
        new_style: &Arc<ComputedValues>,
        resolver: &mut StyleResolverForElement<E>,
    ) where
        E: TElement,
    {
        for request in std::mem::take(&mut self.pending_script) {
            let mut animating_properties = PropertyDeclarationIdSet::default();
            let mut number_of_animating_properties = 0;
            for property in request.keyframes.properties_changed.iter() {
                debug_assert!(property.is_animatable());
                if animating_properties.insert(property.to_physical(new_style.writing_mode)) {
                    number_of_animating_properties += 1;
                }
            }

            let computed_steps = ComputedKeyframe::generate_for_keyframes(
                element,
                &request.keyframes,
                context,
                new_style,
                request.timing_function.clone(),
                resolver,
                animating_properties,
                number_of_animating_properties,
            );

            log::warn!(
                "ANIMSCRIPTSTART name={} properties={} steps={}",
                request.name,
                number_of_animating_properties,
                computed_steps.len()
            );

            self.animations.push(Animation {
                name: request.name,
                properties_changed: request.keyframes.properties_changed.clone(),
                computed_steps,
                // 리스타일 시점의 타임라인 값. `animate()` 호출과 같은 렌더링 갱신이다.
                started_at: context.current_time_for_animations,
                duration: request.duration,
                delay: 0.,
                fill_mode: request.fill_mode,
                iteration_state: request.iteration_state,
                // ***`Pending` 이 아니라 `Running`.*** `start_pending_animations` 가
                // 승급하면서 `animationstart` CSS 이벤트를 쏘는데, 스크립트
                // 애니메이션에 그것이 나가면 안 된다.
                state: AnimationState::Running,
                direction: AnimationDirection::Normal,
                current_direction: AnimationDirection::Normal,
                number_of_animating_properties,
                origin: AnimationOrigin::Script,
                is_new: true,
            });
            self.dirty = true;
        }
    }

    /// Cancel the script animation with the given synthetic name. Returns whether
    /// anything changed.
    ///
    /// 아직 실체가 없으면 요청을 빼고, 이미 만들어졌으면 `Canceled` 로 바꾼다. 끝난
    /// (`Finished`) 애니메이션도 세트에 남아 있으므로 여기서 잡히고, 그때의 취소는
    /// 붙들고 있던 `fill: forwards` 최종 값을 푼다. 이미 `Canceled` 면 아무 일도
    /// 하지 않는다.
    pub fn cancel_script_animation(&mut self, name: &Atom) -> bool {
        let before = self.pending_script.len();
        self.pending_script.retain(|request| &request.name != name);
        if self.pending_script.len() != before {
            self.dirty = true;
            return true;
        }

        for animation in self.animations.iter_mut() {
            if &animation.name == name && animation.state != AnimationState::Canceled {
                animation.state = AnimationState::Canceled;
                self.dirty = true;
                return true;
            }
        }

        false
    }
```

- [ ] **Step 7: 수명 예외 두 자리를 판다**

`ElementAnimationSet::update_animations_for_new_style` 의 순회를 바꾼다:

```rust
        for animation in self.animations.iter_mut() {
            // ***스크립트 애니메이션은 스타일이 취소하지 않는다.*** 스타일에 합성
            // 이름이 있을 리 없으므로 이 검사를 그대로 태우면 만들어지자마자 취소된다.
            // 수명은 `cancel()` 과 `cancel_animations_for_node` 가 쥔다.
            if animation.origin == AnimationOrigin::Script {
                continue;
            }
            if animation.is_cancelled_in_new_style(new_style) {
                animation.state = AnimationState::Canceled;
            }
        }
```

`maybe_start_animations` 의 기존 애니메이션 순회에서 `Canceled` 를 건너뛰는 줄 옆에 한 줄 더:

```rust
        // If the animation was already present in the list for the node, just update its state.
        for existing_animation in animation_state.animations.iter_mut() {
            if existing_animation.state == AnimationState::Canceled {
                continue;
            }

            // 합성 이름은 스타일이 지명할 수 없으므로 아래 이름 비교에 걸릴 일이
            // 없지만, 이름이 우연히 겹쳐도 CSS 가 스크립트 애니메이션을 건드리지
            // 않도록 여기서 끊는다.
            if existing_animation.origin == AnimationOrigin::Script {
                continue;
            }

            if new_animation.name == existing_animation.name {
```

- [ ] **Step 8: 빌드**

Run: `cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu`
Expected: 성공. `start_script_animations` / `cancel_script_animation` 은 아직 호출자가 없지만 `pub` 이므로 경고가 없다.

- [ ] **Step 9: 커밋**

```bash
rustfmt --edition 2024 third_party/stylo/style/servo/animation.rs \
        third_party/stylo/style/stylesheets/keyframes_rule.rs
git diff --check
git add third_party/stylo/style/servo/animation.rs \
        third_party/stylo/style/stylesheets/keyframes_rule.rs
git commit -F - <<'EOF'
stylo: 스크립트 애니메이션 출처와 보관함 (Web Animations 3/6)

AnimationOrigin::{Css, Script} 를 붙이고, 스타일이 수명을 모는 두 자리에서
Script 를 건너뛴다. 아직 Script 애니메이션을 만드는 호출자가 없으므로 기존
동작은 그대로다.

ElementAnimationSet.pending_script 는 스크립트가 파싱한 키프레임을 담아
두는 자리다. SharedStyleContext 가 없는 스크립트에서는 ComputedKeyframe 을
만들 수 없어 리스타일까지 미룬다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 4: stylo 리스타일에서 보관함을 드레인한다

**Files:**
- Modify: `third_party/stylo/style/matching.rs:736-803`

**Interfaces:**
- Consumes: Task 3 의 `ElementAnimationSet::start_script_animations`, `AnimationOrigin`
- Produces: 리스타일마다 `pending_script` 가 비워지고 실제 `Animation` 이 된다는 보장

---

- [ ] **Step 1: 드레인을 `needs_animations_update` 게이트 밖에 넣는다**

`process_animations_for_style` 의 `if needs_animations_update { ... }` 블록 **바로 뒤**, `animation_set.update_transitions_for_new_style(...)` **앞**에 넣는다:

```rust
        // ***스크립트 애니메이션은 `needs_animations_update` 밖에서 만든다.***
        //
        // 그 게이트는 "스타일이 애니메이션 관련해서 바뀌었나" 를 묻는다.
        // `Element.animate()` 는 스타일을 바꾸지 않으므로 게이트를 통과하지 못하고,
        // 안에 두면 요청이 영영 드레인되지 않는다.
        if !animation_set.pending_script.is_empty() {
            let mut resolver = StyleResolverForElement::new(
                *self,
                context,
                RuleInclusion::All,
                PseudoElementResolution::IfApplicable,
            );

            animation_set.start_script_animations::<Self>(
                *self,
                shared_context,
                new_values,
                &mut resolver,
            );
        }
```

`use` 문을 함수 머리에서 넓힌다:

```rust
        use crate::animation::{AnimationOrigin, AnimationSetKey, AnimationState};
```

- [ ] **Step 2: `Finished` retain 에 출처 예외를 넣는다**

`animation_set.animations.retain(...)` 을 바꾼다. 기존 주석은 **그대로 둔다**(그 주석이 이 retain 이 존재하는 이유다) — 클로저 본문만 바꾸고 위에 한 문단 덧붙인다:

```rust
        // 스크립트 애니메이션은 스타일이 지명할 수 없으므로 아래 조건에 영영 걸리지
        // 않는다. 예외를 두지 않으면 끝나는 즉시 지워지고, 그러면 위 2번(검은 화면)이
        // 스크립트 애니메이션에서 그대로 재현된다.
        animation_set.animations.retain(|animation| {
            animation.origin == AnimationOrigin::Script ||
                animation.state != AnimationState::Finished ||
                new_values
                    .get_ui()
                    .animation_name_iter()
                    .any(|name| name.as_atom() == Some(&animation.name))
        });
```

- [ ] **Step 3: 빌드**

Run: `cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu`
Expected: 성공.

실패 케이스와 대응:
- `StyleResolverForElement` / `RuleInclusion` / `PseudoElementResolution` 미해결 → 이미 같은 함수의 `needs_animations_update` 블록에서 쓰고 있으니 파일 상단 `use` 에 있다. 없다면 그 블록에서 쓰는 경로를 그대로 복사한다.
- `context` 를 `&mut` 로 빌리는 것과 `shared_context` 충돌 → `StyleContext.shared` 는 `&'a SharedStyleContext<'a>` 라 필드에서 **참조가 복사**된다. 기존 블록이 같은 조합으로 컴파일되므로 문제가 되지 않는다.

- [ ] **Step 4: 기존 동작이 그대로인지 확인**

Run: `cargo test -p script_tests web_animation`
Expected: 13개 PASS (회귀 없음 확인용)

- [ ] **Step 5: 커밋**

```bash
rustfmt --edition 2024 third_party/stylo/style/matching.rs
git diff --check
git add third_party/stylo/style/matching.rs
git commit -F - <<'EOF'
stylo: 리스타일에서 스크립트 애니메이션 요청을 드레인 (Web Animations 4/6)

needs_animations_update 게이트 밖이다. Element.animate 는 스타일을 바꾸지
않으므로 그 게이트를 통과하지 못한다.

Finished retain 에도 출처 예외를 둔다. 스타일이 합성 이름을 지명할 수 없어
그대로 두면 끝나는 즉시 지워지고, fill: forwards 최종 값이 버려져 애니메이션
종료 후 검은 화면이 재현된다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 5: 스크립트 표면 — pref, WebIDL, `Animation` 객체, `Element.animate`

**Files:**
- Modify: `components/config/prefs.rs`
- Create: `components/script_bindings/webidls/Animation.webidl`
- Modify: `components/script_bindings/webidls/Element.webidl`
- Modify: `components/script_bindings/codegen/Bindings.conf`
- Create: `components/script/dom/animation/animation.rs`
- Modify: `components/script/dom/animation/mod.rs`
- Modify: `components/script/dom/animation/keyframes.rs` (JS 값을 받는 빌더)
- Modify: `components/script/dom/element/element.rs`
- Modify: `components/script/animations.rs`

**Interfaces:**
- Consumes: Task 1·2 의 순수 함수 전부, Task 3 의 `ScriptAnimationRequest` / `AnimationOrigin`
- Produces:
  - `crate::dom::animation::Animation` — DOM 객체, `new(window, target: &Node, name: Atom, can_gc) -> DomRoot<Animation>`
  - `Animations::next_script_animation_name(&self) -> Atom`
  - `Animations::add_script_animation(&self, key: AnimationSetKey, request: ScriptAnimationRequest)`
  - `Animations::cancel_script_animation(&self, key: &AnimationSetKey, name: &Atom) -> bool`
  - `keyframes::build_keyframes_animation(...) -> Result<(KeyframesAnimation, TimingFunction), KeyframeError>`

---

- [ ] **Step 1: pref 를 추가한다**

`components/config/prefs.rs`, `pub dom_wakelock_enabled: bool,` 근처의 알파벳 위치(`dom_webgpu_*` 앞)에:

```rust
    /// Web Animations 최소 구현 — `Element.animate()` 와 `Animation.cancel()`.
    ///
    /// 꺼져 있으면 메서드가 정의되지 않고, 페이지의
    /// `typeof el.animate === 'function'` 검사가 실패해 지금의 CSS `@keyframes`
    /// 폴백이 그대로 돈다. 같은 배포본으로 A/B 가 된다.
    /// 설계: `docs/superpowers/specs/2026-09-17-web-animations-minimal-design.md`
    pub dom_web_animations_enabled: bool,
```

같은 파일의 `Default` 구현에서 인접한 `dom_` 항목 옆에:

```rust
            dom_web_animations_enabled: false,
```

- [ ] **Step 2: WebIDL 을 쓴다**

`components/script_bindings/webidls/Animation.webidl` (신규):

```webidl
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://drafts.csswg.org/web-animations-1/#the-animation-interface
//
// Servo 최소 구현. 재생 제어(play/pause/reverse/finish), 프라미스
// (ready/finished), 이벤트(finish/cancel/remove), playState 는 없다.
// 범위: docs/superpowers/specs/2026-09-17-web-animations-minimal-design.md
[Pref="dom_web_animations_enabled", Exposed=Window]
interface Animation {
  undefined cancel();
};

// https://drafts.csswg.org/web-animations-1/#the-effecttiming-dictionaries
enum FillMode { "none", "forwards", "backwards", "both", "auto" };
```

`components/script_bindings/webidls/Element.webidl` 끝(마지막 `partial interface Element` 뒤)에:

```webidl
// https://drafts.csswg.org/web-animations-1/#the-animatable-interface-mixin
//
// Servo 최소 구현. 키프레임은 배열 형태만 받고, 값은 전부 문자열로 강제된다
// (`offset: 0.5` 는 "0.5" 로 들어와 우리가 파싱한다). 옵션은 아래 넷뿐이며,
// 선언되지 않은 멤버는 WebIDL 이 무시하므로 이 딕셔너리가 곧 지원 범위다.
// 범위: docs/superpowers/specs/2026-09-17-web-animations-minimal-design.md
dictionary ServoKeyframeAnimationOptions {
  unrestricted double duration = 0;
  DOMString easing = "linear";
  unrestricted double iterations = 1;
  FillMode fill = "auto";
};

partial interface Element {
  [Pref="dom_web_animations_enabled", Throws]
  Animation animate(sequence<record<DOMString, DOMString>> keyframes,
                    optional ServoKeyframeAnimationOptions options = {});
};
```

`components/script_bindings/codegen/Bindings.conf` 의 `'Element'` 항목 `'cx'` 배열 끝에 `'Animate'` 를 넣는다. 코드젠이 `cx: &mut JSContext` 를 첫 인자로 넘기고, 그것으로 `CanGc::from_cx(cx)` 를 만든다.

- [ ] **Step 3: DOM `Animation` 객체를 쓴다**

`components/script/dom/animation/animation.rs` (신규):

```rust
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use style::animation::AnimationSetKey;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::AnimationBinding::AnimationMethods;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::node::{Node, NodeDamage, NodeTraits};
use crate::dom::window::Window;
use crate::script_runtime::CanGc;

/// <https://drafts.csswg.org/web-animations-1/#the-animation-interface>
///
/// 대상 요소와 합성 이름만 들고 있는 얇은 핸들이다. 상태는 전부
/// `ElementAnimationSet` 에 있고, 이 객체는 그것을 찾아가는 열쇠일 뿐이다.
#[dom_struct]
pub(crate) struct Animation {
    reflector_: Reflector,

    /// 애니메이션이 붙은 노드.
    target: Dom<Node>,

    /// `-servo-script-<N>`. 페이지의 `@keyframes` 이름과 충돌할 수 없다.
    #[no_trace]
    name: Atom,
}

impl Animation {
    fn new_inherited(target: &Node, name: Atom) -> Animation {
        Animation {
            reflector_: Reflector::new(),
            target: Dom::from_ref(target),
            name,
        }
    }

    pub(crate) fn new(
        window: &Window,
        target: &Node,
        name: Atom,
        can_gc: CanGc,
    ) -> DomRoot<Animation> {
        reflect_dom_object(
            Box::new(Animation::new_inherited(target, name)),
            window,
            can_gc,
        )
    }
}

impl AnimationMethods<crate::DomTypeHolder> for Animation {
    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-cancel>
    ///
    /// 마퀴가 ResizeObserver 재시작마다 부르므로 반복 호출과 이미 끝난 애니메이션에
    /// 대한 호출이 흔하다. 둘 다 터지지 않아야 한다.
    fn Cancel(&self) {
        let target = DomRoot::from_ref(&*self.target);
        let key = AnimationSetKey::new_for_non_pseudo(target.to_opaque());
        let document = target.owner_document();
        if document
            .animations()
            .cancel_script_animation(&key, &self.name)
        {
            target.dirty(NodeDamage::Style);
        }
    }
}
```

`components/script/dom/animation/mod.rs` 를 바꾼다:

```rust
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Web Animations 최소 구현.
//!
//! 설계: `docs/superpowers/specs/2026-09-17-web-animations-minimal-design.md`

pub(crate) use self::animation::*;

#[allow(clippy::module_inception, reason = "The interface name is animation")]
pub(crate) mod animation;
pub(crate) mod keyframes;
```

- [ ] **Step 4: 키프레임 빌더를 쓴다**

`components/script/dom/animation/keyframes.rs` 머리에 `use` 를 추가하고 파일 끝에 빌더를 덧붙인다:

```rust
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

use crate::dom::bindings::str::DOMString;
```

```rust
/// `Element.animate()` 가 넘긴 키프레임 배열을 stylo 의 `KeyframesAnimation` 으로 옮긴다.
///
/// 반환하는 `TimingFunction` 은 전체 기본 타이밍 함수(옵션의 `easing`)다. 프레임이
/// 자기 `easing` 을 선언했으면 그 프레임의 선언 블록에
/// `animation-timing-function` 으로 들어가고, `KeyframesStep` 이 그것을 감지해
/// 기본값을 덮는다 -- CSS `@keyframes` 안에 같은 속성을 쓴 것과 같은 경로다.
///
/// `url` / `quirks_mode` 는 CSS 값 파싱에 쓰이고, `lock` 은 선언 블록을 감싸는 데
/// 쓰인다(문서의 저자 스타일 잠금).
pub fn build_keyframes_animation(
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
    let timing_function =
        parse_timing_function(global_easing, &url_data, quirks_mode).unwrap_or_else(|| {
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
```

`keyframes.rs` 의 순수 함수 쪽(Task 1·2)은 위 `use` 가 필요 없다. 새 import 는 전부
이 빌더가 쓴다.

- [ ] **Step 5: `Animations` 에 스크립트 애니메이션 진입점을 단다**

`components/script/animations.rs`:

`use` 를 넓힌다:

```rust
use style::animation::{
    Animation, AnimationOrigin, AnimationSetKey, AnimationState, DocumentAnimationSet,
    ElementAnimationSet, KeyframesIterationState, ScriptAnimationRequest, Transition,
};
use stylo_atoms::Atom;
```

`Animations` 구조체에 필드를 단다(`timeline_value_at_last_dirty` 뒤):

```rust
    /// `Element.animate()` 로 만든 애니메이션에 붙일 다음 합성 이름의 번호.
    script_animation_counter: Cell<u64>,
```

`Animations::new()` 에 `script_animation_counter: Cell::new(0),` 를 더한다.

`clear()` 뒤에 세 메서드를 더한다:

```rust
    /// `Element.animate()` 가 만든 애니메이션에 붙일 이름.
    ///
    /// 페이지의 `@keyframes` 이름과 충돌할 수 없는 모양이어야 한다 -- 충돌하면
    /// `maybe_start_animations` 가 같은 이름의 CSS 애니메이션으로 착각한다.
    pub(crate) fn next_script_animation_name(&self) -> Atom {
        let index = self.script_animation_counter.get() + 1;
        self.script_animation_counter.set(index);
        Atom::from(format!("-servo-script-{index}"))
    }

    /// 스크립트 애니메이션 요청을 세트에 적재한다. 실제 애니메이션은 다음
    /// 리스타일에서 만들어진다(`ElementAnimationSet::start_script_animations`).
    pub(crate) fn add_script_animation(
        &self,
        key: AnimationSetKey,
        request: ScriptAnimationRequest,
    ) {
        let mut sets = self.sets.sets.write();
        let set = sets.entry(key).or_default();
        set.pending_script.push(request);
        set.dirty = true;
    }

    /// `Animation.cancel()`. 무언가 바뀌었으면 `true`.
    pub(crate) fn cancel_script_animation(&self, key: &AnimationSetKey, name: &Atom) -> bool {
        let mut sets = self.sets.sets.write();
        let Some(set) = sets.get_mut(key) else {
            return false;
        };
        set.cancel_script_animation(name)
    }
```

`add_animation_event` 의 첫 줄에 가드를 넣는다:

```rust
    fn add_animation_event(
        &self,
        key: &AnimationSetKey,
        animation: &Animation,
        event_type: TransitionOrAnimationEventType,
        now: f64,
        pipeline_id: PipelineId,
    ) {
        // ***스크립트 애니메이션은 CSS 애니메이션 이벤트를 쏘지 않는다.***
        // `animationstart`/`animationend` 의 `animationName` 은 합성 이름이라
        // 페이지에 의미가 없고, 폴백 경로가 그 이벤트를 듣고 있다면 오작동한다.
        // 시작은 `AnimationState::Running` 으로 만들어 승급 경로를 피하고, 끝과
        // 취소는 여기서 끊는다.
        if animation.origin == AnimationOrigin::Script {
            return;
        }

        let iteration_index = match animation.iteration_state {
```

`root_newly_animating_dom_nodes` 의 조건을 넓힌다:

```rust
            if set.animations.iter().any(|animation| animation.is_new) ||
                set.transitions.iter().any(|transition| transition.is_new) ||
                !set.pending_script.is_empty()
            {
```

> 왜 필요한가: 문서에 붙지 않았거나 렌더링되지 않는 요소에 `animate()` 를 걸면 요청이
> 드레인되지 않는다. `is_empty()` 가 `pending_script` 를 세므로 `sets.retain` 도 그
> 세트를 지우지 않는다. 노드를 루팅해 두면 다음 `do_post_reflow_update` 의 "렌더링되지
> 않음" 검사가 그것을 잡아 `cancel_all_animations()` 로 요청을 비우고, 그다음
> `sets.retain` 이 세트를 지운다. 누수 창은 렌더링 갱신 한 번이다.

- [ ] **Step 6: `Element.animate` 를 구현한다**

`components/script/dom/element/element.rs` 의 `impl ElementMethods<crate::DomTypeHolder> for Element` 안(파일 끝 근처, 다른 메서드 옆)에:

```rust
    /// <https://drafts.csswg.org/web-animations-1/#dom-animatable-animate>
    ///
    /// Servo 최소 구현. 범위:
    /// `docs/superpowers/specs/2026-09-17-web-animations-minimal-design.md`
    fn Animate(
        &self,
        cx: &mut JSContext,
        keyframes: Vec<Record<DOMString, DOMString>>,
        options: &ServoKeyframeAnimationOptions,
    ) -> Fallible<DomRoot<Animation>> {
        let can_gc = CanGc::from_cx(cx);
        let node = self.upcast::<Node>();
        let window = self.owner_window();
        let document = self.owner_document();

        let iterations = resolve_iterations(options.iterations)
            .map_err(|_| Error::Type("iterations must be a non-negative number".to_owned()))?;

        let (keyframes_animation, timing_function) = build_keyframes_animation(
            &keyframes,
            &options.easing.str(),
            &document.base_url(),
            document.quirks_mode(),
            document.style_shared_author_lock(),
        )
        .map_err(|error| match error {
            KeyframeError::OffsetOutOfRange(_) => {
                Error::Type("keyframe offset must be in [0, 1]".to_owned())
            },
            KeyframeError::OffsetOutOfOrder => {
                Error::Type("keyframe offsets must be non-decreasing".to_owned())
            },
            KeyframeError::InvalidIterations => {
                Error::Type("iterations must be a non-negative number".to_owned())
            },
        })?;

        let name = document.animations().next_script_animation_name();

        // ***duration 이 0 이하면 애니메이션을 만들지 않는다.*** stylo 의 진행도
        // 계산이 duration 으로 나눈다. 핸들은 정상 반환하되 아무것도 붙들지 않는다.
        if let Some(duration) = resolve_duration_seconds(options.duration) {
            let iteration_state = match iterations {
                IterationSpec::Infinite => KeyframesIterationState::Infinite(0.0),
                IterationSpec::Finite(count) => KeyframesIterationState::Finite(0.0, count),
            };

            let fill_mode = match options.fill {
                FillMode::Forwards => AnimationFillMode::Forwards,
                FillMode::Backwards => AnimationFillMode::Backwards,
                FillMode::Both => AnimationFillMode::Both,
                // `auto` 는 이 범위에서 `none` 과 같다 (KeyframeEffect 가 없으므로
                // 명세가 말하는 "effect 의 fill 로 해석" 할 대상이 없다).
                FillMode::None | FillMode::Auto => AnimationFillMode::None,
            };

            document.animations().add_script_animation(
                AnimationSetKey::new_for_non_pseudo(node.to_opaque()),
                ScriptAnimationRequest {
                    name: name.clone(),
                    keyframes: keyframes_animation,
                    duration,
                    iteration_state,
                    fill_mode,
                    timing_function,
                },
            );

            // 요청을 드레인하려면 이 요소가 리스타일되어야 한다.
            node.dirty(NodeDamage::Style);
        }

        Ok(Animation::new(&window, node, name, can_gc))
    }
```

`element.rs` 의 `use` 에 아래를 더한다(기존 import 블록의 알파벳 위치에 맞춘다). `Error`,
`Fallible`, `CanGc`, `JSContext`, `Node`, `NodeDamage`, `QuirksMode`, `DomRoot` 은 이미
있으므로 다시 넣지 않는다. `Animation` 이라는 이름은 이 파일에 아직 없어 충돌하지 않는다.

```rust
use script_bindings::record::Record;
use style::animation::{AnimationSetKey, KeyframesIterationState, ScriptAnimationRequest};
use style::properties::longhands::animation_fill_mode::computed_value::single_value::T as AnimationFillMode;

use crate::dom::animation::Animation;
use crate::dom::animation::keyframes::{
    IterationSpec, KeyframeError, build_keyframes_animation, resolve_duration_seconds,
    resolve_iterations,
};
use crate::dom::bindings::codegen::Bindings::AnimationBinding::FillMode;
use crate::dom::bindings::codegen::Bindings::ElementBinding::ServoKeyframeAnimationOptions;
```

`keyframes.rs` 의 순수 함수들은 `pub` 이지만 모듈이 `pub(crate)` 이므로 크레이트 밖으로 새지 않는다.

- [ ] **Step 7: 빌드**

Run: `cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu`
Expected: 성공.

흔한 실패와 대응:
- **`Animate` 트레이트 시그니처 불일치** — 코드젠이 만든 `ElementMethods` 의 `fn Animate` 선언을 `target/debug/build/script_bindings-*/out/Bindings/ElementBinding.rs` 에서 찾아 그대로 맞춘다. `Bindings.conf` 의 `cx` 목록에 `Animate` 가 있으면 첫 인자가 `cx: &mut JSContext` 다.
- **`FillMode` 변형 이름** — 코드젠이 만든 `AnimationBinding::FillMode` 의 실제 변형 이름을 확인해 맞춘다.
- **`to_computed_value_without_context` 미해결** — `style::values::specified::easing::TimingFunction` 의 인허런트 메서드다. 그 경로로 import 한다.
- **`document.base_url()` 반환형** — `UrlExtraData(url.get_arc())` 가 맞지 않으면 `cssstyledeclaration.rs:363` 의 형태를 그대로 따른다.

- [ ] **Step 8: 순수 테스트가 그대로 도는지 확인**

Run: `cargo test -p script_tests web_animation`
Expected: 13개 PASS

- [ ] **Step 9: pref OFF 무해성을 실기로 확인**

Run: winit_wall 을 **아무 pref 변경 없이** 평소대로 띄우고 `/output` 페이지를 연다.

Expected:
- 개발자 콘솔에서 `typeof document.body.animate` → `"undefined"`
- 로그에 `ANIMSTART name=sd-anim-N` 이 계속 나온다(페이지가 CSS 폴백을 탄다는 뜻)
- 로그에 `ANIMSCRIPTSTART` 가 **한 줄도 없다**
- 전환 동작이 이 작업 전과 같다

이것이 통과하지 않으면 다음 단계로 넘어가지 않는다. 기본값이 OFF 인 변경의 실질 영향은 0이어야 한다.

- [ ] **Step 10: 커밋**

```bash
rustfmt --edition 2024 components/config/prefs.rs \
        components/script/dom/animation/mod.rs \
        components/script/dom/animation/animation.rs \
        components/script/dom/animation/keyframes.rs \
        components/script/dom/element/element.rs \
        components/script/animations.rs
git diff --check
git add components/config/prefs.rs \
        components/script_bindings/webidls/Animation.webidl \
        components/script_bindings/webidls/Element.webidl \
        components/script_bindings/codegen/Bindings.conf \
        components/script/dom/animation/mod.rs \
        components/script/dom/animation/animation.rs \
        components/script/dom/animation/keyframes.rs \
        components/script/dom/element/element.rs \
        components/script/animations.rs
git commit -F - <<'EOF'
script: Element.animate 와 Animation.cancel (Web Animations 5/6)

dom_web_animations_enabled pref 뒤에 둔다. 기본 OFF -- 꺼져 있으면 메서드가
정의되지 않고 페이지의 typeof 검사가 실패해 CSS 폴백이 그대로 돈다.

키프레임은 sequence<record<DOMString, DOMString>> 로 받는다. WebIDL 이 값을
전부 문자열로 강제하므로 offset: 0.5 는 "0.5" 로 들어오고, opacity: 0 은
"0" -- 그대로 유효한 CSS 값이다.

스크립트 애니메이션은 CSS 애니메이션 이벤트를 쏘지 않는다. 시작은
AnimationState::Running 으로 만들어 승급 경로를 피하고, 끝과 취소는
add_animation_event 에서 끊는다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

### Task 6: 실기 검증과 문서

**Files:**
- Create: `tests/html/web_animations_minimal.html`
- Modify: `docs/multigpu/wall_transition_prereplay.md` (결론 갱신)

**Interfaces:**
- Consumes: Task 5 의 전체 표면
- Produces: pref ON 에서의 판정 결과

---

- [ ] **Step 1: 로컬 확인용 페이지를 쓴다**

`tests/html/web_animations_minimal.html`:

```html
<!DOCTYPE html>
<meta charset="utf-8">
<title>Web Animations 최소 구현 확인</title>
<style>
  body { margin: 0; background: #111; color: #eee; font: 14px monospace; }
  .box { width: 120px; height: 120px; background: #4af; margin: 20px; }
  #log { white-space: pre; padding: 20px; }
</style>
<div class="box" id="entrance"></div>
<div class="box" id="marquee" style="background:#fa4"></div>
<div id="log"></div>
<script>
const log = (line) => { document.getElementById('log').textContent += line + '\n'; };

log('typeof Element.prototype.animate = ' + typeof Element.prototype.animate);
if (typeof Element.prototype.animate !== 'function') {
  log('pref OFF -- 여기까지가 정상이다.');
} else {
  // 1. offset 을 명시한 등장 애니메이션 (useBoardHandover 등장/퇴장과 같은 모양)
  document.getElementById('entrance').animate([
    { offset: 0, transformOrigin: '0% 0%', transform: 'translate(-100%, 0%) scale(0.8)', opacity: 0 },
    { offset: 0.6, transform: 'translate(0%, 0%) scale(1)', opacity: 1, easing: 'ease-out' },
    { offset: 1, transform: 'translate(0%, 0%) scale(1)', opacity: 1 },
  ], { duration: 1200, easing: 'linear', iterations: 1, fill: 'forwards' });

  // 2. offset 없는 무한 마퀴 (TickerContent.tsx:116 과 같은 모양)
  const marquee = document.getElementById('marquee').animate([
    { transform: 'translateX(0px)' },
    { transform: 'translateX(400px)' },
  ], { duration: 2000, iterations: Infinity, easing: 'linear' });

  // 3. cancel() 은 반복 호출과 사후 호출에도 터지지 않아야 한다.
  setTimeout(() => { marquee.cancel(); marquee.cancel(); log('cancel x2 OK'); }, 3000);

  // 4. 거절되어야 하는 입력
  const boom = (label, fn) => {
    try { fn(); log(label + ': 거절되지 않음 -- FAIL'); }
    catch (error) { log(label + ': ' + error.name); }
  };
  boom('offset 범위 밖', () => document.body.animate(
    [{ offset: 0, opacity: 0 }, { offset: 1.5, opacity: 1 }], { duration: 100 }));
  boom('offset 역순', () => document.body.animate(
    [{ offset: 0.8, opacity: 0 }, { offset: 0.2, opacity: 1 }], { duration: 100 }));
  boom('iterations 음수', () => document.body.animate(
    [{ opacity: 0 }, { opacity: 1 }], { duration: 100, iterations: -1 }));

  // 5. duration 0 은 예외가 아니라 "아무것도 안 만든다"
  const inert = document.body.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 0 });
  inert.cancel();
  log('duration 0 OK');

  // 6. 빈 키프레임 배열도 예외가 아니다 -- 스텝 없는 애니메이션이라 값이 안 나온다.
  const empty = document.body.animate([], { duration: 100 });
  empty.cancel();
  log('빈 키프레임 OK');

  // 7. 알 수 없는 속성과 파싱 실패한 값은 조용히 버려진다(예외 아님).
  const junk = document.body.animate([
    { opacity: 0, notAProperty: 'x', transform: 'definitely-not-a-transform' },
    { opacity: 1 },
  ], { duration: 100 });
  junk.cancel();
  log('알 수 없는 속성 OK');
}
</script>
```

- [ ] **Step 2: pref ON 으로 로컬 페이지를 확인한다**

Run: `winit_wall` 을 `-Pref dom_web_animations_enabled=true` 로 띄우고 위 페이지를 연다.

Expected:
- `typeof Element.prototype.animate = function`
- 파란 상자가 왼쪽 밖에서 들어와 제자리에 **머문다**(`fill: forwards`)
- 주황 상자가 3초간 계속 스크롤하다가 `cancel x2 OK` 와 함께 멈춘다
- `offset 범위 밖: TypeError`, `offset 역순: TypeError`, `iterations 음수: TypeError`
- `duration 0 OK`, `빈 키프레임 OK`, `알 수 없는 속성 OK` — 이 셋은 **예외가 아니어야** 한다
- 로그에 `ANIMSCRIPTSTART name=-servo-script-1 properties=3 steps=3` 같은 줄이 보인다
- 로그에 `ANIMSTART`(CSS 애니메이션 시작)와 `animationstart` 이벤트가 이 페이지에 대해 **없다**

- [ ] **Step 3: pref ON 으로 `/output` 실기 검증**

Run: 배포본을 `-Pref dom_web_animations_enabled=true -NumaNode 1` 로 띄우고 네 구성을 두 바퀴 순회시킨다.

판정표:

| 항목 | 기대 | 확인 방법 |
|---|---|---|
| 경로가 바뀌었나 | `ANIMSTART name=sd-anim-N` 이 사라지고 `ANIMSCRIPTSTART name=-servo-script-N` 이 나온다 | 로그 `grep -c` |
| **전환 대상 구성 사전 재생** | **사라짐** (이 작업의 목적) | 육안 |
| 애니메이션 종료 후 검은 화면 | 없음 | 육안 |
| 마퀴 무한 스크롤 | 정상 | 육안 |
| 등장/퇴장 모양 | 지금과 동일 | 육안 |
| `PAINTANIM built tx_bound` | **0 아님** | 로그 |
| `SCRIPTBUSY reflow_display`·`reflow_ms` | 변화 없음 | 로그, pref OFF 런과 비교 |
| `WRRATE` fps | 변화 없음 | 로그, pref OFF 런과 비교 |

★`tx_bound` 가 0이면 스크립트 애니메이션이 페인트측에 묶이지 않은 것이고, 그러면 스크립트
정체(`SCRIPTBUSY longest_ms=285~511`) 중에 얼어붙는다. 안 1을 택한 근거가 바로 그것이므로
0이면 여기서 멈추고 원인을 찾는다.★

- [ ] **Step 4: 조사 문서의 결론을 갱신한다**

`docs/multigpu/wall_transition_prereplay.md` 의 `## 3. 왜 엔진 수정 대상이 아닌가` 끝, 근본 해법 목록 뒤에 추가:

```markdown
★2026-09-17: 위 두 갈래 중 **엔진** 쪽을 택했다.★ `Element.animate` 최소 구현 —
설계 `docs/superpowers/specs/2026-09-17-web-animations-minimal-design.md`,
계획 `docs/superpowers/plans/2026-09-17-web-animations-minimal.md`.
`dom_web_animations_enabled` pref 뒤에 있고 기본값은 OFF 다.
```

- [ ] **Step 5: 커밋**

```bash
git add tests/html/web_animations_minimal.html \
        docs/multigpu/wall_transition_prereplay.md
git commit -F - <<'EOF'
docs: Web Animations 확인 페이지와 조사 문서 결론 (Web Animations 6/6)

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## 비목표 (이 계획이 만들지 않는 것)

`getAnimations()`, `Animation` 의 재생 제어(`play`/`pause`/`reverse`/`finish`)·프라미스
(`ready`/`finished`)·이벤트(`finish`/`cancel`/`remove`)·`playState`·`currentTime`,
객체 형태 키프레임, `delay`/`direction`/`iterationStart`/`endDelay`/`composite`,
`KeyframeEffect`·`AnimationEffect` 노출, 의사 요소 대상, WPT 적합성,
그리고 **pref 기본값을 ON 으로 뒤집는 것**(별도 커밋).

# `<iframe toplevel>` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `<iframe toplevel>` 이 붙은 iframe 의 내용을 **부모의 디스플레이 리스트 안에서 그대로 렌더하면서**(모든 CSS 적용) browsing context 만 top-level 로 만들어, `X-Frame-Options` · CSP `frame-ancestors` · JS frame-busting 이 성립하지 않게 한다.

**Architecture:** `<iframe>` 이 겸하던 두 축 — 렌더링 중첩(layout 이 자식 파이프라인을 부모 디스플레이 리스트에 꽂는 것)과 context 중첩(child navigable 이 되는 것) — 을 분리한다. layout 은 건드리지 않는다. `BrowsingContext` 에 `embedding_mode` 를 두고, `NewPipelineInfo.parent_info` 를 순수 함수 `navigable_parent()` 로만 채운다. `parent_info` 가 `None` 이면 `WindowProxy` 에 부모가 없어져 프레이밍 검사가 첫 단계에서 빠져나간다.

**Tech Stack:** Rust (Servo 포크), 크레이트 `servo-constellation-traits` / `servo-script` / `servo-constellation` / `servo-config`. 빌드는 vendored 툴체인 + `no-wgl` 피처.

**Spec:** `docs/superpowers/specs/2026-08-19-toplevel-embed-design.md`

## Global Constraints

- **브랜치**: `wall-toplevel-embed` (이미 생성됨, `wall-iframe-framing-policy` 에서 분기). 새 브랜치를 만들지 않는다.
- **빌드 환경**: 모든 cargo/rustfmt 명령 전에 `. W:\scripts\servo_env.ps1` 를 dot-source 한다. 저장소 경로는 `W:\servo_multigpu-tiled-wall` (긴 경로 회피용 subst 드라이브).
- **빌드 명령**: `cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl --manifest-path W:\servo_multigpu-tiled-wall\Cargo.toml`. ★`no-wgl` 없이 `-p servo-paint` 등 단독 체크는 `servo-webgl` 에서 실패한다 — 기존 제약이며 당신의 변경 탓이 아니다.★
- **커밋 메시지**: 한국어. ★**큰따옴표(`"`) 금지**★ — PowerShell here-string 함정. 인용은 백틱이나 홑따옴표로 하고, 커밋 직후 `git log -1 --format=%B | grep -c '"'` 가 `0` 인지 직접 확인한다.
- **`git add` 는 파일 경로를 명시한다.** `-A` 나 `.` 를 쓰지 않는다 — 작업트리에 이 작업과 무관한 수정/미추적 파일이 다수 있다.
- **커밋 트레일러**: 모든 커밋 메시지 끝에 아래 두 줄.
  ```
  Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_011gaj7C2AhzcjFdu9nw2HGN
  ```
- **pref 이름은 밑줄 표기다.** `--pref dom_iframe_toplevel_embed_enabled=true` 가 맞고 점 표기는 틀리다. 모르는 pref 이름은 조용히 무시되지 않고 `Unknown preference` 로 즉시 패닉한다.
- **`rustfmt --edition 2024 --check` 를 완료 조건으로 쓰지 않는다.** 이 포크의 여러 upstream 파일이 이미 비준수다(`components/net/fetch/methods.rs` 는 변경 전 기준 16 블록). **변경분이 그 파일의 기존 스타일을 따르는지**로 판단한다.

---

## File Structure

| 파일 | 책임 | 작업 |
|---|---|---|
| `components/shared/constellation/from_script_message.rs` | `EmbeddingMode`, `navigable_parent()`, `IFrameLoadInfo.embedding_mode` — 두 크레이트가 공유하는 규칙의 정본 | Task 1, 3 |
| `components/config/prefs.rs` | pref `dom_iframe_toplevel_embed_enabled` | Task 2 |
| `components/script/dom/html/htmliframeelement.rs` | 속성 + pref 판정, 초기 about:blank 파이프라인의 `parent_info` | Task 3 |
| `components/constellation/browsingcontext.rs` | `BrowsingContext.embedding_mode` 저장, `parent_pipeline_id` 의 의미 문서화 | Task 4 |
| `components/constellation/constellation.rs` | BC 생성 시 mode 확정, 내비게이션 파이프라인의 `parent_info` 파생 | Task 4, 5 |
| `tests/html/toplevel_embed_probe.html` | 대조군/처리군 + CSS 매트릭스 프로브 | Task 6 |
| `docs/multigpu/configuration.md` | 설정 정본 표 | Task 7 |

Task 1→2→3→4→5 는 순서 의존이다. Task 6, 7 은 5 이후 아무 때나.

---

### Task 1: `EmbeddingMode` 와 `navigable_parent()` — 규칙의 정본

**Files:**
- Modify: `components/shared/constellation/from_script_message.rs` (파일 끝에 추가)

**Interfaces:**
- Consumes: 없음 (첫 작업)
- Produces:
  - `pub enum EmbeddingMode { Nested, TopLevelEmbed }` — `Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize`
  - `pub fn navigable_parent(mode: EmbeddingMode, presentation_parent: Option<PipelineId>) -> Option<PipelineId>`
  - 둘 다 `servo_constellation_traits::` 에서 바로 쓸 수 있다 (`lib.rs` 가 `pub use from_script_message::*`).

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`components/shared/constellation/from_script_message.rs` 맨 끝에 추가:

```rust
#[cfg(test)]
mod tests {
    use servo_base::id::{PipelineId, PipelineNamespace, PipelineNamespaceId};

    use super::{EmbeddingMode, navigable_parent};

    /// 평범한 iframe 은 표출 부모가 곧 navigable 부모다.
    #[test]
    fn nested_keeps_the_presentation_parent() {
        PipelineNamespace::install(PipelineNamespaceId(1));
        let parent = PipelineId::new();

        assert_eq!(
            navigable_parent(EmbeddingMode::Nested, Some(parent)),
            Some(parent)
        );
    }

    /// `<iframe toplevel>` 은 표출 부모가 있어도 navigable 부모가 없다. 이 한 값이
    /// `WindowProxy` 의 부모를 정하고, 그래서 X-Frame-Options / frame-ancestors /
    /// `top !== self` 프레임버스팅이 전부 성립하지 않게 된다.
    #[test]
    fn toplevel_embed_has_no_navigable_parent() {
        PipelineNamespace::install(PipelineNamespaceId(2));
        let parent = PipelineId::new();

        assert_eq!(navigable_parent(EmbeddingMode::TopLevelEmbed, Some(parent)), None);
    }

    /// 진짜 최상위 문서는 어느 모드에서도 부모가 없다.
    #[test]
    fn a_real_top_level_document_has_no_parent_either_way() {
        assert_eq!(navigable_parent(EmbeddingMode::Nested, None), None);
        assert_eq!(navigable_parent(EmbeddingMode::TopLevelEmbed, None), None);
    }
}
```

- [ ] **Step 2: 테스트가 실패하는 것을 확인한다**

Run:
```powershell
. W:\scripts\servo_env.ps1
cargo test -p servo-constellation-traits --manifest-path W:\servo_multigpu-tiled-wall\Cargo.toml
```
Expected: 컴파일 실패. `cannot find type EmbeddingMode` / `cannot find function navigable_parent`.

- [ ] **Step 3: 최소 구현을 넣는다**

같은 파일에서 `#[cfg(test)] mod tests` **바로 앞**에 추가:

```rust
/// `<iframe>` 이 겸하던 두 축 중 *context 중첩* 쪽을 고르는 값.
///
/// 렌더링 중첩(layout 이 자식 파이프라인을 부모의 디스플레이 리스트에 꽂는 것)은 이
/// 값과 무관하게 언제나 그대로다 — 그래서 `transform`, `border-radius`, `opacity`,
/// 클립, 스태킹, 타일 경계 걸침이 모드와 상관없이 똑같이 적용된다.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum EmbeddingMode {
    /// 표준 `<iframe>`. 내용이 child navigable 이 된다.
    Nested,
    /// `<iframe toplevel>`. 부모의 박스 안에서 렌더되지만 내용은 top-level browsing
    /// context 다. ★설계상 스푸핑을 허용하는 모드이므로 pref 로 잠근다★ —
    /// `dom_iframe_toplevel_embed_enabled`.
    TopLevelEmbed,
}

/// 이 문서에게 알려줄 *navigable* 부모. `NewPipelineInfo::parent_info` 는 오직 이
/// 함수로만 채운다.
///
/// `presentation_parent` 는 *표출* 부모다 — 누가 나를 레이아웃하고 크기를 정하고
/// 렌더하고 정리하는가. 그 값은 `TopLevelEmbed` 에서도 그대로 살아 있어야 한다.
/// ★그것까지 끊으면 부모 element 가 `UpdatePipelineId` 를 못 받아 초기 about:blank 를
/// 계속 렌더한다 — 상자에는 CSS 가 전부 먹히는데 내용만 흰색으로 나온다.★
pub fn navigable_parent(
    mode: EmbeddingMode,
    presentation_parent: Option<PipelineId>,
) -> Option<PipelineId> {
    match mode {
        EmbeddingMode::Nested => presentation_parent,
        EmbeddingMode::TopLevelEmbed => None,
    }
}
```

- [ ] **Step 4: 테스트가 통과하는 것을 확인한다**

Run:
```powershell
cargo test -p servo-constellation-traits --manifest-path W:\servo_multigpu-tiled-wall\Cargo.toml
```
Expected: `test result: ok. 3 passed`.

- [ ] **Step 5: 커밋**

```bash
git add components/shared/constellation/from_script_message.rs
git commit -F - <<'MSG'
feat(embed): navigable 부모와 표출 부모를 가르는 규칙을 한 곳에 둔다

iframe 은 두 가지를 겸한다. 부모의 디스플레이 리스트 안에서 렌더되는 것과, 내용이
child navigable 이 되는 것이다. 앞의 것이 모든 CSS 가 먹히는 이유이고, 뒤의 것에서
X-Frame-Options 와 frame-ancestors 와 프레임버스팅이 나온다.

둘을 가르는 값을 EmbeddingMode 로 두고, 문서에게 알려줄 navigable 부모를 고르는
파생을 navigable_parent 하나로 모은다. 파이프라인을 만드는 경로가 둘이라 규칙이
흩어지기 쉬운데, 그 둘이 같은 함수를 거치게 하려는 것이다.

표출 부모는 TopLevelEmbed 에서도 살아 있어야 한다는 것을 doc 에 적었다. 그것까지
끊으면 부모 element 가 UpdatePipelineId 를 받지 못해 초기 about:blank 를 계속
렌더하고, 상자에는 CSS 가 전부 먹히는데 내용만 흰색으로 보인다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_011gaj7C2AhzcjFdu9nw2HGN
MSG
git log -1 --format=%B | grep -c '"'
```
마지막 명령의 출력이 `0` 이어야 한다.

---

### Task 2: pref `dom_iframe_toplevel_embed_enabled`

**Files:**
- Modify: `components/config/prefs.rs` (struct 정의 + `const_default()` 두 곳)

**Interfaces:**
- Consumes: 없음
- Produces: `pref!(dom_iframe_toplevel_embed_enabled) -> bool`, 기본 `false`.

- [ ] **Step 1: struct 에 필드를 추가한다**

`components/config/prefs.rs` 에서 `pub dom_enforce_framing_policy: bool,` 를 찾는다(이 브랜치에 이미 있다). 그 **바로 뒤**에 추가한다 — `dom_enforce_*` 다음이 `dom_exec_*` 이므로 알파벳 순서상 `dom_iframe_*` 은 뒤에 와야 하지만, 이 둘은 같은 주제(프레이밍)라 나란히 두는 편이 읽힌다. `dom_exec_command_enabled` 앞에 넣는다:

```rust
    /// `<iframe toplevel>` 을 인정한다. 켜면 그 속성이 붙은 iframe 의 내용이 부모의
    /// 박스 안에서 그대로 렌더되면서(모든 CSS 적용) browsing context 만 top-level 이
    /// 되어, `X-Frame-Options` / CSP `frame-ancestors` / `top !== self` 프레임버스팅이
    /// 성립하지 않는다. 꺼져 있으면 그 속성은 완전히 무시되고 평범한 iframe 이다.
    ///
    /// ★설계상 스푸핑을 허용한다★ — 우리 UI 안에 남의 사이트를 진짜처럼 배치할 수
    /// 있다. 사설망 표출 전용 키오스크라는 전제에서만 켠다. v1 은 입력 라우팅이 없어
    /// 클릭재킹은 성립하지 않지만, 입력을 얹으면 그 항목은 되살아난다.
    pub dom_iframe_toplevel_embed_enabled: bool,
```

- [ ] **Step 2: 기본값을 추가한다**

같은 파일의 `const_default()` 에서 `dom_enforce_framing_policy: true,` 를 찾아 그 뒤 `dom_exec_command_enabled: false,` **앞**에 추가:

```rust
            dom_iframe_toplevel_embed_enabled: false,
```

- [ ] **Step 3: 컴파일과 기존 설정 테스트가 통과하는지 확인한다**

Run:
```powershell
. W:\scripts\servo_env.ps1
cargo test -p servo-config --test config_surface --manifest-path W:\servo_multigpu-tiled-wall\Cargo.toml
```
Expected: 전부 PASS. (이 테스트는 `debug_env` 와 *이관된* pref 만 검사하므로 새 pref 때문에 실패하지 않는다. 실패한다면 다른 것이 깨진 것이다.)

- [ ] **Step 4: 커밋**

```bash
git add components/config/prefs.rs
git commit -F - <<'MSG'
feat(config): iframe toplevel 임베드를 여는 pref 를 만든다

기본 false 다. 꺼져 있으면 toplevel 속성은 완전히 무시되고 평범한 iframe 으로
동작하므로, 이 pref 를 켜지 않는 한 엔진 동작은 지금과 같다.

doc 주석에 위험의 성격을 갈라 적었다. v1 은 입력 라우팅이 없어 클릭재킹은 성립하지
않고 남는 것은 스푸핑이다. 입력을 얹는 순간 앞의 항목이 되살아나므로 그때
재평가해야 한다. 뭉뚱그려 클릭재킹 위험이라고만 적으면 그 판단을 그르친다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_011gaj7C2AhzcjFdu9nw2HGN
MSG
git log -1 --format=%B | grep -c '"'
```
출력이 `0` 이어야 한다.

---

### Task 3: 속성 판정과 초기 about:blank 파이프라인

**Files:**
- Modify: `components/shared/constellation/from_script_message.rs` (`IFrameLoadInfo` 에 필드 추가, 약 469행)
- Modify: `components/script/dom/html/htmliframeelement.rs:243` 앞(헬퍼 추가), `:266`(load_info), `:302`(parent_info)

**Interfaces:**
- Consumes: `EmbeddingMode`, `navigable_parent()` (Task 1)
- Produces:
  - `IFrameLoadInfo.embedding_mode: EmbeddingMode` — constellation 이 BC 생성 시 읽는다
  - `HTMLIFrameElement::embedding_mode(&self) -> EmbeddingMode` (private)

- [ ] **Step 1: `IFrameLoadInfo` 에 필드를 추가한다**

`components/shared/constellation/from_script_message.rs` 의 `pub struct IFrameLoadInfo` 에서, `pub target_snapshot_params: TargetSnapshotParams,` 뒤에 추가:

```rust
    /// 이 iframe 의 내용을 child navigable 로 만들 것인가. `<iframe toplevel>` 이고
    /// `dom_iframe_toplevel_embed_enabled` 가 켜져 있을 때만 `TopLevelEmbed` 다.
    /// constellation 은 이 값을 **browsing context 생성 시 1회** 읽어
    /// `BrowsingContext::embedding_mode` 에 저장하고, 이후 내비게이션은 그 저장값을
    /// 쓴다 — 재로드처럼 이 메시지를 싣지 않는 경로에서도 일관되게 하려는 것이다.
    pub embedding_mode: EmbeddingMode,
```

- [ ] **Step 2: element 에 판정 헬퍼를 추가한다**

`components/script/dom/html/htmliframeelement.rs` 의 `fn continue_navigation(` (243행) **바로 앞**에 추가:

```rust
    /// 이 iframe 이 `<iframe toplevel>` 인가 — 속성이 있고 pref 도 켜져 있는가.
    ///
    /// `toplevel` 은 비표준 속성이라 `local_name!` 로 못 쓰고 `LocalName::from` 을
    /// 쓴다(같은 방식이 `servoparser/html.rs` 의 `is` 속성에도 있다).
    ///
    /// ★browsing context 생성 시 1회만 평가된다★ — 속성을 나중에 붙이거나 떼도
    /// 요소가 다시 만들어질 때까지 반영되지 않는다. 살아 있는 browsing context 를
    /// re-parent 하지 않기 위한 v1 의 의도적 제약이다.
    fn embedding_mode(&self) -> EmbeddingMode {
        let has_attribute = self
            .upcast::<Element>()
            .has_attribute(&LocalName::from("toplevel"));
        if has_attribute && pref!(dom_iframe_toplevel_embed_enabled) {
            EmbeddingMode::TopLevelEmbed
        } else {
            EmbeddingMode::Nested
        }
    }
```

임포트를 추가한다. `use servo_constellation_traits::{` 블록에 `EmbeddingMode,` 와 `navigable_parent,` 를 알파벳 순서로 끼워 넣고, 파일에 `use servo_config::pref;` 가 없으면 `use servo_base::id::{...};` 뒤에 추가한다.

- [ ] **Step 3: `load_info` 에 모드를 싣는다**

같은 파일 266행의 `let load_info = IFrameLoadInfo {` 안, `target_snapshot_params,` 뒤에 추가:

```rust
            embedding_mode: self.embedding_mode(),
```

- [ ] **Step 4: 초기 about:blank 파이프라인의 `parent_info` 를 파생시킨다**

같은 파일 302행의

```rust
                    parent_info: Some(window.pipeline_id()),
```

을 다음으로 바꾼다:

```rust
                    // 이 파이프라인은 constellation 이 아니라 여기서 만들므로, 아직
                    // BrowsingContext 가 없어 자기 판정을 쓴다. 실제 사이트가 로드되는
                    // 두 번째 파이프라인은 constellation 이 만들고 저장된
                    // `embedding_mode` 를 쓴다 — 둘 다 `navigable_parent` 를 거친다.
                    parent_info: navigable_parent(
                        self.embedding_mode(),
                        Some(window.pipeline_id()),
                    ),
```

- [ ] **Step 5: 컴파일을 확인한다**

Run:
```powershell
. W:\scripts\servo_env.ps1
cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl --manifest-path W:\servo_multigpu-tiled-wall\Cargo.toml
```
Expected: `IFrameLoadInfo` 를 만드는 다른 곳이 있으면 "missing field `embedding_mode`" 로 실패한다. 실패하면 그 지점에 `embedding_mode: EmbeddingMode::Nested,` 를 넣는다(그 경로들은 `<iframe toplevel>` 과 무관하다). 그 뒤 다시 돌려 통과시킨다.

- [ ] **Step 6: 커밋**

```bash
git add components/shared/constellation/from_script_message.rs components/script/dom/html/htmliframeelement.rs
git commit -F - <<'MSG'
feat(embed): iframe toplevel 속성을 읽어 초기 파이프라인에 반영한다

속성이 붙어 있고 pref 도 켜져 있을 때만 TopLevelEmbed 다. 둘 중 하나라도 없으면
평범한 iframe 이므로, pref 를 켜지 않는 한 동작은 지금과 같다.

toplevel 은 비표준 속성이라 local_name 매크로를 못 쓰고 LocalName::from 을 쓴다.
같은 방식이 servoparser 의 is 속성에도 있다.

browsing context 생성 시 1회만 평가한다는 것을 doc 에 적었다. 속성을 나중에 붙이거나
떼도 요소가 다시 만들어질 때까지 반영되지 않는다 - 살아 있는 browsing context 를
re-parent 하지 않기 위한 v1 의 의도적 제약이다.

여기서 만드는 것은 초기 about:blank 파이프라인 하나뿐이다. 실제 사이트는 그 뒤
constellation 이 만드는 별도 파이프라인으로 로드되므로 다음 작업에서 이어진다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_011gaj7C2AhzcjFdu9nw2HGN
MSG
git log -1 --format=%B | grep -c '"'
```
출력이 `0` 이어야 한다.

---

### Task 4: `BrowsingContext` 에 모드를 저장한다

**Files:**
- Modify: `components/constellation/browsingcontext.rs:19-33`(`NewBrowsingContextInfo`), `:60-105`(struct + `new`)
- Modify: `components/constellation/constellation.rs:1149`(`new_browsing_context`), `:3545`(`handle_script_new_iframe`)

**Interfaces:**
- Consumes: `EmbeddingMode` (Task 1), `IFrameLoadInfo.embedding_mode` (Task 3)
- Produces: `BrowsingContext.embedding_mode: EmbeddingMode` — Task 5 가 읽는다

- [ ] **Step 1: `NewBrowsingContextInfo` 에 필드를 추가하고 기존 필드의 의미를 적는다**

`components/constellation/browsingcontext.rs` 의 `pub struct NewBrowsingContextInfo` 에서 `parent_pipeline_id` 의 doc 주석을 아래로 교체하고 필드를 하나 추가한다:

```rust
    /// 이 browsing context 를 담고 있는 **표출** 부모 파이프라인 — 누가 나를
    /// 레이아웃하고 크기를 정하고 렌더하고 정리하는가. 진짜 최상위면 `None`.
    ///
    /// ★`<iframe toplevel>` 에서도 이 값은 `Some` 이다.★ 문서에게 알려줄 *navigable*
    /// 부모는 `embedding_mode` 와 함께 `navigable_parent()` 로 따로 구한다.
    pub parent_pipeline_id: Option<PipelineId>,

    /// 이 browsing context 의 내용이 child navigable 인지 여부.
    pub embedding_mode: EmbeddingMode,
```

`BrowsingContext` struct 의 `parent_pipeline_id` 에도 같은 doc 주석을 넣고 필드를 추가한다:

```rust
    /// 이 browsing context 를 담고 있는 **표출** 부모 파이프라인. 진짜 최상위면 `None`.
    ///
    /// ★`<iframe toplevel>` 에서도 `Some` 이다★ — navigable 부모는
    /// `navigable_parent(self.embedding_mode, self.parent_pipeline_id)` 로 구한다.
    pub parent_pipeline_id: Option<PipelineId>,

    /// 이 browsing context 의 내용이 child navigable 인지 여부. 생성 시 확정되고
    /// 이후 바뀌지 않는다.
    pub embedding_mode: EmbeddingMode,
```

파일 상단 임포트에 `use servo_constellation_traits::EmbeddingMode;` 를 추가한다(이미 다른 심볼을 그 크레이트에서 쓰고 있으면 그 블록에 끼워 넣는다).

- [ ] **Step 2: `BrowsingContext::new` 가 모드를 받게 한다**

같은 파일의 `pub fn new(` 인자 목록에서 `parent_pipeline_id: Option<PipelineId>,` 뒤에 `embedding_mode: EmbeddingMode,` 를 추가하고, 반환하는 구조체 리터럴의 `parent_pipeline_id,` 뒤에 `embedding_mode,` 를 추가한다.

- [ ] **Step 3: `new_browsing_context` 가 모드를 넘기게 한다**

`components/constellation/constellation.rs:1149` 의 `fn new_browsing_context(` 인자 목록에서 `parent_pipeline_id: Option<PipelineId>,` 뒤에 `embedding_mode: EmbeddingMode,` 를 추가하고, 함수 안의 `BrowsingContext::new(` 호출에서 `parent_pipeline_id,` 뒤에 `embedding_mode,` 를 추가한다.

같은 파일에서 `self.new_browsing_context(` 호출부를 전부 찾아 `new_context_info.parent_pipeline_id,` 뒤에 `new_context_info.embedding_mode,` 를 넣는다:

```powershell
Select-String -Path W:\servo_multigpu-tiled-wall\components\constellation\constellation.rs -Pattern "self.new_browsing_context\("
```

- [ ] **Step 4: `NewBrowsingContextInfo` 를 만드는 곳을 전부 채운다**

```powershell
Select-String -Path W:\servo_multigpu-tiled-wall\components\constellation\constellation.rs -Pattern "NewBrowsingContextInfo \{"
```

`handle_script_new_iframe`(3545행 부근)의 것에는 메시지에서 온 값을 쓴다. 먼저 destructure 에 필드를 추가한다 — `is_private,` 뒤에:

```rust
            embedding_mode,
```

그리고 `NewBrowsingContextInfo { parent_pipeline_id: Some(parent_pipeline_id),` 뒤에:

```rust
                embedding_mode,
```

★`parent_pipeline_id` 는 `Some(parent_pipeline_id)` 그대로 둔다★ — 표출 부모는 살아 있어야 한다.

**나머지 `NewBrowsingContextInfo` 생성 지점**(최상위 WebView 생성, `window.open` 등)에는 `embedding_mode: EmbeddingMode::Nested,` 를 넣는다. 그 경로들은 `<iframe toplevel>` 과 무관하고, 진짜 최상위라 `parent_pipeline_id` 가 `None` 이므로 `navigable_parent` 는 어느 모드에서도 `None` 을 준다.

파일 상단 임포트에 `EmbeddingMode` 를 추가한다.

- [ ] **Step 5: 컴파일을 확인한다**

Run:
```powershell
cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl --manifest-path W:\servo_multigpu-tiled-wall\Cargo.toml
```
Expected: 에러 0. "missing field `embedding_mode`" 가 남아 있으면 Step 4 에서 빠뜨린 생성 지점이다.

- [ ] **Step 6: 커밋**

```bash
git add components/constellation/browsingcontext.rs components/constellation/constellation.rs
git commit -F - <<'MSG'
feat(embed): browsing context 가 자기 임베딩 모드를 기억하게 한다

parent_pipeline_id 하나가 두 뜻을 겸하고 있었다. 누가 나를 레이아웃하고 크기를
정하고 렌더하고 정리하는가 하는 표출 부모와, 내가 child navigable 인가 하는 navigable
부모다. 앞의 것은 constellation 이 쓰고 뒤의 것은 script 의 WindowProxy 가 쓴다.

embedding_mode 를 따로 두고 parent_pipeline_id 는 표출 부모라고 doc 에 명시했다.
iframe toplevel 에서도 표출 부모는 Some 으로 살아 있어야 한다는 것도 함께 적었다.

모드는 browsing context 생성 시 메시지에서 1 회 읽어 저장한다. 이후 내비게이션은
저장값을 쓰므로, 재로드처럼 그 메시지를 싣지 않는 경로에서도 판정이 일관된다.

이 커밋만으로는 동작이 바뀌지 않는다. 저장만 하고 아직 아무도 읽지 않는다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_011gaj7C2AhzcjFdu9nw2HGN
MSG
git log -1 --format=%B | grep -c '"'
```
출력이 `0` 이어야 한다.

---

### Task 5: 내비게이션 파이프라인의 `parent_info` 를 파생시킨다 — 기능이 실제로 켜지는 지점

**Files:**
- Modify: `components/constellation/constellation.rs:3443`(`handle_script_loaded_url_in_iframe_msg`), 특히 `:3525` 의 `Some(parent_pipeline_id),`

**Interfaces:**
- Consumes: `BrowsingContext.embedding_mode` (Task 4), `navigable_parent()` (Task 1)
- Produces: 없음 (기능 완성)

- [ ] **Step 1: BC 에서 모드를 읽는다**

`handle_script_loaded_url_in_iframe_msg` 안에서 이미 BC 를 꺼내는 곳이 있다:

```rust
        let Some(browsing_context) = self.browsing_contexts.get(&browsing_context_id) else {
```

그 블록 아래에 이미 `let browsing_context_size = browsing_context.viewport_details;` 같은 줄들이 있다. 그 옆에 추가한다:

```rust
        let embedding_mode = browsing_context.embedding_mode;
```

- [ ] **Step 2: `new_pipeline` 에 넘기는 부모를 파생값으로 바꾼다**

같은 함수의 `self.new_pipeline(` 호출에서

```rust
            Some(parent_pipeline_id),
```

을 다음으로 바꾼다:

```rust
            // ★이 인자가 NewPipelineInfo::parent_info 가 되고, script 가 그것으로
            // WindowProxy 의 부모를 정한다 — 즉 X-Frame-Options / frame-ancestors /
            // `top !== self` 가 성립할지를 여기서 가른다.★ 실제 사이트가 로드되는
            // 파이프라인은 element 가 만드는 초기 about:blank 가 아니라 이것이고,
            // 교차 출처면 부모의 script thread 도 재사용하지 않으므로, 여기를 빠뜨리면
            // 아무리 element 쪽을 고쳐도 사이트는 계속 차단된다.
            navigable_parent(embedding_mode, Some(parent_pipeline_id)),
```

파일 상단 임포트에 `navigable_parent` 를 추가한다.

- [ ] **Step 3: 빌드한다**

Run:
```powershell
. W:\scripts\servo_env.ps1
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl --release --manifest-path W:\servo_multigpu-tiled-wall\Cargo.toml
```
Expected: `Finished` + exit 0.

- [ ] **Step 4: 기능이 꺼져 있을 때 아무것도 안 바뀌는지 확인한다**

Run (pref 없이, 기존 iframe 프로브):
```powershell
W:\servo_multigpu-tiled-wall\target\release\examples\winit_wall.exe --wall-all-tiles `
  --wall-layout ..\config\wall_layout.local_2x1.json `
  "file:///W:/servo_multigpu-tiled-wall/tests/html/multigpu_wall_iframe_probe.html"
```
Expected: 줄무늬 srcdoc 패턴이 이음매를 넘어 정상 표시된다. 패닉 없음. **창을 닫아서 종료한다** — 강제 종료하면 stderr 가 flush 되지 않는다.

- [ ] **Step 5: 커밋**

```bash
git add components/constellation/constellation.rs
git commit -F - <<'MSG'
feat(embed): 사이트가 실제로 로드되는 파이프라인의 부모를 파생시킨다

이 인자가 NewPipelineInfo 의 parent_info 가 되고, script 가 그것으로 WindowProxy 의
부모를 정한다. 즉 X-Frame-Options 와 frame-ancestors 와 프레임버스팅이 성립할지를
여기서 가른다.

★element 쪽만 고쳐서는 안 된다★ iframe 은 먼저 about:blank 파이프라인을 만들고
실제 사이트는 그 뒤 별도 파이프라인으로 로드되는데, 후자는 element 가 아니라
constellation 이 만든다. 게다가 교차 출처 사이트는 부모의 script thread 를 재사용하지
않아 부모 없는 WindowProxy 캐시도 물려받지 못한다. 스파이크에서 이 지점을 빠뜨려
사이트가 계속 차단됐고, 증상은 로그가 한 줄만 찍히는 것이었다.

pref 가 꺼져 있으면 모드가 Nested 라 파생값이 종전과 같으므로 동작 변화가 없다.
평범한 iframe 프로브로 회귀를 확인했다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_011gaj7C2AhzcjFdu9nw2HGN
MSG
git log -1 --format=%B | grep -c '"'
```
출력이 `0` 이어야 한다.

---

### Task 6: 대조군/처리군 프로브와 실기 검증

**Files:**
- Create: `tests/html/toplevel_embed_probe.html`

**Interfaces:**
- Consumes: Task 2 의 pref 이름, Task 3 의 `toplevel` 속성
- Produces: 없음

- [ ] **Step 1: 프로브를 만든다**

스파이크 브랜치의 페이지를 가져와 이름을 정리한다:

```bash
git show wall-toplevel-embed-spike:tests/html/toplevel_embed_spike.html > tests/html/toplevel_embed_probe.html
```

그 다음 파일을 열어 세 가지를 고친다.

1. 머리말의 `★SPIKE — THROWAWAY★` 표기를 지우고, 실행 커맨드에 pref 를 추가한다:
   ```
   --pref dom_iframe_toplevel_embed_enabled=true
   ```
   그리고 ★프레이밍 pref(`dom_enforce_framing_policy`, `network_enforce_mixed_content`)는 주지 않는다★ 는 문장을 유지한다 — 정책이 강제된 상태에서 처리군만 빠져나가는 것을 보는 게 요점이다.
2. `is_toplevel_embed_spike` 라는 함수 이름 언급을 `embedding_mode` 로 바꾼다.
3. CSS 매트릭스에 중첩 클립을 추가한다. `.stage` 규칙을 다음으로 교체한다:

```css
  /* 중첩 클립까지 확인한다 — 조상의 overflow 클립이 임베드된 사이트에도 걸려야
     한다. 걸리지 않으면 사이트가 이 상자 밖으로 삐져나온다. */
  .stage { display: flex; justify-content: center; padding: 60px 0;
           overflow: hidden; border: 1px dashed #444a5e; border-radius: 12px; }
```

- [ ] **Step 2: pref 를 끈 채로 돌려 대조군과 처리군이 **둘 다** 차단되는지 본다**

Run:
```powershell
W:\servo_multigpu-tiled-wall\target\release\examples\winit_wall.exe --wall-all-tiles `
  --wall-layout ..\config\wall_layout.local_2x1.json `
  "file:///W:/servo_multigpu-tiled-wall/tests/html/toplevel_embed_probe.html"
```
Expected: **양쪽 다 에러 페이지.** pref 가 꺼져 있으면 `toplevel` 속성이 무시된다는 확인이다. 창을 닫아 종료한다.

★이 단계가 설계 문서의 단위 테스트 항목 중 *게이트: pref off 이면 속성이 있어도 Nested* 를 대신한다.★ 그 판정은 `HTMLIFrameElement::embedding_mode()` 안에 있는데, 이를 단위 테스트로 부르려면 DOM 요소와 문서 · 윈도우 · pref 전역 상태를 세워야 해서 비용이 실익을 넘는다. 대신 여기서 **관측 가능한 결과**(속성이 붙어 있어도 차단된다)로 확인한다. Task 1 이 검증하는 파생 규칙과 합치면 게이트 경로 전체가 덮인다.

- [ ] **Step 3: pref 를 켜고 돌려 처리군만 통과하는지 본다**

Run:
```powershell
W:\servo_multigpu-tiled-wall\target\release\examples\winit_wall.exe --wall-all-tiles `
  --wall-layout ..\config\wall_layout.local_2x1.json `
  --pref dom_iframe_toplevel_embed_enabled=true `
  "file:///W:/servo_multigpu-tiled-wall/tests/html/toplevel_embed_probe.html" 2> W:\toplevel_embed.err.log
```
Expected (전부 만족해야 PASS):
- 왼쪽(대조군, 평범한 iframe) = 에러 페이지. ★이게 차단되어야 정책이 실제로 작동 중이라는 증거가 된다★
- 오른쪽(처리군) = 사이트가 **기울어지고 · 모서리가 둥글게 잘리고 · 반투명하게** 표시
- 로그에 `Subframe has no parent` 가 **없다**
- 패닉 없음

창을 닫아 종료한 뒤 로그를 확인한다:
```powershell
Select-String -Path W:\toplevel_embed.err.log -Pattern "Subframe has no parent|panicked|focus chain"
```

- [ ] **Step 4: 포커스 체인 경고를 확인한다**

Step 3 의 검색에 `Aborting the focus operation - focus chain sanity check failed` 가 나오면 **멈추고 보고한다.** 설계 문서의 리스크 항목이며, 재현되면 원인을 규명한 뒤 진행하기로 되어 있다. 나오지 않으면 그 리스크를 설계 문서에서 해소로 표시한다.

- [ ] **Step 5: 커밋**

```bash
git add tests/html/toplevel_embed_probe.html
git commit -F - <<'MSG'
test(embed): 대조군과 처리군을 나란히 놓는 프로브

같은 URL 에 같은 CSS 를 건 상자 두 개를 두고 차이를 toplevel 속성 하나로만 둔다.
왼쪽이 차단되어야 정책이 실제로 작동 중인데 오른쪽만 빠져나갔다는 것이 성립하므로,
대조군의 차단 자체가 판정의 일부다.

CSS 매트릭스에 transform 과 border-radius 와 opacity 에 더해 조상의 overflow 클립을
넣었다. 임베드된 사이트가 그 클립을 받지 않으면 상자 밖으로 삐져나온다.

★load 이벤트를 성공으로 읽으면 안 된다★ 차단된 로드도 Servo 가 에러 문서를 합성해
파싱하므로 load 가 똑같이 발생한다. 판정은 눈으로 한다.

프레임 배경은 밝게 고정했다. Servo 내장 에러 페이지는 CSS 가 없어 캔버스가 투명하고
글자가 검정이라, 어두운 상자에서는 차단된 로드가 검정 위 검정으로 그려져 아무것도
안 그려진 것으로 오진한다.

CSS Grid 는 쓰지 않았다. Servo 는 layout_grid_enabled 가 기본 꺼짐이라 display grid
가 조용히 블록으로 폴백해 두 칸이 세로로 쌓인다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_011gaj7C2AhzcjFdu9nw2HGN
MSG
git log -1 --format=%B | grep -c '"'
```
출력이 `0` 이어야 한다.

---

### Task 7: 설정 정본 표 갱신

**Files:**
- Modify: `docs/multigpu/configuration.md`

**Interfaces:**
- Consumes: Task 2 의 pref 이름
- Produces: 없음

- [ ] **Step 1: 표에 pref 를 추가한다**

`### 표출용 웹 보안 완화` 절(이 브랜치에 이미 있다)의 표에 행을 추가한다:

```markdown
| `dom_iframe_toplevel_embed_enabled` | bool | `false` | `<iframe toplevel>` 을 인정한다. 그 속성이 붙은 iframe 은 부모의 박스 안에서 그대로 렌더되면서(모든 CSS 적용) 내용만 top-level browsing context 가 되어, `X-Frame-Options`/`frame-ancestors`/frame-busting 이 **성립하지 않는다**. 꺼져 있으면 속성은 완전히 무시된다. |
```

그리고 같은 절에 관계를 적는다:

```markdown
**앞의 두 pref 와의 관계.** `<iframe toplevel>` 을 쓰면 `dom_enforce_framing_policy` 와
`network_enforce_mixed_content` 는 **필요 없다.** 앞의 둘은 검사를 *끄는* 것이고,
`toplevel` 은 애초에 그 검사에 *도달하지 않는* 것이다. 평범한 iframe 을 그대로 쓰면서
정책만 무르게 하고 싶을 때 앞의 둘을 쓴다.

**★`toplevel` 이 풀어주지 않는 것★** — 부모 페이지와 임베드된 사이트는 **서로 통신할 수
없다**(부모가 없으므로 `postMessage` 상대가 없다). 입력·포커스·history 는 v1 비범위다.
```

- [ ] **Step 2: 제목의 pref 수를 고친다**

파일 첫 줄과 `## pref NN 개` 절 제목의 숫자를 22 에서 23 으로 고친다.

- [ ] **Step 3: 문서 표류 테스트가 통과하는지 확인한다**

Run:
```powershell
cargo test -p servo-config --test config_surface --manifest-path W:\servo_multigpu-tiled-wall\Cargo.toml
```
Expected: 전부 PASS.

- [ ] **Step 4: 커밋**

```bash
git add docs/multigpu/configuration.md
git commit -F - <<'MSG'
docs(multigpu): 설정 정본 표에 toplevel 임베드 pref 를 넣는다

앞선 두 pref 와의 관계를 적었다. 그 둘은 검사를 끄는 것이고 toplevel 은 애초에 그
검사에 도달하지 않는 것이라, 함께 쓸 일이 없다.

toplevel 이 풀어주지 않는 것도 함께 적었다. 부모가 없으므로 부모 페이지와 임베드된
사이트는 서로 postMessage 상대가 없어 통신할 수 없다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_011gaj7C2AhzcjFdu9nw2HGN
MSG
git log -1 --format=%B | grep -c '"'
```
출력이 `0` 이어야 한다.

---

## 완료 조건

- [ ] `cargo test -p servo-constellation-traits` — Task 1 의 3 개 PASS
- [ ] `cargo test -p servo-config --test config_surface` — 전부 PASS
- [ ] `cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl --release` — exit 0
- [ ] pref off 실기: 대조군·처리군 **둘 다** 차단 (속성이 무시된다)
- [ ] pref on 실기: 대조군 차단, 처리군은 **기울어지고 둥글게 잘리고 반투명한** 사이트
- [ ] 로그에 `Subframe has no parent` 없음, 패닉 없음
- [ ] 포커스 체인 경고가 나오면 **멈추고 보고** (설계 문서 리스크 항목)
- [ ] `git diff --check` 클린

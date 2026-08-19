# `<iframe toplevel>` — 렌더링 중첩은 유지하고 browsing context 중첩만 끊는다 (2026-08-19)

## 목표

월의 자체 레이아웃 HTML 안에 외부 웹페이지를 표출하되, **표출 단위를 특별취급하지 않는다.**
일반 DOM 요소에 적용되는 것(`transform`, `border-radius`, `opacity`, 클립, 스태킹, 스크롤,
타일 경계 걸침, 가드밴드)은 그 웹페이지에도 그대로 적용되어야 한다. 동시에 상용 사이트
대부분이 `X-Frame-Options: DENY` 이므로 프레이밍 정책과 JS frame-busting 을 성립시키지
않아야 한다.

## 배경 — 스파이크로 확정한 것

세 번의 실측을 거쳤다. 근거는 브랜치 `wall-toplevel-embed-spike` 의 커밋 하나에 전부 있다.

### 핵심: `<iframe>` 은 독립된 두 가지를 묶어 팔고 있다

| 축 | 하는 일 | 결정 지점 |
|---|---|---|
| **렌더링 중첩** | layout 이 부모의 디스플레이 리스트 안에 자식 파이프라인을 꽂는다 | `layout/display_list/mod.rs` 의 `visit_iframe` → `common_properties`(조상 transform / 클립 체인 / 스태킹 컨텍스트) + `push_iframe` |
| **context 중첩** | 그 내용이 *child navigable* 이 된다 | `NewPipelineInfo.parent_info` |

**모든 CSS 가 iframe 에 먹히는 이유가 전자다** — 특별한 처리가 아니라 일반 박스와 같은
경로를 타기 때문이다. 그리고 `X-Frame-Options`, CSP `frame-ancestors`, `top !== self`
frame-busting 은 전부 후자에서 나온다. **후자만 끊으면 전자는 그대로 남는다.**

### 끊는 지점은 한 곳

`script_window_proxies.rs` 가 `parent_info` 하나로 `WindowProxy` 의 부모를 정한다.
`None` 이면:

- `xframeoptions.rs` 의 Step 1(*child navigable 이 아니면 통과*) → **XFO 검사 미진입**
- `csp.rs` 의 frame-ancestors → 부모 origin 체인이 비어 차단 근거 없음
- 사이트 안에서 `window.top === window` → **frame-busting 미발동**
- mixed content → 요청의 client 가 그 사이트 자신의 문서라, `file://` 부모가 secure
  context 로 오염시키던 문제가 사라진다

### 실측 결과

2x1 월에서 같은 URL(`https://www.naver.com`)과 같은 CSS 를 건 상자 두 개를 나란히 놓고,
차이는 `toplevel` 속성 하나로만 두었다. **처리군에서 기울어진 rounded rect 안에 사이트가
정상 출력되는 것을 육안 확인했다.** 프레이밍 pref 는 주지 않았다 — 기본값 그대로 정책이
강제된 상태인데도 떴다. 즉 **정책을 끈 것이 아니라 그 검사에 도달하지 않는다.**

### 실패 2회에서 확정한 절단면

**1차 — element 쪽만 끊었더니 여전히 차단.** `<iframe>` 은 먼저 about:blank 파이프라인을
만들고 **실제 사이트는 그 뒤 별도 파이프라인**으로 로드된다. 그 파이프라인의 `parent_info`
는 element 가 아니라 constellation(`handle_script_loaded_url_in_iframe_msg`)이 정한다.
게다가 교차 출처 사이트는 부모 script thread 를 재사용하지 않으므로
(`get_or_create_event_loop_for_new_pipeline`) 부모 없는 `WindowProxy` 캐시도 물려받지
못한다. **증상: 로그의 `[toplevel-embed]` 가 한 줄만 찍힘.**

**2차 — browsing context 의 `parent_pipeline_id` 까지 끊었더니 흰 화면.** 펜딩 세션 히스토리
변경의 *부모에게 알림* 단계는 BC 에 부모가 있을 때만 `UpdatePipelineId` 를 보낸다. 끊으면
부모 element 가 새 파이프라인 id 를 모른 채 초기 about:blank 를 계속 `push_iframe` 한다.
★**상자에는 CSS 가 전부 먹히고 내용만 흰색**이라 "CSS 는 되는데 콘텐츠가 안 된다" 로
오진하기 딱 좋은 모양이다.★ 부수 신호로 `Subframe has no parent` 경고가 뜬다.

**정답 절단면**: browsing context 트리는 **그대로 두고**(크기 전달·활성·가시성·정리·iframe
`load` 이벤트·`UpdatePipelineId` 가 전부 그 위에서 돈다), **파이프라인의 `parent_info` 만**
끊는다. 엔진의 장부는 유지하고 문서의 자기 인식만 바꾼다.

## 사용자 결정 사항

- **요소 형태는 `<iframe toplevel>` 속성.** 새 요소(`<wall-site>`)를 만들지 않는다. 레이아웃,
  CSS, `width`/`height`, `sandbox` 등 iframe 의 모든 의미를 공짜로 상속하는 것이 이 설계의
  요점이기 때문이다.
- **v1 은 표출 전용.** 입력 라우팅·히트테스트·포커스는 비범위다.
- **A안 pref 두 개(`dom_enforce_framing_policy`, `network_enforce_mixed_content`)는 유지한다.**
  이 설계와 직교하고 평범한 iframe 에는 여전히 유효하다. 다만 관계를 문서화한다 —
  `<iframe toplevel>` 을 쓰면 그 두 pref 가 **필요 없다.**

## 설계

### 1. 부모가 두 종류라는 것을 이름으로 드러낸다

현재 `BrowsingContext.parent_pipeline_id` 하나가 두 뜻을 겸하고 있다.

| 뜻 | 쓰는 곳 | toplevel embed 에서 |
|---|---|---|
| **표출 부모** — 누가 나를 레이아웃·크기 결정·렌더·정리하는가 | constellation | **유지** |
| **navigable 부모** — 나는 child navigable 인가 | script 의 `WindowProxy` | **없음** |

다행히 이 둘은 **이미 서로 다른 계층에 산다**(표출 부모는 constellation, navigable 부모는
script 의 `parent_info`). 그래서 대규모 리네임 없이 다음 둘로 충분하다.

1. `BrowsingContext` 에 `embedding_mode: EmbeddingMode` 필드 추가.
   `enum EmbeddingMode { Nested, TopLevelEmbed }`.
2. `BrowsingContext.parent_pipeline_id` 의 doc 주석에 **표출 부모**라고 명시. navigable
   부모를 원하는 코드는 `embedding_mode` 를 함께 봐야 한다고 적는다.

### 2. 파생을 순수 함수 한 곳으로 모은다

```
navigable_parent(embedding_mode, presentation_parent) -> Option<PipelineId>
    Nested         => presentation_parent
    TopLevelEmbed  => None
```

이 설계에서 **유닛 테스트가 가능한 거의 유일한 지점**이므로 의도적으로 함수로 뽑는다.
`NewPipelineInfo.parent_info` 는 전부 이 함수를 거쳐서만 채운다.

★**스파이크 대비 개선**★ — 스파이크는 `parent_info` 를 *메시지*(`IFrameLoadInfo`)에서 읽었다.
본구현은 **BC 의 `embedding_mode` 에서 파생**시킨다. 그래야 재로드처럼 그 필드를 싣지 않는
경로에서도 일관된다. 메시지의 플래그는 **BC 생성 시 1회** `embedding_mode` 를 정하는 데만
쓴다.

**호출 지점은 두 곳이고 둘 다 같은 함수를 쓴다.** 파이프라인을 만드는 경로가 둘이기
때문이다.

| 경로 | 만드는 주체 | mode 출처 |
|---|---|---|
| 초기 about:blank | script (`HTMLIFrameElement` 가 직접 `spawn_pipeline`) | `is_toplevel_embed()` (속성 AND pref) |
| 실제 사이트 내비게이션 | constellation (`handle_script_loaded_url_in_iframe_msg`) | 그 BC 의 `embedding_mode` |

script 쪽은 아직 BC 가 없으므로 자기 판정을 쓰고, constellation 쪽은 이미 확정된 BC 의
`embedding_mode` 를 쓴다. **둘 다 `navigable_parent()` 를 거친다** — 그래야 규칙이 한 군데에만
있다. 이 함수는 두 크레이트가 공유하므로 `servo_constellation_traits`(=
`components/shared/constellation`)에 `EmbeddingMode` 와 함께 둔다.

### 3. 흐름

```
<iframe toplevel>
  → HTMLIFrameElement::is_toplevel_embed()      속성 AND pref
  → IFrameLoadInfo.toplevel_embed: bool
  → BrowsingContext.embedding_mode              생성 시 1회 확정
  → navigable_parent(mode, presentation_parent)
  → NewPipelineInfo.parent_info
```

**속성의 동적 변경은 v1 비지원.** browsing context 생성 시 한 번만 반영하고, 이후 속성을
추가/제거해도 요소가 다시 만들어질 때까지 무시한다. 살아 있는 BC 를 re-parent 하지 않기
위해서다. 이 제약을 요소 doc 주석과 사용자 문서 양쪽에 적는다.

### 4. 게이트와 보안

- pref `dom_iframe_toplevel_embed_enabled`, **기본 `false`**.
- 켜진 채로 기동하면 경고 한 줄을 남긴다(`--ignore-certificate-errors` 선례와 동일한 성격).
- pref 가 꺼져 있으면 `toplevel` 속성은 **완전히 무시**되고 평범한 iframe 으로 동작한다.

**위험의 성격을 정확히 적는다.** 뭉뚱그리면 나중에 입력을 얹을 때 판단을 그르친다.

- **클릭재킹** — v1 에서는 성립하지 않는다. 입력 라우팅이 없어 임베드된 문서가 클릭을
  **받을 수 없다.** 입력을 얹는 순간 이 항목은 되살아나므로, 그때 재평가해야 한다.
- **스푸핑** — 남는 위험은 이쪽이다. 우리 UI 안에 남의 사이트를 진짜처럼 배치할 수 있다.
  사설망 표출 전용 키오스크라는 전제에서 수용한다.

`docs/multigpu/configuration.md` 의 정본 표에 pref 를 추가하고, A안 pref 두 개와의 관계를
적는다.

### 5. 임베드된 문서가 보는 것

| API | 값 |
|---|---|
| `window.top` / `window.parent` | 자기 자신 |
| `window.frameElement` | `null` |
| `X-Frame-Options` / CSP `frame-ancestors` | 적용 안 됨 |
| 쿠키 · 스토리지 | 자기 origin 으로 정상 |
| 부모와의 `postMessage` | **불가** — 부모가 없다 |

마지막 줄은 장점이자 제약이다. 부모 페이지와 임베드된 사이트는 **서로 통신할 수 없다.**
격리가 필요하면 이득이고, 조율이 필요하면 이 방식을 못 쓴다.

### 6. 렌더링 — 설계상 할 일이 없다

layout 을 건드리지 않는다. `visit_iframe` → `common_properties` + `push_iframe` 경로가 그대로
유지되므로 transform · 클립 · `border-radius` · `opacity` · 스태킹 · 스크롤 · **타일별 클립 ·
가드밴드**가 전부 자동으로 따라온다. 목표의 *특별취급하지 않는다* 가 여기서 충족된다.

한 가지 전제가 있다: 하나의 painter 에 WebView 가 둘 이상 보일 때의 페인트 순서. 이는
`wall-iframe-framing-policy` 의 `fix(paint)` 커밋에서 이미 결정적으로 만들어 두었다
(`WebViewId` 오름차순 = 생성 순서 = 나중이 위).

## 테스트

### 단위

- `navigable_parent()` 파생: `Nested` → 표출 부모 그대로, `TopLevelEmbed` → `None`.
- 게이트: pref off 이면 `toplevel` 속성이 있어도 `Nested`.

### 통합 프로브

`tests/html/toplevel_embed_spike.html` 을 정식 테스트 페이지로 승격한다(스파이크 브랜치에서
가져와 이름과 머리말을 정리). 구성은 유지한다 — **같은 URL · 같은 CSS 를 건 대조군(평범한
iframe)과 처리군(`toplevel`)을 나란히** 두고, 차이가 속성 하나뿐이게 한다. CSS 매트릭스는
`transform` · `border-radius` · `opacity` · 중첩 클립을 포함한다.

★**프로브에 CSS Grid 를 쓰지 않는다**★ — Servo 는 `layout_grid_enabled` 가 기본 `false` 라
`display: grid` 가 조용히 블록으로 폴백해 칸이 전부 세로로 쌓인다. flex 를 쓴다.

★**프레임 배경은 밝게 유지한다**★ — Servo 내장 에러 페이지는 CSS 가 없어 캔버스가 투명하고
글자가 검정이라, 어두운 상자 위에서는 차단된 로드가 검정 위 검정으로 그려져 "아무것도 안
그려짐" 으로 오진된다.

### 회귀 스모크 (월 실행)

- 대조군은 차단되고 처리군은 렌더된다 — **둘 다 떠 있어야** 정책이 실제로 작동 중인데
  처리군만 빠져나갔다는 것이 성립한다.
- 로그에 `Subframe has no parent` 가 **없다**(BC 트리가 온전하다는 신호).
- 패닉 없음, 프레임 배리어 경고가 기존 수준을 넘지 않음.

### 정적 검사 (프로젝트 관례)

`cargo check -p servo --example winit_wall --features media-gstreamer,no-wgl`,
`cargo test -p servo-config --test config_surface`, `git diff --check`.
`rustfmt --edition 2024 --check` 는 **완료 조건으로 쓰지 않는다** — 이 포크의 여러 upstream
파일이 이미 비준수다(`components/net/fetch/methods.rs` 는 변경 전 기준 16 블록). 대신
**변경분이 해당 파일의 기존 스타일을 따르는지**로 판단한다.

## 리스크

- **포커스 체인** — 스파이크 2차 실행 로그에 `Aborting the focus operation - focus chain
  sanity check failed` 가 있었다. 표출 전용이라 당장 무해할 수 있으나 **원인 미확인**이다.
  구현 중 재현 여부를 확인하고, 재현되면 원인을 규명한 뒤 진행한다.
- **constellation 불변식** — "표출 부모는 있는데 문서는 top-level" 조합을 세션 히스토리 ·
  활성 전파 · 정리 경로가 어디까지 견디는지 미검증이다. 스파이크에서 단일 로드는 통과했다.
- **upstream 병합 충돌면** — script 와 constellation 을 동시에 건드리므로 리베이스 비용이
  늘어난다.
- **devtools 표현** — 부모 없는 문서가 devtools 트리에 어떻게 보이는지 미확인.

## 비목표 (v1 에서 의도적으로 제외)

*미구현* 이 아니라 *의도적 제외* 다. 문서에도 그렇게 적는다.

- 입력 라우팅 · 히트테스트 · 포커스
- 임베드된 문서의 history / session history 상호작용
- 임베드된 문서 안에서의 `window.open`, fullscreen 요청
- 접근성 트리 접합
- 부모 ↔ 임베드 문서 간 통신 채널
- `toplevel` 속성의 동적 추가/제거 반영
- 새 요소(`<wall-site>`) 도입 — 속성이 안정화된 뒤 별건으로 검토

# 애니메이션되는 transform 이 슬라이스 구조를 흔든다 — 설계

> 2026-10-29 · 대상: `components/layout/display_list/stacking_context.rs`
> 선행: `docs/superpowers/plans/2026-10-29-paint-side-iteration-boundary.md`(증상 A, 수정 완료)

---

## 1. 결함

★애니메이션되는 transform 의 **순간 위치**가 조상 스크롤 프레임의 `scrollable_overflow` 를
바꾸고, WebRender 는 그 값의 **문턱 판정**으로 picture-cache 슬라이스 루트를 고른다.★
그래서 애니메이션이 그 문턱을 지날 때마다 슬라이스 구조가 바뀐다.

사슬(전부 코드 확인):

1. `transform` 은 `recalculate_overflow` 리스타일 손상을 나른다
   (`third_party/stylo/style/properties/longhands.toml` 의 `[transform]`,
   `style/servo/restyle_damage.rs`). 틱마다 조상의 오버플로 메모가 지워진다
   (`components/layout/traversal.rs`).
2. 변환된 자식의 박스가 ★부모의 scrollable overflow 로 투영된다★
   (`components/layout/fragment_tree/box_fragment.rs:543-559` —
   `calculate_transform_matrix(...).outer_transformed_rect(...)`). LTR 에서 왼쪽 초과분은
   `clip_wholly_unreachable_scrollable_overflow` 가 걷어낸다.
   순효과: 띠 기준 `scrollable_size.width = max(0, (X + T) - W)`
   (X = 현재 translate, T = 글자 폭, W = 띠 폭).
3. 그 값이 `define_scroll_frame` 의 `content_rect` 로 WR 에 들어간다
   (`components/layout/display_list/stacking_context.rs:1067-1072`).
4. WR 의 `find_scroll_root` 이 ★문턱 둘★로 슬라이스 루트 자격을 판정한다
   (`vendor_local/webrender/src/spatial_tree.rs:74,77,394-409`):
   `scrollable_size > 0.01` ***그리고*** `viewport_rect > 128`. 자격을 갖춘 것이 없으면
   `outermost_scroll_root` 로 떨어진다(`:422-428`).
5. 스크롤 프레임은 ★WR 슬라이스를 가르는 유일한 것★이다
   (`vendor_local/webrender/src/tile_cache.rs:374-377` — `find_scroll_root` 이 달라지면 새
   슬라이스). `overflow: clip` 은 스크롤 프레임을 만들지 않아 슬라이싱에 기여하지 않는다
   (`stacking_context.rs:973-1079` — `hidden` 과 `clip` 의 구조적 차이는 이것 ***하나***다).

`X = W - T` 에서 `scrollable_size` 가 0 을 지난다 = ★글자가 띠 안에 완전히 들어온 순간.★

---

## 2. 실측 (2026-10-29, 개발 장비, `tests/html/ticker_slice_probe.html`)

| 관측 | 사슬이 설명하는가 |
|---|---|
| 개시 지점이 ★완전 등장★, 글자 폭(`text=`)에 따라 사이클 안에서 이동 | ✓ `X = W - T` |
| `clip=shell` ***또는*** `clip=band` 하나만으로도 사라진다 | ✓ outermost↔real 선택에 ★후보 둘★이 필요하다 |
| 타일을 잘게 하면(`-TileSize 512x512`) ★계속★ 발생, 슬라이스당 1 장(`display`, 기본)에서도 ★사이클당 1 번★ 남는다 | ✓ 타일은 ★증폭기★이고 원인이 아니다 |
| 전 띠가 ★같이★ 멈칫(`phase=same`), `phase=spread` 면 사라짐 | ✓ 다섯이 같은 순간에 문턱을 지난다 |
| fps 60 유지, 무감쇠 | ✓ 구조 재구성이고 비용 급증이나 캐시 워밍이 아니다 |
| `gfx_paint_side_animations_enabled=false` 면 사라짐 | ✓ 바인딩이 없으면 매 프레임 씬 재구성이라 ★특이 사건이 없다★ |
| `tick_divisor` 1↔15, `-FrameTime off`, 페이싱 클럭·커밋 모드·배리어 무영향 | ✓ 전부 다른 층 |
| ★Chrome 은 같은 마크업에서 발생하지 않는다★ | ✓ 같은 문턱 구조가 없다 — ***그래서 이것은 우리 결함이고 `overflow: clip` 은 해법이 아니다*** |

★설명 못 하는 것:★ "오른편에서 등장할 때" 의 별도 멈칫(거의 매 회, `512x512` 에서는 오히려
사라졌다). 별개 원인(글리프 래스터화 쪽)으로 본다. 이 설계의 대상이 아니다.

---

## 3. 왜 overflow 계산 자리를 고칠 수 없는가

처음에 `box_fragment.rs:543-559`(투영하는 자리)를 지목했는데 ★틀렸다.★

- `components/layout/fragment_tree/` 는 ★`DocumentAnimationSet` 을 전혀 보지 못한다★(grep 0건).
  애니메이션 세트는 디스플레이 리스트 층까지만 온다(`display_list/mod.rs:147`,
  `stacking_context.rs:123`, `paint_animation.rs`).
- `scrollable_overflow()` 는 ★인자 없는 메모 접근자★이고(`box_fragment.rs:305-310`) 여러 곳에서
  불린다. 애니메이션 세트를 거기까지 꿰면 광범위한 변경이 된다.
- 그리고 그 값은 ★CSS 가 정의한 값★이다. 스펙은 transform 을 반영하라고 한다. 값을 바꾸면
  `scrollTop`/`scrollWidth` 류의 관측 가능한 동작이 함께 바뀐다.

★그러므로 고칠 자리는 "값" 이 아니라 "그 값이 **슬라이스 결정으로 들어가는 지점**" 이다.★

---

## 4. 설계 — ★`define_scroll_frame` 의 `content_rect` 를 안정화한다★

`stacking_context.rs:1067-1072` 가 그 숫자가 WR 에 닿는 ***유일한*** 자리이고, 그 자리는
디스플레이 리스트 층이라 ★`stacking_context_tree.animations` 가 손에 있다.★

```rust
let scroll_tree_node_id = stacking_context_tree.define_scroll_frame(
    parent_scroll_node_id,
    external_scroll_id,
    self.scrollable_overflow().to_webrender(),   // ← 여기가 흔들린다
    scroll_frame_rect,
    sensitivity,
);
```

### 4-1. 안정화의 방향 — ★"항상 자격 있음" 으로 고정한다★

문턱을 ***넘나드는*** 것이 문제이므로, 한쪽으로 고정하면 된다. 두 방향 중:

| 방향 | 결과 | 판단 |
|---|---|---|
| 항상 ★자격 없음★ (`content_rect = frame_rect`) | 띠가 슬라이스 루트가 되지 못한다 | ★안 된다★ — `scrollable_size = 0` 은 `overflow: hidden` 의 스크립트 스크롤 가능성(`ScrollType::Script`)과 어긋나고, 다른 소비자에게도 보인다 |
| 항상 ★자격 있음★ | 띠가 자기 슬라이스를 갖는다 | ★이쪽★ — 연속 애니메이션 자손을 가진 상자를 분리하는 것이 ***슬라이스의 존재 이유***다 |

★WR 주석 자체가 그 의도를 적는다★(`spatial_tree.rs:398-404`): 작은 중첩 스크롤 루트에
슬라이스를 만드는 것은 "no performance benefit ... cheap to rasterize" 라서 건너뛴다는
것이다. ***연속으로 애니메이션되는 자손이 있으면 그 전제가 성립하지 않는다.***

### 4-2. 조건 — "이 스크롤 프레임 안에 paint-side 애니메이션되는 transform 이 있는가"

★빌드 순서 문제가 있다:★ 스크롤 프레임은 자손을 걷기 ***전***에 정의된다. 그래서 정의
시점에는 자손의 애니메이션 여부를 알 수 없다.

두 변형:

**(a) 두 단계 — 정의하고, 서브트리를 걷은 뒤 `content_rect` 를 고친다.**
`define_scroll_frame` 이 노드 id 를 돌려주므로, 서브트리 순회가 끝난 뒤 그 노드의
`content_rect` 를 패치한다. ★정확하다★(실제로 애니메이션되는 자손이 있을 때만 건드린다).
대가: spatial tree 노드를 사후 수정하는 경로가 필요하다 —
`components/shared/paint/display_list.rs` 의 `SpatialTree`/`SpatialTreeNodeInfo::Scroll` 에
setter 를 더해야 하고, 그 트리가 이미 직렬화된 뒤라면 순서를 봐야 한다.

**(b) 한 단계 — 선행 질의.** 스크롤 프레임 정의 시점에 ★그 박스의 자손 중 transform
애니메이션이 도는 노드가 있는지★를 애니메이션 세트에 직접 묻는다. 세트는 `OpaqueNode` 로
키가 걸려 있어 "이 서브트리에 속하는가" 를 묻기 어렵다 — fragment 서브트리를 한 번 걸어
tag 를 모으면 되지만 ★O(서브트리)★ 비용이 스크롤 프레임마다 든다.

★권고: (a).★ 정확하고, 비용이 자손 수에 비례하지 않으며, 조건이 거짓일 때 ***아무것도
바꾸지 않는다***(= 기본 동작 보존이 구조로 보장된다).

### 4-3. 얼마나 부풀릴 것인가

`MIN_SCROLLABLE_AMOUNT` 는 0.01 ***레이아웃 px*** 이다. 자격을 확실히 넘기려면 그보다
충분히 크고, 다른 소비자에게 의미 있게 보이지 않을 만큼 작아야 한다. ★`content_rect` 를
`frame_rect` 기준으로 1 레이아웃 px 만 넓힌다★(흔들리는 축에서만). 근거를 상수 doc 에 적는다.

★주의:★ `viewport_rect > 128` 문턱은 건드리지 않는다. 띠가 그보다 작으면 애초에 자격이
없고 뒤집힐 것도 없다 — 그 경우 이 변경은 무동작이다(실측으로도 뷰포트 높이 1024 CSS px
아래에서는 증상이 없어야 한다, 5 절 참조).

---

## 5. 검증

### 5-1. 단위

`servo-paint-api`(이 트리에서 유일하게 테스트가 도는 크레이트 —
`cargo test -p servo-layout` 은 테스트 프로파일이 `aws-lc-sys`/NASM 을 끌어와 실패한다)에
안정화 산수를 순수 함수로 두고:

- 애니메이션 자손이 없으면 ★입력 그대로★ 돌려준다(기본 동작 보존)
- 있으면 `scrollable_size` 가 ★`MIN_SCROLLABLE_AMOUNT` 를 확실히 넘는다★
- 흔들리지 않는 축은 건드리지 않는다
- `frame_rect` 가 0 이거나 비정상일 때 패닉하지 않는다

### 5-2. 실기 (개발 장비, 기본 설정)

```
ticker_slice_probe.html?speed=10&text=<50자>     ② 가 사라져야 한다
  ... &-TileSize 512x512                         ★계속 발생하던 것도 사라져야 한다★
  ... &clip=all                                  변화 없음(원래 깨끗)
```

①("오른편 등장 시")은 ★남을 것으로 예상한다★ — 이 설계의 대상이 아니다. 남는다고 해서
이 수정이 실패한 것이 아니고, ★그 구분을 보고에 명시한다.★

### 5-3. 반증 조건을 미리 적는다

★`-TileSize 512x512` 에서 여전히 계속 발생하면 이 설계는 틀렸다★ — 슬라이스 구조를
고정했는데도 남는다면 원인이 슬라이스가 아니라 타일 쪽이라는 뜻이다.

그리고 ★뷰포트 높이 1024 CSS px 아래에서는 수정 전에도 증상이 없어야 한다★(띠 높이가
12.5% 이므로 `viewport_rect.height() > 128` 레이아웃 px 를 못 넘는다). 이것이 참이면 4 절의
사슬이 한 번 더 확인되고, 거짓이면 ★문턱 둘 중 어느 것이 관여하는지 다시 봐야 한다★ —
구현 전에 돌려 볼 가치가 있는 ★공짜 시험★이다.

---

## 6. 범위 밖

- ★4 타일 벽의 저더는 양상이 다르다★(운영자 확인). 이 수정의 대상이 아니다.
- ①(오른편 등장 시 멈칫, `512x512` 에서 사라짐) — 별개 원인, 별개 작업.
- `find_scroll_root` 에 히스테리시스를 넣는 길(= WR 쪽 수정)은 ★채택하지 않는다★:
  `vendor_local/webrender` 포크를 키우고, 결함의 원천이 아니라 증상 쪽이다. 다만 4 절이
  실패하면 돌아올 후보다.

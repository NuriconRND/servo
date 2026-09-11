# 전환 애니메이션 종료 직후 검은 화면 (브랜치 `wall-animation-end-black-frame`)

기준 브랜치: `webgl-content-memory-leak`
마지막 커밋: `54932fb6744`
상태: **조치 넣고 실기 검증 대기 중** (log_ani_debug/08 이 다음 차례)

---

## 1. 증상 (사용자 보고 원문 기준)

세 가지가 보고되었고, 셋 다 미해결이다.

1. **24개 rtsp 라이브스트림 구성으로 전환할 때, 전환 애니메이션이 끝난 직후
   1~2초(나중에는 1초에 조금 못 미치게) 모든 영상이 사라지고 검은 화면만 나온
   뒤 다시 재생된다.** 애니메이션이 **도는 동안에는 영상이 정상 출력**된다.
   WebGL 구성에서도 같은 현상이 나타났다.
   - log_ani_debug/07 기준: WebGL → rtsp 24 전환 16번 중 **13번**에서 발생.
2. **전환될 구성의 target position 에 그 구성의 프레임이 잠깐 출력**된다.
   WebGL 구성으로 전환할 때 매번.
3. **구성의 컨텐츠가 준비되기 전에 전환 애니메이션이 먼저 재생**되고, 준비
   완료(또는 타임아웃) 시 다시 재생된다.

간헐적이다. 한 사이클에서 안 나타나는 경우가 있다(16번 중 3번).

---

## 2. 확정된 사실

### 2-a. 검은 화면의 정체 — 슬롯 둘이 동시에 화면 밖에 있다

07, 11:24:37 (그 초의 `WRRATE resolves` 합계 = 0):

```
node ...872   translate= +11520.0   names=[sd-anim-18]   바인딩 없음
node ...912   translate= -11520.0   names=[sd-anim-19]   바인딩 없음
doc=[]                                    문서에 애니메이션 0개
painter       play=false  tx=[]           미는 값 없음
```

가상 뷰포트 폭이 **11520**(`config/wall_layout.multigpu.json`)이다. 페이지의 두
슬롯 컨테이너는 기본 위치가 각각 `translateX(±100%)` = ±11520, 즉 화면 밖이고,
**그중 하나를 0 으로 데려오는 유일한 수단이 애니메이션 값**이다. 값이 사라지면
둘 다 기본 위치로 돌아가 화면에 아무것도 남지 않는다.

★**영상은 그동안에도 계속 디코드·소비된다**★ — 같은 구간 `MEDIALOCKRATE
consumes=28~116` 정상. **사라지는 것은 영상이 아니라 그것을 담은 컨테이너의
위치다.** 애니메이션은 재생에 관여하지 않는다.

측정 지표: `WRRATE resolves` (그 프레임에 해소된 외부 이미지 수). 검은 구간에
프레임은 60fps 로 계속 나가는데 `resolves` 가 0이고 `render_ms` 가 프레임당
0.13ms 로 떨어진다 — 씬이 비었다는 뜻이다.

### 2-b. 근본원인 — 세트를 지우면 그 노드는 다시는 리스타일되지 않는다

애니메이션을 (다시) 만드는 곳은 `maybe_start_animations` 하나이고, 그것은 그
요소가 **리스타일될 때만** 돈다. 그런데 애니메이션이 리스타일을 유발하는 곳은
`Animations::mark_animating_nodes_as_dirty` 이고:

```rust
for node in sets.keys().filter_map(|key| rooted_nodes.get(&NoTrace(key.node))) {
    node.dirty(NodeDamage::Style);
}
```

★**세트가 있는 노드만 더럽힌다.**★ 그래서 `do_post_reflow_update` 의
`sets.retain(|_, s| !s.is_empty())` 이 세트를 지우면 그 노드는 목록에서 빠지고,
다시는 리스타일되지 않으며, 리스타일되지 않으니 애니메이션도 다시 만들어지지
않는다. **되살릴 유일한 경로가 "이미 애니메이션이 있는 요소" 에만 도는
자기잠금이다.**

실측(07, node 64280494740912, **줄 번호 = 실행 순서**. 로그가 1초 해상도라
줄 번호로 순서를 본다):

```
5447   ANIMSTART sd-anim-4  started_at= 9.718
7470   lost set_missing     names=[sd-anim-4]  translate=11520
8545   (93 DL 동안 그대로)  names=[sd-anim-4]  translate=11520
9164   ANIMSTART sd-anim-4  started_at=13.243     <- 3.5초 뒤 같은 이름 재생성
11392  lost set_missing     names=[sd-anim-4]
14845  ANIMSTART sd-anim-4  started_at=18.036     <- 4.8초 뒤 또
17131  lost set_missing     names=[sd-anim-4]
18142  ANIMSTART sd-anim-4  started_at=20.578     <- 2.5초 뒤 또
```

그 사이 `ANIMCANCEL` 은 **하나도 없다**. 스타일은 계속 `sd-anim-4` 를 지명하고
(`PAINTANIMSTATIC ... names=[sd-anim-4]`), `@keyframes` 도 있다
(`ANIMNOKEYFRAMES=0`). **이름이 있고 키프레임도 있는데 애니메이션 객체만 없는
상태**가, 페이지가 그 요소를 다른 이유로 건드릴 때까지 이어진다. 간격이
2.5~4.8초이고 그 앞 93 DL(약 0.8초)이 검은 화면이다.

간헐적인 이유도 이것으로 설명된다 — 삭제 직후 페이지가 곧 그 요소를 건드리면
짧게 지나가고(16번 중 3번), 아니면 드러난다.

### 2-c. 부수적으로 찾아 고친 별개 결함

`Pending -> Running` 승급이 `update_for_new_timeline_value` 한 곳에만 있었고,
그건 페인트 refresh driver 의 애니메이션 틱을 타고 온다. 그 틱은 두 겹으로
조건부다:

- 페인트가 재생 중인 WebView 는 `gfx_paint_side_animation_tick_divisor`(기본 4)
  프레임마다 한 번만 받는다.
- "애니메이션이 있다" 신호가 꺼지면 refresh driver 가 관찰 자체를 그만둔다
  (`ANIMTICK stop`). 그 신호는 `sets.retain` 이 세트를 지운 **뒤에** 계산된다.

그래서 세트가 한 번 비면 틱이 끊기고, 틱이 끊기면 그 뒤에 만들어진 애니메이션은
영영 시작하지 못한다. 리스타일도 못 구한다 — `Animation::update_from_other` 의
승급은 `old_state != Pending` 일 때만 돌기 때문에 이미 Pending 인 것은 몇 번을
재제출해도 Pending 이다.

실측(05): `sd-anim-85` 가 **9초 동안** `Pending p=0.000 delay=0` 에 머물렀다.

조치: 승급을 `do_post_reflow_update` 에서도 돌린다(커밋 `ad1412abd60`).
06 에서 `ANIMPENDING=0`, `ANIMSTART=201` 로 확인됐다. 사용자가 보고한
"복귀 타이밍이 1초보다 조금 짧아진 것 같다" 가 이 효과로 보인다.
**이것만으로는 주 증상이 해소되지 않는다.**

---

## 3. ★반증된 가설 — 다시 하지 말 것★

한 줄도 빠짐없이 실측으로 잘렸다. 같은 길을 다시 가면 안 된다.

| 가설 | 반증 |
|---|---|
| 끝난 페인트 애니메이션의 값을 다음 프로퍼티 트랜잭션이 지운다 (`reset_dynamic_properties`) | `held=0` × 280줄. 그 경로(`running()==false`)가 이 워크로드에서 **한 번도 안 밟힌다** — 디스플레이 리스트가 항상 먼저 통째로 갈아치운다. 커밋 `3188d88e98a` 는 별개의 잠재 결함 수정으로 남겨 둠 |
| rtsp 첫 프레임이 1~2초 늦어서 그릴 게 없다 | `MEDIARINGS` 를 애니메이션 구간과 대조하면 반대다. 애니메이션이 도는 동안 rings 는 차 있고 끝나는 순간부터 무너진다. 사용자 교정: "애니메이션 도는 동안엔 이미 잘 재생되고 있다" |
| `readyState` 가 첫 프레임 전에 HaveEnoughData 로 뛴다 | 위와 같은 이유로 무관 (실제 코드 사실이긴 하나 이 증상의 원인이 아님) |
| 애니메이션이 `Canceled` 되어 fill 이 버려진다 (스타일 검사 경로) | `ANIMCANCEL` 은 전부 `reason=name_gone new_names=[none]` 이고, 문제 구간에는 **한 번도 안 찍힌다** |
| 레이아웃이 "렌더링 안 됨" 으로 판정해 취소한다 | `ANIMDROP` 14~16회뿐, 전부 `connected=false` + 이미 Canceled. 진짜로 트리에서 빠진 노드의 뒷정리 |
| `@keyframes` 를 stylist 가 아직 못 찾는다 | `ANIMNOKEYFRAMES=0` |
| `PAINTANIM built` 의 `not_in_set` 급증이 결함 신호 | 애니메이션 없는 타일 요소를 세는 정상값. 평소에도 수십 |
| 노드 주소가 같으니 같은 요소다 | `OpaqueNode` 는 **주소**다. 같은 id 가 같은 요소라는 보장이 없다 (07 에서는 3분 30초 동안 안정적이라 같은 요소로 판단했지만, 근거로 쓸 때는 항상 이 단서를 붙일 것) |

### ★내가 반복한 프레이밍 오류★

`unbound_dls` 는 "바인딩 없는 구간의 길이" 인데, 그 구간이 **끝나는** 지점에서
보고된다. 그것만 보고 "애니메이션이 늦게 생긴다" 로 읽었고, 사용자가 보고한
"애니메이션이 끝난 직후" 와 정반대로 설명하게 됐다. **사용자가 두 번 교정해
줬다.** 같은 구간을 어느 쪽 끝에서 부르는지 먼저 정하고 말할 것.

---

## 4. 넣은 조치 (검증 대기)

커밋 `54932fb6744`: `sets.retain` 이 세트를 지우기 **직전에** 그 노드를
`NodeDamage::Style` 로 표시한다.

```rust
let rooted_nodes = self.rooted_nodes.borrow();
sets.retain(|key, state| {
    let keep = !state.is_empty();
    if !keep {
        log::warn!("ANIMSETDROP node={} pseudo={:?}", key.node.0, key.pseudo_element);
        if let Some(node) = rooted_nodes.get(&NoTrace(key.node)) {
            node.dirty(NodeDamage::Style);
        }
    }
    keep
});
```

다음 리플로에서 `update_animations_for_new_style` 이 그 요소에 대해 돌고,
스타일이 여전히 이름을 지명하면 애니메이션이 되살아나며, 지명하지 않으면 아무
일도 일어나지 않는다. 비용은 삭제 한 번당 리스타일 한 번.

삭제 조건도 취소 경로도 건드리지 않았다. 빠져 있던 "다시 볼 기회" 만 돌려준다.

---

## 5. 다음에 할 일 (재개 지점)

### 5-a. 실기 검증

사용자에게 배포본을 주고 **WebGL ↔ rtsp 24 반복** 으로 돌린다(재현율이 제일
높다). 로그에서 셋을 본다:

1. `edge=lost` 직후의 `ANIMSTART` 간격 — 지금 2.5~4.8초. **리플로 한두 번
   (수십 ms)** 으로 줄어야 한다.
2. `PAINTANIMSTATIC ... names=[sd-anim-N]` 의 `unbound_dls` — 지금 93, 최대 780.
   한 자릿수로 떨어져야 한다.
3. `WRRATE resolves` — 전환 직후 무너지는 초가 없어야 한다.

### 5-b. 안 고쳐졌을 때 다음 자리

`unbound_dls` 가 여전히 크고 `ANIMSTART` 가 늦으면, 더티 표시는 됐는데 그 요소가
실제로 리스타일되지 않는다는 뜻이다. 그때 볼 곳:

- `NodeDamage::Style` 이 이 경로에서 충분한 손상 등급인지.
- `rooted_nodes` 에 그 노드가 들어 있는지(`unroot_unused_nodes` 가 이미
  빼 갔다면 `dirty` 자체가 안 불린다) — `ANIMSETDROP` 옆에 rooted 여부를 같이
  찍어 보면 한 번에 갈린다.

### 5-c. 남은 증상 2, 3

증상 1이 해소된 뒤에 따로 본다. 증상 2("target position 프레임")는 같은 기전의
짧은 쪽으로 보이며 `unbound_dls` 가 1~2 로 남을 수 있다. 완전히 없애려면 요소가
디스플레이 리스트에 들어간 첫 프레임과 애니메이션이 붙는 프레임 사이를 없애야
하고, 그건 별도 조치다.

---

## 6. 계측 목록 (전부 이 브랜치에 들어 있음)

동작은 바꾸지 않는다. 로그 레벨은 `warn!` (런처의 RUST_LOG 이 `warn` 로 시작).

| 태그 | 위치 | 무엇을 답하나 |
|---|---|---|
| `PAINTANIMEDGE edge=lost/gained` | `layout/display_list/paint_animation.rs` | 바인딩이 붙고 떨어지는 **그 프레임**. `lost` 에 `reason=` 와 애니메이션 상태, `gained` 에 `unbound_dls=`(그 요소가 자기 스타일로 그려진 디스플레이 리스트 수 = ★잘못된 프레임의 길이★) |
| `set_missing doc=[...]` | 같은 곳 | 그 순간 문서가 들고 있는 키. 한 번도 묶어 본 적 없는 id 뿐이면 새 요소, 우리가 잃은 id 가 있으면 항목이 밑에서 없어진 것 |
| `PAINTANIMSTATIC` | `paint_animation.rs` + `stacking_context.rs` | 바인딩 없이 **실제로 구워진 transform** 과 그때 요소의 계산된 `animation-name`. ★이름이 있는데 애니메이션이 없는 것과 이름 자체가 없는 것은 다른 결함이고, 그려진 값만으로는 안 갈린다★ |
| `ANIMSTART` / `ANIMPENDING` | `script/animations.rs` | 승급이 일어난 순간 / `started_at > now` 라 못 한 경우 |
| `ANIMSETDROP` | 같은 곳 | `sets.retain` 이 지우는 항목 |
| `ANIMDROP` | 같은 곳 | "렌더링 안 됨" 판정으로 취소하는 경로 + `connected=` |
| `ANIMUNBIND` | 같은 곳 | 노드가 트리에서 빠질 때(`unbind_from_tree`) 취소하는 경로 |
| `ANIMCANCEL` | `third_party/stylo/style/servo/animation.rs` | 스타일 검사 취소 + **새 스타일이 실제로 든 animation-name 목록** |
| `ANIMNOKEYFRAMES` | 같은 곳 | 스타일이 이름을 대는데 stylist 에 `@keyframes` 가 없어 애니메이션이 아예 안 만들어지는 경우 |
| `held=` (PAINTANIM) | `paint/painter.rs` | 끝난 페인트 애니메이션의 값이 도로 얹힌 개수 |

★로그 폭주 방지★: 모서리 로그는 `lost`/`gained` 예산을 **따로** 둔다(각 초당
30줄). 전환은 수십 노드를 한꺼번에 묶으므로 예산이 하나면 `gained` 폭주가 정작
필요한 `lost` 한 줄을 같은 초에 밀어낸다. `PAINTANIMSTATIC` 은 첫 unbound DL 에
한 번 + 이후 60개마다 한 번. `ANIMNOKEYFRAMES` 는 이름이 바뀔 때 + 60회마다.

---

## 7. 빌드 · 배포 · 실행

```powershell
. W:\scripts\servo_env.ps1                      # W: = subst 된 프로젝트 루트
Set-Location W:\servo_multigpu-tiled-wall
cargo build -p servo --example winit_wall --features media-gstreamer,no-wgl,webgpu --release
.\etc\multigpu\make_wall_dist.ps1 -Force        # -> target\wall_dist (dll=444, 0.44GB)
```

★`-p servo` + `no-wgl` 이 없으면 surfman 이 wgl 백엔드로 잡혀
`create_isolated_device` / `d3d11_device_ptr` 가 없다고 컴파일이 깨진다.★
`cargo check -p servo-paint` 단독도 같은 이유로 깨진다 —
`--features webgl/no-wgl,webgpu` 를 줄 것.

테스트 장비 실행(사용자가 수행):

```powershell
.\run_wall_dist.ps1 -Layout wall_layout.multigpu.json -Url "https://192.168.1.86:7245/ipwall/#/output" `
  -IgnoreCertErrors -PageFeatures -Pref dom_webgl2_enabled=true,dom_webgpu_enabled=true,gfx_precache_shaders=true `
  -DComp surface -ParallelTiles -PipelineMode uridecodebin3 -DecoderThreads 1 -SinkPacing thread `
  -NoSyncGroup -DurationSec 120 -PresentCadence -FanoutProf -ThreadCpu -LogPath ani_debug_NN.err.log
```

로그 수집 위치:
`\\192.168.1.214\share\0_imsi\손승관\20260814_@795_SDWall_multigpu\wall_dist\log_ani_debug\NN`

---

## 8. 제약 (사용자 지시)

- 수정은 **엔진(winit_wall 에 적용되는 것)에 한한다**. `F:\20260609_SDWall_BrowserTest\frontend` / `backend` 의 `/output` 페이지 코드는 **참고용이며 건드리면 안 된다**.
- ★**명시하지 않는 한 페이지 쪽에 책임을 돌리는 추론을 하지 말 것**★ — 전달되는 수정 요청은 보통 그 페이지가 상용 브라우저에서 정상 동작함을 확인한 뒤에 오는 것이다.
- 커밋 메시지에 Claude 서명·세션 주소를 넣지 않는다.
- 스테이징 금지 파일 3개: `etc/multigpu/config/wall_layout.example_1x1.json`,
  `tests/html/multigpu_standard_video_extended_probe.html`,
  `tests/html/multigpu_standard_video_rtsp_probe.html`.

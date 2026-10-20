# DComp 커밋·BeginDraw 계측과 서피스별 자원 재사용 설계

- 작성: 2026-10-20
- 근거 분석: `F:\20260609_SDWall_BrowserTest\frontend\ANALYSIS-output-ticker-jitter.md`(이하 분석서)
- 대상 브랜치: `wall-sample-index-slip` cba6e697388
- 다루는 것: 분석서 7절의 **W13**(정체 위치 계측) 중 커밋·BeginDraw 부분과 **W1**(bind 마다 GPU 자원을 만들고 지우는 구조)
- 다루지 않는 것: W16(`overflow: hidden` pref), W14(빈 트리 경쟁), W15(페이싱 루프), 프론트 P-clip

---

## 0. 왜 이 두 가지를 한 스펙에 묶나

분석서가 좁힌 사슬은 이렇다.

```text
IPWall 저더
  = 프레임 전달 간격 불균일(값은 정확하다. 한 프레임 -42.33px 고정)
  <- 20ms 넘는 렌더 정체가 초당 5 회
  <- 그 정체의 거의 전부가 compositor_begin_ms(22~24ms), draw_frame_ms 는 1ms 미만
  <- begin_frame 안에서 블로킹할 수 있는 것은 Commit() 하나뿐
```

그런데 ★그 Commit 만 계측이 없다★. `commit_ns` 는 `commit_device_locked`(메서드)만 재고,
`begin_frame` 이 부르는 경로는 `commit_device_ptr_locked`(자유 함수)라 타이머를 지나지 않는다.
그래서 모든 실행에서 `commit_ms=0.0` 이 찍혔고, 22ms 정체의 정체는 ★소거법 추정★으로 남았다.
한 번은 그 추정을 `BeginDraw` 로 잘못 짚기도 했다 -- `compositor_begin_ms` 를 BeginDraw 시간으로
읽은 것인데, 실제 BeginDraw 는 `DCOMPBIND begin_ms` 로 이미 재고 있었고 호출당 0.08~0.31ms 였다.

동시에 분석서는 **유력(미증명) A1** 을 세웠다 -- bind 마다 갱신 영역 크기로 pbuffer 를 새로
만들고, 그 크기가 글자 위치에 따라 프레임마다 달라져 할당이 반복된다. 관측과 맞는 점이 둘이다:
가장자리에서만 비싸진다는 비용 지도, 그리고 GPU 메모리가 글자 주기(6 초)로 수백 MB 씩
오르내린다는 것.

**계측과 수정이 같은 코드 지점에 있다.** 계측 없이 고치면 효과를 판정할 수 없고, 고치지 않고
계측만 하면 A1 을 확인만 하고 끝난다. 그래서 한 스펙으로 묶되 ★계측을 먼저 들여보내고, 그
실측으로 수정의 방향을 정한다★.

---

## 1. 지금 코드가 하는 일 (전부 확인함)

### 1.1 Commit 호출 지점

`Commit()` 을 실제로 부르는 자리는 크레이트 전체에서 `raw_commit`(`dcomp_compositor.rs:1705`)
하나다(예제 바이너리 제외). 그리로 들어오는 경로는 여섯이다.

| # | 경로 | 함수 | 계측 | 스레드 |
|---|---|---|---|---|
| 1 | `end_frame` 안의 즉시 커밋 | `commit_device_locked` :2652 | O (`commit_ns`) | 렌더 |
| 2 | 미뤄 둔 커밋 흘리기 | `flush_deferred_commit` :2634 -> `commit_device_guarded` :2676 -> 1 | O (같은 카운터) | 렌더/셸 |
| 3 | **`begin_frame` 의 마감 선반출** | `commit_scheduler::flush_before_render` :270 -> `commit_device_ptr_locked` :1784 | **X** | **렌더** |
| 4 | 스케줄러 워커 | `commit_scheduler::worker_loop` :167 -> `commit_device_ptr_locked` | **X** | 커밋 워커 |
| 5 | 셸 즉시 커밋(폴백) | `commit_device_ptr` :1760 | **X** | 셸 |
| 6 | 비디오 fast-path | `present_external_only` :2843 | **X** | 렌더 |

★3 번이 문제의 그 호출이다★ -- `begin_frame` 이 매 프레임 부르고, `OUTCOMMIT` 의
`early/scheduled` 가 0.65~0.81 이므로 커밋의 2/3~3/4 가 이 경로로 나간다. 그리고 계측이 없다.

### 1.2 `begin_frame` 이 하는 일 (`dcomp_compositor.rs:3491-3577`)

```text
begin_frames += 1          (수만 센다. 시간은 안 잰다)
close_external_batch()
flush_deferred_commit()    -> 경로 2
flush_before_render()      -> 경로 3   ★여기가 22ms 로 의심되는 자리★
root SetOffsetX/Y          (값이 바뀔 때만)
RemoveAllVisuals()
frame_surfaces.clear()
```

WR 쪽 `compositor_begin_ms`(`vendor_local/webrender/src/renderer/mod.rs:1746-1753`)가 이 함수
전체를 잰 값이다. 즉 22~24ms 는 위 다섯 줄 어딘가의 합이고, ★안을 쪼갠 적이 없다★.

### 1.3 bind 가 하는 일 (`dcomp_compositor.rs:3156-3310`, Virtual arm)

```text
expand_transparent = !불투명 && dirty == valid          (IPWall 알파 슬라이스가 여기 해당)
update_rect_v      = expand ? (타일 ∩ 서피스클립) : (타일 + dirty)
BeginDraw(update_rect_v)   -> (AddRef 된 텍스처, 아틀라스 오프셋, desc)   [begin_ns]
create_render_pbuffer_from_d3d_texture(텍스처, desc 크기)                 [pbuffer_ns]
make_render_pbuffer_current(pbuffer)
expand 면 scissor(update_offset, update 크기) 투명 클리어
... WR 이 그린다 ...
unbind: EndDraw                                                           [end_ns]
        destroy_render_pbuffer(pbuffer)                                   [destroy_ns]
        texture->Release()
```

세 가지가 **bind 마다** 생겼다 사라진다.

1. DComp 가 `BeginDraw` 로 내주는 텍스처(아틀라스 슬롯일 수 있다)
2. 그 텍스처를 감싼 EGL pbuffer (`third_party/surfman/src/platform/windows/angle/device.rs:589-624`)
3. 그 pbuffer 의 깊이·스텐실 -- 컨텍스트 설정이 `ALPHA|DEPTH|STENCIL`
   (`components/shared/paint/rendering_context.rs:852-854`)이라 ANGLE 이 같은 크기로 함께
   만드는 것으로 보인다(ANGLE 내부는 확인하지 않았다)

★그리고 `update_rect_v` 가 프레임마다 달라진다★ -- `expand_transparent` 경로의 서피스 클립은
직전 `add_surface` 의 `clip_rect`(`entry.last_placement`, :3052)이고, 그 값은 WR 쪽에서
"device_clip ∩ 모든 타일 valid_rect 의 바운딩 유니온"이라 글자가 움직이면 같이 움직인다.
크기가 달라지면 DComp 는 다른 슬롯·다른 크기의 텍스처를 내주고, 그러면 pbuffer 와 깊이·스텐실도
매번 새 크기로 만들어진다.

### 1.4 이미 있는 선례 -- SwapChain 쪽은 이미 재사용한다

`SurfaceStorage::SwapChain` 은 `frame_pbuffer: Option<FramePbuffer>`(:666)를 들고 있고,
`if sc.frame_pbuffer.is_none()` 일 때만 만든다(:3077). 해제는 `release_frame_pbuffer`(:607)
하나로 모아 두고 파기 지점마다 부른다(:2896 release_all, :4093 end_frame, :4185 regen,
:4239 demote, :4459 destroy_surface).

★Virtual 쪽에만 그 구조가 없다.★ 이 스펙의 W1 은 그 선례를 Virtual 에 그대로 옮기는 일이다.

---

## 2. 이 스펙이 답해야 할 질문

| Q | 답하는 계측 | 판정 |
|---|---|---|
| Q1 | 22ms 를 먹는 것이 정말 `flush_before_render` 의 `Commit()` 인가 | `COMMITSITE br_max` 와 `DCOMPBIND bf_early_ms_max` | 둘 다 22 근처면 확정 |
| Q2 | 아니라면 `begin_frame` 의 어느 줄인가 | `bf_deferred` / `bf_early` / `bf_removeall` / `bf_total` | 합과 total 의 차이가 남은 몫 |
| Q3 | 정체가 느린 커밋 한 번인가 느린 커밋 여럿인가 | `*_max` 와 `*_over` | max 만 크면 정체, over 가 크면 부하 |
| Q4 | 갱신 렉트가 실제로 프레임마다 달라지는가 (A1) | `update_sizes`, `tex_sizes`, `tex_changed/binds` | IPWall 에서 크고 프로브에서 1 이면 A1 성립 |
| Q5 | GPU 메모리 톱니가 그 할당으로 설명되는가 | `alloc_mb` | `GPUMEM` 진폭과 자릿수가 맞는가 |
| Q6 | 재사용이 실제로 먹히는가 | `tex_reused/binds` | 수정 후 binds 에 근접해야 한다 |

Q4 가 "아니오"면 W1 은 취소한다. 그래서 ★계측이 먼저다★.

---

## 3. W13-C: 커밋 계측

### 3.1 호출 지점마다 따로 센다

`raw_commit` 에 호출 지점을 받는 인자를 추가한다. 여기가 유일한 통로이므로 ★새 커밋 지점이
생겨도 계측을 빠뜨릴 수 없다★ -- 이 파일이 이미 같은 이유로 디바이스 가드를 이 함수 쪽에 모았다.

```rust
/// Commit 을 부른 자리. `COMMITSITE` 한 줄이 이것으로 갈린다.
///
/// ★`before_render` 하나만 계측이 없었고 하필 그것이 전체의 2/3 였다★ -- OUTCOMMIT
/// early/scheduled 0.65~0.81. 그래서 `commit_ms` 가 모든 실행에서 0.0 이었고, 22ms 정체의
/// 정체가 소거법 추정으로 남았다.
#[derive(Clone, Copy)]
pub(crate) enum CommitSite {
    EndFrame,     // end_frame 즉시 커밋
    Deferred,     // flush_deferred_commit
    BeforeRender, // begin_frame -> commit_scheduler::flush_before_render
    Scheduler,    // 커밋 워커
    Direct,       // 셸 즉시 커밋(폴백)
    External,     // 비디오 fast-path
}
```

바꾸는 시그니처는 넷이다.

- `unsafe fn raw_commit(device, site)`
- `fn commit_device_locked(&mut self, device, site)` -- `end_frame` 은 `EndFrame`,
  `commit_device_guarded` 는 호출자가 넘긴 값을 그대로 전달(`Deferred`)
- `pub fn commit_device_ptr_locked(device, site)` -- `flush_before_render` 는 `BeforeRender`,
  `worker_loop` 은 `Scheduler`
- `pub fn commit_device_ptr(device)` -- 셸 ABI 를 지키려고 시그니처는 두고 내부에서 `Direct` 고정

### 3.2 누적기

`raw_commit` 안에서만 쓰는 전역 배열이다. `self` 가 없는 경로(:1768, :1785)가 있으므로
컴포지터 필드에 둘 수 없다.

```rust
struct CommitSlot {
    n: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
    over_n: AtomicU64, // SLOW_COMMIT_MS 를 넘긴 횟수
}
static COMMIT_SITES: [CommitSlot; 6] = [...];
```

`*DCOMP_BIND_PROF` 가 꺼져 있으면 `Instant::now` 를 부르지 않는다(이 파일의 모든 계측과 같은 규약).

★로그는 임계구역 안에서 절대 찍지 않는다.★ 이 파일이 세 번 고쳐 세운 불변식이다 -- 가드 안에서
하는 일은 `Commit()` 하나뿐이고 그 상한을 `lock_wait_painter_us_max` 가 감시한다.
`fetch_add`/`fetch_max` 는 락이 없으므로 이 불변식을 깨지 않는다.

### 3.3 느린 커밋 문턱

디버그 환경변수를 하나 추가한다(`components/config/debug_env.rs`, `cache_index: 21`,
`ALL` 끝에 등록).

```text
SERVO_DCOMP_SLOW_COMMIT_MS   Kind::Int   기본 8
```

창 안에서 이 값을 넘긴 커밋의 수가 `over` 다. ★`max` 하나로는 "22ms 짜리 한 번"과 "22ms 짜리
60 번"이 구분되지 않는다★ -- 앞은 정체고 뒤는 구조적 부하라 고칠 방법이 정반대다.

### 3.4 한 줄 내기

★방출자는 하나다.★ 카운터가 전역인데 페인터 넷이 각자 1 초 창으로 비우면 한 줄이 실제로는
0.25 초 조각이 된다 -- `OUTCOMMIT` 이 이미 같은 이유로 방출자를 스케줄러 스레드로 옮긴 전례가
있다(Fix round 1, Ruling 16).

```rust
/// COMMITSITE 를 낼 컴포지터 하나를 선착순으로 정한다. 0 = 미정.
static COMMITSITE_EMITTER: AtomicUsize = AtomicUsize::new(0);
```

`maybe_emit_bind_profile` 안에서 자기 디바이스 포인터로 `compare_exchange(0, me)` 를 시도하고,
성공했거나 이미 자기면 그 창에 한 줄을 더 낸다. `release_all` 에서 자기 주소면 0 으로 되돌린다.

★`OUTCOMMIT` 쪽이 아니라 `DCOMPBIND` 쪽에 붙이는 이유★: 스케줄러 스레드는
`gfx_present_align_commit_pct` 가 꺼져 있으면 아예 뜨지 않는다. 그런데 A/B 1 순위 arm 이
바로 `-CommitAlign -1` 이고, 그 실행에서 커밋 시간을 못 보면 arm 자체가 무의미해진다.

로그 형식(한 줄, 줄바꿈은 가독성용):

```text
COMMITSITE window_ms=1002 slow_ms=8
  ef n=0   ms=0.0   max=0.00  over=0
  df n=2   ms=0.1   max=0.05  over=0
  br n=252 ms=481.0 max=22.91 over=63
  sc n=56  ms=1.2   max=0.31  over=0
  di n=0   ms=0.0   max=0.00  over=0
  ex n=0   ms=0.0   max=0.00  over=0
```

### 3.5 `begin_frame` 쪼개기

```rust
bf_total_ns / bf_total_ns_max         // begin_frame 전체(= WR 의 compositor_begin_ms)
bf_deferred_ns / bf_deferred_ns_max   // flush_deferred_commit
bf_early_ns / bf_early_ns_max         // flush_before_render        ★주범 후보★
bf_removeall_ns / bf_removeall_ns_max // RemoveAllVisuals
```

`DCOMPBIND` 줄에 붙인다. ★합과 total 의 차이를 남겨 둔다★ -- 셋의 합이 total 에 못 미치면
`SetOffsetX/Y` 나 `close_external_batch` 같은 남은 줄이 범인이고, 그건 아무도 의심하지 않은
자리다. 계측이 "남은 몫"을 보여주지 않으면 그 가능성은 영영 보이지 않는다.

`bf_early_ns` 와 `COMMITSITE br_*` 는 ★같은 호출을 두 겹으로 잰다★. 의도한 것이다 -- 둘이
갈라지면 `flush_before_render` 의 큐 락이나 `retain` 이 범인이고, 같이 크면 `Commit()` 자체다.

---

## 4. W13-B: BeginDraw·갱신 렉트 계측

### 4.1 분포의 꼬리를 남긴다

지금은 `begin_ns`, `pbuffer_ns`, `end_ns`, `destroy_ns` 가 합계뿐이다. 각각 `_max` 를 붙인다.
이유는 3.3 과 같다.

### 4.2 갱신 렉트와 텍스처 크기

bind 마다 이미 손에 있는 값이다(`update_rect_v`, `desc.Width/Height`). 추가로 센다.

```rust
update_px_max: u64,    // 갱신 요청 면적 최대
update_sizes: SizeSet, // 창 안에서 서로 다른 (w,h) 의 수
tex_px_max: u64,
tex_sizes: SizeSet,
alloc_px: u64,         // 새로 만든 pbuffer 들의 텍스처 면적 합 -> alloc_mb 로 환산
```

`SizeSet` 은 ★할당 없는 고정 배열★이다 -- 최대 16 쌍과 넘침 카운터. 타일마다 지나가는 자리에
`HashSet` 을 두지 않는다.

### 4.3 텍스처 동일성 -- 재사용이 먹힐지 미리 아는 값

`SurfaceEntry` 에 진단 전용 필드를 하나 둔다.

```rust
/// 타일마다, 직전 bind 에서 BeginDraw 가 돌려준 텍스처 주소. ★절대 역참조하지 않는다★ --
/// 같은 텍스처가 다시 오는지만 세는 값이고, 그 판정이 W1 재사용의 적중률 예보다.
/// (재사용 구현은 이 주소를 믿지 않고 자기 AddRef 를 들고 비교한다. 5.2 절)
last_begin_texture: FxHashMap<(i32, i32), usize>,
```

★구현하면서 고친 것 -- 서피스 하나로 묶으면 안 된다.★ 초안은 `usize` 하나였는데, 티커 바는
타일 여럿에 걸쳐 있고 WR 은 더티 타일을 차례로 bind 한다(A, B, C, A, B, C …). 서피스 단위로
직전 값과 비교하면 타일마다 텍스처가 고정이어도 매 bind "달라졌다" 로 읽혀 ★A1 이 거짓으로
확정된다★. 타일 좌표로 키를 가른다.

`tex_reused` / `tex_changed` 를 센다. ★IPWall 에서 `tex_changed` 가 `binds` 에 가깝게 나오면
A1 이 성립하고, 동시에 "pbuffer 재사용만으로는 IPWall 이 안 고쳐진다"는 것도 같이 증명된다★ --
재사용은 같은 텍스처가 다시 와야 적중하기 때문이다. 그 경우 6 절의 양자화가 필수가 된다.

### 4.4 `DCOMPBIND` 추가분

```text
... begin_ms=7.9 begin_ms_max=0.31 pbuffer_ms=20.4 pbuffer_ms_max=0.58
    enddraw_ms=1.1 enddraw_ms_max=0.09 teardown_ms=3.2 teardown_ms_max=0.21
    update_px_max=12441600 update_sizes=47 tex_px_max=12582912 tex_sizes=12
    tex_reused=0 tex_changed=60 alloc_mb=712.4
    begin_frame_ms=1283.0 begin_frame_ms_max=23.4
    bf_deferred_ms=0.1 bf_early_ms=1279.0 bf_early_ms_max=22.9
    bf_removeall_ms=2.1 bf_removeall_ms_max=0.04
```

---

## 5. W1-A: 서피스별 pbuffer 재사용

### 5.1 구조

`SwapChain` 의 `frame_pbuffer` 를 Virtual 에 옮긴다. 두는 자리는 `SurfaceEntry` 다 --
`frame_coverage` 와 같이 "Virtual 전용 부기"로 문서화한다.

```rust
/// Virtual bind 가 만든 pbuffer 를 서피스에 매달아 둔다. ★같은 텍스처가 다시 오면
/// 그대로 쓴다★ -- 안 그러면 bind 마다 EGL pbuffer 와, 컨텍스트 설정이 DEPTH|STENCIL
/// 이므로 그만한 깊이·스텐실까지 새로 만들고 지운다. 초당 60~120 회다.
cached_pbuffer: Option<CachedPbuffer>,

struct CachedPbuffer {
    /// BeginDraw 가 돌려준 텍스처. ★이 구조체가 AddRef 하나를 들고 있다★ -- 그래야
    /// 주소가 재활용되지 않아 아래 동일성 비교가 성립한다(5.2 절).
    texture: *mut ID3D11Texture2D,
    size: (u32, u32),
    pbuffer: usize,
}
```

bind(Virtual arm)의 BeginDraw 직후:

```text
같은가 = 캐시가 있고 && 캐시.texture == 이번 texture && 캐시.size == (desc.W, desc.H)

같으면: 이번 BeginDraw 가 준 AddRef 를 즉시 Release 한다(캐시가 이미 하나 들고 있다).
        캐시의 pbuffer 를 그대로 make_current 한다.
        tex_reused += 1

다르면: release_cached_pbuffer(rc, entry)   // 옛 pbuffer destroy + 옛 texture Release
        pbuffer 를 새로 만들고 캐시에 넣는다(AddRef 는 이번 BeginDraw 것을 그대로 보유).
        tex_changed += 1, alloc_px += desc.W * desc.H
```

unbind 는 `EndDraw` 만 한다. `destroy_render_pbuffer` 와 `texture->Release()` 를 **뺀다** --
소유가 캐시로 옮겨갔다. `BoundTile.pbuffer`/`texture` 는 readback 좌표용으로 남기되 비소유로
문서화한다. `drop_bound_without_enddraw` 도 같은 이유로 해제를 뺀다(그 경로의 호출자가
`release_cached_pbuffer` 를 책임진다).

### 5.2 안전성 논증

**주장: 포인터 동일성 비교는 건전하다.**

1. 캐시가 자기 AddRef 를 들고 있다 -> 그 COM 객체는 캐시가 사는 동안 해제되지 않는다 ->
   그 주소가 다른 텍스처에 재배정될 수 없다 -> ★주소가 같으면 객체가 같다★.
2. 캐시된 pbuffer 는 ★그 텍스처를 BeginDraw 가 방금 돌려준 그 bind 안에서만★ current 가 된다.
   즉 쓰는 순간 그 텍스처는 DComp 가 지금 막 내준 유효한 것이다.
3. 크기가 달라지면 캐시를 버린다. 아틀라스 오프셋(`update_offset`)은 매 bind 새로 받아 쓰므로
   슬롯이 바뀌어도 좌표 계산은 그대로 맞는다.

**받아들이는 대가:** AddRef 를 하나 더 들고 있으므로 DComp 아틀라스 할당이 서피스마다 하나씩
오래 산다. 상한은 서피스당 하나이고, 크기 변경·타일 파기·서피스 파기에서 즉시 놓는다.

**실기로 확인해야 하는 것:** DComp 가 `EndDraw` 뒤에 그 텍스처의 **내용**을 무효화할 수 있는가.
우리는 프레임을 넘겨 내용을 읽지 않는다(WR 이 dirty 를 다시 그리고, expand 경로는 나머지를
투명으로 민다). 그래도 `clip=all`/`clip=none` 양쪽에서 육안 + `SERVO_DCOMP_READBACK` 으로 본다.

**알려진 미묘한 점(변화 아님):** 지금도 `destroy_render_pbuffer` 는 current 인 서피스를 지우고
EGL 은 파기를 유예하므로, unbind 뒤 컨텍스트의 current draw surface 는 ★이미 좀비★다. 재사용
후에는 그 자리가 살아 있는 pbuffer 가 된다. unbind 와 다음 bind 사이에 WR 이 거는 GL 은
`end_frame` 의 `gl().flush()` 뿐이고 그린 적은 없다. 즉 위험의 성격이 바뀌지는 않는다 --
그래도 리뷰 항목으로 남긴다.

### 5.3 해제 지점 (빠뜨리면 누수다)

`release_frame_pbuffer` 의 호출부를 그대로 거울삼아 `release_cached_pbuffer` 를 건다.

| 지점 | 줄 | 왜 |
|---|---|---|
| `destroy_surface` | :4442 (remove 직전) | 서피스가 사라진다 |
| `release_all` | :2890 | deinit/Drop |
| `destroy_tile` | :3017 | 그 타일의 DComp 할당이 사라진다 |
| Virtual -> SwapChain 승격 | storage 교체 지점 | Virtual 캐시가 주인을 잃는다 |
| SwapChain -> Virtual 강등 | :4239 부근 | 캐시 상태를 깨끗이 두고 시작한다 |
| 가상 서피스 리사이즈/재생성 | regen 경로 | 텍스처 전부 무효 |
| bind 에서 크기·텍스처가 달라질 때 | 5.1 절 | 본문 |

구현 과제의 검증 단계는 ★`grep -n "release_frame_pbuffer"` 가 내는 줄 전부에 대해 Virtual
대응이 필요한지 한 줄씩 판정★하는 것이다. "찾아봤다"가 아니라 그 표를 남긴다.

---

## 6. W1-B: 갱신 렉트 양자화 (pref, 기본 off)

### 6.1 왜 필요한가

4.3 절이 `tex_changed` 가 `binds` 에 가깝다고 내면 재사용은 적중률 0 이다. 텍스처가 매번
달라지는 이유는 갱신 렉트가 매번 달라지기 때문이고, 그 뿌리는 서피스 클립이 글자를 따라
움직이는 것이다. ★재사용과 고정은 택일이 아니라 짝이다★ -- 분석서 W1 이 둘을 "또는"으로 적은
것은 정정한다.

### 6.2 설계

새 pref:

```rust
/// `expand_transparent` 경로의 BeginDraw 갱신 렉트를 고정 격자로 키운다.
/// `0` = 끔(기본, 오늘 동작). `>0` = 그 픽셀 배수로 바깥으로 키운다. `-1` = 타일 전체.
pub gfx_dcomp_update_quantum_px: i64,
```

적용은 ★`expand_transparent` 가 참인 가지에서만★ 한다. 그 가지의 조건이
`!불투명 && dirty == valid` 이므로 ★그 타일에 보존해야 할 콘텐츠가 애초에 없다★ -- 전면 재도색
슬라이스다. 그래서 렉트를 키우고 그만큼 투명으로 미는 것이 안전하다. 비확장 가지는 부분
갱신이라 키우면 기존 내용을 지운다. 절대 건드리지 않는다.

```text
확장 렉트 = tile_visible_virtual_rect(...)
양자화    = 바깥으로 q 배수 정렬
결과      = 양자화 ∩ tile_rect        (타일 밖으로는 절대 나가지 않는다)
q == -1   -> tile_rect
```

`dirty ⊆ tile` 이고 밖으로만 키우므로 ★dirty 포함은 항상 유지된다★. 투명 클리어는 이미
`update_rect_v` 크기로 시저링되어 있어(:3299-3306) 자동으로 따라 커진다. `origin` 산식
(`update_offset - r_min`, :3366)도 확장 렉트를 전제로 쓰여 있어 그대로 맞는다.

### 6.3 `q = -1`(타일 전체)이 특별한 이유

그렇게 하면 갱신 렉트가 ★타일 크기로 고정★된다. 프레임마다 같다. 그러면

- DComp 가 같은 텍스처를 돌려줄 공산이 크다 -> 5 절 재사용이 적중한다
- 타일 전체 갱신 비율이 100% 가 된다 -> ★그것이 정확히 프로브와 `clip=all` 의 모습이고, 둘 다
  깨끗하다★ (분석서 2-1절: 타일 전체 갱신 100%, 값 결함 0.29 / 0.35)

대가는 EndDraw 가 커밋하는 면적과 DWM 합성량이 는다는 것이다. 프로브가 그 부하로 깨끗하므로
감당 가능한 것으로 본다. 그래도 기본값은 `0`(끔)이고, A/B 로 정한다.

### 6.4 런처

```text
-DcompUpdateQuantum <px>   -> --pref gfx_dcomp_update_quantum_px=<px>
```

`-DComp surface` 가 아니면 의미가 없으므로 그 조합에서만 허용하고 아니면 런처가 거부한다
(`-DcompParallelCommit` 과 같은 방식, `run_wall_dist.ps1:648`).

---

## 7. 범위 밖으로 두는 것과 그 이유

**깊이·스텐실 없는 pbuffer 설정.** 컨텍스트 디스크립터가 `DEPTH|STENCIL` 이라 pbuffer 마다
깊이·스텐실이 붙는다. 이것을 떼면 할당이 절반 이하가 된다. 그러나 (a) EGL 은 서피스 config 가
컨텍스트 config 와 호환이어야 하는데 ANGLE 이 ancillary 만 다른 config 를 받아줄지 확인하지
않았고, (b) WR 이 네이티브 서피스 타깃에 깊이를 쓰는지도 확인하지 않았다. 무엇보다 ★5 절과
6 절이 할당을 초당 60 회에서 서피스당 1 회로 줄이므로, 깊이 버퍼는 "반복 비용"에서 "상주
메모리"로 성격이 바뀐다★ -- 급한 문제가 아니게 된다. 후속으로 남긴다.

**`-TileSize` 축소 A/B.** 코드 변경 없이 지금 빌드로 가능하다. 계측이 들어간 뒤 먼저 돌려 보는
공짜 실험이지만 이 스펙의 구현 대상은 아니다.

**W14 / W15 / W16, 프론트 P-clip.** 다른 결함이거나 다른 레이어다.

---

## 8. 측정 계획

모두 `-DComp surface -NumaNode 1 -DcompBindProf -WrSlowMs 20`, 120 초, divisor 15.
비교 기준은 분석서 2-1절의 같은 조건 표다.

| 단계 | 실행 | 먼저 볼 것 |
|---|---|---|
| M0 | 계측만 들어간 빌드, IPWall | `br max`가 22 근처, `bf_early_ms_max`가 22 근처 -> Q1 확정. 아니면 `bf_*` 분해로 Q2 |
| M1 | 같은 빌드, 프로브 | 같은 지표가 1ms 아래여야 한다(대조군) |
| M2 | 같은 빌드, `ticker_slice_probe.html` 의 `clip=none` / `clip=all` | `tex_changed` 가 `none` 에서 크고 `all` 에서 0 에 가까운가 -> Q4 |
| M3 | 5 절 재사용만, `clip=none` | `tex_reused` 가 거의 0 일 것으로 예상한다(정직한 예측) |
| M4 | 5 절 + 6 절 `-DcompUpdateQuantum -1`, `clip=none` | `tex_reused`가 `binds`에 근접, `alloc_mb` 0 근처, `GPUMEM` 평탄, `br max` 붕괴 |
| M5 | M4 설정으로 라이브 IPWall | 분석서 9 절 수용 기준 |

★M3 이 "효과 없음"으로 나오는 것은 실패가 아니라 예측대로다.★ 그것을 먼저 적어 두지 않으면
M3 에서 작업을 접게 된다.

**반증 조건.** M4 에서 `alloc_mb` 가 0 근처이고 `tex_reused` 가 `binds` 에 근접하는데도
`br max` 가 22ms 로 남으면 A1 은 틀렸고, 정체는 할당이 아니라 DComp/DWM 프로세스 단위의 다른
무엇이다(분석서 2-1절의 "같은 초에 스케줄러 깨어남도 22ms 늦었다"가 그쪽을 가리킨다). 그 경우
이 스펙의 5·6 절은 GPU 메모리 톱니를 없앤 성과로 남기고, 저더 조사는 `-CommitAlign` A/B 로
옮긴다.

**수용 기준**은 분석서 9 절을 그대로 쓴다(값 결함 0.03/페인터·초 이하, `present_d0`
0.1/출력·초 이하, 10ms 넘는 렌더 0.05/페인터·초 이하, 16.7ms 넘는 패스가 있는 창 113 중 1 이하,
`TICKCOMMIT` 산포 3ms 이하). 여기에 둘을 더한다.

| 추가 항목 | 기준 |
|---|---|
| `tex_reused / binds` | 0.95 이상 (M4 설정) |
| `alloc_mb` | 10 /페인터·초 이하 (M4 설정) |

---

## 8-1. M0~M2 실측 (2026-10-20, `log_ani_debug_05\05`, 빌드 `60a8e2b0a98`)

네 런 모두 120 초, `-DComp surface -NumaNode 1 -DcompBindProf -WrSlowMs 20 -CommitAlign 8
-DwmAlign 14 -DcompParallelCommit -PacingThread -DirectRender`, divisor 15, 페인터 4.

| 지표 | M0 IPWall | M1 프로브 | M2 clip=none | M2 clip=all | 기준 |
|---|---|---|---|---|---|
| `br` 커밋 ≥8ms | 2,709/20,123 | 3/22,307 | 2,102/20,971 | 9/22,626 | -- |
| `br` 평균 커밋 | 2.04ms | 0.28 | 1.57 | 0.24 | -- |
| `bf_early` 합(4 페인터·123s) | 41,111ms | 6,295 | 33,032 | 5,450 | -- |
| `present_d0`(= `MISSEVENT`/출력·초) | 0.95 | 0.008 | 0.81 | **0.29** | ≤0.1 |
| `TICKCOMMIT` 산포 p50 | 15.11ms | 1.59 | 13.06 | 1.70 | ≤3 |
| 전달 간격 결함 비율(대용) | 9.50% | 0.06% | 6.89% | 0.14% | -- |
| 샘플 슬립 | 6.90% | 0.13% | 5.19% | 0.14% | -- |

전달 간격 결함은 ★대용 지표★다 -- 분석서가 "한 프레임 0칸 또는 2칸"으로 정의만 하고 어느
레코드의 어느 필드에서 읽는지는 적지 않았으므로 같은 수를 재현할 수 없다. 여기서는
`ANIMSTEPSEQ dt_ms` 중 8ms 미만(0칸)과 25ms 초과(2칸)의 비율을 썼다. 분석서 2 절의 숫자와
직접 비교하지 말 것.

### 무엇이 확정됐나

**Q1 -- 정체 위치는 확정.** M0 의 `begin_frame` 41,450ms 중 `bf_early` 가 41,111ms,
`RemoveAllVisuals` 320ms, 나머지 0 이다. ★단 "세 숫자(`begin_frame_max` / `bf_early_max` /
`COMMITSITE br max`)가 일치한다"는 것은 근거가 아니다★ -- 뒤의 둘은 설계상 같은 호출을 두 겹으로
재므로 일치가 당연하고, 깨끗한 런에서도 똑같이 일치한다(3.5 절). 확정된 것은 `begin_frame` 의
**다른 세 토막이 아니라는 것**뿐이다. 왜 그 Commit 이 느린지는 확정되지 않았다.

**정체의 모양이 바뀌었다.** 가끔 터지는 20~40ms 가 아니라 ★만성 세금★이다. 20ms 넘는 커밋이
하나라도 있는 창은 M0 122 중 38 뿐이고, 비용의 본체는 20,123 번 × 평균 2.04ms 다.

### Q4 -- A1 은 반증됐다

- 가장 깨끗한 M1 이 할당을 가장 많이 한다(2,858MB/s/페인터, 총 1.4TB). M0 은 1,019MB/s 인데
  느린 커밋이 900 배 많다.
- `pbuffer_ms` 총합은 M0 7.4s / M1 6.1s 다. 정체 예산은 `bf_early` 41.1s 대 6.3s 다.
  ★pbuffer 시간을 전부 0 으로 만들어도 35.1s 격차 중 33.7s 가 남는다.★
- GPU 메모리 6 초 톱니도 우리 할당으로 설명되지 않는다(M1 이 제일 많이 할당하고 제일 평탄,
  121 스텝 중 110 이 1MB 미만 변동).

### 슬라이스·비주얼 수도 반증됐다

`clip=none` 과 `clip=all` 의 대비는 한 번에 여섯 가지를 바꾸므로(서피스 4.5→2.0,
비주얼/프레임 1.9→1.0, binds/s 53→39, 전체 갱신 0%→100%, `update_sizes` 16+→1,
재사용 23%→0%) 어느 것이 원인인지 가르지 못한다. 같은 런 **안에서** bind 40 개 이상인 창을
비용별로 묶으면 답이 다르게 나온다.

| M2 clip=none | binds | vis/frame | surfaces | **tex 재사용** | tex 크기수 | **BeginDraw ms/bind** | `bf_early` |
|---|---|---|---|---|---|---|---|
| 싼 창 (n=75) | **131.0** | **3.22** | **6.8** | **36%** | 8.2 | **0.039** | 16.1 ms/s |
| 중간 (n=67) | 68.0 | 2.16 | 4.1 | 31% | 6.9 | 0.042 | 38.7 |
| 비싼 창 (n=170) | 59.8 | 2.03 | 4.0 | **4%** | 15.6 | **0.175** | **165.5** |

★가장 싼 창이 bind 도 비주얼도 서피스도 더 많다.★ M0 도 같은 모양이다(재사용 45→26→4%,
BeginDraw 0.022→0.048→0.169 ms/bind). 두 런에서 단조로 남는 것은 ★텍스처 재사용률★과
★bind 당 BeginDraw 시간★ 둘뿐이다.

읽기: 비싼 것은 우리 pbuffer 가 아니라 DComp 가 새 텍스처를 realize 하는 쪽으로 보인다.
그 신호가 `tex_reused` 이고, 그것이 5 절 W1-A 가 겨냥한 바로 그 양이다. 다만 ★W1-A 는
재사용을 이용할 뿐 만들어내지 못한다★ -- 만드는 쪽은 6 절이다.

### 계측 자체의 결함 둘

1. ★`update_sizes` 가 상한 16 에 포화된다★ -- 더러운 두 런의 **모든** 창에서 16+ 다. 포화된
   지표는 아무것도 가르지 못한다. 상한을 올려야 한다(T5b-1).
2. ★Commit 시간이 호출 지점에만 귀속되고 그 Commit 이 무엇을 실어 날랐는지에는 귀속되지
   않는다★ -- 그래서 위 표가 전부 초 단위 상관이고 인과가 아니다. 커밋 한 번이 실어 나른
   새 텍스처 수에 비용을 붙여야 한다(T5b-2).

### 수용 기준에 대한 나쁜 소식

★`clip=all` 은 느린 커밋을 233 배 줄이고도 `present_d0` 는 2.8 배밖에 못 줄이며, 여전히 기준의
3 배다(0.29 대 0.1).★ 같은 모양인 M1 은 0.008 이다 -- 70 배 차이가 설명되지 않는다. 즉 ★커밋을
완벽히 고쳐도 수용 기준에 닿지 않는다★. 두 번째 결함이 따로 있고, 이 스펙은 그것을 다루지
않는다.

### 기타

- `clip=all` 의 60.72ms 단발은 **창 1(기동)** 이다. 정상 구간 최대는 12.20ms. `max` 만 봤으면
  `clip=all` 을 최악으로 읽었을 것이다 -- 3.3 절이 `over` 를 넣은 이유 그대로다.
- 네 런 모두 `ef`/`df`/`di`/`ex` 가 **n=0** 이다. 커밋은 전부 `br` 68% + `sc` 32% 로 나간다.
  1.1 절 표의 여섯 경로 중 실제로 도는 것은 둘뿐이다.
- `br` 이 느린 창은 `sc` 도 느리다(M0 115 창 중 112). 다만 M0 은 122 창 중 112 가 그 칸이라
  기저율이 높아 변별력이 약하다. `clip=none` 쪽(105 중 94, 깨끗한 창 15 개 대조)이 더 낫다.

---

## 8-2. M2b 실측 (2026-10-20, `log_ani_debug_05\06`, 빌드 `cb6c1cefa0c`)

`COMMITWORK`(커밋 단위 귀속)를 달고 `clip=none` / `clip=all` / IPWall 을 같은 플래그로 다시
받았다. ★Q4' 판정: 갈린다.★

### 실어 나른 일을 고정해도 10 배

`new1` 버킷(bind 하나, 새 텍스처 하나, 재사용 0)만 비교한다.

| | 평균 커밋 | vis/cmt | tex 크기 가짓수 |
|---|---|---|---|
| IPWall | **3.814 ms** | 2.00 | 48 |
| `clip=none` | **3.730 ms** | 2.09 | 64(포화) |
| `clip=all` | **0.388 ms** | 1.00 | 1 |

★로컬 프로브가 라이브 IPWall 을 정량적으로 재현한다(3.73 대 3.81 ms).★ 이제 조사에 라이브
페이지가 필요 없다.

### 비용은 페이로드가 아니라 초 단위 국면이다

`clip=none` 에서 `new2`/`new3_4`(bind 3, visual 4 -- 가장 많이 실어 나름)가 0.39~0.57ms 로
가장 싸고 `new1`(bind 1)이 3.73ms 다. 페이로드로는 설명되지 않는다. 창을 비용으로 가르면
이렇게 된다(페인터 4 합산, 프레임 수는 240~242 로 동일).

| 창 | 커밋 | binds | visuals | **tex 크기 가짓수** | 재사용 | alloc |
|---|---|---|---|---|---|---|
| 싼 초 (42) | 86.5 ms/s | **256.8** | **508.3** | **7.4** | 34.9% | **7,283 MB** |
| 중간 (17) | 323.8 | 175.2 | 419.7 | 11.7 | 17.6% | 4,947 |
| 비싼 초 (63) | **680.5** | 185.6 | 437.6 | **22.4** | 16.7% | 5,593 |

싼 초가 bind 도 visual 도 할당도 **더 많다**. 단조로 오르는 것은 ★그 초에 DComp 가 내준
서로 다른 텍스처 크기의 가짓수★ 하나뿐이고, 전체가 한 줄로 정렬된다 -- `clip=all` 1 가지에
0.388ms/커밋, 싼 초 7.4 에 86.5ms/s, 중간 11.7 에 323.8, 비싼 초 22.4 에 680.5, IPWall 48 에
324.9.

(T5b-1 에서 상한을 64 로 올린 것이 여기서 값을 했다. 16 이었으면 세 줄 다 `16` 이다.)

### 계측 자체의 결함 둘 (고침, `d2c4213d42d`)

1. ★판정을 `reuse` 대 **마지막** 비지 않은 버킷으로 냈다.★ `clip=none` 에서 마지막은
   `new5p` 이고 커밋이 하나였다 -- 표가 "flat 1.8x, 어느 수정도 안 먹는다" 를 찍었는데
   실제 질량은 `new1` 12,919 커밋 × 3.730ms, 기준선 0.710ms 로 **5.3 배**다. 표본 하나가
   뒤집을 수 있는 것은 판정이 아니다. 이제 모든 `new*` 를 커밋 수로 가중한다.
2. 재사용률 분모가 `bind+same` 이었다. `bind` 가 이미 재사용분을 포함한다 -- "모든 텍스처가
   돌아왔다" 가 정의인 버킷이 50% 로 찍혀 자기 행 이름과 모순됐다.

---

## 8-3. T8a 실측 (2026-10-20, `log_ani_debug_05\07`, 빌드 `36212c20ee7`)

`clip=none` 에 `-DcompUpdateQuantum -1` 과 `256` 을 걸었다. 비교 대상은 8-2 절의 같은 페이지
`q=0` 과 `clip=all` 이다. ★판정: 예측대로 갈렸다.★

| | `q=0` | `q=256` | **`q=-1`** | `clip=all` | 기준 |
|---|---|---|---|---|---|
| update 크기 가짓수 | 64(포화) | 23 | **1** | 1 | -- |
| tex 크기 가짓수 | 64 | 21 | **1** | 1 | -- |
| 타일 전체 갱신 | 4/25,461 | 4/25,837 | **전부** | 전부 | -- |
| `COMMITWORK new1` 평균 | 3.730ms | 3.052 | **0.391** | 0.388 | -- |
| `COMMITSITE br over` | 2,082 | 1,559 | **76** | 12 | -- |
| `br` 부하 | 268.2 ms/s | 230.0 | **45.2** | 45.1 | -- |
| `begin_frame` | 67.9 ms/s | 58.0 | **11.9** | 11.5 | -- |
| 타일 왕복 | 27.0 ms/s | 28.2 | **13.8** | 9.6 | -- |
| ★비주얼/프레임★ | 1.9 | 1.9 | **1.9** | 1.0 | -- |
| ★살아 있는 서피스★ | 4.5 | 4.5 | **4.6** | 2.0 | -- |
| `present_d0` | 1.085 | 1.290 | **0.260** | 0.369 | ≤0.1 |
| `TICKCOMMIT` 산포 p50 | 14.31ms | 13.20 | **1.67** | 1.69 | ≤3 |
| 샘플 슬립 | 6.00% | 6.39% | **0.47%** | 0.16% | -- |
| 전달 간격 결함(대용) | 8.01% | 9.34% | **0.58%** | 0.10% | -- |

### 결정적인 줄은 비주얼/서피스다

`q=-1` 에서 ★비주얼/프레임 1.9, 서피스 4.6 -- 두 번째 슬라이스가 그대로 있다★. 그런데 나머지
전부가 `clip=all` 수준으로 내려앉았다(`new1` 0.391 대 0.388, `br` 45.2 대 45.1). 즉
**슬라이스 수는 무죄이고 원인은 텍스처 크기의 가짓수다.** `clip=all` 이 한꺼번에 바꾸던
여섯 가지 중 하나만 분리한 arm 이 전부를 가져갔다.

운영자 육안 판정도 같다: "`-1` 인가한 `clip=none` 및 output 페이지에서 `clip=all` 수준의
개선 확인됨. `256` 은 기존 실행과 육안 개선점 없음."

### ★`q > 0` 은 쓸모가 없다 -- 설계상 그렇다★

`q=256` 이 크기를 23 가지까지밖에 못 줄인다. 양자화 함수는 **주어진 폭에 대해** 크기를
고정하는데, 클립된 띠의 폭 자체가 글자에 따라 변하기 때문이다. 격자가 그 변동을 그대로
물려받는다. ★타일 전체(`-1`)만이 내용과 무관하게 하나다.★ pref 는 `>0` 을 계속 받지만
운용값은 `-1` 뿐이고, `>0` 은 이 사실의 증거로만 남는다.

### T6(pbuffer 재사용)은 반증됐다 -- 취소한다

`q=-1` 에서 크기가 정확히 한 가지인데도 ★`same texture again 0/26,767`★ 이다. DComp 는 같은
크기를 요청해도 같은 텍스처 포인터를 돌려주지 않는다. 5 절의 `CachedPbuffer` 는 "포인터와
크기가 모두 같을 때만 재사용" 이므로 ★적중률이 구조적으로 0★ 이다. 게다가 `q=-1` 에서
pbuffer 시간이 17.8 -> 11.0 ms/s 로 이미 줄었다(바이트는 1,508 -> 2,600 MB/s 로 늘었는데도).
남은 11 ms/s 는 페인터 시간의 1.3% 라 쫓을 값이 아니다.

### 남는 것

`present_d0` 가 1.085 -> **0.260** 으로 4 배 좋아졌지만 기준 0.1 에는 못 닿는다
(`clip=all` 0.369 보다는 낫다). ★8-1 절에서 예고한 두 번째 결함이 그대로 남아 있다★ --
같은 모양의 `wall_anim_jitter_probe` 가 0.008 이므로 30 배 격차다. 이 스펙 밖이다.

---

## 9. 작업 분할

TDD 가능한 단위로 가른다. COM 을 부르는 자리는 단위 테스트가 안 되므로 ★판정 규칙만 순수
함수로 떼어내 거기에 테스트를 건다★.

| # | 내용 | 파일 | 테스트 |
|---|---|---|---|
| T1 | `CommitSite` + `raw_commit` 인자화 + 전역 누적기 + `SERVO_DCOMP_SLOW_COMMIT_MS` | `dcomp_compositor.rs`, `commit_scheduler.rs`, `debug_env.rs` | 누적·비우기 로직 단위 테스트 |
| T2 | 방출자 선출 + `COMMITSITE` 한 줄 | `dcomp_compositor.rs` | 선출 함수 단위 테스트(둘이 다투면 하나만 이긴다) |
| T3 | `begin_frame` 4 분할 계측 | `dcomp_compositor.rs` | 계측이라 없음. `bf_total >= 합` 을 debug_assert 로 |
| T4 | BeginDraw `_max` + `SizeSet` + `tex_reused/changed` + `alloc_mb` | `dcomp_compositor.rs` | `SizeSet` 순수 단위 테스트(중복·넘침) |
| T4b | 런처 요약 복구 (T1~T4 구현 중 추가) | `run_wall_dist.ps1` | 합성 로그로 블록 실행 |
| T5 | **M0~M2 실행.** Q1·Q4 판정 | -- | 로그 (완료, 8-1 절) |
| T5b-1 | `SizeSet` 상한 16 -> 64 (포화 해소) | `dcomp_compositor.rs` | 상한 포화·넘침 단위 테스트 |
| T5b-2 | 커밋 한 번이 실어 나른 일에 비용 귀속 (`COMMITWORK`) | `dcomp_compositor.rs`, `run_wall_dist.ps1` | 버킷 선택·슬롯 확보 순수 함수 |
| T5c | **M2b 실행.** `clip=none` 재측정으로 Q4' 판정 | -- | 로그 (완료, 8-2 절) |
| ~~T6~~ | ~~`CachedPbuffer` + 재사용 판정~~ **취소** -- 8-3 절, 크기가 한 가지여도 같은 텍스처가 안 온다(0/26,767) | -- | -- |
| T7 | `gfx_dcomp_update_quantum_px` + 양자화 함수 + 런처 플래그 | `prefs.rs`, `dcomp_compositor.rs`, `run_wall_dist.ps1` | 양자화 순수 함수: dirty 포함 유지, 타일 밖으로 안 나감, q=-1 이면 타일, q=0 이면 불변 |
| T8a | **T7 arm 실행.** `clip=none -DcompUpdateQuantum -1` | -- | 로그 (완료, 8-3 절) |
| T8 | **라이브 IPWall 에 `-1` 로그 실행.** 기본값 전환 판단 | -- | 로그 + 육안 |

T5 가 ★게이트★였다. **판정은 "아니오"다**(8-1 절) -- A1 은 반증됐고, 슬라이스·비주얼 수도
반증됐다. 그래서 T6~T8 을 ★지금은 시작하지 않는다★. 남은 유일한 단조 신호가 텍스처 재사용률인데,
그것을 인과로 올리려면 초 단위 상관이 아니라 커밋 단위 귀속이 필요하다. 새 게이트는 T5c 다.

**새 판정식(Q4') -- 판정 완료, 갈렸다(8-2 절).** `reuse` 0.710ms 대 `new*` 가중 3.20ms 로
4.5 배이고, 실어 나른 일을 완전히 고정한 `new1` 버킷끼리도 0.388(`clip=all`) 대 3.73~3.81
(`clip=none`/IPWall)로 10 배다. 비용은 ★그 초에 DComp 가 내준 텍스처 크기의 가짓수★를
따라간다. 그래서 **T7 -> T6** 순서로 간다 -- 재사용(5 절)은 양자화(6 절)가 크기를 고정해야
적중할 수 있으므로 양자화가 먼저다.

**T7 의 판정식(T8a).** `clip=none` + `-DcompUpdateQuantum -1`:

| 보는 것 | 성공 | 실패 |
|---|---|---|
| `tex_sizes` | 1 | 여전히 수십 |
| `COMMITWORK new1 avg` | 0.4ms 부근 | 3.7ms 유지 |
| `COMMITSITE br over` | 2,082 -> 수십 | 그대로 |

★실패면 "크기 가짓수" 도 승객이고 5·6 절을 둘 다 접는다.★ 슬라이스 수가 아니라 크기
가짓수가 원인이라는 주장을 이 한 arm 이 통째로 걸고 있다 -- `-1` 은 슬라이스는 둘로 둔 채
크기만 하나로 만들기 때문이다.

**이 스펙이 다루지 않는 두 번째 결함.** `clip=all` 은 느린 커밋을 233 배 줄이고도 `present_d0`
0.29(기준 0.1)에 머문다. 같은 모양의 M1 은 0.008 이다. 커밋을 완벽히 고쳐도 수용 기준에 닿지
않는다는 뜻이고, 그 70 배는 별도 조사가 필요하다.

**T4b 가 왜 생겼나.** `run_wall_dist.ps1` 의 `DCOMPBIND` 요약은 ★자리 기반 정규식 하나★로
40 개 필드를 받고 있었는데, 그것이 이미 깨져 있었다 -- 언젠가 `commit_ms_max` 가 중간에
끼면서 매칭이 멈췄고, 그러면 표가 통째로 사라지는 데서 끝나지 않고 아래 `elseif` 가
"no DCOMPBIND line was logged"(= 네이티브 컴포지터가 안 떴다)를 ★거짓으로★ 경고한다.
M0 를 그 상태로 돌리면 읽는 사람이 먼저 엉뚱한 곳을 파게 된다. `key=value` 파싱으로 바꿔
다시 썩지 않게 하고, `begin_frame` 분해·갱신 렉트·`COMMITSITE` 를 같은 자리에 붙였다.
같은 이유로 "DComp 가 병목인가" 판정식에 `begin_frame` 을 더했다 -- 그것이 빠져 있어서
end_frame 1.7ms·begin_frame 21ms 인 실행을 "병목 아님" 으로 읽고 있었다.

---

## 10. 미확인으로 남는 것

- DComp 가 `EndDraw` 뒤 텍스처 내용을 보존하는지 (문서화되지 않음. M3 의 육안·readback 으로만 판정)
- ANGLE 이 pbuffer 마다 깊이·스텐실을 실제로 만드는지 (추정. `alloc_mb` 와 `GPUMEM` 의 비율로 간접 확인)
- 양자화가 DWM 합성 비용을 얼마나 올리는지 (프로브가 그 부하로 깨끗하다는 것이 유일한 근거)
- `q=-1` 에서 서피스당 상주 메모리 (타일 5760x2160 기준 RGBA 50MB + 깊이·스텐실 50MB 추정)

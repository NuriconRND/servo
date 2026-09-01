# WebGL 캔버스를 컴포지터 서피스로 — 설계안

작성 2026-09-01. 브랜치 `multigpu-wall-pacing`. 근거 로그 `log_webgpu/29~34`.

## 1. 고쳐야 하는 것

DComp Native 를 켠 채로 WebGL 이 제 속도를 내야 한다. 끄는 것은 답이 아니다 — WebGL 콘텐츠에서
GPU 점유율이 **1.5~2 배**가 되고, 어떤 응용이 뜰지는 서비스 제공자가 고를 수 있는 것이 아니다.
혼재가 전제다.

## 2. 원인 (실측으로 좁힌 한 문장)

> **타일을 크로스-디바이스 텍스처로부터 다시 그리면 그 서피스의 `Commit` 이 막힌다.**

| 실행 | 타일 재도색 | 샘플링 대상 | Commit | fps |
|---|---|---|---|---|
| 정적 캔버스(`webgl2_ctx_probe`) | **없음**(binds 0/s) | — | 0.007 ms | **63.6** |
| 비디오 그리드(escape off) | 있음(54/s, 93% full-tile) | **같은 디바이스** EGLImage | 0.44 ms | 61.5 |
| 애니메이션 캔버스(삼각형 1 개) | 있음 | **크로스-디바이스** keyed mutex | **37.4 ms** | **6.7** |

배제된 것(전부 실측): 그리는 양(비디오가 훨씬 많은데 29 배 싸다) · 비주얼 트리 크기(캔버스는
비주얼 1 개인데 비주얼당 3,400 배) · 페이지 복잡도(삼각형 하나로 재현) · 캔버스 lock 빈도(정적
캔버스가 60/s 로 더 많은데 공짜) · external-image 콜백(초당 몇 ms).

디바이스가 실제로 갈린다는 것은 로그가 직접 말한다 — WALLDIAG 의 비디오 링 디바이스 포인터가
painter 의 ANGLE 디바이스와 **완전히 동일**하고, WebGL 격리 디바이스만 다른 포인터다
(`log_webgpu/25`, `33` 양쪽에서 확인).

WebGL 이 격리된 이유는 `35b4c2ed799` — 컴포지터와 ANGLE 렌더러를 공유하면 두 스레드가 한
렌더러를 구동해 `libGLESv2.dll` 에서 0xc0000005 가 났다. ★그 수정이 지금의 비용을 만들었고,
되돌리는 것은 선택지가 아니다.★

## 3. 방향

비디오 escape 와 **같은 기전**을 캔버스에 적용한다: 캔버스를 WR 콘텐츠 패스에서 빼내
컴포지터 서피스로 승격하면, painter 가 크로스-디바이스 텍스처를 DComp 서피스 안으로 샘플링하는
일 자체가 없어진다.

근거가 셋이다. ① 그 경로는 이미 실증됐다(비디오, 60fps 유지) ② **GPU 도 8%p 아낀다**
(22~35% → 14~27%, `log_webgpu/32`) ③ 격리 디바이스를 그대로 두므로 6월의 AV 위험이 없다.

## 4. ★착수 전에 반드시 답해야 하는 하나★

**크로스-디바이스 건너기를 타일 드로우 밖으로 옮기기만 해도 Commit 스톨이 사라지는가?**

이것이 아니면 B 전체가 무의미하다. 비디오가 싼 이유가 "컴포지터 서피스라서"가 아니라 **"애초에
디바이스를 건너지 않아서"** 일 수 있기 때문이다. 캔버스는 컴포지터 서피스로 올려도 여전히
격리 디바이스에서 와야 하므로, 건너기는 위치만 바뀔 뿐 사라지지 않는다.

### 4.1 그것을 가르는 값싼 스파이크 (B 착수 전에 이것부터)

B 를 짓지 않고 같은 질문에 답할 수 있다. `lock_swap_chain`
(`components/paint/webrender_external_images.rs:48`)이 WR 에게 **크로스-디바이스 텍스처를 그대로**
넘기는 대신, **painter 디바이스의 스테이징 텍스처로 한 번 복사해서** 그것을 넘기게 한다.

- Commit 이 떨어지면 → 건너기가 **타일 드로우 안에 있는 것**이 문제였다는 뜻이고, B 가 성립한다.
  게다가 이 스파이크 자체가 쓸 만한 중간 수정이 된다(복사 1 회 대 37ms).
- Commit 이 그대로면 → 위치를 옮겨도 소용없다는 뜻이고, **B 는 짓지 않는다.** 주 단위를 아낀다.

가드: pref 게이트 + 육안 확인(조건부 flush 때와 같은 방식). 실패 모드가 시각적이다.

## 5. B 설계 (스파이크가 통과했을 때)

### 5.1 비디오 경로의 구조 (그대로 따라간다)

1. `layout/display_list/mod.rs:836` — `fragment.yuv_image` 에 2D 히스테리시스를 걸고
   `PREFER_COMPOSITOR_SURFACE | SUPPORTS_EXTERNAL_COMPOSITOR_SURFACE` 부여
2. WR 이 `create_external_surface(id, is_opaque)` 호출 → 콘텐츠 없는 DComp 비주얼 생성
   (`dcomp_compositor.rs:3520`대)
3. WR 이 `add_surface(...)` 호출 → `SurfaceStorage::External` 이면 `add_external_surface` 로 분기
   → provider 링 대여 → `VideoConvertPass` 로 raw D3D11 1-draw → 비디오별 flip 스왑체인 백버퍼를
   채우고 `Present` → 첫 성공 후 `visual.SetContent(swapchain)`

### 5.2 캔버스에 필요한 것

| 단계 | 비디오 | 캔버스 |
|---|---|---|
| 승격 힌트 | `fragment.yuv_image` (`:836`) | `fragment.image_key` 의 `push_image`(`:901`)에 **WebGL 캔버스일 때만** 같은 플래그 |
| 저장소 | `ExternalStorage`(flip 스왑체인 + RTV 캐시 + 세대 dedup) | 대부분 재사용 가능. **다른 점은 소스** — 링이 아니라 WebGL 스왑체인의 프런트 버퍼 |
| 채우기 | `VideoConvertPass`(YUV→RGB 1-draw) | 변환 불필요. **복사 1 회**(포맷이 이미 RGBA) |
| 수명 | 링의 demand TTL | 캔버스 컨텍스트 수명 + `busy_webgl_context_map` 과의 정합 |

### 5.3 단계 (각각 되돌릴 수 있는 지점)

1. **디스플레이 리스트 플래그** — 캔버스에만, pref 게이트. WR 이 실제로
   `create_external_surface` 를 부르는지만 확인(로그). 여기서 WR 이 폴백하면 그 이유를 먼저 푼다.
2. **`ExternalStorage` 를 소스에 대해 일반화** — 링 대여 부분을 소스 추상화 뒤로.
3. **캔버스 소스 구현** — 프런트 버퍼 → 스왑체인 백버퍼 복사 + Present + `SetContent`.
4. **수명·동기화** — `take_surface`/`recycle_surface` 와 `busy_webgl_context_map` 정합, keyed mutex
   획득 위치.
5. **정적 캔버스 최적화** — 내용이 안 바뀌면 Present 를 건너뛴다(정적 캔버스가 이미 63.6fps 인
   이유를 유지).

### 5.4 유지해야 하는 것

- 격리 D3D11 디바이스(6월 AV 수정) — 건드리지 않는다
- 디바이스당 ANGLE 락(`5cc95bd09ee`)
- 조건부 `end_frame` flush(`ff22ec2899a`) — external 이 생기면 조건이 참이 되어 자동으로 다시 걸린다
- `--capture`, 단일 타일 모드, servoshell 무영향

## 6. 미해결

1. **WR 의 컴포지터 서피스 제약이 캔버스에 어디까지 허용되는가** — 블렌딩·변환·클립·z-순서·
   `opacity`. WR 은 조건이 안 맞으면 조용히 콘텐츠 패스로 폴백하므로, **폴백했는지 로그로
   보이게** 만들어야 한다(안 그러면 "적용했는데 아무 효과 없음"으로 시간을 버린다).
2. **`is_opaque`** — 캔버스는 `alpha:true` 가 기본이라 비불투명이 흔하다. 비디오는 대개
   불투명이었다. 비불투명 external 이 DComp 에서 어떻게 합성되는지 확인 필요.
3. **복사 비용** — 4K 캔버스 RGBA 복사가 프레임당 얼마인가. 37ms 보다 싸야 의미가 있다.
   §4.1 스파이크가 이 값을 함께 준다.
4. **여러 캔버스** — 페이지에 캔버스가 N 개면 external 서피스도 N 개. 비디오는 45 개까지 돌려본
   전례가 있으나 캔버스는 미검증.

## 7. 하지 않을 것

- **격리 디바이스 되돌리기** — 6월 AV 재발 위험 + 디바이스당 락으로 WebGL 이 painter 와 다시
  직렬화(방금 46.7%→0.0% 로 없앤 경합).
- **타일 루프 병렬화** — 실측으로 배제됐다. Commit 이 4 회 직렬이라 되살아나는 듯 보였으나,
  DComp 를 맞는 곳에 쓰면 어느 구성도 Commit 에 묶이지 않는다.
  `docs/multigpu/parallel_tile_render_design.md` 참고(보류 상태, 해소된 질문 2 건 포함).
- **keyed mutex 제거** — surfman 에 env 를 넣으려면 config surface 규약상
  `third_party/surfman` 이 `servo-config` 에 의존해야 한다. 시험 하나를 위해 낼 대가가 아니고,
  §4.1 스파이크가 같은 질문에 더 안전하게 답한다.

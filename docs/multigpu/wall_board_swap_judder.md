# 보드 교체 전환 judder -- 원인과 수정 (브랜치 `wall-rotate-opacity-judder`)

작성: 2026-11-17
마지막 커밋: `1d777e05984` (noclear 기본값)
상태: **해결, 기본값 적용.** WebGL 간 전환에 남은 judder 는 별건으로 열려 있다(6절).
로그: `wall_dist_rtprobe/log_rotation_opacity_judder_01/12` ~ `16`

---

## 1. 증상

IPWall 출력 페이지가 벽 전체 보드를 교체하는 전환(bounce/left 2500 ms, 이미지와 비디오를 번갈아)에서
끊김이 보였다. 끊김은 전환의 시작과 끝 무렵에 몰려 있었고, 정지 구간은 멀쩡했다.

- 2x2 벽에서만 났다(타일 5760x2160, AMD RX 580 네 장, `-DComp surface`).
- 같은 페이지·같은 플래그의 단일 타일 대조군(런 10)에서는 나지 않았다.
- 운영자 기준으로 "60fps 로 보이는 전환이 거의 없다" 수준이었다.

---

## 2. 결론

### 원인 사슬

1. 보드 가장자리가 타일을 지나는 동안, 그 타일의 **불투명** picture cache 서피스의 유효 영역(타일 안의 보드
   범위)이 매 프레임 바뀐다.
2. WR 은 유효 영역이 바뀌면 타일 전체를 무효화한다(`ValidRectChanged`, picture.rs 약 916 행).
   - 그 뒤 dirty 를 유효 영역과 교차한다(약 6224 행).
   - 그래서 dirty == valid 이고, BeginDraw 갱신 영역의 크기가 매 프레임 바뀐다(폭 3840/3872/4544/5760 등).
3. DComp 는 같은 크기의 BeginDraw 텍스처를 작은 풀에서 돌려 쓰는 것으로 보인다. 크기가 일정하면 텍스처 포인터가
   최근 8 개 안에서 99% 다시 나온다. 풀 자체는 추정이다(DComp 소스가 없다).
4. 크기가 바뀌면 재사용이 깨지고 BeginDraw 가 느린 경로를 탄다(약 190 us, 평소 8~20 us).
   - 그 프레임의 첫 GPU 제출이 약 10 ms 막힌다. 첫 제출은 in-frame Commit 이거나, 그 앞에 넣은 flush·펜스 Signal 이다.
   - 그래서 렌더가 주기(16.7 ms)를 넘긴다.
5. ticker 때 넣은 고정(T7/T8, `gfx_dcomp_update_quantum_px = -1`)은 **투명** 서피스 분기
   (`!opaque && dirty == valid`)에만 걸렸다. 그래서 불투명인 보드 서피스는 빠져 있었다.

### 수정

pref `gfx_dcomp_opaque_full_tile`, 런처 `-DcompOpaqueFullTile off|clear|noclear`, 기본 `noclear`(2).

- **위치:** `components/paint/dcomp_compositor.rs` 의 bind, Virtual 가지.
- **조건:** 다음이 모두 맞을 때만 걸린다.
  - 서피스가 불투명이다.
  - dirty == valid 이다.
  - 서피스의 타일이 하나다(런처 기본 `tile_size=display` 에서 성립).
  - 직전 프레임의 표시 클립이 유효 영역 안에 있다.
- **동작:** 유효 영역을 `quantize_update_rect` 에 넣어 타일 전체로 고정한다. ticker 고정과 같은 함수, 같은 quantum -1 이다.
- **noclear:** 갱신 영역 밖을 지우지 않는다. 유효 영역 밖은 합성되지 않으므로 보존할 내용이 없다.
- **clear:** 지우기를 한다. 효과는 같지만 GPU 비용이 크다(4절).

### 효과 (런 16)

- 전환당 늦은 렌더: 27.5 → 0.07(noclear) / 0.21(clear)
- 운영자: 레버를 켠 네 런 모두 "모든 전환 애니메이션에서 육안 judder 없음"
- 운영자 추가 확인(같은 날):
  - noclear 에서 눈에 띄는 잔상이 없었다.
  - 정지 비디오 두 경우 모두 이상이 없었다.

---

## 3. 경과

| 런 | 커밋 | 바꾼 것 | 판정 |
|---|---|---|---|
| 12 | `715d72e6292` `c41817b14af` `32636b9f7c9` `fb2862abd55` `ca88108f57a` `a793155faab` | `-DetachTileRender` 를 실제로 분리, 디바이스별 Commit 관문, WALLSPAN 계측 | 조작은 의도대로 됐다. 육안·수치 차이는 없었다. 대기를 옮겼을 뿐이다. 타일 간 결합(join)은 원인도 증폭기도 아니다 |
| 13 | `724c43a25d4` | GPU 시간·완료 프로브(WALLGPU*, `-GpuTimeProbe` / `-GpuFenceProbe`) | 긴 in-frame Commit 은 그 프레임의 첫 GPU 제출이다. flush 나 Signal 을 앞에 넣으면 막힘이 그리로 옮겨 가지만, 늦은 렌더와 육안은 같았다. 우리 GPU 일은 전환 프레임당 1.2~2.9 ms 로 작다 |
| 14 | `d0c1f69e338` | 늦은 프레임 행, Signal 순간의 밀린 프레임, bind 단계, flush, VRAM | 막힌 Signal 의 95.7% 는 밀린 프레임이 0 일 때였다(우리 큐 한도가 아니다). 막힘의 97~99% 가 느린 BeginDraw 와 함께였다. VRAM 압박은 없다. bind 마다의 pbuffer·깊이 버퍼 할당만으로는 원인이 아니다(정지 비디오는 막히지 않는다) |
| 15 | `775284328ee` | `-DcompFullTileUpdates`(WR 에 max_update_rects = 0) | 효과 없음. 구조상 A/A 였다. dirty 는 원래 valid 와 같았고, 매 프레임 바뀌는 것은 불투명 서피스의 유효 영역이었다. 조용한 전환이 매 분 :19~:22 초에 몰렸다(출처 미상) |
| 16 | `4284116d4c7` | `-DcompOpaqueFullTile` 과 bind 별 계측(WALLBINDINFO, 늦은 프레임 `upd=`) | 해결(4절) |
| -- | `1d777e05984` | noclear 를 엔진과 런처의 기본값으로 바꿨다. 회귀 시험 `the_opaque_full_tile_lever_defaults_to_noclear` | 운영자 확인 뒤 적용 |

---

## 4. 런 16 상세

- 빌드: `4284116d4c7`
- 갈래: O0 off, O1 noclear, O2 clear. 교대 2+2+2, 각 120 초.
- 공통 플래그: `-DcompCommitInFrame -SpanProbe -EventProbe -MeshProbe -NoFrameBarrier -GpuTimeProbe -GpuFenceProbe`

### 전환당 수치 (T1~T7, 갈래당 14 전환, 부트 전환 제외)

| | off | noclear | clear |
|---|---|---|---|
| 늦은 렌더(16.7 ms 초과, `render.over`) | 27.5 | 0.07 | 0.21 |
| 늦은 패스 | 17.0 | 0.07 | 0.21 |
| 막힌 제출(4 ms 이상) | 151 | 2.9 | 3.3 |
| 막힌 제출(12 ms 이상) | 46.4 | 0.14 | 0.29 |

- **비율의 95% CI:** noclear/off [0, 0.010], clear/off [0, 0.022].
- **런 단위 비교:** 레버를 켠 런은 모든 지표에서 off 런보다 아래였다. 같은 플래그의 off 런 12 개(런 13~16)와 견주면 p = 1/1820 이다.
- **매 분 :16~:23 초 대역 밖:** 움직임 초당 늦은 렌더가 11.5 → 0.03 / 0 이었다. 대역 안은 off 도 이미 조용해서 판정할 수 없다.
- **O0 의 대표성:** O0 는 런 13~15 의 기준 갈래와 같은 수준이었다(대역 밖 어떤 지표도 p < 0.14 아님).

### 기전 (bind 별 계측)

- **off:** bind 17,808 개 중 4,745 개가 크기가 바뀌었다.
  - 크기가 바뀐 bind 의 BeginDraw 는 90.1% 가 느렸다(120 us 초과). 크기가 같으면 4.4% 였다.
- **막힌 제출(4 ms 이상) 2,650 행:**
  - 88.0% 가 그 렌더 안에 크기 바뀐 bind 가 있었다.
  - 98.3% 가 느린 BeginDraw 가 있었다.
  - 95.6% 는 GPU 에 밀린 이전 프레임이 없을 때였다.
- **noclear/clear:** 불투명 bind 29,372 개가 모두 5760x2160 하나로 고정됐다(`size_changed=0`).
  - 느린 BeginDraw 는 1.95% / 2.19% 였다.
  - 막힌 제출은 2,692 → 61 / 74 였다.
- **용량 반응:** 왼쪽 열은 오른쪽보다 크기 변경이 약 1.8 배 많았다. 막힌 행도 약 1.8 배였다. bounce/left 의 움직임과 맞는다.
- **교란:** 크기 변경과 "최근 8 개에 없던 텍스처" 는 상관 0.97 로 겹친다. 직접 비용은 새 크기 텍스처의 할당일 수 있다.

### 비용

| | off | noclear | clear |
|---|---|---|---|
| GPU 바쁨 | 6.40% | 6.76% | 8.47% |
| bind 당 추가 GPU | -- | 약 +0.1 ms | 약 +1.0 ms(지우기) |
| 정지 비디오에서 GPU 완료가 12 ms 이상 늦은 프레임 | 0.10% | 0.66% | 5.44% |
| VRAM(정지 비디오, 디바이스당) | 476 MB | 533 MB | 533 MB |

- 여섯 런 모두 BeginDraw·EndDraw·Commit 오류는 0 이었다(WALLCOMMIT `rej=0`).
- clear 는 늦은 렌더에서 이득이 없고 비용만 크다. 그래서 noclear 를 기본으로 했다.

### 레버를 켠 런에 남은 늦은 렌더 (38 개)

| 분류 | 개수 |
|---|---|
| 시작 첫 프레임(타일당 1 회) | 16 |
| 부트 첫 이미지 업로드(타일당 31.6 MiB, 39.7~45 ms) | 16 |
| 업로드 바로 다음 프레임 | 2 |
| 일반 전환 31 개 안 | 4 (전환당 0.13) |

---

## 5. ticker judder 수정과의 비교 -- 어느 캐시에 작용했나

ticker 를 잡은 "cache 설정" 은 T7/T8 이다(`36212c20ee7`, 기본값 `b24687d184c`, 2026-10-20).

- **원인:** 투명 서피스의 BeginDraw 크기가 매 프레임 바뀌어 DComp 가 텍스처를 계속 새로 받던 것.
- **효과:** 텍스처 크기 48 종 → 1 종, 8 ms 넘는 Commit 2,668 → 9, 벽 패스 6.35 → 1.03 ms.
- **오늘 수정과의 관계:** 같은 층, 같은 함수다. 다른 것은 대상 서피스뿐이다.
  - **ticker 텍스트 띠:** WR 은 슬라이스 배경이 불투명이거나, 불투명 사각형·이미지가 유효 영역을 덮어야 타일을 불투명으로
    본다(picture.rs 약 1200~1248 행). 텍스트 띠에는 그런 것이 없어 알파(투명) 서피스에 놓인다. 그래서 `expand_transparent` 분기에서 고정됐다.
  - **보드:** 이미지·비디오가 유효 영역을 덮어 불투명 서피스가 된다. 그래서 그 분기를 타지 않았고, 오늘 `expand_opaque` 분기를 더했다.

### 층별 지도 (콘텐츠 → present)

| 층 | 캐시·풀 (키) | ticker 쪽 수정 | 오늘 수정 |
|---|---|---|---|
| 애니메이션 값 샘플링 | 없음 | `8a4256e300b`: paint-side 런웨이가 반복 경계에서 값을 붙들던 것 | -- |
| WR picture cache 슬라이스 구조 | 슬라이스 집합. 스크롤 루트를 크기 문턱으로 고른다(spatial_tree.rs 약 394~409 행) | `7ff4b74aa49`: 애니메이션되는 transform 이 매 사이클 문턱을 넘나들어 슬라이스를 다시 만들던 것(DComp 서피스 4↔12, `2fa18355000` 계측) | -- |
| WR 프레임 발행 | picture·texture·glyph 캐시 작업이 든 프레임 | 11-02 single publish(`6ff39f2c82e`, 기본값 `f70cb3c1b3d`): 그런 프레임을 그리고 버리던 것. 4 타일 벽의 전 구성 judder 를 없앴다 | -- |
| picture cache 타일 무효화 | valid / dirty rect | 손대지 않음. 크기 변동의 출발점은 움직이는 텍스트 띠 | 손대지 않음. 출발점은 보드 범위 = valid rect(`ValidRectChanged`). `775284328ee` 가 여기를 건드리려다 무효였다 |
| WR 렌더 타깃 풀 | 정확한 크기 | (11-12 회전+opacity `60c1bfd9a5a`, `a1f848f3d39`: 각도마다 바뀌는 중간 surface 크기. 재할당 10 초당 254~290 → 9~20) | -- |
| WR 텍스처 캐시 · 글리프 캐시 | 이미지 키 · 글리프 키 | (11-13 텍스처 캐시 예산 `cc1c70f98bf`. 엔진 기본은 꺼짐, 운영자 명령줄은 1024) | -- |
| **DComp BeginDraw → DComp 텍스처 풀** | **갱신 영역 크기** | **T7/T8: 투명 서피스 고정** | **`4284116d4c7`/`1d777e05984`: 불투명 서피스 고정** |
| ANGLE pbuffer와 깊이·스텐실 버퍼 | bind 마다 새로 만든다 | 무관(T6 재사용 시도는 취소) | 무관. 단독 원인 아님(런 14) |
| DComp Commit, DWM | -- | 원인 아님 | 원인 아님. 긴 Commit 은 풀 미스 비용이 드러나는 자리였다 |

### 공통된 모양

**크기를 키로 재사용하는 구조가, 매 프레임 바뀌는 크기 때문에 깨진다.**

- 슬라이스: 크기 문턱
- 렌더 타깃: 정확한 크기
- DComp 텍스처: 갱신 영역 크기

수정은 모두 그 키를 고정한 것이다. 반대로 Commit 시점·DWM 위상·페이싱처럼 타이밍 층을 바꾼 시도는 어느 쪽에서도 효과가 없었다.

---

## 6. WebGL 간 전환 -- 이 수정과의 관계

운영자의 `-NoLog` 임의 시험(2026-11-17, 새 기본값 빌드)에서 대부분의 전환은 judder 가 사라졌다. WebGL → WebGL
전환만 변화가 없다.

### 메커니즘은 독립이다 (코드 확인)

- `4284116d4c7` 과 `1d777e05984` 는 WebGL 코드를 바꾸지 않았다. 바꾼 것은 DComp bind, pref, 런처, 계측뿐이다.
- WebGL 캔버스는 별도 합성 서피스로 승격되지 않는다. `PREFER_COMPOSITOR_SURFACE` 는 비디오 escape 에만 붙는다
  (layout display_list/mod.rs 약 924 행).
- 캔버스는 보드의 picture cache 타일 안에 보통 이미지로 그려진다. 그래서 그 타일의 bind 는 오늘 고정을 그대로 받는다.
  투명 슬라이스라면 T7/T8 로 원래 고정돼 있었다.
- 즉 WebGL 이 이 수정을 우회하지도 않고, 이 수정이 WebGL 에 기대지도 않는다.

### 프레임 시간은 공유한다

- WebGL lock 은 같은 painter 렌더 안의 `prepare_gpu_cache`(renderer/mod.rs 약 1758 행)에서 돈다. 이것은 DComp bind 가 있는 draw_frame 보다 앞이다.
- unlock 은 그 뒤(약 1799 행)에서 돈다.
- 그래서 WebGL 대기는 그 타일의 bind 와 표출을 직접 늦춘다.
- 11-13 의 WebGL 꼬리 측정(런 65/66)은 오늘 수정 전 빌드였다. BeginDraw 풀 미스 비용이 섞였을 수 있어, 새 빌드로 다시 재야 한다.

### 이 수정이 닿지 않는 WebGL 쪽 후보

1. **나가는 컨텍스트 정리.**
   - 2026-11-12 에 운영자가 관찰했다(`2c9233c84c8`): "WebGL 간 전환에서만 애니메이션 끝 직후 멈칫. WebGL 이 아닌 구성에서
     WebGL 로 가는 전환에서는 없음."
   - 정리 시각(durationMs)에 4 GPU 의 swap chain 을 파괴한다.
   - painter 쪽 destroy 단계(그 안에 ReleaseSync)가 p50 8.3 ms, 최대 42 ms 였다(`8cac5b0aea1`).
   - `2c9233c84c8` 의 테이블 락 수정은 이 빌드에 들어 있지만, 효과를 잰 런은 없다.
2. **들어오는 컨텍스트 생성.**
   - WebGL 보드마다 새 iframe 문서다. 그래서 새 컨텍스트가 GPU 넷에 백엔드를 하나씩 만들고, 이 일은 단일 WebGL 스레드에서 한다.
   - 전환마다 11520x4321 서피스 16 개(각 약 199 MB)가 만들어진다(런 52, `1326d3e5e77`).
   - painter 마다 share handle 가져오기 캐시가 비어 있다.
   - 오늘 원인과 같은 "크기 키 풀이 차가운" 사건이지만, surfman·WebGL 쪽이다.
3. **rAF 감속.**
   - 운영자 명령줄이 `-Pref ...gfx_paint_side_animation_tick_divisor=15` 를 넘기고, `-CanvasDefeatsThrottle` 은 쓰지 않는다.
   - 보드 전환(transform/opacity WAAPI)이 도는 동안 WebView 전체가 paint-side 가 된다(iframe 포함 OR 판정, painter.rs 약 1561 행).
   - 그래서 WebGL rAF 가 초당 약 4 회로 떨어진다.
   - clip-path 만 쓰는 프리셋에는 해당하지 않는다.
4. **캔버스 ack 관문.**
   - 단일 WebGL 스레드가 백엔드 넷을 모두 swap 해야 epoch 가 지불된다.
   - 새 컨텍스트 생성이 그 스레드를 잡으면, 나가는 보드의 rAF 와 표시 목록 갱신도 늦어진다.
   - 시한은 `-CanvasAckDeadlineMs`(2000 ms)다.

이미 무효로 판정된 레버(다시 제안하지 말 것):
- `-WebglTileScissorClear`, `-WebglDeferredFrontBuffer`: 런 51/52 에서 무효였다.
- `-NonblockingFrontBufferLock`: 런 29 에서 악화돼 제거됐다.

### 판정에 필요한 것

- **운영자 확인:**
  - WebGL 간 judder 가 움직이는 동안 내내인지, 전환 끝 직후의 멈칫인지. 끝 직후면 1, 움직이는 동안 내내면 3, 시작 부근이면 2 쪽이다.
  - 이미지↔WebGL 전환에서도 나는지.
- **벽 런:**
  - 구성: WebGL 간 전환을 반복하는 페이지로, `-NoLog` 없이 런 16 명령에 `-TickPhase` 를 더해 돌린다.
  - 갈래: 기준 갈래와 `-CanvasDefeatsThrottle` 갈래를 교대한다.
  - 읽을 것:
    - `WALLBINDINFO`: WebGL 보드 타일이 고정되는지(`expand_opaque` 또는 `expand_transparent`)
    - `TICKCADENCE`: rAF 횟수
    - `SURFLIFE` / `DESTROYSTEP` / `LOCKTAIL`: 생성·정리 시각
    - `render.over`

---

## 7. 남은 것

- **부트 첫 이미지 업로드:** 타일당 1 회, 31.6 MiB, 40~45 ms.
  - 페이지가 다음 장면을 opacity 0 으로 미리 붙여 두지만, WR 이 그 장면을 방문하지 않아 미리 올라가지 않는다.
  - 업로드는 보드가 타일에 들어오는 프레임에 일어난다.
- **첫 비디오 전환의 투명 서피스 첫 bind:** T1 의 +0.86 s 부근에 Commit 이 늘었다. 비디오 시작 직후(media.create 뒤
  65~90 ms)의 긴 Commit 도 있다. 둘 다 눈에는 띄지 않았다.
- **레버의 빈틈(코드로만 확인, 벽에서는 미관측):**
  - 레버 조건의 클립은 직전 프레임의 `add_surface` 값이다.
  - 불투명 서피스의 유효 영역이 줄어드는 프레임(보드가 타일을 떠날 때)에는 `contains_box` 가 실패할 수 있다.
    그러면 크기가 바뀌는 예전 경로로 돌아간다.
  - 런 16 에서는 불투명 bind 29,372 개 모두 `size_changed=0` 이라 나지 않았다.
  - 다른 콘텐츠에서는 `WALLBINDINFO` 의 `expand_opaque` 대 `opaque` 로 본다.
- **매 분 :16~:23 초 대역:** 같은 일·같은 래스터 양인데도 GPU 가 35~55% 빠르다. 원인은 미상이다. 환경 탓으로 단정하지 않는다.
- **WebGL 간 전환:** 6 절.

---

## 8. 정정 기록

- **"dirty < valid 라서 크기가 바뀐다"(런 14 뒤의 내 설명)는 틀렸다.** dirty 는 원래 valid 와 같았고, 바뀌는 것은 valid
  자체였다. 그래서 `-DcompFullTileUpdates` 는 무효였다(런 15).
- **"유효 영역이 32 px 단위로 올림된다" 는 틀린 표현이다.** WR 은 장치 픽셀까지만 반올림한다(picture.rs 약 1154~1158 행).
  W x 2176 은 BeginDraw 가 돌려준 텍스처의 `GetDesc` 크기이고, 갱신 영역은 2160 이다. 32 px 단위는 DComp 할당 단위로
  보인다(추정). `4284116d4c7` 커밋 메시지의 같은 표현도 이렇게 읽는다.
- **"런처 기본 tick divisor 15" 는 틀렸다.** 런처는 이 pref 를 넘기지 않는다(엔진 기본 4). 15 는 운영자 명령줄의 `-Pref` 에서 온다.
- **긴 Commit 은 원인이 아니라 비용이 드러나는 자리였다.**
  - Commit 앞에 flush 나 Signal 을 넣으면 막힘이 그리로 옮겨 갔다(런 13).
  - 디바이스별 관문과 분리로는 대기가 다른 자리로 옮겨 갔을 뿐이다(런 12).

---

## 9. 계측 읽는 법 (`4284116d4c7` 이후)

### `WALLBINDINFO t= dev=` (디바이스별, 10 초)

| 필드 | 뜻 |
|---|---|
| `binds` | bind 수 |
| `opaque` | 불투명 서피스 bind |
| `dirty_is_valid` | dirty == valid 인 bind |
| `expand_transparent` | T7/T8 고정이 걸린 bind |
| `expand_opaque` | 이 레버가 걸린 bind |
| `sizes`, `top` | 갱신 크기의 가짓수와 상위 셋 |
| `size_changed` | 같은 서피스의 직전 bind 와 크기가 다른 bind |
| `tex_seen_in_last8` | 최근 8 개 안에서 본 텍스처 |
| `slow_begin_changed` / `slow_begin_same` / `slow_begin_first` | 느린 BeginDraw(120 us 초과)를 크기 바뀜·같음·첫 bind 로 나눈 것 |

판정할 때 먼저 볼 것: 레버가 켜졌다면 `expand_opaque == opaque`, `sizes=1`, `size_changed=0` 이어야 한다.

### `WALLGPUFRAME ... upd=` (늦은 프레임 행)

그 렌더의 bind 마다 `WxH[(v유효WxH)]:flags` 를 적는다.

| 플래그 | 뜻 |
|---|---|
| `o` | 불투명 |
| `d` | dirty == valid |
| `t` | 투명 확장 |
| `x` | 불투명 확장 |
| `c` | 같은 서피스 직전과 크기가 다름 |
| `n` | 이력 안에서 그 서피스의 첫 bind |

### 함께 읽는 줄

- 판정 지표: `WALLEVENT render.over`(육안과 맞는 지표), `WALLGPUQUEUE blocked`
- 비용: `WALLGPU busy` / `span`, `WALLGPUTAG W_Bind`
- `-GpuTimeProbe` 의 요소별 GPU 시간은 flush 가 있어야 정확하다. `-DcompAlwaysFlush` 나 `-GpuFenceProbe` 와 함께 읽는다.

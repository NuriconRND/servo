# WebRender 비디오 offload — 천장 검증 설계

- 날짜: 2026-07-02
- 브랜치: `nuriconrnd/multigpu-tiled-wall` (기준 HEAD `66d93ac44`)
- 범위: **측정/de-risk 세션.** 엔진 구현 없음. C2(내부 fast-path)를 지을 가치가 있는지 숫자로 판정.
- 관련 메모리: `webrender-video-offload`, `multigpu-next-session-tasks`

---

## 1. 배경 / 확정된 근본 원인 (재조사 불필요)

다중 비디오 컴포지터 처리량이 이 머신(FHD30 H.264 타일)에서 **클린 60fps ≈ 24~30 타일**에서 막힘. 목표는 **45~64 타일 60fps**.

근본 원인(이전 세션 확정): WebRender가 각 `<video>`를 **제각각 큰(1080p) 업로드 텍스처**로 그림 → **배칭 불가** → 레이어당 draw call(텍스처 바인드+유니폼)을 **ANGLE가 GL→D3D11로 단일 컴포지터 스레드에서 번역** → CPU 커맨드 제출이 O(N). 40타일에서 `renderer.render()` ≈ 17~20ms CPU인데 A5000 GPU는 ~26%(GPU 바운드 아님), 업로드 <1ms(I420 borrowed/zero-copy), 디코드는 별도 SW CPU.

이미 배제된 막다른 길(재시도 금지): NV12/BGRA 변환(zero-copy 깨짐, 업로드 폭발), HW 디코드(36+ 타일에서 배제), DWM vsync 드라이버(과부하 폭주), 애니메이션 backpressure(stall 유발).

## 2. 이번 세션에 확정한 제약

- **영상 소스:** 45~64 타일 모두 **서로 다른** 영상(최악 케이스). 공유 디코드/텍스처 지름길 없음.
- **DOM 층위:** **비디오 사이 DOM 인터리빙 필요** — 개별 비디오 사이 z끼워넣기 및 per-video CSS 효과(둥근 모서리/블렌드/필터/트랜스폼)를 요구. 따라서 임의 페이지 호환이 필요.
- **관심 범위:** 콘텐츠측 빠른 승리와 범용 엔진 개선 둘 다.

### 2.1 코드 조사로 드러난 블로킹 사실

`components/script/dom/html/htmlvideoelement.rs:123` `get_current_frame_data()`:

```rust
if frame.is_yuv() {
    None                                         // YUV 프레임 → None (WebGL에 아무것도 안 감)
} else if !frame.is_gl_texture() {
    Snapshot::from_vec(... frame.get_data().to_vec())  // BGRA → 프레임당 전체 CPU 복사
} else {
    Snapshot::cleared(size)                      // GL 텍스처 → blank(미구현)
}
```

우리 파이프라인은 **I420 borrowed(YUV) sink**이므로 콘텐츠 페이지의 `texImage2D(video)`는 **`None`을 반환(검은 화면)**. BGRA 강제는 프레임당 ~8MB CPU 복사 × N = readback 막다른 길. 즉 **"값싼 콘텐츠측 C1(진짜 비디오를 WebGL 한 레이어에 그리기)"은 엔진 수정 없이는 불가능.**

## 3. 방향 결정 — C2 (Servo 내부 fast-path)가 타깃

| 접근 | 비디오+DOM 정합성 | 배관 규모 | 이번 세션 판정 |
|---|---|---|---|
| **C1** 콘텐츠측 단일 WebGL canvas | 그리드 위/아래 오버레이만. **인터리빙 불가** | (원래 작다지만) video→WebGL zero-copy 브릿지 부재로 **막힘** | **탈락** (인터리빙 요구 위반 + 코드상 불가) |
| **C2** 내부 컴포지터 fast-path | **전 조합 자동 정확** (안전한 z-연속/비-occlusion 런만 병합, 나머지는 병합 쪼갬) | **최소** — YUV borrowed 평면이 이미 컴포지터 안에 있음 → CPU 왕복·변환 없이 자체 YUV→RGB 셰이더+N quad로 1개 중간 텍스처에 모아 WebRender에 이미지 1개로 넘김 | **타깃** (단, 구현은 다음 세션) |
| **A** DirectComposition 오버레이 | OS 컴포지터가 처리(자연스럽지만 HW 오버레이 평면 수·직사각형/단순알파 제약) | 초대형 (이 WebRender엔 `Compositor` API 부재) | **폴백** (접기론이 실패할 때만) |

**결론:** 인터리빙 요구 + video→WebGL 브릿지 부재 때문에 **C2가 유일하게 정합적이면서 배관도 가장 적은 경로.** 단 이번 세션은 C2를 구현하지 않고 **지을 가치가 있는지 천장만 검증.**

## 4. 검증 질문 2개로 분해

- **Q1 (연산 천장):** N개 비디오를 **한 병합 패스**로 그리고 그 위에 DOM을 얹으면 45~64 타일 60fps가 되는가? → C2를 지을 go/no-go.
- **Q2 (병합 가능성 천장):** 실제 월 레이아웃에서 몇 개의 비디오가 실제로 병합 가능한가(서로 공간적으로 안 가리는가). DOM이 매 비디오 사이를 진짜로 갈라놓으면 C2 이득이 상한됨. → 실제 콘텐츠 레이아웃 의존 사안. 이번 세션엔 **가정으로 명시**하고 Q1에 집중.

## 5. 산출물 (엔진 변경 0)

### 5.1 합성 WebGL 하네스 — `tests/html/video_collapse_ceiling.html`

한 WebGL canvas에 N개 quad를 **단일 레이어/단일 패스**로 그림. 파라미터: `?grid=N` (또는 `?cols=&rows=`), `?mode=v1|v2`, `?dom=0|1`, `?log=1`.

- **V1 (낙관 상한):** **공유 텍스처 1개**를 N quad로 그림. 디코드·업로드 혼입 0 → 순수 draw-call 접기 비용만 격리. "N draw call(우리 단일 컨텍스트) vs N WebRender 레이어"의 순수 비교.
- **V2 (보수 상한):** **N개 서로 다른 1080p 합성 텍스처를 매 프레임 `texImage2D`/`texSubImage2D`로 갱신** 후 N quad. RGBA 업로드까지 포함 → **실제 C2(YUV borrowed 평면 재사용)보다 무거운 상한.** V2가 통과하면 C2는 확실히 통과(보수적 경계).
- **`dom=1`:** 병합된 비디오 레이어 **위에 DOM 자막/테두리 오버레이**를 타일마다 얹어 혼합 케이스 측정.

두 변형이 낙관/보수 상한을 브래킷: `[V1, V2]` 사이에 실제 C2가 위치.

### 5.2 Baseline B

기존 `tests/html/video_grid_6x6_perf.html`(N개 진짜 비디오 = N WebRender 레이어). `dom=1` 변형 추가(타일마다 DOM 오버레이). 이것이 우리가 이겨야 할 **현재값**.

### 5.3 측정 프로토콜

- 계측: `SERVO_LOG_PRESENT_CADENCE=1` + `RUST_LOG=warn,paint=info` → presents/s, max_gap, pending + ">16ms 프레임"의 wr_update(업로드)/wr_render(그리기) 분해.
- 그리드: **N = 30 / 40 / 45 / 64**.
- 각 N에서 **B / V1(dom=0,1) / V2(dom=0,1)** 실행, 정상 상태 30초 후 지표 기록.
- 런처: 기존 `etc/multigpu/run_video_grid_6x6.ps1` 패턴 재사용(하네스 URL/파라미터만 교체).
- 머신 주의: 디스플레이 Intel iGPU @60Hz, 합성 NVIDIA A5000 → cross-adapter present. PresentMon(`D:\PresentMon-2.3.1-x64.exe`, admin, servoshell 반드시 foreground)로 물리 present cadence 교차확인.

### 5.4 결과 문서

- `etc/multigpu/WEBRENDER_VIDEO_OFFLOAD_STATUS.md` 신규 — B/V1/V2 표(N×지표), 판정, 다음 단계.
- 메모리 `webrender-video-offload` 업데이트(검증 결과 + C2 착수 여부).

## 6. 판정 기준 (go/no-go)

| 관측 | 해석 | 다음 |
|---|---|---|
| **V2가 64@60fps 유지** | 무거운 업로드 상한에서도 접기가 통함 | **C2 착수 강하게 정당화** |
| V1은 되나 V2 안 됨 | 접기 자체는 싸고 **업로드가 관건** | C2의 zero-copy 평면 재사용이 결정적 → C2 착수(업로드 경로가 이득의 핵심) |
| **V1도 45@60fps 못 감** | draw-call 접기론 자체가 틀림 | 접기 접근 폐기 → **접근 A(네이티브 오버레이) 재검토** |
| B가 이미 목표 근처 | 병목 재평가 필요 | 원인 재조사 |

## 7. 리스크 / 가정

1. **V2 업로드 과대평가(의도적):** RGBA `texImage2D`는 실제 YUV borrowed보다 무거움 → 보수적 경계라 OK. V1/V2가 실제 C2를 브래킷.
2. **Q2 병합 가능성:** C2의 실이득은 실제 레이아웃 의존. 자막이 각 비디오 위에만 있고 비디오끼리 안 가리면 최대 이득; DOM이 비디오를 공간적으로 갈라놓으면 이득 하락. 이번 세션 범위 밖(가정으로 명시).
3. **디코드 천장:** N개 서로 다른 SW 디코드 CPU가 컴포지터보다 먼저 벽일 수 있음. V1/V2는 디코드가 없어 **순수 컴포지터 천장을 격리**해서 보여줌 → 디코드 벽은 Baseline B의 CPU와 대조해 별도 확인.
4. **WebGL 컨텍스트 스레드/디바이스:** Servo WebGL이 WebRender와 같은 ANGLE→D3D11 단일 스레드를 공유하면 draw가 사라지는 게 아니라 옮겨지는 것 — 단 WebRender의 per-layer 배칭/타일캐시 오버헤드(17~20ms의 출처)는 우회. 측정이 이 메커니즘을 드러냄.

## 8. 명시적 비목표 (이번 세션)

- C2/A의 실제 엔진 구현. (검증 후 별도 세션)
- video→WebGL zero-copy 브릿지 구현.
- 디코드 파이프라인 최적화.
- 프로덕션 월 콘텐츠 재작성.

## 9. 핵심 파일 (참고)

- `components/paint/painter.rs` (update_images=비디오 이미지→합성, render(), present cadence 계측)
- `components/script/dom/html/htmlvideoelement.rs` (§2.1 video→snapshot 경로)
- `components/media/backends/gstreamer/render.rs` (I420 borrowed YUV sink)
- `tests/html/video_grid_6x6_perf.html`, `tests/html/anim_stress.html` (기존 하네스 참고)
- `etc/multigpu/run_video_grid_6x6.ps1` (런처 패턴)

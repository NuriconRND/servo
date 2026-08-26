# 멀티 GPU 타일 확장 시 fps 붕괴 — 구조 진단

작성 2026-08-21. 대상: HEAD(`4ce9526b801`) + `gfx_video_escape_mode=external`.

증상(사용자 실측):

- `virtualViewport 11520x4320`, 타일 1개(5760x2160)를 display 0/1/2/3 로 바꿔 가며 재생
  → **네 경우 모두 30fps 재생**. 단 display 0 외에는 다소 느려 보임.
- **타일을 2개로 늘리면 ~10fps 로 붕괴.** 그런데 **GPU 점유율은 1타일 때와 비슷**.

총 픽셀 처리량으로 보면 `1타일×30fps = 30` vs `2타일×10fps = 20` 으로 **오히려 줄었다.**
GPU 가 한계였다면 점유율이 올라가면서 fps 가 떨어져야 한다. 점유율이 그대로라는 것은
**GPU 작업량이 아니라 CPU 쪽 직렬화/실패 경로가 지배한다**는 뜻이다.

---

---

# ★2026-08-21 갱신 — 아래 §1~§4 의 가설은 실측으로 반증됐다★

실기 로그 2건(`20260821_log_singlegpu.txt`, `20260821_log_multigpu.txt`)을 받아 확인한 결과:

| | 1타일 (singlegpu) | 2타일 (multigpu) |
|---|---|---|
| `presents_per_s` | mean **65.2** / p50 65.3 | mean **14.4** / p50 13.1 |
| 논리 fps (= presents ÷ 타일수) | ≈ **65** | ≈ **6.6** |
| `avg_present_ms` | **0.00** | **0.00** |
| `D3D11 media: EGLImage 래핑 실패` | 0 | ★**0**★ |
| warn 종류 | SetTrackFailed×72, WR셰이더×1, 기타 2 | **동일**(WR셰이더만 1→2 = painter 수) |

**반증된 것 둘:**

1. ★**디바이스 불일치 가설(§1 (b)~(d)) 은 틀렸다.**★ 2타일 실행에 `EGLImage 래핑 실패` 가
   한 줄도 없다. §3 에 "이 예측이 빗나가면 진단이 틀린 것" 이라 적어 둔 그대로다.
   (구조 서술 자체 — 전역 `CONSUMER_DEVICE`, `MiscFlags: 0` — 는 코드 사실이지만,
   **관측된 성능 붕괴의 원인은 아니다.**)
2. ★**순차 present + vsync = `60/N` 이론(§1 (f)) 도 틀렸다.**★ `avg_present_ms=0.00` —
   present 는 사실상 공짜다(DComp surface 모드에서 창 present 를 건너뛴다).

**확정된 사실:**

- 이 장비는 진짜 4-GPU 다. `display 0→adapter 0`, `1→adapter 2`, `2→adapter 3`,
  `3→adapter 1` (LUID 전부 다름). 2타일 실행은 adapter 0 + adapter 2 다.
- 붕괴는 **초선형**이다. 타일 2배에 논리 fps 는 1/10. 총 present 처리량도 65→14 로 줄었다
  (일이 나뉜 게 아니라 **총량이 줄었다**).
- **새 경고가 하나도 없다.** 실패 경로를 타는 게 아니라 어딘가에서 **기다리고 있다.**
- 비용은 present 가 아니라 **렌더 또는 그 위(프레임 생산/배리어)** 에 있다.
  winit_wall 계측 주석이 바로 이 구분을 위해 present 를 따로 재게 해 둔 것이다.

**왜 더 좁히지 못했나 — `RUST_LOG` 가 설정되지 않았다.** 필요한 숫자가 전부 `info!` 라
로그에 없다:

- `Wall frame barrier complete: ... status=completed_before_deadline|completed_after_deadline
  ready=N/M first_to_all_ready_ms=.. request_to_all_ready_ms=.. need_repaint=..` (`paint.rs:626,713`)
- `Wall render start/end: painter PainterId(N) ... render_ms=.. pending=..` (painter 별 렌더 시간)

→ **다음 실행은 §5 의 계측 명령으로.**

## 남은 유력 후보 (아직 검증 안 됨 — 성급히 확정하지 말 것)

1. **월 프레임 배리어 스톨.** 기본값이 `gfx_wall_frame_max_pending=1`,
   `gfx_wall_frame_min_interval_ms=16`, 배리어 데드라인 16ms 다. 타일이 1개면 배리어가
   사실상 무의미하지만 2개부터는 매 프레임 두 타깃을 기다린다. 그리고 ★데드라인 16ms 는
   60Hz vsync 주기 16.667ms 보다 짧아 원래 달성 불가 기준★이다(기존 기록). 매 프레임
   `completed_after_deadline` → `keep-previous-frame` → 재요청이 돌면 초선형 붕괴가 된다.
   **`first_to_all_ready_ms` 와 `request_to_all_ready_ms` 가 이걸 즉시 가른다.**
2. **타일 순차 렌더로 GPU 간 중첩이 0.** `render_all_tiles` 가 메인 스레드에서 타일을 하나씩
   처리하므로 GPU0 이 일할 때 GPU2 는 놀고, 그 반대도 마찬가지다. ★이것은 "각 GPU 점유율이
   1타일 때와 비슷하다" 는 관측과 정확히 맞는다★ — 각 GPU 가 절반의 시간만 일하기 때문이다.
   다만 이것만으로는 2배이지 10배가 아니다.
3. ~~교란 요인 미확인~~ → ★**해소됨**★ (2026-08-21 사용자 확인). 두 레이아웃 파일 모두
   `virtualViewport` 가 **11520x4320 으로 동일**하다. 즉 위 65 → 6.6 은 **뷰포트를 고정한 채
   타일 수만 1 → 2 로 바꾼 순수 비교**다. 교란 없음.

★`RUST_LOG` 의 정체도 확인됐다★ — 미설정이 아니라 **`warn` 으로 명시 설정**돼 있었다.
그래서 `info!` 인 배리어/렌더 로그가 전부 필터됐다. `warn,paint=info` 로 올려야 한다.

---

# (이하 반증된 원래 가설 — 코드 구조 서술은 유효하므로 기록으로 남긴다)

## 결론 한 줄

**비디오 D3D11 plane 링의 "소비자 디바이스"가 프로세스 전역 단 하나인데, painter 는 타일마다
자기 GPU 의 디바이스를 따로 만든다.** 타일이 2개 이상이면 나중에 만들어진 painter 가 전역
값을 덮어쓰고, **나머지 painter 는 매 프레임·매 plane 마다 텍스처 래핑에 실패**한다.

---

## 1. 코드에서 확인된 구조 (전부 HEAD 실물)

### (a) 타일마다 별도의 어댑터 + 디바이스

`paint_api::rendering_context::create_adapter_for_requested_gpu()` 가 타일의
`requested_gpu_index`(= 그 display 를 구동하는 GPU) 로 DXGI 어댑터를 잡는다.
로그: `Selected DXGI adapter index N for requested target GPU`.

→ **painter 마다 서로 다른 `ID3D11Device`.** 다른 GPU 면 당연히 다르고, 같은 어댑터라도
surfman 컨텍스트가 달라 디바이스는 별개다.

### (b) 그런데 링의 소비자 디바이스는 전역 1개

```
components/media/player/d3d11_ring.rs
    static CONSUMER_DEVICE: Mutex<Option<usize>> = Mutex::new(None);   // ← 프로세스 전역 1개
    pub fn set_consumer_device(device: usize) { *lock(&CONSUMER_DEVICE) = Some(device); }
```

`Painter::new` 는 **painter 마다** 외부 이미지 핸들러를 등록한다
(`painter.rs:405` `WebRenderExternalImageHandlers::new` → `:434`
`WindowGLContext::initialize_image_handler(.., rendering_context.clone())`), 그리고 그 안에서:

```
components/media/media-thread/lib.rs:333
    let image_handler = Box::new(MediaExternalImages::new(thread_sender, Some(rendering_context)));
        └─ MediaExternalImages::new → D3d11PlaneRings::set_consumer_device(device)   // ← 덮어쓴다
```

**painter 0 이 자기 디바이스를 publish → painter 1 이 만들어지며 그 값을 덮어쓴다.**
프로듀서(`render-d3d11/lib.rs:173`)는 `consumer_device()` 를 읽어 그 디바이스 위에 plane
텍스처를 만든다. 즉 **모든 비디오 텍스처가 "마지막에 생성된 painter" 의 디바이스 소속**이 된다.

### (c) 그 텍스처는 공유가 아예 불가능하다

```
components/media/backends/gstreamer/render-d3d11/ring_producer.rs  create_dynamic_texture()
    Usage:          D3D11_USAGE_DYNAMIC
    BindFlags:      D3D11_BIND_SHADER_RESOURCE
    CPUAccessFlags: D3D11_CPU_ACCESS_WRITE
    MiscFlags:      0            // ← SHARED / SHARED_NTHANDLE 없음
```

`MiscFlags: 0` 이라 **cross-adapter 는 물론이고 같은 어댑터의 다른 디바이스와도 공유되지
않는다.** 이건 설계상 의도된 것이다 — 이 경로는 "CPU memcpy → 소비자 디바이스의 DYNAMIC
텍스처 → 그 디바이스가 직접 샘플" 을 전제로 만들어졌고(§3-n DynamicUploadSet), 그 전제에
**소비자는 하나**가 깔려 있다.

`wrap_d3d11_texture_as_gl_texture` 의 doc 주석도 그렇게 못박아 두었다 —
*"D3D11 텍스처(**이 컨텍스트의 디바이스 소속**)를 EGLImage 로 GL 텍스처에 바인딩한다"*.

### (d) 그래서 짝이 안 맞는 painter 는 매 프레임 실패한다

```
components/media/media-thread/lib.rs:696
    match rc.wrap_d3d11_texture_as_gl_texture(plane.texture) {
        Some(wrap) => { self.d3d11_wrap_cache.insert(plane.texture, wrap); wrap },
        None => {
            warn!("D3D11 media: EGLImage 래핑 실패 (texture={})", plane.texture);
            return (ExternalImageSource::Invalid, Size2D::zero());     // ← 비디오 텍스처 없음
        },
    }
```

★**실패는 캐시되지 않는다.**★ `d3d11_wrap_cache` 는 성공했을 때만 채워지고, 게다가
**painter 마다 자기 캐시를 따로 가진다.** 그래서 짝이 안 맞는 painter 는

```
36 비디오 × 3 plane(I420) = 프레임당 108회
  → wrap 시도(드라이버 호출) 실패 → warn! 포맷팅 → 비버퍼 stderr 기록
```

를 **매 합성마다 반복**한다. 10fps 라도 초당 1,080 줄이다.

### (e) 링 회전 상태기계도 소비자 1개 전제다

`note_plane_lock_and_plan(ring_id)` 는 *"합성당 1회 소비: 링별 lock_count 0→1 전이에서만
plan 이 나온다"*. painter 가 N 개면 같은 링에 대해 lock 이 N 번 들어오므로 **0→1 은 한
painter 만** 얻는다. 나머지는 plan 없이 다른 슬롯을 보게 된다. `escape=external` 경로
(`MediaVideoExternalSurfaceProvider::acquire`)도 **같은 전역 링**을 쓰므로 동일하다.

### (f) 덧붙여, 표출 자체가 타일 순차다

```
components/servo/examples/winit_wall/main.rs:297  render_all_tiles()
    for tile in &self.tiles {
        tile.rendering_context.make_current();   // 컨텍스트 전환
        webview.paint_target(target);            // 이 타일 WebRender 렌더
        tile.rendering_context.present();        // 스왑
    }
```

메인 스레드에서 **타일을 하나씩** 처리한다. 타일당 5760x2160 present 이고,
`gfx_vsync_enabled=true` 면 present 가 vsync 에 걸리므로 논리 프레임 하나에 N 번 기다린다.
이것만으로도 상한이 `60/N` fps 다(2타일 → 30fps). **관측된 10fps 를 설명하려면 (d) 가 필요하다** —
순차 present 는 상한을 30 으로 낮출 뿐 10 으로 떨어뜨리지 못한다.

---

## 2. 증상이 정확히 맞아떨어지는 이유

| 관측 | 설명 |
|---|---|
| 타일 1개면 display 0/1/2/3 어디서든 30fps | painter 가 **하나뿐**이라 그 painter 가 publish 한 디바이스 = 텍스처 소유 디바이스. 항상 일치한다 |
| display 0 외가 다소 느림 | 그 display 를 구동하는 GPU 가 다르거나(성능 차) 프레젠트 경로가 다름. (b)~(d) 와는 무관한 별개 축 |
| 타일 2개면 ~10fps | painter 중 **하나는 반드시 디바이스가 어긋난다** → 프레임당 108회 실패 래핑 + warn 플러시 |
| **GPU 점유율은 그대로** | 추가된 비용이 GPU 작업이 아니라 **CPU 쪽 드라이버 호출 실패 + 로그 I/O** 다. 어긋난 painter 는 비디오 텍스처를 못 받으므로 GPU 로 보낼 그림이 오히려 **줄어든다** |

마지막 줄이 이 진단의 핵심 근거다. "느려지는데 GPU 는 안 바쁘다" 는 GPU 병목으로는 설명이
안 되고, 이 구조로는 정확히 설명된다.

---

## 3. 확인 방법 (재실행 없이, 기존 로그로)

어긋난 painter 의 실패는 `warn!` 이라 기본 `RUST_LOG=warn` 에서도 찍힌다.

```powershell
# 2타일 로그
Select-String -Path <2타일 로그> -Pattern "EGLImage" | Measure-Object | % Count
# 1타일 로그 (0 이어야 한다)
Select-String -Path <1타일 로그> -Pattern "EGLImage" | Measure-Object | % Count

# 타일이 각각 어느 어댑터를 잡았는지
Select-String -Path <2타일 로그> -Pattern "Selected DXGI adapter index"

# winit_wall 내장 계측 (eprintln 이라 RUST_LOG 무관, 1초마다)
Select-String -Path <로그> -Pattern "Present perf"
```

**예측**: 2타일 로그에는 `EGLImage 래핑 실패` 가 초당 수백~수천 줄, 1타일 로그에는 0 줄.
로그 파일 크기만 비교해도 드러날 것이다. 이 예측이 빗나가면 위 진단은 틀린 것이고,
그때는 `Present perf` 의 `avg_present_ms` 와 `SERVO_VIDEO_ESCAPE_PROF=1` 의
`[vesc-prof] ... present_ms=` 로 순차 present 쪽을 봐야 한다.

---

## 4. 이것이 뜻하는 것

이건 회귀가 아니라 **아직 구현되지 않은 부분**이다. CLAUDE.md 가 "Direct per-GPU swapchain
present and real multi-GPU visual verification are still pending (Phases 4–8)" 라고 적어 둔
바로 그 지점이고, 제로카피 D3D11 업로드 경로(§3-k, §3-n)는 **소비자 painter 가 하나**라는
전제 위에 설계됐다. 단일 GPU 월에서는 그 전제가 늘 참이었으므로 지금까지 드러나지 않았다.

타일 4개를 동시에 붙이려면 아래 중 하나가 필요하다(설계 결정 사항이며 여기서 고르지 않는다):

1. **링을 painter 별로 둔다** — `CONSUMER_DEVICE` 전역 1개를 `painter_id → device` 맵으로,
   `D3d11PlaneRings` 를 painter 별 인스턴스로. 업로드가 painter 수만큼 늘어난다
   (CPU memcpy × N). 타일 수가 적고 GPU 가 진짜 나뉘어 있으면 가장 정직한 방식.
2. **텍스처를 공유 가능하게 만든다** — `MiscFlags` 에 `SHARED_NTHANDLE` 을 주고 각 painter 가
   자기 디바이스로 open. 단 **DYNAMIC + CPU_ACCESS_WRITE 는 공유 텍스처로 만들 수 없다**
   (D3D11 제약) → 업로드 경로를 staging + DEFAULT 로 되돌려야 하고, 그러면 §3-n 이 없앤
   `CopySubresourceRegion` 이 부활한다. **구형 AMD 에서 GPU 점유율을 올렸던 바로 그 복사다.**
3. **cross-adapter 공유** — `D3D11_RESOURCE_MISC_SHARED_NTHANDLE` 은 어댑터를 넘지 못한다.
   진짜 다른 GPU 사이에는 cross-adapter 텍스처나 시스템 메모리 경유가 필요하다. v1 비범위로
   명시해 둔 항목이다.
4. **디코드를 painter 별로 분산** — 타일마다 자기 GPU 에서 디코드/업로드. 36 비디오를 4 GPU 로
   나누는 것이 아니라, 같은 영상을 각 GPU 가 따로 디코드하게 되므로 디코드 부하가 N 배.

★ (f) 의 순차 present 는 위와 **독립된 별개 과제**다. 위를 고쳐도 타일당 present 가 메인
스레드에서 직렬로 도는 한 `60/N` 상한은 남는다.

---

## §5. 다음 실행 — 계측 명령 (이걸로 위 후보 1·2가 갈린다)

`RUST_LOG` 만 추가하면 된다. 플래그는 지금 쓰던 것 그대로.

```powershell
$env:RUST_LOG = "warn,paint=info,servo_media_gstreamer=info,servo_media_gstreamer_render_d3d11=info"
$env:SERVO_VIDEO_ESCAPE_PROF = "1"     # escape=external 경로의 acquire/convert/present 분해

.\engine\winit_wall.exe --wall-layout config\wall_layout.multigpu.json --wall-all-tiles `
  ... (기존 --pref 전부 동일) ... `
  "file:///C:/20260812_SDWall_WallView/html/video_grid_6x6_play.html?rows=6&cols=6" 2> mg_2tile.log
```

**20~30초면 충분하다**(`paint=info` 는 타일마다 프레임당 2줄이라 로그가 빠르게 커진다).
같은 방식으로 **1타일도 한 번** 받아야 비교가 된다 — `2> mg_1tile.log`.

### ★가장 결정적인 계측 — 페이싱 요약★

`paint.rs:1402` 이 프레임 요청이 **막힐 때마다** 사유와 함께 통계를 찍는다(info, 120회마다):

```
Wall frame pacing summary: event=coalesced|released webview=.. logical_frame_id=..
  target_painters=[PainterId(1), PainterId(2)] pending_max=N
  coalesced_for_webview=N total_coalesced=N total_released=N policy=latest-first
```

`total_coalesced` 가 크면 **프레임 요청이 발행되지 못하고 뭉개지고 있다**는 뜻이고,
막힌 사유는 셋 중 하나다(`paint.rs:1259-1283`):

| 사유 | 조건 | 뜻 |
|---|---|---|
| `active_webview=..` | `webview_has_active_frame` | **직전 wall 프레임의 배리어가 아직 안 풀렸다.** 배리어는 "전 painter ready" 또는 16ms 데드라인에서만 풀린다 |
| `pending_max=N` | `pending_max >= gfx_wall_frame_max_pending`(기본 **1**) | 어떤 타깃의 미소비 프레임이 남아 있다. `max_pending` 은 타깃들의 **최댓값**으로 판정하므로 **가장 느린 타일 하나가 전체를 막는다** |
| `min_interval_webview=.. elapsed_ms=..` | `elapsed < gfx_wall_frame_min_interval_ms`(기본 **16**) | 최소 간격 미달 |

★단서★: 1타일 측정치 65fps ≈ 15.4ms 주기로, `min_interval` 16ms 상한(62.5fps)에 거의 붙어
있다. 즉 **1타일은 페이싱 하한에 걸려 돌고 있었다.** 2타일의 6.6fps ≈ 151ms 는 그 16ms 의
약 9.4배다 — "간격이 늘어난" 게 아니라 **요청이 여러 번 연속으로 막히고 있다**는 모양에 가깝다.
`total_coalesced` 증가율이 이걸 바로 확인해 준다.

### 읽는 법 — 숫자가 후보를 어떻게 가르나

```powershell
# (1) 배리어: 데드라인을 놓치고 있나?
Select-String -Path mg_2tile.log -Pattern "status=completed_after_deadline" | Measure-Object | % Count
Select-String -Path mg_2tile.log -Pattern "status=completed_before_deadline" | Measure-Object | % Count

# (2) 배리어 대기 시간: 프레임 예산을 여기서 다 쓰나?
Select-String -Path mg_2tile.log -Pattern "request_to_all_ready_ms=[0-9.]+" -AllMatches |
  % { $_.Matches.Value } | Select-Object -Last 40

# (3) painter별 실제 렌더 시간
Select-String -Path mg_2tile.log -Pattern "Wall render end.*render_ms=[0-9.]+" -AllMatches |
  % { $_.Matches.Value } | Select-Object -Last 40
```

| 관측 | 결론 |
|---|---|
| `completed_after_deadline` 이 대부분 + `request_to_all_ready_ms` 가 100ms 대 | **후보 1(배리어)**. `gfx_wall_frame_max_pending` / `gfx_wall_frame_min_interval_ms` / 데드라인이 표적 |
| `render_ms` 합(두 painter)이 프레임 간격의 대부분 | **후보 2(순차 렌더)**. 표적은 타일 병렬화 또는 렌더 비용 자체 |
| 둘 다 작은데 프레임 간격이 큼 | 병목이 **프레임 생산 상류**(script/layout/refresh driver). `gfx_vsync_enabled=false` A/B 로 페이싱 드라이버부터 배제 |

### 교란 요인 제거 A/B (레이아웃만 바꿔 각각 20초)

`virtualViewport` 를 **11520x4320 으로 고정**하고 타일 수만 1 → 2 로 바꿀 것.
뷰포트가 다르면 페이지 면적이 달라져 비교가 무의미하다.

추가로 가장 값싼 두 갈래:

```
--pref gfx_vsync_enabled=false          # DwmFlush 페이싱 배제
--pref gfx_wr_picture_tile_size=        # (빈 값) 50MB 단일 픽처 타일 배제
```

---

# ★2026-08-21 계측 실행 4건 — 원인 확정 (4타일) + 미해결 1건 (2타일)★

`RUST_LOG=warn,paint=info` 로 받은 로그 4건(`mg_2tile*.log`, UTF-16LE 이므로 변환 필요).
**pref 는 네 실행 모두 동일**하고, 기동 마커(DComp engaged / borderless fullscreen /
`wr-tile-size` override / adapter 선택)도 **전부 동일**하다.

| 실행 | 타일 | 실행시간 | 타일당 fps | `render_ms` p50 | p90 | 렌더 점유율 | 육안 |
|---|---|---|---|---|---|---|---|
| `mg_2tile` | 2 | 36s | **27.4** | 3.29 | 21.6 | 43% | 양호 |
| `mg_2tile_03` | 2 | 82s | **26.6** | 3.98 | 26.4 | 49% | 양호 |
| `mg_2tile_02` | 2 | 33s | **8.5** | 18.41 | 138.3 | 80% | 불량 |
| `mg_2tile_04` | **4** | 28s | **5.1** | 9.15 | 167.2 | 88% | 불량 |

**먼저 앞선 두 가설이 여기서도 배제된다**: 배리어는 거의 전부
`status=completed_before_deadline` (`mg_2tile` 715/0, `_03` 1604/1, `_02` 116/0, `_04` 139/0)
이고, `ready=N/N` 도 항상 만족한다. 미디어 경로도 네 실행 전부
`sync released=36`, `profile_id=` 36, `direct file playback` 36 으로 **동일**하다.
painter 별 렌더 건수도 완전히 균등하다. **시간은 `paint_target()` 안에 있다.**

## ★확정: 4타일을 막는 것은 adapter 3 하나다★

`mg_2tile_04` 를 painter 별로 가르면:

| painter | display | adapter | LUID | `render_ms` p50 | mean |
|---|---|---|---|---|---|
| 1 | 0 | 0 | `0004a009` | 14.15 | 16.10 |
| 2 | 1 | 2 | `00051266` | 6.20 | 6.84 |
| **3** | **2** | **3** | **`00043cbd`** | ★**158.25**★ | ★**139.85**★ |
| 4 | 3 | 1 | `0005754f` | 7.86 | 8.05 |

**한 GPU 가 나머지보다 20~25배 느리다.** 그리고 `render_all_tiles` 가 메인 스레드에서 타일을
순차 처리하므로(`winit_wall/main.rs:297`) 프레임 시간은 **합**이 된다:

```
14.15 + 6.20 + 158.25 + 7.86 = 186.5 ms  →  5.4 fps      (실측 5.1 fps)
```

계산이 실측과 맞는다. **배리어도 페이싱도 미디어도 아니고, 느린 어댑터 하나가 전부다.**

★중요★ — 타일 렌더를 병렬화해도 이건 안 풀린다. 병렬이면 프레임 시간이 `max`(158ms)가 되어
5.4 → 6.3 fps 로 갈 뿐이다. **느린 어댑터를 먼저 해결해야 한다.** 순차 렌더 병렬화는 그
다음에 의미가 생긴다(그때 이득은 `sum→max`).

## 미해결: 같은 2타일 설정의 양극 현상

`mg_2tile`(3.44 / 3.12) 과 `mg_2tile_02`(21.76 / 16.06) 는 **동일한 adapter 0+2, 동일 pref,
동일 기동 마커**인데 **두 painter 가 함께** 6배 느려진다. 시간 전개를 보면 점진적 열화가
아니라 **~5초 지점에 나쁜 상태로 진입해 그대로 눌러앉는다**:

```
mg_2tile    : 15.2 → 5.2 → 10.4 → 11.0 → 9.7 → 5.6 → 9.1 ms   (5초 구간별 평균)
mg_2tile_02 : 22.4 → 75.1 → 69.0 → 69.2 → 63.4 → 71.6 → 77.1 ms
```

로그에 남은 어떤 마커로도 갈리지 않는다. **원인 미상.**

★그러나 중요한 함의★ — **2타일로 27fps 는 이미 나온다.** "타일 2개면 붕괴" 라는 구조적
법칙은 없다. 남은 것은 (a) 느린 어댑터, (b) 이 양극 현상 두 가지다.

## 다음 실험 (둘 다 값싸다)

**1. adapter 3 이 단독으로도 느린가 — 이것부터.**
`display 2` 단일 타일로 20초 돌려 `render_ms` p50 을 본다.

| 결과 | 뜻 |
|---|---|
| 단독에서도 ~158ms | 그 GPU/드라이버/연결 자체가 느리다 → 하드웨어·드라이버 문제 |
| 단독에서는 빠름 | 4타일 동시일 때만 느리다 → 경합(PCIe/전력/CPU) 문제 |

같은 방법으로 display 0/1/3 도 재면 "display 0 외가 다소 느려 보인다" 는 인상이 수치로 확정된다.
어댑터 정체도 같이 확인할 것: `Get-CimInstance Win32_VideoController` 로 LUID `00043cbd` 가
어느 모델인지.

**2. 양극 현상은 렌더 내부 분해로.** `SERVO_VIDEO_ESCAPE_PROF=1` 을 켜면 초당 1회
`[vesc-prof] frames=.. converts=.. presents=.. acquire_ms=.. convert_ms=.. present_ms=..`
가 나온다. 느린 실행에서 어느 단계가 부푸는지 보면 갈린다.
(`SERVO_VIDEO_ESCAPE_PROF` 는 `debug_env.rs:199` 의 **현역 조사용 env** 다 — 기동을 막는
`SERVO_VIDEO_ESCAPE` 와는 이름이 다른 별개 노브이고, 차단 판정은 정확 일치라 충돌하지 않는다.)

## 남은 관찰

양호한 실행조차 27fps 이고 `render_ms` p90 이 21~26ms, max 는 네 실행 모두 ~400ms 다.
30fps 목표에는 이 꼬리도 결국 손봐야 한다.

---

# ★2026-08-21 (2) — "느린 어댑터" 가 아니라 "재생 시작 후 한 painter 가 눌러앉는" 현상★

## 앞 절의 결론을 정정한다

"adapter 3 이 느린 GPU" 가 아니다. **네 어댑터는 전부 같은 모델**이다
(AMD Radeon RX 580 2048SP, 사용자 확인). 그리고 painter 3 은 처음부터 느리지 않았다.

```
mg_2tile_04, 5초 구간별 평균 render_ms
  painter      0s      5s     10s     15s     20s     25s
        1   23.7   14.1   15.7   13.7   13.8   14.5
        2    8.0    6.6    6.4    6.6    6.9    6.2
        3   46.1  161.3  156.7  159.0  155.8  176.5   ← 점프 후 고정
        4    9.8    7.9    7.5    7.6    7.6    7.9
```

`mg_2tile_02`(2타일) 도 **모양이 같다**: `22.4 → 75.1 → 69.0 → 69.2 → 63.4 → 71.6 → 77.1`.
즉 4타일의 "adapter 3 문제" 와 2타일의 "양극 현상" 은 **같은 하나의 버그**이고,
실행마다 걸리는 painter 가 다를 뿐이다.

## 전이 시각 = 재생 시작 + 약 2초 = external 승격 시점

```
08:05:02   painter 3 render =   1.5 ms      (재생 전)
08:05:04   ring 36 개 생성 + Sync group released  ← 36 개 비디오 재생 시작
08:05:05                      19.8 ms
08:05:06                     201.6 ms       ← 점프
08:05:07~                150~170 ms 고정
```

`gfx_video_escape_promote_hysteresis` 기본값이 **10 프레임**(30fps 에서 ≈0.33초)이므로 이
구간은 **비디오 레이어가 external 컴포지터 서피스로 승격되는 시점**과 겹친다.

★사용자 직관("초기 재생 타이밍에 근거가 있지 않나")이 맞았다★ — 다만 **두 타일이 서로
어긋나서가 아니다.** 배리어의 `first_to_all_ready_ms` 는 네 실행 모두 p50 **0.03ms** 로,
painter 들은 사실상 동시에 ready 된다. 원인은 **재생이 실제로 시작되어 승격이 일어나는
순간부터** 특정 painter 가 비싼 경로로 눌러앉는 것이다.

## 배제된 것들 (이번 로그로 확인)

| 후보 | 판정 |
|---|---|
| 배리어 데드라인 | ✗ `completed_before_deadline` 이 715/1604/116/139 로 거의 전부. `after_deadline` 은 통틀어 1건 |
| 타일 간 위상 어긋남 | ✗ `first_to_all_ready_ms` p50 = 0.03ms |
| 미디어 폴백 | ✗ 네 실행 전부 `sync released=36`, `profile_id=` 36, `direct file playback` 36 |
| D3D11 링 churn | ✗ 링 36 개가 기동 시 1회 생성, `ring_id` 최대 36, 재생성 0 |
| painter 간 렌더 건수 불균형 | ✗ 완전 균등 (예: 987/987, 144/144/143/143) |
| 기동 경로 차이 | ✗ 네 실행 모두 DComp engaged / borderless fullscreen / tile-size override / adapter 선택 동일 |
| present 비용 | ✗ `avg_present_ms=0.00` |

**남은 것은 `paint_target()` 내부, 그것도 external 승격 이후의 경로다.**

## 다음 실험 — 각 20~30초, 4타일 고정, 한 번에 하나씩만 바꾼다

| # | 바꾸는 것 | 확인 |
|---|---|---|
| 1 | `--pref gfx_video_escape_mode=` (빈 값) | ★최우선★ 승격이 원인인지. painter 3 의 158ms 가 사라지면 확정 |
| 2 | `--pref gfx_wr_picture_tile_size=` (빈 값) | 50MB 단일 픽처 타일(painter 당) 배제 |
| 3 | `--pref gfx_video_escape_promote_hysteresis=100000` | 승격을 사실상 무한 연기 — #1 의 교차 검증(코드 경로가 다르므로 둘 다 봐야 한다) |
| 4 | `--wall-tile-index 2` 단일 타일 | display 2 를 혼자 돌려 `render_ms` p50 측정. 혼자서도 158ms 면 그 GPU/연결, 빠르면 경합 |

전 실행 공통: `$env:RUST_LOG="warn,paint=info"`, `$env:SERVO_VIDEO_ESCAPE_PROF="1"`.
`[vesc-prof] ... acquire_ms= convert_ms= present_ms=` 이 나오면 승격 경로 안에서 어느 단계가
부푸는지까지 바로 갈린다(이번 로그 4건에는 이 env 가 꺼져 있어 0 건이었다).

**판독 한 줄**: `grep "PainterId(3)" ... | grep -o "render_ms=[0-9.]*"` 의 p50 이
150ms 대에서 10ms 대로 내려오는 조건을 찾으면 그것이 원인이다.

---

# ★2026-08-21 (3) — 원인 확정: 월 프레임 페이싱 게이트★

4타일 실험 3건(`mg_4tile_*.log`)으로 인과가 닫혔다.

| 실행 | escape | tile_size | hyst | fps | painter p50 (1/2/3/4) |
|---|---|---|---|---|---|
| `mg_2tile_04` (기준) | external | display | 10 | 5.1 | 14.2 / 6.2 / **158.3** / 7.9 |
| `mg_4tile_video_escape_off` | **off** | display | — | 5.3 | 6.5 / 13.4 / 11.8 / 11.7 |
| `mg_4tile_default_tile_size` | external | **기본** | 10 | **3.3** | 18.1 / 8.6 / 9.7 / 17.8 (p90 312/320) |
| `mg_4tile_inf_promote_hysteresis` | external | 5760x2160 | **10^6** | 5.2 | 6.3 / 13.2 / 11.3 / 11.8 |

## 결론 1 — 158ms 이상치의 원인은 external 승격 (확정)

escape 를 끄든(#1) 승격을 무한 연기하든(#3) 이상치가 사라지고 네 painter 가 6~14ms 로
균등해진다. **서로 다른 코드 경로 두 갈래가 같은 결과를 내므로 교차 검증이 성립한다.**

## 결론 2 — ★그런데 렌더는 병목이 아니었다★

프레임당 렌더 합이 `186ms → 43ms` 로 **4.3배** 줄었는데 **fps 는 5.1 → 5.3 으로 그대로**다.
프레임 주기 ~190ms 중 렌더는 43ms 뿐 — **나머지 ~147ms 는 렌더 밖에서 쓰인다.**

## 결론 3 — 그 147ms 는 페이싱 게이트다 (확정)

```
mg_4tile_video_escape_off        total_coalesced=1200  total_released=191   →  6.3 : 1
mg_4tile_inf_promote_hysteresis  total_coalesced= 960  total_released=146   →  6.6 : 1
mg_4tile_default_tile_size       total_coalesced=1920  total_released=104   → 18.5 : 1
```

**프레임 요청의 6~18배가 발행되지 못하고 뭉개진다.** 그리고 coalesced 이벤트의
`pending_max` 는 사실상 전부 **1** 이다(17건 중 17건 / 15건 중 14건 / 22건 중 21건).

`components/paint/paint.rs:1245-1268`:

```rust
fn wall_frame_request_pacing_block_reason(&self, request: &WallFrameRequest)
    -> Option<WallFramePacingBlockReason>
{
    if !self.wall_frame_pacing_config.enabled()
        || request.target_painter_ids.len() <= 1     // ← ★타일 1개면 게이트 자체가 통째로 우회★
        || request.pacing_key().is_none()
    { return None; }

    // ... webview_has_active_frame → Active

    let pending_max = self.max_pending_frames_for_targets(&request.target_painter_ids);
    if pending_max >= self.wall_frame_pacing_config.max_pending {   // 1 >= 1  → 항상 참
        return Some(WallFramePacingBlockReason::Pending(pending_max));
    }
    // ... min_interval → TooSoon
}
```

`gfx_wall_frame_max_pending` **기본값은 1** 이다(`prefs.rs:690`). 따라서 어느 타깃이든
미소비 프레임이 **1장만 있어도** 다음 wall 프레임 요청이 막힌다. 게다가 판정이
`max_pending_frames_for_targets` = **타깃들의 최댓값**이라 **가장 늦게 소비되는 painter
하나가 전체를 막는다**. 타일은 메인 스레드에서 순차 소비되므로 마지막 타일은 항상 늦다.

## ★이것이 1타일 65fps ↔ 다타일 붕괴의 불연속을 설명한다★

`target_painter_ids.len() <= 1` 이면 **게이트가 통째로 우회**된다. 즉

- **1타일**: 페이싱 판정 자체가 없음 → 65fps
- **2타일 이상**: 게이트 발동 → `pending_max=1 >= max_pending=1` → 거의 매 요청이 coalesce

점진적 비용 증가가 아니라 **타깃 2개에서 스위치가 켜지는 구조**다. 앞서 "초선형 붕괴" 로
보였던 것의 정체가 이것이다.

## 다음 A/B — pref 하나씩, 4타일 고정

| # | pref | 기대 |
|---|---|---|
| 1 | `--pref gfx_wall_frame_max_pending=3` | 게이트를 연다. **최우선** |
| 2 | `--pref gfx_wall_frame_pacing_enabled=false` | 게이트를 통째로 끈다(1타일과 같은 상태). ★이름과 달리 기본이 `true` 다★ |
| 3 | `--pref gfx_wall_frame_min_interval_ms=1` | #1 로 부족하면 다음에 걸리는 것이 16ms 간격(=62.5fps 상한)이다 |

**예측 산수** — escape off + 페이싱 개방이면 프레임당 렌더 합 43ms 가 상한이 되어
**~23fps** 까지 올라와야 한다(순차 렌더 유지 시). 그보다 낮으면 다른 게이트가 남아 있다는 뜻이고,
23fps 근처면 그 다음 표적은 **타일 순차 렌더의 병렬화**(`sum → max`, 43ms → 14ms ≈ 70fps)다.

★순서 주의★ — escape 를 켠 채 페이싱만 열면 158ms 이상치가 다시 상한이 되어 5.4fps 에 묶인다.
**페이싱(결론 3)과 승격 이상치(결론 1)는 둘 다 풀어야 4타일이 성립한다.**

★재검토 필요★ — "escape=external 이 있어야 A 대조군과 성능이 맞는다" 는 앞선 관찰은
**페이싱 게이트가 모든 것을 ~5fps 로 묶어 둔 상태에서 측정된 것**이다. 게이트를 연 뒤에는
그 비교를 다시 해야 한다.

## 덤: WR 기본 타일 크기는 이 워크로드에서 더 나쁘다

`mg_4tile_default_tile_size`(tile_size 미지정)는 fps 3.3 으로 가장 낮고 p90 이 312/320ms 로
튄다. `gfx_wr_picture_tile_size=display` 는 유지하는 편이 맞다.

---

# ★2026-08-21 (4) — 페이싱은 2배뿐이었다. 진짜 상한은 "생산 단계"★

`mg_4tile_frame_*.log` 4건. 사용자 관찰("개선 없었다")이 맞다.

| pref | 월 fps(=가장 느린 타일) | painter 별 fps | 타일 일관성 |
|---|---|---|---|
| (기준) | 5.1 | 균등 | 유지 |
| `min_interval_ms=1000` | 2.0 | 균등 | 유지 |
| `min_interval_ms=1` | 5.6 | 균등 | 유지 |
| **`max_pending=30`** | **9.9** | 9.9 / 9.9 / 9.9 / 9.9 | 유지 |
| `pacing_enabled=false` | **6.9** | 32.3 / 20.7 / 32.3 / **6.9** | ★**깨짐**★ |

## ★정정★ — `pacing_enabled=false` 의 32fps 는 painter 1 만의 수치다

painter 별로 32.3 / 20.7 / 32.3 / **6.9** fps 로 갈린다. 월의 속도는 **가장 느린 타일**이
정하므로 실제로는 6.9fps 이고, 기준(5.1)에서 사실상 개선이 없다. 게다가 배리어가 기대하는
타깃 수가 **2/3/4 로 뒤섞인다**(`ready=2/2` 410건, `3/3` 492건, `4/4` 256건) — 즉
**프레임이 타일 일부에만 나가 월이 분리된다.** `max_pending=30` 은 361건 전부 `4/4` 로
일관성을 유지한다.

**→ `gfx_wall_frame_pacing_enabled=false` 는 해법이 아니다.** 렌더 카운트만 올리고 월을 깬다.

## 유효한 것은 `max_pending` 뿐이고, 그것도 2배

`5.1 → 9.9 fps`, 네 타일 균등, 일관성 유지. 페이싱 게이트가 실제 병목이었던 것은 맞지만
**전체의 절반**이었다.

## 남은 상한 = 생산 단계 ~73ms

`max_pending=30` 실행의 프레임 예산:

```
프레임 주기              101 ms   (9.9 fps)
  순차 GL 렌더 합(p50)    27.5 ms
  first_to_all_ready_ms    0.05 ms
  final_wait_ms            0.64 ms
  ────────────────────────────────
  설명 안 되는 구간       ~73 ms
```

`max_pending=30` 으로도 `total_coalesced=1200 / total_released=123` (10:1) 이 유지된다.
남은 차단 사유는 `min_interval`(16ms = 62.5fps 상한이라 10fps 를 설명 못 한다)이 아니라
**`Active`** — `webview_has_active_frame`, 즉 **직전 배리어가 아직 안 풀려서**다.
배리어는 요청 시점부터 전 painter ready 까지 열려 있고, `first_to_all_ready_ms` 가 0.05ms
이므로 **요청 → 첫 ready 사이(생산 단계)가 곧 그 73ms** 다.

## 산수가 가리키는 곳: 씬을 타일 수만큼 중복 빌드한다

- 1타일 = 같은 가상 뷰포트(11520x4320) · 같은 36 비디오 = **65fps(15ms 주기)**
- 4타일 = 생산 단계 **~73ms** ≈ 15ms × 4.9

이 포크의 팬아웃은 디스플레이 리스트를 **모든 painter 에게 브로드캐스트**하고
각 painter 가 **전체 씬을 자기 몫으로 다시 빌드**한다(CLAUDE.md: paint.rs 가 display-list /
frame-tree / scroll / viewport / GenerateFrame 을 every registered painter 에 broadcast).
즉 씬 빌드 비용은 **타일 수로 나뉘지 않고 곱해진다.** 관측된 4.9배가 이 구조와 맞는다.

병렬성 관점: painter 마다 WR 백엔드 스레드가 따로 있어 이론상 병렬이고,
`first_to_all_ready_ms`=0.05ms 는 네 painter 가 거의 동시에 끝난다는 뜻이다. 그런데도 벽시계가
4.9배라는 것은 **병렬 실행이 자원 경합으로 직렬화되고 있다**는 뜻이다
(36 개 avdec 스레드 + WR 백엔드 4 + 메인 스레드).

## 다음 측정 3개 — 여기서부터는 추측하지 말 것

1. **`max_pending=30` + `min_interval_ms=1` 동시** — 아직 함께 시험하지 않았다. 두 게이트를
   동시에 열되 `Active`(=일관성)는 유지된다.
2. **생산 단계를 직접 잰다**: `RUST_LOG=warn,paint=debug`.
   `Wall frame barrier progress: ... painter={:?} ... wait_ms={:.3} missing={:?}`(`paint.rs:646`)
   가 **painter 별로 요청 후 몇 ms 에 ready 됐는지**를 찍는다. 10fps × 4 painter 면 20초에
   ~800 줄이라 감당 가능하다. 이 수치가 73ms 를 어떻게 나눠 갖는지가 다음 표적을 정한다.
3. **CPU 포화 여부**: 4타일 실행 중 코어 수 대비 CPU 사용률. 포화라면 "씬 4중 빌드 + 36 디코더"
   가 벽이고, 표적은 (a) 씬 빌드 공유, (b) 디코드 부하 축소(하드웨어 디코드), (c) 타일 수만큼
   코어 확보 중 하나가 된다. 포화가 아니라면 경합 지점이 CPU 가 아니라 다른 곳이다.

★주의★ — 158ms external 승격 이상치(앞 절 결론 1)는 여전히 별개로 남아 있다.
`max_pending` 을 열어도 escape 를 켜면 그 이상치가 다시 상한이 된다.

---

# ★2026-08-21 (5) — 최종: 페인트 스레드가 미디어 이미지 팬아웃으로 포화된다★

`mg_4tile_frame_min_interval_1ms_max_pending_30.log`(`paint=debug`, 26MB)로 확정.

## 두 게이트를 함께 열어도 나아지지 않는다

`max_pending=30` + `min_interval_ms=1` 동시 → **5.2 fps**(네 타일 균등, 일관성 유지).
`max_pending=30` 단독(9.9)보다 오히려 낮다. **페이싱은 원인이 아니었다.**

## 생산 단계 가설도 반증됐다

`paint=debug` 가 찍는 `Wall frame barrier progress ... wait_ms=`(요청 → 해당 painter ready):

```
PainterId 1: p50=3.79   PainterId 2: p50=2.90
PainterId 3: p50=2.99   PainterId 4: p50=2.71     (ms)
```

**~3ms 다. 73ms 가 아니다.** 프레임 예산:

```
프레임 주기          192 ms   (5.2 fps)
  생산(wait_ms)       ~3 ms
  순차 GL 렌더 합     ~42 ms
  ──────────────────────────
  설명 안 되는 구간  ~147 ms  ← 배리어 완료 후 임베더가 그리러 오기까지
```

## 진짜 원인

`paint=debug` 의 라인 구성(38초):

| 라인 | 건수 |
|---|---|
| **`Wall media image fanout`** | ★**37,433**★ (= **초당 ~1,120**) |
| `Wall logical frame N barrier skipped: generated_targets=[]` | 1,595 |
| `Wall frame pacing coalesced` | 469 |
| `Wall frame barrier progress` | 360 |
| `Wall frame pacing released` | 116 |

**초당 1,120 = 36 비디오 × ~31fps.** 즉 **비디오 프레임이 도착할 때마다** 팬아웃이 돌고,
`paint.rs:1773-1788` 이 그것을 **모든 target painter** 로 밀어넣는다.

월은 5.2fps 로 합성하는데 페인트 스레드는 **초당 1,120번** 팬아웃 작업을 계속한다.
화면에 닿는 것은 초당 5.2건뿐 — 나머지는 버려진다.

부하 산수(팬아웃 1건당 페인트 스레드 비용 X):

```
1타일 : 1,120 × X × 1  ≈ 스레드의 25%   → 여유      → 65 fps
4타일 : 1,120 × X × 4  ≈ 스레드의 100%  → 포화      →  5 fps
```

★**"CPU 점유율이 한 번도 상한에 도달하지 않았다" 와 정확히 맞는다**★ — 포화된 것은
머신이 아니라 **스레드 하나**다. 총 CPU 는 낮게 유지된다.

`generate_frame_for_script` 가 1,595회 전 painter 에서 false 를 반환한 것
(`generated_targets=[]`)도 같은 그림이다: `renderer_behind()` =
`display_composite_in_flight` — **임베더가 직전 프레임을 아직 소비하지 못했다.**
임베더가 못 오는 이유가 위의 페인트 스레드 포화다.

## 이 프로젝트가 이미 만난 적 있는 함정이다

초기 기록: *"36개 비디오 × 30fps ≈ 1080 image-update/s 가 `painter.rs update_images` 의
per-arrival 즉시합성 경로를 타면서 winit 이벤트 루프가 압도됨"*. 당시 해법은 페이지의
rAF 하트비트였다. **월 팬아웃은 그 부하를 painter 수만큼 곱한다** — 그래서 1타일에서는
멀쩡하고 4타일에서 무너진다.

## 다음 — 확인 1개, 방향 3개

**확인(가장 값싸고 결정적)**: 4타일 유지, **비디오 수만 줄여서** 재생
(`?rows=3&cols=3` = 9개). 팬아웃이 병목이면 초당 팬아웃이 1,120 → 280 으로 줄어
**fps 가 ~4배 올라야 한다.** 안 오르면 이 진단도 틀린 것이다.

보조 확인: 실행 중 **스레드별** CPU(전체가 아니라). 페인트 스레드가 코어 1개를 100%
쓰고 있어야 한다.

**방향(확인된 뒤에 고를 것)**:
1. **팬아웃 코얼레싱** — 합성 주기당 1회로 묶는다. 지금은 도착마다 × painter 수.
   합성이 5fps 인데 30fps 로 밀어넣는 것은 6배 낭비다.
2. **`gfx_video_decouple_enabled` / `present_external_only` 경로 재검토** — §3-ae 가
   *"비디오당 generate_frame 전체 씬 빌드 플러드"* 를 정확히 이 이유로 만든 우회로다.
   월 팬아웃 경로에 같은 분리가 적용되는지 확인해야 한다.
3. **소스 프레임레이트/개수 축소** — 운용 타협.

★앞선 결론 중 살아남는 것★: external 승격의 158ms 이상치(결론 1)는 여전히 별개 문제로 남는다.
페이싱(결론 3)은 2배짜리 부차 요인으로 격하된다.

---

# ★2026-08-21 (6) — 3x3 검증: 팬아웃은 절반. 나머지는 vsync 드라이버 의심★

`mg_4tile_frame_min_interval_1ms_max_pending_30_3x3.log` (4타일, 비디오만 36 → 9).

| | 36 비디오 | 9 비디오 | 변화 |
|---|---|---|---|
| fanout/s | 1,120 | **249** | 4.5배 ↓ (예측대로) |
| **fps** | 5.2 | **12.1** | ★**2.3배** ↑ (예측 ~4배)★ |
| 렌더 합 p50 | 42 ms | 11.2 ms | 3.8배 ↓ |
| `wait_ms` p50 | 3.0 ms | 0.29 ms | 10배 ↓ |
| `barrier skipped` | 1,595 | **1,561** | ★거의 불변★ |

**판정: 부분 확인.** 팬아웃은 실재하는 비용이 맞지만(줄이니 2.3배) **전부는 아니다.**

```
9 비디오 프레임 예산
  프레임 주기       82.6 ms  (12.1 fps)
    생산(wait_ms)    0.29 ms
    순차 GL 렌더 합  11.2 ms
    ────────────────────────
    설명 안 되는 구간 ~71 ms   ← 여전히 남음
```

## 새 사실: 월 프레임 시도율은 비디오 수와 무관하게 초당 ~50회다

```
36 비디오: (1595 skipped + 196 성공) / 38s = 47 회/s
 9 비디오: (1561 skipped + 473 성공) / 39s = 52 회/s
```

리프레시 드라이버는 계속 요청하는데 대부분이 `renderer_behind()`
(= `display_composite_in_flight`, 임베더가 직전 프레임을 아직 안 가져감)로 버려진다.
**71ms 는 임베더가 돌아오지 않는 시간**이고, 그 경로는
`notify_new_frame_ready` → `tile[0].window.request_redraw()` → winit → `RedrawRequested`
→ `render_all_tiles` 다. 즉 **메인 스레드/이벤트 루프**다.

## ★한 번도 시험하지 않은 변수: `gfx_vsync_enabled`★

`components/servo/examples/winit_wall/vsync_refresh_driver.rs:14-16` 원문:

> Opt-in on Windows desktop via the `gfx_vsync_enabled` pref. **It is not the default
> because under heavy compositor load (many simultaneous videos) it degrades worse than
> the free-running timer.**

**소스가 이 워크로드에서 해롭다고 명시한 노브다.** 그런데 escape 실험 이후 모든 실행에
`gfx_vsync_enabled=true` 가 켜져 있었다(이 조사 맨 처음 명령에는 `=false` 였다).
**4타일에서 끄고 재 본 적이 한 번도 없다.**

## 다음 두 실험 (각 20~30초, 4타일 고정)

**A. vsync 끄기** — `--pref gfx_vsync_enabled=false`, 나머지 그대로(6x6 36 비디오).
기대: 71ms 공백이 줄면 이것이 원인. 안 줄면 이벤트 루프 쪽 다른 요인.

**B. 비디오-무관 상한 재기** — 4타일, `?rows=1&cols=1`(비디오 1개).
팬아웃이 사실상 0 인 상태의 fps 가 **타일 4개 자체의 구조적 상한**이다.

| B 결과 | 뜻 |
|---|---|
| ~60fps | 비용은 전부 비디오/팬아웃 계열 → 코얼레싱이 표적 |
| ~12fps | 비디오와 무관한 **타일당 고정 오버헤드**가 있다 → 표적이 완전히 바뀐다 |

두 실험이 문제를 위아래로 가둔다. A 는 원인 후보 하나를 직접 끄고, B 는 도달 가능한
천장을 확정한다.

---

# ★기준선 정의 (BASE-4T) — 앞으로 "그대로" 는 이것을 뜻한다★

지금까지 실험마다 pref 가 달라졌고 그것을 명시하지 않아 혼란이 있었다. 각 로그의
`servo: config:` 덤프(기본값과 다른 것만 찍힌다)로 전량 재구성한 결과가 아래다.

## 모든 4타일 실행에 공통이었던 값 (= BASE-4T)

```
gfx_dcomp_mode=surface
gfx_refresh_hz=60
gfx_vsync_enabled=true          ← ★예외 없이 전 실행에 켜져 있었다★
gfx_wr_picture_tile_size=display
media_d3d11_enabled=true
media_direct_file_enabled=true
media_gapless_loop_enabled=true
media_avdec_max_threads=1
media_sync_group_target=<비디오 수>
+ dom_* 5 개, --ignore-certificate-errors, --wall-all-tiles, 4타일 layout
```

**`gfx_video_escape_mode` 는 BASE-4T 에 포함하지 않는다**(= 끈다). 켜면 external 승격의
158ms 이상치가 다른 모든 것을 덮어 단일 변수 실험이 성립하지 않기 때문이다.
그 이상치는 **별개 과제로 남아 있고**, 이 트랙이 끝난 뒤 다시 켜서 따로 풀어야 한다.

## 지금까지 실행 = BASE-4T ± 무엇이었나 (사후 재구성)

| 로그 | BASE-4T 대비 차이 | 월 fps |
|---|---|---|
| `mg_4tile_video_escape_off` | **없음 (= BASE-4T 그 자체)** | **5.3** |
| `mg_2tile_04` | `+ gfx_video_escape_mode=external` | 5.1 |
| `mg_4tile_default_tile_size` | `+ escape`, `− gfx_wr_picture_tile_size` | 3.3 |
| `mg_4tile_inf_promote_hysteresis_timing` | `+ escape`, `+ hysteresis=1000000`, tile_size 를 `5760x2160`(값은 display 와 동일) | 5.2 |
| `mg_4tile_frame_max_pending_30` | `+ gfx_wall_frame_max_pending=30` | **9.9** |
| `mg_4tile_frame_min_interval_1ms` | `+ gfx_wall_frame_min_interval_ms=1` | 5.6 |
| `mg_4tile_frame_min_interval_1000ms` | `+ gfx_wall_frame_min_interval_ms=1000` | 2.0 |
| `mg_4tile_frame_pacing_disabled` | `+ gfx_wall_frame_pacing_enabled=false` | 6.9 (월 분리) |
| `..._min_interval_1ms_max_pending_30` | `+ max_pending=30`, `+ min_interval_ms=1` | 5.2 |
| `..._max_pending_30_3x3` | 위 + **비디오 36→9**(`sync_group_target=9`) | 12.1 |

★따라서 페이싱·팬아웃 관련 비교는 전부 **escape 가 꺼진 상태**에서 이루어졌다 —
내부적으로 일관되며, 기준값은 `mg_4tile_video_escape_off` 의 **5.3 fps** 다.★

## BASE-4T 실행 명령 (문자 그대로)

```powershell
$env:RUST_LOG = "warn,paint=info"          # 배리어/렌더 계측. 생산단계까지 보려면 paint=debug
Remove-Item Env:\SERVO_VIDEO_ESCAPE_PROF -ErrorAction SilentlyContinue

.\engine\winit_wall.exe --wall-layout config\wall_layout.multigpu.json --wall-all-tiles `
  --ignore-certificate-errors `
  --pref devtools_server_enabled=true `
  --pref devtools_server_listen_address=0.0.0.0:7000 `
  --pref dom_image_extended_formats_enabled=true `
  --pref dom_video_extended_containers_enabled=true `
  --pref dom_video_network_uri_enabled=true `
  --pref dom_webrtc_enabled=true `
  --pref dom_screen_capture_enabled=true `
  --pref gfx_dcomp_mode=surface `
  --pref gfx_vsync_enabled=true `
  --pref gfx_refresh_hz=60 `
  --pref gfx_wr_picture_tile_size=display `
  --pref media_d3d11_enabled=true `
  --pref media_direct_file_enabled=true `
  --pref media_gapless_loop_enabled=true `
  --pref media_avdec_max_threads=1 `
  --pref media_sync_group_target=36 `
  "file:///C:/20260812_SDWall_WallView/html/video_grid_6x6_play.html?rows=6&cols=6" 2> base4t.log
```

★`gfx_video_escape_mode` 가 없다는 것이 BASE-4T 의 정의다★ — 넣지 말 것.
그리고 **`media_sync_group_target` 은 항상 실제 비디오 수와 같게** 유지한다(3x3 이면 9).

## 다음 두 실험 = BASE-4T 에서 딱 한 줄씩

**A. vsync 끄기** — ★단 한 번도 시험하지 않은 값★

```
BASE-4T 에서:  --pref gfx_vsync_enabled=true   →   --pref gfx_vsync_enabled=false
```

근거: `winit_wall/vsync_refresh_driver.rs:14-16` 이 *"It is not the default because under
heavy compositor load (many simultaneous videos) it degrades worse than the free-running
timer"* 라고 명시한다. 지금이 정확히 그 조건이고, 전 실행에 켜져 있었다.

**B. 비디오-무관 상한** — 비디오만 1개로

```
BASE-4T 에서:  --pref media_sync_group_target=36  →  =1     (2 미만이면 동기그룹 비활성)
               URL 쿼리        ?rows=6&cols=6     →  ?rows=1&cols=1
```

| B 결과 | 뜻 |
|---|---|
| ~60fps | 비용은 전부 비디오/팬아웃 계열 → 코얼레싱이 표적 |
| ~12fps | 비디오와 무관한 **타일당 고정 오버헤드** → 표적이 바뀐다 |

**비교 대상은 BASE-4T 의 5.3 fps 다**(9.9 나 5.2 가 아니다 — 그것들은 페이싱 pref 가
들어간 변형이다). A 와 B 는 각각 BASE-4T 와만 비교하고, 서로 섞지 않는다.

---

# ★2026-08-21 (7) — vsync 드라이버가 지배적이었다 (BASE-4T 대비 3배)★

두 실험 모두 BASE-4T(5.3 fps)에서 한 줄씩만 바꾼 것이다.

| 실행 | 변경 | 월 fps | 렌더 합 p50 | `wait_ms` p50 | fanout/s |
|---|---|---|---|---|---|
| BASE-4T | — | 5.3 | 43.3 ms | — | (info 라 미측정) |
| **B `mg_4tile_1x1`** | `sync_group=1` + 비디오 1개 | **12.4** | 11.2 ms | 0.23 ms | 28 |
| **A `mg_4tile_vsync_disabled`** | `gfx_vsync_enabled=false` | ★**15.8**★ | 26.9 ms | 1.38 ms | 995 |

## 결론

**A 가 지금까지 최대 단일 개선이다.** 비디오 36 개를 그대로 둔 채 pref 한 줄로 5.3 → 15.8(3배).
그리고 **vsync 를 끈 36 비디오(15.8)가 vsync 를 켠 1 비디오(12.4)보다 빠르다** — 즉
비디오 수보다 **vsync 리프레시 드라이버가 지배적**이었다.

소스가 이미 경고하고 있던 그대로다(`vsync_refresh_driver.rs:14-16`):
*"not the default because under heavy compositor load (many simultaneous videos) it
degrades worse than the free-running timer."* 이 조사 내내 켜져 있었던 것이 문제였다.

★B 의 해석 정정★ — 앞 절에서 "B 가 ~12fps 면 비디오 무관 타일당 고정 오버헤드" 라고 썼는데,
실제로 12.4 가 나왔고 **그 오버헤드의 정체가 대부분 vsync 드라이버였다.** 타일 고유의
구조적 한계가 아니었다.

## 남은 예산 (vsync off 기준)

```
프레임 주기        63.3 ms  (15.8 fps)
  생산(wait_ms)     1.38 ms
  순차 GL 렌더 합   26.9 ms
  ────────────────────────
  설명 안 되는 구간  ~35 ms
페이싱: coalesced 2640 / released 105 = 25:1   ← 여전히 대부분 막힌다
```

## 다음 — 아직 시험하지 않은 조합

★`gfx_wall_frame_max_pending=30` 은 **vsync 가 켜진 상태에서만** 시험했다★(9.9 fps).
vsync 를 끈 상태와 결합한 적이 없다. 페이싱이 여전히 25:1 로 막고 있으므로 이것이 다음 수다.

```
BASE-4T 에서 두 줄:
  --pref gfx_vsync_enabled=false
  --pref gfx_wall_frame_max_pending=30
(부족하면 세 번째로 --pref gfx_wall_frame_min_interval_ms=1)
```

**도달 가능한 천장**: 렌더 합이 26.9ms 이므로 순차 렌더를 유지하는 한 상한은 **~37fps** 다.
즉 **30fps 목표는 이 경로 안에서 사정권**이다. 그 이상이 필요하면 타일 병렬 렌더
(`sum → max`, 26.9 → 8.3ms ≈ 120fps)가 다음 표적이 된다.

★별개로 남아 있는 것★ — external 승격의 158ms 이상치. BASE-4T 는 escape 를 끈 상태이므로
이 트랙이 끝나면 `gfx_video_escape_mode=external` 을 다시 켜고 그 이상치를 따로 풀어야 한다.

---

# ★2026-08-21 (8) — 페이싱을 열면 오히려 나빠진다 + primary GPU 편중의 정체★

## 조합 실험 (파일명과 달리 두 실행 모두 vsync **off** 다 — config 덤프에 `gfx_vsync_enabled` 없음)

| 설정 (BASE-4T 기준) | 월 fps | 렌더 합 p50 | `barrier skipped` |
|---|---|---|---|
| `+ gfx_vsync_enabled=false` **단독** | ★**15.8**★ | 26.9 ms | 120 |
| `+ vsync=false, max_pending=30` | **9.0** | 37.2 ms | **1,629** |
| `+ vsync=false, max_pending=30, min_interval=1` | 10.3 | 33.5 ms | **3,444** |

★**페이싱을 열면 나빠진다.**★ `barrier skipped`(= `generated_targets=[]`)가 120 → 1,629 →
3,444 로 폭증한다. 요청은 더 통과하지만 `generate_frame_for_script` 가
`renderer_behind()`(= 임베더가 직전 프레임 미소비)로 전부 거절하고, 그 헛작업이 실제 작업
시간을 먹는다. `painter.rs:1460-1465` 주석이 예고한 그대로다 —
*"In the healthy steady state requests and renders alternate 1:1 on this thread, so this
gate never throttles."* `max_pending=1` 이 자기조절 장치였다.

**→ 현재 최선 = BASE-4T + `gfx_vsync_enabled=false` 단독 = 15.8 fps.**
(앞서 `max_pending=30` 이 5.3 → 9.9 로 좋았던 것은 **vsync 가 켜져 있을 때** 얘기다.
vsync 를 끄면 그 노브는 해롭다. 두 노브를 함께 쓰지 말 것.)

## primary GPU 만 점유율이 2배인 이유

미디어 이미지 팬아웃은 범인이 아니다 — `paint.rs:1810-1813` 이
`painter.update_images(updates.clone())` 를 **네 painter 에 동일하게** 돌린다.
로그의 `source_painter=PainterId(1)`(39,420 건 전량)은 이미지 키의 네임스페이스일 뿐
GPU 작업량 편중이 아니다.

편중을 만드는 지점은 따로 있다 — **D3D11 비디오 업로드/변환 경로가 GPU 하나에 고정된다**:

```
components/media/player/d3d11_ring.rs
    static CONSUMER_DEVICE: Mutex<Option<usize>> = Mutex::new(None);   // ★프로세스 전역 1개★

components/media/backends/gstreamer/render-d3d11/lib.rs:173
    let device = D3d11PlaneRings::consumer_device() ...
    ring_producer::create_plane_textures(device, ...)   // 36 비디오 × plane 텍스처 전부 이 디바이스에
```

즉 **36 개 비디오의 plane 텍스처 생성·CPU 업로드·YUV→RGBA 변환이 전부 단 하나의 GPU 에서**
일어난다. 그 GPU 는 *자기 타일의 합성* + *월 전체의 비디오 전처리* 를 겸하고, 나머지 셋은
자기 타일 합성만 한다. **관측된 "한 GPU 만 2배" 와 정확히 맞는 모양이다.**

★어느 GPU 인가는 등록 순서가 정한다★ — `MediaExternalImages::new` 가
`set_consumer_device(device)` 를 부르는데(`media-thread/lib.rs:333`), 이 함수는
`Painter::new` 마다 호출되고 **1 회 가드가 없다**(`initialize_image_handler` 진입부 확인).
따라서 **마지막에 생성된 painter 의 디바이스가 최종 승자**다.

**확인 방법** — 다음 실행에서 미디어 타깃까지 로그를 켠다:

```
$env:RUST_LOG = "warn,paint=info,servo_media_gstreamer=info,servo_media_gstreamer_render_d3d11=info"
```

`D3D11 video: plane 링 생성 ring_id=N ...`(36 건) 이 나오고, 점유율이 높은 GPU 가
**마지막 타일(tile 3 / display 3 / adapter 1)** 의 GPU 인지 확인한다.
지금까지 실행은 `paint=debug` 만이라 이 라인들이 전부 필터돼 있었다(`ring_id=` 0 건).

**만약 점유율이 높은 GPU 가 display 0(adapter 0) 라면** 이 설명은 맞지 않는다 —
그때는 등록 순서가 예상과 다르거나 다른 요인이다. ★"primary display" 가 이 장비에서 어느
display 인지 먼저 확정해야 한다★ — 토폴로지상 `\.\DISPLAY1` 은 **display 2 / adapter 3** 이고,
display 0 은 `\.\DISPLAY8` 이다. 이름만으로는 primary 를 알 수 없다.

## 다음

1. 위 RUST_LOG 로 한 번 더 돌려 **링이 어느 GPU 에 생성되는지** 확정.
2. 확정되면 표적은 명확하다: **비디오 업로드/변환을 painter 별로 분산**하거나,
   최소한 **월 전체 부하를 한 GPU 에 몰지 않도록** 소비자 디바이스를 나눈다.
   (앞서 배제됐던 "전역 CONSUMER_DEVICE" 구조가 **성능 편중 요인으로는 살아 있다** —
   당시 반증된 것은 "그것이 fps 붕괴의 원인" 이라는 가설이었다.)

## primary = display 0 / adapter 0 (사용자 확인) → CONSUMER_DEVICE 설명은 탈락

`set_consumer_device` 는 `Painter::new` 마다 불리고 1 회 가드가 없으므로 **마지막** painter
(tile 3 / adapter 1)가 이긴다. 관측된 편중은 **첫** painter(adapter 0)이므로 이 설명은 맞지 않는다.

### 배제된 것 (코드로 확인)

- **팬아웃 아님** — `paint.rs:1810-1813` 이 `painter.update_images(updates.clone())` 를
  네 painter 에 **동일하게** 돌린다. `source_painter=PainterId(1)` 은 이미지 키 네임스페이스일 뿐.
- **렌더 경로 아님** — tile 0 은 `webview.paint()`, 나머지는 `paint_target()` 이지만
  둘 다 `painter.render()` 로 수렴한다(`paint.rs:2168-2186`). 게다가 painter 1 의
  `render_ms` 는 **가장 낮다**(vsync off 기준 3.4ms).

→ 추가 부하는 **per-tile 렌더 경로 밖**, 즉 미디어/비디오 D3D11 쪽이다.

### 유력 후보: GStreamer D3D11 디바이스가 adapter 0 에 고정

이 저장소의 실행 스크립트가 이미 적어 둔 알려진 미해결 항목이다
(`etc/multigpu/run_video_wall_d3d11.ps1:103`):

> Multi-GPU caveat: **the gst D3D11 device is created on adapter 0** while the renderer
> (ANGLE) picks its own adapter. On a multi-GPU box a mismatch makes shared-handle import
> fail ... **adapter affinity is the planned follow-up** (spec section 4.5).
> Single-GPU machines are unaffected.

즉 **36 개 비디오의 GStreamer D3D11 디코드/업로드/변환이 전부 adapter 0 에서** 돌고,
그 위에 adapter 0 은 자기 타일 합성까지 한다. 나머지 셋은 자기 타일 합성만 한다.
**primary=adapter 0 이라는 관측과 정확히 맞는다.**

### 확정용 실행 (미디어 로그 타깃을 켜야 보인다)

```
$env:RUST_LOG = "warn,paint=info,servo_media_gstreamer=info,servo_media_gstreamer_render_d3d11=info"
```

지금까지 4타일 로그는 `paint=debug` 뿐이라 `ring_id=` / `profile_id=` 가 **0 건**이었다.
켜면 `D3D11 video: plane 링 생성 ring_id=N ...` 36 건과 어느 디바이스를 쓰는지가 나온다.

## 현재까지의 실용 결론

| 항목 | 상태 |
|---|---|
| **최선 설정** | `BASE-4T + gfx_vsync_enabled=false` → **15.8 fps** (5.3 대비 3배) |
| 페이싱 노브 | vsync off 상태에서는 **해롭다**(15.8 → 9.0). 함께 쓰지 말 것 |
| 순차 렌더 상한 | 렌더 합 26.9ms → **~37fps**. 30fps 는 사정권 |
| GPU 편중 | adapter 0 에 월 전체 비디오 D3D11 부하 집중(추정, 확정 실행 필요) |
| external 승격 158ms | **미해결, 별개** — BASE-4T 는 escape off 상태 |

---

# ★2026-08-21 (9) — 미디어→렌더러 경로의 실제 구성 (코드 확정)★

GPU 엔진 분해 실측: **전 어댑터가 3D 만**, Video Decode 0, **Copy 0**, adapter 0 에 편중.
이 제약 위에서 코드를 읽으면 경로는 다음과 같다.

```
[1] 디코드   avdec_*  소프트웨어, 스레드 1 개씩 × 36        → CPU
             (media_video_decoder_policy 기본 "" = software)
             ★Video Decode 엔진 0 과 일치★

[2] 업로드   appsink sysmem I420  →  CPU memcpy  →  D3D11_USAGE_DYNAMIC plane 텍스처
             (I420 = R8 × 3), MiscFlags: 0
             ★생성 대상이 프로세스 전역 CONSUMER_DEVICE **하나**★
             (d3d11_ring.rs / render-d3d11 lib.rs:173 ensure_ring)
             ※ 여기에 GstD3D11Converter 는 없다 — 프로듀서는 memcpy 만 한다

[3] 팬아웃   PaintMessage::UpdateImages
             → paint.rs:1810  painter.update_images(updates.clone())  ×  **모든 painter**
             36 비디오 × 30fps ≈ 초당 1,120 건, 각 건이 painter 수만큼 복제

[4] 합성     각 painter 의 WebRender 가 plane 3 장을 external texture 로 샘플하고
             **YUV→RGB 를 자기 셰이더에서** 수행 ("WR YUV 직접 샘플")
             ★즉 변환이 공유되지 않고 **painter 마다 36 개 전부 다시** 계산된다★
             ★3D 엔진만 쓰는 것과 일치★
```

## 구조적 결론 두 가지

**(a) YUV→RGB 변환이 타일 수만큼 중복된다.** 한 번 변환해 공유하는 구조가 아니라, 각
painter 의 WR 셰이더가 36 개 비디오의 plane 을 각자 샘플·변환한다. 타일이 늘면 이 3D 작업이
**나뉘지 않고 곱해진다.** 엔진 분해가 "3D 만" 인 것과 정확히 맞는다.

**(b) plane 텍스처가 단 하나의 디바이스에 있다.** `MiscFlags: 0` 이라 공유 불가한
DYNAMIC 텍스처인데 소유자는 전역 `CONSUMER_DEVICE` 한 개다. 나머지 painter 들은 남의
어댑터 텍스처를 샘플하는 셈이다.

## 미해결: adapter 0 편중의 주체가 무엇인가

`set_consumer_device` 는 `Painter::new` 마다 불리고 가드가 없어 **마지막 painter 가 이긴다**.
이번 로그로 생성 순서가 확정됐다:

```
Selected DXGI adapter index:  0 → 2 → 3 → 1
tile 0=adapter 0(primary), tile 1=adapter 2, tile 2=adapter 3, tile 3=adapter 1
```

→ `CONSUMER_DEVICE` = **adapter 1**. 그런데 편중은 **adapter 0** 이다. **맞지 않는다.**
그리고 `RenderingContext` 구현을 확인해도 네 타일 모두 `WindowRenderingContext` →
`SurfmanRenderingContext::media_d3d11_device_handle()` 로 **전부 Some 을 반환**한다
(기본 None 으로 빠지는 타일은 없다).

★로그에는 미디어 텍스처가 어느 어댑터에 생기는지 적는 라인이 아예 없다★
(`plane 링 생성 ring_id=N {w}x{h} gst=...` 뿐). 코드 읽기만으로는 여기까지가 한계다.

## ★결정적 실험 — 빌드 없이, 레이아웃 JSON 순서만 바꾼다★

`CONSUMER_DEVICE` 가설이면 편중은 **마지막 타일의 어댑터**를 따라간다.
adapter 0 고정(= DXGI 기본 어댑터, gst 계열) 가설이면 순서와 무관하게 adapter 0 이다.

`tiles` 배열 순서를 뒤집어 **display 0 을 마지막에** 둔다:

```json
{ "virtualViewport": { "width": 11520, "height": 4320 },
  "tiles": [
    { "display": 3, "rect": [5760, 2160, 5760, 2160] },
    { "display": 2, "rect": [0,    2160, 5760, 2160] },
    { "display": 1, "rect": [5760, 0,    5760, 2160] },
    { "display": 0, "rect": [0,    0,    5760, 2160] }
  ],
  "overlapPx": 0 }
```

| 결과 | 결론 |
|---|---|
| 편중이 **adapter 1 로 이동** | `CONSUMER_DEVICE` 확정 → 표적은 "소비자 디바이스를 painter 별로 분산" |
| 편중이 **adapter 0 에 그대로** | DXGI 기본 어댑터에 묶인 무언가(gst d3d11 등) → 표적은 어댑터 어피니티 |

빌드도 코드 변경도 필요 없고 20 초면 된다. **fps 순위 재기를 멈추고 이것부터 할 것.**

---

# ★★2026-08-24 — 근본 원인 확정: ANGLE LUID display 캐시 충돌 (렌더가 GPU 1장에 전부 몰림)★★

`ctrl_ww` 에 WALLDIAG 진단 3종을 넣고 4타일을 정순/역순으로 돌린 결과다.

```
정순(타일 display 0,1,2,3):  requested_gpu = 0, 2, 3, 1  →  device = 0x1d2c3539480  (넷 전부 동일)
역순(타일 display 3,2,1,0):  requested_gpu = 1, 3, 2, 0  →  device = 0x19260b028e0  (넷 전부 동일)
ring owner: 두 실행 모두 36 개 링이 그 하나의 device
wrap: 네 painter 전부 OK.  EGLImage 실패 0.
```

★**네 painter 가 서로 다른 어댑터를 요청했는데 반환된 `ID3D11Device` 가 하나다.**★
`Selected DXGI adapter index 0/1/2/3` 로그는 나오지만 실제 디바이스는 공유된다.
**즉 이 월은 4-GPU 로 렌더하고 있지 않았다 — RX 580 한 장이 4 타일분을 순차로 하고 있었다.**

## 이 한 줄이 그동안의 모순을 전부 설명한다

| 관측 | 설명 |
|---|---|
| adapter 0 만 점유율 2배+ | 실제 렌더가 전부 거기서 일어난다 |
| `wrap OK` 4/4, `EGLImage` 실패 0 | 같은 디바이스라 래핑이 당연히 성공 |
| **Copy 엔진 0** | 크로스 어댑터 전송이 애초에 없다 |
| 전 어댑터 3D 만, 나머지는 소량 | 나머지 셋은 자기 창 present/스캔아웃만 |
| 타일 2개→10배 붕괴, 4타일 5fps | GPU 1 장에 N 배 일감. **타일 수가 곧 배수** |
| `CONSUMER_DEVICE` 전역 1 개가 문제 없어 보였던 것 | 애초에 디바이스가 하나뿐이라 드러날 수 없었다 |

## 원인 — 저장소가 이미 문서화해 둔 것

`etc/multigpu/patches/README.md` 가 증상을 **글자 그대로** 적어 두었다:

> ANGLE 의 `ANGLEPlatformDisplay` 캐시 키는
> `nativeDisplay / powerPreference / platformType / deviceIdHigh / deviceIdLow` 만 쓰고
> **LUID 는 빠져 있다.** surfman 은 GPU 를 LUID 로 선택하므로, LUID 만 다른 요청이 **같은
> 캐시 키로 충돌**해 처음 만들어진(보통 GPU0) display 가 모든 어댑터에 재사용된다
> → **모든 렌더가 GPU0 집중.** ★**동일 모델 GPU 2 개는 deviceId 로도 구분 불가.**★

이 장비는 **동일 모델 RX 580 2048SP 4 장**이라 deviceId 가 넷 다 같다 → 캐시 키가 완벽히
충돌한다. 4-GPU 동일 모델은 이 버그의 **최악 조건**이다.

## 패치는 있는데 적용돼 있지 않다 (확인함)

```
etc/multigpu/patches/mozangle-0.5.5-angle-luid-display-cache.patch   ← 존재
etc/multigpu/patches/apply_mozangle_angle_luid.ps1                   ← 적용 스크립트

W:\servo\.servo\cargo-home\...\mozangle-0.5.5\gfx\angle\checkout\src\libANGLE\Display.cpp
C:\Users\nuricon\.cargo\...\mozangle-0.5.5\...\Display.cpp
  → luidHigh/luidLow 참조 0 건  = ★미적용★
```

★**커밋할 수 없는 성격의 패치라 머신마다 다시 적용해야 한다**★ — mozangle 은 crates.io
의존성이고 빌드는 **cargo 레지스트리 캐시**(레포 밖)의 소스를 컴파일한다. 그래서 새 워크트리
빌드(`ctrl_base`/`ctrl_ww`)와 테스트 머신 빌드 모두 **패치 없는 상태로 만들어졌다.**
이것이 "단일 GPU 에서는 멀쩡한데 4 GPU 에서 무너진다" 의 정체다.

## 적용

```powershell
etc\multigpu\patches\apply_mozangle_angle_luid.ps1 -Rebuild   # 적용 + ANGLE 강제 리빌드 + DLL 복사
```

적용 후 WALLDIAG 로 **재검증**할 것 — 성공 판정은 명확하다:

```
WALLDIAG consumer-device publish 의 device 값이 painter 마다 **서로 달라야** 한다
```

같은 값이면 패치가 안 먹은 것이다.

## 앞선 결론들의 재평가

- **`gfx_vsync_enabled=false`(5.3→15.8)** — 여전히 유효하지만, 그것은 **GPU 1 장 위에서의**
  개선이었다. 렌더가 실제로 분산되면 수치가 통째로 달라진다.
- **페이싱 노브** — 전부 GPU 1 장 전제에서 잰 값이다. **재측정 대상.**
- **external 승격 158ms 이상치** — painter 하나만 느렸던 것도 "한 GPU 에서 4 타일 경합" 이라는
  전제 위의 관측이다. **재현되는지부터 다시 봐야 한다.**
- **"YUV→RGB 변환이 타일 수만큼 중복" (2026-08-21 (9) 결론 a)** — 이것은 **여전히 사실**이고
  패치와 무관한 별개 구조 문제다. 다만 분산 후에는 GPU 당 1 회씩이 된다.

★즉 이 패치 적용 전에 잰 모든 성능 수치는 "4-GPU 월" 의 수치가 아니다.★

## 왜 빠졌나 (2026-08-24 조사)

**제거된 적이 없다 — 이 머신들에 적용된 적이 없을 뿐이다.**

```
4f7ec023c83  2026-06-14  멀티 GPU 연산 분산용 mozangle ANGLE LUID 패치 + 적용 스크립트 추가
   b0a7ce85 조상? YES        HEAD 조상? YES
```

패치 파일은 6 월부터 저장소에 있었고 양쪽 계보에 다 들어 있다. 문제는 **코드로 커밋될 수 없는
종류**라는 점이다 — mozangle 은 crates.io 의존성이라 빌드가 **cargo 레지스트리 캐시(레포 밖)**
의 소스를 컴파일한다. 저장소는 `.patch` + 적용 스크립트만 들고, **적용은 머신마다 수동**이다.

★그리고 빌드 경로 어디에서도 자동 호출하지 않는다★ — 참조가 전부 문서뿐이다:

```
docs/ai-notes.md · etc/multigpu/patches/README.md · etc/multigpu/WEBGL_WALL_STATUS.md
scripts/servo_env.ps1  → 언급 없음   ← 모두가 소싱하는 환경 스크립트
CLAUDE.md              → 언급 없음   ← 빌드 지침
mach / 빌드 훅          → 없음
```

그래서 **새 머신 셋업 / 새 CARGO_HOME / 레지스트리 캐시 재추출** 때마다 조용히 되돌아간다.
이번 조사용으로 새로 만든 `ctrl_base`·`ctrl_ww` 워크트리도 당연히 미적용으로 빌드됐다.

★★가장 나쁜 점: 실패가 보이지 않는다★★ — `Selected DXGI adapter index 0/1/2/3` 은 정상으로
찍히고 경고도 폴백 로그도 없다. WALLDIAG 를 넣기 전에는 "네 painter 가 같은 디바이스" 를 알
방법이 **아예 없었다.** 이번 조사에서 가설 6 개가 연달아 반증된 이유가 이것이다 — 관측 가능한
모든 지표가 정상으로 보였다.

## 재발 방지 (택일)

| 방안 | 성격 |
|---|---|
| A. `scripts/servo_env.ps1` 에서 적용 스크립트 호출 | 스크립트가 이미 멱등(`[skip] already patched`). 한 줄, 즉효 |
| B. mozangle 포크 + `[patch.crates-io]` | README 가 권하는 영구 해법. 커밋 가능 → 잃어버릴 수 없다 |
| ★C. 기동 시 런타임 경고★ | painter 2 개 이상이 **같은 `media_d3d11_device_handle`** 을 반환하면 크게 경고 |

**C 를 권한다** — A/B 는 이 패치 하나만 지키지만 C 는 *"타일이 실제로 서로 다른 GPU 에서
도는가"* 라는 **불변식**을 지킨다. 드라이버·surfman·백엔드가 어떤 이유로 바뀌어 분산이 깨져도
즉시 드러난다. 지금 넣은 WALLDIAG 진단을 정식 경고로 승격하는 수준의 작업이다.

## 적용 기록 (2026-08-24)

```
apply_mozangle_angle_luid.ps1  →  [ok] W:\servo\.servo\cargo-home\...\mozangle-0.5.5
                                  [ok] C:\Users\nuricon\.cargo\...\mozangle-0.5.5
   Display.cpp luid refs: 0 → 11 (양쪽)
mozangle 산출물 강제 삭제(build/.fingerprint/deps) 후 예제 재빌드 (2 분 53 초)
```

★재빌드 함정★ — 새 ANGLE DLL 은 `target\release\build\mozangle-*\out\` 에만 생기고
`target\release\` 의 것은 **구버전 그대로다**(6,866,944 → 7,011,328 바이트로 크기가 다르다).
**손으로 복사해야 한다.** 적용 스크립트의 `-Rebuild` 분기가 이 복사를 하지만, 그 분기는 전체
`mach build` 를 함께 돌린다.

개발기(GPU 1 장) 회귀 검증: 36/36 D3D11 · 36/36 direct-file · dcomp engaged · panic 0.
분산 검증은 4-GPU 실기에서만 가능하다.

---

# ★★2026-08-24 (2) — ANGLE 패치 후: 버그 A 가 가리고 있던 버그 B 가 드러났다★★

패치된 ANGLE + WALLDIAG 로 4타일 정순/역순 실행.

| | 정순 | 역순 |
|---|---|---|
| publish device 4 개 | **전부 다름** ✅ | **전부 다름** ✅ |
| fan-out 런타임 경고 | 0 (올바르게 조용, 오탐 없음) | 0 |
| ring owner | `0x2233974ee80` = **마지막 publish** = gpu 1 = display 3 | `0x2617de47d90` = 마지막 = gpu 0 = display 0 |
| wrap **OK** | gpu **1** 만 | gpu **0** 만 |
| wrap **FAIL** | gpu 0, 2, 3 | gpu 1, 2, 3 |
| `EGLImage` 실패 warn | **15,740 건** (초당 ~675) | **15,924 건** |
| 육안 | **display 3 에서만 재생**, 나머지 녹색 | **display 0 에서만 재생**, 나머지 녹색 |

★버그 A(ANGLE LUID)는 해결됐다★ — 어댑터별로 디바이스가 갈린다.
★그리고 그것이 가리고 있던 버그 B 가 드러났다★ — **전역 `CONSUMER_DEVICE` 하나.**

## 2026-08-21 의 최초 가설이 옳았다

당시 이렇게 적었다가 반증당했다:

> 실패는 캐시되지 않는다 → 36 비디오 × 3 plane = 프레임당 108 회 실패 래핑 + warn 홍수

당시 `EGLImage` 실패가 **0 건**이라 반증됐는데, 그 이유는 네 painter 가 **같은 디바이스**를
쓰고 있어 실패할 수가 없었기 때문이다(= 버그 A). A 를 고치니 예측한 메커니즘이 **글자 그대로**
나타났다. 구조 서술은 처음부터 맞았고, 관측이 다른 버그에 가려져 있었다.

## 지금 fps 는 측정 의미가 없다

```
정순:  P1=33.9  P2=36.1  P3=31.6  P4=2.6 ms   합 104.2ms → 상한 9.6fps (실측 8.3)
역순:  P1=33.6  P2=35.9  P3=32.4  P4=2.4 ms   합 104.4ms         (실측 8.2)
```

★**정상 동작하는 painter 가 가장 빠르다(2.6ms).**★ 비디오를 못 받는 셋이 32~36ms 인 이유는
실패 경로가 캐시되지 않아 매 프레임 실패 래핑 + `warn!` 포맷팅 + 비버퍼 stderr 를 반복하기
때문이다. **3/4 가 고장난 상태의 수치**라 vsync·페이싱 등 어떤 비교도 아직 무의미하다.

**녹색 화면** = `ExternalImageSource::Invalid` → YUV plane 이 비어 있음(Y=0,U=0,V=0).
BT.601 변환 시 RGB≈(0,135,0). 검정이 아니라 녹색인 것이 "YUV 경로에 데이터 없음" 의 정확한 신호다.

## 다음 표적: `CONSUMER_DEVICE` 를 painter 별로

```rust
// components/media/player/d3d11_ring.rs   ← 고쳐야 할 곳
static CONSUMER_DEVICE: Mutex<Option<usize>> = Mutex::new(None);   // 전역 1개
// render-d3d11/lib.rs ensure_ring 이 이 하나로 36 개 링을 만든다
```

선택지(2026-08-21 (9) 에서 정리한 것과 동일, 이제 실측 근거가 붙었다):

| # | 방식 | 비용 |
|---|---|---|
| 1 | **소비자 디바이스별 링 세트** — 등록된 디바이스마다 plane 텍스처 생성 | CPU memcpy × painter 수 (36→144/frame) |
| 2 | staging + `USAGE_DEFAULT` + `SHARED_NTHANDLE` | §3-n 이 없앤 `CopySubresourceRegion` 부활, **게다가 어댑터를 못 넘는다** |
| 3 | **타일에 보이는 것만 업로드** | 6x6 을 4 타일이 나누면 타일당 9 개 → **총량 36 으로 오늘과 동일**. 가장 효율적이나 가시성 판정 필요 |

★`DYNAMIC + CPU_ACCESS_WRITE` 는 공유 플래그를 붙일 수 없다(D3D11 은 공유 리소스에
`USAGE_DEFAULT` 를 요구)★ — 그래서 "플래그 하나 추가" 로는 끝나지 않는다.

## 영구 가드 추가 제안

지금 `WALLDIAG wrap FAIL` 은 `info!` 다. **`warn!` 로 승격**하면(painter 당 1 회 요약)
이 두 번째 버그도 조용한 실패가 되지 않는다. 첫 번째 가드(디바이스 중복)와 짝을 이룬다:

- 가드 1: painter 들이 **같은 디바이스**를 받음 → 팬아웃이 깨졌다
- 가드 2: painter 가 비디오 텍스처를 **래핑하지 못함** → 그 타일은 영상이 안 나온다

---

# 2026-08-24 (3) — 옵션 3 구현: 1 단계(레지스트리) 완료

## 설계 확정 근거: 가시성 신호는 이미 있다

포스트-패치 로그로 **WebRender 가 타일 밖 external image 를 lock 하지 않음**을 실측했다:

```
EGLImage 실패 15,740 건 / 논리 프레임 241 회
  프레임당 65.3 → painter 당 21.8 → 비디오 환산 7.3 개/타일
  (전량 lock 이면 36 개/타일, 6x6 을 4 타일 분할이면 9 개/타일)
```

→ **소비자의 `lock` 요청 = "그 타일에 보인다"**. 가시성 판정을 새로 만들 필요가 없다.

## 1 단계: `components/media/player/d3d11_ring.rs` (완료, 17/17 통과)

링을 **(비디오 × 디바이스)** 로 나누는 그룹 계층을 얹었다. 기존 슬롯 상태기계는
**그대로 두고**(불변식·기존 테스트 12 개 무손상) 위에 �feature 계층만 추가했다.

```rust
struct GroupState { rings: HashMap<usize,u64>, demand: HashMap<usize,Instant> }
struct Registry   { rings, groups, ring_device }        // ring_device: ring_id -> device
pub const RING_DEMAND_TTL: Duration = Duration::from_secs(2);

create_group() / remove_group(group)
note_demand(group, device)                 // ★가시성 신호★
demanded_devices(group, ttl) -> Vec<usize> // 프로듀서는 여기에만 업로드
ring_for(group, device) -> Option<u64>
attach_ring(group, device, ring_id)
expire_stale_demand(group, ttl) -> Vec<u64>
create_ring(device, planes_per_slot, slots)          // device 인자 추가
take_removed_rings_for_device(device)                // ★디바이스 한정 회수★
RemovedRing { textures, mapped, device }             // device 필드 추가
```

★핵심 안전장치★ `take_removed_rings_for_device` — 소비자가 여럿이면 전역 드레인은
**남의 디바이스 텍스처를 Release** 한다. 설계 단계에서 지목했던 위험이고, 유닛 테스트로 고정했다.

## 테스트에서 실제로 걸린 것 (기록)

처음에 새 테스트가 전역 `take_removed_rings()` 를 써서 **기존 테스트 3 개를 깨뜨렸다.**
`removed_rings()` 큐가 전역이고 테스트는 병렬 실행되기 때문이다. 기존 테스트에는
*"다른 테스트와 병렬 실행될 수 있으므로 개수 대신 내용으로 찾는다"* 는 완화가 이미
있었지만 **큐가 통째로 비워지면 내용 검색도 실패한다** — 완화로는 부족했다.

→ 신규·기존 테스트 **전부** 고유 디바이스 id + `take_removed_rings_for_device` 로 전환해
격리를 완결했다. 지금 전역 드레인을 부르는 테스트는 없다.

## 남은 단계

| 단계 | 파일 | 내용 |
|---|---|---|
| 2 | `media/player/video.rs` | `VideoFrameD3D11YuvData.ring_id` → `group_id` |
| 3 | `gstreamer/render-d3d11/lib.rs` | `ensure_ring` → `ensure_group` + `demanded_devices` 루프(디바이스별 claim/copy/publish) + `expire_stale_demand` |
| 4 | `media-thread/lib.rs` | `binding.group_id` + 자기 디바이스로 `ring_for` 해석, `note_demand`, 드레인을 디바이스 한정으로 |

★4 단계에서 반드시★ — `lib.rs:763`(purge) 과 `:879`(Drop) 의 `take_removed_rings()` 를
**`take_removed_rings_for_device(내 디바이스)`** 로 바꿔야 한다. 안 바꾸면 painter 가 남의
텍스처를 해제한다.

파급: `paint_api::VideoFrameLease.ring_id` 와 `dcomp_compositor` 의 present 중복 판정은
**해석된 per-device ring_id** 를 그대로 받으면 되므로 의미가 오히려 정확해진다(현재는
전역 링 하나라 painter 간 판정이 섞인다).

# 2026-08-24 (4) — 옵션 3 구현 완료 (2~4 단계)

## 2 단계 `media/player/video.rs`
`VideoFrameD3D11YuvData.ring_id` → **`group_id`**. 페이로드는 디바이스를 모른다.

## 3 단계 `render-d3d11/lib.rs` — 프로듀서

```rust
let group_id = self.ensure_group(state, caps)?;                  // 디바이스 무관
D3d11PlaneRings::expire_stale_demand(group_id, RING_DEMAND_TTL); // 끊긴 수요 회수
let target_devices = D3d11PlaneRings::demanded_devices(group_id, RING_DEMAND_TTL);
if target_devices.is_empty() { /* 업로드 보류. 프레임은 정상 반환 */ }
for device in target_devices {
    let ring_id = self.ensure_device_ring(group_id, device, ...)  // 그 GPU 위에 텍스처 생성
    claim_free_slot → copy_planes → publish_slot                  // 디바이스별로 독립
}
```

배압/스테이징/드롭이 **디바이스별로 독립**이다 — 한 GPU 가 밀려도 다른 타일은 계속 간다.
`target_devices` 가 비면 업로드를 건너뛰되 **프레임은 반드시 반환**한다(None 을 돌려주면
appsink 가 `FlowError::Error` 로 바꿔 파이프라인이 통째로 정지한다 — bf70293c4 계열).

## 4 단계 `media-thread/lib.rs` — 소비자 (WR 경로 + escape 경로 **둘 다**)

```rust
D3d11PlaneRings::note_demand(binding.group_id, device);              // ★가시성 신호★
let ring_id = D3d11PlaneRings::ring_for(binding.group_id, device)?;  // 없으면 이번 프레임만 비움
```

★그리고 드레인을 `take_removed_rings_for_device(self.device)` 로 교체★(purge·Drop 두 곳) —
설계 때 최대 리스크로 지목했던 지점이다. 전역 드레인이면 painter 가 남의 텍스처를 해제한다.

## 구현하며 실제로 처리한 것

- **escape 경로(`acquire`)는 `rc` 만 받는다.** `media_d3d11_device_handle()` 이 호출마다
  **AddRef** 하는 API 라 프레임마다 부르면 refcount 가 샌다(4 타일 36 영상 30fps = 초당
  ~4,300 회). `this_thread_d3d11_device()` 로 스레드당 1 회만 구하도록 메모했고, 전제
  ("한 스레드가 두 디바이스를 섞어 쓰지 않는다")를 주석에 명시했다.
- **`MediaVideoExternalSurfaceProvider` 도 프로세스 전역 단일**이다 — `CONSUMER_DEVICE` 와
  같은 버그 계열. 지금은 `acquire` 가 `rc` 로 디바이스를 구해 동작하지만, escape 트랙에서
  따로 정리해야 할 항목으로 남는다.
- 마지막 참조 `htmlmediaelement.rs:927` 하나가 남아 `servo-script` 가 깨졌다(빌드로 잡음).

## 검증 상태

| 항목 | 결과 |
|---|---|
| `d3d11_ring` 유닛 테스트 | **17/17** (기존 12 + 신규 5) |
| 팬아웃 가드 유닛 테스트 | **5/5** |
| 릴리스 빌드 | 성공, 새 경고 없음 |
| 단일 GPU 스모크(개발기) | 36/36 D3D11 · 36/36 direct-file · wrap OK · **EGLImage 실패 0** · panic 0, 링 36 개 |

★**멀티 GPU 분산은 이 개발기(GPU 1 장)에서 검증할 수 없다.**★ 유닛 테스트는 레지스트리
계층만 덮는다. 실기 판정 기준:

| 지표 | 옵션 3 이전 | 기대 |
|---|---|---|
| `WALLDIAG wrap OK` | painter 1 개 | **4 개 전부** |
| `WALLDIAG wrap FAIL` | painter 3 개 | **0** |
| `EGLImage` 실패 warn | 15,740 건 | **0 또는 기동 직후 극소수** |
| `plane 링 생성` 의 `device=` | 1 개 값 | **4 개 값으로 분산** |
| 링 총 개수 | 36 | **36 근처**(경계 걸친 영상만 2 개). 144 에 가까우면 컬링이 기대만큼 안 되는 것 |
| 육안 | 마지막 타일만 재생 | **4 타일 전부 재생** |
| painter 별 `render_ms` | 2.6 / 32~36ms | 균등 |

★fps 는 이 단계에서 판단하지 말 것★ — 지금까지 잰 모든 수치는 3/4 가 고장난 상태의 것이다.
4 타일이 전부 정상 재생되는 것을 먼저 확인하고, 그 다음에 vsync·페이싱을 처음부터 다시 잰다.

---

# ★★2026-08-24 (5) — 실기 검증: 4-GPU 월이 처음으로 실제로 동작★★

`ctrl_B_20260824_173323.err.log` (4타일, 옵션 3 적용 빌드).

## 판정 기준 전항 통과

| 지표 | 옵션 3 이전 | 실측 |
|---|---|---|
| `WALLDIAG wrap OK` | painter 1 개 | ★**gpu 0,1,2,3 전부**★ |
| `WALLDIAG wrap FAIL` | painter 3 개 | **0** |
| `EGLImage` 실패 warn | **15,740** | **0** |
| fan-out 런타임 경고 | 0 | 0 (오탐 없음 유지) |
| 링 총 개수 | 36 (device 1 개) | **36** |
| device 별 링 분포 | — | ★**9 / 9 / 9 / 9 — 4 GPU 균등**★ |
| 육안 | 마지막 타일만 재생 | **4 타일 전부 정상 재생** |

★**device 당 정확히 9 개**★ — 6x6=36 을 2x2 타일이 나누면 타일당 3x3=9. 경계에 걸친 영상이
하나도 없어 **총 링 수가 36 으로 단일 GPU 때와 동일**하다. 즉 **CPU memcpy 총량이 전혀 늘지
않았다** — 옵션 3 이 노렸던 최선 시나리오가 그대로 나왔다(옵션 1 이었다면 144 개, 4 배였다).

## 성능 — 처음으로 의미 있는 수치

```
30s 실행
  P1: 23.6  P2: 23.6  P3: 23.6  P4: 23.6 fps       ← 완전 균등
  render p50: 5.2 / 4.6 / 4.6 / 4.6   합 19.0 ms
  배리어: completed_before_deadline 628 / after 0
  페이싱: total_coalesced=2280  total_released=131
```

**5.1 → 23.6 fps (4.6 배).** painter 렌더 합은 **104.2ms → 19.0ms** (5.5 배 감소) — 실패
래핑 홍수가 사라지고 각 GPU 가 자기 몫 9 개만 처리하기 때문이다.

## 남은 격차와 다음 표적

```
프레임 주기        42.4 ms   (23.6 fps)
  순차 렌더 합     19.0 ms   → 순차 유지 시 상한 52.6 fps
  ────────────────────────
  설명 안 되는 구간 ~23 ms
페이싱: coalesced 2280 / released 131 = 17 : 1   ← 여전히 대부분 막힌다
```

30fps 까지 6.4fps 가 남았고, 렌더는 이미 충분히 빠르다(19ms). **다음 표적은 페이싱 게이트**다.

★그런데 페이싱 노브는 처음부터 다시 재야 한다★ — 이전 측정
(`max_pending=30` → 9.9, `pacing_enabled=false` → 월 분리, vsync off → 15.8 등)은 **전부
3/4 타일이 고장난 상태에서 잰 것**이라 순위가 그대로 유지된다고 볼 근거가 없다. 특히
`gfx_vsync_enabled` 는 이번 실행에서 **꺼져 있었으므로**(런처 기본) 켠 경우도 다시 봐야 한다.

**재측정 순서 제안** — BASE-4T(옵션 3 빌드) 기준, 각 30 초, 한 번에 하나씩:

1. 기준(그대로) = 23.6 fps
2. `-Vsync` (SERVO_WIN_VSYNC=1)
3. `gfx_wall_frame_max_pending` 상향
4. `gfx_wall_frame_min_interval_ms=1`

각 실행에서 **네 타일 fps 가 균등한지**를 먼저 확인할 것 — 균등이 깨지면
(`pacing_enabled=false` 가 그랬듯) 그 설정은 fps 가 올라가도 월로서는 실패다.

---

# 2026-08-24 (6) — 본 워크트리(`servo_multigpu-tiled-wall`, pref 체계)로 이관 완료

## 이관 방식

| 파일 | 방식 |
|---|---|
| `media/player/d3d11_ring.rs`, `media/player/video.rs` | 두 커밋 간 **원본 동일** → 직접 복사 |
| `script/dom/html/htmlmediaelement.rs` | 한 줄(`ring_id:` → `group_id:`) |
| `render-d3d11/lib.rs`, `media-thread/lib.rs` | `git apply -3` — **둘 다 깨끗하게 적용** |

★pref 체계가 보존됐다★ — `pref!(media_d3d11_enabled)`,
`debug_env::string(&debug_env::D3D11_PROFILE)` 그대로. env 시대 코드가 딸려 오지 않았다.
사용자의 기존 미커밋 변경 3 건(레이아웃 JSON·probe HTML)과는 파일이 겹치지 않는다.

## ★빌드 시 반드시 해야 하는 두 가지★

**1. mozangle 강제 재빌드.** 이 워크트리의 `target/release` 는 **패치 전 ANGLE 산출물을
fingerprint 로 재사용**한다(`cc` 가 번들 `.cpp` 에 rerun-if-changed 를 걸지 않는다). 그냥
빌드하면 코드는 옵션 3 인데 ANGLE 은 패치 전이라 **다시 1 GPU 로 몰린다.**

```powershell
Get-ChildItem 'target\release\build'        -Directory -Filter 'mozangle-*' | Remove-Item -Recurse -Force
Get-ChildItem 'target\release\.fingerprint' -Directory -Filter 'mozangle-*' | Remove-Item -Recurse -Force
Get-ChildItem 'target\release\deps'         -Filter '*mozangle*'            | Remove-Item -Force
```

**2. 새 ANGLE DLL 을 손수 복사.** `build\mozangle-*\out\` 에만 생긴다
(6,866,944 → **7,011,328** 바이트로 확인).

## ★예제 실행 시 함정★ — 플러그인은 exe 옆에서 로드된다

`target\release\examples\` 에는 mach 가 GStreamer 를 패키징하지 않는다. 그대로 돌리면:

```
Error initializing GStreamer: ErrorLoadingPlugins(["gstmediafoundation.dll"])
→ d3d11=0/36  direct_file=0/36  링생성=0   (코드 문제로 오인하기 쉽다)
```

사내 GStreamer 루트의 `bin\*.dll` + `lib\gstreamer-1.0\*.dll` 를 `examples\` 로 복사해야
한다(444 개). ★그 다음 **패치된 ANGLE 을 다시 덮어써야 한다**★ — GStreamer 번들에도
`libGLESv2.dll` 이 들어 있어 패치본을 덮어버린다.

## 검증

| 항목 | 결과 |
|---|---|
| 유닛 테스트 | `d3d11_ring` **17/17**, `wall_gpu_fanout` **5/5** |
| 릴리스 빌드 | 성공(5분 15초), 새 경고 없음 |
| 단일 GPU 스모크(pref 문법) | **36/36 d3d11 · 36/36 direct-file · 링 36 · EGLImage 실패 0 · panic 0 · 팬아웃 경고 0** |

## ctrl_ww 워크트리 — 지우기 전 확인할 것

★`ctrl_ww` 는 `--detach` 로 만든 **detached HEAD 라 브랜치가 없고**, 오늘 작업은 전부
**커밋되지 않은 워킹트리 수정**이었다. 그대로 지웠으면 구현이 사라진다.★

- 백업 패치: `W:\opt3_perdevice_rings.patch` (1,266 줄, 워크트리 밖)
- 본 워크트리 이관 완료(위)
- 지울 때: `git worktree remove ../ctrl_ww` — 안의 미추적 레이아웃 JSON 4 개도 함께 사라진다

---

# 2026-08-24 (7) — 재발 방지 스크립트 + pref 체계 배포본

## `etc/multigpu/patches/verify_angle_luid.ps1`

빌드/실행 전에 **한 번 돌리면 오늘 밟은 함정이 전부 걸린다.**

```powershell
etc\multigpu\patches\verify_angle_luid.ps1           # 확인 + 스테일 DLL 자동 복사
etc\multigpu\patches\verify_angle_luid.ps1 -Check    # 보고만, 문제 있으면 exit 1
```

검사 3 항목:
1. 빌드가 실제로 컴파일하는 mozangle 소스에 **LUID 패치가 적용돼 있나**
2. `target\<profile>\libGLESv2.dll` 이 방금 빌드된 것과 같은가(**해시 비교**)
3. `target\<profile>\examples\` 의 사본도 같은가

★해시로 비교한다★ — 패치 전/후 빌드가 같은 크기로 나올 수 있어 크기 비교는 스테일을 놓친다.

**검사 자체를 검증했다**: 패치 전 ANGLE(6,866,944)을 주입 → `-Check` 가 탐지하고 exit 1 →
무인자 실행이 복구 → 재확인 OK/exit 0.

### 이 과정에서 정정한 것 둘

- ★**"GStreamer 번들이 ANGLE 을 덮는다" 는 틀렸다**★ — 사내 번들에 `libGLESv2.dll`/
  `libEGL.dll` 이 **없다**(확인함). 앞 절에 그렇게 적었던 것은 추측이었다. `examples\` 가
  드리프트하는 진짜 이유는 **손으로만 채워지기 때문**이다.
- **성공 경로가 종료 코드를 안 남겼다** — PowerShell 스크립트가 그냥 끝나면 `$LASTEXITCODE`
  에 **직전 값이 남는다**. 체이닝 시 오판하므로 `exit 0` 을 명시했다.
- 덤: `apply_mozangle_angle_luid.ps1` 의 `$repo` 계산이 한 단계 모자라
  `...\etc` 를 가리킨다(잠재 버그, `-Rebuild` 분기와 `.servo\cargo-home` 후보에 영향).
  verify 쪽은 세 단계로 올라가도록 했고 주석에 남겼다.

## `etc/multigpu/make_wall_dist.ps1` + `run_wall_dist.ps1`

pref 체계 배포본(`target\wall_dist`, **444 DLL / 0.43 GB**). clean PATH 검증 통과
(36/36 d3d11 · 36/36 direct-file · EGLImage 0 · 팬아웃 경고 0).

생성기가 하는 것:
- ★`verify_angle_luid.ps1` 을 먼저 돌리고 실패하면 **패키징을 거부**한다★ — 1 GPU 로 도는
  배포본을 만들어 내보내는 것이 최악이다
- GStreamer 를 **전량** 복사(큐레이트 목록이 조용히 빠뜨린 전례) 후 **ANGLE 을 마지막에 다시** 덮음
- 페이지의 `../Wildlife_....mp4` 참조 때문에 `tests\` 구조를 `pages\` 아래에 보존

런처가 하는 것:
- 엔진 노브를 **pref 로** 전달(이 워크트리는 env 를 주면 `removed_env` 가 기동을 막는다).
  들어오기 전 `SERVO_*` 를 전부 지운다
- `RUST_LOG` 를 **무조건** 설정(조건부였다가 진단이 두 번 날아갔다). `media=` 누락 시 경고
- 끝에 판정 요약: 마커 · **타일별 fps** · 링 소유 디바이스 분포 · wrap OK/FAIL · EGLImage · 팬아웃 경고
- ★**타일 간 fps 편차가 2 이상이면 경고**★ — 평균을 올리면서 타일을 갈라놓는 설정
  (`pacing_enabled=false` 가 그랬다)은 월로서는 실패다. 월 속도는 **가장 느린 타일**이 정한다

## 실기 재측정 시작점

```powershell
.\run_wall_dist.ps1 -DurationSec 30                                  # 기준
.\run_wall_dist.ps1 -DurationSec 30 -Vsync                           # vsync
.\run_wall_dist.ps1 -DurationSec 30 -MaxPending 30                   # 페이싱
.\run_wall_dist.ps1 -DurationSec 30 -MinIntervalMs 1
.\run_wall_dist.ps1 -DurationSec 30 -VideoEscape external            # escape(158ms 이상치 재확인)
```

---

# ★★2026-08-24 (8) — 30fps 돌파: `gfx_wall_frame_min_interval_ms` 가 지배적이었다★★

pref 체계 배포본으로 4-GPU 실기 6 회 측정. **모든 실행에서 네 타일 fps 가 균등**했다.

| pref (BASE 외 추가분) | vsync | 타일별 fps |
|---|---|---|
| (없음) | on | 23.4 |
| `gfx_wall_frame_max_pending=30` | off | 23.7 |
| ★**`gfx_wall_frame_min_interval_ms=1`**★ | off | ★**56.9**★ |
| `gfx_wall_frame_min_interval_ms=1` | on | 49.2 |
| `max_pending=30` + `min_interval_ms=1` | on | **23.0** (더하면 나빠진다) |
| `gfx_video_escape_mode=external` | off | 1.5 (아래 회귀) |

★**기본값 `gfx_wall_frame_min_interval_ms=16` 이 병목이었다.**★ 이름상 62.5fps 상한이지만
실제로는 23fps 에 묶고 있었다. 1ms 로 낮추면 **56.9fps** — 30fps 목표를 크게 넘긴다.
`max_pending` 은 이 조합에서도 여전히 **해롭다**(56.9 → 23.0).

★이전 페이싱 측정이 전부 무효였다는 판단이 맞았다★ — 3/4 타일이 고장난 상태에서
`min_interval=1` 은 5.6fps 로 "쓸모없음" 처럼 보였다.

## ★escape=external 회귀 — 내가 만든 것이다★

```
링 생성 device 분포 (escape 실행):
    360 device=0x2695f28ff00   ← gpu 0 에 36 영상 x 10 회 재생성(churn)
      9 / 9 / 9                ← 나머지 GPU 는 정상
wrap OK = gpu 0 하나뿐,  fps 1.5
정상 실행: 9 / 9 / 9 / 9
```

원인: escape 경로(`MediaVideoExternalSurfaceProvider::acquire`)에서 디바이스를 구할 때 쓴
**스레드 로컬 메모 `this_thread_d3d11_device`**. *"acquire 는 그 painter 의 렌더러
스레드에서만 불린다"* 는 전제를 주석에 **"미검증"이라 적어 두고** 넣었는데, 실기에서
성립하지 않았다. 모든 타일의 수요가 painter 1 의 디바이스로 몰려 링이 churn 했다.

**수정**: `RenderingContext` 에 `media_d3d11_device_handle_borrowed()`(AddRef 없는 식별자용)
를 추가하고 escape 경로가 매 프레임 `rc` 에서 직접 구하도록 했다. AddRef 판을 프레임마다
부르면 refcount 가 새기 때문에 스레드 메모를 썼던 것인데, borrowed 판이 그 문제 없이 정확하다.

검증(단일 GPU): escape=external + min_interval=1 → **링 36 개(churn 소멸)**, EGLImage 0,
panic 0, 38.8fps. ★분산 자체는 4-GPU 실기에서만 확인 가능하다.★

## 다음 실기 확인

```powershell
.\run_wall_dist.ps1 -DurationSec 30 -MinIntervalMs 1                        # 56.9 재현 확인
.\run_wall_dist.ps1 -DurationSec 30 -MinIntervalMs 1 -VideoEscape external  # 링 9/9/9/9 확인
```

escape 를 켜면 **두 가지를 함께** 봐야 한다: (1) 링이 9/9/9/9 로 분산되는지, (2) 앞서 미해결로
남은 **external 승격 158ms 이상치**가 재현되는지(당시는 1-GPU 상태의 관측이라 재확인 필요).

---

# 2026-08-25 — 디코더 CPU: `media_video_sink_qos` 신설

## 관측

같은 장비에서 운영 월 솔루션은 같은 영상 54 개를 CPU 여유를 남기고 재생하는데, 이 빌드는
**42 개까지만 정상이고 45 개부터 CPU 100% 로 무너진다.**

★두 가지를 먼저 정정한다★:

1. **팬아웃 가설 반증.** 1 타일과 4 타일이 **둘 다** 45 개에서 포화된다. 타일 수만큼
   곱해지는 페인트 비용(`update_images` x painter 수)은 원인이 아니다. 그리고 앞선 4 타일
   조사와 달리 **총 CPU 가 100% 에 도달**한다 — 단일 스레드 병목이 아니라 전 코어 포화다.
2. **운영 월 솔루션은 Servo 기반이 아니다**(사용자 확인). 공통점은 GStreamer 파이프라인 +
   ffmpeg 계열 element 뿐이다. 따라서 "대조군 A(b0a7ce85) vs 운영 월" 로 코드/런타임을
   가르려던 앞 절의 제안은 **성립하지 않는다 — 폐기한다.** 54 vs 42 는 서로 다른 두
   애플리케이션의 차이라, 디코더가 아니라 Servo 의 프레임당 작업 어디든 원인일 수 있다.

## avdec 에 지금 설정하는 것

`max-threads` **하나뿐**이다(`configure_software_decoder_threads`). 그리고 싱크는:

```
policy=smooth max_buffers=3 drop=false qos=false max_lateness_ns=-1 processing_deadline_ns=0
```

★`qos=false` 는 GStreamer 요소 기본값이 아니라 **이 포크가 명시적으로 끄는 값**이다★
(`render.rs` 의 `appsink.set_property("qos", ...)`). qos 가 꺼져 있으면 QoS 이벤트가 상류로
가지 않아 **avdec 이 부하 상황에서 프레임을 건너뛰지 못한다** — 한계를 넘는 순간 완만한
열화가 아니라 절벽으로 무너진다(42 -> 45 거동과 일치).

`max-threads` 는 **올리면 안 된다** — 원장 §3-a: 자동 스레딩은 디코더당 코어 수만큼 스레드를
만들어 36 타일에서 700+ 스레드로 스케줄러를 무너뜨렸다.

## `media_video_sink_qos` (신설)

기존 `media_video_sink_policy` 로도 qos 를 켤 수 있지만 **네 값이 한꺼번에 바뀐다**:

| 정책 | qos | drop | max-lateness | max-buffers |
|---|---|---|---|---|
| Smooth(기본) | false | false | -1 | 3 |
| LowLatency | true | **true** | **16ms** | **1** |

qos 만 재려면 변수 넷이 섞이므로 독립 노브를 만들었다.

```
--pref media_video_sink_qos=on    # qos 만 on (drop/lateness/buffers 는 Smooth 그대로)
--pref media_video_sink_qos=off   # 강제 off
(빈 값)                            # 정책값 따름 = 종전 동작
```

런처: `run_wall_dist.ps1 -SinkQos on` (그리고 정책 자체는 `-SinkPolicy low-latency`).

**검증**(개발기, 9 개 비포화): 나머지 값 고정된 채 `qos=false` -> `qos=true` 만 바뀜을 로그로
확인. fps 는 74.7 vs 72.2 로 차이 없음 — **부하가 걸려야 개입하므로 당연하다.**

## 실기 판정

```powershell
.\run_wall_dist.ps1 -Layout wall_layout.singlegpu.json -Rows 9 -Cols 5 -MinIntervalMs 1 -DurationSec 30
.\run_wall_dist.ps1 -Layout wall_layout.singlegpu.json -Rows 9 -Cols 5 -MinIntervalMs 1 -DurationSec 30 -SinkQos on
```

| 결과 | 뜻 | 다음 |
|---|---|---|
| CPU 내려가고 재생 유지 | 디코더가 스킵으로 부하를 던다 | 54 개까지 밀어붙일 여지 |
| CPU 그대로, 프레임만 드롭 | 디코드가 이미 최대치 | 절감이 아니라 열화 완화 |
| 차이 없음 | **QoS 가 avdec 까지 전달되지 않는다** | `skip-frame` 직접 설정 / 파이프라인 구조 확인 |

세 번째가 실재한다 — QoS 이벤트는 싱크에서 상류로 전파되는데 중간 요소나 `sync=false` 가
끊을 수 있다. 그때는 avdec 의 `skip-frame`(AVDISCARD)을 직접 거는 쪽으로 가야 한다.

★qos 실험이 무효로 나오면 다음은 디코드 CPU 자체를 분리 측정하는 것이다★ — 같은 45 개를
렌더 경로 없이 디코드만 돌려(예: fakesink) Servo 의 프레임당 작업과 디코드를 갈라야 한다.
지금은 둘이 섞여 있어 "디코더가 비싼지" 자체가 미확정이다.

---

# 2026-08-25 (2) — 디코드/렌더 분리 측정 도구

## 관측이 가리키는 방향

- qos=on 이면 CPU 는 확실히 내려가지만, **운영 월 대비 여전히 높고** qos 가 개입하는 영상은
  정의상 fps 가 정상일 수 없다. **근본 해결책이 아니다**(사용자 판단, 동의).
- ★**영상 수가 늘면 CPU 는 한계까지 오르는데 GPU 점유율은 오히려 크게 떨어진다.**★
  렌더할 내용이 도착하지 않는 것처럼 보인다.

→ 병목은 **GPU 상류**(디코드 + CPU 측 프레임 작업)다. GPU 는 굶고 있다.

문제는 월 안에서는 **디코드 비용과 Servo 의 프레임당 작업이 섞여** 측정된다는 것이다.
"디코더가 비싼가" 자체가 아직 미확정이다.

## `etc/multigpu/tools/measure_decode_only.ps1`

Servo 를 완전히 배제하고 **순수 디코드 비용의 바닥값**을 잰다.

```
filesrc ! qtdemux ! h264parse ! avdec_h264 max-threads=N ! fakesink sync=true
```

업로드도 합성도 표시도 없다. 월과 같은 개수로 돌리면 뺄셈이 성립한다:

```
바닥값            = N 개 디코드 자체 비용 (이 도구)
월 총량 - 바닥값  = Servo 가 프레임당 추가하는 비용 (업로드/이미지 팬아웃/씬/present)
```

`sync=true` 가 기본이다 — 월은 30fps 로 재생하므로 최대속도 디코드는 비교 대상이 아니다.
`-MaxThreads` 로 avdec 스레드 수를, `-GstRoot` 로 **다른 GStreamer 런타임**(1.22.8 vs
1.28.4.100)을 지정해 런타임 축도 같은 방법으로 가를 수 있다.

## 개발기 참고값 (24 논리코어, RX 580 x1)

```
9 개 디코드 = 1.86 코어 (영상당 0.206 코어), 머신 전체 9.2%
-> 45 개 외삽 약 9.3 코어 = 24 코어의 약 40%
```

즉 이 하드웨어에서는 **디코드만으로 45 개가 CPU 를 채우지 못한다.** 그쪽 장비에서 같은 수를
재야 확정이지만, 사실이라면 **나머지가 Servo 의 프레임당 작업**이고 avdec 튜닝은 표적이 아니다.

## 실기 측정 순서

```powershell
# 1) 바닥값 - Servo 없이 45 개 디코드만
etc\multigpu\tools\measure_decode_only.ps1 -Count 45

# 2) 같은 45 개를 월에서 (단일 타일, qos 기본)
.\run_wall_dist.ps1 -Layout wall_layout.singlegpu.json -Rows 9 -Cols 5 -MinIntervalMs 1 -DurationSec 30
```

| 1 번 결과 | 뜻 | 다음 표적 |
|---|---|---|
| 코어 대부분을 이미 소모 | 디코드가 진짜 벽 | avdec/런타임(1.22.8 비교), 하드웨어 디코드 |
| 여유가 크다(예: 40%) | ★**Servo 의 프레임당 작업이 나머지를 먹는다**★ | 업로드(memcpy) / 이미지 팬아웃 / 씬 빌드 |

2 번이 1 번보다 크게 높다면 그 차이가 곧 Servo 오버헤드다. 그때 후보는 순서대로:
CPU memcpy 업로드(영상당 프레임당 FHD I420 약 3.1MB), `update_images` 팬아웃,
painter 별 씬 빌드. `media_d3d11_enabled=false` 로 업로드 경로를 바꿔 보는 것이
그중 업로드 축을 가르는 가장 값싼 A/B 다.


## 2026-08-25 — 업로드 memcpy 반증, 그리고 스레드별 CPU 계측

### 실기 결과: 디코드는 45 개에서 30%

위 "실기 측정 순서" 의 1 번이 그쪽 장비에서 나왔다.

```
measure_decode_only.ps1 -Count 45  ->  CPU 약 30%
```

표의 **"여유가 크다"** 분기다. 디코드는 벽이 아니고, 나머지 약 70% 가 Servo 의 프레임당
작업이다. avdec 튜닝은 표적이 아니므로 종결한다.

### 반증: 업로드 memcpy 도 벽이 아니다

표에서 첫 후보로 적어 둔 "업로드(memcpy)" 는 두 근거로 탈락한다. ★이 절은 그날 잘못
세웠던 가설("copy 가 media stage 의 98% 이니 복사를 GPU 카피 엔진으로 옮기자")의
정정이다.★

**(1) 대조군이 같은 방식을 쓴다.** 같은 장비에서 같은 영상을 54 개 여유롭게 재생하는
운영 월 솔루션(`D:\Project\ViewFlex30\Channel\Common\Common.RendererY\RendererImpl`,
Servo 기반이 아니고 gstreamer + ffmpeg 계열 디코드만 공통)의 `D3D11YV12TextureWorker` 는
이 포크와 사실상 같은 코드다.

| | 대조군 | 이 포크 (`ring_producer::copy_planes`) |
|---|---|---|
| plane 텍스처 | Y/U/V 3 장 | 같음(슬롯당) |
| 포맷 | `A8_UNORM` | `R8_UNORM` |
| Usage / CPUAccess / Misc | `DYNAMIC` / `WRITE` / `0` | 같음 |
| Map 플래그 | `WRITE_DISCARD` | 같음 |
| 복사 | 행 단위 memcpy, RowPitch 보정 | 같음 |
| 프레임당 Map/Unmap | 3 + 3 | 3 + 3 |

Servo 는 링 슬롯이 4 개라 텍스처 수는 더 많지만, free 슬롯을 계속 map 상태로 두므로
프로듀서는 Map 호출이 없고 소비자가 `ConsumePlan::Advance` 에서 3 Unmap + 3 Map 을 한다.
**프레임당 D3D11 호출 수와 복사 바이트 수가 대조군과 같다.**

주의: 대조군이 `_marginedWidth/_marginedHeight` 만 복사하는 것은 다운스케일이 아니라
**영상 표출 영역이 GPU 경계에 걸칠 때의 분할 업로드용 사전 계산값**이다. 한 GPU 영역에
온전히 표출되는 통상 케이스에서는 전체 프레임을 복사한다 — 복사량 이점이 아니다.

**(2) 산술이 안 맞는다.** 개발기 D3D11PROF(9 개 재생) 기준 `copy` p50 = 2.33ms/영상/프레임.

```
45 영상 x 30fps x 2.33ms  =  약 3.1 코어-초/초
```

게다가 이 복사는 **파이프라인별 GStreamer 스트리밍 스레드에서 병렬로** 일어난다
(`render-d3d11/lib.rs` — "build_frame 은 스트리밍 스레드 1 개에서만"). 디코드 30% 와
합쳐도 포화에 한참 못 미친다. `copy` 가 media stage 의 98% 인 것은 사실이지만,
**media stage 자체가 전체 CPU 의 작은 조각**이다.

### 남은 표적

대조군은 "쿼드 N 개에 SRV 3 장씩 물려 그리는 D3D11 렌더러"라 프레임당 비용이
memcpy + SRV 바인딩 + draw 로 끝난다. 이 포크는 프레임마다 external image 갱신 ->
더티 -> 디스플레이 리스트/씬 재구성 -> 합성을 돈다. 45 영상 x 30fps = **초당 1350 회**가
소수의 단일 스레드에 직렬로 몰린다. 관측된 "video 수가 늘어 fps 가 떨어지면 GPU 점유율이
오히려 감소한다(렌더링할 내용이 안 오는 것처럼)"가 정확히 이 모양이다.

다만 이것도 아직 **가설**이다. 이 조사에서 추론만으로 좁혔다가 오귀인한 전례가 여럿이므로
(위 반증 목록 참조) 다음은 계측이다.

### 계측: `thread_cpu_probe`

프로세스의 CPU 를 **스레드 이름별로 귀속**시킨다. 45 개 디코드 스레드(병렬, 정상)인지,
Compositor/Renderer/Script 같은 **소수 단일 스레드가 1.0 코어에 붙어 천장**인지를 바로
가른다. 후자가 곧 "GPU 가 굶는다"의 실체다.

- 도구: `etc/multigpu/tools/thread_cpu_probe` (독립 크레이트, 엔진 변경 없이 외부 관측)
- 스레드 이름: Rust std 가 Windows 에서 `SetThreadDescription` 을 설정하므로 엔진
  스레드는 그대로 읽힌다. GStreamer 스트리밍 스레드는 GLib/C 스레드라 이름이 없어서
  엔진이 첫 appsink 콜백에서 스스로 태깅한다
  (`components/media/backends/gstreamer/thread_name.rs`, `ServoGstVideo-N` /
  `ServoGstAudio-N`). 같은 지점에서 `THREADMAP tid=<n> name=<n>` 도 남기므로 로그만으로도
  대조가 된다.
- 검증(2026-08-25): 의도적으로 2 스레드를 돌린 테스트 프로그램에서 스레드 합계 1.99 코어 =
  `GetProcessTimes` 1.99 코어로 일치, 이름·SATURATED 표시 정상.

```powershell
# 월 실행에 붙여서 (dist 런처)
.
un_wall_dist.ps1 -Layout wall_layout.singlegpu.json -Rows 9 -Cols 5 -MinIntervalMs 1 `
    -DurationSec 40 -ThreadCpu

# 이미 떠 있는 프로세스에 수동으로
engine	hread_cpu_probe.exe --duration 20 --top 20
```

읽는 법:

| 나온 그림 | 뜻 | 다음 표적 |
|---|---|---|
| `ServoGstVideo` 가 코어 대부분, hottest 낮음 | 디코드/업로드가 병렬로 다 먹는 중 | 업로드 축(`media_d3d11_enabled=false` A/B), 하드웨어 디코드 |
| 단일 스레드 하나가 `SATURATED` | ★그 스레드가 천장★ — GPU 가 그 뒤에서 굶는다 | 그 스테이지의 프레임당 작업(씬 재구성/`update_images` 팬아웃/합성) |
| 어느 것도 아니고 총합이 낮다 | 프로세스 밖(드라이버/커널)에 비용 | ETW, GPU 드라이버 스레드 |


## 2026-08-25 실측 — ★천장은 디코드 스레드다. 합성 가설 반증★

`wall_20260825_142322` (테스트 장비, 45 영상, 단일 타일, 30s 샘플).

```
  cores   %cpu    n  hottest  thread group
   44.67  ...    180     0.99  multiqueue:src   <-- SATURATED
    5.66  ...     45     0.14  ServoGstVideo
    0.89  ...     45     0.03  qtdemux:sink
    0.77  ...      1     0.77  main
    0.30  ...      1     0.30  Script
    0.07  ...      1     0.07  WRRenderBackend
    0.03  ...      1     0.03  WRSceneBuilder
    0.01  ...      1     0.01  Constellation
   52.69                       TOTAL = process total (일치)
```

읽는 법대로 읽으면 결론은 하나다.

- **디코드가 전체 CPU 의 85%** (`multiqueue:src` = decodebin3 에서 `avdec_h264` 의 chain 이
  도는 스레드). 개별 스레드 20 개 이상이 정확히 **0.99 코어에 붙어 있다**.
- **업로드(이 포크의 D3D11 memcpy)는 5.66 코어 = 11%.** 앞 절의 산술 추정(약 3.1 코어)과
  같은 자릿수이고, 벽이 아니다.
- ★**브라우저/렌더 측 전체가 1.2 코어 미만(약 2%)이다.**★ WebRender 는 사실상 놀고 있다.
  앞 절에서 세운 "프레임마다 씬 전체 재합성이 병목" 가설은 **반증됐다.**
- 벽은 45 fps 로 present 중인데 소스는 30fps 다. **렌더가 밀리는 상황이 아니다.**

파이프라인은 `h264parse -> avdec_h264 -> appsink` 뿐이고 `videoconvert`/`videoscale` 은
없다(로그의 element added 45/45/45). `max-threads=1` 도 45 개 전부 적용됐다.

### 확인: `multiqueue:src` 가 정말 디코드 스레드인가

"영상 하나당 파이프라인 하나인데 multiqueue 가 들어갈 구석이 있는가" 는 당연한 의문이라
따로 확인했다. **파이프라인은 영상당 하나가 맞다. 그 하나의 playbin3 안에 multiqueue 가
2 개 들어 있다.** 같은 클립을 `playbin3` 로 재생하며 `GST_DEBUG_DUMP_DOT_DIR` 로 그래프를
받아 확인한 결과(2026-08-25, 개발기):

```
playbin3
+-- urisourcebin0 ---- multiqueue0
+-- decodebin3 --- parsebin0 --- multiqueue1 -+- src_0  video/x-h264 1920x1080 30000/1001  [T]
                                              +- src_1  audio/mpeg                        [T]
```

`multiqueue1` 은 parsebin 바로 뒤, **디코더 바로 앞**이고 src 패드마다 태스크(`[T]`)를 갖는다.
GStreamer 에서 디코더는 자기 스레드가 없고 **업스트림 src 패드 태스크 위에서 chain 함수가
돈다.** 즉 `video/x-h264` 를 나르는 `multiqueueN:src_M` 이 곧 `avdec_h264` 가 도는 스레드다.

수치도 맞아떨어진다.

| | |
|---|---|
| 영상당 multiqueue | 2 개 (urisourcebin + decodebin3) |
| multiqueue 당 src 패드 | 2 개 (video, audio) |
| 영상당 패드 태스크 | 4 개 |
| **45 영상 x 4** | **180** = 프로브가 센 스레드 수 |
| 그중 video 를 나르는 것 | 영상당 1 개 = 45 개 |
| 44.67 코어 / 0.99 | **약 45** = 포화된 스레드 수 |

현장 로그의 element 인덱스가 `multiqueue84` 까지 올라간 것도 90 개(45x2) 인스턴스와 맞는다.
(패드 번호는 파일마다 스트림 순서가 달라 video 가 `src_0` 일 때도 `src_1` 일 때도 있다.)

★함정: 위 "element added" 목록은 전체 목록이 아니다.★ `should_log_pipeline_element` 는
factory 에 `dec`/`convert`/`scale` 등이 들어가거나 klass 가 Decoder/Converter/Sink 인 것만
찍는데 `multiqueue` 는 klass 가 Generic 이라 **애초에 로그에 나오지 않는다.** 다만
`videoconvert`(->`convert`)와 `videoscale`(->`scale`)은 그 필터에 걸리므로,
"변환/스케일 요소는 없다" 는 결론 자체는 유효하다.

### 진짜 이상한 점: 디코드 단가가 부하에 따라 변한다

| 조건 | `multiqueue:src` 영상당 |
|---|---|
| 개발기, 9 영상 | **0.23 코어** (포화 아님) |
| 테스트 장비, 45 영상 | **0.99 코어** (포화) |

같은 빌드, 같은 페이지, 같은 pref 다. 디코드가 원래 1 코어씩 먹는 게 아니라
**밀리기 시작하면 무제한으로 먹는다**. 42 개까지 정상이고 45 개부터 절벽으로 무너지는
거동, qos=true 로 내려가는 거동, fps 가 떨어질 때 GPU 점유율이 **오히려** 떨어지는 거동이
전부 여기서 설명된다.

### 가설 (아직 검증 전): 되돌아올 길이 없는 설정

```
sink   : policy=smooth  qos=false  drop=false  max-lateness=-1  (release valve 없음)
clock  : release_sync_group() 가 set_start_time(NONE) + 공유 base_time + 공유 SystemClock
```

`start_time = NONE` 이면 running time 이 벽시계에 고정되고 **다시 base 를 잡지 않는다.**
거기에 qos/drop 이 꺼져 있고 max-lateness 가 무제한이면, 한 번 밀린 파이프라인은
밀린 만큼을 **전속력으로 디코드해서 따라잡으려 한다.** 9 개면 순식간에 따라잡고 0.23 으로
돌아오지만, 45 개면 따라잡는 데 필요한 총량이 장비를 넘어서므로 **영원히 못 따라잡고
전부 1 코어에 붙는다.** 양의 되먹임이라 점진적 열화가 아니라 절벽이 된다.

★이건 추론이다. 이 조사에서 추론만으로 좁혔다가 틀린 게 이번이 세 번째이므로 계측으로
확인한 뒤에만 손댄다.★

### 다음 실험 (순서대로)

1. **램프 — 가장 결정적.** 같은 장비에서 20 / 30 / 36 / 42 / 45 개를 `-ThreadCpu` 로.
   영상당 디코드 단가가 0.25 근처를 유지하다 어느 지점에서 튀면 부하 의존성이 확정된다.
   (새 스위치 필요 없음.)
2. **`-NoSyncGroup`** 45 개. 공유 base time 을 빼면 단가가 내려가는가.
   주의: 개발기 A/B 에서 `ServoGstVideo` 가 0.43 -> 0.02 로 떨어지는 미확인 현상이 있었다.
   렌더/present 는 양쪽 다 정상이었으나, 이 실험 결과를 읽을 때 업로드 열도 같이 볼 것.
3. `measure_decode_only.ps1 -Count 45` 를 다시 돌려 **"cores busy" 줄**을 확보한다.
   (앞서 받은 값은 "30%" 뿐이었다. 이게 throttle 된 바닥값이고, 위 0.99 와의 비가 곧
   "얼마나 폭주하고 있는가"다.)

### 도구 결함 정정 (2026-08-25)

- `thread_cpu_probe` 의 `%cpu` 가 **131.7%** 로 찍혔다. `available_parallelism()` 이 Windows
  에서 **현재 프로세서 그룹만** 세기 때문이다(40 이라고 보고했으나 프로세스는 실제로 52.69
  코어를 씀 = 멀티 그룹 장비). `GetActiveProcessorCount(ALL_PROCESSOR_GROUPS)` 로 교체했다.
  **`cores` 열은 처음부터 옳았고, 위 표의 결론은 영향받지 않는다.**
- `Tee-Object` 가 PowerShell 5.1 에서 UTF-16 으로 저장해 `.threads.txt` 가 grep 에서 깨졌다.
  ASCII 로 명시 저장하도록 변경.


## 2026-08-25 실측 2 — ★디코더는 폭주가 아니라 절반 속도로 뒤처진다★

`wall_20260825_153821` (테스트 장비 80논리/40물리, 45영상, `-VideoRate -ThreadCpu`).

```
sink : sync=true async=true          <- appsink 는 정상적으로 클럭에 동기화된다
VIDEORATE : pts_rate 중앙값 약 0.55x,  fps 16~19 (중앙 약 17), wrapped 0
thread    : multiqueue:src 44.17 코어 / 180 스레드 / hottest 0.98 SATURATED
present   : 9~14 fps
```

### 반증: "싱크가 throttle 을 못 해 디코더가 앞질러 돈다"

앞 절에서 유력하게 봤던 가설이다. **틀렸다.** `pts_rate` 가 0.55x 이므로 디코더는
앞지르는 게 아니라 **재생 속도의 절반밖에 못 따라간다.** `sync=true` 도 로그로 확인됐다.
프레임을 더 많이 만들어서 CPU 를 쓰는 게 아니다.

### 그러면 남는 것은 프레임당 비용이고, 그 배수가 크다

| | 코어 | fps | 프레임당 CPU |
|---|---|---|---|
| 월 (45영상) | 0.98 | 약 17 | **57.6 ms** |
| 같은 장비, 단독 1개 무제한 | 1.00 | 84 | **11.9 ms** |

**같은 장비 같은 클립인데 월 안에서는 프레임당 디코드 비용이 약 4.8 배다.**
단독 단가로 계산하면 45개 30fps 재생에 필요한 양은 16.1 코어인데, 월은 44.2 코어를
쓰고도 17fps 밖에 못 낸다.

### 아직 안 풀린 것: 머신 100% vs 프로세스 68%

사용자 관측으로는 실행 중 머신 CPU 가 100% 였는데 프로세스는 54.5/80 = 68% 다.
**약 25 코어가 어디로 가는지 이 측정에는 안 잡힌다.** 다른 프로세스인지, GPU 드라이버인지,
프로세스 밖 커널 작업인지에 따라 해석이 완전히 달라진다. `thread_cpu_probe` 에
`GetSystemTimes` 기반 머신 전체 라인을 추가해 다음 실행부터 프로세스/머신 비율이 함께
찍히게 했다.

### 다음 실험

1. ★**고친 `measure_decode_only.ps1 -Count 45`**★ — 이게 정확히 남은 갈림길이다.
   45개를 Servo 없이 동시에 돌렸을 때도 영상당 단가가 0.36 에서 1.0 근처로 부풀면
   **장비의 동시성 한계**(메모리 대역/SMT/NUMA)이고 Servo 는 무죄다. 0.36 근처를
   유지하면 **월이 프레임당 비용을 4.8 배로 만들고 있는 것**이고 표적은 코드다.
   (직전까지 이 측정이 계속 무효였던 이유는 `Get-Counter` 가 호출당 1초를 먹는 루프를
   반복 횟수로 돌려 창이 2배가 됐기 때문이다. 시계 기준 루프로 고쳤다.)
2. **20영상에서 `-VideoRate`** — 그 지점은 총 17코어/물리 40코어라 경합이 없다.
   거기서 `pts_rate` 가 1.00x 면 프레임당 단가는 0.553/30 = 18.4ms 로 단독 대비 1.55 배,
   즉 경합 없이도 이미 부풀어 있다는 뜻이 된다. 부풀기 곡선의 출발점을 잡는 값이다.


## 2026-08-25 실측 3 — ★같은 명령이 두 상태로 갈린다★ + 앞선 측정 오염

### 먼저: 이전 측정들이 오염돼 있었다

테스트 장비에서 **대조군 월의 capture/stream 프로세스 등이 같이 돌고 있었다.** 그것들을
종료하자 CPU 100% 포화는 사라졌다. 따라서 앞 절의 "머신 100% vs 프로세스 68%" 간극은
설명됐고, ★그 이전 램프(20/36/42/45) 수치도 전부 오염된 상태에서 잰 것이므로 절대값으로
신뢰하면 안 된다★. `thread_cpu_probe` 에 `GetSystemTimes` 기반 **머신 전체 라인**을 넣어
다음부터는 프로세스/머신 비율이 항상 함께 찍히게 했다 — 이 사고를 다시 겪지 않기 위함이다.

### 정리 후: 동일 명령 8회가 두 무리로 갈린다

`-Rows 9 -Cols 5 -MinIntervalMs 1 -DurationSec 40 -VideoRate -ThreadCpu` 8회 반복.

| | pts_rate | fps | present | 디코드 코어 | hottest | 업로드 | 총 |
|---|---|---|---|---|---|---|---|
| 좋음 3회 | 0.89~0.94 | 27~28 | 27~29 | 44.8 | 0.99 | 5.5~6.4 | 52~53 |
| 나쁨 5회 | 0.59~0.62 | 18 | 6.5~15.6 | 44.4 | 0.98 | 7.7~8.7 | 55~56 |

**디코드 CPU 는 양쪽이 같다(44.1~44.9, hottest 0.98~1.00). 같은 CPU 를 쓰고 산출만 다르다.**
프레임당으로 환산하면 디코드 35ms vs 54ms, 업로드 4.4ms vs 10.6ms — **둘이 함께** 나빠진다.
한 스테이지의 버그가 아니라 메모리 접근 비용이 전반적으로 오르는 모양이다.

★상태는 첫 1초 안에 정해지고 끝까지 변하지 않는다★ (초별 pts_rate 중앙값: 좋은 실행은
0.96 에서 시작해 유지, 나쁜 실행은 첫 창 0.72 뒤 즉시 0.60 에 안착). 열화도 회복도 없다.
= 누적 현상이 아니라 **기동 시점의 배치 결정**이다.

로그 구조 차이는 없다: 같은 어댑터/LUID, d3d11 141/141, direct_file 45/45, EGLImage 실패 0,
fan-out 경고 0, panic 0, sync group 1 회 해제. 총 CPU 도 52~56/80 이라 머신 포화가 아니다.

### 유력 후보: 프로세서 그룹 배치

80 논리 CPU 는 64 를 넘으므로 Windows 가 **프로세서 그룹 2 개**로 쪼갠다. 스레드는 한
그룹 안에서만 스케줄되고, 프로세스가 두 그룹에 걸치면 메모리 트래픽이 인터커넥트를 넘는다.
이 배치는 기동 시 정해지고 프로세스 수명 내내 유지된다 — 관측된 "첫 1초에 결정, 이후 불변,
CPU 동일한데 처리량만 다름"과 정확히 같은 모양이다.

`thread_cpu_probe` 가 이제 이를 직접 보고한다.

```
  topology: 80 logical cpus in 2 processor group(s) [40+40], N NUMA node(s)
  processor group placement (a split process pays for cross-group memory traffic):
    group 0 : 300 threads   28.10 cores
    group 1 : 246 threads   26.40 cores
```

★단 이건 아직 가설이다.★ 좋은 실행과 나쁜 실행의 배치가 실제로 다른지 확인한 뒤에만
손댄다.

### 그리고 배치로 설명되지 않는 잔여분

좋은 실행조차 프레임당 35ms 로, 같은 장비 단독 1 개(11.9ms)의 **3 배**다. 배치 차이는
좋음↔나쁨의 1.5 배를 설명할 뿐 이 기저 3 배는 설명하지 못한다. 다만 단독 측정은 유휴
장비에서 파이프라인 1 개를 돌린 최상 조건이므로, **45 개 동시 실행 자체의 비용**이 얼마인지
아직 모른다. 그게 아래 1 번이다.

### 다음 실험

1. ★**고친 `measure_decode_only.ps1 -Count 45`**★ — 45 개 동시 실행의 기저값. 영상당 단가가
   0.36 근처면 월이 3 배를 얹는 것이고, 1.0 근처로 부풀면 장비의 동시성 한계다.
2. **같은 명령 4~6 회 반복**, 새 프로브로. 좋은 실행과 나쁜 실행의 그룹 배치가 갈리는지.
   갈린다면 다음은 배치를 고정하는 실험이고, 안 갈린다면 이 가설도 버린다.


# ★2026-08-25 결론 — 45영상 CPU 천장의 원인은 GStreamer 파이프라인 구성이다★

앞 절들은 이 결론에 도달하기까지의 과정이고 **대부분이 반증된 가설**이다. 이 절만 읽으면
된다. 나머지는 같은 길을 다시 가지 않기 위한 기록이다.

## 결론

45 x FHD30 을 재생할 때 영상당 디코드 비용이 **0.39 코어여야 하는데 0.98 코어**다.
그 차이는 전부 **파이프라인을 어떻게 구성했는가**에서 온다. Servo 코드가 아니다.

측정(테스트 장비 80 논리 / 40 물리, 45 x FHD30, 30fps 기준으로 정규화):

| 토폴로지 | 페이싱 | 코어/영상 |
|---|---|---|
| plain (`filesrc ! qtdemux ! h264parse ! avdec ! sink`) | 클럭 없음 | **0.39** |
| plain | 공유 클럭(`sync=true`) | 0.864 |
| 월 (`... ! multiqueue ! avdec ! queue ! sink`) | 클럭 없음 | 0.729 |
| 월 | 공유 클럭 | 0.968 |
| **Servo 월 (playbin3)** | | **0.98** |

**Servo 가 얹는 몫은 약 1%다**(0.968 -> 0.98). 나머지는 둘로 나뉜다.

- **공유 `GstSystemClock`** — `obtain()` 은 프로세스당 싱글턴이라 45 개 싱크가 프레임마다
  같은 객체에서 대기한다. plain 에서 +0.47, 월 토폴로지에서 +0.24.
  ※ 45 개를 **별도 프로세스**로 돌리면 0.399 다. 즉 클럭 대기 자체가 아니라 **공유**가 문제다.
- **playbin3 의 multiqueue + queue** — 클럭 없을 때 +0.34, 있을 때 +0.10.
  디코드된 **3.1MB 원본 프레임이 스레드 경계를 두 번 더 건넌다**(multiqueue src 태스크 ->
  queue src 태스크). 한 코어가 쓴 3.1MB 를 다른 코어가 읽는 일이 프레임마다 두 번 추가된다.

0.98 x 45 = 44 코어인데 이 장비는 물리 40 코어다. 그래서 30fps 를 못 지키고 `pts_rate` 가
0.90 으로 떨어진다. 대조군(비 Servo 월 솔루션)이 같은 장비에서 54 개를 50~70% 로 도는 것은
이 0.98 을 만들지 않기 때문이다.

## 반증된 가설 (각각 무엇이 반증했는지)

이 순서대로 틀렸다. 전부 그럴듯했고 전부 계측이 뒤집었다.

| # | 가설 | 무엇이 반증했나 |
|---|---|---|
| 1 | 디코더 설정(avdec)이 문제 | 단독 45 개 디코드 = 0.399 코어/영상. 디코더는 정상 |
| 2 | 프레임마다 씬 전체 재합성이 병목 | 스레드별 CPU: 브라우저/렌더 측 전부 합쳐 1.2 코어 미만, WebRender 는 사실상 유휴 |
| 3 | 업로드 memcpy 가 병목 | 대조군도 같은 map/unmap memcpy 를 쓴다(`D3D11YV12TextureWorker`). 업로드는 5.66 코어(11%) |
| 4 | 싱크가 throttle 을 못 해 디코더가 폭주 | `VIDEORATE` 실측 `pts_rate` = 0.55x. 앞지르는 게 아니라 **뒤처진다**. `sync=true` 도 로그로 확인 |
| 5 | 공유 base time(sync group)이 원인 | `-NoSyncGroup` 45 영상: 44.14 vs 44.16 코어. 차이 없음 |
| 6 | 프로세서 그룹 / NUMA 배치 | 대조군이 같은 장비에서 54 개를 여유롭게 돈다. 하드웨어는 충분 |
| 7 | 메모리 대역 / SMT 경합 | 45 개 동시 디코드가 단독 1 개 대비 **11%** 만 비싸다(0.399 vs 0.36) |
| 8 | 합성이 주사율의 3.3 배라 그게 원인 | 사실이긴 하나(20 영상에서 초당 198 합성) **45 영상에서는 합성이 18.5/s 로 오히려 붕괴**한다. 저부하 낭비와 고부하 붕괴는 별개 문제 |
| 9 | Servo 가 프레임당 4.3 배를 얹는다 | 같은 4.3 배가 **Servo 없이 gst-launch 만으로 재현**된다(1 프로세스 + 월 토폴로지 = 0.968) |

★8 번은 폐기가 아니라 **분리**다.★ 저부하에서 60Hz 화면에 초당 198 회 합성하는 것은 실재하는
낭비이고 별도로 고칠 가치가 있다. 다만 45 영상 붕괴의 원인은 아니다.

## 이 결론에 도달한 측정 순서

계측을 하나씩 만들어 가설을 하나씩 죽였다. 각 도구가 왜 필요했는지가 곧 순서다.

1. **`thread_cpu_probe`** (신설) — 프로세스 CPU 를 스레드 이름별로 귀속. 가설 2 를 죽였다.
   브라우저 측이 유휴이고 `multiqueue:src` 가 44 코어라는 것이 여기서 처음 보였다.
2. **`measure_decode_only.ps1`의 ceiling 모드** — 단독 1 파이프라인 무제한 디코드로
   `30 / ceiling_fps` = 1x 재생의 이론 단가를 얻는다. 이게 있어야 "0.98 이 비싼가"를 말할 수 있다.
3. **`VIDEORATE` + sink `sync` 로그** (엔진) — CPU 수치만으로는 "프레임을 더 많이 만든다"와
   "프레임당 비싸다"가 구분되지 않는다. 가설 4 를 죽였다.
4. **`FRAMEREASON`** (엔진, `#[track_caller]`) — 합성 요청 9 개 경로 중 누가 부르는지.
   가설 8 을 "실재하지만 별개 문제"로 정리했다.
5. **`-SingleProcess`** — 45 개를 한 프로세스에 넣기. 공유 클럭 비용이 여기서 드러났다.
6. **`-PaceWithSleep`** — 클럭을 쓰지 않는 페이싱. 그 비용이 클럭임을 확정.
7. **`-WallTopology`** — multiqueue + queue 를 명시적으로 끼우기. 가설 9 를 죽이고 결론에 도달.

## 다음 (미착수)

**direct-file 재생에서 playbin3 대신 최소 체인을 직접 구성한다** —
`filesrc ! qtdemux ! h264parse ! avdec_h264 ! appsink`. 토폴로지 비용과 클럭 비용이 함께
사라져 0.98 -> 약 0.39, 45 영상 기준 44 -> 18 코어가 목표다.
`media_direct_file_enabled` 가 이미 "파일을 직접 가리키는" 갈래로 존재하므로 거기 붙는다.
설계 갈림길: 코덱 자동 선택을 어디까지 포기할지, 시크/EOS/gapless 루프를 어떻게 직접 다룰지.

`media_video_sink_pacing=thread` 는 그 절반(클럭)을 이미 구현해 뒀지만 **지금은 appsink
콜백(= queue 뒤)에서 자므로 back-pressure 가 디코더까지 닿지 않는다.** 최소 체인이 되면
appsink 가 디코더와 같은 스레드가 되어 제자리를 찾는다. 그때까지는 기본값 `clock` 유지.

## 도구를 쓰다가 밟은 함정 (같은 실수 반복 방지)

- `measure_decode_only.ps1` 의 창이 **명목의 2 배**로 돌았다. `Get-Counter` 가 호출당 약 1 초를
  먹는데 그것을 반복 횟수로 돌렸기 때문. 30.07s 클립을 넘겨 전 파이프라인이 EOS 했고,
  결과가 **음수 코어**로 나왔는데 헤드라인 숫자로 찍혔다. 시계 기준 루프로 교체.
- `thread_cpu_probe` 의 `%cpu` 가 **131.7%** 로 찍혔다. `available_parallelism()` 이 Windows
  에서 현재 프로세서 그룹만 세기 때문. `GetActiveProcessorCount(ALL_PROCESSOR_GROUPS)` 로 교체.
- `-D3d11ProfileMs 0` 이 `-gt 0` 가드에 걸려 **조용히 무시**되고 엔진 기본 8ms 가 남았다.
  `$PSBoundParameters` 로 "전달 안 함"과 "0"을 구분하도록 교체.
- ★위 셋의 공통점: **실행이 자기 구성을 출력하지 않으면 무엇을 쟀는지 알 수 없다.**★
  그래서 도구들이 이제 토폴로지/페이싱/프로세스 수/임계값을 매 실행 헤더에 찍는다.
- 측정 중 **테스트 장비에서 대조군 월의 capture/stream 프로세스가 같이 돌고 있었다.**
  그것을 끄기 전 수치(램프 20/36/42/45 등)는 절대값으로 신뢰하면 안 된다. 그래서
  `thread_cpu_probe` 가 `GetSystemTimes` 기반 **머신 전체 라인**을 항상 함께 찍는다.
- 기준선을 **월과 다른 토폴로지**로 재고 같은 것으로 취급했다. 월의 0.98 이 단일 프로세스
  기준선 0.795 보다 높다는 것이 신호였는데 "Servo 가 얹는 비용"으로 읽었다. 도구가 이제
  `(plain; NOT what the wall runs)` 를 명시한다.


## 2026-08-26 — 월의 실제 파이프라인, 그리고 기각된 오디오 가설

### 실측한 요소 목록 (영상 1 개당)

로그 필터가 `queue`/`multiqueue` 를 빼고 있어서(klass 가 Generic) **디코더 뒤에 큐가 있는지조차
로그로 알 수 없었다.** 필터를 전부 찍도록 넓힌 뒤 받은 실물:

```
filesrc -> typefind -> qtdemux -+-> h264parse -> capsfilter -> multiqueue -> avdec_h264
                                |                                              |
                                |                                    [vbin: vqueue]   <- ***비싼 홉***
                                |                                              |
                                |                                          appsink
                                +-> aacparse -> audiotee              (오디오: 파싱만, 디코드 안 함)
identity x2  (streamsynchronizer 의 스트림당 identity)
```

- ***`vqueue` 확정*** — playsink 소유의 비디오 큐. 원본 3.1MB 프레임이 스레드 경계를 건너는
  자리이고 `uridecodebin3` 교체가 없애는 것이 정확히 이것이다.
- `videoconvert`/`videobalance`/`deinterlace` 는 ***없다*** — `prefer_native_video` 가
  `native-video` 를 켜고 두 필터를 꺼서 픽셀 단위 작업은 피하고 있다.
- 요소 이름을 보지 않고 factory 만 세면 `tee` 가 비디오 경로에 있는 것처럼 보이는데,
  실제 이름은 ***`audiotee`*** 다. 비디오와 무관하다.

### 기각: `media_audio_enabled=false` 로 오디오 경로를 없앤다

월은 muted 라 오디오를 디코드하지 않는데도 `aacparse` + `audiotee` + `identity` 2 개를
유지하고, 양쪽 multiqueue 가 오디오 패드를 하나씩 갖는다(영상당 태스크 4 개 중 2 개).
플래그로 없앨 수 있을 것으로 보고 A/B 했다. ***아무 것도 바뀌지 않았다.***

| 20 영상 | multiqueue:src | 스레드 | 프로세스 총 |
|---|---|---|---|
| audio=on | 6.31 코어 | 80 | 8.15 |
| audio=off (`disable_audio=true` 확인됨) | 6.25 코어 | 80 | 7.94 |

요소 목록은 완전히 동일했다(aacparse 20, tee 20, identity 40, multiqueue 20 - 양쪽 같음).

이유: 이 플래그는 **오디오 싱크 체인만** 끈다. 파일에 오디오 스트림이 있는 한 qtdemux 가
패드를 내고 parsebin 이 파서를 붙이며 multiqueue 가 그 패드를 유지한다 - 소비자가 있든
없든 상관없다. 없애려면 **스트림 선택**(decodebin3 의 `select-stream`)이 필요하고, 그건
`uridecodebin3` 작업 안에 들어간다.

### 1 단계 스파이크의 표적 (셋 다 같은 교체로 해결)

| | 현재 | `uridecodebin3` 교체 후 |
|---|---|---|
| `vqueue` | 원본 3.1MB 가 스레드 경계 통과 | 없음 |
| `audiotee` + `identity` x2 | 쓰지 않는 오디오 경로 유지 | 스트림 선택으로 제거 |
| multiqueue 오디오 패드 x2 | 영상당 태스크 4 | 2 |


## 2026-08-26 — ★병목이 CPU 에서 GPU 로 넘어갔다★ + 반증 2 건

### uridecodebin3 + thread 페이싱: 20 영상에서 CPU 43% 감소

테스트 장비, `-Rows 4 -Cols 5`(20 영상) / `-Rows 5 -Cols 9`(45 영상), 모두 uridecodebin3.

| 영상 | 페이싱 | 총 CPU | 프레임당 CPU | present/s | render_ms p50 | pts |
|---|---|---|---|---|---|---|
| 20 | clock | 16.5 | 27.5 ms | 150~182 | **1.8~2.1 ms** | 1.00 |
| 20 | **thread** | **9.2~9.9** | **15.3~16.5 ms** | 80~86 | **10.9~11.8 ms** | 1.00 |
| 45 | clock | 45.7 | 34.0 ms | 28.5 | **29.9 ms** | 0.99 |
| 45 | thread | 45.3 | 33.5 ms | 28.7 | 28.9 ms | 1.00 |

### `-SinkPacing thread` 를 주면 GPU 가 100% 가 되는 이유

GPU 가 다른 일을 하는 것이 아니다. ***절대 작업량은 오히려 절반***이다
(2.26 -> 1.00 Gpx/s). 바뀐 것은 병목이다.

- `clock`: CPU 가 병목. 디코드 스레드가 16.5 코어를 물고 있어 합성 요청이 드물게 오고,
  GPU 는 올 때마다 1.8ms 에 끝내고 논다 -> GPU 점유율 낮음.
- `thread`: CPU 를 9 코어로 낮춘다. 그러면 합성이 GPU 속도로 밀려 한 번에 11.8ms 가
  걸리고 그동안 GPU 가 계속 일한다 -> ***점유율 100%***. p50 11.8ms 는 60Hz 예산
  (16.7ms) 안이므로 여유가 없다는 뜻이지 부족하다는 뜻은 아니다.

***45 영상에서 두 모드가 같은 이유도 이것이다*** — 거기서는 render_ms p50 이 이미
28.6 / 28.9ms 로 양쪽 다 60Hz 예산을 넘긴다. GPU 가 이미 병목이라 CPU 를 줄여도 결과가
바뀌지 않는다.

| | 20 영상 | 45 영상 |
|---|---|---|
| 병목 | CPU (clock) -> 고치면 GPU | ***GPU*** (양쪽 다 render ~29ms) |
| `thread` 페이싱 효과 | CPU 43% 감소, 유효 | 없음 |

### 반증 1: `thread` 페이싱은 폐기 대상이 아니다

앞 절에서 45 영상 결과 하나(pts 0.50)를 보고 폐기하자고 했는데 틀렸다. 그 측정은
`select-streams` 이전 배포본이었고, 현재 빌드에서는 45 영상에서 두 모드가 동등하며
(45.7 vs 45.3 코어, 둘 다 pts ~1.0) 20 영상에서는 43% 이득이다.

곁들여 `none` 모드(클럭 대기도 sleep 도 없음)를 넣어 분리해 보니 ***공유 클럭 대기 자체는
비용이 아니었다*** — 개발기 20 영상에서 clock 12.7ms/프레임, none 12.5ms/프레임으로
동일하고, thread 만 7.25ms 다. `thread` 의 이득은 클럭 제거가 아니라 ***재우는 행위가
동시성을 낮춰 경합을 줄이는 것***에서 온다(포화 상태인 45 영상에서는 낮출 동시성이 없어
이득도 없다). gst-launch 로 잰 `공유 클럭이 디코딩만큼 쓴다`는 결론은 월에 그대로
옮겨오지 않는다.

### 반증 2: ★픽처 타일을 쪼개면 싸질 것 — 완전히 반대였다★

`gfx_wr_picture_tile_size=display` 가 5760x2160 단일 타일(50MB)이라 비싸다고 보고
쪼개 봤다. 45 영상:

| tile_size | fps | pts | CPU | render_ms p50 | present/s |
|---|---|---|---|---|---|
| `display` | 30.0 | 1.00 | 42.7 | **29.9 ms** | 28.6 |
| `1024x1024` | 15.8 | 0.53 | 46.9 | **99.6 ms** | 8.4 |
| `512x512` | 30.0 | 1.00 | 29.8 | **108.4 ms** | 8.5 |

***합성 1 회가 3.5 배 느려졌다.*** GPU/CPU 점유율이 내려간 것은 일을 덜 해서가 아니라
present 가 28.6 -> 8.5/s 로 굶어서다. `512x512` 에서 pts=1.00 인 것은 디코드는 제 속도로
도는데 화면 갱신만 8.5fps 라는 뜻이고, 육안으로 파탄나 보이는 것이 그것이다.

이유는 구조적이다: 5760x2160 을 512x512 로 쪼개면 타일이 약 100 개가 되고, 45 개 영상이
매 프레임 그 타일들을 가로질러 무효화하므로 타일별 관리 비용이 폭증한다. 픽처 캐시는
정적인 내용이 많을 때 이득이지, 전면이 매 프레임 바뀌는 비디오 월에서는 순수 오버헤드다.
***`display` 가 이미 최적이다.*** 메모리에 남아 있던 `1순위 후보=tile_size display 가
50MB 단일 픽처타일` 가설도 이걸로 반증된다.

### 다음 (미착수)

45 영상의 벽은 ***합성 1 회 29.9ms*** 다. 타일 크기로는 못 줄이고, present 는 28.6/s 로
이미 최소치라 횟수도 줄일 여지가 없다. 남은 축은 `gfx_video_escape_mode=external` —
비디오를 WR 씬에서 빼내 DComp 서피스로 직접 present 하면 45 개 영상을 매 합성마다
샘플링하지 않는다. 이 조사에서 한 번도 제대로 켜본 적이 없다(초기에 재생이 안 된다는
보고가 있었고, 그 뒤로 파이프라인이 바뀌었다).


## 2026-08-26 — `gfx_video_escape_mode=external` 구조 확인 (개발기, 9 영상)

★개발기(AMD 7800M)라 **성능 수치는 근거로 쓰지 않는다.** 여기서 확인한 것은 구조뿐이다.★

`-DcompDebug`/`-VideoEscapeProf` 로 확인:

- ***9 개 영상 전부가 external DComp 서피스로 승격된다*** — `create_external_surface` 9,
  `attach_external_image` 9, `[vesc-prof] converts=270 presents=270`(= 9 x 30fps).
- ***합성 1 회의 draw_calls 가 10 -> 1 로 떨어진다***(16ms 초과 프레임 기준 p50).
  비디오가 콘텐츠 패스에서 빠지고 DComp 가 대신 합성하기 때문이다. 이것이 45 영상의
  `render_ms p50 29.9ms` 를 겨누는 근거다.
- 16ms 초과 프레임 수 133 -> 91.
- ***화면은 육안으로 동일하다*** — 9 칸 모두 재생, 오버레이 텍스트가 영상 위에 정상,
  투명 구멍 없음([[dcomp-transparent-hole-deferred]] 의 그 현상은 나타나지 않았다).
  스케일/배치도 escape off 와 같다.

### 2026-08-20 기록과의 관계 (모순 아님)

당시 "표준 플래그 세트에서 비디오는 external 서피스로 승격되지 않는다"고 기록했는데,
그 세트에는 ***`gfx_video_escape_mode=external` 이 없었다.*** 게이트가
`DComp on` + `이 pref` 둘 다 요구하므로 승격이 없는 것이 정상이었다. 기본값은 여전히 off 다.

### WebRender 상한을 오독할 뻔했다

`MAX_COMPOSITOR_SURFACES = 4` 를 보고 "4 개까지만 승격되니 45 영상엔 무의미"라고 볼 뻔했다.
그 상한은 ***오버레이 경로에만*** 걸린다(`sub_slice_index == len-1` 이면 `OverlaySurfaceLimit`).
YUV 비디오는 macOS 가 아닌 곳에서 `prefer_underlay = true` 라 ***언더레이를 먼저 시도***하고,
언더레이에는 개수 상한이 없다(마스크가 붙은 언더레이만 1 개 제한). 실제로 9/9 가 승격된 것이
그 증거다. — `picture.rs::can_promote_to_surface`

### 남은 것: 테스트 장비 45 영상 A/B (미실행)

개발기에서 볼 수 있는 것은 여기까지다. `render_ms p50` 이 29.9ms 에서 내려오는지는
테스트 장비에서만 판정된다.


## 2026-08-26 — escape=external 이후: Present 가 새 병목, 가설 2 개 연속 반증

`escape=external` 은 45 영상에서 확실한 이득이었다(라운드 17, 5760x2160 singlegpu,
thread 페이싱, uridecodebin3):

| | escape off | **external** |
|---|---|---|
| presents/s | 15.8 | **28.3** |
| 최악 프레임 간격 p50 | 139.1 ms | **43.0 ms** |
| 합성 1 회 draw_calls | 46 | **0** |
| 총 CPU | 46.62 코어 | **35.08 코어** |
| ServoGstVideo 스레드당 | **0.98 (SATURATED)** | **0.77** |

그 다음 벽이 ***Present*** 다 — `present_ms` 가 초당 828~854ms, 즉 렌더러 스레드의 83~85%.

### ★결정적 관측: Present 는 일하는 게 아니라 자고 있다★

스레드 CPU 분해에서 렌더러 계열이 CPU 를 거의 안 쓴다(main 0.29, WRRenderBackend 0.02;
뜨거운 것은 `ServoGstVideo` 45 개뿐). 1 초 중 85%를 Present 에서 보내면서 CPU 는 0.3 코어도
쓰지 않는다 = ***블로킹/대기다.*** 1 회당 비용이 영상 수를 따라 커지는 것(20 개 0.42ms →
45 개 0.65ms)도 고정 제출 비용이면 나오지 않을 모양이다.

### 반증 1 — `Present` 의 flush 가 원인이다 (라운드 18)

D3D11 `Present` 는 즉시 컨텍스트를 flush 하므로, 영상마다 convert→Present 를 번갈아 하면
한 패스에 flush 가 45 번 일어난다고 봤다. convert 를 전부 끝낸 뒤 Present 를 몰아치도록
바꿨다(커밋 b58e4e26060). 결과는 ***이득 0***:

| | 즉시 present | 몰아치기 |
|---|---|---|
| present_ms/s | 836.9 | 854.6 |
| 1 회당 | 0.65 ms | 0.699 ms |
| Present cadence | 28.3/s | 26.9/s |
| CPU | 35.08 코어 | 36.40 코어 |

되돌렸다(04467e9398c). ***육안으로는 "좋아졌다"고 보였는데 그건 라운드 17 의
escape=external 효과였다*** — 이 변경이 얹은 것은 없었다.

### 반증 2 — `BufferCount=2` 라 백버퍼 반납을 기다린다 (라운드 19)

flip 스왑체인 버퍼가 2 장이면 in-flight 가 1 장뿐이라 Present 가 컴포지터의 반납을 기다린다고
봤다. `gfx_video_escape_buffer_count` pref 로 2/3/4 A/B(프리프 적용은 로그로 확인):

| buffers | present_ms/s | 1 회당 | cadence | 영상당 fps | CPU |
|---|---|---|---|---|---|
| 2 | 828.1 | 0.655 ms | 28.2/s | 28.1 | 35.21 |
| 3 | 828.6 | 0.647 ms | 28.4/s | 28.4 | 35.47 |
| 4 | 834.2 | 0.648 ms | 28.4/s | 28.6 | 34.57 |

***완전히 평평하다.*** 백버퍼 대기가 아니다. pref 는 남겨 뒀다(기본 2 = 현행).

### 계측을 먼저: `present_each_ms`

추측을 세 번째로 쌓는 대신, 합계만으로는 구분되지 않던 것을 재기로 했다 —
***모든 호출이 균일하게 0.65ms 인가, 대부분 공짜인데 몇 개가 크게 막히는가.***
`[vesc-prof]` 에 Present 1 회 시간의 min/p50/p90/p99/max 를 추가했다.

- **균일** → 호출당 고정 비용(드라이버/DWM 제출). 횟수를 줄이거나 스레드를 나누는 쪽.
- **이봉** → 주기적인 것에 막히는 것. 그 주기가 16.7ms 면 vsync 다.

개발기 9 영상 기준선(여기서는 병리가 없다): p50 0.115ms, p90 0.17, p99 1.1, max 1.5~2.7.

### 판정 (아래 절)


## ★2026-08-26 — 같은 명령이 두 상태로 갈린다: 원인은 프로세서 그룹★

계측을 넣은 뒤 45 영상 fps 가 파탄났다(28 -> 5). 계측 탓처럼 보였지만 아니다.
***같은 명령·같은 바이너리로 다시 돌리면 정상이다.*** 갈리는 것은 하나뿐이다 —
***Windows 가 프로세스를 어느 프로세서 그룹에 배치했는가.***

45 영상 escape=external 전수(같은 계열 빌드):

| 로그 | 그룹 | 총 코어 | GstVideo/스레드 | present/s | 상태 |
|---|---|---|---|---|---|
| 17/esc_ext | 1 | 35.08 | 0.77 | 28.3 | 정상 |
| 18/defer_ext | 1 | 36.40 | 0.80 | 26.9 | 정상 |
| 19/buf2 · buf3 · buf4 | 1 | 34.6~35.5 | 0.76~0.78 | 28.2~28.4 | 정상 |
| 21/noprof | 1 | 37.07 | 0.81 | 27.8 | 정상 |
| 21/prof_again | 1 | 34.22 | 0.75 | 28.4 | 정상 |
| **20/hist45** | **0** | **46.54** | **0.98 SATURATED** | **4.8** | **파탄** |
| **21/hist45** | **0** | **46.40** | **0.98 SATURATED** | **5.0** | **파탄** |

***9 실행 전부 상관한다.*** group 1 은 예외 없이 정상, group 0 은 둘 다 파탄.
`21/prof_again` 과 `21/hist45` 는 ***플래그까지 완전히 같은 명령***인데 그룹이 갈렸고
결과가 갈렸다. 계측(`present_each_ms`)은 `-VideoEscapeProf` 게이트 안에만 있고,
그걸 끈 `21/noprof` 도 정상이라 계측은 무관함이 양쪽으로 확인된다.

이 장비는 프로세서 그룹 2 개(40+40), NUMA 노드 2 개다. GPU 는 그중 한 노드에 붙어 있다.
그룹은 ***프로세스 생성 시점에 Windows 가 정한다*** — 그래서 같은 명령이 실행마다 다른
상태로 떨어지고, 두 그룹을 넘나든 A/B 는 아무것도 비교하지 않은 것이 된다.

### 파탄의 모습 (이게 왜 "계측 탓"으로 보였나)

group 0 실행은 `wr_render_ms p50 = 137~170ms` 인데 `draw_calls = 0` 이다. 그리는 것이
없는데 렌더가 그만큼 걸린다 — 그 안은 전부 external present 루프다(45 x 2.2ms).
그리고 첫 창부터 그렇다(frames=3,5,5,6...). 여기에 `media_sync_group_target=45` 가
겹친다: 공유 base time 에 묶인 파이프라인은 한 번 뒤처지면 회복 경로가 없어 전속
디코드로 들어간다(런처 주석). ***디코드 0.98 코어 x 45 인데 출력은 5fps*** 인 것이
그 모습이고, DComp 는 Commit 때 스왑체인 버퍼를 놓아주므로 패스가 느려질수록 Present 가
더 막히는 자기강화 고리가 된다. 28fps 와 5fps 는 두 개의 ***안정 상태***다.

### 지난 비교에 미치는 영향

`17/esc_off` 이 group 0(46.62, 0.98 SATURATED), `17/esc_ext` 가 group 1(35.08, 0.77)이었다 —
***그 A/B 는 그룹까지 함께 바뀌었다.*** 다만 escape off 는 group 1 에서도 45 영상이
0.94~0.99 로 포화한다(15/16 라운드, 42.7~45.7 코어)이므로 escape=external 의 이득
(46.6 -> 35.1 코어)은 살아남는다. ***크기는 재확인이 필요하다.***

라운드 18(몰아치기)·19(BufferCount)의 반증은 그대로 유효하다 — 비교된 실행이 전부
group 1 이었다.

### 조치

`run_wall_dist.ps1` 이 이제 기동 직후 1 초 프로브로 ***그룹을 찍고, group 0 이면 경고***한다.
어떤 수치도 그룹 라벨 없이 읽히지 않게 하는 것이 목적이다. 근본 조치(프로세스를 GPU 가
붙은 노드에 고정)는 미착수.


## ★2026-08-26 (2) — 그룹 상관 22/22, 회복 경로 가설 반증, 이제 강제해서 가른다★

앞 절에서 "그룹 상관은 결과일 뿐이고 응용 계층에서 찾아야 한다"고 ***철회했는데, 그 철회가
틀렸다.*** 13 건을 더 쌓았더니 예외가 0 건이다(누적 22/22).

| 실행 | 그룹 | 영상 fps | Present 1 회 | cadence | 디코드/스레드 | 상태 |
|---|---|---|---|---|---|---|
| nosync x6 | 1 | 28.4~28.8 | 0.64~0.65 ms | 28.2~28.8 | 0.68~0.75 | 정상 |
| **nosync_01__** | **0** | **5.0** | **3.25 ms** | 4.2 | 0.97 | **파탄** |
| qos, qos_01, qos_05 | 1 | 27.9~28.4 | 0.65 ms | 27.7~28.2 | 0.77~0.83 | 정상 |
| **qos_02·03·04** | **0** | **6.0** | 2.7~2.8 ms | 5.1~5.4 | 0.98 | **파탄** |

### 반증 3 — 회복 경로가 없어서다

`media_sync_group_target` 의 공유 base time 과 `qos=false/drop=false/max-lateness=-1` 때문에
한 번 뒤처지면 못 돌아온다고 봤다. ***`-NoSyncGroup` 도 `-SinkQos on` 도 파탄을 막지 못한다.***
게다가 두 상태 모두 sync group 은 45/45 arm 성공이었다(기존 미해결 이슈인 "70~78%만 arm" 도
이 건이 아니다).

### 내가 철회한 근거 두 개가 왜 근거가 못 되었나

- **"Windows 11 은 프로세스가 모든 그룹에 걸치는 게 기본이니 그룹 고정은 성립 안 한다"** —
  기본값이 그렇다는 것과 이 프로세스가 실제로 어떻게 배치됐는지는 별개다. 모든 실행에서
  501 개 스레드 ***전부***가 한 그룹으로 보고된다.
- **"논리 40 개짜리 그룹에 46.4 코어는 불가능하다"** — 계산이 안 맞는 것은 사실이고 아직
  설명 못 한다. 다만 그것은 ***측정 필드의 해석 문제***이지, 22/22 상관을 무효화하지 않는다.
  상관을 못 버릴 근거로 "그 필드가 이상하다"를 쓴 것이 실수였다.

***교훈: 상관을 인과로 승격하지 않는 것과, 상관을 근거 없이 폐기하는 것은 다른 실수다.
둘 다 했다. 남은 길은 하나 — 변수를 강제해서 가른다.***

### 조치: `-NumaNode` 로 그룹을 강제한다

`run_wall_dist.ps1 -NumaNode <n>` 추가. Windows 는 그룹을 프로세스 생성 시점에 정하므로
`cmd /c start /NODE` 가 이를 요구할 유일한 경로다(stderr 리디렉션이 winit_wall 에 붙도록
임시 .cmd 래퍼를 쓰고, PID 는 이름으로 찾는다). ***URL 의 `&` 때문에 모든 인자를 인용***한다 —
안 하면 .cmd 에서 명령 구분자로 먹혀 실행이 조용히 잘린다.

판정: `-NumaNode 0` 이 매번 파탄나고 `-NumaNode 1` 이 한 번도 파탄나지 않으면 배치가 원인으로
확정된다. 그때 남는 질문은 "왜 그룹 0 이 나쁜가"이고, 그건 GPU 가 붙은 노드 문제일 수 있다.
반대로 강제해도 갈리면 그룹은 원인이 아니라 ***동반 증상***이고, Present 1 회를 더 잘게
계측하는 쪽으로 간다.


## ★2026-08-26 (3) — 장비는 포화가 아니다(제대로 잰 값), 그리고 -NumaNode 는 안 먹었다★

### 계측기 버그: 머신 전체 CPU 가 한 그룹만 보고 있었다

`GetSystemTimes` 는 64 논리 프로세서를 넘는 장비에서 ***호출 스레드의 프로세서 그룹 하나만***
답한다. 그래서 파탄 실행이 "프로세스 46.40 코어 / WHOLE MACHINE 38.29 코어"라는 불가능한
값으로 찍혔다. ***이 줄은 쓸모없는 게 아니라 해로웠다*** — "장비는 절반만 바쁘다"로 읽혔지만
실제로는 "장비의 절반을 아예 안 봤다"였다. 전 프로세스를 훑어 합산하는 줄로 교체했다.

### 제대로 재니: ★31 코어가 놀고 있는데 파탄난다★

| | 프로세스 | **머신 전체(전 그룹)** | dwm | System |
|---|---|---|---|---|
| 정상 | 32.7 | **33.2 / 80 (41%)** | 0.08 | 0.16 |
| **파탄** | 46.5 | **48.8 / 80 (61%)** | **0.67** | 0.72 |

***CPU 용량 문제가 아니다.*** 파탄 상태에서도 31 코어가 유휴다. 그리고 화면 갱신이 1/5 로
줄었는데 ***dwm 은 8 배 일한다***(0.08 → 0.67). DirectComposition 합성은 우리 프로세스가
아니라 dwm.exe 에서 일어나므로, Present 가 3ms 씩 막히면서 우리 스레드가 CPU 를 안 쓰는
그림과 방향이 맞는다. 다만 dwm 도 0.67 코어라 포화는 아니다.

### `-NumaNode` 는 아무것도 강제하지 못했다 (내 버그)

| 요청 | 실제 그룹 | 결과 |
|---|---|---|
| node 0 | **1** | 정상 |
| node 0 | 0 | 파탄 |
| node 0 | 0 | 파탄 |
| node 1 | **0** | 파탄 |
| node 1 | 1 | 정상 |
| node 1 | 1 | 정상 |

`start /NODE` 를 ***래퍼 cmd.exe 에 적용***했고 그 cmd 가 자식으로 winit_wall 을 띄웠다 —
자식은 노드 선호를 물려받지 않는다. 요청과 실제가 무관하므로 ***그 라운드는 인과를 시험한
것이 아니다.*** 래퍼 안에서 `start "" /NODE n /B /WAIT "<exe>"` 로 winit_wall 을 직접 띄우도록
고쳤다. 그룹↔결과 상관은 6 건이 더 붙어 ***28/28*** 이 됐다(예외 0).

### 다음

인과 검증(강제 후 갈리는지)이 여전히 미완이다. 그리고 CPU 가 남아도는데 파탄난다는 것이
확정됐으므로, 원인 후보에서 "CPU 부족"은 빠진다. 남은 것은 Present 가 무엇을 기다리는가이고,
dwm 이 8 배로 뛰는 것이 유일한 프로세스 밖 단서다.


## ★★2026-08-26 (4) — 인과 확정: 프로세서 그룹 배치가 45 영상 파탄의 원인이다★★

`start /NODE` 를 winit_wall 에 직접 걸도록 고친 뒤 6 회. ***요청한 노드와 실제 그룹이 6/6
일치했고, 결과가 깨끗하게 갈렸다.***

| 요청 | 실제 | 영상 fps | Present 1 회 | 머신 전체 | dwm | 상태 |
|---|---|---|---|---|---|---|
| 0 | 0 | **6.0** | 2.94 ms | 48.7 / 61% | 0.66 | **파탄** |
| 0 | 0 | **6.0** | 2.93 ms | 48.8 / 61% | 0.64 | **파탄** |
| 0 | 0 | **6.0** | 3.02 ms | 48.7 / 61% | 0.64 | **파탄** |
| 1 | 1 | 29.5 | 0.65 ms | 32.3 / 40% | 0.09 | 정상 |
| 1 | 1 | 27.9 | 0.66 ms | 36.6 / 46% | 0.09 | 정상 |
| 1 | 1 | 23.0 | 0.73 ms | 44.5 / 56% | 0.11 | 정상 |

누적 상관 34/34 에 더해 ***강제 실험으로 방향까지 확정***됐다. 노드 0 이면 예외 없이 파탄,
노드 1 이면 예외 없이 정상이다.

### 이 결과가 뒤집는 것

***이 조사에서 group 을 넘나들며 잰 모든 A/B 는 다시 재야 한다.*** 특히
`escape off vs external`(17/esc_off 이 group 0, 17/esc_ext 이 group 1)은 그룹이 함께
바뀐 비교였다. escape=external 의 이득 자체는 group 1 실행끼리 비교해도 남지만
(escape off 는 group 1 에서도 45 영상이 0.94~0.99 로 포화), ***크기는 재확인해야 한다.***

라운드 18(present 몰아치기)·19(BufferCount)의 반증은 비교 대상이 전부 group 1 이라 유효하다.

### 지금 당장의 운용 회피책

`run_wall_dist.ps1 -NumaNode 1`. 런처가 실제 그룹을 찍고 group 0 이면 경고하므로 확인도 된다.

### 남은 질문: 왜 노드 0 이 나쁜가

CPU 부족은 아니다 — 파탄 상태에서도 31 코어가 논다. 유일한 프로세스 밖 단서는 ***dwm 이
8 배 일한다***는 것(0.09 → 0.65 코어)이고, 화면 갱신은 오히려 1/5 다. DirectComposition
합성이 dwm 에서 일어나므로 방향은 맞는다.

유력 가설: ***GPU 가 노드 1 에 붙어 있고, 노드 0 에서 도는 프로세스는 업로드와 present 마다
인터커넥트를 건넌다.*** 확인은 `probe_machine_topology.ps1` 한 줄이다(DXGI 는 이 값을 노출하지
않아 PnP 장치 속성으로 물어야 한다). 확인되면 근본 조치는 노브가 아니라
***기동 시 GPU 가 붙은 노드로 프로세스를 고정***하는 것이다.


## ★★2026-08-26 (5) — 기전 확정: GPU 는 노드 1 에 있고, 노드 0 은 소켓 간 링크를 건넌다★★

`probe_machine_topology.ps1` 실측(테스트 장비):

```
OS   : Windows 11 Enterprise, build 26100  (프로세스가 모든 그룹에 걸치는 게 기본)
CPU  : Intel Xeon Gold 6248 x 2 소켓 (각 20C/40T) = 40 물리 / 80 논리, 2 그룹
GPU  : AMD Radeon RX 580 2048SP x 4 장  ->  ★전부 NUMA node = 1★
```

노드 1 = GPU 가 붙은 소켓, 노드 0 = 반대편 소켓. 강제 실험 결과와 정확히 맞물린다:

| 프로세스가 도는 노드 | GPU 와의 관계 | 영상 fps | Present 1 회 | dwm |
|---|---|---|---|---|
| **1** | GPU 와 같은 소켓 | 23~29.5 | 0.65~0.73 ms | 0.09~0.11 |
| **0** | ***반대편 소켓*** | **6.0** | **2.9~3.0 ms** | **0.64~0.66** |

***반대편 소켓에서 돌면 업로드와 present 마다 소켓 간 링크를 건넌다.*** CPU 용량 문제가
아니라는 것도 이걸로 설명된다(파탄 실행이 31 코어를 남긴다) — 부족한 것은 코어가 아니라
GPU 로 가는 경로다.

### 조치: `-NumaNode auto` 를 기본값으로

런처가 기동 시 디스플레이 어댑터의 NUMA 노드를 읽어 그 노드에 프로세스를 고정한다.
`off` = 이전 동작(Windows 가 고름), 숫자 = 강제.

- 어댑터들이 서로 다른 노드를 보고하면 경고하고 ***아무것도 고정하지 않는다***.
- 아무 어댑터도 노드를 보고하지 않으면(소비자용 파트에서 흔하다) 경고하고 고정하지
  않는다. 개발기가 정확히 이 경우이고, 그것이 정상 동작이다.

### 부수적으로 드러난 것

- ***테스트 장비 GPU 는 RX 580 4 장이다.*** CLAUDE.md 의 서술(A4000/A5000 계열)과 다르다.
- `probe_machine_topology.ps1` 이 PowerShell 5.1 에서 `[ushort]` 를 못 찾아 그룹 섹션이
  통째로 에러였다 — PowerShell 7 전용 타입 가속자다. `[System.UInt16]` 로 교체했다.
  ***테스트 장비는 `powershell.exe`(5.1)로 돈다.***

### 이제 다시 재야 하는 것

`-NumaNode 1` 고정 상태에서 ***escape off vs external 재측정***. 라운드 17 의 그 비교는
off 가 group 0, external 이 group 1 이라 그룹까지 함께 바뀐 것이었다.


## ★★2026-08-26 (6) — 같은 노드에서 재측정: escape 이득의 절반 이상이 NUMA 아티팩트였다★★

`-NumaNode auto` 로 둘 다 group 1(GPU 소켓)에 고정하고 45 영상 재측정:

| | escape **off** | escape **external** |
|---|---|---|
| presents/s | **30.4** | **30.0** |
| render p50 | 31.3 ms | 32.5 ms |
| 프레임 간격 p50 | 79.3 ms | **66.5 ms** |
| 합성 draw_calls | 46 | **0** |
| 프로세스 CPU | 40.70 | **31.98** |
| 디코드/스레드 | 0.90 | **0.71** |
| 머신 전체 | 41.2 / 51% | **32.5 / 41%** |

### 정정

라운드 17 에서 보고한 ***"presents/s 15.8 -> 28.3 (+79%)"와 "render 43.5 -> 34.1ms"는 전부
NUMA 아티팩트다.*** 그 비교는 off 가 group 0(GPU 반대편 소켓), external 이 group 1 이었다.
같은 노드에서 재보면 ***처리량 이득은 0***이다(30.4 vs 30.0), render 도 오히려 근소하게 나쁘다.

살아남은 이득: ***CPU 21% 감소***(40.7 -> 32.0 코어), 디코드 스레드 0.90 -> 0.71(포화 해소),
프레임 간격 p50 79 -> 66ms. `draw_calls 46 -> 0` 은 실재하지만 ***프레임레이트로 바뀌지 않는다.***

### ★그리고 훨씬 큰 사실: 45 영상이 두 모드 모두 ~30fps 로 돈다★

영상당 29.4fps, 머신 CPU 41~51%. ***"45 영상 파탄"의 정체는 대부분 NUMA 였다.***
대조군(비 Servo 월)이 같은 장비에서 54 영상을 50~70%로 도는데, 지금 45 영상이 external 에서
41% 다 — 단순 외삽하면 54 영상이 약 38 코어(48%)로 ***대조군 범위 안***이다.

### 남은 것

- ***54 영상 실측***이 다음 단계다. 목표 대비 어디인지 이제야 말할 수 있는 상태가 됐다.
- 프레임 간격 p50 이 66~79ms 로 크다(30fps 면 33ms 여야 한다). 최악 96~102ms. 즉 평균
  처리량은 맞는데 ***페이싱이 고르지 않다.***
- Present 1 회는 group 1 에서 p50 0.549ms / p90 1.105 / p99 2.833 / max 4.009 다.
  초당 856ms 를 여전히 present 에 쓴다(1321 회). ***균일한 쪽에 가깝다*** — 이봉이 아니다.
  즉 호출당 고정 비용이며, 줄이려면 횟수를 줄이거나 스레드를 나눠야 한다.


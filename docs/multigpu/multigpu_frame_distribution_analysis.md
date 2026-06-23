# 멀티-GPU 월: 프레임 분산 업로드 검증 + GPU 생성 콘텐츠 cross-GPU 판단 (2026-06-23)

GStreamer 백엔드 미디어 프레임이 "CPU 1프레임 → 각 영역 GPU에 적절히 분산 업로드"
전제를 만족하는지 검증하고, GPU 생성 콘텐츠(텍스트/도형/WebGL/WebGPU)를 다른 GPU로
전송해 렌더링할 때의 성능 영향을 판단한 결과. 코드 근거 file:line 포함.

## Q1. 미디어 프레임 분산 업로드

- **(A) CPU 메모리 보유 — ✅.** Windows는 `RenderDummy`(GL 없음). appsink 시스템메모리 I420,
  외부이미지 lock이 `ExternalImageSource::RawData(&[u8])` 반환
  (`media/backends/gstreamer/render.rs`, `media/media-thread/lib.rs`).
- **(B) "각 영역 GPU에 일부만 업로드" — 원래 ❌, 본 변경으로 개선.**
  원래: `PaintMessage::UpdateImages`가 전 타일 painter에 `update_images(updates.clone())`
  broadcast(`paint.rs`), 각 painter가 `update_image(..., &DirtyRect::All)` + full descriptor
  로 **전체 프레임을 모든 GPU에 복제 업로드**, 타일 크롭은 표시-시점(viewport transform +
  scissor, `painter.rs generate_frame`). → GPU마다 표시량의 N배 업로드(2x2 4K ~75% 낭비).

### 개선: 영역별 업로드 (commit b53b9e4cbd1, pref `media_wall_region_upload` 기본 off)
- `Painter::media_tile_dirty_rect`: 외부이미지(미디어)에 한해 이 타일의 가상뷰포트 sub-rect를
  plane 좌표(Y full / I420 chroma half)로 환산 → `DirtyRect::Partial` 전달.
- **webrender가 honor함(소스 확인)**: `texture_cache.rs:1626-1644` — `ExternalImageType::Buffer`
  + `Partial`이면 `offset = dirty.min.y*stride + dirty.min.x*bpp`로 **sub-rect 바이트만 업로드**.
  `realloc`(size/format 변경)일 때만 All 강제(`:941-946`) → 비디오는 첫 프레임만 전체,
  이후 매 프레임 sub-rect. 공유 씬/UV/key 불변.
- **제약**: 미디어 element가 가상뷰포트를 채운다는 가정(월 비디오/캡처 용도). 부분 배치 비디오는
  좌표가 어긋나므로 pref off 유지. element 배치 rect는 공유 디스플레이리스트
  (`layout/display_list/mod.rs push_yuv_image`)에만 있고 업로드 경로엔 없어 일반화는 별도 작업.
- 검증: 2x1 월 + fill 프로브(`tests/html/multigpu_region_upload_probe.html`) 스모크 — 양 타일
  present ready=2/2, barrier completed, req→all-ready ~12.3ms, 무크래시. 시각 seam은 사용자 확인.

## Q2. GPU 생성 콘텐츠 cross-GPU 전송 판단

**핵심: 이 포크는 cross-GPU 픽셀 전송을 의도적으로 회피(올바름).** "한 GPU 프레임을 다른 GPU로
복사"는 P2P 없으면 PCIe 왕복 + 생성 GPU 직렬화 병목으로 부정적. 대신 **"한 번 기술 → 각 GPU가
재생/재래스터화"**. 설계 문서 명시: *"GPU 간 texture copy는 v1에서 사용 안 함"*
(`multigpu_tiled_present_implementation_plan.md` Phase 4).

| 콘텐츠 | 방식 | cross-GPU 복사 | 잔여 비용 |
|---|---|---|---|
| DOM 텍스트/도형 | 디스플레이리스트(벡터) broadcast → 각 GPU webrender 로컬 래스터화 | 없음 | 타일 분량 래스터화(컬링). 무해 |
| WebGL | 타일 GPU별 isolated D3D11 device + GL 커맨드 **전 백엔드 동일 replay**(`webgl_thread.rs:646-648`, SetViewport/Scissor 포함), 표면=full canvas | 없음 | **각 GPU 전체 캔버스 풀 렌더(draw N배)** + 자산 업로드 N× |
| WebGPU | 보조 GPU별 독립 `wgc::global::Global` + 커맨드 mirror/replay (`wgpu_thread.rs`) | 없음 | **각 GPU 전체 렌더(N배)**. present는 아직 primary GPU CPU 리드백(Phase 3 미구현) |
| 미디어(비디오) | CPU 프레임 image-update broadcast | 없음(CPU 공유) | 업로드 N×(→ Q1 영역별 업로드로 개선) |

**WebGL/WebGPU 정밀 모델**: 캔버스는 하나의 DOM element로 **각 GPU에서 전체가 풀 렌더**된다
(앱 커맨드·viewport·scissor 동일 replay, 표면도 full-canvas). **타일별 차이는 WebGL/WebGPU
렌더 내부가 아니라 WebRender 합성 단계에만** 존재(painter별 external-image lock + 타일
scissor/transform). 즉 "GPU가 viewport를 다르게"가 아니라 "compositor가 GPU별 다른 영역을 크롭".

**판단**: 남은 성능 이슈는 cross-GPU 복사가 아니라 (a) 작업 중복 N배 (b) 자산 업로드 N배.
- 비디오 업로드 중복 → **영역별 업로드(Q1)로 해소**(본 변경).
- WebGL/WebGPU 렌더 중복(draw N배)은 cross-GPU 복사를 피하기 위한 **의도된 비용**이며
  transparent하게 줄이기 어렵다(per-GPU scissor는 fragment 일부만 절약 + vertex/compute 풀
  실행 + readPixels/캔버스-as-texture 정합성·앱-관측 scissor 충돌). 워크로드 분할 또는 Phase 3
  GPU-direct present은 별도 사안.

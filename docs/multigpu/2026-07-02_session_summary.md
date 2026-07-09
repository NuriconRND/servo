파일을 직접 생성할 도구가 저에게는 없어서, 저장하실 수 있도록 요약 내용을 아래에 정리해 드립니다. (예: `servo/docs/multigpu/2026-07-02_session_summary.md` 로 저장하시면 됩니다.)

---

# 멀티-GPU 비디오 월 성능 조사 — 세션 요약 (2026-06-30 ~ 07-02)
브랜치: `video-perf-investigation`

## 1. 문제 정의
- winit_wall(단일 프로세스, 하나의 논리 WebView 팬아웃)에서 다수 FHD30 비디오 그리드 재생 시 CPU 폭증·프레임레이트 저하. 상용 브라우저는 정상.
- 반증(사용자 제공): 같은 16영상을 **16개 독립 프로세스**로 돌리면 정상 → 디코드는 병렬화되어 무죄, 병목은 단일 composite 파이프라인.

## 2. CPU 관점 원인 (초기)
- 기본 **SW 디코드 강제**(`lib.rs` VideoDecoderPolicy 기본 Software, HW 디코더 rank NONE) → `=auto`로 복구 가능.
- `<video>`마다 독립 playbin3 파이프라인.
- Windows zero-copy 부재(`render.rs` I420 시스템메모리). **WebRender는 NV12 native 수용** → I420 강요는 군더더기.

## 3. 프레임레이트 병목 = 합성 (측정으로 확정)
- 계측 추가(`painter.rs`): 초당 `Render perf`(composite_fps/update_ms/draw_ms/uploads/generate emitted vs attempts), winit_wall `Present perf`.
- 개선안 (a) coalesce·(b) 부분 damage **둘 다 측정으로 기각**: 이미 pending_frames 가드가 coalesce, draw는 항상 작음.
- 진짜 병목 = **`renderer.update()` 내부 WebRender picture caching**. 면적 ~8–10Mpx에서 절벽(3840×2160 정상 → 3840×3240 폭발 ~600ms/1fps).
- 격리 근거: glFinish 펜스(GPU 래스터/present 아님), 1영상@12.4Mpx는 빠름(면적 단독 아님, primitive수×면적), tile-size 스윕(256px→3366ms) → picture caching 확정. 루트는 반드시 TileCache(scene/frame builder 강제).

## 4. 워크어라운드 (WebRender 포크, env 게이트)
- `[patch.crates-io]`로 로컬 webrender 0.68 포크 연결. `WR_DISABLE_PICTURE_CACHING`.
- 3시도: None composite→패닉, Blit→패닉, **dirty추적 스킵**(현재 채택).
- **실기 검증(정정)**: dirty-skip 패치가 멀티-GPU 대화면에서 **정상 동작** — 4K 16영상 ~1fps→~20fps, 1080p present 11→7ms. (단일 모니터 "런어웨이"는 오버사이즈 창 아티팩트였음.)

## 5. 남은 문제 + 탐색
- 대화면 표출 해상도 커질수록 여전히 하락(면적 비례). 원인 = WebRender가 전부를 한 surface에 **래스터+합성 2패스**.
- 네이티브 컴포지터(DirectComposition): 0.68 미제공, 임베더 구현 500–2000 LOC, all-dynamic엔 부분 효과(60 미보장) → 첫 수로 부적합.
- 사용자 확인: **non-WebRender 방식(DComp/HW 오버레이 추정)으론 대화면 16영상 30–60fps 이미 달성**.

## 6. 추가 실험 knob (커밋됨)
- `SERVO_WALL_VIDEO_PACE_HZ=N`: 비디오 즉시-합성을 초당 N회로 페이싱(coalesce). 1080p는 이미 vsync 60이라 present 변화 없음(효과는 `generate_emitted`로 확인, fps<60인 대화면에서 의미).
- `SERVO_VIDEO_DEC_MAX_THREADS=N`: libav `avdec_*` SW 디코더 max-threads 강제(과다구독 완화, HW엔 no-op).
- 그 외 진단: `SERVO_PERF_GLFINISH`, `SERVO_WR_PICTURE_TILE=WxH`, `RUST_LOG=warn,paint=info`.
- 전체 절차/파라미터: `servo/etc/multigpu/BUILD_DEPLOY_and_PARAMS.txt`, 수동 테스트: `.../MANUAL_TEST_webrender_picturecache.txt`.

## 7. 새 프로젝트 설계 (진행 중)
- **dx_wall_probe** — WebRender 없는 **C++/Win32/DirectX11 + GStreamer** 대조군(리포 루트 `tools/dx_wall_probe/`).
- 같은 wall_layout·DXGI 토폴로지 GPU 배치·per-video 파이프라인·grid를 재현하되, appsink CPU YUV 프레임을 **스왑체인 백버퍼에 직접 렌더**(vsync, 늦은 프레임 드롭). 전 타일 멀티-GPU 단일 프로세스.
- 목적: 직접 DX가 60fps면 WebRender 합성이 병목임을 입증.
- 빌드 = **VS2022 솔루션(.sln/.vcxproj/.props)**. DX12는 후속.
- 설계 spec 커밋: `servo/docs/superpowers/specs/2026-07-02-dx-wall-probe-design.md`. 현재 구현 계획(writing-plans) 단계.

## 8. 주요 커밋 (video-perf-investigation)
- 계측 + 프로브 페이지, webrender 패치+`[patch]`+가이드, `SERVO_WALL_VIDEO_PACE_HZ`, `SERVO_VIDEO_DEC_MAX_THREADS`, 두 spec(picture-cache 워크어라운드 / dx_wall_probe).

## 9. 메모리 파일
- `video-grid-perf-root-cause.md`(CPU 관점), `video-grid-composite-bottleneck.md`(framerate 관점·전 측정 근거·실기 검증 정정 포함).

---

원하시면 이 내용을 위 경로에 파일로 저장하도록 메인 세션에 요청하시면 됩니다(제가 직접 파일을 만들 수는 없습니다).
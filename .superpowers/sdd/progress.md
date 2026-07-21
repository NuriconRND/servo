# SDD Progress Ledger — 비표준 미디어 표출 기능 재이식

- Plan: docs/superpowers/plans/2026-07-21-nonstandard-media-display-port.md
- Spec: docs/superpowers/specs/2026-07-21-nonstandard-media-display-port-design.md
- Branch: nonstandard-media-display-port
- BASE (branch work start): b7f35c1f982 (spec), plan 94266df3c31
- 핵심 규칙: 모든 신규 pref 기본 off·무회귀 계약 / 커스텀 엘리먼트·#5·#6 제외 / GStreamer 1.22.8 유지 / clean re-port(소스 hunk 추출)

## Tasks
(진행되는 대로 아래에 한 줄씩 추가)
- Task 1 (P1) 확장 이미지: complete (commits 2bc3ad8fe52..485f4c0c271, 리뷰 clean+fix반영). pixels load_extended_from_memory+jxl-oxide(image_all_channels), 표준 <img> 분기(pref dom_image_extended_formats_enabled 기본off), 8포맷 스모크 통과, pixels 유닛 3pass. fix wave 485f4c0c271=x-image 브랜딩 제거·probe 상대경로·Cargo.toml 정렬·jxl channels==0 가드·위임/DDS 테스트. rustfmt 불일치=저장소 전반 선재조건(무관).
- Task 0 (P0): complete (commit f2ac635b196, findings note + rtsp_testsrc.mp4 복원). 결과: ①경로 정정 — plugin_lists=`components/servo/gstreamer_plugin_lists/*.in`, servo_env=`..\scripts\servo_env.ps1`, gst번들=`target/dependencies/...` ②RTSP feasible on 1.22.8, common.rs.in에 gstrtsp/gstudp 2줄만(gstreamer.py 무변경) ③script 크레이트 unit test 불가(mozjs)→Task2/3 런타임 probe ④병합충돌 2곳: update_media_state, create_media_player ⑤자산: jxl/avi 有, tiff/exr/qoi/mkv/wmv 無, 라이브 RTSP=사용자몫(OPEN). 계획 f2ac635 이후 P0정합으로 갱신 예정.

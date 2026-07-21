# SDD Progress Ledger — 비표준 미디어 표출 기능 재이식

- Plan: docs/superpowers/plans/2026-07-21-nonstandard-media-display-port.md
- Spec: docs/superpowers/specs/2026-07-21-nonstandard-media-display-port-design.md
- Branch: nonstandard-media-display-port
- BASE (branch work start): b7f35c1f982 (spec), plan 94266df3c31
- 핵심 규칙: 모든 신규 pref 기본 off·무회귀 계약 / 커스텀 엘리먼트·#5·#6 제외 / GStreamer 1.22.8 유지 / clean re-port(소스 hunk 추출)

## Tasks
(진행되는 대로 아래에 한 줄씩 추가)
- Task 0 (P0): complete (commit f2ac635b196, findings note + rtsp_testsrc.mp4 복원). 결과: ①경로 정정 — plugin_lists=`components/servo/gstreamer_plugin_lists/*.in`, servo_env=`..\scripts\servo_env.ps1`, gst번들=`target/dependencies/...` ②RTSP feasible on 1.22.8, common.rs.in에 gstrtsp/gstudp 2줄만(gstreamer.py 무변경) ③script 크레이트 unit test 불가(mozjs)→Task2/3 런타임 probe ④병합충돌 2곳: update_media_state, create_media_player ⑤자산: jxl/avi 有, tiff/exr/qoi/mkv/wmv 無, 라이브 RTSP=사용자몫(OPEN). 계획 f2ac635 이후 P0정합으로 갱신 예정.

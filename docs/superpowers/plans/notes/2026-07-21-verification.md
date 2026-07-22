# Task 5: 통합 검증 & 무회귀 확인 — 결과

날짜: 2026-07-21 (실행 2026-07-22 새벽)
브랜치: `nonstandard-media-display-port`
전제: P1~P4의 신규 pref(`dom_image_extended_formats_enabled`[P1 확장 이미지], `dom_video_extended_containers_enabled`[P2 확장 컨테이너], `dom_video_network_uri_enabled`[P3 RTSP], `dom_screen_capture_enabled`[P4 화면캡처])는 전부 기본값(off) 그대로 — 이번 검증은 **아무 pref도 넘기지 않은 상태**로 실행했다. (주의: `dom_x_image_enabled`/`dom_rtsp_stream_enabled`는 이 브랜치에서 의도적으로 제외한 커스텀 엘리먼트용 pref로, 실제 신규 pref가 아니다.)

---

## Step 1: 월 파이프라인 무회귀 스모크

실행 (모든 신규 pref 기본값 off):
```
target\debug\servoshell.exe --wall-layout <3-tile layout> --wall-all-tiles tests\html\multigpu_wall_sync_probe.html 2> wall_regress.err.log
```
10초 실행 후 `CloseMainWindow()` → 3초 대기 → 필요시 `Stop-Process`(baseline 레시피와 동일한 그레이스풀 종료), stderr 정상 flush 확인(7094줄, 빈 로그 아님).

**환경 노트:** `../config/wall_layout.local_3x1.json`은 최신 `wall-spatial-display-autogpu` 스키마(`display` 필드)로 갱신돼 있으나, 이 브랜치(`nonstandard-media-display-port`)의 `ports/servoshell/wall_layout.rs`는 그보다 이전 시점의 `monitor`/`gpu` 스키마를 그대로 쓴다 — 그래서 그 파일을 그대로 넘기면 `monitor must be a non-negative integer` 파싱 에러가 난다(코드 회귀 아님, 두 브랜치 간 config 스키마 계보 차이). 검증에는 동일한 3-tile 레이아웃(5760x1080 virtual viewport, 1920x1080 타일 3개, overlapPx 32)을 `monitor`/`gpu` 필드로 다시 쓴 로컬 config를 사용했다(커밋 대상 아님, 워크트리에만 존재).

| 신호 | 결과 |
|---|---|
| panic / thread panicked | **0건** (`grep -i panic` 매치 없음) |
| missed-frame / pending-frame 관련 크래시성 경고 | 없음 (barrier miss는 있으나 아래 참조 — 설계된 정상 동작) |
| `scroll_offsets=matched` | **280건**, mismatch 0건 |
| `Wall frame barrier complete` | 279건, `Wall frame metadata ... target_count=3` 프레임들 정상 관측 |
| `Wall frame barrier missed` (경고, 크래시 아님) | 8건 — `policy=keep-previous-frame-for-delayed-targets`로 정상 처리(설계된 지연-타깃 정책) |
| WARN: `Wall tile 2 requested monitor 2, but only 2 monitor(s) are available` | 1건 — **이 실행 환경(2모니터)의 사전 존재 특성**, baseline 로그(`wall_smoke_sync.err.log`, `wall_smoke_sync.release.err.log`, 이전 태스크에서 캡처됨)에도 동일하게 1건씩 있어 이 브랜치가 만든 게 아님 |
| per-tile present/render 균형 | PainterId(1)=556, (2)=542, (3)=375 — tile 3(monitor 2 미가용)이 낮은 건 baseline과 같은 패턴(baseline: 821/770/514, 814/659/467 — 비율 거의 일치) |

**barrier miss 비율 비교**: 이번 실행 8/287 ≈ 2.8% vs baseline `wall_smoke_sync.err.log` 28/498 ≈ 5.6%, `wall_smoke_sync.release.err.log` 88/548 ≈ 16% — 오히려 이번 실행이 더 낮거나 동등한 수준. **회귀 아님.**

**결론: 월 파이프라인은 이 브랜치에서 무회귀. scroll matched 100%(280/280), barrier 정상 완료, 패닉 0, 기존에 있던 barrier-miss/모니터-부족 특성도 baseline과 동일 패턴으로 재현됨(새로 나빠지지 않음).**

---

## Step 2: 게이트 명령 일괄 (`W:\servo_multigpu-tiled-wall`에서, `servo_env.ps1` 소싱 후)

| 명령 | 결과 |
|---|---|
| `cargo test -p servoshell wall_layout --lib` | **PASS** — `3 passed; 0 failed` (`parses_valid_wall_layout`, `calculates_overlap_render_rect_clamped_to_virtual_viewport`, `rejects_out_of_bounds_tile`) |
| `cargo test -p servo-pixels extended_decode --lib` | **PASS** — `3 passed; 0 failed` (`extended_decode_rejects_empty`, `extended_decode_handles_jpeg_xl`, `extended_decode_delegates_standard_formats`) |
| `cargo check -p servoshell` | **PASS** — `Finished \`dev\` profile [unoptimized + debuginfo] target(s) in 3m 40s`, 에러 0건(`surfman` 관련 사전 존재 warning만 있음, 이 브랜치 코드 아님) |
| `git diff --check` | **PASS** — exit 0, whitespace/conflict marker 없음 |

---

## 종합 결론

`nonstandard-media-display-port` 브랜치는 P1(확장 이미지 디코드)~P4(getDisplayMedia) 신규 기능을 전부 pref 기본값 off로 유지한 채 기존 월(wall) 렌더 파이프라인과 공존하며, 이번 검증에서 스크롤 동기화 100% 매칭·프레임 배리어 정상 완료·패닉 0건·기존 게이트(단위 테스트 2종, `cargo check`, `git diff --check`) 전부 통과를 확인했다. 관측된 barrier-miss 경고 및 tile 3(2-모니터 환경) 프레젠트 불균형은 이 브랜치 도입 전부터 있던 실행 환경 특성이며 baseline 로그와 같은 패턴으로 재현되어 회귀가 아니다. 별도로, P4 `getDisplayMedia`는 이미 `DONE_WITH_CONCERNS`로 기록된 알려진 프레임 전달 갭(트랙/게이팅은 동작하되 실제 프레임 전달 경로가 미해결)을 갖고 있으나, 이는 `dom_screen_capture_enabled=true`로 명시적으로 켰을 때만 드러나는 그 기능 자체의 기존 이슈이지 이번 무회귀 검증(모든 pref off) 범위의 회귀가 아니다.

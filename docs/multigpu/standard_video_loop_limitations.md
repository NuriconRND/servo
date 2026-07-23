# 표준 `<video>` 루프 재생 — wmv/ts 컨테이너 제약 (알려진 한계)

- 날짜: 2026-07-23
- 브랜치: `nonstandard-media-display-port`
- 관련: `htmlmediaelement.rs` pause-self 수정(`8f976550e36`), 프로브 `tests/html/multigpu_standard_video_extended_probe.html`

## 요약

`--wall-all-tiles`에서 로컬 `<video loop>`의 **첫 재생(0→끝)은 모든 컨테이너 정상**이다
(mkv/avi/wmv/ts/flv/mov 6종 동시, 30초 클립 기준 30초까지 매끄럽게 재생 — pause-self 버그 수정
`8f976550e36` 이후). 그러나 **루프 경계(끝→seek(0)→재개)에서 `wmv`와 `ts` 두 컨테이너만
재개하지 못하고 0에서 정지**한다. 흔한 웹 컨테이너(mkv/webm/mp4(mov)/avi/flv)는 루프도 정상.

## 근본원인 (계측으로 확정)

강제 `currentTime=0`(루프-seek 모사) 후 파이프라인 상태·DOM 이벤트를 관찰한 결과, **원인이 둘로 갈린다**:

| 컨테이너 | servosrc(기본, 스크립트 바이트 피딩) | direct-file(`SERVO_MEDIA_DIRECT_FILE=1`, playbin 직접 읽기) | 판정 |
|---|---|---|---|
| **wmv** (asf/wmv2) | seek 후 상태 `Playing`·`rs=4`인데 프레임 미생산 → 0 정지 | **정상 seek·재개**(0→0.5→1.0→…) | **servosrc 경로 문제** — Servo-side |
| **ts** (mpegts/h264) | 정지 (seek 후 프레임 미생산; `seeked` 미발생) | **여전히 정지** | **GStreamer tsdemux seek 근본 한계** — 경로 무관 |
| mkv/mov/avi/flv | 정상 | 정상 | — |

- **wmv**: 서보의 servosrc(appsrc 바이트 피딩)가 asf 스트림의 seek-to-0 후 재공급/디코더 재개를
  제대로 못 한다. GStreamer가 파일을 직접 읽는 `direct-file` 경로에서는 정상 seek·재개된다.
- **ts**: servosrc·direct-file **양쪽 모두** seek 후 프레임이 안 나온다 = GStreamer의 mpegts
  demuxer seek 자체의 한계로, 소스 경로를 바꿔도 해소되지 않는다.

## 왜 코드 수정 대신 문서화인가

- 대상이 **드문 컨테이너 2종 + 루프 한정**이고, 흔한 포맷은 전부 정상이다.
- `ts`는 GStreamer 근본 한계라 Servo에서 실효적 수정이 어렵다.
- `wmv`는 Servo-side 수정 가능하나(servosrc asf seek 처리), 비용 대비 효용이 낮다.
- 따라서 **알려진 한계로 문서화**한다.

## 워크어라운드

- **wmv**: `SERVO_MEDIA_DIRECT_FILE=1`로 실행하면 루프 재개가 정상 동작한다(브랜치의 비디오월
  기본 경로와 동일). 단 direct-file는 read-queue 메모리 비용 등 자체 caveat가 있다
  (`components/media/backends/gstreamer/player.rs`의 `DIRECT_FILE_ENV` 주석 및 video-grid 참조).
- **ts**: 신뢰할 워크어라운드 없음. 루프가 필요하면 다른 컨테이너(mp4/mkv 등)로 재인코딩 권장.

## 비고 — 테스트 자산

프로브(`multigpu_standard_video_extended_probe.html`)의 `test_media/test_media.*`는 **30초 클립**을
사용해야 이 거동을 정확히 관찰할 수 있다. 2초 클립은 ~1.5초마다 루프해 "정상 루프"와 "stall"을
구분하기 어렵다(과거 2초 클립으로 인해 루프-정상을 stall로 오인한 사례 있음).

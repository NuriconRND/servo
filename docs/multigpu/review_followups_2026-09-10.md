# 차후 과제 — 2026-09-08~10 작업 리뷰에서 나온 것

대상: `wall-css-animation-stutter`(61) → `rtsp-tcp-transport-settings`(1) →
`wall-video-present-when-raf-idles`(13), 합 75 커밋.

주제와 브랜치 경계가 거의 맞지 않는다 — 애니메이션 브랜치가 메모리 누수 전체와 WebRender
벤더링을 담고 있고, "raf idles" 브랜치가 캡처 크래시와 크래시 리포터를 담고 있다. 병합 전에
주제별로 다시 가르는 편이 낫다(F8).

우선순위는 **F1 → F2 → F3 → 나머지**다. F1 만 실제 결함이고 나머지는 위험·부채다.

---

## F1 (결함) 바운드 큐가 `AddImage`/`DeleteImage` 까지 버린다

`components/paint/paint.rs` `with_painter_mut_detached`

```rust
if !threaded.dispatch_detached(limit, callback) { DETACHED_DROPPED... }   // 통째로 버린다
```

버려지는 `callback` 은 `painter.update_images(updates)` 이고, 그 `updates` 에는
`AddImage`·`DeleteImage` 가 섞인다(같은 자리의 팬아웃 요약 로그가 `adds=`/`deletes=` 를
세는 것이 증거다).

같은 파일의 `drop_superseded_video_frame_updates` 는 **Add/Delete 가 걸린 키를 일부러
코얼레싱에서 제외**한다 — 멱등이 아니기 때문이다. 큐 제한 경로에는 그 보호가 없다.

* 버려진 `AddImage` → 그 페인터는 이미지 키를 영영 모른다(영상 누락, "unknown external
  image" 위험)
* 버려진 `DeleteImage` → 그 페인터의 WebRender 에 영구 누수

실측 `dropped=5434`(100 초). 그 실행에서 영상이 나온 것은 대부분이 평범한 프레임 갱신이었기
때문이지 설계가 막아 준 것이 아니다.

**할 일**: 배치가 **epoch 없는 `UpdateImage` 만**일 때에 한해 버린다. Add/Delete 가 섞이면
블로킹 경로(`with_painter_mut`)로 넘긴다. 판정은 `drop_superseded_video_frame_updates` 가
이미 쓰는 것과 같은 기준이라 그 함수 옆에 두면 규칙이 한곳에 모인다.

## F2 (동작 위험) WebGL 유휴 회수 기본값이 일반 페이지에 과감하다

`dom_webgl_idle_context_reclaim_ms` = `3000`, **기본 켬**, 판정 기준은 "합성됐는가"
(`webgl_thread.rs` `last_composited`).

즉 **스크롤 밖에 있거나 애니메이션이 멈춰 재합성되지 않는 캔버스**도 3 초 뒤 GPU 백엔드를
잃는다. `webglcontextlost`/`webglcontextrestored` 를 구현하지 않은 페이지는 죽은 캔버스가
된다. 벽 앱에는 의도한 동작이지만 **Servo 전역 동작 변경**이다.

**할 일**: 기본 `0`(off)으로 두고 벽 실행(`run_wall_dist.ps1`)에서만 켠다. 또는 판정 기준을
"합성되지 않았다"에서 "문서에 없거나 보이지 않는다"로 좁힌다.

**부수**: 사양상 `webglcontextlost` 는 cancelable 이고 저자가 `preventDefault()` 했을 때만
`restored` 를 보내야 하는데, 지금은 무조건 복구한다. 관대한 방향이라 위험은 낮지만 주석에
남길 것.

## F3 (문서 부채) — **해결됨 (2026-09-10)**

새 pref 8 개가 정본 표(`configuration.md`)에 없었다. 이번에 추가했다. 규칙 자체를 다시 적어
둔다: **pref 를 추가한 커밋은 그 표를 같이 고친다.**

## F4 (깨지기 쉬운 불변식) 생산자 규칙이 관례로만 유지된다

`painter.rs` `last_frame_by_other_source_at` 는 "스스로 프레임을 내는 경로는 자기 프레임 뒤
이 시각을 되돌린다"는 약속에 의존한다. 지금 그 자리가 **세 곳**이고(페인트 애니메이션,
비디오 도착, 주기 생산자) 각각 손으로 save/restore 한다.

네 번째 생산자가 생기면 조용히 깨지고, 증상은 "60Hz 인데 65fps"라는 읽기 어려운 형태다
(실제로 그 형태로 한 번 겪었다 — 애니메이션이 자기 프레임을 "남"으로 읽었다).

**할 일**: `generate_frame_as(source)` 로 감싸 규칙을 잊을 수 없게 만든다.

## F5 (미검증) 링 초기화 상한이 한 번도 걸린 적이 없다

`media_ring_init_max_per_frame` = `1` 과 렌더-프레임 카운터(`shared/paint/render_frame.rs`).
2026-09-10 기준 모든 실행에서 `MEDIALOCKDEFER` 가 **0 건**이다.

**할 일**: 만들게 한 압력이 다른 수정(첫 프레임 스테이징 제거, 블로킹 제거)으로 사라진
것인지, 발동 조건이 틀린 것인지 확인한다. 전자면 그렇게 적고 제거를 검토한다.

## F6 (운용) 진단 로그 양

상시 `warn!` 약 50 줄/초 + 런처가 켜는 `paint=info` 약 480 줄/초 → 60 초에 20MB. 조사에는
맞지만 납품 상태는 아니다.

**할 일**: `-Diag` 한 스위치로 묶는다. 상시 남길 최소 집합(`MEDIAPLAYERS`, `WEBGLDOM`,
`MEDIADOM` 정도)과 조사용을 가른다.

## F7 (구조 결정 필요) 벤더 WebRender

느린 렌더 계측만을 위해 크레이트 전체(약 135k 줄)를 `vendor_local/webrender` 로 복사하고
`Cargo.toml` 의 `webrender = { path = ... }` 로 고정했다. 지금은 업스트림 0.68 과 갈라진
사본이다.

**할 일**: 명시적으로 정한다. 유지한다면 재동기 절차를 이 문서 옆에 적고, 아니면 타이머를
feature 뒤로 넣어 업스트림에 올린다. 지금 상태로는 다음 Servo 리베이스에서 조용히 문제가
된다.

## F8 (정리) 브랜치 재분할

지금 세 브랜치는 이름과 내용이 어긋나 리뷰·되돌리기가 어렵다. 병합 전에 주제별로 가른다:
애니메이션+생산자 / 누수+표출경로 / gapless+캡처크래시 / 계측·인프라.

## F9 (비용 기록) 페인트측 애니메이션 샘플링

`horizon 3000ms ÷ 1/60` = 애니메이션 프래그먼트 하나당 디스플레이 리스트 빌드마다 약 181 회
스타일 질의(`MAX_SAMPLES=256` 상한). 프래그먼트 수에는 상한이 없다.

지금 측정값(`SCRIPTBUSY reflow_ms` 최대 7.1)으로는 문제가 아니지만, 전환 순간 애니메이션
프래그먼트가 수십 개면 리플로 안에서 수천 회가 된다. **비용의 출처로 기록해 둔다** — 리플로가
다시 무거워지면 여기부터 본다.

## F10 (상수) pref 로 뺄 것

* `SEGMENT_ARM_LEAD` = 3s (`player.rs`) — 되감기 몇 초 전에 세그먼트 모드를 무장할지
* `SINK_PACER_RESYNC_AFTER` = 150ms (`player.rs`) — 이만큼 밀리면 따라잡기를 포기

둘 다 콘텐츠 특성에 민감한데 상수로 박혀 있다.

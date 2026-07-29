# SDD Progress Ledger — 비표준 미디어 표출 기능 재이식

- Plan: docs/superpowers/plans/2026-07-21-nonstandard-media-display-port.md
- Spec: docs/superpowers/specs/2026-07-21-nonstandard-media-display-port-design.md
- Branch: nonstandard-media-display-port
- BASE (branch work start): b7f35c1f982 (spec), plan 94266df3c31
- 핵심 규칙: 모든 신규 pref 기본 off·무회귀 계약 / 커스텀 엘리먼트·#5·#6 제외 / GStreamer 1.22.8 유지 / clean re-port(소스 hunk 추출)

## Tasks
(진행되는 대로 아래에 한 줄씩 추가)
- ★빌드 환경 함정(P3/P4에도 적용)★: (1) **이 세션 PowerShell 도구가 exit 1로 깨짐** → 빌드/PS는 bash에서 `powershell.exe -NoProfile -Command/-File`로 우회. (2) mach 빌드가 mozangle build.rs:155 **Os 206 경로길이**로 실패(F: 워크트리 82자) → **`subst W: "F:\20260609_SDWall_BrowserTest\20260606_multigpu_browser"` 후 `W:\servo_multigpu-tiled-wall`(28자)에서 빌드**(CARGO_HOME도 W:\servo\.servo\로 단축됨). env(INCLUDE)는 무관, 순수 경로 문제. 빌드 스크립트: cd W:\servo_multigpu-tiled-wall; `. ..\scripts\servo_env.ps1`; `.\mach build -j 8`. (3) 빌드는 run_in_background+폴러(rustc/cargo procs=0)로 감시. cf. [[mozangle-path-length-build-fail]].
- Task 2 (P2) 확장 비디오 컨테이너: complete (commits 78b6dbf4f90..2c41ade9c2e, 리뷰 clean+fix반영). 표준 <video>가 mkv/avi/wmv/asf/mov/ts/flv 인식(pref dom_video_extended_containers_enabled 기본off). 병합충돌 2곳은 컨테이너 hunk가 해당 함수 미접촉→불필요. fix 2c41ade=gstflv/gstmpegtsdemux 등록+DRY 헬퍼 extended_container_allowed+flv/ts/mov 샘플. 스모크 pref-on: mkv/avi/wmv/flv/ts/mov 전부 rs=4(실demux+첫프레임), pref-off 전부 거부·패닉0. **이월 Minor(최종리뷰 triage)**: is_extended_container_type가 MIME lowercase 안 함(선재 source 동작), probe가 미생성 *.thumb.jpg 6개 참조(verbatim 카피, 404 무해).
- Task 1 (P1) 확장 이미지: complete (commits 2bc3ad8fe52..485f4c0c271, 리뷰 clean+fix반영). pixels load_extended_from_memory+jxl-oxide(image_all_channels), 표준 <img> 분기(pref dom_image_extended_formats_enabled 기본off), 8포맷 스모크 통과, pixels 유닛 3pass. fix wave 485f4c0c271=x-image 브랜딩 제거·probe 상대경로·Cargo.toml 정렬·jxl channels==0 가드·위임/DDS 테스트. rustfmt 불일치=저장소 전반 선재조건(무관).
- Task 0 (P0): complete (commit f2ac635b196, findings note + rtsp_testsrc.mp4 복원). 결과: ①경로 정정 — plugin_lists=`components/servo/gstreamer_plugin_lists/*.in`, servo_env=`..\scripts\servo_env.ps1`, gst번들=`target/dependencies/...` ②RTSP feasible on 1.22.8, common.rs.in에 gstrtsp/gstudp 2줄만(gstreamer.py 무변경) ③script 크레이트 unit test 불가(mozjs)→Task2/3 런타임 probe ④병합충돌 2곳: update_media_state, create_media_player ⑤자산: jxl/avi 有, tiff/exr/qoi/mkv/wmv 無, 라이브 RTSP=사용자몫(OPEN). 계획 f2ac635 이후 P0정합으로 갱신 예정.
- Task 3 (P3) RTSP: complete (commit 0507b1ab3c2, 리뷰 Approved). 표준 <video src=rtsp://>를 NetworkUri 플레이어로 재생(pref dom_video_network_uri_enabled 기본off). StreamType::NetworkUri 변형 전-backend(gstreamer/dummy/ohos) 배선, create_media_player=set_resource_url 힌트+network_uri arm 공존(검증), gstrtsp/gstudp 2줄 등록. 라이브 스모크(로컬 gst-validate-rtsp-server rtsp://127.0.0.1:8554): pref-on readyState=4·첫프레임 t=0.04·9.56s, pref-off 거부 code=4·패닉0. **이월(최종리뷰 triage)**: ①[Important 비블로커] ohos/lib.rs NetworkUri=>todo!() (ohos는 Windows서 미컴파일 unreachable, 기존 Stream=>todo! 형제 패턴; 제대로 고치려면 create_player가 Result 아니라 trait 재설계 필요=범위밖) ②[Minor] pref+scheme 체크 2곳 DRY ③[Minor] player.rs source-setup arm 중복.
- ★리뷰 함정: 저장소 .rs가 CRLF라 plain git diff가 htmlmediaelement.rs를 binary로 표시 → 리뷰 패키지는 `git diff --text`로 생성해야 diff 보임(P2 blob도 동일 CRLF=선재, 손대지 말 것).
- Task 4 (P4) getDisplayMedia: **DONE_WITH_CONCERNS** (commit 86323376472, BASE 94b3c82bfb1). getDisplayMedia() API·MediaDevices.webidl·Bindings.conf·4 pref(dom_screen_capture_enabled 등)·d3d11screencapturesrc 캡처 파이프라인·트랙 생성·pref 게이팅 전부 동작. **미해결**: 캡처 프레임이 <video>에 미도달(videoSize 0x0 STALLED). 캡처 요소 자체는 정상(GST_DEBUG로 1920x1080 BGRA 30fps 생성 확인) → 하류 negotiation 문제. 시도·기각: ①raw-passthrough(media_stream_source video/x-raw) vs d3d11 memory caps ②d3d11download Bin 삽입(create_display_stream) — 둘 다 프레임 미해결. GST_PAD_IS_SINK critical 잔존. pref-off=API unavailable·패닉0(게이팅 정상). 근본원인=워크트리 raw-passthrough(f99f22f/9d5370f) vs d3d11 캡처 소스 caps의 더 깊은 상호작용, 추가 조사 필요. **사용자 판단 대기: 프레임전달 심층 디버깅 vs 문서화 후 이월 vs P4 revert.**
- Task 5 통합검증: complete (note 979dde135b1). wall_layout 3/3, pixels extended_decode 3/3, cargo check clean, git diff --check clean. 월 무회귀: scroll matched 280/280, barrier 279(8 miss=keep-previous 정상), 패닉0. 모든 신규 pref 기본off.

## 최종 whole-branch 리뷰 (opus): **Ready to merge = YES-with-follow-ups. 블로커 0.**
- 교차검증 PASS: servo-media Backend trait(network_uri+NetworkUri+create_display_stream) 전 impl coherent·exhaustive; 플러그인 등록이 pref-off can_play_type 무변경; 제외항목 누수 0; P4 커밋 코드 graceful(전 실패=log::warn+None).
- 수정 완료: 검증노트 pref 오기(dom_x_image/dom_rtsp_stream→실제명) 정정.
- **잔여 follow-up(비블로커, 이월)**: ①[by-design] 타입속성 없는 `<video src=.avi/.flv/.ts/.wmv>`는 pref-off에도 새 데무서로 재생됨(플러그인 등록의 의도된 부수효과, 월 무관·can_play_type 무변경) ②prefs.rs media_screen_capture_* struct/default 필드순서 상이(무해) ③P2 is_extended_container_type MIME lowercase 미적용 ④P3 ohos NetworkUri=>todo!()(unreachable) ⑤P3 pref+scheme 2곳 DRY ⑥P4 프레임전달 갭(사용자 수용, 별도 후속). 모두 acceptable-as-follow-up.

# SDD Progress Ledger — 캡처카드 getUserMedia 디바이스 선택 (2026-07-23)

- Plan: docs/superpowers/plans/2026-07-23-capture-card-device-selection.md
- Spec: docs/superpowers/specs/2026-07-23-capture-card-device-selection-design.md
- Branch: nonstandard-media-display-port, BASE: b829aab55db (plan commit)
- 핵심: 표시=mediafoundation(구분 label+device.path id) / 캡처 element=ksvideosrc 우선 / 미일치=트랙0 / deviceId 미지정=현행 front()

## Tasks
- Task 1: complete (commits b829aab55db..29fa16ff999, review clean). ConstrainString+device_id 필드. [Minor 이월] Constrain* 패밀리 derive 비일관(ConstrainString만 Clone,Debug)
- Task 2: complete (commits 29fa16ff999..c91985ef41a, review clean). device_id.rs 헬퍼+테스트 6/6. [Minor 이월] device_api 빈문자열 미필터(Some("") 가능), 헬퍼 dead-code warn(Task3/4 소비 시 소멸)
- Task 3: complete (commits c91985ef41a..e8acd2d3a29, review clean). 열거 mf 대표+유니크 id+ks 숨김; is_none_or 그대로. [Minor 이월] 오디오도 path 우선 id(과거 display_name; brief 승인된 통일 로직) / mf properties 실기 확인은 Task 7
- Task 4: complete (commits e8acd2d3a29..9ae1dd781d7, review clean). get_track deviceId 매칭+ks 우선, 무지정 front() 무회귀. [Minor 이월] 로그 문구가 비디오 전제(오디오 경로 공유), 동일 키+동일 API 중복 시 silent overwrite
- Task 5: complete (commits 9ae1dd781d7..10a83e57238, review clean). WebIDL deviceId 활성화+변환; 유니온 이름 추정 적중, rustfmt 재랩만. 트레일러 확인됨
- Task 6: complete (commits 10a83e57238..833e5d81d69, review clean). probe ?device= 선택. [Minor 이월] ?device= 빈값은 exact 경로로 첫 장치(무해)
- Task 7: complete (commit 31a73d9e67f, 리뷰 fix 1회 후 CLEAN). 정적검증+release빌드 exit0+실기 4/4포트 선택검증(ks 경로,전부 1080p advancing)+네거티브 clean. 리포트 수치 오기재는 실로그 verbatim으로 정정(수정이력 섹션). 환경노트: local_1x1.json 스키마 선재 불일치→branchschema 사용, RUST_LOG=warn,script=info,servo_media_gstreamer=info 필요, 캡처스트림 open 중 창닫기 teardown 12s+ 행업(선재 이슈)
- 최종 whole-branch 리뷰(opus): Ready to merge = With fixes → fix wave c9f9a13f91c 적용·재리뷰 Approved. 반영: ①(Important) select_device_by_id display_name 폴백 티어(경로없는 장치=오디오 id 왕복; path 티어 우선) ②빈 문자열 deviceId=미지정 ③no-match warn에 caps 사전필터 주석 ④probe 헤더 layout 스키마 주석 ⑤스펙 '최종 리뷰 반영' 절. 실기: bogus-id 백엔드 no-match warn 라이브 확인; 오디오 티어는 이 빌드 audioinput=0(wasapi 플러그인 미등록 추정)이라 정적 검증. 잔여 follow-up(비블로커): T4 로그 문구 video 전제, name_match first-win vs path last-win 비일관, sequence 첫원소 절단, Ideal fail-closed 비표준(업스트림 시 pref화 권고), local_1x1.json 스키마 선재 불일치, teardown 행업 선재. 범위: b829aab55db..c9f9a13f91c (11 commits)

# SDD Progress Ledger — servoshell display-only wall_layout 이식 (2026-07-24)

- Plan: docs/superpowers/plans/2026-07-24-servoshell-display-only-wall-layout.md
- Spec: docs/superpowers/specs/2026-07-24-servoshell-display-only-wall-layout-design.md
- Branch: nonstandard-media-display-port, BASE: 05f4b0a006c (plan commit)
- 핵심: display=공간인덱스(좌상단0,행우선) / 레거시 monitor=별칭(warn)·gpu=무시(warn) / GPU=구동 adapter 자동 / 토폴로지 없으면 winit-nth 폴백 / borderless 풀스크린 유지
- 확인된 전제: servoshell은 no-wgl 피처(Cargo.toml:138)→토폴로지 활성 / crate명 servo-paint-api / PhysicalPosition 이미 import(headed_window.rs:33) / test_2x1_samegpu.json은 레거시 monitor+gpu 스키마

## Tasks
- Task 1: complete (commits 05f4b0a006c..cf8469df8ea, review clean). 토폴로지 헬퍼 verbatim 이식(spatial_order 5/5) + servo re-export. test_read_pixels 실패는 DX-interop 환경 문제로 무관 확정(리뷰어 판정). [Minor 이월] rustfmt/git diff --check는 Task3 Step1에서 / enumerate_display_topology의 GetDesc 실패 시 무로그 폴백(verbatim 상속, 업스트림 이슈)
- Task 2: complete (commits cf8469df8ea..e8bcab00397, review clean). display-only 스키마/파서(레거시 별칭·gpu무시)+공간배치·auto-GPU+폴백+로그 3사이트. wall_layout 5/5, tile.monitor/gpu 잔존 0(독립확인). 적응 2건 정당 판정(3번째 로그 사이트=E0609 불가피, DisplayTopology 타입주석=unused-import 회피). [Minor 이월] winit monitor 매칭이 좌표 완전일치만(크기 교차검증 없음; DPI-aware 매니페스트 확인됨) / 풀스크린 판정 로직 두 분기 중복(brief 코드 그대로) / rustfmt 선재 불일치 2건은 Task3에서 확인
- Task 3: complete (commit 611499eaa0a, review Approved). 정적 15/15, cargo build exit0, 스모크 2회 ready=2/2 panic0, spatial topology 경로 진입 확정(tile1 직접로그+tile0는 requested_gpu=Some(0) 배타성 소스검증). [Important→fix wave] setup_logging 이전 생성되는 primary 창 탓에 tile0 배치로그+레거시 deprecation/gpu경고가 stderr에 전혀 안 나옴(선재 순서문제지만 우리가 만든 경고가 무용지물) → winit_wall 선례대로 eprintln화 예정. [Minor 이월] wall_layout 테스트 로그파일 미보존 / 스펙 '테스트'절의 'Wall display topology' 문구가 실제 로그와 불일치
- Fix wave: complete (commit 283af6d5141). 진단 로그 eprintln 전환(wall:/wall layout: 프리픽스). 실기 증명: tile0+tile1 'Positioning wall tile N on spatial display N' 양쪽 출력(좌측 x=0=spatial0 직접확인), 레거시 경고 4줄 출력, panic0, wall_layout 5/5, build exit0. 비-월 실행은 wall_layout.is_some() 가드로 무영향(코드경로 증명)
- 최종 whole-branch 리뷰(opus): Ready to merge = With fixes. I1 perf분석기 PRESENT_RE 파손 / I2 gpu 폐지로 fan-out 검증하니스 무력화(→사용자 결정: gpu를 명시 override로 부활) / I3 LUID 교차검증+토폴로지 덤프 누락 / I4 폴백 원인 혼동·인덱스 공간 미문서화 / I5 문서·예제config 미갱신. 교차검증 PASS: adapter_index 인덱스공간 일치(EnumAdapters1 동일), no-wgl 무조건 활성, 좌표/페인트 모델 무변경, eprintln 스코프 정확. 창당 토폴로지 열거 비용=비이슈 판정
- Fix wave(최종리뷰 5건): complete (commits 283af6d5141..dcc4a8d52b2). gpu override 부활(WallTile.gpu_override)+LUID 교차검증+토폴로지 덤프+폴백 원인 구분+perf regex 양방향+문서/예제config. **컨트롤러 독립 검증**: wall_layout 6/6, 소스에 5건 전부 존재, 실기 스모크 2회 — 토폴로지 덤프(좌측 x=0 DISPLAY3가 spatial 0! winit 인덱스와 불일치=이 기능의 존재이유 실증), 'gpu' override -> adapter 1, requested_gpu=Some(1) x4336(렌더컨텍스트 도달 확정), LUID mismatch 0, panic 0
- 진단 정합 fix: complete (commit eb5adecd9a1, 재리뷰 Important 2건 해소). gpu_label=override/auto/default adapter 실제해석값 기반, 토폴로지 덤프는 std::sync::Once로 단일타일 프리뷰 모드(--wall-tile-index N, no --wall-all-tiles)에서도 출력. 컨트롤러 확인: 단일타일 덤프 출력됨, (auto-GPU) 리터럴은 주석에만 잔존, 6/6 테스트

# SDD Progress Ledger — IP-wall 타일별 오디오 선택 표출 (2026-07-24)

- Plan: docs/superpowers/plans/2026-07-24-wall-audio-per-tile-selection.md
- Spec: docs/superpowers/specs/2026-07-24-wall-audio-per-tile-selection-design.md
- Branch: nonstandard-media-display-port, BASE: d1f2b954d04 (plan commit)
- 핵심: 오디오 선택=<video>별 muted 토글(가산) / 파이프라인 라우팅 기존재(playbin·MediaStream→autoaudiosink) / Servo autoplay 미강제(is_allowed_to_play=true) / gstwasapi2가 오디오장치 열거 provider(번들에 존재) / 캡처 audio=수동 deviceId(기존 인프라) / getDisplayMedia·autoplay pref·자동페어링 비범위
- 확정 전제: windows_plugins()가 windows.rs.in 각 항목을 {plugin}.dll로 복사(gstwasapi2 한 줄+mach build면 자동복사) / 플러그인 변경은 mach build 필수

## Tasks
- Task 1: complete (commit 76bf45de71b, review Approved). gstwasapi2 등록 1줄+mach build. 열거 검증(controller 대행): audioinput=9, 캡처카드 Analog 01-04 Audio 전부 구분 label/id. Rust/python 이중 파스 계약 유지 확인. ★구현 에이전트가 자체 Monitor/background-wait 루프에 반복 정지→이후 GUI 스모크 태스크엔 Monitor 금지·inline Start-Sleep 지시
- ★근본버그 수정(Task2 테스트 중 발견, systematic-debugging): mute→unmute 오디오 미복원 = set_mute가 live playbin3의 audio-sink를 런타임 스왑(be2148a68cb 도입)한 결함. playbin은 audio-sink를 preroll에만 링크→restore 스왑 무효. 수정 commit 34681fef9b4: 런타임 스왑 제거, GstPlay mute속성+set_audio_track_enabled(가역)만 사용, 미사용 restore_/custom_audio_renderer 정리. ★사용자 release 검증 PASS(mute→unmute 소리 복원)★. cargo check 경고0. 별개 미해결: debug 빌드는 월+동적 video src에서 MakeCurrentFailed 크래시(release 정상)
- Task 2 (그리드 probe): commits dbba89441b9(0바이트 사고)..93232213421(복구). multigpu_wall_audio_grid_probe.html — 4타일 그리드+타일별 가산 체크박스, 파일/RTSP 타일+캡처/WebRTC 자리. Wildlife mp4 AAC 확인. 오디오 sink 수정과 합쳐 검증됨.
- Task 2: complete (commits 76bf45de71b..34681fef9b4 [probe dbba894..9323221 + mute fix 34681fe], review Approved). 그리드 probe byte-identical·placeholder 유지, mute 수정 근본원인 완전제거+인간검증. [Minor 이월] brief Step3의 audio-sink restored/disabled 로그는 수정으로 삭제됨(이제 'mute state updated' 로그로 확인) — Task3/4 검증은 오디오 sink 로그 대신 트랙 카운트+청각으로
- Task 3: complete (commit fca0edbc47f, review Approved). 캡처 타일 getUserMedia audio deviceId 배선(verbatim), audio=1('Analog 04 Audio'→wasapi2sink), caps수정 불요, HTML만. [Minor 이월] MediaStreamTrack.label=undefined(Servo gstreamer 백엔드 미채움, probe 무결) / capaud 빈문자열 분기 brief 그대로 비대칭
- Task 4 (WebRTC 오디오): DONE_WITH_CONCERNS (commit f0cc674738a, probe 배선 정상). 프로듀서 audiotestsrc→webrtcsink OPUS 오퍼 확정(a=rtpmap:101 OPUS/48000/2, BUNDLE video0 audio1) — 소스 오디오 있음. Servo answer가 recvonly audio 트랜시버 부착+BUNDLE 포함하나 오디오 트랙이 JS로 미전달(ontrack video만, a=0). = 엔진 수신측 협상 갭(answer rtpmap 완전성/webrtcbin 2번째 수신pad/트랙배선). ★사용자 결정: B — WebRTC 오디오는 문서화 한계로 이월, 3/4 경로로 마무리★
- Task 5 (마무리): 스펙 추기+메모리. 최종상태: 파일/RTSP/캡처(audio=1→wasapi2sink)=오디오 동작, WebRTC=엔진갭 이월. 근본버그 mute→unmute 수정이 최대 성과(인간검증). 별개 이월: debug 빌드 월+동적video src MakeCurrentFailed 크래시(release 정상)

# SDD Progress Ledger — 월 가드밴드 크롭 present_inset 통합 (2026-07-28)

- Plan: docs/superpowers/plans/2026-07-28-wall-guard-band-present-inset.md
- Spec: docs/superpowers/specs/2026-07-28-wall-guard-band-present-inset-design.md
- Branch: nonstandard-media-display-port, BASE: f41b6927a9b (plan commit)
- 핵심: DComp on이면 gui.rs:646이 blit을 스킵→가드밴드 크롭 소멸(tile1 32px 밀림, 사용자 DCOMP=0 대조로 확정). 크롭 정본을 RenderingContext::present_inset으로 올리고 DComp는 root visual 오프셋으로, blit은 source rect로 소비. 세로 규약 반대(DComp -top / GL bottom).
- 운용: 런타임 육안 판정(눈금 라벨)은 사용자 몫 — 서브에이전트에 GUI 판정 위임 금지. GUI 스모크에 Monitor/백그라운드 대기 루프 금지(과거 세션 정지 사례).

## Tasks
- Task 1: complete (commit 05a79681fa5, review Approved/spec ✅). tile_render_insets를 WallLayout으로 승격 + SideOffsets2D 반환 + 테스트 5개(총 11/11). 리뷰어 독립 검증: 클램프 등가성 수기 추적(구 코드는 use-site별 .max(0), 신규는 함수 내 일괄 — 동작 동일), 5개 테스트 기대값 전부 tile_render_rect 산술로 재계산 일치, 호출처 2곳 모두 마이그레이션(누락 0), inset.top 유지 확인(Task5 몫). [Minor 이월] tile_device_rect/tile_render_device_rect 이중 조회(선재 패턴, 무회귀) / SideOffsets2D::new 인자순서(top,right,bottom,left)가 주석 의존 — 이후 태스크에서 전치 실수 주의
- Task 2: complete (commits ccff1bc75ce + fix 98913ee7852, review Approved/spec ✅). RenderingContext::present_inset(트레잇 기본 zero + Window Cell + Offscreen 양방향 위임, cfg(windows) 미게이트) + servoshell 창생성 시 주입. 리뷰어 확인: 트레잇 구현체는 Window/Offscreen 둘뿐이고 양쪽 갱신, getter/setter 대칭(비대칭이면 zero로 조용히 읽힘), 타입 무변환 통과.
  [plan-mandated Important → 사용자 결정 A: eprintln 전환] tile0 창이 setup_logging 이전 생성돼 info!가 버려지는 선재 문제(:189-192에 기록)로 새 inset 진단이 tile0에서 미출력이었음 → 파일의 배치 진단과 동일하게 eprintln!("wall: ...")로 전환. **실기 2줄 확인**: tile0 render(0,0)-(1952,1080) inset(0,32,0,0) / tile1 render(1888,0)-(3840,1080) inset(0,0,0,32). 이 값이 진단(=tile1 프레임버퍼 원점이 가상 1888)을 직접 입증.
  [Minor 이월] rustfmt 선재 drift는 재확인 안 함(Task 6에서)
- Task 3: complete (commits 9f62ee53893 + 계측기 fix 7001507f61b, review "Needs fixes"→해소). 눈금 프로브 + wall_layout.test_1x2_vertical.json. 실기 확인: 세로 layout이 상하 가드밴드를 실제로 생성 — tile0 inset(0,0,32,0) render 1920x1112 / tile1 inset(32,0,0,0) render y시작 1048. 리뷰어가 JSON 산술 독립 재계산 + 타일링(간극·겹침 0) 확인.
  [plan-mandated Important → 사용자 결정: 수정] STEP=128이 1080을 못 나눠(1080%128=56) 세로 경계 y=1080에 라벨 부재 = 계획의 판정규칙("구석 라벨=타일 원점") 자체가 불성립. STEP=120(gcd)+주눈금 120px로 수정, 4개 타일 원점 전부 라벨 존재 검증(스크립트). 미세눈금 8px 유지(1920·1080 둘 다 나눔, 32px=4칸). 컨트롤러 직접 편집(56줄 단일 파일).
  [Minor 이월] test_* 형제 config들은 레거시 monitor/gpu 단일행 포맷, 신규는 display 다행 포맷(현행 관례 따름 — 결함 아님)
- Task 4: complete (commits 354524e2c02 + fix 288a9084f5a, review "Needs fixes"→2건 해소). DComp root visual에 SetOffsetX/Y(-left,-top). **실기 확정**: log `root guard-band offset (0,0)` / `(-32,0)`, engaged x2, panic 0. **컨트롤러 화면 캡처 판정(4장)**: display0 `0,0`·display1 `1920,0`이 창 구석에 정확 — DComp on/off 양쪽. (off/display1은 GitHub Desktop이 가려 2회 실패 → P/Invoke EnumWindows로 해당 HWND(1920,0) 전면화 후 재캡처 성공. 이후 GUI 검증 시 이 기법 재사용할 것.)
  리뷰어 독립 검증: root_visual 대입 1곳(:1338)·해제 1곳(:1972=teardown 전용)이라 캐시 수명 안전 / frame_surfaces에 WR타일(:2617)+external비디오(:1504) 모두 들어가고 end_frame(:3155-3167)이 전부 root.AddVisual → 균일 적용 사실 확인 / RemoveAllVisuals는 자식만 제거, root 오프셋 보존.
  [Important 2건 → 수정] (a) COM 실패 시 last_root_offset을 무조건 기록 = present_inset이 창 수명 내내 고정이라 **영구 latch, 재시도 없음** → 두 HRESULT 성공 시에만 기록. 이건 계획 예시 코드의 결함이라 계획 문서에도 정정 주석. (b) 신규 매크로 3줄 rustfmt 비준수(구현자는 파일 전체 diff 줄수만 비교해 오판) → 3줄만 정형화, 107→104 hunk.
  ★Task 6 주의: dcomp_compositor.rs에 **선재 rustfmt drift 104 hunk** 존재 → "touched 파일 rustfmt --check 클린"은 성립 불가. 신규 줄 무drift로 판정 기준 조정할 것. 또한 info 로그 보려면 RUST_LOG=info 필요(기본 error). cargo check -p servo-paint는 webgl_thread.rs 선재 feature-unification로 실패 → --features surfman/sm-angle-default 사용.
- Task 5: complete (commits ba3340da6e8 + 주석교정 아래 참조, review Approved/spec ✅). webview_visible_source_rect의 y 원점 top→bottom. **컨트롤러 화면 캡처 판정(세로 1x2, 4케이스)**: DComp OFF(=이번 수정 대상 blit 경로) display0 `0,0`·display1 `0,1080` / DComp ON display0 `0,0`·display1 `0,1080` — 전부 구석 정확. DComp ON 로그 `root guard-band offset (0,0)`·**`(0,-32)`** = Task 4의 y 성분 첫 실기 검증.
  리뷰어 독립 검증(핵심): **중간 flip 부재 확인** — source_rect.origin.y가 blit_framebuffer(rendering_context.rs:1879-1916)까지 무변형 전달, 소스 FBO는 WR이 직접 그리는 평범한 TEXTURE_2D FBO라 보상 패스 없음 → top→bottom이 버그 유발이 아니라 수정임이 확정. DComp 쪽 `-inset.top`(dcomp_compositor.rs:2487-2490)과 대조해 주석의 규약 대비 서술도 사실 확인.
  [Minor → 즉시 교정] 주석의 "visible은 render rect 위쪽에 붙어 있다"가 2x2 아래쪽 행 타일에선 거짓 → bottom inset의 무조건 정의로 교체(주석만, 동작·바이너리 무영향).
  ★캡처 함정: t5_off_d1이 또 다른 앱을 찍음(3회째). SetForegroundWindow는 백그라운드 프로세스에서 실패(fg==target False)하지만 **SetWindowPos HWND_TOPMOST + 폭>=800 필터**로 성공. 이후 GUI 캡처는 이 방식 사용.
- Task 6: complete (CLAUDE.md 편집 — 루트 CLAUDE.md는 git 밖이라 커밋 불가, 개수 3→11 반영). 정적: wall_layout 11/11, cargo check servoshell/servo-paint-api clean, git diff --check clean, **rustfmt drift는 4개 파일 모두 base f41b6927a9b 대비 감소**(dcomp 115→104, rendering_context 17→9, headed_window 27→2, wall_layout 1→0) = 신규 drift 0. 회귀: scroll_offsets=matched 3134, panic/missed/pending/SetOffset실패 0, 오프셋 2줄 정상. 눈금 이음매 캡처: 경계 x=1920에 `1920,480` 라벨 정확 위치, `1800,480`과 간격 정확히 120px(밀림 시 152px이어야 함). **사용자 육안: 비디오 이음매 반복 없음 = 회귀 판정 통과.**

## 선재 이슈 2건 (이번 작업과 무관 — A/B로 확정, 별도 과제)
- **DComp 비디오 투명 구멍**: DCOMP=1 + 월 다중창에서 display1에 사각 영역이 투명(뒤 데스크톱 노출). 크기 대략 1400x570, 좌변이 정확히 타일 경계 x=1920. 원인 방향 = DComp 켜지면 WindowRenderingContext::present() 스킵이라 **비주얼이 안 덮는 영역은 그릴 주체가 없어 투명**. A/B 확정: ①SERVO_MEDIA_D3D11_VIDEO=0에서도 재현(zero-copy 무관) ②DCOMP=0에서 소멸(DComp 고유) ③**overlapPx=0(inset 전부 0 → 우리 코드 no-op)에서도 재현 = 이번 작업 무관**. 다음 진단: 두 번째 창의 WR picture-cache 타일/DComp 서피스 커버리지.
- **종료 시 MakeCurrentFailed 크래시**: `Gui::drop`(gui.rs:183)에서 make_current 실패 → surfman DestroyContext assert(context.rs:177). 창 2개가 공유 EGL 컨텍스트를 각자 파괴하는 teardown 순서 문제. release에서도 발생(메모리엔 debug 전용으로 기록돼 있었음 — 갱신 필요). overlapPx=0에서도 재현 = 이번 작업 무관.

## 최종 whole-branch 리뷰(opus) + fix wave
- 최종 리뷰: Ready to merge = With fixes, Critical 0. ★핵심 지적(Important): **present_inset의 유일한 독자가 DComp뿐이고 blit 경로는 WallLayout에서 값을 재계산** → 스펙 근거(4) "값이 하나라 어긋날 수 없다"가 미성립. 공유된 건 산식이고 값이 아니었음. 발산 시나리오: present_inset은 생성 시 hidpi 스냅샷 vs blit은 매 프레임 라이브 → 혼합 DPI에서 갈리고 ScaleFactorChanged 재주입 경로도 없었음. **이건 컨트롤러(나)의 설계·계획 누락** — "정본을 만든다"에 기존 계산 지점을 리더로 바꾸는 단계를 넣지 않았다.
- 사용자 결정: 배선까지 제대로.
- fix wave (commit 1f4deab342d): wall_tile_render_insets를 present_inset 리더로(Option 제거) + ScaleFactorChanged 재주입 + 트레잇 문서 정정 + readback 비범위 명시 + DComp 클립 주석 정밀화 + last_root_offset i32화 + 테스트 2개(3x1 중간타일=좌우 양쪽 가드밴드, 분수 DPI)+필드명 assert. 13/13, mach build -r 성공, 스모크 로그 정상.
- **분수 DPI 실측 발견(테스트로 고정, 미수정)**: scale 1.5에서 뷰포트 우변에 flush한 타일이 right=1을 얻음. 원인=rect_to_device_rect가 visible/render 각 rect에 원점 round + 크기 truncate를 독립 적용 → 공유 DIP 경계가 두 device 픽셀로 갈림. 화면 이동은 없음(blit은 left/bottom, DComp는 left/top만 씀) 단 webview_rendering_size가 right/bottom을 써서 오프스크린 서피스 크기가 1px 틀어질 수 있음(gui.rs:610의 visible_size는 또 다른 반올림 경로). 선재 문제, 별도 과제.
- fix wave 재리뷰: Critical 0, Important 1 = **fix wave 자신이 주석 불변식 2건을 깨뜨림**(last_root_offset "런타임 재적용 없음", set_present_inset "1회만 호출") → 교정 커밋. 재리뷰어가 Rc/Cell 공유를 직접 추적해 "Cell 하나, 독자 셋, 고정 DPI에서 발산 없음" 확인.
- 문서 정정 커밋 415fe693a11: 스펙 근거(4) 정정+교훈, 눈금 128 산술오류 일반화, 완료기준 "노란 라벨" 전제 제거, rustfmt 기준 현실화.
- ★남은 사용자 확인: fix wave로 blit 값 출처가 바뀌었으므로 눈금 육안 재확인 1회 권장(가로/세로, DComp on/off).

## DComp 투명 구멍 조사 (2026-07-29, systematic-debugging)
- **구멍 = 커버리지 구멍으로 확정.** SERVO_DCOMP_DEBUG 로그 좌표가 관측 구멍과 정확히 일치:
  tile1이 받는 서피스 = id12 clip(0,0)-(1952,512) [bind 0회] / id14 clip(1434,512)-(1952,1080) / id15 clip(0,24)-(1952,1080) [bind는 row0 타일뿐].
  → fb x0..1434, y512..1080을 덮는 비주얼 없음 → desktop x1920..3322, y512..1080 (실측 ~1920..3316, ~509..1080).
  **tile0에는 창 전체를 덮는 id8 clip(0,0)-(1952,1080)이 있는데 tile1에는 대응물 없음** = 두 painter 비대칭.
- **배제됨**: external 비디오 서피스 경로(로그에 `external add` 0줄 → §4.5 다중창 CONSUMER_DEVICE 가설 기각) / withhold(0줄) / 배리어·스크롤 정상 / 이번 가드밴드 작업(overlapPx=0에서도 재현).
- **사용자 실험으로 확증**: `SERVO_WR_PICTURE_TILE_SIZE=1920x1080` → 구멍 소멸. WR 기본 타일 1024x512라 1080 창은 항상 2행 필요; 타일 1장이 창 전체를 덮으면 2행 자체가 불필요.
- 각 painter는 자기 WR document 보유(painter.rs:530-531 add_document). 구멍은 2번째 painter의 2행째가 비는 현상.
- 페이지 가설 검증: 비디오 프로브 4개 중 **3개는 mp4 파일이 리포에 없어 재생 자체가 안 됨** → "이 페이지에서만"은 페이지를 지목 못 함(비교 무효). 4개 레이아웃은 동일 템플릿.
- 남은 변인 분리: video_minimal_probe(비디오O·넘침X)=구멍 없음 / video_file_probe(비디오O·넘침O)=구멍. **신규 multigpu_wall_overflow_probe.html(비디오X·넘침O)로 판정 대기.**

### 이분 탐색 매트릭스 (2026-07-29, 사용자 실기 판정)
| 프로브 | 비디오 | 문서넘침 | 비디오가 뷰포트 밖 | 장식 | 구멍 |
|---|---|---|---|---|---|
| video_minimal | 소형 | X | X | X | 없음 |
| overflow | X | O | - | X | 없음 |
| video_overflow | 소형 | O | X | X | 없음 |
| video_geom | 대형 | O | O | X | 없음 |
| video_file(원본) | 대형 | O | O | **전부** | **남** |

→ **트리거는 "비디오/넘침/기하"가 아니라 file_probe의 장식 중 하나**로 확정.
신규 `multigpu_wall_video_bisect_probe.html`(쿼리 토글: controls/bg/stage/strip/panel)로 판정 대기.
1순위 = `?controls=1` — 이 페이지에서 비디오 '위에' 그려지는 유일한 요소이고, 사용자 관찰
("구멍=순수 비디오 영역, 다른 요소 있는 곳은 멀쩡") 및 최초 스크린샷(컨트롤 바 바로 아래부터 구멍)과 정합.

### ★가설 2건 폐기 (로그로 직접 반증)
- external compositor surface 승격 가설 **폐기**: dcomp_surf.log에 `create_external_surface`/
  `attach_external_image` **0회**, `create_surface`(일반 픽처캐시)만 16개. 비디오는 승격되지 않는다.
- §4.5 provider(전역 OnceLock) 가설 **폐기**: warn 로그에 `no VideoExternalSurfaceProvider
  registered` 없음 + 애초에 external 경로 자체를 안 탐. (처음엔 로그 부재를 근거로 잘못 기각했다가,
  다시 잘못 채택했음 — 두 번 방향 오류. 로그 부재는 "경로 미사용"과 "조기 반환" 양쪽과 양립함을
  코드로 확인하기 전엔 근거가 못 된다.)
### 캡처 함정
- PowerShell `2>` 리다이렉트가 servoshell stderr를 놓쳐 0바이트 로그가 나옴. **`wall:` eprintln
  줄이 없으면 캡처 실패로 판단할 것**(그 줄들은 RUST_LOG 무관 무조건 출력). 확실한 방법 =
  Start-Process -RedirectStandardError + CloseMainWindow().
- 로그 슬라이스는 프레임 218부터였음 → warn-once 메시지는 전부 그 앞에서 소진. 시작부를 봐야 함.

### ★트리거 확정 (2026-07-29): `<main position:relative>` 래퍼
충실한 기준점(마크업·CSS를 file_probe와 동일하게, 장식은 '빼는' 방식)으로 재측정:
- `REMOVED:(none)` = 원본 → **구멍 남** (기준점 확보)
- `?no=main` (래퍼 제거, 자식을 body로 승격) → **구멍 없음** ← **원인**
- `?no=controls` → 구멍 남 (컨트롤 무관)
`main`은 뷰포트(3840x1080)보다 훨씬 큰 **5760x3240 position:relative 박스**. 월 페이지는
"전체 월 크기로 저작하고 일부만 표시"가 정상 패턴이므로 **엣지 케이스가 아니라 통상 저작 방식**.
다음 좁히기: `?main=static`(relative만 해제) / `?main=small`(relative 유지, 3840x1080)로
`position:relative` 자체인지 "거대 위치지정 박스"인지 특정.

★교훈: 첫 bisect 프로브가 `<main>` 래퍼를 통째로 빠뜨려 5개 토글 전부 음성이 나왔고, 2판은
장식을 JS로 '추가'하는 구조라 기준점이 원본과 달랐다(html+body 이중 배경 누락 등).
**이분 탐색은 반드시 재현되는 원본에서 '빼는' 방향으로, 기준점 재현을 먼저 확인하고 시작할 것.**
토글 적용 여부는 화면에 항상 표시할 것(안 먹은 채 판정하면 전부 무의미).
- 속성 특정 완료: `?main=static`(relative만 해제) → **구멍 없음** / `?main=small`(relative 유지,
  3840x1080) → **구멍 남**. ⇒ 원인은 박스 크기가 아니라 **`position: relative` 속성 자체**.
- 확정된 조건 집합: (비디오) + (문서가 뷰포트 넘침) + (`position:relative` 래퍼) + (**translation이
  걸린 2번째 painter**). tile0은 viewport_origin=(0,0)이라 reference frame이 항등 → 항상 정상.
- 메커니즘 가설(코드 미확인): relative = positioned box → Servo DL에서 스태킹 문맥/참조프레임 발생.
  거기에 월 팬아웃이 painter마다 거는 root reference frame 이동(painter.rs:1276-1277,
  `push_reference_frame(-viewport_origin)`; tile1은 -1888)이 겹치면서 WR picture-cache
  슬라이스/타일 할당에 빈 영역이 생긴다. 이 가설은 관측 4개를 모두 설명한다:
  2번째 창에서만 / 타일크기≥창이면 소멸 / 비디오 필요(별도 슬라이스 유발) / overlapPx 무관(0에서도 origin=-1920).
- **페이지측 무비용 회피책 발견**: 래퍼의 `position: relative` 제거(=static). main이 문서 원점에
  있으면 절대배치 자식 위치가 동일해 시각적 차이 없음. 엔진측 회피책은 SERVO_WR_PICTURE_TILE_SIZE=1920x1080.
- 다음: Servo `components/layout/display_list/stacking_context.rs`의 relative 처리 + WR
  picture-cache 슬라이스 할당 경로 확인. 최소재현은 subtract_probe로 확보됨.

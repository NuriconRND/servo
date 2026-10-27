# Runs the packaged multi-GPU wall (pref-era: engine knobs are --pref, not env vars).
#
# ★이 파일은 UTF-8 **with BOM** 이다 -- BOM 을 떼면 파싱이 깨진다.★
#
# 여기에는 "Pure ASCII on purpose -- a Korean launcher failed to parse on a test machine"
# 라고 적혀 있었다. 그 규칙이 생긴 이유는 실재했다: BOM 이 없으면 PowerShell 5.1 은 이
# 파일을 ANSI(여기서는 cp949)로 읽고, 그 디코더는 짝이 안 맞는 선행 바이트를 만나면 뒤의
# 한 바이트를 같이 삼킨다. 한국어로 끝나는 주석 줄에서 그 한 바이트가 ***개행***이라,
# 다음 줄이 통째로 주석 안으로 끌려 들어가고 파서는 엉뚱한 자리에서 "Unexpected token '}'"
# 를 낸다(2026-10-22 재현: PAINTERLOCK 절을 넣자마자 터졌다).
#
# 그런데 그 규칙은 ***이미 지켜지지 않고 있었다*** -- 아래 주석의 대부분이 한국어다. 지금까지
# 멀쩡했던 것은 그 줄들이 전부 ASCII 구두점으로 끝나 개행이 살아남았기 때문이고, 그것은
# 규칙이 아니라 운이다. 그래서 규칙을 다시 적는 대신 ★원인을 없앤다★: BOM 이 있으면
# PowerShell 이 UTF-8 로 읽고 이 함정 자체가 사라진다.
#
# 편집 도구가 BOM 을 떼지 않게 할 것. 확인: `Get-Command .\run_wall_dist.ps1 -Syntax`
# 가 조용히 끝나면 된다 -- 깨져 있으면 거기서 파스 에러가 쏟아진다.
#
# RUST_LOG is set UNCONDITIONALLY. It used to be `if (-not $env:RUST_LOG)` and that
# silently defeated the diagnostics twice: a value left over in the shell (without
# "media=info") won, so the WALLDIAG lines from the media-thread crate never appeared.
# NOTE that crate logs under the target "media", NOT "servo_media_thread" -- its
# Cargo.toml sets [lib] name = "media".

# ★[CmdletBinding()] 을 빼지 말 것.★ 이것이 없으면 PowerShell 은 모르는 명명
# 파라미터를 **조용히 무시하고 그냥 돈다** -- 오류도 경고도 없다. 이 벽의 A/B 는
# 전부 이 스크립트의 스위치로 설정되므로, 배포본이 오래되면 새 스위치가 말없이
# 사라지고 런은 완벽하게 정상으로 보인다. 실제로 2026-09-17 에 -VsyncPhase 를
# 넘긴 네 번의 실기가 통째로 날아갔다(옛 스크립트 + 기본값으로 네 번 같은 런).
# 엔진 쪽은 시끄럽다 -- 모르는 pref 이름은 set_value 가, 타입 불일치는
# try_into().unwrap() 이 패닉한다. 조용한 구멍은 여기 하나뿐이었다.
[CmdletBinding()]
param(
    [string] $Layout = "wall_layout.multigpu.json",
    [string] $Url = "",                  # empty = bundled 6x6 page
    # Serve pages\html over http instead of handing the engine a file:/// URL, and shut the
    # server down when the run ends. Needed for WebGPU and for any page using ES modules:
    #
    #   * ES modules do not load over file:// ("Unsupported scheme").
    #   * ***WebGPU hangs on file:// with no error at all.*** The constellation keys the
    #     WebGPU thread by host name, a file:// URL has an opaque origin and therefore no
    #     host, and that path warns "Invalid host url" and returns WITHOUT answering the
    #     response channel -- so requestAdapter() never resolves and never rejects. The page
    #     just sits there. Run with RUST_LOG=warn and look for that line if unsure.
    #
    # With -Serve a relative -Url ("multigpu_wall_webgpu_min_probe.html", query string
    # allowed) is resolved against the server root; an absolute http/file URL is left alone.
    [switch] $Serve,
    [int]    $ServePort = 8731,
    # Pass --ignore-certificate-errors to the engine, for an -Url on https with a
    # self-signed or internal-CA certificate. This is a FLAG, not a pref, so -Pref cannot
    # carry it, and winit_wall rejects unknown --flags rather than ignoring them.
    [switch] $IgnoreCertErrors,
    # The content features the output pages need. ***Every one of these is OFF in the
    # engine by default***, and nothing in this launcher used to turn them on, so a page
    # using any of them silently rendered nothing:
    #
    #   dom_video_network_uri_enabled          rtsp:// and rtsps:// in a plain <video>.
    #                                          ***Without this an RTSP tile is simply blank.***
    #   dom_video_extended_containers_enabled  Matroska/AVI/WMV/MPEG-TS/FLV in <video>
    #   dom_image_extended_formats_enabled     TIFF/EXR/HDR/TGA/DDS/QOI/PNM/JPEG-XL in <img>
    #   dom_webrtc_enabled                     WebRTC
    #   dom_screen_capture_enabled             getDisplayMedia
    #   dom_webgpu_enabled                     WebGPU (also needs an engine built with the
    #                                          `webgpu` cargo feature -- the dist is)
    #
    # One switch rather than six -Pref arguments, because re-typing six is how a run ends
    # up quietly missing one. -Pref still wins over these: it is appended after.
    [switch] $PageFeatures,
    # Devtools listen address, e.g. -Devtools 127.0.0.1:7000. Empty = devtools off.
    # ***Deliberately NOT part of -PageFeatures: this opens a listening socket.*** Bind to
    # loopback unless you actually need to attach from another machine, and note the engine
    # auto-approves connection requests (this shell has no UI to prompt with).
    [string] $Devtools = "",
    [int]    $Rows = 6,
    [int]    $Cols = 6,
    [int]    $SyncGroup = 0,             # 0 = auto (Rows * Cols)
    # Turn the synchronized group start OFF. It is worth an A/B because releasing
    # the group pins every pipeline to a SHARED base time with start_time=NONE, so
    # running time tracks the wall clock and is never re-based. Together with the
    # smooth policy (qos=false, drop=false, max-lateness=-1) that leaves a pipeline
    # no way to recover once it falls behind -- it just decodes flat out forever.
    [switch] $NoSyncGroup,
    [int]    $DecoderThreads = 1,
    [string] $TileSize = "display",      # gfx_wr_picture_tile_size
    [int]    $RefreshHz = 60,
    # gfx_dcomp_mode. ***Defaults to off, which is also the engine's own default*** -- the
    # launcher used to default to "surface" and that disagreement was never revisited after
    # the DComp investigation that introduced it.
    #
    # On the standard set (escape=off) the native compositor costs 2.5x: a paired A/B on the
    # test machine, same page back to back, measured 61.9fps off against 24.9fps on, a pass
    # at 16.14ms against 42-56ms, and a mean painter render of 0.83ms against 7.75ms with
    # slow frames dropping from 1471 to 53. The cost is IDCompositionSurface::BeginDraw and
    # EndDraw, which the WR compositor calls from bind()/unbind() for every invalidated
    # picture-cache tile -- which is also why a page with no WebGL canvas was fast: with the
    # cache fully valid, bind() is never called.
    #
    # It is still worth turning on for the video escape path, where it cut draw calls from
    # ten to one -- and that path REQUIRES it (see the check below).
    [ValidateSet("off", "on", "surface")]
    [string] $DComp = "off",
    # gfx_vsync_enabled. 기본 off.
    #
    # ★2026-09-17 부터 이 스위치는 둘을 한다.★ 원래는 DwmVsyncRefreshDriver 를 세워
    # **콘텐츠 생산**만 DWM 합성에 맞췄고 셸의 표출 클럭은 건드리지 않았다 -- 그래서
    # 켜도 화면상 변화가 없었고, 그 결과를 "vsync 는 원인이 아니다" 로 읽을 뻔했다.
    # 이제는 셸의 표출 클럭(drive_present_clock)도 같은 드라이버를 구독한다.
    #
    # 왜 필요한가: 소프트 타이머는 정확히 60 개를 내도 디스플레이와 다른 발진기라,
    # 화면에 보이는 위치가 scanout 직전 0~한 주기 사이의 표본이 되어 프레임마다
    # 흔들린다(초당 2,304px 물체에서 60Hz 면 0~38px). vsync 에 묶으면 그 오차가
    # 상수(한 vsync 지연)가 되어 사라진다. -RefreshHz 를 올리는 것은 오차를 작게
    # 만들 뿐이고 프레임 빌드 비용이 배수로 는다.
    #
    # 치르는 값: 이 장비에서 DwmFlush 가 스핀-웨이트라 코어 1 개를 상시 먹는다.
    # 셸이 매 vsync 재등록하므로 그 스레드는 더 이상 유휴로 잠들지 않는다.
    #
    # 판정: WALLCLOCK 의 backstop 필드. 0 이면 vsync 가 몰고 있는 것이고, 오르면
    # 신호가 끊겨 자유 구동 타이머로 내려앉은 것이다(로그에 warn 도 같이 나간다).
    [switch] $Vsync,
    # gfx_vsync_phase_pct: vsync 틱과 실제 표출 사이의 위상 오프셋(vsync 주기의 %).
    # 기본 0. -Vsync 와 같이 쓸 때만 의미가 있다.
    #
    # ★왜 있나★ -Vsync 로 묶었더니 오히려 나빠졌는데(log_ani_debug/30, 31) 개수
    # 계수는 전부 정상이었다 -- 떨림은 "값을 뜬 시각과 화면에 나가는 시각의 간격"
    # 이고 그것을 재는 계수가 없다. 관찰된 모양(자유 구동일 때 나쁜 구간의 상태가
    # 재생 내내)은 위상이 한 자리에 고정됐는데 그 자리가 scanout 경계라는 뜻이다.
    #
    # 0 / 25 / 50 / 75 를 쓸어 보라. 좋은 자리가 있으면 위상 가설이 확정되고 값도
    # 같이 나온다. 어느 값도 좋지 않으면 위상이 아니므로 -Vsync 를 접으면 된다.
    [ValidateRange(0, 99)]
    [int]    $VsyncPhase = 0,
    # gfx_present_sync_interval: 타일 스왑체인 Present 의 SyncInterval. 기본 0.
    #
    # 0 = 어떤 패널의 vblank 도 기다리지 않는다(현재 동작). 1 = 타일마다 자기 출력의
    # vblank 에 맞춘다.
    #
    # ★왜★ 값 경로를 다 고친 뒤에도 남은 저더가 있는데, 같은 빌드로 세 번 돌린
    # 결과가 갈렸고(120초 내내 깨끗 / 타일마다 튐 / 섞임) 세 런의 계수가 전부 같았다
    # (log_ani_debug/39). present 위쪽으로는 그 차이를 설명할 수 없고, 지금 present 는
    # 셋 다 SyncInterval 0 이다. 네 패널은 genlock 되지 않은 독립 60Hz 라 -Vsync(DWM
    # 합성 클럭 하나)로는 못 맞춘다 -- 정렬은 타일마다여야 한다.
    #
    # 1 로 켜면 큐가 찼을 때 호출 스레드가 블록된다. primary 타일의 그 스레드는
    # 메인이므로, WALLPASS 의 pass_ms 와 WALLCLOCK 의 gap/off 를 반드시 같이 보라.
    [ValidateRange(0, 4)]
    [int]    $PresentSync = 0,
    # gfx_present_align_dwm_pct: 표출 클럭을 DWM 합성 격자에 스냅한다.
    # -1(기본) = 끔, 0..99 = DWM 주기의 그 백분율 지점을 겨냥.
    #
    # ★이것이 저더의 원인이고 해법이다.★ 실측(log_ani_debug/47): 같은 빌드·같은
    # 설정인데 깨끗한 런은 커밋이 DWM 주기의 8% 지점에, 저더 런은 60% 지점에
    # 떨어졌고 둘 다 44창 내내 그 자리를 유지했다. DWM 은 vblank 이전에 미리 합성을
    # 시작하므로 60% 지점 커밋은 마감에 걸터앉아 프레임마다 되느냐 마느냐가 갈린다.
    # 어느 자리에 앉을지는 지금 기동 순간의 운이다.
    #
    # 켜면 매 틱 측정된 vblank 로부터 다시 세므로 기동 위상과 무관하게 같은 자리에
    # 떨어진다. 자유 구동 타이머에 상수를 더하는 것과는 다르다 -- 그건 임의 기준점에
    # 상수를 더하는 것이라 여전히 임의다.
    #
    # 커밋은 틱보다 렌더 시간(약 2ms = 주기의 0.12)만큼 늦으므로 0 이 vblank 직후를
    # 겨냥한다. DWMPHASE 의 phase p50 으로 확인하라 -- 0.1 근처면 좋은 자리다.
    #
    # 대기가 아니다. 타이머가 깨어나는 시각만 바뀌므로 생산 스레드가 공유 객체에
    # 줄 서는 회귀(GstSystemClock 사건)가 구조적으로 생기지 않는다.
    [ValidateRange(-1, 99)]
    [int]    $DwmAlign = -1,
    # gfx_present_align_commit_pct -- B3. DComp Commit 을 공통 합성 격자 위의 고정된
    # 위상에 내보낸다. -1 = 꺼짐(기본), 0..99 = 합성 주기의 그 백분율 지점.
    #
    # 커밋은 그동안 렌더 패스의 **끝**에서 나갔고, -DwmAlign 은 패스의 **시작**만
    # 고정한다. 그래서 커밋 위상이 패스 길이를 그대로 물려받았고, 그 산포가 저더의
    # 두 증상을 모두 만들었다 -- 큰 탈선은 합성 마감을 스치고(MISSEVENT nc=1), 그
    # 흔들림이 커밋 간격을 흔들어 합성 경계를 넘나든다(nc=0).
    #
    # ★-DwmAlign 의 바로 앞에 두지 말 것.★ 여유는 틱에서 마감까지의 **전방** 거리라
    # (CommitAlign - DwmAlign) mod 100 이고, 그것이 15%p 미만이면 마감이 패스보다
    # 먼저 와서 거의 매 프레임 즉시 커밋으로 떨어진다 -- 기능이 켜진 채 아무 일도
    # 하지 않는다(프로브의 패스 p95 가 주기의 13.4%). 엔진이 기동 로그에 경고를 한 줄 낸다.
    #
    # ★CommitAlign < DwmAlign 은 적법하다★ -- 마감이 다음 주기의 그 지점이 되므로
    # 여유가 거의 한 주기로 가장 많다. 대신 합성까지의 여유는 가장 적다.
    #
    # ★낡음(2026-09-21 기록, 2026-10-06 `1cdfd52cf3d` 이후 거꾸로다)★ — -CommitAlign 을 쓸 때는 -DcompParallelCommit 을 ***같이 줘야*** 하고, 빼면 엔진이 직렬화 경고를 낸다.
    # 켜면 -DcompParallelCommit 은 무시된다(목적이 겹친다). 기동 로그에 남는다.
    # ★판정은 TICKCOMMIT 의 spread 다★ -- 0 에 가까워져야 성공이다.
    [ValidateRange(-1, 99)]
    [int]    $CommitAlign = -1,
    # gfx_sample_lead_periods -- B2. 타일마다 자기 출력이 그 프레임을 표시할 시각에
    # 애니메이션을 샘플한다. -1 = 꺼짐(기본), 0 = 다음 vblank, 1 = +1 주기 (0..4).
    #
    # B1(출력별 vblank 겨냥, 지금은 B3 -CommitAlign 으로 교체)이 커밋 시각만 옮겨 못 고친 것을 고친다. 실기에서 B1 을 완전히
    # 동작시켜도 저더가 줄지 않았다(설계 문서 `## 실기 결과`).
    #
    # ★이음매에 고정 어긋남이 생긴다 -- 그것이 이 거래의 내용이다.★ 속도 × 출력 간 위상차
    # 이고, 1x 프로브(2,304px/s)에서 최대 26px, 500px/s 에서 5.6px. 판정 기준은 "이음매가
    # 붙었나" 가 아니라 "타일 안에서 등속으로 매끄럽나" 다. 이음매 기하를 같이 보려면
    # 느린 객체(1/4x)로 봐야 한다.
    #
    # 판정은 -DcompBindProf 의 SAMPLELEAD 줄 -- 네 출력의 lead_ms 가 서로 위상차만큼
    # 벌어져 있어야 한다. 전부 같으면 B2 가 걸리지 않은 것이다.
    #
    # ★-DwmAlign 과 같이 써야 한다.★ 이것만 켜면 자유 실행하는 렌더를 합성 격자에 스냅하는
    # 셈이라, 가끔 두 렌더가 같은 격자점에 걸려 그 프레임의 변위가 0 이 되고 다음이 두 칸을
    # 뛴다. -DwmAlign 이 렌더 틱 자체를 같은 격자에 잠가 프레임↔합성을 1:1 로 만든다.
    [ValidateRange(-1, 4)]
    [int]    $SampleLead = -1,
    # gfx_wall_direct_render: 표출 클럭 틱에서 request_redraw() 를 거치지 않고 그 자리에서
    # 바로 타일을 그린다. 기본 꺼짐(= 종전 경로).
    #
    # 화면 표출은 IDCompositionDevice::Commit(순수 COM)이라 메시지 펌프와 무관한데,
    # request_redraw() 는 Windows 에서 WM_PAINT 를 요청하고 그것은 posted message 큐가 다
    # 빈 뒤에야 합성되는 최저 우선순위 메시지다. 이 셸은 엔진 wake 를 초당 300 회 받으므로
    # 렌더가 그 큐 뒤에 선다 -- 틱 간격 p95 17.23ms(주기 16.667ms)의 유력한 후보다.
    #
    # 렌더 스레드는 그대로 메인이다. 바뀌는 것은 누가 언제 부르나뿐이다.
    [switch] $DirectRender,
    # gfx_wall_pacing_thread: 표출 틱을 전용 스레드가 올린다. 기본 꺼짐.
    #
    # ★실제 부하에서 박자가 엔진에 얹힌다.★ 실측(log_ani_debug_02/20, 애니메이션 5 개):
    # 틱이 초당 56.0(최악 41, 60 이어야 한다), about_to_wait 진입이 185 -> 132(최악 56),
    # 틱의 37% 를 타이머가 아니라 엔진 wake 가 알아챈다(프로브 2%). gap_ms p95 17.23 -> 29.08.
    #
    # about_to_wait 은 winit 이 이벤트 큐를 다 비운 뒤에만 도달하는 자리인데 표출 클럭의
    # WaitUntil 이 거기 걸려 있다. 켜면 깨우는 일만 스레드로 옮긴다 -- 렌더는 그대로 메인이다.
    #
    # 판정: WALLCLOCK 의 tick_by pace 가 대부분이고 ticks 가 60 으로 돌아오는가.
    [switch] $PacingThread,
    # gfx_wall_pacing_vblank=false: 페이싱 스레드를 **예전** 경로로 되돌린다.
    # -PacingThread 가 없으면 아무 일도 하지 않는다.
    #
    # 기본(이 스위치 없음) = WaitForVBlank. primary 의 vblank 에서 커널이 깨우므로 DWM
    # 조회도 대기 타이머도 경로에 없다.
    # 이 스위치를 주면 = DwmGetCompositionTimingInfo 로 격자를 묻고 -DwmAlign 지점까지
    # thread::sleep. 그 둘(프로세스 간 호출 지연 + 타이머 깨우기 지터)이 그대로 틱 위상의
    # 지터가 되고, 커밋 위상이 저더를 정하므로 공짜가 아니다.
    #
    # ★-DwmAlign 과 같이 보라.★ vblank 경로라도 -DwmAlign 1..99 면 vblank 뒤에 그만큼 또
    # 자므로 타이머 지터가 돌아온다 -- 앵커만 좋아진 예전 구조다. -DwmAlign 0(또는 생략)
    # 일 때만 대기가 하나도 없는 순수 신호 경로이고, 실측 최적점도 거기다.
    #
    # 판정: WALLCLOCK 의 jit_ms 와 gap_ms p95, 그리고 COMPREFRESH 의 refresh_d[2].
    [switch] $PacingDwmQuery,

    # gfx_wall_pacing_output: 페이싱 스레드가 WaitForVBlank 를 걸 출력을 이름으로 못 박는다.
    # "" = primary(오늘 동작). "\.\DISPLAY22" 처럼 DXGI 출력 이름을 준다.
    # ★왜★ 코드가 전제하던 "DWM 합성은 primary 의 vblank 에 물려 있다" 가 이 벽에서 깨졌다.
    # 2026-10-26 같은 바이너리로 돈 네 런에서 DWM 격자에 붙은 출력이 런마다 달랐고, 어긋난
    # 런은 커밋 위상 52% 에 dup 4.025%, 붙은 런은 0.902% 였다. 먼저 OUTPHASE 로 그 세션의
    # vs_dwmvb_ms 가 0 인 출력을 확인한 뒤 출력마다 한 번씩 돌리면 인과가 증명된다.
    # 로그의 VBANCHOR 줄이 실제로 어느 출력을 기다렸는지 찍는다.
    [string] $PacingOutput = "",
    [string] $VideoEscape = "",          # gfx_video_escape_mode; "external" to enable
    # gfx_video_escape_buffer_count: back buffers on each escaped video's flip swap chain.
    # 0 = leave the pref alone (engine default 2 = current behaviour).
    #
    # With 2 buffers only one frame can be in flight, so Present waits for the compositor to
    # release a back buffer. Measured at 45 videos: the renderer thread spends 85% of every
    # second (854ms) inside Present while using under 0.3 cores -- it is sleeping, not working.
    # Per-call cost also grows with the video count (0.42ms at 20, 0.70ms at 45), which a fixed
    # driver submission cost would not do. Content swap chains are NOT affected (their partial
    # Present catch-up copy requires exactly 2).
    [ValidateRange(0, 4)]
    [int]    $VideoEscapeBuffers = 0,
    # appsink qos, isolated from the sink policy. Empty = policy default (Smooth => off).
    # With qos off the decoder cannot skip frames under load, so an overloaded wall falls
    # off a cliff instead of degrading. See media_video_sink_qos in configuration.md.
    [ValidateSet("", "on", "off")]
    [string] $SinkQos = "",
    # media_video_sink_policy. Changes qos AND drop AND max-lateness AND max-buffers at
    # once -- use -SinkQos when you want to move only qos.
    [ValidateSet("", "smooth", "low-latency")]
    [string] $SinkPolicy = "",
    # media_video_sink_pacing. "thread" turns the sink's clock wait off and paces in the
    # streaming thread instead.
    #
    # GstSystemClock::obtain() is a per-PROCESS singleton, so 45 pipelines' sinks all wait on
    # the same object every frame. Measured on 80 logical cores with 45 x FHD30, decode only,
    # no Servo: 45 separate processes 0.399 cores/video, one process 0.795, one process with
    # the clock replaced by a plain sleep 0.284. The shared clock costs as much as the
    # decoding does. NOTE: "thread" gives each pipeline its own anchor, so videos are NOT
    # synchronised with each other -- that is a separate task (Video Sync Group).
    [ValidateSet("", "clock", "thread", "none")]
    [string] $SinkPacing = "",
    # media_audio_enabled=false: unset playbin3's audio + soft-volume flags.
    #
    # ***NO PERFORMANCE BENEFIT. MEASURED 2026-08-26.*** It was tried expecting one: the wall
    # plays muted video and never decodes audio, yet every pipeline still carries aacparse +
    # audiotee + two streamsynchronizer identities AND an audio pad on both multiqueues --
    # 2 of the 4 pad tasks per video. Unsetting the flag changed NOTHING: with
    # disable_audio=true the element list is byte-for-byte identical (aacparse 20, tee 20,
    # identity 40 at 20 videos) and multiqueue still runs 4 tasks per video. 20-video A/B:
    # 6.31 vs 6.25 cores on multiqueue:src, 80 threads either way.
    #
    # The flag only removes the audio SINK chain. Demuxing, parsing and stream
    # synchronisation happen regardless, because the file still contains an audio stream and
    # qtdemux/parsebin/multiqueue serve it whether or not anything consumes it. Dropping
    # that work needs stream SELECTION (decodebin3's select-stream), not a playbin flag --
    # which lands in the uridecodebin3 work, not here.
    #
    # Kept because the sink chain does go away and the wall is muted, so it is harmless and
    # it is where stream selection will hook in later. Do not expect it to buy anything now.
    [switch] $NoAudio,
    # media_pipeline_mode. uridecodebin3 builds the pipeline without playsink, so the
    # vqueue between the decoder and our appsink is gone -- that queue carries every raw
    # 3.1MB frame across a thread boundary and measured as much CPU as the decoding
    # itself (45 x FHD30, cores/video: no queue 0.284, front queue only 0.294, front+back
    # 0.729). decodebin3 still autoplugs the codec, so H264/H265/VP9 keep working.
    #
    # SPIKE: falls back to playbin3 with a warning when it cannot be used (a renderer that
    # does not hand playbin the appsink itself, or non-local-file playback). Track
    # selection is not implemented on this path.
    [ValidateSet('', 'playbin3', 'uridecodebin3')]
    [string] $PipelineMode = '',
    # gfx_wall_frame_max_pending: how many logical frames the coalescer may hold. The engine
    # default is 1, which means nothing overlaps -- producing frame N+1 cannot start while
    # frame N is being painted. Worth raising when passes/s sits far below what pass_ms says
    # the loop could do (45 tiles: 33/s against a 10.4ms pass, so ~96/s of capacity).
    [int]    $MaxPending = 0,            # 0 = leave the engine default (1)
    # gfx_wall_frame_min_interval_ms. ***The engine default is 0, not 16.*** 0 means "derive
    # one period from gfx_refresh_hz", which -RefreshHz sets to 60 here, so the floor is
    # 16.667ms. The old fixed 16 could not express 60Hz at all (it is 62.5Hz).
    [int]    $MinIntervalMs = 0,         # 0 = derive from -RefreshHz (60 -> 16.667ms)
    [int]    $DurationSec = 0,
    [string] $LogPath = "",
    # Write no log at all: engine stderr goes to the Windows null device, and the post-run
    # analysis is skipped along with it (it has nothing to read). Use it for a demo or a
    # long unattended run where the log would just grow.
    #
    # Runs stay ***unlimited*** by simply not passing -DurationSec; that is separate from
    # this switch. Wall tile windows ignore WM_CLOSE, so stop such a run with
    # `Stop-Process -Name winit_wall -Force`.
    [switch] $NoLog,
    [switch] $KeepRustLog,
    # 로그 ***분량***만 줄이고 판정 지표는 남긴다. RUST_LOG 을 `error,paint=warn` 으로 세운다.
    #
    # judder 판정에 쓰는 줄은 전부 `paint` 크레이트의 warn! 이다 -- COMPREFRESH / OUTPHASE /
    # VBANCHOR / TICKCOMMIT / COMMITCYCLE 이 paint::output_grid, DWMPHASE 가
    # paint::dcomp_compositor. 반면 바이트의 92% 는 `paint` 의 INFO 다(Wall render start/end,
    # [dcomp-dbg] ...). 그래서 paint 를 warn 으로 내리면 ***숫자는 전부 남고 분량만 빠진다.***
    # 2026-10-27 실측(log_fullthrottle_arch/07, multigpu): 기본 1,940 줄/s · 597 KB/s 에서
    # 약 121 줄/s 로, 줄 1/16 · 바이트 1/13 (10/d_* 실측 2.55 MB 대 10/e_* 36.7 MB).
    #
    # ***무엇을 가르려고 있는 스위치인가.*** -NoLog 로 돌리면 judder 가 심해진다는 관찰이
    # 나왔는데, 판정 카운터 자체가 로그 줄이라 -NoLog arm 에는 숫자가 영영 없다. 그래서
    # "로그 분량" 과 "측정 가능" 이 같이 움직여 버려 원인을 가릴 수가 없었다. 이 스위치가
    # 그 가운데 점이다: 분량은 1/13 인데 dup% 는 그대로 나온다. 기본 / -QuietLog / -NoLog
    # 세 점을 같은 processor group 에서 3 회씩 돌면 분량-반응 곡선이 선다.
    #
    # 주의: 이걸 주면 paint 의 INFO 를 먹고 사는 분석 절(WALLSPLIT, DCOMPBIND, COMMITWORK,
    # FRAMEID 등)은 비어서 나온다. 그건 고장이 아니라 이 스위치의 대가다.
    [switch] $QuietLog,
    # 엔진 stderr 를 `Start-Process -RedirectStandardError` 로 돌리던 옛 경로로 되돌린다.
    #
    # ***그 경로는 측정을 망가뜨린다.*** -RedirectStandardError 는 자식의 stderr 를 파이프로
    # 묶고 PowerShell 이 퍼낸다. 2026-10-27 에 같은 장비에서 120,000 줄 × 299 B 를 직접 재 봤다
    # (레코드마다 write+flush, env_logger 와 같은 모양, 3 회 반복):
    #
    #   Start-Process -RedirectStandardError <파일>   쓰기당 127.4 us   최악 1 회 25.4 ms
    #   cmd  2> "<파일>"  (상속 핸들)                  쓰기당   7.1 us   최악 1 회  0.9 ms
    #   Start-Process -RedirectStandardError NUL        쓰기당   1.0 us   최악 1 회  0.05 ms
    #
    # 18 배다. 그리고 최악 1 회 25.4 ms 는 프레임 주기 16.67 ms 보다 길다. env_logger 는
    # 레코드마다 전역 stderr 락 + write_all + flush 를 하므로 그 비용을 로그를 호출한 스레드가
    # 그대로 문다. 기본 RUST_LOG 의 벽 런은 1,928 줄/s 를 쓰니 옛 경로에서는
    # ***초당 245 ms*** -- 벽시계의 1/4 -- 을 로그 안에서 멈춰 있었다.
    #
    # 그래서 로그를 켜면 judder 가 줄고 끄면 심해졌다(log_fullthrottle_arch/08: 블로킹
    # A 245 / B 14.5 / C 1.9 ms/s, 육안 judder A < B < C). 계측이 브레이크였다. 이제
    # NUMA 핀 여부와 무관하게 cmd 래퍼를 쓴다 -- 같은 로그를 남기면서 블로킹이 1/18 이다.
    # -QuietLog 와 함께 쓰면 0.8 ms/s 로, -NoLog(1.9 ms/s)보다도 낮으면서 판정 카운터는 남는다.
    #
    # 이 스위치는 옛 경로와 비교해야 할 때만 쓴다. 평소에 쓸 이유가 없다.
    [switch] $LegacyLogRedirect,
    # SERVO_D3D11_PROFILE=1: per-pipeline stage timings, one heartbeat line per second per
    # video plus any frame over the threshold. The copy= field is the CPU memcpy of the
    # decoded planes into GPU-mapped memory -- the thing to look at when decode is cheap
    # but the wall still saturates CPU.
    [switch] $D3d11Profile,
    # SERVO_D3D11_PROFILE_MS threshold in ms (engine default 8). The engine logs a frame
    # when total >= threshold, so 0 means EVERY frame -- which is why this must be
    # distinguishable from "not supplied". A `-gt 0` guard used to swallow `0` and
    # silently leave the engine default in place, so a run asked for full logging and
    # quietly measured something else.
    [int]    $D3d11ProfileMs = -1,
    # SERVO_MEDIA_VIDEO_RATE=1: one VIDEORATE line per second per video.
    #   fps      = frames the appsink actually received
    #   pts_rate = how fast pts advances against the wall clock
    # 1.00x means the pipeline plays at normal speed; 2.7x means the decoder is
    # running that far ahead, which is the difference between a throttling bug and
    # a machine that is simply contended. Off by default: 45 lines a second.
    [switch] $VideoRate,
    # ★낡음(2026-08-25 기록, 2026-08-31 `4ff3075ca97` 이후 무효)★ — 이 env 는 없다. 설정하면 기동이 막히고, 이 스위치는 `--pref gfx_video_immediate_composite_enabled=false` 를 보낸다.
    # SERVO_DISABLE_VIDEO_IMMEDIATE_COMPOSITE=1: stop re-compositing the whole scene
    # every time a video frame arrives.
    #
    # By default painter.rs::update_images calls generate_frame(SCENE) on each video
    # arrival -- its own comment says it 're-renders the full current display list'
    # and warns it worsens 'as the number of simultaneous videos grows'. The only
    # brake is pending_frames == 0, so the rate is set by how fast a composite
    # completes, NOT by the display refresh: 20 videos measured 236 presents/s on a
    # 60Hz wall. With this set, video frames ride the script rendering-opportunity
    # cadence instead.
    #
    # ★2026-08-31: the engine no longer offers that all-or-nothing choice.★ Video arrivals
    # still drive composites, but COALESCED to the gfx_refresh_hz cadence, so the request
    # rate is the paint cadence rather than the SUM of every video's frame rate. Both
    # extremes measured badly on the 4-GPU wall: per-arrival was 39.19 cores vs 22.72 with
    # the path off, and off entirely dropped a video-only page to 28 composites/s -- below
    # the content's own 30fps. This switch now turns the path OFF ENTIRELY, which is only
    # useful as an A/B arm; it is not the recommended setting.
    [switch] $NoImmediateComposite,
    # No-op: video-driven composites are the engine default again (coalesced). Kept so a
    # script that passes it keeps working and reads correctly.
    [switch] $ImmediateComposite,
    # SERVO_FRAME_REASON_PROF=1: one line per second naming WHICH call site asked for each
    # ★낡음(2026-08-25 기록, 2026-09-09 `9eeb4f911ed` 이후 무효)★ — 11 곳이고 `present_external_only` 는 여기 안 찍힌다.
    # composite. There are nine generate_frame call sites; gfx_refresh_hz only paces the
    # renderer tick (60Hz) and script runs its own 20/30ms timer, so the two together cap
    # around 110/s -- yet a single painter was measured at 200+/s. This says who.
    [switch] $FrameReason,
    # SERVO_WR_SLOW_MS=<n>: the renderer logs a full WRSLOW breakdown (24 fields: pre_draw,
    # unlock, gpu_cache_resolve, draw_frame, tex_cache, upload, ...) for every frame whose
    # renderer time exceeds <n> ms.
    #
    # ***The default is 100 ms, which is useless for stutter.*** At 60 Hz anything over
    # ~16.7 ms has already missed a frame, so judder lives entirely between 17 and 100 and
    # the default threshold never fires. Pass 20 to catch it. WRRATE cannot answer this
    # question at all -- it averages over a one-second window, and an average is exactly
    # what a stutter hides behind: 57 fps of even frames and "60,60,60,20,60" read the same.
    [int]    $WrSlowMs = 0,
    # SERVO_MEDIA_SINK_PROF=1: one line per second per video splitting the appsink callback into
    # pace / diag / build / render / notify.
    #
    # ***D3D11PROF only sees inside build_frame, and that turned out to be 6% of the thread.***
    # 45 videos measured 1.56 ms per frame there (0.047 cores per video) while the streaming
    # thread actually burns 0.76. Subtract the 0.36 pure-decode ceiling and 0.35 cores per video
    # -- 16 cores at 45 videos -- are still unaccounted for. Outside build_frame the callback
    # only does four things, so splitting them names it. notify is the one to watch: it is an
    # IpcSender send per frame, 1350 a second at 45 videos.
    [switch] $SinkProf,
    # SERVO_WEBGL_FANOUT_PROF=1: one line per second breaking down what a WebGL canvas costs
    # when it spans several GPUs. Every WebGL command is replayed once per backend device, and
    # the replay loop runs the DEVICE loop inside the COMMAND loop -- so the "if needed" guard in
    # make_surface_current never hits and N commands across 4 devices cost 4N context switches,
    # 4N global ANGLE lock acquisitions and 4N postcard round-trips.
    #
    # ***The pixel explanation is already ruled out.*** Halving the canvas with ?scale=0.5
    # changed nothing, and all four tiles come in at ~17ms each regardless of which region they
    # own. Read `switches` against `applies`: equal means every command changes context. If
    # switch_ms + lock_wait_ms dominate the window, inverting the loop (4N switches -> 4) is the
    # fix; if apply_ms dominates it is real GL work and inverting the loop buys nothing.
    #
    # Pair it with -PresentCadence: that reports angle_lock_ms per tile, which is the other half
    # -- whether the tiles are slow doing their own work or waiting on the WebGL thread's lock.
    [switch] $FanoutProf,
    # SERVO_DCOMP_BIND_PROF=1: splits the DComp tile bind/unbind round trip into BeginDraw,
    # the EGL pbuffer wrap, EndDraw and teardown, plus the dirty area against the tile area.
    #
    # This is the follow-up to the A/B that turned the native compositor off: the wall went
    # 24.9 -> 61.9fps and the mean painter render 7.75 -> 0.83ms, and the cost was
    # IDCompositionVirtualSurface::BeginDraw/EndDraw for each invalidated picture-cache tile.
    # Which of those four it actually is decides the fix, and they have nothing in common --
    # a slow BeginDraw means waiting for DComp to hand the surface back, an expensive pbuffer
    # wrap is EGL work on an atlas texture, and a dirty area that always equals the tile area
    # means partial update never engages, so the cost tracks -TileSize rather than content.
    #
    # Needs -DComp surface: with the compositor off there is nothing to bind.
    [switch] $DcompBindProf,
    # Canvas-ack deadlock reproduction. The bug it targets showed up once in 35 runs, so
    # leaving verification to chance does not work: 12 clean runs after the fix logged zero
    # WALLACKFLUSH, meaning the recovery never ran and those runs proved nothing.
    # -CanvasAckSkip N makes each painter skip its first N ack sends exactly the way a closed
    # gate would, which reproduces the deadlock on demand.
    # Plan A step 1: have each tile create its own RenderingContext on its own thread, report,
    # and exit without rendering. surfman's Device is thread-local, so a context cannot be
    # handed between threads -- which is why this is a separate run rather than half of the
    # normal one. If it fails, parallel tile render does not stand as designed
    # (docs/multigpu/parallel_tile_render_design.md section 6.2 step 1).
    [switch] $TileThreadSpike,
    # Give each wall tile its own thread so their renders overlap. Off by default: it changes
    # who owns the rendering context, so the serial path stays available for A/B and for
    # falling back if something regresses. Most of a pass is per-tile AcquireSync waiting,
    # which is per-tile and per-GPU and therefore overlappable -- WALLPASS reports a
    # parallel_ceiling of 3.0-3.8x. Two paths cannot cross a thread yet (screenshots, the
    # memory report) and say so in the log rather than failing quietly.
    [switch] $ParallelTiles,
    [int] $CanvasAckSkip = 0,
    # Collect the JS heap every N seconds instead of waiting for memory pressure.
    # Diagnostic only, and only for one question: when DOM objects that own GPU memory are
    # never finalized, either they are garbage and no collection ran, or they are still
    # reachable and collecting changes nothing. Those need opposite fixes and the log cannot
    # tell them apart. Run with this and watch WEBGLLIVE contexts= -- if it falls, they were
    # garbage. Leave it at 0 for any run whose numbers are meant to mean anything.
    [int] $ForceGcSec = 0,
    # Turns the recovery off. Pair it with -CanvasAckSkip to show the injection really does
    # deadlock the wall -- without this control, a run that stays healthy cannot distinguish
    # "recovery worked" from "the injection never bit".
    [switch] $NoCanvasAckRecovery,
    # gfx_dcomp_always_flush_end_frame=true: restore the unconditional gl().flush() in the
    # DComp end_frame. It is conditional by default -- it only runs when a SwapChain or
    # External surface exists, which is when a submit actually has to become visible
    # somewhere else. That one line measured 7.36ms a frame, 90% of the painter's render,
    # on a run where every surface was Virtual and there was nothing to order against.
    #
    # ***Its failure mode is visual, not a crash.*** Stale tiles or tearing. If the wall
    # looks wrong, set this and the old behaviour comes straight back.
    [switch] $DcompAlwaysFlush,
    # gfx_dcomp_commit_in_end_frame=true: put the DComp Commit back inside end_frame.
    #
    # ***Deferring it to the end of the pass is now the default, and it doubled the frame
    # rate.*** Paired A/B on a single-triangle page: in end_frame gave 9.8fps with a 24.90ms
    # Commit and a 108.5ms pass; deferred gave 19.3fps with a 9.83ms Commit and a 53.8ms pass.
    # Commit waits on GPU completion, and letting the other three tiles render first gives it
    # about 11ms to finish in. There is no added latency -- the commits still land in the same
    # pass, only later in it.
    #
    # Use this to get the old behaviour back if a run looks wrong.
    [switch] $DcompCommitInFrame,
    # gfx_webgl_swap_sync: settle the canvas's GPU work on the WebGL thread right after its
    # swap, instead of leaving it for the painter to trip over.
    #
    # The DComp Commit is only expensive when an animating canvas is on the page, and
    # deferring it showed the wait shrinks when given time -- 24.90ms down to 9.83 for about
    # 11ms of extra slack. A fixed cost does not behave that way, so it is waiting on the
    # canvas. The WebGL thread does roughly 20ms of real work a second, so waiting there is
    # nearly free, while waiting inside Commit costs the whole wall.
    #
    # The three values separate three explanations: flush being enough means the commands
    # were merely unsubmitted, finish being required means completion has to be waited for,
    # and neither helping means the premise is wrong. Cost shows as swap_sync_ms, so pair
    # this with -FanoutProf.
    [ValidateSet("off", "flush", "finish")]
    [string] $WebglSwapSync = "off",
    # gfx_webgl_stage_to_painter_device: copy the canvas texture onto the painter's own
    # device before handing it to WebRender, so the tile draw never samples across devices.
    #
    # What is left after the eliminations. Commit is expensive only with an animating canvas
    # -- 9.86ms a visual against 0.30 for video -- and finishing the canvas GPU work first
    # changed nothing, so the source being unready is not it. Video lives on the painter own
    # ANGLE device and is cheap; the canvas is on an isolated one, because sharing was what
    # produced the access violations, and it is expensive. This moves that crossing out of
    # the tile draw. If Commit falls, the crossing is the cause and this is also a usable
    # fix; if it does not, cross-device is not the cause either.
    #
    # Cost shows as stage_ms in WEBGLEXTIMG, so pair with -FanoutProf.
    [switch] $WebglStageCopy,
    # gfx_dcomp_parallel_commit: issue the four deferred DComp Commits on threads instead of
    # one after another.
    #
    # This is the on/off gap. At steady state with a small canvas a pass is 15.4ms -- 5.6 of
    # render and 9.8 of four serial Commits -- against a 16.67ms budget, so 1.3ms of margin.
    # Lose it once and Commit waits for the next vsync, the pass grows, and the wall settles
    # at 42fps; one run sat there for fifty seconds before climbing to 60. With DComp off the
    # pass is 13.8ms, 2.9ms of margin, and it locks to 60 by the sixth second and stays.
    #
    # Commit is a wait and those waits overlap -- batching them already took one from 24.90ms
    # to 9.83. Four at once should turn 9.8 into about 2.4, which buys 8ms of margin instead
    # of 1.3.
    #
    # ***The thread contract is not confirmed.*** We call the vtable directly so COM
    # marshalling never enters, the devices are per painter, and the scope joins before this
    # returns, so nothing touches one device from two threads at once. But creation happens on
    # the main thread and the Commit would not, and DirectComposition may not like that.
    # A violation can be silent, so check the wall by eye, not only the numbers.
    [switch] $DcompParallelCommit,
    # gfx_dcomp_update_quantum_px: pin the BeginDraw update rect to a fixed grid so DComp is
    # ★낡음(2026-10-20 기록, 같은 날 `b24687d184c` 이후 무효)★ — 기본은 `-1` 이다(:504, :510). "(default)" 만 죽었다.
    # asked for ONE texture size instead of dozens. 0 = off (default), -1 = the whole tile,
    # >0 = round the rect's SIZE up to that many pixels.
    #
    # What this buys, measured 2026-10-20 (log_ani_debug_05\05 and \06): a Commit carrying
    # identical work -- one bind, one newly realized texture -- costs 0.39 ms on a page whose
    # textures are all one size and 3.73-3.81 ms on one with many. Inside a single run the
    # seconds sort the same way: 7.4 / 11.7 / 22.4 distinct texture sizes against 86 / 324 /
    # 681 ms/s of Commit. Allocation VOLUME does the opposite -- the cheapest run allocates the
    # most -- so this is about size variety, not bytes.
    #
    # Only the transparent-expand path is touched (a non-opaque slice whose dirty == valid,
    # i.e. a full repaint), where there is no content to preserve. -1 reproduces what
    # ticker_slice_probe.html?clip=all already demonstrates without removing the second slice,
    # which is the point: it separates "one texture size" from "one WebRender slice".
    #
    # ***-1 IS NOW THE ENGINE DEFAULT*** (T8, 2026-10-20): on the live IPWall page it took the
    # wall pass from 6.35 to 1.03 ms and 8ms+ commits from 2,668 to 9. Pass -DcompUpdateQuantum 0
    # to turn it back OFF -- that is the A/B arm now, and it is why this value is always sent to
    # the engine rather than only when non-zero. >0 is not an operating value: the grid fixes the
    # size for a GIVEN width, and the clipped band's width moves with the text, so 256 still left
    # 21 sizes and judder.
    [int] $DcompUpdateQuantum = -1,
    # gfx_wall_rotate_tile_order: start the tile loop one tile further along each pass.
    #
    # One tile pays for the pass. With the big canvas and DComp off, tile 1 took 25.1ms of a
    # 42.2ms pass in 46 of 51 windows while the other three took 5.7 each, and GPU occupancy
    # stayed in the thirties -- so it is a wait, not throughput. Rotating says whose wait it
    # is: if the per-tile averages even out, the cost belongs to going FIRST and this is
    # pipeline back-pressure; if one tile stays slow, it belongs to that painter.
    #
    # Timings stay recorded per tile, not per position, which is what makes the per-tile
    # averages in WALLPASS the answer.
    [switch] $RotateTileOrder,
    # SERVO_LOG_PRESENT_CADENCE=1: the ground truth for "how fast does this wall actually
    # present, and what does one composite cost". One PRESENT line per second per painter,
    # plus a "Slow paint frame" line for every composite over 16ms with the WebRender
    # update/draw split. This is the metric the 45-video work is aimed at (render_ms p50),
    # so an A/B without it cannot be compared to the earlier rounds.
    [switch] $PresentCadence,
    # SERVO_LOG_ANIM_STEP_SEQ: ANIMSTEP 과 같은 창, 같은 표본을 ***도착 순서 그대로*** 한
    # 줄에 찍는다(ANIMSTEPSEQ). 분위수 요약은 버스트를 묻는다 -- "0.5 초 간격 세 번 뒤
    # 1.5~2 초 안정" 같은 구조가 p05/p50/p95 로는 보이지 않고, 실측에서 그 요약이 저더와
    # ***음의 상관***이었다(설계 문서 45). 창당 한 줄 약 700 자, 벽에서 초당 네 줄이다.
    [switch] $AnimStepSeq,
    # SERVO_FRAME_TIME_PROF=1: FRAMETIME -- 틱에 실려 나간 프레임 시각의 간격을 painter 별로
    # 1 초에 한 줄씩 잰다. ★다른 지표는 전부 프레임을 세지만 이것은 그 프레임이 실은 값을
    # 센다★ -- 이 값이 그대로 페이지의 requestAnimationFrame 타임스탬프이므로, 초당 60 프레임이
    # 나가도 이 수열이 고르지 않으면 페이지가 계산하는 위치가 고르지 않다. 같은 출력 페이지를
    # Chrome 은 single tile 에서 60fps 로 돌리는데 winit_wall 은 떨리고 우리 지표는 전부
    # 깨끗했다(2026-10-21) -- 그때 남아 있던 유일한 변수가 이것이다.
    # -AnimStepSeq 를 함께 주면 정렬 전 원시 수열(FRAMETIMESEQ)도 찍는다.
    [switch] $FrameTimeProf,
    # SERVO_WALL_PAINTER_LOCK_PROF=1: PAINTERLOCK -- `with_painter_mut` 가 타일 스레드의
    # 큐에서 기다린 시간을 painter 별로 1 초에 한 줄씩 잰다(n, wait_ms p50/p95/max,
    # over_1ms). 호출한 순간부터 콜백이 그 스레드에서 ***시작***하는 순간까지이고, 콜백
    # 자신의 실행 시간은 빠진다. 스레드 없는 painter 는 기다릴 것이 없어 줄이 안 난다.
    #
    # ★무엇을 가르는가★ -- 표출 틱이 단일 프레임 생산자가 된 뒤로, 틱은 렌더 앞에서
    # painter 마다 블로킹으로 세 번 닿는다(gather_components / issue_wall_frame_request /
    # release_owed_canvas_acks). N 타일이면 틱당 3N 번의 왕복이 셸 스레드에서 직렬로 돈다.
    # 그 왕복이 진행 중인 타일 렌더 뒤에 서면 ***틱과 렌더 패스 시작 사이가 벌어지고***,
    # -DwmAlign 이 격자에 박아 두는 것은 틱 에지뿐이므로 패스 시작은 그만큼 밀린다.
    # -CommitAlign 의 마감도 틱 기준이라 그 예산을 같이 깎는다. 설계에서 확인되지 않은
    # 유일한 항목이 이것이고, 타일이 하나인 개발 장비에서는 3 번, 벽에서는 12 번이다.
    #
    # 판정: `p95 < 1ms` 면 블로킹 호출이 맞다. `p95 >= 1ms` 면 틱이 타일 렌더 뒤에 서고
    # 있다는 뜻이고 비블로킹 포스트로 가야 한다 -- ★어느 쪽이든 생산자는 하나다.★
    # ★`max` 하나로 읽지 말 것★ -- "3ms 한 번" 과 "3ms 를 60 번" 은 결론이 정반대이고,
    # 그 둘을 가르는 것이 `over_1ms` 다.
    [switch] $PainterLockProf,
    # dom_animation_timeline_use_frame_time: 애니메이션의 기준 시각을 "script 가 도달한
    # 순간" 대신 "그 프레임의 시각" 으로 쓴다. 엔진 기본은 켜짐이므로, 이 스위치는 끌 때
    # 쓴다. 왜 필요한지는 설계 문서 50 과 2026-10-15 설계.
    [ValidateSet("on", "off", "")]
    [string] $FrameTime = "",

    # SERVO_DCOMP_DEBUG: per-surface [dcomp-dbg] lines (create_surface, external add,
    # bind). This is the ONLY way to see whether a video actually reached an external
    # compositor surface -- the launcher wipes SERVO_* before every run, so exporting it
    # in the shell does nothing. Needs paint=info, which the default RUST_LOG has.
    [switch] $DcompDebug,
    # SERVO_VIDEO_ESCAPE_PROF: one [vesc-prof] aggregate line per second from the
    # renderer thread (frames/converts/presents/acquires). Use it with -VideoEscape
    # external to tell "promoted and presenting" from "flag set, nothing promoted".
    [switch] $VideoEscapeProf,
    # Attribute the wall's CPU to its individual threads while it plays. Answers the
    # question D3D11PROF cannot: decode and upload run on ~N parallel streaming
    # threads, so if one of the FEW single-threaded stages (Compositor, Renderer,
    # Script) is pinned at ~1.0 cores, that thread is the ceiling and the GPU is
    # starved behind it. Needs -DurationSec (there is nothing to sample otherwise).
    # Pin the process to a NUMA node (= processor group) at creation, instead of letting
    # Windows pick. -1 = let Windows pick (the old behaviour).
    #
    # ***THIS IS THE ONLY THING THAT HAS EVER SEPARATED A GOOD RUN FROM A BAD ONE.*** Measured
    # 2026-08-26 over 22 runs of 45 videos with escape=external, across four different flag
    # sets (-VideoEscapeProf on/off, -NoSyncGroup, -SinkQos on):
    #
    #   group 1 (18 runs): 27.7-28.8 presents/s, 0.64-0.65 ms per Present, decode 0.68-0.83
    #   group 0 ( 4 runs): 4.2-5.4  presents/s, 2.7-3.3  ms per Present, decode 0.97-0.98
    #
    # No exceptions either way, and NOTHING ELSE correlates -- the sync group armed 45/45 in
    # both, and neither removing the shared base time nor allowing frame drops prevented a
    # collapse. Windows picks the group at process creation, so the same command lands in
    # either state at random. Use this to stop rolling dice, and to prove the direction of
    # causation: if -NumaNode 0 collapses every time and -NumaNode 1 never does, it is the
    # placement, not the run.
    #
    # Launching has to go through `cmd /c start /NODE`, which is the only way to ask for a
    # node at creation. That in turn needs a temp .cmd wrapper so the stderr redirect belongs
    # to winit_wall and not to cmd, and the PID has to be found by name afterwards.
    # "auto" (default) = pin to the NUMA node the display adapters are on, derived at launch.
    # "off" = let Windows pick, the pre-2026-08-26 behaviour. A number forces that node.
    #
    # ★★2026-10-22 정정: 이 박스의 GPU 는 더 이상 한 노드에 있지 않다.★★ 2026-10-19 에
    # 고장으로 ***메인보드를 교체***했고, 교체된 보드는 네 장을 두 노드에 갈라 붙인다
    # (`tools\probe_machine_topology.ps1`, 2026-10-22):
    #
    #   RX 580  PCI bus 134 -> NUMA node 1
    #   RX 580  PCI bus 175 -> NUMA node 1
    #   RX 580  PCI bus  24 -> NUMA node 0
    #   RX 580  PCI bus  59 -> NUMA node 0
    #
    # ★그래서 옳은 `-NumaNode` 값이 존재하지 않는다.★ 어느 노드를 고르든 타일 넷 중 ***둘***이
    # 먼 노드에 남는다. 실측(2026-10-22, wall_anim_jitter_probe, 120 초, COMPREFRESH 의
    # 중복 프레임 비율 -- 최악 패널):
    #
    #   -NumaNode auto (안 고정)  0.15% / 0.39%        <- 제일 낫다
    #   -NumaNode 1               1.00% / 1.29%
    #   -NumaNode 0               0.02% / 2.23%        <- 같은 명령이 양쪽 끝을 냈다
    #
    # 고정한 두 arm 은 결함이 ***런 전 구간에 균일***했고(1 초 창의 91% 에 결함), 안 고정한
    # arm 은 몇 창에만 몰렸다. 즉 기동 시 어디에 앉느냐가 런 전체를 결정한다.
    #
    # ★아래 2026-08-26 기록은 **교체 전 보드**의 것이다.★ 그 보드는 네 장이 전부 node 1
    # 이었고, 그때는 `-NumaNode 1` 이 옳았다. 수치는 그 시절 벽을 해석할 때만 쓸 것:
    #
    #   node 1 (그때 GPU 들의 노드): 23-29.5 video fps, 0.65-0.73 ms per Present, dwm 0.09-0.11
    #   node 0 (먼 소켓)           :  6.0     video fps, 2.9-3.0  ms per Present, dwm 0.64-0.66
    #
    # 먼 노드에서는 업로드와 present 가 전부 소켓 간 링크를 건넌다. 용량 문제가 아니다 --
    # 무너진 런들도 80 코어 중 31 개가 놀고 있었다. 지금 보드에서 이 기전을 없애려면
    # ***프로세스 전체 고정이 아니라 타일별 스레드 배치***가 필요하고, 그건 아직 없다.
    [string] $NumaNode = "auto",
    # Hard-confine the wall to its processor group, on top of the node PREFERENCE that
    # `start /NODE` gives. Off by default until measured on the wall itself.
    #
    # ***A preference is not a confinement, and the difference is worth 40% of the decode CPU.***
    # Decode-only baseline, 54 x FHD30, same topology and pacing, cores per video (2026-08-27,
    # with the affinity mask read back from the OS to prove it took):
    #
    #   1 process, no confinement : 1.001 1.004 1.005 1.007 (0.793 once)
    #   1 process, HARD-confined  : 0.597 0.601 0.604 (0.735 twice)
    #   2 processes, none         : 1.009 1.010 | 0.510 0.517 0.534   <- SPLITS IN TWO
    #   2 processes, HARD-confined: 0.520 0.533 0.537 0.539
    #
    # Two processes land either on the same node (as bad as one process) or on different ones
    # (half the cost) -- the same coin-flip the wall itself showed before -NumaNode, and
    # confining removes it. The wall currently decodes at 0.99-1.37 cores per video, sitting
    # exactly on the worst cluster. At 0.60 the 54-video wall would want 32 cores instead of 53,
    # which fits inside one node s 40.
    [switch] $Confine,
    # media_numa_pin_streaming_threads: nail each video's streaming thread to a NUMA node,
    # round-robin. ***ON BY DEFAULT since 2026-08-27*** -- these switches only exist to turn it
    # off or to say so explicitly.
    #
    # It is the opposite trade to -Confine, and the two must not be combined:
    #
    #   nothing   : threads reach both sockets, memory first-touched on one -> 0.99 cores/video
    #   -Confine  : local memory, but half the physical cores (SMT saturates) -> 0.74
    #   NUMA pin  : local memory AND both sockets                             -> 0.53
    #
    # At 54 videos that is 71% of the machine versus 35%, and pts_rate 0.70 versus 1.00. On a
    # single-node box the engine skips it entirely, so leaving it on costs nothing there.
    [switch] $NumaPin,
    # Turn the pin off (media_numa_pin_streaming_threads=false). Wins over -NumaPin if both
    # are passed -- an explicit "off" should never be silently overridden.
    [switch] $NoNumaPin,
    # Extra `--pref name=value` pairs passed straight through, for knobs this launcher does
    # not wrap. WebGL2 and WebGPU are both OFF by default in this engine, so anything using a
    # canvas 3D context needs one of these:
    #
    #   -Pref dom_webgl2_enabled=true      (WebGL2; the engine has the WebGL fixes compiled in)
    #   -Pref dom_webgpu_enabled=true      (also needs -Serve: WebGPU hangs on file://)
    #
    # `webgpu` is a cargo feature and is part of the standard dist build; make_wall_dist.ps1
    # refuses to package an engine without it, because the pref cannot enable a feature that
    # is not compiled in and the page would just hang.
    [string[]] $Pref = @(),
    [switch] $ThreadCpu,
    # Seconds to let playback settle before sampling. The opening seconds are
    # pipeline setup and first-frame staging, which are not the steady state.
    [int]    $ThreadCpuWarmupSec = 8
)

$ErrorActionPreference = "Stop"
$here   = $PSScriptRoot
$engine = Join-Path $here "engine"
$exe    = Join-Path $engine "winit_wall.exe"
$layout = Join-Path $here "config\$Layout"

if (!(Test-Path $exe))    { throw "winit_wall.exe not found: $exe" }
if (!(Test-Path $layout)) { throw "layout not found: $layout" }
# The escaped-video fast path is a method on the DirectComposition compositor, reached through
# the painter's dcomp_shared handle -- which is None whenever the native compositor is not
# engaged. So -VideoEscape with -DComp off does not fall back to anything, it simply never
# runs, and the run looks like a valid escape measurement while measuring nothing. Refuse it
# rather than enabling DComp behind the caller's back: which of the two they meant is their
# call, and this pairing has already cost time once.
if ($VideoEscape -ne "" -and $DComp -eq "off") {
    throw "-VideoEscape $VideoEscape needs the DirectComposition compositor, but -DComp is off. The escape fast-path lives on that compositor, so it would silently do nothing. Pass -DComp surface to measure the escape path, or drop -VideoEscape to measure the standard set."
}
# Not fatal, unlike the escape pairing: -DcompDebug is a diagnostic and asking for it with the
# compositor off is a wasted run rather than a wrong measurement. It already has one way to
# produce nothing (it needs paint=info in RUST_LOG); this is the second, and now the default.
if ($DcompDebug -and $DComp -eq "off") {
    Write-Warning "-DcompDebug with -DComp off will print nothing: there is no native compositor to trace. Add -DComp surface."
}
if ($DcompParallelCommit -and $DcompCommitInFrame) {
    throw "-DcompParallelCommit needs the Commit deferred to the end of the pass; -DcompCommitInFrame issues it inside end_frame, so there would be nothing to run in parallel."
}
# ***-DComp on can stall the wall.*** All four tiles froze mid-run, first seen alongside
# -DcompParallelCommit (log_webgpu/41) and then again with -DComp on ALONE (log_webgpu/45).
# The first sighting was wrongly attributed to the pairing and the combination was refused for
# it; one co-occurrence was never grounds for that, and the second sighting disproves it.
# Nothing is refused on this account now -- the stall belongs to promotion, not to running the
# Commit on a worker, and blocking the wrong switch only hides it.
if ($DcompParallelCommit -and $DComp -eq "off") {
    throw "-DcompParallelCommit has no Commit to parallelise with -DComp off. Pass -DComp surface."
}
if ($DcompBindProf -and $DComp -eq "off") {
    throw "-DcompBindProf measures the DComp tile bind/unbind round trip, but -DComp is off, so nothing binds and the run would report an empty window. Pass -DComp surface."
}
# Only complain when the operator TYPED it. The default is -1 now, so testing `-ne 0` here
# would throw on every -DComp off run -- and -DComp off is this script's own default.
if ($PSBoundParameters.ContainsKey('DcompUpdateQuantum') -and $DcompUpdateQuantum -ne 0 -and $DComp -eq "off") {
    throw "-DcompUpdateQuantum rounds the BeginDraw update rect, and BeginDraw only happens on DComp virtual surfaces. With -DComp off nothing reads the pref, so the run would look configured and change nothing. Pass -DComp surface."
}
if ($DcompUpdateQuantum -lt -1) {
    throw "-DcompUpdateQuantum takes 0 (off), -1 (whole tile) or a positive pixel grid. $DcompUpdateQuantum is none of those."
}
# Commit alignment schedules the deferred Commit onto the common DWM composition grid. There is
# nothing to schedule if the Commit already went out inside end_frame: -DcompCommitInFrame issues
# it there, so flush_deferred_dcomp_commits finds no pending commit and the alignment branch never
# runs. The pairing produces a run that looks aligned on the command line and is not aligned at
# all -- the same failure shape as the -DcompParallelCommit pairing just above.
# The range here must match the engine gate exactly (gfx_present_align_commit_pct is live for
# 0..=99 and off otherwise). ValidateRange on the parameter already bounds it to -1..99, so the
# only value that reaches here with alignment off is -1 -- but spelling the upper bound out keeps
# this guard honest if that ValidateRange is ever widened, rather than throwing for a combination
# that is not actually live.
# -SampleLead 는 애니메이션을 DWM 합성 격자 위에서 샘플한다. 그런데 렌더 틱이 자유 실행이면
# (=-DwmAlign 없음) 렌더와 격자가 서서히 미끄러져, 가끔 두 렌더가 같은 격자점에 걸린다. 그
# 프레임은 변위가 0 이고 다음 프레임이 두 칸을 뛴다 -- 고치려던 저더보다 나쁘다. 실측으로
# 겪은 실패다(출력별 격자로 같은 일을 했을 때 ANIMSTEP p05 가 35.0 에서 29.3 으로 떨어졌다).
# throw 하지 않는 이유: 둘을 따로 재보는 것 자체가 유효한 실험이다. 다만 조용히 나빠지게
# 두지는 않는다.
if ($SampleLead -ge 0 -and -not ($DwmAlign -ge 0 -and $DwmAlign -le 99)) {
    Write-Warning "-SampleLead $SampleLead without -DwmAlign: the render tick free-runs while the animation samples on the composition grid, so the two will slide against each other and produce zero-displacement frames. Pass -DwmAlign (8 is the measured-good phase) unless you are deliberately measuring them apart."
}

if ($CommitAlign -ge 0 -and $CommitAlign -le 99 -and $DcompCommitInFrame) {
    throw "-CommitAlign $CommitAlign needs the Commit deferred to the end of the pass; -DcompCommitInFrame issues it inside end_frame, so there would be nothing to schedule."
}
# Out of 0..99 the engine silently treats the pref as off. -1 is the documented way to say that, so
# say nothing for it; any other out-of-range value is someone expecting alignment and not getting
# it, and that has to be visible on the console rather than only in a phase log that never appears.
if ($CommitAlign -ne -1 -and ($CommitAlign -lt 0 -or $CommitAlign -gt 99)) {
    Write-Warning "-CommitAlign $CommitAlign is outside 0..99, so the engine treats commit alignment as OFF. Pass a value in 0..99 to enable it, or -1 to say off deliberately."
}

$serveRoot = Join-Path $here "pages\html"
$httpServer = $null
if ($Serve) {
    if (!(Test-Path $serveRoot)) { throw "nothing to serve: $serveRoot" }
    # serve_http.ps1 sits beside this script in a dist, but under tools\ in the worktree.
    $serveScript = @((Join-Path $here "serve_http.ps1"), (Join-Path $here "tools\serve_http.ps1")) |
        Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $serveScript) { throw "serve_http.ps1 not found beside $here (nor in tools\)" }
    # It throws unless the port is actually answering, so nothing below races startup.
    $httpServer = & $serveScript -Root $serveRoot -Port $ServePort -Background
}

if ($Url -eq "") {
    if ($Serve) {
        $Url = "http://127.0.0.1:$ServePort/video_grid_6x6_play.html?rows=$Rows&cols=$Cols"
    } else {
        $page = Join-Path $here "pages\html\video_grid_6x6_play.html"
        if (!(Test-Path $page)) { throw "bundled page not found: $page" }
        # -replace takes a REGEX, so a literal backslash is '\\'. Losing one of those
        # backslashes makes it the invalid pattern '\' and every default (no -Url, no
        # -Serve) run dies before the engine starts.
        $Url = "file:///" + (($page -replace '\\', '/')) + "?rows=$Rows&cols=$Cols"
    }
} elseif ($Serve -and $Url -notmatch '^[a-zA-Z][a-zA-Z0-9+.-]*:') {
    # A bare page name under the server root. Keep any query string the caller attached.
    $file = ($Url -split '\?', 2)[0]
    if (!(Test-Path (Join-Path $serveRoot $file))) { throw "not under ${serveRoot}: $file" }
    $Url = "http://127.0.0.1:$ServePort/$Url"
}
if ($NoSyncGroup)      { $SyncGroup = 0 }
elseif ($SyncGroup -le 0) { $SyncGroup = $Rows * $Cols }
if ($NoLog -and $LogPath -ne "") {
    throw "-NoLog and -LogPath contradict each other; pass one or the other"
}
# "NUL" is the Windows null device, so nothing is written and no file by that name appears
# (verified, not assumed). The analysis at the end of this script guards on Test-Path and
# therefore skips itself.
if ($NoLog)           { $LogPath = "NUL" }
elseif ($LogPath -eq "") { $LogPath = Join-Path $here ("wall_{0}.err.log" -f (Get-Date -Format "yyyyMMdd_HHmmss")) }

# ★여기서 절대 경로로 굳힌다.★ 아래에서 이 값을 쓰는 네 곳이 상대 경로를 ***서로 다르게***
# 푼다:
#   - `cmd ... 2> "$LogPath"`  -> cmd 의 현재 디렉터리
#   - `Start-Process -RedirectStandardError` -> ***.NET 프로세스의 CWD***. PowerShell 의
#     현재 위치가 아니고, 같은 줄의 `-WorkingDirectory` 도 아니다.
#   - `Test-Path` / `Select-String -Path` -> PowerShell 의 현재 위치
# 셋이 어긋나면 로그는 엉뚱한 데 쓰이고 분석은 "파일 없음" 으로 조용히 건너뛴다.
#
# 그리고 ***부모 디렉터리를 만들어 준다.*** `2>` 도 `-RedirectStandardError` 도 없는
# 디렉터리를 만들지 않는다 -- `-LogPath ..\logs\run.err.log` 는 그래서 실패한다.
# dist 폴더는 `make_wall_dist.ps1 -Force` 가 통째로 다시 만들므로, 로그를 보존하려면
# ***dist 바깥***에 두어야 한다. 이 두 줄이 그것을 가능하게 한다.
if ($LogPath -ne "NUL") {
    # `[IO.Path]::GetFullPath` 를 홀로 쓰면 안 된다 -- Windows PowerShell 5.1 에서 그것은
    # PowerShell 의 현재 위치가 아니라 ***프로세스의 CWD*** 를 기준으로 푼다. 사용자가
    # 친 상대 경로는 자기가 서 있는 곳 기준이므로 `$PWD` 와 명시적으로 합친다.
    $LogPath = [IO.Path]::GetFullPath([IO.Path]::Combine($PWD.Path, $LogPath))
    $logDir = Split-Path -Parent $LogPath
    if ($logDir -and -not (Test-Path $logDir)) {
        New-Item -ItemType Directory -Path $logDir -Force | Out-Null
    }
}

# Old env knobs block startup in this build (servo_config::removed_env). Clear any that
# leaked in from an earlier session so the run does not die with a migration notice.
Get-ChildItem Env: | Where-Object { $_.Name -like "SERVO_*" } |
    ForEach-Object { [Environment]::SetEnvironmentVariable($_.Name, $null, "Process") }

# Set AFTER the SERVO_* wipe above, or it gets cleared.
if ($D3d11Profile)        { $env:SERVO_D3D11_PROFILE = "1" }
if ($PSBoundParameters.ContainsKey('D3d11ProfileMs')) {
    if ($D3d11ProfileMs -lt 0) { throw "-D3d11ProfileMs must be 0 or greater (0 = log every frame)" }
    $env:SERVO_D3D11_PROFILE_MS = "$D3d11ProfileMs"
}
if ($VideoRate)            { $env:SERVO_MEDIA_VIDEO_RATE = "1" }
if ($FrameReason)          { $env:SERVO_FRAME_REASON_PROF = "1" }
if ($WrSlowMs -gt 0)       { $env:SERVO_WR_SLOW_MS = "$WrSlowMs" }
if ($SinkProf)             { $env:SERVO_MEDIA_SINK_PROF = "1" }
if ($FanoutProf)           { $env:SERVO_WEBGL_FANOUT_PROF = "1" }
if ($DcompBindProf)        { $env:SERVO_DCOMP_BIND_PROF = "1" }
if ($CanvasAckSkip -gt 0)  { $env:SERVO_WALL_CANVAS_ACK_SKIP = "$CanvasAckSkip" }
if ($ForceGcSec -gt 0)     { $env:SERVO_SCRIPT_FORCE_GC_SEC = "$ForceGcSec" }
else                       { Remove-Item Env:\SERVO_SCRIPT_FORCE_GC_SEC -ErrorAction SilentlyContinue }
if ($NoCanvasAckRecovery)  { $env:SERVO_WALL_DISABLE_CANVAS_ACK_RECOVERY = "1" }
if ($PresentCadence)       { $env:SERVO_LOG_PRESENT_CADENCE = "1" }
if ($AnimStepSeq)          { $env:SERVO_LOG_ANIM_STEP_SEQ = "1" }
if ($FrameTimeProf)        { $env:SERVO_FRAME_TIME_PROF = "1" }
if ($PainterLockProf)      { $env:SERVO_WALL_PAINTER_LOCK_PROF = "1" }
if ($DcompDebug)           { $env:SERVO_DCOMP_DEBUG = "1" }
if ($VideoEscapeProf)      { $env:SERVO_VIDEO_ESCAPE_PROF = "1" }

$env:GST_PLUGIN_PATH            = ""
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = ""
$env:PATH = "$engine;$env:PATH"
if ($QuietLog -and $KeepRustLog) {
    throw "-QuietLog and -KeepRustLog contradict each other; pass one or the other"
}
if ($QuietLog) {
    # paint 의 warn! 과 script 의 rAF 클럭만 남긴다. 2026-10-27 에 10/d_01 과 10/e_01 의
    # 태그 집합을 대조해 확인한 결과, `error,paint=warn` 만으로는 ***RAFCLOCK 하나를 잃는다***
    # -- 그것만 `script::raf_clock` 의 warn! 이기 때문이다. 나머지 열둘(COMPREFRESH /
    # OUTPHASE / VBANCHOR / TICKCOMMIT / COMMITCYCLE / SAMPLESLIP / SAMPLELEAD / FRAMEID /
    # DCOMPSTAT = paint::output_grid, DWMPHASE = paint::dcomp_compositor,
    # FRAMETIME = paint::frame_time, ANIMSTEP = paint::painter)는 줄 수까지 같게 남았다.
    # RAFCLOCK 은 "페이지가 같은 위치를 두 번 계산했다" 를 세는 유일한 줄이라 빠지면 안 된다.
    # 비용은 2 줄/s 다.
    # `winit_wall` 은 ***info*** 여야 한다. 셸의 패스 분해(WALLSPLIT/WALLQUEUE/MAINBUSY/
    # WALLPASS/WALLCLOCK)가 전부 INFO 라 warn 으로는 안 돌아온다 -- 10/e_01 에서 프리픽스를
    # 직접 읽어 확인했다. 분량은 408 줄/60s = 6.8 줄/s, 1.4 KB/s 라 사실상 공짜다.
    $env:RUST_LOG = "error,paint=warn,script::raf_clock=warn,winit_wall=info"
} elseif (-not $KeepRustLog) {
    # ***`winit_wall=info` is not optional decoration.*** The shell's own diagnostics
    # (WALLPASS render-pass timings, devtools port) log from the example crate, and without
    # a target for it they fall under the leading `warn` and never appear -- a switch can
    # then look like it did nothing when it worked fine. Learned twice: the same filter
    # hid the image-downscale line from `net` until it was moved to warn.
    $env:RUST_LOG = "warn,paint=info,media=info,winit_wall=info," +
                    "servo_media_gstreamer=info,servo_media_gstreamer_render_d3d11=info"
}
# ***RUST_LOG 의 알 수 없는 맨 토큰은 로그를 끄지 않는다 -- 상한을 Trace 로 올린다.***
# env_filter 는 레벨로 파싱되지 않는 맨 토큰을 `(LevelFilter::Trace, <모듈이름>)` 디렉티브로
# 바꾸고(parser.rs), Filter::filter() 가 디렉티브 레벨의 ***최대값***을 돌려주며(filter.rs),
# components/servo/servo.rs 의 setup_logging 이 그 값을 그대로 log::set_max_level 에 넣는다.
# 그래서 `RUST_LOG=critical` 은 "critical 이상만" 이 아니라 "전부 통과" 가 된다. 출력은
# (critical 로 시작하는 타깃이 없으니) 0 줄인데, 핫 루프의 trace!/debug! 는 매번 Record 를
# 만들고 로거 둘로 디스패치된다 -- 조용해지는 게 아니라 일이 늘어난다.
#
# 2026-10-27 에 이것으로 A/B 한 바퀴를 날렸다: `$env:RUST_LOG="critical"` + -KeepRustLog 를
# "로그 최소화" 로 읽었는데 실제로는 max_level 을 Info 에서 Trace 로 올린 구성이었다.
# 배너에 RUST_LOG=critical 이 그대로 찍혀서 아무도 눈치채지 못했다. 조용히 넘기지 말 것.
$rustLogLevels = @("off", "error", "warn", "info", "debug", "trace")
foreach ($directive in ($env:RUST_LOG -split "," | Where-Object { $_.Trim() -ne "" })) {
    $hasTarget = $directive -match "="
    $level = if ($hasTarget) { ($directive -split "=", 2)[1] } else { $directive }
    if ($rustLogLevels -notcontains $level.Trim().ToLower()) {
        if ($hasTarget) {
            Write-Warning "RUST_LOG directive '$($directive.Trim())' has an unknown level '$($level.Trim())'. Valid: $($rustLogLevels -join ', ')."
        } else {
            Write-Warning "RUST_LOG token '$($directive.Trim())' is NOT a log level, so env_filter reads it as a MODULE NAME at Trace -- it RAISES the global max level to Trace instead of quieting anything, and the hot-loop trace!/debug! calls start building Records every message. Valid levels: $($rustLogLevels -join ', '). To cut the log volume while keeping the judder counters, use -QuietLog."
        }
    }
}

if ($env:RUST_LOG -notmatch "(^|,)media=") {
    Write-Warning "RUST_LOG has no 'media=' target -- the WALLDIAG consumer-device/wrap lines will be MISSING."
}

$tiles = $Rows * $Cols
$argList = @(
    "--wall-layout", $layout, "--wall-all-tiles",
    "--pref", "gfx_dcomp_mode=$DComp",
    "--pref", "gfx_vsync_enabled=$($Vsync.IsPresent.ToString().ToLower())",
    "--pref", "gfx_vsync_phase_pct=$VsyncPhase",
    "--pref", "gfx_present_sync_interval=$PresentSync",
    "--pref", "gfx_present_align_dwm_pct=$DwmAlign",
    "--pref", "gfx_present_align_commit_pct=$CommitAlign",
    "--pref", "gfx_sample_lead_periods=$SampleLead",
    "--pref", "gfx_wall_direct_render=$($DirectRender.IsPresent.ToString().ToLower())",
    "--pref", "gfx_wall_pacing_thread=$($PacingThread.IsPresent.ToString().ToLower())",
    "--pref", "gfx_wall_pacing_vblank=$((-not $PacingDwmQuery.IsPresent).ToString().ToLower())",
    "--pref", "gfx_wall_pacing_output=$PacingOutput",
    "--pref", "gfx_refresh_hz=$RefreshHz",
    "--pref", "gfx_wr_picture_tile_size=$TileSize",
    "--pref", "media_d3d11_enabled=true",
    "--pref", "media_direct_file_enabled=true",
    "--pref", "media_gapless_loop_enabled=true",
    "--pref", "media_avdec_max_threads=$DecoderThreads",
    "--pref", "media_sync_group_target=$SyncGroup"
)
# Accept any TLS certificate. Needed to point -Url at an https page served with a
# self-signed or internal-CA certificate, which is how the output pages are served here.
# It is an engine FLAG, not a pref, so it cannot go through -Pref -- and winit_wall's
# argument parser rejects unknown `--flags` outright, so there was no way in at all.
# The engine logs "accepting ALL TLS certificate errors" when this is on; keep it off for
# anything but a page you control.
if ($IgnoreCertErrors)    { $argList += "--ignore-certificate-errors" }
# ***Must live here, with the other prefs, not up in the env-var section.*** `$argList` is
# created below that section with `= @(...)`, so an append made earlier is thrown away --
# which is exactly what happened on the first attempt, silently: the run started fine and
# the header even said ON, but the engine never saw the pref.
if ($NoImmediateComposite) { $argList += @("--pref", "gfx_video_immediate_composite_enabled=false") }
if ($PageFeatures) {
    foreach ($f in @("dom_video_network_uri_enabled", "dom_video_extended_containers_enabled",
                     "dom_image_extended_formats_enabled", "dom_webrtc_enabled",
                     "dom_screen_capture_enabled", "dom_webgpu_enabled")) {
        $argList += @("--pref", "$f=true")
    }
}
if ($Devtools -ne "") {
    # Site deployments keep devtools off (device auth decision, docs/multigpu/device_auth.md).
    # When on, the console can set window.sdwallDeviceAuth by hand and unlock /output.
    Write-Warning "Devtools ON ($Devtools): development only -- never on a site deployment (device auth can be forged from the console)."
    $argList += @("--pref", "devtools_server_enabled=true")
    $argList += @("--pref", "devtools_server_listen_address=$Devtools")
}
if ($VideoEscape -ne "")  { $argList += @("--pref", "gfx_video_escape_mode=$VideoEscape") }
if ($DcompAlwaysFlush)    { $argList += @("--pref", "gfx_dcomp_always_flush_end_frame=true") }
if ($FrameTime -ne "")    { $argList += @("--pref", "dom_animation_timeline_use_frame_time=$(if ($FrameTime -eq 'on') { 'true' } else { 'false' })") }
if ($DcompCommitInFrame)  { $argList += @("--pref", "gfx_dcomp_commit_in_end_frame=true") }
if ($WebglSwapSync -ne "off") { $argList += @("--pref", "gfx_webgl_swap_sync=$WebglSwapSync") }
if ($WebglStageCopy)      { $argList += @("--pref", "gfx_webgl_stage_to_painter_device=true") }
if ($DcompParallelCommit) { $argList += @("--pref", "gfx_dcomp_parallel_commit=true") }
# Always sent, never conditional. The engine default is -1 now, so "only send it when non-zero"
# would make `-DcompUpdateQuantum 0` a no-op -- the off arm of the A/B would silently run ON.
$argList += @("--pref", "gfx_dcomp_update_quantum_px=$DcompUpdateQuantum")
if ($RotateTileOrder)     { $argList += @("--pref", "gfx_wall_rotate_tile_order=true") }
if ($VideoEscapeBuffers -gt 0) { $argList += @("--pref", "gfx_video_escape_buffer_count=$VideoEscapeBuffers") }
if ($SinkQos -ne "")      { $argList += @("--pref", "media_video_sink_qos=$SinkQos") }
if ($SinkPolicy -ne "")   { $argList += @("--pref", "media_video_sink_policy=$SinkPolicy") }
if ($SinkPacing -ne "")   { $argList += @("--pref", "media_video_sink_pacing=$SinkPacing") }
if ($NoAudio)             { $argList += @("--pref", "media_audio_enabled=false") }
if ($NoNumaPin)           { $argList += @("--pref", "media_numa_pin_streaming_threads=false") }
elseif ($NumaPin)         { $argList += @("--pref", "media_numa_pin_streaming_threads=true") }
if ($PipelineMode -ne "") { $argList += @("--pref", "media_pipeline_mode=$PipelineMode") }
if ($MaxPending -gt 0)    { $argList += @("--pref", "gfx_wall_frame_max_pending=$MaxPending") }
if ($ParallelTiles)       { $argList += @("--pref", "gfx_wall_parallel_tiles=true") }
if ($MinIntervalMs -gt 0) { $argList += @("--pref", "gfx_wall_frame_min_interval_ms=$MinIntervalMs") }
# Passthrough is appended AFTER every pref this launcher sets itself, so `-Pref x=y` wins
# over the launcher's own value for x. Servo takes the last --pref for a given name.
foreach ($p in $Pref)     { $argList += @("--pref", $p) }
# The spike creates its tile contexts on worker threads instead of the main thread, reports,
# and exits -- it never renders, so nothing else in this script applies to that run.
if ($TileThreadSpike)     { $argList += "--tile-thread-spike" }
$argList += $Url

Write-Host "Wall (pref-era) -- $tiles tiles requested by the page grid"
Write-Host "  layout=$layout"
# ★어느 배포본이 돌았는지 로그에 남긴다.★ 이게 없어서 2026-09-17 에 오래된 exe 와
# 오래된 스크립트로 각각 네 번씩 돌린 것을 나중에야 알았다. console.txt 만 보면
# 확정되도록 설정 요약 바로 위에 찍는다.
$buildStamp = Join-Path $PSScriptRoot "BUILD.txt"
if (Test-Path $buildStamp) {
    Write-Host "  build: $((Get-Content $buildStamp) -join ' | ')"
} else {
    Write-Warning "BUILD.txt missing -- this dist was not produced by make_wall_dist.ps1, or the copy was incomplete. Cannot tell which build is running."
}
Write-Host "  dcomp=$DComp dcomp_flush=$(if($DcompAlwaysFlush){'always'}else{'conditional (default)'}) dcomp_commit=$(if($DcompCommitInFrame){'in end_frame'}else{'deferred to end of pass (default)'}) webgl_swap_sync=$WebglSwapSync webgl_stage_copy=$($WebglStageCopy.IsPresent) dcomp_parallel_commit=$($DcompParallelCommit.IsPresent) update_quantum=$(if($DcompUpdateQuantum -eq 0){'off'}elseif($DcompUpdateQuantum -eq -1){'whole tile'}else{"${DcompUpdateQuantum}px"}) rotate_tiles=$($RotateTileOrder.IsPresent) tile_size=$TileSize refresh=${RefreshHz}Hz vsync=$($Vsync.IsPresent) vsync_phase=$VsyncPhase present_sync=$PresentSync dwm_align=$DwmAlign commit_align=$CommitAlign sample_lead=$SampleLead direct_render=$($DirectRender.IsPresent) pacing_thread=$($PacingThread.IsPresent) pacing_clock=$(if(-not $PacingThread.IsPresent){'n/a'}elseif($PacingDwmQuery){'dwm-query+sleep'}else{'vblank'}) pacing_output=$(if($PacingOutput -eq ''){'primary (default)'}else{$PacingOutput}) escape=$(if($VideoEscape -eq ''){'off'}else{$VideoEscape}) escape_buffers=$(if($VideoEscapeBuffers -eq 0){'default(2)'}else{$VideoEscapeBuffers})"
Write-Host "  sync_group=$(if($SyncGroup -le 0){'off'}else{$SyncGroup}) decoder_threads=$DecoderThreads sink_qos=$(if($SinkQos -eq ''){'policy'}else{$SinkQos}) sink_policy=$(if($SinkPolicy -eq ''){'default'}else{$SinkPolicy}) sink_pacing=$(if($SinkPacing -eq ''){'clock'}else{$SinkPacing}) numa_pin=$(if($NoNumaPin){'off'}else{'on(default)'}) audio=$(if($NoAudio){'off'}else{'on'}) pipeline=$(if($PipelineMode -eq ''){'playbin3'}else{$PipelineMode})"
Write-Host "  d3d11_profile=$($D3d11Profile.IsPresent) video_rate=$($VideoRate.IsPresent) immediate_composite=$(if($NoImmediateComposite){'OFF ENTIRELY (A/B arm)'}else{'coalesced (default)'})$(if($PSBoundParameters.ContainsKey('D3d11ProfileMs')){" threshold=${D3d11ProfileMs}ms"}else{" threshold=8ms(default)"})"
# Record it in the transcript. A run that trusted every certificate should say so in
# its own log, not only in the command someone typed.
if ($IgnoreCertErrors) { Write-Host "  ignore_certificate_errors=ON (all TLS errors accepted)" }
Write-Host "  page_features=$(if($PageFeatures){'ON (rtsp/containers/images/webrtc/screen-capture/webgpu)'}else{'off -- rtsp:// video will NOT play'}) devtools=$(if($Devtools -eq ''){'off'}else{$Devtools})"
Write-Host "  RUST_LOG=$env:RUST_LOG"
# A forced-collection run measures a hypothesis, not the product. Say so in the transcript
# so nobody reads its frame rates or its GPU memory as this build's normal behaviour.
if ($ForceGcSec -gt 0) { Write-Host "  force_gc=every ${ForceGcSec}s (DIAGNOSTIC -- perf numbers from this run are not comparable)" }
Write-Host "  log=$(if($NoLog){'off (-NoLog); no post-run analysis either'}else{$LogPath})"

# ***The two flags that decide whether a video grid is CPU-bound.*** Both default OFF, both
# have measured cores/video attached to them above, and together they are the difference
# between 37fps and 62fps at 45 x FHD30 (log_webgpu/73, same build, same layout, same pass):
# playbin3's vqueue carries every raw 3.1MB frame across a thread boundary (0.729 -> 0.284
# cores/video), and GstSystemClock::obtain() is a per-process singleton that all 45 sinks
# wait on every frame (0.795 -> 0.284). Neither shows up as a wall symptom -- the pass looks
# fine and the tiles just run slow -- so a whole afternoon of tile-side A/B ran without them
# (log_webgpu/71-72) and every number from it was measured on a starved configuration.
# Say so before the run, not after.
if ($tiles -ge 16 -and ($PipelineMode -eq '' -or $SinkPacing -eq '')) {
    $missing = @()
    if ($PipelineMode -eq '') { $missing += "-PipelineMode uridecodebin3" }
    if ($SinkPacing  -eq '') { $missing += "-SinkPacing thread" }
    Write-Warning ("$tiles video tiles without $($missing -join ' and '). Those are the CPU flags; " +
        "without them this run is decode-bound and its wall numbers say nothing about the wall. " +
        "Add them unless you are deliberately measuring the default pipeline. " +
        "(-SinkPacing thread gives each pipeline its own anchor, so videos are NOT synchronised with each other.)")
}

# Resolve -NumaNode "auto" into a number by asking the display adapters which node they are
# on. DXGI does not expose this; it is a PnP device property. If they disagree, or none of them
# answers, say so and pin nothing -- guessing here is how a wall ends up on the far socket.
$numaResolved = -1
if ($NumaNode -eq "auto") {
    $nodes = @(Get-PnpDevice -Class Display -Status OK -ErrorAction SilentlyContinue |
        ForEach-Object {
            ($_ | Get-PnpDeviceProperty -KeyName 'DEVPKEY_Device_Numa_Node' -EA SilentlyContinue).Data
        } | Where-Object { $null -ne $_ } | Sort-Object -Unique)
    if ($nodes.Count -eq 1) {
        $numaResolved = [int]$nodes[0]
        Write-Host "  numa=auto -> node $numaResolved (from the display adapters)"
    } elseif ($nodes.Count -gt 1) {
        Write-Warning "Display adapters are split across NUMA nodes ($($nodes -join ', ')). Pinning nothing, which is CORRECT here: with the GPUs on different nodes there is no right -NumaNode value -- whichever you pick, the tiles on the other node pay the inter-socket link. Measured 2026-10-22: unpinned 0.15-0.39% duplicated frames, -NumaNode 1 1.00-1.29%, -NumaNode 0 0.02-2.23%. Do not pass -NumaNode <n> to silence this."
    } else {
        Write-Warning "No display adapter reports a NUMA node, so it cannot be derived. Pinning nothing. On a multi-socket box pass -NumaNode <n> explicitly: landing on the far node has been measured to cut 45-video playback from 29 fps to 6."
    }
} elseif ($NumaNode -eq "off" -or $NumaNode -eq "") {
    $numaResolved = -1
} elseif ($NumaNode -match '^\d+$') {
    $numaResolved = [int]$NumaNode
} else {
    throw "-NumaNode must be 'auto', 'off', or a node number (got '$NumaNode')"
}

# ★런 하나의 구성을 한 블록으로 못박는다 -- 콘솔에 지금, 로그 파일에 런이 끝난 뒤.★
# 2026-10-27 에 run 08/10 의 arm 비교가 통째로 무효가 된 원인이 이 블록의 부재다: e 암은
# -LegacyLogRedirect(파이프, 쓰기당 127 us) 로 돌고 d 암은 cmd 상속 핸들(7.1 us) 로 돌았는데
# 배너 어디에도 그 차이가 안 찍혀서 "로그 분량" A/B 로 읽혔다. 전송로와 RUST_LOG 가 ***동시에***
# 바뀐 비교였고, 그 수치가 다섯 세션을 돌아다녔다. 한 줄이면 막혔다.
#
# ★전송로 판정은 아래 launch 분기와 같은 조건식이어야 한다.★ 둘이 어긋나면 이 블록은
# 거짓말이 되고, 그건 블록이 없는 것보다 나쁘다 -- 틀린 라벨이 붙은 수치는 의심조차 안 받는다.
#
# ★-NoLog 는 "로깅을 끈다" 가 아니다.★ RUST_LOG 은 그대로라 엔진은 레코드 1,850 개/s 를 여전히
# 만들고 `format!` 까지 한 다음 두 로거에 돌린다 -- 공짜가 된 것은 write 하나뿐이다. D 암을
# "로그 없음" 기준선으로 읽으면 포맷 CPU 를 기준선에 포함시킨 채 비교하게 된다. 값에 적어 둔다.
$stderrTransport = "redirect-pipe"
if ($NoLog) {
    $stderrTransport = "nul (RUST_LOG unchanged: records are still built and formatted; only the write is free)"
} elseif ($numaResolved -ge 0 -or -not $LegacyLogRedirect) {
    $stderrTransport = "inherited-handle"
}
# md5 는 BUILD.txt 가 못 가르는 것을 가른다 -- dirty 빌드는 commit 이 같아도 코드가 다르다.
# 139 MB 엔진에서 실측 383 ms 다. 60 초 런 옆에서는 공짜고, 해시가 없으면 "어느 빌드였나" 에
# 답할 방법이 bytes + mtime 뿐이다(그 둘은 서로 다른 빌드에서 같은 값이 나올 수 있다).
$engineMd5 = "unavailable"
try { $engineMd5 = (Get-FileHash -Path $exe -Algorithm MD5).Hash.Substring(0, 12) } catch { }
$runConfig = @(
    ("stderr       : {0}" -f $stderrTransport),
    ("rust_log     : {0}" -f $env:RUST_LOG),
    ("duration_sec : {0}" -f $(if ($DurationSec -gt 0) { $DurationSec } else { "unbounded (no -DurationSec)" })),
    ("engine_md5   : {0}" -f $engineMd5),
    ("args         : {0}" -f ($argList -join ' '))
)
Write-Host ""
Write-Host "RUN CONFIG"
$runConfig | ForEach-Object { Write-Host "  $_" }
Write-Host ""

# ***래퍼가 기본이다 -- NUMA 핀이 없어도.*** 핀 때문이 아니라 ***stderr 때문***이다:
# cmd 의 `2>` 는 자식에게 파일 핸들을 그대로 물려주는데, Start-Process -RedirectStandardError
# 는 파이프를 끼우고 PowerShell 이 퍼낸다. 쓰기당 7.1 us 대 127.4 us, 최악 0.9 ms 대 25.4 ms.
# 자세한 수치와 그것이 무엇을 망쳤는지는 위 -LegacyLogRedirect 주석에 있다.
if ($numaResolved -ge 0 -or -not $LegacyLogRedirect) {
    # Quote EVERY argument: the page URL carries `?rows=5&cols=9`, and a bare `&` inside a
    # .cmd is a command separator, so an unquoted URL silently truncates the run.
    $argStr = ($argList | ForEach-Object { '"' + $_ + '"' }) -join ' '
    $cmdFile = Join-Path $env:TEMP ("wall_numa_{0}_{1}.cmd" -f $PID, $numaResolved)
    # 핀이 없으면 /NODE 를 아예 빼고, 있으면 그 자리에 둔다. 공백 하나까지 중요하다 --
    # `start "" /B` 와 `start "" /NODE 1 /B` 가 같은 줄에서 나와야 한다.
    $nodeArg = if ($numaResolved -ge 0) { "/NODE $numaResolved " } else { "" }
    # ***`start /NODE` has to be the thing that launches winit_wall itself.*** The first cut of
    # this put /NODE on the wrapper cmd.exe and let THAT spawn the exe -- a child inherits no
    # node preference, so the flag did nothing and the run landed wherever Windows felt like.
    # Measured 2026-08-26: three runs asked for node 0 and one of them came up in group 1;
    # three asked for node 1 and one came up in group 0. The requested node and the group the
    # threads actually ran in were uncorrelated, so that round tested nothing.
    #
    # /B keeps it in this console so the `2>` redirect below still reaches winit_wall's stderr;
    # /WAIT keeps the wrapper alive until the wall exits, so nothing is orphaned.
    @"
@echo off
cd /d "$here"
start "" $nodeArg/B /WAIT "$exe" $argStr 2> "$LogPath"
"@ | Set-Content -Encoding ascii $cmdFile
    if ($numaResolved -ge 0) {
        Write-Host "  launching on NUMA node $numaResolved (via cmd start /NODE)"
    } else {
        Write-Host "  launching via cmd (stderr = inherited handle; 18x cheaper per write than -LegacyLogRedirect)"
    }
    # Just run the wrapper. ***The /NODE lives INSIDE it***, on the line that launches
    # winit_wall itself -- an earlier cut put /NODE on this outer cmd and let it spawn the
    # exe as a child, which inherits no node preference, so the flag did nothing at all
    # (measured: 2 of 6 runs came up in the group they had not asked for).
    # `cmd /c` (not the .cmd path directly) so the wrapper exits instead of lingering as a
    # `cmd /K` zombie holding the dist folder open.
    Start-Process cmd -ArgumentList "/c", "`"$cmdFile`"" -WindowStyle Hidden | Out-Null
    # start /B returns immediately, so the PID has to be found by name. Only one wall runs
    # at a time here, so the name is unambiguous.
    $proc = $null
    for ($i = 0; $i -lt 100 -and $null -eq $proc; $i++) {
        Start-Sleep -Milliseconds 100
        $proc = Get-Process winit_wall -ErrorAction SilentlyContinue | Select-Object -First 1
    }
    if ($null -eq $proc) { throw "winit_wall did not start within 10s under the cmd wrapper ($cmdFile)" }
} else {
    $proc = Start-Process -FilePath $exe -ArgumentList $argList -WorkingDirectory $here `
        -RedirectStandardError $LogPath -PassThru
}

# ***WHICH PROCESSOR GROUP THIS PROCESS LANDED IN DECIDES THE RESULT.*** Measured 2026-08-26,
# 45 videos with escape=external, same command and same binary, nine runs:
#
#   group 1 (7 runs): 34.2-37.1 cores, 0.75-0.81 per decode thread, ~28 presents/s -- fine
#   group 0 (2 runs): 46.4-46.5 cores, 0.98 SATURATED,               ~5 presents/s -- collapsed
#
# The box has 2 processor groups (40+40) and 2 NUMA nodes, and the GPU hangs off one of them.
# Windows picks the group at process creation, so the SAME command lands in either state and
# an A/B that straddles the two compares nothing. Print it early, before anyone reads a number
# off this run. The 1s probe costs nothing next to a 40s run.
# Hard confinement, applied after launch and then read back. ***Never report a run as pinned
# without asking the OS whether it actually is.*** The measurement tool spent three rounds on a
# `start /AFFINITY` that was refused in silence, producing unpinned numbers that looked like
# data; the read-back is what makes this trustworthy.
if ($Confine) {
    Add-Type -Namespace Win32 -Name TopoW -MemberDefinition @"
[DllImport("kernel32.dll")] public static extern uint GetActiveProcessorCount(ushort g);
"@ -ErrorAction SilentlyContinue
    $bits = [Win32.TopoW]::GetActiveProcessorCount([System.UInt16]0)
    $want = [uint64]::MaxValue -shr (64 - [int]$bits)
    try {
        $proc.ProcessorAffinity = [IntPtr]([int64]$want)
        $proc.Refresh()
        $got = [uint64][int64]$proc.ProcessorAffinity
        if ($got -eq $want) {
            Write-Host ("  confined    : 0x{0:x} ({1} processors), read back from the OS" -f $got, $bits)
        } else {
            Write-Warning ("-Confine did not take: asked 0x{0:x}, OS reports 0x{1:x}. This run is NOT confined." -f $want, $got)
        }
    } catch {
        Write-Warning "-Confine failed on pid $($proc.Id): $($_.Exception.Message). This run is NOT confined."
    }
}

$grpLines = @()
$probeEarly = Join-Path $engine "thread_cpu_probe.exe"
if (Test-Path $probeEarly) {
    Start-Sleep -Seconds 3        # let the decode threads exist before asking where they are
    $out = & $probeEarly --pid $proc.Id --duration 1 --top 1 2>&1
    # The probe prints this section only on a box with more than one group, so a machine
    # with a single group (the dev box) yields nothing here -- that is not an error.
    # ***Keep EVERY group line, not the first one.*** Taking `-First 1` meant always taking
    # group 0, because the probe lists groups in order -- so the "landed in GROUP 0" warning
    # below fired on every multi-group machine no matter where the work actually was. Two
    # runs on 2026-08-31 both warned while group 0 held 0.00 and 0.03 cores and group 1 held
    # 34.5 and 36.3. A warning that says "discard this run" and is always wrong is worse
    # than no warning: it either burns re-runs or teaches you to ignore the real case.
    $grpLines = @($out | Select-String -Pattern "^\s+group \d" | ForEach-Object { $_.ToString().Trim() })
}
if ($grpLines.Count) {
    $groups = @($grpLines | ForEach-Object {
        if ($_ -match "^group (\d+)\s*:\s*(\d+) threads\s+([0-9.]+) cores") {
            [pscustomobject]@{ Id = [int]$Matches[1]; Threads = [int]$Matches[2]; Cores = [double]$Matches[3]; Line = $_ }
        }
    } | Where-Object { $null -ne $_ })

    foreach ($g in $groups) { Write-Host ("  processor {0}" -f $g.Line) }

    if ($groups.Count) {
        # This sample is taken 3s in, so cores can still be ~0 everywhere. Fall back to
        # thread placement in that case -- it is the thing the NUMA pin actually controls.
        $busiest = if (($groups | Measure-Object -Property Cores -Sum).Sum -gt 0.5) {
            $groups | Sort-Object Cores -Descending | Select-Object -First 1
        } else {
            $groups | Sort-Object Threads -Descending | Select-Object -First 1
        }
        if ($busiest.Id -eq 0) {
            Write-Warning "This run landed in processor GROUP 0. Measured 2026-08-26: at 45 videos with escape=external, group 0 collapses (0.98 cores/decode thread, ~5 presents/s) while group 1 runs at 0.77 and ~28. DO NOT compare this run against a group 1 run -- re-run until it lands in group 1, or treat this as the group 0 arm on purpose."
        }
    }
}

if ($DurationSec -gt 0) {
    $sampled = $false
    if ($ThreadCpu) {
        $probe  = Join-Path $engine "thread_cpu_probe.exe"
        $window = $DurationSec - $ThreadCpuWarmupSec - 2
        if (!(Test-Path $probe)) {
            Write-Warning "thread_cpu_probe.exe is not in engine\ -- repackage with make_wall_dist.ps1. Skipping the thread breakdown."
        } elseif ($window -lt 5) {
            Write-Warning "-DurationSec $DurationSec leaves only ${window}s after a ${ThreadCpuWarmupSec}s warmup. Use at least -DurationSec $($ThreadCpuWarmupSec + 12). Skipping the thread breakdown."
        } else {
            Start-Sleep -Seconds $ThreadCpuWarmupSec
            # ***Not ChangeExtension when -NoLog***: that turns "NUL" into
            # "NUL.threads.txt", which is an ordinary file, and the switch would quietly
            # leave one behind. The breakdown still prints to the console either way.
            $threadLog = if ($NoLog) { "NUL" } else { [IO.Path]::ChangeExtension($LogPath, ".threads.txt") }
            Write-Host ""
            Write-Host "--- thread CPU breakdown (${window}s window, after ${ThreadCpuWarmupSec}s warmup) ---"
            # NOT Tee-Object: on Windows PowerShell 5.1 it writes UTF-16, and the
            # saved file then reads as spaced-out garbage in grep/less. The probe
            # emits pure ASCII, so say so.
            $probeOut = & $probe --pid $proc.Id --duration $window --top 20 2>&1
            $probeOut | ForEach-Object { Write-Host $_ }
            $probeOut | Out-File -FilePath $threadLog -Encoding ascii
            if (-not $NoLog) { Write-Host "  saved: $threadLog" }
            Start-Sleep -Seconds 2
            $sampled = $true
        }
    }
    if (-not $sampled) { Start-Sleep -Seconds $DurationSec }
    # Wall tile windows ignore WM_CLOSE. env_logger writes stderr unbuffered, so a
    # force-kill does not lose log output.
    if (!$proc.HasExited) { Stop-Process -Id $proc.Id -Force }
    Start-Sleep -Seconds 2
} else {
    if ($ThreadCpu) {
        Write-Warning "-ThreadCpu needs -DurationSec to know when to sample. Run the probe by hand instead: engine\thread_cpu_probe.exe --duration 20"
    }
    Write-Host "Running. To stop: Ctrl+C then  Stop-Process -Name winit_wall -Force"
    Wait-Process -Id $proc.Id
}

# The engine is done, so the server has nothing left to answer. Kill it before the log
# analysis below, which does not need it -- otherwise a Ctrl+C during analysis leaves a
# python holding the port and the NEXT -Serve run dies with "port in use".
if ($httpServer -and -not $httpServer.HasExited) {
    Stop-Process -Id $httpServer.Id -Force -ErrorAction SilentlyContinue
}


if ($cmdFile -and (Test-Path $cmdFile)) {
    Remove-Item $cmdFile -Force -ErrorAction SilentlyContinue
}

if (!(Test-Path $LogPath)) { exit 0 }

# ★같은 블록을 로그 파일에도 남긴다.★ 콘솔 출력은 사람이 console.txt 로 따로 저장해야 남지만
# .err.log 는 공유 폴더로 그대로 옮겨진다 -- run 10 이 다섯 세션을 돌아다니는 동안 어느 암이
# 어느 전송로로 돌았는지 ***로그만 봐서는 알 수 없었던*** 것이 그래서다.
#
# ★머리가 아니라 꼬리에 붙인다.★ cmd 의 `2>` 도 -RedirectStandardError 도 기동 시 파일을
# 자르므로 미리 써 둔 머리글은 날아간다. 꼬리는 두 분기에서 똑같이 동작한다.
#
# 분석부의 패턴 중 `RUNCONFIG` 로 시작하는 줄을 읽는 것은 없다. -SimpleMatch 카운터 다섯 개
# ("profile_id=", "direct file playback", "[dcomp-native] engaged", "[wr-tile-size] picture
# tile size override", "panicked")도 이 블록에는 안 걸린다 -- ***단 -Url 이나 -Pref 에 그
# 문자열을 넣으면 args 줄을 통해 섞인다.*** 그때는 마커 수가 1 늘고, 그건 눈에 보인다.
try {
    Add-Content -Path $LogPath -Encoding utf8 -Value (
        @("") + ($runConfig | ForEach-Object { "RUNCONFIG $_" })
    )
} catch {
    Write-Warning "could not append RUN CONFIG to ${LogPath}: $($_.Exception.Message)"
}

Write-Host ""
Write-Host "RUN CONFIG (this log)"
$runConfig | ForEach-Object { Write-Host "  $_" }

function CountOf($pattern) {
    (Select-String -Path $LogPath -Pattern $pattern -SimpleMatch -EA SilentlyContinue | Measure-Object).Count
}

$d3d11   = CountOf "profile_id="
$direct  = CountOf "direct file playback"
$dcompMk = CountOf "[dcomp-native] engaged"
$tileMk  = CountOf "[wr-tile-size] picture tile size override"
$panic   = CountOf "panicked"
$egl     = CountOf "EGLImage"
$fanout  = CountOf "fan-out is BROKEN"

Write-Host ""
Write-Host "markers: d3d11=$d3d11/$tiles direct_file=$direct/$tiles dcomp_engaged=$dcompMk wr_tile_override=$tileMk panics=$panic"

# --- per-tile render rate: the wall runs at the SLOWEST tile, not the average ---
$ends = Select-String -Path $LogPath -Pattern "Wall render end: painter PainterId\((\d+)\)" -EA SilentlyContinue
$perPainter = @{}
foreach ($m in $ends) { $p = $m.Matches[0].Groups[1].Value; $perPainter[$p] = 1 + $perPainter[$p] }
if ($perPainter.Count -gt 0 -and $DurationSec -gt 0) {
    $line = ($perPainter.Keys | Sort-Object | ForEach-Object {
        "P{0}={1:N1}fps" -f $_, ($perPainter[$_] / $DurationSec) }) -join "  "
    Write-Host "tiles  : $line"
    $rates = $perPainter.Values | ForEach-Object { $_ / $DurationSec }
    $spread = ($rates | Measure-Object -Maximum).Maximum - ($rates | Measure-Object -Minimum).Minimum
    if ($spread -gt 2.0) {
        Write-Warning ("Tile rates differ by {0:N1} fps -- the wall is NOT coherent. A config that raises the average while splitting the tiles is a failure, not a win." -f $spread)
    }
}

# --- multi-GPU health ---
Write-Host ""
Write-Host "WALLDIAG -- ring owner devices (expect one per GPU that shows video):"
Select-String -Path $LogPath -Pattern "device=0x[0-9a-f]+" -AllMatches -EA SilentlyContinue |
    ForEach-Object { $_.Matches | ForEach-Object { $_.Value } } |
    Group-Object | Sort-Object Name | ForEach-Object { "  {0}  x{1}" -f $_.Name, $_.Count } | Write-Host
Write-Host "WALLDIAG -- per-painter wrap outcome (expect OK once per painter, no FAIL):"
Select-String -Path $LogPath -Pattern "WALLDIAG wrap (OK|FAIL)" -EA SilentlyContinue |
    ForEach-Object { "  " + ($_.Line -replace "^.*WALLDIAG", "WALLDIAG") } | Write-Host
Write-Host "EGLImage wrap failures: $egl   fan-out BROKEN warnings: $fanout"

# --- D3D11PROF: where the per-frame media time goes (only with -D3d11Profile) ---
$prof = Select-String -Path $LogPath -Pattern "D3D11PROF id=\d+ over=\S+ total=([\d.]+) claim=([\d.]+) copy=([\d.]+) publish=([\d.]+)" -EA SilentlyContinue
if ($prof.Count -gt 0) {
    $tot = @(); $cop = @(); $cla = @(); $pub = @()
    foreach ($m in $prof) {
        $g = $m.Matches[0].Groups
        $tot += [double]$g[1].Value; $cla += [double]$g[2].Value
        $cop += [double]$g[3].Value; $pub += [double]$g[4].Value
    }
    function Stat($a, $name) {
        $sorted = $a | Sort-Object
        $p50 = $sorted[[int]($sorted.Count * 0.5)]
        $p90 = $sorted[[int]($sorted.Count * 0.9)]
        $avg = ($a | Measure-Object -Average).Average
        "  {0,-8} p50={1,7:N2}ms  p90={2,7:N2}ms  avg={3,7:N2}ms" -f $name, $p50, $p90, $avg
    }
    Write-Host ""
    Write-Host "D3D11PROF -- per-frame media stage timings (n=$($prof.Count) samples):"
    Write-Host (Stat $tot "total")
    Write-Host (Stat $cla "claim")
    Write-Host (Stat $cop "copy")
    Write-Host (Stat $pub "publish")
    $copyShare = 100 * (($cop | Measure-Object -Sum).Sum) / [math]::Max((($tot | Measure-Object -Sum).Sum), 0.001)
    Write-Host ("  copy is {0:N0}% of total media stage time" -f $copyShare)
    Write-Host "  (copy = CPU memcpy of decoded planes into GPU-mapped memory)"
}

# --- VIDEORATE: is the decoder running at playback speed? (only with -VideoRate) ---
# The whole point of this block: CPU numbers cannot tell "decoding 2.7x too fast" apart
# from "decoding at 1x but each frame costs 2.7x under contention". pts_rate can.
$rate = Select-String -Path $LogPath -Pattern "VIDEORATE id=(\d+) fps=([\d.]+) pts_rate=([\d.]+)x" -EA SilentlyContinue
$wrapped = (Select-String -Path $LogPath -Pattern "pts_rate=wrapped" -EA SilentlyContinue | Measure-Object).Count
if ($rate) {
    # NOT $rows: PowerShell variable names are case-insensitive, so $rows IS the
    # [int] $Rows parameter above and assigning an array to it fails at runtime.
    $rateRows = foreach ($m in $rate) {
        [pscustomobject]@{
            Id   = [int]$m.Matches[0].Groups[1].Value
            Fps  = [double]$m.Matches[0].Groups[2].Value
            Rate = [double]$m.Matches[0].Groups[3].Value
        }
    }
    # Drop each pipeline's FIRST window: it starts at the first sample, which lands
    # mid-preroll, so it reads far low (measured: 0.30x while the rest sat at 1.00x).
    $kept = foreach ($g in ($rateRows | Group-Object Id)) {
        if ($g.Count -gt 1) { $g.Group | Select-Object -Skip 1 }
    }
    if ($kept) {
        $fpsSorted  = @($kept.Fps)  | Sort-Object
        $rateSorted = @($kept.Rate) | Sort-Object
        $medRate = $rateSorted[[int][math]::Floor($rateSorted.Count / 2)]
        $medFps  = $fpsSorted[[int][math]::Floor($fpsSorted.Count / 2)]
        Write-Host ""
        Write-Host "VIDEORATE -- delivered frames per pipeline ($(($rateRows | Group-Object Id).Count) pipelines, $($fpsSorted.Count) windows):"
        Write-Host ("  fps       min={0:N1}  median={1:N1}  max={2:N1}" -f $fpsSorted[0], $medFps, $fpsSorted[-1])
        Write-Host ("  pts_rate  min={0:N2}x median={1:N2}x max={2:N2}x" -f $rateSorted[0], $medRate, $rateSorted[-1])
        if ($wrapped -gt 0) { Write-Host "  ($wrapped window(s) skipped: gapless loop wrapped pts backwards)" }
        if ($medRate -gt 1.25) {
            Write-Warning ("decoders run {0:N2}x faster than playback -- the sink is NOT throttling. That is where the CPU goes, not contention." -f $medRate)
        } elseif ($medRate -lt 0.85) {
            Write-Host "  -> pipelines are BEHIND playback speed: frames are not being produced fast enough."
        } else {
            Write-Host "  -> playback speed is normal. High per-video CPU is then per-frame cost (contention), not extra frames."
        }
    }
}

# --- FRAMEREASON: which call site is producing the composites (only with -FrameReason) ---
$fr = Select-String -Path $LogPath -Pattern "FRAMEREASON total=(\d+) window_ms=([\d.]+) (.*)$" -EA SilentlyContinue
if ($fr) {
    # ★단일 생산자 판정은 ***창마다*** 한다 -- 로그 전체 누적이 아니다.★ 2026-10-22 전수
    # 리뷰에서 고쳤다.
    #
    # 고장: 부트스트랩 예외(`Painter::send_initial_pipeline_transaction`)는 WR 호출 지점을
    # 하나 더 만들고, 그것이 누적 집계에서는 ***영구히*** 남는다. 그래서 0 초에 난 정상적인
    # 부트스트랩 한 장이 건강한 실행 내내 "not yet two" 를 찍었다 -- 설계 8 절이 구조
    # 판정으로 지목한 바로 그 지표에서. (이번 세션에 런처 게이트가 거짓 경보를 낸 두 번째
    # 사례다. 앞의 것은 비율이 아니라 `fallback > 0` 로 판정했다.)
    #
    # ★줄 번호로 빼지 않는 이유.★ 뺄 이름이 `painter.rs:<줄>` 이라 그 파일을 한 줄만
    # 건드려도 낡는다 -- 이 저장소는 같은 함정에 이미 두 번 빠졌다(시험 리터럴, 2026-10-21
    # 과 10-22, 둘 다 그것을 적은 커밋 자신의 줄 이동으로 낡았다). 창 단위 판정은 번호를
    # 몰라도 되고, 덤으로 ★"두 생산자가 ***동시에*** 있었나" 와 "서로 다른 때에 하나씩
    # 있었나" 를 가른다★ -- 누적 집계는 그 둘을 구별하지 못한다.
    $bootstrapTimes = @()
    foreach ($b in (Select-String -Path $LogPath -Pattern "WALLBOOTSTRAP" -EA SilentlyContinue)) {
        if ($b.Line -match "\[(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z)") {
            $bootstrapTimes += [datetime]::Parse($Matches[1], [Globalization.CultureInfo]::InvariantCulture, [Globalization.DateTimeStyles]::RoundtripKind)
        }
    }

    $tally = @{}
    $totals = @()
    $cleanWindows = 0
    $skippedWindows = 0
    $maxWrSitesInOneWindow = 0
    $windowsWithTwoWrSites = 0
    $externalSeenInCleanWindow = $false
    foreach ($m in $fr) {
        $totals += [int]$m.Matches[0].Groups[1].Value
        $windowMs = [double]$m.Matches[0].Groups[2].Value
        $windowSrcs = @{}
        foreach ($part in ($m.Matches[0].Groups[3].Value -split ' ')) {
            if ($part -match "^((?:painter\.rs:\d+|external_surface|wall_pacer_join)/\S+)=(\d+)$") {
                $tally[$Matches[1]] = [int]$Matches[2] + $tally[$Matches[1]]
                $windowSrcs[($Matches[1] -split '/')[0]] = $true
            }
        }
        # 이 창이 부트스트랩을 품었나. 창은 [t - window_ms, t] 이고 로그 타임스탬프가 초
        # 단위라 1.5 초를 더 준다. 부트스트랩은 파이프라인당 1 회뿐이라 넉넉히 버려도
        # 판정에 쓸 창은 남는다.
        $contaminated = $false
        if ($bootstrapTimes.Count -gt 0 -and $m.Line -match "\[(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z)") {
            $windowEnd = [datetime]::Parse($Matches[1], [Globalization.CultureInfo]::InvariantCulture, [Globalization.DateTimeStyles]::RoundtripKind)
            $windowStart = $windowEnd.AddMilliseconds(-($windowMs + 1500))
            foreach ($bt in $bootstrapTimes) {
                if ($bt -ge $windowStart -and $bt -le $windowEnd) { $contaminated = $true }
            }
        }
        if ($contaminated) {
            $skippedWindows++
            continue
        }
        $cleanWindows++
        $wrInWindow = @($windowSrcs.Keys | Where-Object { $_ -like 'painter.rs:*' }).Count
        if ($wrInWindow -gt $maxWrSitesInOneWindow) { $maxWrSitesInOneWindow = $wrInWindow }
        if ($wrInWindow -ge 2) { $windowsWithTwoWrSites++ }
        if ($windowSrcs.ContainsKey('external_surface')) { $externalSeenInCleanWindow = $true }
    }
    $windows = $fr.Count
    Write-Host ""
    Write-Host "FRAMEREASON -- who asked for the composites ($windows one-second windows):"
    Write-Host ("  composites/s  avg={0:N1}  max={1}" -f (($totals | Measure-Object -Average).Average, ($totals | Measure-Object -Maximum).Maximum))
    foreach ($k in ($tally.Keys | Sort-Object { -$tally[$_] })) {
        Write-Host ("    {0,8:N1}/s  {1}" -f ($tally[$k] / [math]::Max($windows, 1)), $k)
    }
    # wall_pacer_join 은 문이 열린 횟수라 출처로 세지 않는다(합성 total 에도 안 들어간다).
    $wrSitesWholeLog = @($tally.Keys | ForEach-Object { ($_ -split '/')[0] } | Where-Object { $_ -like 'painter.rs:*' } | Sort-Object -Unique)
    $joinKeys = @($tally.Keys | Where-Object { $_ -like 'wall_pacer_join/*' })
    if ($cleanWindows -eq 0) {
        $verdict = "no verdict: every window contained a WALLBOOTSTRAP"
    } elseif ($windowsWithTwoWrSites -gt 0) {
        $verdict = "FAIL: $windowsWithTwoWrSites of $cleanWindows clean windows had two or more WebRender sites AT ONCE"
    } elseif (-not $externalSeenInCleanWindow) {
        $verdict = "OK (one WebRender site; external_surface never fired -- no promoted external video in this run)"
    } else {
        $verdict = "OK (one WebRender site + external_surface)"
    }
    Write-Host ("  clean windows={0} (skipped {1} that contained a WALLBOOTSTRAP), max WebRender sites in one window={2}, wall_pacer_join keys={3}" -f $cleanWindows, $skippedWindows, $maxWrSitesInOneWindow, $joinKeys.Count)
    Write-Host ("  whole-log WebRender sites={0} (the bootstrap exception adds one permanently; NOT the verdict) -- {1}" -f $wrSitesWholeLog.Count, $verdict)
}

# --- WEBGLFANOUT: is a spanning WebGL canvas paying for pixels or for the replay? ---
# The verdict this is here to deliver: switches == applies means the replay loop changes GL
# context on EVERY command, and switch+lock time is then the thing to attack (invert the loop).
# If apply time dominates instead, the replay is doing real GL work and inverting buys nothing.
$wgf = Select-String -Path $LogPath -Pattern "WEBGLFANOUT window_ms=([\d.]+) swaps=(\d+) ctx=(\d+) dev=(\d+) cmds=(\d+) applies=(\d+) switches=(\d+) lock_wait_ms=([\d.]+) switch_ms=([\d.]+) apply_ms=([\d.]+) serialize_ms=([\d.]+) swap_sync_ms=([\d.]+) swap_syncs=(\d+) swap_ms=([\d.]+) clear_ms=([\d.]+) busy_ms=([\d.]+)" -EA SilentlyContinue
if ($wgf) {
    $g = { param($m, $i) [double]$m.Matches[0].Groups[$i].Value }
    $n        = $wgf.Count
    $window   = ($wgf | ForEach-Object { & $g $_ 1 } | Measure-Object -Sum).Sum
    $dev      = ($wgf | ForEach-Object { & $g $_ 4 } | Measure-Object -Maximum).Maximum
    $cmds     = ($wgf | ForEach-Object { & $g $_ 5 } | Measure-Object -Sum).Sum
    $applies  = ($wgf | ForEach-Object { & $g $_ 6 } | Measure-Object -Sum).Sum
    $switches = ($wgf | ForEach-Object { & $g $_ 7 } | Measure-Object -Sum).Sum
    $lockMs   = ($wgf | ForEach-Object { & $g $_ 8 } | Measure-Object -Sum).Sum
    $switchMs = ($wgf | ForEach-Object { & $g $_ 9 } | Measure-Object -Sum).Sum
    $applyMs  = ($wgf | ForEach-Object { & $g $_ 10 } | Measure-Object -Sum).Sum
    $serMs    = ($wgf | ForEach-Object { & $g $_ 11 } | Measure-Object -Sum).Sum
    $syncMs   = ($wgf | ForEach-Object { & $g $_ 12 } | Measure-Object -Sum).Sum
    $syncN    = ($wgf | ForEach-Object { & $g $_ 13 } | Measure-Object -Sum).Sum
    $swapMs   = ($wgf | ForEach-Object { & $g $_ 14 } | Measure-Object -Sum).Sum
    $clearMs  = ($wgf | ForEach-Object { & $g $_ 15 } | Measure-Object -Sum).Sum
    $busyMs   = ($wgf | ForEach-Object { & $g $_ 16 } | Measure-Object -Sum).Sum
    $pct      = { param($ms) if ($window -gt 0) { 100.0 * $ms / $window } else { 0.0 } }
    Write-Host ""
    Write-Host "WEBGLFANOUT -- WebGL multi-GPU replay cost ($n one-second windows, max $dev backend devices):"
    Write-Host ("  commands/s {0,10:N0}   applies/s {1,10:N0}   switches/s {2,10:N0}" -f ($cmds / $n), ($applies / $n), ($switches / $n))
    Write-Host ("  context switch   {0,9:N1} ms  ({1,5:N1}% of window)" -f $switchMs, (& $pct $switchMs))
    Write-Host ("  ANGLE lock wait  {0,9:N1} ms  ({1,5:N1}% of window)" -f $lockMs, (& $pct $lockMs))
    Write-Host ("  apply (w/ switch){0,9:N1} ms  ({1,5:N1}% of window)" -f $applyMs, (& $pct $applyMs))
    Write-Host ("  postcard trip    {0,9:N1} ms  ({1,5:N1}% of window)" -f $serMs, (& $pct $serMs))
    # What the producer-side settle costs. It is meant to be large here and small in the
    # wall pass -- this thread has the headroom, Commit does not.
    Write-Host ("  swap sync        {0,9:N1} ms  ({1,5:N1}% of window)  on {2:N0} swaps" -f $syncMs, (& $pct $syncMs), $syncN)
    # ***The swap path, which nothing measured until now.*** Before these two counters the
    # thread's second was 95.8% unaccounted, so "one triangle and it will not reach 30fps"
    # could not be answered: an idle thread and a thread stuck in swap_buffers looked the same.
    Write-Host ("  swap_buffers     {0,9:N1} ms  ({1,5:N1}% of window)  on {2:N0} swaps" -f $swapMs, (& $pct $swapMs), $swaps)
    Write-Host ("  clear_surface    {0,9:N1} ms  ({1,5:N1}% of window)" -f $clearMs, (& $pct $clearMs))
    Write-Host ("  ---- BUSY TOTAL  {0,9:N1} ms  ({1,5:N1}% of window)" -f $busyMs, (& $pct $busyMs))
    $busyPct = & $pct $busyMs
    if ($busyPct -lt 40) {
        Write-Host ("  => The WebGL thread is IDLE {0:N0}% of the time. The canvas is NOT the limit --" -f (100 - $busyPct)) -ForegroundColor Green
        Write-Host "     it is only asked for $([math]::Round($swaps / $n, 1)) frames a second. Chase whatever drives rAF (the wall pass rate)."
    } else {
        Write-Host ("  => The WebGL thread is BUSY {0:N0}% of the time; it really is a producer-side limit." -f $busyPct) -ForegroundColor Yellow
        Write-Host "     swap_buffers vs clear_surface above says whether buffer turnover or clearing dominates."
    }
    if ($applies -gt 0) {
        $ratio = $switches / $applies
        Write-Host ("  switches per apply {0:N3}" -f $ratio)
        if ($ratio -gt 0.95) {
            Write-Warning "Every replayed command changes GL context (switches ~= applies). This is the interleaved device loop; inverting it would cut switches from 4N to 4."
        } else {
            Write-Host "  Context switching is NOT per-command -- look elsewhere before inverting the replay loop."
        }
    }
} elseif ($FanoutProf) {
    Write-Warning "-FanoutProf was set but no WEBGLFANOUT line was logged. The counters only emit at a swap boundary, so a run with no WebGL canvas produces nothing."
}

# --- WEBGLWATCHDOG: the stall. Every other instrument here reports from the healthy path
# only -- WEBGLFANOUT at a swap boundary, WEBGLEXTIMG at a lock -- so when the four tiles
# freeze they all go quiet at once and "no lines" cannot tell a blocked thread from an idle
# one. This block is written by a SEPARATE thread once a second, ALWAYS, so it still speaks
# when the WebGL thread cannot.
#
# ***Counting messages was not enough.*** The first version counted only "did the loop turn"
# and reported no stall through a 38s freeze (log_webgpu/49) -- because WebRender keeps
# sending FinishedRenderingToContext back on every painter render, which turns the loop even
# with script dead. So the counters are split by KIND, and cmds is the one that means script:
#   cmds=0 while other>0  -> script stopped asking for draws. The fault is UPSTREAM.
#   cmds>0 but swaps=0    -> drawing continues but submission is gated.
#   all 0 and stuck_s>0   -> the WebGL thread itself is blocked, at the phase named.
$wdog = Select-String -Path $LogPath -Pattern "WEBGLWATCHDOG cmds=\+(\d+) swaps=\+(\d+) other=\+(\d+) phase=\[([^\]]+)\] stuck_s=(\d+)" -EA SilentlyContinue
if ($wdog) {
    $wins = foreach ($hit in $wdog) {
        $g = $hit.Matches[0].Groups
        [pscustomobject]@{
            Cmds   = [int]$g[1].Value
            Swaps  = [int]$g[2].Value
            Other  = [int]$g[3].Value
            Phase  = $g[4].Value
            Stuck  = [int]$g[5].Value
        }
    }
    $wins = @($wins)
    $dead     = @($wins | Where-Object { $_.Cmds -eq 0 -and $_.Other -gt 0 })
    $gated    = @($wins | Where-Object { $_.Cmds -gt 0 -and $_.Swaps -eq 0 })
    $blocked  = @($wins | Where-Object { $_.Cmds -eq 0 -and $_.Other -eq 0 -and $_.Stuck -ge 2 })
    $live     = @($wins | Where-Object { $_.Cmds -gt 0 -and $_.Swaps -gt 0 })
    Write-Host ""
    Write-Host "WEBGLWATCHDOG -- $($wins.Count) one-second windows on the WebGL thread:"
    Write-Host ("  healthy (cmds>0, swaps>0) : {0}" -f $live.Count)
    Write-Host ("  script silent (cmds=0)    : {0}" -f $dead.Count)
    Write-Host ("  swaps gated (cmds>0,sw=0) : {0}" -f $gated.Count)
    Write-Host ("  thread blocked            : {0}" -f $blocked.Count)
    if ($blocked.Count -gt 0) {
        $where = ($blocked | Group-Object Phase | Sort-Object Count -Descending | Select-Object -First 1).Name
        Write-Host "  => the WebGL THREAD blocked, at [$where]. That step is the thing to fix." -ForegroundColor Red
    } elseif ($dead.Count -gt 0) {
        Write-Host "  => script stopped asking for draws for $($dead.Count)s while the painters kept running." -ForegroundColor Yellow
        Write-Host "     The WebGL thread is a victim here. Look UPSTREAM: script / layout / rAF." -ForegroundColor Yellow
    } elseif ($gated.Count -gt 0) {
        Write-Host "  => drawing continued but SwapBuffers stopped arriving for $($gated.Count)s." -ForegroundColor Yellow
        Write-Host "     Look at what asks for the swap (rAF -> compositor), not at GL." -ForegroundColor Yellow
    } else {
        Write-Host "  => no stall in this run: every window had both draws and swaps."
    }
} elseif ($FanoutProf) {
    Write-Warning "-FanoutProf was set but no WEBGLWATCHDOG line was logged. That thread emits once a second unconditionally, so its silence means it never started -- not that the run was healthy."
}

# --- TILESPIKE: plan A step 1. Can a tile create and make current its own context off the
# main thread? Everything else about parallel tile render depends on this answer.
$spike = Select-String -Path $LogPath -Pattern "TILESPIKE VERDICT: (PASS|FAIL)" -EA SilentlyContinue
$spikeTiles = Select-String -Path $LogPath -Pattern "TILESPIKE tile (\d+): (OK|CREATE FAILED|MAKE_CURRENT FAILED)" -EA SilentlyContinue
if ($spike -or $spikeTiles) {
    Write-Host ""
    Write-Host "TILESPIKE -- per-tile context creation on worker threads:"
    foreach ($hit in $spikeTiles) {
        $line = $hit.Line -replace '^\[[^\]]*\]\s*', ''
        Write-Host "  $line"
    }
    if ($spike) {
        $verdict = $spike[-1].Matches[0].Groups[1].Value
        if ($verdict -eq 'PASS') {
            Write-Host "  => PASS: plan A's blocking precondition holds; step 2 (move Painter) is next." -ForegroundColor Green
        } else {
            Write-Host "  => FAIL: plan A does not stand as designed. Read the per-tile lines above." -ForegroundColor Red
        }
    } else {
        Write-Warning "Per-tile TILESPIKE lines exist but no VERDICT line -- the run died before joining its workers."
    }
} elseif ($TileThreadSpike) {
    Write-Warning "-TileThreadSpike was set but no TILESPIKE line was logged, so the spike never ran. It is Windows-only and runs before any rendering; check the engine actually reached tile creation."
}

# --- SURFIMPORT: inside the call that used to dominate the wall pass.
# create_surface_texture was ~93% of paint in BOTH DComp modes (900-935 ms/s across four
# painters, log_webgpu/57-59), 10.01 ms an import. Caching the import per share handle took it
# to 0.08 ms (log_webgpu/70) and the whole pass from 21.34 ms to 1.47 ms. This block is kept as
# a regression watch, not a hunt: what it has to catch is the cache MISSING -- a resize storm,
# or a producer handing out a fresh share handle every frame -- which brings the old cost back
# whole. That is why reused= is in the header and is checked before the percentages.
# `live=` was added to the line later and is optional here on purpose. When it was added
# this pattern stopped matching, and the miss did not read as "the pattern is stale" -- it
# read as the warning below, "no canvas was imported at all", on a run where 82 SURFIMPORT
# lines were sitting in the log. A parser that reports absence must be loose about fields
# it does not use.
$imp = Select-String -Path $LogPath -Pattern "SURFIMPORT imports=(\d+) reused=(\d+) \([\d.]+%\) (?:live=\d+ )?open_ms=([\d.]+) pbuffer_ms=([\d.]+) query_ms=([\d.]+) acquire_ms=([\d.]+) rest_ms=([\d.]+)" -EA SilentlyContinue
if ($imp) {
    # The numbers are cumulative, so the last line is the whole run.
    $impG = $imp[-1].Matches[0].Groups
    $impN    = [double]$impG[1].Value
    $impHits = [double]$impG[2].Value
    $open    = [double]$impG[3].Value
    $pbuf    = [double]$impG[4].Value
    $qry     = [double]$impG[5].Value
    $acq     = [double]$impG[6].Value
    $rest    = [double]$impG[7].Value
    $tot  = $open + $pbuf + $qry + $acq + $rest
    Write-Host ""
    Write-Host "SURFIMPORT -- inside create_surface_texture ($impN imports, $impHits served from the cache):"
    if ($tot -gt 0 -and $impN -gt 0) {
        Write-Host ("  OpenSharedResource {0,9:N1} ms  ({1,5:N1}%)  {2,6:N2} ms each" -f $open, (100 * $open / $tot), ($open / $impN))
        Write-Host ("  EGL pbuffer       {0,9:N1} ms  ({1,5:N1}%)  {2,6:N2} ms each" -f $pbuf, (100 * $pbuf / $tot), ($pbuf / $impN))
        Write-Host ("  EGL mutex query   {0,9:N1} ms  ({1,5:N1}%)  {2,6:N2} ms each" -f $qry, (100 * $qry / $tot), ($qry / $impN))
        Write-Host ("  AcquireSync       {0,9:N1} ms  ({1,5:N1}%)  {2,6:N2} ms each" -f $acq, (100 * $acq / $tot), ($acq / $impN))
        Write-Host ("  GL texture + bind {0,9:N1} ms  ({1,5:N1}%)  {2,6:N2} ms each" -f $rest, (100 * $rest / $tot), ($rest / $impN))
        Write-Host ("  TOTAL             {0,9:N1} ms                {1,6:N2} ms each" -f $tot, ($tot / $impN))
        # Query and AcquireSync were one number at first and that number was 68%, which could
        # not be acted on: an EGL call being slow and waiting on another device's GPU need
        # opposite fixes.
        # ***Read the share ratio before the percentages.*** Once the cache is hitting, these
        # five add up to well under a millisecond, and then "which is the biggest share" is a
        # question about noise. Reporting a dominant line anyway is how a solved cost center
        # keeps getting chased -- so say it is solved instead.
        $eachMs = $tot / $impN
        $hitPct = if ($impN -gt 0) { 100 * $impHits / $impN } else { 0 }
        if ($hitPct -ge 90 -and $eachMs -lt 0.5) {
            Write-Host ("  => Not a cost center any more: {0:N0}% of imports are cache hits and one costs {1:N2} ms." -f $hitPct, $eachMs) -ForegroundColor Green
            Write-Host "     The shares below are noise at this scale. Look at WALLPASS/WALLSPLIT for what is left." -ForegroundColor Green
        } else {
            $dom = @(
                @{ n = 'OpenSharedResource'; v = $open; fix = 'cache the opened texture per share handle -- a small, contained patch' },
                @{ n = 'EGL pbuffer';        v = $pbuf; fix = 'the pbuffer must be cached too, so the SurfaceTexture lifetime has to change' },
                @{ n = 'EGL mutex query';    v = $qry;  fix = 'an EGL call, so caching the import removes it' },
                # This line used to read "a WAIT, not creation -- caching will NOT help", and
                # that was measured to be wrong (log_webgpu/70). Re-opening the shared texture
                # every frame is what made the acquire expensive: caching the open took it from
                # 7.41 ms to 0.04 ms. So check the share ratio before blaming the producer.
                @{ n = 'AcquireSync';        v = $acq;  fix = 'a wait -- but if reused% is low, it is the re-import causing it. Cache first, then look for who holds the mutex' },
                @{ n = 'GL texture + bind';  v = $rest; fix = 'the GL texture must persist across frames, so the lifetime has to change' }
            ) | Sort-Object { $_.v } -Descending | Select-Object -First 1
            Write-Host ("  => {0} dominates: {1}" -f $dom.n, $dom.fix) -ForegroundColor Yellow
        }
    }
} else {
    Write-Warning "No SURFIMPORT line was logged. It is a warn! from surfman and emits once a second whenever a WebGL canvas is imported, so its absence means no canvas was imported at all -- not that the import is free."
}

# --- WALLSPLIT: where a wall pass actually spends its time (needs -PresentCadence).
# The DComp-on penalty is ~6.7ms a pass on the same page, and DCOMPBIND accounts for only the
# ~1.5ms spent outside the tile loop. This splits the tile step itself with the SAME window and
# denominator as WALLPASS -- mixing per-sample and per-window averages is how an earlier attempt
# produced a negative remainder.
$split = Select-String -Path $LogPath -Pattern "WALLSPLIT pass_ms=([\d.]+) = make_current=([\d.]+) \+ paint=([\d.]+) \+ present=([\d.]+) \+ dispatch=([\d.]+) \+ join=([\d.]+) \+ flush=([\d.]+) \+ outside=(-?[\d.]+)" -EA SilentlyContinue
if ($split) {
    # ***A run holds two regimes, and averaging them together lies.*** With DComp on, tile
    # surfaces are redrawn every frame until they get promoted to swapchains a few seconds in;
    # before that the Commit flush costs ~9.8ms a pass, after it ~0.05ms. A whole-run average
    # splits the difference and invents an "outside" cost that the steady state does not have.
    # Reporting the warm-up separately is the whole point.
    # NOT $rows -- PowerShell variable names are case-insensitive, so $rows IS the script's
    # own [int] $Rows parameter. Two other blocks in this file carry the same warning and it
    # still caught a third one here.
    $warmupWindows = 10
    $splitRows = @()
    $i = 0
    foreach ($hit in $split) {
        $g = $hit.Matches[0].Groups
        $splitRows += [pscustomobject]@{
            Phase   = if ($i -lt $warmupWindows) { 'warmup' } else { 'steady' }
            Pass    = [double]$g[1].Value
            Mc      = [double]$g[2].Value
            Paint   = [double]$g[3].Value
            Present  = [double]$g[4].Value
            Dispatch = [double]$g[5].Value
            Join     = [double]$g[6].Value
            Flush    = [double]$g[7].Value
            Outside  = [double]$g[8].Value
        }
        $i++
    }
    Write-Host ""
    Write-Host "WALLSPLIT -- where one wall pass goes (per pass, all tiles summed):"
    foreach ($phase in @('warmup', 'steady')) {
        $set = @($splitRows | Where-Object { $_.Phase -eq $phase })
        if ($set.Count -eq 0) { continue }
        $label = if ($phase -eq 'warmup') { "first $($set.Count)s (DComp: before swapchain promotion)" }
                 else { "steady state ($($set.Count) windows)" }
        Write-Host "  $label"
        Write-Host ("    pass_ms     {0,7:N2}" -f (($set | Measure-Object Pass -Average).Average))
        Write-Host ("      make_current{0,5:N2}" -f (($set | Measure-Object Mc -Average).Average))
        Write-Host ("      paint     {0,7:N2}   <- Painter::render for every tile" -f (($set | Measure-Object Paint -Average).Average))
        Write-Host ("      present   {0,7:N2}" -f (($set | Measure-Object Present -Average).Average))
        Write-Host ("      dispatch  {0,7:N2}   <- barrier keep-previous decision + inline painters" -f (($set | Measure-Object Dispatch -Average).Average))
        Write-Host ("      join      {0,7:N2}   <- waiting for the threaded tiles to finish" -f (($set | Measure-Object Join -Average).Average))
        Write-Host ("      flush     {0,7:N2}   <- deferred DComp Commit flush" -f (($set | Measure-Object Flush -Average).Average))
        Write-Host ("      outside   {0,7:N2}   <- still unaccounted: the loop itself" -f (($set | Measure-Object Outside -Average).Average))
    }
    Write-Host "  Compare the same page with -DComp on and -DComp off, STEADY STATE against steady state:"
    Write-Host "  the line that grows there is the one to chase. Warm-up differences are the promotion cost."
    Write-Host "  If join dominates, read WALLQUEUE next: it splits that wait into queue latency vs render."
} elseif ($PresentCadence) {
    Write-Warning "-PresentCadence was set but no WALLSPLIT line was logged. It comes from winit_wall's render pass, so a servoshell run or a run that never painted produces nothing."
}

# --- WALLQUEUE: the other half of WALLSPLIT's `join` (needs -PresentCadence).
# `join` is the wall clock the pass spent waiting for the threaded tiles, and it is
# `queued_ms + render_ms` per tile. The two have opposite causes: render is that tile's own
# work, queued is time the REQUEST sat in line -- the request channel is unbounded and shared
# with image updates (`dispatch_detached`), so a render request can queue behind those.
# WALLPASS already reports render (per-tile avg/max); this is the queue side, same window.
$queue = Select-String -Path $LogPath -Pattern "WALLQUEUE per-tile queued_ms avg=\[([\d.,]+)\] max=\[([\d.,]+)\]" -EA SilentlyContinue
if ($queue) {
    # Same two regimes as WALLSPLIT: warm-up before swapchain promotion, then steady state.
    $warmupWindows = 10
    $tileCount = @(($queue[0].Matches[0].Groups[1].Value -split ',')).Count
    $avgSums = New-Object 'double[]' $tileCount
    $maxes = New-Object 'double[]' $tileCount
    $steady = 0
    $i = 0
    foreach ($hit in $queue) {
        $avgs = @($hit.Matches[0].Groups[1].Value -split ',' | ForEach-Object { [double]$_ })
        $mx = @($hit.Matches[0].Groups[2].Value -split ',' | ForEach-Object { [double]$_ })
        for ($t = 0; $t -lt $tileCount; $t++) {
            if ($t -lt $mx.Count -and $mx[$t] -gt $maxes[$t]) { $maxes[$t] = $mx[$t] }
            # Steady state only for the average: the warm-up windows are a different regime.
            if ($i -ge $warmupWindows -and $t -lt $avgs.Count) { $avgSums[$t] += $avgs[$t] }
        }
        if ($i -ge $warmupWindows) { $steady++ }
        $i++
    }
    Write-Host ""
    Write-Host "WALLQUEUE -- how long a tile render request waited in line before its thread took it:"
    if ($steady -gt 0) {
        $perTile = @(0..($tileCount - 1) | ForEach-Object { "{0:N2}" -f ($avgSums[$_] / $steady) })
        Write-Host ("  steady-state avg ms per tile  [{0}]  ({1} windows)" -f ($perTile -join ", "), $steady)
    } else {
        Write-Host "  (run too short for a steady-state window)"
    }
    Write-Host ("  worst single window per tile  [{0}]" -f (@(0..($tileCount - 1) | ForEach-Object { "{0:N2}" -f $maxes[$_] }) -join ", "))
    Write-Host "  Near zero means the tile threads were idle and took the request at once, so WALLSPLIT's"
    Write-Host "  join is real render time. Large means the request queued behind other work on that thread."
}

# --- WALLACK: the canvas-ack deadlock and its recovery.
# A document that drew a canvas is locked until a painter sends the ack, and both places that
# send it leave the recheck to a later update_images call that a canvas-only page never makes.
# WALLACKFLUSH counts acks the recovery actually sent -- it is the POSITIVE CONTROL, without
# which a clean run cannot be told apart from the recovery never running at all. WALLACKLATCH
# means recovery was not even possible, which is a different and worse bug.
$ackFlush = Select-String -Path $LogPath -Pattern "WALLACKFLUSH: painter PainterId\((\d+)\) sent an owed canvas ack \((\d+) so far\)" -EA SilentlyContinue
$ackLatch = Select-String -Path $LogPath -Pattern "WALLACKLATCH: painter PainterId\((\d+)\) held the canvas ack for (\d+)ms" -EA SilentlyContinue
Write-Host ""
if ($ackLatch) {
    $worstHeld = ($ackLatch | ForEach-Object { [int]$_.Matches[0].Groups[2].Value } | Measure-Object -Maximum).Maximum
    Write-Host "WALLACK -- STALLED: a painter held script's canvas ack for up to $([math]::Round($worstHeld/1000,1))s across $($ackLatch.Count) report(s)." -ForegroundColor Red
    Write-Host "  Recovery could not run, so the gate values on those lines say which condition blocked it."
} elseif ($ackFlush) {
    # The count in the line is that painter's RUNNING TOTAL, not a delta -- bursts are
    # coalesced to one line a second, so summing the lines would double-count. Take the
    # highest total each painter reported.
    $byPainter = $ackFlush | Group-Object { $_.Matches[0].Groups[1].Value } | Sort-Object Name
    $perPainter = foreach ($p in $byPainter) {
        [pscustomobject]@{
            Painter = $p.Name
            Count   = ($p.Group | ForEach-Object { [int]$_.Matches[0].Groups[2].Value } | Measure-Object -Maximum).Maximum
        }
    }
    $sent = ($perPainter | Measure-Object Count -Sum).Sum
    Write-Host "WALLACK -- recovery is live and no stall: $sent owed ack(s) sent, none left held." -ForegroundColor Green
    foreach ($p in $perPainter) {
        Write-Host ("  painter {0}: {1} ack(s) recovered" -f $p.Painter, $p.Count)
    }
    Write-Host "  Each one is a frame that would have hung the wall permanently before this fix."
} else {
    Write-Host "WALLACK -- no stall, and recovery never had to fire in this run."
    Write-Host "  Note this is the ambiguous outcome: it does not prove the recovery path works." -ForegroundColor Yellow
    Write-Host "  To actually test it, reproduce the race on demand: -CanvasAckSkip 1"
}
$ackSkip = Select-String -Path $LogPath -Pattern "WALLACKSKIP: painter PainterId\((\d+)\) withheld an ack owed to (\d+) pipeline" -EA SilentlyContinue
if ($ackSkip) {
    Write-Host "  (failure injection was active: $($ackSkip.Count) ack(s) withheld on purpose)"
    if ($NoCanvasAckRecovery) {
        Write-Host "  Recovery was disabled for this run, so a stall here is the EXPECTED control result." -ForegroundColor Yellow
    }
} elseif ($CanvasAckSkip -gt 0) {
    Write-Warning "-CanvasAckSkip $CanvasAckSkip was set but no ack was ever withheld, so nothing was injected and the run tested nothing. The budget is only spent when an ack is actually owed, so this means no canvas ever put one in flight -- check that the page really has an animating WebGL canvas and that -Pref dom_webgl2_enabled=true was passed."
}

# --- WEBGLEXTIMG: the consumer side -- is the painter WAITING for the canvas, or is there
# nothing to take? A tile render costs 0.23ms with no WebGL canvas and 15ms with one, and all
# of that lands inside renderer.render() with draw_calls=2 and upload_mb=0.0. Neither drawing
# nor uploading leaves waiting, and this callback is the only place a tile can wait.
$wex = Select-String -Path $LogPath -Pattern "WEBGLEXTIMG painter=PainterId\((\d+)\) window_ms=([\d.]+) locks=(\d+) lock_ms=([\d.]+) take_ms=([\d.]+) create_ms=([\d.]+) no_front_buffer=(\d+) unlocks=(\d+) unlock_ms=([\d.]+) destroy_ms=([\d.]+) stage_ms=([\d.]+) stages=(\d+)" -EA SilentlyContinue
if ($wex) {
    $g = { param($m, $i) [double]$m.Matches[0].Groups[$i].Value }
    # NOT $rows -- see the VIDEORATE block above. PowerShell variable names are
    # case-insensitive, so $rows IS the [int] $Rows parameter and assigning a hashtable to
    # it throws at runtime. That warning was already written down and still got walked into,
    # so it is repeated here at the second site rather than left in one place.
    $extRows = @{}
    foreach ($m in $wex) {
        $id = [int](& $g $m 1)
        if (-not $extRows.ContainsKey($id)) { $extRows[$id] = @{ w=0.0; n=0; locks=0.0; lock=0.0; take=0.0; create=0.0; nofb=0.0; unlock=0.0; destroy=0.0; stage=0.0 } }
        $r = $extRows[$id]
        $r.w += (& $g $m 2); $r.n++
        $r.locks += (& $g $m 3); $r.lock += (& $g $m 4); $r.take += (& $g $m 5)
        $r.create += (& $g $m 6); $r.nofb += (& $g $m 7)
        $r.unlock += (& $g $m 9); $r.destroy += (& $g $m 10); $r.stage += (& $g $m 11)
    }
    Write-Host ""
    Write-Host "WEBGLEXTIMG -- what a tile pays to consume the WebGL canvas:"
    Write-Host ("  {0,-8} {1,8} {2,10} {3,11} {4,11} {5,12} {6,12}" -f "painter", "locks/s", "lock ms/s", "unlock ms/s", "create ms/s", "no_front_buf", "destroy ms/s")
    $totalCreate = 0.0; $totalNofb = 0.0; $totalLock = 0.0; $totalUnlock = 0.0; $totalStage = 0.0
    foreach ($id in ($extRows.Keys | Sort-Object)) {
        $r = $extRows[$id]
        $secs = [math]::Max($r.w / 1000.0, 0.001)
        Write-Host ("  {0,-8} {1,8:N1} {2,10:N1} {3,11:N1} {4,11:N1} {5,12:N0} {6,12:N1}" -f $id, ($r.locks/$secs), ($r.lock/$secs), ($r.unlock/$secs), ($r.create/$secs), $r.nofb, ($r.destroy/$secs))
        $totalCreate += $r.create; $totalNofb += $r.nofb; $totalLock += $r.lock; $totalUnlock += $r.unlock; $totalStage += $r.stage
    }
    # The callback's whole cost, against the per-tile render it sits inside. Reporting only
    # the create/lock ratio hid this: create really is ~99% OF THE LOCK, but the lock is a
    # couple of ms a second, so the ratio was true and useless.
    $perTileMsPerSec = ($totalLock + $totalUnlock) / [math]::Max($extRows.Count, 1) /
        [math]::Max((($extRows.Values | ForEach-Object { $_.w } | Measure-Object -Sum).Sum / $extRows.Count / 1000.0), 0.001)
    Write-Host ("  callback total per tile: {0:N1} ms/s" -f $perTileMsPerSec)
    if ($totalStage -gt 0) {
        $stagePerTile = $totalStage / [math]::Max($extRows.Count, 1) /
            [math]::Max((($extRows.Values | ForEach-Object { $_.w } | Measure-Object -Sum).Sum / $extRows.Count / 1000.0), 0.001)
        Write-Host ("  staging copy per tile:  {0:N1} ms/s  (canvas copied onto the painter device)" -f $stagePerTile)
    }
    # Judge on the ABSOLUTE cost, not on internal ratios. A tile render costs ~15ms once a
    # canvas is on the page; if this callback is a few ms a second it cannot be that 15ms no
    # matter how the time splits inside it.
    if ($perTileMsPerSec -gt 100) {
        if ($totalLock -gt 0 -and (100.0 * $totalCreate / $totalLock) -gt 60) {
            Write-Warning "The painter is WAITING for the WebGL surface (create_texture dominates a callback that is itself expensive). Producer and consumer are handing one surface back and forth; break that coupling."
        } else {
            Write-Warning "The external-image callback is expensive, but not in create_texture. Split it further before concluding anything."
        }
    } elseif ($totalNofb -gt 0) {
        Write-Host "  no_front_buffer is non-zero -- some locks found nothing to take, so look at the producer side."
    } else {
        Write-Host "  Locking the canvas is not itself the cost -- a few ms a second, and the keyed-mutex"
        Write-Host "  acquire runs about 0.4ms a lock whether the wall is fast or slow."
        Write-Host "  What decides the frame rate is whether the tile gets REDRAWN from this texture:"
        Write-Host "    static canvas  -> no tile binds -> Commit 0.007ms/frame -> 63.6fps"
        Write-Host "    video (same D3D11 device as the painter) -> 54 binds/s -> Commit 0.44ms -> 61.5fps"
        Write-Host "    animating canvas (isolated device, cross-device share) -> Commit 37ms -> 6.7fps"
        Write-Host "  So read DCOMPBIND's binds/s next to this: cost appears when a tile is redrawn from a"
        Write-Host "  CROSS-DEVICE texture. Parallelising the tile loop does not help -- that was measured"
        Write-Host "  and shelved (docs/multigpu/parallel_tile_render_design.md)."
    }
} elseif ($FanoutProf) {
    Write-Warning "-FanoutProf was set but no WEBGLEXTIMG line was logged. It only emits from the external-image unlock callback, so a run whose page never shows a WebGL canvas produces nothing."
}

# --- DCOMPBIND: where does the DComp compositor spend a frame? ---
# The tile round trip was measured first and cleared: 0.541ms a bind, 3.2 ms/s a painter, and
# only 6 binds/s against 18 renders/s -- most renders bind nothing yet all of them were slow,
# so the cost is not per tile. end_frame is the one thing here that runs every frame, and it
# holds the GL flush, the swap-chain Present and the DWM Commit.
# Parsed as key=value pairs, not as one long positional regex. The positional one rotted the
# moment a field was inserted in the middle (commit_ms_max): it stopped matching, the whole
# table vanished, and the branch below then warned "no DCOMPBIND line was logged" about a run
# whose log was full of them. A false "the compositor never engaged" is worse than no table.
$dbp = Select-String -Path $LogPath -Pattern "DCOMPBIND window_ms=" -EA SilentlyContinue
if ($dbp) {
    # Sums across windows, except *_max (a max of maxima) and the *_sizes counts, which are
    # "how many distinct sizes in THIS window" and mean nothing added up.
    $maxKeys = @("update_sizes", "tex_sizes")
    $f = @{}
    foreach ($m in $dbp) {
        foreach ($kv in [regex]::Matches($m.Line, '([A-Za-z_]+)=([\d.]+)(?:/([\d.]+))?')) {
            $k = $kv.Groups[1].Value
            $x = [double]$kv.Groups[2].Value
            if (-not $f.ContainsKey($k)) { $f[$k] = 0.0 }
            if ($k.EndsWith("_max") -or $maxKeys -contains $k) { $f[$k] = [math]::Max($f[$k], $x) }
            else                                               { $f[$k] += $x }
            # "a/b" fields (frames=60/60, flushed=59/0) keep the second half under <name>_b.
            if ($kv.Groups[3].Success) {
                $k2 = $k + "_b"
                if (-not $f.ContainsKey($k2)) { $f[$k2] = 0.0 }
                $f[$k2] += [double]$kv.Groups[3].Value
            }
        }
    }
    $g = { param($k) if ($f.ContainsKey($k)) { $f[$k] } else { 0.0 } }
    $w = (& $g "window_ms");    $bf = (& $g "frames");      $ef = (& $g "frames_b")
    $binds = (& $g "binds");    $full = (& $g "full_tile")
    $endf = (& $g "end_frame_ms"); $flush = (& $g "flush_ms")
    $flushed = (& $g "flushed"); $skipped = (& $g "flushed_b"); $commit = (& $g "commit_ms")
    $ext = (& $g "external_ms"); $extN = (& $g "externals")
    $pres = (& $g "present_ms"); $presN = (& $g "presents")
    $vis = (& $g "visuals");    $surf = (& $g "surfaces");  $defer = (& $g "deferred")
    $bd = (& $g "begin_ms");    $pbuf = (& $g "pbuffer_ms")
    $ed = (& $g "enddraw_ms");  $td = (& $g "teardown_ms")
    # window_ms sums across every compositor that emitted, one per painter, so it is already
    # painter-seconds; dividing by it gives a per-painter-per-second rate rather than a wall rate.
    $secs = [math]::Max($w / 1000.0, 0.001)
    $tiles = $bd + $pbuf + $ed + $td
    # When the Commit is deferred it happens after end_frame returns, so its time is NOT
    # inside end_frame_ms. Subtracting it from end_frame anyway drove "rest" negative --
    # the kind of nonsense that makes a reader distrust the whole table.
    $commitInFrame = if ($defer -gt 0) { 0.0 } else { $commit }
    Write-Host ""
    Write-Host ("DCOMPBIND -- DComp per painter ({0:N1} frames/s, {1:N1} binds/s):" -f ($ef/$secs), ($binds/$secs))
    Write-Host ("  end_frame     {0,9:N1} ms/s   {1,8:N3} ms per frame" -f ($endf/$secs), $(if ($ef -gt 0) { $endf/$ef } else { 0 }))
    Write-Host ("    gl flush    {0,9:N1} ms/s   (ran on {1:N0} frames, skipped on {2:N0})" -f ($flush/$secs), $flushed, $skipped)
    if ($DcompParallelCommit -and $commit -eq 0) {
        Write-Host "    Commit           (issued on worker threads -- not attributed per painter; read pass_ms in WALLPASS)"
    }
    Write-Host ("    Commit      {0,9:N1} ms/s   ({1})" -f ($commit/$secs), $(if ($defer -gt 0) { "deferred to end of pass on {0:N0} frames (default)" -f $defer } else { "issued in end_frame -- old behaviour" }))
    Write-Host ("    Present     {0,9:N1} ms/s   ({1:N0} swap-chain presents)" -f ($pres/$secs), $presN)
    Write-Host ("    rest        {0,9:N1} ms/s   (surface walk, promote/demote)" -f (($endf-$flush-$commitInFrame-$pres)/$secs))
    # The escape path lives OUTSIDE end_frame: WebRender does not draw video tiles at all, it
    # calls add_surface and we borrow the ring and present each video's own swap chain there.
    # Reporting it beside end_frame keeps a run from looking cheap while the cost sits next door.
    Write-Host ("  external video  {0,7:N1} ms/s  ({1:N0} external adds, outside end_frame)" -f ($ext/$secs), $extN)
    if ($ef -gt 0) {
        # Commit 이 비주얼 트리 크기를 따르는지 보기 위한 값. 비디오는 그리는 양이 훨씬
        # 많은데 Commit 이 29 배 싸므로, 원인은 그리는 양이 아니다. 트리 크기라면 여기서 보인다.
        Write-Host ("  visuals/frame {0,9:N1}   live surfaces {1,5:N1}   -> {2:N4} ms of Commit per visual" -f ($vis/$ef), ($surf/$dbp.Count), $(if ($vis -gt 0) { $commit/$vis } else { 0 }))
    }
    Write-Host ("  tile round trip {0,7:N1} ms/s  (BeginDraw {1:N1} / pbuffer {2:N1} / EndDraw {3:N1} / teardown {4:N1})" -f ($tiles/$secs), ($bd/$secs), ($pbuf/$secs), ($ed/$secs), ($td/$secs))
    Write-Host ("    worst single   BeginDraw {0:N2} / pbuffer {1:N2} / EndDraw {2:N2} / teardown {3:N2} ms" -f (& $g "begin_ms_max"), (& $g "pbuffer_ms_max"), (& $g "enddraw_ms_max"), (& $g "teardown_ms_max"))
    if ($binds -gt 0) { Write-Host ("  full-tile updates: {0:N0}/{1:N0}" -f $full, $binds) }
    # begin_frame is what WebRender reports as compositor_begin_ms, and that is where the
    # 22ms render stalls showed up. The three parts are measured inside it; "rest" is what is
    # left over (close_external_batch, the root offset) -- if rest is the big one, the suspect
    # is a line nobody has looked at yet.
    if ((& $g "begin_frame_ms") -gt 0) {
        Write-Host ("  begin_frame   {0,9:N1} ms/s   worst single pass {1:N2} ms" -f ((& $g "begin_frame_ms")/$secs), (& $g "begin_frame_ms_max"))
        Write-Host ("    deferred commit  {0,6:N1} ms/s   worst {1:N2} ms" -f ((& $g "bf_deferred_ms")/$secs), (& $g "bf_deferred_ms_max"))
        Write-Host ("    before_render    {0,6:N1} ms/s   worst {1:N2} ms   (the scheduler deadline flushed ahead of the render)" -f ((& $g "bf_early_ms")/$secs), (& $g "bf_early_ms_max"))
        Write-Host ("    rest             {0,6:N1} ms/s" -f ((& $g "bf_rest_ms")/$secs))
    }
    # RemoveAllVisuals used to sit in begin_frame and is now inside end_frame, next to the
    # AddVisual loop, so that the window where the visual tree is EMPTY falls inside the device
    # guard -- outside it, the scheduler could commit an empty tree and the bare window showed
    # through for a frame. It is reported on its own line because it is no longer part of
    # begin_frame; its cost is inside end_frame_ms above.
    if ((& $g "removeall_ms") -gt 0) {
        Write-Host ("  RemoveAllVisuals {0,8:N1} ms/s   worst single call {1:N2} ms  (inside end_frame)" -f ((& $g "removeall_ms")/$secs), (& $g "removeall_ms_max"))
    }
    # Does the BeginDraw update rect change shape every frame? If it does, DComp hands back a
    # differently sized texture each time and the EGL pbuffer (plus its depth/stencil) is built
    # and torn down 60-120 times a second. tex_reused is the direct count of "same texture came
    # back for this tile", so it is also the forecast for whether caching the pbuffer can hit.
    if ($binds -gt 0) {
        Write-Host ("  update rect   max {0:N0} px, {1:N0} distinct sizes in a window" -f (& $g "update_px_max"), (& $g "update_sizes"))
        Write-Host ("  BeginDraw tex max {0:N0} px, {1:N0} distinct sizes; same texture again {2:N0}/{3:N0} binds" -f (& $g "tex_px_max"), (& $g "tex_sizes"), (& $g "tex_reused"), $binds)
        Write-Host ("  new pbuffers  {0,9:N1} MB/s  (colour only; ANGLE builds a depth/stencil of the same size alongside)" -f ((& $g "alloc_mb")/$secs))
    }
    if ($ef -gt 0) {
        # Judge on everything this compositor costs, not on end_frame alone. The escape path
        # runs in add_surface, so an escape run can have a cheap end_frame and still be entirely
        # bounded by DComp -- reading only end_frame there would blame WebRender for our own work.
        # DComp total = end_frame + the escape path (add_surface) + begin_frame.
        #
        # begin_frame joined this sum once it was measured, and it changes verdicts: a run can
        # have a 1.7ms end_frame and still spend 21ms a frame inside begin_frame flushing the
        # scheduler's deadline. The old sum said "DComp is not the bottleneck" about exactly
        # that run. The deferred Commit is no longer added separately -- when begin_frame is
        # the one that flushes it, its cost is already inside bf_deferred_ms.
        $bfTotal = (& $g "begin_frame_ms")
        $perFrame = ($endf + $ext + $bfTotal) / $ef
        if ($perFrame -gt 5.0) {
            $parts = @(
                @{n="the external video path (add_surface)"; v=$ext},
                @{n="the DWM Commit"; v=$commit},
                @{n="begin_frame's before_render commit flush"; v=(& $g "bf_early_ms")},
                @{n="the GL flush"; v=$flush},
                @{n="the swap-chain Present"; v=$pres}
            ) | Sort-Object { -$_.v }
            Write-Warning ("DComp costs {0:N2} ms a frame -- this compositor IS the cost, and {1} is the largest part. Fixing it here is on the table." -f $perFrame, $parts[0].n)
            if ($commit -gt 0 -and $commit -ge $flush) {
                # Commit is per painter and blocking, and the painters run one after another on
                # the embedder thread, so N of them add up into the wall's frame time. That is
                # the shape parallelising the tile loop would overlap -- see
                # docs/multigpu/parallel_tile_render_design.md.
                Write-Host ("  Commit alone is {0:N2} ms a frame; four painters run serially, so that is ~{1:N0} ms of every wall pass." -f ($commit/$ef), (4.0*$commit/$ef))
            }
        } elseif ($skipped -gt 0 -and $flushed -eq 0) {
            # The conditional flush did its job. Say so, rather than reading a cheap end_frame
            # as evidence about WebRender -- that reading was written before the fix existed
            # and would now credit the wrong thing.
            Write-Host ("  end_frame is {0:N2} ms a frame and the flush was skipped on all {1:N0} of them." -f $perFrame, $skipped)
            Write-Host "  That is the conditional flush working: every surface was Virtual, so nothing needed submitting"
            Write-Host "  anywhere else. Check the wall visually -- this one is only proven by looking at it."
        } elseif ($extN -gt 0) {
            # Escape engaged. Say what it bought rather than repeating a verdict written for a
            # page that could not use it -- on the video grid this configuration holds 60fps
            # while cutting GPU occupancy from 22-35% to 14-27%.
            Write-Host ("  DComp costs {0:N2} ms a frame and the escape path took {1:N0} surfaces out of the content pass." -f $perFrame, $extN)
            Write-Host ("  Tile binds are down to {0:N1}/s, which is what buys the GPU back. This configuration is working." -f ($binds/$secs))
        } else {
            Write-Host ("  DComp costs only {0:N2} ms a frame, so it is not the bottleneck in this run." -f $perFrame)
            Write-Host "  Look at the WALLPASS lines for where the frame time actually goes."
        }
    }
} elseif ($DcompBindProf) {
    Write-Warning "-DcompBindProf was set but no DCOMPBIND line was logged. It emits from end_frame, so a run where the native compositor never engaged produces nothing."
}

# --- COMMITSITE: which call site actually pays for Commit()? ---
# Five of the six call sites had no timer, and the one that carries two thirds of all commits
# (begin_frame -> flush_before_render) was among them -- which is why DCOMPBIND commit_ms read
# 0.0 in every run while renders were stalling for 22ms inside begin_frame.
#
# The counters are process-global and one elected compositor drains them, so these are WALL
# totals, not per painter. Read "worst" first: one 22ms commit is a stall, sixty of them is
# structural load, and the two are fixed in opposite directions.
$cs = Select-String -Path $LogPath -Pattern "COMMITSITE window_ms=" -EA SilentlyContinue
if ($cs) {
    $siteNames = [ordered]@{
        ef = "end_frame"
        df = "deferred flush"
        br = "before_render (begin_frame)"
        sc = "scheduler / worker"
        di = "shell direct (fallback)"
        ex = "video fast-path"
    }
    $site = @{}
    $csWin = 0.0
    $csSlow = 0.0
    foreach ($m in $cs) {
        if ($m.Line -match "window_ms=([\d.]+) slow_ms=([\d.]+)") {
            $csWin += [double]$Matches[1]
            $csSlow = [double]$Matches[2]
        }
        foreach ($k in @($siteNames.Keys)) {
            if ($m.Line -match "\b$k n=(\d+) ms=([\d.]+) max=([\d.]+) over=(\d+)") {
                if (-not $site.ContainsKey($k)) { $site[$k] = @(0.0, 0.0, 0.0, 0.0) }
                $site[$k][0] += [double]$Matches[1]
                $site[$k][1] += [double]$Matches[2]
                $site[$k][2] = [math]::Max($site[$k][2], [double]$Matches[3])
                $site[$k][3] += [double]$Matches[4]
            }
        }
    }
    $csSecs = [math]::Max($csWin / 1000.0, 0.001)
    Write-Host ""
    Write-Host ("COMMITSITE -- DComp Commit() by call site over {0:N0}s (slow = {1:N0} ms or more):" -f $csSecs, $csSlow)
    $worstSite = ""
    $worstMs = 0.0
    foreach ($k in @($siteNames.Keys)) {
        if (-not $site.ContainsKey($k)) { continue }
        $t = $site[$k]
        if ($t[0] -eq 0) { continue }
        Write-Host ("  {0,-28} {1,8:N0} calls {2,9:N1} ms/s  worst {3,7:N2} ms  slow {4:N0}" -f $siteNames[$k], $t[0], ($t[1]/$csSecs), $t[2], $t[3])
        if ($t[2] -gt $worstMs) { $worstMs = $t[2]; $worstSite = $siteNames[$k] }
    }
    if ($worstMs -ge 10.0) {
        Write-Warning ("One Commit took {0:N1} ms at {1}. A render that waits that long drops frames on a 60Hz wall -- compare it with begin_frame's worst pass above: if they match, the Commit IS the stall." -f $worstMs, $worstSite)
    }
} elseif ($DcompBindProf) {
    Write-Warning "-DcompBindProf was set but no COMMITSITE line was logged. Only one compositor emits it, from the same window as DCOMPBIND -- if DCOMPBIND printed and this did not, the election in maybe_emit_bind_profile is broken."
}

# --- COMMITWORK: does a Commit cost what it CARRIES? ---
# COMMITSITE splits the same commits by where they were called from. That answered where the
# stall lives and nothing about why. The 2026-10-20 runs then showed, inside a single run, that
# the expensive seconds are the ones where DComp stops handing the same texture back (reuse
# 45% -> 4%, bf_early 7.9 -> 158.9 ms/s) while bind count, visual count and surface count all
# move the OTHER way -- the cheapest windows had twice the binds. That is a per-second
# correlation and nothing more, because a commit's duration was never attributed to the work it
# published. This table does that: each commit lands in a bucket by how many textures DComp had
# to realize since the previous commit on that device.
#
# Read avg, not max. If avg rises from new0 to new9p the cost is in the realization; if it is
# flat the texture-reuse signal was a passenger and the search moves elsewhere.
$cw = Select-String -Path $LogPath -Pattern "COMMITWORK window_ms=" -EA SilentlyContinue
if ($cw) {
    # "idle" is commits that carried no bind at all. It is listed for completeness and is NOT
    # the baseline: commits outrun binds (M0 measured 60 commits/s/device against 37 binds/s),
    # so an idle bucket is cheap by construction and comparing it against new1 would manufacture
    # the conclusion. The baseline is "reuse" -- it carried work, and every texture came back.
    $workTags = [ordered]@{
        idle   = "(carried nothing)"
        reuse  = "0 - all reused"
        new1   = "1"
        new2   = "2"
        new3_4 = "3-4"
        new5p  = "5 or more"
    }
    $bucket = @{}
    $cwWin = 0.0
    foreach ($m in $cw) {
        if ($m.Line -match "COMMITWORK window_ms=([\d.]+)") { $cwWin += [double]$Matches[1] }
        foreach ($k in @($workTags.Keys)) {
            if ($m.Line -match "\b$k n=(\d+) ms=([\d.]+) avg=([\d.]+) max=([\d.]+) vis=(\d+) bind=(\d+) same=(\d+)") {
                if (-not $bucket.ContainsKey($k)) { $bucket[$k] = @(0.0, 0.0, 0.0, 0.0, 0.0, 0.0) }
                $bucket[$k][0] += [double]$Matches[1]   # n
                $bucket[$k][1] += [double]$Matches[2]   # ms total
                $bucket[$k][2] = [math]::Max($bucket[$k][2], [double]$Matches[4])  # max
                $bucket[$k][3] += [double]$Matches[5]   # visuals
                $bucket[$k][4] += [double]$Matches[6]   # binds
                $bucket[$k][5] += [double]$Matches[7]   # same-texture binds
            }
        }
    }
    $cwSecs = [math]::Max($cwWin / 1000.0, 0.001)
    Write-Host ""
    Write-Host ("COMMITWORK -- Commit cost by how many textures it had to realize (over {0:N0}s):" -f $cwSecs)
    Write-Host ("  {0,-16} {1,8}  {2,9}  {3,9}  {4,8}  {5,8}  {6,8}" -f "new textures", "commits", "avg ms", "worst ms", "vis/cmt", "bind/cmt", "reuse")
    $firstAvg = $null
    $lastAvg = $null
    $newN = 0.0
    $newMs = 0.0
    foreach ($k in @($workTags.Keys)) {
        if (-not $bucket.ContainsKey($k)) { continue }
        $b = $bucket[$k]
        if ($b[0] -eq 0) { continue }
        $avg = $b[1] / $b[0]
        # bind already counts the reused binds, so the denominator is bind, not bind+same.
        # Dividing by the sum printed 50% for the bucket whose definition is "every texture
        # came back", i.e. 100% -- a column that contradicts its own row label.
        $reuse = if ($b[4] -gt 0) { 100.0 * $b[5] / $b[4] } else { 0.0 }
        Write-Host ("  {0,-16} {1,8:N0}  {2,9:N3}  {3,9:N2}  {4,8:N2}  {5,8:N2}  {6,7:N0}%" -f `
            $workTags[$k], $b[0], $avg, $b[2], ($b[3] / $b[0]), ($b[4] / $b[0]), $reuse)
        if ($k -eq "idle") { continue }   # listed, never the baseline -- see above
        if ($k -eq "reuse") { $firstAvg = $avg; continue }
        # Weighted by commits, NOT the last bucket's average. Taking the last one compared the
        # baseline against whatever bucket happened to be non-empty: on the 2026-10-20 clip=none
        # run that was new5p with ONE commit, so the table printed "flat, 1.8x" and told the
        # reader neither fix would work -- while 12,919 commits sat in new1 at 5.3x the
        # baseline. A verdict that a single sample can flip is not a verdict.
        $newN += $b[0]
        $newMs += $b[1]
    }
    $lastAvg = if ($newN -gt 0) { $newMs / $newN } else { $null }
    if ($null -eq $firstAvg) {
        # No commit in the whole run carried work whose textures all came back. Falling through
        # to "new1 vs new5p" here would print a ratio between two kinds of churn and call it a
        # verdict, which is how a measurement starts lying.
        Write-Host "  No commit carried work with every texture reused, so there is no baseline to"
        Write-Host "  compare against and this table cannot be read as a verdict. That DComp never"
        Write-Host "  hands the same texture back is itself the finding -- check tex_reused above."
    }
    elseif ($firstAvg -gt 0 -and $null -ne $lastAvg) {
        $ratio = $lastAvg / $firstAvg
        if ($ratio -ge 2.0) {
            Write-Host ("  A Commit that had to realize a new texture costs {0:N1}x one whose textures all came back" -f $ratio)
            Write-Host ("  ({1:N0} commits against {2:N0}). That is the cost sitting in texture realization, so holding" -f $ratio, $newN, $bucket["reuse"][0])
            Write-Host "  the update rect still -- one texture size instead of many -- is the lever."
        } else {
            Write-Host ("  Cost is flat across the buckets ({0:N1}x end to end)." -f $ratio)
            Write-Host "  Then texture churn is a passenger, not the cause, and neither pbuffer reuse nor update-rect"
            Write-Host "  quantisation will fix the stall. Look outside this compositor."
        }
    }
} elseif ($DcompBindProf) {
    Write-Warning "-DcompBindProf was set but no COMMITWORK line was logged. It is emitted right after COMMITSITE by the same compositor; if COMMITSITE printed and this did not, the engine predates this table."
}

# --- FRAMEID: how many renders were covered by a wall frame request? ---
# ***This is a barrier-coverage ratio, not a content-freshness ratio.*** Read
# components/paint/output_grid.rs:292-305 before drawing any conclusion from it.
#
# A wall logical frame id is attached only to composites that came from a wall frame request
# (`Painter::generate_frame_for_script`), and `last_ready_wall_logical_frame_id` is sticky
# once set. So `distinct_ids` counts how many DIFFERENT wall frames completed in that second
# -- effectively the script path's own rate -- while `renders` counts every composite from
# every source. A low ratio means most renders went out OUTSIDE any wall frame barrier, so the
# four tiles were not held to the same frame for those composites.
#
# It does NOT mean the picture repeated. Display lists are published in their own transaction
# (painter.rs:3761-3770), separate from `generate_frame`, so a composite requested by the video
# path still picks up script's newest scene.
#
# Measured 2026-10-21 (log_ani_debug_05/13, SINGLE tile, one rotating region):
#   rotating a still image   distinct/renders = 0.99-1.00
#   rotating a 30fps video   distinct/renders = 0.54
# On a single tile the barrier never engages at all (paint.rs:1774 short-circuits at
# target_painter_ids.len() <= 1), so there the ratio is only telling you what fraction of
# composites the script path asked for.
$fid = Select-String -Path $LogPath -Pattern "FRAMEID painter=" -EA SilentlyContinue
if ($fid) {
    $fidByPainter = @{}
    foreach ($m in $fid) {
        if ($m.Line -notmatch "painter=PainterId\((\d+)\) renders=(\d+) with_id=(\d+) distinct_ids=(\d+) without_id=(\d+)") { continue }
        $p = [int]$Matches[1]
        if (-not $fidByPainter.ContainsKey($p)) { $fidByPainter[$p] = @(0, 0, 0, 0) }  # windows, renders, distinct, without_id
        $fidByPainter[$p][0] += 1
        $fidByPainter[$p][1] += [int]$Matches[2]
        $fidByPainter[$p][2] += [int]$Matches[4]
        $fidByPainter[$p][3] += [int]$Matches[5]
    }
    if ($fidByPainter.Count -gt 0) {
        Write-Host ""
        # ★커밋 전 작업분(2026-10-21), 무효 시점 불명 — 같은 블록 머리(:1980)가 이미 반박한다★ — 배리어 커버리지 비율이지 "새 내용" 비율이 아니다.
        Write-Host "FRAMEID -- of the composites that went out, how many carried new content:"
        $worstRatio = 1.0
        foreach ($p in ($fidByPainter.Keys | Sort-Object)) {
            $t = $fidByPainter[$p]
            if ($t[0] -eq 0 -or $t[1] -eq 0) { continue }
            $ratio = [double]$t[2] / [double]$t[1]
            if ($ratio -lt $worstRatio) { $worstRatio = $ratio }
            Write-Host ("  painter {0}   {1,6:N1} composites/s   {2,6:N1} distinct/s   ratio {3:N3}{4}" -f `
                $p, ($t[1] / $t[0]), ($t[2] / $t[0]), $ratio, $(if ($t[3] -gt 0) { "   ({0:N0} had no id)" -f $t[3] } else { "" }))
        }
        if ($worstRatio -ge 0.95) {
            Write-Host "  Nearly every render was covered by a wall frame request, so the tiles were held to"
            Write-Host "  the same frame. This says nothing about whether the content changed."
        } else {
            Write-Warning ("Only {0:N0}% of renders carried a wall frame id, so most composites went out outside any wall frame barrier -- on a multi-tile wall the tiles were not held to the same frame for those. Read FRAMEREASON above to see which other source asked for them. This is NOT evidence that the picture repeated; display lists publish separately from generate_frame." -f ($worstRatio * 100))
        }
    }
}

# --- FRAMETIME: is the timestamp the PAGE receives evenly spaced? ---
# Every other metric in this script counts frames. This one counts the VALUES those frames
# carried. `frame_time` is handed to the script thread and becomes the page's
# requestAnimationFrame timestamp, so the page computes its own position from this sequence:
# 60 frames a second with an uneven sequence still draws an uneven animation, and no frame
# counter anywhere shows it.
#
# Read `repeated` first. It means two consecutive ticks carried the same time, so the page
# computed the same position twice and the screen stood still for that frame -- the one
# failure mode that looks exactly like "the animation is not 60fps" while every frame counter
# reads 60.
$ft = Select-String -Path $LogPath -Pattern "FRAMETIME painter=" -EA SilentlyContinue
if ($ft) {
    $ftWindows = 0; $ftN = 0; $ftDup = 0; $ftOne = 0; $ftSkip1 = 0; $ftSkipN = 0
    $ftFallback = 0; $ftDtMax = 0.0; $ftLagMax = 0.0; $ftP50 = @()
    foreach ($m in $ft) {
        if ($m.Line -notmatch "n=(\d+) per_interval dup=(\d+) one=(\d+) skip1=(\d+) skipN=(\d+) dt_ms p05=([-\d.]+) p50=([-\d.]+) p95=([-\d.]+) max=([-\d.]+) lag_ms p50=([-\d.]+) p95=([-\d.]+) max=([-\d.]+) fallback=(\d+)") { continue }
        $ftWindows++
        $ftN        += [int]$Matches[1]
        $ftDup      += [int]$Matches[2]
        $ftOne      += [int]$Matches[3]
        $ftSkip1    += [int]$Matches[4]
        $ftSkipN    += [int]$Matches[5]
        $ftP50      += [double]$Matches[7]
        $ftDtMax     = [math]::Max($ftDtMax,  [double]$Matches[9])
        $ftLagMax    = [math]::Max($ftLagMax, [double]$Matches[12])
        $ftFallback += [int]$Matches[13]
    }
    if ($ftN -gt 0) {
        $sorted = $ftP50 | Sort-Object
        $median = $sorted[[int]([math]::Floor($sorted.Count / 2))]
        $pct = { param($k) 100.0 * $k / $ftN }
        Write-Host ""
        Write-Host ("FRAMETIME -- the rAF timestamp the page received ({0:N0} windows, {1:N0} intervals):" -f $ftWindows, $ftN)
        Write-Host ("  one period apart     {0,8:N0}  {1,5:N1}%   <- what a clean run is made of" -f $ftOne, (& $pct $ftOne))
        Write-Host ("  repeated             {0,8:N0}  {1,5:N1}%   <- page computed the same position twice" -f $ftDup, (& $pct $ftDup))
        Write-Host ("  one period skipped   {0,8:N0}  {1,5:N1}%" -f $ftSkip1, (& $pct $ftSkip1))
        Write-Host ("  more skipped         {0,8:N0}  {1,5:N1}%" -f $ftSkipN, (& $pct $ftSkipN))
        Write-Host ("  median interval      {0,8:N2} ms  (60Hz = 16.67)" -f $median)
        Write-Host ("  worst gap            {0,8:N2} ms" -f $ftDtMax)
        Write-Host ("  worst lag behind tick{0,8:N2} ms  (how old the value was when it went out)" -f $ftLagMax)
        Write-Host ("  fell back to now()   {0,8:N0}  {1,5:N1}%   <- shell value missing or stale" -f $ftFallback, (& $pct $ftFallback))
        # 비율로 판정한다. 7,100 구간 중 4 개가 어긋난 것은 기동과 장면 전환의 흔적이지
        # 저더가 아니다 -- 숫자는 그대로 위에 찍어 두고, 경고는 화면에서 보일 만한 크기일
        # 때만 낸다(2026-10-21: 처음 판정식이 깨끗한 실행 여섯 개 전부에 경고를 띄웠다).
        $uneven = $ftDup + $ftSkip1 + $ftSkipN
        $unevenPct = & $pct $uneven
        if ($unevenPct -lt 1.0) {
            if ($uneven -eq 0) {
                Write-Host "  Every interval was one period on the way out."
            } else {
                Write-Host ("  {0:N0} intervals ({1:N2}%) were not one period -- startup and scene changes leave that" -f $uneven, $unevenPct)
                Write-Host "  much behind. Too small to be a visible judder; the clock left paint even."
            }
            Write-Host "  Read RAFCLOCK below before concluding anything: the script thread can still lose or"
            Write-Host "  clamp these values, and only that line shows what the page was actually handed."
        } else {
            Write-Warning ("{0:N0} of {1:N0} intervals ({2:N1}%) were not one period -- too many to be startup noise. The page was handed an uneven clock, so it computed uneven positions -- the animation cannot be smooth no matter how many frames go out. This is already uneven on the way out of paint, so the pacing is the first suspect -- RAFCLOCK below can only make it worse, never better." -f $uneven, $ftN, (& $pct $uneven))
        }
    }
} elseif ($FrameTimeProf) {
    Write-Warning "-FrameTimeProf was set but no FRAMETIME line was logged. It emits from frame_time_for_tick, which only runs while something on the page is animating -- a static page produces no ticks and no line."
}

# --- RAFCLOCK: the other end of the same sequence ---
# FRAMETIME is what paint SENT. This is what the page RECEIVED: the timestamp handed to its
# requestAnimationFrame callbacks, after the script thread consumed the tick and
# Document::resolve_frame_time applied its monotonic clamp.
#
# `clamped` is the one to read. It counts rounds where the candidate was older than the last
# value the page saw, so the page was handed the SAME timestamp twice and computed the same
# position twice. A round that could not use a tick falls back to now(), which jumps the clock
# forward, and every paced (therefore earlier) tick after it gets clamped -- 60 frames out, a
# screen that stands still.
$rc = Select-String -Path $LogPath -Pattern "RAFCLOCK pipeline=" -EA SilentlyContinue
if ($rc) {
    $rcWindows = 0; $rcN = 0; $rcDup = 0; $rcOne = 0; $rcSkip1 = 0; $rcSkipN = 0
    $rcFallback = 0; $rcClamped = 0; $rcMax = 0.0; $rcP50 = @()
    foreach ($m in $rc) {
        if ($m.Line -notmatch "n=(\d+) per_interval dup=(\d+) one=(\d+) skip1=(\d+) skipN=(\d+) dt_ms p05=([-\d.]+) p50=([-\d.]+) p95=([-\d.]+) max=([-\d.]+) fallback=(\d+) clamped=(\d+)") { continue }
        $rcWindows++
        $rcN        += [int]$Matches[1]
        $rcDup      += [int]$Matches[2]
        $rcOne      += [int]$Matches[3]
        $rcSkip1    += [int]$Matches[4]
        $rcSkipN    += [int]$Matches[5]
        $rcP50      += [double]$Matches[7]
        $rcMax       = [math]::Max($rcMax, [double]$Matches[9])
        $rcFallback += [int]$Matches[10]
        $rcClamped  += [int]$Matches[11]
    }
    if ($rcN -gt 0) {
        $rcSorted = $rcP50 | Sort-Object
        $rcMedian = $rcSorted[[int]([math]::Floor($rcSorted.Count / 2))]
        $rcPct = { param($k) 100.0 * $k / $rcN }
        Write-Host ""
        Write-Host ("RAFCLOCK -- the timestamp the page's rAF callback received ({0:N0} windows, {1:N0} intervals):" -f $rcWindows, $rcN)
        Write-Host ("  one period apart     {0,8:N0}  {1,5:N1}%" -f $rcOne, (& $rcPct $rcOne))
        Write-Host ("  repeated             {0,8:N0}  {1,5:N1}%   <- page computed the same position twice" -f $rcDup, (& $rcPct $rcDup))
        Write-Host ("  one period skipped   {0,8:N0}  {1,5:N1}%" -f $rcSkip1, (& $rcPct $rcSkip1))
        Write-Host ("  more skipped         {0,8:N0}  {1,5:N1}%" -f $rcSkipN, (& $rcPct $rcSkipN))
        Write-Host ("  median interval      {0,8:N2} ms  (60Hz = 16.67)" -f $rcMedian)
        Write-Host ("  worst gap            {0,8:N2} ms" -f $rcMax)
        Write-Host ("  monotonic clamp hit  {0,8:N0}  {1,5:N1}%   <- handed the previous value again" -f $rcClamped, (& $rcPct $rcClamped))
        Write-Host ("  fell back to now()   {0,8:N0}  {1,5:N1}%   <- the round had no tick to use" -f $rcFallback, (& $rcPct $rcFallback))
        # 같은 이유로 비율 판정이다. `clamped`/`fallback` 은 한 번만 나도 메커니즘으로는
        # 흥미롭지만, 7,100 회차 중 몇 번이면 화면에서는 보이지 않는다. 그 둘을 합쳐
        # 1% 를 넘을 때만 "우리 쪽 결함" 이라고 말한다.
        $rcUneven = $rcDup + $rcSkip1 + $rcSkipN
        $rcUnevenPct = & $rcPct $rcUneven
        $rcDistorted = $rcClamped + $rcFallback
        $rcDistortedPct = & $rcPct $rcDistorted
        if ($rcDistortedPct -ge 1.0) {
            Write-Warning ("The page's clock was distorted on the script side: {0:N0} rounds fell back to now() and {1:N0} were clamped back to the previous value ({2:N1}% together). That is ours, in Document::resolve_frame_time -- a round that cannot use a tick takes now(), which is LATER than the paced tick, so the clock jumps forward and the real ticks after it get clamped to the previous value. The page then computes the same position twice." -f $rcFallback, $rcClamped, $rcDistortedPct)
        } elseif ($rcUnevenPct -lt 1.0) {
            if ($rcUneven -eq 0 -and $rcDistorted -eq 0) {
                Write-Host "  Even, end to end."
            } else {
                Write-Host ("  {0:N0} uneven intervals ({1:N2}%), {2:N0} clamped, {3:N0} fell back -- all of it startup" -f $rcUneven, $rcUnevenPct, $rcClamped, $rcFallback)
                Write-Host "  and scene-change noise at this size, not a visible judder."
            }
            Write-Host "  ★This clears the whole clock path.★ The page was handed one period per frame, so an"
            Write-Host "  animation that still looks uneven is about WHEN the frame carrying each value reached"
            Write-Host "  the screen -- not about the value it carried. Stop looking at the clock and look at"
            Write-Host "  FRAMEREASON (who caused each composite) and the present cadence."
        } else {
            Write-Warning ("{0:N0} of {1:N0} intervals ({2:N1}%) were not one period, with no clamp and no fallback. Compare with FRAMETIME above: if that one is even, the ticks were lost between paint and the page (pending_frame_time is a single Cell and a second tick overwrites the first); if it is uneven too, the problem is upstream in the pacing." -f $rcUneven, $rcN, (& $rcPct $rcUneven))
        }
    }
} elseif ($FrameTimeProf) {
    Write-Warning "-FrameTimeProf was set but no RAFCLOCK line was logged. It emits from Document::resolve_frame_time, which runs on every rendering update -- if FRAMETIME printed and this did not, the ticks are not reaching the script thread at all."
}


# --- COMPREFRESH: 저더를 직접 센 수 (needs -DcompBindProf) ---
# ★이 저장소에서 저더를 ***직접*** 세는 유일한 계수기다.★ `refresh_d` 는 합성 프레임 하나가
# 지나는 동안 그 패널의 `refreshCount` 가 몇 올랐나의 분포이고, `[2]` 이상은 그 패널이 한
# 합성에 리프레시를 둘 이상 썼다는 뜻 -- ***같은 그림을 두 번 보여 줬다***(`output_grid.rs`
# 의 `CompStats::refresh_delta` 주석). fps 도, 패스 길이도, TICKCOMMIT 도 이것과 따로 논다:
# 2026-10-22 의 두 실행은 fps 61.6 / t2c p50 15.9 / ANIMSTEP dt p50 16.8 로 ***구분되지
# 않는데*** 이 수는 4 와 730 이었다.
#
# ★비율만 보지 말고 `w/dup` 을 같이 볼 것.★ 같은 총계라도 전 구간에 고르게 퍼진 것과 몇 초에
# 몰린 것은 원인이 다르다 -- 고르면 기동 시 굳은 상태이고(실측: 10 초 버킷마다 20 건씩
# 120 초), 몰리면 지나가는 사건이다(실측: 121 창 중 6 창).
#
# 실측 기준선(같은 장비·같은 프로브·120 초, 2026-10-22):
#   engine 2026-10-16  0.010 ~ 0.090%   (5/5 런, 핀 없이 group 0·1 양쪽)
#   engine 2026-10-22  0.109 ~ 3.50%    (6/7 런)
# ★COMPREFRESH 는 이 벽이 ***안 쓰는 출력까지*** 센다.★ 프로브 스레드가 열거된 전 출력을
# 훑기 때문이다(`output_grid.rs` 의 `emit_comp_stats(&current.outputs, ..)`). 타일 하나짜리
# 레이아웃에서는 표의 3/4 가 바탕화면만 띄운 유휴 패널이고, ***유휴 출력은 정상적으로***
# `refresh_d[2]` 가 높다 -- 바뀐 게 없으니 DWM 이 합성을 건너뛰고 그동안 패널은 계속
# 리프레시한다. 실측(2026-10-22, 1 타일 런): 구동 안 하는 DISPLAY22 가 모든 arm 에서
# 2.4~2.9% 로 나왔고, 육안으로 저더가 전혀 없던 런에서도 그랬다.
#
# 그래서 ★페인터가 찍는 줄로 구동 집합을 뽑아 걸러낸다★ -- `DCOMPSTAT` 는
# `Painter::note_dcomp_frame_statistics` 가 자기 타일의 모니터로만 부르므로 구동 중인
# 출력에만 뜬다(`SAMPLELEAD` 도 같지만 그쪽은 -SampleLead 가 켜져야 난다).
$crDriven = @{}
foreach ($m in @(Select-String -Path $LogPath -Pattern "DCOMPSTAT out=(\S+)" -EA SilentlyContinue)) {
    if ($m.Line -match "DCOMPSTAT out=(\S+)") { $crDriven[$Matches[1]] = $true }
}
$cr = @(Select-String -Path $LogPath -Pattern "COMPREFRESH out=" -EA SilentlyContinue)
if ($cr.Count -gt 0) {
    $crRows = @{}
    $crOrder = @()
    foreach ($m in $cr) {
        if ($m.Line -notmatch "COMPREFRESH out=(\S+) luid=(\S+).*? frames=(\d+) refresh_d=\[(\d+),(\d+),(\d+),(\d+)\]") { continue }
        $key = $Matches[1]
        if (-not $crRows.ContainsKey($key)) {
            $crRows[$key] = @{ Luid = $Matches[2]; Frames = 0; Dup = 0; D0 = 0; Windows = 0; WithDup = 0; Worst = 0 }
            $crOrder += $key
        }
        $r = $crRows[$key]
        # d2 와 d3+ 를 합친다 -- 둘 다 "같은 그림을 두 번 이상" 이고, 가르면 드문 d3+ 가 묻힌다.
        $dup = [int]$Matches[6] + [int]$Matches[7]
        $r.Frames  = $r.Frames + [int]$Matches[3]
        $r.D0      = $r.D0 + [int]$Matches[4]
        $r.Dup     = $r.Dup + $dup
        $r.Windows = $r.Windows + 1
        if ($dup -gt 0) { $r.WithDup = $r.WithDup + 1 }
        if ($dup -gt $r.Worst) { $r.Worst = $dup }
    }
    if ($crOrder.Count -gt 0) {
        Write-Host ""
        Write-Host "COMPREFRESH -- the judder counter: a panel that showed the same picture twice"
        Write-Host ("  {0,-14} {1,-19} {2,8} {3,7} {4,8} {5,7} {6,8} {7,8}" -f "output", "luid", "frames", "dup", "dup%", "worst", "windows", "w/dup")
        $crWorstPct = 0.0
        $crTotalDup = 0
        $crWindows = 0
        $crWithDup = 0
        $crIdle = 0
        foreach ($key in $crOrder) {
            $r = $crRows[$key]
            $pct = 0.0
            if ($r.Frames -gt 0) { $pct = 100.0 * $r.Dup / $r.Frames }
            # ★구동 집합을 못 구했으면(DCOMPSTAT 없음) 예전처럼 전부 센다★ -- 걸러낼 근거가
            # 없는데 거르면 조용히 틀린 판정을 내게 된다.
            $driven = ($crDriven.Count -eq 0) -or $crDriven.ContainsKey($key)
            $tag = ""
            if (-not $driven) { $tag = "   <- idle, not driven by this run (excluded)"; $crIdle = $crIdle + 1 }
            if ($driven) {
                $crWorstPct = [math]::Max($crWorstPct, $pct)
                $crTotalDup = $crTotalDup + $r.Dup
                $crWindows  = [math]::Max($crWindows, $r.Windows)
                $crWithDup  = [math]::Max($crWithDup, $r.WithDup)
            }
            Write-Host ("  {0,-14} {1,-19} {2,8:N0} {3,7:N0} {4,7:N3}% {5,7:N0} {6,8:N0} {7,8:N0}{8}" -f `
                $key, $r.Luid, $r.Frames, $r.Dup, $pct, $r.Worst, $r.Windows, $r.WithDup, $tag)
        }
        if ($crIdle -gt 0) {
            Write-Host ("  {0} of {1} outputs were not driven by this layout and are excluded from the verdict." -f $crIdle, $crOrder.Count)
            Write-Host "  An idle panel legitimately shows a high dup%: nothing changed, so DWM skipped compositions"
            Write-Host "  while the panel kept refreshing. Do not read those rows as judder."
        } elseif ($crDriven.Count -eq 0) {
            Write-Host "  (No DCOMPSTAT line, so the driven-output set is unknown -- every output is counted. If this"
            Write-Host "   layout drives fewer panels than the machine has, the extra rows are idle, not judder.)"
        }
        # 창 중 몇 %에 결함이 있었나. 고르게 퍼졌는지 몰렸는지를 이 하나로 가른다.
        $crSpreadPct = 0.0
        if ($crWindows -gt 0) { $crSpreadPct = 100.0 * $crWithDup / $crWindows }
        $shape = "episodic"
        if ($crSpreadPct -ge 50.0) { $shape = "CONTINUOUS" }
        Write-Host ("  shape: {0} -- the worst panel had a defect in {1:N0} of {2:N0} one-second windows ({3:N0}%)" -f $shape, $crWithDup, $crWindows, $crSpreadPct)
        if ($crWorstPct -lt 0.10) {
            Write-Host "  ★Clean.★ This is inside the 2026-10-16 engine's measured band (0.010-0.090%) on the same"
            Write-Host "  wall. The panels showed one new picture per composition -- whatever else a run shows,"
            Write-Host "  the wall was not stuttering."
        } elseif ($crWorstPct -lt 0.50) {
            Write-Warning ("Marginal: worst panel {0:N3}% ({1:N0} duplicated frames across all panels), above the 0.010-0.090% band the 2026-10-16 engine held on this wall. {2}. Repeat the run before reading anything into it -- the same command has measured 0.02% and 2.23% on consecutive runs, so a single run does not separate arms." -f $crWorstPct, $crTotalDup, $(if ($shape -eq "CONTINUOUS") { "Spread across the whole run, which points at a state fixed when the process started" } else { "Concentrated in a few windows, so look for a passing event rather than a startup state" }))
        } else {
            Write-Warning ("Judder: worst panel {0:N3}% ({1:N0} duplicated frames across all panels) -- 5x or more above the 2026-10-16 band on this wall. {2}. fps and pass_ms will look normal; they do not measure this." -f $crWorstPct, $crTotalDup, $(if ($shape -eq "CONTINUOUS") { "It ran the whole 120s, which is the signature of a state fixed at process start -- re-run and see whether it flips, before blaming the last code change" } else { "It came in bursts, so look for what else happened at those moments" }))
        }
    }
} elseif ($DcompBindProf) {
    Write-Warning "-DcompBindProf was set but no COMPREFRESH line was logged. It comes from the output-grid probe thread beside OUTPHASE; if those printed and this did not, DComp statistics were unavailable for every target."
}

# --- PAINTERLOCK: 틱이 타일 렌더 뒤에 서는가 (needs -PainterLockProf) ---
# 표출 틱이 단일 프레임 생산자가 된 뒤로, 틱은 렌더를 시작하기 ***전에*** painter 마다 세 번
# 블로킹으로 닿는다(gather_components / issue_wall_frame_request / release_owed_canvas_acks).
# 이 줄은 그 왕복이 타일 스레드의 큐에서 기다린 시간만 재고, 콜백 실행 시간은 빼 둔다.
#
# ★이것이 재는 것은 "틱 자신의 클럭" 이다.★ -DwmAlign 은 틱 에지를 DWM 합성 격자에 박지만
# 패스 시작은 틱 + 이 대기이고, -CommitAlign 의 마감은 틱에서부터 세므로 패스에 남는 예산도
# 그만큼 줄어든다. 즉 이 값이 크면 두 정렬 플래그는 ***멀쩡한 채로 효과만 사라진다.***
# 두 블록이 같이 쓰는 중앙값 도우미. ★PAINTERLOCK 이 안 찍힌 실행에서도 TICKCOMMIT 이
# 돌아야 하므로 블록 밖에 둔다.★
$medianOf = {
    param($values)
    $sorted = @($values | Sort-Object)
    if ($sorted.Count -eq 0) { return 0.0 }
    return $sorted[[int]([math]::Floor($sorted.Count / 2))]
}
$pl = @(Select-String -Path $LogPath -Pattern "PAINTERLOCK painter=" -EA SilentlyContinue)
if ($pl.Count -gt 0) {
    # ★PS 5.1 에서 결과가 하나면 .Count 가 비는 것을 피하려고 전부 @() 로 감싼다.★
    $plRows = @{}
    $plOrder = @()
    foreach ($m in $pl) {
        if ($m.Line -notmatch "PAINTERLOCK painter=(\S+) n=(\d+) wait_ms p50=([-\d.]+) p95=([-\d.]+) max=([-\d.]+)\s+over_1ms=(\d+)") { continue }
        $key = $Matches[1]
        if (-not $plRows.ContainsKey($key)) {
            $plRows[$key] = @{ Windows = 0; N = 0; P50 = @(); P95 = @(); Max = 0.0; Over = 0 }
            $plOrder += $key
        }
        $r = $plRows[$key]
        $r.Windows = $r.Windows + 1
        $r.N       = $r.N + [int]$Matches[2]
        $r.P50     = $r.P50 + [double]$Matches[3]
        $r.P95     = $r.P95 + [double]$Matches[4]
        $r.Max     = [math]::Max($r.Max, [double]$Matches[5])
        $r.Over    = $r.Over + [int]$Matches[6]
    }
    if ($plOrder.Count -gt 0) {
        Write-Host ""
        Write-Host "PAINTERLOCK -- how long the tick waited in the painter thread queues:"
        Write-Host ("  {0,-14} {1,8} {2,9} {3,10} {4,10} {5,12} {6,10} {7,10} {8,9}" -f "painter", "windows", "calls", "p50(med)", "p95(med)", "p95(worst)", "max", "over_1ms/s", "over_1ms%")
        $plVerdictP95 = 0.0
        $plOverPerSec = 0.0
        $plOverPct = 0.0
        $plWorstMax = 0.0
        foreach ($key in $plOrder) {
            $r = $plRows[$key]
            $p50med = & $medianOf $r.P50
            $p95med = & $medianOf $r.P95
            $p95max = 0.0
            foreach ($v in $r.P95) { $p95max = [math]::Max($p95max, $v) }
            $overRate = 0.0
            if ($r.Windows -gt 0) { $overRate = $r.Over / $r.Windows }
            $overPct = 0.0
            if ($r.N -gt 0) { $overPct = 100.0 * $r.Over / $r.N }
            $plVerdictP95 = [math]::Max($plVerdictP95, $p95med)
            $plOverPerSec = [math]::Max($plOverPerSec, $overRate)
            $plOverPct    = [math]::Max($plOverPct, $overPct)
            $plWorstMax   = [math]::Max($plWorstMax, $r.Max)
            Write-Host ("  {0,-14} {1,8:N0} {2,9:N0} {3,10:N2} {4,10:N2} {5,12:N2} {6,10:N2} {7,10:N1} {8,8:N1}%" -f `
                $key, $r.Windows, $r.N, $p50med, $p95med, $p95max, $r.Max, $overRate, $overPct)
        }
        # 판정선은 설계가 적어 둔 그대로 `p95 < 1ms` 다(`SERVO_WALL_PAINTER_LOCK_PROF` 의 doc).
        # ★`max` 하나로 판정하지 않는다★ -- "3ms 한 번" 과 "3ms 를 60 번" 은 결론이 정반대이고,
        # 그 둘을 가르는 것이 over_1ms 다. 그래서 p95 를 1 차 판정으로 쓰고, p95 가 깨끗한데도
        # 꼬리가 자주 무는 경우만 따로 말한다.
        #
        # ★초당 횟수로 판정하지 않는다★ -- 초당 180 회 호출에서 1 회는 0.6% 이고, 그것을
        # 결함이라고 부르면 깨끗한 실행이 전부 경고로 찍혀 판정이 쓸모없어진다. 그래서 꼬리는
        # ***호출 수에 대한 비율***로 본다.
        if ($plVerdictP95 -ge 1.0) {
            Write-Warning ("The tick is queueing behind tile renders: p95(med) up to {0:N2}ms, with {1:N1}% of calls waiting over 1ms. The tick makes three blocking round trips per painter before the render pass starts, so a 4-tile wall pays twelve of them. -DwmAlign pins only the tick edge, so the pass start slides by that much, and -CommitAlign measures its deadline from the tick, so the pass budget shrinks by the same amount. Move these round trips to a non-blocking post, keeping production single-sourced." -f $plVerdictP95, $plOverPct)
        } elseif ($plOverPct -ge 5.0) {
            Write-Warning ("p95(med) is {0:N2}ms, so the typical tick does not wait -- but {1:N1}% of calls ({2:N1}/s) still blocked for over 1ms, worst {3:N2}ms. That tail is a tick that occasionally lands behind a tile render, which shows up as an occasional late pass rather than a steady phase shift. Read it beside TICKCOMMIT spread below: if the spread moves with it, this tail is the judder." -f $plVerdictP95, $plOverPct, $plOverPerSec, $plWorstMax)
        } else {
            Write-Host "  ★The blocking call is fine.★ The tick does not wait on the painters, so whatever sits"
            Write-Host "  between the tick edge and the start of the render pass is not this. If judder remains, read"
            Write-Host "  TICKCOMMIT below and the WALLPASS lines -- the prologue cost would be in the pass, not here."
        }
    }
} elseif ($PainterLockProf) {
    Write-Warning "-PainterLockProf was set but no PAINTERLOCK line was logged. It only measures threaded painters -- a run where every tile painter is inline (no tile threads) has nothing to wait on and prints nothing."
}

# --- TICKCOMMIT: 틱에서 커밋까지 (needs -DcompBindProf) ---
# ★이 값의 뜻이 2026-10-22 에 바뀌었다★ -- 전에는 "패스 길이" 였지만, 지금은 틱과 렌더
# 사이에 생산 프롤로그가 들어가므로 ***프롤로그 + 패스***다. p50 이 통째로 올라갔으면 그
# 프롤로그이고, p95-p05(spread)가 벌어졌으면 그것이 커밋 위상의 지터다.
#
# `late` 는 커밋이 반 주기보다 늦게 나간 횟수다 -- 그 프레임은 겨냥한 위상에서 통째로
# 벗어났다. PAINTERLOCK 과 ★같은 실행에서★ 읽어야 둘의 인과가 갈린다.
$tc = @(Select-String -Path $LogPath -Pattern "TICKCOMMIT device=" -EA SilentlyContinue)
if ($tc.Count -gt 0) {
    $tcRows = @{}
    $tcOrder = @()
    foreach ($m in $tc) {
        if ($m.Line -notmatch "TICKCOMMIT device=(\S+) n=(\d+) t2c_ms p05=([-\d.]+) p50=([-\d.]+) p95=([-\d.]+) max=([-\d.]+) spread=([-\d.]+) late=(-?\d+)") { continue }
        $key = $Matches[1]
        if (-not $tcRows.ContainsKey($key)) {
            $tcRows[$key] = @{ Windows = 0; N = 0; P50 = @(); Spread = @(); Max = 0.0; Late = 0 }
            $tcOrder += $key
        }
        $r = $tcRows[$key]
        $r.Windows = $r.Windows + 1
        $r.N       = $r.N + [int]$Matches[2]
        $r.P50     = $r.P50 + [double]$Matches[4]
        $r.Spread  = $r.Spread + [double]$Matches[7]
        $r.Max     = [math]::Max($r.Max, [double]$Matches[6])
        # late=-1 은 "DWM 주기를 못 구했다" 는 뜻이지 0 건이 아니다. 더하면 거짓말이 된다.
        if ([int]$Matches[8] -gt 0) { $r.Late = $r.Late + [int]$Matches[8] }
    }
    if ($tcOrder.Count -gt 0) {
        # ★마감이 주기의 어디인지 먼저 적는다.★ 마감은 틱 ***다음***의 첫 CommitAlign% 격자점
        # 이므로, CommitAlign 이 DwmAlign 보다 앞서면 여유는 거의 한 주기다
        # (`output_grid::commit_slack_pct`). 8/14 면 94% 라 마감이 다음 틱 1ms 앞이고,
        # ★그러면 t2c 가 반 주기를 넘는 것이 정상이다.★
        $tcSlack = 100
        if ($CommitAlign -ge 0 -and $DwmAlign -ge 0 -and $CommitAlign -ne $DwmAlign) {
            $tcSlack = ($CommitAlign + 100 - $DwmAlign) % 100
        }
        $tcPeriod = 0.0
        if ($RefreshHz -gt 0) { $tcPeriod = 1000.0 / $RefreshHz }
        $tcTarget = $tcPeriod * $tcSlack / 100.0
        Write-Host ""
        Write-Host "TICKCOMMIT -- tick edge to DComp Commit (production prologue + pass):"
        Write-Host ("  {0,-16} {1,8} {2,13} {3,13} {4,10}" -f "device", "windows", "t2c p50(med)", "spread(med)", "max")
        $tcWorstSpread = 0.0
        foreach ($key in $tcOrder) {
            $r = $tcRows[$key]
            $p50med = & $medianOf $r.P50
            $spmed  = & $medianOf $r.Spread
            $tcWorstSpread = [math]::Max($tcWorstSpread, $spmed)
            Write-Host ("  {0,-16} {1,8:N0} {2,13:N2} {3,13:N2} {4,10:N2}" -f $key, $r.Windows, $p50med, $spmed, $r.Max)
        }
        if ($tcTarget -gt 0) {
            Write-Host ("  deadline: -CommitAlign {0} after -DwmAlign {1} is {2}% of a period = {3:N2} ms after the tick." -f $CommitAlign, $DwmAlign, $tcSlack, $tcTarget)
            Write-Host ("  t2c p50 should sit just under that. The number to watch is spread(med), not p50.")
        }
        # ★`late` 를 안 찍는 이유.★ 엔진은 "반 주기보다 늦게 나간 커밋" 을 세는데, 여기처럼
        # 마감이 94% 에 있으면 ***정렬이 완벽할수록 전부 late 로 찍힌다***. 실측 양쪽 다
        # late == n 이었다. 그 수를 그대로 보여 주면 읽는 사람이 결함으로 읽는다.
        Write-Host '  (The engine TICKCOMMIT late counter is omitted: it counts commits past HALF a period,'
        Write-Host '   which every correctly aligned commit is when the deadline sits at 94%. It read late==n in every run.)'
        Write-Host "  Do not compare this against a pre-2026-10-21 log: those runs had no prologue in t2c."
    }
} elseif ($DcompBindProf) {
    Write-Warning "-DcompBindProf was set but no TICKCOMMIT line was logged. It is emitted from the output-grid probe thread beside OUTPHASE/SAMPLESLIP; if those printed and this did not, no commit carried a tick stamp (the shell never called servo::note_present_tick)."
}
if ($fanout -gt 0) { Write-Warning "GPU fan-out is broken -- tiles share one D3D11 device. See the warning text in the log." }
if ($egl -gt 0)    { Write-Warning "Some tiles could not wrap video textures ($egl occurrences) -- those tiles show green." }
exit 0

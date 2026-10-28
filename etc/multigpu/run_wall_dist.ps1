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
    # ★핀이 실제로 걸렸는지는 런처가 알 수 없다.★ 이름을 못 찾으면 엔진이 조용히
    # primary 로 떨어지므로, 아래 배너의 pacing_output= 칸은 "넘겼다" 까지만 말한다.
    # 판정은 엔진 로그의 한 줄이다(기동에 한 번, 토폴로지가 바뀔 때 다시):
    #   wall: pacing vblank: \\.\DISPLAY1 (primary, 출력 1/4) 59.95Hz -- pin "\\.\DISPLAY22" 적용됨
    # 찾지 못했으면 그 자리에 "★pin ... 을 찾지 못해 primary 로 떨어졌다★" 가 난다.
    # (예전에 이 일을 하던 VBANCHOR 줄은 2026-10-27 계측 제거에서 없어졌다.)
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
    # Canvas-ack deadlock reproduction. The bug it targets showed up once in 35 runs, so
    # leaving verification to chance does not work: 12 clean runs after the fix logged zero
    # WALLACKFLUSH, meaning the recovery never ran and those runs proved nothing.
    # -CanvasAckSkip N makes each painter skip its first N ack sends exactly the way a closed
    # gate would, which reproduces the deadlock on demand.
    # Give each wall tile its own thread so their renders overlap. Off by default: it changes
    # who owns the rendering context, so the serial path stays available for A/B and for
    # falling back if something regresses. Most of a pass is per-tile AcquireSync waiting,
    # which is per-tile and per-GPU and therefore overlappable -- WALLPASS reports a
    # parallel_ceiling of 3.0-3.8x. Two paths cannot cross a thread yet (screenshots, the
    # memory report) and say so in the log rather than failing quietly.
    [switch] $ParallelTiles,
    [int] $CanvasAckSkip = 0,
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
    # and neither helping means the premise is wrong.
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
    # dom_animation_timeline_use_frame_time: 애니메이션의 기준 시각을 "script 가 도달한
    # 순간" 대신 "그 프레임의 시각" 으로 쓴다. 엔진 기본은 켜짐이므로, 이 스위치는 끌 때
    # 쓴다. 왜 필요한지는 설계 문서 50 과 2026-10-15 설계.
    [ValidateSet("on", "off", "")]
    [string] $FrameTime = "",
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
    # (2026-10-22 측정. 쓴 도구 `tools\probe_machine_topology.ps1` 은
    # 2026-10-27 계측 제거에서 함께 삭제했다 -- 아래 표가 그 산출물이다):
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
    [string[]] $Pref = @()
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

# -PacingOutput 은 ***vblank 경로에서만*** 읽힌다. 엔진에서 그 pref 를 보는 자리는
# output_grid.rs 의 VblankSource 하나고, 그 구조체는 WaitForVBlank 경로에만 살아 있다.
# -PacingThread 가 없으면 스레드 자체가 없고, -PacingDwmQuery 가 있으면 그 스레드가
# DwmGetCompositionTimingInfo 를 묻는다 -- 둘 다 핀은 그저 무시된다.
# throw 하지 않는 이유: 스윕 스크립트가 -PacingOutput 을 고정해 두고 나머지를 토글하는
# 것은 정당한 사용이고, 이제는 엔진이 어느 출력에 묶였는지를 직접 찍는다(위 참조).
if ($PacingOutput -ne "" -and -not $PacingThread) {
    Write-Warning "-PacingOutput $PacingOutput without -PacingThread: the pin is read only by the pacing thread's vblank source, and that thread is not running. The pin will be ignored."
}
if ($PacingOutput -ne "" -and $PacingDwmQuery) {
    Write-Warning "-PacingOutput $PacingOutput with -PacingDwmQuery: the DWM-query clock asks DwmGetCompositionTimingInfo for the desktop grid and never calls WaitForVBlank, so the pin selects nothing. Drop -PacingDwmQuery to pin an output."
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
if ($CanvasAckSkip -gt 0)  { $env:SERVO_WALL_CANVAS_ACK_SKIP = "$CanvasAckSkip" }
if ($NoCanvasAckRecovery)  { $env:SERVO_WALL_DISABLE_CANVAS_ACK_RECOVERY = "1" }

$env:GST_PLUGIN_PATH            = ""
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = ""
$env:PATH = "$engine;$env:PATH"
# ***RUST_LOG 은 여기서 고정한다.*** 셸에 무엇이 세워져 있든 런마다 같은 필터로 돌아야
# 로그가 서로 비교된다. 기본은 `warn` 하나다 -- 더 보려면 이 줄을 고친다.
$env:RUST_LOG = "warn"
# 아래 검사기가 보는 것은 바로 위 줄이다. `-KeepRustLog` 가 없어졌으므로
# 필터를 바꾸는 유일한 경로가 그 손편집이고, 그것이 정확히 아래 사고가
# 난 자리다.
# ***RUST_LOG 의 알 수 없는 맨 토큰은 로그를 끄지 않는다 -- 상한을 Trace 로 올린다.***
# env_filter 는 레벨로 파싱되지 않는 맨 토큰을 `(LevelFilter::Trace, <모듈이름>)` 디렉티브로
# 바꾸고(parser.rs), Filter::filter() 가 디렉티브 레벨의 ***최대값***을 돌려주며(filter.rs),
# components/servo/servo.rs 의 setup_logging 이 그 값을 그대로 log::set_max_level 에 넣는다.
# 그래서 `RUST_LOG=critical` 은 "critical 이상만" 이 아니라 "전부 통과" 가 된다. 출력은
# (critical 로 시작하는 타깃이 없으니) 0 줄인데, 핫 루프의 trace!/debug! 는 매번 Record 를
# 만들고 로거 둘로 디스패치된다 -- 조용해지는 게 아니라 일이 늘어난다.
#
# 2026-10-27 에 이것으로 A/B 한 바퀴를 날렸다: `RUST_LOG=critical` 을 "로그 최소화" 로
# 읽었는데 실제로는 max_level 을 Info 에서 Trace 로 올린 구성이었다.
# 배너에 RUST_LOG=critical 이 그대로 찍혀서 아무도 눈치채지 못했다. 조용히 넘기지 말 것.
$rustLogLevels = @("off", "error", "warn", "info", "debug", "trace")
foreach ($directive in ($env:RUST_LOG -split "," | Where-Object { $_.Trim() -ne "" })) {
    $hasTarget = $directive -match "="
    $level = if ($hasTarget) { ($directive -split "=", 2)[1] } else { $directive }
    if ($rustLogLevels -notcontains $level.Trim().ToLower()) {
        if ($hasTarget) {
            Write-Warning "RUST_LOG directive '$($directive.Trim())' has an unknown level '$($level.Trim())'. Valid: $($rustLogLevels -join ', ')."
        } else {
            Write-Warning "RUST_LOG token '$($directive.Trim())' is NOT a log level, so env_filter reads it as a MODULE NAME at Trace -- it RAISES the global max level to Trace instead of quieting anything, and the hot-loop trace!/debug! calls start building Records every message. Valid levels: $($rustLogLevels -join ', ')."
        }
    }
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
Write-Host "  immediate_composite=$(if($NoImmediateComposite){'OFF ENTIRELY (A/B arm)'}else{'coalesced (default)'})"
# Record it in the transcript. A run that trusted every certificate should say so in
# its own log, not only in the command someone typed.
if ($IgnoreCertErrors) { Write-Host "  ignore_certificate_errors=ON (all TLS errors accepted)" }
Write-Host "  page_features=$(if($PageFeatures){'ON (rtsp/containers/images/webrtc/screen-capture/webgpu)'}else{'off -- rtsp:// video will NOT play'}) devtools=$(if($Devtools -eq ''){'off'}else{$Devtools})"
Write-Host "  RUST_LOG=$env:RUST_LOG"
Write-Host "  log=$(if($NoLog){'off (-NoLog)'}else{$LogPath})"

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

# ***cmd 래퍼로만 띄운다 -- NUMA 핀이 없어도.*** 핀 때문이 아니라 ***stderr 때문***이다:
# cmd 의 `2>` 는 자식에게 파일 핸들을 그대로 물려주는데, Start-Process -RedirectStandardError
# 는 파이프를 끼우고 PowerShell 이 퍼낸다. 쓰기당 7.1 us 대 127.4 us, 최악 0.9 ms 대 25.4 ms.
# env_logger 는 레코드마다 write+flush 를 하므로 그 차이는 로그를 부른 스레드가 그대로 문다.
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
    Write-Host "  launching via cmd (stderr = inherited handle, 18x cheaper per write than a redirected pipe)"
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

if ($DurationSec -gt 0) {
    Start-Sleep -Seconds $DurationSec
    # Wall tile windows ignore WM_CLOSE. env_logger writes stderr unbuffered, so a
    # force-kill does not lose log output.
    if (!$proc.HasExited) { Stop-Process -Id $proc.Id -Force }
    Start-Sleep -Seconds 2
} else {
    Write-Host "Running. To stop: Ctrl+C then  Stop-Process -Name winit_wall -Force"
    Wait-Process -Id $proc.Id
}

# The engine is done, so the server has nothing left to answer. Kill it rather than leaving a
# python holding the port -- the NEXT -Serve run would die with "port in use".
if ($httpServer -and -not $httpServer.HasExited) {
    Stop-Process -Id $httpServer.Id -Force -ErrorAction SilentlyContinue
}


if ($cmdFile -and (Test-Path $cmdFile)) {
    Remove-Item $cmdFile -Force -ErrorAction SilentlyContinue
}

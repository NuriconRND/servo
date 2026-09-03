# Runs the packaged multi-GPU wall (pref-era: engine knobs are --pref, not env vars).
#
# Pure ASCII on purpose -- a Korean launcher failed to parse on a test machine that
# decodes with a legacy console codepage, swallowing a closing quote.
#
# RUST_LOG is set UNCONDITIONALLY. It used to be `if (-not $env:RUST_LOG)` and that
# silently defeated the diagnostics twice: a value left over in the shell (without
# "media=info") won, so the WALLDIAG lines from the media-thread crate never appeared.
# NOTE that crate logs under the target "media", NOT "servo_media_thread" -- its
# Cargo.toml sets [lib] name = "media".

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
    [switch] $Vsync,                     # gfx_vsync_enabled (default off; see note below)
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
    [int]    $MaxPending = 0,            # 0 = leave default (1)
    [int]    $MinIntervalMs = 0,         # 0 = leave default (16)
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
    # composite. There are nine generate_frame call sites; gfx_refresh_hz only paces the
    # renderer tick (60Hz) and script runs its own 20/30ms timer, so the two together cap
    # around 110/s -- yet a single painter was measured at 200+/s. This says who.
    [switch] $FrameReason,
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
    # ***THE GPUs ARE ON ONE NUMA NODE AND LANDING ON THE OTHER ONE DESTROYS THE WALL.***
    # This box is two Xeon Gold 6248 sockets (20C/40T each) with four Radeon RX 580 all
    # reporting NUMA node 1. Measured 2026-08-26 with the node forced, 45 videos, 6/6:
    #
    #   node 1 (the GPUs' node): 23-29.5 video fps, 0.65-0.73 ms per Present, dwm 0.09-0.11
    #   node 0 (the far socket):  6.0     video fps, 2.9-3.0  ms per Present, dwm 0.64-0.66
    #
    # Every upload and every present crosses the inter-socket link on the far node. It is not
    # a capacity problem -- the collapsed runs left 31 of 80 cores idle. Before this was found,
    # the same command landed in either state at random and every A/B that straddled the two
    # compared nothing.
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
if ($SinkProf)             { $env:SERVO_MEDIA_SINK_PROF = "1" }
if ($FanoutProf)           { $env:SERVO_WEBGL_FANOUT_PROF = "1" }
if ($DcompBindProf)        { $env:SERVO_DCOMP_BIND_PROF = "1" }
if ($CanvasAckSkip -gt 0)  { $env:SERVO_WALL_CANVAS_ACK_SKIP = "$CanvasAckSkip" }
if ($NoCanvasAckRecovery)  { $env:SERVO_WALL_DISABLE_CANVAS_ACK_RECOVERY = "1" }
if ($PresentCadence)       { $env:SERVO_LOG_PRESENT_CADENCE = "1" }
if ($DcompDebug)           { $env:SERVO_DCOMP_DEBUG = "1" }
if ($VideoEscapeProf)      { $env:SERVO_VIDEO_ESCAPE_PROF = "1" }

$env:GST_PLUGIN_PATH            = ""
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = ""
$env:PATH = "$engine;$env:PATH"
if (-not $KeepRustLog) {
    # ***`winit_wall=info` is not optional decoration.*** The shell's own diagnostics
    # (WALLPASS render-pass timings, devtools port) log from the example crate, and without
    # a target for it they fall under the leading `warn` and never appear -- a switch can
    # then look like it did nothing when it worked fine. Learned twice: the same filter
    # hid the image-downscale line from `net` until it was moved to warn.
    $env:RUST_LOG = "warn,paint=info,media=info,winit_wall=info," +
                    "servo_media_gstreamer=info,servo_media_gstreamer_render_d3d11=info"
}
if ($env:RUST_LOG -notmatch "(^|,)media=") {
    Write-Warning "RUST_LOG has no 'media=' target -- the WALLDIAG consumer-device/wrap lines will be MISSING."
}

$tiles = $Rows * $Cols
$argList = @(
    "--wall-layout", $layout, "--wall-all-tiles",
    "--pref", "gfx_dcomp_mode=$DComp",
    "--pref", "gfx_vsync_enabled=$($Vsync.IsPresent.ToString().ToLower())",
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
    $argList += @("--pref", "devtools_server_enabled=true")
    $argList += @("--pref", "devtools_server_listen_address=$Devtools")
}
if ($VideoEscape -ne "")  { $argList += @("--pref", "gfx_video_escape_mode=$VideoEscape") }
if ($DcompAlwaysFlush)    { $argList += @("--pref", "gfx_dcomp_always_flush_end_frame=true") }
if ($DcompCommitInFrame)  { $argList += @("--pref", "gfx_dcomp_commit_in_end_frame=true") }
if ($WebglSwapSync -ne "off") { $argList += @("--pref", "gfx_webgl_swap_sync=$WebglSwapSync") }
if ($WebglStageCopy)      { $argList += @("--pref", "gfx_webgl_stage_to_painter_device=true") }
if ($DcompParallelCommit) { $argList += @("--pref", "gfx_dcomp_parallel_commit=true") }
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
if ($MinIntervalMs -gt 0) { $argList += @("--pref", "gfx_wall_frame_min_interval_ms=$MinIntervalMs") }
# Passthrough is appended AFTER every pref this launcher sets itself, so `-Pref x=y` wins
# over the launcher's own value for x. Servo takes the last --pref for a given name.
foreach ($p in $Pref)     { $argList += @("--pref", $p) }
$argList += $Url

Write-Host "Wall (pref-era) -- $tiles tiles requested by the page grid"
Write-Host "  layout=$layout"
Write-Host "  dcomp=$DComp dcomp_flush=$(if($DcompAlwaysFlush){'always'}else{'conditional (default)'}) dcomp_commit=$(if($DcompCommitInFrame){'in end_frame'}else{'deferred to end of pass (default)'}) webgl_swap_sync=$WebglSwapSync webgl_stage_copy=$($WebglStageCopy.IsPresent) dcomp_parallel_commit=$($DcompParallelCommit.IsPresent) rotate_tiles=$($RotateTileOrder.IsPresent) tile_size=$TileSize refresh=${RefreshHz}Hz vsync=$($Vsync.IsPresent) escape=$(if($VideoEscape -eq ''){'off'}else{$VideoEscape}) escape_buffers=$(if($VideoEscapeBuffers -eq 0){'default(2)'}else{$VideoEscapeBuffers})"
Write-Host "  sync_group=$(if($SyncGroup -le 0){'off'}else{$SyncGroup}) decoder_threads=$DecoderThreads sink_qos=$(if($SinkQos -eq ''){'policy'}else{$SinkQos}) sink_policy=$(if($SinkPolicy -eq ''){'default'}else{$SinkPolicy}) sink_pacing=$(if($SinkPacing -eq ''){'clock'}else{$SinkPacing}) numa_pin=$(if($NoNumaPin){'off'}else{'on(default)'}) audio=$(if($NoAudio){'off'}else{'on'}) pipeline=$(if($PipelineMode -eq ''){'playbin3'}else{$PipelineMode})"
Write-Host "  d3d11_profile=$($D3d11Profile.IsPresent) video_rate=$($VideoRate.IsPresent) immediate_composite=$(if($NoImmediateComposite){'OFF ENTIRELY (A/B arm)'}else{'coalesced (default)'})$(if($PSBoundParameters.ContainsKey('D3d11ProfileMs')){" threshold=${D3d11ProfileMs}ms"}else{" threshold=8ms(default)"})"
# Record it in the transcript. A run that trusted every certificate should say so in
# its own log, not only in the command someone typed.
if ($IgnoreCertErrors) { Write-Host "  ignore_certificate_errors=ON (all TLS errors accepted)" }
Write-Host "  page_features=$(if($PageFeatures){'ON (rtsp/containers/images/webrtc/screen-capture/webgpu)'}else{'off -- rtsp:// video will NOT play'}) devtools=$(if($Devtools -eq ''){'off'}else{$Devtools})"
Write-Host "  RUST_LOG=$env:RUST_LOG"
Write-Host "  log=$(if($NoLog){'off (-NoLog); no post-run analysis either'}else{$LogPath})"

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
        Write-Warning "Display adapters report different NUMA nodes ($($nodes -join ', ')). Pinning nothing -- pass -NumaNode <n> to choose."
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

if ($numaResolved -ge 0) {
    # Quote EVERY argument: the page URL carries `?rows=5&cols=9`, and a bare `&` inside a
    # .cmd is a command separator, so an unquoted URL silently truncates the run.
    $argStr = ($argList | ForEach-Object { '"' + $_ + '"' }) -join ' '
    $cmdFile = Join-Path $env:TEMP ("wall_numa_{0}_{1}.cmd" -f $PID, $numaResolved)
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
start "" /NODE $numaResolved /B /WAIT "$exe" $argStr 2> "$LogPath"
"@ | Set-Content -Encoding ascii $cmdFile
    Write-Host "  launching on NUMA node $numaResolved (via cmd start /NODE)"
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
    if ($null -eq $proc) { throw "winit_wall did not start within 10s under 'start /NODE $numaResolved'" }
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


if ($numaResolved -ge 0 -and $cmdFile -and (Test-Path $cmdFile)) {
    Remove-Item $cmdFile -Force -ErrorAction SilentlyContinue
}

if (!(Test-Path $LogPath)) { exit 0 }

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
$fr = Select-String -Path $LogPath -Pattern "FRAMEREASON total=(\d+) window_ms=[\d.]+ (.*)$" -EA SilentlyContinue
if ($fr) {
    $tally = @{}
    $totals = @()
    foreach ($m in $fr) {
        $totals += [int]$m.Matches[0].Groups[1].Value
        foreach ($part in ($m.Matches[0].Groups[2].Value -split ' ')) {
            if ($part -match "^(painter\.rs:\d+/\S+)=(\d+)$") {
                $tally[$Matches[1]] = [int]$Matches[2] + $tally[$Matches[1]]
            }
        }
    }
    $windows = $fr.Count
    Write-Host ""
    Write-Host "FRAMEREASON -- who asked for the composites ($windows one-second windows):"
    Write-Host ("  composites/s  avg={0:N1}  max={1}" -f (($totals | Measure-Object -Average).Average, ($totals | Measure-Object -Maximum).Maximum))
    foreach ($k in ($tally.Keys | Sort-Object { -$tally[$_] })) {
        Write-Host ("    {0,8:N1}/s  {1}" -f ($tally[$k] / [math]::Max($windows, 1)), $k)
    }
}

# --- WEBGLFANOUT: is a spanning WebGL canvas paying for pixels or for the replay? ---
# The verdict this is here to deliver: switches == applies means the replay loop changes GL
# context on EVERY command, and switch+lock time is then the thing to attack (invert the loop).
# If apply time dominates instead, the replay is doing real GL work and inverting buys nothing.
$wgf = Select-String -Path $LogPath -Pattern "WEBGLFANOUT window_ms=([\d.]+) swaps=(\d+) ctx=(\d+) dev=(\d+) cmds=(\d+) applies=(\d+) switches=(\d+) lock_wait_ms=([\d.]+) switch_ms=([\d.]+) apply_ms=([\d.]+) serialize_ms=([\d.]+) swap_sync_ms=([\d.]+) swap_syncs=(\d+)" -EA SilentlyContinue
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

# --- WALLSPLIT: where a wall pass actually spends its time (needs -PresentCadence).
# The DComp-on penalty is ~6.7ms a pass on the same page, and DCOMPBIND accounts for only the
# ~1.5ms spent outside the tile loop. This splits the tile step itself with the SAME window and
# denominator as WALLPASS -- mixing per-sample and per-window averages is how an earlier attempt
# produced a negative remainder.
$split = Select-String -Path $LogPath -Pattern "WALLSPLIT pass_ms=([\d.]+) = make_current=([\d.]+) \+ paint=([\d.]+) \+ present=([\d.]+) \+ outside=(-?[\d.]+)" -EA SilentlyContinue
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
            Present = [double]$g[4].Value
            Outside = [double]$g[5].Value
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
        Write-Host ("      outside   {0,7:N2}   <- deferred DComp Commit flush + the loop itself" -f (($set | Measure-Object Outside -Average).Average))
    }
    Write-Host "  Compare the same page with -DComp on and -DComp off, STEADY STATE against steady state:"
    Write-Host "  the line that grows there is the one to chase. Warm-up differences are the promotion cost."
} elseif ($PresentCadence) {
    Write-Warning "-PresentCadence was set but no WALLSPLIT line was logged. It comes from winit_wall's render pass, so a servoshell run or a run that never painted produces nothing."
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
$dbp = Select-String -Path $LogPath -Pattern "DCOMPBIND window_ms=([\d.]+) frames=(\d+)/(\d+) binds=(\d+) full_tile=(\d+) end_frame_ms=([\d.]+) flush_ms=([\d.]+) flushed=(\d+)/(\d+) commit_ms=([\d.]+) external_ms=([\d.]+) externals=(\d+) present_ms=([\d.]+) presents=(\d+) visuals=(\d+) surfaces=(\d+) deferred=(\d+) begin_ms=([\d.]+) pbuffer_ms=([\d.]+) enddraw_ms=([\d.]+) teardown_ms=([\d.]+)" -EA SilentlyContinue
if ($dbp) {
    $v = { param($m, $i) [double]$m.Matches[0].Groups[$i].Value }
    $w = 0.0; $bf = 0.0; $ef = 0.0; $binds = 0.0; $full = 0.0
    $endf = 0.0; $flush = 0.0; $commit = 0.0; $bd = 0.0; $pbuf = 0.0; $ed = 0.0; $td = 0.0
    $flushed = 0.0; $skipped = 0.0; $ext = 0.0; $extN = 0.0; $pres = 0.0; $presN = 0.0
    $vis = 0.0; $surf = 0.0; $defer = 0.0
    foreach ($m in $dbp) {
        $w += (& $v $m 1); $bf += (& $v $m 2); $ef += (& $v $m 3); $binds += (& $v $m 4)
        $full += (& $v $m 5); $endf += (& $v $m 6); $flush += (& $v $m 7)
        $flushed += (& $v $m 8); $skipped += (& $v $m 9); $commit += (& $v $m 10)
        $ext += (& $v $m 11); $extN += (& $v $m 12); $pres += (& $v $m 13); $presN += (& $v $m 14)
        $vis += (& $v $m 15); $surf += (& $v $m 16); $defer += (& $v $m 17)
        $bd += (& $v $m 18); $pbuf += (& $v $m 19); $ed += (& $v $m 20); $td += (& $v $m 21)
    }
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
    if ($binds -gt 0) { Write-Host ("  full-tile updates: {0:N0}/{1:N0}" -f $full, $binds) }
    if ($ef -gt 0) {
        # Judge on everything this compositor costs, not on end_frame alone. The escape path
        # runs in add_surface, so an escape run can have a cheap end_frame and still be entirely
        # bounded by DComp -- reading only end_frame there would blame WebRender for our own work.
        # DComp total = end_frame + the escape path (add_surface) + a deferred Commit,
        # which lands outside end_frame and would otherwise vanish from the verdict.
        $perFrame = ($endf + $ext + ($commit - $commitInFrame)) / $ef
        if ($perFrame -gt 5.0) {
            $parts = @(
                @{n="the external video path (add_surface)"; v=$ext},
                @{n="the DWM Commit"; v=$commit},
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

if ($fanout -gt 0) { Write-Warning "GPU fan-out is broken -- tiles share one D3D11 device. See the warning text in the log." }
if ($egl -gt 0)    { Write-Warning "Some tiles could not wrap video textures ($egl occurrences) -- those tiles show green." }
exit 0

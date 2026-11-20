# Runs the packaged multi-GPU wall (pref-era: engine knobs are --pref, not env vars).
#
# **This file is UTF-8 with a BOM, and pure ASCII in content.**
#
# Both halves of that matter, and the reason is one decoder bug. Without a BOM, PowerShell
# 5.1 reads this file as ANSI (cp949 here), and that decoder, on hitting an unpaired lead
# byte, swallows the following byte as well. On a comment line ending in a non-ASCII
# character that byte is the ***newline***, so the next line is pulled into the comment and
# the parser reports "Unexpected token '}'" somewhere unrelated (reproduced 2026-10-22: it
# broke the moment the PAINTERLOCK section was added).
#
# The comments were Korean for a long time and survived only because those lines happened
# to end in ASCII punctuation -- luck, not a rule. On 2026-11-10 they were all translated to
# English on the operator's instruction, which removes the hazard at the source, and the BOM
# is kept as the second line of defence.
#
# Keep both: do not let an editor strip the BOM, and do not reintroduce non-ASCII text.
# Check with `Get-Command .\run_wall_dist.ps1 -Syntax` -- it should finish quietly; a broken
# file spills parse errors there.
#
# RUST_LOG is set UNCONDITIONALLY. It used to be `if (-not $env:RUST_LOG)` and that
# silently defeated the diagnostics twice: a value left over in the shell (without
# "media=info") won, so the WALLDIAG lines from the media-thread crate never appeared.
# NOTE that crate logs under the target "media", NOT "servo_media_thread" -- its
# Cargo.toml sets [lib] name = "media".

# **Do not remove [CmdletBinding()].** Without it PowerShell **silently ignores an unknown
# named parameter and runs anyway** -- no error, no warning. Every A/B on this wall is set
# through this script's switches, so when a dist goes stale a new switch disappears without
# a word and the run looks perfectly normal. That really happened on 2026-09-17: four
# hardware runs passing -VsyncPhase were lost (old script + default value, four identical
# runs). The engine side is loud -- an unknown pref name panics in set_value and a type
# mismatch panics in try_into().unwrap(). This was the only silent hole.
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
    # gfx_wr_profiler_hud -- WebRender's on-screen profiler (HUD).
    # **It puts a number on fps for any page.** Empty = off (default). `on` = the default bundle for wall investigation.
    # Anything else goes straight through as WebRender's profiler UI string (see the pref doc
    # for the syntax).
    #   -ProfilerHud on
    #   -ProfilerHud "FPS,Renderer"
    # **The HUD itself draws text every frame** -- use it for differences between conditions,
    # not for absolute values. It is drawn per painter, so on the wall you get one per tile.
    [string] $ProfilerHud = "",
    # gfx_paint_side_animation_mesh_probe -- measure whether paint-side prediction meshes
    # when it is swapped, and print a MESHPROBE line every 10 s. A run with it off costs one
    # AtomicBool load. (2026-11-16) Big jumps (translate >= 1 px, rotation/scale part >= 0.01,
    # opacity >= 0.02) are also printed as MESHJUMPS (per-window counts) and MESHJUMP lines
    # with a UTC time, to line them up with [handover] transition times.
    [switch] $MeshProbe,
    # (2026-11-12) gfx_wr_render_target_probe -- count WebRender render-target pool churn
    # (dedicated targets allocated / GC-freed / created / deleted by the renderer, with times)
    # and intermediate surfaces shrunk to MAX_SURFACE_SIZE, and print an RTPROBE line every
    # 10 s. Hypothesis: rotate + opacity puts the region in a rotated local-space surface
    # whose size changes with the angle, and the pool only reuses exact sizes. A run with it
    # off costs one AtomicBool load per call site. (2026-11-16) The same switch also counts
    # picture-cache tiles re-rasterized: TILEPROBE (10 s window: tiles, Mpx, frames >= 6 Mpx,
    # per-frame max) and TILESEC (one line per UTC second, all painters summed), to see
    # whether a board-swap slide repaints every tile every frame.
    [switch] $RenderTargetProbe,
    # (2026-11-12) Revert switch for gfx_wr_render_target_reuse_larger (ON by default, here and
    # in the engine). ON: a dedicated WebRender render target (task over 2048 px) may reuse an
    # idle LARGER pooled texture, and a miss grows to the width/height envelope of the idle ones
    # and frees what the envelope supersedes. OFF (this switch): upstream exact-size reuse only.
    # Why: rotate + opacity makes a rotated local-space surface whose size changes with the
    # angle; exact-size reuse then allocates and frees textures all through the rotation. Dev
    # box, 6144x3456 box: 54-56 new ~40 MB textures per 10 s and the only renders over one
    # period. Wall (2026-11-12, log_rotation_opacity_judder_01/02): with this switch 254-290 new
    # textures per 10 s and judder in nearly every repeat of one segment; without it 9-20 and
    # the operator saw almost no judder after the scene appeared.
    [switch] $NoRenderTargetReuse,
    # (2026-11-12) gfx_wr_render_target_keep_frames -- with the reuse above ON, the largest
    # dedicated render target of each format (the envelope) survives this many frames unused
    # instead of the 64 MB / 60-frame GC (the 320 MB red line still frees it). 0 = upstream
    # 60-frame rule only. Default 1800 (30 s at 60 Hz), same as the engine default.
    # Why: wall run log_rotation_opacity_judder_01/01 (60c1bfd9a5a) still allocated 19-20 per
    # 10 s with reuse ON = GC frees 4-5 + supersedes 15: a moving image leaves a tile for over
    # a second, the GC drops that painter's envelope, and it regrows on return.
    [int] $RenderTargetKeepFrames = 1800,
    # (2026-11-13) gfx_wr_texture_cache_probe -- count WebRender texture-cache uploads (bytes,
    # time, frames, per-frame max), allocations and evictions (standalone vs shared atlas), and
    # print a TEXPROBE line every 10 s plus one TEXHEAVY line (UTC time, MB, ms) per frame that
    # uploads >= 4 MiB or spends >= 4 ms. Match TEXHEAVY times against ANIMSCRIPTSTART to see
    # which scene entry/exit uploaded what. Why: same rotate+opacity on an image vs a video, the
    # image judders more on transitions; hypothesis = hidden prewarm (opacity 0) never uploads,
    # and a small eviction budget throws the image away soon after it leaves the screen, so
    # every entry re-uploads it on all four painters. Off costs one AtomicBool load per site.
    [switch] $TextureCacheProbe,
    # (2026-11-16) gfx_wall_event_probe -- stamp rare heavy events with a UTC ms time and how
    # long they took: media player create/release, the real GStreamer teardown on the
    # disposal threads (media.dispose), DComp surface create/destroy, swap chain
    # promote/demote/regen, and (2026-11-16, run 08) render.over: every painter render over one
    # period with its ms time and tile origin (RENDERSPLIT has neither). Prints WALLEVENT (one line per event) and WALLEVENTS (per-window
    # counts) every 10 s. Why: in a board-swap transition the renders over one period peak in
    # the last second of the bounce while re-raster drops (log_rotation_opacity_judder_01/07);
    # this shows whether tearing down the outgoing board lands there. Off costs one AtomicBool
    # load per site.
    [switch] $EventProbe,
    # (2026-11-17) gfx_wall_span_probe -- per-thread rings of wall pass spans: every painter
    # render (with the tick it was sent on), every main-thread wait on painters (tick commit,
    # gather, produce, ack, join, flush, message handling) and every DComp Commit (issue, guard
    # acquired, done; tick / in-frame / recovery). A reporter thread prints WALLSPAN, WALLDETACH,
    # WALLMAINWAIT, WALLCOMMIT and WALLCOMMITTICK every 10 s. Why: -DetachTileRender and
    # -DcompParallelCommit did not decouple the painters in log_rotation_opacity_judder_01/11 and
    # no Commit had ever been timed; these lines show whether a tile really starts its next render
    # while another is still rendering (xpass), whether Commits overlap (conc) and where they
    # end on the DWM grid (end_phase, a circular p50/p05~p95/R summary). Recording one span is
    # two QPC reads and five atomic stores on the recording thread; rec_ns reports the stores
    # plus one QPC read. Commit sites: tick, in-frame, recovery and the video fast path.
    [switch] $SpanProbe,
    # (2026-11-17) gfx_wall_gpu_time_probe -- which page element slows the GPU. At every point
    # where WebRender splits GPU work (batch kinds: B_Image = the image, B_YuvImage = the video,
    # B_Solid = black boxes; W_TileSetup = tile clears; W_Bind/W_Unbind = DComp BeginDraw/EndDraw
    # and pbuffers; W_TextureUpload ...) it ends a pre-created D3D11 TIMESTAMP query (no flush)
    # and reads the results a few frames later without waiting. Prints WALLGPU, WALLGPUTAG,
    # WALLGPULONG (frames followed by a Commit >= 4 ms vs the rest), WALLGPUHEAVY (one line per
    # heavy frame with a ms time) every 10 s and WALLGPUSEC once a second. Why: in
    # log_rotation_opacity_judder_01/12 long Commits happened per device, only in transitions.
    # Starts the span reporter by itself (no -SpanProbe needed, but use both).
    # (2026-11-17, after run 13) Either GPU probe also prints WALLBIND (DComp BeginDraw, pbuffer
    # create, make current, transparent clear, EndDraw, pbuffer destroy CPU times per device, and
    # whether BeginDraw returned the same texture as last time), WALLFLUSH (the end_frame glFlush,
    # i.e. the -DcompAlwaysFlush arm), WALLVRAM (adapter local usage/budget once a second) and
    # WALLGPUFRAME (one row per late frame: render, GPU span, bind steps, flush, Signal and the
    # frames still pending on the GPU at that moment, completion after Signal, Commit).
    [switch] $GpuTimeProbe,
    # (2026-11-17) gfx_wall_gpu_fence_probe -- does Commit wait for GPU completion. Signals an
    # ID3D11Fence right before the Commit critical section in DComp end_frame; a WallGpuFence
    # thread stamps the moment each value completes. WALLGPUDONE compares it with the same
    # device's Commit start/end (long_done_during = the GPU finished while Commit was blocked).
    # Read it with -DcompCommitInFrame: with the tick-head Commit the Signal may submit the
    # frame one period earlier than normal. sig_call_us shows whether Signal itself blocks.
    # (2026-11-17) The fence is also read right before each Signal, so WALLGPUQUEUE splits Signal
    # blocking by how many earlier frames were still unfinished on the GPU at that moment.
    [switch] $GpuFenceProbe,
    # (2026-11-13) gfx_wr_image_tiling_threshold -- images wider or taller than this are split
    # into 512 px tiles in the shared atlas, and only visible tiles are requested, so rotation
    # and movement keep uploading/evicting tiles. 4096 = upstream (default, same as the engine).
    # 8192..16384 keeps such images in one texture (D3D11 max 16384). Ordinary photos never
    # reach it: dom_image_max_decoded_pixels (3840x2160 px by default) shrinks them first
    # (3072x4608 -> 2352x3527, one texture), so this only matters for e.g. 1000x8000 panoramas.
    [int] $ImageTilingThreshold = 4096,
    # (2026-11-13) gfx_wr_texture_cache_evict_budget_mb -- floor (MB) under every WebRender
    # texture-cache eviction threshold. 0 = upstream (standalone 8 MB, shared RGBA atlas ~5.3 MB;
    # default, same as the engine). Raise it (e.g. 1024) to keep images that left the screen
    # resident so a scene that comes back does not re-upload them. Costs that much VRAM per GPU.
    [int] $TextureCacheBudgetMB = 0,
    # Raise RUST_LOG for diagnostic runs only. Empty leaves the default "warn" alone --
    # **the default behaviour does not change.**
    #
    # **Different from `-KeepRustLog`.** That one used ***whatever the shell had set***, so the
    # filter varied run to run and the logs could not be compared; it was removed. This value
    # is ***on the command line***, so it lands in console.txt and in the banner -- which
    # filter a run used is on the record.
    #
    # **The checker below applies as usual** -- a bare token that is not a level ("critical" and
    # friends) is read by env_filter as a module name and ***raises*** the global cap to Trace.
    # That cost a full A/B round on 2026-10-27. Do not wave the warning away.
    #
    #   -RustLog "warn,webgl=debug,script::dom::webgl=debug"
    [string] $RustLog = "",
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
    # gfx_vsync_enabled. Off by default.
    #
    # **Since 2026-09-17 this switch does two things.** Originally it installed
    # DwmVsyncRefreshDriver and aligned **content production** only to DWM composition,
    # leaving the shell's present clock alone -- so turning it on changed nothing on screen,
    # and that result was nearly read as "vsync is not the cause". Now the shell's present
    # clock (drive_present_clock) subscribes to the same driver.
    #
    # Why it is needed: a soft timer can emit exactly 60 and still be a different oscillator
    # from the display, so the on-screen position becomes a sample taken somewhere between 0
    # and one period before scanout, and it wobbles frame to frame (0-38px at 60Hz for an
    # object moving 2,304px/s). Binding to vsync turns that error into a constant (one vsync
    # of latency) and it disappears. Raising -RefreshHz only makes the error smaller, at a
    # multiplied frame-build cost.
    #
    # The price: on this box DwmFlush is a spin-wait and eats one core continuously. The
    # shell re-registers every vsync, so that thread no longer sleeps idle.
    #
    # Verdict: the backstop field on WALLCLOCK. 0 means vsync is driving; if it climbs, the
    # signal was lost and we fell back to the free-running timer (a warn is logged too).
    [switch] $Vsync,
    # gfx_vsync_phase_pct: phase offset between the vsync tick and actual presentation, as a
    # percentage of the vsync period. Default 0. Meaningful only together with -Vsync.
    #
    # **Why it exists** binding to -Vsync made things worse (log_ani_debug/30, 31) while every
    # count stayed normal -- the shake is "the gap between when a value was sampled and when
    # it reaches the screen", and no counter measures that. The observed shape (the state of
    # a free-running run's bad stretch, now lasting the whole run) means the phase got pinned
    # to one spot and that spot is a scanout boundary.
    #
    # Sweep 0 / 25 / 50 / 75. If one spot is good, the phase hypothesis is confirmed and the
    # value comes with it. If none is good, it is not phase, so drop -Vsync.
    [ValidateRange(0, 99)]
    [int]    $VsyncPhase = 0,
    # gfx_present_sync_interval: SyncInterval for the tile swap chain's Present. Default 0.
    #
    # 0 = do not wait for any panel's vblank (current behaviour). 1 = each tile aligns to its
    # own output's vblank.
    #
    # **Why** there is judder left even after every value path was fixed, and three runs of the
    # same build diverged (120s clean / per-tile stutter / mixed) with identical counters in
    # all three (log_ani_debug/39). Nothing above present explains that difference, and today
    # all three presents use SyncInterval 0. The four panels are independent, non-genlocked
    # 60Hz, so -Vsync (one DWM composition clock) cannot align them -- the alignment has to be
    # per tile.
    #
    # With 1, the calling thread blocks when the queue is full. For the primary tile that
    # thread is main, so always read pass_ms on WALLPASS and gap/off on WALLCLOCK alongside it.
    [ValidateRange(0, 4)]
    [int]    $PresentSync = 0,
    # gfx_present_align_dwm_pct: snap the present clock to the DWM composition grid.
    # -1 (default) = off, 0..99 = aim at that percentage point of the DWM period.
    #
    # **This is both the cause of the judder and its cure.** Measured (log_ani_debug/47): same
    # binary, same settings, yet the clean run's commits fell at 8% of the DWM period and the
    # juddering run's at 60%, and both held that spot for all 44 windows. DWM starts
    # composing before the vblank, so a commit at 60% sits on the deadline and makes it or
    # misses it frame by frame. Which spot you get is luck at startup.
    #
    # With it on, every tick counts again from a measured vblank, so it lands in the same
    # place regardless of startup phase. This is not the same as adding a constant to a
    # free-running timer -- that adds a constant to an arbitrary origin and stays arbitrary.
    #
    # The commit trails the tick by the render time (about 2ms = 0.12 of the period), so 0
    # aims just after the vblank. Check with phase p50 on DWMPHASE -- near 0.1 is a good spot.
    #
    # It is not a wait. Only the time the timer wakes changes, so the regression where a
    # production thread queues on a shared object (the GstSystemClock incident) cannot arise
    # structurally.
    [ValidateRange(-1, 99)]
    [int]    $DwmAlign = -1,
    # gfx_present_align_commit_pct -- B3. Issue the DComp Commit at a fixed phase on the
    # common composition grid. -1 = off (default), 0..99 = that percentage point of the period.
    #
    # The commit used to go out at the **end** of the render pass, and -DwmAlign only pins the
    # **start** of the pass. So commit phase inherited the pass length, and that scatter
    # produced both judder symptoms -- a large excursion grazes the composition deadline
    # (MISSEVENT nc=1), and the wobble shakes the commit interval across composition
    # boundaries (nc=0).
    #
    # **Do not place it just ahead of -DwmAlign.** The slack is the **forward** distance from
    # tick to deadline, (CommitAlign - DwmAlign) mod 100, and below 15 points the deadline
    # arrives before the pass does, so nearly every frame falls through to an immediate commit
    # -- the feature is on and does nothing (the probe's pass p95 is 13.4% of the period). The
    # engine prints one warning line at startup.
    #
    # **CommitAlign < DwmAlign is legitimate** -- the deadline becomes that point in the NEXT
    # period, so the slack is nearly a full period, the most available. The trade is that
    # slack to composition is then the least.
    #
    # **Stale note (recorded 2026-09-21; reversed after `1cdfd52cf3d` on 2026-10-06)** --
    # -CommitAlign used to require -DcompParallelCommit alongside it, and the engine warned
    # about serialisation if it was missing. Now turning this on makes -DcompParallelCommit
    # ignored (they overlap in purpose), and that is recorded in the startup log.
    # **The verdict is the spread on TICKCOMMIT** -- success is it approaching 0.
    [ValidateRange(-1, 99)]
    [int]    $CommitAlign = -1,
    # gfx_sample_lead_periods -- B2. Each tile samples the animation at the time its own
    # output will display that frame. -1 = off (default), 0 = next vblank, 1 = +1 period (0..4).
    #
    # This fixes what B1 (aiming at per-output vblanks, now replaced by B3 -CommitAlign) could
    # not, because B1 only moved the commit time. Making B1 fully work on the wall did not
    # reduce judder (design doc, `## Hardware results`).
    #
    # **It introduces a fixed offset at the seams -- that is the content of this trade.** The
    # offset is speed x inter-output phase difference: up to 26px on the 1x probe (2,304px/s),
    # 5.6px at 500px/s. The verdict is not "do the seams line up" but "is motion smooth and
    # uniform WITHIN a tile". To judge seam geometry at the same time, use a slow object (1/4x).
    #
    # **Must be used with -DwmAlign.** On its own it snaps a free-running render onto the
    # composition grid, so now and then two renders land on the same grid point: that frame's
    # displacement is zero and the next jumps two steps. -DwmAlign locks the render tick onto
    # the same grid, making frame-to-composite 1:1.
    [ValidateRange(-1, 4)]
    [int]    $SampleLead = -1,
    # gfx_wall_direct_render: draw the tiles right there on the present-clock tick instead of
    # going through request_redraw(). Off by default (= the previous path).
    #
    # Presenting is IDCompositionDevice::Commit (pure COM) and has nothing to do with the
    # message pump, but request_redraw() asks Windows for WM_PAINT -- the lowest-priority
    # message, synthesised only once the posted-message queue is empty. This shell takes 300
    # engine wakes a second, so the render queues up behind them: a strong candidate for the
    # tick interval's p95 of 17.23ms against a 16.667ms period.
    #
    # The render thread is still main. All that changes is who calls it, and when.
    [switch] $DirectRender,
    # gfx_wall_pacing_thread: a dedicated thread raises the present tick. Off by default.
    #
    # **Under real load the cadence rides on the engine.** Measured (log_ani_debug_02/20, five
    # animations): ticks at 56.0/s (worst 41, should be 60), about_to_wait entries 185 -> 132
    # (worst 56), and 37% of ticks noticed by an engine wake rather than by the timer (2% on
    # the probe). gap_ms p95 17.23 -> 29.08.
    #
    # about_to_wait is only reached once winit has drained the event queue, and the present
    # clock's WaitUntil hangs off it. Turning this on moves only the waking to a thread --
    # the render stays on main.
    #
    # Verdict: is tick_by on WALLCLOCK mostly pace, and do ticks come back to 60.
    [switch] $PacingThread,
    # gfx_wall_pacing_clock=dwmquery: put the pacing thread back on the **old** path.
    # Does nothing without -PacingThread.
    #
    # Default (switch absent) = WaitForVBlank. The kernel wakes us on the primary's vblank,
    # so neither a DWM query nor a wait timer is in the path.
    # With this switch = ask DwmGetCompositionTimingInfo for the grid, then thread::sleep to
    # the -DwmAlign point. Those two (inter-process call latency + timer wake jitter) become
    # jitter in the tick phase, and since commit phase decides judder, it is not free.
    #
    # **Read it together with -DwmAlign.** Even on the vblank path, -DwmAlign 1..99 sleeps that
    # much again after the vblank, so the timer jitter is back -- the old structure with only
    # the anchor improved. Only -DwmAlign 0 (or omitted) is a pure signal path with no wait
    # at all, and that is also where the measured optimum is.
    #
    # Verdict: jit_ms and gap_ms p95 on WALLCLOCK, plus refresh_d[2] on COMPREFRESH.
    [switch] $PacingDwmQuery,
    # gfx_wall_pacing_clock=compositor: wake on DCompositionWaitForCompositorClock.
    #
    # **The three paths differ on two axes -- is the anchor right, and does it query.**
    #
    #              anchor          query       wait timer
    #   vblank     DXGI output (?) none        only when DwmAlign>0
    #   dwmquery   DWM grid, ok    every tick  always
    #   compositor DWM clock, ok   none        only when DwmAlign>0
    #
    # The vblank path's weakness is the anchor: which output attaches to the DWM grid varies
    # run to run (the 2026-10-26 measurement in the -PacingOutput doc). dwmquery has the
    # right anchor but the query latency enters as jitter in the tick phase. This switch
    # **avoids both** -- the DWM clock itself does the waking.
    #
    # Measured on the dev box (2026-10-28 log_DwmClock/01, single output, -DwmAlign 8): tick
    # phase p50 was 9.3% for vblank against 9.5% for dwmquery -- nearly identical, since with
    # one output the anchors agree -- but the **spread was 1.1 points against 2.7**. That 2.5x
    # is the query latency's jitter.
    #
    # **Windows 11 build 22000+ only.** The symbol is resolved at runtime, so older builds
    # still run; without it every rotation falls back to the timer. The engine's binding line
    # says which way it went:
    #   wall: pacing clock: DComp compositor clock (DWM-global, no output selection) 59.94Hz measured -- ...
    #   wall: pacing clock: **DComp compositor clock absent on this Windows** (needs build 22000+) -- ...
    #
    # **-DwmAlign's zero point differs per path.** Here the compositor tick is 0. An optimum
    # found on another path (8) does not carry over, so it ***must be swept again.*** Measure
    # it with -TickPhase.
    [switch] $PacingCompositorClock,
    # gfx_wall_frame_barrier=false: turn the wall frame barrier off.
    #
    # The barrier is the software device that keeps "all tiles raise one logical frame
    # together" (WallFrameCoordinator in components/paint/paint.rs). Turning it off removes
    # two things:
    #   - no tile is ever told to "keep the previous frame" (skips disappear).
    #   - a new wall frame does not wait for the previous one to complete. The only brake
    #     left on production is gfx_wall_frame_max_pending.
    #
    # **The cost:** tiles may show different logical frames at the same time. A caption
    # scrolling horizontally can look ***worse*** crossing a seam. Hence: A/B only.
    [switch] $NoFrameBarrier,
    # gfx_wall_tile_render_detached=true: fire the tile renders and do not wait for them.
    #
    # By default a pass fires the four tiles and then waits for all of them via
    # PaintTargetsInFlight::join(), so one slow GPU delays the next frame on every display.
    # (2026-11-17) With this on, a tile that is still rendering when the next tick comes is
    # left out of that tick -- no gather, no new frame, no render; it keeps showing its
    # previous frame and is never armed -- while the other tiles go ahead. Calls to a busy tile
    # whose result is not needed (display lists, canvas acks, frame-ready, updates) are queued
    # to it without waiting and are never dropped.
    #
    # Until 2026-11-17 this switch did nothing at all: the post-pass commit flush made a
    # blocking round trip to every painter and so waited for every render anyway
    # (log_rotation_opacity_judder_01/11: 0 of 351 overruns started while another tile's
    # overrun was still running). Check it with -SpanProbe: WALLDETACH xpass must be > 0.
    #
    # **Must be used with -DcompCommitInFrame -- enforced below.** With deferred commits a
    # tile's commit is collected after its end_frame and issued at the next tick head; a
    # detached tile may still be rendering then, and a v1 DComp device rejects Commit (and
    # drops the whole batch) while one of its surfaces is open in BeginDraw. With the commit
    # inside end_frame each tile commits its own device on its own thread, so nothing races.
    # Tile 0 still renders on the shell thread, so a slow tile 0 still delays everyone.
    [switch] $DetachTileRender,

    # gfx_wall_pacing_output: name the output the pacing thread puts WaitForVBlank on.
    # "" = primary (today's behaviour). Give a DXGI output name such as "\\.\DISPLAY22".
    #
    # **Why** the code assumed "DWM composition is tied to the primary's vblank", and this wall
    # broke that. Across four runs of the same binary on 2026-10-26 the output attached to the
    # DWM grid differed run to run, and a mismatched run sat at commit phase 52% with dup
    # 4.025% against 0.902% for an attached one. Confirm with OUTPHASE which output has
    # vs_dwmvb_ms = 0 in that session, then run once per output and the causation is proven.
    #
    # **The launcher cannot tell whether the pin actually took.** If the name is not found the
    # engine falls back to primary silently, so the pacing_output= field in the banner below
    # only says "it was passed". The verdict is one line in the engine log (once at startup,
    # again when the topology changes):
    #   wall: pacing vblank: \\.\DISPLAY1 (primary, output 1/4) 59.95Hz -- pin "\\.\DISPLAY22" applied
    # If it was not found, that slot reads "**pin ... not found, fell back to primary**".
    # (The VBANCHOR line that used to do this job went away in the 2026-10-27 strip.)
    [string] $PacingOutput = "",
    # gfx_wall_tick_phase_probe: count which phase of the DWM composition grid the pacing
    # tick lands on.
    #
    # The one variable that decides judder is which phase of the DWM grid the COMMIT lands on
    # (commit_guard.rs module doc: phase 13% -> 63% took dup from 6 to 2146), and since the
    # 2026-10-27 instrumentation strip there has been no way to put a number on it. This
    # switch brings back the minimum -- the tick phase alone.
    #
    # Nothing enters the tick path: the pacing thread only pushes one QPC value into a
    # fixed-size ring (no stderr, no DWM query), and the folding and printing happen on main.
    # A run with it off costs nothing.
    #
    # **Two cumulative lines every 10 s -- the last one is that run's answer.** This run ends
    # via Stop-Process -Force (see -DurationSec below), so an exit hook would receive
    # nothing. env_logger writes stderr unbuffered, so a forced kill does not swallow a line.
    #
    #   wall: tick phase [30s] p05/p50/p95 = 8.1/8.4/9.2 % spread 1.1 pts R=0.998 (n=1798, ...)
    #   wall: tick gap   [30s] p05/p50/p95 = 0.99/1.00/1.01 periods (n=1797, over 1.5p 0, ...)
    #
    # Phase is a ***circular*** variable, so the percentiles are circular statistics.
    # 'spread' is p95-p05 measured in rotated coordinates, so it does not wrap when the data
    # straddles 0%, and R is concentration (1 = one point, 0 = uniform). When R is low, do
    # not trust p50.
    #
    # **Self-check (timer paths only).** A -PacingDwmQuery run has snap_to_dwm_grid_at sleeping
    # toward the -DwmAlign % point of the DWM grid, so phase p50 must come out ***larger than
    # that value by the wake latency*** (a few hundred us to 1 ms = +2 to +6 points). With
    # -DwmAlign 8, p50 should be around 10-14 and the spread should be narrow. If -DwmAlign is
    # outside 0..99 the snap does not align at all and this check does not apply.
    #
    # **The vblank path has no expected value.** There p50 = (X + DwmAlign) mod 100, where X is
    # that output's phase offset between its vblank and the DWM grid -- precisely the unknown
    # being measured (vs_dwmvb). With -DwmAlign 0, align_after_vblank returns immediately, so
    # p50 is X itself.
    #
    # And this check is ***circular***, because the probe and the snap use the same DWM anchor
    # -- it tells you the probe's arithmetic is right, not that the DWM grid itself agrees
    # with the composition deadline.
    [switch] $TickPhase,
    # gfx_wall_single_publish_per_pass -- hold Renderer::update() to consuming one publish
    # per pass. On by default since 2026-11-02 (log_ani_shift_01/17: Reason cache flush 0.00
    # on all four tiles, visible judder gone). This switch turns it OFF -- off drains the
    # whole queue, drawing the earlier frames and then throwing them away
    # (renderer/mod.rs:1082). To compare back:
    #   -ProfilerHud "FPS,Reason cache flush" -NoSinglePublishPerPass
    [switch] $NoSinglePublishPerPass,
    # gfx_wall_arm_keep_previous_on_canvas_wait -- whether to arm a tile whose production was
    # declined by a canvas wait. On by default (the old behaviour): the shell skips both the
    # render and the present for that tile. Off lets the render run so the paint-side
    # animation advances -- the same behaviour as a single-painter setup, and a candidate fix
    # for the ticker juddering under ticker+WebGL. Read it with the KEEPPREV counters:
    #   -ProfilerHud "FPS" -TickPhase -NoArmOnCanvasWait
    [switch] $NoArmOnCanvasWait,
    # gfx_wall_canvas_defeats_paint_side_throttle -- while a canvas is live and drawing, stop
    # counting that WebView as paint-side, so the tick divisor does not cut its ticks.
    # Showing a ticker and a WebGL canvas together used to make the ticker's OR verdict cut
    # WebGL's rAF by the divisor, juddering both (log_webgl_perf_01/01); this breaks that
    # link. Off by default.
    #   -CanvasDefeatsThrottle
    # Compare against lowering the divisor to 1 -- that makes CSS/paint-side pages worse.
    [switch] $CanvasDefeatsThrottle,
    # gfx_wall_canvas_live_window_ms -- the window, in ms, that the switch above uses for
    # "the canvas is live". Default 500. If the fix does not fire on a real page, try raising
    # it -- the verdict is stale_worst on the PAINTSIDE line, and the signal stays on as long
    # as the window is larger than that value.
    #   -CanvasDefeatsThrottle -CanvasLiveWindowMs 2000
    [int] $CanvasLiveWindowMs = 500,
    # gfx_wall_live_canvas_tick_divisor -- the tick divisor used when a paint-side animation
    # and a live canvas are on the page together. Default 2 (30/s at 60Hz). 1 quadruples
    # reflow and judders (log_webgl_perf_01/03); the old default of 15 starves the canvas to
    # four frames a second. If the content's own frame rate is known, match it -- the WebGL
    # source measured here is ?fps=30.
    #   -CanvasDefeatsThrottle -LiveCanvasDivisor 2
    [int] $LiveCanvasDivisor = 2,
    # gfx_wall_canvas_wait_declines_production -- does waiting on a canvas image block tick
    # production. Off by default since 2026-11-03 (log_webgl_perf_01/08: on gave judder after
    # a transition, off did not; KEEPPREV canvas 196 -> 0). This switch restores the old
    # behaviour, where one stale canvas bitmap also discards everything else in that frame
    # (ticker, video).
    #   -CanvasWaitDecline
    [switch] $CanvasWaitDecline,
    # gfx_wall_webgl_tile_scissor_from_webrender -- scissor each WebGL backend's default
    # framebuffer down to the part of the canvas THAT BACKEND'S TILE ACTUALLY SHOWS, so the
    # four GPUs stop each shading the whole canvas and discarding 75% of it
    # (log_webgl_perf_01/09). Off by default.
    #
    # The rectangle comes from WebRender: during frame build each painter records the
    # intersection of the canvas's destination box with its own tile
    # (webrender::wall_canvas_rect), and the WebGL thread reads that back. So it works for
    # ANY canvas size and position, including a canvas animating across the wall during a
    # transition.
    #
    # **Seven flags collapsed into this one on 2026-11-10.** The removed ones derived the same
    # rectangle from tile geometry instead, which required the canvas backing to match the
    # virtual viewport EXACTLY -- and a transition canvas measured 11520x4321 against a
    # 11520x4320 viewport, so 56 of 60 surfaces were skipped and the flag did nothing while
    # appearing to be on (log_webgl_perf_01/25).
    #
    # Do not enable on a page using readPixels/toDataURL/copyTexImage: those read back the
    # whole canvas, and outside the tile rect it was never drawn.
    #   Verdict metric: the SCISSORGATE line -- applied%, and per-reason counts when it is not
    #   applied (no_key / missing / stale / no_isect).
    [switch] $WebglTileScissorFromWebrender,
    # gfx_wall_webgl_tile_scissor_clear -- make the post-swap clear wipe only the tile rect
    # instead of the whole surface. Default off. 2026-11-12.
    #
    # Requires -WebglTileScissorFromWebrender (without it there is no rect to use).
    #
    # What it is for: the surface-acquire tails (LOCKTAIL, >=8ms, max 82ms) are being
    # chased, and the producer holds the surface's keyed mutex for a whole canvas frame --
    # from the bind in swap_buffers to the unbind in the next swap -- with a 199MB
    # full-surface clear inside that window.
    #
    # The arithmetic argues AGAINST that being the cause: 11520x4320x4 = 199MB over
    # ~448GB/s is 0.44ms, 1-2ms with depth and stencil. So this switch is also a way to
    # RULE THE HYPOTHESIS OUT -- if on and off look the same, the clear is not the cause.
    #
    # This was in the tree once (e9e28284b31) and removed on 2026-11-10 because the
    # scissor rect itself was wrong then; lead 8 fixed that (run 39).
    [switch] $WebglTileScissorClear,
    # gfx_wall_webgl_deferred_front_buffer -- hand the consumer a front buffer that is ONE
    # FRAME OLD instead of the newest one. Default off. 2026-11-12.
    #
    # Why: the producer holds a surface's DXGI keyed mutex for a WHOLE canvas frame (the
    # AcquireSync in bind_surface_to_context through the ReleaseSync in the next swap's
    # unbind), and the consumer's AcquireSync waits for that surface's GPU work to retire.
    # Taking the NEWEST surface therefore can mean waiting on work just submitted.
    #
    # swap_buffers also puts the old front buffer into the recycle pool and then, on the
    # next line, picks a size-matching one as the new back buffer -- so it can re-take the
    # surface it just recycled. 43 of 412 tails... precisely, 180 of 412 were on recycled
    # surfaces, which this explains.
    #
    # Cost: +1 frame of presentation latency (16.7ms at 60Hz) and 2 -> 3 surfaces per
    # backend (+199MB each on this wall). The queue is latest-wins: if it fills, the front
    # entry is dropped rather than queueing stale frames.
    #
    # Verdict: if the tails shrink a lot the cause is confirmed and the cost gets weighed;
    # if they do not, this axis is ruled out and the producer's own draw work remains.
    # Watch for visible latency too -- the bounce would appear one frame late.
    [switch] $WebglDeferredFrontBuffer,
    # gfx_wall_webgl_scissor_hold_when_static -- keep a stale scissor rect when the canvas
    # is NOT animating, instead of dropping it and shading the whole surface. ON by default;
    # this switch turns it OFF. 2026-11-12.
    #
    # Why: WR only records the rect while building a display list, so once a transition ends
    # and the screen goes still, the picture holding the canvas is not re-rastered and the
    # rect ages out (run 53: age_max 1263ms). Dropping it turns the scissor off, and then all
    # four tiles shade the entire canvas -- 8Mpx average against 49.8Mpx full, a 6x swing,
    # happening on 24% of scissor changes (to_none 696 of 6000). The frame cost oscillates,
    # which is the judder.
    #
    # A stale rect from a still canvas is still correct -- nothing moved, so nothing changed.
    # The evidence is in the data: picture.rs starts the table with (0, rect) and only appends
    # animation samples, so sample_count == 1 is WR saying "no animation on this canvas". A
    # moving canvas has 2 or more and never qualifies, so this cannot shade less than it
    # should (no black band).
    [switch] $NoScissorHoldWhenStatic,
    # gfx_wall_webgl_tile_scissor_from_tile_geometry -- derive the scissor from the TILE
    # GEOMETRY (the old path) instead of from what WebRender recorded. Off by default.
    # 2026-11-13.
    #
    # Why: the old flag (gfx_wall_webgl_tile_scissor) and this implementation were removed
    # in c3450a9364b on operator instruction; 39 commits later the ticker judders again in
    # the 4-tile-fullscreen + ticker configuration that the old flag had fixed.
    #
    # Run 55 (121) says the RECTANGLE is still right: base 12.4Mpx matches a quarter tile,
    # union 12.4Mpx, widened=0, samples all 1, apply rate 99.60%, AcquireSync tails 0.07%.
    # So the regression is one of those 39 commits, and bisecting costs six field runs.
    # This flag splits the axis in ONE run:
    #   judder gone with the old path -> the scissor path is the cause; narrow inside it
    #   judder stays                  -> not the scissor; bisect the 39 commits
    #
    # Cost: the old path reads the tile geometry directly, so there is no lookup, no age and
    # no window -- and it leaves EMPTY SPACE during transition animations, which is exactly
    # why the widening work (20ee1a167ab onward) was done. Known trade-off for this A/B.
    #   -WebglTileScissorFromTileGeometry
    [switch] $WebglTileScissorFromTileGeometry,
    # gfx_wall_animation_zero_snaps_to_tick -- snap the paint-side animation origin to the
    # ACTUAL tick grid instead of estimating it from two clocks. OFF by default -- the field
    # DISPROVED it (log_webgl_perf_01/59); this switch turns it ON for comparison. 2026-11-13.
    #
    # Why: handle_new_display_list replaces every running paint-side animation on EVERY
    # display list and recomputes the origin as received_at - (now - built_at). built_at is
    # the pacing tick instant, and that difference is not the sub-millisecond scene-build
    # transit but the whole tick-to-arrival latency -- routinely 5-30 ms on the wall. The
    # conversion error therefore lands differently on every list.
    #
    # With WebGL on the page the lists arrive ~36 times a second (DLARRIVE avg 28ms), so the
    # animation origin shakes 36 times a second. Without WebGL the paint-side divisor of 15
    # means four lists a second, which is too rare to see -- matching the operator report
    # that the ticker is clean once WebGL is removed. Production and render are both 60Hz
    # (PRODUCE window edges=60.0/s calls=60.0/s), so it is the ORIGIN that jitters, not the
    # frame count. This code is identical in the a47204b3335 control, which is why run 58
    # juddered there too.
    #
    # WHY IT IS OFF: run 128 (on) measured snap(avg=1017us max=8308us) with miss=516 of 4260,
    # and the operator saw "even in the smoothest stretch the motion is not constant-speed but
    # oscillating", while run 129 (off) was "like before the change". max=8308us sits on the
    # 8333us snap limit -- the estimate often lands mid-grid, so which neighbouring tick is
    # nearer flips on a hair and the origin jumps a whole 16.7ms frame; the 12% that miss use
    # the estimate, so two kinds of origin get mixed. transit itself measures avg 8ms and
    # max 64ms, so quantising it to the grid turns variation into a staircase.
    #
    # Read it with ANIMZERO: snap avg/max is the distance between estimate and real tick,
    # clamped counts the 250ms cap binding (display_list.rs promised a TRANSITCLAMP log that
    # was never implemented), miss counts lists whose tick was not in the 64-slot ring.
    #   -AnimationZeroSnap
    [switch] $AnimationZeroSnap,
    # gfx_wall_webgl_busy_map_single_shard -- keep the busy-counter map on ONE shard, which
    # is the behaviour before 2026-11-13. Off by default (sharded); this switch turns the old
    # single-lock behaviour back on for comparison.
    #
    # Why it changed: WebGLContextBusyMap counts how many front buffers each painter holds,
    # and it used to be one Arc<RwLock<HashMap<..>>> shared by all four painters. The path
    # that hands a canvas to a WebRender frame takes that lock for WRITE every frame
    # (lock_swap_chain and mark_surface_not_busy in webrender_external_images.rs), so four
    # painters x (lock + unlock) = about 480 writes a second on a completely exclusive lock,
    # blocking the other three painters and the WebGL thread. With no WebGL on the page
    # lock_swap_chain is never called at all, which matches the report that the ticker is
    # clean once WebGL is removed. LOCKWAIT never measured this lock -- it only covers the
    # chain mutex and the AcquireSync in the same function.
    #
    # surface_id is (context_id, painter_id) and each painter only touches its own, so
    # sharding by painter removes the write contention. Behaviour is identical: same keys,
    # same counters, same read conditions -- only the shard differs.
    #
    # Read it with BUSYLOCK (tails >= 1ms) and BUSYLOCKS (every 1200 acquisitions): with the
    # switch ON those numbers are the size of the old contention; OFF they should be ~0.
    #   -WebglBusyMapSingleShard
    [switch] $WebglBusyMapSingleShard,
    # gfx_wall_gl_error_check_every -- how often (in frames) Renderer::check_gl_errors calls
    # glGetError to look for OUT_OF_MEMORY. DEFAULT 60 (once a second). 1 is the pre-
    # 2026-11-13 behaviour (every frame), 0 turns it off entirely.
    #
    # Why: glGetError flushes the driver's pending commands and SYNCHRONISES. Run 65 split
    # that section out and measured, in the same configuration, tail p50 17500us with the HUD
    # on against 245us with it off -- while every other candidate in that section was
    # innocent (overlay/DComp bind p50 325us, CPU profile aggregation 0us, HUD drawing
    # p50 73us). So the HUD does not cost what it draws; it costs what glGetError then waits
    # for, which is why frames with identical workload (draws=3, upload=0.00MB) took 41ms.
    #
    # Run 64 had already shown the effect from the outside: dropping -ProfilerHud "FPS" made
    # the ticker judder disappear and cut over-period frames from 305 to 19. This pref is
    # what lets the HUD stay on and still behave.
    #
    # Read it with RENDERSPLIT: glerr is this call, endf3 (devf) is the span just before it --
    # device.end_frame plus the DComp end_frame, see -HudDrawEvery (corrected 2026-11-16) --
    # and glerr_every echoes the setting so a log says which value produced it.
    #   -GlErrorCheckEvery 1    (every frame, for comparison)
    #   -GlErrorCheckEvery 0    (off)
    [ValidateRange(0, 600)]
    [int] $GlErrorCheckEvery = 60,
    # gfx_wall_hud_draw_every -- how many frames between HUD (ProfilerHud) redraws.
    # DEFAULT 15, i.e. four times a second at 60Hz. 1 is the pre-2026-11-13 behaviour (every
    # frame), 0 keeps -ProfilerHud parsing but never draws.
    #
    # Why: run 67 ran six arms. With the HUD on, devf in RENDERSPLIT had a p50 of 18314us;
    # with it off, 275us -- a 45x gap, and it did NOT depend on -GlErrorCheckEvery (the 0 arm
    # juddered just the same).
    #
    # (2026-11-16 correction) This comment used to say devf is device.end_frame, which only
    # sets GL state and never flushes (gl.rs:3757-3782), so 17ms there meant the driver
    # command queue was full. devf is not only device.end_frame: it is the mark_pbo ..
    # mark_endf3 lap in render_impl. That lap also holds the HUD overlay's DComp EndDraw and
    # the DComp compositor end_frame, and with -DcompCommitInFrame the
    # IDCompositionDevice::Commit runs there. Which of these took the 18ms in run 67 was never
    # measured, so "driver queue full" is unproven. The measurements above and the HUD result
    # stand, and so does dbg: drawing the HUD itself is p50 74us with a 14ms max.
    #
    # Four tiles x every frame x text rendering produced that wait. The HUD exists to show fps,
    # which does not need a 60Hz refresh; 15 reads fine and cuts the GL commands to a
    # fifteenth. Frames that skip the HUD do not even bind the debug overlay, so the DComp
    # tile keeps what it already has.
    #
    # Read it with RENDERSPLIT: devf should drop below 1ms, and hud_every echoes the setting.
    #   -ProfilerHud "FPS" -HudDrawEvery 1   (every frame, for comparison)
    [ValidateRange(0, 600)]
    [int] $HudDrawEvery = 15,
    # gfx_wall_canvas_ack_deadline_ms -- deadline that breaks the canvas-ack self-lock.
    # DEFAULT 2000. 0 restores the pre-2026-11-13 behaviour (no deadline at all).
    #
    # Why: in run 69 the 151 arm stopped playing WebGL at 08:51:12 and never recovered --
    # 40 seconds at 0.1-0.3fps to the end of the log, while the 150 arm (no transitions)
    # held 29.7-37.1fps throughout. What stopped was the PRODUCER: consumption and display
    # stayed healthy to the last second (PRODUCE window edges=60.0/s calls=60.0/s miss=0,
    # 240 locks/s, RENDERSPLIT still printing at 08:51:51), while every producer-side tag
    # went silent (SCISSORGATE last at 08:51:11, then 41 seconds of nothing).
    #
    # The loop is in the code: ack_is_releasable needs pending_canvas_images == 0, the only
    # normal way to empty that set is an epoch arriving via update_image, and while it is
    # non-empty script_thread skips the whole document -- so no rAF, no display list, no
    # WebGL commands, and therefore no chance for the epoch that would unblock it. It is a
    # closed loop with no timeout. A comment in webgl_thread.rs already named it a
    # self-locking loop and one path into it was closed on 2026-11-11; run 69 shows another
    # path is still open. Rather than chase paths one at a time, this deadline drops the
    # pending set and sends the ack. One frame may show a stale canvas -- better than 40s.
    #
    # Read it with ACKSTUCK (the deadline actually fired -- if you see it, a path is still
    # open and that line is your lead) and ACKDEBT (how long debts are living).
    [ValidateRange(0, 60000)]
    [int] $CanvasAckDeadlineMs = 2000,
    # gfx_wall_canvas_scissor_lead_periods -- how far ahead WebRender records the
    # per-time scissor table, in refresh periods. **Default 8** (133ms at 60Hz),
    # 0 turns it off, range 0..12. 2026-11-11.
    #
    # ***What it fixes.*** While the canvas moves across the screen (a layout transition)
    # a black band shows at the tile seams. The rect WebRender records is correct for the
    # frame it was recorded in, but the canvas is asynchronous: the bitmap drawn under
    # that scissor is presented in a LATER frame, and the distance the canvas travelled
    # in between is the band.
    #
    # ***What this value does.*** WebRender writes a TABLE of (offset, rect) pairs
    # sampled along the animation. The WebGL thread then picks the interval its own
    # bitmap will actually be on screen -- [age, age + hold], where age is how stale the
    # record is and hold is that canvas's MEASURED frame interval -- and unions only
    # within it. So this value only sets how far the table reaches; raising it does NOT
    # widen the scissor that gets applied, and its cost is the per-sample matrix work.
    #
    # Earlier revisions used this as a fixed window instead, which could not fit both a
    # heavy page (one canvas frame stays up longer, so more movement to cover) and a
    # light one. Four revisions of widening left the band at the start of a transition
    # untouched (log_webgl_perf_01/32-36); the fix was to stop guessing the latency and
    # measure it. See the design doc, section 4.2bis.
    #
    # Default 8 covers a canvas as slow as 15fps: the interval ends at age (<= 34ms, the
    # staleness cutoff) + hold (67ms at 15fps) = 101ms. If the table stops short, the
    # consumer falls back to today's rect and the band returns.
    #
    # Verdict is the band BY EYE. Also watch SCISSORGATE applied% (must not fall) and
    # RENDERCOST last10s_over (the shading cost). The first SCISSOR line per surface
    # prints hold=Nms -- that is the measured frame interval for that canvas.
    #
    #   -WebglTileScissorFromWebrender -CanvasScissorLead 0    # the control arm
    [ValidateRange(0, 12)]
    [int] $CanvasScissorLead = 8,
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
    # (2026-11-18) Run the external CPU probe, engine\wall_cpu_probe.exe, beside the engine.
    # It is a separate, hidden process that reads everything from outside; the engine does no
    # I/O for it. Every -CpuProbeIntervalMs it logs per-LP busy for EVERY processor group (all
    # 80 LPs on the wall), per physical core and NUMA node, every process on the machine
    # (protected ones too) plus DPC/ISR, and winit_wall per thread family (cycles, kernel share,
    # context switches, thread churn). Files next to the run log, same stem:
    #   <stem>.cpu.log  <stem>.cpu.threads.tsv  <stem>.cpu.summary.txt
    # (stem = -LogPath without .err.log; with -NoLog a timestamped stem in this folder). The
    # summary covers the run after an 8 s warmup, in the old thread_cpu_probe table format, so
    # it reads next to the 2026-08/09 .threads.txt files. It ends by itself when winit_wall
    # exits. Why: this build has had no CPU readout since the 2026-10-27/28 instrumentation
    # strip, and on 2026-11-18 the operator saw all 80 LPs near 100% with only -NumaNode auto.
    [switch] $CpuProbe,
    # (2026-11-18) -CpuProbe sample period. Dev box cost (16 LPs, ~4,400 threads machine-wide):
    # 2.3-3.4 ms and <= 0.006 cores per sample, -CpuProbePdh included.
    [ValidateRange(100, 60000)]
    [int]    $CpuProbeIntervalMs = 1000,
    # (2026-11-18) -CpuProbe also logs PDH % Processor Utility / Time / Performance per LP
    # (CPUUTIL lines), to tell which one Task Manager / Resource Monitor is showing.
    [switch] $CpuProbePdh,
    # (2026-11-20) Do not record the DWM state. By default every run records the current
    # session's DWM once right before the engine starts and once after it has exited (3 s after
    # the exit, so DWM has let go of what the engine held): pid, start time and age, handles,
    # threads, working set, private bytes, CPU seconds, its GPU memory per adapter, and each
    # adapter's GPU memory over all processes. Files next to the run log: <stem>.dwm.txt (both
    # records and their difference) and dwm_history.tsv in the same folder (one line per record,
    # appended, so runs can be compared over days). A DWM restart during the run is flagged.
    # Read only and outside the engine: the first GPU memory query takes ~1.3 s and happens
    # before the engine starts. A run stopped with Ctrl+C gets no post record.
    # Why: the same 54-video command ran badly under a DWM that had been up since the 10-29 boot
    # (20.6 days, log_numa_pins/03) and well after DWM restarted on 11-19 (04, 05). Whether
    # DWM accumulates something across wall runs is the open question this records.
    [switch] $NoDwmState,
    # **Stale note (recorded 2026-08-25, void after `4ff3075ca97` on 2026-08-31)** -- that env
    # var does not exist. Setting it blocks startup; this switch sends
    # `--pref gfx_video_immediate_composite_enabled=false` instead.
    #
    # Historically SERVO_DISABLE_VIDEO_IMMEDIATE_COMPOSITE=1: stop re-compositing the whole
    # scene every time a video frame arrives.
    #
    # By default painter.rs::update_images calls generate_frame(SCENE) on each video
    # arrival -- its own comment says it 're-renders the full current display list'
    # and warns it worsens 'as the number of simultaneous videos grows'. The only
    # brake is pending_frames == 0, so the rate is set by how fast a composite
    # completes, NOT by the display refresh: 20 videos measured 236 presents/s on a
    # 60Hz wall. With this set, video frames ride the script rendering-opportunity
    # cadence instead.
    #
    # **2026-08-31: the engine no longer offers that all-or-nothing choice.** Video arrivals
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
    # Give each wall tile its own thread so their renders overlap. Off by default: it changes
    # who owns the rendering context, so the serial path stays available for A/B and for
    # falling back if something regresses. Most of a pass is per-tile AcquireSync waiting,
    # which is per-tile and per-GPU and therefore overlappable -- WALLPASS reports a
    # parallel_ceiling of 3.0-3.8x. Two paths cannot cross a thread yet (screenshots, the
    # memory report) and say so in the log rather than failing quietly.
    [switch] $ParallelTiles,
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
    # **2026-10-28: that A/B's premise no longer holds.** The numbers above date from when the
    # four painters ran ***sequentially on the main thread*** -- "the other three tiles render
    # first, which buys about 11ms" names exactly that structure -- and `-ParallelTiles`
    # removed it. With a thread per tile, end_frame runs on that thread and the Commit goes
    # out right after that tile's render. In a local A/B (one 3840x1080 tile) turning this
    # switch ON won: judder "barely observable" by eye, alongside -DwmAlign 8
    # -PacingDwmQuery. **Not re-confirmed on the wall** -- there are four devices there and the
    # phase story differs.
    #
    # **There is also a measurement pointing the other way** (`flush_deferred_dcomp_commits`
    # doc in paint.rs, 2026-10-23): issuing the commit at the end of the pass makes its phase
    # inherit the pass length, scattering it across 35-62% of the period -- past the DWM
    # intake deadline, where judder grew by tens of times. Issuing it on the tile thread
    # right after the render has the same phase property: the phase follows render time. A
    # commit at the head of the tick is different -- there `-DwmAlign` sets the phase
    # directly. Which of the two wins may depend on the configuration, so look at the wall
    # once more before changing the default.
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
    # (2026-11-17) gfx_dcomp_parallel_commit_wait_all: put -DcompParallelCommit back to its
    # 2026-10-23 behaviour, where the shell waits for all four tick Commits before any tile
    # renders. Since 2026-11-17 the shell issues them and does NOT wait; each tile waits only
    # for its own device's Commit right before it renders (a v1 DComp device only rejects a
    # Commit while one of ITS OWN surfaces is open in BeginDraw). Kept so one dist can A/B both.
    # Read it with -SpanProbe: WALLMAINWAIT tick_commit_us should drop to tens of microseconds
    # and WALLCOMMITTICK gate_p<n>_us shows who waited for whom.
    [switch] $ParallelCommitWaitAll,
    # gfx_dcomp_update_quantum_px: pin the BeginDraw update rect to a fixed grid so DComp is
    # asked for ONE texture size instead of dozens. 0 = off, -1 = the whole tile,
    # >0 = round the rect's SIZE up to that many pixels.
    #
    # **Stale note (recorded 2026-10-20, void the same day after `b24687d184c`)** -- the
    # default is `-1` (:504, :510). Only the "(default)" wording died.
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
    # (2026-11-17) gfx_dcomp_full_tile_updates: tell WebRender the DComp compositor takes no
    # partial updates (max_update_rects = 0), so every invalidated picture-cache tile is redrawn
    # whole and BeginDraw is always asked for one size. Why: -DcompUpdateQuantum -1 fixed the
    # ticker by pinning the update rect, but only on the transparent-expand path (non-opaque and
    # dirty == valid); the partial-update path keeps WebRender's dirty rect, because growing it
    # would erase content WebRender did not redraw. In a board swap, while the board's edge
    # crosses a tile, dirty < valid and the BeginDraw width changes every frame (3840/3872/4544/
    # 5760 x 2176). In log_rotation_opacity_judder_01/14, 97-99% of the blocked submissions were
    # those slow-BeginDraw frames (27-47% of such binds blocked, 1.3-7.1% of whole-tile binds).
    # This lets WebRender itself redraw the whole tile, so the size is one without erasing
    # anything. Cost: more re-raster (steady video redraws the whole tile, not just the video
    # quad). Read WALLBIND tex=/texture sizes and WALLGPUQUEUE/WALLGPUFRAME against a run without.
    [switch] $DcompFullTileUpdates,
    # (2026-11-17) gfx_dcomp_opaque_full_tile: pin the BeginDraw update rect of OPAQUE surfaces to
    # the whole tile too, when dirty == valid and the tile's visible clip lies inside the valid
    # rect. off = today; clear = pin + clear outside the valid rect to transparent; noclear = pin
    # only. Why: run 15 (log_rotation_opacity_judder_01/15) showed -DcompFullTileUpdates is a
    # no-op: a moving board changes the tile's VALID rect, WebRender then invalidates the whole
    # tile and clips dirty back to valid, so dirty already equals valid. The size that changes
    # every frame is the valid rect itself (the board's extent inside the tile), and those
    # surfaces are opaque, so the existing pin (-DcompUpdateQuantum -1) never applies to them.
    # Opaque binds whose width changed every frame blocked 34-61%; constant-width binds 4-6%.
    # Pixels outside the valid rect are never composited (the visual clip is inside it), so
    # 'noclear' should be safe and saves the ~0.8 ms GPU clear per bind that 'clear' pays.
    # Read WALLBINDINFO (expand_opaque=, sizes=, size_changed=) to confirm it took effect.
    #
    # ***noclear IS NOW THE DEFAULT*** (2026-11-17, run 16, log_rotation_opacity_judder_01/16):
    # with it the operator saw no judder in any board-swap transition (noclear and clear, 4 runs),
    # late renders per transition fell from 27.5 (off) to 0.07 (noclear) / 0.21 (clear), and
    # blocked submissions from 151 to about 3. noclear costs ~0.1 ms GPU per bind; clear costs
    # ~1.0 ms and made 5.4% of steady-video frames complete 12 ms+ late. The operator checked
    # noclear for visible stale pixels and steady video: none. Pass -DcompOpaqueFullTile off for
    # the A/B off arm.
    [ValidateSet("off", "clear", "noclear")]
    [string] $DcompOpaqueFullTile = "noclear",
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
    # dom_animation_timeline_use_frame_time: take an animation's reference time from "the
    # time of that frame" instead of "the moment script got there". The engine default is
    # ON, so this switch is for turning it OFF. For why it matters see design note 50 and
    # the 2026-10-15 design.
    [ValidateSet("on", "off", "")]
    [string] $FrameTime = "",
    # ***2026-10-22 correction: the GPUs in this box are no longer on one node.*** The
    # mainboard was ***replaced*** after a failure on 2026-10-19, and the replacement splits
    # the four cards across two nodes (measured 2026-10-22; the tool used,
    # `tools\probe_machine_topology.ps1`, was deleted along with the 2026-10-27
    # instrumentation strip -- the table below is its output):
    #
    #   RX 580  PCI bus 134 -> NUMA node 1
    #   RX 580  PCI bus 175 -> NUMA node 1
    #   RX 580  PCI bus  24 -> NUMA node 0
    #   RX 580  PCI bus  59 -> NUMA node 0
    #
    # **So there is no correct `-NumaNode` value any more.** Whichever node you pick, ***two***
    # of the four tiles stay on the far one. Measured 2026-10-22 (wall_anim_jitter_probe,
    # 120 s, duplicate-frame share from COMPREFRESH -- worst panel):
    #
    #   -NumaNode auto (no pin)   0.15% / 0.39%        <- best
    #   -NumaNode 1               1.00% / 1.29%
    #   -NumaNode 0               0.02% / 2.23%        <- the same command gave both extremes
    #
    # The two pinned arms were ***uniformly*** defective across the whole run (a defect in
    # 91% of 1-second windows) while the unpinned arm clustered it into a few windows. In
    # other words, where it lands at startup decides the entire run.
    #
    # **The 2026-08-26 record below belongs to the PRE-REPLACEMENT board.** There all four
    # cards were on node 1 and `-NumaNode 1` was right. Use those numbers only when
    # interpreting walls from that era:
    #
    #   node 1 (the GPUs' node then): 23-29.5 video fps, 0.65-0.73 ms per Present, dwm 0.09-0.11
    #   node 0 (far socket)         :  6.0     video fps, 2.9-3.0  ms per Present, dwm 0.64-0.66
    #
    # On the far node every upload and present crosses the inter-socket link. It is not a
    # capacity problem -- even the collapsed runs left 31 of 80 cores idle. Removing the
    # mechanism on the current board needs ***per-tile thread placement, not whole-process
    # pinning***, and that does not exist yet.
    #
    # (2026-11-18) It exists now, in the engine: -NoNumaRenderPin / -NumaTileNodes /
    # -NumaPinPolicy below, and the WALLNUMA lines in the log. -NumaNode still decides the
    # processor group of every thread the engine does NOT pin (script, layout, constellation,
    # driver threads) and of allocations made before the pins.
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
    # (2026-11-18) gfx_wall_numa_render_pin=false. The ENGINE pins each tile's render thread,
    # and that painter's WebRender threads (RenderBackend, SceneBuilder, WRWorker), to the NUMA
    # node of the tile's GPU BEFORE the device is created. winit_wall resolves the nodes at
    # startup: adapter LUID -> D3DKMT -> PnP -> DEVPKEY_Device_Numa_Node (WALLNUMA lines).
    # Engine default ON; a no-op on a one-node box or when no adapter reports a node. It needs
    # one thread per GPU: every tile with -ParallelTiles, otherwise only when all tiles' GPUs
    # share one node (the main thread renders them all). Not measured on the wall yet -- this
    # switch is the OFF arm of that A/B.
    [switch] $NoNumaRenderPin,
    # (2026-11-18) gfx_wall_numa_tile_nodes: override the detected tile -> node table, by layout
    # tile index ("0,1,0,1"; '-' or an empty field keeps the detected node). For a near/far
    # A/B on the 2:2 board, or with -NumaFakeSplit to run the code path on a one-node box.
    [string] $NumaTileNodes = "",
    # (2026-11-18) gfx_wall_numa_fake_split: DIAGNOSTIC. Split every real node into N fake
    # nodes by physical cores, so the pin code path and its readback can run on a one-node dev
    # box. Memory locality does not change. 0 = off.
    [ValidateRange(0, 8)]
    [int] $NumaFakeSplit = 0,
    # (2026-11-18) media_numa_pin_policy: where the video streaming threads go (only while the
    # streaming-thread pin is on, i.e. not with -NoNumaPin).
    #   tile       = the node of the tile(s) displaying the video right now (the D3D11 path's
    #                per-frame "devices that demanded this video"), staying put while it shows
    #                on several, and spilling to the least-loaded node once one node has
    #                max(2, cores/4) more videos -- so an all-GPUs-on-one-node box degenerates to
    #                today's spread (0.53 cores/video, 733085450e3). ALL streaming threads of the
    #                pipeline are placed (bus STREAM_STATUS ENTER), because under playbin3 the
    #                decoder is not the appsink thread.
    #   roundrobin = the 2026-08-27 behaviour: the appsink thread only, index % nodes.
    # Empty = engine default (tile; it falls back to roundrobin without a multi-node table).
    [ValidateSet("", "tile", "roundrobin")]
    [string] $NumaPinPolicy = "",
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
if ($ParallelCommitWaitAll -and -not $DcompParallelCommit) {
    throw "-ParallelCommitWaitAll only changes -DcompParallelCommit. Pass -DcompParallelCommit too."
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
# Only complain when the operator TYPED it: noclear is the default now, and -DComp off is this
# script's own default, so testing the value alone would throw on every -DComp off run (the same
# trap -DcompUpdateQuantum hit when its default became -1).
if ($PSBoundParameters.ContainsKey('DcompOpaqueFullTile') -and $DcompOpaqueFullTile -ne "off" -and $DComp -eq "off") {
    throw "-DcompOpaqueFullTile changes the DComp BeginDraw update rect. With -DComp off there is no DComp compositor, so the run would look configured and change nothing. Pass -DComp surface."
}
if ($DcompOpaqueFullTile -ne "off" -and $DComp -ne "off" -and $DcompUpdateQuantum -eq 0) {
    Write-Warning "-DcompOpaqueFullTile with -DcompUpdateQuantum 0: opaque binds are pinned to their own valid rect, which is what they already get, so the lever does nothing. Leave -DcompUpdateQuantum at -1 (default) to pin to the whole tile."
}
if ($DcompFullTileUpdates -and $DComp -eq "off") {
    throw "-DcompFullTileUpdates changes what WebRender asks the DComp compositor to redraw. With -DComp off there is no DComp compositor, so the run would look configured and change nothing. Pass -DComp surface."
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
# -SampleLead samples the animation on the DWM composition grid. But if the render tick
# free-runs (= no -DwmAlign), render and grid drift against each other and now and then
# two renders land on the same grid point: that frame has zero displacement and the next
# one jumps two steps -- worse than the judder being fixed. This was measured, not
# guessed (doing the same thing on per-output grids took ANIMSTEP p05 from 35.0 to 29.3).
# Not a throw, because measuring the two separately is itself a valid experiment -- but it
# will not be allowed to get quietly worse.
if ($SampleLead -ge 0 -and -not ($DwmAlign -ge 0 -and $DwmAlign -le 99)) {
    Write-Warning "-SampleLead $SampleLead without -DwmAlign: the render tick free-runs while the animation samples on the composition grid, so the two will slide against each other and produce zero-displacement frames. Pass -DwmAlign (8 is the measured-good phase) unless you are deliberately measuring them apart."
}

# -DetachTileRender only holds when the Commit is not deferred (2026-11-17 corrected reason):
# a deferred commit goes out at the next tick head, and a detached tile may still be inside
# BeginDraw then -- a v1 DComp device rejects that Commit and drops the whole batch.
if ($DetachTileRender -and -not $DcompCommitInFrame) {
    throw "-DetachTileRender requires -DcompCommitInFrame: with deferred commits the tick-head Commit can land while a detached tile still has a surface open in BeginDraw, and a v1 DComp device then rejects the whole batch."
}
# The frame barrier needs every tile's frame-ready answer before it decides, so a busy tile
# is still waited for there. Detaching works, but only partly, with the barrier on.
if ($DetachTileRender -and -not $NoFrameBarrier) {
    Write-Warning "-DetachTileRender with the frame barrier on: frame-ready for a busy tile still waits for that tile (the barrier needs the answer), so tiles stay partly coupled. Pass -NoFrameBarrier."
}
# (2026-11-17 correction) This used to warn about -NoFrameBarrier too, but the barrier is
# switched off in Paint::register_frame whether or not tiles have their own threads, so
# -NoFrameBarrier does change behaviour without -ParallelTiles. Only the detach switch needs
# tile threads: inline painters always render synchronously on the shell thread.
# (2026-11-17) review: the fence Signal may submit the frame earlier than the tick-head Commit
# would, so in that mode WALLGPUDONE and WALLCOMMIT are not valid for judgment. A warning, not a
# throw: a fence on/off A/B in tick mode is the only way to measure that perturbation.
if ($GpuFenceProbe -and -not $DcompCommitInFrame) {
    Write-Warning "-GpuFenceProbe without -DcompCommitInFrame: with the tick-head Commit the fence Signal may submit this frame up to one period earlier than normal, so long Commits can shrink because of the probe. Do not judge WALLGPUDONE/WALLCOMMIT from this run; compare with a run without -GpuFenceProbe in the same configuration."
}
if ($DetachTileRender -and -not $ParallelTiles) {
    Write-Warning "-DetachTileRender without -ParallelTiles: detaching only changes behaviour when tiles render on their own threads. It will do nothing here."
}
# (2026-11-18) -Confine hard-confines the whole process to ONE processor group
# (SetProcessAffinityMask after launch). The engine pins render threads and video streaming
# threads per NUMA node, which on the wall means both groups. The two cannot both hold: the
# affinity call fails once threads span groups, or the later pins escape it, and the run is not
# what its command line says. The streaming-thread pin already conflicted ("must not be
# combined", -NumaPin comment); this enforces it.
if ($Confine -and (-not $NoNumaRenderPin -or -not $NoNumaPin)) {
    throw "-Confine confines the whole process to one processor group, but the engine pins render threads (turn off with -NoNumaRenderPin) and video streaming threads (turn off with -NoNumaPin) per NUMA node. Pass -NoNumaRenderPin -NoNumaPin with -Confine, or drop -Confine."
}
if ($NumaTileNodes -ne "" -and $NumaTileNodes -notmatch '^\s*(\d+|-)?\s*(,\s*(\d+|-)?\s*)*$') {
    throw "-NumaTileNodes must be a comma list of node numbers by layout tile index, with '-' or an empty field for 'detected' (got '$NumaTileNodes')"
}
if ($NumaTileNodes -ne "" -and $NoNumaRenderPin -and $NoNumaPin) {
    Write-Warning "-NumaTileNodes with -NoNumaRenderPin -NoNumaPin: nothing is placed, so the override changes nothing."
}
if ($NumaPinPolicy -ne "" -and $NoNumaPin) {
    Write-Warning "-NumaPinPolicy $NumaPinPolicy with -NoNumaPin: the streaming-thread pin is off, so the policy does nothing."
}
# (2026-11-18 review) Name only the threads the engine can actually place in this configuration.
if ($NumaNode -match '^\d+$' -and (-not $NoNumaRenderPin -or -not $NoNumaPin)) {
    $placed = @()
    if (-not $NoNumaRenderPin) {
        if ($ParallelTiles) { $placed += "each tile's render thread and its WebRender threads" }
        else { $placed += "the render thread only if every tile's GPU is on one node (serial tiles; on the 2:2 wall 'WALLNUMA render pin:' says Off and the main and WebRender threads stay under /NODE $NumaNode)" }
    }
    if (-not $NoNumaPin) {
        if ($NumaPinPolicy -eq 'roundrobin') { $placed += "video appsink threads (round-robin across nodes, not by tile)" }
        else { $placed += "video streaming threads (by the tile showing the video)" }
    }
    Write-Warning "-NumaNode ${NumaNode}: /NODE picks the processor group of every thread the engine does not pin (and of allocations made before the pins). On a multi-node box the engine places: $($placed -join '; '). Check the WALLNUMA lines in the log."
}

# Pick exactly one pacing clock. Passing both leaves no way, even in the log, to tell
# which one ran.
if ($PacingDwmQuery -and $PacingCompositorClock) {
    throw "-PacingDwmQuery and -PacingCompositorClock select different pacing clocks; pass at most one."
}
if ($PacingCompositorClock -and -not $PacingThread) {
    Write-Warning "-PacingCompositorClock without -PacingThread: the compositor clock is read only by the pacing thread, and that thread is not running. The switch will do nothing."
}
if ($PacingOutput -ne "" -and $PacingCompositorClock) {
    Write-Warning "-PacingOutput $PacingOutput with -PacingCompositorClock: the compositor clock is a single desktop-wide signal with no output parameter, so the pin selects nothing. Drop -PacingCompositorClock to pin an output."
}

# -PacingOutput is read ***only on the vblank path***. The one place the engine reads that
# pref is VblankSource in output_grid.rs, and that struct only exists on the WaitForVBlank
# path. Without -PacingThread there is no thread at all, and with -PacingDwmQuery that
# thread queries DwmGetCompositionTimingInfo instead -- either way the pin is ignored.
# Not a throw: a sweep script that pins -PacingOutput and toggles the rest is legitimate,
# and the engine now logs which output it actually bound to (see above).
if ($PacingOutput -ne "" -and -not $PacingThread) {
    Write-Warning "-PacingOutput $PacingOutput without -PacingThread: the pin is read only by the pacing thread's vblank source, and that thread is not running. The pin will be ignored."
}
if ($TickPhase -and -not $PacingThread) {
    Write-Warning "-TickPhase without -PacingThread: the probe samples the pacing thread's ticks, and that thread is not running. The summary will report no samples."
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

# (2026-11-18) -CpuProbe options only configure the probe; alone they would look set and do nothing.
# A pure parameter check, so it sits up here with the others: below, -Serve starts the http
# server, and a throw after that would leave python holding the port for the next -Serve run.
if (($CpuProbePdh -or $PSBoundParameters.ContainsKey('CpuProbeIntervalMs')) -and -not $CpuProbe) {
    throw "-CpuProbePdh / -CpuProbeIntervalMs only configure -CpuProbe. Pass -CpuProbe too."
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

# Freeze this to an absolute path HERE. The four places below that use it resolve a
# relative path ***differently***:
#   - `cmd ... 2> "$LogPath"`  -> cmd's current directory
#   - `Start-Process -RedirectStandardError` -> the ***.NET process CWD***, which is
#     neither PowerShell's current location nor the `-WorkingDirectory` on the same line.
#   - `Test-Path` / `Select-String -Path` -> PowerShell's current location
# When those disagree the log is written somewhere else and the analysis skips with a
# silent "file not found".
#
# It also ***creates the parent directory***, because neither `2>` nor
# -RedirectStandardError will -- which is why `-LogPath ..\logs\run.err.log` used to
# fail. `make_wall_dist.ps1 -Force` rebuilds the dist folder wholesale, so logs you want
# to keep belong ***outside*** it; these two lines are what makes that possible.
if ($LogPath -ne "NUL") {
    # Do not use `[IO.Path]::GetFullPath` on its own -- on Windows PowerShell 5.1 it
    # resolves against the ***process CWD***, not PowerShell's current location. A relative
    # path the user typed is relative to where they stand, so combine with `$PWD` explicitly.
    $LogPath = [IO.Path]::GetFullPath([IO.Path]::Combine($PWD.Path, $LogPath))
    $logDir = Split-Path -Parent $LogPath
    if ($logDir -and -not (Test-Path $logDir)) {
        New-Item -ItemType Directory -Path $logDir -Force | Out-Null
    }
}

# (2026-11-18) Where -CpuProbe writes: the run log's own stem, made absolute above, so the
# .cpu.* files sit next to the .err.log they belong to. -NoLog has no log to sit next to, so
# it gets a timestamped stem here; the probe is outside the engine, so -NoLog's promise (no
# engine stderr I/O) still holds.
# (2026-11-20) The same stem also names the DWM state record (<stem>.dwm.txt), so it is
# computed once for both.
$runStem = ""
if ($NoLog) {
    $runStem = Join-Path $here ("wall_{0}" -f (Get-Date -Format "yyyyMMdd_HHmmss"))
} elseif ($LogPath -match '\.err\.log$') {
    $runStem = $LogPath -replace '\.err\.log$', ''
} else {
    $runStem = Join-Path (Split-Path -Parent $LogPath) ([IO.Path]::GetFileNameWithoutExtension($LogPath))
}
$cpuStem = ""
if ($CpuProbe) { $cpuStem = $runStem }
$dwmFile = ""
$dwmHistory = ""
if (-not $NoDwmState) {
    $dwmFile = "$runStem.dwm.txt"
    $dwmHistory = Join-Path (Split-Path -Parent $runStem) "dwm_history.tsv"
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
# ***RUST_LOG is pinned here.*** Whatever the shell has set, every run must use the same
# filter or the logs cannot be compared. The default is a bare `warn`.
#
# Diagnostic runs state it with `-RustLog` -- the shell env is still never used (that is
# why `-KeepRustLog` was removed). Being on the command line, the value lands in
# console.txt and in the banner.
$env:RUST_LOG = if ($RustLog.Trim() -ne "") { $RustLog.Trim() } else { "warn" }
# What the checker below sees is the EFFECTIVE value the line above just built -- a
# hand-edit and a `-RustLog` go through the same check. The accident described next
# happened in exactly this filter.
# ***An unknown bare token in RUST_LOG does not quiet the log -- it raises the cap to
# Trace.*** env_filter turns a bare token that does not parse as a level into a
# `(LevelFilter::Trace, <module name>)` directive (parser.rs), Filter::filter() returns
# the MAXIMUM directive level (filter.rs), and setup_logging in components/servo/servo.rs
# feeds that straight to log::set_max_level. So `RUST_LOG=critical` means "everything
# passes", not "critical and above". Output is zero lines (no target starts with
# critical), but trace!/debug! in hot loops now build a Record and dispatch it to two
# loggers every time -- more work, not less.
#
# This cost a full A/B round on 2026-10-27: `RUST_LOG=critical` was read as "minimise
# logging" when it actually raised max_level from Info to Trace, and the banner printed
# RUST_LOG=critical so nobody caught it. Do not wave the warning away.
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
    "--pref", "gfx_wall_frame_barrier=$((-not $NoFrameBarrier.IsPresent).ToString().ToLower())",
    "--pref", "gfx_wall_tile_render_detached=$($DetachTileRender.IsPresent.ToString().ToLower())",
    "--pref", "gfx_wall_pacing_clock=$(if($PacingDwmQuery){'dwmquery'}elseif($PacingCompositorClock){'compositor'}else{'vblank'})",
    "--pref", "gfx_wall_pacing_output=$PacingOutput",
    "--pref", "gfx_wall_tick_phase_probe=$($TickPhase.IsPresent.ToString().ToLower())",
    "--pref", "gfx_wall_single_publish_per_pass=$((-not $NoSinglePublishPerPass.IsPresent).ToString().ToLower())",
    "--pref", "gfx_wall_arm_keep_previous_on_canvas_wait=$((-not $NoArmOnCanvasWait.IsPresent).ToString().ToLower())",
    "--pref", "gfx_wall_canvas_defeats_paint_side_throttle=$($CanvasDefeatsThrottle.IsPresent.ToString().ToLower())",
    "--pref", "gfx_wall_canvas_live_window_ms=$CanvasLiveWindowMs",
    "--pref", "gfx_wall_live_canvas_tick_divisor=$LiveCanvasDivisor",
    "--pref", "gfx_wall_canvas_wait_declines_production=$($CanvasWaitDecline.IsPresent.ToString().ToLower())",    "--pref", "gfx_wall_webgl_tile_scissor_from_webrender=$($WebglTileScissorFromWebrender.IsPresent.ToString().ToLower())",    "--pref", "gfx_wall_webgl_tile_scissor_clear=$($WebglTileScissorClear.IsPresent.ToString().ToLower())",    "--pref", "gfx_wall_webgl_deferred_front_buffer=$($WebglDeferredFrontBuffer.IsPresent.ToString().ToLower())",    "--pref", "gfx_wall_webgl_scissor_hold_when_static=$((-not $NoScissorHoldWhenStatic.IsPresent).ToString().ToLower())",    "--pref", "gfx_wall_webgl_tile_scissor_from_tile_geometry=$($WebglTileScissorFromTileGeometry.IsPresent.ToString().ToLower())",    "--pref", "gfx_wall_animation_zero_snaps_to_tick=$($AnimationZeroSnap.IsPresent.ToString().ToLower())",    "--pref", "gfx_wall_webgl_busy_map_single_shard=$($WebglBusyMapSingleShard.IsPresent.ToString().ToLower())",    "--pref", "gfx_wall_gl_error_check_every=$GlErrorCheckEvery",    "--pref", "gfx_wall_hud_draw_every=$HudDrawEvery",    "--pref", "gfx_wall_canvas_ack_deadline_ms=$CanvasAckDeadlineMs",
    "--pref", "gfx_wall_canvas_scissor_lead_periods=$CanvasScissorLead",
    "--pref", "gfx_refresh_hz=$RefreshHz",
    "--pref", "gfx_wr_picture_tile_size=$TileSize",
    "--pref", "gfx_wr_profiler_hud=$ProfilerHud",
    "--pref", "gfx_paint_side_animation_mesh_probe=$($MeshProbe.IsPresent.ToString().ToLower())",
    "--pref", "gfx_wr_render_target_probe=$($RenderTargetProbe.IsPresent.ToString().ToLower())",
    "--pref", "gfx_wr_render_target_reuse_larger=$((-not $NoRenderTargetReuse.IsPresent).ToString().ToLower())",
    "--pref", "gfx_wr_render_target_keep_frames=$RenderTargetKeepFrames",
    "--pref", "gfx_wr_texture_cache_probe=$($TextureCacheProbe.IsPresent.ToString().ToLower())",
    "--pref", "gfx_wall_event_probe=$($EventProbe.IsPresent.ToString().ToLower())",
    "--pref", "gfx_wall_span_probe=$($SpanProbe.IsPresent.ToString().ToLower())",
    "--pref", "gfx_wall_gpu_time_probe=$($GpuTimeProbe.IsPresent.ToString().ToLower())",
    "--pref", "gfx_wall_gpu_fence_probe=$($GpuFenceProbe.IsPresent.ToString().ToLower())",
    "--pref", "gfx_wr_image_tiling_threshold=$ImageTilingThreshold",
    "--pref", "gfx_wr_texture_cache_evict_budget_mb=$TextureCacheBudgetMB",
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
if ($ParallelCommitWaitAll) { $argList += @("--pref", "gfx_dcomp_parallel_commit_wait_all=true") }
# Always sent, never conditional. The engine default is -1 now, so "only send it when non-zero"
# would make `-DcompUpdateQuantum 0` a no-op -- the off arm of the A/B would silently run ON.
$argList += @("--pref", "gfx_dcomp_update_quantum_px=$DcompUpdateQuantum")
$argList += @("--pref", "gfx_dcomp_full_tile_updates=$($DcompFullTileUpdates.IsPresent.ToString().ToLower())")
$argList += @("--pref", "gfx_dcomp_opaque_full_tile=$(switch ($DcompOpaqueFullTile) { 'clear' { 1 } 'noclear' { 2 } default { 0 } })")
if ($RotateTileOrder)     { $argList += @("--pref", "gfx_wall_rotate_tile_order=true") }
if ($VideoEscapeBuffers -gt 0) { $argList += @("--pref", "gfx_video_escape_buffer_count=$VideoEscapeBuffers") }
if ($SinkQos -ne "")      { $argList += @("--pref", "media_video_sink_qos=$SinkQos") }
if ($SinkPolicy -ne "")   { $argList += @("--pref", "media_video_sink_policy=$SinkPolicy") }
if ($SinkPacing -ne "")   { $argList += @("--pref", "media_video_sink_pacing=$SinkPacing") }
if ($NoAudio)             { $argList += @("--pref", "media_audio_enabled=false") }
if ($NoNumaPin)           { $argList += @("--pref", "media_numa_pin_streaming_threads=false") }
elseif ($NumaPin)         { $argList += @("--pref", "media_numa_pin_streaming_threads=true") }
# (2026-11-18) Sent only when given, so this launcher still runs an engine without these prefs.
if ($NoNumaRenderPin)     { $argList += @("--pref", "gfx_wall_numa_render_pin=false") }
if ($NumaTileNodes -ne "") { $argList += @("--pref", "gfx_wall_numa_tile_nodes=$NumaTileNodes") }
if ($NumaFakeSplit -gt 0) { $argList += @("--pref", "gfx_wall_numa_fake_split=$NumaFakeSplit") }
if ($NumaPinPolicy -ne "") { $argList += @("--pref", "media_numa_pin_policy=$NumaPinPolicy") }
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
# Record WHICH dist actually ran. Without this line, four runs on a stale exe and four
# more on a stale script (2026-09-17) were only noticed afterwards. Printed right above
# the settings banner so console.txt alone settles it.
$buildStamp = Join-Path $PSScriptRoot "BUILD.txt"
if (Test-Path $buildStamp) {
    Write-Host "  build: $((Get-Content $buildStamp) -join ' | ')"
} else {
    Write-Warning "BUILD.txt missing -- this dist was not produced by make_wall_dist.ps1, or the copy was incomplete. Cannot tell which build is running."
}
Write-Host "  dcomp=$DComp dcomp_flush=$(if($DcompAlwaysFlush){'always'}else{'conditional (default)'}) dcomp_commit=$(if($DcompCommitInFrame){'in end_frame'}else{'deferred to next tick head (default)'}) webgl_swap_sync=$WebglSwapSync webgl_stage_copy=$($WebglStageCopy.IsPresent) dcomp_parallel_commit=$(if(-not $DcompParallelCommit){'off'}elseif($ParallelCommitWaitAll){'wait-all'}else{'per-device'}) update_quantum=$(if($DcompUpdateQuantum -eq 0){'off'}elseif($DcompUpdateQuantum -eq -1){'whole tile'}else{"${DcompUpdateQuantum}px"}) full_tile_updates=$($DcompFullTileUpdates.IsPresent) opaque_full_tile=$(if($DcompOpaqueFullTile -eq 'noclear'){'noclear (default)'}else{$DcompOpaqueFullTile}) rotate_tiles=$($RotateTileOrder.IsPresent) tile_size=$TileSize hud=$(if($ProfilerHud -eq ''){'off'}else{$ProfilerHud}) mesh_probe=$($MeshProbe.IsPresent) rt_probe=$($RenderTargetProbe.IsPresent) rt_reuse=$(if($NoRenderTargetReuse){'off'}else{'on (default)'}) rt_keep=$RenderTargetKeepFrames tex_probe=$($TextureCacheProbe.IsPresent) event_probe=$($EventProbe.IsPresent) span_probe=$($SpanProbe.IsPresent) gpu_time_probe=$($GpuTimeProbe.IsPresent) gpu_fence_probe=$($GpuFenceProbe.IsPresent) tiling=$ImageTilingThreshold tex_budget=${TextureCacheBudgetMB}MB refresh=${RefreshHz}Hz vsync=$($Vsync.IsPresent) vsync_phase=$VsyncPhase present_sync=$PresentSync dwm_align=$DwmAlign commit_align=$CommitAlign sample_lead=$SampleLead direct_render=$($DirectRender.IsPresent) pacing_thread=$($PacingThread.IsPresent) pacing_clock=$(if(-not $PacingThread.IsPresent){'n/a'}elseif($PacingDwmQuery){'dwmquery'}elseif($PacingCompositorClock){'compositor'}else{'vblank'}) pacing_output=$(if($PacingOutput -eq ''){'primary (default)'}else{$PacingOutput}) tick_phase=$($TickPhase.IsPresent) single_publish=$(if($NoSinglePublishPerPass){'off'}else{'on (default)'}) arm_on_canvas_wait=$(if($NoArmOnCanvasWait){'off'}else{'on (default)'}) canvas_defeats_throttle=$($CanvasDefeatsThrottle.IsPresent) canvas_live_window=${CanvasLiveWindowMs}ms live_canvas_divisor=$LiveCanvasDivisor canvas_wait_decline=$(if($CanvasWaitDecline){'on'}else{'off (default)'}) scissor_from_wr=$($WebglTileScissorFromWebrender.IsPresent) scissor_clear=$($WebglTileScissorClear.IsPresent) deferred_front=$($WebglDeferredFrontBuffer.IsPresent) scissor_hold=$((-not $NoScissorHoldWhenStatic.IsPresent)) scissor_from_geom=$($WebglTileScissorFromTileGeometry.IsPresent) anim_zero_snap=$($AnimationZeroSnap.IsPresent) busy_single_shard=$($WebglBusyMapSingleShard.IsPresent) glerr_every=$GlErrorCheckEvery hud_every=$HudDrawEvery ack_deadline=$CanvasAckDeadlineMs scissor_lead=$CanvasScissorLead frame_barrier=$(if($NoFrameBarrier){'off'}else{'on (default)'}) tile_render=$(if($DetachTileRender){'detached'}else{'joined (default)'}) escape=$(if($VideoEscape -eq ''){'off'}else{$VideoEscape}) escape_buffers=$(if($VideoEscapeBuffers -eq 0){'default(2)'}else{$VideoEscapeBuffers})"
Write-Host "  sync_group=$(if($SyncGroup -le 0){'off'}else{$SyncGroup}) decoder_threads=$DecoderThreads sink_qos=$(if($SinkQos -eq ''){'policy'}else{$SinkQos}) sink_policy=$(if($SinkPolicy -eq ''){'default'}else{$SinkPolicy}) sink_pacing=$(if($SinkPacing -eq ''){'clock'}else{$SinkPacing}) numa_pin=$(if($NoNumaPin){'off'}else{'on(default)'}) numa_pin_policy=$(if($NoNumaPin){'n/a'}elseif($NumaPinPolicy -eq ''){'tile(default)'}else{$NumaPinPolicy}) numa_render_pin=$(if($NoNumaRenderPin){'off'}else{'on(default)'}) numa_tile_nodes=$(if($NumaTileNodes -eq ''){'auto'}else{$NumaTileNodes}) numa_fake_split=$NumaFakeSplit audio=$(if($NoAudio){'off'}else{'on'}) pipeline=$(if($PipelineMode -eq ''){'playbin3'}else{$PipelineMode})"
Write-Host "  immediate_composite=$(if($NoImmediateComposite){'OFF ENTIRELY (A/B arm)'}else{'coalesced (default)'})"
# Record it in the transcript. A run that trusted every certificate should say so in
# its own log, not only in the command someone typed.
if ($IgnoreCertErrors) { Write-Host "  ignore_certificate_errors=ON (all TLS errors accepted)" }
Write-Host "  page_features=$(if($PageFeatures){'ON (rtsp/containers/images/webrtc/screen-capture/webgpu)'}else{'off -- rtsp:// video will NOT play'}) devtools=$(if($Devtools -eq ''){'off'}else{$Devtools})"
Write-Host "  RUST_LOG=$env:RUST_LOG"
Write-Host "  log=$(if($NoLog){'off (-NoLog)'}else{$LogPath})"
# (2026-11-18) So console.txt alone says whether a set of .cpu.* files belongs to this run.
# "requested", not "on": the probe starts only once the engine runs, and whether it did is the
# "cpu_probe   :" line printed then (or a warning).
$cpuProbeBanner = if (-not $CpuProbe) { "off" } else { "requested (every $CpuProbeIntervalMs ms, pdh=$($CpuProbePdh.IsPresent), warmup 8 s) -> $cpuStem.cpu.log / .cpu.threads.tsv / .cpu.summary.txt" }
Write-Host "  cpu_probe=$cpuProbeBanner"

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

# (2026-11-20) DWM state record (-NoDwmState turns it off). Everything here reads system state
# from outside the engine and runs only before the launch and after the exit.
function ConvertTo-LuidKey([string] $Name) {
    # 'pid_1876_luid_0x00000000_0x0002E396_phys_0' -> '00000000:0002e396', the engine's format
    # ("tile N: display ... luid 00000000:0002e396").
    $m = [regex]::Match($Name, 'luid_0x([0-9A-Fa-f]+)_0x([0-9A-Fa-f]+)')
    if (-not $m.Success) { return $Name }
    return ('{0:x8}:{1:x8}' -f [Convert]::ToUInt32($m.Groups[1].Value, 16), [Convert]::ToUInt32($m.Groups[2].Value, 16))
}
function ConvertTo-GpuMemRows($Rows) {
    return @($Rows | Sort-Object Name | ForEach-Object {
        [pscustomobject]@{
            Luid  = (ConvertTo-LuidKey $_.Name)
            DedMB = [Math]::Round($_.DedicatedUsage / 1MB, 1)
            ShMB  = [Math]::Round($_.SharedUsage / 1MB, 1)
            ComMB = [Math]::Round($_.TotalCommitted / 1MB, 1)
        }
    })
}
function Get-DwmState {
    # The DWM of THIS session (an RDP session would have its own).
    $session = (Get-Process -Id $PID).SessionId
    $st = [ordered]@{ Utc = (Get-Date).ToUniversalTime(); Boot = $null; DwmPid = 0; Start = $null;
        Handles = 0; Threads = 0; WsMB = 0.0; PrivMB = 0.0; CpuSec = 0.0; Gpu = @(); Adapters = @(); Note = '' }
    try { $st.Boot = (Get-CimInstance Win32_OperatingSystem -ErrorAction Stop).LastBootUpTime.ToUniversalTime() } catch { }
    $d = Get-Process dwm -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq $session } | Select-Object -First 1
    if ($null -eq $d) { $st.Note = 'no dwm.exe in this session'; return [pscustomobject]$st }
    $st.DwmPid = $d.Id
    try { $st.Start = $d.StartTime.ToUniversalTime() } catch { }
    $st.Handles = $d.HandleCount
    $st.Threads = $d.Threads.Count
    $st.WsMB = [Math]::Round($d.WorkingSet64 / 1MB, 1)
    $st.PrivMB = [Math]::Round($d.PrivateMemorySize64 / 1MB, 1)
    try { $st.CpuSec = [Math]::Round($d.TotalProcessorTime.TotalSeconds, 1) } catch { }
    try {
        $pfx = 'pid_' + $d.Id + '_'
        $st.Gpu = ConvertTo-GpuMemRows (Get-CimInstance Win32_PerfFormattedData_GPUPerformanceCounters_GPUProcessMemory -ErrorAction Stop |
            Where-Object { $_.Name -like ($pfx + '*') })
        $st.Adapters = ConvertTo-GpuMemRows (Get-CimInstance Win32_PerfFormattedData_GPUPerformanceCounters_GPUAdapterMemory -ErrorAction Stop)
    } catch {
        $st.Note = 'GPU memory counters unavailable: ' + $_.Exception.Message
    }
    return [pscustomobject]$st
}
function Format-UtcStamp($T) {
    if ($null -eq $T) { return '?' }
    return $T.ToString('yyyy-MM-dd HH:mm:ss') + 'Z'
}
function Get-TileLabel([hashtable] $Tiles, [string] $Luid) {
    if ($Tiles -and $Tiles.ContainsKey($Luid)) { return $Tiles[$Luid] }
    return ''
}
function Get-DwmSummary($S) {
    if ($S.DwmPid -eq 0) { return $S.Note }
    $age = '?'
    if ($S.Start) { $age = ('{0:0.00} d' -f ($S.Utc - $S.Start).TotalDays) }
    $ded = [double](($S.Gpu | Measure-Object -Property DedMB -Sum).Sum)
    return ("pid {0}, started {1} (age {2}, boot {3}), handles {4}, private {5} MB, GPU dedicated {6} MB on {7} adapter(s)" -f
        $S.DwmPid, (Format-UtcStamp $S.Start), $age, (Format-UtcStamp $S.Boot), $S.Handles, $S.PrivMB, $ded, @($S.Gpu).Count)
}
function Format-DwmState($S, [string] $Tag, [hashtable] $Tiles) {
    $out = @()
    $out += "===== dwm_state $Tag utc=$(Format-UtcStamp $S.Utc) boot=$(Format-UtcStamp $S.Boot)"
    if ($S.DwmPid -eq 0) { $out += "  $($S.Note)"; return $out }
    $age = '?'
    if ($S.Start) { $age = ('{0:0.00}d' -f ($S.Utc - $S.Start).TotalDays) }
    $out += ("  dwm pid={0} start={1} age={2} handles={3} threads={4} ws={5}MB priv={6}MB cpu={7}s" -f
        $S.DwmPid, (Format-UtcStamp $S.Start), $age, $S.Handles, $S.Threads, $S.WsMB, $S.PrivMB, $S.CpuSec)
    foreach ($g in $S.Gpu) {
        $out += ("  dwm gpu  {0} {1,-6} dedicated={2}MB shared={3}MB committed={4}MB" -f $g.Luid, (Get-TileLabel $Tiles $g.Luid), $g.DedMB, $g.ShMB, $g.ComMB)
    }
    foreach ($a in $S.Adapters) {
        $out += ("  adapter  {0} {1,-6} dedicated={2}MB shared={3}MB committed={4}MB (all processes)" -f $a.Luid, (Get-TileLabel $Tiles $a.Luid), $a.DedMB, $a.ShMB, $a.ComMB)
    }
    if ($S.Note) { $out += "  note: $($S.Note)" }
    return $out
}
function Format-DwmDelta($Pre, $Post, [hashtable] $Tiles) {
    $out = @()
    if ($Pre.DwmPid -eq 0 -or $Post.DwmPid -eq 0) {
        $out += '----- delta: not available (a record found no dwm.exe)'
        return $out
    }
    if ($Pre.DwmPid -ne $Post.DwmPid) {
        $out += ("----- DWM RESTARTED during this run: pid {0} -> {1} (new one started {2}). The records are different processes, so there is no delta." -f
            $Pre.DwmPid, $Post.DwmPid, (Format-UtcStamp $Post.Start))
        return $out
    }
    $out += ("----- delta post-pre, same dwm pid {0}: handles {1:+0;-0;0} threads {2:+0;-0;0} ws {3:+0.0;-0.0;0}MB priv {4:+0.0;-0.0;0}MB cpu {5:+0.0;-0.0;0}s" -f
        $Post.DwmPid, ($Post.Handles - $Pre.Handles), ($Post.Threads - $Pre.Threads), ($Post.WsMB - $Pre.WsMB), ($Post.PrivMB - $Pre.PrivMB), ($Post.CpuSec - $Pre.CpuSec))
    foreach ($pair in @(@('dwm gpu', $Pre.Gpu, $Post.Gpu), @('adapter', $Pre.Adapters, $Post.Adapters))) {
        $keys = @(@($pair[1]) + @($pair[2]) | ForEach-Object { $_.Luid } | Sort-Object -Unique)
        foreach ($k in $keys) {
            $a = @($pair[1]) | Where-Object { $_.Luid -eq $k } | Select-Object -First 1
            $b = @($pair[2]) | Where-Object { $_.Luid -eq $k } | Select-Object -First 1
            $ad = 0.0; $as = 0.0; $bd = 0.0; $bs = 0.0
            if ($a) { $ad = $a.DedMB; $as = $a.ShMB }
            if ($b) { $bd = $b.DedMB; $bs = $b.ShMB }
            $out += ("  {0,-8} {1} {2,-6} dedicated {3:+0.0;-0.0;0}MB shared {4:+0.0;-0.0;0}MB" -f $pair[0], $k, (Get-TileLabel $Tiles $k), ($bd - $ad), ($bs - $as))
        }
    }
    return $out
}
function ConvertTo-DwmTsv($S, [string] $Tag, [string] $Run) {
    $ageH = ''
    if ($S.Start) { $ageH = ('{0:0.00}' -f ($S.Utc - $S.Start).TotalHours) }
    $ded = [double](($S.Gpu | Measure-Object -Property DedMB -Sum).Sum)
    $sh = [double](($S.Gpu | Measure-Object -Property ShMB -Sum).Sum)
    $byAd = (@($S.Gpu) | ForEach-Object { $_.Luid + '=' + $_.DedMB }) -join ';'
    $adAll = (@($S.Adapters) | ForEach-Object { $_.Luid + '=' + $_.DedMB }) -join ';'
    return (@((Format-UtcStamp $S.Utc), $Run, $Tag, $S.DwmPid, (Format-UtcStamp $S.Start), $ageH, (Format-UtcStamp $S.Boot),
        $S.Handles, $S.Threads, $S.WsMB, $S.PrivMB, $S.CpuSec, $ded, $sh, $byAd, $adAll, $S.Note) -join "`t")
}
$dwmTsvHeader = (@('utc', 'run', 'tag', 'dwm_pid', 'dwm_start_utc', 'dwm_age_h', 'boot_utc', 'handles', 'threads', 'ws_mb',
    'priv_mb', 'cpu_s', 'dwm_gpu_dedicated_mb', 'dwm_gpu_shared_mb', 'dwm_gpu_dedicated_mb_by_adapter',
    'adapter_dedicated_mb_all_processes', 'note') -join "`t")
function Write-DwmStatePre([string] $File, [string] $History, [string] $Run) {
    $s = Get-DwmState
    Format-DwmState $s 'pre (right before the engine starts)' @{} | Out-File -LiteralPath $File -Encoding utf8
    if (-not (Test-Path -LiteralPath $History)) { $dwmTsvHeader | Out-File -LiteralPath $History -Encoding utf8 }
    ConvertTo-DwmTsv $s 'pre' $Run | Out-File -LiteralPath $History -Encoding utf8 -Append
    return $s
}
function Write-DwmStatePost($Pre, [string] $File, [string] $History, [string] $Run, [string] $EngineLog) {
    # Tile names for the adapters, from the engine's own "tile N: display ... luid" lines.
    $tiles = @{}
    if ($EngineLog -ne '' -and $EngineLog -ne 'NUL' -and (Test-Path -LiteralPath $EngineLog)) {
        Select-String -LiteralPath $EngineLog -Pattern '^tile (\d+): display .* luid ([0-9a-f]{8}:[0-9a-f]{8})' |
            Select-Object -First 16 | ForEach-Object { $tiles[$_.Matches[0].Groups[2].Value.ToLower()] = 'tile' + $_.Matches[0].Groups[1].Value }
    }
    $s = Get-DwmState
    $delta = Format-DwmDelta $Pre $s $tiles
    (@(Format-DwmState $s 'post (3 s after the engine exited)' $tiles) + $delta) | Out-File -LiteralPath $File -Encoding utf8 -Append
    ConvertTo-DwmTsv $s 'post' $Run | Out-File -LiteralPath $History -Encoding utf8 -Append
    return @($s, $delta)
}
$dwmPre = $null
if ($NoDwmState) {
    Write-Host "  dwm_state   : off (-NoDwmState)"
} else {
    try {
        $dwmPre = Write-DwmStatePre $dwmFile $dwmHistory (Split-Path -Leaf $runStem)
        Write-Host "  dwm_state   : $(Get-DwmSummary $dwmPre) -> $dwmFile"
    } catch {
        $dwmPre = $null
        Write-Warning "DWM state record (before the run) failed: $($_.Exception.Message). The run goes on without it."
    }
}

# ***Always launch through the cmd wrapper, pin or no pin.*** The reason is stderr, not
# the pin: cmd's `2>` hands the child a plain file handle, while Start-Process
# -RedirectStandardError inserts a pipe that PowerShell has to drain. 7.1 us per write
# against 127.4 us, worst case 0.9 ms against 25.4 ms. env_logger does a write+flush per
# record, so that difference is paid by whichever thread called the log macro.
$argStr = ($argList | ForEach-Object { '"' + $_ + '"' }) -join ' '
$cmdFile = Join-Path $env:TEMP ("wall_numa_{0}_{1}.cmd" -f $PID, $numaResolved)
# With no pin, /NODE is left out entirely; with one, it goes in that slot. Every space
# matters -- `start "" /B` and `start "" /NODE 1 /B` have to come out of the same line.
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

# (2026-11-18) -CpuProbe starts the moment the engine PID is known and before -Confine, so its
# baseline predates any pinning. Hidden, its own process, its own files, no /NODE and no pipe.
# It ends by itself when winit_wall exits (its wait between samples is a wait on the engine's
# process handle) -- the force-kill below and a Ctrl+C on an unlimited run included; after a
# Ctrl+C this script prints nothing more, but the probe still writes its summary.
$cpuProbeProc = $null
# (2026-11-18) What became of the probe, for the end-of-run text: "missing" (no exe), "nostart"
# (Start-Process threw), "early" (exited within 1.5 s: a startup refusal), "running".
$cpuProbeState = ""
$cpuProbeStart = $null
$cpuSummary = "$cpuStem.cpu.summary.txt"
$cpuLog = "$cpuStem.cpu.log"
# (2026-11-18) Why the probe stopped, from its own log: its CPUERROR line(s), or the fact that it
# never got as far as creating the log (it exits before that when it cannot create its files).
function Write-CpuProbeError([string] $LogFile) {
    if (!(Test-Path -LiteralPath $LogFile)) {
        Write-Warning "  wall_cpu_probe wrote no $LogFile at all: it could not create its files (folder missing or not writable?)."
        return
    }
    $cpuErr = @(Select-String -LiteralPath $LogFile -Pattern '^CPUERROR ' | ForEach-Object { $_.Line })
    if ($cpuErr.Count -gt 0) {
        $cpuErr | ForEach-Object { Write-Warning "  $_" }
    } else {
        Write-Warning "  $LogFile has no CPUERROR line (killed from outside, or it crashed)."
    }
}
if ($CpuProbe) {
    $cpuProbeExe = Join-Path $engine "wall_cpu_probe.exe"
    if (!(Test-Path $cpuProbeExe)) {
        $cpuProbeState = "missing"
        Write-Warning "-CpuProbe: $cpuProbeExe not found (a dist packaged before the probe existed, or its build failed in make_wall_dist.ps1). Running WITHOUT the CPU probe."
    } else {
        $buildCommit = "?"
        if (Test-Path $buildStamp) {
            $commitMatch = Select-String -Path $buildStamp -Pattern 'commit\s*:\s*(\S+)' | Select-Object -First 1
            if ($commitMatch) { $buildCommit = $commitMatch.Matches[0].Groups[1].Value }
        }
        # The banner essentials, so the summary alone says what was measured.
        $cpuNote = "build=$buildCommit layout=$Layout tiles=$tiles url=$Url dcomp=$DComp parallel_tiles=$($ParallelTiles.IsPresent) " +
            "pipeline=$(if($PipelineMode -eq ''){'playbin3'}else{$PipelineMode}) sink_pacing=$(if($SinkPacing -eq ''){'clock'}else{$SinkPacing}) " +
            "decoder_threads=$DecoderThreads numa_node=$NumaNode numa_pin=$(if($NoNumaPin){'off'}else{'on'}) " +
            "numa_render_pin=$(if($NoNumaRenderPin){'off'}else{'on'}) confine=$($Confine.IsPresent) escape=$(if($VideoEscape -eq ''){'off'}else{$VideoEscape})"
        # ONE quoted argument string, not an array: Start-Process on PowerShell 5.1 joins an
        # array with spaces WITHOUT quoting, so a path or note containing a space would split.
        # The note loses double quotes and trailing backslashes (either would end the quoting).
        $cpuNote = ($cpuNote -replace '"', "'") -replace '\\+$', ''
        $cpuArgs = "--pid $($proc.Id) --out `"$cpuStem`" --interval-ms $CpuProbeIntervalMs --warmup-sec 8"
        if ($CpuProbePdh) { $cpuArgs += " --pdh" }
        $cpuArgs += " --note `"$cpuNote`""
        # (2026-11-18) An earlier run's summary under the same stem would be printed as this
        # run's if this probe died before writing one. The probe deletes it too, first thing,
        # but only if it gets to run at all.
        if (Test-Path -LiteralPath $cpuSummary) {
            try {
                Remove-Item -LiteralPath $cpuSummary -Force -ErrorAction Stop
            } catch {
                Write-Warning "-CpuProbe: cannot delete the previous $cpuSummary ($($_.Exception.Message)); it will not be shown as this run's."
            }
        }
        try {
            $cpuProbeStart = Get-Date
            $cpuProbeProc = Start-Process -FilePath $cpuProbeExe -ArgumentList $cpuArgs -WindowStyle Hidden -PassThru
            # PS 5.1: without touching .Handle now, ExitCode can read empty once it has exited.
            $null = $cpuProbeProc.Handle
        } catch {
            $cpuProbeState = "nostart"
            Write-Warning "-CpuProbe could not start ${cpuProbeExe}: $($_.Exception.Message). Running WITHOUT the CPU probe."
        }
    }
}

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

# (2026-11-18) The probe's startup refusals (layout self-check, identical per-group LP times,
# CreateTime mismatch, cannot open the engine, cannot create its files) all exit within ~0.1 s.
# Say so NOW, not after a long run that turns out to carry no CPU data. (Review) This check sits
# AFTER -Confine and its wait is taken out of the -DurationSec sleep below, so a -CpuProbe run
# confines and lasts exactly like the same command without it. And if the ENGINE is what died,
# say that -- the probe ends with its target, which is not the probe's failure.
$cpuProbeWaitSec = 0.0
if ($cpuProbeProc) {
    $cpuWaitStart = Get-Date
    $cpuProbeExited = $cpuProbeProc.WaitForExit(1500)
    $cpuProbeWaitSec = ((Get-Date) - $cpuWaitStart).TotalSeconds
    if ($cpuProbeExited -and $proc.HasExited) {
        $cpuProbeState = "engine-exited"
        Write-Warning "winit_wall exited within $([int]($cpuProbeWaitSec * 1000)) ms of the CPU probe starting, and the probe ended with it (exit code $($cpuProbeProc.ExitCode)). Look at the engine log, not the probe."
    } elseif ($cpuProbeExited) {
        $cpuProbeState = "early"
        Write-Warning "-CpuProbe: wall_cpu_probe exited at startup (exit code $($cpuProbeProc.ExitCode)). This run has NO CPU data:"
        Write-CpuProbeError $cpuLog
    } else {
        $cpuProbeState = "running"
        Write-Host "  cpu_probe   : running, pid $($cpuProbeProc.Id) watching winit_wall pid $($proc.Id) -> $cpuStem.cpu.*"
    }
}

if ($DurationSec -gt 0) {
    Start-Sleep -Milliseconds ([int][Math]::Max(0, $DurationSec * 1000 - $cpuProbeWaitSec * 1000))
    # Wall tile windows ignore WM_CLOSE. env_logger writes stderr unbuffered, so a
    # force-kill does not lose log output.
    if (!$proc.HasExited) { Stop-Process -Id $proc.Id -Force }
    Start-Sleep -Seconds 2
} else {
    Write-Host "Running. To stop: Ctrl+C then  Stop-Process -Name winit_wall -Force"
    Wait-Process -Id $proc.Id
}

# (2026-11-18) The probe has already seen the engine exit. Give it up to 5 s to write its
# summary, then say where it is and print the machine and target lines. The 5 s start once the
# engine is really gone: after the -DurationSec force-kill its teardown can outlast the 2 s
# sleep above, and the probe only finishes after that. Only a summary written by THIS probe
# (newer than its start) is shown, and the warning says what actually happened.
if ($cpuProbeProc) {
    try { $null = $proc.WaitForExit(15000) } catch { }
    $cpuKilled = $false
    if (-not $cpuProbeProc.WaitForExit(5000)) {
        $cpuKilled = $true
        Write-Warning "wall_cpu_probe (pid $($cpuProbeProc.Id)) was still running 5 s after the engine exited; stopping it."
        Stop-Process -Id $cpuProbeProc.Id -Force -ErrorAction SilentlyContinue
    }
    $cpuSummaryFresh = (Test-Path -LiteralPath $cpuSummary) -and
        ((Get-Item -LiteralPath $cpuSummary).LastWriteTime -ge $cpuProbeStart)
    if ($cpuSummaryFresh) {
        Write-Host "--- CPU probe summary: $cpuSummary"
        Select-String -LiteralPath $cpuSummary -Pattern 'TOTAL \(sum of threads\)|process total|ALL PROCESSES, ALL GROUPS|per-LP busy, all groups|NO STEADY-STATE SAMPLES' |
            ForEach-Object { Write-Host $_.Line }
    } elseif ($cpuProbeState -eq "early") {
        Write-Warning "-CpuProbe: no CPU summary for this run; the probe exited at startup (see the warning when it started)."
    } elseif ($cpuProbeState -eq "engine-exited") {
        Write-Warning "-CpuProbe: no CPU summary worth reading; winit_wall exited right after launch (see the warning above)."
    } elseif ($cpuKilled) {
        Write-Warning "-CpuProbe: no CPU summary for this run; the probe had not finished 5 s after the engine exited and was stopped before writing one. Its per-interval lines up to then are in $cpuLog."
    } else {
        Write-Warning "-CpuProbe: wall_cpu_probe exited (exit code $($cpuProbeProc.ExitCode)) without writing $cpuSummary."
        Write-CpuProbeError $cpuLog
    }
}

# (2026-11-20) DWM state after the run: wait until the engine is really gone (after the
# -DurationSec force-kill its teardown can outlast the 2 s sleep), give DWM 3 s to release what
# the engine held, then record and print the difference.
if ($dwmPre) {
    try {
        try { $null = $proc.WaitForExit(15000) } catch { }
        Start-Sleep -Seconds 3
        $dwmPostResult = Write-DwmStatePost $dwmPre $dwmFile $dwmHistory (Split-Path -Leaf $runStem) $LogPath
        Write-Host "--- DWM state: $dwmFile  (history: $dwmHistory)"
        Write-Host "  after : $(Get-DwmSummary $dwmPostResult[0])"
        $dwmPostResult[1] | ForEach-Object { Write-Host $_ }
        if ($dwmPostResult[0].DwmPid -ne 0 -and $dwmPostResult[0].DwmPid -ne $dwmPre.DwmPid) {
            Write-Warning "DWM restarted during this run (pid $($dwmPre.DwmPid) -> $($dwmPostResult[0].DwmPid)). The Application log (provider Dwminit) says why; the wall output went black while it did."
        }
    } catch {
        Write-Warning "DWM state record (after the run) failed: $($_.Exception.Message)."
    }
}

# The engine is done, so the server has nothing left to answer. Kill it rather than leaving a
# python holding the port -- the NEXT -Serve run would die with "port in use".
if ($httpServer -and -not $httpServer.HasExited) {
    Stop-Process -Id $httpServer.Id -Force -ErrorAction SilentlyContinue
}


if ($cmdFile -and (Test-Path $cmdFile)) {
    Remove-Item $cmdFile -Force -ErrorAction SilentlyContinue
}

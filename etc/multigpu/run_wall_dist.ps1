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
    [ValidateSet("off", "on", "surface")]
    [string] $DComp = "surface",
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
    [switch] $NoImmediateComposite,
    # SERVO_FRAME_REASON_PROF=1: one line per second naming WHICH call site asked for each
    # composite. There are nine generate_frame call sites; gfx_refresh_hz only paces the
    # renderer tick (60Hz) and script runs its own 20/30ms timer, so the two together cap
    # around 110/s -- yet a single painter was measured at 200+/s. This says who.
    [switch] $FrameReason,
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

if ($Url -eq "") {
    $page = Join-Path $here "pages\html\video_grid_6x6_play.html"
    if (!(Test-Path $page)) { throw "bundled page not found: $page" }
    $Url = "file:///" + (($page -replace '\\', '/')) + "?rows=$Rows&cols=$Cols"
}
if ($NoSyncGroup)      { $SyncGroup = 0 }
elseif ($SyncGroup -le 0) { $SyncGroup = $Rows * $Cols }
if ($LogPath -eq "")  { $LogPath = Join-Path $here ("wall_{0}.err.log" -f (Get-Date -Format "yyyyMMdd_HHmmss")) }

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
if ($NoImmediateComposite) { $env:SERVO_DISABLE_VIDEO_IMMEDIATE_COMPOSITE = "1" }
if ($FrameReason)          { $env:SERVO_FRAME_REASON_PROF = "1" }
if ($PresentCadence)       { $env:SERVO_LOG_PRESENT_CADENCE = "1" }
if ($DcompDebug)           { $env:SERVO_DCOMP_DEBUG = "1" }
if ($VideoEscapeProf)      { $env:SERVO_VIDEO_ESCAPE_PROF = "1" }

$env:GST_PLUGIN_PATH            = ""
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = ""
$env:PATH = "$engine;$env:PATH"
if (-not $KeepRustLog) {
    $env:RUST_LOG = "warn,paint=info,media=info," +
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
if ($VideoEscape -ne "")  { $argList += @("--pref", "gfx_video_escape_mode=$VideoEscape") }
if ($VideoEscapeBuffers -gt 0) { $argList += @("--pref", "gfx_video_escape_buffer_count=$VideoEscapeBuffers") }
if ($SinkQos -ne "")      { $argList += @("--pref", "media_video_sink_qos=$SinkQos") }
if ($SinkPolicy -ne "")   { $argList += @("--pref", "media_video_sink_policy=$SinkPolicy") }
if ($SinkPacing -ne "")   { $argList += @("--pref", "media_video_sink_pacing=$SinkPacing") }
if ($NoAudio)             { $argList += @("--pref", "media_audio_enabled=false") }
if ($PipelineMode -ne "") { $argList += @("--pref", "media_pipeline_mode=$PipelineMode") }
if ($MaxPending -gt 0)    { $argList += @("--pref", "gfx_wall_frame_max_pending=$MaxPending") }
if ($MinIntervalMs -gt 0) { $argList += @("--pref", "gfx_wall_frame_min_interval_ms=$MinIntervalMs") }
$argList += $Url

Write-Host "Wall (pref-era) -- $tiles tiles requested by the page grid"
Write-Host "  layout=$layout"
Write-Host "  dcomp=$DComp tile_size=$TileSize refresh=${RefreshHz}Hz vsync=$($Vsync.IsPresent) escape=$(if($VideoEscape -eq ''){'off'}else{$VideoEscape}) escape_buffers=$(if($VideoEscapeBuffers -eq 0){'default(2)'}else{$VideoEscapeBuffers})"
Write-Host "  sync_group=$(if($SyncGroup -le 0){'off'}else{$SyncGroup}) decoder_threads=$DecoderThreads sink_qos=$(if($SinkQos -eq ''){'policy'}else{$SinkQos}) sink_policy=$(if($SinkPolicy -eq ''){'default'}else{$SinkPolicy}) sink_pacing=$(if($SinkPacing -eq ''){'clock'}else{$SinkPacing}) audio=$(if($NoAudio){'off'}else{'on'}) pipeline=$(if($PipelineMode -eq ''){'playbin3'}else{$PipelineMode})"
Write-Host "  d3d11_profile=$($D3d11Profile.IsPresent) video_rate=$($VideoRate.IsPresent) immediate_composite=$(if($NoImmediateComposite){'off'}else{'on'})$(if($PSBoundParameters.ContainsKey('D3d11ProfileMs')){" threshold=${D3d11ProfileMs}ms"}else{" threshold=8ms(default)"})"
Write-Host "  RUST_LOG=$env:RUST_LOG"
Write-Host "  log=$LogPath"

$proc = Start-Process -FilePath $exe -ArgumentList $argList -WorkingDirectory $here `
    -RedirectStandardError $LogPath -PassThru

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
            $threadLog = [IO.Path]::ChangeExtension($LogPath, ".threads.txt")
            Write-Host ""
            Write-Host "--- thread CPU breakdown (${window}s window, after ${ThreadCpuWarmupSec}s warmup) ---"
            # NOT Tee-Object: on Windows PowerShell 5.1 it writes UTF-16, and the
            # saved file then reads as spaced-out garbage in grep/less. The probe
            # emits pure ASCII, so say so.
            $probeOut = & $probe --pid $proc.Id --duration $window --top 20 2>&1
            $probeOut | ForEach-Object { Write-Host $_ }
            $probeOut | Out-File -FilePath $threadLog -Encoding ascii
            Write-Host "  saved: $threadLog"
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

if ($fanout -gt 0) { Write-Warning "GPU fan-out is broken -- tiles share one D3D11 device. See the warning text in the log." }
if ($egl -gt 0)    { Write-Warning "Some tiles could not wrap video textures ($egl occurrences) -- those tiles show green." }
exit 0

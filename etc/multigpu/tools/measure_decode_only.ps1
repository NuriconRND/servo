# Measures what software video decode costs on THIS machine, with nothing else running:
# no Servo, no compositor, no upload, no render. Pure GStreamer + avdec into fakesink.
#
# WHY: the wall's decode threads (multiqueue:src, where avdec_h264's chain function runs)
# pin at ~0.98 cores each from 36 videos up, while the machine as a whole sits at only
# ~68%. Two explanations fit that equally well and they need opposite fixes:
#
#   (i)  the sink is not throttling, so each decoder runs flat out at several times
#        realtime -- burning ~5x the CPU the content actually needs
#   (ii) the sink IS throttling and a single frame simply costs that much on this
#        machine (slower cores), or costs that much once memory bandwidth is contended
#
# One number decides it: the SINGLE-THREAD DECODE CEILING. Decode one clip unthrottled,
# time it, and you get max fps on one thread -- and therefore what 30fps ought to cost:
#
#   cores for 30fps = 30 / ceiling_fps
#
# If the wall's cores-per-video is close to that, the sink is throttling (ii). If it is
# several times higher, the decoders are running away (i). Measured on the dev box:
# ceiling 158fps, so 30fps costs 0.19 cores -- and a throttled pipeline measured 0.19.
#
# THE TRAP THIS SCRIPT USED TO FALL INTO (fixed 2026-08-25): the clip is 30.07s long and
# the default window was 5s warmup + 30s sampling, so every pipeline hit EOS *inside* the
# measurement window. The CPU delta then came out NEGATIVE and it printed
# "-1.87 cores busy" as a headline number with only a warning underneath. Any earlier
# reading from this script that did not also print the clip duration is suspect.
#
# Pure ASCII on purpose (a Korean launcher once failed to parse on a test machine that
# decodes with a legacy console codepage).
#
# Usage:
#   etc\multigpu\tools\measure_decode_only.ps1 -Count 45
#   etc\multigpu\tools\measure_decode_only.ps1 -Count 45 -MaxThreads 2
#   etc\multigpu\tools\measure_decode_only.ps1 -CeilingOnly
#   etc\multigpu\tools\measure_decode_only.ps1 -Count 45 -SingleProcess
#   etc\multigpu\tools\measure_decode_only.ps1 -Count 45 -SingleProcess -PaceWithSleep
#   etc\multigpu\tools\measure_decode_only.ps1 -Count 45 -GstRoot D:\gstreamer\1.0\msvc_x86_64

param(
    [int]    $Count = 45,
    # 0 = pick automatically so the window ends before the clip does.
    [int]    $DurationSec = 0,
    # avdec max-threads. The wall passes media_avdec_max_threads=1, so 1 is the
    # comparable setting; vary it here to see the tradeoff without the wall in the way.
    [int]    $MaxThreads = 1,
    [string] $GstRoot = "F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64",
    [string] $Video = "",
    # Only measure the single-thread ceiling and stop. Fast (one pipeline, one pass).
    [switch] $CeilingOnly,
    [int]    $WarmupSec = 5,
    # Put all N decode chains in ONE gst-launch process instead of N processes.
    #
    # This is the discriminator for the remaining gap. N separate processes each get
    # their own heap and their own GstSystemClock; the wall has all N streaming threads
    # in one address space sharing one allocator and one clock singleton, and with
    # sync=true every frame does a clock wait. Measured: 45 processes cost 0.399
    # cores/video while the wall costs 0.98 for the same decode. If one process also
    # costs ~1.0, the cost is in-process sharing and not Servo's code; if it stays
    # ~0.4, it is Servo-specific.
    [switch] $SingleProcess,
    # Split the chains across N processes instead of 1 or Count. 0 = ignore this and use
    # -SingleProcess / one-per-chain as before.
    #
    # ***THE PROCESS BOUNDARY IS WORTH 2.4x AND NOBODY KNOWS WHY.*** Measured 2026-08-27,
    # 54 x FHD30, identical topology and pacing, same machine:
    #
    #   54 processes, 1 chain each : 0.418 cores per video  (1.19x the single-thread ceiling)
    #   1 process, 54 chains       : 1.005 cores per video  (2.83x)
    #
    # Servo's wall sits at 0.99-1.37, i.e. right on the single-process figure -- so the whole
    # gap the wall has been chasing is this, not anything Servo does. The candidates are heap
    # contention (54 decoders sharing one allocator, ~1620 frame buffers a second), GStreamer's
    # per-process globals (caps interning, registry, GType locks -- the same family as the
    # shared GstSystemClock), and NUMA first-touch putting one process's heap on one node while
    # its threads spread over both.
    #
    # This knob draws the curve that separates them: if cost per video falls smoothly as chains
    # per process drop, it is contention that eases; if it steps, it is structural.
    [int]    $Processes = 0,
    # Confine every launched process to one NUMA node's processors. -1 = do not (default).
    #
    # ***This is a HARD affinity, not the preference `start /NODE` gives on its own.*** That
    # distinction is the whole point: the wall already asks for its GPU's node and still spills,
    # because a process wanting 54 cores cannot fit in a node's 40 -- so its threads cross to the
    # far socket while its heap stays first-touched on the near one, which is the worst of both.
    #
    # The 2026-08-27 process-split curve says the penalty is a STEP, not a slope: 54 chains in
    # one process cost 1.001 cores each, 27 chains in each of two cost 0.478, and squeezing on
    # down to 1 chain per process only reaches 0.412. Chains per process is not the variable;
    # the number of processes is. That kills the "shared heap / global lock contention" reading
    # and points at per-process placement instead.
    #
    # Which leaves a chicken-and-egg to break: inefficient => needs 54 cores => spills => remote
    # memory => stays inefficient. At 0.412 the same 54 videos want 22 cores and fit in a node
    # with room to spare. Forbidding the spill is how to find out which way it settles.
    [int]    $NumaNode = -1,
    # Pace with `identity sleep-time` and sync=false instead of the sink's clock wait.
    #
    # Splits the in-process penalty. One process holding 45 chains cost 0.795
    # cores/video against 0.399 for 45 separate processes -- 2x for nothing but
    # sharing an address space. The one thing every chain touches on every frame is
    # the GstSystemClock singleton (obtain() returns one per process) via the sink's
    # sync=true wait. identity's sleep-time just sleeps the streaming thread, so it
    # paces without any clock: if the cost falls back toward 0.4 the clock is the
    # contention point; if it stays near 0.8 it is the allocator or the task pool.
    [switch] $PaceWithSleep,
    # Insert the elements playbin3 puts around the decoder: a multiqueue in front and a
    # queue behind, so the decoder runs on a multiqueue src pad task exactly as it does
    # in the wall.
    #
    # THIS MATTERS AND WAS MISSED ONCE: the wall runs playbin3 = urisourcebin +
    # decodebin3, which is why its decode threads are named multiqueue:src. The plain
    # chain has no multiqueue at all, so its numbers (0.399 / 0.795 / 0.284 cores per
    # video) do NOT model the wall. The wall's 0.98 sitting ABOVE the single-process
    # 0.795 was the tell, and it was read as a Servo cost instead.
    [switch] $WallTopology,
    # Demux audio as well. Both branches get a queue.
    #
    # ***MEASURED 2026-08-25: a queue on the audio branch ALONE deadlocks.*** It was tried,
    # because the expensive hop is on the video branch (3.1MB raw frames crossing a thread
    # boundary) while audio buffers are kilobytes, so queueing only audio looked like a way
    # to keep audio and still drop the video hop. It does not even start:
    #
    #   d.video_0 ! h264parse ! avdec_h264 ! fakesink
    #   d.audio_0 ! queue ! aacparse ! avdec_aac ! fakesink
    #   => Pipeline is PREROLLING ... and never reaches PLAYING
    #
    # The demuxer has one streaming thread; it blocks in the un-queued video branch and can
    # therefore never push the first audio buffer, so the audio sink never prerolls. Setting
    # async=false on the video sink does NOT help (also measured). Only a queue on BOTH
    # branches prerolls, plays and reaches EOS. That is what decodebin3 uses multiqueue for.
    #
    # The plan survives anyway, because the two hops carry different things: the queue that
    # audio needs sits right after the demuxer and carries COMPRESSED data (~40KB/frame),
    # while the expensive one is playsink's queue AFTER the decoder (3.1MB/frame). Dropping
    # the latter keeps audio working.
    #
    # Note the wall does not decode audio today (videos are muted, no audio decoder thread
    # exists) yet still pays for multiqueue's audio pads -- 4 pad tasks per video.
    [switch] $WithAudio,
    # Use a high-level bin instead of a hand-built chain, to compare what the wall
    # actually runs against what it could run.
    #
    #   playbin3       = urisourcebin + decodebin3 + PLAYSINK   <- what the wall runs
    #   uridecodebin3  = urisourcebin + decodebin3              <- no playsink
    #
    # The expensive hop measured so far is the queue AFTER the decoder, and that queue
    # belongs to playsink, not to decodebin3. If uridecodebin3 lands near the hand-built
    # chain, the wall can keep decodebin3's codec autoplug (H264/H265/VP9 for free) and
    # still lose the hop -- no hand-built pipeline needed.
    #
    # Hardware decoders are demoted to match the wall, which promotes avdec.
    [ValidateSet('', 'playbin3', 'uridecodebin3')]
    [string] $HighLevel = ''
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
if ($Video -eq "") {
    foreach ($candidate in @(
        (Join-Path $repo "tests\Wildlife_FHD30fps_counter_10Mbitrate.mp4"),
        (Join-Path $PSScriptRoot "..\..\..\pages\Wildlife_FHD30fps_counter_10Mbitrate.mp4"),
        (Join-Path (Get-Location) "pages\Wildlife_FHD30fps_counter_10Mbitrate.mp4")
    )) {
        if (Test-Path $candidate) { $Video = (Resolve-Path $candidate).Path; break }
    }
}

$launch = Join-Path $GstRoot "bin\gst-launch-1.0.exe"
$launchName = "gst-launch-1.0"   # StartDecode 가 새 프로세스를 이름으로 찾는다
$discover = Join-Path $GstRoot "bin\gst-discoverer-1.0.exe"
if (!(Test-Path $launch)) { throw "gst-launch-1.0.exe not found: $launch" }
if ($Video -eq "" -or !(Test-Path $Video)) { throw "video not found (pass -Video): $Video" }

$gstBin = Join-Path $GstRoot "bin"
$env:PATH = "$gstBin;$env:PATH"
$env:GST_PLUGIN_PATH = ""
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = Join-Path $GstRoot "lib\gstreamer-1.0"

# The hand-built chains name avdec_h264 outright, but a high-level bin autoplugs and would
# pick the D3D11 hardware decoder -- which decodes on the GPU and measures nothing. The wall
# demotes exactly these and promotes avdec, so match it or the comparison is meaningless.
if ($HighLevel -ne '') {
    $env:GST_PLUGIN_FEATURE_RANK = (@(
        'd3d11h264dec', 'd3d11h265dec', 'd3d11vp9dec', 'd3d11vp8dec', 'd3d11mpeg2dec',
        'd3d11h264device1dec', 'd3d11h264device2dec', 'd3d11h264device3dec',
        'd3d11h265device1dec', 'd3d11h265device2dec', 'd3d11h265device3dec'
    ) | ForEach-Object { "${_}:NONE" }) -join ','
}

$version = (& $launch --version 2>&1 | Select-Object -First 1)
$cores = (Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors
$physical = ((Get-CimInstance Win32_Processor | Measure-Object -Property NumberOfCores -Sum).Sum)

# --- clip duration: the window MUST end before EOS or the numbers are garbage ---
$clipSec = 0.0
if (Test-Path $discover) {
    $line = (& $discover $Video 2>&1 | Select-String -Pattern "^\s*Duration:" | Select-Object -First 1)
    if ($line -and $line -match "(\d+):(\d+):(\d+)\.(\d+)") {
        $clipSec = [double]$Matches[1] * 3600 + [double]$Matches[2] * 60 + [double]$Matches[3] +
                   [double]("0." + $Matches[4])
    }
}

Write-Host "decode-only baseline"
Write-Host "  gstreamer   : $version"
Write-Host "  video       : $Video"
Write-Host ("  clip length : {0}" -f $(if ($clipSec -gt 0) { "{0:N2}s" -f $clipSec } else { "UNKNOWN (gst-discoverer missing)" }))
Write-Host "  cpus        : $cores logical / $physical physical"
Write-Host ""

$videoUri = $Video -replace '\\', '/'
# 29.97fps -> 33366 microseconds per frame.
$sleepUs = [int](1000000.0 * 1001.0 / 30000.0)
# Each chain needs its own demuxer name so N of them can share one process.
$script:chainIndex = 0
function DecodePipeline($sync) {
    if ($HighLevel -ne '') {
        $sinkArgs = if ($PaceWithSleep -and $sync -eq 'true') {
            @("identity", "sleep-time=$sleepUs", "!", "fakesink", "sync=false")
        } else {
            @("fakesink", "sync=$sync")
        }
        if ($HighLevel -eq 'playbin3') {
            # video-sink takes a bin description; audio is discarded either way.
            return @("playbin3", "uri=file:///$videoUri",
                     "video-sink=$($sinkArgs -join ' ')", "audio-sink=fakesink sync=false")
        }
        return @("uridecodebin3", "uri=file:///$videoUri", "!") + $sinkArgs
    }
    $d = 'd' + $script:chainIndex
    $script:chainIndex++
    $chain = if ($WithAudio) {
        # The video branch needs its own queue too -- see the -WithAudio note above. It sits
        # before the parser/decoder so it only ever carries compressed data.
        @("filesrc", "location=$videoUri", "!", "qtdemux", "name=$d", "$d.video_0", "!",
          "queue", "!", "h264parse", "!")
    } else {
        @("filesrc", "location=$videoUri", "!", "qtdemux", "!", "h264parse", "!")
    }
    # multiqueue uses request pads; gst-launch requests them automatically.
    if ($WallTopology) { $chain += @("multiqueue", "!") }
    $chain += @("avdec_h264", "max-threads=$MaxThreads", "!")
    if ($WallTopology) { $chain += @("queue", "!") }
    if ($PaceWithSleep -and $sync -eq "true") {
        # Pace inside the streaming thread, never touching the clock.
        $chain += @("identity", "sleep-time=$sleepUs", "!", "fakesink", "sync=false")
    } else {
        $chain += @("fakesink", "sync=$sync")
    }
    if ($WithAudio) {
        $chain += @("$d.audio_0", "!", "queue", "!", "aacparse", "!", "avdec_aac", "!",
                    "fakesink", "sync=$sync")
    }
    $chain
}

# --- 1. single-thread ceiling: decode the whole clip as fast as one thread can ---
Write-Host "[1] single-thread ceiling (one pipeline, unthrottled)"
$sw = [Diagnostics.Stopwatch]::StartNew()
& $launch @(DecodePipeline "false") 2>&1 | Out-Null
$sw.Stop()
$elapsed = $sw.Elapsed.TotalSeconds
$ceilingFps = 0.0
$coresFor30 = 0.0
if ($clipSec -gt 0 -and $elapsed -gt 0) {
    $frames = $clipSec * 30000.0 / 1001.0
    $ceilingFps = $frames / $elapsed
    $coresFor30 = (30000.0 / 1001.0) / $ceilingFps
    Write-Host ("  decoded {0:N0} frames in {1:N2}s = {2:N0} fps on ONE thread ({3:N1}x realtime)" -f $frames, $elapsed, $ceilingFps, ($clipSec / $elapsed))
    Write-Host ("  => throttled 29.97fps should cost about {0:N2} cores per video" -f $coresFor30)
    Write-Host ("  => a decode thread pinned at 1.00 core is running about {0:N1}x realtime" -f ($ceilingFps / (30000.0 / 1001.0)))
} else {
    Write-Host ("  decoded the clip in {0:N2}s (clip length unknown, cannot convert to fps)" -f $elapsed)
}
Write-Host ""
if ($CeilingOnly) { exit 0 }

# --- 2. N pipelines at playback rate ---
# The window has to fit inside the clip. Leave 3s of margin: pipelines do not all preroll
# at the same instant, and one EOS inside the window poisons the whole measurement.
if ($DurationSec -le 0) {
    $DurationSec = if ($clipSec -gt 0) { [int][math]::Floor($clipSec - $WarmupSec - 3) } else { 20 }
}
if ($clipSec -gt 0 -and ($WarmupSec + $DurationSec + 3) -gt $clipSec) {
    throw ("-DurationSec $DurationSec plus a ${WarmupSec}s warmup runs past the end of a {0:N2}s clip. " -f $clipSec) +
          "Every pipeline would hit EOS inside the window and the CPU delta would be meaningless. Use -DurationSec $([int][math]::Floor($clipSec - $WarmupSec - 3)) or a longer clip."
}
if ($DurationSec -lt 5) { throw "clip too short to measure ($DurationSec s of usable window)" }

# Say what was actually measured. A run that does not name its own configuration is how
# `-D3d11ProfileMs 0` got silently ignored for a whole round of measurements.
Write-Host "[2] $Count pipelines at playback rate"
# Spell out the chain that was actually built, including what -WithAudio inserts.
$topologyLine = ""
if ($HighLevel -eq 'playbin3') {
    $topologyLine = "playbin3 (urisourcebin + decodebin3 + PLAYSINK) ! sink   (what the wall runs)"
} elseif ($HighLevel -eq 'uridecodebin3') {
    $topologyLine = "uridecodebin3 (urisourcebin + decodebin3, no playsink) ! sink"
}
if ($topologyLine -eq "") {
$topologyLine = "filesrc ! qtdemux ! "
if ($WithAudio)     { $topologyLine += "queue ! " }
$topologyLine += "h264parse ! "
if ($WallTopology)  { $topologyLine += "multiqueue ! " }
$topologyLine += "avdec_h264 ! "
if ($WallTopology)  { $topologyLine += "queue ! " }
$topologyLine += "sink"
if ($WallTopology)  { $topologyLine += "   (as playbin3 does)" }
elseif (-not $WithAudio) { $topologyLine += "   (plain; NOT what the wall runs)" }
}
Write-Host ("      topology  : {0}" -f $topologyLine)
Write-Host ("      audio     : {0}" -f $(if ($WithAudio) {
    "demuxed too; a queue on BOTH branches (audio-only queue deadlocks -- see -WithAudio)"
} else {
    "not demuxed (video branch only)"
}))
Write-Host ("      affinity  : {0}" -f $(if ($NumaNode -ge 0) { "HARD, NUMA node $NumaNode only" } else { "none (Windows places it)" }))
Write-Host ("      pacing    : {0}" -f $(if ($PaceWithSleep) { "identity sleep-time, NO clock" } else { "sink sync=true (shared GstSystemClock)" }))
# -Processes N wins over -SingleProcess when both are given; N is clamped to [1, Count].
$groupCount = if ($Processes -gt 0) { [math]::Min([math]::Max($Processes, 1), $Count) }
              elseif ($SingleProcess) { 1 } else { $Count }

# Launch one gst-launch, optionally confined to a NUMA node.
#
# `start /NODE n` alone only expresses a preference; adding /AFFINITY makes it a hard mask, and
# the mask is interpreted WITHIN that node, so all-ones covers exactly that node's processors.
# The wrapper .cmd exists because the redirects have to belong to gst-launch, and `cmd /c` (not
# the .cmd path) so the wrapper exits instead of lingering as a `cmd /K` holding this folder.
Add-Type -Namespace Win32 -Name Topo2 -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern uint GetActiveProcessorCount(ushort g);
'@ -ErrorAction SilentlyContinue
function StartDecode($argList, $tag) {
    if ($NumaNode -lt 0) {
        return Start-Process -FilePath $launch -ArgumentList $argList -WindowStyle Hidden -PassThru `
            -RedirectStandardOutput "$env:TEMP\decode_$tag.out" -RedirectStandardError "$env:TEMP\decode_$tag.err"
    }
    $n = [Win32.Topo2]::GetActiveProcessorCount([System.UInt16]$NumaNode)
    if ($n -le 0 -or $n -gt 64) { throw "NUMA node $NumaNode reports $n processors; cannot build an affinity mask" }
    $mask = ([bigint]1 -shl [int]$n) - 1
    $args = ($argList | ForEach-Object { '"' + $_ + '"' }) -join ' '
    $cmdFile = Join-Path $env:TEMP ("decode_numa_{0}_{1}.cmd" -f $PID, $tag)
    @"
@echo off
start "" /NODE $NumaNode /AFFINITY 0x$($mask.ToString('x')) /B /WAIT "$launch" $args > "$env:TEMP\decode_$tag.out" 2> "$env:TEMP\decode_$tag.err"
"@ | Set-Content -Encoding ascii $cmdFile
    $before = @(Get-Process -Name ($launchName) -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
    Start-Process cmd -ArgumentList "/c", "`"$cmdFile`"" -WindowStyle Hidden | Out-Null
    for ($i = 0; $i -lt 100; $i++) {
        Start-Sleep -Milliseconds 100
        $now = @(Get-Process -Name ($launchName) -ErrorAction SilentlyContinue | Where-Object { $before -notcontains $_.Id })
        if ($now.Count -gt 0) { return $now[0] }
    }
    throw "gst-launch did not start within 10s under 'start /NODE $NumaNode'"
}

$perProc = [math]::Ceiling($Count / [double]$groupCount)
Write-Host ("      processes : {0}   max-threads={1}" -f $(
    if ($groupCount -eq 1) { "1 holding $Count chains" }
    elseif ($groupCount -eq $Count) { "$Count, one per chain" }
    else { "$groupCount, about $perProc chains each" }), $MaxThreads)
Write-Host "  window      : ${WarmupSec}s warmup + ${DurationSec}s sample"

$procs = @()
if ($groupCount -ne 1 -and $groupCount -ne $Count) {
    # Deal the chains out round-robin so every process gets within one chain of the same load.
    $buckets = @{}
    for ($i = 0; $i -lt $Count; $i++) {
        $b = $i % $groupCount
        if (-not $buckets.ContainsKey($b)) { $buckets[$b] = @() }
        $buckets[$b] += DecodePipeline "true"
    }
    foreach ($b in ($buckets.Keys | Sort-Object)) {
        $procs += StartDecode $buckets[$b] $b
    }
} elseif ($groupCount -eq 1) {
    # gst-launch builds ONE pipeline; several disconnected chains in one argument list all
    # run inside it, which is exactly the in-process arrangement the wall has.
    $chains = @()
    for ($i = 0; $i -lt $Count; $i++) { $chains += DecodePipeline "true" }
    $procs += StartDecode $chains 0
} else {
    for ($i = 0; $i -lt $Count; $i++) {
        $procs += StartDecode (DecodePipeline "true") $i
    }
}
Start-Sleep -Seconds $WarmupSec

$t0 = Get-Date
$cpu0 = 0.0
$startAlive = 0
foreach ($p in $procs) { try { $cpu0 += (Get-Process -Id $p.Id).TotalProcessorTime.TotalSeconds; $startAlive++ } catch {} }

# Machine-wide CPU by summing every process's CPU time and differencing it.
#
# NOT Get-Counter '\Processor(_Total)\% Processor Time': a single Get-Counter sample is
# computed over an interval it picks itself, and the results were nonsense -- four runs
# consuming 42 / 36 / 45 / 13 cores of 80 reported 13.6% / 66.5% / 21.5% / 0.8%. The last
# claimed 0.8% while its own process was using 16.5%. The counter path is localised on
# non-English Windows too.
#
# NOT Win32_PerfRawData_PerfOS_Processor either: exact and locale-independent, but the CIM
# query itself took about ten seconds a call, which stretched a 12s window to 31.9s and
# ran past the end of the clip. Measured, then reverted.
#
# Summing Get-Process is fast (<100ms) and needs no perf counters. It undercounts slightly
# because a few system processes are not readable, so it is a floor, not a total.
#
# The loop is bounded by the CLOCK, not by an iteration count: Get-Counter used to block
# for about a second on its own, so `for (22) { Start-Sleep 1; Get-Counter }` ran ~44s, not
# 22 -- past the end of a 30.07s clip, so every pipeline hit EOS and three measurements in
# a row came back empty. The guard above only checks the NOMINAL window.
function AllProcessCpu {
    (Get-Process -EA SilentlyContinue | ForEach-Object {
        try { $_.TotalProcessorTime.TotalSeconds } catch { 0 }
    } | Measure-Object -Sum).Sum
}
$allCpu0 = AllProcessCpu
$loop = [Diagnostics.Stopwatch]::StartNew()
while ($loop.Elapsed.TotalSeconds -lt $DurationSec) { Start-Sleep -Milliseconds 250 }
$allCpu1 = AllProcessCpu

$t1 = Get-Date
$cpu1 = 0.0
$alive = 0
foreach ($p in $procs) {
    try { $cpu1 += (Get-Process -Id $p.Id).TotalProcessorTime.TotalSeconds; $alive++ } catch {}
}
$wall = ($t1 - $t0).TotalSeconds
if ($clipSec -gt 0 -and ($WarmupSec + $wall) -gt $clipSec) {
    Write-Warning ("the sampling window actually ran {0:N1}s (asked for {1}s); with a {2:N1}s warmup that passes the end of a {3:N2}s clip." -f $wall, $DurationSec, $WarmupSec, $clipSec)
}
foreach ($p in $procs) { try { Stop-Process -Id $p.Id -Force -EA SilentlyContinue } catch {} }
Remove-Item "$env:TEMP\decode_*.out" -Force -EA SilentlyContinue
Remove-Item "$env:TEMP\decode_*.err" -Force -EA SilentlyContinue
Remove-Item "$env:TEMP\decode_numa_*.cmd" -Force -EA SilentlyContinue

# A pipeline that exits mid-window takes its CPU total with it, which drives the delta
# negative. Refuse to report rather than print a number that looks like data.
if ($alive -lt $startAlive) {
    Write-Host ""
    Write-Warning "$($startAlive - $alive) of $startAlive pipeline(s) exited during the window -- NO VALID RESULT."
    Write-Warning "The clip is $("{0:N2}" -f $clipSec)s; shorten the window (-DurationSec) or use a longer clip."
    exit 1
}

$busyCores = ($cpu1 - $cpu0) / $wall
Write-Host ""
Write-Host ("results over {0:N1}s ({1})" -f $wall, $(
    if ($groupCount -eq 1) { "1 process holding $Count chains" }
    elseif ($groupCount -eq $Count) { "$alive/$Count pipelines alive throughout" }
    else { "$alive/$groupCount processes alive, $Count chains total" }))
Write-Host ("  decode CPU        : {0:N2} cores busy  ({1:N1}% of {2} logical)" -f $busyCores, (100 * $busyCores / $cores), $cores)
# With several chains per process, a live process still carries all of its chains, so the
# per-video divisor is the chain count, not the process count.
$chainCount = if ($groupCount -eq $Count) { [math]::Max($alive, 1) } else { $Count }
Write-Host ("  per video         : {0:N3} cores" -f ($busyCores / $chainCount))
if ($allCpu0 -and $allCpu1 -and ($allCpu1 -gt $allCpu0)) {
    $allCores = ($allCpu1 - $allCpu0) / $wall
    Write-Host ("  all processes     : {0:N2} cores busy ({1:N1}% of {2}); this run is {3:N0}% of it" -f $allCores, (100 * $allCores / $cores), $cores, (100 * $busyCores / [math]::Max($allCores, 0.01)))
}

if ($coresFor30 -gt 0) {
    $perVideo = $busyCores / $chainCount
    $ratio = $perVideo / $coresFor30
    Write-Host ""
    Write-Host ("  vs the ceiling    : {0:N3} measured / {1:N3} expected = {2:N2}x" -f $perVideo, $coresFor30, $ratio)
    if ($ratio -lt 1.4) {
        Write-Host "  -> throttling works here. Compare this per-video figure against the WALL's"
        Write-Host "     multiqueue:src cores-per-video (thread_cpu_probe). If the wall is much"
        Write-Host "     higher, the wall's sink is what fails to throttle, not the decoder."
    } else {
        Write-Host "  -> even standalone, decode costs more than the ceiling implies: the machine is"
        Write-Host "     contended (memory bandwidth / SMT), not just running unthrottled."
    }
}
exit 0

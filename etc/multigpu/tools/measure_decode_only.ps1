# Measures the CPU cost of N simultaneous software video decodes with NOTHING else running:
# no Servo, no compositor, no upload, no render. Pure GStreamer + avdec into fakesink.
#
# WHY: the wall saturates CPU at ~45 videos while a (non-Servo) reference wall does 54 with
# headroom, and GPU utilization DROPS as fps falls -- i.e. the GPU is starved and the
# bottleneck is upstream. But decode cost and Servo's per-frame work are measured together
# in the wall, so "is the decoder expensive?" is still unanswered. This gives the floor.
#
#   floor            = what N decodes cost by themselves (this script)
#   wall total - floor = what Servo adds per frame (upload, image fanout, scene, present)
#
# Run the SAME N as the wall test so the numbers subtract cleanly.
#
# Pure ASCII on purpose (a Korean launcher once failed to parse on a test machine that
# decodes with a legacy console codepage).
#
# Usage:
#   etc\multigpu\tools\measure_decode_only.ps1 -Count 45
#   etc\multigpu\tools\measure_decode_only.ps1 -Count 45 -MaxThreads 2
#   etc\multigpu\tools\measure_decode_only.ps1 -Count 45 -GstRoot D:\gstreamer\1.0\msvc_x86_64

param(
    [int]    $Count = 45,
    [int]    $DurationSec = 30,
    # avdec max-threads. The wall uses 1 (auto-threading spawned ~700 threads at 36 tiles
    # and thrashed the scheduler -- see the wall notes). Vary it here to see the tradeoff
    # WITHOUT the wall's other costs in the way.
    [int]    $MaxThreads = 1,
    # Point at a different GStreamer to compare runtimes (e.g. 1.22.8 vs 1.28.4.100).
    [string] $GstRoot = "F:\gstreamer-inhouse\1.28.4.100\1.0\msvc_x86_64",
    [string] $Video = "",
    # Decode as fast as possible instead of at playback rate. Off by default: the wall
    # plays at 30fps, so sync=true is the comparable measurement.
    [switch] $Unthrottled
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
if ($Video -eq "") { $Video = Join-Path $repo "tests\Wildlife_FHD30fps_counter_10Mbitrate.mp4" }

$launch = Join-Path $GstRoot "bin\gst-launch-1.0.exe"
if (!(Test-Path $launch)) { throw "gst-launch-1.0.exe not found: $launch" }
if (!(Test-Path $Video))  { throw "video not found: $Video" }

$gstBin = Join-Path $GstRoot "bin"
$env:PATH = "$gstBin;$env:PATH"
$env:GST_PLUGIN_PATH = ""
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = Join-Path $GstRoot "lib\gstreamer-1.0"

$version = (& $launch --version 2>&1 | Select-Object -First 1)
$cores = (Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors

Write-Host "decode-only baseline"
Write-Host "  gstreamer   : $version"
Write-Host "  gst root    : $GstRoot"
Write-Host "  video       : $Video"
Write-Host "  count       : $Count   max-threads=$MaxThreads   sync=$(-not $Unthrottled)"
Write-Host "  logical cpus: $cores"
Write-Host ""

# filesrc -> demux -> parse -> avdec -> fakesink. No conversion, no upload, no display.
# sync=true on the sink makes it decode at playback rate, which is what the wall does.
$sync = if ($Unthrottled) { "false" } else { "true" }
$videoUri = $Video -replace '\\', '/'
$pipeline = @(
    "filesrc", "location=$videoUri", "!",
    "qtdemux", "!", "h264parse", "!",
    "avdec_h264", "max-threads=$MaxThreads", "!",
    "fakesink", "sync=$sync"
)

$procs = @()
for ($i = 0; $i -lt $Count; $i++) {
    $procs += Start-Process -FilePath $launch -ArgumentList $pipeline `
        -WindowStyle Hidden -PassThru -RedirectStandardOutput "$env:TEMP\decode_$i.out" `
        -RedirectStandardError "$env:TEMP\decode_$i.err"
}
Write-Host "started $($procs.Count) decode pipelines; warming up 5s..."
Start-Sleep -Seconds 5

# Sample total CPU of the decode processes only (not the whole machine), plus machine-wide
# for context. Per-process CPU time deltas are more reliable here than the perf counter.
$t0 = Get-Date
$cpu0 = 0.0
foreach ($p in $procs) { try { $cpu0 += (Get-Process -Id $p.Id).TotalProcessorTime.TotalSeconds } catch {} }

$samples = @()
for ($s = 0; $s -lt $DurationSec; $s++) {
    Start-Sleep -Seconds 1
    try { $samples += (Get-Counter '\Processor(_Total)\% Processor Time' -EA Stop).CounterSamples[0].CookedValue } catch {}
}

$t1 = Get-Date
$cpu1 = 0.0
$alive = 0
foreach ($p in $procs) {
    try { $cpu1 += (Get-Process -Id $p.Id).TotalProcessorTime.TotalSeconds; $alive++ } catch {}
}
$wall = ($t1 - $t0).TotalSeconds
$busyCores = ($cpu1 - $cpu0) / $wall

foreach ($p in $procs) { try { Stop-Process -Id $p.Id -Force -EA SilentlyContinue } catch {} }
Remove-Item "$env:TEMP\decode_*.out","$env:TEMP\decode_*.err" -Force -EA SilentlyContinue

Write-Host ""
Write-Host "results over $([math]::Round($wall,1))s (pipelines still alive: $alive/$Count)"
Write-Host ("  decode CPU        : {0:N2} cores busy  ({1:N1}% of {2})" -f $busyCores, (100 * $busyCores / $cores), $cores)
Write-Host ("  per video         : {0:N3} cores" -f ($busyCores / [math]::Max($alive,1)))
if ($samples.Count -gt 0) {
    $avg = ($samples | Measure-Object -Average).Average
    Write-Host ("  machine-wide CPU  : {0:N1}% avg" -f $avg)
}
Write-Host ""
Write-Host "Compare against the wall running the same count. The difference is what Servo"
Write-Host "adds per frame (upload, image fanout, scene build, present) on top of decode."
if ($alive -lt $Count) {
    Write-Warning "$($Count - $alive) pipeline(s) exited early -- check $env:TEMP\decode_*.err next run (this run deleted them). The clip may have ended; use a longer clip or -Unthrottled off."
}
exit 0

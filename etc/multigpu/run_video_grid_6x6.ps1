param(
    [ValidateSet("release", "debug")]
    [string] $Profile = "release",
    [int]    $Grid = 6,          # square N x N when -Cols/-Rows not given
    [int]    $Cols = 0,          # rectangular grid: columns (0 = use -Grid)
    [int]    $Rows = 0,          # rectangular grid: rows    (0 = use -Grid)
    [string] $WindowSize = "1920x1080",
    [int]    $DurationSec = 0,
    [switch] $Detach,
    [string] $LogPrefix = "video_grid_6x6"
)

# Launch the 6x6 (default) independent-<video> grid perf test in a SINGLE servoshell
# window on one monitor (no wall layout).
#
# Metric target (6x6): rAF FPS ~= 60, decoded/s ~= 1080 (30fps x 36), dropped/s ~= 0.
#
# Notes:
#  - No --wall-layout / --wall-all-tiles: this is a single-monitor test, not the wall.
#  - --window-size sets the initial logical window size; move/maximize onto the target
#    monitor as needed. Pass -WindowSize to match the monitor (e.g. 3840x2160 for 4K).
#  - SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF=1: the gstreamer servosrc EnoughData/NeedData
#    backoff otherwise pauses file-byte pushing periodically (~400ms stutter every ~8-12s),
#    starving decoded-frame delivery. Disabling it keeps a steady feed. Safe for a LOCAL
#    file (bounded by MAX_BUFFER_SIZE); do not blanket-enable for network streams.
#  - The page is plain HTML (no ES modules), so file:// works -- no HTTP server needed.
$ErrorActionPreference = "Stop"

$servoRoot  = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$servoExe   = Join-Path $servoRoot "target\$Profile\servoshell.exe"
$pagePath   = Join-Path $servoRoot "tests\html\video_grid_6x6_perf.html"
$logDir     = Join-Path $servoRoot "target\multigpu_logs"
$timestamp  = Get-Date -Format "yyyyMMdd_HHmmss"

if (!(Test-Path $servoExe)) { throw "servoshell.exe not found: $servoExe" }
if (!(Test-Path $pagePath)) { throw "Page not found: $pagePath" }
New-Item -ItemType Directory -Path $logDir -Force | Out-Null

# Rectangular grid when -Cols/-Rows are supplied, otherwise square -Grid.
if ($Cols -gt 0 -or $Rows -gt 0) {
    $c = if ($Cols -gt 0) { $Cols } else { $Grid }
    $r = if ($Rows -gt 0) { $Rows } else { $Grid }
    $query = "?cols=$c&rows=$r"
    $gridDesc = "${c}x${r} = $($c * $r) tiles"
} else {
    $query = "?grid=$Grid"
    $gridDesc = "${Grid}x${Grid} = $($Grid * $Grid) tiles"
}
$url = "file:///" + ($pagePath -replace '\\', '/') + $query

# Smooth-delivery knob (see header).
$env:SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF = "1"
# Cap each software H.264 decoder (avdec_h264) to 1 worker thread. With 36 tiles the
# default auto-threading spawns ~CPU-count threads PER decoder (~700+ total on a 20-core
# box), thrashing the scheduler so the compositor/rAF misses its 60fps deadline (FPS
# jitter) and inflating decoded-frame-pool memory. =1 collapses this to ~1 thread/decoder.
$env:SERVO_GSTREAMER_AVDEC_MAX_THREADS = "1"
$env:RUST_LOG = if ($env:RUST_LOG) { $env:RUST_LOG } else { "warn" }

$arguments = @("--window-size", $WindowSize, $url)

$stdoutLog = Join-Path $logDir "${LogPrefix}_${Profile}_stdout_${timestamp}.log"
$stderrLog = Join-Path $logDir "${LogPrefix}_${Profile}_stderr_${timestamp}.log"

Write-Host "Launching video grid perf test:"
Write-Host "  exe=$servoExe"
Write-Host "  grid=$gridDesc"
Write-Host "  window-size=$WindowSize"
Write-Host "  url=$url"
Write-Host "  SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF=1 (smooth delivery)"
Write-Host "  stderr=$stderrLog"

if ($Detach) {
    $p = Start-Process -FilePath $servoExe -ArgumentList $arguments -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog -PassThru
    Write-Host "Servo video grid running detached. pid=$($p.Id)"
} elseif ($DurationSec -gt 0) {
    $p = Start-Process -FilePath $servoExe -ArgumentList $arguments -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog -PassThru
    Start-Sleep -Seconds $DurationSec
    if (!$p.HasExited) { $p.CloseMainWindow() | Out-Null; Start-Sleep -Seconds 2 }
    if (!$p.HasExited) { Stop-Process -Id $p.Id -Force }
    Write-Host "Servo video grid smoke finished after $DurationSec seconds."
} else {
    Push-Location $servoRoot
    try { & $servoExe @arguments 1> $stdoutLog 2> $stderrLog } finally { Pop-Location }
}

Write-Host "Logs:"
Write-Host "  $stdoutLog"
Write-Host "  $stderrLog"

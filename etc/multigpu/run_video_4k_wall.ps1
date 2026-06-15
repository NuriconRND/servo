param(
    [string] $Layout = "etc\multigpu\config\wall_layout.example_2x1_dualgpu.json",
    [ValidateSet("release", "debug")]
    [string] $Profile = "release",
    [string] $Page = "video_4k_fullbleed.html",
    [int]    $DurationSec = 0,
    [switch] $Detach,
    [string] $LogPrefix = "video_4k_wall"
)

# Launch the 4K H.264 video full-bleed across the multi-GPU wall.
#
# Smooth-60fps notes:
#  - The per-frame compositing fix (composite on video image-update, in
#    components/paint/painter.rs::update_images) is now the default, so no flag is needed for it.
#  - SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF=1: the gstreamer servosrc EnoughData/NeedData backoff
#    otherwise pauses file-byte pushing periodically, starving decoded-frame delivery for ~400ms
#    every ~8-12s (a visible periodic stutter). Disabling it keeps the source fed so delivery is a
#    steady 60fps. Safe here because the wall plays a LOCAL file (bounded by MAX_BUFFER_SIZE=500MB);
#    do not blanket-enable for network streams without considering memory.
#
# The page is plain HTML (no ES modules), so file:// works -- no HTTP server needed.
$ErrorActionPreference = "Stop"

$servoRoot  = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$servoExe   = Join-Path $servoRoot "target\$Profile\servoshell.exe"
$layoutPath = Join-Path $servoRoot $Layout
$pagePath   = Join-Path $servoRoot "tests\html\$Page"
$logDir     = Join-Path $servoRoot "target\multigpu_logs"
$timestamp  = Get-Date -Format "yyyyMMdd_HHmmss"

if (!(Test-Path $servoExe))   { throw "servoshell.exe not found: $servoExe" }
if (!(Test-Path $layoutPath)) { throw "Wall layout not found: $layoutPath" }
if (!(Test-Path $pagePath))   { throw "Page not found: $pagePath" }
New-Item -ItemType Directory -Path $logDir -Force | Out-Null

$url = "file:///" + ($pagePath -replace '\\', '/')

# Smooth-delivery knob (see header).
$env:SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF = "1"
$env:RUST_LOG = if ($env:RUST_LOG) { $env:RUST_LOG } else { "warn" }

$arguments = @("--wall-layout", $layoutPath, "--wall-all-tiles", $url)

$stdoutLog = Join-Path $logDir "${LogPrefix}_${Profile}_stdout_${timestamp}.log"
$stderrLog = Join-Path $logDir "${LogPrefix}_${Profile}_stderr_${timestamp}.log"

Write-Host "Launching 4K video wall:"
Write-Host "  exe=$servoExe"
Write-Host "  layout=$layoutPath"
Write-Host "  url=$url"
Write-Host "  SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF=1 (smooth delivery)"
Write-Host "  stderr=$stderrLog"

if ($Detach) {
    $p = Start-Process -FilePath $servoExe -ArgumentList $arguments -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog -PassThru
    Write-Host "Servo video wall running detached. pid=$($p.Id)"
} elseif ($DurationSec -gt 0) {
    $p = Start-Process -FilePath $servoExe -ArgumentList $arguments -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog -PassThru
    Start-Sleep -Seconds $DurationSec
    if (!$p.HasExited) { $p.CloseMainWindow() | Out-Null; Start-Sleep -Seconds 2 }
    if (!$p.HasExited) { Stop-Process -Id $p.Id -Force }
    Write-Host "Servo video wall smoke finished after $DurationSec seconds."
} else {
    Push-Location $servoRoot
    try { & $servoExe @arguments 1> $stdoutLog 2> $stderrLog } finally { Pop-Location }
}

Write-Host "Logs:"
Write-Host "  $stdoutLog"
Write-Host "  $stderrLog"

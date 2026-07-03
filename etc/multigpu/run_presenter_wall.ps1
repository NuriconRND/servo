# run_presenter_wall.ps1
# Presenter 송출 화면(/#/output)을 2x1 dual-GPU wall로 표출.
# - self-signed HTTPS(nuriconAll.com) 대응: --ignore-certificate-errors
# - 송출 화면은 WebRTC(RTCPeerConnection)로 카메라/미디어를 수신하므로
#   Servo의 dom_webrtc_enabled / dom_webrtc_transceiver_enabled pref를 켠다
#   (기본 off 이면 "RTCPeerConnection is not defined" → "송출 화면 준비 중..."에서 멈춤).
param(
    [string] $Url = "https://127.0.0.1:5175/#/output",
    [string] $Layout = "etc\multigpu\config\wall_layout.example_2x1_dualgpu.json",
    [ValidateSet("release", "debug")]
    [string] $Profile = "release",
    [switch] $NoWebRTC,        # WebRTC pref 비활성화(디버깅용)
    [switch] $Detach,
    [int] $DurationSec = 0,
    [string] $LogPrefix = "presenter_output_wall"
)

$ErrorActionPreference = "Stop"

$servoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$servoExe = Join-Path $servoRoot "target\$Profile\servoshell.exe"
$layoutPath = Join-Path $servoRoot $Layout
$logDir = Join-Path $servoRoot "target\multigpu_logs"
$timestamp = Get-Date -Format "yyyyMMdd_HHmmss"
$stdoutLog = Join-Path $logDir "${LogPrefix}_${Profile}_stdout_${timestamp}.log"
$stderrLog = Join-Path $logDir "${LogPrefix}_${Profile}_stderr_${timestamp}.log"

if (!(Test-Path $servoExe)) { throw "servoshell.exe not found: $servoExe" }
if (!(Test-Path $layoutPath)) { throw "Wall layout not found: $layoutPath" }

New-Item -ItemType Directory -Path $logDir -Force | Out-Null

$env:RUST_LOG = if ($env:RUST_LOG) { $env:RUST_LOG } else { "info" }

$arguments = @(
    "--wall-layout", $layoutPath,
    "--wall-all-tiles",
    "--ignore-certificate-errors"
)
if (-not $NoWebRTC) {
    $arguments += @("--pref", "dom_webrtc_enabled=true", "--pref", "dom_webrtc_transceiver_enabled=true")
}
# The output page's image preloader uses IndexedDB; without it the preload never
# completes and the "송출 화면 준비 중" overlay never clears (the readiness gate
# waits on preloadedShowId). Servo defaults dom_indexeddb_enabled to false.
$arguments += @("--pref", "dom_indexeddb_enabled=true")
$arguments += $Url

Write-Host "Launching Presenter wall:"
Write-Host "  exe=$servoExe"
Write-Host "  layout=$layoutPath"
Write-Host "  url=$Url"
Write-Host "  webrtc=$(-not $NoWebRTC)"
Write-Host "  ignore_cert_errors=True"
Write-Host "  stdout=$stdoutLog"
Write-Host "  stderr=$stderrLog"

if ($Detach) {
    $process = Start-Process -FilePath $servoExe `
        -ArgumentList $arguments `
        -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog `
        -RedirectStandardError $stderrLog `
        -PassThru
    Write-Host "Presenter wall is running detached. pid=$($process.Id)"
} elseif ($DurationSec -gt 0) {
    $process = Start-Process -FilePath $servoExe `
        -ArgumentList $arguments `
        -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog `
        -RedirectStandardError $stderrLog `
        -PassThru
    Start-Sleep -Seconds $DurationSec
    if (!$process.HasExited) { $process.CloseMainWindow() | Out-Null; Start-Sleep -Seconds 2 }
    if (!$process.HasExited) { Stop-Process -Id $process.Id -Force }
    Write-Host "Presenter wall smoke finished after $DurationSec seconds."
} else {
    Push-Location $servoRoot
    try {
        & $servoExe @arguments 1> $stdoutLog 2> $stderrLog
    } finally {
        Pop-Location
    }
}

Write-Host "Logs:"
Write-Host "  $stdoutLog"
Write-Host "  $stderrLog"

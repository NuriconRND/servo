param(
    [ValidateSet("release","debug")] [string] $Profile = "release",
    [ValidateSet("v1","v2")]        [string] $Mode = "v2",
    [ValidateSet("0","1")]          [string] $Dom = "0",
    [int]    $Grid = 8,
    [int]    $Cols = 0,
    [int]    $Rows = 0,
    [string] $WindowSize = "1920x1080",
    [int]    $DurationSec = 0,
    [switch] $Detach,
    [string] $LogPrefix = "ceiling"
)
# 천장 검증 WebGL 하네스(video_collapse_ceiling.html)를 단일 servoshell 창으로 실행.
# 계측: SERVO_LOG_PRESENT_CADENCE=1 + RUST_LOG=warn,paint=info → present cadence +
#       >16ms 프레임의 wr_update/wr_render 분해가 stderr 로 남는다.
$ErrorActionPreference = "Stop"

$servoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$servoExe  = Join-Path $servoRoot "target\$Profile\servoshell.exe"
$pagePath  = Join-Path $servoRoot "tests\html\video_collapse_ceiling.html"
$logDir    = Join-Path $servoRoot "target\multigpu_logs"
$timestamp = Get-Date -Format "yyyyMMdd_HHmmss"

if (!(Test-Path $servoExe)) { throw "servoshell.exe not found: $servoExe" }
if (!(Test-Path $pagePath)) { throw "Page not found: $pagePath" }
New-Item -ItemType Directory -Path $logDir -Force | Out-Null

if ($Cols -gt 0 -or $Rows -gt 0) {
    $c = if ($Cols -gt 0) { $Cols } else { $Grid }
    $r = if ($Rows -gt 0) { $Rows } else { $Grid }
    $gridQ = "cols=$c&rows=$r"; $gridDesc = "${c}x${r} = $($c*$r) tiles"
} else {
    $gridQ = "grid=$Grid"; $gridDesc = "${Grid}x${Grid} = $($Grid*$Grid) tiles"
}
$url = "file:///" + ($pagePath -replace '\\','/') + "?$gridQ&mode=$Mode&dom=$Dom&log=1"

$env:SERVO_LOG_PRESENT_CADENCE = "1"
$env:RUST_LOG = "warn,paint=info"

$arguments = @("--window-size", $WindowSize, $url)
$stdoutLog = Join-Path $logDir "${LogPrefix}_${Mode}_dom${Dom}_${Profile}_stdout_${timestamp}.log"
$stderrLog = Join-Path $logDir "${LogPrefix}_${Mode}_dom${Dom}_${Profile}_stderr_${timestamp}.log"

Write-Host "Launching collapse-ceiling harness:"
Write-Host "  mode=$Mode dom=$Dom grid=$gridDesc window=$WindowSize"
Write-Host "  url=$url"
Write-Host "  stderr=$stderrLog"

if ($Detach) {
    $p = Start-Process -FilePath $servoExe -ArgumentList $arguments -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog -PassThru
    Write-Host "running detached. pid=$($p.Id)  stderr=$stderrLog"
} elseif ($DurationSec -gt 0) {
    $p = Start-Process -FilePath $servoExe -ArgumentList $arguments -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog -PassThru
    Start-Sleep -Seconds $DurationSec
    if (!$p.HasExited) { $p.CloseMainWindow() | Out-Null; Start-Sleep -Seconds 2 }
    if (!$p.HasExited) { Stop-Process -Id $p.Id -Force }
    Write-Host "smoke finished after $DurationSec s. stderr=$stderrLog"
} else {
    Push-Location $servoRoot
    try { & $servoExe @arguments 1> $stdoutLog 2> $stderrLog } finally { Pop-Location }
}

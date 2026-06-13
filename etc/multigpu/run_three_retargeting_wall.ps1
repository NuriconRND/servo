param(
    [string] $Layout = "etc\multigpu\config\wall_layout.example_2x1_dualgpu.json",
    [ValidateSet("release", "debug")]
    [string] $Profile = "release",
    [int]    $Port = 8753,
    [switch] $Faithful,        # use original textured materials (appear black under Servo WebGPU)
    [switch] $NoWebGPU,        # do NOT pass --pref dom_webgpu_enabled=true (page will report it needs WebGPU)
    [int]    $DurationSec = 0,
    [switch] $Detach,
    [string] $LogPrefix = "three_retargeting_wall"
)

# Launch the three.js WebGPU animation-retargeting example across the multi-GPU wall.
#
# Why an HTTP server: the example is an ES-module page (three.webgpu.js + addons).
# Servo's module loader rejects the file:// scheme ("Unsupported scheme"), so the
# page is served over http://127.0.0.1:$Port and Servo is pointed at that URL.
$ErrorActionPreference = "Stop"

$servoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$servoExe  = Join-Path $servoRoot "target\$Profile\servoshell.exe"
$htmlRoot  = Join-Path $servoRoot "tests\html"
$layoutPath = Join-Path $servoRoot $Layout
$logDir    = Join-Path $servoRoot "target\multigpu_logs"
$timestamp = Get-Date -Format "yyyyMMdd_HHmmss"

if (!(Test-Path $servoExe))   { throw "servoshell.exe not found: $servoExe" }
if (!(Test-Path $layoutPath)) { throw "Wall layout not found: $layoutPath" }
$page = Join-Path $htmlRoot "webgpu_fanout_three_retargeting.html"
if (!(Test-Path $page))       { throw "Page not found: $page" }
New-Item -ItemType Directory -Path $logDir -Force | Out-Null

# 1) Ensure a static HTTP server is serving tests/html on $Port.
$listening = $false
try {
    $listening = [bool](Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue)
} catch { $listening = $false }

if (-not $listening) {
    $py = (Get-Command python -ErrorAction SilentlyContinue).Source
    if (-not $py) { throw "python not found on PATH (needed to serve the ES-module page over HTTP)." }
    Write-Host "Starting HTTP server: $py -m http.server $Port (root=$htmlRoot)"
    Start-Process -FilePath $py `
        -ArgumentList @("-m", "http.server", "$Port", "--bind", "127.0.0.1") `
        -WorkingDirectory $htmlRoot `
        -WindowStyle Hidden | Out-Null
    Start-Sleep -Seconds 2
} else {
    Write-Host "HTTP server already listening on 127.0.0.1:$Port"
}

# 2) Build the page URL.
$query = ""
if ($Faithful) { $query = "?faithful=1" }
$url = "http://127.0.0.1:$Port/webgpu_fanout_three_retargeting.html$query"

# 3) Compose servoshell arguments.
$env:RUST_LOG = if ($env:RUST_LOG) { $env:RUST_LOG } else { "info" }
$arguments = @("--wall-layout", $layoutPath, "--wall-all-tiles")
if (-not $NoWebGPU) { $arguments += @("--pref", "dom_webgpu_enabled=true") }
$arguments += $url

$stdoutLog = Join-Path $logDir "${LogPrefix}_${Profile}_stdout_${timestamp}.log"
$stderrLog = Join-Path $logDir "${LogPrefix}_${Profile}_stderr_${timestamp}.log"

Write-Host "Launching three.js retargeting wall:"
Write-Host "  exe=$servoExe"
Write-Host "  layout=$layoutPath"
Write-Host "  url=$url"
Write-Host "  webgpu=$([bool](-not $NoWebGPU))  faithful=$([bool]$Faithful)"
Write-Host "  stderr=$stderrLog"

if ($Detach) {
    $p = Start-Process -FilePath $servoExe -ArgumentList $arguments -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog -PassThru
    Write-Host "Servo wall running detached. pid=$($p.Id)"
} elseif ($DurationSec -gt 0) {
    $p = Start-Process -FilePath $servoExe -ArgumentList $arguments -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog -PassThru
    Start-Sleep -Seconds $DurationSec
    if (!$p.HasExited) { $p.CloseMainWindow() | Out-Null; Start-Sleep -Seconds 2 }
    if (!$p.HasExited) { Stop-Process -Id $p.Id -Force }
    Write-Host "Servo wall smoke finished after $DurationSec seconds."
} else {
    Push-Location $servoRoot
    try { & $servoExe @arguments 1> $stdoutLog 2> $stderrLog } finally { Pop-Location }
}

Write-Host "Logs:"
Write-Host "  $stdoutLog"
Write-Host "  $stderrLog"

param(
    [string] $Url = "https://map.kakao.com/",
    [string] $Layout = "etc\multigpu\config\wall_layout.example_2x1_dualgpu.json",
    [ValidateSet("release", "debug")]
    [string] $Profile = "release",
    [int] $DurationSec = 0,
    [switch] $Detach,
    [switch] $DisableWebGL,
    [switch] $MultiProcess,
    [string] $LogPrefix = "kakao_map_wall"
)

$ErrorActionPreference = "Stop"

$servoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$servoExe = Join-Path $servoRoot "target\$Profile\servoshell.exe"
$layoutPath = Join-Path $servoRoot $Layout
$logDir = Join-Path $servoRoot "target\multigpu_logs"
$timestamp = Get-Date -Format "yyyyMMdd_HHmmss"
$stdoutLog = Join-Path $logDir "${LogPrefix}_${Profile}_stdout_${timestamp}.log"
$stderrLog = Join-Path $logDir "${LogPrefix}_${Profile}_stderr_${timestamp}.log"

if (!(Test-Path $servoExe)) {
    throw "servoshell.exe not found: $servoExe"
}

if (!(Test-Path $layoutPath)) {
    throw "Wall layout not found: $layoutPath"
}

New-Item -ItemType Directory -Path $logDir -Force | Out-Null

$env:RUST_LOG = if ($env:RUST_LOG) { $env:RUST_LOG } else { "info" }

$layoutJson = Get-Content -LiteralPath $layoutPath -Raw | ConvertFrom-Json
$tileCount = @($layoutJson.tiles).Count
if ($tileCount -lt 1) {
    throw "Wall layout must contain at least one tile: $layoutPath"
}

function New-ServoArguments {
    param([int] $TileIndex = -1)

    $args = @("--wall-layout", $layoutPath)
    if ($TileIndex -ge 0) {
        $args += @("--wall-tile-index", "$TileIndex")
    } else {
        $args += "--wall-all-tiles"
    }

    if ($DisableWebGL) {
        $args += @("--userscripts", (Join-Path $servoRoot "etc\multigpu\userscripts\disable_webgl"))
    }

    $args += $Url
    return $args
}

$arguments = New-ServoArguments

Write-Host "Launching Servo wall:"
Write-Host "  exe=$servoExe"
Write-Host "  layout=$layoutPath"
Write-Host "  url=$Url"
Write-Host "  disable_webgl=$DisableWebGL"
Write-Host "  multi_process=$MultiProcess"
Write-Host "  stdout=$stdoutLog"
Write-Host "  stderr=$stderrLog"

if ($MultiProcess) {
    $processes = @()
    for ($tile = 0; $tile -lt $tileCount; $tile++) {
        $tileStdoutLog = Join-Path $logDir "${LogPrefix}_${Profile}_tile${tile}_stdout_${timestamp}.log"
        $tileStderrLog = Join-Path $logDir "${LogPrefix}_${Profile}_tile${tile}_stderr_${timestamp}.log"
        $tileArguments = New-ServoArguments -TileIndex $tile
        $process = Start-Process -FilePath $servoExe `
            -ArgumentList $tileArguments `
            -WorkingDirectory $servoRoot `
            -RedirectStandardOutput $tileStdoutLog `
            -RedirectStandardError $tileStderrLog `
            -PassThru

        $processes += $process
        Write-Host "  tile${tile}: pid=$($process.Id)"
        Write-Host "    stdout=$tileStdoutLog"
        Write-Host "    stderr=$tileStderrLog"
    }

    if ($DurationSec -gt 0) {
        Start-Sleep -Seconds $DurationSec
        foreach ($process in $processes) {
            if (!$process.HasExited) {
                $process.CloseMainWindow() | Out-Null
                Start-Sleep -Milliseconds 500
            }
            if (!$process.HasExited) {
                Stop-Process -Id $process.Id -Force
            }
        }
        Write-Host "Servo wall multi-process smoke finished after $DurationSec seconds."
    } else {
        Write-Host "Servo wall is running detached in multi-process mode."
    }
} elseif ($Detach) {
    $process = Start-Process -FilePath $servoExe `
        -ArgumentList $arguments `
        -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog `
        -RedirectStandardError $stderrLog `
        -PassThru

    Write-Host "Servo wall is running detached."
    Write-Host "  pid=$($process.Id)"
} elseif ($DurationSec -gt 0) {
    $process = Start-Process -FilePath $servoExe `
        -ArgumentList $arguments `
        -WorkingDirectory $servoRoot `
        -RedirectStandardOutput $stdoutLog `
        -RedirectStandardError $stderrLog `
        -PassThru

    Start-Sleep -Seconds $DurationSec

    if (!$process.HasExited) {
        $process.CloseMainWindow() | Out-Null
        Start-Sleep -Seconds 2
    }

    if (!$process.HasExited) {
        Stop-Process -Id $process.Id -Force
    }

    Write-Host "Servo wall smoke finished after $DurationSec seconds."
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

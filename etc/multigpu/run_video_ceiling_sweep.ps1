param(
    [ValidateSet("release","debug")] [string] $Profile = "release",
    [string] $WindowSize = "1920x1080",
    [int]    $WarmupSec = 12,
    [int]    $SteadySec = 30,
    # 부분 실행용 필터(비우면 전체). 예: -Series "v1,v2"  -Ns "45,64"
    [string] $Series = "baseline,v1,v2",
    [string] $Doms = "0,1",
    [string] $Ns = "30,40,45,64"
)
# B/V1/V2 × dom{0,1} × N{30,40,45,64} 스윕. 각 config를 detached 로 (Warmup+Steady)초 돌리고
# stderr 를 개별 로그로 캡처, manifest.csv 에 기록. present cadence 는 detached 에서도 stderr 로
# 남으므로 자동 수집 가능(단 PresentMon 물리 present 교차확인은 Task5에서 foreground 로 별도).
$ErrorActionPreference = "Stop"

$servoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$servoExe  = Join-Path $servoRoot "target\$Profile\servoshell.exe"
$ceilPage  = Join-Path $servoRoot "tests\html\video_collapse_ceiling.html"
$gridPage  = Join-Path $servoRoot "tests\html\video_grid_6x6_perf.html"
if (!(Test-Path $servoExe)) { throw "servoshell.exe not found: $servoExe" }

$timestamp = Get-Date -Format "yyyyMMdd_HHmmss"
$sweepDir  = Join-Path $servoRoot "target\multigpu_logs\sweep_$timestamp"
New-Item -ItemType Directory -Path $sweepDir -Force | Out-Null

# N → cols x rows 매핑
$gridMap = @{ 30 = @(6,5); 40 = @(8,5); 45 = @(9,5); 64 = @(8,8) }

$seriesSel = $Series.Split(",") | ForEach-Object { $_.Trim() }
$domSel    = $Doms.Split(",")   | ForEach-Object { $_.Trim() }
$nSel      = $Ns.Split(",")     | ForEach-Object { [int]$_.Trim() }

$env:SERVO_LOG_PRESENT_CADENCE = "1"
$env:RUST_LOG = "warn,paint=info"

$rows = @()
foreach ($n in $nSel) {
    if (-not $gridMap.ContainsKey($n)) { Write-Warning "no grid mapping for N=$n, skip"; continue }
    $cols = $gridMap[$n][0]; $rowsN = $gridMap[$n][1]
    foreach ($series in $seriesSel) {
        foreach ($dom in $domSel) {
            if ($series -eq "baseline") {
                $page = $gridPage
                $url  = "file:///" + ($page -replace '\\','/') + "?cols=$cols&rows=$rowsN&dom=$dom&log=1"
                # baseline 만 실제 비디오 → 디코드 정책 env 필요
                $env:SERVO_GSTREAMER_AVDEC_MAX_THREADS = "1"
                $env:SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF = "1"
                $mode = "-"
            } else {
                $page = $ceilPage
                $url  = "file:///" + ($page -replace '\\','/') + "?cols=$cols&rows=$rowsN&mode=$series&dom=$dom&log=1"
                Remove-Item Env:\SERVO_GSTREAMER_AVDEC_MAX_THREADS -ErrorAction SilentlyContinue
                Remove-Item Env:\SERVO_MEDIA_DISABLE_ENOUGHDATA_BACKOFF -ErrorAction SilentlyContinue
                $mode = $series
            }
            $stderr = Join-Path $sweepDir "${series}_dom${dom}_n${n}.stderr.log"
            $stdout = Join-Path $sweepDir "${series}_dom${dom}_n${n}.stdout.log"
            Write-Host "[sweep] series=$series dom=$dom n=$n ($cols x $rowsN)  -> $stderr"
            $p = Start-Process -FilePath $servoExe -ArgumentList @("--window-size",$WindowSize,$url) `
                 -WorkingDirectory $servoRoot -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru
            Start-Sleep -Seconds ($WarmupSec + $SteadySec)
            if (!$p.HasExited) { Stop-Process -Id $p.Id -Force }
            Start-Sleep -Seconds 1
            $rows += [pscustomobject]@{ label=$series; mode=$mode; dom=$dom; n=$n; cols=$cols; rows=$rowsN; stderr=$stderr }
        }
    }
}
$manifest = Join-Path $sweepDir "manifest.csv"
$rows | Export-Csv -NoTypeInformation -Encoding utf8 $manifest
Write-Host "sweep done. manifest=$manifest"
Write-Host "parse with: etc\multigpu\tools\parse_ceiling_logs.ps1 -Manifest `"$manifest`""

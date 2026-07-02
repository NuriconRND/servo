param(
    [Parameter(Mandatory=$true)] [string] $Manifest,
    [int] $DropFirst = 10   # 워밍업으로 앞 N개 초당 샘플 버림
)
# 스윕 manifest 를 읽어 config별 정상상태 중앙값 표(markdown)를 stdout 으로 출력.
$ErrorActionPreference = "Stop"
function Median($xs) {
    $s = @($xs | Where-Object { $_ -ne $null } | Sort-Object)
    if ($s.Count -eq 0) { return $null }
    return $s[[int][math]::Floor($s.Count/2)]
}
function Pct($xs, $p) {
    $s = @($xs | Where-Object { $_ -ne $null } | Sort-Object)
    if ($s.Count -eq 0) { return $null }
    $idx = [int][math]::Floor(($s.Count-1) * $p); return $s[$idx]
}

$configs = Import-Csv $Manifest
$fmt = { param($v,$d=1) if ($null -eq $v) { "-" } else { [math]::Round($v,$d) } }

"| series | dom | N | rAF fps | present/s | max_gap ms | wr_render p50 | wr_render p95 | wr_update p50 |"
"|---|---|---|---|---|---|---|---|---|"
foreach ($c in $configs) {
    if (!(Test-Path $c.stderr)) { "| $($c.label) | $($c.dom) | $($c.n) | (no log) | | | | | |"; continue }
    $lines = Get-Content $c.stderr

    # 페이지 rAF fps: [CEILING] 또는 [GRIDPERF] ... fps=NN.N maxGapMs=NN.N
    $fps = @(); $gap = @()
    foreach ($ln in ($lines | Select-String -Pattern "\[(CEILING|GRIDPERF)\].* fps=([0-9.]+) maxGapMs=([0-9.]+)")) {
        $m = [regex]::Match($ln.Line, "fps=([0-9.]+) maxGapMs=([0-9.]+)")
        if ($m.Success) { $fps += [double]$m.Groups[1].Value; $gap += [double]$m.Groups[2].Value }
    }
    # present cadence: Present cadence: painter .. presents/s=NN.N max_gap_ms=NN.NN pending=N
    $pps = @()
    foreach ($ln in ($lines | Select-String -Pattern "Present cadence: painter .* presents/s=([0-9.]+)")) {
        $m = [regex]::Match($ln.Line, "presents/s=([0-9.]+)")
        if ($m.Success) { $pps += [double]$m.Groups[1].Value }
    }
    # slow-frame 분해: Slow paint frame: .. wr_update_ms=NN.NN wr_render_ms=NN.NN
    $wu = @(); $wr = @()
    foreach ($ln in ($lines | Select-String -Pattern "wr_update_ms=([0-9.]+) wr_render_ms=([0-9.]+)")) {
        $m = [regex]::Match($ln.Line, "wr_update_ms=([0-9.]+) wr_render_ms=([0-9.]+)")
        if ($m.Success) { $wu += [double]$m.Groups[1].Value; $wr += [double]$m.Groups[2].Value }
    }
    # 워밍업 제거(초당 샘플 계열) — Count가 DropFirst 이하여도 항상 min(DropFirst,Count)만큼 앞을 잘라낸다.
    # (기존엔 Count -gt DropFirst 가드가 "Count <= DropFirst"인 경우 트림을 통째로 건너뛰어
    #  워밍업이 섞인 배열을 그대로 steady-state처럼 Median/Pct에 흘려보내는 문제가 있었음)
    $fpsSampleCount = $fps.Count
    $dropN = [Math]::Min($DropFirst, $fps.Count)
    if ($dropN -lt $fps.Count) { $fps = $fps[$dropN..($fps.Count-1)] } else { $fps = @() }
    $dropN = [Math]::Min($DropFirst, $gap.Count)
    if ($dropN -lt $gap.Count) { $gap = $gap[$dropN..($gap.Count-1)] } else { $gap = @() }
    $dropN = [Math]::Min($DropFirst, $pps.Count)
    if ($dropN -lt $pps.Count) { $pps = $pps[$dropN..($pps.Count-1)] } else { $pps = @() }
    if ($fpsSampleCount -le $DropFirst) {
        Write-Warning "config $($c.label) dom$($c.dom) n$($c.n): only $fpsSampleCount fps samples (<= DropFirst=$DropFirst) — row is unreliable"
    }

    $row = "| {0} | {1} | {2} | {3} | {4} | {5} | {6} | {7} | {8} |" -f `
        $c.label, $c.dom, $c.n,
        (& $fmt (Median $fps)), (& $fmt (Median $pps)), (& $fmt (Median $gap)),
        (& $fmt (Median $wr)), (& $fmt (Pct $wr 0.95)), (& $fmt (Median $wu))
    $row
}
